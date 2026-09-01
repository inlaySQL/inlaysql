//! R4's first costed access-path slice: explicit ANALYZE, safe fallback and
//! the same choice in EXPLAIN and execution.

use inlaysql_core::{mem, Engine, Error, SharedStorage, Storage, Value};

fn engine() -> Engine {
    mem::engine().expect("open in-memory engine")
}

fn run(engine: &mut Engine, sql: &str) {
    engine
        .execute(sql, &[])
        .unwrap_or_else(|error| panic!("`{sql}`: {error}"));
}

/// Every row a query answers with, as comparable text.
fn answer(engine: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    engine
        .query(sql, &[])
        .unwrap_or_else(|error| panic!("`{sql}`: {error}"))
        .rows
        .iter()
        .map(|row| row.iter().map(|cell| format!("{cell:?}")).collect())
        .collect()
}

fn details(engine: &mut Engine, sql: &str) -> Vec<String> {
    engine
        .query(sql, &[])
        .unwrap_or_else(|error| panic!("`{sql}`: {error}"))
        .rows
        .into_iter()
        .map(|row| row[2].as_str().expect("EXPLAIN detail is text").to_string())
        .collect()
}

fn joined_engine() -> Engine {
    let mut engine = engine();
    populate_joined(&mut engine);
    engine
}

fn populate_joined(engine: &mut Engine) {
    run(
        engine,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
    );
    run(
        engine,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
    );
    run(
        engine,
        "CREATE INDEX posts_user_id ON posts (user_id) USING BTREE",
    );
    for id in 1..=4 {
        run(
            engine,
            &format!("INSERT INTO users VALUES ({id}, 'user{id}')"),
        );
    }
    for id in 1..=32 {
        let user_id = 1 + ((id - 1) % 4);
        run(
            engine,
            &format!("INSERT INTO posts VALUES ({id}, {user_id}, 'post{id}')"),
        );
    }
}

fn shared_engine(storage: SharedStorage) -> Engine {
    Engine::open(
        Box::new(storage),
        Box::new(mem::MemIndexFactory),
        Box::new(mem::LogicalClock::new()),
    )
    .expect("open shared engine")
}

