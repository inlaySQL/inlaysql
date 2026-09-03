//! `EXPLAIN`: that it reports the access path the executor actually takes,
//! and that it takes none itself.
//!
//! The assertion with real value here is the *pair*: a query that can use an
//! index reports that it does, and the same query shape against a table with
//! no index reports a scan. Either half on its own passes for an `EXPLAIN`
//! that always says the same thing, which is the way this feature fails
//! silently — an `EXPLAIN` that claims an index for a query that scans ends
//! the investigation instead of starting it.

use inlaysql_core::{mem, Engine, Error, Value};

fn engine() -> Engine {
    mem::engine().expect("open in-memory engine")
}

fn run(engine: &mut Engine, sql: &str) {
    engine
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

/// The `detail` column of every `EXPLAIN` node, in tree order.
fn plan(engine: &mut Engine, sql: &str) -> Vec<String> {
    plan_with(engine, sql, &[])
}

fn plan_with(engine: &mut Engine, sql: &str, params: &[Value]) -> Vec<String> {
    let result = engine
        .query(sql, params)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    assert_eq!(
        result.columns,
        vec!["id".to_string(), "parent".to_string(), "detail".to_string()],
        "`{sql}` did not report EXPLAIN's columns"
    );
    result
        .rows
        .iter()
        .map(|row| match &row[2] {
            Value::Text(detail) => detail.as_str().to_string(),
            other => panic!("`{sql}`: detail was {other:?}, not text"),
        })
        .collect()
}

/// The whole tree as `id|parent|detail` lines, for the tests that care about
/// the shape and not only the wording.
fn tree(engine: &mut Engine, sql: &str) -> Vec<String> {
    engine
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .rows
        .iter()
        .map(|row| {
            let (Value::Integer(id), Value::Integer(parent), Value::Text(detail)) =
                (&row[0], &row[1], &row[2])
            else {
                panic!("`{sql}`: unexpected row {row:?}")
            };
            format!("{id}|{parent}|{detail}")
        })
        .collect()
}

fn refuse(engine: &mut Engine, sql: &str) -> Error {
    engine
        .execute(sql, &[])
        .expect_err(&format!("`{sql}` was accepted"))
}

/// Whether any node says a given thing.
fn says(plan: &[String], fragment: &str) -> bool {
    plan.iter().any(|detail| detail.contains(fragment))
}

fn assert_says(plan: &[String], fragment: &str) {
    assert!(
        says(plan, fragment),
        "expected a node containing `{fragment}`, got {plan:#?}"
    );
}

fn assert_never_says(plan: &[String], fragment: &str) {
    assert!(
        !says(plan, fragment),
        "expected no node containing `{fragment}`, got {plan:#?}"
    );
}

// ------------------------------------------------------- scan versus index

/// The pair the whole feature rests on: one table with an index on the
/// filtered column, one without, and the *same* query text against each.
#[test]
fn the_same_query_reports_an_index_where_there_is_one_and_a_scan_where_there_is_not() {
    let mut engine = engine();
    for table in ["indexed", "bare"] {
        run(
            &mut engine,
            &format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, author TEXT, year INTEGER)"),
        );
        run(
            &mut engine,
            &format!("INSERT INTO {table} VALUES (1, 'ada', 1843)"),
        );
    }
    run(
        &mut engine,
        "CREATE INDEX indexed_author ON indexed (author) USING BTREE",
    );

    let with = plan(
        &mut engine,
        "EXPLAIN SELECT year FROM indexed WHERE author = 'ada'",
    );
    assert_eq!(
        with,
        vec!["SEARCH indexed USING INDEX indexed_author (author=?)"],
        "the index exists and covers the filter, so it has to be reported"
    );

    let without = plan(
        &mut engine,
        "EXPLAIN SELECT year FROM bare WHERE author = 'ada'",
    );
    assert_eq!(
        without,
        vec!["SCAN bare"],
        "no index covers `author` on `bare`, and claiming one would be the \
         failure this test exists for"
    );
}

