//! A clean profiling harness for the query phase only.
//!
//! `AHL-472` profiled `inlaysql-bench --suite joins` directly and found the
//! result unpublishable: the process also ran SQLite's setup and its own
//! bulk-load writes in the same profiled window, and SQLite's file locking
//! showed up in a *read* sample. `PERF.md` names this explicitly and asks
//! whoever picks up `ValueRef` (AHL-478) to build something that isolates the
//! query phase before trusting a profile again. This binary is that
//! something, and it is committed so the next perf pass does not have to
//! rebuild it.
//!
//! What it does *not* do, on purpose:
//! * **No SQLite in a sampled window.** `inlaysql-bench`'s suites open a
//!   `rusqlite::Connection` in the same process to produce a comparison row;
//!   that connection's own file locking is what leaked into AHL-472's
//!   supposedly read-only sample. No suite here opens SQLite while the
//!   [`PHASE_MARKER`] window is running. The one place SQLite appears at all
//!   is `--tail` (below), which opens it *after* InlaySQL's timed loop has
//!   finished, for a comparison histogram a sampler is not meant to see.
//! * **No setup inside the sampled window.** Schema, bulk load and index
//!   build all happen before this process prints the
//!   [`PHASE_MARKER`] line. A profiler is meant to attach *after* that line
//!   and for exactly [`Config::seconds`] afterwards — see the module-level
//!   usage note below for the two-process recipe.
//!
//! # Usage
//!
//! ```sh
//! cargo build --release -p inlaysql-bench --bin profile
//! target/release/profile --suite joins --rows 20000 --limit 10 --seconds 30 &
//! pid=$!
//! # Wait for the "query phase" marker on stderr, then sample it, e.g.:
//! #   sample "$pid" 30 -f /tmp/joins.sample
//! wait "$pid"
//! ```
//!
//! `--suite` is one of `points`, `indexed`, `joins` — the same three
//! `PERF.md` and `bench/run.sh` use, with the same schemas, so a profile
//! taken here lines up with the wall-clock numbers those suites report — plus
//! `indexed-range`, added for AHL-479 so the entry-range walk (`indexed`'s
//! 50-row range shape in `bench/run.sh`) can be profiled on its own rather
//! than diluted by `indexed`'s point-lookup loop.
//!
//! # How the answer is consumed (AHL-535)
//!
//! `points` and `indexed-range` read their rows through
//! `Database::query_prepared_each_ref` and *touch every column they select* —
//! summing a row id and a body's length into a checksum the loop
//! `black_box`es at the end. Before AHL-535 both called `query_prepared` and
//! looked only at `rows.len()`.
//!
//! Both halves of that change matter and they pull in opposite directions.
//! Stepping rows through the borrowing API is what SQLite's side has always
//! done — `sqlite3_step` into caller-owned registers — so the old harness was
//! comparing SQLite's step loop against a `Vec<Vec<Value>>` built and dropped
//! per query, which is a difference in API shape, not in engine speed.
//! Reading the columns is the other half: an answer nobody looks at is not a
//! workload anybody has, and a row count is a number the engine can produce
//! without the caller ever touching a byte. Together they measure the same
//! thing on both sides — descend, decode, hand the caller the bytes, caller
//! reads them.
//!
//! And `writes` (AHL-480), the write-mode sibling: it profiles the *durable
//! commit* loop instead of a read loop, one `INSERT` auto-committed at a time
//! on a single connection, matching `crates/inlaysql-bench/src/points.rs`'s
//! "point write (one durable commit each)" row — the shape `BENCHMARK.md`
//! measures against MySQL/PostgreSQL. Setup pre-loads past
//! [`CDC_WARMUP_ROWS`] so the timed window is steady state, not the first
//! 4,096 commits before the change-log retention window fills.
//!
//! And `batch-insert` (AHL-542), the multi-row sibling of `writes`: one
//! prepared `INSERT INTO batch (id, n) VALUES (?, ?), ... x100` per
//! auto-committed transaction, which is the shape
//! `bench/external/batch_driver.py` drives MySQL and PostgreSQL with and
//! `crates/inlaysql-bench/src/bin/sql_shapes.rs --mode batch` measures
//! wall-clock. `writes` cannot stand in for it: at one row per commit ~95% of
//! that loop is the fsync, so the per-row *structural* cost this suite exists
//! to show — the root-to-leaf path re-decoded, deep-cloned and re-encoded once
//! per row — is invisible there and is ~99% of the work here. Same table,
//! same `Durability::Full` (the engine default, one barrier per statement) and
//! the same `--batch` rows per statement the external driver uses.
//!
//! # `--tail` (AHL-552): the histogram a profile cannot draw
//!
//! `points --tail true` replaces the batched loop with one that times every
//! query on its own `Instant` pair and files the delta into a fixed
//! log2 histogram (`<250 ns`, then doublings up to `≥1 ms`), printed at the
//! end with the count *and the share of total time* in each bucket — a p99
//! says how far the tail reaches, the time share says how much of the ops/s
//! it is eating. For every query slower than `--tail-threshold` (µs, default
//! 3) it also records the query's ordinal and the delta of
//! [`inlaysql::Diagnostics`] across it — page-cache evictions, inserts,
//! index doublings, raw-leaf admissions, decodes, device reads, state-block
//! reads — so a slow query can say which rare event, if any, it paid for. The
//! ordinals are printed and their gaps summarised so a periodic cause shows
//! as a period. Then the same loop, the same histogram and the same seeded
//! key sequence run against SQLite in WAL mode in the same process, so
//! whatever share of the tail is the operating system's is visible on both
//! sides; and a third histogram of back-to-back `Instant::now()` pairs with
//! nothing between them shows what the harness's own timer contributes.
//!
//! A self-time profile could not have drawn this: a tail made of *rare*
//! events (a clock sweep, a table doubling, a scheduler preemption) does not
//! accumulate enough samples to appear as a frame, and the published bench
//! reports only p50/p95/p99 with no record of what any one slow query did.
//!
//! And `retrieval`, 2026-08-30: `PERF.md`'s vector-kernel section named this
//! the missing piece — "`bin/profile.rs` does not cover the retrieval suite
//! yet, and adding it is the first step" — because everything downstream of
//! the exact-`f32` kernel finding (the ~48% that is not arithmetic: candidate
//! heap, visited set, neighbour-list fetches) needed a way to isolate the
//! query phase the same way `joins` and `writes` already do. It mirrors
//! `crates/inlaysql-bench/src/main.rs`'s own retrieval suite: the same corpus
//! generator (`VOCABULARY`, `synthetic_document`/`synthetic_query`,
//! `hashed_embedding`), the same schema (`docs (id INTEGER, body TEXT,
//! embedding VECTOR(dim))`, indexed on both `body` and `embedding`) and the
//! same three query shapes (`vector_score`, `bm25_score`, `fuse`). The
//! generator is restated here rather than imported: `profile` and
//! `inlaysql-bench` are two separate binary crates in one package (see this
//! crate's `Cargo.toml`), so there is no library target to share it through —
//! the same reason `run_points`/`run_indexed`/`run_joins` restate their
//! siblings' schemas instead of calling into them. `--query` selects which
//! one shape the timed loop measures (`vector`, `bm25` or `hybrid`, default
//! `vector`) rather than cycling all three, for the reason `Shapes::LimitOnly`
//! exists on `joins`: the shape under investigation would otherwise get one
//! sample in three. `--quantized true` switches the embedding column to
//! `VECTOR(dim, INT8)`, to profile the int8 path in isolation.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use inlaysql::embedding::hashed_embedding;
use inlaysql::{Database, EngineOptions, FileDevice, Statement, Value};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::Rng;

