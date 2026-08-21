//! How much the page cache is worth at the server-to-server benchmark's own
//! table size (2000 rows, AHL-495's `SERVER_ROWS` default), measured
//! in-process so the wire, the client and the socket are out of the picture.
//!
//! Two numbers come out of this test, on two runs:
//!
//! * The decoded-cache row (`cache_bytes=8388608`) against the
//!   `cache_bytes=0` row — the per-handle page cache's worth, guarded by the
//!   assertion below.
//! * The `cache_bytes=0` row run with and without
//!   `INLAYSQL_DISABLE_SHARED_READ_CACHE=1` — the shared raw-page cache's
//!   worth on the miss path it sits in front of, which is the only place it
//!   can show. Two separate process runs are required, not one, because the
//!   budget is fixed when the first handle on a file opens.

use inlaysql::{Database, EngineOptions};

fn point_read_rate(cache_bytes: usize, reads: u64) -> f64 {
    let dir = std::env::temp_dir().join(format!(
        "inlaysql-cache-role-{}-{}-{}.inlay",
        std::process::id(),
        cache_bytes,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let _ = std::fs::remove_file(&dir);
    let mut db = Database::open_on_with_options(
        inlaysql::FileDevice::open(&dir).expect("device"),
        EngineOptions {
            page_cache_bytes: cache_bytes,
            ..EngineOptions::default()
        },
    )
    .expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .expect("create");
    for start in (1..=2000).step_by(100) {
        let end = (start + 99).min(2000);
        let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, 'body-{id}')"));
        }
        db.execute(&sql, &[]).expect("insert");
    }
    let stmt = db
        .prepare("SELECT body FROM kv WHERE id = ?")
        .expect("prepare");
    for id in 1..=100 {
        db.execute_prepared(&stmt, &[inlaysql::Value::Integer(id)])
            .expect("warmup");
    }
    let start = std::time::Instant::now();
    for id in 0..reads {
        db.execute_prepared(&stmt, &[inlaysql::Value::Integer((id % 2000 + 1) as i64)])
            .expect("read");
    }
    let elapsed = start.elapsed();
    drop(db);
    std::fs::remove_file(&dir).ok();
    reads as f64 / elapsed.as_secs_f64()
}

#[test]
fn cache_on_vs_off_at_the_benchmark_table_size() {
    let uncached = point_read_rate(0, 100_000);
    let cached = point_read_rate(8 << 20, 100_000);
    println!(
        "point reads/s: cache_bytes=0: {uncached:.0}, cache_bytes=8MiB: {cached:.0} \
         (run with INLAYSQL_DISABLE_SHARED_READ_CACHE=1 to see the shared raw cache's \
         share of the cache_bytes=0 row)"
    );
    // The decoded cache is worth an order of magnitude here (measured ~279k
    // against ~2.8M reads/s in release). The guard is deliberately loose — a
    // factor of two — so a slow shared CI machine cannot flake it, while a
    // regression that quietly disabled the cache would fail it.
    assert!(
        cached > uncached * 1.5,
        "the decoded page cache should be worth at least 50% at this table size \
         (got {cached:.0} against {uncached:.0})"
    );
}
