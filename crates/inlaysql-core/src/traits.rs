//! The seam between the deterministic core and the outside world.
//!
//! Every capability the core cannot implement itself — durable storage,
//! full-text search, approximate nearest neighbours, the clock — is expressed
//! here as a trait. Production wiring lives in the `inlaysql` crate; the
//! deterministic test wiring lives in [`crate::mem`].
//!
//! # Persisting an index
//!
//! [`FullTextIndex::save`] / [`FullTextIndex::load`] (and their vector
//! counterparts) let a backend be written into the database file so that
//! opening it does not have to re-read every row.
//!
//! **This is a cache, never a source of truth.** The engine stamps each saved
//! blob with the write version it reflects and throws it away unless that
//! version matches the committed data. A backend that cannot persist returns
//! `None` and is simply rebuilt; a blob that fails to decode is discarded and
//! rebuilt. Neither case can produce a wrong answer — the worst outcome is a
//! slower open.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::btree::{BackupSummary, Device};
use crate::error::Result;
use crate::hnsw::VectorMetric;
use crate::row::RowBuf;

/// Stable identifier of a row inside a table.
///
/// Row ids are assigned by the engine in increasing order and never reused in
/// this stage. Both index backends key their entries by row id.
pub type RowId = u64;

/// A predicate a filtered retrieval search pushes into the walk.
///
/// `Ok(true)` admits the row into the result set; `Ok(false)` excludes it
/// from the results *without* excluding it from the walk — the search still
/// traverses through a rejected row to reach its neighbours, which is what
/// keeps a selective filter from severing the graph (see
/// [`VectorIndex::search`]). The predicate runs on the engine's side and may
/// decode a row, so it can fail; errors propagate out of the search.
pub type RowFilter<'a> = dyn Fn(RowId) -> Result<bool> + 'a;

/// A row id paired with a relevance score. Higher scores rank first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scored {
    /// The row this score belongs to.
    pub id: RowId,
    /// Backend-specific relevance. Only the *ordering* is meaningful across
    /// backends — fusion works on ranks, not raw scores.
    pub score: f32,
}

impl Scored {
    /// Convenience constructor.
    pub fn new(id: RowId, score: f32) -> Self {
        Self { id, score }
    }
}

/// Durable key/value storage for rows and engine metadata.
///
/// Writes are buffered by the implementation and made durable by
/// [`Storage::commit`]; the engine calls it once per statement.
pub trait Storage {
    /// Whether this database file can contain the v4 int8 vector encodings.
    ///
    /// Memory backends and new files return `true`. A grandfathered v3 tree
    /// overrides this to keep an old header from silently describing new row
    /// bytes an older binary cannot understand.
    fn supports_quantized_vectors(&self) -> bool {
        true
    }

    /// Write (or overwrite) a row.
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()>;

    /// Read a single row.
    ///
    /// [`RowBuf`] rather than `Vec<u8>` (`AHL-478`): a backend that caches
    /// its committed pages behind an `Rc` (`TreeStorage`, the production
    /// path) can hand back a share of those bytes instead of a fresh copy of
    /// them, so a row a filter goes on to reject was never copied at all. A
    /// backend with nothing to share (`MemStorage`, the deterministic test
    /// path) returns `RowBuf::Owned` and pays exactly what it paid before —
    /// this is a widening of what a backend is *allowed* to return, not a
    /// new obligation on every implementation.
    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>>;

    /// Delete a row. Deleting a missing row is not an error.
    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()>;

    /// Read the next run of a table's rows, ordered by row id ascending.
    ///
    /// This is the whole of the scan surface, and it is deliberately a *batch*
    /// rather than the `Vec<(RowId, Vec<u8>)>` of the whole table it used to
    /// be: a `SELECT ... LIMIT 10` over a million rows must not decode a
    /// million rows first, and an executor that streams cannot be built on a
    /// call that materialises. [`RowScan`] turns it back into an ordinary
    /// [`Iterator`], which is what the engine actually uses.
    ///
    /// Three obligations, all of which the engine relies on:
    ///
    /// * **Only rows strictly after `after`.** `None` starts at the beginning.
    ///   Row ids are the resume token, which works because the answer is
    ///   ordered by them.
    /// * **At most `limit` rows**, and *fewer only when there are no more*.
    ///   A short batch is how [`RowScan`] learns the scan is finished, so a
    ///   backend that returns a short batch with rows still to come truncates
    ///   the caller's query.
    /// * **One snapshot across batches.** Two consecutive calls must answer
    ///   from the same committed state. Every backend here gets this for free
    ///   — a batch takes `&self`, so nothing can commit between two of them —
    ///   and the engine's own writers materialise their candidates up front
    ///   rather than reading a scan they are changing.
    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>>;

