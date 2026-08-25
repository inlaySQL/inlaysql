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
//! # Two handles, one graph
//!
//! Keeping the graph in the file is what makes this cheap, and it is also the
//! one thing about it that is genuinely harder than the in-RAM index. Two
//! `Database` handles on one database hold two of these over the *same*
//! namespace. When one of them rebuilds — which is what any handle does on
//! opening to a stamp that is not current — it renumbers every node
//! underneath the other, **without changing a row**, so nothing moves the
//! write version the engine watches on the other handle's behalf.
//!
//! [`PagedHnswIndex::adopt_stored_graph`] is the answer: the header is
//! re-read on every commit and every search rather than remembered, and the
//! decoded node cache is dropped whenever it moved. That method's comment has
//! the three things a stale copy gets wrong, and the loudest of them is not
//! the worst: a stale entry point starts the walk at whatever row now holds
//! that index and answers with the wrong neighbours, with no error anywhere.
//! [`crate::bm25_paged::PagedBm25Index`] does the same thing with term
//! ordinals, for the same reason.
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
use crate::traits::{RowFilter, RowId, RowScan, Scored, Storage, VectorIndex};

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
    ///
    /// `Cell`, along with the three below and `stored_version`, because these
    /// five describe *the file* rather than this instance, and the file is
    /// shared — see [`PagedHnswIndex::adopt_stored_graph`], which has to be
    /// able to correct them from a `&self` search as well as from a `&mut
    /// self` commit.
    node_count: Cell<usize>,
    /// Node index of the entry point.
    entry: Cell<Option<usize>>,
    entry_level: Cell<usize>,
    tombstones: Cell<usize>,
    /// Row id -> node index, for the *live* nodes.
    ///
    /// Not a `Cell`, and refreshed lazily rather than with the four scalars
    /// above, because rebuilding it costs one pass over every node record and
    /// **no read needs it**: a search answers out of the records it walks.
    /// Only maintenance reads it, and maintenance has a `&mut self` — see
    /// [`PagedHnswIndex::adopt_live_map`].
    live: BTreeMap<RowId, usize>,
    /// Whether `live` still describes the graph the file holds, or belongs to
    /// one another handle has since rebuilt out from under it.
    live_is_stale: Cell<bool>,
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
    stored_version: Cell<Option<u64>>,
    /// The write version the commit in flight will describe.
    pending_version: Option<u64>,
    /// The header bytes this instance last read out of the file or wrote into
    /// it, so [`PagedHnswIndex::adopt_stored_graph`] can tell "nobody has
    /// touched this since I last looked" from "somebody has" with one
    /// comparison and no decode. Empty until the first read or write.
    header_seen: RefCell<Vec<u8>>,
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
            node_count: Cell::new(0),
            entry: Cell::new(None),
            entry_level: Cell::new(0),
            tombstones: Cell::new(0),
            live: BTreeMap::new(),
            live_is_stale: Cell::new(false),
            pending_inserts: Vec::new(),
            pending_removes: Vec::new(),
            cache: RefCell::new(NodeCache::new(DEFAULT_CACHE_NODES)),
            distance_calls: Cell::new(0),
            params,
            built_m: params.m,
            built_ef_construction: params.ef_construction,
            owns_transaction: true,
            stored_version: Cell::new(None),
            pending_version: None,
            header_seen: RefCell::new(Vec::new()),
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
        self.node_count.set(0);
        self.pending_inserts.clear();
        self.pending_removes.clear();
        self.reset_graph();
        // Until something completes the rebuild, there is no current graph
        // here — say so, rather than leaving the old stamp on an empty one.
        self.stored_version.set(None);
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
    ///
    /// Read out of the header scalars rather than out of `live`, which is the
    /// same number — every insert adds a node and a `live` entry, every
    /// tombstone removes a `live` entry and adds a tombstone — but only these
    /// two are corrected by [`PagedHnswIndex::adopt_stored_graph`] on a
    /// `&self` call. Counting the map instead would keep answering with a
    /// graph another handle has already replaced.
    pub fn len(&self) -> usize {
        self.node_count.get().saturating_sub(self.tombstones.get())
    }

    /// Whether the index holds no embeddings.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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

        self.node_count.set(node_count);
        self.entry.set(has_entry.then_some(entry));
        self.entry_level.set(entry_level);
        self.tombstones.set(tombstones);
        self.stored_version.set(stored_version);
        *self.header_seen.borrow_mut() = bytes;
        self.built_m = self.params.m;
        self.built_ef_construction = self.params.ef_construction;
        self.load_live_map()
    }

    /// Rebuild the `RowId -> node index` map from the node records the header
    /// says are part of the graph.
    ///
    /// The (small) records, not the embeddings: one pass, one record in memory
    /// at a time. Records at or beyond `node_count` are not part of this
    /// graph — a rebuild that shrank, or a batch that a crash interrupted, can
    /// leave them behind. The header is what says how far the graph goes.
    fn load_live_map(&mut self) -> Result<()> {
        let node_count = self.node_count.get();
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
        self.live_is_stale.set(false);
        Ok(())
    }

    /// Re-read the header, and throw away everything decoded from the old
    /// graph if it moved.
    ///
    /// **This is the price of keeping the graph in the file rather than in the
    /// handle**, and it is not optional. Two `Database` handles on one
    /// database each hold their own `PagedHnswIndex` over the *same*
    /// namespace, so the other one deciding to rebuild — which is what every
    /// handle does when it opens on a stamp that is not current — reassigns
    /// every node index underneath this one, **without changing a row** and
    /// therefore without moving the write version this handle watches. What
    /// this instance remembered is then wrong in ways that do not announce
    /// themselves:
    ///
    /// * `entry` and `entry_level` are where a search *starts*. A stale pair
    ///   starts the walk at whatever row now occupies that index, so the query
    ///   comes back with the wrong neighbours — a wrong answer with no error
    ///   anywhere, which is exactly what `docs/indexes.md` says this design has
    ///   to make impossible.
    /// * `node_count` is what bounds a neighbour index
    ///   ([`NodeRecord::decode`]) and sizes the visited set. Stale-low it
    ///   refuses a perfectly good record as corrupt; stale-high it hands
    ///   [`PagedHnswIndex::insert_node`] an index that overwrites a node the
    ///   other handle's graph is still using.
    /// * The node cache is keyed by node index, so every entry in it names a
    ///   different row than it did.
    ///
    /// So the header is re-read rather than remembered, on every commit and
    /// every search, exactly as
    /// [`crate::bm25_paged::PagedBm25Index::adopt_stored_statistics`] re-reads
    /// its corpus statistics. It costs one metadata read, and the common case —
    /// nobody else wrote — is a byte comparison against `header_seen` with no
    /// decode and nothing thrown away. `live` is *not* rebuilt here; see
    /// [`PagedHnswIndex::adopt_live_map`] for why that waits for a `&mut self`.
    ///
    /// A header this build cannot read — a foreign dimension or encoding — is
    /// left alone rather than acted on. Both are handled once, loudly, at
    /// [`PagedHnswIndex::restore`], where there is a `&mut self` to purge with.
    ///
    /// **What this cannot see**, stated rather than left to be rediscovered: a
    /// foreign rebuild that leaves the header byte-identical. The header is
    /// what the file publishes about the graph, and nothing in it counts
    /// rebuilds, so two graphs with the same node count, the same tombstone
    /// count and the same entry at the same level are indistinguishable from
    /// here. Reaching that needs a handle whose graph was built by a *different
    /// insert order* over the same rows with no tombstones — the graph is a
    /// pure function of the insert sequence, so same order means the same graph
    /// and nothing to adopt — and the max-level row landing on the same index
    /// under both orders. Closing it properly means a rebuild counter in the
    /// header, which is a format change and needs its own answer for two
    /// handles bumping it to the same value.
    /// [`crate::bm25_paged::PagedBm25Index::adopt_stored_statistics`] has the
    /// same residual for the same reason.
    fn adopt_stored_graph(&self) -> Result<()> {
        let Some(bytes) = self.storage.get_meta(&self.header_key())? else {
            // No header at all: nothing has ever been committed here, so
            // whatever this instance holds is its own uncommitted work.
            return Ok(());
        };
        if *self.header_seen.borrow() == bytes {
            return Ok(());
        }
        let mut cursor = Cursor::new(&bytes);
        let (Ok(dim), Ok(node_count), Ok(has_entry), Ok(entry), Ok(entry_level), Ok(tombstones)) = (
            cursor.u32(),
            cursor.u32(),
            cursor.u8(),
            cursor.u32(),
            cursor.u32(),
            cursor.u32(),
        ) else {
            return Ok(());
        };
        if dim as usize != self.dim {
            return Ok(());
        }
        let stored_version = match cursor.u8() {
            Ok(1) => cursor.array8().ok().map(u64::from_le_bytes),
            _ => None,
        };
        let encoding = match cursor.u8() {
            Ok(1) => VectorEncoding::Q8,
            // Absent entirely on a header written before quantised paged
            // indexes existed — see [`PagedHnswIndex::restore`].
            Ok(0) | Err(_) => VectorEncoding::Exact,
            Ok(_) => return Ok(()),
        };
        if encoding != self.encoding {
            return Ok(());
        }
        self.node_count.set(node_count as usize);
        self.entry.set((has_entry != 0).then_some(entry as usize));
        self.entry_level.set(entry_level as usize);
        self.tombstones.set(tombstones as usize);
        // Unconditionally: the stamp is what the *file* claims, and another
        // handle can restamp without moving a count — or leave no stamp at
        // all, having been caught mid-build. This instance's copy of that
        // claim is a cache like any other.
        self.stored_version.set(stored_version);
        // Everything decoded out of the old graph is suspect, node indices
        // most of all.
        self.cache.borrow_mut().clear();
        self.live_is_stale.set(true);
        *self.header_seen.borrow_mut() = bytes;
        Ok(())
    }

    /// Rebuild `live` when [`PagedHnswIndex::adopt_stored_graph`] found the
    /// graph replaced.
    ///
    /// Split out of the adopt itself because it is the one `O(nodes)` step on
    /// this path and only maintenance needs it: a search never consults the
    /// map, it reads each record's own row id as it walks. Paying a full node
    /// scan inside a `SELECT` that happened to be the first thing to notice
    /// another handle's rebuild is the cost this split exists to avoid.
    fn adopt_live_map(&mut self) -> Result<()> {
        if !self.live_is_stale.get() {
            return Ok(());
        }
        self.load_live_map()
    }

    /// Read a node record, through the cache.
    fn fetch_node(&self, idx: usize) -> Result<NodeRecord> {
        self.cache.borrow_mut().fetch(
            idx,
            self.dim,
            self.encoding,
            self.node_count.get(),
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
        self.stored_version.set(self.pending_version);
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
        put_len(&mut out, self.node_count.get());
        match self.entry.get() {
            Some(entry) => {
                out.push(1);
                put_len(&mut out, entry);
            }
            None => {
                out.push(0);
                put_len(&mut out, 0);
            }
        }
        put_len(&mut out, self.entry_level.get());
        put_len(&mut out, self.tombstones.get());
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
        self.storage.put_meta(&self.header_key(), &out)?;
        // What this instance now believes the file says, so the next
        // [`PagedHnswIndex::adopt_stored_graph`] does not mistake this
        // handle's own write for somebody else's rebuild and throw the cache
        // and the `live` map away after every commit.
        *self.header_seen.borrow_mut() = out;
        Ok(())
    }

    fn reset_graph(&mut self) {
        self.entry.set(None);
        self.entry_level.set(0);
        self.tombstones.set(0);
        self.built_m = self.params.m;
        self.built_ef_construction = self.params.ef_construction;
        self.live.clear();
        self.live_is_stale.set(false);
        self.cache.get_mut().clear();
    }

    /// Rebuild the graph from the committed node records plus the pending batch,
    /// inserting in row-id order for determinism.
    fn build(&mut self) -> Result<()> {
        // The final live set: committed live nodes from storage, minus pending
        // removes, plus pending inserts (whose embeddings are still in memory).
        let mut live: BTreeMap<RowId, StoredVector> = BTreeMap::new();
        let old_count = self.node_count.get();
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
        self.node_count.set(0);

        let shift = level_shift(self.params.m);
        let ceiling = max_level_for(live.len(), self.params.m);
        let mut visited = Visited::new(live.len());
        for (id, vector) in live {
            let level = level_of(id, shift, ceiling);
            self.insert_node(id, vector, level, &mut visited)?;
        }

        // A rebuild can shrink (tombstones dropped), leaving stale records.
        for idx in self.node_count.get()..old_count {
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
        let Some(mut ep) = self.entry.get() else {
            let record = NodeRecord {
                id,
                deleted: false,
                vector,
                neighbors: vec![Vec::new(); level + 1],
            };
            self.store_node(0, record)?;
            self.node_count.set(1);
            self.entry.set(Some(0));
            self.entry_level.set(level);
            self.live.insert(id, 0);
            return Ok(());
        };

        let new_index = self.node_count.get();
        // Add the node with empty adjacency first, so the greedy walk and the
        // reverse edges it will create have a record to read.
        let mut record = NodeRecord {
            id,
            deleted: false,
            vector: vector.clone(),
            neighbors: vec![Vec::new(); level + 1],
        };
        self.store_node(new_index, record.clone())?;
        self.node_count.set(new_index + 1);
        self.live.insert(id, new_index);

        // Descend from the top layer to this node's top layer.
        let mut current = self.entry_level.get();
        while current > level {
            let nearest = self.search_layer(&vector, ep, 1, current, None, visited)?;
            ep = nearest[0].node;
            current -= 1;
        }

        for layer in (0..=current).rev() {
            let candidates = self.search_layer(
                &vector,
                ep,
                self.params.ef_construction,
                layer,
                None,
                visited,
            )?;
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
        if level > self.entry_level.get() {
            self.entry.set(Some(new_index));
            self.entry_level.set(level);
        }
        Ok(())
    }

    /// Best-first search of one layer, fetching node records on demand.
    ///
    /// `query` stays exact even when the index is quantised, for the same
    /// reason [`crate::hnsw::HnswIndex::search`] keeps it exact: quantising
    /// the corpus is the declared storage trade-off, and throwing away query
    /// precision too would add recall loss without saving resident memory.
    ///
    /// With a `filter`, only live rows the filter admits enter `results` and
    /// count toward `ef`; rejected rows are still expanded, so the walk
    /// reaches admissible neighbours on their far side — see
    /// [`crate::hnsw::search_layer`] for why severing that connectivity would
    /// silently drop matches, and for the two stop rules (beam full, frontier
    /// drained). The record fetch a filtered walk needs for the row id is the
    /// same one the distance already needs, so the filter costs predicate
    /// evaluations, not extra reads.
    #[allow(clippy::too_many_arguments)]
    fn search_layer(
        &self,
        query: &StoredVector,
        entry: usize,
        ef: usize,
        layer: usize,
        filter: Option<&RowFilter>,
        visited: &mut Visited,
    ) -> Result<Vec<Candidate>> {
        let ef = ef.max(1);
        visited.restart(self.node_count.get());

        let mut frontier: BinaryHeap<Reverse<Candidate>> = BinaryHeap::new();
        let mut results: BinaryHeap<Candidate> = BinaryHeap::new();
        let admits = |record: &NodeRecord| -> Result<bool> {
            match filter {
                None => Ok(true),
                Some(filter) => Ok(!record.deleted && filter(record.id)?),
            }
        };

        let entry_record = self.fetch_node(entry)?;
        let start = Candidate {
            distance: stored_distance(&self.distance_calls, query, &entry_record.vector),
            node: entry,
        };
        visited.visit(entry);
        frontier.push(Reverse(start));
        if admits(&entry_record)? {
            results.push(start);
        }

        while let Some(Reverse(current)) = frontier.pop() {
            // See [`crate::hnsw::search_layer`]: the beam-full stop compares
            // against the worst *admissible* result, so a filter that admits
            // few rows keeps the walk going past the rejected ones — until
            // the frontier runs out, which is the exact-scan fallback.
            if let Some(worst) = results.peek() {
                if results.len() >= ef && current.distance > worst.distance {
                    break;
                }
            }
            let record = self.fetch_node(current.node)?;
            let neighbors = record.neighbors.get(layer).cloned().unwrap_or_default();
            for neighbor in neighbors {
                if !visited.visit(neighbor) {
                    continue;
                }
                let neighbor_record = self.fetch_node(neighbor)?;
                let candidate = Candidate {
                    distance: stored_distance(&self.distance_calls, query, &neighbor_record.vector),
                    node: neighbor,
                };
                let enters = match results.peek() {
                    None => true,
                    Some(worst) => results.len() < ef || candidate.distance < worst.distance,
                };
                if !enters {
                    continue;
                }
                frontier.push(Reverse(candidate));
                if admits(&neighbor_record)? {
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
        self.tombstones.set(self.tombstones.get() + 1);
        self.live.remove(&id);
        Ok(())
    }

    /// Re-point `entry` at the highest-level live node after the previous entry
    /// was tombstoned.
    fn repick_entry(&mut self) -> Result<()> {
        let mut best: Option<usize> = None;
        for index in 0..self.node_count.get() {
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
                self.entry.set(Some(index));
                self.entry_level
                    .set(self.fetch_node(index)?.neighbors.len().saturating_sub(1));
            }
            None => {
                self.entry.set(None);
                self.entry_level.set(0);
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
        // Before anything is written: another handle may have rebuilt this
        // graph in the file since the last batch, and the node indices it left
        // are the ones to build on.
        self.adopt_stored_graph()?;
        self.adopt_live_map()?;
        let reshaped = self.params.m != self.built_m
            || self.params.ef_construction != self.built_ef_construction;
        let pending = !self.pending_inserts.is_empty() || !self.pending_removes.is_empty();
        if !pending && !reshaped {
            // Nothing changed here, but the database moved on — a write to some
            // other table advances the write version too. Restamping is one
            // metadata write and it is the difference between reopening
            // instantly and rebuilding this graph from the rows for no reason.
            if self.pending_version != self.stored_version.get() {
                return self.finish();
            }
            return Ok(());
        }

        // The first commit has no graph to grow, and a retune has to re-derive
        // the whole graph under the new parameters. Either way: rebuild.
        if self.node_count.get() == 0 || reshaped {
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
        let mut visited = Visited::new(self.node_count.get());
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
        if self.tombstones.get() * 2 >= self.node_count.get() {
            self.build()?;
        } else if let Some(entry) = self.entry.get() {
            if self.fetch_node(entry)?.deleted {
                self.repick_entry()?;
            }
        }
        self.finish()
    }

    fn search(&self, query: &[f32], k: usize, filter: Option<&RowFilter>) -> Result<Vec<Scored>> {
        if query.len() != self.dim {
            return Err(Error::Type(alloc::format!(
                "query has dimension {} but the index expects {}",
                query.len(),
                self.dim
            )));
        }
        // A read has to do this too, not only a write: another handle can
        // rebuild this graph without changing a row, so there is no write
        // version for the engine to notice on this handle's behalf.
        self.adopt_stored_graph()?;
        let Some(mut ep) = self.entry.get() else {
            return Ok(Vec::new());
        };
        if k == 0 {
            return Ok(Vec::new());
        }

        let query = StoredVector::Exact(normalise(query));
        let mut visited = Visited::new(self.node_count.get());
        for layer in (1..=self.entry_level.get()).rev() {
            // Unfiltered descent, as in the in-memory index: it only picks
            // where layer 0 starts, and layer 0 expands through rejected
            // nodes, so filtering here would cost without buying reach.
            let nearest = self.search_layer(&query, ep, 1, layer, None, &mut visited)?;
            ep = nearest[0].node;
        }

        let hits = self.search_layer(&query, ep, self.params.ef_for(k), 0, filter, &mut visited)?;
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
        self.stored_version.get()
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
        let hits = index.search(&[1.0, 0.0, 0.0], 2, None).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn searching_before_commit_returns_nothing() {
        let mut index = index(3);
        index.insert(1, &[1.0, 0.0, 0.0]).unwrap();
        assert!(index.search(&[1.0, 0.0, 0.0], 1, None).unwrap().is_empty());
        index.commit().unwrap();
        assert_eq!(index.search(&[1.0, 0.0, 0.0], 1, None).unwrap().len(), 1);
    }

    #[test]
    fn dimension_mismatch_is_an_error() {
        let mut index = index(3);
        assert!(index.insert(1, &[1.0]).is_err());
        assert!(index.search(&[1.0], 1, None).is_err());
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
                .search(&[1.0, 0.0, 0.0, 0.0], 5, None)
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
            .search(query, 10, None)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        let exact: Vec<RowId> = brute
            .search(query, 10, None)
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

    // -------------------------------------------------------- filtered search

    #[test]
    fn a_filter_that_accepts_everything_returns_the_unfiltered_answer() {
        // The tie to the unfiltered path: same rows, same order, same scores.
        let dim = 8;
        let rows = vectors(128, dim);
        let mut index = index(dim);
        for (id, vector) in rows.iter().enumerate() {
            index.insert(id as u64 + 1, vector).unwrap();
        }
        index.commit().unwrap();
        for seed in 0..8 {
            let query: Vec<f32> = (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect();
            assert_eq!(
                index.search(&query, 10, None).unwrap(),
                index.search(&query, 10, Some(&|_| Ok(true))).unwrap(),
                "filtered path diverged from unfiltered on query {seed}"
            );
        }
    }

    #[test]
    fn a_walk_through_rejected_nodes_reaches_any_admitted_row() {
        // The paged twin of the connectivity test: with a filter admitting
        // exactly one row, the walk must traverse rejected records (fetched
        // through storage, possibly past the cache bound) and still reach it.
        let dim = 8;
        let rows = vectors(200, dim);
        let mut index = index(dim).with_cache_capacity(16);
        for (id, vector) in rows.iter().enumerate() {
            index.insert(id as u64 + 1, vector).unwrap();
        }
        index.commit().unwrap();

        for seed in 0..4 {
            let query: Vec<f32> = (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect();
            for target in (1..=200).step_by(25) {
                let hits = index
                    .search(&query, 10, Some(&|id| Ok(id == target)))
                    .unwrap();
                assert_eq!(
                    hits.iter().map(|h| h.id).collect::<Vec<_>>(),
                    vec![target],
                    "query {seed} did not reach admitted row {target}"
                );
            }
        }
    }

    #[test]
    fn a_filter_that_rejects_everything_returns_nothing_and_terminates() {
        let dim = 8;
        let rows = vectors(128, dim);
        let mut index = index(dim);
        for (id, vector) in rows.iter().enumerate() {
            index.insert(id as u64 + 1, vector).unwrap();
        }
        index.commit().unwrap();
        let query: Vec<f32> = (0..dim).map(|i| (i as f32).sin()).collect();
        let hits = index.search(&query, 10, Some(&|_| Ok(false))).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn filtered_recall_matches_the_brute_force_oracle() {
        // The filtered walk, scored against the exhaustive filtered top-k, at
        // a moderate (10%) and a selective (1%) filter.
        let dim = 8;
        let rows = vectors(400, dim);
        let mut index = index(dim);
        let mut brute = BruteForceVectorIndex::new(dim);
        for (id, vector) in rows.iter().enumerate() {
            index.insert(id as u64 + 1, vector).unwrap();
            brute.insert(id as u64 + 1, vector).unwrap();
        }
        index.commit().unwrap();

        for (label, filter) in [
            (
                "moderate",
                &(move |id: RowId| id.is_multiple_of(10)) as &dyn Fn(RowId) -> bool,
            ),
            (
                "selective",
                &(move |id: RowId| id.is_multiple_of(100)) as &dyn Fn(RowId) -> bool,
            ),
        ] {
            let mut total = 0.0;
            for seed in 0..12 {
                let query: Vec<f32> = (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect();
                let truth: Vec<RowId> = brute
                    .search(&query, 10, Some(&|id| Ok(filter(id))))
                    .unwrap()
                    .into_iter()
                    .map(|h| h.id)
                    .collect();
                if truth.is_empty() {
                    total += 1.0;
                    continue;
                }
                let got = index
                    .search(&query, 10, Some(&|id| Ok(filter(id))))
                    .unwrap();
                let hit = got.iter().filter(|s| truth.contains(&s.id)).count();
                total += hit as f64 / truth.len() as f64;
            }
            let recall = total / 12.0;
            assert!(recall >= 0.95, "{label} filter recall@10 was {recall:.3}");
        }
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
            assert!(!index.search(&query, 5, None).unwrap().is_empty());
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
                    .search(&query(seed), 10, None)
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
                .search(&query(seed), 10, None)
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
            let hits = index.search(&[1.0, 0.0, 0.0], 4, None).unwrap();
            assert!(hits.iter().all(|hit| hit.id != 1));
        }

        // A fresh handle over the same storage still hides the deleted row.
        let restored = PagedHnswIndex::open(storage, "hnsw", 3).unwrap();
        assert!(restored
            .search(&[1.0, 0.0, 0.0], 4, None)
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
            .search(query, 10, None)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();

        let storage = quantized.into_storage();
        let restored = PagedHnswIndex::open_quantized(storage, "hnsw", dim).unwrap();
        let after: Vec<RowId> = restored
            .search(query, 10, None)
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
                .search(&query, 10, None)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect();
            let paged_found: Vec<RowId> = paged
                .search(&query, 10, None)
                .unwrap()
                .into_iter()
                .map(|h| h.id)
                .collect();
            let in_memory_found: Vec<RowId> = in_memory
                .search(&query, 10, None)
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
            .search(&query(0), 10, None)
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
            .search(&query(0), 10, None)
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
        assert!(mismatched.search(&[1.0; 6], 10, None).unwrap().is_empty());

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
        let hits = mismatched.search(&query, 1, None).unwrap();
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
        assert_eq!(reopened.search(&query, 1, None).unwrap()[0].id, 32);
    }

    /// Two handles on one database hold two `PagedHnswIndex` instances over
    /// the *same* namespace, so one of them rebuilding — which is what every
    /// handle does when it opens on a stamp that is not current — reassigns
    /// every node index underneath the other. No row changed, so nothing moved
    /// the write version the engine watches on the second handle's behalf, and
    /// the left-behind handle goes on reading node indices that now name
    /// different rows.
    ///
    /// The twin of `bm25_paged`'s
    /// `a_rebuild_by_another_handle_is_adopted_rather_than_overwritten`, and
    /// the symptom is the one a graph has rather than the one a count has: the
    /// walk starts at a stale entry point, expands adjacency that belongs to
    /// somebody else's graph, and answers with the wrong rows — so this
    /// asserts on returned ids, against an oracle that ran the identical
    /// insert sequence on storage nobody else touched.
    #[test]
    fn a_rebuild_by_another_handle_is_adopted_rather_than_overwritten() {
        let dim = 8;
        // Two disjoint corpora of the same size: the rebuild has to be over
        // *different* embeddings, or determinism would hand both handles the
        // same graph and there would be nothing to get wrong.
        let rows = vectors(61, dim);
        let (mine, theirs) = rows.split_at(30);
        let late = rows[60].clone();

        let storage = crate::shared::SharedStorage::new(alloc::boxed::Box::new(MemStorage::new()));

        // The handle that will be left behind.
        let mut first = PagedHnswIndex::new(storage.clone(), "hnsw", dim);
        for (i, vector) in mine.iter().enumerate() {
            first.insert(i as u64 + 1, vector).unwrap();
        }
        first.commit().unwrap();

        // The handle that decides the saved graph is not current and rebuilds
        // it from the rows, over different embeddings, so every node index in
        // the file is reassigned.
        let mut second = PagedHnswIndex::open(storage.clone(), "hnsw", dim).unwrap();
        second.reset().unwrap();
        for (i, vector) in theirs.iter().enumerate() {
            second.insert(i as u64 + 1, vector).unwrap();
        }
        second.commit().unwrap();

        // The first handle now does what it was going to do anyway. It must
        // build on the graph it finds rather than the one it remembers.
        first.insert(31, &late).unwrap();
        first.commit().unwrap();

        // The same insert sequence, on storage no second handle ever touched.
        let mut oracle = PagedHnswIndex::new(MemStorage::new(), "hnsw", dim);
        for (i, vector) in theirs.iter().enumerate() {
            oracle.insert(i as u64 + 1, vector).unwrap();
        }
        oracle.commit().unwrap();
        oracle.insert(31, &late).unwrap();
        oracle.commit().unwrap();

        let ids = |hits: Vec<Scored>| hits.into_iter().map(|hit| hit.id).collect::<Vec<_>>();
        for seed in 0..8usize {
            let query: Vec<f32> = (0..dim).map(|i| ((seed * dim + i) as f32).sin()).collect();
            let expected = ids(oracle.search(&query, 5, None).unwrap());
            assert_eq!(
                ids(first
                    .search(&query, 5, None)
                    .expect("the left-behind handle could not even walk the graph")),
                expected,
                "the left-behind handle answered query {seed} from a stale view"
            );
            // And the handle that did the rebuild agrees with it, which is
            // what rules out the two having diverged into private views of one
            // shared graph.
            assert_eq!(
                ids(second
                    .search(&query, 5, None)
                    .expect("the rebuilding handle could not even walk the graph")),
                expected,
                "the rebuilding handle answered query {seed} from a stale view"
            );
        }
    }
}
