//! The ~1 MiB ceiling on one statement, pinned: `DELETE FROM t`,
//! `UPDATE t SET ...` and `INSERT INTO t SELECT ... FROM t` are hard errors on
//! a large table, not slow paths.
//!
//! This is `docs/enterprise-readiness.md` blocker 5, and these tests are what
//! moved it from *reported* to *verified*. Nothing here lifts the ceiling —
//! see that entry, and `docs/recovery.md`'s "What lifting the one-region
//! ceiling would take", for why the fix is a format change with its own
//! deterministic-simulation proof and not something to land beside its own
//! verification.
//!
//! # What the bound actually is
//!
//! Not a row count, which is how the limit was previously written down. One
//! commit record must fit one WAL region — `WAL_BLOCKS` (256) ×
//! `DEFAULT_PAGE_SIZE` (4096) = 1 MiB — and the record carries **a copy of
//! every page the commit wrote** (`crates/inlaysql-core/src/wal.rs`), because a
//! record that cannot rebuild its own pages is not a commit under the torn-write
//! model `docs/recovery.md` describes. So the quantity that has to fit is the
//! transaction's copy-on-write dirty set in bytes, and the row count that
//! corresponds to depends on how wide the rows are and how much of the tree the
//! statement touches. Measured on a two-column table, one durable commit,
//! nothing else running (`the_row_counts_where_each_statement_breaks` below):
//!
//! | statement | 8-byte bodies | 512-byte bodies |
//! | --- | --- | --- |
//! | `UPDATE t SET body = 'x'` | 17,000 ok / 17,500 refused | 1,687 ok / 1,750 refused |
//! | `INSERT INTO t (body) SELECT body FROM t` | 16,500 ok / 17,000 refused | 1,687 ok / 1,750 refused |
//! | `DELETE FROM t` | 68,750 ok / 70,625 refused | 3,000 ok |
//! | buffered `INSERT`s inside `BEGIN`..`COMMIT` | refused at 11,340 | refused at 884 |
//!
//! `UPDATE` and `INSERT ... SELECT` scale with the bytes they write, as
//! expected — a 64× wider row costs a 10× lower ceiling. **`DELETE` does
//! not**, and that is the one genuinely surprising result: it survives a
//! 512-byte × 3,000-row table an `UPDATE` cannot touch at 1,750, and it fails
//! at a row count that barely moves with row width at all.
//! The reason is that a whole-table delete costs almost nothing in *pages* —
//! `CowBTree::supersede` drops a page from the dirty set when the transaction
//! replaces it again, and a delete that empties a leaf drops it entirely, so
//! the tree collapses out of the record as fast as it is walked — while it
//! costs one change-log entry per row. `crates/inlaysql-core/src/cdc.rs`
//! writes one record per *statement* holding `(table name, row id, kind)` per
//! *row*, and repeats the table name in every entry. So the binding term in
//! `DELETE FROM t` is a change-log record the caller never asked for, and
//! `a_whole_table_delete_is_bounded_by_its_change_log_record` proves it by
//! moving the threshold with nothing but the length of the table's name.
//!
//! # Why a refusal is the acceptable state, and what has to hold for that
//!
//! A statement that cannot be expressed is a blocker. A statement that reports
//! success having applied half of itself is a data-loss bug, and it is strictly
//! worse. Every test here therefore asserts the second half as hard as the
//! first: the refusal arrives, *and* the table is byte-for-byte what it was,
//! *and* the handle still works afterwards. That last clause is not free — see
//! `a_commit_refused_for_size_leaves_a_usable_handle`, which is a regression
//! test for a real bug this verification pass found.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql::{Database, Error, Outcome, Value};
use inlaysql_core::sim::SimDisk;

/// Copy-on-write never reclaims a page here (`page_reuse` is off by default),
/// so the file grows with the number of writes rather than the amount of
/// data. Kept modest on purpose: `SimDisk::sync` clones this whole buffer
/// into a 16-entry fault-injection history on every commit
/// (`inlaysql-core/src/sim/disk.rs`), so a gigabyte-scale capacity here costs
/// gigabytes per commit, and cargo runs this file's five tests concurrently —
/// that combination is what OOM-killed CI rather than merely running slowly.
const CAPACITY: usize = 64 * 1024 * 1024;

/// Headroom for `the_row_counts_where_each_statement_breaks` below, which
/// deliberately pushes into the tens of thousands of rows and is `#[ignore]`d
/// for exactly that reason.
const REPRO_CAPACITY: usize = 256 * 1024 * 1024;

