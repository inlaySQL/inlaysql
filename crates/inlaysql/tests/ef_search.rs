//! `ef_search` at query time: the one knob that trades recall against latency
//! on an ANN index, driven through real SQL against a real HNSW graph.
//!
//! `inlaysql-core`'s `hnsw` unit tests prove the property at the level of one
//! graph walk. What is here is what only the shipped crate can show: that the
//! number a caller sets is the number that reaches that walk, that `EXPLAIN`
//! names the point the query will actually run at, that a value too narrow to
//! answer the query is refused rather than quietly widened — and that a caller
//! who sets nothing gets, row for row, the answer this engine always gave.

use inlaysql::{Database, Error, Value};

/// Dimension and corpus size for the recall measurement.
///
/// Both are as small as they can be while the *premise* still holds: the
/// shipped tuning must not already recall 1.0, or every assertion below would
/// pass with `ef_search` wired to nothing at all. Sixty-four uniformly random
/// dimensions is what does it — uniform because random vectors have no cluster
/// structure for the graph's upper layers to exploit, which is the case HNSW
/// finds hard and therefore the case a recall test has to be run on. The engine
/// also over-fetches candidates fourfold before the `LIMIT` trims them, so
/// there is a lot of slack to use up before the top ten are wrong at all.
const DIM: usize = 64;
const ROWS: u64 = 1_200;

/// A corpus small enough to build in no time, for the tests that are about the
/// plumbing rather than about recall: what `EXPLAIN` reports and what is
/// refused do not depend on how many rows there are.
const SMALL_ROWS: u64 = 64;

/// Uniformly random vectors, deterministically — no clock, no RNG.
fn corpus(rows: u64) -> Vec<Vec<f32>> {
    let mut state = 0x51ed_2701_u64;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((state >> 33) as f32 / u32::MAX as f32) - 0.5
    };
    (0..rows)
        .map(|_| (0..DIM).map(|_| next()).collect())
        .collect()
}

fn query_vector(seed: u64) -> Vec<f32> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ seed;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((state >> 33) as f32 / u32::MAX as f32) - 0.5
    };
    (0..DIM).map(|_| next()).collect()
}

fn loaded(rows: u64) -> (Database, Vec<Vec<f32>>) {
    let mut db = Database::open_in_memory().expect("open");
    db.execute(
        &format!("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR({DIM}))"),
        &[],
    )
    .unwrap();
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .unwrap();

    let vectors = corpus(rows);
    // One transaction: the graph is built once, at the commit, rather than
    // re-reconciled once per row.
    db.begin().unwrap();
    for (index, vector) in vectors.iter().enumerate() {
        db.execute(
            "INSERT INTO docs (id, embedding) VALUES (?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                Value::Vector(vector.clone()),
            ],
        )
        .unwrap();
    }
    db.commit().unwrap();
    (db, vectors)
}

/// The ids one `vector_score` query returns, best first.
fn ids(db: &mut Database, query: &[f32], k: usize) -> Vec<i64> {
    db.query(
        &format!(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs \
             ORDER BY score DESC LIMIT {k}"
        ),
        &[Value::Vector(query.to_vec())],
    )
    .unwrap()
    .rows
    .iter()
    .map(|row| match row[0] {
        Value::Integer(id) => id,
        ref other => panic!("id was {other:?}"),
    })
    .collect()
}

