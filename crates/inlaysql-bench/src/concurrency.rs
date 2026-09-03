//! Concurrent-writer throughput, InlaySQL against SQLite.
//!
//! "Multiple concurrent writers" is InlaySQL's headline claim against SQLite,
//! which allows exactly one. This is the suite that has to be able to embarrass
//! us, so read the caveats before the numbers.
//!
//! # What "concurrent" means here
//!
//! Several OS threads write one database file. Each thread opens an independent
//! file handle and gets a WAL region; the short conflict/sequence reservation
//! is ordered, while record flushes overlap. Threads retry first-committer-wins
//! conflicts until every disjoint-key transaction lands.
//!
//! # What it measures
//!
//! * **Committed transactions per second.** Retries are not counted as
//!   commits — only work that landed.
//! * **The conflict rate.** InlaySQL settles a race by first-committer-wins,
//!   so a writer whose snapshot went stale is rolled back and has to retry.
//!   Retried work is real work the machine did and threw away.
//! * **That nothing was lost.** Every suite run verifies the file holds exactly
//!   the rows the writers were told they committed. A throughput number over
//!   dropped writes would be worse than no number at all — and before
//!   [`Error::Conflict`] existed that is exactly what this suite would have
//!   printed, because the engine reported a rolled-back transaction as
//!   committed.
//! * **Per-commit latency (p50/p95/p99/max).** Throughput and the conflict
//!   rate say nothing about the tail: the adaptive gather window
//!   (`CommitCoordinator::coalesce_normal_commits` in `inlaysql`'s
//!   `device.rs`) can hold a flush leader for as long as
//!   `COMMIT_COALESCE_MAX_YIELDS` — up to about 2.3ms — to gather a bigger
//!   cohort before its `fsync`, which is exactly the kind of cost a
//!   commits/s average hides and a p99 exposes. Measured per successful
//!   `db.execute` call (a conflicted attempt's own duration is not counted —
//!   the conflict rate above already prices retries in), merged across every
//!   writer thread at a given writer count.
//!
//! # The SQLite baseline
//!
//! Same shape: N connections to one file, taking turns, one row per
//! transaction. SQLite serializes writers with a lock rather than aborting
//! them, so in this interleaving it never conflicts — the lock is always free
//! by the time the next connection asks for it. Its number is therefore "N
//! sessions' worth of per-statement overhead", and the honest comparison is
//! against InlaySQL's *committed* throughput, with our conflict rate read as
//! the price of the optimistic design.

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use inlaysql::{Database, EngineOptions, Error, FileDevice, Value};

// `crate::points::Durability` is SQLite's own `synchronous` levels, not
// InlaySQL's — aliased so the two do not collide in this file, which
// measures both.
use crate::points::{open_sqlite, remove_sqlite_files, Durability as SqliteDurability};
use crate::{percentiles, Config};

/// InlaySQL's [`Durability`](inlaysql::Durability) for the writers this suite
/// opens, from `INLAYSQL_BENCH_DURABILITY` (`full` or `normal`, case
/// insensitive) — `full` when unset, matching every other suite and the
/// engine's own default. Read once per process; every writer thread and the
/// schema-creating handle share the same choice, and the baseline sweep in
/// `docs`/`PERF.md` is `full` (unset), so the multi-writer numbers already
/// published are reproduced by not setting this at all.
fn bench_durability() -> inlaysql::Durability {
    match std::env::var("INLAYSQL_BENCH_DURABILITY") {
        Ok(value) if value.eq_ignore_ascii_case("normal") => inlaysql::Durability::Normal,
        Ok(value) if value.eq_ignore_ascii_case("full") => inlaysql::Durability::Full,
        Ok(other) => {
            eprintln!(
                "ignoring INLAYSQL_BENCH_DURABILITY={other:?} (expected \"full\" or \"normal\"); using full"
            );
            inlaysql::Durability::Full
        }
        Err(_) => inlaysql::Durability::Full,
    }
}

