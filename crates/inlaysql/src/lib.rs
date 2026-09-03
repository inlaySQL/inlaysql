//! InlaySQL — an embedded SQL database with first-class hybrid retrieval.
//!
//! ```no_run
//! use inlaysql::{Database, Value};
//!
//! let mut db = Database::open("app.inlay")?;
//! db.execute(
//!     "CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR(384))",
//!     &[],
//! )?;
//! db.execute("CREATE INDEX docs_body ON docs (body)", &[])?;
//! db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])?;
//! db.execute(
//!     "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
//!     &[
//!         Value::Integer(1),
//!         Value::Text("embedded database written in rust".into()),
//!         Value::Vector(vec![0.0; 384]),
//!     ],
//! )?;
//!
//! // One statement, two retrievers, one fused ranking.
//! let results = db.query(
//!     "SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
//!      FROM docs ORDER BY score DESC LIMIT 5",
//!     &[Value::Vector(vec![0.0; 384]), Value::Text("rust database".into())],
//! )?;
//! # Ok::<(), inlaysql::Error>(())
//! ```
//!
//! # How the pieces fit
//!
//! All the SQL handling lives in `inlaysql-core`, which is `no_std` and talks
//! to the outside world only through traits. This crate supplies the
//! implementations: [`TreeStorage`] (the in-house copy-on-write B+ tree with a
//! write-ahead log) for rows, [`inlaysql_core::bm25::Bm25Index`] for BM25 and
//! [`inlaysql_core::hnsw::HnswIndex`] for approximate nearest neighbours.
//! [`RedbStorage`] remains available for comparison and benchmarks.
//!
//! # What a database is on disk
//!
//! One file, laid out as `inlaysql_core`'s header + write-ahead log + B-tree
//! pages. The full-text and vector indexes live in that same file: they are
//! written back on [`Database::checkpoint`] and restored on open, stamped with
//! the write version they describe so a stale one is rebuilt rather than
//! trusted.
//!
//! Because the tree is copy-on-write, a committed root is an immutable
//! snapshot — so [`Database::backup_to`] copies one out to another file while
//! writers keep committing, and [`vacuum()`] compacts one by rebuilding it.
//! The two are not the same operation: see [`mod@backup`] for which is which.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Doc comments here explain the implementation to whoever is reading the
// source, so they link to private items on purpose: `[`CommitCoordinator`]`
// is the thing the sentence is about, whether or not a docs.rs reader can
// click it. Rustdoc's default is to reject those links in the docs of a
// public item, which would mean either deleting the reference or promoting
// an internal type to keep a sentence readable. Allowed instead; every other
// rustdoc lint stays denied; `AGENTS.md` documents the gate that runs them.
#![allow(rustdoc::private_intra_doc_links)]

pub mod asyncio;
mod backup;
mod device;
pub mod sqllogictest;
mod statement;
mod storage;
mod vacuum;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use inlaysql_core::btree::Device;
use inlaysql_core::mem::MemStorage;
use inlaysql_core::{Clock, Engine, FullTextIndex, IndexFactory, VectorIndex};

pub use inlaysql_core::EngineOptions;

pub use asyncio::{block_on, AsyncDatabase, Task};
pub use backup::{backup, BackupOutcome, BackupSummary, SourceAccess};
pub use device::{CommitStats, FileDevice};
pub use inlaysql_core::bm25::Bm25Index;
pub use inlaysql_core::bm25_paged::PagedBm25Index;
pub use inlaysql_core::btree::Diagnostics;
/// The stand-in embedder lives in the core because every build has to agree on
/// it byte for byte — the WASM module in a browser tab has to bucket trigrams
/// exactly as the CLI that seeded the file did. Re-exported here so
/// `inlaysql::embedding::hashed_embedding` keeps working.
pub use inlaysql_core::embedding;
pub use inlaysql_core::hnsw::{HnswIndex, VectorMetric};
pub use inlaysql_core::TreeStorage;
pub use inlaysql_core::{
    is_reserved_table_name, Cancel, Catalog, Change, ChangeKind, Changes, Collation, Column,
    ColumnInfo, DataType, Durability, Error, Index, IndexKind, Outcome, Reindexed, Result,
    ResultSet, Stopped, Table, TableAccess, Value, ValueRef, VectorTuning, RESERVED_TABLE_PREFIX,
};
pub use statement::Statement;
pub use storage::RedbStorage;
pub use vacuum::vacuum;