/// The true nearest `k` by cosine similarity, computed exhaustively here — the
/// oracle the approximate answer is scored against.
fn exact(vectors: &[Vec<f32>], query: &[f32], k: usize) -> Vec<i64> {
    let norm = |v: &[f32]| {
        v.iter()
            .map(|x| x * x)
            .sum::<f32>()
            .sqrt()
            .max(f32::MIN_POSITIVE)
    };
    let query_norm = norm(query);
    let mut scored: Vec<(f32, i64)> = vectors
        .iter()
        .enumerate()
        .map(|(index, vector)| {
            let dot: f32 = vector.iter().zip(query).map(|(a, b)| a * b).sum();
            (dot / (norm(vector) * query_norm), index as i64 + 1)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(k).map(|(_, id)| id).collect()
}

/// Mean recall@k over 30 deterministic queries, at whatever `ef_search` the
/// handle currently has in force.
fn recall(db: &mut Database, vectors: &[Vec<f32>], k: usize) -> f64 {
    const QUERIES: u64 = 30;
    let mut total = 0.0;
    for seed in 0..QUERIES {
        let query = query_vector(seed);
        let truth = exact(vectors, &query, k);
        let found = ids(db, &query, k);
        let hit = found.iter().filter(|id| truth.contains(id)).count();
        total += hit as f64 / k as f64;
    }
    total / QUERIES as f64
}

/// The `detail` column of every `EXPLAIN` node.
fn plan(db: &mut Database, sql: &str) -> Vec<String> {
    db.query(sql, &[Value::Vector(vec![0.0; DIM])])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .rows
        .iter()
        .map(|row| match &row[2] {
            Value::Text(detail) => detail.as_str().to_string(),
            other => panic!("detail was {other:?}"),
        })
        .collect()
}

/// **The assertion the knob exists for, through SQL.** A caller who asks for
/// more recall on an important query gets it — all the way to the exact
/// answer — and one who asks for less latency on a cheap query gives some up.
///
/// Three points, one graph, one set of queries, one oracle: only `ef_search`
/// moves between them, which is what makes this evidence that the number
/// reaches the walk rather than evidence about HNSW.
#[test]
fn ef_search_moves_recall_in_the_direction_it_was_asked_to() {
    let (mut db, vectors) = loaded(ROWS);
    let k = 10;

    let default = recall(&mut db, &vectors, k);
    assert!(
        default < 1.0,
        "the default tuning already recalls {default:.4} here, so this test can no longer \
         tell a connected ef_search from a disconnected one; make the corpus harder"
    );

    // The floor: a beam exactly as wide as the answer, which is the cheapest
    // walk the engine will run. Anything narrower is refused.
    db.set_vector_ef_search(Some(k));
    let narrow = recall(&mut db, &vectors, k);

    // Wider than the corpus itself, so the walk cannot stop early and the
    // answer is the exhaustive one. Asserting that *endpoint* rather than
    // "better than before" is deliberate: an improvement of one row in three
    // hundred would also satisfy `>`, and would be much weaker evidence.
    db.set_vector_ef_search(Some(2 * ROWS as usize));
    let wide = recall(&mut db, &vectors, k);

    assert_eq!(
        wide, 1.0,
        "a candidate list wider than the corpus recalled {wide:.4}, not the exact answer \
         (the default recalled {default:.4})"
    );
    assert!(
        narrow < default,
        "ef_search = {k} recalled {narrow:.4}, no worse than the default's {default:.4}"
    );
}

/// Setting nothing changes nothing — the constraint every query written
/// against this engine before today is owed. Checked as an exact row-for-row
/// identity rather than as a recall number, because a mean would hide a
/// handful of moved rows.
#[test]
fn an_unset_ef_search_returns_exactly_the_rows_it_always_did() {
    let (mut db, _) = loaded(SMALL_ROWS);
    let query = query_vector(7);
    let before = ids(&mut db, &query, 10);
    assert_eq!(
        db.vector_ef_search(),
        None,
        "a fresh handle imposes nothing"
    );

    db.set_vector_ef_search(Some(256));
    assert_eq!(db.vector_ef_search(), Some(256));
    db.set_vector_ef_search(None);

    assert_eq!(db.vector_ef_search(), None);
    assert_eq!(
        ids(&mut db, &query, 10),
        before,
        "clearing ef_search did not restore the untuned answer"
    );

    // `Some(0)` is the same thing said the other way, and is the spelling the
    // MySQL server's `SET inlaysql_hnsw_ef_search = 0` uses. Read as a beam of
    // zero it would refuse every query on this handle while the refusal told
    // the caller to set it to zero.
    db.set_vector_ef_search(Some(0));
    assert_eq!(db.vector_ef_search(), None);
    assert_eq!(ids(&mut db, &query, 10), before);
}

/// `EXPLAIN` names the operating point, and it is the one in force — not a
/// restatement of what was set, and not a constant. Untuned it is the index's
/// own `ef_for(k)`, which widens with the candidate count; tuned it is the
/// session's number.
#[test]
fn explain_reports_the_ef_the_search_will_run_at() {
    let (mut db, _) = loaded(SMALL_ROWS);

    // `LIMIT 10` asks the index for 40 candidates, and `HnswParams::DEFAULT`
    // searches 40 of them with `max(ef_search = 64, 40 * 2) = 80`.
    let untuned = plan(
        &mut db,
        "EXPLAIN SELECT id, vector_score(embedding, ?) AS score FROM docs \
         ORDER BY score DESC LIMIT 10",
    );
    assert!(
        untuned.iter().any(|line| line.contains("(ef=80)")),
        "untuned plan did not report the default operating point: {untuned:?}"
    );

    // Not a constant: a wider `LIMIT` asks for more candidates and the default
    // tuning widens the beam to match — which is exactly why the untuned
    // operating point cannot be reported as one number by
    // `@@inlaysql_hnsw_ef_search` and has to be reported per query here.
    let wider = plan(
        &mut db,
        "EXPLAIN SELECT id, vector_score(embedding, ?) AS score FROM docs \
         ORDER BY score DESC LIMIT 100",
    );
    assert!(
        wider.iter().any(|line| line.contains("(ef=800)")),
        "the reported ef did not widen with the query: {wider:?}"
    );

    db.set_vector_ef_search(Some(512));
    let tuned = plan(
        &mut db,
        "EXPLAIN SELECT id, vector_score(embedding, ?) AS score FROM docs \
         ORDER BY score DESC LIMIT 10",
    );
    assert!(
        tuned.iter().any(|line| line.contains("(ef=512)")),
        "the session's ef_search did not reach the plan: {tuned:?}"
    );
}

/// A candidate list narrower than the answer is refused, by name and with the
/// number that would work — never clamped up to fit, which would report one
/// `ef` and search at another.
#[test]
fn an_ef_search_too_narrow_for_the_query_is_refused() {
    let (mut db, _) = loaded(SMALL_ROWS);
    // Nine against a `LIMIT 10`: off by one, so this cannot be passing
    // against some other bound by accident.
    db.set_vector_ef_search(Some(9));

    let sql = "SELECT id, vector_score(embedding, ?) AS score FROM docs \
               ORDER BY score DESC LIMIT 10";
    let error = db
        .query(sql, &[Value::Vector(vec![0.0; DIM])])
        .expect_err("a beam narrower than the answer must not be answered");
    let Error::Unsupported(message) = &error else {
        panic!("expected a refusal, got {error:?}");
    };
    assert!(
        message.contains('9') && message.contains("10"),
        "the refusal named neither the value nor the minimum: {message}"
    );

    // And `EXPLAIN` refuses identically. A plan for a query the engine will
    // not run would be describing nothing.
    assert!(
        db.query(&format!("EXPLAIN {sql}"), &[Value::Vector(vec![0.0; DIM])])
            .is_err(),
        "EXPLAIN described a query the engine refuses to run"
    );

    // The smallest value that is not refused is the one the message named,
    // and it really does answer — a beam exactly as wide as the `LIMIT` is
    // legal, which is what keeps the cheap end of the trade reachable.
    db.set_vector_ef_search(Some(10));
    assert_eq!(ids(&mut db, &query_vector(3), 10).len(), 10);

    // A `LIMIT` wider than the beam is refused for the same reason, from the
    // other side: the floor moves with the query, not with the variable.
    assert!(db
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs \
             ORDER BY score DESC LIMIT 20",
            &[Value::Vector(vec![0.0; DIM])],
        )
        .is_err());
}
