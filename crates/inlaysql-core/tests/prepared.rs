//! Prepared statements: parse once, bind many times, and refuse to run against
//! a schema the plan was not built for.
//!
//! The "parses once" claim is counted, not timed — `Engine::statements_parsed`
//! is incremented by the one function in the crate that calls the parser, so a
//! test can assert the exact number rather than hope a stopwatch agrees. The
//! point-lookup claim is counted too, through the same `Storage::scan` wrapper
//! `primary_key.rs` uses: a prepared `WHERE id = ?` has to seek, not scan.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::row::RowBuf;
use inlaysql_core::traits::{RowId, Storage};
use inlaysql_core::{Engine, EngineOptions, Error, Result, Value};

/// The row count every `scan_batch` call asked for, in call order.
type BatchSizes = Rc<RefCell<Vec<usize>>>;

/// `MemStorage` that counts how often the engine falls back to a full scan,
/// and records how many rows each of those scans asked for.
struct CountingStorage {
    inner: MemStorage,
    scans: Rc<Cell<usize>>,
    batch_sizes: BatchSizes,
}

impl Storage for CountingStorage {
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        self.inner.put_row(table, id, bytes)
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        self.inner.get_row(table, id)
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        self.inner.delete_row(table, id)
    }

    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        self.scans.set(self.scans.get() + 1);
        self.batch_sizes.borrow_mut().push(limit);
        self.inner.scan_batch(table, after, limit)
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put_meta(key, bytes)
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get_meta(key)
    }

    fn put_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.inner.put_index_entry(key)
    }

    fn delete_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.inner.delete_index_entry(key)
    }

    fn scan_index_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        self.inner.scan_index_range(start, end)
    }

    fn commit(&mut self) -> Result<()> {
        self.inner.commit()
    }

    fn rollback(&mut self) -> Result<()> {
        self.inner.rollback()
    }
}

fn counting_engine() -> (Engine, Rc<Cell<usize>>) {
    counting_engine_with_options(EngineOptions::default())
}

fn counting_engine_with_options(options: EngineOptions) -> (Engine, Rc<Cell<usize>>) {
    let (engine, scans, _) = batch_recording_engine(options);
    (engine, scans)
}

/// [`counting_engine_with_options`], also handing back the row count every
/// `scan_batch` call asked for, in call order.
fn batch_recording_engine(options: EngineOptions) -> (Engine, Rc<Cell<usize>>, BatchSizes) {
    let scans = Rc::new(Cell::new(0));
    let batch_sizes = Rc::new(RefCell::new(Vec::new()));
    let engine = Engine::open_with_options(
        Box::new(CountingStorage {
            inner: MemStorage::new(),
            scans: scans.clone(),
            batch_sizes: batch_sizes.clone(),
        }),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
        options,
    )
    .expect("open");
    (engine, scans, batch_sizes)
}

fn seeded_join(cache_bytes: usize) -> (Engine, Rc<Cell<usize>>) {
    let (mut engine, scans) = counting_engine_with_options(EngineOptions {
        hash_join_cache_bytes: cache_bytes,
        ..EngineOptions::default()
    });
    engine
        .execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .unwrap();
    engine
        .execute(
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
            &[],
        )
        .unwrap();
    for id in 1..=2 {
        engine
            .execute(
                "INSERT INTO users VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(format!("user-{id}").into())],
            )
            .unwrap();
    }
    for id in 1..=4 {
        engine
            .execute(
                "INSERT INTO posts VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Integer(1 + (id - 1) % 2),
                    Value::Text(format!("post-{id}").into()),
                ],
            )
            .unwrap();
    }
    scans.set(0);
    (engine, scans)
}

/// A table of `n` rows keyed by `INTEGER PRIMARY KEY`.
fn seeded(rows: i64) -> (Engine, Rc<Cell<usize>>) {
    let (mut engine, scans) = counting_engine();
    engine
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    for id in 1..=rows {
        engine
            .execute(
                "INSERT INTO kv (id, body) VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(format!("row-{id}").into())],
            )
            .unwrap();
    }
    scans.set(0);
    (engine, scans)
}

#[test]
fn a_prepared_statement_is_parsed_once_however_often_it_runs() {
    let (mut engine, _) = seeded(20);
    let before = engine.statements_parsed();

    let lookup = engine.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
    for id in 1..=20 {
        let rows = engine.run_query(&lookup, &[Value::Integer(id)]).unwrap();
        assert_eq!(
            rows.rows,
            vec![vec![Value::Text(format!("row-{id}").into())]]
        );
    }

    assert_eq!(
        engine.statements_parsed() - before,
        1,
        "twenty executions of one prepared statement must parse exactly once"
    );
}

