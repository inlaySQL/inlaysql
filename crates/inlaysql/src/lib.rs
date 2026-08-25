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
//! writers keep committing, and [`vacuum`] compacts one by rebuilding it. The
//! two are not the same operation: see [`backup`] for which is which.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

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
pub use device::FileDevice;
pub use inlaysql_core::bm25::Bm25Index;
pub use inlaysql_core::bm25_paged::PagedBm25Index;
/// The stand-in embedder lives in the core because every build has to agree on
/// it byte for byte — the WASM module in a browser tab has to bucket trigrams
/// exactly as the CLI that seeded the file did. Re-exported here so
/// `inlaysql::embedding::hashed_embedding` keeps working.
pub use inlaysql_core::embedding;
pub use inlaysql_core::hnsw::HnswIndex;
pub use inlaysql_core::TreeStorage;
pub use inlaysql_core::{
    is_reserved_table_name, Cancel, Catalog, Change, ChangeKind, Changes, Collation, Column,
    ColumnInfo, DataType, Error, Index, IndexKind, Outcome, Result, ResultSet, Stopped, Table,
    TableAccess, Value, RESERVED_TABLE_PREFIX,
};
pub use statement::Statement;
pub use storage::RedbStorage;
pub use vacuum::vacuum;

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

    /// How many statements this handle has parsed since it was opened.
    ///
    /// Diagnostic: it is how a caller (or a test) confirms that a prepared
    /// statement really is parsed once rather than on every execution.
    pub fn statements_parsed(&self) -> u64 {
        self.engine.statements_parsed()
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

/// Builds the production index backends.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeIndexFactory;

impl IndexFactory for NativeIndexFactory {
    fn full_text(&self, _table: &str, _column: &str) -> Result<Box<dyn FullTextIndex>> {
        Ok(Box::new(Bm25Index::new()))
    }

    fn vector(&self, _table: &str, _column: &str, dim: usize) -> Result<Box<dyn VectorIndex>> {
        Ok(Box::new(HnswIndex::new(dim)))
    }

    fn quantized_vector(
        &self,
        _table: &str,
        _column: &str,
        dim: usize,
    ) -> Result<Box<dyn VectorIndex>> {
        Ok(Box::new(HnswIndex::new_quantized(dim)))
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
