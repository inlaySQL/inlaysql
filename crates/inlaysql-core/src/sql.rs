//! SQL front end: parse with `sqlparser`, then resolve into a [`Plan`].
//!
//! The dialect is SQLite's, plus two additions that make hybrid retrieval
//! expressible in plain SQL rather than in a bespoke API:
//!
//! * the `VECTOR(n)` column type, and
//! * the retrieval functions `vector_score`, `bm25_score` and `fuse`.
//!
//! Retrieval functions are not evaluated row by row — the planner recognises
//! them, hoists them out of the projection and turns each leaf into an index
//! probe. That is what lets a single ordinary `SELECT` drive an ANN search, a
//! BM25 search and a rank fusion at once.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use sqlparser::ast::{
    Cte, Distinct, DuplicateTreatment, Expr, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, Ident, JoinConstraint, JoinOperator, LimitClause, NamedWindowDefinition,
    NamedWindowExpr, ObjectName, ObjectType, OffsetRows, OrderBy, OrderByExpr, OrderByKind, Query,
    Select, SelectItem as AstSelectItem, SelectItemQualifiedWildcardKind, SetExpr, SetOperator,
    SetQuantifier, Statement, TableAliasColumnDef, TableFactor, TableObject, UnaryOperator,
    Value as AstValue, WindowSpec, WindowType, With,
};
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;

use crate::catalog::{Catalog, Column, IndexKind, Table, TableConstraints, UniqueConstraint};
use crate::collation::Collation;
use crate::error::{Error, Result};
use crate::fusion::DEFAULT_RRF_K;
use crate::hnsw::VectorMetric;
use crate::plan::{
    AggFunc, Aggregate, AlterAction, AlterTablePlan, AnalyzePlan, BinaryOp, CastType,
    CompareAffinity, ConflictAction, ConflictUpdate, CreateIndexPlan, CreateTablePlan,
    CreateUniqueIndexPlan, DeletePlan, DropIndexPlan, DropTablePlan, Expr as PlanExpr, FrameBound,
    FrameUnit, FromItem, InsertPlan, InsertSource, Join, JoinKind, OnConflict, Order, OrderKey,
    Plan, RecursivePlan, ScalarFunc, ScalarItem, ScalarPlan, ScoreExpr, SelectItem, SelectPlan,
    SetOp, SetOperationPlan, Subquery, SubqueryBody, SubqueryOp, UnaryOp, UpdatePlan, WindowFn,
    WindowFrame, WindowFunc,
};
use crate::statement::Statement as Prepared;
use crate::value::{DataType, Value};

/// Default header for an unaliased retrieval expression.
const DEFAULT_SCORE_LABEL: &str = "score";

/// How deeply parentheses may nest in one statement.
///
/// Generous next to anything a person writes, and far below what it takes to
/// exhaust a stack. This constant used to be 64, and "far below" was an
/// assertion rather than a measurement — it was wrong. Measured on a 2 MiB
/// stack, which is what a spawned thread gets and therefore what a server
/// connection and a test both get, planning and evaluating
/// `abs(abs(...abs(1)...))`:
///
/// | build | deepest nesting that survives |
/// | --- | --- |
/// | debug (`cargo test`) | 26 — 28 aborts the process |
/// | release | past 64 |
///
/// So the old limit was safe in release and permitted a stack overflow in
/// every debug build, including the ones CI runs and the ones the fuzzer
/// builds with `-Cdebug-assertions`. 16 keeps a margin against the *debug*
/// cliff, which is the binding one, and leaves more room still for `wasm32`,
/// whose default stack is smaller than a native thread's. Nothing an
/// application writes comes close: an ORM's nested `where` closures reach
/// five or six.
const MAX_NESTING_DEPTH: usize = 16;

/// How many infix operators may chain together without parentheses.
///
/// Parentheses are not the only way to build a deep expression tree. `a || b
/// || c` is left-associative, so a flat chain of *n* operators is an AST *n*
/// levels deep with no parenthesis anywhere in it — invisible to
/// [`MAX_NESTING_DEPTH`], and the planner recurses down that left spine.
/// Measured on a 2 MiB thread stack (what a spawned thread gets, and so what a
/// server connection gets): 1,000 operators plan fine, 2,000 abort the process
/// with a stack overflow.
///
/// SQLite's own limit for this is `SQLITE_MAX_EXPR_DEPTH`, 1,000. Ours is
/// lower because that is the number our planner crashed *near*, not a number
/// it survives with room to spare — the frames here are larger than SQLite's.
/// 512 keeps a 2x margin against the measured cliff and is still far past any
/// chain a person or an ORM writes; an ORM's long `IN (?, ?, ...)` list is
/// commas, which do not chain.
const MAX_CHAIN_LENGTH: usize = 512;

/// Reject a statement whose parentheses nest deeper than [`MAX_NESTING_DEPTH`].
///
/// # Why this is here and not in the parser
///
/// `sqlparser` has its own recursion limit — and we do not get it.
/// `inlaysql-core` depends on it with `default-features = false`, because the
/// default features pull in `std` and this crate is `no_std` on purpose. That
/// also drops `recursive-protection`, and without it `RecursionCounter` is
/// compiled as a stub whose `try_decrease` always succeeds. So the parser
/// recurses without a bound, and four hundred bytes of `(` overflow the stack.
///
/// The coverage-guided fuzzer found exactly that. It matters more than it looks:
/// the MCP server hands arbitrary text from a language model straight to this
/// function, so an unbounded parser is a way to kill the process from the far
/// side of a tool call.
///
/// Turning the feature on would mean giving the core `std`, which is the one
/// thing it may not have — the whole simulation and WASM story rests on it. So
/// the depth is bounded here instead: one linear pass, no allocation, before
/// the parser ever sees the text.
fn check_nesting(sql: &str) -> Result<()> {
    let mut depth = 0usize;
    // Parens inside a literal are text, not structure. Tracking quotes keeps
    // `SELECT '((('` working.
    let mut in_single = false;
    let mut in_double = false;
    // One chain counter per parenthesis depth: operators at depth `d` chain
    // with each other, and a nested `(...)` starts its own chain. Fixed size,
    // because `depth` is already bounded above — no allocation in this pass.
    let mut chain = [0usize; MAX_NESTING_DEPTH + 1];

    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_single || in_double {
            match byte {
                b'\'' if in_single => in_single = false,
                b'"' if in_double => in_double = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match byte {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Err(Error::Unsupported(alloc::format!(
                        "expression nests more than {MAX_NESTING_DEPTH} levels deep"
                    )));
                }
                chain[depth] = 0;
            }
            b')' => depth = depth.saturating_sub(1),
            // A comma ends one expression and starts the next, so the chain
            // restarts. This is what keeps a long `VALUES (..), (..), (..)`
            // or a wide projection from being read as one enormous chain.
            b',' => chain[depth] = 0,
            _ => {}
        }

        // Operator tokens. A two-byte operator (`||`, `<=`, `<>`, `!=`) is one
        // operator, so a symbol immediately following another symbol does not
        // count again — otherwise the effective bound would be half the
        // documented one for exactly the chain shape it exists to catch.
        // Word-shaped operators are matched whole, so an identifier that
        // merely contains one (`in_stock`) is not mistaken for it.
        let is_operator = |b: u8| {
            matches!(
                b,
                b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'|' | b'&' | b'~' | b'!'
            )
        };
        if is_operator(byte) {
            if i == 0 || !is_operator(bytes[i - 1]) {
                chain[depth] += 1;
            }
        } else if byte.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &sql[start..i];
            if is_operator_word(word) {
                chain[depth] += 1;
            }
            if chain[depth] > MAX_CHAIN_LENGTH {
                return Err(Error::Unsupported(alloc::format!(
                    "expression chains more than {MAX_CHAIN_LENGTH} operators"
                )));
            }
            continue;
        }

        if chain[depth] > MAX_CHAIN_LENGTH {
            return Err(Error::Unsupported(alloc::format!(
                "expression chains more than {MAX_CHAIN_LENGTH} operators"
            )));
        }
        i += 1;
    }
    Ok(())
}

/// Whether a bare word is an operator that chains rather than an operand.
fn is_operator_word(word: &str) -> bool {
    // `eq_ignore_ascii_case` rather than lowercasing: this runs before the
    // parser on every statement, and it may not allocate.
    const OPERATORS: &[&str] = &[
        "AND", "OR", "NOT", "IS", "LIKE", "GLOB", "REGEXP", "MATCH", "BETWEEN", "COLLATE",
        "ESCAPE", "IN",
    ];
    OPERATORS
        .iter()
        .any(|candidate| word.eq_ignore_ascii_case(candidate))
}

/// Parse and resolve one SQL statement against `catalog`.
///
/// The result is reusable: `?` placeholders become [`PlanExpr::Param`] slots
/// rather than values, and the statement carries the schema its ordinals were
/// resolved against so that a later execution can check it is still true. This
/// is the only function in the crate that parses.
pub fn prepare(sql: &str, catalog: &Catalog) -> Result<Prepared> {
    check_nesting(sql)?;
    // Before `sqlparser`, because `sqlparser` has no `REINDEX` — its SQLite
    // grammar stops at the statements SQLite's own parser generates a plan
    // for, and `REINDEX` is not one of them. See [`parse_reindex`] for why
    // that is a tokenizer pass rather than a string comparison.
    if let Some(plan) = parse_reindex(sql, catalog)? {
        return Ok(Prepared::new(sql, plan, 0, Vec::new(), catalog));
    }
    let mut statements = Parser::parse_sql(&SQLiteDialect {}, sql)
        .map_err(|e| Error::Parse(alloc::format!("{e}")))?;

    if statements.len() != 1 {
        return Err(Error::Unsupported(alloc::format!(
            "expected exactly one statement, got {}",
            statements.len()
        )));
    }

    let mut binder = Binder::new(catalog);
    let plan = plan_statement(statements.remove(0), catalog, &mut binder)?;
    reject_write_subqueries(&plan)?;

    let vector_params = binder.vector_params();
    Ok(Prepared::new(
        sql,
        plan,
        binder.count,
        vector_params,
        catalog,
    ))
}

/// `REINDEX [name]`, or `None` when the statement is not one.
///
/// # Why this is hand-rolled
///
/// `sqlparser` does not have the statement. Its SQLite grammar covers what
/// SQLite compiles into a query plan, and `REINDEX` is maintenance — the
/// keyword exists in its keyword table only as a `VACUUM` option. So the
/// choice was to add the statement here or to leave the engine with no
/// spelling for "build the deferred indexes now", which is what
/// `crates/inlaysql-core/src/engine.rs` documents as costing the first read
/// after a bulk load the whole build with nothing able to ask for it earlier.
///
/// It is a **tokenizer** pass and not a `starts_with("REINDEX")`, and that is
/// the part worth defending: `/* comment */ REINDEX`, `reindex "my table"`,
/// ``REINDEX `t` ;`` and `REINDEX --trailing` all have to mean what they mean
/// in every other statement, and each one of them is a case a string
/// comparison gets wrong in a different direction. The tokenizer is the same
/// one the parser below would have used, so there is one lexer here, not two.
///
/// Anything that is not `REINDEX` returns `None` untouched — this runs in
/// front of every statement the engine ever parses, so it may not have an
/// opinion about any of them.
fn parse_reindex(sql: &str, catalog: &Catalog) -> Result<Option<Plan>> {
    use sqlparser::keywords::Keyword;
    use sqlparser::tokenizer::{Token, Tokenizer};

    if !could_be_reindex(sql) {
        return Ok(None);
    }
    let tokens = match Tokenizer::new(&SQLiteDialect {}, sql).tokenize() {
        Ok(tokens) => tokens,
        // Not this function's error to report: hand it back and let the real
        // parser fail on it with the message it would have given anyway.
        Err(_) => return Ok(None),
    };
    let mut words = tokens
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)));

    // Quoted, this is an identifier and not the keyword — `"REINDEX"` on its
    // own is a `SELECT`-less nonsense statement, but it is the parser's
    // nonsense, not ours.
    match words.next() {
        Some(Token::Word(word))
            if word.keyword == Keyword::REINDEX && word.quote_style.is_none() => {}
        _ => return Ok(None),
    }

    let mut target: Option<String> = None;
    let mut seen_semicolon = false;
    for token in words {
        match token {
            Token::EOF => break,
            Token::SemiColon if !seen_semicolon => seen_semicolon = true,
            // An identifier, quoted or not. A keyword used as a name arrives
            // as a `Word` too, which is what lets `REINDEX "order"` work.
            Token::Word(word) if target.is_none() && !seen_semicolon => {
                target = Some(word.value);
            }
            Token::DoubleQuotedString(name) if target.is_none() && !seen_semicolon => {
                target = Some(name);
            }
            other => {
                return Err(Error::Parse(alloc::format!(
                    "REINDEX takes one optional table or index name, found `{other}`"
                )))
            }
        }
    }

    Ok(Some(Plan::Reindex(plan_reindex(target, catalog)?)))
}

/// Whether `sql` could be a `REINDEX` at all, decided without tokenizing.
///
/// **This is why [`parse_reindex`] is not a tax on every other statement.**
/// It runs in front of every statement this engine parses, and tokenizing
/// twice to find one keyword would put a second lexer pass on the path of
/// every `SELECT` — `PERF.md` has parsing at roughly half the cost of the
/// point read it precedes. So the leading whitespace and comments are skipped
/// by hand here and the first seven bytes are compared in place: one pass, no
/// allocation, and it stops at the byte that decides.
///
/// Conservative in the direction that matters. A false *positive* costs one
/// tokenize of a statement that turns out not to be a `REINDEX`; a false
/// negative would send a real `REINDEX` to a parser that has never heard of
/// it, so every form of leading trivia SQLite accepts is skipped here.
fn could_be_reindex(sql: &str) -> bool {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        // `--` and `/* */` and nothing else, because SQLite's dialect has
        // nothing else. `#` is MySQL's comment and the engine does not take
        // it — the MySQL shim strips it before the engine ever sees it — so
        // skipping it here would be this function believing in a comment the
        // parser two lines later does not.
        if let Some(after) = rest.strip_prefix("--") {
            rest = match after.find('\n') {
                Some(at) => &after[at + 1..],
                None => "",
            };
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*") {
            rest = match after.find("*/") {
                Some(at) => &after[at + 2..],
                None => "",
            };
            continue;
        }
        break;
    }
    // Bytes, not characters: `REINDEX` is ASCII, so a seven-byte prefix of a
    // valid `str` that matches it cannot have split a multi-byte character.
    rest.as_bytes()
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case(b"REINDEX"))
}

/// Resolve `REINDEX`'s optional name onto the tables to rebuild.
///
/// SQLite resolves the name as a collation, then a table, then an index. This
/// engine has no `REINDEX`-able collation — a collation here is a property of
/// a column, and changing one is `ALTER TABLE`, which rebuilds the indexes
/// itself — so the order is table, then index, then a refusal that says so.
/// Guessing "everything" from a name that matched nothing would turn a typo
/// into a full-database rebuild.
fn plan_reindex(target: Option<String>, catalog: &Catalog) -> Result<crate::plan::ReindexPlan> {
    let Some(name) = target else {
        return Ok(crate::plan::ReindexPlan {
            tables: catalog
                .tables()
                .map(|table| table.name.to_ascii_lowercase())
                .collect(),
            index: None,
        });
    };
    if let Some(table) = catalog.table(&name) {
        return Ok(crate::plan::ReindexPlan {
            tables: alloc::vec![table.name.to_ascii_lowercase()],
            index: None,
        });
    }
    if let Some(index) = catalog
        .indexes()
        .find(|index| index.name.eq_ignore_ascii_case(&name))
    {
        return Ok(crate::plan::ReindexPlan {
            tables: alloc::vec![index.table.to_ascii_lowercase()],
            index: Some(index.name.clone()),
        });
    }
    Err(Error::Catalog(alloc::format!(
        "unable to identify the object to be reindexed: `{name}` is neither a table nor an index"
    )))
}

/// Resolve one parsed statement into a [`Plan`].
///
/// Separate from [`prepare`] because `EXPLAIN <statement>` has to resolve the
/// statement inside it through exactly this match — an `EXPLAIN` that
/// described a statement the engine would refuse, or refused one it accepts,
/// would be a second front end.
fn plan_statement(statement: Statement, catalog: &Catalog, binder: &mut Binder) -> Result<Plan> {
    match statement {
        Statement::Analyze(analyze) => plan_analyze(analyze, catalog),
        Statement::CreateTable(create) => plan_create_table(create, catalog, binder),
        Statement::CreateIndex(create) => plan_create_index(create, catalog),
        Statement::AlterTable(alter) => plan_alter_table(alter, catalog),
        Statement::Drop {
            object_type: ObjectType::Index,
            names,
            ..
        } => plan_drop_index(names),
        Statement::Drop {
            object_type: ObjectType::Table,
            names,
            if_exists,
            cascade,
            restrict,
            purge,
            temporary,
            table,
        } => {
            if cascade || restrict || purge || temporary || table.is_some() {
                return Err(Error::Unsupported(
                    "CASCADE, RESTRICT, PURGE and TEMPORARY are not in SQLite's DROP TABLE"
                        .to_string(),
                ));
            }
            plan_drop_table(names, if_exists)
        }
        Statement::Insert(insert) => plan_insert(insert, catalog, binder),
        Statement::Query(query) => plan_select(*query, catalog, binder),
        Statement::Update(update) => plan_update(update, catalog, binder),
        Statement::Delete(delete) => plan_delete(delete, catalog, binder),
        Statement::Explain {
            describe_alias,
            analyze,
            verbose,
            query_plan,
            estimate,
            statement,
            format,
            options,
        } => plan_explain(
            ExplainModifiers {
                describe_alias,
                analyze,
                verbose,
                query_plan,
                estimate,
                format,
                options,
            },
            *statement,
            catalog,
            binder,
        ),
        // MySQL's `EXPLAIN <table>` / `DESCRIBE <table>`, which is a column
        // listing rather than a plan. Named rather than swept into the
        // catch-all below, because the two spellings are one keyword apart
        // and the message has to say which one this engine has.
        Statement::ExplainTable { .. } => Err(Error::Unsupported(
            "EXPLAIN <table> is not supported; EXPLAIN <statement> reports a query plan"
                .to_string(),
        )),
        statement @ (Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. }) => plan_transaction(&statement),
        other => Err(Error::Unsupported(alloc::format!(
            "statement not supported in this stage: {other}"
        ))),
    }
}

/// Everything written between `EXPLAIN` and the statement it describes.
///
/// Grouped so [`plan_explain`] refuses them one by one and by name: each is a
/// different feature wearing the same keyword, and accepting one silently
/// would mean answering a question nobody asked.
struct ExplainModifiers {
    describe_alias: sqlparser::ast::DescribeAlias,
    analyze: bool,
    verbose: bool,
    query_plan: bool,
    estimate: bool,
    format: Option<sqlparser::ast::AnalyzeFormatKind>,
    options: Option<Vec<sqlparser::ast::UtilityOption>>,
}

/// `EXPLAIN <statement>`: plan the statement, and wrap the plan rather than
/// running it.
///
/// # What `EXPLAIN` means here
///
/// sqlite3's bare `EXPLAIN` dumps VDBE bytecode and its `EXPLAIN QUERY PLAN`
/// reports the access path. There is no bytecode in this engine — the
/// executor walks a [`Plan`] directly — so the first of those is not merely
/// unimplemented, it has nothing to describe. `EXPLAIN <statement>` therefore
/// means the query plan, which is also what MySQL's `EXPLAIN` means and what
/// anyone typing it into a MySQL client expects. `EXPLAIN QUERY PLAN` is
/// accepted as sqlite3's spelling of the same request rather than as a
/// second, different one.
///
/// # What it refuses
///
/// * `EXPLAIN ANALYZE` runs the statement and reports what actually happened.
///   This never runs the statement — that is the whole point of it being safe
///   to `EXPLAIN` a `DELETE` — so accepting the keyword and reporting a plan
///   would answer a different question than the one asked.
/// * `VERBOSE`, `ESTIMATE`, `FORMAT ...` and Postgres's parenthesised options
///   each select an output this engine does not produce. Reporting the
///   ordinary plan under them would be the same silent substitution.
/// * DDL and transaction control have no access path to report. sqlite3
///   answers those with an empty `EXPLAIN QUERY PLAN`, which reads as "this
///   query does nothing"; naming what cannot be described is the honest
///   version, and is this repository's rule everywhere else.
fn plan_explain(
    modifiers: ExplainModifiers,
    statement: Statement,
    catalog: &Catalog,
    binder: &mut Binder,
) -> Result<Plan> {
    let alias = modifiers.describe_alias;
    for (present, what) in [
        (modifiers.analyze, "ANALYZE"),
        (modifiers.verbose, "VERBOSE"),
        (modifiers.estimate, "ESTIMATE"),
        (modifiers.format.is_some(), "FORMAT"),
        (modifiers.options.is_some(), "(...)"),
    ] {
        if present {
            return Err(Error::Unsupported(alloc::format!(
                "{alias} {what} is not supported; {alias} <statement> reports the plan the \
                 executor would choose and never runs the statement"
            )));
        }
    }
    // `query_plan` is sqlite3's `EXPLAIN QUERY PLAN`, and is deliberately not
    // in the refusal list above: it asks for exactly what this produces.
    let _ = modifiers.query_plan;

    let inner = plan_statement(statement, catalog, binder)?;
    match &inner {
        Plan::Select(_)
        | Plan::Scalar(_)
        | Plan::SetOperation(_)
        | Plan::Insert(_)
        | Plan::Update(_)
        | Plan::Delete(_) => Ok(Plan::Explain(Box::new(inner))),
        Plan::CreateTable(_)
        | Plan::DropTable(_)
        | Plan::AlterTable(_)
        | Plan::CreateIndex(_)
        | Plan::CreateUniqueIndex(_)
        | Plan::DropIndex(_)
        | Plan::Reindex(_)
        | Plan::Analyze(_)
        | Plan::Begin
        | Plan::Commit
        | Plan::Rollback
        | Plan::Savepoint(_)
        | Plan::ReleaseSavepoint(_)
        | Plan::RollbackToSavepoint(_)
        | Plan::Explain(_) => Err(Error::Unsupported(alloc::format!(
            "{alias} describes a query plan; this statement has none"
        ))),
    }
}

/// Parse and resolve one statement, checking it against `params`.
///
/// A convenience for callers that only want the plan for a single execution —
/// tooling, tests and fuzz targets. Anything that runs the same statement more
/// than once should keep the [`Prepared`] from [`prepare`] instead.
pub fn plan(sql: &str, params: &[Value], catalog: &Catalog) -> Result<Plan> {
    let prepared = prepare(sql, catalog)?;
    prepared.check_parameters(params)?;
    Ok(prepared.plan().clone())
}

/// One CTE from a `WITH` clause, already planned — kept on [`Binder::ctes`]
/// for the rest of the statement, and cloned into a [`FromItem`] at every
/// reference site. A CTE referenced twice is therefore planned once but may
/// run twice, once per clone — SQLite itself sometimes does the same (it has
/// no rule promising a CTE is materialised); see `plan_ctes`.
struct CteEntry {
    /// As written; matched case-insensitively, same as a table name.
    name: String,
    /// The synthetic table a reference to this CTE presents to the rest of
    /// the planner — built the same way [`derived_table`] builds one for
    /// `FROM (SELECT ...)`, with the CTE's own name rather than an alias.
    table: Table,
    /// The planned query, cloned into each [`FromItem::derived`] that
    /// references this CTE.
    body: SubqueryBody,
}

/// Numbers the `?` placeholders in the order they are encountered.
///
/// Resolution walks the statement in textual order (projection, then `WHERE`,
/// then `LIMIT`), so the numbering matches what the caller wrote. Aggregate
/// functions are collected here too, in the same order, so [`PlanExpr::Agg`]
/// references resolve against the plan's aggregate list.
struct Binder<'c> {
    /// The catalog a subquery's `FROM` is resolved against.
    ///
    /// It rides on the binder because a subquery is reached through
    /// [`resolve_expr`], which is threaded with the binder and not with the
    /// catalog — and adding a catalog argument to every expression helper
    /// instead would say nothing about why it was there.
    catalog: &'c Catalog,
    count: usize,
    /// Aggregate functions encountered, in resolution order.
    ///
    /// One query level's worth: [`plan_subquery`] swaps this out around the
    /// inner query, because an aggregate written inside a subquery belongs to
    /// that subquery's plan and would otherwise turn the outer `SELECT` into an
    /// aggregate one. Placeholders are *not* swapped — `?` is numbered across
    /// the whole statement, subqueries included, which is what the caller
    /// counts when it binds.
    aggregates: Vec<Aggregate>,
    /// Window functions encountered, in resolution order.
    ///
    /// One query level's worth, swapped around a subquery/CTE/compound arm
    /// exactly as [`Binder::aggregates`] is and for the same reason — a
    /// window function written inside one belongs to *its* plan. Unlike
    /// `aggregates`, a non-empty `windows` never turns the query into an
    /// aggregate one: see [`WindowFn`]'s doc for why the two are evaluated at
    /// different points in the pipeline.
    windows: Vec<WindowFn>,
    /// Named windows (`WINDOW w AS (...)`) declared by the `SELECT` currently
    /// being resolved, in declaration order — [`plan_select_arm`] sets this
    /// fresh at entry and restores whatever was there before at exit, so a
    /// name is visible only to the query level that declared it, the same
    /// scoping [`Binder::aggregates`] and [`Binder::windows`] have (a
    /// subquery's own `WINDOW` clause never leaks out, and an outer one never
    /// leaks in).
    named_windows: Vec<(String, NamedWindow)>,
    /// Subqueries resolved so far, which is where [`Subquery::id`] comes from.
    subqueries: usize,
    /// One capture list per subquery level currently being resolved, innermost
    /// last. Resolution is strictly nested, so a stack is enough.
    captures: Vec<Vec<PlanExpr>>,
    /// Whether a subquery may appear at all.
    ///
    /// False for a stored `DEFAULT` or `CHECK` expression, which SQLite does
    /// not allow one in either. Refusing at the point the subquery is reached
    /// gives that reason, where letting it resolve against the empty catalog a
    /// stored expression is parsed with would report "no such table" and say
    /// nothing about why.
    subqueries_allowed: bool,
    /// One frame per enclosing `WITH` clause, innermost last: the CTEs it
    /// declares that have been planned so far, in list order.
    ///
    /// A CTE is visible for the rest of the statement it was written in —
    /// every arm of a compound, every subquery, every derived table however
    /// deep — not merely the `FROM` clause it sits beside, which is why this
    /// lives on the binder rather than on [`Scope`] (whose `parent` chain
    /// stops at a derived table's boundary on purpose; see
    /// [`push_source`]'s `TableFactor::Derived` arm). See [`plan_ctes`].
    ctes: Vec<Vec<CteEntry>>,
    /// One frame per enclosing `WITH` clause, aligned with [`Binder::ctes`]:
    /// every name that clause declares, whether or not it has been planned
    /// yet.
    ///
    /// Checked before a name is allowed to fall back to the catalog, so a
    /// self- or forward-reference among sibling CTEs is refused rather than
    /// silently resolving to a same-named real table — confirmed against
    /// sqlite3 that this matters: `WITH t AS (SELECT a FROM t) SELECT a FROM
    /// t` is a circular-reference error there even when a real table `t`
    /// exists, not a scan of it. See [`Binder::resolve_cte`].
    cte_reserved: Vec<Vec<String>>,
    /// Per placeholder, the embedding dimension the statement's *shape* pins
    /// it to, or `None` where it pins none.
    ///
    /// A `VECTOR` value has no type of its own on the MySQL wire — MySQL only
    /// grew one in 9.0 and no driver in the field sends its code — so a bound
    /// embedding arrives as an untyped string of bytes that is
    /// indistinguishable from a `TEXT` or `BLOB` parameter carrying the same
    /// bytes. Without this the wire server had no way to tell them apart and
    /// every embedding had to be inlined into the SQL as decimal text, which
    /// cost 3.22x the corpus in wire bytes. The statement already knows the
    /// answer at plan time — the target column's width, or the width of the
    /// column `vector_score()` is scoring against — so it is recorded here
    /// rather than guessed from the bytes, where a text embedding whose length
    /// happened to be `4 * dim` would decode as garbage floats.
    vector_params: Vec<Option<usize>>,
    /// The name and synthetic table of the `WITH RECURSIVE` CTE whose
    /// recursive term is being resolved right now, if any — what a bare
    /// `FROM name`/`JOIN name` inside it resolves to instead of erroring on
    /// [`Binder::resolve_cte`]'s ordinary "not yet defined" check. See
    /// `try_plan_recursive_cte`.
    ///
    /// Not a stack: saved and restored around each recursive term the same
    /// way [`Binder::aggregates`] is, so a recursive CTE nested inside
    /// another's recursive term resolves its *own* name here rather than the
    /// outer one's.
    recursive_self: Option<(String, Table)>,
    /// Whether [`Binder::recursive_self`] has been resolved to yet, while it
    /// is set. A second occurrence — a repeated `FROM`/`JOIN`, or one nested
    /// in a subquery — is refused: SQLite allows exactly one, and the
    /// semi-naive loop this plans for has no meaning for a second reference
    /// with its own, different frontier at the same step.
    recursive_self_used: bool,
}

impl<'c> Binder<'c> {
    /// A binder for one statement, resolving against `catalog`.
    fn new(catalog: &'c Catalog) -> Self {
        Self {
            catalog,
            count: 0,
            aggregates: Vec::new(),
            windows: Vec::new(),
            named_windows: Vec::new(),
            subqueries: 0,
            captures: Vec::new(),
            subqueries_allowed: true,
            ctes: Vec::new(),
            cte_reserved: Vec::new(),
            vector_params: Vec::new(),
            recursive_self: None,
            recursive_self_used: false,
        }
    }

    /// The synthetic table and planned body a CTE named `name` resolves to,
    /// searching enclosing `WITH` clauses innermost first.
    ///
    /// `Ok(None)` means no `WITH` clause in scope names it at all, so the
    /// caller falls back to the catalog. `Err` means a `WITH` clause *does*
    /// name it but this engine cannot yet resolve the reference — see
    /// [`Binder::cte_reserved`]'s doc for why that is a deliberate, narrower
    /// rule than SQLite's own (SQLite allows a non-circular forward
    /// reference between siblings; this planner resolves a `WITH` list
    /// strictly in written order and refuses one instead of adding lazy,
    /// topological resolution for a shape no ORM emits).
    fn resolve_cte(&self, name: &str) -> Result<Option<(Table, SubqueryBody)>> {
        for (resolved, reserved) in self.ctes.iter().zip(self.cte_reserved.iter()).rev() {
            if let Some(entry) = resolved.iter().find(|e| e.name.eq_ignore_ascii_case(name)) {
                return Ok(Some((entry.table.clone(), entry.body.clone())));
            }
            if reserved.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                return Err(Error::Unsupported(alloc::format!(
                    "`{name}` references a CTE not yet defined at this point in its WITH list \
                     (a self- or forward-reference); SQLite allows this and resolves it lazily, \
                     but this engine plans a WITH list strictly in order and does not"
                )));
            }
        }
        Ok(None)
    }

    /// Claim the next placeholder slot.
    fn take(&mut self) -> PlanExpr {
        let index = self.count;
        self.count += 1;
        PlanExpr::Param(index)
    }

    /// Record that `expr`, if it is a bare placeholder, must carry an embedding
    /// of `dim` components. See [`Binder::vector_params`].
    ///
    /// A no-op for anything else, which is deliberate: only a bare `?` names a
    /// slot a caller can bind. `-?` over a vector column is not an embedding
    /// with a sign, it is nonsense, and it keeps the type error it already had
    /// rather than being decoded as one.
    fn pin_vector_param(&mut self, expr: &PlanExpr, dim: usize) {
        let PlanExpr::Param(index) = expr else {
            return;
        };
        if self.vector_params.len() <= *index {
            self.vector_params.resize(*index + 1, None);
        }
        self.vector_params[*index] = Some(dim);
    }

    /// One entry per placeholder, in `?` order — see [`Binder::vector_params`].
    fn vector_params(&self) -> Vec<Option<usize>> {
        let mut pinned = self.vector_params.clone();
        pinned.resize(self.count, None);
        pinned
    }

    /// Register an outer reference made by the query at `depth`, returning the
    /// slot [`PlanExpr::Outer`] should name.
    ///
    /// Equal captures fold into one, so `WHERE o.x = i.a AND o.x < i.b` carries
    /// `o.x` down once rather than twice.
    fn capture(&mut self, depth: usize, expr: PlanExpr) -> usize {
        let list = &mut self.captures[depth - 1];
        match list.iter().position(|existing| *existing == expr) {
            Some(index) => index,
            None => {
                list.push(expr);
                list.len() - 1
            }
        }
    }
}

// ------------------------------------------------------------------- ANALYZE

/// Resolve SQLite's `ANALYZE` maintenance statement.
///
/// The first statistics prototype performs a deterministic full scan of one
/// or all tables. Other dialects expose sampling, partitions, column lists and
/// metadata-only variants through the same keyword; accepting those fields and
/// silently doing a full scan would answer a different request, so each is
/// refused here.
fn plan_analyze(analyze: sqlparser::ast::Analyze, catalog: &Catalog) -> Result<Plan> {
    if analyze.partitions.is_some()
        || analyze.for_columns
        || !analyze.columns.is_empty()
        || analyze.cache_metadata
        || analyze.noscan
        || analyze.compute_statistics
    {
        return Err(Error::Unsupported(
            "ANALYZE options are not supported; use ANALYZE [TABLE] <table> or bare ANALYZE"
                .to_string(),
        ));
    }

    let tables = match analyze.table_name {
        Some(name) => {
            let name = object_name(&name)?;
            catalog.require_table(&name)?;
            alloc::vec![name.to_ascii_lowercase()]
        }
        None => catalog
            .tables()
            .map(|table| table.name.to_ascii_lowercase())
            .collect(),
    };
    Ok(Plan::Analyze(AnalyzePlan { tables }))
}

// ---------------------------------------------------------------- CREATE TABLE

fn plan_create_table(
    create: sqlparser::ast::CreateTable,
    catalog: &Catalog,
    binder: &mut Binder,
) -> Result<Plan> {
    use sqlparser::ast::ColumnOption as Opt;

    reject_unsupported_create_table(&create)?;

    if let Some(query) = create.query.as_deref() {
        return plan_create_table_as_select(&create, query, catalog, binder);
    }

    // Stored expressions name no table, so they resolve against nothing.
    let empty = Catalog::new();
    let name = object_name(&create.name)?;
    let mut columns: Vec<Column> = Vec::with_capacity(create.columns.len());
    let mut constraints = TableConstraints::default();
    // A `PRIMARY KEY (...)` written as a table constraint, resolved once every
    // column exists.
    let mut primary_key: Option<Vec<String>> = None;

    for column in &create.columns {
        let ty = if create.strict {
            resolve_strict_data_type(&column.data_type, &column.name.value)?
        } else {
            resolve_data_type(&column.data_type)?
        };
        let mut resolved = Column::new(&column.name.value, ty);
        let mut declares_primary_key = false;
        let mut autoincrement = false;

        for option in &column.options {
            match &option.option {
                // An explicit `NULL` asks for exactly what every column
                // already is.
                Opt::Null => {}
                Opt::NotNull => resolved.not_null = true,
                Opt::Default(expr) => {
                    // Rendered rather than resolved: the catalog is a durable
                    // format, so it stores what was written. Checking it now
                    // means a nonsense default fails at `CREATE TABLE` rather
                    // than at the first `INSERT` that omits the column.
                    resolve_expr(expr, &Scope::empty(), &mut stored_binder(&empty))?;
                    resolved.default = Some(expr.to_string());
                }
                Opt::PrimaryKey(key) => {
                    reject_index_options(
                        &column.name.value,
                        &key.index_type,
                        &key.index_options,
                        &key.characteristics,
                    )?;
                    declares_primary_key = true;
                }
                Opt::Unique(unique) => {
                    reject_index_options(
                        &column.name.value,
                        &unique.index_type,
                        &unique.index_options,
                        &unique.characteristics,
                    )?;
                    constraints
                        .unique
                        .push(UniqueConstraint::new(alloc::vec![resolved.name.clone()]));
                }
                // SQLite attaches `COLLATE` to any column, whatever its
                // declared type, and consults it only for `TEXT` comparisons.
                // Accepting it on an `INTEGER` column is therefore not a
                // silently-dropped clause: it is recorded, and it is asked
                // exactly where SQLite asks it.
                Opt::Collation(name) => {
                    resolved.collation = Collation::from_name(&object_name(name)?)?;
                }
                Opt::Check(check) => constraints.checks.push(check.expr.to_string()),
                Opt::ForeignKey(key) => constraints
                    .foreign_keys
                    .push(foreign_key(key, Some(&resolved.name))?),
                // sqlparser hands SQLite's `AUTOINCREMENT` back as a raw token
                // rather than as an option of its own.
                Opt::DialectSpecific(tokens) => match tokens.as_slice() {
                    [token] if is_identifier(token, "AUTOINCREMENT") => autoincrement = true,
                    _ => {
                        return Err(Error::Unsupported(alloc::format!(
                            "column option `{}` on `{}` is not supported",
                            option.option,
                            column.name.value
                        )))
                    }
                },
                other => {
                    return Err(Error::Unsupported(alloc::format!(
                        "column option `{other}` on `{}` is not implemented yet",
                        column.name.value
                    )))
                }
            }
        }

        // `AUTOINCREMENT` asks for a key that is never reused, and this engine
        // has no way to do anything else: the row-id counter is monotonic and
        // persisted, so deleting the highest row does not hand its key back
        // out. Accepting it is therefore a statement of fact rather than a
        // silently-ignored clause. SQLite's own restriction applies —
        // `AUTOINCREMENT` is only meaningful on the column that *is* the row
        // id — and is enforced here rather than dropped.
        if autoincrement && !(declares_primary_key && resolved.ty == DataType::Integer) {
            return Err(Error::Unsupported(alloc::format!(
                "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY; `{}` is not one",
                column.name.value
            )));
        }
        // Confirmed against sqlite3: `AUTOINCREMENT` on a `WITHOUT ROWID`
        // table is refused outright, not merely ineffective — there is no
        // row id counter on such a table for it to advance.
        if autoincrement && create.without_rowid {
            return Err(Error::Unsupported(
                "AUTOINCREMENT is not allowed on a WITHOUT ROWID table".to_string(),
            ));
        }

        if declares_primary_key {
            if primary_key.is_some() {
                return Err(Error::Catalog(alloc::format!(
                    "table `{name}` declares more than one PRIMARY KEY"
                )));
            }
            primary_key = Some(alloc::vec![resolved.name.clone()]);
        }
        columns.push(resolved);
    }

    for constraint in &create.constraints {
        table_constraint(
            &name,
            constraint,
            &columns,
            &mut constraints,
            &mut primary_key,
        )?;
    }

    if columns.is_empty() {
        return Err(Error::Catalog(alloc::format!(
            "table `{name}` must declare at least one column"
        )));
    }

    // A `WITHOUT ROWID` table's `PRIMARY KEY` is not a unique index over the
    // row: it *is* the row's storage location. Confirmed against sqlite3:
    // `PRIMARY KEY missing on table t` refuses one with none at all — unlike
    // an ordinary table, where every column defaults to a hidden row id —
    // and even a lone `INTEGER PRIMARY KEY` does *not* become a row id
    // alias here, unlike on an ordinary table (`Table::without_rowid`'s doc
    // has the measurement); its columns are also implicitly `NOT NULL`, the
    // same rule an ordinary table's row id itself is never absent for.
    if create.without_rowid {
        let key = primary_key.ok_or_else(|| {
            Error::Catalog(alloc::format!("PRIMARY KEY missing on table `{name}`"))
        })?;
        // Same disclosed gap `plan_create_index` refuses by name: a `UNIQUE`
        // constraint gets a secondary index backing it, and a secondary
        // index needs a row id to point back with — the primary key's own
        // columns do not need one (they *are* the storage key), but any
        // other `UNIQUE` here would.
        if let Some(group) = constraints.unique.first() {
            return Err(Error::Unsupported(alloc::format!(
                "UNIQUE on a WITHOUT ROWID table's column `{}` is not supported yet, for the \
                 same reason CREATE INDEX on one is not: it would need a secondary index \
                 pointing back to the row by row id, and this table has none",
                group.columns.join(", ")
            )));
        }
        for column_name in &key {
            let (ordinal, _) = require_column(&name, &columns, column_name)?;
            columns[ordinal].not_null = true;
        }
        let table = Table {
            name,
            columns,
            strict: create.strict,
            without_rowid: true,
            primary_key: key,
            temporary: create.temporary,
        };
        for group in &constraints.unique {
            for column in &group.columns {
                table.require_column(column)?;
            }
        }
        for key in &constraints.foreign_keys {
            for column in &key.columns {
                table.require_column(column)?;
            }
        }
        return Ok(Plan::CreateTable(CreateTablePlan {
            table,
            constraints,
            if_not_exists: create.if_not_exists,
            as_select: None,
        }));
    }

    // SQLite's rule, and it is worth stating because it decides the storage
    // layout: a *single* `INTEGER PRIMARY KEY` column is not an index at all,
    // it is an alias for the row id. Every other primary key — composite, or
    // on any other affinity — is a unique index and nothing more, which is why
    // it becomes an ordinary `UNIQUE` constraint here rather than an error.
    if let Some(key) = primary_key {
        let rowid_alias = match key.as_slice() {
            [only] => {
                let (ordinal, column) = require_column(&name, &columns, only)?;
                (column.ty == DataType::Integer).then_some(ordinal)
            }
            _ => None,
        };
        match rowid_alias {
            Some(ordinal) => columns[ordinal].primary_key = true,
            None => constraints.unique.push(UniqueConstraint::new(key)),
        }
    }

    // Same disclosed gap `plan_create_index` refuses by name: a `UNIQUE`
    // constraint (including a composite or non-integer `PRIMARY KEY`, folded
    // into one above) gets a secondary index backing it, and the storage
    // router that gives a temporary table its own rows (`temp_storage`) has
    // no way to route a scalar-index entry by table — its key carries only
    // the index's own name, never the table's. A lone `INTEGER PRIMARY KEY`
    // is unaffected: it is the row id itself, not an index.
    if create.temporary {
        if let Some(group) = constraints.unique.first() {
            return Err(Error::Unsupported(alloc::format!(
                "UNIQUE on a temporary table's column `{}` is not supported yet, for the same \
                 reason CREATE INDEX on one is not: it would need a secondary index, and this \
                 storage router has no table to point one at",
                group.columns.join(", ")
            )));
        }
    }

    let table = Table {
        without_rowid: false,
        primary_key: Vec::new(),
        name,
        columns,
        strict: create.strict,
        temporary: create.temporary,
    };
    for group in &constraints.unique {
        for column in &group.columns {
            table.require_column(column)?;
        }
    }
    for key in &constraints.foreign_keys {
        for column in &key.columns {
            table.require_column(column)?;
        }
    }

    Ok(Plan::CreateTable(CreateTablePlan {
        table,
        constraints,
        if_not_exists: create.if_not_exists,
        as_select: None,
    }))
}

/// `CREATE TABLE ... AS SELECT`.
///
/// Verified against a real sqlite3 binary. The new table's columns take
/// their *name* from the select list — an explicit alias, else a bare
/// column's own name, else the expression's rendered source text, the same
/// rule [`resolve_returning`] already applies — via [`SubqueryBody::labels`],
/// which already replicates SQLite's one surprising case: a compound
/// query's names come from its left arm alone. Their *type* is the source
/// column's declared type, but only where the item is a bare reference to a
/// stored column; every other column — an expression, a compound query's
/// arm, a `SELECT` with no `FROM` — gets no declared type in SQLite at all.
/// No constraint, default, `COLLATE` or primary key survives from a source
/// column either, and neither does SQLite's: `CREATE TABLE t AS SELECT id
/// FROM src` does not make `t.id` a rowid alias however `src` declared it.
///
/// This engine's catalog has no representation for SQLite's genuinely
/// type-less column — every stored column already had one of the affinities
/// in [`DataType`] before this feature existed, and adding one now would be
/// a catalog format change out of proportion to composing two statements
/// that already work. `DataType::Numeric` is used instead: unlike
/// `DataType::Blob`, which `sql::coerce` accepts only actual blob bytes for
/// and would reject the integer or text an ordinary expression evaluates
/// to, `Numeric` passes an integer, blob or vector through unchanged and
/// only reshapes a real (to an integer, when exact) or a numeric-looking
/// text — the narrow, disclosed difference from SQLite's true no-op.
///
/// A compound query's own column-unification affinity rule — SQLite can
/// still type a compound's column from an arm that is itself an expression
/// — is not replicated. Treating every compound column as having no source
/// column is strictly safe, only sometimes narrower than SQLite's answer,
/// never wrong.
fn plan_create_table_as_select(
    create: &sqlparser::ast::CreateTable,
    query: &Query,
    catalog: &Catalog,
    binder: &mut Binder,
) -> Result<Plan> {
    // Neither is reachable through SQLite's own grammar — a column or
    // constraint list and `AS SELECT` do not parse together — but a more
    // permissive dialect in `sqlparser` might one day hand both back, and
    // silently ignoring either would be exactly the dropped-clause bug this
    // file exists to refuse.
    if !create.columns.is_empty() || !create.constraints.is_empty() || create.strict {
        return Err(Error::Unsupported(
            "CREATE TABLE ... AS SELECT does not take an explicit column or constraint list, \
             or STRICT"
                .to_string(),
        ));
    }

    let name = object_name(&create.name)?;
    let body = plan_query_body(query, catalog, binder, None)?;
    let labels: Vec<String> = body.labels().into_iter().map(str::to_string).collect();
    let types: Vec<Option<DataType>> = match &body {
        SubqueryBody::Select(plan) => plan.output_columns().into_iter().map(|c| c.ty).collect(),
        SubqueryBody::Scalar(_) | SubqueryBody::SetOp(_) | SubqueryBody::Recursive(_) => {
            alloc::vec![None; labels.len()]
        }
        SubqueryBody::RecursiveSelf(_) => unreachable!(
            "a recursive CTE's self-reference only ever appears inside a FromItem, never as a \
             top-level query body"
        ),
    };

    let mut columns: Vec<Column> = Vec::with_capacity(labels.len());
    for (label, ty) in labels.into_iter().zip(types) {
        if columns
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&label))
        {
            return Err(Error::Unsupported(alloc::format!(
                "CREATE TABLE ... AS SELECT does not support two columns both named `{label}`; \
                 alias one of them"
            )));
        }
        columns.push(Column::new(&label, ty.unwrap_or(DataType::Numeric)));
    }
    if columns.is_empty() {
        return Err(Error::Catalog(alloc::format!(
            "table `{name}` must declare at least one column"
        )));
    }

    Ok(Plan::CreateTable(CreateTablePlan {
        table: Table {
            without_rowid: false,
            primary_key: Vec::new(),
            name,
            columns,
            strict: false,
            temporary: create.temporary,
        },
        constraints: TableConstraints::default(),
        if_not_exists: create.if_not_exists,
        as_select: Some(Box::new(body)),
    }))
}

/// Fold one table-level constraint into the table being built.
fn table_constraint(
    table: &str,
    constraint: &sqlparser::ast::TableConstraint,
    columns: &[Column],
    constraints: &mut TableConstraints,
    primary_key: &mut Option<Vec<String>>,
) -> Result<()> {
    use sqlparser::ast::TableConstraint as Tc;

    match constraint {
        Tc::PrimaryKey(key) => {
            reject_index_options(
                table,
                &key.index_type,
                &key.index_options,
                &key.characteristics,
            )?;
            if primary_key.is_some() {
                return Err(Error::Catalog(alloc::format!(
                    "table `{table}` declares more than one PRIMARY KEY"
                )));
            }
            *primary_key = Some(index_columns(&key.columns)?);
        }
        Tc::Unique(unique) => {
            reject_index_options(
                table,
                &unique.index_type,
                &unique.index_options,
                &unique.characteristics,
            )?;
            if unique.nulls_distinct != sqlparser::ast::NullsDistinctOption::None {
                return Err(Error::Unsupported(
                    "UNIQUE ... NULLS [NOT] DISTINCT is a PostgreSQL extension and is not \
                     supported; SQLite treats every NULL as distinct"
                        .to_string(),
                ));
            }
            constraints
                .unique
                .push(UniqueConstraint::new(index_columns(&unique.columns)?));
        }
        Tc::Check(check) => constraints.checks.push(check.expr.to_string()),
        Tc::ForeignKey(key) => constraints.foreign_keys.push(foreign_key(key, None)?),
        other => {
            let _ = columns;
            return Err(Error::Unsupported(alloc::format!(
                "table constraint `{other}` is not supported"
            )));
        }
    }
    Ok(())
}

/// Read a constraint's column list, refusing the expression and prefix forms
/// SQLite's `CREATE TABLE` does not have.
fn index_columns(columns: &[sqlparser::ast::IndexColumn]) -> Result<Vec<String>> {
    let collated = collated_index_columns(columns)?;
    if let Some(column) = collated.iter().find(|column| column.collation.is_some()) {
        return Err(Error::Unsupported(alloc::format!(
            "COLLATE on `{}` inside a UNIQUE or PRIMARY KEY column list is not \
             supported; declare the collation on the column instead, so the constraint and \
             the column agree about what a duplicate is",
            column.name
        )));
    }
    // An operator class picks how an index compares. A `UNIQUE` or
    // `PRIMARY KEY` constraint is not an index here — it is a statement about
    // duplicates, decided by the columns' own collations — so there is nothing
    // for one to apply to, and accepting it would be the silently-dropped
    // clause this module refuses to have.
    if let Some(column) = collated
        .iter()
        .find(|column| column.operator_class.is_some())
    {
        return Err(Error::Unsupported(alloc::format!(
            "an operator class on `{}` inside a UNIQUE or PRIMARY KEY column list has \
             nothing to apply to; it selects how an index compares, and a constraint \
             compares by the column's own collation",
            column.name
        )));
    }
    Ok(collated.into_iter().map(|column| column.name).collect())
}

/// One entry of an index or constraint column list.
struct IndexColumnSpec {
    /// The column name, as written.
    name: String,
    /// The `COLLATE` written beside it, if there was one.
    collation: Option<Collation>,
    /// The operator class written after it, if there was one — pgvector's
    /// `vector_l2_ops` and friends. Resolved (or refused) by whoever knows
    /// what kind of index is being declared; see [`plan_create_index`].
    operator_class: Option<String>,
}

/// The columns of an index or constraint list, each with the `COLLATE` and the
/// operator class written beside it.
///
/// `CREATE INDEX i ON t (name COLLATE NOCASE)` is real SQLite and is the only
/// way to key an index under a collation the column did not declare — which is
/// worth having, because a `BINARY` column can then carry a `NOCASE` index that
/// `WHERE name = ? COLLATE NOCASE` will actually use.
fn collated_index_columns(columns: &[sqlparser::ast::IndexColumn]) -> Result<Vec<IndexColumnSpec>> {
    let mut names = Vec::with_capacity(columns.len());
    for column in columns {
        let (expr, collation) = peel_collation(&column.column.expr)?;
        let Expr::Identifier(ident) = expr else {
            return Err(Error::Unsupported(alloc::format!(
                "`{}` is not a plain column name; expression constraints are not supported",
                column.column.expr
            )));
        };
        names.push(IndexColumnSpec {
            name: ident.value.clone(),
            collation,
            operator_class: column
                .operator_class
                .as_ref()
                .map(alloc::string::ToString::to_string),
        });
    }
    if names.is_empty() {
        return Err(Error::Type(
            "a UNIQUE or PRIMARY KEY constraint needs at least one column".to_string(),
        ));
    }
    Ok(names)
}

/// Refuse the index tuning a constraint may carry.
///
/// `USING BTREE`, `COMMENT`, `DEFERRABLE` and friends are MySQL and PostgreSQL
/// spellings that SQLite has no equivalent for. Accepting and ignoring them
/// would be the silent-drop bug this phase exists to avoid; the constraint
/// itself is honoured, only the tuning is refused.
fn reject_index_options(
    owner: &str,
    index_type: &Option<sqlparser::ast::IndexType>,
    index_options: &[sqlparser::ast::IndexOption],
    characteristics: &Option<sqlparser::ast::ConstraintCharacteristics>,
) -> Result<()> {
    if index_type.is_some() || !index_options.is_empty() {
        return Err(Error::Unsupported(alloc::format!(
            "index options on a constraint of `{owner}` are not supported"
        )));
    }
    if characteristics.is_some() {
        return Err(Error::Unsupported(alloc::format!(
            "DEFERRABLE / INITIALLY DEFERRED on a constraint of `{owner}` is not supported"
        )));
    }
    Ok(())
}

/// Record a `FOREIGN KEY` / `REFERENCES` declaration.
///
/// **Recorded, never enforced.** SQLite has shipped with foreign keys off by
/// default since 3.6.19 and every framework's migrations assume that, so
/// enforcing them here would break exactly the code this phase exists to run.
/// Dropping them silently was the other option, and it is the one that lies.
fn foreign_key(
    key: &sqlparser::ast::ForeignKeyConstraint,
    column: Option<&str>,
) -> Result<crate::catalog::ForeignKey> {
    let columns = match column {
        // `col INTEGER REFERENCES other(id)` — the local column is the one it
        // was written on, which sqlparser leaves for the caller to supply.
        Some(name) if key.columns.is_empty() => alloc::vec![name.to_string()],
        _ => key.columns.iter().map(|c| c.value.clone()).collect(),
    };
    if columns.is_empty() {
        return Err(Error::Type(
            "a FOREIGN KEY needs at least one column".to_string(),
        ));
    }
    Ok(crate::catalog::ForeignKey {
        columns,
        table: object_name(&key.foreign_table)?,
        referenced: key
            .referred_columns
            .iter()
            .map(|c| c.value.clone())
            .collect(),
        on_delete: key.on_delete.map(|action| action.to_string()),
        on_update: key.on_update.map(|action| action.to_string()),
    })
}

/// Look a column up in a not-yet-built table, by name.
fn require_column<'a>(
    table: &str,
    columns: &'a [Column],
    name: &str,
) -> Result<(usize, &'a Column)> {
    columns
        .iter()
        .enumerate()
        .find(|(_, column)| column.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| Error::Catalog(alloc::format!("no column `{name}` on table `{table}`")))
}

// ------------------------------------------------------------ DROP / ALTER TABLE

fn plan_drop_table(names: Vec<ObjectName>, if_exists: bool) -> Result<Plan> {
    let [name] = names.as_slice() else {
        return Err(Error::Unsupported(
            "DROP TABLE accepts exactly one name".to_string(),
        ));
    };
    Ok(Plan::DropTable(DropTablePlan {
        name: object_name(name)?,
        if_exists,
    }))
}

/// Resolve `ALTER TABLE`, restricted to the four operations SQLite has.
fn plan_alter_table(alter: sqlparser::ast::AlterTable, catalog: &Catalog) -> Result<Plan> {
    use sqlparser::ast::AlterTableOperation as Op;

    if alter.only || alter.location.is_some() || alter.on_cluster.is_some() {
        return Err(Error::Unsupported(
            "ONLY, SET LOCATION and ON CLUSTER are not in SQLite's ALTER TABLE".to_string(),
        ));
    }
    if alter.table_type.is_some() {
        return Err(Error::Unsupported(
            "ALTER ICEBERG/DYNAMIC/EXTERNAL TABLE is not supported".to_string(),
        ));
    }
    let name = object_name(&alter.name)?;
    // `IF EXISTS` on the table itself is not SQLite syntax, so it is refused
    // rather than quietly treated as present.
    if alter.if_exists {
        return Err(Error::Unsupported(
            "ALTER TABLE IF EXISTS is not in SQLite's dialect".to_string(),
        ));
    }
    // SQLite applies exactly one operation per statement.
    let [operation] = alter.operations.as_slice() else {
        return Err(Error::Unsupported(
            "ALTER TABLE takes one operation at a time in SQLite's dialect".to_string(),
        ));
    };

    let action = match operation {
        Op::AddColumn {
            if_not_exists,
            column_def,
            column_position,
            ..
        } => {
            if *if_not_exists {
                return Err(Error::Unsupported(
                    "ALTER TABLE ADD COLUMN IF NOT EXISTS is not in SQLite's dialect".to_string(),
                ));
            }
            if column_position.is_some() {
                return Err(Error::Unsupported(
                    "FIRST / AFTER on ADD COLUMN is a MySQL extension; a new column is always \
                     added last"
                        .to_string(),
                ));
            }
            AlterAction::AddColumn(added_column(
                column_def,
                catalog.require_table(&name)?.strict,
            )?)
        }
        Op::RenameTable { table_name } => {
            let target = match table_name {
                sqlparser::ast::RenameTableNameKind::To(name)
                | sqlparser::ast::RenameTableNameKind::As(name) => name,
            };
            AlterAction::RenameTable(object_name(target)?)
        }
        Op::RenameColumn {
            old_column_name,
            new_column_name,
        } => AlterAction::RenameColumn {
            from: old_column_name.value.clone(),
            to: new_column_name.value.clone(),
        },
        Op::DropColumn {
            column_names,
            if_exists,
            drop_behavior,
            ..
        } => {
            if *if_exists || drop_behavior.is_some() {
                return Err(Error::Unsupported(
                    "IF EXISTS / CASCADE / RESTRICT on DROP COLUMN are not in SQLite's dialect"
                        .to_string(),
                ));
            }
            let [column] = column_names.as_slice() else {
                return Err(Error::Unsupported(
                    "DROP COLUMN drops one column at a time".to_string(),
                ));
            };
            AlterAction::DropColumn(column.value.clone())
        }
        other => {
            return Err(Error::Unsupported(alloc::format!(
                "ALTER TABLE {other} is not supported; SQLite has ADD COLUMN, RENAME TO, \
                 RENAME COLUMN and DROP COLUMN"
            )))
        }
    };

    // Resolved against the catalog now so that a rename of a column that does
    // not exist fails at prepare time.
    let table = catalog.require_table(&name)?;
    match &action {
        AlterAction::RenameColumn { from, .. } => {
            table.require_column(from)?;
        }
        AlterAction::DropColumn(column) => {
            table.require_column(column)?;
        }
        _ => {}
    }

    Ok(Plan::AlterTable(AlterTablePlan {
        table: table.name.clone(),
        action,
    }))
}

/// Resolve the column definition of an `ALTER TABLE ... ADD COLUMN`.
///
/// SQLite's restrictions, and each is a restriction because the alternative
/// would need every existing row to be checked or rewritten against a
/// constraint they were never written under: no `PRIMARY KEY`, no `UNIQUE`,
/// and `NOT NULL` only with a non-`NULL` default.
fn added_column(column: &sqlparser::ast::ColumnDef, strict: bool) -> Result<Column> {
    use sqlparser::ast::ColumnOption as Opt;

    let empty = Catalog::new();
    let ty = if strict {
        resolve_strict_data_type(&column.data_type, &column.name.value)?
    } else {
        resolve_data_type(&column.data_type)?
    };
    let mut resolved = Column::new(&column.name.value, ty);
    for option in &column.options {
        match &option.option {
            Opt::Null => {}
            Opt::NotNull => resolved.not_null = true,
            Opt::Default(expr) => {
                resolve_expr(expr, &Scope::empty(), &mut stored_binder(&empty))?;
                resolved.default = Some(expr.to_string());
            }
            Opt::Collation(name) => {
                resolved.collation = Collation::from_name(&object_name(name)?)?;
            }
            Opt::PrimaryKey(_) | Opt::Unique(_) => {
                return Err(Error::Unsupported(alloc::format!(
                    "cannot add a PRIMARY KEY or UNIQUE column to an existing table; \
                     `{}` would have to be checked against every row already there",
                    column.name.value
                )))
            }
            other => {
                return Err(Error::Unsupported(alloc::format!(
                    "column option `{other}` on an added column is not supported"
                )))
            }
        }
    }
    if resolved.not_null && matches!(resolved.default.as_deref(), None | Some("NULL")) {
        return Err(Error::Constraint(alloc::format!(
            "cannot add a NOT NULL column `{}` without a non-NULL default; the rows already \
             in the table would violate it",
            column.name.value
        )));
    }
    Ok(resolved)
}

// ------------------------------------------------------------------ transactions

/// Resolve `BEGIN` / `COMMIT` / `ROLLBACK` / `SAVEPOINT` / `RELEASE` onto the
/// engine's transaction API.
///
/// A `SAVEPOINT` names a position in the transaction's own log of the writes
/// it has made so far, not a second copy of the storage engine's buffered
/// state — see [`crate::engine::Engine::rollback_to_savepoint`] for why that
/// is enough to make `ROLLBACK TO` sound: it reuses the same full-discard
/// rollback an ordinary `ROLLBACK` already does, then replays a prefix of
/// what was buffered, rather than partially undoing dirty pages, free-list
/// bookkeeping and retrieval-index staging in place.
fn plan_transaction(statement: &Statement) -> Result<Plan> {
    match statement {
        Statement::StartTransaction {
            modes,
            statements,
            exception,
            has_end_keyword,
            modifier,
            transaction,
            ..
        } => {
            if !modes.is_empty() {
                return Err(Error::Unsupported(alloc::format!(
                    "transaction mode `{}` is not supported",
                    modes[0]
                )));
            }
            if !statements.is_empty() || exception.is_some() || *has_end_keyword {
                return Err(Error::Unsupported(
                    "BEGIN ... END blocks are not supported".to_string(),
                ));
            }
            if modifier.is_some() {
                return Err(Error::Unsupported(
                    "DEFERRED / IMMEDIATE / EXCLUSIVE are not supported; every transaction \
                     here pins its snapshot at BEGIN"
                        .to_string(),
                ));
            }
            let _ = transaction;
            Ok(Plan::Begin)
        }
        Statement::Commit {
            chain, modifier, ..
        } => {
            if *chain || modifier.is_some() {
                return Err(Error::Unsupported(
                    "COMMIT AND CHAIN is not supported".to_string(),
                ));
            }
            Ok(Plan::Commit)
        }
        Statement::Rollback { chain, savepoint } => {
            if *chain {
                return Err(Error::Unsupported(
                    "ROLLBACK AND CHAIN is not supported".to_string(),
                ));
            }
            match savepoint {
                Some(name) => Ok(Plan::RollbackToSavepoint(name.value.clone())),
                None => Ok(Plan::Rollback),
            }
        }
        Statement::Savepoint { name } => Ok(Plan::Savepoint(name.value.clone())),
        Statement::ReleaseSavepoint { name } => Ok(Plan::ReleaseSavepoint(name.value.clone())),
        other => Err(Error::Unsupported(alloc::format!(
            "transaction statement `{other}` is not supported"
        ))),
    }
}

/// Refuse the `CREATE TABLE` clauses this stage does not implement.
///
/// Until this existed, every one of them was dropped on the floor. A table
/// declared `WITHOUT ROWID` got a row id; a table-level `UNIQUE (a, b)`
/// constrained nothing. The statement reported success and built something
/// else, which is worse than refusing. What is left here is what is still not
/// implemented — the constraints and `IF NOT EXISTS` moved out of this list
/// and into the planner when they became real.
fn reject_unsupported_create_table(create: &sqlparser::ast::CreateTable) -> Result<()> {
    let not_yet = |what: &str| {
        Err(Error::Unsupported(alloc::format!(
            "{what} is not implemented yet"
        )))
    };

    if create.or_replace {
        return not_yet("CREATE OR REPLACE TABLE");
    }
    if create.like.is_some() {
        return not_yet("CREATE TABLE ... LIKE");
    }
    if create.clone.is_some() {
        return not_yet("CREATE TABLE ... CLONE");
    }
    if !matches!(
        create.table_options,
        sqlparser::ast::CreateTableOptions::None
    ) {
        return not_yet("CREATE TABLE options");
    }
    Ok(())
}

// ------------------------------------------------------------------ CREATE INDEX

fn plan_create_index(create: sqlparser::ast::CreateIndex, catalog: &Catalog) -> Result<Plan> {
    if create.concurrently {
        return Err(Error::Unsupported(
            "CREATE INDEX CONCURRENTLY is not supported".to_string(),
        ));
    }
    if create.if_not_exists {
        return Err(Error::Unsupported(
            "CREATE INDEX IF NOT EXISTS is not supported".to_string(),
        ));
    }
    if !create.include.is_empty()
        || create.nulls_distinct.is_some()
        || !create.with.is_empty()
        || create.predicate.is_some()
        || !create.alter_options.is_empty()
    {
        return Err(Error::Unsupported(
            "INCLUDE, NULLS DISTINCT, WITH, a partial-index WHERE and ALGORITHM/LOCK are not \
             supported on CREATE INDEX"
                .to_string(),
        ));
    }
    // `USING` lands in `using` before the column list and in `index_options`
    // after it; both spellings mean the same thing and both are honoured.
    let mut trailing = None;
    for option in &create.index_options {
        match option {
            sqlparser::ast::IndexOption::Using(kind) => trailing = Some(kind),
            sqlparser::ast::IndexOption::Comment(_) => {
                return Err(Error::Unsupported(
                    "COMMENT on an index is not supported".to_string(),
                ))
            }
        }
    }
    let requested = requested_index_kind(create.using.as_ref().or(trailing))?;
    let name = create
        .name
        .ok_or_else(|| Error::Type("CREATE INDEX needs an index name".to_string()))?;
    let name = object_name(&name)?;
    let table_name = object_name(&create.table_name)?;
    let table = catalog.require_table(&table_name)?;
    // Disclosed, narrower than sqlite3 rather than silent or fake: a
    // secondary index's entries point back to a row by row id, and a
    // `WITHOUT ROWID` table does not have one — it would need to point back
    // by primary key instead, a real feature this engine does not have yet.
    // Refusing by name is the honest answer until it does, not this or the
    // planner silently building an index that could never resolve a probe.
    if table.without_rowid {
        return Err(Error::Unsupported(alloc::format!(
            "CREATE INDEX on WITHOUT ROWID table `{table_name}` is not supported yet: a \
             secondary index's entries point back to a row by row id, and this table has none"
        )));
    }
    // Disclosed for a different reason than `WITHOUT ROWID`'s: a temporary
    // table's rows live behind `temp_storage::TempTableRouter`, which routes
    // `put_row`/`get_row`/`scan_batch` by table name, but a scalar-index
    // entry key (`crate::index::entry_key`) carries only the *index's* name —
    // there is nothing for the router to route a `put_index_entry` call by.
    if table.temporary {
        return Err(Error::Unsupported(alloc::format!(
            "CREATE INDEX on temporary table `{table_name}` is not supported yet: a scalar \
             index's entries are keyed by the index's name alone, with no table name for the \
             temporary-table storage router to route by"
        )));
    }

    let names = collated_index_columns(&create.columns)?;
    let mut ordinals = Vec::with_capacity(names.len());
    let mut types = Vec::with_capacity(names.len());
    let mut collations = Vec::with_capacity(names.len());
    for spec in &names {
        let (column, written) = (&spec.name, &spec.collation);
        let (ordinal, resolved) = table.require_column(column)?;
        // The index's collation is the column's unless the statement wrote one
        // over it. A *unique* index may not: the constraint it enforces is
        // stated in terms of the column, so a `UNIQUE` index keyed under some
        // other collation would decide "duplicate" one way through the probe
        // and another way through the scan that answers when no index applies.
        // Refusing says so; silently keying it under the column's collation
        // would be the dropped clause this project does not do.
        if let Some(written) = written {
            if create.unique && *written != resolved.collation {
                return Err(Error::Unsupported(alloc::format!(
                    "CREATE UNIQUE INDEX ... ({column} COLLATE {written}) is not supported: \
                     the constraint would enforce {written} while `{column}` is declared \
                     {declared}, and the two paths that check it would disagree about what a \
                     duplicate is; declare the collation on the column instead",
                    declared = resolved.collation
                )));
            }
        }
        ordinals.push(ordinal);
        types.push((resolved.name.clone(), resolved.ty));
        collations.push(written.unwrap_or(resolved.collation));
    }
    let [(first_name, first_type)] = types.as_slice() else {
        // More than one column means a B-tree index by default: a bare
        // `CREATE INDEX idx ON t (a, b)` has meant a scalar index for as long
        // as this engine has had one, and inferring `FullText` for two `TEXT`
        // columns the way a *single* `TEXT` column already does would
        // silently change what that long-standing default means. So a
        // multi-column full-text index — MySQL's `FULLTEXT(a, b)`, one
        // combined relevance score over the concatenation of every named
        // column's text — has to be asked for explicitly: `USING FULLTEXT`
        // or `USING BM25`.
        if requested == Some(IndexKind::FullText) {
            index_metric(IndexKind::FullText, create.unique, &names)?;
            return fulltext_plan(name, table_name, ordinals, &names, &types, create.unique);
        }
        // A vector index cannot cover more than one column at all — two
        // embedding columns are generally two different vector spaces, and
        // there is no defensible meaning for one ANN graph over both. Named
        // here rather than left to fall through to `btree_plan`, which would
        // report "a B-tree index needs an orderable column" about a request
        // that never asked for a B-tree.
        if requested == Some(IndexKind::Vector) {
            return Err(Error::Unsupported(String::from(
                "a vector index covers exactly one column; two embedding columns are generally \
                 two different vector spaces and there is no single defensible meaning for one \
                 ANN graph over both",
            )));
        }
        index_metric(IndexKind::BTree, create.unique, &names)?;
        return btree_plan(
            name,
            table_name,
            ordinals,
            &types,
            collations,
            create.unique,
            requested,
        );
    };
    // `USING` used to parse and then be dropped on the floor, which is the bug
    // class this engine refuses to have: an index of a kind nobody built would
    // be reported as if it existed.
    let kind = match requested {
        Some(kind) => kind,
        // Inferred from the column type, as it always has been: on a `TEXT`
        // column `CREATE INDEX` means the BM25 index, which is what every
        // database written against this engine so far assumes. A scalar
        // B-tree index on a `TEXT` column is available by saying so —
        // `USING BTREE` — or by making it `UNIQUE`.
        None => match first_type {
            DataType::Text => IndexKind::FullText,
            DataType::Vector(_) | DataType::QuantizedVector(_) => IndexKind::Vector,
            _ => IndexKind::BTree,
        },
    };
    // The distance the graph will be built under, which for anything but a
    // vector index means refusing the clause rather than dropping it.
    let metric = index_metric(kind, create.unique, &names)?;
    // A retrieval index has no ordered key for a `COLLATE` to apply to, so the
    // clause would have nowhere to go — and a dropped clause is the bug this
    // module is built to refuse. Checked once `kind` is known, because that is
    // what decides whether there is a key at all.
    if kind != IndexKind::BTree
        && !create.unique
        && names.iter().any(|column| column.collation.is_some())
    {
        return Err(Error::Unsupported(alloc::format!(
            "COLLATE on `{first_name}` needs an ordered index; write USING BTREE, or drop the \
             clause — a {} index has no collated key",
            if kind == IndexKind::Vector {
                "vector"
            } else {
                "full-text"
            }
        )));
    }
    if kind == IndexKind::BTree || create.unique {
        return btree_plan(
            name,
            table_name,
            ordinals,
            &types,
            collations,
            create.unique,
            requested,
        );
    }
    // The catalog re-checks this, but the message here can name the syntax
    // that would have worked.
    if kind == IndexKind::FullText && *first_type != DataType::Text {
        return Err(Error::Type(alloc::format!(
            "a full-text index needs a TEXT column, but `{first_name}` is {first_type}; \
             USING BTREE gives an ordered scalar index instead"
        )));
    }

    Ok(Plan::CreateIndex(CreateIndexPlan {
        name,
        table: table_name.to_ascii_lowercase(),
        columns: ordinals,
        kind,
        unique: false,
        // A retrieval index has no collated key; the list is kept the same
        // length as the column list so the catalog's own check passes.
        collations: alloc::vec![Collation::Binary; 1],
        metric,
    }))
}

/// Which distance a `CREATE INDEX` asked its vector index to be built under.
///
/// The spelling is pgvector's **operator class**, written after the column:
///
/// ```sql
/// CREATE INDEX items_embedding ON items USING hnsw (embedding vector_l2_ops)
/// ```
///
/// That statement is pgvector's own, verbatim — `USING hnsw` already resolves
/// to this engine's vector index ([`requested_index_kind`]), the parser already
/// carries the operator class, and `vector_cosine_ops` / `vector_l2_ops` are
/// already what a pgvector user writes and what their migration files contain.
/// This is the project's "compatibility where it is real" test passing rather
/// than being claimed: nothing here is a lookalike token that means something
/// else. MariaDB's `DISTANCE=euclidean` and MySQL's `DISTANCE` index option
/// were the alternatives; both are index *options*, and this dialect's parser
/// admits only `USING` and `COMMENT` there, so adopting either would have been
/// a new spelling for a thing that already has one.
///
/// It is refused everywhere it would mean nothing: on a B-tree or full-text
/// index, on a `CREATE UNIQUE INDEX` (which over a `VECTOR` column builds no
/// graph at all), and for `vector_ip_ops`, whose reason is [`VectorMetric`]'s
/// own. Any operator class on any column of the list is enough to trigger
/// those refusals — a vector index has exactly one column, so for the kind
/// that can honour one there is no ambiguity about which it applies to.
fn index_metric(
    kind: IndexKind,
    unique: bool,
    columns: &[IndexColumnSpec],
) -> Result<VectorMetric> {
    let Some(written) = columns
        .iter()
        .find_map(|column| column.operator_class.as_deref())
    else {
        // The default, and therefore what every index that exists today is.
        return Ok(VectorMetric::Cosine);
    };
    // `CREATE UNIQUE INDEX` over a `VECTOR` column is a constraint and nothing
    // else — there is no ordered index that could enforce it, so it becomes a
    // named `UNIQUE` with a per-write scan and no graph at all. An operator
    // class on one would therefore choose the distance of an index that does
    // not exist, which is precisely the silently-dropped clause this module is
    // built to refuse. Checked before the kind, because `kind` here is
    // `Vector` — inferred from the column — and would otherwise accept it.
    if unique {
        return Err(Error::Unsupported(alloc::format!(
            "`{written}` selects the distance an ANN graph is built under, and \
             CREATE UNIQUE INDEX over a VECTOR column builds no graph — it is a constraint \
             enforced by a scan. Declare the constraint and the index separately"
        )));
    }
    if kind != IndexKind::Vector {
        return Err(Error::Unsupported(alloc::format!(
            "`{written}` selects the distance a vector index is built under, and this is a \
             {} index; drop it, or write USING VECTOR on a VECTOR column",
            match kind {
                IndexKind::FullText => "full-text",
                _ => "B-tree",
            }
        )));
    }
    VectorMetric::from_ops_name(written)
}

/// Which structure `USING <type>` asked for.
///
/// `HASH` is deliberately absent: this engine has no hash index, and mapping
/// it onto the B-tree one would answer a range query from a structure the user
/// was told is a hash.
fn requested_index_kind(using: Option<&sqlparser::ast::IndexType>) -> Result<Option<IndexKind>> {
    use sqlparser::ast::IndexType;
    Ok(match using {
        None => None,
        Some(IndexType::BTree) => Some(IndexKind::BTree),
        Some(IndexType::Custom(ident)) => {
            let name = ident.value.to_ascii_uppercase();
            match name.as_str() {
                "BTREE" => Some(IndexKind::BTree),
                "FULLTEXT" | "BM25" => Some(IndexKind::FullText),
                "VECTOR" | "HNSW" | "ANN" => Some(IndexKind::Vector),
                other => {
                    return Err(Error::Unsupported(alloc::format!(
                        "USING {other} is not a structure this engine has; use BTREE, FULLTEXT \
                         or VECTOR"
                    )))
                }
            }
        }
        Some(other) => {
            return Err(Error::Unsupported(alloc::format!(
                "USING {other} is not a structure this engine has; use BTREE, FULLTEXT or VECTOR"
            )))
        }
    })
}

/// A multi-column `CREATE INDEX ... USING FULLTEXT` (or `USING BM25`) —
/// MySQL's `FULLTEXT(a, b, ...)`: one combined BM25 relevance score over the
/// concatenation of every named column's text, so a query term matching
/// either column contributes to the same rank
/// (the engine's `concatenated_full_text` does the concatenation).
/// Every named column has to be `TEXT` — there is no such thing as a
/// full-text index over a non-text column, multi-column or not — and, like
/// the single-column case, it cannot be `UNIQUE` and cannot carry `COLLATE`
/// (a retrieval index has no ordered key for a collation to apply to).
fn fulltext_plan(
    name: String,
    table_name: String,
    ordinals: Vec<usize>,
    names: &[IndexColumnSpec],
    types: &[(String, DataType)],
    unique: bool,
) -> Result<Plan> {
    if unique {
        return Err(Error::Unsupported(String::from(
            "only a B-tree index can be UNIQUE; a full-text index is not a constraint",
        )));
    }
    if let Some(column) = names.iter().find(|column| column.collation.is_some()) {
        return Err(Error::Unsupported(alloc::format!(
            "COLLATE on `{}` needs an ordered index; write USING BTREE, or drop the \
             clause — a full-text index has no collated key",
            column.name
        )));
    }
    for (column, ty) in types {
        if *ty != DataType::Text {
            return Err(Error::Type(alloc::format!(
                "a full-text index needs TEXT columns, but `{column}` is {ty}"
            )));
        }
    }
    Ok(Plan::CreateIndex(CreateIndexPlan {
        name,
        table: table_name.to_ascii_lowercase(),
        columns: ordinals,
        kind: IndexKind::FullText,
        unique: false,
        // A retrieval index has no collated key; the list is kept the same
        // length as the column list so the catalog's own check passes.
        collations: alloc::vec![Collation::Binary; types.len()],
        // A full-text index has no distance; `index_metric` has already
        // refused an operator class on one.
        metric: VectorMetric::Cosine,
    }))
}

/// A `CREATE [UNIQUE] INDEX` that resolves to a scalar B-tree index, or the
/// constraint-only fallback for the one column type that cannot carry one.
#[allow(clippy::too_many_arguments)]
fn btree_plan(
    name: String,
    table_name: String,
    ordinals: Vec<usize>,
    types: &[(String, DataType)],
    collations: Vec<Collation>,
    unique: bool,
    requested: Option<IndexKind>,
) -> Result<Plan> {
    let unorderable = types
        .iter()
        .find(|(_, ty)| matches!(ty, DataType::Vector(_) | DataType::QuantizedVector(_)));
    if let Some((column, ty)) = unorderable {
        // A `UNIQUE` over a vector column is still a constraint that has to be
        // enforced; it just cannot be enforced by an ordered index, so it
        // keeps the scan. Anything else on a vector column is a plain refusal.
        if unique && requested.is_none() {
            return Ok(Plan::CreateUniqueIndex(CreateUniqueIndexPlan {
                name,
                table: table_name,
                columns: types.iter().map(|(name, _)| name.clone()).collect(),
            }));
        }
        return Err(Error::Type(alloc::format!(
            "a B-tree index needs an orderable column, but `{column}` is {ty}"
        )));
    }
    Ok(Plan::CreateIndex(CreateIndexPlan {
        name,
        table: table_name.to_ascii_lowercase(),
        columns: ordinals,
        kind: IndexKind::BTree,
        unique,
        collations,
        // A B-tree index has no distance; `index_metric` has already refused
        // an operator class on one.
        metric: VectorMetric::Cosine,
    }))
}

fn plan_drop_index(names: Vec<ObjectName>) -> Result<Plan> {
    let [name] = names.as_slice() else {
        return Err(Error::Unsupported(
            "DROP INDEX accepts exactly one name".to_string(),
        ));
    };
    Ok(Plan::DropIndex(DropIndexPlan {
        name: object_name(name)?,
    }))
}

/// One of SQLite's five type affinities.
///
/// An affinity is not a storage class and not a constraint: it is what a
/// *declared type name* decides, and SQLite decides it from the spelling
/// alone. Both the column resolver and `CAST` need the same answer, so the
/// rules live in exactly one place — [`affinity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// SQLite's affinity rules, in the order its documentation states them.
///
/// The order is the whole algorithm and it is load-bearing: `VARCHAR` matches
/// rule 2 through `CHAR`, `POINT` matches rule 1 through `INT`, and
/// `FLOATING POINT` — SQLite's own worked example — is INTEGER rather than
/// REAL because rule 1 sees the `INT` in `POINT` first. Reordering these five
/// tests would silently change what a column stores.
///
/// 1. contains `INT` → INTEGER
/// 2. otherwise contains `CHAR`, `CLOB` or `TEXT` → TEXT
/// 3. otherwise contains `BLOB`, or the type was omitted → BLOB
/// 4. otherwise contains `REAL`, `FLOA` or `DOUB` → REAL
/// 5. otherwise → NUMERIC
fn affinity(declared: &str) -> Affinity {
    let rendered = declared.to_ascii_uppercase();
    if rendered.contains("INT") {
        Affinity::Integer
    } else if rendered.contains("CHAR") || rendered.contains("CLOB") || rendered.contains("TEXT") {
        Affinity::Text
    } else if rendered.contains("BLOB") || rendered.is_empty() {
        Affinity::Blob
    } else if rendered.contains("REAL") || rendered.contains("FLOA") || rendered.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// Map a parsed SQL type onto an InlaySQL column type.
///
/// `VECTOR(n)` reaches us as [`sqlparser::ast::DataType::Custom`] because it is
/// not a standard type, so it is handled first. Everything else is decided by
/// [`affinity`] — **any** type name is accepted, exactly as in SQLite.
///
/// This used to be a whitelist that refused any spelling it did not recognise,
/// which made `DATETIME`, `BOOLEAN`, `JSON`, `ENUM(...)` and `DECIMAL(8,2)`
/// hard errors. That was stricter than the dialect this engine claims to
/// implement, and it is the single thing that stopped a stock ORM's migrations
/// from running (`docs/architecture.md`, decision D7).
fn resolve_data_type(ty: &sqlparser::ast::DataType) -> Result<DataType> {
    if let sqlparser::ast::DataType::Custom(name, modifiers) = ty {
        let name = object_name(name)?;
        if name.eq_ignore_ascii_case("vector") {
            return resolve_vector_type(modifiers);
        }
    }

    Ok(match affinity(&ty.to_string()) {
        Affinity::Integer => DataType::Integer,
        Affinity::Text => DataType::Text,
        Affinity::Blob => DataType::Blob,
        Affinity::Real => DataType::Real,
        Affinity::Numeric => DataType::Numeric,
    })
}

/// Resolve a column's declared type inside a `STRICT` table.
///
/// Checked directly against a real sqlite3 binary: only `INT`/`INTEGER`,
/// `REAL`, `TEXT`, `BLOB` and `ANY` are allowed — case insensitively, with no
/// length or precision argument — and every other name, including ones an
/// ordinary table would resolve to `NUMERIC` (`DECIMAL`, `DATETIME`,
/// `BOOLEAN`, `JSON`, or any name carrying a length like `VARCHAR(10)`), is
/// refused rather than silently answering with a real column's affinity.
/// `VECTOR(n)` is not SQLite's to refuse or allow, and stays exactly as
/// strict as it already is outside `STRICT` — a dimension and a value type
/// it already checks on every write — so it is accepted here unchanged.
fn resolve_strict_data_type(ty: &sqlparser::ast::DataType, column_name: &str) -> Result<DataType> {
    use sqlparser::ast::DataType as Ast;

    if let Ast::Custom(name, modifiers) = ty {
        let name = object_name(name)?;
        if name.eq_ignore_ascii_case("vector") {
            return resolve_vector_type(modifiers);
        }
        if name.eq_ignore_ascii_case("any") && modifiers.is_empty() {
            return Ok(DataType::Any);
        }
    }
    Ok(match ty {
        Ast::Int(None) | Ast::Integer(None) => DataType::Integer,
        Ast::Real => DataType::Real,
        Ast::Text => DataType::Text,
        Ast::Blob(None) => DataType::Blob,
        Ast::Unspecified => {
            return Err(Error::Unsupported(alloc::format!(
                "STRICT table column `{column_name}` has no declared type; STRICT requires \
                 one of INT/INTEGER, REAL, TEXT, BLOB, ANY or VECTOR(n)"
            )))
        }
        _ => {
            return Err(Error::Unsupported(alloc::format!(
                "STRICT table column `{column_name}` has type `{ty}`, which is not one of \
                 INT/INTEGER, REAL, TEXT, BLOB, ANY or VECTOR(n) — STRICT allows only those, \
                 with no length or precision argument"
            )))
        }
    })
}

fn resolve_vector_type(modifiers: &[String]) -> Result<DataType> {
    let (dim, quantized) = match modifiers {
        [dim] => (dim, false),
        [dim, encoding] if encoding.trim().eq_ignore_ascii_case("int8") => (dim, true),
        [_, encoding] => {
            return Err(Error::Unsupported(alloc::format!(
                "VECTOR encoding `{}` is not supported; use VECTOR(n) or VECTOR(n, INT8)",
                encoding.trim()
            )))
        }
        _ => {
            return Err(Error::Unsupported(
                "VECTOR requires a dimension and optional INT8 encoding, e.g. \
                 VECTOR(384) or VECTOR(384, INT8)"
                    .to_string(),
            ));
        }
    };
    let dim: usize = dim
        .trim()
        .parse()
        .map_err(|_| Error::Type(alloc::format!("VECTOR dimension `{dim}` is not a number")))?;
    if dim == 0 {
        return Err(Error::Type("VECTOR dimension must be positive".to_string()));
    }
    Ok(if quantized {
        DataType::QuantizedVector(dim)
    } else {
        DataType::Vector(dim)
    })
}

// ---------------------------------------------------------------------- INSERT

fn plan_insert(
    insert: sqlparser::ast::Insert,
    catalog: &Catalog,
    binder: &mut Binder,
) -> Result<Plan> {
    reject_unsupported_insert_clauses(&insert)?;
    let TableObject::TableName(name) = &insert.table else {
        return Err(Error::Unsupported(
            "INSERT INTO TABLE FUNCTION is not supported".to_string(),
        ));
    };
    let table_name = object_name(name)?;
    let table = catalog.require_table(&table_name)?.clone();

    // Which table column each supplied value belongs to.
    let targets: Vec<usize> = if insert.columns.is_empty() {
        (0..table.columns.len()).collect()
    } else {
        let mut targets = Vec::with_capacity(insert.columns.len());
        for column in &insert.columns {
            let column = object_name(column)?;
            let (ordinal, _) = table.require_column(&column)?;
            if targets.contains(&ordinal) {
                return Err(Error::Type(alloc::format!(
                    "column `{column}` is named twice in one INSERT"
                )));
            }
            targets.push(ordinal);
        }
        targets
    };

    let source = match insert.source.as_ref() {
        // `INSERT INTO t DEFAULT VALUES`: one row, every column taking its
        // default. sqlparser produces no source at all for it.
        None => InsertSource::Values(alloc::vec![alloc::vec![None; table.columns.len()]]),
        Some(query) => match query.body.as_ref() {
            SetExpr::Values(values) => {
                let mut rows = Vec::with_capacity(values.rows.len());
                for row in &values.rows {
                    let exprs = &row.content;
                    if exprs.len() != targets.len() {
                        return Err(Error::Type(alloc::format!(
                            "row has {} value(s) but {} column(s) were targeted",
                            exprs.len(),
                            targets.len()
                        )));
                    }
                    let mut full_row = alloc::vec![None; table.columns.len()];
                    for (expr, &target) in exprs.iter().zip(targets.iter()) {
                        let column = &table.columns[target];
                        // A literal is checked against its column now, so bad
                        // SQL fails at prepare time rather than at the first
                        // execution. A `?` cannot be checked until it is
                        // bound; the executor coerces every cell, so the check
                        // happens either way.
                        full_row[target] = Some(match bind_value(expr, binder)? {
                            PlanExpr::Literal(value) => {
                                PlanExpr::Literal(coerce(value, column, table.strict)?)
                            }
                            other => {
                                if let Some(dim) = column.ty.vector_dim() {
                                    binder.pin_vector_param(&other, dim);
                                }
                                other
                            }
                        });
                    }
                    rows.push(full_row);
                }
                InsertSource::Values(rows)
            }
            // `INSERT ... SELECT`. The query is planned exactly as a standalone
            // one, so every shape a `SELECT` supports — joins, aggregates,
            // `ORDER BY`, `LIMIT`, and since AHL-473 a compound
            // (`UNION`/`INTERSECT`/`EXCEPT`) — works here for free.
            _ => {
                let body = plan_query_body(query, catalog, binder, None)?;
                if matches!(body, SubqueryBody::Scalar(_)) {
                    return Err(Error::Unsupported(
                        "INSERT ... SELECT needs a query with a FROM clause".to_string(),
                    ));
                }
                if body.width() != targets.len() {
                    return Err(Error::Type(alloc::format!(
                        "the SELECT returns {} column(s) but {} column(s) were targeted",
                        body.width(),
                        targets.len()
                    )));
                }
                InsertSource::Select {
                    query: Box::new(body),
                    targets,
                }
            }
        },
    };

    let on_conflict = resolve_on_conflict(&insert, &table, catalog, binder)?;
    let returning = resolve_returning(insert.returning.as_deref(), &table, binder)?;

    Ok(Plan::Insert(Box::new(InsertPlan {
        table: table.name.clone(),
        source,
        on_conflict,
        returning,
    })))
}

/// Resolve `INSERT OR ...`, `REPLACE INTO` and `ON CONFLICT ...` into one
/// decision.
///
/// SQLite spells the same policy two ways — `INSERT OR IGNORE` and
/// `ON CONFLICT DO NOTHING` are the same thing — so they collapse here rather
/// than in the executor.
fn resolve_on_conflict(
    insert: &sqlparser::ast::Insert,
    table: &Table,
    catalog: &Catalog,
    binder: &mut Binder,
) -> Result<OnConflict> {
    use sqlparser::ast::{OnConflictAction, OnInsert, SqliteOnConflict};

    let from_or = match insert.or {
        None => None,
        Some(SqliteOnConflict::Ignore) => Some(ConflictAction::Ignore),
        Some(SqliteOnConflict::Replace) => Some(ConflictAction::Replace),
        // `ROLLBACK`, `ABORT` and `FAIL` differ only in what they do to the
        // *statement's earlier rows* on failure, and a statement here is
        // already atomic — every write it made is discarded together. `ABORT`
        // is therefore exactly what happens, and the other two would be a
        // promise about partial writes that cannot occur.
        Some(SqliteOnConflict::Abort) => Some(ConflictAction::Abort),
        Some(other) => {
            return Err(Error::Unsupported(alloc::format!(
                "INSERT OR {other} is not supported; use OR ABORT, OR IGNORE or OR REPLACE"
            )))
        }
    };
    if insert.replace_into {
        return Ok(OnConflict::or(ConflictAction::Replace));
    }
    if insert.ignore {
        return Ok(OnConflict::or(ConflictAction::Ignore));
    }

    let Some(on) = &insert.on else {
        return Ok(match from_or {
            Some(action) => OnConflict::or(action),
            None => OnConflict::abort(),
        });
    };
    if from_or.is_some() {
        return Err(Error::Unsupported(
            "an INSERT may carry either OR ... or ON CONFLICT, not both".to_string(),
        ));
    }
    let OnInsert::OnConflict(on) = on else {
        return Err(Error::Unsupported(
            "ON DUPLICATE KEY UPDATE is MySQL syntax; write ON CONFLICT ... DO UPDATE".to_string(),
        ));
    };
    let target = resolve_conflict_target(on.conflict_target.as_ref(), table, catalog)?;

    let action = match &on.action {
        OnConflictAction::DoNothing => ConflictAction::Ignore,
        OnConflictAction::DoUpdate(update) => {
            // `SET` and `WHERE` see the stored row and the proposed one side
            // by side, with the stored table first so that an unqualified name
            // means what it does everywhere else.
            let scope = excluded_scope(table);
            let mut assignments = Vec::with_capacity(update.assignments.len());
            for assignment in &update.assignments {
                let sqlparser::ast::AssignmentTarget::ColumnName(name) = &assignment.target else {
                    return Err(Error::Unsupported(
                        "only simple column assignments are supported".to_string(),
                    ));
                };
                let column = assignment_target_column(name)?;
                let (ordinal, column) = table.require_column(&column)?;
                let dim = column.ty.vector_dim();
                let value = resolve_expr(&assignment.value, &scope, binder)?;
                if let Some(dim) = dim {
                    binder.pin_vector_param(&value, dim);
                }
                assignments.push((ordinal, value));
            }
            if assignments.is_empty() {
                return Err(Error::Type(
                    "ON CONFLICT DO UPDATE needs at least one assignment".to_string(),
                ));
            }
            let filter = update
                .selection
                .as_ref()
                .map(|expr| resolve_expr(expr, &scope, binder))
                .transpose()?;
            ConflictAction::Update(Box::new(ConflictUpdate {
                assignments,
                filter,
            }))
        }
    };
    Ok(OnConflict::clause(target, action))
}

/// Resolve an `ON CONFLICT (...)` target to the column ordinals of the
/// constraint it names.
///
/// The target has to name a real uniqueness constraint, and SQLite's reason is
/// not pedantry: **the target decides which conflicts the clause answers for.**
/// `ON CONFLICT (id) DO UPDATE` on a row that collides on some *other* unique
/// column is an ordinary violation, not an upsert. That distinction was found
/// by the differential oracle rather than guessed at, which is most of why the
/// oracle exists.
fn resolve_conflict_target(
    target: Option<&sqlparser::ast::ConflictTarget>,
    table: &Table,
    catalog: &Catalog,
) -> Result<Option<Vec<usize>>> {
    use sqlparser::ast::ConflictTarget;

    let Some(target) = target else {
        return Ok(None);
    };
    let ConflictTarget::Columns(columns) = target else {
        return Err(Error::Unsupported(
            "ON CONFLICT ON CONSTRAINT is a PostgreSQL extension; name the columns instead"
                .to_string(),
        ));
    };
    let mut named = Vec::with_capacity(columns.len());
    for column in columns {
        named.push(table.require_column(&column.value)?.0);
    }

    let is_rowid_alias = matches!(named.as_slice(), [only] if table.rowid_alias() == Some(*only));
    let matches_unique = catalog.constraints(&table.name).is_some_and(|constraints| {
        constraints.unique.iter().any(|group| {
            group.columns.len() == named.len()
                && group.columns.iter().all(|column| {
                    table
                        .column(column)
                        .is_some_and(|(ordinal, _)| named.contains(&ordinal))
                })
        })
    });
    if is_rowid_alias || matches_unique {
        return Ok(Some(named));
    }
    Err(Error::Catalog(alloc::format!(
        "ON CONFLICT ({}) does not name a PRIMARY KEY or UNIQUE constraint of `{}`",
        columns
            .iter()
            .map(|c| c.value.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        table.name
    )))
}

/// The scope an `ON CONFLICT DO UPDATE` resolves in: the stored row, then the
/// proposed row under the name `excluded`.
fn excluded_scope(table: &Table) -> Scope<'static> {
    let excluded = Table {
        without_rowid: false,
        temporary: false,
        primary_key: Vec::new(),
        name: "excluded".to_string(),
        columns: table.columns.clone(),
        strict: false,
    };
    Scope {
        sources: alloc::vec![FromItem::table(table.clone()), FromItem::table(excluded)],
        aliases: alloc::vec![None, None],
        // Both sources hold the same column names, so an unqualified `total`
        // would be ambiguous by the ordinary rule. SQLite reads it as the
        // stored row, and so does this.
        unqualified: Some(0),
        parent: None,
        depth: 0,
    }
}

/// Resolve a `RETURNING` clause over the table the statement writes.
fn resolve_returning(
    returning: Option<&[AstSelectItem]>,
    table: &Table,
    binder: &mut Binder,
) -> Result<Option<Vec<SelectItem>>> {
    let Some(returning) = returning else {
        return Ok(None);
    };
    let scope = Scope::single(table);
    let mut items = Vec::with_capacity(returning.len());
    for item in returning {
        let (expr, alias) = match item {
            AstSelectItem::UnnamedExpr(expr) => (expr, None),
            AstSelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
            AstSelectItem::Wildcard(_) => {
                for (index, column) in table.columns.iter().enumerate() {
                    items.push(SelectItem::Column {
                        index,
                        label: column.name.clone(),
                    });
                }
                continue;
            }
            other => {
                return Err(Error::Unsupported(alloc::format!(
                    "RETURNING item `{other}` is not supported"
                )))
            }
        };
        if matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            if let Ok(index) = resolve_column_ref(expr, &scope) {
                let label = alias
                    .unwrap_or_else(|| scope.column_name(index).unwrap_or_default().to_string());
                items.push(SelectItem::Column { index, label });
                continue;
            }
        }
        let before = binder.aggregates.len();
        let before_windows = binder.windows.len();
        let resolved = resolve_expr(expr, &scope, binder)?;
        if binder.aggregates.len() != before {
            return Err(Error::Unsupported(
                "aggregate functions are not allowed in RETURNING".to_string(),
            ));
        }
        if binder.windows.len() != before_windows {
            return Err(Error::Unsupported(
                "window functions are not allowed in RETURNING".to_string(),
            ));
        }
        items.push(SelectItem::Expr {
            expr: resolved,
            label: alias.unwrap_or_else(|| expr.to_string()),
        });
    }
    if items.is_empty() {
        return Err(Error::Type(
            "RETURNING must project at least one expression".to_string(),
        ));
    }
    Ok(Some(items))
}

/// Refuse the `INSERT` clauses this stage does not implement.
///
/// What is left here is the dialect this engine does not claim: MySQL's
/// priority modifiers, Snowflake's multi-table form, ClickHouse's settings.
/// The clauses that used to be in this list — `OR REPLACE`, `OR IGNORE`,
/// `ON CONFLICT`, `RETURNING` — are implemented now, which is what this list
/// existing in the first place was for.
fn reject_unsupported_insert_clauses(insert: &sqlparser::ast::Insert) -> Result<()> {
    let not_yet = |what: &str| {
        Err(Error::Unsupported(alloc::format!(
            "{what} is not implemented yet"
        )))
    };

    if !insert.assignments.is_empty() {
        return not_yet("INSERT ... SET");
    }
    if insert.overwrite {
        return not_yet("INSERT OVERWRITE");
    }
    if insert.partitioned.is_some() || !insert.after_columns.is_empty() {
        return not_yet("INSERT ... PARTITION");
    }
    if insert.table_alias.is_some() || insert.insert_alias.is_some() {
        return not_yet("an alias on an INSERT target");
    }
    if insert.priority.is_some() {
        return not_yet("an INSERT priority modifier");
    }
    if insert.output.is_some() {
        return not_yet("INSERT ... OUTPUT");
    }
    if insert.settings.is_some() || insert.format_clause.is_some() {
        return not_yet("INSERT ... SETTINGS/FORMAT");
    }
    if insert.multi_table_insert_type.is_some()
        || !insert.multi_table_into_clauses.is_empty()
        || !insert.multi_table_when_clauses.is_empty()
        || insert.multi_table_else_clause.is_some()
    {
        return not_yet("multi-table INSERT");
    }
    Ok(())
}

/// Fit a value to its column, widening integers to reals and checking vector
/// dimensions. Anything else that does not match is a type error.
///
/// `strict` is the enclosing table's [`Table::strict`](crate::catalog::Table)
/// flag. A `STRICT` table's `INTEGER` and `TEXT` columns check and convert a
/// value more narrowly — and, for `TEXT`, more *broadly* — than an ordinary
/// affinity does; every arm below that reads `strict` cites the sqlite3
/// behaviour it was checked against.
pub(crate) fn coerce(value: Value, column: &Column, strict: bool) -> Result<Value> {
    let mismatch = |value: &Value| {
        Err(Error::Type(alloc::format!(
            "column `{}` is {} but the value is {}",
            column.name,
            column.ty,
            value.type_name()
        )))
    };

    match (&column.ty, &value) {
        (_, Value::Null) => Ok(Value::Null),
        (DataType::Integer, Value::Integer(_)) => Ok(value),
        // `STRICT INT` accepts a `REAL` only when it round-trips exactly —
        // confirmed against sqlite3: `INSERT ... VALUES (2.0)` into a
        // `STRICT INT` column stores the integer `2`, and `(2.5)` is refused
        // ("cannot store REAL value in INT column"). No non-strict column
        // of this engine's has ever taken this conversion, and that stays
        // unchanged: the arm requires `strict`.
        (DataType::Integer, Value::Real(r)) if strict => match crate::eval::integer_affinity(*r) {
            Value::Integer(i) => Ok(Value::Integer(i)),
            _ => mismatch(&value),
        },
        (DataType::Real, Value::Real(_)) => Ok(value),
        (DataType::Real, Value::Integer(i)) => Ok(Value::Real(*i as f64)),
        (DataType::Text, Value::Text(_)) => Ok(value),
        // `STRICT TEXT` accepts a number by rendering it exactly as
        // `CAST(x AS TEXT)` would — confirmed against sqlite3: a `STRICT
        // TEXT` column storing the integer `5` reads back as the text `5`.
        // A `BLOB` is still refused ("cannot store BLOB value in TEXT
        // column"), which is why this is two arms naming `Integer`/`Real`
        // rather than a blanket non-`Text` one.
        (DataType::Text, Value::Integer(i)) if strict => Ok(Value::Text(i.to_string().into())),
        (DataType::Text, Value::Real(r)) if strict => {
            Ok(Value::Text(crate::eval::real_to_text(*r).into()))
        }
        (DataType::Blob, Value::Blob(_)) => Ok(value),
        // `NUMERIC` is the affinity every unrecognised type name resolves to,
        // and unlike the four above it is not a storage class: SQLite converts
        // what it can and stores the rest unchanged. An embedding is the one
        // thing that cannot land here, because it is not a SQLite value at all.
        (DataType::Numeric, Value::Vector(_)) => mismatch(&value),
        (DataType::Numeric, _) => Ok(crate::eval::numeric_affinity(value)),
        // `ANY` is `STRICT`'s "no affinity at all" column, only ever reached
        // inside one — confirmed against sqlite3: every storage class round
        // -trips through it unconverted. A `VECTOR` still cannot, for the
        // same reason `NUMERIC` refuses one two arms up: it is not a SQLite
        // value at all.
        (DataType::Any, Value::Vector(_)) => mismatch(&value),
        (DataType::Any, _) => Ok(value),
        (DataType::Vector(dim) | DataType::QuantizedVector(dim), Value::Vector(v)) => {
            if v.len() == *dim {
                Ok(value)
            } else {
                Err(Error::Type(alloc::format!(
                    "column `{}` is {} but the value has dimension {}",
                    column.name,
                    column.ty,
                    v.len()
                )))
            }
        }
        _ => mismatch(&value),
    }
}

// ---------------------------------------------------------------------- SELECT

fn plan_select(query: Query, catalog: &Catalog, binder: &mut Binder) -> Result<Plan> {
    Ok(match plan_query_body(&query, catalog, binder, None)? {
        SubqueryBody::Select(plan) => Plan::Select(plan),
        SubqueryBody::Scalar(plan) => Plan::Scalar(plan),
        SubqueryBody::SetOp(plan) => Plan::SetOperation(plan),
        // A `WITH RECURSIVE cte AS (...) SELECT ... FROM cte` statement's
        // top-level body is the trailing `SELECT`, an ordinary `Select` —
        // `Recursive` only ever exists nested inside that `Select`'s `FROM`,
        // as one `FromItem`'s `derived` body. See `try_plan_recursive_cte`.
        SubqueryBody::Recursive(_) | SubqueryBody::RecursiveSelf(_) => unreachable!(
            "a WITH clause's own trailing query is never itself a recursive CTE's body"
        ),
    })
}

/// Plan one query — a top-level `SELECT`, a subquery, or a derived table.
///
/// `parent` is the scope of the query that encloses this one, and `None` means
/// there is none to correlate against.
fn plan_query_body(
    query: &Query,
    catalog: &Catalog,
    binder: &mut Binder,
    parent: Option<&Scope<'_>>,
) -> Result<SubqueryBody> {
    let Query {
        with,
        body,
        order_by,
        limit_clause,
        fetch,
        locks,
        for_clause,
        settings,
        format_clause,
        pipe_operators,
        ..
    } = query;

    // Everything a `Query` can carry besides its body, its order, its limit
    // and its `WITH` clause. Until this existed each of these parsed and was
    // then dropped on the floor — a `WITH` clause in particular used to run
    // the main `SELECT` and silently ignore the CTE it was written to use;
    // `with` is handled below instead, by `plan_ctes`.
    if fetch.is_some() {
        return Err(Error::Unsupported(
            "FETCH is not supported; use LIMIT".to_string(),
        ));
    }
    if !locks.is_empty() {
        return Err(Error::Unsupported(
            "FOR UPDATE / FOR SHARE is not supported".to_string(),
        ));
    }
    if for_clause.is_some() || settings.is_some() || format_clause.is_some() {
        return Err(Error::Unsupported(
            "FOR XML/JSON, SETTINGS and FORMAT are not supported".to_string(),
        ));
    }
    if !pipe_operators.is_empty() {
        return Err(Error::Unsupported(
            "pipe operators are not supported".to_string(),
        ));
    }

    // `WITH` clauses last the rest of this call — every arm of a compound,
    // every subquery and every derived table nested inside it, however deep
    // (see `plan_ctes`) — so the frame is popped once, after the dispatch
    // below returns, rather than at each of that dispatch's several exits.
    let with_pushed = match with {
        Some(with) => {
            plan_ctes(with, catalog, binder)?;
            true
        }
        None => false,
    };

    let result = match &**body {
        SetExpr::Select(select) => {
            plan_select_arm(select, order_by, limit_clause, catalog, binder, parent)
        }
        SetExpr::SetOperation { .. } => {
            plan_compound(body, order_by, limit_clause, catalog, binder, parent)
        }
        // `((SELECT ...))` — an extra layer of parentheses around a query. It
        // is only the same query when the wrapper carries nothing of its own;
        // an `ORDER BY` or `LIMIT` on the outside of a bracketed query means
        // something the inner plan cannot express, so it is refused rather
        // than flattened.
        SetExpr::Query(inner) => {
            if order_by.is_some() || limit_clause.is_some() {
                Err(Error::Unsupported(
                    "ORDER BY or LIMIT outside a parenthesised query is not supported; write \
                     them inside the parentheses"
                        .to_string(),
                ))
            } else {
                plan_query_body(inner, catalog, binder, parent)
            }
        }
        SetExpr::Values(_) => Err(Error::Unsupported(
            "a bare VALUES list is not supported yet".to_string(),
        )),
        other => Err(Error::Unsupported(alloc::format!(
            "`{other}` is not supported as a query body"
        ))),
    };

    if with_pushed {
        binder.ctes.pop();
        binder.cte_reserved.pop();
    }
    result
}

/// Plan one arm of a `SELECT` body — the restricted grammar with no
/// compound, no `ORDER BY`/`LIMIT` of its own when called as one arm of a
/// compound (`order_by`/`limit_clause` are `None` there; SQLite's grammar
/// gives those to the whole compound, never to an arm).
fn plan_select_arm(
    select: &Select,
    order_by: &Option<OrderBy>,
    limit_clause: &Option<LimitClause>,
    catalog: &Catalog,
    binder: &mut Binder,
    parent: Option<&Scope<'_>>,
) -> Result<SubqueryBody> {
    reject_unsupported_clauses(select)?;
    let distinct = resolve_distinct(select.distinct.as_ref())?;

    if select.from.is_empty() {
        if select.selection.is_some()
            || order_by.is_some()
            || limit_clause.is_some()
            || !select_group_by(&select.group_by)?.is_empty()
            || select.having.is_some()
            || !select.named_window.is_empty()
        {
            return Err(Error::Unsupported(
                "WHERE, GROUP BY, HAVING, ORDER BY, WINDOW and LIMIT need a FROM clause"
                    .to_string(),
            ));
        }
        // `DISTINCT` over the single row a `FROM`-less `SELECT` produces
        // cannot remove anything, so it is dropped rather than refused — the
        // result is the same row either way, which is not the silent-drop
        // hazard the rest of this function guards against.
        return plan_scalar_select(select, binder, parent);
    }

    let (scope, joins) = resolve_from(select, catalog, binder, parent)?;

    // `Binder::named_windows` is this query level's alone — swapped out and
    // restored the same way `Binder::aggregates`/`Binder::windows` are around
    // a subquery, so a `WINDOW` clause in a correlated subquery reached while
    // resolving this select's own `WHERE`/`HAVING`/items cannot shadow (or be
    // shadowed by) this select's own named windows. `plan_select_body` is a
    // separate function, rather than the rest of this one inline, so that
    // every one of its many early returns still restores this before
    // propagating.
    let outer_named_windows = core::mem::take(&mut binder.named_windows);
    let result = plan_select_body(
        select,
        order_by,
        limit_clause,
        distinct,
        scope,
        joins,
        binder,
    );
    binder.named_windows = outer_named_windows;
    result
}

/// The rest of [`plan_select_arm`] once `FROM` has resolved a [`Scope`] and
/// [`Binder::named_windows`] has been reset for this query level.
fn plan_select_body<'p>(
    select: &Select,
    order_by: &Option<OrderBy>,
    limit_clause: &Option<LimitClause>,
    distinct: bool,
    scope: Scope<'p>,
    joins: Vec<Join>,
    binder: &mut Binder,
) -> Result<SubqueryBody> {
    resolve_named_windows(&select.named_window, &scope, binder)?;

    let mut items = Vec::new();
    let mut score: Option<ScoreExpr> = None;

    for item in &select.projection {
        let (expr, alias) = match item {
            AstSelectItem::UnnamedExpr(expr) => (expr, None),
            AstSelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
            AstSelectItem::Wildcard(_) => {
                for source in 0..scope.sources.len() {
                    let base = scope.base(source);
                    for (index, column) in scope.columns(source).iter().enumerate() {
                        items.push(SelectItem::Column {
                            index: base + index,
                            label: column.name.clone(),
                        });
                    }
                }
                continue;
            }
            AstSelectItem::QualifiedWildcard(kind, _) => {
                let qualifier = match kind {
                    SelectItemQualifiedWildcardKind::ObjectName(name) => object_name(name)?,
                    SelectItemQualifiedWildcardKind::Expr(_) => {
                        return Err(Error::Unsupported(
                            "expression wildcards are not supported".to_string(),
                        ))
                    }
                };
                let source = scope.source(&qualifier).ok_or_else(|| {
                    Error::Catalog(alloc::format!(
                        "`{qualifier}` does not refer to a table in this query"
                    ))
                })?;
                let base = scope.base(source);
                for (index, column) in scope.columns(source).iter().enumerate() {
                    items.push(SelectItem::Column {
                        index: base + index,
                        label: column.name.clone(),
                    });
                }
                continue;
            }
            other => {
                return Err(Error::Unsupported(alloc::format!(
                    "projection item `{other}` is not supported"
                )))
            }
        };

        if let Some(expr) = resolve_score_expr(expr, &scope, binder)? {
            if score.is_some() {
                return Err(Error::Unsupported(
                    "a query may contain only one retrieval expression".to_string(),
                ));
            }
            let label = alias.unwrap_or_else(|| DEFAULT_SCORE_LABEL.to_string());
            score = Some(expr);
            items.push(SelectItem::Score { label });
            continue;
        }

        // A bare column reference projects a column; anything else is a scalar
        // expression, which may reference aggregate functions.
        if matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            if let Ok(index) = resolve_column_ref(expr, &scope) {
                let label = alias
                    .unwrap_or_else(|| scope.column_name(index).unwrap_or_default().to_string());
                items.push(SelectItem::Column { index, label });
                continue;
            }
        }

        let resolved = resolve_expr(expr, &scope, binder)?;
        items.push(SelectItem::Expr {
            expr: resolved,
            label: alias.unwrap_or_else(|| expr.to_string()),
        });
    }

    let mut filter = None;
    if let Some(selection) = &select.selection {
        let before = binder.aggregates.len();
        let before_windows = binder.windows.len();
        let resolved = resolve_expr(selection, &scope, binder)?;
        if binder.aggregates.len() != before {
            return Err(Error::Unsupported(
                "aggregate functions are not allowed in WHERE".to_string(),
            ));
        }
        if binder.windows.len() != before_windows {
            return Err(Error::Unsupported(
                "window functions are not allowed in WHERE".to_string(),
            ));
        }
        filter = Some(resolved);
    }

    let (group_by, group_collations) = resolve_group_by(&select.group_by, &scope, binder)?;

    let mut having = None;
    if let Some(selection) = &select.having {
        let before_windows = binder.windows.len();
        let resolved = resolve_expr(selection, &scope, binder)?;
        if binder.windows.len() != before_windows {
            return Err(Error::Unsupported(
                "window functions are not allowed in HAVING".to_string(),
            ));
        }
        having = Some(resolved);
    }

    let is_aggregate = !group_by.is_empty() || !binder.aggregates.is_empty();
    if score.is_some() && is_aggregate {
        return Err(Error::Unsupported(
            "retrieval and aggregation cannot be combined in one query".to_string(),
        ));
    }

    let mut order = resolve_order_by(order_by.as_ref(), &scope, &items, binder)?;
    // `DISTINCT` folds projected rows, and each projected column folds under
    // its own collation — the same rule a `GROUP BY` key follows, which is
    // what `SELECT DISTINCT name` and `SELECT name ... GROUP BY name` agreeing
    // on a `NOCASE` column depends on. Only computed for a `DISTINCT` query:
    // nothing reads it otherwise.
    let distinct_collations = if distinct {
        items
            .iter()
            .map(|item| item_collation(item, &scope, binder))
            .collect()
    } else {
        Vec::new()
    };
    let (limit, offset) = resolve_limit(limit_clause.as_ref(), &scope, binder)?;

    // A retrieval query without an explicit ORDER BY means "best matches
    // first" — anything else would throw away the ranking we just computed.
    if order.is_empty() && score.is_some() {
        order.push(Order::new(OrderKey::Score, true));
    }

    if order.iter().any(|o| o.key == OrderKey::Score) && score.is_none() {
        return Err(Error::Catalog(
            "ORDER BY references a score, but the query selects none".to_string(),
        ));
    }

    Ok(SubqueryBody::Select(Box::new(SelectPlan {
        distinct,
        distinct_collations,
        from: scope.sources,
        joins,
        items,
        score,
        filter,
        group_by,
        group_collations,
        having,
        aggregates: binder.aggregates.clone(),
        windows: binder.windows.clone(),
        order,
        limit,
        offset,
    })))
}

// -------------------------------------------------------------------- WITH

/// Plan every CTE in one `WITH` clause, pushing a new frame onto
/// [`Binder::ctes`]/[`Binder::cte_reserved`] that the caller ([`plan_query_body`])
/// pops once it is done with everything the clause is visible to.
///
/// Names are reserved for the *whole list* before any body is planned, which
/// is what makes a self- or forward-reference fail as an unresolved name
/// (see [`Binder::resolve_cte`]) instead of silently falling through to a
/// same-named real table the moment that reference is inside the very CTE
/// being defined.
fn plan_ctes(with: &With, catalog: &Catalog, binder: &mut Binder) -> Result<()> {
    let reserved = with
        .cte_tables
        .iter()
        .map(|cte| cte.alias.name.value.clone())
        .collect();
    binder.ctes.push(Vec::new());
    binder.cte_reserved.push(reserved);

    for cte in &with.cte_tables {
        if let Err(error) = plan_one_cte(cte, catalog, binder, with.recursive) {
            binder.ctes.pop();
            binder.cte_reserved.pop();
            return Err(error);
        }
    }
    Ok(())
}

/// Plan one `name AS (query)` entry of a `WITH` clause and push it onto the
/// frame [`plan_ctes`] just opened.
///
/// `with_recursive` is the enclosing clause's own `RECURSIVE` keyword, not a
/// property of this one entry — confirmed against sqlite3, a `WITH
/// RECURSIVE` list may freely mix members that self-reference with ones that
/// do not (`WITH RECURSIVE t(a) AS (SELECT 1), cnt(x) AS (SELECT 1 UNION ALL
/// SELECT x+1 FROM cnt WHERE x<3) SELECT * FROM t, cnt` runs there), so this
/// only *attempts* the recursive shape when the keyword is present and falls
/// back to the ordinary path when a given entry turns out not to use it.
fn plan_one_cte(
    cte: &Cte,
    catalog: &Catalog,
    binder: &mut Binder,
    with_recursive: bool,
) -> Result<()> {
    if cte.materialized.is_some() {
        return Err(Error::Unsupported(
            "AS MATERIALIZED / AS NOT MATERIALIZED is not in SQLite's dialect".to_string(),
        ));
    }
    let name = cte.alias.name.value.clone();

    // A CTE cannot correlate to anything outside itself — like a derived
    // table, and for the same reason (SQLite has no `LATERAL`) — so it plans
    // with no parent scope. Its aggregate list is isolated exactly the way
    // [`plan_subquery`] isolates a subquery's: one CTE's `COUNT(*)` must not
    // leak into a sibling's, or into the statement that uses it.
    let outer_aggregates = core::mem::take(&mut binder.aggregates);
    let outer_windows = core::mem::take(&mut binder.windows);
    let planned = if with_recursive {
        match try_plan_recursive_cte(&name, cte, catalog, binder) {
            Ok(Some(body)) => Ok(body),
            Ok(None) => plan_query_body(&cte.query, catalog, binder, None),
            Err(error) => Err(error),
        }
    } else {
        plan_query_body(&cte.query, catalog, binder, None)
    };
    binder.aggregates = outer_aggregates;
    binder.windows = outer_windows;
    let body = planned?;

    let mut table = derived_table(Some(&name), &body)?;
    apply_column_aliases(&mut table, &cte.alias.columns)?;

    binder
        .ctes
        .last_mut()
        .expect("plan_ctes pushed a frame before calling this")
        .push(CteEntry { name, table, body });
    Ok(())
}

/// Attempt to plan `cte` as `WITH RECURSIVE name AS (seed UNION [ALL]
/// recursive)`. `Ok(None)` means `cte`'s body never actually references
/// `name` at all — a member of a `WITH RECURSIVE` list that simply is not
/// recursive itself, which sqlite3 allows — so the caller plans it the
/// ordinary way instead.
///
/// The shape accepted, confirmed against sqlite3 3.54:
///
/// * The body must be a compound (`a UNION [ALL] b`, or `a UNION b UNION
///   ALL c`, ...); a bare `SELECT` can never be legally recursive — there is
///   nothing to seed from — so this always answers `Ok(None)` for one, and
///   a self-reference inside it (illegal either way) surfaces the ordinary
///   ["not yet defined"](Binder::resolve_cte) refusal once the caller
///   re-plans it.
/// * `name` may not appear anywhere in every arm but the last — sqlite3
///   refuses that as a "circular reference", and so, indirectly, does this:
///   every arm but the last plans with `name` still unresolvable, through
///   the same [`fold_compound_arms`] [`plan_compound`] itself uses, so a
///   reference anywhere in it hits [`Binder::resolve_cte`]'s ordinary
///   refusal.
/// * `name` must appear in the *last* arm, exactly once, in its `FROM` (see
///   `push_source`'s [`Binder::recursive_self`] check) — a second
///   occurrence, repeated or nested in a subquery, is refused. Neither an
///   aggregate nor a window function may appear there either — not a
///   sqlite3 rule this happens to match, but a real correctness limit of
///   [`crate::Engine::run_recursive`]'s semi-naive evaluation: a step only
///   ever sees the previous step's new rows, never the whole table an
///   aggregate would need to fold over.
/// * The operator immediately to the left of the last arm — the one
///   [`RecursivePlan::all`] records — must be `UNION` or `UNION ALL`;
///   sqlite3 refuses `INTERSECT`/`EXCEPT` there too (the same "circular
///   reference" message, since neither has a meaning for "keep going until
///   a step adds nothing new").
/// * The last arm is always a plain `SELECT`, never a further compound:
///   guaranteed for free by [`flatten_compound`], which only ever pushes a
///   `Select` onto its operand list — a parenthesised or further-nested arm
///   is refused there already, for every compound, not only a recursive
///   one, matching sqlite3's own "recursive-select must be a simple SELECT".
fn try_plan_recursive_cte(
    name: &str,
    cte: &Cte,
    catalog: &Catalog,
    binder: &mut Binder,
) -> Result<Option<SubqueryBody>> {
    let mut operands: Vec<&Select> = Vec::new();
    let mut ops: Vec<SetOp> = Vec::new();
    if flatten_compound(&cte.query.body, &mut operands, &mut ops).is_err() || operands.len() < 2 {
        return Ok(None);
    }
    let last = *operands.last().expect("checked len >= 2 above");
    let seed_operands = &operands[..operands.len() - 1];
    let (last_op, seed_ops) = ops
        .split_last()
        .expect("len(ops) == len(operands) - 1 >= 1");

    let seed = fold_compound_arms(seed_operands, seed_ops, catalog, binder, None)?;
    let mut seed_table = derived_table(None, &seed)?;
    // The recursive term names the CTE's own declared columns (`cnt(x)`,
    // not whatever the seed's own unaliased projection happened to be
    // called) — the same rewrite `plan_one_cte` applies to the *outer*
    // reference's table, applied here too since the self-reference needs
    // it while resolving the recursive term, before that outer rewrite runs.
    apply_column_aliases(&mut seed_table, &cte.alias.columns)?;

    // Saved and restored rather than cleared, so a recursive CTE nested
    // inside this one's recursive term resolves its own name here rather
    // than this one's — see `Binder::recursive_self`'s doc.
    let outer_recursive_self = binder.recursive_self.take();
    let outer_recursive_self_used = binder.recursive_self_used;
    binder.recursive_self = Some((name.to_string(), seed_table));
    binder.recursive_self_used = false;
    let recursive = plan_compound_arm(last, catalog, binder, None);
    let used = binder.recursive_self_used;
    binder.recursive_self = outer_recursive_self;
    binder.recursive_self_used = outer_recursive_self_used;
    let recursive = recursive?;

    if !used {
        return Ok(None);
    }

    let all = match last_op {
        SetOp::UnionAll => true,
        SetOp::Union => false,
        other => {
            return Err(Error::Unsupported(alloc::format!(
                "the recursive term of `{name}` must be combined with UNION or UNION ALL, not \
                 {other}"
            )));
        }
    };

    if let SubqueryBody::Select(plan) = &recursive {
        if !plan.aggregates.is_empty() || !plan.group_by.is_empty() {
            return Err(Error::Unsupported(alloc::format!(
                "the recursive term of `{name}` may not use an aggregate function; a step only \
                 ever sees the previous step's new rows, never the whole table an aggregate \
                 would need"
            )));
        }
        if !plan.windows.is_empty() {
            return Err(Error::Unsupported(alloc::format!(
                "the recursive term of `{name}` may not use a window function, for the same \
                 reason it may not use an aggregate"
            )));
        }
    }

    let left_width = seed.width();
    let right_width = recursive.width();
    if left_width != right_width {
        return Err(Error::Type(alloc::format!(
            "the recursive term of `{name}` does not have the same number of result columns as \
             its seed ({right_width} vs {left_width})"
        )));
    }
    let collations = (0..left_width)
        .map(|position| {
            body_output_collation(&seed, position).map_or(Collation::Binary, |(c, _)| c)
        })
        .collect();

    Ok(Some(SubqueryBody::Recursive(Box::new(RecursivePlan {
        seed: Box::new(seed),
        recursive: Box::new(recursive),
        all,
        collations,
    }))))
}

/// Rename a synthetic table's columns positionally — `WITH t(a, b) AS
/// (...)`'s column alias list.
///
/// **Not** shared with a derived table's `FROM (SELECT ...) AS d(n)`, even
/// though the rewrite is identical, and even though AHL-463 refused that one
/// too: confirmed against sqlite3 3.54, `WITH t(a, b) AS (...)` is real
/// SQLite syntax and a derived table's column alias list is not (a syntax
/// error there), so only this call site exists — `push_source`'s
/// `TableFactor::Derived` arm keeps refusing it on purpose, not for lack of
/// this code.
///
/// Purely a label rewrite. Collation and the (lack of a real) type are
/// untouched: SQLite computes those from the underlying projection, and an
/// alias does not change what was projected, only what it is called.
fn apply_column_aliases(table: &mut Table, columns: &[TableAliasColumnDef]) -> Result<()> {
    if columns.is_empty() {
        return Ok(());
    }
    if columns.len() != table.columns.len() {
        return Err(Error::Type(alloc::format!(
            "table `{}` has {} column(s) but {} alias(es) were given",
            table.name,
            table.columns.len(),
            columns.len()
        )));
    }
    for (column, alias) in table.columns.iter_mut().zip(columns) {
        if alias.data_type.is_some() {
            return Err(Error::Unsupported(
                "a typed column alias is not supported".to_string(),
            ));
        }
        column.name = alias.name.value.clone();
    }
    Ok(())
}

// ------------------------------------------------------------ set operations

/// Fold a flat sequence of compound arms left-associatively into one
/// `SubqueryBody`, in [`flatten_compound`]'s output order — the part of
/// [`plan_compound`] shared with `try_plan_recursive_cte`, which folds only
/// the non-recursive prefix of a `WITH RECURSIVE` definition this same way
/// before planning its one recursive arm separately.
///
/// `operands` must be non-empty; `ops` one shorter, one operator between
/// each consecutive pair.
fn fold_compound_arms(
    operands: &[&Select],
    ops: &[SetOp],
    catalog: &Catalog,
    binder: &mut Binder,
    parent: Option<&Scope<'_>>,
) -> Result<SubqueryBody> {
    let mut arms = operands.iter();
    let mut acc = plan_compound_arm(
        arms.next()
            .expect("fold_compound_arms is never called with an empty `operands`"),
        catalog,
        binder,
        parent,
    )?;

    for (op, arm) in ops.iter().zip(arms) {
        let right = plan_compound_arm(arm, catalog, binder, parent)?;

        let left_width = acc.width();
        let right_width = right.width();
        if left_width != right_width {
            return Err(Error::Type(alloc::format!(
                "SELECTs to the left and right of {op} do not have the same number of result \
                 columns ({left_width} vs {right_width})"
            )));
        }
        let collations = (0..left_width)
            .map(|position| {
                body_output_collation(&acc, position)
                    .map_or(Collation::Binary, |(collation, _)| collation)
            })
            .collect();

        acc = SubqueryBody::SetOp(Box::new(SetOperationPlan {
            op: *op,
            left: Box::new(acc),
            right: Box::new(right),
            collations,
            order: Vec::new(),
            limit: None,
            offset: None,
        }));
    }
    Ok(acc)
}

/// Plan a chain of `UNION [ALL]` / `INTERSECT` / `EXCEPT`.
///
/// SQLite's own semantics, not the SQL standard's — verified against a real
/// sqlite3 3.54 binary, since these are exactly the places an assumption
/// would be wrong rather than merely incomplete:
///
/// * **Precedence.** sqlparser's generic grammar gives `INTERSECT` a higher
///   binding power than `UNION`/`EXCEPT` (the standard's rule), but SQLite
///   gives every compound operator the *same* precedence, left-associative:
///   `a UNION b INTERSECT c` measured as `(a UNION b) INTERSECT c` there, not
///   `a UNION (b INTERSECT c)` — the two disagree over the same tables.
///   [`flatten_compound`] reads off sqlparser's tree in source order
///   regardless of how it grouped it (an in-order walk of an infix
///   expression tree always recovers the original left-to-right sequence,
///   whatever shape precedence climbing gave it), and the fold below is what
///   redoes the grouping SQLite's way.
/// * **Collation.** A compound's per-column comparison — for deduplication,
///   and for its own `ORDER BY` unless a term overrides it — is the
///   *leftmost* `SELECT`'s collation for that column, full stop, however
///   many operators deep: measured, `t1(NOCASE) UNION t2(BINARY) INTERSECT
///   t3(BINARY)` still compares case-insensitively at the `INTERSECT` step,
///   and an explicit `COLLATE` anywhere but the left arm has no effect at
///   all, not even on the very next operand of the same operator.
///   [`body_output_collation`]'s `SubqueryBody::SetOp` arm recurses into
///   `left`, which is this rule.
/// * **Which literal survives a dedup collision differs by operator**:
///   `UNION` keeps the *last*-inserted row of a colliding group (measured:
///   `t1('ADA' NOCASE) UNION t2('ada')` reports `ada`, the right arm's
///   bytes) where `INTERSECT`/`EXCEPT` only ever emit rows drawn from the
///   left arm, deduplicated first-wins like an ordinary `DISTINCT`
///   (measured: `t1('ADA','ada' NOCASE) INTERSECT t2('ADA')` reports `ADA`,
///   the left arm's first occurrence). See `engine.rs::combine_set_operation`.
/// * **Output labels come from the left arm**, recursively, same rule as
///   collation ([`SubqueryBody::labels`]).
/// * **`ORDER BY`/`LIMIT` bind to the whole compound**, not the last arm,
///   and an `ORDER BY` term may only be an output label or a 1-based
///   ordinal — narrower than an ordinary `SELECT`'s `ORDER BY`. Measured
///   against sqlite3: even `ORDER BY id + 1`, built only from a name that
///   *is* an output column, is refused there ("does not match any column in
///   the result set"), and so is a qualified column most of the time
///   (`ORDER BY t1.a` sometimes resolves there, inconsistently enough not
///   to be worth matching). [`resolve_compound_order_by`] is a separate,
///   stricter resolver for exactly this reason — reusing the ordinary
///   [`resolve_order_by`] against the synthetic scope below would let
///   `id + 1` resolve too, since `id` really is one of that scope's
///   columns, which is one case too many.
///   `LIMIT`/`OFFSET` reuse the ordinary [`resolve_limit`], which does not
///   have this problem (a `LIMIT` expression is not a column reference).
fn plan_compound(
    body: &SetExpr,
    order_by: &Option<OrderBy>,
    limit_clause: &Option<LimitClause>,
    catalog: &Catalog,
    binder: &mut Binder,
    parent: Option<&Scope<'_>>,
) -> Result<SubqueryBody> {
    let mut operands: Vec<&Select> = Vec::new();
    let mut ops: Vec<SetOp> = Vec::new();
    flatten_compound(body, &mut operands, &mut ops)?;
    let acc = fold_compound_arms(&operands, &ops, catalog, binder, parent)?;

    // `ORDER BY`/`LIMIT` bind to the whole compound; resolve them against a
    // synthetic single-source scope built the same way `FROM (SELECT ...)`
    // builds one for a derived table.
    let synthetic = derived_table(None, &acc)?;
    let synthetic_scope = Scope::single(&synthetic);
    let items: Vec<SelectItem> = synthetic
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| SelectItem::Column {
            index,
            label: column.name.clone(),
        })
        .collect();
    let order = resolve_compound_order_by(order_by.as_ref(), &items, &synthetic_scope, binder)?;
    let (limit, offset) = resolve_limit(limit_clause.as_ref(), &synthetic_scope, binder)?;

    let SubqueryBody::SetOp(mut plan) = acc else {
        unreachable!("the fold above always leaves at least one SetOp behind")
    };
    plan.order = order;
    plan.limit = limit;
    plan.offset = offset;
    Ok(SubqueryBody::SetOp(plan))
}

/// Plan one arm of a compound.
///
/// Always a plain `SELECT` (see [`flatten_compound`]), and never with its own
/// `ORDER BY`/`LIMIT` — SQLite's grammar gives those to the whole compound.
/// Its aggregate list is isolated exactly the way [`plan_subquery`] isolates
/// a subquery's: one arm's `COUNT(*)` must not leak into a sibling's. Its
/// *capture* list is deliberately not isolated — an arm may correlate to
/// whatever the whole compound could, measured against sqlite3: `... WHERE x
/// IN (SELECT a FROM t1 UNION SELECT t.x FROM t2)` correlates `t.x` in the
/// second arm to the outer query exactly as it would in the first, so both
/// arms share the one capture frame [`plan_subquery`] pushed (if any) around
/// the whole compound.
fn plan_compound_arm(
    select: &Select,
    catalog: &Catalog,
    binder: &mut Binder,
    parent: Option<&Scope<'_>>,
) -> Result<SubqueryBody> {
    let outer_aggregates = core::mem::take(&mut binder.aggregates);
    let outer_windows = core::mem::take(&mut binder.windows);
    let planned = plan_select_arm(select, &None, &None, catalog, binder, parent);
    binder.aggregates = outer_aggregates;
    binder.windows = outer_windows;
    planned
}

/// Read a chain of `SetExpr::SetOperation` nodes off in source order,
/// undoing whatever grouping sqlparser's precedence climbing gave it.
///
/// An in-order walk (left subtree, this node's operator, right subtree)
/// recovers the original left-to-right sequence of operands and operators
/// regardless of how the tree associated them — the same property that lets
/// a binary expression tree round-trip back to infix notation. [`plan_compound`]
/// then folds the flat sequence left-associatively itself, which is what
/// makes every operator the same precedence here whatever sqlparser thought.
fn flatten_compound<'a>(
    expr: &'a SetExpr,
    operands: &mut Vec<&'a Select>,
    ops: &mut Vec<SetOp>,
) -> Result<()> {
    match expr {
        SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            flatten_compound(left, operands, ops)?;
            ops.push(resolve_set_op(*op, *set_quantifier)?);
            flatten_compound(right, operands, ops)
        }
        SetExpr::Select(select) => {
            operands.push(select.as_ref());
            Ok(())
        }
        // SQLite's compound grammar has no parenthesised arm at all —
        // `select-core` is a bare `SELECT` (or `VALUES`, refused separately
        // below), never a parenthesised query. Confirmed against sqlite3:
        // `... UNION (SELECT ...)` is a syntax error there. sqlparser's
        // generic grammar is looser, so this refuses rather than silently
        // accepting a shape SQLite itself would reject.
        SetExpr::Query(_) => Err(Error::Unsupported(
            "a parenthesised arm of a compound query (UNION/INTERSECT/EXCEPT) is not supported; \
             SQLite's own grammar does not allow one either"
                .to_string(),
        )),
        SetExpr::Values(_) => Err(Error::Unsupported(
            "a VALUES list as an arm of a compound query is not supported yet".to_string(),
        )),
        other => Err(Error::Unsupported(alloc::format!(
            "`{other}` is not supported as an arm of a compound query"
        ))),
    }
}

/// Map a parsed operator and quantifier onto [`SetOp`], refusing every shape
/// SQLite's own grammar does not have — confirmed against sqlite3: no
/// `INTERSECT ALL`, no `EXCEPT ALL`, no `MINUS`, no explicit `UNION
/// DISTINCT` (bare `UNION` is already distinct; the keyword is a syntax
/// error there).
fn resolve_set_op(op: SetOperator, quantifier: SetQuantifier) -> Result<SetOp> {
    match (op, quantifier) {
        (SetOperator::Union, SetQuantifier::All) => Ok(SetOp::UnionAll),
        (SetOperator::Union, SetQuantifier::None) => Ok(SetOp::Union),
        (SetOperator::Intersect, SetQuantifier::None) => Ok(SetOp::Intersect),
        (SetOperator::Except, SetQuantifier::None) => Ok(SetOp::Except),
        (SetOperator::Intersect, SetQuantifier::All) => Err(Error::Unsupported(
            "INTERSECT ALL is not supported; SQLite has no ALL form of INTERSECT".to_string(),
        )),
        (SetOperator::Except, SetQuantifier::All) => Err(Error::Unsupported(
            "EXCEPT ALL is not supported; SQLite has no ALL form of EXCEPT".to_string(),
        )),
        (SetOperator::Minus, _) => Err(Error::Unsupported(
            "MINUS is not in SQLite's dialect; use EXCEPT".to_string(),
        )),
        (op, quantifier) => Err(Error::Unsupported(alloc::format!(
            "`{op} {quantifier}` is not supported"
        ))),
    }
}

/// Plan a subquery written inside `outer`'s expressions.
///
/// Two things are scoped to one query level and are swapped around the inner
/// plan: the aggregate list (an inner `COUNT(*)` must not make the outer
/// `SELECT` an aggregate query) and the capture list (which becomes
/// [`Subquery::captures`]). The `?` counter is deliberately *not* — placeholder
/// numbering runs across the whole statement in written order, which is what a
/// caller binds against.
fn plan_subquery(query: &Query, outer: &Scope<'_>, binder: &mut Binder) -> Result<Subquery> {
    if !binder.subqueries_allowed {
        return Err(Error::Unsupported(
            "a stored DEFAULT or CHECK expression may not contain a subquery; SQLite does not \
             allow one there either"
                .to_string(),
        ));
    }
    let id = binder.subqueries;
    binder.subqueries += 1;

    let outer_aggregates = core::mem::take(&mut binder.aggregates);
    let outer_windows = core::mem::take(&mut binder.windows);
    binder.captures.push(Vec::new());
    let catalog = binder.catalog;
    let planned = plan_query_body(query, catalog, binder, Some(outer));
    let captures = binder.captures.pop().unwrap_or_default();
    binder.aggregates = outer_aggregates;
    binder.windows = outer_windows;

    Ok(Subquery {
        id,
        body: Box::new(planned?),
        captures,
    })
}

/// The binder a stored `DEFAULT` or `CHECK` is resolved with.
///
/// No subqueries, matching [`parse_expression`] — which is what re-resolves the
/// stored text later, and which would otherwise refuse at the first `INSERT`
/// something `CREATE TABLE` had accepted.
fn stored_binder(catalog: &Catalog) -> Binder<'_> {
    let mut binder = Binder::new(catalog);
    binder.subqueries_allowed = false;
    binder
}

/// Whether a resolved expression contains a subquery anywhere.
///
/// The walk is over the *resolved* plan rather than the AST so that there is
/// one definition of "contains a subquery" — the one the executor would meet.
fn contains_subquery(expr: &PlanExpr) -> bool {
    match expr {
        PlanExpr::Subquery { .. } => true,
        PlanExpr::Literal(_)
        | PlanExpr::Param(_)
        | PlanExpr::Column(_)
        | PlanExpr::Outer(_)
        | PlanExpr::Agg(_)
        | PlanExpr::Window(_) => false,
        PlanExpr::Unary { expr, .. } | PlanExpr::Cast { expr, .. } => contains_subquery(expr),
        PlanExpr::Binary { left, right, .. } => contains_subquery(left) || contains_subquery(right),
        PlanExpr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            contains_subquery(expr)
                || contains_subquery(pattern)
                || escape.as_deref().is_some_and(contains_subquery)
        }
        PlanExpr::InList { expr, list, .. } => {
            contains_subquery(expr) || list.iter().any(contains_subquery)
        }
        PlanExpr::Between {
            expr, low, high, ..
        } => contains_subquery(expr) || contains_subquery(low) || contains_subquery(high),
        PlanExpr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            operand.as_deref().is_some_and(contains_subquery)
                || branches
                    .iter()
                    .any(|(when, then)| contains_subquery(when) || contains_subquery(then))
                || else_result.as_deref().is_some_and(contains_subquery)
        }
        PlanExpr::Func { args, .. } => args.iter().any(contains_subquery),
        PlanExpr::Collate { expr, .. } => contains_subquery(expr),
    }
}

/// Refuse a subquery in a statement that cannot run one.
///
/// `UPDATE`, `DELETE` and `INSERT ... VALUES` build their environment and then
/// take the engine mutably to write, so that environment cannot hold the shared
/// borrow a subquery needs to read through (`Engine::read_env`). Rather than
/// discover that at execution — where it would be an error in the middle of a
/// statement that has already written rows — it is refused here.
///
/// The query of an `INSERT ... SELECT` is *not* refused: it runs to completion
/// before any row is written, through the ordinary read path.
fn reject_subquery(what: &str, expr: &PlanExpr) -> Result<()> {
    if contains_subquery(expr) {
        return Err(Error::Unsupported(alloc::format!(
            "a subquery in {what} is not supported yet; subqueries are implemented for SELECT \
             (including the query of an INSERT ... SELECT)"
        )));
    }
    Ok(())
}

/// Apply [`reject_subquery`] to every expression of a write statement.
///
/// One place rather than a call at each resolution site, so that a new
/// expression position on a write statement cannot quietly acquire a subquery
/// nobody can run. `SELECT` and the query of an `INSERT ... SELECT` are what
/// this deliberately does not walk.
fn reject_write_subqueries(plan: &Plan) -> Result<()> {
    fn returning(what: &str, items: Option<&Vec<SelectItem>>) -> Result<()> {
        for item in items.into_iter().flatten() {
            if let SelectItem::Expr { expr, .. } = item {
                reject_subquery(what, expr)?;
            }
        }
        Ok(())
    }

    match plan {
        Plan::Insert(insert) => {
            if let InsertSource::Values(rows) = &insert.source {
                for cell in rows.iter().flatten().flatten() {
                    reject_subquery("INSERT ... VALUES", cell)?;
                }
            }
            if let ConflictAction::Update(update) = &insert.on_conflict.action {
                for (_, expr) in &update.assignments {
                    reject_subquery("ON CONFLICT DO UPDATE", expr)?;
                }
                if let Some(filter) = &update.filter {
                    reject_subquery("ON CONFLICT DO UPDATE ... WHERE", filter)?;
                }
            }
            returning("INSERT ... RETURNING", insert.returning.as_ref())
        }
        Plan::Update(update) => {
            for (_, expr) in &update.assignments {
                reject_subquery("UPDATE ... SET", expr)?;
            }
            if let Some(filter) = &update.filter {
                reject_subquery("UPDATE ... WHERE", filter)?;
            }
            returning("UPDATE ... RETURNING", update.returning.as_ref())
        }
        Plan::Delete(delete) => {
            if let Some(filter) = &delete.filter {
                reject_subquery("DELETE ... WHERE", filter)?;
            }
            returning("DELETE ... RETURNING", delete.returning.as_ref())
        }
        // The same check, on the statement inside. `EXPLAIN` must refuse
        // exactly what running would refuse: describing a plan the engine
        // would not accept is a promise the next statement cannot keep.
        Plan::Explain(inner) => reject_write_subqueries(inner),
        _ => Ok(()),
    }
}

/// Refuse a subquery that projects anything but exactly one column.
///
/// SQLite reports this when the statement is prepared (`sub-select returns N
/// columns - expected 1`) rather than when it runs, and so does this — a
/// two-column scalar subquery is a mistake in the query, not a value.
fn require_single_column(what: &str, query: &Subquery) -> Result<()> {
    let width = query.body.width();
    if width == 1 {
        return Ok(());
    }
    Err(Error::Type(alloc::format!(
        "{what} must return exactly one column, but it returns {width}"
    )))
}

/// Whether a `SELECT` asked for `DISTINCT`.
fn resolve_distinct(distinct: Option<&Distinct>) -> Result<bool> {
    match distinct {
        None | Some(Distinct::All) => Ok(false),
        Some(Distinct::Distinct) => Ok(true),
        Some(Distinct::On(_)) => Err(Error::Unsupported(
            "SELECT DISTINCT ON is a PostgreSQL extension and is not supported".to_string(),
        )),
    }
}

/// Resolve a `SELECT` with no `FROM` into a [`ScalarPlan`].
///
/// `parent` is still threaded through: a correlated `(SELECT o.a)` has no
/// `FROM` of its own and reads nothing but the outer row.
fn plan_scalar_select(
    select: &Select,
    binder: &mut Binder,
    parent: Option<&Scope<'_>>,
) -> Result<SubqueryBody> {
    let scope = Scope {
        sources: Vec::new(),
        aliases: Vec::new(),
        unqualified: None,
        parent,
        depth: capture_depth(parent, binder),
    };
    let mut items = Vec::new();
    for item in &select.projection {
        let (expr, alias) = match item {
            AstSelectItem::UnnamedExpr(expr) => (expr, None),
            AstSelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
            other => {
                return Err(Error::Unsupported(alloc::format!(
                    "projection item `{other}` is not supported in a SELECT without FROM"
                )))
            }
        };
        let before_windows = binder.windows.len();
        let resolved = resolve_expr(expr, &scope, binder)?;
        if binder.windows.len() != before_windows {
            return Err(Error::Unsupported(
                "a window function needs a FROM clause".to_string(),
            ));
        }
        let label = alias.unwrap_or_else(|| expr.to_string());
        items.push(ScalarItem {
            expr: resolved,
            label,
        });
    }
    if items.is_empty() {
        return Err(Error::Unsupported(
            "SELECT without FROM must project at least one expression".to_string(),
        ));
    }
    Ok(SubqueryBody::Scalar(ScalarPlan { items }))
}

/// The `GROUP BY` expressions, or an empty list when the clause is absent.
fn select_group_by(group_by: &GroupByExpr) -> Result<&[Expr]> {
    match group_by {
        GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(Error::Unsupported(
                    "GROUP BY modifiers are not supported".to_string(),
                ));
            }
            Ok(exprs)
        }
        GroupByExpr::All(_) => Err(Error::Unsupported(
            "GROUP BY ALL is not supported".to_string(),
        )),
    }
}

/// Resolve `GROUP BY` expressions. Aggregate functions are not allowed in the
/// grouping key.
fn resolve_group_by(
    group_by: &GroupByExpr,
    scope: &Scope,
    binder: &mut Binder,
) -> Result<(Vec<PlanExpr>, Vec<Collation>)> {
    let expressions = select_group_by(group_by)?;
    let mut resolved = Vec::with_capacity(expressions.len());
    let mut collations = Vec::with_capacity(expressions.len());
    for expr in expressions {
        let before = binder.aggregates.len();
        let before_windows = binder.windows.len();
        let key = resolve_expr(expr, scope, binder)?;
        if binder.aggregates.len() != before {
            return Err(Error::Unsupported(
                "aggregate functions are not allowed in GROUP BY".to_string(),
            ));
        }
        if binder.windows.len() != before_windows {
            return Err(Error::Unsupported(
                "window functions are not allowed in GROUP BY".to_string(),
            ));
        }
        // Grouping is an equality question over one expression, so it asks the
        // single-operand rule: an explicit `COLLATE` on the key, else the
        // column's, else `BINARY`.
        collations.push(term_collation(&key, scope, binder));
        resolved.push(key);
    }
    Ok((resolved, collations))
}

/// Resolve a scalar expression: literals, column references, arithmetic,
/// comparison, logical operators and aggregate functions. `scope` holds the
/// sources the query reads from, and the scope of the query that encloses it;
/// a scope with neither (a top-level `SELECT` without `FROM`) makes every
/// column reference an error.
fn resolve_expr(expr: &Expr, scope: &Scope<'_>, binder: &mut Binder) -> Result<PlanExpr> {
    use sqlparser::ast::BinaryOperator as Op;

    match expr {
        Expr::Nested(inner) => resolve_expr(inner, scope, binder),
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            if scope.sources.is_empty() && scope.parent.is_none() {
                return Err(Error::Catalog(
                    "a column reference needs a FROM clause".to_string(),
                ));
            }
            resolve_column_expr(expr, scope, binder)
        }
        Expr::Value(value) => bind_literal(&value.value, binder),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => Ok(PlanExpr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(resolve_expr(expr, scope, binder)?),
        }),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(PlanExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(resolve_expr(expr, scope, binder)?),
        }),
        Expr::IsNull(expr) => Ok(PlanExpr::Unary {
            op: UnaryOp::IsNull,
            expr: Box::new(resolve_expr(expr, scope, binder)?),
        }),
        Expr::IsNotNull(expr) => Ok(PlanExpr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(resolve_expr(expr, scope, binder)?),
        }),
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => resolve_expr(expr, scope, binder),
        Expr::BinaryOp { left, op, right } => {
            let op = match op {
                Op::Plus => BinaryOp::Add,
                Op::Minus => BinaryOp::Sub,
                Op::Multiply => BinaryOp::Mul,
                Op::Divide => BinaryOp::Div,
                Op::Modulo => BinaryOp::Mod,
                Op::Eq => BinaryOp::Eq,
                Op::NotEq => BinaryOp::NotEq,
                Op::Lt => BinaryOp::Lt,
                Op::LtEq => BinaryOp::LtEq,
                Op::Gt => BinaryOp::Gt,
                Op::GtEq => BinaryOp::GtEq,
                Op::And => BinaryOp::And,
                Op::Or => BinaryOp::Or,
                Op::StringConcat => BinaryOp::Concat,
                // SQLite 3.38+'s JSON path operators (AHL-490); see
                // `plan::BinaryOp::JsonExtractJson`/`JsonExtractText`.
                Op::Arrow => BinaryOp::JsonExtractJson,
                Op::LongArrow => BinaryOp::JsonExtractText,
                // These parse and mean something in SQLite, so they must be
                // named rather than swept into the catch-all: `MATCH` and
                // `REGEXP` are unregistered user functions in stock SQLite
                // and this engine has no equivalent.
                Op::Match => {
                    return Err(Error::Unsupported(
                        "MATCH is not supported; use bm25_score() for full-text search".to_string(),
                    ))
                }
                Op::Regexp => {
                    return Err(Error::Unsupported(
                        "REGEXP is not supported; there is no regular-expression engine here"
                            .to_string(),
                    ))
                }
                other => {
                    return Err(Error::Unsupported(alloc::format!(
                        "operator `{other}` is not supported"
                    )))
                }
            };
            let left = resolve_expr(left, scope, binder)?;
            let right = resolve_expr(right, scope, binder)?;
            // Resolved once, here, and only meaningful for the comparison
            // operators: `AND`, `||` and arithmetic compare nothing, and
            // asking the rules about them would answer a question nobody put.
            let is_comparison = matches!(
                op,
                BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
            );
            let collation = if is_comparison {
                compare_collation(&left, &right, scope, binder)
            } else {
                Collation::Binary
            };
            // AHL-486: stage one of SQLite's comparison rule, ahead of the
            // class-order ranking `mem_cmp`/`compare_cells` already apply.
            let affinity = if is_comparison {
                compare_affinity(&left, &right, scope, binder)
            } else {
                CompareAffinity::None
            };
            Ok(PlanExpr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
                collation,
                affinity,
            })
        }
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => {
            if *any {
                return Err(Error::Unsupported(
                    "LIKE ANY is not supported; it is not in SQLite's dialect".to_string(),
                ));
            }
            let value = resolve_expr(expr, scope, binder)?;
            let pattern = resolve_expr(pattern, scope, binder)?;
            let escape = escape_char
                .as_ref()
                .map(|escape| bind_literal(&escape.value, binder))
                .transpose()?;
            Ok(PlanExpr::Like {
                negated: *negated,
                expr: Box::new(value),
                pattern: Box::new(pattern),
                escape: escape.map(Box::new),
            })
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let value = resolve_expr(expr, scope, binder)?;
            let mut candidates = Vec::with_capacity(list.len());
            for candidate in list {
                candidates.push(resolve_expr(candidate, scope, binder)?);
            }
            // **The left operand alone decides.** `x IN (a, b)` is documented
            // as `x = a OR x = b`, but SQLite does not resolve those two `=`
            // the way it resolves a written one: `sqlite3ExprCodeIN`'s
            // list path codes every `OP_Eq` with
            // `sqlite3ExprCollSeq(pParse, pExpr->pLeft)` and never looks at the
            // list at all. So `'ada' IN (name)` on a `NOCASE` column compares
            // under `BINARY` — a corner worth pinning rather than smoothing
            // over, because the differential oracle would find it either way.
            let collation = term_collation(&value, scope, binder);
            // AHL-486: same "left operand alone" rule as `collation` above —
            // see `term_affinity`'s doc for why the candidates are not asked.
            let affinity = term_affinity(&value, scope, binder);
            Ok(PlanExpr::InList {
                negated: *negated,
                expr: Box::new(value),
                list: candidates,
                collation,
                affinity,
            })
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            // The probe is resolved in *this* scope, the subquery in a child of
            // it. Order matters for `?` numbering: `a IN (SELECT b FROM t
            // WHERE c = ?)` numbers the probe's placeholders first because
            // that is the order they are written.
            let probe = resolve_expr(expr, scope, binder)?;
            let query = plan_subquery(subquery, scope, binder)?;
            require_single_column("IN (SELECT ...)", &query)?;
            // The subquery's single output column is the right operand of
            // every comparison this makes, so it can supply the collation the
            // probe does not — `name IN (SELECT alias FROM ...)` compares under
            // `alias`'s collation when `name` is a literal.
            // `sqlite3BinaryCompareCollSeq` in full: an explicit `COLLATE`
            // wins wherever it is, the left operand first; then the left's
            // implicit collation; then the right's.
            let collation = match body_output_collation(&query.body, 0) {
                Some((right, right_explicit)) => {
                    if !has_explicit_collation(&probe) && right_explicit {
                        right
                    } else {
                        expr_collation(&probe, scope, binder).unwrap_or(right)
                    }
                }
                None => term_collation(&probe, scope, binder),
            };
            // AHL-486: unlike a literal `IN (...)` list, this *does* combine
            // both sides — see `body_output_affinity`'s doc.
            let affinity = combine_affinity(
                expr_affinity(&probe, scope, binder),
                body_output_affinity(&query.body, 0),
            );
            Ok(PlanExpr::Subquery {
                op: SubqueryOp::In {
                    negated: *negated,
                    probe: Box::new(probe),
                    collation,
                    affinity,
                },
                query: Box::new(query),
            })
        }
        Expr::InUnnest { .. } => Err(Error::Unsupported(
            "IN UNNEST(...) is not supported".to_string(),
        )),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let value = resolve_expr(expr, scope, binder)?;
            let low = resolve_expr(low, scope, binder)?;
            let high = resolve_expr(high, scope, binder)?;
            // `x BETWEEN y AND z` is `x >= y AND x <= z`, and SQLite resolves
            // those two comparisons *separately* (`exprCodeBetween` builds two
            // nodes and `codeCompare` asks about each). A `COLLATE` on the
            // upper bound alone therefore applies to the upper comparison and
            // not the lower, which is why there are two fields here.
            let low_collation = compare_collation(&value, &low, scope, binder);
            let high_collation = compare_collation(&value, &high, scope, binder);
            // AHL-486: each bound combines with `value` exactly as `=` would,
            // confirmed against sqlite3 (`'2' BETWEEN lo AND hi` over
            // `INTEGER` bounds converts the literal probe).
            let low_affinity = compare_affinity(&value, &low, scope, binder);
            let high_affinity = compare_affinity(&value, &high, scope, binder);
            Ok(PlanExpr::Between {
                negated: *negated,
                expr: Box::new(value),
                low: Box::new(low),
                high: Box::new(high),
                low_collation,
                high_collation,
                low_affinity,
                high_affinity,
            })
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let operand = operand
                .as_ref()
                .map(|operand| resolve_expr(operand, scope, binder))
                .transpose()?;
            let mut branches = Vec::with_capacity(conditions.len());
            for branch in conditions {
                let condition = resolve_expr(&branch.condition, scope, binder)?;
                let result = resolve_expr(&branch.result, scope, binder)?;
                branches.push((condition, result));
            }
            let else_result = else_result
                .as_ref()
                .map(|result| resolve_expr(result, scope, binder))
                .transpose()?;
            // One collation per branch, because the simple form is one `=` per
            // `WHEN` and SQLite resolves each against the operand separately.
            // The searched form gets none: its `WHEN`s are predicates, and any
            // comparison inside them resolved its own already.
            let branch_collations = match &operand {
                Some(operand) => branches
                    .iter()
                    .map(|(when, _)| compare_collation(operand, when, scope, binder))
                    .collect(),
                None => Vec::new(),
            };
            // AHL-486: one affinity per branch, aligned with
            // `branch_collations` and resolved the same combining way.
            let branch_affinities = match &operand {
                Some(operand) => branches
                    .iter()
                    .map(|(when, _)| compare_affinity(operand, when, scope, binder))
                    .collect(),
                None => Vec::new(),
            };
            Ok(PlanExpr::Case {
                operand: operand.map(Box::new),
                branches,
                else_result: else_result.map(Box::new),
                branch_collations,
                branch_affinities,
            })
        }
        Expr::Cast {
            kind,
            expr,
            data_type,
            array,
            format,
        } => {
            if !matches!(kind, sqlparser::ast::CastKind::Cast) {
                return Err(Error::Unsupported(alloc::format!(
                    "{kind:?} is not supported; SQLite's dialect has only CAST(x AS type)"
                )));
            }
            if *array || format.is_some() {
                return Err(Error::Unsupported(
                    "CAST ... ARRAY and CAST ... FORMAT are not supported".to_string(),
                ));
            }
            Ok(PlanExpr::Cast {
                expr: Box::new(resolve_expr(expr, scope, binder)?),
                to: resolve_cast_type(data_type)?,
            })
        }
        Expr::Collate { expr, collation } => Ok(PlanExpr::Collate {
            expr: Box::new(resolve_expr(expr, scope, binder)?),
            collation: Collation::from_name(&object_name(collation)?)?,
        }),
        Expr::IsDistinctFrom(..) | Expr::IsNotDistinctFrom(..) => Err(Error::Unsupported(
            "IS [NOT] DISTINCT FROM is not supported; write the IS NULL cases out".to_string(),
        )),
        Expr::ILike { .. } => Err(Error::Unsupported(
            "ILIKE is not supported; SQLite's LIKE is already case-insensitive for ASCII"
                .to_string(),
        )),
        Expr::SimilarTo { .. } => Err(Error::Unsupported(
            "SIMILAR TO is not supported".to_string(),
        )),
        Expr::RLike { regexp, .. } => Err(Error::Unsupported(alloc::format!(
            "{} is not supported; there is no regular-expression engine here",
            if *regexp { "REGEXP" } else { "RLIKE" }
        ))),
        // `EXISTS` asks only whether a row came back, so it puts no constraint
        // on the subquery's width — SQLite does not either.
        Expr::Exists { subquery, negated } => Ok(PlanExpr::Subquery {
            op: SubqueryOp::Exists { negated: *negated },
            query: Box::new(plan_subquery(subquery, scope, binder)?),
        }),
        Expr::Subquery(subquery) => {
            let query = plan_subquery(subquery, scope, binder)?;
            require_single_column("a scalar subquery", &query)?;
            Ok(PlanExpr::Subquery {
                op: SubqueryOp::Scalar,
                query: Box::new(query),
            })
        }
        Expr::Function(function) => match resolve_window_function(function, scope, binder)? {
            Some(index) => Ok(PlanExpr::Window(index)),
            None => match resolve_aggregate(function, scope, binder)? {
                Some(index) => Ok(PlanExpr::Agg(index)),
                None => resolve_scalar_function(function, scope, binder),
            },
        },
        // `substr` and `trim` do not arrive as ordinary function calls:
        // sqlparser gives each its own node so that it can also parse the
        // `SUBSTRING(x FROM 2 FOR 3)` and `TRIM(LEADING 'x' FROM y)` spellings.
        // SQLite's dialect has only the comma form, so the others are refused
        // rather than quietly mapped onto it.
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            special,
            shorthand,
        } => {
            if !special {
                return Err(Error::Unsupported(
                    "SUBSTRING(x FROM y FOR z) is not in SQLite's dialect; write                      substr(x, y, z)"
                        .to_string(),
                ));
            }
            let _ = shorthand;
            let Some(from) = substring_from else {
                return Err(Error::Type(
                    "substr() takes between 2 and 3 arguments, got 1".to_string(),
                ));
            };
            let mut args = alloc::vec![
                resolve_expr(expr, scope, binder)?,
                resolve_expr(from, scope, binder)?,
            ];
            if let Some(length) = substring_for {
                args.push(resolve_expr(length, scope, binder)?);
            }
            Ok(PlanExpr::Func {
                func: ScalarFunc::Substr,
                collation: func_collation(&args, scope, binder),
                args,
            })
        }
        Expr::Trim {
            trim_where,
            trim_what,
            expr,
            trim_characters,
        } => {
            if trim_where.is_some() || trim_what.is_some() {
                return Err(Error::Unsupported(
                    "TRIM(LEADING/TRAILING/BOTH ... FROM ...) is not in SQLite's dialect; \
                     use trim(), ltrim() or rtrim()"
                        .to_string(),
                ));
            }
            let mut args = alloc::vec![resolve_expr(expr, scope, binder)?];
            // `trim(x, y)` — the comma form SQLite has — arrives as a list of
            // trim characters rather than as a second function argument.
            if let Some(characters) = trim_characters {
                let [set] = characters.as_slice() else {
                    return Err(Error::Type(alloc::format!(
                        "trim() takes between 1 and 2 arguments, got {}",
                        characters.len() + 1
                    )));
                };
                args.push(resolve_expr(set, scope, binder)?);
            }
            Ok(PlanExpr::Func {
                func: ScalarFunc::Trim,
                collation: func_collation(&args, scope, binder),
                args,
            })
        }
        other => Err(Error::Unsupported(alloc::format!(
            "expression `{other}` is not supported"
        ))),
    }
}

/// The affinity a `CAST` converts to, chosen from the written type name.
///
/// The same [`affinity`] rules [`resolve_data_type`] uses — one implementation,
/// so a `CAST` and a column declaration can never disagree about what
/// `DATETIME` means. The one addition here is refusing `VECTOR`, which SQLite
/// would read as `NUMERIC` and which means something else entirely here.
fn resolve_cast_type(ty: &sqlparser::ast::DataType) -> Result<CastType> {
    if let sqlparser::ast::DataType::Custom(name, _) = ty {
        if object_name(name)?.eq_ignore_ascii_case("vector") {
            return Err(Error::Unsupported(
                "CAST to VECTOR is not supported; bind an embedding as a `?` parameter".to_string(),
            ));
        }
    }

    Ok(match affinity(&ty.to_string()) {
        Affinity::Integer => CastType::Integer,
        Affinity::Text => CastType::Text,
        Affinity::Blob => CastType::Blob,
        Affinity::Real => CastType::Real,
        Affinity::Numeric => CastType::Numeric,
    })
}

/// Resolve an `UPDATE` statement.
fn plan_update(
    update: sqlparser::ast::Update,
    catalog: &Catalog,
    binder: &mut Binder,
) -> Result<Plan> {
    if update.from.is_some() {
        return Err(Error::Unsupported(
            "UPDATE ... FROM is not supported yet".to_string(),
        ));
    }
    if let Some(or) = &update.or {
        return Err(Error::Unsupported(alloc::format!(
            "UPDATE OR {or} is not implemented yet"
        )));
    }
    if update.output.is_some() {
        return Err(Error::Unsupported(
            "UPDATE ... OUTPUT is not implemented yet".to_string(),
        ));
    }
    if !update.order_by.is_empty() || update.limit.is_some() {
        return Err(Error::Unsupported(
            "ORDER BY and LIMIT on UPDATE are not implemented yet".to_string(),
        ));
    }
    let table_name = table_with_joins_name(&update.table)?;
    let table = catalog.require_table(&table_name)?;
    let scope = Scope::single(table);

    let mut set = Vec::with_capacity(update.assignments.len());
    for assignment in update.assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(name) = assignment.target else {
            return Err(Error::Unsupported(
                "only simple column assignments are supported".to_string(),
            ));
        };
        let column_name = assignment_target_column(&name)?;
        let (index, column) = table.require_column(&column_name)?;
        let dim = column.ty.vector_dim();
        let expr = resolve_expr(&assignment.value, &scope, binder)?;
        if let Some(dim) = dim {
            binder.pin_vector_param(&expr, dim);
        }
        set.push((index, expr));
    }

    let filter = update
        .selection
        .map(|selection| resolve_expr(&selection, &scope, binder))
        .transpose()?;
    let returning = resolve_returning(update.returning.as_deref(), table, binder)?;

    Ok(Plan::Update(UpdatePlan {
        table: table.name.clone(),
        assignments: set,
        filter,
        returning,
    }))
}

/// Resolve a `DELETE` statement.
fn plan_delete(
    delete: sqlparser::ast::Delete,
    catalog: &Catalog,
    binder: &mut Binder,
) -> Result<Plan> {
    if !delete.tables.is_empty() {
        return Err(Error::Unsupported(
            "multi-table DELETE is not supported".to_string(),
        ));
    }
    if delete.using.is_some() {
        return Err(Error::Unsupported(
            "DELETE ... USING is not implemented yet".to_string(),
        ));
    }
    if delete.output.is_some() {
        return Err(Error::Unsupported(
            "DELETE ... OUTPUT is not implemented yet".to_string(),
        ));
    }
    if !delete.order_by.is_empty() || delete.limit.is_some() {
        return Err(Error::Unsupported(
            "ORDER BY and LIMIT on DELETE are not implemented yet".to_string(),
        ));
    }
    let table_name = from_table_name(&delete.from)?;
    let table = catalog.require_table(&table_name)?;
    let scope = Scope::single(table);
    let filter = delete
        .selection
        .map(|selection| resolve_expr(&selection, &scope, binder))
        .transpose()?;
    let returning = resolve_returning(delete.returning.as_deref(), table, binder)?;
    Ok(Plan::Delete(DeletePlan {
        table: table.name.clone(),
        filter,
        returning,
    }))
}

/// Extract the table name from a `TableWithJoins`, rejecting joins.
fn table_with_joins_name(table: &sqlparser::ast::TableWithJoins) -> Result<String> {
    if !table.joins.is_empty() {
        return Err(Error::Unsupported("JOIN is not supported yet".to_string()));
    }
    let sqlparser::ast::TableFactor::Table { name, .. } = &table.relation else {
        return Err(Error::Unsupported(
            "only plain table references are supported".to_string(),
        ));
    };
    object_name(name)
}

/// Extract the first table name from a `DELETE`'s `FROM` clause.
fn from_table_name(from: &sqlparser::ast::FromTable) -> Result<String> {
    let tables = match from {
        sqlparser::ast::FromTable::WithFromKeyword(tables)
        | sqlparser::ast::FromTable::WithoutKeyword(tables) => tables,
    };
    let [table] = tables.as_slice() else {
        return Err(Error::Unsupported(
            "DELETE must name exactly one table".to_string(),
        ));
    };
    table_with_joins_name(table)
}

/// Refuse the `SELECT` clauses this stage does not implement.
///
/// Every one of these used to parse and then be discarded, which is the bug
/// class this phase exists to close: a `SELECT ... QUALIFY ...` reported
/// success and returned the unfiltered rows. `DISTINCT` is no longer here
/// because it is implemented; the rest are refused until they are.
fn reject_unsupported_clauses(select: &Select) -> Result<()> {
    let not_yet = |what: &str| {
        Err(Error::Unsupported(alloc::format!(
            "{what} is not supported"
        )))
    };

    if select.top.is_some() {
        return not_yet("SELECT TOP");
    }
    if select.into.is_some() {
        return not_yet("SELECT ... INTO");
    }
    if select.exclude.is_some() {
        return not_yet("SELECT ... EXCLUDE");
    }
    if !select.lateral_views.is_empty() {
        return not_yet("LATERAL VIEW");
    }
    if select.prewhere.is_some() {
        return not_yet("PREWHERE");
    }
    if !select.connect_by.is_empty() {
        return not_yet("CONNECT BY");
    }
    if !select.cluster_by.is_empty() {
        return not_yet("CLUSTER BY");
    }
    if !select.distribute_by.is_empty() {
        return not_yet("DISTRIBUTE BY");
    }
    if !select.sort_by.is_empty() {
        return not_yet("SORT BY");
    }
    // `WINDOW` is implemented (`plan_select_arm` resolves it into named
    // window definitions); `QUALIFY` is not SQLite's dialect at all (SQLite
    // has no such clause), so it stays refused rather than becoming a
    // filter over window results the way it would in a dialect that has it.
    if select.qualify.is_some() {
        return not_yet("QUALIFY");
    }
    if select.value_table_mode.is_some() {
        return not_yet("SELECT AS STRUCT/VALUE");
    }
    if select.select_modifiers.is_some() {
        return not_yet("SELECT modifiers");
    }
    if !select.optimizer_hints.is_empty() {
        return not_yet("optimizer hints");
    }
    Ok(())
}

/// Resolve the `FROM` clause into a [`Scope`] and the joins that bind its
/// tables together.
///
/// `FROM a, b JOIN c ON ...` reaches the planner as a list of `TableWithJoins`,
/// each with its own join chain, but SQL evaluates them left to right as one
/// nested-loop product. This flattens them: each `TableWithJoins` contributes
/// its relation's table, then each join's table, with a comma introducing an
/// implicit cross join (`Inner` with no predicate).
fn resolve_from<'p>(
    select: &Select,
    catalog: &Catalog,
    binder: &mut Binder,
    parent: Option<&'p Scope<'p>>,
) -> Result<(Scope<'p>, Vec<Join>)> {
    // Read before `push_source` runs: a derived table plans its own query,
    // which pushes and pops capture levels of its own.
    let depth = capture_depth(parent, binder);
    let mut sources = Vec::new();
    let mut aliases = Vec::new();
    // (kind, raw ON expression) for every table after the first, in join order.
    let mut pending: Vec<(JoinKind, Option<&Expr>)> = Vec::new();

    for (relation_index, table_with_joins) in select.from.iter().enumerate() {
        if relation_index > 0 {
            pending.push((JoinKind::Inner, None));
        }
        push_source(
            &table_with_joins.relation,
            catalog,
            binder,
            &mut sources,
            &mut aliases,
        )?;
        for join in &table_with_joins.joins {
            if join.global {
                return Err(Error::Unsupported(
                    "GLOBAL JOIN is not supported".to_string(),
                ));
            }
            let (kind, on) = join_kind(&join.join_operator)?;
            pending.push((kind, on));
            push_source(&join.relation, catalog, binder, &mut sources, &mut aliases)?;
        }
    }

    // A `WITHOUT ROWID` table joined against anything else is refused here
    // rather than silently answered wrong: every join strategy below this
    // planner (hash build, index probe, materialise) reads its inner side
    // through the ordinary row-id path, which this table's rows are not
    // reachable through at all — see `Engine::without_rowid_stream`'s doc.
    // A lone `WITHOUT ROWID` table as the *only* source is unaffected; that
    // is the one shape `run_select_to` actually handles.
    if sources.len() > 1 {
        if let Some(item) = sources
            .iter()
            .find(|item| item.derived.is_none() && item.table.without_rowid)
        {
            return Err(Error::Unsupported(alloc::format!(
                "joining WITHOUT ROWID table `{}` with anything else is not supported yet",
                item.table.name
            )));
        }
    }

    let scope = Scope {
        sources,
        aliases,
        unqualified: None,
        parent,
        depth,
    };

    let mut joins = Vec::with_capacity(pending.len());
    for (kind, on) in pending {
        let on = on
            .map(|expr| resolve_expr(expr, &scope, binder))
            .transpose()?;
        joins.push(Join { kind, on });
    }

    Ok((scope, joins))
}

/// The capture list a query with this parent registers into.
///
/// `0` — none — when there is no enclosing scope to capture from. Otherwise the
/// binder's current stack depth, which [`plan_subquery`] pushed a level onto
/// just before planning this query.
fn capture_depth(parent: Option<&Scope<'_>>, binder: &Binder) -> usize {
    match parent {
        Some(_) => binder.captures.len(),
        None => 0,
    }
}

/// The name a `FROM (SELECT ...)` with no `AS` answers to.
///
/// Deliberately unspellable: a derived table with no alias has no name a
/// qualified reference could use, and a name containing parentheses cannot come
/// out of the tokeniser as an identifier. It exists only so error messages and
/// the plan have something to print.
const UNNAMED_DERIVED: &str = "(subquery)";

/// Add one table factor's resolved source (and alias) to the source list.
fn push_source(
    factor: &TableFactor,
    catalog: &Catalog,
    binder: &mut Binder,
    sources: &mut Vec<FromItem>,
    aliases: &mut Vec<Option<String>>,
) -> Result<()> {
    match factor {
        TableFactor::Table {
            name, alias, args, ..
        } => {
            if args.is_some() {
                let table_name = object_name(name)?;
                // `json_each`/`json_tree` (SQLite's json1) are the table-
                // valued calls a JSON-shaped query is most likely to reach
                // this through — named specifically rather than left to the
                // generic message below, per AHL-490's refusal (pinned in
                // `unsupported.test`).
                return Err(Error::Unsupported(alloc::format!(
                    "table-valued functions are not supported: `{table_name}(...)` in FROM \
                     would need a mechanism this engine does not have for a function that \
                     returns rows"
                )));
            }
            let table_name = object_name(name)?;
            // While a `WITH RECURSIVE` CTE's recursive term is being
            // resolved, its own name is not an ordinary (not yet fully
            // planned) CTE reference — it is the one bare name in the whole
            // statement that means "the previous step's frontier". Checked
            // ahead of `resolve_cte`, which would otherwise refuse it as a
            // self-reference the same way it refuses any other.
            if let Some((self_name, self_table)) = binder.recursive_self.clone() {
                if table_name.eq_ignore_ascii_case(&self_name) {
                    if binder.recursive_self_used {
                        return Err(Error::Unsupported(alloc::format!(
                            "`{table_name}` may be referenced only once in the FROM clause of a \
                             recursive common table expression's recursive term; SQLite refuses \
                             a second occurrence too, whether repeated or nested in a subquery"
                        )));
                    }
                    binder.recursive_self_used = true;
                    sources.push(FromItem {
                        table: self_table.clone(),
                        derived: Some(Box::new(SubqueryBody::RecursiveSelf(self_table))),
                    });
                    aliases.push(alias.as_ref().map(|alias| alias.name.value.clone()));
                    return Ok(());
                }
            }
            // A CTE name shadows a real table of the same name for the rest
            // of the statement it was declared in — checked first, and
            // ahead of the catalog, which is what makes `WITH t AS (...)
            // SELECT * FROM t` read the CTE even when a stored table `t`
            // exists too.
            match binder.resolve_cte(&table_name)? {
                Some((table, body)) => {
                    sources.push(FromItem {
                        table,
                        derived: Some(Box::new(body)),
                    });
                }
                None => {
                    let table = catalog.require_table(&table_name)?.clone();
                    sources.push(FromItem::table(table));
                }
            }
            aliases.push(alias.as_ref().map(|alias| alias.name.value.clone()));
            Ok(())
        }
        // `FROM (SELECT ...) [AS alias]`. The inner query is planned against no
        // parent scope: SQLite has no `LATERAL`, so a derived table cannot see
        // the query it sits in, and a name from out there has to be an error.
        TableFactor::Derived {
            lateral,
            subquery,
            alias,
            ..
        } => {
            if *lateral {
                return Err(Error::Unsupported(
                    "LATERAL is not in SQLite's dialect".to_string(),
                ));
            }
            let alias = match alias {
                Some(alias) => {
                    // Unlike `WITH t(a, b) AS (...)`, this is not SQLite
                    // syntax at all — confirmed against sqlite3 3.54:
                    // `FROM (SELECT ...) AS d(n)` is a syntax error there,
                    // even though sqlparser's generic grammar accepts it.
                    // Accepting it here would be adding syntax outside
                    // SQLite's dialect, not merely finishing a feature, so
                    // this keeps refusing it rather than reusing
                    // `apply_column_aliases` the way `plan_one_cte` does.
                    if !alias.columns.is_empty() {
                        return Err(Error::Unsupported(
                            "a column alias list on a derived table is not supported; SQLite \
                             itself has no such syntax — alias the columns inside the subquery \
                             instead"
                                .to_string(),
                        ));
                    }
                    Some(alias.name.value.clone())
                }
                None => None,
            };
            let body = plan_query_body(subquery, catalog, binder, None)?;
            let table = derived_table(alias.as_deref(), &body)?;
            sources.push(FromItem {
                table,
                derived: Some(Box::new(body)),
            });
            aliases.push(alias);
            Ok(())
        }
        TableFactor::NestedJoin { .. } => Err(Error::Unsupported(
            "a parenthesised join in FROM is not supported; write the joins in sequence"
                .to_string(),
        )),
        other => Err(Error::Unsupported(alloc::format!(
            "`{other}` is not supported in FROM"
        ))),
    }
}

/// The synthetic table a derived table presents to the rest of the planner.
///
/// Its columns are the inner query's output headers, in order. They carry
/// [`DataType::Numeric`] because a projected expression has no declared type —
/// `Numeric` is SQLite's "no affinity of its own" and is the only one of the
/// five that is not a storage class. Nothing reads it: a derived row arrives
/// already decoded, so the type is never used to encode, coerce or index. The
/// one place it does bite is a retrieval function, which needs a real `VECTOR`
/// or `TEXT` column and is refused above.
///
/// **The collation is carried across, and the type is not.** That asymmetry is
/// SQLite's: `sqlite3SelectAddColumnTypeAndCollation` puts each projected
/// expression's collating sequence on the synthetic column, and a collation —
/// unlike an affinity — is still consulted on the far side, by every
/// comparison, `ORDER BY`, `GROUP BY` and `DISTINCT` over the derived column.
/// Dropping it would lose a declared collation the moment a query wrapped a
/// subquery in `FROM`, silently, which is the shape of the bug this whole
/// change exists to close.
fn derived_table(alias: Option<&str>, body: &SubqueryBody) -> Result<Table> {
    let labels = body.labels();
    if labels.is_empty() {
        return Err(Error::Unsupported(
            "a derived table must project at least one column".to_string(),
        ));
    }
    Ok(Table {
        without_rowid: false,
        temporary: false,
        primary_key: Vec::new(),
        name: alias.unwrap_or(UNNAMED_DERIVED).to_string(),
        columns: labels
            .into_iter()
            .enumerate()
            .map(|(position, label)| {
                Column::new(label, DataType::Numeric).with_collation(
                    body_output_collation(body, position)
                        .map_or(Collation::Binary, |(collation, _)| collation),
                )
            })
            .collect(),
        strict: false,
    })
}

/// Map a parsed join operator onto a [`JoinKind`] and its `ON` expression.
fn join_kind(operator: &JoinOperator) -> Result<(JoinKind, Option<&Expr>)> {
    let (kind, constraint) = match operator {
        JoinOperator::Inner(constraint) | JoinOperator::Join(constraint) => {
            (JoinKind::Inner, constraint)
        }
        JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
            (JoinKind::Left, constraint)
        }
        other => {
            return Err(Error::Unsupported(alloc::format!(
                "join type `{other:?}` is not supported"
            )))
        }
    };
    let on = match constraint {
        JoinConstraint::On(expr) => Some(expr),
        JoinConstraint::None => None,
        JoinConstraint::Using(_) => {
            return Err(Error::Unsupported(
                "JOIN ... USING is not supported; write the ON predicate".to_string(),
            ))
        }
        JoinConstraint::Natural => {
            return Err(Error::Unsupported(
                "NATURAL JOIN is not supported; write the ON predicate".to_string(),
            ))
        }
    };
    Ok((kind, on))
}

/// The sources a query reads from, in join order, with any aliases.
///
/// A subquery's scope keeps a `parent` pointer, which is the whole of how a
/// correlated reference resolves: a name this query does not have is looked for
/// in the query that encloses it, and found there it becomes a capture rather
/// than a column of this row. See [`capture_outer`].
struct Scope<'p> {
    sources: Vec<FromItem>,
    aliases: Vec<Option<String>>,
    /// When set, an unqualified column name resolves against this source alone
    /// instead of having to be unambiguous across all of them.
    ///
    /// Exactly one construct needs it, and it needs it because its two sources
    /// have identical column names by construction: `ON CONFLICT DO UPDATE`,
    /// where the stored row sits beside `excluded`. Everywhere else ambiguity
    /// is a real mistake and stays an error.
    unqualified: Option<usize>,
    /// The enclosing query's scope. `None` at the top level, and also for a
    /// derived table: SQLite has no `LATERAL`, so `FROM (SELECT ...)` cannot
    /// see the query it sits in, and giving it no parent is what makes that a
    /// "no such column" error rather than a silent capture.
    parent: Option<&'p Scope<'p>>,
    /// Which capture list on [`Binder::captures`] this query registers into:
    /// a query at depth `d` uses `captures[d - 1]`, and `0` means it has none
    /// (a top-level query, or a derived table, both of which have no parent).
    ///
    /// It is the *binder stack's* depth, not the scope chain's. Those differ:
    /// a derived table nested inside a subquery starts a fresh scope chain
    /// (`parent: None`) while the binder stack is still however many subquery
    /// levels deep it was — so counting `parent.depth + 1` would have written
    /// that derived table's own subqueries' captures into an enclosing
    /// subquery's list. `a_derived_table_inside_a_subquery_captures_at_its_own_level`
    /// is the regression test.
    depth: usize,
}

impl<'p> Scope<'p> {
    /// An empty scope, for a `SELECT` without `FROM`.
    fn empty() -> Self {
        Scope {
            sources: Vec::new(),
            aliases: Vec::new(),
            unqualified: None,
            parent: None,
            depth: 0,
        }
    }

    /// A scope of exactly one table, for `UPDATE` and `DELETE`.
    fn single(table: &Table) -> Self {
        Scope {
            sources: alloc::vec![FromItem::table(table.clone())],
            aliases: alloc::vec![None],
            unqualified: None,
            parent: None,
            depth: 0,
        }
    }

    /// Ordinal of the first column this source contributes to the joined row.
    fn base(&self, index: usize) -> usize {
        self.sources[..index]
            .iter()
            .map(|item| item.table.columns.len())
            .sum()
    }

    /// The columns of one source.
    fn columns(&self, index: usize) -> &[Column] {
        &self.sources[index].table.columns
    }

    /// The source whose name or alias matches `qualifier`, if any.
    ///
    /// **Once a source has an explicit alias, its own table name stops
    /// answering to a qualifier — only the alias does.** Confirmed against
    /// sqlite3 3.54: `SELECT t.x FROM t AS a` is `no such column: t.x`, not a
    /// reference to `a`. Getting this wrong is not merely a stricter-than-
    /// necessary error: a qualifier that wrongly matches an aliased source by
    /// its real table name resolves *locally* an identifier that should have
    /// missed and been captured from an enclosing query instead — which is
    /// exactly the shape a correlated subquery reading its own outer table
    /// under a second alias hits (`WHERE a.x = t.x` inside `SELECT ... FROM
    /// t AS a`, correlating out to an unaliased outer `t`): the inner `t.x`
    /// wrongly bound to the inner row's own `a.x` instead of the outer
    /// capture, so the predicate became `a.x = a.x` — true for every
    /// non-`NULL` row regardless of what the outer row actually held, and
    /// `NULL` (not "no match") for a `NULL` one. Only a source with no
    /// explicit alias still answers to its real name, which is why an
    /// unaliased `FROM t` and an aliased `FROM t AS a` in the same query
    /// still let `t.x` and `a.x` pick out the two sources unambiguously —
    /// also confirmed against sqlite3.
    fn source(&self, qualifier: &str) -> Option<usize> {
        self.sources.iter().enumerate().position(|(index, item)| {
            match self.aliases[index].as_deref() {
                Some(alias) => alias.eq_ignore_ascii_case(qualifier),
                None => item.table.name.eq_ignore_ascii_case(qualifier),
            }
        })
    }

    /// The name of the column at a joined-row ordinal, for default headers.
    fn column_name(&self, ordinal: usize) -> Option<&str> {
        for (index, item) in self.sources.iter().enumerate() {
            let base = self.base(index);
            if ordinal < base + item.table.columns.len() {
                return Some(&item.table.columns[ordinal - base].name);
            }
        }
        None
    }

    /// The collating sequence declared on the column at a joined-row ordinal.
    ///
    /// `Some(Collation::Binary)` for a column that declared none, and `None`
    /// only for an ordinal no source covers. The difference matters: SQLite's
    /// `sqlite3ExprCollSeq` gives a column with no `COLLATE` the *default*
    /// collation rather than no collation at all, so a plain column on the left
    /// of a comparison stops the right operand's `NOCASE` from applying. That
    /// is the documented rule — "the collating function of that column is used
    /// with precedence to the left operand" — and it is what
    /// `a_plain_column_on_the_left_wins_over_a_collated_column_on_the_right`
    /// pins.
    fn collation_at(&self, ordinal: usize) -> Option<Collation> {
        for (index, item) in self.sources.iter().enumerate() {
            let base = self.base(index);
            if ordinal < base + item.table.columns.len() {
                return Some(item.table.columns[ordinal - base].collation);
            }
        }
        None
    }

    /// The comparison affinity the column at a joined-row ordinal carries —
    /// [`column_affinity`] of its declared type, or `None` for an ordinal no
    /// source covers or whose column is `VECTOR`/`VECTOR(.., INT8)`, neither
    /// of which is one of SQLite's five affinities.
    fn affinity_at(&self, ordinal: usize) -> Option<CastType> {
        for (index, item) in self.sources.iter().enumerate() {
            let base = self.base(index);
            if ordinal < base + item.table.columns.len() {
                return column_affinity(item.table.columns[ordinal - base].ty);
            }
        }
        None
    }
}

// ------------------------------------------------------------- collation rules

/// Whether an expression carries an explicit `COLLATE` anywhere inside it.
///
/// This is SQLite's `EP_Collate` flag, which propagates from a node's operands
/// and from a function's argument list up to the node itself. It is the first
/// question `sqlite3BinaryCompareCollSeq` asks of each operand, and the whole
/// reason `x = y COLLATE NOCASE` compares under `NOCASE` even though `x` is a
/// column with a collation of its own.
fn has_explicit_collation(expr: &PlanExpr) -> bool {
    matches!(expr, PlanExpr::Collate { .. })
        || collation_operands(expr)
            .into_iter()
            .any(has_explicit_collation)
}

/// The operands `EP_Collate` propagates through, in the order SQLite searches
/// them: left before right, and a function's arguments in written order.
///
/// A subquery's *body* is deliberately absent. SQLite does not propagate a
/// collation out of a subquery either (`EP_Collate` is not set from
/// `x.pSelect`), and it could not usefully: the body is a different row.
fn collation_operands(expr: &PlanExpr) -> Vec<&PlanExpr> {
    match expr {
        PlanExpr::Literal(_)
        | PlanExpr::Param(_)
        | PlanExpr::Column(_)
        | PlanExpr::Outer(_)
        | PlanExpr::Agg(_)
        | PlanExpr::Window(_) => Vec::new(),
        PlanExpr::Unary { expr, .. }
        | PlanExpr::Cast { expr, .. }
        | PlanExpr::Collate { expr, .. } => alloc::vec![expr.as_ref()],
        PlanExpr::Binary { left, right, .. } => alloc::vec![left.as_ref(), right.as_ref()],
        PlanExpr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            let mut out = alloc::vec![expr.as_ref(), pattern.as_ref()];
            out.extend(escape.as_deref());
            out
        }
        PlanExpr::InList { expr, list, .. } => {
            let mut out = alloc::vec![expr.as_ref()];
            out.extend(list.iter());
            out
        }
        PlanExpr::Between {
            expr, low, high, ..
        } => alloc::vec![expr.as_ref(), low.as_ref(), high.as_ref()],
        PlanExpr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            let mut out: Vec<&PlanExpr> = operand.as_deref().into_iter().collect();
            for (when, then) in branches {
                out.push(when);
                out.push(then);
            }
            out.extend(else_result.as_deref());
            out
        }
        PlanExpr::Func { args, .. } => args.iter().collect(),
        PlanExpr::Subquery { op, .. } => match op {
            SubqueryOp::In { probe, .. } => alloc::vec![probe.as_ref()],
            SubqueryOp::Scalar | SubqueryOp::Exists { .. } => Vec::new(),
        },
    }
}

/// The collating sequence one expression carries — SQLite's
/// `sqlite3ExprCollSeq`, written out.
///
/// The walk, in order: a `COLLATE` is the answer; a column is its declared
/// collation (`BINARY` when it declared none, which is *not* the same as
/// having none); `CAST` is transparent, as is unary `+`, which this planner
/// has already dropped by the time an expression gets here; and anything else
/// hands the question to whichever operand carries an explicit `COLLATE`.
/// `None` means the expression has no collation of its own — a literal, a
/// parameter, an arithmetic result — which is what lets the *other* operand of
/// a comparison decide.
fn expr_collation(expr: &PlanExpr, scope: &Scope<'_>, binder: &Binder<'_>) -> Option<Collation> {
    collation_of(expr, &|ordinal| scope.collation_at(ordinal), &|slot| {
        // A correlated reference reads a column of the query that encloses
        // this one, so its collation is that column's — looked up where the
        // capture was resolved rather than guessed at here.
        let parent = scope.parent?;
        let captured = binder
            .captures
            .get(scope.depth.checked_sub(1)?)?
            .get(slot)?;
        expr_collation(captured, parent, binder)
    })
}

/// [`expr_collation`]'s walk, over whatever can answer "what collation does the
/// column at this joined-row ordinal have".
///
/// Split out because the same walk has to run in two places: inside a query
/// being resolved, where a [`Scope`] answers, and over a subquery whose plan is
/// already built, where its own `FROM` list does.
fn collation_of(
    expr: &PlanExpr,
    column: &dyn Fn(usize) -> Option<Collation>,
    outer: &dyn Fn(usize) -> Option<Collation>,
) -> Option<Collation> {
    match expr {
        PlanExpr::Collate { collation, .. } => Some(*collation),
        PlanExpr::Column(ordinal) => column(*ordinal),
        PlanExpr::Outer(slot) => outer(*slot),
        PlanExpr::Cast { expr, .. } => collation_of(expr, column, outer),
        other => collation_operands(other)
            .into_iter()
            .find(|operand| has_explicit_collation(operand))
            .and_then(|operand| collation_of(operand, column, outer)),
    }
}

/// The collating sequence a subquery's output column at `position` carries,
/// and whether it came from an explicit `COLLATE`.
///
/// Two callers, both of which are SQLite behaviour rather than convenience:
///
/// * `x IN (SELECT ...)` — SQLite builds the ephemeral index that answers it
///   with `sqlite3BinaryCompareCollSeq(pParse, lhs, pEList->a[0].pExpr)`, so
///   the projected expression is a real operand of the comparison. The
///   explicit-ness matters there: an explicit `COLLATE` on the subquery's
///   projection beats an *implicit* one on the probe.
/// * a derived table — `sqlite3SelectAddColumnTypeAndCollation` puts each
///   projected expression's collating sequence on the synthetic column, so
///   `FROM (SELECT nc AS s FROM t)` gives `s` whatever `nc` had. Without that
///   step a collation is silently lost the moment a query wraps a subquery in
///   `FROM`, which is a supported shape here.
///
/// A projection this cannot resolve — a correlated reference reaching out of
/// the subquery, a retrieval score — answers `None`, which leaves whatever the
/// caller already had standing.
fn body_output_collation(body: &SubqueryBody, position: usize) -> Option<(Collation, bool)> {
    match body {
        SubqueryBody::Select(plan) => {
            let column = |ordinal: usize| -> Option<Collation> {
                let mut base = 0;
                for item in &plan.from {
                    let width = item.table.columns.len();
                    if ordinal < base + width {
                        return Some(item.table.columns[ordinal - base].collation);
                    }
                    base += width;
                }
                None
            };
            match plan.items.get(position)? {
                SelectItem::Column { index, .. } => column(*index).map(|c| (c, false)),
                SelectItem::Expr { expr, .. } => collation_of(expr, &column, &|_| None)
                    .map(|c| (c, has_explicit_collation(expr))),
                SelectItem::Score { .. } => None,
            }
        }
        // A `SELECT` with no `FROM` has no columns, so only an explicit
        // `COLLATE` in the projection can carry one.
        SubqueryBody::Scalar(plan) => {
            let expr = &plan.items.get(position)?.expr;
            collation_of(expr, &|_| None, &|_| None).map(|c| (c, has_explicit_collation(expr)))
        }
        // A compound's per-column collation is always the *left* arm's,
        // recursively down to the leftmost `SELECT` — measured against
        // sqlite3: a right-arm collation, explicit or not, has no effect on
        // the comparison at all, however many operators deep. See
        // `plan_compound`'s doc for what was checked.
        SubqueryBody::SetOp(plan) => body_output_collation(&plan.left, position),
        // Same rule: always the seed's, never the recursive arm's.
        SubqueryBody::Recursive(plan) => body_output_collation(&plan.seed, position),
        SubqueryBody::RecursiveSelf(table) => {
            table.columns.get(position).map(|c| (c.collation, false))
        }
    }
}

/// The collating sequence a comparison between two expressions uses —
/// SQLite's `sqlite3BinaryCompareCollSeq`.
///
/// An explicit `COLLATE` on either side wins, the left one first; failing that
/// the left operand's own collation, then the right's; failing everything,
/// `BINARY`. Every comparison operator goes through this, and so do `IN`,
/// `BETWEEN` and a simple `CASE`, because SQLite defines all three in terms of
/// `=` with the same left operand each time.
fn compare_collation(
    left: &PlanExpr,
    right: &PlanExpr,
    scope: &Scope<'_>,
    binder: &Binder<'_>,
) -> Collation {
    let resolved = if has_explicit_collation(left) {
        expr_collation(left, scope, binder)
    } else if has_explicit_collation(right) {
        expr_collation(right, scope, binder)
    } else {
        expr_collation(left, scope, binder).or_else(|| expr_collation(right, scope, binder))
    };
    resolved.unwrap_or_default()
}

/// The collating sequence a single-operand term uses: an `ORDER BY` key, a
/// `GROUP BY` key, a `DISTINCT` column or an aggregate's argument.
///
/// The same walk as [`expr_collation`], with `BINARY` where the expression
/// carries nothing.
fn term_collation(expr: &PlanExpr, scope: &Scope<'_>, binder: &Binder<'_>) -> Collation {
    expr_collation(expr, scope, binder).unwrap_or_default()
}

/// The collating sequence `nullif`, `min` and `max` compare under.
///
/// Not [`compare_collation`]: SQLite flags those three `SQLITE_FUNC_NEEDCOLL`
/// and codes an `OP_CollSeq` from the *first argument that has a collation*,
/// with no explicit-beats-implicit step. `min('B' COLLATE NOCASE, 'a')` and
/// `min('B', 'a' COLLATE NOCASE)` therefore differ, and they differ here too.
fn func_collation(args: &[PlanExpr], scope: &Scope<'_>, binder: &Binder<'_>) -> Collation {
    args.iter()
        .find_map(|arg| expr_collation(arg, scope, binder))
        .unwrap_or_default()
}

// -------------------------------------------------------------- affinity rules
//
// AHL-486: before this, a comparison ranked a cross-storage-class pair by
// `mem_cmp`'s class order and nothing else — correct for `1 = 'a'`, wrong for
// `id = '1'` against an `INTEGER` column, where SQLite converts `'1'` to `1`
// first and only falls back to class order if the two sides are still
// different classes afterwards. What follows is that first stage,
// `sqlite3ExprAffinity`/`sqlite3CompareAffinity`, resolved here for the same
// reason a collation is: the rule needs the *expression* on each side (is it
// a bare column? a `CAST`?), and by the time `eval.rs` sees a value that
// information is gone.

/// [`crate::value::DataType`] onto the affinity a comparison sees — SQLite
/// draws no distinction between "what a column stores" and "what a `CAST`
/// converts to" here, so this is the one place the two enums meet.
/// `VECTOR`/`VECTOR(.., INT8)` answer `None`: neither is one of SQLite's five
/// storage affinities, and a comparison against one already fails in
/// `eval::compare_cells` before affinity would matter. `ANY` answers `None`
/// too, and for the same reason as the others rather than a new one: it is
/// SQLite's spelling of "no affinity", confirmed against a real sqlite3
/// binary — `WHERE a = '5'` against a `STRICT` `ANY` column holding the
/// integer `5` matches nothing, exactly the storage-class-order comparison
/// an affinity-free column gets everywhere else.
fn column_affinity(ty: DataType) -> Option<CastType> {
    match ty {
        DataType::Integer => Some(CastType::Integer),
        DataType::Real => Some(CastType::Real),
        DataType::Text => Some(CastType::Text),
        DataType::Blob => Some(CastType::Blob),
        DataType::Numeric => Some(CastType::Numeric),
        DataType::Vector(_) | DataType::QuantizedVector(_) | DataType::Any => None,
    }
}

/// The affinity one expression carries into a comparison — SQLite's
/// `sqlite3ExprAffinity`, run past `sqlite3ExprSkipCollateAndLikely` first.
///
/// Exactly three shapes carry one: a stored column (its declared affinity), a
/// `CAST(... AS type)` (the affinity the cast *target* names — not the
/// operand's own, and not transparent to it the way `collation_of` is),
/// and a correlated reference (the outer column's affinity, looked up where
/// the capture was resolved). `COLLATE` is transparent, confirmed against a
/// real sqlite3 3.54 binary: `id COLLATE NOCASE = '1'` still matches an
/// `INTEGER` column. Every other shape — arithmetic, a function call
/// (`likely(id) = '1'` does *not* match), a literal, a parameter — has none.
///
/// **One corner this cannot reproduce, and says so rather than smoothing it
/// over:** `+id = '1'` does not match in sqlite3 3.54, unlike bare
/// `id = '1'`, even though SQLite's own parser folds a leading unary `+` into
/// its operand at parse time (`spanUnaryPrefix`) — the same node reaches
/// affinity resolution either way, and yet the two spellings answer
/// differently. This planner elides `+expr` into `expr` for the identical
/// reason (there is no `UnaryOp` for it), which means `+id` and `id` become
/// the same [`PlanExpr::Column`] here and cannot be told apart. Nobody writes
/// a bare unary `+` immediately next to a comparison on purpose; it is
/// recorded as a known, unchased gap rather than silently claimed to match.
fn expr_affinity(expr: &PlanExpr, scope: &Scope<'_>, binder: &Binder<'_>) -> Option<CastType> {
    affinity_of(expr, &|ordinal| scope.affinity_at(ordinal), &|slot| {
        let parent = scope.parent?;
        let captured = binder
            .captures
            .get(scope.depth.checked_sub(1)?)?
            .get(slot)?;
        expr_affinity(captured, parent, binder)
    })
}

/// [`expr_affinity`]'s walk, over whatever can answer "what affinity does the
/// column at this joined-row ordinal carry" — split out for the same reason
/// [`collation_of`] is: a subquery whose plan is already built answers
/// through its own `FROM` list, via [`body_output_affinity`].
fn affinity_of(
    expr: &PlanExpr,
    column: &dyn Fn(usize) -> Option<CastType>,
    outer: &dyn Fn(usize) -> Option<CastType>,
) -> Option<CastType> {
    match expr {
        PlanExpr::Column(ordinal) => column(*ordinal),
        PlanExpr::Cast { to, .. } => Some(*to),
        PlanExpr::Collate { expr, .. } => affinity_of(expr, column, outer),
        PlanExpr::Outer(slot) => outer(*slot),
        _ => None,
    }
}

/// The affinity a subquery's output column at `position` carries — the same
/// walk [`body_output_collation`] does, and needed for the same caller:
/// `probe IN (SELECT ...)` combines the probe's affinity with the subquery's
/// single projected column exactly as `probe = column` would (unlike a
/// literal `IN (...)` list — see `Expr::InList`'s `affinity` field doc — because a
/// `SELECT`'s ephemeral index is built with the combined affinity SQLite uses
/// for any other comparison, confirmed against a real sqlite3 3.54 binary:
/// `'1' IN (SELECT id FROM ids)` matches an `INTEGER` `id`, where
/// `'1' IN (id)` over a literal list does not).
fn body_output_affinity(body: &SubqueryBody, position: usize) -> Option<CastType> {
    match body {
        SubqueryBody::Select(plan) => {
            let column = |ordinal: usize| -> Option<CastType> {
                let mut base = 0;
                for item in &plan.from {
                    let width = item.table.columns.len();
                    if ordinal < base + width {
                        return column_affinity(item.table.columns[ordinal - base].ty);
                    }
                    base += width;
                }
                None
            };
            match plan.items.get(position)? {
                SelectItem::Column { index, .. } => column(*index),
                SelectItem::Expr { expr, .. } => affinity_of(expr, &column, &|_| None),
                SelectItem::Score { .. } => None,
            }
        }
        SubqueryBody::Scalar(plan) => {
            let expr = &plan.items.get(position)?.expr;
            affinity_of(expr, &|_| None, &|_| None)
        }
        // Same rule `body_output_collation` uses: a compound's per-column
        // affinity is the left arm's alone.
        SubqueryBody::SetOp(plan) => body_output_affinity(&plan.left, position),
        SubqueryBody::Recursive(plan) => body_output_affinity(&plan.seed, position),
        SubqueryBody::RecursiveSelf(table) => table
            .columns
            .get(position)
            .and_then(|c| column_affinity(c.ty)),
    }
}

/// Combine two operands' affinities into the one conversion a comparison
/// between them applies — SQLite's `sqlite3CompareAffinity`, condensed.
///
/// The documented three-rule version is: numeric affinity wins if either side
/// has it; failing that, text affinity wins if one side has it and the other
/// has *none at all*; failing that, no affinity is applied. This drops the
/// "and the other has none at all" qualifier from the text rule — `Text` is
/// returned whenever either side is `Text`, regardless of what the other side
/// declares — and still agrees with sqlite3 in every corner checked,
/// including a `TEXT` column against a `BLOB` column, because
/// `eval.rs`'s `affinity_conversion` only ever renders an `INTEGER`/`REAL`
/// value as text and a `BLOB`-affinity operand's actual value is never one,
/// so resolving `Text` here and resolving `None` do the identical nothing to
/// it.
fn combine_affinity(left: Option<CastType>, right: Option<CastType>) -> CompareAffinity {
    fn is_numeric(affinity: CastType) -> bool {
        matches!(
            affinity,
            CastType::Integer | CastType::Real | CastType::Numeric
        )
    }

    if left.is_some_and(is_numeric) || right.is_some_and(is_numeric) {
        CompareAffinity::Numeric
    } else if left == Some(CastType::Text) || right == Some(CastType::Text) {
        CompareAffinity::Text
    } else {
        CompareAffinity::None
    }
}

/// The affinity conversion a comparison between two written operands applies
/// — every comparison operator, `BETWEEN`'s two halves and a simple `CASE`
/// branch, all of which resolve a real left *and* right operand the way
/// SQLite's `sqlite3CompareAffinity(pLeft, sqlite3ExprAffinity(pRight))` does.
fn compare_affinity(
    left: &PlanExpr,
    right: &PlanExpr,
    scope: &Scope<'_>,
    binder: &Binder<'_>,
) -> CompareAffinity {
    combine_affinity(
        expr_affinity(left, scope, binder),
        expr_affinity(right, scope, binder),
    )
}

/// The affinity conversion a single operand's own affinity decides alone —
/// **`x IN (a, b, ...)`'s rule**, not the general one: see
/// `Expr::InList`'s `affinity` field doc for why the candidate list is never
/// consulted here.
fn term_affinity(expr: &PlanExpr, scope: &Scope<'_>, binder: &Binder<'_>) -> CompareAffinity {
    combine_affinity(expr_affinity(expr, scope, binder), None)
}

/// Resolve a column reference to its ordinal in the joined row.
///
/// An unqualified name must be unambiguous across every source; a qualified
/// name matches by table name or alias. This is the *local* resolution only:
/// an outer reference is [`resolve_column_expr`]'s business, because it has to
/// register a capture rather than produce an ordinal.
fn resolve_column_ref(expr: &Expr, scope: &Scope<'_>) -> Result<usize> {
    lookup_column(expr, scope)?.ok_or_else(|| missing_column(expr))
}

/// The error a name nothing in scope answers to produces.
fn missing_column(expr: &Expr) -> Error {
    match expr {
        Expr::Identifier(ident) => {
            Error::Catalog(alloc::format!("no such column: {}", ident.value))
        }
        Expr::CompoundIdentifier(parts) => Error::Catalog(alloc::format!(
            "`{}` does not refer to a table in this query",
            join_idents(parts)
        )),
        other => Error::Unsupported(alloc::format!("expected a column reference, got `{other}`")),
    }
}

/// Look a column reference up in `scope` alone.
///
/// `Ok(None)` means "this scope does not have that name", which is a question
/// rather than a mistake — the caller goes on to ask the enclosing scope. An
/// `Err` is a definite error: an ambiguous unqualified name, or a qualifier
/// that *is* one of this query's sources but has no such column. Collapsing
/// those two into "not found" would let `SELECT (SELECT t.missing) FROM t`
/// resolve `t.missing` against something outside, which is how a typo turns
/// into a wrong answer instead of an error.
fn lookup_column(expr: &Expr, scope: &Scope<'_>) -> Result<Option<usize>> {
    match expr {
        Expr::Identifier(ident) => {
            let name = &ident.value;
            if let Some(source) = scope.unqualified {
                let (ordinal, _) = scope.sources[source].table.require_column(name)?;
                return Ok(Some(scope.base(source) + ordinal));
            }
            let mut found = None;
            for (index, item) in scope.sources.iter().enumerate() {
                if let Some((ordinal, _)) = item.table.column(name) {
                    if found.is_some() {
                        return Err(Error::Catalog(alloc::format!(
                            "column `{name}` is ambiguous"
                        )));
                    }
                    found = Some(scope.base(index) + ordinal);
                }
            }
            Ok(found)
        }
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            [qualifier, column] => match scope.source(&qualifier.value) {
                Some(source) => {
                    let (ordinal, _) = scope.sources[source].table.require_column(&column.value)?;
                    Ok(Some(scope.base(source) + ordinal))
                }
                None => Ok(None),
            },
            _ => Err(Error::Catalog(alloc::format!(
                "`{}` is not a column reference",
                join_idents(parts)
            ))),
        },
        other => Err(Error::Unsupported(alloc::format!(
            "expected a column reference, got `{other}`"
        ))),
    }
}

/// Resolve a column reference to an expression: a column of this query's row,
/// or a capture from an enclosing one.
fn resolve_column_expr(expr: &Expr, scope: &Scope<'_>, binder: &mut Binder) -> Result<PlanExpr> {
    if let Some(ordinal) = lookup_column(expr, scope)? {
        return Ok(PlanExpr::Column(ordinal));
    }
    match capture_outer(expr, scope, binder)? {
        Some(outer) => Ok(outer),
        None => Err(missing_column(expr)),
    }
}

/// Resolve `expr` in an enclosing scope, registering the capture chain that
/// carries its value inwards.
///
/// A reference that skips a level — the innermost query naming a column of the
/// outermost — makes *every* level between them capture it in turn, so each
/// subquery's plan only ever reads its own row and its own capture list. That
/// is why this recurses rather than walking to the top and returning an
/// absolute ordinal.
fn capture_outer(expr: &Expr, scope: &Scope<'_>, binder: &mut Binder) -> Result<Option<PlanExpr>> {
    let Some(parent) = scope.parent else {
        return Ok(None);
    };
    let in_parent = match lookup_column(expr, parent)? {
        Some(ordinal) => PlanExpr::Column(ordinal),
        None => match capture_outer(expr, parent, binder)? {
            Some(expr) => expr,
            None => return Ok(None),
        },
    };
    Ok(Some(PlanExpr::Outer(
        binder.capture(scope.depth, in_parent),
    )))
}

// ------------------------------------------------------------ window functions
//
// AHL-494: `OVER (PARTITION BY ... ORDER BY ... frame)`, `WINDOW name AS
// (...)` and `FILTER (WHERE ...)` on an aggregate. Every rule below was
// checked against a real sqlite3 3.54 binary rather than assumed — see the
// window-functions sqllogictest file for the queries and
// `crates/inlaysql-core/src/plan.rs`'s `WindowFrame` doc for the default-frame
// measurement in particular, which is the one that silently changes an answer
// if it is wrong.

/// One `WINDOW name AS (...)` definition, or the merged result of an inline
/// `OVER (...)` that extended one via its own leading name — both are the
/// same [`WindowSpec`] shape, so [`resolve_window_spec`] resolves both into
/// this.
#[derive(Debug, Clone)]
struct NamedWindow {
    partition_by: Vec<PlanExpr>,
    partition_collations: Vec<Collation>,
    order_by: Vec<Order>,
    /// `None` means this window specifies no frame of its own, which is what
    /// lets an extension add one; `Some` only from an explicit frame clause.
    /// The *default* frame ([`WindowFrame::whole_partition`] /
    /// [`WindowFrame::default_range`]) is chosen later, once a call site's
    /// `order_by` is known — not stored here.
    frame: Option<WindowFrame>,
}

/// Resolve every `WINDOW name AS (...)` a `SELECT` declares, in order, into
/// [`Binder::named_windows`] — later definitions may reference earlier ones
/// by name (`WINDOW b AS (a ORDER BY x)`), matching sqlite3's own order of
/// resolution.
fn resolve_named_windows(
    defs: &[NamedWindowDefinition],
    scope: &Scope,
    binder: &mut Binder,
) -> Result<()> {
    for NamedWindowDefinition(name, expr) in defs {
        let spec = match expr {
            NamedWindowExpr::WindowSpec(spec) => spec,
            // `WINDOW b AS a` — a bare reference to another named window,
            // with no parentheses at all. Not SQLite's grammar (`WINDOW`
            // always takes a parenthesised specification there), so this is
            // refused rather than guessed at.
            NamedWindowExpr::NamedWindow(_) => {
                return Err(Error::Unsupported(alloc::format!(
                    "WINDOW {} AS <name> is not supported; give a full window specification in \
                     parentheses",
                    name.value
                )));
            }
        };
        let resolved = resolve_window_spec(spec, scope, binder)?;
        binder.named_windows.push((name.value.clone(), resolved));
    }
    Ok(())
}

/// The named window `name` refers to, or "no such window" — sqlite3's own
/// wording for both a bare `OVER name` and an inline `OVER (name ...)`.
fn find_named_window<'b>(binder: &'b Binder, name: &str) -> Result<&'b NamedWindow> {
    binder
        .named_windows
        .iter()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        .map(|(_, window)| window)
        .ok_or_else(|| Error::Catalog(alloc::format!("no such window: {name}")))
}

/// Resolve one [`WindowSpec`] — a `WINDOW name AS (...)` definition or an
/// inline `OVER (...)` clause, which share this exact shape — merging it with
/// the named window its own leading name refers to, if any.
///
/// The merge rule is sqlite3 3.54's, confirmed directly: an extension may add
/// `PARTITION BY` **never**, however it is spelled (even onto a base with
/// none), `ORDER BY` and a frame each **only when the base has none of its
/// own** — every other combination is "cannot override ... clause of window:
/// `name`" there, and here.
fn resolve_window_spec(
    spec: &WindowSpec,
    scope: &Scope,
    binder: &mut Binder,
) -> Result<NamedWindow> {
    let base = match &spec.window_name {
        Some(name) => Some(find_named_window(binder, &name.value)?.clone()),
        None => None,
    };
    let base_name = || {
        spec.window_name
            .as_ref()
            .map(|n| n.value.as_str())
            .unwrap_or("")
    };

    if !spec.partition_by.is_empty() && spec.window_name.is_some() {
        return Err(Error::Unsupported(alloc::format!(
            "cannot override the PARTITION BY clause of window: {}",
            base_name()
        )));
    }
    let (partition_by, partition_collations) = if !spec.partition_by.is_empty() {
        resolve_window_partition_by(&spec.partition_by, scope, binder)?
    } else if let Some(base) = &base {
        (base.partition_by.clone(), base.partition_collations.clone())
    } else {
        (Vec::new(), Vec::new())
    };

    if !spec.order_by.is_empty() {
        if let Some(base) = &base {
            if !base.order_by.is_empty() {
                return Err(Error::Unsupported(alloc::format!(
                    "cannot override the ORDER BY clause of window: {}",
                    base_name()
                )));
            }
        }
    }
    let order_by = if !spec.order_by.is_empty() {
        resolve_window_order_by(&spec.order_by, scope, binder)?
    } else if let Some(base) = &base {
        base.order_by.clone()
    } else {
        Vec::new()
    };

    if spec.window_frame.is_some() {
        if let Some(base) = &base {
            if base.frame.is_some() {
                return Err(Error::Unsupported(alloc::format!(
                    "cannot override the frame specification of window: {}",
                    base_name()
                )));
            }
        }
    }
    let frame = match &spec.window_frame {
        Some(raw) => Some(resolve_window_frame(raw, &order_by, scope, binder)?),
        None => base.as_ref().and_then(|base| base.frame.clone()),
    };

    Ok(NamedWindow {
        partition_by,
        partition_collations,
        order_by,
        frame,
    })
}

/// `PARTITION BY`, resolved the same single-operand-collation way `GROUP BY`
/// is (`resolve_group_by`) — grouping is grouping, whether a `GROUP BY` key
/// or a window's partition key, and a `NOCASE` column partitions under that
/// collation for the same reason it groups under it (AHL-469).
fn resolve_window_partition_by(
    exprs: &[Expr],
    scope: &Scope,
    binder: &mut Binder,
) -> Result<(Vec<PlanExpr>, Vec<Collation>)> {
    let mut resolved = Vec::with_capacity(exprs.len());
    let mut collations = Vec::with_capacity(exprs.len());
    for expr in exprs {
        let before_windows = binder.windows.len();
        let key = resolve_expr(expr, scope, binder)?;
        if binder.windows.len() != before_windows {
            return Err(Error::Unsupported(
                "a window function may not appear inside another window function's PARTITION BY"
                    .to_string(),
            ));
        }
        collations.push(term_collation(&key, scope, binder));
        resolved.push(key);
    }
    Ok((resolved, collations))
}

/// A window's own `ORDER BY`, reusing [`Order`] — the type `ORDER BY` itself
/// resolves to — but not [`resolve_order_by`]'s positional-integer/
/// output-alias resolution: a window's `ORDER BY` is over the row (or, in an
/// aggregate query, the group), not over the statement's own output columns,
/// so every term is an ordinary expression.
fn resolve_window_order_by(
    terms: &[OrderByExpr],
    scope: &Scope,
    binder: &mut Binder,
) -> Result<Vec<Order>> {
    let mut order = Vec::with_capacity(terms.len());
    for term in terms {
        if term.with_fill.is_some() {
            return Err(Error::Unsupported(
                "ORDER BY ... WITH FILL is not supported".to_string(),
            ));
        }
        let desc = !term.options.asc.unwrap_or(true);
        let nulls_first = term.options.nulls_first.unwrap_or(!desc);
        let (inner, explicit) = peel_collation(&term.expr)?;
        let before_windows = binder.windows.len();
        let key = resolve_expr(inner, scope, binder)?;
        if binder.windows.len() != before_windows {
            return Err(Error::Unsupported(
                "a window function may not appear inside another window function's ORDER BY"
                    .to_string(),
            ));
        }
        let collation = match explicit {
            Some(collation) => collation,
            None => term_collation(&key, scope, binder),
        };
        order.push(Order {
            key: OrderKey::Expr(key),
            collation,
            desc,
            nulls_first,
        });
    }
    Ok(order)
}

/// An explicit frame clause. Only `ROWS` is implemented — `RANGE` and
/// `GROUPS` are refused by name — and the bounds are validated the way
/// sqlite3 validates them: confirmed against a real binary, a frame may not
/// *start* at `UNBOUNDED FOLLOWING`, may not *end* at `UNBOUNDED PRECEDING`,
/// and (category by category — `UNBOUNDED PRECEDING` < `<n> PRECEDING` <
/// `CURRENT ROW` < `<n> FOLLOWING` < `UNBOUNDED FOLLOWING`) may not start in a
/// later category than it ends in. Two bounds in the *same* category (`ROWS
/// BETWEEN 5 PRECEDING AND 2 PRECEDING`) are accepted here exactly as
/// sqlite3 accepts them — whether `5` is really `>= 2` is a question about
/// values, not categories, and is answered at execution (an inverted pair
/// like `2 PRECEDING AND 5 PRECEDING` is not an error there either; it is an
/// always-empty frame, confirmed against sqlite3).
fn resolve_window_frame(
    frame: &sqlparser::ast::WindowFrame,
    order_by: &[Order],
    scope: &Scope,
    binder: &mut Binder,
) -> Result<WindowFrame> {
    let unit = match frame.units {
        sqlparser::ast::WindowFrameUnits::Rows => FrameUnit::Rows,
        sqlparser::ast::WindowFrameUnits::Range => FrameUnit::Range,
        sqlparser::ast::WindowFrameUnits::Groups => FrameUnit::Groups,
    };
    let start = resolve_frame_bound(&frame.start_bound, scope, binder)?;
    // `end_bound = None` is the shorthand form (`ROWS 1 PRECEDING`), which
    // sqlparser's own doc says "must behave the same as `CURRENT ROW`" —
    // confirmed against sqlite3, whose grammar refuses the shorthand
    // altogether when the bound is a `FOLLOWING` one, which the category
    // check below reproduces without special-casing the shorthand: a
    // `FOLLOWING` start against an implied `CURRENT ROW` end is already a
    // later-than-earlier violation.
    let end = match &frame.end_bound {
        Some(bound) => resolve_frame_bound(bound, scope, binder)?,
        None => FrameBound::CurrentRow,
    };
    if matches!(start, FrameBound::UnboundedFollowing) {
        return Err(Error::Unsupported(
            "a window frame may not start at UNBOUNDED FOLLOWING".to_string(),
        ));
    }
    if matches!(end, FrameBound::UnboundedPreceding) {
        return Err(Error::Unsupported(
            "a window frame may not end at UNBOUNDED PRECEDING".to_string(),
        ));
    }
    if frame_bound_rank(&start) > frame_bound_rank(&end) {
        return Err(Error::Unsupported(
            "a window frame's start must not come after its end".to_string(),
        ));
    }
    // Confirmed against sqlite3: a `RANGE` bound that compares *values*
    // (`<expr> PRECEDING`/`FOLLOWING`, as opposed to `CURRENT ROW` or
    // `UNBOUNDED`, which stay peer-group-based regardless) needs exactly one
    // `ORDER BY` term to compare against — zero terms means no value to
    // offset from, and more than one means no single value the offset could
    // mean. `GROUPS` and `CURRENT ROW`/`UNBOUNDED` `RANGE` bounds have no
    // such restriction: a peer group is well-defined by any number of
    // `ORDER BY` terms, the same rule the implicit default frame already
    // relies on.
    let has_value_offset =
        |bound: &FrameBound| matches!(bound, FrameBound::Preceding(_) | FrameBound::Following(_));
    if unit == FrameUnit::Range
        && (has_value_offset(&start) || has_value_offset(&end))
        && order_by.len() != 1
    {
        return Err(Error::Unsupported(alloc::format!(
            "a RANGE frame with a PRECEDING/FOLLOWING offset needs exactly one ORDER BY term to \
             compare it against, this window has {}",
            order_by.len()
        )));
    }
    Ok(WindowFrame { unit, start, end })
}

/// One `PRECEDING`/`FOLLOWING`/`CURRENT ROW` bound. An offset expression is
/// resolved like any other (a literal or a `?`; window/aggregate nesting is
/// blocked the same way [`resolve_window_partition_by`] blocks it) — whether
/// it is a *non-negative* number is a question the value cannot answer until
/// execution, so that check is in `engine.rs`.
fn resolve_frame_bound(
    bound: &sqlparser::ast::WindowFrameBound,
    scope: &Scope,
    binder: &mut Binder,
) -> Result<FrameBound> {
    Ok(match bound {
        sqlparser::ast::WindowFrameBound::CurrentRow => FrameBound::CurrentRow,
        sqlparser::ast::WindowFrameBound::Preceding(None) => FrameBound::UnboundedPreceding,
        sqlparser::ast::WindowFrameBound::Following(None) => FrameBound::UnboundedFollowing,
        sqlparser::ast::WindowFrameBound::Preceding(Some(expr)) => {
            FrameBound::Preceding(Box::new(resolve_frame_offset(expr, scope, binder)?))
        }
        sqlparser::ast::WindowFrameBound::Following(Some(expr)) => {
            FrameBound::Following(Box::new(resolve_frame_offset(expr, scope, binder)?))
        }
    })
}

/// A frame bound's `<expr>` in `<expr> PRECEDING`/`<expr> FOLLOWING`.
fn resolve_frame_offset(expr: &Expr, scope: &Scope, binder: &mut Binder) -> Result<PlanExpr> {
    let before_windows = binder.windows.len();
    let resolved = resolve_expr(expr, scope, binder)?;
    if binder.windows.len() != before_windows {
        return Err(Error::Unsupported(
            "a window function may not appear inside a window frame bound".to_string(),
        ));
    }
    Ok(resolved)
}

/// The ordering [`resolve_window_frame`]'s validity check compares bounds by:
/// `UNBOUNDED PRECEDING` furthest left, `UNBOUNDED FOLLOWING` furthest right,
/// `CURRENT ROW` in the middle, `PRECEDING`/`FOLLOWING` either side of it —
/// same-category pairs (two `PRECEDING`s or two `FOLLOWING`s) rank equal,
/// which is exactly what lets them both through here regardless of which
/// literal is larger (see [`resolve_window_frame`]'s doc).
fn frame_bound_rank(bound: &FrameBound) -> i8 {
    match bound {
        FrameBound::UnboundedPreceding => -1,
        FrameBound::Preceding(_) => 0,
        FrameBound::CurrentRow => 1,
        FrameBound::Following(_) => 2,
        FrameBound::UnboundedFollowing => 3,
    }
}

/// Recognise a window function call (`func(...) OVER (...)`), resolving it
/// into [`PlanExpr::Window`]. Returns `None` for anything with no `OVER`
/// clause at all, so aggregate and scalar resolution still see a plain call —
/// mirroring [`resolve_aggregate`]'s own `None` convention.
fn resolve_window_function(
    function: &sqlparser::ast::Function,
    scope: &Scope,
    binder: &mut Binder,
) -> Result<Option<usize>> {
    let Some(over) = &function.over else {
        return Ok(None);
    };
    let name = object_name(&function.name)?;
    let lower = name.to_ascii_lowercase();

    let func = match lower.as_str() {
        "row_number" => WindowFunc::RowNumber,
        "rank" => WindowFunc::Rank,
        "dense_rank" => WindowFunc::DenseRank,
        "ntile" => WindowFunc::Ntile,
        "lag" => WindowFunc::Lag,
        "lead" => WindowFunc::Lead,
        "first_value" => WindowFunc::FirstValue,
        "last_value" => WindowFunc::LastValue,
        "nth_value" => WindowFunc::NthValue,
        "count" => WindowFunc::Agg(AggFunc::Count),
        "sum" => WindowFunc::Agg(AggFunc::Sum),
        "min" => WindowFunc::Agg(AggFunc::Min),
        "max" => WindowFunc::Agg(AggFunc::Max),
        "avg" => WindowFunc::Agg(AggFunc::Avg),
        "group_concat" => WindowFunc::Agg(AggFunc::GroupConcat),
        "percent_rank" => WindowFunc::PercentRank,
        "cume_dist" => WindowFunc::CumeDist,
        _ => {
            return Err(Error::Unsupported(alloc::format!(
                "{name}() may not be used as a window function"
            )))
        }
    };

    if function.null_treatment.is_some() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() IGNORE NULLS / RESPECT NULLS is not supported"
        )));
    }
    if !function.within_group.is_empty() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() WITHIN GROUP is not supported"
        )));
    }
    if let Some(filter) = &function.filter {
        let _ = filter;
        if !matches!(func, WindowFunc::Agg(_)) {
            return Err(Error::Unsupported(
                "FILTER clause may only be used with aggregate window functions".to_string(),
            ));
        }
    }

    let FunctionArguments::List(list) = &function.args else {
        return Err(Error::Type(alloc::format!(
            "{name}() requires an argument list"
        )));
    };
    if list.duplicate_treatment.is_some() {
        return Err(Error::Unsupported(
            "DISTINCT is not supported for window functions".to_string(),
        ));
    }
    if !list.clauses.is_empty() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() with an ORDER BY / LIMIT clause inside the call is not supported"
        )));
    }
    let raw_args = list.args.as_slice();

    // A window function may not appear inside another window function's own
    // arguments or `FILTER` — confirmed against sqlite3 3.54 ("misuse of
    // window function"), the same rule `WHERE`/`GROUP BY`/`HAVING` enforce.
    let before_windows = binder.windows.len();

    let args = match func {
        WindowFunc::RowNumber
        | WindowFunc::Rank
        | WindowFunc::DenseRank
        | WindowFunc::PercentRank
        | WindowFunc::CumeDist => {
            if !raw_args.is_empty() {
                return Err(arity_error(&name, 0, raw_args.len()));
            }
            Vec::new()
        }
        WindowFunc::Ntile => {
            let [n] = raw_args else {
                return Err(arity_error(&name, 1, raw_args.len()));
            };
            alloc::vec![resolve_expr(unnamed_arg(&name, n)?, scope, binder)?]
        }
        WindowFunc::Lag | WindowFunc::Lead => {
            if raw_args.is_empty() || raw_args.len() > 3 {
                return Err(Error::Unsupported(alloc::format!(
                    "{name}() takes 1 to 3 arguments, got {}",
                    raw_args.len()
                )));
            }
            let mut resolved = Vec::with_capacity(raw_args.len());
            for arg in raw_args {
                resolved.push(resolve_expr(unnamed_arg(&name, arg)?, scope, binder)?);
            }
            resolved
        }
        WindowFunc::FirstValue | WindowFunc::LastValue => {
            let [value] = raw_args else {
                return Err(arity_error(&name, 1, raw_args.len()));
            };
            alloc::vec![resolve_expr(unnamed_arg(&name, value)?, scope, binder)?]
        }
        WindowFunc::NthValue => {
            let [value, n] = raw_args else {
                return Err(arity_error(&name, 2, raw_args.len()));
            };
            alloc::vec![
                resolve_expr(unnamed_arg(&name, value)?, scope, binder)?,
                resolve_expr(unnamed_arg(&name, n)?, scope, binder)?,
            ]
        }
        WindowFunc::Agg(AggFunc::Count) => {
            let [arg] = raw_args else {
                return Err(arity_error(&name, 1, raw_args.len()));
            };
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => Vec::new(),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                    alloc::vec![resolve_expr(expr, scope, binder)?]
                }
                other => {
                    return Err(Error::Unsupported(alloc::format!(
                        "{name}() argument `{other}` is not supported"
                    )))
                }
            }
        }
        WindowFunc::Agg(AggFunc::GroupConcat) => {
            let (value, separator) = match raw_args {
                [value] => (value, None),
                [value, separator] => (value, Some(separator)),
                _ => return Err(arity_error(&name, 1, raw_args.len())),
            };
            let mut resolved =
                alloc::vec![resolve_expr(unnamed_arg(&name, value)?, scope, binder)?];
            if let Some(separator) = separator {
                resolved.push(resolve_expr(unnamed_arg(&name, separator)?, scope, binder)?);
            }
            resolved
        }
        WindowFunc::Agg(_) => {
            let [arg] = raw_args else {
                return Err(arity_error(&name, 1, raw_args.len()));
            };
            alloc::vec![resolve_expr(unnamed_arg(&name, arg)?, scope, binder)?]
        }
    };

    let filter = function
        .filter
        .as_ref()
        .map(|filter| resolve_expr(filter, scope, binder))
        .transpose()?;

    if binder.windows.len() != before_windows {
        return Err(Error::Unsupported(alloc::format!(
            "a window function may not appear inside {name}()'s own arguments or FILTER"
        )));
    }

    let resolved_over = match over {
        WindowType::WindowSpec(spec) => resolve_window_spec(spec, scope, binder)?,
        WindowType::NamedWindow(ident) => find_named_window(binder, &ident.value)?.clone(),
    };

    // `MIN`/`MAX` order their values, so the aggregate family needs the same
    // collation resolution `resolve_aggregate` gives its own argument.
    let collation = args
        .first()
        .map_or(Collation::Binary, |arg| term_collation(arg, scope, binder));

    // SQLite's implicit default frame (`WindowFrame`'s doc has the
    // measurement): the whole partition with no `ORDER BY`, otherwise
    // `UNBOUNDED PRECEDING` to the current row's peer group.
    let frame = match resolved_over.frame {
        Some(frame) => frame,
        None if resolved_over.order_by.is_empty() => WindowFrame::whole_partition(),
        None => WindowFrame::default_range(),
    };

    let index = binder.windows.len();
    binder.windows.push(WindowFn {
        func,
        args,
        filter,
        partition_by: resolved_over.partition_by,
        partition_collations: resolved_over.partition_collations,
        order_by: resolved_over.order_by,
        frame,
        collation,
    });
    Ok(Some(index))
}

/// Recognise an aggregate function call, resolving it into [`PlanExpr::Agg`].
///
/// Returns `None` for anything that is not an aggregate function, so the caller
/// can decide how to reject it — or, for `min`/`max`, resolve it as the scalar
/// function of the same name instead.
fn resolve_aggregate(
    function: &sqlparser::ast::Function,
    scope: &Scope,
    binder: &mut Binder,
) -> Result<Option<usize>> {
    let name = object_name(&function.name)?;
    let func = if name.eq_ignore_ascii_case("count") {
        Some(AggFunc::Count)
    } else if name.eq_ignore_ascii_case("sum") {
        Some(AggFunc::Sum)
    } else if name.eq_ignore_ascii_case("min") {
        Some(AggFunc::Min)
    } else if name.eq_ignore_ascii_case("max") {
        Some(AggFunc::Max)
    } else if name.eq_ignore_ascii_case("avg") {
        Some(AggFunc::Avg)
    } else if name.eq_ignore_ascii_case("group_concat") {
        Some(AggFunc::GroupConcat)
    } else {
        None
    };
    let Some(func) = func else {
        return Ok(None);
    };

    let FunctionArguments::List(list) = &function.args else {
        // `min`/`max` written with no argument list at all is not an
        // aggregate; let the scalar resolver produce the arity error.
        if matches!(func, AggFunc::Min | AggFunc::Max) {
            return Ok(None);
        }
        return Err(Error::Type(alloc::format!(
            "{name}() requires an argument list"
        )));
    };
    let args = list.args.as_slice();

    // `min(a, b)` is the scalar function, not the aggregate. SQLite decides
    // the same way — by arity — and the two mean genuinely different things
    // for `NULL`, so guessing would be a wrong answer rather than an error.
    if matches!(func, AggFunc::Min | AggFunc::Max)
        && args.len() != 1
        && list.duplicate_treatment.is_none()
    {
        return Ok(None);
    }

    // `resolve_window_function` claims every call with an `OVER` clause
    // before this is ever reached (see the dispatch in `resolve_expr`), so
    // `over` is always `None` here — checked anyway, defensively, since a
    // wrong dispatch order would otherwise plan `sum(x) OVER (...)` as a
    // plain `GROUP BY` aggregate and silently drop the `OVER`, which is
    // exactly the bug class `AGENTS.md` warns about.
    if function.over.is_some() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() OVER (...) was not resolved as a window function"
        )));
    }
    if !function.within_group.is_empty() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() WITHIN GROUP is not supported"
        )));
    }
    if !list.clauses.is_empty() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() with an ORDER BY / LIMIT clause inside the call is not supported"
        )));
    }

    let distinct = match list.duplicate_treatment {
        Some(DuplicateTreatment::Distinct) => true,
        Some(DuplicateTreatment::All) | None => false,
    };
    // SQLite's rule, and it exists for a reason: `group_concat(DISTINCT x, y)`
    // has no defined answer, because the separator is not part of what is
    // being deduplicated.
    if distinct && args.len() != 1 {
        return Err(Error::Unsupported(alloc::format!(
            "{name}(DISTINCT ...) takes exactly one argument"
        )));
    }

    // A window function may not appear inside a plain aggregate's own
    // argument list — confirmed against sqlite3 3.54 ("misuse of window
    // function"), the same rule that keeps a window function out of `WHERE`/
    // `GROUP BY` — so every argument and the `FILTER` predicate below are
    // watched for one leaking in.
    let before_windows = binder.windows.len();

    let (arg, separator) = match func {
        AggFunc::Count => {
            let [arg] = args else {
                return Err(arity_error(&name, 1, args.len()));
            };
            let arg = match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {
                    if distinct {
                        return Err(Error::Unsupported(
                            "COUNT(DISTINCT *) is not supported; name a column".to_string(),
                        ));
                    }
                    None
                }
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                    Some(resolve_expr(expr, scope, binder)?)
                }
                other => {
                    return Err(Error::Unsupported(alloc::format!(
                        "{name}() argument `{other}` is not supported"
                    )))
                }
            };
            (arg, None)
        }
        AggFunc::GroupConcat => {
            let (value, separator) = match args {
                [value] => (value, None),
                [value, separator] => (value, Some(separator)),
                _ => return Err(arity_error(&name, 1, args.len())),
            };
            let value = resolve_expr(unnamed_arg(&name, value)?, scope, binder)?;
            let separator = separator
                .map(|arg| resolve_expr(unnamed_arg(&name, arg)?, scope, binder))
                .transpose()?;
            (Some(value), separator)
        }
        _ => {
            let [arg] = args else {
                return Err(arity_error(&name, 1, args.len()));
            };
            let arg = resolve_expr(unnamed_arg(&name, arg)?, scope, binder)?;
            (Some(arg), None)
        }
    };

    // `FILTER (WHERE ...)` — accepted on a plain aggregate, not only a
    // window one: confirmed against sqlite3 3.54, `SUM(x) FILTER (WHERE
    // y > 0)` inside an ordinary `GROUP BY` query is not refused there.
    let filter = function
        .filter
        .as_ref()
        .map(|filter| resolve_expr(filter, scope, binder))
        .transpose()?;

    if binder.windows.len() != before_windows {
        return Err(Error::Unsupported(alloc::format!(
            "a window function may not appear inside {name}()'s own arguments or FILTER"
        )));
    }

    // `MIN`/`MAX` order their values and `DISTINCT` folds them, so both need
    // the argument's collation — SQLite flags the aggregate `min`/`max`
    // `SQLITE_FUNC_NEEDCOLL` exactly as it flags the scalar pair.
    let collation = arg
        .as_ref()
        .map_or(Collation::Binary, |arg| term_collation(arg, scope, binder));
    let index = binder.aggregates.len();
    binder.aggregates.push(Aggregate {
        func,
        arg,
        distinct,
        separator,
        collation,
        filter,
    });
    Ok(Some(index))
}

/// Resolve a scalar function call, by SQLite's name and arity.
///
/// An unknown name is an error rather than a `NULL`: a query that calls a
/// function this engine does not have is a query whose author expected
/// something to happen, and answering `NULL` would be the same silent lie as a
/// dropped clause.
fn resolve_scalar_function(
    function: &sqlparser::ast::Function,
    scope: &Scope,
    binder: &mut Binder,
) -> Result<PlanExpr> {
    let name = object_name(&function.name)?;
    let lower = name.to_ascii_lowercase();
    let func = match lower.as_str() {
        "length" => ScalarFunc::Length,
        "upper" => ScalarFunc::Upper,
        "lower" => ScalarFunc::Lower,
        // SQLite registers both spellings for the same function.
        "substr" | "substring" => ScalarFunc::Substr,
        "trim" => ScalarFunc::Trim,
        "ltrim" => ScalarFunc::LTrim,
        "rtrim" => ScalarFunc::RTrim,
        "replace" => ScalarFunc::Replace,
        "instr" => ScalarFunc::Instr,
        "abs" => ScalarFunc::Abs,
        "round" => ScalarFunc::Round,
        "coalesce" => ScalarFunc::Coalesce,
        "ifnull" => ScalarFunc::IfNull,
        "nullif" => ScalarFunc::NullIf,
        "min" => ScalarFunc::Min,
        "max" => ScalarFunc::Max,
        "hex" => ScalarFunc::Hex,
        "octet_length" => ScalarFunc::OctetLength,
        "unhex" => ScalarFunc::Unhex,
        "random" => ScalarFunc::Random,
        "date" => ScalarFunc::Date,
        "time" => ScalarFunc::Time,
        "datetime" => ScalarFunc::DateTime,
        "strftime" => ScalarFunc::Strftime,
        "unixepoch" => ScalarFunc::UnixEpoch,
        "current_timestamp" => ScalarFunc::CurrentTimestamp,
        "current_date" => ScalarFunc::CurrentDate,
        "current_time" => ScalarFunc::CurrentTime,
        // Shim-target-only primitives (AHL-465): MySQL behaviours with no
        // SQLite spelling, named so a plain SQL statement is never mistaken
        // for the SQLite dialect. `crates/inlaysql-server`'s shim is the
        // only intended caller; nothing here refuses a direct call, because
        // this project refuses clauses rather than functions it has.
        "mysql_substr" => ScalarFunc::MysqlSubstr,
        "mysql_hex" => ScalarFunc::MysqlHex,
        "mysql_nullif" => ScalarFunc::MysqlNullIf,
        "mysql_round" => ScalarFunc::MysqlRound,
        // SQLite's json1 functions (AHL-490). `json_each`/`json_tree` are
        // table-valued — this engine has no mechanism for a function that
        // returns rows in `FROM`, refused by name where `FROM` items are
        // resolved — and `json_patch` is not implemented at all, so neither
        // name appears here; both reach an ordinary "no such function"
        // refusal, pinned in `unsupported.test`.
        "json" => ScalarFunc::Json,
        "json_extract" => ScalarFunc::JsonExtract,
        "json_valid" => ScalarFunc::JsonValid,
        "json_type" => ScalarFunc::JsonType,
        "json_quote" => ScalarFunc::JsonQuote,
        "json_array" => ScalarFunc::JsonArray,
        "json_object" => ScalarFunc::JsonObject,
        "json_array_length" => ScalarFunc::JsonArrayLength,
        "json_set" => ScalarFunc::JsonSet,
        "json_insert" => ScalarFunc::JsonInsert,
        "json_replace" => ScalarFunc::JsonReplace,
        "json_remove" => ScalarFunc::JsonRemove,
        _ => {
            return Err(Error::Unsupported(alloc::format!(
                "no such function: {name}"
            )))
        }
    };

    // `resolve_window_function` claims every call with an `OVER` clause
    // first, so `over` is always `None` by the time a scalar function
    // reaches here — see the equivalent defensive check in
    // `resolve_aggregate`.
    if function.over.is_some() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() OVER (...) was not resolved as a window function"
        )));
    }
    if function.filter.is_some() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() FILTER (WHERE ...) is not supported; FILTER only applies to an aggregate"
        )));
    }
    if !function.within_group.is_empty() {
        return Err(Error::Unsupported(alloc::format!(
            "{name}() WITHIN GROUP is not supported"
        )));
    }

    let raw = match &function.args {
        // `CURRENT_TIMESTAMP` and friends are spelled without parentheses.
        FunctionArguments::None => &[][..],
        FunctionArguments::List(list) => {
            if list.duplicate_treatment.is_some() {
                return Err(Error::Unsupported(alloc::format!(
                    "{name}(DISTINCT ...) is not an aggregate"
                )));
            }
            if !list.clauses.is_empty() {
                return Err(Error::Unsupported(alloc::format!(
                    "{name}() with a clause inside the call is not supported"
                )));
            }
            list.args.as_slice()
        }
        FunctionArguments::Subquery(_) => {
            return Err(Error::Unsupported(alloc::format!(
                "{name}() over a subquery is not supported"
            )))
        }
    };

    // Arity, checked here so the evaluator can trust the shapes it is handed.
    let (least, most) = match func {
        ScalarFunc::Length
        | ScalarFunc::Upper
        | ScalarFunc::Lower
        | ScalarFunc::Abs
        | ScalarFunc::Hex
        | ScalarFunc::OctetLength
        | ScalarFunc::Unhex
        | ScalarFunc::MysqlHex => (1, Some(1)),
        ScalarFunc::Substr | ScalarFunc::MysqlSubstr => (2, Some(3)),
        ScalarFunc::Trim
        | ScalarFunc::LTrim
        | ScalarFunc::RTrim
        | ScalarFunc::Round
        | ScalarFunc::MysqlRound => (1, Some(2)),
        ScalarFunc::Replace => (3, Some(3)),
        ScalarFunc::Instr | ScalarFunc::IfNull | ScalarFunc::NullIf | ScalarFunc::MysqlNullIf => {
            (2, Some(2))
        }
        ScalarFunc::Coalesce | ScalarFunc::Min | ScalarFunc::Max => (2, None),
        ScalarFunc::Random
        | ScalarFunc::CurrentTimestamp
        | ScalarFunc::CurrentDate
        | ScalarFunc::CurrentTime => (0, Some(0)),
        ScalarFunc::Date | ScalarFunc::Time | ScalarFunc::DateTime | ScalarFunc::UnixEpoch => {
            (0, None)
        }
        ScalarFunc::Strftime => (1, None),
        ScalarFunc::Json | ScalarFunc::JsonValid | ScalarFunc::JsonQuote => (1, Some(1)),
        // `json_extract` wants a document and at least one path — checked
        // against sqlite3, `json_extract(x)` alone is a wrong-argument-count
        // error there too.
        ScalarFunc::JsonExtract => (2, None),
        ScalarFunc::JsonType | ScalarFunc::JsonArrayLength => (1, Some(2)),
        ScalarFunc::JsonArray | ScalarFunc::JsonObject => (0, None),
        // `json_set`/`json_insert`/`json_replace` take a document alone (a
        // no-op) or a document plus one or more `(path, value)` pairs — an
        // odd total, checked below.
        ScalarFunc::JsonSet | ScalarFunc::JsonInsert | ScalarFunc::JsonReplace => (1, None),
        // `json_remove` takes a document and any number of paths, including
        // none — checked against sqlite3, `json_remove(x)` is `x` unchanged.
        ScalarFunc::JsonRemove => (1, None),
    };
    if raw.len() < least || most.is_some_and(|most| raw.len() > most) {
        return Err(Error::Type(match most {
            Some(most) if most == least => {
                alloc::format!("{name}() takes {least} argument(s), got {}", raw.len())
            }
            Some(most) => alloc::format!(
                "{name}() takes between {least} and {most} arguments, got {}",
                raw.len()
            ),
            None => alloc::format!(
                "{name}() takes at least {least} argument(s), got {}",
                raw.len()
            ),
        }));
    }
    // `json_object`'s labels pair with values, and `json_set`/`json_insert`/
    // `json_replace`'s document pairs with `(path, value)` — both need an
    // exact parity beyond the plain range check above, checked against
    // sqlite3's own wording ("json_object() requires an even number of
    // arguments", "json_set() needs an odd number of arguments").
    if matches!(func, ScalarFunc::JsonObject) && raw.len() % 2 != 0 {
        return Err(Error::Type(alloc::format!(
            "{name}() requires an even number of arguments"
        )));
    }
    if matches!(
        func,
        ScalarFunc::JsonSet | ScalarFunc::JsonInsert | ScalarFunc::JsonReplace
    ) && raw.len() % 2 == 0
    {
        return Err(Error::Type(alloc::format!(
            "{name}() needs an odd number of arguments"
        )));
    }

    let mut args = Vec::with_capacity(raw.len());
    for arg in raw {
        args.push(resolve_expr(unnamed_arg(&name, arg)?, scope, binder)?);
    }
    // Only `nullif`, `min` and `max` read this, but resolving it for every
    // call keeps one rule in one place — and it is the same walk the operand
    // of the call already had to do.
    let collation = func_collation(&args, scope, binder);
    Ok(PlanExpr::Func {
        func,
        args,
        collation,
    })
}

/// Recognise a retrieval expression, or return `None` for ordinary expressions.
fn resolve_score_expr(
    expr: &Expr,
    scope: &Scope,
    binder: &mut Binder,
) -> Result<Option<ScoreExpr>> {
    let Expr::Function(function) = expr else {
        return Ok(None);
    };
    let name = object_name(&function.name)?;
    let args = match &function.args {
        FunctionArguments::List(list) => list.args.as_slice(),
        _ => return Ok(None),
    };

    // A literal query is checked against the column now; a `?` can only be
    // checked once it is bound, which the executor does on every execution.
    let resolved = if name.eq_ignore_ascii_case("vector_score") {
        let (column, query) = expect_two(&name, args)?;
        let (index, column) = resolve_retrieval_column(column, scope)?;
        let Some(dim) = column.ty.vector_dim() else {
            return Err(Error::Type(alloc::format!(
                "vector_score() needs a VECTOR column, but `{}` is {}",
                column.name,
                column.ty
            )));
        };
        let query = bind_value(query, binder)?;
        binder.pin_vector_param(&query, dim);
        if let PlanExpr::Literal(literal) = &query {
            let Value::Vector(embedding) = literal else {
                return Err(Error::Type(
                    "vector_score() needs an embedding as its second argument".to_string(),
                ));
            };
            if embedding.len() != dim {
                return Err(Error::Type(alloc::format!(
                    "query embedding has dimension {} but column `{}` is VECTOR({dim})",
                    embedding.len(),
                    column.name
                )));
            }
        }
        ScoreExpr::Vector {
            column: index,
            query,
        }
    } else if name.eq_ignore_ascii_case("bm25_score") {
        // `bm25_score(column, 'terms')` is the single-column call this has
        // always accepted; `bm25_score(a, b, ..., 'terms')` is its
        // multi-column extension — every argument but the last names a
        // column, MySQL's `MATCH(a, b, ...)`, and the last is the query. The
        // two-argument case is exactly the one-column instance of this, so
        // nothing about it changes.
        let (query, column_args) = args.split_last().ok_or_else(|| arity_error(&name, 2, 0))?;
        if column_args.is_empty() {
            return Err(Error::Type(alloc::format!(
                "bm25_score() takes at least 2 arguments (one or more TEXT columns, then the \
                 query), got {}",
                args.len()
            )));
        }
        let mut columns = Vec::with_capacity(column_args.len());
        for column in column_args {
            let column = unnamed_arg(&name, column)?;
            let (index, column) = resolve_retrieval_column(column, scope)?;
            if column.ty != DataType::Text {
                return Err(Error::Type(alloc::format!(
                    "bm25_score() needs a TEXT column, but `{}` is {}",
                    column.name,
                    column.ty
                )));
            }
            columns.push(index);
        }
        let query = unnamed_arg(&name, query)?;
        let query = bind_value(query, binder)?;
        if matches!(&query, PlanExpr::Literal(literal) if !matches!(literal, Value::Text(_))) {
            return Err(Error::Type(
                "bm25_score() needs a text query as its final argument".to_string(),
            ));
        }
        ScoreExpr::Text { columns, query }
    } else if name.eq_ignore_ascii_case("fuse") || name.eq_ignore_ascii_case("rrf") {
        let mut parts = Vec::with_capacity(args.len());
        for arg in args {
            let arg = unnamed_arg(&name, arg)?;
            let part = resolve_score_expr(arg, scope, binder)?.ok_or_else(|| {
                Error::Type(alloc::format!(
                    "fuse() arguments must be retrieval expressions, got `{arg}`"
                ))
            })?;
            parts.push(part);
        }
        if parts.len() < 2 {
            return Err(Error::Type(
                "fuse() needs at least two retrieval expressions".to_string(),
            ));
        }
        ScoreExpr::Fuse {
            parts,
            k: DEFAULT_RRF_K,
        }
    } else {
        return Ok(None);
    };

    Ok(Some(resolved))
}

fn expect_one<'a>(name: &str, args: &'a [FunctionArg]) -> Result<&'a Expr> {
    let [arg] = args else {
        return Err(arity_error(name, 1, args.len()));
    };
    unnamed_arg(name, arg)
}

fn expect_two<'a>(name: &str, args: &'a [FunctionArg]) -> Result<(&'a Expr, &'a Expr)> {
    let [first, second] = args else {
        return Err(arity_error(name, 2, args.len()));
    };
    Ok((unnamed_arg(name, first)?, unnamed_arg(name, second)?))
}

fn arity_error(name: &str, expected: usize, got: usize) -> Error {
    Error::Type(alloc::format!(
        "{name}() takes {expected} argument(s), got {got}"
    ))
}

fn unnamed_arg<'a>(name: &str, arg: &'a FunctionArg) -> Result<&'a Expr> {
    match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Ok(expr),
        other => Err(Error::Unsupported(alloc::format!(
            "{name}() does not accept the argument `{other}`"
        ))),
    }
}

/// Resolve a retrieval function's column against the driving table.
///
/// Retrieval indexes live over one table's rows, so a joined query may only
/// score the driving table. A reference to any other table's column is rejected
/// here rather than discovered at execution.
fn resolve_retrieval_column<'a>(expr: &Expr, scope: &'a Scope<'_>) -> Result<(usize, &'a Column)> {
    let driving = &scope.sources[0];
    if driving.derived.is_some() {
        return Err(Error::Unsupported(
            "a retrieval function needs a stored table; it cannot score a subquery's rows"
                .to_string(),
        ));
    }
    let driving = &driving.table;

    // The common mistake: a qualified reference to a non-driving table.
    if let Expr::CompoundIdentifier(parts) = expr {
        if let [qualifier, _] = parts.as_slice() {
            if let Some(source) = scope.source(&qualifier.value) {
                if source != 0 {
                    return Err(Error::Unsupported(alloc::format!(
                        "retrieval functions may only reference the driving table in a join; \
                         `{}` is not it",
                        qualifier.value
                    )));
                }
            }
        }
    }

    let ordinal = resolve_column_ref(expr, scope)?;
    if ordinal >= driving.columns.len() {
        return Err(Error::Unsupported(
            "retrieval functions may only reference the driving table in a join".to_string(),
        ));
    }
    Ok((ordinal, &driving.columns[ordinal]))
}

fn resolve_order_by(
    order_by: Option<&OrderBy>,
    scope: &Scope,
    items: &[SelectItem],
    binder: &mut Binder,
) -> Result<Vec<Order>> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    let OrderByKind::Expressions(exprs) = &order_by.kind else {
        return Err(Error::Unsupported(
            "ORDER BY ALL is not supported".to_string(),
        ));
    };
    if let Some(interpolate) = &order_by.interpolate {
        let _ = interpolate;
        return Err(Error::Unsupported(
            "ORDER BY ... INTERPOLATE is not supported".to_string(),
        ));
    }

    let mut order = Vec::with_capacity(exprs.len());
    for term in exprs {
        if term.with_fill.is_some() {
            return Err(Error::Unsupported(
                "ORDER BY ... WITH FILL is not supported".to_string(),
            ));
        }
        let desc = !term.options.asc.unwrap_or(true);
        // SQLite sorts `NULL` below every value, so the default placement
        // follows the direction; `NULLS FIRST`/`NULLS LAST` overrides it.
        let nulls_first = term.options.nulls_first.unwrap_or(!desc);
        // A `COLLATE` written directly on the term is peeled off first, so
        // that `ORDER BY name COLLATE NOCASE` still resolves `name` against
        // the *result columns* — SQLite resolves an `ORDER BY` name as an
        // output alias before a table column, and wrapping it in `COLLATE`
        // must not change which one it finds.
        let (inner, explicit) = peel_collation(&term.expr)?;
        let key = resolve_order_key(inner, scope, items, binder)?;
        let collation = match explicit {
            Some(collation) => collation,
            None => order_key_collation(&key, scope, binder),
        };
        order.push(Order {
            key,
            collation,
            desc,
            nulls_first,
        });
    }
    Ok(order)
}

/// Strip a top-level `COLLATE` off an expression, returning what it wrapped
/// and the collation it named.
///
/// Only the outermost one: a `COLLATE` deeper in the expression is part of the
/// expression and is resolved by the ordinary rules.
fn peel_collation(expr: &Expr) -> Result<(&Expr, Option<Collation>)> {
    match expr {
        Expr::Collate { expr, collation } => {
            Ok((expr, Some(Collation::from_name(&object_name(collation)?)?)))
        }
        other => Ok((other, None)),
    }
}

/// What one `ORDER BY` term sorts on.
///
/// SQLite resolves a bare name against the result columns before the table's,
/// and a bare integer against the result columns by position. That positional
/// form is why this cannot simply call [`resolve_expr`]: `ORDER BY 1` means
/// "the first output column", and planning it as the constant `1` would sort
/// by nothing at all while reporting success.
fn resolve_order_key(
    expr: &Expr,
    scope: &Scope,
    items: &[SelectItem],
    binder: &mut Binder,
) -> Result<OrderKey> {
    if let Expr::Value(value) = expr {
        if let AstValue::Number(text, _) = &value.value {
            let Ok(position) = text.parse::<usize>() else {
                return Err(Error::Type(alloc::format!(
                    "ORDER BY `{text}` is not a result column"
                )));
            };
            if position == 0 || position > items.len() {
                return Err(Error::Catalog(alloc::format!(
                    "ORDER BY position {position} is not between 1 and {}",
                    items.len()
                )));
            }
            return Ok(item_key(&items[position - 1]));
        }
    }

    // A bare name resolves like SQLite: the retrieval-score alias, then a
    // projection alias, then a column.
    if let Expr::Identifier(ident) = expr {
        let name = ident.value.clone();

        if items.iter().any(
            |item| matches!(item, SelectItem::Score { label } if label.eq_ignore_ascii_case(&name)),
        ) {
            return Ok(OrderKey::Score);
        }

        for item in items {
            let label = match item {
                SelectItem::Column { label, .. } | SelectItem::Expr { label, .. } => label,
                SelectItem::Score { .. } => continue,
            };
            if label.eq_ignore_ascii_case(&name) {
                return Ok(item_key(item));
            }
        }

        if let Ok(index) = resolve_column_ref(expr, scope) {
            return Ok(OrderKey::Column(index));
        }

        return Err(Error::Catalog(alloc::format!(
            "no such column or alias `{name}`"
        )));
    }

    // Otherwise, order by a scalar expression over the row.
    Ok(OrderKey::Expr(resolve_expr(expr, scope, binder)?))
}

/// `ORDER BY` over a compound query (`UNION`/`INTERSECT`/`EXCEPT`) — stricter
/// than [`resolve_order_by`], and deliberately not built on it.
///
/// SQLite accepts only an output label or a 1-based ordinal here, confirmed
/// against sqlite3: even a bare expression built *only* from an output
/// column's own name is refused — `ORDER BY id + 1` fails with "does not
/// match any column in the result set" even when `id` is itself an output
/// column, which is exactly the case [`resolve_order_key`]'s fallback to an
/// arbitrary expression would otherwise accept (it can resolve `id` against
/// the synthetic scope [`plan_compound`] builds, same as any ordinary
/// `SELECT`). This is why a compound's `ORDER BY` gets its own resolver
/// rather than reusing the general one against that same scope.
fn resolve_compound_order_by(
    order_by: Option<&OrderBy>,
    items: &[SelectItem],
    scope: &Scope<'_>,
    binder: &Binder,
) -> Result<Vec<Order>> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    let OrderByKind::Expressions(exprs) = &order_by.kind else {
        return Err(Error::Unsupported(
            "ORDER BY ALL is not supported".to_string(),
        ));
    };
    if order_by.interpolate.is_some() {
        return Err(Error::Unsupported(
            "ORDER BY ... INTERPOLATE is not supported".to_string(),
        ));
    }

    let mut order = Vec::with_capacity(exprs.len());
    for term in exprs {
        if term.with_fill.is_some() {
            return Err(Error::Unsupported(
                "ORDER BY ... WITH FILL is not supported".to_string(),
            ));
        }
        let desc = !term.options.asc.unwrap_or(true);
        let nulls_first = term.options.nulls_first.unwrap_or(!desc);
        let (inner, explicit) = peel_collation(&term.expr)?;
        let key = compound_order_key(inner, items)?;
        let collation = match explicit {
            Some(collation) => collation,
            None => order_key_collation(&key, scope, binder),
        };
        order.push(Order {
            key,
            collation,
            desc,
            nulls_first,
        });
    }
    Ok(order)
}

/// The only two `ORDER BY` term shapes a compound query accepts: a 1-based
/// ordinal, or a bare name matching an output label. Anything else —
/// including a name that would resolve under the ordinary column-reference
/// rules, and including an expression built only from output columns — is
/// refused; see [`resolve_compound_order_by`].
fn compound_order_key(expr: &Expr, items: &[SelectItem]) -> Result<OrderKey> {
    if let Expr::Value(value) = expr {
        if let AstValue::Number(text, _) = &value.value {
            let Ok(position) = text.parse::<usize>() else {
                return Err(Error::Type(alloc::format!(
                    "ORDER BY `{text}` is not a result column"
                )));
            };
            if position == 0 || position > items.len() {
                return Err(Error::Catalog(alloc::format!(
                    "ORDER BY position {position} is not between 1 and {}",
                    items.len()
                )));
            }
            return Ok(item_key(&items[position - 1]));
        }
    }
    if let Expr::Identifier(ident) = expr {
        for item in items {
            let label = match item {
                SelectItem::Column { label, .. } | SelectItem::Expr { label, .. } => label,
                SelectItem::Score { .. } => continue,
            };
            if label.eq_ignore_ascii_case(&ident.value) {
                return Ok(item_key(item));
            }
        }
    }
    Err(Error::Catalog(
        "ORDER BY over a compound query may only name an output column or a 1-based ordinal"
            .to_string(),
    ))
}

/// The sort key that reproduces one output column.
fn item_key(item: &SelectItem) -> OrderKey {
    match item {
        SelectItem::Column { index, .. } => OrderKey::Column(*index),
        SelectItem::Expr { expr, .. } => OrderKey::Expr(expr.clone()),
        SelectItem::Score { .. } => OrderKey::Score,
    }
}

/// The collating sequence one projected column carries, for `DISTINCT`.
fn item_collation(item: &SelectItem, scope: &Scope<'_>, binder: &Binder<'_>) -> Collation {
    match item {
        SelectItem::Column { index, .. } => scope.collation_at(*index).unwrap_or_default(),
        SelectItem::Expr { expr, .. } => term_collation(expr, scope, binder),
        // A retrieval score is a number and has no text to collate.
        SelectItem::Score { .. } => Collation::Binary,
    }
}

/// The collating sequence one `ORDER BY` key sorts under.
fn order_key_collation(key: &OrderKey, scope: &Scope<'_>, binder: &Binder<'_>) -> Collation {
    match key {
        OrderKey::Column(index) => scope.collation_at(*index).unwrap_or_default(),
        OrderKey::Expr(expr) => term_collation(expr, scope, binder),
        OrderKey::Score => Collation::Binary,
    }
}

/// Resolve `LIMIT` and `OFFSET` into expressions evaluated at execution.
///
/// They are expressions rather than counts because SQLite allows `LIMIT ?`,
/// and a bound parameter has no value until the statement runs.
fn resolve_limit(
    clause: Option<&LimitClause>,
    scope: &Scope<'_>,
    binder: &mut Binder,
) -> Result<(Option<PlanExpr>, Option<PlanExpr>)> {
    let Some(clause) = clause else {
        return Ok((None, None));
    };
    match clause {
        LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => {
            if !limit_by.is_empty() {
                return Err(Error::Unsupported(
                    "LIMIT ... BY is a ClickHouse extension and is not supported".to_string(),
                ));
            }
            let limit = limit
                .as_ref()
                .map(|expr| bind_row_count("LIMIT", expr, scope, binder))
                .transpose()?;
            let offset = match offset {
                Some(offset) => {
                    if offset.rows != OffsetRows::None {
                        return Err(Error::Unsupported(
                            "OFFSET ... ROW/ROWS is not in SQLite's dialect".to_string(),
                        ));
                    }
                    Some(bind_row_count("OFFSET", &offset.value, scope, binder)?)
                }
                None => None,
            };
            Ok((limit, offset))
        }
        // `LIMIT offset, count`: SQLite accepts it, with the arguments in the
        // opposite order to everything else.
        LimitClause::OffsetCommaLimit { offset, limit } => {
            let offset = bind_row_count("OFFSET", offset, scope, binder)?;
            let limit = bind_row_count("LIMIT", limit, scope, binder)?;
            Ok((Some(limit), Some(offset)))
        }
    }
}

/// A `LIMIT` or `OFFSET` operand: a literal, a `?`, or an uncorrelated
/// subquery — never a row expression.
///
/// SQLite evaluates the clause once, before the query runs, so a reference to
/// a column would have nothing to read. A *correlated* subquery is the same
/// mistake wearing a subquery's clothes: it would be evaluated against no row
/// at all, so it is refused here rather than failing per execution.
fn bind_row_count(
    what: &str,
    expr: &Expr,
    scope: &Scope<'_>,
    binder: &mut Binder,
) -> Result<PlanExpr> {
    if let Ok(bound) = bind_value(expr, binder) {
        return Ok(bound);
    }
    if matches!(unnest(expr), Expr::Subquery(_)) {
        let bound = resolve_expr(expr, scope, binder)?;
        if let PlanExpr::Subquery { query, .. } = &bound {
            if !query.captures.is_empty() {
                return Err(Error::Unsupported(alloc::format!(
                    "{what} `{expr}` may not reference a column: it is evaluated once, before \
                     the query reads a row"
                )));
            }
        }
        return Ok(bound);
    }
    Err(Error::Unsupported(alloc::format!(
        "{what} `{expr}` must be a literal, a `?` parameter or an uncorrelated subquery"
    )))
}

/// Strip the parentheses `sqlparser` records as [`Expr::Nested`].
fn unnest(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unnest(inner),
        other => other,
    }
}

// ------------------------------------------------------------------ literals

/// Resolve an expression that must be a literal or a `?`.
///
/// Returns [`PlanExpr::Literal`] when the value is known now and
/// [`PlanExpr::Param`] when it arrives at execution. Constant folding is done
/// here rather than left to the evaluator so that the common case — a literal —
/// costs nothing per execution, and so that a bad literal is a prepare-time
/// error.
fn bind_value(expr: &Expr, binder: &mut Binder) -> Result<PlanExpr> {
    match expr {
        Expr::Value(value) => bind_literal(&value.value, binder),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match bind_value(expr, binder)? {
            PlanExpr::Literal(Value::Integer(i)) => Ok(PlanExpr::Literal(Value::Integer(-i))),
            PlanExpr::Literal(Value::Real(r)) => Ok(PlanExpr::Literal(Value::Real(-r))),
            PlanExpr::Literal(other) => Err(Error::Type(alloc::format!(
                "cannot negate a {} value",
                other.type_name()
            ))),
            // `-?`: the sign is known, the magnitude is not. The evaluator
            // negates it once the parameter is bound.
            param => Ok(PlanExpr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(param),
            }),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => bind_value(expr, binder),
        Expr::Function(function) => {
            let name = object_name(&function.name)?;
            if !name.eq_ignore_ascii_case("vector") {
                return Err(Error::Unsupported(alloc::format!(
                    "`{name}()` cannot be used as a value here"
                )));
            }
            let FunctionArguments::List(list) = &function.args else {
                return Err(Error::Type(
                    "vector() takes one string literal, e.g. vector('[0.1, 0.2]')".to_string(),
                ));
            };
            let arg = expect_one(&name, &list.args)?;
            // Only a literal: `vector(?)` would mean parsing an embedding out
            // of text on every execution, when binding `Value::Vector`
            // directly is both faster and better typed.
            let PlanExpr::Literal(Value::Text(text)) = bind_value(arg, binder)? else {
                return Err(Error::Type(
                    "vector() takes a string literal, e.g. vector('[0.1, 0.2]'); \
                     bind an embedding as a `?` parameter instead"
                        .to_string(),
                ));
            };
            Ok(PlanExpr::Literal(parse_vector_literal(&text)?))
        }
        Expr::Array(array) => {
            let mut values = Vec::with_capacity(array.elem.len());
            for element in &array.elem {
                let PlanExpr::Literal(value) = bind_value(element, binder)? else {
                    return Err(Error::Type(
                        "a vector literal may not contain `?`; bind the whole embedding instead"
                            .to_string(),
                    ));
                };
                values.push(value.as_f64().ok_or_else(|| {
                    Error::Type("vector literals may only contain numbers".to_string())
                })? as f32);
            }
            Ok(PlanExpr::Literal(Value::Vector(values)))
        }
        other => Err(Error::Unsupported(alloc::format!(
            "expected a literal or `?` placeholder, got `{other}`"
        ))),
    }
}

fn bind_literal(value: &AstValue, binder: &mut Binder) -> Result<PlanExpr> {
    let value = match value {
        AstValue::Null => Value::Null,
        AstValue::Placeholder(_) => return Ok(binder.take()),
        AstValue::Number(text, _) => {
            if let Ok(i) = text.parse::<i64>() {
                Value::Integer(i)
            } else {
                text.parse::<f64>()
                    .map(Value::Real)
                    .map_err(|_| Error::Type(alloc::format!("`{text}` is not a number")))?
            }
        }
        AstValue::SingleQuotedString(s)
        | AstValue::DoubleQuotedString(s)
        | AstValue::EscapedStringLiteral(s) => Value::Text(s.clone().into()),
        AstValue::Boolean(b) => Value::Integer(i64::from(*b)),
        AstValue::HexStringLiteral(hex) => Value::Blob(parse_blob_literal(hex)?),
        other => {
            return Err(Error::Unsupported(alloc::format!(
                "literal `{other}` is not supported"
            )))
        }
    };
    Ok(PlanExpr::Literal(value))
}

/// Decode the digits of an `X'...'` blob literal.
///
/// SQLite requires an even number of hexadecimal digits and nothing else, and
/// rejects the literal outright otherwise. The row codec and [`Value::Blob`]
/// already round-trip the result; only this spelling was missing.
fn parse_blob_literal(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return Err(Error::Type(alloc::format!(
            "blob literal X'{hex}' has an odd number of hexadecimal digits"
        )));
    }
    let digits = hex.as_bytes();
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.as_chunks::<2>().0 {
        let high = (pair[0] as char).to_digit(16);
        let low = (pair[1] as char).to_digit(16);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(Error::Type(alloc::format!(
                "blob literal X'{hex}' is not hexadecimal"
            )));
        };
        bytes.push((high * 16 + low) as u8);
    }
    Ok(bytes)
}

/// Parse `'[0.1, 0.2, 0.3]'` (brackets optional) into an embedding.
///
/// Writing a 384-dimensional embedding out by hand is not the point — bind it
/// as a parameter instead. This exists so that small examples and tests can be
/// expressed entirely in SQL.
fn parse_vector_literal(text: &str) -> Result<Value> {
    let trimmed = text.trim().trim_start_matches('[').trim_end_matches(']');
    let mut values = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        values.push(
            part.parse::<f32>()
                .map_err(|_| Error::Type(alloc::format!("`{part}` is not a number")))?,
        );
    }
    if values.is_empty() {
        return Err(Error::Type("vector literal is empty".to_string()));
    }
    Ok(Value::Vector(values))
}

// ----------------------------------------------------------------- constraints

/// A table's constraints, resolved against its current shape.
///
/// Built from the catalog rather than carried in a plan, and that is the
/// point: a plan holds ordinals and a prepared statement re-checks the *shape*
/// it resolved them against, but a table can be dropped and recreated with the
/// same columns and different constraints. Reading them live means a
/// constraint can never be enforced from a stale copy. The engine caches the
/// result per table and throws the cache away whenever the catalog moves.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableRules {
    /// Per column, in ordinal order: the resolved `DEFAULT`, if declared.
    pub defaults: Vec<Option<PlanExpr>>,
    /// Every `CHECK`: the text it was written as, for the error message, and
    /// the expression to evaluate over the row.
    pub checks: Vec<(String, PlanExpr)>,
    /// Every `UNIQUE` constraint, as column ordinals.
    pub unique: Vec<Vec<usize>>,
}

/// Resolve one table's declared constraints against its columns.
pub(crate) fn table_rules(table: &Table, catalog: &Catalog) -> Result<TableRules> {
    let scope = Scope::single(table);
    let mut defaults = Vec::with_capacity(table.columns.len());
    for column in &table.columns {
        defaults.push(match &column.default {
            // A default is a constant: it is evaluated with no row in hand, so
            // resolving it in an empty scope is what makes a column reference
            // inside one an error rather than a panic later.
            Some(text) => Some(parse_expression(text, &Scope::empty())?),
            None => None,
        });
    }

    let mut checks = Vec::new();
    let mut unique = Vec::new();
    if let Some(constraints) = catalog.constraints(&table.name) {
        for check in &constraints.checks {
            checks.push((check.clone(), parse_expression(check, &scope)?));
        }
        for group in &constraints.unique {
            let mut ordinals = Vec::with_capacity(group.columns.len());
            for column in &group.columns {
                ordinals.push(table.require_column(column)?.0);
            }
            unique.push(ordinals);
        }
    }
    Ok(TableRules {
        defaults,
        checks,
        unique,
    })
}

/// Parse a stored expression — a `DEFAULT` or a `CHECK` — and resolve it in
/// `scope`.
///
/// The text came out of the catalog, so it parsed once already; it is parsed
/// as the body of a `SELECT` because that is the only entry point `sqlparser`
/// offers for an expression, and because doing it that way means a stored
/// expression goes through exactly the same resolver as a written one.
fn parse_expression(text: &str, scope: &Scope<'_>) -> Result<PlanExpr> {
    let sql = alloc::format!("SELECT {text}");
    check_nesting(&sql)?;
    let mut statements = Parser::parse_sql(&SQLiteDialect {}, &sql)
        .map_err(|e| Error::Parse(alloc::format!("in `{text}`: {e}")))?;
    let [Statement::Query(query)] = statements.as_mut_slice() else {
        return Err(Error::Parse(alloc::format!(
            "`{text}` is not an expression"
        )));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(Error::Parse(alloc::format!(
            "`{text}` is not an expression"
        )));
    };
    let [AstSelectItem::UnnamedExpr(expr)] = select.projection.as_slice() else {
        return Err(Error::Parse(alloc::format!(
            "`{text}` is not a single expression"
        )));
    };
    // An empty catalog, deliberately: a stored expression names no table, and
    // `subqueries_allowed = false` is what refuses the one construct that
    // would have wanted it.
    let empty = Catalog::new();
    let mut binder = Binder::new(&empty);
    binder.subqueries_allowed = false;
    let resolved = resolve_expr(expr, scope, &mut binder)?;
    if binder.count > 0 || !binder.aggregates.is_empty() || !binder.windows.is_empty() {
        return Err(Error::Unsupported(alloc::format!(
            "`{text}` may not contain a `?` placeholder, an aggregate function or a window \
             function"
        )));
    }
    Ok(resolved)
}

/// Whether a stored expression references a column by name.
///
/// Used by `ALTER TABLE DROP COLUMN`, which SQLite refuses when a `CHECK`
/// names the column. Answering by tokenising rather than by substring search
/// is what keeps `CHECK (label <> 'a')` from blocking a drop of column `a`.
pub(crate) fn expression_mentions(text: &str, column: &str) -> Result<bool> {
    Ok(tokens(text)?
        .iter()
        .any(|token| is_identifier(token, column)))
}

/// Rewrite every reference to `old` in a stored expression as `new`.
///
/// `ALTER TABLE RENAME COLUMN` has to do this or a `CHECK` would go on naming
/// a column that no longer exists. It works on the token stream, so a string
/// literal that happens to spell the old name is left alone — a literal is a
/// `SingleQuotedString` token, never a word.
pub(crate) fn rewrite_column_reference(text: &str, old: &str, new: &str) -> Result<String> {
    use sqlparser::tokenizer::{Token, Word};

    let mut out = String::new();
    for token in tokens(text)? {
        match &token {
            Token::Word(word) if word.value.eq_ignore_ascii_case(old) => {
                out.push_str(
                    &Token::Word(Word {
                        value: new.to_string(),
                        quote_style: word.quote_style,
                        keyword: word.keyword,
                    })
                    .to_string(),
                );
            }
            other => out.push_str(&other.to_string()),
        }
    }
    Ok(out)
}

/// Tokenise a stored expression.
fn tokens(text: &str) -> Result<Vec<sqlparser::tokenizer::Token>> {
    sqlparser::tokenizer::Tokenizer::new(&SQLiteDialect {}, text)
        .tokenize()
        .map_err(|e| Error::Parse(alloc::format!("in `{text}`: {e}")))
}

/// Whether a token is the identifier `name`, quoted or not.
///
/// Only a `Word` can be one. A string literal is a `SingleQuotedString`, so
/// `CHECK (label <> 'a')` does not count as naming a column `a` — which is the
/// whole reason this tokenises rather than searching for a substring.
fn is_identifier(token: &sqlparser::tokenizer::Token, name: &str) -> bool {
    matches!(token, sqlparser::tokenizer::Token::Word(word)
        if word.value.eq_ignore_ascii_case(name))
}

// ----------------------------------------------------------------- identifiers

/// Resolve an `UPDATE ... SET` (or `ON CONFLICT DO UPDATE SET`) assignment
/// target to a plain column name.
///
/// SQLite's own grammar has no qualified form here at all, verified directly
/// against `sqlite3`: `UPDATE t SET t.col = 1` is a syntax error even when
/// `t` names the statement's own target table, wrong qualifier or right,
/// aliased or not, two-part or three. `sqlparser`'s `SQLiteDialect` is looser
/// than the real grammar and parses it anyway, so a compound name reaches
/// here syntactically valid and is refused on purpose, with a message that
/// names what was written rather than [`object_name`]'s generic "no schemas"
/// one every other qualified name gets.
///
/// A MySQL client's own `t.col = ?` is real syntax there (Eloquent writes it
/// on every save of a model with timestamps) — `crates/inlaysql-server`'s
/// shim is where that qualifier is checked against the statement's own table
/// and stripped, or refused by name, before the statement ever reaches this
/// parser. Nothing here accepts it; see AHL-475.
fn assignment_target_column(name: &ObjectName) -> Result<String> {
    if name.0.len() > 1 {
        return Err(Error::Unsupported(alloc::format!(
            "qualified column `{name}` is not supported on the left of SET; SQLite has no \
             qualified assignment target here — write the bare column name"
        )));
    }
    object_name(name)
}

/// Render an object name as a bare identifier string.
fn object_name(name: &ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [part] => part
            .as_ident()
            .map(|ident| ident.value.clone())
            .ok_or_else(|| {
                Error::Unsupported(alloc::format!("`{name}` is not a plain identifier"))
            }),
        _ => Err(Error::Unsupported(alloc::format!(
            "qualified name `{name}` is not supported; this stage has no schemas"
        ))),
    }
}

fn join_idents(parts: &[Ident]) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(&part.value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                temporary: false,
                primary_key: Vec::new(),
                name: "docs".to_string(),
                columns: vec![
                    Column::new("id", DataType::Integer),
                    Column::new("body", DataType::Text),
                    Column::new("embedding", DataType::Vector(3)),
                ],
                strict: false,
            })
            .unwrap();
        catalog
    }

    fn join_catalog() -> Catalog {
        let mut catalog = catalog();
        catalog
            .create_table(Table {
                without_rowid: false,
                temporary: false,
                primary_key: Vec::new(),
                name: "authors".to_string(),
                columns: vec![
                    Column::new("id", DataType::Integer),
                    Column::new("name", DataType::Text),
                ],
                strict: false,
            })
            .unwrap();
        catalog
    }

    fn agg_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                temporary: false,
                primary_key: Vec::new(),
                name: "t".to_string(),
                columns: vec![Column::new("a", DataType::Integer)],
                strict: false,
            })
            .unwrap();
        catalog
    }

    #[test]
    fn creates_a_table_with_a_vector_column() {
        let plan = plan(
            "CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR(384))",
            &[],
            &Catalog::new(),
        )
        .unwrap();
        let Plan::CreateTable(create) = plan else {
            panic!("expected CREATE TABLE")
        };
        assert_eq!(create.table.columns[2].ty, DataType::Vector(384));
    }

    #[test]
    fn int8_is_an_explicit_vector_storage_choice() {
        let plan = plan(
            "CREATE TABLE docs (embedding VECTOR(384, INT8))",
            &[],
            &Catalog::new(),
        )
        .unwrap();
        let Plan::CreateTable(create) = plan else {
            panic!("expected CREATE TABLE")
        };
        assert_eq!(create.table.columns[0].ty, DataType::QuantizedVector(384));
        assert_eq!(create.table.columns[0].ty.to_string(), "VECTOR(384, INT8)");
    }

    #[test]
    fn a_select_without_from_becomes_a_scalar_plan() {
        let plan = plan("SELECT 1 + 2 * 3", &[], &Catalog::new()).unwrap();
        let Plan::Scalar(scalar) = plan else {
            panic!("expected a scalar plan")
        };
        assert_eq!(scalar.items.len(), 1);
        assert_eq!(
            scalar.items[0].expr,
            PlanExpr::Binary {
                collation: Collation::Binary,
                affinity: CompareAffinity::None,
                op: BinaryOp::Add,
                left: Box::new(PlanExpr::Literal(Value::Integer(1))),
                right: Box::new(PlanExpr::Binary {
                    collation: Collation::Binary,
                    affinity: CompareAffinity::None,
                    op: BinaryOp::Mul,
                    left: Box::new(PlanExpr::Literal(Value::Integer(2))),
                    right: Box::new(PlanExpr::Literal(Value::Integer(3))),
                }),
            }
        );
    }

    #[test]
    fn a_scalar_select_rejects_where() {
        let err = plan("SELECT 1 WHERE 1", &[], &Catalog::new()).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
    }

    #[test]
    fn insert_numbers_placeholders_in_order() {
        let plan = plan(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(1),
                Value::Text("hello".to_string().into()),
                Value::Vector(vec![1.0, 0.0, 0.0]),
            ],
            &catalog(),
        )
        .unwrap();
        let Plan::Insert(insert) = plan else {
            panic!("expected INSERT")
        };
        assert_eq!(
            insert.source,
            InsertSource::Values(vec![vec![
                Some(PlanExpr::Param(0)),
                Some(PlanExpr::Param(1)),
                Some(PlanExpr::Param(2))
            ]])
        );
    }

    #[test]
    fn insert_places_omitted_columns_and_literals() {
        // Columns out of declaration order, one of them omitted: the plan is
        // widened to table width with the values in the right slots.
        let plan = plan(
            "INSERT INTO docs (body, id) VALUES ('x', ?)",
            &[Value::Integer(4)],
            &catalog(),
        )
        .unwrap();
        let Plan::Insert(insert) = plan else {
            panic!("expected INSERT")
        };
        assert_eq!(
            insert.source,
            InsertSource::Values(vec![vec![
                Some(PlanExpr::Param(0)),
                Some(PlanExpr::Literal(Value::Text("x".to_string().into()))),
                // Not `Some(NULL)`: the statement never named this column, so
                // its default applies rather than an explicit `NULL`.
                None,
            ]])
        );
    }

    /// The clauses AHL-410 refused rather than silently dropping, now that
    /// they are implemented. Each one has to *plan*: a refusal here would mean
    /// the phase did not land, and an acceptance that planned the wrong thing
    /// is what the refusals existed to prevent in the first place.
    #[test]
    fn the_conflict_clauses_plan_instead_of_being_refused() {
        let catalog = catalog();
        for (sql, expected) in [
            (
                "INSERT INTO docs (id) VALUES (1) ON CONFLICT DO NOTHING",
                ConflictAction::Ignore,
            ),
            (
                "INSERT OR IGNORE INTO docs (id) VALUES (1)",
                ConflictAction::Ignore,
            ),
            (
                "INSERT OR REPLACE INTO docs (id) VALUES (1)",
                ConflictAction::Replace,
            ),
            ("REPLACE INTO docs (id) VALUES (1)", ConflictAction::Replace),
            (
                "INSERT OR ABORT INTO docs (id) VALUES (1)",
                ConflictAction::Abort,
            ),
            ("INSERT INTO docs (id) VALUES (1)", ConflictAction::Abort),
        ] {
            let Plan::Insert(insert) = plan(sql, &[], &catalog).unwrap() else {
                panic!("`{sql}` did not plan as an INSERT")
            };
            assert_eq!(insert.on_conflict.action, expected, "`{sql}`");
            // None of these names a constraint, so each answers for any of them.
            assert_eq!(insert.on_conflict.target, None, "`{sql}`");
        }
    }

    /// `ON CONFLICT (id) DO UPDATE` names the row-id alias, which is a real
    /// uniqueness constraint, so it resolves; `excluded` reads the proposed
    /// row and a bare name reads the stored one.
    #[test]
    fn an_upsert_resolves_excluded_against_the_proposed_row() {
        let mut keyed = Catalog::new();
        keyed
            .create_table(Table {
                without_rowid: false,
                temporary: false,
                primary_key: Vec::new(),
                name: "docs".to_string(),
                columns: vec![
                    Column::primary_key("id", DataType::Integer),
                    Column::new("body", DataType::Text),
                    Column::new("embedding", DataType::Vector(3)),
                ],
                strict: false,
            })
            .unwrap();
        let Plan::Insert(insert) = plan(
            "INSERT INTO docs (id, body) VALUES (1, 'x') \
             ON CONFLICT (id) DO UPDATE SET body = excluded.body WHERE body <> excluded.body",
            &[],
            &keyed,
        )
        .unwrap() else {
            panic!("expected an INSERT")
        };
        // The target is the row-id alias, resolved to its ordinal — which is
        // what decides that a collision on some *other* unique column is a
        // violation rather than an upsert.
        assert_eq!(insert.on_conflict.target, Some(vec![0]));
        let ConflictAction::Update(update) = &insert.on_conflict.action else {
            panic!("expected DO UPDATE, got {:?}", insert.on_conflict.action)
        };
        // `docs` is (id, body, embedding), so `excluded.body` is ordinal 4.
        assert_eq!(update.assignments, vec![(1, PlanExpr::Column(4))]);
        assert_eq!(
            update.filter,
            Some(PlanExpr::Binary {
                collation: Collation::Binary,
                // Both sides are the `body` column (TEXT affinity), so this
                // resolves to `Text` — a no-op conversion here, since neither
                // side is ever anything but TEXT, but the resolved value is
                // still checked rather than assumed.
                affinity: CompareAffinity::Text,
                op: BinaryOp::NotEq,
                left: Box::new(PlanExpr::Column(1)),
                right: Box::new(PlanExpr::Column(4)),
            })
        );
    }

    /// A conflict target that matches no uniqueness constraint would make the
    /// clause unreachable, so SQLite refuses it and so does this.
    #[test]
    fn an_unreachable_conflict_target_is_refused() {
        let err = plan(
            "INSERT INTO docs (id) VALUES (1) ON CONFLICT (body) DO NOTHING",
            &[],
            &catalog(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Catalog(_)), "got {err}");
    }

    #[test]
    fn returning_resolves_on_every_write_statement() {
        let catalog = catalog();
        for sql in [
            "INSERT INTO docs (id) VALUES (1) RETURNING id",
            "UPDATE docs SET id = 1 RETURNING id, body",
            "DELETE FROM docs RETURNING *",
        ] {
            plan(sql, &[], &catalog).unwrap_or_else(|e| panic!("`{sql}` was refused: {e}"));
        }
        // An aggregate over "the row" has no meaning, and SQLite refuses it.
        assert!(matches!(
            plan("DELETE FROM docs RETURNING COUNT(*)", &[], &catalog).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    /// `INSERT OR ROLLBACK` and `OR FAIL` promise something about *partial*
    /// writes, and a statement here is already atomic, so they are refused
    /// rather than quietly treated as `OR ABORT`.
    #[test]
    fn conflict_resolutions_that_promise_partial_writes_are_refused() {
        for sql in [
            "INSERT OR ROLLBACK INTO docs (id) VALUES (1)",
            "INSERT OR FAIL INTO docs (id) VALUES (1)",
            "UPDATE OR REPLACE docs SET id = 1",
            "UPDATE OR IGNORE docs SET id = 1",
        ] {
            let err = plan(sql, &[], &catalog()).unwrap_err();
            assert!(
                matches!(err, Error::Unsupported(_)),
                "`{sql}` gave {err} instead of a refusal"
            );
        }
    }

    /// The constraints AHL-410 refused now resolve into the catalog. The
    /// enforcement they get is asserted end to end in `constraints.rs`; what
    /// this checks is that the declaration survives planning at all.
    #[test]
    fn declared_constraints_reach_the_catalog() {
        let Plan::CreateTable(create) = plan(
            "CREATE TABLE t (a INTEGER NOT NULL DEFAULT 1 CHECK (a > 0), \
             b TEXT UNIQUE, c INTEGER REFERENCES u(id) ON DELETE CASCADE, \
             UNIQUE (a, b), CHECK (b <> ''))",
            &[],
            &Catalog::new(),
        )
        .unwrap() else {
            panic!("expected CREATE TABLE")
        };
        assert!(create.table.columns[0].not_null);
        assert_eq!(create.table.columns[0].default.as_deref(), Some("1"));
        assert_eq!(
            create.constraints.unique,
            vec![
                UniqueConstraint::new(vec!["b".to_string()]),
                UniqueConstraint::new(vec!["a".to_string(), "b".to_string()])
            ]
        );
        assert_eq!(create.constraints.checks, ["a > 0", "b <> ''"]);
        let key = &create.constraints.foreign_keys[0];
        assert_eq!(key.columns, ["c"]);
        assert_eq!(key.table, "u");
        assert_eq!(key.referenced, ["id"]);
        assert_eq!(key.on_delete.as_deref(), Some("CASCADE"));
    }

    /// SQLite's model, which decides the storage layout: a lone
    /// `INTEGER PRIMARY KEY` is the row id, and every other primary key is a
    /// unique index wearing a different name.
    #[test]
    fn only_a_lone_integer_primary_key_is_the_row_id() {
        let rowid = |sql: &str| {
            let Plan::CreateTable(create) = plan(sql, &[], &Catalog::new()).unwrap() else {
                panic!("expected CREATE TABLE")
            };
            (create.table.rowid_alias(), create.constraints.unique)
        };

        assert_eq!(
            rowid("CREATE TABLE t (a INTEGER PRIMARY KEY)"),
            (Some(0), vec![])
        );
        assert_eq!(
            rowid("CREATE TABLE t (a INTEGER, PRIMARY KEY (a))"),
            (Some(0), vec![])
        );
        // Text, so an index rather than the key.
        assert_eq!(
            rowid("CREATE TABLE t (a TEXT PRIMARY KEY)"),
            (None, vec![UniqueConstraint::new(vec!["a".to_string()])])
        );
        // Composite, so an index however the columns are typed.
        assert_eq!(
            rowid("CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY (a, b))"),
            (
                None,
                vec![UniqueConstraint::new(vec![
                    "a".to_string(),
                    "b".to_string()
                ])]
            )
        );
    }

    /// `AUTOINCREMENT` asks for keys that are never reused, which the row-id
    /// counter already guarantees — so it is accepted where SQLite allows it
    /// and refused where SQLite does not.
    #[test]
    fn autoincrement_is_accepted_only_where_sqlite_allows_it() {
        plan(
            "CREATE TABLE t (a INTEGER PRIMARY KEY AUTOINCREMENT)",
            &[],
            &Catalog::new(),
        )
        .unwrap();
        for sql in [
            "CREATE TABLE t (a INTEGER AUTOINCREMENT)",
            "CREATE TABLE t (a TEXT PRIMARY KEY AUTOINCREMENT)",
        ] {
            let err = plan(sql, &[], &Catalog::new()).unwrap_err();
            assert!(matches!(err, Error::Unsupported(_)), "`{sql}`: {err}");
        }
    }

    /// Still refused, for a stated reason rather than because nobody got to
    /// it: `COLLATE` needs collations the comparison path does not have.
    #[test]
    fn table_clauses_that_remain_unsupported_are_refused() {
        // `COLLATE NOCASE` used to be here. It is a real clause now
        // (AHL-469); a collation this engine does not have still is not.
        let sql = "CREATE TABLE t (a TEXT COLLATE utf8mb4_unicode_ci)";
        let err = plan(sql, &[], &Catalog::new()).unwrap_err();
        assert!(
            matches!(err, Error::Unsupported(_)),
            "`{sql}` gave {err} instead of a refusal"
        );
    }

    /// Verified against a real sqlite3 binary: `CREATE TABLE t AS SELECT id,
    /// body AS renamed FROM docs` keeps `id`'s and `body`'s declared types —
    /// an alias does not lose it — and neither an expression nor a literal
    /// gets one at all.
    #[test]
    fn create_table_as_select_keeps_a_bare_columns_type_but_not_an_expressions() {
        let plan = plan(
            "CREATE TABLE t AS SELECT id, body AS renamed, id + 1 AS incremented, \
             'lit' AS lit FROM docs",
            &[],
            &catalog(),
        )
        .unwrap();
        let Plan::CreateTable(create) = plan else {
            panic!("expected CREATE TABLE")
        };
        assert!(create.as_select.is_some());
        let columns = &create.table.columns;
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].ty, DataType::Integer);
        assert_eq!(columns[1].name, "renamed");
        assert_eq!(
            columns[1].ty,
            DataType::Text,
            "an alias does not stop a bare column's type from carrying over"
        );
        assert_eq!(columns[2].name, "incremented");
        assert_eq!(
            columns[2].ty,
            DataType::Numeric,
            "an expression has no declared type in SQLite; Numeric is the closest \
             affinity this catalog has to that without a format change"
        );
        assert_eq!(columns[3].name, "lit");
        assert_eq!(columns[3].ty, DataType::Numeric);
        for column in columns {
            assert!(
                !column.primary_key,
                "CTAS carries over no column's PRIMARY KEY"
            );
            assert!(!column.not_null, "CTAS carries over no column's NOT NULL");
            assert!(
                column.default.is_none(),
                "CTAS carries over no column's DEFAULT"
            );
        }
    }

    #[test]
    fn create_table_as_select_refuses_two_columns_with_the_same_name() {
        let err = plan("CREATE TABLE t AS SELECT id, id FROM docs", &[], &catalog()).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "got {err}");
    }

    /// Real sqlite3 can type a compound's column from an arm that is itself
    /// an expression (`SELECT a+1 FROM t UNION SELECT a FROM t` keeps `a`'s
    /// `INTEGER`) — an affinity-unification rule this catalog does not
    /// replicate. Every compound column is `Numeric` instead: always safe,
    /// only sometimes narrower than SQLite's answer.
    #[test]
    fn create_table_as_select_of_a_compound_query_is_untyped() {
        let plan = plan(
            "CREATE TABLE t AS SELECT id FROM docs UNION SELECT id FROM docs",
            &[],
            &catalog(),
        )
        .unwrap();
        let Plan::CreateTable(create) = plan else {
            panic!("expected CREATE TABLE")
        };
        assert_eq!(create.table.columns[0].ty, DataType::Numeric);
    }

    #[test]
    fn transaction_and_savepoint_statements_plan_onto_the_engine_api() {
        for (sql, expected) in [
            ("BEGIN", Plan::Begin),
            ("BEGIN TRANSACTION", Plan::Begin),
            ("COMMIT", Plan::Commit),
            ("END", Plan::Commit),
            ("ROLLBACK", Plan::Rollback),
            ("SAVEPOINT s", Plan::Savepoint("s".to_string())),
            (
                "RELEASE SAVEPOINT s",
                Plan::ReleaseSavepoint("s".to_string()),
            ),
            ("RELEASE s", Plan::ReleaseSavepoint("s".to_string())),
            (
                "ROLLBACK TO SAVEPOINT s",
                Plan::RollbackToSavepoint("s".to_string()),
            ),
            ("ROLLBACK TO s", Plan::RollbackToSavepoint("s".to_string())),
        ] {
            assert_eq!(
                plan(sql, &[], &Catalog::new()).unwrap(),
                expected,
                "`{sql}`"
            );
        }
    }

    /// SQLite's affinity rules, including the two that read as accidents:
    /// `POINT` is INTEGER because rule 1 sees the `INT` inside it, and
    /// `STRING` is NUMERIC because it matches none of the four named tests.
    #[test]
    fn declared_types_follow_sqlite_affinity() {
        let ty = |declared: &str| {
            let sql = alloc::format!("CREATE TABLE t (a {declared})");
            let Plan::CreateTable(create) = plan(&sql, &[], &Catalog::new())
                .unwrap_or_else(|e| panic!("`{sql}` was refused: {e}"))
            else {
                panic!("expected CREATE TABLE")
            };
            create.table.columns[0].ty
        };

        for declared in ["INT", "INTEGER", "BIGINT", "TINYINT", "POINT", "INT2"] {
            assert_eq!(ty(declared), DataType::Integer, "{declared}");
        }
        for declared in ["TEXT", "VARCHAR(255)", "CHARACTER(20)", "CLOB", "NVARCHAR"] {
            assert_eq!(ty(declared), DataType::Text, "{declared}");
        }
        for declared in ["BLOB", ""] {
            assert_eq!(ty(declared), DataType::Blob, "{declared}");
        }
        for declared in ["REAL", "DOUBLE", "DOUBLE PRECISION", "FLOAT"] {
            assert_eq!(ty(declared), DataType::Real, "{declared}");
        }
        for declared in [
            "NUMERIC",
            "DECIMAL(8,2)",
            "BOOLEAN",
            "DATE",
            "DATETIME",
            "JSON",
            "STRING",
        ] {
            assert_eq!(ty(declared), DataType::Numeric, "{declared}");
        }
        // The InlaySQL extension is still resolved before the rules run.
        assert_eq!(ty("VECTOR(4)"), DataType::Vector(4));
    }

    /// The two options that were never refused, because the engine already
    /// does exactly what they ask.
    #[test]
    fn primary_key_and_explicit_null_still_plan() {
        for sql in [
            "CREATE TABLE t (a INTEGER PRIMARY KEY)",
            "CREATE TABLE t (a INTEGER NULL)",
        ] {
            plan(sql, &[], &Catalog::new()).unwrap_or_else(|e| panic!("`{sql}` was refused: {e}"));
        }
    }

    #[test]
    fn insert_checks_vector_dimension() {
        let err = plan(
            "INSERT INTO docs VALUES (1, 'x', vector('[1.0, 2.0]'))",
            &[],
            &catalog(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Type(_)), "got {err}");
    }

    #[test]
    fn hybrid_select_becomes_one_fused_score() {
        let params = vec![
            Value::Vector(vec![1.0, 0.0, 0.0]),
            Value::Text("rust".to_string().into()),
        ];
        let plan = plan(
            "SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
             FROM docs ORDER BY score DESC LIMIT 5",
            &params,
            &catalog(),
        )
        .unwrap();
        let Plan::Select(select) = plan else {
            panic!("expected SELECT")
        };
        assert_eq!(select.limit, Some(PlanExpr::Literal(Value::Integer(5))));
        assert_eq!(select.order, vec![Order::new(OrderKey::Score, true)]);
        let Some(ScoreExpr::Fuse { parts, .. }) = select.score else {
            panic!("expected a fused score")
        };
        assert!(matches!(parts[0], ScoreExpr::Vector { column: 2, .. }));
        assert!(matches!(&parts[1], ScoreExpr::Text { columns, .. } if columns.as_slice() == [1]));
    }

    #[test]
    fn retrieval_queries_default_to_best_first() {
        let plan = plan(
            "SELECT id, bm25_score(body, 'rust') FROM docs",
            &[],
            &catalog(),
        )
        .unwrap();
        let Plan::Select(select) = plan else {
            panic!("expected SELECT")
        };
        assert_eq!(select.order, vec![Order::new(OrderKey::Score, true)]);
        assert_eq!(select.items[1].label(), "score");
    }

    #[test]
    fn score_functions_check_their_column_type() {
        let err = plan("SELECT bm25_score(id, 'rust') FROM docs", &[], &catalog()).unwrap_err();
        assert!(matches!(err, Error::Type(_)), "got {err}");
    }

    #[test]
    fn unbound_placeholders_are_rejected() {
        let err = plan("SELECT id FROM docs WHERE id = ?", &[], &catalog()).unwrap_err();
        assert!(matches!(err, Error::Bind(_)), "got {err}");
    }

    #[test]
    fn surplus_parameters_are_rejected() {
        let err = plan(
            "SELECT id FROM docs WHERE id = 1",
            &[Value::Integer(9)],
            &catalog(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Bind(_)), "got {err}");
    }

    #[test]
    fn where_clause_becomes_a_boolean_expression() {
        let plan = plan(
            "SELECT * FROM docs WHERE id >= 3 AND body = 'x'",
            &[],
            &catalog(),
        )
        .unwrap();
        let Plan::Select(select) = plan else {
            panic!("expected SELECT")
        };
        assert_eq!(select.items.len(), 3);
        assert_eq!(
            select.filter,
            Some(PlanExpr::Binary {
                collation: Collation::Binary,
                affinity: CompareAffinity::None,
                op: BinaryOp::And,
                left: Box::new(PlanExpr::Binary {
                    collation: Collation::Binary,
                    // `id` is `INTEGER`; the literal has no affinity of its
                    // own, so `id`'s wins.
                    affinity: CompareAffinity::Numeric,
                    op: BinaryOp::GtEq,
                    left: Box::new(PlanExpr::Column(0)),
                    right: Box::new(PlanExpr::Literal(Value::Integer(3))),
                }),
                right: Box::new(PlanExpr::Binary {
                    collation: Collation::Binary,
                    // `body` is `TEXT`, same reasoning.
                    affinity: CompareAffinity::Text,
                    op: BinaryOp::Eq,
                    left: Box::new(PlanExpr::Column(1)),
                    right: Box::new(PlanExpr::Literal(Value::Text("x".to_string().into()))),
                }),
            })
        );
    }

    #[test]
    fn an_inner_join_resolves_both_tables_columns() {
        let plan = plan(
            "SELECT docs.id, a.name FROM docs JOIN authors a ON docs.id = a.id",
            &[],
            &join_catalog(),
        )
        .unwrap();
        let Plan::Select(select) = plan else {
            panic!("expected SELECT")
        };
        assert_eq!(select.from.len(), 2);
        assert_eq!(select.joins.len(), 1);
        assert_eq!(select.joins[0].kind, JoinKind::Inner);
        // `docs` contributes ordinals 0..3, `authors` 3..5; `docs.id` is 0 and
        // `a.name` is 4.
        assert_eq!(
            select.items,
            vec![
                SelectItem::Column {
                    index: 0,
                    label: "id".to_string()
                },
                SelectItem::Column {
                    index: 4,
                    label: "name".to_string()
                },
            ]
        );
        assert_eq!(
            select.joins[0].on,
            Some(PlanExpr::Binary {
                collation: Collation::Binary,
                // Both `docs.id` and `authors.id` are `INTEGER`.
                affinity: CompareAffinity::Numeric,
                op: BinaryOp::Eq,
                left: Box::new(PlanExpr::Column(0)),
                right: Box::new(PlanExpr::Column(3)),
            })
        );
    }

    #[test]
    fn a_left_join_expands_a_wildcard_to_both_tables() {
        let plan = plan(
            "SELECT * FROM docs LEFT JOIN authors ON docs.id = authors.id",
            &[],
            &join_catalog(),
        )
        .unwrap();
        let Plan::Select(select) = plan else {
            panic!("expected SELECT")
        };
        assert_eq!(select.joins[0].kind, JoinKind::Left);
        assert_eq!(select.items.len(), 5);
    }

    // ------------------------------------------------------------ subqueries

    /// The `SelectPlan` of a planned query, for the subquery tests below.
    fn select_plan(sql: &str, catalog: &Catalog) -> SelectPlan {
        match plan(sql, &[], catalog).unwrap() {
            Plan::Select(select) => *select,
            other => panic!("expected a SELECT plan, got {other:?}"),
        }
    }

    /// The one subquery in an expression, whatever it is wrapped in.
    fn only_subquery(expr: &PlanExpr) -> &Subquery {
        find_subquery(expr).unwrap_or_else(|| panic!("no subquery in {expr:?}"))
    }

    fn find_subquery(expr: &PlanExpr) -> Option<&Subquery> {
        match expr {
            PlanExpr::Subquery { query, .. } => Some(query),
            PlanExpr::Unary { expr, .. } => find_subquery(expr),
            PlanExpr::Binary { left, right, .. } => {
                find_subquery(left).or_else(|| find_subquery(right))
            }
            _ => None,
        }
    }

    #[test]
    fn an_uncorrelated_subquery_captures_nothing() {
        let plan = select_plan(
            "SELECT id FROM docs WHERE id IN (SELECT id FROM authors)",
            &join_catalog(),
        );
        let query = only_subquery(plan.filter.as_ref().unwrap());
        assert!(
            query.captures.is_empty(),
            "an uncorrelated subquery must capture nothing, or the executor will \
             re-run it per row: {:?}",
            query.captures
        );
        assert!(matches!(*query.body, SubqueryBody::Select(_)));
    }

    #[test]
    fn a_correlated_subquery_captures_the_outer_column_once() {
        // `docs.id` is named twice; one capture is what the plan should hold.
        let plan = select_plan(
            "SELECT id FROM docs WHERE EXISTS \
             (SELECT 1 FROM authors WHERE authors.id = docs.id AND authors.id <> docs.id + 1)",
            &join_catalog(),
        );
        let query = only_subquery(plan.filter.as_ref().unwrap());
        assert_eq!(
            query.captures,
            vec![PlanExpr::Column(0)],
            "the same outer column twice is one capture"
        );
    }

    #[test]
    fn a_capture_chain_carries_a_reference_through_every_level() {
        // The innermost query names `docs.id`, two levels out, so the middle
        // level must capture it and the inner one must read *that*.
        let plan = select_plan(
            "SELECT id FROM docs WHERE EXISTS (SELECT 1 FROM authors WHERE EXISTS \
             (SELECT 1 FROM authors AS a2 WHERE a2.id = docs.id))",
            &join_catalog(),
        );
        let middle = only_subquery(plan.filter.as_ref().unwrap());
        assert_eq!(middle.captures, vec![PlanExpr::Column(0)]);
        let SubqueryBody::Select(middle_plan) = &*middle.body else {
            panic!("expected a SELECT body");
        };
        let inner = only_subquery(middle_plan.filter.as_ref().unwrap());
        assert_eq!(
            inner.captures,
            vec![PlanExpr::Outer(0)],
            "the inner level reads the middle level's capture, not an absolute ordinal"
        );
    }

    #[test]
    fn a_derived_table_inside_a_subquery_captures_at_its_own_level() {
        // A derived table starts a fresh scope chain (it cannot be correlated),
        // but the *binder's* capture stack is still as deep as the subquery
        // nesting. Counting the scope chain instead wrote a subquery inside the
        // derived table into the enclosing subquery's capture list, which made
        // the outer `EXISTS` correlated against a row it never mentions and left
        // the inner one reading a capture slot nothing filled.
        let plan = select_plan(
            "SELECT id FROM docs WHERE EXISTS (SELECT 1 FROM \
             (SELECT (SELECT COUNT(*) FROM authors WHERE authors.id = x.id) AS c \
              FROM authors AS x) AS d WHERE d.c > 0)",
            &join_catalog(),
        );
        let exists = only_subquery(plan.filter.as_ref().unwrap());
        assert!(
            exists.captures.is_empty(),
            "the EXISTS reads nothing of the outer row, so it captures nothing: {:?}",
            exists.captures
        );

        let SubqueryBody::Select(exists_plan) = &*exists.body else {
            panic!("expected a SELECT body");
        };
        let derived = exists_plan.from[0]
            .derived
            .as_ref()
            .expect("the source is a derived table");
        let SubqueryBody::Select(derived_plan) = &**derived else {
            panic!("expected a SELECT body");
        };
        let SelectItem::Expr { expr, .. } = &derived_plan.items[0] else {
            panic!("expected a projected expression");
        };
        assert_eq!(
            only_subquery(expr).captures,
            vec![PlanExpr::Column(0)],
            "the count captures `x.id` from the derived table's own scope"
        );
    }

    #[test]
    fn an_inner_aggregate_stays_in_the_inner_plan() {
        let plan = select_plan(
            "SELECT id FROM docs WHERE id > (SELECT COUNT(*) FROM authors)",
            &join_catalog(),
        );
        assert!(
            plan.aggregates.is_empty(),
            "an aggregate written inside a subquery must not make the outer query \
             an aggregate one: {:?}",
            plan.aggregates
        );
        let query = only_subquery(plan.filter.as_ref().unwrap());
        let SubqueryBody::Select(inner) = &*query.body else {
            panic!("expected a SELECT body");
        };
        assert_eq!(inner.aggregates.len(), 1);
    }

    #[test]
    fn placeholders_are_numbered_across_a_subquery_in_written_order() {
        let prepared = prepare(
            "SELECT id FROM docs WHERE body = ? AND id IN (SELECT id FROM authors WHERE name = ?) \
             AND id > ?",
            &join_catalog(),
        )
        .unwrap();
        assert_eq!(prepared.parameter_count(), 3);
    }

    #[test]
    fn a_derived_table_takes_its_columns_from_the_inner_query() {
        let plan = select_plan(
            "SELECT n FROM (SELECT id AS n, body FROM docs) AS d",
            &catalog(),
        );
        let [item] = plan.from.as_slice() else {
            panic!("expected one source");
        };
        assert!(item.derived.is_some(), "the source is a derived table");
        assert_eq!(item.table.name, "d");
        let names: Vec<&str> = item
            .table
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names, ["n", "body"]);
    }

    #[test]
    fn a_prepared_statement_stamps_the_tables_only_its_subquery_reads() {
        // Without this the subquery's ordinals would survive an `ALTER TABLE`
        // on a table the outer query never names — a wrong column, silently.
        let statement = prepare(
            "SELECT id FROM docs WHERE id IN (SELECT id FROM authors)",
            &join_catalog(),
        )
        .unwrap();
        let mut moved = Catalog::new();
        moved
            .create_table(Table {
                without_rowid: false,
                temporary: false,
                primary_key: Vec::new(),
                name: "docs".to_string(),
                columns: vec![
                    Column::new("id", DataType::Integer),
                    Column::new("body", DataType::Text),
                    Column::new("embedding", DataType::Vector(3)),
                ],
                strict: false,
            })
            .unwrap();
        moved
            .create_table(Table {
                without_rowid: false,
                temporary: false,
                primary_key: Vec::new(),
                name: "authors".to_string(),
                columns: vec![
                    Column::new("name", DataType::Text),
                    Column::new("id", DataType::Integer),
                ],
                strict: false,
            })
            .unwrap();
        assert!(matches!(
            statement.check_schema(&moved).unwrap_err(),
            Error::Stale(_)
        ));
    }

    #[test]
    fn a_derived_table_cannot_see_the_query_it_sits_in() {
        // SQLite has no LATERAL, so `docs.id` here is a missing name rather
        // than a capture. Resolving it would silently answer a different query.
        let err = plan(
            "SELECT n FROM docs, (SELECT authors.id AS n FROM authors WHERE authors.id = docs.id) AS d",
            &[],
            &join_catalog(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Catalog(_)), "got {err}");
    }

    #[test]
    fn a_subquery_of_the_wrong_width_is_refused_at_plan_time() {
        for sql in [
            "SELECT (SELECT id, name FROM authors) FROM docs",
            "SELECT id FROM docs WHERE id IN (SELECT id, name FROM authors)",
        ] {
            let err = plan(sql, &[], &join_catalog()).unwrap_err();
            assert!(matches!(err, Error::Type(_)), "`{sql}` gave {err}");
        }
    }

    #[test]
    fn a_subquery_in_a_write_statement_is_refused() {
        for sql in [
            "UPDATE docs SET body = (SELECT name FROM authors)",
            "DELETE FROM docs WHERE id IN (SELECT id FROM authors)",
            "INSERT INTO docs (id) VALUES ((SELECT 1))",
            "INSERT INTO docs (id) VALUES (1) RETURNING (SELECT 1)",
        ] {
            let err = plan(sql, &[], &join_catalog()).unwrap_err();
            assert!(matches!(err, Error::Unsupported(_)), "`{sql}` gave {err}");
        }
    }

    #[test]
    fn an_unsupported_join_is_rejected() {
        let err = plan(
            "SELECT * FROM docs RIGHT JOIN authors ON docs.id = authors.id",
            &[],
            &join_catalog(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "got {err}");
    }

    #[test]
    fn an_aggregate_with_group_by_plans() {
        let plan = plan(
            "SELECT a, COUNT(*) AS n FROM t GROUP BY a",
            &[],
            &agg_catalog(),
        )
        .unwrap();
        let Plan::Select(select) = plan else {
            panic!("expected SELECT")
        };
        assert_eq!(select.group_by, vec![PlanExpr::Column(0)]);
        assert_eq!(select.aggregates.len(), 1);
        assert!(matches!(select.aggregates[0].func, AggFunc::Count));
        assert!(select.aggregates[0].arg.is_none());
    }

    #[test]
    fn aggregates_in_where_are_rejected() {
        let err = plan("SELECT a FROM t WHERE COUNT(*) > 1", &[], &agg_catalog()).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "got {err}");
    }

    #[test]
    fn update_plans_assignments_and_filter() {
        let plan = plan("UPDATE docs SET id = id + 1 WHERE id > 2", &[], &catalog()).unwrap();
        let Plan::Update(update) = plan else {
            panic!("expected UPDATE")
        };
        assert_eq!(update.assignments.len(), 1);
        assert_eq!(update.assignments[0].0, 0);
        assert!(update.filter.is_some());
    }

    #[test]
    fn delete_plans_a_filter() {
        let plan = plan("DELETE FROM docs WHERE id = 1", &[], &catalog()).unwrap();
        let Plan::Delete(delete) = plan else {
            panic!("expected DELETE")
        };
        assert!(delete.filter.is_some());
    }

    // ------------------------------------------------------------ collations

    /// A table whose three text columns declare the three collations, so a
    /// resolution rule can be read off the plan rather than off a result set.
    fn collated_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                temporary: false,
                primary_key: Vec::new(),
                name: "t".to_string(),
                columns: vec![
                    Column::new("id", DataType::Integer),
                    Column::new("nc", DataType::Text).with_collation(Collation::NoCase),
                    Column::new("bin", DataType::Text),
                    Column::new("rt", DataType::Text).with_collation(Collation::RTrim),
                ],
                strict: false,
            })
            .unwrap();
        catalog
    }

    /// The collation the top-level comparison of a `WHERE` clause resolved.
    fn where_collation(sql: &str) -> Collation {
        let Plan::Select(select) = plan(sql, &[], &collated_catalog()).unwrap() else {
            panic!("expected a SELECT")
        };
        match select.filter.expect("a filter") {
            PlanExpr::Binary { collation, .. } => collation,
            PlanExpr::InList { collation, .. } => collation,
            PlanExpr::Subquery {
                op: SubqueryOp::In { collation, .. },
                ..
            } => collation,
            other => panic!("not a comparison: {other:?}"),
        }
    }

    /// SQLite's rules, each one a case where getting it wrong returns a
    /// different number of rows. Every expectation here was confirmed against
    /// the sqlite3 binary as well; this pins *where the decision is made*,
    /// which a result set cannot show.
    #[test]
    fn a_comparison_resolves_the_collation_sqlite_resolves() {
        // The column's own collation, from either side.
        assert_eq!(
            where_collation("SELECT id FROM t WHERE nc = 'x'"),
            Collation::NoCase
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE 'x' = nc"),
            Collation::NoCase
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE bin = 'x'"),
            Collation::Binary
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE rt = 'x'"),
            Collation::RTrim
        );

        // An explicit COLLATE beats an implicit one, on either side.
        assert_eq!(
            where_collation("SELECT id FROM t WHERE nc = 'x' COLLATE BINARY"),
            Collation::Binary
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE bin COLLATE NOCASE = 'x'"),
            Collation::NoCase
        );
        // Two explicit ones: the left wins.
        assert_eq!(
            where_collation("SELECT id FROM t WHERE bin COLLATE RTRIM = 'x' COLLATE NOCASE"),
            Collation::RTrim
        );

        // Two columns: the *left* one wins, and a column that declared nothing
        // still has the default — which is why this is not `NoCase`.
        assert_eq!(
            where_collation("SELECT id FROM t WHERE bin = nc"),
            Collation::Binary
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE nc = bin"),
            Collation::NoCase
        );

        // `CAST` and unary `+` are transparent to a column's collation; `||`
        // is not, because its result is not a column.
        assert_eq!(
            where_collation("SELECT id FROM t WHERE CAST(nc AS TEXT) = 'x'"),
            Collation::NoCase
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE +nc = 'x'"),
            Collation::NoCase
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE nc || '' = 'x'"),
            Collation::Binary
        );
        // But an explicit one propagates out of anything.
        assert_eq!(
            where_collation("SELECT id FROM t WHERE (bin COLLATE NOCASE) || '' = 'x'"),
            Collation::NoCase
        );

        // Neither side has one: BINARY.
        assert_eq!(
            where_collation("SELECT id FROM t WHERE 'a' = 'b'"),
            Collation::Binary
        );
    }

    /// `IN` over a written list takes the collation of its **left operand
    /// alone** — SQLite codes every `OP_Eq` from `pExpr->pLeft` and never looks
    /// at the list. `IN (SELECT ...)` is the ordinary two-operand rule, because
    /// the subquery's column is a real operand of the comparison.
    #[test]
    fn in_resolves_differently_for_a_list_and_for_a_subquery() {
        assert_eq!(
            where_collation("SELECT id FROM t WHERE nc IN ('x')"),
            Collation::NoCase
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE 'x' IN (nc)"),
            Collation::Binary,
            "a list contributes no collation, even when it is a NOCASE column"
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE 'x' IN (SELECT nc FROM t)"),
            Collation::NoCase,
            "a subquery's column is an operand and does contribute one"
        );
        assert_eq!(
            where_collation("SELECT id FROM t WHERE bin IN (SELECT nc FROM t)"),
            Collation::Binary,
            "the probe has one of its own, so the subquery's is not reached"
        );
        // …unless the subquery's is *explicit*, which beats an implicit one
        // wherever it sits. That is the third step of the rule, and it is the
        // one an "left, else right" shortcut gets wrong.
        assert_eq!(
            where_collation("SELECT id FROM t WHERE bin IN (SELECT nc COLLATE NOCASE FROM t)"),
            Collation::NoCase
        );
        // An explicit one on the probe still outranks it.
        assert_eq!(
            where_collation(
                "SELECT id FROM t WHERE bin COLLATE RTRIM IN (SELECT nc COLLATE NOCASE FROM t)"
            ),
            Collation::RTrim
        );
    }

    /// `x BETWEEN y AND z` is two comparisons and SQLite resolves each on its
    /// own, so a `COLLATE` on one bound applies to that bound alone.
    #[test]
    fn between_resolves_its_two_bounds_separately() {
        let Plan::Select(select) = plan(
            "SELECT id FROM t WHERE bin BETWEEN 'a' AND 'b' COLLATE NOCASE",
            &[],
            &collated_catalog(),
        )
        .unwrap() else {
            panic!("expected a SELECT")
        };
        let PlanExpr::Between {
            low_collation,
            high_collation,
            ..
        } = select.filter.expect("a filter")
        else {
            panic!("expected a BETWEEN")
        };
        assert_eq!(low_collation, Collation::Binary);
        assert_eq!(high_collation, Collation::NoCase);
    }

    /// A simple `CASE` is one `=` per `WHEN`, and each resolves against the
    /// operand separately.
    #[test]
    fn a_simple_case_resolves_a_collation_per_branch() {
        let Plan::Select(select) = plan(
            "SELECT CASE bin WHEN 'a' COLLATE NOCASE THEN 1 WHEN 'b' THEN 2 END FROM t",
            &[],
            &collated_catalog(),
        )
        .unwrap() else {
            panic!("expected a SELECT")
        };
        let SelectItem::Expr { expr, .. } = &select.items[0] else {
            panic!("expected an expression item")
        };
        let PlanExpr::Case {
            branch_collations, ..
        } = expr
        else {
            panic!("expected a CASE")
        };
        assert_eq!(
            branch_collations,
            &[Collation::NoCase, Collation::Binary],
            "the explicit COLLATE belongs to its own branch and no other"
        );
    }

    /// `ORDER BY`, `GROUP BY`, `DISTINCT` and an aggregate's argument each ask
    /// the single-operand rule: an explicit `COLLATE`, else the column's, else
    /// `BINARY`.
    #[test]
    fn the_folding_and_ordering_clauses_carry_their_collations() {
        let select = |sql: &str| match plan(sql, &[], &collated_catalog()).unwrap() {
            Plan::Select(select) => *select,
            other => panic!("expected a SELECT, got {other:?}"),
        };

        let ordered = select("SELECT id FROM t ORDER BY nc, bin, rt");
        assert_eq!(
            ordered
                .order
                .iter()
                .map(|term| term.collation)
                .collect::<Vec<_>>(),
            vec![Collation::NoCase, Collation::Binary, Collation::RTrim]
        );
        assert_eq!(
            select("SELECT id FROM t ORDER BY bin COLLATE NOCASE").order[0].collation,
            Collation::NoCase
        );
        // An `ORDER BY` term wrapped in `COLLATE` still resolves against the
        // *result* columns first, which is where SQLite looks.
        assert_eq!(
            select("SELECT nc AS bin FROM t ORDER BY bin").order[0].key,
            OrderKey::Column(1),
            "a bare name is an output alias before it is a table column"
        );

        let grouped = select("SELECT nc FROM t GROUP BY nc");
        assert_eq!(grouped.group_collations, vec![Collation::NoCase]);
        let grouped = select("SELECT bin FROM t GROUP BY bin COLLATE RTRIM");
        assert_eq!(grouped.group_collations, vec![Collation::RTrim]);

        let distinct = select("SELECT DISTINCT nc, bin, rt FROM t");
        assert_eq!(
            distinct.distinct_collations,
            vec![Collation::NoCase, Collation::Binary, Collation::RTrim]
        );
        // Nothing reads them when the query is not `DISTINCT`, so nothing is
        // computed for it either.
        assert!(select("SELECT nc FROM t").distinct_collations.is_empty());

        let aggregated = select("SELECT MIN(nc), COUNT(DISTINCT bin) FROM t");
        assert_eq!(
            aggregated
                .aggregates
                .iter()
                .map(|a| a.collation)
                .collect::<Vec<_>>(),
            vec![Collation::NoCase, Collation::Binary]
        );
    }

    /// A derived table's synthetic column carries the projected expression's
    /// collation, the way `sqlite3SelectAddColumnTypeAndCollation` does.
    ///
    /// Without this a collation is lost the moment a query nests — silently,
    /// which is the exact shape of the bug this whole change closes.
    #[test]
    fn a_derived_table_carries_its_projections_collations() {
        let columns = |sql: &str| -> Vec<Collation> {
            let Plan::Select(select) = plan(sql, &[], &collated_catalog()).unwrap() else {
                panic!("expected a SELECT")
            };
            select.from[0]
                .table
                .columns
                .iter()
                .map(|column| column.collation)
                .collect()
        };

        assert_eq!(
            columns("SELECT s FROM (SELECT nc AS s, bin AS b, rt AS r FROM t) d"),
            vec![Collation::NoCase, Collation::Binary, Collation::RTrim]
        );
        // An expression's collation is the expression's, not its column's:
        // `||` carries none out, an explicit `COLLATE` carries its own, and
        // `CAST` is transparent.
        assert_eq!(
            columns("SELECT s FROM (SELECT nc || '' AS s FROM t) d"),
            vec![Collation::Binary]
        );
        assert_eq!(
            columns("SELECT s FROM (SELECT bin COLLATE NOCASE AS s FROM t) d"),
            vec![Collation::NoCase]
        );
        assert_eq!(
            columns("SELECT s FROM (SELECT CAST(nc AS TEXT) AS s FROM t) d"),
            vec![Collation::NoCase]
        );
        // And it is the collation, not the type, that survives: a derived
        // column has no declared type to carry.
        let Plan::Select(select) = plan(
            "SELECT s FROM (SELECT nc AS s FROM t) d",
            &[],
            &collated_catalog(),
        )
        .unwrap() else {
            panic!("expected a SELECT")
        };
        assert_eq!(select.from[0].table.columns[0].ty, DataType::Numeric);
    }

    /// A collation name this engine does not have is refused rather than
    /// treated as `BINARY`, wherever it is written.
    #[test]
    fn an_unknown_collation_is_refused_wherever_it_appears() {
        for sql in [
            "SELECT id FROM t WHERE nc = 'x' COLLATE utf8mb4_unicode_ci",
            "SELECT id FROM t ORDER BY nc COLLATE unicode",
            "SELECT id FROM t GROUP BY nc COLLATE fancy",
        ] {
            let error = plan(sql, &[], &collated_catalog()).unwrap_err();
            assert!(
                matches!(error, Error::Unsupported(_)),
                "`{sql}` gave {error} instead of a refusal"
            );
        }
        let error = plan(
            "CREATE TABLE u (a TEXT COLLATE utf8mb4_unicode_ci)",
            &[],
            &Catalog::new(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    }

    /// A `UNIQUE` index may not be keyed under a collation its column did not
    /// declare: the probe and the scan that both enforce the constraint would
    /// then disagree about what a duplicate is.
    #[test]
    fn a_unique_index_may_not_override_its_columns_collation() {
        let error = plan(
            "CREATE UNIQUE INDEX i ON t (bin COLLATE NOCASE)",
            &[],
            &collated_catalog(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
        assert!(
            error.to_string().contains("declare the collation"),
            "{error}"
        );

        // A *non*-unique index may, which is the whole point of writing it.
        let Plan::CreateIndex(create) = plan(
            "CREATE INDEX i ON t (bin COLLATE NOCASE) USING BTREE",
            &[],
            &collated_catalog(),
        )
        .unwrap() else {
            panic!("expected a CREATE INDEX")
        };
        assert_eq!(create.collations, vec![Collation::NoCase]);

        // And one that repeats the column's own collation is fine either way.
        let Plan::CreateIndex(create) = plan(
            "CREATE UNIQUE INDEX i ON t (nc COLLATE NOCASE)",
            &[],
            &collated_catalog(),
        )
        .unwrap() else {
            panic!("expected a CREATE INDEX")
        };
        assert_eq!(create.collations, vec![Collation::NoCase]);
    }

    /// An index with no `COLLATE` of its own inherits the column's, which is
    /// what makes `CREATE INDEX i ON t (nc)` answer `WHERE nc = ?`.
    #[test]
    fn an_index_inherits_its_columns_collation() {
        let Plan::CreateIndex(create) = plan(
            "CREATE INDEX i ON t (nc, bin, rt) USING BTREE",
            &[],
            &collated_catalog(),
        )
        .unwrap() else {
            panic!("expected a CREATE INDEX")
        };
        assert_eq!(
            create.collations,
            vec![Collation::NoCase, Collation::Binary, Collation::RTrim]
        );
    }

    /// `USING FULLTEXT` (or `USING BM25`) is what turns a multi-column
    /// `CREATE INDEX` into a combined BM25 index — a bare one still means a
    /// B-tree, exactly as it always has, even over two `TEXT` columns.
    #[test]
    fn using_fulltext_makes_a_multi_column_create_index_a_full_text_one() {
        for using in ["USING FULLTEXT", "USING BM25"] {
            let Plan::CreateIndex(create) = plan(
                &alloc::format!("CREATE INDEX i ON t (bin, rt) {using}"),
                &[],
                &collated_catalog(),
            )
            .unwrap() else {
                panic!("expected a CREATE INDEX for {using}")
            };
            assert_eq!(create.kind, IndexKind::FullText, "for {using}");
            assert_eq!(create.columns.len(), 2, "for {using}");
            assert!(!create.unique, "for {using}");
        }
    }

    #[test]
    fn a_bare_multi_column_create_index_stays_a_b_tree() {
        let Plan::CreateIndex(create) =
            plan("CREATE INDEX i ON t (bin, rt)", &[], &collated_catalog()).unwrap()
        else {
            panic!("expected a CREATE INDEX")
        };
        assert_eq!(create.kind, IndexKind::BTree);
    }

    #[test]
    fn a_multi_column_full_text_index_may_not_be_unique() {
        let error = plan(
            "CREATE UNIQUE INDEX i ON t (bin, rt) USING FULLTEXT",
            &[],
            &collated_catalog(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    }

    #[test]
    fn a_multi_column_full_text_index_may_not_carry_collate() {
        let error = plan(
            "CREATE INDEX i ON t (bin COLLATE NOCASE, rt) USING FULLTEXT",
            &[],
            &collated_catalog(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("has no collated key"), "{error}");
    }

    #[test]
    fn a_multi_column_full_text_index_needs_every_column_to_be_text() {
        let error = plan(
            "CREATE INDEX i ON t (bin, id) USING FULLTEXT",
            &[],
            &collated_catalog(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Type(_)), "{error:?}");
    }

    /// A vector index still covers exactly one column — two `VECTOR` columns
    /// are generally two different embedding spaces — and the message names
    /// that reason rather than reporting the `B-tree` error a fallthrough to
    /// `btree_plan` would give.
    #[test]
    fn using_vector_on_two_columns_is_refused_with_its_own_reason() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                temporary: false,
                primary_key: Vec::new(),
                name: "vecs".to_string(),
                columns: vec![
                    Column::new("a", DataType::Vector(4)),
                    Column::new("b", DataType::Vector(4)),
                ],
                strict: false,
            })
            .unwrap();
        let error = plan("CREATE INDEX i ON vecs (a, b) USING VECTOR", &[], &catalog).unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("exactly one column"), "{error}");
    }

    /// `bm25_score(a, b, ..., query)` — every argument but the last names a
    /// column; the two-argument case is the single-column call this has
    /// always accepted.
    #[test]
    fn bm25_score_accepts_more_than_one_column() {
        let Plan::Select(select) = plan(
            "SELECT id, bm25_score(bin, rt, 'x') AS score FROM t",
            &[],
            &collated_catalog(),
        )
        .unwrap() else {
            panic!("expected a SELECT")
        };
        let Some(ScoreExpr::Text { columns, .. }) = select.score else {
            panic!("expected a text score")
        };
        assert_eq!(columns, vec![2, 3]);
    }

    #[test]
    fn bm25_score_needs_at_least_one_column_and_a_query() {
        let error = plan(
            "SELECT bm25_score('just-a-query') FROM t",
            &[],
            &collated_catalog(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Type(_)), "{error:?}");
    }
}
