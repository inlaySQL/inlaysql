//! Declared constraints, enforced — and the other half of every one of them:
//! that the rejection leaves the database exactly as it was.
//!
//! A constraint test that only asserts the error is half a test. If the sixth
//! row of an `INSERT` violates a `CHECK`, the first five must not be there
//! afterwards; if a `NOT NULL` rejects an `UPDATE`, the old value must still
//! be readable. Both directions are asserted here for every constraint,
//! because the failure mode that matters is the silent partial write, not the
//! missing error message.
//!
//! The expected *values* come from running the same SQL through the `sqlite3`
//! binary; what is written here is the behaviour SQLite has, not the behaviour
//! this engine happened to produce.

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::{Engine, Error, IndexKind, Value};

fn engine() -> Engine {
    Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::default()),
    )
    .expect("open")
}

/// Every row of a table, as strings, ordered by row id.
fn rows(engine: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    engine
        .query(sql, &[])
        .expect("query")
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect())
        .collect()
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => format!("i:{i}"),
        Value::Real(r) => format!("f:{r}"),
        Value::Text(t) => format!("t:{t}"),
        Value::Blob(b) => format!("b:{}", b.len()),
        Value::Vector(v) => format!("v:{}", v.len()),
    }
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

// ------------------------------------------------------------------- DEFAULT

#[test]
fn a_default_fills_a_column_the_statement_omitted() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER DEFAULT 7, b TEXT DEFAULT 'x', \
         c INTEGER)",
    );
    // Omitted: the default applies. Named and set to NULL: it does not — that
    // distinction is the whole reason the plan keeps them apart.
    run(&mut engine, "INSERT INTO t (id) VALUES (1)");
    run(
        &mut engine,
        "INSERT INTO t (id, a, b) VALUES (2, NULL, NULL)",
    );
    run(&mut engine, "INSERT INTO t (id, a) VALUES (3, 1)");
    run(&mut engine, "INSERT INTO t DEFAULT VALUES");

    assert_eq!(
        rows(&mut engine, "SELECT id, a, b, c FROM t ORDER BY id"),
        vec![
            vec!["i:1", "i:7", "t:x", "NULL"],
            vec!["i:2", "NULL", "NULL", "NULL"],
            vec!["i:3", "i:1", "t:x", "NULL"],
            vec!["i:4", "i:7", "t:x", "NULL"],
        ]
    );
}

#[test]
fn a_default_may_be_an_expression() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER DEFAULT (2 * 3 + 1), \
         b TEXT DEFAULT (upper('ab')))",
    );
    run(&mut engine, "INSERT INTO t (id) VALUES (1)");
    assert_eq!(
        rows(&mut engine, "SELECT a, b FROM t"),
        vec![vec!["i:7", "t:AB"]]
    );
}

#[test]
fn a_default_that_references_a_column_is_refused() {
    let mut engine = engine();
    let err = refuse(
        &mut engine,
        "CREATE TABLE t (a INTEGER, b INTEGER DEFAULT (a + 1))",
    );
    assert!(matches!(err, Error::Catalog(_)), "got {err}");
}

// ------------------------------------------------------------------ NOT NULL

#[test]
fn not_null_is_enforced_and_the_rejection_changes_nothing() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER NOT NULL)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 10)");

    let err = refuse(&mut engine, "INSERT INTO t VALUES (2, NULL)");
    assert!(matches!(err, Error::Constraint(_)), "got {err}");
    assert!(err.to_string().contains("NOT NULL constraint failed: t.a"));

    // An omitted `NOT NULL` column with no default is the same violation.
    assert!(matches!(
        refuse(&mut engine, "INSERT INTO t (id) VALUES (3)"),
        Error::Constraint(_)
    ));
    // And an UPDATE that would null it out.
    assert!(matches!(
        refuse(&mut engine, "UPDATE t SET a = NULL"),
        Error::Constraint(_)
    ));

    assert_eq!(
        rows(&mut engine, "SELECT id, a FROM t ORDER BY id"),
        vec![vec!["i:1", "i:10"]]
    );
}

