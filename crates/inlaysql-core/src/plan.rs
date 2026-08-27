//! The logical plan the SQL front end produces and the executor consumes.
//!
//! Plans are resolved against the catalog: column names have become ordinals
//! and a table reference has been checked to exist. The executor therefore
//! never looks a name up again.
//!
//! What a plan does *not* contain is the parameter values. A `?` resolves to
//! [`Expr::Param`], an index into the slice supplied at execution. That is
//! what makes a plan reusable: the same plan runs with different parameters,
//! which is the whole point of [`crate::statement::Statement`].

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::catalog::{Column, IndexKind, Table, TableConstraints};
use crate::collation::Collation;
use crate::value::{DataType, Value};

/// A planned statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// `CREATE TABLE`.
    CreateTable(CreateTablePlan),
    /// `DROP TABLE`.
    DropTable(DropTablePlan),
    /// `ALTER TABLE`.
    AlterTable(AlterTablePlan),
    /// `CREATE INDEX`.
    CreateIndex(CreateIndexPlan),
    /// `CREATE UNIQUE INDEX`, which is recorded as a constraint rather than as
    /// an index — see [`CreateUniqueIndexPlan`].
    CreateUniqueIndex(CreateUniqueIndexPlan),
    /// `DROP INDEX`.
    DropIndex(DropIndexPlan),
    /// `INSERT`.
    Insert(Box<InsertPlan>),
    /// `SELECT ... FROM`.
    ///
    /// Boxed: a [`SelectPlan`] is far larger than the other variants, and
    /// boxing it keeps the whole [`Plan`] from growing to its size.
    Select(Box<SelectPlan>),
    /// `SELECT` with no `FROM`: evaluate each scalar expression once.
    Scalar(ScalarPlan),
    /// `UNION [ALL]` / `INTERSECT` / `EXCEPT`.
    SetOperation(Box<SetOperationPlan>),
    /// `UPDATE`.
    Update(UpdatePlan),
    /// `DELETE`.
    Delete(DeletePlan),
    /// `EXPLAIN <statement>` — describe the plan inside without running it.
    ///
    /// The inner plan is the whole of what `EXPLAIN` reports on: there is no
    /// second, parallel representation of a query for reporting purposes, so
    /// nothing here can describe a plan the executor would not run. What the
    /// plan alone does *not* hold is the access-path choices — which index,
    /// hash or probe — because those are made at execution against the bound
    /// parameters and the catalog. [`crate::explain`] asks the executor's own
    /// chooser for them rather than re-deriving them.
    Explain(Box<Plan>),
    /// `REINDEX [name]` — do the deferred retrieval-index build now.
    Reindex(ReindexPlan),
    /// `ANALYZE` — refresh the optional planner statistics for resolved tables.
    Analyze(AnalyzePlan),
    /// `BEGIN` / `BEGIN TRANSACTION`.
    Begin,
    /// `COMMIT` / `END`.
    Commit,
    /// `ROLLBACK`.
    Rollback,
    /// `SAVEPOINT name` — starts a transaction first if none is open.
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] name`.
    ReleaseSavepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] name`.
    RollbackToSavepoint(String),
}

impl Plan {
    /// The existing tables this plan reads or writes, in join order.
    ///
    /// Empty for `CREATE TABLE` (whose table does not exist yet) and for a
    /// `SELECT` without `FROM`. This is what a prepared statement re-checks
    /// before it runs: those two shapes depend on no schema, everything else
    /// depends on the tables it lists. `CREATE INDEX` lists its target table
    /// (whose column ordinal the plan holds); `DROP INDEX` depends on no
    /// table shape, only a name.
    ///
    /// `DROP TABLE` and `ALTER TABLE` name their target but hold no ordinals
    /// into it, so they are deliberately absent: stamping them would make a
    /// prepared `DROP TABLE` go stale for a change it is about to undo anyway.
    pub fn tables(&self) -> Vec<&str> {
        match self {
            // An `INSERT ... SELECT` depends on the shape of everything the
            // query reads as well as the table it writes, because the plan
            // holds ordinals into both.
            Plan::Insert(insert) => {
                let mut tables = alloc::vec![insert.table.as_str()];
                if let InsertSource::Select { query, .. } = &insert.source {
                    query.tables_read(&mut tables);
                }
                tables
            }
            Plan::Select(select) => {
                let mut tables = Vec::new();
                select.tables_read(&mut tables);
                tables
            }
            Plan::SetOperation(plan) => {
                let mut tables = Vec::new();
                plan.tables_read(&mut tables);
                tables
            }
            Plan::Update(update) => alloc::vec![update.table.as_str()],
            Plan::Delete(delete) => alloc::vec![delete.table.as_str()],
            Plan::CreateIndex(create) => alloc::vec![create.table.as_str()],
            // The same tables the statement itself depends on, and for the
            // same reason: `EXPLAIN` reads the inner plan's column ordinals to
            // name the columns an index is probed on, so an `ALTER TABLE` that
            // moved them has to make this stale too.
            Plan::Explain(inner) => inner.tables(),
            // `REINDEX` names its tables but holds no ordinal into any of
            // them: it rebuilds a retrieval index from whatever the rows say
            // now. Listing them here would make a prepared `REINDEX` go stale
            // for a column rename it does not care about.
            Plan::Analyze(analyze) => analyze.tables.iter().map(String::as_str).collect(),
            // Empty for an ordinary `CREATE TABLE`, per this method's own doc
            // comment — its table does not exist yet. `... AS SELECT` is the
            // exception: the plan holds ordinals into whatever its query
            // reads, exactly as `INSERT ... SELECT` does above.
            Plan::CreateTable(create) => match &create.as_select {
                Some(query) => {
                    let mut tables = Vec::new();
                    query.tables_read(&mut tables);
                    tables
                }
                None => Vec::new(),
            },
            Plan::DropTable(_)
            | Plan::AlterTable(_)
            | Plan::Scalar(_)
            | Plan::CreateUniqueIndex(_)
            | Plan::DropIndex(_)
            | Plan::Reindex(_)
            | Plan::Begin
            | Plan::Commit
            | Plan::Rollback
            | Plan::Savepoint(_)
            | Plan::ReleaseSavepoint(_)
            | Plan::RollbackToSavepoint(_) => Vec::new(),
        }
    }

