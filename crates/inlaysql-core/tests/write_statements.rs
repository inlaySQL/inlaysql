//! `INSERT ... SELECT`, the conflict clauses, `RETURNING`, and `BEGIN` /
//! `COMMIT` / `ROLLBACK` written as SQL.
//!
//! Every one of these was an explicit `Error::Unsupported` after AHL-410,
//! which is what made it safe to leave them unimplemented. The tests here are
//! the other side of that: what each one does now, checked against what
//! `sqlite3` does with the same statements.

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::{Engine, Error, Outcome, Value};

fn engine() -> Engine {
    Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::default()),
    )
    .expect("open")
}

fn run(engine: &mut Engine, sql: &str) -> Outcome {
    engine
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
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
        .map(|row| row.iter().map(render).collect())
        .collect()
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => format!("i:{i}"),
        Value::Real(r) => format!("f:{r}"),
        Value::Text(t) => format!("t:{t}"),
        other => format!("{other:?}"),
    }
}

/// `t (id INTEGER PRIMARY KEY, e TEXT UNIQUE, n INTEGER)` with two rows.
fn seeded() -> Engine {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, e TEXT UNIQUE, n INTEGER)",
    );
    run(
        &mut engine,
        "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20)",
    );
    engine
}

// ------------------------------------------------------------ INSERT ... SELECT

#[test]
fn insert_select_copies_rows_and_defaults_what_it_does_not_name() {
    let mut engine = seeded();
    run(
        &mut engine,
        "CREATE TABLE archive (id INTEGER PRIMARY KEY, e TEXT, note TEXT DEFAULT 'copied')",
    );
    let outcome = run(
        &mut engine,
        "INSERT INTO archive (id, e) SELECT id, e FROM t ORDER BY id",
    );
    assert_eq!(outcome, Outcome::Written(2));
    assert_eq!(
        rows(&mut engine, "SELECT id, e, note FROM archive ORDER BY id"),
        vec![
            vec!["i:1", "t:a", "t:copied"],
            vec!["i:2", "t:b", "t:copied"],
        ]
    );
}

/// The query runs to completion before any row is written, which is what makes
/// `INSERT INTO t SELECT ... FROM t` terminate instead of feeding itself.
#[test]
fn insert_select_reads_the_table_as_it_was() {
    let mut engine = seeded();
    run(
        &mut engine,
        "INSERT INTO t (e, n) SELECT e || '!', n FROM t",
    );
    assert_eq!(
        rows(&mut engine, "SELECT e FROM t ORDER BY id"),
        vec![vec!["t:a"], vec!["t:b"], vec!["t:a!"], vec!["t:b!"]]
    );
}

#[test]
fn insert_select_checks_the_column_count() {
    let mut engine = seeded();
    let err = refuse(&mut engine, "INSERT INTO t (id, e) SELECT id FROM t");
    assert!(matches!(err, Error::Type(_)), "got {err}");
}

/// Aggregates and joins come for free, because the query is planned exactly as
/// a standalone `SELECT`.
#[test]
fn insert_select_accepts_any_query_shape() {
    let mut engine = seeded();
    run(&mut engine, "CREATE TABLE totals (n INTEGER, c INTEGER)");
    run(
        &mut engine,
        "INSERT INTO totals (n, c) SELECT SUM(n), COUNT(*) FROM t",
    );
    assert_eq!(
        rows(&mut engine, "SELECT n, c FROM totals"),
        vec![vec!["i:30", "i:2"]]
    );
}

// --------------------------------------------------------------- OR IGNORE

#[test]
fn or_ignore_skips_the_conflicting_row_and_keeps_the_rest() {
    let mut engine = seeded();
    // The middle row conflicts on the UNIQUE column; SQLite keeps the other
    // two and reports two changes.
    let outcome = run(
        &mut engine,
        "INSERT OR IGNORE INTO t VALUES (3, 'c', 30), (4, 'a', 40), (5, 'd', 50)",
    );
    assert_eq!(outcome, Outcome::Written(2));
    assert_eq!(
        rows(&mut engine, "SELECT id, e FROM t ORDER BY id"),
        vec![
            vec!["i:1", "t:a"],
            vec!["i:2", "t:b"],
            vec!["i:3", "t:c"],
            vec!["i:5", "t:d"],
        ]
    );
    // `ON CONFLICT DO NOTHING` is the same policy under another spelling.
    run(
        &mut engine,
        "INSERT INTO t VALUES (6, 'a', 60) ON CONFLICT DO NOTHING",
    );
    assert_eq!(rows(&mut engine, "SELECT id FROM t").len(), 4);
}

