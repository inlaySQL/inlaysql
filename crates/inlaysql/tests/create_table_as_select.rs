//! `CREATE TABLE ... AS SELECT`.
//!
//! Column naming and typing rules — an aliased or bare column keeps its
//! source's declared type, an expression does not, a compound query is
//! untyped — are covered at the plan level in `inlaysql-core`'s
//! `sql::tests` module, verified there against a real sqlite3 binary. These
//! tests are end to end: does the new table actually hold the right rows,
//! in one commit, with none of the source table's constraints.

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

/// A source table exercising every kind of thing `CREATE TABLE ... AS
/// SELECT` must *not* carry over: a `PRIMARY KEY`, a `NOT NULL`, a
/// `DEFAULT`.
fn loaded_src(db: &mut Database) {
    db.execute(
        "CREATE TABLE src (id INTEGER PRIMARY KEY, name TEXT NOT NULL, \
         price REAL, tag TEXT DEFAULT 'x')",
        &[],
    )
    .expect("create src");
    db.execute("INSERT INTO src VALUES (1, 'a', 1.5, 't1')", &[])
        .expect("insert 1");
    db.execute("INSERT INTO src VALUES (2, 'b', 2.5, 't2')", &[])
        .expect("insert 2");
}

/// Verified against a real sqlite3 binary: `CREATE TABLE t AS SELECT * FROM
/// src` copies every value, keeps `id`'s and the other columns' declared
/// types, and carries over none of `src`'s constraints.
#[test]
fn values_and_bare_column_types_round_trip() {
    let (_disk, mut db) = opened();
    loaded_src(&mut db);
    db.execute("CREATE TABLE t AS SELECT * FROM src", &[])
        .expect("create table as select");

    let result = db
        .query("SELECT id, name, price, tag FROM t ORDER BY id", &[])
        .expect("select from t");
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Integer(1),
                Value::Text("a".into()),
                Value::Real(1.5),
                Value::Text("t1".into())
            ],
            vec![
                Value::Integer(2),
                Value::Text("b".into()),
                Value::Real(2.5),
                Value::Text("t2".into())
            ],
        ]
    );

    // `id` kept `src`'s INTEGER type, so a non-integer value is refused.
    assert!(db
        .execute("INSERT INTO t (id, name) VALUES ('x', 'c')", &[])
        .is_err());

    // None of `src`'s constraints did: a second row with `id = 1` is not a
    // PRIMARY KEY violation, a NULL `name` is not a NOT NULL violation, and
    // an omitted `tag` is NULL rather than `src`'s `DEFAULT 'x'`.
    db.execute("INSERT INTO t (id, name) VALUES (1, NULL)", &[])
        .expect("no PRIMARY KEY or NOT NULL survived into t");
    let seeded = db
        .query("SELECT count(*) FROM t WHERE id = 1", &[])
        .expect("count id=1");
    assert_eq!(seeded.rows, vec![vec![Value::Integer(2)]]);
    let tag = db
        .query("SELECT tag FROM t WHERE name IS NULL", &[])
        .expect("query tag");
    assert_eq!(
        tag.rows,
        vec![vec![Value::Null]],
        "no DEFAULT survived into t"
    );
}

/// Verified against a real sqlite3 binary: an existing table is left exactly
/// as it was — the `SELECT` does not run against it at all.
#[test]
fn if_not_exists_leaves_an_existing_table_untouched() {
    let (_disk, mut db) = opened();
    loaded_src(&mut db);
    db.execute("CREATE TABLE t (only_col INTEGER)", &[])
        .expect("create t");
    db.execute("INSERT INTO t VALUES (99)", &[])
        .expect("seed t");

    db.execute("CREATE TABLE IF NOT EXISTS t AS SELECT * FROM src", &[])
        .expect("IF NOT EXISTS is not an error against an existing table");

    let rows = db.query("SELECT * FROM t", &[]).expect("select t");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(99)]],
        "an existing t is left exactly as it was"
    );
}

#[test]
fn duplicate_projected_names_are_refused() {
    let (_disk, mut db) = opened();
    loaded_src(&mut db);
    let err = db
        .execute("CREATE TABLE t AS SELECT id, id FROM src", &[])
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "got {err}");
}

#[test]
fn a_query_with_no_matching_rows_still_creates_the_table() {
    let (_disk, mut db) = opened();
    loaded_src(&mut db);
    db.execute("CREATE TABLE t AS SELECT * FROM src WHERE 0", &[])
        .expect("create table as select with no matching rows");
    let rows = db.query("SELECT count(*) FROM t", &[]).expect("count t");
    assert_eq!(rows.rows, vec![vec![Value::Integer(0)]]);
}

#[test]
fn a_bound_parameter_reaches_the_inner_select() {
    let (_disk, mut db) = opened();
    loaded_src(&mut db);
    db.execute(
        "CREATE TABLE t AS SELECT * FROM src WHERE id = ?",
        &[Value::Integer(2)],
    )
    .expect("create table as select with a parameter");
    let rows = db.query("SELECT name FROM t", &[]).expect("select t");
    assert_eq!(rows.rows, vec![vec![Value::Text("b".into())]]);
}

/// A compound query works too, and duplicates fold the way `UNION` always
/// does.
#[test]
fn a_compound_query_populates_the_new_table_too() {
    let (_disk, mut db) = opened();
    loaded_src(&mut db);
    db.execute(
        "CREATE TABLE t AS SELECT id FROM src UNION SELECT id + 10 FROM src",
        &[],
    )
    .expect("create table as select from a compound query");
    let rows = db
        .query("SELECT id FROM t ORDER BY id", &[])
        .expect("select t");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(11)],
            vec![Value::Integer(12)],
        ]
    );
}

/// The property the whole design turns on: a process that dies between
/// creating the table and populating it must not be possible, because there
/// is no "between" — both happen inside the one commit `end_write` closes.
/// A crash schedule is `docs/recovery.md`'s job to sweep; what this test can
/// cheaply pin is that the statement asks for exactly one durable sync, not
/// two, which is what a crash *could* have split.
#[test]
fn create_and_populate_commit_exactly_once() {
    let (disk, mut db) = opened();
    loaded_src(&mut db);
    let before = disk.borrow().sync_count();
    db.execute("CREATE TABLE t AS SELECT * FROM src", &[])
        .expect("create table as select");
    let after = disk.borrow().sync_count();
    assert_eq!(
        after - before,
        1,
        "the table's declaration and its rows must land in one durable commit, \
         not two a crash between them could split"
    );
}
