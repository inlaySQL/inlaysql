//! A BM25 index whose postings do not have to fit in RAM.
//!
//! [`crate::bm25::Bm25Index`] holds the term dictionary, every postings list
//! and a per-document term list in memory. Measured
//! (`crates/inlaysql/tests/index_memory_cost.rs`) that is ~1,800 bytes per
//! document once the dictionary saturates, so ten million documents is ~17 GiB
//! **per connection** — the number `docs/enterprise-readiness.md` blocker 6
//! names as the remaining ceiling on "vector + BM25 + SQL in one file at
//! scale", and the one this module exists to remove. It is the same Okapi
//! BM25, the same MaxScore walk and the same answers; what changes is where
//! the postings live.
//!
//! It is the full-text twin of [`crate::hnsw_paged::PagedHnswIndex`] and is
//! shaped like it deliberately: the structure is ordinary rows in the engine's
//! own copy-on-write tree, written into whatever transaction the caller has
//! open, restored by re-opening rather than by replaying, and stamped with the
//! write version it describes so that a crash mid-build is visible rather than
//! plausible. Sharing between connections then comes for free and without an
//! invalidation protocol, for the reason blocker 6 records: the mechanism this
//! codebase has for sharing immutable data between handles is not an `Arc`, it
//! is the file — `FileDevice` keeps one raw-page cache per file, sound because
//! a committed page id names bytes that never change again.
//!
//! # The hard requirement: identical scores, not similar ones
//!
//! BM25 is corpus-relative. `idf` is a function of the live document count and
//! a term's document frequency; the length normalisation divides by the mean
//! document length. A backend that computed any of those slightly differently
//! would not fail — it would return a plausible ranking with two hits
//! transposed, which is the silent-wrong-answer failure `docs/indexes.md` is
//! written against. So three things are deliberate:
//!
//! * **The arithmetic is not transcribed twice.** [`crate::bm25::idf`],
//!   [`crate::bm25::average_length`], [`crate::bm25::length_normalisation`]
//!   and [`crate::bm25::contribution`] are called by both backends. Floating
//!   point is not associative, so a second copy that grouped a multiplication
//!   differently would agree to a printed decimal and disagree as bits.
//! * **The statistics are maintained to be exactly equal**, not approximately:
//!   `live` and `total_length` move on exactly the events they move on in the
//!   in-memory index, and a term's `document_frequency` is the count of its
//!   postings, maintained by the same insert/replace/remove rules.
//! * **A document's contributions are summed in query order**, as they are
//!   there, because that is what every published BM25 number from this engine
//!   already means.
//!
//! Skipping is the one place the two backends are allowed to differ, and it
//! cannot change the answer: MaxScore only ever declines to visit a document
//! whose *entire* possible score is strictly below the `k`-th best already
//! held, so any valid upper bound prunes a different amount of work and the
//! same set of results. `bm25_paged_agreement.rs` asserts the whole result set
//! — scores, not rankings — against a freshly built in-memory index rather
//! than trusting that argument.
//!
//! # The layout
//!
//! Row keys in [`Storage`] are `(namespace, u64)`, so every structure below is
//! a `u64`-keyed table under a namespace no SQL identifier can spell (the
//! leading `\u{1}`, the same trick
//! [`crate::hnsw_paged::PagedHnswIndex`]'s `\u{1}ann:` namespace uses). Four
//! of them, derived from one base:
//!
//! ```text
//! <base>          documents,  key = row id
//!     doc     := u32 length, u32 term_count, u32 * term_count   (term ordinals)
//! <base>\u{1}d  dictionary, key = FNV-1a 64 of the term
//!     bucket  := u32 count, (string term, u32 ordinal)*
//! <base>\u{1}x  term records, key = term ordinal
//!     term    := string term, u32 document_frequency, u32 max_frequency,
//!                u32 min_length, u32 next_slot, u32 chunk_count, chunk*
//!     chunk   := u32 slot, u64 greatest row id in that chunk
//! <base>\u{1}p  postings chunks, key = (term ordinal << 32) | slot
//!     chunk   := u32 count, posting*
//!     posting := u64 row id, u32 frequency, u32 document length
//! ```
//!
//! Four decisions in there are load-bearing.
//!
//! **Documents are identified by row id, not by a dense ordinal.** The
//! in-memory index assigns ordinals so that a length or a row id is an array
//! index; on disk there are no arrays, only keyed lookups, so an ordinal would
//! buy nothing and would cost a `RowId -> ordinal` map that is resident and
//! grows with the corpus — which is the very thing being removed. Walk order
//! changes from ordinal order to row-id order as a result, and that cannot
//! change the answer: the answer is the top `k` under a total order on
//! `(score, row id)`, so it is a function of the *set* of documents scored,
//! never of the order they were reached in.
//!
//! **A posting carries its document's length.** Otherwise scoring a document
//! costs a second keyed read for its length, once per document the walk
//! reaches — the dominant cost of a query, and the one that would make a paged
//! backend hopeless rather than merely slower. It costs four bytes per posting
//! on disk and nothing in memory, and it cannot go stale: re-indexing a
//! document rewrites every posting it has.
//!
//! **A term's chunks are found through a directory, not by scanning.** The
//! term record lists `(slot, greatest row id)` per chunk in ascending order,
//! which is a skip list: a MaxScore cursor that has been demoted and is
//! thousands of postings behind advances over whole chunks without reading
//! any of them. It is also what makes a mid-list write cheap — a re-indexed
//! document rewrites the one chunk that holds its row id, not the list.
//!
//! **A chunk is keyed by a slot, and the slot is reused on rewrite.** Keying a
//! chunk by its greatest row id would make the directory redundant, but it
//! would also mean every append renames a key; keying it positionally would
//! renumber the tail on every split. A slot does neither.
//!
//! # What stays resident
//!
//! * The header scalars: live documents, total length in terms, the next term
//!   ordinal, the stamps. `O(1)`.
//! * `pending` — the documents accepted since the last [`FullTextIndex::commit`].
//!   Bounded by the caller's commit interval, exactly as
//!   [`crate::hnsw_paged::PagedHnswIndex`]'s `pending_inserts` is, and with the
//!   same caveat: a *full rebuild* driven by the engine hands over every row
//!   before committing once, and that phase is `O(n)` here as it is there.
//!   [`PagedBm25Index::with_pending_limit`] bounds it for a caller that owns
//!   its own transaction.
//! * The cache: at most `cache_capacity` decoded entries — dictionary buckets,
//!   term records and postings chunks — evicted least-recently-used first.
//!
//! Not resident: the term dictionary, the postings, the per-document term
//! lists. Those are the three that grow with the corpus.
//!
//! The one entry that is not `O(1)` in bytes is a very common term's record,
//! because it carries that term's chunk directory: `O(document frequency /
//! POSTINGS_PER_CHUNK)`, so a term in every one of ten million documents
//! carries ~40,000 directory entries, half a megabyte, read on demand and held
//! as *one* of the `cache_capacity` entries. The bound is stated in entries
//! rather than bytes for the same reason
//! [`crate::hnsw_paged::PagedHnswIndex`]'s is, and this is where that
//! approximation is loosest.
//!
//! # What it costs, and why this is opt-in
//!
//! Writes. An inverted index update touches one chunk per *distinct term* of
//! the document, and a 120-token chunk of English has around a hundred of
//! them, each landing on a different leaf page; the first time a term is seen
//! it costs a dictionary bucket and a term record as well. Under copy-on-write
//! that is a few hundred page copies for one document, and a commit record has
//! to hold every page it copied (`docs/enterprise-readiness.md` blocker 5).
//! Two things blunt that and neither removes it: a batch is applied
//! **term-major**, so a term mentioned by fifty of the pending documents is
//! rewritten once rather than fifty times, and outside a caller's transaction
//! the batch commits itself as [`Storage::transaction_is_nearly_full`] says
//! so. What is left is a read *inside* an open transaction after many
//! documents, where committing is forbidden and the whole batch has to fit one
//! record — refused rather than half-applied, but refused.
//!
//! And the file. Every superseded page is abandoned rather than reclaimed
//! unless `page_reuse` is on (blocker 4), so a bulk load through this backend
//! grows the file by tens of kilobytes per document.
//!
//! That is the trade, and it is why this defaults to off. The in-memory index
//! is faster and this one fits. Nothing about the choice is inferred — see
//! [`crate::EngineOptions::paged_text_indexes`].
//!
//! # Two handles, one structure
//!
//! Sharing through the file is what makes this cheap, and it is also the one
//! thing about it that is genuinely harder than the in-memory backend. Two
//! `Database` handles on one database hold two of these over the *same*
//! namespaces. When one of them rebuilds — which is what any handle does on
//! opening to a stamp that is not current — it rewrites the document records
//! and reassigns every term ordinal underneath the other, **without changing a
//! row**, so nothing moves the write version the engine watches on the other
//! handle's behalf.
//!
//! [`PagedBm25Index::adopt_stored_statistics`] is the answer: the corpus
//! statistics and the term-ordinal counter are re-read from the header on
//! every commit and every search rather than remembered, and the decoded cache
//! is dropped whenever they moved. That method's comment has the two failure
//! modes it prevents, one loud and one silent.
//!
//! [`crate::hnsw_paged::PagedHnswIndex`] has the same exposure — a rebuild
//! reassigns node indices the same way — and does not do this. It is recorded
//! in `docs/indexes.md` rather than fixed here, because changing that
//! backend's protocol is not this module's business.
//!
//! # Read-your-writes, and who owns the transaction
//!
//! Identical to [`crate::hnsw_paged`]: applying a batch reads back chunks it
//! wrote earlier in the same batch, which requires a backend whose reads see
//! its own buffered writes ([`crate::btree::CowBTree::get`] does). And
//! [`PagedBm25Index::joined_to_caller_transaction`] is what the engine uses so
//! that the rows and the index that describes them land in one commit or
//! neither does.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::cmp::Ordering;

