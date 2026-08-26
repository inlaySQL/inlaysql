//! The distance a vector index is built under, from SQL.
//!
//! The engine used to have exactly one: cosine, everywhere, unstated. That is
//! the right default — most embedding models are trained for it and every
//! database written against this engine assumes it — but it is not the only
//! one anybody needs, and a user whose model was trained for Euclidean
//! distance had no way to say so and no way to find out that the answer they
//! were getting was ranked by something else.
//!
//! So the tests here are about *saying so*, and about what happens when the
//! statement says something the engine cannot honour. The assertions that
//! matter come in pairs: the same rows and the same query under two metrics
//! must come back in **different** orders, or nothing here is wired to
//! anything.

use inlaysql_core::hnsw::VectorMetric;
use inlaysql_core::{mem, Catalog, Engine, Error, IndexKind, Value};

fn engine() -> Engine {
    mem::engine().expect("open in-memory engine")
}

fn run(engine: &mut Engine, sql: &str) {
    engine
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

fn refuse(engine: &mut Engine, sql: &str) -> Error {
    engine
        .execute(sql, &[])
        .expect_err(&format!("`{sql}` was accepted"))
}

/// A table of two-dimensional embeddings whose *lengths* differ, which is the
/// only kind of corpus on which cosine and L2 can disagree at all.
fn corpus(engine: &mut Engine) {
    run(
        engine,
        "CREATE TABLE items (id INTEGER PRIMARY KEY, embedding VECTOR(2))",
    );
    for (id, x, y) in [(1, 1.0, 0.0), (2, 8.0, 0.0), (3, 0.7, 0.72)] {
        engine
            .execute(
                "INSERT INTO items VALUES (?, ?)",
                &[Value::Integer(id), Value::Vector(vec![x, y])],
            )
            .expect("insert");
    }
}

/// The ids a `vector_score` query returns, best first.
fn ranked(engine: &mut Engine, query: Vec<f32>) -> Vec<i64> {
    engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM items \
             ORDER BY score DESC LIMIT 10",
            &[Value::Vector(query)],
        )
        .expect("vector query")
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("id was {other:?}"),
        })
        .collect()
}

/// The `detail` column of an `EXPLAIN`, in tree order.
fn plan(engine: &mut Engine, sql: &str, params: &[Value]) -> Vec<String> {
    engine
        .query(sql, params)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .rows
        .iter()
        .map(|row| row[2].as_str().expect("text detail").to_string())
        .collect()
}

/// **The assertion the whole feature exists for.** One corpus, one query, two
/// indexes: the answers differ, and each is right under its own metric.
///
/// Under cosine, rows 1 and 2 are the same vector — they point the same way,
/// and cosine has thrown their lengths away — so both score 1.0 and row 3,
/// which points somewhere else, comes last. Under L2 row 3 is *nearer* than
/// row 2 despite pointing elsewhere, because it is a fifth of the distance
/// away. Neither ranking is an approximation of the other.
#[test]
fn the_two_metrics_rank_the_same_rows_differently() {
    let mut cosine = engine();
    corpus(&mut cosine);
    run(
        &mut cosine,
        "CREATE INDEX items_embedding ON items (embedding)",
    );

    let mut l2 = engine();
    corpus(&mut l2);
    run(
        &mut l2,
        "CREATE INDEX items_embedding ON items (embedding vector_l2_ops)",
    );

    let query = vec![1.0, 0.0];
    assert_eq!(ranked(&mut cosine, query.clone()), vec![1, 2, 3]);
    assert_eq!(ranked(&mut l2, query), vec![1, 3, 2]);
}

/// pgvector's own statement, verbatim.
///
/// This is what "compatibility where it is real" has to mean: not a lookalike
/// token, but the line out of somebody's existing migration file running
/// unchanged. `USING hnsw` already resolves to this engine's vector index and
/// the operator class already parses; all that was missing was the meaning.
#[test]
fn pgvectors_own_create_index_spelling_works() {
    for sql in [
        "CREATE INDEX items_embedding ON items USING hnsw (embedding vector_l2_ops)",
        "CREATE INDEX items_embedding ON items (embedding vector_l2_ops) USING hnsw",
        "CREATE INDEX items_embedding ON items (embedding VECTOR_L2_OPS)",
        "CREATE INDEX items_embedding ON items USING vector (embedding vector_cosine_ops)",
    ] {
        let mut engine = engine();
        corpus(&mut engine);
        run(&mut engine, sql);
        let expected = if sql.contains("cosine") {
            VectorMetric::Cosine
        } else {
            VectorMetric::L2
        };
        let index = engine
            .catalog()
            .indexes_for("items")
            .into_iter()
            .find(|index| index.kind == IndexKind::Vector)
            .expect("a vector index");
        assert_eq!(index.metric, expected, "`{sql}` recorded the wrong metric");
    }
}

