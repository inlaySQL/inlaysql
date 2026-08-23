//! Scalar B-tree indexes: that they are maintained, that they are used, and —
//! the only one that really matters — that using one never changes an answer.
//!
//! The shape of almost every test here is the same, and it is deliberate: run
//! a query against a table with the index and against the identical table
//! without it, and assert the two agree. An index that returns *fewer* rows
//! than a scan is the failure mode worth hunting, and it is invisible to any
//! test that only looks at the indexed side.
//!
//! The storage double counts full scans and can read the raw index entries, so
//! "the index was used" and "the index describes exactly the rows that exist"
//! are both assertions rather than inferences.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::row::RowBuf;
use inlaysql_core::traits::{RowId, Storage};
use inlaysql_core::{Engine, Error, IndexKind, Result, Value};

/// `MemStorage` behind a handle the test keeps, so it can count scans and read
/// the index entries the engine wrote.
#[derive(Clone, Default)]
struct Probe {
    inner: Rc<RefCell<MemStorage>>,
    scans: Rc<Cell<usize>>,
    /// Point reads by row id — how many rows an index probe actually fetched.
    reads: Rc<Cell<usize>>,
    /// Which tables were scanned sequentially, in call order.
    ///
    /// A join always scans its *driving* table, so "the index was used" for a
    /// join's inner side is a question about one table rather than about the
    /// statement — which is what [`Probe::scans`] alone cannot answer.
    scanned: Rc<RefCell<Vec<String>>>,
}

impl Probe {
    /// Every index entry key in the database, in key order.
    fn entries(&self) -> Vec<Vec<u8>> {
        self.inner
            .borrow()
            .scan_index_range(&[1], Some(&[2]))
            .expect("scan entries")
    }

    /// How many sequential batches were read from one table.
    fn scans_of(&self, table: &str) -> usize {
        self.scanned
            .borrow()
            .iter()
            .filter(|name| name == &table)
            .count()
    }

    /// Forget every scan and point read recorded so far.
    fn reset(&self) {
        self.scans.set(0);
        self.reads.set(0);
        self.scanned.borrow_mut().clear();
    }
}

impl Storage for Probe {
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        self.inner.borrow_mut().put_row(table, id, bytes)
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        self.reads.set(self.reads.get() + 1);
        self.inner.borrow().get_row(table, id)
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        self.inner.borrow_mut().delete_row(table, id)
    }

    /// Counts every sequential read of a table, which is what "the index was
    /// used" is asserted by the *absence* of.
    ///
    /// Since AHL-462 a scan reaches storage as a run of bounded batches rather
    /// than one call, so this counts batches. Every assertion below is `== 0`
    /// or `> 0` — "was this table walked at all" — which batching does not
    /// change: an index probe issues no batch, and a scan issues at least one.
    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        self.scans.set(self.scans.get() + 1);
        self.scanned.borrow_mut().push(table.to_string());
        self.inner.borrow().scan_batch(table, after, limit)
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.inner.borrow_mut().put_meta(key, bytes)
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.borrow().get_meta(key)
    }

    fn put_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.inner.borrow_mut().put_index_entry(key)
    }

    fn delete_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.inner.borrow_mut().delete_index_entry(key)
    }

    fn scan_index_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        self.inner.borrow().scan_index_range(start, end)
    }

    fn commit(&mut self) -> Result<()> {
        self.inner.borrow_mut().commit()
    }

    fn rollback(&mut self) -> Result<()> {
        self.inner.borrow_mut().rollback()
    }
}

fn engine_on(probe: &Probe) -> Engine {
    Engine::open(
        Box::new(probe.clone()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .expect("open")
}

fn engine() -> (Engine, Probe) {
    let probe = Probe::default();
    let engine = engine_on(&probe);
    (engine, probe)
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

/// The rendered rows of a query, in the order the engine returned them.
fn rows(engine: &mut Engine, sql: &str, params: &[Value]) -> Vec<Vec<String>> {
    engine
        .query(sql, params)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect())
        .collect()
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => format!("i:{i}"),
        Value::Real(r) => format!("f:{r:?}"),
        Value::Text(t) => format!("t:{t}"),
        Value::Blob(b) => format!("b:{b:?}"),
        Value::Vector(v) => format!("v:{}", v.len()),
    }
}

/// The DDL and rows every "with and without the index" test shares.
const SETUP: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, r REAL, s TEXT, b BLOB)",
    "INSERT INTO t VALUES (1, 10, 1.5, 'apple', x'00')",
    "INSERT INTO t VALUES (2, 20, -0.0, 'banana', x'0001')",
    "INSERT INTO t VALUES (3, 20, 2.5, 'apple pie', x'01')",
    "INSERT INTO t VALUES (4, NULL, NULL, NULL, NULL)",
    "INSERT INTO t VALUES (5, -7, 1e308, '', x'')",
    "INSERT INTO t VALUES (6, 0, 0.0, 'Apple', x'ff')",
    "INSERT INTO t VALUES (7, 20, 1.5, 'apple', x'00')",
];

/// The indexes the "with" side of every comparison declares.
const INDEXES: &[&str] = &[
    "CREATE INDEX t_n ON t (n)",
    "CREATE INDEX t_r ON t (r)",
    "CREATE INDEX t_s ON t (s) USING BTREE",
    "CREATE INDEX t_b ON t (b)",
    "CREATE INDEX t_ns ON t (n, s)",
];

/// Build the same data twice — once indexed, once not — and assert every query
/// in `queries` returns the same rows from both, in the same order.
///
/// This is the test this whole file exists for. Everything else is a detail of
/// how the index is built; this is whether it tells the truth.
fn same_with_and_without_index(queries: &[&str]) {
    let (mut plain, plain_probe) = engine();
    let (mut indexed, indexed_probe) = engine();
    for sql in SETUP {
        run(&mut plain, sql);
        run(&mut indexed, sql);
    }
    for sql in INDEXES {
        run(&mut indexed, sql);
    }

    for sql in queries {
        let expected = rows(&mut plain, sql, &[]);
        let actual = rows(&mut indexed, sql, &[]);
        assert_eq!(
            actual, expected,
            "`{sql}` disagreed with the unindexed table"
        );
    }
    assert!(
        !indexed_probe.entries().is_empty(),
        "the indexed side built no entries, so this compared nothing"
    );
    assert!(plain_probe.entries().is_empty());
}

/// One entry per row per index, always — `NULL`s included. It is the invariant
/// that makes "the index describes exactly the rows that exist" checkable
/// without decoding a single key.
fn assert_entry_count(probe: &Probe, rows: usize, indexes: usize) {
    assert_eq!(
        probe.entries().len(),
        rows * indexes,
        "expected {rows} rows x {indexes} indexes of entries"
    );
}

// ---------------------------------------------------------------- maintenance

#[test]
fn an_index_built_over_existing_rows_describes_them() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    assert_entry_count(&probe, 0, 0);
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    assert_entry_count(&probe, 7, 1);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 20", &[]),
        vec![vec!["i:2"], vec!["i:3"], vec!["i:7"]]
    );
}

#[test]
fn insert_update_and_delete_keep_the_entries_in_step() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(&mut engine, "CREATE INDEX t_n ON t (n)");

    for id in 1..=5 {
        run(
            &mut engine,
            &format!("INSERT INTO t VALUES ({id}, {})", id * 10),
        );
    }
    assert_entry_count(&probe, 5, 1);

    // An UPDATE must remove the old entry as well as write the new one, or the
    // index would answer `n = 30` with a row whose `n` is 99.
    run(&mut engine, "UPDATE t SET n = 99 WHERE id = 3");
    assert_entry_count(&probe, 5, 1);
    assert!(rows(&mut engine, "SELECT id FROM t WHERE n = 30", &[]).is_empty());
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 99", &[]),
        vec![vec!["i:3"]]
    );

    run(&mut engine, "DELETE FROM t WHERE id = 3");
    assert_entry_count(&probe, 4, 1);
    assert!(rows(&mut engine, "SELECT id FROM t WHERE n = 99", &[]).is_empty());

    run(&mut engine, "DELETE FROM t");
    assert_entry_count(&probe, 0, 1);
}

/// Updating the indexed column *through* the indexed column: the candidates
/// come from the index, and then every one of them moves inside it.
///
/// This is the shape that goes wrong when candidates are streamed rather than
/// materialised — a row whose new value lands ahead of the cursor is visited
/// twice, and `n = n + 1` runs away. The executor materialises, so it does
/// not; asserting it here is what would notice if that ever changed.
#[test]
fn updating_the_indexed_column_by_the_indexed_column_visits_each_row_once() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    for id in 1..=5i64 {
        run(&mut engine, &format!("INSERT INTO t VALUES ({id}, {id})"));
    }

    run(&mut engine, "UPDATE t SET n = n + 10 WHERE n >= 3");
    assert_entry_count(&probe, 5, 1);
    assert_eq!(
        rows(&mut engine, "SELECT id, n FROM t ORDER BY id", &[]),
        vec![
            vec!["i:1", "i:1"],
            vec!["i:2", "i:2"],
            vec!["i:3", "i:13"],
            vec!["i:4", "i:14"],
            vec!["i:5", "i:15"],
        ]
    );
    // And the index agrees with what the rows now say.
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 13", &[]),
        vec![vec!["i:3"]]
    );
    assert!(rows(&mut engine, "SELECT id FROM t WHERE n = 3", &[]).is_empty());
}

/// The row id is part of an entry's key, so a row that *moves* — an `UPDATE`
/// of the `INTEGER PRIMARY KEY` — has to take its entries with it.
#[test]
fn a_row_that_changes_its_primary_key_moves_its_entries() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    run(&mut engine, "INSERT INTO t VALUES (1, 5)");
    run(&mut engine, "UPDATE t SET id = 9 WHERE id = 1");
    assert_entry_count(&probe, 1, 1);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 5", &[]),
        vec![vec!["i:9"]]
    );
}

