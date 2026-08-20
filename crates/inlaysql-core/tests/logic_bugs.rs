//! Metamorphic tests for logic bugs: queries that must agree with each other,
//! whatever the right answer is.
//!
//! # Why this shape of test
//!
//! A crash announces itself. A *logic* bug — a `WHERE` clause that silently
//! drops a row, an `ORDER BY` that puts one in the wrong place — does not. You
//! cannot catch it with an oracle unless you have one, and writing an oracle
//! means writing a second database.
//!
//! [SQLancer](https://github.com/sqlancer/sqlancer) solved that by comparing a
//! database against *itself*: generate a random table and a random predicate,
//! then ask questions whose answers must be related no matter what the data is.
//! These tests apply the two techniques that fit the current dialect.
//!
//! **TLP (ternary logic partitioning).** SQL's three-valued logic means every
//! row satisfies exactly one of `p`, `NOT p`, `p IS NULL`. So the three result
//! sets must partition the table: together they are everything, and they never
//! overlap. If a predicate loses a row, one of those two properties breaks.
//!
//! Writing this test is what found that the dialect had neither `NOT` nor
//! `IS NULL`, which is a fair advertisement for the technique: the property
//! cannot even be *expressed* without them.
//!
//! **PQS-style row retrieval.** Pick a row that is known to exist, build a
//! predicate that is true for it by construction, and check the query returns
//! it. A row the engine can scan but cannot find through a filter is a bug
//! nothing else here would notice.
//!
//! # What this is not
//!
//! It is not SQLancer. SQLancer is a Java tool with years of generator tuning,
//! and pointing it at InlaySQL is worthwhile work that is not done. This is the
//! same *idea*, in-repo, on every `cargo test`, over a dialect small enough
//! that the generator fits on a screen. `TESTING.md` says so plainly rather
//! than letting the label do work the code has not done.

use inlaysql_core::mem::SeededRng;
use inlaysql_core::{Engine, Rng, Value};

/// Columns the generated predicates range over.
const COLUMNS: &[&str] = &["a", "b"];
const OPERATORS: &[&str] = &["=", "<>", "<", "<=", ">", ">="];

/// A table of `rows` rows over two nullable integer columns.
///
/// Nulls are the point: three-valued logic is where a SQL engine's filters go
/// wrong, and a table without them exercises none of it.
fn populated(rng: &mut SeededRng, rows: usize) -> Engine {
    let mut engine = inlaysql_core::mem::engine().expect("engine");
    engine
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
            &[],
        )
        .unwrap();
    for id in 1..=rows as i64 {
        let value = |rng: &mut SeededRng| match rng.next_u64() % 4 {
            0 => Value::Null,
            _ => Value::Integer((rng.next_u64() % 7) as i64 - 3),
        };
        let (a, b) = (value(rng), value(rng));
        engine
            .execute(
                "INSERT INTO t (id, a, b) VALUES (?, ?, ?)",
                &[Value::Integer(id), a, b],
            )
            .unwrap();
    }
    engine
}

/// A random predicate over the table's columns.
fn predicate(rng: &mut SeededRng, depth: usize) -> String {
    let column = |rng: &mut SeededRng| COLUMNS[(rng.next_u64() % COLUMNS.len() as u64) as usize];
    let operator =
        |rng: &mut SeededRng| OPERATORS[(rng.next_u64() % OPERATORS.len() as u64) as usize];

    if depth == 0 || rng.next_u64().is_multiple_of(3) {
        return match rng.next_u64() % 3 {
            // column <op> literal
            0 => format!(
                "{} {} {}",
                column(rng),
                operator(rng),
                (rng.next_u64() % 7) as i64 - 3
            ),
            // column <op> column
            1 => format!("{} {} {}", column(rng), operator(rng), column(rng)),
            // arithmetic, which is where integer division and NULL propagation
            // meet the comparison rules
            _ => format!(
                "{} + {} {} {}",
                column(rng),
                (rng.next_u64() % 5) as i64 - 2,
                operator(rng),
                column(rng)
            ),
        };
    }

    let (left, right) = (predicate(rng, depth - 1), predicate(rng, depth - 1));
    match rng.next_u64() % 2 {
        0 => format!("({left}) AND ({right})"),
        _ => format!("({left}) OR ({right})"),
    }
}

