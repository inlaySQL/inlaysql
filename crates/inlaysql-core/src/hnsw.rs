//! An in-engine approximate nearest neighbour index: a small, deterministic
//! HNSW (hierarchical navigable small world) graph.
//!
//! Stage 4 moves retrieval out of borrowed crates and into the engine. This is
//! the vector half: it replaces the `instant_distance`-backed index in the
//! production crate with one that lives in `inlaysql-core`, so the engine owns
//! its ANN index end to end. It is deterministic — the graph layout is a pure
//! function of the inserted rows — which keeps the whole thing simulator-clean
//! and makes two builds over the same rows agree byte for byte.
//!
//! Embeddings are L2-normalised on the way in, so cosine similarity reduces to
//! a dot product and the graph distance is `1 - dot`.
//!
//! # Incremental maintenance
//!
//! The graph is a cache over [`HnswIndex::embeddings`], which are the source of
//! truth. Before AHL-381 every [`VectorIndex::commit`] discarded the graph and
//! rebuilt it from every embedding — a full `O(n log n)` build to add one row.
//! Now `commit` reconciles the pending writes against the graph:
//!
//! * an **insert** appends one node via the standard greedy insert, connecting
//!   it to `M` neighbours per layer found by the same search queries use;
//! * a **remove** is a tombstone — the node stays in the graph to keep it
//!   connected (unlinking a small-world graph can partition it) but is skipped
//!   by [`VectorIndex::search`];
//! * a full rebuild happens only when a commit finds more tombstones than live
//!   nodes, or when the graph-shaping parameters were retuned.
//!
//! An insert into a graph of `n` nodes costs a number of distance computations
//! bounded by `ef_construction * M` and the layer count, independent of `n`;
//! see [`HnswIndex::distance_calls`]. The layer ceiling is recomputed per
//! commit from the live count, so a node inserted early can land a layer below
//! where a full rebuild of the same rows would have put it — but only when the
//! corpus crossed a power of `M` in between, which moves a rare upper-layer
//! node one sparse layer, and the graph is still a pure function of the insert
//! sequence, so determinism holds.
//!
//! # Why the layer distribution is `1/M` and not `1/2`
//!
//! HNSW's upper layers exist to be *sparse*: the descent through them is a
//! greedy walk with a candidate list of one, so it only lands on the right
//! part of the graph if each layer holds few enough nodes that greedy cannot
//! get stuck. The published parameter is `mL = 1/ln(M)`, which puts a node on
//! layer `l` with probability `M^-l` — at `M = 16`, one node in sixteen
//! reaches layer 1.
//!
//! The first implementation here used the trailing zeros of a hashed row id,
//! which is geometric with ratio `1/2`: *half* the corpus on layer 1. That is
//! fine at a thousand rows and quietly ruinous at a hundred thousand, because
//! the greedy descent then has to cross a 50,000-node layer with `ef = 1` and
//! reliably strands itself in a local minimum. It is why recall used to fall
//! as the corpus grew. Counting trailing zero *nibbles* instead of bits gives
//! exactly `16^-l`, stays a pure function of the row id, and needs no RNG.

use alloc::collections::{BTreeMap, BinaryHeap};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;
use core::cmp::{Ordering, Reverse};

use crate::error::{Error, Result};
use crate::quantize::Q8Vector;
use crate::row::{put_len, Cursor};
use crate::traits::{RowFilter, RowId, Scored, VectorIndex};

/// On-disk format of the persisted index. Bumped whenever the layout changes;
/// a mismatch makes the engine rebuild rather than misread.
///
/// Version 2 keeps version 1's byte layout but not its graph: the layer
/// distribution and the neighbour selection both changed, so a version-1 file
/// would decode into a valid graph with the old, worse recall. Rebuilding is
/// cheap next to shipping a silently degraded index.
///
/// Version 3 adds a per-node `deleted` flag (and an inline vector for deleted
/// nodes, whose embedding no longer exists), so a graph that has been
/// incrementally maintained across deletes round-trips without resurrecting the
/// deleted rows.
///
/// Version 4 stores embeddings and tombstoned-node vectors as one `f32` scale
/// plus `dim` signed bytes. Exact indexes continue to write and read version 3
/// byte-for-byte; only an opted-in int8 column writes version 4.
const FORMAT_VERSION_EXACT: u8 = 3;
const FORMAT_VERSION_Q8: u8 = 4;

/// Absolute ceiling on a node's layer, whatever the corpus size says.
///
/// A guard rail, not a tuning knob: [`max_level_for`] derives the real cap
/// from the number of rows, and at `M = 16` this one is not reachable below
/// `16^12` rows.
const MAX_LEVEL: usize = 12;

/// The knobs that decide what the graph costs and how well it answers.
///
/// Defaults are [`HnswParams::DEFAULT`]; [`HnswIndex::with_params`] exists so
/// the benchmark can sweep them (`bench --suite sweep`) without a rebuild per
/// point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    /// Maximum connections per node, per layer above zero. Layer 0 gets twice
    /// this, which is the usual `M0 = 2M`: it is the only layer every node is
    /// on, so it carries all of the fine-grained structure.
    pub m: usize,
    /// Candidate-list size while building. Buys recall at build time, once,
    /// rather than at query time on every query.
    pub ef_construction: usize,
    /// Floor on the candidate-list size while searching.
    pub ef_search: usize,
    /// The search list also scales with the request: the effective `ef` is
    /// `max(ef_search, k * ef_search_multiplier)`. A constant `ef` is a
    /// different amount of headroom for `k = 1` than for `k = 100`, and the
    /// engine over-fetches candidates for fusion, so `k` arrives already
    /// multiplied.
    pub ef_search_multiplier: usize,
}

impl HnswParams {
    /// The shipped defaults, chosen by the sweep in `bench --suite sweep`.
    pub const DEFAULT: Self = Self {
        m: 16,
        ef_construction: 200,
        ef_search: 64,
        ef_search_multiplier: 2,
    };

    /// Connection budget for one layer: `2 * m` at layer 0, `m` above it.
    pub(crate) fn degree(&self, layer: usize) -> usize {
        if layer == 0 {
            self.m.saturating_mul(2)
        } else {
            self.m
        }
    }

    /// The candidate-list size a `k`-nearest query should search with.
    pub(crate) fn ef_for(&self, k: usize) -> usize {
        self.ef_search
            .max(k.saturating_mul(self.ef_search_multiplier))
            .max(k)
            .max(1)
    }
}

impl Default for HnswParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Which representation a vector column's index stores its embeddings in.
///
/// `pub(crate)` rather than private: [`crate::hnsw_paged::PagedHnswIndex`] is
/// the same algorithm over the same [`StoredVector`] shape, just with the
/// nodes living in a [`crate::traits::Storage`] backend instead of in memory,
/// and needs the same encoding to choose the same wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VectorEncoding {
    /// Full-precision `f32` per component.
    Exact,
    /// Symmetric int8 quantisation — see [`crate::quantize::Q8Vector`].
    Q8,
}

/// A vector in the representation selected by the SQL column.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StoredVector {
    /// Full-precision `f32` per component.
    Exact(Vec<f32>),
    /// Symmetric int8 quantisation — see [`crate::quantize::Q8Vector`].
    Q8(Q8Vector),
}

impl StoredVector {
    pub(crate) fn from_f32(values: &[f32], encoding: VectorEncoding) -> Self {
        match encoding {
            VectorEncoding::Exact => Self::Exact(values.to_vec()),
            VectorEncoding::Q8 => Self::Q8(Q8Vector::from_f32(values)),
        }
    }

    pub(crate) fn to_f32(&self) -> Vec<f32> {
        match self {
            Self::Exact(values) => values.clone(),
            Self::Q8(values) => values.to_f32(),
        }
    }

    pub(crate) fn normalised(&self, encoding: VectorEncoding) -> Self {
        Self::from_f32(&normalise(&self.to_f32()), encoding)
    }

    pub(crate) fn payload_bytes(&self) -> usize {
        match self {
            Self::Exact(values) => values.len() * core::mem::size_of::<f32>(),
            Self::Q8(values) => values.payload_bytes(),
        }
    }
}

/// One node of the graph.
#[derive(Debug, Clone)]
struct Node {
    /// The row this node stands for.
    id: RowId,
    /// L2-normalised embedding.
    vector: StoredVector,
    /// `neighbors[l]` are the node indices connected at layer `l`.
    neighbors: Vec<Vec<usize>>,
    /// Tombstoned nodes stay in the graph for navigation but are skipped by
    /// search. Their embedding is gone (the row was removed), which is why the
    /// on-disk format carries their vector inline.
    deleted: bool,
}