#[test]
fn analyze_costs_existing_join_paths_and_explain_matches_execution() {
    let mut engine = joined_engine();

    let before = details(
        &mut engine,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    assert!(before
        .iter()
        .any(|detail| detail.starts_with("HASH JOIN posts")));
    assert!(!before.iter().any(|detail| detail.contains("COSTED")));

    run(&mut engine, "ANALYZE");

    let pk = details(
        &mut engine,
        "EXPLAIN SELECT posts.id, users.name FROM posts \
         JOIN users ON posts.user_id = users.id",
    );
    assert!(pk
        .iter()
        .any(|detail| detail.contains("HASH JOIN users") && detail.contains("COSTED")));

    // Written `users JOIN posts`, planned `posts JOIN users`: with statistics
    // the cost model is now allowed to choose which side drives, and building
    // the smaller side is cheaper than building the larger one. The assertion
    // moved with the behaviour rather than being relaxed — it still pins a
    // costed hash join, on the side the model actually picks.
    let secondary = details(
        &mut engine,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    assert!(
        secondary
            .iter()
            .any(|detail| { detail.contains("HASH JOIN users") && detail.contains("COSTED") }),
        "expected the reordered plan, got {secondary:?}"
    );

    let limited = details(
        &mut engine,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id LIMIT 1",
    );
    assert!(limited.iter().any(|detail| {
        detail.contains("INDEX NESTED LOOP JOIN posts USING INDEX posts_user_id")
            && detail.contains("COSTED")
    }));

    let rows = engine
        .query(
            "SELECT users.name, posts.title FROM users \
             JOIN posts ON posts.user_id = users.id",
            &[],
        )
        .expect("costed join")
        .rows;
    assert_eq!(rows.len(), 32);
    assert_eq!(
        rows[0],
        vec![Value::Text("user1".into()), Value::Text("post1".into())]
    );
}

#[test]
fn a_row_write_makes_stats_stale_and_restores_the_rule_based_fallback() {
    let mut engine = joined_engine();
    run(&mut engine, "ANALYZE");
    let costed = details(
        &mut engine,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    assert!(costed.iter().any(|detail| detail.contains("COSTED")));

    run(&mut engine, "INSERT INTO users VALUES (5, 'user5')");
    let stale = details(
        &mut engine,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    assert!(!stale.iter().any(|detail| detail.contains("COSTED")));
    assert!(stale
        .iter()
        .any(|detail| detail.starts_with("HASH JOIN posts")));
}

#[test]
fn unsupported_analyze_options_are_refused_by_name() {
    let mut engine = joined_engine();
    let error = engine
        .execute("ANALYZE TABLE users COMPUTE STATISTICS", &[])
        .expect_err("unsupported ANALYZE option was accepted");
    assert!(matches!(error, Error::Unsupported(_)), "got {error:?}");
    assert!(error.to_string().contains("ANALYZE"), "got {error}");
}

#[test]
fn analyze_requires_a_committed_snapshot() {
    let mut engine = joined_engine();
    engine.begin().expect("begin");
    let error = engine
        .execute("ANALYZE", &[])
        .expect_err("ANALYZE ran inside a transaction");
    assert!(matches!(error, Error::Transaction(_)), "got {error:?}");
    engine.rollback().expect("rollback");
}

#[test]
fn analyzed_stats_survive_reopening_the_engine() {
    let shared = SharedStorage::new(Box::new(mem::MemStorage::new()));
    let mut first = shared_engine(shared.clone());
    populate_joined(&mut first);
    run(&mut first, "ANALYZE");
    drop(first);

    let mut reopened = shared_engine(shared);
    let plan = details(
        &mut reopened,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    // `HASH JOIN users` from a query written `users JOIN posts`: the stats
    // survived the reopen, so the cost model is live and reorders — which is
    // what this test is really asserting.
    assert!(
        plan.iter()
            .any(|detail| { detail.contains("HASH JOIN users") && detail.contains("COSTED") }),
        "expected costed, reordered plan after reopening, got {plan:?}"
    );
}

#[test]
fn a_catalog_revision_rejects_stats_after_same_shape_ddl() {
    let shared = SharedStorage::new(Box::new(mem::MemStorage::new()));
    let mut first = shared_engine(shared.clone());
    populate_joined(&mut first);
    run(&mut first, "ANALYZE");

    // The final catalog is byte-for-byte the same as before this pair, and no
    // row changed. A catalog byte stamp alone would therefore accept the old
    // stats after reopening; the independent schema revision must reject it.
    run(&mut first, "CREATE TABLE scratch (id INTEGER PRIMARY KEY)");
    run(&mut first, "DROP TABLE scratch");
    drop(first);

    let mut reopened = shared_engine(shared);
    let plan = details(
        &mut reopened,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    assert!(!plan.iter().any(|detail| detail.contains("COSTED")));
    assert!(plan
        .iter()
        .any(|detail| detail.starts_with("HASH JOIN posts")));
}

#[test]
fn a_corrupt_persisted_stats_blob_falls_back_to_rules() {
    let shared = SharedStorage::new(Box::new(mem::MemStorage::new()));
    let mut first = shared_engine(shared.clone());
    populate_joined(&mut first);
    run(&mut first, "ANALYZE");

    let mut corruptor = shared.clone();
    corruptor
        .put_meta("planner_stats", b"not planner stats")
        .expect("write corrupt stats");
    corruptor.commit().expect("commit corrupt stats");
    drop(first);

    let mut reopened = shared_engine(shared);
    let plan = details(
        &mut reopened,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    assert!(!plan.iter().any(|detail| detail.contains("COSTED")));
    assert!(plan
        .iter()
        .any(|detail| detail.starts_with("HASH JOIN posts")));
}

/// A reordered join returns exactly what the written order returns.
///
/// Join *ordering* is the one planner choice that can change an answer rather
/// than only the work: every expression in the plan indexes into the joined
/// row by ordinal, and swapping the sources moves every one of them. A remap
/// that missed a field would not be slow, it would be wrong — in the
/// projection, the `WHERE`, an `ORDER BY`, an aggregate's argument.
///
/// So each query below is run twice on identical data: once with statistics,
/// where the cost model is free to reorder, and once without, where it cannot.
/// The two must agree, whatever the planner chose.
#[test]
fn a_reordered_join_answers_exactly_as_the_written_order_does() {
    let queries = [
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id \
         ORDER BY users.name, posts.title",
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id \
         ORDER BY posts.id",
        // A `WHERE` over both sides: its ordinals move with everything else.
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id \
         WHERE users.id < 4 AND posts.title <> 'zzz' ORDER BY users.name, posts.title",
        // An aggregate and a GROUP BY, whose keys are ordinals too.
        "SELECT users.name, COUNT(*), MAX(posts.title) FROM users JOIN posts \
         ON posts.user_id = users.id GROUP BY users.name ORDER BY users.name",
        // `SELECT *`, where the output order itself is the thing at risk.
        "SELECT * FROM users JOIN posts ON posts.user_id = users.id \
         ORDER BY users.id, posts.id",
        // An expression over both sides, and a LIMIT.
        "SELECT users.name || '/' || posts.title FROM users JOIN posts \
         ON posts.user_id = users.id ORDER BY 1 LIMIT 7",
    ];

    let build = |analyse: bool| {
        let mut engine = engine();
        run(
            &mut engine,
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
        );
        run(
            &mut engine,
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
        );
        for id in 1..=6i64 {
            run(
                &mut engine,
                &format!("INSERT INTO users VALUES ({id}, 'user{id}')"),
            );
        }
        for id in 1..=48i64 {
            let user = 1 + ((id - 1) % 6);
            run(
                &mut engine,
                &format!("INSERT INTO posts VALUES ({id}, {user}, 'title{id}')"),
            );
        }
        run(
            &mut engine,
            "CREATE INDEX posts_user_id ON posts (user_id) USING BTREE",
        );
        if analyse {
            run(&mut engine, "ANALYZE");
        }
        engine
    };

    let mut costed = build(true);
    let mut plain = build(false);
    for sql in queries {
        assert_eq!(
            answer(&mut costed, sql),
            answer(&mut plain, sql),
            "the costed plan and the shape-rule plan disagree on: {sql}"
        );
    }
}