/// Printed to stderr, flushed, the instant setup finishes and the timed loop
/// is about to start. A profiler attaches to this process's pid after seeing
/// this line, not before — attaching earlier would sample the setup writes
/// this harness exists to keep out of the window.
const PHASE_MARKER: &str = "PROFILE_QUERY_PHASE_START";

struct Config {
    suite: String,
    rows: usize,
    limit: usize,
    seconds: u64,
    seed: u64,
    payload: usize,
    /// [`EngineOptions::page_cache_bytes`]. `None` keeps the engine default
    /// ([`inlaysql_core::btree::DEFAULT_PAGE_CACHE_BYTES`], 8 MiB). Set this
    /// below the suite's working set (`--rows` x row size) to profile the
    /// miss path in isolation, per AHL-488: a cache that never stops evicting
    /// is a cleaner way to force misses than merely growing `--rows`, because
    /// it does not also change how many distinct pages the suite touches.
    page_cache_bytes: Option<usize>,
    /// Embedding width. `retrieval` only; matches
    /// `crates/inlaysql-bench/src/main.rs`'s `Config::dim` default so
    /// `--suite retrieval` without overrides reproduces the corpus behind the
    /// published `PERF.md`/`BENCHMARK.md` vector numbers (`--rows` stands in
    /// for that suite's `--docs`, which every other profiled suite already
    /// aliases to `--rows`).
    dim: usize,
    /// `retrieval` only: which of the three query shapes the timed loop runs
    /// — `vector`, `bm25` or `hybrid`.
    query: String,
    /// `retrieval` only: `VECTOR(dim, INT8)` instead of `VECTOR(dim)`, to
    /// profile the int8 path in isolation.
    quantized: bool,
    /// `batch-insert` only: rows per `INSERT` statement. The default is the
    /// 100 `bench/external/batch_driver.py` uses against MySQL and
    /// PostgreSQL, so this suite profiles the shape the published batch cell
    /// compares.
    batch: usize,
    /// `points` only: per-query tail histogram instead of the batched loop.
    /// See the module note "`--tail`".
    tail: bool,
    /// `--tail` only: a query slower than this many microseconds is recorded
    /// with its ordinal and its diagnostics delta.
    tail_threshold_micros: f64,
    /// `--tail` only: load the rows the way `crates/inlaysql-bench/src/points.rs`
    /// does — one durable auto-committed `INSERT` per row — instead of one
    /// batched transaction, so the handle enters the read loop in the state
    /// the published bench reads from.
    tail_durable: bool,
    /// `--tail` only: stop each engine's loop after this many queries rather
    /// than after `--seconds`; `0` means by seconds. `5000` with
    /// `--tail-durable true` is the published bench's read phase exactly.
    queries: u64,
}

impl Config {
    fn from_args() -> Self {
        let mut config = Config {
            suite: "joins".to_string(),
            rows: 20_000,
            limit: 10,
            seconds: 30,
            seed: 42,
            payload: 64,
            page_cache_bytes: None,
            dim: 384,
            query: "vector".to_string(),
            quantized: false,
            batch: 100,
            tail: false,
            tail_threshold_micros: 3.0,
            tail_durable: false,
            queries: 0,
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        for pair in args.chunks(2) {
            let [flag, value] = pair else {
                eprintln!("ignoring trailing argument {pair:?}");
                continue;
            };
            match flag.as_str() {
                "--suite" => config.suite = value.clone(),
                "--rows" => config.rows = value.parse().unwrap_or(config.rows),
                "--limit" => config.limit = value.parse().unwrap_or(config.limit),
                "--seconds" => config.seconds = value.parse().unwrap_or(config.seconds),
                "--seed" => config.seed = value.parse().unwrap_or(config.seed),
                "--payload" => config.payload = value.parse().unwrap_or(config.payload),
                "--page-cache-bytes" => {
                    config.page_cache_bytes = Some(value.parse().unwrap_or_else(|_| {
                        eprintln!("bad --page-cache-bytes {value:?}, keeping the default");
                        inlaysql_core::btree::DEFAULT_PAGE_CACHE_BYTES
                    }))
                }
                "--dim" => config.dim = value.parse().unwrap_or(config.dim),
                "--batch" => config.batch = value.parse().unwrap_or(config.batch).max(1),
                "--query" => match value.as_str() {
                    "vector" | "bm25" | "hybrid" => config.query = value.clone(),
                    other => {
                        eprintln!("unknown --query {other:?}, expected vector, bm25 or hybrid")
                    }
                },
                "--quantized" => match value.as_str() {
                    "true" | "1" => config.quantized = true,
                    "false" | "0" => config.quantized = false,
                    other => eprintln!("bad --quantized {other:?}, expected true or false"),
                },
                "--tail" => match value.as_str() {
                    "true" | "1" => config.tail = true,
                    "false" | "0" => config.tail = false,
                    other => eprintln!("bad --tail {other:?}, expected true or false"),
                },
                "--tail-threshold" => match value.parse::<f64>() {
                    Ok(micros) if micros > 0.0 => config.tail_threshold_micros = micros,
                    _ => eprintln!("bad --tail-threshold {value:?}, expected microseconds"),
                },
                "--tail-durable" => match value.as_str() {
                    "true" | "1" => config.tail_durable = true,
                    "false" | "0" => config.tail_durable = false,
                    other => eprintln!("bad --tail-durable {other:?}, expected true or false"),
                },
                "--queries" => config.queries = value.parse().unwrap_or(config.queries),
                other => eprintln!("unknown flag {other}"),
            }
        }
        config
    }

    /// Open a database at `path` honouring [`Config::page_cache_bytes`] when
    /// set, and the engine default otherwise. Every suite's setup goes
    /// through this instead of `Database::open` so `--page-cache-bytes` reaches
    /// all of them uniformly.
    fn open(&self, path: &Path) -> Result<Database, inlaysql::Error> {
        match self.page_cache_bytes {
            Some(page_cache_bytes) => Database::open_on_with_options(
                FileDevice::open(path)?,
                EngineOptions {
                    page_cache_bytes,
                    ..EngineOptions::default()
                },
            ),
            None => Database::open(path),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_args();
    let target = Path::new("target");
    std::fs::create_dir_all(target)?;
    let path = target.join(format!("profile-{}.inlay", config.suite));
    let _ = std::fs::remove_file(&path);

    eprintln!(
        "profile: suite={} rows={} limit={} seconds={} pid={}",
        config.suite,
        config.rows,
        config.limit,
        config.seconds,
        std::process::id()
    );

    let outcome = match config.suite.as_str() {
        "points" if config.tail => tail::run_points(&config, &path),
        "points" => run_points(&config, &path),
        "indexed" => run_indexed(&config, &path),
        "indexed-range" => run_indexed_range(&config, &path),
        "aggregate" => run_aggregate(&config, &path, AggregateShapes::Both),
        "aggregate-scalar" => run_aggregate(&config, &path, AggregateShapes::ScalarOnly),
        "joins" => run_joins(&config, &path, Shapes::All),
        "joins-limit" => run_joins(&config, &path, Shapes::LimitOnly),
        "writes" => run_writes(&config, &path),
        "batch-insert" => run_batch_insert(&config, &path),
        "retrieval" => run_retrieval(&config, &path),
        other => {
            eprintln!(
                "unknown suite `{other}`, expected points, indexed, indexed-range, aggregate, \
                 aggregate-scalar, joins, joins-limit, writes, batch-insert or retrieval"
            );
            std::process::exit(2);
        }
    };

    let _ = std::fs::remove_file(&path);
    outcome
}

/// Call once setup is finished and the read-only loop is about to begin.
fn announce_query_phase() {
    println!("{PHASE_MARKER}");
    std::io::stdout().flush().ok();
    eprintln!("{PHASE_MARKER}");
    std::io::stderr().flush().ok();
}

/// How many iterations run between clock checks.
///
/// `Instant::now()` is a syscall (`mach_absolute_time` on macOS), and a sampler
/// attached to this loop would otherwise spend a double-digit percentage of
/// its samples inside the harness's own timer rather than the engine — the
/// same artifact `PERF.md` names for `points` ("the harness's own per-lookup
/// timer, not engine work"). Checking the deadline once per batch instead of
/// once per iteration keeps that share negligible without changing what is
/// measured: the reported duration still comes from one `Instant::now()` pair
/// bracketing the whole loop.
const CLOCK_CHECK_BATCH: u64 = 256;

fn run_for(
    seconds: u64,
    mut one_iteration: impl FnMut() -> Result<(), inlaysql::Error>,
) -> Result<(u64, Duration), Box<dyn std::error::Error>> {
    let budget = Duration::from_secs(seconds);
    let started = Instant::now();
    let mut iterations: u64 = 0;
    loop {
        for _ in 0..CLOCK_CHECK_BATCH {
            one_iteration()?;
            iterations += 1;
        }
        if started.elapsed() >= budget {
            break;
        }
    }
    Ok((iterations, started.elapsed()))
}

fn report(label: &str, iterations: u64, elapsed: Duration) {
    println!(
        "{label}: {iterations} iterations in {elapsed:.2?} ({:.0} ops/s)",
        iterations as f64 / elapsed.as_secs_f64().max(f64::EPSILON)
    );
}

/// `points`: one row by primary key, matching `crates/inlaysql-bench/src/points.rs`'s
/// schema (`kv (id INTEGER PRIMARY KEY, body TEXT)`), the same as `PERF.md`
/// section 2's traced workload.
fn run_points(config: &Config, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = config.open(path)?;
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
    let insert = db.prepare("INSERT INTO kv (id, body) VALUES (?, ?)")?;
    let payload = "x".repeat(config.payload);

    db.begin()?;
    for id in 1..=config.rows as i64 {
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(
            &insert,
            &[Value::Integer(id), Value::Text(payload.clone().into())],
        ) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(
                &insert,
                &[Value::Integer(id), Value::Text(payload.clone().into())],
            )?;
        }
    }
    db.commit()?;

    let lookup = db.prepare("SELECT body FROM kv WHERE id = ?")?;
    let mut rng = SeededRng::new(config.seed);
    let rows = config.rows as u64;

    // Read through the borrowing API, and read the column rather than
    // counting the rows: `query_prepared_each_ref` hands back a `&str` into
    // the page, so summing its length is the whole cost of consuming the
    // answer. See `read_the_answer` for why this is the fair shape and not
    // the flattering one.
    let mut checksum = 0u64;
    announce_query_phase();
    let (iterations, elapsed) = run_for(config.seconds, || {
        let key = 1 + (rng.next_u64() % rows) as i64;
        let delivered = db.query_prepared_each_ref(&lookup, &[Value::Integer(key)], |row| {
            checksum += row[0].as_str().map_or(0, str::len) as u64;
            Ok(())
        })?;
        debug_assert_eq!(delivered, 1);
        Ok(())
    })?;
    std::hint::black_box(checksum);
    report("points", iterations, elapsed);
    Ok(())
}

/// `indexed`: `WHERE email = ?`, matching `crates/inlaysql-bench/src/indexed.rs`'s
/// schema (`users (id INTEGER PRIMARY KEY, email TEXT, body TEXT)`, indexed on
/// `email`).
fn run_indexed(config: &Config, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = config.open(path)?;
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, body TEXT)",
        &[],
    )?;
    let insert = db.prepare("INSERT INTO users (id, email, body) VALUES (?, ?, ?)")?;
    let payload = "x".repeat(config.payload);

