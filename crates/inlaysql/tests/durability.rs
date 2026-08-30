//! `EngineOptions::durability` end-to-end, through the real `FileDevice` /
//! `CommitCoordinator` — not the simulation harness.
//!
//! `crates/inlaysql-core/tests/durability_dst.rs` covers the deterministic
//! crash/torn-write/reordered-sync side of this on the simulated device. What
//! that harness cannot exercise at all is the actual barrier a real file
//! takes, or the cross-handle ratchet `CommitCoordinator::durability`
//! implements — `SimDisk`/`Simulator` have only one sync strength (see that
//! file's module doc for the harness gap this states explicitly). So this
//! file covers the fast, deterministic, no-fault-injection half: the happy
//! path (data survives an ordinary close/reopen at `Durability::Normal`
//! exactly as it does at `Durability::Full`) and the functional shape of the
//! "strongest wins" rule (two handles on one file disagreeing never errors,
//! deadlocks, or loses a commit — only `crates/inlaysql/src/device.rs`'s
//! white-box `group_commit_tests` module can observe *which* barrier the
//! coordinator actually chose, since that is process-internal state with no
//! public surface).

use std::path::{Path, PathBuf};

use inlaysql::{Database, Durability, EngineOptions, Value};

/// A database file that deletes itself when the test ends, whatever the
/// outcome — mirrors the same helper in `free_list_growth.rs`.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-durability-test-{name}-{}-{}.inlay",
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
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn options(durability: Durability) -> EngineOptions {
    EngineOptions {
        durability,
        ..EngineOptions::default()
    }
}

/// The default is unchanged: no existing caller sets `durability`, so every
/// one of them gets `Durability::Full`, bit for bit what `EngineOptions`
/// meant before this option existed.
#[test]
fn the_default_engine_options_stay_full_durability() {
    assert_eq!(EngineOptions::default().durability, Durability::Full);
}

/// The ordinary case with no crash involved: rows committed at
/// `Durability::Normal` survive a clean close and reopen exactly as they
/// would at `Full` — relaxing the barrier trades a loss bound on power
/// failure, not correctness on the path every commit actually takes.
#[test]
fn rows_committed_at_normal_durability_survive_a_clean_reopen() {
    let db_file = TempDb::new("normal-reopen");

    {
        let mut db = Database::open_on_with_options(
            inlaysql::FileDevice::open(db_file.path()).unwrap(),
            options(Durability::Normal),
        )
        .unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", &[])
            .unwrap();
        for id in 0..200i64 {
            db.execute(
                "INSERT INTO t (id, n) VALUES (?, ?)",
                &[Value::Integer(id), Value::Integer(id * 2)],
            )
            .unwrap();
        }
    }

    let mut reopened = Database::open(db_file.path()).unwrap();
    let rows = reopened
        .query("SELECT id, n FROM t ORDER BY id", &[])
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 200);
    for (id, row) in rows.iter().enumerate() {
        assert_eq!(row[0], Value::Integer(id as i64));
        assert_eq!(row[1], Value::Integer(id as i64 * 2));
    }
}

/// Two handles on the same file asking for different levels must not error,
/// deadlock, or lose either handle's commits — whichever order they open in.
/// This is the functional half of "strongest wins": the arbitration itself
/// (which barrier actually runs) is white-box tested in `src/device.rs`'s
/// `group_commit_tests`, since it is process-internal state with no public
/// surface to assert against from here.
#[test]
fn handles_disagreeing_on_durability_still_commit_and_recover_together() {
    let db_file = TempDb::new("disagreeing-handles");

    {
        let mut creator = Database::open(db_file.path()).unwrap();
        creator
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, tag TEXT)", &[])
            .unwrap();
    }

    // Opened first, asks for the default (Full) — deliberately kept open
    // across the whole block, so its request is live while the second
    // handle asks for something weaker.
    let mut full_handle = Database::open_on_with_options(
        inlaysql::FileDevice::open(db_file.path()).unwrap(),
        options(Durability::Full),
    )
    .unwrap();

    let mut normal_handle = Database::open_on_with_options(
        inlaysql::FileDevice::open(db_file.path()).unwrap(),
        options(Durability::Normal),
    )
    .unwrap();

    full_handle
        .execute(
            "INSERT INTO t (id, tag) VALUES (?, ?)",
            &[Value::Integer(1), Value::Text("full".into())],
        )
        .unwrap();
    normal_handle
        .execute(
            "INSERT INTO t (id, tag) VALUES (?, ?)",
            &[Value::Integer(2), Value::Text("normal".into())],
        )
        .unwrap();

    drop(full_handle);
    drop(normal_handle);

    let mut reopened = Database::open(db_file.path()).unwrap();
    let rows = reopened
        .query("SELECT id, tag FROM t ORDER BY id", &[])
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Text("full".into())],
            vec![Value::Integer(2), Value::Text("normal".into())],
        ]
    );
}
