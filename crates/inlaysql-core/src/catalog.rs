//! Table definitions, index declarations and their persistent encoding.
//!
//! Identifiers are compared case-insensitively (SQLite's behaviour) but the
//! spelling the user wrote is preserved for result-set headers.
//!
//! # Format versions and grandfathering
//!
//! The catalog has its own on-disk version, separate from the B-tree's page
//! format version (see `crate::btree`). Version 2 added index declarations:
//! before it, every `TEXT` and `VECTOR` column was indexed implicitly and the
//! catalog recorded nothing about *which* indexes were wanted. Version 3 adds
//! the opt-in `VECTOR(n, INT8)` type tag. Version 4 adds declared constraints —
//! `NOT NULL`, `DEFAULT`, `UNIQUE`, `CHECK`, `FOREIGN KEY` — and the `NUMERIC`
//! affinity that SQLite's type rules produce for every name it does not
//! recognise. Version 5 adds scalar B-tree indexes ([`IndexKind::BTree`]),
//! which brought the first index declaration that can name more than one
//! column and the first that can be `UNIQUE`. The multi-column encoding it
//! introduced is generic in [`IndexKind`], not B-tree-specific — see
//! [`Catalog::required_version`] — so a multi-column `FullText` index
//! (composite/multi-column retrieval indexes) also forces version 5 and
//! needed no format change of its own. Version 6 adds declared
//! collations ([`crate::collation::Collation`]) on a column and on each column
//! of an index. Version 7 adds a vector index's distance metric
//! ([`crate::hnsw::VectorMetric`]).
//!
//! **A catalog is written at the lowest version that can express it.** A
//! database with exact vectors and no constraints is still written as version
//! 2, so opening and editing it does not make it unreadable to the build that
//! created it; only a table that actually declares a constraint (or uses
//! `NUMERIC` or `VECTOR(n, INT8)`) forces the higher version, only a
//! B-tree index forces version 5, only a collation that is not `BINARY`
//! forces version 6, and only a vector index whose metric is not cosine
//! forces version 7. The pre-1.0 policy is *recreate, not
//! migrate*: a build that predates version 5 refuses a version-5 catalog with
//! [`Error::FormatVersion`] and reads nothing, rather than decoding the table
//! section and silently losing the index declarations that follow it — which
//! for a B-tree index would be the worst possible failure, since the entries
//! would still be in the tree and would still be read.
//!
//! Version 6 forces the bump for exactly the same reason, one step further in:
//! a `NOCASE` index's entries are keyed by the *folded* value
//! ([`crate::index`]), so a build that read the declaration without the
//! collation would probe the unfolded key, miss every entry, and answer
//! `WHERE name = 'ADA'` with nothing while the table still held the row.
//!
//! Version 7 forces it for the vector equivalent: an index declared
//! `vector_l2_ops` whose metric an older build did not read would be rebuilt
//! as a cosine graph over unnormalised vectors and would answer `vector_score`
//! with the wrong neighbours — again with no error anywhere, because both
//! metrics are defined on the same embeddings.
//!
//! A version-1 catalog (written by an older binary) is decoded and
//! **grandfathered**: every indexable column of every table it describes is
//! given an implicit index declaration, so a database written before
//! `CREATE INDEX` existed keeps the behaviour it was built under. New tables
//! created after that point opt in with `CREATE INDEX` like any other. This is
//! the migration answer: existing columns keep what they have, and nothing is
//! silently dropped.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::collation::Collation;
use crate::error::{Error, Result};
use crate::hnsw::VectorMetric;
use crate::row::{put_len, put_string, Cursor};
use crate::value::DataType;

/// Metadata key under which the catalog is stored.
pub const CATALOG_KEY: &str = "catalog";

/// Magic prefix of a versioned catalog. The first four bytes of a version-1
/// catalog are a `u32` table count, so a value that could never be one
/// distinguishes the two unambiguously.
const CATALOG_MAGIC: &[u8; 4] = b"ISQL";

const CATALOG_VERSION_EXACT: u32 = 2;
const CATALOG_VERSION_QUANTIZED: u32 = 3;
const CATALOG_VERSION_CONSTRAINTS: u32 = 4;
const CATALOG_VERSION_BTREE: u32 = 5;
const CATALOG_VERSION_COLLATION: u32 = 6;
const CATALOG_VERSION_METRIC: u32 = 7;
const CATALOG_VERSION_STRICT: u32 = 8;
const CATALOG_VERSION_WITHOUT_ROWID: u32 = 9;

const TYPE_INTEGER: u8 = 1;
const TYPE_REAL: u8 = 2;
const TYPE_TEXT: u8 = 3;
const TYPE_BLOB: u8 = 4;
const TYPE_VECTOR: u8 = 5;
const TYPE_VECTOR_Q8: u8 = 6;
const TYPE_NUMERIC: u8 = 7;
const TYPE_ANY: u8 = 8;

const INDEX_FULL_TEXT: u8 = 1;
const INDEX_VECTOR: u8 = 2;
const INDEX_BTREE: u8 = 3;

/// Set in the high bit of a column's type tag when the column is the table's
/// `INTEGER PRIMARY KEY`.
///
/// Encoding the flag inside the existing tag byte rather than adding a field
/// keeps databases written before primary keys existed readable: their tags
/// have the bit clear, which decodes to "not a primary key".
const PRIMARY_KEY_FLAG: u8 = 0x80;

/// One column of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column name as written in `CREATE TABLE`.
    pub name: String,
    /// Declared type.
    pub ty: DataType,
    /// Whether this column is the table's `INTEGER PRIMARY KEY`.
    ///
    /// As in SQLite, such a column is not a separate index: it *is* the row
    /// id. The value the caller inserts becomes the key the row is stored
    /// under, so looking a row up by it is a single tree descent rather than a
    /// scan. See [`Table::rowid_alias`].
    pub primary_key: bool,
    /// Whether the column was declared `NOT NULL`.
    pub not_null: bool,
    /// The `DEFAULT` expression, as it was written.
    ///
    /// Kept as text rather than as a resolved [`crate::plan::Expr`] because the
    /// catalog is a durable format and a plan is not: an expression tree would
    /// have to be versioned byte for byte, where SQL text is what the user
    /// wrote and what `sqlite_master` would have stored. It is re-resolved at
    /// plan time, against the table as it stands then.
    pub default: Option<String>,
    /// The collating sequence `COLLATE` declared for this column, or
    /// [`Collation::Binary`] when it declared none.
    ///
    /// This is the *implicit* collation SQLite's resolution rules reach for
    /// when a comparison carries no explicit `COLLATE` of its own — see
    /// [`crate::collation`]. It is recorded whatever the column's declared
    /// type and consulted only when both sides of a comparison are `TEXT`,
    /// which is SQLite's rule too: `COLLATE NOCASE` on an `INTEGER` column is
    /// accepted and simply never asked.
    pub collation: Collation,
}

/// One `FOREIGN KEY` declaration, recorded and **not enforced**.
///
/// SQLite has shipped with foreign keys off by default since they were added
/// in 3.6.19, and every framework's migrations are written for that. Recording
/// the declaration without enforcing it is therefore the compatible answer —
/// but it is only honest if it is said out loud, which is what this type's
/// existence and `README`/`TESTING.md` do. The alternative, silently dropping
/// the clause, is the bug class the previous phase existed to close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// Columns of this table, in declaration order.
    pub columns: Vec<String>,
    /// The referenced table, as written.
    pub table: String,
    /// The referenced columns; empty means "that table's primary key".
    pub referenced: Vec<String>,
    /// `ON DELETE` action as written (`CASCADE`, `SET NULL`, …), if any.
    pub on_delete: Option<String>,
    /// `ON UPDATE` action as written, if any.
    pub on_update: Option<String>,
}

/// One `UNIQUE` constraint over one or more columns.
///
/// It carries a name when it came from `CREATE UNIQUE INDEX`, so that
/// `DROP INDEX` can find it again. A constraint written inside `CREATE TABLE`
/// has no name and cannot be dropped on its own, which is SQLite's rule too.
///
/// **A constraint, and now an access path too.** When every column it covers
/// is orderable, a matching [`IndexKind::BTree`] index is declared beside it —
/// by `CREATE UNIQUE INDEX` under the same name, or by `CREATE TABLE` under a
/// generated one — and enforcing the constraint becomes a probe of that index
/// rather than a scan of the table per row written (`docs/architecture.md`, decision D3).
/// A `UNIQUE` over a `VECTOR` column has no ordered index to use and keeps the
/// scan; it is correct and it is slow, and it says so here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueConstraint {
    /// The name `CREATE UNIQUE INDEX` gave it, if it came from one.
    pub name: Option<String>,
    /// The columns it covers, in declaration order.
    pub columns: Vec<String>,
}

impl UniqueConstraint {
    /// An unnamed constraint, as written inside a `CREATE TABLE`.
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            name: None,
            columns,
        }
    }
}

/// Everything a `CREATE TABLE` declared that is not one column's own name,
/// type, `NOT NULL` or `DEFAULT`.
///
/// These live beside [`Table`] rather than inside it because they are not part
/// of a row's *shape*: a plan holds column ordinals and a prepared statement
/// re-checks the shape it resolved them against, while constraints are read
/// from the live catalog on every execution and so can never go stale in a
/// plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableConstraints {
    /// `UNIQUE` constraints. A column-level `UNIQUE` is a one-column entry,
    /// exactly as SQLite treats it.
    pub unique: Vec<UniqueConstraint>,
    /// `CHECK` expressions, as written. Column-level and table-level `CHECK`
    /// mean the same thing and are held in one list.
    pub checks: Vec<String>,
    /// `FOREIGN KEY` declarations. See [`ForeignKey`] — recorded, never
    /// enforced.
    pub foreign_keys: Vec<ForeignKey>,
}

impl TableConstraints {
    /// Whether nothing at all was declared, which is what decides whether the
    /// catalog needs the version-4 encoding.
    pub fn is_empty(&self) -> bool {
        self.unique.is_empty() && self.checks.is_empty() && self.foreign_keys.is_empty()
    }