// -------------------------------------------------------------- OR REPLACE

#[test]
fn or_replace_deletes_every_row_it_conflicts_with() {
    let mut engine = seeded();
    // Conflicts with row 1 on the row id *and* with row 2 on `e`: SQLite
    // deletes both and inserts one.
    run(&mut engine, "REPLACE INTO t VALUES (1, 'b', 99)");
    assert_eq!(
        rows(&mut engine, "SELECT id, e, n FROM t ORDER BY id"),
        vec![vec!["i:1", "t:b", "i:99"]]
    );
    run(&mut engine, "INSERT OR REPLACE INTO t VALUES (1, 'z', 5)");
    assert_eq!(
        rows(&mut engine, "SELECT id, e, n FROM t ORDER BY id"),
        vec![vec!["i:1", "t:z", "i:5"]]
    );
}

/// `INSERT OR IGNORE` and `ON CONFLICT DO NOTHING` are **not** the same
/// clause, which is easy to believe and wrong. `OR IGNORE` is a
/// conflict-resolution algorithm and SQLite applies it to every constraint;
/// `ON CONFLICT DO NOTHING` is the upsert clause and covers uniqueness only.
/// Both directions confirmed against `sqlite3`.
#[test]
fn or_ignore_covers_every_constraint_and_do_nothing_covers_uniqueness() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0 CHECK (n >= 0))",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 1)");

    // `OR IGNORE` skips a CHECK and a NOT NULL violation.
    run(&mut engine, "INSERT OR IGNORE INTO t VALUES (2, -1)");
    run(&mut engine, "INSERT OR IGNORE INTO t VALUES (3, NULL)");
    assert_eq!(rows(&mut engine, "SELECT id FROM t"), vec![vec!["i:1"]]);

    // `DO NOTHING` does not.
    for sql in [
        "INSERT INTO t VALUES (2, -1) ON CONFLICT DO NOTHING",
        "INSERT INTO t VALUES (3, NULL) ON CONFLICT DO NOTHING",
    ] {
        assert!(
            matches!(refuse(&mut engine, sql), Error::Constraint(_)),
            "{sql}"
        );
    }
    assert_eq!(rows(&mut engine, "SELECT id FROM t"), vec![vec!["i:1"]]);
}

/// `REPLACE` on a `NOT NULL` column does not replace a *row*: it substitutes
/// the column's default for the `NULL`, and only aborts when there is no
/// usable one. It does not absorb a `CHECK` violation at all.
#[test]
fn or_replace_substitutes_a_default_for_a_null_but_not_for_a_failed_check() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL DEFAULT 7 CHECK (n >= 0))",
    );
    run(&mut engine, "INSERT OR REPLACE INTO t VALUES (1, NULL)");
    assert_eq!(
        rows(&mut engine, "SELECT id, n FROM t"),
        vec![vec!["i:1", "i:7"]]
    );
    assert!(matches!(
        refuse(&mut engine, "INSERT OR REPLACE INTO t VALUES (2, -1)"),
        Error::Constraint(_)
    ));

    // With no default there is nothing to substitute, so it aborts.
    run(
        &mut engine,
        "CREATE TABLE u (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
    );
    assert!(matches!(
        refuse(&mut engine, "INSERT OR REPLACE INTO u VALUES (1, NULL)"),
        Error::Constraint(_)
    ));
}

/// A `CHECK` is evaluated before uniqueness, so a row that fails both reports
/// the `CHECK` — and an upsert clause does not absorb it.
#[test]
fn a_check_is_reported_ahead_of_a_collision() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER CHECK (n >= 0))",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 1)");
    let err = refuse(
        &mut engine,
        "INSERT INTO t VALUES (1, -1) ON CONFLICT DO NOTHING",
    );
    assert!(
        err.to_string().contains("CHECK constraint failed"),
        "got {err}"
    );
}

// ------------------------------------------------------- ON CONFLICT DO UPDATE

#[test]
fn do_update_is_an_upsert_over_the_stored_and_proposed_rows() {
    let mut engine = seeded();
    run(
        &mut engine,
        "INSERT INTO t VALUES (1, 'a', 5) \
         ON CONFLICT (id) DO UPDATE SET n = n + excluded.n",
    );
    assert_eq!(
        rows(&mut engine, "SELECT id, n FROM t ORDER BY id"),
        vec![vec!["i:1", "i:15"], vec!["i:2", "i:20"]]
    );
    // No conflict: it inserts, and the DO UPDATE never fires.
    run(
        &mut engine,
        "INSERT INTO t VALUES (3, 'c', 7) ON CONFLICT (id) DO UPDATE SET n = 0",
    );
    assert_eq!(
        rows(&mut engine, "SELECT id, n FROM t ORDER BY id"),
        vec![vec!["i:1", "i:15"], vec!["i:2", "i:20"], vec!["i:3", "i:7"]]
    );
}