#[test]
fn dropping_the_index_moves_the_plan_back_to_a_scan() {
    // The regression this catches is an `EXPLAIN` computed from the statement
    // text rather than from the catalog as it stands now.
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, author TEXT)",
    );
    run(
        &mut engine,
        "CREATE INDEX t_author ON t (author) USING BTREE",
    );
    assert_says(
        &plan(&mut engine, "EXPLAIN SELECT id FROM t WHERE author = 'ada'"),
        "USING INDEX t_author",
    );

    run(&mut engine, "DROP INDEX t_author");
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT id FROM t WHERE author = 'ada'"),
        vec!["SCAN t"]
    );
}

#[test]
fn an_integer_primary_key_equality_is_reported_as_a_point_lookup() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n TEXT)",
    );
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT n FROM t WHERE id = 7"),
        vec!["SEARCH t USING INTEGER PRIMARY KEY (rowid=?)"]
    );
    // A range on the row id is not a point lookup, and there is no secondary
    // index to fall back to.
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT n FROM t WHERE id > 7"),
        vec!["SCAN t"]
    );
}

#[test]
fn a_range_over_an_index_reports_both_of_its_bounds() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, year INTEGER)",
    );
    run(&mut engine, "CREATE INDEX t_year ON t (year)");
    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN SELECT id FROM t WHERE year >= 1800 AND year < 1900"
        ),
        vec!["SEARCH t USING INDEX t_year (year>? AND year<?)"]
    );
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT id FROM t WHERE year > 1800"),
        vec!["SEARCH t USING INDEX t_year (year>?)"]
    );
}

#[test]
fn a_composite_index_reports_how_much_of_its_key_the_filter_bound() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, author TEXT, year INTEGER)",
    );
    run(
        &mut engine,
        "CREATE INDEX t_author_year ON t (author, year) USING BTREE",
    );

    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN SELECT id FROM t WHERE author = 'ada' AND year = 1843"
        ),
        vec!["SEARCH t USING INDEX t_author_year (author=? AND year=?)"]
    );
    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN SELECT id FROM t WHERE author = 'ada' AND year > 1800"
        ),
        vec!["SEARCH t USING INDEX t_author_year (author=? AND year>?)"]
    );
    // Only the *trailing* column is bound, so nothing is contiguous and the
    // index cannot answer at all — the case that would look like a win to a
    // naive reader of the plan.
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT id FROM t WHERE year = 1843"),
        vec!["SCAN t"]
    );
}

#[test]
fn an_or_filter_is_reported_as_a_scan_because_an_index_cannot_answer_it() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, author TEXT)",
    );
    run(
        &mut engine,
        "CREATE INDEX t_author ON t (author) USING BTREE",
    );
    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN SELECT id FROM t WHERE author = 'ada' OR author = 'bob'"
        ),
        vec!["SCAN t"],
        "one side of an OR cannot narrow the other, so the index is not used"
    );
}

/// The access path depends on the *bound value*, not only on the text — which
/// is why `EXPLAIN` is answered at execution rather than at prepare time.
#[test]
fn a_bound_parameter_decides_the_access_path_and_explain_follows_it() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n TEXT)",
    );
    let sql = "EXPLAIN SELECT n FROM t WHERE id = ?";
    assert_eq!(
        plan_with(&mut engine, sql, &[Value::Integer(3)]),
        vec!["SEARCH t USING INTEGER PRIMARY KEY (rowid=?)"]
    );
    assert_eq!(
        plan_with(&mut engine, sql, &[Value::Text("three".into())]),
        vec!["SCAN t"],
        "a text key cannot address an INTEGER PRIMARY KEY, so this really is a scan"
    );
}

// ------------------------------------------------------------------- joins