#[test]
fn dropping_an_index_removes_its_entries_and_leaves_the_others() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    run(&mut engine, "CREATE INDEX t_s ON t (s) USING BTREE");
    assert_entry_count(&probe, 7, 2);

    run(&mut engine, "DROP INDEX t_n");
    assert_entry_count(&probe, 7, 1);

    // And a *new* index of the same name must not inherit the old entries: it
    // would describe rows as they were, which is the worst kind of stale.
    run(&mut engine, "DELETE FROM t WHERE id = 1");
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    assert_entry_count(&probe, 6, 2);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 10", &[]),
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn dropping_a_table_takes_its_entries_with_it() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    assert_entry_count(&probe, 7, 1);
    run(&mut engine, "DROP TABLE t");
    assert_entry_count(&probe, 0, 0);
}

/// `ALTER TABLE` never changes an indexed value — the column an index names
/// cannot be dropped, a rename leaves values alone, and a new column is
/// appended — so the entries survive it untouched and still answer.
#[test]
fn alter_table_leaves_the_entries_true() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    run(
        &mut engine,
        "ALTER TABLE t ADD COLUMN extra TEXT DEFAULT 'x'",
    );
    assert_entry_count(&probe, 7, 1);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 20", &[]),
        vec![vec!["i:2"], vec!["i:3"], vec!["i:7"]]
    );

    run(&mut engine, "ALTER TABLE t RENAME COLUMN n TO amount");
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE amount = 20", &[]),
        vec![vec!["i:2"], vec!["i:3"], vec!["i:7"]]
    );

    // A rename of the *table* must not orphan the entries either.
    run(&mut engine, "ALTER TABLE t RENAME TO u");
    assert_entry_count(&probe, 7, 1);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM u WHERE amount = 20", &[]),
        vec![vec!["i:2"], vec!["i:3"], vec!["i:7"]]
    );

    // And the indexed column cannot be dropped out from under the index.
    let err = refuse(&mut engine, "ALTER TABLE u DROP COLUMN amount");
    assert!(err.to_string().contains("indexed"), "got {err}");
}

#[test]
fn a_rolled_back_transaction_leaves_no_entries_behind() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    run(&mut engine, "INSERT INTO t VALUES (1, 1)");

    engine.begin().expect("begin");
    run(&mut engine, "INSERT INTO t VALUES (2, 2)");
    run(&mut engine, "DELETE FROM t WHERE id = 1");
    engine.rollback().expect("rollback");

    assert_entry_count(&probe, 1, 1);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 1", &[]),
        vec![vec!["i:1"]]
    );
    assert!(rows(&mut engine, "SELECT id FROM t WHERE n = 2", &[]).is_empty());
}

/// The entries an open transaction wrote have to be visible to that
/// transaction, or a statement that inserts a row and then reads it back
/// through an index would not find it.
#[test]
fn a_transaction_sees_its_own_entries() {
    let (mut engine, _) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    engine.begin().expect("begin");
    run(&mut engine, "INSERT INTO t VALUES (1, 42)");
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 42", &[]),
        vec![vec!["i:1"]]
    );
    engine.commit().expect("commit");
}

#[test]
fn the_entries_survive_reopening_the_database() {
    let probe = Probe::default();
    {
        let mut engine = engine_on(&probe);
        for sql in SETUP {
            run(&mut engine, sql);
        }
        run(&mut engine, "CREATE INDEX t_n ON t (n)");
    }
    let before = probe.entries();
    let mut reopened = engine_on(&probe);
    assert_eq!(
        probe.entries(),
        before,
        "reopening rewrote the entries; a B-tree index needs no rebuild"
    );
    assert_eq!(
        rows(&mut reopened, "SELECT id FROM t WHERE n = 20", &[]),
        vec![vec!["i:2"], vec!["i:3"], vec!["i:7"]]
    );
    assert!(reopened
        .catalog()
        .indexes_for("t")
        .iter()
        .any(|index| index.name == "t_n" && index.kind == IndexKind::BTree));
}

// -------------------------------------------------------------- planner rule

#[test]
fn an_equality_on_an_indexed_column_does_not_scan() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX t_n ON t (n)");

    probe.scans.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 20", &[]),
        vec![vec!["i:2"], vec!["i:3"], vec!["i:7"]]
    );
    assert_eq!(probe.scans.get(), 0, "an indexed equality still scanned");

    // A bound parameter is the case that matters for a prepared statement: the
    // probe is built at execution, so one plan serves every binding.
    let statement = engine
        .prepare("SELECT id FROM t WHERE n = ?")
        .expect("prepare");
    probe.scans.set(0);
    let found = engine
        .run_query(&statement, &[Value::Integer(10)])
        .expect("run");
    assert_eq!(found.rows.len(), 1);
    assert_eq!(probe.scans.get(), 0, "a parameterised probe still scanned");

    // A column with no index still scans, which is what makes the assertion
    // above mean anything.
    probe.scans.set(0);
    rows(&mut engine, "SELECT id FROM t WHERE s = 'apple'", &[]);
    assert!(probe.scans.get() > 0);
}

#[test]
fn a_range_and_a_between_on_an_indexed_column_do_not_scan() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX t_n ON t (n)");

    for sql in [
        "SELECT id FROM t WHERE n > 0",
        "SELECT id FROM t WHERE n >= 10 AND n < 21",
        "SELECT id FROM t WHERE n BETWEEN 10 AND 20",
        "SELECT id FROM t WHERE n <= 0",
        "SELECT id FROM t WHERE 20 = n",
    ] {
        probe.scans.set(0);
        rows(&mut engine, sql, &[]);
        assert_eq!(probe.scans.get(), 0, "`{sql}` still scanned");
    }
}

/// An `OR` is not a conjunction, and one side of it cannot narrow the other.
/// Answering it from an index range would silently drop the rows the other
/// side matches.
#[test]
fn a_disjunction_is_not_answered_from_an_index() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    probe.scans.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 10 OR id = 4", &[]),
        vec![vec!["i:1"], vec!["i:4"]]
    );
    assert!(probe.scans.get() > 0, "an OR was answered from an index");
}

/// A multi-column index is probed on its leading columns, and only its leading
/// columns: an equality on the *second* column alone leaves entries scattered
/// through the whole index, so it is not a range.
#[test]
fn a_composite_index_is_probed_on_its_leading_columns() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX t_ns ON t (n, s)");

    probe.scans.set(0);
    assert_eq!(
        rows(
            &mut engine,
            "SELECT id FROM t WHERE n = 20 AND s = 'apple'",
            &[]
        ),
        vec![vec!["i:7"]]
    );
    assert_eq!(probe.scans.get(), 0);

    // Leading column alone: still a range, just a wider one.
    probe.scans.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 20", &[]),
        vec![vec!["i:2"], vec!["i:3"], vec!["i:7"]]
    );
    assert_eq!(probe.scans.get(), 0);

    // Second column alone: a scan, and the right answer.
    probe.scans.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE s = 'apple'", &[]),
        vec![vec!["i:1"], vec!["i:7"]]
    );
    assert!(probe.scans.get() > 0);
}

/// An index probe is a *stage of the streaming pipeline*, not a materialised
/// list of rows in front of it (AHL-423 meeting AHL-462).
///
/// The probe reads its range of index entries — cheap: they are keys with no
/// value, and the whole range has to be read anyway to sort it back into row-id
/// order — and then hands those ids downstream one at a time. So a `LIMIT`
/// stops fetching *rows* as soon as it has enough, exactly as it does over a
/// sequential scan. An indexed path that materialised its rows first would read
/// all 200 here and throw 197 away, which is gap G5 in a narrower place.
#[test]
fn an_indexed_limit_fetches_only_the_rows_it_returns() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, body TEXT)",
    );
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    engine.begin().expect("begin");
    for id in 1..=200i64 {
        run(
            &mut engine,
            &format!("INSERT INTO t VALUES ({id}, 1, 'row-{id}')"),
        );
    }
    engine.commit().expect("commit");

    // The whole range: 200 entries, 200 rows fetched.
    probe.scans.set(0);
    probe.reads.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 1", &[]).len(),
        200
    );
    assert_eq!(probe.scans.get(), 0, "the indexed equality scanned");
    assert_eq!(probe.reads.get(), 200);

    // The same probe under a `LIMIT`: the same 200 entries, three rows.
    probe.scans.set(0);
    probe.reads.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE n = 1 LIMIT 3", &[]),
        vec![vec!["i:1"], vec!["i:2"], vec!["i:3"]]
    );
    assert_eq!(probe.scans.get(), 0, "the indexed LIMIT scanned");
    assert_eq!(
        probe.reads.get(),
        3,
        "an indexed LIMIT 3 fetched {} rows",
        probe.reads.get()
    );

    // And `ORDER BY` — which disables the pushdown, because a sort chooses
    // *which* rows survive — still reads the range and still answers.
    probe.reads.set(0);
    assert_eq!(
        rows(
            &mut engine,
            "SELECT id FROM t WHERE n = 1 ORDER BY id DESC LIMIT 2",
            &[]
        ),
        vec![vec!["i:200"], vec!["i:199"]]
    );
    assert_eq!(probe.reads.get(), 200);
}

/// A `NUMERIC` column holds every storage class at once, and no ordered index
/// may answer for it (`engine.rs::join_probe`'s rule) — an index over it is
/// maintained and never chosen, whatever the comparison's answer turns out to
/// be. Before AHL-477, this engine's comparison *errored* on a cross-class
/// compare instead of answering by SQLite's fixed class order; the error
/// happened to make the "never chosen" claim easy to see (it survived to the
/// caller unfiltered), but the fix has to keep holding once the comparison
/// answers correctly instead — a probe that narrowed to the number's range
/// and skipped the string would silently miss nothing here since strings
/// rank above every number and `v = 5` cannot match one, but the column
/// still must not be an access path, because `v` might one day be compared
/// against a `TEXT` or `BLOB` value that *would* be lost that way.
#[test]
fn a_numeric_column_keeps_its_index_and_compares_by_class() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v DATETIME)",
    );
    run(&mut engine, "CREATE INDEX t_v ON t (v)");
    run(&mut engine, "INSERT INTO t VALUES (1, 5), (2, 7)");
    // Maintained: one entry per row.
    assert_entry_count(&probe, 2, 1);

    probe.scans.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE v = 5", &[]),
        vec![vec!["i:1"]]
    );
    assert!(
        probe.scans.get() > 0,
        "a NUMERIC column must not be an access path"
    );

    // A `TEXT` row added to the same `NUMERIC` column: SQLite's class order
    // ranks every `TEXT` value above every number, so `v = 5` still cannot
    // match it — confirmed against sqlite3 — and still must not error the
    // way it used to. The scan is still the access path (a probe range still
    // may not narrow this column), and it now answers instead of failing.
    run(&mut engine, "INSERT INTO t VALUES (3, 'text')");
    assert_entry_count(&probe, 3, 1);
    probe.scans.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE v = 5", &[]),
        vec![vec!["i:1"]]
    );
    assert!(
        probe.scans.get() > 0,
        "a NUMERIC column must not become an access path just because a \
         cross-class compare no longer errors"
    );
}

