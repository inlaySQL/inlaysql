//! A nearest-neighbour index whose graph does not have to fit in RAM.
//!
//! [`crate::hnsw::HnswIndex`] keeps every embedding and every node's adjacency
//! in memory. A hundred thousand 384-dimension embeddings is ~150 MB of `f32`
//! before the graph — the ceiling this module exists to remove. It is the same
//! HNSW algorithm, deterministic and incrementally maintained, but the graph
//! lives in the backing [`Storage`] and only a bounded *working set* is held in
//! memory.
//!
//! # What is in memory and what is not
//!
//! Each node is stored as one row in a synthetic table (the index's
//! `namespace`), keyed by its node index. A node record is its row id, a
//! tombstone flag, its per-layer adjacency, and its L2-normalised vector. The
//! [`NodeCache`] holds at most `cache_capacity` decoded nodes; everything else
//! is fetched on demand through [`Storage::get_row`]. Steady-state search and
//! incremental maintenance therefore cost `O(cache_capacity + ef)` resident
//! memory, whatever the corpus size — the number the acceptance criteria ask to
//! be *measured*, and which [`PagedHnswIndex::cache_len`] exposes.
//!
//! The vector itself is stored in whichever representation
//! [`crate::hnsw::VectorEncoding`] the column declares —
//! [`PagedHnswIndex::new`]/[`PagedHnswIndex::open`] for a plain `f32` vector,
//! [`PagedHnswIndex::new_quantized`]/[`PagedHnswIndex::open_quantized`] for a
//! `VECTOR(n, INT8)` column — the same choice [`crate::hnsw::HnswIndex`]
//! already makes, and by the same [`crate::quantize::Q8Vector`] representation.
//! Quantising shrinks the payload every node carries in both the cache and on
//! disk, so it lowers `cache_capacity`'s bytes-per-node as well as file size.
//!
//! Two small structures stay in memory by design:
//!
//! * `live`, a `RowId -> node index` map used by insert/remove bookkeeping. It
//!   is a few bytes per row, next to the thousands of bytes per row the vectors
//!   cost, so it does not move the memory bound.
//! * `pending_inserts`, the embeddings accepted since the last commit. It is
//!   bounded by the caller's commit interval, not by the corpus.
//!
//! A full build (first commit, a parameter retune, or a tombstone overflow) is
//! the one O(n) phase, exactly as it is for the in-RAM index: it gathers the
//! live set to insert it in row-id order. Search and steady-state maintenance
//! stay bounded.
//!
//! # Read-your-writes, and who owns the transaction
//!
//! [`VectorIndex::commit`] writes node records through [`Storage::put_row`] and
//! reads them back through [`Storage::get_row`] while it is still building the
//! same commit — an insert's greedy walk reads neighbours that earlier inserts
//! in the same window just wrote. That requires a backend whose reads see its
//! own buffered writes. [`crate::mem::MemStorage`] always did; the
//! copy-on-write [`crate::TreeStorage`] now does too, because a writer reading
//! its own transaction is what any SQL database means by a transaction (see
//! [`crate::btree::CowBTree::get`]).
//!
//! That leaves *who commits*. By default the index owns its backend and makes
//! its own graph durable. Inside the engine it must not: the engine's rows and
//! the index's nodes share one transaction, and an index that called
//! [`Storage::commit`] on its own would make the engine's half-finished
//! statement durable early. [`PagedHnswIndex::joined_to_caller_transaction`]
//! turns that off — the graph is written into the open transaction and becomes
//! durable when the engine commits it, so the rows and the index that describes
//! them land in the same commit or neither does.
//!
//! # Determinism
//!
//! The graph is a pure function of the insert sequence, using the same layer
//! assignment ([`crate::hnsw::level_of`]), distance ([`crate::hnsw::distance`])
//! and neighbour heuristic as the in-RAM index. Two builds over the same rows
//! agree byte for byte, which keeps the whole thing simulator-clean.

use alloc::collections::{BTreeMap, BinaryHeap};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::cmp::Reverse;

use crate::error::{Error, Result};
use crate::hnsw::{
    decode_stored_vector, encode_stored_vector, level_of, level_shift, max_level_for, normalise,
    stored_distance, Candidate, HnswParams, StoredVector, VectorEncoding, Visited,
};
use crate::row::{put_len, Cursor};
use crate::traits::{RowId, RowScan, Scored, Storage, VectorIndex};

/// How many decoded nodes the cache holds by default. At dim 384 a node record
/// is ~1.5 KiB, so this is ~6 MiB of working set.
pub const DEFAULT_CACHE_NODES: usize = 4096;

/// One node of the graph, as it is stored and cached.
#[derive(Debug, Clone)]
struct NodeRecord {
    /// The row this node stands for.
    id: RowId,
    /// Tombstoned nodes stay in the graph for navigation but are skipped by
    /// search. Their vector is kept (it is stored inline, unlike the in-RAM
    /// index, which can recompute a live node's vector from its embedding).
    deleted: bool,
    /// L2-normalised embedding, in the encoding the whole index shares — see
    /// [`PagedHnswIndex::encoding`].
    vector: StoredVector,
    /// `neighbors[l]` are the node indices connected at layer `l`.
    neighbors: Vec<Vec<usize>>,
}

impl NodeRecord {
    /// Encode the record.
    ///
    /// ```text
    /// record := u8 deleted, u64 row id, u32 layer_count, layer*, vector
    /// layer   := u32 neighbour_count, u32 * neighbour_count   (node indices)
    /// vector  := f32 * dim                    (`VectorEncoding::Exact`)
    ///          | f32 scale, i8 * dim          (`VectorEncoding::Q8`)
    /// ```
    ///
    /// The vector's wire format is not tagged per record — it is a property of
    /// the whole index, stored once in the header (see
    /// [`PagedHnswIndex::write_header`]) and supplied to
    /// [`NodeRecord::decode`] the same way `dim` already is. Only one encoding
    /// is ever live for a given namespace, so tagging every record with it
    /// would repeat the same byte node after node for nothing.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(self.deleted as u8);
        out.extend_from_slice(&self.id.to_le_bytes());
        put_len(&mut out, self.neighbors.len());
        for layer in &self.neighbors {
            put_len(&mut out, layer.len());
            for neighbor in layer {
                put_len(&mut out, *neighbor);
            }
        }
        encode_stored_vector(&mut out, &self.vector);
        out
    }

    /// Parse a record produced by [`NodeRecord::encode`]. `node_count` bounds
    /// every neighbour index, so a corrupt record is refused rather than allowed
    /// to send a search off the end of the graph. `encoding` picks the vector
    /// tail's width, exactly as the caller that is decoding already knows `dim`.
    fn decode(
        bytes: &[u8],
        dim: usize,
        encoding: VectorEncoding,
        node_count: usize,
    ) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let deleted = cursor.u8()? != 0;
        let id = RowId::from_le_bytes(cursor.array8()?);
        let layer_count = cursor.count(4)?;
        let mut neighbors = Vec::with_capacity(layer_count);
        for _ in 0..layer_count {
            let neighbour_count = cursor.count(4)?;
            let mut layer = Vec::with_capacity(neighbour_count);
            for _ in 0..neighbour_count {
                let neighbor = cursor.u32()? as usize;
                if neighbor >= node_count {
                    return Err(Error::Corrupt(alloc::format!(
                        "paged HNSW node {id} links to out-of-range node {neighbor}"
                    )));
                }
                layer.push(neighbor);
            }
            neighbors.push(layer);
        }
        let vector = decode_stored_vector(&mut cursor, dim, encoding)?;
        Ok(Self {
            id,
            deleted,
            vector,
            neighbors,
        })
    }
}