use crate::bm25::{
    average_length, contribution, idf, length_normalisation, tokenize, Impact, TopK,
};
use crate::error::{Error, Result};
use crate::row::{put_len, put_string, Cursor};
use crate::traits::{FullTextIndex, RowFilter, RowId, RowScan, Scored, Storage};

/// On-disk layout of the paged index. A header that does not carry this is not
/// read: the structure is purged and the caller's usual rebuild-from-rows
/// handling takes over, which is the same answer every other "this index
/// cannot prove it describes these rows" case gets.
const FORMAT_VERSION: u32 = 1;

/// How many decoded entries the cache holds by default.
///
/// An entry is a postings chunk (~4 KiB at [`POSTINGS_PER_CHUNK`]), a term
/// record or a dictionary bucket (tens of bytes each), so this is single-digit
/// megabytes of working set for a corpus of any size.
pub const DEFAULT_CACHE_ENTRIES: usize = 2048;

/// Postings per chunk.
///
/// The trade is read amplification against write amplification and directory
/// size: a bigger chunk means fewer, coarser skips and a whole chunk rewritten
/// to change one posting; a smaller one means a longer directory and more tree
/// descents per walk. 256 postings is ~4 KiB, which is one default page.
const POSTINGS_PER_CHUNK: usize = 256;

/// How many documents may be buffered before a caller that owns its
/// transaction applies them. See [`PagedBm25Index::with_pending_limit`].
const DEFAULT_PENDING_DOCUMENTS: usize = 4096;

/// One document's entry in a term's postings list.
///
/// The document's length rides along so that scoring never needs a second
/// keyed read — see the [module note](self#the-layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Posting {
    id: RowId,
    frequency: u32,
    length: u32,
}

/// Where one chunk of a term's postings lives, and the greatest row id in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkRef {
    slot: u32,
    max_id: RowId,
}

/// Everything about one term except its postings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TermRecord {
    /// The term itself. Held here so that retiring a term whose last posting
    /// went away can find its dictionary bucket, which is keyed by the term's
    /// hash and cannot be reached from the ordinal alone.
    term: String,
    /// How many documents mention it. Exactly the number of postings, because
    /// `idf` is computed from it and a drifting count reranks silently.
    document_frequency: u32,
    /// The bound MaxScore prunes on. Widened only, never tightened, exactly as
    /// in [`crate::bm25::Bm25Index`]: a loose bound prunes less and stays
    /// correct, a tightened-too-far one drops results.
    impact: Impact,
    /// Next unused chunk slot for this term.
    next_slot: u32,
    /// The chunk directory, ascending by `max_id` and partitioning the row-id
    /// space: chunk `i` holds exactly the postings in
    /// `(chunks[i - 1].max_id, chunks[i].max_id]`.
    chunks: Vec<ChunkRef>,
}

impl TermRecord {
    fn new(term: &str) -> Self {
        Self {
            term: String::from(term),
            ..Self::default()
        }
    }

    /// Hand out a fresh chunk slot.
    ///
    /// Slots are per term and are reused whenever a chunk is rewritten in
    /// place, so this only advances on a *split*. Exhausting it needs four
    /// billion splits of one term, and the honest answer to that is an error
    /// rather than a wrapped slot silently aliasing another chunk.
    fn take_slot(&mut self) -> Result<u32> {
        let slot = self.next_slot;
        self.next_slot = self.next_slot.checked_add(1).ok_or_else(|| {
            Error::Index(alloc::format!(
                "paged BM25 term `{}` has exhausted its chunk slots",
                self.term
            ))
        })?;
        Ok(slot)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_string(&mut out, &self.term);
        put_len(&mut out, self.document_frequency as usize);
        put_len(&mut out, self.impact.max_frequency as usize);
        put_len(&mut out, self.impact.min_length as usize);
        put_len(&mut out, self.next_slot as usize);
        put_len(&mut out, self.chunks.len());
        for chunk in &self.chunks {
            put_len(&mut out, chunk.slot as usize);
            out.extend_from_slice(&chunk.max_id.to_le_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let term = cursor.string()?;
        let document_frequency = cursor.u32()?;
        let max_frequency = cursor.u32()?;
        let min_length = cursor.u32()?;
        let next_slot = cursor.u32()?;
        let count = cursor.count(12)?;
        let mut chunks = Vec::with_capacity(count);
        for _ in 0..count {
            let slot = cursor.u32()?;
            let max_id = RowId::from_le_bytes(cursor.array8()?);
            chunks.push(ChunkRef { slot, max_id });
        }
        Ok(Self {
            term,
            document_frequency,
            impact: Impact {
                max_frequency,
                min_length,
            },
            next_slot,
            chunks,
        })
    }
}

/// What one edit does to one term's postings for one row: replace it with this
/// frequency and length, or drop it.
type Edit = Option<(u32, u32)>;

// ------------------------------------------------------------------- the cache

/// Which structure a cache key names.
const BUCKET: u8 = 0;
const TERM: u8 = 1;
const CHUNK: u8 = 2;

/// A decoded structure, shared rather than cloned.
///
/// `Rc` and not a clone-on-read like [`crate::hnsw_paged`]'s node cache: a
/// postings chunk is kilobytes and a walk touches one per few hundred
/// postings, so copying it out of the cache on every hit would be most of the
/// cost of a query.
#[derive(Clone)]
enum Cached {
    Bucket(Rc<Vec<(String, u32)>>),
    /// `None` is a *negative* entry: a term ordinal with no record. Cached
    /// because a query term absent from the corpus is the common case and
    /// would otherwise cost a tree descent on every query that mentions it.
    Term(Option<Rc<TermRecord>>),
    Chunk(Rc<Vec<Posting>>),
}

/// A bounded least-recently-used cache, which is the whole of the resident
/// working set.
///
/// Recency is a second `BTreeMap` keyed by a monotonic stamp rather than the
/// `Vec` [`crate::hnsw_paged`]'s cache uses, because this one is touched per
/// postings chunk rather than per graph node: at a few thousand entries, an
/// `O(n)` `Vec::remove` per hit would be the query.
struct Cache {
    capacity: usize,
    entries: BTreeMap<(u8, u64), (u64, Cached)>,
    /// stamp -> key, so the least recently used is the first entry.
    recency: BTreeMap<u64, (u8, u64)>,
    clock: u64,
}

impl Cache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: BTreeMap::new(),
            recency: BTreeMap::new(),
            clock: 0,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.recency.clear();
    }

    fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        self.evict();
    }

    fn get(&mut self, kind: u8, key: u64) -> Option<Cached> {
        let entry = self.entries.get_mut(&(kind, key))?;
        let previous = entry.0;
        self.clock += 1;
        entry.0 = self.clock;
        let value = entry.1.clone();
        self.recency.remove(&previous);
        self.recency.insert(self.clock, (kind, key));
        Some(value)
    }

    fn insert(&mut self, kind: u8, key: u64, value: Cached) {
        self.clock += 1;
        if let Some((previous, _)) = self.entries.insert((kind, key), (self.clock, value)) {
            self.recency.remove(&previous);
        }
        self.recency.insert(self.clock, (kind, key));
        self.evict();
    }

    fn forget(&mut self, kind: u8, key: u64) {
        if let Some((stamp, _)) = self.entries.remove(&(kind, key)) {
            self.recency.remove(&stamp);
        }
    }

    fn evict(&mut self) {
        while self.entries.len() > self.capacity {
            let Some(stamp) = self.recency.keys().next().copied() else {
                break;
            };
            if let Some(key) = self.recency.remove(&stamp) {
                self.entries.remove(&key);
            }
        }
    }
}

