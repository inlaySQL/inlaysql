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

/// Every row a query answers with, or the message it refuses with, as
/// comparable text.
///
/// A refusal is part of an aggregate's answer — `SUM`'s integer overflow is an
/// error rather than a number — so a tie test that compared only rows would let
/// one path refuse where the other returned a value, which is the drift that
/// matters most.
fn outcome(engine: &mut Engine, sql: &str) -> Result<Vec<Vec<String>>, String> {
    engine
        .query(sql, &[])
        .map(|answer| {
            answer
                .rows
                .iter()
                .map(|row| row.iter().map(|cell| format!("{cell:?}")).collect())
                .collect()
        })
        .map_err(|error| error.to_string())
}

/// Require one query to answer the same folded from the row stream as it does
/// folded from collected rows.
///
/// `GROUP_CONCAT` is the lever: `can_stream_aggregate` refuses any query
/// containing one, because its separator is read from the group's first row. So
/// appending one sends the whole query down the collecting path while leaving
/// every other aggregate in it unchanged, and its column is dropped before
/// comparing — what is compared is the same columns computed two ways.
fn assert_streamed_matches_collected(engine: &mut Engine, projection: &str, rest: &str) {
    let streamed = outcome(engine, &format!("SELECT {projection} FROM {rest}"));
    let collected = outcome(
        engine,
        &format!("SELECT {projection}, GROUP_CONCAT(id) FROM {rest}"),
    )
    .map(|rows| {
        rows.into_iter()
            .map(|mut row| {
                row.pop();
                row
            })
            .collect()
    });
    assert_eq!(
        streamed, collected,
        "streamed and collected disagree on: SELECT {projection} FROM {rest}"
    );
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

    // Written `posts JOIN users`, planned `users JOIN posts`: with statistics
    // the cost model is allowed to choose which side drives, and driving from
    // the smaller table is what the measurement says is cheaper — every
    // outer row pays the join loop, so 20k users driving beats 160k posts
    // driving for the same output. (The first costing pinned the opposite
    // swap here, and the benchmark caught it: `PERF.md`, 2026-09-02.)
    let pk = details(
        &mut engine,
        "EXPLAIN SELECT posts.id, users.name FROM posts \
         JOIN users ON posts.user_id = users.id",
    );
    assert!(
        pk.iter()
            .any(|detail| detail.contains("HASH JOIN posts") && detail.contains("COSTED")),
        "expected the reordered plan, got {pk:?}"
    );

    // Written `users JOIN posts` is already the cheaper order and stays as
    // written — costed, not swapped.
    let secondary = details(
        &mut engine,
        "EXPLAIN SELECT users.name, posts.title FROM users \
         JOIN posts ON posts.user_id = users.id",
    );
    assert!(
        secondary
            .iter()
            .any(|detail| { detail.contains("HASH JOIN posts") && detail.contains("COSTED") }),
        "expected the written order, costed, got {secondary:?}"
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
        "EXPLAIN SELECT posts.id, users.name FROM posts \
         JOIN users ON posts.user_id = users.id",
    );
    // `HASH JOIN posts` from a query written `posts JOIN users`: the stats
    // survived the reopen, so the cost model is live and reorders — which is
    // what this test is really asserting.
    assert!(
        plan.iter()
            .any(|detail| { detail.contains("HASH JOIN posts") && detail.contains("COSTED") }),
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

/// Grouping is correct, and the rewrite did not move the rows.
///
/// Grouping moved from sorting every input row to an ordered map, so the
/// question this pins is whether anything observable changed. Two things are
/// asserted and one deliberately is not:
///
/// * The groups and their counts are right.
/// * An explicit `ORDER BY` orders them.
/// * The order *without* an `ORDER BY` is **not** asserted, because it is not
///   key order and never was — checked against the pre-rewrite engine, which
///   emits the same first-seen order this one does. The old code sorted its
///   input only to bring equal keys adjacent, not to order the output, so
///   removing that sort changed no order anyone could observe. Writing this
///   down because the obvious assumption — sorted input, sorted output — is
///   wrong here, and a future rewrite to a hash map should know that the
///   order is already unspecified rather than discover it.
#[test]
fn group_by_groups_correctly_and_orders_when_asked() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, name TEXT)",
    );
    for (id, n) in [
        (1, 7),
        (2, 3),
        (3, 9),
        (4, 3),
        (5, 1),
        (6, 7),
        (7, 1),
        (8, 5),
    ] {
        run(
            &mut engine,
            &format!("INSERT INTO t VALUES ({id}, {n}, 'row{id}')"),
        );
    }

    let ordered = answer(
        &mut engine,
        "SELECT n, COUNT(*) FROM t GROUP BY n ORDER BY n",
    );
    assert_eq!(
        ordered,
        vec![
            vec!["Integer(1)".to_string(), "Integer(2)".to_string()],
            vec!["Integer(3)".to_string(), "Integer(2)".to_string()],
            vec!["Integer(5)".to_string(), "Integer(1)".to_string()],
            vec!["Integer(7)".to_string(), "Integer(2)".to_string()],
            vec!["Integer(9)".to_string(), "Integer(1)".to_string()],
        ],
        "wrong groups or counts"
    );

    // The same groups arrive with no `ORDER BY`, whatever order they arrive in.
    let mut implicit = answer(&mut engine, "SELECT n, COUNT(*) FROM t GROUP BY n");
    implicit.sort();
    let mut expected = ordered;
    expected.sort();
    assert_eq!(
        implicit, expected,
        "the implicit and explicit orders disagree on content"
    );
}

