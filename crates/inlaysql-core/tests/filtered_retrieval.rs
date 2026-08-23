//! Filtered retrieval: a restrictive `WHERE` must not under-fill a `LIMIT`.
//!
//! A retrieval query (`vector_score`, `bm25_score`, `fuse`) ranks the whole
//! corpus, so a `WHERE` applied to its output can discard every candidate.
//! The engine fixes this by pushing the filter *into* the retriever — see
//! `Engine::retrieve_filtered` — and these tests pin the behaviour down
//! against an in-memory environment whose vector index is exact (brute
//! force), so the "correct top-k for the filter" can be asserted exactly
//! rather than by recall.

use inlaysql_core::bm25::Bm25Index;
use inlaysql_core::fusion::{reciprocal_rank_fusion, DEFAULT_RRF_K};
use inlaysql_core::mem::cosine_similarity;
use inlaysql_core::{mem, Engine, FullTextIndex, ResultSet, Rng, Scored, Value};

const DIM: usize = 4;

/// One corpus row: the `id` column, its `tenant`, and its embedding.
type Row = (i64, i64, Vec<f32>);

/// Build an engine with `docs` rows: `tenant = id % tenants`, a deterministic
/// pseudo-random embedding, and a short text body. Returns the engine, the
/// `(id, tenant, embedding)` corpus, and a query embedding.
fn build(docs: usize, tenants: usize, seed: u64) -> (Engine, Vec<Row>, Vec<f32>) {
    let mut rng = mem::SeededRng::new(seed);
    let mut vector = || -> Vec<f32> {
        (0..DIM)
            .map(|_| ((rng.next_u64() >> 33) as i16) as f32 / 16384.0)
            .collect()
    };
    let embeddings: Vec<Vec<f32>> = (0..docs).map(|_| vector()).collect();
    let query = vector();

    let mut engine = mem::engine().expect("open in-memory engine");
    engine
        .execute(
            &format!(
                "CREATE TABLE docs (id INTEGER, tenant INTEGER, body TEXT, embedding VECTOR({DIM}))"
            ),
            &[],
        )
        .expect("create table");
    engine
        .execute("CREATE INDEX docs_body ON docs (body)", &[])
        .expect("create body index");
    engine
        .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .expect("create embedding index");

    let mut corpus = Vec::with_capacity(docs);
    for (index, embedding) in embeddings.iter().enumerate() {
        let id = (index + 1) as i64;
        let tenant = id % tenants as i64;
        let body = format!("document {id} word{}", id % 4);
        engine
            .execute(
                "INSERT INTO docs (id, tenant, body, embedding) VALUES (?, ?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Integer(tenant),
                    Value::Text(body.into()),
                    Value::Vector(embedding.clone()),
                ],
            )
            .expect("insert");
        corpus.push((id, tenant, embedding.clone()));
    }

    (engine, corpus, query)
}

/// The correct top-k for a filter: exhaustive cosine similarity restricted to
/// `tenant`, ties broken by ascending id — the same total order the engine uses.
fn exact_filtered_top(corpus: &[Row], tenant: i64, query: &[f32], k: usize) -> Vec<i64> {
    let mut scored: Vec<(f32, i64)> = corpus
        .iter()
        .filter(|(_, t, _)| *t == tenant)
        .map(|(id, _, embedding)| (cosine_similarity(query, embedding), *id))
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    scored.into_iter().take(k).map(|(_, id)| id).collect()
}

fn ids(result: &ResultSet) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| row[0].as_i64().expect("id column"))
        .collect()
}

#[test]
fn a_selective_filter_returns_limit_rows_not_nothing() {
    // 1% of the corpus belongs to tenant 7, so a fixed candidate budget of 40
    // contains essentially none of it. The query must still return ten rows.
    let (mut engine, corpus, query) = build(1_000, 100, 42);
    let result = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs WHERE tenant = ? ORDER BY score DESC LIMIT 10",
            &[Value::Vector(query), Value::Integer(7)],
        )
        .expect("filtered vector query");

    assert_eq!(ids(&result).len(), 10);
    assert!(
        corpus
            .iter()
            .filter(|(_, tenant, _)| *tenant == 7)
            .all(|(id, _, _)| ids(&result).contains(id)),
        "a row from another tenant leaked in"
    );
}

#[test]
fn filtered_vector_ranking_matches_the_brute_force_oracle() {
    // Tenant 3 owns 20 of 200 rows; the query asks for the correct top 10 of
    // those, which the exact in-memory index makes deterministic.
    let (mut engine, corpus, query) = build(200, 10, 7);
    let result = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs WHERE tenant = ? ORDER BY score DESC LIMIT 10",
            &[Value::Vector(query.clone()), Value::Integer(3)],
        )
        .expect("filtered vector query");

    assert_eq!(ids(&result), exact_filtered_top(&corpus, 3, &query, 10));
}

#[test]
fn a_filter_that_cannot_be_satisfied_returns_what_matches() {
    // Tenant 4 owns 3 rows, so `LIMIT 10` cannot be satisfied. The query returns
    // the 3 rows — not 0, and not 10 padded with other tenants.
    let (mut engine, corpus, query) = build(30, 10, 11);
    let result = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs WHERE tenant = ? ORDER BY score DESC LIMIT 10",
            &[Value::Vector(query.clone()), Value::Integer(4)],
        )
        .expect("filtered vector query");

    assert_eq!(ids(&result), exact_filtered_top(&corpus, 4, &query, 10));
    assert_eq!(ids(&result).len(), 3);
}

