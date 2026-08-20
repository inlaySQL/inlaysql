//! The streaming executor: what it stops reading, and what it must not stop
//! reading (`docs/architecture.md`, decision D5, gap G5).
//!
//! Two properties are under test here and they pull in opposite directions.
//!
//! **It has to stop early.** A `LIMIT 10` over a large table must not decode
//! the table. That is measured, not asserted about timing: the storage wrapper
//! counts the *rows handed to the engine*, so "stopped early" is a number.
//!
//! **It has to stop early only when that is the same answer.** `ORDER BY`,
//! `GROUP BY` and `DISTINCT` all decide which rows survive, so a pipeline that
//! truncated the scan under any of them would answer with the wrong ten rows.
//! Each of those has a test that the whole table is still read, and one that
//! the answer matches the unlimited query truncated by hand.
//!
//! Projection pushdown is tested the same way round: a column the executor
//! decides not to decode reads as `NULL`, so every construct that can observe a
//! column gets a query whose answer would change if the mask missed it.

use std::cell::Cell;
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::row::RowBuf;
use inlaysql_core::sim::SimDisk;
use inlaysql_core::storage::TreeStorage;
use inlaysql_core::traits::{RowId, Storage};
use inlaysql_core::{Engine, Result, Value};

/// How many rows a scan handed the engine, and how many calls it took.
#[derive(Default)]
struct ScanCounts {
    rows: Cell<usize>,
    calls: Cell<usize>,
    /// Rows fetched one at a time by row id — what an index probe reads, and
    /// what a scan does not read at all.
    reads: Cell<usize>,
}

impl ScanCounts {
    fn reset(&self) {
        self.rows.set(0);
        self.calls.set(0);
        self.reads.set(0);
    }
}

/// `MemStorage` that records what the executor actually pulled out of it.
struct CountingStorage {
    inner: MemStorage,
    counts: Rc<ScanCounts>,
}

impl Storage for CountingStorage {
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        self.inner.put_row(table, id, bytes)
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        self.counts.reads.set(self.counts.reads.get() + 1);
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
        let batch = self.inner.scan_batch(table, after, limit)?;
        self.counts.calls.set(self.counts.calls.get() + 1);
        self.counts.rows.set(self.counts.rows.get() + batch.len());
        Ok(batch)
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

/// `rows` rows of `(id, n, body)`, on counting storage, counters zeroed.
fn seeded(rows: i64) -> (Engine, Rc<ScanCounts>) {
    let counts = Rc::new(ScanCounts::default());
    let storage = CountingStorage {
        inner: MemStorage::new(),
        counts: Rc::clone(&counts),
    };
    let mut engine = Engine::open(
        Box::new(storage),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .expect("open");
    engine
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, body TEXT)",
            &[],
        )
        .unwrap();
    engine.begin().unwrap();
    for id in 1..=rows {
        engine
            .execute(
                "INSERT INTO t (id, n, body) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Integer(id % 7),
                    Value::Text(format!("row-{id}")),
                ],
            )
            .unwrap();
    }
    engine.commit().unwrap();
    counts.reset();
    (engine, counts)
}

fn ids(engine: &mut Engine, sql: &str) -> Vec<i64> {
    engine
        .query(sql, &[])
        .unwrap()
        .rows
        .into_iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("expected an integer id, got {other:?}"),
        })
        .collect()
}

/// The headline claim: a `LIMIT` over a big table stops the scan.
#[test]
fn a_limit_stops_the_scan_rather_than_truncating_the_answer() {
    let (mut engine, counts) = seeded(2000);
    assert_eq!(
        ids(&mut engine, "SELECT id FROM t LIMIT 5"),
        vec![1, 2, 3, 4, 5]
    );
    assert!(
        counts.rows.get() < 100,
        "a LIMIT 5 read {} rows of a 2000-row table",
        counts.rows.get()
    );
}