/// Grouping is a collation question, and the key type carries the collation to
/// answer it: `'Ada'` and `'ADA'` are one group under `NOCASE`, two under
/// `BINARY`.
#[test]
fn group_by_folds_under_the_columns_collation() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, nc TEXT COLLATE NOCASE, bin TEXT)",
    );
    for (id, text) in [(1, "Ada"), (2, "ADA"), (3, "ada"), (4, "Grace")] {
        run(
            &mut engine,
            &format!("INSERT INTO t VALUES ({id}, '{text}', '{text}')"),
        );
    }
    assert_eq!(
        answer(&mut engine, "SELECT nc, COUNT(*) FROM t GROUP BY nc").len(),
        2,
        "NOCASE should fold the three spellings of ada into one group"
    );
    assert_eq!(
        answer(&mut engine, "SELECT bin, COUNT(*) FROM t GROUP BY bin").len(),
        4,
        "BINARY should keep them apart"
    );
}

/// The streamed group key finds the same groups the collected one does.
///
/// A row whose group already exists probes the map with a key buffer reused
/// across rows and materialises nothing; only a row that opens a group builds
/// an owned key. That is a change to *when* the key exists, and it would be an
/// easy place to lose the part of the key that is not the values — the
/// collations, without which `'Ada'` and `'ADA'` stop being one group — or to
/// carry a stale value from the previous row into a shorter key.
///
/// So both paths are made to group the same rows and agree on every group, in
/// order, with the same representative row: `NULL` keys, which sort before
/// everything and are one group rather than none; a `NOCASE` column beside the
/// `BINARY` spelling of the same values; and multi-column keys, where only the
/// second column distinguishes two rows.
#[test]
fn a_streamed_group_key_finds_the_same_groups_a_collected_one_does() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE k (id INTEGER PRIMARY KEY, nc TEXT COLLATE NOCASE, bin TEXT, n INTEGER)",
    );
    for (id, nc, bin, n) in [
        (1, "'Ada'", "'Ada'", "1"),
        (2, "'ADA'", "'ADA'", "1"),
        (3, "'ada'", "'ada'", "2"),
        (4, "'Grace'", "'Grace'", "NULL"),
        (5, "NULL", "NULL", "1"),
        (6, "NULL", "'x'", "NULL"),
        (7, "'Ada'", "NULL", "2"),
    ] {
        run(
            &mut engine,
            &format!("INSERT INTO k VALUES ({id}, {nc}, {bin}, {n})"),
        );
    }

    for keys in [
        "nc",
        "bin",
        "n",
        "nc, n",
        "n, nc",
        "nc, bin",
        "bin, nc, n",
        "n, n",
    ] {
        for projection in ["COUNT(*)", "id, COUNT(*)", "COUNT(*), MIN(bin), MAX(nc)"] {
            assert_streamed_matches_collected(
                &mut engine,
                projection,
                &format!("k GROUP BY {keys}"),
            );
        }
    }

    // And what those groups are, pinned once: the collation decides how many
    // there are, and `NULL` is a group of its own rather than a row that
    // vanishes.
    assert_eq!(
        answer(&mut engine, "SELECT nc, COUNT(*) FROM k GROUP BY nc").len(),
        3,
        "NOCASE: the four spellings of ada, Grace, and the two NULLs"
    );
    assert_eq!(
        answer(&mut engine, "SELECT bin, COUNT(*) FROM k GROUP BY bin").len(),
        6,
        "BINARY: the same values, with the spellings of ada kept apart"
    );
    assert_eq!(
        answer(&mut engine, "SELECT nc, n, COUNT(*) FROM k GROUP BY nc, n").len(),
        5,
        "a multi-column key must distinguish rows the first column alone does not"
    );
}

