//! The executor: turns a [`Plan`] into rows, using only the traits in
//! [`crate::traits`].
//!
//! ## Index maintenance
//!
//! A retrieval index exists only where the catalog records one: a `CREATE
//! INDEX` on a `TEXT` column declares a full-text index, on a `VECTOR` column
//! a nearest-neighbour one. A query that scores a column with no index is an
//! error, not a silent fall back. [`Engine::open_implicit`] restores the
//! old index-everything behaviour as a per-table default for the demo.
//!
//! Indexes are written into the database alongside the rows and restored on
//! open, stamped with the write version they reflect. A stamp that no longer
//! matches — or bytes that do not decode — means the index is rebuilt from the
//! rows, so a stale or torn index can cost time but never an answer. See
//! `docs/indexes.md`.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use crate::catalog::{
    auto_index_name, auto_unique_index_name, Catalog, Index, IndexKind, Table, CATALOG_KEY,
};
use crate::cdc::{self, ChangeKind, Changes, CDC_FLOOR_KEY, CDC_RETENTION};
use crate::collation::Collation;
use crate::error::{Error, Result};
use crate::eval::{self, Computed, Env, SharedRng, SubqueryRunner};
use crate::exec::{
    Decode, DecodeFilter, ExecRow, Filter, IndexProbe, JoinInner, NestedLoopJoin, ProbeKind,
    RowBytes, RowStream,
};
use crate::fusion::{reciprocal_rank_fusion, sort_by_score_desc};
use crate::hnsw_paged::PagedHnswIndex;
use crate::plan::{
    Aggregate, AlterAction, AlterTablePlan, ConflictAction, ConflictUpdate, CreateTablePlan,
    DeletePlan, DropTablePlan, FrameBound, InsertPlan, InsertSource, OnConflict, Order, OrderKey,
    Plan, ScalarPlan, ScoreExpr, SelectItem, SelectPlan, SetOp, SetOperationPlan, SubqueryBody,
    UpdatePlan, WindowFn, WindowFunc,
};
use crate::row::{decode_row, decode_row_masked, encode_typed_row, ColumnMask, RowBuf};
use crate::shared::SharedStorage;
use crate::sql::{self, TableRules};
use crate::statement::Statement;
use crate::traits::{
    scan_all, Clock, FullTextIndex, IndexFactory, Rng, RowId, RowScan, Scored, Storage, VectorIndex,
};
use crate::value::{DataType, Value};

/// Metadata key holding the next row id to hand out.
const NEXT_ROW_ID_KEY: &str = "next_row_id";

/// Metadata key holding the number of committed mutations.
///
/// Every statement that changes a row bumps it, in the same storage commit as
/// the change. A persisted index carries the version it reflects, so the
/// engine can tell at a glance whether a saved index still describes the rows
/// on disk. See [`Engine::persist_indexes`].
const WRITE_VERSION_KEY: &str = "write_version";

/// How many row mutations may accumulate before the engine rewrites the
/// persisted indexes.
///
/// Saving an index costs time proportional to its *size*, not to the change,
/// so doing it after every statement would make a row-at-a-time load
/// quadratic. Batching bounds that cost: at most one save per this many
/// changed rows, plus one on an explicit [`Engine::checkpoint`]. Nothing about
/// correctness depends on the number — a stale saved index is discarded on
/// open, not trusted.
const INDEX_PERSIST_INTERVAL: u64 = 1024;

/// Bytes of a saved index per metadata entry.
///
/// A saved index is megabytes; a B-tree value has to fit in a page, which is
/// 4 KiB by default. So the blob is split across numbered entries. 2 KiB
/// leaves room for the key and the page's own bookkeeping without assuming a
/// particular page size — the [`Storage`] trait deliberately does not expose
/// one, because not every backend has pages at all.
const INDEX_CHUNK_BYTES: usize = 2048;

/// How many bytes of chunks to write before committing.
///
/// A saved index is far larger than any transaction the storage engine is
/// built for: under copy-on-write, every entry written copies its root-to-leaf
/// path into fresh pages, and the write-ahead log has to hold all of them.
/// Writing a ten-megabyte index in one transaction overflows the log by two
/// orders of magnitude. So the save is committed in bounded batches. That is
/// safe because the header is cleared before the first batch and written after
/// the last: a crash in between leaves a header that no longer parses, and the
/// index is rebuilt.
const INDEX_COMMIT_BYTES: usize = 64 * 1024;

/// How many candidates each retriever returns when the query has no `LIMIT`.
const DEFAULT_CANDIDATES: usize = 64;

/// Multiplier applied to `LIMIT` when sizing each retriever's candidate list.
///
/// Fusion can only rank what the retrievers returned, so each leaf has to
/// over-fetch: a row that is 40th by vector similarity but 1st by BM25 should
/// still be able to win.
const CANDIDATE_OVERFETCH: usize = 4;

/// What a statement produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A DDL statement completed.
    Ddl,
    /// Rows were written.
    Written(usize),
    /// Rows were returned.
    Rows(ResultSet),
}

impl Outcome {
    /// Take the result set, or fail if the statement did not produce one.
    pub fn into_rows(self) -> Result<ResultSet> {
        match self {
            Outcome::Rows(rows) => Ok(rows),
            _ => Err(Error::Unsupported(
                "statement did not return rows".to_string(),
            )),
        }
    }
}

/// A query result.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultSet {
    /// Column headers, in output order.
    pub columns: Vec<String>,
    /// Rows, each the same width as `columns`.
    pub rows: Vec<Vec<Value>>,
}

/// How an [`Engine`] is opened.
///
/// Defaults are the shipped behaviour: explicit `CREATE INDEX`, vector indexes
/// held in memory, and a page cache of
/// [`DEFAULT_PAGE_CACHE_BYTES`](crate::btree::DEFAULT_PAGE_CACHE_BYTES).
#[derive(Debug, Clone, Copy)]
pub struct EngineOptions {
    /// Index every `TEXT` and `VECTOR` column of every table this engine
    /// creates, as it did before `CREATE INDEX` existed.
    pub implicit_indexes: bool,
    /// Keep vector indexes in the database instead of in memory.
    ///
    /// The in-memory [`crate::hnsw::HnswIndex`] holds every embedding and the
    /// whole graph in RAM, so a corpus that does not fit in RAM cannot be
    /// indexed at all. [`crate::hnsw_paged::PagedHnswIndex`] keeps the graph in
    /// the database file and a bounded cache in memory instead, writing through
    /// the engine's own transaction so the graph commits with the rows it
    /// describes.
    ///
    /// The trade is open time against steady-state memory: a paged index does
    /// not have to be rebuilt or reloaded on open, but every search that misses
    /// the cache is a read from the file rather than a pointer chase.
    pub paged_vector_indexes: bool,
    /// How much memory the storage engine may hold decoded database pages in.
    ///
    /// Without a cache every level of every tree descent reads its page from
    /// the device and decodes it again, on every statement. With one, a page
    /// that is still resident costs a lookup. The database is copy-on-write and
    /// never reuses a page id, so a cached page can never be stale — see
    /// [`crate::btree::cache`] for the argument and for what would break it.
    ///
    /// **This is resident memory that was not spent before.** The budget is per
    /// open database handle and is a ceiling, not a reservation: a small
    /// database never reaches it, and a handle that reads nothing holds
    /// nothing. Set it to `0` to opt out entirely and get the old behaviour
    /// back — correctness does not depend on it either way.
    ///
    /// Ignored by backends that are already in memory, such as
    /// [`crate::mem::MemStorage`].
    pub page_cache_bytes: usize,
    /// Let this handle draw on the free list instead of always growing the
    /// file (Phase 2 item 6, `CowBTree::set_page_reuse`).
    ///
    /// Without this, a page a commit stops using — because a row or an index
    /// entry was deleted, or a copy-on-write update superseded it — is never
    /// reclaimed: the file's high-water mark only ever grows, even under
    /// steady-state churn where the *live* data size is flat. This is why
    /// `false` (the default, and every existing caller before this option
    /// existed) means a database file grows forever in normal use.
    ///
    /// # Read this before enabling it
    ///
    /// **Do not enable this on a file any process might open read-only while
    /// a writer here has it on.** `Database::open_read_only` takes no OS
    /// lock, by design, so a page this handle reclaims and overwrites could
    /// be one a read-only reader — in this process or any other — still has
    /// open. Reclamation can only prove liveness for readers this process's
    /// reservation gate can see, which a lock-free read-only handle is not.
    /// This is a real, load-bearing constraint, not a caveat: it is the
    /// reason this defaults to off instead of being reclaimed automatically.
    /// See `CowBTree::set_page_reuse`'s doc comment for the full argument.
    pub page_reuse: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            implicit_indexes: false,
            paged_vector_indexes: false,
            page_cache_bytes: crate::btree::DEFAULT_PAGE_CACHE_BYTES,
            page_reuse: false,
        }
    }
}

/// The database engine.
///
/// It owns the catalog and the live indexes, and drives storage through the
/// [`Storage`] trait. Swap the constructor arguments and the same engine runs
/// against real files or against a simulated environment.
pub struct Engine {
    storage: SharedStorage,
    factory: Box<dyn IndexFactory>,
    clock: Box<dyn Clock>,
    /// Where `random()` comes from.
    ///
    /// `inlaysql-core` cannot draw a random number itself, so this is the only
    /// source, and it is seeded from the injected [`Clock`] rather than from
    /// the host: under `mem`'s logical clock the seed is fixed and a
    /// simulation replays exactly, while a real clock varies it per process.
    /// [`Engine::set_rng`] replaces it outright.
    ///
    /// Shared rather than owned outright so that an [`Env`] can hold it while
    /// the engine is borrowed mutably, which every write statement does.
    rng: SharedRng,
    /// The clock reading the statement in flight started from, so that every
    /// `'now'` in one statement sees one instant — as SQLite's
    /// `sqlite3StmtCurrentTime` does.
    statement_now: Cell<i64>,
    catalog: Catalog,
    /// Declared constraints, resolved against the catalog and kept until the
    /// catalog moves.
    ///
    /// Resolving a `CHECK` means parsing it, and a prepared `INSERT` that runs
    /// a hundred thousand times must not parse it a hundred thousand times.
    /// The cache is keyed by lowercased table name and is thrown away wholesale
    /// by every path that replaces the catalog — which is the only way it can
    /// be wrong, and the only reason it is safe to hold at all.
    rules: BTreeMap<String, Rc<TableRules>>,
    /// Keyed by table name and the index's *full* column list, in the order
    /// the index declared it — not by a single column. A single-column index
    /// (still, by far, the common case) keys under a one-element list, which
    /// is a different Rust type but the same identity a `(table, column)` key
    /// always meant; nothing about matching changed. What this makes
    /// possible is a multi-column `FullText` index living beside a
    /// single-column one over one of its own columns (`(body)` and
    /// `(title, body)` can coexist — see `Catalog::create_index`), which a
    /// key of just the column could not have told apart. See
    /// `Engine::index_meta_key_for` for why the *persisted* key format for a
    /// single column is untouched by this.
    text_indexes: BTreeMap<(String, Vec<String>), Box<dyn FullTextIndex>>,
    /// See [`Engine::text_indexes`]. Always a one-element column list in
    /// practice — [`IndexKind::Vector`] stays single-column, see its docs —
    /// but kept the same key shape as the full-text map so every retrieval
    /// index goes through one maintenance path rather than two.
    vector_indexes: BTreeMap<(String, Vec<String>), Box<dyn VectorIndex>>,
    /// Set by writes, cleared by [`Engine::refresh_indexes`].
    indexes_dirty: bool,
    next_row_id: RowId,
    /// The row id the last `INSERT` that auto-assigned one handed out. See
    /// [`Engine::last_insert_row_id`].
    last_insert_row_id: Option<RowId>,
    /// Number of committed row mutations. Stamped onto persisted indexes.
    write_version: u64,
    /// The `write_version` the persisted indexes were saved at.
    persisted_version: u64,
    /// Rows changed by the statement in flight, awaiting a change record.
    pending_changes: Vec<(String, RowId, ChangeKind)>,
    /// The newest change version that has been dropped from the log.
    cdc_floor: u64,
    /// Statements parsed since this engine was opened.
    ///
    /// A `Cell` so that [`Engine::prepare`] can take `&self`: preparing is a
    /// read of the catalog, and nothing about it should force a caller to hold
    /// the engine mutably. Exposed by [`Engine::statements_parsed`], which is
    /// how a test proves a prepared statement really is parsed only once.
    parses: Cell<u64>,
    /// Whether an explicit transaction is open. While it is, writes buffer
    /// across statements instead of committing at the end of each, and are made
    /// durable only by [`Engine::commit`].
    in_transaction: bool,
    /// How this engine was opened. `implicit_indexes` is the pre-`CREATE INDEX`
    /// behaviour, kept available for the demo and for databases that want
    /// automatic indexing; `paged_vector_indexes` decides whether a vector
    /// index lives in the database or in memory.
    options: EngineOptions,
}

impl Engine {
    /// Open an engine over the given environment, restoring any existing
    /// catalog and rebuilding the indexes from the stored rows.
    ///
    /// Indexes are created only where the catalog records a `CREATE INDEX`
    /// (or where a legacy database was grandfathered). This is the default.
    pub fn open(
        storage: Box<dyn Storage>,
        factory: Box<dyn IndexFactory>,
        clock: Box<dyn Clock>,
    ) -> Result<Self> {
        Self::open_with_options(storage, factory, clock, EngineOptions::default())
    }

    /// Open an engine that indexes every `TEXT` and `VECTOR` column of every
    /// table it creates, as it did before `CREATE INDEX` existed.
    ///
    /// The choice is recorded as ordinary index declarations in the catalog at
    /// `CREATE TABLE` time, so it is a per-table default rather than a
    /// persistent mode: once a table exists, its indexes are declared and
    /// behave exactly as if they had been created by hand.
    pub fn open_implicit(
        storage: Box<dyn Storage>,
        factory: Box<dyn IndexFactory>,
        clock: Box<dyn Clock>,
    ) -> Result<Self> {
        Self::open_with_options(
            storage,
            factory,
            clock,
            EngineOptions {
                implicit_indexes: true,
                ..EngineOptions::default()
            },
        )
    }

    /// Open an engine with an explicit choice for every option. See
    /// [`EngineOptions`].
    pub fn open_with_options(
        storage: Box<dyn Storage>,
        factory: Box<dyn IndexFactory>,
        clock: Box<dyn Clock>,
        options: EngineOptions,
    ) -> Result<Self> {
        // One handle from here on: an index that keeps itself in the database
        // takes a clone of this, so its writes join the engine's transaction
        // rather than opening one of their own.
        let storage = SharedStorage::new(storage);
        let catalog = match storage.get_meta(CATALOG_KEY)? {
            Some(bytes) => Catalog::decode(&bytes)?,
            None => Catalog::new(),
        };
        let next_row_id = read_counter(&storage, NEXT_ROW_ID_KEY, "next row id")?.unwrap_or(1);
        let write_version =
            read_counter(&storage, WRITE_VERSION_KEY, "write version")?.unwrap_or_default();
        let cdc_floor = read_counter(&storage, CDC_FLOOR_KEY, "change floor")?.unwrap_or_default();

        // Seeded from the clock, which is itself injected: in the simulation
        // that is a logical counter, so the stream is reproducible.
        let seed = clock.now_micros() as u64;
        let mut engine = Engine {
            storage,
            factory,
            rng: Rc::new(RefCell::new(
                Box::new(crate::mem::SeededRng::new(seed)) as Box<dyn Rng>
            )),
            statement_now: Cell::new(0),
            clock,
            catalog,
            rules: BTreeMap::new(),
            text_indexes: BTreeMap::new(),
            vector_indexes: BTreeMap::new(),
            indexes_dirty: false,
            next_row_id,
            last_insert_row_id: None,
            write_version,
            persisted_version: write_version,
            pending_changes: Vec::new(),
            cdc_floor,
            parses: Cell::new(0),
            in_transaction: false,
            options,
        };
        engine.restore_indexes()?;
        Ok(engine)
    }

    /// The catalog, for tooling and tests.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Resident bytes occupied by one vector index's embedding payloads.
    /// Backends that cannot measure this return `None`.
    pub fn vector_index_resident_bytes(&self, table: &str, column: &str) -> Option<usize> {
        self.vector_indexes
            .get(&retrieval_key(table, &[column.to_string()]))
            .and_then(|index| index.resident_vector_bytes())
    }

    /// The injected clock, exposed so callers can see the same time the engine
    /// would.
    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    /// Replace the generator `random()` draws from.
    ///
    /// The default is seeded from the clock at open, which is reproducible
    /// under a logical clock and varies under a real one. A simulation that
    /// wants a specific stream sets its own.
    pub fn set_rng(&mut self, rng: Box<dyn Rng>) {
        self.rng = Rc::new(RefCell::new(rng));
    }

