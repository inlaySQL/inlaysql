//! MySQL-only DDL decoration, translated into the dialect the engine speaks.
//!
//! This is decision **D1** in `docs/architecture.md` applied to DDL. `inlaysql-core` speaks
//! SQLite's dialect and keeps speaking it; `AUTO_INCREMENT`, `ENGINE=InnoDB`,
//! `DEFAULT CHARSET=utf8mb4` and the rest are MySQL spellings with no place in
//! it. They arrive in the first statement of nearly every ORM migration, so
//! they are recognised and removed here, before the statement reaches the
//! engine — never added to the engine's grammar.
//!
//! # The rule this module is built around
//!
//! **A clause this server cannot faithfully honour is refused, never accepted
//! and ignored.** A statement that reports success while building something
//! else is the worst failure mode a database has: the migration moves on, and
//! everything after it is written against a schema that was never created. So
//! every clause below is in exactly one of two lists, and the code says which:
//!
//! * **Neutralised** — the clause describes something InlaySQL already does, or
//!   something with no observable effect here. Removing it changes nothing
//!   about the table that gets built. Each removal is still reported: it comes
//!   back as a MySQL warning (`1618 ER_WARN_OPTION_IGNORED`), so
//!   `SHOW WARNINGS` after a `CREATE TABLE` lists every clause that was
//!   dropped and why. Nothing here is silent.
//! * **Refused** — the clause changes what the table means, and InlaySQL cannot
//!   reproduce it. The statement fails with a MySQL error code that names the
//!   clause.
//!
//! # What is *not* this module's business
//!
//! `NOT NULL`, `DEFAULT`, `UNIQUE`, `CHECK`, a foreign key declared *inside*
//! `CREATE TABLE`, `DATETIME` / `TIMESTAMP` / `JSON` column types and
//! SQLite's own four `ALTER TABLE` operations are all ordinary SQL the
//! engine already implements (Phase 1b, AHL-412). They are left exactly as
//! written so the engine runs them in its own words.
//!
//! # Post-creation index and constraint DDL (Phase 3, AHL-474)
//!
//! Laravel's schema builder — and every ORM modelled on it — never inlines a
//! fluent `->unique()`/`->index()`/`->foreign()` into `CREATE TABLE`. It
//! compiles a *separate* `ALTER TABLE ... ADD {INDEX|UNIQUE|CONSTRAINT}`
//! straight after. Core has the target syntax for the index cases
//! (`CREATE INDEX`, `CREATE UNIQUE INDEX`, `DROP INDEX`, all pre-existing) but
//! no `ALTER TABLE` operation for any of them — SQLite's own `ALTER TABLE`
//! only ever had `ADD COLUMN`, `RENAME TO`, `RENAME COLUMN` and `DROP
//! COLUMN`. So this module rewrites the MySQL shapes onto the free-standing
//! statements core already runs, *before* the engine's parser ever sees an
//! operation it has no name for:
//!
//! * `ADD INDEX|KEY [name] (cols)` → `CREATE INDEX [name] ON t (cols)`.
//! * `ADD UNIQUE [INDEX|KEY] [name] (cols)` and
//!   `ADD CONSTRAINT name UNIQUE (cols)` → `CREATE UNIQUE INDEX name ON t
//!   (cols)`.
//! * `DROP INDEX|KEY name` → the standalone `DROP INDEX name` — MySQL scopes
//!   an index name to its table; SQLite's index names are global to the
//!   database, so the table qualifier has nowhere to go and is simply not
//!   needed to say the same thing.
//! * `ADD CONSTRAINT name FOREIGN KEY ...` has no ALTER path at all: there is
//!   nowhere in the catalog to *record* a foreign key added after the table
//!   exists (only `CREATE TABLE` can), so it never reaches the engine.
//!   Answering `1235` would be wrong, though — a foreign key core *did*
//!   record (one written inline in `CREATE TABLE`) is unenforced too,
//!   SQLite's own long-standing default, not a gap (see `docs/server.md`).
//!   So this is OK, with a `1618` naming the constraint that was not
//!   recorded and will never be checked.
//! * `RENAME INDEX a TO b` is refused (`1235`): core has no rename for an
//!   index, only drop-and-recreate, which is a different statement with a
//!   window where the index does not exist at all.
//!
//! `TRUNCATE TABLE t` and standalone `RENAME TABLE a TO b` are handled here
//! too, for the same reason: MySQL statement shapes core's SQLite dialect
//! does not have, translated onto the ones it does (`DELETE FROM t` and
//! `ALTER TABLE a RENAME TO b`).
//!
//! ## Multiple operations, and why the result is not atomic
//!
//! MySQL's `ALTER TABLE` accepts a comma-separated list of operations in one
//! statement (`ADD COLUMN x INT, ADD INDEX (x)`); SQLite's applies exactly
//! one per statement, and the engine already refuses more than one outright.
//! [`translate`] therefore returns *every* statement one MySQL `ALTER TABLE`
//! expands to, in [`Translation::statements`], and the caller
//! ([`crate::connection`]) runs them one at a time against the engine.
//! **That sequence is not atomic the way MySQL's single statement is.** If
//! the third of five operations fails, the first two already happened and
//! the last two never will; the error names the operation that failed, and
//! nothing here undoes the ones before it. Wrapping the whole `ALTER TABLE`
//! in an explicit transaction on the client side is the only way to get
//! atomicity back.
//!
//! Most of what is translated here — `ADD INDEX`, `DROP INDEX`, the standalone
//! `RENAME TABLE` — carries no `1618`, on the same reasoning
//! `crate::mysqlfunc` uses for a scalar-function mapping that means the same
//! thing it always did: nothing about the table is different, only which
//! statement says so. A warning fires only where something is genuinely lost
//! — an unrecorded foreign key, or `TRUNCATE`'s row-id reset — or where the
//! request cannot be honoured at all.
//!
//! ## A qualified column on the left of `UPDATE ... SET` (AHL-475)
//!
//! `UPDATE users SET name = ?, users.updated_at = ?` is what Eloquent writes
//! on every save of a model with timestamps, and it is real MySQL syntax —
//! but checked directly against a real `sqlite3` binary, a qualified
//! assignment target is a syntax error in *every* case there, including the
//! statement's own table name. So this is not a SQLite feature core is
//! missing (decision D1's own test); [`update_set`] strips a qualifier that
//! names the statement's own table or alias and refuses one that does not,
//! by name, before the statement ever reaches core's parser. No warning: the
//! table and the column are exactly what was written, only the spelling
//! changed.

use crate::errors::MysqlError;
use inlaysql::Catalog;

/// A clause that was removed — or, since AHL-469, narrowed — and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    /// The clause as it was written, re-rendered from its tokens.
    pub clause: String,
    /// What a reader needs to know about its removal.
    pub reason: String,
    /// What the clause became, when it was translated into something the
    /// engine can honour rather than removed outright.
    ///
    /// `Some` only for a MySQL collation mapped onto one of SQLite's three.
    /// The warning still fires: `utf8mb4_unicode_ci` becoming `NOCASE` is a
    /// *narrowing* — case folding without accent folding — and a client that
    /// is not told would read equality it does not get.
    pub mapped_to: Option<String>,
}

impl Dropped {
    fn new(clause: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            clause: clause.into(),
            reason: reason.into(),
            mapped_to: None,
        }
    }

    /// A clause that was translated rather than removed.
    fn mapped(
        clause: impl Into<String>,
        onto: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            clause: clause.into(),
            reason: reason.into(),
            mapped_to: Some(onto.into()),
        }
    }

    /// The warning a client sees.
    ///
    /// `1618` is `ER_WARN_OPTION_IGNORED`, whose whole meaning is "the server
    /// understood this option and did not apply it" — exactly what happened,
    /// and near enough for a mapping that applied *part* of it.
    pub fn warning(&self) -> (u16, String) {
        match &self.mapped_to {
            Some(onto) => (
                1618,
                format!(
                    "`{}` was mapped to `COLLATE {onto}`: {}",
                    self.clause, self.reason
                ),
            ),
            None => (
                1618,
                format!("`{}` was ignored: {}", self.clause, self.reason),
            ),
        }
    }
}

// ---------------------------------------------------------------- collations

/// What one MySQL collation name becomes in the engine's dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mapped {
    /// The engine's collation means exactly what MySQL's does. No warning.
    Exact(&'static str),
    /// The engine's collation is the nearest thing and is *narrower*. The
    /// clause is applied and a `1618` names what is missing, because equality
    /// the caller expects and does not get is the failure this whole item
    /// exists to close.
    Narrower(&'static str, String),
    /// Nothing here means this. The clause is dropped, `BINARY` stands, and
    /// the warning names it.
    Unknown,
}

/// Map a MySQL collation name onto one of SQLite's three.
///
/// The rules, in the order they are asked:
///
/// * `*_bin`, and `binary` itself, are byte-wise — which is exactly
///   `BINARY`. An exact mapping, no warning.
/// * `*_ci` is case-insensitive, and `NOCASE` is case-insensitive **for ASCII
///   only**. Applied, with a warning, because MySQL's `_ci` collations fold
///   more than that: all of them fold non-ASCII case (`'É' = 'é'`), and every
///   one whose name is not `_as_` folds accents too (`'é' = 'e'`). A caller
///   who is not told will read one guarantee and get a smaller one.
/// * `*_cs` is case-sensitive, which for equality is `BINARY`. The *ordering*
///   still differs — MySQL sorts by Unicode collation weights and this sorts
///   by code point — so it warns.
/// * Anything else is not recognised. Dropped, and named.
fn map_collation(name: &str) -> Mapped {
    let bare = name.trim_matches(|c| c == '\'' || c == '"' || c == '`');
    let lower = bare.to_ascii_lowercase();

    if lower == "binary" || lower.ends_with("_bin") {
        return Mapped::Exact("BINARY");
    }
    if lower.ends_with("_ci") {
        let accents = if lower.contains("_as_") {
            // `utf8mb4_0900_as_ci` is accent-*sensitive*, so accents are the
            // one thing that already agrees.
            String::new()
        } else {
            format!(
                " and it is accent-insensitive, where `NOCASE` is not — `'é' = 'e'` is true                  under `{bare}` and false here"
            )
        };
        return Mapped::Narrower(
            "NOCASE",
            format!(
                "`NOCASE` folds ASCII `A`-`Z` and nothing else, exactly as SQLite does.                  `{bare}` also folds non-ASCII case — `'É' = 'é'` is true there and false                  here{accents}. See Divergences in `docs/server.md`"
            ),
        );
    }
    if lower.ends_with("_cs") {
        return Mapped::Narrower(
            "BINARY",
            format!(
                "`{bare}` compares case-sensitively, which for equality is what `BINARY` does.                  The *ordering* still differs: MySQL sorts by Unicode collation weights and                  this sorts by UTF-8 code point"
            ),
        );
    }
    Mapped::Unknown
}

/// The result of translating one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    /// The statement(s) to hand to the engine, in order.
    ///
    /// Almost always exactly one. More than one only when a single MySQL
    /// `ALTER TABLE` bundled several operations together (see the module
    /// docs on why running them is **not atomic**), or when one operation
    /// became its own free-standing statement (`ADD INDEX` becoming a
    /// separate `CREATE INDEX`). Empty when every operation in the
    /// statement was a `1618` on its own — an `ADD CONSTRAINT ... FOREIGN
    /// KEY` with nothing else beside it — so there is nothing left to run at
    /// all, and the reply is a plain OK carrying the warning.
    pub statements: Vec<String>,
    /// Every clause that was removed, in the order it was found.
    pub dropped: Vec<Dropped>,
}

/// Translate one statement's MySQL-only DDL decoration.
///
/// `sql` must already have been through [`crate::sqltext::normalize`], so
/// comments are gone. A statement with nothing to translate comes back
/// **byte-for-byte unchanged**, as the single element of
/// [`Translation::statements`]: the re-rendering below only runs when
/// something was actually rewritten, so a tokenizer bug cannot quietly
/// reshape a statement this module had no business touching.
///
/// `catalog` is read only to synthesise the name of an unnamed index the
/// same way MySQL does — see [`synthesize_index_name`] — and to answer
/// whether a table exists when every operation in an `ALTER TABLE` turned
/// into a warning and nothing else. Nothing here writes to it.
pub fn translate(sql: &str, catalog: &Catalog) -> Result<Translation, MysqlError> {
    let tokens = tokenize(sql);
    let mut dropped = Vec::new();

    let statements = match kind(&tokens) {
        Kind::CreateTable => create_table(&tokens, &mut dropped)?.map(|s| vec![s]),
        Kind::CreateIndex => create_index(&tokens, &mut dropped)?.map(|s| vec![s]),
        Kind::AlterTable => alter_table(&tokens, &mut dropped, catalog)?,
        Kind::TruncateTable => truncate_table(&tokens, &mut dropped)?,
        Kind::RenameTable => rename_table(&tokens)?,
        Kind::Update => update_set(&tokens)?,
        Kind::Insert => insert_on_duplicate_key_update(&tokens)?,
        Kind::Other => None,
    };

    Ok(Translation {
        statements: statements.unwrap_or_else(|| vec![sql.to_string()]),
        dropped,
    })
}

// ------------------------------------------------------------------- tokens

/// One lexical token.
///
/// Quoted spans keep their quotes, because they are handed back to the engine's
/// parser verbatim: a backtick-quoted `` `order` `` must stay quoted or it
/// becomes a keyword, and a string literal's escapes are not this module's to
/// reinterpret.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A bare word — a keyword or an unquoted identifier.
    Word(String),
    /// A quoted identifier, with its quotes.
    Quoted(String),
    /// A string literal, with its quotes.
    Str(String),
    /// A number.
    Num(String),
    /// A single character of punctuation.
    Punct(char),
}

impl Token {
    /// Whether this is the bare word `keyword`, compared case-insensitively.
    fn is(&self, keyword: &str) -> bool {
        matches!(self, Token::Word(word) if word.eq_ignore_ascii_case(keyword))
    }

    /// The bare word this token is, if it is one.
    fn word(&self) -> Option<&str> {
        match self {
            Token::Word(word) => Some(word),
            _ => None,
        }
    }