    /// Rename every reference to `old` as `new`, for `ALTER TABLE RENAME
    /// COLUMN`. `CHECK` expressions are rewritten by the caller, which has the
    /// parser; this handles the parts that are plain names.
    fn rename_column(&mut self, old: &str, new: &str) {
        for group in &mut self.unique {
            for column in group.columns.iter_mut() {
                if column.eq_ignore_ascii_case(old) {
                    *column = new.to_string();
                }
            }
        }
        for key in &mut self.foreign_keys {
            for column in key.columns.iter_mut() {
                if column.eq_ignore_ascii_case(old) {
                    *column = new.to_string();
                }
            }
        }
    }

    /// Whether any `UNIQUE` or `FOREIGN KEY` names this column.
    fn mentions(&self, column: &str) -> bool {
        self.unique
            .iter()
            .flat_map(|group| group.columns.iter())
            .chain(self.foreign_keys.iter().flat_map(|key| key.columns.iter()))
            .any(|name| name.eq_ignore_ascii_case(column))
    }
}

/// One table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Table name as written in `CREATE TABLE`.
    pub name: String,
    /// Columns in declaration order; the ordinal is the row's field index.
    pub columns: Vec<Column>,
    /// Whether this table was declared `STRICT`.
    ///
    /// A strict column's declared type must be one of `INT`/`INTEGER`,
    /// `REAL`, `TEXT`, `BLOB` or `ANY` — nothing else, and no length or
    /// precision modifier — and `sql::coerce` checks and converts a value
    /// against it more narrowly than an ordinary affinity does: an `INTEGER`
    /// column accepts a `REAL` only when it converts losslessly, a `TEXT`
    /// column accepts a number by rendering it exactly as `CAST(x AS TEXT)`
    /// would, and every other combination that an ordinary affinity would
    /// coerce or accept is instead a type error.
    pub strict: bool,
    /// Whether this table was declared `WITHOUT ROWID`.
    ///
    /// There is no hidden row id at all — [`Table::rowid_alias`] is always
    /// `None` here even for a single `INTEGER PRIMARY KEY` column, unlike an
    /// ordinary table, confirmed against sqlite3 — so the row is stored
    /// under its own [`Table::primary_key`] columns' encoded bytes instead
    /// (`storage::primary_key_bytes`, reusing the same collation-aware
    /// value encoding a scalar index's entry key uses). `AUTOINCREMENT`
    /// combined with this is refused at plan time, the same as sqlite3
    /// refuses it: there is nothing for it to increment.
    pub without_rowid: bool,
    /// The primary key's columns, by name, in the order the `PRIMARY KEY`
    /// clause declared them — the order that decides how the composite
    /// storage key sorts, so declaration order and not table-column order.
    ///
    /// Empty exactly when `!without_rowid`; an ordinary table's primary key
    /// (a single `INTEGER PRIMARY KEY` column, or a composite one) is
    /// represented the existing ways instead — [`Column::primary_key`], or
    /// an ordinary [`UniqueConstraint`] backed by a secondary index — neither
    /// of which is disturbed by this field existing.
    pub primary_key: Vec<String>,
}

impl Table {
    /// A non-`STRICT`, rowid table — every table this engine built before
    /// `STRICT`/`WITHOUT ROWID` existed, and every synthetic one (a virtual
    /// `information_schema` view, a test fixture) that is not real user
    /// schema.
    pub fn new(name: String, columns: Vec<Column>) -> Self {
        Self {
            name,
            columns,
            strict: false,
            without_rowid: false,
            primary_key: Vec::new(),
        }
    }

    /// The ordinal of the column that aliases the row id, if the table has one.
    ///
    /// This is the single fact the planner needs to turn `WHERE id = 42` into a
    /// point lookup, and the executor needs to store a row under the id the
    /// caller supplied.
    pub fn rowid_alias(&self) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.primary_key && column.ty == DataType::Integer)
    }

    /// Look a column up by name, case-insensitively.
    pub fn column(&self, name: &str) -> Option<(usize, &Column)> {
        self.columns
            .iter()
            .enumerate()
            .find(|(_, c)| c.name.eq_ignore_ascii_case(name))
    }

    /// Look a column up by name, or fail with a catalog error.
    pub fn require_column(&self, name: &str) -> Result<(usize, &Column)> {
        self.column(name).ok_or_else(|| {
            Error::Catalog(alloc::format!(
                "no column `{name}` on table `{}`",
                self.name
            ))
        })
    }
}

/// Which structure an index declaration names.
///
/// The kind is inferred from the column type unless the statement says
/// otherwise with `USING`: a `VECTOR` column can only carry a
/// nearest-neighbour index, a `TEXT` column defaults to full-text (which is
/// what `CREATE INDEX` on a `TEXT` column has always meant here), and every
/// other scalar column gets an ordered B-tree index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// An Okapi BM25 full-text index.
    ///
    /// Usually one column, but — unlike [`IndexKind::Vector`] — it can name
    /// several: `CREATE INDEX idx ON docs (title, body) USING FULLTEXT` is
    /// MySQL's `FULLTEXT(title, body)`, one combined relevance score over the
    /// concatenation of every named column's text. There is no ambiguity in
    /// what that means the way there is for two embedding columns, so this is
    /// the one retrieval kind the single-column restriction does not apply
    /// to.
    FullText,
    /// An approximate-nearest-neighbour vector index.
    ///
    /// Always exactly one column. Two `VECTOR` columns are, in general, two
    /// different embedding spaces; there is no standard meaning for one HNSW
    /// graph over both, so unlike [`IndexKind::FullText`] this kind keeps the
    /// single-column restriction.
    Vector,
    /// An ordered scalar index: entries in the same copy-on-write tree as the
    /// rows, keyed by [`crate::index`]'s memcomparable encoding. This is the
    /// only kind that can cover more than one column and the only one that can
    /// be `UNIQUE`.
    BTree,
}

impl IndexKind {
    /// Whether this kind is one the engine holds as a separate retrieval
    /// backend (and therefore saves, loads and rebuilds), rather than as rows
    /// in the tree.
    pub fn is_retrieval(self) -> bool {
        matches!(self, IndexKind::FullText | IndexKind::Vector)
    }
}

/// One index declaration, from `CREATE INDEX name ON table (columns...)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// Index name as written in `CREATE INDEX`.
    ///
    /// Names are unique across the whole catalog — a named `UNIQUE`
    /// constraint shares the namespace — which is what lets
    /// [`crate::index`] key a B-tree index's entries by name alone.
    pub name: String,
    /// Lowercased table name it indexes.
    pub table: String,
    /// Lowercased column names it indexes, in declaration order. Never empty.
    ///
    /// [`IndexKind::BTree`] and [`IndexKind::FullText`] can both have more
    /// than one; [`IndexKind::Vector`] is always exactly one, and the catalog
    /// enforces that — see the two variants' docs for why the line falls
    /// there and not in the same place for both retrieval kinds.
    pub columns: Vec<String>,
    /// Which structure backs it.
    pub kind: IndexKind,
    /// Whether the declaration was `CREATE UNIQUE INDEX`.
    ///
    /// A unique B-tree index is the access path *and* the enforcement for the
    /// matching [`UniqueConstraint`]; the constraint is what owns the error
    /// message, so both are recorded.
    pub unique: bool,
    /// The collating sequence each column of this index is keyed under, in the
    /// same order as [`Index::columns`].
    ///
    /// **This is part of the key format, not decoration.** A `NOCASE` column's
    /// entries hold the folded value ([`crate::index::encode_value`]), so a
    /// probe that resolved a different collation would read a different run of
    /// bytes and answer a different set of rows than a scan. That is why the
    /// planner will only choose an index whose collation for a column *equals*
    /// the collation the comparison resolved — SQLite's rule, and the reason
    /// this is recorded per column rather than inferred from the table.
    ///
    /// Always the same length as [`Index::columns`]: an index decoded from a
    /// catalog written before version 6 is filled with `BINARY`, which is the
    /// only thing those builds could write, and [`Catalog::create_index`]
    /// refuses a declaration whose two lists disagree rather than padding one.
    pub collations: Vec<Collation>,
    /// The distance an [`IndexKind::Vector`] index's graph is built and
    /// searched under, from the operator class the declaration wrote
    /// (`vector_l2_ops`) or [`VectorMetric::Cosine`] when it wrote none.
    ///
    /// **This is part of what the index *is*, not decoration.** An HNSW
    /// graph's neighbour lists are the answer to "what is near what" under one
    /// distance; searched under another they route the walk by the wrong
    /// geometry and return plausible, wrong rows with no error — so this
    /// travels to the backend at open time and is checked against the graph on
    /// disk there ([`crate::hnsw::HnswIndex::load`],
    /// [`crate::hnsw_paged::PagedHnswIndex::restore`]).
    ///
    /// Every other kind carries [`VectorMetric::Cosine`] and means nothing by
    /// it; [`Catalog::create_index`] refuses a declaration that says otherwise
    /// rather than record a metric nothing will read.
    pub metric: VectorMetric,
}

impl Index {
    /// A single-column declaration, which is every retrieval index. Cosine,
    /// which is the only metric a non-vector index can carry and the default
    /// for one that is.
    pub fn single(name: String, table: String, column: String, kind: IndexKind) -> Self {
        Self {
            name,
            table,
            columns: alloc::vec![column],
            kind,
            unique: false,
            collations: alloc::vec![Collation::Binary],
            metric: VectorMetric::Cosine,
        }
    }

    /// A single-column [`IndexKind::Vector`] declaration under an explicit
    /// metric.
    pub fn vector(name: String, table: String, column: String, metric: VectorMetric) -> Self {
        Self {
            metric,
            ..Self::single(name, table, column, IndexKind::Vector)
        }
    }

    /// The collating sequence this index keys its column at `position` under.
    ///
    /// `BINARY` for a position the declaration recorded nothing for, which is
    /// every column of every index written before catalog version 6.
    pub fn collation(&self, position: usize) -> Collation {
        crate::collation::at(&self.collations, position)
    }

    /// The first (often only) column.
    ///
    /// A [`IndexKind::Vector`] index has exactly one column, so this is
    /// always *the* column for one; a [`IndexKind::FullText`] index may have
    /// more, and callers that need all of them should read [`Index::columns`]
    /// instead — the retrieval backends are keyed by `(table, columns)`, not
    /// `(table, column)`, precisely so a multi-column `FullText` index is not
    /// forced through this method.
    pub fn column(&self) -> &str {
        self.columns.first().map_or("", String::as_str)
    }

