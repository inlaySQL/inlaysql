//! InlaySQL's side of the read-shape and batch-insert scoreboard cells that
//! have no Rust suite: aggregate/GROUP BY and batch insert.
//!
//! The indexed range scan and the two-table join cells do NOT run through
//! this binary — their InlaySQL numbers come from the existing
//! `SUITE=indexed` / `SUITE=joins` suites (and this file deliberately
//! duplicates none of their shapes, so the two sides cannot drift). The
//! aggregate shapes are defined by `bench/external/read_driver.py`, and this
//! binary measures the same table and the same two statements; the batch
//! shape is defined by `bench/external/batch_driver.py` and measured here
//! with commits-per-fsync read from the coordinator counters, the same
//! keeper-handle pattern the instrumentation task uses.
//!
//! Env: MODE (`agg`|`batch`), REPS, QUERIES (agg), BATCH, STATEMENTS (batch),
//! DIR. Medians over reps, like the Python drivers.

use std::path::{Path, PathBuf};
use std::time::Instant;

use inlaysql::{Database, FileDevice, Value};

fn median(mut vals: Vec<f64>) -> f64 {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = vals.len();
    if n % 2 == 1 {
        vals[n / 2]
    } else {
        (vals[n / 2 - 1] + vals[n / 2]) / 2.0
    }
}

fn email(i: i64) -> String {
    format!("user{i:012}@example.com")
}

/// Load `indexed`'s table shape — plus the 100-bucket `n` column the
/// aggregate shapes read — exactly as `read_driver.py` builds it for the
/// opponents: explicit ids, 2000-row multi-row INSERT batches, index on
/// `email` built after the rows.
fn load_users(db: &mut Database, rows: usize) -> Result<(), Box<dyn std::error::Error>> {
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, body TEXT, n INTEGER)",
        &[],
    )?;
    let payload = "x".repeat(64);
    db.begin()?;
    for base in (1..=rows as i64).step_by(2000) {
        let top = (base + 1999).min(rows as i64);
        let count = (top - base + 1) as usize;
        let placeholders = vec!["(?, ?, ?, ?)"; count].join(",");
        let sql = format!("INSERT INTO users (id, email, body, n) VALUES {placeholders}");
        let mut params = Vec::with_capacity(count * 4);
        for id in base..=top {
            params.push(Value::Integer(id));
            params.push(Value::Text(email(id).into()));
            params.push(Value::Text(payload.clone().into()));
            params.push(Value::Integer(id % 100));
        }
        if let Err(inlaysql::Error::Transaction(_)) = db.execute(&sql, &params) {
            db.commit()?;
            db.begin()?;
            db.execute(&sql, &params)?;
        }
    }
    db.commit()?;
    db.execute("CREATE INDEX users_email ON users (email) USING BTREE", &[])?;
    db.execute("ANALYZE", &[])?;
    Ok(())
}