    /// Write (or overwrite) a `WITHOUT ROWID` table's row, addressed by its
    /// primary key's encoded bytes rather than a [`RowId`] — there is no
    /// row id on such a table at all.
    ///
    /// `key` is built the same collation-aware way a scalar index's entry
    /// key is (`crate::storage::primary_key_bytes`), which is what makes it
    /// a legal key in the same tree namespace `key`'s table already owns:
    /// implementing this is exactly [`Storage::put_row`] with the row id's
    /// eight fixed bytes replaced by an arbitrary-length one, not a second
    /// storage scheme.
    ///
    /// **The default refuses**, the same reasoning [`Storage::put_index_entry`]
    /// gives: a backend that has not implemented this cannot hold a
    /// `WITHOUT ROWID` table, and the engine fails the `CREATE TABLE` (and
    /// every write to one already created) rather than accept rows nothing
    /// can read back.
    fn put_row_keyed(&mut self, table: &str, key: &[u8], bytes: &[u8]) -> Result<()> {
        let _ = (table, key, bytes);
        Err(unsupported_without_rowid())
    }

    /// Read a single `WITHOUT ROWID` row by its primary key's encoded bytes.
    /// See [`Storage::put_row_keyed`].
    fn get_row_keyed(&self, table: &str, key: &[u8]) -> Result<Option<RowBuf>> {
        let _ = (table, key);
        Err(unsupported_without_rowid())
    }

    /// Delete a `WITHOUT ROWID` row by its primary key's encoded bytes.
    /// Deleting a missing one is not an error, matching [`Storage::delete_row`].
    fn delete_row_keyed(&mut self, table: &str, key: &[u8]) -> Result<()> {
        let _ = (table, key);
        Err(unsupported_without_rowid())
    }

    /// The next run of a `WITHOUT ROWID` table's rows, ordered by primary
    /// key bytes ascending — [`Storage::scan_batch`]'s three obligations
    /// (resume-after, short-batch-means-done, one snapshot across batches)
    /// apply here identically, with the primary key's bytes standing in for
    /// the row id as both the sort order and the resume token.
    fn scan_batch_keyed(
        &self,
        table: &str,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        let _ = (table, after, limit);
        Err(unsupported_without_rowid())
    }

    /// Write an engine metadata entry (the catalog lives here).
    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()>;

    /// Read an engine metadata entry.
    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Write one scalar secondary index entry.
    ///
    /// The key is an opaque, order-carrying byte string built by
    /// [`crate::index`]; the entry has no value. A backend only has to store
    /// the key and be able to walk a range of them in ascending byte order —
    /// which is what makes an index entry an ordinary write, in the same
    /// transaction as the row it describes, with the same durability.
    ///
    /// **The default refuses.** A backend that has not implemented this cannot
    /// hold an index, and the engine will fail the `CREATE INDEX` — and every
    /// write to an already-indexed table — rather than accept a declaration
    /// nothing maintains. An index that is not maintained returns wrong rows
    /// with no error at all, so silence is the one answer that is not
    /// available here.
    fn put_index_entry(&mut self, key: &[u8]) -> Result<()> {
        let _ = key;
        Err(unsupported_index())
    }

    /// Remove one scalar secondary index entry. Removing a missing entry is
    /// not an error, matching [`Storage::delete_row`].
    fn delete_index_entry(&mut self, key: &[u8]) -> Result<()> {
        let _ = key;
        Err(unsupported_index())
    }

