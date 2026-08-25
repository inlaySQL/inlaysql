//! The dialect shim: the statements answered here instead of by the engine.
//!
//! This is decision **D1** in `docs/architecture.md`. `inlaysql-core` speaks SQLite's
//! dialect and keeps speaking it; the MySQL-shaped statements a driver sends
//! are recognised at this layer and answered from [`Catalog`] and session
//! state. Nothing in this file adds SQL syntax to the engine, and anything it
//! does not recognise is passed through unchanged for the engine to accept or
//! refuse on its own terms.
//!
//! # The rule this module is built around
//!
//! A metadata answer that is *wrong* is worse than no answer at all: a
//! migration tool that is told a column exists when it does not will happily
//! generate the next statement against a schema that was never there. So every
//! path here either answers from the catalog or returns a MySQL error naming
//! what it could not do. There is no branch that guesses, and no branch that
//! returns an empty result set to mean "I did not understand the filter".

use inlaysql::{Catalog, DataType, ResultSet, Table, Value};

use crate::errors::MysqlError;
use crate::{mysqlddl, mysqlfunc};

use crate::session::{Session, Warning, SERVER_VERSION};
use crate::sqltext::{
    find_keyword, first_word, like_matches, matching_close_paren, normalize, split_top_level,
    starts_with_keyword, strip_keyword, unquote_identifier, unquote_string,
};

/// What the connection should do with a statement.
#[derive(Debug)]
pub enum Intercepted {
    /// Not the shim's business — hand it to the engine.
    PassThrough,
    /// MySQL-only DDL decoration was translated out of it; run these
    /// statements on the engine instead, in order, and report `warnings` for
    /// what was removed.
    ///
    /// Almost always one statement. More than one only for a MySQL `ALTER
    /// TABLE` that bundled several operations, or an operation that became
    /// its own free-standing statement (see `crate::mysqlddl`) — and running
    /// that sequence is **not atomic** the way MySQL's single statement is;
    /// see `Connection::run_statements_on_engine`. Empty when every
    /// operation in the statement turned into a warning and nothing else (an
    /// `ADD CONSTRAINT ... FOREIGN KEY` on its own), so there is nothing left
    /// to run — the caller answers a plain OK.
    Rewritten {
        /// The statements to hand to the engine, in order.
        statements: Vec<String>,
        /// One warning per clause that was removed.
        warnings: Vec<Warning>,
    },
    /// Handled; reply OK.
    Ok,
    /// Handled; reply with these rows.
    Rows(Box<ResultSet>),
    /// Open a transaction.
    Begin,
    /// Commit the open transaction.
    Commit,
    /// Roll the open transaction back.
    Rollback,
    /// Turn autocommit on or off.
    SetAutocommit(bool),
    /// Select a default schema.
    UseDatabase(String),
    /// Recognised, but this server cannot do it.
    Failed(MysqlError),
}

/// The name reported when no schema has been selected.
pub const DEFAULT_SCHEMA: &str = "inlaysql";

/// Whether [`intercept`] would answer this statement itself.
///
/// Needed at `COM_STMT_PREPARE`, where the statement must be *classified*
/// without being *run*: preparing `COMMIT` may not commit anything. It is
/// deliberately a decision about the statement's shape only, and shares the
/// leading-keyword dispatch with [`intercept`] so the two cannot drift.
pub fn handles(sql: &str) -> bool {
    let sql = normalize(sql);
    if sql.is_empty() {
        return true;
    }
    match first_word(&sql).as_str() {
        "SET" | "SHOW" | "USE" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT"
        | "RELEASE" | "DESCRIBE" | "DESC" | "DO" => true,
        "SELECT" => !matches!(select_target(&sql), SelectTarget::Engine),
        _ => false,
    }
}

/// Whether a statement *reads* the warning list rather than replacing it.
///
/// MySQL clears warnings at the start of every statement except the ones whose
/// whole purpose is to read them. Without this, a `SHOW WARNINGS` would clear
/// the list before answering and could only ever report none.
pub fn reads_warnings(sql: &str) -> bool {
    let sql = normalize(sql);
    let Some(rest) = strip_keyword(&sql, "SHOW") else {
        return false;
    };
    let rest = strip_keyword(rest, "SESSION")
        .or_else(|| strip_keyword(rest, "GLOBAL"))
        .unwrap_or(rest);
    starts_with_keyword(rest, "WARNINGS") || starts_with_keyword(rest, "ERRORS")
}

/// Where a `SELECT` should be answered.
enum SelectTarget {
    /// The engine's, whatever it makes of it.
    Engine,
    /// An `information_schema` view.
    InfoSchema,
    /// `EXISTS (SELECT ... FROM information_schema...) [[AS] alias]` filling
    /// the whole projection list — see [`existence_probe`].
    InfoSchemaExists {
        /// The subquery's own text, cut out of the statement (the span
        /// between the parentheses, not including them).
        subquery: (usize, usize),
        /// The alias the client gave the boolean result, if any.
        alias: Option<String>,
    },
    /// A projection of session state, with no table behind it.
    Session {
        /// The projection list, already cut out of the statement.
        select_list: (usize, usize),
    },
}

/// The single place that decides whether a `SELECT` belongs to the shim.
///
/// Both [`handles`] and [`handle_select`] go through this, so a statement can
/// never be classified one way when it is prepared and the other way when it
/// is executed.
fn select_target(sql: &str) -> SelectTarget {
    // `SELECT` is six bytes; the projection starts after it.
    const LIST_START: usize = 6;
    let from = find_keyword(sql, "from");

    // An `EXISTS (subquery)` filling the whole projection has no top-level
    // `FROM` of its own — the subquery's `FROM` sits at parenthesis depth 1,
    // invisible to the search above by design (see `find_keyword`). Checked
    // before the `mentions_session_state` fallback below on purpose: that
    // heuristic has no notion of `EXISTS` and would otherwise sometimes
    // accept this shape by accident, whenever the subquery's own `WHERE`
    // happened to mention `schema()`/`database()`/`@@...`.
    if from.is_none() {
        if let Some(target) = existence_probe(sql, LIST_START) {
            return target;
        }
    }

    if let Some(at) = from {
        let target = sql[at + 4..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if target.contains("information_schema") {
            return SelectTarget::InfoSchema;
        }
        // `FROM DUAL` is MySQL's way of writing a row source with no table.
        if !target.trim_end_matches(';').eq_ignore_ascii_case("dual") {
            return SelectTarget::Engine;
        }
    }

    let list_end = from.unwrap_or(sql.len());
    if list_end < LIST_START || !mentions_session_state(&sql[LIST_START..list_end]) {
        return SelectTarget::Engine;
    }
    SelectTarget::Session {
        select_list: (LIST_START, list_end),
    }
}

/// `EXISTS (SELECT ... FROM ...) [[AS] alias]` filling the whole projection
/// list — the shape an existence check like Laravel's
/// `hasTable()`/`hasColumn()` (and most MySQL-targeting ORMs' schema
/// builders) compiles to. `select_list_start` is where the projection begins
/// (`LIST_START` in [`select_target`]; a parameter here only so this stays
/// testable on its own).
///
/// Recognised narrowly, by exact shape, in keeping with this shim's rule of
/// refusing anything it does not understand rather than guessing: trailing
/// text after the closing `)` that is not a clean alias means this function
/// simply does not match, and the statement falls through to the ordinary
/// paths below exactly as it did before this function existed.
///
/// When the subquery targets `information_schema`, this shim answers it
/// itself (`InfoSchemaExists`). Otherwise the whole original statement is
/// handed to the engine unchanged (`Engine`) — a scalar `EXISTS (...)` over a
/// real table is already something `inlaysql-core` understands (`README.md`,
/// "Subqueries too"), and was never this shim's business; it only reached
/// here because the same false positive above can misfire on a real table's
/// subquery too, whenever that subquery's `WHERE` mentions a session-state
/// marker.
fn existence_probe(sql: &str, select_list_start: usize) -> Option<SelectTarget> {
    if sql.len() < select_list_start {
        return None;
    }
    let rest = strip_keyword(&sql[select_list_start..], "EXISTS")?;
    let open_at = sql.len() - rest.len();
    if !rest.starts_with('(') {
        return None;
    }
    let close_at = matching_close_paren(sql, open_at)?;

    let trailer = sql[close_at + 1..].trim();
    let alias = if trailer.is_empty() {
        None
    } else {
        let named = strip_keyword(trailer, "AS").unwrap_or(trailer);
        let alias_like = !named.is_empty()
            && named
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '`' || c == '"');
        if !alias_like {
            // Not a clean alias — do not guess what this shape means.
            return None;
        }
        Some(unquote_identifier(named))
    };

    let subquery = &sql[open_at + 1..close_at];
    let sub_from = find_keyword(subquery, "from")?;
    let sub_target = subquery[sub_from + 4..]
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    if sub_target.contains("information_schema") {
        Some(SelectTarget::InfoSchemaExists {
            subquery: (open_at + 1, close_at),
            alias,
        })
    } else {
        Some(SelectTarget::Engine)
    }
}