/// A probe of a class the column cannot hold falls back to the scan, and the
/// scan now answers by SQLite's class order instead of erroring (AHL-477):
/// the index is still not the access path, but the query no longer fails.
#[test]
fn a_mismatched_probe_falls_back_to_the_scan_that_now_answers() {
    let (mut engine, probe) = engine();
    for sql in SETUP {
        run(&mut engine, sql);
    }
    for sql in INDEXES {
        run(&mut engine, sql);
    }
    // `s` is `TEXT`; `1` is `INTEGER`. Every number ranks below every `TEXT`
    // value, so `s = 1` cannot match any row — confirmed against sqlite3 —
    // without the index ever being consulted for it.
    probe.reset();
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE s = 1", &[]),
        Vec::<Vec<String>>::new()
    );
    assert!(
        probe.scans.get() > 0,
        "a TEXT column must not be probed for an INTEGER key"
    );
}

/// A `NUMERIC` column that mixes every storage class *can* be indexed — the
/// index is maintained the same as any other (AHL-477 changed no storage
/// format, only how values compare) — it is simply never an access path for
/// it, so an indexed engine and a plain one have to agree on the cross-class
/// `ORDER BY`, `WHERE` and `DISTINCT` answers a scan alone produces. This is
/// [`same_with_and_without_index`]'s own shape, kept as its own test rather
/// than folded into `SETUP`/`INDEXES` because every other test in this file
/// shares that fixture and a `NUMERIC` column would change what every one of
/// them is exercising.
#[test]
fn a_mixed_class_column_can_be_indexed_and_still_agrees_unindexed() {
    let (mut plain, plain_probe) = engine();
    let (mut indexed, indexed_probe) = engine();
    let setup = [
        "CREATE TABLE m (id INTEGER PRIMARY KEY, v NUMERIC)",
        "INSERT INTO m VALUES (1, NULL)",
        "INSERT INTO m VALUES (2, 2)",
        "INSERT INTO m VALUES (3, 1)",
        "INSERT INTO m VALUES (4, 'xyz')",
        "INSERT INTO m VALUES (5, X'0304')",
        "INSERT INTO m VALUES (6, 1.5)",
        "INSERT INTO m VALUES (7, 2)",
    ];
    for sql in setup {
        run(&mut plain, sql);
        run(&mut indexed, sql);
    }
    run(&mut indexed, "CREATE INDEX m_v ON m (v)");
    assert_entry_count(&indexed_probe, 7, 1);

    // The invariant is agreement, not a hard-coded expectation — the same
    // discipline every other `same_with_and_without_index` call in this file
    // uses — because the cross-class shapes these particular queries produce
    // are already pinned against a real sqlite3 3.54 binary elsewhere
    // (`order_by.test`, `expr.test`, `distinct.test`); what only this file
    // can check is that adding an index to a `NUMERIC` column changes
    // nothing about the answer.
    let queries = [
        "SELECT id FROM m ORDER BY v",
        "SELECT id FROM m ORDER BY v DESC",
        "SELECT id FROM m WHERE v > 1 ORDER BY id",
        "SELECT id FROM m WHERE v < 'xyz' ORDER BY id",
        "SELECT DISTINCT id FROM (SELECT id FROM m WHERE v = 2) ORDER BY id",
    ];
    indexed_probe.reset();
    for sql in queries {
        let expected = rows(&mut plain, sql, &[]);
        let actual = rows(&mut indexed, sql, &[]);
        assert_eq!(
            actual, expected,
            "`{sql}` disagreed with the unindexed table"
        );
    }
    assert!(
        indexed_probe.scans_of("m") > 0,
        "a NUMERIC column must not become an access path just because it has an index"
    );
    assert!(plain_probe.entries().is_empty());
}

// ------------------------------------------------------- index vs. no index

#[test]
fn equalities_agree_with_the_unindexed_table() {
    same_with_and_without_index(&[
        "SELECT id FROM t WHERE n = 20",
        "SELECT id FROM t WHERE n = 0",
        "SELECT id FROM t WHERE n = -7",
        "SELECT id FROM t WHERE n = 999",
        // The integer 20 and the real 20.0 are one key, and they must find the
        // same rows.
        "SELECT id FROM t WHERE n = 20.0",
        "SELECT id FROM t WHERE r = 1.5",
        // -0.0 is stored in row 2; `r = 0.0` has to find it.
        "SELECT id FROM t WHERE r = 0.0",
        "SELECT id FROM t WHERE r = -0.0",
        "SELECT id FROM t WHERE r = 0",
        "SELECT id FROM t WHERE s = 'apple'",
        "SELECT id FROM t WHERE s = ''",
        "SELECT id FROM t WHERE s = 'Apple'",
        "SELECT id FROM t WHERE b = x'00'",
        "SELECT id FROM t WHERE b = x''",
        // A NULL never equals anything, index or not.
        "SELECT id FROM t WHERE n = NULL",
        "SELECT id FROM t WHERE n IS NULL",
        "SELECT id FROM t WHERE n IS NOT NULL",
    ]);
}

/// AHL-486: `WHERE id = '1'` against an `INTEGER` column now matches, because
/// the engine applies SQLite's comparison affinity before ranking by storage
/// class — checked directly against a real sqlite3 3.54 binary. The index
/// path has to agree, and does so the same way
/// [`a_mismatched_probe_falls_back_to_the_scan_that_now_answers`] shows for a
/// pair affinity does *not* convert: `indexable_probe` still refuses a probe
/// whose value is not already the column's own storage class, so a term
/// affinity would convert never reaches an index at all — it falls back to
/// the full scan, and the scan (through `eval::comparison`) is what answers
/// correctly. `same_with_and_without_index` proves the two engines agree;
/// the explicit scan-count assertions below prove *why* — the index was
/// never consulted for the term that needed converting, not that it
/// happened to answer the same by coincidence.
#[test]
fn affinity_converted_equalities_agree_with_the_unindexed_table_and_bypass_the_index() {
    same_with_and_without_index(&[
        // The exact repro: `id` is the rowid alias (`INTEGER`), `'1'` is
        // `TEXT` with no affinity of its own.
        "SELECT id FROM t WHERE id = '1'",
        "SELECT id FROM t WHERE id = ' 1 '",
        "SELECT id FROM t WHERE id = '1.0'",
        "SELECT id FROM t WHERE id = '1e0'",
        // Not well-formed: stays TEXT, so this is still a genuine cross-class
        // miss rather than a conversion, and must still not error.
        "SELECT id FROM t WHERE id = '1x'",
        "SELECT id FROM t WHERE id = 'abc'",
        // `n` is `INTEGER` and indexed (`t_n`); `20`/`10` written as `TEXT`.
        "SELECT id FROM t WHERE n = '20'",
        "SELECT id FROM t WHERE n = '10'",
        "SELECT id FROM t WHERE n < '20'",
        "SELECT id FROM t WHERE n IN ('20', '10')",
        "SELECT id FROM t WHERE id BETWEEN '1' AND '3'",
        // `r` is `REAL` and indexed (`t_r`).
        "SELECT id FROM t WHERE r = '1.5'",
        // `s` is `TEXT` and indexed (`t_s`): a numeric literal renders as
        // text, not the reverse, so this stays a miss (no stored `s` is
        // `'20'`) but for the right reason now — checked in
        // `a_mismatched_probe_falls_back_to_the_scan_that_now_answers`'s
        // style below.
        "SELECT id FROM t WHERE s = 20",
    ]);

    // The row counts a plain scan would have missed before this fix, so a
    // regression cannot pass by both `same_with_and_without_index` engines
    // being wrong the same way.
    let (mut plain, _) = engine();
    for sql in SETUP {
        run(&mut plain, sql);
    }
    assert_eq!(
        rows(&mut plain, "SELECT id FROM t WHERE id = '1'", &[]),
        vec![vec!["i:1"]]
    );
    assert_eq!(
        rows(&mut plain, "SELECT id FROM t WHERE n = '20'", &[]),
        vec![vec!["i:2"], vec!["i:3"], vec!["i:7"]]
    );
    assert_eq!(
        rows(
            &mut plain,
            "SELECT id FROM t WHERE id BETWEEN '1' AND '3'",
            &[]
        ),
        vec![vec!["i:1"], vec!["i:2"], vec!["i:3"]]
    );

    // Proof the index was never the access path for the converting terms —
    // an indexed engine that silently used a stale/unconverted key here would
    // still pass the row-count assertions above by coincidence if it read
    // every entry, so this checks the *mechanism*, not only the outcome.
    let (mut indexed, probe) = engine();
    for sql in SETUP {
        run(&mut indexed, sql);
    }
    for sql in INDEXES {
        run(&mut indexed, sql);
    }
    for sql in [
        "SELECT id FROM t WHERE id = '1'",
        "SELECT id FROM t WHERE n = '20'",
        "SELECT id FROM t WHERE r = '1.5'",
    ] {
        probe.reset();
        rows(&mut indexed, sql, &[]);
        assert!(
            probe.scans_of("t") > 0,
            "`{sql}` must fall back to a scan rather than probe with an \
             unconverted key"
        );
    }
}