/// FNV-1a over the term's bytes: the dictionary's key.
///
/// Hand-written and fixed here rather than taken from a crate, for the reason
/// every other encoding in this engine is: the bytes a database contains must
/// not change because a dependency changed its hash.
fn hash_term(term: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in term.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The postings-chunk key for one slot of one term.
fn chunk_key(ordinal: u32, slot: u32) -> u64 {
    (u64::from(ordinal) << 32) | u64::from(slot)
}

// ------------------------------------------------------------------- the index

/// A BM25 index over a [`Storage`] backend.
///
/// See the [module note](self) for the layout, the memory model and the
/// identical-scores requirement.
pub struct PagedBm25Index<S: Storage> {
    storage: S,
    /// Documents: `row id -> (length, term ordinals)`.
    documents: String,
    /// Dictionary: `hash(term) -> [(term, ordinal)]`.
    dictionary: String,
    /// Term records: `ordinal -> TermRecord`.
    terms: String,
    /// Postings: `(ordinal, slot) -> [Posting]`.
    postings: String,

    /// Live document count: what `doc_count` means when scoring.
    ///
    /// `Cell`, along with the two below and `stored_version`, because these
    /// four describe *the file* rather than this instance, and the file is
    /// shared — see [`PagedBm25Index::adopt_stored_statistics`], which has to
    /// be able to correct them from a `&self` search as well as from a `&mut
    /// self` commit.
    live: Cell<usize>,
    /// Sum of every live document's length, so the average is `O(1)`.
    total_length: Cell<u64>,
    /// Next unused term ordinal.
    next_term: Cell<u32>,

    /// Documents accepted since the last commit; `None` is a removal.
    ///
    /// A map and not a list, so that the last operation on a row wins and a
    /// row touched ten times in one batch is one unit of work — the same
    /// collapsing [`crate::Engine::catch_up_indexes`] does for the same
    /// reason. Iterating it in row-id order is also what keeps a batch's
    /// effect independent of the order the caller happened to hand it over.
    pending: BTreeMap<RowId, Option<String>>,
    /// How large `pending` may grow before a caller that owns its transaction
    /// applies it.
    pending_limit: usize,

    cache: RefCell<Cache>,

    /// Whether this index makes its own writes durable. False when it shares a
    /// transaction with a caller that will commit for it.
    owns_transaction: bool,
    /// The write version the structure in storage describes. `None` means "not
    /// current": new, or caught by a crash between batches.
    stored_version: Cell<Option<u64>>,
    /// The write version the commit in flight will describe.
    pending_version: Option<u64>,
}

impl<S: Storage> PagedBm25Index<S> {
    /// An empty index on a fresh namespace of `storage`.
    pub fn new(storage: S, namespace: impl Into<String>) -> Self {
        let base: String = namespace.into();
        Self {
            documents: base.clone(),
            dictionary: alloc::format!("{base}\u{1}d"),
            terms: alloc::format!("{base}\u{1}x"),
            postings: alloc::format!("{base}\u{1}p"),
            storage,
            live: Cell::new(0),
            total_length: Cell::new(0),
            next_term: Cell::new(0),
            pending: BTreeMap::new(),
            pending_limit: DEFAULT_PENDING_DOCUMENTS,
            cache: RefCell::new(Cache::new(DEFAULT_CACHE_ENTRIES)),
            owns_transaction: true,
            stored_version: Cell::new(None),
            pending_version: None,
        }
    }

    /// Open a previously committed index, restoring its header so it answers
    /// immediately without a rebuild.
    ///
    /// Unlike [`crate::hnsw_paged::PagedHnswIndex::open`] this reads *nothing*
    /// but the header — there is no resident `RowId -> node` map to rebuild,
    /// because there are no node indices. Re-opening is therefore `O(1)`, which
    /// is what makes adopting another handle's commit cheap.
    pub fn open(storage: S, namespace: impl Into<String>) -> Result<Self> {
        let mut index = Self::new(storage, namespace);
        index.restore()?;
        Ok(index)
    }

    /// Set the resident working-set bound, in decoded entries.
    pub fn with_cache_capacity(mut self, entries: usize) -> Self {
        self.cache.get_mut().set_capacity(entries);
        self
    }

    /// Bound how many documents may be buffered before they are applied.
    ///
    /// Only honoured while this index owns its transaction: applying a batch
    /// early means committing it, and committing inside a caller's transaction
    /// is the one thing this must never do. Inside the engine the batch is
    /// therefore bounded by the caller's commit interval instead — see the
    /// [module note](self#what-stays-resident).
    pub fn with_pending_limit(mut self, documents: usize) -> Self {
        self.pending_limit = documents.max(1);
        self
    }

    /// Write into the caller's open transaction and leave the commit to them.
    ///
    /// Use this whenever the backing storage is shared with something else that
    /// is mid-transaction — inside the engine, always. See
    /// [`crate::hnsw_paged::PagedHnswIndex::joined_to_caller_transaction`],
    /// which this mirrors exactly.
    pub fn joined_to_caller_transaction(mut self) -> Self {
        self.owns_transaction = false;
        self
    }

    /// Whether this index will call [`Storage::commit`] itself.
    pub fn owns_transaction(&self) -> bool {
        self.owns_transaction
    }

    /// Number of indexed documents.
    pub fn len(&self) -> usize {
        self.live.get()
    }

    /// Whether the index holds no documents.
    pub fn is_empty(&self) -> bool {
        self.live.get() == 0
    }

    /// How many decoded entries are resident right now.
    pub fn cache_len(&self) -> usize {
        self.cache.borrow().len()
    }

    /// The bound `cache_len` will never exceed.
    pub fn cache_capacity(&self) -> usize {
        self.cache.borrow().capacity()
    }

    /// Hand back the backing storage, dropping the in-memory working set.
    pub fn into_storage(self) -> S {
        self.storage
    }

    /// Delete the whole index, leaving an empty one over the same namespaces.
    ///
    /// The engine calls this when it has decided to rebuild from the rows.
    /// Without it, re-indexing every row on top of a structure that just
    /// restored itself would find each document already present and retire it
    /// first — correct, but twice the work and twice the writes.
    pub fn clear(&mut self) -> Result<()> {
        for namespace in [
            self.documents.clone(),
            self.dictionary.clone(),
            self.terms.clone(),
            self.postings.clone(),
        ] {
            let stale: Vec<RowId> = RowScan::new(&self.storage, &namespace)
                .map(|row| row.map(|(id, _)| id))
                .collect::<Result<_>>()?;
            for id in stale {
                self.storage.delete_row(&namespace, id)?;
                self.flush_if_transaction_is_full()?;
            }
        }
        self.live.set(0);
        self.total_length.set(0);
        self.next_term.set(0);
        self.pending.clear();
        self.cache.get_mut().clear();
        // Until something completes the rebuild there is no current index
        // here — say so, rather than leaving the old stamp on an empty one.
        self.stored_version.set(None);
        self.write_header(None)
    }

    // --------------------------------------------------------------- header

    fn header_key(&self) -> String {
        let mut key = self.documents.clone();
        key.push_str(":header");
        key
    }

    /// Restore the corpus statistics from a prior commit.
    ///
    /// A header this build cannot read is not an error and not something to
    /// half-trust: the structure is purged and the index comes back empty,
    /// which is exactly what "nothing saved" looks like to the engine, so its
    /// ordinary rebuild-from-rows path takes over. Leaving the rows in place
    /// would be worse than deleting them — a later build would find a
    /// dictionary it did not write.
    fn restore(&mut self) -> Result<()> {
        let Some(bytes) = self.storage.get_meta(&self.header_key())? else {
            return Ok(());
        };
        let mut cursor = Cursor::new(&bytes);
        let Ok(format) = cursor.u32() else {
            return self.clear();
        };
        if format != FORMAT_VERSION {
            return self.clear();
        }
        self.live.set(u64::from_le_bytes(cursor.array8()?) as usize);
        self.total_length.set(u64::from_le_bytes(cursor.array8()?));
        self.next_term.set(cursor.u32()?);
        // Absent on a header written mid-build, which is how an index a crash
        // caught half-applied is told apart from a complete one.
        self.stored_version.set(match cursor.u8() {
            Ok(1) => Some(u64::from_le_bytes(cursor.array8()?)),
            _ => None,
        });
        Ok(())
    }

    /// Re-read the corpus statistics from the header, and throw away the cache
    /// if they moved.
    ///
    /// **This is the price of keeping the index in the file rather than in the
    /// handle**, and it is not optional. Two `Database` handles on one
    /// database each hold their own `PagedBm25Index` over the *same*
    /// namespaces, so the other one deciding to rebuild — which is what every
    /// handle does when it opens on a stamp that is not current — rewrites the
    /// document records, the dictionary and the term ordinals underneath this
    /// one, without changing a single row and therefore without moving the
    /// write version this handle watches. What this instance remembered is
    /// then wrong in two ways that do not announce themselves:
    ///
    /// * `live` and `total_length` are what `idf` and the length normalisation
    ///   are computed from, so a stale pair rescores the entire corpus. The
    ///   only visible symptom is that the retire step tries to subtract a
    ///   document this instance never counted.
    /// * **Term ordinals are handed out from a counter in the header.** Two
    ///   instances that both believe the next ordinal is 5 will give it to two
    ///   different terms, and each will then read the other's postings under
    ///   it. That is a wrong answer with no error anywhere, which is exactly
    ///   what `docs/indexes.md` says this design has to make impossible.
    ///
    /// So the header is re-read rather than remembered, and the decoded cache
    /// goes with it whenever it moved. It costs one metadata read per commit
    /// and per search, and it is always consistent with the document records
    /// this instance can see, because both are written into the same
    /// transaction and the header is written last.
    fn adopt_stored_statistics(&self) -> Result<()> {
        let Some(bytes) = self.storage.get_meta(&self.header_key())? else {
            // No header at all: nothing has ever been committed here, so
            // whatever this instance holds is its own uncommitted work.
            return Ok(());
        };
        let mut cursor = Cursor::new(&bytes);
        let (Ok(format), Ok(live), Ok(total_length), Ok(next_term)) =
            (cursor.u32(), cursor.array8(), cursor.array8(), cursor.u32())
        else {
            return Ok(());
        };
        if format != FORMAT_VERSION {
            // A header this build cannot read is handled once, at
            // [`PagedBm25Index::restore`], where there is a `&mut self` to
            // purge with. Leaving it alone here is the conservative half of
            // the same decision.
            return Ok(());
        }
        let live = u64::from_le_bytes(live) as usize;
        let total_length = u64::from_le_bytes(total_length);
        // Unconditionally, and before the early return: the stamp is what the
        // *file* claims, and another handle can restamp without moving a
        // count — or leave no stamp at all, having been caught mid-build. This
        // instance's copy of that claim is a cache like any other.
        self.stored_version.set(match cursor.u8() {
            Ok(1) => cursor.array8().ok().map(u64::from_le_bytes),
            _ => None,
        });
        if live == self.live.get()
            && total_length == self.total_length.get()
            && next_term == self.next_term.get()
        {
            return Ok(());
        }
        self.live.set(live);
        self.total_length.set(total_length);
        self.next_term.set(next_term);
        // Everything decoded out of the old structure is suspect, ordinals
        // most of all.
        self.cache.borrow_mut().clear();
        Ok(())
    }

    /// Write the header. `version` is `None` for an index that is not (yet)
    /// complete — see [`PagedBm25Index::flush_if_transaction_is_full`].
    fn write_header(&mut self, version: Option<u64>) -> Result<()> {
        let mut out = Vec::with_capacity(30);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.live.get() as u64).to_le_bytes());
        out.extend_from_slice(&self.total_length.get().to_le_bytes());
        out.extend_from_slice(&self.next_term.get().to_le_bytes());
        match version {
            Some(version) => {
                out.push(1);
                out.extend_from_slice(&version.to_le_bytes());
            }
            None => out.push(0),
        }
        let key = self.header_key();
        self.storage.put_meta(&key, &out)
    }

    /// Commit the batch so far when the backend says the open transaction is
    /// close to its limit.
    ///
    /// Applying a batch writes far more than one transaction can hold — a
    /// write-ahead log region is a hard ceiling, not a slow path. The header
    /// written here carries **no version stamp**, so a crash between batches
    /// leaves an index that is structurally readable but visibly not current,
    /// and the engine rebuilds it rather than trusting it.
    ///
    /// **Called after every single row write**, not once per document, and that
    /// granularity is load-bearing rather than cautious. One document of
    /// ordinary English has around a hundred distinct terms, and the first time
    /// each is seen it costs a dictionary bucket, a term record and a postings
    /// chunk — three rows on three different leaf pages, because the terms are
    /// scattered across the whole key space. Under copy-on-write that is
    /// ~300 pages the commit record has to carry, which is a third of a megabyte
    /// past the ceiling from *one* document. Checking per document is checking
    /// after the damage.
    ///
    /// Nothing is flushed when the caller owns the transaction: committing then
    /// would make the caller's own buffered writes durable early.
    fn flush_if_transaction_is_full(&mut self) -> Result<()> {
        if !self.owns_transaction || !self.storage.transaction_is_nearly_full() {
            return Ok(());
        }
        self.write_header(None)?;
        self.storage.commit()
    }

    /// Stamp the header with the version this index now describes, then commit
    /// — unless the caller owns the transaction, in which case the writes stay
    /// buffered for it to commit with everything else it has in flight.
    fn finish(&mut self) -> Result<()> {
        self.write_header(self.pending_version)?;
        self.stored_version.set(self.pending_version);
        if self.owns_transaction {
            self.storage.commit()
        } else {
            Ok(())
        }
    }

    // ---------------------------------------------------------------- reads

    fn fetch_bucket(&self, hash: u64) -> Result<Rc<Vec<(String, u32)>>> {
        if let Some(Cached::Bucket(bucket)) = self.cache.borrow_mut().get(BUCKET, hash) {
            return Ok(bucket);
        }
        let bucket = match self.storage.get_row(&self.dictionary, hash)? {
            Some(bytes) => Rc::new(decode_bucket(bytes.as_slice())?),
            None => Rc::new(Vec::new()),
        };
        self.cache
            .borrow_mut()
            .insert(BUCKET, hash, Cached::Bucket(bucket.clone()));
        Ok(bucket)
    }

    /// The ordinal `term` is indexed under, if it is indexed at all.
    ///
    /// "At all" is exact and matters: [`crate::bm25::Bm25Index`] drops a term
    /// from its dictionary the moment its last posting goes, so a query term
    /// whose documents have all been deleted contributes nothing there. A
    /// paged backend that left the entry behind would give that term an `idf`
    /// and a cursor over an empty list — harmless for the sum, but it would
    /// change nothing only by accident. The entry is deleted here too.
    fn lookup_term(&self, term: &str) -> Result<Option<u32>> {
        let bucket = self.fetch_bucket(hash_term(term))?;
        Ok(bucket
            .iter()
            .find(|(candidate, _)| candidate == term)
            .map(|(_, ordinal)| *ordinal))
    }

    fn fetch_term(&self, ordinal: u32) -> Result<Option<Rc<TermRecord>>> {
        let key = u64::from(ordinal);
        if let Some(Cached::Term(record)) = self.cache.borrow_mut().get(TERM, key) {
            return Ok(record);
        }
        let record = match self.storage.get_row(&self.terms, key)? {
            Some(bytes) => Some(Rc::new(TermRecord::decode(bytes.as_slice())?)),
            None => None,
        };
        self.cache
            .borrow_mut()
            .insert(TERM, key, Cached::Term(record.clone()));
        Ok(record)
    }

    fn fetch_chunk(&self, ordinal: u32, slot: u32) -> Result<Rc<Vec<Posting>>> {
        let key = chunk_key(ordinal, slot);
        if let Some(Cached::Chunk(postings)) = self.cache.borrow_mut().get(CHUNK, key) {
            return Ok(postings);
        }
        let postings = match self.storage.get_row(&self.postings, key)? {
            Some(bytes) => Rc::new(decode_chunk(bytes.as_slice())?),
            // A directory entry naming a chunk that is not there is corruption
            // the walk must not paper over — but it also cannot be repaired
            // here, and the rows are the source of truth, so it is reported
            // and the engine rebuilds.
            None => {
                return Err(Error::Corrupt(alloc::format!(
                    "paged BM25 term {ordinal} names chunk {slot}, which is not in storage"
                )))
            }
        };
        self.cache
            .borrow_mut()
            .insert(CHUNK, key, Cached::Chunk(postings.clone()));
        Ok(postings)
    }

    fn read_document(&self, id: RowId) -> Result<Option<(u32, Vec<u32>)>> {
        let Some(bytes) = self.storage.get_row(&self.documents, id)? else {
            return Ok(None);
        };
        let mut cursor = Cursor::new(bytes.as_slice());
        let length = cursor.u32()?;
        let count = cursor.count(4)?;
        let mut ordinals = Vec::with_capacity(count);
        for _ in 0..count {
            ordinals.push(cursor.u32()?);
        }
        Ok(Some((length, ordinals)))
    }

    // --------------------------------------------------------------- writes

    fn store_bucket(&mut self, hash: u64, bucket: Vec<(String, u32)>) -> Result<()> {
        let mut out = Vec::new();
        put_len(&mut out, bucket.len());
        for (term, ordinal) in &bucket {
            put_string(&mut out, term);
            put_len(&mut out, *ordinal as usize);
        }
        self.storage.put_row(&self.dictionary, hash, &out)?;
        self.cache
            .get_mut()
            .insert(BUCKET, hash, Cached::Bucket(Rc::new(bucket)));
        self.flush_if_transaction_is_full()
    }

    fn delete_bucket(&mut self, hash: u64) -> Result<()> {
        self.storage.delete_row(&self.dictionary, hash)?;
        self.cache
            .get_mut()
            .insert(BUCKET, hash, Cached::Bucket(Rc::new(Vec::new())));
        Ok(())
    }

    fn store_term(&mut self, ordinal: u32, record: TermRecord) -> Result<()> {
        let key = u64::from(ordinal);
        self.storage.put_row(&self.terms, key, &record.encode())?;
        self.cache
            .get_mut()
            .insert(TERM, key, Cached::Term(Some(Rc::new(record))));
        self.flush_if_transaction_is_full()
    }

    fn delete_term(&mut self, ordinal: u32) -> Result<()> {
        let key = u64::from(ordinal);
        self.storage.delete_row(&self.terms, key)?;
        self.cache.get_mut().insert(TERM, key, Cached::Term(None));
        Ok(())
    }

    fn store_chunk(&mut self, ordinal: u32, slot: u32, postings: Vec<Posting>) -> Result<()> {
        let key = chunk_key(ordinal, slot);
        let mut out = Vec::with_capacity(4 + postings.len() * 16);
        put_len(&mut out, postings.len());
        for posting in &postings {
            out.extend_from_slice(&posting.id.to_le_bytes());
            out.extend_from_slice(&posting.frequency.to_le_bytes());
            out.extend_from_slice(&posting.length.to_le_bytes());
        }
        self.storage.put_row(&self.postings, key, &out)?;
        self.cache
            .get_mut()
            .insert(CHUNK, key, Cached::Chunk(Rc::new(postings)));
        self.flush_if_transaction_is_full()
    }

    fn delete_chunk(&mut self, ordinal: u32, slot: u32) -> Result<()> {
        let key = chunk_key(ordinal, slot);
        self.storage.delete_row(&self.postings, key)?;
        self.cache.get_mut().forget(CHUNK, key);
        Ok(())
    }

    /// The ordinal for `term`, allocating one — and its empty term record and
    /// dictionary entry — if this is a term the index has not seen.
    fn term_for(&mut self, term: &str) -> Result<u32> {
        let hash = hash_term(term);
        let bucket = self.fetch_bucket(hash)?;
        if let Some((_, ordinal)) = bucket.iter().find(|(candidate, _)| candidate == term) {
            return Ok(*ordinal);
        }
        let ordinal = self.next_term.get();
        self.next_term.set(ordinal.checked_add(1).ok_or_else(|| {
            Error::Index(String::from(
                "paged BM25 index has exhausted its term ordinals; rebuild it",
            ))
        })?);
        let mut bucket = (*bucket).clone();
        bucket.push((String::from(term), ordinal));
        // Sorted, so that a bucket's bytes are a function of its contents and
        // not of the order two colliding terms were first seen in.
        bucket.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        self.store_bucket(hash, bucket)?;
        self.store_term(ordinal, TermRecord::new(term))?;
        Ok(ordinal)
    }

    /// Apply the buffered batch: retire what each row used to say, index what
    /// it says now, then rewrite each affected term's chunks **once**.
    ///
    /// Term-major and not document-major, which is the difference between a
    /// bulk load touching a term's chunks once per document that mentions it
    /// and once per batch. It is also what bounds the intermediate: the edit
    /// map is `O(documents in the batch × their distinct terms)`, never
    /// `O(corpus)`.
    fn apply(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // Before anything is written: another handle may have rebuilt this
        // index in the file since the last batch, and the counters and the
        // ordinal counter it left are the ones to build on.
        self.adopt_stored_statistics()?;
        let pending = core::mem::take(&mut self.pending);
        // term ordinal -> row id -> what to do with that row's posting. A map
        // of maps so that "this row lost the term" followed by "this row has
        // the term again" — an ordinary `UPDATE` that kept a word — collapses
        // to one edit rather than two that have to be ordered correctly.
        let mut edits: BTreeMap<u32, BTreeMap<RowId, Edit>> = BTreeMap::new();

        for (id, text) in pending {
            if let Some((length, ordinals)) = self.read_document(id)? {
                for ordinal in ordinals {
                    edits.entry(ordinal).or_default().insert(id, None);
                }
                // `checked_`, not `-`: reaching zero here would mean the
                // header this instance last read disagrees with the document
                // records in the file, which `adopt_stored_statistics` exists
                // to prevent. If it happens anyway the answer is to refuse and
                // let the engine rebuild from the rows, because carrying on
                // means every score in the corpus is computed against a
                // document count that is wrong.
                let live = self.live.get().checked_sub(1).ok_or_else(|| {
                    Error::Corrupt(alloc::format!(
                        "paged BM25 index holds a record for row {id} but counts no documents"
                    ))
                })?;
                self.live.set(live);
                self.total_length
                    .set(self.total_length.get().saturating_sub(u64::from(length)));
                self.storage.delete_row(&self.documents, id)?;
            }
            let Some(text) = text else {
                continue;
            };

            let tokens = tokenize(&text);
            let length = tokens.len() as u32;
            // Count first, then write one posting per distinct term, rather
            // than hunting the postings list once per occurrence.
            let mut frequencies: BTreeMap<u32, u32> = BTreeMap::new();
            for token in &tokens {
                let ordinal = self.term_for(token)?;
                *frequencies.entry(ordinal).or_insert(0) += 1;
            }
            let mut out = Vec::with_capacity(8 + frequencies.len() * 4);
            put_len(&mut out, length as usize);
            put_len(&mut out, frequencies.len());
            for (ordinal, frequency) in &frequencies {
                out.extend_from_slice(&ordinal.to_le_bytes());
                edits
                    .entry(*ordinal)
                    .or_default()
                    .insert(id, Some((*frequency, length)));
            }
            self.storage.put_row(&self.documents, id, &out)?;
            self.live.set(self.live.get() + 1);
            self.total_length
                .set(self.total_length.get() + u64::from(length));
            self.flush_if_transaction_is_full()?;
        }

        for (ordinal, term_edits) in edits {
            self.apply_term_edits(ordinal, &term_edits)?;
            self.flush_if_transaction_is_full()?;
        }
        Ok(())
    }

    /// Rewrite one term's postings for a batch of edits, touching only the
    /// chunks the edits fall in.
    ///
    /// The directory partitions the row-id space, so the edits — already in
    /// row-id order — split cleanly into runs, one run per chunk. Chunks no
    /// edit falls in are copied to the new directory without being read at
    /// all, which is what keeps a one-document `UPDATE` from rewriting a
    /// hundred-thousand-posting list.
    fn apply_term_edits(&mut self, ordinal: u32, edits: &BTreeMap<RowId, Edit>) -> Result<()> {
        let Some(record) = self.fetch_term(ordinal)? else {
            return Err(Error::Corrupt(alloc::format!(
                "paged BM25 edit names term {ordinal}, which has no record"
            )));
        };
        let mut record = (*record).clone();
        let directory = core::mem::take(&mut record.chunks);
        let edits: Vec<(RowId, Edit)> = edits.iter().map(|(id, edit)| (*id, *edit)).collect();

        let mut rebuilt: Vec<ChunkRef> = Vec::with_capacity(directory.len() + 1);
        let mut next_edit = 0usize;
        let mut next_chunk = 0usize;

        while next_edit < edits.len() {
            // Chunks entirely below the next edit are untouched. Copied, not
            // read.
            while next_chunk < directory.len() && directory[next_chunk].max_id < edits[next_edit].0
            {
                rebuilt.push(directory[next_chunk]);
                next_chunk += 1;
            }

            let (target, limit) = if next_chunk < directory.len() {
                let target = directory[next_chunk];
                next_chunk += 1;
                (Some(target), target.max_id)
            } else {
                // Past the last chunk: every remaining edit appends. It goes
                // into the final chunk when there is one, so that a stream of
                // appends fills chunks instead of leaving a trail of
                // one-posting ones, and into fresh chunks when there is not.
                (rebuilt.pop(), RowId::MAX)
            };

            let run_end = next_edit + edits[next_edit..].partition_point(|(id, _)| *id <= limit);
            let run = &edits[next_edit..run_end];
            next_edit = run_end;

            let mut postings: Vec<Posting> = match target {
                Some(chunk) => (*self.fetch_chunk(ordinal, chunk.slot)?).clone(),
                None => Vec::new(),
            };
            if !merge_edits(&mut postings, run, &mut record) {
                // Nothing in this chunk actually moved — a removal of a row
                // this term never had, which is what an `UPDATE` that dropped
                // a word looks like from the other terms' point of view.
                if let Some(chunk) = target {
                    rebuilt.push(chunk);
                }
                continue;
            }

            if postings.is_empty() {
                if let Some(chunk) = target {
                    self.delete_chunk(ordinal, chunk.slot)?;
                }
                continue;
            }
            // The target's slot is reused for the first piece, so an append
            // that does not split costs no slot at all.
            let mut reuse = target.map(|chunk| chunk.slot);
            let pieces: Vec<Vec<Posting>> = postings
                .chunks(POSTINGS_PER_CHUNK)
                .map(<[Posting]>::to_vec)
                .collect();
            for piece in pieces {
                let slot = match reuse.take() {
                    Some(slot) => slot,
                    None => record.take_slot()?,
                };
                let max_id = piece[piece.len() - 1].id;
                self.store_chunk(ordinal, slot, piece)?;
                rebuilt.push(ChunkRef { slot, max_id });
            }
        }
        while next_chunk < directory.len() {
            rebuilt.push(directory[next_chunk]);
            next_chunk += 1;
        }
        record.chunks = rebuilt;

        if record.document_frequency == 0 {
            // A term nothing mentions any more leaves the index entirely, so
            // that it is absent from every document frequency and from
            // `lookup_term` — which is what `Bm25Index` does by dropping an
            // emptied postings list.
            return self.retire_term(ordinal, &record);
        }
        self.store_term(ordinal, record)
    }

    /// Drop a term whose last posting has gone: its chunks, its record and its
    /// dictionary entry.
    fn retire_term(&mut self, ordinal: u32, record: &TermRecord) -> Result<()> {
        for chunk in &record.chunks {
            self.delete_chunk(ordinal, chunk.slot)?;
        }
        self.delete_term(ordinal)?;
        let hash = hash_term(&record.term);
        let bucket = self.fetch_bucket(hash)?;
        let mut bucket = (*bucket).clone();
        bucket.retain(|(term, _)| *term != record.term);
        if bucket.is_empty() {
            self.delete_bucket(hash)
        } else {
            self.store_bucket(hash, bucket)
        }
    }
}

