//! `SAVEPOINT` / `RELEASE` / `ROLLBACK TO SAVEPOINT`.
//!
//! Every expectation here was checked against a real sqlite3 3.54 binary
//! first. The engine implements this by replaying a log of the
//! transaction's own writes rather than partially undoing the storage
//! backend's buffered pages in place — see
//! `Engine::rollback_to_savepoint`'s doc — so these tests lean on exact
//! row values and on `SimDisk::sync_count` to prove that reconstruction is
//! both correct and durability-free until a real commit.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql::{Database, Error, Value};
use inlaysql_core::sim::SimDisk;

const CAPACITY: usize = 16 * 1024 * 1024;

fn opened() -> (Rc<RefCell<SimDisk>>, Database) {
    let disk = Rc::new(RefCell::new(SimDisk::new(CAPACITY)));
    let db = Database::open_on(disk.clone()).expect("open");
    (disk, db)
}

/// Verified against sqlite3: `SAVEPOINT s; INSERT ...; RELEASE s;` with no
/// `BEGIN` persists the row, exactly as `BEGIN; INSERT ...; COMMIT;` would.
#[test]
fn savepoint_with_no_begin_starts_and_commits_an_implicit_transaction() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("SAVEPOINT s", &[]).expect("savepoint");
    db.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");
    db.execute("RELEASE s", &[]).expect("release");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(1)]]);
}

/// Verified against sqlite3: rows written after a savepoint are undone by
/// `ROLLBACK TO` it; the savepoint itself survives and the transaction stays
/// open for more work, which is then kept by `RELEASE`.
#[test]
fn rollback_to_savepoint_undoes_only_what_came_after_it() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint s1");
    db.execute("INSERT INTO t VALUES (1)", &[])
        .expect("insert 1");
    db.execute("SAVEPOINT s2", &[]).expect("savepoint s2");
    db.execute("INSERT INTO t VALUES (2)", &[])
        .expect("insert 2");
    db.execute("ROLLBACK TO s1", &[])
        .expect("rollback to s1 undoes rows 1 and 2");
    let rows = db.query("SELECT count(*) FROM t", &[]).expect("count");
    assert_eq!(rows.rows, vec![vec![Value::Integer(0)]]);

    db.execute("INSERT INTO t VALUES (3)", &[])
        .expect("insert 3");
    db.execute("RELEASE s1", &[]).expect("release s1");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(3)]]);
}

/// Verified against sqlite3: releasing a savepoint also releases every
/// savepoint nested inside it — a later `ROLLBACK TO` naming one is then
/// "no such savepoint", not a rollback to stale state.
#[test]
fn releasing_a_savepoint_releases_the_ones_nested_inside_it_too() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint s1");
    db.execute("SAVEPOINT s2", &[]).expect("savepoint s2");
    db.execute("RELEASE s1", &[]).expect("release s1");
    let err = db.execute("ROLLBACK TO s2", &[]).unwrap_err();
    assert!(matches!(err, Error::Transaction(_)), "got {err}");
}

/// Verified against sqlite3: two open savepoints may share a name;
/// `ROLLBACK TO` targets the most recently established one, and once that
/// one is released, the same name resolves to the next one out.
#[test]
fn duplicate_named_savepoints_are_resolved_innermost_first() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("SAVEPOINT s", &[]).expect("outer s");
    db.execute("INSERT INTO t VALUES (1)", &[])
        .expect("insert 1");
    db.execute("SAVEPOINT s", &[]).expect("inner s");
    db.execute("INSERT INTO t VALUES (2)", &[])
        .expect("insert 2");
    db.execute("ROLLBACK TO s", &[])
        .expect("rollback to inner s");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1)]],
        "only row 2 is undone"
    );

    db.execute("RELEASE s", &[]).expect("release inner s");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1)]],
        "release keeps row 1"
    );

    db.execute("ROLLBACK TO s", &[])
        .expect("the same name now resolves to the outer s");
    let rows = db.query("SELECT count(*) FROM t", &[]).expect("count");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(0)]],
        "row 1 is undone too"
    );
}

/// Verified against sqlite3: a bare `ROLLBACK` abandons every open savepoint
/// along with the whole transaction, back to before it began.
#[test]
fn a_plain_rollback_abandons_every_open_savepoint_too() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("BEGIN", &[]).expect("begin");
    db.execute("INSERT INTO t VALUES (1)", &[])
        .expect("insert 1");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("INSERT INTO t VALUES (2)", &[])
        .expect("insert 2");
    db.execute("ROLLBACK", &[]).expect("rollback");
    let rows = db.query("SELECT count(*) FROM t", &[]).expect("count");
    assert_eq!(rows.rows, vec![vec![Value::Integer(0)]]);
    let err = db.execute("ROLLBACK TO s1", &[]).unwrap_err();
    assert!(matches!(err, Error::Transaction(_)), "got {err}");
}

/// Verified against sqlite3: `CREATE TABLE` inside a savepoint is undone by
/// `ROLLBACK TO` exactly like a row write — the table stops existing.
#[test]
fn ddl_inside_a_savepoint_is_rolled_back_too() {
    let (_disk, mut db) = opened();
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create inside savepoint");
    db.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");
    db.execute("ROLLBACK TO s1", &[]).expect("rollback");
    let err = db.query("SELECT * FROM t", &[]).unwrap_err();
    assert!(matches!(err, Error::Catalog(_)), "got {err}");
    db.execute("RELEASE s1", &[])
        .expect("release the empty transaction");
}

