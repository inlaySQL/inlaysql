//! Where a vector query's time actually goes.
//!
//! `PLAN.md`'s W4 assumes the answer is the distance kernel and prescribes
//! "SIMD distance kernels (NEON/AVX-512 behind a leaf crate)". Before writing
//! any intrinsics this measures the two quantities that decide whether that
//! would help at all: how many distance computations one query performs, and
//! how much of the query's wall clock those computations account for.
//!
//! Run it deliberately, in release, and read the numbers:
//!
//! ```sh
//! cargo test --release -p inlaysql-core --test vector_query_cost -- --nocapture --ignored
//! ```
//!
//! It is `#[ignore]`d because it is an instrument, not an assertion: it prints
//! a measurement and passes as long as the index answers at all. A timing
//! threshold here would fail on a busy machine and teach everyone to ignore it.

use std::time::Instant;

use inlaysql_core::hnsw::HnswIndex;
use inlaysql_core::traits::VectorIndex;

const VECTORS: usize = 2_000;
const DIM: usize = 384;
const QUERIES: usize = 100;
const K: usize = 10;

/// Deterministic unit-norm vectors, uniformly random in direction.
///
/// This is the ANN worst case on purpose — `bench/README.md`'s "two corpora"
/// section explains why: random directions in 384 dimensions concentrate, so
/// there is no downhill for a graph to walk, and any parameter that survives
/// here survives on real embeddings. Returns `count` of them so the caller can
/// hold some back: querying with a vector that is *in* the index is a much
/// easier question than the one an application asks, and grading recall on it
/// flatters every setting equally.
fn vectors(count: usize) -> Vec<Vec<f32>> {
    let mut state = 0x243f_6a88_85a3_08d3u64;
    let mut roll = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..count)
        .map(|_| {
            let raw: Vec<f32> = (0..DIM)
                .map(|_| (roll() % 2_000) as f32 / 1_000.0 - 1.0)
                .collect();
            let norm = raw
                .iter()
                .map(|x| x * x)
                .sum::<f32>()
                .sqrt()
                .max(f32::EPSILON);
            raw.into_iter().map(|x| x / norm).collect()
        })
        .collect()
}

/// A copy of `hnsw::distance`'s inner loop, minus the call counter and the
/// `1.0 -` at the end. Kept here rather than called through the crate because
/// the point is to time the arithmetic on its own, with no index around it.
fn lane_dot(a: &[f32], b: &[f32]) -> f32 {
    const LANES: usize = 8;
    let mut lanes = [0.0f32; LANES];
    let (left, left_rem) = a.as_chunks::<LANES>();
    let (right, right_rem) = b.as_chunks::<LANES>();
    for (x, y) in left.iter().zip(right) {
        for lane in 0..LANES {
            lanes[lane] += x[lane] * y[lane];
        }
    }
    let mut dot = 0.0f32;
    for lane in lanes {
        dot += lane;
    }
    for (x, y) in left_rem.iter().zip(right_rem) {
        dot += x * y;
    }
    dot
}

/// The same loop written with fused multiply-add.
///
/// This is the one kernel-level idea the assembly leaves on the table: the
/// compiled loop issues a separate `fmul.4s` and `fadd.4s` per lane group,
/// where `fmla.4s` would do both. Rust will not fuse them on its own, because
/// FMA rounds once where multiply-then-add rounds twice and the results differ
/// — so this is not a free rewrite, it is a change to what the index computes,
/// and worth knowing the size of before anyone trades recall reproducibility
/// for it.
fn fused_dot(a: &[f32], b: &[f32]) -> f32 {
    const LANES: usize = 8;
    let mut lanes = [0.0f32; LANES];
    let (left, left_rem) = a.as_chunks::<LANES>();
    let (right, right_rem) = b.as_chunks::<LANES>();
    for (x, y) in left.iter().zip(right) {
        for lane in 0..LANES {
            lanes[lane] = x[lane].mul_add(y[lane], lanes[lane]);
        }
    }
    let mut dot = 0.0f32;
    for lane in lanes {
        dot += lane;
    }
    for (x, y) in left_rem.iter().zip(right_rem) {
        dot = x.mul_add(*y, dot);
    }
    dot
}

/// Time `kernel` over `pairs` vector pairs, `QUERIES` times.
fn time(
    corpus: &[Vec<f32>],
    pairs: usize,
    kernel: fn(&[f32], &[f32]) -> f32,
) -> std::time::Duration {
    let mut sink = 0.0f32;
    let started = Instant::now();
    for _ in 0..QUERIES {
        for pair in 0..pairs {
            sink += kernel(&corpus[pair % VECTORS], &corpus[(pair + 1) % VECTORS]);
        }
    }
    let elapsed = started.elapsed();
    assert!(sink.is_finite(), "the compiler kept the dot products");
    elapsed / QUERIES as u32
}

