//! `INTEGER PRIMARY KEY` behaves as SQLite's row-id alias, and the planner
//! turns an equality on it into a point lookup.
//!
//! The point-lookup assertions do not measure time — a timing test would be
//! flaky and would prove nothing about small tables anyway. They count calls to
//! [`Storage::scan_batch`] through a wrapper, which is the actual claim: the
//! engine never asks for the whole table. These tables are small enough that a
//! scan is one batch, so a count is still a count of scans.

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

/// An engine over counting storage, plus the counter.
fn engine() -> (Engine, Rc<Cell<usize>>) {
    let scans = Rc::new(Cell::new(0));
    let storage = CountingStorage {
        inner: MemStorage::new(),
        scans: scans.clone(),
    };
    let engine = Engine::open(
        Box::new(storage),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .expect("open");
    (engine, scans)
}

/// Three rows with explicit keys, on a table whose `id` aliases the row id.
fn seeded() -> (Engine, Rc<Cell<usize>>) {
    let (mut engine, scans) = engine();
    engine
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    for (id, body) in [(10, "ten"), (20, "twenty"), (30, "thirty")] {
        engine
            .execute(
                "INSERT INTO t (id, body) VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(body.to_string().into())],
            )
            .unwrap();
    }
    scans.set(0);
    (engine, scans)
}

#[test]
fn a_lookup_by_primary_key_does_not_scan_the_table() {
    let (mut engine, scans) = seeded();
    let rows = engine
        .query("SELECT body FROM t WHERE id = 20", &[])
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Text("twenty".into())]]);
    assert_eq!(scans.get(), 0, "the engine scanned instead of seeking");
}

#[test]
fn a_lookup_by_an_ordinary_column_still_scans() {
    let (mut engine, scans) = seeded();
    let rows = engine
        .query("SELECT id FROM t WHERE body = 'twenty'", &[])
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Integer(20)]]);
    assert_eq!(scans.get(), 1);
}

#[test]
fn an_and_conjunction_still_seeks_and_still_filters() {
    let (mut engine, scans) = seeded();

    // The equality pins the row; the other conjunct is applied to it.
    let hit = engine
        .query("SELECT body FROM t WHERE id = 30 AND body = 'thirty'", &[])
        .unwrap();
    assert_eq!(hit.rows, vec![vec![Value::Text("thirty".into())]]);

    let miss = engine
        .query("SELECT body FROM t WHERE id = 30 AND body = 'nope'", &[])
        .unwrap();
    assert!(miss.rows.is_empty(), "the second conjunct was not applied");
    assert_eq!(scans.get(), 0);
}

#[test]
fn an_or_disjunction_must_scan_because_another_row_could_match() {
    let (mut engine, scans) = seeded();
    let rows = engine
        .query("SELECT id FROM t WHERE id = 10 OR body = 'thirty'", &[])
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(10)], vec![Value::Integer(30)]]
    );
    assert_eq!(scans.get(), 1);
}

#[test]
fn update_and_delete_by_primary_key_also_seek() {
    let (mut engine, scans) = seeded();
    engine
        .execute("UPDATE t SET body = 'TWENTY' WHERE id = 20", &[])
        .unwrap();
    engine.execute("DELETE FROM t WHERE id = 10", &[]).unwrap();
    assert_eq!(scans.get(), 0, "a keyed write scanned the table");

    let rows = engine.query("SELECT id, body FROM t", &[]).unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(20), Value::Text("TWENTY".into())],
            vec![Value::Integer(30), Value::Text("thirty".into())],
        ]
    );
}

#[test]
fn the_inserted_key_is_the_key_the_row_is_stored_under() {
    let (mut engine, _) = seeded();
    // Row ids drive scan order, so an explicit key out of insertion order must
    // still come back in key order.
    engine
        .execute(
            "INSERT INTO t (id, body) VALUES (?, ?)",
            &[Value::Integer(1), Value::Text("one".into())],
        )
        .unwrap();
    let rows = engine.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(10)],
            vec![Value::Integer(20)],
            vec![Value::Integer(30)],
        ]
    );
}