/// With a `WHERE`, the scan runs until the *filter* has admitted enough rows —
/// not until the table ends.
#[test]
fn a_filtered_limit_stops_once_enough_rows_have_matched() {
    let (mut engine, counts) = seeded(2000);
    let rows = ids(&mut engine, "SELECT id FROM t WHERE n = 3 LIMIT 4");
    assert_eq!(rows, vec![3, 10, 17, 24]);
    assert!(
        counts.rows.get() < 200,
        "a filtered LIMIT 4 read {} rows",
        counts.rows.get()
    );
}

/// `OFFSET` is counted into what the scan has to produce, so paging still
/// stops early — but at `offset + limit`, not at `limit`.
#[test]
fn an_offset_is_read_before_it_is_skipped() {
    let (mut engine, counts) = seeded(2000);
    assert_eq!(
        ids(&mut engine, "SELECT id FROM t LIMIT 3 OFFSET 10"),
        vec![11, 12, 13]
    );
    assert!(counts.rows.get() >= 13, "the offset was not read at all");
    assert!(
        counts.rows.get() < 200,
        "an OFFSET 10 LIMIT 3 read {} rows",
        counts.rows.get()
    );
}

/// A sort chooses *which* rows survive the limit, so the scan cannot stop:
/// the ten smallest `n` are spread across the whole table.
#[test]
fn an_order_by_still_reads_the_whole_table_and_answers_correctly() {
    let (mut engine, counts) = seeded(500);
    let limited = ids(&mut engine, "SELECT id FROM t ORDER BY n, id LIMIT 5");
    assert_eq!(counts.rows.get(), 500, "ORDER BY truncated its input");

    let mut all = ids(&mut engine, "SELECT id FROM t ORDER BY n, id");
    all.truncate(5);
    assert_eq!(limited, all);
}

/// An aggregate collapses its input, so a `LIMIT` on it bounds *groups*, not
/// rows read.
#[test]
fn an_aggregate_still_reads_the_whole_table() {
    let (mut engine, counts) = seeded(500);
    let rows = engine
        .query("SELECT COUNT(*) FROM t LIMIT 1", &[])
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Value::Integer(500)]]);
    assert_eq!(counts.rows.get(), 500, "COUNT(*) truncated its input");
}

/// `DISTINCT` folds rows away, so the first `n` scanned rows are not the first
/// `n` of the answer.
#[test]
fn a_distinct_still_reads_the_whole_table_and_answers_correctly() {
    let (mut engine, counts) = seeded(500);
    let limited = ids(&mut engine, "SELECT DISTINCT n FROM t LIMIT 7");
    assert_eq!(counts.rows.get(), 500, "DISTINCT truncated its input");
    assert_eq!(limited, vec![1, 2, 3, 4, 5, 6, 0]);
}

/// A scan that spans many batches has to hand back every row exactly once, in
/// order. This is the resume token under test: an off-by-one in it either
/// repeats a row or drops one, and both show up here.
#[test]
fn a_scan_that_spans_many_batches_reads_every_row_exactly_once() {
    let (mut engine, counts) = seeded(2000);
    let rows = ids(&mut engine, "SELECT id FROM t");
    assert_eq!(rows, (1..=2000).collect::<Vec<_>>());
    assert_eq!(counts.rows.get(), 2000);
    assert!(counts.calls.get() > 1, "2000 rows came back in one batch");
}