    db.begin()?;
    for id in 1..=config.rows as i64 {
        let bound = [
            Value::Integer(id),
            Value::Text(email(id).into()),
            Value::Text(payload.clone().into()),
        ];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &bound)?;
        }
    }
    db.commit()?;
    db.execute("CREATE INDEX users_email ON users (email) USING BTREE", &[])?;

    let lookup = db.prepare("SELECT id, body FROM users WHERE email = ?")?;
    let mut rng = SeededRng::new(config.seed);
    let rows = config.rows as i64;

    announce_query_phase();
    let (iterations, elapsed) = run_for(config.seconds, || {
        let id = 1 + (rng.next_u64() % rows as u64) as i64;
        let result = db.query_prepared(&lookup, &[Value::Text(email(id).into())])?;
        debug_assert_eq!(result.rows.len(), 1);
        Ok(())
    })?;
    report("indexed", iterations, elapsed);
    Ok(())
}

fn email(id: i64) -> String {
    format!("user{id:012}@example.com")
}

/// Rows one range query returns, matching `crates/inlaysql-bench/src/indexed.rs`'s
/// `RANGE_SIZE` — a small, exact probe, not a scan pretending to be one.
const RANGE_SIZE: usize = 50;

/// `indexed-range`: `WHERE email >= ? AND email < ?`, `RANGE_SIZE` rows a
/// query, over the same schema and index as `indexed`. Split out as its own
/// suite (AHL-479) rather than folded into `run_indexed`'s loop, so a profile
/// of the entry-range walk is not diluted by the point-lookup shape — the
/// same reason `joins` gets its own suite rather than sharing `points`'.
/// `aggregate`: the `GROUP BY` and scalar-aggregate shapes
/// `crates/inlaysql-bench/src/bin/sql_shapes.rs` measures against MySQL and
/// PostgreSQL, which is the worst multiple `BENCHMARK.md` publishes against
/// anyone — 3.4-6.0x slower than both. Added so that loss can be attributed
/// before anything is built to fix it.
///
/// Same schema and the same 100-bucket `n` column `read_driver.py` builds for
/// the opponents, so what is profiled here is the shape that lost.
///
/// Which of the two shapes `run_aggregate` cycles through.
///
/// `AHL-546` split this out because the two shapes turned out to cost about
/// the same — `SELECT n, COUNT(*) FROM users GROUP BY n` at 210/s against
/// the scalar `SELECT COUNT(*), MIN(id), MAX(id) FROM users` at 225/s,
/// `PERF.md` 2026-09-03 — which a `--suite aggregate` profile that alternates
/// between them cannot distinguish: half its samples are the grouped shape's
/// cost, diluting whatever the scalar shape's own split shows. `ScalarOnly`
/// is `--suite aggregate-scalar`, mirroring `Shapes::LimitOnly` below for the
/// same reason — one suite, one shape, one profile that is not an average of
/// two different costs.
enum AggregateShapes {
    /// Both shapes, alternating — `--suite aggregate`'s original behaviour,
    /// kept so an existing profile invocation still measures what it always
    /// measured.
    Both,
    /// Only the scalar shape.
    ScalarOnly,
}