    /// Whether running this plan can only read.
    ///
    /// Transaction control is read-only by this measure and that is
    /// deliberate: `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `RELEASE` and
    /// `ROLLBACK TO` write nothing themselves — the last replays only writes
    /// the same handle already made, which a read-only handle never has any
    /// of — and the statements *inside* a transaction are each judged on
    /// their own. A read-only handle can therefore take a consistent
    /// snapshot across several `SELECT`s, which is exactly what a
    /// transaction is for.
    ///
    /// `EXPLAIN` is read-only *whatever it wraps*, and that is load-bearing
    /// rather than a nicety: it is what stops `EXPLAIN DELETE FROM t` from
    /// taking the write path, being counted against a transaction's size
    /// budget, or being rolled back as a failed write. It never runs the
    /// statement inside it — see [`crate::explain`].
    ///
    /// `REINDEX` is deliberately **not** read-only even though it changes no
    /// row: it commits index structure into the database and saves index
    /// blobs, so a handle opened read-only cannot run it and must not be told
    /// it can.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Plan::Select(_)
                | Plan::Scalar(_)
                | Plan::SetOperation(_)
                | Plan::Explain(_)
                | Plan::Begin
                | Plan::Commit
                | Plan::Rollback
                | Plan::Savepoint(_)
                | Plan::ReleaseSavepoint(_)
                | Plan::RollbackToSavepoint(_)
        )
    }

    /// Every name this plan touches, and what it does to each.
    ///
    /// **Written for authorisation, and only sound because it is derived from
    /// the resolved plan rather than from the statement's text.** A caller
    /// deciding whether a user may run `SELECT (SELECT secret FROM vault) FROM
    /// public` has to see *both* tables; a keyword scan over the text that
    /// finds only the first one is a privilege bypass, not a cosmetic miss.
    /// The MySQL-wire server's per-table grants are checked against exactly
    /// this list — see `crates/inlaysql-server/src/acl.rs`.
    ///
    /// Deliberately *not* [`Plan::tables`], which exists for a different job:
    /// that one lists the tables whose *shape* a plan's ordinals depend on, so
    /// it leaves out `DROP TABLE`/`ALTER TABLE`'s own target (nothing to go
    /// stale) and misses the tables an `UPDATE`'s or `DELETE`'s subqueries
    /// read. Both omissions would be holes here.
    ///
    /// The name in each pair is a **table** name except for
    /// [`TableAccess::DropIndex`], where it is an *index* name — SQLite's
    /// `DROP INDEX` names no table at all, so a caller that needs one has to
    /// resolve it through the catalog. Keeping that case in its own variant is
    /// what stops an index name being read as a table name.
    ///
    /// The match below has no wildcard arm on purpose: a new [`Plan`] variant
    /// must fail to compile here rather than default to "touches nothing",
    /// which an authorisation caller would read as "anyone may run it".
    pub fn table_access(&self) -> Vec<(&str, TableAccess)> {
        let mut out = Vec::new();
        let mut reads: Vec<&str> = Vec::new();

        match self {
            // The table does not exist yet, so there is nothing to read; the
            // name is still the one a per-table grant would be written for.
            // `... AS SELECT` is the exception, and a load-bearing one: a
            // caller with `CREATE` but not `SELECT` on the source table must
            // not be able to copy it out through a new one, so its reads are
            // recorded exactly as an `INSERT ... SELECT`'s are below.
            Plan::CreateTable(create) => {
                out.push((create.table.name.as_str(), TableAccess::Create));
                if let Some(query) = &create.as_select {
                    query.tables_read(&mut reads);
                }
            }
            Plan::DropTable(drop) => out.push((drop.name.as_str(), TableAccess::Drop)),
            Plan::AlterTable(alter) => out.push((alter.table.as_str(), TableAccess::Alter)),
            // Building an index reads every row of the table to fill it, and
            // changes what the table costs to write for ever after. Both
            // spellings name their target, so both are attributable.
            Plan::CreateIndex(create) => {
                out.push((create.table.as_str(), TableAccess::Create));
                out.push((create.table.as_str(), TableAccess::Read));
            }
            Plan::CreateUniqueIndex(create) => {
                out.push((create.table.as_str(), TableAccess::Create));
                out.push((create.table.as_str(), TableAccess::Read));
            }
            Plan::DropIndex(drop) => out.push((drop.name.as_str(), TableAccess::DropIndex)),
            // Rebuilding an index reads every row of the table to fill it and
            // rewrites the structure that answers for it — the same two things
            // `CREATE INDEX` does, minus the declaration. Every table is named
            // because [`ReindexPlan`] resolved them at plan time; a bare
            // `REINDEX` that carried "all of them" as an absence would be
            // read here as "touches nothing", which is a grant nobody wrote.
            Plan::Reindex(reindex) => {
                for table in &reindex.tables {
                    out.push((table.as_str(), TableAccess::Alter));
                    out.push((table.as_str(), TableAccess::Read));
                }
            }
            // `ANALYZE` reads every row and replaces derived planner metadata.
            // Its table list is resolved at prepare time for the same
            // authorisation reason as `REINDEX`'s list.
            Plan::Analyze(analyze) => {
                for table in &analyze.tables {
                    out.push((table.as_str(), TableAccess::Alter));
                    out.push((table.as_str(), TableAccess::Read));
                }
            }
            Plan::Insert(insert) => {
                let target = insert.table.as_str();
                out.push((target, TableAccess::Insert));
                // The conflict policy can do more to the target than an insert
                // does, and each extra thing is a different privilege: `INSERT
                // OR REPLACE`/`REPLACE INTO` deletes the rows it collides
                // with, and `ON CONFLICT ... DO UPDATE` reads and rewrites
                // them. MySQL draws the same two lines.
                match &insert.on_conflict.action {
                    ConflictAction::Replace => out.push((target, TableAccess::Delete)),
                    ConflictAction::Update(update) => {
                        out.push((target, TableAccess::Update));
                        out.push((target, TableAccess::Read));
                        for (_, expr) in &update.assignments {
                            expr.tables_read(&mut reads);
                        }
                        if let Some(filter) = &update.filter {
                            filter.tables_read(&mut reads);
                        }
                    }
                    ConflictAction::Abort | ConflictAction::Ignore => {}
                }
                match &insert.source {
                    InsertSource::Values(rows) => {
                        for row in rows {
                            for cell in row.iter().flatten() {
                                cell.tables_read(&mut reads);
                            }
                        }
                    }
                    InsertSource::Select { query, .. } => query.tables_read(&mut reads),
                }
                if let Some(items) = &insert.returning {
                    out.push((target, TableAccess::Read));
                    select_items_tables_read(items, &mut reads);
                }
            }
            Plan::Update(update) => {
                let target = update.table.as_str();
                out.push((target, TableAccess::Update));
                // An `UPDATE` that picks its rows with a `WHERE`, computes a
                // new value from the old one, or projects a `RETURNING` is
                // reading the table as well as writing it, and MySQL wants
                // SELECT for exactly that. A blind `UPDATE t SET x = 1` reads
                // nothing and needs nothing extra.
                let mut reads_target = update.filter.is_some() || update.returning.is_some();
                for (_, expr) in &update.assignments {
                    if !matches!(expr, Expr::Literal(_) | Expr::Param(_)) {
                        reads_target = true;
                    }
                    expr.tables_read(&mut reads);
                }
                if let Some(filter) = &update.filter {
                    filter.tables_read(&mut reads);
                }
                if let Some(items) = &update.returning {
                    select_items_tables_read(items, &mut reads);
                }
                if reads_target {
                    out.push((target, TableAccess::Read));
                }
            }
            Plan::Delete(delete) => {
                let target = delete.table.as_str();
                out.push((target, TableAccess::Delete));
                if delete.filter.is_some() || delete.returning.is_some() {
                    out.push((target, TableAccess::Read));
                }
                if let Some(filter) = &delete.filter {
                    filter.tables_read(&mut reads);
                }
                if let Some(items) = &delete.returning {
                    select_items_tables_read(items, &mut reads);
                }
            }
            Plan::Select(select) => select.tables_read(&mut reads),
            Plan::SetOperation(plan) => plan.tables_read(&mut reads),
            // No `FROM`, but a scalar subquery in the projection still reads
            // whatever it names: `SELECT (SELECT secret FROM vault)`.
            Plan::Scalar(scalar) => {
                for item in &scalar.items {
                    item.expr.tables_read(&mut reads);
                }
            }
            // `EXPLAIN` never runs the statement inside it, but it does
            // describe it — which index answers which predicate is a fact
            // about the data. It carries the wrapped statement's requirements
            // unchanged rather than being free to run.
            Plan::Explain(inner) => return inner.table_access(),
            // These write nothing and name nothing. Transaction control is a
            // session-level act, not a table-level one — `ROLLBACK TO
            // SAVEPOINT` replays writes this same handle already passed its
            // own check for, not a new act needing one of its own.
            Plan::Begin
            | Plan::Commit
            | Plan::Rollback
            | Plan::Savepoint(_)
            | Plan::ReleaseSavepoint(_)
            | Plan::RollbackToSavepoint(_) => {}
        }

        out.extend(reads.into_iter().map(|table| (table, TableAccess::Read)));
        out
    }

    /// This statement's output columns, in projection order — empty for a
    /// statement that produces no rows (a `CREATE TABLE`, an `INSERT` with no
    /// `RETURNING`, `BEGIN`, and so on).
    ///
    /// This is prepare-time metadata, discoverable without running the
    /// statement — the same job `sqlite3_column_name` and
    /// `sqlite3_column_decltype` do together. `ty` is `None` wherever the
    /// plan does not statically know a column's type: a computed expression,
    /// a retrieval score, or any column of a `SELECT` with no `FROM`. SQLite
    /// draws the same line — `sqlite3_column_decltype` answers `NULL` for an
    /// expression too, only ever naming a *declared* column type.
    ///
    /// `schema` is the table list [`crate::statement::Statement`] already
    /// resolved from the catalog to stamp itself with (`Statement::new`'s
    /// `schema` field) — passed in rather than looked up again, since an
    /// `INSERT`/`UPDATE`/`DELETE`'s `RETURNING` items are resolved against
    /// exactly the first table that list names ([`Plan::tables`] always puts
    /// the write's own target there first, even when the statement also
    /// reads others).
    pub fn output_columns(&self, schema: &[Table]) -> Vec<ColumnInfo> {
        match self {
            Plan::Select(select) => select.output_columns(),
            Plan::SetOperation(plan) => plan.output_columns(),
            Plan::Scalar(scalar) => scalar
                .items
                .iter()
                .map(|item| ColumnInfo {
                    name: item.label.clone(),
                    ty: None,
                })
                .collect(),
            Plan::Insert(insert) => match (&insert.returning, schema.first()) {
                (Some(items), Some(table)) => select_item_columns(items, &[table]),
                _ => Vec::new(),
            },
            Plan::Update(update) => match (&update.returning, schema.first()) {
                (Some(items), Some(table)) => select_item_columns(items, &[table]),
                _ => Vec::new(),
            },
            Plan::Delete(delete) => match (&delete.returning, schema.first()) {
                (Some(items), Some(table)) => select_item_columns(items, &[table]),
                _ => Vec::new(),
            },
            // Fixed, whatever the inner statement is: `EXPLAIN` projects its
            // own three columns, never the wrapped statement's.
            Plan::Explain(_) => crate::explain::columns(),
            Plan::CreateTable(_)
            | Plan::DropTable(_)
            | Plan::AlterTable(_)
            | Plan::CreateIndex(_)
            | Plan::CreateUniqueIndex(_)
            | Plan::DropIndex(_)
            // SQLite's `REINDEX` returns no rows, and neither does this one.
            // What was built is reported through `Engine::reindex`'s return
            // value, which is where a caller that wants the detail asks.
            | Plan::Reindex(_)
            | Plan::Analyze(_)
            | Plan::Begin
            | Plan::Commit
            | Plan::Rollback
            | Plan::Savepoint(_)
            | Plan::ReleaseSavepoint(_)
            | Plan::RollbackToSavepoint(_) => Vec::new(),
        }
    }
}

/// What a statement does to one name it touches.
///
/// See [`Plan::table_access`], which is the only thing that produces these and
/// which explains why they are derived from the plan rather than from the
/// statement's text.
///
/// The engine itself enforces none of this — it has no notion of a user. This
/// is a *description*, for a layer that does: the MySQL-wire server maps each
/// variant onto the MySQL privilege of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAccess {
    /// Rows are read from the named table.
    Read,
    /// Rows are added to it.
    Insert,
    /// Stored rows are rewritten in it.
    Update,
    /// Stored rows are removed from it.
    Delete,
    /// It is created, or an index over it is.
    Create,
    /// It is dropped.
    Drop,
    /// Its definition is changed.
    Alter,
    /// The named **index** is dropped. The name is an index name, not a table
    /// name: `DROP INDEX` names no table, so a caller that wants to check a
    /// per-table grant has to resolve the index through the catalog first.
    DropIndex,
}

/// Record every stored table a `RETURNING` list reads.
///
/// The same walk [`SelectPlan::tables_read`] does over its own projection, and
/// for the same reason: a projected item may be a scalar subquery over a table
/// nothing else in the statement names.
fn select_items_tables_read<'a>(items: &'a [SelectItem], out: &mut Vec<&'a str>) {
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            expr.tables_read(out);
        }
    }
}

/// One column of a statement's projected output.
///
/// See [`Plan::output_columns`].
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    /// The header this column reports — the same label a result set carries.
    pub name: String,
    /// This column's declared type, where the plan projects a stored column
    /// directly; `None` otherwise.
    pub ty: Option<DataType>,
}

/// The declared type of the joined row's column at `index`, given the tables
/// contributing to it in order — `None` when `index` falls outside all of
/// them, which does not happen for a plan that resolved cleanly against
/// `tables`.
fn column_type_at(tables: &[&Table], index: usize) -> Option<DataType> {
    let mut base = 0;
    for table in tables {
        let width = table.columns.len();
        if index < base + width {
            return Some(table.columns[index - base].ty);
        }
        base += width;
    }
    None
}

/// [`SelectItem`]s projected against `tables`, turned into the
/// name-and-type pairs [`Plan::output_columns`] promises. Shared by
/// [`SelectPlan::output_columns`] (a possibly-joined `FROM`) and
/// `RETURNING` (always a single table).
fn select_item_columns(items: &[SelectItem], tables: &[&Table]) -> Vec<ColumnInfo> {
    items
        .iter()
        .map(|item| match item {
            SelectItem::Column { index, label } => ColumnInfo {
                name: label.clone(),
                ty: column_type_at(tables, *index),
            },
            SelectItem::Expr { label, .. } | SelectItem::Score { label } => ColumnInfo {
                name: label.clone(),
                ty: None,
            },
        })
        .collect()
}

/// A planned `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTablePlan {
    /// The table to create.
    pub table: Table,
    /// The constraints its declaration carried.
    pub constraints: TableConstraints,
    /// `IF NOT EXISTS`: an existing table of this name is not an error, and
    /// the statement does nothing.
    pub if_not_exists: bool,
    /// `CREATE TABLE ... AS SELECT`: the query that populates the new table,
    /// once it is created. `None` for an ordinary `CREATE TABLE`, whose table
    /// starts empty.
    pub as_select: Option<Box<SubqueryBody>>,
}

/// A planned `DROP TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTablePlan {
    /// Table name as written; matched case-insensitively.
    pub name: String,
    /// `IF EXISTS`: a missing table is not an error.
    pub if_exists: bool,
}

/// A planned `ALTER TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTablePlan {
    /// The table to alter, as the catalog spells it.
    pub table: String,
    /// What to do to it.
    pub action: AlterAction,
}

/// The `ALTER TABLE` operations SQLite has, and only those.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    /// `ADD COLUMN`: the new column is appended, and every stored row is
    /// rewritten to carry its default.
    AddColumn(Column),
    /// `RENAME TO`: the new table name.
    RenameTable(String),
    /// `RENAME COLUMN`.
    RenameColumn {
        /// The column as it is called now.
        from: String,
        /// What to call it.
        to: String,
    },
    /// `DROP COLUMN`: the column to remove, by name.
    DropColumn(String),
}

/// A planned `CREATE INDEX`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexPlan {
    /// Index name as written.
    pub name: String,
    /// Lowercased target table name.
    pub table: String,
    /// Target column ordinals, in written order, resolved against the table at
    /// plan time. Never empty; a [`IndexKind::Vector`] plan always has
    /// exactly one, but [`IndexKind::BTree`] and [`IndexKind::FullText`] may
    /// both have more.
    pub columns: Vec<usize>,
    /// The index kind, inferred from the column's type unless `USING` said so.
    pub kind: IndexKind,
    /// The distance an [`IndexKind::Vector`] index's graph will be built and
    /// searched under, from the operator class the column list wrote
    /// (`vector_l2_ops`) or the default when it wrote none. Always
    /// [`crate::hnsw::VectorMetric::Cosine`] for every other kind, which the
    /// front end refuses to let a statement contradict.
    pub metric: crate::hnsw::VectorMetric,
    /// `CREATE UNIQUE INDEX`. Only a B-tree index can carry it.
    pub unique: bool,
    /// The collating sequence each indexed column is keyed under, in written
    /// order: the column's own declared collation unless the index wrote
    /// `COLLATE` over it, which SQLite allows and which is the only way to have
    /// a `NOCASE` index on a `BINARY` column.
    pub collations: Vec<crate::collation::Collation>,
}

