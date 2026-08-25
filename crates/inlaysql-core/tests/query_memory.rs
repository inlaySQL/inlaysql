//! The ceiling on what one statement may hold
//! (`docs/enterprise-readiness.md`, blocker 8, second half).
//!
//! `ORDER BY`, `GROUP BY`, `DISTINCT` and window functions cannot answer
//! before they have read every input row — see `crate::exec`'s module docs for
//! why that is inherent — so each one holds its whole input at once. The
//! question these tests settle is not whether that happens; it is what happens
//! when it does not fit. Unbounded, the answer is the operating system's
//! out-of-memory killer, which does not end the query, it ends the process. So
//! the property under test is a **refusal**: one statement fails, with an error
//! that says what it hit, and the handle keeps working.
//!
//! There is no spilling to disk here and none is being tested for. A refused
//! query is recoverable; a dead process is not, and that is the whole trade.

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::{Engine, EngineOptions, Error, Value};

/// How many rows every table below holds. Enough that the sort's working set
/// is comfortably over the small ceilings used here and comfortably under the
/// shipped default, so neither number is being tested by accident.
const ROWS: i64 = 2_000;

/// An engine with `budget` bytes of blocking-query memory and `ROWS` rows.
fn seeded(budget: usize) -> Engine {
    let mut engine = Engine::open_with_options(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
        EngineOptions {
            query_memory_bytes: budget,
            ..EngineOptions::default()
        },
    )
    .expect("open");
    engine
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, grp INTEGER, body TEXT)",
            &[],
        )
        .unwrap();
    engine.begin().unwrap();
    for id in 1..=ROWS {
        engine
            .execute(
                "INSERT INTO t (id, grp, body) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Integer(id % 7),
                    Value::Text(format!("row-{id}-padding-padding-padding").into()),
                ],
            )
            .unwrap();
    }
    engine.commit().unwrap();
    engine
}

/// Roughly what one row of `t` costs once decoded, used to pick ceilings that
/// are unambiguously above or below the working set rather than near it.
const APPROX_ROW_BYTES: usize = 160;

fn refusal(engine: &mut Engine, sql: &str) -> String {
    match engine.query(sql, &[]) {
        Err(Error::Memory(message)) => message,
        Err(other) => panic!("{sql} failed, but not on memory: {other:?}"),
        Ok(rows) => panic!("{sql} was allowed to hold {} rows", rows.rows.len()),
    }
}

/// Every blocking operator is bounded, not just the sort. Each of these has to
/// hold the whole input for its own reason, and each is a way to take the
/// process down if it is the one that was left unbounded.
#[test]
fn every_blocking_operator_is_refused_past_the_ceiling() {
    let mut engine = seeded(ROWS as usize * APPROX_ROW_BYTES / 8);
    for sql in [
        "SELECT id, body FROM t ORDER BY body",
        "SELECT grp, count(*) FROM t GROUP BY grp",
        "SELECT DISTINCT grp FROM t",
        "SELECT id, row_number() OVER (ORDER BY id) FROM t",
        "SELECT count(*) FROM t",
    ] {
        let message = refusal(&mut engine, sql);
        assert!(
            message.contains("ceiling"),
            "{sql}: the refusal must say what it hit, got: {message}"
        );
    }
}

/// The refusal names the number it hit and how to move it, because the person
/// reading it has to decide between raising the ceiling and narrowing the
/// query, and cannot do that from "out of memory".
#[test]
fn the_refusal_names_the_ceiling_and_what_to_do_about_it() {
    let budget = ROWS as usize * APPROX_ROW_BYTES / 8;
    let mut engine = seeded(budget);
    let message = refusal(&mut engine, "SELECT id, body FROM t ORDER BY body");
    assert!(
        message.contains(&budget.to_string()),
        "the refusal must name the ceiling it hit, got: {message}"
    );
    assert!(
        message.contains("query_memory_bytes"),
        "the refusal must name the option that moves it, got: {message}"
    );
    assert!(
        message.contains("Nothing was written"),
        "the refusal must say what state it left behind, got: {message}"
    );
}