fn run_aggregate(
    config: &Config,
    path: &Path,
    shapes: AggregateShapes,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = config.open(path)?;
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, body TEXT, n INTEGER)",
        &[],
    )?;
    let insert = db.prepare("INSERT INTO users (id, email, body, n) VALUES (?, ?, ?, ?)")?;
    let payload = "x".repeat(config.payload);

    db.begin()?;
    for id in 1..=config.rows as i64 {
        let bound = [
            Value::Integer(id),
            Value::Text(email(id).into()),
            Value::Text(payload.clone().into()),
            Value::Integer(id % 100),
        ];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &bound)?;
        }
    }
    db.commit()?;
    db.execute("CREATE INDEX users_email ON users (email) USING BTREE", &[])?;
    db.execute("ANALYZE", &[])?;

    let group = db.prepare("SELECT n, COUNT(*) FROM users GROUP BY n")?;
    let scalar = db.prepare("SELECT COUNT(*), MIN(id), MAX(id) FROM users")?;
    // Warmed outside the timed window, as every other suite here does.
    db.query_prepared(&group, &[])?;
    db.query_prepared(&scalar, &[])?;

    let timed: Vec<&Statement> = match shapes {
        AggregateShapes::Both => vec![&group, &scalar],
        AggregateShapes::ScalarOnly => vec![&scalar],
    };
    let label = match shapes {
        AggregateShapes::Both => "aggregate",
        AggregateShapes::ScalarOnly => "aggregate-scalar",
    };
    announce_query_phase();
    let mut cycle = 0usize;
    let (iterations, elapsed) = run_for(config.seconds, || {
        let shape = timed[cycle % timed.len()];
        cycle += 1;
        db.query_prepared(shape, &[]).map(|_| ())
    })?;
    report(label, iterations, elapsed);
    Ok(())
}

fn run_indexed_range(config: &Config, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = config.open(path)?;
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, body TEXT)",
        &[],
    )?;
    let insert = db.prepare("INSERT INTO users (id, email, body) VALUES (?, ?, ?)")?;
    let payload = "x".repeat(config.payload);

    db.begin()?;
    for id in 1..=config.rows as i64 {
        let bound = [
            Value::Integer(id),
            Value::Text(email(id).into()),
            Value::Text(payload.clone().into()),
        ];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &bound)?;
        }
    }
    db.commit()?;
    db.execute("CREATE INDEX users_email ON users (email) USING BTREE", &[])?;

    let range = db.prepare("SELECT id, body FROM users WHERE email >= ? AND email < ?")?;
    let mut rng = SeededRng::new(config.seed);
    let bound = (config.rows.saturating_sub(RANGE_SIZE)).max(1) as u64;

    // Both columns are read, per row, for the reason this module's "How the
    // answer is consumed" gives: a fifty-row answer nobody looks at is not
    // the workload anybody has.
    let mut checksum = 0u64;
    announce_query_phase();
    let (iterations, elapsed) = run_for(config.seconds, || {
        let start = 1 + (rng.next_u64() % bound) as i64;
        let delivered = db.query_prepared_each_ref(
            &range,
            &[
                Value::Text(email(start).into()),
                Value::Text(email(start + RANGE_SIZE as i64).into()),
            ],
            |row| {
                checksum += row[0].as_i64().unwrap_or(0) as u64;
                checksum += row[1].as_str().map_or(0, str::len) as u64;
                Ok(())
            },
        )?;
        debug_assert_eq!(delivered, RANGE_SIZE);
        Ok(())
    })?;
    std::hint::black_box(checksum);
    report("indexed-range", iterations, elapsed);
    Ok(())
}

/// Which of `run_joins`'s four query shapes the timed loop cycles through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shapes {
    /// All four, as `PERF.md`'s "join and range profile" section profiled them.
    All,
    /// Only the two `LIMIT 10` shapes.
    ///
    /// These are the standing loss `BENCHMARK.md` publishes — 4.65x and 5.81x
    /// slower than SQLite where the full-join shapes are 1.20x slower and 3.65x
    /// faster — and they cannot be seen in the `All` profile at all. A full
    /// join takes ~11 ms and a `LIMIT 10` takes ~20 µs, so cycling the four
    /// evenly gives the two shapes under investigation about one sample in
    /// five hundred. Profiling them together does not dilute the answer, it
    /// erases it.
    LimitOnly,
}

/// `joins`: `users` x `posts`, PK inner and secondary-index inner, cycling
/// through the shapes `crates/inlaysql-bench/src/joins.rs` measures — the
/// exact workload `PERF.md`'s "join and range profile" section names
/// (`--suite joins --rows 20000 --queries 100 --limit 10`).
fn run_joins(
    config: &Config,
    path: &Path,
    shapes: Shapes,
) -> Result<(), Box<dyn std::error::Error>> {
    const POSTS_PER_USER: usize = 8;
    let mut db = config.open(path)?;
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
        &[],
    )?;
    db.execute(
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
        &[],
    )?;
    let payload = "x".repeat(config.payload);

    let insert_user = db.prepare("INSERT INTO users (id, name) VALUES (?, ?)")?;
    let insert_post = db.prepare("INSERT INTO posts (id, user_id, title) VALUES (?, ?, ?)")?;
    db.begin()?;
    for id in 1..=config.rows as i64 {
        let bound = [Value::Integer(id), Value::Text(format!("user{id}").into())];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert_user, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert_user, &bound)?;
        }
    }
    let total_posts = config.rows * POSTS_PER_USER;
    for post_id in 1..=total_posts as i64 {
        let user_id = 1 + ((post_id - 1) % config.rows as i64);
        let bound = [
            Value::Integer(post_id),
            Value::Integer(user_id),
            Value::Text(payload.clone().into()),
        ];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert_post, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert_post, &bound)?;
        }
    }
    db.commit()?;
    db.execute(
        "CREATE INDEX posts_user_id ON posts (user_id) USING BTREE",
        &[],
    )?;
    // The same statistics `crates/inlaysql-bench/src/joins.rs` collects before
    // it times anything. Without it the planner falls back to its shape rules
    // and this profile measures a *different planner state* than the benchmark
    // it is meant to explain — which is how a join-ordering change measured as
    // "no difference" here while changing the plan there.
    db.execute("ANALYZE", &[])?;

    let pk_inner = db
        .prepare("SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id")?;
    let pk_inner_limit = db.prepare(&format!(
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id LIMIT {}",
        config.limit
    ))?;
    let indexed_inner = db.prepare(
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id",
    )?;
    let indexed_inner_limit = db.prepare(&format!(
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id LIMIT {}",
        config.limit
    ))?;
    // Force the first plan/index build cost outside the timed window, exactly
    // as the suite's own comment does for the retrieval workload.
    let all: [&Statement; 4] = [
        &pk_inner,
        &pk_inner_limit,
        &indexed_inner,
        &indexed_inner_limit,
    ];
    // Every shape is warmed, whichever subset is then timed, so the two loops
    // start from the same plan cache and the same warm pages.
    for shape in &all {
        db.query_prepared(shape, &[])?;
    }
    let timed: Vec<&Statement> = match shapes {
        Shapes::All => all.to_vec(),
        Shapes::LimitOnly => vec![&pk_inner_limit, &indexed_inner_limit],
    };

    announce_query_phase();
    let mut cycle = 0usize;
    let (iterations, elapsed) = run_for(config.seconds, || {
        let shape = timed[cycle % timed.len()];
        cycle += 1;
        db.query_prepared(shape, &[]).map(|_| ())
    })?;
    report(
        match shapes {
            Shapes::All => "joins",
            Shapes::LimitOnly => "joins-limit",
        },
        iterations,
        elapsed,
    );
    Ok(())
}

/// Rows pre-loaded (batched, outside the timed window) before the profiled
/// loop starts.
///
/// `inlaysql_core::cdc::CDC_RETENTION` is 4,096 and is `pub(crate)`, not part
/// of this crate's public surface, so it is restated here rather than
/// imported — checked against the real constant by
/// `a_warmup_matches_cdc_retention` below, so the two cannot silently drift.
/// Below that many committed statements, every commit's change-log write is
/// a pure insert at the tail of the `cdc:` key range; at and above it, each
/// commit also *expires* the oldest surviving entry, which sits in a
/// different, far leaf of the shared row/metadata tree. `BENCHMARK.md`'s
/// `SUITE=points` row commits 20,000 rows one at a time, so ~80% of that
/// published number is already in the steady state this warms straight to.
const CDC_WARMUP_ROWS: usize = 4_096;