/// Rows per loading transaction. Kept well under the refusal threshold the
/// table above records, because the loader is not what these tests are about —
/// it is a bulk `INSERT` that has already had to be written as batches for
/// exactly the reason the tests then demonstrate.
fn batch_for(width: usize, name_len: usize) -> usize {
    (150_000 / (width + name_len).max(16)).max(1)
}

/// A table of `rows` rows whose `body` is about `width` bytes wide.
fn loaded(name: &str, rows: usize, width: usize) -> Database {
    loaded_with_capacity(name, rows, width, CAPACITY)
}

/// As `loaded`, but with an explicit disk capacity for the bisecting test,
/// which needs more headroom than the default tests do.
fn loaded_with_capacity(name: &str, rows: usize, width: usize, capacity: usize) -> Database {
    let disk = Rc::new(RefCell::new(SimDisk::new(capacity)));
    let mut db = Database::open_on(disk).expect("open");
    db.execute(
        &format!("CREATE TABLE {name} (id INTEGER PRIMARY KEY, body TEXT)"),
        &[],
    )
    .expect("create");
    let insert = db
        .prepare(&format!("INSERT INTO {name} (id, body) VALUES (?, ?)"))
        .expect("prepare");
    let batch = batch_for(width, name.len());
    let mut id = 1;
    while id <= rows {
        db.begin().expect("begin");
        for _ in 0..batch {
            if id > rows {
                break;
            }
            let body = "x".repeat(width);
            db.execute_prepared(
                &insert,
                &[Value::Integer(id as i64), Value::Text(body.into())],
            )
            .unwrap_or_else(|error| panic!("insert {id}: {error}"));
            id += 1;
        }
        db.commit().expect("commit a loading batch");
    }
    db
}

fn count(db: &mut Database, name: &str) -> i64 {
    match db.query(&format!("SELECT COUNT(*) FROM {name}"), &[]) {
        Ok(result) => match result.rows.first().and_then(|row| row.first()) {
            Some(Value::Integer(n)) => *n,
            other => panic!("COUNT(*) returned {other:?}"),
        },
        Err(error) => panic!("COUNT(*) failed after a refused statement: {error}"),
    }
}

/// The refusal every one of these produces: raised by the storage backend at
/// commit, after the statement has run and buffered everything it wanted to
/// write. `Error::Transaction` is the *other* refusal — see
/// `an_explicit_transaction_is_refused_before_the_statement_runs`.
#[track_caller]
fn assert_refused_for_size(result: Result<Outcome, Error>) {
    match result {
        Err(Error::Storage(message)) => assert!(
            message.contains("does not fit the write-ahead log"),
            "refused, but not for size: {message}"
        ),
        Err(other) => panic!("refused for the wrong reason: {other}"),
        Ok(outcome) => panic!("expected a refusal, got {outcome:?}"),
    }
}

/// `UPDATE t SET ...` with no `WHERE`. Every row is rewritten, so every leaf of
/// the table is copied into the commit record.
#[test]
fn a_wide_update_is_refused_rather_than_half_applied() {
    let mut db = loaded("t", 2_000, 512);
    assert_refused_for_size(db.execute("UPDATE t SET body = 'replaced'", &[]));

    // The whole point: the statement did not happen. Not some of it.
    assert_eq!(count(&mut db, "t"), 2_000);
    let replaced = db
        .query("SELECT COUNT(*) FROM t WHERE body = 'replaced'", &[])
        .expect("the handle still answers after a refusal");
    assert_eq!(replaced.rows, vec![vec![Value::Integer(0)]]);
}

/// `INSERT INTO t SELECT ... FROM t` — the bulk copy. It writes as many rows as
/// it reads, so its record is the whole of the new data.
#[test]
fn a_bulk_insert_select_is_refused_rather_than_half_applied() {
    let mut db = loaded("t", 2_000, 512);
    assert_refused_for_size(db.execute("INSERT INTO t (body) SELECT body FROM t", &[]));
    assert_eq!(count(&mut db, "t"), 2_000);
}