#[test]
fn a_filtered_fuse_query_returns_limit_rows() {
    let (mut engine, corpus, query) = build(1_000, 100, 5);
    let result = engine
        .query(
            "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score FROM docs WHERE tenant = ? ORDER BY score DESC LIMIT 10",
            &[
                Value::Vector(query),
                Value::Text("document word1".to_string().into()),
                Value::Integer(7),
            ],
        )
        .expect("filtered hybrid query");

    assert_eq!(ids(&result).len(), 10);
    assert!(
        corpus
            .iter()
            .filter(|(_, tenant, _)| *tenant == 7)
            .all(|(id, _, _)| ids(&result).contains(id)),
        "a row from another tenant leaked in"
    );
}

#[test]
fn filtered_retrieval_is_deterministic() {
    // The filtered walk has branching control flow; the same query twice must
    // return the same rows, order included.
    let run = |seed: u64| {
        let (mut engine, _, query) = build(500, 100, seed);
        engine
            .query(
                "SELECT id, vector_score(embedding, ?) AS score FROM docs WHERE tenant = ? ORDER BY score DESC LIMIT 10",
                &[Value::Vector(query), Value::Integer(42)],
            )
            .expect("filtered vector query")
            .rows
    };
    assert_eq!(run(99), run(99));
}

#[test]
fn a_permissive_filter_answers_like_the_unfiltered_query() {
    // The tie to the unfiltered path, at engine level: a `WHERE` every row
    // satisfies must return the same rows, in the same order, as the query
    // with no `WHERE` at all. The filtered path is the fast path; this pins
    // it to the slow one.
    let (mut engine, _, query) = build(200, 10, 21);
    let unfiltered = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC LIMIT 10",
            &[Value::Vector(query.clone())],
        )
        .expect("unfiltered vector query");
    let filtered = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs WHERE id = id ORDER BY score DESC LIMIT 10",
            &[Value::Vector(query)],
        )
        .expect("permissively filtered vector query");

    assert_eq!(ids(&filtered), ids(&unfiltered));
}

#[test]
fn a_filter_matching_no_rows_returns_an_empty_result() {
    // The pathological end: the walk must terminate and report nothing, not
    // hang and not fabricate a tenant.
    let (mut engine, _, query) = build(100, 10, 3);
    let result = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs WHERE tenant = ? ORDER BY score DESC LIMIT 10",
            &[Value::Vector(query), Value::Integer(999)],
        )
        .expect("filtered vector query");
    assert!(result.rows.is_empty());
}

#[test]
fn a_filter_matching_one_row_returns_exactly_it() {
    // `WHERE id = 5` pins one row out of the corpus; the answer is that row
    // and only that row — no padding, no under-fill.
    let (mut engine, _, query) = build(100, 10, 4);
    let result = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs WHERE id = ? ORDER BY score DESC LIMIT 10",
            &[Value::Vector(query), Value::Integer(5)],
        )
        .expect("filtered vector query");
    assert_eq!(ids(&result), vec![5]);
}

#[test]
fn filtered_bm25_ranking_matches_the_oracle() {
    // The text side gets the same pushdown: the filtered result must equal
    // the exhaustive BM25 ranking restricted to the tenant, truncated to the
    // limit. Bm25Index is exact, so this pins the whole filtered text path.
    let (mut engine, corpus, _) = build(300, 10, 8);
    let tenant = 3;
    let query_text = "document word1";

    let mut oracle = Bm25Index::new();
    for (id, _, _) in &corpus {
        oracle
            .insert(*id as u64, &format!("document {id} word{}", id % 4))
            .unwrap();
    }
    let expected: Vec<i64> = oracle
        .search(query_text, 10, Some(&|id| Ok((id as i64) % 10 == tenant)))
        .unwrap()
        .into_iter()
        .map(|hit| hit.id as i64)
        .collect();

    let result = engine
        .query(
            "SELECT id, bm25_score(body, ?) AS score FROM docs WHERE tenant = ? ORDER BY score DESC LIMIT 10",
            &[
                Value::Text(query_text.to_string().into()),
                Value::Integer(tenant),
            ],
        )
        .expect("filtered text query");
    assert_eq!(ids(&result), expected);
}

#[test]
fn filtered_fusion_matches_the_rrf_over_exact_filtered_lists() {
    // Both sides of a `fuse` must receive the same filter, and the fused
    // answer must equal reciprocal rank fusion over each side's *exact*
    // filtered candidate list — the same budget the engine uses.
    let (mut engine, corpus, query) = build(300, 10, 8);
    let tenant = 3;
    let query_text = "document word1";

    // The exact vector side: exhaustive cosine over the tenant's rows.
    let mut vector_list: Vec<Scored> = corpus
        .iter()
        .filter(|(_, t, _)| *t == tenant)
        .map(|(id, _, embedding)| Scored::new(*id as u64, cosine_similarity(&query, embedding)))
        .collect();
    vector_list.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });
    vector_list.truncate(40);

    // The exact text side: the same Bm25Index the engine uses.
    let mut oracle = Bm25Index::new();
    for (id, _, _) in &corpus {
        oracle
            .insert(*id as u64, &format!("document {id} word{}", id % 4))
            .unwrap();
    }
    let text_list = oracle
        .search(query_text, 40, Some(&|id| Ok((id as i64) % 10 == tenant)))
        .unwrap();

    let mut fused = reciprocal_rank_fusion(&[vector_list, text_list], DEFAULT_RRF_K);
    fused.truncate(40);
    let expected: Vec<i64> = fused
        .into_iter()
        .take(10)
        .map(|hit| hit.id as i64)
        .collect();

    let result = engine
        .query(
            "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score FROM docs WHERE tenant = ? ORDER BY score DESC LIMIT 10",
            &[
                Value::Vector(query),
                Value::Text(query_text.to_string().into()),
                Value::Integer(tenant),
            ],
        )
        .expect("filtered hybrid query");
    assert_eq!(ids(&result), expected);
}
