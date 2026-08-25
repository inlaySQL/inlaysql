//! Error type shared by the whole engine.

use alloc::string::String;
use core::fmt;

/// Result alias used throughout InlaySQL.
pub type Result<T> = core::result::Result<T, Error>;

/// Everything that can go wrong inside the core engine.
///
/// Backend crates map their own failures onto [`Error::Storage`] and
/// [`Error::Index`] so the core never has to know about redb or tantivy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The SQL text could not be parsed.
    Parse(String),
    /// The SQL parsed, but uses a feature this stage does not implement yet.
    Unsupported(String),
    /// A table or column does not exist, or already exists.
    Catalog(String),
    /// A value does not fit the column it was written to, or an expression
    /// was given an argument of the wrong type.
    Type(String),
    /// A declared constraint was violated — today, a duplicate primary key.
    Constraint(String),
    /// The supplied bind parameters do not match the placeholders in the SQL.
    Bind(String),
    /// A prepared statement was planned against a schema this database no
    /// longer has, so its column ordinals can no longer be trusted.
    ///
    /// Nothing was read or written. Prepare the statement again against the
    /// current catalog; that is always the correct response.
    Stale(String),
    /// The storage backend failed.
    Storage(String),
    /// Another writer committed first, so this transaction was based on a
    /// stale snapshot and was rolled back (first-committer-wins).
    ///
    /// Nothing was written. The handle has been reloaded from the winner's
    /// committed state, so retrying the statement is the correct response.
    Conflict,
    /// A transaction was misused: begun while one was already open, committed
    /// or rolled back while none was, or grown past what the storage backend
    /// can hold in a single commit.
    Transaction(String),
    /// An index backend failed.
    Index(String),
    /// Persisted bytes could not be decoded.
    Corrupt(String),
    /// The database file's on-disk format version does not match this binary.
    ///
    /// A file written by a *newer* binary is not corruption — it is from the
    /// future, and the message says so. A file written by an *older* binary is
    /// likewise not corruption; it is a format this build no longer opens (see
    /// `docs/recovery.md` for the policy). Either way the file is not read.
    FormatVersion(String),
    /// An expression asked for a string or blob larger than
    /// [`crate::eval::MAX_LENGTH`].
    ///
    /// This is SQLite's `SQLITE_TOOBIG`, and it exists for the same reason:
    /// the string functions compose, so their *output* sizes multiply.
    /// `replace(x, 'a', 'aaaa')` nested forty deep asks for 4^40 bytes from an
    /// 810-byte statement, and without a bound the engine spends the rest of
    /// its life trying to build it. The length is computed before the
    /// allocation is attempted, so this is a refusal rather than a failed
    /// `Vec` growth.
    TooBig(String),
    /// A statement's working set passed
    /// [`crate::EngineOptions::query_memory_bytes`].
    ///
    /// `ORDER BY`, `GROUP BY`, `DISTINCT` and window functions are blocking by
    /// definition: none can emit its first row before it has seen its last
    /// input row, so each holds its whole input at once. Without a ceiling the
    /// only thing that stops one is the operating system, and what the
    /// operating system does is kill the process — which on a server takes
    /// every other connection with it. This is the refusal that happens
    /// instead: one statement fails, the handle is untouched, and the
    /// connection that asked can retry with a `LIMIT` or a narrower `WHERE`.
    ///
    /// Nothing was written; a read cannot have written anything, and this is
    /// raised while the input is being collected, before any fold or sort.
    Memory(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(m) => write!(f, "parse error: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::Catalog(m) => write!(f, "catalog error: {m}"),
            Error::Type(m) => write!(f, "type error: {m}"),
            Error::Constraint(m) => write!(f, "constraint failed: {m}"),
            Error::Bind(m) => write!(f, "bind error: {m}"),
            Error::Stale(m) => write!(f, "stale prepared statement: {m}"),
            Error::Storage(m) => write!(f, "storage error: {m}"),
            Error::Conflict => write!(
                f,
                "write conflict: another writer committed first, nothing was written"
            ),
            Error::Transaction(m) => write!(f, "transaction error: {m}"),
            Error::Index(m) => write!(f, "index error: {m}"),
            Error::Corrupt(m) => write!(f, "corrupt data: {m}"),
            Error::FormatVersion(m) => write!(f, "format version mismatch: {m}"),
            Error::TooBig(m) => write!(f, "string or blob too big: {m}"),
            Error::Memory(m) => write!(f, "query memory limit exceeded: {m}"),
        }
    }
}

impl core::error::Error for Error {}
