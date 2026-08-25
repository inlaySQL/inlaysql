//! An in-engine BM25 full-text index.
//!
//! Stage 4 moves retrieval out of borrowed crates and into the engine. This is
//! the full-text half: it replaces the `tantivy`-backed index in the production
//! crate with one that lives in `inlaysql-core`, so the engine owns its BM25
//! scoring end to end. It is a plain, deterministic Okapi BM25 over postings
//! lists — no stemming, no stop words, no language model — which is exactly the
//! predictable behaviour the deterministic simulation tests need.
//!
//! # Why the shape below, and not a map of maps
//!
//! The first version of this index was `BTreeMap<String, BTreeMap<RowId, u32>>`
//! plus `BTreeMap<RowId, u32>` of lengths, which reads well and costs three
//! tree descents per posting scored: one to walk the postings, one to fetch the
//! document's length, one to reach the score accumulator. Scoring is the inner
//! loop of every text and hybrid query, so those descents *were* the query.
//!
//! What replaces it is the ordinary inverted-index layout: documents get dense
//! ordinals, so a length or a row id is an array index rather than a lookup;
//! postings for a term are one contiguous `Vec`, so walking them is a linear
//! scan; and scoring is document-at-a-time, so there is no accumulator to
//! probe at all — a document's score is finished before the next one starts.
//!
//! That layout is also what makes the query stop doing work it cannot use. A
//! `LIMIT 10` over a term every document mentions still had to score every
//! document and then sort the whole corpus to keep ten of them. Two changes
//! remove both halves: the answer is held in a bounded heap of the best `k`
//! rather than a `Vec` of everything, and the walk is a MaxScore one — the
//! terms that could contribute least stop driving it once their combined
//! ceiling falls below the `k`-th best score, so documents only those terms
//! mention are never visited at all. The ceilings come from [`Impact`], which
//! is why the index tracks each term's largest frequency and smallest
//! document.
//!
//! Three invariants make this safe to swap in, and all three are pinned by
//! tests:
//!
//! - **The on-disk format does not change.** [`Bm25Index::encode`] still emits
//!   terms in sorted order and postings in row-id order, so a file written
//!   before this change reads back identically and a file written after it is
//!   byte-for-byte what the old code would have written. Determinism holds:
//!   the bytes depend on the index's contents, never on the order it was
//!   built in.
//! - **The scores do not change, bit for bit.** A document's contributions are
//!   summed in query-term order here exactly as they were when the accumulator
//!   held them, and floating-point addition is order-dependent — so this is a
//!   property to preserve deliberately, not a coincidence to hope for. It is
//!   also why the MaxScore walk never abandons a document part-way through
//!   scoring it, which is the usual last trick of the algorithm: a document
//!   that reaches the answer is summed the same way it always was, and only
//!   whole documents are skipped.
//! - **The answer does not change, ties included.** Skipping is only ever
//!   applied to documents whose *entire* possible score is strictly below the
//!   `k`-th best already held. Strictly, because ranking breaks a tie by the
//!   lower row id: a document that merely equalled the `k`-th best could still
//!   displace it, so "cannot beat" is not a safe test and "cannot reach" is.

use alloc::collections::{BTreeMap, BinaryHeap};
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::error::{Error, Result};
use crate::fusion::sort_by_score_desc;
use crate::row::{put_len, put_string, Cursor};
use crate::traits::{FullTextIndex, RowFilter, RowId, Scored};

/// Term-frequency saturation. The Robertson/Zaragoza default.
const K1: f32 = 1.2;
/// Document-length normalisation strength. The Robertson/Zaragoza default.
const B: f32 = 0.75;
/// On-disk format of the persisted index. Bumped whenever the layout changes;
/// a mismatch makes the engine rebuild rather than misread.
const FORMAT_VERSION: u8 = 1;

// ---------------------------------------------------------------- the scalars
//
// The four arithmetic steps of Okapi BM25, written once and called from both
// backends. `crate::bm25_paged` has to return *byte-identical* `Vec<Scored>`,
// and floating-point arithmetic is not associative, so "the same formula" is
// not enough — a second transcription that grouped one multiplication
// differently would produce scores that are equal to a printed decimal and
// unequal as bits, and would silently rerank hits whose scores differ in the
// last place. Sharing the expressions is what makes the agreement structural
// instead of a coincidence that holds until somebody edits one copy.