    /// Every index entry key in `[start, end)`, in ascending key order,
    /// including this transaction's own writes.
    ///
    /// `end` of `None` means "to the end of the key space". Seeing the open
    /// transaction's writes is required, not optional: a statement that
    /// inserts a row and then reads it back through an index has to find it,
    /// exactly as [`Storage::scan_batch`] has to.
    fn scan_index_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        let _ = (start, end);
        Err(unsupported_index())
    }

    /// The row id of every index entry in `[start, end)`, in the order the
    /// backend visits them — **not necessarily row-id order**, the same
    /// caveat [`Storage::scan_index_range`] carries: a range spanning more
    /// than one value groups by value first, so a caller that needs row-id
    /// order sorts the result.
    ///
    /// Every caller of [`Storage::scan_index_range`] today (`AHL-479`) reads
    /// back the whole key only to decode the eight-byte row id off the end of
    /// it and discard the rest, since an index entry's value is always empty.
    /// This is that same answer without the detour: a backend that can reach
    /// the row id without materialising the full key and resolving its
    /// (empty) value — [`crate::storage::TreeStorage`], which reads it
    /// straight out of a borrowed tree entry rather than cloning the key into
    /// an owned `Vec<u8>` first — should override this rather than leave the
    /// default running.
    ///
    /// **The default is exactly [`Storage::scan_index_range`] plus the decode
    /// every caller already did by hand**, so a backend that only implements
    /// the general walk (a test double, [`crate::mem::MemStorage`], anything
    /// this trait does not know about yet) answers correctly without writing
    /// this method at all — only slower, and only exactly as slow as it
    /// already was before this method existed.
    fn scan_index_row_ids(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<RowId>> {
        self.scan_index_range(start, end)?
            .iter()
            .map(|key| crate::index::row_id_from_entry(key))
            .collect()
    }

    /// Make all buffered writes durable.
    fn commit(&mut self) -> Result<()>;

    /// Advance this handle to the state other handles have committed, and say
    /// whether anything moved.
    ///
    /// A backend that caches the committed state — the copy-on-write tree
    /// caches its root — otherwise reads the snapshot it opened on for as long
    /// as it lives, so a handle that only ever reads never sees another
    /// handle's writes. The engine calls this between statements, outside an
    /// explicit transaction, to close that gap.
    ///
    /// Two things the implementation must honour:
    ///
    /// * **Refuse while a transaction is open.** Buffered writes are rooted at
    ///   the snapshot they were built against; moving underneath them would
    ///   rebase a transaction the caller believes is pinned. Answer `false` and
    ///   change nothing.
    /// * **`true` means the caller has work to do.** The engine reloads its
    ///   catalog and counters on `true` and does nothing at all on `false`, so
    ///   a backend that answers `true` when nothing moved makes every statement
    ///   pay for a reload.
    ///
    /// The default is `false`: a backend with no cached snapshot — an in-memory
    /// map, a backend that reads through on every call — is already current.
    fn refresh(&mut self) -> Result<bool> {
        Ok(false)
    }

    /// Discard the buffered writes of the open transaction, leaving the
    /// committed state untouched.
    ///
    /// The engine calls this when a multi-statement transaction is rolled back,
    /// so that the writes the transaction buffered never become durable. After
    /// it returns, reads see exactly the last committed state.
    fn rollback(&mut self) -> Result<()>;

    /// Whether the open transaction is close to whatever this backend's limit
    /// on one transaction is.
    ///
    /// A caller writing an unbounded amount of data — the engine saving an
    /// index, which is megabytes — has no way to know how much a backend can
    /// take in one go. Guessing produces a byte budget that is fine on one
    /// backend and overflows another; the write-ahead log backend has a hard
    /// ceiling of one log region per transaction, and exceeding it is an
    /// error, not a slow path.
    ///
    /// So the backend answers instead, and the caller commits when it says so.
    /// The default is `false`: a backend with no such limit never needs to
    /// interrupt anyone.
    ///
    /// **The answer has to cover what committing will still add, not only what
    /// the transaction is holding now.** A backend that does bookkeeping of its
    /// own inside `commit` — the copy-on-write tree writes free-list rows there
    /// when `page_reuse` is on — does that work after the last moment anybody
    /// could have asked, and a caller who stopped exactly when told is then
    /// left holding a transaction that can never be committed. Answering `true`
    /// too early costs one extra commit; answering it too late costs the whole
    /// batch. See `CowBTree::projected_record_len`.
    fn transaction_is_nearly_full(&self) -> bool {
        false
    }

    /// Write a consistent copy of this backend's committed state to `dest`,
    /// without stopping any writer on the source.
    ///
    /// `&self`, deliberately: a backup must not be able to move the snapshot it
    /// is copying, and taking a shared reference is what makes that a
    /// compile-time fact rather than a convention — every method that advances
    /// the committed state (`commit`, `refresh`, `rollback`) takes `&mut self`.
    ///
    /// `dest` is a [`Device`] because the only backend that can answer this is
    /// the copy-on-write tree, and for it a backup is a page copy: an already
    /// committed root is an immutable, consistent snapshot, so the copy is
    /// never a mix of two commits however many land while it runs. See
    /// [`crate::btree::backup`] for the whole argument, including the one
    /// configuration it refuses.
    ///
    /// **The default refuses**, and the message says which backend could not.
    /// An in-memory backend has no device to copy and no file to produce; a
    /// caller asking for a backup of one has asked for something that does not
    /// exist, and inventing a plausible-looking answer (dumping rows into a
    /// fresh tree, say) would be a different operation wearing this name.
    fn backup_to(&self, dest: &mut dyn Device) -> Result<BackupSummary> {
        let _ = dest;
        Err(unsupported_backup())
    }
}

/// How many rows the first batch of a [`RowScan`] asks for.
///
/// Small on purpose: a `LIMIT 1` must not pay for a thousand decoded rows it
/// throws away, and the great majority of scans in an OLTP workload stop early.
const FIRST_SCAN_BATCH: usize = 32;

/// The ceiling a [`RowScan`]'s batch grows to.
///
/// A batch costs one descent to the resume key, so a full scan wants big
/// batches and a short one wants small batches. Doubling up to this bound gives
/// both: a scan that reads everything pays `O(rows / 512)` extra descents,
/// which against the per-row decode is noise.
const MAX_SCAN_BATCH: usize = 512;