/// Merge one chunk's postings with the edits that fall inside it, maintaining
/// the term's document frequency and impact bound as it goes.
///
/// Answers whether anything moved, so a run of removals for rows this term
/// never held costs no write at all.
fn merge_edits(
    postings: &mut Vec<Posting>,
    run: &[(RowId, Edit)],
    record: &mut TermRecord,
) -> bool {
    let mut merged: Vec<Posting> = Vec::with_capacity(postings.len() + run.len());
    let mut changed = false;
    let mut existing = 0usize;
    let mut edit = 0usize;

    while existing < postings.len() || edit < run.len() {
        let take_edit = match (postings.get(existing), run.get(edit)) {
            (Some(posting), Some((id, _))) => *id <= posting.id,
            (None, Some(_)) => true,
            _ => false,
        };
        if !take_edit {
            merged.push(postings[existing]);
            existing += 1;
            continue;
        }
        let (id, action) = run[edit];
        edit += 1;
        let replaced = postings
            .get(existing)
            .is_some_and(|posting| posting.id == id);
        if replaced {
            existing += 1;
        }
        match action {
            Some((frequency, length)) => {
                if !replaced {
                    record.document_frequency += 1;
                }
                record.impact.widen(frequency, length);
                merged.push(Posting {
                    id,
                    frequency,
                    length,
                });
                changed = true;
            }
            None => {
                if replaced {
                    record.document_frequency -= 1;
                    changed = true;
                }
            }
        }
    }
    if changed {
        *postings = merged;
    }
    changed
}