impl Node {
    fn new(id: RowId, vector: StoredVector, level: usize) -> Self {
        Self {
            id,
            vector,
            neighbors: vec![Vec::new(); level + 1],
            deleted: false,
        }
    }
}

/// A hierarchical navigable small world index.
pub struct HnswIndex {
    dim: usize,
    encoding: VectorEncoding,
    /// Source of truth: the embeddings, keyed by row id. Updated by insert and
    /// remove; the graph is reconciled to these on [`HnswIndex::commit`].
    embeddings: BTreeMap<RowId, StoredVector>,
    /// The committed graph, empty until the first commit. Tombstoned nodes stay
    /// here until a rebuild drops them.
    nodes: Vec<Node>,
    /// Row id -> node index, for the *live* node standing for each id. A
    /// tombstoned node is unmapped here, so an insert of an id that was removed
    /// (or replaced) creates a fresh node rather than reviving a stale one.
    node_ids: BTreeMap<RowId, usize>,
    /// Node index of the entry point, `None` when the graph is empty.
    entry: Option<usize>,
    /// The entry point's layer.
    entry_level: usize,
    /// Number of tombstoned nodes still in the graph. A commit that finds these
    /// outnumber the live nodes rebuilds.
    tombstones: usize,
    /// Row ids inserted since the last commit, in arrival order.
    pending_inserts: Vec<RowId>,
    /// Row ids removed since the last commit, in arrival order.
    pending_removes: Vec<RowId>,
    /// Distance computations since the last reset. See
    /// [`HnswIndex::distance_calls`].
    distance_calls: Cell<u64>,
    /// The `m` the committed graph was built under. A retune forces a rebuild
    /// rather than silently mixing degrees across the graph.
    built_m: usize,
    /// The `ef_construction` the committed graph was built under.
    built_ef_construction: usize,
    /// Tuning. Not persisted: the graph on disk is whatever built it, and the
    /// next [`HnswIndex::commit`] rebuilds under the parameters in force then.
    params: HnswParams,
}

impl HnswIndex {
    /// An empty index over vectors of the given dimension, with
    /// [`HnswParams::DEFAULT`].
    pub fn new(dim: usize) -> Self {
        Self::with_params(dim, HnswParams::DEFAULT)
    }

    /// An empty int8-quantised index over vectors of the given dimension.
    pub fn new_quantized(dim: usize) -> Self {
        Self::with_encoding(dim, HnswParams::DEFAULT, VectorEncoding::Q8)
    }

    /// An empty index with explicit tuning.
    pub fn with_params(dim: usize, params: HnswParams) -> Self {
        Self::with_encoding(dim, params, VectorEncoding::Exact)
    }

    fn with_encoding(dim: usize, params: HnswParams, encoding: VectorEncoding) -> Self {
        Self {
            dim,
            encoding,
            embeddings: BTreeMap::new(),
            nodes: Vec::new(),
            node_ids: BTreeMap::new(),
            entry: None,
            entry_level: 0,
            tombstones: 0,
            pending_inserts: Vec::new(),
            pending_removes: Vec::new(),
            distance_calls: Cell::new(0),
            built_m: params.m,
            built_ef_construction: params.ef_construction,
            params,
        }
    }

    /// Bytes occupied by vector payloads in the source map and committed
    /// graph. Container and adjacency overhead are intentionally excluded so
    /// exact and int8 columns are directly comparable.
    pub fn resident_vector_bytes(&self) -> usize {
        self.embeddings
            .values()
            .map(StoredVector::payload_bytes)
            .chain(self.nodes.iter().map(|node| node.vector.payload_bytes()))
            .sum()
    }

    /// The tuning in force.
    pub fn params(&self) -> HnswParams {
        self.params
    }

    /// Distance computations made since the last
    /// [`HnswIndex::reset_distance_calls`].
    ///
    /// This is the number the incremental-insert guarantee is expressed in: a
    /// single insert into an `n`-node graph costs a number of distance
    /// computations bounded by `ef_construction * M` times the layer count,
    /// independent of `n`, where a full rebuild touches every node. It exists
    /// so tests and benchmarks can assert that property by counting rather than
    /// by timing, which would not survive a noisy machine.
    pub fn distance_calls(&self) -> u64 {
        self.distance_calls.get()
    }

    /// Reset the distance counter. Call before measuring one operation in
    /// isolation.
    pub fn reset_distance_calls(&self) {
        self.distance_calls.set(0);
    }

    /// Retune. `m` and `ef_construction` shape the graph, so they only take
    /// effect on the next [`HnswIndex::commit`]; `ef_search` applies to the
    /// very next query.
    pub fn set_params(&mut self, params: HnswParams) {
        self.params = params;
    }

    /// Number of indexed embeddings.
    pub fn len(&self) -> usize {
        self.embeddings.len()
    }

