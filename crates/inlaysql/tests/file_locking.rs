//! OS-level advisory locking on the database file (AHL-401).
//!
//! `FileDevice` coordinates handles *inside one process* through the
//! `(dev, ino)`-keyed `CommitCoordinator` registry in `src/device.rs` — that
//! is what `concurrent_writers.rs` proves. Nothing stopped, or even detected,
//! a *second process* opening the same file. This file exercises the fix:
//! `FileDevice::open` now takes an OS-level advisory lock
//! (`std::fs::File::try_lock`), acquired once per `(dev, ino)` per process
//! and held by the `CommitCoordinator` so every same-process handle shares
//! it instead of contending for its own — refcounted exactly like the
//! coordinator already was.
//!
//! What is and is not covered, precisely:
//! - Same-process multi-handle usage keeps working
//!   (`multiple_handles_in_one_process_still_work`) — a direct, fast check.
//! - The lock is released when the last handle in a process drops
//!   (`lock_is_released_after_last_handle_drops`). This is a same-process
//!   test, but a real one: each `FileDevice::open` call is an independent
//!   `open(2)`, so if the coordinator's lock were not genuinely released on
//!   drop, the reopen in this test would itself see the "locked" error.
//! - A genuine second *process* is refused, and the lock is released when
//!   that other process exits (`second_process_is_refused`). This re-invokes
//!   this very test binary as a child process — the only two-process check
//!   in this file, and the only one that proves cross-process (not just
//!   cross-handle) refusal. The child body lives in `lock_helper_process`,
//!   which is not a real test: it is gated behind `#[ignore]` (so normal
//!   `cargo test` runs never execute it) and behind an env var (so even an
//!   accidental `--ignored` run is a harmless no-op instead of a hang).
//!
//! Not covered: behaviour under a filesystem that does not support advisory
//! locks at all (e.g. some network filesystems), and Windows-specific lock
//! semantics — this file only runs where the workspace's tests run (CI is
//! Ubuntu; this was also exercised locally on macOS).
//!
//! # Read-only mode (AHL-405)
//!
//! `Database::open_read_only` / `FileDevice::open_read_only` take **no** OS
//! lock at all, on purpose: unlike a shared/exclusive `flock`, this needs no
//! cooperation from — or even awareness of — the writer, so it coexists with
//! a read-write handle in another process without a sidecar lock file. The
//! tests below exercise the two-process case with the same helper pattern as
//! `second_process_is_refused`
//! (`read_only_handle_opens_beside_a_writer_in_another_process`), the
//! snapshot-refresh consequence of `commit_generation` always answering
//! `None` for a read-only device
//! (`read_only_handle_sees_a_commit_made_after_it_opened`), the write refusal
//! (`writes_through_a_read_only_handle_are_refused`), the missing-path error
//! (`opening_a_missing_path_read_only_is_an_error`), and two read-only
//! handles coexisting (`two_read_only_handles_coexist`).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use inlaysql::{Database, FileDevice, Value};

/// A database file path that is removed on drop, whatever the outcome.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-file-locking-{name}-{}.inlay",
            std::process::id()
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

/// Two (and more) handles in the same process must keep working: the
/// in-process `CommitCoordinator` is still what serialises them, and it must
/// share the one OS lock rather than each handle trying to take its own —
/// which, since OS advisory locks are scoped to the open file description and
/// not the process, would otherwise deadlock a single process against
/// itself.
#[test]
fn multiple_handles_in_one_process_still_work() {
    let db = TempDb::new("multi-handle");

    let mut first = Database::open(db.path()).expect("first handle opens");
    first
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    first
        .execute("INSERT INTO kv (id, n) VALUES (1, 10)", &[])
        .unwrap();
    drop(first);

    // A second, independent handle on the same path, opened after the first
    // is gone, must still succeed and see the committed row.
    let mut second = Database::open(db.path()).expect("second handle in-process opens");
    let rows = second.query("SELECT n FROM kv WHERE id = 1", &[]).unwrap();
    assert_eq!(rows.rows.len(), 1);

    // A *third* handle, concurrent with the second (both alive at once), is
    // the exact case the coordinator registry exists for: it must not
    // deadlock or be refused just because another handle on the same path is
    // still open in this process.
    let third = FileDevice::open(db.path());
    assert!(
        third.is_ok(),
        "a second concurrent in-process handle must not be refused: {:?}",
        third.err()
    );
    drop(third);
    drop(second);
}

