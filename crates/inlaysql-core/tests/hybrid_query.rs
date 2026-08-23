//! End-to-end tests for the hybrid query path, run entirely in memory.
//!
//! These are the deterministic-simulation tests the project rule asks for: the
//! engine is driven through a seeded, in-memory environment with no clock and
//! no filesystem, so a failure here reproduces byte for byte on any machine.

use inlaysql_core::{mem, Engine, ResultSet, Value};

/// A tiny four-dimensional "topic space": database, web, cooking, retrieval.
/// Hand-written vectors keep it obvious which document should be near which
/// query — no embedding model in the loop to explain a surprise away.
const DIM: usize = 4;

struct Doc {
    id: i64,
    body: &'static str,
    embedding: [f32; DIM],
}

const DOCS: &[Doc] = &[
    Doc {
        // Keyword stuffing: tops the BM25 list on term frequency alone, while
        // sitting nowhere near the query in embedding space. The failure mode
        // lexical search has.
        id: 1,
        body: "rust database rust database rust database rust database",
        embedding: [0.0, 0.9, 0.0, 0.1],
    },
    Doc {
        // Sits exactly on the query vector but shares no words with the query.
        // The failure mode vector search has: no lexical anchor at all.
        id: 2,
        body: "storage layer internals and page cache design",
        embedding: [0.6, 0.0, 0.0, 0.8],
    },
    Doc {
        // Second on both lists, first on neither — the genuinely relevant row
        // that only fusion surfaces.
        id: 3,
        body: "embedded database written in rust with vector retrieval",
        embedding: [0.5, 0.0, 0.0, 0.7],
    },
    Doc {
        id: 4,
        body: "cast iron skillet cornbread recipe",
        embedding: [0.0, 0.0, 1.0, 0.0],
    },
    Doc {
        id: 5,
        body: "a web framework for building sites",
        embedding: [0.0, 1.0, 0.0, 0.0],
    },
    // Near-miss neighbours. They carry none of the query terms, so they only
    // affect the vector ranking — which is the point: with a realistic number
    // of plausible neighbours, a lexical-only hit sinks in the vector list
    // instead of sitting just below the top on a five-row corpus.
    Doc {
        id: 6,
        body: "log structured merge tree compaction strategies",
        embedding: [0.4, 0.0, 0.0, 0.6],
    },
    Doc {
        id: 7,
        body: "page cache eviction policies",
        embedding: [0.3, 0.2, 0.1, 0.5],
    },
    Doc {
        id: 8,
        body: "write ahead log replay and crash recovery",
        embedding: [0.2, 0.3, 0.0, 0.4],
    },
];

/// The query embedding: "storage plus retrieval".
const QUERY_EMBEDDING: [f32; DIM] = [0.6, 0.0, 0.0, 0.8];
const QUERY_TEXT: &str = "rust database";

fn open() -> Engine {
    let mut engine = mem::engine().expect("open in-memory engine");
    engine
        .execute(
            "CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR(4))",
            &[],
        )
        .expect("create table");
    engine
        .execute("CREATE INDEX docs_body ON docs (body)", &[])
        .expect("create body index");
    engine
        .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .expect("create embedding index");
    for doc in DOCS {
        engine
            .execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(doc.id),
                    Value::Text(doc.body.to_string().into()),
                    Value::Vector(doc.embedding.to_vec()),
                ],
            )
            .expect("insert");
    }
    engine
}

fn ids(result: &ResultSet) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| row[0].as_i64().expect("id column"))
        .collect()
}

fn vector_only(engine: &mut Engine) -> ResultSet {
    engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs LIMIT 8",
            &[Value::Vector(QUERY_EMBEDDING.to_vec())],
        )
        .expect("vector query")
}

fn text_only(engine: &mut Engine) -> ResultSet {
    engine
        .query(
            "SELECT id, bm25_score(body, ?) AS score FROM docs LIMIT 8",
            &[Value::Text(QUERY_TEXT.to_string().into())],
        )
        .expect("text query")
}

fn hybrid(engine: &mut Engine) -> ResultSet {
    engine
        .query(
            "SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
             FROM docs ORDER BY score DESC LIMIT 3",
            &[
                Value::Vector(QUERY_EMBEDDING.to_vec()),
                Value::Text(QUERY_TEXT.to_string().into()),
            ],
        )
        .expect("hybrid query")
}

#[test]
fn each_retriever_alone_picks_a_different_winner() {
    // Neither retriever finds document 3 on its own; both rank it second.
    let mut engine = open();
    assert_eq!(
        ids(&vector_only(&mut engine))[..2],
        [2, 3],
        "vector ranking"
    );
    assert_eq!(ids(&text_only(&mut engine))[..2], [1, 3], "bm25 ranking");
}

