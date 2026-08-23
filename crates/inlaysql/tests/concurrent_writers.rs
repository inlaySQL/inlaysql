//! Several writers on one database file.
//!
//! The storage engine settles concurrent commits by first-committer-wins: the
//! loser's transaction is rolled back and it is told so. What this file pins
//! down is the *told so* — the property a benchmark measuring concurrent-writer
//! throughput depends on, and the one that was missing.
//!
//! Until [`Error::Conflict`] existed, the tree reported the lost race and the
//! layer above discarded the report, so `execute` returned `Ok` for a write
//! that had just been thrown away. Ten inserts across two handles left five
//! rows and no error anywhere. A number measured on that is not a throughput
//! number, it is a data-loss rate.
//!
//! The first tests use a shared in-process device to pin conflict reporting
//! precisely. `parallel_file_handles_commit_disjoint_rows_without_false_conflicts`
//! opens independent file handles on OS threads: their commit reservations are
//! ordered, but their per-region WAL appends and durability syncs overlap.
//!
//! The last group is the other half of the same story (AHL-400). A handle now
//! re-reads the committed state at the start of every statement it runs outside
//! an explicit transaction, so a reader beside a writer stops being frozen at
//! the moment it opened. Two consequences are pinned here: a reader sees a
//! commit it did not make, and a handle inside `BEGIN` still does not — its
//! snapshot is pinned until the transaction ends, which is what makes the
//! conflict tests above reproducible in the first place.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql::{Database, Error, FileDevice, Value};

/// A database file that deletes itself when the test ends, whatever the
/// outcome — including a panic.
struct TempDb {
    path: std::path::PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("inlaysql-{name}-{}.inlay", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    fn device(&self) -> Rc<RefCell<FileDevice>> {
        Rc::new(RefCell::new(FileDevice::open(&self.path).unwrap()))
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Open `count` writers on one device, with the table already created.
fn writers(device: &Rc<RefCell<FileDevice>>, count: usize) -> Vec<Database> {
    let mut creator = Database::open_on(device.clone()).unwrap();
    creator
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    drop(creator);
    (0..count)
        .map(|_| Database::open_on(device.clone()).unwrap())
        .collect()
}

fn insert(db: &mut Database, id: i64) -> Result<(), Error> {
    db.execute(
        "INSERT INTO kv (id, n) VALUES (?, ?)",
        &[Value::Integer(id), Value::Integer(id)],
    )
    .map(|_| ())
}

fn ids(device: &Rc<RefCell<FileDevice>>) -> Vec<i64> {
    let mut reader = Database::open_on(device.clone()).unwrap();
    reader
        .query("SELECT id FROM kv ORDER BY id", &[])
        .unwrap()
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("id came back as {other:?}"),
        })
        .collect()
}

#[test]
fn the_writer_that_loses_the_race_is_told_it_lost() {
    let temp = TempDb::new("conflict-reported");
    let device = temp.device();
    let [mut a, mut b] = <[Database; 2]>::try_from(writers(&device, 2)).ok().unwrap();

    // B pins its snapshot before A commits. Two writers can only overlap on a
    // row if they planned against a state that did not have it, and an explicit
    // transaction is the one place that state is held still: outside one, B
    // would re-read the committed root at the start of its INSERT, find A's row
    // and report the constraint instead — see the test below.
    b.begin().unwrap();

    // A commits first to the same row B intends to insert.
    insert(&mut a, 1).unwrap();
    insert(&mut b, 1).unwrap();
    let outcome = b.commit();

    assert_eq!(
        outcome,
        Err(Error::Conflict),
        "B overlapped A's row and must be told the write was rolled back"
    );
    assert_eq!(
        ids(&device),
        vec![1],
        "the rolled-back row must not be in the file"
    );
}

#[test]
fn a_second_writer_outside_a_transaction_sees_the_row_instead_of_conflicting() {
    let temp = TempDb::new("conflict-becomes-constraint");
    let device = temp.device();
    let [mut a, mut b] = <[Database; 2]>::try_from(writers(&device, 2)).ok().unwrap();

    insert(&mut a, 1).unwrap();

    // B is not in a transaction, so its INSERT starts by re-reading the
    // committed state and plans against a database that already holds row 1.
    // The duplicate key is then an ordinary constraint violation, decided
    // before anything is written, rather than a lost optimistic race decided at
    // commit. This is the same answer SQLite and MySQL give, and it is only
    // possible because the snapshot moved.
    assert_eq!(
        insert(&mut b, 1),
        Err(Error::Constraint(
            "UNIQUE constraint failed: kv.id".to_string()
        ))
    );
    assert_eq!(ids(&device), vec![1]);
}

#[test]
fn a_conflicted_handle_is_still_usable() {
    let temp = TempDb::new("conflict-recovers");
    let device = temp.device();
    let [mut a, mut b] = <[Database; 2]>::try_from(writers(&device, 2)).ok().unwrap();

    b.begin().unwrap();
    insert(&mut a, 1).unwrap();
    insert(&mut b, 1).unwrap();
    assert_eq!(b.commit(), Err(Error::Conflict));

    // B reloaded from A's committed state when it lost, so its next attempt
    // starts from that state rather than from the one it lost with. Nobody
    // else commits in between, so this one lands.
    insert(&mut b, 2).unwrap();

    assert_eq!(ids(&device), vec![1, 2]);
    // And B can see the row A wrote, which is the proof it reloaded rather
    // than merely dropping its own transaction.
    assert_eq!(
        b.query("SELECT id FROM kv ORDER BY id", &[])
            .unwrap()
            .rows
            .len(),
        2
    );
}

#[test]
fn disjoint_stale_writers_rebase_without_losing_writes() {
    const WRITERS: usize = 4;
    const PER_WRITER: i64 = 25;

    let temp = TempDb::new("conflict-retry");
    let device = temp.device();
    let mut writers = writers(&device, WRITERS);

    // Round robin makes every writer's root stale, while the row keys remain
    // disjoint. Those transactions rebase instead of producing false conflicts.
    let mut conflicts = 0;
    for round in 0..PER_WRITER {
        for (index, db) in writers.iter_mut().enumerate() {
            let id = round * WRITERS as i64 + index as i64 + 1;
            // Retry until it commits. Each conflict reloads the handle onto
            // the winner's state, so this terminates: the retry is never
            // starting from the state it just lost with.
            loop {
                match insert(db, id) {
                    Ok(()) => break,
                    Err(Error::Conflict) => conflicts += 1,
                    Err(other) => panic!("writer {index} failed on id {id}: {other}"),
                }
            }
        }
    }

    let expected: Vec<i64> = (1..=PER_WRITER * WRITERS as i64).collect();
    assert_eq!(
        ids(&device),
        expected,
        "every write that returned Ok must be in the file"
    );
    assert_eq!(conflicts, 0, "disjoint row writes should rebase cleanly");
}

#[test]
fn parallel_file_handles_commit_disjoint_rows_without_false_conflicts() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    const WRITERS: usize = 4;
    const PER_WRITER: i64 = 25;