/// A planned `CREATE UNIQUE INDEX` that gets no index.
///
/// It becomes a named `UNIQUE` constraint in the catalog and nothing else.
/// That is now the *narrow* case: a unique index over orderable columns is a
/// real B-tree index (a [`CreateIndexPlan`] with `unique` set), which both
/// enforces the constraint by a probe and answers queries. Only a constraint
/// naming a column no ordered index can cover — a `VECTOR` — is left with the
/// O(rows) scan per write that this used to mean for everything.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateUniqueIndexPlan {
    /// Index name as written; `DROP INDEX` matches it case-insensitively.
    pub name: String,
    /// Target table, as the catalog spells it.
    pub table: String,
    /// The columns it covers, in written order.
    pub columns: Vec<String>,
}

/// A planned `DROP INDEX`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexPlan {
    /// Index name as written; matched case-insensitively.
    pub name: String,
}

/// A planned `REINDEX`.
///
/// The table list is resolved **at plan time**, not at run time, and that is
/// what makes the statement attributable: [`Plan::table_access`] has to be
/// able to name every table an authorisation layer would gate this on, and a
/// bare `REINDEX` names none of them in its own text. A plan that carried
/// `None` for "all of them" would answer "touches nothing" there, which is
/// what a caller reads as "anyone may run it".
///
/// The cost of resolving early is that a *prepared* bare `REINDEX` reused
/// after a `CREATE TABLE` rebuilds the tables it was planned against and not
/// the new one. `Engine::execute` re-plans every time, so only an explicitly
/// prepared-and-kept statement can see it, and the answer is to prepare it
/// again — the same answer every other plan that holds resolved names gives.
#[derive(Debug, Clone, PartialEq)]
pub struct ReindexPlan {
    /// The tables whose retrieval indexes to build, lowercased, in catalog
    /// order. Empty when the database has no tables at all, which makes the
    /// statement a no-op rather than an error.
    pub tables: Vec<String>,
    /// The one index to build, when the statement named an index rather than a
    /// table; `None` means every index of every table in `tables`.
    ///
    /// SQLite's `REINDEX <index-name>` rebuilds that index and not its
    /// siblings, so neither does this. Widening it to the whole table would be
    /// the accept-and-do-something-else shape `AGENTS.md` calls out: the
    /// statement would report success having done more than it was asked.
    pub index: Option<String>,
}

/// A planned `ANALYZE`.
///
/// The table list is resolved at plan time, including for bare `ANALYZE`.
/// Keeping the names in the plan gives an authorisation layer an exact list of
/// tables whose rows will be read and whose derived statistics will be
/// replaced. `Engine::execute` reparses each call, while a prepared statement
/// should be prepared again after a schema change for the same reason as a
/// prepared `REINDEX`.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzePlan {
    /// The tables to scan, lowercased and in catalog order.
    pub tables: Vec<String>,
}

/// A planned `UPDATE`.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdatePlan {
    /// Target table.
    pub table: String,
    /// Assignments, in `SET` order: a column ordinal and the expression to
    /// evaluate against the row being replaced.
    pub assignments: Vec<(usize, Expr)>,
    /// `WHERE` filter, evaluated against the *old* row.
    pub filter: Option<Expr>,
    /// `RETURNING`, projected over each row *after* it was changed — which is
    /// what SQLite returns.
    pub returning: Option<Vec<SelectItem>>,
}

/// A planned `DELETE`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeletePlan {
    /// Target table.
    pub table: String,
    /// `WHERE` filter; deleting every row when absent.
    pub filter: Option<Expr>,
    /// `RETURNING`, projected over each row before it was removed — the only
    /// version of it there is.
    pub returning: Option<Vec<SelectItem>>,
}

/// A `SELECT` with no `FROM` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarPlan {
    /// Output expressions, in order.
    pub items: Vec<ScalarItem>,
}

/// One output column of a [`ScalarPlan`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarItem {
    /// The expression to evaluate.
    pub expr: Expr,
    /// Header to report.
    pub label: String,
}

/// A scalar expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A literal value.
    Literal(Value),
    /// A `?` placeholder, referenced by its position in the statement text.
    ///
    /// Resolved against the parameter slice at execution, never at plan time —
    /// that is what lets one plan serve many bindings.
    Param(usize),
    /// A stored column, referenced by ordinal.
    Column(usize),
    /// A correlated reference out of a subquery into the query that encloses
    /// it, by slot in that subquery's [`Subquery::captures`].
    ///
    /// The ordinal is deliberately *not* into the enclosing row: a subquery
    /// three levels down would then need to know how wide every level above it
    /// is. A capture list per level keeps each subquery's plan a closed thing —
    /// it reads its own row and a flat list of values handed to it.
    Outer(usize),
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Expr>,
    },
    /// A binary operation.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
        /// The collating sequence a *comparison* uses, resolved at plan time by
        /// SQLite's rules (see [`crate::collation`]). Meaningless — and always
        /// [`Collation::Binary`] — for arithmetic, `AND`/`OR` and `||`, none of
        /// which compare anything.
        ///
        /// It is resolved once, here, rather than per row: the rules need the
        /// *expression*, and the evaluator only ever sees values.
        collation: Collation,
        /// The affinity conversion a *comparison* applies before comparing,
        /// resolved at plan time from both operands (`sql.rs`'s
        /// `compare_affinity`, SQLite's `sqlite3CompareAffinity`).
        /// Meaningless — and always [`CompareAffinity::None`] — for
        /// arithmetic, `AND`/`OR` and `||`: the same operators `collation`
        /// above excludes, and for the same reason.
        affinity: CompareAffinity,
    },
    /// `expr COLLATE name` — the postfix operator that assigns an explicit
    /// collating sequence.
    ///
    /// It is a value-preserving wrapper: evaluating it evaluates the operand
    /// and nothing else. What it changes is *plan-time* resolution, where it is
    /// the highest-precedence source of a collation for any comparison,
    /// ordering or grouping that encloses it. It stays in the tree after
    /// resolution so that a plan round-trips to something readable and so that
    /// `ORDER BY x COLLATE NOCASE` still says what it means.
    Collate {
        /// The operand.
        expr: Box<Expr>,
        /// The collation it names.
        collation: Collation,
    },
    /// A reference to an aggregate function in [`SelectPlan::aggregates`],
    /// evaluated over the current group. Only meaningful in an aggregate
    /// query; the executor supplies one value per aggregate.
    Agg(usize),
    /// A reference to a window function in [`SelectPlan::windows`], evaluated
    /// over its own partition and frame. The planner only ever produces this
    /// inside a query's own projection list or its own `ORDER BY` — every
    /// other position (`WHERE`, `GROUP BY`, `HAVING`, inside another window
    /// function's own clauses, inside an aggregate's argument) is refused at
    /// resolution time, confirmed against sqlite3 3.54's "misuse of window
    /// function" error for every one of those.
    Window(usize),
    /// `expr [NOT] LIKE pattern [ESCAPE escape]`.
    ///
    /// SQLite's `LIKE` is case-insensitive, but only over ASCII `A`–`Z`; a
    /// non-ASCII letter compares exactly. `%` matches any run of characters
    /// and `_` exactly one.
    Like {
        /// `NOT LIKE` when true.
        negated: bool,
        /// The value being matched.
        expr: Box<Expr>,
        /// The pattern.
        pattern: Box<Expr>,
        /// The `ESCAPE` character, which must evaluate to a single-character
        /// string. Absent means nothing in the pattern is escaped.
        escape: Option<Box<Expr>>,
    },
    /// `expr [NOT] IN (a, b, ...)` over a literal list.
    ///
    /// `IN (SELECT ...)` is a different construct and is not planned here.
    InList {
        /// `NOT IN` when true.
        negated: bool,
        /// The value being looked for.
        expr: Box<Expr>,
        /// The candidate list, which may be empty.
        list: Vec<Expr>,
        /// The collating sequence every `=` this expands to uses. SQLite treats
        /// `x IN (y, z)` as `x = y OR x = z`, so its left operand is `x` every
        /// time and one collation answers for the whole list.
        collation: Collation,
        /// The affinity conversion every `=` this expands to applies.
        ///
        /// **The left operand alone decides, same as `collation` above — and
        /// this one has no combining rule to fall back on at all.** SQLite's
        /// `sqlite3ExprCodeIN` only ever asks `sqlite3ExprAffinity(pLeft)`;
        /// unlike a written `=`, the affinity a *candidate* itself carries is
        /// never consulted, confirmed against a real sqlite3 3.54 binary:
        /// `'1' = id` matches an `INTEGER` column but `'1' IN (id)` does not.
        affinity: CompareAffinity,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// `NOT BETWEEN` when true.
        negated: bool,
        /// The value being bounded.
        expr: Box<Expr>,
        /// Inclusive lower bound.
        low: Box<Expr>,
        /// Inclusive upper bound.
        high: Box<Expr>,
        /// The collating sequence `x >= low` uses.
        ///
        /// Two fields and not one, because SQLite really does resolve the two
        /// halves separately: `exprCodeBetween` builds `x >= y` and `x <= z` as
        /// two comparisons and asks `sqlite3BinaryCompareCollSeq` about each,
        /// so `x BETWEEN y AND z COLLATE NOCASE` compares its lower bound under
        /// `BINARY` and its upper under `NOCASE`.
        low_collation: Collation,
        /// The collating sequence `x <= high` uses.
        high_collation: Collation,
        /// The affinity conversion `x >= low` applies, resolved from `expr`
        /// and `low` together — unlike [`Self::InList`]'s, this one *does*
        /// combine both sides, confirmed against sqlite3: a literal probe
        /// against `INTEGER` bounds (`'2' BETWEEN lo AND hi`) still converts.
        low_affinity: CompareAffinity,
        /// The affinity conversion `x <= high` applies, resolved from `expr`
        /// and `high` together.
        high_affinity: CompareAffinity,
    },
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
    ///
    /// With an `operand` this is the simple form, where each `WHEN` is
    /// compared to the operand with `=`; without one it is the searched form,
    /// where each `WHEN` is a predicate. No branch matching and no `ELSE` is
    /// `NULL`.
    Case {
        /// The simple form's operand, evaluated once.
        operand: Option<Box<Expr>>,
        /// `WHEN` condition and its `THEN` result, in written order.
        branches: Vec<(Expr, Expr)>,
        /// The `ELSE` result.
        else_result: Option<Box<Expr>>,
        /// The collating sequence each branch's `=` uses, aligned with
        /// `branches`.
        ///
        /// Per branch and not per expression, because that is where SQLite
        /// resolves it: `sqlite3ExprCodeTarget` codes one `OP_Eq` per `WHEN`
        /// and asks `sqlite3BinaryCompareCollSeq` about the operand and *that*
        /// `WHEN`, so `CASE x WHEN y COLLATE NOCASE THEN 1 WHEN z THEN 2 END`
        /// really does compare its two branches differently. Empty for the
        /// searched form, which compares nothing of its own.
        branch_collations: Vec<Collation>,
        /// The affinity conversion each branch's `=` applies, aligned with
        /// `branches` exactly as `branch_collations` is, and resolved the
        /// same combining way `Binary`'s is — the simple form's `=` is a
        /// written comparison with two real operands, not a probe against a
        /// list the way `InList`'s is.
        branch_affinities: Vec<CompareAffinity>,
    },
    /// `CAST(expr AS type)`, following SQLite's conversion rules.
    Cast {
        /// The value being converted.
        expr: Box<Expr>,
        /// The affinity to convert to.
        to: CastType,
    },
    /// A scalar function call, resolved to a known function and its arguments.
    ///
    /// Arity is checked at plan time, so the evaluator never has to decide
    /// what `substr()` with one argument means. Variadic functions
    /// (`coalesce`, `min`, `max`, the date/time family) keep their whole
    /// argument list here.
    Func {
        /// Which function.
        func: ScalarFunc,
        /// Arguments in written order.
        args: Vec<Expr>,
        /// The collating sequence the three functions that compare their
        /// arguments use: `nullif`, and the scalar `min`/`max`.
        ///
        /// SQLite flags those three `SQLITE_FUNC_NEEDCOLL` and codes an
        /// `OP_CollSeq` before the call, taking the collation of **the first
        /// argument that has one** — which is not the same rule a comparison
        /// operator uses, and is why this is resolved separately rather than
        /// reusing it. [`Collation::Binary`] for every other function, which
        /// never reads it.
        collation: Collation,
    },
    /// A subquery in an expression position: `(SELECT ...)`,
    /// `EXISTS (SELECT ...)` or `x [NOT] IN (SELECT ...)`.
    Subquery {
        /// What the rows the subquery returns are turned into.
        op: SubqueryOp,
        /// The subquery itself.
        query: Box<Subquery>,
    },
}

