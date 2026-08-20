//! Benchmark harness for InlaySQL.
//!
//! Seven suites, all reproducible from one seed:
//!
//! * **retrieval** — ingest, then vector / BM25 / hybrid query latency. This is
//!   InlaySQL's own workload.
//! * **points** — one row by primary key, read and written, measured against
//!   SQLite. See [`points`] for how the comparison is kept fair.
//! * **indexed** — lookup by a non-key column, point and small range, indexed
//!   and unindexed, measured against SQLite with the same index. See
//!   [`indexed`].
//! * **joins** — `users` x `posts`, PK inner and secondary-index inner, with
//!   and without `LIMIT`, measured against SQLite with the same schema and
//!   index — the AHL-464 index nested-loop join shape. See [`joins`].
//! * **vectors** — approximate nearest neighbour recall and latency against
//!   `sqlite-vec`. See [`vectors`].
//! * **concurrency** — several writers on one file, measured against SQLite.
//!   See [`concurrency`], which explains what "concurrent" can mean here today.
//! * **sweep** — the HNSW parameter grid behind the shipped defaults. Not part
//!   of `--suite all`: it builds a graph per point and takes minutes. See
//!   [`sweep`].
//!
//! Plus three modes that are not suites: `--export <dir>` writes the
//! retrieval corpus, the queries and the correct answers so that engines this
//! binary cannot link against — DuckDB, pgvector — can be asked the same
//! questions from a container (see [`export`]); `--export-oltp <dir>` does
//! the same for the point-read/point-write workload — the rows to load and
//! the exact lookup-key sequence — so MySQL and PostgreSQL can be asked the
//! same questions [`points`] already asks SQLite in-process (see
//! [`oltp_export`]). Both write InlaySQL's own measured numbers, on the host,
//! alongside the exported files. `--oltp-replay <dir> --oltp-db <path>`
//! measures InlaySQL a second time over files `--export-oltp` already wrote,
//! with its database file at `<path>` instead of the host path the first
//! measurement used — `bench/external/compose.yml`'s `inlaysql-oltp` service
//! runs this inside a container, with `<path>` on a named Docker volume, the
//! same class MySQL and PostgreSQL use, so all three engines' commits cross
//! the same virtualised-disk boundary (see [`oltp_export::replay`]). See
//! `bench/compare.sh` and `bench/README.md`.
//!
//! Run it through `bench/run.sh`, which pins the parameters and records the
//! toolchain alongside the results.
//!
//! ```sh
//! cargo run --release -p inlaysql-bench -- --docs 5000 --queries 200 --seed 42
//! cargo run --release -p inlaysql-bench -- --suite points --rows 20000
//! cargo run --release -p inlaysql-bench -- --suite indexed --rows 100000
//! cargo run --release -p inlaysql-bench -- --suite joins --rows 20000 --queries 100 --limit 20
//! cargo run --release -p inlaysql-bench -- --suite concurrency --writers 8
//! cargo run --release -p inlaysql-bench -- --suite sweep --docs 20000
//! ```

mod concurrency;
mod export;
mod indexed;
mod joins;
mod oltp_export;
mod points;
mod sweep;
mod vectors;

use std::time::{Duration, Instant};

use inlaysql::embedding::hashed_embedding;
use inlaysql::{Database, Value};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::Rng;

/// Words the synthetic corpus is drawn from. A small vocabulary keeps term
/// statistics interesting: some words are common, some are rare.
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

/// Which suites to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suite {
    Retrieval,
    Points,
    /// Lookup by a non-key column, with the B-tree index and without it: a
    /// point query and a small range query, both indexed and unindexed.
    Indexed,
    /// `users` x `posts`, PK inner and secondary-index inner, with and
    /// without `LIMIT` — the AHL-464 index nested-loop join shape.
    Joins,
    Vectors,
    /// Exact versus int8 on both corpus shapes, without the unrelated paged
    /// and incremental auxiliary builds in the full vectors suite.
    Quantization,
    Concurrency,
    /// Deliberately outside [`Suite::All`]: a full grid is minutes of graph
    /// builds, and it answers a tuning question rather than reporting the
    /// engine's numbers.
    Sweep,
    All,
}

