//! Vector retrieval: recall and latency, InlaySQL against `sqlite-vec`.
//!
//! # Why recall is reported before latency
//!
//! An approximate index can be made arbitrarily fast by being arbitrarily
//! wrong, so a latency number on its own is meaningless. Every engine here is
//! measured against the same oracle — exhaustive cosine similarity computed in
//! Rust — and its **recall@k**, the fraction of the true top-k it actually
//! returned, is printed next to its latency. Read the two together or not at
//! all.
//!
//! # The baseline
//!
//! [`sqlite-vec`](https://github.com/asg017/sqlite-vec) is SQLite's vector
//! extension and the thing an InlaySQL user would otherwise reach for. Its
//! `vec0` tables scan exhaustively, so its recall is 1.0 by construction and
//! its latency grows linearly with the corpus. That is the trade this
//! benchmark exists to show: InlaySQL's HNSW gives up a little recall to stop
//! reading the whole table.
//!
//! pgvector is deliberately **not** here. It needs a running PostgreSQL
//! server, which would make this suite unreproducible from `cargo run` — the
//! one property the project rules require of a published number. Comparing
//! against it belongs with the containerised benchmark work in Stage 5.
//!
//! # Why there are two corpora
//!
//! What an ANN index can achieve is decided by the data, not by the index, and
//! by one property of it: **intrinsic dimensionality**. A graph index works by
//! walking downhill towards the query. That only terminates near the right
//! answer if the data has structure to walk along.
//!
//! Uniformly random unit vectors have none. In 384 dimensions every pair is
//! near-orthogonal and every distance concentrates around the same value — the
//! 10th nearest neighbour and the 1000th differ by about a percent — so there
//! is no downhill, and holding recall fixed as the corpus grows costs an
//! `ef_search` that grows with it. Measured here (`--suite sweep`), recall@10
//! at `M = 16` needs `ef` 256 at 5,000 vectors, 1024 at 20,000 and beyond 2048
//! at 100,000 to stay near 0.98. That is a linear scan wearing a graph.
//!
//! Real embeddings are the opposite: a few hundred nominal dimensions with an
//! intrinsic dimensionality in the tens, because meaning is clustered. The same
//! index on the same `dim = 384`, over embeddings derived from text, holds
//! recall@10 at 0.998 from 5,000 vectors to 100,000 with `ef` fixed at 64.
//!
//! Publishing only the first number describes a workload nobody has; publishing
//! only the second hides the worst case. Both run, both are printed, and the
//! gap between them is the most informative thing this suite reports.
//!
//! This was not a deliberate choice before AHL-372. The suite had one corpus
//! that was *meant* to be uniform on the sphere and, through an off-by-one-bit
//! divisor, was neither that nor realistic — see [`uniform_corpus`].
//!
//! # The filtered cases
//!
//! After the unfiltered comparison, three filtered passes run the same
//! corpus again, at ~10%, ~1% and ~0.1% of rows per bucket (`WHERE tenant %
//! ? = ?`). The ~1% pass is the failure mode AHL-379 is about: a fixed
//! candidate budget contains essentially none of one tenant, so filtering
//! *after* retrieval returns nothing. The engine now pushes the filter into
//! the index walk — rejected rows are traversed but neither returned nor
//! counted — so each pass reports both recall (against the bucket's own
//! exhaustive top-k) and the latency the filtered walk costs. The ~0.1%
//! bucket is the pathological end: the filter admits fewer rows than the
//! `LIMIT`, so the walk drains the whole graph and answers exactly — the
//! case where the old over-fetch loop re-walked the graph once per doubling
//! round before giving up. The unfiltered row above is the permissive end:
//! a filter that admits everything costs one walk, and the engine's tie
//! test pins filtered-everything to unfiltered exactly.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use inlaysql::embedding::hashed_embedding;
use inlaysql::{Database, Value};
use inlaysql_core::mem::{cosine_similarity, SeededRng};
use inlaysql_core::Rng;

use crate::{percentiles, Config, VOCABULARY};

/// A corpus entry: a row id and its embedding.
pub type Vector = (u64, Vec<f32>);

