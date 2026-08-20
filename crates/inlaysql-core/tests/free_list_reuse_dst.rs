//! Deterministic simulation coverage for Phase 2 item 6's free list —
//! specifically for **page id reuse**, which `dst_sweep.rs` was never
//! written to exercise: every seed there runs with `CowBTree::page_reuse()`
//! off, so a page id is never handed out twice and the sweep cannot catch a
//! bug that only exists once reuse is turned on.
//!
//! # Why this needs its own device, not `dst_sweep.rs`'s `Simulator`
//!
//! `CowBTree`'s reclaim logic only offers a freed page id once the device
//! can *prove* two things: the freeing commit is durable
//! (`Device::commit_point`) and no reader this device can see is pinned to
//! an older root (`Device::min_reader_seq`). `Simulator`/`SimDisk`, as used
//! by every other DST test, never implement either — both stay at their
//! default `None`, which is deliberately "unknown, so never reclaim" (see
//! `CowBTree::refill_free_candidates`'s doc comment). Run this sweep's
//! workload over the ordinary `Simulator` and it would pass trivially,
//! having never actually recycled a page — the DST-shaped version of a test
//! with an assertion that never executes.
//!
//! [`TrustedDevice`] is the smallest wrapper that changes that, and it is
//! built to be trustworthy the same way `FileDevice` is, not by weakening
//! the model: `Device::sync` here reports the *real*
//! [`SyncOutcome`](inlaysql_core::sim::SyncOutcome) as an `Err` — matching
//! what a real file's `fsync` does on failure — rather than `Simulator`'s
//! own `Device` impl, which always returns `Ok` and leaves the workload to
//! notice a crash separately via `Simulator::crashed()`. That one change is
//! what makes `commit_point` safe to cache here: `CowBTree::checkpoint`
//! only reaches `set_commit_point` *after* `write_state`'s sync returns
//! `Ok`, so on this device — exactly as on a real file — a checkpoint whose
//! own sync silently failed can never go on to zero its log region and
//! still publish a commit point nobody can trust. Reusing the adversarial
//! `Simulator`/`SyncOutcome` fault draws instead of inventing a new fault
//! model means this sweep still shares the same crash/torn-write schedule
//! (`FaultSchedule::random_with`) the rest of the suite is measured against.
//!
//! The assertion is the same one every other sweep makes — the recovered
//! database is byte-for-byte one of the states the workload actually
//! committed — plus one more: across the sweep, reclamation must actually
//! have fired at least once (`CowBTree::pages_reused`), or this file would
//! be exercising nothing.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use inlaysql_core::btree::{CommitPoint, CowBTree, Device};
use inlaysql_core::error::{Error, Result};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::sim::{FaultSchedule, SimDisk, Simulator, SyncOutcome};
use inlaysql_core::traits::Rng;

const PAGE: usize = 256;
const BLOCK: usize = 512;
const CAPACITY: usize = 8 << 20;

/// Number of commit batches each seed's workload performs. Higher than
/// `dst_sweep.rs`'s `BATCHES` and over a narrow key space (see `sweep`) on
/// purpose: reclamation only has something to prove once the same handful
/// of pages have been superseded, checkpointed past and reused several
/// times over, not just once.
const BATCHES: usize = 200;

/// How many distinct keys the workload cycles through. Small relative to
/// `BATCHES` so the tree's own page set is superseded and freed repeatedly
/// rather than growing monotonically — heavy churn is what the free list
/// exists for.
const KEY_SPACE: u64 = 24;

/// A [`Device`] over one [`Simulator`], trustworthy for
/// [`Device::commit_point`], [`Device::commit_generation`] and the reader
/// watermark ([`Device::min_reader_seq`]) the same way `FileDevice` is —
/// see the module doc for what that requires and why. Single-writer, so the
/// reservation gate is a formality (`begin_commit`/`end_commit` never
/// contend), but the trust properties those two methods and the reader
/// registry provide are exactly what `CowBTree`'s free list needs to ever
/// draw a candidate at all.
struct TrustedDevice {
    sim: Simulator,
    generation: Cell<u64>,
    commit_point: Cell<Option<(u64, u64, u64)>>,
    append: Cell<[Option<usize>; 1]>,
    readers: std::cell::RefCell<HashMap<u64, u64>>,
    next_token: Cell<u64>,
}