    /// Whether this declaration covers exactly `columns`, in order.
    pub fn covers(&self, columns: &[String]) -> bool {
        self.columns.len() == columns.len()
            && self
                .columns
                .iter()
                .zip(columns)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// Whether this declaration covers exactly `columns`, in order, **under
    /// exactly `collations`**.
    ///
    /// Two indexes over the same columns under different collations are two
    /// different indexes, not a duplicate: their entries are keyed by different
    /// bytes and each answers a comparison the other cannot. SQLite allows both
    /// to exist for that reason, and so does this.
    pub fn covers_collated(&self, columns: &[String], collations: &[Collation]) -> bool {
        self.covers(columns)
            && (0..columns.len()).all(|position| {
                self.collation(position) == crate::collation::at(collations, position)
            })
    }
}

/// The deterministic name a grandfathered or implicitly-created index gets.
///
/// These are real, droppable declarations once materialised; the prefix marks
/// them as engine-created rather than user-written.
pub(crate) fn auto_index_name(table: &str, column: &str) -> String {
    alloc::format!("__inlaysql_auto_{}_{}", table, column)
}

/// The name of the B-tree index that backs an unnamed `UNIQUE` constraint —
/// one written inside `CREATE TABLE`, which SQLite gives no name either.
///
/// The `__inlaysql_uniq_` prefix keeps it out of [`auto_index_name`]'s space
/// as well as the user's, and `nth` disambiguates two constraints whose column
/// lists render the same (`UNIQUE (a_b)` beside `UNIQUE (a, b)`).
pub(crate) fn auto_unique_index_name(table: &str, columns: &[String], nth: usize) -> String {
    alloc::format!("__inlaysql_uniq_{}_{}_{}", table, columns.join("_"), nth)
}

/// The name prefix a layer *above* the engine reserves for its own tables.
///
/// The engine itself attaches no meaning to it beyond this constant: it will
/// create, read and drop such a table like any other, because it has no
/// concept of a privileged one. What the prefix buys is a single rule that
/// every layer with something to hide can apply — the MySQL-wire server keeps
/// its account store here (`crates/inlaysql-server/src/acl.rs`), and both that
/// server and the MCP server refuse to name one of these tables in an answer
/// or a statement. Declared once, in the crate both of them depend on, so the
/// rule cannot be spelled two slightly different ways.
///
/// The catalog's own generated unique-index names share the prefix
/// ([`auto_unique_index_name`]); those are index names, not table names, and
/// [`is_reserved_table_name`] is only ever asked about the latter.
pub const RESERVED_TABLE_PREFIX: &str = "__inlaysql_";

/// Whether `name` is a table reserved for a layer above the engine.
///
/// Case-insensitively, because table names are: `SELECT * FROM __INLAYSQL_USER`
/// reaches the same table and has to be caught by the same rule. The bare
/// prefix with nothing after it is *not* reserved — a rule that matched it
/// would be a rule with no table behind it.
pub fn is_reserved_table_name(name: &str) -> bool {
    name.len() > RESERVED_TABLE_PREFIX.len()
        && name[..RESERVED_TABLE_PREFIX.len()].eq_ignore_ascii_case(RESERVED_TABLE_PREFIX)
}

/// All tables and indexes known to a database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
    /// Declared constraints, keyed by lowercased table name. A table with
    /// nothing declared has no entry.
    constraints: BTreeMap<String, TableConstraints>,
    /// Index declarations, keyed by lowercased name.
    indexes: BTreeMap<String, Index>,
}