/// The most bytes one commit's write-ahead-log record may hold, for the page
/// size every database this crate opens or creates actually uses.
///
/// Not a separately maintained number: it is
/// [`inlaysql_core::wal::max_record_len`] evaluated at
/// [`inlaysql_core::btree::DEFAULT_PAGE_SIZE`] — the exact same formula
/// `CowBTree::commit` measures a transaction's encoded record against before
/// writing it, and the exact same page size every `Database::open*`
/// constructor hands `TreeStorage`. A caller sizing a batch import against any
/// other number is sizing it against a ceiling nothing here enforces; this is
/// the one that does. See `docs/enterprise-readiness.md` blocker 5 for why the
/// ceiling itself (one WAL region, ~1 MiB by default) is a deliberate,
/// load-bearing constant rather than something to raise.
pub fn max_transaction_bytes() -> usize {
    inlaysql_core::wal::max_record_len(inlaysql_core::btree::DEFAULT_PAGE_SIZE)
}

/// An open database.
pub struct Database {
    engine: Engine,
    /// Set only by [`Database::open_read_only`]. Every write is refused
    /// before it reaches the engine — see [`Database::check_writable`].
    read_only: bool,
}

impl Database {
    /// Open the database file at `path`, creating it if it does not exist.
    ///
    /// Rows live in the in-house copy-on-write B+ tree with a write-ahead log
    /// (see `inlaysql_core::btree`), so a database is one file that survives
    /// crashes. Indexes are rebuilt from the stored rows as part of opening.
    ///
    /// Retrieval indexes are explicit: a `TEXT` or `VECTOR` column is indexed
    /// only where a `CREATE INDEX` declared it (or where a database written
    /// before `CREATE INDEX` existed was grandfathered). Use
    /// [`Database::open_implicit`] to restore the old index-everything
    /// behaviour.
    /// `":memory:"` is refused rather than quietly taken as a filename — see
    /// the error text for why.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        // SQLite spells an in-memory database `":memory:"`, and anyone porting
        // from it — or from `rusqlite` — writes that first. Treating it as an
        // ordinary path is legal on every filesystem this runs on, so it
        // silently produced a real file called `:memory:` on disk: durable
        // where the caller wanted ephemeral, in the working directory, and
        // invisible until someone noticed a stray file. Refusing is this
        // project's rule for a construct it does not mean what the caller
        // thinks (`docs/architecture.md`: refuse, never ignore), and it is the
        // only response that names the call the caller actually wanted.
        if path.as_os_str() == ":memory:" {
            return Err(Error::Unsupported(
                "`:memory:` is not a path here — call `Database::open_in_memory()` \
                 for an in-memory database. Opening it as a file would have \
                 created one named `:memory:` on disk."
                    .to_string(),
            ));
        }
        Self::open_on(FileDevice::open(path)?)
    }

    /// Open the database file at `path` for reading only.
    ///
    /// Takes **no OS advisory lock**, unlike [`Database::open`] — see
    /// [`FileDevice::open_read_only`] for why that is safe and what it costs.
    /// That is what restores the workflow `docs/mcp.md` describes: a second
    /// process (an agent, a CLI) can hold this beside an application that has
    /// the same file open for writing.
    ///
    /// The file must already exist. Unlike [`Database::open`], this never
    /// creates one — a path that does not exist is [`Error::Storage`], not a
    /// silently empty database, because a typo'd path and an intentional new
    /// database should not look the same. Opening a file whose write-ahead
    /// log needs replay to read cleanly is also an error here: recovery is a
    /// write, and this handle cannot perform one.
    ///
    /// Every statement that would write is refused before it touches
    /// storage — see [`Database::is_read_only`], which this uses to decide.
    ///
    /// # Cost
    ///
    /// A read-only handle has no in-process proof that it is the only writer
    /// (because it never locks one out), so it cannot answer "did anything
    /// change?" without asking the file: every statement pays the full
    /// write-ahead-log scan that [`inlaysql_core::btree::CowBTree::refresh`]
    /// falls back to when [`inlaysql_core::btree::Device::commit_generation`]
    /// answers `None` — measured at roughly 236 µs, against roughly 7 µs for
    /// a read-write handle's fast path. `docs/mcp.md` documents this trade for
    /// the MCP server, which opens read-only by default.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let mut database = Self::open_on(FileDevice::open_read_only(path)?)?;
        database.read_only = true;
        Ok(database)
    }

    /// Open a database on an explicit I/O backend.
    ///
    /// A [`Device`] is the engine's whole view of durable storage: byte
    /// offsets, reads, writes and a sync. Pass [`FileDevice`] for ordinary
    /// blocking file I/O, an `io_uring`-backed device on Linux, or anything
    /// else that can answer those four questions.
    ///
    /// ```no_run
    /// use inlaysql::{Database, FileDevice};
    ///
    /// let db = Database::open_on(FileDevice::open("app.inlay")?)?;
    /// # Ok::<(), inlaysql::Error>(())
    /// ```
    pub fn open_on<D: Device + 'static>(device: D) -> Result<Self> {
        Self::open_on_with(device, EngineOptions::default())
    }

    /// Open a database whose vector indexes live in the file rather than in
    /// memory.
    ///
    /// The default [`HnswIndex`] holds every embedding and the whole graph in
    /// RAM — about twice the corpus bytes before the graph — which puts a
    /// ceiling on how large a corpus can be indexed at all. This opens
    /// [`inlaysql_core::hnsw_paged::PagedHnswIndex`] instead: the graph is
    /// stored as ordinary rows in the same database file, read through a
    /// bounded cache, and written inside the engine's own transaction, so a
    /// commit makes the rows and the index that describes them durable together
    /// and a crash cannot leave one without the other.
    ///
    /// It is also what makes opening cheap: a paged index is already in the
    /// file, so it does not have to be rebuilt from the rows or reloaded from a
    /// saved blob. The cost is per search — a cache miss is a read from the
    /// file rather than a pointer chase.
    ///
    /// The file format is unchanged; a database can be opened either way.
    pub fn open_paged(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_on_with(
            FileDevice::open(path)?,
            EngineOptions {
                paged_vector_indexes: true,
                ..EngineOptions::default()
            },
        )
    }

    /// Open a database that indexes every `TEXT` and `VECTOR` column of every
    /// table it creates, as it did before `CREATE INDEX` existed.
    ///
    /// This is the pre-`CREATE INDEX` behaviour kept available for the demo and
    /// for databases that want automatic indexing. The choice is recorded as
    /// ordinary index declarations in the catalog at `CREATE TABLE` time.
    pub fn open_implicit(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_on_with(
            FileDevice::open(path)?,
            EngineOptions {
                implicit_indexes: true,
                ..EngineOptions::default()
            },
        )
    }

    /// Open a database on an explicit I/O backend with an explicit choice for
    /// every engine option. See [`EngineOptions`].
    pub fn open_on_with_options<D: Device + 'static>(
        device: D,
        options: EngineOptions,
    ) -> Result<Self> {
        Self::open_on_with(device, options)
    }

    fn open_on_with<D: Device + 'static>(device: D, options: EngineOptions) -> Result<Self> {
        // The page cache and the free-list choice both belong to the tree,
        // which is built before the engine, so they are applied here rather
        // than inside `Engine`.
        let storage = TreeStorage::open_on_with_options(
            device,
            options.page_cache_bytes,
            options.page_reuse,
            options.durability,
            options.commit_absorption,
        )?;
        Ok(Self {
            engine: Engine::open_with_options(
                Box::new(storage),
                Box::new(NativeIndexFactory),
                Box::new(SystemClock),
                options,
            )?,
            read_only: false,
        })
    }

    /// Open a database that never touches the filesystem.
    ///
    /// Useful for tests and for short-lived caches. The same query engine and
    /// the same index implementations are used; only the row store differs.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Self {
            engine: Engine::open(
                Box::new(MemStorage::new()),
                Box::new(NativeIndexFactory),
                Box::new(SystemClock),
            )?,
            read_only: false,
        })
    }

    /// Run a statement, binding `?` placeholders from `params` in order.
    ///
    /// The statement is parsed and planned on every call. For anything that
    /// runs more than once, [`Database::prepare`] it instead.
    ///
    /// On a handle from [`Database::open_read_only`], a statement that would
    /// write is refused with [`Error::Storage`] before anything runs — see
    /// [`Database::check_writable`].
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        self.check_writable(sql, params)?;
        self.engine.execute(sql, params)
    }

    /// Run a statement that returns rows.
    ///
    /// Subject to the same read-only refusal as [`Database::execute`] — a
    /// statement handed to `query` is ordinarily a `SELECT`, but nothing
    /// prevents a caller from passing a write here instead, and the engine
    /// would run it.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<ResultSet> {
        self.check_writable(sql, params)?;
        self.engine.query(sql, params)
    }

    /// Parse and plan a statement once, to run it many times.
    ///
    /// Parsing and planning is most of the cost of a point read — the tree
    /// descent it asks for is a few microseconds, and working out which
    /// descent to do costs about as much again. A [`Statement`] pays that once.
    ///
    /// ```
    /// use inlaysql::{Database, Value};
    ///
    /// let mut db = Database::open_in_memory()?;
    /// db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
    /// db.execute("INSERT INTO kv VALUES (1, 'one')", &[])?;
    ///
    /// let lookup = db.prepare("SELECT body FROM kv WHERE id = ?")?;
    /// let rows = db.query_prepared(&lookup, &[Value::Integer(1)])?;
    /// assert_eq!(rows.rows, vec![vec![Value::Text("one".into())]]);
    /// # Ok::<(), inlaysql::Error>(())
    /// ```
    pub fn prepare(&self, sql: &str) -> Result<Statement> {
        Ok(Statement::new(self.engine.prepare(sql)?))
    }

    /// [`Database::prepare`], but against the committed state as it is *now*.
    ///
    /// This is what a caller that plans a statement in order to run it *once*
    /// wants — a wire protocol deciding whether it can stream the answer, for
    /// instance. [`Database::prepare`] plans against whatever this handle last
    /// read, which is right for a statement that will be kept and re-run
    /// (every execution re-validates, and [`Error::Stale`] says when to plan
    /// again) and wrong for a one-shot: a table another connection created
    /// since the last statement would be [`Error::Catalog`] "no such table"
    /// here, where [`Database::execute`] would have found it, because
    /// `execute` refreshes *before* it parses.
    ///
    /// ```
    /// use inlaysql::Database;
    ///
    /// let mut db = Database::open_in_memory()?;
    /// db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY)", &[])?;
    /// let select = db.prepare_fresh("SELECT id FROM kv")?;
    /// assert_eq!(select.columns().len(), 1);
    /// # Ok::<(), inlaysql::Error>(())
    /// ```
    pub fn prepare_fresh(&mut self, sql: &str) -> Result<Statement> {
        Ok(Statement::new(self.engine.prepare_fresh(sql)?))
    }

    /// Run a prepared statement, binding `?` placeholders from `params`.
    ///
    /// Fails with [`Error::Bind`] if the parameter count is wrong, and with
    /// [`Error::Stale`] if the table the statement was planned against has
    /// changed since — see [`Statement`] for why that check exists. On a
    /// handle from [`Database::open_read_only`], also fails with
    /// [`Error::Storage`] if the statement would write — cheaply, since a
    /// prepared [`Statement`] already knows [`Statement::is_read_only`]
    /// without planning again.
    pub fn execute_prepared(&mut self, statement: &Statement, params: &[Value]) -> Result<Outcome> {
        self.check_writable_statement(statement)?;
        self.engine.run(statement.as_core(), params)
    }

    /// Run a prepared statement that returns rows.
    ///
    /// Subject to the same read-only refusal as [`Database::execute_prepared`].
    pub fn query_prepared(&mut self, statement: &Statement, params: &[Value]) -> Result<ResultSet> {
        self.check_writable_statement(statement)?;
        self.engine.run_query(statement.as_core(), params)
    }

    /// Run a prepared query and visit each final row without retaining the
    /// whole result set. Returns the number of rows delivered.
    ///
    /// The slice is borrowed only for the callback invocation; copy values you
    /// need to keep. For a non-blocking query (`SELECT` without sorting,
    /// aggregation, windows or `DISTINCT`) the engine reuses one projected-row
    /// allocation from beginning to end. This is the appropriate API for wire
    /// protocols, exports and scans that serialise or count rows as they arrive
    /// rather than needing random access to every row afterwards.
    ///
    /// Only read-only statements are accepted. A callback can fail after some
    /// rows have been delivered; refusing writes prevents that consumer error
    /// from looking like a failed statement after a mutation already committed.
    ///
    /// ```
    /// use inlaysql::{Database, Value};
    ///
    /// let mut db = Database::open_in_memory()?;
    /// db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
    /// db.execute("INSERT INTO kv VALUES (1, 'one'), (2, 'two')", &[])?;
    /// let query = db.prepare("SELECT body FROM kv")?;
    /// let mut bodies = Vec::new();
    /// let count = db.query_prepared_each(&query, &[], |row| {
    ///     bodies.push(row[0].clone());
    ///     Ok(())
    /// })?;
    /// assert_eq!(count, 2);
    /// assert_eq!(bodies[0], Value::Text("one".into()));
    /// # Ok::<(), inlaysql::Error>(())
    /// ```
    pub fn query_prepared_each(
        &mut self,
        statement: &Statement,
        params: &[Value],
        each: impl FnMut(&[Value]) -> Result<()>,
    ) -> Result<usize> {
        self.check_writable_statement(statement)?;
        self.engine
            .run_query_each(statement.as_core(), params, each)
    }

    /// [`Database::query_prepared_each`], but the callback is handed
    /// **borrowed** cells rather than owned [`Value`]s.
    ///
    /// This is the API SQLite's `sqlite3_step`/`sqlite3_column_*` shape
    /// corresponds to: the engine steps a row into place and the caller reads
    /// the columns it wants out of it, without a copy of the row being made on
    /// the way. A [`ValueRef::Text`] is a `&str` into the page the row was
    /// decoded from; reading it, measuring it, hashing it or writing it to a
    /// socket allocates nothing at all. [`ValueRef::to_owned_value`] is the
    /// explicit copy, for the columns you actually want to keep.
    ///
    /// [`Database::query_prepared`] and [`Database::query_prepared_each`] are
    /// unchanged and still right for a caller that wants the whole answer, or
    /// wants owned values without thinking about lifetimes.
    ///
    /// A single stored table with `WHERE`, `LIMIT` and `OFFSET`, projected as
    /// bare columns, runs a pipeline that allocates nothing per row. Every
    /// other shape — `ORDER BY`, `GROUP BY`, `DISTINCT`, windows, joins,
    /// derived tables, retrieval scoring, and projections holding an
    /// expression — **falls back to the owned path** and borrows out of the
    /// row it built, because those operators cannot emit a row before they
    /// have seen the whole input. Same rows, same order, either way; only the
    /// allocations differ. See [`inlaysql_core::Engine::run_query_each_ref`].
    ///
    /// Read-only statements only, for the same reason
    /// [`Database::query_prepared_each`] refuses writes.
    ///
    /// ```
    /// use inlaysql::{Database, Value};
    ///
    /// let mut db = Database::open_in_memory()?;
    /// db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
    /// db.execute("INSERT INTO kv VALUES (1, 'one'), (2, 'two')", &[])?;
    /// let query = db.prepare("SELECT id, body FROM kv WHERE id >= ?")?;
    ///
    /// // Reading every row's text costs no allocation at all.
    /// let mut bytes = 0usize;
    /// let rows = db.query_prepared_each_ref(&query, &[Value::Integer(1)], |row| {
    ///     bytes += row[1].as_str().map_or(0, str::len);
    ///     Ok(())
    /// })?;
    /// assert_eq!((rows, bytes), (2, 6));
    /// # Ok::<(), inlaysql::Error>(())
    /// ```
    pub fn query_prepared_each_ref(
        &mut self,
        statement: &Statement,
        params: &[Value],
        each: impl FnMut(&[ValueRef<'_>]) -> Result<()>,
    ) -> Result<usize> {
        self.check_writable_statement(statement)?;
        self.engine
            .run_query_each_ref(statement.as_core(), params, each)
    }

    /// How many statements this handle has parsed since it was opened.
    ///
    /// Diagnostic: it is how a caller (or a test) confirms that a prepared
    /// statement really is parsed once rather than on every execution.
    pub fn statements_parsed(&self) -> u64 {
        self.engine.statements_parsed()
    }

    /// A snapshot of this handle's page-cache and device-read counters.
    ///
    /// Diagnostic, and costs nothing when unread: every counter is kept on
    /// a path that already did the work counted. `bin/profile --tail` takes
    /// one before and one after each query to say what a slow query did
    /// that a fast one did not — see [`Diagnostics`].
    pub fn diagnostics(&self) -> Diagnostics {
        self.engine.diagnostics()
    }

    /// Write the retrieval indexes into the database file now.
    ///
    /// The engine does this on its own once enough rows have changed, so a
    /// long-running database never needs it. It is worth calling explicitly
    /// after a bulk load and before closing: the difference is whether the
    /// next open restores the indexes from bytes or re-reads every row to
    /// rebuild them.
    ///
    /// A saved index is only ever a cache. It carries the write version it was
    /// taken at and is discarded on open unless that version still matches the
    /// committed data, so skipping this call costs time and never correctness.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.engine.checkpoint()
    }

    /// Build the retrieval indexes now, instead of leaving it to whichever
    /// query arrives first.
    ///
    /// Index builds are deferred: a `CREATE INDEX` and every write after it
    /// leave the work pending, and the first read that needs the index pays
    /// for all of it. That is the right trade for a database taking a row at a
    /// time, and the wrong one after a bulk load — a 1.2M-vector corpus put
    /// four and a half minutes of graph building inside one innocent `SELECT`,
    /// which is where this exists to move it to.
    ///
    /// The deferral itself is unchanged. A loader that never queries still
    /// pays nothing; this is a request, not a policy.
    ///
    /// `table` narrows the build to one table's indexes, `None` covers them
    /// all. Nothing pending is a **no-op**, reported as an empty
    /// [`Reindexed`], so calling this on a schedule costs one flag test.
    ///
    /// Stoppable through [`Database::set_cancel`], between one index and the
    /// next — see [`inlaysql_core::Engine::reindex`] for exactly where the
    /// check lands and what a stopped build leaves behind (the work still
    /// pending, and a handle that answers correctly).
    ///
    /// `REINDEX [name]` is the same thing in SQL, for a caller that has a
    /// statement string rather than a handle. Both are refused on a read-only
    /// handle, and for the same reason: building an index commits structure
    /// into the file, which is a write however it was asked for.
    pub fn reindex(&mut self, table: Option<&str>) -> Result<Reindexed> {
        if self.read_only {
            return Err(Error::Storage(
                "this database handle is open read-only; refusing to build indexes, which \
                 writes them into the file"
                    .to_string(),
            ));
        }
        self.engine.reindex(table)
    }

    /// Install the signal this handle's long loops ask before carrying on.
    ///
    /// Without one there is no statement timeout and no way to stop a running
    /// statement — the engine core is `no_std` and can neither read a clock nor
    /// be interrupted by a thread, so both have to be supplied. See
    /// [`Cancel`], and [`inlaysql_core::Engine::set_cancel`] for what the core
    /// promises about a statement that is stopped: nothing was written, and
    /// this handle stays usable.
    ///
    /// The MySQL-wire server installs one per connection, which is what
    /// `--max-execution-time` and `KILL` are built on. An embedded caller that
    /// wants a cancel button — a desktop application, a request handler with a
    /// deadline — implements the trait over an `AtomicBool` it can set from
    /// wherever the button is.
    pub fn set_cancel(&mut self, cancel: Box<dyn Cancel>) {
        self.engine.set_cancel(cancel);
    }

    /// Install the handle every vector search on this database asks for its
    /// candidate-list size. See [`VectorTuning`].
    ///
    /// For a host whose value moves — the MySQL-wire server, where any session
    /// may `SET inlaysql_hnsw_ef_search` between two statements and
    /// `@@inlaysql_hnsw_ef_search` has to report the number the *next* search
    /// will use. One handle onto the connection's own state means the reported
    /// number and the enforced one are the same load, not two copies that can
    /// drift. An embedded caller with a value that does not move wants
    /// [`Database::set_vector_ef_search`] instead.
    pub fn set_vector_tuning(&mut self, tuning: Box<dyn VectorTuning>) {
        self.engine.set_vector_tuning(tuning);
    }

    /// Pin the candidate-list size (`ef`) every vector search on this handle
    /// runs at, or `None` to leave each index's own `ef_search` in force.
    ///
    /// This is the recall/latency trade, and it is the only knob that offers
    /// it at query time: a larger `ef` holds more of the graph in the walk's
    /// beam and finds more of the true nearest neighbours, a smaller one
    /// returns sooner and finds fewer. `None` — the default, and what every
    /// query did before this existed — leaves the tuning the index was built
    /// with, which is [`inlaysql_core::hnsw::HnswParams::DEFAULT`].
    ///
    /// A value below the number of rows a query asks for is **refused when
    /// that query runs**, not clamped up to fit; see
    /// [`Database::vector_ef_search`] for reading back what is in force.
    ///
    /// `Some(0)` means `None`. Zero is not a candidate list — a walk that may
    /// hold nothing finds nothing — so the only reading that is not a
    /// contradiction is the one the MySQL server gives it, where
    /// `SET inlaysql_hnsw_ef_search = 0` is how a session goes back to the
    /// index's own tuning. Treating it as a beam of zero instead would refuse
    /// every query on this handle and tell the caller to "set it to 0".
    pub fn set_vector_ef_search(&mut self, ef: Option<usize>) {
        self.engine
            .set_vector_tuning(Box::new(FixedEfSearch(ef.filter(|ef| *ef > 0))));
    }

    /// The candidate-list size vector searches on this handle run at, or
    /// `None` when each index's own `ef_search` is in force.
    ///
    /// Read through the installed handle, so this is the same number the next
    /// search will use rather than a copy of what was last set.
    pub fn vector_ef_search(&self) -> Option<usize> {
        self.engine.vector_ef_search()
    }

    /// Whether a transaction is open right now.
    ///
    /// See [`inlaysql_core::Engine::in_transaction`] — true after
    /// [`Database::begin`], and also after a bare `SAVEPOINT` with no
    /// preceding `BEGIN`, which opens one implicitly.
    pub fn in_transaction(&self) -> bool {
        self.engine.in_transaction()
    }

    /// Start an explicit transaction.
    ///
    /// See [`inlaysql_core::Engine::begin`]. Statements between this and
    /// [`Database::commit`] are buffered into one durable commit, so a bulk
    /// load pays one `fsync` instead of one per row.
    pub fn begin(&mut self) -> Result<()> {
        self.engine.begin()
    }

    /// Commit the open transaction.
    ///
    /// See [`inlaysql_core::Engine::commit`]. A lost write race surfaces as
    /// [`Error::Conflict`].
    pub fn commit(&mut self) -> Result<()> {
        self.engine.commit()
    }

    /// Roll back the open transaction, leaving the database byte-identical to
    /// its state before [`Database::begin`].
    pub fn rollback(&mut self) -> Result<()> {
        self.engine.rollback()
    }

    /// Whether a statement would only read.
    ///
    /// Planning is the only honest way to answer this: a leading `SELECT` says
    /// nothing (`SELECT` can be a statement the parser rejects, and a write can
    /// be spelled many ways), whereas a plan is either a read or it is not.
    /// The MCP server uses it to refuse writes on a read-only connection
    /// *before* anything touches the database.
    pub fn is_read_only(&self, sql: &str, params: &[Value]) -> Result<bool> {
        let statement = self.prepare(sql)?;
        statement.as_core().check_parameters(params)?;
        Ok(statement.is_read_only())
    }

    /// The guard behind [`Database::execute`] and [`Database::query`].
    ///
    /// A handle from [`Database::open_read_only`] has an underlying device
    /// that already refuses [`inlaysql_core::btree::Device::write`] — see
    /// [`FileDevice::open_read_only`] — so this is not what makes a write
    /// impossible. It is what makes refusing one *cheap and clear*: reusing
    /// [`Database::is_read_only`] answers by planning, the same honest check
    /// the MCP server already uses, rather than by letting the statement run
    /// deep enough to fail on its first write and surface whatever error that
    /// path happens to produce.
    fn check_writable(&self, sql: &str, params: &[Value]) -> Result<()> {
        if !self.read_only || self.is_read_only(sql, params)? {
            return Ok(());
        }
        Err(Error::Storage(format!(
            "this database handle is open read-only; refusing to run a write \
             statement: `{}`",
            first_words(sql)
        )))
    }

    /// [`Database::check_writable`] for a statement that has already been
    /// planned, so it reuses [`Statement::is_read_only`] instead of planning
    /// `sql` a second time.
    fn check_writable_statement(&self, statement: &Statement) -> Result<()> {
        if !self.read_only || statement.is_read_only() {
            return Ok(());
        }
        Err(Error::Storage(format!(
            "this database handle is open read-only; refusing to run a write \
             statement: `{}`",
            first_words(statement.sql())
        )))
    }

    /// Committed row changes after `from`, in commit order.
    ///
    /// Pass `0` to start from the beginning of the retained log, or the
    /// [`Changes::version`] from the previous call to continue. Always check
    /// [`Changes::lost`]: a consumer that has been away longer than the
    /// retention window has to resynchronise from a scan rather than pretend
    /// the log was complete.
    ///
    /// A record says *what* changed, not what it became — see
    /// [`inlaysql_core::cdc`] for why that is the contract.
    pub fn changes(&self, from: u64) -> Result<Changes> {
        self.engine.changes(from)
    }

    /// The current change version, for a consumer that wants to start from
    /// "now" rather than replay history.
    pub fn change_version(&self) -> u64 {
        self.engine.change_version()
    }

    /// The tables this database knows about.
    pub fn catalog(&self) -> &Catalog {
        self.engine.catalog()
    }

    /// The row id of the last row this handle inserted without being told the
    /// key — SQLite's `last_insert_rowid()`.
    ///
    /// `None` until this handle runs such an `INSERT`. An `INSERT` that
    /// supplied its own `INTEGER PRIMARY KEY` does not change it, a multi-row
    /// `INSERT` reports its last assigned row, and no other statement touches
    /// it. See [`inlaysql_core::Engine::last_insert_row_id`] for the full
    /// contract.
    ///
    /// ```
    /// use inlaysql::Database;
    ///
    /// let mut db = Database::open_in_memory()?;
    /// db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
    /// assert_eq!(db.last_insert_row_id(), None);
    ///
    /// db.execute("INSERT INTO kv (body) VALUES ('one')", &[])?;
    /// assert_eq!(db.last_insert_row_id(), Some(1));
    /// # Ok::<(), inlaysql::Error>(())
    /// ```
    pub fn last_insert_row_id(&self) -> Option<u64> {
        self.engine.last_insert_row_id()
    }

    /// Resident vector payload bytes for one ANN index, if its backend exposes
    /// the measurement. Graph/container overhead is excluded.
    pub fn vector_index_resident_bytes(&self, table: &str, column: &str) -> Option<usize> {
        self.engine.vector_index_resident_bytes(table, column)
    }
}

