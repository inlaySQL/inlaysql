//! `CREATE TEMPORARY TABLE` (and `CREATE TEMP TABLE`).
//!
//! An ordinary, row-id-keyed table in every way except where it lives: never
//! durable, never visible to another handle open on the same file, gone the
//! moment this one closes — the same as sqlite3's own `TEMP` schema. Storage
//! is routed by table name to an in-memory backend
//! (`inlaysql_core::temp_storage::TempTableRouter`), which is what makes
//! joining one against a durable table work with no special-casing at all,
//! unlike `WITHOUT ROWID`'s join refusal (`without_rowid.rs`).
//!
//! Disclosed rather than silent gaps — see `Table::temporary`'s doc: a
//! secondary index (`CREATE INDEX`/a non-key `UNIQUE`) on one of these
//! tables, `ALTER TABLE` on one, and creating or dropping one inside an
//! explicit transaction (row-level writes to an already-existing one are
//! unaffected). Every expectation here was checked against a real sqlite3
//! 3.54 binary first.

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

#[test]
fn a_temp_table_is_created_written_and_read() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER, b TEXT)", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'x'), (2, 'y')", &[])
        .expect("insert");
    let rows = db
        .query("SELECT a, b FROM t ORDER BY a", &[])
        .expect("select");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1), Value::Text("x".into())],
            vec![Value::Integer(2), Value::Text("y".into())],
        ]
    );
}

/// `CREATE TEMP TABLE` is the same statement under sqlite3's other spelling.
#[test]
fn temp_is_accepted_as_well_as_temporary() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TEMP TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(1)]]);
}

/// Verified against sqlite3: an unqualified `t` resolves to the temporary
/// one whenever both exist, and creating the temporary one is not blocked by
/// a durable table of the same name already existing.
#[test]
fn a_temp_table_shadows_a_durable_table_of_the_same_name() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create durable");
    db.execute("INSERT INTO t VALUES (99)", &[])
        .expect("insert durable");
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create temp of the same name");
    // The temporary one is empty and shadows the durable one, which still
    // has its row — confirmed against sqlite3.
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, Vec::<Vec<Value>>::new());
    db.execute("DROP TABLE t", &[])
        .expect("drop drops the shadowing temp table, not the durable one");
    let rows = db.query("SELECT a FROM t", &[]).expect("select after drop");
    assert_eq!(rows.rows, vec![vec![Value::Integer(99)]]);
}

/// Verified against sqlite3: `CREATE TEMP TABLE IF NOT EXISTS t` still
/// creates a temporary `t` when only a durable one exists — the two are not
/// the same name for this purpose, the same as the shadowing test above.
#[test]
fn if_not_exists_checks_the_schema_the_statement_targets() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create durable");
    db.execute("INSERT INTO t VALUES (99)", &[])
        .expect("insert durable");
    db.execute("CREATE TEMPORARY TABLE IF NOT EXISTS t (a INTEGER)", &[])
        .expect("a durable t is no obstacle to creating a temporary one");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        Vec::<Vec<Value>>::new(),
        "the newly created (empty) temporary t shadows the durable one"
    );
}

/// A temporary table belongs to the handle that created it — confirmed
/// against sqlite3, where a second connection to the same file has no idea
/// it exists at all.
#[test]
fn a_temp_table_is_invisible_to_another_handle() {
    let (disk, mut a) = opened();
    a.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create on handle a");
    a.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");

    let mut b = Database::open_on(disk.clone()).expect("open handle b");
    let error = b.query("SELECT a FROM t", &[]).unwrap_err();
    assert!(
        error.to_string().contains("no such table"),
        "expected a no-such-table error on the second handle, got: {error}"
    );
}

/// A temporary table is never durable: it does not survive the handle that
/// created it closing and the file being reopened.
#[test]
fn a_temp_table_does_not_survive_reopening_the_file() {
    let disk = Rc::new(RefCell::new(SimDisk::new(CAPACITY)));
    {
        let mut db = Database::open_on(disk.clone()).expect("open");
        db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
            .expect("create");
        db.execute("INSERT INTO t VALUES (1)", &[]).expect("insert");
    }
    let mut reopened = Database::open_on(disk.clone()).expect("reopen");
    let error = reopened.query("SELECT a FROM t", &[]).unwrap_err();
    assert!(
        error.to_string().contains("no such table"),
        "expected a no-such-table error after reopening, got: {error}"
    );
}

