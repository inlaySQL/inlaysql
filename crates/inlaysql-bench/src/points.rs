//! Point reads and point writes, InlaySQL against SQLite.
//!
//! # What is being compared
//!
//! The narrowest workload a storage engine has: fetch one row by primary key,
//! and write one row durably. No retrieval, no ranking — just the B-tree, the
//! log and the `fsync`. It is the workload where SQLite is strongest and where
//! any honest comparison starts.
//!
//! # Making it a fair fight
//!
//! * **Same schema and same key.** `id INTEGER PRIMARY KEY` is the row id in
//!   both engines, so both do one tree descent per lookup.
//! * **Prepared statements on both sides.** Each engine prepares its statement
//!   once outside the timed loop and binds the key per iteration: InlaySQL
//!   through `Database::prepare` + `query_prepared_each_ref`, SQLite through
//!   `Connection::prepare` + `Statement::query_row`. Preparing on one side and
//!   not the other would measure a parser, not a storage engine — which is
//!   what this suite used to have to do, because InlaySQL had no prepare API.
//! * **Both sides step, both sides read.** SQLite's API has always been
//!   `sqlite3_step` into caller-owned registers and `sqlite3_column_text` into
//!   the caller's hands; until AHL-535 InlaySQL had no equivalent, so this
//!   suite called `query_prepared` and compared a step loop against a
//!   `Vec<Vec<Value>>` built and dropped per query. That was a difference in
//!   API shape, not in engine speed, and it is gone: both sides now step a row
//!   and read the `body` column as a borrowed `&str`, summing its length into
//!   a checksum the loop `black_box`es. SQLite's side changed too, from
//!   `row.get::<String>(0)` — which copies out of its page — to
//!   `row.get_ref(0)?.as_str()`, which does not. The comparison got *harder*
//!   for us, not easier: this removed an allocation per lookup from SQLite's
//!   loop as well as from ours. Reading the column rather than counting rows
//!   is the other half of the same fairness: an answer nobody looks at is not
//!   a workload anybody has, and a row count is a number either engine can
//!   produce without the caller touching a byte.
//! * **Durability stated, not assumed.** SQLite is measured twice: in its
//!   default rollback-journal mode with `synchronous=FULL` *and* `fullfsync`
//!   (which is what makes it comparable to InlaySQL on macOS — see the pragma
//!   below), and in WAL mode with `synchronous=NORMAL`, which is what most
//!   applications actually run. InlaySQL commits and syncs once per statement;
//!   there is no knob. Read the WAL row as "what you give up durability for",
//!   not as the like-for-like column.
//! * **Same seeded key order** for the lookups, so both engines answer the
//!   same questions in the same sequence.
//!
//! The numbers are wall-clock on one machine and mean nothing in the abstract;
//! what they are for is catching a regression and telling us where we stand.

use std::path::Path;
use std::time::{Duration, Instant};

use inlaysql::{Database, Value};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::Rng;

use crate::{percentiles, Config};

/// One engine's result for one workload.
struct Timing {
    label: &'static str,
    elapsed: Duration,
    samples: Vec<Duration>,
}

impl Timing {
    fn per_second(&self, operations: usize) -> f64 {
        operations as f64 / self.elapsed.as_secs_f64().max(f64::EPSILON)
    }
}

/// The primary-key lookup sequence this suite measures, generated once from
/// the seed so every consumer — the in-process SQLite comparison here, and
/// the OLTP export that lets MySQL and PostgreSQL answer the same lookups —
/// asks the identical questions in the identical order.
pub(crate) fn lookup_keys(seed: u64, rows: usize, lookups: usize) -> Vec<i64> {
    let mut rng = SeededRng::new(seed);
    (0..lookups)
        .map(|_| 1 + (rng.next_u64() % rows as u64) as i64)
        .collect()
}

pub fn run(config: &Config, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let rows = config.rows;
    let lookups = config.lookups;
    println!("\n=== point workload: {rows} rows, {lookups} lookups by primary key ===");
    println!("(prepared statements on both sides; parse and plan happen once, outside the loop)");

    // The same key sequence for every engine.
    let keys = lookup_keys(config.seed, rows, lookups);
    let payload = "x".repeat(config.payload);

    let inlay = inlaysql_points(&dir.join("points-inlaysql.inlay"), rows, &keys, &payload)?;
    let inlay_batched =
        inlaysql_batched_write(&dir.join("points-inlaysql-batched.inlay"), rows, &payload)?;
    let sqlite_journal = sqlite_points(
        &dir.join("points-sqlite-journal.db"),
        rows,
        &keys,
        &payload,
        Durability::JournalFull,
    )?;
    let sqlite_wal = sqlite_points(
        &dir.join("points-sqlite-wal.db"),
        rows,
        &keys,
        &payload,
        Durability::WalNormal,
    )?;

    report(
        "point write (one durable commit each)",
        rows,
        &[&inlay.0, &sqlite_journal.0, &sqlite_wal.0],
    );
    report(
        "batched write (many rows per commit)",
        rows,
        &[&inlay_batched, &inlay.0, &sqlite_journal.0],
    );
    report(
        "point read (by primary key)",
        lookups,
        &[&inlay.1, &sqlite_journal.1, &sqlite_wal.1],
    );
    Ok(())
}