/// The average document length a query normalises against.
///
/// Corpus-relative, which is why a paged backend has to track exactly the same
/// `live` count and `total_length` sum the in-memory one does: an average that
/// differs in the last bit changes every score in the answer.
pub(crate) fn average_length(total_length: u64, live: usize) -> f32 {
    if live == 0 {
        return 0.0;
    }
    total_length as f32 / live as f32
}

/// Inverse document frequency of a term appearing in `document_frequency` of
/// `doc_count` documents.
pub(crate) fn idf(doc_count: usize, document_frequency: usize) -> f32 {
    let document_frequency = document_frequency as f32;
    libm::logf(1.0 + (doc_count as f32 - document_frequency + 0.5) / (document_frequency + 0.5))
}

/// The length-normalisation denominator term for one document.
pub(crate) fn length_normalisation(length: u32, average_length: f32) -> f32 {
    K1 * (1.0 - B + B * length as f32 / average_length.max(f32::EPSILON))
}

/// One query term's contribution to one document's score.
pub(crate) fn contribution(idf: f32, frequency: u32, normalisation: f32) -> f32 {
    let frequency = frequency as f32;
    idf * (frequency * (K1 + 1.0)) / (frequency + normalisation)
}

/// Split text into lowercase alphanumeric terms.
///
/// Deliberately crude: no stemming, no stop words. It only has to be
/// predictable, so that two builds over the same rows agree exactly.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

/// One document's entry in a term's postings list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Posting {
    /// The document's dense ordinal, not its row id — this is what makes the
    /// list mergeable by integer comparison and the length lookup an index.
    doc: u32,
    /// How many times the term occurs in that document.
    frequency: u32,
}

/// The extremes of one term's postings, for bounding its contribution.
///
/// Shared with [`crate::bm25_paged`], which keeps the same pair per term in
/// its on-disk term record: a bound computed a different way would prune a
/// different set of documents, and the whole point of the paged backend is
/// that it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Impact {
    pub(crate) max_frequency: u32,
    pub(crate) min_length: u32,
}

impl Default for Impact {
    fn default() -> Self {
        // The identity for widening: any posting lowers the length and raises
        // the frequency.
        Self {
            max_frequency: 0,
            min_length: u32::MAX,
        }
    }
}

impl Impact {
    pub(crate) fn widen(&mut self, frequency: u32, length: u32) {
        self.max_frequency = self.max_frequency.max(frequency);
        self.min_length = self.min_length.min(length);
    }

    /// The largest score a document in this term's postings could take.
    pub(crate) fn ceiling(&self, idf: f32, average_length: f32) -> f32 {
        if self.max_frequency == 0 {
            return 0.0;
        }
        let length = if self.min_length == u32::MAX {
            0
        } else {
            self.min_length
        };
        contribution(
            idf,
            self.max_frequency,
            length_normalisation(length, average_length),
        )
    }
}

/// One query term's walk over its postings.
///
/// There is one of these per *occurrence* of a term in the query rather than
/// per distinct term: a query that repeats a term scores it twice, which is
/// what the accumulator did and so what every published BM25 number here
/// already means.
struct TermWalk<'a> {
    postings: &'a [Posting],
    idf: f32,
    /// The largest contribution any document in `postings` could take. This is
    /// what MaxScore orders and partitions the terms on.
    ceiling: f32,
    /// How far into `postings` the walk has reached. It only ever moves
    /// forward, because the document under consideration only ever does.
    position: usize,
}

impl TermWalk<'_> {
    /// The document this cursor is parked on, if it has any left.
    fn current(&self) -> Option<u32> {
        self.postings.get(self.position).map(|posting| posting.doc)
    }

    /// Advance to `doc` and report the term's frequency there, or `None` if
    /// this term does not occur in it. Either way the cursor ends up past
    /// every document before the next candidate.
    ///
    /// The search gallops rather than stepping because the distance varies by
    /// orders of magnitude: a term driving the walk is at most one posting
    /// behind, while one MaxScore has demoted may be thousands behind, having
    /// been consulted for nothing in between. Doubling reaches either in
    /// `O(log distance)` instead of paying for the gap.
    fn seek(&mut self, doc: u32) -> Option<u32> {
        let remaining = &self.postings[self.position..];
        let mut window = 1;
        while window < remaining.len() && remaining[window - 1].doc < doc {
            window *= 2;
        }
        let window = &remaining[..window.min(remaining.len())];
        self.position += window.partition_point(|posting| posting.doc < doc);

        let posting = self.postings.get(self.position)?;
        if posting.doc != doc {
            return None;
        }
        self.position += 1;
        Some(posting.frequency)
    }
}

