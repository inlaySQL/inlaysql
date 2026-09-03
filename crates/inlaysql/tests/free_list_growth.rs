//! Proves the whole point of Phase 2 item 6 (AHL-481): with page reuse on, a
//! sustained write/delete/write/checkpoint churn workload stops growing the
//! file, where the same workload with reuse off (still the default) grows it
//! without bound.
//!
//! The first two tests run directly on `CowBTree<FileDevice>`, the same layer
//! `crates/inlaysql/tests/index_recovery_dst.rs` already uses to reach the
//! storage engine below the SQL surface — that predates `page_reuse` reaching
//! `EngineOptions`/`Database`, and stays because it isolates the mechanism
//! from everything the SQL surface adds on top. The third proves the same
//! thing through `Database::open_on_with_options`, which is what actually
//! matters now that the option is public: a caller reaching this through
//! ordinary `CREATE TABLE`/`INSERT`/`DELETE` gets the same win, not just
//! `CowBTree` callers who already knew this existed.
//!
//! The fourth is about the price rather than the win:
//! `the_size_question_covers_the_free_list_rows_committing_will_add` pins the
//! one thing turning reuse on changes for every batched writer in the engine —
//! committing now writes free-list rows of its own, so
//! `Storage::transaction_is_nearly_full` has to count them or the answer
//! arrives after the transaction is already too large to commit.

use std::fs;
use std::path::{Path, PathBuf};

use inlaysql::{Database, EngineOptions, FileDevice, Value};
use inlaysql_core::btree::{CowBTree, Durability, DEFAULT_PAGE_CACHE_BYTES, DEFAULT_PAGE_SIZE};
use inlaysql_core::{RowId, Storage, TreeStorage};

/// A database file that deletes itself when the test ends, whatever the
/// outcome — including a panic. Same pattern as `concurrent_writers.rs`'s
/// `TempDb`, since this crate has no `tempfile` dependency and does not need
/// one for a single-file, single-process test.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-free-list-growth-{name}-{}.inlay",
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

/// One churn round: overwrite every key in `keys` with a fresh, page-filling
/// value, commit, then delete and reinsert a rotating subset so old pages are
/// genuinely superseded rather than merely grown into — checkpointing
/// periodically so reclaim (which requires a durable, checkpoint-covered
/// free-list row) has something to draw from.
fn churn_round(
    tree: &mut CowBTree<FileDevice>,
    round: usize,
    keys: usize,
    value_len: usize,
) -> inlaysql_core::Result<()> {
    for i in 0..keys {
        let key = format!("k{i:06}").into_bytes();
        let value: Vec<u8> = (0..value_len)
            .map(|b| ((round * 31 + i * 7 + b) % 251) as u8)
            .collect();
        tree.put(&key, &value)?;
    }
    tree.commit()?;
    // Delete and reinsert a rotating quarter of the keys, so their old pages
    // are superseded (freed) and, once reclaim-eligible, drawn again by the
    // very next round's `put` above.
    let start = (round * keys / 4) % keys;
    for offset in 0..keys / 4 {
        let i = (start + offset) % keys;
        let key = format!("k{i:06}").into_bytes();
        tree.delete(&key)?;
    }
    tree.commit()?;
    if round.is_multiple_of(4) {
        tree.checkpoint()?;
    }
    Ok(())
}

/// Run `rounds` of churn on a fresh file at `path`, with `reuse` deciding
/// whether the tree is allowed to draw on the free list, and return the
/// file's byte size at the end.
fn run(path: &Path, reuse: bool, rounds: usize, keys: usize, value_len: usize) -> u64 {
    let device = FileDevice::open(path).expect("open");
    let mut tree = CowBTree::open_or_create(device, DEFAULT_PAGE_SIZE).expect("open_or_create");
    tree.set_page_reuse(reuse);

    for round in 0..rounds {
        churn_round(&mut tree, round, keys, value_len).expect("churn round");
    }
    tree.checkpoint().expect("final checkpoint");
    drop(tree);

    fs::metadata(path).expect("stat").len()
}

#[test]
fn heavy_churn_stops_growing_the_file_once_reuse_is_on() {
    let reuse_off = TempDb::new("reuse-off");
    let reuse_on = TempDb::new("reuse-on");

    // Deliberately small values and a deliberately narrow key space: what
    // matters is that the *same* pages keep getting superseded and freed
    // round after round, which is exactly the shape of workload G7 names
    // ("write, delete, write, checkpoint, repeat") and exactly the shape a
    // monotonic allocator handles by growing forever.
    const ROUNDS: usize = 60;
    const KEYS: usize = 40;
    const VALUE_LEN: usize = 300; // bigger than DEFAULT_PAGE_SIZE / 8, forces real churn

    let off_size = run(reuse_off.path(), false, ROUNDS, KEYS, VALUE_LEN);
    let on_size = run(reuse_on.path(), true, ROUNDS, KEYS, VALUE_LEN);

    assert!(
        off_size > 0 && on_size > 0,
        "both files should hold real data (off={off_size}, on={on_size})"
    );
    // The whole point: reclaiming superseded pages should leave the file
    // substantially smaller than never reclaiming anything at all, for the
    // identical workload. A generous bound (well under 1.0) rather than a
    // tight one — this is a churn-shape proof, not a byte-exact budget.
    assert!(
        on_size < off_size * 3 / 4,
        "page reuse did not bound file growth: reuse off = {off_size} bytes, \
         reuse on = {on_size} bytes (expected reuse-on to be well below \
         3/4 of reuse-off)"
    );
}