/// `writes` (AHL-480): the durable-commit loop, one `INSERT` auto-committed
/// per iteration on a single connection — matching
/// `crates/inlaysql-bench/src/points.rs`'s "point write (one durable commit
/// each)" row, which is what `BENCHMARK.md`'s sequential-write comparison
/// against MySQL/PostgreSQL measures. No `SELECT` runs in the timed window;
/// this is `points`'s write mode, isolated the same way `run_points`'s read
/// loop isolates reads.
fn run_writes(config: &Config, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = config.open(path)?;
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
    let insert = db.prepare("INSERT INTO kv (id, body) VALUES (?, ?)")?;
    let payload = "x".repeat(config.payload);

    // Warm past the change-log retention window in one batched transaction —
    // this part is setup, not the durable-commit path being profiled, so it
    // pays one fsync rather than thousands.
    let warmup = CDC_WARMUP_ROWS.max(1);
    db.begin()?;
    for id in 1..=warmup as i64 {
        let bound = [Value::Integer(id), Value::Text(payload.clone().into())];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &bound)?;
        }
    }
    db.commit()?;

    let mut next_id = warmup as i64 + 1;
    announce_query_phase();
    let (iterations, elapsed) = run_for(config.seconds, || {
        db.execute_prepared(
            &insert,
            &[Value::Integer(next_id), Value::Text(payload.clone().into())],
        )?;
        next_id += 1;
        Ok(())
    })?;
    report("writes", iterations, elapsed);
    Ok(())
}

/// Words the synthetic corpus is drawn from — restated verbatim from
/// `crates/inlaysql-bench/src/main.rs`'s `VOCABULARY`. See the module note on
/// why this is a copy rather than an import.
const VOCABULARY: &[&str] = &[
    "database",
    "embedded",
    "vector",
    "search",
    "index",
    "storage",
    "engine",
    "query",
    "rust",
    "async",
    "cache",
    "page",
    "commit",
    "replica",
    "shard",
    "tokenizer",
    "ranking",
    "recall",
    "latency",
    "throughput",
    "hybrid",
    "retrieval",
    "segment",
    "compaction",
    "journal",
];

/// A corpus document: 12-35 words, matching
/// `crates/inlaysql-bench/src/main.rs`'s `synthetic_document`.
fn synthetic_document(rng: &mut SeededRng) -> String {
    let length = 12 + (rng.next_u64() % 24) as usize;
    words(rng, length)
}

/// A query: 2-4 words, matching `crates/inlaysql-bench/src/main.rs`'s
/// `synthetic_query`.
fn synthetic_query(rng: &mut SeededRng) -> String {
    let length = 2 + (rng.next_u64() % 3) as usize;
    words(rng, length)
}

fn words(rng: &mut SeededRng, count: usize) -> String {
    (0..count)
        .map(|_| VOCABULARY[(rng.next_u64() % VOCABULARY.len() as u64) as usize])
        .collect::<Vec<_>>()
        .join(" ")
}

/// `retrieval`, 2026-08-30: vector / BM25 / hybrid query latency, matching
/// `crates/inlaysql-bench/src/main.rs`'s retrieval suite — the same schema
/// (`docs (id INTEGER, body TEXT, embedding VECTOR(dim))`, indexed on both
/// `body` and `embedding`) and the same corpus generator. See the module note
/// for why `--query` measures one shape at a time instead of cycling all
/// three, and for `--quantized`.
/// The batch-insert shape (AHL-542): `--batch` rows in one prepared
/// multi-row `INSERT`, one auto-committed transaction per statement.
///
/// The timed loop checks the clock every statement rather than every
/// [`CLOCK_CHECK_BATCH`], because one iteration here is a hundred rows and a
/// durable commit — coarse batching would overshoot `--seconds` by minutes,
/// not milliseconds.
///
/// Setup pre-loads past [`CDC_WARMUP_ROWS`] for the reason `run_writes` does:
/// the change-log retention window has to be full before the window is steady
/// state.
fn run_batch_insert(config: &Config, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = config.open(path)?;
    db.execute(
        "CREATE TABLE batch (id INTEGER PRIMARY KEY, n INTEGER)",
        &[],
    )?;
    let placeholders = vec!["(?, ?)"; config.batch].join(",");
    let insert = db.prepare(&format!("INSERT INTO batch (id, n) VALUES {placeholders}"))?;

    let mut next_id: i64 = 1;
    let bind = |first: i64| {
        let mut params = Vec::with_capacity(config.batch * 2);
        for id in first..first + config.batch as i64 {
            params.push(Value::Integer(id));
            params.push(Value::Integer(id % 1000));
        }
        params
    };

    // Warm past the change-log retention window in one batched transaction,
    // exactly as `run_writes` does: setup, not the loop being profiled.
    let warmup = CDC_WARMUP_ROWS.max(1);
    db.begin()?;
    while (next_id as usize) <= warmup {
        let params = bind(next_id);
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &params) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &params)?;
        }
        next_id += config.batch as i64;
    }
    db.commit()?;

    announce_query_phase();
    let budget = Duration::from_secs(config.seconds);
    let started = Instant::now();
    let mut statements: u64 = 0;
    while started.elapsed() < budget {
        db.execute_prepared(&insert, &bind(next_id))?;
        next_id += config.batch as i64;
        statements += 1;
    }
    let elapsed = started.elapsed();
    report("batch-insert statements", statements, elapsed);
    report(
        "batch-insert rows",
        statements * config.batch as u64,
        elapsed,
    );
    Ok(())
}

fn run_retrieval(config: &Config, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = config.open(path)?;
    let vector_type = if config.quantized {
        format!("VECTOR({}, INT8)", config.dim)
    } else {
        format!("VECTOR({})", config.dim)
    };
    db.execute(
        &format!("CREATE TABLE docs (id INTEGER, body TEXT, embedding {vector_type})"),
        &[],
    )?;
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])?;
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])?;

    let insert = db.prepare("INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)")?;
    let mut rng = SeededRng::new(config.seed);
    // The corpus is generated before the query stream, exactly as
    // `main.rs`'s retrieval suite draws both from one seeded stream in that
    // order, so `--seed` reproduces the same documents here too.
    let corpus: Vec<String> = (0..config.rows)
        .map(|_| synthetic_document(&mut rng))
        .collect();

    db.begin()?;
    for (index, body) in corpus.iter().enumerate() {
        let bound = [
            Value::Integer(index as i64),
            Value::Text(body.clone().into()),
            Value::Vector(hashed_embedding(body, config.dim)),
        ];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &bound)?;
        }
    }
    db.commit()?;

    let vector_only = db.prepare(&format!(
        "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC LIMIT {}",
        config.limit
    ))?;
    let text_only = db.prepare(&format!(
        "SELECT id, bm25_score(body, ?) AS score FROM docs ORDER BY score DESC LIMIT {}",
        config.limit
    ))?;
    let hybrid = db.prepare(&format!(
        "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
         FROM docs ORDER BY score DESC LIMIT {}",
        config.limit
    ))?;

    // Both indexes commit on first read, not on `CREATE INDEX` — warm both
    // kinds of build here, whichever shape ends up timed, so neither the
    // HNSW graph build nor the BM25 index build leaks into the timed window.
    db.query_prepared(
        &vector_only,
        &[Value::Vector(hashed_embedding("warmup", config.dim))],
    )?;
    db.query_prepared(&text_only, &[Value::Text("warmup".to_string().into())])?;

    announce_query_phase();
    let (iterations, elapsed) = run_for(config.seconds, || {
        let query = synthetic_query(&mut rng);
        match config.query.as_str() {
            "bm25" => db
                .query_prepared(&text_only, &[Value::Text(query.into())])
                .map(|_| ()),
            "hybrid" => db
                .query_prepared(
                    &hybrid,
                    &[
                        Value::Vector(hashed_embedding(&query, config.dim)),
                        Value::Text(query.into()),
                    ],
                )
                .map(|_| ()),
            _ => db
                .query_prepared(
                    &vector_only,
                    &[Value::Vector(hashed_embedding(&query, config.dim))],
                )
                .map(|_| ()),
        }
    })?;
    report(&format!("retrieval-{}", config.query), iterations, elapsed);
    Ok(())
}