fn decode_bucket(bytes: &[u8]) -> Result<Vec<(String, u32)>> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.count(9)?;
    let mut bucket = Vec::with_capacity(count);
    for _ in 0..count {
        let term = cursor.string()?;
        let ordinal = cursor.u32()?;
        bucket.push((term, ordinal));
    }
    Ok(bucket)
}

fn decode_chunk(bytes: &[u8]) -> Result<Vec<Posting>> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.count(16)?;
    let mut postings = Vec::with_capacity(count);
    for _ in 0..count {
        let id = RowId::from_le_bytes(cursor.array8()?);
        let frequency = cursor.u32()?;
        let length = cursor.u32()?;
        postings.push(Posting {
            id,
            frequency,
            length,
        });
    }
    Ok(postings)
}

// -------------------------------------------------------------------- the walk

/// One query term's walk over its postings, chunk by chunk.
///
/// The paged twin of [`crate::bm25`]'s `TermWalk`, with the same contract:
/// [`PagedTermWalk::current`] is the document the cursor is parked on, and
/// [`PagedTermWalk::seek`] moves forward only. The invariant that makes
/// `current` cheap is that `position` always addresses a live posting or the
/// walk is exhausted, re-established by `settle` after every move.
struct PagedTermWalk<'a, S: Storage> {
    index: &'a PagedBm25Index<S>,
    ordinal: u32,
    record: Rc<TermRecord>,
    /// The next directory entry to load; the loaded one is `chunk - 1`.
    chunk: usize,
    postings: Rc<Vec<Posting>>,
    position: usize,
    idf: f32,
    /// The largest contribution any document in this term's postings could
    /// take. What MaxScore orders and partitions the terms on.
    ceiling: f32,
}