/// A subquery, and what the enclosing query has to hand it.
///
/// # Why the captures are here and not in the body
///
/// A *correlated* subquery reads columns of the query that encloses it. Rather
/// than let the inner plan reach into an outer row — which would tie its
/// ordinals to a row shape it cannot see — the enclosing query evaluates
/// [`Subquery::captures`] against its own row and hands the resulting values
/// down as a flat list. [`Expr::Outer`] inside [`Subquery::body`] indexes that
/// list.
///
/// An empty capture list therefore means exactly "uncorrelated", which is what
/// lets the executor evaluate the subquery once per statement and reuse the
/// answer for every outer row.
#[derive(Debug, Clone, PartialEq)]
pub struct Subquery {
    /// Identity within the statement it was planned from, assigned in
    /// resolution order.
    ///
    /// This is the memoisation key for an uncorrelated subquery: it is a
    /// constant for the whole statement, so it is evaluated at most once
    /// however many rows the outer query walks.
    pub id: usize,
    /// The inner query.
    pub body: Box<SubqueryBody>,
    /// Expressions evaluated in the *enclosing* scope, once per evaluation of
    /// this subquery. Empty when the subquery is uncorrelated.
    pub captures: Vec<Expr>,
}

/// The query inside a subquery — the same two shapes a top-level query has.
#[derive(Debug, Clone, PartialEq)]
pub enum SubqueryBody {
    /// `SELECT ... FROM ...`.
    Select(Box<SelectPlan>),
    /// `SELECT <expr>` with no `FROM`, which is one row of constants.
    Scalar(ScalarPlan),
    /// `UNION [ALL]` / `INTERSECT` / `EXCEPT` of two query bodies.
    ///
    /// A chain of more than two arms is not represented as a flat list: it
    /// folds left-associatively into nested [`SetOperationPlan`]s, `left`
    /// holding everything to its left in source order — see
    /// `sql.rs::plan_compound`, which verifies against a real sqlite3 binary
    /// that this is SQLite's own grouping (every compound operator shares one
    /// precedence, unlike the SQL-standard rule sqlparser's generic grammar
    /// otherwise applies).
    SetOp(Box<SetOperationPlan>),
}

impl SubqueryBody {
    /// How many columns the subquery projects.
    ///
    /// A scalar subquery and an `IN (SELECT ...)` both require exactly one, and
    /// SQLite reports the mismatch at prepare time rather than at run time —
    /// so this is checked in the planner.
    pub fn width(&self) -> usize {
        match self {
            SubqueryBody::Select(plan) => plan.items.len(),
            SubqueryBody::Scalar(plan) => plan.items.len(),
            // A compound's width is its left arm's — checked equal to the
            // right arm's at plan time, so either would do, but the left is
            // the one whose *labels* the compound also reports.
            SubqueryBody::SetOp(plan) => plan.left.width(),
        }
    }

    /// The headers this body projects, which become a derived table's column
    /// names.
    ///
    /// For a compound, SQLite takes these from the left arm alone — verified
    /// against sqlite3: `SELECT a AS x FROM t1 UNION SELECT a AS y FROM t2`
    /// reports `x`, not `y`, however many arms are chained.
    pub fn labels(&self) -> Vec<&str> {
        match self {
            SubqueryBody::Select(plan) => plan.items.iter().map(SelectItem::label).collect(),
            SubqueryBody::Scalar(plan) => plan.items.iter().map(|i| i.label.as_str()).collect(),
            SubqueryBody::SetOp(plan) => plan.left.labels(),
        }
    }

    /// Record every stored table this body reads, transitively.
    pub fn tables_read<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            SubqueryBody::Select(plan) => plan.tables_read(out),
            SubqueryBody::Scalar(plan) => {
                for item in &plan.items {
                    item.expr.tables_read(out);
                }
            }
            SubqueryBody::SetOp(plan) => plan.tables_read(out),
        }
    }
}

/// `UNION [ALL]` / `INTERSECT` / `EXCEPT` over two query bodies.
///
/// SQLite's own semantics, not the SQL standard's, verified against a real
/// sqlite3 binary — see `sql.rs::plan_compound` for what was checked and
/// where each empirical finding is used.
#[derive(Debug, Clone, PartialEq)]
pub struct SetOperationPlan {
    /// Which operator, and whether it deduplicates.
    pub op: SetOp,
    /// Everything to the left, in source order — a plain arm, or (for a
    /// chain of more than two) a nested compound.
    pub left: Box<SubqueryBody>,
    /// The next arm to the right.
    pub right: Box<SubqueryBody>,
    /// The collating sequence each output column compares under, for both
    /// deduplication and (unless a term overrides it) `ORDER BY` — always
    /// the *left* arm's, recursively down to the leftmost `SELECT`, and an
    /// explicit `COLLATE` anywhere in the right arm has no effect at all.
    /// Aligned with the output columns.
    pub collations: Vec<Collation>,
    /// `ORDER BY`, binding to the whole compound rather than to the last
    /// arm — resolved against the compound's own output (a label or a
    /// 1-based ordinal only; SQLite refuses anything else here too).
    pub order: Vec<Order>,
    /// `LIMIT`, over the whole compound's result.
    pub limit: Option<Expr>,
    /// `OFFSET`, on the same terms as [`SetOperationPlan::limit`].
    pub offset: Option<Expr>,
}

impl SetOperationPlan {
    /// This compound's output columns: labels from the left arm, recursively;
    /// no type, since nothing here projects a stored column directly (the
    /// same reason a derived table's synthetic columns carry none).
    pub fn output_columns(&self) -> Vec<ColumnInfo> {
        self.left
            .labels()
            .into_iter()
            .map(|name| ColumnInfo {
                name: name.to_string(),
                ty: None,
            })
            .collect()
    }

    /// Record every stored table this compound reads, transitively, through
    /// both arms.
    pub fn tables_read<'a>(&'a self, out: &mut Vec<&'a str>) {
        self.left.tables_read(out);
        self.right.tables_read(out);
        for term in &self.order {
            if let OrderKey::Expr(expr) = &term.key {
                expr.tables_read(out);
            }
        }
        if let Some(limit) = &self.limit {
            limit.tables_read(out);
        }
        if let Some(offset) = &self.offset {
            offset.tables_read(out);
        }
    }
}

/// The `UNION`/`INTERSECT`/`EXCEPT` family, and whether it deduplicates.
///
/// SQLite has no `INTERSECT ALL`/`EXCEPT ALL` and no `MINUS` — confirmed
/// against sqlite3, which refuses all three as syntax errors — so `ALL` only
/// ever varies `UNION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// `UNION`: concatenate, then fold rows that compare equal into one,
    /// keeping the *last*-occurring row of each group — confirmed against
    /// sqlite3: on a case-only collision under a `NOCASE` column, the right
    /// arm's bytes survive, not the left's.
    Union,
    /// `UNION ALL`: concatenate, keep every row.
    UnionAll,
    /// `INTERSECT`: rows of the left arm (deduplicated, first occurrence
    /// wins — confirmed against sqlite3) that also appear in the right.
    Intersect,
    /// `EXCEPT`: rows of the left arm (deduplicated, first occurrence wins)
    /// that do *not* appear in the right.
    Except,
}

impl fmt::Display for SetOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SetOp::Union => "UNION",
            SetOp::UnionAll => "UNION ALL",
            SetOp::Intersect => "INTERSECT",
            SetOp::Except => "EXCEPT",
        })
    }
}

/// What the enclosing expression does with a subquery's rows.
#[derive(Debug, Clone, PartialEq)]
pub enum SubqueryOp {
    /// `(SELECT ...)` used as a value. The first row's only column, or `NULL`
    /// when there are no rows — SQLite's rule, including that later rows are
    /// ignored rather than being an error.
    Scalar,
    /// `[NOT] EXISTS (SELECT ...)`: whether the subquery returned any row at
    /// all. Never `NULL`.
    Exists {
        /// `NOT EXISTS` when true.
        negated: bool,
    },
    /// `probe [NOT] IN (SELECT ...)`, with the same three-valued logic
    /// [`Expr::InList`] has.
    In {
        /// `NOT IN` when true.
        negated: bool,
        /// The value being looked for, evaluated in the enclosing scope.
        probe: Box<Expr>,
        /// The collating sequence the comparison against each returned row
        /// uses, resolved from the probe and the subquery's output column
        /// exactly as `probe = column` would be.
        collation: Collation,
        /// The affinity conversion the comparison against each returned row
        /// applies, resolved from the probe and the subquery's output column
        /// together — **unlike [`Expr::InList`]'s**, confirmed against
        /// sqlite3: `'1' IN (SELECT id FROM ids)` matches an `INTEGER` `id`
        /// where the literal-list form does not, because a `SELECT`'s
        /// ephemeral index is built with the combined affinity the way a
        /// written `=` is, not with the probe's alone.
        affinity: CompareAffinity,
    },
}

impl Expr {
    /// Record every stored column this expression reads into `mask`.
    ///
    /// This is what makes projection pushdown safe: a column the executor
    /// leaves undecoded reads as `NULL`, so *anything* that can observe a
    /// column has to be walked into the mask before a row is decoded. The match
    /// below is deliberately exhaustive — a new [`Expr`] variant is a compile
    /// error here rather than a query that silently returns `NULL`.
    pub fn columns_read(&self, mask: &mut crate::row::ColumnMask) {
        match self {
            // A parameter, a literal and an aggregate or window reference read
            // no stored column directly: an aggregate's *argument* and a
            // window function's *arguments*/`PARTITION BY`/`ORDER BY`/frame
            // are walked where the `SelectPlan` holds them
            // (`engine::needed_columns`), not through the reference to it.
            // `Outer` reads the *enclosing* query's row, never this one; the
            // capture expression that feeds it is walked where the enclosing
            // plan holds it, which is the `Expr::Subquery` arm below.
            Expr::Literal(_) | Expr::Param(_) | Expr::Agg(_) | Expr::Window(_) | Expr::Outer(_) => {
            }
            Expr::Column(index) => mask.add(*index),
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::Collate { expr, .. } => {
                expr.columns_read(mask)
            }
            Expr::Binary { left, right, .. } => {
                left.columns_read(mask);
                right.columns_read(mask);
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                expr.columns_read(mask);
                pattern.columns_read(mask);
                if let Some(escape) = escape {
                    escape.columns_read(mask);
                }
            }
            Expr::InList { expr, list, .. } => {
                expr.columns_read(mask);
                for item in list {
                    item.columns_read(mask);
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                expr.columns_read(mask);
                low.columns_read(mask);
                high.columns_read(mask);
            }
            Expr::Case {
                operand,
                branches,
                else_result,
                ..
            } => {
                if let Some(operand) = operand {
                    operand.columns_read(mask);
                }
                for (when, then) in branches {
                    when.columns_read(mask);
                    then.columns_read(mask);
                }
                if let Some(else_result) = else_result {
                    else_result.columns_read(mask);
                }
            }
            Expr::Func { args, .. } => {
                for arg in args {
                    arg.columns_read(mask);
                }
            }
            // The subquery's *body* reads its own tables' rows, not this one's,
            // so it contributes nothing here. What does is everything the
            // enclosing row has to supply: the `IN` probe and every correlated
            // capture. Missing either would decode the column as `NULL` and
            // answer a subtly different query.
            Expr::Subquery { op, query } => {
                if let SubqueryOp::In { probe, .. } = op {
                    probe.columns_read(mask);
                }
                for capture in &query.captures {
                    capture.columns_read(mask);
                }
            }
        }
    }

    /// Record every stored table reachable from this expression's subqueries.
    ///
    /// [`Plan::tables`] is what a prepared statement re-checks before it runs,
    /// and a subquery's plan holds column ordinals exactly as the outer one
    /// does — so a table only a subquery reads has to be stamped too, or
    /// `ALTER TABLE` on it would silently move the ordinals the subquery
    /// projects. Exhaustive for the same reason [`Expr::columns_read`] is.
    pub fn tables_read<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expr::Literal(_)
            | Expr::Param(_)
            | Expr::Agg(_)
            | Expr::Window(_)
            | Expr::Outer(_)
            | Expr::Column(_) => {}
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::Collate { expr, .. } => {
                expr.tables_read(out)
            }
            Expr::Binary { left, right, .. } => {
                left.tables_read(out);
                right.tables_read(out);
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                expr.tables_read(out);
                pattern.tables_read(out);
                if let Some(escape) = escape {
                    escape.tables_read(out);
                }
            }
            Expr::InList { expr, list, .. } => {
                expr.tables_read(out);
                for item in list {
                    item.tables_read(out);
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                expr.tables_read(out);
                low.tables_read(out);
                high.tables_read(out);
            }
            Expr::Case {
                operand,
                branches,
                else_result,
                ..
            } => {
                if let Some(operand) = operand {
                    operand.tables_read(out);
                }
                for (when, then) in branches {
                    when.tables_read(out);
                    then.tables_read(out);
                }
                if let Some(else_result) = else_result {
                    else_result.tables_read(out);
                }
            }
            Expr::Func { args, .. } => {
                for arg in args {
                    arg.tables_read(out);
                }
            }
            Expr::Subquery { op, query } => {
                if let SubqueryOp::In { probe, .. } = op {
                    probe.tables_read(out);
                }
                for capture in &query.captures {
                    capture.tables_read(out);
                }
                query.body.tables_read(out);
            }
        }
    }
}