/// The tenant column's modulus: `tenant = id % TENANTS`. The filtered passes
/// bucket it further with `tenant % buckets`, for `buckets` in {10, 100,
/// 1000} — each bucket then owns ~10%, ~1% and ~0.1% of the rows, and every
/// bucket is an exact `id % buckets` set because `buckets` divides `TENANTS`.
/// The ~1% bucket is the selectivity AHL-379 is about: a fixed candidate
/// budget filtered afterwards contains essentially none of one tenant.
const TENANTS: u64 = 1000;

/// The two data shapes this suite measures. See the module note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Uniform on the sphere: maximal intrinsic dimensionality, and the worst
    /// case any graph index can be handed.
    Uniform,
    /// Embeddings derived from text, so the corpus clusters the way real ones
    /// do. What an application would actually store.
    Text,
}

impl Shape {
    pub fn label(self) -> &'static str {
        match self {
            Shape::Uniform => "uniform random (ANN worst case)",
            Shape::Text => "text-derived embeddings (realistic)",
        }
    }
}

/// What one engine achieved on the vector workload.
struct Outcome {
    label: &'static str,
    build: Duration,
    samples: Vec<Duration>,
    /// Mean fraction of the true top-k returned.
    recall: f64,
    file_bytes: u64,
    resident_vector_bytes: Option<usize>,
}

pub fn run(config: &Config, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for shape in [Shape::Text, Shape::Uniform] {
        run_shape(config, dir, shape, true)?;
    }
    Ok(())
}

/// The AHL-383 acceptance run: exact and int8 on both corpus shapes without
/// rebuilding the unrelated paged and incremental indexes between them.
pub fn run_quantization(config: &Config, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for shape in [Shape::Text, Shape::Uniform] {
        run_shape(config, dir, shape, false)?;
    }
    Ok(())
}

fn run_shape(
    config: &Config,
    dir: &Path,
    shape: Shape,
    auxiliary: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (corpus, queries) = corpus(config, shape);
    let k = config.limit;
    println!(
        "\n=== vector workload: {} vectors, dim {}, {} queries, top-{k} ===\n=== corpus: {} ===",
        corpus.len(),
        config.dim,
        queries.len(),
        shape.label()
    );

    // The oracle: exhaustive cosine similarity, computed here so that both
    // engines are scored against the same answer.
    let truth: Vec<Vec<u64>> = queries.iter().map(|q| exact_top_k(&corpus, q, k)).collect();

    let (ours, ours_moderate, ours_selective, ours_pathological) = inlaysql_vectors(
        &dir.join("vectors-inlaysql.inlay"),
        &corpus,
        &queries,
        k,
        &truth,
        false,
    )?;
    let (quantized, _, _, _) = inlaysql_vectors(
        &dir.join("vectors-inlaysql-int8.inlay"),
        &corpus,
        &queries,
        k,
        &truth,
        true,
    )?;
    let theirs = sqlite_vec_vectors(&dir.join("vectors-sqlite.db"), &corpus, &queries, k, &truth)?;

    println!(
        "\n{:<28} {:>10} {:>12} {:>10} {:>10} {:>10}",
        "engine", "recall@k", "build", "p50", "p95", "max"
    );
    for outcome in [&ours, &quantized, &theirs] {
        let (p50, p95, max) = percentiles(&outcome.samples);
        println!(
            "{:<28} {:>10.3} {:>12} {:>10} {:>10} {:>10}",
            outcome.label,
            outcome.recall,
            format!("{:.2?}", outcome.build),
            format!("{p50:.2?}"),
            format!("{p95:.2?}"),
            format!("{max:.2?}")
        );
    }

    let recall_loss = ours.recall - quantized.recall;
    println!(
        "\nint8 recall delta vs exact HNSW: {recall_loss:+.3} ({})",
        shape.label()
    );
    println!(
        "file bytes: exact={} int8={} ({:.2}x smaller)",
        ours.file_bytes,
        quantized.file_bytes,
        ours.file_bytes as f64 / quantized.file_bytes.max(1) as f64
    );
    if let (Some(exact), Some(int8)) = (ours.resident_vector_bytes, quantized.resident_vector_bytes)
    {
        println!(
            "resident vector payload: exact={:.1} MiB int8={:.1} MiB ({:.2}x smaller)",
            exact as f64 / 1_048_576.0,
            int8 as f64 / 1_048_576.0,
            exact as f64 / int8.max(1) as f64
        );
    }

    let (ours_p50, ..) = percentiles(&ours.samples);
    let (theirs_p50, ..) = percentiles(&theirs.samples);
    let ratio = theirs_p50.as_secs_f64() / ours_p50.as_secs_f64().max(f64::EPSILON);
    println!(
        "\nInlaySQL's p50 query is {ratio:.2}x {} than sqlite-vec's, at {:.1}% of its recall.",
        if ratio >= 1.0 { "faster" } else { "slower" },
        ours.recall * 100.0
    );

    // The filtered cases, measured against the same oracle restricted to one
    // bucket each. This is the question AHL-379 asks: what does pushing a
    // `WHERE` into the probe cost, and how much recall survives it?
    for (label, filtered) in [
        ("~10%", &ours_moderate),
        ("~1%", &ours_selective),
        ("~0.1%", &ours_pathological),
    ] {
        let (p50, p95, max) = percentiles(&filtered.samples);
        println!("\n=== filtered ({label} of rows per bucket) ===");
        println!(
            "{:<28} {:>10} {:>10} {:>10} {:>10}",
            "engine", "recall@k", "p50", "p95", "max"
        );
        println!(
            "{:<28} {:>10.3} {:>10} {:>10} {:>10}",
            filtered.label,
            filtered.recall,
            format!("{p50:.2?}"),
            format!("{p95:.2?}"),
            format!("{max:.2?}")
        );
    }

    if auxiliary {
        incremental_maintenance(&corpus);
        paged_memory(&corpus, shape);
        paged_quantization(dir, &corpus, shape)?;
    }
    Ok(())
}

