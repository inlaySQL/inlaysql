//! `WITH RECURSIVE`.
//!
//! The engine has no incremental single-row execution the way sqlite3's VDBE
//! does — a query either scans a stored table or materialises a derived
//! one — so this runs by semi-naive iteration instead: the seed runs once,
//! then the recursive term runs repeatedly, each time seeing only the
//! previous step's *new* rows (`Engine::run_recursive`), until a step adds
//! nothing new. Every expectation here was checked against a real sqlite3
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

fn ints(db: &mut Database, sql: &str) -> Vec<i64> {
    db.query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .rows
        .into_iter()
        .map(|row| match row.into_iter().next() {
            Some(Value::Integer(n)) => n,
            other => panic!("expected one integer column, got {other:?}"),
        })
        .collect()
}

/// Verified against sqlite3: a `UNION ALL` counter generates its own
/// sequence, stopping when the `WHERE` guard on the recursive term stops
/// producing rows.
#[test]
fn a_bounded_counter_generates_its_own_sequence() {
    let (_disk, mut db) = opened();
    let rows = ints(
        &mut db,
        "WITH RECURSIVE cnt(x) AS (\
           SELECT 1 \
           UNION ALL \
           SELECT x + 1 FROM cnt WHERE x < 5\
         ) SELECT x FROM cnt",
    );
    assert_eq!(rows, vec![1, 2, 3, 4, 5]);
}

/// Verified against sqlite3: under `UNION` (not `ALL`), a row that repeats
/// one already produced is dropped from the *next* step's frontier too, not
/// only from the final output — without that, `(x + 1) % 3` cycles forever.
/// sqlite3 returns exactly these three rows and stops.
#[test]
fn union_drops_a_repeated_row_from_the_next_steps_frontier_too() {
    let (_disk, mut db) = opened();
    let rows = ints(
        &mut db,
        "WITH RECURSIVE cnt(x) AS (\
           SELECT 1 \
           UNION \
           SELECT (x + 1) % 3 FROM cnt WHERE x < 10\
         ) SELECT x FROM cnt",
    );
    assert_eq!(rows, vec![1, 2, 0]);
}

/// Verified against sqlite3: a graph-reachability query, the canonical
/// `WITH RECURSIVE` use case — the recursive term joins the CTE against a
/// real table, and `UNION` is what makes it terminate on a graph with no
/// natural upper bound (no `WHERE x < n` guard at all).
#[test]
fn a_graph_reachability_query_terminates_on_union_alone() {
    let (_disk, mut db) = opened();
    db.execute("CREATE TABLE edges (src INTEGER, dst INTEGER)", &[])
        .expect("create");
    db.execute(
        "INSERT INTO edges VALUES (1, 2), (2, 3), (3, 4), (4, 2)",
        &[],
    )
    .expect("insert");
    let rows = ints(
        &mut db,
        "WITH RECURSIVE reach(n) AS (\
           SELECT 1 \
           UNION \
           SELECT e.dst FROM edges e JOIN reach r ON e.src = r.n\
         ) SELECT n FROM reach ORDER BY n",
    );
    assert_eq!(rows, vec![1, 2, 3, 4]);
}

/// Verified against sqlite3: `WITH RECURSIVE t(a) AS (SELECT 1), cnt(x) AS
/// (...)` — a `WITH RECURSIVE` list may mix a member that never references
/// itself with one that does.
#[test]
fn a_non_recursive_member_of_a_recursive_with_list_still_plans_ordinarily() {
    let (_disk, mut db) = opened();
    let rows = db
        .query(
            "WITH RECURSIVE t(a) AS (SELECT 1), cnt(x) AS (\
               SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 3\
             ) SELECT a, x FROM t, cnt ORDER BY x",
            &[],
        )
        .expect("query");
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(1), Value::Integer(3)],
        ]
    );
}

/// The single most common shape this feature is used for, and the one that
/// would hang without `Engine::run_recursive`'s `LIMIT` short-circuit: no
/// `WHERE` guard on the recursive term at all, relying entirely on the
/// outer `LIMIT` to end it. Verified against sqlite3, which returns at once
/// here rather than running forever.
///
/// `x < 1_000_000` is a backstop, not the mechanism under test: if the
/// `LIMIT` pushdown ever regresses, this still terminates (slower, having
/// done a bounded amount of extra work) instead of hanging the test suite.
#[test]
fn a_limit_with_no_where_guard_ends_the_recursion_early() {
    let (_disk, mut db) = opened();
    let rows = ints(
        &mut db,
        "WITH RECURSIVE cnt(x) AS (\
           SELECT 1 \
           UNION ALL \
           SELECT x + 1 FROM cnt WHERE x < 1000000\
         ) SELECT x FROM cnt LIMIT 5",
    );
    assert_eq!(rows, vec![1, 2, 3, 4, 5]);
}