impl<'a, S: Storage> PagedTermWalk<'a, S> {
    fn start(
        index: &'a PagedBm25Index<S>,
        ordinal: u32,
        record: Rc<TermRecord>,
        idf: f32,
        ceiling: f32,
    ) -> Result<Self> {
        let mut walk = Self {
            index,
            ordinal,
            record,
            chunk: 0,
            postings: Rc::new(Vec::new()),
            position: 0,
            idf,
            ceiling,
        };
        walk.settle()?;
        Ok(walk)
    }

    /// Make `position` address a posting, loading chunks as needed, or leave
    /// the walk exhausted.
    fn settle(&mut self) -> Result<()> {
        while self.position >= self.postings.len() {
            if self.chunk >= self.record.chunks.len() {
                self.postings = Rc::new(Vec::new());
                self.position = 0;
                return Ok(());
            }
            let slot = self.record.chunks[self.chunk].slot;
            self.postings = self.index.fetch_chunk(self.ordinal, slot)?;
            self.chunk += 1;
            self.position = 0;
        }
        Ok(())
    }

    /// The posting this cursor is parked on, if it has any left.
    fn current(&self) -> Option<Posting> {
        self.postings.get(self.position).copied()
    }

    /// Advance to `doc` and report the term's frequency there, or `None` if
    /// this term does not occur in it. Either way the cursor ends up parked on
    /// the first posting at or after `doc`.
    fn seek(&mut self, doc: RowId) -> Result<Option<u32>> {
        // Chunks whose greatest row id is below `doc` are skipped without
        // being read: the directory says so. This is the whole reason a
        // demoted MaxScore cursor thousands of postings behind is cheap.
        if self.current().is_some_and(|posting| posting.id < doc)
            && self.record.chunks[self.chunk - 1].max_id < doc
        {
            let mut target = self.chunk;
            while target < self.record.chunks.len() && self.record.chunks[target].max_id < doc {
                target += 1;
            }
            self.chunk = target;
            self.postings = Rc::new(Vec::new());
            self.position = 0;
            self.settle()?;
        }
        if self.current().is_some_and(|posting| posting.id < doc) {
            let remaining = &self.postings[self.position..];
            self.position += remaining.partition_point(|posting| posting.id < doc);
            self.settle()?;
        }
        match self.current() {
            Some(posting) if posting.id == doc => {
                self.position += 1;
                self.settle()?;
                Ok(Some(posting.frequency))
            }
            _ => Ok(None),
        }
    }
}

// ------------------------------------------------------------------ the trait

impl<S: Storage> FullTextIndex for PagedBm25Index<S> {
    fn insert(&mut self, id: RowId, text: &str) -> Result<()> {
        self.pending.insert(id, Some(String::from(text)));
        self.apply_if_pending_is_full()
    }

    fn remove(&mut self, id: RowId) -> Result<()> {
        self.pending.insert(id, None);
        self.apply_if_pending_is_full()
    }

    fn commit(&mut self) -> Result<()> {
        self.adopt_stored_statistics()?;
        if self.pending.is_empty() {
            // Nothing changed here, but the database moved on — a write to
            // some other table advances the write version too. Restamping is
            // one metadata write and it is the difference between reopening
            // instantly and rebuilding this index from the rows for nothing.
            if self.pending_version != self.stored_version.get() {
                return self.finish();
            }
            return Ok(());
        }
        self.apply()?;
        self.finish()
    }

    fn search(&self, query: &str, k: usize, filter: Option<&RowFilter>) -> Result<Vec<Scored>> {
        // A read has to do this too, not only a write: another handle can
        // rebuild this index without changing a row, so there is no write
        // version for the engine to notice on this handle's behalf.
        self.adopt_stored_statistics()?;
        let doc_count = self.live.get();
        if doc_count == 0 || k == 0 {
            return Ok(Vec::new());
        }
        let average = average_length(self.total_length.get(), doc_count);

        // One cursor per *occurrence* of a term in the query rather than per
        // distinct term: a query that repeats a term scores it twice, which is
        // what `Bm25Index` does and so what every published BM25 number here
        // already means.
        let mut cursors: Vec<PagedTermWalk<S>> = Vec::new();
        for term in tokenize(query) {
            let Some(ordinal) = self.lookup_term(&term)? else {
                continue;
            };
            let Some(record) = self.fetch_term(ordinal)? else {
                continue;
            };
            let idf = idf(doc_count, record.document_frequency as usize);
            let ceiling = record.impact.ceiling(idf, average);
            cursors.push(PagedTermWalk::start(self, ordinal, record, idf, ceiling)?);
        }
        if cursors.is_empty() {
            return Ok(Vec::new());
        }

        // MaxScore's ordering, as in `Bm25Index::search`: cheapest term first,
        // and the running total of what the terms up to each point could
        // contribute. Summed in `f64` so that rounding can only ever make the
        // bound larger than the `f32` scores it gates — a bound rounded the
        // other way would prune a document it could not justify pruning.
        let mut order: Vec<usize> = (0..cursors.len()).collect();
        order.sort_unstable_by(|left, right| {
            cursors[*left]
                .ceiling
                .partial_cmp(&cursors[*right].ceiling)
                .unwrap_or(Ordering::Equal)
                .then(left.cmp(right))
        });
        let mut headroom: Vec<f64> = Vec::with_capacity(order.len() + 1);
        let mut running = 0.0f64;
        headroom.push(running);
        for term in &order {
            running += f64::from(cursors[*term].ceiling);
            headroom.push(running);
        }

        let mut demoted = 0usize;
        let mut best = TopK::new(k);
        loop {
            // The next document any driving cursor still has. Query terms are
            // few, so a linear minimum beats maintaining a heap over them.
            //
            // The *posting* and not just the row id, because the document's
            // length rides along in it — see the module note. At least one
            // driving cursor is parked exactly on the minimum, so the length
            // is in hand before any cursor has been advanced past it.
            let mut next: Option<Posting> = None;
            for term in &order[demoted..] {
                let Some(posting) = cursors[*term].current() else {
                    continue;
                };
                let better = match next {
                    Some(current) => posting.id < current.id,
                    None => true,
                };
                if better {
                    next = Some(posting);
                }
            }
            let Some(next) = next else {
                break;
            };

            // A document the filter rejects is skipped without consuming a
            // result slot, and never raises the threshold — so a selective
            // filter costs the walk its skipping and never its correctness.
            let admitted = match filter {
                Some(filter) => filter(next.id)?,
                None => true,
            };
            let normalisation = length_normalisation(next.length, average);
            let mut score = 0.0f32;
            // Query order, not `order`: floating-point addition is
            // order-dependent and the published score is the query-order sum.
            // Every cursor is advanced whether or not the document is
            // admitted, because the walk past it is what makes the next one
            // reachable.
            for cursor in &mut cursors {
                if let Some(frequency) = cursor.seek(next.id)? {
                    if admitted {
                        score += contribution(cursor.idf, frequency, normalisation);
                    }
                }
            }
            if !admitted {
                continue;
            }

            best.offer(Scored::new(next.id, score));
            let Some(threshold) = best.threshold() else {
                continue;
            };
            while demoted < order.len() && headroom[demoted + 1] < f64::from(threshold) {
                demoted += 1;
            }
            if demoted == order.len() {
                // Not even every term together can reach the k-th best, so
                // nothing still unread can enter the answer.
                break;
            }
        }

        Ok(best.into_ranked())
    }