/// A corpus whose embeddings exceed a stated memory budget is searchable, with
/// resident memory bounded and measured.
///
/// The in-RAM `HnswIndex` holds every embedding and its normalised copy. The
/// paged backend (`inlaysql_core::hnsw_paged`) holds at most `CACHE_NODES`
/// decoded nodes and reads the rest through storage on demand. This drives it
/// over a corpus orders of magnitude larger than that cache and reports the
/// resident working set as a *count* — the project's convention, because a
/// count survives a noisy machine where a wall-clock RSS reading would not.
///
/// This is the "corpus larger than RAM" case the benchmark suite publishes: the
/// number that matters is the cache bound versus the corpus bytes, and the
/// recall the graph still delivers while paying it.
fn paged_memory(corpus: &[Vector], shape: Shape) {
    use inlaysql_core::hnsw_paged::PagedHnswIndex;
    use inlaysql_core::mem::MemStorage;
    use inlaysql_core::VectorIndex;

    const CACHE_NODES: usize = 256;

    let dim = corpus[0].1.len();
    let mut index =
        PagedHnswIndex::new(MemStorage::new(), "bench", dim).with_cache_capacity(CACHE_NODES);
    for (id, embedding) in corpus {
        index.insert(*id, embedding).unwrap();
    }
    index.commit().unwrap();

    // Recall against the same exhaustive oracle this suite already computes,
    // and the peak cache the queries actually touched.
    let mut rng = SeededRng::new(0x5a17_0000_0000_0000);
    let mut next = || (rng.next_u64() >> 40) as f32 / 16_777_216.0 - 0.5;
    let mut recall_sum = 0.0;
    let mut peak_cache = 0usize;
    for _ in 0..25 {
        let query: Vec<f32> = (0..dim).map(|_| next()).collect();
        let truth = exact_top_k(corpus, &query, 10);
        let hits = index.search(&query, 10, None).unwrap();
        let got: Vec<u64> = hits.iter().map(|hit| hit.id).collect();
        recall_sum += recall(&got, &truth);
        peak_cache = peak_cache.max(index.cache_len());
    }

    let corpus_bytes = corpus.len() * dim * 4;
    let working_set_bytes = peak_cache * dim * 4;
    println!("\n=== corpus larger than RAM: paged HNSW (direct) ===");
    println!(
        "corpus: {} vectors x dim {dim} = {:.1} MiB of f32",
        corpus.len(),
        corpus_bytes as f64 / (1_048_576.0)
    );
    println!(
        "cache bound: {CACHE_NODES} nodes (~{:.1} MiB working set); peak resident: {peak_cache} nodes",
        working_set_bytes as f64 / (1_048_576.0)
    );
    println!(
        "resident / corpus: {peak_cache} / {} nodes ({:.2}% held in memory)",
        corpus.len(),
        100.0 * peak_cache as f64 / corpus.len() as f64
    );
    println!(
        "recall@10 vs exhaustive: {:.3} ({})",
        recall_sum / 25.0,
        shape.label()
    );
}