/// Writing nothing means cosine, which is what every index that already exists
/// is and what every query written against this engine assumes.
#[test]
fn the_default_is_cosine_and_is_stated_as_such() {
    let mut engine = engine();
    corpus(&mut engine);
    run(
        &mut engine,
        "CREATE INDEX items_embedding ON items (embedding)",
    );
    let index = engine.catalog().indexes_for("items")[0].clone();
    assert_eq!(index.metric, VectorMetric::Cosine);
}

/// `EXPLAIN` names the metric, and names it for a cosine index too.
///
/// Not only for the interesting case: which distance ranked the rows decides
/// which rows came back, and a plan that printed it only when it was unusual
/// would leave the common case looking like the question had not been asked.
/// The column list is rendered the way `CREATE INDEX` spells it, so what the
/// plan prints is what you would write to reproduce it.
#[test]
fn explain_reports_which_metric_an_index_uses() {
    for (ddl, expected) in [
        (
            "CREATE INDEX items_embedding ON items (embedding)",
            "SEARCH items USING VECTOR INDEX items_embedding (embedding vector_cosine_ops) \
             FOR vector_score",
        ),
        (
            "CREATE INDEX items_embedding ON items (embedding vector_l2_ops)",
            "SEARCH items USING VECTOR INDEX items_embedding (embedding vector_l2_ops) \
             FOR vector_score",
        ),
    ] {
        let mut engine = engine();
        corpus(&mut engine);
        run(&mut engine, ddl);
        let details = plan(
            &mut engine,
            "EXPLAIN SELECT id, vector_score(embedding, ?) AS score FROM items \
             ORDER BY score DESC LIMIT 3",
            &[Value::Vector(vec![1.0, 0.0])],
        );
        assert!(
            details.iter().any(|detail| detail.starts_with(expected)),
            "after `{ddl}` the plan was {details:?}, wanted one starting `{expected}`"
        );
    }
}

/// Inner product is refused, with the reason and with the transformation that
/// is exact. See [`VectorMetric`]'s own docs: it is not a metric, so the graph
/// this engine would build on it is one whose invariants do not hold, and
/// shipping that quietly is the thing the refusal is instead of.
#[test]
fn inner_product_is_refused_and_says_why() {
    let mut engine = engine();
    corpus(&mut engine);
    let error = refuse(
        &mut engine,
        "CREATE INDEX items_embedding ON items (embedding vector_ip_ops)",
    );
    let Error::Unsupported(message) = &error else {
        panic!("expected a refusal, got {error:?}")
    };
    assert!(message.contains("not a metric"), "{message}");
    assert!(message.contains("vector_cosine_ops"), "{message}");
    // And no index was left behind by the attempt.
    assert!(engine.catalog().indexes_for("items").is_empty());
}

/// An operator class that this engine does not have is refused rather than
/// ignored — the silently-dropped clause is the bug class this front end is
/// built to not have.
#[test]
fn an_unknown_operator_class_is_refused() {
    let mut engine = engine();
    corpus(&mut engine);
    let error = refuse(
        &mut engine,
        "CREATE INDEX items_embedding ON items (embedding vector_hamming_ops)",
    );
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
}

/// An operator class on an index that has no distance is refused, in all three
/// places one can be written.
#[test]
fn an_operator_class_is_refused_where_there_is_no_distance() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, title TEXT, n INTEGER)",
    );
    for sql in [
        // A B-tree index compares by collation, not by distance.
        "CREATE INDEX docs_n ON docs (n vector_l2_ops)",
        // Neither does a full-text one, single- or multi-column.
        "CREATE INDEX docs_body ON docs (body vector_l2_ops)",
        "CREATE INDEX docs_text ON docs (body vector_l2_ops, title) USING FULLTEXT",
    ] {
        let error = refuse(&mut engine, sql);
        assert!(matches!(error, Error::Unsupported(_)), "`{sql}`: {error:?}");
    }

    // And a constraint is not an index: it decides duplicates by the columns'
    // own collations, so there is nothing for an operator class to apply to.
    let error = refuse(
        &mut engine,
        "CREATE TABLE t (a INTEGER, UNIQUE (a vector_l2_ops))",
    );
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
}