impl Catalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new table with no declared constraints. Fails if the name is
    /// already taken.
    pub fn create_table(&mut self, table: Table) -> Result<()> {
        self.create_table_with(table, TableConstraints::default())
    }

    /// Register a new table and the constraints its `CREATE TABLE` declared.
    pub fn create_table_with(&mut self, table: Table, constraints: TableConstraints) -> Result<()> {
        let key = table.name.to_ascii_lowercase();
        if self.tables.contains_key(&key) {
            return Err(Error::Catalog(alloc::format!(
                "table `{}` already exists",
                table.name
            )));
        }
        // Every name a constraint mentions has to exist, or the constraint
        // could never fire and the table would claim a guarantee it does not
        // have.
        for column in constraints
            .unique
            .iter()
            .flat_map(|group| group.columns.iter())
            .chain(
                constraints
                    .foreign_keys
                    .iter()
                    .flat_map(|k| k.columns.iter()),
            )
        {
            table.require_column(column)?;
        }
        if !constraints.is_empty() {
            self.constraints.insert(key.clone(), constraints);
        }
        self.tables.insert(key, table);
        Ok(())
    }

    /// The constraints one table declared, or `None` when it declared none.
    pub fn constraints(&self, table: &str) -> Option<&TableConstraints> {
        self.constraints.get(&table.to_ascii_lowercase())
    }

    /// Remove a table, its constraints and every index declared on it.
    ///
    /// Returns the table and the index declarations that went with it, so the
    /// caller can drop the backends and the rows they describe.
    pub fn drop_table(&mut self, name: &str) -> Result<(Table, Vec<Index>)> {
        let key = name.to_ascii_lowercase();
        let Some(table) = self.tables.remove(&key) else {
            return Err(Error::Catalog(alloc::format!("no such table: {name}")));
        };
        self.constraints.remove(&key);
        let dropped: Vec<String> = self
            .indexes
            .iter()
            .filter(|(_, index)| index.table.eq_ignore_ascii_case(&key))
            .map(|(name, _)| name.clone())
            .collect();
        let indexes = dropped
            .iter()
            .filter_map(|name| self.indexes.remove(name))
            .collect();
        Ok((table, indexes))
    }

    /// `ALTER TABLE ... RENAME TO`: move a table, its constraints and its index
    /// declarations to a new name.
    pub fn rename_table(&mut self, from: &str, to: &str) -> Result<()> {
        let old = from.to_ascii_lowercase();
        let new = to.to_ascii_lowercase();
        if !self.tables.contains_key(&old) {
            return Err(Error::Catalog(alloc::format!("no such table: {from}")));
        }
        if old != new && self.tables.contains_key(&new) {
            return Err(Error::Catalog(alloc::format!(
                "table `{to}` already exists"
            )));
        }
        let mut table = self.tables.remove(&old).expect("checked above");
        table.name = to.to_string();
        self.tables.insert(new.clone(), table);
        if let Some(constraints) = self.constraints.remove(&old) {
            self.constraints.insert(new.clone(), constraints);
        }
        for index in self.indexes.values_mut() {
            if index.table == old {
                index.table = new.clone();
            }
        }
        Ok(())
    }

    /// `ALTER TABLE ... ADD COLUMN`: append a column to a table.
    pub fn add_column(&mut self, table: &str, column: Column) -> Result<()> {
        let target = self.require_table_mut(table)?;
        if target.column(&column.name).is_some() {
            return Err(Error::Catalog(alloc::format!(
                "duplicate column name: {}",
                column.name
            )));
        }
        target.columns.push(column);
        Ok(())
    }

    /// `ALTER TABLE ... RENAME COLUMN`: rename a column and every constraint
    /// and index declaration that names it.
    ///
    /// `rewrite_check` is handed each `CHECK` expression so the caller — which
    /// owns the parser — can rewrite the references inside it. The catalog
    /// cannot do that itself without becoming a second SQL front end.
    pub fn rename_column(
        &mut self,
        table: &str,
        old: &str,
        new: &str,
        rewrite_check: impl Fn(&str) -> Result<String>,
    ) -> Result<()> {
        let key = table.to_ascii_lowercase();
        let target = self.require_table_mut(table)?;
        let (ordinal, _) = target.require_column(old)?;
        if !old.eq_ignore_ascii_case(new) && target.column(new).is_some() {
            return Err(Error::Catalog(alloc::format!(
                "duplicate column name: {new}"
            )));
        }
        target.columns[ordinal].name = new.to_string();

        if let Some(constraints) = self.constraints.get_mut(&key) {
            constraints.rename_column(old, new);
            for check in &mut constraints.checks {
                *check = rewrite_check(check)?;
            }
        }
        let lowered = old.to_ascii_lowercase();
        for index in self.indexes.values_mut() {
            if index.table != key {
                continue;
            }
            for column in index.columns.iter_mut() {
                if *column == lowered {
                    *column = new.to_ascii_lowercase();
                }
            }
        }
        Ok(())
    }

    /// `ALTER TABLE ... DROP COLUMN`: remove a column, refusing every case
    /// SQLite refuses.
    ///
    /// Returns the ordinal that was removed, so the caller can rewrite the
    /// stored rows. `check_mentions` answers whether a `CHECK` expression
    /// references the column — again the caller's job, because it needs the
    /// parser.
    pub fn drop_column(
        &mut self,
        table: &str,
        column: &str,
        check_mentions: impl Fn(&str, &str) -> Result<bool>,
    ) -> Result<usize> {
        let key = table.to_ascii_lowercase();
        let target = self.require_table(table)?;
        let (ordinal, definition) = target.require_column(column)?;
        let name = definition.name.clone();
        if target.columns.len() == 1 {
            return Err(Error::Catalog(alloc::format!(
                "cannot drop column `{name}`: a table must keep at least one column"
            )));
        }
        if definition.primary_key {
            return Err(Error::Catalog(alloc::format!(
                "cannot drop column `{name}`: it is the PRIMARY KEY"
            )));
        }
        if let Some(constraints) = self.constraints.get(&key) {
            if constraints.mentions(&name) {
                return Err(Error::Catalog(alloc::format!(
                    "cannot drop column `{name}`: a UNIQUE or FOREIGN KEY constraint names it"
                )));
            }
            for check in &constraints.checks {
                if check_mentions(check, &name)? {
                    return Err(Error::Catalog(alloc::format!(
                        "cannot drop column `{name}`: a CHECK constraint names it"
                    )));
                }
            }
        }
        let lowered = name.to_ascii_lowercase();
        if self
            .indexes
            .values()
            .any(|index| index.table == key && index.columns.contains(&lowered))
        {
            return Err(Error::Catalog(alloc::format!(
                "cannot drop column `{name}`: it is indexed"
            )));
        }

        self.require_table_mut(table)?.columns.remove(ordinal);
        Ok(ordinal)
    }

    fn require_table_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(&name.to_ascii_lowercase())
            .ok_or_else(|| Error::Catalog(alloc::format!("no such table: {name}")))
    }

    /// Look a table up by name, case-insensitively.
    ///
    /// The map is keyed by the lowercased name, so a name that is already
    /// lowercase is a key already and is looked up as it stands. That case is
    /// the hot one — [`Statement::check_schema`](crate::Statement::check_schema)
    /// runs this on every execution of every prepared statement, and the
    /// `String` the unconditional `to_ascii_lowercase` allocated was a
    /// measurable share of a point read. A name carrying an uppercase byte
    /// still takes the allocating path, and gets the identical answer.
    pub fn table(&self, name: &str) -> Option<&Table> {
        if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return self.tables.get(&name.to_ascii_lowercase());
        }
        self.tables.get(name)
    }

    /// Look a table up by name, or fail with a catalog error.
    pub fn require_table(&self, name: &str) -> Result<&Table> {
        self.table(name)
            .ok_or_else(|| Error::Catalog(alloc::format!("no such table: {name}")))
    }

    /// Every table, ordered by lowercased name.
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.values()
    }

    /// Every index declaration, ordered by lowercased name.
    pub fn indexes(&self) -> impl Iterator<Item = &Index> {
        self.indexes.values()
    }

    /// The index declarations for one table, in name order.
    pub fn indexes_for(&self, table: &str) -> Vec<&Index> {
        self.indexes
            .values()
            .filter(|index| index.table.eq_ignore_ascii_case(table))
            .collect()
    }

    /// Register an index declaration. Fails if the name is taken, the table or
    /// column does not exist, or the column type cannot carry the index kind.
    pub fn create_index(&mut self, index: Index) -> Result<()> {
        let key = index.name.to_ascii_lowercase();
        // A named `UNIQUE` constraint and the B-tree index that enforces it are
        // one object under one name — that is what `CREATE UNIQUE INDEX`
        // creates and what `DROP INDEX` removes. Any other reuse of the name is
        // a collision.
        let is_own_constraint = index.unique
            && index.kind == IndexKind::BTree
            && self
                .unique_constraint(&index.name)
                .is_some_and(|table| table.eq_ignore_ascii_case(&index.table));
        if self.indexes.contains_key(&key)
            || (!is_own_constraint && self.unique_constraint(&index.name).is_some())
        {
            return Err(Error::Catalog(alloc::format!(
                "index `{}` already exists",
                index.name
            )));
        }
        if index.columns.is_empty() {
            return Err(Error::Catalog(alloc::format!(
                "index `{}` names no columns",
                index.name
            )));
        }
        // The collation list is part of the key format, so a declaration whose
        // two lists disagree would encode entries under collations nobody
        // chose. Refuse it here rather than let `Index::collation` paper over
        // it with a default.
        if index.collations.len() != index.columns.len() {
            return Err(Error::Catalog(alloc::format!(
                "index `{}` declares {} column(s) and {} collation(s)",
                index.name,
                index.columns.len(),
                index.collations.len()
            )));
        }
        // At most one index per column list *per kind, per collation*. A `TEXT`
        // column can carry both a full-text index and a B-tree index — they
        // answer different questions — and it can carry a `BINARY` B-tree index
        // beside a `NOCASE` one, for the same reason: neither can answer the
        // other's comparison. What is refused is a second index that would key
        // the identical bytes under another name.
        //
        // Two *vector* indexes over one column under two metrics are refused
        // by this same rule, and deliberately: unlike a B-tree index, where
        // the planner picks by the comparison's collation, `vector_score(col,
        // ?)` names only the column, so a second graph over it would be one
        // nobody could ask for and one the backend map — keyed by
        // `(table, columns)` — could not hold beside the first anyway.
        if let Some(existing) = self.indexes.values().find(|existing| {
            existing.kind == index.kind
                && existing.table.eq_ignore_ascii_case(&index.table)
                && existing.covers_collated(&index.columns, &index.collations)
        }) {
            if index.kind == IndexKind::Vector && existing.metric != index.metric {
                return Err(Error::Catalog(alloc::format!(
                    "`{}`.`{}` already has a {} vector index (`{}`); one column carries one \
                     vector index, because `vector_score` names the column and not the metric \
                     and could not say which of two it meant. Drop `{}` first",
                    index.table,
                    index.column(),
                    existing.metric.ops_name(),
                    existing.name,
                    existing.name
                )));
            }
            return Err(Error::Catalog(alloc::format!(
                "`{}` already has an index of that kind on ({}) under {}",
                index.table,
                index.columns.join(", "),
                crate::collation::describe(&index.collations)
            )));
        }
        let table = self.require_table(&index.table)?;
        // A vector index covers exactly one column: two `VECTOR` columns are
        // generally two different embedding spaces, and there is no
        // defensible meaning for one ANN graph over both — unlike
        // `FullText`, which has an obvious one (see `IndexKind::FullText`'s
        // docs) and so is not restricted here.
        if index.kind == IndexKind::Vector && index.columns.len() > 1 {
            return Err(Error::Unsupported(String::from(
                "a vector index covers exactly one column",
            )));
        }
        if index.unique && index.kind != IndexKind::BTree {
            return Err(Error::Unsupported(String::from(
                "only a B-tree index can be UNIQUE; a retrieval index is not a constraint",
            )));
        }
        // Only a vector index has a distance to be built under. Recording one
        // on any other kind would put a number in the catalog that nothing
        // reads and that a later reader could mistake for a promise.
        if index.kind != IndexKind::Vector && index.metric != VectorMetric::Cosine {
            return Err(Error::Unsupported(alloc::format!(
                "`{}` is not a vector index, so `{}` has nothing to apply to",
                index.name,
                index.metric.ops_name()
            )));
        }
        for name in &index.columns {
            let (_, column) = table.require_column(name)?;
            match (index.kind, column.ty) {
                (IndexKind::FullText, DataType::Text) => {}
                (IndexKind::Vector, DataType::Vector(_) | DataType::QuantizedVector(_)) => {}
                // Every SQLite affinity is orderable; a vector is not, and
                // there is no total order to give it.
                (
                    IndexKind::BTree,
                    DataType::Integer
                    | DataType::Real
                    | DataType::Text
                    | DataType::Blob
                    | DataType::Numeric,
                ) => {}
                (IndexKind::FullText, other) => {
                    return Err(Error::Type(alloc::format!(
                        "a full-text index needs a TEXT column, but `{}` is {other}",
                        column.name
                    )))
                }
                (IndexKind::Vector, other) => {
                    return Err(Error::Type(alloc::format!(
                        "a vector index needs a VECTOR column, but `{}` is {other}",
                        column.name
                    )))
                }
                (IndexKind::BTree, other) => {
                    return Err(Error::Type(alloc::format!(
                        "a B-tree index needs an orderable column, but `{}` is {other}",
                        column.name
                    )))
                }
            }
        }
        self.indexes.insert(key, index);
        Ok(())
    }

    /// The B-tree index declared on exactly `columns` of `table` **under
    /// exactly `collations`**, if one is.
    ///
    /// This is how `UNIQUE` enforcement finds the index that can answer it
    /// with a probe instead of a scan. The collations have to match for the
    /// same reason the planner's do: a `UNIQUE` group's keys are the columns'
    /// declared collations, and an index keyed under any other would probe
    /// bytes that mean something else and miss the duplicate. A mismatch is
    /// not wrong here, only slower — the caller falls back to the scan, and
    /// `unique_key_collides` decides either way.
    pub fn btree_index_on(
        &self,
        table: &str,
        columns: &[String],
        collations: &[Collation],
    ) -> Option<&Index> {
        self.indexes.values().find(|index| {
            index.kind == IndexKind::BTree
                && index.table.eq_ignore_ascii_case(table)
                && index.covers_collated(columns, collations)
        })
    }

    /// Remove an index declaration by name, returning it. Fails if there is no
    /// such index.
    pub fn drop_index(&mut self, name: &str) -> Result<Index> {
        self.indexes
            .remove(&name.to_ascii_lowercase())
            .ok_or_else(|| Error::Catalog(alloc::format!("no such index: {name}")))
    }

    /// Register a named `UNIQUE` constraint from a `CREATE UNIQUE INDEX`.
    ///
    /// The name lives in the same space as the retrieval index declarations,
    /// because in SQLite it is the same space: `DROP INDEX` takes either.
    pub fn create_unique_constraint(
        &mut self,
        table: &str,
        constraint: UniqueConstraint,
    ) -> Result<()> {
        let key = table.to_ascii_lowercase();
        let definition = self.require_table(table)?;
        for column in &constraint.columns {
            definition.require_column(column)?;
        }
        if let Some(name) = &constraint.name {
            if self.indexes.contains_key(&name.to_ascii_lowercase())
                || self.unique_constraint(name).is_some()
            {
                return Err(Error::Catalog(alloc::format!(
                    "index `{name}` already exists"
                )));
            }
        }
        self.constraints
            .entry(key)
            .or_default()
            .unique
            .push(constraint);
        Ok(())
    }

    /// The table a named `UNIQUE` constraint belongs to, if one has that name.
    pub fn unique_constraint(&self, name: &str) -> Option<&str> {
        self.constraints.iter().find_map(|(table, constraints)| {
            constraints
                .unique
                .iter()
                .any(|group| {
                    group
                        .name
                        .as_deref()
                        .is_some_and(|declared| declared.eq_ignore_ascii_case(name))
                })
                .then_some(table.as_str())
        })
    }

    /// Remove a named `UNIQUE` constraint, returning whether one went.
    pub fn drop_unique_constraint(&mut self, name: &str) -> bool {
        for constraints in self.constraints.values_mut() {
            let before = constraints.unique.len();
            constraints.unique.retain(|group| {
                !group
                    .name
                    .as_deref()
                    .is_some_and(|declared| declared.eq_ignore_ascii_case(name))
            });
            if constraints.unique.len() != before {
                return true;
            }
        }
        false
    }

    /// The lowest format version that can express this catalog.
    ///
    /// Writing the lowest one that works is what keeps an ordinary database
    /// readable by the build that made it: a file only becomes version 4 when
    /// a table actually declares a constraint or uses an affinity older
    /// versions cannot name.
    fn required_version(&self) -> u32 {
        // A `WITHOUT ROWID` table forces version 9 for the same reason every
        // other tag-introducing version does: an older build would decode
        // it as an ordinary rowid table and offer a row id that does not
        // exist.
        if self.tables.values().any(|table| table.without_rowid) {
            return CATALOG_VERSION_WITHOUT_ROWID;
        }
        // A `STRICT` table, or an `ANY` column (only reachable inside one),
        // forces version 8 for the same reason every other tag-introducing
        // version does: an older build would decode a tag it never learned,
        // or read `strict` past a byte that is not there.
        if self
            .tables
            .values()
            .any(|table| table.strict || table.columns.iter().any(|c| c.ty == DataType::Any))
        {
            return CATALOG_VERSION_STRICT;
        }
        // A non-cosine vector index forces version 7 for the reason the module
        // docs give: an older build would read the declaration, miss the
        // metric, and rebuild the graph as cosine over vectors L2 declared
        // unnormalised. Refusing to open is the only answer that cannot
        // silently answer with the wrong neighbours.
        if self
            .indexes
            .values()
            .any(|index| index.metric != VectorMetric::Cosine)
        {
            return CATALOG_VERSION_METRIC;
        }
        // A declared collation forces version 6 for the reason the module docs
        // give: an older build would read the declaration, miss the collation,
        // and probe a `NOCASE` index with an unfolded key. Refusing to open is
        // the only answer that cannot silently lose rows.
        let uses_collations = self
            .tables
            .values()
            .any(|table| table.columns.iter().any(|c| !c.collation.is_binary()))
            || self
                .indexes
                .values()
                .any(|index| index.collations.iter().any(|c| !c.is_binary()));
        if uses_collations {
            return CATALOG_VERSION_COLLATION;
        }
        // A B-tree index cannot be expressed at all before version 5: the
        // index section had one column and no kind tag for it. It must force
        // the bump, because an older build that decoded the section anyway
        // would open a database whose tree is full of index entries it does
        // not know to maintain — the one failure mode worse than refusing.
        if self
            .indexes
            .values()
            .any(|index| index.kind == IndexKind::BTree || index.unique || index.columns.len() != 1)
        {
            return CATALOG_VERSION_BTREE;
        }
        let uses_constraints = self.constraints.values().any(|c| !c.is_empty())
            || self.tables.values().any(|table| {
                table
                    .columns
                    .iter()
                    .any(|column| column.not_null || column.default.is_some())
            });
        let uses_numeric = self
            .tables
            .values()
            .any(|table| table.columns.iter().any(|c| c.ty == DataType::Numeric));
        if uses_constraints || uses_numeric {
            return CATALOG_VERSION_CONSTRAINTS;
        }
        if self.tables.values().any(|table| {
            table
                .columns
                .iter()
                .any(|column| column.ty.is_quantized_vector())
        }) {
            return CATALOG_VERSION_QUANTIZED;
        }
        CATALOG_VERSION_EXACT
    }

    /// Serialise the catalog for storage, in the lowest version that fits it.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(CATALOG_MAGIC);
        let version = self.required_version();
        out.extend_from_slice(&version.to_le_bytes());
        put_len(&mut out, self.tables.len());
        for (key, table) in &self.tables {
            put_string(&mut out, &table.name);
            if version >= CATALOG_VERSION_STRICT {
                out.push(u8::from(table.strict));
            }
            if version >= CATALOG_VERSION_WITHOUT_ROWID {
                out.push(u8::from(table.without_rowid));
                put_len(&mut out, table.primary_key.len());
                for column in &table.primary_key {
                    put_string(&mut out, column);
                }
            }
            put_len(&mut out, table.columns.len());
            for column in &table.columns {
                put_string(&mut out, &column.name);
                let flags = if column.primary_key {
                    PRIMARY_KEY_FLAG
                } else {
                    0
                };
                match column.ty {
                    DataType::Integer => out.push(TYPE_INTEGER | flags),
                    DataType::Real => out.push(TYPE_REAL | flags),
                    DataType::Text => out.push(TYPE_TEXT | flags),
                    DataType::Blob => out.push(TYPE_BLOB | flags),
                    DataType::Numeric => out.push(TYPE_NUMERIC | flags),
                    DataType::Vector(dim) => {
                        out.push(TYPE_VECTOR | flags);
                        put_len(&mut out, dim);
                    }
                    DataType::QuantizedVector(dim) => {
                        out.push(TYPE_VECTOR_Q8 | flags);
                        put_len(&mut out, dim);
                    }
                    DataType::Any => out.push(TYPE_ANY | flags),
                }
                if version >= CATALOG_VERSION_CONSTRAINTS {
                    out.push(u8::from(column.not_null));
                    put_option_string(&mut out, column.default.as_deref());
                }
                if version >= CATALOG_VERSION_COLLATION {
                    out.push(column.collation.tag());
                }
            }
            if version >= CATALOG_VERSION_CONSTRAINTS {
                let empty = TableConstraints::default();
                let constraints = self.constraints.get(key).unwrap_or(&empty);
                put_len(&mut out, constraints.unique.len());
                for group in &constraints.unique {
                    put_option_string(&mut out, group.name.as_deref());
                    put_len(&mut out, group.columns.len());
                    for column in &group.columns {
                        put_string(&mut out, column);
                    }
                }
                put_len(&mut out, constraints.checks.len());
                for check in &constraints.checks {
                    put_string(&mut out, check);
                }
                put_len(&mut out, constraints.foreign_keys.len());
                for key in &constraints.foreign_keys {
                    put_len(&mut out, key.columns.len());
                    for column in &key.columns {
                        put_string(&mut out, column);
                    }
                    put_string(&mut out, &key.table);
                    put_len(&mut out, key.referenced.len());
                    for column in &key.referenced {
                        put_string(&mut out, column);
                    }
                    put_option_string(&mut out, key.on_delete.as_deref());
                    put_option_string(&mut out, key.on_update.as_deref());
                }
            }
        }
        put_len(&mut out, self.indexes.len());
        for index in self.indexes.values() {
            put_string(&mut out, &index.name);
            put_string(&mut out, &index.table);
            if version >= CATALOG_VERSION_BTREE {
                // The column list and the `UNIQUE` flag are what version 5
                // added; before it there was exactly one column and no flag,
                // and `required_version` guarantees we never get here with an
                // index the older layout cannot hold.
                put_len(&mut out, index.columns.len());
                for column in &index.columns {
                    put_string(&mut out, column);
                }
            } else {
                put_string(&mut out, index.column());
            }
            out.push(match index.kind {
                IndexKind::FullText => INDEX_FULL_TEXT,
                IndexKind::Vector => INDEX_VECTOR,
                IndexKind::BTree => INDEX_BTREE,
            });
            if version >= CATALOG_VERSION_BTREE {
                out.push(u8::from(index.unique));
            }
            if version >= CATALOG_VERSION_COLLATION {
                // One tag per column, written from the column list's length
                // rather than the collation list's: they are the same length
                // for anything this build creates, and reading the columns
                // keeps a short list (an index decoded from version 5) from
                // encoding as a short one and shifting everything after it.
                for position in 0..index.columns.len() {
                    out.push(index.collation(position).tag());
                }
            }
            if version >= CATALOG_VERSION_METRIC {
                // Last, after the collation tags, so a version-6 index section
                // is byte for byte what it was — the same reason the paged
                // graph header appends rather than prepends its tags.
                out.push(index.metric.tag());
            }
        }
        out
    }

    /// Parse a catalog previously produced by [`Catalog::encode`].
    ///
    /// A version-1 catalog (no magic prefix) is decoded and grandfathered: its
    /// indexable columns become implicit index declarations, preserving the
    /// automatic-indexing behaviour they were built under.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.starts_with(CATALOG_MAGIC) {
            Self::decode_versioned(bytes)
        } else {
            let mut catalog = Self::decode_v1(bytes)?;
            catalog.grandfather();
            Ok(catalog)
        }
    }

    /// Decode the versioned (version 2, 3 or 4) format.
    fn decode_versioned(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        cursor.take(CATALOG_MAGIC.len())?;
        let version = cursor.u32()?;
        if !matches!(
            version,
            CATALOG_VERSION_EXACT
                | CATALOG_VERSION_QUANTIZED
                | CATALOG_VERSION_CONSTRAINTS
                | CATALOG_VERSION_BTREE
                | CATALOG_VERSION_COLLATION
                | CATALOG_VERSION_METRIC
                | CATALOG_VERSION_STRICT
                | CATALOG_VERSION_WITHOUT_ROWID
        ) {
            return Err(Error::FormatVersion(alloc::format!(
                "catalog format version {version} is not supported (this build supports \
                 {CATALOG_VERSION_EXACT} through {CATALOG_VERSION_WITHOUT_ROWID}); pre-1.0 the \
                 policy is to recreate the database, not to migrate it"
            )));
        }
        let mut catalog = decode_tables(&mut cursor, version)?;
        let index_count = cursor.count(4)?;
        for _ in 0..index_count {
            let name = cursor.string()?;
            let table = cursor.string()?.to_ascii_lowercase();
            let columns = if version >= CATALOG_VERSION_BTREE {
                let count = cursor.count(4)?;
                let mut columns = Vec::with_capacity(count);
                for _ in 0..count {
                    columns.push(cursor.string()?.to_ascii_lowercase());
                }
                columns
            } else {
                alloc::vec![cursor.string()?.to_ascii_lowercase()]
            };
            let kind = match cursor.u8()? {
                INDEX_FULL_TEXT => IndexKind::FullText,
                INDEX_VECTOR => IndexKind::Vector,
                // Only reachable at version 5: an older catalog cannot carry
                // the tag, and the version check above already refused a
                // newer one.
                INDEX_BTREE if version >= CATALOG_VERSION_BTREE => IndexKind::BTree,
                other => {
                    return Err(Error::Corrupt(alloc::format!(
                        "unknown index kind tag {other}"
                    )))
                }
            };
            let unique = version >= CATALOG_VERSION_BTREE && cursor.u8()? != 0;
            if columns.is_empty() {
                return Err(Error::Corrupt(alloc::format!(
                    "index `{name}` names no columns"
                )));
            }
            let mut collations = Vec::with_capacity(columns.len());
            for _ in 0..columns.len() {
                collations.push(if version >= CATALOG_VERSION_COLLATION {
                    Collation::from_tag(cursor.u8()?)?
                } else {
                    Collation::Binary
                });
            }
            // Absent before version 7, and every index those builds could
            // write is cosine.
            let metric = if version >= CATALOG_VERSION_METRIC {
                VectorMetric::from_tag(cursor.u8()?)?
            } else {
                VectorMetric::Cosine
            };
            catalog.indexes.insert(
                name.to_ascii_lowercase(),
                Index {
                    name,
                    table,
                    columns,
                    kind,
                    unique,
                    collations,
                    metric,
                },
            );
        }
        Ok(catalog)
    }

    /// Decode the legacy (version 1) format: tables only, no index section.
    fn decode_v1(bytes: &[u8]) -> Result<Self> {
        decode_tables(&mut Cursor::new(bytes), 1)
    }

    /// Give every indexable column an implicit index declaration, as the
    /// automatic-indexing engine did before `CREATE INDEX` existed.
    fn grandfather(&mut self) {
        let mut implicit = Vec::new();
        for table in self.tables.values() {
            for column in &table.columns {
                let kind = match column.ty {
                    DataType::Text => Some(IndexKind::FullText),
                    DataType::Vector(_) | DataType::QuantizedVector(_) => Some(IndexKind::Vector),
                    _ => None,
                };
                if let Some(kind) = kind {
                    implicit.push(Index::single(
                        auto_index_name(
                            &table.name.to_ascii_lowercase(),
                            &column.name.to_ascii_lowercase(),
                        ),
                        table.name.to_ascii_lowercase(),
                        column.name.to_ascii_lowercase(),
                        kind,
                    ));
                }
            }
        }
        for index in implicit {
            self.indexes.insert(index.name.to_ascii_lowercase(), index);
        }
    }
}