#[test]
fn a_prepared_hash_join_reuses_its_inner_build_on_the_same_snapshot() {
    let (mut engine, scans) = seeded_join(1024 * 1024);
    let join = engine
        .prepare(
            "SELECT users.name, posts.title \
             FROM users JOIN posts ON posts.user_id = users.id",
        )
        .unwrap();

    let before = scans.get();
    assert_eq!(engine.run_query(&join, &[]).unwrap().rows.len(), 4);
    let first = scans.get();
    assert_eq!(first - before, 2, "first run scans outer and inner");

    assert_eq!(engine.run_query(&join, &[]).unwrap().rows.len(), 4);
    assert_eq!(
        scans.get() - first,
        1,
        "same-snapshot rerun scans only the outer table"
    );
}

#[test]
fn a_committed_row_change_invalidates_the_prepared_hash_build() {
    let (mut engine, scans) = seeded_join(1024 * 1024);
    let join = engine
        .prepare(
            "SELECT users.name, posts.title \
             FROM users JOIN posts ON posts.user_id = users.id",
        )
        .unwrap();
    assert_eq!(engine.run_query(&join, &[]).unwrap().rows.len(), 4);

    engine
        .execute("INSERT INTO posts VALUES (5, 1, 'post-5')", &[])
        .unwrap();
    let before = scans.get();
    assert_eq!(engine.run_query(&join, &[]).unwrap().rows.len(), 5);
    assert_eq!(
        scans.get() - before,
        2,
        "a newer write version rebuilds the inner before returning rows"
    );
}

#[test]
fn an_open_transaction_with_writes_bypasses_the_committed_hash_build() {
    let (mut engine, scans) = seeded_join(1024 * 1024);
    let join = engine
        .prepare(
            "SELECT users.name, posts.title \
             FROM users JOIN posts ON posts.user_id = users.id",
        )
        .unwrap();
    assert_eq!(engine.run_query(&join, &[]).unwrap().rows.len(), 4);

    engine.begin().unwrap();
    engine
        .execute("INSERT INTO posts VALUES (5, 1, 'pending')", &[])
        .unwrap();
    let before = scans.get();
    assert_eq!(engine.run_query(&join, &[]).unwrap().rows.len(), 5);
    assert_eq!(
        scans.get() - before,
        2,
        "read-your-writes cannot reuse a build from the committed snapshot"
    );
    engine.rollback().unwrap();
}

#[test]
fn a_zero_hash_cache_budget_rebuilds_normally() {
    let (mut engine, scans) = seeded_join(0);
    let join = engine
        .prepare(
            "SELECT users.name, posts.title \
             FROM users JOIN posts ON posts.user_id = users.id",
        )
        .unwrap();

    assert_eq!(engine.run_query(&join, &[]).unwrap().rows.len(), 4);
    let after_first = scans.get();
    assert_eq!(engine.run_query(&join, &[]).unwrap().rows.len(), 4);
    assert_eq!(
        scans.get() - after_first,
        2,
        "disabled cache retains no inner build"
    );
}

#[test]
fn the_one_shot_path_parses_every_time() {
    // The baseline the prepared path is measured against — if this ever stops
    // being true, the test above stops proving anything.
    let (mut engine, _) = seeded(20);
    let before = engine.statements_parsed();
    for id in 1..=20 {
        engine
            .query("SELECT body FROM kv WHERE id = ?", &[Value::Integer(id)])
            .unwrap();
    }
    assert_eq!(engine.statements_parsed() - before, 20);
}

#[test]
fn a_prepared_point_read_still_seeks_rather_than_scans() {
    let (mut engine, scans) = seeded(20);
    let lookup = engine.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
    for id in 1..=20 {
        engine.run_query(&lookup, &[Value::Integer(id)]).unwrap();
    }
    assert_eq!(
        scans.get(),
        0,
        "a bound `?` key must pin the row id the way a literal does"
    );
}