/// `CREATE UNIQUE INDEX` over a `VECTOR` column builds no graph at all — it is
/// a constraint enforced by a per-write scan, because no ordered index can
/// cover a vector. So an operator class on one would choose the distance of an
/// index that does not exist, and it is refused rather than dropped.
///
/// This one is worth a test of its own because it is the arm that *nearly*
/// slipped through: the kind is inferred as `Vector` from the column, so every
/// check that keys on the kind accepts it, and only the `UNIQUE` does not.
#[test]
fn an_operator_class_on_a_unique_vector_index_is_refused() {
    let mut engine = engine();
    corpus(&mut engine);
    let error = refuse(
        &mut engine,
        "CREATE UNIQUE INDEX items_embedding ON items (embedding vector_l2_ops)",
    );
    let Error::Unsupported(message) = &error else {
        panic!("expected a refusal, got {error:?}")
    };
    assert!(message.contains("builds no graph"), "{message}");

    // Without the operator class it is still the constraint it always was.
    run(
        &mut engine,
        "CREATE UNIQUE INDEX items_embedding ON items (embedding)",
    );
}

/// One column carries one vector index.
///
/// `vector_score(embedding, ?)` names the column and not the metric, so two
/// graphs over it would be one nobody could ask for. pgvector can hold both
/// because its query names the operator (`<->` against `<=>`); this dialect's
/// function does not, so the second declaration is refused with the reason
/// rather than accepted and then arbitrarily resolved.
#[test]
fn one_column_cannot_carry_two_metrics() {
    let mut engine = engine();
    corpus(&mut engine);
    run(
        &mut engine,
        "CREATE INDEX items_cosine ON items (embedding)",
    );
    let error = refuse(
        &mut engine,
        "CREATE INDEX items_l2 ON items (embedding vector_l2_ops)",
    );
    let Error::Catalog(message) = &error else {
        panic!("expected a catalog refusal, got {error:?}")
    };
    assert!(message.contains("vector_cosine_ops"), "{message}");
    assert!(message.contains("items_cosine"), "{message}");

    // Dropping the first makes room for the second, which is the answer the
    // message gives.
    run(&mut engine, "DROP INDEX items_cosine");
    run(
        &mut engine,
        "CREATE INDEX items_l2 ON items (embedding vector_l2_ops)",
    );
    assert_eq!(ranked(&mut engine, vec![1.0, 0.0]), vec![1, 3, 2]);
}

/// The catalog is written at the lowest version that can express it, so a
/// database that uses no metric is byte for byte the database it was before
/// metrics existed — and one that does forces the bump rather than being
/// silently readable as cosine by a build that would rebuild it wrong.
#[test]
fn only_a_non_default_metric_forces_the_catalog_version() {
    /// The `u32` version that follows the four-byte magic.
    fn version(catalog: &Catalog) -> u32 {
        let bytes = catalog.encode();
        u32::from_le_bytes(bytes[4..8].try_into().expect("a version word"))
    }

    let mut cosine = engine();
    corpus(&mut cosine);
    run(
        &mut cosine,
        "CREATE INDEX items_embedding ON items (embedding)",
    );
    assert_eq!(version(cosine.catalog()), 2);

    let mut l2 = engine();
    corpus(&mut l2);
    run(
        &mut l2,
        "CREATE INDEX items_embedding ON items (embedding vector_l2_ops)",
    );
    assert_eq!(version(l2.catalog()), 7);

    // ...and it survives the round trip.
    let decoded = Catalog::decode(&l2.catalog().encode()).expect("decode a version-7 catalog");
    assert_eq!(decoded.indexes_for("items")[0].metric, VectorMetric::L2);
}

/// `COLLATE` and an operator class are both refusable on a vector index, and
/// the operator-class one has to keep working when both are written.
#[test]
fn collate_on_a_vector_index_is_still_refused() {
    let mut engine = engine();
    corpus(&mut engine);
    let error = refuse(
        &mut engine,
        "CREATE INDEX items_embedding ON items (embedding COLLATE NOCASE)",
    );
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
}