impl Suite {
    fn includes(self, other: Suite) -> bool {
        (self == Suite::All && other != Suite::Sweep) || self == other
    }
}

pub struct Config {
    docs: usize,
    queries: usize,
    seed: u64,
    dim: usize,
    limit: usize,
    /// Rows loaded by the point-workload suite.
    rows: usize,
    /// Primary-key lookups performed by the point-workload suite.
    lookups: usize,
    /// Payload bytes per row in the point-workload suite. Small on purpose:
    /// the point is to measure the tree and the sync, not memcpy.
    payload: usize,
    /// Highest writer count the concurrency suite sweeps up to.
    writers: usize,
    /// Transactions each writer commits in the concurrency suite.
    txns: usize,
    /// Where to write the retrieval corpus for the containerised baselines,
    /// if asked. See [`export`] and `bench/compare.sh`.
    export: Option<std::path::PathBuf>,
    /// Where to write the OLTP point-read/point-write workload for the
    /// containerised baselines, if asked. See [`oltp_export`] and
    /// `bench/compare.sh`.
    oltp_export: Option<std::path::PathBuf>,
    /// Where to *read* an already-exported OLTP workload from, for a second,
    /// containerised InlaySQL measurement over it. See
    /// [`oltp_export::replay`] and `bench/external/compose.yml`'s
    /// `inlaysql-oltp` service.
    oltp_replay: Option<std::path::PathBuf>,
    /// Where the containerised measurement's database file goes — a named
    /// Docker volume, not the corpus directory `oltp_replay` reads from and
    /// not the host path the first (`--export-oltp`) measurement used.
    /// Defaults to `<oltp_replay>/bench-oltp-container.inlay` if not given.
    oltp_db: Option<std::path::PathBuf>,
    suite: Suite,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            docs: 2_000,
            queries: 100,
            seed: 42,
            dim: 384,
            limit: 10,
            rows: 20_000,
            lookups: 5_000,
            payload: 64,
            writers: 8,
            txns: 200,
            export: None,
            oltp_export: None,
            oltp_replay: None,
            oltp_db: None,
            suite: Suite::All,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_args();
    let target = std::path::Path::new("target");

    // Exporting is a mode, not a suite: it writes the fixtures the
    // containerised baselines read and runs nothing, so a comparison against
    // DuckDB/pgvector or MySQL/PostgreSQL asks every engine exactly the same
    // questions. All of `--export`, `--export-oltp` and `--oltp-replay` may
    // be given in one invocation, though `bench/compare.sh` only ever passes
    // one of `--export`+`--export-oltp` (on the host) or `--oltp-replay`
    // (inside the `inlaysql-oltp` container) at a time — so nothing returns
    // until every requested mode has run.
    let mut exported = false;
    if let Some(directory) = &config.export {
        export::run(&config, directory)?;
        exported = true;
    }
    if let Some(directory) = &config.oltp_export {
        oltp_export::run(&config, directory)?;
        exported = true;
    }
    if let Some(directory) = &config.oltp_replay {
        let db_path = config
            .oltp_db
            .clone()
            .unwrap_or_else(|| directory.join("bench-oltp-container.inlay"));
        oltp_export::replay(directory, &db_path)?;
        exported = true;
    }
    if exported {
        return Ok(());
    }

    if config.suite.includes(Suite::Sweep) {
        sweep::run(&config)?;
    }
    if config.suite.includes(Suite::Points) {
        points::run(&config, target)?;
    }
    if config.suite.includes(Suite::Indexed) {
        indexed::run(&config, target)?;
    }
    if config.suite.includes(Suite::Joins) {
        joins::run(&config, target)?;
    }
    if config.suite.includes(Suite::Vectors) {
        vectors::run(&config, target)?;
    }
    if config.suite == Suite::Quantization {
        vectors::run_quantization(&config, target)?;
    }
    if config.suite.includes(Suite::Concurrency) {
        concurrency::run(&config, target)?;
    }
    if !config.suite.includes(Suite::Retrieval) {
        return Ok(());
    }

