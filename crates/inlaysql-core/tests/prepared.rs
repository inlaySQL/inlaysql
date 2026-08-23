//! Prepared statements: parse once, bind many times, and refuse to run against
//! a schema the plan was not built for.
//!
//! The "parses once" claim is counted, not timed — `Engine::statements_parsed`
//! is incremented by the one function in the crate that calls the parser, so a
//! test can assert the exact number rather than hope a stopwatch agrees. The
//! point-lookup claim is counted too, through the same `Storage::scan` wrapper
//! `primary_key.rs` uses: a prepared `WHERE id = ?` has to seek, not scan.

use std::cell::Cell;
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::row::RowBuf;
use inlaysql_core::traits::{RowId, Storage};
use inlaysql_core::{Engine, Error, Result, Value};

/// `MemStorage` that counts how often the engine falls back to a full scan.
struct CountingStorage {
    inner: MemStorage,
    scans: Rc<Cell<usize>>,
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
    let scans = Rc::new(Cell::new(0));
    let engine = Engine::open(
        Box::new(CountingStorage {
            inner: MemStorage::new(),
            scans: scans.clone(),
        }),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .expect("open");
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