    /// The expression environment for the statement in flight.
    ///
    /// Reads the clock once per statement rather than once per row: `'now'`
    /// must not move underneath a query, and a logical clock that ticks on
    /// every read would make it.
    fn env<'a>(&self, params: &'a [Value]) -> Env<'a> {
        Env::new(params, self.statement_now.get(), Rc::clone(&self.rng))
    }

    /// The same environment, able to evaluate subqueries.
    ///
    /// It borrows the engine for as long as it lives, which is why it is the
    /// read path's environment alone: [`Engine::insert`], [`Engine::update`]
    /// and [`Engine::delete`] build theirs and then take `&mut self` to write,
    /// so they cannot hold one. A subquery in any of those statements is
    /// refused in the planner (`sql::reject_write_subqueries`) rather than
    /// reaching an environment that could not run it.
    fn read_env<'a>(&'a self, params: &'a [Value]) -> Env<'a> {
        Env::new(params, self.statement_now.get(), Rc::clone(&self.rng)).with_subqueries(self)
    }

    /// The row id of the last row this handle inserted *without being told the
    /// key* — SQLite's `last_insert_rowid()`, and what a MySQL OK packet and
    /// Eloquent's `lastInsertId()` need.
    ///
    /// Precisely:
    ///
    /// * **Key omitted or `NULL`** on a table whose `INTEGER PRIMARY KEY`
    ///   aliases the row id: the engine takes the next value from its counter,
    ///   writes it back into the row, and reports it here.
    /// * **No `INTEGER PRIMARY KEY` at all:** the engine still assigns the key
    ///   the row is stored under, and still reports it. SQLite does the same.
    /// * **Key supplied explicitly:** *not* reported. The caller already knows
    ///   the key it chose, and overwriting the last assigned one would make a
    ///   subsequent read describe a row nobody asked about. The previous value
    ///   survives unchanged.
    /// * **Multi-row `INSERT`:** the *last* row that was assigned a key, which
    ///   is the highest of them — rows are assigned in statement order.
    /// * **A statement that inserted nothing** — a failed `INSERT`, an `UPDATE`
    ///   or `DELETE` that matched no rows, any `SELECT`, any DDL: unchanged.
    ///   Nothing here is ever cleared by a later statement.
    /// * **Before any such `INSERT`:** `None`. It is per handle and lives in
    ///   memory only, so a freshly opened handle starts at `None` however many
    ///   rows the file already holds.
    ///
    /// Set when the row enters the transaction, not when the transaction
    /// commits: inside `BEGIN`..`COMMIT` the id is visible to the statements
    /// that follow, and a later `ROLLBACK` leaves it pointing at a row that no
    /// longer exists. That is SQLite's behaviour too, and it is the only one
    /// that lets a caller use the id within the transaction that produced it.
    pub fn last_insert_row_id(&self) -> Option<RowId> {
        self.last_insert_row_id
    }

    /// Parse and plan one SQL statement, without running it.
    ///
    /// The returned [`Statement`] can be run many times with different
    /// parameters and never parses again. It is plain owned data — not
    /// borrowed from this engine — so it can be moved to another thread, and
    /// [`Engine::run`] re-checks the schema it was planned against before
    /// trusting its column ordinals.
    pub fn prepare(&self, sql: &str) -> Result<Statement> {
        self.parses.set(self.parses.get() + 1);
        sql::prepare(sql, &self.catalog)
    }

    /// How many statements this engine has parsed since it was opened.
    ///
    /// Prepared statements exist to keep this number down: `N` executions of
    /// one prepared statement add one, where `N` calls to [`Engine::execute`]
    /// add `N`.
    pub fn statements_parsed(&self) -> u64 {
        self.parses.get()
    }

    /// Run a prepared statement with `params` bound to its placeholders.
    ///
    /// Fails with [`Error::Bind`] if the parameter count is wrong and with
    /// [`Error::Stale`] if the table the statement was planned against has
    /// changed — a plan holds column *ordinals*, so running it against a
    /// different shape would return the wrong column rather than an error.
    pub fn run(&mut self, statement: &Statement, params: &[Value]) -> Result<Outcome> {
        self.refresh_snapshot()?;
        self.run_refreshed(statement, params)
    }

    /// [`Engine::run`] without the snapshot refresh, for the callers that have
    /// just done one. Splitting it keeps [`Engine::execute`] — which has to
    /// refresh *before* it parses, or it would plan against a catalog another
    /// handle has already changed — from reading the committed state twice for
    /// one statement.
    fn run_refreshed(&mut self, statement: &Statement, params: &[Value]) -> Result<Outcome> {
        statement.validate(&self.catalog, params)?;
        // One clock reading per statement, taken before anything runs, so
        // every `'now'` inside it agrees.
        self.statement_now.set(self.clock.now_micros());
        // A write inside an open transaction has to fit what the storage
        // backend can hold in one commit. Refuse it *before* running it, so a
        // too-large transaction is reported without a half-written statement:
        // the caller commits what it has, starts a new transaction and retries.
        if self.in_transaction && !statement.plan().is_read_only() {
            self.ensure_transaction_fits()?;
        }
        let outcome = match statement.plan() {
            Plan::CreateTable(create) => self.create_table(create),
            Plan::DropTable(drop) => self.drop_table(drop),
            Plan::AlterTable(alter) => self.alter_table(alter),
            Plan::CreateIndex(create) => self.create_index(create),
            Plan::CreateUniqueIndex(create) => self.create_unique_index(create),
            Plan::DropIndex(drop) => self.drop_index(drop),
            Plan::Insert(insert) => self.insert(insert, params),
            Plan::Select(select) => Ok(Outcome::Rows(self.select(select, params)?)),
            Plan::Scalar(scalar) => Ok(Outcome::Rows(self.select_scalar(scalar, params)?)),
            Plan::SetOperation(set_op) => Ok(Outcome::Rows(self.select_set_op(set_op, params)?)),
            Plan::Update(update) => self.update(update, params),
            Plan::Delete(delete) => self.delete(delete, params),
            Plan::Begin => self.begin().map(|()| Outcome::Ddl),
            Plan::Commit => self.commit().map(|()| Outcome::Ddl),
            Plan::Rollback => self.rollback().map(|()| Outcome::Ddl),
        };
        if outcome.is_err() && !statement.plan().is_read_only() {
            self.discard_failed_statement();
        }
        outcome
    }

    /// Undo whatever a write statement buffered before it failed.
    ///
    /// A statement is atomic: a `CHECK` that rejects the sixth row of an
    /// `INSERT` must leave the first five unwritten, which is what SQLite does
    /// and what a test in `constraints.rs` asserts in both directions. The
    /// engine buffers writes and commits them at the end of the statement, so
    /// a failure between those two points leaves them buffered — and the *next*
    /// statement's commit would make them durable, which is the part that would
    /// be silent.
    ///
    /// Inside an explicit transaction there is nothing to do here and doing
    /// something would be wrong: the buffer holds the caller's earlier
    /// statements too, and discarding those is `ROLLBACK`'s decision to make,
    /// not this one's. That is the state [`Engine::begin`] already documents.
    fn discard_failed_statement(&mut self) {
        if self.in_transaction {
            return;
        }
        // Both of these can only fail for reasons the failing statement has
        // already reported; there is no second error worth returning here, and
        // the reload is what puts the in-memory catalog back in agreement with
        // the committed one after, say, a `CREATE TABLE` that was rejected
        // after it had already been added in memory.
        let _ = self.storage.rollback();
        let _ = self.reload();
    }

    /// Run a prepared statement that must return rows.
    pub fn run_query(&mut self, statement: &Statement, params: &[Value]) -> Result<ResultSet> {
        self.run(statement, params)?.into_rows()
    }

    /// Plan and run one SQL statement.
    ///
    /// Parses every time. For a statement that runs more than once, prepare it
    /// with [`Engine::prepare`] and run it with [`Engine::run`] instead.
    ///
    /// The snapshot is refreshed before the parse rather than after it, because
    /// planning reads the catalog: a table another handle created would
    /// otherwise be "no such table" here even though the statement about to run
    /// would have found it.
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        self.refresh_snapshot()?;
        let statement = self.prepare(sql)?;
        self.run_refreshed(&statement, params)
    }

    /// Run a statement that must return rows.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<ResultSet> {
        self.execute(sql, params)?.into_rows()
    }

    /// Start an explicit transaction.
    ///
    /// Until [`Engine::commit`], every statement's writes are buffered rather
    /// than committed: a thousand single-row inserts become one durable commit
    /// (one `fsync`) instead of a thousand. The whole transaction is atomic —
    /// a crash leaves either all of its writes or none of them — and every row
    /// it changed shares one change-record version, exactly as one multi-row
    /// statement does.
    ///
    /// Reads see the transaction's own writes: a row inserted by one statement
    /// is found by the next, because the tree resolves a read against the
    /// transaction's working root and its buffered pages before the committed
    /// data area (see [`crate::btree::CowBTree::get`]).
    ///
    /// What they do *not* see is anybody else's writes. The snapshot is pinned
    /// at [`Engine::begin`] and stays pinned until the transaction ends: the
    /// per-statement refresh that lets a handle outside a transaction pick up
    /// another handle's commits does nothing here, so a transaction reads one
    /// consistent state from start to finish. A concurrent commit is discovered
    /// at [`Engine::commit`], which rebases disjoint writes and reports a real
    /// overlap as [`Error::Conflict`].
    ///
    /// Within a transaction a statement may still fail, leaving the engine in
    /// an indeterminate state; [`Engine::rollback`] (or [`Engine::commit`]) is
    /// the way to leave it.
    pub fn begin(&mut self) -> Result<()> {
        if self.in_transaction {
            return Err(Error::Transaction(
                "a transaction is already open".to_string(),
            ));
        }
        // Pin the state as it is *now*, not as this handle last happened to see
        // it. Refreshing here is what makes the pin mean something: without it
        // a handle that had been idle would begin its transaction on whatever
        // snapshot it was left on, and every statement inside would read a
        // database that had moved on long before `BEGIN` was called.
        self.refresh_snapshot()?;
        self.in_transaction = true;
        Ok(())
    }

    /// Commit the open transaction: make every write since [`Engine::begin`]
    /// durable in one storage commit.
    ///
    /// A lost race surfaces as [`Error::Conflict`], exactly as for a
    /// single-statement write: the engine reloads the winner's state and the
    /// handle stays usable. On a success or a conflict the transaction is over.
    pub fn commit(&mut self) -> Result<()> {
        self.require_transaction("commit")?;
        self.bump_write_version()?;
        let result = self.commit_storage();
        self.in_transaction = false;
        result
    }

    /// Discard every write since [`Engine::begin`], leaving the database
    /// byte-identical to its state before the transaction.
    ///
    /// The buffered writes are dropped and the engine reloads itself from the
    /// committed store, so its catalog, counters and indexes agree with what is
    /// actually on disk.
    pub fn rollback(&mut self) -> Result<()> {
        self.require_transaction("rollback")?;
        self.in_transaction = false;
        self.storage.rollback()?;
        self.reload()
    }

    fn require_transaction(&self, what: &str) -> Result<()> {
        if self.in_transaction {
            Ok(())
        } else {
            Err(Error::Transaction(alloc::format!(
                "{what} with no transaction open"
            )))
        }
    }

    /// Finish a write statement: persist the row-id counter, then either commit
    /// the buffered writes immediately (outside a transaction) or leave them
    /// buffered for [`Engine::commit`] (inside one).
    ///
    /// The row-id counter is written on every path so that a transaction's
    /// auto-assigned keys stay ahead of the rows on disk even if the
    /// transaction is later rolled back: on rollback the counter is re-read from
    /// the committed store, where it last matches the committed rows.
    fn end_write(&mut self) -> Result<()> {
        self.storage
            .put_meta(NEXT_ROW_ID_KEY, &self.next_row_id.to_le_bytes())?;
        if !self.in_transaction {
            self.bump_write_version()?;
            return self.commit_storage();
        }
        Ok(())
    }

    /// Whether a write statement can be buffered into the open transaction, or
    /// the transaction has grown too large for the storage backend to commit
    /// in one record.
    fn ensure_transaction_fits(&self) -> Result<()> {
        if self.storage.transaction_is_nearly_full() {
            return Err(Error::Transaction(
                "transaction is too large for the write-ahead log; \
                 commit it and start a new one"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Commit the storage transaction, keeping the engine and the store in
    /// agreement when another writer got there first.
    ///
    /// Every write in this file goes through here rather than calling
    /// [`Storage::commit`] directly, because a commit has two failure modes
    /// and only one of them is a fault. A lost race
    /// ([`Error::Conflict`]) means the transaction was rolled back and the
    /// store now holds the winner's state — while this engine's catalog,
    /// counters and in-memory indexes still describe the transaction that was
    /// thrown away. Left alone, that divergence would outlive the failed
    /// statement and answer later queries with rows that were never committed.
    ///
    /// So the engine reloads itself from the store before returning the error.
    /// The statement failed, the handle is usable, and retrying is correct.
    fn commit_storage(&mut self) -> Result<()> {
        match self.storage.commit() {
            Ok(()) => {
                // The storage layer may have rebased this statement after a
                // disjoint concurrent commit and assigned newer monotonic
                // metadata values than this Engine predicted. Keep the
                // handle's counters aligned with the root it just committed.
                self.next_row_id =
                    read_counter(&self.storage, NEXT_ROW_ID_KEY, "next row id")?.unwrap_or(1);
                self.write_version =
                    read_counter(&self.storage, WRITE_VERSION_KEY, "write version")?
                        .unwrap_or_default();
                self.cdc_floor =
                    read_counter(&self.storage, CDC_FLOOR_KEY, "change floor")?.unwrap_or_default();
                Ok(())
            }
            Err(Error::Conflict) => {
                self.reload()?;
                Err(Error::Conflict)
            }
            Err(other) => Err(other),
        }
    }

    /// Step this handle onto the state other handles have committed, at the
    /// start of every statement that is not inside an explicit transaction.
    ///
    /// Without this a handle's view of the file advances only when *it*
    /// commits: the storage backend caches the committed root when it opens and
    /// re-reads it inside its own commit, so a handle that only reads reads the
    /// snapshot it opened on for as long as it lives. Two `Database` handles on
    /// one file — or, later, two connections to one server — would each see a
    /// private, frozen database.
    ///
    /// Inside `BEGIN`..`COMMIT` nothing happens here. A transaction is a pinned
    /// snapshot by definition, and [`Storage::refresh`] refuses while writes
    /// are buffered anyway; the check is here as well so the intent is stated
    /// where the policy lives rather than only where it is enforced.
    ///
    /// # Cost
    ///
    /// This runs before every statement, so the path where nothing moved has to
    /// be nearly free. Everything above [`Storage::refresh`] is: it answers
    /// `false` and this returns, having touched no catalog, no counter and no
    /// index. Only a root that actually moved reaches
    /// [`Engine::adopt_committed_state`](Self::adopt_committed_state), which is
    /// what keeps a foreign commit from costing an index rebuild.
    ///
    /// [`Storage::refresh`] itself is free on the tree backend too, on the path
    /// that matters: it answers from a commit counter the device keeps, without
    /// reading anything, whenever nothing has been committed since this handle
    /// last looked. Only a device that cannot count commits — the simulated
    /// disk, deliberately — or a generation that really moved pays for reading
    /// the log. See [`crate::btree::CowBTree::refresh`].
    fn refresh_snapshot(&mut self) -> Result<()> {
        if self.in_transaction {
            return Ok(());
        }
        if !self.storage.refresh()? {
            return Ok(());
        }
        self.adopt_committed_state()
    }

    /// Re-read the catalog and counters after another handle's commit, keeping
    /// the live indexes when they still describe the committed rows.
    ///
    /// This is [`Engine::reload`] with the one difference that matters on a
    /// path taken by ordinary reads: `reload` throws every index away, and an
    /// index thrown away is an index rebuilt from every row of its table the
    /// next time anything reads it. That is the right price for a rolled-back
    /// transaction, which happens rarely; it is not the right price for
    /// "somebody else committed", which on a busy file happens constantly.
    ///
    /// So the rebuild is conditioned on the two things it depends on. The
    /// **write version** counts committed row mutations, and it is the same
    /// stamp [`Engine::load_saved_indexes`](Self::load_saved_indexes) checks a
    /// saved index against; if it has not moved, no row changed and the indexes
    /// in memory are exactly as current as they were. The **catalog** decides
    /// which indexes exist at all, so a foreign `CREATE TABLE` or `CREATE
    /// INDEX` has to be honoured even though it changed no row. When either
    /// moved, [`Engine::restore_indexes`](Self::restore_indexes) runs — the
    /// same code as on open, which loads a saved index whose stamp matches the
    /// new write version and rebuilds from the rows only when it does not.
    fn adopt_committed_state(&mut self) -> Result<()> {
        let catalog = match self.storage.get_meta(CATALOG_KEY)? {
            Some(bytes) => Catalog::decode(&bytes)?,
            None => Catalog::new(),
        };
        let previous_version = self.write_version;
        let previous_catalog = core::mem::replace(&mut self.catalog, catalog);
        // Unconditional, and before the early return below: the constraints
        // cache is keyed off a catalog this handle no longer holds, whether or
        // not the new one turns out to be equal.
        self.invalidate_rules();

        self.next_row_id =
            read_counter(&self.storage, NEXT_ROW_ID_KEY, "next row id")?.unwrap_or(1);
        self.write_version =
            read_counter(&self.storage, WRITE_VERSION_KEY, "write version")?.unwrap_or_default();
        self.cdc_floor =
            read_counter(&self.storage, CDC_FLOOR_KEY, "change floor")?.unwrap_or_default();

        if self.write_version == previous_version && self.catalog == previous_catalog {
            // Whatever the other handle committed, it was not a row and not a
            // schema — a checkpointed index blob, a trimmed change record. The
            // indexes this handle holds still describe the committed rows.
            return Ok(());
        }

        self.persisted_version = self.write_version;
        self.text_indexes.clear();
        self.vector_indexes.clear();
        self.indexes_dirty = false;
        // Anything this handle had queued describes rows from before the state
        // it just adopted; the commit that would have published them is gone.
        self.pending_changes.clear();
        self.restore_indexes()
    }

    /// Discard every piece of in-memory state and rebuild it from the store.
    ///
    /// The same work [`Engine::open`] does, on an engine that is already open.
    fn reload(&mut self) -> Result<()> {
        self.catalog = match self.storage.get_meta(CATALOG_KEY)? {
            Some(bytes) => Catalog::decode(&bytes)?,
            None => Catalog::new(),
        };
        self.invalidate_rules();
        self.next_row_id =
            read_counter(&self.storage, NEXT_ROW_ID_KEY, "next row id")?.unwrap_or(1);
        self.write_version =
            read_counter(&self.storage, WRITE_VERSION_KEY, "write version")?.unwrap_or_default();
        self.cdc_floor =
            read_counter(&self.storage, CDC_FLOOR_KEY, "change floor")?.unwrap_or_default();
        self.persisted_version = self.write_version;
        self.text_indexes.clear();
        self.vector_indexes.clear();
        self.indexes_dirty = false;
        // Changes the rolled-back statement had queued describe rows that do
        // not exist. Publishing them would tell a CDC consumer about a write
        // nobody made.
        self.pending_changes.clear();
        self.restore_indexes()
    }

    // ------------------------------------------------------------------ DDL

    fn create_table(&mut self, plan: &CreateTablePlan) -> Result<Outcome> {
        let table = &plan.table;
        // `IF NOT EXISTS` asks whether the *name* is taken, not whether the
        // table matches: SQLite does not compare the two definitions either.
        if plan.if_not_exists && self.catalog.table(&table.name).is_some() {
            return Ok(Outcome::Ddl);
        }
        if table
            .columns
            .iter()
            .any(|column| column.ty.is_quantized_vector())
            && !self.storage.supports_quantized_vectors()
        {
            return Err(Error::FormatVersion(
                "VECTOR(n, INT8) requires file format 4; this grandfathered version-3 \
                 database remains exact-vector compatible but must be recreated to opt in"
                    .to_string(),
            ));
        }
        self.invalidate_rules();
        self.catalog
            .create_table_with(table.clone(), plan.constraints.clone())?;
        // Resolve the constraints now rather than at the first `INSERT`, so a
        // `CHECK` that references a column it cannot see fails at the statement
        // that declared it.
        sql::table_rules(table, &self.catalog)?;
        if self.options.implicit_indexes {
            // The pre-`CREATE INDEX` behaviour, as a per-table default: declare
            // a full-text index for every TEXT column and a vector index for
            // every VECTOR column, so the demo and automatic-indexing users
            // get what they used to without any opt-in.
            for column in &table.columns {
                let kind = match column.ty {
                    DataType::Text => Some(IndexKind::FullText),
                    DataType::Vector(_) | DataType::QuantizedVector(_) => Some(IndexKind::Vector),
                    _ => None,
                };
                if let Some(kind) = kind {
                    self.catalog.create_index(Index::single(
                        auto_index_name(&table.name, &column.name),
                        table.name.to_ascii_lowercase(),
                        column.name.to_ascii_lowercase(),
                        kind,
                    ))?;
                }
            }
        }
        // A `UNIQUE` written inside `CREATE TABLE` gets a B-tree index to
        // enforce it. Without one, every row written to the table costs a full
        // scan per constraint; with one it costs a probe. The table is empty,
        // so there is nothing to build — only the declaration to record.
        self.declare_unique_indexes(&table.name)?;
        self.open_indexes_for(table)?;
        let encoded = self.catalog.encode();
        self.storage.put_meta(CATALOG_KEY, &encoded)?;
        self.end_write()?;
        Ok(Outcome::Ddl)
    }

    /// `DROP TABLE`: remove the declaration, its indexes and every row.
    ///
    /// The rows go one at a time because that is the whole of the storage
    /// surface — there is no "drop this key range" — so this is O(rows), and
    /// each deletion joins the statement's transaction like any other write.
    fn drop_table(&mut self, plan: &DropTablePlan) -> Result<Outcome> {
        if self.catalog.table(&plan.name).is_none() {
            if plan.if_exists {
                return Ok(Outcome::Ddl);
            }
            return Err(Error::Catalog(alloc::format!(
                "no such table: {}",
                plan.name
            )));
        }
        self.invalidate_rules();
        let (table, indexes) = self.catalog.drop_table(&plan.name)?;
        for index in &indexes {
            self.forget_index(index)?;
            // Unlike `ALTER TABLE`, this really does invalidate the entries:
            // the rows they point at are about to stop existing.
            self.purge_index_entries(index)?;
        }
        for (id, _) in scan_all(&self.storage, &table.name)? {
            self.storage.delete_row(&table.name, id)?;
            self.note_change(&table.name, id, ChangeKind::Delete);
        }
        self.storage.put_meta(CATALOG_KEY, &self.catalog.encode())?;
        self.end_write()?;
        Ok(Outcome::Ddl)
    }

    /// `ALTER TABLE`, restricted to the four operations SQLite has.
    ///
    /// Three of the four rewrite every row, where SQLite would only rewrite the
    /// schema: this engine stores a row as a positional list of values with no
    /// column directory, so adding, removing or reordering a column changes
    /// what every stored row means. Rewriting is O(rows) and honest; the
    /// alternative — decoding old rows against a new shape — is the kind of
    /// silent misread the format versions exist to prevent.
    fn alter_table(&mut self, plan: &AlterTablePlan) -> Result<Outcome> {
        let before = self.catalog.require_table(&plan.table)?.clone();
        // Every declared index of this table is about to have its key, its
        // column name or its rows change underneath it. Blanking the saved
        // copies now means the rebuild below reads the rows rather than a blob
        // describing the table as it used to be.
        let indexes: Vec<Index> = self
            .catalog
            .indexes_for(&before.name)
            .into_iter()
            .cloned()
            .collect();
        for index in &indexes {
            self.forget_index(index)?;
        }
        self.invalidate_rules();

        match &plan.action {
            AlterAction::AddColumn(column) => {
                self.catalog.add_column(&before.name, column.clone())?;
                let after = self.catalog.require_table(&before.name)?.clone();
                // The new column's default has to be *materialised* into every
                // existing row, because a row is only as wide as it was
                // written. SQLite answers the default from its schema instead;
                // the two are indistinguishable from a query's point of view.
                let rules = sql::table_rules(&after, &self.catalog)?;
                let ordinal = after.columns.len() - 1;
                let env = self.env(&[]);
                let filled = match &rules.defaults[ordinal] {
                    Some(expr) => sql::coerce(
                        eval::evaluate(expr, &[], Computed::NONE, &env)?,
                        &after.columns[ordinal],
                    )?,
                    None => Value::Null,
                };
                self.rewrite_rows(&after, |row| {
                    row.resize(ordinal, Value::Null);
                    row.push(filled.clone());
                    Ok(())
                })?;
            }
            AlterAction::RenameTable(target) => {
                if self.catalog.table(target).is_some() {
                    return Err(Error::Catalog(alloc::format!(
                        "table `{target}` already exists"
                    )));
                }
                // Rows are keyed by table name, so the move is a copy under the
                // new name and a delete under the old one.
                for (id, bytes) in scan_all(&self.storage, &before.name)? {
                    self.storage.put_row(target, id, &bytes)?;
                    self.storage.delete_row(&before.name, id)?;
                    self.note_change(target, id, ChangeKind::Insert);
                }
                self.catalog.rename_table(&before.name, target)?;
            }
            AlterAction::RenameColumn { from, to } => {
                self.catalog
                    .rename_column(&before.name, from, to, |check| {
                        sql::rewrite_column_reference(check, from, to)
                    })?;
                // Stored rows are positional, so nothing about them changed —
                // only what the ordinals are called.
            }
            AlterAction::DropColumn(column) => {
                let ordinal =
                    self.catalog
                        .drop_column(&before.name, column, sql::expression_mentions)?;
                let after = self.catalog.require_table(&before.name)?.clone();
                self.rewrite_rows(&after, |row| {
                    if ordinal < row.len() {
                        row.remove(ordinal);
                    }
                    Ok(())
                })?;
            }
        }

        // Whatever moved, the live indexes describe the table as it was. Throw
        // them away and rebuild from the rows, which are now current.
        self.text_indexes.clear();
        self.vector_indexes.clear();
        self.indexes_dirty = false;
        self.restore_indexes()?;
        self.storage.put_meta(CATALOG_KEY, &self.catalog.encode())?;
        self.end_write()?;
        Ok(Outcome::Ddl)
    }

    /// Drop one index's backend and blank its saved copy.
    ///
    /// There is no `delete` on the metadata surface, so "no saved index" is
    /// spelled as an empty header — the same way [`Engine::drop_index`] says
    /// it. Chunks are left behind unreachable; the header is what makes them
    /// an index.
    fn forget_index(&mut self, index: &Index) -> Result<()> {
        let key = retrieval_key(&index.table, &index.columns);
        match index.kind {
            IndexKind::FullText => {
                self.text_indexes.remove(&key);
            }
            IndexKind::Vector => {
                self.vector_indexes.remove(&key);
            }
            // A B-tree index has no backend and no saved copy — its entries
            // *are* the index, and they are already durable rows. Nothing to
            // forget, and deliberately nothing to purge: this is called by
            // `ALTER TABLE`, which never changes an indexed value (a column an
            // index names cannot be dropped, a rename leaves values alone, and
            // an added column is appended), so the entries stay exactly true.
            // `DROP INDEX` and `DROP TABLE` purge through
            // [`Engine::purge_index_entries`] instead.
            IndexKind::BTree => return Ok(()),
        }
        self.storage
            .put_meta(&index_meta_key_for(&index.table, &index.columns), &[])
    }

    /// Delete every entry of one B-tree index.
    ///
    /// O(entries), because deleting a key range is not on the storage surface.
    /// Outside a transaction it commits as it goes, for the reason
    /// [`Engine::build_btree_index`] does — one log region is a hard ceiling
    /// and copy-on-write dirties more than the payload. Inside a caller's
    /// transaction it must not: committing there would make the caller's
    /// buffered writes durable at a moment they did not choose, so the purge
    /// stays in one transaction and fails if it does not fit, exactly as
    /// `DROP TABLE`'s row deletion does.
    fn purge_index_entries(&mut self, index: &Index) -> Result<()> {
        if index.kind != IndexKind::BTree {
            return Ok(());
        }
        let range = crate::index::KeyRange::whole(&index.name);
        let keys = self
            .storage
            .scan_index_range(&range.start, range.end.as_deref())?;
        for key in keys {
            self.storage.delete_index_entry(&key)?;
            if !self.in_transaction && self.storage.transaction_is_nearly_full() {
                self.commit_storage()?;
            }
        }
        Ok(())
    }

    /// Declare a B-tree index for every `UNIQUE` constraint of `table` that
    /// does not have one yet, so the constraint is enforced by a probe rather
    /// than by a scan of the whole table per row written.
    ///
    /// A constraint over a column no B-tree index can cover — a `VECTOR`
    /// column — keeps the scan. It is slow and it is correct, and it is what
    /// the constraint cost before this existed.
    fn declare_unique_indexes(&mut self, table: &str) -> Result<()> {
        let Some(constraints) = self.catalog.constraints(table) else {
            return Ok(());
        };
        let definition = self.catalog.require_table(table)?;
        let mut wanted: Vec<(String, Vec<String>, Vec<Collation>)> = Vec::new();
        for (nth, group) in constraints.unique.iter().enumerate() {
            let columns: Vec<String> = group
                .columns
                .iter()
                .map(|column| column.to_ascii_lowercase())
                .collect();
            let orderable = columns.iter().all(|column| {
                definition
                    .column(column)
                    .is_some_and(|(_, c)| c.ty.vector_dim().is_none())
            });
            // The index that enforces a `UNIQUE` has to key its entries under
            // the columns' *declared* collations, because that is what the
            // constraint means: on a `NOCASE` column, `'Ada'` and `'ADA'` are
            // one key and the constraint has to say so. An index under any
            // other collation would not answer the same question, which is why
            // the lookup below asks for these ones by name.
            let collations: Vec<Collation> = columns
                .iter()
                .map(|column| {
                    definition
                        .column(column)
                        .map_or(Collation::Binary, |(_, c)| c.collation)
                })
                .collect();
            if !orderable
                || self
                    .catalog
                    .btree_index_on(table, &columns, &collations)
                    .is_some()
            {
                continue;
            }
            let name = match &group.name {
                Some(name) => name.clone(),
                None => auto_unique_index_name(&table.to_ascii_lowercase(), &columns, nth),
            };
            wanted.push((name, columns, collations));
        }
        for (name, columns, collations) in wanted {
            self.catalog.create_index(Index {
                name,
                table: table.to_ascii_lowercase(),
                columns,
                kind: IndexKind::BTree,
                unique: true,
                collations,
            })?;
        }
        Ok(())
    }

    /// Rewrite every row of a table through `change`.
    fn rewrite_rows(
        &mut self,
        table: &Table,
        mut change: impl FnMut(&mut Vec<Value>) -> Result<()>,
    ) -> Result<()> {
        for (id, bytes) in scan_all(&self.storage, &table.name)? {
            let mut row = decode_row(&bytes)?;
            change(&mut row)?;
            self.storage
                .put_row(&table.name, id, &encode_table_row(table, &row))?;
            self.note_change(&table.name, id, ChangeKind::Update);
        }
        Ok(())
    }

    /// The resolved constraints of one table, parsing them at most once per
    /// catalog.
    fn rules_for(&mut self, table: &Table) -> Result<Rc<TableRules>> {
        let key = table.name.to_ascii_lowercase();
        if let Some(rules) = self.rules.get(&key) {
            return Ok(Rc::clone(rules));
        }
        let rules = Rc::new(sql::table_rules(table, &self.catalog)?);
        self.rules.insert(key, Rc::clone(&rules));
        Ok(rules)
    }

    /// Throw away every resolved constraint, because the catalog moved.
    fn invalidate_rules(&mut self) {
        self.rules.clear();
    }

    /// Create the index backends this table's catalog declares, if they are not
    /// already open.
    fn open_indexes_for(&mut self, table: &Table) -> Result<()> {
        let declared: Vec<Index> = self
            .catalog
            .indexes_for(&table.name)
            .into_iter()
            .cloned()
            .collect();
        for index in declared {
            self.open_one_index(table, &index)?;
        }
        Ok(())
    }

    /// Create (or replace) the backend for one declared index.
    ///
    /// Called for a fresh table on open and for a newly created index; in both
    /// cases the key is not yet populated, so `insert` cannot clobber a live
    /// index. Replacement is deliberate only in `load_saved_indexes`, which
    /// re-creates the backends after a failed decode.
    fn open_one_index(&mut self, table: &Table, index: &Index) -> Result<()> {
        // A B-tree index has no backend to open: it is rows in the tree, and
        // the tree is already open.
        if index.kind == IndexKind::BTree {
            return Ok(());
        }
        let key = retrieval_key(&index.table, &index.columns);
        match index.kind {
            IndexKind::FullText => {
                // `factory.full_text` takes a single column name for logging
                // purposes only — neither shipped `IndexFactory` reads it —
                // so a multi-column index just passes its first; nothing
                // downstream keys on this value, `key` above does that.
                let backend = self.factory.full_text(&index.table, index.column())?;
                self.text_indexes.insert(key, backend);
            }
            IndexKind::BTree => unreachable!("returned above"),
            IndexKind::Vector => {
                let (_, column) = table.require_column(index.column())?;
                let Some(dim) = column.ty.vector_dim() else {
                    // The catalog already validated this; unreachable.
                    return Err(Error::Index(alloc::format!(
                        "vector index on non-vector column `{}`",
                        index.column()
                    )));
                };
                let quantized = column.ty.is_quantized_vector();
                let backend = if self.options.paged_vector_indexes {
                    self.open_paged_vector_index(&index.table, index.column(), dim, quantized)?
                } else if quantized {
                    self.factory
                        .quantized_vector(&index.table, index.column(), dim)?
                } else {
                    self.factory.vector(&index.table, index.column(), dim)?
                };
                self.vector_indexes.insert(key, backend);
            }
        }
        Ok(())
    }

    /// Open the paged ANN backend for one column, restoring whatever graph the
    /// database already holds for it.
    ///
    /// It shares the engine's storage handle and does not commit: its node
    /// writes join whatever transaction the engine has open, so the graph and
    /// the rows it describes reach the log together.
    ///
    /// `quantized` mirrors what the same column already gets from the
    /// in-memory backend (see [`IndexFactory::quantized_vector`]): a
    /// `VECTOR(n, INT8)` column gets a [`PagedHnswIndex`] that stores int8
    /// nodes, not the exact `f32` every paged index used to store regardless
    /// of the column's declared type.
    fn open_paged_vector_index(
        &self,
        table: &str,
        column: &str,
        dim: usize,
        quantized: bool,
    ) -> Result<Box<dyn VectorIndex>> {
        let namespace = vector_index_namespace(table, column);
        let index = if quantized {
            PagedHnswIndex::open_quantized(self.storage.clone(), namespace, dim)?
        } else {
            PagedHnswIndex::open(self.storage.clone(), namespace, dim)?
        }
        .joined_to_caller_transaction();
        Ok(Box::new(index))
    }

    /// `CREATE INDEX`: declare the index, build it from the existing rows, and
    /// persist the updated catalog.
    fn create_index(&mut self, create: &crate::plan::CreateIndexPlan) -> Result<Outcome> {
        // A scan inside a transaction now does see the transaction's own rows,
        // so the original reason for this refusal — an index built over the
        // committed state would silently omit them — no longer holds. The
        // refusal stays because nothing has yet established what the *other*
        // half should do: `CREATE INDEX` builds a backend that a rollback would
        // have to unbuild, and for a self-persisting backend that means undoing
        // durable structure as well as the catalog entry. Lifting it is its own
        // change, with its own crash tests; refusing is the honest state today.
        if self.in_transaction {
            return Err(Error::Transaction(
                "cannot CREATE INDEX inside a transaction; commit it first".to_string(),
            ));
        }
        let table = self.catalog.require_table(&create.table)?.clone();
        let mut columns = Vec::with_capacity(create.columns.len());
        for ordinal in &create.columns {
            let column = table
                .columns
                .get(*ordinal)
                .ok_or_else(|| Error::Catalog("column ordinal out of range".to_string()))?;
            columns.push(column.name.to_ascii_lowercase());
        }
        let index = Index {
            name: create.name.clone(),
            table: create.table.clone(),
            columns,
            kind: create.kind,
            unique: create.unique,
            collations: create.collations.clone(),
        };
        // A unique index is a constraint as well as an access path, and the
        // constraint half is the half that changes answers. It is recorded
        // first so that it exists before the entries are built, and so that
        // `DROP INDEX` finds both halves under the one name.
        if create.unique {
            self.invalidate_rules();
            self.catalog.create_unique_constraint(
                &create.table,
                crate::catalog::UniqueConstraint {
                    name: Some(create.name.clone()),
                    columns: index.columns.clone(),
                },
            )?;
        }
        if let Err(error) = self.catalog.create_index(index.clone()) {
            // The constraint was recorded a moment ago and the index that was
            // to enforce it was refused; leaving the constraint behind would
            // half-apply the statement.
            if create.unique {
                self.catalog.drop_unique_constraint(&create.name);
            }
            return Err(error);
        }
        self.open_one_index(&table, &index)?;

        // A table may already have rows; the new index has to describe them.
        // The rows are the source of truth, so this is a scan, exactly as an
        // index rebuild on open is.
        // Materialised, not streamed: the build writes as it goes (see
        // `build_btree_index`), and a statement must see the table as it stood
        // when it began — the same rule `UPDATE` and `DELETE` follow.
        let rows = scan_all(&self.storage, &table.name)?;
        if index.kind == IndexKind::BTree {
            if let Err(error) = self.build_btree_index(&table, &index, &rows) {
                // The declaration is undone and the handle reloaded, because a
                // large build commits as it goes and a failure part way through
                // has already made some entries durable. Reloading is what
                // makes the catalog and the tree agree again — and the entries
                // that reached the platter are unreachable, because nothing
                // reads a prefix no declaration names, and the next
                // `CREATE INDEX` of this name clears them before it starts.
                let _ = self.catalog.drop_index(&create.name);
                if create.unique {
                    self.catalog.drop_unique_constraint(&create.name);
                }
                self.invalidate_rules();
                self.reload()?;
                return Err(error);
            }
        } else {
            // Every named column, not just the first: a single-column index
            // (`index.columns` has one entry) behaves exactly as before, and a
            // multi-column `FullText` index gets every existing row's combined
            // text the same way a freshly inserted row would.
            for (id, bytes) in &rows {
                let row = decode_row(bytes)?;
                self.index_row_for_index(&table, &index, *id, &row)?;
            }
        }

        self.storage.put_meta(CATALOG_KEY, &self.catalog.encode())?;
        self.end_write()?;
        Ok(Outcome::Ddl)
    }

    /// Write one B-tree index's entries for a table that already has rows,
    /// refusing if the rows already violate a `UNIQUE` declaration.
    ///
    /// # Why this commits as it goes
    ///
    /// One transaction has a hard ceiling — a log region — and a copy-on-write
    /// tree dirties far more than the payload it writes, so an index over even
    /// a few thousand rows does not fit in one. So the build is batched, on the
    /// backend's own answer to "is this transaction nearly full" rather than on
    /// a byte budget that would be right for one backend and wrong for another,
    /// exactly as [`Engine::persist_indexes`] is.
    ///
    /// That makes the ordering load-bearing, and it is: **entries first, the
    /// catalog last.** A crash in the middle leaves entries under a name no
    /// declaration mentions, which nothing can read and which the next
    /// `CREATE INDEX` of that name clears before it starts. The other order —
    /// declaring the index and then filling it — would leave a *declared* index
    /// that is missing rows, and that is an index that lies.
    ///
    /// # The duplicate check
    ///
    /// The index is the check: entries are sorted by encoded value, so two rows
    /// that collide are adjacent, and one pass finds them. That is
    /// O(rows log rows) instead of the O(rows²) the scan-based check cost. A
    /// shared encoding is confirmed against [`unique_key_collides`] — the same
    /// function every other uniqueness decision goes through — because it, not
    /// the encoding, is what says a `NULL` never collides.
    fn build_btree_index(
        &mut self,
        table: &Table,
        index: &Index,
        rows: &[(RowId, RowBuf)],
    ) -> Result<()> {
        let ordinals = index_ordinals(table, index)?;
        let mut entries: Vec<(Vec<u8>, RowId)> = Vec::with_capacity(rows.len());
        for (id, bytes) in rows {
            let row = decode_row(bytes)?;
            let values = index_values(table, index, &row)?;
            entries.push((
                crate::index::entry_key(&index.name, &values, &index.collations, *id)?,
                *id,
            ));
        }
        if index.unique {
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for pair in entries.windows(2) {
                // The row id is the last eight bytes; two entries collide when
                // everything before it is equal.
                let left = &pair[0].0[..pair[0].0.len() - 8];
                let right = &pair[1].0[..pair[1].0.len() - 8];
                if left != right {
                    continue;
                }
                let (Some(left), Some(right)) = (
                    self.storage.get_row(&table.name, pair[0].1)?,
                    self.storage.get_row(&table.name, pair[1].1)?,
                ) else {
                    continue;
                };
                let (left, right) = (decode_row(&left)?, decode_row(&right)?);
                if unique_key_collides(&ordinals, &index.collations, &left, &right) {
                    return Err(conflict_error(
                        table,
                        &Conflict {
                            id: pair[1].1,
                            values: right,
                            columns: ordinals,
                        },
                    ));
                }
            }
        }

        // Anything left under this name by a build that crashed part way is
        // cleared first, so the entries written below are the whole index.
        self.purge_index_entries(index)?;
        for (key, _) in entries {
            self.storage.put_index_entry(&key)?;
            // `CREATE INDEX` is refused inside a caller's transaction, so this
            // commit can only be making the engine's own work durable.
            if self.storage.transaction_is_nearly_full() {
                self.commit_storage()?;
            }
        }
        Ok(())
    }

    /// `CREATE UNIQUE INDEX`: record a named `UNIQUE` constraint.
    ///
    /// The rows already in the table have to satisfy it, exactly as they do in
    /// SQLite — a unique index over duplicate data is an error, not a
    /// constraint that starts out already violated. Checking it is the same
    /// O(rows²) scan the constraint itself costs, once.
    fn create_unique_index(
        &mut self,
        create: &crate::plan::CreateUniqueIndexPlan,
    ) -> Result<Outcome> {
        let table = self.catalog.require_table(&create.table)?.clone();
        let mut ordinals = Vec::with_capacity(create.columns.len());
        for column in &create.columns {
            ordinals.push(table.require_column(column)?.0);
        }
        // A constraint's keys are its columns' declared collations. This is the
        // path a `UNIQUE` over a `VECTOR` takes — no ordered index can cover it
        // — but the text half of a mixed group still has to fold.
        let collations: Vec<Collation> = ordinals
            .iter()
            .map(|ordinal| table.columns[*ordinal].collation)
            .collect();

        let stored: Vec<Vec<Value>> = RowScan::new(&self.storage, &table.name)
            .map(|row| decode_row(&row?.1))
            .collect::<Result<_>>()?;
        for (index, row) in stored.iter().enumerate() {
            for other in &stored[index + 1..] {
                if unique_key_collides(&ordinals, &collations, row, other) {
                    return Err(conflict_error(
                        &table,
                        &Conflict {
                            id: 0,
                            values: other.clone(),
                            columns: ordinals.clone(),
                        },
                    ));
                }
            }
        }

        self.invalidate_rules();
        self.catalog.create_unique_constraint(
            &create.table,
            crate::catalog::UniqueConstraint {
                name: Some(create.name.clone()),
                columns: create.columns.clone(),
            },
        )?;
        self.storage.put_meta(CATALOG_KEY, &self.catalog.encode())?;
        self.end_write()?;
        Ok(Outcome::Ddl)
    }

    /// `DROP INDEX`: remove the declaration, discard the in-memory backend, and
    /// clear the persisted copy so it is not restored on the next open.
    fn drop_index(&mut self, drop: &crate::plan::DropIndexPlan) -> Result<Outcome> {
        // A name may belong to a retrieval index or to a `UNIQUE` constraint
        // that `CREATE UNIQUE INDEX` recorded; in SQLite they share one
        // namespace, so `DROP INDEX` takes either.
        let dropped_constraint = self.catalog.unique_constraint(&drop.name).is_some();
        if dropped_constraint {
            self.invalidate_rules();
            self.catalog.drop_unique_constraint(&drop.name);
        }
        // A `CREATE UNIQUE INDEX` on orderable columns records both halves
        // under the one name, so the constraint above and the index here are
        // the same object and both go.
        let index = match self.catalog.drop_index(&drop.name) {
            Ok(index) => index,
            Err(error) if dropped_constraint => {
                // A unique constraint over a column no B-tree index can cover
                // — a `VECTOR` — has no index half at all.
                let _ = error;
                self.storage.put_meta(CATALOG_KEY, &self.catalog.encode())?;
                self.end_write()?;
                return Ok(Outcome::Ddl);
            }
            Err(error) => return Err(error),
        };
        let key = retrieval_key(&index.table, &index.columns);
        match index.kind {
            IndexKind::FullText => {
                self.text_indexes.remove(&key);
            }
            IndexKind::Vector => {
                self.vector_indexes.remove(&key);
            }
            // The entries are the index. Dropping the declaration without them
            // would leave rows in the tree that a later `CREATE INDEX` of the
            // same name would read as its own — an index describing rows that
            // may no longer exist.
            IndexKind::BTree => {
                self.purge_index_entries(&index)?;
                self.storage.put_meta(CATALOG_KEY, &self.catalog.encode())?;
                self.end_write()?;
                return Ok(Outcome::Ddl);
            }
        }

        // Blank the saved-index header. From here on there is no saved index,
        // so the next open rebuilds — or, since the declaration is gone, does
        // not attempt to build it at all. Stale chunks are left behind; they
        // are unreachable without a header pointing at them.
        self.storage
            .put_meta(&index_meta_key_for(&index.table, &index.columns), &[])?;

        self.storage.put_meta(CATALOG_KEY, &self.catalog.encode())?;
        self.end_write()?;
        Ok(Outcome::Ddl)
    }

    /// Bring every index back up on open: from the saved copy where one is
    /// usable, from the rows where it is not.
    ///
    /// A table is restored as a unit. If any one of its indexes has to be
    /// rebuilt, the table's rows have to be read anyway, so restoring the
    /// others from bytes would save nothing and would leave two code paths
    /// disagreeing about what "up to date" means.
    fn restore_indexes(&mut self) -> Result<()> {
        let tables: Vec<Table> = self.catalog.tables().cloned().collect();
        for table in &tables {
            self.open_indexes_for(table)?;
            if self.load_saved_indexes(table)? {
                continue;
            }
            // A backend that restored itself from the database has to be
            // emptied first, or the rows below would be indexed a second time
            // on top of the copy it just opened.
            self.reset_self_persisting_indexes(table)?;
            for row in scan_all(&self.storage, &table.name)? {
                let (id, bytes) = row;
                let row = decode_row(&bytes)?;
                // Only the retrieval half. A B-tree index needs no rebuild:
                // its entries are durable rows that were written in the same
                // transaction as the rows they describe, so they are already
                // exactly as current as the data — and re-deriving them here
                // would make every open O(rows × indexes) of pointless writes.
                self.index_row_retrieval(table, id, &row)?;
            }
        }
        self.refresh_indexes()
    }

    /// Empty the vector indexes of `table` that keep themselves in the
    /// database, ahead of a rebuild from the rows.
    fn reset_self_persisting_indexes(&mut self, table: &Table) -> Result<()> {
        let declared: Vec<Index> = self
            .catalog
            .indexes_for(&table.name)
            .into_iter()
            .cloned()
            .collect();
        for index in declared {
            if index.kind != IndexKind::Vector {
                continue;
            }
            let key = retrieval_key(&index.table, &index.columns);
            if let Some(backend) = self.vector_indexes.get_mut(&key) {
                if backend.is_self_persisting() {
                    backend.reset()?;
                }
            }
        }
        Ok(())
    }

    /// Try to restore one table's indexes from their saved copies.
    ///
    /// Returns `false` — meaning "rebuild from the rows" — for every reason a
    /// saved copy might not be usable: none was written, it was stamped at a
    /// different write version than the committed data, the backend cannot
    /// restore itself, or the bytes do not decode. None of those is an error:
    /// the rows are the source of truth and the saved copy is only ever a
    /// short cut. That is what makes a torn or half-written index harmless.
    fn load_saved_indexes(&mut self, table: &Table) -> Result<bool> {
        let declared: Vec<Index> = self
            .catalog
            .indexes_for(&table.name)
            .into_iter()
            .cloned()
            .collect();
        let mut restored: Vec<(Index, Vec<u8>)> = Vec::new();
        for index in &declared {
            // A B-tree index is not a backend and has no saved copy: its
            // entries were committed with the rows and are already current.
            // It must not be able to answer "rebuild the table", or every
            // open would rebuild every retrieval index for no reason.
            if index.kind == IndexKind::BTree {
                continue;
            }
            // A backend that keeps itself in the database restored itself when
            // it was opened; there is no blob to read. It is held to the same
            // standard all the same — the stamp it carries has to describe the
            // committed rows, or it is stale and the table is rebuilt.
            if let Some(backend) = self
                .vector_indexes
                .get(&retrieval_key(&index.table, &index.columns))
            {
                if backend.is_self_persisting() {
                    if backend.stored_write_version() == Some(self.write_version) {
                        continue;
                    }
                    return Ok(false);
                }
            }
            let Some(payload) = self.read_saved_index(&index.table, &index.columns)? else {
                return Ok(false);
            };
            restored.push((index.clone(), payload));
        }

        // Nothing declared on this table: trivially "restored".
        if restored.is_empty() {
            return Ok(true);
        }

        // Every blob is present and current — now they have to decode. A
        // failure here abandons the whole table rather than leaving half its
        // indexes loaded and half empty.
        for (index, payload) in restored {
            let key = retrieval_key(&index.table, &index.columns);
            let outcome = match index.kind {
                IndexKind::FullText => self
                    .text_indexes
                    .get_mut(&key)
                    .map(|index| index.load(&payload)),
                IndexKind::Vector => self
                    .vector_indexes
                    .get_mut(&key)
                    .map(|index| index.load(&payload)),
                IndexKind::BTree => unreachable!("filtered out above"),
            };
            match outcome {
                Some(Ok(())) => {}
                _ => {
                    self.open_indexes_for(table)?;
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Read an index's saved backend back, or `None` if there is not a
    /// current, complete one to read.
    fn read_saved_index(&self, table: &str, columns: &[String]) -> Result<Option<Vec<u8>>> {
        let base = index_meta_key_for(table, columns);
        let Some(header) = self.storage.get_meta(&base)? else {
            return Ok(None);
        };
        let Some(header) = IndexHeader::decode(&header) else {
            return Ok(None);
        };
        if header.version != self.write_version {
            return Ok(None);
        }

        let mut payload = Vec::with_capacity(header.length);
        for chunk in 0..header.chunks {
            let Some(bytes) = self.storage.get_meta(&index_chunk_key(&base, chunk))? else {
                return Ok(None);
            };
            payload.extend_from_slice(&bytes);
        }
        // A short read means a chunk went missing between the header being
        // written and now. Treat it as no saved index at all.
        if payload.len() != header.length {
            return Ok(None);
        }
        Ok(Some(payload))
    }

    /// Write every index into the database, stamped with the write version it
    /// reflects.
    ///
    /// Called when enough has changed to be worth the cost, and by
    /// [`Engine::checkpoint`]. A backend that returns `None` from `save` is
    /// skipped and simply gets rebuilt next time.
    fn persist_indexes(&mut self) -> Result<()> {
        let tables: Vec<Table> = self.catalog.tables().cloned().collect();
        let mut saved: Vec<(String, Vec<u8>)> = Vec::new();

        for table in &tables {
            let declared: Vec<Index> = self
                .catalog
                .indexes_for(&table.name)
                .into_iter()
                .cloned()
                .collect();
            for index in declared {
                let key = retrieval_key(&index.table, &index.columns);
                let bytes = match index.kind {
                    IndexKind::FullText => {
                        self.text_indexes.get(&key).and_then(|index| index.save())
                    }
                    IndexKind::Vector => {
                        self.vector_indexes.get(&key).and_then(|index| index.save())
                    }
                    // Its entries are already durable rows; there is nothing
                    // to serialise and nothing that could be stale.
                    IndexKind::BTree => None,
                };
                if let Some(bytes) = bytes {
                    saved.push((index_meta_key_for(&index.table, &index.columns), bytes));
                }
            }
        }

        if saved.is_empty() {
            self.persisted_version = self.write_version;
            return Ok(());
        }

        for (base, bytes) in saved {
            let previous = self
                .storage
                .get_meta(&base)?
                .and_then(|header| IndexHeader::decode(&header))
                .map(|header| header.chunks)
                .unwrap_or(0);

            // Clear the header before touching a single chunk. From here until
            // the header is rewritten there is no saved index, so a crash in
            // the middle can only cost a rebuild.
            self.storage.put_meta(&base, &[])?;
            self.commit_storage()?;

            let chunks = bytes.chunks(INDEX_CHUNK_BYTES);
            let count = chunks.len();
            let mut pending = 0;
            for (number, chunk) in chunks.enumerate() {
                self.storage
                    .put_meta(&index_chunk_key(&base, number), chunk)?;
                pending += chunk.len();
                if self.batch_is_full(pending) {
                    self.commit_storage()?;
                    pending = 0;
                }
            }
            // An index that shrank leaves chunks nothing will ever read. They
            // are harmless — the header bounds the read — but blanking them
            // keeps the file from growing to its own high-water mark forever.
            for number in count..previous {
                self.storage
                    .put_meta(&index_chunk_key(&base, number), &[])?;
                pending += INDEX_CHUNK_BYTES;
                if self.batch_is_full(pending) {
                    self.commit_storage()?;
                    pending = 0;
                }
            }
            self.commit_storage()?;

            // The header last: writing it is what makes the chunks a saved
            // index rather than loose bytes.
            let header = IndexHeader {
                version: self.write_version,
                chunks: count,
                length: bytes.len(),
            };
            self.storage.put_meta(&base, &header.encode())?;
            self.commit_storage()?;
        }

        self.persisted_version = self.write_version;
        Ok(())
    }

    /// Whether the index-saving batch should be committed now.
    ///
    /// Two conditions, and the second one is not an optimisation. Under
    /// copy-on-write, writing `n` bytes of chunks dirties far more than `n`
    /// bytes of pages — every entry copies its whole root-to-leaf path — so a
    /// byte budget measured in *payload* says very little about the size of
    /// the transaction it produces. Measured on a 5,000-document index, 64 KiB
    /// of payload became a 1.1 MiB log record, which does not fit the log and
    /// fails the save outright.
    ///
    /// So the backend is asked as well. A backend with no transaction limit
    /// answers `false` and the payload budget alone decides, exactly as before.
    fn batch_is_full(&self, pending: usize) -> bool {
        pending >= INDEX_COMMIT_BYTES || self.storage.transaction_is_nearly_full()
    }

    /// Commit the graph a self-persisting index just wrote, on a path where the
    /// engine — not the caller — owns the transaction.
    ///
    /// A lost race is not a failure of the read that triggered this. The
    /// storage layer discarded the transaction and
    /// [`Engine::commit_storage`](Self::commit_storage) reloaded this handle
    /// from the winner's state, which rebuilds the indexes from the winner's
    /// rows — so by the time the error would be returned, the engine already
    /// agrees with the database and the read can go ahead. Reporting a conflict
    /// on a `SELECT` would say a statement failed when none did.
    fn commit_index_writes(&mut self) -> Result<()> {
        match self.commit_storage() {
            Ok(()) | Err(Error::Conflict) => Ok(()),
            Err(other) => Err(other),
        }
    }

    /// Save the indexes into the database now, whatever the batching policy
    /// would have decided.
    ///
    /// Worth calling before closing a database that has just been loaded: it
    /// is the difference between the next open being instant and it re-reading
    /// every row.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.in_transaction {
            return Err(Error::Transaction(
                "cannot checkpoint inside a transaction; commit or roll back first".to_string(),
            ));
        }
        self.refresh_indexes()?;
        self.persist_indexes()
    }

    /// Record that a statement changed rows, in the same commit as the change.
    ///
    /// The version counter and the change record are written here rather than
    /// by each statement so that they cannot drift apart: one call, one
    /// version, one record, all landing in the caller's commit.
    fn bump_write_version(&mut self) -> Result<()> {
        // A statement that matched nothing changed nothing. Not bumping keeps
        // a persisted index valid across a `DELETE` that deleted no rows, and
        // keeps the change log free of empty entries.
        if self.pending_changes.is_empty() {
            return Ok(());
        }

        self.write_version += 1;
        self.storage
            .put_meta(WRITE_VERSION_KEY, &self.write_version.to_le_bytes())?;

        let entries = core::mem::take(&mut self.pending_changes);
        self.storage.put_meta(
            &cdc::record_key(self.write_version),
            &cdc::encode_record(&entries),
        )?;
        self.trim_changes()
    }

    /// Drop change records that have fallen out of the retention window, in
    /// batches of [`cdc::CDC_TRIM_BATCH`] rather than one per commit.
    ///
    /// There is no `delete` on the metadata surface, so an expired record is
    /// overwritten with nothing. `cdc_floor` is what makes that
    /// distinguishable from "this statement changed nothing": a reader
    /// comparing its position against the floor learns it fell behind instead
    /// of silently receiving a short list.
    ///
    /// # Why batched (AHL-480)
    ///
    /// The oldest surviving `cdc:` key sits at the opposite end of the
    /// retained range from everything else a commit writes — the row, the
    /// `next_row_id`/`write_version` counters and the newest `cdc:` entry are
    /// one adjacent, usually page_slot-shared cluster; the expiring key is a
    /// distant leaf of its own. Expiring exactly one entry every commit meant
    /// every commit past the retention window paid a third copy-on-write
    /// path purely to keep the log's *lower* bound tight, which a profile of
    /// the durable-commit loop found real (see `cdc::CDC_TRIM_BATCH`'s doc
    /// comment). Waiting until a whole batch has expired, then dropping it in
    /// one commit, pays that distant path once every
    /// [`cdc::CDC_TRIM_BATCH`] commits instead of every one — the log is
    /// simply allowed to run up to that many entries past `CDC_RETENTION`
    /// before the trim catches up, which is still a bound, just a slightly
    /// looser one, and changes nothing else about the retained *contents*
    /// (every version at or after `cdc_floor + 1` is still there, still in
    /// commit order).
    fn trim_changes(&mut self) -> Result<()> {
        let Some(expired) = self.write_version.checked_sub(CDC_RETENTION) else {
            return Ok(());
        };
        if expired < self.cdc_floor + cdc::CDC_TRIM_BATCH {
            return Ok(());
        }
        for version in (self.cdc_floor + 1)..=expired {
            self.storage.put_meta(&cdc::record_key(version), &[])?;
        }
        self.cdc_floor = expired;
        self.storage
            .put_meta(CDC_FLOOR_KEY, &self.cdc_floor.to_le_bytes())
    }

    /// Note that a row changed, for the change record this statement will
    /// write when it commits.
    fn note_change(&mut self, table: &str, id: RowId, kind: ChangeKind) {
        self.pending_changes.push((table.to_string(), id, kind));
    }

    /// Committed row changes after `from`, in commit order.
    ///
    /// Pass `0` to start from the beginning of the retained log, or the
    /// [`Changes::version`] from the previous call to continue. Check
    /// [`Changes::lost`] before trusting the result: a consumer that has been
    /// away longer than the retention window has to resynchronise from a scan.
    ///
    /// Records say *what* changed, not what it became — see [`crate::cdc`] for
    /// why.
    pub fn changes(&self, from: u64) -> Result<Changes> {
        let mut changes = Vec::new();
        // Versions are dense — one per statement that changed a row — so the
        // log can be read by counting rather than by scanning keys, which the
        // `Storage` surface deliberately does not offer.
        for version in (from.max(self.cdc_floor) + 1)..=self.write_version {
            let Some(bytes) = self.storage.get_meta(&cdc::record_key(version))? else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            changes.extend(cdc::decode_record(version, &bytes)?);
        }
        Ok(Changes {
            changes,
            version: self.write_version,
            floor: self.cdc_floor,
        })
    }

    /// The current change version, for a consumer that wants to start from
    /// "now" rather than replay history.
    pub fn change_version(&self) -> u64 {
        self.write_version
    }

    /// Make pending index writes searchable.
    ///
    /// Index commits are deferred to the first read that needs them rather
    /// than run at the end of every write. Deferring keeps writes cheap and
    /// pays the cost once per read-after-write; the vector index maintains its
    /// graph incrementally on commit (AHL-381), so the first read after a load
    /// pays one full build and every later read only reconciles the rows that
    /// changed since.
    fn refresh_indexes(&mut self) -> Result<()> {
        if !self.indexes_dirty {
            return Ok(());
        }
        for index in self.text_indexes.values_mut() {
            index.commit()?;
        }
        // A self-persisting index is told two things before it commits: which
        // write version its graph will describe, and whether it may make its
        // own writes durable. It may not inside a caller's transaction — the
        // caller's rows are buffered in the same one.
        let write_version = self.write_version;
        let may_commit = !self.in_transaction;
        let mut wrote_to_storage = false;
        for index in self.vector_indexes.values_mut() {
            if index.is_self_persisting() {
                index.prepare_commit(write_version, may_commit);
                wrote_to_storage = true;
            }
            index.commit()?;
        }
        self.indexes_dirty = false;

        // A self-persisting index has just written its graph into the open
        // transaction and, by design, did not commit it. Someone has to, or the
        // graph a later open would find is the one from before these rows.
        // Inside a transaction that someone is the caller's `commit`, which is
        // exactly what makes the rows and the index atomic; outside one it is
        // here, at the read that asked for the refresh.
        if wrote_to_storage && !self.in_transaction {
            self.commit_index_writes()?;
        }

        // Now that the indexes agree with the rows, this is the only moment at
        // which saving them is meaningful. Doing it here rather than after
        // every write also means a bulk load pays for at most one save.
        //
        // Skipped inside an open transaction: saving commits the storage
        // transaction, which would make a transaction's buffered writes durable
        // before its own `commit`. The save happens at the first read after the
        // transaction instead.
        if !self.in_transaction
            && self.write_version.saturating_sub(self.persisted_version) >= INDEX_PERSIST_INTERVAL
        {
            self.persist_indexes()?;
        }
        Ok(())
    }

    // --------------------------------------------------------------- INSERT

    fn insert(&mut self, insert: &InsertPlan, params: &[Value]) -> Result<Outcome> {
        let table = self.catalog.require_table(&insert.table)?.clone();
        let rules = self.rules_for(&table)?;
        let alias = table.rowid_alias();
        let env = self.env(params);

        // Every row is built before any is written, so an expression that
        // fails — a bad `?`, a vector of the wrong dimension — cannot leave
        // half a statement behind.
        let proposed = self.proposed_rows(insert, &table, &rules, params, &env)?;

        let mut written = 0usize;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        for mut row in proposed {
            // `assigned` is per row, not per statement: a multi-row `INSERT`
            // may name some keys and leave others to the engine, and only the
            // engine-chosen ones are what `last_insert_rowid()` is asking
            // about.
            let (id, assigned) = match alias {
                Some(ordinal) => self.rowid_for(&table, &mut row, ordinal)?,
                None => (self.next_row_id, true),
            };
            // The key is reserved as soon as it is *resolved*, not when the row
            // is written — a row a conflict clause goes on to skip has still
            // used up its key. SQLite's `sqlite_sequence` behaves exactly this
            // way, and the difference is visible: an `INSERT OR IGNORE` that
            // skips one row makes the next assigned key skip a number too. A
            // statement that *fails* keeps nothing, because the counter is
            // re-read from the committed store when its writes are discarded.
            self.reserve_row_id(id);

            // `NOT NULL` and `CHECK` come before uniqueness, as they do in
            // SQLite, and the order shows: a row that fails a `CHECK` *and*
            // collides reports the `CHECK`, and an `ON CONFLICT DO NOTHING`
            // does not absorb it.
            if !self.apply_constraints(&table, &rules, &mut row, &insert.on_conflict, &env)? {
                continue;
            }

            let conflicts = self.conflicting_rows(&table, &rules, id, &row)?;
            if !conflicts.is_empty() {
                // Which conflict, if any, the clause answers for. A target
                // narrows it to one constraint; the others are then not the
                // clause's business at all — SQLite neither acts on them nor
                // raises for them, as long as the targeted one is present.
                let answered = match &insert.on_conflict.target {
                    None => Some(0),
                    Some(target) => conflicts
                        .iter()
                        .position(|conflict| same_columns(&conflict.columns, target)),
                };
                match (&insert.on_conflict.action, answered) {
                    // Nothing the clause covers collided, so whatever did is an
                    // ordinary violation — and so is any conflict at all under
                    // the default policy.
                    (_, None) => return Err(conflict_error(&table, &conflicts[0])),
                    (ConflictAction::Abort, _) => {
                        return Err(conflict_error(&table, &conflicts[0]))
                    }
                    // SQLite does not count a row it skipped, and does not
                    // report its key as the last inserted one.
                    (ConflictAction::Ignore, Some(_)) => continue,
                    (ConflictAction::Replace, Some(_)) => {
                        let mut deleted: Vec<RowId> = Vec::new();
                        for conflict in &conflicts {
                            // A row can collide on two constraints at once and
                            // appear twice; deleting it twice would de-index it
                            // twice.
                            if deleted.contains(&conflict.id) {
                                continue;
                            }
                            deleted.push(conflict.id);
                            self.remove_btree_entries(&table, conflict.id, &conflict.values)?;
                            self.storage.delete_row(&table.name, conflict.id)?;
                            self.deindex_row_retrieval(&table, conflict.id, &conflict.values)?;
                            self.note_change(&table.name, conflict.id, ChangeKind::Delete);
                        }
                    }
                    (ConflictAction::Update(update), Some(index)) => {
                        let conflict = &conflicts[index];
                        if let Some(next) =
                            self.upsert_row(&table, &rules, update, &conflict.values, &row, &env)?
                        {
                            let existing = conflict.id;
                            let old = conflict.values.clone();
                            self.ensure_unique(&table, &rules, existing, &next)?;
                            let id = self.write_changed_row(&table, existing, &old, next)?;
                            written += 1;
                            if let Some(items) = &insert.returning {
                                returned.push(self.project_stored(&table, id, items, &env)?);
                            }
                        }
                        continue;
                    }
                }
            }

            self.storage
                .put_row(&table.name, id, &encode_table_row(&table, &row))?;
            // Only after the row is in the transaction, and only when the key
            // came from the counter: a caller reading this back is asking what
            // key it did not supply, and a row that failed to be written has no
            // key to report.
            if assigned {
                self.last_insert_row_id = Some(id);
            }
            self.index_row(&table, id, &row)?;
            self.note_change(&table.name, id, ChangeKind::Insert);
            written += 1;
            if let Some(items) = &insert.returning {
                let exec = ExecRow {
                    id,
                    score: None,
                    values: row,
                    aggregates: Vec::new(),
                    windows: Vec::new(),
                };
                returned.push(project_row(items, &exec, &env)?);
            }
        }

        self.end_write()?;
        match &insert.returning {
            Some(items) => Ok(Outcome::Rows(ResultSet {
                columns: items.iter().map(|item| item.label().to_string()).collect(),
                rows: returned,
            })),
            None => Ok(Outcome::Written(written)),
        }
    }

    /// Build every row an `INSERT` proposes, from its `VALUES` or its `SELECT`,
    /// with each column the statement did not name filled from its `DEFAULT`.
    fn proposed_rows(
        &mut self,
        insert: &InsertPlan,
        table: &Table,
        rules: &TableRules,
        params: &[Value],
        env: &Env<'_>,
    ) -> Result<Vec<Vec<Value>>> {
        let fill = |ordinal: usize, cell: Option<&crate::plan::Expr>| -> Result<Value> {
            // The distinction the plan keeps: a column the statement named and
            // set to `NULL` is `NULL`; a column it never named takes its
            // default, and `NULL` only when there is none.
            let expr = match cell {
                Some(expr) => Some(expr),
                None => rules.defaults[ordinal].as_ref(),
            };
            let value = match expr {
                Some(expr) => eval::evaluate(expr, &[], Computed::NONE, env)?,
                None => Value::Null,
            };
            sql::coerce(value, &table.columns[ordinal])
        };

        match &insert.source {
            InsertSource::Values(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for cells in rows {
                    let mut row = Vec::with_capacity(cells.len());
                    for (ordinal, cell) in cells.iter().enumerate() {
                        row.push(fill(ordinal, cell.as_ref())?);
                    }
                    out.push(row);
                }
                Ok(out)
            }
            InsertSource::Select { query, targets } => {
                // The query runs to completion first. That is not only
                // simplest: `INSERT INTO t SELECT ... FROM t` must read the
                // table as it was, not as the insert is making it. `query`
                // is a `SELECT` or (since AHL-473) a compound; never a
                // `Scalar`, which `sql::plan_insert` already refused.
                let result = self.select_body(query, params)?;
                let mut out = Vec::with_capacity(result.rows.len());
                for values in result.rows {
                    let mut row = Vec::with_capacity(table.columns.len());
                    for ordinal in 0..table.columns.len() {
                        let supplied = targets
                            .iter()
                            .position(|target| *target == ordinal)
                            .map(|index| values[index].clone());
                        row.push(match supplied {
                            Some(value) => sql::coerce(value, &table.columns[ordinal])?,
                            None => fill(ordinal, None)?,
                        });
                    }
                    out.push(row);
                }
                Ok(out)
            }
        }
    }

    /// The row id a table whose `INTEGER PRIMARY KEY` aliases it will use, and
    /// whether the engine chose it.
    ///
    /// The supplied value becomes the storage key, so the row is reachable by
    /// a single tree descent. A `NULL` key is filled in from the counter and
    /// written back into the row, which is what makes
    /// `INSERT INTO t (body) VALUES (?)` behave the way SQLite's users expect.
    /// That filled-in case is the `true` in the returned pair, and the only one
    /// [`Engine::last_insert_row_id`] reports.
    fn rowid_for(
        &mut self,
        table: &Table,
        row: &mut [Value],
        ordinal: usize,
    ) -> Result<(RowId, bool)> {
        match row.get(ordinal) {
            Some(Value::Integer(key)) => {
                RowId::try_from(*key).map(|id| (id, false)).map_err(|_| {
                    // Row keys are big-endian unsigned, so a negative key would
                    // sort after every positive one and quietly break scan
                    // order. Rejecting it beats silently reordering the table.
                    Error::Unsupported(alloc::format!(
                        "an INTEGER PRIMARY KEY must be positive; got {key}"
                    ))
                })
            }
            Some(Value::Null) | None => {
                let id = self.next_row_id;
                if let Some(slot) = row.get_mut(ordinal) {
                    *slot = Value::Integer(id as i64);
                }
                Ok((id, true))
            }
            Some(other) => {
                let _ = table;
                Err(Error::Type(alloc::format!(
                    "an INTEGER PRIMARY KEY must be an integer; got {other:?}"
                )))
            }
        }
    }

    /// Keep the counter ahead of every key in use, so a later `NULL` key
    /// cannot collide with a row that was inserted explicitly.
    ///
    /// It only ever moves forward, which is exactly what SQLite's
    /// `AUTOINCREMENT` guarantees and what this engine gives without being
    /// asked: deleting the highest row does not hand its key out again.
    fn reserve_row_id(&mut self, id: RowId) {
        self.next_row_id = self.next_row_id.max(id.saturating_add(1));
    }

    /// Every stored row a proposed row would collide with, each tagged with the
    /// constraint it collided on.
    ///
    /// The order is SQLite's index order — the row id first, then each `UNIQUE`
    /// in declaration order — because an untargeted `DO UPDATE` acts on the
    /// first one, and "first" has to mean the same thing in both engines.
    ///
    /// Each `UNIQUE` group is answered by a B-tree index probe when one covers
    /// it, and by a full scan when none does — see
    /// [`Engine::colliding_rows`]. The error, the ordering and the set of
    /// conflicts are identical either way; only the cost changes. A table with
    /// no `UNIQUE` constraint pays nothing: the row-id lookup below is a single
    /// tree descent, exactly as before.
    fn conflicting_rows(
        &self,
        table: &Table,
        rules: &TableRules,
        id: RowId,
        row: &[Value],
    ) -> Result<Vec<Conflict>> {
        let mut found: Vec<Conflict> = Vec::new();
        if let Some(bytes) = self.storage.get_row(&table.name, id)? {
            found.push(Conflict {
                id,
                values: decode_row(&bytes)?,
                columns: table.rowid_alias().into_iter().collect(),
            });
        }
        // No scan here, and none below: `colliding_rows` reaches for an index
        // when one covers the group and only falls back to a scan when none
        // does, so a table with no `UNIQUE` constraint iterates nothing at all.
        for group in &rules.unique {
            // The row that already holds this key is not skipped even when
            // the row-id conflict above already found it: one stored row
            // can violate two constraints at once, and an `ON CONFLICT (e)`
            // has to be able to find the `e` one. It appears twice, tagged
            // differently, which is exactly what the target matches on.
            for (existing, values) in self.colliding_rows(table, group, row, None)? {
                found.push(Conflict {
                    id: existing,
                    values,
                    columns: group.clone(),
                });
            }
        }
        Ok(found)
    }

    /// Check a row against every `UNIQUE` constraint, excluding one row id.
    ///
    /// The exclusion is what makes it usable for a row being *changed*: a row
    /// always collides with itself, and that is not a violation.
    fn ensure_unique(
        &self,
        table: &Table,
        rules: &TableRules,
        id: RowId,
        row: &[Value],
    ) -> Result<()> {
        for group in &rules.unique {
            if let Some((existing, values)) = self
                .colliding_rows(table, group, row, Some(id))?
                .into_iter()
                .next()
            {
                return Err(conflict_error(
                    table,
                    &Conflict {
                        id: existing,
                        values,
                        columns: group.clone(),
                    },
                ));
            }
        }
        Ok(())
    }

    /// Apply an `ON CONFLICT DO UPDATE` to the row that was already there.
    ///
    /// `None` means the clause's `WHERE` excluded it, which in SQLite leaves
    /// the stored row exactly as it was — it does not fall back to inserting.
    fn upsert_row(
        &self,
        table: &Table,
        rules: &TableRules,
        update: &ConflictUpdate,
        existing: &[Value],
        proposed: &[Value],
        env: &Env<'_>,
    ) -> Result<Option<Vec<Value>>> {
        // The pair the plan resolved its ordinals against: the stored row,
        // then the proposed one under the name `excluded`.
        let mut pair = Vec::with_capacity(existing.len() + proposed.len());
        pair.extend_from_slice(existing);
        pair.extend_from_slice(proposed);

        if let Some(filter) = &update.filter {
            if !eval::is_truthy(&eval::evaluate(filter, &pair, Computed::NONE, env)?) {
                return Ok(None);
            }
        }
        let mut next = existing.to_vec();
        next.resize(table.columns.len(), Value::Null);
        for (ordinal, expr) in &update.assignments {
            next[*ordinal] = sql::coerce(
                eval::evaluate(expr, &pair, Computed::NONE, env)?,
                &table.columns[*ordinal],
            )?;
        }
        self.apply_constraints(table, rules, &mut next, &OnConflict::abort(), env)?;
        Ok(Some(next))
    }

    /// Write a row that replaces one already stored, moving it when its
    /// `INTEGER PRIMARY KEY` changed.
    ///
    /// The key *is* the storage key, so changing the column has to move the
    /// row. Writing it back under the old key would leave the stored key and
    /// the column disagreeing — a row that `WHERE id = 5` cannot find and
    /// `SELECT id` reports as 5.
    fn write_changed_row(
        &mut self,
        table: &Table,
        id: RowId,
        old: &[Value],
        next: Vec<Value>,
    ) -> Result<RowId> {
        let moved = match table.rowid_alias() {
            Some(ordinal) => match next.get(ordinal) {
                Some(Value::Integer(key)) => RowId::try_from(*key).map_err(|_| {
                    Error::Unsupported(alloc::format!(
                        "an INTEGER PRIMARY KEY must be positive; got {key}"
                    ))
                })?,
                // A `NULL` written over the key keeps the row where it is,
                // as SQLite does for an `UPDATE`.
                _ => id,
            },
            None => id,
        };
        if moved != id && self.storage.get_row(&table.name, moved)?.is_some() {
            return Err(Error::Constraint(alloc::format!(
                "UNIQUE constraint failed: {}.{}",
                table.name,
                table
                    .rowid_alias()
                    .map_or("rowid", |o| &table.columns[o].name)
            )));
        }

        // Every storage write this row needs, with nothing between them that
        // could commit — see [`Engine::write_btree_entries`]. The row and its
        // entries move together or not at all.
        self.remove_btree_entries(table, id, old)?;
        if moved != id {
            self.storage.delete_row(&table.name, id)?;
        }
        self.storage
            .put_row(&table.name, moved, &encode_table_row(table, &next))?;
        self.write_btree_entries(table, moved, &next)?;

        // Then the retrieval backends, which may commit whatever is buffered.
        self.deindex_row_retrieval(table, id, old)?;
        self.index_row_retrieval(table, moved, &next)?;

        if moved != id {
            self.note_change(&table.name, id, ChangeKind::Delete);
            self.reserve_row_id(moved);
        }
        self.note_change(&table.name, moved, ChangeKind::Update);
        Ok(moved)
    }

    /// Project a `RETURNING` clause over a row read back from storage.
    fn project_stored(
        &self,
        table: &Table,
        id: RowId,
        items: &[SelectItem],
        env: &Env<'_>,
    ) -> Result<Vec<Value>> {
        let values = match self.storage.get_row(&table.name, id)? {
            Some(bytes) => decode_row(&bytes)?,
            None => Vec::new(),
        };
        project_row(
            items,
            &ExecRow {
                id,
                score: None,
                values,
                aggregates: Vec::new(),
                windows: Vec::new(),
            },
            env,
        )
    }

    /// Check the constraints a row has to satisfy on its own — `NOT NULL` and
    /// `CHECK` — applying the conflict policy that covers them.
    ///
    /// `UNIQUE` is not here: it is a question about the *other* rows, answered
    /// by [`Engine::conflicting_rows`], because what to do about a collision is
    /// the `INSERT`'s decision and not a flat refusal.
    ///
    /// `false` means the row is to be skipped, which only `INSERT OR IGNORE`
    /// can ask for. The order — `NOT NULL`, then `CHECK`, then uniqueness at
    /// the caller — is SQLite's `sqlite3GenerateConstraintChecks`, and it is
    /// observable: a row that both fails a `CHECK` and collides reports the
    /// `CHECK`.
    fn apply_constraints(
        &self,
        table: &Table,
        rules: &TableRules,
        row: &mut [Value],
        policy: &OnConflict,
        env: &Env<'_>,
    ) -> Result<bool> {
        let covers = policy.covers_every_constraint;
        for ordinal in 0..table.columns.len() {
            if !table.columns[ordinal].not_null {
                continue;
            }
            if !matches!(row.get(ordinal), None | Some(Value::Null)) {
                continue;
            }
            // SQLite's `REPLACE` on a `NOT NULL` column does not replace a
            // *row*: it replaces the `NULL` with the column's default, and only
            // aborts when there is no usable one.
            if covers && matches!(policy.action, ConflictAction::Replace) {
                if let Some(expr) = &rules.defaults[ordinal] {
                    let value = sql::coerce(
                        eval::evaluate(expr, &[], Computed::NONE, env)?,
                        &table.columns[ordinal],
                    )?;
                    if value != Value::Null {
                        row[ordinal] = value;
                        continue;
                    }
                }
            }
            if covers && matches!(policy.action, ConflictAction::Ignore) {
                return Ok(false);
            }
            return Err(Error::Constraint(alloc::format!(
                "NOT NULL constraint failed: {}.{}",
                table.name,
                table.columns[ordinal].name
            )));
        }

        for (text, expr) in &rules.checks {
            let value = eval::evaluate(expr, row, Computed::NONE, env)?;
            // SQLite's rule, and it is the one people get wrong: a `CHECK`
            // fails only when it is *false*. `NULL` — which is what any check
            // over a `NULL` column yields — passes.
            if value == Value::Null || eval::is_truthy(&value) {
                continue;
            }
            if covers && matches!(policy.action, ConflictAction::Ignore) {
                return Ok(false);
            }
            return Err(Error::Constraint(alloc::format!(
                "CHECK constraint failed: {text}"
            )));
        }
        Ok(true)
    }

    /// The rows a statement has to consider, as a stream.
    ///
    /// The three access paths this engine has, in the order they are tried:
    ///
    /// * **Point.** A filter that pins the row id (`WHERE id = 42` on an
    ///   `INTEGER PRIMARY KEY`) collapses to one tree descent and at most one
    ///   row.
    /// * **Indexed.** A filter a scalar B-tree index can answer an equality or
    ///   a range from (AHL-423) becomes an index range probe followed by one
    ///   descent per surviving row id. The probe itself is materialised — it is
    ///   a run of `\x01idx:` keys, not rows — but the *rows* are fetched one at
    ///   a time, so a `LIMIT` over an indexed filter still decodes only the
    ///   rows it hands out.
    /// * **Scan.** Everything else streams the table in row-id order.
    ///
    /// The filter is still evaluated over every row all three yield, so this is
    /// purely a matter of how many rows are read — never of which ones match.
    /// That is the contract [`pinned_rowid`] and [`Engine::indexed_candidates`]
    /// share, and it is why choosing badly here is slow rather than wrong.
    fn candidate_bytes(
        &self,
        table: &Table,
        filter: &Option<crate::plan::Expr>,
        params: &[Value],
    ) -> Result<RowBytes<'_>> {
        if let Some(id) = pinned_rowid(table, filter.as_ref(), params) {
            return Ok(RowBytes::Point(
                self.storage
                    .get_row(&table.name, id)?
                    .map(|bytes| (id, bytes)),
            ));
        }
        if let Some(ids) = self.indexed_candidates(table, filter.as_ref(), params)? {
            return Ok(RowBytes::indexed(&self.storage, &table.name, ids));
        }
        Ok(RowBytes::Scan(RowScan::new(&self.storage, &table.name)))
    }

    /// The rows a *write* statement has to consider, materialised.
    ///
    /// `UPDATE` and `DELETE` read the rows they are about to change, and
    /// SQLite's rule is that the statement sees the table as it stood when it
    /// began — `UPDATE t SET n = n + 1 WHERE n < 10` must not re-visit a row it
    /// has already raised. Reading the candidates into a `Vec` first is what
    /// guarantees that, and it is why the streaming path above is for readers
    /// only. It is also what the borrow checker asks for: the loop that writes
    /// holds `&mut self`.
    fn candidate_rows(
        &self,
        table: &Table,
        filter: &Option<crate::plan::Expr>,
        params: &[Value],
    ) -> Result<Vec<(RowId, RowBuf)>> {
        // The same three access paths a reader gets — point, index probe,
        // scan — drained into a `Vec` up front rather than pulled row by row.
        self.candidate_bytes(table, filter, params)?.collect()
    }

    /// The rows a scalar B-tree index narrows a filter down to, or `None` when
    /// no index applies and the caller has to scan.
    ///
    /// This is a **rule, not a cost model** (`docs/architecture.md`, D6): the most
    /// constrained applicable index wins, and if that turns out to be a bad
    /// choice it is still a correct one, because the caller re-evaluates the
    /// whole `WHERE` over every row this returns. An index here can only
    /// change *how many rows are read*, never which ones match — the same
    /// contract [`pinned_rowid`] has always had.
    fn indexed_candidates(
        &self,
        table: &Table,
        filter: Option<&crate::plan::Expr>,
        params: &[Value],
    ) -> Result<Option<Vec<RowId>>> {
        let filter = match filter {
            Some(filter) => filter,
            None => return Ok(None),
        };
        // Nothing to choose between, and nothing to pay for: a table with no
        // B-tree index must not make every query walk its filter looking for
        // one.
        let candidates: Vec<&Index> = self
            .catalog
            .indexes_for(&table.name)
            .into_iter()
            .filter(|index| index.kind == IndexKind::BTree)
            .collect();
        if candidates.is_empty() {
            return Ok(None);
        }
        let mut terms = Vec::new();
        collect_conjuncts(filter, params, table, &mut terms);
        if terms.is_empty() {
            return Ok(None);
        }

        let mut best: Option<(usize, crate::index::KeyRange)> = None;
        for index in candidates {
            let Some((bound_columns, range)) = index_probe(table, index, &terms)? else {
                continue;
            };
            // More bound columns is a narrower scan; ties keep the first,
            // which is index-name order and therefore deterministic.
            if best.as_ref().is_none_or(|(best, _)| bound_columns > *best) {
                best = Some((bound_columns, range));
            }
        }
        let Some((_, range)) = best else {
            return Ok(None);
        };

        // `scan_index_row_ids` (`AHL-479`) rather than `scan_index_range` plus
        // a decode of each key by hand: every one of these entries is read
        // only to throw its key away once the row id is off the end of it,
        // which is exactly what the row-id-only walk skips the allocation and
        // the value resolution for.
        let mut ids = self
            .storage
            .scan_index_row_ids(&range.start, range.end.as_deref())?;
        // Entries are ordered by value and only then by row id, so a range
        // covering more than one value is not in row-id order. Callers — and
        // `ORDER BY`-less results — expect row-id order, and a duplicate can
        // never appear because one row contributes one entry per index.
        ids.sort_unstable();
        Ok(Some(ids))
    }

    /// Every stored row that shares one `UNIQUE` group's key with `row`,
    /// excluding `exclude`.
    ///
    /// Answered from the B-tree index when one covers exactly that group, and
    /// by a full scan when none does. The two paths agree by construction: the
    /// index narrows the candidates and [`unique_key_collides`] — the same
    /// function, on the same stored values — decides every one of them.
    fn colliding_rows(
        &self,
        table: &Table,
        group: &[usize],
        row: &[Value],
        exclude: Option<RowId>,
    ) -> Result<Vec<(RowId, Vec<Value>)>> {
        // SQLite's rule: a `NULL` never collides with anything, so a group
        // with one is not a candidate for collision at all and needs no
        // lookup.
        if group
            .iter()
            .any(|ordinal| row.get(*ordinal).unwrap_or(&NULL) == &Value::Null)
        {
            return Ok(Vec::new());
        }

        let names: Vec<String> = group
            .iter()
            .filter_map(|ordinal| table.columns.get(*ordinal))
            .map(|column| column.name.to_ascii_lowercase())
            .collect();
        // A `UNIQUE` group's keys are its columns' declared collations, so only
        // an index under exactly those can answer it with a probe. Any other
        // index (a `BINARY` one beside a `NOCASE` column, say) would look in
        // the wrong run of bytes; the scan below is the fallback, and
        // `unique_key_collides` gives the same verdict either way.
        let collations: Vec<Collation> = group
            .iter()
            .filter_map(|ordinal| table.columns.get(*ordinal))
            .map(|column| column.collation)
            .collect();
        let candidates: Vec<(RowId, RowBuf)> = match self
            .catalog
            .btree_index_on(&table.name, &names, &collations)
            .filter(|_| names.len() == group.len())
        {
            Some(index) => {
                let values = index_values(table, index, row)?;
                let range =
                    crate::index::KeyRange::equality(&index.name, &values, &index.collations)?;
                let keys = self
                    .storage
                    .scan_index_range(&range.start, range.end.as_deref())?;
                let mut rows = Vec::with_capacity(keys.len());
                for key in &keys {
                    let id = crate::index::row_id_from_entry(key)?;
                    if let Some(bytes) = self.storage.get_row(&table.name, id)? {
                        rows.push((id, bytes));
                    }
                }
                rows
            }
            // No index covers this group, so the only way to know is to look
            // at every row. This is the O(rows)-per-write cost the constraint
            // used to have unconditionally.
            None => scan_all(&self.storage, &table.name)?,
        };

        let mut found = Vec::new();
        for (id, bytes) in candidates {
            if Some(id) == exclude {
                continue;
            }
            let values = decode_row(&bytes)?;
            if unique_key_collides(group, &collations, row, &values) {
                found.push((id, values));
            }
        }
        found.sort_by_key(|(id, _)| *id);
        Ok(found)
    }

    /// Add a row to every index the table declares.
    ///
    /// The B-tree entries are written through [`Storage::put_index_entry`],
    /// which means they join the statement's transaction beside the row
    /// itself. That is the crash-safety property in one sentence: there is no
    /// moment at which the row is durable and its index entry is not, because
    /// they reach the log in the same record.
    fn index_row(&mut self, table: &Table, id: RowId, row: &[Value]) -> Result<()> {
        self.write_btree_entries(table, id, row)?;
        self.index_row_retrieval(table, id, row)
    }

    /// Write this row's B-tree entries, and nothing else.
    ///
    /// **Every storage write that belongs to one row has to happen before any
    /// retrieval backend is touched, and this is why.** A self-persisting ANN
    /// backend shares the engine's storage handle and, once
    /// [`VectorIndex::prepare_commit`] has told it that it may, commits inside
    /// its own `insert` in order to break a large graph build into
    /// transactions that fit the log. A commit that lands between the row and
    /// its index entries makes one durable without the other, and the DST
    /// sweep found exactly that: sixteen rows and eight entries in the same
    /// recovered database. A B-tree index has no write-version stamp to catch
    /// it with — the entries *are* the index — so the fix is that there is
    /// never a moment between them.
    fn write_btree_entries(&mut self, table: &Table, id: RowId, row: &[Value]) -> Result<()> {
        for key in self.btree_entry_keys(table, id, row)? {
            self.storage.put_index_entry(&key)?;
        }
        Ok(())
    }

    /// Remove this row's B-tree entries, and nothing else. See
    /// [`Engine::write_btree_entries`] for why it is separable.
    fn remove_btree_entries(&mut self, table: &Table, id: RowId, row: &[Value]) -> Result<()> {
        for key in self.btree_entry_keys(table, id, row)? {
            self.storage.delete_index_entry(&key)?;
        }
        Ok(())
    }

    /// The retrieval half alone: the BM25 and ANN backends, which are rebuilt
    /// from the rows on open and so are the only half a rebuild has to redo.
    ///
    /// Walks the *declared indexes*, not the table's columns: a single column
    /// can now be named by more than one `FullText` index at once (a
    /// single-column `(body)` index and a multi-column `(title, body)` one
    /// can coexist — see `Catalog::create_index`'s dup-check), so "one index
    /// per column" is no longer the shape to iterate, "one index" is.
    fn index_row_retrieval(&mut self, table: &Table, id: RowId, row: &[Value]) -> Result<()> {
        self.indexes_dirty = true;
        let declared: Vec<Index> = self
            .catalog
            .indexes_for(&table.name)
            .into_iter()
            .filter(|index| index.kind.is_retrieval())
            .cloned()
            .collect();
        for index in &declared {
            self.index_row_for_index(table, index, id, row)?;
        }
        Ok(())
    }

    /// The key this row contributes to each B-tree index of `table`.
    ///
    /// Every row contributes exactly one entry per index, `NULL`s included, so
    /// "one entry per row per index" is an invariant a test can check — and
    /// the DST sweep does.
    fn btree_entry_keys(
        &self,
        table: &Table,
        id: RowId,
        row: &[Value],
    ) -> Result<Vec<alloc::vec::Vec<u8>>> {
        let mut keys = Vec::new();
        for index in self.catalog.indexes_for(&table.name) {
            if index.kind != IndexKind::BTree {
                continue;
            }
            let values = index_values(table, index, row)?;
            keys.push(crate::index::entry_key(
                &index.name,
                &values,
                &index.collations,
                id,
            )?);
        }
        Ok(keys)
    }

    /// Add a row's contribution to one declared retrieval index, if its
    /// backend is open.
    ///
    /// For `FullText` this is [`concatenated_full_text`] over every named
    /// column — MySQL's `FULLTEXT(a, b)`: one combined relevance score over
    /// the concatenation, so a query term matching either column ranks the
    /// row. For `Vector` it is the one named column's embedding, unchanged
    /// from before this existed — a vector index is always exactly one
    /// column (see `IndexKind::Vector`'s docs).
    fn index_row_for_index(
        &mut self,
        table: &Table,
        index: &Index,
        id: RowId,
        row: &[Value],
    ) -> Result<()> {
        let key = retrieval_key(&index.table, &index.columns);
        match index.kind {
            IndexKind::FullText => {
                if let Some(text) = concatenated_full_text(table, &index.columns, row)? {
                    if let Some(backend) = self.text_indexes.get_mut(&key) {
                        backend.insert(id, &text)?;
                    }
                }
            }
            IndexKind::Vector => {
                let (ordinal, _) = table.require_column(index.column())?;
                if let Some(Value::Vector(embedding)) = row.get(ordinal) {
                    if let Some(backend) = self.vector_indexes.get_mut(&key) {
                        backend.insert(id, embedding)?;
                    }
                }
            }
            IndexKind::BTree => {}
        }
        Ok(())
    }

    /// The retrieval half of removing a row from its indexes.
    ///
    /// There is no whole-row counterpart, deliberately: every caller has to
    /// interleave the storage half with its own row write, in the order
    /// [`Engine::write_btree_entries`] explains, and a convenience wrapper
    /// that hid the ordering is what made the DST sweep fail once already.
    fn deindex_row_retrieval(&mut self, table: &Table, id: RowId, row: &[Value]) -> Result<()> {
        self.indexes_dirty = true;
        let declared: Vec<Index> = self
            .catalog
            .indexes_for(&table.name)
            .into_iter()
            .filter(|index| index.kind.is_retrieval())
            .cloned()
            .collect();
        for index in &declared {
            self.deindex_row_for_index(table, index, id, row)?;
        }
        Ok(())
    }

    /// Remove a row's contribution to one declared retrieval index, if its
    /// backend is open.
    ///
    /// Gated on the same condition [`Engine::index_row_for_index`] inserted
    /// under — "at least one named column holds text" for `FullText`, "the
    /// one named column holds a vector" for `Vector` — so a row that never
    /// contributed to an index is never asked to remove itself from one.
    /// `FullTextIndex::remove`/`VectorIndex::remove` are no-ops for an
    /// absent id regardless, so this gate is a cheap skip, not a correctness
    /// requirement — but matching the insert side's condition exactly is
    /// what keeps the two paths from silently drifting apart.
    fn deindex_row_for_index(
        &mut self,
        table: &Table,
        index: &Index,
        id: RowId,
        row: &[Value],
    ) -> Result<()> {
        let key = retrieval_key(&index.table, &index.columns);
        match index.kind {
            IndexKind::FullText => {
                if concatenated_full_text(table, &index.columns, row)?.is_some() {
                    if let Some(backend) = self.text_indexes.get_mut(&key) {
                        backend.remove(id)?;
                    }
                }
            }
            IndexKind::Vector => {
                let (ordinal, _) = table.require_column(index.column())?;
                if matches!(row.get(ordinal), Some(Value::Vector(_))) {
                    if let Some(backend) = self.vector_indexes.get_mut(&key) {
                        backend.remove(id)?;
                    }
                }
            }
            IndexKind::BTree => {}
        }
        Ok(())
    }

    // ---------------------------------------------------------- UPDATE/DELETE

    fn update(&mut self, plan: &UpdatePlan, params: &[Value]) -> Result<Outcome> {
        let table = self.catalog.require_table(&plan.table)?.clone();
        let rules = self.rules_for(&table)?;
        let env = self.env(params);
        let mut count = 0;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        for (id, bytes) in self.candidate_rows(&table, &plan.filter, params)? {
            let row = decode_row(&bytes)?;
            if !self.matches(&plan.filter, &row, &env)? {
                continue;
            }
            let mut next = row.clone();
            next.resize(table.columns.len(), Value::Null);
            for (index, expr) in &plan.assignments {
                let value = sql::coerce(
                    eval::evaluate(expr, &row, Computed::NONE, &env)?,
                    &table.columns[*index],
                )?;
                next[*index] = value;
            }
            self.apply_constraints(&table, &rules, &mut next, &OnConflict::abort(), &env)?;
            // A `UNIQUE` constraint has to be re-checked against every *other*
            // row, which is the same O(rows) scan an `INSERT` pays and for the
            // same reason.
            self.ensure_unique(&table, &rules, id, &next)?;
            let id = self.write_changed_row(&table, id, &row, next)?;
            count += 1;
            if let Some(items) = &plan.returning {
                returned.push(self.project_stored(&table, id, items, &env)?);
            }
        }
        self.end_write()?;
        match &plan.returning {
            Some(items) => Ok(Outcome::Rows(ResultSet {
                columns: items.iter().map(|item| item.label().to_string()).collect(),
                rows: returned,
            })),
            None => Ok(Outcome::Written(count)),
        }
    }

    fn delete(&mut self, plan: &DeletePlan, params: &[Value]) -> Result<Outcome> {
        let table = self.catalog.require_table(&plan.table)?.clone();
        let env = self.env(params);
        let mut count = 0;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        for (id, bytes) in self.candidate_rows(&table, &plan.filter, params)? {
            let row = decode_row(&bytes)?;
            if !self.matches(&plan.filter, &row, &env)? {
                continue;
            }
            // `RETURNING` on a `DELETE` can only mean the row as it was: it is
            // projected before the row stops existing.
            if let Some(items) = &plan.returning {
                let exec = ExecRow {
                    id,
                    score: None,
                    values: row.clone(),
                    aggregates: Vec::new(),
                    windows: Vec::new(),
                };
                returned.push(project_row(items, &exec, &env)?);
            }
            // Storage first, backends second — see `write_btree_entries`.
            self.remove_btree_entries(&table, id, &row)?;
            self.storage.delete_row(&table.name, id)?;
            self.deindex_row_retrieval(&table, id, &row)?;
            self.note_change(&table.name, id, ChangeKind::Delete);
            count += 1;
        }
        self.end_write()?;
        match &plan.returning {
            Some(items) => Ok(Outcome::Rows(ResultSet {
                columns: items.iter().map(|item| item.label().to_string()).collect(),
                rows: returned,
            })),
            None => Ok(Outcome::Written(count)),
        }
    }

    /// Whether a row passes an optional `WHERE` filter.
    fn matches(
        &self,
        filter: &Option<crate::plan::Expr>,
        row: &[Value],
        env: &Env<'_>,
    ) -> Result<bool> {
        match filter {
            Some(filter) => Ok(eval::is_truthy(&eval::evaluate(
                filter,
                row,
                Computed::NONE,
                env,
            )?)),
            None => Ok(true),
        }
    }

    // --------------------------------------------------------------- SELECT

    /// Run any of the three query-body shapes to completion, dispatching on
    /// which one it is. The one entry point [`InsertPlan`]'s `SELECT` source
    /// uses, since it may be any of them — a compound included, since
    /// AHL-473.
    fn select_body(&mut self, body: &SubqueryBody, params: &[Value]) -> Result<ResultSet> {
        match body {
            SubqueryBody::Select(plan) => self.select(plan, params),
            SubqueryBody::Scalar(plan) => self.select_scalar(plan, params),
            SubqueryBody::SetOp(plan) => self.select_set_op(plan, params),
        }
    }

    /// Evaluate a `SELECT` with no `FROM`: each scalar expression once, into a
    /// single row.
    fn select_scalar(&self, plan: &ScalarPlan, params: &[Value]) -> Result<ResultSet> {
        let env = self.read_env(params);
        self.run_scalar(plan, &env)
    }

    /// The same, against an environment the caller already has — which is how a
    /// `(SELECT 1)` subquery runs inside another query.
    fn run_scalar(&self, plan: &ScalarPlan, env: &Env<'_>) -> Result<ResultSet> {
        let columns = plan
            .items
            .iter()
            .map(|item| item.label.clone())
            .collect::<Vec<_>>();
        let mut row = Vec::with_capacity(plan.items.len());
        for item in &plan.items {
            row.push(eval::evaluate(&item.expr, &[], Computed::NONE, env)?);
        }
        Ok(ResultSet {
            columns,
            rows: alloc::vec![row],
        })
    }

    fn select(&mut self, plan: &SelectPlan, params: &[Value]) -> Result<ResultSet> {
        self.refresh_indexes()?;
        let env = self.read_env(params);
        self.run_select(plan, &env, None)
    }

    /// Evaluate a `UNION`/`INTERSECT`/`EXCEPT` chain.
    fn select_set_op(&mut self, plan: &SetOperationPlan, params: &[Value]) -> Result<ResultSet> {
        self.refresh_indexes()?;
        let env = self.read_env(params);
        self.run_set_operation(plan, &env, None)
    }

    /// Run a planned `SELECT` through the streaming pipeline.
    ///
    /// Split from [`Engine::select`] because index refresh is the only part
    /// that needs `&mut self`; everything below reads, which is what lets the
    /// pipeline hold a borrow of storage for as long as the query runs.
    ///
    /// # Where it stops streaming, and why
    ///
    /// `scan → filter → join → limit` streams: a row is read, decoded, tested
    /// and joined before the next one is touched, and `LIMIT` ends the scan.
    /// `ORDER BY`, `GROUP BY`/aggregates and `DISTINCT` cannot — none of them
    /// can emit a first row before it has seen the last input row — so the
    /// pipeline is collected before them, and a query that has any of them
    /// reads its whole input exactly as it always did.
    ///
    /// # Re-entrancy
    ///
    /// A correlated subquery in the `WHERE` clause calls back into this
    /// function from inside the pipeline, while the outer scan is mid-stream.
    /// That is safe because no [`Storage`] call keeps a `RefCell` borrow past
    /// its own return — [`RowScan`] holds a plain `&` between batches — so two
    /// live scans over one [`SharedStorage`] never overlap a borrow. See
    /// `shared.rs`, which is where that property is a stated contract rather
    /// than an accident.
    ///
    /// `cap` is the caller's row budget: `EXISTS` and a scalar subquery want
    /// one row, and passing that in lets `stop_after` end the inner scan there.
    fn run_select(
        &self,
        plan: &SelectPlan,
        env: &Env<'_>,
        cap: Option<usize>,
    ) -> Result<ResultSet> {
        let driving = &plan.from[0];
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();

        // `LIMIT` and `OFFSET` may be bound parameters, so they are numbers
        // only now. A retrieval query has to fetch enough candidates to cover
        // both, or an `OFFSET` would page past the end of what was ranked.
        let limit = match (row_count(plan.limit.as_ref(), env)?, cap) {
            (Some(limit), Some(cap)) => Some(limit.min(cap)),
            (limit, cap) => limit.or(cap),
        };
        let offset = row_count(plan.offset.as_ref(), env)?.unwrap_or(0);
        let fetch = limit.map(|limit| limit.saturating_add(offset));

        // Which columns any of this can observe. Everything else is walked past
        // rather than turned into a `String` or a `Vec` on the heap.
        let mask = needed_columns(plan);
        let driving_mask = mask.slice(0, driving.table.columns.len());

        // Whether `LIMIT` may end the scan rather than truncate the answer.
        //
        // Only when nothing downstream can reorder or fold the rows. A sort
        // chooses *which* rows survive, an aggregate collapses them and a
        // `DISTINCT` drops duplicates, so in all three cases the first `n` rows
        // off the scan are not the first `n` rows of the answer. A retrieval
        // query is excluded for the same reason: its rows arrive in score
        // order and are re-sorted by row id.
        //
        // When it does apply, the scan already yields row-id ascending — and a
        // join preserves that, because it emits its outer rows in order and
        // `sort_rows` breaks ties stably — so taking the first `n` is exactly
        // what sorting and then truncating would have produced.
        let stop_after = match (
            plan.order.is_empty() && !is_aggregate && !plan.distinct && plan.score.is_none(),
            fetch,
        ) {
            (true, Some(fetch)) => Some(fetch),
            _ => None,
        };

        // Which rows we even look at: retrieval when the query asked for it,
        // otherwise a point lookup or a scan depending on what the filter pins
        // down. A filter on a single-table retrieval query is pushed into the
        // fetch — see [`Engine::retrieve_filtered`] — because a fixed candidate
        // budget filtered afterwards under-fills a restrictive `WHERE`.
        let params = env.params();
        let mut stream: RowStream<'_> = if plan.joins.is_empty() {
            match (&driving.derived, &plan.score, &plan.filter) {
                // A derived table has no storage to stream from, so it
                // materialises in full before the outer pipeline starts. That
                // cost is real and is not hidden: `FROM (SELECT ...)` builds
                // the whole inner result, and a `LIMIT` on the outer query does
                // not shorten it. Pushing the outer limit inward is a planner
                // rewrite, not something this loop can do.
                (Some(body), _, filter) => {
                    let base = self.derived_stream(body, env)?;
                    match filter {
                        Some(filter) => Box::new(Filter::new(base, filter, env)),
                        None => base,
                    }
                }
                (None, Some(score), Some(filter)) => Box::new(
                    self.retrieve_filtered(
                        &driving.table,
                        score,
                        filter,
                        fetch,
                        &driving_mask,
                        env,
                    )?
                    .into_iter()
                    .map(Ok),
                ),
                (None, Some(score), None) => Box::new(
                    self.retrieve_rows(&driving.table, score, fetch, &driving_mask, env)?
                        .into_iter()
                        .map(Ok),
                ),
                (None, None, _) => {
                    let source = self.candidate_bytes(&driving.table, &plan.filter, params)?;
                    match &plan.filter {
                        // Fused (AHL-478): a row the predicate rejects is
                        // tested against borrowed cells and never
                        // materialised into `Value`s at all. See
                        // `DecodeFilter`.
                        Some(filter) => {
                            Box::new(DecodeFilter::new(source, &driving_mask, filter, env))
                        }
                        None => Box::new(Decode::new(source, &driving_mask)),
                    }
                }
            }
        } else {
            // Joined: candidates come from the driving table alone, so a
            // `WHERE` that references other tables cannot be pushed into the
            // fetch; it is applied to the joined rows below instead.
            let mut stream: RowStream<'_> = match (&driving.derived, &plan.score) {
                (Some(body), _) => self.derived_stream(body, env)?,
                (None, Some(score)) => Box::new(
                    self.retrieve_rows(&driving.table, score, fetch, &driving_mask, env)?
                        .into_iter()
                        .map(Ok),
                ),
                (None, None) => Box::new(Decode::new(
                    self.candidate_bytes(&driving.table, &plan.filter, params)?,
                    &driving_mask,
                )),
            };
            let mut offset_of = driving.table.columns.len();
            for (index, join) in plan.joins.iter().enumerate() {
                let inner = &plan.from[index + 1];
                let width = inner.table.columns.len();
                let side = match &inner.derived {
                    // Materialised for the same reason a stored inner side is —
                    // a nested loop replays it per outer row — and at the same
                    // cost, plus running the inner query once. A derived table
                    // has no index, so the probe chooser does not apply to it.
                    Some(body) => JoinInner::Materialised {
                        rows: self.run_body(body, env, None)?,
                        width,
                    },
                    None => self.join_inner(&inner.table, offset_of, join.on.as_ref(), &mask)?,
                };
                offset_of += width;
                stream = Box::new(NestedLoopJoin::new(
                    stream,
                    side,
                    join.kind,
                    join.on.as_ref(),
                    env,
                ));
            }
            match &plan.filter {
                Some(filter) => Box::new(Filter::new(stream, filter, env)),
                None => stream,
            }
        };

        if let Some(stop) = stop_after {
            stream = Box::new(stream.take(stop));
        }
        let mut rows: Vec<ExecRow> = stream.collect::<Result<Vec<_>>>()?;

        if is_aggregate {
            rows = self.aggregate(plan, rows, env)?;
        }

        // Window functions run over the rows a `GROUP BY` already folded (or
        // the plain joined rows, for a non-aggregate query) — after
        // `WHERE`/`GROUP BY`/`HAVING`, before `DISTINCT`/`ORDER BY`/`LIMIT`
        // (`docs/architecture.md` phase 1 item 6), so `SELECT DISTINCT` folds on a window
        // function's own output and `ORDER BY` may sort by one.
        if !plan.windows.is_empty() {
            rows = window(plan, rows, env)?;
        }

        // `DISTINCT` folds *projected* rows, not stored ones, and it happens
        // before `ORDER BY` so that the order applies to what survives.
        if plan.distinct {
            rows = distinct_rows(&plan.items, &plan.distinct_collations, rows, env)?;
        }

        rows = sort_rows(rows, &plan.order, env)?;

        // `OFFSET` skips before `LIMIT` counts; an offset past the end leaves
        // nothing, which is not an error.
        if offset > 0 {
            rows.drain(..offset.min(rows.len()));
        }
        if let Some(limit) = limit {
            rows.truncate(limit);
        }

        project(&plan.items, rows, env)
    }

    /// Run the query inside a subquery or a derived table, returning its rows.
    fn run_body(
        &self,
        body: &SubqueryBody,
        env: &Env<'_>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        match body {
            SubqueryBody::Select(plan) => Ok(self.run_select(plan, env, cap)?.rows),
            SubqueryBody::Scalar(plan) => Ok(self.run_scalar(plan, env)?.rows),
            SubqueryBody::SetOp(plan) => Ok(self.run_set_operation(plan, env, cap)?.rows),
        }
    }

    /// Run a `UNION`/`INTERSECT`/`EXCEPT` chain: both arms run through the
    /// ordinary pipeline to completion — [`Engine::run_body`], recursively,
    /// so a chain of more than two arms folds one step at a time — and are
    /// then combined by [`combine_set_operation`]. Materialising rather than
    /// streaming is deliberate here (`docs/architecture.md` Phase 1c item 2): a streaming
    /// merge is a later optimisation, and `INTERSECT`/`EXCEPT` need the whole
    /// of the right arm before the first output row can be decided anyway.
    ///
    /// `cap` bounds the *final* result the same way [`Engine::run_select`]'s
    /// does — never either arm's own materialisation, since deduplication
    /// and set membership both need to see every row either arm produced.
    fn run_set_operation(
        &self,
        plan: &SetOperationPlan,
        env: &Env<'_>,
        cap: Option<usize>,
    ) -> Result<ResultSet> {
        let left = self.run_body(&plan.left, env, None)?;
        let right = self.run_body(&plan.right, env, None)?;
        let combined = combine_set_operation(plan.op, left, right, &plan.collations);

        let mut rows: Vec<ExecRow> = combined
            .into_iter()
            .zip(1u64..)
            .map(|(values, id)| ExecRow::scanned(id, values))
            .collect();
        rows = sort_rows(rows, &plan.order, env)?;

        let offset = row_count(plan.offset.as_ref(), env)?.unwrap_or(0);
        if offset > 0 {
            rows.drain(..offset.min(rows.len()));
        }
        let limit = match (row_count(plan.limit.as_ref(), env)?, cap) {
            (Some(limit), Some(cap)) => Some(limit.min(cap)),
            (limit, cap) => limit.or(cap),
        };
        if let Some(limit) = limit {
            rows.truncate(limit);
        }

        let columns = plan.left.labels().into_iter().map(str::to_string).collect();
        Ok(ResultSet {
            columns,
            rows: rows.into_iter().map(|row| row.values).collect(),
        })
    }

    /// A derived table's rows, as a stream the outer pipeline can consume.
    ///
    /// The row ids are positions, one-based. A derived table has no stored row
    /// id, and this is only ever used to break `ORDER BY` ties — where "the
    /// order the inner query produced them in" is the honest tie-break and the
    /// one that matches reading the subquery's own output top to bottom.
    fn derived_stream<'a>(&self, body: &SubqueryBody, env: &Env<'_>) -> Result<RowStream<'a>> {
        let rows = self.run_body(body, env, None)?;
        Ok(Box::new(
            rows.into_iter()
                .zip(1u64..)
                .map(|(values, id)| Ok(ExecRow::scanned(id, values))),
        ))
    }

    /// Where one join's inner rows come from: an index probe when the `ON`
    /// justifies one, the whole table otherwise.
    ///
    /// `offset_of` is where the inner table's columns begin in the joined row,
    /// which is what translates the plan's ordinals — held against the
    /// concatenation of every table in `FROM` order — onto this table.
    fn join_inner(
        &self,
        inner: &Table,
        offset_of: usize,
        on: Option<&crate::plan::Expr>,
        mask: &ColumnMask,
    ) -> Result<JoinInner<'_>> {
        let inner_mask = mask.slice(offset_of, inner.columns.len());
        match self.join_probe(inner, offset_of, on) {
            Some((key, ty, collation, kind)) => Ok(JoinInner::probe(IndexProbe::new(
                &self.storage,
                &inner.name,
                inner_mask,
                inner.columns.len(),
                key,
                ty,
                collation,
                kind,
            ))),
            None => self.materialise_inner(inner, &inner_mask),
        }
    }

    /// The index probe a join's `ON` justifies for its inner side, or `None`
    /// when the inner side has to be materialised (`docs/architecture.md`, decision D6).
    ///
    /// This is a **rule, not a cost model**, the same way
    /// [`Engine::indexed_candidates`] is, and it is deliberately the narrowest
    /// rule that pays for the common ORM join. Every condition below has to
    /// hold, and anything else falls back to reading the whole inner table:
    ///
    /// * **There is an `ON`.** A cross join constrains nothing.
    /// * **A top-level conjunct is `outer_column = inner_column`.** Only `AND`
    ///   is descended, for the reason [`collect_conjuncts`] gives: one side of
    ///   an `OR` cannot narrow the other, so a probe built from it would leave
    ///   out the rows the other side matches. The remaining conjuncts need no
    ///   special handling — [`crate::exec::NestedLoopJoin`] re-evaluates the
    ///   *whole* `ON` over every candidate, so they are the residual filter by
    ///   construction, and the surviving pairs are the same expression's
    ///   verdict either way.
    /// * **One side is a column of a table the join has already produced** (an
    ///   ordinal below `offset_of`) **and the other is a column of the inner
    ///   table.** A literal, a parameter, an expression, or two columns of the
    ///   same table are all not this rule.
    /// * **The inner column is the `INTEGER PRIMARY KEY`, or leads a scalar
    ///   B-tree index.** Only a leading column has its matching entries in one
    ///   contiguous run — see [`index_probe`].
    /// * **The inner column's declared type is a storage class.** A `NUMERIC`
    ///   column holds every class at once and no ordered index may answer for
    ///   it; the per-key half of the same test is [`indexable_probe`], applied
    ///   in [`IndexProbe`] because the key is only known per outer row.
    ///
    /// Choosing badly here is slow, never wrong: the probe narrows which rows
    /// are *read*, and the `ON` still decides which pairs survive.
    fn join_probe(
        &self,
        inner: &Table,
        offset_of: usize,
        on: Option<&crate::plan::Expr>,
    ) -> Option<(usize, DataType, Collation, ProbeKind)> {
        let mut keys = Vec::new();
        collect_join_keys(on?, offset_of, inner.columns.len(), &mut keys);
        if keys.is_empty() {
            return None;
        }

        // The row id first, wherever it appears: one descent and at most one
        // row, which no secondary index can beat. An `INTEGER PRIMARY KEY` is a
        // number and no collation is consulted comparing numbers, so this path
        // is reachable whatever the `ON` resolved.
        if let Some(alias) = inner.rowid_alias() {
            if let Some(key) = keys.iter().find(|key| key.inner == alias) {
                return Some((
                    key.outer,
                    DataType::Integer,
                    Collation::Binary,
                    ProbeKind::RowId,
                ));
            }
        }

        let indexes = self.catalog.indexes_for(&inner.name);
        for key in &keys {
            let ty = inner.columns[key.inner].ty;
            if !matches!(
                ty,
                DataType::Integer | DataType::Real | DataType::Text | DataType::Blob
            ) {
                continue;
            }
            let mut best: Option<&Index> = None;
            for index in &indexes {
                if index.kind != IndexKind::BTree {
                    continue;
                }
                let leads = index
                    .columns
                    .first()
                    .and_then(|column| inner.column(column))
                    .is_some_and(|(leading, _)| leading == key.inner);
                if !leads {
                    continue;
                }
                // The index's leading column has to be keyed under the very
                // collation the `ON` resolved. Anything else answers a
                // different question — see [`JoinKey::collation`] — and the
                // fallback (reading the inner table) is correct under every
                // collation, so declining here costs speed and never an answer.
                if index.collation(0) != key.collation {
                    continue;
                }
                // Every applicable index selects the same rows, so this is only
                // about how many entries the range walk reads: a narrower index
                // is a shorter one. Ties keep the first, which is catalog order
                // — index name — and therefore deterministic.
                if best.is_none_or(|best| index.columns.len() < best.columns.len()) {
                    best = Some(index);
                }
            }
            if let Some(index) = best {
                return Some((
                    key.outer,
                    ty,
                    key.collation,
                    ProbeKind::Index(index.name.clone()),
                ));
            }
        }
        None
    }

    /// Read a join's inner table into memory, decoding only what the statement
    /// can observe of it.
    ///
    /// A nested loop replays the inner side once per outer row, so it has to be
    /// re-readable — this is the materialisation `PERF.md` names, and it is the
    /// fallback now that [`Engine::join_probe`] can replace it. It is paid once
    /// per join rather than being re-cloned per outer row, and the columns the
    /// query never mentions are walked past rather than allocated.
    fn materialise_inner(&self, table: &Table, mask: &ColumnMask) -> Result<JoinInner<'_>> {
        let mut rows = Vec::new();
        for row in RowScan::new(&self.storage, &table.name) {
            rows.push(decode_row_masked(&row?.1, mask)?);
        }
        Ok(JoinInner::Materialised {
            rows,
            width: table.columns.len(),
        })
    }

    /// Fetch and decode the retrieval candidates for a single table.
    ///
    /// With no `LIMIT` the fetch is capped at [`DEFAULT_CANDIDATES`]; a
    /// `LIMIT` over-fetches by [`CANDIDATE_OVERFETCH`] so that fusion has more
    /// than the bare minimum to rank.
    fn retrieve_rows(
        &self,
        table: &Table,
        score: &ScoreExpr,
        limit: Option<usize>,
        mask: &ColumnMask,
        env: &Env<'_>,
    ) -> Result<Vec<ExecRow>> {
        let candidate_limit = limit
            .map(|limit| limit.saturating_mul(CANDIDATE_OVERFETCH))
            .unwrap_or(DEFAULT_CANDIDATES)
            .max(1);
        let mut rows = Vec::new();
        for scored in self.evaluate_score(table, score, candidate_limit, env)? {
            if let Some(bytes) = self.storage.get_row(&table.name, scored.id)? {
                rows.push(ExecRow {
                    id: scored.id,
                    score: Some(scored.score),
                    values: decode_row_masked(&bytes, mask)?,
                    aggregates: Vec::new(),
                    windows: Vec::new(),
                });
            }
        }
        Ok(rows)
    }

    /// Group the joined rows by the `GROUP BY` keys and compute the aggregates,
    /// emitting one row per group.
    ///
    /// Without a `GROUP BY` the whole input is one group — empty input still
    /// emits a single row, so `SELECT COUNT(*) FROM empty` returns one `0`.
    /// With a `GROUP BY`, zero input rows produce zero groups.
    fn aggregate(
        &self,
        plan: &SelectPlan,
        rows: Vec<ExecRow>,
        env: &Env<'_>,
    ) -> Result<Vec<ExecRow>> {
        let mut groups: Vec<Vec<ExecRow>> = Vec::new();
        if plan.group_by.is_empty() {
            groups.push(rows);
        } else {
            let mut keyed: Vec<(Vec<Value>, ExecRow)> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut keys = Vec::with_capacity(plan.group_by.len());
                for expr in &plan.group_by {
                    keys.push(eval::evaluate(expr, &row.values, Computed::NONE, env)?);
                }
                keyed.push((keys, row));
            }
            let collations = &plan.group_collations;
            keyed.sort_by(|a, b| compare_group_keys(&a.0, &b.0, collations));

            let mut current: Vec<ExecRow> = Vec::new();
            let mut current_key: Option<Vec<Value>> = None;
            for (key, row) in keyed {
                if let Some(previous) = &current_key {
                    if compare_group_keys(previous, &key, collations) != core::cmp::Ordering::Equal
                    {
                        groups.push(core::mem::take(&mut current));
                    }
                }
                current_key = Some(key);
                current.push(row);
            }
            if !current.is_empty() {
                groups.push(current);
            }
        }

        let width = plan.from.iter().map(|item| item.table.columns.len()).sum();
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            // Borrowed, not cloned: the aggregate evaluator only reads the
            // group, and copying every row a third time is exactly the cost
            // `PERF.md` names against this function.
            let group_values: Vec<&[Value]> =
                group.iter().map(|row| row.values.as_slice()).collect();
            let mut aggregates = Vec::with_capacity(plan.aggregates.len());
            for aggregate in &plan.aggregates {
                aggregates.push(eval::evaluate_aggregate(aggregate, &group_values, env)?);
            }
            drop(group_values);

            // The representative row is the first of the group, so non-aggregate
            // projection expressions and `HAVING` see the grouping key's value.
            // An empty group (only possible with no `GROUP BY`) is all `NULL`.
            let id = group.first().map(|row| row.id).unwrap_or(0);
            let representative = group
                .into_iter()
                .next()
                .map(|row| row.values)
                .unwrap_or_else(|| alloc::vec![Value::Null; width]);

            if let Some(having) = &plan.having {
                if !eval::is_truthy(&eval::evaluate(
                    having,
                    &representative,
                    Computed::aggregates(&aggregates),
                    env,
                )?) {
                    continue;
                }
            }

            out.push(ExecRow {
                id,
                score: None,
                values: representative,
                aggregates,
                windows: Vec::new(),
            });
        }
        Ok(out)
    }

    /// Retrieve and filter in one pass, over-fetching until the filter admits
    /// enough rows.
    ///
    /// A fixed candidate budget cannot serve a filtered query: the retriever is
    /// asked for the best matches overall, and a filter that keeps a small
    /// fraction of them can discard every one, so `LIMIT 10` returns nothing at
    /// all. Instead the budget doubles each round until the filter admits
    /// `limit` rows, or until the retriever reports it has nothing more to give
    /// (its `search` returns fewer than asked). That second case is the
    /// exact-scan fallback: every row the index can rank has been seen, so the
    /// rows that passed the filter are the complete answer for that filter — a
    /// filter too selective for any bounded over-fetch degrades to correctness,
    /// never to an empty result. Rows are only ever ranked within one probe, so
    /// the answer is a deterministic function of the query and the corpus.
    fn retrieve_filtered(
        &self,
        table: &Table,
        score: &ScoreExpr,
        filter: &crate::plan::Expr,
        limit: Option<usize>,
        mask: &ColumnMask,
        env: &Env<'_>,
    ) -> Result<Vec<ExecRow>> {
        // A query with no `LIMIT` is still capped, at the same candidate budget
        // an unfiltered query gets.
        let want = limit.unwrap_or(DEFAULT_CANDIDATES);
        if want == 0 {
            return Ok(Vec::new());
        }

        let mut overfetch = CANDIDATE_OVERFETCH;
        loop {
            let k = want.saturating_mul(overfetch).max(1);
            let hits = self.evaluate_score(table, score, k, env)?;
            // Fewer than asked for means the retriever has run dry: everything
            // it can rank is already in `hits`, so this round's filtered result
            // is complete and the loop must end here.
            let exhausted = hits.len() < k;

            let mut matched = Vec::with_capacity(hits.len());
            for hit in &hits {
                if let Some(bytes) = self.storage.get_row(&table.name, hit.id)? {
                    let row = decode_row_masked(&bytes, mask)?;
                    if eval::is_truthy(&eval::evaluate(filter, &row, Computed::NONE, env)?) {
                        matched.push(ExecRow {
                            id: hit.id,
                            score: Some(hit.score),
                            values: row,
                            aggregates: Vec::new(),
                            windows: Vec::new(),
                        });
                    }
                }
            }

            if matched.len() >= want || exhausted {
                matched.truncate(want);
                return Ok(matched);
            }

            // The budget doubles geometrically, so it reaches the index size in
            // O(log n) rounds and then trips `exhausted`; it cannot loop
            // forever.
            overfetch = overfetch.saturating_mul(2);
        }
    }

    /// Evaluate a retrieval expression into a ranked candidate list.
    ///
    /// The query side of each leaf is an expression, so an embedding or a
    /// search string can be bound per execution. Its type — and, for a vector,
    /// its dimension — is checked here rather than at prepare time, because
    /// that is the first moment a `?` has a value at all.
    fn evaluate_score(
        &self,
        table: &Table,
        expr: &ScoreExpr,
        k: usize,
        env: &Env<'_>,
    ) -> Result<Vec<Scored>> {
        match expr {
            ScoreExpr::Vector { column, query } => {
                let index = self.vector_index(table, *column)?;
                let query = bind_embedding(table, *column, query, env)?;
                let mut hits = index.search(&query, k)?;
                sort_by_score_desc(&mut hits);
                Ok(hits)
            }
            ScoreExpr::Text { columns, query } => {
                let index = self.text_index(table, columns)?;
                let Value::Text(query) = eval::evaluate(query, &[], Computed::NONE, env)? else {
                    return Err(Error::Type(
                        "bm25_score() needs a text query as its final argument".to_string(),
                    ));
                };
                let mut hits = index.search(&query, k)?;
                sort_by_score_desc(&mut hits);
                Ok(hits)
            }
            ScoreExpr::Fuse { parts, k: rrf_k } => {
                let mut lists = Vec::with_capacity(parts.len());
                for part in parts {
                    lists.push(self.evaluate_score(table, part, k, env)?);
                }
                let mut fused = reciprocal_rank_fusion(&lists, *rrf_k);
                fused.truncate(k);
                Ok(fused)
            }
        }
    }

    /// The `FullText` backend that answers `bm25_score(columns..., query)`.
    ///
    /// Resolves which *declared* index the named columns mean first (see
    /// [`Engine::resolve_full_text_index`]), then looks up that index's own
    /// backend under its own key — never a key built straight from the
    /// query's ordinals, so a query naming the same columns in a different
    /// order still finds the one backend that indexes them.
    fn text_index(&self, table: &Table, columns: &[usize]) -> Result<&dyn FullTextIndex> {
        let index = self.resolve_full_text_index(table, columns)?;
        let key = retrieval_key(&index.table, &index.columns);
        self.text_indexes
            .get(&key)
            .map(|index| index.as_ref())
            .ok_or_else(|| {
                Error::Index(alloc::format!(
                    "no full-text index on ({})",
                    index.columns.join(", ")
                ))
            })
    }

    /// The declared `FullText` index of `table` that covers exactly the named
    /// `columns` — order-independent, since a combined BM25 score does not
    /// depend on which order the columns were named in: `bm25_score(title,
    /// body, ?)` and `bm25_score(body, title, ?)` mean the same query and find
    /// the same index. `bm25_score(body, ?)`, the single-column call this has
    /// always accepted, finds the single-column index over `body` by the same
    /// rule — a set of one, matched the same way it always was.
    fn resolve_full_text_index(&self, table: &Table, columns: &[usize]) -> Result<&Index> {
        let mut names: Vec<String> = Vec::with_capacity(columns.len());
        for &ordinal in columns {
            let column = table
                .columns
                .get(ordinal)
                .ok_or_else(|| Error::Catalog("column ordinal out of range".to_string()))?;
            names.push(column.name.to_ascii_lowercase());
        }
        let mut matches = self
            .catalog
            .indexes_for(&table.name)
            .into_iter()
            .filter(|index| {
                index.kind == IndexKind::FullText && same_column_set(&index.columns, &names)
            });
        let found = matches.next().ok_or_else(|| {
            Error::Index(alloc::format!(
                "no full-text index on ({})",
                names.join(", ")
            ))
        })?;
        if matches.next().is_some() {
            // Only reachable if two `FullText` indexes were declared over the
            // same columns in different orders — the catalog's dup-check
            // compares column lists positionally (`Index::covers`), so it
            // does not refuse that the way it refuses a true duplicate. There
            // is nothing to prefer between them, so this is reported rather
            // than guessed at.
            return Err(Error::Index(alloc::format!(
                "more than one full-text index covers ({}); this cannot be resolved automatically",
                names.join(", ")
            )));
        }
        Ok(found)
    }

    fn vector_index(&self, table: &Table, column: usize) -> Result<&dyn VectorIndex> {
        let key = index_key(table, column)?;
        self.vector_indexes
            .get(&key)
            .map(|index| index.as_ref())
            .ok_or_else(|| {
                Error::Index(alloc::format!("no vector index on `{}`", key.1.join(", ")))
            })
    }
}