    println!(
        "\n=== retrieval workload: docs={} queries={} dim={} limit={} seed={} ===",
        config.docs, config.queries, config.dim, config.limit, config.seed
    );

    let path = target.join("inlaysql-bench.inlay");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path)?;
    db.execute(
        &format!(
            "CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR({}))",
            config.dim
        ),
        &[],
    )?;
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])?;
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])?;

    let mut rng = SeededRng::new(config.seed);
    let corpus: Vec<String> = (0..config.docs)
        .map(|_| synthetic_document(&mut rng))
        .collect();

    let started = Instant::now();
    // Ingest inside explicit transactions: the engine batches until the log is
    // nearly full, commits, and starts a new transaction. One `fsync` per batch
    // instead of one per document.
    let ingest_result = batched(&mut db, config.docs, |db, index| {
        let body = &corpus[index];
        db.execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(index as i64),
                Value::Text(body.clone()),
                Value::Vector(hashed_embedding(body, config.dim)),
            ],
        )?;
        Ok(())
    });
    if let Err(error) = ingest_result {
        return Err(error.into());
    }
    let ingest = started.elapsed();
    println!(
        "\ningest: {} docs in {:.2?} ({:.0} docs/s)",
        config.docs,
        ingest,
        config.docs as f64 / ingest.as_secs_f64().max(f64::EPSILON)
    );

    // Queries are drawn from the same seeded stream, so a rerun with the same
    // seed asks exactly the same questions.
    let queries: Vec<String> = (0..config.queries)
        .map(|_| synthetic_query(&mut rng))
        .collect();

    let vector_only = format!(
        "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC LIMIT {}",
        config.limit
    );
    let text_only = format!(
        "SELECT id, bm25_score(body, ?) AS score FROM docs ORDER BY score DESC LIMIT {}",
        config.limit
    );
    let hybrid = format!(
        "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
         FROM docs ORDER BY score DESC LIMIT {}",
        config.limit
    );

    // Index commits are deferred to the first read, so that read pays for the
    // whole load. Measuring it separately keeps it out of the latency numbers
    // and makes the cost visible instead of hiding it in a p99.
    let warmup = Instant::now();
    db.query(&text_only, &[Value::Text("database".to_string())])?;
    println!("index build on first read: {:.2?}", warmup.elapsed());

    println!("\nquery latency ({} queries each)", config.queries);
    println!(
        "{:<12} {:>10} {:>10} {:>10}",
        "workload", "p50", "p95", "max"
    );

    let vector_times = measure(&queries, |query| {
        db.query(
            &vector_only,
            &[Value::Vector(hashed_embedding(query, config.dim))],
        )
        .map(|_| ())
    })?;
    report("vector", &vector_times);

    let text_times = measure(&queries, |query| {
        db.query(&text_only, &[Value::Text(query.to_string())])
            .map(|_| ())
    })?;
    report("bm25", &text_times);

    let hybrid_times = measure(&queries, |query| {
        db.query(
            &hybrid,
            &[
                Value::Vector(hashed_embedding(query, config.dim)),
                Value::Text(query.to_string()),
            ],
        )
        .map(|_| ())
    })?;
    report("hybrid", &hybrid_times);

    let _ = std::fs::remove_file(&path);
    Ok(())
}