fn run_agg(dir: &Path, reps: usize, queries: usize) -> Result<(), Box<dyn std::error::Error>> {
    let path = dir.join("shapes-agg.inlay");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path)?;
    load_users(&mut db, 100_000)?;

    let mut results: Vec<(&str, Vec<f64>)> = Vec::new();
    for (name, sql, expected) in [
        (
            "agg_group",
            "SELECT n, COUNT(*) FROM users GROUP BY n",
            100usize,
        ),
        (
            "agg_scalar",
            "SELECT COUNT(*), MIN(id), MAX(id) FROM users",
            1usize,
        ),
        // AHL-546: `agg_scalar` above keeps `COUNT(*)` because that is the
        // exact statement `BENCHMARK.md` publishes against MySQL/PostgreSQL.
        // When this shape was added, `COUNT(*)` still forced a scan and the
        // `MIN`/`MAX` optimisation could not fire for the whole statement;
        // since AHL-548 `COUNT(*)` answers from the leaves' cell counts
        // (`Engine::try_scalar_aggregate`'s doc), and the two shapes differ
        // by exactly that leaf walk. Kept so the walk's own cost stays
        // visible next to the descents'.
        (
            "agg_minmax_only",
            "SELECT MIN(id), MAX(id) FROM users",
            1usize,
        ),
    ] {
        let stmt = db.prepare(sql)?;
        let mut throughputs = Vec::with_capacity(reps);
        for _rep in 0..reps {
            let started = Instant::now();
            for _ in 0..queries {
                let rows = db.query_prepared(&stmt, &[])?;
                if rows.rows.len() != expected {
                    return Err(format!(
                        "{name}: expected {expected} rows, got {} — refusing to time a wrong answer",
                        rows.rows.len()
                    )
                    .into());
                }
            }
            throughputs.push(queries as f64 / started.elapsed().as_secs_f64());
        }
        println!("{name} per-rep: {throughputs:.0?}");
        results.push((name, throughputs));
    }

    println!("=== InlaySQL aggregate (100000 rows, {queries} queries/rep, {reps} reps) ===");
    for (name, throughputs) in &results {
        println!(
            "{name:12} {:.0}/s  ({:.0}–{:.0})",
            median(throughputs.clone()),
            throughputs.iter().cloned().fold(f64::INFINITY, f64::min),
            throughputs
                .iter()
                .cloned()
                .fold(f64::NEG_INFINITY, f64::max),
        );
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn run_batch(
    dir: &Path,
    reps: usize,
    batch: usize,
    statements: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = dir.join("shapes-batch.inlay");
    let _ = std::fs::remove_file(&path);
    let keeper = FileDevice::open(&path)?;
    let mut db = Database::open_on_with_options(
        FileDevice::open(&path)?,
        inlaysql::EngineOptions::default(),
    )?;
    db.execute(
        "CREATE TABLE batch (id INTEGER PRIMARY KEY, n INTEGER)",
        &[],
    )?;
    let baseline = keeper.commit_stats().expect("read-write handle");

    let placeholders = vec!["(?, ?)"; batch].join(",");
    let sql = format!("INSERT INTO batch (id, n) VALUES {placeholders}");
    let stmt = db.prepare(&sql)?;

    let mut rows_rates = Vec::with_capacity(reps);
    let mut stmt_rates = Vec::with_capacity(reps);
    let mut cfsyncs = Vec::with_capacity(reps);
    for rep in 0..reps {
        let base = (rep * batch * statements + 1) as i64;
        let started = Instant::now();
        for s in 0..statements {
            let first = base + (s * batch) as i64;
            let mut params = Vec::with_capacity(batch * 2);
            for r in first..first + batch as i64 {
                params.push(Value::Integer(r));
                params.push(Value::Integer(r % 1000));
            }
            db.execute_prepared(&stmt, &params)?;
        }
        let elapsed = started.elapsed();
        let stats = keeper.commit_stats().expect("read-write handle");
        let flushes = stats.normal_flushes - baseline.normal_flushes;
        let tickets = stats.normal_tickets_flushed - baseline.normal_tickets_flushed;
        let rows = batch * statements;
        rows_rates.push(rows as f64 / elapsed.as_secs_f64());
        stmt_rates.push(statements as f64 / elapsed.as_secs_f64());
        cfsyncs.push(tickets as f64 / flushes.max(1) as f64);
        println!(
            "rep {rep}: {:.0} rows/s  {:.0} commits/s  c/fsync {tickets}/{flushes} = {:.2}",
            rows_rates[rep], stmt_rates[rep], cfsyncs[rep],
        );
    }

    let mut verify = Database::open(&path)?;
    let rows = verify.query("SELECT COUNT(*) FROM batch", &[])?.rows;
    let count = match rows.first().and_then(|r| r.first()) {
        Some(inlaysql::Value::Integer(n)) => *n as usize,
        other => return Err(format!("COUNT(*) returned {other:?}").into()),
    };
    let expected = reps * batch * statements;
    if count != expected {
        return Err(format!("lost writes: expected {expected}, table holds {count}").into());
    }
    println!(
        "\nMEDIANS InlaySQL: rows/s {:.0}  commits/s {:.0}  c/fsync {:.2}",
        median(rows_rates.clone()),
        median(stmt_rates.clone()),
        median(cfsyncs.clone()),
    );
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::var("MODE").unwrap_or_else(|_| "agg".to_string());
    let reps: usize = std::env::var("REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let queries: usize = std::env::var("QUERIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let batch: usize = std::env::var("BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let statements: usize = std::env::var("STATEMENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let dir: PathBuf = std::env::var("DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("inlaysql-sql-shapes"));
    std::fs::create_dir_all(&dir)?;

    match mode.as_str() {
        "agg" => run_agg(&dir, reps, queries),
        "batch" => run_batch(&dir, reps, batch, statements),
        other => Err(format!("unknown MODE {other:?} (agg|batch)").into()),
    }
}