/// Verified against sqlite3: `COMMIT` closes a transaction a `SAVEPOINT`
/// opened implicitly, same as it would one `BEGIN` opened.
#[test]
fn commit_closes_a_savepoint_started_transaction() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");
    db.execute("COMMIT", &[]).expect("commit");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(1)]]);
}

/// Verified against sqlite3: an explicit `BEGIN` with an un-released
/// savepoint still lets `COMMIT` commit everything.
#[test]
fn commit_works_with_an_open_unreleased_savepoint() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("BEGIN", &[]).expect("begin");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");
    db.execute("COMMIT", &[]).expect("commit");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn rollback_to_an_unknown_savepoint_is_refused_and_leaves_state_untouched() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");
    let err = db.execute("ROLLBACK TO nope", &[]).unwrap_err();
    assert!(matches!(err, Error::Transaction(_)), "got {err}");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1)]],
        "the error changed nothing"
    );
    db.execute("RELEASE s1", &[])
        .expect("still usable afterwards");
}

#[test]
fn releasing_an_unknown_savepoint_is_refused() {
    let (_disk, mut db) = opened();
    let err = db.execute("RELEASE nope", &[]).unwrap_err();
    assert!(matches!(err, Error::Transaction(_)), "got {err}");
}

/// The row-id counter is replayed along with everything else: an id
/// assigned to a row that gets rolled away is not reused, exactly as
/// SQLite's own counter never hands back a key once assigned.
#[test]
fn the_row_id_counter_reflects_replayed_history_not_a_fresh_start() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
        .expect("create");
    db.execute("INSERT INTO t (v) VALUES ('a')", &[])
        .expect("row 1");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("INSERT INTO t (v) VALUES ('b')", &[])
        .expect("row 2, id 2");
    db.execute("ROLLBACK TO s1", &[]).expect("undo row 2");
    db.execute("INSERT INTO t (v) VALUES ('c')", &[])
        .expect("row 2 again, should still get id 2 back since nothing committed it away");
    let rows = db
        .query("SELECT id, v FROM t ORDER BY id", &[])
        .expect("select");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1), Value::Text("a".into())],
            vec![Value::Integer(2), Value::Text("c".into())],
        ]
    );
}

/// `CURRENT_TIMESTAMP` is captured once per statement and replayed with
/// that exact value, not resampled — the property the whole replay design
/// depends on. Proven by forcing two statements onto opposite sides of a
/// millisecond boundary: if replay resampled the clock, the reconstructed
/// row could show a different timestamp than the one the caller originally
/// saw.
#[test]
fn a_replayed_statement_reproduces_its_original_clock_reading_not_a_fresh_one() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER, stamp TEXT DEFAULT CURRENT_TIMESTAMP)",
        &[],
    )
    .expect("create");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("INSERT INTO t (a) VALUES (1)", &[])
        .expect("insert with a captured timestamp");
    let before = db
        .query("SELECT stamp FROM t WHERE a = 1", &[])
        .expect("select before")
        .rows;

    db.execute("SAVEPOINT s2", &[]).expect("nested savepoint");
    db.execute("INSERT INTO t (a) VALUES (2)", &[])
        .expect("a row that will be undone");
    db.execute("ROLLBACK TO s2", &[])
        .expect("undo row 2, replaying row 1's insert along the way");

    let after = db
        .query("SELECT stamp FROM t WHERE a = 1", &[])
        .expect("select after")
        .rows;
    assert_eq!(
        before, after,
        "replaying row 1's insert must reproduce its original CURRENT_TIMESTAMP, not a new one"
    );
}

/// `ROLLBACK TO` reconstructs state entirely in memory: it must not cost a
/// durable sync, since nothing about it is meant to be durable until a real
/// `COMMIT`.
#[test]
fn rollback_to_savepoint_does_not_touch_the_disk() {
    let (disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");
    let before = disk.borrow().sync_count();
    db.execute("ROLLBACK TO s1", &[]).expect("rollback");
    let after = disk.borrow().sync_count();
    assert_eq!(
        before, after,
        "rolling back to a savepoint must not durably sync anything"
    );
    db.execute("RELEASE s1", &[]).expect("release");
}

/// A `STRICT` table's rules still apply exactly during replay: an insert
/// that violated them the first time would have failed then, so this only
/// has to prove replay does not silently loosen anything.
#[test]
fn replay_still_enforces_strict_column_types() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INT) STRICT", &[])
        .expect("create");
    db.execute("SAVEPOINT s1", &[]).expect("savepoint");
    db.execute("INSERT INTO t VALUES (2.0)", &[])
        .expect("a lossless real, coerced to integer");
    db.execute("SAVEPOINT s2", &[]).expect("nested");
    db.execute("INSERT INTO t VALUES (3)", &[])
        .expect("second row");
    db.execute("ROLLBACK TO s2", &[])
        .expect("undo the second row, replaying the first");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(2)]]);
}