/// Every row of one table, streamed in row-id order.
///
/// The engine's sequential access path. It pulls [`Storage::scan_batch`] one
/// batch at a time and hands the rows out one at a time, so a consumer that
/// stops — `LIMIT`, a filter that has already found what it needed — leaves the
/// rest of the table unread and undecoded.
///
/// Errors are yielded, not swallowed: a failing batch produces one `Err` and
/// then the scan ends, so `collect::<Result<Vec<_>>>()` reports exactly the
/// first failure.
pub struct RowScan<'a> {
    storage: &'a dyn Storage,
    table: alloc::string::String,
    /// The last row id handed out, which is where the next batch resumes.
    after: Option<RowId>,
    batch: alloc::vec::IntoIter<(RowId, RowBuf)>,
    /// How large the next batch should be.
    size: usize,
    /// Set once a short batch (or an error) has proved there is nothing more.
    finished: bool,
    /// Where a cancelled statement is noticed. `None` for a scan nobody can
    /// cancel — [`RowScan::new`], which is what an external caller has.
    interrupt: Option<&'a Interrupt>,
}

impl<'a> RowScan<'a> {
    /// Begin scanning `table`. Reads nothing until the first [`Iterator::next`].
    pub fn new(storage: &'a dyn Storage, table: &str) -> Self {
        Self {
            storage,
            table: alloc::string::String::from(table),
            after: None,
            batch: Vec::new().into_iter(),
            size: FIRST_SCAN_BATCH,
            finished: false,
            interrupt: None,
        }
    }

    /// The same scan, cancellable.
    ///
    /// This is the single point that makes "stop this statement" reach a table
    /// scan at all: every sequential read in the engine — a `SELECT`'s access
    /// path, an `UPDATE`'s candidate list, a hash-join build, a `UNIQUE`
    /// re-check, an index rebuild — is one of these, so a check here covers
    /// all of them without each one carrying its own. The check is spent per
    /// *batch* rather than per row, against a countdown measured in rows, so a
    /// scan that reads 512 rows in one call spends 512 rows of the interval.
    pub fn watched(storage: &'a dyn Storage, table: &str, interrupt: &'a Interrupt) -> Self {
        Self {
            interrupt: Some(interrupt),
            ..Self::new(storage, table)
        }
    }
}

impl Iterator for RowScan<'_> {
    type Item = Result<(RowId, RowBuf)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.batch.next() {
                self.after = Some(row.0);
                return Some(Ok(row));
            }
            if self.finished {
                return None;
            }
            // Before the read, not after it: a cancelled scan should stop
            // costing tree descents the moment it is cancelled, not one batch
            // later.
            if let Some(interrupt) = self.interrupt {
                if let Err(error) = interrupt.check_rows(self.size) {
                    self.finished = true;
                    return Some(Err(error));
                }
            }
            let batch = match self.storage.scan_batch(&self.table, self.after, self.size) {
                Ok(batch) => batch,
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            };
            // A backend that could not fill the batch has nothing left; that is
            // the contract on `scan_batch`, and it is what ends the scan without
            // a further round trip that would return nothing.
            if batch.len() < self.size {
                self.finished = true;
            }
            if batch.is_empty() {
                return None;
            }
            self.size = self.size.saturating_mul(2).min(MAX_SCAN_BATCH);
            self.batch = batch.into_iter();
        }
    }
}

/// Every row of `table`, materialised.
///
/// The write paths use this deliberately: an `UPDATE` or a `DELETE` reads the
/// rows it is about to change, and SQLite's semantics are that the statement
/// sees the table as it was when it started. Reading the candidates into a
/// `Vec` first is what guarantees that — see [`RowScan`] for the streaming
/// form, which is for readers.
pub fn scan_all(storage: &dyn Storage, table: &str) -> Result<Vec<(RowId, RowBuf)>> {
    RowScan::new(storage, table).collect()
}

/// [`scan_all`], cancellable. See [`RowScan::watched`].
pub fn scan_all_watched(
    storage: &dyn Storage,
    table: &str,
    interrupt: &Interrupt,
) -> Result<Vec<(RowId, RowBuf)>> {
    RowScan::watched(storage, table, interrupt).collect()
}

/// [`scan_all`] for a `WITHOUT ROWID` table, keyed by primary key bytes
/// rather than row id.
///
/// A plain loop over [`Storage::scan_batch_keyed`] rather than a second
/// [`RowScan`]: that struct's self-doubling batch size and cancellable
/// variant exist for the streaming *read* path, which this table does not
/// have yet either (`Engine::without_rowid_stream` materialises fully, the
/// same choice `Engine::derived_stream` makes for a derived table) — so
/// there is nothing here for a streaming iterator to buy back.
pub fn scan_all_keyed(storage: &dyn Storage, table: &str) -> Result<Vec<(Vec<u8>, RowBuf)>> {
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let batch = storage.scan_batch_keyed(table, after.as_deref(), MAX_SCAN_BATCH)?;
        let short = batch.len() < MAX_SCAN_BATCH;
        if let Some((key, _)) = batch.last() {
            after = Some(key.clone());
        }
        out.extend(batch);
        if short {
            return Ok(out);
        }
    }
}

/// A BM25-ranked full-text index over one text column.
pub trait FullTextIndex {
    /// Index (or re-index) a document.
    fn insert(&mut self, id: RowId, text: &str) -> Result<()>;

    /// Drop a document from the index.
    fn remove(&mut self, id: RowId) -> Result<()>;

    /// Make pending index writes visible to [`FullTextIndex::search`].
    fn commit(&mut self) -> Result<()>;

