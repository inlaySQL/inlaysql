//! Lookup by a non-key column — the query an ORM emits all day.
//!
//! # What is being compared
//!
//! `SELECT ... WHERE email = ?` and a small `WHERE email >= ? AND email < ?`
//! range on a table with no primary-key access path to the answer. It is the
//! workload `PERF.md` names as the largest application-visible win left in
//! the project, and it is the one where the shape of the answer changes
//! rather than its constant factor: without a secondary index the engine
//! reads and decodes every row in the table, so the cost grows with the
//! table; with one it is a tree descent (point) or an entry-range read
//! (range) and a handful of row reads.
//!
//! Four rows are measured for each query shape, and the first two are the
//! point:
//!
//! * **InlaySQL with the index** — the range scan.
//! * **InlaySQL without it** — the same engine, the same rows, the same query,
//!   the full scan. This is the "before" of the before/after, measured in the
//!   same process on the same machine in the same second, which is the only
//!   way to state a speed-up without a machine-to-machine caveat. This is the
//!   row that regenerates the AHL-423 ~3,800x figure instead of leaving it as
//!   an unreproduced assertion.
//! * **SQLite with the same index**, in both of the durability configurations
//!   `points` uses, so the number is anchored to something outside this repo.
//!
//! # Making it a fair fight
//!
//! * **The same index on both sides.** SQLite has no `USING` in `CREATE
//!   INDEX`, and InlaySQL's inferred kind on a `TEXT` column is the BM25
//!   index, so the InlaySQL side spells it `USING BTREE`. Same structure,
//!   different syntax.
//! * **Prepared statements on both sides**, bound per iteration, as in
//!   `points` — otherwise this measures a parser.
//! * **The same seeded lookup sequence**, so both engines answer the same
//!   questions in the same order, and every point lookup matches exactly one
//!   row.
//! * **The range is small and exact.** `email`'s id is zero-padded to a fixed
//!   width, so lexicographic order on the column equals numeric order on the
//!   id, and a bound of `[start, start + RANGE_SIZE)` returns exactly
//!   [`RANGE_SIZE`] rows on both engines — a small range, not a scan pretending
//!   to be one.
//! * **The scan is not handicapped.** It gets the same page cache, the same
//!   prepared statement and the same warm database; the only difference is
//!   that no index exists for the planner to use.
//!
//! The unindexed row is expected to be slow and the ratio to grow with
//! `--rows`. That is not a flattering benchmark, it is the definition of the
//! problem: a scan is O(rows) and a probe is not, so the number this prints is
//! a property of the table size as much as of the engine. Read it with
//! `--rows` in hand.

use std::path::Path;
use std::time::{Duration, Instant};

use inlaysql::{Database, Value};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::Rng;

use crate::points::{open_sqlite, remove_sqlite_files, Durability};
use crate::{percentiles, Config};

/// Rows returned by one range query. Small on purpose — the point is a range
/// probe, not a scan wearing a `BETWEEN`.
const RANGE_SIZE: usize = 50;

/// One engine's result for one workload.
struct Timing {
    label: String,
    elapsed: Duration,
    samples: Vec<Duration>,
}

impl Timing {
    fn per_second(&self, operations: usize) -> f64 {
        operations as f64 / self.elapsed.as_secs_f64().max(f64::EPSILON)
    }
}

/// The value stored in row `id`'s indexed column. Distinct per row, and the
/// same width for every row, so no lookup is cheaper than another because its
/// key is shorter.
fn email(id: i64) -> String {
    format!("user{id:012}@example.com")
}

/// The lookup sequence, generated once from the seed so every engine answers
/// the identical questions in the identical order.
fn lookup_emails(seed: u64, rows: usize, lookups: usize) -> Vec<String> {
    let mut rng = SeededRng::new(seed);
    (0..lookups)
        .map(|_| email(1 + (rng.next_u64() % rows as u64) as i64))
        .collect()
}