/// The first few words of a statement, for an error message that names what
/// was refused without dumping an arbitrarily long (or sensitive-looking)
/// SQL string into it.
fn first_words(sql: &str) -> String {
    sql.split_whitespace().take(6).collect::<Vec<_>>().join(" ")
}

/// A candidate-list size that does not move, for an embedded caller that set
/// one number and is not a session. See [`Database::set_vector_ef_search`].
#[derive(Debug, Clone, Copy)]
struct FixedEfSearch(Option<usize>);

impl VectorTuning for FixedEfSearch {
    fn ef_search(&self) -> Option<usize> {
        self.0
    }
}

/// Builds the production index backends.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeIndexFactory;

impl IndexFactory for NativeIndexFactory {
    fn full_text(&self, _table: &str, _column: &str) -> Result<Box<dyn FullTextIndex>> {
        Ok(Box::new(Bm25Index::new()))
    }

    fn vector(
        &self,
        _table: &str,
        _column: &str,
        dim: usize,
        metric: VectorMetric,
    ) -> Result<Box<dyn VectorIndex>> {
        Ok(Box::new(HnswIndex::with_metric(dim, metric)))
    }

    fn quantized_vector(
        &self,
        _table: &str,
        _column: &str,
        dim: usize,
        metric: VectorMetric,
    ) -> Result<Box<dyn VectorIndex>> {
        Ok(Box::new(HnswIndex::quantized_with_metric(dim, metric)))
    }
}

/// The wall clock, as the operating system reports it.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_micros(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_micros() as i64)
            .unwrap_or(0)
    }
}