/// Both join strategies, from the same two tables and the same `ON` — only
/// the `LIMIT` differs, which is exactly what the chooser keys on.
#[test]
fn a_full_scan_join_hashes_and_a_limited_one_probes_the_index() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, author_id INTEGER)",
    );
    run(
        &mut engine,
        "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT)",
    );
    run(&mut engine, "INSERT INTO authors VALUES (1, 'ada')");
    run(&mut engine, "INSERT INTO posts VALUES (1, 1)");

    let hashed = plan(
        &mut engine,
        "EXPLAIN SELECT posts.id, authors.name FROM posts \
         JOIN authors ON posts.author_id = authors.id",
    );
    assert_eq!(
        hashed,
        vec!["SCAN posts", "HASH JOIN authors (BUILD ON authors.id)"],
        "with no LIMIT the outer side is read end to end, which is what pays \
         for the hash build"
    );

    let probed = plan(
        &mut engine,
        "EXPLAIN SELECT posts.id, authors.name FROM posts \
         JOIN authors ON posts.author_id = authors.id LIMIT 5",
    );
    assert_eq!(
        probed,
        vec![
            "SCAN posts",
            "INDEX NESTED LOOP JOIN authors USING INTEGER PRIMARY KEY (rowid=?)",
            "LIMIT 5 PUSHED INTO SCAN",
        ],
        "a LIMIT stops the outer scan early, so the per-row probe is cheaper \
         than a whole-table hash build"
    );
}

#[test]
fn a_join_probe_names_the_secondary_index_it_descends() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, slug TEXT)",
    );
    run(
        &mut engine,
        "CREATE TABLE tags (id INTEGER PRIMARY KEY, slug TEXT)",
    );
    run(
        &mut engine,
        "CREATE INDEX tags_slug ON tags (slug) USING BTREE",
    );
    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN SELECT posts.id FROM posts JOIN tags ON posts.slug = tags.slug LIMIT 1"
        ),
        vec![
            "SCAN posts",
            "INDEX NESTED LOOP JOIN tags USING INDEX tags_slug (slug=?)",
            "LIMIT 1 PUSHED INTO SCAN",
        ]
    );
}

#[test]
fn a_join_with_neither_a_hash_key_nor_an_index_says_it_materialises() {
    let mut engine = engine();
    // A `NUMERIC` column: no declared storage class, so it can neither be
    // hashed nor answered from an ordered index.
    run(
        &mut engine,
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k NUMERIC)",
    );
    run(
        &mut engine,
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k NUMERIC)",
    );
    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN SELECT a.id FROM a JOIN b ON a.k = b.k LIMIT 1"
        ),
        vec![
            "SCAN a",
            "NESTED LOOP JOIN b (MATERIALISED: no index or hash key applies)",
            "LIMIT 1 PUSHED INTO SCAN",
        ]
    );
}

#[test]
fn a_left_join_is_marked_as_one() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE a (id INTEGER PRIMARY KEY)");
    run(
        &mut engine,
        "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER)",
    );
    let plan = plan(
        &mut engine,
        "EXPLAIN SELECT a.id FROM a LEFT JOIN b ON a.id = b.a_id",
    );
    assert_says(&plan, "LEFT HASH JOIN b");
}

// -------------------------------------------------------------- retrieval

/// The three retrieval paths are the ones with no visible difference at all
/// from the caller's side: same `SELECT`, same `ORDER BY score`.
#[test]
fn a_retrieval_query_names_the_index_that_answers_it() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(4))",
    );
    run(&mut engine, "CREATE INDEX docs_body ON docs (body)");
    run(
        &mut engine,
        "CREATE INDEX docs_embedding ON docs (embedding)",
    );
    engine
        .execute(
            "INSERT INTO docs VALUES (1, 'rust database', ?)",
            &[Value::Vector(vec![1.0, 0.0, 0.0, 0.0])],
        )
        .expect("insert doc");

    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN SELECT id, bm25_score(body, 'rust') AS score FROM docs ORDER BY score DESC"
        ),
        vec![
            "SEARCH docs USING FULL-TEXT INDEX docs_body (body) FOR bm25_score",
            "SORT FOR ORDER BY",
        ]
    );

    assert_says(
        &plan_with(
            &mut engine,
            "EXPLAIN SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC",
            &[Value::Vector(vec![1.0, 0.0, 0.0, 0.0])],
        ),
        // The column list is rendered the way `CREATE INDEX` spells it,
        // operator class included: which distance ranked the rows decides
        // which rows came back, so a plan that named the index but not its
        // metric described two different queries with one line.
        "SEARCH docs USING VECTOR INDEX docs_embedding (embedding vector_cosine_ops) \
         FOR vector_score",
    );
}