/// The paged backend's own quantisation payoff: real files on disk, exact
/// against int8, over `Database::open_paged`.
///
/// This is the paged-index counterpart to the "file bytes" / "resident vector
/// payload" numbers `run_shape` already prints for the in-memory `HnswIndex`
/// (see [`inlaysql_vectors`]) — the number PLAN.md's "Retrieval — extend the
/// moat" section and BENCHMARK.md's quantisation section want for the paged
/// path, measured the same way: a real `.inlay` file, not a synthetic byte
/// count.
fn paged_quantization(
    dir: &Path,
    corpus: &[Vector],
    shape: Shape,
) -> Result<(), Box<dyn std::error::Error>> {
    let dim = corpus[0].1.len();
    let exact_path = dir.join("paged-exact.inlay");
    let quantized_path = dir.join("paged-int8.inlay");

    let (exact_file_bytes, exact_resident) = paged_build(&exact_path, corpus, dim, false)?;
    let (quantized_file_bytes, quantized_resident) =
        paged_build(&quantized_path, corpus, dim, true)?;

    println!(
        "\n=== paged HNSW quantisation: exact vs int8 ({}) ===",
        shape.label()
    );
    println!(
        "file bytes: exact={exact_file_bytes} int8={quantized_file_bytes} ({:.2}x smaller)",
        exact_file_bytes as f64 / quantized_file_bytes.max(1) as f64
    );
    if let (Some(exact), Some(int8)) = (exact_resident, quantized_resident) {
        println!(
            "resident cache payload: exact={:.1} MiB int8={:.1} MiB ({:.2}x smaller)",
            exact as f64 / 1_048_576.0,
            int8 as f64 / 1_048_576.0,
            exact as f64 / int8.max(1) as f64
        );
    }
    Ok(())
}

/// Build a paged index over `corpus` at `path`, query it once so the cache is
/// warm, and report the file's size on disk plus its resident cache bytes.
fn paged_build(
    path: &Path,
    corpus: &[Vector],
    dim: usize,
    quantized: bool,
) -> Result<(u64, Option<usize>), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let mut db = Database::open_paged(path)?;
    let vector_type = if quantized {
        format!("VECTOR({dim}, INT8)")
    } else {
        format!("VECTOR({dim})")
    };
    db.execute(
        &format!("CREATE TABLE vecs (id INTEGER PRIMARY KEY, embedding {vector_type})"),
        &[],
    )?;
    db.execute("CREATE INDEX vecs_embedding ON vecs (embedding)", &[])?;

    crate::batched(&mut db, corpus.len(), |db, index| {
        let (id, embedding) = &corpus[index];
        db.execute(
            "INSERT INTO vecs (id, embedding) VALUES (?, ?)",
            &[Value::Integer(*id as i64), Value::Vector(embedding.clone())],
        )?;
        Ok(())
    })?;

    // A node's cache entry is written when it is inserted, not only when a
    // query later touches it (see `PagedHnswIndex::store_node`), so a corpus
    // that fits under `DEFAULT_CACHE_NODES` is already fully resident here —
    // no warm-up queries needed. One query all the same, so the number
    // reported is "what a caller sees after using the index", and so both
    // configurations go through the identical procedure.
    let sql =
        "SELECT id, vector_score(embedding, ?) AS score FROM vecs ORDER BY score DESC LIMIT 1";
    db.query(sql, &[Value::Vector(corpus[0].1.clone())])?;

    let resident_vector_bytes = db.vector_index_resident_bytes("vecs", "embedding");
    db.checkpoint()?;
    let file_bytes = std::fs::metadata(path)?.len();
    drop(db);
    let _ = std::fs::remove_file(path);
    Ok((file_bytes, resident_vector_bytes))
}