/// The engine is what runs a subquery: the evaluator has a row, and this has
/// the storage the subquery's own `FROM` needs.
///
/// Deliberately `&self` — a subquery only ever reads — which is what keeps it
/// re-entrant into a pipeline that is already streaming. See
/// [`Engine::run_select`] for why that does not double-borrow storage.
impl SubqueryRunner for Engine {
    fn run(
        &self,
        body: &SubqueryBody,
        env: &Env<'_>,
        max_rows: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        self.run_body(body, env, max_rows)
    }
}

/// Resolve a `vector_score()` query into an embedding of the column's width.
///
/// The dimension check is the reason this is not a bare `evaluate`: an index
/// asked to search with the wrong number of dimensions would either error deep
/// inside a backend or, worse, compare the prefix it was given.
fn bind_embedding(
    table: &Table,
    column: usize,
    query: &crate::plan::Expr,
    env: &Env<'_>,
) -> Result<Vec<f32>> {
    let Value::Vector(embedding) = eval::evaluate(query, &[], Computed::NONE, env)? else {
        return Err(Error::Type(
            "vector_score() needs an embedding as its second argument".to_string(),
        ));
    };
    if let Some(dim) = table
        .columns
        .get(column)
        .and_then(|column| column.ty.vector_dim())
    {
        if embedding.len() != dim {
            return Err(Error::Type(alloc::format!(
                "query embedding has dimension {} but column `{}` is VECTOR({dim})",
                embedding.len(),
                table.columns[column].name
            )));
        }
    }
    Ok(embedding)
}