#[test]
fn a_rejected_row_takes_the_rest_of_its_statement_with_it() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER NOT NULL)",
    );
    // The third row violates; SQLite leaves the table empty, and so must this.
    assert!(matches!(
        refuse(
            &mut engine,
            "INSERT INTO t VALUES (1, 1), (2, 2), (3, NULL)"
        ),
        Error::Constraint(_)
    ));
    assert!(rows(&mut engine, "SELECT id FROM t").is_empty());

    // And the writes it discarded must not surface on the next statement's
    // commit either, which is the part that would be silent.
    run(&mut engine, "INSERT INTO t VALUES (9, 9)");
    assert_eq!(
        rows(&mut engine, "SELECT id, a FROM t ORDER BY id"),
        vec![vec!["i:9", "i:9"]]
    );
}

// --------------------------------------------------------------------- CHECK

#[test]
fn a_check_rejects_only_a_false_result() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER CHECK (a > 0), b TEXT, \
         CHECK (b <> 'no'))",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 5, 'yes')");
    // SQLite: a CHECK passes unless it is *false*, and any comparison with
    // NULL is NULL. `sqlite3 :memory:` confirms this row is accepted.
    run(&mut engine, "INSERT INTO t VALUES (2, NULL, NULL)");

    let err = refuse(&mut engine, "INSERT INTO t VALUES (3, -1, 'yes')");
    assert!(matches!(err, Error::Constraint(_)), "got {err}");
    assert!(err.to_string().contains("CHECK constraint failed"));
    assert!(matches!(
        refuse(&mut engine, "INSERT INTO t VALUES (4, 1, 'no')"),
        Error::Constraint(_)
    ));
    assert!(matches!(
        refuse(&mut engine, "UPDATE t SET a = -5 WHERE id = 1"),
        Error::Constraint(_)
    ));

    assert_eq!(
        rows(&mut engine, "SELECT id, a, b FROM t ORDER BY id"),
        vec![vec!["i:1", "i:5", "t:yes"], vec!["i:2", "NULL", "NULL"]]
    );
}

// -------------------------------------------------------------------- UNIQUE

#[test]
fn unique_is_enforced_on_insert_and_update() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, e TEXT UNIQUE, n INTEGER)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'a', 1)");

    let err = refuse(&mut engine, "INSERT INTO t VALUES (2, 'a', 2)");
    assert!(matches!(err, Error::Constraint(_)), "got {err}");
    assert!(err.to_string().contains("UNIQUE constraint failed: t.e"));

    run(&mut engine, "INSERT INTO t VALUES (2, 'b', 2)");
    assert!(matches!(
        refuse(&mut engine, "UPDATE t SET e = 'a' WHERE id = 2"),
        Error::Constraint(_)
    ));
    assert_eq!(
        rows(&mut engine, "SELECT id, e FROM t ORDER BY id"),
        vec![vec!["i:1", "t:a"], vec!["i:2", "t:b"]]
    );
}

/// SQLite's two rules for a unique key, both confirmed against `sqlite3`:
/// a `NULL` never collides with anything, and the comparison is by storage
/// class, so the integer 1 and the real 1.0 are one key but the text '1' is
/// another.
#[test]
fn unique_follows_sqlites_null_and_storage_class_rules() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (a NUMERIC UNIQUE)");
    run(&mut engine, "INSERT INTO t VALUES (NULL)");
    run(&mut engine, "INSERT INTO t VALUES (NULL)");
    run(&mut engine, "INSERT INTO t VALUES (1)");
    run(&mut engine, "INSERT INTO t VALUES ('x')");
    assert!(matches!(
        refuse(&mut engine, "INSERT INTO t VALUES (1.0)"),
        Error::Constraint(_)
    ));
    assert_eq!(rows(&mut engine, "SELECT a FROM t").len(), 4);
}