impl Config {
    fn from_args() -> Self {
        let mut config = Config::default();
        let args: Vec<String> = std::env::args().skip(1).collect();
        for pair in args.chunks(2) {
            let [flag, value] = pair else {
                eprintln!("ignoring trailing argument {:?}", pair);
                continue;
            };
            if flag == "--suite" {
                config.suite = match value.as_str() {
                    "retrieval" => Suite::Retrieval,
                    "points" => Suite::Points,
                    "indexed" => Suite::Indexed,
                    "joins" => Suite::Joins,
                    "vectors" => Suite::Vectors,
                    "quantization" => Suite::Quantization,
                    "concurrency" => Suite::Concurrency,
                    "sweep" => Suite::Sweep,
                    "all" => Suite::All,
                    other => {
                        eprintln!("unknown suite `{other}`, running all");
                        Suite::All
                    }
                };
                continue;
            }
            if flag == "--export" {
                config.export = Some(std::path::PathBuf::from(value));
                continue;
            }
            if flag == "--export-oltp" {
                config.oltp_export = Some(std::path::PathBuf::from(value));
                continue;
            }
            if flag == "--oltp-replay" {
                config.oltp_replay = Some(std::path::PathBuf::from(value));
                continue;
            }
            if flag == "--oltp-db" {
                config.oltp_db = Some(std::path::PathBuf::from(value));
                continue;
            }
            let parsed = value.parse().unwrap_or_else(|_| {
                eprintln!("{flag}: `{value}` is not a number, using the default");
                0
            });
            if parsed == 0 {
                continue;
            }
            match flag.as_str() {
                "--docs" => config.docs = parsed as usize,
                "--queries" => config.queries = parsed as usize,
                "--seed" => config.seed = parsed,
                "--dim" => config.dim = parsed as usize,
                "--limit" => config.limit = parsed as usize,
                "--rows" => config.rows = parsed as usize,
                "--lookups" => config.lookups = parsed as usize,
                "--payload" => config.payload = parsed as usize,
                "--writers" => config.writers = parsed as usize,
                "--txns" => config.txns = parsed as usize,
                other => eprintln!("unknown flag {other}"),
            }
        }
        config
    }
}

fn synthetic_document(rng: &mut SeededRng) -> String {
    let length = 12 + (rng.next_u64() % 24) as usize;
    words(rng, length)
}

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

/// Run `insert` for `count` rows, batching them into explicit transactions so
/// the engine commits one `fsync` per batch rather than one per row.
///
/// The batch boundary is the engine's own limit: when a transaction is about
/// to overflow the write-ahead log the engine refuses the next statement with
/// [`inlaysql::Error::Transaction`] *before* running it, so committing there is
/// exactly "flush what is buffered" — the retry inserts the row, never doubles
/// it.
fn batched(
    db: &mut Database,
    count: usize,
    mut insert: impl FnMut(&mut Database, usize) -> Result<(), inlaysql::Error>,
) -> Result<(), inlaysql::Error> {
    db.begin()?;
    for index in 0..count {
        match insert(db, index) {
            Ok(()) => {}
            Err(inlaysql::Error::Transaction(_)) => {
                db.commit()?;
                db.begin()?;
                insert(db, index)?;
            }
            Err(other) => return Err(other),
        }
    }
    db.commit()
}

fn measure(
    queries: &[String],
    mut run: impl FnMut(&str) -> Result<(), inlaysql::Error>,
) -> Result<Vec<Duration>, inlaysql::Error> {
    let mut timings = Vec::with_capacity(queries.len());
    for query in queries {
        let started = Instant::now();
        run(query)?;
        timings.push(started.elapsed());
    }
    Ok(timings)
}

/// The p50, p95 and maximum of a set of timings.
pub fn percentiles(timings: &[Duration]) -> (Duration, Duration, Duration) {
    let mut sorted = timings.to_vec();
    sorted.sort();
    let percentile = |p: f64| {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[index]
    };
    (
        percentile(0.50),
        percentile(0.95),
        sorted.last().copied().unwrap_or_default(),
    )
}

fn report(label: &str, timings: &[Duration]) {
    let (p50, p95, max) = percentiles(timings);
    println!(
        "{:<12} {:>10} {:>10} {:>10}",
        label,
        format!("{p50:.2?}"),
        format!("{p95:.2?}"),
        format!("{max:.2?}")
    );
}
