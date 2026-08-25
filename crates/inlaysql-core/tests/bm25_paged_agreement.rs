//! The paged BM25 index must answer *identically* to the in-memory one.
//!
//! Not "similarly", and not "in the same order". BM25's `idf` and its length
//! normalisation are corpus-relative — functions of the live document count,
//! a term's document frequency and the mean document length — so a backend
//! that computed any of them slightly differently would not fail. It would
//! return a plausible ranking with two hits transposed, or the same ranking
//! with scores that differ in the last place, and the second is worse: the
//! engine's `fuse()` and a user's `ORDER BY bm25_score(...)` both consume the
//! number, not the rank.
//!
//! So every assertion here compares the whole `Vec<Scored>` — ids *and*
//! scores — against a freshly built [`Bm25Index`] over the same documents,
//! and one of them compares the score bits directly so that the bar is
//! written down rather than implied by `f32: PartialEq`.
//!
//! Three other properties share this file because they are about the same
//! object:
//!
//! * the on-disk format round-trips — an index reopened from storage answers
//!   exactly as the one that was committed;
//! * a crash mid-build leaves something recoverable, and specifically **a
//!   stamp implies a complete index**, which is the whole of the currency
//!   protocol in `docs/indexes.md`;
//! * the bounded cache really is bounded, over a corpus far larger than it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use inlaysql_core::bm25::Bm25Index;
use inlaysql_core::bm25_paged::PagedBm25Index;
use inlaysql_core::mem::MemStorage;
use inlaysql_core::row::RowBuf;
use inlaysql_core::traits::{FullTextIndex, RowFilter, RowId, Scored, Storage};
use inlaysql_core::{Error, Result};

// =====================================================================
// storage doubles
// =====================================================================

/// A `MemStorage` several handles share, so that an index can be "reopened".
///
/// `inlaysql_core::SharedStorage` would do, but it holds a `Box<dyn Storage>`
/// and this file needs to reach past it to model a crash.
#[derive(Clone, Default)]
struct Shared {
    inner: Rc<RefCell<MemStorage>>,
    /// Mutating calls left before the machine stops. `None` is a machine that
    /// does not stop.
    budget: Rc<Cell<Option<usize>>>,
    stopped: Rc<Cell<bool>>,
    /// Mutating calls made, so a crash sweep knows how far a whole build
    /// reaches instead of guessing at a bound.
    writes: Rc<Cell<usize>>,
}

impl Shared {
    /// Stop after `writes` further mutating calls, discarding everything that
    /// was buffered and not yet committed — which is exactly what a power cut
    /// leaves behind for a copy-on-write tree: the last durable commit, and
    /// nothing after it.
    fn stop_after(&self, writes: usize) {
        self.budget.set(Some(writes));
    }

    fn has_stopped(&self) -> bool {
        self.stopped.get()
    }

    fn writes(&self) -> usize {
        self.writes.get()
    }

    /// Charge one mutating call against the budget, stopping if it runs out.
    fn charge(&self) -> Result<()> {
        if self.stopped.get() {
            return Err(Error::Storage(String::from("the machine has stopped")));
        }
        self.writes.set(self.writes.get() + 1);
        let Some(left) = self.budget.get() else {
            return Ok(());
        };
        if left > 0 {
            self.budget.set(Some(left - 1));
            return Ok(());
        }
        self.stopped.set(true);
        self.inner.borrow_mut().rollback()?;
        Err(Error::Storage(String::from("the machine has stopped")))
    }

    /// A handle onto the same bytes with no fault injection, which is what
    /// "reboot and reopen the file" means.
    fn rebooted(&self) -> Shared {
        Shared {
            inner: Rc::clone(&self.inner),
            budget: Rc::new(Cell::new(None)),
            stopped: Rc::new(Cell::new(false)),
            writes: Rc::new(Cell::new(0)),
        }
    }
}

impl Storage for Shared {
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        self.charge()?;
        self.inner.borrow_mut().put_row(table, id, bytes)
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        self.inner.borrow().get_row(table, id)
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        self.charge()?;
        self.inner.borrow_mut().delete_row(table, id)
    }

    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        self.inner.borrow().scan_batch(table, after, limit)
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.charge()?;
        self.inner.borrow_mut().put_meta(key, bytes)
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.borrow().get_meta(key)
    }

    fn commit(&mut self) -> Result<()> {
        self.charge()?;
        self.inner.borrow_mut().commit()
    }

    fn rollback(&mut self) -> Result<()> {
        self.inner.borrow_mut().rollback()
    }
}