/// A scalar function, named exactly as SQLite names it.
///
/// Only functions this engine implements appear here: an unknown name is a
/// plan-time error rather than a value, because a function silently returning
/// `NULL` is the same class of bug as a clause that parses and is discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    /// `length(X)` — characters for text, bytes for a blob.
    Length,
    /// `upper(X)` — ASCII case folding, as SQLite does without ICU.
    Upper,
    /// `lower(X)`.
    Lower,
    /// `substr(X, Y[, Z])` (also spelled `substring`).
    Substr,
    /// `trim(X[, Y])`.
    Trim,
    /// `ltrim(X[, Y])`.
    LTrim,
    /// `rtrim(X[, Y])`.
    RTrim,
    /// `replace(X, Y, Z)`.
    Replace,
    /// `instr(X, Y)` — 1-based position, `0` when absent.
    Instr,
    /// `abs(X)`.
    Abs,
    /// `round(X[, Y])` — always a REAL.
    Round,
    /// `coalesce(X, Y, ...)` — the first non-`NULL` argument.
    Coalesce,
    /// `ifnull(X, Y)`.
    IfNull,
    /// `nullif(X, Y)`.
    NullIf,
    /// `min(X, Y, ...)` — the *scalar* form, two arguments or more.
    Min,
    /// `max(X, Y, ...)` — the scalar form.
    Max,
    /// `hex(X)`.
    Hex,
    /// `octet_length(X)` — byte count, always, unlike `length()` which
    /// counts characters for text. SQLite has had this since 3.43; it is
    /// what MySQL's `LENGTH()` means, which is the whole reason it exists
    /// here (AHL-465).
    OctetLength,
    /// `unhex(X)` — the inverse of `hex()`: a blob decoded from a
    /// hexadecimal string, or `NULL` when `X` is not one (odd length, or a
    /// character outside `0-9A-Fa-f`). SQLite's single-argument form only;
    /// the two-argument ignore-set form is not implemented.
    Unhex,
    /// `random()` — drawn from the injected [`crate::traits::Rng`], never
    /// from the host.
    Random,
    /// `date(...)`.
    Date,
    /// `time(...)`.
    Time,
    /// `datetime(...)`.
    DateTime,
    /// `strftime(format, ...)`.
    Strftime,
    /// `unixepoch(...)` — whole seconds since 1970.
    UnixEpoch,
    /// `CURRENT_TIMESTAMP` — `datetime('now')`, from the injected
    /// [`crate::traits::Clock`].
    CurrentTimestamp,
    /// `CURRENT_DATE` — `date('now')`.
    CurrentDate,
    /// `CURRENT_TIME` — `time('now')`.
    CurrentTime,
    /// `mysql_substr(s, pos[, len])` — the shim-target-only primitive behind
    /// MySQL's `SUBSTRING`: position `0` (or out of range) is the empty
    /// string, a non-positive `len` is the empty string, and any `NULL`
    /// argument is `NULL`. Not SQLite's `substr()`, whose corners differ —
    /// see `docs/server.md`'s Divergences section before AHL-465. Not
    /// advertised as part of the SQLite dialect; `crates/inlaysql-server`'s
    /// shim is the only intended caller (AHL-465).
    MysqlSubstr,
    /// `mysql_hex(X)` — the shim-target-only primitive behind MySQL's
    /// `HEX()`: `NULL` stays `NULL` (SQLite's `hex()` answers `''`), and a
    /// number is rendered as the hexadecimal of its *value*, not the bytes
    /// of its text (`mysql_hex(255)` is `'FF'`, not `'323535'`). Text and
    /// blob arguments behave exactly as `hex()` already does. Shim-target
    /// only, like [`ScalarFunc::MysqlSubstr`] (AHL-465).
    MysqlHex,
    /// `mysql_nullif(X, Y)` — the shim-target-only primitive behind MySQL's
    /// `NULLIF`: the comparison coerces a number and a string the way
    /// MySQL's `=` does (the string is read as a number) rather than
    /// SQLite's `nullif()`, which compares by storage class and never
    /// converts. Shim-target only (AHL-465).
    MysqlNullIf,
    /// `mysql_round(X[, Y])` — the shim-target-only primitive behind
    /// MySQL's `ROUND()` on a float argument: MySQL 8.4.11 rounds a halfway
    /// case to even (`ROUND(2.5e0)` is `2`), where the engine's own
    /// `round()` rounds away from zero (`3`) — measured, not assumed, and
    /// recorded in `docs/server.md`. Negative `Y` rounds to tens, hundreds,
    /// … (`round()` clamps `Y` to zero). Shim-target only (AHL-465).
    MysqlRound,
    /// `json(X)` — validate `X` as JSON and return its canonical (minified)
    /// text, or `NULL` for a `NULL` argument. This is the explicit spelling
    /// of "treat this text as JSON" that `json_extract`/`->` are recognised
    /// as implicitly when they appear as an argument to another JSON
    /// function — see [`Self::JsonSet`] (AHL-490).
    Json,
    /// `json_extract(X, P, ...)` — the node at each path `P`. One path
    /// returns the SQL value at that node (a JSON object/array's own text
    /// for a composite, the unwrapped SQL value for a scalar); two or more
    /// return a JSON array of them. `NULL` if `X` or any `P` is `NULL`; an
    /// error if `X` is not JSON or any `P` is not a valid path — checked
    /// against sqlite3, which errors rather than answering `NULL` there.
    JsonExtract,
    /// `json_valid(X)` — `1`/`0`, or `NULL` for a `NULL` argument. Never
    /// errors, unlike every other function here: this is the one meant to be
    /// called on text that might not be JSON.
    JsonValid,
    /// `json_type(X[, P])` — the lowercase kind of the node at `P` (`$` if
    /// omitted): `object`, `array`, `integer`, `real`, `text`, `true`,
    /// `false` or `null`. `NULL` for a `NULL` argument or a path with no
    /// match.
    JsonType,
    /// `json_quote(X)` — `X` as a JSON *string* literal: quoted and escaped
    /// text for a SQL number, and the JSON `null` literal for SQL `NULL` —
    /// this is the one place a SQL value's own type is not preserved, since
    /// the whole point is to produce a string. A `BLOB` argument is an
    /// error, "JSON cannot hold BLOB values" (checked against sqlite3).
    JsonQuote,
    /// `json_array(X, ...)` — a JSON array of the arguments, each converted
    /// the way [`Self::JsonSet`]'s value argument is (composing a nested
    /// `json_array()`/`json_object()`/... call rather than stringifying its
    /// result).
    JsonArray,
    /// `json_object(K, V, ...)` — a JSON object of the argument pairs. Every
    /// `K` must be `TEXT` — even a `NULL` key is refused, "json_object()
    /// labels must be TEXT" (checked against sqlite3) — and a duplicate key
    /// is kept, not merged, the same as a hand-written duplicate would be.
    JsonObject,
    /// `json_array_length(X[, P])` — the element count of the array at `P`
    /// (`$` if omitted); `0` if the node there is not an array (including a
    /// scalar or an object — checked against sqlite3, which does not error);
    /// `NULL` for a `NULL` argument or a path with no match.
    JsonArrayLength,
    /// `json_set(X, P, V, ...)` — write `V` at each path `P` in turn,
    /// creating a missing intermediate object/array as needed, and
    /// overwriting whatever was already at `P`. A `NULL` `P` skips that pair
    /// (leaves `X` there untouched, not an error and not `NULL`-propagating
    /// — checked against sqlite3); a `NULL` `V` writes a JSON `null`. `V` is
    /// spliced as raw JSON, not stringified, when it is written as a direct
    /// call to `json()`, `json_extract()`, `json_array()`, `json_object()`,
    /// `json_set()`, `json_insert()`, `json_replace()`, `json_remove()` or
    /// the `->` operator — SQLite's own "composing" rule (json1.html),
    /// checked directly rather than assumed. This engine detects that rule
    /// syntactically, from the argument's own expression shape, which is
    /// narrower than sqlite3's real subtype propagation: sqlite3 also
    /// recognises a `CASE` or scalar subquery that yields one of those
    /// calls, and this does not. That gap is intentional and documented
    /// (`eval.rs`'s `json_composed_value`) rather than silent — it is a
    /// shape no framework's query builder writes.
    JsonSet,
    /// `json_insert(X, P, V, ...)` — like [`Self::JsonSet`], but a pair
    /// whose path already names something is left untouched.
    JsonInsert,
    /// `json_replace(X, P, V, ...)` — like [`Self::JsonSet`], but a pair
    /// whose path names nothing is left untouched (and creates no
    /// intermediate object/array, since nothing is written at all).
    JsonReplace,
    /// `json_remove(X, P, ...)` — delete the node at each path `P` in turn.
    /// Unlike the `Set`/`Insert`/`Replace` family, a `NULL` `P` here
    /// propagates `NULL` for the *whole* result rather than skipping that
    /// one path — checked against sqlite3, and it is genuinely the other
    /// way round from `json_set`'s pairs, not a typo carried over from them.
    /// `P` equal to `$` removes the whole document, i.e. the result is
    /// `NULL` (there is no parent to delete the root out of).
    JsonRemove,
}