/// The ids a filter selects.
fn ids(engine: &mut Engine, filter: &str) -> Vec<i64> {
    engine
        .query(&format!("SELECT id FROM t WHERE {filter}"), &[])
        .unwrap_or_else(|error| panic!("`{filter}` failed: {error}"))
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("expected an integer id, got {other:?}"),
        })
        .collect()
}

#[test]
fn a_predicate_and_its_negation_and_its_nulls_partition_the_table() {
    let mut rng = SeededRng::new(0x7137);
    for round in 0..300 {
        let mut engine = populated(&mut rng, 16);
        let all: Vec<i64> = ids(&mut engine, "1 = 1");
        let p = predicate(&mut rng, 2);

        let matched = ids(&mut engine, &p);
        let negated = ids(&mut engine, &format!("NOT ({p})"));
        // `IS NULL` is the only way to ask about the unknown: `NOT (p OR NOT p)`
        // would itself be unknown for exactly the rows it is trying to find.
        let unknown = ids(&mut engine, &format!("({p}) IS NULL"));

        let mut union: Vec<i64> = matched
            .iter()
            .chain(&negated)
            .chain(&unknown)
            .copied()
            .collect();
        union.sort_unstable();
        let mut deduped = union.clone();
        deduped.dedup();

        assert_eq!(
            union, deduped,
            "round {round}: a row satisfies more than one of p / NOT p / unknown\n\
             predicate: {p}\n  p: {matched:?}\n  NOT p: {negated:?}\n  unknown: {unknown:?}"
        );
        assert_eq!(
            union, all,
            "round {round}: the three partitions do not cover the table\n\
             predicate: {p}\n  p: {matched:?}\n  NOT p: {negated:?}\n  unknown: {unknown:?}"
        );
    }
}

#[test]
fn a_row_that_exists_can_be_found_by_a_predicate_true_of_it() {
    let mut rng = SeededRng::new(0xF1AD);
    for round in 0..300 {
        let mut engine = populated(&mut rng, 12);
        let rows = engine.query("SELECT id, a, b FROM t", &[]).unwrap();
        let Some(row) = rows
            .rows
            .get((rng.next_u64() % rows.rows.len() as u64) as usize)
        else {
            continue;
        };
        let (Value::Integer(id), a, b) = (&row[0], &row[1], &row[2]) else {
            panic!("expected an integer id");
        };

        // A predicate that is true for this row by construction. NULL columns
        // are skipped: nothing compares true to NULL, which is correct rather
        // than a bug to find.
        let mut conjuncts = Vec::new();
        if let Value::Integer(a) = a {
            conjuncts.push(format!("a = {a}"));
        }
        if let Value::Integer(b) = b {
            conjuncts.push(format!("b = {b}"));
        }
        if conjuncts.is_empty() {
            continue;
        }
        let filter = conjuncts.join(" AND ");

        assert!(
            ids(&mut engine, &filter).contains(id),
            "round {round}: row {id} exists but `WHERE {filter}` does not return it"
        );
    }
}

#[test]
fn a_filter_never_returns_a_row_the_table_does_not_have() {
    let mut rng = SeededRng::new(0x0DD1);
    for round in 0..200 {
        let mut engine = populated(&mut rng, 16);
        let all = ids(&mut engine, "1 = 1");
        let p = predicate(&mut rng, 2);
        for id in ids(&mut engine, &p) {
            assert!(
                all.contains(&id),
                "round {round}: `WHERE {p}` returned row {id}, which is not in the table"
            );
        }
    }
}

#[test]
fn ordering_a_result_does_not_change_which_rows_it_contains() {
    // A sort that drops or duplicates a row is a classic logic bug, and one
    // that looks fine in any single query's output.
    let mut rng = SeededRng::new(0x5017);
    for round in 0..200 {
        let mut engine = populated(&mut rng, 16);
        let p = predicate(&mut rng, 2);

        let mut unordered = ids(&mut engine, &p);
        let ordered: Vec<i64> = engine
            .query(&format!("SELECT id FROM t WHERE {p} ORDER BY a DESC"), &[])
            .unwrap()
            .rows
            .iter()
            .map(|row| match row[0] {
                Value::Integer(id) => id,
                ref other => panic!("{other:?}"),
            })
            .collect();

        let mut sorted = ordered.clone();
        unordered.sort_unstable();
        sorted.sort_unstable();
        assert_eq!(
            unordered, sorted,
            "round {round}: ORDER BY changed the result set for `{p}`"
        );
    }
}