/// Incremental ANN maintenance, measured directly against [`HnswIndex`].
///
/// The SQL path above cannot show this: the engine defers index commits to the
/// first read, so a load there pays one full build whatever the maintenance
/// cost is. This section builds the graph once, then inserts rows one at a
/// time, committing each, and reports the per-row cost — and, because the
/// AHL-381 guarantee is a *count* rather than a wall-clock number, how many
/// distance computations each approach makes. A full rebuild re-inserts every
/// node (roughly `n` inserts' worth of computations); one incremental insert
/// must stay bounded by `ef_construction * M`, independent of `n`.
fn incremental_maintenance(corpus: &[Vector]) {
    use inlaysql_core::hnsw::HnswIndex;
    use inlaysql_core::VectorIndex;

    const EXTRA: usize = 100;

    let dim = corpus[0].1.len();
    let started = Instant::now();
    let mut index = HnswIndex::new(dim);
    for (id, embedding) in corpus {
        index.insert(*id, embedding).unwrap();
    }
    index.commit().unwrap();
    let full_build = started.elapsed();
    let full_build_calls = index.distance_calls();

    // One insert, counted: reset the counter so the full build's billions of
    // computations do not mask the single insert we care about.
    let probe = corpus.last().expect("non-empty corpus");
    index.reset_distance_calls();
    index.insert(probe.0 + 1, &probe.1).unwrap();
    index.commit().unwrap();
    let insert_calls = index.distance_calls();

    let start_id = corpus.len() as u64 + 2;
    let timing = Instant::now();
    for i in 0..EXTRA {
        let embedding = corpus[i % corpus.len()].1.clone();
        index.insert(start_id + i as u64, &embedding).unwrap();
        index.commit().unwrap();
    }
    let per_row = timing.elapsed() / EXTRA as u32;

    println!("\n=== incremental ANN maintenance (HNSW, direct) ===");
    println!(
        "full rebuild of {} nodes: {full_build:.2?} ({full_build_calls} distance computations)",
        corpus.len()
    );
    println!(
        "one incremental insert + commit: {per_row:.2?} ({insert_calls} distance computations)"
    );
}

/// A deterministic corpus and query set: `(row id, embedding)` pairs.
pub fn corpus(config: &Config, shape: Shape) -> (Vec<Vector>, Vec<Vec<f32>>) {
    match shape {
        Shape::Uniform => uniform_corpus(config),
        Shape::Text => text_corpus(config),
    }
}

/// Uniformly random unit vectors: the ANN worst case. See the module note.
///
/// The divisor is the interesting part. The shift takes 24 bits, so it has to
/// be `2^24` for a component in `[-0.5, 0.5)`. It was `2^23` until AHL-372,
/// which put every component in `[-0.5, 1.5)` — a mean of `+0.5` on every
/// axis, so the corpus leaned on the all-ones diagonal at a mean pairwise
/// cosine of 0.43. It was neither uniform, as the comment claimed, nor
/// realistic, and the recall published against it was measuring an accident.
fn uniform_corpus(config: &Config) -> (Vec<Vector>, Vec<Vec<f32>>) {
    let mut rng = SeededRng::new(config.seed);
    let mut vector = |dim: usize| -> Vec<f32> {
        (0..dim)
            .map(|_| (rng.next_u64() >> 40) as f32 / 16_777_216.0 - 0.5)
            .collect()
    };
    let corpus = (1..=config.docs as u64)
        .map(|id| (id, vector(config.dim)))
        .collect();
    let queries = (0..config.queries).map(|_| vector(config.dim)).collect();
    (corpus, queries)
}

/// Embeddings of synthetic documents, so the corpus clusters the way a real
/// one does.
///
/// The same generator the `retrieval` suite and `bench/compare.sh` already
/// use, which is the point: it is the project's existing stand-in for real
/// embeddings, not a shape invented to flatter the index.
fn text_corpus(config: &Config) -> (Vec<Vector>, Vec<Vec<f32>>) {
    let mut rng = SeededRng::new(config.seed);
    let document = |rng: &mut SeededRng, length: usize| -> String {
        (0..length)
            .map(|_| VOCABULARY[(rng.next_u64() % VOCABULARY.len() as u64) as usize])
            .collect::<Vec<_>>()
            .join(" ")
    };

    let mut corpus = Vec::with_capacity(config.docs);
    for id in 1..=config.docs as u64 {
        let length = 12 + (rng.next_u64() % 24) as usize;
        let body = document(&mut rng, length);
        corpus.push((id, hashed_embedding(&body, config.dim)));
    }
    let mut queries = Vec::with_capacity(config.queries);
    for _ in 0..config.queries {
        let length = 2 + (rng.next_u64() % 3) as usize;
        let body = document(&mut rng, length);
        queries.push(hashed_embedding(&body, config.dim));
    }
    (corpus, queries)
}

