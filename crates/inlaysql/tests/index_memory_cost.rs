//! What a retrieval index costs in RAM, per document and per vector.
//!
//! `docs/enterprise-readiness.md` blocker 6 says the retrieval indexes are
//! fully resident and paid for once per connection, and puts a number on it —
//! "a 10M-vector corpus at 384 dimensions is roughly 15 GB of `f32` per
//! connection". That number is an estimate from the declared dimension, and an
//! estimate is exactly what this repository does not accept from itself: it
//! counts only the payload, and the payload is not what the process actually
//! holds. `HnswIndex` keeps each embedding *twice* (the source map and the
//! committed graph node), `Bm25Index` holds a term dictionary and a per-document
//! term list that the estimate does not mention at all, and every container
//! around them has a header and a growth factor.
//!
//! So this measures instead of estimating. It installs a counting global
//! allocator and reports live heap growth for:
//!
//! * `Bm25Index` — bytes per document, at four corpus sizes.
//! * `HnswIndex` — bytes per vector, exact and int8, at three corpus sizes.
//! * A whole file-backed `Database` handle over the same corpus, which is the
//!   quantity blocker 6 is actually about: what *one connection* holds.
//! * The same handle opened with `Database::open_paged`, which is the only
//!   lever that exists today.
//! * Several handles on one file at once, because "per connection" is a
//!   multiplier and a multiplier has to be shown multiplying.
//!
//! Run it deliberately, in release, and read the numbers:
//!
//! ```sh
//! cargo test --release -p inlaysql --test index_memory_cost -- --nocapture --ignored
//! ```
//!
//! It is `#[ignore]`d because it is an instrument, not an assertion: it prints
//! a measurement and passes as long as the indexes answer at all. A byte
//! threshold here would fail on the next allocator change and teach everyone to
//! ignore it.
//!
//! # Why one test in its own binary
//!
//! The allocator is process-wide and `cargo test` runs a file's tests on
//! several threads at once, so any other test allocating at the same time lands
//! in the same number. One test, one binary, one measurement — the same reason
//! `crates/inlaysql-server/tests/streaming_memory.rs` is its own file.
//!
//! # Why *live* heap and not peak
//!
//! Peak answers "how much did building it cost", which is a transient. Blocker
//! 6 is about what is still held afterwards, for as long as the connection
//! lives, so the number is live bytes with the builder's scratch already freed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use inlaysql::{Bm25Index, Database, HnswIndex, Value};
use inlaysql_core::{FullTextIndex, VectorIndex};

// =====================================================================
// the measurement
// =====================================================================

/// Live heap bytes right now.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// The system allocator with a running total around it.
///
/// Relaxed ordering throughout: this test is single-threaded by construction
/// (see the module note), so there is nothing for a stronger ordering to
/// synchronise with and the counter must not become the thing the measurement
/// is measuring.
struct Measured;

unsafe impl GlobalAlloc for Measured {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded unchanged to the allocator this one
        // wraps, which is the same contract this method was called under.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `pointer` came from `alloc` above with this same `layout`,
        // which is what this method's own contract already requires.
        unsafe { System.dealloc(pointer, layout) }
    }
}

