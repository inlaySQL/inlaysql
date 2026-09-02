//! `AHL-539`'s quick throughput number, not a bench suite.
//!
//! `crates/inlaysql-bench/src/bin/profile.rs`'s suites are all file-backed —
//! `Database::open`, never `Database::open_in_memory` — so they cannot show
//! what `MemStorage` sharing committed row bytes as `Arc<[u8]>` instead of
//! copying them into a fresh `Vec<u8>` is worth. This is a single `#[ignore]`d
//! test, run by hand (`cargo test --release -p inlaysql --test
//! in_memory_point_read_throughput -- --ignored --nocapture`) and compared
//! interleaved against the same binary built from the pre-`AHL-539` tree, the
//! same way `bin/profile`'s suites are compared in `PERF.md`. It is not part
//! of the default `cargo test` run because a wall-clock number measured once
//! in CI is exactly the kind of number `AGENTS.md` warns is not reproducible
//! evidence — the comparison that matters is the interleaved one, done by
//! hand, and reported in `PERF.md`.

use std::time::Instant;

use inlaysql::{Database, Value};

#[test]
#[ignore = "wall-clock throughput number, run by hand interleaved against a baseline binary"]
fn two_hundred_k_point_reads_on_a_twenty_k_row_in_memory_table() {
    let rows = 20_000i64;
    let reads = 200_000i64;

    let mut db = Database::open_in_memory().expect("open in-memory");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .expect("create");
    let insert = db
        .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
        .expect("prepare insert");
    let payload = "x".repeat(64);
    db.begin().expect("begin");
    for id in 1..=rows {
        db.execute_prepared(
            &insert,
            &[Value::Integer(id), Value::Text(payload.clone().into())],
        )
        .expect("insert");
    }
    db.commit().expect("commit");

    let point = db
        .prepare("SELECT body FROM kv WHERE id = ?")
        .expect("prepare point");

    // Warm: fill whatever scratch state the first execution of each id sets
    // up, so the timed loop measures steady state.
    let mut sink = 0usize;
    for id in 1..=rows.min(2_000) {
        db.query_prepared_each_ref(&point, &[Value::Integer(id)], |row| {
            sink += row[0].as_str().map_or(0, str::len);
            Ok(())
        })
        .expect("warm");
    }

    let start = Instant::now();
    for i in 0..reads {
        let id = 1 + (i % rows);
        db.query_prepared_each_ref(&point, &[Value::Integer(id)], |row| {
            sink += row[0].as_str().map_or(0, str::len);
            Ok(())
        })
        .expect("point read");
    }
    let elapsed = start.elapsed();
    assert!(sink > 0);
    println!(
        "in_memory_point_reads: {reads} iterations in {elapsed:.2?} ({:.0} ops/s)",
        reads as f64 / elapsed.as_secs_f64().max(f64::EPSILON)
    );
}
