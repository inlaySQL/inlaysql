//! The two SQL functions that are not pure, held to the seam that makes them
//! reproducible anyway.
//!
//! `random()` and `datetime('now')` are the only places where a query's answer
//! depends on something outside the database. `inlaysql-core` is `no_std`, so
//! neither can reach the host: the time arrives through
//! [`inlaysql_core::Clock`] and the randomness through [`inlaysql_core::Rng`],
//! and both are injected when the engine is opened.
//!
//! That is not a style preference. The deterministic simulation replays a
//! workload byte for byte from a seed, and the `determinism` job in CI fails
//! the build if an OS-facing crate enters core's dependency tree. A `random()`
//! that called into the host would pass `cargo test` and quietly make every
//! DST sweep unreproducible — the failure would show up as a seed that no
//! longer reproduces, months later, in a test of something else entirely.
//!
//! So these tests assert the property directly: same environment, same answer.

use std::cell::Cell;
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage, SeededRng};
use inlaysql_core::{Clock, Engine, Value};

/// An engine over the in-memory environment, with the clock started at a fixed
/// instant and a chosen generator.
///
/// `start_micros` is microseconds since the Unix epoch, which is what the
/// production clock reports and what the date functions read.
fn engine_at(start_micros: i64, seed: u64) -> Engine {
    let mut engine = Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        // A tick of zero keeps every reading inside one run identical, so a
        // test asserting a formatted timestamp is not racing the clock it
        // injected.
        Box::new(LogicalClock::with_tick(start_micros, 0)),
    )
    .expect("open");
    engine.set_rng(Box::new(SeededRng::new(seed)));
    engine
}

fn scalar(engine: &mut Engine, sql: &str) -> Value {
    let result = engine.query(sql, &[]).expect("query");
    result.rows[0][0].clone()
}

#[test]
fn the_clock_reaches_sql_only_through_the_injected_trait() {
    // 2001-09-09T01:46:40Z, chosen because every field is distinct.
    let mut engine = engine_at(1_000_000_000_000_000, 1);

    assert_eq!(
        scalar(&mut engine, "SELECT datetime('now')"),
        Value::Text("2001-09-09 01:46:40".to_string().into())
    );
    assert_eq!(
        scalar(&mut engine, "SELECT date('now')"),
        Value::Text("2001-09-09".to_string().into())
    );
    assert_eq!(
        scalar(&mut engine, "SELECT time('now')"),
        Value::Text("01:46:40".to_string().into())
    );
    assert_eq!(
        scalar(&mut engine, "SELECT unixepoch('now')"),
        Value::Integer(1_000_000_000)
    );
    assert_eq!(
        scalar(&mut engine, "SELECT strftime('%Y-%m-%d', 'now')"),
        Value::Text("2001-09-09".to_string().into())
    );

    // The bare keyword forms read the same clock as the functions.
    assert_eq!(
        scalar(&mut engine, "SELECT CURRENT_TIMESTAMP"),
        Value::Text("2001-09-09 01:46:40".to_string().into())
    );
    assert_eq!(
        scalar(&mut engine, "SELECT CURRENT_DATE"),
        Value::Text("2001-09-09".to_string().into())
    );
    assert_eq!(
        scalar(&mut engine, "SELECT CURRENT_TIME"),
        Value::Text("01:46:40".to_string().into())
    );
}

#[test]
fn a_different_injected_clock_gives_a_different_answer() {
    // If the engine were reading the host clock, moving the injected one would
    // change nothing — which is exactly the bug this guards against.
    let mut engine = engine_at(0, 1);
    assert_eq!(
        scalar(&mut engine, "SELECT date('now')"),
        Value::Text("1970-01-01".to_string().into())
    );
}

#[test]
fn one_statement_sees_one_instant() {
    // SQLite caches the time for the duration of a statement, so two `'now'`s
    // in one query cannot straddle a second boundary. A clock that ticks on
    // every read would break that, and this engine's logical clock does tick
    // by default — so the property has to be the engine's, not the clock's.
    let mut engine = Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        // One second per reading: if the statement read the clock twice, the
        // two columns would differ by a second and the assertion would fail.
        Box::new(LogicalClock::with_tick(1_000_000_000_000_000, 1_000_000)),
    )
    .expect("open");

    let result = engine
        .query("SELECT datetime('now'), datetime('now')", &[])
        .expect("query");
    assert_eq!(result.rows[0][0], result.rows[0][1]);
}