/// The range-query start ids, generated once from the seed so every engine
/// answers the identical `[start, start + RANGE_SIZE)` ranges in the
/// identical order. Bounded so every range fits inside `1..=rows` — no engine
/// is asked for a short range at the edge of the table.
fn range_starts(seed: u64, rows: usize, queries: usize) -> Vec<i64> {
    let mut rng = SeededRng::new(seed);
    let bound = rows.saturating_sub(RANGE_SIZE).max(1) as u64;
    (0..queries)
        .map(|_| 1 + (rng.next_u64() % bound) as i64)
        .collect()
}

pub fn run(config: &Config, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let rows = config.rows;
    let lookups = config.lookups;
    let ranges = config.queries;
    println!(
        "\n=== indexed lookup: {rows} rows, {lookups} point lookups + {ranges} range queries \
         (range size {RANGE_SIZE}) by a non-key column ==="
    );
    println!(
        "(the unindexed row is the same engine on the same rows with no index to use: a full \
         scan, so its cost grows with --rows)"
    );

    let keys = lookup_emails(config.seed, rows, lookups);
    let starts = range_starts(config.seed, rows, ranges);
    let payload = "x".repeat(config.payload);

    let indexed = inlaysql_lookup(
        &dir.join("indexed-inlaysql.inlay"),
        rows,
        &keys,
        &starts,
        &payload,
        true,
    )?;
    let unindexed = inlaysql_lookup(
        &dir.join("indexed-inlaysql-scan.inlay"),
        rows,
        &keys,
        &starts,
        &payload,
        false,
    )?;
    let sqlite_journal = sqlite_lookup(
        &dir.join("indexed-sqlite-journal.db"),
        rows,
        &keys,
        &starts,
        &payload,
        Durability::JournalFull,
    )?;
    let sqlite_wal = sqlite_lookup(
        &dir.join("indexed-sqlite-wal.db"),
        rows,
        &keys,
        &starts,
        &payload,
        Durability::WalNormal,
    )?;

    report(
        "indexed point lookup (WHERE email = ?)",
        lookups,
        &[&indexed.0, &unindexed.0, &sqlite_journal.0, &sqlite_wal.0],
    );
    report(
        "indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)",
        ranges,
        &[&indexed.1, &unindexed.1, &sqlite_journal.1, &sqlite_wal.1],
    );
    Ok(())
}