    /// The character this token is, if it is punctuation.
    fn punct(&self) -> Option<char> {
        match self {
            Token::Punct(c) => Some(*c),
            _ => None,
        }
    }

    /// The token as written.
    fn text(&self) -> &str {
        match self {
            Token::Word(text) | Token::Quoted(text) | Token::Str(text) | Token::Num(text) => text,
            Token::Punct(_) => "",
        }
    }

    /// The identifier this token names, with any quoting removed.
    fn name(&self) -> String {
        match self {
            Token::Quoted(text) => crate::sqltext::unquote_identifier(text),
            other => other.text().to_string(),
        }
    }

    /// Whether this token could be the value of a `NAME = value` option.
    fn is_value(&self) -> bool {
        matches!(
            self,
            Token::Word(_) | Token::Quoted(_) | Token::Str(_) | Token::Num(_)
        )
    }
}

fn tokenize(sql: &str) -> Vec<Token> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // Quoted identifiers. A doubled quote inside one is an escaped quote,
        // not the end of the span.
        if c == '`' || c == '"' || c == '[' {
            let close = if c == '[' { ']' } else { c };
            let mut text = String::from(c);
            i += 1;
            while i < chars.len() {
                text.push(chars[i]);
                i += 1;
                if chars[i - 1] == close {
                    if i < chars.len() && chars[i] == close && close != ']' {
                        text.push(close);
                        i += 1;
                        continue;
                    }
                    break;
                }
            }
            tokens.push(Token::Quoted(text));
            continue;
        }

        if c == '\'' {
            let mut text = String::from(c);
            i += 1;
            while i < chars.len() {
                let inner = chars[i];
                text.push(inner);
                i += 1;
                if inner == '\\' && i < chars.len() {
                    text.push(chars[i]);
                    i += 1;
                    continue;
                }
                if inner == '\'' {
                    if i < chars.len() && chars[i] == '\'' {
                        text.push('\'');
                        i += 1;
                        continue;
                    }
                    break;
                }
            }
            tokens.push(Token::Str(text));
            continue;
        }

        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            tokens.push(Token::Num(chars[start..i].iter().collect()));
            continue;
        }

        if c.is_alphanumeric() || c == '_' || c == '$' || c == '@' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric()
                    || chars[i] == '_'
                    || chars[i] == '$'
                    || chars[i] == '@')
            {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i].iter().collect()));
            continue;
        }

        tokens.push(Token::Punct(c));
        i += 1;
    }
    tokens
}

/// Put a token sequence back together as SQL.
///
/// The spacing is only cosmetic — the engine's parser is whitespace-insensitive
/// — but it is what a person reads in an error message or a test failure, so it
/// is kept close to how the statement was written.
fn render(tokens: &[Token]) -> String {
    let mut out = String::new();
    let mut previous: Option<&Token> = None;
    for token in tokens {
        let tight_before = matches!(token.punct(), Some(',' | ')' | ';' | '.'));
        let tight_after = matches!(previous.and_then(Token::punct), Some('(' | '.'));
        if !out.is_empty() && !tight_before && !tight_after {
            out.push(' ');
        }
        match token {
            Token::Punct(c) => out.push(*c),
            other => out.push_str(other.text()),
        }
        previous = Some(token);
    }
    out
}

// ------------------------------------------------------------ dispatch

/// Which statement shape this is, as far as this module cares.
enum Kind {
    CreateTable,
    CreateIndex,
    AlterTable,
    /// The standalone MySQL statement, not SQLite's `DELETE`.
    TruncateTable,
    /// The standalone MySQL statement, not `ALTER TABLE ... RENAME TO`.
    RenameTable,
    /// `UPDATE ... SET`, checked for a qualified assignment target.
    Update,
    /// `INSERT ...`, checked for `ON DUPLICATE KEY UPDATE`.
    Insert,
    /// Nothing here to translate.
    Other,
}

fn kind(tokens: &[Token]) -> Kind {
    let Some(first) = tokens.first() else {
        return Kind::Other;
    };
    if first.is("ALTER") {
        return if tokens.get(1).is_some_and(|t| t.is("TABLE")) {
            Kind::AlterTable
        } else {
            Kind::Other
        };
    }
    if first.is("TRUNCATE") {
        return Kind::TruncateTable;
    }
    // `RENAME TABLE a TO b` — the standalone statement. `ALTER TABLE t RENAME
    // TO u` also starts with a word ending in "RENAME" territory, but it
    // starts with `ALTER`, not `RENAME`, so the two cannot collide here.
    if first.is("RENAME") && tokens.get(1).is_some_and(|t| t.is("TABLE")) {
        return Kind::RenameTable;
    }
    if first.is("UPDATE") {
        return Kind::Update;
    }
    if first.is("INSERT") {
        return Kind::Insert;
    }
    if !first.is("CREATE") {
        return Kind::Other;
    }
    // `CREATE [OR REPLACE] [TEMPORARY] TABLE` and
    // `CREATE [UNIQUE|FULLTEXT|SPATIAL] INDEX` both hide the noun a few words in.
    for token in tokens.iter().skip(1).take(4) {
        if token.is("TABLE") {
            return Kind::CreateTable;
        }
        if token.is("INDEX") {
            return Kind::CreateIndex;
        }
    }
    Kind::Other
}

// -------------------------------------------------------- CREATE TABLE

fn create_table(
    tokens: &[Token],
    dropped: &mut Vec<Dropped>,
) -> Result<Option<String>, MysqlError> {
    // The column list is the first top-level parenthesis group. Without one
    // this is not a shape this module understands, and the engine's parser is
    // the right thing to report that.
    let Some(open) = tokens.iter().position(|t| t.punct() == Some('(')) else {
        return Ok(None);
    };
    let Some(close) = matching_paren(tokens, open) else {
        return Ok(None);
    };

    let items = split_top_level(&tokens[open + 1..close]);

    // Pass one: find the primary key, and refuse the inline index declarations
    // that would otherwise be dropped or mis-parsed. `KEY x (a)` inside a
    // `CREATE TABLE` asks for an index; the engine would create none, and the
    // table would come back missing it.
    let mut key_columns: Vec<String> = Vec::new();
    for item in &items {
        let Some(first) = item.first() else { continue };
        let second_is_index = item
            .get(1)
            .is_some_and(|t| t.is("KEY") || t.is("INDEX") || t.is("PRIMARY"));

        if first.is("PRIMARY") && item.get(1).is_some_and(|t| t.is("KEY")) {
            key_columns.extend(paren_names(item));
            continue;
        }
        if first.is("KEY")
            || first.is("INDEX")
            || ((first.is("FULLTEXT") || first.is("SPATIAL") || first.is("UNIQUE"))
                && second_is_index)
        {
            return Err(MysqlError::unsupported(format!(
                "`{}` is not supported inside CREATE TABLE: InlaySQL would create no index for \
                 it, and a table that silently came back without the index it declared is worse \
                 than a refusal. Create the table first, then `CREATE INDEX` — note that the \
                 engine's only scalar access path today is the INTEGER PRIMARY KEY row id",
                render(item)
            )));
        }
    }

    // The table options are read *before* the columns, because one of them —
    // `COLLATE` — is the default the columns inherit. Their warnings are held
    // back and appended afterwards, so `SHOW WARNINGS` still lists every
    // dropped clause in the order it was written.
    let tail = &tokens[close + 1..];
    let mut tail_dropped = Vec::new();
    let (tail_changed, table_collation) = table_options(tail, &mut tail_dropped)?;

    // Pass two: rewrite the column definitions.
    let mut changed = tail_changed;
    let mut rebuilt: Vec<Vec<Token>> = Vec::with_capacity(items.len());
    for item in &items {
        let is_constraint = item.first().is_some_and(|first| {
            ["PRIMARY", "UNIQUE", "FOREIGN", "CHECK", "CONSTRAINT"]
                .iter()
                .any(|keyword| first.is(keyword))
        });
        if is_constraint {
            rebuilt.push(item.clone());
            continue;
        }
        let (tokens, item_changed) =
            column_definition(item, &key_columns, table_collation.as_deref(), dropped)?;
        changed |= item_changed;
        rebuilt.push(tokens);
    }
    dropped.append(&mut tail_dropped);

    if !changed {
        return Ok(None);
    }

    let mut out: Vec<Token> = tokens[..=open].to_vec();
    for (index, item) in rebuilt.iter().enumerate() {
        if index > 0 {
            out.push(Token::Punct(','));
        }
        out.extend(item.iter().cloned());
    }
    out.push(Token::Punct(')'));
    if !tail_changed {
        out.extend(tail.iter().cloned());
    }
    Ok(Some(render(&out)))
}

/// The MySQL integer family.
///
/// Not the engine's list — the engine maps type names onto its own five storage
/// classes and refuses the ones it has no room for (`MEDIUMINT` today). This is
/// only the question "did the user write an integer type?", which is what
/// decides whether `AUTO_INCREMENT` could possibly be the row id. A type MySQL
/// calls an integer and InlaySQL does not support is the engine's error to
/// report, in its own words, and it says something more useful than this
/// module could.
const INTEGER_TYPES: &[&str] = &[
    "TINYINT",
    "SMALLINT",
    "MEDIUMINT",
    "INT",
    "INTEGER",
    "BIGINT",
    "INT1",
    "INT2",
    "INT3",
    "INT4",
    "INT8",
    "SERIAL",
];

/// The functions MySQL accepts after `ON UPDATE` to mean "the current time".
const NOW_FUNCTIONS: &[&str] = &[
    "CURRENT_TIMESTAMP",
    "NOW",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "SYSDATE",
];

/// The MySQL types a character set and a collation apply to.
///
/// MySQL attaches both to string types and to nothing else, so a table-level
/// `COLLATE` propagates only here. Writing `COLLATE NOCASE` onto an `INT`
/// would be harmless — the engine records it and never asks — but it would
/// push the catalog to the version that can hold collations for no reason, and
/// it would say something about the column that MySQL does not.
const TEXT_TYPES: &[&str] = &[
    "CHAR",
    "VARCHAR",
    "CHARACTER",
    "NCHAR",
    "NVARCHAR",
    "TINYTEXT",
    "TEXT",
    "MEDIUMTEXT",
    "LONGTEXT",
    "ENUM",
    "SET",
    "CLOB",
];

/// Rewrite one column definition. Returns the tokens and whether they changed.
///
/// `table_collation` is the collation the table's own `COLLATE` clause asked
/// for, already mapped. MySQL applies it to every string column that does not
/// write one of its own, and so does this: without that step the mapping would
/// fix nothing for the statement that actually matters, since Laravel and
/// every other ORM put the collation on the table and not on the columns.
fn column_definition(
    item: &[Token],
    key_columns: &[String],
    table_collation: Option<&str>,
    dropped: &mut Vec<Dropped>,
) -> Result<(Vec<Token>, bool), MysqlError> {
    let Some(name_token) = item.first() else {
        return Ok((item.to_vec(), false));
    };
    let name = name_token.name();
    let declared_type = item
        .get(1)
        .and_then(Token::word)
        .unwrap_or("")
        .to_ascii_uppercase();

    let mut out: Vec<Token> = Vec::with_capacity(item.len());
    let mut changed = false;
    let mut collation_written = false;
    let mut auto_increment: Option<String> = None;
    let mut column_is_primary_key = false;
    let mut depth = 0i32;
    let mut i = 0;

    while i < item.len() {
        let token = &item[i];
        match token.punct() {
            Some('(') => depth += 1,
            Some(')') => depth -= 1,
            _ => {}
        }

        // Only the column's own top-level clauses are inspected. Anything
        // inside parentheses — a type's length, a `DEFAULT (expr)` — is copied
        // through untouched.
        if depth == 0 && i > 0 {
            // --------------------------------------------- neutralised
            if token.is("UNSIGNED") {
                dropped.push(Dropped::new(
                    "UNSIGNED",
                    unsigned_reason(&name, &declared_type),
                ));
                changed = true;
                i += 1;
                continue;
            }
            if token.is("AUTO_INCREMENT") {
                auto_increment = Some(render(&item[i..=i]));
                changed = true;
                i += 1;
                continue;
            }
            // `COLLATE x` — the one clause here that is translated rather
            // than dropped (AHL-469). What the engine writes in its place is
            // whichever of `BINARY`, `NOCASE` it can honour.
            if let Some((consumed, named)) = collation_clause(item, i) {
                let written = render(&item[i..consumed]);
                match map_collation(named) {
                    Mapped::Exact(onto) => out.extend(collate_tokens(onto)),
                    Mapped::Narrower(onto, reason) => {
                        dropped.push(Dropped::mapped(written, onto, reason));
                        out.extend(collate_tokens(onto));
                    }
                    Mapped::Unknown => dropped.push(Dropped::new(
                        written,
                        format!(
                            "`{named}` is not a collation this engine has. It compares \
                             `{name}` byte for byte, the way SQLite's BINARY collation does; \
                             `BINARY`, `NOCASE` and `RTRIM` are the three it can honour"
                        ),
                    )),
                }
                collation_written = true;
                changed = true;
                i = consumed;
                continue;
            }
            // `CHARACTER SET x` / `CHARSET x` — a storage encoding, and this
            // engine has exactly one.
            if let Some(consumed) = charset_or_collation(item, i) {
                dropped.push(Dropped::new(
                    render(&item[i..consumed]),
                    format!(
                        "InlaySQL stores every string as UTF-8, so `{name}` has one encoding \
                         and there is nothing to select. What a character set *also* implies \
                         in MySQL — a default collation — is honoured separately; see \
                         Divergences in `docs/server.md`"
                    ),
                ));
                changed = true;
                i = consumed;
                continue;
            }

            // ------------------------------------------------- refused
            if token.is("ZEROFILL") {
                return Err(MysqlError::unsupported(format!(
                    "`ZEROFILL` on `{name}` is not supported: it promises that every value comes \
                     back padded to the column's display width, and this server returns the \
                     number as it was stored"
                )));
            }
            if token.is("ON")
                && item.get(i + 1).is_some_and(|t| t.is("UPDATE"))
                && item
                    .get(i + 2)
                    .is_some_and(|t| NOW_FUNCTIONS.iter().any(|f| t.is(f)))
            {
                return Err(MysqlError::unsupported(format!(
                    "`ON UPDATE {}` on `{name}` is not supported: InlaySQL never writes a column \
                     the statement did not name, so the value would silently stop tracking the \
                     row's last update",
                    item[i + 2].text()
                )));
            }
            if token.is("PRIMARY") && item.get(i + 1).is_some_and(|t| t.is("KEY")) {
                column_is_primary_key = true;
            }
        }

        out.push(token.clone());
        i += 1;
    }

    if let Some(clause) = auto_increment {
        let is_primary_key =
            column_is_primary_key || key_columns.iter().any(|c| c.eq_ignore_ascii_case(&name));
        let is_integer = INTEGER_TYPES
            .iter()
            .any(|candidate| declared_type == *candidate);

        if !is_integer {
            return Err(MysqlError::unsupported(format!(
                "`AUTO_INCREMENT` on `{name}` is not supported: InlaySQL auto-assigns a key only \
                 for an INTEGER PRIMARY KEY, and `{name}` is declared `{declared_type}`"
            )));
        }
        if !is_primary_key {
            return Err(MysqlError::unsupported(format!(
                "`AUTO_INCREMENT` on `{name}` is not supported: InlaySQL auto-assigns a key only \
                 for the column that is the INTEGER PRIMARY KEY, and `{name}` is not declared \
                 PRIMARY KEY. There is no separate counter for an ordinary column"
            )));
        }
        dropped.push(Dropped::new(
            clause,
            format!(
                "an INTEGER PRIMARY KEY is InlaySQL's row id and is already assigned from a \
                 monotonic counter when `{name}` is omitted or NULL, so the clause describes \
                 what this column does anyway"
            ),
        ));
    }

    // The table's `COLLATE` reaches every string column that did not write one
    // of its own — MySQL's rule, and the one that makes the mapping matter,
    // since an ORM puts the collation on the table.
    if !collation_written {
        if let Some(collation) = table_collation {
            if TEXT_TYPES.iter().any(|ty| declared_type == *ty) {
                out.extend(collate_tokens(collation));
                changed = true;
            }
        }
    }

    Ok((out, changed))
}

