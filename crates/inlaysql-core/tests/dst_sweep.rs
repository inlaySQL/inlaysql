//! A seed-driven sweep: thousands of randomized workloads and fault schedules,
//! each replayed to prove the storage engine recovers to a consistent state.
//!
//! This is the credibility centrepiece of the deterministic simulation testing
//! strategy. For each seed a workload writes, deletes, commits and checkpoints
//! against a fault-injecting simulator, then the surviving durable image is
//! reopened. The assertion is not that "everything we wrote is there" — crashes
//! are *supposed* to lose the last commit. The assertion is **atomicity**: the
//! recovered database must be byte-for-byte one of the states the workload
//! actually committed, never a mix of two commits and never a torn page.
//!
//! The fault schedule injects crashes and torn writes. A reordered sync is
//! deliberately not injected here: it interacts with log truncation at a
//! checkpoint in a way that is documented in `docs/recovery.md` as a hardening
//! follow-up (the engine detects the inconsistency rather than silently
//! corrupting).
//!
//! Every decision — the workload's operations and the fault schedule — is a
//! pure function of the seed, so a failing seed reproduces exactly on any
//! machine: `cargo test --test dst_sweep -- <seed>`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use inlaysql_core::btree::{CommitOutcome, CowBTree, Device};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::sim::{FaultSchedule, SimDisk, Simulator};
use inlaysql_core::traits::Rng;
use inlaysql_core::Result;

const PAGE: usize = 256;
const BLOCK: usize = 512;
const CAPACITY: usize = 8 << 20;

/// Number of commit batches each seed's workload performs.
const BATCHES: usize = 64;

/// Run one seed and assert the recovered state is a committed snapshot.
fn sweep(seed: u64) {
    // Crash and torn-write faults only, 1% each per sync.
    let sim = Simulator::with_disk(
        seed,
        SimDisk::with_block_size(BLOCK, CAPACITY),
        FaultSchedule::random_with(seed, 10, 10, 0),
    );
    let mut db = CowBTree::create(sim, PAGE).unwrap();

    // If create's own sync faulted, the header never became durable and the
    // database simply does not exist yet — nothing to recover.
    let durable = db.device().disk().durable();
    if durable.len() < 8 || &durable[..8] != b"INLAYSQL" {
        return;
    }

    let mut rng = SeededRng::new(seed ^ 0x9E37_79B9_7F4A_7C15);
    let mut snapshots: Vec<BTreeMap<Vec<u8>, Vec<u8>>> = vec![BTreeMap::new()];
    let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    'workload: for _ in 0..BATCHES {
        let ops = (rng.next_u64() % 8) as usize;
        for _ in 0..ops {
            let key = format!("k{:016x}", rng.next_u64()).into_bytes();
            if rng.next_u64().is_multiple_of(4) {
                expected.remove(&key);
                db.delete(&key).unwrap();
            } else {
                let value = format!("v{:016x}", rng.next_u64()).into_bytes();
                expected.insert(key.clone(), value.clone());
                db.put(&key, &value).unwrap();
            }
        }
        db.commit().unwrap();
        // Record this commit's intended state *before* checking for a fault: a
        // torn write can leave the whole record durable (the commit survived),
        // so it belongs in the set of recoverable snapshots.
        snapshots.push(expected.clone());
        if db.device().crashed() {
            break 'workload;
        }
        if rng.next_u64().is_multiple_of(8) {
            db.checkpoint().unwrap();
            if db.device().crashed() {
                break 'workload;
            }
        }
    }

    let image = db.device().disk().durable().to_vec();
    let reopened = match CowBTree::open(SimDisk::with_image(BLOCK, &image)) {
        Ok(db) => db,
        Err(err) => panic!("seed {seed}: recovery failed: {err}"),
    };
    let recovered: BTreeMap<Vec<u8>, Vec<u8>> = reopened
        .scan()
        .unwrap_or_else(|err| panic!("seed {seed}: scan of recovered tree failed: {err}"))
        .into_iter()
        .map(|(key, value)| (key, value.into_vec()))
        .collect();
    assert!(
        snapshots.contains(&recovered),
        "seed {seed}: recovered state is not any committed snapshot"
    );
}

#[test]
fn hundreds_of_seeds_recover_to_a_committed_snapshot() {
    for seed in 0..500u64 {
        sweep(seed);
    }
}

/// Upper bound on the value length in [`sweep_large`]. Large enough to spill
/// across several overflow pages at `PAGE = 256`, small enough that a commit
/// record still fits the write-ahead-log region.
const LARGE_VALUE_MAX: usize = PAGE * 4;