/// Decode the table section shared by every catalog format.
fn decode_tables(cursor: &mut Cursor<'_>, version: u32) -> Result<Catalog> {
    let table_count = cursor.count(8)?;
    let mut catalog = Catalog::new();
    for _ in 0..table_count {
        let name = cursor.string()?;
        let strict = version >= CATALOG_VERSION_STRICT && cursor.u8()? != 0;
        let (without_rowid, primary_key) = if version >= CATALOG_VERSION_WITHOUT_ROWID {
            let without_rowid = cursor.u8()? != 0;
            let count = cursor.count(4)?;
            let mut primary_key = Vec::with_capacity(count);
            for _ in 0..count {
                primary_key.push(cursor.string()?);
            }
            (without_rowid, primary_key)
        } else {
            (false, Vec::new())
        };
        let column_count = cursor.count(5)?;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            let column_name = cursor.string()?;
            let tag = cursor.u8()?;
            let primary_key = tag & PRIMARY_KEY_FLAG != 0;
            let ty = match tag & !PRIMARY_KEY_FLAG {
                TYPE_INTEGER => DataType::Integer,
                TYPE_REAL => DataType::Real,
                TYPE_TEXT => DataType::Text,
                TYPE_BLOB => DataType::Blob,
                TYPE_NUMERIC if version >= CATALOG_VERSION_CONSTRAINTS => DataType::Numeric,
                TYPE_VECTOR => DataType::Vector(cursor.u32()? as usize),
                TYPE_VECTOR_Q8 if version >= CATALOG_VERSION_QUANTIZED => {
                    DataType::QuantizedVector(cursor.u32()? as usize)
                }
                TYPE_ANY if version >= CATALOG_VERSION_STRICT => DataType::Any,
                other => {
                    return Err(Error::Corrupt(alloc::format!(
                        "unknown catalog type tag {other}"
                    )))
                }
            };
            let (not_null, default) = if version >= CATALOG_VERSION_CONSTRAINTS {
                (cursor.u8()? != 0, take_option_string(cursor)?)
            } else {
                (false, None)
            };
            let collation = if version >= CATALOG_VERSION_COLLATION {
                Collation::from_tag(cursor.u8()?)?
            } else {
                Collation::Binary
            };
            columns.push(Column {
                name: column_name,
                ty,
                primary_key,
                not_null,
                default,
                collation,
            });
        }
        let constraints = if version >= CATALOG_VERSION_CONSTRAINTS {
            decode_constraints(cursor)?
        } else {
            TableConstraints::default()
        };
        catalog.create_table_with(
            Table {
                name,
                columns,
                strict,
                without_rowid,
                primary_key,
            },
            constraints,
        )?;
    }
    Ok(catalog)
}