    let temp = TempDb::new("parallel-conflict-retry");
    let mut creator = Database::open(temp.path()).unwrap();
    creator
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    drop(creator);

    let barrier = Arc::new(Barrier::new(WRITERS));
    let max_writer_version = Arc::new(AtomicU64::new(0));
    let conflicts: usize = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for index in 0..WRITERS {
            let barrier = barrier.clone();
            let max_writer_version = max_writer_version.clone();
            let path = temp.path().to_path_buf();
            workers.push(scope.spawn(move || {
                let mut db = Database::open(path).unwrap();
                barrier.wait();
                let mut conflicts = 0;
                for round in 0..PER_WRITER {
                    let id = round * WRITERS as i64 + index as i64 + 1;
                    loop {
                        match insert(&mut db, id) {
                            Ok(()) => break,
                            Err(Error::Conflict) => conflicts += 1,
                            Err(other) => panic!("writer {index} failed on id {id}: {other}"),
                        }
                    }
                }
                max_writer_version.fetch_max(db.change_version(), Ordering::Relaxed);
                conflicts
            }));
        }
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum()
    });

    let reader = FileDevice::open(temp.path()).unwrap();
    let device = Rc::new(RefCell::new(reader));
    let expected: Vec<i64> = (1..=PER_WRITER * WRITERS as i64).collect();
    assert_eq!(ids(&device), expected);
    assert_eq!(
        conflicts, 0,
        "disjoint parallel writes should rebase cleanly"
    );
    assert_eq!(
        max_writer_version.load(Ordering::Relaxed),
        expected.len() as u64,
        "the last writer must learn the commit version storage assigned it"
    );

    // Monotonic metadata is rebased with the rows: CDC versions must remain a
    // gap-free commit order rather than overwriting one another at a stale
    // version number.
    let reader = Database::open(temp.path()).unwrap();
    let changes = reader.changes(0).unwrap();
    assert_eq!(changes.changes.len(), expected.len());
    let versions: Vec<u64> = changes
        .changes
        .iter()
        .map(|change| change.version)
        .collect();
    assert_eq!(versions, (1..=expected.len() as u64).collect::<Vec<_>>());
}

