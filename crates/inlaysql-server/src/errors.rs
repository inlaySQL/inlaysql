//! Engine errors, translated into the codes a MySQL client expects.
//!
//! The mapping matters more than it looks. A driver does not read the message —
//! it branches on the code. An ORM retries `1213`, reports `1062` as "this
//! record already exists", and treats an unknown code as a fatal connection
//! fault. Returning a plausible-looking message under the wrong number turns a
//! recoverable condition into a crash, so anything this server cannot classify
//! becomes `1105` (unknown error) rather than a guess that reads better.

use inlaysql::Error;

/// A MySQL error, ready to be put on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlError {
    /// The numeric code the client branches on.
    pub code: u16,
    /// The five-character SQLSTATE.
    pub sqlstate: &'static str,
    /// The human-readable message.
    pub message: String,
}

impl MysqlError {
    /// Build an error.
    pub fn new(code: u16, sqlstate: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            sqlstate,
            message: message.into(),
        }
    }

    /// `ER_PARSE_ERROR` — the statement is not valid SQL here.
    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(1064, "42000", message)
    }

    /// `ER_NOT_SUPPORTED_YET` — understood, but not implemented.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(1235, "42000", message)
    }

    /// `ER_BAD_FIELD_ERROR` — no such column.
    pub fn bad_field(message: impl Into<String>) -> Self {
        Self::new(1054, "42S22", message)
    }

    /// `ER_NO_SUCH_TABLE`.
    pub fn no_such_table(name: &str) -> Self {
        Self::new(1146, "42S02", format!("Table '{name}' doesn't exist"))
    }

    /// `ER_UNKNOWN_TABLE` — a qualifier in a field list (e.g. the left side of
    /// an `UPDATE ... SET`) names something other than a table the statement
    /// reads or writes. MySQL's own wording and code for exactly this.
    pub fn unknown_table_in_field_list(name: &str) -> Self {
        Self::new(
            1109,
            "42S02",
            format!("Unknown table '{name}' in field list"),
        )
    }

    /// `ER_ACCESS_DENIED_ERROR`.
    pub fn access_denied(user: &str, with_password: bool) -> Self {
        Self::new(
            1045,
            "28000",
            format!(
                "Access denied for user '{user}' (using password: {})",
                if with_password { "YES" } else { "NO" }
            ),
        )
    }

    /// `ER_CON_COUNT_ERROR` — too many connections.
    pub fn too_many_connections() -> Self {
        Self::new(1040, "08004", "Too many connections")
    }

    /// `ER_UNKNOWN_COM_ERROR` — a command byte this server does not implement.
    pub fn unknown_command(byte: u8) -> Self {
        Self::new(
            1047,
            "08S01",
            format!("Unknown command (0x{byte:02x}) — this server implements a subset of the MySQL protocol"),
        )
    }

    /// `ER_UNKNOWN_ERROR`, the honest answer when nothing else fits.
    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new(1105, "HY000", message)
    }
}

impl std::fmt::Display for MysqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ERROR {} ({}): {}",
            self.code, self.sqlstate, self.message
        )
    }
}