// `realloc` and `alloc_zeroed` are deliberately not overridden: their default
// implementations are written in terms of `alloc` and `dealloc` above, so they
// are already accounted for, and reimplementing them would only add a way for
// the accounting to disagree with itself.
#[global_allocator]
static ALLOCATOR: Measured = Measured;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// Live heap held by whatever `build` returns, once its scratch has been freed.
///
/// The value is handed back rather than dropped inside, so the caller decides
/// when it dies — measuring a structure and then dropping it inside the meter
/// would report zero.
fn held<T>(build: impl FnOnce() -> T) -> (T, usize) {
    let before = live();
    let value = build();
    (value, live().saturating_sub(before))
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

// =====================================================================
// the corpora
// =====================================================================

/// Vectors are 384-dimensional because that is the number blocker 6 quotes,
/// and it is `all-MiniLM-L6-v2`'s — the embedding almost every local RAG stack
/// starts from.
const DIM: usize = 384;

/// Tokens per document. A retrieval corpus is chunks, not whole files, and a
/// 120-token chunk is the middle of what every chunker in use emits.
const TERMS_PER_DOC: usize = 120;

/// Distinct words the generator may draw from.
///
/// This is the parameter BM25's per-document cost is most sensitive to and the
/// one an estimate always omits: a document's cost is its *distinct* terms, and
/// how many of 120 tokens are distinct depends entirely on how heavy the head
/// of the distribution is. A 200,000-word vocabulary drawn Zipf-ian is roughly
/// English, and is stated here rather than buried so the number below can be
/// read as "for a corpus like this" instead of as a universal constant.
const VOCABULARY: usize = 200_000;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The `rank`th word of the vocabulary, as a base-26 string.
///
/// Length rises with rank, which is backwards from real language (common words
/// are short) and is deliberately the conservative direction: the head of a
/// Zipf draw is what a `BTreeMap<String, u32>` holds, so short common words
/// would make the dictionary look cheaper than it is.
fn word(rank: usize, out: &mut String) {
    out.clear();
    let mut n = rank + 1;
    while n > 0 {
        out.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
    }
}

/// One document of [`TERMS_PER_DOC`] Zipf-drawn words.
///
/// `rank = floor(V^u)` for uniform `u` gives a density proportional to `1/rank`
/// — Zipf with exponent 1, the shape natural language actually has. A uniform
/// draw would make every document's terms distinct and every posting list one
/// entry long, which is the worst case for the dictionary and the best case for
/// the postings, and resembles no corpus anyone indexes.
fn document(rng: &mut Rng, buffer: &mut String, scratch: &mut String) {
    buffer.clear();
    let span = (VOCABULARY as f64).ln();
    for _ in 0..TERMS_PER_DOC {
        let rank = (rng.unit() * span).exp() as usize;
        word(rank.min(VOCABULARY - 1), scratch);
        buffer.push_str(scratch);
        buffer.push(' ');
    }
}

/// A deterministic unit-norm vector.
///
/// Random directions in 384 dimensions are the ANN worst case for *recall* —
/// `bench/README.md` explains why — but memory does not care about direction,
/// and this costs nothing to generate. The bytes a vector occupies are a
/// function of `DIM` and the encoding alone.
fn vector(rng: &mut Rng, out: &mut Vec<f32>) {
    out.clear();
    let mut norm = 0.0f32;
    for _ in 0..DIM {
        let x = (rng.next() % 2_000) as f32 / 1_000.0 - 1.0;
        norm += x * x;
        out.push(x);
    }
    let norm = norm.sqrt().max(f32::EPSILON);
    for x in out.iter_mut() {
        *x /= norm;
    }
}

// =====================================================================
// the instrument
// =====================================================================

#[test]
#[ignore = "an instrument, not an assertion — run it with --nocapture"]
fn what_a_resident_retrieval_index_costs_per_connection() {
    bm25_bytes_per_document();
    hnsw_bytes_per_vector();
    a_whole_connection();
}

/// BM25's resident cost, and where inside the structure it goes.
///
/// Reported per document at four sizes rather than at one, because the
/// structure has both per-document terms (`ids`, `lengths`, `doc_terms`,
/// `ordinals`) and per-*term* terms (`terms`, `postings`, `impacts`), and the
/// vocabulary grows sub-linearly with the corpus (Heaps' law). One size cannot
/// tell those apart; four can.
fn bm25_bytes_per_document() {
    println!();
    println!("=== BM25 (`Bm25Index`), fully resident, no paged variant exists ===");
    println!(
        "{:>10}  {:>12}  {:>14}  {:>12}  {:>16}",
        "documents", "held", "bytes/doc", "vocabulary", "10M docs would be"
    );

    for documents in [2_000usize, 8_000, 32_000, 128_000] {
        let (index, bytes) = held(|| {
            let mut index = Bm25Index::new();
            let mut rng = Rng::new(0x243f_6a88_85a3_08d3);
            let mut body = String::new();
            let mut scratch = String::new();
            for id in 1..=documents as u64 {
                document(&mut rng, &mut body, &mut scratch);
                index.insert(id, &body).unwrap();
            }
            index
        });

        // The dictionary size is not exposed, so it is counted the way a
        // caller could: the encoding writes one entry per live term.
        let vocabulary = observed_vocabulary(&index);
        let per_document = bytes as f64 / documents as f64;
        println!(
            "{documents:>10}  {:>10.1} MiB  {per_document:>14.0}  {vocabulary:>12}  {:>13.1} GiB",
            mib(bytes),
            per_document * 10_000_000.0 / (1024.0 * 1024.0 * 1024.0)
        );
        assert!(!index.is_empty(), "the index answered nothing");
        drop(index);
    }

    println!(
        "Every byte above is in the connection's own heap: `Bm25Index` has no \
         paged backend at all,"
    );
    println!(
        "so this is paid once per open handle and is not reduced by \
         `Database::open_paged`."
    );
}

/// How many distinct terms the index currently holds, counted the only way a
/// caller can: the encoding emits the term count in its header.
fn observed_vocabulary(index: &Bm25Index) -> u32 {
    let bytes = index.encode();
    // `index := u8 version, u32 term_count, ...` — see `Bm25Index::encode`.
    u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]])
}