/// The true top-k by cosine similarity, ties broken by row id.
pub fn exact_top_k(corpus: &[Vector], query: &[f32], k: usize) -> Vec<u64> {
    let mut scored: Vec<(f32, u64)> = corpus
        .iter()
        .map(|(id, embedding)| (cosine_similarity(query, embedding), *id))
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    scored.into_iter().take(k).map(|(_, id)| id).collect()
}

/// The true top-k for a filter: [`exact_top_k`] restricted to the rows whose
/// tenant bucket is `bucket` — `tenant = id % TENANTS`, bucketed by
/// `tenant % buckets`, which is `id % buckets` because `buckets` divides
/// `TENANTS`.
///
/// Deliberately NOT filtered to `id % buckets == bucket` directly: this is
/// the same arithmetic the SQL side evaluates, so a mismatch between the two
/// would show up as a recall number rather than a silent disagreement.
fn exact_filtered_top_k(
    corpus: &[Vector],
    buckets: u64,
    bucket: u64,
    query: &[f32],
    k: usize,
) -> Vec<u64> {
    let mut scored: Vec<(f32, u64)> = corpus
        .iter()
        .filter(|(id, _)| id % TENANTS % buckets == bucket)
        .map(|(id, embedding)| (cosine_similarity(query, embedding), *id))
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    scored.into_iter().take(k).map(|(_, id)| id).collect()
}

/// Fraction of `truth` present in `got`.
pub fn recall(got: &[u64], truth: &[u64]) -> f64 {
    if truth.is_empty() {
        return 1.0;
    }
    got.iter().filter(|id| truth.contains(id)).count() as f64 / truth.len() as f64
}

fn inlaysql_vectors(
    path: &PathBuf,
    corpus: &[Vector],
    queries: &[Vec<f32>],
    k: usize,
    truth: &[Vec<u64>],
    quantized: bool,
) -> Result<(Outcome, Outcome, Outcome, Outcome), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let dim = corpus[0].1.len();
    let mut db = Database::open(path)?;
    let vector_type = if quantized {
        format!("VECTOR({dim}, INT8)")
    } else {
        format!("VECTOR({dim})")
    };
    db.execute(
        &format!(
            "CREATE TABLE vecs (id INTEGER PRIMARY KEY, tenant INTEGER, embedding {vector_type})"
        ),
        &[],
    )?;
    db.execute("CREATE INDEX vecs_embedding ON vecs (embedding)", &[])?;

    let started = Instant::now();
    crate::batched(&mut db, corpus.len(), |db, index| {
        let (id, embedding) = &corpus[index];
        db.execute(
            "INSERT INTO vecs (id, tenant, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(*id as i64),
                Value::Integer((id % TENANTS) as i64),
                Value::Vector(embedding.clone()),
            ],
        )?;
        Ok(())
    })?;
    // The graph is built on the first read, so the first query would otherwise
    // carry the whole build cost as an outlier.
    let sql = format!(
        "SELECT id, vector_score(embedding, ?) AS score FROM vecs ORDER BY score DESC LIMIT {k}"
    );
    db.query(&sql, &[Value::Vector(queries[0].clone())])?;
    let build = started.elapsed();

    let mut samples = Vec::with_capacity(queries.len());
    let mut total_recall = 0.0;
    for (query, truth) in queries.iter().zip(truth) {
        let at = Instant::now();
        let rows = db.query(&sql, &[Value::Vector(query.clone())])?;
        samples.push(at.elapsed());
        total_recall += recall(&row_ids(&rows), truth);
    }

    // The filtered passes reuse the same index. Each query is pinned to a
    // bucket (`tenant % BUCKETS`) and scored against that bucket's own
    // exhaustive top-k, so recall measures the filter, not the approximation.
    let moderate = filtered_pass(&mut db, corpus, queries, k, TENANTS / 100, quantized)?;
    let selective = filtered_pass(&mut db, corpus, queries, k, TENANTS / 10, quantized)?;
    let pathological = filtered_pass(&mut db, corpus, queries, k, TENANTS, quantized)?;

    let resident_vector_bytes = db.vector_index_resident_bytes("vecs", "embedding");
    let file_bytes = std::fs::metadata(path)?.len();
    drop(db);
    let _ = std::fs::remove_file(path);
    Ok((
        Outcome {
            label: if quantized {
                "InlaySQL (HNSW int8)"
            } else {
                "InlaySQL (HNSW exact)"
            },
            build,
            samples,
            recall: total_recall / queries.len() as f64,
            file_bytes,
            resident_vector_bytes,
        },
        moderate,
        selective,
        pathological,
    ))
}