#[test]
fn a_fused_query_lists_both_of_the_rankings_it_combines() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(4))",
    );
    run(&mut engine, "CREATE INDEX docs_body ON docs (body)");
    run(
        &mut engine,
        "CREATE INDEX docs_embedding ON docs (embedding)",
    );
    engine
        .execute(
            "INSERT INTO docs VALUES (1, 'rust database', ?)",
            &[Value::Vector(vec![1.0, 0.0, 0.0, 0.0])],
        )
        .expect("insert doc");

    let tree = engine
        .query(
            "EXPLAIN SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, 'rust')) \
             AS score FROM docs ORDER BY score DESC LIMIT 5",
            &[Value::Vector(vec![1.0, 0.0, 0.0, 0.0])],
        )
        .expect("explain fused query");
    let details: Vec<String> = tree
        .rows
        .iter()
        .map(|row| row[2].as_str().expect("text detail").to_string())
        .collect();
    assert_says(&details, "FUSE 2 RANKED LIST(S)");
    assert_says(&details, "USING VECTOR INDEX docs_embedding");
    assert_says(&details, "USING FULL-TEXT INDEX docs_body");

    // The two leaves hang off the fuse node, not off the query root — that is
    // what says they are one ranked answer rather than two row sources.
    let fuse_id = match &tree.rows[0][0] {
        Value::Integer(id) => *id,
        other => panic!("unexpected id {other:?}"),
    };
    for row in &tree.rows[1..3] {
        assert_eq!(
            row[1],
            Value::Integer(fuse_id),
            "a fused leaf should be a child of the FUSE node"
        );
    }
}

#[test]
fn a_filtered_retrieval_says_the_where_is_pushed_into_the_search() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, lang TEXT)",
    );
    run(&mut engine, "CREATE INDEX docs_body ON docs (body)");
    run(&mut engine, "INSERT INTO docs VALUES (1, 'rust', 'en')");
    assert_says(
        &plan(
            &mut engine,
            "EXPLAIN SELECT id, bm25_score(body, 'rust') AS score FROM docs \
             WHERE lang = 'en' ORDER BY score DESC",
        ),
        "(WHERE PUSHED INTO RETRIEVAL)",
    );
}

/// A retrieval query with no index is refused when it runs, so `EXPLAIN` has
/// to refuse it too — describing a plan the engine would reject is a promise
/// the next statement cannot keep.
#[test]
fn explaining_a_retrieval_query_with_no_index_fails_the_way_running_it_would() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)",
    );
    let explained = refuse(
        &mut engine,
        "EXPLAIN SELECT id, bm25_score(body, 'rust') AS score FROM docs ORDER BY score DESC",
    );
    let ran = refuse(
        &mut engine,
        "SELECT id, bm25_score(body, 'rust') AS score FROM docs ORDER BY score DESC",
    );
    assert!(matches!(explained, Error::Index(_)), "got {explained:?}");
    assert_eq!(explained.to_string(), ran.to_string());
}

// ------------------------------------------------- what happens after rows

#[test]
fn the_blocking_operators_are_reported_and_so_is_a_limit_they_stop() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );

    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT n FROM t LIMIT 3"),
        vec!["SCAN t", "LIMIT 3 PUSHED INTO SCAN"]
    );
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT n FROM t ORDER BY n LIMIT 3"),
        vec![
            "SCAN t",
            "SORT FOR ORDER BY",
            "LIMIT APPLIED AFTER MATERIALISING"
        ],
        "a sort chooses which rows survive, so the first three off the scan \
         are not the first three of the answer"
    );
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT DISTINCT n FROM t"),
        vec!["SCAN t", "FOLD FOR DISTINCT"]
    );
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT n, COUNT(*) FROM t GROUP BY n"),
        vec!["SCAN t", "SORT FOR GROUP BY"]
    );
    assert_says(
        &plan(
            &mut engine,
            "EXPLAIN SELECT n, ROW_NUMBER() OVER (ORDER BY n) FROM t",
        ),
        "EVALUATE 1 WINDOW FUNCTION(S)",
    );
}

#[test]
fn an_offset_is_folded_into_the_pushed_limit() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT id FROM t LIMIT 5 OFFSET 10"),
        vec!["SCAN t", "LIMIT 15 PUSHED INTO SCAN"],
        "the scan has to produce the offset rows too before it may stop"
    );
}

// --------------------------------------------------- subqueries, compounds