/// `DELETE FROM t` with no `WHERE`, and the reason it breaks.
///
/// Two runs of the same statement over the same number of identical rows,
/// differing in nothing but how many characters the table is named. The long
/// name is refused and the short one is not, which is only possible if the
/// binding term is the change-log record — the one structure in the commit
/// whose size depends on the table's *name*, because
/// `cdc::encode_record` repeats it once per changed row.
#[test]
fn a_whole_table_delete_is_bounded_by_its_change_log_record() {
    const ROWS: usize = 20_000;
    let long = "deliveries_archive_2026_partition_qxz_shard_seventeen_backfill".to_string();
    assert!(long.len() > 60);

    let mut db = loaded(&long, ROWS, 8);
    assert_refused_for_size(db.execute(&format!("DELETE FROM {long}"), &[]));
    assert_eq!(count(&mut db, &long), ROWS as i64);

    // Same rows, same statement, a one-character name — and it commits. The
    // rows the delete removes are nearly free: `CowBTree::supersede` drops each
    // emptied leaf out of the dirty set, so the tree collapses out of the
    // record rather than into it.
    let mut db = loaded("t", ROWS, 8);
    assert!(matches!(
        db.execute("DELETE FROM t", &[]),
        Ok(Outcome::Written(ROWS))
    ));
    assert_eq!(count(&mut db, "t"), 0);
}

/// Inside `BEGIN`..`COMMIT` the refusal arrives *before* the statement runs,
/// which is a different and better error than the one above: nothing was
/// buffered, so the caller can commit what it has and carry on.
///
/// `Engine::ensure_transaction_fits` checks at half the region, not at the
/// region, and the margin is load-bearing rather than cautious: the statement
/// that follows a `false` answer can still copy a whole root-to-leaf path per
/// write before anyone looks again.
#[test]
fn an_explicit_transaction_is_refused_before_the_statement_runs() {
    let mut db = loaded("t", 0, 8);
    db.begin().expect("begin");
    let insert = db
        .prepare("INSERT INTO t (id, body) VALUES (?, ?)")
        .expect("prepare");

    let mut written = 0i64;
    let refused = loop {
        written += 1;
        let body = "x".repeat(512);
        match db.execute_prepared(
            &insert,
            &[Value::Integer(written), Value::Text(body.into())],
        ) {
            Ok(_) => assert!(written < 50_000, "no refusal in a whole region's worth"),
            Err(error) => break error,
        }
    };
    match &refused {
        Error::Transaction(message) => assert!(
            message.contains("too large for the write-ahead log"),
            "refused, but not for size: {message}"
        ),
        other => panic!("refused for the wrong reason: {other}"),
    }

    // The refusal is the whole of what happened: the rows already buffered are
    // still buffered, and committing them is still allowed. A caller draining a
    // large import in batches depends on exactly this.
    db.commit().expect("what was buffered still commits");
    assert_eq!(count(&mut db, "t"), written - 1);
}

/// A `COMMIT` refused for size has to leave a handle the caller can
/// keep using, and it did not.
///
/// `Engine::commit` marked the transaction over and returned the error while
/// the storage backend still held the entire write set. The two visible
/// consequences are asserted below — `rollback` refusing because the
/// transaction it would discard is already "over", and the *next* statement
/// failing with a size it did not cause — but the one that mattered is
/// invisible here because this particular error happens to be permanent: those
/// abandoned writes were still queued to be made durable by whatever committed
/// next. A transient failure in the same place would have committed them
/// silently, at a moment nobody chose, which is the exact failure
/// `Engine::discard_failed_statement` was written to prevent for the
/// autocommit path. The explicit-`COMMIT` path never reached it, because
/// `Plan::is_read_only` answers `true` for `Plan::Commit`.
///
/// Both spellings are checked. They are the same code path now; they were the
/// same broken code path before, which is why fixing `Engine::commit` rather
/// than the `is_read_only` predicate was the right lever.
#[test]
fn a_commit_refused_for_size_leaves_a_usable_handle() {
    for sql_spelling in [false, true] {
        let mut db = loaded("t", 2_000, 512);

        if sql_spelling {
            db.execute("BEGIN", &[]).expect("begin");
        } else {
            db.begin().expect("begin");
        }
        // Under the half-region margin when it starts, over the whole region by
        // the time it ends: this is the shape that can only fail at `COMMIT`.
        db.execute("UPDATE t SET body = 'replaced'", &[])
            .expect("the statement itself buffers fine");

        let committed = if sql_spelling {
            db.execute("COMMIT", &[]).map(|_| ())
        } else {
            db.commit()
        };
        assert_refused_for_size(committed.map(|()| Outcome::Ddl));

        // Nothing was applied, and the write set is gone rather than lying in
        // wait for the next commit.
        assert_eq!(count(&mut db, "t"), 2_000);
        let replaced = db
            .query("SELECT COUNT(*) FROM t WHERE body = 'replaced'", &[])
            .expect("query after a refused commit");
        assert_eq!(replaced.rows, vec![vec![Value::Integer(0)]]);

        // An ordinary small write now succeeds, and commits *only itself*.
        db.execute(
            "INSERT INTO t (id, body) VALUES (?, ?)",
            &[Value::Integer(999_999), Value::Text("small".into())],
        )
        .expect("a small write after a refused commit");
        assert_eq!(count(&mut db, "t"), 2_001);
        let replaced = db
            .query("SELECT COUNT(*) FROM t WHERE body = 'replaced'", &[])
            .expect("query after the small write");
        assert_eq!(
            replaced.rows,
            vec![vec![Value::Integer(0)]],
            "the abandoned write set was made durable by the next commit"
        );
    }
}