    /// Return up to `k` documents ranked by BM25 relevance, best first.
    ///
    /// `filter` restricts which documents may appear in the result. A document
    /// it rejects is skipped without consuming a result slot, and the scan
    /// continues past it — an inverted index has no graph to keep connected,
    /// so filtered search is exactly "keep scanning until `k` documents pass
    /// or the postings run out". Fewer than `k` results therefore means the
    /// filter's answer over the whole index, never a partial probe.
    fn search(&self, query: &str, k: usize, filter: Option<&RowFilter>) -> Result<Vec<Scored>>;

    /// Serialise the committed index, or `None` if this backend cannot be
    /// persisted. See the [module-level note](self#persisting-an-index).
    fn save(&self) -> Option<Vec<u8>> {
        None
    }

    /// Restore state produced by [`FullTextIndex::save`].
    fn load(&mut self, bytes: &[u8]) -> Result<()> {
        let _ = bytes;
        Err(unsupported_load())
    }

    /// Whether this backend stores its own structure inside the database,
    /// rather than being serialised into a blob by the engine.
    ///
    /// The full-text twin of [`VectorIndex::is_self_persisting`], with the
    /// same four consequences for the engine — see that method, and
    /// [`crate::bm25_paged::PagedBm25Index`], which is the one backend here
    /// that answers `true`.
    ///
    /// **Every method under this heading is defaulted**, deliberately: a
    /// full-text backend outside this repository implements `insert`,
    /// `remove`, `commit` and `search` and gets the pre-existing behaviour
    /// unchanged, because the defaults spell out exactly what the engine
    /// assumed before any of them existed. Widening the trait was the whole
    /// design cost of a paged BM25 index, and this is the smallest form it
    /// could take.
    fn is_self_persisting(&self) -> bool {
        false
    }

    /// Throw the index away and start empty.
    ///
    /// Called before a rebuild, so that a self-persisting backend does not add
    /// a second copy of every document on top of the copy it just restored. A
    /// backend the engine holds only in memory has nothing to do here.
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    /// Tell a self-persisting backend what the commit it is about to do means.
    ///
    /// Identical in meaning to [`VectorIndex::prepare_commit`]: `write_version`
    /// is the version of the committed rows the index will describe once it
    /// finishes, and `may_commit` is whether the backend is allowed to make
    /// its own writes durable — false whenever the engine is inside a caller's
    /// transaction.
    fn prepare_commit(&mut self, write_version: u64, may_commit: bool) {
        let _ = (write_version, may_commit);
    }

    /// The write version the structure this backend restored from the database
    /// describes, or `None` when there is nothing current to restore —
    /// including a structure a crash left half-written, since a backend stamps
    /// the version only on the commit that completes it.
    fn stored_write_version(&self) -> Option<u64> {
        None
    }
}

/// A nearest-neighbour index over one vector column.
pub trait VectorIndex {
    /// Add or replace an embedding.
    fn insert(&mut self, id: RowId, embedding: &[f32]) -> Result<()>;

    /// Drop an embedding.
    fn remove(&mut self, id: RowId) -> Result<()>;

    /// Make pending writes searchable. Graph-based backends rebuild here.
    fn commit(&mut self) -> Result<()>;

    /// Return up to `k` neighbours ranked by similarity, closest first.
    ///
    /// `filter` restricts which rows may appear in the result. A row it
    /// rejects is excluded from the result set and does not count toward `k`,
    /// but the walk still expands through it, so its neighbours stay
    /// reachable: pruning rejected nodes themselves would break the graph's
    /// connectivity and silently drop matches behind them.
    ///
    /// The walk stops when its candidate beam fills with matching rows —
    /// which for a permissive filter is exactly the unfiltered walk, byte for
    /// byte — or when the graph is exhausted. Exhaustion happens only when
    /// the filter admits fewer rows than the beam, and then the returned set
    /// is *complete* for that filter: a filter too selective for any bounded
    /// probe degrades to correctness, never to a partial answer. One walk,
    /// where the pre-pushdown engine re-walked the graph once per doubling
    /// round of its over-fetch loop. Passing `None` searches unfiltered with
    /// exactly the behaviour and cost of before.
    fn search(&self, query: &[f32], k: usize, filter: Option<&RowFilter>) -> Result<Vec<Scored>>;

