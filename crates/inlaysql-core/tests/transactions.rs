//! Explicit transactions: a batch of statements as one durable unit.
//!
//! The claims here are counted, not timed, exactly as the rest of the suite:
//! a transaction is "one commit" because the commit counter advances once,
//! "one `fsync`" because the simulated device records one sync, and a rollback
//! is "byte-identical" because the simulator's durable image does not move.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::row::RowBuf;
use inlaysql_core::sim::{FaultSchedule, SimDisk, Simulator};
use inlaysql_core::traits::{RowId, Storage};
use inlaysql_core::{Engine, Error, Result, TreeStorage, Value};

const BLOCK: usize = 512;
const CAPACITY: usize = 8 << 20;

/// `MemStorage` that counts how often the engine commits.
struct CommitCounter {
    inner: MemStorage,
    commits: Rc<Cell<usize>>,
}

impl Storage for CommitCounter {
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        self.inner.put_row(table, id, bytes)
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        self.inner.get_row(table, id)
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        self.inner.delete_row(table, id)
    }

    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        self.inner.scan_batch(table, after, limit)
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put_meta(key, bytes)
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get_meta(key)
    }

    fn put_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.inner.put_index_entry(key)
    }

    fn delete_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.inner.delete_index_entry(key)
    }

    fn scan_index_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        self.inner.scan_index_range(start, end)
    }

    fn commit(&mut self) -> Result<()> {
        self.commits.set(self.commits.get() + 1);
        self.inner.commit()
    }

    fn rollback(&mut self) -> Result<()> {
        self.inner.rollback()
    }
}

fn counting_engine() -> (Engine, Rc<Cell<usize>>) {
    let commits = Rc::new(Cell::new(0));
    let engine = Engine::open(
        Box::new(CommitCounter {
            inner: MemStorage::new(),
            commits: commits.clone(),
        }),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .expect("open");
    (engine, commits)
}

fn create_table(engine: &mut Engine) {
    engine
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
}

fn insert(engine: &mut Engine, id: i64) {
    engine
        .execute(
            "INSERT INTO t (id, body) VALUES (?, ?)",
            &[Value::Integer(id), Value::Text("row".to_string().into())],
        )
        .unwrap();
}

#[test]
fn a_transaction_of_many_statements_is_one_commit() {
    let (mut engine, commits) = counting_engine();
    create_table(&mut engine);
    commits.set(0);

    engine.begin().unwrap();
    for id in 1..=100 {
        insert(&mut engine, id);
    }
    engine.commit().unwrap();

    assert_eq!(commits.get(), 1, "a transaction must commit exactly once");
    let rows = engine.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(rows.rows.len(), 100);
}

#[test]
fn statements_without_a_transaction_commit_one_each() {
    // The baseline the transaction is measured against: without `begin`, every
    // statement is its own commit.
    let (mut engine, commits) = counting_engine();
    create_table(&mut engine);
    commits.set(0);

    for id in 1..=10 {
        insert(&mut engine, id);
    }
    assert_eq!(commits.get(), 10);
}

#[test]
fn create_index_inside_a_transaction_is_refused() {
    let mut engine = inlaysql_core::mem::engine().unwrap();
    engine
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();

    engine.begin().unwrap();
    // Building the index scans committed state, so it would silently omit the
    // transaction's own inserts. Refused rather than wrong.
    let err = engine
        .execute("CREATE INDEX t_body ON t (body)", &[])
        .unwrap_err();
    assert!(matches!(err, Error::Transaction(_)), "got {err}");
    engine.commit().unwrap();

    // Outside the transaction the same statement succeeds.
    engine
        .execute("CREATE INDEX t_body ON t (body)", &[])
        .unwrap();
}

#[test]
fn every_row_in_one_transaction_shares_one_change_version() {
    let mut engine = inlaysql_core::mem::engine().unwrap();
    create_table(&mut engine);
    let before = engine.change_version();

    engine.begin().unwrap();
    for id in 1..=5 {
        insert(&mut engine, id);
    }
    engine.commit().unwrap();

    let changes = engine.changes(before).unwrap();
    assert_eq!(changes.changes.len(), 5);
    let version = changes.changes[0].version;
    assert!(
        changes
            .changes
            .iter()
            .all(|change| change.version == version),
        "rows changed by one transaction must share a version"
    );
    assert_eq!(engine.change_version(), before + 1);
}

#[test]
fn a_rollback_discards_every_write_of_the_transaction() {
    let mut engine = inlaysql_core::mem::engine().unwrap();
    create_table(&mut engine);
    insert(&mut engine, 1);
    let version = engine.change_version();

    engine.begin().unwrap();
    insert(&mut engine, 2);
    insert(&mut engine, 3);
    engine.rollback().unwrap();

    let rows = engine.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Integer(1)]]);
    assert_eq!(engine.change_version(), version);

    // The handle is still usable after the rollback.
    insert(&mut engine, 4);
    let rows = engine.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(rows.rows.len(), 2);
}