fn encode_table_row(table: &Table, row: &[Value]) -> Vec<u8> {
    let types: Vec<DataType> = table.columns.iter().map(|column| column.ty).collect();
    encode_typed_row(row, &types)
}

/// The row id a `WHERE` filter pins down, if it pins one.
///
/// This is the whole of the engine's index selection today, and it is
/// deliberately the narrowest rule that pays: an equality against the
/// `INTEGER PRIMARY KEY`, anywhere in a top-level conjunction. `a = 1 AND b > 2`
/// still pins the row — the `b > 2` half is applied as an ordinary filter
/// afterwards — while `a = 1 OR b > 2` does not, because a second row could
/// satisfy the other side.
///
/// The key may be a `?`, which is the whole point of a prepared point read:
/// `WHERE id = ?` plans once and still descends the tree once per execution.
fn pinned_rowid(
    table: &Table,
    filter: Option<&crate::plan::Expr>,
    params: &[Value],
) -> Option<RowId> {
    use crate::plan::{BinaryOp, Expr};

    /// An integer key an expression names, whether written out or bound.
    fn integer_key(expr: &Expr, params: &[Value]) -> Option<i64> {
        match expr {
            Expr::Literal(Value::Integer(key)) => Some(*key),
            Expr::Param(index) => match params.get(*index) {
                Some(Value::Integer(key)) => Some(*key),
                _ => None,
            },
            _ => None,
        }
    }

    fn walk(expr: &Expr, alias: usize, params: &[Value]) -> Option<RowId> {
        match expr {
            Expr::Binary {
                op: BinaryOp::And,
                left,
                right,
                ..
            } => walk(left, alias, params).or_else(|| walk(right, alias, params)),
            // The row id is an `INTEGER PRIMARY KEY`, and no collation applies
            // to an integer comparison — so this path needs no collation check
            // the way the text ones below do.
            Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
                ..
            } => {
                let key = match (without_collate(left), without_collate(right)) {
                    (Expr::Column(column), other) | (other, Expr::Column(column))
                        if *column == alias =>
                    {
                        integer_key(without_collate(other), params)?
                    }
                    _ => return None,
                };
                RowId::try_from(key).ok()
            }
            _ => None,
        }
    }

    walk(filter?, table.rowid_alias()?, params)
}