/// Translate an engine error.
///
/// [`Error::Catalog`] carries several distinct conditions in one variant, so
/// its message is inspected to separate them: a client that asks for a missing
/// table has to get `1146`, not a generic failure, or `Schema::hasTable` in
/// every ORM stops working.
///
/// The match below has deliberately no catch-all arm: a new engine error
/// variant should fail this build and be given a code on purpose, rather than
/// defaulting to whichever one happens to compile.
pub fn from_engine(error: &Error) -> MysqlError {
    match error {
        Error::Parse(message) => {
            MysqlError::parse(format!("You have an error in your SQL syntax: {message}"))
        }

        Error::Unsupported(message) => MysqlError::unsupported(format!(
            "{message} — InlaySQL implements a subset of SQL; see `docs/server.md`"
        )),

        Error::Catalog(message) => classify_catalog(message),

        Error::Constraint(message) => classify_constraint(message),

        // `HY000` rather than a `22xxx` class, which is what MySQL itself uses
        // for 1366. It is not pedantry: PDO renders SQLSTATE `22007` as
        // "Invalid datetime format", so a plain integer/text mismatch was
        // reaching users as a date error and sending them looking in entirely
        // the wrong place.
        Error::Type(message) => {
            MysqlError::new(1366, "HY000", format!("Incorrect value: {message}"))
        }

        Error::Bind(message) => MysqlError::new(
            1210,
            "HY000",
            format!("Incorrect arguments to EXECUTE: {message}"),
        ),

        // First-committer-wins looks exactly like a deadlock rollback to a
        // client: nothing was written, and retrying is correct. 1213 is the
        // code whose documented remedy is "retry the transaction", which is
        // why every ORM's retry logic already recognises it.
        Error::Conflict => MysqlError::new(
            1213,
            "40001",
            "Deadlock found when trying to get lock; try restarting transaction \
             (InlaySQL: another writer committed first, nothing was written)",
        ),

        Error::Transaction(message) => {
            MysqlError::new(1568, "25001", format!("Transaction error: {message}"))
        }

        // A stale plan is retried in-process; if one still reaches here the
        // schema changed under a prepared statement and re-preparing is the
        // client's move.
        Error::Stale(message) => MysqlError::new(
            1615,
            "HY000",
            format!("Prepared statement needs to be re-prepared: {message}"),
        ),

        Error::Storage(message) => classify_storage(message),

        Error::Index(message) => {
            MysqlError::new(1030, "HY000", format!("Got error from index: {message}"))
        }

        Error::Corrupt(message) => MysqlError::new(
            1194,
            "HY000",
            format!("Table is marked as crashed: {message}"),
        ),

        Error::FormatVersion(message) => {
            MysqlError::new(1112, "42000", format!("Unsupported file format: {message}"))
        }

        // 1301 is MySQL's own answer to this exact situation — a string
        // function whose result outgrew what the server will hand back. MySQL
        // truncates and warns where we refuse outright, which is the
        // divergence `docs/server.md` records rather than the code choice.
        Error::TooBig(message) => MysqlError::new(
            1301,
            "HY000",
            format!("Result of a string function was too large: {message}"),
        ),

        // `ER_OUT_OF_SORTMEMORY`, whose SQLSTATE is HY001 (memory allocation
        // error) and whose MySQL wording — "consider increasing the sort buffer
        // size" — describes this exactly: a blocking operator wanted more room
        // than it was given. It is the code a driver already classifies as a
        // resource failure rather than a bad statement, which is what decides
        // whether an ORM retries or reports, and it is the right classification
        // here: the same statement over fewer rows succeeds.
        Error::Memory(message) => {
            MysqlError::new(1038, "HY001", format!("Out of memory: {message}"))
        }

        // Two codes for two conditions, because a client acts on them
        // differently and MySQL already taught it how.
        //
        // `ER_QUERY_TIMEOUT` (3024) is what `max_execution_time` raises, and
        // its SQLSTATE `HY000` is what a driver maps to a retryable
        // resource condition rather than a bad statement — the same statement
        // with a `LIMIT`, or on a quieter server, succeeds.
        //
        // `ER_QUERY_INTERRUPTED` (1317, SQLSTATE `70100`) is what `KILL`
        // raises. A pool that sees it knows the connection is still good and
        // that a human made a decision, so retrying immediately is exactly
        // what it must not do.
        Error::Cancelled(inlaysql::Stopped::Timeout) => MysqlError::new(
            3024,
            "HY000",
            format!(
                "Query execution was interrupted, maximum statement execution time exceeded \
                 (InlaySQL: {})",
                inlaysql::Stopped::Timeout.message()
            ),
        ),
        Error::Cancelled(inlaysql::Stopped::Killed) => MysqlError::new(
            1317,
            "70100",
            format!(
                "Query execution was interrupted (InlaySQL: {})",
                inlaysql::Stopped::Killed.message()
            ),
        ),
    }
}

/// Split [`Error::Constraint`] by which constraint failed.
///
/// This used to map every constraint to `1062 ER_DUP_ENTRY`, and the comment
/// said why: a duplicate `INTEGER PRIMARY KEY` was the only constraint the
/// engine enforced. AHL-412 added `NOT NULL`, `UNIQUE` and `CHECK`, and the
/// blanket mapping then reported a null in a `NOT NULL` column as a *duplicate
/// entry* — a client is told the row already exists when the real problem is a
/// missing value, which sends whoever reads the log looking in the wrong place.
///
/// The engine spells these messages the way SQLite does
/// (`NOT NULL constraint failed: t.c`), so the prefix is the classifier. An
/// unrecognised one stays `1062`, because a duplicate key is still the case a
/// client is most likely to be branching on.
fn classify_constraint(message: &str) -> MysqlError {
    let lower = message.to_ascii_lowercase();
    if lower.starts_with("not null constraint failed") {
        // MySQL's own wording is "Column 'x' cannot be null"; the column is
        // already named in the engine's message, so it is passed through
        // rather than reformatted into something less precise.
        return MysqlError::new(1048, "23000", format!("Column cannot be null: {message}"));
    }
    if lower.starts_with("check constraint failed") {
        // 3819 is MySQL 8's ER_CHECK_CONSTRAINT_VIOLATED, and it is HY000
        // rather than a 23xxx class — MySQL's own choice, not ours.
        return MysqlError::new(
            3819,
            "HY000",
            format!("Check constraint violated: {message}"),
        );
    }
    MysqlError::new(1062, "23000", format!("Duplicate entry: {message}"))
}

/// Split [`Error::Catalog`] into the codes clients branch on.
fn classify_catalog(message: &str) -> MysqlError {
    let lower = message.to_ascii_lowercase();
    if let Some(name) = lower.strip_prefix("no such table: ") {
        return MysqlError::no_such_table(name.trim());
    }
    if lower.starts_with("no such index") {
        return MysqlError::new(1176, "42000", format!("Key does not exist: {message}"));
    }
    if lower.contains("already exists") {
        let code = if lower.starts_with("index") {
            1061
        } else {
            1050
        };
        return MysqlError::new(code, "42S01", message.to_string());
    }
    if lower.starts_with("no column") || lower.starts_with("no such column") {
        return MysqlError::bad_field(format!("Unknown column: {message}"));
    }
    MysqlError::new(1146, "42S02", message.to_string())
}