/// `--tail` (AHL-552): per-query latency histograms for the point read, on
/// both engines, with a diagnostics delta for every slow query. See the
/// module note "`--tail`".
mod tail {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use inlaysql::{Diagnostics, Value};
    use inlaysql_core::mem::SeededRng;
    use inlaysql_core::Rng;

    use super::{announce_query_phase, Config};

    /// The histogram's first upper bound, in nanoseconds; every later bucket
    /// doubles it. Twelve doublings from 250 ns is 1,024,000 ns, so the last
    /// bounded bucket ends at ~1 ms and the final one is open.
    const FIRST_BOUND_NS: u64 = 250;
    /// Bounded buckets: `<250 ns`, then `[250 ns, 500 ns)` … `[512 µs, 1 ms)`.
    const BOUNDED: usize = 13;
    /// All buckets, the open `≥1 ms` one included.
    const BUCKETS: usize = BOUNDED + 1;

    /// Per-query samples and their log2 histogram.
    struct Histogram {
        counts: [u64; BUCKETS],
        /// Nanoseconds summed per bucket, so the share of *time* can be
        /// reported next to the share of *queries*.
        nanos: [u64; BUCKETS],
        /// Every sample, up to the capacity reserved before the loop began,
        /// so p50/p95/p99 can be exact rather than bucket-resolution. Filled
        /// only while it can grow without reallocating: the `Vec` of samples
        /// is itself on the list of suspects, so it is never allowed to be
        /// one.
        samples: Vec<u32>,
        /// Samples the capacity could not hold — reported, never silently
        /// dropped.
        overflow: u64,
    }

    impl Histogram {
        fn with_capacity(samples: usize) -> Self {
            Self {
                counts: [0; BUCKETS],
                nanos: [0; BUCKETS],
                samples: Vec::with_capacity(samples),
                overflow: 0,
            }
        }

        #[inline]
        fn bucket(ns: u64) -> usize {
            let mut bound = FIRST_BOUND_NS;
            for i in 0..BOUNDED {
                if ns < bound {
                    return i;
                }
                bound <<= 1;
            }
            BOUNDED
        }

        #[inline]
        fn record(&mut self, ns: u64) {
            let at = Self::bucket(ns);
            self.counts[at] += 1;
            self.nanos[at] += ns;
            if self.samples.len() < self.samples.capacity() {
                self.samples.push(ns.min(u32::MAX as u64) as u32);
            } else {
                self.overflow += 1;
            }
        }

        fn total(&self) -> u64 {
            self.counts.iter().sum()
        }

        fn total_nanos(&self) -> u64 {
            self.nanos.iter().sum()
        }

        /// Count and nanoseconds of every sample at or above `threshold_ns`,
        /// from the exact samples.
        fn above(&self, threshold_ns: u64) -> (u64, u64) {
            self.samples
                .iter()
                .filter(|&&ns| ns as u64 >= threshold_ns)
                .fold((0, 0), |(n, t), &ns| (n + 1, t + ns as u64))
        }

        fn percentiles(&self) -> (u32, u32, u32, u32) {
            let mut sorted = self.samples.clone();
            sorted.sort_unstable();
            let at = |q: f64| {
                if sorted.is_empty() {
                    0
                } else {
                    sorted[((sorted.len() - 1) as f64 * q) as usize]
                }
            };
            (
                at(0.50),
                at(0.95),
                at(0.99),
                sorted.last().copied().unwrap_or(0),
            )
        }

        fn print(&self, label: &str, threshold_ns: u64) {
            let total = self.total().max(1);
            let total_nanos = self.total_nanos().max(1);
            let (p50, p95, p99, max) = self.percentiles();
            println!(
                "\n{label}: {} queries in {:.2?} ({:.0} ops/s), p50 {} p95 {} p99 {} max {}",
                self.total(),
                Duration::from_nanos(self.total_nanos()),
                self.total() as f64 / (self.total_nanos() as f64 / 1e9).max(f64::EPSILON),
                fmt_ns(p50 as u64),
                fmt_ns(p95 as u64),
                fmt_ns(p99 as u64),
                fmt_ns(max as u64),
            );
            if self.overflow > 0 {
                println!(
                    "  ({} samples past the reserved capacity are in the buckets but not the percentiles)",
                    self.overflow
                );
            }
            println!(
                "  {:<18} {:>10} {:>8} {:>12} {:>8}",
                "bucket", "queries", "share", "time", "share"
            );
            for i in 0..BUCKETS {
                let name = if i == 0 {
                    format!("<{}", fmt_ns(FIRST_BOUND_NS))
                } else if i == BOUNDED {
                    format!(">={}", fmt_ns(FIRST_BOUND_NS << (BOUNDED - 1)))
                } else {
                    format!(
                        "[{}, {})",
                        fmt_ns(FIRST_BOUND_NS << (i - 1)),
                        fmt_ns(FIRST_BOUND_NS << i)
                    )
                };
                if self.counts[i] == 0 {
                    continue;
                }
                println!(
                    "  {:<18} {:>10} {:>7.2}% {:>12} {:>7.2}%",
                    name,
                    self.counts[i],
                    100.0 * self.counts[i] as f64 / total as f64,
                    format!("{:.2?}", Duration::from_nanos(self.nanos[i])),
                    100.0 * self.nanos[i] as f64 / total_nanos as f64,
                );
            }
            let (slow, slow_nanos) = self.above(threshold_ns);
            println!(
                "  >= {} (the threshold): {slow} queries, {:.3}% of queries, {:.2}% of time",
                fmt_ns(threshold_ns),
                100.0 * slow as f64 / total as f64,
                100.0 * slow_nanos as f64 / total_nanos as f64,
            );
        }
    }

    fn fmt_ns(ns: u64) -> String {
        format!("{:.2?}", Duration::from_nanos(ns))
    }

    /// One counter picked out of a [`Diagnostics`] snapshot.
    type Counter<'a> = &'a dyn Fn(&Diagnostics) -> u64;

    /// What one slow InlaySQL query did, as the counters moved across it.
    struct SlowMark {
        ordinal: u64,
        ns: u64,
        delta: Diagnostics,
    }

    /// `after - before`, field by field; the two residency gauges carry
    /// `after`'s value.
    fn delta(before: &Diagnostics, after: &Diagnostics) -> Diagnostics {
        Diagnostics {
            page_cache_evictions: after.page_cache_evictions - before.page_cache_evictions,
            page_cache_inserts: after.page_cache_inserts - before.page_cache_inserts,
            page_cache_index_grows: after.page_cache_index_grows - before.page_cache_index_grows,
            page_cache_len: after.page_cache_len,
            page_cache_bytes: after.page_cache_bytes,
            raw_leaf_inserts: after.raw_leaf_inserts - before.raw_leaf_inserts,
            decodes: after.decodes - before.decodes,
            device_reads: after.device_reads - before.device_reads,
            state_reads: after.state_reads - before.state_reads,
        }
    }

    /// How many samples to reserve: a generous ceiling on what the loop can
    /// produce in `seconds`, so the sample `Vec` never reallocates mid-run.
    fn sample_capacity(seconds: u64) -> usize {
        (seconds as usize).max(1) * 3_000_000
    }