/// A missing trailing column reads as `NULL`, which is what a row narrower
/// than its table means everywhere else in the engine.
static NULL: Value = Value::Null;

/// See past a `COLLATE` to the expression it wraps.
///
/// Every rule below matches on the *shape* of an operand — a column here, a
/// literal there — and a `COLLATE` is a plan-time annotation that changes
/// neither. The collation it named has already been resolved into the
/// comparison's own field by the time a plan reaches this file, so peeling the
/// wrapper here loses nothing: `WHERE s = ? COLLATE NOCASE` is still a
/// column-against-a-value comparison, and the collation it must be answered
/// under is still the one the node carries.
///
/// Not doing this is not *wrong* — an unrecognised shape only means no index
/// is used — but it would quietly make every explicitly-collated query a full
/// scan, which is exactly the sort of silent loss this engine's tests exist to
/// catch.
fn without_collate(expr: &crate::plan::Expr) -> &crate::plan::Expr {
    match expr {
        crate::plan::Expr::Collate { expr, .. } => without_collate(expr),
        other => other,
    }
}

/// One `column <op> value` comparison from a filter's top-level conjunction.
struct Term {
    /// Column ordinal in the driving table.
    ordinal: usize,
    /// The comparison, oriented so the column is on the left.
    op: crate::plan::BinaryOp,
    /// The other side, already resolved against the parameters.
    value: Value,
    /// The collating sequence the planner resolved for this comparison.
    ///
    /// **An index may only answer a term whose collation it is keyed under.**
    /// A `NOCASE` index holds folded keys, so probing it for a `BINARY` `=`
    /// would return rows the filter then rejects (merely slow) *and* — the half
    /// that matters — probing a `BINARY` index for a `NOCASE` `=` would look up
    /// the unfolded bytes and miss every row that differs only in case. Same
    /// query, different answer depending on the access path, which is the bug
    /// class this repository treats as the worst kind. [`index_probe`] enforces
    /// it; SQLite's planner has the identical rule.
    collation: Collation,
}