/// **The conflict target narrows what the clause answers for.** A row that
/// collides on some constraint the target does not name is an ordinary
/// violation, not an upsert. The differential oracle found this; nobody
/// guessed it.
#[test]
fn a_conflict_the_target_does_not_name_is_still_a_violation() {
    let mut engine = seeded();
    // Collides on `e` (row 1 holds 'a'), and the target names `id`.
    let err = refuse(
        &mut engine,
        "INSERT INTO t VALUES (9, 'a', 0) ON CONFLICT (id) DO UPDATE SET n = 0",
    );
    assert!(
        err.to_string().contains("UNIQUE constraint failed: t.e"),
        "got {err}"
    );
    assert!(matches!(
        refuse(
            &mut engine,
            "INSERT INTO t VALUES (9, 'a', 0) ON CONFLICT (id) DO NOTHING"
        ),
        Error::Constraint(_)
    ));

    // When it collides on *both*, the clause acts on the one it named and
    // leaves the other alone — no error, no second row touched.
    run(
        &mut engine,
        "INSERT INTO t VALUES (1, 'b', 0) ON CONFLICT (id) DO UPDATE SET n = 77",
    );
    assert_eq!(
        rows(&mut engine, "SELECT id, e, n FROM t ORDER BY id"),
        vec![vec!["i:1", "t:a", "i:77"], vec!["i:2", "t:b", "i:20"]]
    );
    // Targeting `e` instead picks the other row.
    run(
        &mut engine,
        "INSERT INTO t VALUES (1, 'b', 0) ON CONFLICT (e) DO UPDATE SET n = 88",
    );
    assert_eq!(
        rows(&mut engine, "SELECT id, e, n FROM t ORDER BY id"),
        vec![vec!["i:1", "t:a", "i:77"], vec!["i:2", "t:b", "i:88"]]
    );
}

/// A `WHERE` that excludes the row leaves it exactly as it was — SQLite does
/// not fall back to inserting.
#[test]
fn do_update_where_leaves_an_excluded_row_untouched() {
    let mut engine = seeded();
    run(
        &mut engine,
        "INSERT INTO t VALUES (1, 'a', 99) \
         ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE n > 100",
    );
    assert_eq!(
        rows(&mut engine, "SELECT id, n FROM t ORDER BY id"),
        vec![vec!["i:1", "i:10"], vec!["i:2", "i:20"]]
    );
}

#[test]
fn do_update_still_enforces_the_other_constraints() {
    let mut engine = engine();
    run(
        &mut engine,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL CHECK (n > 0))",
    );
    run(&mut engine, "INSERT INTO t VALUES (1, 5)");
    for sql in [
        "INSERT INTO t VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET n = NULL",
        "INSERT INTO t VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET n = -1",
    ] {
        assert!(matches!(refuse(&mut engine, sql), Error::Constraint(_)));
    }
    assert_eq!(rows(&mut engine, "SELECT n FROM t"), vec![vec!["i:5"]]);
}

// ----------------------------------------------------------------- RETURNING

#[test]
fn returning_reports_the_row_each_statement_wrote() {
    let mut engine = seeded();

    // An assigned key is visible, which is most of why anybody writes it.
    let result = engine
        .query("INSERT INTO t (e, n) VALUES ('c', 30) RETURNING id, e", &[])
        .expect("insert returning");
    assert_eq!(result.columns, ["id", "e"]);
    assert_eq!(result.rows, vec![vec![Value::Integer(3), Value::from("c")]]);

    // UPDATE returns the row *after* the change.
    assert_eq!(
        rows(&mut engine, "UPDATE t SET n = n * 2 RETURNING id, n"),
        vec![
            vec!["i:1", "i:20"],
            vec!["i:2", "i:40"],
            vec!["i:3", "i:60"]
        ]
    );

    // DELETE can only return the row as it was.
    assert_eq!(
        rows(&mut engine, "DELETE FROM t WHERE id = 2 RETURNING *"),
        vec![vec!["i:2", "t:b", "i:40"]]
    );
    assert_eq!(rows(&mut engine, "SELECT id FROM t ORDER BY id").len(), 2);
}

