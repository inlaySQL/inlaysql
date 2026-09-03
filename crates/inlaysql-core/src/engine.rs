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
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use crate::bm25_paged::PagedBm25Index;
use crate::btree::{BackupSummary, Durability};
use crate::catalog::{
    auto_index_name, auto_unique_index_name, Catalog, Index, IndexKind, Table, CATALOG_KEY,
};
use crate::cdc::{self, ChangeKind, Changes, CDC_FLOOR_KEY, CDC_RETENTION};
use crate::collation::Collation;
use crate::error::{Error, Result};
use crate::eval::{self, Computed, Env, SharedRng, SubqueryRunner};
use crate::exec::{
    collect_bounded, fnv1a, mix64, park, AggregateInput, Decode, DecodeFilter, ExecRow, Filter,
    HashJoin, HashJoinTable, IndexProbe, JoinInner, NestedLoopJoin, ProbeKind, RowBytes, RowStream,
};
use crate::fusion::{reciprocal_rank_fusion, sort_by_score_desc};
use crate::hnsw::VectorMetric;
use crate::hnsw_paged::PagedHnswIndex;
use crate::plan::{
    AggFunc, Aggregate, AlterAction, AlterTablePlan, AnalyzePlan, ConflictAction, ConflictUpdate,
    CreateTablePlan, DeletePlan, DropTablePlan, Expr, FrameBound, FrameUnit, FromItem, InsertPlan,
    InsertSource, JoinKind, OnConflict, Order, OrderKey, Plan, RecursivePlan, ReindexPlan,
    ScalarPlan, ScoreExpr, SelectItem, SelectPlan, SetOp, SetOperationPlan, SubqueryBody,
    UpdatePlan, WindowFn, WindowFunc,
};
use crate::planner::{self, JoinDecision, JoinPath, PlannerStats, STATS_META_KEY};
use crate::row::{
    decode_row, decode_row_masked, decode_row_ref_masked_into, decode_row_ref_wanted_into,
    decode_value_at, encode_typed_row, encode_typed_row_into, ColumnMask, RowBuf,
};
use crate::shared::SharedStorage;
use crate::sql::{self, TableRules};
use crate::statement::Statement;
use crate::traits::{
    Cancel, Clock, FullTextIndex, IndexFactory, Interrupt, Rng, RowId, RowScan, Scored,
    StatementClock, Storage, VectorIndex, VectorTuning,
};
use crate::value::{DataType, Value, ValueRef};

/// A borrowed projected-row consumer used by the internal push pipeline.
type RowSink<'a> = dyn FnMut(&[Value]) -> Result<()> + 'a;

/// The reusable buffers of [`Engine::run_borrowed_select`]. See
/// [`Engine::borrow_scratch`] for why they outlive the call.
#[derive(Default)]
struct BorrowScratch {
    /// Which decoded column each output cell comes from, from
    /// [`borrowed_projection`].
    projection: Vec<usize>,
    /// The row currently decoded, one cell per column of the driving table.
    cells: Vec<ValueRef<'static>>,
    /// The projected row handed to the callback.
    out: Vec<ValueRef<'static>>,
}

/// Metadata key holding the next row id to hand out.
const NEXT_ROW_ID_KEY: &str = "next_row_id";

/// Metadata key holding the number of committed mutations.
///
/// Every statement that changes a row bumps it, in the same storage commit as
/// the change. A persisted index carries the version it reflects, so the
/// engine can tell at a glance whether a saved index still describes the rows
/// on disk. See [`Engine::persist_indexes`].
const WRITE_VERSION_KEY: &str = "write_version";

/// Metadata key holding the catalog revision used to invalidate planner stats.
const SCHEMA_VERSION_KEY: &str = "schema_version";

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

/// Default ceiling for one retained prepared hash-join build.
///
/// The cache holds at most one build per engine, so this bounds its accounted
/// payload rather than multiplying per entry. Set
/// [`EngineOptions::hash_join_cache_bytes`] to zero to disable it.
const DEFAULT_HASH_JOIN_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Default ceiling for one statement's blocking working set.
///
/// Chosen to be far above any sane query and far below any machine this runs
/// on: a `GROUP BY` over a million ordinary rows costs tens of megabytes, so
/// nothing legitimate meets this, while a runaway sort over an unbounded scan
/// meets it long before the operating system starts choosing which process to
/// kill. It is per statement — see [`EngineOptions::query_memory_bytes`], which
/// is also how to change it or remove it.
const DEFAULT_QUERY_MEMORY_BYTES: usize = 512 * 1024 * 1024;

/// Multiplier applied to `LIMIT` when sizing each retriever's candidate list.
///
/// Fusion can only rank what the retrievers returned, so each leaf has to
/// over-fetch: a row that is 40th by vector similarity but 1st by BM25 should
/// still be able to win.
const CANDIDATE_OVERFETCH: usize = 4;

/// How many candidates a retrieval query asks each index for.
///
/// The one place the rule lives, because three callers have to agree on it: the
/// unfiltered fetch ([`Engine::retrieve_rows`]), the filtered one
/// ([`Engine::retrieve_filtered`]), and [`crate::explain`], which reports the
/// `ef` a vector search will run at and derives it from this number. An
/// `EXPLAIN` that computed the candidate budget its own way would eventually
/// report an operating point the executor does not use, which is the whole
/// class of bug that module exists to rule out.
///
/// The two arms differ, and did before this was factored out: a query with no
/// `LIMIT` is capped at [`DEFAULT_CANDIDATES`] outright, while a filtered one
/// takes that cap as the *rows it wants* and over-fetches on top of it, because
/// the filter is applied inside the walk and rejected rows do not consume the
/// budget.
pub(crate) fn candidate_limit(limit: Option<usize>, filtered: bool) -> usize {
    if filtered {
        limit
            .unwrap_or(DEFAULT_CANDIDATES)
            .saturating_mul(CANDIDATE_OVERFETCH)
            .max(1)
    } else {
        limit
            .map(|limit| limit.saturating_mul(CANDIDATE_OVERFETCH))
            .unwrap_or(DEFAULT_CANDIDATES)
            .max(1)
    }
}

/// How many rows a retrieval query can return: its `LIMIT`, or the same cap an
/// unbounded one is held to.
///
/// The floor a session's `ef_search` is checked against — see
/// [`check_ef_search`] — and therefore worth naming rather than repeating.
pub(crate) fn rows_wanted(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_CANDIDATES)
}

/// Refuse a session `ef_search` narrower than the answer the query asked for.
///
/// `ef` is how many candidates the graph walk may hold at once, so a walk with
/// `ef < wanted` cannot come back holding `wanted` rows: it would return a
/// short answer while more rows existed, without saying so. Widening it to fit
/// is the other silent option and is worse — the search would run at a number
/// the caller did not choose while `@@inlaysql_hnsw_ef_search` and `EXPLAIN`
/// reported the one they did. So the query is refused and names the smallest
/// value that works, which is pgvector's rule for `hnsw.ef_search` too.
///
/// **The floor is the query's row budget, not the candidate count.** The engine
/// asks each retriever for [`CANDIDATE_OVERFETCH`] times as many candidates as
/// the query can return, so that a fused ranking has more than the bare
/// minimum to work with; an `ef` below *that* is merely a narrower beam, which
/// is exactly what a caller asking for less latency is asking for, and
/// refusing it would put the cheap half of the recall/latency trade out of
/// reach for no correctness gain. A short candidate list is what the index's
/// contract already allows ("up to `k`").
///
/// One consequence worth knowing at the very bottom of the range: at `ef`
/// close to `wanted`, a walk that spends its beam on tombstoned rows can still
/// come back with fewer than `wanted`. That is what a beam barely wide enough
/// means, and it is why the shipped tuning holds `ef >= 2k`.
///
/// Only reachable when a session has set `ef_search` at all: a query that asked
/// for nothing gets [`crate::hnsw::HnswParams::ef_for`], whose own `max(k)`
/// clause puts it above this floor by construction.
pub(crate) fn check_ef_search(ef: usize, wanted: usize) -> Result<()> {
    if ef >= wanted {
        return Ok(());
    }
    Err(Error::Unsupported(alloc::format!(
        "ef_search = {ef} is narrower than the {wanted} rows this query asks for, so the \
         search could not return them all: a candidate list is the beam the graph walk \
         holds at once, and one narrower than the answer cannot hold it. The row budget is \
         the query's LIMIT, or {DEFAULT_CANDIDATES} when it has none. Raise ef_search to \
         at least {wanted}, lower the LIMIT, or clear it to restore the index's own tuning \
         (`SET inlaysql_hnsw_ef_search = 0` on the MySQL server)"
    )))
}

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

/// What a forced index build covered.
///
/// Private on purpose: the scope a caller can ask for is a table name or
/// nothing, and this is how the engine spells the two extra shapes the
/// `REINDEX` statement can resolve to — a list of tables, and a single index.
enum Reindex {
    /// Every retrieval index this handle holds.
    Everything,
    /// One table's, by lowercased table name.
    Table(String),
    /// Several tables', by lowercased table name.
    Tables(Vec<String>),
    /// Exactly one index, by its backend key.
    ///
    /// A B-tree index resolves to a key no backend lives under, so `REINDEX
    /// <btree index>` covers nothing and reports nothing. That is the right
    /// answer rather than a gap: a B-tree index's entries *are* durable rows,
    /// written in the same commit as the rows they describe, so there is no
    /// state for a rebuild to correct (see `Engine::load_saved_indexes`).
    Index((String, Vec<String>)),
}

impl Reindex {
    /// Whether the backend living under `key` is in scope.
    fn covers(&self, key: &(String, Vec<String>)) -> bool {
        match self {
            Reindex::Everything => true,
            Reindex::Table(table) => &key.0 == table,
            Reindex::Tables(tables) => tables.contains(&key.0),
            Reindex::Index(wanted) => wanted == key,
        }
    }
}

/// What [`Engine::reindex`] built.
///
/// Empty means nothing was pending and nothing ran — which is the honest
/// answer for a database whose indexes already describe every committed row,
/// and the answer a caller should expect from a second `REINDEX` in a row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reindexed {
    /// The retrieval indexes that were committed, by catalog name.
    ///
    /// Exactly the indexes whose table was holding writes that had not reached
    /// them yet. A backend does not report whether its own commit found
    /// anything, so the claim stops there: "its table had pending writes and
    /// this brought it up to date", never "this many rows were re-indexed".
    pub indexes: Vec<String>,
}

impl Reindexed {
    /// Whether the build did nothing because nothing was pending.
    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
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
    /// Keep full-text indexes in the database instead of in memory.
    ///
    /// The in-memory [`crate::bm25::Bm25Index`] holds the term dictionary,
    /// every postings list and a per-document term list in RAM — measured at
    /// ~1,800 bytes per document once the dictionary saturates, so ten million
    /// documents is ~17 GiB *per connection*
    /// (`crates/inlaysql/tests/index_memory_cost.rs`).
    /// [`crate::bm25_paged::PagedBm25Index`] puts the postings in the database
    /// file and a bounded cache in memory instead, writing through the
    /// engine's own transaction so the index commits with the rows it
    /// describes.
    ///
    /// **The trade is writes.** An inverted index update touches one chunk per
    /// distinct term of the document — around a hundred for a 120-token chunk
    /// of English — and each is a page the commit record has to carry. The
    /// ordinary path absorbs that, because index commits are deferred to the
    /// first read that needs them and that read is normally outside any
    /// transaction, where the backend may commit in batches; a read *inside*
    /// an open transaction after many documents may be refused for size. The
    /// file also grows far faster than it does with the in-memory backend
    /// unless [`EngineOptions::page_reuse`] is on.
    ///
    /// The scores are identical either way, bit for bit, and that is asserted
    /// rather than argued
    /// (`crates/inlaysql-core/tests/bm25_paged_agreement.rs`, and through the
    /// whole SQL path in `crates/inlaysql/tests/paged_full_text.rs`).
    ///
    /// Off by default: the in-memory index is faster, and this trade has to be
    /// explicit.
    pub paged_text_indexes: bool,
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
    /// Maximum resident bytes for one hash-join build retained across
    /// executions on the same committed snapshot.
    ///
    /// A prepared full-scan equi-join otherwise rescans and decodes its inner
    /// table every time it runs, even when no row has changed. The retained
    /// build is immutable and is reused only while `write_version` agrees;
    /// schema changes clear it, and reads after writes in an open transaction
    /// bypass it. At most one build is held, and a build larger than this
    /// ceiling runs normally without being retained. Set to `0` to disable.
    pub hash_join_cache_bytes: usize,
    /// Most bytes one statement may hold in a blocking operator's input.
    ///
    /// `ORDER BY`, `GROUP BY`, `DISTINCT` and window functions cannot emit
    /// their first row before they have seen their last input row, so each
    /// holds its whole input at once — see [`crate::exec`]'s module docs for
    /// why that is inherent rather than a gap. Without a ceiling the only thing
    /// that ends such a query is the operating system's out-of-memory killer,
    /// and that does not end the query, it ends the process. On an embedded
    /// handle that is one application; on the MySQL-wire server it is every
    /// other connection as well. This is the number that turns "the process
    /// died" into [`crate::Error::Memory`] on the one statement responsible,
    /// with nothing written and the handle still usable.
    ///
    /// It is a **per-statement** ceiling, not a per-process one: `n`
    /// concurrent connections can each be holding this much. Size it against
    /// the machine divided by the connections it serves.
    ///
    /// It bounds the collected input, which is the dominant term, and not what
    /// the sort, fold or projection then allocate on top of it. Set to `0` to
    /// remove the ceiling entirely, which is what every caller had before this
    /// option existed.
    ///
    /// Not a spill-to-disk threshold. A query past this is refused, not slowed:
    /// a refused statement is recoverable and a dead process is not.
    pub query_memory_bytes: usize,
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
    /// How strong a barrier an ordinary commit waits on before returning.
    ///
    /// `F_FULLFSYNC`/`fsync` (macOS/Linux, [`Durability::Full`], the
    /// default) is measured at 97.1% of a single-writer commit's wall-clock
    /// time on this project's reference host, and its cost is flat with
    /// respect to bytes queued — see `PERF.md`'s Phase 0 section.
    /// [`Durability::Normal`] trades a documented, bounded amount of loss
    /// for most of that: measured 32x single-writer throughput on the same
    /// host (`PERF.md`).
    ///
    /// # Read this before setting anything other than `Durability::Full`
    ///
    /// **This changes what "committed" can mean on power loss, not just
    /// speed.** [`Durability::Full`] never loses a committed write, ever.
    /// [`Durability::Normal`] survives a process crash or an OS crash with
    /// zero loss, but a **power failure** can lose commits still sitting in
    /// the drive's own volatile write cache, bounded to commits since the
    /// last checkpoint or WAL-region wrap. It can never corrupt or invent
    /// state — recovery always lands on a real past commit — but it can
    /// hand back one older than the caller last saw acknowledged. See
    /// [`Durability`]'s doc comment and `docs/recovery.md`'s "Durability
    /// levels" section for the exact bound, the per-platform syscall
    /// mapping, and why `Device::sync` (checkpoints, the state block) is
    /// never weakened by this option regardless of the level chosen here.
    ///
    /// **This is effectively per-file, not freely mixable per-handle** — the
    /// same cross-process/in-process distinction [`EngineOptions::page_reuse`]
    /// already has to draw, for a related reason: `inlaysql::FileDevice`'s
    /// commit barrier is shared by every handle this process has open on a
    /// given `(dev, ino)`, not held per handle. Two handles on the same file
    /// requesting different levels do not each get their own barrier — the
    /// device arbitrates with **strongest wins, for as long as any handle
    /// sharing it stays open**: once one handle has required `Full`
    /// (including simply defaulting to it), that file stays at `Full` until
    /// every handle on it closes, even if another handle asked for `Normal`.
    /// This is the safe default read of "different levels on one file": a
    /// caller who forgets to opt a handle into `Normal` gets the guarantee
    /// it already expected instead of silently inheriting a weaker one
    /// somebody else chose. See `CowBTree::set_durability`'s doc comment for
    /// the full argument.
    pub durability: Durability,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            implicit_indexes: false,
            paged_vector_indexes: false,
            paged_text_indexes: false,
            page_cache_bytes: crate::btree::DEFAULT_PAGE_CACHE_BYTES,
            hash_join_cache_bytes: DEFAULT_HASH_JOIN_CACHE_BYTES,
            query_memory_bytes: DEFAULT_QUERY_MEMORY_BYTES,
            page_reuse: false,
            durability: Durability::Full,
        }
    }
}

/// Identity and immutable payload of the one retained hash-join build.
struct CachedHashJoin {
    write_version: u64,
    table_name: String,
    mask: ColumnMask,
    inner_key: usize,
    width: usize,
    /// Part of the identity, not a detail: the bucket layout is built from a
    /// collation-folded hash, so a `NOCASE` build answers a `BINARY` probe with
    /// the wrong bucket and would silently drop pairs.
    collation: Collation,
    table: Rc<HashJoinTable>,
}

impl CachedHashJoin {
    fn matches(
        &self,
        write_version: u64,
        table_name: &str,
        mask: &ColumnMask,
        inner_key: usize,
        width: usize,
        collation: Collation,
    ) -> bool {
        self.write_version == write_version
            && self.table_name == table_name
            && self.mask == *mask
            && self.inner_key == inner_key
            && self.width == width
            && self.collation == collation
    }
}

/// One join access path chosen by the executor and `EXPLAIN` together.
pub(crate) enum JoinStrategy {
    /// Build the inner table's hash table and probe it with `outer_key`.
    ///
    /// `collation` is what the `ON`'s `=` resolved: it decides both the bucket
    /// hash and the candidate comparison, and a build made under one collation
    /// cannot answer a probe under another.
    Hash {
        outer_key: usize,
        inner_key: usize,
        collation: Collation,
    },
    /// Probe the inner primary key or scalar B-tree index per outer row.
    Probe {
        key: usize,
        ty: DataType,
        collation: Collation,
        kind: ProbeKind,
    },
    /// Materialise the inner table and replay it for each outer row.
    Materialise,
}

/// A join strategy, plus the optional cost comparison that selected it.
pub(crate) struct JoinChoice {
    /// The path the executor should use.
    pub strategy: JoinStrategy,
    /// Present only when fresh, complete statistics selected between existing
    /// paths; `None` means the legacy shape rule made the choice.
    pub cost: Option<JoinDecision>,
}

/// One entry in [`Engine::transaction_log`]: enough to run a write statement
/// a second time and reach the exact state it reached the first time.
///
/// `now` is the point of this: [`Engine::run_refreshed`] samples the clock
/// once per statement so every `'now'` inside it agrees, and a replay has to
/// reproduce that same reading rather than sample a new one — otherwise a
/// `ROLLBACK TO SAVEPOINT` after an `INSERT ... VALUES (now())` would change
/// the very row it is supposed to be reconstructing unchanged.
#[derive(Clone)]
struct LoggedStatement {
    statement: Statement,
    params: Vec<Value>,
    now: i64,
}

/// One open `SAVEPOINT`, recording where in [`Engine::transaction_log`] it
/// was established.
struct SavepointFrame {
    name: String,
    log_position: usize,
}

/// The database engine.
///
/// It owns the catalog and the live indexes, and drives storage through the
/// [`Storage`] trait. Swap the constructor arguments and the same engine runs
/// against real files or against a simulated environment.
pub struct Engine {
    storage: SharedStorage,
    factory: Box<dyn IndexFactory>,
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
    /// The injected clock, plus the statement in flight's reading of it, so
    /// that every `'now'` in one statement sees one instant — as SQLite's
    /// `sqlite3StmtCurrentTime` does. The reading is *deferred*: a statement
    /// that contains no time function never takes one. Shared with every
    /// [`Env`] built for the statement, which is why it is an [`Rc`] rather
    /// than owned outright — an environment outlives the borrow of the engine
    /// that made it.
    statement_clock: Rc<StatementClock>,
    /// Where "stop this statement" is noticed.
    ///
    /// Empty unless a host installs a signal ([`Engine::set_cancel`]), and a
    /// null branch per few thousand rows when it does not — the core cannot
    /// time a statement out or hear a `KILL` on its own, so this is the seam
    /// that lets whoever can say so. Armed once per statement, beside
    /// `statement_clock`, so a deadline covers exactly one statement.
    interrupt: Interrupt,
    /// Where the candidate-list size a session chose for its vector searches is
    /// read from.
    ///
    /// Empty unless a host installs one ([`Engine::set_vector_tuning`]), and
    /// then every vector search asks it once — see [`VectorTuning`] for why it
    /// is a handle rather than a number copied in here. `None` from it, and no
    /// handle at all, both mean the same thing and both take exactly the path
    /// every query took before this existed.
    vector_tuning: Option<Box<dyn VectorTuning>>,
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
    /// The last full-scan hash build, retained across prepared executions.
    ///
    /// One entry is deliberate: it gives the common repeated-statement case
    /// reuse without turning every distinct join an application has ever run
    /// into resident memory. The entry carries the committed row version and
    /// the exact physical build shape; [`Engine::hash_join_table`] owns the
    /// validity and budget checks.
    hash_join_cache: RefCell<Option<CachedHashJoin>>,
    /// The buffers [`Engine::run_borrowed_select`] re-lends to every row it
    /// pushes into a borrowing consumer.
    ///
    /// They live on the handle rather than on the call because a **point
    /// read** is one row and one query: buffers scoped to the call would
    /// allocate three vectors per lookup, which is most of what
    /// [`Engine::run_query_each_ref`] exists to remove. Held here, a handle
    /// pays for them once in its life and every lookup after the first
    /// allocates nothing at all.
    ///
    /// Empty whenever they are parked — see [`park`] — so the `'static` is
    /// honest: nothing borrowed from a page is ever stored across a row, let
    /// alone across a statement.
    borrow_scratch: RefCell<BorrowScratch>,
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
    /// The tables, lowercased, whose retrieval backends are holding writes
    /// that have not been committed into them yet.
    ///
    /// Set by writes, cleared by [`Engine::build_indexes`]. A **set** rather
    /// than the single flag this used to be, and the reason is `REINDEX t`: a
    /// build narrowed to one table has to leave the other tables pending, and
    /// with one flag the only two things it could do were leave every table
    /// pending — so the *next* `REINDEX t` would claim to have rebuilt an
    /// index that had nothing to do — or clear it and tell the next read that
    /// a table nobody built was current, which is the silent-empty-index
    /// failure. Per table, both answers are exact.
    ///
    /// It costs a `BTreeSet` probe per indexed row where it used to cost a
    /// store. That used to sit next to a `Vec<Index>` clone
    /// [`Engine::index_row_retrieval`] did per row, which was orders of
    /// magnitude more; that clone is now [`RowIndexes`], taken once per
    /// statement, and this probe is what is left of the per-row cost.
    dirty_tables: BTreeSet<String>,
    next_row_id: RowId,
    /// The row id the last `INSERT` that auto-assigned one handed out. See
    /// [`Engine::last_insert_row_id`].
    last_insert_row_id: Option<RowId>,
    /// Number of committed row mutations. Stamped onto persisted indexes.
    write_version: u64,
    /// Number of committed catalog changes. DDL does not change
    /// `write_version`, so planner stats need this separate currency.
    schema_version: u64,
    /// Optional cardinality snapshot used by the staged cost planner.
    ///
    /// It is derived from committed rows and is discarded whenever the data
    /// or catalog moves. A stale or incomplete snapshot therefore cannot
    /// affect a plan; the rule-based chooser remains the fallback.
    planner_stats: PlannerStats,
    /// The `write_version` the persisted indexes were saved at.
    persisted_version: u64,
    /// The `write_version` the *live* retrieval indexes are known to describe
    /// in full.
    ///
    /// Normally exactly `write_version`: this handle indexes every row it
    /// writes before it commits it. It falls behind in two places, and both
    /// are why it exists as a separate number rather than being read off
    /// `write_version`:
    ///
    /// * Another handle committed. The gap names exactly the change-log
    ///   versions this handle has not applied, which is what
    ///   [`Engine::catch_up_indexes`](Self::catch_up_indexes) replays instead
    ///   of re-reading every row of every table.
    /// * This handle's own commit was *rebased* onto a concurrent one (see
    ///   [`Engine::commit_storage`](Self::commit_storage)). The winner's rows
    ///   are now committed underneath this handle's indexes without
    ///   [`Storage::refresh`] ever reporting a move — this handle already
    ///   holds the rebased root. Leaving this behind at the pre-commit value
    ///   is what makes the next statement notice; without it the missing rows
    ///   would stay missing from this handle's indexes until some unrelated
    ///   commit happened to force a rebuild.
    indexed_version: u64,
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
    /// Whether the open transaction was started by the first `SAVEPOINT`
    /// rather than an explicit `BEGIN` — decides whether releasing the
    /// outermost savepoint ends the transaction (SQLite's rule, confirmed
    /// against a real sqlite3 binary) or merely drops the marker.
    transaction_is_implicit: bool,
    /// Every write statement run since the transaction began, in order —
    /// [`Engine::rollback_to_savepoint`]'s replay log. Cleared whenever the
    /// transaction ends, by any path.
    transaction_log: Vec<LoggedStatement>,
    /// Open savepoints, innermost (most recently established) last.
    /// `ROLLBACK TO` truncates back to, and keeps, the named frame;
    /// `RELEASE` drops it and every frame above it — both confirmed against
    /// a real sqlite3 binary, including the case of two open savepoints
    /// sharing a name.
    savepoints: Vec<SavepointFrame>,
    /// Set only inside [`Engine::rollback_to_savepoint`]'s replay loop: stops
    /// a replayed statement from re-sampling the clock (it must reproduce the
    /// exact reading its first run captured, not a new one) or being logged
    /// a second time.
    replaying: bool,
    /// How this engine was opened. `implicit_indexes` is the pre-`CREATE INDEX`
    /// behaviour, kept available for the demo and for databases that want
    /// automatic indexing; `paged_vector_indexes` decides whether a vector
    /// index lives in the database or in memory.
    options: EngineOptions,
}

/// The index declarations one statement maintains, resolved once before its
/// first row instead of again for every row.
///
/// [`Catalog::indexes_for`] filters the whole index map and allocates a
/// `Vec<&Index>` on each call, and the retrieval half then deep-cloned every
/// `Index` that `Vec` held — both of them per row, for a set that cannot
/// change while a statement runs. Every write statement already depends on
/// exactly that: it takes its own [`Table`] clone before the first row and
/// never re-reads it, because no DDL can interleave with the row loop.
///
/// Owned rather than borrowed, and that is the whole reason the clone existed
/// in the first place: every consumer needs `&mut self` immediately
/// afterwards — `self.storage.put_index_entry`, a retrieval backend's
/// `insert` — so a `Vec<&Index>` borrowed out of `self.catalog` cannot
/// survive the call it exists to feed. The clone is still here; it is now one
/// per statement rather than one per row.
struct RowIndexes {
    /// The B-tree indexes, in the catalog's name order — which is the order
    /// [`btree_entry_keys`] emits entries in, and the order the DST sweep's
    /// "one entry per row per index" count walks.
    btree: Vec<Index>,
    /// The retrieval indexes — `FullText` and `Vector` — in the same order.
    retrieval: Vec<Index>,
}

impl RowIndexes {
    /// Split one table's declared indexes into the two halves the write path
    /// maintains separately. Exhaustive over [`IndexKind`] on purpose: a kind
    /// added later has to be placed in one half or the other here, rather
    /// than silently falling out of both and leaving an index nothing writes.
    fn resolve(catalog: &Catalog, table: &str) -> Self {
        let mut btree = Vec::new();
        let mut retrieval = Vec::new();
        for index in catalog.indexes_for(table) {
            match index.kind {
                IndexKind::BTree => btree.push(index.clone()),
                IndexKind::FullText | IndexKind::Vector => retrieval.push(index.clone()),
            }
        }
        Self { btree, retrieval }
    }
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
        // Wrapped in the temporary-table router before anything else touches
        // it, so every backend this engine can be opened over — the on-disk
        // tree, the in-memory simulation backend, `redb` — gets `CREATE
        // TEMPORARY TABLE` the same way, for free. One handle from here on:
        // an index that keeps itself in the database takes a clone of this,
        // so its writes join the engine's transaction rather than opening
        // one of their own.
        let storage =
            SharedStorage::new(Box::new(crate::temp_storage::TempTableRouter::new(storage)));
        let catalog = match storage.get_meta(CATALOG_KEY)? {
            Some(bytes) => Catalog::decode(&bytes)?,
            None => Catalog::new(),
        };
        let next_row_id = read_counter(&storage, NEXT_ROW_ID_KEY, "next row id")?.unwrap_or(1);
        let write_version =
            read_counter(&storage, WRITE_VERSION_KEY, "write version")?.unwrap_or_default();
        let cdc_floor = read_counter(&storage, CDC_FLOOR_KEY, "change floor")?.unwrap_or_default();
        let schema_version =
            read_counter(&storage, SCHEMA_VERSION_KEY, "schema version")?.unwrap_or_default();
        let planner_stats = load_planner_stats(&storage, write_version, schema_version, &catalog)?;

        // Seeded from the clock, which is itself injected: in the simulation
        // that is a logical counter, so the stream is reproducible.
        let clock: Rc<dyn Clock> = Rc::from(clock);
        let seed = clock.now_micros() as u64;
        let mut engine = Engine {
            storage,
            factory,
            rng: Rc::new(RefCell::new(
                Box::new(crate::mem::SeededRng::new(seed)) as Box<dyn Rng>
            )),
            statement_clock: Rc::new(StatementClock::new(clock)),
            interrupt: Interrupt::none(),
            vector_tuning: None,
            catalog,
            rules: BTreeMap::new(),
            hash_join_cache: RefCell::new(None),
            borrow_scratch: RefCell::new(BorrowScratch::default()),
            text_indexes: BTreeMap::new(),
            vector_indexes: BTreeMap::new(),
            dirty_tables: BTreeSet::new(),
            next_row_id,
            last_insert_row_id: None,
            write_version,
            schema_version,
            planner_stats,
            persisted_version: write_version,
            indexed_version: write_version,
            pending_changes: Vec::new(),
            cdc_floor,
            parses: Cell::new(0),
            in_transaction: false,
            transaction_is_implicit: false,
            transaction_log: Vec::new(),
            savepoints: Vec::new(),
            replaying: false,
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
        self.statement_clock.clock()
    }