/// The one gap `WITHOUT ROWID` has that this feature does not: a temporary
/// table's rows are ordinary, row-id-keyed rows reachable through the same
/// `Storage::get_row`/`scan_batch` every join strategy already uses, so
/// nothing has to refuse joining one — confirmed against sqlite3.
#[test]
fn joining_a_temp_table_with_a_durable_table_works() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE main_t (id INTEGER, v TEXT)", &[])
        .expect("create durable");
    db.execute("INSERT INTO main_t VALUES (1, 'durable')", &[])
        .expect("insert durable");
    db.execute("CREATE TEMPORARY TABLE temp_t (id INTEGER, v TEXT)", &[])
        .expect("create temp");
    db.execute("INSERT INTO temp_t VALUES (1, 'temp')", &[])
        .expect("insert temp");
    let rows = db
        .query(
            "SELECT main_t.v, temp_t.v FROM main_t JOIN temp_t ON main_t.id = temp_t.id",
            &[],
        )
        .expect("join");
    assert_eq!(
        rows.rows,
        vec![vec![
            Value::Text("durable".into()),
            Value::Text("temp".into())
        ]]
    );
}

#[test]
fn create_index_on_a_temp_table_is_refused() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create");
    let error = db.execute("CREATE INDEX idx ON t (a)", &[]).unwrap_err();
    assert!(
        error.to_string().contains("temporary"),
        "expected a refusal naming the temporary table, got: {error}"
    );
}

#[test]
fn a_non_key_unique_constraint_on_a_temp_table_is_refused() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "CREATE TEMPORARY TABLE t (a INTEGER PRIMARY KEY, b TEXT UNIQUE)",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("temporary"),
        "expected a refusal naming the temporary table, got: {error}"
    );
}

/// A lone `INTEGER PRIMARY KEY` is a row id alias on a temporary table the
/// same as on an ordinary one — unlike `WITHOUT ROWID`, nothing about how a
/// row is addressed changes here, only where it lives.
#[test]
fn an_integer_primary_key_is_still_a_rowid_alias() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TEMPORARY TABLE t (a INTEGER PRIMARY KEY, b TEXT)",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t (b) VALUES ('first')", &[])
        .expect("auto-assigned key");
    let rows = db.query("SELECT a, b FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1), Value::Text("first".into())]]
    );
}

#[test]
fn alter_table_on_a_temp_table_is_refused() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create");
    let error = db
        .execute("ALTER TABLE t ADD COLUMN b TEXT", &[])
        .unwrap_err();
    assert!(
        error.to_string().contains("temporary"),
        "expected a refusal naming the temporary table, got: {error}"
    );
}

#[test]
fn creating_a_temp_table_inside_a_transaction_is_refused() {
    let (_disk, mut db) = opened();
    db.execute("BEGIN", &[]).expect("begin");
    let error = db
        .execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .unwrap_err();
    assert!(
        error.to_string().contains("transaction"),
        "expected a refusal naming the transaction, got: {error}"
    );
    db.execute("ROLLBACK", &[])
        .expect("rollback the transaction itself");
}

#[test]
fn dropping_a_temp_table_inside_a_transaction_is_refused() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create outside a transaction");
    db.execute("BEGIN", &[]).expect("begin");
    let error = db.execute("DROP TABLE t", &[]).unwrap_err();
    assert!(
        error.to_string().contains("transaction"),
        "expected a refusal naming the transaction, got: {error}"
    );
    db.execute("ROLLBACK", &[])
        .expect("rollback the transaction itself");
    // The table is untouched: the refused DROP never ran.
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, Vec::<Vec<Value>>::new());
}