/// The same statement under a ceiling it fits in is not affected in any way:
/// same rows, same order. A budget that changed an answer would be worse than
/// no budget.
#[test]
fn the_same_query_under_the_ceiling_answers_exactly_as_before() {
    let mut bounded = seeded(64 * 1024 * 1024);
    let mut unbounded = seeded(0);
    for sql in [
        "SELECT id, body FROM t ORDER BY body LIMIT 50",
        "SELECT grp, count(*) AS n FROM t GROUP BY grp ORDER BY grp",
        "SELECT DISTINCT grp FROM t ORDER BY grp DESC",
    ] {
        let with = bounded.query(sql, &[]).expect(sql);
        let without = unbounded.query(sql, &[]).expect(sql);
        assert_eq!(with.columns, without.columns, "{sql}");
        assert_eq!(with.rows, without.rows, "{sql}");
    }
}

/// A ceiling of zero is no ceiling — the behaviour every caller had before the
/// option existed, kept reachable rather than only documented.
#[test]
fn a_ceiling_of_zero_removes_it() {
    let mut engine = seeded(0);
    let rows = engine
        .query("SELECT id FROM t ORDER BY body", &[])
        .expect("no ceiling means no refusal");
    assert_eq!(rows.rows.len(), ROWS as usize);
}

/// The ceiling bounds what *blocks*, and a query that does not block is not
/// bounded by it — which is the other half of blocker 8 and the reason the two
/// changes belong together. `SELECT * FROM big_table` streams: it holds one row
/// at a time, so a ceiling of a few hundred bytes does not touch it, while the
/// same rows behind an `ORDER BY` are refused by that same ceiling.
#[test]
fn a_query_that_does_not_block_is_not_bounded_by_the_ceiling() {
    let mut engine = seeded(512);

    let mut delivered = 0usize;
    let scan = engine
        .prepare("SELECT id, body FROM t")
        .expect("prepare a plain scan");
    engine
        .run_query_each(&scan, &[], |_row| {
            delivered += 1;
            Ok(())
        })
        .expect("a non-blocking scan holds one row, not the table");
    assert_eq!(delivered, ROWS as usize);

    // The same rows, sorted, over the same ceiling.
    let sorted = engine
        .prepare("SELECT id, body FROM t ORDER BY body")
        .expect("prepare a sort");
    let refused = engine.run_query_each(&sorted, &[], |_row| Ok(()));
    assert!(
        matches!(refused, Err(Error::Memory(_))),
        "a sort over the same rows must still be refused, got {refused:?}"
    );
}

/// A refusal is recoverable, which is the entire point of preferring it to an
/// allocation. Nothing was read into the answer, nothing was written, and the
/// very next statement on the same handle works.
#[test]
fn the_handle_is_still_usable_after_a_refusal() {
    let mut engine = seeded(ROWS as usize * APPROX_ROW_BYTES / 8);
    refusal(&mut engine, "SELECT id, body FROM t ORDER BY body");

    let rows = engine
        .query("SELECT id FROM t WHERE id = 42", &[])
        .expect("the handle survived the refusal");
    assert_eq!(rows.rows, vec![vec![Value::Integer(42)]]);

    engine
        .execute(
            "INSERT INTO t (id, grp, body) VALUES (?, ?, ?)",
            &[
                Value::Integer(ROWS + 1),
                Value::Integer(0),
                Value::Text("after".into()),
            ],
        )
        .expect("and can still write");
    let count = engine.query("SELECT count(*) FROM t", &[]);
    // Counting is itself a blocking operator, so it is refused under this
    // ceiling too — asked here only to show the refusal is about *memory* and
    // not about the handle having been poisoned.
    assert!(matches!(count, Err(Error::Memory(_))));
    let after = engine
        .query(
            "SELECT body FROM t WHERE id = ?",
            &[Value::Integer(ROWS + 1)],
        )
        .expect("the write landed");
    assert_eq!(after.rows, vec![vec![Value::Text("after".into())]]);
}

/// A sort the planner can push a small `LIMIT` into still has to hold every
/// row it might choose from — that is what makes a sort a sort — so this is
/// not a way around the ceiling, and the test exists so nobody assumes it is.
#[test]
fn a_limit_does_not_exempt_a_sort_from_the_ceiling() {
    let mut engine = seeded(ROWS as usize * APPROX_ROW_BYTES / 8);
    refusal(&mut engine, "SELECT id, body FROM t ORDER BY body LIMIT 1");
}