#[test]
fn ranges_agree_with_the_unindexed_table() {
    same_with_and_without_index(&[
        "SELECT id FROM t WHERE n > 0",
        "SELECT id FROM t WHERE n >= 0",
        "SELECT id FROM t WHERE n < 20",
        "SELECT id FROM t WHERE n <= 20",
        "SELECT id FROM t WHERE n > 0 AND n < 20",
        "SELECT id FROM t WHERE n BETWEEN -7 AND 10",
        "SELECT id FROM t WHERE n NOT BETWEEN -7 AND 10",
        "SELECT id FROM t WHERE r > 1.0",
        "SELECT id FROM t WHERE r < 1e308",
        "SELECT id FROM t WHERE r <= 1e308",
        // The prefix-versus-longer-string case, which is where a text encoding
        // that is not prefix-free goes wrong.
        "SELECT id FROM t WHERE s > 'apple'",
        "SELECT id FROM t WHERE s >= 'apple'",
        "SELECT id FROM t WHERE s < 'apple'",
        "SELECT id FROM t WHERE s <= 'apple pie'",
        "SELECT id FROM t WHERE b > x'00'",
        "SELECT id FROM t WHERE b < x'01'",
    ]);
}

#[test]
fn compound_and_ordered_queries_agree_with_the_unindexed_table() {
    same_with_and_without_index(&[
        "SELECT id FROM t WHERE n = 20 AND s = 'apple'",
        "SELECT id FROM t WHERE n = 20 AND s > 'a'",
        "SELECT id FROM t WHERE n = 20 AND id > 2",
        "SELECT id FROM t WHERE n = 10 OR s = 'apple'",
        "SELECT id, n FROM t WHERE n >= 0 ORDER BY n DESC, id",
        "SELECT id FROM t WHERE n = 20 ORDER BY id DESC LIMIT 2",
        // An unsorted `LIMIT` over an index probe: the pushdown is live here,
        // so the pipeline stops pulling row ids off the probe part way through
        // the range. It has to stop on the same rows a scan would have.
        "SELECT id FROM t WHERE n = 20 LIMIT 2",
        "SELECT id FROM t WHERE n = 20 LIMIT 1 OFFSET 1",
        "SELECT id FROM t WHERE n >= 0 LIMIT 3",
        "SELECT id FROM t WHERE n > 999 LIMIT 3",
        "SELECT COUNT(*) FROM t WHERE n = 20",
        "SELECT n, COUNT(*) FROM t WHERE n IS NOT NULL GROUP BY n ORDER BY n",
        "SELECT DISTINCT n FROM t WHERE n > -100 ORDER BY n",
        // A self-join: the driving side may use an index, the inner side is a
        // scan, and the result must not care.
        "SELECT a.id, b.id FROM t AS a JOIN t AS b ON a.n = b.n WHERE a.n = 20",
        "SELECT a.id FROM t AS a LEFT JOIN t AS b ON a.id = b.n WHERE a.n = 20",
    ]);
}

// ------------------------------------------------- index nested-loop join

/// The two tables every join comparison joins, and the rows that make the
/// interesting cases reachable.
///
/// `a` is the outer side and its keys cover each shape a probe has to survive:
/// a key that matches two inner rows, one that matches one, one that matches
/// none, a `NULL`, a key that names a *row id* rather than an indexed value, and
/// — through `a.r` — a `REAL` key against an `INTEGER` column, which the index
/// encoding deliberately puts in one domain. `b`'s keys carry duplicates and a
/// `NULL` of their own; `e` stays empty.
const JOIN_SETUP: &[&str] = &[
    "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, s TEXT, r REAL)",
    "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, s TEXT, note TEXT)",
    "CREATE TABLE e (id INTEGER PRIMARY KEY, k INTEGER)",
    "INSERT INTO a VALUES (1, 10, 'x', 10.0)",
    "INSERT INTO a VALUES (2, 10, 'y', 2.0)",
    "INSERT INTO a VALUES (3, 20, 'x', 2.5)",
    "INSERT INTO a VALUES (4, NULL, 'x', NULL)",
    "INSERT INTO a VALUES (5, 99, NULL, 30.0)",
    "INSERT INTO a VALUES (6, 2, 'z', 99.0)",
    "INSERT INTO b VALUES (1, 10, 'x', 'first')",
    "INSERT INTO b VALUES (2, 10, 'y', NULL)",
    "INSERT INTO b VALUES (3, 20, 'x', 'third')",
    "INSERT INTO b VALUES (4, NULL, 'x', 'fourth')",
    "INSERT INTO b VALUES (5, 30, NULL, 'fifth')",
];

/// The indexes the "with" side of every join comparison declares.
///
/// `b_ks` is here so the composite case is covered: an equality on its leading
/// column alone is a prefix range, and its entries within that range are in
/// *value* order rather than row-id order — which is what the probe has to put
/// back.
///
/// `USING BTREE` on the text columns is load-bearing: a `TEXT` column's index
/// is a full-text one unless the declaration says otherwise, and a full-text
/// index is not an ordered access path, so the rule declines it and the join
/// falls back — which is what
/// [`a_join_the_rule_does_not_cover_falls_back_to_the_scan`] checks on purpose.
const JOIN_INDEXES: &[&str] = &[
    "CREATE INDEX b_k ON b (k)",
    "CREATE INDEX b_s ON b (s) USING BTREE",
    "CREATE INDEX b_ks ON b (k, s)",
    // Full-text, by that default, and therefore never a join access path.
    "CREATE INDEX b_note ON b (note)",
    "CREATE INDEX a_k ON a (k)",
    "CREATE INDEX e_k ON e (k)",
];

/// Build the same two tables twice — once indexed, once not — and assert every
/// query agrees, row for row and in the same order.
///
/// The join counterpart of [`same_with_and_without_index`], and it is the test
/// the index nested-loop join exists to survive: the probe reads a different
/// set of rows, and must produce the identical answer from them.
fn same_join_with_and_without_index(queries: &[&str]) {
    let (mut plain, plain_probe) = engine();
    let (mut indexed, indexed_probe) = engine();
    for sql in JOIN_SETUP {
        run(&mut plain, sql);
        run(&mut indexed, sql);
    }
    for sql in JOIN_INDEXES {
        run(&mut indexed, sql);
    }

    for sql in queries {
        let expected = rows(&mut plain, sql, &[]);
        let actual = rows(&mut indexed, sql, &[]);
        assert_eq!(
            actual, expected,
            "`{sql}` disagreed with the unindexed tables"
        );
    }
    assert!(
        !indexed_probe.entries().is_empty(),
        "the indexed side built no entries, so this compared nothing"
    );
    assert!(plain_probe.entries().is_empty());
}

/// The headline equivalence: a probed join answers exactly as a materialised
/// one, over every shape the rule has to get right.
#[test]
fn joins_agree_with_and_without_the_index() {
    same_join_with_and_without_index(&[
        // The plain shapes, both kinds, in both operand orders. Unordered as
        // well as ordered: the probe has to reproduce the *arrival* order, not
        // only the set.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k",
        "SELECT a.id, b.note FROM a LEFT JOIN b ON b.k = a.k ORDER BY a.id, b.id",
        // The row-id probe: one descent, at most one row. Row 6's key is 2,
        // which names a row; row 5's is 99, which names none.
        "SELECT a.id, b.note FROM a JOIN b ON a.k = b.id",
        "SELECT a.id, b.note FROM a LEFT JOIN b ON a.k = b.id",
        "SELECT a.id, b.note FROM a JOIN b ON b.id = a.k",
        // A REAL key against an INTEGER column, which the index encoding puts
        // in one domain because `=` does: `10.0` finds the rows `10` does.
        "SELECT a.id, b.id FROM a JOIN b ON a.r = b.k",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.r = b.k",
        // The same against the row id, where `2.0` names row 2, `2.5` names
        // nothing at all, and `99.0` is past the end of the table.
        "SELECT a.id, b.note FROM a JOIN b ON a.r = b.id",
        "SELECT a.id, b.note FROM a LEFT JOIN b ON a.r = b.id",
        // A text key, which is a different class through the same encoding.
        "SELECT a.id, b.id FROM a JOIN b ON a.s = b.s",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.s = b.s",
        // Composite `ON`: one conjunct is the probe, the rest are the residual
        // filter the operator re-applies over the probed rows.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.s = b.s",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k AND a.s = b.s",
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND b.note IS NOT NULL",
        "SELECT a.id, b.id FROM a JOIN b ON b.note IS NOT NULL AND a.k = b.k",
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND b.id > 1 AND a.id < 4",
        // Two probeable equalities at once: whichever the rule picks, the
        // answer is the other one's too.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.k = b.id",
        // An empty inner table, probed and materialised alike.
        "SELECT a.id, e.id FROM a JOIN e ON a.k = e.k",
        "SELECT a.id, e.id FROM a LEFT JOIN e ON a.k = e.k",
        "SELECT a.id, e.id FROM a LEFT JOIN e ON a.k = e.id",
        // The shapes the rule does not cover, which must fall back and still
        // agree: a range, a disjunction, a literal, a cross join, an equality
        // between two columns of the same table.
        "SELECT a.id, b.id FROM a JOIN b ON a.k > b.k",
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k OR a.id = b.id",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = 10",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.k = b.id",
        "SELECT a.id, b.id FROM a, b",
        // The rest of the pipeline over a probed join.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k LIMIT 3",
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k LIMIT 2 OFFSET 1",
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE b.note IS NOT NULL",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k ORDER BY b.id DESC, a.id",
        "SELECT COUNT(*) FROM a JOIN b ON a.k = b.k",
        "SELECT a.k, COUNT(*) FROM a JOIN b ON a.k = b.k GROUP BY a.k ORDER BY a.k",
        "SELECT DISTINCT a.k FROM a JOIN b ON a.k = b.k ORDER BY a.k",
        // Three tables: the second join's outer side is the first join's
        // output, so its probe reads its key out of a row that is itself joined.
        "SELECT a.id, b.id, c.id FROM a JOIN b ON a.k = b.k JOIN b AS c ON b.k = c.k",
        "SELECT a.id, b.id, c.id FROM a LEFT JOIN b ON a.k = b.id LEFT JOIN b AS c ON a.k = c.k",
        // A self-join, where the same table is both sides of the probe.
        "SELECT x.id, y.id FROM b AS x JOIN b AS y ON x.k = y.k ORDER BY x.id, y.id",
    ]);
}