/// A hit ordered weakest-first, so that a [`BinaryHeap`]'s maximum is the hit
/// to drop next.
///
/// The comparison is exactly [`sort_by_score_desc`]'s, reversed — score
/// descending, row id ascending on a tie — so the `k` this keeps are the `k` a
/// full sort of everything would have put in front.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Weakest(pub(crate) Scored);

impl Eq for Weakest {}

impl Ord for Weakest {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .score
            .partial_cmp(&self.0.score)
            .unwrap_or(Ordering::Equal)
            .then(self.0.id.cmp(&other.0.id))
    }
}

impl PartialOrd for Weakest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The best `k` hits seen so far.
///
/// This exists for the threshold as much as for the bound on memory: MaxScore
/// can only skip a document once it knows what score it has to beat, and that
/// score is this heap's weakest entry.
pub(crate) struct TopK {
    k: usize,
    hits: BinaryHeap<Weakest>,
}

impl TopK {
    pub(crate) fn new(k: usize) -> Self {
        // Not `with_capacity(k)`: `k` comes from a `LIMIT` and may be any
        // number a user can type, while the heap only ever needs room for as
        // many documents as actually match.
        Self {
            k,
            hits: BinaryHeap::new(),
        }
    }

    pub(crate) fn offer(&mut self, hit: Scored) {
        if self.hits.len() < self.k {
            self.hits.push(Weakest(hit));
            return;
        }
        let Some(weakest) = self.hits.peek() else {
            return;
        };
        if Weakest(hit) < *weakest {
            self.hits.pop();
            self.hits.push(Weakest(hit));
        }
    }

    /// The score a document now has to beat, or `None` while there is still
    /// room — until `k` hits are held, nothing can be ruled out.
    pub(crate) fn threshold(&self) -> Option<f32> {
        if self.hits.len() < self.k {
            return None;
        }
        self.hits.peek().map(|weakest| weakest.0.score)
    }

    pub(crate) fn into_ranked(self) -> Vec<Scored> {
        let mut hits: Vec<Scored> = self.hits.into_iter().map(|weakest| weakest.0).collect();
        sort_by_score_desc(&mut hits);
        hits
    }
}

/// An Okapi BM25 index over postings lists.
#[derive(Debug, Default, Clone)]
pub struct Bm25Index {
    /// term -> term ordinal, sorted by term so that [`Bm25Index::encode`] is
    /// canonical without sorting anything.
    terms: BTreeMap<String, u32>,
    /// term ordinal -> postings, ascending by document ordinal.
    postings: Vec<Vec<Posting>>,
    /// term ordinal -> (largest term frequency, smallest document length) over
    /// that term's postings.
    ///
    /// These are what let [`FullTextIndex::search`] bound a term's largest
    /// possible contribution without reading its postings, which is what makes
    /// skipping possible. BM25 rises with term frequency and falls with
    /// document length, so the pair gives an upper bound even though no single
    /// document need hold both extremes.
    ///
    /// They are only ever *widened*. A removal can leave a bound looser than
    /// the postings now justify, and that is deliberate: a loose bound prunes
    /// less and stays correct, where a tightened-too-far one would silently
    /// drop results. Tightness is restored on the next [`Bm25Index::decode`],
    /// which recomputes them exactly.
    impacts: Vec<Impact>,
    /// Term ordinals whose list went empty and can be handed out again.
    free_terms: Vec<u32>,

    /// document ordinal -> row id.
    ids: Vec<RowId>,
    /// row id -> document ordinal, sorted by row id for a canonical encoding.
    ordinals: BTreeMap<RowId, u32>,
    /// document ordinal -> length in terms.
    lengths: Vec<u32>,
    /// document ordinal -> the terms it contains.
    ///
    /// Removal exists to be cheap: without this, dropping one document means
    /// walking every posting list in the index looking for it, which made
    /// re-indexing a document quadratic in the corpus — and re-indexing is
    /// what every `UPDATE` does.
    doc_terms: Vec<Vec<u32>>,
    /// Document ordinals freed by removal, ready to be handed out again.
    free_docs: Vec<u32>,