/// Collect every `column <op> value` an `AND` chain constrains, ignoring
/// everything else.
///
/// Only `AND` is descended. `a = 1 OR b = 2` contributes nothing, and it must
/// not: an index range built from one side of an `OR` would leave out the rows
/// the other side matches.
fn collect_conjuncts(
    expr: &crate::plan::Expr,
    params: &[Value],
    table: &Table,
    out: &mut Vec<Term>,
) {
    use crate::plan::{BinaryOp, Expr};

    fn resolve(expr: &Expr, params: &[Value]) -> Option<Value> {
        match expr {
            Expr::Literal(value) => Some(value.clone()),
            // A `?` is resolved here, at execution, which is the whole point:
            // one plan serves every binding and each still gets its own probe.
            Expr::Param(index) => params.get(*index).cloned(),
            _ => None,
        }
    }

    /// The same comparison with the operands swapped, so `1 < a` becomes
    /// `a > 1`.
    fn flip(op: BinaryOp) -> BinaryOp {
        match op {
            BinaryOp::Lt => BinaryOp::Gt,
            BinaryOp::LtEq => BinaryOp::GtEq,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::GtEq => BinaryOp::LtEq,
            other => other,
        }
    }

    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
            ..
        } => {
            collect_conjuncts(left, params, table, out);
            collect_conjuncts(right, params, table, out);
        }
        Expr::Binary {
            op,
            left,
            right,
            collation,
            // Affinity conversion happens once, inside `eval::comparison`,
            // when the filter this index only narrows is re-applied to every
            // candidate — see `run_select`'s `DecodeFilter`/`Filter` step.
            // `indexable_probe` below already refuses any term whose value is
            // not the column's own storage class, so a term affinity would
            // ever convert (`WHERE id = '1'` against an `INTEGER` column)
            // never reaches an index probe at all; it is not read here.
            affinity: _,
        } if matches!(
            op,
            BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
        ) =>
        {
            let term = match (without_collate(left), without_collate(right)) {
                // A joined plan numbers the right table's columns after the
                // driving table's, so an ordinal past the end belongs to
                // another table and this index knows nothing about it.
                (Expr::Column(ordinal), other) if *ordinal < table.columns.len() => {
                    resolve(without_collate(other), params).map(|value| Term {
                        ordinal: *ordinal,
                        op: *op,
                        value,
                        collation: *collation,
                    })
                }
                (other, Expr::Column(ordinal)) if *ordinal < table.columns.len() => {
                    resolve(without_collate(other), params).map(|value| Term {
                        ordinal: *ordinal,
                        op: flip(*op),
                        value,
                        collation: *collation,
                    })
                }
                _ => None,
            };
            out.extend(term);
        }
        // `BETWEEN` is two bounds written as one, and an index answers it as
        // two bounds. `NOT BETWEEN` is a union of two ranges and is left alone.
        // Both bounds carry the one collation the expression resolved, because
        // both are comparisons against the same left operand.
        Expr::Between {
            negated: false,
            expr,
            low,
            high,
            low_collation,
            high_collation,
            // Not read here — same reason as `Expr::Binary`'s `affinity`
            // above.
            low_affinity: _,
            high_affinity: _,
        } => {
            if let Expr::Column(ordinal) = without_collate(expr) {
                if *ordinal < table.columns.len() {
                    if let Some(value) = resolve(without_collate(low), params) {
                        out.push(Term {
                            ordinal: *ordinal,
                            op: BinaryOp::GtEq,
                            value,
                            collation: *low_collation,
                        });
                    }
                    if let Some(value) = resolve(without_collate(high), params) {
                        out.push(Term {
                            ordinal: *ordinal,
                            op: BinaryOp::LtEq,
                            value,
                            collation: *high_collation,
                        });
                    }
                }
            }
        }
        _ => {}
    }
}

/// Collect every `outer_column = inner_column` equality a join's `ON` puts at
/// the top level of an `AND` chain, as `(joined-row ordinal of the outer
/// column, ordinal of the inner column within the inner table)`.
///
/// `offset_of` is where the inner table begins in the joined row and `width` is
/// how many columns it has, so "below `offset_of`" is a table the join has
/// already produced and `offset_of .. offset_of + width` is the inner table
/// itself. An ordinal at or above the end belongs to a table this join has not
/// reached yet and constrains nothing here.
///
/// Only `AND` is descended, for the reason [`collect_conjuncts`] gives about
/// the filter: one side of an `OR` cannot narrow the other.
fn collect_join_keys(
    expr: &crate::plan::Expr,
    offset_of: usize,
    width: usize,
    out: &mut Vec<JoinKey>,
) {
    use crate::plan::{BinaryOp, Expr};

    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
            ..
        } => {
            collect_join_keys(left, offset_of, width, out);
            collect_join_keys(right, offset_of, width, out);
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
            collation,
            // Not read here — `NestedLoopJoin` re-evaluates the whole `ON`
            // over every candidate a probe or a fallback scan produces (see
            // `join_probe`'s doc), so the probe key itself never needs to be
            // affinity-converted for correctness, only the residual filter
            // does.
            affinity: _,
        } => {
            if let (Expr::Column(left), Expr::Column(right)) =
                (without_collate(left), without_collate(right))
            {
                let inner = offset_of..offset_of + width;
                if *left < offset_of && inner.contains(right) {
                    out.push(JoinKey {
                        outer: *left,
                        inner: *right - offset_of,
                        collation: *collation,
                    });
                } else if *right < offset_of && inner.contains(left) {
                    out.push(JoinKey {
                        outer: *right,
                        inner: *left - offset_of,
                        collation: *collation,
                    });
                }
            }
        }
        _ => {}
    }
}

/// One `outer_column = inner_column` equality a join's `ON` offers as a probe
/// key, and the collating sequence that equality resolved.
struct JoinKey {
    /// Joined-row ordinal of the outer column the key is read from.
    outer: usize,
    /// Ordinal of the inner column within the inner table.
    inner: usize,
    /// What the `ON`'s `=` compares under. An index may only answer this key if
    /// it is keyed under the same collation, for the reason [`Term::collation`]
    /// gives — and here the stakes are the same: a probe that missed rows the
    /// materialising path finds would make the join's answer depend on whether
    /// an index happened to exist.
    collation: Collation,
}

/// Whether a probe of `value` against a column declared `ty` can be answered
/// from an ordered index without changing what the query returns.
///
/// Three things have to hold, and each of them is a way an index could
/// otherwise lie:
///
/// * **The column's declared type has to be a storage class.** `coerce` then
///   guarantees every stored value is `NULL` or exactly that class. A
///   `NUMERIC` column is excluded for this reason: it holds any class at once,
///   and this engine's comparison operator *errors* on a cross-class compare
///   rather than returning false — so an index that skipped the other classes
///   would turn an error into an empty result. Such an index is still built and
///   maintained; it is simply never chosen.
/// * **The probe has to be of a class the column can hold**, for the same
///   reason: `WHERE text_col = 1` errors on the first row of a scan, and must
///   keep doing so.
/// * **The probe must not be `NaN`.** `eval::comparison` treats `NaN` as equal
///   to every number (its `partial_cmp` returns `None` and the fallback is
///   `Equal`), which no ordered structure can reproduce.
///
/// Both access paths that can be handed a value ask this: the filter's index
/// selection above, and [`crate::exec::IndexProbe`], which asks it per outer
/// row because a join key is only known then.
pub(crate) fn indexable_probe(ty: DataType, value: &Value) -> bool {
    match (ty, value) {
        (DataType::Integer | DataType::Real, Value::Integer(_)) => true,
        (DataType::Integer | DataType::Real, Value::Real(real)) => !real.is_nan(),
        (DataType::Text, Value::Text(_)) => true,
        (DataType::Blob, Value::Blob(_)) => true,
        _ => false,
    }
}