/// The index probe is the access path an *early-stopping* join takes: with a
/// `LIMIT`, the inner table is probed, not walked.
///
/// The driving table is still scanned — it has to be — so this is asserted per
/// table rather than per statement. A full scan (no `LIMIT`) prefers the hash
/// join instead; see
/// [`a_full_scan_join_builds_a_hash_table_instead_of_probing`].
#[test]
fn an_indexed_join_probes_the_inner_table_instead_of_scanning_it() {
    let (mut indexed, probe) = engine();
    for sql in JOIN_SETUP {
        run(&mut indexed, sql);
    }
    for sql in JOIN_INDEXES {
        run(&mut indexed, sql);
    }

    for sql in [
        // By secondary index.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k LIMIT 2",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.k = a.k LIMIT 2",
        "SELECT a.id, b.id FROM a JOIN b ON a.s = b.s LIMIT 2",
        // By row id.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.id LIMIT 2",
        // With a residual conjunct, which does not disturb the probe.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.s = b.s LIMIT 2",
    ] {
        probe.reset();
        rows(&mut indexed, sql, &[]);
        assert_eq!(probe.scans_of("b"), 0, "`{sql}` scanned the inner table");
        assert!(probe.scans_of("a") > 0, "`{sql}` did not scan the outer");
    }

    // Without the index the same query walks `b`, which is what makes the
    // assertion above mean anything.
    let (mut plain, plain_probe) = engine();
    for sql in JOIN_SETUP {
        run(&mut plain, sql);
    }
    plain_probe.reset();
    rows(
        &mut plain,
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k LIMIT 2",
        &[],
    );
    assert!(plain_probe.scans_of("b") > 0);
}

/// A full scan prefers the hash join over the index probe: the inner table is
/// read once into buckets, not descended into once per outer row.
///
/// This is the O(inner) build + O(outer) probe trade against the index probe's
/// O(outer) descents: it wins when the outer side is scanned in full and loses
/// when a `LIMIT` stops the scan early — which is exactly the split the two
/// tests around this one draw.
#[test]
fn a_full_scan_join_builds_a_hash_table_instead_of_probing() {
    let (mut indexed, probe) = engine();
    let (mut plain, _) = engine();
    for engine in [&mut indexed, &mut plain] {
        for sql in JOIN_SETUP {
            run(engine, sql);
        }
    }
    for sql in JOIN_INDEXES {
        run(&mut indexed, sql);
    }

    for (sql, builds) in [
        ("SELECT a.id, b.id FROM a JOIN b ON a.k = b.k", true),
        // Same inner table, key, mask and committed version as the first
        // query: `LEFT` changes output handling, not the immutable hash build.
        ("SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k", false),
        ("SELECT a.id, b.id FROM a JOIN b ON a.s = b.s", true),
        (
            "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.s = b.s",
            true,
        ),
    ] {
        probe.reset();
        let indexed_rows = rows(&mut indexed, sql, &[]);
        // The hash join scans the inner table once to build, and never does a
        // point read by row id.
        assert_eq!(
            probe.scans_of("b") > 0,
            builds,
            "`{sql}` hash-build reuse disagreed with its physical shape"
        );
        assert_eq!(probe.reads.get(), 0, "`{sql}` probed by row id instead");
        // And it answers exactly as the unindexed scan does.
        assert_eq!(
            indexed_rows,
            rows(&mut plain, sql, &[]),
            "`{sql}` disagreed with the unindexed scan"
        );
    }
}

/// Every shape the rule declines, and it must decline rather than guess: each
/// of these still walks the inner table.
#[test]
fn a_join_the_rule_does_not_cover_falls_back_to_the_scan() {
    let (mut engine, probe) = engine();
    for sql in JOIN_SETUP {
        run(&mut engine, sql);
    }
    for sql in JOIN_INDEXES {
        run(&mut engine, sql);
    }

    for sql in [
        // Not an equality.
        "SELECT a.id, b.id FROM a JOIN b ON a.k > b.k",
        "SELECT a.id, b.id FROM a JOIN b ON a.k <> b.k",
        // A disjunction: one side of an `OR` cannot narrow the other.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k OR a.id = b.id",
        // Not a column on the outer side.
        "SELECT a.id, b.id FROM a JOIN b ON b.k = 10",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k + 0",
        // Both columns in the inner table.
        "SELECT a.id, b.id FROM a JOIN b ON b.k = b.id",
        // No `ON` at all.
        "SELECT a.id, b.id FROM a, b",
        // An index of a kind that is not an ordered access path: `b.note`
        // carries a full-text index, which cannot answer an equality range.
        "SELECT a.id, b.id FROM a JOIN b ON a.s = b.note",
    ] {
        probe.reset();
        rows(&mut engine, sql, &[]);
        assert!(
            probe.scans_of("b") > 0,
            "`{sql}` was probed, and the rule does not cover it"
        );
    }
}

/// A composite index answers a join on its *leading* column, and the rows come
/// back in row-id order.
///
/// This is the case a single-column index cannot check: entries under one
/// leading-column prefix are ordered by the *second* column and only then by row
/// id, so a probe that handed them on as it read them would emit the pairs in an
/// order the materialising path never produces. Here the second column's order
/// is deliberately the reverse of the row-id order.
///
/// A `LIMIT` is what selects the probe path here: a full scan would hash the
/// inner table and never consult this index.
#[test]
fn a_composite_index_answers_a_join_on_its_leading_column_in_row_id_order() {
    let (mut indexed, probe) = engine();
    let (mut plain, _) = engine();
    for engine in [&mut indexed, &mut plain] {
        run(engine, "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER)");
        run(
            engine,
            "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, s TEXT)",
        );
        run(engine, "INSERT INTO a VALUES (1, 7)");
        // Row ids ascend while `s` descends, so value order and row-id order
        // disagree on every pair.
        run(engine, "INSERT INTO b VALUES (1, 7, 'd')");
        run(engine, "INSERT INTO b VALUES (2, 7, 'c')");
        run(engine, "INSERT INTO b VALUES (3, 7, 'b')");
        run(engine, "INSERT INTO b VALUES (4, 7, 'a')");
    }
    run(&mut indexed, "CREATE INDEX b_ks ON b (k, s)");

    let sql = "SELECT b.id FROM a JOIN b ON a.k = b.k LIMIT 4";
    probe.reset();
    let probed = rows(&mut indexed, sql, &[]);
    assert_eq!(probe.scans_of("b"), 0, "the composite index was not used");
    assert_eq!(
        probed,
        vec![vec!["i:1"], vec!["i:2"], vec!["i:3"], vec!["i:4"]]
    );
    assert_eq!(probed, rows(&mut plain, sql, &[]));
}

/// A `NUMERIC` join key is not an access path, for the same reason a `NUMERIC`
/// filter is not: the column holds every storage class at once, and no
/// ordered index may answer for it. Before AHL-477 a cross-class join
/// comparison *errored*, which made "never probed" easy to see because the
/// error reached the caller unfiltered from the scan; now it answers by
/// SQLite's class order instead, and the column still must not become an
/// access path just because the comparison stopped failing.
#[test]
fn a_numeric_join_column_is_not_probed() {
    let (mut indexed, probe) = engine();
    run(
        &mut indexed,
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER)",
    );
    run(
        &mut indexed,
        "CREATE TABLE b (id INTEGER PRIMARY KEY, v DATETIME)",
    );
    run(&mut indexed, "CREATE INDEX b_v ON b (v)");
    run(&mut indexed, "INSERT INTO a VALUES (1, 5)");
    run(&mut indexed, "INSERT INTO b VALUES (1, 5)");

    probe.reset();
    assert_eq!(
        rows(
            &mut indexed,
            "SELECT a.id, b.id FROM a JOIN b ON a.k = b.v",
            &[]
        ),
        vec![vec!["i:1", "i:1"]]
    );
    assert!(
        probe.scans_of("b") > 0,
        "a NUMERIC column must not be a join access path"
    );

    // A `TEXT` row added to the same `NUMERIC` column: `a.k` is `5`
    // (`INTEGER`), and every `TEXT` value ranks above every number in
    // SQLite's class order, so `a.k = b.v` still cannot match it — confirmed
    // against sqlite3. The join still must not probe `b`'s index for it, and
    // now answers instead of erroring.
    run(&mut indexed, "INSERT INTO b VALUES (2, 'text')");
    probe.reset();
    assert_eq!(
        rows(
            &mut indexed,
            "SELECT a.id, b.id FROM a JOIN b ON a.k = b.v",
            &[]
        ),
        vec![vec!["i:1", "i:1"]]
    );
    assert!(
        probe.scans_of("b") > 0,
        "a NUMERIC column must not become a join access path just because a \
         cross-class compare no longer errors"
    );
}

/// A key of a class the indexed column cannot hold falls back to the scan,
/// and the scan now answers by SQLite's class order instead of erroring
/// (AHL-477) — the join counterpart of
/// [`a_mismatched_probe_falls_back_to_the_scan_that_now_answers`]. The index
/// still must not be consulted, and the plain and indexed engines still must
/// agree with each other, now on a real (empty) result instead of on the
/// same error.
#[test]
fn a_join_key_of_the_wrong_class_falls_back_to_the_scan_that_now_answers() {
    let (mut plain, _) = engine();
    let (mut indexed, indexed_probe) = engine();
    for engine in [&mut plain, &mut indexed] {
        run(engine, "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER)");
        run(engine, "CREATE TABLE b (id INTEGER PRIMARY KEY, s TEXT)");
        run(engine, "INSERT INTO a VALUES (1, 5)");
        run(engine, "INSERT INTO b VALUES (1, 'five')");
    }
    run(&mut indexed, "CREATE INDEX b_s ON b (s) USING BTREE");

    let sql = "SELECT a.id, b.id FROM a JOIN b ON a.k = b.s";
    // `5` (`INTEGER`) against `'five'` (`TEXT`): every number ranks below
    // every `TEXT` value, so this cannot match — confirmed against sqlite3.
    indexed_probe.reset();
    let expected = rows(&mut plain, sql, &[]);
    let actual = rows(&mut indexed, sql, &[]);
    assert_eq!(expected, Vec::<Vec<String>>::new());
    assert_eq!(actual, expected);
    assert!(
        indexed_probe.scans_of("b") > 0,
        "a TEXT column must not be probed for an INTEGER join key"
    );
}