/// The [`sweep`] workload, but writing multi-page values that spill to overflow
/// chains. A crash mid-overflow-write must recover to a committed snapshot — a
/// row is never half-written, however many pages it spans.
fn sweep_large(seed: u64) {
    let sim = Simulator::with_disk(
        seed,
        SimDisk::with_block_size(BLOCK, CAPACITY),
        FaultSchedule::random_with(seed, 10, 10, 0),
    );
    let mut db = CowBTree::create(sim, PAGE).unwrap();

    let durable = db.device().disk().durable();
    if durable.len() < 8 || &durable[..8] != b"INLAYSQL" {
        return;
    }

    let mut rng = SeededRng::new(seed ^ 0x6A09_E667_F3BC_C909);
    let mut snapshots: Vec<BTreeMap<Vec<u8>, Vec<u8>>> = vec![BTreeMap::new()];
    let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    'workload: for _ in 0..BATCHES {
        let ops = (rng.next_u64() % 4) as usize;
        for _ in 0..ops {
            let key = format!("k{:016x}", rng.next_u64()).into_bytes();
            if rng.next_u64().is_multiple_of(4) {
                expected.remove(&key);
                db.delete(&key).unwrap();
            } else {
                let len = 1 + (rng.next_u64() as usize) % LARGE_VALUE_MAX;
                let value: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xff) as u8).collect();
                expected.insert(key.clone(), value.clone());
                if let Err(err) = db.put(&key, &value) {
                    panic!("seed {seed}: put of {} bytes failed: {err}", value.len());
                }
            }
        }
        if let Err(err) = db.commit() {
            panic!("seed {seed}: commit failed: {err}");
        }
        snapshots.push(expected.clone());
        if db.device().crashed() {
            break 'workload;
        }
        if rng.next_u64().is_multiple_of(8) {
            db.checkpoint().unwrap();
            if db.device().crashed() {
                break 'workload;
            }
        }
    }

    let image = db.device().disk().durable().to_vec();
    let reopened = match CowBTree::open(SimDisk::with_image(BLOCK, &image)) {
        Ok(db) => db,
        Err(err) => panic!("seed {seed}: recovery failed: {err}"),
    };
    let recovered: BTreeMap<Vec<u8>, Vec<u8>> = reopened
        .scan()
        .unwrap_or_else(|err| panic!("seed {seed}: scan of recovered tree failed: {err}"))
        .into_iter()
        .map(|(key, value)| (key, value.into_vec()))
        .collect();
    assert!(
        snapshots.contains(&recovered),
        "seed {seed}: recovered state is not any committed snapshot"
    );
}

#[test]
fn large_values_recover_to_a_committed_snapshot() {
    for seed in 0..500u64 {
        sweep_large(seed);
    }
}

#[test]
#[ignore = "expensive: run with --release -- --ignored, or in CI"]
fn thousands_of_seeds_recover_to_a_committed_snapshot() {
    for seed in 0..10_000u64 {
        sweep(seed);
    }
}

/// One view of a shared simulated disk, assigned to a particular WAL region.
#[derive(Clone)]
struct RegionalDevice<D> {
    shared: Rc<RefCell<D>>,
    region: usize,
}

impl<D: Device> Device for RegionalDevice<D> {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        self.shared.borrow().read(offset, buf)
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        self.shared.borrow_mut().write(offset, data)
    }

    fn sync(&mut self) -> Result<()> {
        self.shared.borrow_mut().sync()
    }

    fn wal_region(&self) -> usize {
        self.region
    }
}

/// Exercise recovery over records interleaved across every v5 WAL region.
/// The workload deliberately reuses keys so it contains both clean rebases and
/// first-committer-wins conflicts.
fn sweep_multi_writer(seed: u64) {
    let simulator = Simulator::with_disk(
        seed,
        SimDisk::with_block_size(BLOCK, CAPACITY),
        FaultSchedule::random_with(seed, 10, 10, 0),
    );
    let shared = Rc::new(RefCell::new(simulator));
    let device = |region| RegionalDevice {
        shared: shared.clone(),
        region,
    };
    let first = CowBTree::create(device(0), PAGE).unwrap();
    if shared.borrow().disk().durable().get(..8) != Some(&b"INLAYSQL"[..]) {
        return;
    }
    let mut writers = vec![first];
    for region in 1..inlaysql_core::wal::WAL_REGIONS {
        writers.push(CowBTree::open(device(region)).unwrap());
    }

    let mut rng = SeededRng::new(seed ^ 0xD1B5_4A32_D192_ED03);
    let mut expected = BTreeMap::new();
    let mut snapshots = vec![expected.clone()];
    for _ in 0..BATCHES {
        let writer_index = rng.next_u64() as usize % writers.len();
        let key = format!("shared-{}", rng.next_u64() % 16).into_bytes();
        let delete = rng.next_u64().is_multiple_of(5);
        let value = format!("v{:016x}", rng.next_u64()).into_bytes();
        let writer = &mut writers[writer_index];
        if delete {
            writer.delete(&key).unwrap();
        } else {
            writer.put(&key, &value).unwrap();
        }
        match writer.commit().unwrap() {
            CommitOutcome::Committed => {
                if delete {
                    expected.remove(&key);
                } else {
                    expected.insert(key, value);
                }
                snapshots.push(expected.clone());
            }
            CommitOutcome::Conflict => {}
        }
        if shared.borrow().crashed() {
            break;
        }
    }

    let image = shared.borrow().disk().durable().to_vec();
    drop(writers);
    let reopened = CowBTree::open(SimDisk::with_image(BLOCK, &image))
        .unwrap_or_else(|err| panic!("multi-writer seed {seed}: recovery failed: {err}"));
    let recovered: BTreeMap<Vec<u8>, Vec<u8>> = reopened
        .scan()
        .unwrap_or_else(|err| panic!("multi-writer seed {seed}: scan failed: {err}"))
        .into_iter()
        .map(|(key, value)| (key, value.into_vec()))
        .collect();
    assert!(
        snapshots.contains(&recovered),
        "multi-writer seed {seed}: recovered state is not a committed interleaving"
    );
}

#[test]
fn multi_writer_regions_recover_to_a_committed_interleaving() {
    for seed in 0..500u64 {
        sweep_multi_writer(seed);
    }
}