#[test]
fn a_composite_unique_needs_every_column_to_match() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, UNIQUE (a, b))",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 1, 1)");
    run(&mut engine, "INSERT INTO t VALUES (2, 1, 2)");
    assert!(matches!(
        refuse(&mut engine, "INSERT INTO t VALUES (3, 1, 1)"),
        Error::Constraint(_)
    ));
    assert_eq!(rows(&mut engine, "SELECT id FROM t").len(), 2);
}

/// A `TEXT PRIMARY KEY` is a unique index in SQLite, not the row id — and it
/// used to be refused outright here.
#[test]
fn a_text_primary_key_is_a_unique_constraint() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (k TEXT PRIMARY KEY, v INTEGER)",
    );
    run(&mut engine, "INSERT INTO t VALUES ('a', 1)");
    assert!(matches!(
        refuse(&mut engine, "INSERT INTO t VALUES ('a', 2)"),
        Error::Constraint(_)
    ));
    run(&mut engine, "INSERT INTO t VALUES ('b', 2)");
    assert_eq!(rows(&mut engine, "SELECT k FROM t ORDER BY k").len(), 2);
}

/// `CREATE UNIQUE INDEX` is the spelling a framework's migrations use. Both
/// halves are real: the `UNIQUE` half is a constraint, recorded and enforced
/// exactly as an inline `UNIQUE (...)` is, and the *index* half is a B-tree
/// index under the same name that enforces it with a probe and answers a query
/// filtering on the column. `crates/inlaysql-core/tests/btree_index.rs` is
/// where the index half is tested; this stays the constraint's own test.
#[test]
fn create_unique_index_records_and_enforces_a_constraint() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL)",
    );
    run(
        &mut engine,
        "CREATE UNIQUE INDEX users_email ON users (email)",
    );
    run(&mut engine, "INSERT INTO users VALUES (1, 'a@example.com')");

    let err = refuse(&mut engine, "INSERT INTO users VALUES (2, 'a@example.com')");
    assert!(matches!(err, Error::Constraint(_)), "got {err}");
    assert!(err
        .to_string()
        .contains("UNIQUE constraint failed: users.email"));

    run(&mut engine, "INSERT INTO users VALUES (2, 'b@example.com')");
    assert!(matches!(
        refuse(
            &mut engine,
            "UPDATE users SET email = 'a@example.com' WHERE id = 2"
        ),
        Error::Constraint(_)
    ));
    assert_eq!(
        rows(&mut engine, "SELECT email FROM users ORDER BY id"),
        vec![vec!["t:a@example.com"], vec!["t:b@example.com"]]
    );

    // It is droppable by the name it was given, and the constraint goes with
    // it — the two share one namespace, as they do in SQLite.
    run(&mut engine, "DROP INDEX users_email");
    run(
        &mut engine,
        "UPDATE users SET email = 'a@example.com' WHERE id = 2",
    );
    assert_eq!(
        rows(&mut engine, "SELECT email FROM users ORDER BY id"),
        vec![vec!["t:a@example.com"], vec!["t:a@example.com"]]
    );
}

/// A unique index over data that already violates it is an error, not a
/// constraint that starts out already broken.
#[test]
fn create_unique_index_checks_the_rows_already_there() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, e TEXT)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'x'), (2, 'x')");
    assert!(matches!(
        refuse(&mut engine, "CREATE UNIQUE INDEX t_e ON t (e)"),
        Error::Constraint(_)
    ));

    run(&mut engine, "DELETE FROM t WHERE id = 2");
    run(&mut engine, "CREATE UNIQUE INDEX t_e ON t (e)");
    // A second index of the same name is refused, whichever kind it is.
    assert!(matches!(
        refuse(&mut engine, "CREATE UNIQUE INDEX t_e ON t (id)"),
        Error::Catalog(_)
    ));
}