    /// The index is already in the database; there is no blob to hand back,
    /// and asking for one would serialise the very thing this backend exists
    /// not to hold in memory. Currency is tracked by the header stamp instead.
    fn save(&self) -> Option<Vec<u8>> {
        None
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
}

impl<S: Storage> PagedBm25Index<S> {
    /// Apply the buffered batch early, for a caller that owns its transaction.
    ///
    /// Applying means committing — the writes are far larger than one
    /// write-ahead-log record — so this does nothing at all inside a caller's
    /// transaction, where committing is the one forbidden move. The header it
    /// leaves behind carries no stamp, so a crash here reads as "not current"
    /// and the engine rebuilds.
    fn apply_if_pending_is_full(&mut self) -> Result<()> {
        if !self.owns_transaction || self.pending.len() < self.pending_limit {
            return Ok(());
        }
        self.apply()?;
        self.write_header(None)?;
        self.storage.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bm25::Bm25Index;
    use crate::mem::MemStorage;
    use alloc::vec;

    fn index() -> PagedBm25Index<MemStorage> {
        let mut index = PagedBm25Index::new(MemStorage::new(), "fts");
        index.insert(1, "embedded rust database engine").unwrap();
        index.insert(2, "rust web framework").unwrap();
        index.insert(3, "cooking with cast iron").unwrap();
        index.commit().unwrap();
        index
    }

    /// The same three documents in the in-memory backend, for the comparison
    /// that is the whole point of this module.
    fn in_memory() -> Bm25Index {
        let mut index = Bm25Index::new();
        index.insert(1, "embedded rust database engine").unwrap();
        index.insert(2, "rust web framework").unwrap();
        index.insert(3, "cooking with cast iron").unwrap();
        index
    }

    #[test]
    fn ranks_the_more_specific_match_first() {
        let hits = index().search("embedded database", 10, None).unwrap();
        assert_eq!(hits[0].id, 1);
    }

    #[test]
    fn unknown_terms_match_nothing() {
        assert!(index().search("quantum", 10, None).unwrap().is_empty());
    }

    /// Scores, not rankings. Two backends that agree on the order and disagree
    /// in the last bit of a score are exactly the failure this module is
    /// written against, because fusion and the engine's own `ORDER BY` both
    /// consume the number.
    #[test]
    fn every_score_equals_the_in_memory_backend_bit_for_bit() {
        let paged = index();
        let memory = in_memory();
        for query in [
            "rust",
            "embedded database",
            "cooking iron",
            "absent",
            "rust rust",
            "embedded rust database engine web framework cooking cast iron",
        ] {
            for k in [1usize, 2, 3, 10] {
                assert_eq!(
                    paged.search(query, k, None).unwrap(),
                    memory.search(query, k, None).unwrap(),
                    "`{query}` at k={k}"
                );
            }
        }
    }

    #[test]
    fn reindexing_replaces_the_old_document() {
        let mut index = index();
        index.insert(1, "cooking").unwrap();
        index.commit().unwrap();
        let hits = index.search("embedded", 10, None).unwrap();
        assert!(hits.is_empty(), "stale postings survived: {hits:?}");

        let mut memory = in_memory();
        memory.insert(1, "cooking").unwrap();
        assert_eq!(
            index.search("cooking", 10, None).unwrap(),
            memory.search("cooking", 10, None).unwrap()
        );
    }

    #[test]
    fn removal_drops_the_document() {
        let mut index = index();
        index.remove(2).unwrap();
        index.commit().unwrap();
        assert_eq!(index.len(), 2);
        let hits = index.search("rust", 10, None).unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn emptying_the_index_leaves_nothing_that_can_be_found() {
        let mut index = index();
        for id in [1, 2, 3] {
            index.remove(id).unwrap();
        }
        index.commit().unwrap();
        assert!(index.is_empty());
        assert!(index.search("rust", 10, None).unwrap().is_empty());
        // Every term went with its last posting, so the dictionary is empty
        // too — the property `Bm25Index` gets by dropping an emptied list.
        for term in ["rust", "embedded", "cooking"] {
            assert_eq!(index.lookup_term(term).unwrap(), None, "`{term}` survived");
        }
    }

    /// A reopened index answers exactly as the one that was committed, and
    /// holds the same corpus statistics — which is the part that would go
    /// wrong silently, since `idf` and the length normalisation are computed
    /// from them.
    #[test]
    fn a_committed_index_reopens_and_scores_identically() {
        let original = index();
        let expected: Vec<Vec<Scored>> = ["rust", "embedded database", "cooking iron"]
            .iter()
            .map(|query| original.search(query, 10, None).unwrap())
            .collect();
        let live = original.len();

        let storage = original.into_storage();
        let restored = PagedBm25Index::open(storage, "fts").unwrap();
        assert_eq!(restored.len(), live);
        for (query, expected) in ["rust", "embedded database", "cooking iron"]
            .iter()
            .zip(&expected)
        {
            assert_eq!(
                &restored.search(query, 10, None).unwrap(),
                expected,
                "reopened index diverged on `{query}`"
            );
        }
    }

    #[test]
    fn searching_before_commit_returns_nothing() {
        let mut index = PagedBm25Index::new(MemStorage::new(), "fts");
        index.insert(1, "rust").unwrap();
        assert!(index.search("rust", 10, None).unwrap().is_empty());
        index.commit().unwrap();
        assert_eq!(index.search("rust", 10, None).unwrap().len(), 1);
    }

    #[test]
    fn a_filter_that_accepts_everything_returns_the_unfiltered_answer() {
        let index = index();
        for query in ["rust", "embedded database", "cooking iron", "absent"] {
            assert_eq!(
                index.search(query, 10, None).unwrap(),
                index.search(query, 10, Some(&|_| Ok(true))).unwrap(),
                "filtered path diverged for `{query}`"
            );
        }
    }

    #[test]
    fn a_rejected_document_is_skipped_without_consuming_a_slot() {
        let index = index();
        let hits = index
            .search("rust framework", 10, Some(&|id| Ok(id != 2)))
            .unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn a_failing_filter_propagates_the_error() {
        let index = index();
        let result = index.search(
            "rust",
            10,
            Some(&|_| Err(Error::Type(String::from("boom")))),
        );
        assert!(matches!(result, Err(Error::Type(message)) if message == "boom"));
    }

    /// A corpus far larger than one chunk, so the directory, the splits and
    /// the cross-chunk walk are all exercised — and every score still has to
    /// equal the in-memory backend's.
    #[test]
    fn a_corpus_spanning_many_chunks_agrees_with_the_in_memory_backend() {
        let documents = 1_500u64;
        let mut paged = PagedBm25Index::new(MemStorage::new(), "fts").with_cache_capacity(8);
        let mut memory = Bm25Index::new();
        for id in 1..=documents {
            let body = body(id);
            paged.insert(id, &body).unwrap();
            memory.insert(id, &body).unwrap();
        }
        paged.commit().unwrap();

        assert!(
            paged.cache_len() <= 8,
            "cache grew to {} entries, bound is 8",
            paged.cache_len()
        );
        for query in [
            "alpha",
            "epsilon",
            "alpha epsilon",
            "beta gamma delta",
            "alpha beta gamma delta epsilon",
            "epsilon epsilon alpha",
        ] {
            for k in [1usize, 5, 50, 1_500, 2_000] {
                assert_eq!(
                    paged.search(query, k, None).unwrap(),
                    memory.search(query, k, None).unwrap(),
                    "`{query}` at k={k}"
                );
            }
        }
    }

    /// Zipf-ish by hand: `alpha` is in nearly everything, `epsilon` is rare —
    /// the shape that makes MaxScore actually demote terms, and that makes
    /// documents tie on score constantly.
    fn body(id: u64) -> String {
        const VOCABULARY: [(u64, &str); 5] = [
            (50, "alpha"),
            (75, "beta"),
            (90, "gamma"),
            (97, "delta"),
            (100, "epsilon"),
        ];
        let mut state = id.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut roll = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut body = String::new();
        for _ in 0..3 + roll() % 12 {
            let draw = roll() % 100;
            let (_, word) = VOCABULARY
                .iter()
                .find(|(bound, _)| draw < *bound)
                .expect("the last bound is 100");
            body.push_str(word);
            body.push(' ');
        }
        body
    }

    /// Churn is where a paged postings list goes wrong: chunks split, empty,
    /// get reused and get retired. The index has to end up scoring exactly
    /// like one built fresh over the surviving rows.
    #[test]
    fn churn_converges_on_a_freshly_built_index() {
        let mut churned = PagedBm25Index::new(MemStorage::new(), "fts");
        let mut fresh = Bm25Index::new();

        for round in 0..40u64 {
            for id in 1..=60u64 {
                churned.insert(id, &body(id + round * 7)).unwrap();
            }
            for id in (1..=60u64).step_by(3) {
                churned.remove(id).unwrap();
            }
            churned.commit().unwrap();
        }
        // The state the churn is required to converge on.
        for id in 1..=60u64 {
            let body = body(id + 1_000);
            churned.insert(id, &body).unwrap();
            fresh.insert(id, &body).unwrap();
        }
        churned.commit().unwrap();

        for query in ["alpha", "alpha epsilon", "beta gamma delta"] {
            for k in [1usize, 10, 60] {
                assert_eq!(
                    churned.search(query, k, None).unwrap(),
                    fresh.search(query, k, None).unwrap(),
                    "churn changed the answer for `{query}` at k={k}"
                );
            }
        }
    }

    /// A rebuild has to leave nothing behind: no orphaned chunk, no dictionary
    /// entry, no leftover length in the corpus statistics.
    #[test]
    fn a_reset_leaves_an_index_that_rebuilds_clean() {
        let mut index = index();
        index.reset().unwrap();
        assert!(index.is_empty());
        assert_eq!(index.stored_write_version(), None);
        for namespace in [
            index.documents.clone(),
            index.dictionary.clone(),
            index.terms.clone(),
            index.postings.clone(),
        ] {
            let rows: Vec<RowId> = RowScan::new(&index.storage, &namespace)
                .map(|row| row.unwrap().0)
                .collect();
            assert!(rows.is_empty(), "`{namespace}` still holds {rows:?}");
        }

        for (id, text) in [
            (1u64, "embedded rust database engine"),
            (2, "rust web framework"),
            (3, "cooking with cast iron"),
        ] {
            index.insert(id, text).unwrap();
        }
        index.commit().unwrap();
        assert_eq!(
            index.search("rust framework", 10, None).unwrap(),
            in_memory().search("rust framework", 10, None).unwrap()
        );
    }

    /// The stamp is the whole currency check, so it may only appear on the
    /// commit that completes the index.
    #[test]
    fn the_stamp_only_lands_on_a_completed_commit() {
        let mut index = PagedBm25Index::new(MemStorage::new(), "fts");
        index.prepare_commit(7, true);
        index.insert(1, "rust").unwrap();
        assert_eq!(index.stored_write_version(), None, "stamped before commit");
        index.commit().unwrap();
        assert_eq!(index.stored_write_version(), Some(7));

        let storage = index.into_storage();
        let reopened = PagedBm25Index::open(storage, "fts").unwrap();
        assert_eq!(reopened.stored_write_version(), Some(7));
    }

    /// A header this build cannot read is purged rather than half-trusted, so
    /// that it looks exactly like "nothing saved" to the caller.
    #[test]
    fn an_unreadable_header_purges_rather_than_misreads() {
        let index = index();
        let header_key = index.header_key();
        let mut storage = index.into_storage();
        storage
            .put_meta(&header_key, &[0xff, 0xff, 0xff, 0xff])
            .unwrap();
        storage.commit().unwrap();

        let reopened = PagedBm25Index::open(storage, "fts").unwrap();
        assert!(reopened.is_empty());
        assert_eq!(reopened.stored_write_version(), None);
        assert!(reopened.search("rust", 10, None).unwrap().is_empty());
        // And the rows went with it, so a rebuild on top does not find a
        // dictionary it did not write.
        let rows: Vec<RowId> = RowScan::new(&reopened.storage, &reopened.documents)
            .map(|row| row.unwrap().0)
            .collect();
        assert!(rows.is_empty(), "purge left documents behind: {rows:?}");
    }

    /// A document with no terms at all is still a document: it counts toward
    /// `doc_count` and contributes a zero to `total_length`, both of which
    /// move every other document's score.
    #[test]
    fn an_empty_document_still_counts_toward_the_corpus_statistics() {
        let mut paged = PagedBm25Index::new(MemStorage::new(), "fts");
        let mut memory = Bm25Index::new();
        for (id, text) in [(1u64, "rust database"), (2, ""), (3, "rust")] {
            paged.insert(id, text).unwrap();
            memory.insert(id, text).unwrap();
        }
        paged.commit().unwrap();
        assert_eq!(paged.len(), memory.len());
        assert_eq!(
            paged.search("rust", 10, None).unwrap(),
            memory.search("rust", 10, None).unwrap()
        );
    }

    /// Two handles on one database hold two `PagedBm25Index` instances over
    /// the *same* namespaces, so one of them rebuilding — which is what every
    /// handle does when it opens on a stamp that is not current — rewrites the
    /// document records and reassigns every term ordinal underneath the other.
    /// No row changed, so nothing moved the write version the engine watches
    /// on the second handle's behalf.
    ///
    /// Two things went wrong before `adopt_stored_statistics` existed, and the
    /// quiet one is the worse one: the retire step underflowed `live` (loud,
    /// and how this was found), and the term-ordinal counter was handed out
    /// twice, so each instance read the other's postings under an ordinal that
    /// meant a different word (silent, and a wrong answer with no error
    /// anywhere).
    #[test]
    fn a_rebuild_by_another_handle_is_adopted_rather_than_overwritten() {
        let storage = crate::shared::SharedStorage::new(alloc::boxed::Box::new(MemStorage::new()));

        // The handle that will be left behind.
        let mut first = PagedBm25Index::new(storage.clone(), "fts");
        for id in 1..=30u64 {
            first.insert(id, &body(id)).unwrap();
        }
        first.commit().unwrap();

        // The handle that decides the saved index is not current and rebuilds
        // it from the rows, over *different* text, so the dictionary and every
        // ordinal in it are reassigned.
        let mut second = PagedBm25Index::open(storage.clone(), "fts").unwrap();
        second.reset().unwrap();
        for id in 1..=30u64 {
            second.insert(id, &body(id + 500)).unwrap();
        }
        second.commit().unwrap();

        // The first handle now does what it was going to do anyway. It must
        // build on what it finds rather than on what it remembers.
        first.insert(31, "alpha beta gamma").unwrap();
        first.commit().unwrap();

        let mut oracle = Bm25Index::new();
        for id in 1..=30u64 {
            oracle.insert(id, &body(id + 500)).unwrap();
        }
        oracle.insert(31, "alpha beta gamma").unwrap();

        assert_eq!(first.len(), oracle.len(), "live count diverged");
        for query in ["alpha", "alpha epsilon", "beta gamma delta", "gamma"] {
            assert_eq!(
                first.search(query, 10, None).unwrap(),
                oracle.search(query, 10, None).unwrap(),
                "the left-behind handle answered `{query}` from a stale view"
            );
            // And the handle that did the rebuild agrees with it, which is
            // what rules out the two having diverged into private views of
            // one shared structure.
            assert_eq!(
                second.search(query, 10, None).unwrap(),
                oracle.search(query, 10, None).unwrap(),
                "the rebuilding handle answered `{query}` from a stale view"
            );
        }
    }

    /// An insert and a remove of the same row inside one batch must collapse
    /// to the last of them, not apply twice.
    #[test]
    fn the_last_operation_on_a_row_in_a_batch_wins() {
        let mut paged = PagedBm25Index::new(MemStorage::new(), "fts");
        paged.insert(1, "rust database").unwrap();
        paged.insert(1, "cooking iron").unwrap();
        paged.insert(2, "rust web").unwrap();
        paged.remove(2).unwrap();
        paged.commit().unwrap();

        let mut memory = Bm25Index::new();
        memory.insert(1, "cooking iron").unwrap();
        assert_eq!(paged.len(), 1);
        for query in ["rust", "cooking", "iron"] {
            assert_eq!(
                paged.search(query, 10, None).unwrap(),
                memory.search(query, 10, None).unwrap(),
                "`{query}`"
            );
        }
    }
}