/// The tokens spelling `COLLATE <name>`.
fn collate_tokens(collation: &str) -> [Token; 2] {
    [
        Token::Word("COLLATE".to_string()),
        Token::Word(collation.to_string()),
    ]
}

/// Why dropping `UNSIGNED` is stated the way it is, per width.
///
/// The divergence is not the same at every width and saying so matters. Every
/// value of `TINYINT/SMALLINT/MEDIUMINT/INT UNSIGNED` fits an `i64`, so nothing
/// is lost but MySQL's refusal of a negative. `BIGINT UNSIGNED` is the one that
/// genuinely cannot round-trip its top half.
fn unsigned_reason(name: &str, declared_type: &str) -> String {
    let upper = declared_type.to_ascii_uppercase();
    if upper.starts_with("BIGINT") || upper == "INT8" || upper == "SERIAL" {
        format!(
            "InlaySQL integers are signed 64-bit. `{name}` therefore stores no value above \
             9223372036854775807 — the top half of MySQL's BIGINT UNSIGNED range does not \
             round-trip — and a negative value MySQL would reject is stored rather than \
             refused. Documented under Divergences in `docs/server.md`"
        )
    } else {
        format!(
            "InlaySQL integers are signed 64-bit, which covers every value of MySQL's \
             `{upper} UNSIGNED` range, so nothing is lost to the width. What is lost is the \
             refusal: a negative value MySQL would reject is stored in `{name}` rather than \
             refused. Documented under Divergences in `docs/server.md`"
        )
    }
}

/// Consume `COLLATE x` starting at `i`, returning the index just past it and
/// the collation it named.
///
/// Separate from [`charset_or_collation`] because the two are no longer treated
/// alike: a character set is dropped, and a collation is translated.
fn collation_clause(item: &[Token], i: usize) -> Option<(usize, &str)> {
    if !item[i].is("COLLATE") {
        return None;
    }
    let mut at = i + 1;
    if item.get(at).and_then(Token::punct) == Some('=') {
        at += 1;
    }
    let named = item.get(at).filter(|t| t.is_value())?;
    Some((at + 1, named.text()))
}

/// Consume `CHARACTER SET x` / `CHARSET x` / `COLLATE x` starting at `i`.
///
/// Returns the index just past the clause, or `None` if there is no such clause
/// here.
fn charset_or_collation(item: &[Token], i: usize) -> Option<usize> {
    let mut at = i;
    if item[at].is("CHARACTER") && item.get(at + 1).is_some_and(|t| t.is("SET")) {
        at += 2;
    } else if item[at].is("CHARSET") || item[at].is("COLLATE") {
        at += 1;
    } else {
        return None;
    }
    if item.get(at).and_then(Token::punct) == Some('=') {
        at += 1;
    }
    if item.get(at).is_some_and(Token::is_value) {
        at += 1;
    }
    Some(at)
}

/// The words that end a `CREATE TABLE` in the *engine's* dialect.
///
/// A tail beginning with one of these is not MySQL's and is not this module's:
/// it goes to the engine untouched, which has a better answer for it than a
/// MySQL translator could invent.
const ENGINE_DIALECT_TAILS: &[&str] = &["STRICT", "WITHOUT", "AS"];

/// Consume the table options that trail a `CREATE TABLE`.
///
/// Every option here is a storage or presentation hint with no bearing on what
/// the table holds, so the whole tail is removed. An option that is *not* on
/// this list is refused rather than guessed at: the list is short on purpose,
/// and a `PARTITION BY` quietly discarded would build a table nobody asked for.
fn table_options(
    tail: &[Token],
    dropped: &mut Vec<Dropped>,
) -> Result<(bool, Option<String>), MysqlError> {
    if tail
        .first()
        .is_some_and(|token| ENGINE_DIALECT_TAILS.iter().any(|word| token.is(word)))
    {
        return Ok((false, None));
    }

    let mut i = 0;
    let mut found = false;
    let mut collation: Option<String> = None;

    while i < tail.len() {
        if tail[i].punct() == Some(',') {
            i += 1;
            continue;
        }
        let start = i;
        // `DEFAULT` only ever qualifies a charset or collation here.
        if tail[i].is("DEFAULT") {
            i += 1;
        }
        let Some(token) = tail.get(i) else {
            return Err(unsupported_table_option(&tail[start..]));
        };

        if token.is("CHARACTER") && tail.get(i + 1).is_some_and(|t| t.is("SET")) {
            i += 2;
        } else if [
            "CHARSET",
            "COLLATE",
            "ENGINE",
            "ROW_FORMAT",
            "AUTO_INCREMENT",
        ]
        .iter()
        .any(|keyword| token.is(keyword))
        {
            i += 1;
        } else {
            return Err(unsupported_table_option(&tail[start..]));
        }

        if tail.get(i).and_then(Token::punct) == Some('=') {
            i += 1;
        }
        if !tail.get(i).is_some_and(Token::is_value) {
            return Err(unsupported_table_option(&tail[start..]));
        }
        let value = tail[i].text().to_string();
        i += 1;

        let clause = &tail[start..i];
        // A table-level `COLLATE` is the one option here that is not simply
        // removed: it becomes the default every string column inherits, so it
        // is carried out of this function rather than only reported.
        if clause.iter().any(|t| t.is("COLLATE")) {
            match map_collation(&value) {
                Mapped::Exact(onto) => collation = Some(onto.to_string()),
                Mapped::Narrower(onto, reason) => {
                    dropped.push(Dropped::mapped(render(clause), onto, reason));
                    collation = Some(onto.to_string());
                }
                Mapped::Unknown => dropped.push(Dropped::new(
                    render(clause),
                    format!(
                        "`{value}` is not a collation this engine has, so every string column \
                         of this table compares byte for byte, the way SQLite's BINARY \
                         collation does. `BINARY`, `NOCASE` and `RTRIM` are the three it can \
                         honour"
                    ),
                )),
            }
        } else {
            dropped.push(Dropped::new(render(clause), table_option_reason(clause)));
        }
        found = true;
    }
    Ok((found, collation))
}

fn table_option_reason(clause: &[Token]) -> String {
    if clause.iter().any(|t| t.is("ENGINE")) {
        return "InlaySQL has one storage engine — a copy-on-write B+ tree with MVCC — and no \
                pluggable engine to select"
            .to_string();
    }
    if clause.iter().any(|t| t.is("ROW_FORMAT")) {
        return "InlaySQL has one on-disk row format".to_string();
    }
    if clause.iter().any(|t| t.is("AUTO_INCREMENT")) {
        return "InlaySQL's row id counter always starts at 1 and cannot be seeded. The keys are \
                still unique and increasing, they are simply not the numbers this asked for — \
                see Divergences in `docs/server.md`"
            .to_string();
    }
    // What is left is a character set: one encoding here, and nothing to pick.
    "InlaySQL stores every string as UTF-8, so there is one encoding and nothing to select. The \
     collation a character set implies in MySQL is honoured separately — see Divergences in \
     `docs/server.md`"
        .to_string()
}

fn unsupported_table_option(rest: &[Token]) -> MysqlError {
    MysqlError::unsupported(format!(
        "table option `{}` is not supported by this server",
        render(rest)
    ))
}

// -------------------------------------------------------- CREATE INDEX

fn create_index(
    tokens: &[Token],
    dropped: &mut Vec<Dropped>,
) -> Result<Option<String>, MysqlError> {
    let mut depth = 0i32;
    for (i, token) in tokens.iter().enumerate() {
        match token.punct() {
            Some('(') => depth += 1,
            Some(')') => depth -= 1,
            _ => {}
        }
        if depth == 0 && token.is("USING") {
            let named = tokens.get(i + 1).map(Token::text).unwrap_or("");
            return Err(MysqlError::unsupported(format!(
                "`USING {named}` is not supported: InlaySQL picks the index kind from the \
                 column's type — BM25 full-text for TEXT, HNSW for VECTOR — and has no B-tree or \
                 hash index on a scalar column. Dropping the clause would build a different \
                 index from the one asked for"
            )));
        }
    }
    Ok(strip_online_ddl_specs(tokens, dropped).map(|t| render(&t)))
}

// --------------------------------------------------------- ALTER TABLE

/// One operation inside an `ALTER TABLE`'s comma-separated list, classified
/// for the shapes this module rewrites (see the module docs). Everything
/// else is [`AlterOp::Other`] and reaches the engine as its own `ALTER
/// TABLE` — core already knows `ADD COLUMN`, `RENAME COLUMN`, `DROP COLUMN`
/// and `RENAME TO`, and refuses anything else in its own words.
enum AlterOp {
    /// `ADD [CONSTRAINT [symbol]] {INDEX|KEY|UNIQUE [INDEX|KEY]} [name] (cols)`.
    AddIndex {
        name: Option<String>,
        unique: bool,
        columns: Vec<String>,
    },
    /// `ADD [CONSTRAINT [symbol]] FOREIGN KEY ...`, in any of its spellings.
    AddForeignKey,
    /// `DROP {INDEX|KEY} name`.
    DropIndex { name: String },
    /// `RENAME {INDEX|KEY} a TO b`.
    RenameIndex { from: String, to: String },
    /// Not one of the shapes above.
    Other,
}

/// Classify one comma-separated operation out of an `ALTER TABLE`.
fn parse_alter_operation(op: &[Token]) -> AlterOp {
    let Some(first) = op.first() else {
        return AlterOp::Other;
    };

    if first.is("ADD") {
        let paren = op.iter().position(|t| t.punct() == Some('('));
        let head = &op[1..paren.unwrap_or(op.len())];

        // Checked before the `CONSTRAINT`-consuming logic below: a foreign
        // key never becomes an index, and its symbol (if it has one) plays
        // no part in the warning this becomes.
        if head.iter().any(|t| t.is("FOREIGN")) {
            return AlterOp::AddForeignKey;
        }

        let mut i = 1;
        // `ADD CONSTRAINT [symbol] UNIQUE ...` — the symbol, when there is
        // one, names the index unless an explicit index name follows too;
        // MySQL lets a statement give both, and the index name wins.
        let mut symbol = None;
        if head.first().is_some_and(|t| t.is("CONSTRAINT")) {
            i += 1;
            if op.get(i).is_some_and(|t| !t.is("UNIQUE")) {
                symbol = Some(op[i].name());
                i += 1;
            }
        }

        let unique = op.get(i).is_some_and(|t| t.is("UNIQUE"));
        if unique {
            i += 1;
        }
        let has_index_keyword = op.get(i).is_some_and(|t| t.is("INDEX") || t.is("KEY"));
        if !unique && !has_index_keyword {
            // `ADD` something that is not an index, a unique constraint or a
            // foreign key — `ADD COLUMN`, most often. Core's business.
            return AlterOp::Other;
        }
        if has_index_keyword {
            i += 1;
        }

        let mut name = symbol;
        if op.get(i).is_some_and(|t| t.punct() != Some('(')) {
            name = Some(op[i].name());
            i += 1;
        }
        let columns = paren_names(&op[i..]);
        return AlterOp::AddIndex {
            name,
            unique,
            columns,
        };
    }

    if first.is("DROP") {
        if op.get(1).is_some_and(|t| t.is("INDEX") || t.is("KEY")) {
            if let Some(name) = op.get(2) {
                return AlterOp::DropIndex { name: name.name() };
            }
        }
        return AlterOp::Other;
    }

    if first.is("RENAME") && op.get(1).is_some_and(|t| t.is("INDEX") || t.is("KEY")) {
        if let (Some(from), Some(to_kw), Some(to)) = (op.get(2), op.get(3), op.get(4)) {
            if to_kw.is("TO") {
                return AlterOp::RenameIndex {
                    from: from.name(),
                    to: to.name(),
                };
            }
        }
    }

    AlterOp::Other
}