impl TrustedDevice {
    fn new(sim: Simulator) -> Self {
        Self {
            sim,
            generation: Cell::new(0),
            commit_point: Cell::new(None),
            append: Cell::new([None; 1]),
            readers: std::cell::RefCell::new(HashMap::new()),
            next_token: Cell::new(1),
        }
    }

    fn crashed(&self) -> bool {
        self.sim.crashed()
    }

    fn image(&self) -> Vec<u8> {
        self.sim.disk().durable().to_vec()
    }
}

impl Device for TrustedDevice {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        self.sim.disk().read(offset, buf)
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        self.sim.disk_mut().write(offset, data)
    }

    /// The one change from `Simulator`'s own `Device` impl (see the module
    /// doc): a drawn crash or torn write is reported honestly as an `Err`,
    /// the same as a real `fsync` failure would be, instead of always
    /// returning `Ok` and leaving `Simulator::crashed()` as the only signal.
    /// Every caller in `CowBTree` already handles a failing `sync` — that is
    /// what a real `FileDevice` can do at any moment, and it is exactly the
    /// path that keeps `set_commit_point` from ever being reached with a
    /// value this device cannot back.
    fn sync(&mut self) -> Result<()> {
        match self.sim.sync() {
            SyncOutcome::Committed => Ok(()),
            SyncOutcome::Crashed => Err(Error::Storage(
                "simulated crash or torn write during sync".to_string(),
            )),
        }
    }

    fn begin_commit(&self) -> Result<()> {
        Ok(())
    }

    fn end_commit(&self) -> Option<u64> {
        let next = self.generation.get() + 1;
        self.generation.set(next);
        Some(next)
    }

    fn commit_generation(&self) -> Option<u64> {
        Some(self.generation.get())
    }

    fn commit_point(&self, region: usize) -> Option<CommitPoint> {
        let (root, next, seq) = self.commit_point.get()?;
        let append_offset = self.append.get()[region]?;
        Some(CommitPoint {
            root,
            next,
            seq,
            append_offset,
        })
    }

    fn set_commit_point(&self, region: usize, point: Option<CommitPoint>) {
        match point {
            Some(p) => {
                self.commit_point.set(Some((p.root, p.next, p.seq)));
                let mut append = self.append.get();
                append[region] = Some(p.append_offset);
                self.append.set(append);
            }
            None => {
                self.commit_point.set(None);
                self.append.set([None; 1]);
            }
        }
    }

    fn register_reader(&self) -> Option<u64> {
        let token = self.next_token.get();
        self.next_token.set(token + 1);
        self.readers.borrow_mut().insert(token, 0);
        Some(token)
    }

    fn update_reader(&self, token: u64, seq: u64) {
        self.readers.borrow_mut().insert(token, seq);
    }

    fn release_reader(&self, token: u64) {
        self.readers.borrow_mut().remove(&token);
    }

    fn min_reader_seq(&self) -> Option<u64> {
        self.readers.borrow().values().copied().min()
    }
}

