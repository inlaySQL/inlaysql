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
//! * **No SQLite.** `inlaysql-bench`'s suites open a `rusqlite::Connection`
//!   in the same process to produce a comparison row; that connection's own
//!   file locking is what leaked into AHL-472's supposedly read-only sample.
//!   This binary never links against SQLite at all.
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
//! And `writes` (AHL-480), the write-mode sibling: it profiles the *durable
//! commit* loop instead of a read loop, one `INSERT` auto-committed at a time
//! on a single connection, matching `crates/inlaysql-bench/src/points.rs`'s
//! "point write (one durable commit each)" row — the shape `BENCHMARK.md`
//! measures against MySQL/PostgreSQL. Setup pre-loads past
//! [`CDC_WARMUP_ROWS`] so the timed window is steady state, not the first
//! 4,096 commits before the change-log retention window fills.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

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
        "points" => run_points(&config, &path),
        "indexed" => run_indexed(&config, &path),
        "indexed-range" => run_indexed_range(&config, &path),
        "joins" => run_joins(&config, &path),
        "writes" => run_writes(&config, &path),
        other => {
            eprintln!(
                "unknown suite `{other}`, expected points, indexed, indexed-range, joins or writes"
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
        if let Err(inlaysql::Error::Transaction(_)) =
            db.execute_prepared(&insert, &[Value::Integer(id), Value::Text(payload.clone())])
        {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &[Value::Integer(id), Value::Text(payload.clone())])?;
        }
    }
    db.commit()?;

    let lookup = db.prepare("SELECT body FROM kv WHERE id = ?")?;
    let mut rng = SeededRng::new(config.seed);
    let rows = config.rows as u64;

    announce_query_phase();
    let (iterations, elapsed) = run_for(config.seconds, || {
        let key = 1 + (rng.next_u64() % rows) as i64;
        let result = db.query_prepared(&lookup, &[Value::Integer(key)])?;
        debug_assert_eq!(result.rows.len(), 1);
        Ok(())
    })?;
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
            Value::Text(email(id)),
            Value::Text(payload.clone()),
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
        let result = db.query_prepared(&lookup, &[Value::Text(email(id))])?;
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
            Value::Text(email(id)),
            Value::Text(payload.clone()),
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

    announce_query_phase();
    let (iterations, elapsed) = run_for(config.seconds, || {
        let start = 1 + (rng.next_u64() % bound) as i64;
        let result = db.query_prepared(
            &range,
            &[
                Value::Text(email(start)),
                Value::Text(email(start + RANGE_SIZE as i64)),
            ],
        )?;
        debug_assert_eq!(result.rows.len(), RANGE_SIZE);
        Ok(())
    })?;
    report("indexed-range", iterations, elapsed);
    Ok(())
}

/// `joins`: `users` x `posts`, PK inner and secondary-index inner, cycling
/// through all four shapes `crates/inlaysql-bench/src/joins.rs` measures — the
/// exact workload `PERF.md`'s "join and range profile" section names
/// (`--suite joins --rows 20000 --queries 100 --limit 10`).
fn run_joins(config: &Config, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
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
        let bound = [Value::Integer(id), Value::Text(format!("user{id}"))];
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
            Value::Text(payload.clone()),
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
    let shapes: [&Statement; 4] = [
        &pk_inner,
        &pk_inner_limit,
        &indexed_inner,
        &indexed_inner_limit,
    ];
    for shape in &shapes {
        db.query_prepared(shape, &[])?;
    }

    announce_query_phase();
    let mut cycle = 0usize;
    let (iterations, elapsed) = run_for(config.seconds, || {
        let shape = shapes[cycle % shapes.len()];
        cycle += 1;
        db.query_prepared(shape, &[]).map(|_| ())
    })?;
    report("joins", iterations, elapsed);
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
        let bound = [Value::Integer(id), Value::Text(payload.clone())];
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
            &[Value::Integer(next_id), Value::Text(payload.clone())],
        )?;
        next_id += 1;
        Ok(())
    })?;
    report("writes", iterations, elapsed);
    Ok(())
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
                    &[Value::Integer(id), Value::Text(format!("row-{id}"))],
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
                    Value::Text(format!("row-{id}")),
                    "page_cache_bytes={page_cache_bytes}: row {id} wrong"
                );
            }

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
