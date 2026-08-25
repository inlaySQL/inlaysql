//! Stopping a statement, and what it is allowed to leave behind.
//!
//! The core cannot time a statement out or hear a `KILL` — it is `no_std`, so
//! it has no clock and no thread that could interrupt one. What it has is
//! [`inlaysql_core::Cancel`], the same seam as the [`inlaysql_core::Clock`]
//! beside it: the host answers, and the core asks from inside every loop that
//! can run long.
//!
//! The interesting half of this is not that a statement stops. It is what the
//! database looks like afterwards, and the bar is exact: **a cancelled
//! statement must leave the database in the state an un-run one would.** A
//! `COMMIT` refused for size used to strand its write set and brick the handle
//! (`a_commit_refused_for_size_leaves_a_usable_handle`, in `transactions.rs`);
//! the same class of bug is available to anything that fails part-way through
//! a write. So the tests here do not check one convenient cancellation point —
//! they sweep every one, by tripping the signal on the first question, then
//! the second, and so on until the statement runs to completion, asserting the
//! same two things at every stop: nothing was written, and the handle still
//! works.

use std::cell::Cell;
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::{Cancel, Engine, Error, Outcome, Statement, Stopped, Value};

/// A cancellation signal that trips on the `budget`-th question and not before.
///
/// Counting *questions* rather than rows or milliseconds is what keeps these
/// tests independent of how often the engine decides to ask: the sweeps below
/// walk the budget upward until the statement finishes, so they cover every
/// point the engine can be stopped at whatever the stride happens to be.
struct Trip {
    /// Questions still to answer "carry on" before answering "stop". `None`
    /// never stops.
    budget: Cell<Option<u64>>,
    /// How many times the engine has asked since the last arming.
    asked: Cell<u64>,
    /// How many statements have been armed.
    statements: Cell<u64>,
    /// What to answer when it trips.
    reason: Cell<Stopped>,
}

impl Trip {
    fn new() -> Rc<Self> {
        Rc::new(Trip {
            budget: Cell::new(None),
            asked: Cell::new(0),
            statements: Cell::new(0),
            reason: Cell::new(Stopped::Killed),
        })
    }

    /// Answer "carry on" `budget` times and then "stop".
    fn trip_after(&self, budget: u64) {
        self.budget.set(Some(budget));
        self.asked.set(0);
    }

    fn never_trip(&self) {
        self.budget.set(None);
        self.asked.set(0);
    }
}

/// The engine's half of a [`Trip`] — a separate type because the engine owns
/// what it is given and the test keeps a share of it.
struct Signal(Rc<Trip>);

impl Cancel for Signal {
    fn statement_began(&self) {
        self.0.statements.set(self.0.statements.get() + 1);
    }

    fn stop(&self) -> Option<Stopped> {
        self.0.asked.set(self.0.asked.get() + 1);
        match self.0.budget.get() {
            None => None,
            Some(0) => Some(self.0.reason.get()),
            Some(left) => {
                self.0.budget.set(Some(left - 1));
                None
            }
        }
    }
}

/// How many rows the tables below hold. Several times the engine's check
/// stride, so a scan of one asks more than once and a sweep has more than one
/// cancellation point to find.
const ROWS: i64 = 4000;

fn open() -> Engine {
    Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::default()),
    )
    .expect("open")
}

/// An engine holding [`ROWS`] rows in `t`, with the signal wired into it.
fn seeded() -> (Engine, Rc<Trip>) {
    let mut engine = open();
    engine
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, body TEXT)",
            &[],
        )
        .expect("create t");
    fill(
        &mut engine,
        "INSERT INTO t (id, n, body) VALUES (?, ?, ?)",
        ROWS,
    );

    let trip = Trip::new();
    engine.set_cancel(Box::new(Signal(Rc::clone(&trip))));
    (engine, trip)
}

fn fill(engine: &mut Engine, sql: &str, rows: i64) {
    let insert = engine.prepare(sql).expect("prepare insert");
    engine.begin().expect("begin");
    for id in 1..=rows {
        engine
            .run(
                &insert,
                &[
                    Value::Integer(id),
                    Value::Integer(id),
                    Value::Text(format!("row {id}").into()),
                ],
            )
            .expect("insert");
    }
    engine.commit().expect("commit");
}