impl ScalarFunc {
    /// The name this function is written with, for error messages.
    pub fn name(&self) -> &'static str {
        match self {
            ScalarFunc::Length => "length",
            ScalarFunc::Upper => "upper",
            ScalarFunc::Lower => "lower",
            ScalarFunc::Substr => "substr",
            ScalarFunc::Trim => "trim",
            ScalarFunc::LTrim => "ltrim",
            ScalarFunc::RTrim => "rtrim",
            ScalarFunc::Replace => "replace",
            ScalarFunc::Instr => "instr",
            ScalarFunc::Abs => "abs",
            ScalarFunc::Round => "round",
            ScalarFunc::Coalesce => "coalesce",
            ScalarFunc::IfNull => "ifnull",
            ScalarFunc::NullIf => "nullif",
            ScalarFunc::Min => "min",
            ScalarFunc::Max => "max",
            ScalarFunc::Hex => "hex",
            ScalarFunc::OctetLength => "octet_length",
            ScalarFunc::Unhex => "unhex",
            ScalarFunc::Random => "random",
            ScalarFunc::Date => "date",
            ScalarFunc::Time => "time",
            ScalarFunc::DateTime => "datetime",
            ScalarFunc::Strftime => "strftime",
            ScalarFunc::UnixEpoch => "unixepoch",
            ScalarFunc::CurrentTimestamp => "current_timestamp",
            ScalarFunc::CurrentDate => "current_date",
            ScalarFunc::CurrentTime => "current_time",
            ScalarFunc::MysqlSubstr => "mysql_substr",
            ScalarFunc::MysqlHex => "mysql_hex",
            ScalarFunc::MysqlNullIf => "mysql_nullif",
            ScalarFunc::MysqlRound => "mysql_round",
            ScalarFunc::Json => "json",
            ScalarFunc::JsonExtract => "json_extract",
            ScalarFunc::JsonValid => "json_valid",
            ScalarFunc::JsonType => "json_type",
            ScalarFunc::JsonQuote => "json_quote",
            ScalarFunc::JsonArray => "json_array",
            ScalarFunc::JsonObject => "json_object",
            ScalarFunc::JsonArrayLength => "json_array_length",
            ScalarFunc::JsonSet => "json_set",
            ScalarFunc::JsonInsert => "json_insert",
            ScalarFunc::JsonReplace => "json_replace",
            ScalarFunc::JsonRemove => "json_remove",
        }
    }

    /// Whether this function's value can change between two calls with the
    /// same arguments — `random()` and everything that reads the clock.
    ///
    /// Nothing depends on this yet; it is here so that a later constant-folding
    /// pass cannot fold a call that must be evaluated per row.
    pub fn is_volatile(&self) -> bool {
        matches!(
            self,
            ScalarFunc::Random
                | ScalarFunc::Date
                | ScalarFunc::Time
                | ScalarFunc::DateTime
                | ScalarFunc::Strftime
                | ScalarFunc::UnixEpoch
                | ScalarFunc::CurrentTimestamp
                | ScalarFunc::CurrentDate
                | ScalarFunc::CurrentTime
        )
    }
}

/// The affinity a [`Expr::Cast`] converts to.
///
/// These are SQLite's five affinities, chosen from the written type name by
/// the same spelling rules SQLite uses. They are deliberately *not*
/// [`crate::value::DataType`]: that type is what a column stores and is part
/// of the catalog encoding, while this is only the shape of a conversion —
/// and it needs `Numeric`, which is not a storage class at all.
///
/// This doubles as the affinity a bare column reference carries into a
/// *comparison* (see [`CompareAffinity`]): a stored column's declared type
/// and a `CAST` target name the same five things, so `sql.rs`'s
/// `column_affinity` maps [`crate::value::DataType`] onto this rather than
/// inventing a second enum for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastType {
    /// `INTEGER` affinity: truncate reals, parse the numeric prefix of text.
    Integer,
    /// `REAL` affinity.
    Real,
    /// `TEXT` affinity.
    Text,
    /// `BLOB` affinity: no conversion beyond reinterpreting text as bytes.
    Blob,
    /// `NUMERIC` affinity: integer when the value is one, real otherwise.
    Numeric,
}

/// What SQLite's comparison-affinity rule converts before two operands are
/// compared — the *outcome* of combining the affinity each side of a `=`,
/// `<`, `IN`, `BETWEEN` or simple `CASE` branch carries (`sql.rs`'s
/// `combine_affinity`, from `sqlite3CompareAffinity`), resolved once at plan
/// time exactly as [`Collation`] is, because it needs the *expression* (is
/// this a column? a `CAST`?) and the evaluator only ever sees the value that
/// expression produced.
///
/// This is stage one of a two-stage rule (AHL-486); [`crate::eval::mem_cmp`]
/// and [`crate::eval::compare_cells`]'s storage-class ranking is stage two
/// and is unconditional — it runs whether or not this converted anything.
/// `eval.rs`'s `affinity_conversion` is what actually applies a resolved
/// value of this type to one comparison operand, immediately before
/// `compare_cells` ranks the (possibly converted) pair. Checked against a
/// real sqlite3 3.54 binary corner by corner: `BLOB` affinity behaves
/// exactly like `None` here because `affinity_conversion`'s two arms are
/// both no-ops on a value outside their own storage class, so resolving it
/// to `Numeric` whenever the *other* side is numeric (rather than reproducing
/// `sqlite3CompareAffinity`'s stricter "only when neither side already has a
/// non-numeric affinity" test) answers identically in every pair tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareAffinity {
    /// Neither side's affinity beats the other's absence: compare the two
    /// operands exactly as they are, which is `mem_cmp`'s class order alone.
    /// This is also what a comparison against a `VECTOR` value gets, since
    /// `VECTOR` is this engine's own addition and not one of SQLite's five
    /// affinities.
    None,
    /// One side has `INTEGER`, `REAL` or `NUMERIC` affinity: a well-formed
    /// numeric `TEXT` operand on the other side is converted to a number
    /// first (`eval.rs`'s `numeric_affinity_of_text`, the same rule a
    /// `NUMERIC`-affinity column already coerces an inserted value under) —
    /// why `id = '1'` matches an `INTEGER` column and `id = '1x'`/
    /// `id = 'abc'` do not.
    Numeric,
    /// One side has `TEXT` affinity and the other has none of its own: an
    /// `INTEGER`/`REAL` operand on the other side renders as text first
    /// (`eval.rs`'s `affinity_conversion`, the `Text` arm) — why `s = 1`
    /// against a `TEXT` column compares `s` to `'1'`, not `1` to a parsed
    /// `s`, and stays false unless `s` is literally `'1'`.
    Text,
}

/// Unary operators on [`Expr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Unary minus.
    Neg,
    /// `NOT`, in SQL's three-valued logic: `NOT NULL` is `NULL`.
    Not,
    /// `IS NULL`. Unlike every other predicate, this one is never itself
    /// unknown — it is how a query asks about the unknown.
    IsNull,
    /// `IS NOT NULL`.
    IsNotNull,
}

/// Binary operators on [`Expr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `=`
    Eq,
    /// `<>`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `||` — string concatenation, `NULL` if either side is `NULL`.
    Concat,
    /// `->` — the JSON representation of the node a path names: quoted text
    /// for a JSON string, the object/array's own text for a composite,
    /// `NULL` for a missing path or a `NULL` operand. Checked against
    /// sqlite3 3.54, which added this operator in 3.38.0 (2022-02-22)
    /// (AHL-490).
    JsonExtractJson,
    /// `->>` — the SQL value at a path: the same node `->` finds, unwrapped
    /// to its native SQL type for a scalar (a JSON string becomes SQL
    /// `TEXT`, `true`/`false` become `1`/`0`), and left as JSON text for a
    /// composite — the one point where `->` and `->>` answer identically,
    /// checked against sqlite3 (AHL-490).
    JsonExtractText,
}

/// A planned `INSERT`.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertPlan {
    /// Target table.
    pub table: String,
    /// Where the rows come from.
    pub source: InsertSource,
    /// What to do with a row that collides with one already stored.
    pub on_conflict: OnConflict,
    /// `RETURNING`, projected over each row as it was written — so an
    /// `INTEGER PRIMARY KEY` the engine assigned is visible, which is most of
    /// why anybody writes it.
    pub returning: Option<Vec<SelectItem>>,
}

/// Where an `INSERT`'s rows come from.
#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    /// `VALUES (...), (...)`: one entry per row, each already widened to full
    /// table width. A cell is an expression rather than a value because it may
    /// be a `?`; `None` means the statement named no value for that column, so
    /// its `DEFAULT` applies — which is a different thing from an explicit
    /// `NULL` and has to stay distinguishable here.
    Values(Vec<Vec<Option<Expr>>>),
    /// `INSERT INTO t (a, b) SELECT ...`: the query, and the column ordinal
    /// each of its output columns lands in. Columns not named take their
    /// defaults, exactly as in the `VALUES` form.
    ///
    /// A [`SubqueryBody`] rather than a bare [`SelectPlan`] since AHL-473: a
    /// compound (`UNION`/`INTERSECT`/`EXCEPT`) works here exactly as a plain
    /// query does. Still refused: a query with no `FROM` at all (`Scalar`) —
    /// unchanged from before, see `sql.rs::plan_insert`.
    Select {
        /// The query supplying the rows.
        query: Box<SubqueryBody>,
        /// Target column ordinals, aligned with the query's output columns.
        targets: Vec<usize>,
    },
}

/// What an `INSERT` does with a row that violates a uniqueness constraint.
///
/// Every one of these used to parse and then be silently discarded, which was
/// the worst of the dropped-clause bugs: the statement whose entire purpose is
/// the clause reported success and did the opposite.
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflict {
    /// The constraint the action answers for, as column ordinals.
    ///
    /// **This narrows what the clause applies to**, which is not obvious and
    /// is exactly what the differential oracle caught: `ON CONFLICT (id) DO
    /// UPDATE` on a row that collides on some *other* unique column is an
    /// ordinary constraint violation, not an upsert. `None` — the `OR IGNORE`
    /// and `OR REPLACE` spellings, and a bare `ON CONFLICT` — answers for any
    /// constraint.
    pub target: Option<Vec<usize>>,
    /// What to do about a conflict the target covers.
    pub action: ConflictAction,
    /// Whether the policy answers for `NOT NULL` and `CHECK` as well as for
    /// uniqueness.
    ///
    /// The two spellings are **not** the same clause, which is the second
    /// thing the differential oracle caught here. `INSERT OR IGNORE` is a
    /// conflict-resolution *algorithm* and SQLite applies it to every
    /// constraint, so a row failing a `CHECK` is skipped. `ON CONFLICT DO
    /// NOTHING` is the upsert clause and covers uniqueness only, so the same
    /// row is an error. Collapsing them would have been wrong in exactly the
    /// cases people write them for.
    pub covers_every_constraint: bool,
}

impl OnConflict {
    /// The default policy: report the violation, write nothing.
    pub fn abort() -> Self {
        Self {
            target: None,
            action: ConflictAction::Abort,
            covers_every_constraint: true,
        }
    }

    /// An `INSERT OR ...` policy: no target, and it answers for every
    /// constraint.
    pub fn or(action: ConflictAction) -> Self {
        Self {
            target: None,
            action,
            covers_every_constraint: true,
        }
    }

    /// An `ON CONFLICT ...` policy, which answers for uniqueness only.
    pub fn clause(target: Option<Vec<usize>>, action: ConflictAction) -> Self {
        Self {
            target,
            action,
            covers_every_constraint: false,
        }
    }
}

/// What to do about a conflict the clause's target covers.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictAction {
    /// The default: report the violation and write nothing (SQLite's `ABORT`).
    Abort,
    /// `INSERT OR IGNORE`, `ON CONFLICT DO NOTHING`: skip the row and carry on.
    Ignore,
    /// `INSERT OR REPLACE`, `REPLACE INTO`: delete every row that conflicts,
    /// then write this one.
    Replace,
    /// `ON CONFLICT (...) DO UPDATE SET ...` — the upsert.
    Update(Box<ConflictUpdate>),
}

/// The `DO UPDATE` half of an upsert.
///
/// Its expressions are resolved over the stored row *followed by* the row the
/// `INSERT` proposed: an ordinal below the table's width reads the row already
/// there, and one at or above it reads `excluded`. That is what lets
/// `SET total = total + excluded.total` mean what it says.
#[derive(Debug, Clone, PartialEq)]
pub struct ConflictUpdate {
    /// Assignments, in `SET` order.
    pub assignments: Vec<(usize, Expr)>,
    /// The optional `WHERE`, over the same pair of rows. A row it excludes is
    /// left exactly as it was — SQLite does not fall back to inserting.
    pub filter: Option<Expr>,
}