// =====================================================================
// corpora
// =====================================================================

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A Zipf-ish document over a small vocabulary.
///
/// Small on purpose: a heavy head is what makes MaxScore actually demote
/// terms, and what makes documents tie on score constantly — a tie being
/// exactly what two backends that disagreed in the last bit would break
/// differently.
fn skewed(seed: u64) -> String {
    const VOCABULARY: [(u64, &str); 6] = [
        (45, "alpha"),
        (68, "beta"),
        (84, "gamma"),
        (93, "delta"),
        (98, "epsilon"),
        (100, "zeta"),
    ];
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut body = String::new();
    for _ in 0..2 + rng.next() % 25 {
        let draw = rng.next() % 100;
        let (_, word) = VOCABULARY
            .iter()
            .find(|(bound, _)| draw < *bound)
            .expect("the last bound is 100");
        body.push_str(word);
        body.push(' ');
    }
    body
}

/// A document over a wide vocabulary, so that most terms have a one-document
/// postings list and the dictionary carries the weight instead.
fn wide(seed: u64) -> String {
    let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
    let mut body = String::new();
    for _ in 0..4 + rng.next() % 30 {
        let word = rng.next() % 4_000;
        body.push('w');
        body.push_str(&word.to_string());
        body.push(' ');
    }
    body
}

/// Every query shape that could tell the two backends apart: a term in nearly
/// everything, a rare one, mixtures, a repeat (which is scored twice, on
/// purpose), and a term the corpus has never seen.
const QUERIES: [&str; 9] = [
    "alpha",
    "zeta",
    "alpha zeta",
    "beta gamma delta",
    "alpha beta gamma delta epsilon zeta",
    "zeta zeta alpha",
    "alpha nonesuch",
    "nonesuch",
    "w17 w2500 alpha",
];

const LIMITS: [usize; 6] = [1, 2, 10, 100, 1_000, usize::MAX];

/// Assert two answers are the same answer, scores included, bit for bit.
fn agree(paged: &[Scored], memory: &[Scored], what: &str) {
    assert_eq!(
        paged.len(),
        memory.len(),
        "{what}: {} hits against {}",
        paged.len(),
        memory.len()
    );
    for (left, right) in paged.iter().zip(memory) {
        assert_eq!(left.id, right.id, "{what}: row ids diverged");
        assert_eq!(
            left.score.to_bits(),
            right.score.to_bits(),
            "{what}: row {} scored {} paged and {} in memory",
            left.id,
            left.score,
            right.score
        );
    }
}

/// Build both backends over the same documents and check every query at every
/// limit, unfiltered and under two filters.
fn compare(documents: &[(RowId, String)], label: &str) {
    let mut paged = PagedBm25Index::new(Shared::default(), "fts");
    let mut memory = Bm25Index::new();
    for (id, body) in documents {
        paged.insert(*id, body).unwrap();
        memory.insert(*id, body).unwrap();
    }
    paged.commit().unwrap();
    assert_eq!(paged.len(), memory.len(), "{label}: live count diverged");

    for query in QUERIES {
        for k in LIMITS {
            agree(
                &paged.search(query, k, None).unwrap(),
                &memory.search(query, k, None).unwrap(),
                &format!("{label}: `{query}` at k={k}"),
            );
        }
        // A filter lowers the threshold — a rejected document never raises it
        // — so it prunes less. It must not prune differently.
        let seventh: &RowFilter = &|id: RowId| Ok(id.is_multiple_of(7));
        let nothing: &RowFilter = &|_: RowId| Ok(false);
        for (name, filter) in [("every seventh", seventh), ("nothing", nothing)] {
            for k in [1usize, 10, 1_000] {
                agree(
                    &paged.search(query, k, Some(filter)).unwrap(),
                    &memory.search(query, k, Some(filter)).unwrap(),
                    &format!("{label}: `{query}` at k={k} under {name}"),
                );
            }
        }
    }
}

// =====================================================================
// agreement
// =====================================================================

#[test]
fn a_handful_of_documents_scores_identically() {
    compare(
        &[
            (1, String::from("embedded rust database engine")),
            (2, String::from("rust web framework")),
            (3, String::from("cooking with cast iron")),
        ],
        "handful",
    );
}

/// Below one postings chunk, so every term's list is a single chunk and the
/// directory is never consulted.
#[test]
fn a_corpus_inside_one_chunk_scores_identically() {
    let documents: Vec<(RowId, String)> = (1..=100).map(|id| (id, skewed(id))).collect();
    compare(&documents, "one chunk");
}

/// Well past a chunk, so the directory, the splits and the cross-chunk walk
/// are all in play — and MaxScore is skipping over whole chunks it never
/// reads, which is the optimisation most likely to change an answer.
#[test]
fn a_corpus_spanning_many_chunks_scores_identically() {
    let documents: Vec<(RowId, String)> = (1..=2_000).map(|id| (id, skewed(id))).collect();
    compare(&documents, "many chunks");
}