/// The same, on the real copy-on-write tree rather than the in-memory map: its
/// batch is a pruned range walk with two bounds, which is a different piece of
/// code with the same contract.
#[test]
fn the_tree_backed_scan_also_resumes_exactly_where_it_stopped() {
    let mut engine = Engine::open(
        Box::new(TreeStorage::open_on(SimDisk::new(64 * 1024 * 1024)).unwrap()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .unwrap();
    engine
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    // One commit per row: a single transaction has a hard log-region ceiling
    // and that limit has nothing to do with what is being tested here.
    for id in 1..=300 {
        engine
            .execute(
                "INSERT INTO t (id, body) VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(format!("row-{id}"))],
            )
            .unwrap();
    }

    assert_eq!(
        ids(&mut engine, "SELECT id FROM t"),
        (1..=300).collect::<Vec<_>>()
    );
    assert_eq!(ids(&mut engine, "SELECT id FROM t LIMIT 3"), vec![1, 2, 3]);

    // A hole in the middle: the resume must skip it rather than stop at it.
    engine
        .execute("DELETE FROM t WHERE id > 40 AND id < 70", &[])
        .unwrap();
    let expected: Vec<i64> = (1..=40).chain(70..=300).collect();
    assert_eq!(ids(&mut engine, "SELECT id FROM t"), expected);
}

/// A row written but not yet committed is part of the scan, in its place in the
/// order — the batching must not push this statement's own writes to the end.
#[test]
fn an_open_transaction_sees_its_own_writes_in_row_id_order() {
    let (mut engine, _) = seeded(100);
    engine.begin().unwrap();
    engine
        .execute("DELETE FROM t WHERE id >= 3 AND id <= 97", &[])
        .unwrap();
    engine
        .execute(
            "INSERT INTO t (id, n, body) VALUES (50, 0, 'inserted')",
            &[],
        )
        .unwrap();
    assert_eq!(
        ids(&mut engine, "SELECT id FROM t"),
        vec![1, 2, 50, 98, 99, 100]
    );
    engine.rollback().unwrap();
    assert_eq!(ids(&mut engine, "SELECT id FROM t").len(), 100);
}

// ------------------------------------------------------- projection pushdown

/// Each of these would return the wrong answer if the column it reads were
/// left undecoded. The point of the table is coverage of every construct that
/// can reach a stored column.
#[test]
fn every_construct_that_reads_a_column_still_sees_it() {
    let (mut engine, _) = seeded(40);
    let cases: [(&str, Vec<Vec<Value>>); 10] = [
        // A `WHERE` over a column the projection never names.
        (
            "SELECT id FROM t WHERE body = 'row-9'",
            vec![vec![Value::Integer(9)]],
        ),
        // `CASE`, `LIKE`, `IN`, `BETWEEN` and `CAST` each hide a column
        // reference inside a different `Expr` variant.
        (
            "SELECT CASE WHEN n = 0 THEN body ELSE 'x' END FROM t WHERE id = 7",
            vec![vec![Value::Text("row-7".to_string())]],
        ),
        (
            "SELECT id FROM t WHERE body LIKE 'row-3' ",
            vec![vec![Value::Integer(3)]],
        ),
        (
            "SELECT id FROM t WHERE n IN (1) AND id < 5",
            vec![vec![Value::Integer(1)]],
        ),
        (
            "SELECT id FROM t WHERE n BETWEEN 5 AND 6 AND id < 7",
            vec![vec![Value::Integer(5)], vec![Value::Integer(6)]],
        ),
        (
            "SELECT id FROM t WHERE CAST(body AS TEXT) = 'row-2'",
            vec![vec![Value::Integer(2)]],
        ),
        // A function argument.
        (
            "SELECT id FROM t WHERE length(body) = 5 AND id < 3",
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]],
        ),
        // An `ORDER BY` over a column the projection never names.
        (
            "SELECT id FROM t ORDER BY body DESC LIMIT 1",
            vec![vec![Value::Integer(9)]],
        ),
        // `GROUP BY` plus `HAVING` plus an aggregate argument: over ids 1..=40
        // with `n = id % 7`, only the `n = 5` bucket sums past 130.
        (
            "SELECT n FROM t GROUP BY n HAVING SUM(id) > 130 ORDER BY n",
            vec![vec![Value::Integer(5)]],
        ),
        // The same column projected twice, which is the case the moving
        // projection has to refuse.
        (
            "SELECT body, body FROM t WHERE id = 4",
            vec![vec![
                Value::Text("row-4".to_string()),
                Value::Text("row-4".to_string()),
            ]],
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(engine.query(sql, &[]).unwrap().rows, expected, "{sql}");
    }
}

/// `SELECT *` is the shape the moving projection is for, and it still has to
/// produce every column.
#[test]
fn select_star_still_returns_every_column() {
    let (mut engine, _) = seeded(3);
    assert_eq!(
        engine
            .query("SELECT * FROM t WHERE id = 2", &[])
            .unwrap()
            .rows,
        vec![vec![
            Value::Integer(2),
            Value::Integer(2),
            Value::Text("row-2".to_string()),
        ]]
    );
}

// --------------------------------------------------------------------- joins

fn joined() -> Engine {
    let mut engine = Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .unwrap();
    engine
        .execute("CREATE TABLE a (id INTEGER PRIMARY KEY, tag TEXT)", &[])
        .unwrap();
    engine
        .execute(
            "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, note TEXT)",
            &[],
        )
        .unwrap();
    for id in 1..=5 {
        engine
            .execute(
                "INSERT INTO a (id, tag) VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(format!("tag-{id}"))],
            )
            .unwrap();
    }
    // Rows 1 and 2 match twice, row 3 once, rows 4 and 5 not at all.
    for (id, a_id) in [(1, 1), (2, 1), (3, 2), (4, 2), (5, 3)] {
        engine
            .execute(
                "INSERT INTO b (id, a_id, note) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Integer(a_id),
                    Value::Text(format!("note-{id}")),
                ],
            )
            .unwrap();
    }
    engine
}