#[test]
fn fusion_promotes_the_document_both_retrievers_agree_on() {
    let mut engine = open();
    let fused = hybrid(&mut engine);

    assert_eq!(fused.columns, vec!["id", "body", "score"]);
    assert_eq!(
        ids(&fused)[0],
        3,
        "expected the doc ranked well by both retrievers to win, got {:?}",
        ids(&fused)
    );
}

#[test]
fn fused_ranking_matches_rank_fusion_over_the_two_sub_queries() {
    let mut engine = open();
    let vector = ids(&vector_only(&mut engine));
    let text = ids(&text_only(&mut engine));
    let fused = ids(&hybrid(&mut engine));

    // The sub-queries ask for every row, so they return exactly the candidate
    // lists the fused query worked from. Recompute RRF over them by hand here;
    // the engine must agree.
    let mut expected: Vec<(i64, f32)> = Vec::new();
    for list in [&vector, &text] {
        for (rank, id) in list.iter().enumerate() {
            let contribution = 1.0 / (60.0 + rank as f32 + 1.0);
            match expected.iter_mut().find(|(other, _)| other == id) {
                Some((_, score)) => *score += contribution,
                None => expected.push((*id, contribution)),
            }
        }
    }
    expected.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let expected: Vec<i64> = expected.into_iter().map(|(id, _)| id).take(3).collect();

    assert_eq!(fused, expected);
}

#[test]
fn scores_come_back_as_a_projected_column() {
    let mut engine = open();
    let fused = hybrid(&mut engine);
    let scores: Vec<f64> = fused
        .rows
        .iter()
        .map(|row| row[2].as_f64().expect("score column"))
        .collect();

    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "not sorted: {scores:?}"
    );
    assert!(
        scores.iter().all(|s| *s > 0.0),
        "no score computed: {scores:?}"
    );
}

#[test]
fn where_and_limit_apply_to_retrieval_results() {
    let mut engine = open();
    let result = engine
        .query(
            "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
             FROM docs WHERE id >= 3 ORDER BY score DESC LIMIT 2",
            &[
                Value::Vector(QUERY_EMBEDDING.to_vec()),
                Value::Text(QUERY_TEXT.to_string().into()),
            ],
        )
        .expect("filtered hybrid query");

    assert!(result.rows.len() <= 2);
    assert!(ids(&result).iter().all(|id| *id >= 3), "{:?}", ids(&result));
}

#[test]
fn a_query_with_no_retrieval_scans_in_row_order() {
    let mut engine = open();
    let result = engine
        .query("SELECT id, body FROM docs", &[])
        .expect("scan");
    assert_eq!(ids(&result), vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn identical_runs_produce_identical_results() {
    // The whole point of keeping the core free of I/O and clocks: two runs of
    // the same workload are indistinguishable, so a simulation failure can be
    // replayed exactly.
    let digest = |_: usize| {
        let mut engine = open();
        format!(
            "{:?}{:?}{:?}",
            hybrid(&mut engine),
            vector_only(&mut engine),
            text_only(&mut engine)
        )
    };
    assert_eq!(digest(0), digest(1));
}

#[test]
fn insert_order_does_not_change_the_ranking() {
    // Row ids differ when documents arrive in a different order, so this also
    // pins down that ties break on a stable key rather than on arrival order.
    let mut forwards = mem::engine().expect("engine");
    let mut backwards = mem::engine().expect("engine");
    for engine in [&mut forwards, &mut backwards] {
        engine
            .execute(
                "CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR(4))",
                &[],
            )
            .expect("create table");
        engine
            .execute("CREATE INDEX docs_body ON docs (body)", &[])
            .expect("create body index");
        engine
            .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
            .expect("create embedding index");
    }

    let insert = |engine: &mut Engine, doc: &Doc| {
        engine
            .execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(doc.id),
                    Value::Text(doc.body.to_string().into()),
                    Value::Vector(doc.embedding.to_vec()),
                ],
            )
            .expect("insert");
    };
    for doc in DOCS {
        insert(&mut forwards, doc);
    }
    for doc in DOCS.iter().rev() {
        insert(&mut backwards, doc);
    }

    assert_eq!(ids(&hybrid(&mut forwards)), ids(&hybrid(&mut backwards)));
}

#[test]
fn errors_are_reported_not_panicked() {
    let mut engine = open();
    for (sql, what) in [
        ("SELECT missing FROM docs", "unknown column"),
        ("SELECT id FROM nope", "unknown table"),
        (
            "SELECT vector_score(body, ?) FROM docs",
            "wrong column type",
        ),
        ("INSERT INTO docs VALUES (1, 'x')", "wrong arity"),
    ] {
        let result = engine.execute(sql, &[Value::Vector(QUERY_EMBEDDING.to_vec())]);
        assert!(result.is_err(), "{what}: `{sql}` unexpectedly succeeded");
    }
}