/// Opening and dropping handles from several threads at once must never make
/// a process look like a *different* process to itself.
///
/// The window this is aimed at: `Weak::upgrade` in the coordinator registry
/// starts failing the instant the last strong reference's count reaches zero,
/// which is before the thread doing that drop has closed the coordinator's
/// lock file. A thread opening the database inside that window would see
/// `WouldBlock` from a lock this very process still holds, and report it as
/// another process — the failure a connection-per-handle server would hit
/// whenever its connection count touched zero. `coordinator_for` retries
/// rather than believing the first `WouldBlock`.
///
/// **What this test does and does not prove.** It is a churn smoke test, not
/// a regression test for that retry: the window is a handful of instructions
/// between an atomic decrement and a `close(2)`, and it does not reproduce.
/// Twelve thousand open/close cycles across eight threads with the retry
/// disabled failed to provoke it even once. So this asserts the honest,
/// weaker property — concurrent open/close churn does not spuriously refuse —
/// and the retry itself stands on its reasoning, not on this test. Do not
/// read a pass here as evidence the retry works.
#[test]
fn reopening_while_another_thread_drops_a_handle_is_not_mistaken_for_another_process() {
    const THREADS: usize = 4;
    const ROUNDS: usize = 40;

    let db = TempDb::new("open-close-churn");
    let path = db.path().to_path_buf();

    let threads: Vec<_> = (0..THREADS)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                for _ in 0..ROUNDS {
                    // Open and immediately drop, so every thread is
                    // repeatedly taking the registry to zero strong
                    // references while the others race to open.
                    let handle = FileDevice::open(&path)?;
                    drop(handle);
                }
                Ok::<(), inlaysql::Error>(())
            })
        })
        .collect();

    for thread in threads {
        let result = thread.join().expect("open/close thread panicked");
        assert!(
            result.is_ok(),
            "a same-process reopen was refused: {:?}",
            result.err()
        );
    }
}

/// After the last handle in the process drops, the OS lock must be released:
/// opening the same path again afterwards succeeds.
#[test]
fn lock_is_released_after_last_handle_drops() {
    let db = TempDb::new("release-on-drop");

    let handle = FileDevice::open(db.path()).expect("first open");
    drop(handle);

    let reopened = FileDevice::open(db.path());
    assert!(
        reopened.is_ok(),
        "reopening after the only handle dropped must succeed: {:?}",
        reopened.err()
    );
}

const HELPER_PATH_VAR: &str = "INLAYSQL_FILE_LOCKING_HELPER_PATH";
const HELPER_READY: &str = "LOCKED";

/// Not a real test: the body a child process runs for
/// `second_process_is_refused`. Gated behind `#[ignore]` so `cargo test`
/// never runs it on its own, and behind the env var so an `--ignored` run
/// without the var set is a harmless no-op rather than a hang.
///
/// When invoked correctly, it opens the path named by
/// `INLAYSQL_FILE_LOCKING_HELPER_PATH`, reports success or failure on
/// stdout, and — on success — blocks reading stdin so the parent decides
/// exactly when this process, and the OS lock it holds, goes away.
#[test]
#[ignore = "subprocess helper for second_process_is_refused, not a standalone test"]
fn lock_helper_process() {
    let Some(path) = std::env::var_os(HELPER_PATH_VAR) else {
        return;
    };
    match FileDevice::open(&path) {
        Ok(device) => {
            println!("{HELPER_READY}");
            let _ = std::io::stdout().flush();
            // Hold the lock until the parent closes our stdin.
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            drop(device);
        }
        Err(e) => {
            println!("FAILED: {e}");
            let _ = std::io::stdout().flush();
        }
    }
}