/// The numbers in this file's table, rerun.
///
/// Ignored by default: it bisects each threshold, which means loading and
/// throwing away a few dozen databases of up to seventy thousand rows. It is
/// how the table above is regenerated, and it is the answer to "has the ceiling
/// moved?" after any change to the record layout, the change log, or what a
/// write dirties.
///
/// ```sh
/// cargo test --release -p inlaysql --test large_statements -- --ignored --nocapture
/// ```
#[test]
#[ignore = "bisects three thresholds over ~70k-row databases; run explicitly"]
fn the_row_counts_where_each_statement_breaks() {
    fn survives(rows: usize, width: usize, sql: &str) -> bool {
        let mut db = loaded_with_capacity("t", rows, width, REPRO_CAPACITY);
        let survived = db.execute(sql, &[]).is_ok();
        if !survived {
            assert_eq!(
                count(&mut db, "t"),
                rows as i64,
                "a refusal applied part of {sql}"
            );
        }
        survived
    }

    fn bisect(mut lo: usize, mut hi: usize, width: usize, sql: &str) {
        assert!(survives(lo, width, sql), "{sql} already fails at {lo}");
        assert!(!survives(hi, width, sql), "{sql} still works at {hi}");
        while hi - lo > lo / 20 + 1 {
            let mid = (lo + hi) / 2;
            if survives(mid, width, sql) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        println!("{width:>4}-byte rows | {sql:<42} | last ok {lo}, first refused {hi}");
    }

    /// Where `Storage::transaction_is_nearly_full` stops a caller buffering
    /// rows into one explicit transaction. A different — and better — refusal:
    /// it arrives at the *start* of a statement, at half the region, so nothing
    /// was written and what is already buffered still commits.
    fn buffered_insert_refusal(width: usize) -> i64 {
        let mut db = loaded_with_capacity("t", 0, width, REPRO_CAPACITY);
        db.begin().expect("begin");
        let insert = db
            .prepare("INSERT INTO t (id, body) VALUES (?, ?)")
            .expect("prepare");
        let mut written = 0i64;
        loop {
            written += 1;
            let body = "x".repeat(width);
            if db
                .execute_prepared(
                    &insert,
                    &[Value::Integer(written), Value::Text(body.into())],
                )
                .is_err()
            {
                return written;
            }
            assert!(written < 200_000, "no refusal in a whole region's worth");
        }
    }

    bisect(4_000, 20_000, 8, "UPDATE t SET body = 'x'");
    bisect(4_000, 20_000, 8, "INSERT INTO t (body) SELECT body FROM t");
    bisect(20_000, 80_000, 8, "DELETE FROM t");
    bisect(1_000, 2_000, 512, "UPDATE t SET body = 'x'");
    bisect(1_000, 2_000, 512, "INSERT INTO t (body) SELECT body FROM t");
    // `DELETE` is the odd one out: it barely notices row width, because what
    // bounds it is the change-log record rather than the rows. Not bisected —
    // a 512-byte × 70,000-row database is 36 MB of load for a cell whose
    // *point* is that it does not move.
    assert!(
        survives(3_000, 512, "DELETE FROM t"),
        "DELETE no longer survives a table an UPDATE cannot touch"
    );
    println!(" 512-byte rows | {:<42} | 3000 ok", "DELETE FROM t");

    for width in [8usize, 512] {
        println!(
            "{width:>4}-byte rows | {:<42} | refused at {}",
            "buffered INSERTs inside BEGIN..COMMIT",
            buffered_insert_refusal(width)
        );
    }
}
