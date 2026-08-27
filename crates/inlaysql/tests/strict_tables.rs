//! `CREATE TABLE ... STRICT`.
//!
//! Every expectation here was checked against a real sqlite3 3.54 binary
//! first. `STRICT` changes two things: which type names a column may
//! declare (`INT`/`INTEGER`, `REAL`, `TEXT`, `BLOB`, `ANY` only, no length
//! or precision, no other name), and how narrowly a value is checked and
//! converted against one.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql::{Database, Error, Value};
use inlaysql_core::sim::SimDisk;

const CAPACITY: usize = 64 * 1024 * 1024;

fn opened() -> Database {
    Database::open_on(Rc::new(RefCell::new(SimDisk::new(CAPACITY)))).expect("open")
}

/// Verified against sqlite3: `INSERT ... VALUES (2.0)` into a `STRICT INT`
/// column stores the integer `2`; `(2.5)` is refused ("cannot store REAL
/// value in INT column"). Non-strict `INTEGER` takes neither conversion —
/// unchanged from before this feature existed.
#[test]
fn strict_int_accepts_a_lossless_real_and_rejects_a_lossy_one() {
    let mut db = opened();
    db.execute("CREATE TABLE t (a INT) STRICT", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (2.0)", &[])
        .expect("a lossless real is accepted");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Integer(2)]]);

    let err = db.execute("INSERT INTO t VALUES (2.5)", &[]).unwrap_err();
    assert!(matches!(err, Error::Type(_)), "got {err}");

    let mut plain = opened();
    plain
        .execute("CREATE TABLE t (a INTEGER)", &[])
        .expect("create non-strict");
    assert!(
        plain.execute("INSERT INTO t VALUES (2.0)", &[]).is_err(),
        "non-strict INTEGER must not gain STRICT's real-to-integer conversion"
    );
}

#[test]
fn strict_int_rejects_text() {
    let mut db = opened();
    db.execute("CREATE TABLE t (a INT) STRICT", &[])
        .expect("create");
    let err = db.execute("INSERT INTO t VALUES ('x')", &[]).unwrap_err();
    assert!(matches!(err, Error::Type(_)), "got {err}");
}

/// Verified against sqlite3: a `STRICT TEXT` column storing the integer `5`
/// reads back as the text `5`, and the real `1.5` as the text `1.5` — the
/// same rendering `CAST(x AS TEXT)` uses. A `BLOB` is still refused.
#[test]
fn strict_text_accepts_numbers_by_stringifying_them() {
    let mut db = opened();
    db.execute("CREATE TABLE t (a TEXT) STRICT", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (5)", &[])
        .expect("an integer is stringified");
    db.execute("INSERT INTO t VALUES (1.5)", &[])
        .expect("a real is stringified");
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Text("5".into())],
            vec![Value::Text("1.5".into())],
        ]
    );

    let err = db
        .execute("INSERT INTO t VALUES (x'0102')", &[])
        .unwrap_err();
    assert!(matches!(err, Error::Type(_)), "got {err}");
}

#[test]
fn strict_blob_rejects_everything_but_a_blob() {
    let mut db = opened();
    db.execute("CREATE TABLE t (a BLOB) STRICT", &[])
        .expect("create");
    db.execute("INSERT INTO t VALUES (x'0102')", &[])
        .expect("a real blob is fine");
    for sql in ["INSERT INTO t VALUES (5)", "INSERT INTO t VALUES ('x')"] {
        let err = db.execute(sql, &[]).unwrap_err();
        assert!(matches!(err, Error::Type(_)), "{sql}: got {err}");
    }
}

/// Verified against sqlite3: `ANY` is `STRICT`'s no-affinity column — every
/// storage class round-trips through it exactly as given.
#[test]
fn strict_any_accepts_every_storage_class_unchanged() {
    let mut db = opened();
    db.execute("CREATE TABLE t (a ANY) STRICT", &[])
        .expect("create");
    for sql in [
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (1.5)",
        "INSERT INTO t VALUES ('x')",
        "INSERT INTO t VALUES (x'0102')",
        "INSERT INTO t VALUES (NULL)",
    ] {
        db.execute(sql, &[]).expect("any value at all is fine");
    }
    let rows = db.query("SELECT a FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Real(1.5)],
            vec![Value::Text("x".into())],
            vec![Value::Blob(vec![1, 2])],
            vec![Value::Null],
        ]
    );
}