/// Decode one table's version-4 constraint section.
///
/// The `count` bounds are the smallest an element can encode to, which is what
/// lets a corrupt length be rejected before anything is allocated for it — see
/// [`Cursor::count`]. They are the *empty* element in each case: a `UNIQUE`
/// with no name and no columns is 1 + 4 bytes, and a `FOREIGN KEY` with no
/// columns and an empty table name is 4 + 4 + 4 + 1 + 1.
fn decode_constraints(cursor: &mut Cursor<'_>) -> Result<TableConstraints> {
    let mut constraints = TableConstraints::default();
    let unique_count = cursor.count(5)?;
    for _ in 0..unique_count {
        let name = take_option_string(cursor)?;
        let column_count = cursor.count(4)?;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            columns.push(cursor.string()?);
        }
        constraints.unique.push(UniqueConstraint { name, columns });
    }
    let check_count = cursor.count(4)?;
    for _ in 0..check_count {
        constraints.checks.push(cursor.string()?);
    }
    let key_count = cursor.count(14)?;
    for _ in 0..key_count {
        let column_count = cursor.count(4)?;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            columns.push(cursor.string()?);
        }
        let table = cursor.string()?;
        let referenced_count = cursor.count(4)?;
        let mut referenced = Vec::with_capacity(referenced_count);
        for _ in 0..referenced_count {
            referenced.push(cursor.string()?);
        }
        constraints.foreign_keys.push(ForeignKey {
            columns,
            table,
            referenced,
            on_delete: take_option_string(cursor)?,
            on_update: take_option_string(cursor)?,
        });
    }
    Ok(constraints)
}

/// Append a present flag and, when present, the string.
fn put_option_string(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            out.push(1);
            put_string(out, value);
        }
        None => out.push(0),
    }
}

fn take_option_string(cursor: &mut Cursor<'_>) -> Result<Option<String>> {
    match cursor.u8()? {
        0 => Ok(None),
        1 => Ok(Some(cursor.string()?)),
        other => Err(Error::Corrupt(alloc::format!(
            "expected 0 or 1 for an optional string, got {other}"
        ))),
    }
}

impl Column {
    /// Convenience constructor for an ordinary column: nullable, no default,
    /// `BINARY` collation.
    pub fn new(name: &str, ty: DataType) -> Self {
        Self {
            name: name.to_string(),
            ty,
            primary_key: false,
            not_null: false,
            default: None,
            collation: Collation::Binary,
        }
    }

    /// Convenience constructor for a column declared `PRIMARY KEY`.
    pub fn primary_key(name: &str, ty: DataType) -> Self {
        Self {
            primary_key: true,
            ..Self::new(name, ty)
        }
    }

    /// The same column, declared `NOT NULL`.
    pub fn not_null(mut self) -> Self {
        self.not_null = true;
        self
    }

    /// The same column, with `DEFAULT <expr>` as written.
    pub fn with_default(mut self, expr: &str) -> Self {
        self.default = Some(expr.to_string());
        self
    }