/// A bounded least-recently-used cache of decoded node records.
///
/// The cache is the whole of the index's resident working set: a hit is a
/// `clone` of an already-decoded record, a miss is one [`Storage::get_row`]
/// plus a decode, and the entry that was used longest ago is evicted when the
/// cache overflows its capacity. Its size is the number the memory bound is
/// stated in.
struct NodeCache {
    capacity: usize,
    entries: BTreeMap<usize, NodeRecord>,
    /// Recency order, least-recently-used first.
    recency: Vec<usize>,
}

impl NodeCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: BTreeMap::new(),
            recency: Vec::new(),
        }
    }

    /// How many records are resident.
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// The bound `len` will never exceed.
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.recency.clear();
    }

    /// Shrink the bound, evicting from the least-recently-used end first.
    fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.entries.len() > self.capacity {
            let Some(victim) = self.recency.first().copied() else {
                break;
            };
            self.recency.remove(0);
            self.entries.remove(&victim);
        }
    }

    /// Move `idx` to the most-recently-used end.
    fn touch(&mut self, idx: usize) {
        if let Some(position) = self.recency.iter().position(|&other| other == idx) {
            self.recency.remove(position);
        }
        self.recency.push(idx);
    }

    /// Insert (or refresh) a record, evicting until the bound holds.
    fn insert(&mut self, idx: usize, record: NodeRecord) {
        self.touch(idx);
        self.entries.insert(idx, record);
        while self.entries.len() > self.capacity {
            let Some(victim) = self.recency.first().copied() else {
                break;
            };
            self.recency.remove(0);
            self.entries.remove(&victim);
        }
    }

    /// The record for `idx`, from the cache or, on a miss, from storage.
    ///
    /// See the module note on read-your-writes: a miss goes to [`Storage::get_row`],
    /// so the backend must expose its own buffered writes to reads.
    fn fetch(
        &mut self,
        idx: usize,
        dim: usize,
        encoding: VectorEncoding,
        node_count: usize,
        storage: &dyn Storage,
        namespace: &str,
    ) -> Result<NodeRecord> {
        if let Some(record) = self.entries.get(&idx).cloned() {
            self.touch(idx);
            return Ok(record);
        }
        let bytes = storage.get_row(namespace, idx as RowId)?.ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "paged HNSW node {idx} is referenced but absent from storage"
            ))
        })?;
        let record = NodeRecord::decode(&bytes, dim, encoding, node_count)?;
        self.insert(idx, record.clone());
        Ok(record)
    }
}

/// A paged HNSW index over a [`Storage`] backend.
///
/// See the [module note](self) for the memory model and the read-your-writes
/// precondition.
pub struct PagedHnswIndex<S: Storage> {
    storage: S,
    /// The synthetic table node records are stored under.
    namespace: String,
    dim: usize,
    /// The representation every node's vector is stored in — a property of
    /// the whole index, fixed at construction from the column's declared
    /// type and checked against the header on [`PagedHnswIndex::open`]. See
    /// [`crate::hnsw::VectorEncoding`].
    encoding: VectorEncoding,
    /// Number of nodes in the graph (live plus tombstoned).
    node_count: usize,
    /// Node index of the entry point.
    entry: Option<usize>,
    entry_level: usize,
    tombstones: usize,
    /// Row id -> node index, for the *live* nodes.
    live: BTreeMap<RowId, usize>,
    /// Row ids (with embeddings) inserted since the last commit, in order.
    pending_inserts: Vec<(RowId, Vec<f32>)>,
    /// Row ids removed since the last commit, in order.
    pending_removes: Vec<RowId>,
    cache: RefCell<NodeCache>,
    distance_calls: Cell<u64>,
    params: HnswParams,
    built_m: usize,
    built_ef_construction: usize,
    /// Whether this index makes its own graph durable. False when it shares a
    /// transaction with a caller that will commit for it — see the
    /// [module note](self#read-your-writes-and-who-owns-the-transaction).
    owns_transaction: bool,
    /// The write version the graph in storage describes, as restored or as last
    /// completed. `None` means "not current": a fresh index, or one a crash
    /// caught between batches.
    stored_version: Option<u64>,
    /// The write version the commit in flight will describe.
    pending_version: Option<u64>,
}

impl<S: Storage> PagedHnswIndex<S> {
    /// An empty index over vectors of `dim`, with [`HnswParams::DEFAULT`] and
    /// the default cache size, on a fresh namespace of `storage`.
    pub fn new(storage: S, namespace: impl Into<String>, dim: usize) -> Self {
        Self::with_params(storage, namespace, dim, HnswParams::DEFAULT)
    }

    /// An empty int8-quantised index over vectors of `dim`, mirroring
    /// [`crate::hnsw::HnswIndex::new_quantized`]: a `VECTOR(n, INT8)` column
    /// gets the same ~4x storage/memory win here that it already gets from
    /// the in-memory index.
    pub fn new_quantized(storage: S, namespace: impl Into<String>, dim: usize) -> Self {
        Self::with_encoding(
            storage,
            namespace,
            dim,
            HnswParams::DEFAULT,
            VectorEncoding::Q8,
        )
    }

    /// An empty index with explicit tuning.
    pub fn with_params(
        storage: S,
        namespace: impl Into<String>,
        dim: usize,
        params: HnswParams,
    ) -> Self {
        Self::with_encoding(storage, namespace, dim, params, VectorEncoding::Exact)
    }

    fn with_encoding(
        storage: S,
        namespace: impl Into<String>,
        dim: usize,
        params: HnswParams,
        encoding: VectorEncoding,
    ) -> Self {
        Self {
            storage,
            namespace: namespace.into(),
            dim,
            encoding,
            node_count: 0,
            entry: None,
            entry_level: 0,
            tombstones: 0,
            live: BTreeMap::new(),
            pending_inserts: Vec::new(),
            pending_removes: Vec::new(),
            cache: RefCell::new(NodeCache::new(DEFAULT_CACHE_NODES)),
            distance_calls: Cell::new(0),
            params,
            built_m: params.m,
            built_ef_construction: params.ef_construction,
            owns_transaction: true,
            stored_version: None,
            pending_version: None,
        }
    }