/// Answer `EXISTS (subquery)` by running `subquery` through the
/// `information_schema` evaluator and reporting whether it returned any
/// rows, the way MySQL's own `EXISTS` does. A refusal from the subquery
/// itself (an unsupported clause, an unknown relation, ...) propagates
/// unchanged — an honest error either way.
fn existence_result(
    subquery: &str,
    alias: Option<&str>,
    params: &[Value],
    catalog: &Catalog,
    session: &Session,
) -> Intercepted {
    match crate::infoschema::query(subquery, params, catalog, session) {
        Intercepted::Rows(result) => {
            let exists = !result.rows.is_empty();
            rows(
                &[alias.unwrap_or("EXISTS")],
                vec![vec![Value::Integer(exists as i64)]],
            )
        }
        other => other,
    }
}

/// Classify one statement.
///
/// `params` are the bound values of a prepared statement, used to resolve `?`
/// placeholders in the metadata queries an ORM prepares rather than sends as
/// text. They are never substituted into SQL as text — a placeholder resolves
/// to a [`Value`] inside the comparison that uses it, so there is no path here
/// where a bound value can become syntax.
pub fn intercept(
    sql: &str,
    params: &[Value],
    catalog: &Catalog,
    session: &mut Session,
) -> Intercepted {
    let sql = normalize(sql);
    if sql.is_empty() {
        return Intercepted::Ok;
    }

    match first_word(&sql).as_str() {
        "SET" => handle_set(&sql, session),
        "SHOW" => handle_show(&sql, catalog, session),
        "USE" => handle_use(&sql, catalog),
        "BEGIN" => Intercepted::Begin,
        "START" => {
            if starts_with_keyword(strip_keyword(&sql, "START").unwrap_or(""), "TRANSACTION") {
                Intercepted::Begin
            } else {
                Intercepted::Failed(MysqlError::unsupported(format!("`{sql}` is not supported")))
            }
        }
        "COMMIT" => Intercepted::Commit,
        "ROLLBACK" => handle_rollback(&sql),
        // Savepoints are how an ORM implements a nested transaction. The
        // engine has no nested transactions, and quietly answering OK would
        // make an inner "rollback" silently keep its writes — the exact class
        // of bug this shim refuses to create.
        "SAVEPOINT" => Intercepted::Failed(MysqlError::unsupported(
            "SAVEPOINT is not supported: InlaySQL has no nested transactions, so a \
             savepoint could not be rolled back to",
        )),
        "RELEASE" => Intercepted::Failed(MysqlError::unsupported(
            "RELEASE SAVEPOINT is not supported: InlaySQL has no nested transactions",
        )),
        "DESCRIBE" | "DESC" => handle_describe(&sql, catalog, session),
        // `DO expr` evaluates and discards. Nothing observable, so nothing to do.
        "DO" => Intercepted::Ok,
        "SELECT" => handle_select(&sql, params, catalog, session),
        // Everything else runs on the engine — the shim only translates it out
        // of MySQL's dialect first. See [`crate::mysqlddl`] for the DDL clauses
        // that are removed or refused, and [`crate::mysqlfunc`] for the scalar
        // functions that are mapped or refused, and why each list is drawn
        // where it is.
        _ => handle_engine_statement(&sql, catalog),
    }
}

/// Translate a statement out of MySQL's dialect and into the engine's.
///
/// Two passes, in this order and only this order:
///
/// 1. [`crate::mysqlddl`] takes the MySQL-only DDL decoration off it, refusing
///    the clauses it cannot honour. It runs first because it recognises
///    `ON UPDATE CURRENT_TIMESTAMP` and `ON UPDATE NOW()` by name — rewriting
///    `NOW()` before it looked would hide the clause it exists to refuse.
/// 2. [`crate::mysqlfunc`] rewrites the MySQL-named scalar functions into the
///    ones the engine has.
///
/// Shared with `COM_STMT_PREPARE`, which has to translate a statement it is not
/// about to run, so the two paths cannot disagree about what a statement means.
pub fn translate(sql: &str, catalog: &Catalog) -> Result<mysqlddl::Translation, MysqlError> {
    let translation = mysqlddl::translate(sql, catalog)?;
    let statements = translation
        .statements
        .iter()
        .map(|statement| mysqlfunc::rewrite(statement))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(mysqlddl::Translation {
        statements,
        dropped: translation.dropped,
    })
}

/// Turn a translation into warnings a client can read back.
pub fn translation_warnings(translation: &mysqlddl::Translation) -> Vec<Warning> {
    translation
        .dropped
        .iter()
        .map(|dropped| {
            let (code, message) = dropped.warning();
            Warning { code, message }
        })
        .collect()
}

/// Hand a statement to the engine, translated if it needed translating.
///
/// A statement with nothing to translate comes back as [`Intercepted::
/// PassThrough`] rather than as a `Rewritten` carrying an identical string, so
/// the engine really does receive the client's bytes when the shim had no
/// business touching them.
fn handle_engine_statement(sql: &str, catalog: &Catalog) -> Intercepted {
    match translate(sql, catalog) {
        Err(error) => Intercepted::Failed(error),
        Ok(translation)
            if translation.dropped.is_empty()
                && translation.statements.len() == 1
                && translation.statements[0] == sql =>
        {
            Intercepted::PassThrough
        }
        Ok(translation) => {
            let warnings = translation_warnings(&translation);
            Intercepted::Rewritten {
                statements: translation.statements,
                warnings,
            }
        }
    }
}

// ------------------------------------------------------------------ SET

fn handle_set(sql: &str, session: &mut Session) -> Intercepted {
    let rest = match strip_keyword(sql, "SET") {
        Some(rest) => rest,
        None => return Intercepted::PassThrough,
    };

    // `SET NAMES x [COLLATE y]` and `SET CHARACTER SET x` are not assignments;
    // they set a group of variables at once.
    if let Some(tail) = strip_keyword(rest, "NAMES") {
        let charset = tail
            .split_whitespace()
            .next()
            .map(unquote_identifier)
            .unwrap_or_default();
        for name in [
            "character_set_client",
            "character_set_connection",
            "character_set_results",
        ] {
            session.set_variable(name, &charset);
        }
        return Intercepted::Ok;
    }
    if strip_keyword(rest, "CHARACTER").is_some() || strip_keyword(rest, "CHARSET").is_some() {
        return Intercepted::Ok;
    }
    // `SET TRANSACTION ISOLATION LEVEL ...`: recorded, and honest, because an
    // explicit transaction really does pin its snapshot.
    if strip_keyword(rest, "TRANSACTION").is_some() {
        return Intercepted::Ok;
    }

    let mut autocommit = None;
    for assignment in split_top_level(rest, ',') {
        if assignment.trim().is_empty() {
            continue;
        }
        let Some((lhs, rhs)) = split_assignment(&assignment) else {
            // A `SET` shape nobody here recognises. Recording nothing and
            // saying OK is the right answer: session settings this server does
            // not model have no effect either way, and refusing them would
            // break connection setup for every driver.
            continue;
        };
        let value = assignment_value(&rhs);

        if let Some(user_var) = lhs.strip_prefix('@').filter(|v| !v.starts_with('@')) {
            session.set_user_variable(user_var, &value);
            continue;
        }
        let name = system_variable_name(&lhs);
        if name.eq_ignore_ascii_case("autocommit") {
            match parse_bool(&value) {
                Some(on) => autocommit = Some(on),
                None => {
                    return Intercepted::Failed(MysqlError::new(
                        1231,
                        "42000",
                        format!("Variable 'autocommit' can't be set to the value of '{value}'"),
                    ))
                }
            }
            continue;
        }
        session.set_variable(&name, &value);
    }

    match autocommit {
        Some(on) => Intercepted::SetAutocommit(on),
        None => Intercepted::Ok,
    }
}

/// Split `name = value` or `name := value` at the first top-level `=`.
fn split_assignment(text: &str) -> Option<(String, String)> {
    let bytes: Vec<char> = text.chars().collect();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, c) in bytes.iter().enumerate() {
        if let Some(q) = quote {
            if *c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(*c),
            '(' => depth += 1,
            ')' => depth -= 1,
            '=' if depth == 0 => {
                let lhs: String = bytes[..i].iter().collect();
                // `:=` — the colon belongs to the operator, not the name.
                let lhs = lhs.trim().trim_end_matches(':').trim().to_string();
                let rhs: String = bytes[i + 1..].iter().collect();
                return Some((lhs, rhs.trim().to_string()));
            }
            _ => {}
        }
    }
    None
}