/// The narrowest range of one index the collected terms justify, and how many
/// of its columns that range binds.
///
/// The rule is the standard one: equalities down the leading columns, then at
/// most one range predicate on the column after them. Nothing is bound beyond
/// the first column an equality does not cover, because entries past that
/// point are not contiguous.
///
/// **A term is only usable when its collation is the one the index is keyed
/// under**, which is SQLite's rule and is what stops this from being an
/// optimisation that changes answers. See [`Term::collation`].
fn index_probe(
    table: &Table,
    index: &Index,
    terms: &[Term],
) -> Result<Option<(usize, crate::index::KeyRange)>> {
    use crate::plan::BinaryOp;

    let ordinals = index_ordinals(table, index)?;
    let usable = |term: &Term, ordinal: usize, position: usize| {
        term.ordinal == ordinal
            && indexable_probe(table.columns[ordinal].ty, &term.value)
            && term.collation == index.collation(position)
    };

    let mut equalities: Vec<&Value> = Vec::with_capacity(ordinals.len());
    for (position, ordinal) in ordinals.iter().enumerate() {
        let found = terms
            .iter()
            .find(|term| term.op == BinaryOp::Eq && usable(term, *ordinal, position));
        match found {
            Some(term) => equalities.push(&term.value),
            None => break,
        }
    }

    let mut range = crate::index::KeyRange::equality(&index.name, &equalities, &index.collations)?;
    let mut bound = equalities.len();
    if equalities.len() < ordinals.len() {
        let position = equalities.len();
        let ordinal = ordinals[position];
        let mut narrowed = false;
        for term in terms.iter().filter(|term| usable(term, ordinal, position)) {
            // Both edges are widened to include the whole group of entries
            // that encode equal to the bound, even for a strict `<` or `>`.
            // The filter rejects what does not belong; the alternative is
            // reasoning about a boundary where two distinct values share an
            // encoding, which is where an index quietly loses a row.
            match term.op {
                BinaryOp::Gt | BinaryOp::GtEq => {
                    range = range.with_lower(
                        &index.name,
                        &equalities,
                        &index.collations,
                        &term.value,
                    )?;
                    narrowed = true;
                }
                BinaryOp::Lt | BinaryOp::LtEq => {
                    range = range.with_upper(
                        &index.name,
                        &equalities,
                        &index.collations,
                        &term.value,
                    )?;
                    narrowed = true;
                }
                _ => {}
            }
        }
        if narrowed {
            bound += 1;
        }
    }
    if bound == 0 {
        return Ok(None);
    }
    Ok(Some((bound, range)))
}

/// The ordinals one index's columns have in `table`.
fn index_ordinals(table: &Table, index: &Index) -> Result<Vec<usize>> {
    index
        .columns
        .iter()
        .map(|column| Ok(table.require_column(column)?.0))
        .collect()
}

/// The values `row` contributes to one index's key, in the index's column
/// order.
fn index_values<'a>(table: &Table, index: &Index, row: &'a [Value]) -> Result<Vec<&'a Value>> {
    let mut values = Vec::with_capacity(index.columns.len());
    for column in &index.columns {
        let (ordinal, _) = table.require_column(column)?;
        values.push(row.get(ordinal).unwrap_or(&NULL));
    }
    Ok(values)
}

/// The text one row contributes to a (possibly multi-column) `FullText`
/// index: every named column's `TEXT` value, in the index's own declared
/// order, joined by a single space.
///
/// A space is enough of a boundary — not a distinct marker token — because
/// [`crate::bm25::tokenize`] splits on any non-alphanumeric character, so a
/// term can never straddle the join the way it could if the columns were
/// glued together bare (`"circus"` + `"clown"` must never read as one term
/// `"circusclown"`). This is the same convention Postgres's own guidance for
/// concatenating several columns into one `to_tsvector` input gives.
///
/// A column that is `NULL` or holds a non-text value contributes nothing —
/// not even an empty piece to join — so a single-column index over a `NULL`
/// column still indexes nothing, exactly as it always has. `None` when *no*
/// named column held text, which is when the row is skipped entirely: this
/// is one function for both the insert side (skip means "do not index yet")
/// and the delete side (skip means "cannot have been indexed").
fn concatenated_full_text(
    table: &Table,
    columns: &[String],
    row: &[Value],
) -> Result<Option<String>> {
    let mut parts: Vec<&str> = Vec::with_capacity(columns.len());
    for column in columns {
        let (ordinal, _) = table.require_column(column)?;
        if let Some(Value::Text(text)) = row.get(ordinal) {
            parts.push(text.as_str());
        }
    }
    Ok(if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    })
}

/// Read a `u64` counter out of engine metadata.
fn read_counter(storage: &dyn Storage, key: &str, what: &str) -> Result<Option<u64>> {
    match storage.get_meta(key)? {
        Some(bytes) => {
            let bytes: [u8; 8] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Corrupt(alloc::format!("{what} is malformed")))?;
            Ok(Some(u64::from_le_bytes(bytes)))
        }
        None => Ok(None),
    }
}

/// Metadata key an index's persisted backend lives under.
///
/// Keyed by table and column names, never by index name: a name is a
/// user-facing handle that can change spelling between writes, and would
/// strand a saved index under the old one.
///
/// **A single-column index keeps the exact key it has always had** —
/// `index:<table>:<column>` — because that is this key's on-disk identity and
/// existing databases depend on it never moving; see `docs/indexes.md`. A
/// multi-column index (only [`IndexKind::FullText`] can be one; see
/// [`Index::columns`]) needs a key of its own, and it is built so it can
/// never collide with a legacy single-column key no matter what the columns
/// are named: the third segment begins with `\u{2}`, one column per
/// `\u{2}`-terminated run, and a real column's name — which is exactly what a
/// single-column key's third segment *is* — cannot begin with a control
/// character, the same invariant [`vector_index_namespace`] already relies on
/// for its own leading `\u{1}`.
///
/// This needed no catalog format change to support (`Catalog::required_version`
/// already forces the same version bump a multi-column *B-tree* index does,
/// because the column-list encoding it introduced was never kind-specific);
/// this key is the one place a multi-column retrieval index is genuinely new
/// on disk, and it is additive by construction — nothing about the
/// single-column format moved to make room for it.
fn index_meta_key_for(table: &str, columns: &[String]) -> String {
    let table = table.to_ascii_lowercase();
    match columns {
        [column] => alloc::format!("index:{table}:{}", column.to_ascii_lowercase()),
        columns => {
            let mut key = alloc::format!("index:{table}:\u{2}");
            for column in columns {
                key.push_str(&column.to_ascii_lowercase());
                key.push('\u{2}');
            }
            key
        }
    }
}

/// The in-memory key one retrieval index's backend lives under: table name
/// plus its full, lowercased column list, in the order the index declares
/// them. See [`Engine::text_indexes`] for why this is a list rather than one
/// column.
fn retrieval_key(table: &str, columns: &[String]) -> (String, Vec<String>) {
    (
        table.to_ascii_lowercase(),
        columns
            .iter()
            .map(|column| column.to_ascii_lowercase())
            .collect(),
    )
}

/// Whether `a` and `b` name the same columns, order and case aside.
///
/// Used only to resolve which declared `FullText` index a query's
/// `bm25_score(columns..., query)` means — the concatenation a combined
/// score is built from does not depend on which order the columns were
/// named in, so neither should finding the index.
fn same_column_set(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<String> = a.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut b: Vec<String> = b.iter().map(|c| c.to_ascii_lowercase()).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

/// Metadata key one chunk of a saved index lives under.
fn index_chunk_key(base: &str, chunk: usize) -> String {
    alloc::format!("{base}/{chunk}")
}

/// The synthetic table a paged ANN index keeps its graph in.
///
/// The leading `\u{1}` is what keeps it out of everyone else's way. Row keys
/// are `table_name \0 row_id` and a SQL identifier cannot begin with a control
/// character, so no real table can produce this prefix; engine metadata keys
/// begin with `\0`, so those cannot either. The graph is therefore ordinary
/// rows in the same tree — same log, same recovery — in a namespace nothing
/// else can name.
fn vector_index_namespace(table: &str, column: &str) -> String {
    alloc::format!(
        "\u{1}ann:{}.{}",
        table.to_ascii_lowercase(),
        column.to_ascii_lowercase()
    )
}

/// What the entry at a saved index's base key holds: which write version the
/// index reflects, and how to reassemble it.
struct IndexHeader {
    version: u64,
    chunks: usize,
    length: usize,
}

impl IndexHeader {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&(self.chunks as u64).to_le_bytes());
        out.extend_from_slice(&(self.length as u64).to_le_bytes());
        out
    }

    /// `None` for anything that is not a header — a truncated write, or bytes
    /// from a format we no longer speak. The caller rebuilds instead.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; 24] = bytes.try_into().ok()?;
        Some(Self {
            version: u64::from_le_bytes(bytes[0..8].try_into().ok()?),
            chunks: u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize,
            length: u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize,
        })
    }
}

/// The map key one column's own single-column retrieval index lives under.
///
/// Used only for [`IndexKind::Vector`] lookups, which are always exactly one
/// column (see that variant's docs). A `FullText` lookup goes through
/// [`Engine::resolve_full_text_index`] instead, because it may have to find a
/// *multi*-column index from several columns named in one query.
fn index_key(table: &Table, column: usize) -> Result<(String, Vec<String>)> {
    let column = table
        .columns
        .get(column)
        .ok_or_else(|| Error::Catalog("column ordinal out of range".to_string()))?;
    Ok((
        table.name.to_ascii_lowercase(),
        alloc::vec![column.name.to_ascii_lowercase()],
    ))
}

// -------------------------------------------------------------- window functions
//
// AHL-494. Runs after `Engine::aggregate` (so `row.values`/`row.aggregates`
// are already whatever a `GROUP BY` folded them to, or the plain joined row
// otherwise) and before `distinct_rows`/`sort_rows`/`LIMIT`. One partition at
// a time, one window function at a time: a partition is a short-lived `Vec`
// of original row indices, never a second copy of the rows themselves.

/// The `Computed` a window's own `PARTITION BY`/`ORDER BY`/args/`FILTER`
/// evaluate against: the row's already-folded aggregates, never another
/// window's result — a window function may not reference another one
/// (`sql.rs::resolve_window_function` refuses it at plan time), so `windows`
/// is always empty here.
fn window_row_computed(row: &ExecRow) -> Computed<'_> {
    Computed {
        aggregates: &row.aggregates,
        windows: &[],
    }
}

/// Evaluate every [`SelectPlan::windows`] entry, filling [`ExecRow::windows`].
///
/// One function at a time, over the *whole* row set: each has its own
/// `PARTITION BY`, so the partitioning for one window function has nothing to
/// do with another's, and re-partitioning per function (rather than
/// partitioning once and answering every function per group) is what keeps
/// that true without threading a join of every function's partition key
/// together.
fn window(plan: &SelectPlan, mut rows: Vec<ExecRow>, env: &Env<'_>) -> Result<Vec<ExecRow>> {
    for row in &mut rows {
        row.windows = alloc::vec![Value::Null; plan.windows.len()];
    }
    if rows.is_empty() {
        return Ok(rows);
    }

    for (window_index, wf) in plan.windows.iter().enumerate() {
        // The partition key for every row, evaluated once per row rather than
        // once per comparison — the same reason `sort_rows` evaluates its
        // keys up front instead of inside the comparator.
        let mut keys: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut key = Vec::with_capacity(wf.partition_by.len());
            for expr in &wf.partition_by {
                key.push(eval::evaluate(
                    expr,
                    &row.values,
                    window_row_computed(row),
                    env,
                )?);
            }
            keys.push(key);
        }

        // A stable sort groups equal partition keys into contiguous runs
        // without disturbing their relative order — which is what makes a
        // `ROWS` frame's tie-break the input order, confirmed against
        // sqlite3 (see `WindowFrame`'s doc and the sqllogictest file).
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_by(|&a, &b| compare_group_keys(&keys[a], &keys[b], &wf.partition_collations));

        let mut start = 0;
        while start < order.len() {
            let mut end = start + 1;
            while end < order.len()
                && compare_group_keys(
                    &keys[order[start]],
                    &keys[order[end]],
                    &wf.partition_collations,
                ) == core::cmp::Ordering::Equal
            {
                end += 1;
            }
            evaluate_window_partition(wf, window_index, &order[start..end], &mut rows, env)?;
            start = end;
        }
    }
    Ok(rows)
}

/// Evaluate one window function over one partition — `partition` is the
/// original row indices in that partition, in partition-sort order (which
/// preserves their relative input order, since the sort that produced it was
/// stable).
fn evaluate_window_partition(
    wf: &WindowFn,
    window_index: usize,
    partition: &[usize],
    rows: &mut [ExecRow],
    env: &Env<'_>,
) -> Result<()> {
    // The partition's own sequence: sorted by the window's `ORDER BY`,
    // stably — ties keep the partition order above, which is itself stable
    // from the original row order. An empty `ORDER BY` leaves the partition
    // order as the sequence, which is what makes the whole-partition default
    // frame order-independent (every row's frame is the same set regardless
    // of sequence order).
    let mut sequence: Vec<usize> = partition.to_vec();
    if !wf.order_by.is_empty() {
        let mut keyed: Vec<(Vec<SortKey>, usize)> = Vec::with_capacity(sequence.len());
        for &index in &sequence {
            keyed.push((window_sort_keys(&wf.order_by, &rows[index], env)?, index));
        }
        keyed.sort_by(|a, b| compare_sort_keys(&a.0, &b.0, &wf.order_by));
        sequence = keyed.into_iter().map(|(_, index)| index).collect();
    }
    let n = sequence.len();

    // Peer-group boundaries (index-into-`sequence`, inclusive), needed by
    // `RANK`/`DENSE_RANK` and by the default frame's peer-group-aware end
    // bound (`WindowFrame`'s doc). Only computed when there is an `ORDER BY`
    // to tie on; with none, every row is the whole partition's own peer
    // (`rank`/`dense_rank` never actually reach that case without an
    // `ORDER BY` producing a total order, but the default frame does, and
    // `WindowFrame::whole_partition` is exactly "every row's peer group is
    // the whole partition").
    let peer_end: Vec<usize> = if wf.order_by.is_empty() {
        alloc::vec![n.saturating_sub(1); n]
    } else {
        let mut keys: Vec<Vec<SortKey>> = Vec::with_capacity(n);
        for &index in &sequence {
            keys.push(window_sort_keys(&wf.order_by, &rows[index], env)?);
        }
        let mut ends = alloc::vec![0usize; n];
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            while j < n
                && compare_sort_keys(&keys[i], &keys[j], &wf.order_by) == core::cmp::Ordering::Equal
            {
                j += 1;
            }
            for slot in ends.iter_mut().take(j).skip(i) {
                *slot = j - 1;
            }
            i = j;
        }
        ends
    };

    match wf.func {
        WindowFunc::RowNumber => {
            for (position, &row_index) in sequence.iter().enumerate() {
                rows[row_index].windows[window_index] = Value::Integer(position as i64 + 1);
            }
        }
        WindowFunc::Rank | WindowFunc::DenseRank => {
            let mut rank = 1i64;
            let mut dense = 1i64;
            let mut position = 0;
            while position < n {
                let end = peer_end[position];
                let value = Value::Integer(if wf.func == WindowFunc::Rank {
                    rank
                } else {
                    dense
                });
                for &row_index in &sequence[position..=end] {
                    rows[row_index].windows[window_index] = value.clone();
                }
                rank += (end - position + 1) as i64;
                dense += 1;
                position = end + 1;
            }
        }
        WindowFunc::Ntile => {
            let buckets = match eval::evaluate(&wf.args[0], &[], Computed::NONE, env)? {
                Value::Integer(n) => n,
                Value::Real(n) if n as i64 as f64 == n => n as i64,
                _ => {
                    return Err(Error::Type(
                        "argument of ntile must be a positive integer".to_string(),
                    ))
                }
            };
            if buckets < 1 {
                return Err(Error::Type(
                    "argument of ntile must be a positive integer".to_string(),
                ));
            }
            for (position, &row_index) in sequence.iter().enumerate() {
                rows[row_index].windows[window_index] =
                    Value::Integer(ntile_bucket(position, n, buckets));
            }
        }
        WindowFunc::Lag | WindowFunc::Lead => {
            let direction = if wf.func == WindowFunc::Lag {
                -1i64
            } else {
                1i64
            };
            for (position, &row_index) in sequence.iter().enumerate() {
                let computed = window_row_computed(&rows[row_index]);
                let offset = match wf.args.get(1) {
                    Some(expr) => {
                        match eval::evaluate(expr, &rows[row_index].values, computed, env)? {
                            Value::Integer(n) => n,
                            Value::Real(n) if n as i64 as f64 == n => n as i64,
                            Value::Null => 1,
                            other => {
                                return Err(Error::Type(alloc::format!(
                                    "{}() offset must be an integer, got {}",
                                    if wf.func == WindowFunc::Lag {
                                        "lag"
                                    } else {
                                        "lead"
                                    },
                                    other.type_name()
                                )))
                            }
                        }
                    }
                    None => 1,
                };
                // A negative offset reaches the other way — confirmed
                // against sqlite3: `lag(x, -1)` answers exactly what
                // `lead(x, 1)` would.
                let target = position as i64 + direction * offset;
                let value = if target >= 0 && (target as usize) < n {
                    let source = sequence[target as usize];
                    eval::evaluate(
                        &wf.args[0],
                        &rows[source].values,
                        window_row_computed(&rows[source]),
                        env,
                    )?
                } else {
                    match wf.args.get(2) {
                        Some(expr) => eval::evaluate(expr, &rows[row_index].values, computed, env)?,
                        None => Value::Null,
                    }
                };
                rows[row_index].windows[window_index] = value;
            }
        }
        WindowFunc::FirstValue
        | WindowFunc::LastValue
        | WindowFunc::NthValue
        | WindowFunc::Agg(_) => {
            let start_bound = resolve_frame_bound_once(&wf.frame.start, env, "starting")?;
            let end_bound = resolve_frame_bound_once(&wf.frame.end, env, "ending")?;
            for position in 0..n {
                let frame = frame_range(
                    &start_bound,
                    &end_bound,
                    wf.frame.rows,
                    position,
                    peer_end[position],
                    n,
                );
                let row_index = sequence[position];
                let value = match (wf.func, frame) {
                    (_, None) => Value::Null,
                    (WindowFunc::FirstValue, Some((first, _))) => {
                        let source = sequence[first];
                        eval::evaluate(
                            &wf.args[0],
                            &rows[source].values,
                            window_row_computed(&rows[source]),
                            env,
                        )?
                    }
                    (WindowFunc::LastValue, Some((_, last))) => {
                        let source = sequence[last];
                        eval::evaluate(
                            &wf.args[0],
                            &rows[source].values,
                            window_row_computed(&rows[source]),
                            env,
                        )?
                    }
                    (WindowFunc::NthValue, Some((first, last))) => {
                        let nth = match eval::evaluate(
                            &wf.args[1],
                            &rows[row_index].values,
                            window_row_computed(&rows[row_index]),
                            env,
                        )? {
                            Value::Integer(n) => n,
                            Value::Real(n) if n as i64 as f64 == n => n as i64,
                            _ => {
                                return Err(Error::Type(
                                    "second argument to nth_value must be a positive integer"
                                        .to_string(),
                                ))
                            }
                        };
                        if nth < 1 {
                            return Err(Error::Type(
                                "second argument to nth_value must be a positive integer"
                                    .to_string(),
                            ));
                        }
                        // `i64` arithmetic here, not `usize`: `nth` came
                        // straight from the caller and an astronomically
                        // large value must answer `NULL` (out of range)
                        // rather than overflow a `usize` addition.
                        let target = (first as i64).saturating_add(nth - 1);
                        if target > last as i64 {
                            Value::Null
                        } else {
                            let source = sequence[target as usize];
                            eval::evaluate(
                                &wf.args[0],
                                &rows[source].values,
                                window_row_computed(&rows[source]),
                                env,
                            )?
                        }
                    }
                    (WindowFunc::Agg(func), Some((first, last))) => {
                        let group: Vec<&[Value]> = sequence[first..=last]
                            .iter()
                            .map(|&index| rows[index].values.as_slice())
                            .collect();
                        let synthetic = Aggregate {
                            func,
                            arg: wf.args.first().cloned(),
                            distinct: false,
                            separator: wf.args.get(1).cloned(),
                            collation: wf.collation,
                            filter: wf.filter.clone(),
                        };
                        eval::evaluate_aggregate(&synthetic, &group, env)?
                    }
                    _ => unreachable!("only FirstValue/LastValue/NthValue/Agg read the frame"),
                };
                rows[row_index].windows[window_index] = value;
            }
        }
    }
    Ok(())
}

/// `ntile(buckets)`'s bucket for the row at 0-based `position` of `total`
/// rows: buckets are as even as possible, with the earlier buckets one row
/// larger when `total` does not divide evenly — confirmed against sqlite3,
/// 5 rows into 2 buckets is 3 then 2, not 2 then 3.
fn ntile_bucket(position: usize, total: usize, buckets: i64) -> i64 {
    let buckets = buckets as usize;
    let base = total / buckets;
    let remainder = total % buckets;
    let larger_rows = remainder * (base + 1);
    let bucket0 = if position < larger_rows {
        position / (base + 1)
    } else {
        // `base` is at least 1 here: `base == 0` only when `buckets > total`,
        // which makes `remainder == total` and `larger_rows == total`, so
        // every `position < total` takes the branch above.
        remainder + (position - larger_rows) / base
    };
    bucket0 as i64 + 1
}

/// One [`FrameBound`] with its offset expression (constant for the whole
/// statement, the same assumption [`row_count`] makes for `LIMIT`/`OFFSET`)
/// evaluated once rather than once per row.
enum ResolvedBound {
    UnboundedPreceding,
    Preceding(i64),
    CurrentRow,
    Following(i64),
    UnboundedFollowing,
}

fn resolve_frame_bound_once(
    bound: &FrameBound,
    env: &Env<'_>,
    edge: &str,
) -> Result<ResolvedBound> {
    Ok(match bound {
        FrameBound::UnboundedPreceding => ResolvedBound::UnboundedPreceding,
        FrameBound::CurrentRow => ResolvedBound::CurrentRow,
        FrameBound::UnboundedFollowing => ResolvedBound::UnboundedFollowing,
        FrameBound::Preceding(expr) => ResolvedBound::Preceding(frame_offset(expr, env, edge)?),
        FrameBound::Following(expr) => ResolvedBound::Following(frame_offset(expr, env, edge)?),
    })
}

/// A frame bound's `<expr>` in `<expr> PRECEDING`/`<expr> FOLLOWING` — SQLite
/// requires a non-negative integer there (confirmed against sqlite3: `ROWS
/// BETWEEN -1 PRECEDING AND CURRENT ROW` fails with exactly this wording at
/// run time, not at prepare time, which is why this is here and not in
/// `sql.rs`).
fn frame_offset(expr: &crate::plan::Expr, env: &Env<'_>, edge: &str) -> Result<i64> {
    let value = eval::evaluate(expr, &[], Computed::NONE, env)?;
    let offset = match value {
        Value::Integer(n) => n,
        Value::Real(n) if n as i64 as f64 == n => n as i64,
        _ => {
            return Err(Error::Type(alloc::format!(
                "frame {edge} offset must be a non-negative integer"
            )))
        }
    };
    if offset < 0 {
        return Err(Error::Type(alloc::format!(
            "frame {edge} offset must be a non-negative integer"
        )));
    }
    Ok(offset)
}

/// A bound's row position relative to `position`, using `i64::MIN`/`MAX` as
/// sentinels for the two `UNBOUNDED` variants so that the emptiness and
/// clamping arithmetic in [`frame_range`] does not need to special-case them.
fn bound_position(bound: &ResolvedBound, position: i64) -> i64 {
    match bound {
        ResolvedBound::UnboundedPreceding => i64::MIN,
        ResolvedBound::Preceding(offset) => position.saturating_sub(*offset),
        ResolvedBound::CurrentRow => position,
        ResolvedBound::Following(offset) => position.saturating_add(*offset),
        ResolvedBound::UnboundedFollowing => i64::MAX,
    }
}

/// The frame for the row at `position` (0-based, within a partition of `n`
/// rows), as an inclusive `(first, last)` pair of indices into the
/// partition's sequence — `None` when the frame is empty.
///
/// `rows` (an explicit `ROWS` frame) counts positions literally. The default,
/// `RANGE`-shaped frame (`!rows`) reinterprets a `CURRENT ROW` *end* bound as
/// "the end of this row's peer group" (`peer_end`) instead of the row's own
/// position — see [`WindowFrame`]'s doc for the sqlite3 measurement this
/// implements; every other bound (both defaults ever construct are
/// `UNBOUNDED PRECEDING`/`UNBOUNDED FOLLOWING`/this one `CURRENT ROW`) reads
/// the same either way.
///
/// Emptiness is decided from the *unclamped* positions first — a frame like
/// `2 PRECEDING AND 5 PRECEDING` is empty at *every* row (the start is always
/// later than the end), which clamping each bound independently to the
/// partition would hide; only once a frame is known non-empty are its bounds
/// clamped into `0..n`. Confirmed against sqlite3 (see the sqllogictest
/// file's frame-past-the-edge cases).
fn frame_range(
    start: &ResolvedBound,
    end: &ResolvedBound,
    rows: bool,
    position: usize,
    peer_end: usize,
    n: usize,
) -> Option<(usize, usize)> {
    let i = position as i64;
    let raw_start = bound_position(start, i);
    let raw_end = if !rows && matches!(end, ResolvedBound::CurrentRow) {
        peer_end as i64
    } else {
        bound_position(end, i)
    };
    if n == 0 || raw_start > raw_end || raw_end < 0 || raw_start > n as i64 - 1 {
        return None;
    }
    let first = raw_start.max(0) as usize;
    let last = raw_end.min(n as i64 - 1) as usize;
    Some((first, last))
}