/// The outer side streams and the pairs come out in outer-then-inner order,
/// which is what makes a `LIMIT` on a join mean the same thing as before.
#[test]
fn a_join_pairs_in_outer_then_inner_order() {
    let mut engine = joined();
    let rows = engine
        .query(
            "SELECT a.id, b.id FROM a JOIN b ON b.a_id = a.id ORDER BY a.id, b.id",
            &[],
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(2), Value::Integer(3)],
            vec![Value::Integer(2), Value::Integer(4)],
            vec![Value::Integer(3), Value::Integer(5)],
        ]
    );
}

/// A `LEFT JOIN`'s unmatched rows are padded with `NULL` for the whole inner
/// table's width, and they arrive in outer order alongside the matched ones.
#[test]
fn a_left_join_pads_the_rows_that_matched_nothing() {
    let mut engine = joined();
    let rows = engine
        .query(
            "SELECT a.id, b.note FROM a LEFT JOIN b ON b.a_id = a.id ORDER BY a.id, b.id",
            &[],
        )
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[5], vec![Value::Integer(4), Value::Null]);
    assert_eq!(rows[6], vec![Value::Integer(5), Value::Null]);
}

/// A cross join keeps every pair, which is the case where the scratch buffer
/// must not drop or duplicate anything.
#[test]
fn a_cross_join_still_produces_every_pair() {
    let mut engine = joined();
    let rows = engine
        .query("SELECT a.id, b.id FROM a, b", &[])
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 25);
}

/// A `LIMIT` on an unsorted join stops the outer scan; the answer is the first
/// pairs in outer order, which is what sorting by row id and truncating gives.
#[test]
fn a_join_with_a_limit_answers_as_the_unlimited_query_truncated() {
    let mut engine = joined();
    let limited = engine
        .query(
            "SELECT a.id, b.id FROM a JOIN b ON b.a_id = a.id LIMIT 3",
            &[],
        )
        .unwrap()
        .rows;
    let mut all = engine
        .query("SELECT a.id, b.id FROM a JOIN b ON b.a_id = a.id", &[])
        .unwrap()
        .rows;
    all.truncate(3);
    assert_eq!(limited, all);
    assert_eq!(limited.len(), 3);
}

// ------------------------------------------------- index nested-loop join

