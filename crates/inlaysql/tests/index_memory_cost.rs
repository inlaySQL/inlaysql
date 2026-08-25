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
//! * `PagedBm25Index` over a real file, at the same sizes and on the same
//!   documents, which is what makes the two numbers comparable rather than
//!   merely both printed.
//! * `HnswIndex` — bytes per vector, exact and int8, at three corpus sizes.
//! * A whole file-backed `Database` handle over the same corpus, which is the
//!   quantity blocker 6 is actually about: what *one connection* holds.
//! * The same handle with each paging lever on — `paged_vector_indexes`,
//!   `paged_text_indexes`, and both.
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

use inlaysql::{
    Bm25Index, Database, EngineOptions, FileDevice, HnswIndex, PagedBm25Index, TreeStorage, Value,
};
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

/// A scratch directory of this process's own.
fn workspace(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "inlaysql-index-memory-cost-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
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
    println!("=== BM25 (`Bm25Index`), fully resident ===");
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

    println!("Every byte above is in the connection's own heap, paid once per open handle.");
    paged_bm25_bytes_per_document();
}

/// The same corpora through `PagedBm25Index`, on a real file.
///
/// On a file and not `MemStorage`, because a memory-backed store would move
/// the postings from one heap allocation to another and report a saving that
/// does not exist. What is measured is the same quantity as above — live heap
/// after the builder's scratch is freed — so the two tables are directly
/// comparable, which is the only reason either number is worth printing.
///
/// The cache is left at its default: a ceiling is not a reservation, and the
/// interesting number is what a built index settles at, not what it could hold.
///
/// # Why the file size is in this table too
///
/// Because it is the price, and a memory table that omitted it would be an
/// advertisement. An inverted index update touches one page per distinct term
/// of the document, copy-on-write copies every one of them, and a bulk load has
/// to commit each time the write-ahead-log region fills — every half megabyte
/// of dirty pages. Every superseded page is then abandoned rather than
/// reclaimed, because `page_reuse` is off by default
/// (`docs/enterprise-readiness.md` blocker 4), so the file grows by *hundreds
/// of kilobytes per document*. That column is the whole cost of this backend
/// and nobody should have to discover it from a full disk.
///
/// Reuse is left off here rather than turned on, for a reason that is a finding
/// in its own right: **with `page_reuse` on, this build is refused for size.**
/// `Storage::transaction_is_nearly_full` answers from the dirty set as it
/// stands, and committing with reuse on then writes free-list entries of its
/// own — so a batch that was under the ceiling when it was last asked is over
/// it by the time the record is built (measured: refused at 1,076,352 bytes
/// against a 1,048,576-byte region, having last been asked at 524,288). That
/// is blocker 4's flag meeting blocker 5's ceiling, it is not specific to this
/// index, and any batched writer that trusts that method is exposed to it.
///
/// # Why this stops at 8,000 where the resident table goes to 128,000
///
/// The resident table's number *falls* with corpus size — its dictionary is
/// still saturating — so it needs four sizes to show where it flattens. This
/// one holds nothing per document at all, so two sizes are enough to show that
/// it does not move, and a third would cost several gigabytes of file and an
/// hour of `fsync`s to print the same figure again.
fn paged_bm25_bytes_per_document() {
    let directory = workspace("bm25");
    println!();
    println!("=== BM25 (`PagedBm25Index`), postings in the file ===");
    // No "10M docs would be" column here, unlike the table above, and the
    // absence is the point: that column multiplies a *slope* by ten million,
    // and this backend has no slope to multiply. Extrapolating a fixed cost
    // that way would print 77 GiB for a figure that does not move.
    println!(
        "{:>10}  {:>12}  {:>14}  {:>8}  {:>12}",
        "documents", "held", "bytes/doc", "cached", "file"
    );

    for documents in [2_000usize, 8_000] {
        let path = directory.join(format!("{documents}.inlay"));
        let (index, bytes) = held(|| {
            let storage = TreeStorage::open_on(FileDevice::open(&path).unwrap()).unwrap();
            let mut index = PagedBm25Index::new(storage, "\u{1}fts:docs\u{1}body\u{1}");
            let mut rng = Rng::new(0x243f_6a88_85a3_08d3);
            let mut body = String::new();
            let mut scratch = String::new();
            for id in 1..=documents as u64 {
                document(&mut rng, &mut body, &mut scratch);
                index.insert(id, &body).unwrap();
            }
            index.commit().unwrap();
            index
        });

        let per_document = bytes as f64 / documents as f64;
        let file = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        println!(
            "{documents:>10}  {:>10.1} MiB  {per_document:>14.0}  {:>8}  {:>8.1} MiB",
            mib(bytes),
            index.cache_len(),
            mib(file as usize),
        );
        // Prove the thing answers, so the measurement is of a working index
        // rather than of an empty one that costs nothing by doing nothing.
        assert!(!index.is_empty(), "the index held no documents");
        let mut body = String::new();
        let mut scratch = String::new();
        document(
            &mut Rng::new(0x243f_6a88_85a3_08d3),
            &mut body,
            &mut scratch,
        );
        assert!(
            !index.search(&body, 10, None).unwrap().is_empty(),
            "the index answered nothing"
        );
        drop(index);
        let _ = std::fs::remove_file(&path);
    }

    println!(
        "Held here is the bounded entry cache plus this handle's 8 MiB page cache — not the \
         corpus — which"
    );
    println!(
        "is why the figure is the *same* at both sizes while the resident one is still falling. \
         Read the"
    );
    println!(
        "bytes/doc column as that fixed cost divided by the corpus, not as a per-document price: \
         there is"
    );
    println!(
        "none. The two tables are not quite like for like, and in this one's favour — a \
         `Bm25Index` has"
    );
    println!(
        "no storage handle to carry, so ~8 MiB of what is held here is a page cache the other \
         never had."
    );
    println!(
        "The file column is the price, and it is paid once for the database rather than once \
         per connection."
    );
    let _ = std::fs::remove_dir_all(&directory);
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

    let directory = workspace("connection");

    println!();
    println!("=== one connection's handle on a {ROWS}-row corpus (dim {DIM}) ===");
    println!(
        "{:>16}  {:>14}  {:>11}  {:>17}  {:>15}",
        "opened as", "held on open", "bytes/row", "each +1 handle", "of that, ANN"
    );

    for (label, options) in [
        ("neither", paging(false, false)),
        ("vectors", paging(true, false)),
        ("text", paging(false, true)),
        ("both", paging(true, true)),
    ] {
        let path = directory.join(format!("{label}.inlay"));
        let _ = std::fs::remove_file(&path);
        build(&path, ROWS, options);

        // A fresh handle on a file that already holds the corpus: this is
        // exactly what `serve_connection` does when a client connects.
        let (first, first_bytes) = held(|| open_and_query(&path, options));
        // And the rest, on the same file in the same process, which is what
        // "per connection" means. Measured as a group and divided, so the
        // one-time allocations the first handle drags in with it — the parsed
        // catalog, the format tables — do not land in the marginal number.
        let (rest, rest_bytes) = held(|| {
            let mut handles = Vec::with_capacity(HANDLES - 1);
            for _ in 1..HANDLES {
                handles.push(open_and_query(&path, options));
            }
            handles
        });

        // The backend's own account of its vector payload, for the part of the
        // marginal cost the paged index is supposed to move.
        let ann = first
            .vector_index_resident_bytes("docs", "embedding")
            .unwrap_or(0);
        println!(
            "{label:>16}  {:>10.1} MiB  {:>11.0}  {:>13.1} MiB  {:>11.1} MiB",
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
        "`paged_vector_indexes` moves the ANN graph into the file and leaves a bounded node \
         cache behind;"
    );
    println!(
        "`paged_text_indexes` does the same for the postings. What the `both` row still holds \
         is the"
    );
    println!("page caches, the catalog and the engine — none of which grows with the corpus.");

    let _ = std::fs::remove_dir_all(&directory);
}

/// The two paging levers, as engine options.
fn paging(vectors: bool, text: bool) -> EngineOptions {
    EngineOptions {
        paged_vector_indexes: vectors,
        paged_text_indexes: text,
        ..EngineOptions::default()
    }
}

/// Load `rows` documents-with-embeddings into a fresh file.
fn build(path: &Path, rows: usize, options: EngineOptions) {
    let mut db = open(path, options);
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

fn open(path: &Path, options: EngineOptions) -> Database {
    Database::open_on_with_options(FileDevice::open(path).unwrap(), options).unwrap()
}

/// Open a handle and run one hybrid query on it, so what is measured is a
/// connection that has been used rather than one that has only been opened.
fn open_and_query(path: &Path, options: EngineOptions) -> Database {
    let mut db = open(path, options);
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
