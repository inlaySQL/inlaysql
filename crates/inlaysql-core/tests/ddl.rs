//! `DROP TABLE`, `CREATE TABLE IF NOT EXISTS` and SQLite's four `ALTER TABLE`
//! operations.
//!
//! Three of the four rewrite every stored row, because a row here is a
//! positional list of values with no column directory — so the assertions that
//! matter are the ones about *data* surviving the schema change, not about the
//! catalog. Each one reads the rows back afterwards.

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::{Engine, Error, Value};

fn engine() -> Engine {
    Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::default()),
    )
    .expect("open")
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

fn rows(engine: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    engine
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => format!("i:{i}"),
                    Value::Real(r) => format!("f:{r}"),
                    Value::Text(t) => format!("t:{t}"),
                    other => format!("{other:?}"),
                })
                .collect()
        })
        .collect()
}

// -------------------------------------------------------- CREATE / DROP TABLE

#[test]
fn if_not_exists_is_a_no_op_and_leaves_the_first_definition_alone() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'kept')");
    // A second definition with different columns: SQLite compares the name
    // only, so this does nothing at all rather than replacing anything.
    run(&mut engine, "CREATE TABLE IF NOT EXISTS t (x REAL)");
    assert_eq!(rows(&mut engine, "SELECT a FROM t"), vec![vec!["t:kept"]]);
    // Without it, the same statement is still an error.
    assert!(matches!(
        refuse(&mut engine, "CREATE TABLE t (x REAL)"),
        Error::Catalog(_)
    ));
}

#[test]
fn drop_table_removes_the_rows_as_well_as_the_declaration() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'x')");
    run(&mut engine, "DROP TABLE t");

    assert!(matches!(
        engine.query("SELECT a FROM t", &[]).unwrap_err(),
        Error::Catalog(_)
    ));
    // Recreating the same name must not resurrect the old rows, which is what
    // would happen if only the catalog entry had been removed.
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
    );
    assert!(rows(&mut engine, "SELECT a FROM t").is_empty());
}

#[test]
fn drop_table_if_exists_is_a_no_op_and_without_it_is_an_error() {
    let mut engine = engine();
    run(&mut engine, "DROP TABLE IF EXISTS nothing");
    assert!(matches!(
        refuse(&mut engine, "DROP TABLE nothing"),
        Error::Catalog(_)
    ));
}

#[test]
fn dropping_a_table_drops_its_indexes() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)",
    );
    run(&mut engine, "CREATE INDEX t_body ON t (body)");
    run(&mut engine, "INSERT INTO t VALUES (1, 'hello world')");
    run(&mut engine, "DROP TABLE t");
    assert!(engine.catalog().indexes().next().is_none());

    // The name is free again, and the new table starts with no index.
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)",
    );
    run(&mut engine, "CREATE INDEX t_body ON t (body)");
    run(&mut engine, "INSERT INTO t VALUES (1, 'fresh text')");
    assert!(
        rows(
            &mut engine,
            "SELECT id, bm25_score(body, 'hello') AS s FROM t"
        )
        .is_empty(),
        "the dropped table's documents must not survive in the rebuilt index"
    );
}

// ----------------------------------------------------------- ALTER ADD COLUMN

#[test]
fn add_column_fills_existing_rows_with_the_default() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'x'), (2, 'y')");

    run(&mut engine, "ALTER TABLE t ADD COLUMN n INTEGER DEFAULT 5");
    run(&mut engine, "ALTER TABLE t ADD COLUMN m TEXT");
    run(&mut engine, "INSERT INTO t (id, a) VALUES (3, 'z')");

    assert_eq!(
        rows(&mut engine, "SELECT id, a, n, m FROM t ORDER BY id"),
        vec![
            vec!["i:1", "t:x", "i:5", "NULL"],
            vec!["i:2", "t:y", "i:5", "NULL"],
            vec!["i:3", "t:z", "i:5", "NULL"],
        ]
    );
}