/// A second, more direct proof at the `CowBTree` level: reopen the
/// reuse-enabled file and reread `CowBTree::next_page_id`-derived growth via
/// `pages_reused()` — confirming the smaller file size above is actually
/// explained by reclamation firing, not by some unrelated difference between
/// the two runs.
#[test]
fn the_smaller_file_is_explained_by_reclamation_actually_firing() {
    let db = TempDb::new("reuse-on-direct");

    let device = FileDevice::open(db.path()).expect("open");
    let mut tree = CowBTree::open_or_create(device, DEFAULT_PAGE_SIZE).expect("open_or_create");
    tree.set_page_reuse(true);

    const ROUNDS: usize = 60;
    const KEYS: usize = 40;
    const VALUE_LEN: usize = 300;
    for round in 0..ROUNDS {
        churn_round(&mut tree, round, KEYS, VALUE_LEN).expect("churn round");
    }
    tree.checkpoint().expect("final checkpoint");

    assert!(
        tree.pages_reused() > 0,
        "reclamation never fired across {ROUNDS} rounds of churn — the file-size \
         win in `heavy_churn_stops_growing_the_file_once_reuse_is_on` would be \
         unexplained if this were ever zero"
    );
}

/// The same churn shape as `heavy_churn_stops_growing_the_file_once_reuse_is_on`,
/// but reached the way an application actually would: `EngineOptions::page_reuse`
/// through `Database::open_on_with_options`, and ordinary `INSERT ... ON
/// CONFLICT DO UPDATE`/`DELETE` rather than `CowBTree::put`/`delete` directly.
/// This is the one that would have caught the gap this file's own doc comment
/// used to name: the mechanism working at the tree level proves nothing about
/// whether a real caller can reach it.
fn run_sql(path: &Path, reuse: bool, rounds: usize, keys: usize, value_len: usize) -> u64 {
    let device = FileDevice::open(path).expect("open");
    let mut db = Database::open_on_with_options(
        device,
        EngineOptions {
            page_reuse: reuse,
            ..EngineOptions::default()
        },
    )
    .expect("open");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v BLOB)", &[])
        .expect("create table");

    for round in 0..rounds {
        for i in 0..keys {
            let value: Vec<u8> = (0..value_len)
                .map(|b| ((round * 31 + i * 7 + b) % 251) as u8)
                .collect();
            // A key this round's delete pass (below) just removed needs
            // INSERT; every other key needs UPDATE — which of the two any
            // given key needs changes round to round, so upsert is the one
            // statement that is always right, matching `put`'s "set this
            // key's value, whatever it was before" at the tree level.
            db.execute(
                "INSERT INTO t (id, v) VALUES (?, ?) \
                 ON CONFLICT (id) DO UPDATE SET v = excluded.v",
                &[Value::Integer(i as i64), Value::Blob(value)],
            )
            .expect("upsert");
        }
        let start = (round * keys / 4) % keys;
        for offset in 0..keys / 4 {
            let i = (start + offset) % keys;
            db.execute("DELETE FROM t WHERE id = ?", &[Value::Integer(i as i64)])
                .expect("delete");
        }
        if round.is_multiple_of(4) {
            db.checkpoint().expect("checkpoint");
        }
    }
    db.checkpoint().expect("final checkpoint");
    drop(db);

    fs::metadata(path).expect("stat").len()
}

#[test]
fn the_public_api_gets_the_same_win_through_ordinary_sql() {
    let reuse_off = TempDb::new("sql-reuse-off");
    let reuse_on = TempDb::new("sql-reuse-on");

    const ROUNDS: usize = 60;
    const KEYS: usize = 40;
    const VALUE_LEN: usize = 300;

    let off_size = run_sql(reuse_off.path(), false, ROUNDS, KEYS, VALUE_LEN);
    let on_size = run_sql(reuse_on.path(), true, ROUNDS, KEYS, VALUE_LEN);

    assert!(
        off_size > 0 && on_size > 0,
        "both files should hold real data (off={off_size}, on={on_size})"
    );
    assert!(
        on_size < off_size * 3 / 4,
        "EngineOptions::page_reuse did not bound file growth reached through \
         ordinary SQL: reuse off = {off_size} bytes, reuse on = {on_size} \
         bytes (expected reuse-on well below 3/4 of reuse-off)"
    );
}

