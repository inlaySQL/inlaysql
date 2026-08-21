//! HNSW parameter sweep: the recall/latency curve behind the shipped defaults.
//!
//! [`vectors`](crate::vectors) answers "how good is the index?" at one point.
//! This suite answers "why that point?" — it walks a grid of `M`,
//! `ef_construction` and `ef_search` over the same corpus, the same queries and
//! the same exhaustive oracle, and prints recall and latency for every
//! combination. A single number cannot justify a default; a curve can.
//!
//! It drives [`HnswIndex`] directly rather than through SQL. The engine's
//! insert path costs milliseconds per row — ten seconds of redb writes per
//! thousand vectors — which would swamp the graph build this suite is trying to
//! measure, and none of it varies with the parameters under test. Recall is
//! identical either way: the same graph answers the same queries.
//!
//! `ef_search` is a query-time knob, so one built graph is measured at every
//! `ef_search` in the grid. Only `M` and `ef_construction` force a rebuild.
//!
//! ```sh
//! cargo run --release -p inlaysql-bench -- --suite sweep --docs 20000
//! ```

use std::time::{Duration, Instant};

use inlaysql_core::hnsw::{HnswIndex, HnswParams};
use inlaysql_core::VectorIndex;

use crate::vectors::{corpus, exact_top_k, recall, Shape};
use crate::{percentiles, Config};

/// Graph-shaping parameters. Each entry costs one build.
const GRID_M: &[usize] = &[16, 32];
const GRID_EF_CONSTRUCTION: &[usize] = &[100, 200, 400];
/// Query-time candidate list, measured against every built graph.
const GRID_EF_SEARCH: &[usize] = &[64, 128, 256, 512, 1024, 2048];

pub fn run(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    for shape in [Shape::Text, Shape::Uniform] {
        run_shape(config, shape)?;
    }
    Ok(())
}

fn run_shape(config: &Config, shape: Shape) -> Result<(), Box<dyn std::error::Error>> {
    let (corpus, queries) = corpus(config, shape);
    let k = config.limit;
    let truth: Vec<Vec<u64>> = queries.iter().map(|q| exact_top_k(&corpus, q, k)).collect();

    println!(
        "\n=== hnsw sweep: {} vectors, dim {}, {} queries, top-{k} ===\n=== corpus: {} ===",
        corpus.len(),
        config.dim,
        queries.len(),
        shape.label()
    );
    println!(
        "\n{:>4} {:>7} {:>7} {:>10} {:>10} {:>10} {:>10}",
        "M", "efC", "efS", "recall@k", "build", "p50", "p95"
    );

    for &m in GRID_M {
        for &ef_construction in GRID_EF_CONSTRUCTION {
            let mut params = HnswParams {
                m,
                ef_construction,
                ef_search: GRID_EF_SEARCH[0],
                // Fixed at 1 so the swept `ef_search` is the whole story; the
                // shipped default scales it with `k` instead.
                ef_search_multiplier: 1,
            };
            let started = Instant::now();
            let mut index = HnswIndex::with_params(config.dim, params);
            for (id, embedding) in &corpus {
                index.insert(*id, embedding)?;
            }
            index.commit()?;
            let build = started.elapsed();

            for &ef_search in GRID_EF_SEARCH {
                params.ef_search = ef_search;
                index.set_params(params);
                let (score, samples) = measure(&index, &queries, &truth, k)?;
                let (p50, p95, _) = percentiles(&samples);
                println!(
                    "{m:>4} {ef_construction:>7} {ef_search:>7} {score:>10.3} \
                     {:>10} {:>10} {:>10}",
                    format!("{build:.2?}"),
                    format!("{p50:.2?}"),
                    format!("{p95:.2?}")
                );
            }
        }
    }

    println!(
        "\nBuild time is per (M, efC) row; the four efS rows under it share one \
         graph.\nShipped default: M={}, efC={}, efS=max({}, k*{}).",
        HnswParams::DEFAULT.m,
        HnswParams::DEFAULT.ef_construction,
        HnswParams::DEFAULT.ef_search,
        HnswParams::DEFAULT.ef_search_multiplier
    );
    Ok(())
}

/// Mean recall and per-query latency for one configuration.
fn measure(
    index: &HnswIndex,
    queries: &[Vec<f32>],
    truth: &[Vec<u64>],
    k: usize,
) -> Result<(f64, Vec<Duration>), Box<dyn std::error::Error>> {
    let mut samples = Vec::with_capacity(queries.len());
    let mut total = 0.0;
    for (query, truth) in queries.iter().zip(truth) {
        let at = Instant::now();
        let hits = index.search(query, k, None)?;
        samples.push(at.elapsed());
        let ids: Vec<u64> = hits.into_iter().map(|hit| hit.id).collect();
        total += recall(&ids, truth);
    }
    Ok((total / queries.len() as f64, samples))
}
