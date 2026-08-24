//! How much skipping is left in a BM25 query.
//!
//! The index already skips: a MaxScore walk stops visiting documents whose
//! whole possible score cannot reach the `k`-th best found so far. `PLAN.md`'s
//! R6 asks for the next step, per-block impact bounds (block-max WAND), which
//! refines the same idea from one bound per term to one per block of postings.
//!
//! Whether that is worth building depends on a number nobody had measured: how
//! many documents the current walk still visits. If it already visits a
//! handful, block-max bounds have nothing left to skip and the work belongs
//! elsewhere. The filter is the instrument — it is called once per document the
//! walk considers, so counting the calls counts the visits.
//!
//! ```sh
//! cargo test --release -p inlaysql-core --test bm25_skipping_headroom -- --nocapture --ignored
//! ```
//!
//! **Block-max was then built, measured against this, and reverted.** It moved
//! the flat-vocabulary column from 1,381 visits to 1,380 and cost 6.8% on the
//! BM25 p50, because the per-candidate bound check is dearer than the 0.1% of
//! visits it removed. `PERF.md` section 4 has the numbers and the reason —
//! these documents are 8 to 32 terms long, so term frequencies are almost all
//! 1 or 2 and a block's maximum is the list's maximum. This file stays as the
//! measurement that says so, and as the check anyone repeating the idea should
//! run first.

use core::cell::Cell;

use inlaysql_core::bm25::Bm25Index;
use inlaysql_core::traits::{FullTextIndex, RowFilter};

const DOCS: u64 = 2_000;
const QUERIES: usize = 100;
const K: usize = 10;

/// The benchmark's own vocabulary and document shape, so this measures the
/// corpus `BENCHMARK.md` publishes rather than a friendlier one. It is a flat
/// vocabulary of twenty common words, which is the hard case for any
/// impact-based skipping: every term is in most documents, so every term's
/// `idf` — and therefore every term's bound — is nearly the same, and a bound
/// that does not vary cannot separate a promising document from a hopeless one.
const VOCABULARY: [&str; 20] = [
    "database", "embedded", "vector", "search", "index", "storage", "engine", "query", "rust",
    "async", "cache", "page", "record", "column", "table", "commit", "journal", "recovery",
    "schema", "planner",
];

/// A Zipf-ish vocabulary for contrast: the same twenty words, drawn so the
/// first is in nearly everything and the last is rare. Real text looks like
/// this, and it is where impact bounds have something to work with.
fn skewed(draw: u64) -> &'static str {
    // Roughly 1/rank, normalised over a hundred buckets.
    const BOUNDS: [u64; 20] = [
        30, 45, 55, 63, 69, 74, 78, 82, 85, 88, 90, 92, 94, 95, 96, 97, 98, 99, 100, 100,
    ];
    let pick = draw % 100;
    let index = BOUNDS.iter().position(|bound| pick < *bound).unwrap_or(19);
    VOCABULARY[index]
}

fn build(skew: bool) -> (Bm25Index, Vec<String>) {
    let mut state = 0x243f_6a88_85a3_08d3u64;
    let mut roll = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let word = |draw: u64| -> &'static str {
        if skew {
            skewed(draw)
        } else {
            VOCABULARY[(draw % VOCABULARY.len() as u64) as usize]
        }
    };

    let mut index = Bm25Index::new();
    for id in 1..=DOCS {
        let length = 8 + roll() % 24;
        let body: Vec<&str> = (0..length).map(|_| word(roll())).collect();
        index.insert(id, &body.join(" ")).unwrap();
    }

    // The engine commits an index before the first read that needs it, so a
    // measurement taken without this would be of a state no query ever sees.
    index.commit().unwrap();

    // The benchmark's queries: two to four words from the same vocabulary.
    let queries = (0..QUERIES)
        .map(|_| {
            let terms = 2 + roll() % 3;
            (0..terms)
                .map(|_| word(roll()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    (index, queries)
}

fn visits(index: &Bm25Index, queries: &[String], k: usize) -> f64 {
    let seen = Cell::new(0usize);
    let counting: &RowFilter = &|_| {
        seen.set(seen.get() + 1);
        Ok(true)
    };
    for query in queries {
        index.search(query, k, Some(counting)).unwrap();
    }
    seen.get() as f64 / queries.len() as f64
}

#[test]
#[ignore = "an instrument, not an assertion — run it with --nocapture"]
fn how_many_documents_a_bm25_query_still_visits() {
    println!();
    println!("{DOCS} documents, {QUERIES} queries of 2-4 terms, k={K}");
    println!(
        "{:>28}  {:>14}  {:>14}  {:>10}",
        "corpus", "visits (k=10)", "visits (k=inf)", "skipped"
    );

    for (label, skew) in [
        ("flat vocabulary (the bench)", false),
        ("Zipf-ish vocabulary", true),
    ] {
        let (index, queries) = build(skew);
        let pruned = visits(&index, &queries, K);
        // `usize::MAX` never fills the heap, so there is never a threshold and
        // nothing is ever skipped: the exhaustive walk, and the ceiling on what
        // any better bound could remove.
        let exhaustive = visits(&index, &queries, usize::MAX);
        let skipped = (1.0 - pruned / exhaustive) * 100.0;
        println!("{label:>28}  {pruned:>14.0}  {exhaustive:>14.0}  {skipped:>9.1}%");
    }

    println!();
    println!(
        "The `k=inf` column is every document that matches any query term. What is\n\
         left for block-max bounds to remove is the gap between the two columns,\n\
         minus the {K} the answer needs."
    );
}