/// SQLite's restrictions on `ADD COLUMN`, and each exists because the rows
/// already in the table were never written under the new constraint.
#[test]
fn add_column_refuses_what_sqlite_refuses() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
    );
    for sql in [
        "ALTER TABLE t ADD COLUMN b INTEGER PRIMARY KEY",
        "ALTER TABLE t ADD COLUMN b INTEGER UNIQUE",
        "ALTER TABLE t ADD COLUMN b INTEGER NOT NULL",
        "ALTER TABLE t ADD COLUMN b INTEGER NOT NULL DEFAULT NULL",
        "ALTER TABLE t ADD COLUMN a TEXT",
    ] {
        let err = refuse(&mut engine, sql);
        assert!(
            matches!(
                err,
                Error::Unsupported(_) | Error::Constraint(_) | Error::Catalog(_)
            ),
            "`{sql}`: {err}"
        );
    }
    // NOT NULL *with* a real default is fine — every existing row gets it.
    run(
        &mut engine,
        "ALTER TABLE t ADD COLUMN b INTEGER NOT NULL DEFAULT 0",
    );
}

// ------------------------------------------------------------- ALTER RENAME TO

#[test]
fn rename_to_moves_the_rows_and_the_indexes() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)",
    );
    run(&mut engine, "CREATE INDEX t_body ON t (body)");
    run(
        &mut engine,
        "INSERT INTO t VALUES (1, 'hello world'), (2, 'other')",
    );

    run(&mut engine, "ALTER TABLE t RENAME TO papers");

    assert!(matches!(
        engine.query("SELECT id FROM t", &[]).unwrap_err(),
        Error::Catalog(_)
    ));
    assert_eq!(
        rows(&mut engine, "SELECT id, body FROM papers ORDER BY id"),
        vec![vec!["i:1", "t:hello world"], vec!["i:2", "t:other"]]
    );
    // The full-text index followed the table and still answers.
    assert_eq!(
        rows(
            &mut engine,
            "SELECT id, bm25_score(body, 'hello') AS s FROM papers"
        )
        .into_iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>(),
        vec!["i:1"]
    );
    // And a new insert still gets a fresh key rather than colliding.
    run(&mut engine, "INSERT INTO papers (body) VALUES ('third')");
    assert_eq!(rows(&mut engine, "SELECT id FROM papers").len(), 3);
}

#[test]
fn rename_to_an_existing_name_is_refused() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE a (x INTEGER)");
    run(&mut engine, "CREATE TABLE b (x INTEGER)");
    assert!(matches!(
        refuse(&mut engine, "ALTER TABLE a RENAME TO b"),
        Error::Catalog(_)
    ));
}

// --------------------------------------------------------- ALTER RENAME COLUMN

#[test]
fn rename_column_keeps_the_values_and_rewrites_the_constraints() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER CHECK (a > 0), b TEXT UNIQUE)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 5, 'x')");

    run(&mut engine, "ALTER TABLE t RENAME COLUMN a TO amount");
    run(&mut engine, "ALTER TABLE t RENAME COLUMN b TO label");

    assert_eq!(
        rows(&mut engine, "SELECT id, amount, label FROM t"),
        vec![vec!["i:1", "i:5", "t:x"]]
    );
    // The CHECK followed the rename rather than naming a column that is gone.
    assert_eq!(
        engine.catalog().constraints("t").unwrap().checks,
        ["amount > 0"]
    );
    assert!(matches!(
        refuse(&mut engine, "UPDATE t SET amount = -1"),
        Error::Constraint(_)
    ));
    // So did the UNIQUE.
    assert_eq!(
        engine.catalog().constraints("t").unwrap().unique[0].columns,
        ["label"]
    );
    assert!(matches!(
        refuse(&mut engine, "INSERT INTO t VALUES (2, 1, 'x')"),
        Error::Constraint(_)
    ));
}

/// A string literal that happens to spell the old name is not a reference to
/// it, which is why the rewrite tokenises instead of searching for text.
#[test]
fn rename_column_does_not_rewrite_string_literals() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (a TEXT, b TEXT, CHECK (b <> 'a'))",
    );
    run(&mut engine, "ALTER TABLE t RENAME COLUMN a TO renamed");
    assert_eq!(
        engine.catalog().constraints("t").unwrap().checks,
        ["b <> 'a'"]
    );
    assert!(matches!(
        refuse(&mut engine, "INSERT INTO t VALUES ('x', 'a')"),
        Error::Constraint(_)
    ));
}

// ----------------------------------------------------------- ALTER DROP COLUMN