/// Row-level writes to an already-existing temporary table, unlike its own
/// creation or removal, are fully transactional: they are ordinary rows
/// behind `TempTableRouter`, committed and rolled back together with
/// whatever a durable table in the same transaction does.
#[test]
fn rolling_back_a_transaction_undoes_writes_to_a_temp_table_too() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (1)", &[]).expect("seed");
    db.execute("BEGIN", &[]).expect("begin");
    db.execute("INSERT INTO t VALUES (2)", &[])
        .expect("insert inside transaction");
    db.execute("ROLLBACK", &[]).expect("rollback");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(1)]]);
}

/// A transaction that writes to a temporary and a durable table together
/// commits or rolls back as one unit, whichever table a write in it touched.
#[test]
fn a_transaction_spanning_a_temp_and_a_durable_table_rolls_back_together() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE d (a INTEGER)", &[])
        .expect("create durable");
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create temp");
    db.execute("BEGIN", &[]).expect("begin");
    db.execute("INSERT INTO d VALUES (1)", &[])
        .expect("insert durable");
    db.execute("INSERT INTO t VALUES (1)", &[])
        .expect("insert temp");
    db.execute("ROLLBACK", &[]).expect("rollback");
    assert_eq!(
        db.query("SELECT a FROM d", &[])
            .expect("select durable")
            .rows,
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        db.query("SELECT a FROM t", &[]).expect("select temp").rows,
        Vec::<Vec<Value>>::new()
    );
}

/// Regression test: `DROP TABLE` on a temporary table has to actually erase
/// its rows from the router's in-memory backend, not just release the name
/// — the same class of bug `without_rowid.rs`'s
/// `drop_table_removes_every_row` caught for that feature's own storage.
#[test]
fn dropping_a_temp_table_and_recreating_it_starts_empty() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (1), (2), (3)", &[])
        .expect("insert");
    db.execute("DROP TABLE t", &[]).expect("drop");
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("recreate");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, Vec::<Vec<Value>>::new());
}

#[test]
fn returning_works_on_insert_update_and_delete() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TEMPORARY TABLE t (a INTEGER PRIMARY KEY, b TEXT)",
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
        .query("DELETE FROM t WHERE a = 1 RETURNING a", &[])
        .expect("delete returning");
    assert_eq!(deleted.rows, vec![vec![Value::Integer(1)]]);
}

/// `CREATE TEMPORARY TABLE ... AS SELECT` populates from the same query
/// planning path an ordinary `CTAS` does.
#[test]
fn create_temp_table_as_select_populates_from_a_durable_table() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE src (a INTEGER, b TEXT)", &[])
        .expect("create durable source");
    db.execute("INSERT INTO src VALUES (1, 'x'), (2, 'y')", &[])
        .expect("seed source");
    db.execute("CREATE TEMPORARY TABLE copy AS SELECT * FROM src", &[])
        .expect("create temp as select");
    let rows = db
        .query("SELECT a, b FROM copy ORDER BY a", &[])
        .expect("select from the temp copy");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1), Value::Text("x".into())],
            vec![Value::Integer(2), Value::Text("y".into())],
        ]
    );
}

/// The two features compose: nothing about routing a table's rows to
/// in-memory storage by name conflicts with keying them by primary key
/// bytes instead of a row id.
#[test]
fn without_rowid_and_temporary_combine() {
    let (_disk, mut db) = opened();
    db.execute(
        "CREATE TEMPORARY TABLE t (a INTEGER, b TEXT, PRIMARY KEY (a)) WITHOUT ROWID",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t VALUES (2, 'y'), (1, 'x')", &[])
        .expect("insert");
    let rows = db.query("SELECT a, b FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1), Value::Text("x".into())],
            vec![Value::Integer(2), Value::Text("y".into())],
        ],
        "primary-key order, the same as a durable WITHOUT ROWID table"
    );
}

#[test]
fn aggregates_over_a_temp_table_work() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TEMPORARY TABLE t (a INTEGER)", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (1), (2), (3)", &[])
        .expect("insert");
    let rows = db
        .query("SELECT COUNT(*), SUM(a) FROM t", &[])
        .expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(3), Value::Integer(6)]]);
}