/// Strip the scope prefixes a variable name can carry.
///
/// A variable arrives spelled any of half a dozen ways — `sql_mode`,
/// `SESSION sql_mode`, `@@sql_mode`, `@@session.sql_mode` — and all of them
/// name the same thing. The order below matters: `@@` comes off first, because
/// otherwise `session` in `@@session.sql_mode` is not at the start of the
/// string, and the dotted form is handled before the spaced one, because
/// `session.sql_mode` begins with the word `session` too.
fn system_variable_name(lhs: &str) -> String {
    let mut name = lhs.trim();
    name = name.strip_prefix("@@").unwrap_or(name);

    for prefix in ["session.", "global.", "local."] {
        if name
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            name = &name[prefix.len()..];
            break;
        }
    }
    for keyword in ["SESSION", "GLOBAL", "LOCAL"] {
        if let Some(rest) = strip_scope_word(name, keyword) {
            name = rest;
            break;
        }
    }
    unquote_identifier(name)
}

/// Strip `keyword` only when whitespace follows it, so `SESSION x` loses its
/// scope word and `session.x` — already handled above — is left alone.
fn strip_scope_word<'a>(name: &'a str, keyword: &str) -> Option<&'a str> {
    let head = name.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &name[keyword.len()..];
    if rest.starts_with(char::is_whitespace) {
        Some(rest.trim_start())
    } else {
        None
    }
}