    /// Open a previously committed index from `storage`, restoring its header
    /// and `live` map so it answers immediately without a rebuild.
    pub fn open(storage: S, namespace: impl Into<String>, dim: usize) -> Result<Self> {
        let mut index = Self::new(storage, namespace, dim);
        index.restore()?;
        Ok(index)
    }

    /// Open a previously committed int8-quantised index. See
    /// [`PagedHnswIndex::new_quantized`].
    ///
    /// If the namespace instead holds a graph written under a different
    /// encoding — most commonly a database created before quantised paged
    /// indexes existed, where every paged graph was written exact regardless
    /// of the column's declared type — [`PagedHnswIndex::restore`] purges it
    /// and comes back empty rather than misreading it, and the caller's usual
    /// stale-index handling rebuilds it from the rows. See the module note on
    /// the header format.
    pub fn open_quantized(storage: S, namespace: impl Into<String>, dim: usize) -> Result<Self> {
        let mut index = Self::new_quantized(storage, namespace, dim);
        index.restore()?;
        Ok(index)
    }

    /// Set the resident working-set bound, in decoded nodes.
    pub fn with_cache_capacity(mut self, nodes: usize) -> Self {
        self.cache.get_mut().set_capacity(nodes);
        self
    }

    /// Write the graph into the caller's open transaction and leave the commit
    /// to the caller.
    ///
    /// Use this whenever the backing storage is shared with something else that
    /// is mid-transaction — inside the engine, always. Committing here would
    /// make the sharer's buffered writes durable at a moment it did not choose,
    /// which is the difference between "the rows and their index land together"
    /// and "half a statement is on disk". See the
    /// [module note](self#read-your-writes-and-who-owns-the-transaction).
    pub fn joined_to_caller_transaction(mut self) -> Self {
        self.owns_transaction = false;
        self
    }

    /// Whether this index will call [`Storage::commit`] itself.
    pub fn owns_transaction(&self) -> bool {
        self.owns_transaction
    }

    /// Delete the whole graph, leaving an empty index over the same namespace.
    ///
    /// The engine calls this when it has decided to rebuild from the rows —
    /// without it, re-inserting every row into a graph that still holds the old
    /// nodes would tombstone each one and roughly double the node count for no
    /// gain. Like every other write here, the deletions go into the open
    /// transaction and become durable when it commits.
    ///
    /// Deletes every row the namespace actually holds, found by scanning it,
    /// rather than trusting `0..self.node_count`. The two agree whenever this
    /// index restored its own header, but not for one [`PagedHnswIndex::open`]
    /// left at its constructed default of zero because [`PagedHnswIndex::restore`]
    /// found a header written under a different [`VectorEncoding`] and refused
    /// to trust it — the namespace can still hold that older graph's rows, and
    /// they have to go before the rebuild-from-rows this call is always a
    /// prelude to writes fresh ones on top.
    pub fn clear(&mut self) -> Result<()> {
        let stale: Vec<RowId> = RowScan::new(&self.storage, &self.namespace)
            .map(|row| row.map(|(id, _)| id))
            .collect::<Result<_>>()?;
        for idx in stale {
            self.storage.delete_row(&self.namespace, idx)?;
        }
        self.node_count = 0;
        self.pending_inserts.clear();
        self.pending_removes.clear();
        self.reset_graph();
        // Until something completes the rebuild, there is no current graph
        // here — say so, rather than leaving the old stamp on an empty one.
        self.stored_version = None;
        self.write_header(None)
    }

    /// How many decoded nodes are resident right now.
    pub fn cache_len(&self) -> usize {
        self.cache.borrow().len()
    }

    /// The bound `cache_len` will never exceed.
    pub fn cache_capacity(&self) -> usize {
        self.cache.borrow().capacity()
    }

    /// Bytes occupied by vector payloads currently resident in the node
    /// cache. Everything outside the cache lives in `storage`, not in memory —
    /// that bound is the whole point of this backend, see the
    /// [module note](self). Container and adjacency overhead are intentionally
    /// excluded so exact and int8 columns are directly comparable, mirroring
    /// [`crate::hnsw::HnswIndex::resident_vector_bytes`].
    pub fn resident_vector_bytes(&self) -> usize {
        self.cache
            .borrow()
            .entries
            .values()
            .map(|record| record.vector.payload_bytes())
            .sum()
    }

    /// Number of live indexed embeddings.
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// Whether the index holds no embeddings.
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Distance computations since the last reset. See
    /// [`crate::hnsw::HnswIndex::distance_calls`] for why this is a count.
    pub fn distance_calls(&self) -> u64 {
        self.distance_calls.get()
    }

    /// Reset the distance counter.
    pub fn reset_distance_calls(&self) {
        self.distance_calls.set(0);
    }

    /// Retune. `m` and `ef_construction` shape the graph and take effect on the
    /// next [`VectorIndex::commit`]; `ef_search` applies to the next query.
    pub fn set_params(&mut self, params: HnswParams) {
        self.params = params;
    }

    /// The tuning in force.
    pub fn params(&self) -> HnswParams {
        self.params
    }

    /// Hand back the backing storage, dropping the in-memory working set.
    ///
    /// The graph is already durable in `storage`, so this is how a caller that
    /// wants to reopen the index later recovers the handle.
    pub fn into_storage(self) -> S {
        self.storage
    }

    fn header_key(&self) -> String {
        let mut key = self.namespace.clone();
        key.push_str(":header");
        key
    }

