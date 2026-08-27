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

    let secondary = details(
        &mut engine,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    assert!(secondary
        .iter()
        .any(|detail| { detail.contains("HASH JOIN posts") && detail.contains("COSTED") }));

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
    assert!(plan
        .iter()
        .any(|detail| { detail.contains("HASH JOIN posts") && detail.contains("COSTED") }));
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