/// Whether to open this suite's writers with commit-side absorption
/// (AHL-544), from `INLAYSQL_BENCH_ABSORPTION` (`1`/`true`/`on`, case
/// insensitive) — off when unset, matching the engine's own default and every
/// published concurrency number. Read once per process; every writer thread
/// and the schema-creating handle share the same choice, which they must,
/// because absorption is a property of the file's commit gate rather than of
/// one handle. See `EngineOptions::commit_absorption`.
fn bench_absorption() -> bool {
    match std::env::var("INLAYSQL_BENCH_ABSORPTION") {
        Ok(value) => {
            let value = value.trim();
            value.eq_ignore_ascii_case("1")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}

/// Open `path` with [`bench_durability`]'s level — the concurrency suite's
/// own stand-in for `Database::open`, which always opens at `Durability::Full`.
fn open_inlaysql(path: &Path) -> Result<Database, inlaysql::Error> {
    Database::open_on_with_options(
        FileDevice::open(path)?,
        EngineOptions {
            durability: bench_durability(),
            commit_absorption: bench_absorption(),
            ..EngineOptions::default()
        },
    )
}

/// One engine's result at one writer count.
struct Outcome {
    label: String,
    writers: usize,
    /// Transactions that committed.
    committed: usize,
    /// Transactions rolled back and retried.
    conflicts: usize,
    elapsed: Duration,
    /// One entry per committed transaction: the wall-clock time of exactly
    /// the `db.execute`/`Connection::execute` call that committed it, merged
    /// across every writer thread at this writer count. Never a conflicted
    /// attempt's duration — see the module doc's latency bullet.
    samples: Vec<Duration>,
}

impl Outcome {
    fn per_second(&self) -> f64 {
        self.committed as f64 / self.elapsed.as_secs_f64().max(f64::EPSILON)
    }

    /// Aborted transactions as a fraction of all attempts.
    fn conflict_rate(&self) -> f64 {
        let attempts = self.committed + self.conflicts;
        if attempts == 0 {
            return 0.0;
        }
        self.conflicts as f64 / attempts as f64
    }
}

pub fn run(config: &Config, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let counts = writer_counts(config.writers);
    println!(
        "\n=== concurrent writers: {} transactions per writer, one row each, OS threads; levels {counts:?} ===",
        config.txns,
    );
    println!(
        "(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers\n\
         still serialize at its file lock.)"
    );

    let mut outcomes = Vec::new();
    for &writers in &counts {
        outcomes.push(inlaysql_writers(
            &dir.join("concurrency-inlaysql.inlay"),
            writers,
            config.txns,
        )?);
    }
    for &writers in &counts {
        outcomes.push(sqlite_writers(
            &dir.join("concurrency-sqlite.db"),
            writers,
            config.txns,
            SqliteDurability::JournalFull,
        )?);
    }

    println!(
        "\n{:<40} {:>8} {:>12} {:>12} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "engine", "writers", "commits/s", "committed", "conflicts", "p50", "p95", "p99", "max"
    );
    for outcome in &outcomes {
        let (p50, p95, p99, max) = percentiles(&outcome.samples);
        println!(
            "{:<40} {:>8} {:>12.0} {:>12} {:>9.1}% {:>10} {:>10} {:>10} {:>10}",
            outcome.label,
            outcome.writers,
            outcome.per_second(),
            outcome.committed,
            outcome.conflict_rate() * 100.0,
            format!("{p50:.2?}"),
            format!("{p95:.2?}"),
            format!("{p99:.2?}"),
            format!("{max:.2?}"),
        );
    }

    // The conclusion a reader would otherwise have to derive, stated where it
    // cannot be missed: if adding writers does not add throughput, the claim
    // that this engine has concurrent writers is not yet worth much.
    if let (Some(one), Some(many)) = (
        outcomes.iter().find(|o| o.writers == 1 && o.is_ours()),
        outcomes
            .iter()
            .filter(|o| o.is_ours())
            .max_by_key(|o| o.writers),
    ) {
        let scaling = many.per_second() / one.per_second().max(f64::EPSILON);
        println!(
            "\nInlaySQL at {} writers does {scaling:.2}x the work of 1 writer, \
             aborting {:.1}% of transactions.",
            many.writers,
            many.conflict_rate() * 100.0
        );
        if many.conflict_rate() > 0.25 {
            println!(
                "The writers touch disjoint keys, so these are snapshot conflicts. They are\n\
                 reported and retried; per-writer WAL regions make the retrying commits'\n\
                 durability flushes overlap. See bench/README.md."
            );
        }
    }
    Ok(())
}

impl Outcome {
    fn is_ours(&self) -> bool {
        self.label.starts_with("InlaySQL")
    }
}

/// The writer counts to sweep: `WRITER_LEVELS` can focus the run on an
/// explicit comma-separated set (for example `1,32` or `1,128`); otherwise it
/// uses 1 (the baseline) up to the requested maximum, doubling. Single-writer
/// throughput is what every other row is read against.
fn writer_counts(max: usize) -> Vec<usize> {
    if let Ok(raw) = std::env::var("WRITER_LEVELS") {
        let mut levels = Vec::new();
        for value in raw.split(',') {
            match value.trim().parse::<usize>() {
                Ok(level) if level > 0 => levels.push(level),
                _ => eprintln!("ignoring invalid WRITER_LEVELS entry {value:?}"),
            }
        }
        levels.sort_unstable();
        levels.dedup();
        if !levels.is_empty() {
            return levels;
        }
        eprintln!("WRITER_LEVELS had no positive entries; using the default sweep");
    }

    let mut counts = Vec::new();
    let mut writers = 1;
    while writers <= max.max(1) {
        counts.push(writers);
        writers *= 2;
    }
    if *counts.last().unwrap_or(&0) != max.max(1) {
        counts.push(max.max(1));
    }
    counts
}

/// One writer thread's outcome: transactions committed, transactions
/// conflicted, and one latency sample per commit — see the module doc's
/// latency bullet for what each sample times.
type WriterResult = Result<(usize, usize, Vec<Duration>), Error>;

fn inlaysql_writers(
    path: &Path,
    writers: usize,
    txns: usize,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let mut creator = open_inlaysql(path)?;
    let absorption = bench_absorption();
    // No TEXT or VECTOR column: this suite is about the tree and the sync, and
    // an indexed column would also make every conflict pay for an index
    // rebuild, which is a different measurement.
    creator.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", &[])?;
    drop(creator);
    // The coordinator, and with it the absorption counters, lives only as long
    // as some handle on this file does. Held across the run so the cohort
    // report below is the run's own and not a fresh coordinator's zeroes.
    let keeper = FileDevice::open(path)?;

    // Open and lock every handle before starting the clock. SQLite's baseline
    // creates all of its connections before its timed loop; including
    // InlaySQL's per-thread open/lock handoff here would make this a setup
    // comparison rather than a steady-state commit comparison.
    let ready = Arc::new(Barrier::new(writers + 1));
    let start = Arc::new(Barrier::new(writers + 1));
    let (results, elapsed) = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for index in 0..writers {
            let path = path.to_path_buf();
            let ready = ready.clone();
            let start = start.clone();
            handles.push(scope.spawn(move || {
                let mut db = open_inlaysql(&path)?;
                ready.wait();
                start.wait();
                let mut committed = 0;
                let mut conflicts = 0;
                let mut samples = Vec::with_capacity(txns);
                for round in 0..txns {
                    let id = (round * writers + index + 1) as i64;
                    loop {
                        // Timed around exactly this attempt, not the whole
                        // retry loop: a conflict's own duration is real work
                        // (already counted in `conflicts`), but folding it
                        // into a commit's latency would blur "how long did a
                        // commit take" with "how many times did this writer
                        // have to retry", which the conflict rate already
                        // answers.
                        let attempted = Instant::now();
                        match db.execute(
                            "INSERT INTO kv (id, n) VALUES (?, ?)",
                            &[Value::Integer(id), Value::Integer(id)],
                        ) {
                            Ok(_) => {
                                samples.push(attempted.elapsed());
                                committed += 1;
                                break;
                            }
                            Err(Error::Conflict) => conflicts += 1,
                            Err(other) => return Err(other),
                        }
                    }
                }
                Ok((committed, conflicts, samples))
            }));
        }
        ready.wait();
        let started = Instant::now();
        start.wait();
        let results: Vec<WriterResult> = handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|panic| {
                    std::panic::resume_unwind(panic);
                })
            })
            .collect();
        (results, started.elapsed())
    });
    let mut committed = 0;
    let mut conflicts = 0;
    let mut samples = Vec::new();
    for result in results {
        let (worker_committed, worker_conflicts, worker_samples) = result?;
        committed += worker_committed;
        conflicts += worker_conflicts;
        samples.extend(worker_samples);
    }

    verify(path, txns * writers)?;
    // Absorption's own STOP condition is "do cohorts form at all?", so the
    // suite reports it rather than leaving a flat measurement ambiguous
    // between "it does not help" and "it never ran".
    if absorption {
        let (cohorts, members) = keeper.absorption_stats().unwrap_or((0, 0));
        println!(
            "  absorption: {writers} writers, {cohorts} cohorts, {members} members judged \
             ({:.2} members/cohort, {:.1}% of commits absorbed)",
            members as f64 / (cohorts.max(1)) as f64,
            100.0 * members as f64 / (committed.max(1)) as f64,
        );
    }
    drop(keeper);
    let _ = std::fs::remove_file(path);
    Ok(Outcome {
        label: if absorption {
            "InlaySQL (parallel WAL regions, absorption)".to_string()
        } else {
            "InlaySQL (parallel WAL regions)".to_string()
        },
        writers,
        committed,
        conflicts,
        elapsed,
        samples,
    })
}