#[test]
fn returning_on_a_statement_that_matched_nothing_returns_no_rows() {
    let mut engine = seeded();
    assert!(rows(&mut engine, "DELETE FROM t WHERE id = 99 RETURNING id").is_empty());
    assert!(rows(&mut engine, "UPDATE t SET n = 0 WHERE id = 99 RETURNING id").is_empty());
}

/// `last_insert_rowid()` is a question about keys the *engine* chose, and it is
/// decided per row rather than per statement: one `INSERT` may name some keys
/// and leave others to the counter.
#[test]
fn last_insert_row_id_reports_only_the_keys_the_engine_chose() {
    let mut engine = seeded();
    assert_eq!(engine.last_insert_row_id(), None);

    // Every key supplied: nothing to report.
    run(&mut engine, "INSERT INTO t VALUES (5, 'e', 1)");
    assert_eq!(engine.last_insert_row_id(), None);

    // Mixed in one statement: the assigned one is reported, not the named one,
    // even though the named one is written last.
    run(
        &mut engine,
        "INSERT INTO t (id, e) VALUES (NULL, 'f'), (9, 'g')",
    );
    assert_eq!(engine.last_insert_row_id(), Some(6));

    // A skipped row has no key to report, so the previous value survives.
    run(&mut engine, "INSERT OR IGNORE INTO t (e) VALUES ('f')");
    assert_eq!(engine.last_insert_row_id(), Some(6));
}

// -------------------------------------------------------------- transactions

#[test]
fn begin_commit_and_rollback_work_as_sql() {
    let mut engine = seeded();

    run(&mut engine, "BEGIN");
    run(&mut engine, "INSERT INTO t VALUES (3, 'c', 30)");
    // Read-your-writes inside the transaction.
    assert_eq!(rows(&mut engine, "SELECT id FROM t").len(), 3);
    run(&mut engine, "ROLLBACK");
    assert_eq!(rows(&mut engine, "SELECT id FROM t").len(), 2);

    run(&mut engine, "BEGIN TRANSACTION");
    run(&mut engine, "INSERT INTO t VALUES (3, 'c', 30)");
    run(&mut engine, "COMMIT");
    assert_eq!(rows(&mut engine, "SELECT id FROM t").len(), 3);

    // `END` is SQLite's spelling of COMMIT.
    run(&mut engine, "BEGIN");
    run(&mut engine, "DELETE FROM t WHERE id = 3");
    run(&mut engine, "END");
    assert_eq!(rows(&mut engine, "SELECT id FROM t").len(), 2);
}

#[test]
fn transaction_misuse_is_reported_rather_than_absorbed() {
    let mut engine = seeded();
    assert!(matches!(
        refuse(&mut engine, "COMMIT"),
        Error::Transaction(_)
    ));
    assert!(matches!(
        refuse(&mut engine, "ROLLBACK"),
        Error::Transaction(_)
    ));
    run(&mut engine, "BEGIN");
    assert!(matches!(
        refuse(&mut engine, "BEGIN"),
        Error::Transaction(_)
    ));
    run(&mut engine, "ROLLBACK");
}

/// A savepoint is a nested rollback point, and the storage engine buffers a
/// transaction as one set of writes. Refusing is the honest answer.
#[test]
fn savepoints_are_refused() {
    let mut engine = seeded();
    for sql in [
        "SAVEPOINT s",
        "RELEASE SAVEPOINT s",
        "ROLLBACK TO SAVEPOINT s",
    ] {
        assert!(matches!(refuse(&mut engine, sql), Error::Unsupported(_)));
    }
}

/// Updating the row-id alias moves the row, because the column *is* the
/// storage key. Writing it back under the old key would leave `SELECT id`
/// reporting a value that `WHERE id = ...` could not find.
#[test]
fn updating_the_primary_key_moves_the_row() {
    let mut engine = seeded();
    run(&mut engine, "UPDATE t SET id = 7 WHERE id = 1");
    assert_eq!(
        rows(&mut engine, "SELECT id, e FROM t ORDER BY id"),
        vec![vec!["i:2", "t:b"], vec!["i:7", "t:a"]]
    );
    assert_eq!(
        rows(&mut engine, "SELECT e FROM t WHERE id = 7"),
        vec![vec!["t:a"]]
    );
    assert!(rows(&mut engine, "SELECT e FROM t WHERE id = 1").is_empty());
    // And moving it onto an occupied key is a constraint violation.
    assert!(matches!(
        refuse(&mut engine, "UPDATE t SET id = 2 WHERE id = 7"),
        Error::Constraint(_)
    ));
}