    /// Restore node count, entry point and the `live` map from a prior commit.
    ///
    /// The header carries an encoding tag as its very last byte — appended
    /// there, after the (already optional) write-version stamp, rather than
    /// up front where `dim` is. Every earlier field's position and width
    /// predates this change, so a leading tag would shift them for a header
    /// this method still has to read; a trailing one lets an absent byte mean
    /// exactly what it always did before quantised paged indexes existed:
    /// exact. See the [module note](self) — there is no
    /// `CATALOG_VERSION_*`-style version number for this header at all, so
    /// this mirrors the one compatibility trick the format already used
    /// (the write-version stamp is read the same optional-trailing-byte way).
    fn restore(&mut self) -> Result<()> {
        let Some(bytes) = self.storage.get_meta(&self.header_key())? else {
            return Ok(());
        };
        let mut cursor = Cursor::new(&bytes);
        let dim = cursor.u32()? as usize;
        if dim != self.dim {
            return Err(Error::Corrupt(alloc::format!(
                "paged HNSW header declares dimension {dim} but the column expects {}",
                self.dim
            )));
        }
        let node_count = cursor.u32()? as usize;
        let has_entry = cursor.u8()? != 0;
        let entry = cursor.u32()? as usize;
        let entry_level = cursor.u32()? as usize;
        let tombstones = cursor.u32()? as usize;
        // The stamp is absent on a header written mid-build, which is how a
        // graph a crash left half-finished is told apart from a complete one.
        let stored_version = match cursor.u8() {
            Ok(1) => Some(u64::from_le_bytes(cursor.array8()?)),
            _ => None,
        };
        // Absent entirely on a header written before quantised paged indexes
        // existed: every paged graph was implicitly exact then, whatever the
        // column declared, because the engine did not yet wire the column's
        // encoding through to this backend.
        let encoding = match cursor.u8() {
            Ok(0) => VectorEncoding::Exact,
            Ok(1) => VectorEncoding::Q8,
            Ok(_) => {
                return Err(Error::Corrupt(alloc::format!(
                    "paged HNSW header for {} has an unrecognised vector encoding tag",
                    self.namespace
                )))
            }
            Err(_) => VectorEncoding::Exact,
        };
        if encoding != self.encoding {
            // The graph on disk was written under a different encoding than
            // the column now declares. There is nothing to salvage: a Q8
            // node's payload is the wrong width to reinterpret as `f32`s (and
            // vice versa).
            //
            // Purge it here rather than just leaving `self` at its
            // constructed default: every other reason this method leaves
            // `node_count` at zero (a fresh namespace, or one `clear` already
            // emptied) also means the namespace itself is empty, and losing
            // that invariant would leave the wrong-encoding rows for
            // `PagedHnswIndex::build` to trip over the moment a caller
            // commits without first calling `reset` — the standard protocol
            // (`Engine::reset_self_persisting_indexes`) always does, but this
            // type's own invariants should not depend on every caller
            // following it. `clear` leaves `stored_version: None`, so the
            // caller's existing "the saved copy is not current" handling
            // still rebuilds it from the rows. See `Engine::load_saved_indexes`.
            return self.clear();
        }

        self.node_count = node_count;
        self.entry = has_entry.then_some(entry);
        self.entry_level = entry_level;
        self.tombstones = tombstones;
        self.stored_version = stored_version;
        self.built_m = self.params.m;
        self.built_ef_construction = self.params.ef_construction;

        // Rebuild the id -> index map by scanning the (small) node records, not
        // the embeddings. One pass, one record in memory at a time.
        //
        // Records at or beyond `node_count` are not part of this graph: a
        // rebuild that shrank, or a batch that a crash interrupted, can leave
        // them behind. The header is what says how far the graph goes.
        self.live.clear();
        for row in RowScan::new(&self.storage, &self.namespace) {
            let (index, bytes) = row?;
            if index as usize >= node_count {
                continue;
            }
            let record = NodeRecord::decode(&bytes, self.dim, self.encoding, node_count)?;
            if !record.deleted {
                self.live.insert(record.id, index as usize);
            }
        }
        Ok(())
    }

    /// Read a node record, through the cache.
    fn fetch_node(&self, idx: usize) -> Result<NodeRecord> {
        self.cache.borrow_mut().fetch(
            idx,
            self.dim,
            self.encoding,
            self.node_count,
            &self.storage,
            &self.namespace,
        )
    }

    /// Read a node's vector, through the cache.
    fn fetch_vector(&self, idx: usize) -> Result<StoredVector> {
        self.fetch_node(idx).map(|record| record.vector)
    }

    /// Write a node record to storage (buffered until the backend commits) and
    /// into the cache.
    fn store_node(&mut self, idx: usize, record: NodeRecord) -> Result<()> {
        self.storage
            .put_row(&self.namespace, idx as RowId, &record.encode())?;
        self.cache.get_mut().insert(idx, record);
        self.flush_if_transaction_is_full()
    }

    /// Commit the batch so far when the backend says the open transaction is
    /// close to its limit.
    ///
    /// Building a graph over a large corpus writes far more than one
    /// transaction can hold — a write-ahead log region is a hard ceiling, not a
    /// slow path — so the build has to be broken up. The header written here
    /// carries **no version stamp**, so a crash between batches leaves a graph
    /// that is structurally sound (every node it claims exists) but visibly not
    /// current, and the engine rebuilds it rather than trusting it.
    ///
    /// Nothing is flushed when the caller owns the transaction: committing then
    /// would make the caller's own buffered writes durable early, which is the
    /// one thing this index must never do.
    fn flush_if_transaction_is_full(&mut self) -> Result<()> {
        if !self.owns_transaction || !self.storage.transaction_is_nearly_full() {
            return Ok(());
        }
        self.write_header(None)?;
        self.storage.commit()
    }

    /// Persist the graph header, then commit the node writes — unless the
    /// caller owns the transaction, in which case the writes stay buffered for
    /// it to commit with everything else it has in flight.
    fn finish(&mut self) -> Result<()> {
        // Only here, on the header that completes the graph, does the version
        // stamp go in.
        self.write_header(self.pending_version)?;
        self.stored_version = self.pending_version;
        if self.owns_transaction {
            self.storage.commit()
        } else {
            Ok(())
        }
    }

    /// Write the header that lets [`PagedHnswIndex::open`] restore the graph.
    ///
    /// `version` is the write version the graph describes, or `None` for a
    /// graph that is not (yet) complete — see
    /// [`PagedHnswIndex::flush_if_transaction_is_full`].
    ///
    /// The encoding tag is written last, and always — see
    /// [`PagedHnswIndex::restore`] for why its position is load-bearing.
    fn write_header(&mut self, version: Option<u64>) -> Result<()> {
        let mut out = Vec::new();
        put_len(&mut out, self.dim);
        put_len(&mut out, self.node_count);
        match self.entry {
            Some(entry) => {
                out.push(1);
                put_len(&mut out, entry);
            }
            None => {
                out.push(0);
                put_len(&mut out, 0);
            }
        }
        put_len(&mut out, self.entry_level);
        put_len(&mut out, self.tombstones);
        match version {
            Some(version) => {
                out.push(1);
                out.extend_from_slice(&version.to_le_bytes());
            }
            None => out.push(0),
        }
        out.push(match self.encoding {
            VectorEncoding::Exact => 0,
            VectorEncoding::Q8 => 1,
        });
        self.storage.put_meta(&self.header_key(), &out)
    }

    fn reset_graph(&mut self) {
        self.entry = None;
        self.entry_level = 0;
        self.tombstones = 0;
        self.built_m = self.params.m;
        self.built_ef_construction = self.params.ef_construction;
        self.live.clear();
        self.cache.get_mut().clear();
    }

