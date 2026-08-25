//! `INTEGER` is 64 bits, and comparisons have to treat it that way.
//!
//! An `f64` represents every integer up to 2^53 exactly and then starts
//! skipping: 2^53 and 2^53 + 1 are the same `f64`. So any comparison that
//! widens two `INTEGER`s to `f64` before comparing them stops being able to
//! tell large ids apart. This is not a corner case in practice — Snowflake
//! ids, Twitter-style ids, epoch nanoseconds and most external-system ids all
//! live above 2^53, and a column holding them is exactly the column an
//! application filters and joins on.
//!
//! `mem_cmp` (`ORDER BY`, `DISTINCT`, index order) always had an exact
//! two-integer case. The filter path (`compare_cells`) and the unique-index
//! collision check did not, so the engine disagreed with itself: a row could
//! sort as distinct and filter as equal.

use inlaysql::{Database, Value};

/// 2^53. The first integer whose successor an `f64` cannot represent.
const PIVOT: i64 = 9_007_199_254_740_992;

fn open() -> Database {
    let mut db = Database::open_in_memory().expect("open");
    db.execute("CREATE TABLE ids (id INTEGER PRIMARY KEY, tag TEXT)", &[])
        .expect("create");
    db
}

fn ids_matching(db: &mut Database, sql: &str, param: i64) -> Vec<i64> {
    db.query(sql, &[Value::Integer(param)])
        .expect("query")
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("unexpected value {other:?}"),
        })
        .collect()
}

/// The headline: equality must not confuse two adjacent large integers.
#[test]
fn equality_distinguishes_integers_above_two_to_the_53() {
    let mut db = open();
    for (id, tag) in [(PIVOT, "pivot"), (PIVOT + 1, "pivot plus one")] {
        db.execute(
            "INSERT INTO ids (id, tag) VALUES (?, ?)",
            &[Value::Integer(id), Value::Text(tag.into())],
        )
        .expect("insert");
    }

    assert_eq!(
        ids_matching(&mut db, "SELECT id FROM ids WHERE id = ?", PIVOT),
        vec![PIVOT],
        "`= 2^53` matched something other than exactly the row holding 2^53"
    );
    assert_eq!(
        ids_matching(&mut db, "SELECT id FROM ids WHERE id = ?", PIVOT + 1),
        vec![PIVOT + 1],
        "`= 2^53 + 1` matched something other than exactly the row holding it"
    );
}

/// Ordering comparisons have to separate them too, not just equality.
#[test]
fn ordering_comparisons_separate_adjacent_large_integers() {
    let mut db = open();
    for id in [PIVOT, PIVOT + 1, PIVOT + 2] {
        db.execute(
            "INSERT INTO ids (id, tag) VALUES (?, 'x')",
            &[Value::Integer(id)],
        )
        .expect("insert");
    }

    assert_eq!(
        ids_matching(
            &mut db,
            "SELECT id FROM ids WHERE id > ? ORDER BY id",
            PIVOT
        ),
        vec![PIVOT + 1, PIVOT + 2],
        "`> 2^53` did not exclude exactly 2^53"
    );
    assert_eq!(
        ids_matching(
            &mut db,
            "SELECT id FROM ids WHERE id <= ? ORDER BY id",
            PIVOT + 1
        ),
        vec![PIVOT, PIVOT + 1],
        "`<= 2^53 + 1` did not include exactly the two rows at or below it"
    );
}

/// The filter and the sort must agree. `mem_cmp` has always compared two
/// integers exactly, so before this was fixed a pair of rows could be distinct
/// to `ORDER BY`/`DISTINCT` and equal to `WHERE` at the same time.
#[test]
fn the_filter_agrees_with_the_sort_on_large_integers() {
    let mut db = open();
    for id in [PIVOT, PIVOT + 1] {
        db.execute(
            "INSERT INTO ids (id, tag) VALUES (?, 'x')",
            &[Value::Integer(id)],
        )
        .expect("insert");
    }

    let distinct = db
        .query("SELECT DISTINCT id FROM ids ORDER BY id", &[])
        .expect("distinct")
        .rows
        .len();
    assert_eq!(distinct, 2, "DISTINCT collapsed two different integers");

    let filtered = ids_matching(&mut db, "SELECT id FROM ids WHERE id = ?", PIVOT).len();
    assert_eq!(
        filtered, 1,
        "the filter matched both rows that DISTINCT kept apart"
    );
}

/// A `UNIQUE` index must not refuse a row whose key differs from an existing
/// one only above 2^53. This is the same widening in the collision check, and
/// its symptom is the opposite: a spurious duplicate-key error on data that is
/// not duplicated.
#[test]
fn a_unique_index_admits_adjacent_large_integers() {
    let mut db = Database::open_in_memory().expect("open");
    db.execute(
        "CREATE TABLE external (id INTEGER PRIMARY KEY, ref INTEGER)",
        &[],
    )
    .expect("create");
    db.execute("CREATE UNIQUE INDEX external_ref ON external (ref)", &[])
        .expect("create index");

    db.execute(
        "INSERT INTO external (id, ref) VALUES (1, ?)",
        &[Value::Integer(PIVOT)],
    )
    .expect("first row");

    db.execute(
        "INSERT INTO external (id, ref) VALUES (2, ?)",
        &[Value::Integer(PIVOT + 1)],
    )
    .expect("a distinct id one above 2^53 was refused as a duplicate");

    // And the constraint still does its job on a genuine duplicate.
    assert!(
        db.execute(
            "INSERT INTO external (id, ref) VALUES (3, ?)",
            &[Value::Integer(PIVOT)],
        )
        .is_err(),
        "a real duplicate was accepted"
    );
}

/// The extremes, where a widening would also lose the distinction.
///
/// These go in an ordinary column rather than the primary key: an `INTEGER
/// PRIMARY KEY` is the row id here and must be positive, so `i64::MIN` is
/// refused before any comparison happens.
#[test]
fn the_ends_of_the_range_survive_a_round_trip_and_a_filter() {
    const EXTREMES: [i64; 4] = [i64::MIN, i64::MIN + 1, i64::MAX - 1, i64::MAX];

    let mut db = Database::open_in_memory().expect("open");
    db.execute(
        "CREATE TABLE edges (id INTEGER PRIMARY KEY, value INTEGER)",
        &[],
    )
    .expect("create");
    for (position, value) in EXTREMES.iter().enumerate() {
        db.execute(
            "INSERT INTO edges (id, value) VALUES (?, ?)",
            &[Value::Integer(position as i64 + 1), Value::Integer(*value)],
        )
        .expect("insert");
    }

    for value in EXTREMES {
        let matched: Vec<i64> = db
            .query(
                "SELECT value FROM edges WHERE value = ?",
                &[Value::Integer(value)],
            )
            .expect("query")
            .rows
            .iter()
            .map(|row| match row[0] {
                Value::Integer(found) => found,
                ref other => panic!("unexpected value {other:?}"),
            })
            .collect();
        assert_eq!(
            matched,
            vec![value],
            "`= {value}` did not match exactly its own row"
        );
    }
}