/// AHL-532: a `LIMIT` over an unfiltered scan sizes the scan's first batch
/// to the rows the statement can consume, not to the default. `LIMIT 3`
/// reads three rows, not thirty-two it then drops — on the `LIMIT 10` joins
/// that was a second leaf read and admitted for nothing (`PERF.md`). A
/// filter puts the default back, because how many rows it will pass is
/// unknown; the hint is a batch size, never a bound on the answer.
#[test]
fn a_limited_unfiltered_scan_asks_for_its_limit_not_the_default_batch() {
    let (mut engine, _, batch_sizes) = batch_recording_engine(EngineOptions::default());
    engine
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    engine
        .execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .unwrap();
    engine
        .execute(
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
            &[],
        )
        .unwrap();
    engine
        .execute(
            "CREATE INDEX posts_user_id ON posts (user_id) USING BTREE",
            &[],
        )
        .unwrap();
    for id in 1..=100 {
        engine
            .execute(
                "INSERT INTO kv (id, body) VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(format!("row-{id}").into())],
            )
            .unwrap();
    }
    for id in 1..=3 {
        engine
            .execute(
                "INSERT INTO users VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(format!("user-{id}").into())],
            )
            .unwrap();
    }
    for id in 1..=9 {
        engine
            .execute(
                "INSERT INTO posts VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Integer(1 + (id - 1) % 3),
                    Value::Text(format!("post-{id}").into()),
                ],
            )
            .unwrap();
    }

    let cases: [(&str, usize, &[usize]); 7] = [
        // The scan reads exactly the limit.
        ("SELECT body FROM kv LIMIT 3", 3, &[3]),
        // ... plus the offset it has to skip past.
        ("SELECT body FROM kv LIMIT 3 OFFSET 2", 3, &[5]),
        // A limit past the ceiling is clamped to one full batch, which on a
        // hundred-row table is also the last.
        ("SELECT body FROM kv LIMIT 1000", 100, &[512]),
        // A filter makes the rows needed unknown: the default batch stands,
        // and it grows as it always did.
        ("SELECT body FROM kv WHERE body <> '' LIMIT 3", 3, &[32]),
        (
            "SELECT body FROM kv WHERE body = 'row-90' LIMIT 3",
            1,
            &[32, 64, 128],
        ),
        // Both `LIMIT 10` join shapes `bin/profile --suite joins-limit`
        // times: the driving side is a scan, the inner side a probe, and no
        // `WHERE` stands between the scan and the limit.
        (
            "SELECT posts.id, users.name FROM posts JOIN users \
             ON posts.user_id = users.id LIMIT 3",
            3,
            &[3],
        ),
        (
            "SELECT users.name, posts.title FROM users JOIN posts \
             ON posts.user_id = users.id LIMIT 3",
            3,
            &[3],
        ),
    ];
    for (sql, rows, expected) in cases {
        let statement = engine.prepare(sql).unwrap();
        batch_sizes.borrow_mut().clear();
        let answer = engine.run_query(&statement, &[]).unwrap();
        assert_eq!(answer.rows.len(), rows, "{sql}");
        assert_eq!(
            batch_sizes.borrow().as_slice(),
            expected,
            "{sql}: batches asked for"
        );
    }
}

#[test]
fn a_prepared_insert_writes_a_different_row_each_time() {
    let (mut engine, _) = counting_engine();
    engine
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    let before = engine.statements_parsed();

    let insert = engine
        .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
        .unwrap();
    for id in 1..=5 {
        engine
            .run(
                &insert,
                &[Value::Integer(id), Value::Text(format!("row-{id}").into())],
            )
            .unwrap();
    }
    assert_eq!(engine.statements_parsed() - before, 1);

    let rows = engine.query("SELECT id, body FROM kv", &[]).unwrap();
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(
        rows.rows[4],
        vec![Value::Integer(5), Value::Text("row-5".into())]
    );
}

#[test]
fn a_prepared_update_and_delete_rebind_their_filters() {
    let (mut engine, _) = seeded(3);

    let update = engine
        .prepare("UPDATE kv SET body = ? WHERE id = ?")
        .unwrap();
    engine
        .run(&update, &[Value::Text("changed".into()), Value::Integer(2)])
        .unwrap();
    engine
        .run(&update, &[Value::Text("also".into()), Value::Integer(3)])
        .unwrap();

    let delete = engine.prepare("DELETE FROM kv WHERE id = ?").unwrap();
    engine.run(&delete, &[Value::Integer(1)]).unwrap();

    let rows = engine.query("SELECT id, body FROM kv", &[]).unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(2), Value::Text("changed".into())],
            vec![Value::Integer(3), Value::Text("also".into())],
        ]
    );
}