fn report(workload: &str, operations: usize, timings: &[&Timing]) {
    println!("\n{workload}");
    println!(
        "{:<40} {:>12} {:>10} {:>10} {:>10} {:>10}",
        "engine", "ops/s", "p50", "p95", "p99", "max"
    );
    for timing in timings {
        let (p50, p95, p99, max) = percentiles(&timing.samples);
        println!(
            "{:<40} {:>12.0} {:>10} {:>10} {:>10} {:>10}",
            timing.label,
            timing.per_second(operations),
            format!("{p50:.2?}"),
            format!("{p95:.2?}"),
            format!("{p99:.2?}"),
            format!("{max:.2?}")
        );
    }
    // The ratio is what a reader actually wants, so state it rather than
    // leaving it as an exercise.
    if let [ours, theirs, ..] = timings {
        let ratio = theirs.elapsed.as_secs_f64() / ours.elapsed.as_secs_f64().max(f64::EPSILON);
        if ratio >= 1.0 {
            println!("{} is {ratio:.2}x faster than {}", ours.label, theirs.label);
        } else {
            println!(
                "{} is {:.2}x slower than {}",
                ours.label,
                1.0 / ratio,
                theirs.label
            );
        }
    }
}

fn inlaysql_points(
    path: &Path,
    rows: usize,
    keys: &[i64],
    payload: &str,
) -> Result<(Timing, Timing), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let mut db = Database::open(path)?;
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;

    // Prepared outside the timed loops, exactly as on the SQLite side: what is
    // being measured is the write and the descent, not the parser.
    let insert = db.prepare("INSERT INTO kv (id, body) VALUES (?, ?)")?;
    let lookup = db.prepare("SELECT body FROM kv WHERE id = ?")?;

    let mut writes = Vec::with_capacity(rows);
    let started = Instant::now();
    for id in 1..=rows as i64 {
        let at = Instant::now();
        db.execute_prepared(
            &insert,
            &[Value::Integer(id), Value::Text(payload.to_string().into())],
        )?;
        writes.push(at.elapsed());
    }
    let write_elapsed = started.elapsed();

    // Stepped and read, not collected and counted — see the module note "Both
    // sides step, both sides read". `checksum` exists so the read cannot be
    // optimised away; it is `black_box`ed below.
    let mut checksum = 0u64;
    let mut reads = Vec::with_capacity(keys.len());
    let started = Instant::now();
    for key in keys {
        let at = Instant::now();
        let delivered = db.query_prepared_each_ref(&lookup, &[Value::Integer(*key)], |row| {
            checksum += row[0].as_str().map_or(0, str::len) as u64;
            Ok(())
        })?;
        debug_assert_eq!(delivered, 1, "point read missed row {key}");
        reads.push(at.elapsed());
    }
    let read_elapsed = started.elapsed();
    std::hint::black_box(checksum);

    debug_assert_eq!(
        db.statements_parsed(),
        3,
        "the timed loops parsed a statement: CREATE plus two prepares is all this may cost"
    );

    let _ = std::fs::remove_file(path);
    Ok((
        Timing {
            label: "InlaySQL",
            elapsed: write_elapsed,
            samples: writes,
        },
        Timing {
            label: "InlaySQL",
            elapsed: read_elapsed,
            samples: reads,
        },
    ))
}

/// Write `rows` rows inside explicit transactions, batching them so a single
/// commit never exceeds the write-ahead log.
///
/// This is the row the whole batch-writes stage exists to add: the same
/// per-statement insert loop as [`inlaysql_points`], but wrapped in
/// `begin`/`commit` so thousands of rows cost one `fsync` per batch rather
/// than one per row. The batch boundary is the engine's own limit — when a
/// transaction is about to overflow the log the engine says so
/// ([`inlaysql::Error::Transaction`]), the write commits, and a fresh
/// transaction starts.
fn inlaysql_batched_write(
    path: &Path,
    rows: usize,
    payload: &str,
) -> Result<Timing, Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let mut db = Database::open(path)?;
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;

    let insert = db.prepare("INSERT INTO kv (id, body) VALUES (?, ?)")?;

    let mut writes = Vec::with_capacity(rows);
    let started = Instant::now();
    db.begin()?;
    for id in 1..=rows as i64 {
        let at = Instant::now();
        match db.execute_prepared(
            &insert,
            &[Value::Integer(id), Value::Text(payload.to_string().into())],
        ) {
            Ok(_) => {}
            // The engine refuses a statement that would overflow the log, and
            // the error is raised before the statement runs, so committing here
            // is exactly "flush what is buffered" — no row is lost or doubled.
            Err(inlaysql::Error::Transaction(_)) => {
                db.commit()?;
                db.begin()?;
                db.execute_prepared(
                    &insert,
                    &[Value::Integer(id), Value::Text(payload.to_string().into())],
                )?;
            }
            Err(other) => return Err(other.into()),
        }
        writes.push(at.elapsed());
    }
    db.commit()?;
    let elapsed = started.elapsed();

    let _ = std::fs::remove_file(path);
    Ok(Timing {
        label: "InlaySQL (batched)",
        elapsed,
        samples: writes,
    })
}