    /// Rebuild the graph from the committed node records plus the pending batch,
    /// inserting in row-id order for determinism.
    fn build(&mut self) -> Result<()> {
        // The final live set: committed live nodes from storage, minus pending
        // removes, plus pending inserts (whose embeddings are still in memory).
        let mut live: BTreeMap<RowId, StoredVector> = BTreeMap::new();
        let old_count = self.node_count;
        for row in RowScan::new(&self.storage, &self.namespace) {
            let (_, bytes) = row?;
            let record = NodeRecord::decode(&bytes, self.dim, self.encoding, old_count)?;
            if !record.deleted {
                live.insert(record.id, record.vector);
            }
        }
        for id in &self.pending_removes {
            live.remove(id);
        }
        for (id, embedding) in &self.pending_inserts {
            live.insert(
                *id,
                StoredVector::from_f32(&normalise(embedding), self.encoding),
            );
        }

        self.reset_graph();
        self.node_count = 0;

        let shift = level_shift(self.params.m);
        let ceiling = max_level_for(live.len(), self.params.m);
        let mut visited = Visited::new(live.len());
        for (id, vector) in live {
            let level = level_of(id, shift, ceiling);
            self.insert_node(id, vector, level, &mut visited)?;
        }

        // A rebuild can shrink (tombstones dropped), leaving stale records.
        for idx in self.node_count..old_count {
            self.storage.delete_row(&self.namespace, idx as RowId)?;
        }
        Ok(())
    }

    /// Greedily insert one node, connecting it to `M` neighbours per layer.
    fn insert_node(
        &mut self,
        id: RowId,
        vector: StoredVector,
        level: usize,
        visited: &mut Visited,
    ) -> Result<()> {
        let Some(mut ep) = self.entry else {
            let record = NodeRecord {
                id,
                deleted: false,
                vector,
                neighbors: vec![Vec::new(); level + 1],
            };
            self.store_node(0, record)?;
            self.node_count = 1;
            self.entry = Some(0);
            self.entry_level = level;
            self.live.insert(id, 0);
            return Ok(());
        };

        let new_index = self.node_count;
        // Add the node with empty adjacency first, so the greedy walk and the
        // reverse edges it will create have a record to read.
        let mut record = NodeRecord {
            id,
            deleted: false,
            vector: vector.clone(),
            neighbors: vec![Vec::new(); level + 1],
        };
        self.store_node(new_index, record.clone())?;
        self.node_count += 1;
        self.live.insert(id, new_index);

        // Descend from the top layer to this node's top layer.
        let mut current = self.entry_level;
        while current > level {
            let nearest = self.search_layer(&vector, ep, 1, current, visited)?;
            ep = nearest[0].node;
            current -= 1;
        }

        for layer in (0..=current).rev() {
            let candidates =
                self.search_layer(&vector, ep, self.params.ef_construction, layer, visited)?;
            ep = candidates[0].node;
            let degree = self.params.degree(layer);
            let selected = self.select_neighbors(&candidates, degree)?;
            record.neighbors[layer] = selected.clone();
            for neighbor in selected {
                self.link_back(neighbor, new_index, layer, degree)?;
            }
        }

        // Re-store with the filled adjacency.
        self.store_node(new_index, record)?;
        if level > self.entry_level {
            self.entry = Some(new_index);
            self.entry_level = level;
        }
        Ok(())
    }