/// AHL-486, written as a `JOIN`'s `ON` instead of a `WHERE`: `a.k` is
/// `INTEGER` and `b.s` is `TEXT` holding the numeral's own spelling, so
/// SQLite's comparison affinity converts `b.s` before comparing — confirmed
/// against a real sqlite3 3.54 binary — where
/// [`a_join_key_of_the_wrong_class_falls_back_to_the_scan_that_now_answers`]
/// above is the same shape with a value that does *not* convert. The plain
/// and indexed engines have to agree on the row this now finds, and the
/// index still must not be the access path for it: `join_probe` offers
/// `b_s` as a candidate (`b.s`'s declared type is a storage class), but
/// `IndexProbe::prepare` rejects the outer `INTEGER` key against a `TEXT`-
/// keyed index at that point and falls back to reading the whole of `b`,
/// which is what lets `NestedLoopJoin`'s full re-evaluation of `a.k = b.s` —
/// now affinity-aware — decide the match instead of a stale unconverted key.
#[test]
fn a_join_key_the_affinity_now_converts_agrees_with_and_without_the_index() {
    let (mut plain, _) = engine();
    let (mut indexed, indexed_probe) = engine();
    for engine in [&mut plain, &mut indexed] {
        run(engine, "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER)");
        run(engine, "CREATE TABLE b (id INTEGER PRIMARY KEY, s TEXT)");
        run(engine, "INSERT INTO a VALUES (1, 5)");
        run(engine, "INSERT INTO b VALUES (1, '5'), (2, 'five')");
    }
    run(&mut indexed, "CREATE INDEX b_s ON b (s) USING BTREE");

    let sql = "SELECT a.id, b.id FROM a JOIN b ON a.k = b.s";
    indexed_probe.reset();
    let expected = rows(&mut plain, sql, &[]);
    let actual = rows(&mut indexed, sql, &[]);
    assert_eq!(expected, vec![vec!["i:1", "i:1"]]);
    assert_eq!(actual, expected);
    assert!(
        indexed_probe.scans_of("b") > 0,
        "an INTEGER join key against a TEXT-affinity index must fall back to \
         the scan even once affinity conversion is what makes it match"
    );
}

/// A `NULL` join key matches nothing, including another `NULL`, and a probe
/// that read the index's `NULL` entries would have had to reject them anyway.
/// SQLite's rule, and `crates/inlaysql/tests/differential.rs` holds it against
/// SQLite itself.
#[test]
fn a_null_join_key_matches_nothing_on_either_path() {
    same_join_with_and_without_index(&[
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.id = 4",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k WHERE a.id = 4",
        // The inner side's `NULL` is unreachable from either direction.
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE b.id = 4",
        "SELECT a.id, b.s FROM a LEFT JOIN b ON a.s = b.s WHERE a.id = 5",
    ]);

    // And the answer itself, so the comparison above cannot pass by both sides
    // being wrong in the same way.
    let (mut engine, _) = engine();
    for sql in JOIN_SETUP {
        run(&mut engine, sql);
    }
    for sql in JOIN_INDEXES {
        run(&mut engine, sql);
    }
    assert_eq!(
        rows(
            &mut engine,
            "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k WHERE a.id = 4",
            &[]
        ),
        vec![vec!["i:4", "NULL"]]
    );
}

// -------------------------------------------------------------------- UNIQUE

/// The error message is the observable half of a `UNIQUE` constraint, and it
/// must not change just because the check got faster.
#[test]
fn unique_is_enforced_by_the_index_with_the_same_error() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, e TEXT UNIQUE)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'a')");

    probe.scans.set(0);
    let err = refuse(&mut engine, "INSERT INTO t VALUES (2, 'a')");
    assert!(matches!(err, Error::Constraint(_)), "got {err}");
    assert!(
        err.to_string().ends_with("UNIQUE constraint failed: t.e"),
        "got {err}"
    );
    assert_eq!(
        probe.scans.get(),
        0,
        "the UNIQUE check scanned instead of probing"
    );

    // The UPDATE path is the same check with one row excluded.
    run(&mut engine, "INSERT INTO t VALUES (2, 'b')");
    probe.scans.set(0);
    let err = refuse(&mut engine, "UPDATE t SET e = 'a' WHERE id = 2");
    assert!(
        err.to_string().ends_with("UNIQUE constraint failed: t.e"),
        "got {err}"
    );
    assert_eq!(probe.scans.get(), 0);

    // A row always collides with itself, and that is not a violation.
    run(&mut engine, "UPDATE t SET e = 'b' WHERE id = 2");

    // SQLite's rule: a NULL never collides, with anything, including another
    // NULL.
    run(&mut engine, "INSERT INTO t VALUES (3, NULL)");
    run(&mut engine, "INSERT INTO t VALUES (4, NULL)");
    assert_eq!(
        rows(&mut engine, "SELECT COUNT(*) FROM t", &[]),
        vec![vec!["i:4"]]
    );
}

#[test]
fn a_unique_index_is_the_constraint_and_the_access_path() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT)",
    );
    run(&mut engine, "INSERT INTO users VALUES (1, 'a@example.com')");
    run(
        &mut engine,
        "CREATE UNIQUE INDEX users_email ON users (email)",
    );
    assert_entry_count(&probe, 1, 1);

    let err = refuse(&mut engine, "INSERT INTO users VALUES (2, 'a@example.com')");
    assert!(
        err.to_string()
            .ends_with("UNIQUE constraint failed: users.email"),
        "got {err}"
    );

    // And it answers the query an ORM emits all day.
    probe.scans.set(0);
    assert_eq!(
        rows(
            &mut engine,
            "SELECT id FROM users WHERE email = 'a@example.com'",
            &[]
        ),
        vec![vec!["i:1"]]
    );
    assert_eq!(probe.scans.get(), 0);

    // One name, one object: DROP INDEX removes both halves.
    run(&mut engine, "DROP INDEX users_email");
    assert_entry_count(&probe, 0, 0);
    run(&mut engine, "INSERT INTO users VALUES (2, 'a@example.com')");
}

/// A unique index over rows that already violate it is an error, not a
/// constraint that starts out already broken — and the table must be
/// unchanged afterwards.
#[test]
fn a_unique_index_over_duplicate_rows_is_refused() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, e TEXT)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'a'), (2, 'a')");
    let err = refuse(&mut engine, "CREATE UNIQUE INDEX t_e ON t (e)");
    assert!(matches!(err, Error::Constraint(_)), "got {err}");
    assert!(
        err.to_string().ends_with("UNIQUE constraint failed: t.e"),
        "got {err}"
    );
    assert_entry_count(&probe, 0, 0);
    assert!(engine.catalog().indexes_for("t").is_empty());

    // The refusal left nothing behind, so the same statement works once the
    // duplicate is gone.
    run(&mut engine, "DELETE FROM t WHERE id = 2");
    run(&mut engine, "CREATE UNIQUE INDEX t_e ON t (e)");
    assert_entry_count(&probe, 1, 1);
}

#[test]
fn a_composite_unique_constraint_is_probed_not_scanned() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, UNIQUE (a, b))",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 1, 'x'), (2, 1, 'y')");
    probe.scans.set(0);
    let err = refuse(&mut engine, "INSERT INTO t VALUES (3, 1, 'x')");
    assert!(
        err.to_string()
            .ends_with("UNIQUE constraint failed: t.a, t.b"),
        "got {err}"
    );
    assert_eq!(probe.scans.get(), 0);
    // Different in one column is not a collision.
    run(&mut engine, "INSERT INTO t VALUES (3, 2, 'x')");
}

/// `ON CONFLICT` reaches the same rows through the index that it used to reach
/// through the scan, so the clause behaves identically.
#[test]
fn on_conflict_still_finds_the_row_the_index_points_at() {
    let (mut engine, _) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, e TEXT UNIQUE, n INTEGER)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'a', 1)");
    run(
        &mut engine,
        "INSERT INTO t VALUES (2, 'a', 2) ON CONFLICT (e) DO UPDATE SET n = excluded.n",
    );
    assert_eq!(
        rows(&mut engine, "SELECT id, e, n FROM t", &[]),
        vec![vec!["i:1", "t:a", "i:2"]]
    );

    run(
        &mut engine,
        "INSERT INTO t VALUES (3, 'a', 3) ON CONFLICT DO NOTHING",
    );
    assert_eq!(
        rows(&mut engine, "SELECT id, e, n FROM t", &[]),
        vec![vec!["i:1", "t:a", "i:2"]]
    );

    run(&mut engine, "INSERT OR REPLACE INTO t VALUES (4, 'a', 4)");
    assert_eq!(
        rows(&mut engine, "SELECT id, e, n FROM t", &[]),
        vec![vec!["i:4", "t:a", "i:4"]]
    );
}

// ------------------------------------------------------- what stays refused

/// `NaN` compares equal to every number in this engine, which no ordered index
/// can reproduce. Writing one into an indexed column is refused rather than
/// indexed wrongly — and the refusal leaves the table as it was.
#[test]
fn a_nan_in_an_indexed_column_is_refused() {
    let (mut plain, _) = engine();
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, r REAL)",
    );
    run(&mut engine, "CREATE INDEX t_r ON t (r)");
    let err = engine
        .execute(
            "INSERT INTO t VALUES (?, ?)",
            &[Value::Integer(1), Value::Real(f64::NAN)],
        )
        .expect_err("a NaN must not be indexed");
    assert!(matches!(err, Error::Unsupported(_)), "got {err}");
    assert!(err.to_string().contains("NaN"), "got {err}");
    assert_entry_count(&probe, 0, 1);
    assert!(rows(&mut engine, "SELECT id FROM t", &[]).is_empty());

    // Without an index there is no ordering to be wrong about, so it is
    // stored, exactly as before.
    run(
        &mut plain,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, r REAL)",
    );
    plain
        .execute(
            "INSERT INTO t VALUES (?, ?)",
            &[Value::Integer(1), Value::Real(f64::NAN)],
        )
        .expect("no index, no ordering to break");
}