fn report(workload: &str, operations: usize, timings: &[&Timing]) {
    println!("\n{workload}");
    println!(
        "{:<46} {:>12} {:>10} {:>10} {:>10} {:>10}",
        "engine", "ops/s", "p50", "p95", "p99", "max"
    );
    for timing in timings {
        let (p50, p95, p99, max) = percentiles(&timing.samples);
        println!(
            "{:<46} {:>12.0} {:>10} {:>10} {:>10} {:>10}",
            timing.label,
            timing.per_second(operations),
            format!("{p50:.2?}"),
            format!("{p95:.2?}"),
            format!("{p99:.2?}"),
            format!("{max:.2?}")
        );
    }
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

/// Load the table, optionally index it, then time the point lookups and the
/// range lookups.
///
/// The rows go in inside explicit transactions, as `points`' batched writer
/// does: the load is setup, not the measurement, and one `fsync` per row would
/// make a large `--rows` take minutes for no gain in what is being measured.
fn inlaysql_lookup(
    path: &Path,
    rows: usize,
    keys: &[String],
    starts: &[i64],
    payload: &str,
    indexed: bool,
) -> Result<(Timing, Timing), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let mut db = Database::open(path)?;
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, body TEXT)",
        &[],
    )?;

    let insert = db.prepare("INSERT INTO users (id, email, body) VALUES (?, ?, ?)")?;
    db.begin()?;
    for id in 1..=rows as i64 {
        let bound = [
            Value::Integer(id),
            Value::Text(email(id).into()),
            Value::Text(payload.to_string().into()),
        ];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &bound)?;
        }
    }
    db.commit()?;

    // Built after the rows, which is also the harder path for the engine: the
    // index has to describe a table that already exists.
    if indexed {
        db.execute("CREATE INDEX users_email ON users (email) USING BTREE", &[])?;
    }

    let label = if indexed {
        "InlaySQL (B-tree index)".to_string()
    } else {
        "InlaySQL (no index: full scan)".to_string()
    };

    let lookup = db.prepare("SELECT id, body FROM users WHERE email = ?")?;
    let mut samples = Vec::with_capacity(keys.len());
    let started = Instant::now();
    for key in keys {
        let at = Instant::now();
        let result = db.query_prepared(&lookup, &[Value::Text(key.clone().into())])?;
        debug_assert_eq!(result.rows.len(), 1, "every key matches exactly one row");
        samples.push(at.elapsed());
    }
    let point_elapsed = started.elapsed();
    let point = Timing {
        label: label.clone(),
        elapsed: point_elapsed,
        samples,
    };

    let range = db.prepare("SELECT id, body FROM users WHERE email >= ? AND email < ?")?;
    let mut samples = Vec::with_capacity(starts.len());
    let started = Instant::now();
    for &start in starts {
        let low = email(start);
        let high = email(start + RANGE_SIZE as i64);
        let at = Instant::now();
        let result =
            db.query_prepared(&range, &[Value::Text(low.into()), Value::Text(high.into())])?;
        debug_assert_eq!(
            result.rows.len(),
            RANGE_SIZE,
            "every range starts inside the table and is RANGE_SIZE wide"
        );
        samples.push(at.elapsed());
    }
    let range_elapsed = started.elapsed();
    let range = Timing {
        label,
        elapsed: range_elapsed,
        samples,
    };

    let _ = std::fs::remove_file(path);
    Ok((point, range))
}

fn sqlite_lookup(
    path: &Path,
    rows: usize,
    keys: &[String],
    starts: &[i64],
    payload: &str,
    durability: Durability,
) -> Result<(Timing, Timing), Box<dyn std::error::Error>> {
    remove_sqlite_files(path);
    let conn = open_sqlite(path, durability)?;
    conn.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, body TEXT)",
        [],
    )?;
    conn.execute("BEGIN", [])?;
    {
        let mut insert = conn.prepare("INSERT INTO users (id, email, body) VALUES (?1, ?2, ?3)")?;
        for id in 1..=rows as i64 {
            insert.execute(rusqlite::params![id, email(id), payload])?;
        }
    }
    conn.execute("COMMIT", [])?;
    conn.execute("CREATE INDEX users_email ON users (email)", [])?;

    let label = durability.label();

    let mut samples = Vec::with_capacity(keys.len());
    let mut lookup = conn.prepare("SELECT id, body FROM users WHERE email = ?1")?;
    let started = Instant::now();
    for key in keys {
        let at = Instant::now();
        let _: (i64, String) = lookup.query_row([key], |row| Ok((row.get(0)?, row.get(1)?)))?;
        samples.push(at.elapsed());
    }
    let point_elapsed = started.elapsed();
    drop(lookup);
    let point = Timing {
        label: format!("{label} (index)"),
        elapsed: point_elapsed,
        samples,
    };

    let mut samples = Vec::with_capacity(starts.len());
    let mut range = conn.prepare("SELECT id, body FROM users WHERE email >= ?1 AND email < ?2")?;
    let started = Instant::now();
    for &start in starts {
        let low = email(start);
        let high = email(start + RANGE_SIZE as i64);
        let at = Instant::now();
        let rows_returned = range
            .query_map(rusqlite::params![low, high], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .count();
        debug_assert_eq!(rows_returned, RANGE_SIZE);
        samples.push(at.elapsed());
    }
    let range_elapsed = started.elapsed();
    drop(range);
    let range = Timing {
        label: format!("{label} (index)"),
        elapsed: range_elapsed,
        samples,
    };

    drop(conn);
    remove_sqlite_files(path);
    Ok((point, range))
}