    /// Replace the generator `random()` draws from.
    ///
    /// The default is seeded from the clock at open, which is reproducible
    /// under a logical clock and varies under a real one. A simulation that
    /// wants a specific stream sets its own.
    pub fn set_rng(&mut self, rng: Box<dyn Rng>) {
        self.rng = Rc::new(RefCell::new(rng));
    }

    /// Install the signal every long loop in this engine asks before carrying
    /// on. See [`Cancel`].
    ///
    /// Until this is called there is no statement timeout and no way to stop a
    /// running statement, because there is nothing in a `no_std` core that
    /// could provide either. A host that installs one gets both, and one that
    /// does not pays a null branch per few thousand rows.
    pub fn set_cancel(&mut self, cancel: Box<dyn Cancel>) {
        self.interrupt = Interrupt::with(cancel);
    }

    /// Install the handle every vector search asks for its candidate-list size.
    /// See [`VectorTuning`].
    ///
    /// Until this is called — and whenever the installed handle answers `None`
    /// — every vector search uses the `ef` its own index would have chosen,
    /// which is the behaviour and the recall of every build before this one.
    pub fn set_vector_tuning(&mut self, tuning: Box<dyn VectorTuning>) {
        self.vector_tuning = Some(tuning);
    }

    /// The candidate-list size imposed on this engine's vector searches, or
    /// `None` when each index's own `ef_search` is in force.
    ///
    /// Read through the installed handle rather than out of a field, so a host
    /// that reports this number to a client is reporting the same load the
    /// search itself makes. See [`VectorTuning`].
    pub fn vector_ef_search(&self) -> Option<usize> {
        self.vector_tuning
            .as_ref()
            .and_then(|tuning| tuning.ef_search())
    }