#[test]
#[ignore = "an instrument, not an assertion — run it with --nocapture"]
fn where_a_vector_query_spends_its_time() {
    let all = vectors(VECTORS + QUERIES);
    let corpus = &all[..VECTORS];
    let queries: Vec<&Vec<f32>> = all[VECTORS..].iter().collect();

    let mut index = HnswIndex::new(DIM);
    for (row, vector) in corpus.iter().enumerate() {
        index.insert(row as u64 + 1, vector).unwrap();
    }
    index.commit().unwrap();

    // Warm every page and branch predictor the real measurement will use.
    for query in &queries {
        index.search(query, K, None).unwrap();
    }

    let before = index.distance_calls();
    let started = Instant::now();
    for query in &queries {
        index.search(query, K, None).unwrap();
    }
    let elapsed = started.elapsed();
    let calls = index.distance_calls() - before;

    let per_query = elapsed / QUERIES as u32;
    let calls_per_query = calls as f64 / QUERIES as f64;

    // What the dot products alone cost. This has to be the *same* kernel the
    // index uses, not a naive one: `hnsw::distance` sums into eight explicit
    // accumulators precisely so the compiler may vectorise it, and a reference
    // written as a single `map(..).sum()` measures the scalar loop the engine
    // deliberately does not run — it comes out slower than the whole query,
    // which says something about the reference and nothing about the engine.
    //
    // The gap between this and the query time is everything that is not
    // arithmetic: graph traversal, candidate heaps, visited sets, node
    // fetches.
    let pairs = calls_per_query.round() as usize;
    let arithmetic = time(corpus, pairs, lane_dot);
    let fused = time(corpus, pairs, fused_dot);

    let share = arithmetic.as_secs_f64() / per_query.as_secs_f64() * 100.0;
    let fused_share = fused.as_secs_f64() / per_query.as_secs_f64() * 100.0;

    println!();
    println!("vectors={VECTORS} dim={DIM} k={K} queries={QUERIES}");
    println!("query p_mean            {per_query:.2?}");
    println!("distance calls / query  {calls_per_query:.0}");
    println!("dot products alone      {arithmetic:.2?}  ({share:.0}% of the query)");
    println!("the same, fused (fmla)  {fused:.2?}  ({fused_share:.0}% of the query)");
    println!();
    println!(
        "The remaining {:.0}% is not arithmetic. Any kernel work — intrinsics, \
         fusion, a wider unroll — can only ever attack the {share:.0}%, and \
         switching to fmla would take {:.0}% off the whole query while changing \
         what the index computes.",
        100.0 - share,
        share - fused_share
    );

    // The other half of the question. 1,300-odd distance calls over a corpus
    // of 2,000 is a graph barely beating the brute-force scan it replaced, so
    // the cheaper lever is doing fewer of them rather than doing each faster.
    // `ef_search` is the dial; recall is what it costs.
    println!();
    println!(
        "{:>10}  {:>10}  {:>8}  {:>10}",
        "ef_search", "calls/query", "recall", "p_mean"
    );
    let exact = exhaustive(corpus, &queries, K);
    for ef_search in [8, 16, 32, 64, 128] {
        let mut params = index.params();
        params.ef_search = ef_search;
        // Only the search dial moves: the graph is not rebuilt, so every row
        // here is the same index answering with a different budget.
        index.set_params(params);

        for query in &queries {
            index.search(query, K, None).unwrap();
        }
        let before = index.distance_calls();
        let started = Instant::now();
        let mut found = 0usize;
        for (query, truth) in queries.iter().zip(&exact) {
            let hits = index.search(query, K, None).unwrap();
            found += hits.iter().filter(|hit| truth.contains(&hit.id)).count();
        }
        let elapsed = started.elapsed() / QUERIES as u32;
        let calls = (index.distance_calls() - before) as f64 / QUERIES as f64;
        let recall = found as f64 / (QUERIES * K) as f64;
        println!("{ef_search:>10}  {calls:>11.0}  {recall:>8.3}  {elapsed:>10.2?}");
    }
}

/// The true `k` nearest by exhaustive cosine distance — the oracle recall is
/// measured against, so a recall number here is not the index grading itself.
fn exhaustive(corpus: &[Vec<f32>], queries: &[&Vec<f32>], k: usize) -> Vec<Vec<u64>> {
    queries
        .iter()
        .map(|query| {
            let mut scored: Vec<(f32, u64)> = corpus
                .iter()
                .enumerate()
                .map(|(row, vector)| (1.0 - lane_dot(query, vector), row as u64 + 1))
                .collect();
            scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
            scored.into_iter().take(k).map(|(_, id)| id).collect()
        })
        .collect()
}
