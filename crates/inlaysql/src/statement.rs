//! The handle callers hold onto a prepared statement.

use std::sync::Arc;

/// A statement that has been parsed and planned, ready to run many times.
///
/// Prepare one with [`Database::prepare`](crate::Database::prepare) or
/// [`AsyncDatabase::prepare`](crate::AsyncDatabase::prepare), then run it with
/// `execute_prepared` / `query_prepared`, binding the `?` placeholders afresh
/// on every call.
///
/// ```
/// use inlaysql::{Database, Value};
///
/// let mut db = Database::open_in_memory()?;
/// db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
///
/// let insert = db.prepare("INSERT INTO kv (id, body) VALUES (?, ?)")?;
/// for id in 1..=3 {
///     db.execute_prepared(&insert, &[Value::Integer(id), Value::Text("x".into())])?;
/// }
///
/// let lookup = db.prepare("SELECT body FROM kv WHERE id = ?")?;
/// let row = db.query_prepared(&lookup, &[Value::Integer(2)])?;
/// assert_eq!(row.rows.len(), 1);
/// # Ok::<(), inlaysql::Error>(())
/// ```
///
/// # Cloning, sharing and the schema
///
/// The handle is reference-counted, so cloning it is free and the same
/// statement can be handed to several tasks — including across the boundary
/// into an [`AsyncDatabase`](crate::AsyncDatabase)'s I/O thread.
///
/// A statement is *not* bound to the database that prepared it, because a plan
/// is just data. What keeps that honest is the schema stamp it carries: every
/// execution re-checks the table definition its column ordinals were resolved
/// against, and a mismatch is [`Error::Stale`](crate::Error::Stale) rather than
/// a row read out of the wrong column.
#[derive(Debug, Clone)]
pub struct Statement {
    inner: Arc<inlaysql_core::Statement>,
}

impl Statement {
    pub(crate) fn new(inner: inlaysql_core::Statement) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    pub(crate) fn as_core(&self) -> &inlaysql_core::Statement {
        &self.inner
    }

    /// The statement text this was prepared from.
    pub fn sql(&self) -> &str {
        self.inner.sql()
    }

    /// How many `?` placeholders the statement has.
    pub fn parameter_count(&self) -> usize {
        self.inner.parameter_count()
    }

    /// Whether running this statement can only read.
    pub fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    /// This statement's output columns, in projection order.
    ///
    /// Empty for a statement that produces no rows (`CREATE TABLE`, an
    /// `INSERT` without `RETURNING`, `BEGIN`, and so on). A column's type is
    /// `None` where the plan does not statically know one: a computed
    /// expression, a retrieval score, or a `SELECT` with no `FROM` — the
    /// same line SQLite itself draws for prepared-statement metadata.
    ///
    /// ```
    /// use inlaysql::Database;
    ///
    /// let mut db = Database::open_in_memory()?;
    /// db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
    ///
    /// let select = db.prepare("SELECT id, body FROM kv WHERE id = ?")?;
    /// let columns = select.columns();
    /// assert_eq!(columns.len(), 2);
    /// assert_eq!(columns[0].name, "id");
    ///
    /// let insert = db.prepare("INSERT INTO kv (id, body) VALUES (?, ?)")?;
    /// assert!(insert.columns().is_empty());
    /// # Ok::<(), inlaysql::Error>(())
    /// ```
    pub fn columns(&self) -> &[inlaysql_core::ColumnInfo] {
        self.inner.columns()
    }
}
