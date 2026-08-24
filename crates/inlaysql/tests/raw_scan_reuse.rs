//! The raw leaf scan, reading pages whose ids have been handed out again.
//!
//! A table scan parses leaf pages in place rather than decoding them into
//! cached nodes (AHL-455/AHL-466), and it now reads *through* the page cache.
//! A cache keyed by page id is only sound while a page id means one immutable
//! sequence of bytes — which is exactly what the free list stops guaranteeing
//! when it hands an id out again (`EngineOptions::page_reuse`, AHL-481).
//! AHL-406 is what that class of mistake looks like when it escapes: a
//! database in a state no commit ever wrote, with no checksum failing.
//!
//! An independent review found that no existing sweep reaches this
//! combination, and the claim that one did was wrong:
//!
//! * `dst_sweep` and `index_recovery_dst` run on `Simulator`, which answers
//!   `None` to `Device::commit_point` and `Device::min_reader_seq` —
//!   deliberately "unknown, so never reclaim". Neither ever recycles a page
//!   id, whatever `page_reuse` is set to.
//! * `free_list_reuse_dst` does recycle ids, on its own `TrustedDevice`, but
//!   verifies through `CowBTree::scan` — the *decoded* walk.
//!   `walk_raw_row_values` is never executed by it.
//!
//! So this file drives the raw scan the way production does — a SQL table scan
//! goes `RowScan` → `Storage::scan_batch` → `scan_prefix_row_values_raw_from`
//! → `walk_raw_row_values` — over a real device where reclamation actually
//! fires, and checks the scan returns exactly the rows that are there after
//! page ids have been recycled many times over.
//!
//! **What this does not cover, stated rather than implied.** There are no
//! injected faults here. Combining reuse with crash and torn writes at this
//! level needs a fault-injecting device that also answers the durability
//! questions honestly — `free_list_reuse_dst`'s `TrustedDevice` is that device
//! but lives at the `CowBTree` layer, and `scan_prefix_row_values_raw_from` is
//! `pub(crate)`, so neither half can reach the other today. That sweep is
//! still owed; this closes the larger half of the risk, which is whether the
//! scan reads recycled pages correctly at all.

use std::fs;
use std::path::{Path, PathBuf};

use inlaysql::{Database, EngineOptions, FileDevice, Value};

/// Rows the churn workload cycles through.
const ROWS: i64 = 60;
/// Churn rounds. Enough that reclamation fires many times, so the scan is
/// reading pages that have been several different pages before.
const ROUNDS: usize = 40;
/// A payload large enough that a row is a real fraction of a page, so deleting
/// rows empties leaves instead of merely loosening them.
const PAYLOAD: usize = 300;

/// A database file that deletes itself when the test ends, whatever the
/// outcome. Same pattern as `free_list_growth.rs`'s, since this crate has no
/// `tempfile` dependency and does not need one for a single-file test.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-raw-scan-reuse-{name}-{}.inlay",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn body(id: i64, round: usize) -> String {
    format!("row {id} round {round} {}", "x".repeat(PAYLOAD))
}

/// Every row the table holds, read back through the raw scan.
///
/// `SELECT id, body FROM churn` with no `WHERE` is a full table scan, which is
/// the path this file exists to exercise.
fn table_scan(db: &mut Database) -> Vec<(i64, String)> {
    let rows = db
        .query("SELECT id, body FROM churn ORDER BY id", &[])
        .expect("table scan");
    rows.rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Integer(id), Value::Text(body)) => (*id, body.to_string()),
            other => panic!("unexpected row shape: {other:?}"),
        })
        .collect()
}

/// Run the churn workload, asserting on every round that a full table scan
/// returns exactly the rows the table holds, and return the file's final size.
///
/// The size is what proves reclamation fired: `CowBTree::pages_reused` is a
/// per-handle counter that a reopened tree reports as zero, and `Database`
/// does not expose the tree it owns, so the observable difference is the file
/// this workload leaves behind.
fn churn(reuse: bool, name: &str) -> u64 {
    let temp = TempDb::new(name);
    let device = FileDevice::open(temp.path()).expect("open");
    let mut db = Database::open_on_with_options(
        device,
        EngineOptions {
            page_reuse: reuse,
            ..EngineOptions::default()
        },
    )
    .expect("open database");

    db.execute(
        "CREATE TABLE churn (id INTEGER PRIMARY KEY, body TEXT)",
        &[],
    )
    .expect("create table");

    for round in 0..ROUNDS {
        // Rewrite every row, so the previous round's leaves are superseded.
        for id in 1..=ROWS {
            db.execute(
                "INSERT OR REPLACE INTO churn (id, body) VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(body(id, round).into())],
            )
            .expect("insert");
        }

        // Drop half of them, so whole leaves empty and reach the free list.
        // Alternating which half by round keeps the freed set moving.
        let keep_even = round % 2 == 0;
        for id in 1..=ROWS {
            if (id % 2 == 0) == keep_even {
                continue;
            }
            db.execute("DELETE FROM churn WHERE id = ?", &[Value::Integer(id)])
                .expect("delete");
        }

        // Reclamation needs the freeing commit to be durable and covered by a
        // checkpoint before it will draw an id back out.
        db.checkpoint().expect("checkpoint");

        // The assertion, every round: the raw scan sees exactly what is there.
        // A page id served from the cache after being handed to a different
        // page would show up here as a row that should not exist, a row that
        // should and does not, or a body from an earlier round.
        let expected: Vec<(i64, String)> = (1..=ROWS)
            .filter(|id| (id % 2 == 0) == keep_even)
            .map(|id| (id, body(id, round)))
            .collect();
        assert_eq!(
            table_scan(&mut db),
            expected,
            "round {round}: the raw scan disagreed with what the table holds"
        );

        // Re-insert the deleted half so the next round starts from a full
        // table and the freed pages get drawn back out.
        for id in 1..=ROWS {
            if (id % 2 == 0) == keep_even {
                continue;
            }
            db.execute(
                "INSERT INTO churn (id, body) VALUES (?, ?)",
                &[Value::Integer(id), Value::Text(body(id, round).into())],
            )
            .expect("reinsert");
        }
        db.checkpoint().expect("checkpoint");
    }

    drop(db);
    fs::metadata(temp.path()).expect("stat").len()
}

#[test]
fn the_raw_scan_reads_recycled_pages_correctly() {
    let with_reuse = churn(true, "on");
    let without_reuse = churn(false, "off");

    // Non-vacuity. Without this the assertions inside `churn` would pass just
    // as happily on a file where no page id was ever handed out twice, and
    // this test would be an ordinary scan test wearing this file's name.
    assert!(
        with_reuse < without_reuse,
        "reclamation never fired: {ROUNDS} rounds left {with_reuse} bytes with \
         page reuse on and {without_reuse} with it off, so the scan above never \
         read a recycled page and proves nothing about the hazard it exists for"
    );
}