    /// [`VectorIndex::search`] with the caller's candidate-list size imposed,
    /// rather than the one this backend would have chosen for `k` itself.
    ///
    /// `ef` is the whole recall/latency trade: it is how many candidates the
    /// walk may hold at once, so a larger one visits more of the graph and
    /// finds more of the true neighbours, and a smaller one returns sooner and
    /// finds fewer.
    ///
    /// `ef` may be **smaller than `k`**, and then fewer than `k` neighbours
    /// come back — which is within this trait's existing contract ("up to
    /// `k`") and is exactly what a caller asking for less latency is asking
    /// for. What [`Engine`](crate::Engine) does guarantee is that `ef` is at
    /// least the number of rows the *query* can return, which is `k` divided
    /// by the engine's over-fetch; a session that asks for less than that is
    /// refused rather than silently widened.
    ///
    /// **The default refuses.** A backend that has no candidate list to size
    /// cannot honour this number, and accepting it would mean a session that
    /// set `ef_search`, read it back, and saw it in `EXPLAIN` was searching
    /// under a different one — the reported-but-not-enforced failure this
    /// engine treats as worse than reporting nothing at all. A backend that
    /// does have a beam overrides this; one that does not says so, loudly, and
    /// a query that never asked for an `ef` still goes through
    /// [`VectorIndex::search`] and is unaffected either way.
    fn search_with_ef(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        filter: Option<&RowFilter>,
    ) -> Result<Vec<Scored>> {
        let _ = (query, k, ef, filter);
        Err(crate::error::Error::Unsupported(alloc::format!(
            "this vector index backend has no candidate list to size, so it cannot search \
             with ef_search = {ef}: it is exhaustive, and already returns the exact answer \
             an unbounded ef would. Unset the session's ef_search to query it"
        )))
    }

    /// The candidate-list size this backend would search `k` neighbours with
    /// if nothing were imposed, or `None` for a backend that has no candidate
    /// list at all.
    ///
    /// Exists for `EXPLAIN`, which reports the operating point a vector search
    /// will run at — the number is only choosable if it is also visible, and a
    /// plan that named an `ef` the search would not use would be describing a
    /// query nobody ran.
    fn ef_for(&self, k: usize) -> Option<usize> {
        let _ = k;
        None
    }

    /// Resident bytes occupied by vector payloads, when the backend can
    /// report them. Graph/container overhead is excluded.
    fn resident_vector_bytes(&self) -> Option<usize> {
        None
    }

    /// Whether this backend stores its own structure inside the database,
    /// rather than being serialised into a blob by the engine.
    ///
    /// Such a backend restores itself when it is opened, so the engine must not
    /// rebuild it from the rows just because it produced no blob — the whole
    /// point is that it did not have to. It still takes part in the
    /// write-version protocol through [`VectorIndex::save`] / `load`: what it
    /// saves is a stamp, not the index, and a stamp that no longer matches the
    /// committed data means the same thing it always did — rebuild.
    fn is_self_persisting(&self) -> bool {
        false
    }

    /// Throw the index away and start empty.
    ///
    /// Called before a rebuild, so that a self-persisting backend does not add
    /// a second copy of every row on top of the copy it just restored. A
    /// backend the engine holds only in memory has nothing to do here.
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    /// Tell a self-persisting backend what the commit it is about to do means.
    ///
    /// `write_version` is the version of the committed rows the index will
    /// describe once it finishes, and it is what a later open compares against
    /// [`VectorIndex::stored_write_version`] to decide whether the structure in
    /// the database is still current.
    ///
    /// `may_commit` is whether the backend is allowed to make its own writes
    /// durable. It is false whenever the engine is inside a caller's
    /// transaction: committing there would make the caller's buffered rows
    /// durable at a moment the caller did not choose. When it is true the rows
    /// are already committed and the backend is free to break a large build
    /// into as many transactions as it needs — which it must, because one
    /// transaction has a hard size ceiling.
    fn prepare_commit(&mut self, write_version: u64, may_commit: bool) {
        let _ = (write_version, may_commit);
    }

    /// The write version the structure this backend restored from the database
    /// describes, or `None` when there is nothing current to restore.
    ///
    /// `None` also covers a structure that was left half-written by a crash: a
    /// backend stamps the version only on the commit that completes it.
    fn stored_write_version(&self) -> Option<u64> {
        None
    }

    /// Serialise the committed index, or `None` if this backend cannot be
    /// persisted. See the [module-level note](self#persisting-an-index).
    fn save(&self) -> Option<Vec<u8>> {
        None
    }

    /// Restore state produced by [`VectorIndex::save`].
    fn load(&mut self, bytes: &[u8]) -> Result<()> {
        let _ = bytes;
        Err(unsupported_load())
    }
}

fn unsupported_backup() -> crate::error::Error {
    crate::error::Error::Unsupported(alloc::string::String::from(
        "this storage backend has no durable device to copy, so it cannot produce a \
         backup; only a file-backed database can",
    ))
}

fn unsupported_index() -> crate::error::Error {
    crate::error::Error::Unsupported(alloc::string::String::from(
        "this storage backend cannot hold scalar secondary index entries, so an index on it \
         would describe rows nothing keeps up to date",
    ))
}

fn unsupported_without_rowid() -> crate::error::Error {
    crate::error::Error::Unsupported(alloc::string::String::from(
        "this storage backend cannot hold a WITHOUT ROWID table's rows, which are addressed by \
         primary key rather than by row id",
    ))
}

fn unsupported_load() -> crate::error::Error {
    crate::error::Error::Unsupported(alloc::string::String::from(
        "this index backend cannot be restored from bytes",
    ))
}