/// Run one seed's churn workload with page reuse on, and assert the
/// recovered database — after whatever crash the fault schedule drew —
/// is a state the workload actually committed.
fn sweep(seed: u64) -> u64 {
    let sim = Simulator::with_disk(
        seed,
        SimDisk::with_block_size(BLOCK, CAPACITY),
        FaultSchedule::random_with(seed, 10, 10, 0),
    );
    let mut db = match CowBTree::create(TrustedDevice::new(sim), PAGE) {
        Ok(db) => db,
        Err(_) => return 0,
    };
    if db.device().crashed() {
        return 0;
    }
    db.set_page_reuse(true);

    let mut rng = SeededRng::new(seed ^ 0xA5A5_5A5A_1234_9E37);
    let mut snapshots: Vec<BTreeMap<Vec<u8>, Vec<u8>>> = vec![BTreeMap::new()];
    let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    'workload: for batch in 0..BATCHES {
        let ops = 1 + (rng.next_u64() % 6) as usize;
        for _ in 0..ops {
            let key = format!("k{:04}", rng.next_u64() % KEY_SPACE).into_bytes();
            if rng.next_u64().is_multiple_of(3) {
                expected.remove(&key);
                db.delete(&key).unwrap();
            } else {
                let value = format!("v{:016x}-{batch}", rng.next_u64()).into_bytes();
                expected.insert(key.clone(), value.clone());
                db.put(&key, &value).unwrap();
            }
        }
        let commit_result = db.commit();
        // Record this commit's intended state *before* checking for a fault,
        // exactly as `dst_sweep.rs` does and for the same reason: a torn
        // write can leave the whole record durable — `TrustedDevice::sync`
        // reports that as an `Err` (see its doc comment on why that is the
        // honest, real-file-shaped answer), but "the caller was told sync
        // might have failed" and "the bytes did not reach the platter" are
        // different facts. The commit may have survived either way, so its
        // state belongs in the set of recoverable snapshots regardless of
        // `commit_result`.
        snapshots.push(expected.clone());
        if commit_result.is_err() || db.device().crashed() {
            break 'workload;
        }
        if rng.next_u64().is_multiple_of(6) {
            let checkpoint_result = db.checkpoint();
            if checkpoint_result.is_err() || db.device().crashed() {
                break 'workload;
            }
        }
    }

    let reused = db.pages_reused();
    let image = db.device().image();
    drop(db);

    let reopened = match CowBTree::open(SimDisk::with_image(BLOCK, &image)) {
        Ok(db) => db,
        Err(err) => panic!("seed {seed}: recovery failed: {err}"),
    };
    let recovered: BTreeMap<Vec<u8>, Vec<u8>> = reopened
        .scan()
        .unwrap_or_else(|err| panic!("seed {seed}: scan of recovered tree failed: {err}"))
        .into_iter()
        // `scan()` is a raw, prefix-less walk of the whole tree, so it also
        // returns the free list's own bookkeeping rows (`FREE_LIST_PREFIX`
        // in `tree.rs`, mirrored here since it is private) — rows no SQL-level
        // table scan would ever see, since those are always prefix-scoped to
        // one table. `expected`/`snapshots` are built purely from this
        // workload's own keys, so they must be excluded here for the
        // comparison to mean anything, exactly as a real caller would never
        // see them mixed into a table's results.
        .filter(|(key, _)| !key.starts_with(b"\x02free\0"))
        .map(|(key, value)| (key, value.into_vec()))
        .collect();
    assert!(
        snapshots.contains(&recovered),
        "seed {seed}: recovered state is not any committed snapshot"
    );
    reused
}

#[test]
fn heavy_churn_with_reuse_on_recovers_to_a_committed_snapshot() {
    let mut total_reused = 0u64;
    for seed in 0..300u64 {
        total_reused += sweep(seed);
    }
    // If this is ever 0, the sweep above is exercising nothing but the
    // ordinary (no-reuse) path with extra ceremony — see the module doc.
    // 300 seeds of heavy churn over 24 keys gives reclamation many chances
    // to fire; a regression that silently stops it (e.g. a durability or
    // liveness check that is always too conservative) would show up here
    // as this dropping to zero without any test going red on its own.
    assert!(
        total_reused > 0,
        "no seed ever reused a page — this sweep is not testing reuse"
    );
}

#[test]
#[ignore = "expensive: run with --release -- --ignored, or in CI"]
fn thousands_of_seeds_of_heavy_churn_with_reuse_on_recover_to_a_committed_snapshot() {
    let mut total_reused = 0u64;
    for seed in 0..5_000u64 {
        total_reused += sweep(seed);
    }
    assert!(
        total_reused > 0,
        "no seed ever reused a page — this sweep is not testing reuse"
    );
}