#[test]
fn a_duplicate_primary_key_is_rejected() {
    let (mut engine, _) = seeded();
    let error = engine
        .execute(
            "INSERT INTO t (id, body) VALUES (?, ?)",
            &[Value::Integer(20), Value::Text("again".into())],
        )
        .unwrap_err();
    assert!(
        matches!(&error, Error::Constraint(message) if message.contains("t.id")),
        "expected a constraint error, got {error:?}"
    );
}

#[test]
fn an_omitted_key_is_assigned_and_never_collides() {
    let (mut engine, _) = seeded();
    engine
        .execute(
            "INSERT INTO t (body) VALUES (?)",
            &[Value::Text("assigned".into())],
        )
        .unwrap();

    let rows = engine
        .query("SELECT id FROM t WHERE body = 'assigned'", &[])
        .unwrap();
    let Value::Integer(assigned) = rows.rows[0][0] else {
        panic!("expected an integer key, got {:?}", rows.rows[0][0]);
    };
    assert!(
        assigned > 30,
        "assigned key {assigned} collides with an existing row"
    );

    // And the assigned key really is the row id: seeking by it works.
    let seeked = engine
        .query(&format!("SELECT body FROM t WHERE id = {assigned}"), &[])
        .unwrap();
    assert_eq!(seeked.rows, vec![vec![Value::Text("assigned".into())]]);
}

#[test]
fn a_negative_key_is_rejected_rather_than_silently_reordering_the_table() {
    let (mut engine, _) = seeded();
    let error = engine
        .execute(
            "INSERT INTO t (id, body) VALUES (?, ?)",
            &[Value::Integer(-1), Value::Text("negative".into())],
        )
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "got {error:?}");
}

/// A primary key on anything but a lone `INTEGER` column is a unique index in
/// SQLite, not the row id — so it does **not** become the storage key, but it
/// does get a unique B-tree index, which both enforces it and answers a lookup
/// on it without a scan.
#[test]
fn a_primary_key_on_a_non_integer_column_is_a_unique_index() {
    let (mut engine, scans) = engine();
    engine
        .execute("CREATE TABLE named (name TEXT PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    assert_eq!(engine.catalog().table("named").unwrap().rowid_alias(), None);
    engine
        .execute("INSERT INTO named VALUES ('a', 'first')", &[])
        .unwrap();

    // Enforced, which is why accepting it is honest.
    let error = engine
        .execute("INSERT INTO named VALUES ('a', 'second')", &[])
        .unwrap_err();
    assert!(matches!(error, Error::Constraint(_)), "got {error:?}");

    // And an access path: the B-tree index backing the constraint answers the
    // lookup, so no row is read that the filter then throws away.
    scans.set(0);
    let found = engine
        .query("SELECT body FROM named WHERE name = 'a'", &[])
        .unwrap();
    assert_eq!(found.rows.len(), 1);
    assert_eq!(
        scans.get(),
        0,
        "a TEXT primary key should be answered from its index, not a scan"
    );
}

#[test]
fn a_table_without_a_primary_key_is_unchanged() {
    let (mut engine, scans) = engine();
    engine
        .execute("CREATE TABLE plain (id INTEGER, body TEXT)", &[])
        .unwrap();
    engine
        .execute(
            "INSERT INTO plain (id, body) VALUES (?, ?)",
            &[Value::Integer(99), Value::Text("body".into())],
        )
        .unwrap();
    scans.set(0);

    // `id` is an ordinary column here, so 99 is data, not an address.
    let rows = engine
        .query("SELECT body FROM plain WHERE id = 99", &[])
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Text("body".into())]]);
    assert_eq!(scans.get(), 1);
}
