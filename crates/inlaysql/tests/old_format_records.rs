//! A database written before AHL-564 keeps working, unchanged, forever.
//!
//! Format version 6 changed how a commit record encodes a page: a v6 entry
//! stores the bytes on either side of the page's longest run of zeros and
//! names the run, where a v5 entry copies the whole image
//! (`inlaysql_core::wal::HOLE_ELIDED_FORMAT_VERSION`). The file's header names
//! its version and every codec path dispatches on it, so a v5 database is read
//! *and written* as a v5 database by a v6 build — it does not migrate, it is
//! not rewritten, and nothing about it is upgraded in place.
//!
//! That claim is worth a test rather than a sentence because the failure it
//! guards against is silent: a v6 build that appended a v6 record into a v5
//! file would produce a record the file's own scan rejects — a commit that
//! returned success and then vanished. So this drives a real file through
//! enough commits to **wrap a WAL region and checkpoint**, which is the path
//! that appends records, re-scans them and replays them, and requires every
//! acknowledged row afterwards from a handle with no memory of any of it.
//!
//! This is a single-test binary on purpose. The arm is selected by
//! `INLAYSQL_WHOLE_PAGE_WAL_RECORD`, which `FileDevice::create_format_version`
//! reads once per process, so a second test in this file could not choose the
//! other arm and would only make this one's setup racy.

use std::path::PathBuf;

use inlaysql::{Database, Value};

/// Enough single-row commits to fill a one-mebibyte region several times over
/// at any record size either format produces, so the wrap, the state-block
/// checkpoint and the region rescan all happen inside the run rather than
/// being hoped for.
const COMMITS: i64 = 700;

fn scratch(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inlaysql-old-format-{name}-{}.inlay",
        std::process::id()
    ));
    path
}

/// The header's version field, read straight off block zero.
fn header_version(path: &PathBuf) -> u32 {
    let bytes = std::fs::read(path).expect("read the database file");
    u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]])
}

#[test]
fn a_v5_database_is_still_read_and_written_as_a_v5_database() {
    // Before any database is opened: the switch is latched on first use.
    unsafe { std::env::set_var("INLAYSQL_WHOLE_PAGE_WAL_RECORD", "1") };

    let path = scratch("v5");
    let _ = std::fs::remove_file(&path);

    let body = "b".repeat(200);
    {
        let mut db = Database::open(&path).expect("create");
        db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
            .expect("create table");
        let insert = db
            .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
            .expect("prepare");
        for id in 1..=COMMITS {
            db.execute_prepared(
                &insert,
                &[Value::Integer(id), Value::Text(body.clone().into())],
            )
            .expect("insert");
        }
    }

    assert_eq!(
        header_version(&path),
        inlaysql_core::wal::MULTI_REGION_FORMAT_VERSION,
        "the environment switch has to have produced a v5 file, or this test \
         is checking that v6 can read v6"
    );
    assert_eq!(
        inlaysql_core::btree::FORMAT_VERSION,
        inlaysql_core::wal::HOLE_ELIDED_FORMAT_VERSION,
        "this build writes v6 by default, which is what makes the file above old"
    );

    // A cold handle: no cached commit point, no cached state, everything
    // derived from the file — which means every one of those v5 records is
    // decoded as a v5 record.
    {
        let mut db = Database::open(&path).expect("reopen");
        let rows = db
            .query("SELECT COUNT(*) FROM kv", &[])
            .expect("count after reopen");
        assert_eq!(rows.rows, vec![vec![Value::Integer(COMMITS)]]);

        // And it keeps committing. A v6 record appended here would be a
        // record this file's own scan rejects.
        let insert = db
            .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
            .expect("prepare");
        for id in COMMITS + 1..=COMMITS * 2 {
            db.execute_prepared(
                &insert,
                &[Value::Integer(id), Value::Text(body.clone().into())],
            )
            .expect("insert into a v5 file");
        }
    }

    assert_eq!(
        header_version(&path),
        inlaysql_core::wal::MULTI_REGION_FORMAT_VERSION,
        "a v5 database must not quietly become a v6 one by being written to"
    );

    // Cold again, and every row this file ever acknowledged is there.
    {
        let mut db = Database::open(&path).expect("reopen again");
        let rows = db
            .query("SELECT COUNT(*) FROM kv", &[])
            .expect("count after the second reopen");
        assert_eq!(rows.rows, vec![vec![Value::Integer(COMMITS * 2)]]);
        for id in [1i64, COMMITS, COMMITS + 1, COMMITS * 2] {
            let rows = db
                .query("SELECT body FROM kv WHERE id = ?", &[Value::Integer(id)])
                .expect("point read");
            assert_eq!(
                rows.rows,
                vec![vec![Value::Text(body.clone().into())]],
                "row {id} did not survive"
            );
        }
    }

    let _ = std::fs::remove_file(&path);
}