#[test]
fn a_correlated_subquery_is_distinguished_from_one_that_runs_once() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER)",
    );
    run(
        &mut engine,
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER)",
    );

    assert_says(
        &plan(
            &mut engine,
            "EXPLAIN SELECT id FROM a WHERE k IN (SELECT k FROM b)",
        ),
        "LIST SUBQUERY 0 (RUN ONCE)",
    );
    assert_says(
        &plan(
            &mut engine,
            "EXPLAIN SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
        ),
        "CORRELATED EXISTS SUBQUERY 0 (RUN PER ROW)",
    );
}

#[test]
fn a_derived_table_says_it_is_materialised_and_its_own_plan_hangs_off_it() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    assert_eq!(
        tree(
            &mut engine,
            "EXPLAIN SELECT x.id FROM (SELECT id, n FROM t WHERE n = 3) AS x",
        ),
        vec![
            "1|0|SCAN x (SUBQUERY, MATERIALISED)",
            "2|1|SEARCH t USING INDEX t_n (n=?)",
        ]
    );
}

#[test]
fn a_compound_query_reports_both_arms_under_one_node() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE a (id INTEGER PRIMARY KEY)");
    run(&mut engine, "CREATE TABLE b (id INTEGER PRIMARY KEY)");
    assert_eq!(
        tree(
            &mut engine,
            "EXPLAIN SELECT id FROM a UNION ALL SELECT id FROM b",
        ),
        vec!["1|0|COMPOUND QUERY (UNION ALL)", "2|1|SCAN a", "3|1|SCAN b",]
    );
}

#[test]
fn a_select_with_no_from_is_one_constant_row() {
    let mut engine = engine();
    assert_eq!(
        plan(&mut engine, "EXPLAIN SELECT 1 + 1"),
        vec!["SCAN CONSTANT ROW"]
    );
}

// ------------------------------------------------------ write statements

/// The value of `EXPLAIN` on a write is the same as on a read: which rows it
/// has to touch. It goes through the same chooser, so the answers match.
#[test]
fn a_delete_reports_the_same_access_path_the_matching_select_does() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, author TEXT)",
    );
    run(
        &mut engine,
        "CREATE INDEX t_author ON t (author) USING BTREE",
    );

    assert_eq!(
        plan(&mut engine, "EXPLAIN DELETE FROM t WHERE author = 'ada'"),
        vec!["DELETE FROM t", "SEARCH t USING INDEX t_author (author=?)"]
    );
    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN UPDATE t SET author = 'bob' WHERE id = 1"
        ),
        vec!["UPDATE t", "SEARCH t USING INTEGER PRIMARY KEY (rowid=?)"]
    );
    assert_eq!(
        plan(&mut engine, "EXPLAIN DELETE FROM t"),
        vec!["DELETE FROM t", "SCAN t"],
        "no filter is every row, and saying so is the point"
    );
}

#[test]
fn an_insert_reports_where_its_rows_come_from() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE src (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(
        &mut engine,
        "CREATE TABLE dst (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    assert_eq!(
        plan(&mut engine, "EXPLAIN INSERT INTO dst (n) VALUES (1), (2)"),
        vec!["INSERT INTO dst", "VALUES (2 ROW(S))"]
    );
    assert_eq!(
        plan(
            &mut engine,
            "EXPLAIN INSERT INTO dst (n) SELECT n FROM src WHERE id = 4"
        ),
        vec![
            "INSERT INTO dst",
            "SEARCH src USING INTEGER PRIMARY KEY (rowid=?)",
        ]
    );
}

/// The property that makes `EXPLAIN` safe to type at a production prompt.
#[test]
fn explain_never_runs_the_statement_it_describes() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    for id in 1..=3 {
        run(&mut engine, &format!("INSERT INTO t VALUES ({id}, {id})"));
    }
    let before = engine.query("SELECT id, n FROM t", &[]).expect("read").rows;

    for sql in [
        "EXPLAIN INSERT INTO t VALUES (99, 99)",
        "EXPLAIN UPDATE t SET n = n + 1",
        "EXPLAIN DELETE FROM t",
        "EXPLAIN SELECT id FROM t",
    ] {
        let explained = engine
            .query(sql, &[])
            .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
        assert!(!explained.rows.is_empty(), "`{sql}` described nothing");
        let after = engine.query("SELECT id, n FROM t", &[]).expect("read").rows;
        assert_eq!(after, before, "`{sql}` changed the table");
    }

    // Nor does it advance the change feed, which is what a replica would see.
    assert_eq!(
        engine
            .query("SELECT COUNT(*) FROM t", &[])
            .expect("count")
            .rows,
        vec![vec![Value::Integer(3)]]
    );
}