    /// Whether the index holds no embeddings.
    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }

    /// Serialise the index, graph included.
    ///
    /// ```text
    /// index := u8 version, u32 dim,
    ///          u32 embedding_count, embedding*,
    ///          u32 node_count, node*,
    ///          u8 has_entry, u32 entry index, u32 entry level
    /// embedding := u64 row id, f32 * dim
    /// node      := u8 deleted, u64 row id, u32 layer_count, layer*, (f32*dim)?
    /// layer     := u32 neighbour_count, u32 * neighbour_count   (node indices)
    /// ```
    ///
    /// The graph is stored rather than recomputed because building it is the
    /// expensive part — every insert walks the graph. A live node's vector is
    /// *not* stored: it is the normalised form of the embedding, so
    /// [`HnswIndex::decode`] recomputes it and the file stays half the size. A
    /// tombstoned node has no embedding left to recompute from, so its vector
    /// is stored inline and it can still be traversed after a reload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(match self.encoding {
            VectorEncoding::Exact => FORMAT_VERSION_EXACT,
            VectorEncoding::Q8 => FORMAT_VERSION_Q8,
        });
        put_len(&mut out, self.dim);

        put_len(&mut out, self.embeddings.len());
        for (id, embedding) in &self.embeddings {
            out.extend_from_slice(&id.to_le_bytes());
            encode_stored_vector(&mut out, embedding);
        }

        put_len(&mut out, self.nodes.len());
        for node in &self.nodes {
            out.push(node.deleted as u8);
            out.extend_from_slice(&node.id.to_le_bytes());
            put_len(&mut out, node.neighbors.len());
            for layer in &node.neighbors {
                put_len(&mut out, layer.len());
                for neighbor in layer {
                    put_len(&mut out, *neighbor);
                }
            }
            if node.deleted {
                encode_stored_vector(&mut out, &node.vector);
            }
        }

        match self.entry {
            Some(entry) => {
                out.push(1);
                put_len(&mut out, entry);
                put_len(&mut out, self.entry_level);
            }
            None => {
                out.push(0);
                put_len(&mut out, 0);
                put_len(&mut out, 0);
            }
        }
        out
    }

    /// Parse bytes produced by [`HnswIndex::encode`].
    ///
    /// Everything the graph asserts about itself is checked here — every live
    /// node has an embedding, every neighbour index is in range, the entry
    /// point exists and is live, and no two live nodes claim the same row. A
    /// graph that fails any of those would search into nowhere, and the
    /// engine's answer would be silently wrong rather than loudly missing.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let version = cursor.u8()?;
        let encoding = match version {
            FORMAT_VERSION_EXACT => VectorEncoding::Exact,
            FORMAT_VERSION_Q8 => VectorEncoding::Q8,
            _ => {
                return Err(Error::Corrupt(alloc::format!(
                    "vector index format version {version} is not supported"
                )))
            }
        };
        let dim = cursor.count(4)?;
        let mut index = Self::with_encoding(dim, HnswParams::DEFAULT, encoding);

        let embedding_count = cursor.count(8)?;
        for _ in 0..embedding_count {
            let id = RowId::from_le_bytes(cursor.array8()?);
            let embedding = decode_stored_vector(&mut cursor, dim, encoding)?;
            index.embeddings.insert(id, embedding);
        }

        // One byte of `deleted`, eight of row id, four of layer count.
        let node_count = cursor.count(13)?;
        let mut nodes = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            let deleted = cursor.u8()? != 0;
            let id = RowId::from_le_bytes(cursor.array8()?);
            let layer_count = cursor.count(4)?;
            let mut neighbors = Vec::with_capacity(layer_count);
            for _ in 0..layer_count {
                let neighbour_count = cursor.count(4)?;
                let mut layer = Vec::with_capacity(neighbour_count);
                for _ in 0..neighbour_count {
                    layer.push(cursor.u32()? as usize);
                }
                neighbors.push(layer);
            }
            // A live node's vector is the normalised embedding and must be
            // recomputable; a tombstoned node has no embedding left, so its
            // vector was stored inline and is read back here.
            let vector = if deleted {
                decode_stored_vector(&mut cursor, dim, encoding)?
            } else {
                index
                    .embeddings
                    .get(&id)
                    .map(|embedding| embedding.normalised(encoding))
                    .ok_or_else(|| {
                        Error::Corrupt(alloc::format!("graph node {id} has no stored embedding"))
                    })?
            };
            nodes.push(Node {
                id,
                vector,
                neighbors,
                deleted,
            });
        }

        for (position, node) in nodes.iter().enumerate() {
            for layer in &node.neighbors {
                for neighbor in layer {
                    if *neighbor >= node_count {
                        return Err(Error::Corrupt(alloc::format!(
                            "graph node {position} links to out-of-range node {neighbor}"
                        )));
                    }
                }
            }
        }

        let has_entry = cursor.u8()? != 0;
        let entry = cursor.u32()? as usize;
        let entry_level = cursor.u32()? as usize;
        if has_entry && entry >= node_count {
            return Err(Error::Corrupt(alloc::format!(
                "graph entry point {entry} is out of range ({node_count} nodes)"
            )));
        }
        if has_entry && nodes[entry].deleted {
            return Err(Error::Corrupt(alloc::format!(
                "graph entry point {entry} is a deleted node"
            )));
        }

        let mut node_ids = BTreeMap::new();
        let mut tombstones = 0usize;
        for (position, node) in nodes.iter().enumerate() {
            if node.deleted {
                tombstones += 1;
            } else if node_ids.insert(node.id, position).is_some() {
                return Err(Error::Corrupt(alloc::format!(
                    "graph has two live nodes for row {}",
                    node.id
                )));
            }
        }

        index.nodes = nodes;
        index.node_ids = node_ids;
        index.tombstones = tombstones;
        index.entry = has_entry.then_some(entry);
        index.entry_level = entry_level;
        Ok(index)
    }

    /// Rebuild the graph from the current embeddings. Deterministic: rows are
    /// visited in row-id order, and each node's layer is a pure function of its
    /// row id and the corpus size.
    fn build(&mut self) -> Result<()> {
        self.nodes.clear();
        self.node_ids.clear();
        self.entry = None;
        self.entry_level = 0;
        self.tombstones = 0;
        self.built_m = self.params.m;
        self.built_ef_construction = self.params.ef_construction;

        let count = self.embeddings.len();
        let ceiling = max_level_for(count, self.params.m);
        let shift = level_shift(self.params.m);
        let pending: Vec<(RowId, StoredVector, usize)> = self
            .embeddings
            .iter()
            .map(|(id, embedding)| {
                (
                    *id,
                    embedding.normalised(self.encoding),
                    level_of(*id, shift, ceiling),
                )
            })
            .collect();

        // One scratch set for the whole build. Allocating a fresh one per
        // inserted node would dominate: there are `count` inserts and each
        // touches a handful of layers.
        let mut visited = Visited::new(count);
        self.nodes.reserve(count);
        for (id, vector, level) in pending {
            self.insert_node(id, vector, level, &mut visited)?;
        }
        Ok(())
    }

    /// Greedily insert one node into the graph.
    fn insert_node(
        &mut self,
        id: RowId,
        vector: StoredVector,
        level: usize,
        visited: &mut Visited,
    ) -> Result<()> {
        // The first node is the entry point at its own level.
        let Some(mut ep) = self.entry else {
            self.nodes.push(Node::new(id, vector, level));
            self.entry = Some(0);
            self.entry_level = level;
            self.node_ids.insert(id, 0);
            return Ok(());
        };

        // Descend from the graph's top layer to this node's top layer. When the
        // new node's level exceeds the current entry point's, `current` stops at
        // the entry level: there are no nodes above it to connect to.
        let mut current = self.entry_level;
        while current > level {
            let nearest = search_layer(
                &self.nodes,
                &vector,
                ep,
                1,
                current,
                None,
                visited,
                &self.distance_calls,
            )?;
            ep = nearest[0].node;
            current -= 1;
        }

        // Connect the node at every existing layer from `current` down to 0.
        let new_index = self.nodes.len();
        self.nodes.push(Node::new(id, vector.clone(), level));
        self.node_ids.insert(id, new_index);
        for layer in (0..=current).rev() {
            let candidates = search_layer(
                &self.nodes,
                &vector,
                ep,
                self.params.ef_construction,
                layer,
                None,
                visited,
                &self.distance_calls,
            )?;
            // The nearest node at this layer seeds the next layer's search.
            ep = candidates[0].node;

            let degree = self.params.degree(layer);
            let selected = select_neighbors(&self.nodes, &candidates, degree, &self.distance_calls);
            self.nodes[new_index].neighbors[layer] = selected.clone();
            for neighbor in selected {
                self.link_back(neighbor, new_index, layer, degree);
            }
        }

        if level > self.entry_level {
            self.entry = Some(new_index);
            self.entry_level = level;
        }
        Ok(())
    }

    /// Add the reverse edge `neighbor -> new_index`, pruning `neighbor`'s list
    /// back to `degree` if that pushed it over.
    ///
    /// Without the prune, a node that happens to sit in a dense region keeps
    /// accumulating reverse edges — one build here produced hubs with hundreds
    /// of links, which costs a distance computation each on every search that
    /// touches them. Pruning with the same diversity heuristic that chose the
    /// forward edges, rather than by plain truncation, is what keeps the graph
    /// connected while it is trimmed.
    fn link_back(&mut self, neighbor: usize, new_index: usize, layer: usize, degree: usize) {
        self.nodes[neighbor].neighbors[layer].push(new_index);
        if self.nodes[neighbor].neighbors[layer].len() <= degree {
            return;
        }
        let mut candidates: Vec<Candidate> = self.nodes[neighbor].neighbors[layer]
            .iter()
            .map(|&other| Candidate {
                distance: stored_distance(
                    &self.distance_calls,
                    &self.nodes[neighbor].vector,
                    &self.nodes[other].vector,
                ),
                node: other,
            })
            .collect();
        candidates.sort_unstable();
        self.nodes[neighbor].neighbors[layer] =
            select_neighbors(&self.nodes, &candidates, degree, &self.distance_calls);
    }

    /// Mark a node deleted, leaving it in the graph for navigation but out of
    /// search results and out of `node_ids`.
    fn tombstone(&mut self, index: usize) {
        if self.nodes[index].deleted {
            return;
        }
        self.nodes[index].deleted = true;
        self.tombstones += 1;
        let id = self.nodes[index].id;
        self.node_ids.remove(&id);
    }

    /// Re-point `entry` at a live node after the previous entry was tombstoned.
    ///
    /// Picks the highest-level live node (ties broken by the lowest index, so
    /// the choice is deterministic), preserving as much of the descent as a
    /// dead entry point could offer. It is O(n), but it runs only when the
    /// entry point itself is deleted and the tombstone count has not yet
    /// crossed the rebuild threshold.
    fn repick_entry(&mut self) {
        let mut best: Option<usize> = None;
        for (index, node) in self.nodes.iter().enumerate() {
            if node.deleted {
                continue;
            }
            best = Some(match best {
                None => index,
                Some(current) => {
                    let current_level = self.nodes[current].neighbors.len().saturating_sub(1);
                    if node.neighbors.len().saturating_sub(1) > current_level {
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
                self.entry_level = self.nodes[index].neighbors.len().saturating_sub(1);
            }
            None => {
                self.entry = None;
                self.entry_level = 0;
            }
        }
    }
}

/// Best-first search of one layer: up to `ef` nodes nearest to `query` at
/// `layer`, nearest first.
///
/// With a `filter`, only rows the filter admits (and, among them, only live
/// nodes) may enter `results` or count toward `ef` — but every node visited
/// is still expanded and may still enter the frontier, so a rejected node
/// routes the walk to admissible neighbours on its far side. Cutting rejected
/// nodes out of the graph instead would sever that connectivity and silently
/// drop matches, which is precisely the failure this design exists to prevent.
///
/// The walk ends one of two ways:
///
/// * the beam fills (`ef` admissible results) and the nearest unexpanded
///   frontier node is farther than the worst of them — the ordinary HNSW
///   stop, which is exactly what a permissive filter costs (the unfiltered
///   walk, byte for byte);
/// * the frontier drains — with a filter that admits few rows this means
///   every node in the graph has been seen and the admissible set returned is
///   complete for that filter, not a partial probe. One pass, where the
///   pre-pushdown engine re-walked the graph once per doubling round of its
///   over-fetch loop.
///
///
/// A free function rather than a method so that the caller can hold the scratch
/// [`Visited`] mutably while `nodes` is borrowed shared — the graph build needs
/// both at once.
#[allow(clippy::too_many_arguments)]
fn search_layer(
    nodes: &[Node],
    query: &StoredVector,
    entry: usize,
    ef: usize,
    layer: usize,
    filter: Option<&RowFilter>,
    visited: &mut Visited,
    distance_calls: &Cell<u64>,
) -> Result<Vec<Candidate>> {
    let ef = ef.max(1);
    visited.restart(nodes.len());

    // `frontier` pops nearest-first (what to expand next); `results` pops
    // farthest-first (what to evict when it overflows `ef`). Two heaps rather
    // than two sorted vectors: the sorted-vector version re-sorted the whole
    // frontier on every expansion, which is what made a 5,000-row build take
    // three quarters of a minute.
    //
    // With a filter, `results` holds only admissible nodes — a rejected node
    // rides the frontier but is never an answer — and the admission check
    // below runs once per node visited, never per expansion.
    let mut frontier: BinaryHeap<Reverse<Candidate>> = BinaryHeap::new();
    let mut results: BinaryHeap<Candidate> = BinaryHeap::new();
    let admits = |node: usize| -> Result<bool> {
        match filter {
            // Unfiltered keeps every node, tombstones included: they are
            // dropped after the walk, exactly as before the filter existed.
            None => Ok(true),
            Some(filter) => Ok(!nodes[node].deleted && filter(nodes[node].id)?),
        }
    };

    let start = Candidate {
        distance: stored_distance(distance_calls, query, &nodes[entry].vector),
        node: entry,
    };
    visited.visit(entry);
    frontier.push(Reverse(start));
    if admits(entry)? {
        results.push(start);
    }

    while let Some(Reverse(current)) = frontier.pop() {
        // Everything still in the frontier is at least this far away, so once
        // the nearest of them is worse than the worst result we hold, no
        // expansion can improve the answer. The comparison is against the
        // worst *admissible* result, so a filter that admits few nodes keeps
        // the walk going past the rejected ones instead of stopping with an
        // under-filled (or empty) answer — until the frontier itself runs
        // out, which is the exact-scan fallback.
        if let Some(worst) = results.peek() {
            if results.len() >= ef && current.distance > worst.distance {
                break;
            }
        }
        for &neighbor in neighbors_at(nodes, current.node, layer) {
            if !visited.visit(neighbor) {
                continue;
            }
            let candidate = Candidate {
                distance: stored_distance(distance_calls, query, &nodes[neighbor].vector),
                node: neighbor,
            };
            // Enter the frontier when the results are not yet full or the
            // candidate is closer than the worst admissible result — rejected
            // nodes ride the frontier under the same rule, because they still
            // route. Once the admissible results are full, farther nodes are
            // dropped as before: they cannot be answers, and keeping them
            // would turn a selective filter into an unbounded walk.
            let enters = match results.peek() {
                None => true,
                Some(worst) => results.len() < ef || candidate.distance < worst.distance,
            };
            if !enters {
                continue;
            }
            frontier.push(Reverse(candidate));
            if admits(neighbor)? {
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

/// A node's links at `layer`, or nothing if it does not reach that layer.
///
/// Decoding validates that every neighbour index is in range but not that a
/// node is tall enough for the layer it is reached from, so a corrupt file
/// would otherwise index past the end of `neighbors` mid-query.
fn neighbors_at(nodes: &[Node], node: usize, layer: usize) -> &[usize] {
    nodes[node]
        .neighbors
        .get(layer)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

impl VectorIndex for HnswIndex {
    fn insert(&mut self, id: RowId, embedding: &[f32]) -> Result<()> {
        if embedding.len() != self.dim {
            return Err(Error::Type(alloc::format!(
                "embedding has dimension {} but the index expects {}",
                embedding.len(),
                self.dim
            )));
        }
        self.embeddings
            .insert(id, StoredVector::from_f32(embedding, self.encoding));
        self.pending_inserts.push(id);
        Ok(())
    }

    fn remove(&mut self, id: RowId) -> Result<()> {
        self.embeddings.remove(&id);
        self.pending_removes.push(id);
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let reshaped = self.params.m != self.built_m
            || self.params.ef_construction != self.built_ef_construction;
        let pending = !self.pending_inserts.is_empty() || !self.pending_removes.is_empty();
        if !pending && !reshaped {
            return Ok(());
        }

        // The first commit has no graph to grow, and a retune has to re-derive
        // the whole graph under the new parameters. Either way: rebuild.
        if self.nodes.is_empty() || reshaped {
            self.build()?;
            self.pending_inserts.clear();
            self.pending_removes.clear();
            return Ok(());
        }

        // Removals first, so an id removed and reinserted in the same window —
        // an update — leaves one tombstone behind and then a fresh node.
        let removes = core::mem::take(&mut self.pending_removes);
        for id in removes {
            if let Some(&index) = self.node_ids.get(&id) {
                self.tombstone(index);
            }
        }

        // Inserts next, in arrival order: the graph is a pure function of the
        // row sequence even when one id arrives more than once.
        let inserts = core::mem::take(&mut self.pending_inserts);
        let shift = level_shift(self.params.m);
        let ceiling = max_level_for(self.embeddings.len(), self.params.m);
        let mut visited = Visited::new(self.nodes.len());
        for id in inserts {
            let Some(embedding) = self.embeddings.get(&id) else {
                // Removed again before this commit ran: nothing to insert.
                continue;
            };
            let vector = embedding.normalised(self.encoding);
            let level = level_of(id, shift, ceiling);
            // A replace without an intervening remove retires the old node the
            // same way a remove would.
            if let Some(&old) = self.node_ids.get(&id) {
                self.tombstone(old);
            }
            self.insert_node(id, vector, level, &mut visited)?;
        }

        // Repair. More tombstones than live nodes means over half the graph is
        // dead and search spends most of its budget walking past it: rebuild.
        if self.tombstones * 2 >= self.nodes.len() {
            self.build()?;
        } else if let Some(entry) = self.entry {
            if self.nodes[entry].deleted {
                self.repick_entry();
            }
        }
        Ok(())
    }

    fn save(&self) -> Option<Vec<u8>> {
        Some(self.encode())
    }

    fn load(&mut self, bytes: &[u8]) -> Result<()> {
        let restored = Self::decode(bytes)?;
        if restored.dim != self.dim {
            return Err(Error::Corrupt(alloc::format!(
                "persisted vector index has dimension {} but the column declares {}",
                restored.dim,
                self.dim
            )));
        }
        if restored.encoding != self.encoding {
            return Err(Error::Corrupt(alloc::string::String::from(
                "persisted vector index encoding does not match its column",
            )));
        }
        // Tuning is not persisted, so the caller's stays in force. `params` is
        // the live configuration; the file only carries the graph. The loaded
        // graph is assumed to have been built under those parameters, so the
        // next commit maintains it incrementally instead of rebuilding — a
        // retune via [`HnswIndex::set_params`] still forces a rebuild.
        let params = self.params;
        *self = restored;
        self.params = params;
        self.built_m = params.m;
        self.built_ef_construction = params.ef_construction;
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize, filter: Option<&RowFilter>) -> Result<Vec<Scored>> {
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

        // Keep the query in f32. Quantising the corpus is the declared storage
        // trade-off; throwing away query precision too would add recall loss
        // without saving resident memory.
        let query = StoredVector::Exact(normalise(query));
        let mut visited = Visited::new(self.nodes.len());
        for layer in (1..=self.entry_level).rev() {
            // The descent is unfiltered: it only picks where layer 0 starts,
            // and a rejected node is a fine place to start from, because the
            // layer-0 walk expands through rejected nodes. Filtering here
            // would cost predicate evaluations on upper-layer nodes without
            // changing which rows the walk can reach.
            let nearest = search_layer(
                &self.nodes,
                &query,
                ep,
                1,
                layer,
                None,
                &mut visited,
                &self.distance_calls,
            )?;
            ep = nearest[0].node;
        }

        let hits = search_layer(
            &self.nodes,
            &query,
            ep,
            self.params.ef_for(k),
            0,
            filter,
            &mut visited,
            &self.distance_calls,
        )?;
        // Tombstoned nodes still route the search but are not answers. They
        // can be safely dropped here because `ef_for` over-fetches: the rebuild
        // threshold keeps tombstones below half the graph and `ef >= 2k`, so
        // there are always at least `k` live candidates among the hits.
        // Filter-rejected nodes never made it into `hits` at all — the walk
        // held them in the frontier only.
        Ok(hits
            .into_iter()
            .filter(|hit| !self.nodes[hit.node].deleted)
            .map(|hit| Scored::new(self.nodes[hit.node].id, 1.0 - hit.distance))
            .take(k)
            .collect())
    }

    fn resident_vector_bytes(&self) -> Option<usize> {
        Some(HnswIndex::resident_vector_bytes(self))
    }
}

/// A `(distance, node)` pair, ordered by distance and then by node index so
/// that the ordering is total and two builds over the same rows break ties the
/// same way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Candidate {
    pub(crate) distance: f32,
    pub(crate) node: usize,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // `total_cmp`, not `partial_cmp`: a NaN distance out of a corrupt file
        // must still order, or the heaps below would misbehave silently.
        self.distance
            .total_cmp(&other.distance)
            .then(self.node.cmp(&other.node))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A reusable "have I seen this node" set.
///
/// One `u32` stamp per node and a generation counter, so clearing between
/// searches is an increment rather than a pass over the array. The graph build
/// runs one of these per node per layer; a `BTreeSet` per call was a
/// measurable share of build time, and a freshly zeroed `Vec<bool>` per call
/// would be worse.
pub(crate) struct Visited {
    stamp: Vec<u32>,
    generation: u32,
}

impl Visited {
    pub(crate) fn new(len: usize) -> Self {
        Self {
            stamp: vec![0; len],
            generation: 0,
        }
    }

    /// Begin a fresh search over `len` nodes, forgetting everything visited.
    pub(crate) fn restart(&mut self, len: usize) {
        if self.stamp.len() < len {
            self.stamp.resize(len, 0);
        }
        match self.generation.checked_add(1) {
            Some(next) => self.generation = next,
            // Wrapping would make stale stamps look current. Once every four
            // billion searches, pay for a real clear.
            None => {
                self.stamp.fill(0);
                self.generation = 1;
            }
        }
    }

    /// Mark `node` seen, returning whether this call was the first to do so.
    pub(crate) fn visit(&mut self, node: usize) -> bool {
        let stamp = &mut self.stamp[node];
        if *stamp == self.generation {
            return false;
        }
        *stamp = self.generation;
        true
    }
}

/// The layer a node belongs to, as a pure function of its row id.
///
/// `shift` trailing zero bits of a mixed hash stand for one layer, so a node
/// reaches layer `l` with probability `2^-(shift*l)` — at `shift = 4`, one in
/// sixteen per layer, which is HNSW's `mL = 1/ln(M)` for `M = 16`. See the
/// module note on why the ratio matters.
pub(crate) fn level_of(id: RowId, shift: u32, ceiling: usize) -> usize {
    ((mix64(id).trailing_zeros() / shift) as usize).min(ceiling)
}

/// How many trailing zero bits stand for one layer, given `m`.
pub(crate) fn level_shift(m: usize) -> u32 {
    m.max(2).ilog2()
}

/// The highest layer worth using for `count` rows.
///
/// One layer per factor of `m`, so the top layer still holds a handful of
/// nodes rather than one. Deriving this from the corpus rather than fixing it
/// keeps small indexes flat — a 100-row graph gains nothing from six layers of
/// descent — and lets large ones grow.
pub(crate) fn max_level_for(count: usize, m: usize) -> usize {
    let m = m.max(2);
    let mut level = 0;
    let mut reach = m;
    while reach < count && level < MAX_LEVEL {
        reach = reach.saturating_mul(m);
        level += 1;
    }
    level
}

/// Append a vector's payload in its own encoding's wire format. Shared with
/// [`crate::hnsw_paged`], whose node records carry the same payload shape.
pub(crate) fn encode_stored_vector(out: &mut Vec<u8>, vector: &StoredVector) {
    match vector {
        StoredVector::Exact(values) => {
            for value in values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        StoredVector::Q8(values) => {
            out.extend_from_slice(&values.scale.to_le_bytes());
            out.extend(values.values.iter().map(|value| *value as u8));
        }
    }
}

/// Parse a vector payload produced by [`encode_stored_vector`]. Shared with
/// [`crate::hnsw_paged`].
pub(crate) fn decode_stored_vector(
    cursor: &mut Cursor<'_>,
    dim: usize,
    encoding: VectorEncoding,
) -> Result<StoredVector> {
    match encoding {
        VectorEncoding::Exact => {
            let mut values = Vec::with_capacity(dim);
            for _ in 0..dim {
                values.push(f32::from_le_bytes(cursor.array4()?));
            }
            Ok(StoredVector::Exact(values))
        }
        VectorEncoding::Q8 => {
            let scale = f32::from_le_bytes(cursor.array4()?);
            let values = cursor.take(dim)?.iter().map(|value| *value as i8).collect();
            Ok(StoredVector::Q8(Q8Vector { scale, values }))
        }
    }
}

/// SplitMix64 finaliser, to decorrelate consecutive row ids.
fn mix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

/// L2-normalise an embedding, leaving zero vectors untouched.
pub(crate) fn normalise(embedding: &[f32]) -> Vec<f32> {
    let norm = libm::sqrtf(embedding.iter().map(|x| x * x).sum::<f32>());
    if norm == 0.0 {
        return embedding.to_vec();
    }
    embedding.iter().map(|x| x / norm).collect()
}

/// How many accumulators the dot product carries. See [`distance`].
const LANES: usize = 8;

/// `1 - cosine` on already-normalised vectors, where the cosine is just a dot
/// product.
///
/// This is the whole cost of the index — a 100,000-row build calls it in the
/// billions — so it is written to be turned into SIMD, twice over:
///
/// * The general [`crate::mem::cosine_similarity`] cannot assume normalised
///   inputs, so it recomputes both norms: three passes and two square roots
///   where one pass will do.
/// * Summing into one accumulator forbids vectorisation outright. Float
///   addition is not associative, so a compiler may not reorder
///   `dot += x * y` into lanes — it has to emit the scalar loop it was
///   given. Eight explicit accumulators state the reassociation in the source
///   instead, which is both vectorisable and still a fixed summation order:
///   the same inputs give the same bits on every run, which the simulation
///   tests depend on.
pub(crate) fn distance(counter: &Cell<u64>, a: &[f32], b: &[f32]) -> f32 {
    counter.set(counter.get().saturating_add(1));
    let mut lanes = [0.0f32; LANES];
    let (left_chunks, left_rem) = a.as_chunks::<LANES>();
    let (right_chunks, right_rem) = b.as_chunks::<LANES>();
    for (x, y) in left_chunks.iter().zip(right_chunks) {
        for lane in 0..LANES {
            lanes[lane] += x[lane] * y[lane];
        }
    }

    let mut dot = 0.0f32;
    for lane in lanes {
        dot += lane;
    }
    // Whatever a dimension that is not a multiple of `LANES` leaves over.
    for (x, y) in left_rem.iter().zip(right_rem) {
        dot += x * y;
    }
    1.0 - dot
}

/// Distance between two vectors, dispatching on which side (if either) is
/// quantised. Shared with [`crate::hnsw_paged`].
pub(crate) fn stored_distance(counter: &Cell<u64>, a: &StoredVector, b: &StoredVector) -> f32 {
    match (a, b) {
        (StoredVector::Exact(left), StoredVector::Exact(right)) => distance(counter, left, right),
        (StoredVector::Q8(left), StoredVector::Exact(right)) => {
            counter.set(counter.get().saturating_add(1));
            1.0 - left.dot_f32(right)
        }
        (StoredVector::Exact(left), StoredVector::Q8(right)) => {
            counter.set(counter.get().saturating_add(1));
            1.0 - right.dot_f32(left)
        }
        (StoredVector::Q8(left), StoredVector::Q8(right)) => {
            counter.set(counter.get().saturating_add(1));
            1.0 - left.dot_q8(right)
        }
    }
}

/// Choose up to `degree` links for a node out of `candidates`, nearest first.
///
/// This is HNSW's neighbour heuristic rather than plain truncation: a
/// candidate is kept only when the new node is closer to it than any
/// already-kept neighbour is. Truncating to the `m` nearest fills a node's
/// links with a single tight cluster, and a graph of tight clusters has no
/// long edges to travel along — the greedy search walks into one and stops.
/// The heuristic deliberately keeps a few further-out candidates instead,
/// which is what makes the graph navigable.
///
/// If the heuristic is too strict to fill the budget, the nearest rejected
/// candidates make up the difference: an under-connected node is a worse
/// failure than a redundant edge.
fn select_neighbors(
    nodes: &[Node],
    candidates: &[Candidate],
    degree: usize,
    distance_calls: &Cell<u64>,
) -> Vec<usize> {
    let mut selected: Vec<usize> = Vec::with_capacity(degree);
    for candidate in candidates {
        if selected.len() >= degree {
            break;
        }
        let diverse = selected.iter().all(|&kept| {
            stored_distance(
                distance_calls,
                &nodes[candidate.node].vector,
                &nodes[kept].vector,
            ) > candidate.distance
        });
        if diverse {
            selected.push(candidate.node);
        }
    }
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
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn index(dim: usize) -> HnswIndex {
        HnswIndex::new(dim)
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
    fn similarity_is_reported_not_distance() {
        let mut index = index(3);
        index.insert(1, &[1.0, 0.0, 0.0]).unwrap();
        index.commit().unwrap();
        let hits = index.search(&[1.0, 0.0, 0.0], 1, None).unwrap();
        assert!(
            (hits[0].score - 1.0).abs() < 1e-5,
            "score was {}",
            hits[0].score
        );
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
    fn removal_drops_the_embedding() {
        let mut index = index(3);
        index.insert(1, &[1.0, 0.0, 0.0]).unwrap();
        index.insert(2, &[0.0, 1.0, 0.0]).unwrap();
        index.commit().unwrap();
        index.remove(1).unwrap();
        index.commit().unwrap();
        let hits = index.search(&[1.0, 0.0, 0.0], 4, None).unwrap();
        assert!(hits.iter().all(|hit| hit.id != 1));
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
    fn approximate_search_matches_brute_force_on_small_data() {
        // Random (but deterministic) vectors, no ties, so the top-k is
        // unambiguous and recall is a meaningful check.
        let mut index = index(8);
        let mut brute = crate::mem::BruteForceVectorIndex::new(8);
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((seed >> 33) as i16) as f32 / 16384.0
        };
        for i in 0..64u64 {
            let mut v = vec![0.0f32; 8];
            for c in v.iter_mut() {
                *c = next();
            }
            index.insert(i + 1, &v).unwrap();
            brute.insert(i + 1, &v).unwrap();
        }
        index.commit().unwrap();
        brute.commit().unwrap();

        let query = {
            let mut v = vec![0.0f32; 8];
            for c in v.iter_mut() {
                *c = next();
            }
            v
        };
        let approx: Vec<RowId> = index
            .search(&query, 10, None)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        let exact: Vec<RowId> = brute
            .search(&query, 10, None)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        // The true nearest neighbour must be found, and most of the top-10
        // must agree (approximation is allowed, but not much).
        assert_eq!(approx[0], exact[0]);
        let overlap = approx.iter().filter(|id| exact.contains(id)).count();
        assert!(
            overlap >= 8,
            "recall too low: approx {approx:?} exact {exact:?}"
        );
    }

    /// A graph over `count` deterministic pseudo-random vectors.
    fn built(count: RowId, dim: usize) -> HnswIndex {
        let mut index = HnswIndex::new(dim);
        let mut state = 0x51ed_2701_u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as f32 / u32::MAX as f32) - 0.5
        };
        for id in 1..=count {
            let vector: Vec<f32> = (0..dim).map(|_| next()).collect();
            index.insert(id, &vector).unwrap();
        }
        index.commit().unwrap();
        index
    }

    #[test]
    fn a_restored_graph_returns_the_same_neighbours() {
        let original = built(64, 8);
        let mut restored = HnswIndex::new(8);
        restored.load(&original.save().unwrap()).unwrap();

        for seed in 0..8 {
            let query: Vec<f32> = (0..8).map(|i| ((seed * 8 + i) as f32).sin()).collect();
            assert_eq!(
                original.search(&query, 10, None).unwrap(),
                restored.search(&query, 10, None).unwrap(),
                "restored graph diverged on query {seed}"
            );
        }
    }

    #[test]
    fn quantized_graph_round_trips_and_shrinks_vector_memory() {
        let exact = built(256, 384);
        let mut quantized = HnswIndex::new_quantized(384);
        for (id, embedding) in &exact.embeddings {
            quantized.insert(*id, &embedding.to_f32()).unwrap();
        }
        quantized.commit().unwrap();

        let exact_bytes = exact.resident_vector_bytes();
        let q8_bytes = quantized.resident_vector_bytes();
        assert!(
            exact_bytes * 100 >= q8_bytes * 390,
            "exact={exact_bytes} q8={q8_bytes}"
        );

        let saved = quantized.save().unwrap();
        assert_eq!(saved[0], FORMAT_VERSION_Q8);
        let mut restored = HnswIndex::new_quantized(384);
        restored.load(&saved).unwrap();
        let query = exact.embeddings[&1].to_f32();
        assert_eq!(
            quantized.search(&query, 10, None).unwrap(),
            restored.search(&query, 10, None).unwrap()
        );
    }

    #[test]
    fn an_empty_index_round_trips() {
        let mut restored = HnswIndex::new(4);
        restored.load(&HnswIndex::new(4).save().unwrap()).unwrap();
        assert!(restored.is_empty());
        assert!(restored
            .search(&[1.0, 0.0, 0.0, 0.0], 5, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_graph_with_a_dangling_neighbour_is_refused() {
        // A neighbour index past the end of the node list would send a search
        // off the end of the graph. It has to be caught at decode, not by a
        // panic during the first query.
        let mut index = built(8, 4);
        index.nodes[0].neighbors[0] = vec![usize::from(u8::MAX)];
        assert!(matches!(
            HnswIndex::decode(&index.encode()),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn a_node_without_an_embedding_is_refused() {
        let mut index = built(8, 4);
        let orphan = index.nodes[0].id;
        index.embeddings.remove(&orphan);
        assert!(matches!(
            HnswIndex::decode(&index.encode()),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn a_dimension_mismatch_is_refused_on_load() {
        let bytes = built(4, 8).save().unwrap();
        assert!(HnswIndex::new(4).load(&bytes).is_err());
    }

    #[test]
    fn a_truncated_encoding_is_rejected_not_panicked() {
        let bytes = built(8, 4).encode();
        for cut in [0, 1, 5, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                HnswIndex::decode(&bytes[..cut]).is_err(),
                "a {cut}-byte prefix decoded as a whole index"
            );
        }
    }

    #[test]
    fn a_future_format_version_is_refused() {
        let mut bytes = built(4, 4).encode();
        bytes[0] = FORMAT_VERSION_Q8 + 1;
        assert!(matches!(HnswIndex::decode(&bytes), Err(Error::Corrupt(_))));
    }

    // ------------------------------------------------------------- tuning

    #[test]
    fn one_node_in_m_reaches_the_next_layer() {
        // The parameter that decides whether the upper layers can be navigated
        // greedily. At ratio 1/2 — what this used to be — layer 1 holds half
        // the corpus and the descent gets stuck; see the module note.
        let shift = level_shift(16);
        let count = 100_000u64;
        let above = (1..=count)
            .filter(|id| level_of(*id, shift, MAX_LEVEL) >= 1)
            .count();
        let ratio = above as f64 / count as f64;
        assert!(
            (ratio - 1.0 / 16.0).abs() < 0.005,
            "one node in {:.1} reached layer 1, wanted one in 16",
            1.0 / ratio
        );
    }

    #[test]
    fn the_top_layer_follows_the_corpus_size() {
        // Small graphs stay flat; each factor of M earns one more layer.
        assert_eq!(max_level_for(0, 16), 0);
        assert_eq!(max_level_for(16, 16), 0);
        assert_eq!(max_level_for(5_000, 16), 3);
        assert_eq!(max_level_for(100_000, 16), 4);
        // ...and it cannot run away, whatever the corpus claims.
        assert!(max_level_for(usize::MAX, 16) <= MAX_LEVEL);
    }

    #[test]
    fn the_search_list_widens_with_k() {
        let params = HnswParams::DEFAULT;
        // Below the floor, the floor wins; above it, k does.
        assert_eq!(params.ef_for(1), params.ef_search);
        assert_eq!(params.ef_for(10), 20.max(params.ef_search));
        assert_eq!(params.ef_for(100), 200);
        // And `ef` is never narrower than the answer being asked for.
        assert!(params.ef_for(1_000) >= 1_000);
    }

    #[test]
    fn no_node_keeps_more_links_than_its_budget() {
        // Reverse edges are added on every insert that picks a node as a
        // neighbour. Unpruned they accumulate without bound, and every one of
        // them is a distance computation on every search that walks through.
        let index = built(1_000, 12);
        let params = HnswParams::DEFAULT;
        for (position, node) in index.nodes.iter().enumerate() {
            for (layer, links) in node.neighbors.iter().enumerate() {
                assert!(
                    links.len() <= params.degree(layer),
                    "node {position} has {} links at layer {layer}, budget is {}",
                    links.len(),
                    params.degree(layer)
                );
            }
        }
    }

    /// Mean recall@k of the graph against exhaustive search, over `queries`
    /// deterministic queries.
    fn measured_recall(index: &HnswIndex, dim: usize, k: usize, queries: usize) -> f64 {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as f32 / u32::MAX as f32) - 0.5
        };
        // The oracle normalises once, not once per query: this runs on every
        // push and the brute-force pass is most of it.
        let oracle: Vec<(RowId, Vec<f32>)> = index
            .embeddings
            .iter()
            .map(|(id, embedding)| (*id, normalise(&embedding.to_f32())))
            .collect();

        // The oracle's own distance computations are not part of what the index
        // measured; they are scored through a throwaway counter.
        let counter = Cell::new(0);
        let mut total = 0.0;
        for _ in 0..queries {
            let query: Vec<f32> = (0..dim).map(|_| next()).collect();
            let normalised = normalise(&query);

            let mut exact: Vec<(f32, RowId)> = oracle
                .iter()
                .map(|(id, embedding)| (distance(&counter, &normalised, embedding), *id))
                .collect();
            exact.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            let truth: Vec<RowId> = exact.into_iter().take(k).map(|(_, id)| id).collect();

            let found = index.search(&query, k, None).unwrap();
            let hit = found.iter().filter(|s| truth.contains(&s.id)).count();
            total += hit as f64 / k as f64;
        }
        total / queries as f64
    }

    #[test]
    fn recall_does_not_fall_as_the_corpus_grows() {
        recall_holds_across([400, 1_600]);
    }

    // -------------------------------------------------------- filtered search

    /// Mean recall@k of a *filtered* search against the brute-force oracle
    /// restricted to the same filter, over `queries` deterministic queries.
    ///
    /// The oracle is exhaustive filtered-then-ranked — the answer the old
    /// engine-side over-fetch loop degraded to when the filter was selective
    /// enough — so this measures exactly what the filter pushdown is not
    /// allowed to lose.
    fn measured_filtered_recall(
        index: &HnswIndex,
        dim: usize,
        k: usize,
        queries: usize,
        filter: &dyn Fn(RowId) -> bool,
    ) -> f64 {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as f32 / u32::MAX as f32) - 0.5
        };
        let oracle: Vec<(RowId, Vec<f32>)> = index
            .embeddings
            .iter()
            .filter(|(id, _)| filter(**id))
            .map(|(id, embedding)| (*id, normalise(&embedding.to_f32())))
            .collect();

        let counter = Cell::new(0);
        let mut total = 0.0;
        for _ in 0..queries {
            let query: Vec<f32> = (0..dim).map(|_| next()).collect();
            let normalised = normalise(&query);

            let mut exact: Vec<(f32, RowId)> = oracle
                .iter()
                .map(|(id, embedding)| (distance(&counter, &normalised, embedding), *id))
                .collect();
            exact.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            let truth: Vec<RowId> = exact.into_iter().take(k).map(|(_, id)| id).collect();
            if truth.is_empty() {
                total += 1.0;
                continue;
            }

            let found = index.search(&query, k, Some(&|id| Ok(filter(id)))).unwrap();
            let hit = found.iter().filter(|s| truth.contains(&s.id)).count();
            total += hit as f64 / truth.len() as f64;
        }
        total / queries as f64
    }

    #[test]
    fn a_filter_that_accepts_everything_returns_the_unfiltered_answer() {
        // The tie to the slow path: a filter that admits every row must make
        // the filtered search agree with the unfiltered one exactly — same
        // rows, same order, same scores. One beam, one walk.
        let index = built(256, 12);
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as f32 / u32::MAX as f32) - 0.5
        };
        for _ in 0..16 {
            let query: Vec<f32> = (0..12).map(|_| next()).collect();
            assert_eq!(
                index.search(&query, 10, None).unwrap(),
                index.search(&query, 10, Some(&|_| Ok(true))).unwrap(),
            );
        }
    }

    #[test]
    fn a_filter_that_rejects_everything_returns_nothing_and_terminates() {
        // The pathological case the old over-fetch loop's doc comment names:
        // a filter nothing satisfies must end with an empty answer, not hang.
        let index = built(256, 8);
        let query = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        let hits = index.search(&query, 10, Some(&|_| Ok(false))).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn a_walk_through_rejected_nodes_reaches_any_admitted_row() {
        // Connectivity is the whole point of traversing *through* rejected
        // nodes: for a filter admitting exactly one row, the search must find
        // that row wherever it sits in the graph. With the result budget
        // (ef >= 64) never filled, the walk drains the frontier — and a
        // drained walk on a connected graph has seen every node, so a miss
        // here is a severed graph, the silent recall regression this design
        // exists to prevent.
        let index = built(200, 8);
        let mut state = 0x5eed_5eed_5eed_5eedu64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as f32 / u32::MAX as f32) - 0.5
        };
        for round in 0..8 {
            let query: Vec<f32> = (0..8).map(|_| next()).collect();
            for target in (1..=200).step_by(25) {
                let hits = index
                    .search(&query, 10, Some(&|id| Ok(id == target)))
                    .unwrap();
                assert_eq!(
                    hits.len(),
                    1,
                    "round {round}: filter admitting only {target} returned {hits:?}"
                );
                assert_eq!(hits[0].id, target);
            }
        }
    }

    #[test]
    fn a_failing_filter_propagates_the_error() {
        let index = built(16, 4);
        let query = vec![0.1, 0.2, 0.3, 0.4];
        let result = index.search(
            &query,
            5,
            Some(&|_| Err(Error::Type(alloc::string::String::from("boom")))),
        );
        assert!(matches!(result, Err(Error::Type(message)) if message == "boom"));
    }

    #[test]
    fn filtered_recall_holds_across_selectivities() {
        // Recall@10 against the exhaustive filtered oracle, at a permissive
        // (100%), a moderate (10%) and a selective (1%) filter. The filtered
        // walk holds the same beam over admitted rows the unfiltered search
        // holds over all rows, so recall must not fall with selectivity —
        // the selective case used to cost the engine its whole over-fetch
        // loop and still saw a narrower beam than this.
        let index = built(1_600, 12);
        let k = 10;
        let baseline = measured_recall(&index, 12, k, 12);

        let permissive = measured_filtered_recall(&index, 12, k, 12, &|_| true);
        let moderate = measured_filtered_recall(&index, 12, k, 12, &|id| id % 10 == 0);
        let selective = measured_filtered_recall(&index, 12, k, 12, &|id| id % 100 == 0);

        assert!(
            (permissive - baseline).abs() < 0.001,
            "permissive filtered recall {permissive:.3} drifted from unfiltered {baseline:.3}"
        );
        for (label, recall) in [("moderate", moderate), ("selective", selective)] {
            assert!(
                recall >= baseline - 0.05,
                "{label} filter recall {recall:.3} fell below the unfiltered index's own {baseline:.3}"
            );
        }
    }

    #[test]
    fn filtered_search_is_deterministic() {
        let index = built(256, 8);
        let query = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        let run = || {
            index
                .search(&query, 10, Some(&|id| Ok(id % 3 == 0)))
                .unwrap()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn a_filter_selective_enough_to_drain_does_it_in_one_pass() {
        // The old over-fetch loop paid one full walk per doubling round once
        // the filter outlasted the candidate budget — several times the graph
        // size for a filter nothing admits. The pushdown walks the graph
        // once and stops. Pinned as a count, not a time (the project's
        // convention for a number that has to survive a noisy machine).
        let count = 4_000u64;
        let index = built(count, 12);
        index.reset_distance_calls();
        let query: Vec<f32> = (0..12).map(|i| ((i as f32) + 0.5).sin()).collect();
        // A filter admitting ~1% of the corpus: the candidate beam (ef = 80
        // for k = 40) can never fill, so the walk must drain the graph and
        // answer exactly — in one pass.
        let hits = index
            .search(&query, 40, Some(&|id| Ok(id % 100 == 0)))
            .unwrap();
        assert_eq!(hits.len(), 40, "1% of the corpus admits 40 rows");
        assert!(hits.iter().all(|hit| hit.id % 100 == 0));
        let calls = index.distance_calls();
        assert!(
            calls < count + count / 10,
            "a 1%-filtered search cost {calls} distance computations; one pass \
             over a {count}-node graph costs ~{count}, and the old over-fetch \
             loop paid one such walk per doubling round"
        );
    }

    /// A filtered search over an index with tombstones routes through them
    /// but never answers with one, and still reaches live rows behind them.
    #[test]
    fn filtered_search_skips_tombstoned_nodes_but_still_finds_live_rows() {
        let mut index = built(64, 8);
        for id in 1..=32 {
            index.remove(id).unwrap();
        }
        index.commit().unwrap();
        let query = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        let hits = index.search(&query, 10, Some(&|_| Ok(true))).unwrap();
        assert!(!hits.is_empty(), "live rows were unreachable");
        assert!(hits.iter().all(|hit| hit.id > 32));
    }

    /// The same property over a 64x range rather than a 4x one.
    ///
    /// Split out because the cost is in the build and the property is not: an
    /// unoptimised 25,600-node graph is twenty seconds, which is most of this
    /// crate's test run for one more octave of confidence. Run it explicitly,
    /// and in the nightly job — see `TESTING.md`.
    ///
    /// ```sh
    /// cargo test --release -p inlaysql-core -- --ignored recall_holds
    /// ```
    #[test]
    #[ignore = "expensive: builds a 25,600-node graph"]
    fn recall_holds_over_a_wide_range_of_corpus_sizes() {
        recall_holds_across([400, 1_600, 6_400, 25_600]);
    }

    /// Assert that recall@10 clears 0.95 at every size and never slides as the
    /// corpus grows.
    ///
    /// This is the shape of the bug AHL-372 exists to fix: recall@10 went 0.90
    /// at 5,000 vectors to 0.73 at 20,000 — it *fell* as the corpus grew, which
    /// is backwards, because the layer distribution put half the corpus one
    /// layer up and the greedy descent could not cross it.
    ///
    /// The published acceptance is dim 384 at 100,000 vectors, measured by
    /// `bench --suite vectors`. No unit test is going to build that; this holds
    /// the same property small enough to run on a push, so a change that
    /// reintroduces the slope is caught in CI rather than in the nightly
    /// benchmark. It is a guard rail, not the measurement.
    fn recall_holds_across<const N: usize>(sizes: [RowId; N]) {
        // Starts at zero so the first size only has the absolute bound to
        // clear; the second assertion is between successive sizes.
        let mut previous = 0.0;
        for count in sizes {
            let recall = measured_recall(&built(count, 12), 12, 10, 15);
            assert!(
                recall >= 0.95,
                "recall@10 was {recall:.3} at {count} vectors"
            );
            assert!(
                recall >= previous - 0.03,
                "recall@10 fell from {previous:.3} to {recall:.3} by {count} vectors"
            );
            previous = recall;
        }
    }

    // ----------------------------------------------------- incremental insert

    /// `count` deterministic pseudo-random vectors of `dim` components.
    fn vectors(count: RowId, dim: usize) -> Vec<Vec<f32>> {
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
    fn an_incremental_insert_does_not_touch_every_node() {
        // Small corpus and small search parameters keep this in the fast suite.
        // The property — insert cost is bounded by the parameters, not by the
        // corpus — is pinned at the production parameters in the ignored test
        // below.
        let params = HnswParams {
            m: 8,
            ef_construction: 24,
            ef_search: 32,
            ef_search_multiplier: 1,
        };
        let count = 4_000u64;
        let mut index = HnswIndex::with_params(4, params);
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

    /// The same property at production parameters and scale: one insert into a
    /// 100,000-node graph costs a fraction of the graph, counted not timed.
    ///
    /// A full rebuild would touch every node (and, per node, walk the graph
    /// again — on the order of `count * ef_construction` distance
    /// computations). One incremental insert stays under `count / 4`.
    #[test]
    #[ignore = "expensive: builds a 100,000-node graph"]
    fn an_incremental_insert_into_a_large_graph_touches_a_fraction() {
        let count = 100_000u64;
        let mut index = HnswIndex::new(4);
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
            calls < count / 4,
            "inserting one row into a {count}-node graph cost {calls} distance \
             computations; a full rebuild would touch every node"
        );
    }

    #[test]
    fn incremental_inserts_match_a_full_rebuilds_recall() {
        // The same rows built two ways — one commit at the end, or a commit
        // every 20 rows — must answer with recall within tolerance of each
        // other against the exhaustive oracle.
        let dim = 12;
        let count = 2_000u64;
        let rows = vectors(count, dim);

        let full = {
            let mut index = HnswIndex::new(dim);
            for (id, vector) in rows.iter().enumerate() {
                index.insert(id as u64 + 1, vector).unwrap();
            }
            index.commit().unwrap();
            index
        };
        let incremental = {
            let mut index = HnswIndex::new(dim);
            for (id, vector) in rows.iter().enumerate() {
                index.insert(id as u64 + 1, vector).unwrap();
                if (id + 1) % 20 == 0 {
                    index.commit().unwrap();
                }
            }
            index.commit().unwrap();
            index
        };

        let full_recall = measured_recall(&full, dim, 10, 20);
        let inc_recall = measured_recall(&incremental, dim, 10, 20);
        assert!(
            (full_recall - inc_recall).abs() <= 0.02,
            "incremental recall {inc_recall:.3} diverged from a full rebuild's {full_recall:.3}"
        );
    }

    #[test]
    fn incremental_builds_over_the_same_rows_agree() {
        // Same rows, same insert order, same commit boundaries -> same answers.
        let build = || {
            let mut index = index(4);
            for i in 0..20u64 {
                let angle = (i as f32) * 0.1;
                index
                    .insert(i + 1, &[angle.cos(), angle.sin(), 0.0, 0.0])
                    .unwrap();
                if (i + 1) % 5 == 0 {
                    index.commit().unwrap();
                }
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

    // ------------------------------------------------------------ deletion

    #[test]
    fn a_graph_with_tombstones_round_trips_and_hides_deleted_rows() {
        let mut original = built(64, 8);
        for id in 1..=10 {
            original.remove(id).unwrap();
        }
        original.commit().unwrap();

        // 10 of 64 nodes tombstoned is well below the rebuild threshold, so the
        // dead nodes are still there for navigation, and the deleted rows are
        // gone from the answer.
        assert_eq!(
            original.nodes.iter().filter(|node| node.deleted).count(),
            10
        );
        assert!(original
            .search(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 100, None)
            .unwrap()
            .iter()
            .all(|hit| hit.id > 10));

        let mut restored = HnswIndex::new(8);
        restored.load(&original.save().unwrap()).unwrap();
        for seed in 0..8 {
            let query: Vec<f32> = (0..8).map(|i| ((seed * 8 + i) as f32).sin()).collect();
            assert_eq!(
                original.search(&query, 10, None).unwrap(),
                restored.search(&query, 10, None).unwrap(),
                "restored graph diverged on query {seed}"
            );
        }
        // The deleted rows stay deleted after the round trip.
        assert!(restored
            .search(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 100, None)
            .unwrap()
            .iter()
            .all(|hit| hit.id > 10));
    }

    #[test]
    fn a_full_rebuild_repairs_after_enough_deletions() {
        let mut index = built(1_000, 8);
        for id in 1..=500 {
            index.remove(id).unwrap();
        }
        index.commit().unwrap();

        // Half the graph was dead: the commit rebuilt, dropping every tombstone.
        assert_eq!(index.tombstones, 0);
        assert!(index.nodes.iter().all(|node| !node.deleted));

        // And the surviving rows still answer.
        let hits = index
            .search(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 10, None)
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|hit| hit.id > 500));
    }

    #[test]
    fn retuning_the_graph_parameters_rebuilds() {
        // `built` uses the defaults, so layer 0 has a degree of 2 * 16. Halving
        // `m` must not leave the existing nodes at the old degree: the next
        // commit rebuilds under the retuned parameter.
        let mut index = built(512, 8);
        let mut params = index.params();
        params.m = 8;
        index.set_params(params);
        index
            .insert(999, &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .unwrap();
        index.commit().unwrap();

        for node in &index.nodes {
            assert!(
                node.neighbors[0].len() <= 16,
                "layer 0 degree {} did not shrink with the retune",
                node.neighbors[0].len()
            );
        }
    }
}