    /// The same column, declared `COLLATE <collation>`.
    pub fn with_collation(mut self, collation: Collation) -> Self {
        self.collation = collation;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sample() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "docs".to_string(),
                columns: vec![
                    Column::primary_key("id", DataType::Integer),
                    Column::new("body", DataType::Text),
                    Column::new("embedding", DataType::Vector(384)),
                ],
                strict: false,
            })
            .unwrap();
        catalog
    }

    /// `Catalog::table` takes a non-allocating path for an already-lowercase
    /// name and the allocating one otherwise. Both branches must answer the
    /// same question, including for a name that is not ASCII at all.
    #[test]
    fn a_table_is_found_whatever_the_case_of_the_name_asked_for() {
        let mut catalog = sample();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "MiXeD".to_string(),
                columns: vec![Column::new("a", DataType::Integer)],
                strict: false,
            })
            .unwrap();

        for name in ["docs", "DOCS", "Docs", "dOcS"] {
            assert_eq!(catalog.table(name).map(|t| t.name.as_str()), Some("docs"));
            assert!(catalog.require_table(name).is_ok());
        }
        // Registered under a mixed-case name: still keyed by the lowercase.
        for name in ["MiXeD", "mixed", "MIXED"] {
            assert_eq!(catalog.table(name).map(|t| t.name.as_str()), Some("MiXeD"));
        }
        assert!(catalog.table("nosuch").is_none());
        assert!(catalog.table("NOSUCH").is_none());
        assert!(catalog.require_table("NOSUCH").is_err());
    }

    #[test]
    fn the_primary_key_survives_encoding() {
        let decoded = Catalog::decode(&sample().encode()).unwrap();
        let table = decoded.table("docs").unwrap();
        assert_eq!(table.rowid_alias(), Some(0));
        assert!(!table.columns[1].primary_key);
    }

    #[test]
    fn a_table_without_a_primary_key_has_no_rowid_alias() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "plain".to_string(),
                columns: vec![Column::new("a", DataType::Integer)],
                strict: false,
            })
            .unwrap();
        assert_eq!(catalog.table("plain").unwrap().rowid_alias(), None);
    }

    #[test]
    fn round_trips() {
        let catalog = sample();
        let decoded = Catalog::decode(&catalog.encode()).unwrap();
        assert_eq!(decoded, catalog);
    }

    #[test]
    fn lookups_ignore_case() {
        let catalog = sample();
        let table = catalog.table("DOCS").expect("table");
        assert_eq!(table.column("Embedding").unwrap().0, 2);
    }

    #[test]
    fn duplicate_table_is_rejected() {
        let mut catalog = sample();
        let err = catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "DOCS".to_string(),
                columns: vec![],
                strict: false,
            })
            .unwrap_err();
        assert!(matches!(err, Error::Catalog(_)));
    }

    /// The byte layout a version-1 binary wrote: tables only, no magic, no
    /// index section. Reconstructing it here keeps the grandfathering test
    /// honest without depending on an old build.
    fn encode_v1(catalog: &Catalog) -> Vec<u8> {
        let mut out = Vec::new();
        put_len(&mut out, catalog.tables.len());
        for table in catalog.tables.values() {
            put_string(&mut out, &table.name);
            put_len(&mut out, table.columns.len());
            for column in &table.columns {
                put_string(&mut out, &column.name);
                let flags = if column.primary_key {
                    PRIMARY_KEY_FLAG
                } else {
                    0
                };
                match column.ty {
                    DataType::Integer => out.push(TYPE_INTEGER | flags),
                    DataType::Real => out.push(TYPE_REAL | flags),
                    DataType::Text => out.push(TYPE_TEXT | flags),
                    DataType::Blob => out.push(TYPE_BLOB | flags),
                    DataType::Vector(dim) => {
                        out.push(TYPE_VECTOR | flags);
                        put_len(&mut out, dim);
                    }
                    DataType::Numeric | DataType::QuantizedVector(_) | DataType::Any => {
                        panic!(
                            "version-1 fixtures cannot contain NUMERIC, ANY or quantized vectors"
                        )
                    }
                }
            }
        }
        out
    }

    #[test]
    fn create_index_round_trips() {
        let mut catalog = sample();
        catalog
            .create_index(Index {
                name: "docs_body".to_string(),
                table: "docs".to_string(),
                columns: alloc::vec!["body".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: alloc::vec![Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap();
        let decoded = Catalog::decode(&catalog.encode()).unwrap();
        assert_eq!(decoded, catalog);
        assert_eq!(decoded.indexes_for("docs").len(), 1);
    }

    #[test]
    fn create_index_validates_its_column_type() {
        let mut catalog = sample();
        let err = catalog
            .create_index(Index {
                name: "bad".to_string(),
                table: "docs".to_string(),
                columns: alloc::vec!["body".to_string()],
                kind: IndexKind::Vector,
                unique: false,
                collations: alloc::vec![Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap_err();
        assert!(matches!(err, Error::Type(_)), "got {err}");

        let err = catalog
            .create_index(Index {
                name: "bad".to_string(),
                table: "docs".to_string(),
                columns: alloc::vec!["embedding".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: alloc::vec![Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap_err();
        assert!(matches!(err, Error::Type(_)), "got {err}");
    }

    /// A table with two `TEXT` columns, for the multi-column `FullText`
    /// tests below — `sample()` only has one.
    fn sample_with_two_text_columns() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "docs".to_string(),
                columns: vec![
                    Column::primary_key("id", DataType::Integer),
                    Column::new("title", DataType::Text),
                    Column::new("body", DataType::Text),
                ],
                strict: false,
            })
            .unwrap();
        catalog
    }

    #[test]
    fn a_multi_column_full_text_index_is_accepted() {
        let mut catalog = sample_with_two_text_columns();
        catalog
            .create_index(Index {
                name: "docs_search".to_string(),
                table: "docs".to_string(),
                columns: vec!["title".to_string(), "body".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: vec![Collation::Binary, Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap();
        let index = catalog.indexes_for("docs")[0];
        assert_eq!(index.columns, vec!["title", "body"]);

        // Multi-column encoding is generic in `IndexKind`, not B-tree
        // specific (see `Catalog::required_version`), so this needed no
        // catalog format change: it round-trips like anything else.
        let decoded = Catalog::decode(&catalog.encode()).unwrap();
        assert_eq!(decoded, catalog);
    }

    #[test]
    fn a_column_can_be_named_by_a_single_and_a_multi_column_full_text_index_at_once() {
        // `(body)` and `(title, body)` answer different questions, so both
        // are allowed — the catalog's dup-check compares the whole column
        // list, not "is this column already indexed".
        let mut catalog = sample_with_two_text_columns();
        catalog
            .create_index(Index::single(
                "docs_body".to_string(),
                "docs".to_string(),
                "body".to_string(),
                IndexKind::FullText,
            ))
            .unwrap();
        catalog
            .create_index(Index {
                name: "docs_search".to_string(),
                table: "docs".to_string(),
                columns: vec!["title".to_string(), "body".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: vec![Collation::Binary, Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap();
        assert_eq!(catalog.indexes_for("docs").len(), 2);
    }

    #[test]
    fn a_multi_column_vector_index_is_still_refused() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "docs".to_string(),
                columns: vec![
                    Column::new("a", DataType::Vector(4)),
                    Column::new("b", DataType::Vector(4)),
                ],
                strict: false,
            })
            .unwrap();
        let err = catalog
            .create_index(Index {
                name: "bad".to_string(),
                table: "docs".to_string(),
                columns: vec!["a".to_string(), "b".to_string()],
                kind: IndexKind::Vector,
                unique: false,
                collations: vec![Collation::Binary, Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "got {err}");
    }

    #[test]
    fn duplicate_index_name_is_rejected() {
        let mut catalog = sample();
        catalog
            .create_index(Index {
                name: "idx".to_string(),
                table: "docs".to_string(),
                columns: alloc::vec!["body".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: alloc::vec![Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap();
        let err = catalog
            .create_index(Index {
                name: "IDX".to_string(),
                table: "docs".to_string(),
                columns: alloc::vec!["embedding".to_string()],
                kind: IndexKind::Vector,
                unique: false,
                collations: alloc::vec![Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap_err();
        assert!(matches!(err, Error::Catalog(_)), "got {err}");
    }

    #[test]
    fn drop_index_returns_the_declaration() {
        let mut catalog = sample();
        catalog
            .create_index(Index {
                name: "idx".to_string(),
                table: "docs".to_string(),
                columns: alloc::vec!["body".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: alloc::vec![Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap();
        let dropped = catalog.drop_index("IDX").unwrap();
        assert_eq!(dropped.column(), "body");
        assert!(catalog.indexes_for("docs").is_empty());
        assert!(matches!(
            catalog.drop_index("idx").unwrap_err(),
            Error::Catalog(_)
        ));
    }

    #[test]
    fn a_version_one_catalog_is_grandfathered() {
        let bytes = encode_v1(&sample());
        let decoded = Catalog::decode(&bytes).unwrap();

        // Both indexable columns keep their implicit indexes.
        let indexes = decoded.indexes_for("docs");
        assert_eq!(indexes.len(), 2);
        assert!(indexes
            .iter()
            .any(|i| i.column() == "body" && i.kind == IndexKind::FullText));
        assert!(indexes
            .iter()
            .any(|i| i.column() == "embedding" && i.kind == IndexKind::Vector));

        // And a version-1 catalog that has no indexable columns is unchanged.
        let mut plain = Catalog::new();
        plain
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "nums".to_string(),
                columns: vec![Column::new("n", DataType::Integer)],
                strict: false,
            })
            .unwrap();
        let decoded = Catalog::decode(&encode_v1(&plain)).unwrap();
        assert!(decoded.indexes().next().is_none());
    }

    #[test]
    fn a_future_catalog_version_is_refused() {
        let mut bytes = sample().encode();
        // Magic is b"ISQL"; the version follows at offset 4.
        bytes[4..8].copy_from_slice(&(CATALOG_VERSION_WITHOUT_ROWID + 1).to_le_bytes());
        let err = Catalog::decode(&bytes).unwrap_err();
        assert!(matches!(err, Error::FormatVersion(_)), "got {err}");
    }

    /// The version-4 additions round-trip, and — the part that matters for the
    /// recreate-not-migrate policy — a catalog that declares nothing new is
    /// still written at the version an older build can read.
    #[test]
    fn constraints_force_version_four_and_nothing_else_does() {
        let plain = sample().encode();
        assert_eq!(
            u32::from_le_bytes(plain[4..8].try_into().unwrap()),
            CATALOG_VERSION_EXACT,
            "a table with no constraints must stay readable by an older build"
        );

        let mut catalog = Catalog::new();
        catalog
            .create_table_with(
                Table {
                    without_rowid: false,
                    primary_key: Vec::new(),
                    name: "users".to_string(),
                    columns: vec![
                        Column::primary_key("id", DataType::Integer),
                        Column::new("email", DataType::Text).not_null(),
                        Column::new("age", DataType::Integer).with_default("18"),
                        Column::new("price", DataType::Numeric),
                    ],
                    strict: false,
                },
                TableConstraints {
                    unique: vec![UniqueConstraint::new(vec!["email".to_string()])],
                    checks: vec!["age > 0".to_string()],
                    foreign_keys: vec![ForeignKey {
                        columns: vec!["id".to_string()],
                        table: "other".to_string(),
                        referenced: vec!["id".to_string()],
                        on_delete: Some("CASCADE".to_string()),
                        on_update: None,
                    }],
                },
            )
            .unwrap();

        let bytes = catalog.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_CONSTRAINTS
        );
        let decoded = Catalog::decode(&bytes).unwrap();
        assert_eq!(decoded, catalog);
        assert_eq!(decoded.constraints("USERS").unwrap().checks, ["age > 0"]);
        assert!(decoded.table("users").unwrap().columns[1].not_null);
        assert_eq!(
            decoded.table("users").unwrap().columns[2]
                .default
                .as_deref(),
            Some("18")
        );
    }

    /// A `NUMERIC` column on its own is enough to need version 4, because no
    /// earlier version has a tag for it.
    #[test]
    fn a_numeric_column_alone_forces_version_four() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "t".to_string(),
                columns: vec![Column::new("n", DataType::Numeric)],
                strict: false,
            })
            .unwrap();
        let bytes = catalog.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_CONSTRAINTS
        );
        assert_eq!(Catalog::decode(&bytes).unwrap(), catalog);
    }

    /// A `STRICT` table forces version 8, because no earlier version has a
    /// byte for the flag at all.
    #[test]
    fn a_strict_table_forces_version_eight() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "t".to_string(),
                columns: vec![Column::new("a", DataType::Integer)],
                strict: true,
            })
            .unwrap();
        let bytes = catalog.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_STRICT
        );
        assert_eq!(Catalog::decode(&bytes).unwrap(), catalog);
    }

    /// An `ANY` column alone — inside a non-strict table, which cannot
    /// happen through SQL but is not this layer's job to refuse — also
    /// forces version 8, for the same reason as the flag: no earlier
    /// version has a tag for it.
    #[test]
    fn an_any_column_alone_forces_version_eight() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "t".to_string(),
                columns: vec![Column::new("a", DataType::Any)],
                strict: false,
            })
            .unwrap();
        let bytes = catalog.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_STRICT
        );
        assert_eq!(Catalog::decode(&bytes).unwrap(), catalog);
    }

    /// A plain table with no `STRICT` flag and no `ANY` column still writes
    /// the lowest version that fits it — `STRICT`'s byte must not leak into
    /// a database that never asked for it.
    #[test]
    fn a_table_with_neither_strict_nor_any_does_not_force_version_eight() {
        let catalog = sample();
        let bytes = catalog.encode();
        assert!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) < CATALOG_VERSION_STRICT);
    }

    /// A `WITHOUT ROWID` table forces version 9, because no earlier version
    /// has a byte for the flag — or for the primary key's own column list,
    /// which only means something once there is no row id to fall back to.
    #[test]
    fn a_without_rowid_table_forces_version_nine() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: true,
                primary_key: alloc::vec!["a".to_string()],
                name: "t".to_string(),
                columns: vec![Column::new("a", DataType::Integer).not_null()],
                strict: false,
            })
            .unwrap();
        let bytes = catalog.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_WITHOUT_ROWID
        );
        let decoded = Catalog::decode(&bytes).unwrap();
        assert_eq!(decoded, catalog);
        assert_eq!(decoded.table("t").unwrap().primary_key, ["a"]);
    }

    #[test]
    fn dropping_a_table_takes_its_indexes_and_constraints_with_it() {
        let mut catalog = sample();
        catalog
            .create_index(Index {
                name: "idx".to_string(),
                table: "docs".to_string(),
                columns: alloc::vec!["body".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: alloc::vec![Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap();
        let (table, indexes) = catalog.drop_table("DOCS").unwrap();
        assert_eq!(table.name, "docs");
        assert_eq!(indexes.len(), 1);
        assert!(catalog.table("docs").is_none());
        assert!(catalog.indexes().next().is_none());
        assert!(matches!(
            catalog.drop_table("docs").unwrap_err(),
            Error::Catalog(_)
        ));
    }

    #[test]
    fn renaming_a_table_moves_its_indexes() {
        let mut catalog = sample();
        catalog
            .create_index(Index {
                name: "idx".to_string(),
                table: "docs".to_string(),
                columns: alloc::vec!["body".to_string()],
                kind: IndexKind::FullText,
                unique: false,
                collations: alloc::vec![Collation::Binary],
                metric: VectorMetric::Cosine,
            })
            .unwrap();
        catalog.rename_table("docs", "Papers").unwrap();
        assert!(catalog.table("docs").is_none());
        assert_eq!(catalog.table("papers").unwrap().name, "Papers");
        assert_eq!(catalog.indexes_for("papers").len(), 1);
    }

    #[test]
    fn a_column_a_constraint_names_cannot_be_dropped() {
        let mut catalog = Catalog::new();
        catalog
            .create_table_with(
                Table {
                    without_rowid: false,
                    primary_key: Vec::new(),
                    name: "t".to_string(),
                    columns: vec![
                        Column::new("a", DataType::Integer),
                        Column::new("b", DataType::Integer),
                    ],
                    strict: false,
                },
                TableConstraints {
                    unique: vec![UniqueConstraint::new(vec!["a".to_string()])],
                    ..TableConstraints::default()
                },
            )
            .unwrap();
        assert!(catalog.drop_column("t", "a", |_, _| Ok(false)).is_err());
        assert_eq!(catalog.drop_column("t", "b", |_, _| Ok(false)).unwrap(), 1);
        // The last column standing cannot go either.
        assert!(catalog.drop_column("t", "a", |_, _| Ok(false)).is_err());
    }

    /// The version-6 addition round-trips, and — the half that matters for the
    /// recreate-not-migrate policy — a catalog that declares no collation is
    /// still written at the version an older build can read.
    #[test]
    fn collations_force_version_six_and_nothing_else_does() {
        let plain = sample().encode();
        assert_eq!(
            u32::from_le_bytes(plain[4..8].try_into().unwrap()),
            CATALOG_VERSION_EXACT,
            "a table with no declared collation must stay readable by an older build"
        );

        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "people".to_string(),
                columns: vec![
                    Column::primary_key("id", DataType::Integer),
                    Column::new("name", DataType::Text).with_collation(Collation::NoCase),
                    Column::new("code", DataType::Text).with_collation(Collation::RTrim),
                    Column::new("plain", DataType::Text),
                ],
                strict: false,
            })
            .unwrap();
        catalog
            .create_index(Index {
                name: "people_name".to_string(),
                table: "people".to_string(),
                columns: vec!["name".to_string()],
                kind: IndexKind::BTree,
                unique: false,
                collations: vec![Collation::NoCase],
                metric: VectorMetric::Cosine,
            })
            .unwrap();

        let bytes = catalog.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_COLLATION
        );
        let decoded = Catalog::decode(&bytes).unwrap();
        assert_eq!(decoded, catalog);
        let table = decoded.table("people").unwrap();
        assert_eq!(table.columns[1].collation, Collation::NoCase);
        assert_eq!(table.columns[2].collation, Collation::RTrim);
        assert_eq!(table.columns[3].collation, Collation::Binary);
        assert_eq!(
            decoded.indexes().next().unwrap().collation(0),
            Collation::NoCase
        );
    }

    /// An index's collation alone forces the bump, even when every column
    /// declared nothing: a `NOCASE` index keys folded bytes, and a build that
    /// read the declaration without the collation would probe the wrong ones.
    #[test]
    fn an_index_collation_alone_forces_version_six() {
        let mut catalog = sample();
        catalog
            .create_index(Index {
                name: "docs_body_nc".to_string(),
                table: "docs".to_string(),
                columns: vec!["body".to_string()],
                kind: IndexKind::BTree,
                unique: false,
                collations: vec![Collation::NoCase],
                metric: VectorMetric::Cosine,
            })
            .unwrap();
        let bytes = catalog.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_COLLATION
        );
        assert_eq!(Catalog::decode(&bytes).unwrap(), catalog);
    }

    /// A vector index's metric forces version 7, and — the recreate-not-migrate
    /// half — a cosine one forces nothing, so every database that exists today
    /// still encodes at the version it always did.
    ///
    /// The bump has to happen for the same reason the collation one does. An
    /// older build would read the declaration, miss the metric, and rebuild
    /// the graph as cosine over vectors L2 declared unnormalised — answering
    /// `vector_score` with the wrong neighbours, with no error anywhere,
    /// because both metrics are defined on the same embeddings.
    #[test]
    fn a_vector_metric_forces_version_seven_and_cosine_forces_nothing() {
        let mut cosine = sample();
        cosine
            .create_index(Index::single(
                "docs_embedding".to_string(),
                "docs".to_string(),
                "embedding".to_string(),
                IndexKind::Vector,
            ))
            .unwrap();
        assert_eq!(
            u32::from_le_bytes(cosine.encode()[4..8].try_into().unwrap()),
            CATALOG_VERSION_EXACT,
            "a cosine vector index must stay readable by an older build"
        );

        let mut l2 = sample();
        l2.create_index(Index::vector(
            "docs_embedding".to_string(),
            "docs".to_string(),
            "embedding".to_string(),
            VectorMetric::L2,
        ))
        .unwrap();
        let bytes = l2.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_METRIC
        );
        let decoded = Catalog::decode(&bytes).unwrap();
        assert_eq!(decoded, l2);
        assert_eq!(decoded.indexes().next().unwrap().metric, VectorMetric::L2);
    }

    /// A metric on an index that has no distance is refused rather than
    /// recorded: a number in the catalog that nothing reads is one a later
    /// reader could mistake for a promise.
    #[test]
    fn only_a_vector_index_can_carry_a_metric() {
        let mut catalog = sample();
        let error = catalog
            .create_index(Index {
                metric: VectorMetric::L2,
                ..Index::single(
                    "docs_body".to_string(),
                    "docs".to_string(),
                    "body".to_string(),
                    IndexKind::FullText,
                )
            })
            .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "got {error}");
    }

    /// Two indexes over one column under two collations are two indexes, and a
    /// third under a collation one of them already has is a duplicate.
    #[test]
    fn one_column_can_carry_two_indexes_under_two_collations() {
        let mut catalog = sample();
        let declare = |name: &str, collation| Index {
            name: name.to_string(),
            table: "docs".to_string(),
            columns: alloc::vec!["body".to_string()],
            kind: IndexKind::BTree,
            unique: false,
            collations: alloc::vec![collation],
            metric: VectorMetric::Cosine,
        };
        catalog
            .create_index(declare("body_bin", Collation::Binary))
            .unwrap();
        catalog
            .create_index(declare("body_nc", Collation::NoCase))
            .unwrap();
        let err = catalog
            .create_index(declare("body_again", Collation::NoCase))
            .unwrap_err();
        assert!(matches!(err, Error::Catalog(_)), "got {err}");

        // A declaration whose two lists disagree is refused rather than
        // padded: it would key entries under a collation nobody chose.
        let err = catalog
            .create_index(Index {
                collations: alloc::vec![],
                metric: VectorMetric::Cosine,
                ..declare("body_empty", Collation::Binary)
            })
            .unwrap_err();
        assert!(matches!(err, Error::Catalog(_)), "got {err}");
    }

    #[test]
    fn quantized_vector_uses_v3_and_round_trips() {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                without_rowid: false,
                primary_key: Vec::new(),
                name: "docs".to_string(),
                columns: vec![Column::new("embedding", DataType::QuantizedVector(384))],
                strict: false,
            })
            .unwrap();
        let bytes = catalog.encode();
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            CATALOG_VERSION_QUANTIZED
        );
        assert_eq!(Catalog::decode(&bytes).unwrap(), catalog);

        let exact = sample().encode();
        assert_eq!(
            u32::from_le_bytes(exact[4..8].try_into().unwrap()),
            CATALOG_VERSION_EXACT
        );
    }
}