/// The table both aggregate tie tests run over: `NULL`s in every position that
/// matters, three groups of two, a `NOCASE` column beside a `BINARY` one.
fn aggregate_engine() -> Engine {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, n INTEGER, r REAL, s TEXT, \
         nc TEXT COLLATE NOCASE)",
    );
    for (id, g, n, r, s, nc) in [
        (1, 1, "1", "1.5", "'b'", "'Bee'"),
        (2, 1, "NULL", "2.5", "'a'", "'apple'"),
        (3, 2, "3", "NULL", "NULL", "NULL"),
        (4, 2, "3", "4.0", "'c'", "'Cat'"),
        (5, 3, "-7", "0.5", "'a'", "'apple'"),
        (6, 3, "9", "1.0", "'a'", "'APPLE'"),
    ] {
        run(
            &mut engine,
            &format!("INSERT INTO t VALUES ({id}, {g}, {n}, {r}, {s}, {nc})"),
        );
    }
    engine
}

/// A `SUM` whose values arrive as integers and then as reals, in that order —
/// the promotion that has to carry the exact integer total into the real one.
/// Written as a `CAST` rather than trusted to a column's affinity so that what
/// is being tested is in the query.
const MIXED_SUM: &str = "SUM(CASE WHEN id <= 2 THEN CAST(n AS INTEGER) ELSE CAST(n AS REAL) END)";

/// An aggregate answers the same streamed as it does collected.
///
/// Streaming is a second path through the aggregate code, and this codebase's
/// recurring bug shape is two paths through one rule — so the standing rule is
/// that a fast path is tied to the slow one by a test.
///
/// The values reach the parts of the fold that are easy to get wrong from a
/// stream: `NULL`s every function skips, a `SUM` mixing integers and reals,
/// `MIN`/`MAX` across storage classes and under a collation, `DISTINCT`, and
/// `FILTER`, which narrows what one aggregate sees without touching the rest.
///
/// A tie test can only catch the two paths *disagreeing*, and since they now
/// share one step function ([`eval::AggFold::step`]) a broken step breaks both
/// alike — so `the_fold_answers_what_it_is_supposed_to` below pins the
/// arithmetic itself, and the two tests are only meaningful together.
#[test]
fn an_aggregate_streams_to_the_same_answer_it_collects() {
    let mut engine = aggregate_engine();

    let shapes = [
        "COUNT(*)",
        "COUNT(n)",
        "SUM(n)",
        "SUM(r)",
        MIXED_SUM,
        "AVG(n)",
        "AVG(n), AVG(r)",
        "MIN(n), MAX(n)",
        "MIN(s), MAX(s)",
        // The same values under a different collation: the fold's ordering has
        // to be the argument's, not the default.
        "MIN(nc), MAX(nc)",
        "COUNT(DISTINCT n)",
        "SUM(DISTINCT n), AVG(DISTINCT n)",
        "MIN(DISTINCT nc), MAX(DISTINCT s)",
        "COUNT(*) FILTER (WHERE n > 0)",
        "SUM(n) FILTER (WHERE id <> 5)",
        "MIN(n) FILTER (WHERE n > 0), MAX(nc) FILTER (WHERE id < 4)",
        "AVG(r) FILTER (WHERE r IS NOT NULL)",
        "COUNT(*), COUNT(n), SUM(n), AVG(r), MIN(s), MAX(s)",
        // A bare column beside an aggregate: which row represents the group is
        // observable, so it is pinned. Without this, taking the last row
        // instead of the first would go unnoticed — every other shape here
        // projects only aggregates.
        "s, COUNT(*)",
        "id, n, MIN(s)",
    ];

    for projection in shapes {
        for tail in ["", " GROUP BY g", " GROUP BY g HAVING COUNT(*) > 1"] {
            assert_streamed_matches_collected(&mut engine, projection, &format!("t{tail}"));
        }
    }

    // Empty input: no `GROUP BY` still answers one row, a `GROUP BY` answers
    // none. Both paths agree on that too.
    run(&mut engine, "DELETE FROM t");
    for projection in ["COUNT(*), SUM(n), MIN(s), AVG(n), MAX(nc)", "g, COUNT(*)"] {
        for tail in ["", " GROUP BY g"] {
            assert_streamed_matches_collected(&mut engine, projection, &format!("t{tail}"));
        }
    }
    assert_eq!(
        answer(&mut engine, "SELECT COUNT(*), SUM(n), MIN(s) FROM t"),
        vec![vec![
            "Integer(0)".to_string(),
            "Null".to_string(),
            "Null".to_string()
        ]],
        "an ungrouped aggregate over no rows must still answer one row"
    );
    assert!(
        answer(&mut engine, "SELECT g, COUNT(*) FROM t GROUP BY g").is_empty(),
        "a GROUP BY over no rows must answer no rows"
    );
    assert!(
        answer(&mut engine, "SELECT COUNT(*) FROM t HAVING COUNT(*) > 0").is_empty(),
        "HAVING must be able to reject the streamed row"
    );
}