/// A planned `SELECT`.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectPlan {
    /// `SELECT DISTINCT`: fold output rows that project equal values into one.
    pub distinct: bool,
    /// The collating sequence each projected column is folded under by
    /// `DISTINCT`, aligned with [`SelectPlan::items`]. Empty when the query is
    /// not `DISTINCT`, since nothing reads it then.
    pub distinct_collations: Vec<Collation>,
    /// Sources, in join order. The first is the driving table.
    pub from: Vec<FromItem>,
    /// Joins, one per table after the first: `joins[i]` joins `from[i + 1]`.
    pub joins: Vec<Join>,
    /// Output columns, in order.
    pub items: Vec<SelectItem>,
    /// The retrieval expression, if the query asked for one. Its presence is
    /// what turns a sequential scan into an index-driven candidate fetch, and
    /// it is answered from the driving table only — see [`ScoreExpr`].
    pub score: Option<ScoreExpr>,
    /// `WHERE` filter: a boolean expression over the row's columns. Rows for
    /// which it is not true (0, `NULL`, …) are dropped.
    pub filter: Option<Expr>,
    /// `GROUP BY` expressions, resolved to the joined row's ordinals.
    pub group_by: Vec<Expr>,
    /// The collating sequence each `GROUP BY` key buckets under, aligned with
    /// [`SelectPlan::group_by`]. Grouping is an equality question, so it asks
    /// the collation the same way a comparison does.
    pub group_collations: Vec<Collation>,
    /// `HAVING` filter, evaluated per group after aggregation.
    pub having: Option<Expr>,
    /// Aggregate functions referenced by the projection, `HAVING` or
    /// `ORDER BY`, in the order they are encountered.
    pub aggregates: Vec<Aggregate>,
    /// Window functions referenced by the projection or `ORDER BY`, in the
    /// order they are encountered — see [`WindowFn`]'s doc for exactly where
    /// one is and is not allowed to appear.
    pub windows: Vec<WindowFn>,
    /// `ORDER BY`, in key order: the first term decides, later terms break
    /// its ties. Empty when the query did not ask for an order.
    pub order: Vec<Order>,
    /// `LIMIT`. An expression rather than a count because SQLite allows
    /// `LIMIT ?`, and a bound parameter is not known until execution.
    pub limit: Option<Expr>,
    /// `OFFSET`, on the same terms as [`SelectPlan::limit`].
    pub offset: Option<Expr>,
}

impl SelectPlan {
    /// This query's output columns: names always, and types wherever an
    /// item projects a stored column of the joined row directly. A derived
    /// table's synthesised columns count — their labels are real even where
    /// their types are not known. See [`Plan::output_columns`].
    pub fn output_columns(&self) -> Vec<ColumnInfo> {
        let tables: Vec<&Table> = self.from.iter().map(|item| &item.table).collect();
        select_item_columns(&self.items, &tables)
    }

    /// Record every stored table this query reads, including through its
    /// derived tables and its subqueries. See [`Expr::tables_read`].
    pub fn tables_read<'a>(&'a self, out: &mut Vec<&'a str>) {
        for item in &self.from {
            match &item.derived {
                // A derived table's name is the alias it was given, which is
                // not a catalog name and must not be stamped as one. What it
                // reads is inside it.
                Some(body) => body.tables_read(out),
                None => out.push(item.table.name.as_str()),
            }
        }
        for join in &self.joins {
            if let Some(on) = &join.on {
                on.tables_read(out);
            }
        }
        for item in &self.items {
            if let SelectItem::Expr { expr, .. } = item {
                expr.tables_read(out);
            }
        }
        if let Some(filter) = &self.filter {
            filter.tables_read(out);
        }
        for expr in &self.group_by {
            expr.tables_read(out);
        }
        if let Some(having) = &self.having {
            having.tables_read(out);
        }
        for aggregate in &self.aggregates {
            if let Some(arg) = &aggregate.arg {
                arg.tables_read(out);
            }
            if let Some(separator) = &aggregate.separator {
                separator.tables_read(out);
            }
            if let Some(filter) = &aggregate.filter {
                filter.tables_read(out);
            }
        }
        for window in &self.windows {
            for arg in &window.args {
                arg.tables_read(out);
            }
            if let Some(filter) = &window.filter {
                filter.tables_read(out);
            }
            for expr in &window.partition_by {
                expr.tables_read(out);
            }
            for term in &window.order_by {
                if let OrderKey::Expr(expr) = &term.key {
                    expr.tables_read(out);
                }
            }
            frame_bound_tables_read(&window.frame.start, out);
            frame_bound_tables_read(&window.frame.end, out);
        }
        for term in &self.order {
            if let OrderKey::Expr(expr) = &term.key {
                expr.tables_read(out);
            }
        }
        if let Some(limit) = &self.limit {
            limit.tables_read(out);
        }
        if let Some(offset) = &self.offset {
            offset.tables_read(out);
        }
    }
}

/// Record every stored table a [`FrameBound`]'s own expression reads, if it
/// has one. The planner only ever resolves a literal or a `?` here — a frame
/// bound is a row count, not a value over the row — but this stays exhaustive
/// with [`Expr::tables_read`] the same way every other walk in this module
/// does, rather than assuming a bound can never grow a column reference.
fn frame_bound_tables_read<'a>(bound: &'a FrameBound, out: &mut Vec<&'a str>) {
    match bound {
        FrameBound::Preceding(expr) | FrameBound::Following(expr) => expr.tables_read(out),
        FrameBound::UnboundedPreceding
        | FrameBound::CurrentRow
        | FrameBound::UnboundedFollowing => {}
    }
}

/// One entry of a `FROM` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct FromItem {
    /// The columns this source contributes to the joined row, and the name it
    /// answers to.
    ///
    /// For a stored table this is the catalog's definition. For a derived table
    /// it is synthesised from the inner query's output labels and is never
    /// looked up in the catalog — see [`FromItem::derived`].
    pub table: Table,
    /// `Some` for `FROM (SELECT ...)`: the query whose rows this source is.
    ///
    /// A derived table cannot be correlated — SQLite has no `LATERAL` — so it
    /// is a [`SubqueryBody`] with no capture list rather than a [`Subquery`].
    pub derived: Option<Box<SubqueryBody>>,
}

impl FromItem {
    /// A plain `FROM t`.
    pub fn table(table: Table) -> Self {
        Self {
            table,
            derived: None,
        }
    }
}

/// How a [`Join`] combines rows from its two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `INNER JOIN` (or plain `JOIN`): only rows that satisfy the predicate.
    Inner,
    /// `LEFT JOIN`: unmatched rows from the left side survive, with the right
    /// side's columns filled with `NULL`.
    Left,
}

/// One join, joining `from[i + 1]` onto the tables before it.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// Whether unmatched left rows survive.
    pub kind: JoinKind,
    /// The `ON` predicate, resolved over the joined row; `None` for a cross
    /// join, which pairs every left row with every right row.
    pub on: Option<Expr>,
}

/// An aggregate function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    /// `COUNT`
    Count,
    /// `SUM`
    Sum,
    /// `MIN`
    Min,
    /// `MAX`
    Max,
    /// `AVG`
    Avg,
    /// `GROUP_CONCAT` — the non-`NULL` values as text, joined by a separator
    /// (`,` unless one is given). `NULL` when the group has no such value.
    GroupConcat,
}

/// One aggregate in a [`SelectPlan`].
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregate {
    /// Which function.
    pub func: AggFunc,
    /// Its argument, resolved over the joined row; `None` means `COUNT(*)`.
    pub arg: Option<Expr>,
    /// `COUNT(DISTINCT x)` and friends: fold duplicate argument values into
    /// one before the function sees them.
    pub distinct: bool,
    /// `GROUP_CONCAT(x, sep)`'s separator. `None` means `,`, and it is
    /// meaningless for every other function.
    pub separator: Option<Expr>,
    /// The collating sequence the argument's *values* are compared under: the
    /// `DISTINCT` fold for any function, and the ordering `MIN`/`MAX` pick from.
    ///
    /// SQLite flags the aggregate `min`/`max` `SQLITE_FUNC_NEEDCOLL` exactly as
    /// it flags the scalar pair, and folds `DISTINCT` under the argument's
    /// collation too — so `COUNT(DISTINCT name)` on a `NOCASE` column counts
    /// `'Ada'` and `'ADA'` once.
    pub collation: Collation,
    /// `FILTER (WHERE ...)`: narrows the rows this aggregate folds, without
    /// touching what any other aggregate or the projection itself sees.
    /// Confirmed against sqlite3 3.54, which accepts `FILTER` on a plain
    /// (non-window) aggregate too — `SUM(x) FILTER (WHERE y > 0)` in an
    /// ordinary `GROUP BY` query is not a window-only clause there, so it is
    /// not one here either. `None` is every aggregate without one.
    pub filter: Option<Expr>,
}

impl Aggregate {
    /// An aggregate with neither `DISTINCT` nor a separator nor a `FILTER` —
    /// every form but `COUNT(DISTINCT ..)`, `GROUP_CONCAT(x, sep)` and
    /// `agg(...) FILTER (WHERE ...)` — under `BINARY`.
    pub fn plain(func: AggFunc, arg: Option<Expr>) -> Self {
        Self {
            func,
            arg,
            distinct: false,
            separator: None,
            collation: Collation::Binary,
            filter: None,
        }
    }
}

/// A window function: `func(args) [FILTER (WHERE ...)] OVER (PARTITION BY
/// ... ORDER BY ... frame)`, one entry in [`SelectPlan::windows`].
///
/// Evaluated after `WHERE`/`GROUP BY`/`HAVING` and before `DISTINCT`/
/// `ORDER BY`/`LIMIT` (`docs/architecture.md` phase 1 item 6): the executor's window stage
/// runs over the rows a `GROUP BY` already folded (or the plain joined rows,
/// for a non-aggregate query), so a window function's own `PARTITION BY`/
/// `ORDER BY` may reference a plan aggregate (`RANK() OVER (ORDER BY
/// SUM(sal))`) exactly as `HAVING` can.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowFn {
    /// Which function.
    pub func: WindowFunc,
    /// The function's own arguments (not `PARTITION BY`/`ORDER BY`), resolved
    /// over the row: `lag`/`lead`'s value and offset/default, `ntile`'s
    /// bucket count, `nth_value`'s `n`, and the aggregate family's single
    /// argument (`None` only for `count(*)`, exactly as
    /// [`Aggregate::arg`]).
    pub args: Vec<Expr>,
    /// `FILTER (WHERE ...)`, aggregate window functions only — confirmed
    /// against sqlite3 3.54, which refuses it on `row_number()` and the rest
    /// of the ranking family with "FILTER clause may only be used with
    /// aggregate window functions". Narrows the frame's rows before the
    /// aggregate folds them, the same rule [`Aggregate::filter`] applies.
    pub filter: Option<Expr>,
    /// `PARTITION BY`, resolved over the row. Empty means the whole result is
    /// one partition.
    pub partition_by: Vec<Expr>,
    /// The collating sequence each `PARTITION BY` key groups under, aligned
    /// with [`WindowFn::partition_by`] — the same single-operand rule
    /// [`SelectPlan::group_collations`] uses, so a `NOCASE` column partitions
    /// under that collation (AHL-469) rather than a second, independent rule.
    pub partition_collations: Vec<Collation>,
    /// `ORDER BY`, within the partition. Reuses [`Order`] — the exact type
    /// `ORDER BY` itself resolves to and [`crate::engine`] already sorts
    /// with — so a window's ordering shares sqlite3's total order and
    /// affinity rules (AHL-477/AHL-486) rather than a second comparator.
    /// Empty means the partition has no defined sequence, which is also what
    /// selects the whole-partition default frame (see [`WindowFrame`]).
    pub order_by: Vec<Order>,
    /// The frame `sum()`/`first_value()`/etc. fold over, relative to each
    /// row's position in the partition's `ORDER BY` sequence.
    pub frame: WindowFrame,
    /// The collating sequence [`WindowFunc::Agg`]'s `min`/`max` order by —
    /// [`Aggregate::collation`]'s exact rule, resolved from
    /// [`WindowFn::args`]'s first (only) argument. [`Collation::Binary`] for
    /// every other function, which never reads it.
    pub collation: Collation,
}