/// `EXPLAIN` reads nothing, so it cannot fail on a statement whose *execution*
/// would need a row — and it must not be counted as a write inside a
/// transaction either.
#[test]
fn explaining_a_write_inside_a_transaction_leaves_the_transaction_read_only() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    engine.begin().expect("begin");
    let _ = plan(&mut engine, "EXPLAIN INSERT INTO t VALUES (1)");
    engine.rollback().expect("rollback");
    assert_eq!(
        engine
            .query("SELECT COUNT(*) FROM t", &[])
            .expect("count")
            .rows,
        vec![vec![Value::Integer(0)]]
    );
}

// ------------------------------------------------------------- refusals

#[test]
fn explain_query_plan_is_accepted_as_sqlites_spelling_of_the_same_request() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    assert_eq!(
        plan(&mut engine, "EXPLAIN QUERY PLAN SELECT id FROM t"),
        plan(&mut engine, "EXPLAIN SELECT id FROM t")
    );
}

#[test]
fn explain_analyze_is_refused_rather_than_answered_with_a_plan() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    let error = refuse(&mut engine, "EXPLAIN ANALYZE SELECT id FROM t");
    assert!(matches!(error, Error::Unsupported(_)), "got {error:?}");
    assert!(
        error.to_string().contains("ANALYZE"),
        "the refusal has to name the clause: {error}"
    );
}

#[test]
fn explaining_something_with_no_query_plan_is_refused_by_name() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    for sql in [
        "EXPLAIN CREATE TABLE u (id INTEGER)",
        "EXPLAIN DROP TABLE t",
        "EXPLAIN CREATE INDEX t_id ON t (id)",
        "EXPLAIN BEGIN",
        "EXPLAIN COMMIT",
    ] {
        let error = refuse(&mut engine, sql);
        assert!(
            matches!(error, Error::Unsupported(_)),
            "`{sql}` should be refused, got {error:?}"
        );
    }
}

#[test]
fn explaining_a_statement_that_does_not_plan_reports_the_planners_own_error() {
    let mut engine = engine();
    let error = refuse(&mut engine, "EXPLAIN SELECT id FROM missing");
    assert!(
        error.to_string().contains("missing"),
        "the underlying error has to survive being wrapped: {error}"
    );
}

/// The engine is not the place `DESCRIBE <table>` is answered — the MySQL
/// shim is — and quietly reporting a plan for it would be a different answer
/// to a different question.
#[test]
fn explaining_a_bare_table_name_is_refused() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    let error = refuse(&mut engine, "EXPLAIN t");
    assert!(matches!(error, Error::Unsupported(_)), "got {error:?}");
}

/// `EXPLAIN` says the same thing every time for the same inputs, which is what
/// makes it usable in a test at all.
#[test]
fn the_plan_is_stable_across_repeated_calls() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)",
    );
    run(&mut engine, "CREATE INDEX t_s ON t (s) USING BTREE");
    let sql = "EXPLAIN SELECT id FROM t WHERE s = 'x'";
    let first = plan(&mut engine, sql);
    assert_eq!(first, plan(&mut engine, sql));
    assert_never_says(&first, "SCAN");
}

/// A prepared `EXPLAIN` reports its own three columns before it runs, which is
/// what a wire client reads at `COM_STMT_PREPARE`.
#[test]
fn a_prepared_explain_describes_its_own_result_set() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    let statement = engine.prepare("EXPLAIN SELECT id FROM t").expect("prepare");
    let names: Vec<&str> = statement
        .columns()
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, vec!["id", "parent", "detail"]);
    assert!(
        statement.is_read_only(),
        "EXPLAIN of anything is a read, which is what keeps it off the write path"
    );
}