/// Creates the per-column index backends the engine needs.
///
/// The engine asks for an index the first time it sees an indexable column,
/// so the factory decides which real implementation gets used.
pub trait IndexFactory {
    /// Build a full-text index for `table.column`.
    fn full_text(&self, table: &str, column: &str) -> Result<Box<dyn FullTextIndex>>;

    /// Build an exact vector index for `table.column`, under `metric`.
    ///
    /// `metric` is not advice. An ANN graph's neighbour lists are the answer to
    /// "what is near what" under one distance, so a backend that ignored this
    /// and built a cosine graph for an index declared `vector_l2_ops` would
    /// answer every query with plausible, wrong rows and report nothing. It is
    /// a required parameter rather than a defaulted method for exactly that
    /// reason: an implementor cannot fail to see it.
    fn vector(
        &self,
        table: &str,
        column: &str,
        dim: usize,
        metric: VectorMetric,
    ) -> Result<Box<dyn VectorIndex>>;

    /// Build an int8 vector index for `table.column`, under `metric`.
    ///
    /// Existing factories remain source-compatible and exact by default.
    /// Backends that store quantised vectors override this method.
    fn quantized_vector(
        &self,
        table: &str,
        column: &str,
        dim: usize,
        metric: VectorMetric,
    ) -> Result<Box<dyn VectorIndex>> {
        self.vector(table, column, dim, metric)
    }
}

/// The only way the core can learn the time.
///
/// Nothing in this stage depends on wall-clock time for results; the trait
/// exists so that when MVCC timestamps arrive they are injectable, and a
/// simulation can advance time by hand.
pub trait Clock {
    /// Microseconds since an implementation-defined epoch, monotonic.
    fn now_micros(&self) -> i64;
}

/// Why a statement was stopped before it finished.
///
/// A closed set rather than a message, because the two mean different things
/// to a client and get different MySQL error codes: a timeout is a limit the
/// server chose and the same statement over fewer rows would have succeeded,
/// while a kill is somebody's decision about this statement specifically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The statement outlived the deadline the host gave it.
    Timeout,
    /// Something outside asked for it to stop — a `KILL`, an operator, an
    /// application's own cancel handle.
    Killed,
}

impl Stopped {
    /// What to say when this ends a statement. Written here rather than at the
    /// error site so both variants read the same way wherever they surface.
    pub fn message(self) -> &'static str {
        match self {
            Stopped::Timeout => {
                "statement cancelled: it ran past the deadline it was given. Nothing was \
                 written — a statement is undone as a unit, so the database is exactly as \
                 an un-run statement would have left it, and this handle is still usable."
            }
            Stopped::Killed => {
                "statement cancelled: something asked for it to stop. Nothing was written — \
                 a statement is undone as a unit, so the database is exactly as an un-run \
                 statement would have left it, and this handle is still usable."
            }
        }
    }
}

/// The only way the core can be told to abandon the statement in flight.
///
/// The same seam as [`Clock`], and for the same reason: `inlaysql-core` is
/// `no_std`, so it can neither read a clock nor own a thread that would
/// interrupt one — it cannot time a statement out and it cannot hear a `KILL`.
/// What it *can* do is ask, from inside the loops that run long, and let
/// whoever installed the signal decide. A host that installs nothing pays a
/// null check per few thousand rows and nothing else.
///
/// # What a cancelled statement is allowed to leave behind
///
/// Nothing. The core only asks this question while a statement is *producing
/// or collecting* rows — never while it is making them durable — so a refusal
/// travels out as an ordinary `Err` and takes the same statement-atomicity
/// path a `CHECK` violation or a type error takes
/// (`Engine::discard_failed_statement`): the buffered writes are dropped and
/// the handle is reloaded. Checking inside the commit would be the one place
/// that could half-apply a write, which is exactly why it is not checked
/// there.
///
/// # Implementing one
///
/// [`Cancel::stop`] is called from hot loops, so it must be cheap: an atomic
/// load, or a clock read the core has already amortised down to one per few
/// thousand rows. It must not block, allocate or take a lock that a `KILL`
/// arriving on another thread also wants.
pub trait Cancel {
    /// A new statement is beginning; arm whatever this signal measures.
    ///
    /// Called once per statement from the same place the engine takes its one
    /// clock reading, so a deadline covers exactly one statement and a `KILL
    /// QUERY` that arrived between two of them does not fall on the wrong one.
    /// The default does nothing, for a signal that is not time-based.
    fn statement_began(&self) {}

    /// Why the statement in flight must stop, or `None` to carry on.
    fn stop(&self) -> Option<Stopped>;
}

/// How many rows of work the core does between two [`Cancel::stop`] calls.
///
/// The number trades responsiveness against per-row cost. A row costs tens to
/// hundreds of nanoseconds here, so a thousand of them is tens to hundreds of
/// microseconds between checks — far below any timeout worth configuring, and
/// far above the cost of the check itself.
const CANCEL_STRIDE: u32 = 1024;