/// How the SQLite baseline is configured. Both are real configurations people
/// ship; neither is "SQLite with durability turned off".
#[derive(Clone, Copy)]
pub enum Durability {
    /// The default: rollback journal, every commit fsynced.
    JournalFull,
    /// What most applications set: WAL, fsync at checkpoints.
    WalNormal,
}

impl Durability {
    pub fn label(self) -> &'static str {
        match self {
            Durability::JournalFull => "SQLite (journal, sync=FULL, fullfsync)",
            Durability::WalNormal => "SQLite (WAL, sync=NORMAL)",
        }
    }
}

/// Every file SQLite may create for a database at `path`.
///
/// Removed before and after a run: a leftover WAL from a previous suite would
/// silently change what the next one measures.
pub fn remove_sqlite_files(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// Open a SQLite connection configured for `durability`.
///
/// Shared with the concurrency suite so that "the SQLite baseline" means the
/// same thing everywhere in this harness.
pub fn open_sqlite(path: &Path, durability: Durability) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    match durability {
        Durability::JournalFull => {
            conn.pragma_update(None, "journal_mode", "delete")?;
            conn.pragma_update(None, "synchronous", "FULL")?;
            // Rust's `File::sync_all` is `F_FULLFSYNC` on macOS — a real
            // barrier through the drive's write cache — while SQLite defaults
            // to plain `fsync`, which on that platform returns before the data
            // is on the platter. Without this pragma the like-for-like column
            // would be comparing a durable commit against a hopeful one.
            // No-op on Linux, where `fsync` is already the barrier.
            conn.pragma_update(None, "fullfsync", "ON")?;
        }
        Durability::WalNormal => {
            conn.pragma_update(None, "journal_mode", "wal")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
    }
    Ok(conn)
}

fn sqlite_points(
    path: &Path,
    rows: usize,
    keys: &[i64],
    payload: &str,
    durability: Durability,
) -> Result<(Timing, Timing), Box<dyn std::error::Error>> {
    remove_sqlite_files(path);

    let conn = open_sqlite(path, durability)?;
    conn.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", [])?;

    // `Connection::execute` and `Connection::query_row` prepare per call.
    // Holding the `Statement` is how rusqlite exposes SQLite's own
    // prepare/bind/reset cycle, which is the like-for-like against InlaySQL's.
    let mut insert = conn.prepare("INSERT INTO kv (id, body) VALUES (?1, ?2)")?;
    let mut lookup = conn.prepare("SELECT body FROM kv WHERE id = ?1")?;

    let mut writes = Vec::with_capacity(rows);
    let started = Instant::now();
    for id in 1..=rows as i64 {
        let at = Instant::now();
        insert.execute(rusqlite::params![id, payload])?;
        writes.push(at.elapsed());
    }
    let write_elapsed = started.elapsed();

    // `get_ref` rather than `get::<String>`: SQLite hands back a pointer into
    // its own page and `row.get(0)` copies it into a `String`. Reading the
    // length off the borrowed `&str` is what the InlaySQL side now does too —
    // see the module note "Both sides step, both sides read".
    let mut checksum = 0u64;
    let mut reads = Vec::with_capacity(keys.len());
    let started = Instant::now();
    for key in keys {
        let at = Instant::now();
        lookup.query_row([key], |row| {
            checksum += row.get_ref(0)?.as_str().map(str::len).unwrap_or(0) as u64;
            Ok(())
        })?;
        reads.push(at.elapsed());
    }
    let read_elapsed = started.elapsed();
    std::hint::black_box(checksum);

    drop(insert);
    drop(lookup);
    drop(conn);
    remove_sqlite_files(path);

    Ok((
        Timing {
            label: durability.label(),
            elapsed: write_elapsed,
            samples: writes,
        },
        Timing {
            label: durability.label(),
            elapsed: read_elapsed,
            samples: reads,
        },
    ))
}