#[test]
fn an_index_of_a_kind_this_engine_does_not_have_is_refused() {
    let (mut engine, _) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)",
    );
    for sql in [
        "CREATE INDEX t_n ON t (n) USING HASH",
        "CREATE INDEX t_n ON t (n) USING GIN",
        "CREATE INDEX t_n ON t (n) WHERE n > 0",
    ] {
        let err = refuse(&mut engine, sql);
        assert!(matches!(err, Error::Unsupported(_)), "`{sql}` gave {err}");
    }
    assert!(engine.catalog().indexes_for("t").is_empty());
}

#[test]
fn using_names_the_structure_when_the_column_type_would_not() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'hello world')");
    // The inferred kind on a TEXT column is still full-text, which is what
    // every database written against this engine assumes.
    run(&mut engine, "CREATE INDEX t_body_ft ON t (body)");
    assert_entry_count(&probe, 0, 0);
    // And the scalar one is available by saying so, on the same column.
    run(
        &mut engine,
        "CREATE INDEX t_body_bt ON t (body) USING BTREE",
    );
    assert_entry_count(&probe, 1, 1);

    probe.scans.set(0);
    assert_eq!(
        rows(
            &mut engine,
            "SELECT id FROM t WHERE body = 'hello world'",
            &[]
        ),
        vec![vec!["i:1"]]
    );
    assert_eq!(probe.scans.get(), 0);
    // Both still answer their own kind of question.
    assert_eq!(
        rows(
            &mut engine,
            "SELECT id, bm25_score(body, 'hello') FROM t ORDER BY score DESC LIMIT 1",
            &[]
        )
        .len(),
        1
    );
}

// ---------------------------------------------------------------- collations
//
// A `NOCASE` index keys the *folded* value (`inlaysql_core::index`), so an
// index probe and a scan read different bytes for the same query. If the
// planner ever chose an index whose collation is not the one the comparison
// resolved, the two would return different rows — divergence by access path,
// which is the failure this whole file is built to catch. Everything below is
// that check, for collations (AHL-469).

/// One `NOCASE` column, one `BINARY`, one `RTRIM`, and a second table to join
/// against. The rows differ only in case wherever it matters, so a comparison
/// that ignored the collation would give a visibly different answer.
const COLLATION_SETUP: &[&str] = &[
    "CREATE TABLE p (id INTEGER PRIMARY KEY, nc TEXT COLLATE NOCASE, bin TEXT, \
     rt TEXT COLLATE RTRIM)",
    "INSERT INTO p VALUES (1, 'ada', 'ADA', 'a')",
    "INSERT INTO p VALUES (2, 'ADA', 'ada', 'a  ')",
    "INSERT INTO p VALUES (3, 'Grace', 'Grace', 'g')",
    "INSERT INTO p VALUES (4, 'grace', 'GRACE', 'G')",
    "INSERT INTO p VALUES (5, NULL, NULL, NULL)",
    "INSERT INTO p VALUES (6, '', '', '')",
    "INSERT INTO p VALUES (7, 'ada', 'ada', 'a')",
    "CREATE TABLE q (id INTEGER PRIMARY KEY, nc TEXT COLLATE NOCASE, bin TEXT)",
    "INSERT INTO q VALUES (1, 'ADA', 'ada')",
    "INSERT INTO q VALUES (2, 'grace', 'GRACE')",
    "INSERT INTO q VALUES (3, 'zoe', 'zoe')",
    "INSERT INTO q VALUES (4, NULL, NULL)",
];

/// The indexes the "with" side declares.
///
/// `p.bin` carries two: one keyed under `BINARY` and one under `NOCASE`. That
/// is the case the selection rule exists for — the column declares one
/// collation and the comparison may resolve either, so choosing by the column
/// would be wrong half the time.
const COLLATION_INDEXES: &[&str] = &[
    "CREATE INDEX p_nc ON p (nc) USING BTREE",
    "CREATE INDEX p_bin ON p (bin) USING BTREE",
    "CREATE INDEX p_bin_nc ON p (bin COLLATE NOCASE) USING BTREE",
    "CREATE INDEX p_rt ON p (rt) USING BTREE",
    "CREATE INDEX q_nc ON q (nc) USING BTREE",
    "CREATE INDEX q_bin ON q (bin) USING BTREE",
];

/// [`same_with_and_without_index`] over the collation fixture.
fn same_collated_with_and_without_index(queries: &[&str]) {
    let (mut plain, plain_probe) = engine();
    let (mut indexed, indexed_probe) = engine();
    for sql in COLLATION_SETUP {
        run(&mut plain, sql);
        run(&mut indexed, sql);
    }
    for sql in COLLATION_INDEXES {
        run(&mut indexed, sql);
    }

    for sql in queries {
        let expected = rows(&mut plain, sql, &[]);
        let actual = rows(&mut indexed, sql, &[]);
        assert_eq!(
            actual, expected,
            "`{sql}` disagreed with the unindexed tables"
        );
    }
    assert!(
        !indexed_probe.entries().is_empty(),
        "the indexed side built no entries, so this compared nothing"
    );
    assert!(plain_probe.entries().is_empty());
}

/// The headline equivalence, for collations: every comparison answers the same
/// whether an index took part or not.
#[test]
fn collated_queries_agree_with_and_without_the_index() {
    same_collated_with_and_without_index(&[
        // Equality on each declared collation, both operand orders.
        "SELECT id FROM p WHERE nc = 'ADA' ORDER BY id",
        "SELECT id FROM p WHERE 'ADA' = nc ORDER BY id",
        "SELECT id FROM p WHERE bin = 'ADA' ORDER BY id",
        "SELECT id FROM p WHERE rt = 'a' ORDER BY id",
        "SELECT id FROM p WHERE rt = 'a    ' ORDER BY id",
        "SELECT id FROM p WHERE nc = '' ORDER BY id",
        "SELECT id FROM p WHERE nc IS NULL ORDER BY id",
        // An explicit COLLATE on either side, which is what makes the two
        // indexes on `bin` both reachable and both wrong for the other query.
        "SELECT id FROM p WHERE bin = 'ADA' COLLATE NOCASE ORDER BY id",
        "SELECT id FROM p WHERE bin COLLATE NOCASE = 'ADA' ORDER BY id",
        "SELECT id FROM p WHERE nc = 'ADA' COLLATE BINARY ORDER BY id",
        "SELECT id FROM p WHERE nc COLLATE BINARY = 'ADA' ORDER BY id",
        // Ranges, where folding has to preserve order and not only equality.
        "SELECT id FROM p WHERE nc > 'B' ORDER BY id",
        "SELECT id FROM p WHERE nc >= 'ADA' ORDER BY id",
        "SELECT id FROM p WHERE nc < 'grace' ORDER BY id",
        "SELECT id FROM p WHERE bin > 'B' ORDER BY id",
        "SELECT id FROM p WHERE bin > 'B' COLLATE NOCASE ORDER BY id",
        "SELECT id FROM p WHERE nc BETWEEN 'a' AND 'grace' ORDER BY id",
        "SELECT id FROM p WHERE bin BETWEEN 'a' AND 'B' COLLATE NOCASE ORDER BY id",
        "SELECT id FROM p WHERE rt > 'a' ORDER BY id",
        // `IN`, whose collation comes from the left operand alone.
        "SELECT id FROM p WHERE nc IN ('ADA', 'GRACE') ORDER BY id",
        "SELECT id FROM p WHERE bin IN ('ADA', 'GRACE') ORDER BY id",
        // The rest of the pipeline, which folds by collation too.
        "SELECT id FROM p ORDER BY nc, id",
        "SELECT id FROM p ORDER BY bin, id",
        "SELECT id FROM p ORDER BY bin COLLATE NOCASE, id",
        "SELECT nc, COUNT(*) FROM p GROUP BY nc ORDER BY nc",
        "SELECT bin, COUNT(*) FROM p GROUP BY bin ORDER BY bin",
        "SELECT DISTINCT nc FROM p ORDER BY nc",
        "SELECT DISTINCT bin FROM p ORDER BY bin",
        "SELECT COUNT(DISTINCT nc), COUNT(DISTINCT bin) FROM p",
        "SELECT MIN(nc), MAX(nc), MIN(bin), MAX(bin) FROM p",
        // `LIKE` never uses a collating sequence, so it must not be answered
        // from a collated index either.
        "SELECT id FROM p WHERE nc LIKE 'ada' ORDER BY id",
        "SELECT id FROM p WHERE bin LIKE 'ada' ORDER BY id",
        // A `LIMIT` over an indexed filter stops early; it has to stop on the
        // same rows.
        "SELECT id FROM p WHERE nc = 'ADA' ORDER BY id LIMIT 1",
    ]);
}

/// The cross-feature check against the index nested-loop join (AHL-464): a
/// join whose `ON` is over a collated column must probe only when the index's
/// collation matches, and must answer identically either way.
#[test]
fn collated_joins_agree_with_and_without_the_index() {
    same_collated_with_and_without_index(&[
        // Both sides NOCASE: the probe is legal and folds.
        "SELECT p.id, q.id FROM p JOIN q ON p.nc = q.nc ORDER BY p.id, q.id",
        "SELECT p.id, q.id FROM p LEFT JOIN q ON p.nc = q.nc ORDER BY p.id, q.id",
        // Both sides BINARY.
        "SELECT p.id, q.id FROM p JOIN q ON p.bin = q.bin ORDER BY p.id, q.id",
        // Mixed, in both orders — the left operand decides, so these two are
        // *different queries*, and each has to match its own unindexed answer.
        "SELECT p.id, q.id FROM p JOIN q ON p.nc = q.bin ORDER BY p.id, q.id",
        "SELECT p.id, q.id FROM p JOIN q ON p.bin = q.nc ORDER BY p.id, q.id",
        "SELECT p.id, q.id FROM p LEFT JOIN q ON p.bin = q.nc ORDER BY p.id, q.id",
        // An explicit COLLATE on the `ON`, which changes which index may serve.
        "SELECT p.id, q.id FROM p JOIN q ON p.bin = q.bin COLLATE NOCASE ORDER BY p.id, q.id",
        // A residual conjunct beside the probe.
        "SELECT p.id, q.id FROM p JOIN q ON p.nc = q.nc AND q.id > 1 ORDER BY p.id, q.id",
        // And the aggregate/limit shapes over a probed join.
        "SELECT COUNT(*) FROM p JOIN q ON p.nc = q.nc",
        "SELECT p.id, q.id FROM p JOIN q ON p.nc = q.nc ORDER BY p.id, q.id LIMIT 2",
    ]);
}