#[test]
fn a_prepared_retrieval_query_rebinds_its_embedding_and_its_terms() {
    let (mut engine, _) = counting_engine();
    engine
        .execute(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(3))",
            &[],
        )
        .unwrap();
    engine
        .execute("CREATE INDEX docs_body ON docs (body)", &[])
        .unwrap();
    engine
        .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .unwrap();
    for (id, body, embedding) in [
        (1, "rust database engine", vec![1.0, 0.0, 0.0]),
        (2, "python web framework", vec![0.0, 1.0, 0.0]),
    ] {
        engine
            .execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Text(body.to_string().into()),
                    Value::Vector(embedding),
                ],
            )
            .unwrap();
    }

    let before = engine.statements_parsed();
    let search = engine
        .prepare(
            "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
             FROM docs LIMIT 1",
        )
        .unwrap();

    let rust = engine
        .run_query(
            &search,
            &[
                Value::Vector(vec![1.0, 0.0, 0.0]),
                Value::Text("rust".into()),
            ],
        )
        .unwrap();
    assert_eq!(rust.rows[0][0], Value::Integer(1));

    let python = engine
        .run_query(
            &search,
            &[
                Value::Vector(vec![0.0, 1.0, 0.0]),
                Value::Text("python".into()),
            ],
        )
        .unwrap();
    assert_eq!(python.rows[0][0], Value::Integer(2));

    assert_eq!(engine.statements_parsed() - before, 1);
}

#[test]
fn a_bound_embedding_of_the_wrong_width_is_rejected_at_execution() {
    let (mut engine, _) = counting_engine();
    engine
        .execute("CREATE TABLE docs (id INTEGER, embedding VECTOR(3))", &[])
        .unwrap();
    engine
        .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .unwrap();
    let search = engine
        .prepare("SELECT id, vector_score(embedding, ?) FROM docs")
        .unwrap();
    let error = engine
        .run_query(&search, &[Value::Vector(vec![1.0, 0.0])])
        .unwrap_err();
    assert!(matches!(error, Error::Type(_)), "got {error}");
}

#[test]
fn a_bound_value_of_the_wrong_type_is_rejected_at_execution() {
    let (mut engine, _) = counting_engine();
    engine
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    let insert = engine
        .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
        .unwrap();
    let error = engine
        .run(&insert, &[Value::Integer(1), Value::Integer(2)])
        .unwrap_err();
    assert!(matches!(error, Error::Type(_)), "got {error}");
}

#[test]
fn the_wrong_number_of_parameters_is_a_bind_error() {
    let (mut engine, _) = seeded(1);
    let lookup = engine.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
    assert_eq!(lookup.parameter_count(), 1);

    for params in [vec![], vec![Value::Integer(1), Value::Integer(2)]] {
        let error = engine.run(&lookup, &params).unwrap_err();
        assert!(matches!(error, Error::Bind(_)), "got {error}");
    }
}

#[test]
fn creating_an_unrelated_table_does_not_invalidate_a_statement() {
    let (mut engine, _) = seeded(1);
    let lookup = engine.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
    engine
        .execute("CREATE TABLE other (a INTEGER)", &[])
        .unwrap();
    assert!(engine.run(&lookup, &[Value::Integer(1)]).is_ok());
}

#[test]
fn a_statement_prepared_against_another_schema_refuses_to_run() {
    // The failure this guards: a plan projects column *2*. Run it against a
    // table whose column 2 is something else and it answers with the wrong
    // value and no error anywhere. Two engines is the reachable way to build
    // that situation today, and the check is the same one that will catch
    // `ALTER TABLE` when there is one.
    let (mut first, _) = counting_engine();
    first
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    let lookup = first.prepare("SELECT body FROM kv WHERE id = ?").unwrap();

    let (mut second, _) = counting_engine();
    second
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, title TEXT)", &[])
        .unwrap();
    second
        .execute(
            "INSERT INTO kv (id, title) VALUES (?, ?)",
            &[Value::Integer(1), Value::Text("secret".into())],
        )
        .unwrap();

    let error = second.run(&lookup, &[Value::Integer(1)]).unwrap_err();
    assert!(matches!(error, Error::Stale(_)), "got {error}");
}

#[test]
fn a_statement_whose_table_is_gone_refuses_to_run() {
    let (mut first, _) = counting_engine();
    first
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    let lookup = first.prepare("SELECT body FROM kv WHERE id = ?").unwrap();

    let (mut empty, _) = counting_engine();
    let error = empty.run(&lookup, &[Value::Integer(1)]).unwrap_err();
    assert!(matches!(error, Error::Stale(_)), "got {error}");
}

#[test]
fn preparing_a_statement_never_touches_the_data() {
    let (mut engine, scans) = seeded(3);
    let before = engine.query("SELECT id FROM kv", &[]).unwrap();
    scans.set(0);

    let statements = [
        "SELECT body FROM kv WHERE id = ?",
        "INSERT INTO kv (id, body) VALUES (?, ?)",
        "DELETE FROM kv WHERE id = ?",
        "UPDATE kv SET body = ? WHERE id = ?",
    ];
    for sql in statements {
        engine.prepare(sql).unwrap();
    }

    assert_eq!(scans.get(), 0, "preparing read rows");
    let after = engine.query("SELECT id FROM kv", &[]).unwrap();
    assert_eq!(before, after, "preparing changed the table");
}