#[test]
fn strict_refuses_an_unknown_type_name() {
    let mut db = opened();
    let err = db
        .execute("CREATE TABLE t (a NUMERIC) STRICT", &[])
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "got {err}");
}

#[test]
fn strict_refuses_a_missing_type_name() {
    let mut db = opened();
    let err = db.execute("CREATE TABLE t (a) STRICT", &[]).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "got {err}");
}

#[test]
fn strict_refuses_a_type_with_a_length_modifier() {
    let mut db = opened();
    let err = db
        .execute("CREATE TABLE t (a VARCHAR(10)) STRICT", &[])
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "got {err}");
}

/// Verified against sqlite3: `ALTER TABLE ... ADD COLUMN` on a `STRICT`
/// table needs a real type too ("missing datatype").
#[test]
fn strict_alter_table_add_column_requires_a_valid_type() {
    let mut db = opened();
    db.execute("CREATE TABLE t (a INT) STRICT", &[])
        .expect("create");
    let err = db.execute("ALTER TABLE t ADD COLUMN b", &[]).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "got {err}");
    db.execute("ALTER TABLE t ADD COLUMN b TEXT", &[])
        .expect("a real type is accepted");
    db.execute("INSERT INTO t (a, b) VALUES (1, 5)", &[])
        .expect("the new column is strict too");
    let rows = db.query("SELECT b FROM t", &[]).expect("select");
    assert_eq!(rows.rows, vec![vec![Value::Text("5".into())]]);
}

/// `VECTOR(n)` is not SQLite's to refuse or allow; it stays exactly as
/// strict as it already is outside `STRICT`.
#[test]
fn strict_still_allows_vector_columns() {
    let mut db = opened();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, embedding VECTOR(3)) STRICT",
        &[],
    )
    .expect("create");
    db.execute(
        "INSERT INTO t (embedding) VALUES (?)",
        &[Value::Vector(vec![1.0, 2.0, 3.0])],
    )
    .expect("insert");
    let err = db
        .execute(
            "INSERT INTO t (embedding) VALUES (?)",
            &[Value::Vector(vec![1.0, 2.0])],
        )
        .unwrap_err();
    assert!(matches!(err, Error::Type(_)), "got {err}");
}

/// `INTEGER PRIMARY KEY` still aliases the row id in a `STRICT` table.
#[test]
fn strict_integer_primary_key_is_still_a_rowid_alias() {
    let mut db = opened();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT) STRICT",
        &[],
    )
    .expect("create");
    db.execute("INSERT INTO t (v) VALUES ('a')", &[])
        .expect("insert without naming id");
    let rows = db.query("SELECT id, v FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1), Value::Text("a".into())]]
    );
}

/// A `STRICT` table's declaration survives a reopen, and keeps checking
/// values just as narrowly afterwards — proving the catalog round-trips the
/// flag and the `ANY` type rather than only holding them in memory.
#[test]
fn strict_survives_reopening_the_database() {
    let disk = Rc::new(RefCell::new(SimDisk::new(CAPACITY)));
    {
        let mut db = Database::open_on(disk.clone()).expect("open");
        db.execute("CREATE TABLE t (a INT, b ANY) STRICT", &[])
            .expect("create");
        db.execute("INSERT INTO t VALUES (1, 'x')", &[])
            .expect("insert");
    }
    let mut reopened = Database::open_on(disk).expect("reopen");
    let err = reopened
        .execute("INSERT INTO t VALUES ('not an int', 1)", &[])
        .unwrap_err();
    assert!(
        matches!(err, Error::Type(_)),
        "STRICT did not survive reopening: {err}"
    );
    let rows = reopened.query("SELECT a, b FROM t", &[]).expect("select");
    assert_eq!(
        rows.rows,
        vec![vec![Value::Integer(1), Value::Text("x".into())]]
    );
}