    /// Live document count: what `doc_count` means when scoring.
    live: usize,
    /// Sum of every live document's length, so the average is O(1) rather
    /// than a walk of the whole corpus on every single query.
    total_length: u64,
}

impl Bm25Index {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of indexed documents.
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether the index holds no documents.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    fn average_length(&self) -> f32 {
        average_length(self.total_length, self.live)
    }

    /// The ordinal for `id`, allocating one if this is a new document.
    fn ordinal_for(&mut self, id: RowId) -> u32 {
        if let Some(ordinal) = self.ordinals.get(&id) {
            return *ordinal;
        }
        let ordinal = match self.free_docs.pop() {
            Some(reused) => {
                self.ids[reused as usize] = id;
                self.lengths[reused as usize] = 0;
                self.doc_terms[reused as usize].clear();
                reused
            }
            None => {
                self.ids.push(id);
                self.lengths.push(0);
                self.doc_terms.push(Vec::new());
                (self.ids.len() - 1) as u32
            }
        };
        self.ordinals.insert(id, ordinal);
        ordinal
    }

    /// The ordinal for `term`, allocating one if this is a new term.
    fn term_for(&mut self, term: &str) -> u32 {
        if let Some(ordinal) = self.terms.get(term) {
            return *ordinal;
        }
        let ordinal = match self.free_terms.pop() {
            Some(reused) => {
                self.postings[reused as usize].clear();
                self.impacts[reused as usize] = Impact::default();
                reused
            }
            None => {
                self.postings.push(Vec::new());
                self.impacts.push(Impact::default());
                (self.postings.len() - 1) as u32
            }
        };
        self.terms.insert(String::from(term), ordinal);
        ordinal
    }

    /// Serialise the index.
    ///
    /// ```text
    /// index    := u8 version, u32 term_count, term*, u32 doc_count, doc*
    /// term     := string term, u32 posting_count, posting*
    /// posting  := u64 row id, u32 term frequency
    /// doc      := u64 row id, u32 length in terms
    /// ```
    ///
    /// The postings are what make this worth storing: rebuilding them means
    /// re-reading and re-tokenising every document in the table.
    ///
    /// Terms come out in sorted order and postings in row-id order, neither of
    /// which is how they are held in memory. That is deliberate: the bytes
    /// have to be a function of the index's *contents* alone, or two engines
    /// that indexed the same rows in different orders would write different
    /// files and the determinism sweep would be measuring insertion order.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(FORMAT_VERSION);
        put_len(&mut out, self.terms.len());
        let mut row_ordered: Vec<(RowId, u32)> = Vec::new();
        for (term, ordinal) in &self.terms {
            put_string(&mut out, term);
            let postings = &self.postings[*ordinal as usize];
            put_len(&mut out, postings.len());
            row_ordered.clear();
            row_ordered.extend(
                postings
                    .iter()
                    .map(|posting| (self.ids[posting.doc as usize], posting.frequency)),
            );
            row_ordered.sort_unstable_by_key(|(id, _)| *id);
            for (id, frequency) in &row_ordered {
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&frequency.to_le_bytes());
            }
        }
        put_len(&mut out, self.live);
        for (id, ordinal) in &self.ordinals {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&self.lengths[*ordinal as usize].to_le_bytes());
        }
        out
    }

    /// Parse bytes produced by [`Bm25Index::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let version = cursor.u8()?;
        if version != FORMAT_VERSION {
            return Err(Error::Corrupt(alloc::format!(
                "BM25 index format version {version} is not {FORMAT_VERSION}"
            )));
        }

        let mut index = Self::new();
        let term_count = cursor.count(8)?;
        for _ in 0..term_count {
            let term = cursor.string()?;
            let posting_count = cursor.count(12)?;
            let term_ordinal = index.term_for(&term);
            for _ in 0..posting_count {
                let id = RowId::from_le_bytes(cursor.array8()?);
                let frequency = u32::from_le_bytes(cursor.array4()?);
                let doc = index.ordinal_for(id);
                index.postings[term_ordinal as usize].push(Posting { doc, frequency });
                index.doc_terms[doc as usize].push(term_ordinal);
            }
            // Postings arrive in row-id order, and ordinals are handed out as
            // documents are first seen, so the two orders only coincide for
            // the first term. Scoring merges on the ordinal, so sort on it.
            index.postings[term_ordinal as usize].sort_unstable_by_key(|posting| posting.doc);
        }

        let doc_count = cursor.count(12)?;
        for _ in 0..doc_count {
            let id = RowId::from_le_bytes(cursor.array8()?);
            let length = u32::from_le_bytes(cursor.array4()?);
            let ordinal = index.ordinal_for(id);
            index.lengths[ordinal as usize] = length;
            index.live += 1;
            index.total_length += u64::from(length);
        }

        // Lengths arrive after the postings, so the impact bounds could not be
        // widened as the postings were read. Computing them here rather than
        // guessing is also what makes a decoded index's bounds exactly tight.
        for (postings, impact) in index.postings.iter().zip(&mut index.impacts) {
            for posting in postings {
                impact.widen(posting.frequency, index.lengths[posting.doc as usize]);
            }
        }
        Ok(index)
    }
}