/// A second *process* opening a file this process already has open for
/// writing must be refused immediately — not left to block forever — with an
/// error that names the file and says plainly that another process holds it.
#[test]
fn second_process_is_refused() {
    let db = TempDb::new("second-process");
    let exe = std::env::current_exe().expect("current test binary path");

    let mut child = Command::new(&exe)
        .arg("lock_helper_process")
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .env(HELPER_PATH_VAR, db.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper process");

    // The libtest harness the child runs under prints its own preamble
    // ("running 1 test", ...) before our helper's own output, so scan for
    // the marker line rather than assuming it is the first line out.
    let stdout = child.stdout.take().expect("helper stdout piped");
    let result_line = read_marker_line_with_timeout(stdout, Duration::from_secs(10))
        .expect("helper process reported a result before timing out");
    assert_eq!(
        result_line, HELPER_READY,
        "helper process failed to lock the file"
    );

    // The helper process now holds the OS lock. Opening the same path from
    // *this* process (a different process, from the OS's point of view) must
    // be refused, not block.
    let refused = FileDevice::open(db.path());
    let message = match refused {
        Ok(_) => panic!("a second process must be refused, not allowed to open"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains(&db.path().display().to_string()),
        "error should name the file: {message}"
    );
    assert!(
        message.to_lowercase().contains("process"),
        "error should say plainly that another process holds it: {message}"
    );

    // Tell the helper to exit, releasing its lock.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "bye");
        drop(stdin);
    }
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    assert!(
        status.is_some(),
        "helper process did not exit in time; its lock may still be held"
    );

    // Now that the other process is gone, opening must succeed again.
    let reopened = FileDevice::open(db.path());
    assert!(
        reopened.is_ok(),
        "reopening after the other process exited must succeed: {:?}",
        reopened.err()
    );
}

// --------------------------------------------------- read-only mode (AHL-405)

/// The `id` column of `kv`, in order — the same shape `concurrent_writers.rs`
/// uses for its own refresh assertions.
fn select_ids(db: &mut Database) -> Vec<i64> {
    db.query("SELECT id FROM kv ORDER BY id", &[])
        .unwrap()
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("id came back as {other:?}"),
        })
        .collect()
}

/// Seeds `path` with a `kv (id INTEGER PRIMARY KEY, n INTEGER)` table holding
/// one row (`id = 1`), through a normal read-write handle that is closed
/// again before returning — so the OS lock is released and the caller starts
/// from a clean, unlocked, already-a-real-database file.
fn seed_kv(path: &Path) {
    let mut writer = Database::open(path).expect("seed writer opens");
    writer
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .expect("create table");
    writer
        .execute("INSERT INTO kv (id, n) VALUES (1, 10)", &[])
        .expect("seed row");
}

/// A read-only handle must open successfully *while a read-write handle in
/// another process holds the file's exclusive OS lock*, and must see the data
/// that was committed before the reader opened. This is the case the whole
/// change exists for: `docs/mcp.md`'s `inlaysql serve --mcp app.inlay` beside
/// a running application.
///
/// Uses the same two-process helper (`lock_helper_process`) as
/// `second_process_is_refused`: the helper opens the file read-write and
/// blocks holding the lock, and this process — a genuinely different one, not
/// just a different handle — opens read-only beside it.
#[test]
fn read_only_handle_opens_beside_a_writer_in_another_process() {
    let db = TempDb::new("readonly-second-process");
    seed_kv(db.path());

    let exe = std::env::current_exe().expect("current test binary path");
    let mut child = Command::new(&exe)
        .arg("lock_helper_process")
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .env(HELPER_PATH_VAR, db.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper process");

    let stdout = child.stdout.take().expect("helper stdout piped");
    let result_line = read_marker_line_with_timeout(stdout, Duration::from_secs(10))
        .expect("helper process reported a result before timing out");
    assert_eq!(
        result_line, HELPER_READY,
        "helper process failed to lock the file"
    );

    // The helper process now holds the exclusive OS lock as a writer. A
    // read-only handle takes no lock at all, so opening it here — a
    // different process from the helper's point of view — must succeed
    // rather than being refused, and it must see the row seeded before the
    // helper started.
    let mut reader = Database::open_read_only(db.path())
        .expect("a read-only handle must open beside a writer in another process");
    assert_eq!(select_ids(&mut reader), vec![1]);
    drop(reader);

    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "bye");
        drop(stdin);
    }
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    assert!(
        status.is_some(),
        "helper process did not exit in time; its lock may still be held"
    );
}