/// A plain `CREATE INDEX` on a scalar column is now a real B-tree index
/// (`docs/architecture.md`, decision D3). What stays refused is a column with no ordering:
/// a `VECTOR` has none, and giving it one would be inventing an answer.
#[test]
fn a_scalar_index_is_built_and_a_vector_one_is_refused() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, v VECTOR(2))",
    );
    run(&mut engine, "CREATE INDEX t_n ON t (n)");
    assert!(engine
        .catalog()
        .indexes_for("t")
        .iter()
        .any(|index| index.name == "t_n" && index.kind == IndexKind::BTree));

    let err = refuse(&mut engine, "CREATE INDEX t_v ON t (v) USING BTREE");
    assert!(matches!(err, Error::Type(_)), "got {err}");
    assert!(err.to_string().contains("orderable"), "got {err}");
}

// --------------------------------------------------------------- FOREIGN KEY

/// Recorded and not enforced, which is SQLite's own default. The point of the
/// test is that both halves are true: the declaration survives into the
/// catalog, and a row that violates it is still accepted.
#[test]
fn a_foreign_key_is_recorded_and_not_enforced() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE parent (id INTEGER PRIMARY KEY)");
    run(
        &mut engine,
        "CREATE TABLE child (id INTEGER PRIMARY KEY, \
         parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE)",
    );
    run(&mut engine, "INSERT INTO child VALUES (1, 999)");
    assert_eq!(
        rows(&mut engine, "SELECT parent_id FROM child"),
        vec![vec!["i:999"]]
    );

    let keys = &engine
        .catalog()
        .constraints("child")
        .expect("child declares constraints")
        .foreign_keys;
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].columns, ["parent_id"]);
    assert_eq!(keys[0].table, "parent");
    assert_eq!(keys[0].on_delete.as_deref(), Some("CASCADE"));
}

// ------------------------------------------------------------------ affinity

/// The types a Laravel migration actually emits. Every one of these was a hard
/// error before decision D7; the values are what `sqlite3` stores for them.
#[test]
fn framework_type_names_resolve_and_store_like_sqlite() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         name VARCHAR(255) NOT NULL, \
         price DECIMAL(8,2), \
         active BOOLEAN DEFAULT 0, \
         payload JSON, \
         created_at DATETIME)",
    );
    run(
        &mut engine,
        "INSERT INTO t (name, price, payload, created_at) \
         VALUES ('a', '10.50', '{\"k\":1}', '2024-01-01 00:00:00')",
    );
    // `sqlite3` gives: integer 1, text a, real 10.5, integer 0, text {"k":1},
    // text 2024-01-01 00:00:00.
    assert_eq!(
        rows(
            &mut engine,
            "SELECT id, name, price, active, payload, created_at FROM t"
        ),
        vec![vec![
            "i:1",
            "t:a",
            "f:10.5",
            "i:0",
            "t:{\"k\":1}",
            "t:2024-01-01 00:00:00"
        ]]
    );
}

/// `NUMERIC` is not a storage class: it converts what it can and stores the
/// rest unchanged. Every expectation here was produced by `sqlite3`.
#[test]
fn numeric_affinity_converts_only_what_is_a_number() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n NUMERIC)",
    );
    for (id, literal) in [
        (1, "4.0"),
        (2, "'10.50'"),
        (3, "'7'"),
        (4, "'abc'"),
        (5, "' 9 '"),
        (6, "'1e16'"),
        (7, "'9223372036854775808'"),
        (8, "2.5"),
        (9, "''"),
        (10, "'0x10'"),
    ] {
        run(
            &mut engine,
            &format!("INSERT INTO t VALUES ({id}, {literal})"),
        );
    }
    assert_eq!(
        rows(&mut engine, "SELECT n FROM t ORDER BY id"),
        vec![
            vec!["i:4"],
            vec!["f:10.5"],
            vec!["i:7"],
            vec!["t:abc"],
            vec!["i:9"],
            vec!["i:10000000000000000"],
            vec!["f:9223372036854776000"],
            vec!["f:2.5"],
            vec!["t:"],
            vec!["t:0x10"],
        ]
    );
}