    /// A cancellable sequential scan of `table`. Every scan the engine starts
    /// goes through here rather than through [`RowScan::new`], which is what
    /// makes "stop this statement" reach a table scan at all.
    fn scan(&self, table: &str) -> RowScan<'_> {
        RowScan::watched(&self.storage, table, &self.interrupt)
    }

    /// [`Engine::scan`], materialised — the write paths' view of a table, which
    /// has to be read before it is changed (see [`crate::traits::scan_all`]).
    fn scan_all(&self, table: &str) -> Result<Vec<(RowId, RowBuf)>> {
        self.scan(table).collect()
    }

    /// The expression environment for the statement in flight.
    ///
    /// Reads the clock at most once per statement, never once per row: `'now'`
    /// must not move underneath a query, and a logical clock that ticks on
    /// every read would make it. "At most" because the reading is deferred to
    /// the first time function that asks — see [`StatementClock`].
    fn env<'a>(&self, params: &'a [Value]) -> Env<'a> {
        Env::with_statement_clock(
            params,
            Rc::clone(&self.statement_clock),
            Rc::clone(&self.rng),
        )
    }

    /// The same environment, able to evaluate subqueries.
    ///
    /// It borrows the engine for as long as it lives, which is why it is the
    /// read path's environment alone: [`Engine::insert`], [`Engine::update`]
    /// and [`Engine::delete`] build theirs and then take `&mut self` to write,
    /// so they cannot hold one. A subquery in any of those statements is
    /// refused in the planner (`sql::reject_write_subqueries`) rather than
    /// reaching an environment that could not run it.
    pub(crate) fn read_env<'a>(&'a self, params: &'a [Value]) -> Env<'a> {
        Env::with_statement_clock(
            params,
            Rc::clone(&self.statement_clock),
            Rc::clone(&self.rng),
        )
        .with_subqueries(self)
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

    /// [`Engine::prepare`] against the committed state as it is *now*, which is
    /// what [`Engine::execute`] does before it parses and [`Engine::prepare`]
    /// deliberately does not.
    ///
    /// The refresh has to happen *before* the parse or planning reads a catalog
    /// another handle has already moved on from: a table a second connection
    /// created a moment ago is "no such table" here, permanently, rather than a
    /// plan that merely needs re-validating. That is the failure a one-shot
    /// caller hits — a statement it will plan once, run once and throw away —
    /// and it is the reason this exists beside `prepare`: a caller that wants
    /// the plan *and* the pre-parse refresh would otherwise have to reach for
    /// `execute`, which also runs the statement and hands back a materialised
    /// [`ResultSet`].
    pub fn prepare_fresh(&mut self, sql: &str) -> Result<Statement> {
        self.refresh_snapshot()?;
        self.prepare(sql)
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
        // At most one clock reading per statement, and it is taken by the
        // first `'now'` that asks rather than here, so every `'now'` inside
        // the statement agrees and a statement without one — nearly all of
        // them — never touches the clock at all. A replayed statement (see
        // `rollback_to_savepoint`) arrives with its instant already pinned by
        // `replay_transaction_up_to` and must keep it: sampling a fresh one
        // here would let a `ROLLBACK TO SAVEPOINT` change a row that used
        // `'now'` or similar, which is exactly the kind of divergence
        // "replay" is supposed to rule out.
        if !self.replaying {
            self.statement_clock.begin_statement();
        }
        // And one arming of the cancellation signal, in the same place and for
        // the same reason: a deadline has to cover exactly one statement, and a
        // `KILL QUERY` that landed between two of them must not fall on the
        // next one.
        self.interrupt.begin_statement();
        // A write inside an open transaction has to fit what the storage
        // backend can hold in one commit. Refuse it *before* running it, so a
        // too-large transaction is reported without a half-written statement:
        // the caller commits what it has, starts a new transaction and retries.
        if self.in_transaction && !statement.plan().is_read_only() {
            self.ensure_transaction_fits()?;
        }
        let outcome = match statement.plan() {
            Plan::CreateTable(create) => self.create_table(create, params),
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
            // Deliberately does not run `inner`, and deliberately reads no
            // rows: every decision `EXPLAIN` reports is made from the plan,
            // the catalog and the bound parameters. See [`crate::explain`].
            Plan::Explain(inner) => {
                Ok(Outcome::Rows(crate::explain::explain(self, inner, params)?))
            }
            Plan::Reindex(reindex) => self.run_reindex(reindex),
            Plan::Analyze(analyze) => self.run_analyze(analyze),
            Plan::Begin => self.begin().map(|()| Outcome::Ddl),
            Plan::Commit => self.commit().map(|()| Outcome::Ddl),
            Plan::Rollback => self.rollback().map(|()| Outcome::Ddl),
            Plan::Savepoint(name) => self.savepoint(name).map(|()| Outcome::Ddl),
            Plan::ReleaseSavepoint(name) => self.release_savepoint(name).map(|()| Outcome::Ddl),
            Plan::RollbackToSavepoint(name) => {
                self.rollback_to_savepoint(name).map(|()| Outcome::Ddl)
            }
        };
        // Logged so `rollback_to_savepoint` can replay a prefix of the
        // transaction later. Only a *successful* write counts: a failed
        // statement inside a transaction leaves nothing new buffered (see
        // `discard_failed_statement`'s doc), so there is nothing here for a
        // later savepoint rollback to reconstruct. Never logged while
        // replaying, or a rollback would grow the very log it is reading.
        if self.in_transaction
            && !self.replaying
            && !statement.plan().is_read_only()
            && outcome.is_ok()
        {
            self.transaction_log.push(LoggedStatement {
                statement: statement.clone(),
                params: params.to_vec(),
                // Forces the reading if the statement never took one: the
                // log entry has to name an instant for the replay to pin,
                // and a statement that ignored the time will ignore it again.
                now: self.statement_clock.now_micros(),
            });
        }
        if self.must_discard(statement, &outcome) {
            self.discard_failed_statement();
        }
        outcome
    }

    /// Whether a statement that has just finished left buffered writes that
    /// have to be thrown away.
    ///
    /// `is_read_only` is the proxy for it everywhere except one place. A
    /// **cancelled `REINDEX`** must not be discarded, and the reason is not
    /// tidiness: [`Engine::discard_failed_statement`] reloads the handle, a
    /// reload rebuilds every index it cannot restore from a saved blob, and
    /// that rebuild is [`Engine::restore_indexes`] — which is deliberately not
    /// interruptible. Discarding here would make a `KILL` on a four-minute
    /// index build cost the *whole* build with no way to stop it, which is the
    /// opposite of what the client asked for.
    ///
    /// It is sound because of where the cancellation lands:
    /// [`Engine::build_indexes`] only ever stops between backends, so there is
    /// no half-applied write to undo, and the tables it did not reach are
    /// still marked dirty — the state a build that was never asked for leaves.
    /// A `REINDEX` that failed for any *other* reason takes the ordinary path.
    fn must_discard(&self, statement: &Statement, outcome: &Result<Outcome>) -> bool {
        match outcome {
            Ok(_) => false,
            Err(Error::Cancelled(_)) if matches!(statement.plan(), Plan::Reindex(_)) => false,
            Err(_) => !statement.plan().is_read_only(),
        }
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

    /// Run a prepared query and visit each final row without retaining a
    /// [`ResultSet`]. Returns the number of rows delivered.
    ///
    /// The callback's slice is valid only for that call. Non-blocking queries
    /// reuse its backing allocation for the next row, which is the point of
    /// this API: a caller that serialises or counts rows should not pay for a
    /// `Vec<Vec<Value>>` containing the whole answer. Sorting, aggregation,
    /// windows and `DISTINCT` still materialise internally because their SQL
    /// semantics require seeing the complete input before emitting a row.
    pub fn run_query_each(
        &mut self,
        statement: &Statement,
        params: &[Value],
        mut each: impl FnMut(&[Value]) -> Result<()>,
    ) -> Result<usize> {
        self.begin_row_callback(statement, params)?;
        self.each_owned_row(statement, params, &mut each)
    }

    /// Run a prepared query and visit each final row as **borrowed** cells.
    /// Returns the number of rows delivered.
    ///
    /// [`Engine::run_query_each`] already reuses the projected row's `Vec`, but
    /// the cells inside it are owned [`Value`]s: a `TEXT` column is a `String`
    /// allocated out of the page's bytes and freed again before the next row.
    /// This hands the callback [`ValueRef`]s instead, which for
    /// `NULL`/`INTEGER`/`REAL` are the value itself and for `TEXT`/`BLOB` are a
    /// slice of the page the row was decoded from. A consumer that reads a row
    /// — sums a column, writes it to a socket, copies one field — allocates
    /// nothing per row at all. That is what `PERF.md` measured as the last
    /// remaining cost of a point read after AHL-527: `drop_in_place<ResultSet>`
    /// at 9% and `ValueRef::to_owned_value` at 2% were the answer being
    /// materialised, not the statement doing its work.
    ///
    /// # Which shapes actually borrow, and which fall back
    ///
    /// The borrowing pipeline reads one stored table and projects bare
    /// columns. `WHERE` (evaluated on the borrowed cells, exactly as
    /// [`crate::exec::DecodeFilter`] does), `LIMIT` and `OFFSET` are all part of
    /// it. Everything else — `ORDER BY`, `GROUP BY`/aggregates, window
    /// functions, `DISTINCT`, joins, derived tables, scored retrieval,
    /// `WITHOUT ROWID` tables, and any projection holding an expression —
    /// **falls back to the owned path** and borrows the callback's cells out of
    /// the owned row it built. The answer is identical either way; only the
    /// allocations differ. The blocking operators cannot do otherwise: none of
    /// them can emit a first row before it has seen the last input row, so the
    /// rows have to exist somewhere while they are sorted or folded.
    ///
    /// Same refusal as [`Engine::run_query_each`]: read-only statements only,
    /// because a callback may fail after rows have been delivered and that
    /// consumer error should not look like a failed statement after a mutation
    /// has already committed.
    ///
    /// The slice, and every cell in it, is valid only for that one call — the
    /// page it borrows from is released as the scan moves on. Copy what you
    /// need to keep with [`ValueRef::to_owned_value`].
    pub fn run_query_each_ref(
        &mut self,
        statement: &Statement,
        params: &[Value],
        mut each: impl FnMut(&[ValueRef<'_>]) -> Result<()>,
    ) -> Result<usize> {
        self.begin_row_callback(statement, params)?;

        if let Plan::Select(select) = statement.plan() {
            // Taken out for the length of the statement and put back
            // afterwards. A re-entrant call — a correlated subquery inside the
            // `WHERE` — finds empty buffers and allocates its own, which costs
            // it the reuse and nothing else.
            let mut scratch = self.borrow_scratch.take();
            if borrowed_projection(select, &mut scratch.projection) {
                self.refresh_indexes()?;
                let env = self.read_env(params);
                let delivered = self.run_borrowed_select(select, &env, &mut scratch, &mut each);
                self.borrow_scratch.replace(scratch);
                return delivered;
            }
            self.borrow_scratch.replace(scratch);
        }

        // The fallback. The rows are built owned by the ordinary pipeline and
        // borrowed for the callback, so this consumer sees one API whatever
        // the query turned out to be — an `ORDER BY` costs what it always did
        // rather than being refused.
        let mut parked: Vec<ValueRef<'static>> = Vec::new();
        let mut borrowing = |row: &[Value]| -> Result<()> {
            let mut cells: Vec<ValueRef<'_>> = core::mem::take(&mut parked);
            cells.extend(row.iter().map(ValueRef::from));
            let outcome = each(&cells);
            parked = park(cells);
            outcome
        };
        self.each_owned_row(statement, params, &mut borrowing)
    }

    /// The checks and per-statement resets every row-callback API makes before
    /// it runs anything.
    ///
    /// Split out so [`Engine::run_query_each`] and
    /// [`Engine::run_query_each_ref`] cannot drift: a statement that is refused
    /// by one has to be refused by the other, and a clock or interrupt reset
    /// missed on one path would be a difference in behaviour between two APIs
    /// that are supposed to answer identically.
    fn begin_row_callback(&mut self, statement: &Statement, params: &[Value]) -> Result<()> {
        if !statement.is_read_only() {
            return Err(Error::Unsupported(
                "row callbacks require a read-only statement".to_string(),
            ));
        }
        self.refresh_snapshot()?;
        statement.validate(&self.catalog, params)?;
        self.statement_clock.begin_statement();
        self.interrupt.begin_statement();
        Ok(())
    }

    /// Deliver every final row to `each` as owned `Value`s, counting them.
    ///
    /// The tail of [`Engine::run_query_each`], and the fallback
    /// [`Engine::run_query_each_ref`] borrows from for the shapes that have to
    /// materialise.
    fn each_owned_row(
        &mut self,
        statement: &Statement,
        params: &[Value],
        each: &mut RowSink<'_>,
    ) -> Result<usize> {
        let Plan::Select(select) = statement.plan() else {
            let result = self.run_refreshed(statement, params)?.into_rows()?;
            let count = result.rows.len();
            for row in &result.rows {
                each(row)?;
            }
            return Ok(count);
        };

        self.refresh_indexes()?;
        let env = self.read_env(params);
        let mut count = 0usize;
        let mut counted = |row: &[Value]| {
            each(row)?;
            count += 1;
            Ok(())
        };
        self.run_select_to(select, &env, None, Some(&mut counted))?;
        Ok(count)
    }

    /// Push one stored table's rows into a borrowing consumer without
    /// materialising any of them.
    ///
    /// The shape [`borrowed_projection`] admitted: one stored table, bare
    /// columns out, and optionally a `WHERE`, a `LIMIT` and an `OFFSET`. It is
    /// deliberately the same sequence of decisions
    /// [`Engine::run_select_to`]'s non-blocking single-table arm makes — the
    /// same [`scan_shape`], the same [`needed_columns`] mask, the same
    /// [`Engine::candidate_bytes`] access-path choice, the same
    /// borrowed-cell predicate test [`crate::exec::DecodeFilter`] runs — with
    /// one difference at the end: where that path turns the surviving cells
    /// into owned `Value`s "once, at the boundary", this one hands them
    /// straight to the callback and never crosses the boundary at all.
    ///
    /// Two buffers live across the whole scan and are re-lent to each row:
    /// the decoded cells and the projected row. Both are [`park`]ed between
    /// rows — cleared, so nothing borrowed outlives the page it came from —
    /// which is what makes "allocates nothing per row" true rather than
    /// aspirational. `a_borrowing_scan_allocates_nothing_per_row` counts it.
    fn run_borrowed_select(
        &self,
        plan: &SelectPlan,
        env: &Env<'_>,
        scratch: &mut BorrowScratch,
        sink: &mut dyn FnMut(&[ValueRef<'_>]) -> Result<()>,
    ) -> Result<usize> {
        let ScanShape {
            limit,
            offset,
            stop_after,
            ..
        } = scan_shape(plan, env, None)?;

        // `LIMIT 0` reads nothing. The owned path expresses this as `take(0)`
        // on the stream; here the emit-then-test loop below would deliver one
        // row before it noticed, so it is answered before the scan opens.
        if limit == Some(0) {
            return Ok(0);
        }

        let driving = &plan.from[0];
        let mask = needed_columns(plan);
        let driving_mask = mask.slice(0, driving.table.columns.len());
        // Same rule as `run_select_to`: without a `WHERE` every scanned row
        // reaches the consumer, so a first batch the size of the `LIMIT` reads
        // exactly what it needs. Under a filter the count is unknown.
        let first_batch = if plan.filter.is_none() {
            stop_after
        } else {
            None
        };
        let source =
            self.candidate_bytes(&driving.table, &plan.filter, env.params(), first_batch)?;

        // Whether a projected cell can be *moved* out of the decoded row or has
        // to be cloned. Only `ValueRef::Vector` owns anything, so this is about
        // one column type — but that one would allocate a `Vec<f32>` per row
        // per repeat, which is exactly what this API promises not to do.
        let projection = &scratch.projection;
        let mut moving = true;
        for (at, index) in projection.iter().enumerate() {
            if projection[..at].contains(index) {
                moving = false;
                break;
            }
        }

        let mut parked_cells = core::mem::take(&mut scratch.cells);
        let mut parked_out = core::mem::take(&mut scratch.out);
        let mut skipped = 0usize;
        let mut delivered = 0usize;

        for row in source {
            let (_, bytes) = row?;
            // The parked buffer is empty, so lending it to this row's cells
            // only shortens `'static`; `park` empties it again before it is
            // stored back. Same argument as `DecodeFilter::next`.
            let mut cells: Vec<ValueRef<'_>> = core::mem::take(&mut parked_cells);
            let verdict = decode_row_ref_masked_into(bytes.as_slice(), &driving_mask, &mut cells)
                .and_then(|()| match &plan.filter {
                    Some(filter) => eval::evaluate_ref(filter, &cells, Computed::NONE, env)
                        .map(|truth| eval::is_truthy(&truth)),
                    None => Ok(true),
                });
            // A failure ends the scan, so the buffer is dropped rather than
            // parked: there is no next row to lend it to.
            let admitted = verdict?;
            // `OFFSET` counts rows the `WHERE` admitted, then `LIMIT` counts
            // what is left — the order `finish_blocking` and the streamed path
            // both apply.
            if !admitted || skipped < offset {
                skipped += usize::from(admitted);
                parked_cells = park(cells);
                continue;
            }

            let mut out: Vec<ValueRef<'_>> = core::mem::take(&mut parked_out);
            for &index in projection.iter() {
                out.push(match cells.get_mut(index) {
                    Some(cell) if moving => core::mem::replace(cell, ValueRef::Null),
                    Some(cell) => cell.clone(),
                    None => ValueRef::Null,
                });
            }
            let outcome = sink(&out);
            parked_out = park(out);
            parked_cells = park(cells);
            outcome?;

            delivered += 1;
            if Some(delivered) == limit {
                break;
            }
        }
        scratch.cells = parked_cells;
        scratch.out = parked_out;
        Ok(delivered)
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

    /// Whether a transaction is open right now.
    ///
    /// True after [`Engine::begin`] and until the matching [`Engine::commit`]
    /// or [`Engine::rollback`] — including a transaction [`Engine::savepoint`]
    /// opened implicitly, since a caller that only issues `SAVEPOINT` has no
    /// other way to learn one is now open behind it.
    pub fn in_transaction(&self) -> bool {
        self.in_transaction
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
        self.transaction_is_implicit = false;
        self.transaction_log.clear();
        self.savepoints.clear();
        Ok(())
    }

    /// Commit the open transaction: make every write since [`Engine::begin`]
    /// durable in one storage commit.
    ///
    /// A lost race surfaces as [`Error::Conflict`], exactly as for a
    /// single-statement write: the engine reloads the winner's state and the
    /// handle stays usable. **Any** failure ends the transaction — the SQL
    /// contract for a commit that returns an error is that the transaction is
    /// over and nothing in it happened — so a failure that is not a conflict
    /// has to discard the buffered writes too.
    ///
    /// It did not until this was fixed, and the transaction that exposed it is
    /// one this whole item is about: a `COMMIT` refused because the write set
    /// does not fit one WAL region. `in_transaction` went false while the
    /// storage backend kept the entire write set buffered, which left the
    /// handle in a state no API could clear —
    /// [`Engine::rollback`] answers "rollback with no transaction open"
    /// precisely because the transaction is already over — and left the next
    /// autocommit statement to commit the abandoned writes along with its own.
    /// That is the silent-durability failure
    /// [`Engine::discard_failed_statement`](Self::discard_failed_statement)
    /// exists to prevent, and the explicit-`COMMIT` path did not reach it:
    /// [`Plan::is_read_only`](crate::plan::Plan::is_read_only) answers `true`
    /// for `Plan::Commit`, which is the right answer for the read-only
    /// connection guard that question is really asked for, and the wrong proxy
    /// for "this statement left nothing to clean up".
    ///
    /// Here it happens to be self-limiting — the write set only ever grows, so
    /// the next statement fails the same way and *its* discard finally clears
    /// it — but only because this particular error is permanent. A transient
    /// one would have made those writes durable at a moment nobody chose.
    pub fn commit(&mut self) -> Result<()> {
        self.require_transaction("commit")?;
        self.bump_write_version()?;
        let result = self.commit_storage();
        self.in_transaction = false;
        self.transaction_is_implicit = false;
        self.transaction_log.clear();
        self.savepoints.clear();
        match result {
            // A conflict has already done both halves: the storage layer threw
            // the transaction away and `commit_storage` reloaded this handle
            // from the winner. Repeating it would only cost a second reload.
            Ok(()) | Err(Error::Conflict) => result,
            Err(error) => {
                // Same two steps, and the same reasoning, as
                // `discard_failed_statement`: neither can fail for a reason the
                // error being returned has not already reported.
                let _ = self.storage.rollback();
                let _ = self.reload();
                Err(error)
            }
        }
    }

    /// Discard every write since [`Engine::begin`], leaving the database
    /// byte-identical to its state before the transaction.
    ///
    /// The buffered writes are dropped and the engine reloads itself from the
    /// committed store, so its catalog, counters and indexes agree with what is
    /// actually on disk. Every open savepoint is abandoned too — a plain
    /// `ROLLBACK` discards them along with everything else, confirmed against
    /// a real sqlite3 binary; only `ROLLBACK TO name` keeps the transaction
    /// and that one savepoint alive.
    pub fn rollback(&mut self) -> Result<()> {
        self.require_transaction("rollback")?;
        self.in_transaction = false;
        self.transaction_is_implicit = false;
        self.transaction_log.clear();
        self.savepoints.clear();
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

    /// `SAVEPOINT name`. Starts an implicit transaction first when none is
    /// open — confirmed against sqlite3: `SAVEPOINT s; ...; RELEASE s;` with
    /// no `BEGIN` persists its writes exactly as `BEGIN; ...; COMMIT;` would.
    fn savepoint(&mut self, name: &str) -> Result<()> {
        if !self.in_transaction {
            self.begin()?;
            self.transaction_is_implicit = true;
        }
        self.savepoints.push(SavepointFrame {
            name: name.to_string(),
            log_position: self.transaction_log.len(),
        });
        Ok(())
    }

    /// `RELEASE [SAVEPOINT] name`: keep everything this savepoint (and any
    /// nested one above it) buffered, and forget the markers. Releasing the
    /// outermost savepoint of a transaction *this* statement started
    /// implicitly commits it — confirmed against sqlite3 — but leaves an
    /// explicit `BEGIN`'s transaction open for its own `COMMIT`/`ROLLBACK`.
    fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let position = self
            .savepoints
            .iter()
            .rposition(|frame| frame.name == name)
            .ok_or_else(|| Error::Transaction(alloc::format!("no such savepoint: {name}")))?;
        self.savepoints.truncate(position);
        if self.savepoints.is_empty() && self.transaction_is_implicit {
            self.commit()?;
        }
        Ok(())
    }

    /// `ROLLBACK TO [SAVEPOINT] name`: reconstruct the transaction's state as
    /// it was when `name` was established, keeping the transaction (and that
    /// savepoint) open. Any savepoint nested *after* `name` no longer exists
    /// once its own base state is gone — confirmed against sqlite3 — but
    /// `name` itself may be rolled back to again, repeatedly.
    ///
    /// This does not partially undo the storage backend's buffered writes in
    /// place. It discards all of them (the same full rollback an ordinary
    /// `ROLLBACK` already does, which is already proven sound for every
    /// piece of per-transaction state — dirty pages, free-list bookkeeping,
    /// retrieval-index staging, the row-id counter) and replays the prefix
    /// of [`Engine::transaction_log`] that led to `name`, through the exact
    /// same [`Engine::run_refreshed`] every one of those statements already
    /// ran through once. A second, independent undo mechanism for each of
    /// those subsystems would be more code with more ways to disagree with
    /// the first one; replaying the same deterministic inputs against the
    /// same starting state is the same proof this codebase's DST sweeps
    /// already rest on.
    fn rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        let position = self
            .savepoints
            .iter()
            .rposition(|frame| frame.name == name)
            .ok_or_else(|| Error::Transaction(alloc::format!("no such savepoint: {name}")))?;
        let target_len = self.savepoints[position].log_position;
        self.savepoints.truncate(position + 1);
        self.replay_transaction_up_to(target_len)
    }

    /// The replay [`Engine::rollback_to_savepoint`] runs.
    ///
    /// Every entry here already ran once, against a state replay is
    /// reconstructing byte for byte, so a failure partway through is not a
    /// user error to report and continue past — it means something this
    /// engine assumed was deterministic was not. That is not a case to
    /// leave the caller in a half-replayed transaction over: the whole
    /// transaction is aborted, the same way an unrecoverable commit failure
    /// already is above, and the error says so rather than naming whichever
    /// replayed statement happened to be the one that surfaced it.
    fn replay_transaction_up_to(&mut self, target_len: usize) -> Result<()> {
        let prefix: Vec<LoggedStatement> = self.transaction_log[..target_len].to_vec();
        self.storage.rollback()?;
        self.reload()?;
        self.transaction_log.clear();
        self.replaying = true;
        let result = (|| -> Result<()> {
            for entry in &prefix {
                self.statement_clock.pin(entry.now);
                self.run_refreshed(&entry.statement, &entry.params)?;
            }
            Ok(())
        })();
        self.replaying = false;
        if let Err(error) = result {
            self.in_transaction = false;
            self.transaction_is_implicit = false;
            self.savepoints.clear();
            let _ = self.storage.rollback();
            let _ = self.reload();
            return Err(Error::Transaction(alloc::format!(
                "ROLLBACK TO SAVEPOINT could not reconstruct the transaction and aborted it \
                 entirely: {error}"
            )));
        }
        // Logging was suppressed during replay (`self.replaying`); restore
        // the log to exactly what was just replayed, so a later `ROLLBACK
        // TO` an even earlier savepoint still has it, and the next new
        // statement extends it rather than starting over.
        self.transaction_log = prefix;
        Ok(())
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
        let predicted = self.write_version;
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
                self.schema_version =
                    read_counter(&self.storage, SCHEMA_VERSION_KEY, "schema version")?
                        .unwrap_or_default();
                if self.write_version == predicted {
                    // Nobody got between this handle and the root it committed
                    // onto, so its indexes describe every committed row: the
                    // rows it just wrote were indexed before `end_write` ran.
                    self.indexed_version = self.write_version;
                } else {
                    // A rebase. The winner's rows are committed underneath this
                    // handle's indexes, which have never seen them, and
                    // [`Storage::refresh`] will not report it — this handle
                    // already holds the rebased root. `indexed_version` stays
                    // where the previous commit left it, which is the version
                    // the next statement's catch-up has to replay from; the one
                    // version of overlap it re-applies is this statement's own,
                    // and re-applying a row is idempotent.
                    self.indexed_version = self.indexed_version.min(self.write_version);
                }
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
        // `refresh` answering `false` means the committed *root* has not moved
        // since this handle last looked, which is almost always the same thing
        // as "this handle is current". The exception is a rebased commit: the
        // root moved *while* this handle was committing onto it, so it already
        // holds the winner's root and `refresh` has nothing to report, yet its
        // indexes never saw the winner's rows. `indexed_version` is what
        // records that, and it is the reason this is not a plain early return.
        if !self.storage.refresh()? && self.indexed_version == self.write_version {
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
    /// INDEX` has to be honoured even though it changed no row.
    ///
    /// When only the write version moved, the change log names exactly the
    /// rows that moved with it, and
    /// [`Engine::catch_up_indexes`](Self::catch_up_indexes) applies those and
    /// nothing else. That is the difference between "one connection inserted
    /// one row" costing one re-indexed document and costing every document in
    /// the database, once per other connection — the shape that made this the
    /// dominant cost on a multi-connection server, because a saved index blob
    /// is stamped at the version it was written at and is therefore stale for
    /// all but one in [`INDEX_PERSIST_INTERVAL`] commits.
    ///
    /// Only when the catalog moved too, or the log cannot answer, does
    /// [`Engine::restore_indexes`](Self::restore_indexes) run — the same code
    /// as on open, which loads a saved index whose stamp matches the new write
    /// version and rebuilds from the rows only when it does not.
    fn adopt_committed_state(&mut self) -> Result<()> {
        let mut catalog = match self.storage.get_meta(CATALOG_KEY)? {
            Some(bytes) => Catalog::decode(&bytes)?,
            None => Catalog::new(),
        };
        // Carried across before the replace below, or this handle's own
        // `CREATE TEMPORARY TABLE`s would vanish the moment it notices any
        // other handle's commit — see `Catalog::carry_temp_schema_from`.
        catalog.carry_temp_schema_from(&mut self.catalog);
        // Not `write_version`: after a rebased commit this handle's indexes
        // describe an *older* version than the counter does. See
        // [`Engine::indexed_version`](Self::indexed_version).
        let previous_version = self.indexed_version;
        let previous_catalog = core::mem::replace(&mut self.catalog, catalog);
        let catalog_changed = self.catalog != previous_catalog;
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
        self.schema_version =
            read_counter(&self.storage, SCHEMA_VERSION_KEY, "schema version")?.unwrap_or_default();
        self.planner_stats = if catalog_changed {
            PlannerStats::empty(self.write_version)
        } else {
            load_planner_stats(
                &self.storage,
                self.write_version,
                self.schema_version,
                &self.catalog,
            )?
        };

        if self.write_version == previous_version && self.catalog == previous_catalog {
            // Whatever the other handle committed, it was not a row and not a
            // schema — a checkpointed index blob, a trimmed change record. The
            // indexes this handle holds still describe the committed rows.
            return Ok(());
        }

        // Anything this handle had queued describes rows from before the state
        // it just adopted; the commit that would have published them is gone.
        self.pending_changes.clear();

        // The catalog is what decides which indexes exist and over which
        // columns, so a schema this handle has not seen leaves nothing to
        // catch up *to* — an index it has never opened has no incremental
        // form. That falls through to the wholesale restore below.
        if self.catalog == previous_catalog && self.catch_up_indexes(previous_version)? {
            self.indexed_version = self.write_version;
            // Same reset the rebuild path below does, and for the same reason:
            // the blob on disk is stamped at a version that is no longer
            // current, and rewriting megabytes of index is not the business of
            // the read that happened to notice. Leaving it alone would make a
            // handle that has only ever read start saving indexes — a write,
            // which a read-only handle cannot make at all.
            self.persisted_version = self.write_version;
            return self.refresh_indexes();
        }

        self.persisted_version = self.write_version;
        self.text_indexes.clear();
        self.vector_indexes.clear();
        self.dirty_tables.clear();
        self.restore_indexes()
    }

    /// Apply the rows another handle committed since `from` to the retrieval
    /// indexes this handle already holds, or answer `false` when they have to
    /// be rebuilt from every row instead.
    ///
    /// # Why this is sound
    ///
    /// The precondition is `indexed_version`'s invariant: this handle's
    /// retrieval indexes describe every committed row as of `from`. The change
    /// log names every row that changed after it — that is the whole contract
    /// of [`crate::cdc`], and it is written in the same commit as the change,
    /// so a row cannot move without a record. Reconciling exactly those rows
    /// therefore lands on the same index as reading all of them would; the
    /// last test in `tests/foreign_commit_indexes.rs` asserts that
    /// equivalence against a freshly built index rather than trusting the
    /// argument.
    ///
    /// Each row is reconciled rather than replayed: the id is dropped from
    /// every retrieval index of its table and then re-derived from the
    /// committed row, if there still is one. This handle has no record of what
    /// a row used to say, so a targeted "remove the old text" is not available
    /// — and it is not needed, because dropping and re-deriving is correct for
    /// an insert, an update and a delete alike, is idempotent, and collapses
    /// any number of changes to one row into one unit of work.
    ///
    /// # When it declines
    ///
    /// * The log no longer reaches back to `from`. A consumer that fell behind
    ///   the retention window has to resynchronise from a scan, and so does an
    ///   index. This also bounds the work: the replay can never span more than
    ///   [`CDC_RETENTION`] statements, because past that the log itself is the
    ///   thing that is missing.
    /// * A record inside the range is missing or empty. Every version in the
    ///   retained range has exactly one non-empty record, so this cannot
    ///   happen — and if it did it would mean this handle cannot know what
    ///   changed, which is precisely when guessing must not be an option.
    /// * A vector backend that keeps itself in the database is not current
    ///   after re-opening it — its graph is mid-build, or was written by a
    ///   binary this one cannot read. See
    ///   [`Engine::adopt_self_persisting_vector_indexes`], which is what makes
    ///   such a backend current, and why replaying rows into one would be
    ///   wrong rather than merely redundant.
    fn catch_up_indexes(&mut self, from: u64) -> Result<bool> {
        if from > self.write_version || from < self.cdc_floor {
            return Ok(false);
        }
        if !self.adopt_self_persisting_vector_indexes()? {
            return Ok(false);
        }
        if !self.adopt_self_persisting_text_indexes()? {
            return Ok(false);
        }

        // Deduplicated per table: the reconcile below is idempotent, so a row
        // touched by ten of the replayed statements is still one unit of work.
        let mut touched: BTreeMap<String, BTreeSet<RowId>> = BTreeMap::new();
        for version in (from + 1)..=self.write_version {
            let Some(bytes) = self.storage.get_meta(&cdc::record_key(version))? else {
                return Ok(false);
            };
            if bytes.is_empty() {
                return Ok(false);
            }
            for change in cdc::decode_record(version, &bytes)? {
                touched
                    .entry(change.table.to_ascii_lowercase())
                    .or_default()
                    .insert(change.id);
            }
        }

        for (name, ids) in &touched {
            // A table the catalog does not name was dropped and recreated
            // inside the replayed range, back to a schema equal to this
            // handle's. Its rows are reconciled under the name that survived,
            // if any; there is no backend under this one to reconcile.
            let Some(table) = self.catalog.table(name).cloned() else {
                continue;
            };
            let declared: Vec<Index> = self
                .catalog
                .indexes_for(&table.name)
                .into_iter()
                .filter(|index| index.kind.is_retrieval())
                .cloned()
                .collect();
            // A table with no retrieval index has nothing that a rebuild would
            // have redone either: its B-tree entries are durable rows the
            // writer committed beside the change.
            if declared.is_empty() {
                continue;
            }
            // The ones the replay must leave alone. A self-persisting backend
            // was brought up to date by re-opening it, above, and the writer's
            // edit is already in the graph this handle just read: replaying the
            // row into it would apply that edit a second time — `remove`
            // tombstoning a node the graph still has live, `insert` adding a
            // duplicate of it — and would do so as *writes*, from a handle that
            // is only reading, into a graph shared by every other handle on the
            // file.
            let declared: Vec<Index> = declared
                .into_iter()
                .filter(|index| !self.index_is_self_persisting(index))
                .collect();
            if declared.is_empty() {
                continue;
            }
            // Once for the table, not once per row: `name` is already the
            // lowercased key this set holds, and the loop below reconciles
            // however many rows into the same backends.
            self.dirty_tables.insert(name.clone());
            for &id in ids {
                for index in &declared {
                    let key = retrieval_key(&index.table, &index.columns);
                    match index.kind {
                        IndexKind::FullText => {
                            if let Some(backend) = self.text_indexes.get_mut(&key) {
                                backend.remove(id)?;
                            }
                        }
                        IndexKind::Vector => {
                            if let Some(backend) = self.vector_indexes.get_mut(&key) {
                                backend.remove(id)?;
                            }
                        }
                        IndexKind::BTree => unreachable!("filtered out above"),
                    }
                }
                if let Some(bytes) = self.storage.get_row(&table.name, id)? {
                    let row = decode_row(&bytes)?;
                    for index in &declared {
                        self.index_row_for_index(&table, index, id, &row)?;
                    }
                }
            }
        }
        Ok(true)
    }

    /// The `Vector` index declaration a retrieval key names, if the catalog
    /// still names one.
    fn declared_vector_index(&self, key: &(String, Vec<String>)) -> Option<Index> {
        let table = self.catalog.table(&key.0)?;
        self.catalog
            .indexes_for(&table.name)
            .into_iter()
            .find(|index| {
                index.kind == IndexKind::Vector
                    && retrieval_key(&index.table, &index.columns) == *key
            })
            .cloned()
    }

    /// Whether `index`'s backend keeps its own structure inside the database.
    fn index_is_self_persisting(&self, index: &Index) -> bool {
        let key = retrieval_key(&index.table, &index.columns);
        match index.kind {
            IndexKind::Vector => self
                .vector_indexes
                .get(&key)
                .is_some_and(|backend| backend.is_self_persisting()),
            IndexKind::FullText => self
                .text_indexes
                .get(&key)
                .is_some_and(|backend| backend.is_self_persisting()),
            IndexKind::BTree => false,
        }
    }

    /// The `FullText` index declaration a retrieval key names, if the catalog
    /// still names one. The text twin of [`Engine::declared_vector_index`].
    fn declared_text_index(&self, key: &(String, Vec<String>)) -> Option<Index> {
        let table = self.catalog.table(&key.0)?;
        self.catalog
            .indexes_for(&table.name)
            .into_iter()
            .find(|index| {
                index.kind == IndexKind::FullText
                    && retrieval_key(&index.table, &index.columns) == *key
            })
            .cloned()
    }

    /// Re-read every full-text backend that keeps its structure in the
    /// database, for exactly the reasons
    /// [`Engine::adopt_self_persisting_vector_indexes`] gives — the writer
    /// already applied its rows to the postings in the file, so replaying them
    /// here would apply them twice, as writes from a handle that only read.
    ///
    /// Cheaper than the vector half, and worth saying so: re-opening a paged
    /// BM25 index reads its header and nothing else, because it keeps no
    /// resident `RowId -> ordinal` map to rebuild. So this is `O(1)` per index
    /// where the ANN side is `O(nodes)`.
    fn adopt_self_persisting_text_indexes(&mut self) -> Result<bool> {
        let stale: Vec<(String, Vec<String>)> = self
            .text_indexes
            .iter()
            .filter(|(_, backend)| backend.is_self_persisting())
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            let (Some(index), Some(table)) = (
                self.declared_text_index(&key),
                self.catalog.table(&key.0).cloned(),
            ) else {
                return Ok(false);
            };
            self.open_one_index(&table, &index)?;
            let current = self
                .text_indexes
                .get(&key)
                .is_some_and(|backend| backend.stored_write_version() == Some(self.write_version));
            if !current {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Re-read every vector backend that keeps its structure in the database,
    /// so this handle sees the graph the committing handle wrote rather than
    /// the copy it held before. Answers `false` when what it finds is not
    /// current, which is the caller's signal to rebuild from the rows instead.
    ///
    /// # Why re-opening is the catch-up, and a replay would be wrong
    ///
    /// For an in-memory backend this handle's copy *is* the index, so bringing
    /// it up to date means applying the changed rows to it. For a
    /// self-persisting one the index is in the file and the writer already
    /// updated it there — every node record, the entry point, the live set and
    /// the stamp. What this handle holds is a cache of that, and the honest way
    /// to refresh a cache is to read it again. Replaying rows on top would
    /// apply the writer's change twice and, worse, would do it as writes from a
    /// handle that only read.
    ///
    /// # What it costs, and why it is still worth doing
    ///
    /// Re-opening walks the graph's node records to rebuild the `RowId ->
    /// node` map, so this is O(nodes) per foreign commit — not free, and the
    /// reason `docs/enterprise-readiness.md` presents the paged index as a
    /// trade rather than a default. It replaces something far worse: declining
    /// here means the *whole table* is rebuilt, which re-tokenises every
    /// document into the full-text index and re-inserts every embedding into a
    /// graph that was already correct. That is the failure
    /// `tests/foreign_commit_indexes.rs` exists to prevent, and turning the
    /// paged index on used to reintroduce it — measured at 41 re-indexed
    /// documents for one foreign insert into a 40-row table.
    fn adopt_self_persisting_vector_indexes(&mut self) -> Result<bool> {
        let stale: Vec<(String, Vec<String>)> = self
            .vector_indexes
            .iter()
            .filter(|(_, backend)| backend.is_self_persisting())
            .map(|(key, _)| key.clone())
            .collect();
        if stale.is_empty() {
            return Ok(true);
        }
        for key in stale {
            // The catalog is known equal to the one these backends were opened
            // under — `adopt_committed_state` checked that before calling here
            // — so a backend with no declaration behind it cannot happen. If it
            // somehow does, decline rather than leave a live index nobody can
            // name.
            let (Some(index), Some(table)) = (
                self.declared_vector_index(&key),
                self.catalog.table(&key.0).cloned(),
            ) else {
                return Ok(false);
            };
            self.open_one_index(&table, &index)?;
            // The same currency test `load_saved_indexes` applies to a saved
            // blob, applied to the stamp the graph carries: a graph that does
            // not describe these rows is not a catch-up, it is a rebuild.
            let current = self
                .vector_indexes
                .get(&key)
                .is_some_and(|backend| backend.stored_write_version() == Some(self.write_version));
            if !current {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Discard every piece of in-memory state and rebuild it from the store.
    ///
    /// The same work [`Engine::open`] does, on an engine that is already open.
    fn reload(&mut self) -> Result<()> {
        let mut catalog = match self.storage.get_meta(CATALOG_KEY)? {
            Some(bytes) => Catalog::decode(&bytes)?,
            None => Catalog::new(),
        };
        // Carried across for the same reason `Engine::adopt_committed_state`
        // does: nothing durable ever describes a temporary table, so a plain
        // replace would silently drop every one this handle holds. Safe to
        // carry unconditionally even here, on a real rollback path, because
        // `CREATE TEMPORARY TABLE`/`DROP TABLE` of one are refused inside a
        // transaction (`Engine::create_table`, `Engine::drop_table`) and
        // validated *before* mutating `self.catalog` when they run outside
        // one (`Engine::create_table_uncommitted`) — there is never an
        // uncommitted temporary-schema change for a reload to need to undo.
        catalog.carry_temp_schema_from(&mut self.catalog);
        self.catalog = catalog;
        self.invalidate_rules();
        self.next_row_id =
            read_counter(&self.storage, NEXT_ROW_ID_KEY, "next row id")?.unwrap_or(1);
        self.write_version =
            read_counter(&self.storage, WRITE_VERSION_KEY, "write version")?.unwrap_or_default();
        self.cdc_floor =
            read_counter(&self.storage, CDC_FLOOR_KEY, "change floor")?.unwrap_or_default();
        self.schema_version =
            read_counter(&self.storage, SCHEMA_VERSION_KEY, "schema version")?.unwrap_or_default();
        self.planner_stats = load_planner_stats(
            &self.storage,
            self.write_version,
            self.schema_version,
            &self.catalog,
        )?;
        self.persisted_version = self.write_version;
        self.text_indexes.clear();
        self.vector_indexes.clear();
        self.dirty_tables.clear();
        // Changes the rolled-back statement had queued describe rows that do
        // not exist. Publishing them would tell a CDC consumer about a write
        // nobody made.
        self.pending_changes.clear();
        self.restore_indexes()
    }

    // ------------------------------------------------------------------ DDL

    /// Persist the catalog and advance its independent revision counter.
    ///
    /// DDL can leave `write_version` unchanged when it touches no rows, but it
    /// still invalidates planner statistics. The separate counter makes that
    /// invalidation survive a close and reopen, including a drop/recreate with
    /// an identical catalog encoding.
    fn persist_catalog(&mut self) -> Result<()> {
        self.schema_version = self.schema_version.saturating_add(1);
        self.planner_stats = PlannerStats::empty(self.write_version);
        self.storage
            .put_meta(SCHEMA_VERSION_KEY, &self.schema_version.to_le_bytes())?;
        self.storage.put_meta(CATALOG_KEY, &self.catalog.encode())
    }

    fn create_table(&mut self, plan: &CreateTablePlan, params: &[Value]) -> Result<Outcome> {
        // A temporary table's declaration lives only in `self.catalog` —
        // never in `Storage`'s buffered writes, since it is never durable at
        // all (`Catalog::temp_tables`'s doc) — so there is nothing for
        // `ROLLBACK` to undo it with. Refusing here rather than letting it
        // silently survive a rollback is the same choice this feature makes
        // everywhere else: a disclosed gap, not a wrong answer nobody asked
        // for. Row-level writes to an already-existing temporary table are
        // unaffected — those go through `Storage::commit`/`rollback` like any
        // other table's, via `temp_storage::TempTableRouter`.
        if plan.table.temporary && self.in_transaction {
            return Err(Error::Unsupported(
                "CREATE TEMPORARY TABLE inside a transaction is not supported yet: its \
                 declaration is not buffered the way an ordinary CREATE TABLE's is, so \
                 ROLLBACK could not undo it"
                    .to_string(),
            ));
        }
        // `IF NOT EXISTS` asks whether the *name* is taken **in the schema
        // this statement targets** — a durable `t` existing is no obstacle to
        // `CREATE TEMPORARY TABLE IF NOT EXISTS t`, and a temporary `t`
        // existing is no obstacle to the durable form, the same shadowing
        // [`Catalog::create_temp_table`]'s doc describes. `... AS SELECT`
        // leaves with the same answer: a caller finding its table already
        // there does not have its `SELECT` run either, which matches a real
        // sqlite3 binary — an existing `t2` is left untouched by `CREATE
        // TABLE IF NOT EXISTS t2 AS SELECT ...`, byte for byte.
        let exists = if plan.table.temporary {
            self.catalog.temp_table(&plan.table.name).is_some()
        } else {
            self.catalog.durable_table(&plan.table.name).is_some()
        };
        if plan.if_not_exists && exists {
            return Ok(Outcome::Ddl);
        }
        self.create_table_uncommitted(plan)?;
        match &plan.as_select {
            None => {
                self.end_write()?;
                Ok(Outcome::Ddl)
            }
            // The table is created and empty at this point, so populating it
            // is exactly an `INSERT INTO <the new table> SELECT ...` — built
            // here rather than at plan time because it targets column
            // ordinals that only exist once `create_table_uncommitted` has
            // registered them. One `end_write` covers both: a process that
            // dies between them must not leave a table with no rows behind,
            // any more than a crash mid-`INSERT` may.
            Some(query) => {
                let insert = InsertPlan {
                    table: plan.table.name.clone(),
                    source: InsertSource::Select {
                        query: query.clone(),
                        targets: (0..plan.table.columns.len()).collect(),
                    },
                    on_conflict: OnConflict::abort(),
                    returning: None,
                };
                let (written, _returned) = self.insert_uncommitted(&insert, params)?;
                self.end_write()?;
                Ok(Outcome::Written(written))
            }
        }
    }

    /// The declaration half of `CREATE TABLE`: registers the table and its
    /// indexes, but takes no commit. Shared by an ordinary `CREATE TABLE`,
    /// which commits immediately after, and `... AS SELECT`, which still has
    /// rows to insert into the same transaction first.
    fn create_table_uncommitted(&mut self, plan: &CreateTablePlan) -> Result<()> {
        let table = &plan.table;
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
        if table.temporary {
            // Validated *before* any catalog mutation, deliberately: a
            // failure here must leave nothing behind for a later
            // `Engine::reload` to (wrongly) carry forward — see
            // `Catalog::carry_temp_schema_from`'s doc for why that carry
            // is otherwise safe. Implicit indexes and `declare_unique_indexes`
            // do not apply below: a temporary table refuses every `UNIQUE`
            // beyond a lone `INTEGER PRIMARY KEY` (`sql::plan_create_table`)
            // and `CREATE INDEX` outright (`sql::plan_create_index`), so
            // there is never one to declare.
            sql::table_rules(table, &self.catalog)?;
            self.catalog
                .create_temp_table(table.clone(), plan.constraints.clone())?;
            self.storage.set_temp_table(&table.name, true);
            return Ok(());
        }
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
        self.persist_catalog()?;
        Ok(())
    }

    /// `DROP TABLE`: remove the declaration, its indexes and every row.
    ///
    /// The rows go one at a time because that is the whole of the storage
    /// surface — there is no "drop this key range" — so this is O(rows), and
    /// each deletion joins the statement's transaction like any other write.
    fn drop_table(&mut self, plan: &DropTablePlan) -> Result<Outcome> {
        let temporary = match self.catalog.table(&plan.name) {
            Some(existing) => existing.temporary,
            None => {
                if plan.if_exists {
                    return Ok(Outcome::Ddl);
                }
                return Err(Error::Catalog(alloc::format!(
                    "no such table: {}",
                    plan.name
                )));
            }
        };
        // Same reasoning as `CREATE TEMPORARY TABLE` inside a transaction:
        // removing the declaration is a `self.catalog`-only change with
        // nothing buffered in `Storage` for `ROLLBACK` to undo.
        if temporary && self.in_transaction {
            return Err(Error::Unsupported(
                "DROP TABLE on a temporary table inside a transaction is not supported yet, \
                 for the same reason CREATE TEMPORARY TABLE inside one is not"
                    .to_string(),
            ));
        }
        self.invalidate_rules();
        let (table, indexes) = self.catalog.drop_table(&plan.name)?;
        for index in &indexes {
            self.forget_index(index)?;
            // Unlike `ALTER TABLE`, this really does invalidate the entries:
            // the rows they point at are about to stop existing.
            self.purge_index_entries(index)?;
        }
        if table.without_rowid {
            // No `note_change` here, for the same disclosed reason
            // `Engine::delete_without_rowid` has none: there is no `RowId`
            // to report one under.
            for (key, _) in crate::traits::scan_all_keyed(&self.storage, &table.name)? {
                self.storage.delete_row_keyed(&table.name, &key)?;
            }
        } else {
            // The scan and the deletes below still reach `table`'s rows
            // through `temp_storage::TempTableRouter` even for a temporary
            // table, because `Storage::set_temp_table(name, false)` has not
            // run yet — that has to wait until after the rows are gone, or
            // these very calls would be routed to the durable side instead
            // and find nothing.
            for (id, _) in self.scan_all(&table.name)? {
                self.storage.delete_row(&table.name, id)?;
                // A temporary table's rows were never visible to another
                // handle to begin with, so there is nothing for a CDC
                // consumer — which reads the *durable* change log — to be
                // told; logging one anyway would describe a table it has no
                // way to look up.
                if !table.temporary {
                    self.note_change(&table.name, id, ChangeKind::Delete);
                }
            }
        }
        if table.temporary {
            // Releases the row storage this table's name was routed to —
            // without this, a long-lived handle that creates and drops many
            // temporary tables would leak an entry in
            // `temp_storage::TempTableRouter`'s bookkeeping per drop, none
            // of them ever reachable again.
            self.storage.set_temp_table(&table.name, false);
        } else {
            self.persist_catalog()?;
        }
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
        // Every `ALTER TABLE` action, `RENAME COLUMN` included: the catalog
        // mutation methods it goes through (`Catalog::add_column`,
        // `Catalog::rename_column`, ...) only know how to reach
        // `Catalog::tables`, not `Catalog::temp_tables` — a disclosed gap
        // rather than a confusing "no such table" from a method that was
        // never told to look in the other schema.
        if before.temporary {
            return Err(Error::Unsupported(alloc::format!(
                "ALTER TABLE on temporary table `{}` is not supported yet",
                before.name
            )));
        }
        // `RENAME COLUMN` is a pure catalog change — nothing about a stored
        // row changes, only what its ordinal is called — so it needs
        // nothing here. The other three all rewrite every row keyed by
        // `RowId`, which this table's rows are not; a disclosed gap rather
        // than a silent one, the same as everywhere else in this feature.
        if before.without_rowid && !matches!(plan.action, AlterAction::RenameColumn { .. }) {
            return Err(Error::Unsupported(alloc::format!(
                "{} on WITHOUT ROWID table `{}` is not supported yet",
                match &plan.action {
                    AlterAction::AddColumn(_) => "ADD COLUMN",
                    AlterAction::RenameTable(_) => "RENAME TO",
                    AlterAction::DropColumn(_) => "DROP COLUMN",
                    AlterAction::RenameColumn { .. } => unreachable!("excluded above"),
                },
                before.name
            )));
        }
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
                        after.strict,
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
                for (id, bytes) in self.scan_all(&before.name)? {
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
        self.dirty_tables.clear();
        self.restore_indexes()?;
        self.persist_catalog()?;
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
                // A B-tree index has no distance to be built under.
                metric: VectorMetric::default(),
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
        for (id, bytes) in self.scan_all(&table.name)? {
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
        // Statistics include table and index identities. A catalog change
        // therefore invalidates them even when no row mutation advances the
        // write version; the next explicit ANALYZE can refresh the snapshot.
        self.planner_stats = PlannerStats::empty(self.write_version);
        // A hash build's mask and column ordinals were resolved against this
        // same catalog. Even when DDL changed no row and therefore did not
        // advance `write_version`, a build from the old shape is not valid for
        // a freshly prepared plan over the new one.
        self.hash_join_cache.get_mut().take();
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
                let backend = if self.options.paged_text_indexes {
                    self.open_paged_text_index(&index.table, &index.columns)?
                } else {
                    self.factory.full_text(&index.table, index.column())?
                };
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
                // The declaration's metric, not a default: it decides both
                // what the graph stores (cosine normalises, L2 does not) and
                // how two stored vectors are compared, and a backend opened
                // under the wrong one answers with the wrong neighbours and no
                // error. See `catalog::Index::metric`.
                let metric = index.metric;
                let backend = if self.options.paged_vector_indexes {
                    self.open_paged_vector_index(
                        &index.table,
                        index.column(),
                        dim,
                        quantized,
                        metric,
                    )?
                } else if quantized {
                    self.factory
                        .quantized_vector(&index.table, index.column(), dim, metric)?
                } else {
                    self.factory
                        .vector(&index.table, index.column(), dim, metric)?
                };
                self.vector_indexes.insert(key, backend);
            }
        }
        Ok(())
    }

    /// Open the paged BM25 backend for one index, restoring whatever postings
    /// the database already holds for it.
    ///
    /// Like the paged ANN backend it shares the engine's storage handle and
    /// does not commit: its writes join whatever transaction the engine has
    /// open, so the postings and the rows they describe reach the log
    /// together.
    fn open_paged_text_index(
        &self,
        table: &str,
        columns: &[String],
    ) -> Result<Box<dyn FullTextIndex>> {
        let index = PagedBm25Index::open(
            self.storage.clone(),
            full_text_index_namespace(table, columns),
        )?
        .joined_to_caller_transaction();
        Ok(Box::new(index))
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
        metric: VectorMetric,
    ) -> Result<Box<dyn VectorIndex>> {
        let namespace = vector_index_namespace(table, column);
        let index = if quantized {
            PagedHnswIndex::open_quantized_with_metric(
                self.storage.clone(),
                namespace,
                dim,
                metric,
            )?
        } else {
            PagedHnswIndex::open_with_metric(self.storage.clone(), namespace, dim, metric)?
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
            metric: create.metric,
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
        let rows = self.scan_all(&table.name)?;
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

        self.persist_catalog()?;
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
            // Nothing is written yet, so a stop here is as clean as one during
            // the scan that produced `rows`.
            self.interrupt.check()?;
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
            // Checked even though this loop commits as it goes, because the
            // recovery for a build that failed part way through already exists
            // and is what `create_index`'s error arm does: the declaration is
            // dropped and the handle reloaded, which leaves the entries that
            // reached the platter unreachable — nothing reads a prefix no
            // declaration names, and the next `CREATE INDEX` of this name
            // purges them before it starts. A `CREATE INDEX` over ten million
            // rows is exactly the statement an operator wants to be able to
            // stop.
            self.interrupt.check()?;
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

        let stored: Vec<Vec<Value>> = self
            .scan(&table.name)
            .map(|row| decode_row(&row?.1))
            .collect::<Result<_>>()?;
        for (index, row) in stored.iter().enumerate() {
            // The inner loop, not the outer: this is O(rows squared) and the
            // outer one advances once per `rows` comparisons, which on a large
            // table is minutes between checks.
            for other in &stored[index + 1..] {
                self.interrupt.check()?;
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
        self.persist_catalog()?;
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
                self.persist_catalog()?;
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
                self.persist_catalog()?;
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

        self.persist_catalog()?;
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
            let indexes = RowIndexes::resolve(&self.catalog, &table.name);
            // The one scan in this engine that is deliberately **not**
            // cancellable, and it has to stay that way.
            //
            // By the time this loop runs the retrieval indexes have already
            // been cleared and `persisted_version` already advanced, so an
            // early return here would leave the handle holding empty indexes
            // that claim to describe the committed rows — `bm25_score` and
            // `vector_score` would then answer *nothing* for rows that are
            // visibly there, with no error anywhere. That is the exact
            // silent-wrong-answer failure `docs/enterprise-readiness.md`
            // blocker 1 was about.
            //
            // It is also reached from two places where cancellation makes no
            // sense at all: `refresh_snapshot`, which runs *before* a statement
            // has been given a deadline, and `reload`, which is how the engine
            // recovers from a statement that was itself cancelled. A rebuild
            // is the engine repairing its own consistency, not the client's
            // work, and the client is not entitled to interrupt it.
            for row in crate::traits::scan_all(&self.storage, &table.name)? {
                let (id, bytes) = row;
                let row = decode_row(&bytes)?;
                // Only the retrieval half. A B-tree index needs no rebuild:
                // its entries are durable rows that were written in the same
                // transaction as the rows they describe, so they are already
                // exactly as current as the data — and re-deriving them here
                // would make every open O(rows × indexes) of pointless writes.
                self.index_row_retrieval(table, &indexes, id, &row)?;
            }
        }
        // Every table has just been read or restored at the committed state,
        // so this is the one place that can set the invariant outright rather
        // than maintain it.
        self.indexed_version = self.write_version;
        self.refresh_indexes()
    }

    /// Empty the retrieval indexes of `table` that keep themselves in the
    /// database, ahead of a rebuild from the rows.
    fn reset_self_persisting_indexes(&mut self, table: &Table) -> Result<()> {
        let declared: Vec<Index> = self
            .catalog
            .indexes_for(&table.name)
            .into_iter()
            .cloned()
            .collect();
        for index in declared {
            let key = retrieval_key(&index.table, &index.columns);
            match index.kind {
                IndexKind::Vector => {
                    if let Some(backend) = self.vector_indexes.get_mut(&key) {
                        if backend.is_self_persisting() {
                            backend.reset()?;
                        }
                    }
                }
                IndexKind::FullText => {
                    if let Some(backend) = self.text_indexes.get_mut(&key) {
                        if backend.is_self_persisting() {
                            backend.reset()?;
                        }
                    }
                }
                IndexKind::BTree => {}
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
            let key = retrieval_key(&index.table, &index.columns);
            let stamp = match index.kind {
                IndexKind::Vector => self
                    .vector_indexes
                    .get(&key)
                    .filter(|backend| backend.is_self_persisting())
                    .map(|backend| backend.stored_write_version()),
                IndexKind::FullText => self
                    .text_indexes
                    .get(&key)
                    .filter(|backend| backend.is_self_persisting())
                    .map(|backend| backend.stored_write_version()),
                IndexKind::BTree => unreachable!("filtered out above"),
            };
            if let Some(stamp) = stamp {
                if stamp == Some(self.write_version) {
                    continue;
                }
                return Ok(false);
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

    /// Write a consistent copy of the committed database to `dest`, while
    /// other handles keep committing to the source.
    ///
    /// The copy is one committed snapshot — never a mix of two commits, and
    /// never one table read at an older snapshot than another, which is the
    /// failure a statement-at-a-time SQL dump has to work to avoid and this
    /// gets for free from the copy-on-write tree. See
    /// [`crate::btree::backup`] for the argument and for the one configuration
    /// it refuses to guess at.
    ///
    /// Refused inside an explicit transaction, for the same reason
    /// [`Engine::checkpoint`] is: a backup is of *committed* state, and this
    /// handle's buffered writes are not that. Rolling them silently out of the
    /// copy would be defensible; doing it without saying so, while the caller
    /// believes the transaction is part of the database, is not.
    ///
    /// The snapshot is stepped forward first, so a handle that has been idle
    /// between statements copies what the file holds now rather than what it
    /// held when it last ran one.
    ///
    /// # What the copy does *not* carry forward verbatim
    ///
    /// Nothing that matters, but worth stating: the retrieval index blobs the
    /// engine persists into the tree are copied like any other row, so the
    /// backup opens with whatever was last saved. Those blobs are a cache
    /// stamped with the write version they describe — a stale one is discarded
    /// and rebuilt on open (`crate::traits`, "Persisting an index"), so the
    /// worst a backup taken between saves costs is a rebuild, never a wrong
    /// answer. [`Engine::checkpoint`] first if that rebuild matters.
    pub fn backup_to(&mut self, dest: &mut dyn crate::btree::Device) -> Result<BackupSummary> {
        if self.in_transaction {
            return Err(Error::Transaction(
                "cannot back up inside a transaction; commit or roll back first".to_string(),
            ));
        }
        self.refresh_snapshot()?;
        self.storage.backup_to(dest)
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
    ///
    /// That trade is right for the incremental case and wrong for the bulk
    /// one, where "one full build" is minutes and lands on whichever query
    /// happens to arrive first with nothing in the statement to explain why.
    /// [`Engine::reindex`] is the way to ask for it deliberately instead; it
    /// runs exactly this work through [`Engine::build_indexes`], so the
    /// deferral itself is unchanged — a loader that never queries still pays
    /// nothing.
    fn refresh_indexes(&mut self) -> Result<()> {
        if self.dirty_tables.is_empty() {
            return Ok(());
        }
        self.build_indexes(&Reindex::Everything).map(|_| ())
    }

    /// Record that `table`'s retrieval backends are holding uncommitted writes.
    fn mark_indexes_dirty(&mut self, table: &str) {
        // Compared case-insensitively against a set that holds one entry per
        // table written since the last build — a handful at most — rather than
        // lowercasing the name into a fresh `String` on every row. This runs
        // once per indexed row, so the allocation is the thing to avoid.
        if self
            .dirty_tables
            .iter()
            .any(|dirty| dirty.eq_ignore_ascii_case(table))
        {
            return;
        }
        self.dirty_tables.insert(table.to_ascii_lowercase());
    }

    /// Commit the retrieval indexes in `scope`, and report which ones.
    ///
    /// The one place index backends are committed, reached from the deferred
    /// path ([`Engine::refresh_indexes`]) and from the forced one
    /// ([`Engine::reindex`]) alike, so the two cannot come to mean different
    /// things.
    ///
    /// # Where a cancellation lands, and why it is safe here
    ///
    /// The check sits **between** backends and never inside one. What that
    /// buys is exact: at every point the engine can stop, every backend is
    /// either fully committed or has not been touched, and its table is still
    /// in `dirty_tables` — so the work is still pending and the next read does
    /// it, the same state a build that was never asked for leaves. Nothing can
    /// be caught half-built, which is what makes this cancellable where
    /// [`Engine::restore_indexes`] deliberately is not: that one runs with the
    /// indexes already cleared, so stopping it leaves a handle whose
    /// `bm25_score` silently answers nothing.
    ///
    /// What it does *not* buy is a stoppable single build. `VectorIndex::
    /// commit` is one opaque call, and on a corpus with one vector index that
    /// call is the whole four minutes. Pushing the check inside it would mean
    /// every backend promising to restore its pending set on the way out —
    /// `HnswIndex::build` moves each vector out of the row register as it
    /// inserts it, and `PagedHnswIndex::build` has already overwritten the
    /// stored graph by then — and a seam whose contract three of the four
    /// backends here cannot keep is worse than no seam.
    fn build_indexes(&mut self, scope: &Reindex) -> Result<Reindexed> {
        // The no-op, and the only place it is decided. Nothing has been
        // written since the last build, so every backend already describes
        // every committed row and committing them again would be work with no
        // outcome — including the storage commit and the index save below,
        // which are what would make an idle `REINDEX` in a cron job cost real
        // I/O for nothing.
        if self.dirty_tables.is_empty() {
            return Ok(Reindexed::default());
        }
        // A self-persisting index is told two things before it commits: which
        // write version its structure will describe, and whether it may make
        // its own writes durable. It may not inside a caller's transaction —
        // the caller's rows are buffered in the same one.
        let write_version = self.write_version;
        let may_commit = !self.in_transaction;
        let mut wrote_to_storage = false;
        let mut committed: Vec<(String, Vec<String>)> = Vec::new();
        // Borrowed as fields rather than through `&self`, which is what lets
        // them live across the `iter_mut()` loops below.
        let interrupt = &self.interrupt;
        let dirty = &self.dirty_tables;
        for (key, index) in self.text_indexes.iter_mut() {
            if !scope.covers(key) {
                continue;
            }
            interrupt.check_now()?;
            if index.is_self_persisting() {
                index.prepare_commit(write_version, may_commit);
                wrote_to_storage = true;
            }
            index.commit()?;
            // Every in-scope backend is committed, and that is deliberate: a
            // self-persisting one restamps itself here with the current write
            // version even when it had nothing pending, which is the
            // difference between the next open reading its graph back and
            // rebuilding it from every row. Only the ones whose *table* was
            // holding writes are reported, which is what makes a second
            // `REINDEX t` in a row say it built nothing rather than name an
            // index it knew was already current.
            if dirty.contains(&key.0) {
                committed.push(key.clone());
            }
        }
        for (key, index) in self.vector_indexes.iter_mut() {
            if !scope.covers(key) {
                continue;
            }
            interrupt.check_now()?;
            if index.is_self_persisting() {
                index.prepare_commit(write_version, may_commit);
                wrote_to_storage = true;
            }
            index.commit()?;
            if dirty.contains(&key.0) {
                committed.push(key.clone());
            }
        }
        // A table stops being dirty only once *every* backend it has was
        // committed. `REINDEX <index>` on a table with two retrieval indexes
        // covers one of them, and marking the table clean there would tell the
        // next read the other one was current.
        self.clear_dirty_tables_covered_by(scope);

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
        //
        // And skipped after a narrowed build, which is not a policy choice but
        // a correctness one: `persist_indexes` stamps *every* blob with the
        // current write version, so saving while another table's index is
        // still dirty would write a blob claiming to describe rows it has
        // never seen — and the next open would believe it.
        if !self.in_transaction
            && self.dirty_tables.is_empty()
            && self.write_version.saturating_sub(self.persisted_version) >= INDEX_PERSIST_INTERVAL
        {
            self.persist_indexes()?;
        }
        Ok(Reindexed {
            indexes: self.index_names_for(&committed),
        })
    }

    /// Drop from `dirty_tables` every table all of whose live retrieval
    /// backends `scope` covered.
    fn clear_dirty_tables_covered_by(&mut self, scope: &Reindex) {
        let fully_covered = |engine: &Engine, table: &str| -> bool {
            engine
                .text_indexes
                .keys()
                .chain(engine.vector_indexes.keys())
                .filter(|key| key.0 == table)
                .all(|key| scope.covers(key))
        };
        let done: Vec<String> = self
            .dirty_tables
            .iter()
            .filter(|table| fully_covered(self, table))
            .cloned()
            .collect();
        for table in done {
            self.dirty_tables.remove(&table);
        }
    }

    /// The catalog names of the backends under `keys`.
    ///
    /// Derived from the catalog rather than carried out of the loop, because a
    /// backend has no name — the maps are keyed by table and column list, and
    /// the name is what a person asked for and what a report has to give back.
    fn index_names_for(&self, keys: &[(String, Vec<String>)]) -> Vec<String> {
        if keys.is_empty() {
            return Vec::new();
        }
        self.catalog
            .indexes()
            .filter(|index| index.kind.is_retrieval())
            .filter(|index| keys.contains(&retrieval_key(&index.table, &index.columns)))
            .map(|index| index.name.clone())
            .collect()
    }

    /// Run a planned `ANALYZE`, replacing the selected tables' derived stats.
    fn run_analyze(&mut self, plan: &AnalyzePlan) -> Result<Outcome> {
        if self.in_transaction {
            return Err(Error::Transaction(
                "ANALYZE cannot run inside a transaction; analyze committed rows after \
                 committing or rolling back"
                    .to_string(),
            ));
        }
        let mut stats = if self.planner_stats.is_current(self.write_version) {
            self.planner_stats.clone()
        } else {
            PlannerStats::empty(self.write_version)
        };
        for name in &plan.tables {
            let table = self.catalog.require_table(name)?.clone();
            let indexes = self.catalog.indexes_for(&table.name);
            let table_stats =
                planner::collect_table(&self.storage, &table, &indexes, &self.interrupt)?;
            stats
                .tables
                .insert(table.name.to_ascii_lowercase(), table_stats);
        }
        stats.data_version = self.write_version;
        stats.stamp_catalog(&self.catalog, self.schema_version);
        self.storage.put_meta(STATS_META_KEY, &stats.encode())?;
        self.end_write()?;
        self.planner_stats = stats;
        Ok(Outcome::Ddl)
    }

    /// Run the deferred index build now, rather than leaving it to whichever
    /// read arrives first.
    ///
    /// This is the embedded half of the `REINDEX` statement — both go through
    /// [`Engine::build_indexes`], so there is one build here, not two. It
    /// changes no default: nothing about [`Engine::refresh_indexes`]'s
    /// deferral moves, so a bulk loader that never queries still never pays
    /// for a build it did not ask for.
    ///
    /// `table` narrows it to one table's indexes; `None` covers every index
    /// this handle holds. A build with nothing pending is a **no-op** that
    /// reports an empty [`Reindexed`] — it does not re-derive an index that is
    /// already current, and there is no spelling here that would make it,
    /// because that work has no correct outcome different from doing nothing.
    ///
    /// Allowed inside a transaction, as SQLite's `REINDEX` is, with the one
    /// consequence [`Engine::refresh_indexes`] already documents: the indexes
    /// are not *saved* until the transaction ends, because saving commits.
    pub fn reindex(&mut self, table: Option<&str>) -> Result<Reindexed> {
        self.refresh_snapshot()?;
        // The same arming every statement gets, in the same place, so a host
        // that installed a deadline covers this call too — it is exactly the
        // call a host is most likely to want to put one on.
        self.statement_clock.begin_statement();
        self.interrupt.begin_statement();
        let scope = match table {
            None => Reindex::Everything,
            Some(name) => {
                self.catalog.require_table(name)?;
                Reindex::Table(name.to_ascii_lowercase())
            }
        };
        self.build_indexes(&scope)
    }

    /// The `REINDEX` statement, resolved.
    fn run_reindex(&mut self, plan: &ReindexPlan) -> Result<Outcome> {
        let scope = match &plan.index {
            Some(name) => match self
                .catalog
                .indexes()
                .find(|index| index.name.eq_ignore_ascii_case(name))
            {
                Some(index) => Reindex::Index(retrieval_key(&index.table, &index.columns)),
                // Planned against a catalog this handle has since moved past.
                // `Plan::tables` is empty for `REINDEX`, so the staleness check
                // every other plan gets does not cover this one.
                None => {
                    return Err(Error::Catalog(alloc::format!(
                        "no such index: {name}; the catalog moved since this statement was \
                         prepared"
                    )))
                }
            },
            None => Reindex::Tables(plan.tables.clone()),
        };
        self.build_indexes(&scope)?;
        Ok(Outcome::Ddl)
    }

    // --------------------------------------------------------------- INSERT

    fn insert(&mut self, insert: &InsertPlan, params: &[Value]) -> Result<Outcome> {
        let (written, returned) = self.insert_uncommitted(insert, params)?;
        self.end_write()?;
        match &insert.returning {
            Some(items) => Ok(Outcome::Rows(ResultSet {
                columns: items.iter().map(|item| item.label().to_string()).collect(),
                rows: returned,
            })),
            None => Ok(Outcome::Written(written)),
        }
    }

    /// Everything `INSERT` does short of committing: proposes, checks
    /// constraints, resolves conflicts and writes every row. Split out so
    /// `CREATE TABLE ... AS SELECT` can populate its new table inside the
    /// same commit that creates it, rather than in a second one a crash
    /// between the two could turn into a table with no rows.
    fn insert_uncommitted(
        &mut self,
        insert: &InsertPlan,
        params: &[Value],
    ) -> Result<(usize, Vec<Vec<Value>>)> {
        let table = self.catalog.require_table(&insert.table)?.clone();
        if table.without_rowid {
            return self.insert_uncommitted_without_rowid(insert, &table, params);
        }
        let rules = self.rules_for(&table)?;
        let alias = table.rowid_alias();
        let env = self.env(params);

        // Every row is built before any is written, so an expression that
        // fails — a bad `?`, a vector of the wrong dimension — cannot leave
        // half a statement behind.
        let proposed = self.proposed_rows(insert, &table, &rules, params, &env)?;

        // The indexes this statement maintains, for the same reason `table`
        // above is a clone taken once: no DDL can interleave with the loop
        // below, so the set every row writes into is the set resolved here.
        let indexes = RowIndexes::resolve(&self.catalog, &table.name);
        // Likewise the column types the row encoder reads, and the buffer it
        // encodes into — one allocation for the statement rather than one
        // grown from empty per row. `encoded` is only ever read back on the
        // line that fills it.
        let mut encoder = RowEncoder::for_table(&table);

        let mut written = 0usize;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        for mut row in proposed {
            // Nothing here is durable yet — a cancelled `INSERT` leaves through
            // `discard_failed_statement` with its buffered rows dropped, the
            // same path a `CHECK` violation on the sixth row of six leaves
            // through.
            self.interrupt.check()?;
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
                            self.remove_btree_entries(
                                &table,
                                &indexes,
                                conflict.id,
                                &conflict.values,
                            )?;
                            self.storage.delete_row(&table.name, conflict.id)?;
                            self.deindex_row_retrieval(
                                &table,
                                &indexes,
                                conflict.id,
                                &conflict.values,
                            )?;
                            if !table.temporary {
                                self.note_change(&table.name, conflict.id, ChangeKind::Delete);
                            }
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
                            let id = self.write_changed_row(
                                &table,
                                &indexes,
                                &mut encoder,
                                existing,
                                &old,
                                next,
                            )?;
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
                .put_row(&table.name, id, encoder.encode(&row))?;
            // Only after the row is in the transaction, and only when the key
            // came from the counter: a caller reading this back is asking what
            // key it did not supply, and a row that failed to be written has no
            // key to report.
            if assigned {
                self.last_insert_row_id = Some(id);
            }
            self.index_row(&table, &indexes, id, &row)?;
            if !table.temporary {
                self.note_change(&table.name, id, ChangeKind::Insert);
            }
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

        Ok((written, returned))
    }

    /// `INSERT` into a `WITHOUT ROWID` table.
    ///
    /// The primary key's own encoded bytes are the storage key
    /// (`storage::primary_key_bytes`), so there is no row id counter to
    /// consult here and no secondary index to maintain — `sql.rs` already
    /// refuses both a non-key `UNIQUE` constraint and `CREATE INDEX` on one
    /// of these tables, for exactly this reason. That also makes conflict
    /// resolution simpler than [`Engine::insert_uncommitted`]'s general
    /// path: the primary key is the *only* constraint that can collide, so
    /// there is no second target an `ON CONFLICT (columns)` clause could be
    /// naming instead, and no `Conflict` list to search.
    ///
    /// Two gaps, both disclosed rather than silent: `ON CONFLICT DO UPDATE`
    /// is refused outright (the general path's upsert machinery is
    /// `RowId`-keyed throughout, not yet worth duplicating for this), and
    /// a written row does not reach the CDC change log
    /// ([`Engine::note_change`] is `RowId`-keyed too) or retrieval-index
    /// maintenance (moot — this table cannot have one).
    fn insert_uncommitted_without_rowid(
        &mut self,
        insert: &InsertPlan,
        table: &Table,
        params: &[Value],
    ) -> Result<(usize, Vec<Vec<Value>>)> {
        let rules = self.rules_for(table)?;
        let env = self.env(params);
        let pk_ordinals: Vec<usize> = table
            .primary_key
            .iter()
            .map(|column| table.require_column(column).map(|(ordinal, _)| ordinal))
            .collect::<Result<_>>()?;
        let pk_collations: Vec<Collation> = pk_ordinals
            .iter()
            .map(|&ordinal| table.columns[ordinal].collation)
            .collect();

        let proposed = self.proposed_rows(insert, table, &rules, params, &env)?;
        let mut written = 0usize;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        for mut row in proposed {
            self.interrupt.check()?;
            if !self.apply_constraints(table, &rules, &mut row, &insert.on_conflict, &env)? {
                continue;
            }
            let key_values: Vec<&Value> =
                pk_ordinals.iter().map(|&ordinal| &row[ordinal]).collect();
            let key = crate::storage::primary_key_bytes(&key_values, &pk_collations)?;
            if self.storage.get_row_keyed(&table.name, &key)?.is_some() {
                // A target that does not name the primary key cannot be what
                // answers this conflict — there is nothing else here for it
                // to name — so it falls through to the same plain refusal an
                // unanswered conflict gets on the general path.
                let answered = match &insert.on_conflict.target {
                    None => true,
                    Some(target) => same_columns(&pk_ordinals, target),
                };
                if !answered {
                    return Err(conflict_error(
                        table,
                        &Conflict {
                            id: 0,
                            values: Vec::new(),
                            columns: pk_ordinals,
                        },
                    ));
                }
                match &insert.on_conflict.action {
                    ConflictAction::Abort => {
                        return Err(conflict_error(
                            table,
                            &Conflict {
                                id: 0,
                                values: Vec::new(),
                                columns: pk_ordinals,
                            },
                        ))
                    }
                    ConflictAction::Ignore => continue,
                    ConflictAction::Replace => {
                        self.storage.delete_row_keyed(&table.name, &key)?;
                    }
                    ConflictAction::Update(_) => {
                        return Err(Error::Unsupported(
                            "ON CONFLICT DO UPDATE on a WITHOUT ROWID table is not supported yet"
                                .to_string(),
                        ));
                    }
                }
            }
            self.storage
                .put_row_keyed(&table.name, &key, &encode_table_row(table, &row))?;
            written += 1;
            if let Some(items) = &insert.returning {
                let exec = ExecRow {
                    id: 0,
                    score: None,
                    values: row,
                    aggregates: Vec::new(),
                    windows: Vec::new(),
                };
                returned.push(project_row(items, &exec, &env)?);
            }
        }
        Ok((written, returned))
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
            sql::coerce(value, &table.columns[ordinal], table.strict)
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
                            Some(value) => {
                                sql::coerce(value, &table.columns[ordinal], table.strict)?
                            }
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
                table.strict,
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
    ///
    /// `encoder` is the statement's [`RowEncoder`]: the column types and the
    /// buffer the row is encoded into, built once per statement rather than
    /// once per row — the hoist the `INSERT` loop got in AHL-517/518, which
    /// `UPDATE` and `ON CONFLICT DO UPDATE` reach through here (AHL-545).
    fn write_changed_row(
        &mut self,
        table: &Table,
        indexes: &RowIndexes,
        encoder: &mut RowEncoder,
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
        self.remove_btree_entries(table, indexes, id, old)?;
        if moved != id {
            self.storage.delete_row(&table.name, id)?;
        }
        self.storage
            .put_row(&table.name, moved, encoder.encode(&next))?;
        self.write_btree_entries(table, indexes, moved, &next)?;

        // Then the retrieval backends, which may commit whatever is buffered.
        self.deindex_row_retrieval(table, indexes, id, old)?;
        self.index_row_retrieval(table, indexes, moved, &next)?;

        if moved != id {
            if !table.temporary {
                self.note_change(&table.name, id, ChangeKind::Delete);
            }
            self.reserve_row_id(moved);
        }
        if !table.temporary {
            self.note_change(&table.name, moved, ChangeKind::Update);
        }
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
                        table.strict,
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
        first_batch: Option<usize>,
    ) -> Result<RowBytes<'_>> {
        if let Some(id) = pinned_rowid(table, filter.as_ref(), params) {
            return Ok(RowBytes::Point(
                self.storage
                    .get_row(&table.name, id)?
                    .map(|bytes| (id, bytes)),
            ));
        }
        if let Some(ids) = self.indexed_candidates(table, filter.as_ref(), params)? {
            return Ok(RowBytes::indexed(
                &self.storage,
                &table.name,
                ids,
                &self.interrupt,
            ));
        }
        let scan = self.scan(&table.name);
        Ok(RowBytes::Scan(match first_batch {
            Some(rows) => scan.with_first_batch(rows),
            None => scan,
        }))
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
        self.candidate_bytes(table, filter, params, None)?.collect()
    }

    /// Which scalar B-tree index answers a filter, and over what range — or
    /// `None` when no index applies and the caller has to scan.
    ///
    /// This is a **rule, not a cost model** (`docs/architecture.md`, D6): the most
    /// constrained applicable index wins, and if that turns out to be a bad
    /// choice it is still a correct one, because the caller re-evaluates the
    /// whole `WHERE` over every row it reads. An index here can only
    /// change *how many rows are read*, never which ones match — the same
    /// contract [`pinned_rowid`] has always had.
    ///
    /// Split out of [`Engine::indexed_candidates`] so that
    /// [`crate::explain`] reports the index this returns rather than deciding
    /// again from the same inputs. Two implementations of one rule would
    /// drift, and the way they would drift is the worst one available: an
    /// `EXPLAIN` claiming an index for a query that actually scans.
    pub(crate) fn choose_index<'a>(
        &'a self,
        table: &Table,
        filter: Option<&crate::plan::Expr>,
        params: &[Value],
    ) -> Result<Option<(&'a Index, IndexRange)>> {
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

        let mut best: Option<(&Index, IndexRange)> = None;
        for index in candidates {
            let Some(probe) = index_probe(table, index, &terms)? else {
                continue;
            };
            // More bound columns is a narrower scan; ties keep the first,
            // which is index-name order and therefore deterministic.
            if best
                .as_ref()
                .is_none_or(|(_, best)| probe.bound() > best.bound())
            {
                best = Some((index, probe));
            }
        }
        Ok(best)
    }

    /// The rows [`Engine::choose_index`]'s range names, in row-id order.
    fn indexed_candidates(
        &self,
        table: &Table,
        filter: Option<&crate::plan::Expr>,
        params: &[Value],
    ) -> Result<Option<Vec<RowId>>> {
        let Some((_, probe)) = self.choose_index(table, filter, params)? else {
            return Ok(None);
        };
        let range = probe.range;

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
                // Row ids straight off the index leaf, the way `indexed_candidates`
                // and the join probe already read them (AHL-479): the same
                // range, answered from the retained range cursor when it
                // covers it, with no owned `Vec<u8>` per matching key and no
                // resolution of the index's always-empty value. This runs
                // once per written row per `UNIQUE` group, so it was the
                // write path's one remaining decoded-entry walk.
                let ids = self
                    .storage
                    .scan_index_row_ids(&range.start, range.end.as_deref())?;
                let mut rows = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some(bytes) = self.storage.get_row(&table.name, id)? {
                        rows.push((id, bytes));
                    }
                }
                rows
            }
            // No index covers this group, so the only way to know is to look
            // at every row. This is the O(rows)-per-write cost the constraint
            // used to have unconditionally.
            None => self.scan_all(&table.name)?,
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
    ///
    /// `indexes` is the statement's own [`RowIndexes`], resolved once for
    /// every row it writes.
    fn index_row(
        &mut self,
        table: &Table,
        indexes: &RowIndexes,
        id: RowId,
        row: &[Value],
    ) -> Result<()> {
        self.write_btree_entries(table, indexes, id, row)?;
        self.index_row_retrieval(table, indexes, id, row)
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
    fn write_btree_entries(
        &mut self,
        table: &Table,
        indexes: &RowIndexes,
        id: RowId,
        row: &[Value],
    ) -> Result<()> {
        for key in btree_entry_keys(table, indexes, id, row)? {
            self.storage.put_index_entry(&key)?;
        }
        Ok(())
    }

    /// Remove this row's B-tree entries, and nothing else. See
    /// [`Engine::write_btree_entries`] for why it is separable.
    fn remove_btree_entries(
        &mut self,
        table: &Table,
        indexes: &RowIndexes,
        id: RowId,
        row: &[Value],
    ) -> Result<()> {
        for key in btree_entry_keys(table, indexes, id, row)? {
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
    fn index_row_retrieval(
        &mut self,
        table: &Table,
        indexes: &RowIndexes,
        id: RowId,
        row: &[Value],
    ) -> Result<()> {
        self.mark_indexes_dirty(&table.name);
        for index in &indexes.retrieval {
            self.index_row_for_index(table, index, id, row)?;
        }
        Ok(())
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
    fn deindex_row_retrieval(
        &mut self,
        table: &Table,
        indexes: &RowIndexes,
        id: RowId,
        row: &[Value],
    ) -> Result<()> {
        self.mark_indexes_dirty(&table.name);
        for index in &indexes.retrieval {
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
        if table.without_rowid {
            return self.update_without_rowid(plan, &table, params);
        }
        let rules = self.rules_for(&table)?;
        let env = self.env(params);
        let indexes = RowIndexes::resolve(&self.catalog, &table.name);
        let mut encoder = RowEncoder::for_table(&table);
        let mut count = 0;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        for (id, bytes) in self.candidate_rows(&table, &plan.filter, params)? {
            // The candidates were read by a checked scan, but that finished
            // before this loop started: an `UPDATE` over a million rows spends
            // almost all of its time here, re-checking constraints and writing.
            self.interrupt.check()?;
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
                    table.strict,
                )?;
                next[*index] = value;
            }
            self.apply_constraints(&table, &rules, &mut next, &OnConflict::abort(), &env)?;
            // A `UNIQUE` constraint has to be re-checked against every *other*
            // row, which is the same O(rows) scan an `INSERT` pays and for the
            // same reason.
            self.ensure_unique(&table, &rules, id, &next)?;
            let id = self.write_changed_row(&table, &indexes, &mut encoder, id, &row, next)?;
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

    /// `UPDATE` on a `WITHOUT ROWID` table.
    ///
    /// The primary key is the storage key, so an assignment that changes
    /// one of its columns moves the row — delete the old key, write the
    /// new one — the same rule `Engine::write_changed_row` already applies
    /// when an ordinary table's `INTEGER PRIMARY KEY` is the target of its
    /// own `UPDATE`, generalised from one column to however many the key
    /// has. `RowId`-keyed machinery (`ensure_unique`, `write_changed_row`,
    /// retrieval-index and CDC upkeep) is not reused for the same reasons
    /// [`Engine::insert_uncommitted_without_rowid`] does not reuse it.
    fn update_without_rowid(
        &mut self,
        plan: &UpdatePlan,
        table: &Table,
        params: &[Value],
    ) -> Result<Outcome> {
        let rules = self.rules_for(table)?;
        let env = self.env(params);
        let pk_ordinals: Vec<usize> = table
            .primary_key
            .iter()
            .map(|column| table.require_column(column).map(|(ordinal, _)| ordinal))
            .collect::<Result<_>>()?;
        let pk_collations: Vec<Collation> = pk_ordinals
            .iter()
            .map(|&ordinal| table.columns[ordinal].collation)
            .collect();
        // Read up front, exactly as `candidate_rows` does for the ordinary
        // path: the statement sees the table as it was when it started, not
        // as this loop is changing it.
        let candidates = crate::traits::scan_all_keyed(&self.storage, &table.name)?;

        let mut count = 0;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        let mut encoder = RowEncoder::for_table(table);
        for (old_key, bytes) in candidates {
            self.interrupt.check()?;
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
                    table.strict,
                )?;
                next[*index] = value;
            }
            self.apply_constraints(table, &rules, &mut next, &OnConflict::abort(), &env)?;
            let key_values: Vec<&Value> =
                pk_ordinals.iter().map(|&ordinal| &next[ordinal]).collect();
            let new_key = crate::storage::primary_key_bytes(&key_values, &pk_collations)?;
            if new_key != old_key {
                if self.storage.get_row_keyed(&table.name, &new_key)?.is_some() {
                    return Err(conflict_error(
                        table,
                        &Conflict {
                            id: 0,
                            values: Vec::new(),
                            columns: pk_ordinals,
                        },
                    ));
                }
                self.storage.delete_row_keyed(&table.name, &old_key)?;
            }
            self.storage
                .put_row_keyed(&table.name, &new_key, encoder.encode(&next))?;
            count += 1;
            if let Some(items) = &plan.returning {
                let exec = ExecRow {
                    id: 0,
                    score: None,
                    values: next,
                    aggregates: Vec::new(),
                    windows: Vec::new(),
                };
                returned.push(project_row(items, &exec, &env)?);
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
        if table.without_rowid {
            return self.delete_without_rowid(plan, &table, params);
        }
        let env = self.env(params);
        let indexes = RowIndexes::resolve(&self.catalog, &table.name);
        let mut count = 0;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        for (id, bytes) in self.candidate_rows(&table, &plan.filter, params)? {
            self.interrupt.check()?;
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
            self.remove_btree_entries(&table, &indexes, id, &row)?;
            self.storage.delete_row(&table.name, id)?;
            self.deindex_row_retrieval(&table, &indexes, id, &row)?;
            if !table.temporary {
                self.note_change(&table.name, id, ChangeKind::Delete);
            }
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

    /// `DELETE` on a `WITHOUT ROWID` table: the primary key bytes decoded
    /// straight back out of the matched row are the storage key, so there
    /// is nothing else to look up. Not yet reaching retrieval-index or CDC
    /// upkeep, for the same disclosed reasons
    /// [`Engine::insert_uncommitted_without_rowid`] does not.
    fn delete_without_rowid(
        &mut self,
        plan: &DeletePlan,
        table: &Table,
        params: &[Value],
    ) -> Result<Outcome> {
        let env = self.env(params);
        let candidates = crate::traits::scan_all_keyed(&self.storage, &table.name)?;
        let mut count = 0;
        let mut returned: Vec<Vec<Value>> = Vec::new();
        for (key, bytes) in candidates {
            self.interrupt.check()?;
            let row = decode_row(&bytes)?;
            if !self.matches(&plan.filter, &row, &env)? {
                continue;
            }
            if let Some(items) = &plan.returning {
                let exec = ExecRow {
                    id: 0,
                    score: None,
                    values: row.clone(),
                    aggregates: Vec::new(),
                    windows: Vec::new(),
                };
                returned.push(project_row(items, &exec, &env)?);
            }
            self.storage.delete_row_keyed(&table.name, &key)?;
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
            SubqueryBody::Recursive(plan) => self.select_recursive(plan, params),
            SubqueryBody::RecursiveSelf(_) => unreachable!(
                "a recursive CTE's self-reference only ever appears inside a FromItem, never as \
                 a top-level query body"
            ),
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

    /// Evaluate a `WITH RECURSIVE` CTE run as a top-level query, e.g.
    /// `INSERT INTO t SELECT * FROM (WITH RECURSIVE cnt(x) AS (...) SELECT x
    /// FROM cnt)`. The ordinary `FROM`-reference path is
    /// [`Engine::run_recursive`] by way of [`Engine::run_body`]; this exists
    /// only because [`Engine::select_body`] has to answer every
    /// [`SubqueryBody`] shape.
    fn select_recursive(&mut self, plan: &RecursivePlan, params: &[Value]) -> Result<ResultSet> {
        self.refresh_indexes()?;
        let env = self.read_env(params);
        let columns = plan.seed.labels().into_iter().map(str::to_string).collect();
        let rows = self.run_recursive(plan, &env, None)?;
        Ok(ResultSet { columns, rows })
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
        self.run_select_to(plan, env, cap, None)
    }

    /// [`Engine::run_select`] with an optional row consumer.
    ///
    /// A consumer changes ownership, not SQL semantics: non-blocking queries
    /// project each row into one reusable scratch buffer, while blocking
    /// queries still materialise for their sort/aggregate and then visit the
    /// final rows. The empty `rows` in the returned set are an internal signal
    /// only; public callback APIs return the counted rows instead.
    fn run_select_to(
        &self,
        plan: &SelectPlan,
        env: &Env<'_>,
        cap: Option<usize>,
        mut sink: Option<&mut RowSink<'_>>,
    ) -> Result<ResultSet> {
        let ScanShape {
            limit,
            offset,
            fetch,
            stop_after,
            full_scan,
            reorderable,
        } = scan_shape(plan, env, cap)?;

        // Join *ordering*, the one choice the cost model was never allowed to
        // make. The rewrite produces the plan the same query written the other
        // way round would have produced, so what runs afterwards is a shape
        // this engine already executes — see `should_swap_leading_join`.
        //
        // The clone is paid only by a two-table inner join whose stats say the
        // written order is the worse one; `can_swap_leading_join` is checked
        // first and costs nothing.
        // The cost is asked with `stop_after`, not `fetch`: under an `ORDER
        // BY` the `LIMIT` truncates the sorted answer rather than ending the
        // scan, so the outer side is read in full whatever the `LIMIT` says.
        let swapped;
        let plan = if reorderable && self.should_swap_leading_join(plan, stop_after, env.params()) {
            swapped = {
                let mut candidate = plan.clone();
                candidate.swap_leading_join();
                candidate
            };
            &swapped
        } else {
            plan
        };

        let driving = &plan.from[0];
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();

        // The `MIN`/`MAX` optimisation: a scalar `MIN`/`MAX` over the rowid
        // or an indexed leading column answers from one tree descent per
        // aggregate, with no row scanned at all. See
        // `Engine::try_min_max_scalar`'s doc for exactly which statements
        // qualify; everything else falls through to the ordinary pipeline
        // below unchanged.
        if is_aggregate {
            if let Some(rows) = self.try_min_max_scalar(plan)? {
                return self.finish_blocking(plan, rows, env, offset, limit, sink);
            }
        }

        let outer_rows = self.estimated_outer_rows(plan, fetch, env.params());

        // Which columns any of this can observe. Everything else is walked past
        // rather than turned into a `String` or a `Vec` on the heap.
        let mask = needed_columns(plan);
        let driving_mask = mask.slice(0, driving.table.columns.len());

        let non_blocking = !is_aggregate
            && plan.windows.is_empty()
            && !plan.distinct
            && plan.order.is_empty()
            && plan.score.is_none();

        // Which rows we even look at: retrieval when the query asked for it,
        // otherwise a point lookup or a scan depending on what the filter pins
        // down. A filter on a single-table retrieval query is pushed into the
        // fetch — see [`Engine::retrieve_filtered`] — because a fixed candidate
        // budget filtered afterwards under-fills a restrictive `WHERE`.
        let params = env.params();

        // How many driving rows the first scan batch should hold. `stop_after`
        // is the most rows the pipeline will pull before `LIMIT` ends it, and
        // without a `WHERE` every driving row reaches the consumer, so a
        // first batch that size reads exactly what a `LIMIT 10` needs instead
        // of `FIRST_SCAN_BATCH` rows it then drops. Under a filter the rows
        // needed is unknown and the default batch stands.
        let first_batch = if plan.filter.is_none() {
            stop_after
        } else {
            None
        };

        // One ordinary join can stay borrowed all the way into a row callback:
        // no iterator item ever has to own the joined row, and one projection
        // buffer serves the complete result. Multi-join and residual-filter
        // shapes keep the general iterator pipeline until their state machines
        // can make the same ownership guarantee.
        if non_blocking
            && plan.joins.len() == 1
            && plan.from.len() == 2
            && plan.filter.is_none()
            && driving.derived.is_none()
            && plan.from[1].derived.is_none()
        {
            if let Some(sink) = sink.take() {
                return self.run_single_join_to(
                    plan,
                    env,
                    &mask,
                    &driving_mask,
                    full_scan,
                    outer_rows,
                    offset,
                    limit,
                    sink,
                );
            }
        }

        // A streamed aggregate over one stored table folds straight from the
        // row bytes: each row is decoded into one borrowed buffer the fold
        // reuses, and only a row that opens a group is ever materialised. The
        // other sources — derived, scored, `WITHOUT ROWID`, joined — hand
        // over decoded rows and take the general stream below.
        if plan.joins.is_empty()
            && driving.derived.is_none()
            && plan.score.is_none()
            && !driving.table.without_rowid
            && self.can_stream_aggregate(plan)
        {
            let source = self.candidate_bytes(&driving.table, &plan.filter, params, None)?;
            let rows = self.stream_aggregate(
                plan,
                AggregateInput::Bytes {
                    source,
                    mask: &driving_mask,
                    filter: plan.filter.as_ref(),
                },
                env,
            )?;
            return self.finish_blocking(plan, rows, env, offset, limit, sink);
        }

        let mut stream: RowStream<'_> = if plan.joins.is_empty() {
            match (&driving.derived, &plan.score, &plan.filter) {
                // A derived table has no storage to stream from, so it
                // materialises in full before the outer pipeline starts. That
                // cost is real and is not hidden: `FROM (SELECT ...)` builds
                // the whole inner result, and a `LIMIT` on the outer query does
                // not shorten it. Pushing the outer limit inward is a planner
                // rewrite, not something this loop can do — except for a
                // recursive CTE with no filter on top of it, where the
                // alternative is running an unguarded recursion to
                // completion; see `Engine::derived_stream`'s doc.
                (Some(body), _, filter) => {
                    let recursive_cap = if filter.is_none()
                        && matches!(body.as_ref(), SubqueryBody::Recursive(_))
                    {
                        stop_after
                    } else {
                        None
                    };
                    let base = self.derived_stream(body, env, recursive_cap)?;
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
                // `WITHOUT ROWID`: `sql.rs`'s `resolve_from` already refused
                // any join for one of these, so `plan.joins.is_empty()`
                // holds and this is the whole query's only source.
                (None, None, _) if driving.table.without_rowid => {
                    let base = self.without_rowid_stream(&driving.table.name)?;
                    match &plan.filter {
                        Some(filter) => Box::new(Filter::new(base, filter, env)),
                        None => base,
                    }
                }
                (None, None, _) => {
                    let source =
                        self.candidate_bytes(&driving.table, &plan.filter, params, first_batch)?;
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
                (Some(body), _) => self.derived_stream(body, env, None)?,
                (None, Some(score)) => Box::new(
                    self.retrieve_rows(&driving.table, score, fetch, &driving_mask, env)?
                        .into_iter()
                        .map(Ok),
                ),
                (None, None) => Box::new(Decode::new(
                    self.candidate_bytes(&driving.table, &plan.filter, params, first_batch)?,
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
                    None => self.join_inner(
                        &plan.from,
                        index + 1,
                        offset_of,
                        join.on.as_ref(),
                        &mask,
                        full_scan,
                        if index == 0 { outer_rows } else { None },
                    )?,
                };
                offset_of += width;
                stream = Box::new(NestedLoopJoin::new(
                    stream,
                    side,
                    join.kind,
                    join.on.as_ref(),
                    env,
                    &self.interrupt,
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

        // A query with none of the blocking operators — aggregate, window,
        // `DISTINCT`, `ORDER BY` — can be projected straight out of the stream,
        // skipping the intermediate `Vec<ExecRow>` the blocking path needs in
        // order to sort and fold. The stream already yields rows in row-id
        // order, so `sort_rows` with an empty `ORDER BY` (a stable re-sort of
        // that same order) is a no-op: skipping it changes only the
        // allocations, not the answer.
        if non_blocking {
            // `stop_after` already bounded the scan to `limit + offset`; skip
            // the offset and take the limit to finish the page.
            if offset > 0 {
                stream = Box::new(stream.skip(offset));
            }
            if let Some(limit) = limit {
                stream = Box::new(stream.take(limit));
            }
            return match sink.as_mut() {
                Some(sink) => {
                    project_stream_to(&plan.items, stream, env, *sink)?;
                    Ok(ResultSet {
                        columns: plan
                            .items
                            .iter()
                            .map(|item| item.label().to_string())
                            .collect(),
                        rows: Vec::new(),
                    })
                }
                None => project_stream(&plan.items, stream, env, limit),
            };
        }

        // An ungrouped aggregate does not have to hold its input at all: its
        // answer is one row, and every function it can use folds from the
        // argument values alone. Streaming it skips materialising the whole
        // table into `ExecRow`s only to fold and drop them — measured at
        // 18.38 ms against a 10.28 ms scan-and-decode floor for
        // `SELECT ... FROM users`, so the holding was 44% of the query
        // (`PERF.md`, 2026-09-01).
        //
        // Everything else below still blocks, and `collect_bounded`'s
        // per-statement ceiling still applies to it.
        // Past this point the query is blocking: it has to hold every input row
        // before it can produce a single output row, so this is where one
        // statement can take the process down and where the per-statement
        // ceiling is applied. See [`collect_bounded`]. The streamed case above
        // is the exception, and everything after this point is shared by both.
        let rows: Vec<ExecRow> = if self.can_stream_aggregate(plan) {
            self.stream_aggregate(plan, AggregateInput::Rows(stream), env)?
        } else {
            let mut collected =
                collect_bounded(stream, self.options.query_memory_bytes, &self.interrupt)?;
            if is_aggregate {
                collected = self.aggregate(plan, collected, env)?;
            }
            collected
        };
        self.finish_blocking(plan, rows, env, offset, limit, sink)
    }

    /// Everything a blocking `SELECT` does once its input is held: windows,
    /// `DISTINCT`, `ORDER BY`, `OFFSET`/`LIMIT`, projection, and the sink.
    ///
    /// Shared by the two ways [`Engine::run_select_to`] arrives at held rows
    /// — the general stream collected or streamed-aggregated, and the
    /// bytes-fed aggregate — so the stages after the fold are one piece of
    /// code whichever produced them.
    fn finish_blocking(
        &self,
        plan: &SelectPlan,
        mut rows: Vec<ExecRow>,
        env: &Env<'_>,
        offset: usize,
        limit: Option<usize>,
        mut sink: Option<&mut RowSink<'_>>,
    ) -> Result<ResultSet> {
        // Window functions run over the rows a `GROUP BY` already folded (or
        // the plain joined rows, for a non-aggregate query) — after
        // `WHERE`/`GROUP BY`/`HAVING`, before `DISTINCT`/`ORDER BY`/`LIMIT`
        // (`docs/architecture.md` phase 1 item 6), so `SELECT DISTINCT` folds on a window
        // function's own output and `ORDER BY` may sort by one.
        if !plan.windows.is_empty() {
            rows = window(plan, rows, env, &self.interrupt)?;
        }

        // `DISTINCT` folds *projected* rows, not stored ones, and it happens
        // before `ORDER BY` so that the order applies to what survives.
        if plan.distinct {
            rows = distinct_rows(
                &plan.items,
                &plan.distinct_collations,
                rows,
                env,
                &self.interrupt,
            )?;
        }

        rows = sort_rows(rows, &plan.order, env, &self.interrupt)?;

        // `OFFSET` skips before `LIMIT` counts; an offset past the end leaves
        // nothing, which is not an error.
        if offset > 0 {
            rows.drain(..offset.min(rows.len()));
        }
        if let Some(limit) = limit {
            rows.truncate(limit);
        }

        let result = project(&plan.items, rows, env)?;
        if let Some(sink) = sink.as_mut() {
            for row in &result.rows {
                (*sink)(row)?;
            }
            return Ok(ResultSet {
                columns: result.columns,
                rows: Vec::new(),
            });
        }
        Ok(result)
    }

    /// Push one non-blocking stored-table join into a borrowed-row consumer.
    #[allow(clippy::too_many_arguments)]
    fn run_single_join_to(
        &self,
        plan: &SelectPlan,
        env: &Env<'_>,
        mask: &ColumnMask,
        driving_mask: &ColumnMask,
        full_scan: bool,
        outer_rows: Option<u64>,
        offset: usize,
        limit: Option<usize>,
        sink: &mut dyn FnMut(&[Value]) -> Result<()>,
    ) -> Result<ResultSet> {
        let columns = plan
            .items
            .iter()
            .map(|item| item.label().to_string())
            .collect();
        if limit == Some(0) {
            return Ok(ResultSet {
                columns,
                rows: Vec::new(),
            });
        }

        let driving = &plan.from[0];
        let join = &plan.joins[0];
        // No filter on this path, so the join consumes at most `limit + offset`
        // driving rows — see `first_batch` in `run_select_to`.
        let outer: RowStream<'_> = Box::new(Decode::new(
            self.candidate_bytes(
                &driving.table,
                &None,
                env.params(),
                limit.map(|limit| limit.saturating_add(offset)),
            )?,
            driving_mask,
        ));
        let mut side = self.join_inner(
            &plan.from,
            1,
            driving.table.columns.len(),
            join.on.as_ref(),
            mask,
            full_scan,
            outer_rows,
        )?;
        let hash_key_is_full_on = side.is_hash() && is_single_equality(join.on.as_ref());
        let mut skipped = 0usize;
        let mut emitted = 0usize;
        let mut projected = Vec::with_capacity(plan.items.len());
        let direct_projection = plan
            .items
            .iter()
            .all(|item| !matches!(item, SelectItem::Expr { .. }));

        // Selective late materialisation (SLM) on the outer side: when the
        // driving table's projected columns are just its row id, the outer scan
        // decodes only the join key — one bare `Value` per row, no `Vec<Value>`
        // container — and the row id is read off the scan, never decoded. This
        // is the outer half of `PERF.md`'s "materialisation point = projection";
        // the inner side stays the cached decoded table.
        if let JoinInner::Hash(hash) = &mut side {
            let outer_width = driving.table.columns.len();
            let rowid_only = driving.table.rowid_alias().is_some_and(|rowid| {
                plan.items.iter().all(|item| match item {
                    SelectItem::Column { index, .. } => *index >= outer_width || *index == rowid,
                    SelectItem::Score { .. } => true,
                    SelectItem::Expr { .. } => false,
                })
            });
            if hash_key_is_full_on
                && direct_projection
                && rowid_only
                && join.kind == JoinKind::Inner
            {
                let rowid = driving.table.rowid_alias().unwrap();
                let key_ordinal = hash.key_ordinal();
                for row in self.scan(&driving.table.name) {
                    let (row_id, row_bytes) = row?;
                    let key = decode_value_at(row_bytes.as_slice(), key_ordinal)?;
                    hash.prepare_key(&key);
                    for index in 0..hash.rows().len() {
                        if !hash.candidate_matches_key(index, &key) {
                            continue;
                        }
                        if skipped < offset {
                            skipped += 1;
                            continue;
                        }
                        projected.clear();
                        let inner = &hash.rows()[index];
                        for item in &plan.items {
                            match item {
                                SelectItem::Column { index: col, .. } if *col == rowid => {
                                    projected.push(Value::Integer(row_id as i64));
                                }
                                SelectItem::Column { index: col, .. } => projected.push(
                                    inner
                                        .get(*col - outer_width)
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                ),
                                SelectItem::Score { .. } => projected.push(Value::Null),
                                SelectItem::Expr { .. } => {
                                    unreachable!("direct_projection excludes expressions")
                                }
                            }
                        }
                        sink(&projected)?;
                        emitted += 1;
                        if limit.is_some_and(|limit| emitted >= limit) {
                            return Ok(ResultSet {
                                columns,
                                rows: Vec::new(),
                            });
                        }
                    }
                }
                return Ok(ResultSet {
                    columns,
                    rows: Vec::new(),
                });
            }
        }

        let joiner = NestedLoopJoin::new(
            outer,
            side,
            join.kind,
            join.on.as_ref(),
            env,
            &self.interrupt,
        );
        if hash_key_is_full_on && direct_projection {
            joiner.try_for_each_hash_pair(|_, score, outer, inner| {
                if skipped < offset {
                    skipped += 1;
                    return Ok(true);
                }
                project_split_row(&plan.items, outer, inner, score, &mut projected)?;
                sink(&projected)?;
                emitted += 1;
                Ok(limit.is_none_or(|limit| emitted < limit))
            })?;
        } else {
            joiner.try_for_each_borrowed(hash_key_is_full_on, |_, score, values| {
                if skipped < offset {
                    skipped += 1;
                    return Ok(true);
                }
                project_borrowed_row(&plan.items, values, score, env, &mut projected)?;
                sink(&projected)?;
                emitted += 1;
                Ok(limit.is_none_or(|limit| emitted < limit))
            })?;
        }
        Ok(ResultSet {
            columns,
            rows: Vec::new(),
        })
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
            SubqueryBody::Recursive(plan) => self.run_recursive(plan, env, cap),
            SubqueryBody::RecursiveSelf(_) => Ok(env
                .recursive_frontier()
                .expect(
                    "a RecursiveSelf reference only ever resolves inside \
                     Engine::run_recursive's loop, which always sets one first",
                )
                .to_vec()),
        }
    }

    /// Run a recursive CTE by semi-naive iteration: `seed` once, then
    /// `recursive` repeatedly, each time through [`Env::with_recursive_frontier`]
    /// seeing only the *previous step's newly produced rows* as
    /// [`SubqueryBody::RecursiveSelf`] — not the whole table accumulated so
    /// far — until a step adds nothing new.
    ///
    /// Verified against sqlite3 3.54, including the trap a naive
    /// implementation falls into: under `UNION` (not `UNION ALL`), a row
    /// that repeats one already produced is dropped from the *next* step's
    /// frontier too, not only from the final output. Without that, a
    /// recursive term whose output cycles (`(x + 1) % 3`, say) never
    /// converges — sqlite3 confirms the cycling case terminates, which is
    /// only possible if a repeated row stops propagating.
    ///
    /// `cap` ends the loop once at least that many rows exist in total,
    /// *before* filtering the last step against what came before — see
    /// `sql.rs`'s `run_select_to` call site, the only one that ever passes
    /// one: a bare `FROM recursive_cte LIMIT n` with nothing else on the
    /// outer query, where "the recursive evaluation produced n rows" and
    /// "the query answered n rows" are the same fact, so ending the loop
    /// there is exactly what a `LIMIT`-aware caller means and matches
    /// sqlite3's own short-circuit for this shape (confirmed: an unguarded
    /// `WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt)
    /// SELECT x FROM cnt LIMIT 5` returns at once there rather than running
    /// forever). Every other shape — a `WHERE`, a `JOIN`, an `ORDER BY` on
    /// the outer query — passes `None` and gets the whole table, the same
    /// policy an ordinary derived table already has (see
    /// [`Engine::derived_stream`]'s doc): unbounded, but not un-killable,
    /// since [`Interrupt::check`] is asked every step.
    fn run_recursive(
        &self,
        plan: &RecursivePlan,
        env: &Env<'_>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut all_rows = self.run_body(&plan.seed, env, None)?;
        if !plan.all {
            let keep = duplicate_keep_mask(&all_rows, &plan.collations, Keep::First);
            let mut kept = Vec::with_capacity(all_rows.len());
            for (row, keep) in all_rows.into_iter().zip(keep) {
                if keep {
                    kept.push(row);
                }
            }
            all_rows = kept;
        }
        let mut frontier = all_rows.clone();

        while !frontier.is_empty() {
            self.interrupt.check_now()?;
            if cap.is_some_and(|cap| all_rows.len() >= cap) {
                break;
            }
            let step_env = env.with_recursive_frontier(&frontier);
            let mut next = self.run_body(&plan.recursive, &step_env, None)?;
            if !plan.all {
                next.retain(|row| {
                    !all_rows.iter().any(|seen| {
                        compare_projections(seen, row, &plan.collations)
                            == core::cmp::Ordering::Equal
                    })
                });
                let keep = duplicate_keep_mask(&next, &plan.collations, Keep::First);
                let mut kept = Vec::with_capacity(next.len());
                for (row, keep) in next.into_iter().zip(keep) {
                    if keep {
                        kept.push(row);
                    }
                }
                next = kept;
            }
            all_rows.extend(next.iter().cloned());
            frontier = next;
        }
        if let Some(cap) = cap {
            all_rows.truncate(cap);
        }
        Ok(all_rows)
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
        rows = sort_rows(rows, &plan.order, env, &self.interrupt)?;

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
    ///
    /// `cap` is `None` for almost every caller: an ordinary derived table
    /// materialises in full before the outer pipeline starts, and a `LIMIT`
    /// on the outer query does not shorten it — that cost is real and is not
    /// hidden, since pushing the outer limit inward in general is a planner
    /// rewrite, not something this loop can do. The one caller that passes
    /// `Some` is `run_select_to`'s driving-side dispatch, and only for a
    /// [`SubqueryBody::Recursive`] body in the one shape where "the recursive
    /// evaluation produced `n` rows" and "the query answered `n` rows" are
    /// the same fact — see [`Engine::run_recursive`]'s doc for why that one
    /// case needs it to avoid running an unguarded recursion to completion
    /// under a `LIMIT` that was supposed to end it early.
    fn derived_stream<'a>(
        &self,
        body: &SubqueryBody,
        env: &Env<'_>,
        cap: Option<usize>,
    ) -> Result<RowStream<'a>> {
        let rows = self.run_body(body, env, cap)?;
        Ok(Box::new(
            rows.into_iter()
                .zip(1u64..)
                .map(|(values, id)| Ok(ExecRow::scanned(id, values))),
        ))
    }

    /// A `WITHOUT ROWID` table's rows, as a stream the outer pipeline can
    /// consume — the same materialise-then-stream treatment
    /// [`Engine::derived_stream`] gives a derived table, since this table's
    /// rows are not reachable through [`Engine::candidate_bytes`]'s row-id
    /// path at all. Every column is decoded, not only the ones the
    /// statement can observe (`ColumnMask` does not reach this path yet) —
    /// real, just not as tight as the ordinary scan.
    ///
    /// The row ids are positions, one-based, exactly as a derived table's
    /// are: there is no `SELECT rowid` on a `WITHOUT ROWID` table to
    /// answer, so nothing downstream needs them to mean anything beyond
    /// breaking ties.
    fn without_rowid_stream<'a>(&self, table: &str) -> Result<RowStream<'a>> {
        let rows = crate::traits::scan_all_keyed(&self.storage, table)?;
        let mut decoded = Vec::with_capacity(rows.len());
        for (_, bytes) in rows {
            decoded.push(decode_row(&bytes)?);
        }
        Ok(Box::new(
            decoded
                .into_iter()
                .zip(1u64..)
                .map(|(values, id)| Ok(ExecRow::scanned(id, values))),
        ))
    }

    /// Estimate how many rows the first join's driving side will produce.
    ///
    /// The estimate is deliberately an upper bound for a `LIMIT` and does not
    /// pretend to know filter selectivity. A derived source has no persisted
    /// table stats, and an incomplete/stale snapshot returns `None`, which
    /// keeps the existing shape rule in force.
    pub(crate) fn estimated_outer_rows(
        &self,
        plan: &SelectPlan,
        fetch: Option<usize>,
        params: &[Value],
    ) -> Option<u64> {
        let driving = plan.from.first()?;
        if driving.derived.is_some() || !self.planner_stats.is_current(self.write_version) {
            return None;
        }
        let table = self.planner_stats.table(&driving.table.name)?;
        let rows = if pinned_rowid(&driving.table, plan.filter.as_ref(), params).is_some() {
            1
        } else {
            table.row_count
        };
        Some(fetch.map_or(rows, |fetch| rows.min(fetch as u64)))
    }

    /// Whether a two-table inner join is cheaper with its sources exchanged.
    ///
    /// This is the one thing the cost model was never allowed to decide. Until
    /// now it chose *how* to join two tables — hash build or index probe — but
    /// never *which one drives*, so a join ran in written order whatever the
    /// cardinalities were. `BENCHMARK.md` measures what that costs: the same
    /// two tables, the same `ON`, written both ways round, come out ~7.5x
    /// faster one way and ~1.15x slower the other. Nothing chose between them
    /// and the faster one only won by being written that way.
    ///
    /// Both orientations are costed by the *same* function the path choice
    /// uses, rather than by a second cost model that could disagree with it.
    /// The swapped plan is built with [`SelectPlan::swap_leading_join`], so
    /// what is costed is exactly what would run.
    ///
    /// Ties keep the written order. A cost model that reorders on equal
    /// estimates would make the plan depend on estimation noise, and a query
    /// whose plan changes without its data changing is one nobody can reason
    /// about.
    pub(crate) fn should_swap_leading_join(
        &self,
        plan: &SelectPlan,
        fetch: Option<usize>,
        params: &[Value],
    ) -> bool {
        if !plan.can_swap_leading_join() || !self.planner_stats.is_current(self.write_version) {
            return false;
        }
        let Some(as_written) = self.leading_join_cost(plan, fetch, params) else {
            return false;
        };
        let mut swapped = plan.clone();
        swapped.swap_leading_join();
        let Some(other) = self.leading_join_cost(&swapped, fetch, params) else {
            return false;
        };
        other < as_written
    }

    /// The costed work units for `plan`'s leading join as written, or `None`
    /// when anything the estimate needs is missing.
    fn leading_join_cost(
        &self,
        plan: &SelectPlan,
        fetch: Option<usize>,
        params: &[Value],
    ) -> Option<u64> {
        let outer_rows = self.estimated_outer_rows(plan, fetch, params)?;
        let offset_of = plan.from[0].table.columns.len();
        let on = plan.joins[0].on.as_ref();
        let hash_available = hash_join_key(&plan.from, 1, offset_of, on).is_some();
        let probe = self.join_probe(&plan.from[1].table, offset_of, on);
        self.costed_join_decision(
            &plan.from,
            1,
            Some(outer_rows),
            hash_available,
            probe.as_ref(),
        )
        .map(|decision| decision.cost)
    }

    /// Choose one join path for the executor and `EXPLAIN` to share.
    ///
    /// Fresh stats are used only for the first stored-table join, where the
    /// driving cardinality is observable. Later joins and unknown shapes use
    /// the existing deterministic rule. A costed choice may select a hash
    /// build for a limited query, or an index probe for a full scan; both are
    /// already implemented operators and retain the same residual `ON`
    /// evaluation.
    pub(crate) fn join_strategy(
        &self,
        from: &[FromItem],
        inner_index: usize,
        offset_of: usize,
        on: Option<&crate::plan::Expr>,
        full_scan: bool,
        outer_rows: Option<u64>,
    ) -> JoinChoice {
        let hash_key = hash_join_key(from, inner_index, offset_of, on);
        let inner = &from[inner_index].table;
        let probe = self.join_probe(inner, offset_of, on);
        let decision = self.costed_join_decision(
            from,
            inner_index,
            outer_rows,
            hash_key.is_some(),
            probe.as_ref(),
        );

        if let Some(decision) = decision {
            match decision.path {
                JoinPath::Hash => {
                    if let Some(key) = hash_key.as_ref() {
                        return JoinChoice {
                            strategy: JoinStrategy::Hash {
                                outer_key: key.0.outer,
                                inner_key: key.0.inner,
                                collation: key.0.collation,
                            },
                            cost: Some(decision),
                        };
                    }
                }
                JoinPath::Probe => {
                    if let Some((key, ty, collation, kind)) = probe {
                        return JoinChoice {
                            strategy: JoinStrategy::Probe {
                                key,
                                ty,
                                collation,
                                kind,
                            },
                            cost: Some(decision),
                        };
                    }
                }
            }
        }

        if full_scan {
            if let Some(key) = hash_key {
                return JoinChoice {
                    strategy: JoinStrategy::Hash {
                        outer_key: key.0.outer,
                        inner_key: key.0.inner,
                        collation: key.0.collation,
                    },
                    cost: None,
                };
            }
        }
        match probe {
            Some((key, ty, collation, kind)) => JoinChoice {
                strategy: JoinStrategy::Probe {
                    key,
                    ty,
                    collation,
                    kind,
                },
                cost: None,
            },
            None => JoinChoice {
                strategy: JoinStrategy::Materialise,
                cost: None,
            },
        }
    }

    /// Cost a first join when every required cardinality is known.
    fn costed_join_decision(
        &self,
        from: &[FromItem],
        inner_index: usize,
        outer_rows: Option<u64>,
        hash_available: bool,
        probe: Option<&(usize, DataType, Collation, ProbeKind)>,
    ) -> Option<JoinDecision> {
        if inner_index != 1
            || !self.planner_stats.is_current(self.write_version)
            || from.first()?.derived.is_some()
        {
            return None;
        }
        let outer_rows = outer_rows?;
        // Presence of the outer stats is part of the completeness check even
        // though the caller already supplied the LIMIT-aware row estimate.
        self.planner_stats.table(&from[0].table.name)?;
        let inner_stats = self.planner_stats.table(&from[inner_index].table.name)?;
        let group_size = match probe {
            None => None,
            Some((_, _, _, ProbeKind::RowId)) => Some(1),
            Some((_, _, _, ProbeKind::Index(name))) => {
                inner_stats.index(name).map(|stats| stats.group_size())
            }
        };
        planner::choose_join(
            outer_rows,
            inner_stats.row_count,
            group_size,
            hash_available,
        )
    }

    /// Where one join's inner rows come from: a costed choice when fresh stats
    /// cover the first join, then the existing shape rule otherwise.
    ///
    /// `offset_of` is where the inner table's columns begin in the joined row,
    /// which is what translates the plan's ordinals — held against the
    /// concatenation of every table in `FROM` order — onto this table. `from`
    /// and `inner_index` name the tables before and at the join, so a hash-join
    /// key can be checked for a matching declared class on both sides.
    #[allow(clippy::too_many_arguments)]
    fn join_inner(
        &self,
        from: &[FromItem],
        inner_index: usize,
        offset_of: usize,
        on: Option<&crate::plan::Expr>,
        mask: &ColumnMask,
        full_scan: bool,
        outer_rows: Option<u64>,
    ) -> Result<JoinInner<'_>> {
        let inner = &from[inner_index].table;
        let inner_mask = mask.slice(offset_of, inner.columns.len());
        let choice = self.join_strategy(from, inner_index, offset_of, on, full_scan, outer_rows);
        match choice.strategy {
            JoinStrategy::Hash {
                outer_key,
                inner_key,
                collation,
            } => {
                let table = self.hash_join_table(
                    &inner.name,
                    inner_mask,
                    inner_key,
                    inner.columns.len(),
                    collation,
                )?;
                Ok(JoinInner::Hash(HashJoin::from_table(table, outer_key)))
            }
            JoinStrategy::Probe {
                key,
                ty,
                collation,
                kind,
            } => Ok(JoinInner::probe(IndexProbe::new(
                &self.storage,
                &inner.name,
                inner_mask,
                inner.columns.len(),
                key,
                ty,
                collation,
                kind,
                &self.interrupt,
            ))),
            JoinStrategy::Materialise => self.materialise_inner(inner, &inner_mask),
        }
    }

    /// Reuse the immutable build half of a full-scan hash join when the same
    /// physical shape runs again on the same committed rows.
    ///
    /// `write_version` is the row currency used by every persisted retrieval
    /// index too: it advances in the same commit as a row mutation. A catalog
    /// change clears the entry in [`Engine::invalidate_rules`]. An explicit
    /// transaction that has buffered a row change cannot use the committed
    /// version as its identity yet, so it bypasses both lookup and insertion.
    /// This keeps read-your-writes exact without inventing an uncommitted
    /// snapshot number.
    fn hash_join_table(
        &self,
        table_name: &str,
        mask: ColumnMask,
        inner_key: usize,
        width: usize,
        collation: Collation,
    ) -> Result<Rc<HashJoinTable>> {
        let transaction_has_writes = self.in_transaction && !self.pending_changes.is_empty();
        if !transaction_has_writes {
            let cache = self.hash_join_cache.borrow();
            if let Some(cached) = cache.as_ref() {
                if cached.matches(
                    self.write_version,
                    table_name,
                    &mask,
                    inner_key,
                    width,
                    collation,
                ) {
                    return Ok(Rc::clone(&cached.table));
                }
            }
        }

        let table = HashJoin::build_table(
            &self.storage,
            table_name,
            mask.clone(),
            inner_key,
            width,
            collation,
            &self.interrupt,
        )?;
        if !transaction_has_writes
            && self.options.hash_join_cache_bytes > 0
            && table.resident_bytes() <= self.options.hash_join_cache_bytes
        {
            self.hash_join_cache.replace(Some(CachedHashJoin {
                write_version: self.write_version,
                table_name: table_name.to_string(),
                mask,
                inner_key,
                width,
                collation,
                table: Rc::clone(&table),
            }));
        }
        Ok(table)
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
    pub(crate) fn join_probe(
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
        for row in self.scan(&table.name) {
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
        let candidate_limit = candidate_limit(limit, false);
        let wanted = rows_wanted(limit);
        let mut rows = Vec::new();
        for scored in self.evaluate_score(table, score, candidate_limit, wanted, None, env)? {
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

    /// The `MIN`/`MAX` optimisation (`AHL-546`): a scalar (no `GROUP BY`)
    /// query whose every aggregate is `MIN`/`MAX` over the table's rowid or a
    /// column carrying a leading B-tree index answers from one descent per
    /// aggregate to the first or last key of that access path — sqlite3's own
    /// "min/max optimization" — instead of decoding every row to fold three
    /// accumulators that never move once the first (or last) row has been
    /// seen.
    ///
    /// Fires only when nothing else in the statement forces a scan:
    ///
    /// * No `WHERE`, `GROUP BY`, `HAVING`, `DISTINCT`, join, retrieval score
    ///   or window function.
    /// * Exactly one source table, stored (not derived) and not
    ///   `WITHOUT ROWID` — such a table has no rowid at all, and its primary
    ///   key's own index is exactly the general indexed-column case below, so
    ///   nothing is lost by excluding it here and it stays out of this
    ///   already-narrow rewrite's proof obligation.
    /// * Every aggregate is a plain, non-`DISTINCT`, filter-less `MIN` or
    ///   `MAX` of a bare stored column — see [`Engine::min_max_boundary`] for
    ///   which columns qualify.
    /// * Every projected expression is answerable from the aggregates alone
    ///   — no bare column. This path never holds a representative row the
    ///   general aggregate path would have picked as the table's first row
    ///   (`Engine::aggregate`'s doc); a projection that read one would see
    ///   `NULL` instead, which is not what the general path returns for a
    ///   non-empty table.
    ///
    /// **`COUNT(*)` is deliberately not answered here.** This engine keeps no
    /// transactionally exact row count — `ANALYZE`'s statistics are a
    /// snapshot of whatever was last collected, not the live count, and using
    /// a stale number to answer `COUNT(*)` would be exactly the silent wrong
    /// answer `AGENTS.md` refuses. Without one, `COUNT(*)` still needs a scan,
    /// and a statement that mixes it with `MIN`/`MAX` still scans as a whole
    /// — so a `COUNT(*)` anywhere in `plan.aggregates` sends the whole
    /// statement to the general path below, `MIN`/`MAX` included in it.
    ///
    /// Returns `None` — never an error — for every shape this does not cover,
    /// so [`Engine::run_select_to`] just runs the ordinary pipeline instead.
    /// An error here is reserved for a corrupt index that names a row that no
    /// longer exists — the same "this should be impossible" case
    /// [`Engine::indexed_candidates`]'s siblings raise rather than mask.
    fn try_min_max_scalar(&self, plan: &SelectPlan) -> Result<Option<Vec<ExecRow>>> {
        let Some(table) = min_max_scalar_shape(plan) else {
            return Ok(None);
        };

        let mut answers = Vec::with_capacity(plan.aggregates.len());
        for aggregate in &plan.aggregates {
            // `min_max_scalar_shape` has already checked every aggregate is a
            // plain, non-`DISTINCT`, filter-less `MIN`/`MAX` of a bare column
            // — see its doc — so only the boundary read can still say no.
            let take_max = aggregate.func == AggFunc::Max;
            let Some(Expr::Column(ordinal)) = &aggregate.arg else {
                unreachable!("min_max_scalar_shape only admits a bare column argument")
            };
            let Some(access) = self.min_max_access(table, *ordinal, aggregate.collation) else {
                return Ok(None);
            };
            answers.push(self.min_max_boundary(table, *ordinal, take_max, access)?);
        }

        Ok(Some(alloc::vec![ExecRow {
            id: 0,
            score: None,
            values: alloc::vec![Value::Null; table.columns.len()],
            aggregates: answers,
            windows: Vec::new(),
        }]))
    }

    /// The ordered access path that answers `MIN`/`MAX` of column `ordinal`
    /// without a scan, or `None` when there is none: an un-indexed column, or
    /// an index whose leading collation disagrees with the comparison's own
    /// — the same rule [`Engine::choose_index`] holds a `WHERE` probe to, and
    /// for the same reason: an index built under a different collation is a
    /// different key order, and answering `MIN` from its first entry would
    /// answer the wrong question.
    ///
    /// Catalog-only — no storage read — so [`crate::explain`] can call this
    /// too and report exactly the path [`Engine::min_max_boundary`] would
    /// take, rather than a second guess at it.
    pub(crate) fn min_max_access<'a>(
        &'a self,
        table: &Table,
        ordinal: usize,
        collation: Collation,
    ) -> Option<MinMaxAccess<'a>> {
        if table.rowid_alias() == Some(ordinal) {
            return Some(MinMaxAccess::Rowid);
        }
        let column_name = table.columns[ordinal].name.as_str();
        self.catalog
            .indexes_for(&table.name)
            .into_iter()
            .find(|index| {
                index.kind == IndexKind::BTree
                    && index.columns[0].eq_ignore_ascii_case(column_name)
                    && index
                        .collations
                        .first()
                        .copied()
                        .unwrap_or(Collation::Binary)
                        == collation
            })
            .map(MinMaxAccess::Index)
    }

    /// The `MIN`/`MAX` boundary value `access` names.
    ///
    /// `NULL`s: `MIN` skips them, `MAX` of an all-`NULL` column is `NULL` —
    /// sqlite3's rule. Skipping is only interesting for `MIN`: `NULL` sorts
    /// below every other value in this engine's index encoding
    /// (`crate::index`'s module doc), so `MAX`'s plain last entry is already
    /// `NULL` only when every entry is, which is the answer wanted; `MIN`
    /// instead starts its descent just past the run of `NULL` entries.
    fn min_max_boundary(
        &self,
        table: &Table,
        ordinal: usize,
        take_max: bool,
        access: MinMaxAccess<'_>,
    ) -> Result<Value> {
        let index = match access {
            // The rowid itself — `MIN(rowid)`/`MAX(rowid)`, or the declared
            // `INTEGER PRIMARY KEY` column that aliases it. A rowid is never
            // `NULL`, so there is no skip to make on the `MIN` side here.
            MinMaxAccess::Rowid => {
                let row = if take_max {
                    self.storage.last_in_table(&table.name)?
                } else {
                    self.storage.first_in_table(&table.name)?
                };
                return Ok(match row {
                    Some((id, _)) => Value::Integer(id as i64),
                    None => Value::Null,
                });
            }
            MinMaxAccess::Index(index) => index,
        };

        let prefix = crate::index::index_prefix(&index.name);
        let upper = crate::index::upper_bound(&prefix);
        let entry = if take_max {
            // The tree's rightmost entry names the greatest *value*, but not
            // necessarily the row `AggFold::Extreme` would keep: two rows
            // that compare equal under this column's collation (`'Grace'`
            // and `'grace'` under `NOCASE`, say) share one encoded value and
            // therefore one contiguous run of entries, ordered by row id —
            // and the fold keeps whichever it saw *first*, because only a
            // strictly greater value replaces the running best (`AggFold::step`'s
            // doc). The rightmost entry of that run is the *highest* row id,
            // the opposite one. Stripping the trailing row id off the
            // rightmost entry recovers the exact encoded value alone —
            // `entry_key`'s row id suffix is always the last eight bytes —
            // and re-descending to *that* value's first entry is the same
            // one-descent cost for the row the fold actually keeps.
            let Some(greatest) = self.storage.last_index_entry(&prefix, upper.as_deref())? else {
                return Ok(Value::Null);
            };
            let value_prefix = greatest
                .get(..greatest.len().saturating_sub(8))
                .ok_or_else(|| {
                    Error::Corrupt(alloc::format!(
                        "index `{}` entry is too short to hold a row id",
                        index.name
                    ))
                })?;
            let value_upper = crate::index::upper_bound(value_prefix);
            self.storage
                .first_index_entry(value_prefix, value_upper.as_deref())?
        } else {
            // The whole run of `NULL` entries shares the prefix
            // `index_prefix ++ encode(NULL)`; its upper bound is the first
            // key past every one of them, `NULL` or not, so starting there
            // is exactly "skip the `NULL`s".
            let null_prefix =
                crate::index::probe_prefix(&index.name, &[&Value::Null], &index.collations)?;
            let skip_nulls = crate::index::upper_bound(&null_prefix).unwrap_or(null_prefix);
            self.storage
                .first_index_entry(&skip_nulls, upper.as_deref())?
        };
        let Some(entry) = entry else {
            // Nothing past the `NULL`s (`MIN`), or the index has no entries
            // at all (`MAX`, or a `MIN` over an empty table): both are
            // `NULL`, sqlite3's answer for `MIN`/`MAX` of nothing.
            return Ok(Value::Null);
        };
        let row_id = crate::index::row_id_from_entry(&entry)?;
        let Some(row_bytes) = self.storage.get_row(&table.name, row_id)? else {
            return Err(Error::Corrupt(alloc::format!(
                "index `{}` names row {row_id} of `{}`, which does not exist",
                index.name,
                table.name
            )));
        };
        // Read back from the row rather than decoded out of the index key:
        // the index encoding is one-way (`crate::index::encode_value`, a
        // `NOCASE` column's entry holds the *folded* text), so the original
        // value — the one `MIN`/`MAX` has to return — lives only in the row.
        let row = decode_row(&row_bytes)?;
        Ok(row.get(ordinal).cloned().unwrap_or(Value::Null))
    }

    /// Group the joined rows by the `GROUP BY` keys and compute the aggregates,
    /// Whether an aggregate query can be folded as its rows arrive, instead of
    /// being held and folded afterwards.
    ///
    /// Three conditions, each one a case where holding the rows is doing
    /// something streaming would lose:
    ///
    /// * There is an aggregate. Without one there is nothing to fold into.
    /// * No window function. A window runs over the rows a `GROUP BY` folded,
    ///   and reasoning about that on streamed output is a separate change with
    ///   its own argument to make.
    /// * Every aggregate folds from its argument values alone
    ///   ([`eval::folds_from_values_alone`]) — `GROUP_CONCAT` reads its
    ///   separator from the group's first row and so needs the rows.
    ///
    /// A `GROUP BY` is allowed: grouping streams too, into one accumulator per
    /// group rather than one list of rows per group.
    fn can_stream_aggregate(&self, plan: &SelectPlan) -> bool {
        !plan.aggregates.is_empty()
            && plan.windows.is_empty()
            && plan.aggregates.iter().all(eval::folds_from_values_alone)
    }

    /// Fold an aggregate query from the row stream, holding one row per group
    /// instead of every row.
    ///
    /// What a group keeps is its first row — the representative the collecting
    /// path uses for non-aggregate projection expressions and for `HAVING` —
    /// plus one running accumulator per aggregate. For
    /// `SELECT n, COUNT(*) FROM t GROUP BY n` over 100,000 rows in 100 groups
    /// that is 100 rows and 100 counters, against 100,000 `ExecRow`s before.
    ///
    /// The accumulator is [`eval::AggFold`], which is the same fold
    /// [`Engine::aggregate`] finishes with: `fold_aggregate_values` is a loop
    /// over `AggFold::step`, and this function takes that step as each row
    /// arrives. So `SUM(n)` holds a running total rather than a hundred
    /// thousand integers, and `MIN(body)` holds one body — with no second
    /// implementation of `SUM`'s promotion rules or `MIN`/`MAX`'s ordering to
    /// drift from the first.
    /// `an_aggregate_streams_to_the_same_answer_it_collects` requires the two
    /// paths to agree, on grouped and ungrouped shapes both.
    ///
    /// One aggregate still holds a value per row: `DISTINCT` folds equal values
    /// into one before the function sees them, and cannot tell what is a
    /// duplicate until every value has arrived.
    ///
    /// With no `GROUP BY`, empty input still emits one row, all `NULL` across
    /// the joined row's width: `SELECT COUNT(*) FROM empty` answers `0`, not
    /// nothing. With a `GROUP BY`, empty input emits no rows.
    fn stream_aggregate(
        &self,
        plan: &SelectPlan,
        input: AggregateInput<'_>,
        env: &Env<'_>,
    ) -> Result<Vec<ExecRow>> {
        /// What one aggregate holds for one group while the stream runs.
        ///
        /// Which of the three it is follows from the aggregate itself and never
        /// changes, so it is decided once per group rather than per row.
        enum Slot {
            /// `COUNT(*)`: rows, not values. There is no argument to evaluate
            /// and nothing to hold but the counter.
            Rows(i64),
            /// Folded as the values arrive, through the same
            /// [`eval::AggFold::step`] the collecting path finishes with. One
            /// running scalar — or, for `MIN`/`MAX`, one value — instead of one
            /// value per row.
            Folding(eval::AggFold),
            /// Held until the end. `DISTINCT` folds equal values into one
            /// before the function sees them, and what is a duplicate is not
            /// known until every value has arrived, so this one still costs a
            /// value per row.
            Collecting(Vec<Value>),
        }

        impl Slot {
            fn new(aggregate: &Aggregate) -> Self {
                match (&aggregate.arg, aggregate.distinct) {
                    (None, _) => Slot::Rows(0),
                    (Some(_), true) => Slot::Collecting(Vec::new()),
                    (Some(_), false) => match eval::AggFold::new(aggregate) {
                        Some(fold) => Slot::Folding(fold),
                        // `GROUP_CONCAT`, which `can_stream_aggregate` has
                        // already sent to the collecting path — so this is the
                        // right answer to a question that is never asked.
                        None => Slot::Collecting(Vec::new()),
                    },
                }
            }
        }

        /// One group's running state: what it will project from, and what each
        /// aggregate has accumulated for it.
        struct Accumulator {
            id: crate::traits::RowId,
            representative: Vec<Value>,
            slots: Vec<Slot>,
        }

        /// The per-row fold, written once over [`AggregateCells`] so a row
        /// that arrives as owned `Value`s and one that arrives as cells
        /// borrowed from its bytes take the same path: same key, same probe,
        /// same `AggFold::step`. What differs is only where a cell is read
        /// from — and that a borrowed row which lands in an existing group is
        /// never materialised at all.
        struct Folder<'p, 'e, 'v> {
            plan: &'p SelectPlan,
            env: &'e Env<'v>,
            groups: GroupTable<Accumulator>,
            collations: Rc<[Collation]>,
            /// One key, refilled per row and reused. Most rows land in a
            /// group that already exists — that is what grouping *is*, a
            /// hundred thousand rows into a hundred groups — and such a row
            /// allocates nothing to find its group: the probe is this buffer,
            /// cleared and refilled, and the collations are one `Rc` held for
            /// the whole loop rather than a refcount bump per row. Only a row
            /// that opens a new group materialises an owned key, and it does
            /// so by *taking* this buffer rather than copying it, so a key is
            /// built once per group and never per row.
            probe: GroupKey,
            held: usize,
            budget: usize,
        }

        impl Folder<'_, '_, '_> {
            fn step<C: AggregateCells + ?Sized>(
                &mut self,
                id: crate::traits::RowId,
                cells: &C,
            ) -> Result<()> {
                let plan = self.plan;
                let env = self.env;
                self.probe.values.clear();
                for expr in &plan.group_by {
                    self.probe.values.push(cells.eval(expr, env)?);
                }

                // One hash, one probe, and on a miss the bucket the probe
                // ended at is where the new group goes: a row that opens a
                // group no longer descends twice, which the ordered map could
                // not avoid.
                let hash = hash_group_key(&self.probe.values, &self.collations);
                let index = match self.groups.find(hash, &self.probe) {
                    Ok(index) => index,
                    Err(bucket) => {
                        // A new group keeps this row, because the first row of
                        // a group is the representative the collecting path
                        // projects non-aggregate expressions from. This is the
                        // one place a row is materialised.
                        let representative = cells.to_owned_row();
                        self.held = self.held.saturating_add(
                            representative
                                .iter()
                                .map(|value| value.heap_bytes())
                                .sum::<usize>(),
                        );
                        // The probe *becomes* the stored key rather than being
                        // copied into one, so a group's key and the probe
                        // later rows are compared against are the same
                        // construction, collations included — the hash and
                        // the comparison both read the collations, so a key
                        // whose collations differed from the probe's would
                        // group one way and search another.
                        let key = core::mem::replace(
                            &mut self.probe,
                            GroupKey {
                                values: Vec::with_capacity(plan.group_by.len()),
                                collations: Rc::clone(&self.collations),
                            },
                        );
                        self.groups.insert_at(
                            bucket,
                            hash,
                            key,
                            Accumulator {
                                id,
                                representative,
                                slots: plan.aggregates.iter().map(Slot::new).collect(),
                            },
                        )
                    }
                };
                let group = self.groups.value_mut(index);

                for (slot, aggregate) in plan.aggregates.iter().enumerate() {
                    // `FILTER (WHERE ...)` narrows what this aggregate folds
                    // and nothing else, exactly as it does on the collecting
                    // path — including for `COUNT(*)`, which is why it runs
                    // before the count rather than after it.
                    if let Some(filter) = &aggregate.filter {
                        if !eval::is_truthy(&cells.eval(filter, env)?) {
                            continue;
                        }
                    }
                    let Some(arg) = &aggregate.arg else {
                        // `COUNT(*)` has nothing to evaluate.
                        if let Slot::Rows(count) = &mut group.slots[slot] {
                            *count += 1;
                        }
                        continue;
                    };
                    let value = cells.eval(arg, env)?;
                    match &mut group.slots[slot] {
                        Slot::Folding(fold) => {
                            // Re-read rather than added to: `MIN`/`MAX`
                            // *replaces* its running value, so the ceiling has
                            // to charge for the one it now holds and not for
                            // every one it ever held. Everything else here
                            // owns no heap at all.
                            let before = fold.heap_bytes();
                            fold.step(value)?;
                            self.held = self
                                .held
                                .saturating_sub(before)
                                .saturating_add(fold.heap_bytes());
                        }
                        Slot::Collecting(values) => {
                            self.held = self
                                .held
                                .saturating_add(value.heap_bytes() + core::mem::size_of::<Value>());
                            values.push(value);
                        }
                        // A slot's shape came from this same aggregate, so a
                        // row counter never has an argument to fold. Counted
                        // rather than panicked on: a miscount is not worth a
                        // process.
                        Slot::Rows(count) => *count += 1,
                    }
                }

                // What still grows with the input, and so still meets the same
                // per-statement ceiling: a `GROUP BY` over many distinct keys
                // holds one representative row per group, and a `DISTINCT`
                // aggregate holds one value per row because it cannot know
                // what is a duplicate until it has them all. `SUM`/`AVG`/
                // `MIN`/`MAX` no longer do — they fold as the rows arrive, so
                // `MIN(body)` over any number of rows holds one body — and
                // `COUNT(*)` never did.
                if self.budget > 0 && self.held > self.budget {
                    let budget = self.budget;
                    return Err(Error::Memory(alloc::format!(
                        "this statement has to hold one row per group, and one value per row \
                         for each DISTINCT aggregate, before it can answer, and that is past \
                         the {budget}-byte per-statement ceiling. Narrow the `WHERE`, or raise \
                         `EngineOptions::query_memory_bytes`. Nothing was written."
                    )));
                }
                Ok(())
            }
        }

        let slots = plan.aggregates.len();
        let collations: Rc<[Collation]> = plan.group_collations.as_slice().into();
        let mut folder = Folder {
            plan,
            env,
            groups: GroupTable::new(),
            probe: GroupKey {
                values: Vec::with_capacity(plan.group_by.len()),
                collations: Rc::clone(&collations),
            },
            collations,
            held: 0,
            budget: self.options.query_memory_bytes,
        };

        match input {
            AggregateInput::Rows(stream) => {
                for row in stream {
                    let row = row?;
                    self.interrupt.check()?;
                    folder.step(row.id, row.values.as_slice())?;
                }
            }
            AggregateInput::Bytes {
                source,
                mask,
                filter,
            } => {
                // One buffer for every row, parked between rows the way
                // `DecodeFilter` parks its scratch: a row's cells borrow from
                // that row's bytes, are folded, and are cleared before the
                // bytes go, so a row that lands in an existing group costs no
                // allocation at all. `park` is what makes the `'static`
                // honest — see its doc.
                //
                // The rows arrive by callback rather than by iterator
                // (`RowBytes::for_each_row`): a scan hands each row's bytes
                // over while its leaf is borrowed, so no row is ever wrapped
                // in a `RowBuf`, refcounted, or written into a batch `Vec` on
                // its way here (`PERF.md`, AHL-538).
                let mut scratch: Vec<ValueRef<'static>> = Vec::new();
                source.for_each_row(&mut |id, bytes| {
                    self.interrupt.check()?;
                    let mut cells: Vec<ValueRef<'_>> = core::mem::take(&mut scratch);
                    // The row is folded and dropped, never returned, so the
                    // walk stops at the last column the fold reads — see
                    // `decode_row_ref_wanted_into` for the contract that
                    // trades.
                    let outcome =
                        decode_row_ref_wanted_into(bytes, mask, &mut cells).and_then(|()| {
                            // The `WHERE`, on the borrowed cells, before any
                            // fold — the same test and the same three-valued
                            // truth `DecodeFilter` applies on the general
                            // path.
                            if let Some(filter) = filter {
                                let truth =
                                    eval::evaluate_ref(filter, &cells, Computed::NONE, env)?;
                                if !eval::is_truthy(&truth) {
                                    return Ok(());
                                }
                            }
                            folder.step(id, cells.as_slice())
                        });
                    scratch = park(cells);
                    outcome
                })?;
            }
        }

        let Folder {
            groups, collations, ..
        } = folder;

        let mut groups = groups;
        let width = plan.from.iter().map(|item| item.table.columns.len()).sum();
        if groups.is_empty() && plan.group_by.is_empty() {
            // No rows and no `GROUP BY` is still one group: the aggregate of
            // nothing. With a `GROUP BY` it is no groups, which is what an
            // empty map already means.
            let key = GroupKey {
                values: Vec::new(),
                collations: Rc::clone(&collations),
            };
            let hash = hash_group_key(&key.values, &collations);
            if let Err(bucket) = groups.find(hash, &key) {
                groups.insert_at(
                    bucket,
                    hash,
                    key,
                    Accumulator {
                        id: 0,
                        representative: alloc::vec![Value::Null; width],
                        slots: plan.aggregates.iter().map(Slot::new).collect(),
                    },
                );
            }
        }

        let mut out = Vec::with_capacity(groups.len());
        for group in groups.into_values() {
            self.interrupt.check()?;
            let Accumulator {
                id,
                representative,
                slots: group_slots,
            } = group;
            let mut aggregates = Vec::with_capacity(slots);
            for (aggregate, slot) in plan.aggregates.iter().zip(group_slots) {
                aggregates.push(match slot {
                    // `COUNT(*)`: counts rows, not values. Any other function
                    // without an argument is refused in the same words the
                    // collecting path refuses it in.
                    Slot::Rows(count) => match aggregate.func {
                        crate::plan::AggFunc::Count => Value::Integer(count),
                        _ => {
                            return Err(Error::Type(
                                "SUM/AVG/MIN/MAX/GROUP_CONCAT require an argument".to_string(),
                            ))
                        }
                    },
                    Slot::Folding(fold) => fold.finish()?,
                    Slot::Collecting(values) => {
                        eval::fold_aggregate_values(aggregate, values, &[], env)?
                    }
                });
            }

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

    /// Fold an aggregate query from rows already collected, emitting one row
    /// per group.
    ///
    /// [`Engine::stream_aggregate`] handles this without holding the input and
    /// is preferred where it applies; this path remains for the shapes it
    /// cannot take — a window function, or a `GROUP_CONCAT` that reads its
    /// separator from the group's rows.
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
            // Grouped by an ordered map keyed on the group key, not by sorting
            // every input row.
            //
            // This used to build a `(key, row)` pair per input row and sort the
            // whole vector. That is `O(n log n)` comparisons for a query whose
            // answer has `g` rows, and it moves an entire `ExecRow` on every
            // swap — profiling `SELECT n, COUNT(*) FROM users GROUP BY n` over
            // 100,000 rows with 100 groups put **~15% of the query in
            // `quicksort`** and much of the `memmove` beside it (`PERF.md`,
            // 2026-09-01). Both servers this loses to hash-aggregate instead.
            //
            // A hash table makes it `O(n)` and moves no rows at all. Its
            // iteration order is first-seen order, and that is not what a
            // query without an `ORDER BY` observes anyway: `sort_rows` runs
            // after the aggregate and orders groups by representative rowid,
            // so the map's order survives only as the stable sort's tie-break
            // — joined rows sharing a driving rowid, and the synthetic
            // empty-input group. (This used to be a `BTreeMap`, kept ordered
            // on the belief that its order was the output order; it was not,
            // see the root plan's B4a notes, 2026-09-02.)
            let collations: Rc<[Collation]> = plan.group_collations.as_slice().into();
            let mut buckets: GroupTable<Vec<ExecRow>> = GroupTable::new();
            for row in rows {
                self.interrupt.check()?;
                let mut keys = Vec::with_capacity(plan.group_by.len());
                for expr in &plan.group_by {
                    keys.push(eval::evaluate(expr, &row.values, Computed::NONE, env)?);
                }
                let hash = hash_group_key(&keys, &collations);
                let key = GroupKey {
                    values: keys,
                    collations: Rc::clone(&collations),
                };
                let index = match buckets.find(hash, &key) {
                    Ok(index) => index,
                    Err(bucket) => buckets.insert_at(bucket, hash, key, Vec::new()),
                };
                buckets.value_mut(index).push(row);
            }
            groups.extend(buckets.into_values());
        }

        let width = plan.from.iter().map(|item| item.table.columns.len()).sum();
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            // One check per *group*, and a group can be one row, so this is the
            // per-row check for a high-cardinality `GROUP BY` and a cheap one
            // for a low-cardinality fold whose real cost is in the evaluator
            // below.
            self.interrupt.check()?;
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

    /// Retrieve and filter in one pass: the filter is pushed *into* the
    /// retriever's walk rather than applied to its output.
    ///
    /// This replaced the over-fetch loop of earlier revisions. Asking the
    /// retriever for `limit * 4` candidates and filtering afterwards can
    /// discard every one of them when the filter is selective, so the engine
    /// used to double the candidate budget each round and re-run the search
    /// from scratch until the filter admitted `limit` rows — geometrically
    /// re-walking the index, once per round, for filters that keep a small
    /// fraction of rows.
    ///
    /// Now the engine compiles the `WHERE` predicate into a
    /// [`RowFilter`](crate::traits::RowFilter) and hands it to the retriever,
    /// which keeps walking past rejected rows instead of re-running. The
    /// index's contract is that a rejected row is excluded from the result
    /// set and from the candidate budget but never from the traversal, and
    /// the walk ends in a single pass once the filter's answer is secured:
    ///
    /// * a permissive filter costs the same walk an unfiltered query does,
    ///   and returns the same rows it would have;
    /// * a selective filter keeps the walk going until its candidate beam is
    ///   full of matching rows, or the index genuinely runs out of
    ///   candidates — and "ran out" means every row the index can rank has
    ///   been seen, so the surviving rows are the complete answer for that
    ///   filter, exactly as the old loop's exhaustion round was;
    /// * a filter nothing satisfies still terminates: the walk is bounded by
    ///   the visited set and returns an empty answer, never a hang.
    ///
    /// Rows are only ever ranked within one probe, so the answer is a
    /// deterministic function of the query and the corpus.
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

        // The retriever is asked for the same over-fetched budget an
        // unfiltered query gets, in filter-passing rows rather than plain
        // rows, so a permissive filter sees exactly the candidate set (and
        // recall) an unfiltered query would.
        let candidate_limit = candidate_limit(Some(want), true);

        // The index stays ignorant of SQL: it only knows row ids, and this
        // closure is where they become rows and a `WHERE`. It runs inside the
        // walk — once per candidate the walk visits — so decode and predicate
        // cost is paid for visited candidates, not for the whole corpus.
        //
        // It is also the one place cancellation reaches inside a retrieval
        // walk. The walk itself is behind [`crate::FullTextIndex::search`] /
        // [`crate::VectorIndex::search`], which any backend may implement and
        // which take no signal, so an *unfiltered* search runs to completion
        // however long it takes — the documented gap, `docs/server.md`. A
        // filtered one is interruptible because this closure is called once per
        // candidate the walk visits.
        let predicate: &dyn Fn(RowId) -> Result<bool> = &|id| {
            self.interrupt.check()?;
            match self.storage.get_row(&table.name, id)? {
                Some(bytes) => {
                    let row = decode_row_masked(&bytes, mask)?;
                    Ok(eval::is_truthy(&eval::evaluate(
                        filter,
                        &row,
                        Computed::NONE,
                        env,
                    )?))
                }
                None => Ok(false),
            }
        };

        let hits =
            self.evaluate_score(table, score, candidate_limit, want, Some(predicate), env)?;
        let mut matched = Vec::with_capacity(hits.len().min(want));
        for hit in hits.into_iter().take(want) {
            if let Some(bytes) = self.storage.get_row(&table.name, hit.id)? {
                matched.push(ExecRow {
                    id: hit.id,
                    score: Some(hit.score),
                    values: decode_row_masked(&bytes, mask)?,
                    aggregates: Vec::new(),
                    windows: Vec::new(),
                });
            }
        }
        Ok(matched)
    }

    /// Evaluate a retrieval expression into a ranked candidate list.
    ///
    /// The query side of each leaf is an expression, so an embedding or a
    /// search string can be bound per execution. Its type — and, for a vector,
    /// its dimension — is checked here rather than at prepare time, because
    /// that is the first moment a `?` has a value at all.
    ///
    /// `filter`, when present, is pushed into every underlying retriever —
    /// for a [`ScoreExpr::Fuse`], into each part's search, so the fused
    /// ranking only ever sees rows that passed it.
    ///
    /// `k` is how many candidates each retriever is asked for and `wanted` is
    /// how many rows the query can return. They differ by
    /// [`CANDIDATE_OVERFETCH`], and both are needed: `k` sizes the fetch, and
    /// `wanted` is the floor a session's `ef_search` is checked against —
    /// which has to be the *answer* rather than the over-fetch, or the
    /// cheapest half of the recall/latency trade would be unreachable. Checked
    /// only in the vector arm, because `ef_search` means nothing to a BM25
    /// index and a text-only query must not be refused by a variable that
    /// cannot affect it.
    fn evaluate_score(
        &self,
        table: &Table,
        expr: &ScoreExpr,
        k: usize,
        wanted: usize,
        filter: Option<&dyn Fn(RowId) -> Result<bool>>,
        env: &Env<'_>,
    ) -> Result<Vec<Scored>> {
        match expr {
            ScoreExpr::Vector { column, query } => {
                let index = self.vector_index(table, *column)?;
                let query = bind_embedding(table, *column, query, env)?;
                let mut hits = match self.vector_ef_search() {
                    None => index.search(&query, k, filter)?,
                    Some(ef) => {
                        check_ef_search(ef, wanted)?;
                        index.search_with_ef(&query, k, ef, filter)?
                    }
                };
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
                let mut hits = index.search(&query, k, filter)?;
                sort_by_score_desc(&mut hits);
                Ok(hits)
            }
            ScoreExpr::Fuse { parts, k: rrf_k } => {
                let mut lists = Vec::with_capacity(parts.len());
                for part in parts {
                    lists.push(self.evaluate_score(table, part, k, wanted, filter, env)?);
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
    pub(crate) fn resolve_full_text_index(
        &self,
        table: &Table,
        columns: &[usize],
    ) -> Result<&Index> {
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

    /// The candidate-list size the backend answering `table.column` would
    /// search `k` neighbours with, or `None` for a backend that has no
    /// candidate list at all.
    ///
    /// Asked of the *backend* rather than derived from a default, because that
    /// is the object the search will make the same call on — see
    /// [`crate::explain`], which reports it.
    pub(crate) fn vector_index_ef_for(
        &self,
        table: &Table,
        column: usize,
        k: usize,
    ) -> Result<Option<usize>> {
        Ok(self.vector_index(table, column)?.ef_for(k))
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

    /// The *declared* `Vector` index over one column, for [`crate::explain`],
    /// which needs the index's name rather than its backend.
    ///
    /// Matched on the same `(table, [column])` key [`index_key`] builds and
    /// [`Engine::open_one_index`] registers the backend under, so an index
    /// named here is the one [`Engine::vector_index`] would find. It is still
    /// a separate lookup — the catalog holds the name, the backend map does
    /// not — and it fails with the same message when there is no such index,
    /// because a query `EXPLAIN` cannot describe is a query that would not
    /// have run either.
    pub(crate) fn resolve_vector_index(&self, table: &Table, column: usize) -> Result<&Index> {
        let key = index_key(table, column)?;
        self.catalog
            .indexes_for(&table.name)
            .into_iter()
            .find(|index| {
                index.kind == IndexKind::Vector
                    && retrieval_key(&index.table, &index.columns) == key
            })
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

/// The storage representation of every column of `table`, in ordinal order —
/// [`encode_typed_row`]'s second argument.
///
/// Statement-invariant, for the same reason a statement's `Table` clone is:
/// no DDL can interleave with a row loop. A statement that writes many rows
/// builds this once, inside a [`RowEncoder`], and hands it to
/// [`encode_typed_row_into`] per row; [`encode_table_row`] is the single-row
/// spelling of the same two lines.
fn column_types(table: &Table) -> Vec<DataType> {
    table.columns.iter().map(|column| column.ty).collect()
}

/// What a statement that writes many rows keeps across its row loop: the
/// column types [`encode_typed_row_into`] reads and the buffer it encodes
/// into — one allocation for the statement rather than one `Vec<DataType>`
/// and one `Vec<u8>` grown from empty per row. `INSERT`, `UPDATE` and
/// `ON CONFLICT DO UPDATE` each build one; [`encode_table_row`] stays the
/// single-row spelling for the paths that write one row.
struct RowEncoder {
    types: Vec<DataType>,
    encoded: Vec<u8>,
}

impl RowEncoder {
    fn for_table(table: &Table) -> Self {
        Self {
            types: column_types(table),
            encoded: Vec::new(),
        }
    }

    /// `row` in storage form, valid until the next call.
    fn encode(&mut self, row: &[Value]) -> &[u8] {
        encode_typed_row_into(&mut self.encoded, row, &self.types);
        &self.encoded
    }
}

fn encode_table_row(table: &Table, row: &[Value]) -> Vec<u8> {
    encode_typed_row(row, &column_types(table))
}

/// The decisions a `SELECT`'s `LIMIT`, `OFFSET` and driving-table filter make
/// before a single row is read.
///
/// Split out of [`Engine::run_select_to`] so [`crate::explain`] can report
/// them instead of deciding again from the same inputs. The two shapes that
/// look identical from outside and are not — a hash join and an index nested
/// loop — are chosen from `full_scan` alone, so an `EXPLAIN` that recomputed
/// it and got it wrong would be worse than no `EXPLAIN`.
pub(crate) struct ScanShape {
    /// `LIMIT`, resolved (a `?` is a number only now), narrowed by the
    /// caller's own row budget.
    pub limit: Option<usize>,
    /// `OFFSET`, resolved; zero when the query has none.
    pub offset: usize,
    /// How many rows the row source has to produce to satisfy both.
    pub fetch: Option<usize>,
    /// `Some(n)` when `LIMIT` may end the scan rather than truncate the
    /// answer.
    pub stop_after: Option<usize>,
    /// Whether the driving side really is read end to end.
    pub full_scan: bool,
    /// Whether the cost model may exchange which table drives a join.
    ///
    /// Reordering changes the order rows leave an unordered join. That is
    /// invisible when the whole join is read (the answer is a set) and when
    /// an `ORDER BY` decides the order afterwards — but under a `LIMIT` with
    /// no `ORDER BY` a different order is a different *set*, and a plan
    /// choice may not decide which rows a query returns. So: no `LIMIT`, or
    /// an `ORDER BY`. A scored query is excluded because its driving table
    /// is part of its meaning. (Ties under the `ORDER BY` may still come out
    /// in another order, as they may in SQLite; a query that cares says so.)
    pub reorderable: bool,
}

/// Resolve one `SELECT`'s [`ScanShape`] against the bound parameters.
///
/// `cap` is the caller's own row budget — `EXISTS` and a scalar subquery want
/// one row — and it folds into `limit` exactly as a written `LIMIT` does.
pub(crate) fn scan_shape(
    plan: &SelectPlan,
    env: &Env<'_>,
    cap: Option<usize>,
) -> Result<ScanShape> {
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

    // A join's inner side is chosen from this rather than per row. A hash join
    // pays an up-front O(inner) scan to build its table, then O(1) per
    // outer row; an index probe pays a B-tree descent per outer row with no
    // build. The hash join therefore wins only when the outer side is
    // scanned in full — a `LIMIT` (or an `EXISTS` cap, which `fetch`
    // already reflects) would have stopped after a few outer rows and made
    // the probe cheaper, and a `WHERE` that pins the primary key makes the
    // outer side a single row. Score queries are excluded because their
    // driving side is a bounded candidate set, not a scan of the table.
    let driving_is_a_point_lookup =
        pinned_rowid(&plan.from[0].table, plan.filter.as_ref(), env.params()).is_some();
    let full_scan = fetch.is_none() && plan.score.is_none() && !driving_is_a_point_lookup;
    let reorderable = plan.score.is_none() && (fetch.is_none() || !plan.order.is_empty());

    Ok(ScanShape {
        limit,
        offset,
        fetch,
        stop_after,
        full_scan,
        reorderable,
    })
}

/// The ordered access path [`Engine::min_max_access`] found for one column —
/// the table's own rowid order, or a named B-tree index's.
#[derive(Debug, Clone, Copy)]
pub(crate) enum MinMaxAccess<'a> {
    /// `MIN(rowid)`/`MAX(rowid)`, including a declared `INTEGER PRIMARY KEY`
    /// column, which is the rowid under another name.
    Rowid,
    /// A B-tree index whose leading column is the one asked for, under a
    /// matching collation.
    Index(&'a Index),
}

impl MinMaxAccess<'_> {
    /// `EXPLAIN`'s wording for this access path, shared with the executor's
    /// own choice so the two cannot describe different plans — see
    /// [`crate::explain`]'s module doc.
    pub(crate) fn detail(self) -> String {
        match self {
            MinMaxAccess::Rowid => "INTEGER PRIMARY KEY".to_string(),
            MinMaxAccess::Index(index) => alloc::format!("INDEX {}", index.name),
        }
    }
}

/// Whether `plan` is a scalar `MIN`/`MAX` query the [`Engine::try_min_max_scalar`]
/// rewrite may answer, and if so, its one source table.
///
/// Purely structural — no catalog, no storage — so both the executor and
/// [`crate::explain`] can ask it before either commits to anything: it is the
/// complete list of conditions from [`Engine::try_min_max_scalar`]'s doc
/// except whether an access path actually exists for each aggregate's column,
/// which needs the catalog and is [`Engine::min_max_access`]'s question
/// alone.
pub(crate) fn min_max_scalar_shape(plan: &SelectPlan) -> Option<&Table> {
    if plan.filter.is_some()
        || !plan.group_by.is_empty()
        || plan.having.is_some()
        || plan.distinct
        || !plan.joins.is_empty()
        || plan.from.len() != 1
        || plan.score.is_some()
        || !plan.windows.is_empty()
        || plan.aggregates.is_empty()
    {
        return None;
    }
    let driving = &plan.from[0];
    if driving.derived.is_some() || driving.table.without_rowid {
        return None;
    }
    let table = &driving.table;

    for aggregate in &plan.aggregates {
        if !matches!(aggregate.func, AggFunc::Min | AggFunc::Max) {
            // `COUNT`/`SUM`/`AVG`/`GROUP_CONCAT` all need a scan — see
            // `Engine::try_min_max_scalar`'s doc for why `COUNT(*)` is not
            // special-cased even though the executor could answer it cheaply
            // once the engine keeps an exact count.
            return None;
        }
        if aggregate.distinct || aggregate.filter.is_some() {
            return None;
        }
        if !matches!(aggregate.arg, Some(Expr::Column(_))) {
            return None;
        }
    }

    // No projected expression may read a raw column: see
    // `Engine::try_min_max_scalar`'s doc for why that would disagree with the
    // general path's answer. Reuses `Expr::columns_read` — the same walker
    // projection pushdown trusts to find every column an expression can
    // observe — rather than a second one that could disagree about a variant
    // it missed.
    let mut read = ColumnMask::none(table.columns.len());
    for item in &plan.items {
        match item {
            SelectItem::Expr { expr, .. } => expr.columns_read(&mut read),
            // `Column`/`Score` read a raw column or the retrieval score
            // outright; `plan.score.is_none()` already ruled the latter out
            // above, so only `Column` can still reach here.
            SelectItem::Column { .. } | SelectItem::Score { .. } => return None,
        }
    }
    if read.walk_len(table.columns.len()) != 0 {
        return None;
    }

    Some(table)
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
pub(crate) fn pinned_rowid(
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
pub(crate) struct JoinKey {
    /// Joined-row ordinal of the outer column the key is read from.
    pub outer: usize,
    /// Ordinal of the inner column within the inner table.
    pub inner: usize,
    /// What the `ON`'s `=` compares under. An index may only answer this key if
    /// it is keyed under the same collation, for the reason [`Term::collation`]
    /// gives — and here the stakes are the same: a probe that missed rows the
    /// materialising path finds would make the join's answer depend on whether
    /// an index happened to exist.
    pub collation: Collation,
}

/// The join key a hash join can build on, or `None` when none qualifies.
///
/// A hash join can only be handed a key whose equality it can reproduce with a
/// hash: the two columns have to share a declared storage class, so equal
/// values are the same [`Value`] class and hash alike — an `INTEGER` next to a
/// `REAL` compares as `f64` and would need normalisation the hash does not do.
/// A hash that *over*-groups is safe because candidates still compare their
/// keys (and residual predicates still run), but one that splits equal keys
/// apart is wrong, which is what that condition rules out.
///
/// **A non-binary collation used to be refused here too, and is not any more.**
/// The objection was real — hashing the stored bytes of a `NOCASE` key puts
/// `'KEY'` and `'key'` in different buckets and the join then misses a pair the
/// `ON` calls equal, which is the "splits equal keys apart" failure. The fix is
/// to hash what the collation *compares* rather than what the row stores:
/// `HashJoin` folds the key through [`Collation::fold`] for both the bucket and
/// the candidate comparison, so equal-under-collation values group together and
/// the grouping errs only toward over-grouping. Refusing instead cost a
/// measured ~183x on an unindexed `TEXT COLLATE NOCASE` join (it fell all the
/// way to `Materialise`), which is the shape a MySQL-compatible server meets by
/// default, since MySQL's own default collation is case-insensitive.
///
/// **`REAL` used to be refused too, and is not any more.** The type list was
/// `INTEGER | TEXT | BLOB`, which excluded a `REAL`-to-`REAL` key for a reason
/// that only ever applied to a `REAL`-to-`INTEGER` one — and the
/// same-declared-class check above already makes that pair unreachable. So the
/// exclusion cost ~235x (again all the way down to `Materialise`) and bought
/// nothing. What a float key does need is two properties the hash now has, and
/// neither is about class mixing: `-0.0` and `+0.0` are equal under `=` and
/// must hash alike, and `NaN` is equal to nothing at all, so it may hash
/// anywhere as long as the candidate comparison rejects it — which plain `f64`
/// equality already does.
///
/// What keeps a `REAL` column's values *actually* `Value::Real`, so that
/// hashing them as floats is sound, is write-side affinity:
/// [`crate::sql::coerce`] converts an `INTEGER` bound into a `REAL` column to
/// `Value::Real` on the way in, and refuses the classes it cannot convert. A
/// derived column with no stored column behind it carries `DataType::Numeric`,
/// which is not in this list, so it cannot reach the hash join either.
pub(crate) fn hash_join_key(
    from: &[FromItem],
    inner_index: usize,
    offset_of: usize,
    on: Option<&crate::plan::Expr>,
) -> Option<(JoinKey, DataType)> {
    let inner = &from[inner_index].table;
    let mut keys = Vec::new();
    collect_join_keys(on?, offset_of, inner.columns.len(), &mut keys);
    for key in keys {
        let inner_ty = inner.columns[key.inner].ty;
        if !matches!(
            inner_ty,
            DataType::Integer | DataType::Real | DataType::Text | DataType::Blob
        ) {
            continue;
        }
        if outer_column_type(from, inner_index, key.outer) == inner_ty {
            return Some((key, inner_ty));
        }
    }
    None
}

/// Whether `ON` is exactly one equality, with no residual conjunct to run
/// after a hash candidate's key has been compared directly.
fn is_single_equality(on: Option<&crate::plan::Expr>) -> bool {
    matches!(
        on,
        Some(crate::plan::Expr::Binary {
            op: crate::plan::BinaryOp::Eq,
            ..
        })
    )
}

/// The declared type of a joined-row column that belongs to one of the tables
/// the join has already produced, i.e. an ordinal below `offset_of`.
fn outer_column_type(from: &[FromItem], inner_index: usize, ordinal: usize) -> DataType {
    let mut base = 0;
    for item in &from[..inner_index] {
        let width = item.table.columns.len();
        if ordinal < base + width {
            return item.table.columns[ordinal - base].ty;
        }
        base += width;
    }
    // Unreachable for a valid plan — a join key's outer ordinal is below
    // `offset_of`, the sum of these widths. `Numeric` never equals a hashable
    // class, so a plan that somehow reached here simply declines the hash join.
    DataType::Numeric
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

/// The narrowest range of one index the collected terms justify, and the shape
/// of the key that produced it.
///
/// The shape is carried alongside the bytes rather than being re-derived,
/// because [`crate::explain`] renders it (`docs_author (author=? AND year>?)`)
/// and the executor walks it: one value, so what `EXPLAIN` prints is by
/// construction the probe that runs.
pub(crate) struct IndexRange {
    /// How many leading columns an equality bound.
    pub equalities: usize,
    /// Whether the column after them was narrowed from below (`>` or `>=`).
    pub lower: bool,
    /// Whether that same column was narrowed from above (`<` or `<=`).
    pub upper: bool,
    /// The run of entries to walk.
    pub range: crate::index::KeyRange,
}

impl IndexRange {
    /// How many of the index's columns this range binds — what
    /// [`Engine::choose_index`] ranks candidates by, since a longer bound
    /// prefix is a shorter walk.
    pub fn bound(&self) -> usize {
        self.equalities + usize::from(self.lower || self.upper)
    }
}

/// The narrowest range of one index the collected terms justify.
///
/// The rule is the standard one: equalities down the leading columns, then at
/// most one range predicate on the column after them. Nothing is bound beyond
/// the first column an equality does not cover, because entries past that
/// point are not contiguous.
///
/// **A term is only usable when its collation is the one the index is keyed
/// under**, which is SQLite's rule and is what stops this from being an
/// optimisation that changes answers. See [`Term::collation`].
fn index_probe(table: &Table, index: &Index, terms: &[Term]) -> Result<Option<IndexRange>> {
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
    let mut lower = false;
    let mut upper = false;
    if equalities.len() < ordinals.len() {
        let position = equalities.len();
        let ordinal = ordinals[position];
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
                    lower = true;
                }
                BinaryOp::Lt | BinaryOp::LtEq => {
                    range = range.with_upper(
                        &index.name,
                        &equalities,
                        &index.collations,
                        &term.value,
                    )?;
                    upper = true;
                }
                _ => {}
            }
        }
    }
    let probe = IndexRange {
        equalities: equalities.len(),
        lower,
        upper,
        range,
    };
    if probe.bound() == 0 {
        return Ok(None);
    }
    Ok(Some(probe))
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

/// The key this row contributes to each B-tree index of `table`.
///
/// Every row contributes exactly one entry per index, `NULL`s included, so
/// "one entry per row per index" is an invariant a test can check — and the
/// DST sweep does.
///
/// Every key is built before any is written, and that is deliberate:
/// [`index_values`] can fail on a row the catalog and the table disagree
/// about, and a half-written entry list for one row is exactly the state
/// [`Engine::write_btree_entries`] exists to make impossible.
fn btree_entry_keys(
    table: &Table,
    indexes: &RowIndexes,
    id: RowId,
    row: &[Value],
) -> Result<Vec<alloc::vec::Vec<u8>>> {
    let mut keys = Vec::with_capacity(indexes.btree.len());
    for index in &indexes.btree {
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

/// Load the optional planner statistics cache.
///
/// The blob is derived state, so corruption or a version mismatch disables it
/// rather than preventing the database from opening. An actual storage read
/// failure still propagates: that is an I/O failure, not stale statistics.
fn load_planner_stats(
    storage: &dyn Storage,
    write_version: u64,
    schema_version: u64,
    catalog: &Catalog,
) -> Result<PlannerStats> {
    let Some(bytes) = storage.get_meta(STATS_META_KEY)? else {
        return Ok(PlannerStats::empty(write_version));
    };
    let catalog = catalog.encode();
    match PlannerStats::decode(&bytes) {
        Ok(stats)
            if stats.is_current(write_version)
                && stats.schema_version == schema_version
                && stats.catalog == catalog =>
        {
            Ok(stats)
        }
        Ok(_) | Err(_) => Ok(PlannerStats::empty(write_version)),
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

/// The base namespace a paged BM25 index keeps its structures under.
///
/// The leading `\u{1}` does the same job it does for
/// [`vector_index_namespace`]: a SQL identifier cannot begin with a control
/// character and engine metadata keys begin with `\0`, so nothing else in the
/// tree can produce this prefix.
///
/// Two further details are load-bearing rather than cosmetic, both because
/// this is the one namespace built from a *list* of columns.
///
/// * **Every column is `\u{1}`-terminated**, so `(ab, c)` and `(a, bc)` do not
///   spell the same namespace. A separator a column name could contain — a
///   dot, as the vector namespace uses for its single column — would let two
///   different indexes share one set of postings, which is a silent wrong
///   answer rather than an error.
/// * **The base therefore never contains `\u{1}\u{1}`**, because a column name
///   is never empty. [`PagedBm25Index`] derives its dictionary, term and
///   postings namespaces by appending `\u{1}d`, `\u{1}x` and `\u{1}p`, so
///   every derived name does contain it — which is what makes a derived name
///   unable to collide with another index's base no matter what the columns
///   are called.
fn full_text_index_namespace(table: &str, columns: &[String]) -> String {
    let mut namespace = alloc::format!("\u{1}fts:{}\u{1}", table.to_ascii_lowercase());
    for column in columns {
        namespace.push_str(&column.to_ascii_lowercase());
        namespace.push('\u{1}');
    }
    namespace
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
fn window(
    plan: &SelectPlan,
    mut rows: Vec<ExecRow>,
    env: &Env<'_>,
    interrupt: &Interrupt,
) -> Result<Vec<ExecRow>> {
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
            interrupt.check()?;
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

    // Peer-group boundaries and index (index-into-`sequence`), needed by
    // `RANK`/`DENSE_RANK`, the default frame's peer-group-aware `CURRENT
    // ROW` bound, and an explicit `RANGE`/`GROUPS` frame's own peer-group
    // bounds (`WindowFrame`'s doc). Only computed when there is an `ORDER
    // BY` to tie on; with none, every row is the whole partition's own
    // single peer group (`rank`/`dense_rank` never actually reach that case
    // without an `ORDER BY` producing a total order, but a frame's default
    // or an explicit `RANGE`/`GROUPS` does, and `WindowFrame::whole_partition`
    // is exactly "every row's peer group is the whole partition").
    let (peer_start, peer_end, group_of, group_start, group_end) = if wf.order_by.is_empty() {
        (
            alloc::vec![0usize; n],
            alloc::vec![n.saturating_sub(1); n],
            alloc::vec![0usize; n],
            if n == 0 {
                Vec::new()
            } else {
                alloc::vec![0usize]
            },
            if n == 0 {
                Vec::new()
            } else {
                alloc::vec![n - 1]
            },
        )
    } else {
        let mut keys: Vec<Vec<SortKey>> = Vec::with_capacity(n);
        for &index in &sequence {
            keys.push(window_sort_keys(&wf.order_by, &rows[index], env)?);
        }
        let mut starts = alloc::vec![0usize; n];
        let mut ends = alloc::vec![0usize; n];
        let mut group_of = alloc::vec![0usize; n];
        let mut group_start = Vec::new();
        let mut group_end = Vec::new();
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            while j < n
                && compare_sort_keys(&keys[i], &keys[j], &wf.order_by) == core::cmp::Ordering::Equal
            {
                j += 1;
            }
            let group_index = group_start.len();
            group_start.push(i);
            group_end.push(j - 1);
            for slot in i..j {
                starts[slot] = i;
                ends[slot] = j - 1;
                group_of[slot] = group_index;
            }
            i = j;
        }
        (starts, ends, group_of, group_start, group_end)
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
        // Verified against sqlite3 3.54, ties included: both are the same
        // rank-family loop as `RANK`/`DENSE_RANK` above, just a different
        // formula per peer group instead of an integer.
        WindowFunc::PercentRank => {
            let mut rank = 1i64;
            let mut position = 0;
            while position < n {
                let end = peer_end[position];
                let value = Value::Real(if n <= 1 {
                    0.0
                } else {
                    (rank - 1) as f64 / (n - 1) as f64
                });
                for &row_index in &sequence[position..=end] {
                    rows[row_index].windows[window_index] = value.clone();
                }
                rank += (end - position + 1) as i64;
                position = end + 1;
            }
        }
        WindowFunc::CumeDist => {
            let mut position = 0;
            while position < n {
                let end = peer_end[position];
                let value = Value::Real((end + 1) as f64 / n as f64);
                for &row_index in &sequence[position..=end] {
                    rows[row_index].windows[window_index] = value.clone();
                }
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
            let needs_numeric = wf.frame.unit == FrameUnit::Range
                && (matches!(
                    wf.frame.start,
                    FrameBound::Preceding(_) | FrameBound::Following(_)
                ) || matches!(
                    wf.frame.end,
                    FrameBound::Preceding(_) | FrameBound::Following(_)
                ));
            let numeric = if needs_numeric {
                // `sql.rs`'s `resolve_window_frame` refuses a value-offset
                // `RANGE` bound unless the window has exactly one `ORDER BY`
                // term, so indexing it here is safe.
                let term = core::slice::from_ref(&wf.order_by[0]);
                let mut values = Vec::with_capacity(n);
                for &index in &sequence {
                    let key = match &window_sort_keys(term, &rows[index], env)?[0] {
                        SortKey::Value(Value::Integer(x)) => Some(*x as f64),
                        SortKey::Value(Value::Real(x)) => Some(*x),
                        _ => None,
                    };
                    // "Comparison space": negated for `DESC` so the array is
                    // monotonically non-decreasing in sequence order either
                    // way — see `numeric_range_bound`'s doc.
                    values.push(if term[0].desc { key.map(|v| -v) } else { key });
                }
                let lo = values.iter().position(Option::is_some).unwrap_or(n);
                let hi = values
                    .iter()
                    .rposition(Option::is_some)
                    .map_or(0, |p| p + 1);
                Some((values, lo, hi))
            } else {
                None
            };
            let ctx = FrameContext {
                unit: wf.frame.unit,
                peer_start: &peer_start,
                peer_end: &peer_end,
                group_of: &group_of,
                group_start: &group_start,
                group_end: &group_end,
                numeric: numeric.as_ref().map(|(keys, lo, hi)| NumericFrameKeys {
                    keys,
                    lo: *lo,
                    hi: *hi,
                }),
            };
            for position in 0..n {
                let frame = frame_range(&ctx, &start_bound, &end_bound, position, n);
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

/// A `RANGE` frame's value-offset bounds for one window: the single
/// `ORDER BY` term's key per sequence position, in "comparison space" (see
/// [`numeric_range_bound`]'s doc), and `[lo, hi)` — the contiguous
/// sub-range that actually holds a value, since a `NULL` or non-numeric key
/// (SQLite's storage-class order keeps them together, before or after every
/// number) sorts to one or both ends rather than scattering through it.
struct NumericFrameKeys<'a> {
    keys: &'a [Option<f64>],
    lo: usize,
    hi: usize,
}

/// Everything [`frame_range`] needs beyond the two bounds themselves, one
/// per window function evaluated — bundled so that generalising from `ROWS`
/// alone to `ROWS`/`RANGE`/`GROUPS` did not turn every call site into an
/// eight-argument function call.
struct FrameContext<'a> {
    unit: FrameUnit,
    /// This row's peer group, as `[peer_start[p], peer_end[p]]` — read for
    /// a `CURRENT ROW` bound under `Range`/`Groups`, on *either* side (a
    /// `ROWS` `CURRENT ROW` always means the row's own literal position
    /// instead, on both sides too, so neither is read then).
    peer_start: &'a [usize],
    peer_end: &'a [usize],
    /// This row's 0-based peer-group index, and each group's own first/last
    /// position — read for a `Groups` `<n> PRECEDING`/`FOLLOWING` bound,
    /// which counts groups rather than rows.
    group_of: &'a [usize],
    group_start: &'a [usize],
    group_end: &'a [usize],
    /// Read for a `Range` `<n> PRECEDING`/`FOLLOWING` bound. `None` unless
    /// at least one of the two bounds actually is one — see this window's
    /// call site in `evaluate_window_partition`.
    numeric: Option<NumericFrameKeys<'a>>,
}

/// A bound's row position relative to `position`, using `i64::MIN`/`MAX` as
/// sentinels for the `UNBOUNDED` variants — and, past the last group or
/// before the numeric range, for a `Groups`/`Range` `PRECEDING`/`FOLLOWING`
/// bound that overruns one — so that the emptiness and clamping arithmetic
/// in [`frame_range`] does not need to special-case any of them.
///
/// `is_start` only matters for `Range`/`Groups`: a `CURRENT ROW` bound reads
/// `peer_start` as a frame start and `peer_end` as a frame end (the "whole
/// peer group" reinterpretation [`crate::plan::WindowFrame`]'s doc measures
/// against sqlite3, on *either* side, unlike the position a `ROWS` `CURRENT
/// ROW` always means); a `Groups` offset bound resolves to its target group's
/// first position as a start, last as an end.
fn bound_position(
    ctx: &FrameContext,
    bound: &ResolvedBound,
    position: usize,
    is_start: bool,
) -> i64 {
    let group_bound = |target: i64| -> i64 {
        if target < 0 {
            i64::MIN
        } else if target as usize >= ctx.group_start.len() {
            i64::MAX
        } else if is_start {
            ctx.group_start[target as usize] as i64
        } else {
            ctx.group_end[target as usize] as i64
        }
    };
    match bound {
        ResolvedBound::UnboundedPreceding => i64::MIN,
        ResolvedBound::UnboundedFollowing => i64::MAX,
        ResolvedBound::CurrentRow => match ctx.unit {
            FrameUnit::Rows => position as i64,
            FrameUnit::Range | FrameUnit::Groups => {
                if is_start {
                    ctx.peer_start[position] as i64
                } else {
                    ctx.peer_end[position] as i64
                }
            }
        },
        ResolvedBound::Preceding(offset) | ResolvedBound::Following(offset) => {
            let preceding = matches!(bound, ResolvedBound::Preceding(_));
            let sign = if preceding { -1i64 } else { 1i64 };
            match ctx.unit {
                FrameUnit::Rows => (position as i64).saturating_add(sign * offset),
                FrameUnit::Groups => {
                    group_bound((ctx.group_of[position] as i64).saturating_add(sign * offset))
                }
                FrameUnit::Range => {
                    let numeric = ctx
                        .numeric
                        .as_ref()
                        .expect("a Range offset bound always has one computed");
                    numeric_range_bound(numeric, position, ctx, preceding, *offset, is_start)
                }
            }
        }
    }
}

/// A `RANGE` `<n> PRECEDING`/`FOLLOWING` bound's row position, by binary
/// search: the first (`is_start`) or last (`!is_start`) sequence position
/// whose key is within the offset of the current row's own — confirmed
/// against sqlite3, ASC and DESC both (the window-functions sqllogictest
/// file has both measurements). `numeric.keys` is already in "comparison
/// space" — negated for a `DESC` term at the call site that built it — so
/// it is monotonically non-decreasing in sequence order regardless of
/// direction, and `PRECEDING` always means "toward a smaller key here",
/// `FOLLOWING` "toward a larger one", the same either way.
///
/// The current row's own key is `None` when it is `NULL` or (a disclosed,
/// narrower answer than sqlite3's own degenerate one — see
/// [`crate::plan::WindowFrame`]'s doc) not numeric at all; either way this
/// returns the row's own peer group rather than attempting a value comparison,
/// matching sqlite3's measured `NULL` behaviour and standing in for the
/// non-numeric case too.
fn numeric_range_bound(
    numeric: &NumericFrameKeys<'_>,
    position: usize,
    ctx: &FrameContext,
    preceding: bool,
    offset: i64,
    is_start: bool,
) -> i64 {
    let Some(current) = numeric.keys[position] else {
        return if is_start {
            ctx.peer_start[position] as i64
        } else {
            ctx.peer_end[position] as i64
        };
    };
    let delta = offset as f64;
    let threshold = if preceding {
        current - delta
    } else {
        current + delta
    };
    let region = &numeric.keys[numeric.lo..numeric.hi];
    if is_start {
        // Smallest index with key >= threshold.
        let count = region.partition_point(|k| k.expect("within [lo, hi)") < threshold);
        (numeric.lo + count) as i64
    } else {
        // Largest index with key <= threshold, one past it minus one.
        let count = region.partition_point(|k| k.expect("within [lo, hi)") <= threshold);
        (numeric.lo + count) as i64 - 1
    }
}

/// The frame for the row at `position` (0-based, within a partition of `n`
/// rows), as an inclusive `(first, last)` pair of indices into the
/// partition's sequence — `None` when the frame is empty.
///
/// Emptiness is decided from the *unclamped* positions first — a frame like
/// `2 PRECEDING AND 5 PRECEDING` is empty at *every* row (the start is always
/// later than the end), which clamping each bound independently to the
/// partition would hide; only once a frame is known non-empty are its bounds
/// clamped into `0..n`. Confirmed against sqlite3 (see the sqllogictest
/// file's frame-past-the-edge cases).
fn frame_range(
    ctx: &FrameContext,
    start: &ResolvedBound,
    end: &ResolvedBound,
    position: usize,
    n: usize,
) -> Option<(usize, usize)> {
    let raw_start = bound_position(ctx, start, position, true);
    let raw_end = bound_position(ctx, end, position, false);
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
fn sort_rows(
    mut rows: Vec<ExecRow>,
    order: &[Order],
    env: &Env<'_>,
    interrupt: &Interrupt,
) -> Result<Vec<ExecRow>> {
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
        // The key-building pass, not the comparator below: this is where an
        // `ORDER BY` expression is evaluated (once per row), and it is the half
        // that scales with what the expression costs. The comparator only ever
        // compares keys that are already values, so an `O(n log n)` run of it
        // over an input `collect_bounded` has already capped is bounded work
        // that would cost more to interrupt than to finish.
        interrupt.check()?;
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
pub(crate) fn row_count(expr: Option<&crate::plan::Expr>, env: &Env<'_>) -> Result<Option<usize>> {
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
    interrupt: &Interrupt,
) -> Result<Vec<ExecRow>> {
    let mut projected = Vec::with_capacity(rows.len());
    for row in &rows {
        interrupt.check()?;
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
            // Two `INTEGER`s collide only when they are the same 64-bit
            // integer. Comparing them through `f64` — which is what this did —
            // makes every pair above 2^53 look identical, because an `f64`
            // cannot represent consecutive integers up there. The symptom is a
            // `UNIQUE` constraint refusing a row whose key is genuinely new:
            // insert an external id of 2^53, then one of 2^53 + 1, and the
            // second was rejected as a duplicate of the first.
            (Value::Integer(a), Value::Integer(b)) => a == b,
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
/// A `GROUP BY` key, ordered the way [`compare_group_keys`] orders one.
///
/// Carries its collations because [`Ord`] takes no context and grouping is a
/// collation question: `'Ada'` and `'ADA'` are one group under `NOCASE` and two
/// under `BINARY`. Every key in one query shares the same slice, so this is a
/// refcount bump per group rather than a copy per row.
/// One row's cells as the streamed aggregate reads them.
///
/// Two homes for a cell, one fold: owned `Value`s off an already-decoded
/// stream, or `ValueRef`s borrowed from the row's own bytes. The fold asks
/// only these two things of a row — evaluate an expression against it, and
/// materialise it if it turns out to be the first of its group — so
/// `Engine::stream_aggregate`'s loop body is written once and never learns
/// which it was handed.
trait AggregateCells {
    /// Evaluate `expr` against this row, with no aggregates or windows
    /// computed yet — the fold runs before either exists.
    fn eval(&self, expr: &crate::plan::Expr, env: &Env<'_>) -> Result<Value>;
    /// This row as owned values: the group representative.
    fn to_owned_row(&self) -> Vec<Value>;
}

impl AggregateCells for [Value] {
    fn eval(&self, expr: &crate::plan::Expr, env: &Env<'_>) -> Result<Value> {
        // A bare column — the common `GROUP BY n` and `SUM(n)` — is read
        // straight off the row here; the general evaluator's answer is the
        // same, this just spares its call and dispatch per row per
        // expression. It is not spared the bounds check: an ordinal past the
        // row is the same corruption whichever way it is read.
        if let crate::plan::Expr::Column(index) = expr {
            if let Some(value) = self.get(*index) {
                return Ok(value.clone());
            }
        }
        eval::evaluate(expr, self, Computed::NONE, env)
    }

    fn to_owned_row(&self) -> Vec<Value> {
        self.to_vec()
    }
}

impl AggregateCells for [ValueRef<'_>] {
    fn eval(&self, expr: &crate::plan::Expr, env: &Env<'_>) -> Result<Value> {
        if let crate::plan::Expr::Column(index) = expr {
            if let Some(cell) = self.get(*index) {
                return Ok(cell.to_owned_value());
            }
        }
        eval::evaluate_ref(expr, self, Computed::NONE, env)
    }

    fn to_owned_row(&self) -> Vec<Value> {
        self.iter().map(ValueRef::to_owned_value).collect()
    }
}

struct GroupKey {
    values: Vec<Value>,
    collations: Rc<[Collation]>,
}

impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        compare_group_keys(&self.values, &other.values, &self.collations)
            == core::cmp::Ordering::Equal
    }
}

impl Eq for GroupKey {}

/// The hash of a `GROUP BY` key, agreeing with [`compare_group_keys`]: two
/// keys that compare `Equal` hash the same.
///
/// That agreement is the whole contract, and every arm below is one case of
/// it. Numbers compare across classes — `1` and `1.0` are one group — so an
/// integer hashes through the same `f64` funnel a real does, with the sign
/// of zero normalised (`-0.0 == 0.0`) and every `NaN` folded to one pattern.
/// The funnel is lossy above 2^53 exactly as the comparison is, which is the
/// point: a hash more precise than the comparison would put equal keys in
/// different groups. Text hashes what its collation *compares*, through
/// `Collation::fold`, so `'Ada'` and `'ADA'` are one bucket under `NOCASE`
/// and `'a'` and `'a  '` are under `RTRIM`. A vector compares by length
/// alone, so it hashes by length alone. `NULL`s are one group.
///
/// One case has no consistent answer: the comparison calls `NaN` equal to
/// every number, which no equivalence — and no hash — can honour. Under the
/// ordered map that made a `NaN` key's group depend on insertion order;
/// here a `NaN` forms its own group unless it collides. Both are arbitrary;
/// this one is at least stable.
fn hash_group_key(values: &[Value], collations: &[Collation]) -> u64 {
    const NULL: u64 = 0x9E37_79B9_7F4A_7C15;
    const TEXT: u64 = 0x2545_F491_4F6C_DD1D;
    const BLOB: u64 = 0x6A09_E667_F3BC_C909;
    const VECTOR: u64 = 0xBB67_AE85_84CA_A73B;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (position, value) in values.iter().enumerate() {
        let collation = crate::collation::at(collations, position);
        let part = match value {
            Value::Null => NULL,
            Value::Integer(integer) => numeric_bits(*integer as f64),
            Value::Real(real) => numeric_bits(*real),
            Value::Text(text) => fnv1a(&collation.fold(text.as_bytes())) ^ TEXT,
            Value::Blob(bytes) => fnv1a(bytes) ^ BLOB,
            Value::Vector(vector) => (vector.len() as u64) ^ VECTOR,
        };
        hash = mix64(hash ^ part);
    }
    hash
}

/// The bit pattern a number hashes by: sign of zero normalised, `NaN`
/// canonical. See [`hash_group_key`].
fn numeric_bits(real: f64) -> u64 {
    if real.is_nan() {
        f64::NAN.to_bits()
    } else {
        (real + 0.0).to_bits()
    }
}

/// Groups found by hash and confirmed by [`compare_group_keys`], iterated in
/// the order they were opened.
///
/// Open addressing over a bucket array of entry indices, linear probing, load
/// held at or under one half; entries live in a `Vec` in insertion order and
/// are never removed, so there is no deletion to get right. The stored hash
/// is compared before the key is, so a full key comparison happens once per
/// hit and almost never per collision. `alloc` only, no `unsafe`.
///
/// The profile that motivated this (`PERF.md`, 2026-09-02): the ordered map's
/// `get_mut` was 10% of a `GROUP BY` over 100k rows in 100 groups, with the
/// `mem_cmp` beneath it another 5% — seven key comparisons per row where one
/// suffices.
struct GroupTable<V> {
    /// Entry index per bucket, or [`GROUP_EMPTY`].
    buckets: Vec<u32>,
    /// `(hash, key, value)` in the order the groups were opened.
    entries: Vec<(u64, GroupKey, V)>,
}

/// A vacant bucket.
const GROUP_EMPTY: u32 = u32::MAX;

impl<V> GroupTable<V> {
    fn new() -> Self {
        Self {
            buckets: Vec::new(),
            entries: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    fn bucket_of(&self, hash: u64) -> usize {
        // The hash is already mixed; take the top bits, which are the ones
        // the last multiply spread the most.
        let bits = self.buckets.len().trailing_zeros();
        (hash >> (64 - bits)) as usize
    }

    /// The entry holding `probe`, or the vacant bucket it would go in.
    ///
    /// The `Err` bucket is only valid until the next insertion, which may
    /// grow the table; [`GroupTable::insert_at`] re-derives it in that case.
    fn find(&self, hash: u64, probe: &GroupKey) -> core::result::Result<usize, usize> {
        if self.buckets.is_empty() {
            return Err(0);
        }
        let mask = self.buckets.len() - 1;
        let mut at = self.bucket_of(hash);
        loop {
            let index = self.buckets[at];
            if index == GROUP_EMPTY {
                return Err(at);
            }
            let (stored_hash, key, _) = &self.entries[index as usize];
            if *stored_hash == hash
                && compare_group_keys(&probe.values, &key.values, &probe.collations)
                    == core::cmp::Ordering::Equal
            {
                return Ok(index as usize);
            }
            at = (at + 1) & mask;
        }
    }

    /// Open a group at the bucket [`GroupTable::find`] returned, or wherever
    /// it lands after the table has grown. Returns the entry index.
    fn insert_at(&mut self, bucket: usize, hash: u64, key: GroupKey, value: V) -> usize {
        let index = self.entries.len();
        let bucket = if self.buckets.is_empty() || (index + 1) * 2 > self.buckets.len() {
            self.grow();
            self.vacant_bucket(hash)
        } else {
            bucket
        };
        self.buckets[bucket] = index as u32;
        self.entries.push((hash, key, value));
        index
    }

    fn vacant_bucket(&self, hash: u64) -> usize {
        let mask = self.buckets.len() - 1;
        let mut at = self.bucket_of(hash);
        while self.buckets[at] != GROUP_EMPTY {
            at = (at + 1) & mask;
        }
        at
    }

    fn grow(&mut self) {
        let capacity = (self.buckets.len() * 2).max(64);
        self.buckets = alloc::vec![GROUP_EMPTY; capacity];
        for (index, (hash, _, _)) in self.entries.iter().enumerate() {
            let at = self.vacant_bucket(*hash);
            self.buckets[at] = index as u32;
        }
    }

    fn value_mut(&mut self, index: usize) -> &mut V {
        &mut self.entries[index].2
    }

    fn into_values(self) -> impl Iterator<Item = V> {
        self.entries.into_iter().map(|(_, _, value)| value)
    }
}

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

/// The most output rows [`project_stream`] will reserve on a `LIMIT`'s word
/// alone.
///
/// A `LIMIT` bounds the answer without describing it: `LIMIT 1000000` over a
/// three-row table would otherwise reserve for a million rows and return
/// three. Trusting it up to a page's worth puts every page a client actually
/// asks for in one allocation, and caps the cost of a wrong guess at tens of
/// kilobytes rather than tens of megabytes. Past this the vector doubles the
/// way it always did.
const PROJECTION_ROWS_RESERVED: usize = 1024;

/// How many output rows to reserve for a stream bounded by `limit`.
///
/// An unbounded query reserves nothing: guessing the size of a scan's answer
/// is cardinality estimation's job, and a guess made here would be wrong in
/// the one direction that costs memory rather than time.
fn reserved_rows(limit: Option<usize>) -> usize {
    limit.unwrap_or(0).min(PROJECTION_ROWS_RESERVED)
}

/// [`project`] for a query whose rows stream straight out of the pipeline:
/// the same projection, but consuming a [`RowStream`] rather than an already
/// collected `Vec<ExecRow>`, so a non-blocking query never materialises the
/// intermediate `ExecRow`s. The answer is identical — see [`Engine::run_select`]
/// for why skipping the empty-`ORDER BY` re-sort is safe.
///
/// `limit` is the caller's `LIMIT`, already applied to `stream`, and is used
/// only to size the output vector — see [`reserved_rows`]. It cannot change
/// which rows come out, because the rows it would have to drop have already
/// been dropped by the `take` on the stream.
///
/// The per-row projection is [`moving_projection`]'s, the same as the
/// collected path's: a plain column list moves each value out of the row
/// rather than cloning it, and only a projection with an expression in it
/// falls back to [`project_row`].
fn project_stream(
    items: &[SelectItem],
    stream: RowStream<'_>,
    env: &Env<'_>,
    limit: Option<usize>,
) -> Result<ResultSet> {
    let columns = items.iter().map(|item| item.label().to_string()).collect();
    let mut out_rows = Vec::with_capacity(reserved_rows(limit));
    match moving_projection(items) {
        Some(moves) => {
            for row in stream {
                let mut row = row?;
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
            for row in stream {
                let row = row?;
                out_rows.push(project_row(items, &row, env)?);
            }
        }
    }
    Ok(ResultSet {
        columns,
        rows: out_rows,
    })
}

/// Project a non-blocking stream into one reusable output row.
///
/// Unlike [`project_stream`], this does not transfer each row into a retained
/// outer `Vec`: the consumer must finish with the slice before it returns, and
/// the allocation is cleared and reused for the next row.
fn project_stream_to(
    items: &[SelectItem],
    stream: RowStream<'_>,
    env: &Env<'_>,
    sink: &mut dyn FnMut(&[Value]) -> Result<()>,
) -> Result<()> {
    let mut values = Vec::with_capacity(items.len());
    match moving_projection(items) {
        Some(moves) => {
            for row in stream {
                let mut row = row?;
                values.clear();
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
                sink(&values)?;
            }
        }
        None => {
            for row in stream {
                let row = row?;
                values.clear();
                for item in items {
                    values.push(project_one(item, &row, env)?);
                }
                sink(&values)?;
            }
        }
    }
    Ok(())
}

/// Project a joined row borrowed from [`NestedLoopJoin`]'s reusable buffer.
fn project_borrowed_row(
    items: &[SelectItem],
    values: &[Value],
    score: Option<f32>,
    env: &Env<'_>,
    out: &mut Vec<Value>,
) -> Result<()> {
    out.clear();
    for item in items {
        out.push(match item {
            SelectItem::Column { index, .. } => values.get(*index).cloned().unwrap_or(Value::Null),
            SelectItem::Expr { expr, .. } => eval::evaluate(expr, values, Computed::NONE, env)?,
            SelectItem::Score { .. } => score
                .map(|score| Value::Real(f64::from(score)))
                .unwrap_or(Value::Null),
        });
    }
    Ok(())
}

/// Project direct columns from a hash pair without first concatenating it.
fn project_split_row(
    items: &[SelectItem],
    outer: &[Value],
    inner: Option<&[Value]>,
    score: Option<f32>,
    out: &mut Vec<Value>,
) -> Result<()> {
    out.clear();
    for item in items {
        out.push(match item {
            SelectItem::Column { index, .. } if *index < outer.len() => outer[*index].clone(),
            SelectItem::Column { index, .. } => inner
                .and_then(|inner| inner.get(*index - outer.len()))
                .cloned()
                .unwrap_or(Value::Null),
            SelectItem::Score { .. } => score
                .map(|score| Value::Real(f64::from(score)))
                .unwrap_or(Value::Null),
            // The caller admits only direct projections. Fail loudly if that
            // guard ever drifts instead of manufacturing a `NULL` result.
            SelectItem::Expr { .. } => {
                return Err(Error::Unsupported(
                    "internal split-row projection received an expression".to_string(),
                ))
            }
        });
    }
    Ok(())
}

/// One projected value, shared by the reusable-row callback path.
fn project_one(item: &SelectItem, row: &ExecRow, env: &Env<'_>) -> Result<Value> {
    Ok(match item {
        SelectItem::Column { index, .. } => row.values.get(*index).cloned().unwrap_or(Value::Null),
        SelectItem::Expr { expr, .. } => eval::evaluate(
            expr,
            &row.values,
            Computed {
                aggregates: &row.aggregates,
                windows: &row.windows,
            },
            env,
        )?,
        SelectItem::Score { .. } => row
            .score
            .map(|score| Value::Real(f64::from(score)))
            .unwrap_or(Value::Null),
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
/// Whether [`Engine::run_query_each_ref`] can answer this `SELECT` without
/// materialising a row, filling `projection` with which column of the decoded
/// row each output cell comes from when it can.
///
/// `projection` is a buffer the handle keeps
/// ([`Engine::borrow_scratch`]) rather than a returned `Vec`, so deciding this
/// costs no allocation on a point read — which is one query per row and would
/// otherwise pay for the decision as often as for the answer.
///
/// The admitted shape is one stored table projected as bare columns. The
/// exclusions are not arbitrary — each one names something the borrowed cells
/// cannot survive:
///
/// * **`ORDER BY`, `GROUP BY`/aggregates, windows, `DISTINCT`.** Every one of
///   them has to see the last input row before it can emit the first output
///   row, so the rows have to be held; a cell borrowed from a page cannot be,
///   because the scan moves on. These materialise, and say so.
/// * **A join, a derived table, a scored retrieval.** Their rows are assembled
///   from more than one source, and the assembly is where an owned row already
///   exists. `run_single_join_to` is the borrowed-row join, and it borrows a
///   reusable `Vec<Value>` rather than page bytes.
/// * **A `WITHOUT ROWID` table.** Its scan is a different source
///   ([`Engine::without_rowid_stream`]) that yields decoded rows.
/// * **A projection with an expression in it.** `a + 1` and `upper(body)`
///   produce a *new* value that has to live somewhere; borrowing it would mean
///   borrowing from a buffer this function would have to invent, which is the
///   owned path with extra steps.
///
/// Anything excluded here still answers — [`Engine::run_query_each_ref`] falls
/// back to the owned pipeline — so this is a performance decision, never a
/// correctness one, which is what
/// `the_borrowing_path_ties_the_owned_one_row_for_row` checks across both
/// sides of every one of these conditions.
fn borrowed_projection(plan: &SelectPlan, projection: &mut Vec<usize>) -> bool {
    projection.clear();
    if !plan.joins.is_empty() || plan.from.len() != 1 {
        return false;
    }
    let driving = &plan.from[0];
    if driving.derived.is_some() || plan.score.is_some() || driving.table.without_rowid {
        return false;
    }
    if !plan.group_by.is_empty()
        || !plan.aggregates.is_empty()
        || !plan.windows.is_empty()
        || plan.distinct
        || !plan.order.is_empty()
    {
        return false;
    }
    for item in &plan.items {
        match item {
            SelectItem::Column { index, .. } => projection.push(*index),
            SelectItem::Expr { .. } | SelectItem::Score { .. } => {
                projection.clear();
                return false;
            }
        }
    }
    true
}

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

    /// Every pair `compare_group_keys` calls `Equal` hashes the same — the
    /// one contract `hash_group_key` has — checked on the pairs where the
    /// two could most plausibly disagree.
    #[test]
    fn group_key_hash_agrees_with_group_key_comparison() {
        use crate::value::Text;
        let text = |s: &str| Value::Text(Text::from(s));
        let same = |a: Value, b: Value, collation: Collation| {
            let collations = [collation];
            assert_eq!(
                compare_group_keys(
                    core::slice::from_ref(&a),
                    core::slice::from_ref(&b),
                    &collations
                ),
                core::cmp::Ordering::Equal,
                "{a:?} vs {b:?} should compare equal"
            );
            assert_eq!(
                hash_group_key(core::slice::from_ref(&a), &collations),
                hash_group_key(core::slice::from_ref(&b), &collations),
                "{a:?} vs {b:?} compare equal but hash differently"
            );
        };
        let differ = |a: Value, b: Value, collation: Collation| {
            let collations = [collation];
            assert_ne!(
                compare_group_keys(
                    core::slice::from_ref(&a),
                    core::slice::from_ref(&b),
                    &collations
                ),
                core::cmp::Ordering::Equal
            );
            assert_ne!(
                hash_group_key(core::slice::from_ref(&a), &collations),
                hash_group_key(core::slice::from_ref(&b), &collations),
                "{a:?} vs {b:?} should not collide"
            );
        };
        same(Value::Integer(1), Value::Real(1.0), Collation::Binary);
        same(Value::Real(0.0), Value::Real(-0.0), Collation::Binary);
        same(Value::Integer(0), Value::Real(-0.0), Collation::Binary);
        same(Value::Null, Value::Null, Collation::NoCase);
        same(
            Value::Integer(1 << 53 | 1),
            Value::Real((1u64 << 53) as f64),
            Collation::Binary,
        );
        same(text("Ada"), text("ADA"), Collation::NoCase);
        same(text("a"), text("a   "), Collation::RTrim);
        same(
            Value::Vector(alloc::vec![1.0, 2.0]),
            Value::Vector(alloc::vec![3.0, 4.0]),
            Collation::Binary,
        );
        same(
            Value::Real(f64::NAN),
            Value::Real(-f64::NAN),
            Collation::Binary,
        );
        differ(text("Ada"), text("ADA"), Collation::Binary);
        differ(text("a"), text("a "), Collation::Binary);
        differ(text("1"), Value::Integer(1), Collation::Binary);
        differ(text("ab"), Value::Blob(b"ab".to_vec()), Collation::Binary);
        differ(Value::Integer(1), Value::Integer(2), Collation::Binary);
        // Positional: the same values in the other order are another key.
        let collations = [Collation::Binary, Collation::Binary];
        assert_ne!(
            hash_group_key(&[Value::Integer(1), Value::Integer(2)], &collations),
            hash_group_key(&[Value::Integer(2), Value::Integer(1)], &collations)
        );
    }

    /// The table finds what it stored across growth, keeps first-seen order,
    /// and never confuses two keys whose hashes collide.
    #[test]
    fn the_group_table_survives_growth_and_hash_collisions() {
        let collations: Rc<[Collation]> = [Collation::NoCase].as_slice().into();
        let key = |n: i64| GroupKey {
            values: alloc::vec![Value::Integer(n)],
            collations: Rc::clone(&collations),
        };
        let mut table: GroupTable<i64> = GroupTable::new();
        for n in 0..5_000 {
            // Deliberately only 16 distinct hashes, so probe runs are long
            // and the key comparison is what separates entries.
            let probe = key(n % 1_000);
            let hash = ((n % 1_000) % 16) as u64;
            match table.find(hash, &probe) {
                Ok(index) => *table.value_mut(index) += 1,
                Err(bucket) => {
                    table.insert_at(bucket, hash, probe, 1);
                }
            }
        }
        assert_eq!(table.len(), 1_000);
        for n in 0..1_000 {
            let index = table
                .find((n % 16) as u64, &key(n))
                .expect("every key was inserted");
            assert_eq!(table.entries[index].2, 5);
        }
        let opened: Vec<i64> = table
            .entries
            .iter()
            .map(|(_, k, _)| match k.values[0] {
                Value::Integer(n) => n,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            opened,
            (0..1_000).collect::<Vec<_>>(),
            "not first-seen order"
        );
        assert_eq!(table.into_values().sum::<i64>(), 5_000);
    }
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cmp::Ordering;

    fn text(s: &str) -> Value {
        Value::Text(s.to_string().into())
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

    /// The projection's capacity hint trusts a `LIMIT` only as far as it is
    /// safe to: a page's worth, never the number a client happened to write.
    ///
    /// A hint is not an answer — the result is whatever the stream yields, and
    /// `a_limit_stops_the_scan_rather_than_truncating_the_answer` in
    /// `streaming.rs` is what pins that. This pins the arithmetic, including
    /// the two ends that would otherwise be found by an out-of-memory report:
    /// no `LIMIT` reserves nothing, and an absurd one reserves the cap.
    #[test]
    fn a_limit_reserves_a_page_at_most() {
        assert_eq!(reserved_rows(None), 0, "an unbounded query guesses nothing");
        assert_eq!(reserved_rows(Some(0)), 0);
        assert_eq!(reserved_rows(Some(20)), 20, "a page-sized LIMIT is trusted");
        assert_eq!(
            reserved_rows(Some(PROJECTION_ROWS_RESERVED)),
            PROJECTION_ROWS_RESERVED
        );
        assert_eq!(
            reserved_rows(Some(usize::MAX)),
            PROJECTION_ROWS_RESERVED,
            "a LIMIT larger than any answer must not be reserved for"
        );
    }
}