/// The text of an assigned value, with quotes removed if it was a literal.
fn assignment_value(rhs: &str) -> String {
    let rhs = rhs.trim();
    unquote_string(rhs).unwrap_or_else(|| rhs.to_string())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => Some(true),
        "0" | "off" | "false" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------- transactions

fn handle_rollback(sql: &str) -> Intercepted {
    let rest = strip_keyword(sql, "ROLLBACK").unwrap_or("");
    let rest = strip_keyword(rest, "WORK").unwrap_or(rest);
    if starts_with_keyword(rest, "TO") {
        return Intercepted::Failed(MysqlError::unsupported(
            "ROLLBACK TO SAVEPOINT is not supported: InlaySQL has no nested transactions",
        ));
    }
    Intercepted::Rollback
}

fn handle_use(sql: &str, catalog: &Catalog) -> Intercepted {
    let name = unquote_identifier(strip_keyword(sql, "USE").unwrap_or("").trim());
    if name.is_empty() {
        return Intercepted::Failed(MysqlError::parse("USE needs a database name"));
    }
    match check_schema(&name, catalog) {
        Ok(()) => Intercepted::UseDatabase(name),
        Err(error) => Intercepted::Failed(error),
    }
}

/// One database file is one schema. A name that is not it, and is not one of
/// MySQL's own, is refused rather than silently aliased onto this database.
fn check_schema(name: &str, _catalog: &Catalog) -> Result<(), MysqlError> {
    if name.eq_ignore_ascii_case("information_schema")
        || name.eq_ignore_ascii_case("mysql")
        || name.eq_ignore_ascii_case("performance_schema")
        || name.eq_ignore_ascii_case("sys")
    {
        return Err(MysqlError::new(
            1044,
            "42000",
            format!("Access denied for user to database '{name}'"),
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------- SHOW

/// The words trailing a `SHOW`, split into the parts every form of it uses.
struct ShowTail {
    /// Names introduced by `FROM` or `IN`, in order.
    names: Vec<String>,
    /// The `LIKE` pattern, if there was one.
    like: Option<String>,
    /// Whether a `WHERE` clause was present.
    has_where: bool,
}

fn parse_show_tail(rest: &str) -> ShowTail {
    let mut tail = ShowTail {
        names: Vec::new(),
        like: None,
        has_where: false,
    };
    let tokens = tokenize(rest);
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if token.eq_ignore_ascii_case("from") || token.eq_ignore_ascii_case("in") {
            if let Some(name) = tokens.get(i + 1) {
                tail.names.push(name.clone());
            }
            i += 2;
            continue;
        }
        if token.eq_ignore_ascii_case("like") {
            if let Some(pattern) = tokens.get(i + 1) {
                tail.like = Some(unquote_string(pattern).unwrap_or_else(|| pattern.clone()));
            }
            i += 2;
            continue;
        }
        if token.eq_ignore_ascii_case("where") {
            tail.has_where = true;
        }
        i += 1;
    }
    tail
}

/// Split on whitespace, keeping quoted spans whole.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        if let Some(q) = quote {
            current.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                current.push(c);
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// The last component of a possibly-qualified name.
fn last_name_part(name: &str) -> String {
    split_top_level(name, '.')
        .last()
        .map(|part| unquote_identifier(part))
        .unwrap_or_default()
}

fn handle_show(sql: &str, catalog: &Catalog, session: &Session) -> Intercepted {
    let rest = match strip_keyword(sql, "SHOW") {
        Some(rest) => rest,
        None => return Intercepted::PassThrough,
    };
    // `FULL` widens the column set of TABLES and COLUMNS.
    let (full, rest) = match strip_keyword(rest, "FULL") {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    // Scope words that make no difference to what this server answers.
    let rest = strip_keyword(rest, "SESSION")
        .or_else(|| strip_keyword(rest, "GLOBAL"))
        .unwrap_or(rest);

    if starts_with_keyword(rest, "DATABASES") || starts_with_keyword(rest, "SCHEMAS") {
        return show_databases(rest, session);
    }
    if let Some(after) = strip_keyword(rest, "TABLE") {
        if starts_with_keyword(after, "STATUS") {
            return show_table_status(after, catalog, session);
        }
    }
    if starts_with_keyword(rest, "TABLES") {
        return show_tables(rest, full, catalog, session);
    }
    if starts_with_keyword(rest, "COLUMNS") || starts_with_keyword(rest, "FIELDS") {
        return show_columns(rest, full, catalog);
    }
    if starts_with_keyword(rest, "KEYS")
        || starts_with_keyword(rest, "INDEX")
        || starts_with_keyword(rest, "INDEXES")
    {
        return show_keys(rest, catalog);
    }
    if starts_with_keyword(rest, "VARIABLES") {
        return show_variables(rest, session);
    }
    if starts_with_keyword(rest, "STATUS") {
        // No status counters are kept, and inventing them would be fiction.
        return rows(&["Variable_name", "Value"], Vec::new());
    }
    if starts_with_keyword(rest, "WARNINGS") {
        // The warnings the last statement raised — one per MySQL-only clause
        // the shim removed from it. This is the other half of "nothing is
        // dropped silently": the OK packet says how many, and this says which.
        let data = session
            .warnings()
            .iter()
            .map(|warning| {
                vec![
                    Value::Text("Warning".to_string().into()),
                    Value::Integer(warning.code as i64),
                    Value::Text(warning.message.clone().into()),
                ]
            })
            .collect();
        return rows(&["Level", "Code", "Message"], data);
    }
    if starts_with_keyword(rest, "ERRORS") {
        // Errors are reported on the statement that caused them and not
        // retained, so this is always empty rather than sometimes wrong.
        return rows(&["Level", "Code", "Message"], Vec::new());
    }
    if starts_with_keyword(rest, "ENGINES") {
        return show_engines();
    }
    if let Some(after) = strip_keyword(rest, "CREATE") {
        if let Some(after) = strip_keyword(after, "TABLE") {
            return show_create_table(after, catalog);
        }
        if let Some(after) =
            strip_keyword(after, "DATABASE").or_else(|| strip_keyword(after, "SCHEMA"))
        {
            let name = unquote_identifier(after.trim());
            return rows(
                &["Database", "Create Database"],
                vec![vec![
                    Value::Text(name.clone().into()),
                    Value::Text(format!("CREATE DATABASE `{name}`").into()),
                ]],
            );
        }
    }

    Intercepted::Failed(MysqlError::unsupported(format!(
        "SHOW {} is not supported by this server",
        first_word(rest)
    )))
}

fn show_databases(rest: &str, session: &Session) -> Intercepted {
    let tail = parse_show_tail(rest);
    let name = schema_name(session);
    let matched = tail
        .like
        .as_ref()
        .map(|pattern| like_matches(pattern, &name))
        .unwrap_or(true);
    rows(
        &["Database"],
        if matched {
            vec![vec![Value::Text(name.into())]]
        } else {
            Vec::new()
        },
    )
}

fn show_tables(rest: &str, full: bool, catalog: &Catalog, session: &Session) -> Intercepted {
    let tail = parse_show_tail(rest);
    if tail.has_where {
        return Intercepted::Failed(MysqlError::unsupported(
            "SHOW TABLES ... WHERE is not supported; use LIKE, or query \
             information_schema.tables",
        ));
    }
    let schema = schema_name(session);
    let mut data = Vec::new();
    for table in catalog.tables() {
        if let Some(pattern) = &tail.like {
            if !like_matches(pattern, &table.name) {
                continue;
            }
        }
        let mut row = vec![Value::Text(table.name.clone().into())];
        if full {
            row.push(Value::Text("BASE TABLE".to_string().into()));
        }
        data.push(row);
    }

    let header = format!("Tables_in_{schema}");
    if full {
        rows(&[&header, "Table_type"], data)
    } else {
        rows(&[&header], data)
    }
}

fn show_columns(rest: &str, full: bool, catalog: &Catalog) -> Intercepted {
    let tail = parse_show_tail(rest);
    let Some(name) = tail.names.first() else {
        return Intercepted::Failed(MysqlError::parse("SHOW COLUMNS needs FROM <table>"));
    };
    let name = last_name_part(name);
    let Some(table) = catalog.table(&name) else {
        return Intercepted::Failed(MysqlError::no_such_table(&name));
    };
    columns_result(table, catalog, full, tail.like.as_deref())
}

fn handle_describe(sql: &str, catalog: &Catalog, _session: &Session) -> Intercepted {
    let rest = strip_keyword(sql, "DESCRIBE")
        .or_else(|| strip_keyword(sql, "DESC"))
        .unwrap_or("")
        .trim();
    // `DESCRIBE SELECT ...` is EXPLAIN, which is a different feature.
    if starts_with_keyword(rest, "SELECT") {
        return Intercepted::Failed(MysqlError::unsupported(
            "DESCRIBE <statement> (EXPLAIN) is not supported",
        ));
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = last_name_part(parts.next().unwrap_or(""));
    if name.is_empty() {
        return Intercepted::Failed(MysqlError::parse("DESCRIBE needs a table name"));
    }
    let Some(table) = catalog.table(&name) else {
        return Intercepted::Failed(MysqlError::no_such_table(&name));
    };
    let like = parts
        .next()
        .and_then(|rest| unquote_string(rest.trim()))
        .or_else(|| parts.next().map(str::to_string));
    columns_result(table, catalog, false, like.as_deref())
}

fn columns_result(table: &Table, catalog: &Catalog, full: bool, like: Option<&str>) -> Intercepted {
    let indexes = catalog.indexes_for(&table.name);
    let mut data = Vec::new();
    for column in &table.columns {
        if let Some(pattern) = like {
            if !like_matches(pattern, &column.name) {
                continue;
            }
        }
        let key = if column.primary_key {
            "PRI"
        } else if indexes.iter().any(|index| {
            index
                .columns
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&column.name))
        }) {
            "MUL"
        } else {
            ""
        };
        let is_text = matches!(
            column.ty,
            DataType::Text | DataType::Vector(_) | DataType::QuantizedVector(_)
        );
        // The name reported is the MySQL collation whose behaviour the column
        // actually has, not a fixed string a driver expects to read: a
        // `BINARY` column is `utf8mb4_bin` and a `NOCASE` one is
        // `utf8mb4_general_ci`, because that is now a statement about
        // comparison and not only about the encoding (AHL-469). `RTRIM` has no
        // MySQL equivalent at all — MySQL pads rather than trims — so it is
        // reported under its own name rather than under one that would
        // mislead.
        let collation = if is_text {
            Value::Text(
                crate::infoschema::mysql_collation_name(column.collation)
                    .to_string()
                    .into(),
            )
        } else {
            Value::Null
        };

        let mut row = vec![
            Value::Text(column.name.clone().into()),
            Value::Text(mysql_type_name(column.ty).into()),
        ];
        if full {
            row.push(collation);
        }
        row.extend([
            // Every column is nullable: the engine refuses NOT NULL outright,
            // so claiming otherwise would be a schema this database cannot have.
            Value::Text("YES".to_string().into()),
            Value::Text(key.to_string().into()),
            // Likewise DEFAULT — refused, therefore always absent.
            Value::Null,
            Value::Text(String::new().into()),
        ]);
        if full {
            row.push(Value::Text(
                "select,insert,update,references".to_string().into(),
            ));
            row.push(Value::Text(String::new().into()));
        }
        data.push(row);
    }

    if full {
        rows(
            &[
                "Field",
                "Type",
                "Collation",
                "Null",
                "Key",
                "Default",
                "Extra",
                "Privileges",
                "Comment",
            ],
            data,
        )
    } else {
        rows(&["Field", "Type", "Null", "Key", "Default", "Extra"], data)
    }
}

fn show_keys(rest: &str, catalog: &Catalog) -> Intercepted {
    let tail = parse_show_tail(rest);
    let Some(name) = tail.names.first() else {
        return Intercepted::Failed(MysqlError::parse("SHOW KEYS needs FROM <table>"));
    };
    let name = last_name_part(name);
    let Some(table) = catalog.table(&name) else {
        return Intercepted::Failed(MysqlError::no_such_table(&name));
    };

    let mut data = Vec::new();
    let key_row = |key_name: &str, non_unique: i64, column: &str, index_type: &str| {
        vec![
            Value::Text(table.name.clone().into()),
            Value::Integer(non_unique),
            Value::Text(key_name.to_string().into()),
            Value::Integer(1),
            Value::Text(column.to_string().into()),
            Value::Text("A".to_string().into()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Text("YES".to_string().into()),
            Value::Text(index_type.to_string().into()),
            Value::Text(String::new().into()),
            Value::Text(String::new().into()),
            Value::Text("YES".to_string().into()),
            Value::Null,
        ]
    };

    // The INTEGER PRIMARY KEY is the row id rather than a separate structure,
    // but it is the one unique key there is, and it is what an ORM looks for.
    if let Some(position) = table.rowid_alias() {
        data.push(key_row(
            "PRIMARY",
            0,
            &table.columns[position].name,
            "BTREE",
        ));
    }
    for index in catalog.indexes_for(&table.name) {
        let index_type = match index.kind {
            inlaysql::IndexKind::FullText => "FULLTEXT",
            inlaysql::IndexKind::Vector => "VECTOR",
            inlaysql::IndexKind::BTree => "BTREE",
        };
        // Retrieval indexes are not unique constraints; saying otherwise would
        // invite an ORM to build an upsert on one. A B-tree index may be one,
        // and then it is reported as one.
        let non_unique = i64::from(!index.unique);
        for column in &index.columns {
            data.push(key_row(&index.name, non_unique, column, index_type));
        }
    }

    rows(
        &[
            "Table",
            "Non_unique",
            "Key_name",
            "Seq_in_index",
            "Column_name",
            "Collation",
            "Cardinality",
            "Sub_part",
            "Packed",
            "Null",
            "Index_type",
            "Comment",
            "Index_comment",
            "Visible",
            "Expression",
        ],
        data,
    )
}

fn show_variables(rest: &str, session: &Session) -> Intercepted {
    let tail = parse_show_tail(rest);
    let data = session
        .all_variables()
        .into_iter()
        .filter(|(name, _)| {
            tail.like
                .as_ref()
                .map(|pattern| like_matches(pattern, name))
                .unwrap_or(true)
        })
        .map(|(name, value)| vec![Value::Text(name.into()), Value::Text(value.into())])
        .collect();
    rows(&["Variable_name", "Value"], data)
}

fn show_engines() -> Intercepted {
    rows(
        &[
            "Engine",
            "Support",
            "Comment",
            "Transactions",
            "XA",
            "Savepoints",
        ],
        vec![vec![
            Value::Text("InlaySQL".to_string().into()),
            Value::Text("DEFAULT".to_string().into()),
            Value::Text(
                "Copy-on-write B+ tree with MVCC and hybrid retrieval"
                    .to_string()
                    .into(),
            ),
            Value::Text("YES".to_string().into()),
            Value::Text("NO".to_string().into()),
            Value::Text("NO".to_string().into()),
        ]],
    )
}

fn show_table_status(rest: &str, catalog: &Catalog, session: &Session) -> Intercepted {
    let tail = parse_show_tail(strip_keyword(rest, "STATUS").unwrap_or(rest));
    let schema = schema_name(session);
    let data = catalog
        .tables()
        .filter(|table| {
            tail.like
                .as_ref()
                .map(|pattern| like_matches(pattern, &table.name))
                .unwrap_or(true)
        })
        .map(|table| {
            vec![
                Value::Text(table.name.clone().into()),
                Value::Text("InlaySQL".to_string().into()),
                Value::Integer(10),
                Value::Text("Dynamic".to_string().into()),
                // Row counts are not tracked; NULL says "unknown", which is
                // true, where 0 would say "empty", which may not be.
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text("utf8mb4_general_ci".to_string().into()),
                Value::Null,
                Value::Text(String::new().into()),
                Value::Text(schema.clone().into()),
            ]
        })
        .collect();
    rows(
        &[
            "Name",
            "Engine",
            "Version",
            "Row_format",
            "Rows",
            "Avg_row_length",
            "Data_length",
            "Max_data_length",
            "Index_length",
            "Data_free",
            "Auto_increment",
            "Create_time",
            "Update_time",
            "Check_time",
            "Collation",
            "Checksum",
            "Create_options",
            "Comment",
        ],
        data,
    )
}

fn show_create_table(rest: &str, catalog: &Catalog) -> Intercepted {
    let name = last_name_part(rest.trim());
    let Some(table) = catalog.table(&name) else {
        return Intercepted::Failed(MysqlError::no_such_table(&name));
    };

    let mut ddl = format!("CREATE TABLE `{}` (\n", table.name);
    let mut parts = Vec::new();
    for column in &table.columns {
        let mut part = format!("  `{}` {}", column.name, mysql_type_name(column.ty));
        if column.primary_key {
            part.push_str(" PRIMARY KEY");
        }
        parts.push(part);
    }
    for index in catalog.indexes_for(&table.name) {
        let kind = match index.kind {
            inlaysql::IndexKind::FullText => "FULLTEXT KEY",
            inlaysql::IndexKind::Vector => "VECTOR KEY",
            inlaysql::IndexKind::BTree if index.unique => "UNIQUE KEY",
            inlaysql::IndexKind::BTree => "KEY",
        };
        parts.push(format!(
            "  {} `{}` (`{}`)",
            kind,
            index.name,
            index.columns.join("`, `")
        ));
    }
    ddl.push_str(&parts.join(",\n"));
    ddl.push_str("\n)");

    rows(
        &["Table", "Create Table"],
        vec![vec![
            Value::Text(table.name.clone().into()),
            Value::Text(ddl.into()),
        ]],
    )
}

/// A catalog type, spelled the way a MySQL client expects to read one.
///
/// `VECTOR` has no MySQL equivalent and is reported under its own name rather
/// than disguised as a blob: a client that does not know it should see
/// something it does not recognise, not something it will mis-handle.
fn mysql_type_name(ty: DataType) -> String {
    match ty {
        DataType::Integer => "bigint".to_string(),
        DataType::Real => "double".to_string(),
        DataType::Text => "text".to_string(),
        DataType::Blob => "blob".to_string(),
        // `NUMERIC` is a real MySQL type name, and the closest one — but it is
        // not the same thing. Here it is SQLite's NUMERIC *affinity*: a value
        // is stored as an integer when it is exactly one and as a double
        // otherwise, per row, so there is no fixed precision or scale to
        // report. A client reading `numeric` and expecting `DECIMAL(10,2)`
        // semantics will not get them. See docs/server.md, "Divergences".
        DataType::Numeric => "numeric".to_string(),
        DataType::Vector(dim) => format!("vector({dim})"),
        DataType::QuantizedVector(dim) => format!("vector({dim},int8)"),
    }
}

// --------------------------------------------------------------- SELECT

fn handle_select(sql: &str, params: &[Value], catalog: &Catalog, session: &Session) -> Intercepted {
    // A projection with no table is only the shim's business when it names
    // something only the shim knows. `SELECT 1` belongs to the engine.
    let select_list = match select_target(sql) {
        SelectTarget::Engine => return handle_engine_statement(sql, catalog),
        SelectTarget::InfoSchema => return crate::infoschema::query(sql, params, catalog, session),
        SelectTarget::InfoSchemaExists {
            subquery: (start, end),
            alias,
        } => return existence_result(&sql[start..end], alias.as_deref(), params, catalog, session),
        SelectTarget::Session {
            select_list: (start, end),
        } => &sql[start..end],
    };

    let (select_list, _) = split_trailing_limit(select_list);
    let mut columns = Vec::new();
    let mut row = Vec::new();
    for item in split_top_level(select_list, ',') {
        if item.trim().is_empty() {
            continue;
        }
        let (expr, alias) = split_alias(&item);
        match session_expression(&expr, session) {
            Some(value) => {
                columns.push(alias.unwrap_or_else(|| expr.trim().to_string()));
                row.push(value);
            }
            None => {
                return Intercepted::Failed(MysqlError::unsupported(format!(
                    "`{}` is not something this server can evaluate without a table",
                    expr.trim()
                )))
            }
        }
    }
    if columns.is_empty() {
        return handle_engine_statement(sql, catalog);
    }
    rows_owned(columns, vec![row])
}

/// Whether a projection names something only the session knows about.
fn mentions_session_state(select_list: &str) -> bool {
    let lower = select_list.to_ascii_lowercase();
    lower.contains("@@")
        || lower.contains('@')
        || [
            "version(",
            "database(",
            "schema(",
            "last_insert_id(",
            "connection_id(",
            "user(",
            "current_user",
            "session_user",
            "system_user",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Cut a trailing `LIMIT n` off a projection, which clients append to probes.
fn split_trailing_limit(select_list: &str) -> (&str, Option<u64>) {
    match find_keyword(select_list, "limit") {
        Some(at) => {
            let limit = select_list[at + 5..].trim().parse().ok();
            (&select_list[..at], limit)
        }
        None => (select_list, None),
    }
}

/// Split `expr AS alias` or `expr alias`.
fn split_alias(item: &str) -> (String, Option<String>) {
    let item = item.trim();
    if let Some(at) = find_keyword(item, "as") {
        let expr = item[..at].trim().to_string();
        let alias = unquote_identifier(item[at + 2..].trim());
        return (expr, Some(alias));
    }
    // A bare trailing identifier is an alias only when what precedes it is a
    // complete expression; the only such shape here ends in `)` or is a
    // variable reference.
    if let Some(at) = item.rfind(char::is_whitespace) {
        let (head, tail) = item.split_at(at);
        let head = head.trim();
        let tail = tail.trim();
        let looks_complete = head.ends_with(')') || head.starts_with('@');
        let alias_like = tail
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '`' || c == '"');
        if looks_complete && alias_like && !tail.is_empty() {
            return (head.to_string(), Some(unquote_identifier(tail)));
        }
    }
    (item.to_string(), None)
}

/// Evaluate the handful of expressions the session can answer.
fn session_expression(expr: &str, session: &Session) -> Option<Value> {
    let trimmed = expr.trim();
    let lower = trimmed.to_ascii_lowercase();
    let call = lower.replace(' ', "");

    match call.as_str() {
        "version()" => return Some(Value::Text(SERVER_VERSION.to_string().into())),
        "database()" | "schema()" => {
            return Some(match &session.database {
                Some(name) => Value::Text(name.clone().into()),
                None => Value::Null,
            })
        }
        "last_insert_id()" => return Some(Value::Integer(session.last_insert_id as i64)),
        "connection_id()" => return Some(Value::Integer(session.connection_id as i64)),
        "user()" | "current_user()" | "current_user" | "session_user()" | "system_user()" => {
            return Some(Value::Text(format!("{}@localhost", session.user).into()))
        }
        "null" => return Some(Value::Null),
        _ => {}
    }

    if trimmed.starts_with("@@") {
        // Passed whole, `@@` included: `system_variable_name` peels the scope
        // off in the one order that handles every spelling.
        let name = system_variable_name(trimmed);
        return Some(match session.variable(&name) {
            Some(value) => Value::Text(value.into()),
            // An unknown system variable is NULL rather than an error, which
            // is what MySQL returns for `SELECT @@nonexistent` in a session
            // context and what drivers probing for optional variables expect.
            None => Value::Null,
        });
    }
    if let Some(name) = trimmed.strip_prefix('@') {
        return Some(match session.user_variable(&unquote_identifier(name)) {
            Some(value) => Value::Text(value.to_string().into()),
            None => Value::Null,
        });
    }
    if let Some(text) = unquote_string(trimmed) {
        return Some(Value::Text(text.into()));
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        return Some(Value::Integer(number));
    }
    None
}

// ---------------------------------------------------------------- shared

/// The schema name to report.
pub fn schema_name(session: &Session) -> String {
    session
        .database
        .clone()
        .unwrap_or_else(|| DEFAULT_SCHEMA.to_string())
}

/// Build an intercepted result set.
pub fn rows(columns: &[&str], data: Vec<Vec<Value>>) -> Intercepted {
    rows_owned(columns.iter().map(|c| c.to_string()).collect(), data)
}

/// Build an intercepted result set from owned headers.
pub fn rows_owned(columns: Vec<String>, data: Vec<Vec<Value>>) -> Intercepted {
    Intercepted::Rows(Box::new(ResultSet {
        columns,
        rows: data,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlaysql::{Column, Index, IndexKind};

    fn catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                name: "docs".to_string(),
                columns: vec![
                    Column::primary_key("id", DataType::Integer),
                    Column::new("body", DataType::Text),
                    Column::new("score", DataType::Real),
                    Column::new("embedding", DataType::Vector(4)),
                ],
            })
            .unwrap();
        catalog
            .create_table(Table {
                name: "user_roles".to_string(),
                columns: vec![Column::new("role", DataType::Text)],
            })
            .unwrap();
        catalog
            .create_index(Index {
                name: "docs_body".to_string(),
                table: "docs".to_string(),
                columns: vec!["body".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: vec![inlaysql::Collation::Binary],
            })
            .unwrap();
        catalog
    }

    fn session() -> Session {
        Session::new(
            1,
            "root",
            Some("app".to_string()),
            crate::session::Limits::default(),
        )
    }

    fn run(sql: &str) -> Intercepted {
        intercept(sql, &[], &catalog(), &mut session())
    }

    fn result(sql: &str) -> ResultSet {
        match run(sql) {
            Intercepted::Rows(rows) => *rows,
            other => panic!("{sql} was not answered with rows: {other:?}"),
        }
    }

    fn text(value: &Value) -> String {
        match value {
            Value::Text(t) => t.to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Null => "NULL".to_string(),
            other => format!("{other:?}"),
        }
    }

    fn column(rows: &ResultSet, name: &str) -> Vec<String> {
        let at = rows
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("no column {name} in {:?}", rows.columns));
        rows.rows.iter().map(|row| text(&row[at])).collect()
    }

    // --------------------------------------------------------- SHOW TABLES

    #[test]
    fn show_tables_lists_the_catalog() {
        let rows = result("SHOW TABLES");
        assert_eq!(rows.columns, vec!["Tables_in_app"]);
        assert_eq!(column(&rows, "Tables_in_app"), vec!["docs", "user_roles"]);
    }

    #[test]
    fn show_full_tables_adds_the_type_column() {
        let rows = result("SHOW FULL TABLES");
        assert_eq!(rows.columns, vec!["Tables_in_app", "Table_type"]);
        assert_eq!(
            column(&rows, "Table_type"),
            vec!["BASE TABLE", "BASE TABLE"]
        );
    }

    #[test]
    fn show_tables_honours_a_like_pattern_including_its_escapes() {
        assert_eq!(
            column(&result("SHOW TABLES LIKE 'doc%'"), "Tables_in_app"),
            vec!["docs"]
        );
        // The escaped underscore must not match `docs`-shaped names.
        assert_eq!(
            column(&result("SHOW TABLES LIKE 'user\\_roles'"), "Tables_in_app"),
            vec!["user_roles"]
        );
        assert!(result("SHOW TABLES LIKE 'nothing%'").rows.is_empty());
    }

    /// A filter this server cannot evaluate must be an error. Returning every
    /// table, or none, would both be confidently wrong answers.
    #[test]
    fn show_tables_with_an_unsupported_filter_is_refused() {
        match run("SHOW TABLES WHERE Tables_in_app = 'docs'") {
            Intercepted::Failed(error) => assert_eq!(error.code, 1235),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // -------------------------------------------------------- SHOW COLUMNS

    #[test]
    fn show_columns_describes_the_table() {
        let rows = result("SHOW COLUMNS FROM docs");
        assert_eq!(
            rows.columns,
            vec!["Field", "Type", "Null", "Key", "Default", "Extra"]
        );
        assert_eq!(
            column(&rows, "Field"),
            vec!["id", "body", "score", "embedding"]
        );
        assert_eq!(
            column(&rows, "Type"),
            vec!["bigint", "text", "double", "vector(4)"]
        );
        assert_eq!(column(&rows, "Key"), vec!["PRI", "MUL", "", ""]);
    }

    /// The engine refuses NOT NULL and DEFAULT outright, so every column really
    /// is nullable with no default. Reporting anything else would describe a
    /// schema this database cannot hold.
    #[test]
    fn show_columns_does_not_invent_constraints_the_engine_refuses() {
        let rows = result("SHOW COLUMNS FROM docs");
        assert!(column(&rows, "Null").iter().all(|v| v == "YES"));
        assert!(column(&rows, "Default").iter().all(|v| v == "NULL"));
    }

    #[test]
    fn show_full_columns_adds_collation_and_privileges() {
        let rows = result("SHOW FULL COLUMNS FROM docs");
        assert_eq!(
            rows.columns,
            vec![
                "Field",
                "Type",
                "Collation",
                "Null",
                "Key",
                "Default",
                "Extra",
                "Privileges",
                "Comment"
            ]
        );
        // The reported name follows the column's declared collation, which for
        // a column that declared none is `BINARY` — and `utf8mb4_bin` is the
        // MySQL collation that behaves the way this column really does. Saying
        // `utf8mb4_general_ci` here, as this did before AHL-469, told a client
        // the comparison was case-insensitive when it was not.
        assert_eq!(
            column(&rows, "Collation"),
            vec!["NULL", "utf8mb4_bin", "NULL", "utf8mb4_bin"]
        );
    }

    /// A `NOCASE` column reports the name of a MySQL collation that really is
    /// case-insensitive.
    #[test]
    fn a_nocase_column_reports_a_case_insensitive_collation_name() {
        let mut catalog = catalog();
        catalog
            .create_table(Table {
                name: "people".to_string(),
                columns: vec![
                    Column::new("plain", DataType::Text),
                    Column::new("folded", DataType::Text)
                        .with_collation(inlaysql::Collation::NoCase),
                ],
            })
            .unwrap();
        let rows = match intercept(
            "SHOW FULL COLUMNS FROM people",
            &[],
            &catalog,
            &mut session(),
        ) {
            Intercepted::Rows(rows) => *rows,
            other => panic!("not answered with rows: {other:?}"),
        };
        assert_eq!(
            column(&rows, "Collation"),
            vec!["utf8mb4_bin", "utf8mb4_general_ci"]
        );
    }

    #[test]
    fn show_columns_accepts_backticks_and_qualified_names() {
        assert_eq!(
            column(&result("SHOW COLUMNS FROM `docs`"), "Field").len(),
            4
        );
        assert_eq!(
            column(&result("SHOW COLUMNS FROM app.docs"), "Field").len(),
            4
        );
        assert_eq!(
            column(&result("SHOW COLUMNS FROM `app`.`docs`"), "Field").len(),
            4
        );
    }

    #[test]
    fn describe_is_show_columns() {
        assert_eq!(
            result("DESCRIBE docs").columns,
            result("SHOW COLUMNS FROM docs").columns
        );
        assert_eq!(column(&result("DESC docs"), "Field").len(), 4);
    }

    #[test]
    fn show_columns_on_a_missing_table_is_1146() {
        match run("SHOW COLUMNS FROM nope") {
            Intercepted::Failed(error) => {
                assert_eq!(error.code, 1146);
                assert!(error.message.contains("nope"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // ----------------------------------------------------------- SHOW KEYS

    #[test]
    fn show_keys_reports_the_primary_key_and_the_retrieval_indexes() {
        let rows = result("SHOW KEYS FROM docs");
        assert_eq!(column(&rows, "Key_name"), vec!["PRIMARY", "docs_body"]);
        assert_eq!(column(&rows, "Column_name"), vec!["id", "body"]);
        assert_eq!(column(&rows, "Index_type"), vec!["BTREE", "FULLTEXT"]);
        // A BM25 index is not a unique constraint, and must not look like one.
        assert_eq!(column(&rows, "Non_unique"), vec!["0", "1"]);
    }

    #[test]
    fn show_index_is_a_synonym_for_show_keys() {
        assert_eq!(
            result("SHOW INDEX FROM docs").columns,
            result("SHOW KEYS FROM docs").columns
        );
    }

    // ------------------------------------------------------ SHOW VARIABLES

    #[test]
    fn show_variables_filters_with_like() {
        let rows = result("SHOW VARIABLES LIKE 'version'");
        assert_eq!(column(&rows, "Variable_name"), vec!["version"]);
        assert_eq!(column(&rows, "Value"), vec![SERVER_VERSION]);

        let many = result("SHOW VARIABLES LIKE 'character_set%'");
        assert!(many.rows.len() >= 5, "got {:?}", many.rows);
    }

    #[test]
    fn show_create_table_renders_the_catalog_definition() {
        let rows = result("SHOW CREATE TABLE docs");
        let ddl = column(&rows, "Create Table").remove(0);
        assert!(ddl.contains("CREATE TABLE `docs`"), "{ddl}");
        assert!(ddl.contains("`id` bigint PRIMARY KEY"), "{ddl}");
        assert!(ddl.contains("`embedding` vector(4)"), "{ddl}");
        assert!(ddl.contains("FULLTEXT KEY `docs_body`"), "{ddl}");
    }

    #[test]
    fn an_unknown_show_is_refused_rather_than_passed_to_the_engine() {
        match run("SHOW TRIGGERS") {
            Intercepted::Failed(error) => assert_eq!(error.code, 1235),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // ------------------------------------------------------------ sessions

    #[test]
    fn session_functions_are_answered_without_the_engine() {
        assert_eq!(
            column(&result("SELECT VERSION()"), "VERSION()"),
            vec![SERVER_VERSION]
        );
        assert_eq!(
            column(&result("SELECT DATABASE()"), "DATABASE()"),
            vec!["app"]
        );
        assert_eq!(
            column(&result("SELECT CONNECTION_ID()"), "CONNECTION_ID()"),
            vec!["1"]
        );
        assert_eq!(
            column(&result("SELECT USER()"), "USER()"),
            vec!["root@localhost"]
        );
    }

    #[test]
    fn database_is_null_when_no_schema_is_selected() {
        let mut session = Session::new(1, "root", None, crate::session::Limits::default());
        match intercept("SELECT DATABASE()", &[], &catalog(), &mut session) {
            Intercepted::Rows(rows) => assert_eq!(rows.rows[0][0], Value::Null),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn system_variables_are_answered_with_and_without_a_trailing_limit() {
        assert_eq!(
            column(
                &result("SELECT @@version_comment LIMIT 1"),
                "@@version_comment"
            )
            .len(),
            1
        );
        assert_eq!(
            column(&result("SELECT @@session.sql_mode"), "@@session.sql_mode"),
            vec!["STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION"]
        );
        // An unknown system variable is NULL, as it is in MySQL.
        assert_eq!(result("SELECT @@wibble").rows[0][0], Value::Null);
    }

    /// Every spelling of a scoped variable has to reach the same name. The
    /// `@@session.x` form is the one that broke: `session` is a prefix of the
    /// name only once `@@` has been removed, so the order of the two strips
    /// decides whether it resolves at all.
    #[test]
    fn every_spelling_of_a_scoped_variable_names_the_same_thing() {
        for spelling in [
            "sql_mode",
            "SESSION sql_mode",
            "GLOBAL sql_mode",
            "LOCAL sql_mode",
            "@@sql_mode",
            "@@session.sql_mode",
            "@@global.sql_mode",
            "@@local.sql_mode",
            "@@SESSION.SQL_MODE",
            "`sql_mode`",
        ] {
            // Case is preserved here and folded by the lookup, so compare the
            // way `Session::variable` does.
            assert!(
                system_variable_name(spelling).eq_ignore_ascii_case("sql_mode"),
                "{spelling} became {}",
                system_variable_name(spelling)
            );
        }
        // A name that merely starts with a scope word keeps all of it.
        assert_eq!(
            system_variable_name("session_track_gtids"),
            "session_track_gtids"
        );
        assert_eq!(system_variable_name("@@globally_unique"), "globally_unique");
    }

    #[test]
    fn every_spelling_of_a_scoped_variable_reads_back_through_select() {
        for spelling in [
            "@@sql_mode",
            "@@session.sql_mode",
            "@@SESSION.sql_mode",
            "@@global.sql_mode",
        ] {
            let rows = result(&format!("SELECT {spelling}"));
            assert_eq!(
                rows.rows[0][0],
                Value::Text(
                    "STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION"
                        .to_string()
                        .into()
                ),
                "{spelling}"
            );
        }
    }

    #[test]
    fn aliases_are_honoured() {
        let rows = result("SELECT VERSION() AS v, DATABASE() AS db");
        assert_eq!(rows.columns, vec!["v", "db"]);
        assert_eq!(rows.rows[0][1], Value::Text("app".to_string().into()));
    }

    /// An ordinary query must reach the engine untouched — the shim is not
    /// allowed to start answering real SQL.
    #[test]
    fn ordinary_statements_pass_through() {
        for sql in [
            "SELECT 1",
            "SELECT id, body FROM docs",
            "INSERT INTO docs (id) VALUES (1)",
            "UPDATE docs SET body = 'x'",
            "DELETE FROM docs",
            "SELECT COUNT(*) FROM docs",
        ] {
            assert!(
                matches!(run(sql), Intercepted::PassThrough),
                "{sql} should have gone to the engine"
            );
        }
    }

    /// DDL goes to the engine too. It takes the translation route so MySQL-only
    /// decoration can come off it, but a statement with none is handed over as
    /// `PassThrough` — the engine receives the client's own bytes rather than
    /// anything this crate re-rendered.
    #[test]
    fn ddl_without_mysql_decoration_reaches_the_engine_unchanged() {
        for statement in [
            "CREATE TABLE t (a INTEGER)",
            "ALTER TABLE t ADD COLUMN b INT",
        ] {
            assert!(
                matches!(run(statement), Intercepted::PassThrough),
                "{statement} should have reached the engine untouched"
            );
        }
    }

    /// The statement that used to be a syntax error. It reaches the engine as
    /// ordinary SQL, and every clause taken off it is reported.
    #[test]
    fn mysql_ddl_is_translated_and_the_removals_are_reported() {
        match run("CREATE TABLE t (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY) ENGINE=InnoDB") {
            Intercepted::Rewritten {
                statements,
                warnings,
            } => {
                assert_eq!(statements, vec!["CREATE TABLE t (id BIGINT PRIMARY KEY)"]);
                assert_eq!(warnings.len(), 3);
                assert!(warnings.iter().all(|w| w.code == 1618));
            }
            other => panic!("{other:?}"),
        }
    }

    /// AHL-474: `ADD INDEX` reaches the engine as its own free-standing
    /// `CREATE INDEX`, and — since nothing about the table is different,
    /// only which statement says so — with no warning attached.
    #[test]
    fn add_index_reaches_the_engine_as_a_separate_create_index_statement() {
        match run("ALTER TABLE docs ADD INDEX docs_score_index (score)") {
            Intercepted::Rewritten {
                statements,
                warnings,
            } => {
                assert_eq!(
                    statements,
                    vec!["CREATE INDEX `docs_score_index` ON docs (`score`)"]
                );
                assert!(
                    warnings.is_empty(),
                    "a repositioning is not a dropped clause"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// AHL-474: a post-creation `ADD CONSTRAINT ... FOREIGN KEY` has nowhere
    /// in core to be recorded, so nothing runs on the engine at all — but the
    /// reply still carries a `1618` naming what was not recorded, never a
    /// silent success.
    #[test]
    fn a_post_creation_foreign_key_touches_the_engine_not_at_all() {
        match run(
            "ALTER TABLE docs ADD CONSTRAINT docs_role_foreign FOREIGN KEY (score) \
             REFERENCES user_roles (role)",
        ) {
            Intercepted::Rewritten {
                statements,
                warnings,
            } => {
                assert!(
                    statements.is_empty(),
                    "nothing here can be recorded, so nothing runs: {statements:?}"
                );
                assert_eq!(warnings.len(), 1);
                assert_eq!(warnings[0].code, 1618);
                assert!(
                    warnings[0]
                        .message
                        .to_ascii_lowercase()
                        .contains("foreign key"),
                    "{}",
                    warnings[0].message
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// A refusal from the translation is an error the client sees, not a
    /// statement quietly stripped of the clause it asked for.
    #[test]
    fn a_ddl_clause_that_cannot_be_honoured_is_refused_by_the_shim() {
        match run("CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT AUTO_INCREMENT)") {
            Intercepted::Failed(error) => assert_eq!(error.code, 1235),
            other => panic!("{other:?}"),
        }
    }

    /// The order of the two translation passes is load-bearing.
    ///
    /// [`crate::mysqlddl`] refuses `ON UPDATE NOW()` by recognising `NOW` after
    /// `ON UPDATE`. If the function pass ran first it would have turned that
    /// into `datetime('now')`, the DDL pass would no longer see the clause it
    /// exists to refuse, and a column that silently stops tracking its row's
    /// last update would be created instead.
    #[test]
    fn the_ddl_pass_runs_before_the_function_pass() {
        match run("CREATE TABLE t (a INT, ts TIMESTAMP ON UPDATE NOW())") {
            Intercepted::Failed(error) => {
                assert_eq!(error.code, 1235);
                assert!(error.message.contains("ON UPDATE"), "{error}");
            }
            other => panic!("ON UPDATE NOW() must still be refused, got {other:?}"),
        }
    }

    /// A MySQL-named function in an ordinary statement is rewritten on its way
    /// to the engine, and nothing is warned about — the mapping means the same
    /// thing, unlike a dropped DDL clause.
    #[test]
    fn a_mysql_named_function_is_rewritten_on_the_way_to_the_engine() {
        match run("SELECT CONCAT(a, b) FROM docs WHERE CHAR_LENGTH(body) > 3") {
            Intercepted::Rewritten {
                statements,
                warnings,
            } => {
                assert_eq!(
                    statements,
                    vec!["SELECT ('' || a || b) FROM docs WHERE length(body) > 3"]
                );
                assert!(warnings.is_empty(), "a mapping is not a dropped clause");
            }
            other => panic!("{other:?}"),
        }
        // A table-less one goes the same way rather than being answered here.
        match run("SELECT NOW()") {
            Intercepted::Rewritten { statements, .. } => {
                assert_eq!(statements, vec!["SELECT datetime('now')"])
            }
            other => panic!("{other:?}"),
        }
    }

    /// AHL-475's pass runs in the same DDL-translation step, ahead of the
    /// function pass, and the two combine cleanly: a qualified assignment
    /// target loses its qualifier, and a MySQL-named function in the same
    /// statement is still mapped.
    #[test]
    fn a_qualified_set_target_and_a_mysql_function_combine_in_one_statement() {
        match run("UPDATE docs SET body = CONCAT(body, '!'), docs.score = 1 WHERE id = 1") {
            Intercepted::Rewritten {
                statements,
                warnings,
            } => {
                assert_eq!(
                    statements,
                    vec!["UPDATE docs SET body = ('' || body || '!'), score = 1 WHERE id = 1"]
                );
                assert!(warnings.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    /// And a function the shim will not map is refused before the engine sees
    /// it, so the message can say why the obvious mapping is not there.
    #[test]
    fn an_unmappable_function_is_refused_by_the_shim() {
        for sql in [
            "SELECT GREATEST(a, b) FROM docs",
            "UPDATE docs SET body = CONCAT_WS('-', body, 'x')",
            "DELETE FROM docs WHERE DATEDIFF(a, b) > 1",
            "INSERT INTO docs (body) VALUES (DATE_FORMAT(a, '%Y'))",
        ] {
            match run(sql) {
                Intercepted::Failed(error) => assert_eq!(error.code, 1235, "{sql}"),
                other => panic!("{sql}: {other:?}"),
            }
        }
    }

    /// `SHOW WARNINGS` must not clear the list it is about to read.
    #[test]
    fn only_the_statements_that_read_warnings_keep_them() {
        assert!(reads_warnings("SHOW WARNINGS"));
        assert!(reads_warnings("show warnings limit 5"));
        assert!(reads_warnings("SHOW SESSION ERRORS"));
        assert!(!reads_warnings("SHOW TABLES"));
        assert!(!reads_warnings("SELECT 1"));
    }

    #[test]
    fn show_warnings_reports_what_the_last_statement_dropped() {
        let mut session = session();
        session.set_warnings(vec![Warning {
            code: 1618,
            message: "`ENGINE = InnoDB` was ignored: one storage engine".to_string(),
        }]);
        match intercept("SHOW WARNINGS", &[], &catalog(), &mut session) {
            Intercepted::Rows(rows) => {
                assert_eq!(rows.columns, vec!["Level", "Code", "Message"]);
                assert_eq!(rows.rows[0][0], Value::Text("Warning".to_string().into()));
                assert_eq!(rows.rows[0][1], Value::Integer(1618));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn from_dual_is_treated_as_no_table() {
        assert_eq!(
            column(&result("SELECT VERSION() FROM DUAL"), "VERSION()").len(),
            1
        );
    }

    // ---------------------------------------------------------------- SET

    #[test]
    fn set_names_records_the_charset_and_answers_ok() {
        let mut session = session();
        assert!(matches!(
            intercept("SET NAMES utf8mb4", &[], &catalog(), &mut session),
            Intercepted::Ok
        ));
        assert_eq!(
            session.variable("character_set_client").as_deref(),
            Some("utf8mb4")
        );
    }

    #[test]
    fn ordinary_session_sets_are_no_ops_that_still_read_back() {
        let mut session = session();
        assert!(matches!(
            intercept(
                "SET SESSION sql_mode = 'ANSI', @@global.time_zone = '+00:00'",
                &[],
                &catalog(),
                &mut session
            ),
            Intercepted::Ok
        ));
        assert_eq!(session.variable("sql_mode").as_deref(), Some("ANSI"));
        assert_eq!(session.variable("time_zone").as_deref(), Some("+00:00"));
    }

    #[test]
    fn setting_autocommit_is_not_a_no_op() {
        let mut session = session();
        assert!(matches!(
            intercept("SET autocommit=0", &[], &catalog(), &mut session),
            Intercepted::SetAutocommit(false)
        ));
        assert!(matches!(
            intercept(
                "SET @@session.autocommit = ON",
                &[],
                &catalog(),
                &mut session
            ),
            Intercepted::SetAutocommit(true)
        ));
    }

    #[test]
    fn user_variables_round_trip() {
        let mut session = session();
        intercept("SET @x = 'hello'", &[], &catalog(), &mut session);
        match intercept("SELECT @x", &[], &catalog(), &mut session) {
            Intercepted::Rows(rows) => assert_eq!(rows.rows[0][0], Value::Text("hello".into())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_version_gated_set_is_swallowed_whole() {
        assert!(matches!(
            run("/*!40101 SET @@SESSION.sql_mode = 'X' */"),
            Intercepted::Ok
        ));
    }

    // -------------------------------------------------------- transactions

    #[test]
    fn transaction_statements_map_onto_the_engine_api() {
        assert!(matches!(run("BEGIN"), Intercepted::Begin));
        assert!(matches!(run("START TRANSACTION"), Intercepted::Begin));
        assert!(matches!(run("COMMIT"), Intercepted::Commit));
        assert!(matches!(run("ROLLBACK"), Intercepted::Rollback));
    }

    /// The one that would silently lose data if it answered OK: an ORM's
    /// nested transaction must fail loudly, not appear to work.
    #[test]
    fn savepoints_are_refused_rather_than_faked() {
        for sql in [
            "SAVEPOINT trans1",
            "ROLLBACK TO SAVEPOINT trans1",
            "RELEASE SAVEPOINT trans1",
        ] {
            match run(sql) {
                Intercepted::Failed(error) => assert_eq!(error.code, 1235, "{sql}"),
                other => panic!("{sql} should be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn use_selects_a_schema_but_refuses_mysqls_own() {
        match run("USE app") {
            Intercepted::UseDatabase(name) => assert_eq!(name, "app"),
            other => panic!("{other:?}"),
        }
        match run("USE mysql") {
            Intercepted::Failed(error) => assert_eq!(error.code, 1044),
            other => panic!("{other:?}"),
        }
    }

    // ------------------------------------------------- EXISTS(information_schema)

    /// The exact statement `Illuminate\Database\Schema\Builder::hasTable()`
    /// sends — verbatim from a real Laravel 11 migration run against this
    /// server, which is how this was found: it reached `session_expression`
    /// (not `infoschema::query`) and failed there, because the subquery's own
    /// `schema()` call made `mentions_session_state` true for the *outer*
    /// statement even though the outer statement has no top-level `FROM` for
    /// the ordinary dispatch to see. `docs` exists in `catalog()`, `nope`
    /// does not — both directions are asserted so a fix that always returns
    /// `true` would be caught.
    #[test]
    fn exists_wrapping_an_information_schema_subquery_answers_has_table() {
        let rows = result(
            "select exists (select 1 from information_schema.tables where \
             table_schema = schema() and table_name = 'docs' and table_type \
             in ('BASE TABLE', 'SYSTEM VERSIONED')) as `exists`",
        );
        assert_eq!(rows.columns, vec!["exists"]);
        assert_eq!(column(&rows, "exists"), vec!["1"]);

        let rows = result(
            "select exists (select 1 from information_schema.tables where \
             table_schema = schema() and table_name = 'nope' and table_type \
             in ('BASE TABLE', 'SYSTEM VERSIONED')) as `exists`",
        );
        assert_eq!(column(&rows, "exists"), vec!["0"]);
    }

    /// The same idiom without `EXISTS` — `SELECT 1 FROM information_schema...`
    /// — which some ORMs use directly as an existence probe. `1` is a
    /// constant projected once per matching row, not a column reference;
    /// before this was recognised the shim tried to resolve `"1"` as a column
    /// name against `TABLES_COLUMNS` and refused with error 1054.
    #[test]
    fn a_bare_literal_projects_once_per_row_in_an_information_schema_query() {
        let rows = result("select 1 from information_schema.tables where table_name = 'docs'");
        assert_eq!(column(&rows, "1"), vec!["1"]);
    }

    /// `EXISTS (subquery)` over a *real* table must still reach the engine,
    /// unaffected by the `information_schema` special case above — the same
    /// dispatch bug this shim carried would have misrouted this one too,
    /// since its `WHERE` also mentions `schema()`.
    #[test]
    fn exists_over_a_real_table_is_not_claimed_by_the_shim() {
        assert!(!handles(
            "select exists (select 1 from docs where body = schema()) as `exists`"
        ));
    }

    /// An alias-free `EXISTS (...)` still gets a sane column name.
    #[test]
    fn exists_probe_defaults_its_column_name_when_unaliased() {
        let rows = result(
            "select exists (select 1 from information_schema.tables where \
             table_name = 'docs')",
        );
        assert_eq!(rows.columns, vec!["EXISTS"]);
    }

    /// A trailer after the closing `)` that is not a clean alias (here,
    /// arithmetic on the boolean) means `existence_probe` does not recognise
    /// the shape at all — per this shim's rule of refusing to guess, it must
    /// not silently drop the `+ 1` and answer as if it were not there.
    /// `select_target` is exercised directly because the honest outcome from
    /// here on (the whole statement passed to the real engine, which this
    /// unit test harness does not run) is `PassThrough`, not a value this
    /// test could otherwise distinguish from "answered wrong".
    #[test]
    fn exists_probe_refuses_a_dirty_trailer_rather_than_guessing() {
        let clean = "select exists (select 1 from information_schema.tables \
                      where table_name = 'docs') as `exists`";
        assert!(matches!(
            select_target(clean),
            SelectTarget::InfoSchemaExists { .. }
        ));

        let dirty = "select exists (select 1 from information_schema.tables \
                      where table_name = 'docs') + 1";
        assert!(!matches!(
            select_target(dirty),
            SelectTarget::InfoSchemaExists { .. }
        ));
    }
}