/// HNSW's resident cost, exact and int8.
///
/// The estimate blocker 6 quotes is `n * dim * 4`. What this shows is that the
/// index holds the embedding twice — `embeddings` is the source of truth and
/// every committed `Node` carries its own normalised copy — so the payload
/// alone is already double, before the per-layer adjacency `Vec<Vec<usize>>`
/// that the graph is made of.
fn hnsw_bytes_per_vector() {
    println!();
    println!("=== ANN (`HnswIndex`), fully resident ===");
    println!(
        "{:>10}  {:>9}  {:>12}  {:>13}  {:>12}  {:>18}",
        "vectors", "encoding", "held", "bytes/vector", "payload est.", "10M vectors would be"
    );

    for vectors in [2_000usize, 8_000, 32_000] {
        for (label, quantized) in [("exact", false), ("int8", true)] {
            let (index, bytes) = held(|| {
                let mut index = if quantized {
                    HnswIndex::new_quantized(DIM)
                } else {
                    HnswIndex::new(DIM)
                };
                let mut rng = Rng::new(0x9e37_79b9_7f4a_7c15);
                let mut embedding = Vec::with_capacity(DIM);
                for id in 1..=vectors as u64 {
                    vector(&mut rng, &mut embedding);
                    index.insert(id, &embedding).unwrap();
                }
                index.commit().unwrap();
                index
            });

            let per_vector = bytes as f64 / vectors as f64;
            // What the blocker-6 estimate counts: one copy of the declared
            // payload, nothing else.
            let payload = if quantized { DIM + 4 } else { DIM * 4 };
            println!(
                "{vectors:>10}  {label:>9}  {:>8.1} MiB  {per_vector:>13.0}  {payload:>12}  \
                 {:>15.1} GiB",
                mib(bytes),
                per_vector * 10_000_000.0 / (1024.0 * 1024.0 * 1024.0)
            );
            // Prove the thing answered, so the measurement is of a working
            // index rather than of a half-built one.
            let mut rng = Rng::new(1);
            let mut query = Vec::with_capacity(DIM);
            vector(&mut rng, &mut query);
            assert!(!index.search(&query, 10, None).unwrap().is_empty());
            drop(index);
        }
    }
}