/// Every row the writers were told they committed has to be in the file.
///
/// This is the assertion that makes the throughput number mean anything, so it
/// is checked on every run rather than left to the test suite.
///
/// The count is read through a **freshly opened** handle, not through one of
/// the writers. A writer's tree caches the root it last committed and only
/// re-reads it when it commits again, so an open handle keeps answering from
/// its own snapshot — it never sees another writer's rows until it writes.
/// That is a real limitation worth knowing about (see `bench/README.md`), and
/// counting rows through it would report every other writer's commits as lost.
fn verify(path: &Path, expected: usize) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = Database::open(path)?;
    let rows = db.query("SELECT id FROM kv", &[])?.rows.len();
    if rows != expected {
        return Err(format!(
            "concurrency suite lost writes: {expected} transactions committed, {rows} rows in the file"
        )
        .into());
    }
    Ok(())
}

fn sqlite_writers(
    path: &Path,
    writers: usize,
    txns: usize,
    durability: SqliteDurability,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    remove_sqlite_files(path);

    let creator = open_sqlite(path, durability)?;
    creator.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", [])?;
    drop(creator);

    let connections: Vec<rusqlite::Connection> = (0..writers)
        .map(|_| open_sqlite(path, durability))
        .collect::<Result<_, _>>()?;

    let mut committed = 0;
    let mut samples = Vec::with_capacity(txns * writers);
    let started = Instant::now();
    for round in 0..txns {
        for (index, conn) in connections.iter().enumerate() {
            let id = (round * writers + index + 1) as i64;
            let attempted = Instant::now();
            conn.execute(
                "INSERT INTO kv (id, n) VALUES (?1, ?2)",
                rusqlite::params![id, id],
            )?;
            samples.push(attempted.elapsed());
            committed += 1;
        }
    }
    let elapsed = started.elapsed();

    let rows: i64 = connections[0].query_row("SELECT count(*) FROM kv", [], |row| row.get(0))?;
    if rows as usize != txns * writers {
        return Err(format!("SQLite baseline lost writes: {rows} of {}", txns * writers).into());
    }

    drop(connections);
    remove_sqlite_files(path);
    Ok(Outcome {
        label: durability.label().to_string(),
        writers,
        committed,
        // A lock made every writer wait its turn; none was ever rolled back.
        conflicts: 0,
        elapsed,
        samples,
    })
}