/// The index just past a table name starting at `start` — a bare name or a
/// `schema.table` qualified one, which is as far as this needs to go: one
/// database file is one schema (`docs/server.md`), so nothing here nests
/// further.
fn table_name_end(tokens: &[Token], start: usize) -> usize {
    let mut i = start;
    if !tokens
        .get(i)
        .is_some_and(|t| matches!(t, Token::Word(_) | Token::Quoted(_)))
    {
        return i;
    }
    i += 1;
    while tokens.get(i).and_then(Token::punct) == Some('.')
        && tokens
            .get(i + 1)
            .is_some_and(|t| matches!(t, Token::Word(_) | Token::Quoted(_)))
    {
        i += 2;
    }
    i
}

/// MySQL's own rule for an index that names none of its own (MySQL Reference
/// Manual, "CREATE TABLE Statement" → "Secondary Indexes": *"If you do not
/// assign a name to an index, MySQL assigns the name of the first indexed
/// column, with an optional suffix (`_2`, `_3`, ...) to make it unique."*).
///
/// `existing` is every name already on the table this index is joining —
/// real ones already in the catalog, and every one already handed out
/// earlier in the same `ALTER TABLE`, so two unnamed indexes added by one
/// statement cannot collide with each other either.
fn synthesize_index_name(columns: &[String], existing: &[String]) -> String {
    let base = columns.first().cloned().unwrap_or_default();
    let mut candidate = base.clone();
    let mut suffix = 2;
    while existing
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&candidate))
    {
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
    candidate
}

/// Rewrite the MySQL shapes `ALTER TABLE` can carry that core has no
/// operation for — see the module docs for the full list and why the result
/// is not atomic when more than one statement comes out of it.
fn alter_table(
    tokens: &[Token],
    dropped: &mut Vec<Dropped>,
    catalog: &Catalog,
) -> Result<Option<Vec<String>>, MysqlError> {
    // MySQL's online-DDL steering can be comma-joined with any operation and
    // is not one of the shapes below; it has to come off first; or it would
    // be mistaken for an unrecognised, pass-through operation.
    let cleaned = strip_online_ddl_specs(tokens, dropped);
    let tokens: &[Token] = cleaned.as_deref().unwrap_or(tokens);

    let name_end = table_name_end(tokens, 2);
    if name_end <= 2 || tokens.len() <= name_end {
        // Not a shape this function understands — no table name found, or
        // nothing follows it. Leave it to the engine's own parser to say why.
        return Ok(cleaned.map(|t| vec![render(&t)]));
    }
    let table_prefix = &tokens[..name_end];
    let table_name = render(&tokens[2..name_end]);
    let bare_table_name = tokens[2..name_end]
        .last()
        .map(Token::name)
        .unwrap_or_default();

    let operations = split_top_level(&tokens[name_end..]);

    // Names already on the table, so an index this statement leaves unnamed
    // does not collide with one an earlier statement created — MySQL
    // disambiguates against the table's real index list the same way.
    let mut existing_names: Vec<String> = catalog
        .indexes_for(&bare_table_name)
        .into_iter()
        .map(|index| index.name.clone())
        .collect();

    let mut statements = Vec::with_capacity(operations.len());
    let mut rewrote_an_operation = false;

    for op in &operations {
        match parse_alter_operation(op) {
            AlterOp::AddIndex {
                name,
                unique,
                columns,
            } => {
                rewrote_an_operation = true;
                if columns.is_empty() {
                    return Err(MysqlError::unsupported(format!(
                        "`{}` is not supported: no column list was found for the index",
                        render(op)
                    )));
                }
                let name = name.unwrap_or_else(|| synthesize_index_name(&columns, &existing_names));
                existing_names.push(name.clone());
                let keyword = if unique { "UNIQUE INDEX" } else { "INDEX" };
                let column_list = columns
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                statements.push(format!(
                    "CREATE {keyword} `{name}` ON {table_name} ({column_list})"
                ));
            }
            AlterOp::AddForeignKey => {
                rewrote_an_operation = true;
                dropped.push(Dropped::new(
                    render(op),
                    "InlaySQL has no ALTER TABLE path to record a foreign key added after the \
                     table already exists — only CREATE TABLE can declare one, and even there \
                     it is recorded but never enforced, SQLite's own long-standing default (see \
                     docs/server.md). This one is not recorded anywhere at all: nothing in the \
                     catalog says it exists, and nothing will ever check it",
                ));
            }
            AlterOp::DropIndex { name } => {
                rewrote_an_operation = true;
                statements.push(format!("DROP INDEX `{name}`"));
            }
            AlterOp::RenameIndex { from, to } => {
                return Err(MysqlError::unsupported(format!(
                    "`RENAME INDEX {from} TO {to}` is not supported: InlaySQL has no index \
                     rename, only DROP INDEX and CREATE INDEX — a different statement, with a \
                     window where the index does not exist at all"
                )));
            }
            AlterOp::Other => {
                let mut statement: Vec<Token> = table_prefix.to_vec();
                statement.extend(op.iter().cloned());
                statements.push(render(&statement));
            }
        }
    }

    // Nothing here was this module's business at all: no online-DDL steering
    // came off, this was one operation, and it was not one of the shapes
    // above. Byte-for-byte, so a bug here cannot reshape a statement it had
    // no reason to touch.
    if cleaned.is_none() && operations.len() == 1 && !rewrote_an_operation {
        return Ok(None);
    }

    if statements.is_empty() {
        // Every operation in the statement turned into a warning and nothing
        // else — an `ADD CONSTRAINT ... FOREIGN KEY` on its own, most likely.
        // Answering OK is right (the warning already says what was not
        // recorded), but only for a table that really exists: silently
        // saying OK about one that does not would be the wrong kind of quiet.
        if catalog.table(&bare_table_name).is_none() {
            return Err(MysqlError::no_such_table(&bare_table_name));
        }
    }

    Ok(Some(statements))
}

// ------------------------------------------------------- TRUNCATE TABLE

/// `TRUNCATE [TABLE] t` → `DELETE FROM t`, the nearest thing SQLite's dialect
/// has — there is no `TRUNCATE` statement in it at all.
///
/// Always warned, never silently: InlaySQL's row id counter only ever moves
/// forward (Divergences, "The row id counter always starts at 1",
/// `docs/server.md`) and there is no way to seed or rewind it, so unlike
/// MySQL's `TRUNCATE` — which restarts `AUTO_INCREMENT` at its starting
/// value — the next row inserted after this keeps numbering from wherever
/// the table's counter already was. That is exactly what a plain `DELETE`
/// does in MySQL too; it is `TRUNCATE`'s one guarantee that does not survive
/// the translation.
fn truncate_table(
    tokens: &[Token],
    dropped: &mut Vec<Dropped>,
) -> Result<Option<Vec<String>>, MysqlError> {
    let mut i = 1;
    if tokens.get(i).is_some_and(|t| t.is("TABLE")) {
        i += 1;
    }
    let end = table_name_end(tokens, i);
    if end <= i {
        // Not a shape this function understands; leave it to the engine's
        // own parser, which has no TRUNCATE either and will say so honestly.
        return Ok(None);
    }
    let name = render(&tokens[i..end]);
    let clause = render(&tokens[..end]);
    dropped.push(Dropped::new(
        clause,
        format!(
            "InlaySQL has no TRUNCATE statement, so this became `DELETE FROM {name}`, which \
             removes every row the same way TRUNCATE does. What it does not do is reset the row \
             id counter: InlaySQL's counter only ever moves forward and cannot be seeded or \
             rewound, so the next row inserted into {name} keeps numbering from wherever the \
             counter already was rather than restarting at 1"
        ),
    ));
    Ok(Some(vec![format!("DELETE FROM {name}")]))
}

// ---------------------------------------------------------- RENAME TABLE

/// The standalone `RENAME TABLE a TO b[, c TO d, ...]` → one `ALTER TABLE ...
/// RENAME TO ...` per pair — exactly what it means, so nothing here warns.
///
/// MySQL's own multi-pair form renames every pair as one atomic operation
/// (it is even how a table swap is done without a window where a name is
/// missing). Split into separate statements, this is not that — see the
/// module docs on why a multi-statement translation is never atomic here.
fn rename_table(tokens: &[Token]) -> Result<Option<Vec<String>>, MysqlError> {
    let pairs = split_top_level(&tokens[2..]);
    if pairs.is_empty() {
        return Err(MysqlError::parse(
            "RENAME TABLE needs at least one `name TO new_name` pair",
        ));
    }
    let mut statements = Vec::with_capacity(pairs.len());
    for pair in &pairs {
        let Some(to_at) = pair.iter().position(|t| t.is("TO")) else {
            return Err(MysqlError::unsupported(format!(
                "`RENAME TABLE {}` is not supported: expected `name TO new_name`",
                render(pair)
            )));
        };
        let from = render(&pair[..to_at]);
        let to = render(&pair[to_at + 1..]);
        statements.push(format!("ALTER TABLE {from} RENAME TO {to}"));
    }
    Ok(Some(statements))
}

// -------------------------------------------------------- UPDATE ... SET

/// `UPDATE t SET t.col = ?` → `UPDATE t SET col = ?` when the qualifier names
/// the statement's own target table, or its alias once one is given — real
/// MySQL syntax, written by Eloquent on every save of a model with
/// timestamps (AHL-475).
///
/// This is *not* a SQLite feature core is missing: checked directly against a
/// real `sqlite3` binary, a qualified column on the left of `SET` is a syntax
/// error there in every case, right table, wrong table, aliased or not, so
/// core keeps refusing it (`inlaysql_core::sql::assignment_target_column`).
/// The qualifier only ever means one thing in a single-table `UPDATE`, so it
/// is checked and stripped here, before the statement ever reaches core's
/// parser, rather than taught to a dialect that does not have this construct
/// at all.
///
/// Once the table is aliased, the alias is the *only* valid qualifier — the
/// real table name is not, checked directly against `sqlite3`: `UPDATE users
/// AS u ... WHERE users.id = ...` is `no such column: users.id` there, not a
/// second name for the same source. The assignment target follows the same
/// rule.
///
/// Only the plain single-table shape is recognised: `UPDATE name [[AS] alias]
/// SET ...`. Anything else — a `JOIN`, `LOW_PRIORITY`/`IGNORE`, a shape this
/// function cannot make sense of — comes back `Ok(None)` and is passed
/// through unchanged for core to accept or refuse on its own terms, the same
/// as every other statement this module does not recognise.
///
/// ## MySQL's own upsert syntax: `ON DUPLICATE KEY UPDATE` (AHL-476)
///
/// Eloquent's `upsert()` compiles to `INSERT ... ON DUPLICATE KEY UPDATE col =
/// VALUES(col), ...` on a MySQL connection, and core refuses the clause by
/// name today (`inlaysql_core::sql::resolve_on_conflict`) rather than doing
/// something else with it. Core already has SQLite's own upsert —
/// `ON CONFLICT [(target)] DO UPDATE SET ... ` with `excluded.col` reading the
/// row that failed to insert — so [`insert_on_duplicate_key_update`] rewrites
/// one onto the other before the statement reaches core's parser, the same
/// move [`update_set`] makes for a qualified assignment target.
///
/// **The conflict target is dropped, not resolved.** MySQL's clause has no
/// target of its own: it fires on a collision with *any* unique or primary
/// key. The obvious worry is that SQLite's `DO UPDATE` needs a target to say
/// which constraint it answers for, and a table with more than one unique
/// constraint would need the target picked from the catalog, or the statement
/// refused as ambiguous. Checked directly against a real `sqlite3` binary
/// first, that worry does not hold: `ON CONFLICT DO UPDATE SET ...` with no
/// `(target)` at all is valid SQLite, not just for `DO NOTHING`, and it
/// resolves against *any* colliding constraint, primary key included — the
/// same check `resolve_conflict_target` in `inlaysql_core::sql` makes when
/// core plans it, and `Engine::insert` in `inlaysql_core::engine` honours it
/// the same way at execution time (`target: None` answers the first
/// constraint any row it looked at collided on, whichever one that is). That
/// is exactly MySQL's own "any unique or primary key" rule, so a bare
/// `ON CONFLICT DO UPDATE` is not a narrower stand-in for the MySQL clause —
/// it is the same clause, and no catalog lookup is needed to say so.
///
/// The one case this leaves open — a proposed row that collides with *two
/// different* stored rows, each through a different unique constraint — is
/// not this shim's to resolve either. MySQL's own manual describes exactly
/// one of the colliding rows being updated in that case, and does not commit
/// to which; this server picks whichever constraint its own conflict check
/// reaches first (in practice, the order the table's unique constraints were
/// declared), which is a real answer, just not one guaranteed to agree with
/// MySQL's own arbitrary pick. That is not a fix this shim can make more
/// correct than MySQL's own contract already is.
///
/// **`VALUES(col)` and the MySQL 8.0.20+ row-alias form both become
/// `excluded.col`.** Laravel's grammar backtick-quotes the function name too
/// (`` `values`(`col`) ``), so both spellings are recognised. Once a row
/// alias is given (`... AS new ON DUPLICATE KEY UPDATE col = new.col`), the
/// alias — not `VALUES(...)` — is how MySQL 8.0.20+ spells the same
/// reference, and the alias clause itself has no SQLite equivalent to leave
/// behind, so it is stripped from the statement rather than passed through.
/// **A column-alias list on the row alias
/// (`AS new (a, b) ON DUPLICATE KEY UPDATE x = new.a`) is refused (`1235`),
/// named:** resolving it needs the real column each alias renames, which
/// this shim only has when the `INSERT` names its own column list explicitly
/// — and even then it is a corner nobody writes (Eloquent never emits it),
/// so it is refused rather than guessed at.
///
/// **The affected-rows count is not MySQL's 0/1/2 convention, and this is a
/// documented divergence, not a bug.** MySQL reports 1 for an inserted row, 2
/// for an updated one, and 0 for an update that wrote back the values already
/// there. `Engine::insert` reports one count for the whole statement — the
/// number of rows it wrote, insert or update alike, the same count SQLite's
/// own `changes()` would give — with no per-row insert/update distinction to
/// draw the MySQL numbers from. Manufacturing one would mean adding a
/// MySQL-shaped reporting convention to the engine, which is not a SQLite
/// feature core is missing; see `docs/server.md` for how this is written down
/// for a caller who counts on it.
fn update_set(tokens: &[Token]) -> Result<Option<Vec<String>>, MysqlError> {
    let name_end = table_name_end(tokens, 1);
    if name_end <= 1 {
        return Ok(None);
    }
    let table_name = tokens[1..name_end]
        .last()
        .map(Token::name)
        .unwrap_or_default();

    let mut i = name_end;
    let mut alias: Option<String> = None;
    if tokens.get(i).is_some_and(|t| t.is("AS")) {
        i += 1;
        match tokens.get(i) {
            Some(t) if matches!(t, Token::Word(_) | Token::Quoted(_)) && !t.is("SET") => {
                alias = Some(t.name());
                i += 1;
            }
            // `AS` with nothing sensible after it — not a shape this
            // understands.
            _ => return Ok(None),
        }
    } else if let Some(t) = tokens.get(i) {
        if matches!(t, Token::Word(_) | Token::Quoted(_)) && !t.is("SET") {
            alias = Some(t.name());
            i += 1;
        }
    }

    if !tokens.get(i).is_some_and(|t| t.is("SET")) {
        return Ok(None);
    }
    let set_start = i + 1;

    // The assignment list ends at the first top-level WHERE or RETURNING, or
    // the end of the statement — not one buried inside a subquery in an
    // assignment's value.
    let mut depth = 0i32;
    let mut set_end = tokens.len();
    for (offset, token) in tokens[set_start..].iter().enumerate() {
        match token.punct() {
            Some('(') => depth += 1,
            Some(')') => depth -= 1,
            _ => {}
        }
        if depth == 0 && (token.is("WHERE") || token.is("RETURNING")) {
            set_end = set_start + offset;
            break;
        }
    }

    let assignments = split_top_level(&tokens[set_start..set_end]);
    if assignments.is_empty() {
        return Ok(None);
    }

    let mut rewritten = Vec::with_capacity(assignments.len());
    let mut changed = false;
    for item in &assignments {
        match strip_assignment_qualifier(item, &table_name, alias.as_deref())? {
            Some(stripped) => {
                changed = true;
                rewritten.push(stripped);
            }
            None => rewritten.push(item.clone()),
        }
    }

    if !changed {
        return Ok(None);
    }

    let mut out: Vec<Token> = tokens[..set_start].to_vec();
    for (index, item) in rewritten.iter().enumerate() {
        if index > 0 {
            out.push(Token::Punct(','));
        }
        out.extend(item.iter().cloned());
    }
    out.extend(tokens[set_end..].iter().cloned());

    Ok(Some(vec![render(&out)]))
}