impl FullTextIndex for Bm25Index {
    fn insert(&mut self, id: RowId, text: &str) -> Result<()> {
        self.remove(id)?;
        let tokens = tokenize(text);

        // Count first, then write one posting per distinct term, rather than
        // hunting the postings list once per occurrence.
        let mut frequencies: BTreeMap<u32, u32> = BTreeMap::new();
        for token in &tokens {
            let term = self.term_for(token);
            *frequencies.entry(term).or_insert(0) += 1;
        }

        let doc = self.ordinal_for(id);
        self.lengths[doc as usize] = tokens.len() as u32;
        self.live += 1;
        self.total_length += tokens.len() as u64;

        for (term, frequency) in frequencies {
            self.impacts[term as usize].widen(frequency, tokens.len() as u32);
            let postings = &mut self.postings[term as usize];
            let posting = Posting { doc, frequency };
            // Ordinals ascend for an append-only load, so this is a push in
            // the common case and a memmove only when a freed ordinal was
            // handed back out.
            match postings.binary_search_by_key(&doc, |posting| posting.doc) {
                Ok(at) => postings[at] = posting,
                Err(at) => postings.insert(at, posting),
            }
            self.doc_terms[doc as usize].push(term);
        }
        Ok(())
    }

    fn remove(&mut self, id: RowId) -> Result<()> {
        let Some(doc) = self.ordinals.remove(&id) else {
            return Ok(());
        };
        for term in core::mem::take(&mut self.doc_terms[doc as usize]) {
            let postings = &mut self.postings[term as usize];
            if let Ok(at) = postings.binary_search_by_key(&doc, |posting| posting.doc) {
                postings.remove(at);
            }
            // A term nothing mentions any more leaves the index entirely, so
            // that it is absent from the encoding and from every document
            // frequency — which is what the map-of-maps did by dropping an
            // emptied inner map.
            if postings.is_empty() {
                self.terms.retain(|_, ordinal| *ordinal != term);
                self.free_terms.push(term);
            }
        }
        self.live -= 1;
        self.total_length -= u64::from(self.lengths[doc as usize]);
        self.lengths[doc as usize] = 0;
        self.free_docs.push(doc);
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    fn save(&self) -> Option<Vec<u8>> {
        Some(self.encode())
    }

    fn load(&mut self, bytes: &[u8]) -> Result<()> {
        *self = Self::decode(bytes)?;
        Ok(())
    }

    fn search(&self, query: &str, k: usize, filter: Option<&RowFilter>) -> Result<Vec<Scored>> {
        let doc_count = self.live;
        if doc_count == 0 || k == 0 {
            return Ok(Vec::new());
        }
        let average_length = self.average_length();

        let mut cursors: Vec<TermWalk> = Vec::new();
        for term in tokenize(query) {
            let Some(ordinal) = self.terms.get(&term) else {
                continue;
            };
            let postings = &self.postings[*ordinal as usize];
            let idf = idf(doc_count, postings.len());
            cursors.push(TermWalk {
                postings: postings.as_slice(),
                idf,
                ceiling: self.impacts[*ordinal as usize].ceiling(idf, average_length),
                position: 0,
            });
        }
        if cursors.is_empty() {
            return Ok(Vec::new());
        }

        // MaxScore's ordering: cheapest term first, and the running total of
        // what the terms up to each point could contribute. Once that total
        // falls below the k-th best score, a document only those terms mention
        // cannot reach the answer, so they stop driving the walk and are read
        // only for documents the dearer terms turn up.
        //
        // The total is summed in `f64` so that rounding can only ever make the
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

        // How many of the leading terms in `order` no longer drive the walk.
        let mut demoted = 0usize;
        let mut best = TopK::new(k);
        loop {
            // The next document any driving cursor still has. Query terms are
            // few, so a linear minimum beats maintaining a heap over them.
            let mut next = u32::MAX;
            for term in &order[demoted..] {
                if let Some(doc) = cursors[*term].current() {
                    next = next.min(doc);
                }
            }
            if next == u32::MAX {
                break;
            }

            let id = self.ids[next as usize];
            // A document the filter rejects is skipped without consuming a
            // result slot — there is no graph here to keep connected, so
            // "filtered" is exactly this: keep walking the postings, only
            // score what the filter admits. A rejected document also never
            // raises the threshold, so a selective filter costs the walk its
            // skipping and never its correctness: it changes the answer's
            // size, and can never return a partial probe.
            let admitted = match filter {
                Some(filter) => filter(id)?,
                None => true,
            };

            let normalisation = length_normalisation(self.lengths[next as usize], average_length);
            let mut score = 0.0f32;
            // Query order, not `order`: floating-point addition is
            // order-dependent and the published score is the query-order sum.
            // Every cursor is seeked whether or not the document is admitted,
            // because the walk past it is what makes the next one reachable.
            for cursor in &mut cursors {
                if let Some(frequency) = cursor.seek(next) {
                    if admitted {
                        score += contribution(cursor.idf, frequency, normalisation);
                    }
                }
            }
            if !admitted {
                continue;
            }

            best.offer(Scored::new(id, score));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    fn index() -> Bm25Index {
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
    fn rare_terms_outweigh_common_ones() {
        // "rust" appears in two documents, "framework" in one.
        let hits = index().search("rust framework", 10, None).unwrap();
        assert_eq!(hits[0].id, 2);
    }

    #[test]
    fn unknown_terms_match_nothing() {
        assert!(index().search("quantum", 10, None).unwrap().is_empty());
    }

    #[test]
    fn reindexing_replaces_the_old_document() {
        let mut index = index();
        index.insert(1, "cooking").unwrap();
        let hits = index.search("embedded", 10, None).unwrap();
        assert!(hits.is_empty(), "stale postings survived: {hits:?}");
    }

    #[test]
    fn removal_drops_the_document() {
        let mut index = index();
        index.remove(2).unwrap();
        let hits = index.search("rust", 10, None).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            alloc::vec![1]
        );
    }

    #[test]
    fn results_respect_k() {
        assert_eq!(index().search("rust", 1, None).unwrap().len(), 1);
    }

    #[test]
    fn a_restored_index_scores_identically() {
        let original = index();
        let mut restored = Bm25Index::new();
        restored.load(&original.save().unwrap()).unwrap();

        for query in ["rust", "embedded database", "cooking iron", "absent"] {
            assert_eq!(
                original.search(query, 10, None).unwrap(),
                restored.search(query, 10, None).unwrap(),
                "scores diverged for `{query}`"
            );
        }
    }

    #[test]
    fn an_empty_index_round_trips() {
        let mut restored = Bm25Index::new();
        restored.load(&Bm25Index::new().save().unwrap()).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn a_truncated_encoding_is_rejected_not_panicked() {
        let bytes = index().encode();
        for cut in [0, 1, 5, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                Bm25Index::decode(&bytes[..cut]).is_err(),
                "a {cut}-byte prefix decoded as a whole index"
            );
        }
    }

    #[test]
    fn a_future_format_version_is_refused() {
        let mut bytes = index().encode();
        bytes[0] = FORMAT_VERSION + 1;
        assert!(matches!(
            Bm25Index::decode(&bytes),
            Err(crate::error::Error::Corrupt(_))
        ));
    }

    // ------------------------------------------------- representation invariants

    /// The encoding may not depend on the order documents were inserted in —
    /// the determinism sweep rests on this, and the in-memory layout now holds
    /// documents in insertion order rather than row-id order, so it is no
    /// longer true for free.
    #[test]
    fn the_encoding_is_independent_of_insertion_order() {
        let mut forwards = Bm25Index::new();
        for (id, text) in [
            (1, "embedded rust database engine"),
            (2, "rust web framework"),
            (3, "cooking with cast iron"),
        ] {
            forwards.insert(id, text).unwrap();
        }

        let mut backwards = Bm25Index::new();
        for (id, text) in [
            (3, "cooking with cast iron"),
            (2, "rust web framework"),
            (1, "embedded rust database engine"),
        ] {
            backwards.insert(id, text).unwrap();
        }

        assert_eq!(
            forwards.encode(),
            backwards.encode(),
            "the bytes depend on insertion order"
        );
    }

    /// A save/load round trip is a fixed point, which is what lets a rebuilt
    /// index and a restored one be compared byte for byte.
    #[test]
    fn a_round_trip_reaches_a_fixed_point() {
        let original = index();
        let restored = Bm25Index::decode(&original.encode()).unwrap();
        assert_eq!(original.encode(), restored.encode());
        assert_eq!(original.len(), restored.len());
    }

    /// Removing every document must leave an index that encodes as an empty
    /// one — no orphaned terms, no lingering length total.
    #[test]
    fn emptying_the_index_leaves_nothing_behind() {
        let mut index = index();
        for id in [1, 2, 3] {
            index.remove(id).unwrap();
        }
        assert!(index.is_empty());
        assert_eq!(index.encode(), Bm25Index::new().encode());
        assert_eq!(index.average_length(), 0.0);
    }

    /// Churn must not leak ordinals or corrupt the postings: the same document
    /// inserted, removed and reinserted many times has to end up scoring like
    /// a freshly built index holding the same rows.
    #[test]
    fn churn_converges_on_a_freshly_built_index() {
        let mut churned = Bm25Index::new();
        for round in 0..50 {
            churned.insert(1, "embedded rust database engine").unwrap();
            churned.insert(2, "rust web framework").unwrap();
            if round % 2 == 0 {
                churned.remove(1).unwrap();
            }
            churned.remove(2).unwrap();
        }
        churned.insert(1, "embedded rust database engine").unwrap();
        churned.insert(2, "rust web framework").unwrap();
        churned.insert(3, "cooking with cast iron").unwrap();

        let fresh = index();
        assert_eq!(churned.encode(), fresh.encode());
        for query in ["rust", "embedded database", "cooking iron"] {
            assert_eq!(
                churned.search(query, 10, None).unwrap(),
                fresh.search(query, 10, None).unwrap(),
                "churn changed the scores for `{query}`"
            );
        }
    }

    /// A repeated query term is scored once per occurrence. This looks like a
    /// quirk and is load-bearing: it is what the accumulator did, so it is
    /// what every published BM25 number already means.
    #[test]
    fn a_repeated_query_term_counts_twice() {
        let index = index();
        let once = index.search("rust", 10, None).unwrap();
        let twice = index.search("rust rust", 10, None).unwrap();
        assert_eq!(once.len(), twice.len());
        for (single, double) in once.iter().zip(&twice) {
            assert_eq!(single.id, double.id);
            assert_eq!(double.score, single.score * 2.0, "row {}", single.id);
        }
    }

    // ------------------------------------------------------------ skipping

    /// A corpus with the two properties that make skipping interesting and
    /// make it risky: term frequencies spread far enough apart that MaxScore
    /// really does demote the common terms, and a vocabulary small enough that
    /// documents tie on score constantly — a tie being exactly what a bound
    /// compared the wrong way round would quietly drop.
    fn skewed_corpus() -> Bm25Index {
        // Zipf-ish by hand: `alpha` is in nearly everything, `epsilon` is rare.
        const VOCABULARY: [(u64, &str); 5] = [
            (50, "alpha"),
            (75, "beta"),
            (90, "gamma"),
            (97, "delta"),
            (100, "epsilon"),
        ];
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let mut roll = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut index = Bm25Index::new();
        for id in 1..=600 {
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
            index.insert(id, &body).unwrap();
        }
        index
    }

    /// `usize::MAX` leaves the heap of best hits forever unfilled, so there is
    /// never a threshold, so nothing is ever demoted: the exhaustive answer
    /// this index used to compute for every query, whatever the `LIMIT`.
    fn exhaustive(index: &Bm25Index, query: &str, filter: Option<&RowFilter>) -> Vec<Scored> {
        index.search(query, usize::MAX, filter).unwrap()
    }

    /// The point of the whole exercise: skipping may not change the answer.
    #[test]
    fn a_pruned_walk_answers_exactly_as_an_exhaustive_one() {
        let index = skewed_corpus();
        for query in [
            "alpha",
            "epsilon",
            "alpha epsilon",
            "beta gamma delta",
            "alpha beta gamma delta epsilon",
            "epsilon epsilon alpha",
        ] {
            let full = exhaustive(&index, query, None);
            for k in [1, 2, 10, 50, 599, 600, 601] {
                let pruned = index.search(query, k, None).unwrap();
                assert_eq!(
                    pruned,
                    full[..k.min(full.len())],
                    "query `{query}` at k={k}"
                );
            }
        }
    }

    /// A filter lowers the threshold — rejected documents never raise it — so
    /// it prunes less. It must not prune differently.
    #[test]
    fn a_pruned_walk_answers_exactly_as_an_exhaustive_one_under_a_filter() {
        let index = skewed_corpus();
        let selective: &RowFilter = &|id| Ok(id % 7 == 0);
        for query in ["alpha epsilon", "beta gamma delta"] {
            let full = exhaustive(&index, query, Some(selective));
            for k in [1, 5, 10] {
                let pruned = index.search(query, k, Some(selective)).unwrap();
                assert_eq!(
                    pruned,
                    full[..k.min(full.len())],
                    "query `{query}` at k={k}"
                );
            }
        }
    }

    /// And it has to actually skip, or the two tests above pass by measuring
    /// nothing. The filter is the only view into the walk from outside: it is
    /// called once per document the walk considers, so counting the calls
    /// counts the documents visited.
    #[test]
    fn a_small_k_visits_fewer_documents_than_the_whole_corpus() {
        let index = skewed_corpus();
        let visited = Cell::new(0usize);
        let counting: &RowFilter = &|_| {
            visited.set(visited.get() + 1);
            Ok(true)
        };

        index.search("alpha epsilon", 10, Some(counting)).unwrap();
        let pruned = visited.get();

        visited.set(0);
        exhaustive(&index, "alpha epsilon", Some(counting));
        let full = visited.get();

        assert!(
            pruned < full,
            "a LIMIT 10 visited {pruned} documents and an exhaustive walk {full} — nothing was skipped"
        );
    }

    // --------------------------------------------------------- filtered search

    #[test]
    fn a_filter_that_accepts_everything_returns_the_unfiltered_answer() {
        // The tie to the unfiltered path: same rows, same order, same scores.
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
        // Doc 2 ranks first for "rust framework"; rejecting it must promote
        // doc 1 into the result rather than leave a hole the k budget ate.
        let index = index();
        let hits = index
            .search("rust framework", 10, Some(&|id| Ok(id != 2)))
            .unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            alloc::vec![1]
        );
        assert_eq!(hits.len(), 1, "the rejected doc's slot must not stay empty");
    }

    #[test]
    fn a_filter_that_rejects_everything_returns_nothing() {
        let index = index();
        let hits = index.search("rust", 10, Some(&|_| Ok(false))).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn a_filter_admitting_one_document_finds_it_wherever_it_ranks() {
        // The worst-ranked match for "rust" is doc 2 (it shares the term with
        // doc 1 but nothing else) — and it must still be found, because the
        // postings scan is exhaustive regardless of the filter.
        let index = index();
        let hits = index.search("rust", 10, Some(&|id| Ok(id == 2))).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            alloc::vec![2]
        );
    }

    #[test]
    fn a_failing_filter_propagates_the_error() {
        let index = index();
        let result = index.search(
            "rust",
            10,
            Some(&|_| Err(Error::Type(alloc::string::String::from("boom")))),
        );
        assert!(matches!(result, Err(Error::Type(message)) if message == "boom"));
    }
}