/// Group commit's whole job (AHL-461) is to let concurrent committers share
/// `fsync`/`F_FULLFSYNC` calls without weakening what a successful commit
/// means. This pins the observable half of that contract: with several
/// writer threads committing in lockstep — a `Barrier` per round forces their
/// commits to line up the way real concurrent clients would, which is what
/// gives the leader/follower batching in `FileDevice::sync` something to
/// batch — every commit that returns `Ok` must still have a row that survives
/// a completely fresh handle opened after every writer has exited. A follower
/// that wrongly skipped its own fsync would show up here as a missing row,
/// not as an error: nothing upstream of storage would ever know.
///
/// The internal proof that a follower only ever skips when a leader's flush
/// target provably covered it — and never otherwise — is a separate,
/// deterministic white-box test of `CommitCoordinator::make_durable` next to
/// its implementation in `crates/inlaysql/src/device.rs`.
#[test]
fn group_commit_batches_concurrent_committers_without_losing_a_row() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    const WRITERS: usize = 8;
    const ROUNDS: i64 = 20;

    let temp = TempDb::new("group-commit-survives-reopen");
    let mut creator = Database::open(temp.path()).unwrap();
    creator
        .execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();
    drop(creator);

    let round_barrier = Arc::new(Barrier::new(WRITERS));
    let conflicts = Arc::new(AtomicU64::new(0));

    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for index in 0..WRITERS {
            let round_barrier = round_barrier.clone();
            let conflicts = conflicts.clone();
            let path = temp.path().to_path_buf();
            workers.push(scope.spawn(move || {
                let mut db = Database::open(path).unwrap();
                for round in 0..ROUNDS {
                    // Every writer arrives here together, so their commits —
                    // and therefore their `Device::sync` calls — are as close
                    // to simultaneous as this test can make them.
                    round_barrier.wait();
                    // Disjoint ids, exactly as the round-robin scheme above:
                    // no writer should ever see a real conflict.
                    let id = round * WRITERS as i64 + index as i64 + 1;
                    match insert(&mut db, id) {
                        Ok(()) => {}
                        Err(Error::Conflict) => {
                            conflicts.fetch_add(1, Ordering::Relaxed);
                            // Retry once, outside the barrier: a conflict here
                            // would mean this test's id scheme collided, not
                            // that the commit was ever left unacknowledged.
                            insert(&mut db, id)
                                .unwrap_or_else(|e| panic!("writer {index} retry failed: {e}"));
                        }
                        Err(other) => panic!("writer {index} failed on id {id}: {other}"),
                    }
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });

    assert_eq!(
        conflicts.load(Ordering::Relaxed),
        0,
        "disjoint round-robin ids should never conflict"
    );

    // Every writer thread has exited, so every `Database`/`FileDevice` handle
    // it held is gone: this is a genuinely fresh handle, not one that could be
    // trusting cached state left over from a writer's own commit.
    let reader = FileDevice::open(temp.path()).unwrap();
    let device = Rc::new(RefCell::new(reader));
    let expected: Vec<i64> = (1..=ROUNDS * WRITERS as i64).collect();
    assert_eq!(
        ids(&device),
        expected,
        "every commit that reported success must have a row that survives a \
         fresh open — a follower that skipped its own fsync must have been \
         genuinely covered by the leader's"
    );
}

/// The commit path no longer re-derives the committed state and the log's
/// append position from the file on every commit — a read-write `FileDevice`
/// hands both back from memory, which is what took the reservation gate off
/// the critical path (AHL-468). The cached answer is believed without being
/// checked, so what has to be pinned is the one thing that can invalidate it:
/// a **log region wrap**, where the region is rewritten from the start and
/// every offset in it stops meaning what it meant.
///
/// So this writes a fat enough payload, for long enough, that every writer's
/// region wraps several times while the others are committing into theirs, and
/// then asserts the file holds exactly the rows the writers were told they
/// committed. A cached append offset that survived a wrap would place a record
/// on top of a live one, or behind the zeroed prefix where no scan will ever
/// find it; either way the row goes missing here and nowhere upstream of
/// storage would ever know.
#[test]
fn writers_whose_log_regions_wrap_repeatedly_lose_no_row() {
    use std::sync::{Arc, Barrier};

    const WRITERS: usize = 4;
    const PER_WRITER: i64 = 120;
    /// Big enough that a commit record is a sizeable fraction of a region, so
    /// the 1 MiB regions wrap several times over the run rather than never.
    const PAYLOAD: usize = 2000;

    let temp = TempDb::new("wal-wrap-under-concurrency");
    let mut creator = Database::open(temp.path()).unwrap();
    creator
        .execute(
            "CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER, body TEXT)",
            &[],
        )
        .unwrap();
    drop(creator);

    let barrier = Arc::new(Barrier::new(WRITERS));
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for index in 0..WRITERS {
            let barrier = barrier.clone();
            let path = temp.path().to_path_buf();
            workers.push(scope.spawn(move || {
                let mut db = Database::open(path).unwrap();
                let body = "x".repeat(PAYLOAD);
                barrier.wait();
                for round in 0..PER_WRITER {
                    let id = round * WRITERS as i64 + index as i64 + 1;
                    loop {
                        match db.execute(
                            "INSERT INTO kv (id, n, body) VALUES (?, ?, ?)",
                            &[
                                Value::Integer(id),
                                Value::Integer(id),
                                Value::Text(body.clone().into()),
                            ],
                        ) {
                            Ok(_) => break,
                            Err(Error::Conflict) => {}
                            Err(other) => panic!("writer {index} failed on id {id}: {other}"),
                        }
                    }
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });

    let reader = FileDevice::open(temp.path()).unwrap();
    let device = Rc::new(RefCell::new(reader));
    let expected: Vec<i64> = (1..=PER_WRITER * WRITERS as i64).collect();
    assert_eq!(
        ids(&device),
        expected,
        "every row acknowledged across repeated log-region wraps must survive a \
         fresh open"
    );
}

// ------------------------------------------------------- snapshot refresh

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

#[test]
fn a_reader_opened_before_a_commit_sees_the_row_afterwards() {
    let temp = TempDb::new("reader-refresh");
    let device = temp.device();
    let [mut writer, mut reader] = <[Database; 2]>::try_from(writers(&device, 2)).ok().unwrap();

    // The reader's snapshot is taken here, while the table is still empty. This
    // is the read that used to freeze it: before AHL-400 the committed root was
    // cached at open and only ever re-read inside the handle's own commit, so a
    // handle that never wrote never moved.
    assert_eq!(select_ids(&mut reader), Vec::<i64>::new());

    insert(&mut writer, 1).unwrap();
    insert(&mut writer, 2).unwrap();

    // Nothing happened on the reader's side: it did not write, did not commit,
    // and was not reopened. The statement boundary alone is what advances it.
    assert_eq!(select_ids(&mut reader), vec![1, 2]);

    // And it keeps up: a later commit is visible to the next statement too.
    insert(&mut writer, 3).unwrap();
    assert_eq!(select_ids(&mut reader), vec![1, 2, 3]);
}

#[test]
fn a_reader_picks_up_a_table_another_handle_created() {
    let temp = TempDb::new("reader-refresh-catalog");
    let device = temp.device();
    let [mut writer, mut reader] = <[Database; 2]>::try_from(writers(&device, 2)).ok().unwrap();

    assert!(reader.query("SELECT id FROM later", &[]).is_err());

    writer
        .execute(
            "CREATE TABLE later (id INTEGER PRIMARY KEY, n INTEGER)",
            &[],
        )
        .unwrap();
    writer
        .execute("INSERT INTO later (id, n) VALUES (7, 7)", &[])
        .unwrap();

    // A `CREATE TABLE` changes no row, so the write version does not move; the
    // catalog is the thing that did. The refresh has to notice both, or a
    // server connection would answer "no such table" for a table its own
    // migration had already created on another connection.
    let rows = reader.query("SELECT id FROM later", &[]).unwrap().rows;
    assert_eq!(rows, vec![vec![Value::Integer(7)]]);
}

#[test]
fn a_handle_inside_a_transaction_keeps_its_pinned_snapshot() {
    let temp = TempDb::new("transaction-pinned");
    let device = temp.device();
    let [mut writer, mut reader] = <[Database; 2]>::try_from(writers(&device, 2)).ok().unwrap();

    insert(&mut writer, 1).unwrap();
    reader.begin().unwrap();
    assert_eq!(select_ids(&mut reader), vec![1]);

    // The writer commits while the reader's transaction is open.
    insert(&mut writer, 2).unwrap();

    // Every statement inside the transaction reads the state it was pinned at,
    // however many times it is asked and however much has been committed since.
    assert_eq!(select_ids(&mut reader), vec![1]);
    assert_eq!(select_ids(&mut reader), vec![1]);

    // Ending the transaction releases the pin — and this reader wrote nothing,
    // so there is nothing to conflict over.
    reader.commit().unwrap();
    assert_eq!(select_ids(&mut reader), vec![1, 2]);
}

#[test]
fn a_rolled_back_transaction_also_releases_the_pin() {
    let temp = TempDb::new("transaction-pinned-rollback");
    let device = temp.device();
    let [mut writer, mut reader] = <[Database; 2]>::try_from(writers(&device, 2)).ok().unwrap();

    insert(&mut writer, 1).unwrap();
    reader.begin().unwrap();
    assert_eq!(select_ids(&mut reader), vec![1]);

    insert(&mut writer, 2).unwrap();
    assert_eq!(select_ids(&mut reader), vec![1], "still pinned");

    reader.rollback().unwrap();
    assert_eq!(select_ids(&mut reader), vec![1, 2]);
}

// -------------------------------------------------------- last insert row id

#[test]
fn last_insert_row_id_reports_only_the_keys_the_engine_chose() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", &[])
        .unwrap();

    // Nothing inserted yet.
    assert_eq!(db.last_insert_row_id(), None);

    // Omitted key: assigned, and reported.
    db.execute("INSERT INTO kv (n) VALUES (10)", &[]).unwrap();
    assert_eq!(db.last_insert_row_id(), Some(1));

    // Explicit key: the caller already knows it, so the last assigned id
    // stands. The counter still moves past it.
    db.execute("INSERT INTO kv (id, n) VALUES (50, 11)", &[])
        .unwrap();
    assert_eq!(db.last_insert_row_id(), Some(1));

    // An explicit NULL is the same as omitting the column.
    db.execute("INSERT INTO kv (id, n) VALUES (NULL, 12)", &[])
        .unwrap();
    assert_eq!(db.last_insert_row_id(), Some(51));

    // Multi-row: the last row that was assigned a key.
    db.execute("INSERT INTO kv (n) VALUES (13), (14), (15)", &[])
        .unwrap();
    assert_eq!(db.last_insert_row_id(), Some(54));

    // Statements that insert nothing leave it alone.
    db.execute("UPDATE kv SET n = 0 WHERE id = 1", &[]).unwrap();
    db.execute("DELETE FROM kv WHERE id = 99", &[]).unwrap();
    db.query("SELECT id FROM kv", &[]).unwrap();
    assert_eq!(db.last_insert_row_id(), Some(54));

    // A failed INSERT reports no id: there is no row to point at.
    assert!(db
        .execute("INSERT INTO kv (id, n) VALUES (1, 16)", &[])
        .is_err());
    assert_eq!(db.last_insert_row_id(), Some(54));
}

#[test]
fn last_insert_row_id_is_per_handle_and_starts_empty() {
    let temp = TempDb::new("last-insert-row-id");
    let device = temp.device();
    let [mut a, mut b] = <[Database; 2]>::try_from(writers(&device, 2)).ok().unwrap();

    a.execute("INSERT INTO kv (n) VALUES (1)", &[]).unwrap();
    assert_eq!(a.last_insert_row_id(), Some(1));

    // B can see A's row — the snapshot refreshed — but the id A was handed is
    // A's, not a property of the file.
    assert_eq!(select_ids(&mut b), vec![1]);
    assert_eq!(b.last_insert_row_id(), None);

    b.execute("INSERT INTO kv (n) VALUES (2)", &[]).unwrap();
    assert_eq!(b.last_insert_row_id(), Some(2));
    assert_eq!(a.last_insert_row_id(), Some(1));
}

#[test]
fn a_table_without_an_integer_primary_key_still_reports_its_assigned_row_id() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE notes (body TEXT)", &[]).unwrap();
    db.execute("INSERT INTO notes (body) VALUES ('one')", &[])
        .unwrap();
    // The key is not in any column, but it is still the key the row is stored
    // under and still what SQLite's last_insert_rowid() would report.
    assert_eq!(db.last_insert_row_id(), Some(1));
    db.execute("INSERT INTO notes (body) VALUES ('two')", &[])
        .unwrap();
    assert_eq!(db.last_insert_row_id(), Some(2));
}