/// Strip a qualifier off one `SET` assignment's target, if it names the
/// statement's own table — or, once the table is aliased, its alias, which
/// is then the *only* name that qualifies (see [`update_set`]).
///
/// `Ok(None)` means nothing needed to change: the target was already
/// unqualified, or the item is not a plain `[qualifier .] column = ...`
/// shape at all — left for core's own parser to accept or refuse, the same
/// as it does today. `Err` is a qualifier that named something else, or a
/// three-part name: SQLite has no qualified assignment target of any shape
/// (verified directly against `sqlite3`), so neither is a form core could
/// resolve even passed through unchanged.
fn strip_assignment_qualifier(
    item: &[Token],
    table_name: &str,
    alias: Option<&str>,
) -> Result<Option<Vec<Token>>, MysqlError> {
    let Some(eq) = item.iter().position(|t| t.punct() == Some('=')) else {
        return Ok(None);
    };
    let target = &item[..eq];
    match target {
        [qualifier, dot, column] if dot.punct() == Some('.') => {
            let qualifier_name = qualifier.name();
            let expected = alias.unwrap_or(table_name);
            if qualifier_name.eq_ignore_ascii_case(expected) {
                let mut stripped = vec![column.clone()];
                stripped.extend(item[eq..].iter().cloned());
                Ok(Some(stripped))
            } else {
                Err(MysqlError::unknown_table_in_field_list(&qualifier_name))
            }
        }
        [_, d1, _, d2, _] if d1.punct() == Some('.') && d2.punct() == Some('.') => {
            Err(MysqlError::unsupported(format!(
                "`{}` is not supported: a three-part qualified assignment target has no \
                 SQLite equivalent",
                render(target)
            )))
        }
        _ => Ok(None),
    }
}

// --------------------------------------- INSERT ... ON DUPLICATE KEY UPDATE

/// `INSERT ... ON DUPLICATE KEY UPDATE assignment_list` → `INSERT ... ON
/// CONFLICT DO UPDATE SET assignment_list'`, MySQL's own upsert syntax
/// rewritten onto the one core's SQLite dialect has (AHL-476). See the module
/// docs on [`update_set`] for the reasoning: no catalog lookup for a conflict
/// target (core's own targetless `DO UPDATE` already answers for any unique
/// or primary key collision, which is exactly what MySQL's clause does),
/// `VALUES(col)`/the row-alias form both becoming `excluded.col`, the
/// row-alias column-list refusal, and the affected-rows divergence.
///
/// Only the text after `ON DUPLICATE KEY UPDATE` is touched. Everything
/// before it — the target table, the column list, every `VALUES (...)` row —
/// is copied through exactly as written; the one exception is a MySQL
/// 8.0.20+ row alias (`AS new [(...)]`) immediately in front of the clause,
/// which is stripped because SQLite has no row alias on `INSERT` to leave it
/// as.
///
/// `Ok(None)` when no top-level `ON DUPLICATE KEY UPDATE` is found — every
/// ordinary `INSERT`, including `INSERT OR ...`, which has no such clause to
/// find — and the statement is passed through unchanged, the same as every
/// other statement this module does not recognise. `REPLACE INTO` is not
/// even dispatched here: it starts with the word `REPLACE`, not `INSERT`.
fn insert_on_duplicate_key_update(tokens: &[Token]) -> Result<Option<Vec<String>>, MysqlError> {
    let mut depth = 0i32;
    let mut odku_start = None;
    for (i, token) in tokens.iter().enumerate() {
        match token.punct() {
            Some('(') => depth += 1,
            Some(')') => depth -= 1,
            _ => {}
        }
        if depth == 0
            && token.is("ON")
            && tokens.get(i + 1).is_some_and(|t| t.is("DUPLICATE"))
            && tokens.get(i + 2).is_some_and(|t| t.is("KEY"))
            && tokens.get(i + 3).is_some_and(|t| t.is("UPDATE"))
        {
            odku_start = Some(i);
            break;
        }
    }
    let Some(odku_start) = odku_start else {
        return Ok(None);
    };

    let assignment_tokens = &tokens[odku_start + 4..];
    if assignment_tokens.is_empty() {
        return Err(MysqlError::parse(
            "ON DUPLICATE KEY UPDATE needs at least one assignment",
        ));
    }
    let assignments = split_top_level(assignment_tokens);
    if assignments.is_empty() {
        return Err(MysqlError::parse(
            "ON DUPLICATE KEY UPDATE needs at least one assignment",
        ));
    }

    let (head, alias) = strip_row_alias(&tokens[..odku_start])?;

    let mut out: Vec<Token> = head.to_vec();
    out.push(Token::Word("ON".to_string()));
    out.push(Token::Word("CONFLICT".to_string()));
    out.push(Token::Word("DO".to_string()));
    out.push(Token::Word("UPDATE".to_string()));
    out.push(Token::Word("SET".to_string()));
    for (index, assignment) in assignments.iter().enumerate() {
        if index > 0 {
            out.push(Token::Punct(','));
        }
        out.extend(rewrite_odku_assignment(assignment, alias.as_deref()));
    }

    Ok(Some(vec![render(&out)]))
}