/// The amortised cancellation check every long loop in the core goes through.
///
/// One object per [`crate::Engine`], holding the injected signal and the
/// countdown that keeps a per-row check from costing a virtual call per row.
/// Two things about its shape are deliberate:
///
/// * **No signal is a single predictable branch.** An embedded caller that
///   never installs one — every caller before this existed, and the benchmark
///   harness — pays one null test per row and no call at all, which is why the
///   point-read path did not move.
/// * **Work is counted in rows, not in calls.** [`Interrupt::check_rows`] lets
///   a loop that advances a whole batch at a time (a scan reading 512 rows in
///   one [`Storage::scan_batch`]) spend the batch against the same countdown a
///   per-row loop spends one row against, so the interval between two checks is
///   a fixed amount of *work* wherever it is measured from.
pub struct Interrupt {
    signal: Option<alloc::boxed::Box<dyn Cancel>>,
    /// Rows of work left before the next [`Cancel::stop`] call.
    countdown: core::cell::Cell<u32>,
}

impl Interrupt {
    /// An interrupt nothing can trip, which is what an engine has until a host
    /// installs a signal.
    pub fn none() -> Self {
        Self {
            signal: None,
            countdown: core::cell::Cell::new(CANCEL_STRIDE),
        }
    }

    /// Install `signal` as the thing every long loop asks.
    pub fn with(signal: alloc::boxed::Box<dyn Cancel>) -> Self {
        Self {
            signal: Some(signal),
            countdown: core::cell::Cell::new(CANCEL_STRIDE),
        }
    }

    /// Whether a host has installed a signal at all.
    pub fn is_armed(&self) -> bool {
        self.signal.is_some()
    }

    /// Arm the signal for a statement that is about to run.
    ///
    /// The countdown is reset too, so the first rows of a statement are checked
    /// on the same schedule as the last rows of the one before did not.
    pub fn begin_statement(&self) {
        if let Some(signal) = &self.signal {
            self.countdown.set(CANCEL_STRIDE);
            signal.statement_began();
        }
    }

    /// Ask, having done one row of work since the last time.
    #[inline]
    pub fn check(&self) -> Result<()> {
        self.check_rows(1)
    }

    /// Ask now, whatever the countdown says.
    ///
    /// For the loops whose unit is not a row and is not bounded: committing
    /// one index backend is a single call that can run for minutes, so
    /// spending one row of the stride against it would put the next check
    /// [`CANCEL_STRIDE`] index commits away — which on a database with one
    /// index means never. Cheap for the same reason [`Interrupt::check_rows`]
    /// is: a handle with no signal installed still takes one load and one
    /// predictable branch, and never calls out.
    #[inline]
    pub fn check_now(&self) -> Result<()> {
        self.check_rows(usize::MAX)
    }

    /// Ask, having done `rows` rows of work since the last time.
    #[inline]
    pub fn check_rows(&self, rows: usize) -> Result<()> {
        // The whole cost of cancellation on a handle that never installed a
        // signal: one load and one predictable branch, no call.
        let Some(signal) = &self.signal else {
            return Ok(());
        };
        let spent = u32::try_from(rows).unwrap_or(u32::MAX);
        let left = self.countdown.get();
        if left > spent {
            self.countdown.set(left - spent);
            return Ok(());
        }
        self.countdown.set(CANCEL_STRIDE);
        match signal.stop() {
            Some(reason) => Err(crate::error::Error::Cancelled(reason)),
            None => Ok(()),
        }
    }
}

impl Default for Interrupt {
    fn default() -> Self {
        Self::none()
    }
}

/// Only the countdown is observable state, and it is a cache of "how long since
/// we last asked" rather than anything a reader needs to see.
impl core::fmt::Debug for Interrupt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Interrupt")
            .field("armed", &self.signal.is_some())
            .finish()
    }
}

/// The only way the core can be told what operating point a session wants its
/// vector searches run at.
///
/// The same seam as [`Cancel`], and for the same reason: `inlaysql-core` has no
/// notion of a session, so a number one session chose and another did not can
/// only arrive from outside. A host that installs nothing leaves every index's
/// own tuning in force, which is exactly what every query did before this
/// existed.
///
/// # Why it is a handle and not a copied number
///
/// The value is read at search time, through whatever the host installed, so
/// there is one place it lives. The alternative — the host writing its number
/// into the engine on every change — is two copies of one setting, and two
/// copies is how a server ends up reporting an `ef_search` it is not searching
/// with. `inlaysql-server` installs a handle onto the same per-connection state
/// `@@inlaysql_hnsw_ef_search` is answered from, so the reported number cannot
/// be a different number from the enforced one; it is the same load.
pub trait VectorTuning {
    /// The candidate-list size (`ef`) every vector search must use, or `None`
    /// to leave each index's own `ef_search` in force.
    ///
    /// Called once per vector search — not per row and not per distance
    /// computation — so it may read shared state, but it must not block.
    fn ef_search(&self) -> Option<usize>;
}

/// The only way the core can obtain randomness.
pub trait Rng {
    /// Next pseudo-random 64-bit word.
    fn next_u64(&mut self) -> u64;
}