    /// Slow marks kept in full; past this the count still grows but the
    /// per-query record does not.
    const MAX_MARKS: usize = 200_000;

    pub(super) fn run_points(
        config: &Config,
        path: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let threshold_ns = (config.tail_threshold_micros * 1_000.0) as u64;
        let payload = "x".repeat(config.payload);
        let rows = config.rows as u64;
        let budget = Duration::from_secs(config.seconds);

        // --- InlaySQL ------------------------------------------------------
        let mut db = config.open(path)?;
        db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
        let insert = db.prepare("INSERT INTO kv (id, body) VALUES (?, ?)")?;
        if config.tail_durable {
            for id in 1..=config.rows as i64 {
                let bound = [Value::Integer(id), Value::Text(payload.clone().into())];
                db.execute_prepared(&insert, &bound)?;
            }
        } else {
            db.begin()?;
            for id in 1..=config.rows as i64 {
                let bound = [Value::Integer(id), Value::Text(payload.clone().into())];
                if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &bound) {
                    db.commit()?;
                    db.begin()?;
                    db.execute_prepared(&insert, &bound)?;
                }
            }
            db.commit()?;
        }
        let lookup = db.prepare("SELECT body FROM kv WHERE id = ?")?;
        let query_cap = if config.queries == 0 {
            u64::MAX
        } else {
            config.queries
        };

        let mut rng = SeededRng::new(config.seed);
        let mut checksum = 0u64;
        let mut histogram = Histogram::with_capacity(sample_capacity(config.seconds));
        let mut marks: Vec<SlowMark> = Vec::with_capacity(MAX_MARKS);
        let mut slow_total: u64 = 0;
        let mut ordinal: u64 = 0;
        // One snapshot per query, taken *after* the timed pair so the pair
        // brackets only the query; the delta is against the previous one.
        let first_snapshot = db.diagnostics();
        let mut last = first_snapshot;
        println!(
            "tail: rows={} seconds={} queries={} durable_setup={} threshold={} at start: {:?}",
            config.rows,
            config.seconds,
            config.queries,
            config.tail_durable,
            fmt_ns(threshold_ns),
            first_snapshot
        );

        announce_query_phase();
        let started = Instant::now();
        let deadline = started + budget;
        loop {
            let key = 1 + (rng.next_u64() % rows) as i64;
            let at = Instant::now();
            let delivered = db.query_prepared_each_ref(&lookup, &[Value::Integer(key)], |row| {
                checksum += row[0].as_str().map_or(0, str::len) as u64;
                Ok(())
            })?;
            let end = Instant::now();
            debug_assert_eq!(delivered, 1);
            let ns = (end - at).as_nanos() as u64;
            histogram.record(ns);
            let now = db.diagnostics();
            if ns >= threshold_ns {
                slow_total += 1;
                if marks.len() < MAX_MARKS {
                    marks.push(SlowMark {
                        ordinal,
                        ns,
                        delta: delta(&last, &now),
                    });
                }
            }
            last = now;
            ordinal += 1;
            if end >= deadline || ordinal >= query_cap {
                break;
            }
        }
        let inlay_elapsed = started.elapsed();
        std::hint::black_box(checksum);
        let final_snapshot = db.diagnostics();

        histogram.print("InlaySQL point read", threshold_ns);
        println!(
            "  wall {:.2?} for {ordinal} queries ({:.0} ops/s including the harness's timer pair)",
            inlay_elapsed,
            ordinal as f64 / inlay_elapsed.as_secs_f64().max(f64::EPSILON)
        );
        println!(
            "  counters over the whole run: {:?}",
            delta(&first_snapshot, &final_snapshot)
        );
        report_marks(&marks, slow_total, ordinal, threshold_ns);
        drop(db);
        let _ = std::fs::remove_file(path);

        // --- SQLite, WAL, same process, same loop, same keys ------------------
        let sqlite_path = path.with_extension("sqlite");
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", sqlite_path.display()));
        }
        let conn = rusqlite::Connection::open(&sqlite_path)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", [])?;
        {
            let mut insert = conn.prepare("INSERT INTO kv (id, body) VALUES (?1, ?2)")?;
            if !config.tail_durable {
                conn.execute_batch("BEGIN")?;
            }
            for id in 1..=config.rows as i64 {
                insert.execute(rusqlite::params![id, payload])?;
            }
            if !config.tail_durable {
                conn.execute_batch("COMMIT")?;
            }
        }
        let mut lookup = conn.prepare("SELECT body FROM kv WHERE id = ?1")?;
        let mut rng = SeededRng::new(config.seed);
        let mut checksum = 0u64;
        let mut sqlite_hist = Histogram::with_capacity(sample_capacity(config.seconds));
        let mut sqlite_ordinal: u64 = 0;
        let mut sqlite_slow: Vec<u64> = Vec::with_capacity(MAX_MARKS);
        let started = Instant::now();
        let deadline = started + budget;
        loop {
            let key = 1 + (rng.next_u64() % rows) as i64;
            let at = Instant::now();
            lookup.query_row([key], |row| {
                checksum += row.get_ref(0)?.as_str().map(str::len).unwrap_or(0) as u64;
                Ok(())
            })?;
            let end = Instant::now();
            let ns = (end - at).as_nanos() as u64;
            sqlite_hist.record(ns);
            if ns >= threshold_ns && sqlite_slow.len() < MAX_MARKS {
                sqlite_slow.push(sqlite_ordinal);
            }
            sqlite_ordinal += 1;
            if end >= deadline || sqlite_ordinal >= query_cap {
                break;
            }
        }
        let sqlite_elapsed = started.elapsed();
        std::hint::black_box(checksum);
        sqlite_hist.print("SQLite (WAL, sync=NORMAL) point read", threshold_ns);
        println!(
            "  wall {:.2?} for {sqlite_ordinal} queries ({:.0} ops/s including the harness's timer pair)",
            sqlite_elapsed,
            sqlite_ordinal as f64 / sqlite_elapsed.as_secs_f64().max(f64::EPSILON)
        );
        report_ordinals("SQLite", &sqlite_slow, sqlite_ordinal);
        drop(lookup);
        drop(conn);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", sqlite_path.display()));
        }

        // --- The harness's own timer pair, nothing between ------------------
        let mut floor = Histogram::with_capacity(sample_capacity(1));
        let started = Instant::now();
        let deadline = started + Duration::from_secs(1);
        loop {
            let at = Instant::now();
            let end = Instant::now();
            floor.record((end - at).as_nanos() as u64);
            if end >= deadline {
                break;
            }
        }
        floor.print(
            "harness floor (two Instant::now() calls, nothing between)",
            threshold_ns,
        );
        Ok(())
    }

    /// Which counters moved on the slow queries, and where in the run they
    /// fell.
    fn report_marks(marks: &[SlowMark], slow_total: u64, total: u64, threshold_ns: u64) {
        println!(
            "\n  slow queries (>= {}): {slow_total} of {total}; {} recorded in full",
            fmt_ns(threshold_ns),
            marks.len()
        );
        if marks.is_empty() {
            return;
        }
        let count = |f: Counter| marks.iter().filter(|m| f(&m.delta) > 0).count();
        let nanos =
            |f: Counter| -> u64 { marks.iter().filter(|m| f(&m.delta) > 0).map(|m| m.ns).sum() };
        let named: [(&str, Counter); 7] = [
            ("page_cache_evictions", &|d| d.page_cache_evictions),
            ("page_cache_inserts", &|d| d.page_cache_inserts),
            ("page_cache_index_grows", &|d| d.page_cache_index_grows),
            ("raw_leaf_inserts", &|d| d.raw_leaf_inserts),
            ("decodes", &|d| d.decodes),
            ("device_reads", &|d| d.device_reads),
            ("state_reads", &|d| d.state_reads),
        ];
        println!(
            "  {:<24} {:>12} {:>8} {:>12}",
            "counter moved", "slow queries", "share", "their time"
        );
        for (name, f) in named.iter() {
            let n = count(f);
            println!(
                "  {:<24} {:>12} {:>7.2}% {:>12}",
                name,
                n,
                100.0 * n as f64 / marks.len() as f64,
                format!("{:.2?}", Duration::from_nanos(nanos(f)))
            );
        }
        let quiet = marks
            .iter()
            .filter(|m| {
                let d = &m.delta;
                d.page_cache_evictions
                    + d.page_cache_inserts
                    + d.page_cache_index_grows
                    + d.raw_leaf_inserts
                    + d.decodes
                    + d.device_reads
                    + d.state_reads
                    == 0
            })
            .count();
        println!(
            "  {:<24} {:>12} {:>7.2}%",
            "no counter moved",
            quiet,
            100.0 * quiet as f64 / marks.len() as f64
        );
        let ordinals: Vec<u64> = marks.iter().map(|m| m.ordinal).collect();
        report_ordinals("InlaySQL", &ordinals, total);
        println!("  first slow queries (ordinal: latency, moved counters):");
        for m in marks.iter().take(24) {
            let d = &m.delta;
            let mut moved = Vec::new();
            if d.page_cache_evictions > 0 {
                moved.push(format!("evictions={}", d.page_cache_evictions));
            }
            if d.page_cache_inserts > 0 {
                moved.push(format!("inserts={}", d.page_cache_inserts));
            }
            if d.page_cache_index_grows > 0 {
                moved.push(format!("index_grows={}", d.page_cache_index_grows));
            }
            if d.raw_leaf_inserts > 0 {
                moved.push(format!("raw_leaf_inserts={}", d.raw_leaf_inserts));
            }
            if d.decodes > 0 {
                moved.push(format!("decodes={}", d.decodes));
            }
            if d.device_reads > 0 {
                moved.push(format!("device_reads={}", d.device_reads));
            }
            if d.state_reads > 0 {
                moved.push(format!("state_reads={}", d.state_reads));
            }
            println!(
                "    {:>9}: {:>10}  {}  (resident {} pages, {} bytes)",
                m.ordinal,
                fmt_ns(m.ns),
                if moved.is_empty() {
                    "-".to_string()
                } else {
                    moved.join(" ")
                },
                d.page_cache_len,
                d.page_cache_bytes
            );
        }
    }

    /// Where the slow queries fell: the gaps between consecutive slow
    /// ordinals (a periodic cause shows as a tight gap distribution) and a
    /// twenty-slice timeline of the run (a burst shows as one hot slice).
    fn report_ordinals(engine: &str, ordinals: &[u64], total: u64) {
        if ordinals.len() < 2 {
            return;
        }
        let mut gaps: Vec<u64> = ordinals.windows(2).map(|w| w[1] - w[0]).collect();
        gaps.sort_unstable();
        let at = |q: f64| gaps[((gaps.len() - 1) as f64 * q) as usize];
        println!(
            "  {engine} slow-query gaps (in queries): min {} p10 {} median {} p90 {} max {}; mean {:.0}",
            gaps[0],
            at(0.10),
            at(0.50),
            at(0.90),
            gaps[gaps.len() - 1],
            total as f64 / ordinals.len() as f64
        );
        let mut slices = [0u64; 20];
        let width = (total / 20).max(1);
        for &o in ordinals {
            slices[((o / width) as usize).min(19)] += 1;
        }
        println!(
            "  {engine} slow queries per twentieth of the run: {:?}",
            slices
        );
    }
}