#[test]
fn the_transaction_state_machine_rejects_misuse() {
    let mut engine = inlaysql_core::mem::engine().unwrap();

    assert!(matches!(engine.commit(), Err(Error::Transaction(_))));
    assert!(matches!(engine.rollback(), Err(Error::Transaction(_))));

    engine.begin().unwrap();
    assert!(matches!(engine.begin(), Err(Error::Transaction(_))));

    engine.rollback().unwrap();
    assert!(matches!(engine.rollback(), Err(Error::Transaction(_))));
}

#[test]
fn a_transaction_that_changed_nothing_advances_no_version() {
    let mut engine = inlaysql_core::mem::engine().unwrap();
    create_table(&mut engine);
    insert(&mut engine, 1);
    let before = engine.change_version();

    engine.begin().unwrap();
    engine.execute("DELETE FROM t WHERE id = 999", &[]).unwrap();
    engine.commit().unwrap();

    assert_eq!(engine.change_version(), before);
}

#[test]
fn a_transaction_of_many_statements_costs_one_sync() {
    let disk = Rc::new(RefCell::new(SimDisk::with_block_size(BLOCK, CAPACITY)));
    let mut engine = Engine::open(
        Box::new(TreeStorage::open_on(disk.clone()).unwrap()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .unwrap();
    create_table(&mut engine);
    let before = disk.borrow().sync_count();

    engine.begin().unwrap();
    for id in 1..=50 {
        insert(&mut engine, id);
    }
    engine.commit().unwrap();

    let after = disk.borrow().sync_count();
    assert_eq!(
        after - before,
        1,
        "fifty inserts inside one transaction must cost one fsync"
    );
}

#[test]
fn statements_without_a_transaction_sync_once_each() {
    let disk = Rc::new(RefCell::new(SimDisk::with_block_size(BLOCK, CAPACITY)));
    let mut engine = Engine::open(
        Box::new(TreeStorage::open_on(disk.clone()).unwrap()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .unwrap();
    create_table(&mut engine);
    let before = disk.borrow().sync_count();

    for id in 1..=50 {
        insert(&mut engine, id);
    }

    let after = disk.borrow().sync_count();
    assert_eq!(after - before, 50);
}

#[test]
fn a_rollback_leaves_the_durable_image_byte_identical() {
    let disk = Rc::new(RefCell::new(SimDisk::with_block_size(BLOCK, CAPACITY)));
    let mut engine = Engine::open(
        Box::new(TreeStorage::open_on(disk.clone()).unwrap()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .unwrap();
    create_table(&mut engine);
    insert(&mut engine, 1);

    let before = disk.borrow().durable().to_vec();
    engine.begin().unwrap();
    insert(&mut engine, 2);
    engine.rollback().unwrap();
    let after = disk.borrow().durable().to_vec();

    assert_eq!(before, after, "a rollback moved bytes on the device");
}

/// A transaction sees its own writes — on the copy-on-write tree, not just on
/// the in-memory backend.
///
/// This is the property `MemStorage` always had for free and the tree did not:
/// its reads used to start at the *committed* root, so a `BEGIN; INSERT;
/// SELECT` on a real database file returned nothing. Anything that builds
/// something inside a transaction by reading back what it just wrote — the
/// paged ANN index does exactly this — read a hole where its own record was.
#[test]
fn a_transaction_reads_its_own_writes_on_the_tree() {
    let disk = Rc::new(RefCell::new(SimDisk::with_block_size(BLOCK, CAPACITY)));
    let mut engine = Engine::open(
        Box::new(TreeStorage::open_on(disk.clone()).unwrap()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .unwrap();
    create_table(&mut engine);

    engine.begin().unwrap();
    insert(&mut engine, 1);
    insert(&mut engine, 2);

    let rows = engine.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(
        rows.rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            // Ordered by row id, and both rows are visible before the commit.
        ],
        "a scan inside the transaction did not see the transaction's rows"
    );

    // A point lookup takes a different path through the tree; it agrees.
    let rows = engine.query("SELECT id FROM t WHERE id = 2", &[]).unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Integer(2)]]);

    // A row deleted in the same transaction stops being visible.
    engine.execute("DELETE FROM t WHERE id = 1", &[]).unwrap();
    let rows = engine.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Integer(2)]]);

    // And a rollback takes back everything the transaction could see.
    engine.rollback().unwrap();
    let rows = engine.query("SELECT id FROM t", &[]).unwrap();
    assert!(rows.rows.is_empty(), "rollback left rows behind: {rows:?}");
}

#[test]
fn a_transaction_that_outgrows_the_log_is_rejected_clearly() {
    let disk = Rc::new(RefCell::new(SimDisk::with_block_size(BLOCK, CAPACITY)));
    let mut engine = Engine::open(
        Box::new(TreeStorage::open_on(disk.clone()).unwrap()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .unwrap();
    create_table(&mut engine);

    // A value large enough that a few hundred rows overflow the write-ahead log.
    let body = "x".repeat(2000);
    engine.begin().unwrap();
    let mut rejected = None;
    let mut accepted = 0;
    for id in 1..=2000 {
        match engine.execute(
            "INSERT INTO t (id, body) VALUES (?, ?)",
            &[Value::Integer(id), Value::Text(body.clone().into())],
        ) {
            Ok(_) => accepted += 1,
            Err(Error::Transaction(message)) => {
                rejected = Some(message);
                break;
            }
            Err(other) => panic!("expected a transaction error, got {other:?}"),
        }
    }
    let message = rejected.expect("a too-large transaction was not rejected");
    assert!(
        message.contains("write-ahead log"),
        "unclear error: {message}"
    );
    assert!(accepted > 0, "the transaction was rejected before any row");

    // The rows accepted so far commit fine, and the loop can continue in a
    // fresh transaction — the error is a "commit now" signal, not corruption.
    engine.commit().unwrap();
    engine.begin().unwrap();
    engine
        .execute(
            "INSERT INTO t (id, body) VALUES (?, ?)",
            &[Value::Integer(10_000), Value::Text(body.clone().into())],
        )
        .unwrap();
    engine.commit().unwrap();

    let rows = engine.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(rows.rows.len(), accepted + 1);
}

/// Rows one seed's transaction tries to insert.
const ROWS: u64 = 16;

/// Open an engine over a fault-injecting simulator.
fn open(sim: Rc<RefCell<Simulator>>) -> Result<Engine> {
    Engine::open(
        Box::new(TreeStorage::open_on(sim)?),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
}

/// Run one seed: one transaction of `ROWS` inserts under fault injection, then
/// reopen the surviving image and require either all of the transaction or none
/// of it — never a mix.
fn crash_sweep(seed: u64) {
    let sim = Rc::new(RefCell::new(Simulator::with_disk(
        seed,
        SimDisk::with_block_size(BLOCK, CAPACITY),
        // Crash and torn writes only, as in `dst_sweep`; reordered syncs are
        // excluded for the same documented reason (docs/recovery.md).
        FaultSchedule::random_with(seed, 10, 10, 0),
    )));

    let Ok(mut engine) = open(sim.clone()) else {
        return;
    };
    if sim.borrow().crashed() {
        return; // create's own sync faulted: no durable database yet.
    }
    if engine
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .is_err()
    {
        return;
    }
    if sim.borrow().crashed() {
        return; // the table's commit never reached the platter.
    }

    engine.begin().unwrap();
    for id in 1..=ROWS {
        if engine
            .execute(
                "INSERT INTO t (id, body) VALUES (?, ?)",
                &[
                    Value::Integer(id as i64),
                    Value::Text("x".to_string().into()),
                ],
            )
            .is_err()
        {
            break;
        }
    }
    if !sim.borrow().crashed() {
        let _ = engine.commit();
    }
    drop(engine);

    let image = sim.borrow().disk().durable().to_vec();
    let Ok(reopened) = TreeStorage::open_on(SimDisk::with_image(BLOCK, &image)) else {
        return; // a crash before the header became durable: no database to check.
    };
    let count = inlaysql_core::traits::scan_all(&reopened, "t")
        .unwrap()
        .len();
    assert!(
        count == 0 || count == ROWS as usize,
        "seed {seed}: recovered {count} rows, expected 0 or {ROWS}"
    );
}

#[test]
fn a_crash_mid_transaction_recovers_to_all_or_nothing() {
    for seed in 0..200u64 {
        crash_sweep(seed);
    }
}