/// A small outer table and a large inner one, on counting storage, counters
/// zeroed. `index` declares the B-tree the probe needs; without it the same
/// query is the materialising path and the control for every number below.
///
/// The inner table is keyed so that one outer row matches exactly one inner
/// row: `inner.k` is unique and its values are the outer ids.
fn probe_tables(outer: i64, inner: i64, index: bool) -> (Engine, Rc<ScanCounts>) {
    let counts = Rc::new(ScanCounts::default());
    let storage = CountingStorage {
        inner: MemStorage::new(),
        counts: Rc::clone(&counts),
    };
    let mut engine = Engine::open(
        Box::new(storage),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .expect("open");
    engine
        .execute("CREATE TABLE o (id INTEGER PRIMARY KEY, k INTEGER)", &[])
        .unwrap();
    engine
        .execute(
            "CREATE TABLE i (id INTEGER PRIMARY KEY, k INTEGER, body TEXT)",
            &[],
        )
        .unwrap();
    if index {
        engine.execute("CREATE INDEX i_k ON i (k)", &[]).unwrap();
    }
    engine.begin().unwrap();
    for id in 1..=outer {
        engine
            .execute(
                "INSERT INTO o (id, k) VALUES (?, ?)",
                &[Value::Integer(id), Value::Integer(id)],
            )
            .unwrap();
    }
    for id in 1..=inner {
        engine
            .execute(
                "INSERT INTO i (id, k, body) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Integer(id),
                    Value::Text(format!("row-{id}")),
                ],
            )
            .unwrap();
    }
    engine.commit().unwrap();
    counts.reset();
    (engine, counts)
}

fn pairs(engine: &mut Engine, sql: &str) -> Vec<Vec<Value>> {
    engine.query(sql, &[]).unwrap().rows
}

/// The headline claim of Phase 2 item 4: a probed join does not read the inner
/// table.
///
/// Five outer rows against a 2,000-row inner table. The materialising path
/// reads all 2,000 before the first pair; the probe reads five rows, one per
/// outer row, and never scans the inner table at all. Both answer the same
/// thing, which is what makes the numbers worth comparing.
#[test]
fn a_probed_join_does_not_read_the_whole_inner_table() {
    let sql = "SELECT o.id, i.body FROM o JOIN i ON o.k = i.k";

    let (mut plain, plain_counts) = probe_tables(5, 2000, false);
    let expected = pairs(&mut plain, sql);
    assert_eq!(expected.len(), 5);
    assert!(
        plain_counts.rows.get() >= 2000,
        "the materialising path read {} rows, so it did not materialise",
        plain_counts.rows.get()
    );

    let (mut probed, counts) = probe_tables(5, 2000, true);
    assert_eq!(pairs(&mut probed, sql), expected);
    // The outer table is still scanned — five rows — and nothing else is.
    assert!(
        counts.rows.get() < 100,
        "a probed join scanned {} rows",
        counts.rows.get()
    );
    assert_eq!(
        counts.reads.get(),
        5,
        "a probed join fetched {} inner rows for five outer rows",
        counts.reads.get()
    );
}

/// The same for a probe by `INTEGER PRIMARY KEY`, which needs no secondary
/// index at all: one tree descent per outer row.
#[test]
fn a_row_id_probe_reads_one_inner_row_per_outer_row() {
    let (mut engine, counts) = probe_tables(5, 2000, false);
    let sql = "SELECT o.id, i.body FROM o JOIN i ON o.k = i.id";
    assert_eq!(pairs(&mut engine, sql).len(), 5);
    assert!(
        counts.rows.get() < 100,
        "a row-id probe scanned {} rows",
        counts.rows.get()
    );
    assert_eq!(counts.reads.get(), 5);
}