#[cfg(test)]
mod tests {
    /// Pins [`CDC_WARMUP_ROWS`] to the real, private
    /// `inlaysql_core::cdc::CDC_RETENTION` it restates in a doc comment, so a
    /// future change to the retention window cannot silently leave this
    /// harness warming to the wrong point. Exercised through the public
    /// `changes()` API rather than the private constant: after `2 *
    /// CDC_WARMUP_ROWS` committed statements, `Changes::floor` — the newest
    /// version no longer retained — is exactly `total - CDC_RETENTION` by
    /// `Engine::trim_changes`'s own arithmetic, so recovering `CDC_RETENTION`
    /// from it and comparing catches any drift.
    #[test]
    fn a_warmup_matches_cdc_retention() {
        use inlaysql::{Database, Value};

        let dir = std::env::temp_dir().join(format!(
            "inlaysql-profile-cdc-retention-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cdc.inlay");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", &[])
            .unwrap();
        let insert = db.prepare("INSERT INTO t (id, v) VALUES (?, ?)").unwrap();

        let total = super::CDC_WARMUP_ROWS as u64 * 2;
        for id in 1..=total as i64 {
            db.execute_prepared(&insert, &[Value::Integer(id), Value::Integer(id)])
                .unwrap();
        }

        let changes = db.changes(0).unwrap();
        assert!(
            changes.lost(0),
            "expected the log to have fallen behind after 2x the retention window"
        );
        assert_eq!(
            total - changes.floor,
            super::CDC_WARMUP_ROWS as u64,
            "CDC_WARMUP_ROWS no longer matches inlaysql_core::cdc::CDC_RETENTION; \
             update the constant in profile.rs to keep the writes suite's warmup \
             landing in steady state"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--page-cache-bytes` (AHL-488) exists to force the miss path: a cache
    /// that cannot hold the working set evicts on every insert, so every
    /// lookup after the first pays the full read-and-decode cost this task
    /// profiles. This is the correctness half of that — the *fast* path
    /// (default cache, warm hits) and the deliberately undersized one this
    /// flag turns on must answer the same query identically, so a profiling
    /// run is never trusted at the cost of the numbers it measures being
    /// wrong. Exercised at both `0` (cache off entirely) and a budget too
    /// small to hold one page, which still must not lose or corrupt a row.
    #[test]
    fn page_cache_bytes_changes_the_cache_not_the_answer() {
        use inlaysql::Value;

        for page_cache_bytes in [0usize, 1, 4096, super::CDC_WARMUP_ROWS] {
            let dir = std::env::temp_dir().join(format!(
                "inlaysql-profile-cache-bytes-{page_cache_bytes}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("cache.inlay");
            let config = super::Config {
                suite: "points".to_string(),
                rows: 200,
                limit: 10,
                seconds: 0,
                seed: 7,
                payload: 64,
                page_cache_bytes: Some(page_cache_bytes),
                dim: 384,
                query: "vector".to_string(),
                quantized: false,
                batch: 100,
                tail: false,
                tail_threshold_micros: 3.0,
                tail_durable: false,
                queries: 0,
            };

            let mut db = config.open(&path).unwrap();
            db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
                .unwrap();
            let insert = db
                .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
                .unwrap();
            for id in 1..=200i64 {
                db.execute_prepared(
                    &insert,
                    &[Value::Integer(id), Value::Text(format!("row-{id}").into())],
                )
                .unwrap();
            }

            let lookup = db.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
            // Every row, forwards then backwards, so both a page's first and
            // last entry are exercised whether or not the page it lives on
            // survives to the next lookup.
            for id in (1..=200i64).chain((1..=200i64).rev()) {
                let result = db.query_prepared(&lookup, &[Value::Integer(id)]).unwrap();
                assert_eq!(
                    result.rows.len(),
                    1,
                    "page_cache_bytes={page_cache_bytes}: row {id} missing"
                );
                assert_eq!(
                    result.rows[0][0],
                    Value::Text(format!("row-{id}").into()),
                    "page_cache_bytes={page_cache_bytes}: row {id} wrong"
                );
            }

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