/// Split [`Error::Storage`] so a read-only refusal is not reported as I/O
/// failure. The refusal is generated by `Database::check_writable`, which is
/// the planning-based check — this only classifies what it already decided.
fn classify_storage(message: &str) -> MysqlError {
    if message.contains("read-only") {
        return MysqlError::new(
            1290,
            "HY000",
            "The MySQL server is running with the --read-only option so it cannot \
             execute this statement",
        );
    }
    MysqlError::new(
        1030,
        "HY000",
        format!("Got error from storage engine: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duplicate_key_is_1062() {
        let error = from_engine(&Error::Constraint("duplicate id 1".into()));
        assert_eq!(error.code, 1062);
        assert_eq!(error.sqlstate, "23000");
    }

    /// Each constraint the engine enforces gets its own code. They all used to
    /// come back as 1062, so a null in a `NOT NULL` column was reported to the
    /// client as a duplicate entry.
    #[test]
    fn each_constraint_gets_its_own_code() {
        let null = from_engine(&Error::Constraint(
            "NOT NULL constraint failed: users.name".into(),
        ));
        assert_eq!(null.code, 1048, "ER_BAD_NULL_ERROR");
        assert_eq!(null.sqlstate, "23000");
        assert!(null.message.contains("users.name"), "{}", null.message);

        let check = from_engine(&Error::Constraint(
            "CHECK constraint failed: age > 0".into(),
        ));
        assert_eq!(check.code, 3819, "ER_CHECK_CONSTRAINT_VIOLATED");
        assert_eq!(check.sqlstate, "HY000", "MySQL's own class for 3819");

        let unique = from_engine(&Error::Constraint(
            "UNIQUE constraint failed: users.email".into(),
        ));
        assert_eq!(unique.code, 1062, "ER_DUP_ENTRY");
        assert_eq!(unique.sqlstate, "23000");
    }

    #[test]
    fn a_missing_table_is_1146_and_names_the_table() {
        let error = from_engine(&Error::Catalog("no such table: users".into()));
        assert_eq!(error.code, 1146);
        assert_eq!(error.sqlstate, "42S02");
        assert!(error.message.contains("users"), "got {}", error.message);
    }

    #[test]
    fn an_existing_table_is_1050() {
        assert_eq!(
            from_engine(&Error::Catalog("table `users` already exists".into())).code,
            1050
        );
    }

    #[test]
    fn a_missing_column_is_1054() {
        assert_eq!(
            from_engine(&Error::Catalog("no column `x` on table `t`".into())).code,
            1054
        );
    }

    /// The mapping the whole retry story rests on: a lost write race has to
    /// arrive as the code an ORM already knows to retry.
    #[test]
    fn a_write_conflict_is_the_retryable_deadlock_code() {
        let error = from_engine(&Error::Conflict);
        assert_eq!(error.code, 1213);
        assert_eq!(error.sqlstate, "40001");
    }

    #[test]
    fn a_parse_failure_is_1064() {
        assert_eq!(from_engine(&Error::Parse("bad".into())).code, 1064);
    }

    /// An unimplemented feature must say so rather than looking like a syntax
    /// error the user could fix by rewriting the query.
    #[test]
    fn unsupported_is_distinct_from_a_syntax_error() {
        let error = from_engine(&Error::Unsupported("DISTINCT is not supported yet".into()));
        assert_eq!(error.code, 1235);
        assert!(error.message.contains("DISTINCT"), "got {}", error.message);
    }

    #[test]
    fn a_read_only_refusal_is_not_reported_as_an_io_failure() {
        let error = from_engine(&Error::Storage(
            "this database handle is open read-only; refusing to run a write statement: `INSERT`"
                .into(),
        ));
        assert_eq!(error.code, 1290);
        assert_ne!(error.code, 1030);
    }

    #[test]
    fn an_ordinary_storage_failure_is_1030() {
        assert_eq!(
            from_engine(&Error::Storage("disk on fire".into())).code,
            1030
        );
    }

    /// Found by pointing PHP's PDO at this server: a `22xxx` SQLSTATE on a type
    /// mismatch is rendered by the driver as "Invalid datetime format", which
    /// sends the reader hunting for a date bug that does not exist.
    #[test]
    fn a_type_mismatch_is_not_reported_under_a_datetime_sqlstate() {
        let error = from_engine(&Error::Type("cannot compare INTEGER and TEXT".into()));
        assert_eq!(error.code, 1366);
        assert_eq!(error.sqlstate, "HY000");
        assert!(
            !error.sqlstate.starts_with("22"),
            "a 22xxx class makes drivers call this a datetime error"
        );
    }
}