/// What the two paths agree *on*.
///
/// Since they fold through one step function, a step that is wrong is wrong on
/// both sides and the tie test above stays green while every number is
/// different. This pins the numbers: the integer-to-real promotion carrying its
/// running total, `AVG`'s divisor counting only the values it summed,
/// `MIN`/`MAX` under the argument's collation rather than the default, and the
/// `NULL` that every one of them skips.
#[test]
fn the_fold_answers_what_it_is_supposed_to() {
    let mut engine = aggregate_engine();
    assert_eq!(
        answer(
            &mut engine,
            &format!(
                "SELECT SUM(n), COUNT(n), AVG(n), AVG(r), {MIXED_SUM}, MIN(s), MAX(s), \
                 MIN(nc), MAX(nc) FROM t"
            )
        ),
        vec![vec![
            // 1 + 3 + 3 - 7 + 9, with row 2's NULL skipped, and exact.
            "Integer(9)".to_string(),
            "Integer(5)".to_string(),
            // 9 / 5, over the five non-NULL values and not over six rows.
            "Real(1.8)".to_string(),
            // (1.5 + 2.5 + 4.0 + 0.5 + 1.0) / 5.
            "Real(1.9)".to_string(),
            // Integer 1 from row 1, then reals 3.0, 3.0, -7.0, 9.0: the
            // promotion has to carry the 1 across, so 9.0 and not 8.0.
            "Real(9.0)".to_string(),
            "Text(\"a\")".to_string(),
            "Text(\"c\")".to_string(),
            // Under NOCASE, 'apple' and 'APPLE' are equal and the first wins;
            // under BINARY the answer would be 'APPLE'. The collation the
            // argument carries is the one the fold has to compare under.
            "Text(\"apple\")".to_string(),
            "Text(\"Cat\")".to_string(),
        ]],
        "the fold's arithmetic, pinned independently of which path computes it"
    );
}

/// `SUM` refuses an integer overflow rather than wrapping or silently
/// promoting, folded incrementally exactly as it did folded at the end.
///
/// The exact sum of integers is what `SUM` promised, so this is an error in
/// SQLite and here — and a refusal the streamed path made and the collected one
/// did not (or the reverse) would be the worst kind of drift, because the two
/// answers would both look plausible.
#[test]
fn a_streamed_sum_refuses_the_overflow_a_collected_one_refuses() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE big (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    for (id, n) in [(1, "9223372036854775807"), (2, "1"), (3, "-1")] {
        run(&mut engine, &format!("INSERT INTO big VALUES ({id}, {n})"));
    }

    // Ungrouped, and again with the overflowing values inside one group of a
    // `GROUP BY` — the refusal has to survive being one group's answer.
    for tail in ["", " GROUP BY n > 0"] {
        assert_streamed_matches_collected(&mut engine, "SUM(n)", &format!("big{tail}"));
    }

    let refused = outcome(&mut engine, "SELECT SUM(n) FROM big").expect_err("i64::MAX + 1");
    assert!(
        refused.contains("integer overflow"),
        "the refusal must name what it hit, got: {refused}"
    );
    // The same values that overflow as a `SUM` are fine as an `AVG`, which
    // never promised an exact integer.
    assert_eq!(
        answer(&mut engine, "SELECT AVG(n) FROM big"),
        vec![vec![format!(
            "Real({:?})",
            (i64::MAX as f64 + 1.0 - 1.0) / 3.0
        )]],
        "AVG accumulates in a real and has no overflow to refuse"
    );
}