// ------------------------------------------------------- MIN/MAX (AHL-546)

/// `MIN`/`MAX` of the rowid reports the optimisation and never a scan.
#[test]
fn min_max_of_the_rowid_reports_the_optimisation() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    let plan = plan(&mut engine, "EXPLAIN SELECT MIN(id), MAX(id) FROM t");
    assert_says(&plan, "MIN/MAX OPTIMIZATION");
    assert_says(&plan, "USING INTEGER PRIMARY KEY");
    assert_never_says(&plan, "SCAN t");
}

/// A column with a leading B-tree index reports the optimisation by that
/// index's name, and a column with none falls back to a scan.
#[test]
fn min_max_of_an_indexed_column_names_the_index_and_falls_back_without_one() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, indexed INTEGER, bare INTEGER)",
    );
    run(
        &mut engine,
        "CREATE INDEX t_indexed ON t (indexed) USING BTREE",
    );

    let indexed_plan = plan(&mut engine, "EXPLAIN SELECT MIN(indexed) FROM t");
    assert_says(&indexed_plan, "MIN/MAX OPTIMIZATION");
    assert_says(&indexed_plan, "USING INDEX t_indexed");

    let bare_plan = plan(&mut engine, "EXPLAIN SELECT MIN(bare) FROM t");
    assert_never_says(&bare_plan, "MIN/MAX OPTIMIZATION");
    assert_says(&bare_plan, "SCAN t");
}

/// `COUNT(*)` forces a scan even alongside `MIN`/`MAX`, because this engine
/// keeps no transactionally exact row count — see
/// `Engine::try_min_max_scalar`'s doc.
#[test]
fn count_star_alongside_min_max_still_scans() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    let plan = plan(
        &mut engine,
        "EXPLAIN SELECT COUNT(*), MIN(id), MAX(id) FROM t",
    );
    assert_never_says(&plan, "MIN/MAX OPTIMIZATION");
    assert_says(&plan, "SCAN t");
}

/// Every one of `WHERE`, `GROUP BY`, `DISTINCT` and a join sends the
/// statement to the general path, mutation by mutation: dropping any one of
/// `try_min_max_scalar`'s conditions has to fail one of these.
#[test]
fn where_group_by_distinct_and_a_join_all_fall_back_to_the_general_path() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(
        &mut engine,
        "CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER)",
    );

    for sql in [
        "EXPLAIN SELECT MIN(id) FROM t WHERE n > 0",
        "EXPLAIN SELECT n, MIN(id) FROM t GROUP BY n",
        "EXPLAIN SELECT DISTINCT MIN(id) FROM t",
        "EXPLAIN SELECT MIN(t.id) FROM t JOIN u ON u.t_id = t.id",
    ] {
        let plan = plan(&mut engine, sql);
        assert_never_says(&plan, "MIN/MAX OPTIMIZATION");
    }
}

/// A projection that reads a raw column alongside the aggregates falls back:
/// this rewrite never holds the representative row the general path's
/// answer for that column would come from.
#[test]
fn a_raw_column_in_the_projection_falls_back() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    let plan = plan(&mut engine, "EXPLAIN SELECT n, MIN(id) FROM t");
    assert_never_says(&plan, "MIN/MAX OPTIMIZATION");
}

/// `HAVING`, `COUNT(DISTINCT ...)`, `FILTER`, a non-column argument and a
/// derived `FROM` each have to fall back too — the remaining conditions
/// `min_max_scalar_shape` checks beyond the ones already covered above.
#[test]
fn having_distinct_filter_expression_and_a_derived_table_all_fall_back() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 5), (2, 5), (3, 9)");

    for sql in [
        "EXPLAIN SELECT MIN(id) FROM t HAVING MIN(id) > 0",
        "EXPLAIN SELECT MIN(DISTINCT n) FROM t",
        "EXPLAIN SELECT MIN(id) FILTER (WHERE n > 0) FROM t",
        "EXPLAIN SELECT MIN(id + 1) FROM t",
        "EXPLAIN SELECT MIN(id) FROM (SELECT * FROM t)",
    ] {
        let plan = plan(&mut engine, sql);
        assert_never_says(&plan, "MIN/MAX OPTIMIZATION");
    }
}