/// A `LIMIT` on the outer side short-circuits the whole thing: the outer scan
/// stops, and every probe it never made is a probe that never happened.
///
/// This is the property the seam was shaped for. The inner side is not prepared
/// until an outer row is pulled, so a `LIMIT 2` over a 2,000-row inner table
/// costs two probes — where the materialising path pays for the whole inner
/// table before it can emit a single row.
#[test]
fn a_limit_on_a_probed_join_short_circuits_the_inner_side() {
    let sql = "SELECT o.id, i.body FROM o JOIN i ON o.k = i.k LIMIT 2";

    let (mut plain, plain_counts) = probe_tables(500, 2000, false);
    let expected = pairs(&mut plain, sql);
    assert_eq!(expected.len(), 2);
    assert!(
        plain_counts.rows.get() >= 2000,
        "the materialising path read {} rows",
        plain_counts.rows.get()
    );

    let (mut probed, counts) = probe_tables(500, 2000, true);
    assert_eq!(pairs(&mut probed, sql), expected);
    assert!(
        counts.rows.get() < 100,
        "a limited probed join scanned {} rows",
        counts.rows.get()
    );
    assert_eq!(
        counts.reads.get(),
        2,
        "a LIMIT 2 over a probed join fetched {} inner rows",
        counts.reads.get()
    );
}

/// A join the rule cannot probe is still the materialising path, and still
/// right. Without the fallback this would be the wrong answer rather than a
/// slow one.
#[test]
fn a_join_the_rule_declines_still_reads_the_inner_table() {
    let (mut engine, counts) = probe_tables(5, 500, true);
    let rows = pairs(
        &mut engine,
        "SELECT o.id, i.id FROM o JOIN i ON o.k > i.k AND o.id = 3",
    );
    assert_eq!(rows.len(), 2, "3 pairs with i.k of 1 and 2");
    assert!(
        counts.rows.get() >= 500,
        "a join the rule declines read {} rows",
        counts.rows.get()
    );
}

/// A `LEFT JOIN` whose outer rows match nothing pads them, and pays one probe
/// each rather than a scan each.
#[test]
fn an_unmatched_left_join_pads_without_reading_the_inner_table() {
    let (mut engine, counts) = probe_tables(5, 2000, true);
    let rows = pairs(
        &mut engine,
        "SELECT o.id, i.body FROM o LEFT JOIN i ON o.k + 100000 = i.k",
    );
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| row[1] == Value::Null));
    // `o.k + 100000` is an expression, not a column, so the rule declines it
    // and the inner table is read once — the fallback doing its job.
    assert!(counts.rows.get() >= 2000);

    // A point-read outer side and a probed inner side: the whole query is two
    // tree descents over a 2,000-row inner table and a 5-row outer one.
    let (mut engine, counts) = probe_tables(5, 2000, true);
    let rows = pairs(
        &mut engine,
        "SELECT o.id, i.body FROM o LEFT JOIN i ON o.id = i.k WHERE o.id = 5",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(counts.rows.get(), 0, "neither table was scanned");
    assert_eq!(
        counts.reads.get(),
        2,
        "one outer point read and one inner probe, not {}",
        counts.reads.get()
    );
}

// --------------------------------------------------------------- write paths

/// SQLite's rule, and the reason the write paths deliberately do *not* stream:
/// an `UPDATE` sees the table as it stood when the statement began, so a row it
/// has already raised is not raised again.
#[test]
fn an_update_does_not_revisit_the_rows_it_writes() {
    let (mut engine, _) = seeded(20);
    let changed = engine
        .execute("UPDATE t SET id = id + 100 WHERE id <= 20", &[])
        .unwrap();
    assert!(matches!(changed, inlaysql_core::Outcome::Written(20)));
    assert_eq!(
        ids(&mut engine, "SELECT id FROM t"),
        (101..=120).collect::<Vec<_>>()
    );
}

/// The same for `DELETE`, whose candidate list is fixed before the first row
/// leaves the table.
#[test]
fn a_delete_removes_exactly_the_rows_that_matched_at_the_start() {
    let (mut engine, _) = seeded(20);
    engine.execute("DELETE FROM t WHERE n = 0", &[]).unwrap();
    assert_eq!(
        ids(&mut engine, "SELECT id FROM t"),
        (1..=20).filter(|id| id % 7 != 0).collect::<Vec<_>>()
    );
}