/// Strip a MySQL 8.0.20+ row alias (`AS new` or `AS new (col, ...)`) off the
/// end of an `INSERT`'s head, if one is there, returning what is left and the
/// alias name.
///
/// SQLite has no row alias on `INSERT`, and the clause has nothing left to
/// mean once `VALUES(col)`/`new.col` inside `ON DUPLICATE KEY UPDATE` are
/// rewritten onto `excluded.col` — the whole reason a caller writes it — so it
/// is removed rather than passed through for core's parser to trip over.
///
/// The parenthesized column-alias form (`AS new (a, b)`) is refused (`1235`):
/// resolving `new.a` then needs the real column `a` renames, which requires
/// the `INSERT`'s own column list and this function does not have it (nor
/// does anything else in this module reach for the catalog to get it — see
/// [`insert_on_duplicate_key_update`]'s docs).
fn strip_row_alias(head: &[Token]) -> Result<(&[Token], Option<String>), MysqlError> {
    if head.is_empty() {
        return Ok((head, None));
    }

    let mut end = head.len();
    let mut has_column_aliases = false;
    if head[end - 1].punct() == Some(')') {
        let close = end - 1;
        let mut depth = 0i32;
        let mut open = None;
        for i in (0..=close).rev() {
            match head[i].punct() {
                Some(')') => depth += 1,
                Some('(') => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(open) = open else {
            return Ok((head, None));
        };
        has_column_aliases = true;
        end = open;
    }

    if end < 2 {
        return Ok((head, None));
    }
    let alias_token = &head[end - 1];
    let as_token = &head[end - 2];
    if !as_token.is("AS") || !matches!(alias_token, Token::Word(_) | Token::Quoted(_)) {
        return Ok((head, None));
    }
    if has_column_aliases {
        return Err(MysqlError::unsupported(format!(
            "`{}` is not supported: a row-alias column list on ON DUPLICATE KEY UPDATE needs \
             the real column each alias renames, which this server only has from the INSERT's \
             own column list — write VALUES(col), or AS {} without a column list",
            render(&head[end - 2..]),
            alias_token.name()
        )));
    }
    Ok((&head[..end - 2], Some(alias_token.name())))
}

/// Rewrite one `ON DUPLICATE KEY UPDATE` assignment: `VALUES(col)` (bare or
/// backtick-quoted — Laravel's grammar quotes the function name too,
/// `` `values`(`col`) ``) and, once a row alias is given, `alias.col`, both
/// become `excluded.col` wherever they appear in the assignment's value, not
/// only at its top level — `n = n + VALUES(n)` is real MySQL syntax and both
/// occurrences of the stored column's name mean different things.
///
/// Neither substitution changes the number or order of `?` placeholders, so a
/// prepared statement's bound parameters still line up after the rewrite.
fn rewrite_odku_assignment(item: &[Token], alias: Option<&str>) -> Vec<Token> {
    let mut out = Vec::with_capacity(item.len());
    let mut i = 0;
    while i < item.len() {
        if item[i].name().eq_ignore_ascii_case("VALUES")
            && item.get(i + 1).and_then(Token::punct) == Some('(')
        {
            if let (Some(column), Some(close)) = (item.get(i + 2), item.get(i + 3)) {
                if matches!(column, Token::Word(_) | Token::Quoted(_)) && close.punct() == Some(')')
                {
                    out.push(Token::Word("excluded".to_string()));
                    out.push(Token::Punct('.'));
                    out.push(column.clone());
                    i += 4;
                    continue;
                }
            }
        }
        if let Some(alias) = alias {
            if matches!(&item[i], Token::Word(_) | Token::Quoted(_))
                && item[i].name().eq_ignore_ascii_case(alias)
                && item.get(i + 1).and_then(Token::punct) == Some('.')
                && item.get(i + 2).is_some()
            {
                out.push(Token::Word("excluded".to_string()));
                out.push(Token::Punct('.'));
                out.push(item[i + 2].clone());
                i += 3;
                continue;
            }
        }
        out.push(item[i].clone());
        i += 1;
    }
    out
}

/// The values MySQL accepts for `ALGORITHM` and `LOCK`.
///
/// Matching the *value* as well as the keyword is what keeps a column named
/// `lock` safe: `ADD COLUMN lock INT` does not look like `LOCK = NONE`.
const ALGORITHM_VALUES: &[&str] = &["DEFAULT", "INSTANT", "INPLACE", "COPY"];
const LOCK_VALUES: &[&str] = &["DEFAULT", "NONE", "SHARED", "EXCLUSIVE"];

/// Strip `ALGORITHM = x` / `LOCK = x` wherever they appear at the top level,
/// returning the cleaned tokens, or `None` if there was nothing to strip.
fn strip_online_ddl_specs(tokens: &[Token], dropped: &mut Vec<Dropped>) -> Option<Vec<Token>> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut changed = false;
    let mut depth = 0i32;
    let mut i = 0;

    while i < tokens.len() {
        let token = &tokens[i];
        match token.punct() {
            Some('(') => depth += 1,
            Some(')') => depth -= 1,
            _ => {}
        }

        if depth == 0 {
            let values = if token.is("ALGORITHM") {
                Some(ALGORITHM_VALUES)
            } else if token.is("LOCK") {
                Some(LOCK_VALUES)
            } else {
                None
            };
            if let Some(values) = values {
                let mut at = i + 1;
                if tokens.get(at).and_then(Token::punct) == Some('=') {
                    at += 1;
                }
                let matched = tokens
                    .get(at)
                    .is_some_and(|value| values.iter().any(|v| value.is(v)));
                if matched {
                    dropped.push(Dropped::new(
                        render(&tokens[i..=at]),
                        "InlaySQL has one way of applying a schema change; MySQL's online-DDL \
                         steering has nothing to select between",
                    ));
                    // The spec is comma-separated from the rest, so exactly one
                    // of the commas around it has to go with it.
                    if out.last().and_then(Token::punct) == Some(',') {
                        out.pop();
                    } else if tokens.get(at + 1).and_then(Token::punct) == Some(',') {
                        at += 1;
                    }
                    changed = true;
                    i = at + 1;
                    continue;
                }
            }
        }

        out.push(token.clone());
        i += 1;
    }

    changed.then_some(out)
}

// -------------------------------------------------------------- helpers

/// The index of the `)` closing the `(` at `open`.
fn matching_paren(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, token) in tokens.iter().enumerate().skip(open) {
        match token.punct() {
            Some('(') => depth += 1,
            Some(')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split on commas that are not inside parentheses.
fn split_top_level(tokens: &[Token]) -> Vec<Vec<Token>> {
    let mut items = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0i32;
    for token in tokens {
        match token.punct() {
            Some('(') => depth += 1,
            Some(')') => depth -= 1,
            Some(',') if depth == 0 => {
                items.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(token.clone());
    }
    if !current.is_empty() {
        items.push(current);
    }
    items
}

/// The identifiers listed in an item's first parenthesis group.
///
/// Used to read the columns out of `PRIMARY KEY (a, b)`. Length prefixes and
/// `ASC`/`DESC` are skipped, so `PRIMARY KEY (email(191) DESC)` still names
/// `email`.
fn paren_names(item: &[Token]) -> Vec<String> {
    let Some(open) = item.iter().position(|t| t.punct() == Some('(')) else {
        return Vec::new();
    };
    let Some(close) = matching_paren(item, open) else {
        return Vec::new();
    };
    split_top_level(&item[open + 1..close])
        .iter()
        .filter_map(|part| part.first().map(Token::name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlaysql::{Collation, Column, DataType, Index, IndexKind, Table};

    /// Every test below that does not care about pre-existing indexes runs
    /// against an empty catalog.
    fn empty_catalog() -> Catalog {
        Catalog::new()
    }

    /// The statements a translation becomes, for a case that must not be
    /// refused.
    fn statements(sql: &str) -> Vec<String> {
        statements_against(sql, &empty_catalog())
    }

    fn statements_against(sql: &str, catalog: &Catalog) -> Vec<String> {
        translate(sql, catalog)
            .unwrap_or_else(|e| panic!("{sql} was refused: {e}"))
            .statements
    }

    /// The SQL a statement becomes, for a case that must not be refused and
    /// must become exactly one statement.
    fn out(sql: &str) -> String {
        let statements = statements(sql);
        let [only] = statements.as_slice() else {
            panic!(
                "{sql} became {} statements, not one: {statements:?}",
                statements.len()
            );
        };
        only.clone()
    }

    /// The reasons recorded for every clause that was dropped.
    fn dropped(sql: &str) -> Vec<String> {
        translate(sql, &empty_catalog())
            .unwrap_or_else(|e| panic!("{sql} was refused: {e}"))
            .dropped
            .iter()
            .map(|d| d.clause.clone())
            .collect()
    }

    /// The error code a statement is refused with.
    fn refusal(sql: &str) -> MysqlError {
        refusal_against(sql, &empty_catalog())
    }

    fn refusal_against(sql: &str, catalog: &Catalog) -> MysqlError {
        match translate(sql, catalog) {
            Err(error) => error,
            Ok(translation) => panic!(
                "{sql} should have been refused, became {:?}",
                translation.statements
            ),
        }
    }

    /// A catalog holding one table, `users(id, email)`, with whatever indexes
    /// the caller wants already declared on it — for the disambiguation and
    /// FK-warning tests, which need something to check against.
    fn catalog_with_users_table(indexes: Vec<Index>) -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                name: "users".to_string(),
                columns: vec![
                    Column::primary_key("id", DataType::Integer),
                    Column::new("email", DataType::Text),
                    Column::new("name", DataType::Text).with_collation(Collation::NoCase),
                ],
            })
            .unwrap();
        for index in indexes {
            catalog.create_index(index).unwrap();
        }
        catalog
    }

    // ------------------------------------------------------ neutralised

    /// The statement that was the whole wall: it now reaches the engine.
    #[test]
    fn auto_increment_on_an_integer_primary_key_is_dropped() {
        assert_eq!(
            out("create table t (id bigint auto_increment primary key)"),
            "create table t (id bigint primary key)"
        );
        // The clause is reported as it was written, not as this module spells it.
        assert_eq!(
            dropped("create table t (id bigint auto_increment primary key)"),
            vec!["auto_increment"]
        );
        assert_eq!(
            dropped("create table t (id bigint AUTO_INCREMENT primary key)"),
            vec!["AUTO_INCREMENT"]
        );
    }

    /// The key can be declared as a table constraint instead, which is the
    /// other spelling MySQL clients emit.
    #[test]
    fn auto_increment_sees_a_table_level_primary_key_too() {
        assert_eq!(
            out("create table t (`id` bigint auto_increment, primary key (`id`))"),
            "create table t (`id` bigint, primary key (`id`))"
        );
    }

    #[test]
    fn unsigned_is_dropped_and_says_what_that_costs() {
        assert_eq!(
            out("create table t (id bigint unsigned primary key)"),
            "create table t (id bigint primary key)"
        );
        let translation = translate(
            "create table t (id bigint unsigned primary key)",
            &empty_catalog(),
        )
        .unwrap();
        assert!(
            translation.dropped[0]
                .reason
                .contains("9223372036854775807"),
            "the BIGINT reason must name the value that stops round-tripping: {}",
            translation.dropped[0].reason
        );
        // A narrower width loses no range at all, and says so instead.
        let narrow = translate("create table t (n int unsigned)", &empty_catalog()).unwrap();
        assert!(
            narrow.dropped[0]
                .reason
                .contains("nothing is lost to the width"),
            "got {}",
            narrow.dropped[0].reason
        );
    }

    #[test]
    fn table_options_are_dropped_in_every_spelling() {
        assert_eq!(
            out("create table t (a int) engine=InnoDB default charset=utf8mb4 collate=utf8mb4_unicode_ci"),
            "create table t (a int)"
        );
        // Laravel's own spelling: `character set` in full, collation quoted.
        assert_eq!(
            out(
                "create table t (a int) default character set utf8mb4 collate 'utf8mb4_unicode_ci'"
            ),
            "create table t (a int)"
        );
        assert_eq!(
            out("create table t (a int) ENGINE = InnoDB, ROW_FORMAT = DYNAMIC, AUTO_INCREMENT = 100"),
            "create table t (a int)"
        );
    }

    /// A character set is dropped — one encoding here, nothing to select — and
    /// a collation is *translated* (AHL-469), because the engine has three of
    /// its own now and one of them means what MySQL's did.
    #[test]
    fn a_column_charset_is_dropped_and_its_collation_is_translated() {
        assert_eq!(
            out("create table t (a varchar(255) character set utf8mb4 collate utf8mb4_bin)"),
            "create table t (a varchar (255) COLLATE BINARY)"
        );
        assert_eq!(
            out("create table t (a varchar(255) charset=utf8mb4)"),
            "create table t (a varchar (255))"
        );
        assert_eq!(
            out("create table t (a varchar(255) collate utf8mb4_unicode_ci)"),
            "create table t (a varchar (255) COLLATE NOCASE)"
        );
    }

    /// `*_bin` is exact and says nothing; `*_ci` is a narrowing and has to.
    #[test]
    fn a_narrowed_collation_warns_and_an_exact_one_does_not() {
        let exact = translate(
            "create table t (a varchar(255) collate utf8mb4_bin)",
            &empty_catalog(),
        )
        .unwrap();
        assert!(
            exact.dropped.is_empty(),
            "a byte-wise collation is exactly BINARY: {:?}",
            exact.dropped
        );

        let narrowed = translate(
            "create table t (a varchar(255) collate utf8mb4_unicode_ci)",
            &empty_catalog(),
        )
        .unwrap();
        let [warning] = narrowed.dropped.as_slice() else {
            panic!("expected one warning, got {:?}", narrowed.dropped)
        };
        assert_eq!(warning.mapped_to.as_deref(), Some("NOCASE"));
        let (code, message) = warning.warning();
        assert_eq!(code, 1618);
        assert!(message.contains("COLLATE NOCASE"), "{message}");
        // The two gaps this leaves have to be named, not implied.
        assert!(message.contains("accent-insensitive"), "{message}");
        assert!(message.contains("non-ASCII case"), "{message}");
    }

    /// `utf8mb4_0900_as_ci` is accent-*sensitive*, so the accent gap is the one
    /// thing it does not have — and the warning must not claim it does.
    #[test]
    fn an_accent_sensitive_ci_collation_does_not_claim_an_accent_gap() {
        let translation = translate(
            "create table t (a varchar(255) collate utf8mb4_0900_as_ci)",
            &empty_catalog(),
        )
        .unwrap();
        let [warning] = translation.dropped.as_slice() else {
            panic!("expected one warning, got {:?}", translation.dropped)
        };
        let (_, message) = warning.warning();
        assert!(!message.contains("accent-insensitive"), "{message}");
        assert!(message.contains("non-ASCII case"), "{message}");
    }

    /// A collation nothing here means is dropped, not guessed at: `BINARY`
    /// stands and the warning names it.
    #[test]
    fn an_unknown_collation_is_dropped_and_named() {
        let translation = translate(
            "create table t (a varchar(255) collate utf8mb4_klingon)",
            &empty_catalog(),
        )
        .unwrap();
        assert_eq!(
            translation.statements,
            vec!["create table t (a varchar (255))"]
        );
        let [warning] = translation.dropped.as_slice() else {
            panic!("expected one warning, got {:?}", translation.dropped)
        };
        assert_eq!(warning.mapped_to, None);
        let (_, message) = warning.warning();
        assert!(message.contains("utf8mb4_klingon"), "{message}");
        assert!(message.contains("BINARY"), "{message}");
    }

    /// A `*_cs` collation compares case-sensitively, which for equality is
    /// `BINARY`; the ordering still differs and the warning says so.
    #[test]
    fn a_case_sensitive_collation_maps_to_binary_with_the_ordering_named() {
        let translation = translate(
            "create table t (a varchar(255) collate utf8mb4_0900_as_cs)",
            &empty_catalog(),
        )
        .unwrap();
        assert_eq!(
            translation.statements,
            vec!["create table t (a varchar (255) COLLATE BINARY)"]
        );
        let [warning] = translation.dropped.as_slice() else {
            panic!("expected one warning, got {:?}", translation.dropped)
        };
        assert_eq!(warning.mapped_to.as_deref(), Some("BINARY"));
        assert!(warning.warning().1.contains("ordering"), "{warning:?}");
    }

    /// The table's collation reaches every string column that wrote none — the
    /// step that makes the mapping matter, since an ORM puts the collation on
    /// the table and never on the columns.
    #[test]
    fn a_table_collation_reaches_the_string_columns_and_nothing_else() {
        assert_eq!(
            out(
                "create table t (a int, b varchar(255), c text, d datetime) \
                 default charset=utf8mb4 collate=utf8mb4_unicode_ci"
            ),
            "create table t (a int, b varchar (255) COLLATE NOCASE, c text COLLATE NOCASE, \
             d datetime)"
        );
    }

    /// A column that wrote its own keeps it: MySQL's rule, and the one that
    /// lets a single case-sensitive column live in a case-insensitive table.
    #[test]
    fn a_column_collation_overrides_the_tables() {
        assert_eq!(
            out(
                "create table t (a varchar(255) collate utf8mb4_bin, b varchar(255)) \
                 collate=utf8mb4_unicode_ci"
            ),
            "create table t (a varchar (255) COLLATE BINARY, b varchar (255) COLLATE NOCASE)"
        );
    }

    #[test]
    fn online_ddl_steering_comes_off_an_alter() {
        assert_eq!(
            out("alter table t add column x int, algorithm=inplace"),
            "alter table t add column x int"
        );
        assert_eq!(
            out("alter table t add column x int, ALGORITHM = INPLACE, LOCK = NONE"),
            "alter table t add column x int"
        );
        assert_eq!(
            out("alter table t algorithm=copy, add column x int"),
            "alter table t add column x int"
        );
    }

    /// A column really called `lock` must not be mistaken for a `LOCK =` spec.
    #[test]
    fn a_column_named_lock_survives() {
        assert_eq!(
            out("alter table t add column lock int"),
            "alter table t add column lock int"
        );
    }

    // ----------------------------------------------------------- refused

    /// The three that change what the table means. Each has to fail, and fail
    /// with a code a client can act on rather than a syntax error.
    #[test]
    fn auto_increment_off_the_primary_key_is_refused() {
        let error = refusal("create table t (id bigint primary key, n bigint auto_increment)");
        assert_eq!(error.code, 1235);
        assert!(
            error.message.contains("not declared PRIMARY KEY"),
            "{error}"
        );
    }

    #[test]
    fn auto_increment_on_a_non_integer_is_refused() {
        let error = refusal("create table t (id varchar(36) auto_increment primary key)");
        assert_eq!(error.code, 1235);
        assert!(error.message.contains("VARCHAR"), "{error}");
    }

    #[test]
    fn on_update_current_timestamp_is_refused() {
        for sql in [
            "create table t (a int, ts timestamp on update current_timestamp)",
            "create table t (a int, ts timestamp on update CURRENT_TIMESTAMP(3))",
            "create table t (a int, ts timestamp on update now())",
        ] {
            let error = refusal(sql);
            assert_eq!(error.code, 1235, "{sql}");
            assert!(error.message.contains("ON UPDATE"), "{error}");
        }
    }

    /// A foreign key's `ON UPDATE` is a different clause with the same first
    /// two words, and must not be caught by the one above.
    #[test]
    fn a_foreign_key_action_is_not_mistaken_for_a_timestamp_default() {
        let translation = translate(
            "create table t (a int references u(id) on update cascade)",
            &empty_catalog(),
        )
        .unwrap();
        assert!(translation.dropped.is_empty());
        assert_eq!(
            translation.statements,
            vec!["create table t (a int references u(id) on update cascade)"]
        );
    }

    #[test]
    fn zerofill_is_refused() {
        assert_eq!(
            refusal("create table t (a int unsigned zerofill)").code,
            1235
        );
    }

    #[test]
    fn an_inline_index_declaration_is_refused_rather_than_dropped() {
        for sql in [
            "create table t (a int, key t_a_index (a))",
            "create table t (a int, index t_a_index (a))",
            "create table t (a int, unique key t_a_unique (a))",
            "create table t (a int, fulltext key t_a_ft (a))",
        ] {
            let error = refusal(sql);
            assert_eq!(error.code, 1235, "{sql}");
            assert!(error.message.contains("CREATE INDEX"), "{error}");
        }
    }

    #[test]
    fn an_unknown_table_option_is_refused_rather_than_guessed_at() {
        let error = refusal("create table t (a int) partition by hash (a)");
        assert_eq!(error.code, 1235);
        assert!(error.message.contains("partition"), "{error}");
    }

    #[test]
    fn an_index_type_hint_is_refused() {
        let error = refusal("create index i on t (a) using btree");
        assert_eq!(error.code, 1235);
        assert!(error.message.contains("BM25"), "{error}");
    }

    // -------------------------------------------------------- untouched

    /// Anything without MySQL decoration must come back byte for byte, so a
    /// bug in this module cannot reshape a statement it had no business
    /// touching.
    #[test]
    fn statements_with_nothing_to_translate_are_returned_verbatim() {
        for sql in [
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, e VECTOR(4))",
            "create table t (a int not null default 3)",
            "create index docs_body on docs (body)",
            "alter table t add column x int",
            "drop table if exists t",
            "select * from docs",
            "insert into docs (body) values ('x')",
            "",
        ] {
            let translation = translate(sql, &empty_catalog()).unwrap();
            assert_eq!(
                translation.statements,
                vec![sql.to_string()],
                "{sql} was rewritten"
            );
            assert!(translation.dropped.is_empty(), "{sql}");
        }
    }

    /// A `CREATE TABLE` that ends in the *engine's* dialect is not MySQL's and
    /// is not this module's. Refusing `STRICT` as an unknown "table option"
    /// would be a worse answer than the engine's own.
    #[test]
    fn a_tail_in_the_engines_own_dialect_is_left_to_the_engine() {
        for sql in [
            "create table t (a int) strict",
            "create table t (a int) without rowid",
            "create table t (a int) as select 1",
        ] {
            let translation = translate(sql, &empty_catalog()).unwrap();
            assert_eq!(translation.statements, vec![sql.to_string()]);
            assert!(translation.dropped.is_empty(), "{sql}");
        }
    }

    /// Phase 1b of `docs/architecture.md` implements these in core. Translating around them
    /// here would hide which layer owes the fix.
    #[test]
    fn constraints_that_belong_to_core_are_left_alone() {
        let sql = "create table t (a int not null, b varchar(255) default 'x' unique, \
                   c timestamp null, unique (a), foreign key (a) references u(id))";
        assert_eq!(
            translate(sql, &empty_catalog()).unwrap().statements,
            vec![sql.to_string()]
        );
    }

    // ------------------------------------------------------- the corpus

    /// The statement this whole change exists for, in the shape a schema
    /// builder emits it: quoted identifiers, unsigned auto-increment key,
    /// charset and collation on the table.
    #[test]
    fn the_migration_shape_becomes_ordinary_sql() {
        let translation = translate(
            "create table `users` (`id` bigint unsigned not null auto_increment primary key, \
             `name` varchar(255) not null, `email` varchar(255) not null) \
             default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
            &empty_catalog(),
        )
        .unwrap();
        assert_eq!(
            translation.statements,
            vec![
                "create table `users` (`id` bigint not null primary key, \
                 `name` varchar (255) not null COLLATE NOCASE, \
                 `email` varchar (255) not null COLLATE NOCASE)"
            ]
        );
        assert_eq!(
            translation
                .dropped
                .iter()
                .map(|d| d.clause.as_str())
                .collect::<Vec<_>>(),
            vec![
                "UNSIGNED",
                "auto_increment",
                "default character set utf8mb4",
                "collate 'utf8mb4_unicode_ci'"
            ]
        );
    }

    /// Every drop is reported. A client that runs one of these statements can
    /// ask what happened to it and get a straight answer.
    #[test]
    fn every_drop_becomes_a_warning_that_names_the_clause() {
        let translation = translate(
            "create table t (id bigint unsigned auto_increment primary key) engine=InnoDB",
            &empty_catalog(),
        )
        .unwrap();
        assert_eq!(translation.dropped.len(), 3);
        for drop in &translation.dropped {
            let (code, message) = drop.warning();
            assert_eq!(code, 1618);
            assert!(message.contains(&drop.clause), "{message}");
            assert!(message.len() > drop.clause.len() + 20, "{message}");
        }
    }

    // ------------------------------------------------------- tokenizer

    /// A quoted span is handed to the engine exactly as it arrived. If the
    /// backticks or the escapes inside them were lost, a column called `order`
    /// would become a keyword and a default of `it's` would become a syntax
    /// error.
    #[test]
    fn quoted_spans_survive_tokenizing_and_rendering() {
        for (sql, expected) in [
            (
                "create table `we``ird` (`a b` int)",
                "create table `we``ird` (`a b` int)",
            ),
            (
                "create table t (a varchar(3) default 'a,b')",
                "create table t (a varchar (3) default 'a,b')",
            ),
            (
                "create table t (a varchar(3) default 'it''s')",
                "create table t (a varchar (3) default 'it''s')",
            ),
            (
                "create table t (a varchar(3) default 'it\\'s')",
                "create table t (a varchar (3) default 'it\\'s')",
            ),
        ] {
            assert_eq!(render(&tokenize(sql)), expected, "{sql}");
        }
    }

    #[test]
    fn a_comma_inside_a_string_does_not_split_a_column_list() {
        let translation = translate(
            "create table t (a int unsigned, b varchar(9) default 'x, y')",
            &empty_catalog(),
        )
        .unwrap();
        assert_eq!(
            translation.statements,
            vec!["create table t (a int, b varchar (9) default 'x, y')"]
        );
    }

    // ------------------------------------------------- ALTER TABLE indexes

    /// `ADD INDEX`/`ADD KEY`, named or not, becomes a free-standing `CREATE
    /// INDEX`. Item 1 of AHL-474: this is the exact shape Laravel's
    /// `->index()` compiles to.
    #[test]
    fn add_index_becomes_a_free_standing_create_index() {
        assert_eq!(
            statements("alter table users add index users_email_index (email)"),
            vec!["CREATE INDEX `users_email_index` ON users (`email`)"]
        );
        assert_eq!(
            statements("alter table `users` add key (`email`)"),
            vec!["CREATE INDEX `email` ON `users` (`email`)"]
        );
        // Nothing is lost by the rewrite, so nothing is warned about.
        assert!(dropped("alter table users add index (email)").is_empty());
    }

    /// `ADD UNIQUE`, in every spelling MySQL accepts, becomes `CREATE UNIQUE
    /// INDEX` — item 2 and item 3 of AHL-474.
    #[test]
    fn add_unique_and_add_constraint_unique_become_create_unique_index() {
        assert_eq!(
            statements("alter table users add unique (email)"),
            vec!["CREATE UNIQUE INDEX `email` ON users (`email`)"]
        );
        assert_eq!(
            statements("alter table users add unique index users_email_unique (email)"),
            vec!["CREATE UNIQUE INDEX `users_email_unique` ON users (`email`)"]
        );
        assert_eq!(
            statements("alter table users add unique key users_email_unique (email)"),
            vec!["CREATE UNIQUE INDEX `users_email_unique` ON users (`email`)"]
        );
        // Laravel's own compiled form: the constraint symbol names the index.
        assert_eq!(
            statements("alter table users add constraint users_email_unique unique (email)"),
            vec!["CREATE UNIQUE INDEX `users_email_unique` ON users (`email`)"]
        );
    }

    /// A composite index still names itself after only the *first* column —
    /// MySQL's rule, not "every column joined together".
    #[test]
    fn a_composite_index_is_named_after_its_first_column_only() {
        assert_eq!(
            statements("alter table users add index (email, name)"),
            vec!["CREATE INDEX `email` ON users (`email`, `name`)"]
        );
    }

    /// An unnamed index that collides with one already on the table gets
    /// MySQL's own disambiguating suffix (`_2`, `_3`, ...), checked against
    /// the catalog — not against nothing, or two migrations that each add an
    /// unnamed index on the same first column would collide.
    #[test]
    fn an_unnamed_index_is_disambiguated_against_the_catalog() {
        let catalog = catalog_with_users_table(vec![Index::single(
            "email".to_string(),
            "users".to_string(),
            "email".to_string(),
            IndexKind::FullText,
        )]);
        assert_eq!(
            statements_against("alter table users add index (email)", &catalog),
            vec!["CREATE INDEX `email_2` ON users (`email`)"]
        );
    }

    /// Two unnamed indexes added by the *same* statement must not collide
    /// with each other either, even with no catalog entry yet.
    #[test]
    fn two_unnamed_indexes_in_one_statement_disambiguate_against_each_other() {
        assert_eq!(
            statements("alter table users add index (email), add index (email)"),
            vec![
                "CREATE INDEX `email` ON users (`email`)",
                "CREATE INDEX `email_2` ON users (`email`)",
            ]
        );
    }

    /// `DROP INDEX`/`DROP KEY` inside an `ALTER TABLE` becomes the standalone
    /// `DROP INDEX` — item 5 of AHL-474. No table qualifier: SQLite's index
    /// names are global, unlike MySQL's per-table ones, so there is nowhere
    /// for it to go and nothing lost by leaving it off.
    #[test]
    fn drop_index_becomes_a_standalone_drop_index() {
        assert_eq!(
            statements("alter table users drop index users_email_unique"),
            vec!["DROP INDEX `users_email_unique`"]
        );
        assert_eq!(
            statements("alter table users drop key users_email_unique"),
            vec!["DROP INDEX `users_email_unique`"]
        );
    }

    /// `RENAME INDEX` has no counterpart at all — core can drop and recreate
    /// an index but cannot rename one in place — so it is refused rather than
    /// silently turned into two statements with a window between them where
    /// the index does not exist. Item 5 of AHL-474.
    #[test]
    fn rename_index_is_refused() {
        let error = refusal("alter table users rename index a to b");
        assert_eq!(error.code, 1235);
        assert!(error.message.contains("RENAME INDEX"), "{error}");

        let error = refusal("alter table users rename key a to b");
        assert_eq!(error.code, 1235);
    }

    /// `ADD CONSTRAINT ... FOREIGN KEY` has no ALTER path in core at all —
    /// only `CREATE TABLE` can declare one, and even there it is unenforced.
    /// So this is OK, with a `1618` naming exactly what was not recorded —
    /// item 4 of AHL-474, and never a silent success.
    #[test]
    fn add_foreign_key_is_ok_with_a_warning_naming_what_was_not_recorded() {
        let catalog = catalog_with_users_table(vec![]);
        let translation = translate(
            "alter table users add constraint users_role_id_foreign foreign key (role_id) \
             references roles (id)",
            &catalog,
        )
        .unwrap();
        assert!(
            translation.statements.is_empty(),
            "nothing runs on the engine: {:?}",
            translation.statements
        );
        let [warning] = translation.dropped.as_slice() else {
            panic!(
                "expected exactly one warning, got {:?}",
                translation.dropped
            )
        };
        let (code, message) = warning.warning();
        assert_eq!(code, 1618);
        assert!(
            message.to_ascii_lowercase().contains("foreign key"),
            "{message}"
        );
        assert!(message.contains("not recorded"), "{message}");
        assert!(message.contains("never"), "{message}");
    }

    /// The same statement against a table that does not exist must not
    /// silently answer OK — the warning explains what was not recorded, not
    /// what table it was never recorded *on*.
    #[test]
    fn add_foreign_key_on_a_missing_table_is_still_refused() {
        let error = refusal(
            "alter table ghosts add constraint ghosts_x_foreign foreign key (x) \
             references y (id)",
        );
        assert_eq!(error.code, 1146);
    }

    /// `ADD FOREIGN KEY` with no `CONSTRAINT` symbol at all is the same
    /// shape under MySQL's other spelling.
    #[test]
    fn add_foreign_key_without_a_constraint_symbol_is_ok_with_a_warning() {
        let catalog = catalog_with_users_table(vec![]);
        let translation = translate(
            "alter table users add foreign key (role_id) references roles (id)",
            &catalog,
        )
        .unwrap();
        assert!(translation.statements.is_empty());
        assert_eq!(translation.dropped.len(), 1);
    }

    /// Laravel's own shape: `CREATE TABLE` with its usual decoration, then a
    /// separate `ADD UNIQUE`, then `ADD INDEX`, then `ADD CONSTRAINT ...
    /// FOREIGN KEY`, then `DROP INDEX` — every statement AHL-471 found the
    /// shim refusing, run in the order a migration actually sends them.
    #[test]
    fn a_realistic_laravel_migration_sequence_translates_end_to_end() {
        let catalog = catalog_with_users_table(vec![]);
        assert_eq!(
            statements_against(
                "alter table `users` add unique `users_email_unique` (`email`)",
                &catalog
            ),
            vec!["CREATE UNIQUE INDEX `users_email_unique` ON `users` (`email`)"]
        );
        assert_eq!(
            statements_against(
                "alter table `users` add index `users_name_index` (`name`)",
                &catalog
            ),
            vec!["CREATE INDEX `users_name_index` ON `users` (`name`)"]
        );
        let fk = translate(
            "alter table `users` add constraint `users_role_id_foreign` foreign key (`role_id`) \
             references `roles` (`id`)",
            &catalog,
        )
        .unwrap();
        assert!(fk.statements.is_empty());
        assert_eq!(fk.dropped.len(), 1);
        assert_eq!(
            statements_against(
                "alter table `users` drop index `users_name_index`",
                &catalog
            ),
            vec!["DROP INDEX `users_name_index`"]
        );
    }

    // -------------------------------------------- ALTER TABLE, multi-op

    /// MySQL's comma-separated operation list becomes one statement per
    /// operation — SQLite's `ALTER TABLE` (and the engine) accepts exactly
    /// one. Item 6 of AHL-474.
    #[test]
    fn a_multi_operation_alter_splits_into_one_statement_per_operation() {
        assert_eq!(
            statements("alter table users add column age int, add index (age)"),
            vec![
                "alter table users add column age int",
                "CREATE INDEX `age` ON users (`age`)",
            ]
        );
    }

    /// Three operations, not two — the split is not hard-coded to a pair.
    #[test]
    fn a_three_operation_alter_splits_into_three_statements() {
        assert_eq!(
            statements(
                "alter table users add column age int, add index (age), \
                 drop index users_name_index"
            ),
            vec![
                "alter table users add column age int",
                "CREATE INDEX `age` ON users (`age`)",
                "DROP INDEX `users_name_index`",
            ]
        );
    }

    // ------------------------------------------------------ TRUNCATE TABLE

    /// `TRUNCATE TABLE` becomes `DELETE FROM`, and always carries a `1618`:
    /// the row id counter is not reset, unlike MySQL's own `TRUNCATE`. Item 7
    /// of AHL-474.
    #[test]
    fn truncate_table_becomes_delete_from_with_a_warning() {
        let translation = translate("truncate table users", &empty_catalog()).unwrap();
        assert_eq!(translation.statements, vec!["DELETE FROM users"]);
        let [warning] = translation.dropped.as_slice() else {
            panic!("expected one warning, got {:?}", translation.dropped)
        };
        let (code, message) = warning.warning();
        assert_eq!(code, 1618);
        assert!(message.contains("row id"), "{message}");
        assert!(message.contains("DELETE FROM users"), "{message}");
    }

    /// MySQL allows `TABLE` to be dropped entirely.
    #[test]
    fn truncate_without_the_table_keyword_is_the_same_statement() {
        assert_eq!(
            translate("truncate users", &empty_catalog())
                .unwrap()
                .statements,
            vec!["DELETE FROM users"]
        );
    }

    // ------------------------------------------------------- RENAME TABLE

    /// The standalone `RENAME TABLE` becomes `ALTER TABLE ... RENAME TO`,
    /// core's own spelling of the same thing — item 7 of AHL-474. Nothing is
    /// lost, so nothing is warned about.
    #[test]
    fn standalone_rename_table_becomes_alter_table_rename_to() {
        let translation = translate("rename table users to people", &empty_catalog()).unwrap();
        assert_eq!(
            translation.statements,
            vec!["ALTER TABLE users RENAME TO people"]
        );
        assert!(translation.dropped.is_empty());
    }

    /// MySQL's multi-pair form splits into one `ALTER TABLE` per pair.
    #[test]
    fn a_multi_pair_rename_table_splits_into_one_statement_per_pair() {
        assert_eq!(
            statements("rename table a to b, c to d"),
            vec!["ALTER TABLE a RENAME TO b", "ALTER TABLE c RENAME TO d"]
        );
    }

    // ------------------------------------ UPDATE ... SET qualifier (AHL-475)

    /// The exact shape Eloquent writes on every save of a model with
    /// timestamps: a bare assignment beside a qualified one, the qualifier
    /// naming the statement's own table.
    #[test]
    fn a_qualified_set_target_naming_the_table_is_stripped() {
        assert_eq!(
            out("update users set name = ?, users.updated_at = ? where users.id = ?"),
            "update users set name = ?, updated_at = ? where users.id = ?"
        );
    }

    /// Every assignment qualified, not just one.
    #[test]
    fn every_qualified_assignment_in_the_list_is_stripped() {
        assert_eq!(
            out("update users set users.name = ?, users.updated_at = ?"),
            "update users set name = ?, updated_at = ?"
        );
    }

    /// Backtick-quoted, the way a schema builder's grammar actually spells
    /// identifiers.
    #[test]
    fn a_backtick_quoted_qualifier_is_recognised_and_stripped() {
        assert_eq!(
            out("update `users` set `name` = ?, `users`.`updated_at` = ?"),
            "update `users` set `name` = ?, `updated_at` = ?"
        );
    }

    /// `WHERE`'s own qualified reference is untouched — only the assignment
    /// target is this function's business.
    #[test]
    fn where_is_left_exactly_as_written() {
        assert_eq!(
            out("update users set users.name = ? where users.id = ? and users.active = 1"),
            "update users set name = ? where users.id = ? and users.active = 1"
        );
    }

    /// `RETURNING` likewise.
    #[test]
    fn returning_is_left_exactly_as_written() {
        assert_eq!(
            out("update users set users.name = ? returning users.id, users.name"),
            "update users set name = ? returning users.id, users.name"
        );
    }

    /// A qualifier naming a real table that is not the statement's own is
    /// refused by name — MySQL's own code and wording for exactly this.
    #[test]
    fn a_qualifier_naming_another_table_is_refused_by_name() {
        let error = refusal("update users set name = ?, posts.updated_at = ?");
        assert_eq!(error.code, 1109, "ER_UNKNOWN_TABLE");
        assert_eq!(error.sqlstate, "42S02");
        assert!(error.message.contains("posts"), "{}", error.message);
    }

    /// A qualifier naming nothing at all gets the same refusal — this pass
    /// has no catalog access and needs none: the qualifier is checked against
    /// the statement's own table and alias alone.
    #[test]
    fn a_qualifier_naming_no_table_at_all_is_refused_by_name() {
        let error = refusal("update users set name = ?, bogus.updated_at = ?");
        assert_eq!(error.code, 1109);
        assert!(error.message.contains("bogus"), "{}", error.message);
    }

    /// A three-part name has no SQLite equivalent — refused outright rather
    /// than silently dropping the leading part.
    #[test]
    fn a_three_part_qualified_target_is_refused() {
        let error = refusal("update users set name = ?, main.users.updated_at = ?");
        assert_eq!(error.code, 1235, "ER_NOT_SUPPORTED_YET");
        assert!(error.message.contains("main"), "{}", error.message);
    }

    /// An aliased target: the qualifier must be the alias once one is given —
    /// checked directly against `sqlite3`, which refuses the statement's own
    /// real table name once it has been aliased (`WHERE users.id = ...` on
    /// `UPDATE users AS u` is `no such column: users.id` there). This module
    /// follows the same rule for the assignment target it is allowed to
    /// rewrite at all.
    #[test]
    fn an_aliased_target_is_matched_by_its_alias() {
        assert_eq!(
            out("update users as u set name = ?, u.updated_at = ? where u.id = ?"),
            "update users as u set name = ?, updated_at = ? where u.id = ?"
        );
    }

    /// Once aliased, the real table name is no longer a valid qualifier for
    /// the assignment target either.
    #[test]
    fn an_aliased_target_rejects_the_real_table_name_as_a_qualifier() {
        let error = refusal("update users as u set name = ?, users.updated_at = ?");
        assert_eq!(error.code, 1109);
        assert!(error.message.contains("users"), "{}", error.message);
    }

    /// Nothing qualified at all: byte-for-byte pass-through, not a
    /// `Rewritten` carrying an identical string — the overwhelmingly common
    /// case, and the one `handle_engine_statement` relies on to answer
    /// `PassThrough`.
    #[test]
    fn an_update_with_no_qualified_target_is_untouched() {
        let translation =
            translate("update users set name = ? where id = ?", &empty_catalog()).unwrap();
        assert_eq!(
            translation.statements,
            vec!["update users set name = ? where id = ?"]
        );
        assert!(translation.dropped.is_empty());
    }

    /// `ON CONFLICT DO UPDATE SET` is SQLite's own upsert syntax, not
    /// something this pass rewrites — it stays refused by core the same way
    /// plain `UPDATE ... SET` does, consistently, because `sqlite3` refuses a
    /// qualified target there too.
    #[test]
    fn on_conflict_do_update_set_is_not_this_functions_business() {
        let translation = translate(
            "insert into users (id, name) values (1, 'a') on conflict (id) do update set \
             users.name = excluded.name",
            &empty_catalog(),
        )
        .unwrap();
        assert_eq!(
            translation.statements,
            vec![
                "insert into users (id, name) values (1, 'a') on conflict (id) do update set \
                 users.name = excluded.name"
            ]
        );
    }

    // ------------------------------- INSERT ... ON DUPLICATE KEY UPDATE

    /// `VALUES(col)` becomes `excluded.col`, and no conflict target is added —
    /// see the module docs on why that is the exact mapping and not a
    /// narrower stand-in for it.
    #[test]
    fn values_col_becomes_excluded_col_with_no_target() {
        assert_eq!(
            out(
                "insert into t (id, e, n) values (?, ?, ?) on duplicate key update \
                 n = values(n)"
            ),
            "insert into t (id, e, n) values (?, ?, ?) ON CONFLICT DO UPDATE SET n = excluded.n"
        );
    }

    /// Laravel's grammar backtick-quotes the function name too.
    #[test]
    fn a_backtick_quoted_values_function_is_recognised() {
        assert_eq!(
            out(
                "insert into `t` (`id`, `e`, `n`) values (?, ?, ?) on duplicate key update \
                 `n` = `values`(`n`)"
            ),
            "insert into `t` (`id`, `e`, `n`) values (?, ?, ?) ON CONFLICT DO UPDATE SET \
             `n` = excluded.`n`"
        );
    }

    /// `n = n + VALUES(n)` — the stored column and the proposed one appear in
    /// the same expression, meaning different things, and only the second
    /// occurrence is rewritten.
    #[test]
    fn values_col_is_rewritten_wherever_it_appears_in_the_expression() {
        assert_eq!(
            out("insert into t (id, n) values (?, ?) on duplicate key update n = n + values(n)"),
            "insert into t (id, n) values (?, ?) ON CONFLICT DO UPDATE SET n = n + excluded.n"
        );
    }

    /// Every Laravel `upsert()` call sends a comma-separated assignment list,
    /// one `VALUES(col)` per column, not only the first.
    #[test]
    fn every_assignment_in_the_list_is_rewritten() {
        assert_eq!(
            out(
                "insert into t (id, e, n) values (?, ?, ?) on duplicate key update \
                 e = values(e), n = values(n)"
            ),
            "insert into t (id, e, n) values (?, ?, ?) ON CONFLICT DO UPDATE SET \
             e = excluded.e, n = excluded.n"
        );
    }

    /// A multi-row `VALUES` list — Laravel's `upsert()` sends every proposed
    /// row in one statement — is left exactly as written; only the clause
    /// after `ON DUPLICATE KEY UPDATE` is touched.
    #[test]
    fn a_multi_row_values_list_is_untouched() {
        assert_eq!(
            out(
                "insert into t (id, e, n) values (?, ?, ?), (?, ?, ?) on duplicate key update \
                 n = values(n)"
            ),
            "insert into t (id, e, n) values (?, ?, ?), (?, ?, ?) ON CONFLICT DO UPDATE SET \
             n = excluded.n"
        );
    }

    /// The MySQL 8.0.20+ row-alias form: `AS new` in place of `VALUES(col)`.
    #[test]
    fn a_row_alias_reference_becomes_excluded() {
        assert_eq!(
            out(
                "insert into t (id, n) values (?, ?) as new on duplicate key update \
                 n = new.n"
            ),
            "insert into t (id, n) values (?, ?) ON CONFLICT DO UPDATE SET n = excluded.n"
        );
    }

    /// A row-alias column list needs the real column each alias renames,
    /// which this pass does not resolve — refused, naming the clause.
    #[test]
    fn a_row_alias_column_list_is_refused() {
        let error = refusal(
            "insert into t (id, n) values (?, ?) as new (i, m) on duplicate key update \
             n = new.m",
        );
        assert_eq!(error.code, 1235);
        assert!(error.message.contains("new"), "{}", error.message);
    }

    /// An assignment that never mentions `VALUES(...)` or the row alias at
    /// all — `ON DUPLICATE KEY UPDATE hits = hits + 1` — still has its
    /// keyword rewritten even though nothing inside the assignment changes.
    #[test]
    fn an_assignment_with_no_values_reference_still_gets_the_keyword_rewrite() {
        assert_eq!(
            out("insert into t (id) values (?) on duplicate key update hits = hits + 1"),
            "insert into t (id) values (?) ON CONFLICT DO UPDATE SET hits = hits + 1"
        );
    }

    /// An ordinary `INSERT` with no `ON DUPLICATE KEY UPDATE` at all is
    /// byte-for-byte pass-through, not a `Rewritten` carrying an identical
    /// string — the overwhelmingly common case.
    #[test]
    fn an_insert_with_no_on_duplicate_key_update_is_untouched() {
        let translation =
            translate("insert into t (id, n) values (?, ?)", &empty_catalog()).unwrap();
        assert_eq!(
            translation.statements,
            vec!["insert into t (id, n) values (?, ?)"]
        );
        assert!(translation.dropped.is_empty());
    }

    /// `ON DUPLICATE KEY UPDATE` with nothing after it is a parse error, not
    /// a silently accepted no-op.
    #[test]
    fn an_empty_on_duplicate_key_update_is_a_parse_error() {
        let error = refusal("insert into t (id) values (?) on duplicate key update");
        assert_eq!(error.code, 1064);
    }

    /// The crux verification this change rests on: a table with *more than
    /// one* unique constraint — `users` here has `id` (primary key), a
    /// unique `email` index and a `NOCASE`-collated `name` — gets no catalog
    /// lookup and no ambiguity refusal. The clause is translated onto
    /// SQLite's own targetless `ON CONFLICT DO UPDATE`, which core already
    /// resolves against *any* colliding unique or primary key, exactly like
    /// MySQL's clause does. See the module docs on why this is the exact
    /// mapping, verified against a real `sqlite3` binary and against
    /// `inlaysql-core` directly — and note this translation does not even
    /// read `catalog`'s indexes to reach this answer, unlike
    /// `alter_table`'s: the same output would come back for `email` and
    /// `name` whether or not either is actually unique, because a bare
    /// `ON CONFLICT DO UPDATE` needs no target resolved against anything.
    #[test]
    fn a_table_with_several_unique_constraints_gets_no_target_and_no_refusal() {
        let catalog = catalog_with_users_table(vec![Index {
            unique: true,
            ..Index::single(
                "users_email".to_string(),
                "users".to_string(),
                "email".to_string(),
                IndexKind::BTree,
            )
        }]);

        assert_eq!(
            statements_against(
                "insert into users (id, email, name) values (?, ?, ?) on duplicate key update \
                 email = values(email), name = values(name)",
                &catalog,
            ),
            vec![
                "insert into users (id, email, name) values (?, ?, ?) ON CONFLICT DO UPDATE SET \
                 email = excluded.email, name = excluded.name"
            ]
        );
    }
}