/// One filtered pass: `WHERE tenant % buckets = ?` on a corpus whose tenant
/// column holds `id % TENANTS`, so each bucket owns exactly `TENANTS /
/// buckets` of the tenants and the bucket a query lands in is a pure function
/// of its row id. Scored against the bucket's own exhaustive top-k.
fn filtered_pass(
    db: &mut Database,
    corpus: &[Vector],
    queries: &[Vec<f32>],
    k: usize,
    buckets: u64,
    quantized: bool,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let filtered_sql = format!(
        "SELECT id, vector_score(embedding, ?) AS score FROM vecs WHERE tenant % ? = ? ORDER BY score DESC LIMIT {k}"
    );
    let mut samples = Vec::with_capacity(queries.len());
    let mut filtered_recall = 0.0;
    for (index, query) in queries.iter().enumerate() {
        let bucket = (index as u64) % buckets;
        let truth = exact_filtered_top_k(corpus, buckets, bucket, query, k);
        let at = Instant::now();
        let rows = db.query(
            &filtered_sql,
            &[
                Value::Vector(query.clone()),
                Value::Integer(buckets as i64),
                Value::Integer(bucket as i64),
            ],
        )?;
        samples.push(at.elapsed());
        filtered_recall += recall(&row_ids(&rows), &truth);
    }
    Ok(Outcome {
        label: if quantized {
            "InlaySQL (HNSW int8, filtered)"
        } else {
            "InlaySQL (HNSW exact, filtered)"
        },
        build: Duration::ZERO,
        samples,
        recall: filtered_recall / queries.len() as f64,
        file_bytes: 0,
        resident_vector_bytes: None,
    })
}

fn row_ids(rows: &inlaysql::ResultSet) -> Vec<u64> {
    rows.rows
        .iter()
        .filter_map(|row| match row[0] {
            Value::Integer(id) => Some(id as u64),
            _ => None,
        })
        .collect()
}

fn sqlite_vec_vectors(
    path: &PathBuf,
    corpus: &[Vector],
    queries: &[Vec<f32>],
    k: usize,
    truth: &[Vec<u64>],
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let dim = corpus[0].1.len();

    // SAFETY: registering an auto-extension mutates SQLite's global extension
    // list, which is not thread-safe. This is a single-threaded benchmark
    // binary and the call happens before any connection is opened.
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    }

    let conn = rusqlite::Connection::open(path)?;
    // Cosine, to match InlaySQL's metric — otherwise the two engines would be
    // answering different questions and the recall column would be a fiction.
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE vecs USING vec0(embedding float[{dim}] distance_metric=cosine)"
    ))?;

    let started = Instant::now();
    let transaction = conn.unchecked_transaction()?;
    {
        let mut insert =
            transaction.prepare("INSERT INTO vecs (rowid, embedding) VALUES (?1, ?2)")?;
        for (id, embedding) in corpus {
            insert.execute(rusqlite::params![*id as i64, as_blob(embedding)])?;
        }
    }
    transaction.commit()?;
    let build = started.elapsed();

    let mut samples = Vec::with_capacity(queries.len());
    let mut total_recall = 0.0;
    let mut select = conn.prepare(&format!(
        "SELECT rowid FROM vecs WHERE embedding MATCH ?1 ORDER BY distance LIMIT {k}"
    ))?;
    for (query, truth) in queries.iter().zip(truth) {
        let at = Instant::now();
        let ids: Vec<u64> = select
            .query_map([as_blob(query)], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<i64>, _>>()?
            .into_iter()
            .map(|id| id as u64)
            .collect();
        samples.push(at.elapsed());
        total_recall += recall(&ids, truth);
    }

    drop(select);
    drop(conn);
    let file_bytes = std::fs::metadata(path)?.len();
    let _ = std::fs::remove_file(path);
    Ok(Outcome {
        label: "sqlite-vec (exhaustive)",
        build,
        samples,
        recall: total_recall / queries.len() as f64,
        file_bytes,
        resident_vector_bytes: None,
    })
}

/// `sqlite-vec` takes embeddings as little-endian `f32` blobs.
fn as_blob(embedding: &[f32]) -> Vec<u8> {
    embedding
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}