/// Every `(id, n, body)` in `t`, in row-id order — the whole observable state
/// the write tests compare before and after.
fn snapshot(engine: &mut Engine) -> Vec<(i64, i64, String)> {
    engine
        .query("SELECT id, n, body FROM t ORDER BY id", &[])
        .expect("snapshot")
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [Value::Integer(id), Value::Integer(n), Value::Text(body)] => {
                (*id, *n, body.to_string())
            }
            other => panic!("unexpected row shape: {other:?}"),
        })
        .collect()
}

fn cancelled(error: &Error) -> Stopped {
    match error {
        Error::Cancelled(reason) => *reason,
        other => panic!("expected a cancellation, got {other}"),
    }
}

/// How many times `sql` asks the host, run to completion with nothing tripping.
fn questions(engine: &mut Engine, trip: &Trip, sql: &str) -> u64 {
    trip.never_trip();
    let _ = engine
        .query(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    trip.asked.get()
}

/// The signal reaches a plain table scan, and the error says which of the two
/// reasons it was — a client acts differently on a timeout than on a `KILL`,
/// which is why the two are separate variants rather than one with a message.
#[test]
fn a_scan_stops_when_the_signal_says_so() {
    let (mut engine, trip) = seeded();

    trip.trip_after(0);
    let error = engine
        .query("SELECT id FROM t WHERE n > 0", &[])
        .expect_err("a scan past the stride must be stoppable");
    assert_eq!(cancelled(&error), Stopped::Killed);

    trip.reason.set(Stopped::Timeout);
    trip.trip_after(0);
    let error = engine
        .query("SELECT id FROM t WHERE n > 0", &[])
        .expect_err("a scan past the stride must be stoppable");
    assert_eq!(cancelled(&error), Stopped::Timeout);

    // With nothing tripping the same statement answers in full: the signal is
    // a gate, not a tax on correctness.
    trip.never_trip();
    assert_eq!(
        engine
            .query("SELECT id FROM t", &[])
            .expect("rows")
            .rows
            .len(),
        ROWS as usize
    );
}

/// Every shape that is *not* a plain scan has a loop of its own, and a check in
/// the scan alone would leave each of them free to run for as long as it liked
/// once the scan had finished.
///
/// The evidence is the number of questions asked, against a streaming scan of
/// the same rows as the baseline. A blocking operator reads its whole input and
/// then walks it again, so it must ask *more* than the scan that fed it; a join
/// pairs every outer row against a whole inner side, so it must ask far more.
/// An unchecked loop would come in at the baseline exactly.
#[test]
fn the_signal_reaches_sorts_aggregates_and_joins() {
    let (mut engine, trip) = seeded();
    engine
        .execute(
            "CREATE TABLE u (id INTEGER PRIMARY KEY, n INTEGER, body TEXT)",
            &[],
        )
        .expect("create u");
    fill(
        &mut engine,
        "INSERT INTO u (id, n, body) VALUES (?, ?, ?)",
        200,
    );

    // A streaming scan over every row of `t`, with no operator behind it.
    let baseline = questions(&mut engine, &trip, "SELECT id FROM t WHERE n > 0");
    assert!(baseline > 0, "the baseline scan never asked at all");

    for sql in [
        "SELECT id FROM t ORDER BY body",
        "SELECT n, COUNT(*) FROM t GROUP BY n",
        "SELECT DISTINCT n FROM t",
        // Not an equality, so the inner side is materialised and replayed:
        // 4000 outer rows against 200 inner ones is 800,000 pairs, none of
        // which the scan check can see.
        "SELECT t.id FROM t JOIN u ON t.n > u.n",
    ] {
        let asked = questions(&mut engine, &trip, sql);
        assert!(
            asked > baseline,
            "`{sql}` asked {asked} times, no more than the {baseline} a plain scan of the \
             same rows asks — its own loop is not checked"
        );
    }

    // And a join really can be stopped deep inside its pairing loop, past
    // anything the scan would have caught.
    trip.trip_after(baseline * 2);
    let error = engine
        .query("SELECT t.id FROM t JOIN u ON t.n > u.n", &[])
        .expect_err("a join must be stoppable while it is pairing");
    assert_eq!(cancelled(&error), Stopped::Killed);
}

/// The write half of an `UPDATE`, `DELETE` or `INSERT ... SELECT` is checked
/// as well as the scan that fed it.
///
/// This is the assertion the sweeps below cannot make. A write statement reads
/// its candidates first and changes them second, and the reading half is
/// already covered by the scan check — so a sweep would still find several
/// stopping points, and still find nothing written at any of them, with the
/// write loop completely unchecked. What separates the two is the *count*: a
/// statement that scans N rows and then writes N rows must ask about twice as
/// often as one that only scans them.
#[test]
fn the_write_loop_is_checked_as_well_as_the_scan_that_fed_it() {
    let (mut engine, trip) = seeded();
    let baseline = questions(&mut engine, &trip, "SELECT id FROM t WHERE n > 0");
    assert!(baseline > 0, "the baseline scan never asked at all");

    for sql in [
        "UPDATE t SET n = n + 1 WHERE n > 0",
        "INSERT INTO copy SELECT id, n, body FROM t",
        "DELETE FROM t WHERE n > 0",
    ] {
        if sql.starts_with("INSERT") {
            engine
                .execute(
                    "CREATE TABLE copy (id INTEGER PRIMARY KEY, n INTEGER, body TEXT)",
                    &[],
                )
                .expect("create copy");
        }
        trip.never_trip();
        engine
            .execute(sql, &[])
            .unwrap_or_else(|error| panic!("`{sql}`: {error}"));
        let asked = trip.asked.get();
        assert!(
            asked > baseline,
            "`{sql}` asked {asked} times, no more than the {baseline} its candidate scan \
             alone asks — the loop that writes is not checked, so a statement already \
             past its scan could not be stopped"
        );
    }
}

/// A cancelled `UPDATE` leaves the table exactly as an un-run one would, and
/// the handle keeps working — checked at *every* point the statement can be
/// stopped at, not at one convenient one.
///
/// The sweep is the point. A cancellation during the candidate scan proves
/// nothing about discarding writes; a cancellation during the write loop is the
/// one that would strand a half-applied statement, and only walking the budget
/// upward is guaranteed to reach it.
#[test]
fn a_cancelled_update_leaves_the_table_and_the_handle_as_it_found_them() {
    let (mut engine, trip) = seeded();
    let before = snapshot(&mut engine);
    assert_eq!(before.len(), ROWS as usize);

    let mut stops = 0;
    for budget in 0..64 {
        trip.trip_after(budget);
        let outcome = engine.execute("UPDATE t SET n = n + 1000, body = 'rewritten'", &[]);
        trip.never_trip();
        match outcome {
            Err(error) => {
                assert_eq!(cancelled(&error), Stopped::Killed, "budget {budget}");
                stops += 1;
            }
            Ok(outcome) => {
                // The budget outran the statement. That ends the sweep, and it
                // is also what proves the sweep covered the whole statement
                // rather than stopping short of the write loop.
                assert!(
                    matches!(outcome, Outcome::Written(n) if n == ROWS as usize),
                    "budget {budget}: {outcome:?}"
                );
                assert!(
                    stops > 1,
                    "the sweep found only {stops} stopping point(s); it cannot have reached \
                     the write loop"
                );
                return;
            }
        }
        // Nothing was written, and the handle is not poisoned — reading it back
        // is itself a statement, so this checks both at once.
        assert_eq!(
            snapshot(&mut engine),
            before,
            "budget {budget} left a write behind"
        );
    }
    panic!("the update never ran to completion; the sweep is too short");
}

/// The same sweep for `DELETE`, which removes rows and de-indexes them rather
/// than rewriting them, and so would leave a different kind of debris.
#[test]
fn a_cancelled_delete_leaves_every_row_where_it_was() {
    let (mut engine, trip) = seeded();
    let before = snapshot(&mut engine);

    for budget in 0..64 {
        trip.trip_after(budget);
        let outcome = engine.execute("DELETE FROM t WHERE n > 0", &[]);
        trip.never_trip();
        match outcome {
            Err(error) => assert_eq!(cancelled(&error), Stopped::Killed, "budget {budget}"),
            Ok(outcome) => {
                assert!(
                    matches!(outcome, Outcome::Written(n) if n == ROWS as usize),
                    "budget {budget}: {outcome:?}"
                );
                return;
            }
        }
        assert_eq!(
            snapshot(&mut engine),
            before,
            "budget {budget} deleted something"
        );
    }
    panic!("the delete never ran to completion; the sweep is too short");
}

/// A cancelled `INSERT ... SELECT` writes nothing, including none of the rows
/// it had already built and buffered.
#[test]
fn a_cancelled_insert_select_writes_no_rows() {
    let (mut engine, trip) = seeded();
    engine
        .execute(
            "CREATE TABLE copy (id INTEGER PRIMARY KEY, n INTEGER, body TEXT)",
            &[],
        )
        .expect("create copy");

    for budget in 0..64 {
        trip.trip_after(budget);
        let outcome = engine.execute("INSERT INTO copy SELECT id, n, body FROM t", &[]);
        trip.never_trip();
        let copied = engine
            .query("SELECT COUNT(*) FROM copy", &[])
            .expect("count")
            .rows[0][0]
            .clone();
        match outcome {
            Err(error) => {
                assert_eq!(cancelled(&error), Stopped::Killed, "budget {budget}");
                assert_eq!(
                    copied,
                    Value::Integer(0),
                    "budget {budget}: a cancelled INSERT ... SELECT left rows behind"
                );
            }
            Ok(_) => {
                assert_eq!(copied, Value::Integer(ROWS));
                return;
            }
        }
    }
    panic!("the insert never ran to completion; the sweep is too short");
}

/// Inside an explicit transaction the engine's rule for *any* failed statement
/// applies — the transaction is in an indeterminate state and `ROLLBACK` is how
/// to leave it — and cancellation does not get a special one. What this pins is
/// that the way out works: the rollback undoes everything the transaction did,
/// the cancelled statement included, and the handle takes new work afterwards.
#[test]
fn a_cancelled_statement_in_a_transaction_is_undone_by_rollback() {
    let (mut engine, trip) = seeded();
    let before = snapshot(&mut engine);

    engine.begin().expect("begin");
    engine
        .execute("UPDATE t SET body = 'first' WHERE id = 1", &[])
        .expect("a statement before the cancelled one");
    trip.trip_after(0);
    let error = engine
        .execute("UPDATE t SET n = n + 1000", &[])
        .expect_err("must be stoppable");
    assert_eq!(cancelled(&error), Stopped::Killed);
    trip.never_trip();

    // A cancelled statement is a failed statement, and failing one does not end
    // a transaction the caller began.
    engine
        .rollback()
        .expect("rollback after a cancelled statement");
    assert_eq!(
        snapshot(&mut engine),
        before,
        "the rollback left the cancelled statement's writes, or the earlier one's"
    );

    // And the handle takes new work, including a new transaction.
    engine.begin().expect("begin again");
    engine
        .execute("UPDATE t SET n = 0 WHERE id = 1", &[])
        .expect("write after a cancelled transaction");
    engine
        .commit()
        .expect("commit after a cancelled transaction");
    assert_eq!(snapshot(&mut engine)[0].1, 0);
}

/// One arming per statement, at the same moment the engine takes its one clock
/// reading. A deadline that covered two statements would fire on the wrong one,
/// and a `KILL QUERY` that arrived between two would fall on a statement that
/// was not running when it was issued.
#[test]
fn the_signal_is_armed_once_per_statement() {
    let (mut engine, trip) = seeded();
    let started = trip.statements.get();

    engine.query("SELECT 1", &[]).expect("scalar");
    engine.query("SELECT id FROM t LIMIT 1", &[]).expect("scan");
    engine
        .execute("UPDATE t SET n = n WHERE id = 1", &[])
        .expect("update");
    assert_eq!(trip.statements.get() - started, 3);

    // A prepared statement run many times is many statements, which is what a
    // per-statement deadline has to mean.
    let plan: Statement = engine
        .prepare("SELECT id FROM t WHERE id = ?")
        .expect("prepare");
    for id in 1..=5 {
        engine.run_query(&plan, &[Value::Integer(id)]).expect("run");
    }
    assert_eq!(trip.statements.get() - started, 8);
}

/// A statement short enough never to reach a check is never asked about, which
/// is the property the point-read path rests on: one row of work must not cost
/// a call out into the host, or the engine's best number would pay for a
/// feature it is not using.
#[test]
fn a_short_statement_is_never_asked() {
    let (mut engine, trip) = seeded();
    let lookup = engine
        .prepare("SELECT body FROM t WHERE id = ?")
        .expect("prepare");
    trip.never_trip();
    for id in 1..=200 {
        engine
            .run_query(&lookup, &[Value::Integer(id)])
            .expect("point read");
    }
    assert_eq!(
        trip.asked.get(),
        0,
        "two hundred point reads asked the host {} times; the stride is not working",
        trip.asked.get()
    );
}