/// The cross-feature check against subqueries (AHL-463): the comparison an
/// `IN (SELECT ...)` makes resolves a collation from the probe and the
/// subquery's column, and the answer must not depend on an index.
#[test]
fn collated_subqueries_agree_with_and_without_the_index() {
    same_collated_with_and_without_index(&[
        "SELECT id FROM p WHERE nc IN (SELECT nc FROM q) ORDER BY id",
        "SELECT id FROM p WHERE bin IN (SELECT bin FROM q) ORDER BY id",
        "SELECT id FROM p WHERE bin IN (SELECT nc FROM q) ORDER BY id",
        "SELECT id FROM p WHERE nc IN (SELECT bin FROM q) ORDER BY id",
        "SELECT id FROM p WHERE nc NOT IN (SELECT nc FROM q WHERE nc IS NOT NULL) ORDER BY id",
        "SELECT id FROM p WHERE EXISTS (SELECT 1 FROM q WHERE q.nc = p.nc) ORDER BY id",
        "SELECT id FROM p WHERE EXISTS (SELECT 1 FROM q WHERE q.bin = p.nc) ORDER BY id",
        "SELECT id, (SELECT COUNT(*) FROM q WHERE q.nc = p.nc) FROM p ORDER BY id",
        // A derived table's synthetic columns carry the projected
        // expressions' collations, so the same comparison one level down has
        // to reach the same answer — and the index selection under it has to
        // decline, because a derived table has no index at all.
        "SELECT id FROM (SELECT id, nc, bin FROM p) d WHERE d.nc = 'ADA' ORDER BY id",
        "SELECT id FROM (SELECT id, nc, bin FROM p) d WHERE d.bin = 'ADA' ORDER BY id",
        "SELECT id FROM (SELECT id, bin COLLATE NOCASE AS s FROM p) d WHERE d.s = 'ADA' \
         ORDER BY id",
        "SELECT COUNT(*) FROM (SELECT DISTINCT s FROM (SELECT nc AS s FROM p) d)",
        "SELECT id FROM (SELECT id, nc AS s FROM p) d ORDER BY d.s, id",
    ]);
}

/// The rule itself: an index whose collation matches answers without a scan,
/// and one whose collation does not is declined rather than used.
///
/// Declining is not a missed optimisation — it is the only correct answer. A
/// `BINARY` index probed for a `NOCASE` equality would look up the unfolded
/// bytes and miss every row that differs only in case.
#[test]
fn an_index_answers_only_the_collation_it_is_keyed_under() {
    let (mut engine, probe) = engine();
    for sql in COLLATION_SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX p_nc ON p (nc) USING BTREE");
    run(&mut engine, "CREATE INDEX p_bin ON p (bin) USING BTREE");

    // Each column's own collation: answered from its index.
    for sql in [
        "SELECT id FROM p WHERE nc = 'ADA'",
        "SELECT id FROM p WHERE bin = 'ADA'",
        "SELECT id FROM p WHERE nc > 'B'",
    ] {
        probe.scans.set(0);
        rows(&mut engine, sql, &[]);
        assert_eq!(probe.scans.get(), 0, "`{sql}` still scanned");
    }

    // The other collation: no index is keyed under it, so the scan stands.
    for sql in [
        "SELECT id FROM p WHERE nc = 'ADA' COLLATE BINARY",
        "SELECT id FROM p WHERE bin = 'ADA' COLLATE NOCASE",
        "SELECT id FROM p WHERE bin COLLATE NOCASE > 'B'",
    ] {
        probe.scans.set(0);
        rows(&mut engine, sql, &[]);
        assert!(
            probe.scans.get() > 0,
            "`{sql}` was answered from an index keyed under another collation"
        );
    }

    // And once the matching index exists, the same query stops scanning — the
    // rule is about the collation, not about giving up on text.
    run(
        &mut engine,
        "CREATE INDEX p_bin_nc ON p (bin COLLATE NOCASE) USING BTREE",
    );
    probe.scans.set(0);
    assert_eq!(
        rows(
            &mut engine,
            "SELECT id FROM p WHERE bin = 'ADA' COLLATE NOCASE",
            &[]
        ),
        vec![vec!["i:1"], vec!["i:2"], vec!["i:7"]]
    );
    assert_eq!(probe.scans.get(), 0);
}

/// The same rule on the join side (AHL-464): the probe is only built when the
/// index's collation is the one the `ON` resolved.
#[test]
fn a_join_probes_only_an_index_keyed_under_the_ons_collation() {
    let (mut engine, probe) = engine();
    for sql in COLLATION_SETUP {
        run(&mut engine, sql);
    }
    run(&mut engine, "CREATE INDEX q_nc ON q (nc) USING BTREE");

    // `p.nc = q.nc` resolves NOCASE, and `q_nc` is keyed under it: the inner
    // table is probed, so it is never scanned.
    probe.reset();
    rows(
        &mut engine,
        "SELECT p.id, q.id FROM p JOIN q ON p.nc = q.nc",
        &[],
    );
    assert_eq!(
        probe.scans_of("q"),
        0,
        "the inner side was scanned despite a matching index"
    );

    // `p.bin = q.nc` resolves BINARY from the left operand, and nothing is
    // keyed under BINARY on `q.nc`: the inner side falls back.
    probe.reset();
    rows(
        &mut engine,
        "SELECT p.id, q.id FROM p JOIN q ON p.bin = q.nc",
        &[],
    );
    assert!(
        probe.scans_of("q") > 0,
        "the inner side was probed with an index keyed under another collation"
    );
}

/// A `UNIQUE` constraint on a `NOCASE` column collides on case, through the
/// index probe and through the scan alike — the two paths the constraint has.
#[test]
fn a_unique_nocase_column_collides_on_case() {
    let (mut engine, _probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE UNIQUE)",
    );
    run(&mut engine, "INSERT INTO u VALUES (1, 'Ada')");
    let error = refuse(&mut engine, "INSERT INTO u VALUES (2, 'ADA')");
    assert!(matches!(error, Error::Constraint(_)), "{error:?}");
    // The same value under another case is still the same key on `UPDATE`.
    run(&mut engine, "INSERT INTO u VALUES (3, 'Grace')");
    let error = refuse(&mut engine, "UPDATE u SET name = 'ada' WHERE id = 3");
    assert!(matches!(error, Error::Constraint(_)), "{error:?}");
    // And a row that only changes its own case is not a violation of itself.
    run(&mut engine, "UPDATE u SET name = 'ADA' WHERE id = 1");
    assert_eq!(
        rows(&mut engine, "SELECT name FROM u ORDER BY id", &[]),
        vec![vec!["t:ADA"], vec!["t:Grace"]]
    );

    // A `BINARY` column with the same constraint takes both spellings, which
    // is what makes the refusals above mean something.
    run(
        &mut engine,
        "CREATE TABLE v (id INTEGER PRIMARY KEY, name TEXT UNIQUE)",
    );
    run(&mut engine, "INSERT INTO v VALUES (1, 'Ada')");
    run(&mut engine, "INSERT INTO v VALUES (2, 'ADA')");
    assert_eq!(
        rows(&mut engine, "SELECT COUNT(*) FROM v", &[]),
        vec![vec!["i:2"]]
    );
}

/// Two indexes over the same column under different collations are two
/// indexes, and each row contributes an entry to both.
#[test]
fn one_column_can_carry_two_indexes_under_two_collations() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'Ada')");
    run(&mut engine, "CREATE INDEX t_s ON t (s) USING BTREE");
    run(
        &mut engine,
        "CREATE INDEX t_s_nc ON t (s COLLATE NOCASE) USING BTREE",
    );
    assert_entry_count(&probe, 1, 2);

    // A third under a collation one of them already has is the same index
    // under another name, and is refused.
    let error = refuse(
        &mut engine,
        "CREATE INDEX t_s_again ON t (s COLLATE BINARY) USING BTREE",
    );
    assert!(matches!(error, Error::Catalog(_)), "{error:?}");

    // Both are maintained on every write.
    run(&mut engine, "INSERT INTO t VALUES (2, 'ADA')");
    assert_entry_count(&probe, 2, 2);
    run(&mut engine, "UPDATE t SET s = 'Grace' WHERE id = 1");
    assert_entry_count(&probe, 2, 2);
    run(&mut engine, "DELETE FROM t WHERE id = 2");
    assert_entry_count(&probe, 1, 2);
}

/// The entries a `NOCASE` index writes are keyed by the folded value, and the
/// row id is what keeps two rows that fold together from becoming one entry.
#[test]
fn a_nocase_index_folds_its_keys_and_still_keeps_every_row() {
    let (mut engine, probe) = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT COLLATE NOCASE)",
    );
    run(&mut engine, "CREATE INDEX t_s ON t (s) USING BTREE");
    run(&mut engine, "INSERT INTO t VALUES (1, 'Ada')");
    run(&mut engine, "INSERT INTO t VALUES (2, 'ADA')");
    run(&mut engine, "INSERT INTO t VALUES (3, 'ada')");
    assert_entry_count(&probe, 3, 1);

    // Three distinct entries whose keys agree on everything but the row id.
    let entries = probe.entries();
    let prefixes: Vec<Vec<u8>> = entries
        .iter()
        .map(|key| key[..key.len() - 8].to_vec())
        .collect();
    assert_eq!(prefixes[0], prefixes[1]);
    assert_eq!(prefixes[1], prefixes[2]);

    probe.scans.set(0);
    assert_eq!(
        rows(&mut engine, "SELECT id FROM t WHERE s = 'aDa'", &[]),
        vec![vec!["i:1"], vec!["i:2"], vec!["i:3"]]
    );
    assert_eq!(probe.scans.get(), 0);
}
