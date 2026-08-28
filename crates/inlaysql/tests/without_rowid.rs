//! `CREATE TABLE ... WITHOUT ROWID`.
//!
//! No hidden row id at all: the row is stored under its own primary key's
//! encoded bytes, index-organized, not a rowid table with a mandatory
//! unique index bolted on. Every expectation here was checked against a
//! real sqlite3 3.54 binary first.
//!
//! Two things this does not support yet, disclosed rather than silent —
//! see `Table::without_rowid`'s doc and `Engine::insert_uncommitted_without_rowid`'s:
//! a secondary index (`CREATE INDEX`/a non-key `UNIQUE`) on one of these
//! tables, and joining one against anything else in the same query.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql::{Database, Value};
use inlaysql_core::sim::SimDisk;

const CAPACITY: usize = 16 * 1024 * 1024;

fn opened() -> (Rc<RefCell<SimDisk>>, Database) {
    let disk = Rc::new(RefCell::new(SimDisk::new(CAPACITY)));
    let db = Database::open_on(disk.clone()).expect("open");
    (disk, db)
}

/// Verified against sqlite3: rows come back in primary-key order (an
/// index-organized table's natural scan order), not insertion order.
#[test]
fn rows_are_stored_and_scanned_in_primary_key_order() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER, b TEXT, c REAL, PRIMARY KEY (a, b)) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t VALUES (2, 'y', 2.5)", &[])
        .expect("insert 2");
    db.execute("INSERT INTO t VALUES (1, 'x', 1.5)", &[])
        .expect("insert 1");
    let rows = db.query("SELECT a, b, c FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1), Value::Text("x".into()), Value::Real(1.5)],
            vec![Value::Integer(2), Value::Text("y".into()), Value::Real(2.5)],
        ]
    );
}

/// Verified against sqlite3 ("no such column: rowid"): there is no hidden
/// row id to name. This engine never supported the bare `rowid` pseudo-
/// column on an ordinary table either, so there is nothing new to refuse
/// here — confirming that rather than a behaviour this change had to add.
#[test]
fn there_is_no_rowid_pseudo_column() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY) WITHOUT ROWID", &[])
        .expect("create");
    let error = db.query("SELECT rowid FROM t", &[]).unwrap_err();
    assert!(
        error.to_string().contains("rowid"),
        "expected a no-such-column-rowid error, got: {error}"
    );
}

/// Verified against sqlite3 ("PRIMARY KEY missing on table t"): unlike an
/// ordinary table, every column of which defaults to a hidden row id, one
/// of these needs an explicit key to be stored under at all.
#[test]
fn a_primary_key_is_mandatory() {
    let (_disk, mut db) = opened();
    let error = db
        .execute("CREATE TABLE t (a INTEGER, b TEXT) WITHOUT ROWID", &[])
        .unwrap_err();
    assert!(
        error.to_string().contains("PRIMARY KEY missing"),
        "expected a PRIMARY KEY missing refusal, got: {error}"
    );
}

/// Verified against sqlite3: `AUTOINCREMENT` is refused outright on one of
/// these, not merely ineffective — there is no row id counter for it to
/// advance.
#[test]
fn autoincrement_is_refused() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "CREATE TABLE t (a INTEGER PRIMARY KEY AUTOINCREMENT) WITHOUT ROWID",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("AUTOINCREMENT"),
        "expected an AUTOINCREMENT refusal, got: {error}"
    );
}

/// Verified against sqlite3 ("UNIQUE constraint failed: t.a, t.b"): a
/// duplicate primary key is the one possible conflict on one of these
/// tables, reported the same way an ordinary UNIQUE violation is.
#[test]
fn a_duplicate_primary_key_is_refused() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'x')", &[])
        .expect("insert");
    let error = db
        .execute("INSERT INTO t VALUES (1, 'x')", &[])
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("UNIQUE constraint failed: t.a, t.b"),
        "expected a UNIQUE constraint failure naming both key columns, got: {error}"
    );
}

/// Verified against sqlite3: even a lone `INTEGER PRIMARY KEY` does not
/// become a row id alias on one of these tables — unlike an ordinary
/// table, a `NULL` there is a `NOT NULL` violation, not an auto-assigned
/// key.
#[test]
fn a_lone_integer_primary_key_still_does_not_alias_a_row_id() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    let error = db
        .execute("INSERT INTO t (b) VALUES ('z')", &[])
        .unwrap_err();
    assert!(
        error.to_string().contains("NOT NULL"),
        "expected a NOT NULL refusal, not an auto-assigned key, got: {error}"
    );
}

/// Verified against sqlite3: `INSERT OR IGNORE`/`INSERT OR REPLACE` apply
/// to the primary key the same way they apply to an ordinary table's own
/// unique constraints.
#[test]
fn or_ignore_and_or_replace_apply_to_the_primary_key() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'first')", &[])
        .expect("insert");
    db.execute("INSERT OR IGNORE INTO t VALUES (1, 'second')", &[])
        .expect("ignore");
    assert_eq!(
        db.query("SELECT b FROM t WHERE a = 1", &[])
            .expect("select")
            .rows,
        vec![vec![Value::Text("first".into())]]
    );
    db.execute("INSERT OR REPLACE INTO t VALUES (1, 'third')", &[])
        .expect("replace");
    assert_eq!(
        db.query("SELECT b FROM t WHERE a = 1", &[])
            .expect("select")
            .rows,
        vec![vec![Value::Text("third".into())]]
    );
}