/// The quantity blocker 6 is actually about: one connection's whole handle.
///
/// The two standalone measurements above are structures. This is a
/// `Database` on a real file — page cache, catalog, engine and both retrieval
/// indexes — opened the way `crates/inlaysql-server/src/lib.rs` opens one per
/// connection, and then opened again beside itself to show the multiplier.
fn a_whole_connection() {
    // Above `hnsw_paged::DEFAULT_CACHE_NODES` (4096) on purpose. Under it the
    // paged index caches the entire corpus and reports the same resident bytes
    // as the in-RAM one, which would make the comparison say nothing — the
    // cache is a ceiling, and a corpus that fits below the ceiling never
    // touches it. Not much larger, because every row here goes through the
    // whole SQL path and the paged graph build writes each node through the
    // tree, which is minutes rather than seconds; the per-row slopes were
    // established at scale by the two sections above, and what this section
    // adds is everything *around* the indexes, which the row count does not
    // change.
    const ROWS: usize = 8_000;
    const HANDLES: usize = 4;

    let directory =
        std::env::temp_dir().join(format!("inlaysql-index-memory-cost-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();

    println!();
    println!("=== one connection's handle on a {ROWS}-row corpus (dim {DIM}) ===");
    println!(
        "{:>10}  {:>14}  {:>11}  {:>17}  {:>15}",
        "opened as", "held on open", "bytes/row", "each +1 handle", "of that, ANN"
    );

    for paged in [false, true] {
        let path = directory.join(if paged {
            "paged.inlay"
        } else {
            "resident.inlay"
        });
        let _ = std::fs::remove_file(&path);
        build(&path, ROWS, paged);

        // A fresh handle on a file that already holds the corpus: this is
        // exactly what `serve_connection` does when a client connects.
        let (first, first_bytes) = held(|| open_and_query(&path, paged));
        // And the rest, on the same file in the same process, which is what
        // "per connection" means. Measured as a group and divided, so the
        // one-time allocations the first handle drags in with it — the parsed
        // catalog, the format tables — do not land in the marginal number.
        let (rest, rest_bytes) = held(|| {
            let mut handles = Vec::with_capacity(HANDLES - 1);
            for _ in 1..HANDLES {
                handles.push(open_and_query(&path, paged));
            }
            handles
        });

        // The backend's own account of its vector payload, for the part of the
        // marginal cost the paged index is supposed to move.
        let ann = first
            .vector_index_resident_bytes("docs", "embedding")
            .unwrap_or(0);
        println!(
            "{:>10}  {:>10.1} MiB  {:>11.0}  {:>13.1} MiB  {:>11.1} MiB",
            if paged { "open_paged" } else { "open" },
            mib(first_bytes),
            first_bytes as f64 / ROWS as f64,
            mib(rest_bytes) / (HANDLES - 1) as f64,
            mib(ann)
        );
        drop(first);
        drop(rest);
        let _ = std::fs::remove_file(&path);
    }

    println!(
        "`open_paged` moves the ANN graph into the file and leaves a bounded node cache \
         behind."
    );
    println!(
        "It does not touch BM25, which has no paged backend — so whatever the two rows \
         still share"
    );
    println!("is what a paged vector index cannot reach.");

    let _ = std::fs::remove_dir_all(&directory);
}

/// Load `rows` documents-with-embeddings into a fresh file.
fn build(path: &Path, rows: usize, paged: bool) {
    let mut db = open(path, paged);
    db.execute(
        &format!("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR({DIM}))"),
        &[],
    )
    .unwrap();
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])
        .unwrap();
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .unwrap();

    let mut rng = Rng::new(0x243f_6a88_85a3_08d3);
    let mut vectors = Rng::new(0x9e37_79b9_7f4a_7c15);
    let mut body = String::new();
    let mut scratch = String::new();
    let mut embedding = Vec::with_capacity(DIM);
    // Batched, or every row pays its own `fsync` and the load takes minutes
    // that say nothing about memory. Fifty rows and not more: a commit record
    // must fit one write-ahead-log region, and a row here carries a 1.5 KiB
    // embedding plus its text (`docs/enterprise-readiness.md` blocker 5).
    db.begin().unwrap();
    for id in 1..=rows as u64 {
        document(&mut rng, &mut body, &mut scratch);
        vector(&mut vectors, &mut embedding);
        db.execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(id as i64),
                Value::Text(body.clone().into()),
                Value::Vector(embedding.clone()),
            ],
        )
        .unwrap();
        if id % 50 == 0 {
            db.commit().unwrap();
            db.begin().unwrap();
        }
    }
    db.commit().unwrap();
    // Write the index blobs, so a later open restores rather than rebuilds —
    // the state a long-running server file is in.
    db.checkpoint().unwrap();
}

fn open(path: &Path, paged: bool) -> Database {
    if paged {
        Database::open_paged(path).unwrap()
    } else {
        Database::open(path).unwrap()
    }
}

/// Open a handle and run one hybrid query on it, so what is measured is a
/// connection that has been used rather than one that has only been opened.
fn open_and_query(path: &Path, paged: bool) -> Database {
    let mut db = open(path, paged);
    let mut rng = Rng::new(7);
    let mut query = Vec::with_capacity(DIM);
    vector(&mut rng, &mut query);
    let mut body = String::new();
    let mut scratch = String::new();
    document(&mut Rng::new(11), &mut body, &mut scratch);
    db.query(
        "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
         FROM docs ORDER BY score DESC LIMIT 10",
        &[Value::Vector(query), Value::Text(body.into())],
    )
    .unwrap();
    db
}