/// Sparse, non-contiguous row ids, because the paged backend keys everything
/// by row id where the in-memory one uses a dense ordinal. If anything
/// depended on ids being 1..n, this is what finds it.
#[test]
fn sparse_row_ids_score_identically() {
    let documents: Vec<(RowId, String)> = (1..=400).map(|n| (n * 977 + 1, skewed(n))).collect();
    compare(&documents, "sparse ids");
}

/// A wide vocabulary: thousands of terms with one-document postings lists,
/// which is where the dictionary and the term records carry the cost rather
/// than the postings.
#[test]
fn a_wide_vocabulary_scores_identically() {
    let documents: Vec<(RowId, String)> = (1..=600).map(|id| (id, wide(id))).collect();
    compare(&documents, "wide vocabulary");
}

/// Empty and one-word documents alongside long ones. An empty document indexes
/// no terms and is still a document: it counts toward `doc_count` and pulls
/// the mean length down, so it moves every other document's score.
#[test]
fn degenerate_documents_score_identically() {
    let mut documents: Vec<(RowId, String)> = Vec::new();
    for id in 1..=200u64 {
        let body = match id % 4 {
            0 => String::new(),
            1 => String::from("alpha"),
            2 => skewed(id),
            _ => skewed(id).repeat(6),
        };
        documents.push((id, body));
    }
    compare(&documents, "degenerate");
}

/// Updates and deletes, applied in many small batches, are where a paged
/// postings list actually goes wrong: chunks split, shrink, empty, get their
/// slots reused and get retired with their term. The index has to converge on
/// a freshly built one over the surviving rows.
#[test]
fn churn_converges_on_a_freshly_built_index() {
    let mut paged = PagedBm25Index::new(Shared::default(), "fts");
    let mut memory = Bm25Index::new();

    for round in 0..30u64 {
        for id in 1..=150u64 {
            let body = skewed(id + round * 31);
            paged.insert(id, &body).unwrap();
            memory.insert(id, &body).unwrap();
        }
        for id in (round % 5 + 1..=150).step_by(5) {
            paged.remove(id).unwrap();
            memory.remove(id).unwrap();
        }
        // Committing every round, so the churn really is many batches rather
        // than one collapsed one — the batch is where "last write wins" hides
        // an ordering bug.
        paged.commit().unwrap();
    }

    assert_eq!(paged.len(), memory.len(), "live count diverged after churn");
    for query in QUERIES {
        for k in [1usize, 10, 150] {
            agree(
                &paged.search(query, k, None).unwrap(),
                &memory.search(query, k, None).unwrap(),
                &format!("after churn: `{query}` at k={k}"),
            );
        }
    }
}

/// Every document deleted must leave an index that finds nothing and has no
/// terms left — the same "no orphaned postings" property `Bm25Index` gets by
/// dropping an emptied list.
#[test]
fn deleting_every_document_leaves_an_index_that_finds_nothing() {
    let mut paged = PagedBm25Index::new(Shared::default(), "fts");
    for id in 1..=300u64 {
        paged.insert(id, &skewed(id)).unwrap();
    }
    paged.commit().unwrap();
    for id in 1..=300u64 {
        paged.remove(id).unwrap();
    }
    paged.commit().unwrap();

    assert!(paged.is_empty());
    for query in QUERIES {
        assert!(
            paged.search(query, 10, None).unwrap().is_empty(),
            "`{query}` still matched something"
        );
    }
}

// =====================================================================
// the on-disk format
// =====================================================================

/// Reopening the file must reproduce the answers exactly, which means the
/// corpus statistics survived as well as the postings — `live` and the total
/// length are what `idf` and the normalisation are computed from, so losing
/// either would rescore everything without losing a single posting.
#[test]
fn a_committed_index_round_trips_through_storage() {
    let storage = Shared::default();
    let mut memory = Bm25Index::new();
    {
        let mut paged = PagedBm25Index::new(storage.clone(), "fts");
        for id in 1..=800u64 {
            let body = skewed(id);
            paged.insert(id, &body).unwrap();
            memory.insert(id, &body).unwrap();
        }
        paged.commit().unwrap();
        // And a second batch after the first is durable, so what is reopened
        // is an index that was written in more than one go.
        for id in (1..=800u64).step_by(3) {
            let body = wide(id);
            paged.insert(id, &body).unwrap();
            memory.insert(id, &body).unwrap();
        }
        paged.commit().unwrap();
    }

    let reopened = PagedBm25Index::open(storage, "fts").unwrap();
    assert_eq!(reopened.len(), memory.len());
    for query in QUERIES {
        for k in [1usize, 10, 800] {
            agree(
                &reopened.search(query, k, None).unwrap(),
                &memory.search(query, k, None).unwrap(),
                &format!("reopened: `{query}` at k={k}"),
            );
        }
    }
}