/// Verified against sqlite3: updating a primary-key column moves the row —
/// generalising the single-column rule an ordinary table's own
/// `INTEGER PRIMARY KEY` already follows to however many columns the key
/// has here.
#[test]
fn updating_the_primary_key_moves_the_row() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'x')", &[])
        .expect("insert");
    db.execute("UPDATE t SET a = 9 WHERE a = 1", &[])
        .expect("update");
    assert_eq!(
        db.query("SELECT a, b FROM t", &[]).expect("select").rows,
        vec![vec![Value::Integer(9), Value::Text("x".into())]]
    );
}

/// `DELETE` removes the row by its own key, and a later `SELECT` no longer
/// finds it.
#[test]
fn delete_removes_the_row_by_its_own_key() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'x'), (2, 'y')", &[])
        .expect("insert");
    db.execute("DELETE FROM t WHERE a = 1", &[])
        .expect("delete");
    assert_eq!(
        db.query("SELECT a FROM t", &[]).expect("select").rows,
        vec![vec![Value::Integer(2)]]
    );
}

/// A dropped `WITHOUT ROWID` table's rows do not survive re-creating a
/// table under the same name — the regression this exists to catch is an
/// orphaned row a `RowId`-only `DROP TABLE` failed to reach at all.
#[test]
fn drop_table_removes_every_row() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'x')", &[])
        .expect("insert");
    db.execute("DROP TABLE t", &[]).expect("drop");
    db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)", &[])
        .expect("recreate, ordinary rowid table this time");
    assert_eq!(
        db.query("SELECT * FROM t", &[]).expect("select").rows,
        Vec::<Vec<Value>>::new()
    );
}

/// `STRICT` and `WITHOUT ROWID` combine. Confirmed against sqlite3 that both
/// orders are real syntax there, comma-separated (`... STRICT, WITHOUT
/// ROWID` and `... WITHOUT ROWID, STRICT` both parse on a real sqlite3
/// binary) — but the `sqlparser` dependency this engine's own parser is
/// built on does not accept a comma between the two at all, only this
/// order with none, a pre-existing gap in that dependency's own SQLite
/// dialect and not something this feature's own logic needs to work
/// around.
#[test]
fn strict_and_without_rowid_combine() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID STRICT",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'x')", &[])
        .expect("insert");
    // A `REAL` into a `STRICT TEXT` column is *not* refused — verified
    // against sqlite3, it is stringified the way `CAST(x AS TEXT)` would be
    // — so this checks a combination that really is: a `BLOB`, which has no
    // such conversion.
    let error = db
        .execute("INSERT INTO t VALUES (2, X'0102')", &[])
        .unwrap_err();
    assert!(
        error.to_string().contains("BLOB"),
        "STRICT should still refuse a BLOB for a TEXT column, got: {error}"
    );
}

/// Disclosed, not silent: a secondary index needs a row id to point back
/// with, and this table has none.
#[test]
fn create_index_is_refused() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    let error = db.execute("CREATE INDEX idx ON t (b)", &[]).unwrap_err();
    assert!(
        error.to_string().contains("WITHOUT ROWID"),
        "expected a WITHOUT ROWID refusal naming the reason, got: {error}"
    );
}

/// Same reason, same disclosure: a non-key `UNIQUE` would need a secondary
/// index too.
#[test]
fn a_non_key_unique_constraint_is_refused() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT UNIQUE) WITHOUT ROWID",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("UNIQUE"),
        "expected a UNIQUE-on-WITHOUT-ROWID refusal, got: {error}"
    );
}

/// Disclosed, not silent: every join strategy reads its inner side by row
/// id, which this table's rows are not reachable through.
#[test]
fn joining_a_without_rowid_table_is_refused() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID",
        &[],
    )
    .expect("create t");
    db.execute("CREATE TABLE u (a INTEGER PRIMARY KEY)", &[])
        .expect("create u");
    let error = db
        .query("SELECT * FROM t JOIN u ON t.a = u.a", &[])
        .unwrap_err();
    assert!(
        error.to_string().contains("WITHOUT ROWID"),
        "expected a WITHOUT-ROWID-join refusal, got: {error}"
    );
}

/// `RETURNING` on `INSERT`/`UPDATE`/`DELETE` all work, projecting the row
/// exactly as they do for an ordinary table.
#[test]
fn returning_works_on_insert_update_and_delete() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    let inserted = db
        .query("INSERT INTO t VALUES (1, 'x') RETURNING a, b", &[])
        .expect("insert returning");
    assert_eq!(
        inserted.rows,
        vec![vec![Value::Integer(1), Value::Text("x".into())]]
    );
    let updated = db
        .query("UPDATE t SET b = 'y' WHERE a = 1 RETURNING b", &[])
        .expect("update returning");
    assert_eq!(updated.rows, vec![vec![Value::Text("y".into())]]);
    let deleted = db
        .query("DELETE FROM t WHERE a = 1 RETURNING b", &[])
        .expect("delete returning");
    assert_eq!(deleted.rows, vec![vec![Value::Text("y".into())]]);
}

/// A whole-table aggregate (`COUNT(*)`) reads through the same stream a
/// plain `SELECT` does.
#[test]
fn aggregates_over_a_without_rowid_table_work() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY) WITHOUT ROWID", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (1), (2), (3)", &[])
        .expect("insert");
    assert_eq!(
        db.query("SELECT count(*), sum(a) FROM t", &[])
            .expect("select")
            .rows,
        vec![vec![Value::Integer(3), Value::Integer(6)]]
    );
}