/// A read-only handle has no in-process proof that nothing changed — its
/// `commit_generation` always answers `None` — so it must re-derive the
/// committed state from the file on every statement and pick up a commit made
/// by a writer *after* the reader opened. Both handles are in this process
/// here (that is enough to exercise the mechanism: the read-only device never
/// consults the writer's in-process counter either way), while
/// `read_only_handle_opens_beside_a_writer_in_another_process` covers the
/// genuinely cross-process case.
#[test]
fn read_only_handle_sees_a_commit_made_after_it_opened() {
    let db = TempDb::new("readonly-refresh");
    seed_kv(db.path());

    let mut writer = Database::open(db.path()).expect("writer opens");
    let mut reader = Database::open_read_only(db.path()).expect("reader opens");

    assert_eq!(select_ids(&mut reader), vec![1]);

    writer
        .execute("INSERT INTO kv (id, n) VALUES (2, 20)", &[])
        .expect("writer inserts a second row");

    assert_eq!(
        select_ids(&mut reader),
        vec![1, 2],
        "a read-only handle must see a commit made after it opened"
    );
}

/// A write statement — ad hoc or prepared — through a read-only handle must
/// be refused with a clear error before it changes anything, not silently
/// accepted and not a panic.
#[test]
fn writes_through_a_read_only_handle_are_refused() {
    let db = TempDb::new("readonly-write-refused");
    seed_kv(db.path());

    let mut reader = Database::open_read_only(db.path()).expect("reader opens");

    let result = reader.execute("INSERT INTO kv (id, n) VALUES (2, 20)", &[]);
    let message = match result {
        Ok(outcome) => panic!("a write through a read-only handle must be refused: {outcome:?}"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.to_lowercase().contains("read-only")
            || message.to_lowercase().contains("read only"),
        "error should say the handle is read-only: {message}"
    );

    // A prepared write statement must be refused the same way.
    let insert = reader
        .prepare("INSERT INTO kv (id, n) VALUES (3, 30)")
        .expect("prepare a write statement");
    assert!(
        reader.execute_prepared(&insert, &[]).is_err(),
        "a prepared write through a read-only handle must be refused"
    );

    // Nothing was written: only the seeded row is still there.
    assert_eq!(select_ids(&mut reader), vec![1]);
}

/// Opening a path that does not exist read-only must be an error, not a
/// silently created empty database — unlike `Database::open`, which creates
/// one.
#[test]
fn opening_a_missing_path_read_only_is_an_error() {
    let db = TempDb::new("readonly-missing");
    // `TempDb::new` removes any file at this path and nothing recreates it.

    let result = Database::open_read_only(db.path());
    assert!(
        result.is_err(),
        "opening a missing path read-only must be an error, not an empty database"
    );
}

/// Two read-only handles on the same file, neither of which takes an OS lock,
/// must coexist without either refusing the other.
#[test]
fn two_read_only_handles_coexist() {
    let db = TempDb::new("readonly-readonly");
    seed_kv(db.path());

    let mut first = Database::open_read_only(db.path()).expect("first read-only handle opens");
    let mut second =
        Database::open_read_only(db.path()).expect("second read-only handle must not be refused");

    assert_eq!(select_ids(&mut first), vec![1]);
    assert_eq!(select_ids(&mut second), vec![1]);
}

/// Reads lines from `stdout` until one is exactly `"LOCKED"` or starts with
/// `"FAILED:"` (the two lines `lock_helper_process` ever prints), or the
/// stream ends, or `timeout` elapses. Everything else — the libtest harness's
/// own preamble and summary lines — is skipped.
fn read_marker_line_with_timeout(
    stdout: impl std::io::Read + Send + 'static,
    timeout: Duration,
) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            if trimmed == HELPER_READY || trimmed.starts_with("FAILED:") {
                let _ = tx.send(trimmed.to_string());
                return;
            }
        }
    });
    rx.recv_timeout(timeout).ok()
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
