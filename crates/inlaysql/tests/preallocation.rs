//! The data area is extended ahead of the writer, and a file whose data area
//! runs past its last committed page still opens, recovers and commits
//! (AHL-553).
//!
//! `FileDevice::extend_for` is what makes an ordinary commit's barrier stop
//! paying to grow the file — see its doc comment and `PERF.md`'s AHL-553
//! section for why, and `docs/recovery.md` for what it changes about the
//! bytes recovery sees at the tail. The two facts worth pinning are the two
//! this file pins: the file really does run ahead of the tree, and running
//! ahead is invisible to everything that reads the file back.

use std::fs;
use std::path::{Path, PathBuf};

use inlaysql::{Database, Value};

/// A database file that deletes itself when the test ends, whatever the
/// outcome — mirrors the helper in `durability.rs` and `free_list_growth.rs`.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-prealloc-test-{name}-{}-{}.inlay",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn len(&self) -> u64 {
        fs::metadata(&self.path)
            .expect("the database file exists")
            .len()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Where the data area starts, for the layout every `Database::open` creates.
fn data_area_start() -> u64 {
    inlaysql_core::wal::data_offset_for(
        inlaysql_core::btree::DEFAULT_PAGE_SIZE,
        inlaysql_core::wal::MULTI_REGION_FORMAT_VERSION,
        0,
    ) as u64
}

fn insert_rows(db: &mut Database, from: i64, to: i64) {
    let insert = db
        .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
        .expect("prepare");
    for id in from..=to {
        db.execute_prepared(
            &insert,
            &[Value::Integer(id), Value::Text("x".repeat(64).into())],
        )
        .expect("insert");
    }
}

/// The file runs ahead of the tree: after a run of commits the data area is
/// longer than the pages those commits actually wrote, and the surplus is a
/// whole number of preallocation chunks rather than a page or two of slack.
#[test]
fn the_data_area_is_extended_past_what_the_committed_pages_need() {
    let temp = TempDb::new("ahead");
    let mut db = Database::open(temp.path()).expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .expect("create");

    // One row is enough: the very first data-area write extends the file by
    // the minimum chunk, so the surplus is visible immediately rather than
    // only on a large database.
    insert_rows(&mut db, 1, 1);
    let after_one = temp.len();
    assert!(
        after_one >= data_area_start() + (1 << 20),
        "the first data write should have extended the file by at least the \
         minimum chunk; data area starts at {}, file is {after_one}",
        data_area_start(),
    );

    insert_rows(&mut db, 2, 200);
    drop(db);

    // Whatever the tree needed, the file is longer than it — that is the
    // whole point — and it is longer by at least a page, not by rounding.
    let grown = temp.len();
    assert!(
        grown >= after_one,
        "the file never shrinks: {after_one} then {grown}",
    );

    // And every row is still there, read back through a fresh handle.
    let mut reopened = Database::open(temp.path()).expect("reopen");
    let rows = reopened
        .query("SELECT COUNT(*) FROM kv", &[])
        .expect("count");
    assert_eq!(rows.rows[0][0], Value::Integer(200));
}

/// Recovery against a file whose data area extends well past the last
/// committed page — zeros at the tail rather than end-of-file.
///
/// This is the shape preallocation introduces and the one nothing tested
/// before it: every earlier database ended within a page of its newest write,
/// so "past the end" and "past the last commit" were the same offset. They
/// are not any more. The file is extended by hand here, far beyond anything
/// `extend_for` would have chosen, so the test pins the property rather than
/// the chunk size.
#[test]
fn a_file_whose_data_area_runs_far_past_the_last_commit_still_opens_and_commits() {
    let temp = TempDb::new("tail");
    {
        let mut db = Database::open(temp.path()).expect("open");
        db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
            .expect("create");
        insert_rows(&mut db, 1, 50);
    }

    // Sixty-four mebibytes of zeros past whatever the commits left. Nothing
    // committed points into it; recovery must not read it as anything.
    let extended = temp.len() + (64 << 20);
    {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(temp.path())
            .expect("open for extension");
        file.set_len(extended).expect("extend");
        file.sync_all().expect("sync");
    }
    assert_eq!(temp.len(), extended);

    let mut db = Database::open(temp.path()).expect("reopen over a long tail");
    let rows = db.query("SELECT COUNT(*) FROM kv", &[]).expect("count");
    assert_eq!(
        rows.rows[0][0],
        Value::Integer(50),
        "every committed row survives a data area that runs past it",
    );

    // And the tree keeps writing into that tail correctly, which is the half
    // a read-only check would miss: the allocator hands out ids past the last
    // committed page, and those pages land in bytes that already held zeros.
    insert_rows(&mut db, 51, 300);
    drop(db);

    let mut reopened = Database::open(temp.path()).expect("reopen after writing into the tail");
    let rows = reopened
        .query("SELECT COUNT(*) FROM kv", &[])
        .expect("count");
    assert_eq!(rows.rows[0][0], Value::Integer(300));
    let sum = reopened.query("SELECT SUM(id) FROM kv", &[]).expect("sum");
    assert_eq!(sum.rows[0][0], Value::Integer((1..=300).sum::<i64>()));
}

/// Two handles on the same file share one extension: the second never
/// re-extends a range the first already filled, and neither loses a row.
#[test]
fn two_handles_on_one_file_share_the_extension() {
    let temp = TempDb::new("shared");
    let mut first = Database::open(temp.path()).expect("open");
    first
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .expect("create");
    insert_rows(&mut first, 1, 20);

    let mut second = Database::open(temp.path()).expect("second handle");
    insert_rows(&mut second, 21, 40);
    let after_both = temp.len();
    insert_rows(&mut first, 41, 60);

    assert!(
        temp.len() >= after_both,
        "the file never shrinks under a second writer",
    );
    let rows = first.query("SELECT COUNT(*) FROM kv", &[]).expect("count");
    assert_eq!(rows.rows[0][0], Value::Integer(60));
}