/// A window's `ORDER BY` keys for one row, evaluated with the same
/// [`SortKey`] the query-level `ORDER BY` uses (see [`sort_rows`]) — the
/// comparator [`compare_sort_keys`] then applies is the identical one, not a
/// second copy, which is what keeps a window's ordering agreeing with
/// sqlite3's total order and affinity rules (AHL-477/AHL-486) the same way
/// `ORDER BY` itself does.
fn window_sort_keys(order: &[Order], row: &ExecRow, env: &Env<'_>) -> Result<Vec<SortKey>> {
    let mut keys = Vec::with_capacity(order.len());
    for term in order {
        keys.push(match &term.key {
            OrderKey::Score => SortKey::Score(row.score.unwrap_or(f32::MIN)),
            OrderKey::Column(index) => {
                SortKey::Value(row.values.get(*index).cloned().unwrap_or(Value::Null))
            }
            OrderKey::Expr(expr) => SortKey::Value(eval::evaluate(
                expr,
                &row.values,
                window_row_computed(row),
                env,
            )?),
        });
    }
    Ok(keys)
}

/// Compare two rows' [`window_sort_keys`] term by term, the same way
/// [`sort_rows`]'s own comparator does (minus the row-id tie-break, which a
/// window's sequence gets from the stable sort that built it instead).
fn compare_sort_keys(a: &[SortKey], b: &[SortKey], order: &[Order]) -> core::cmp::Ordering {
    for (index, term) in order.iter().enumerate() {
        let ordering = a[index].compare(&b[index], term);
        if ordering != core::cmp::Ordering::Equal {
            return ordering;
        }
    }
    core::cmp::Ordering::Equal
}

/// Sort the result rows by every `ORDER BY` term in turn. Ties always break on
/// row id so a query returns the same order on every run.
fn sort_rows(mut rows: Vec<ExecRow>, order: &[Order], env: &Env<'_>) -> Result<Vec<ExecRow>> {
    if order.is_empty() {
        rows.sort_by_key(|row| row.id);
        return Ok(rows);
    }

    // Evaluate each sort key once per row (an expression is evaluated here,
    // not once per comparison), then sort by the whole tuple. The row is
    // *moved* into its keyed form and back out again: this used to clone every
    // value twice, which on a wide row is a heap allocation per text cell per
    // sort.
    let mut keyed: Vec<KeyedRow> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut keys = Vec::with_capacity(order.len());
        for term in order {
            keys.push(match &term.key {
                OrderKey::Score => SortKey::Score(row.score.unwrap_or(f32::MIN)),
                OrderKey::Column(index) => {
                    SortKey::Value(row.values.get(*index).cloned().unwrap_or(Value::Null))
                }
                OrderKey::Expr(expr) => SortKey::Value(eval::evaluate(
                    expr,
                    &row.values,
                    Computed {
                        aggregates: &row.aggregates,
                        windows: &row.windows,
                    },
                    env,
                )?),
            });
        }
        keyed.push(KeyedRow { keys, row });
    }

    keyed.sort_by(|a, b| {
        for (index, term) in order.iter().enumerate() {
            let ordering = a.keys[index].compare(&b.keys[index], term);
            if ordering != core::cmp::Ordering::Equal {
                return ordering;
            }
        }
        a.row.id.cmp(&b.row.id)
    });

    Ok(keyed.into_iter().map(|keyed| keyed.row).collect())
}

/// A row paired with its evaluated sort keys, ready to be ordered.
struct KeyedRow {
    keys: Vec<SortKey>,
    row: ExecRow,
}

/// A sort key: a retrieval score, or a plain value (a column or an expression).
enum SortKey {
    Score(f32),
    Value(Value),
}

impl SortKey {
    /// Compare under one `ORDER BY` term, honouring its direction and its
    /// `NULL` placement.
    ///
    /// The two are independent: `DESC` reverses the ordering of values, but
    /// where `NULL` lands is decided separately, because
    /// `ORDER BY x DESC NULLS LAST` has to mean something different from
    /// `ORDER BY x DESC` with `NULL` reversed along with everything else.
    /// Only the default placement makes the two agree.
    fn compare(&self, other: &Self, term: &Order) -> core::cmp::Ordering {
        use core::cmp::Ordering;

        if let (SortKey::Value(a), SortKey::Value(b)) = (self, other) {
            match (a == &Value::Null, b == &Value::Null) {
                (true, true) => return Ordering::Equal,
                (true, false) => {
                    return if term.nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    return if term.nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, false) => {}
            }
        }

        let ordering = match (self, other) {
            (SortKey::Score(a), SortKey::Score(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (SortKey::Value(a), SortKey::Value(b)) => compare_values(a, b, term.collation),
            // Mixed keys only occur if a plan is malformed; order by nothing.
            _ => Ordering::Equal,
        };
        if term.desc {
            ordering.reverse()
        } else {
            ordering
        }
    }
}

/// `ORDER BY`'s and `GROUP BY`'s general sort-key comparator: SQLite's fixed
/// storage-class order, `NULL` below every number (INTEGER and REAL
/// interleaved by value) below `TEXT` below `BLOB` — confirmed against a real
/// sqlite3 3.54 binary, including that `1` and `1.0` compare equal and that a
/// `TEXT`/`BLOB` pair with identical bytes never does (`TEXT` always sorts
/// below `BLOB`).
///
/// This **used to fall back to `Ordering::Equal`** for a pair that was
/// neither `NULL`-involving, same-class-numeric, same-class-text nor
/// same-class-blob (a `TEXT`/`INTEGER` pair, say) — which is not merely wrong
/// for that one pair: `sort_by` requires a total order, and a comparator that
/// answers "equal" for values it has no rule for lets an intransitive triple
/// through, corrupting the whole sort rather than misplacing one row.
/// [`eval::mem_cmp`] already had the right rule — the same one `DISTINCT`,
/// `UNION`'s dedup and index keys use — so this now defers to it entirely
/// rather than keeping a second, independently-wrong copy: one
/// implementation is the only way to keep `ORDER BY`, `GROUP BY`, `DISTINCT`,
/// set-operation dedup, `MIN`/`MAX` and index-vs-scan agreement from drifting
/// apart again. `collation` composes exactly as it did before — `mem_cmp`
/// takes it and consults it for a `TEXT` pair and nothing else, so `COLLATE`
/// keeps deciding how two `TEXT` values compare while the class order above
/// it stays fixed.
fn compare_values(left: &Value, right: &Value, collation: Collation) -> core::cmp::Ordering {
    eval::mem_cmp(left, right, collation)
}

/// A `LIMIT` or `OFFSET` expression as a row count.
///
/// SQLite reads whatever it is given as an integer, treats a `NULL` or a
/// negative number as "no limit", and clamps a negative `OFFSET` to zero — so
/// none of those is an error here either.
fn row_count(expr: Option<&crate::plan::Expr>, env: &Env<'_>) -> Result<Option<usize>> {
    let Some(expr) = expr else {
        return Ok(None);
    };
    let value = eval::evaluate(expr, &[], Computed::NONE, env)?;
    Ok(match value {
        Value::Null => None,
        Value::Integer(count) => usize::try_from(count).ok(),
        Value::Real(count) => usize::try_from(count as i64).ok(),
        other => {
            return Err(Error::Type(alloc::format!(
                "LIMIT/OFFSET must be a number, got {}",
                other.type_name()
            )))
        }
    })
}

/// Fold rows that project the same values into one, keeping the first.
///
/// The comparison is SQLite's storage-class ordering, the same one
/// `COUNT(DISTINCT x)` uses, so `1` and `1.0` are one row.
fn distinct_rows(
    items: &[SelectItem],
    collations: &[Collation],
    rows: Vec<ExecRow>,
    env: &Env<'_>,
) -> Result<Vec<ExecRow>> {
    let mut projected = Vec::with_capacity(rows.len());
    for row in &rows {
        projected.push(project_row(items, row, env)?);
    }
    let keep = duplicate_keep_mask(&projected, collations, Keep::First);
    Ok(rows
        .into_iter()
        .zip(keep)
        .filter_map(|(row, keep)| keep.then_some(row))
        .collect())
}

/// Which occurrence of a group of equal rows [`duplicate_keep_mask`] keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// The one that occurs earliest in the input — `SELECT DISTINCT`'s rule,
    /// the same one an ordinary table scan's row order already gives a plain
    /// `GROUP BY`.
    First,
    /// The one that occurs latest — `UNION`'s rule: measured against
    /// sqlite3, a case-only collision between the left and right arms of a
    /// `UNION` under a `NOCASE` column keeps the *right* arm's bytes, not
    /// the left's (`combine_set_operation`'s doc has the measurement).
    Last,
}

/// For each row, whether it survives folding rows that compare equal (under
/// `collations`, SQLite's storage-class ordering) into one — the sort-then-
/// scan `distinct_rows` used to do inline, generalised so [`combine_set_operation`]
/// can reuse it for `UNION`'s deduplication pass, which needs the opposite
/// tie-break (see [`Keep`]).
///
/// Sorted rather than quadratic, then handed back in the *input*'s order (a
/// `Vec<bool>` the same length as `rows`) so a caller with no `ORDER BY` of
/// its own still answers in whatever order it scanned or concatenated.
fn duplicate_keep_mask(rows: &[Vec<Value>], collations: &[Collation], keep: Keep) -> Vec<bool> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| compare_projections(&rows[a], &rows[b], collations).then(a.cmp(&b)));

    let mut keep_flags = alloc::vec![false; rows.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i + 1;
        while j < order.len()
            && compare_projections(&rows[order[i]], &rows[order[j]], collations)
                == core::cmp::Ordering::Equal
        {
            j += 1;
        }
        let survivor = match keep {
            Keep::First => order[i],
            Keep::Last => order[j - 1],
        };
        keep_flags[survivor] = true;
        i = j;
    }
    keep_flags
}

/// Combine two arms of a `UNION`/`INTERSECT`/`EXCEPT` into the compound's
/// rows, SQLite's semantics rather than the SQL standard's — see
/// `sql.rs::plan_compound`'s doc for the sqlite3 measurements this
/// implements. All four dedup via [`duplicate_keep_mask`], reusing the same
/// sort-and-fold [`distinct_rows`] already used, not a separate
/// implementation:
///
/// * `UNION ALL` — concatenate, keep every row, left arm first.
/// * `UNION` — concatenate, then fold: a colliding group keeps its *last*
///   occurrence, so a row from the right arm overwrites an equal one from
///   the left.
/// * `INTERSECT` — fold the left arm alone (first occurrence wins, same as
///   `DISTINCT`), then keep only the rows that have a match anywhere in the
///   right arm.
/// * `EXCEPT` — the same left-arm fold, keeping only the rows that have *no*
///   match in the right arm.
///
/// `INTERSECT`/`EXCEPT` never fold the right arm's own duplicates — only
/// whether *some* row of it matches is asked, via a sort-then-binary-search
/// rather than folding it too, since folding would cost more than it saves.
fn combine_set_operation(
    op: SetOp,
    mut left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
    collations: &[Collation],
) -> Vec<Vec<Value>> {
    match op {
        SetOp::UnionAll => {
            left.extend(right);
            left
        }
        SetOp::Union => {
            left.extend(right);
            let keep = duplicate_keep_mask(&left, collations, Keep::Last);
            left.into_iter()
                .zip(keep)
                .filter_map(|(row, keep)| keep.then_some(row))
                .collect()
        }
        SetOp::Intersect | SetOp::Except => {
            let keep = duplicate_keep_mask(&left, collations, Keep::First);
            let deduped: Vec<Vec<Value>> = left
                .into_iter()
                .zip(keep)
                .filter_map(|(row, keep)| keep.then_some(row))
                .collect();

            let mut right_sorted = right;
            right_sorted.sort_by(|a, b| compare_projections(a, b, collations));
            let matches_right = |row: &Vec<Value>| {
                right_sorted
                    .binary_search_by(|probe| compare_projections(probe, row, collations))
                    .is_ok()
            };

            deduped
                .into_iter()
                .filter(|row| matches_right(row) == (op == SetOp::Intersect))
                .collect()
        }
    }
}

fn compare_projections(
    left: &[Value],
    right: &[Value],
    collations: &[Collation],
) -> core::cmp::Ordering {
    for (position, (a, b)) in left.iter().zip(right.iter()).enumerate() {
        let ordering = eval::mem_cmp(a, b, crate::collation::at(collations, position));
        if ordering != core::cmp::Ordering::Equal {
            return ordering;
        }
    }
    core::cmp::Ordering::Equal
}

/// A stored row a write would collide with, and the constraint it collides on.
///
/// The constraint matters as well as the row, because an `ON CONFLICT (...)`
/// target only answers for the constraint it names.
struct Conflict {
    id: RowId,
    values: Vec<Value>,
    /// The constraint's columns as ordinals: a `UNIQUE` group, or the one
    /// column that is the row-id alias. Empty when a table with no alias
    /// somehow collided on its key, which cannot match any target.
    columns: Vec<usize>,
}

/// Whether two constraints cover the same columns, in any order.
fn same_columns(left: &[usize], right: &[usize]) -> bool {
    left.len() == right.len() && left.iter().all(|column| right.contains(column))
}

/// Whether two rows share the key one `UNIQUE` constraint covers.
///
/// SQLite's two rules, and both matter. **A `NULL` never collides**, with
/// anything, including another `NULL` — which is why a nullable unique column
/// can hold any number of empty rows. And the comparison is by storage class:
/// the integer `1` and the real `1.0` are the same key, the text `'1'` is not.
fn unique_key_collides(
    group: &[usize],
    collations: &[Collation],
    row: &[Value],
    other: &[Value],
) -> bool {
    group.iter().enumerate().all(|(position, ordinal)| {
        let left = row.get(*ordinal).unwrap_or(&Value::Null);
        let right = other.get(*ordinal).unwrap_or(&Value::Null);
        if *left == Value::Null || *right == Value::Null {
            return false;
        }
        match (left, right) {
            (Value::Integer(_) | Value::Real(_), Value::Integer(_) | Value::Real(_)) => {
                match (left.as_f64(), right.as_f64()) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                }
            }
            // Two `TEXT` values collide when the *constraint's* collation says
            // they are equal — which for a `NOCASE` column makes `'Ada'` and
            // `'ADA'` one key, and is the whole point of declaring it.
            (Value::Text(a), Value::Text(b)) => {
                crate::collation::at(collations, position).compare(a, b)
                    == core::cmp::Ordering::Equal
            }
            _ => left == right,
        }
    })
}

/// The error a uniqueness collision raises, naming the columns that collided.
///
/// The message is SQLite's, and it is chosen deliberately: it is raised from
/// the index-backed check and from the scan alike, so which one answered a
/// given constraint is invisible to a caller matching on it — which is what
/// let the index replace the scan underneath without changing behaviour.
fn conflict_error(table: &Table, conflict: &Conflict) -> Error {
    let mut names: Vec<String> = conflict
        .columns
        .iter()
        .map(|ordinal| alloc::format!("{}.{}", table.name, table.columns[*ordinal].name))
        .collect();
    if names.is_empty() {
        names.push(alloc::format!("{}.rowid", table.name));
    }
    Error::Constraint(alloc::format!(
        "UNIQUE constraint failed: {}",
        names.join(", ")
    ))
}

/// The values one row contributes to the result set.
fn project_row(items: &[SelectItem], row: &ExecRow, env: &Env<'_>) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(match item {
            SelectItem::Column { index, .. } => {
                row.values.get(*index).cloned().unwrap_or(Value::Null)
            }
            SelectItem::Expr { expr, .. } => eval::evaluate(
                expr,
                &row.values,
                Computed {
                    aggregates: &row.aggregates,
                    windows: &row.windows,
                },
                env,
            )?,
            SelectItem::Score { .. } => match row.score {
                Some(score) => Value::Real(f64::from(score)),
                None => Value::Null,
            },
        });
    }
    Ok(out)
}

/// Compare two `GROUP BY` keys lexicographically by [`compare_values`], each
/// under the collation its own key expression resolved.
fn compare_group_keys(
    left: &[Value],
    right: &[Value],
    collations: &[Collation],
) -> core::cmp::Ordering {
    for (position, (a, b)) in left.iter().zip(right.iter()).enumerate() {
        let ordering = compare_values(a, b, crate::collation::at(collations, position));
        if ordering != core::cmp::Ordering::Equal {
            return ordering;
        }
    }
    core::cmp::Ordering::Equal
}

/// One output column of a projection that can be satisfied by moving a value
/// out of the row rather than cloning it.
enum Moved {
    /// Take the value at this ordinal.
    Column(usize),
    /// The retrieval score, which is a `f32` and costs nothing to copy.
    Score,
}

/// The moves that answer `items`, or `None` when the projection has to clone.
///
/// Two conditions, both load-bearing. **No expression**, because expressions
/// are evaluated in item order and one that read a column an earlier item had
/// already moved out would see the `NULL` left in its place. **No repeated
/// ordinal**, for exactly the same reason — `SELECT a, a FROM t` would report
/// the second `a` as `NULL`.
///
/// What is left is the shape most queries have: `SELECT *`, or a list of plain
/// columns. `PERF.md` names `Engine::project` cloning every value a second time
/// as one of the three remaining allocation sources on the point-read path;
/// this is that clone removed for those queries.
fn moving_projection(items: &[SelectItem]) -> Option<Vec<Moved>> {
    let mut moves = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SelectItem::Column { index, .. } => {
                if moves
                    .iter()
                    .any(|taken| matches!(taken, Moved::Column(seen) if seen == index))
                {
                    return None;
                }
                moves.push(Moved::Column(*index));
            }
            SelectItem::Score { .. } => moves.push(Moved::Score),
            SelectItem::Expr { .. } => return None,
        }
    }
    Some(moves)
}

fn project(items: &[SelectItem], rows: Vec<ExecRow>, env: &Env<'_>) -> Result<ResultSet> {
    let columns = items.iter().map(|item| item.label().to_string()).collect();
    let mut out_rows = Vec::with_capacity(rows.len());
    match moving_projection(items) {
        Some(moves) => {
            for mut row in rows {
                let mut values = Vec::with_capacity(moves.len());
                for taken in &moves {
                    values.push(match taken {
                        Moved::Column(index) => row
                            .values
                            .get_mut(*index)
                            .map(|slot| core::mem::replace(slot, Value::Null))
                            .unwrap_or(Value::Null),
                        Moved::Score => match row.score {
                            Some(score) => Value::Real(f64::from(score)),
                            None => Value::Null,
                        },
                    });
                }
                out_rows.push(values);
            }
        }
        None => {
            for row in &rows {
                out_rows.push(project_row(items, row, env)?);
            }
        }
    }
    Ok(ResultSet {
        columns,
        rows: out_rows,
    })
}

/// Every column of the driving tables a planned `SELECT` can observe.
///
/// The union of the `WHERE`, the `ON` predicates, the projection, `GROUP BY`,
/// `HAVING`, every aggregate argument and every `ORDER BY` term. Ordinals are
/// into the *joined* row — the concatenation of the tables in `FROM` order —
/// which is how the plan holds them; [`ColumnMask::slice`] rebases them onto one
/// table when its bytes are decoded.
///
/// Anything not in this set decodes as `NULL`, so the rule for changing it is
/// blunt: if a new construct can read a stored column, it is walked here or the
/// query returns the wrong answer.
fn needed_columns(plan: &SelectPlan) -> ColumnMask {
    let width: usize = plan.from.iter().map(|item| item.table.columns.len()).sum();
    let mut mask = ColumnMask::none(width);
    if let Some(filter) = &plan.filter {
        filter.columns_read(&mut mask);
    }
    for join in &plan.joins {
        if let Some(on) = &join.on {
            on.columns_read(&mut mask);
        }
    }
    for item in &plan.items {
        match item {
            SelectItem::Column { index, .. } => mask.add(*index),
            SelectItem::Expr { expr, .. } => expr.columns_read(&mut mask),
            SelectItem::Score { .. } => {}
        }
    }
    for expr in &plan.group_by {
        expr.columns_read(&mut mask);
    }
    if let Some(having) = &plan.having {
        having.columns_read(&mut mask);
    }
    for aggregate in &plan.aggregates {
        if let Some(arg) = &aggregate.arg {
            arg.columns_read(&mut mask);
        }
        if let Some(separator) = &aggregate.separator {
            separator.columns_read(&mut mask);
        }
        if let Some(filter) = &aggregate.filter {
            filter.columns_read(&mut mask);
        }
    }
    for window in &plan.windows {
        for arg in &window.args {
            arg.columns_read(&mut mask);
        }
        if let Some(filter) = &window.filter {
            filter.columns_read(&mut mask);
        }
        for expr in &window.partition_by {
            expr.columns_read(&mut mask);
        }
        for term in &window.order_by {
            match &term.key {
                OrderKey::Score => {}
                OrderKey::Column(index) => mask.add(*index),
                OrderKey::Expr(expr) => expr.columns_read(&mut mask),
            }
        }
        // The frame's own bound expressions are evaluated against an empty
        // row (see `engine::frame_offset`), so a column reference in one is
        // already an error rather than a read — walked anyway for the same
        // reason `LIMIT`/`OFFSET` are, below.
        for bound in [&window.frame.start, &window.frame.end] {
            match bound {
                FrameBound::Preceding(expr) | FrameBound::Following(expr) => {
                    expr.columns_read(&mut mask)
                }
                FrameBound::UnboundedPreceding
                | FrameBound::CurrentRow
                | FrameBound::UnboundedFollowing => {}
            }
        }
    }
    for term in &plan.order {
        match &term.key {
            OrderKey::Score => {}
            OrderKey::Column(index) => mask.add(*index),
            OrderKey::Expr(expr) => expr.columns_read(&mut mask),
        }
    }
    // `LIMIT`/`OFFSET` are evaluated against an empty row, so a column
    // reference in one is already an error rather than a read — walking them
    // costs nothing and keeps this function's rule ("everything that can
    // observe a column") true without an exception to remember.
    if let Some(limit) = &plan.limit {
        limit.columns_read(&mut mask);
    }
    if let Some(offset) = &plan.offset {
        offset.columns_read(&mut mask);
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cmp::Ordering;

    fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }
    fn blob(bytes: &[u8]) -> Value {
        Value::Blob(bytes.to_vec())
    }

    /// The same mixed-class corpus `eval.rs`'s `mem_cmp_is_a_total_order_...`
    /// test exhausts, kept in sync deliberately rather than shared: this one
    /// pins `compare_values` — the exact function `docs/architecture.md` and AHL-477 name
    /// — by its own name, so a future edit that reintroduces a second,
    /// independently-wrong copy here (which is exactly how this bug and the
    /// `eval.rs::value_cmp` one it was found beside both got in) fails a test
    /// in the file it touched, not only in `eval.rs`.
    fn mixed_class_corpus() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Integer(-1_000_000),
            Value::Integer(-1),
            Value::Integer(0),
            Value::Integer(1),
            Value::Real(1.0),
            Value::Integer(2),
            Value::Real(1.5),
            Value::Real(-2.5),
            Value::Integer(i64::MAX),
            Value::Real(1e300),
            text(""),
            text("Abc"),
            text("abc"),
            text("abd"),
            text("z"),
            blob(&[]),
            blob(&[0]),
            blob(&[1, 2]),
            blob(&[1, 2, 3]),
            blob(&[255]),
        ]
    }

    /// `compare_values` — `ORDER BY`'s and `GROUP BY`'s general sort-key
    /// comparator — is a genuine total order (reflexive, antisymmetric,
    /// transitive) over a value set that crosses every SQLite storage class,
    /// exhaustively rather than by sampling. This is the AHL-477 regression:
    /// the old comparator fell back to `Ordering::Equal` for a pair it had
    /// no rule for, which does not merely misplace that pair — `sort_by`
    /// requires a total order, so one broken pair can corrupt an entire
    /// sort. A future change that reintroduces any such fallback fails this
    /// test rather than surfacing as a flaky, data-dependent `ORDER BY` bug.
    #[test]
    fn compare_values_is_a_total_order_over_every_storage_class() {
        let values = mixed_class_corpus();

        for a in &values {
            assert_eq!(
                compare_values(a, a, Collation::Binary),
                Ordering::Equal,
                "{a:?} did not compare equal to itself"
            );
        }

        for a in &values {
            for b in &values {
                let forward = compare_values(a, b, Collation::Binary);
                let backward = compare_values(b, a, Collation::Binary);
                assert_eq!(
                    forward,
                    backward.reverse(),
                    "{a:?} vs {b:?}: {forward:?} does not reverse to {backward:?}"
                );
            }
        }

        for a in &values {
            for b in &values {
                if compare_values(a, b, Collation::Binary) == Ordering::Greater {
                    continue;
                }
                for c in &values {
                    if compare_values(b, c, Collation::Binary) == Ordering::Greater {
                        continue;
                    }
                    assert_ne!(
                        compare_values(a, c, Collation::Binary),
                        Ordering::Greater,
                        "{a:?} <= {b:?} <= {c:?}, but {a:?} > {c:?}"
                    );
                }
            }
        }
    }

    /// `compare_values` now defers entirely to `eval::mem_cmp` rather than
    /// keeping a second, independently maintained copy of the same rule —
    /// this pins the delegation itself, over every pair in the corpus, so a
    /// future edit cannot quietly fork the two implementations back apart
    /// without a test noticing.
    #[test]
    fn compare_values_agrees_with_mem_cmp_on_every_pair() {
        let values = mixed_class_corpus();
        for a in &values {
            for b in &values {
                assert_eq!(
                    compare_values(a, b, Collation::Binary),
                    eval::mem_cmp(a, b, Collation::Binary),
                    "compare_values and mem_cmp disagree on {a:?} vs {b:?}"
                );
            }
        }
    }

    /// `compare_group_keys` — the multi-column `GROUP BY` key comparator —
    /// shares the same class order per column, confirmed against sqlite3:
    /// `NULL` groups with `NULL`, `1` groups with `1.0` (same class,
    /// numeric), and a `TEXT`/`BLOB` pair that never collides still orders
    /// consistently rather than comparing "equal" by accident.
    #[test]
    fn compare_group_keys_shares_the_class_order_per_column() {
        let collations = [Collation::Binary];
        assert_eq!(
            compare_group_keys(&[Value::Null], &[Value::Null], &collations),
            Ordering::Equal
        );
        assert_eq!(
            compare_group_keys(&[Value::Integer(1)], &[Value::Real(1.0)], &collations),
            Ordering::Equal,
            "1 and 1.0 are the same GROUP BY key"
        );
        assert_eq!(
            compare_group_keys(&[Value::Integer(1)], &[text("a")], &collations),
            Ordering::Less,
            "an INTEGER key orders below a TEXT key rather than comparing equal"
        );
        assert_eq!(
            compare_group_keys(&[text("a")], &[blob(b"a")], &collations),
            Ordering::Less,
            "TEXT orders below BLOB even with identical bytes"
        );
    }
}