#[test]
fn random_replays_from_the_injected_generator() {
    let draw = |seed: u64| {
        let mut engine = engine_at(0, seed);
        let mut values = Vec::new();
        for _ in 0..8 {
            values.push(scalar(&mut engine, "SELECT random()"));
        }
        values
    };

    // Same seed, same stream: this is the property a DST replay rests on.
    assert_eq!(draw(7), draw(7));
    // Different seed, different stream — otherwise the generator is not being
    // consulted at all.
    assert_ne!(draw(7), draw(8));

    // And it does move: eight draws that were all equal would satisfy the two
    // assertions above while being useless.
    let values = draw(7);
    assert!(
        values.iter().any(|value| *value != values[0]),
        "random() returned the same value eight times: {values:?}"
    );
}

#[test]
fn random_is_evaluated_per_row_not_once() {
    let mut engine = engine_at(0, 42);
    engine
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
        .unwrap();
    for id in 1..=32i64 {
        engine
            .execute("INSERT INTO t (id) VALUES (?)", &[Value::Integer(id)])
            .unwrap();
    }

    let result = engine.query("SELECT random() FROM t", &[]).expect("query");
    assert_eq!(result.rows.len(), 32);
    let distinct = {
        let mut values: Vec<String> = result.rows.iter().map(|row| alloc_debug(&row[0])).collect();
        values.sort();
        values.dedup();
        values.len()
    };
    // A constant-folded `random()` would give one value for all 32 rows.
    assert!(
        distinct > 24,
        "random() produced only {distinct} distinct values over 32 rows"
    );
}

#[test]
fn random_never_returns_the_value_whose_negation_is_itself() {
    // SQLite masks the sign bit rather than negating, so `random()` is never
    // `i64::MIN`. Draw enough that a naive implementation would have hit it.
    let mut engine = engine_at(0, 3);
    for _ in 0..2000 {
        assert_ne!(
            scalar(&mut engine, "SELECT random()"),
            Value::Integer(i64::MIN)
        );
    }
}

fn alloc_debug(value: &Value) -> String {
    format!("{value:?}")
}

/// A clock that counts how many times it was read, so a test can assert not
/// just what a statement saw but whether it looked at all.
struct CountingClock {
    reads: Rc<Cell<usize>>,
}

impl Clock for CountingClock {
    fn now_micros(&self) -> i64 {
        self.reads.set(self.reads.get() + 1);
        // A moving value, so a statement that took two readings would show two
        // different instants rather than accidentally agreeing.
        self.reads.get() as i64 * 1_000_000
    }
}

/// The clock is read once per statement *at most*, and only by a statement
/// that asks for the time.
///
/// Both halves matter. Reading it eagerly on every statement put a clock call
/// on the primary-key point read, which does not mention the time and cannot
/// observe it (AHL-527). Reading it more than once inside one statement would
/// let two `'now'`s in the same query disagree, which is the property
/// `sqlite3StmtCurrentTime` exists to hold.
#[test]
fn only_a_statement_that_asks_for_the_time_reads_the_clock() {
    let reads = Rc::new(Cell::new(0usize));
    let mut engine = Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(CountingClock {
            reads: Rc::clone(&reads),
        }),
    )
    .expect("open");
    engine
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    engine
        .execute("INSERT INTO kv (id, body) VALUES (1, 'a')", &[])
        .unwrap();

    // Whatever opening and populating the database cost, it is behind us.
    let before = reads.get();
    for _ in 0..16 {
        assert_eq!(
            engine
                .query("SELECT body FROM kv WHERE id = ?", &[Value::Integer(1)])
                .expect("query")
                .rows
                .len(),
            1
        );
    }
    assert_eq!(
        reads.get(),
        before,
        "a point read that never mentions the time must not read the clock"
    );

    // And one that does ask reads it exactly once, however many times it asks.
    let row = engine
        .query(
            "SELECT unixepoch('now'), unixepoch('now'), unixepoch(datetime('now'))",
            &[],
        )
        .expect("query")
        .rows
        .remove(0);
    assert_eq!(
        reads.get(),
        before + 1,
        "three time functions in one statement must share one reading"
    );
    assert_eq!(row[0], row[1]);
    assert_eq!(row[1], row[2]);
}