/// The window function family this engine implements.
///
/// Every ranking/navigation function SQLite has (`percent_rank` and
/// `cume_dist` are the two SQLite ships that this does not — see
/// `unsupported.test`), plus the aggregate family reused wholesale as window
/// functions via [`WindowFunc::Agg`], exactly as sqlite3 lets `sum`, `count`,
/// `avg`, `min`, `max` and `group_concat` all appear with an `OVER` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunc {
    /// `row_number()` — 1-based position within the partition's `ORDER BY`
    /// sequence, ties broken by input order. Ignores the frame.
    RowNumber,
    /// `rank()` — 1-based position of the row's *peer group* (rows tied under
    /// `ORDER BY`); a peer group of size `n` advances the next rank by `n`,
    /// so ties leave gaps. Ignores the frame.
    Rank,
    /// `dense_rank()` — like [`Self::Rank`] but with no gaps: the next peer
    /// group always advances by exactly one. Ignores the frame.
    DenseRank,
    /// `ntile(n)` — divides the partition into `n` buckets as evenly as
    /// possible, earlier buckets one row larger when it does not divide
    /// evenly (confirmed against sqlite3: 5 rows into 2 buckets is 3 then 2,
    /// not 2 then 3). `n` must be a positive integer. Ignores the frame.
    Ntile,
    /// `percent_rank()` — `(rank() - 1) / (partition size - 1)` as a real,
    /// `0.0` for a one-row partition rather than a division by zero.
    /// Confirmed against sqlite3 3.54, ties included: `Self::Rank`'s value is
    /// the peer group's own rank, so a tied group answers with its shared
    /// rank exactly as `rank()` does. Ignores the frame.
    PercentRank,
    /// `cume_dist()` — the fraction of the partition at or before the
    /// current row's peer group: `(peer group's last position + 1) /
    /// partition size`. Confirmed against sqlite3 3.54: with no `ORDER BY`
    /// the whole partition is one peer group, so every row answers `1.0`.
    /// Ignores the frame.
    CumeDist,
    /// `lag(expr[, offset[, default]])` — `expr` evaluated `offset` rows
    /// (default 1) back in the `ORDER BY` sequence, or `default` (default
    /// `NULL`) when that position falls outside the partition. A negative
    /// `offset` reaches forward instead — confirmed against sqlite3, which
    /// answers `lag(x, -1)` exactly as `lead(x, 1)`. Ignores the frame.
    Lag,
    /// `lead(expr[, offset[, default]])` — the mirror of [`Self::Lag`],
    /// forward by default. Ignores the frame.
    Lead,
    /// `first_value(expr)` — `expr` at the frame's first row.
    FirstValue,
    /// `last_value(expr)` — `expr` at the frame's last row. With the default
    /// frame this is usually *not* the partition's last row — see
    /// [`WindowFrame`]'s doc for why `last_value()` with no explicit frame
    /// answers the current row's own value far more often than a query's
    /// author expects.
    LastValue,
    /// `nth_value(expr, n)` — `expr` at the frame's `n`th row (1-based), or
    /// `NULL` if the frame has fewer than `n` rows. `n` must be a positive
    /// integer.
    NthValue,
    /// One of the aggregate functions, applied over the frame's rows instead
    /// of the whole group — `sum(x) OVER (...)` and the rest of
    /// [`AggFunc`]. [`WindowFn::args`] holds its single argument (or is empty
    /// for `count(*)`), and [`WindowFn::filter`] is its `FILTER`.
    Agg(AggFunc),
}

impl WindowFunc {
    /// Whether this function reads the frame at all. The ranking and
    /// navigation functions answer from the row's *position*, not from a
    /// range of rows, so a frame clause on one of them is accepted (SQLite
    /// parses it) but has no effect — matching sqlite3, which does not
    /// refuse `row_number() OVER (ORDER BY x ROWS BETWEEN ...)` even though
    /// the frame changes nothing about the answer.
    pub fn reads_frame(self) -> bool {
        matches!(
            self,
            WindowFunc::FirstValue | WindowFunc::LastValue | WindowFunc::NthValue
        ) || matches!(self, WindowFunc::Agg(_))
    }
}

/// A window frame: which rows of the partition, relative to the current row,
/// [`WindowFunc::FirstValue`]/[`LastValue`]/[`NthValue`]/[`WindowFunc::Agg`]
/// fold over. The ranking and navigation functions ([`WindowFunc::reads_frame`]
/// is `false`) ignore this entirely.
///
/// Only `ROWS` frames (position-counted) are implemented; an explicit `RANGE`
/// or `GROUPS` frame is refused by name at resolution time (`sql.rs`) rather
/// than silently treated as `ROWS`, since a value-based `RANGE` frame answers
/// a different question than a position-based one the moment `ORDER BY` has
/// ties. What *is* implemented is SQLite's implicit default frame, which is
/// itself defined in `RANGE` terms and needs its own peer-group-aware
/// evaluation, not the `ROWS` one — confirmed against sqlite3 3.54 with a
/// tied `ORDER BY` column (`docs` in the window-functions sqllogictest file
/// has the measurement):
///
/// * **No `ORDER BY` at all** (in this window's own clause): the default
///   frame is the whole partition, whichever function reads it.
/// * **An `ORDER BY`, no explicit frame**: the default is "`RANGE BETWEEN
///   UNBOUNDED PRECEDING AND CURRENT ROW`", which despite the name `CURRENT
///   ROW` extends to the *end of the current row's peer group* — every row
///   that ties with it under `ORDER BY` — not merely up to the current row's
///   own position. This is why `last_value()` with no frame so often answers
///   the current row's own value: it only does when every row is its own
///   peer group (an `ORDER BY` with no ties), and answers the *whole tied
///   group's* last row otherwise.
///
/// [`WindowFrame::rows`] tells the executor which reading applies:
/// position-based bounds (an explicit `ROWS` frame) count rows in the
/// partition's `ORDER BY` sequence; `false` (no explicit frame) asks for the
/// two defaults above, chosen by whether [`WindowFn::order_by`] is empty.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowFrame {
    /// `true` for an explicit `ROWS BETWEEN ... AND ...` (or its `ROWS <bound>`
    /// shorthand, equivalent to `... AND CURRENT ROW`): bounds count rows by
    /// position. `false` is SQLite's implicit default, whose meaning depends
    /// on whether [`WindowFn::order_by`] is empty — see this type's doc.
    pub rows: bool,
    /// The frame's start bound.
    pub start: FrameBound,
    /// The frame's end bound.
    pub end: FrameBound,
}

impl WindowFrame {
    /// The frame SQLite uses when a window has no `ORDER BY` of its own: the
    /// whole partition, for every row in it.
    pub fn whole_partition() -> Self {
        Self {
            rows: false,
            start: FrameBound::UnboundedPreceding,
            end: FrameBound::UnboundedFollowing,
        }
    }

    /// The frame SQLite defaults to when a window has an `ORDER BY` but no
    /// explicit frame clause: from the start of the partition to the end of
    /// the current row's peer group.
    pub fn default_range() -> Self {
        Self {
            rows: false,
            start: FrameBound::UnboundedPreceding,
            end: FrameBound::CurrentRow,
        }
    }
}

/// One edge of a [`WindowFrame`].
///
/// `Preceding`/`Following` carry an expression rather than a bare count for
/// the same reason [`SelectPlan::limit`] does: SQLite allows a bound
/// parameter (`ROWS BETWEEN ? PRECEDING AND CURRENT ROW`), so the row count
/// is not known until execution. Only meaningful when [`WindowFrame::rows`]
/// is `true` — the default-frame bounds ([`WindowFrame::whole_partition`],
/// [`WindowFrame::default_range`]) only ever use the three constant
/// variants.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameBound {
    /// The partition's first row.
    UnboundedPreceding,
    /// `<expr> PRECEDING` — the row this many positions before the current
    /// one, clamped to the partition's start when it would fall before it.
    Preceding(Box<Expr>),
    /// The current row's own position. In a `ROWS` frame this is literally
    /// the row's index; the *default* `RANGE`-shaped frame reinterprets an
    /// end bound of this variant as "the end of the current row's peer
    /// group" instead — see [`WindowFrame`]'s doc.
    CurrentRow,
    /// `<expr> FOLLOWING` — the mirror of [`Self::Preceding`].
    Following(Box<Expr>),
    /// The partition's last row.
    UnboundedFollowing,
}

/// One output column.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// A stored column, projected by ordinal.
    Column {
        /// Ordinal in the table.
        index: usize,
        /// Header to report.
        label: String,
    },
    /// A scalar expression over the row, projected by value.
    Expr {
        /// The expression to evaluate per row.
        expr: Expr,
        /// Header to report.
        label: String,
    },
    /// The retrieval score computed by [`SelectPlan::score`].
    Score {
        /// Header to report.
        label: String,
    },
}

impl SelectItem {
    /// The header this item contributes to the result set.
    pub fn label(&self) -> &str {
        match self {
            SelectItem::Column { label, .. }
            | SelectItem::Expr { label, .. }
            | SelectItem::Score { label } => label,
        }
    }
}

/// A retrieval expression.
///
/// Each leaf is answered by exactly one index; [`ScoreExpr::Fuse`] combines
/// the resulting ranked lists. In a joined query the expression may reference
/// only the driving table (the first in `FROM`): a retrieval index lives over
/// one table's rows, so a join changes what "the row" is in a way a single
/// probe cannot rank. See `README.md`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScoreExpr {
    /// `vector_score(column, embedding)` — approximate nearest neighbours.
    ///
    /// Always one column: a vector index never covers more than one (see
    /// [`crate::catalog::IndexKind::Vector`]), so there is nothing to make
    /// this variant plural for.
    Vector {
        /// Vector column ordinal.
        column: usize,
        /// Query embedding: a literal, or the `?` it will be bound from.
        query: Expr,
    },
    /// `bm25_score(column [, column ...], 'terms')` — full-text relevance.
    ///
    /// One or more columns, matching a full-text index of the same column
    /// set (order does not matter — see `Engine::text_index`) declared over
    /// the driving table. `bm25_score(body, ?)` — the single-column case — is
    /// exactly what this has always meant; naming more than one column asks
    /// for a multi-column index's combined score, MySQL's
    /// `MATCH(a, b) AGAINST(...)`.
    Text {
        /// Text column ordinals, as named in the call, in written order.
        columns: Vec<usize>,
        /// Query string: a literal, or the `?` it will be bound from.
        query: Expr,
    },
    /// `fuse(a, b, ...)` — reciprocal rank fusion over the child lists.
    Fuse {
        /// Child expressions; each contributes one ranked list.
        parts: Vec<ScoreExpr>,
        /// The RRF damping constant.
        k: f32,
    },
}

/// One `ORDER BY` term.
#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    /// What to sort on.
    pub key: OrderKey,
    /// The collating sequence text keys are compared under.
    ///
    /// SQLite's rule for a sort term is the same shape as for a comparison but
    /// with one operand: an explicit `COLLATE` on the term wins, otherwise the
    /// column's declared collation, otherwise `BINARY`.
    pub collation: Collation,
    /// Descending when true.
    pub desc: bool,
    /// Where `NULL`s go, which is a separate question from the direction.
    ///
    /// SQLite sorts `NULL` below every value, so the default is
    /// `!desc` — first ascending, last descending — and an explicit
    /// `NULLS FIRST`/`NULLS LAST` overrides it in either direction.
    pub nulls_first: bool,
}

impl Order {
    /// An ordering with SQLite's default `NULL` placement for its direction,
    /// under the `BINARY` collation.
    pub fn new(key: OrderKey, desc: bool) -> Self {
        Self {
            key,
            collation: Collation::Binary,
            desc,
            nulls_first: !desc,
        }
    }
}

/// The sort key.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderKey {
    /// Sort on the retrieval score.
    Score,
    /// Sort on a stored column.
    Column(usize),
    /// Sort on a scalar expression over the row.
    Expr(Expr),
}