#[test]
#[ignore = "diagnostic only, prints the numbers behind the gate assertions"]
fn print_sizes_for_report() {
    let reuse_off = TempDb::new("report-off");
    let reuse_on = TempDb::new("report-on");
    const ROUNDS: usize = 60;
    const KEYS: usize = 40;
    const VALUE_LEN: usize = 300;
    let off_size = run(reuse_off.path(), false, ROUNDS, KEYS, VALUE_LEN);
    let on_size = run(reuse_on.path(), true, ROUNDS, KEYS, VALUE_LEN);
    println!(
        "REPORT reuse_off_bytes={off_size} reuse_on_bytes={on_size} ratio={:.3}",
        on_size as f64 / off_size as f64
    );
}

#[test]
#[ignore = "diagnostic only, prints size at each checkpoint to show it plateaus"]
fn print_size_over_time_for_report() {
    let reuse_off = TempDb::new("report-time-off");
    let reuse_on = TempDb::new("report-time-on");
    const KEYS: usize = 40;
    const VALUE_LEN: usize = 300;
    const CHUNK: usize = 20;
    const CHUNKS: usize = 10;

    let off_device = FileDevice::open(reuse_off.path()).expect("open");
    let mut off_tree =
        CowBTree::open_or_create(off_device, DEFAULT_PAGE_SIZE).expect("open_or_create");
    let on_device = FileDevice::open(reuse_on.path()).expect("open");
    let mut on_tree =
        CowBTree::open_or_create(on_device, DEFAULT_PAGE_SIZE).expect("open_or_create");
    on_tree.set_page_reuse(true);

    for chunk in 0..CHUNKS {
        for round in 0..CHUNK {
            let r = chunk * CHUNK + round;
            churn_round(&mut off_tree, r, KEYS, VALUE_LEN).expect("churn");
            churn_round(&mut on_tree, r, KEYS, VALUE_LEN).expect("churn");
        }
        off_tree.checkpoint().expect("checkpoint");
        on_tree.checkpoint().expect("checkpoint");
        let off_size = fs::metadata(reuse_off.path()).expect("stat").len();
        let on_size = fs::metadata(reuse_on.path()).expect("stat").len();
        println!(
            "REPORT round={} off_bytes={off_size} on_bytes={on_size} on_pages_reused={}",
            (chunk + 1) * CHUNK,
            on_tree.pages_reused()
        );
    }
}

/// `Storage::transaction_is_nearly_full` has to warn *before* the transaction
/// becomes uncommittable, and with `page_reuse` on it did not.
///
/// The question was answered from the dirty set as it stood, and committing
/// with reuse on then does more work: `CowBTree::finalize_free_list` turns
/// every page the transaction superseded into a durable free-list row, and
/// deletes the row of every page it drew from the free list, *before* the
/// commit record is built. That work lands after the last moment anybody could
/// have asked, so a writer that batched exactly the way the contract tells it
/// to was handed a record too large for the log region and a transaction it
/// could never commit — the failure
/// `crates/inlaysql/tests/large_statements.rs`'s
/// `a_commit_refused_for_size_leaves_a_usable_handle` exists to keep survivable
/// and this one exists to keep from happening at all.
///
/// The shape here is the worst case rather than a typical one, and it is a
/// realistic statement: deleting rows whose values live in overflow chains.
/// `CowBTree::free_overflow_chain` supersedes every page of the chain, so the
/// dirty set barely moves — the row itself is a few bytes of leaf — while the
/// free-list work owed grows by a chain's worth of pages per row. Measured
/// against the code without the fix: 2 dirty pages (8,272 bytes) when the
/// backend was last asked, 187,903 pages by the time the record was built, and
/// a refusal at 771,905,580 bytes against a 1,048,576-byte region.
#[test]
fn the_size_question_covers_the_free_list_rows_committing_will_add() {
    let db = TempDb::new("nearly-full-with-reuse");
    let device = FileDevice::open(db.path()).expect("open");
    let mut storage = TreeStorage::open_on_with_options(
        device,
        DEFAULT_PAGE_CACHE_BYTES,
        true,
        Durability::Full,
        false,
    )
    .expect("open_on_with_options");

    // Wide enough that every value needs an overflow chain of its own.
    const ROWS: RowId = 200;
    const WIDTH: usize = 64 * 1024;
    for id in 1..=ROWS {
        storage
            .put_row("t", id, &vec![(id % 251) as u8; WIDTH])
            .expect("put");
        storage.commit().expect("commit a loading row");
    }

    // The protocol every batched writer in this engine follows: write, ask,
    // commit when told. Every one of those commits has to succeed, or the
    // answer arrived too late to be worth having.
    let mut commits = 0;
    for id in 1..=ROWS {
        storage.delete_row("t", id).expect("delete");
        if storage.transaction_is_nearly_full() {
            storage.commit().unwrap_or_else(|error| {
                panic!(
                    "commit {commits} was refused after the backend said the transaction \
                     was nearly full at row {id}: {error}"
                )
            });
            commits += 1;
        }
    }
    storage.commit().expect("final commit");

    assert!(
        commits > 0,
        "the size question never fired, so this proved nothing about when it fires"
    );
    assert!(
        inlaysql_core::traits::scan_all(&storage, "t")
            .expect("scan")
            .is_empty(),
        "every row was deleted, so the table has to be empty"
    );
}