/// The corpus is far larger than the cache, so it provably did not all fit in
/// memory — the whole point of the module — and the bound is never exceeded.
#[test]
fn a_corpus_larger_than_the_cache_is_searchable_with_bounded_memory() {
    const CAPACITY: usize = 16;
    let mut paged = PagedBm25Index::new(Shared::default(), "fts").with_cache_capacity(CAPACITY);
    let mut memory = Bm25Index::new();
    for id in 1..=3_000u64 {
        let body = wide(id);
        paged.insert(id, &body).unwrap();
        memory.insert(id, &body).unwrap();
    }
    paged.commit().unwrap();

    for query in QUERIES {
        agree(
            &paged.search(query, 10, None).unwrap(),
            &memory.search(query, 10, None).unwrap(),
            &format!("bounded cache: `{query}`"),
        );
        assert!(
            paged.cache_len() <= CAPACITY,
            "cache grew to {} entries, bound is {CAPACITY}",
            paged.cache_len()
        );
    }
}

// =====================================================================
// crash recovery
// =====================================================================

/// A crash anywhere in a build must leave something recoverable, and the
/// specific invariant that makes "recoverable" checkable is **a stamp implies
/// a complete index**: the write version goes into the header only on the
/// commit that finishes the build, so a header a crash caught mid-batch
/// carries none and the engine's ordinary staleness check rebuilds rather than
/// believes it.
///
/// Swept rather than sampled at one convenient point, the way
/// `cancellation.rs` sweeps every stopping place: stop after one storage write,
/// then after two, and so on until the build completes.
#[test]
fn a_crash_mid_build_never_leaves_a_stamped_index() {
    let documents: Vec<(RowId, String)> = (1..=120).map(|id| (id, skewed(id))).collect();
    let mut memory = Bm25Index::new();
    for (id, body) in &documents {
        memory.insert(*id, body).unwrap();
    }

    // What a complete build costs, so the sweep covers all of it rather than
    // stopping wherever a guessed bound happened to fall.
    let total = {
        let counter = Shared::default();
        let mut paged = PagedBm25Index::new(counter.clone(), "fts").with_pending_limit(16);
        for (id, body) in &documents {
            paged.insert(*id, body).unwrap();
        }
        paged.prepare_commit(9, true);
        paged.commit().unwrap();
        counter.writes()
    };

    let mut completed = 0usize;
    let mut crashed = 0usize;
    // Past `total` as well as up to it, so the sweep ends with a schedule that
    // lets the build finish — the control case that proves the assertions
    // below are not passing because nothing ever completed.
    for stop in (1..=total + 13).step_by(13) {
        let storage = Shared::default();
        storage.stop_after(stop);
        let mut paged = PagedBm25Index::new(storage.clone(), "fts").with_pending_limit(16);
        let mut failed = false;
        for (id, body) in &documents {
            if paged.insert(*id, body).is_err() {
                failed = true;
                break;
            }
        }
        if !failed {
            paged.prepare_commit(9, true);
            failed = paged.commit().is_err();
        }
        drop(paged);

        if !failed && !storage.has_stopped() {
            completed += 1;
            // A build that finished is stamped and answers exactly.
            let reopened = PagedBm25Index::open(storage.rebooted(), "fts").unwrap();
            assert_eq!(reopened.stored_write_version(), Some(9));
            agree(
                &reopened.search("alpha zeta", 10, None).unwrap(),
                &memory.search("alpha zeta", 10, None).unwrap(),
                "a completed build",
            );
            continue;
        }

        crashed += 1;
        // Whatever survived, opening it must not fail and must not claim to
        // describe the rows.
        let mut reopened = PagedBm25Index::open(storage.rebooted(), "fts")
            .unwrap_or_else(|error| panic!("stop at {stop}: reopening failed: {error}"));
        assert_ne!(
            reopened.stored_write_version(),
            Some(9),
            "stop at {stop}: a half-built index claimed to be current"
        );

        // And it is recoverable: the engine's response to an unstamped index
        // is `reset` and rebuild from the rows, which must land on exactly the
        // index a fresh build would have produced.
        reopened.reset().unwrap();
        for (id, body) in &documents {
            reopened.insert(*id, body).unwrap();
        }
        reopened.prepare_commit(9, true);
        reopened.commit().unwrap();
        assert_eq!(
            reopened.len(),
            memory.len(),
            "stop at {stop}: rebuild lost rows"
        );
        for query in ["alpha", "alpha zeta", "beta gamma delta"] {
            agree(
                &reopened.search(query, 10, None).unwrap(),
                &memory.search(query, 10, None).unwrap(),
                &format!("stop at {stop}: rebuilt `{query}`"),
            );
        }
    }

    // The sweep has to have exercised both halves, or it proved nothing.
    assert!(
        crashed > 0,
        "no crash schedule actually interrupted a build"
    );
    assert!(completed > 0, "no crash schedule let a build finish");
}