    /// Best-first search of one layer, fetching node records on demand.
    ///
    /// `query` stays exact even when the index is quantised, for the same
    /// reason [`crate::hnsw::HnswIndex::search`] keeps it exact: quantising
    /// the corpus is the declared storage trade-off, and throwing away query
    /// precision too would add recall loss without saving resident memory.
    fn search_layer(
        &self,
        query: &StoredVector,
        entry: usize,
        ef: usize,
        layer: usize,
        visited: &mut Visited,
    ) -> Result<Vec<Candidate>> {
        let ef = ef.max(1);
        visited.restart(self.node_count);

        let mut frontier: BinaryHeap<Reverse<Candidate>> = BinaryHeap::new();
        let mut results: BinaryHeap<Candidate> = BinaryHeap::new();

        let entry_vector = self.fetch_vector(entry)?;
        let start = Candidate {
            distance: stored_distance(&self.distance_calls, query, &entry_vector),
            node: entry,
        };
        visited.visit(entry);
        frontier.push(Reverse(start));
        results.push(start);

        while let Some(Reverse(current)) = frontier.pop() {
            if results.len() >= ef && current.distance > results.peek().expect("non-empty").distance
            {
                break;
            }
            let record = self.fetch_node(current.node)?;
            let neighbors = record.neighbors.get(layer).cloned().unwrap_or_default();
            for neighbor in neighbors {
                if !visited.visit(neighbor) {
                    continue;
                }
                let neighbor_vector = self.fetch_vector(neighbor)?;
                let candidate = Candidate {
                    distance: stored_distance(&self.distance_calls, query, &neighbor_vector),
                    node: neighbor,
                };
                let worst = results.peek().expect("non-empty").distance;
                if results.len() < ef || candidate.distance < worst {
                    frontier.push(Reverse(candidate));
                    results.push(candidate);
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }

        let mut out = results.into_vec();
        out.sort_unstable();
        Ok(out)
    }

    /// The neighbour heuristic, with vectors fetched on demand. See
    /// [`crate::hnsw`] for why it keeps a few further-out candidates rather than
    /// truncating to the nearest.
    fn select_neighbors(&self, candidates: &[Candidate], degree: usize) -> Result<Vec<usize>> {
        let mut selected: Vec<usize> = Vec::with_capacity(degree);
        for candidate in candidates {
            if selected.len() >= degree {
                break;
            }
            let candidate_vector = self.fetch_vector(candidate.node)?;
            let mut diverse = true;
            for &kept in &selected {
                let kept_vector = self.fetch_vector(kept)?;
                if stored_distance(&self.distance_calls, &candidate_vector, &kept_vector)
                    <= candidate.distance
                {
                    diverse = false;
                    break;
                }
            }
            if diverse {
                selected.push(candidate.node);
            }
        }
        // If the heuristic is too strict to fill the budget, the nearest
        // rejected candidates make up the difference.
        if selected.len() < degree {
            for candidate in candidates {
                if selected.len() >= degree {
                    break;
                }
                if !selected.contains(&candidate.node) {
                    selected.push(candidate.node);
                }
            }
        }
        Ok(selected)
    }

    /// Add the reverse edge `neighbor -> new_index`, pruning `neighbor`'s list
    /// back to `degree` if that pushed it over.
    fn link_back(
        &mut self,
        neighbor: usize,
        new_index: usize,
        layer: usize,
        degree: usize,
    ) -> Result<()> {
        let mut record = self.fetch_node(neighbor)?;
        record.neighbors[layer].push(new_index);
        if record.neighbors[layer].len() <= degree {
            self.store_node(neighbor, record)?;
            return Ok(());
        }

        let neighbor_vector = record.vector.clone();
        let mut candidates: Vec<Candidate> = Vec::with_capacity(record.neighbors[layer].len());
        for &other in &record.neighbors[layer] {
            let other_vector = self.fetch_vector(other)?;
            candidates.push(Candidate {
                distance: stored_distance(&self.distance_calls, &neighbor_vector, &other_vector),
                node: other,
            });
        }
        candidates.sort_unstable();
        record.neighbors[layer] = self.select_neighbors(&candidates, degree)?;
        self.store_node(neighbor, record)
    }

    /// Mark a node deleted, leaving it in the graph for navigation but out of
    /// search results and out of `live`.
    fn tombstone(&mut self, index: usize) -> Result<()> {
        let mut record = self.fetch_node(index)?;
        if record.deleted {
            return Ok(());
        }
        let id = record.id;
        record.deleted = true;
        self.store_node(index, record)?;
        self.tombstones += 1;
        self.live.remove(&id);
        Ok(())
    }

    /// Re-point `entry` at the highest-level live node after the previous entry
    /// was tombstoned.
    fn repick_entry(&mut self) -> Result<()> {
        let mut best: Option<usize> = None;
        for index in 0..self.node_count {
            let record = self.fetch_node(index)?;
            if record.deleted {
                continue;
            }
            best = Some(match best {
                None => index,
                Some(current) => {
                    let current_level = self.fetch_node(current)?.neighbors.len().saturating_sub(1);
                    if record.neighbors.len().saturating_sub(1) > current_level {
                        index
                    } else {
                        current
                    }
                }
            });
        }
        match best {
            Some(index) => {
                self.entry = Some(index);
                self.entry_level = self.fetch_node(index)?.neighbors.len().saturating_sub(1);
            }
            None => {
                self.entry = None;
                self.entry_level = 0;
            }
        }
        Ok(())
    }
}

impl<S: Storage> VectorIndex for PagedHnswIndex<S> {
    fn insert(&mut self, id: RowId, embedding: &[f32]) -> Result<()> {
        if embedding.len() != self.dim {
            return Err(Error::Type(alloc::format!(
                "embedding has dimension {} but the index expects {}",
                embedding.len(),
                self.dim
            )));
        }
        self.pending_inserts.push((id, embedding.to_vec()));
        Ok(())
    }

    fn remove(&mut self, id: RowId) -> Result<()> {
        self.pending_removes.push(id);
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let reshaped = self.params.m != self.built_m
            || self.params.ef_construction != self.built_ef_construction;
        let pending = !self.pending_inserts.is_empty() || !self.pending_removes.is_empty();
        if !pending && !reshaped {
            // Nothing changed here, but the database moved on — a write to some
            // other table advances the write version too. Restamping is one
            // metadata write and it is the difference between reopening
            // instantly and rebuilding this graph from the rows for no reason.
            if self.pending_version != self.stored_version {
                return self.finish();
            }
            return Ok(());
        }

        // The first commit has no graph to grow, and a retune has to re-derive
        // the whole graph under the new parameters. Either way: rebuild.
        if self.node_count == 0 || reshaped {
            self.build()?;
            self.pending_inserts.clear();
            self.pending_removes.clear();
            return self.finish();
        }

        // Removals first, so an id removed and reinserted in the same window —
        // an update — leaves one tombstone behind and then a fresh node.
        let removes = core::mem::take(&mut self.pending_removes);
        for id in removes {
            if let Some(&index) = self.live.get(&id) {
                self.tombstone(index)?;
            }
        }

        // Inserts next, in arrival order.
        let inserts = core::mem::take(&mut self.pending_inserts);
        let shift = level_shift(self.params.m);
        let ceiling = max_level_for(self.live.len() + inserts.len(), self.params.m);
        let mut visited = Visited::new(self.node_count);
        for (id, embedding) in inserts {
            let vector = StoredVector::from_f32(&normalise(&embedding), self.encoding);
            let level = level_of(id, shift, ceiling);
            // A replace without an intervening remove retires the old node the
            // same way a remove would.
            if let Some(&old) = self.live.get(&id) {
                self.tombstone(old)?;
            }
            self.insert_node(id, vector, level, &mut visited)?;
        }

        // Repair. More tombstones than live nodes means over half the graph is
        // dead: rebuild.
        if self.tombstones * 2 >= self.node_count {
            self.build()?;
        } else if let Some(entry) = self.entry {
            if self.fetch_node(entry)?.deleted {
                self.repick_entry()?;
            }
        }
        self.finish()
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<Scored>> {
        if query.len() != self.dim {
            return Err(Error::Type(alloc::format!(
                "query has dimension {} but the index expects {}",
                query.len(),
                self.dim
            )));
        }
        let Some(mut ep) = self.entry else {
            return Ok(Vec::new());
        };
        if k == 0 {
            return Ok(Vec::new());
        }

        let query = StoredVector::Exact(normalise(query));
        let mut visited = Visited::new(self.node_count);
        for layer in (1..=self.entry_level).rev() {
            let nearest = self.search_layer(&query, ep, 1, layer, &mut visited)?;
            ep = nearest[0].node;
        }

        let hits = self.search_layer(&query, ep, self.params.ef_for(k), 0, &mut visited)?;
        let mut scored = Vec::with_capacity(k.min(hits.len()));
        for hit in hits {
            let record = self.fetch_node(hit.node)?;
            if record.deleted {
                continue;
            }
            scored.push(Scored::new(record.id, 1.0 - hit.distance));
            if scored.len() >= k {
                break;
            }
        }
        Ok(scored)
    }

    fn resident_vector_bytes(&self) -> Option<usize> {
        Some(PagedHnswIndex::resident_vector_bytes(self))
    }

    fn is_self_persisting(&self) -> bool {
        true
    }

    fn reset(&mut self) -> Result<()> {
        self.clear()
    }

    fn prepare_commit(&mut self, write_version: u64, may_commit: bool) {
        self.pending_version = Some(write_version);
        self.owns_transaction = may_commit;
    }

    fn stored_write_version(&self) -> Option<u64> {
        self.stored_version
    }

    /// The graph is already in the database; there is no blob to hand back, and
    /// asking for one would serialise the very thing this index exists not to
    /// hold in memory. Currency is tracked by the header stamp instead — see
    /// [`VectorIndex::stored_write_version`].
    fn save(&self) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{BruteForceVectorIndex, MemStorage};

    /// A fresh in-memory-backed paged index over `dim` dimensions.
    fn index(dim: usize) -> PagedHnswIndex<MemStorage> {
        PagedHnswIndex::new(MemStorage::new(), "hnsw", dim)
    }

    /// `count` deterministic pseudo-random vectors of `dim` components.
    fn vectors(count: u64, dim: usize) -> Vec<Vec<f32>> {
        let mut state = 0x51ed_2701_u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as f32 / u32::MAX as f32) - 0.5
        };
        (0..count)
            .map(|_| (0..dim).map(|_| next()).collect())
            .collect()
    }

    #[test]
    fn returns_the_closest_neighbour_first() {
        let mut index = index(3);
        index.insert(1, &[1.0, 0.0, 0.0]).unwrap();
        index.insert(2, &[0.0, 1.0, 0.0]).unwrap();
        index.insert(3, &[0.9, 0.1, 0.0]).unwrap();
        index.insert(4, &[0.0, 0.0, 1.0]).unwrap();
        index.commit().unwrap();
        let hits = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn searching_before_commit_returns_nothing() {
        let mut index = index(3);
        index.insert(1, &[1.0, 0.0, 0.0]).unwrap();
        assert!(index.search(&[1.0, 0.0, 0.0], 1).unwrap().is_empty());
        index.commit().unwrap();
        assert_eq!(index.search(&[1.0, 0.0, 0.0], 1).unwrap().len(), 1);
    }

    #[test]
    fn dimension_mismatch_is_an_error() {
        let mut index = index(3);
        assert!(index.insert(1, &[1.0]).is_err());
        assert!(index.search(&[1.0], 1).is_err());
    }

    #[test]
    fn two_builds_over_the_same_rows_agree() {
        let build = || {
            let mut index = index(4);
            for i in 0..20u64 {
                let angle = (i as f32) * 0.1;
                index
                    .insert(i + 1, &[angle.cos(), angle.sin(), 0.0, 0.0])
                    .unwrap();
            }
            index.commit().unwrap();
            index
                .search(&[1.0, 0.0, 0.0, 0.0], 5)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn recall_matches_the_brute_force_oracle() {
        let dim = 8;
        let rows = vectors(64, dim);
        let mut index = index(dim);
        let mut brute = BruteForceVectorIndex::new(dim);
        for (id, vector) in rows.iter().enumerate() {
            index.insert(id as u64 + 1, vector).unwrap();
            brute.insert(id as u64 + 1, vector).unwrap();
        }
        index.commit().unwrap();

        let query = &rows[rows.len() - 1];
        let approx: Vec<RowId> = index
            .search(query, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        let exact: Vec<RowId> = brute
            .search(query, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(approx[0], exact[0], "true nearest neighbour not found");
        let overlap = approx.iter().filter(|id| exact.contains(id)).count();
        assert!(
            overlap >= 8,
            "recall too low: approx {approx:?} exact {exact:?}"
        );
    }

    /// The whole point of the module: a corpus far larger than the cache is
    /// searchable, and the resident working set never exceeds the bound.
    #[test]
    fn a_corpus_larger_than_the_cache_is_searchable_with_bounded_memory() {
        let dim = 8;
        let count = 2_000u64;
        let capacity = 32;

        let mut index = index(dim).with_cache_capacity(capacity);
        let rows = vectors(count, dim);
        for (id, vector) in rows.iter().enumerate() {
            index.insert(id as u64 + 1, vector).unwrap();
        }
        index.commit().unwrap();

        // The corpus is ~2000 * (8*4 + ~4*2) bytes, orders of magnitude larger
        // than the 32-node cache, so it provably did not all fit in memory.
        for seed in 0..8 {
            let query: Vec<f32> = (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect();
            assert!(!index.search(&query, 5).unwrap().is_empty());
            assert!(
                index.cache_len() <= capacity,
                "cache grew to {} nodes, bound is {capacity}",
                index.cache_len()
            );
        }
    }

    /// A restored index answers exactly as the one that was committed.
    #[test]
    fn a_committed_index_reopens_without_rebuilding() {
        let dim = 6;
        let storage = MemStorage::new();
        let mut original = PagedHnswIndex::new(storage, "hnsw", dim);
        for (id, vector) in vectors(64, dim).into_iter().enumerate() {
            original.insert(id as u64 + 1, &vector).unwrap();
        }
        original.commit().unwrap();

        let query = |seed: usize| -> Vec<f32> {
            (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect()
        };
        let expected: Vec<Vec<RowId>> = (0..8)
            .map(|seed| {
                original
                    .search(&query(seed), 10)
                    .unwrap()
                    .into_iter()
                    .map(|h| h.id)
                    .collect()
            })
            .collect();

        let storage = original.into_storage();
        let restored = PagedHnswIndex::open(storage, "hnsw", dim).unwrap();
        for (seed, expected) in expected.iter().enumerate() {
            let got: Vec<RowId> = restored
                .search(&query(seed), 10)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect();
            assert_eq!(got, *expected, "restored graph diverged on query {seed}");
        }
    }

    #[test]
    fn removal_drops_the_embedding_and_round_trips() {
        let storage = MemStorage::new();
        {
            let mut index = PagedHnswIndex::new(storage.clone(), "hnsw", 3);
            index.insert(1, &[1.0, 0.0, 0.0]).unwrap();
            index.insert(2, &[0.0, 1.0, 0.0]).unwrap();
            index.commit().unwrap();
            index.remove(1).unwrap();
            index.commit().unwrap();
            let hits = index.search(&[1.0, 0.0, 0.0], 4).unwrap();
            assert!(hits.iter().all(|hit| hit.id != 1));
        }

        // A fresh handle over the same storage still hides the deleted row.
        let restored = PagedHnswIndex::open(storage, "hnsw", 3).unwrap();
        assert!(restored
            .search(&[1.0, 0.0, 0.0], 4)
            .unwrap()
            .iter()
            .all(|hit| hit.id != 1));
    }

    #[test]
    fn an_incremental_insert_does_not_touch_every_node() {
        let params = HnswParams {
            m: 8,
            ef_construction: 24,
            ef_search: 32,
            ef_search_multiplier: 1,
        };
        let count = 4_000u64;
        let mut index = PagedHnswIndex::with_params(MemStorage::new(), "hnsw", 4, params);
        for (id, vector) in vectors(count, 4).into_iter().enumerate() {
            index.insert(id as u64 + 1, &vector).unwrap();
        }
        index.commit().unwrap();

        index.reset_distance_calls();
        let extra = vectors(1, 4);
        index.insert(count + 1, &extra[0]).unwrap();
        index.commit().unwrap();

        let calls = index.distance_calls();
        assert!(
            calls < count / 2,
            "inserting one row into a {count}-node graph cost {calls} distance \
             computations; touching every node would be {count}"
        );
    }

    // ------------------------------------------------------- quantisation

    /// A fresh in-memory-backed *quantised* paged index over `dim` dimensions.
    fn quantized_index(dim: usize) -> PagedHnswIndex<MemStorage> {
        PagedHnswIndex::new_quantized(MemStorage::new(), "hnsw", dim)
    }

    #[test]
    fn quantized_index_round_trips_and_shrinks_resident_bytes() {
        let dim = 384;
        let rows = vectors(256, dim);

        let mut exact = index(dim);
        let mut quantized = quantized_index(dim);
        for (id, vector) in rows.iter().enumerate() {
            exact.insert(id as u64 + 1, vector).unwrap();
            quantized.insert(id as u64 + 1, vector).unwrap();
        }
        exact.commit().unwrap();
        quantized.commit().unwrap();

        // Warm both caches over the whole corpus so `resident_vector_bytes`
        // is comparing the same thing on both sides, not "whatever a handful
        // of searches happened to touch".
        for idx in 0..rows.len() {
            exact.fetch_node(idx).unwrap();
            quantized.fetch_node(idx).unwrap();
        }
        let exact_bytes = exact.resident_vector_bytes();
        let q8_bytes = quantized.resident_vector_bytes();
        assert!(
            exact_bytes * 100 >= q8_bytes * 390,
            "exact={exact_bytes} q8={q8_bytes}, expected roughly a 4x reduction"
        );

        // The graph still answers the same query the same way after a
        // save/reload of the quantised index — the round-trip pattern used
        // throughout this module's other tests, extended to Q8.
        let query = &rows[rows.len() - 1];
        let before: Vec<RowId> = quantized
            .search(query, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();

        let storage = quantized.into_storage();
        let restored = PagedHnswIndex::open_quantized(storage, "hnsw", dim).unwrap();
        let after: Vec<RowId> = restored
            .search(query, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(before, after, "reopened quantised graph diverged");
    }

    /// The paged quantised index must not lose meaningfully more recall than
    /// the in-memory quantised index over the same corpus and queries — see
    /// `hnsw::tests::quantized_graph_round_trips_and_shrinks_vector_memory`
    /// for the in-memory-only version of this property.
    #[test]
    fn quantized_paged_recall_matches_the_in_memory_quantized_index() {
        let dim = 32;
        let rows = vectors(512, dim);

        let mut paged = quantized_index(dim);
        let mut in_memory = crate::hnsw::HnswIndex::new_quantized(dim);
        for (id, vector) in rows.iter().enumerate() {
            paged.insert(id as u64 + 1, vector).unwrap();
            in_memory.insert(id as u64 + 1, vector).unwrap();
        }
        paged.commit().unwrap();
        in_memory.commit().unwrap();

        let mut paged_hits = 0usize;
        let mut in_memory_hits = 0usize;
        let mut brute = BruteForceVectorIndex::new(dim);
        for (id, vector) in rows.iter().enumerate() {
            brute.insert(id as u64 + 1, vector).unwrap();
        }
        brute.commit().unwrap();

        for seed in 0..10 {
            let query: Vec<f32> = (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect();
            let truth: Vec<RowId> = brute
                .search(&query, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect();
            let paged_found: Vec<RowId> = paged
                .search(&query, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect();
            let in_memory_found: Vec<RowId> = in_memory
                .search(&query, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect();
            paged_hits += paged_found.iter().filter(|id| truth.contains(id)).count();
            in_memory_hits += in_memory_found
                .iter()
                .filter(|id| truth.contains(id))
                .count();
        }

        // Both are the same algorithm over the same encoding, so their recall
        // should be close, not merely "both non-zero".
        let paged_recall = paged_hits as f64 / 100.0;
        let in_memory_recall = in_memory_hits as f64 / 100.0;
        assert!(
            (paged_recall - in_memory_recall).abs() <= 0.1,
            "paged recall {paged_recall:.3} diverged from in-memory recall {in_memory_recall:.3}"
        );
        assert!(
            paged_recall >= 0.7,
            "paged recall too low: {paged_recall:.3}"
        );
    }

    /// A paged header written before quantised paged indexes existed has no
    /// encoding tag at all — the trailing byte [`PagedHnswIndex::restore`]
    /// looks for simply is not there. It must still open, as exact, rather
    /// than fail or misread: this is the compatibility property the whole
    /// module note on the header format exists to state.
    #[test]
    fn a_header_without_an_encoding_tag_opens_as_exact() {
        let dim = 6;
        let storage = MemStorage::new();
        let mut original = PagedHnswIndex::new(storage.clone(), "hnsw", dim);
        for (id, vector) in vectors(32, dim).into_iter().enumerate() {
            original.insert(id as u64 + 1, &vector).unwrap();
        }
        original.commit().unwrap();

        let query = |seed: usize| -> Vec<f32> {
            (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect()
        };
        let expected: Vec<RowId> = original
            .search(&query(0), 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();

        // Simulate a header written before this change existed: strip the
        // trailing encoding byte this change appends.
        let header_key = original.header_key();
        let mut storage = original.into_storage();
        let header = storage.get_meta(&header_key).unwrap().unwrap();
        storage
            .put_meta(&header_key, &header[..header.len() - 1])
            .unwrap();
        storage.commit().unwrap();

        let restored = PagedHnswIndex::open(storage, "hnsw", dim).unwrap();
        let got: Vec<RowId> = restored
            .search(&query(0), 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(got, expected, "pre-encoding-tag header failed to restore");
    }

    /// A graph written exact — which is what every paged index wrote before
    /// the column's encoding was threaded through, whatever the column
    /// declared — must not be misread as quantised. Opening it with
    /// `open_quantized` has to come back empty (forcing the caller's usual
    /// rebuild-from-rows path), not an error and not silently wrong vectors.
    #[test]
    fn opening_an_exact_graph_as_quantized_forces_a_rebuild_rather_than_misreading() {
        let dim = 6;
        let storage = MemStorage::new();
        let mut original = PagedHnswIndex::new(storage.clone(), "hnsw", dim);
        for (id, vector) in vectors(32, dim).into_iter().enumerate() {
            original.insert(id as u64 + 1, &vector).unwrap();
        }
        original.commit().unwrap();
        let storage = original.into_storage();

        let mut mismatched = PagedHnswIndex::open_quantized(storage, "hnsw", dim)
            .expect("a mismatched encoding must not be a hard error");
        assert!(
            mismatched.is_empty(),
            "mismatched graph should not resurrect the old vectors"
        );
        assert_eq!(
            mismatched.stored_write_version(),
            None,
            "an encoding mismatch must look exactly like \"nothing saved\", so the \
             caller's normal staleness check rebuilds it"
        );
        assert!(mismatched.search(&[1.0; 6], 10).unwrap().is_empty());

        // And a rebuild — re-index every row, then commit, deliberately
        // *without* calling `reset()` first — must complete and answer
        // correctly. `Engine::reset_self_persisting_indexes` always calls
        // `reset()` before this in the real recovery sequence, but this
        // type's own invariants should not depend on every caller following
        // that protocol: `open_quantized` already purged the stale
        // exact-format rows above, which is what makes skipping `reset()`
        // here safe rather than a rebuild that either misreads the leftover
        // rows (if a purge trusted a stale `node_count`) or double-counts
        // them.
        let mut rows = vectors(32, dim);
        for (id, vector) in rows.iter().enumerate() {
            mismatched.insert(id as u64 + 1, vector).unwrap();
        }
        mismatched.commit().unwrap();
        assert_eq!(mismatched.len(), 32, "rebuild did not index every row");

        let query = rows.pop().unwrap();
        let hits = mismatched.search(&query, 1).unwrap();
        assert_eq!(
            hits[0].id, 32,
            "rebuilt quantised graph answered incorrectly"
        );

        // The rebuild is durable and reopens as quantised from here on: no
        // more leftover exact rows to trip over, and the header now agrees
        // with the column.
        let storage = mismatched.into_storage();
        let reopened = PagedHnswIndex::open_quantized(storage, "hnsw", dim).unwrap();
        assert_eq!(reopened.len(), 32);
        assert_eq!(reopened.search(&query, 1).unwrap()[0].id, 32);
    }
}