/// A `WHERE` on the *outer* query does not get the same short-circuit — see
/// `Engine::derived_stream`'s doc for why that is the same, already-accepted
/// policy an ordinary derived table has. This only checks correctness, not
/// speed: the recursive term's own guard is what actually bounds the work.
#[test]
fn an_outer_where_filters_the_fully_materialised_result() {
    let (_disk, mut db) = opened();
    let rows = ints(
        &mut db,
        "WITH RECURSIVE cnt(x) AS (\
           SELECT 1 \
           UNION ALL \
           SELECT x + 1 FROM cnt WHERE x < 5\
         ) SELECT x FROM cnt WHERE x > 2",
    );
    assert_eq!(rows, vec![3, 4, 5]);
}

/// Verified against sqlite3 ("multiple references to recursive table"): the
/// recursive term may name the CTE only once in its `FROM`.
#[test]
fn the_recursive_term_may_reference_the_cte_only_once() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "WITH RECURSIVE cnt(x) AS (\
               SELECT 1 \
               UNION ALL \
               SELECT a.x + b.x FROM cnt a, cnt b WHERE a.x < 3\
             ) SELECT x FROM cnt",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("once"),
        "expected a message about a single reference, got: {error}"
    );
}

/// Verified against sqlite3 ("circular reference"): the seed may not
/// reference the CTE it is seeding.
#[test]
fn the_seed_may_not_reference_the_cte() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "WITH RECURSIVE cnt(x) AS (\
               SELECT x FROM cnt \
               UNION ALL \
               SELECT x + 1 FROM cnt WHERE x < 5\
             ) SELECT x FROM cnt",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("not yet defined") || error.to_string().contains("circular"),
        "expected a self-reference refusal, got: {error}"
    );
}

/// Verified against sqlite3 ("circular reference"): `INTERSECT`/`EXCEPT`
/// cannot combine a seed with a recursive term — neither has a meaning for
/// "keep going until a step adds nothing new".
#[test]
fn intersect_may_not_combine_a_seed_with_a_recursive_term() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "WITH RECURSIVE cnt(x) AS (\
               SELECT 1 \
               INTERSECT \
               SELECT x + 1 FROM cnt WHERE x < 5\
             ) SELECT x FROM cnt",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("UNION"),
        "expected a refusal naming UNION/UNION ALL, got: {error}"
    );
}

/// Not a sqlite3-matching rule but a real limit of semi-naive iteration: a
/// step only ever sees that step's new rows, never the whole table, so an
/// aggregate over the recursive term cannot mean what it would over a plain
/// table.
#[test]
fn the_recursive_term_may_not_use_an_aggregate() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "WITH RECURSIVE cnt(x) AS (\
               SELECT 1 \
               UNION ALL \
               SELECT count(*) FROM cnt WHERE x < 5\
             ) SELECT x FROM cnt",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("aggregate"),
        "expected a refusal naming the aggregate restriction, got: {error}"
    );
}

/// A bare `SELECT` with no compound at all can never be legally recursive —
/// there is nothing to seed from — so this is refused the same way an
/// ordinary forward/self-reference is, not specially. Verified against
/// sqlite3 ("circular reference").
#[test]
fn a_bare_select_with_no_seed_cannot_be_recursive() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "WITH RECURSIVE cnt(x) AS (SELECT x + 1 FROM cnt) SELECT x FROM cnt",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("not yet defined") || error.to_string().contains("circular"),
        "expected a self-reference refusal, got: {error}"
    );
}

/// The recursive term's column count must match the seed's, the same rule
/// an ordinary compound arm follows.
#[test]
fn the_recursive_term_must_match_the_seeds_column_count() {
    let (_disk, mut db) = opened();
    let error = db
        .execute(
            "WITH RECURSIVE cnt(x) AS (\
               SELECT 1 \
               UNION ALL \
               SELECT x, x + 1 FROM cnt WHERE x < 5\
             ) SELECT x FROM cnt",
            &[],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("number of result"),
        "expected a column-count mismatch refusal, got: {error}"
    );
}