#[test]
fn drop_column_rewrites_every_row() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c TEXT)",
    );
    run(
        &mut engine,
        "INSERT INTO t VALUES (1, 'x', 10, 'p'), (2, 'y', 20, 'q')",
    );

    run(&mut engine, "ALTER TABLE t DROP COLUMN b");

    assert_eq!(
        rows(&mut engine, "SELECT id, a, c FROM t ORDER BY id"),
        vec![vec!["i:1", "t:x", "t:p"], vec!["i:2", "t:y", "t:q"]]
    );
    assert!(matches!(
        engine.query("SELECT b FROM t", &[]).unwrap_err(),
        Error::Catalog(_)
    ));
}

/// SQLite's refusals, each because dropping the column would leave something
/// naming a column that is not there.
#[test]
fn drop_column_refuses_a_column_something_depends_on() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c INTEGER, \
         UNIQUE (a), CHECK (c > 0))",
    );
    run(&mut engine, "CREATE INDEX t_b ON t (b)");
    for column in ["id", "a", "b", "c"] {
        let sql = format!("ALTER TABLE t DROP COLUMN {column}");
        let err = refuse(&mut engine, &sql);
        assert!(matches!(err, Error::Catalog(_)), "`{sql}`: {err}");
    }

    // A table must also keep at least one column.
    run(&mut engine, "CREATE TABLE one (x INTEGER)");
    assert!(matches!(
        refuse(&mut engine, "ALTER TABLE one DROP COLUMN x"),
        Error::Catalog(_)
    ));
}

/// Three of the four `ALTER`s re-encode every row through the table's *new*
/// column types, and a `VECTOR` column is the one where that could lose
/// something: its stored form depends on the declared type, not only on the
/// value. So the embeddings have to survive an `ALTER` byte for byte, and the
/// index over them has to still find them.
#[test]
fn altering_a_table_does_not_disturb_its_embeddings() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(3))",
    );
    run(
        &mut engine,
        "CREATE INDEX docs_embedding ON docs (embedding)",
    );
    run(
        &mut engine,
        "INSERT INTO docs VALUES (1, 'first', vector('[1.0, 0.0, 0.0]')), \
         (2, 'second', vector('[0.0, 1.0, 0.0]'))",
    );

    run(
        &mut engine,
        "ALTER TABLE docs ADD COLUMN score INTEGER DEFAULT 0",
    );
    run(&mut engine, "ALTER TABLE docs DROP COLUMN body");
    run(&mut engine, "ALTER TABLE docs RENAME TO papers");

    // The nearest neighbour of `[1, 0, 0]` is still row 1, which is only true
    // if the embeddings came through the re-encoding unchanged and the index
    // was rebuilt over them.
    let result = engine
        .query(
            "SELECT id, vector_score(embedding, vector('[1.0, 0.0, 0.0]')) AS s \
             FROM papers LIMIT 1",
            &[],
        )
        .expect("retrieval after ALTER");
    assert_eq!(result.rows[0][0], Value::Integer(1));
    assert_eq!(
        rows(&mut engine, "SELECT id, score FROM papers ORDER BY id"),
        vec![vec!["i:1", "i:0"], vec!["i:2", "i:0"]]
    );
}

#[test]
fn alter_table_refuses_what_is_not_in_sqlites_dialect() {
    let mut engine = engine();
    run(&mut engine, "CREATE TABLE t (a INTEGER, b INTEGER)");
    for sql in [
        "ALTER TABLE t ALTER COLUMN a TYPE TEXT",
        "ALTER TABLE t ADD CONSTRAINT u UNIQUE (a)",
        "ALTER TABLE t DROP COLUMN IF EXISTS a",
    ] {
        let err = refuse(&mut engine, sql);
        assert!(matches!(err, Error::Unsupported(_)), "`{sql}`: {err}");
    }
}

/// A prepared statement holds column ordinals, so an `ALTER` that moves them
/// has to invalidate it rather than let it read the wrong column.
#[test]
fn altering_a_table_makes_its_prepared_statements_stale() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 'x', 'y')");
    let statement = engine.prepare("SELECT b FROM t").expect("prepare");
    assert_eq!(
        engine.run_query(&statement, &[]).unwrap().rows[0][0],
        Value::Text("y".to_string().into())
    );

    run(&mut engine, "ALTER TABLE t DROP COLUMN a");
    assert!(matches!(
        engine.run_query(&statement, &[]).unwrap_err(),
        Error::Stale(_)
    ));
}
