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

use inlaysql_core::btree::{
    AbsorbDecision, AbsorbOutcome, AbsorbQueue, AbsorbSeal, AbsorbTxn, CommitOutcome, CowBTree,
    Device, PageId, PendingOps,
};
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

// ---------------------------------------------------------------------------
// Commit-side absorption (AHL-544, `docs/research/commit-group-slice1.md`)
//
// Absorption moves the first-committer-wins *decision* to whichever writer
// holds the commit reservation gate: it judges every transaction parked
// behind it, in gate-arrival order, each against the previous member's
// logical post-rebase state. Every writer still rebases, encodes, appends
// into its own region, publishes its own ticket and runs its own sync.
//
// The claim that has to be checked rather than asserted is that this changes
// nothing: the same transactions, offered in the same order, must produce the
// same outcomes and the same bytes whether the gate holder judged them or
// each writer judged itself. That is `absorption_matches_serial_commit_order`
// below; everything else here is a named way for that equality to break.
//
// A parked writer is a *thread* blocked on the gate, and this harness has no
// threads — `inlaysql-core` is `no_std` and the simulator is single-threaded
// by construction. `CowBTree::park_for_absorption` is what stands in for the
// park: a follower offers its transaction, the leader commits (and judges the
// offers it finds), then each follower commits in turn. The observable
// sequence at the gate is identical to the threaded one, and it is
// deterministic, which the threaded one is not.
// ---------------------------------------------------------------------------

/// The absorption bookkeeping shared by every writer on one simulated file.
/// The same [`AbsorbQueue`] `inlaysql`'s `FileDevice` holds behind a `Mutex`,
/// here behind a `RefCell` — the chain arithmetic under test is the
/// production one, not a second copy of it.
type Gate = Rc<RefCell<AbsorbQueue>>;

/// A [`RegionalDevice`] that also absorbs: one WAL region of a shared
/// simulated disk, plus a share of one absorption queue.
#[derive(Clone)]
struct AbsorbingDevice<D> {
    shared: Rc<RefCell<D>>,
    region: usize,
    gate: Gate,
}

impl<D: Device> Device for AbsorbingDevice<D> {
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

    fn set_commit_absorption(&self, enabled: bool) {
        if enabled {
            self.gate.borrow_mut().enabled = true;
        }
    }

    fn absorb_offer(&self, root: PageId, ops: &mut PendingOps) -> Option<u64> {
        self.gate.borrow_mut().offer(root, ops)
    }

    fn absorb_claim(&self, token: u64, ops: &mut PendingOps) -> Option<AbsorbDecision> {
        self.gate.borrow_mut().claim(token, ops)
    }

    fn absorb_cohort(&self, seq: u64, decide: &mut dyn FnMut(&[AbsorbTxn]) -> Vec<AbsorbOutcome>) {
        // `decide` reads the tree, which borrows `shared`, never `gate`.
        self.gate.borrow_mut().cohort(seq, decide);
    }

    fn absorption_seal(&self) -> Option<AbsorbSeal> {
        self.gate.borrow().seal()
    }

    fn set_absorption_seal(&self, seal: Option<AbsorbSeal>) {
        self.gate.borrow_mut().set_seal(seal);
    }
}

/// One logical transaction in an absorption scenario: which writer runs it,
/// and what it does. Deliberately one key per transaction — the property
/// under test is *which* transactions conflict, and one key is enough to
/// make any of them.
#[derive(Clone, Debug)]
struct Txn {
    writer: usize,
    key: Vec<u8>,
    value: Option<Vec<u8>>,
}

/// A group of transactions that reach the gate together: the first is the
/// leader, the rest park behind it. Every member's transaction is built
/// *before* the leader commits, which is what makes them able to conflict
/// with it at all.
type Cohort = Vec<Txn>;

/// A file of `writers` handles over one simulated disk, all absorbing or all
/// not. Returns the handles and the shared disk and queue.
#[allow(clippy::type_complexity)]
fn absorbing_writers(
    writers: usize,
    schedule: FaultSchedule,
    absorption: bool,
) -> Option<(
    Vec<CowBTree<AbsorbingDevice<Simulator>>>,
    Rc<RefCell<Simulator>>,
    Gate,
)> {
    let simulator = Simulator::with_disk(0, SimDisk::with_block_size(BLOCK, CAPACITY), schedule);
    let shared = Rc::new(RefCell::new(simulator));
    let gate: Gate = Rc::new(RefCell::new(AbsorbQueue::default()));
    let device = |region| AbsorbingDevice {
        shared: shared.clone(),
        region,
        gate: gate.clone(),
    };
    let mut first = CowBTree::create(device(0), PAGE).ok()?;
    if shared.borrow().disk().durable().get(..8) != Some(&b"INLAYSQL"[..]) {
        return None;
    }
    first.set_commit_absorption(absorption);
    let mut handles = vec![first];
    for region in 1..inlaysql_core::wal::WAL_REGIONS {
        if handles.len() >= writers {
            break;
        }
        let mut handle = CowBTree::open(device(region)).ok()?;
        handle.set_commit_absorption(absorption);
        handles.push(handle);
    }
    Some((handles, shared, gate))
}

/// Apply one cohort exactly as the gate would see it: every member buffers
/// its transaction, every follower parks, the leader commits (judging the
/// parked followers if absorption is on), then each follower commits in
/// arrival order. Returns one outcome per member, in that order.
fn run_cohort(
    writers: &mut [CowBTree<AbsorbingDevice<Simulator>>],
    cohort: &Cohort,
    absorption: bool,
    shared: &Rc<RefCell<Simulator>>,
) -> Vec<CommitOutcome> {
    for txn in cohort {
        let writer = &mut writers[txn.writer];
        match &txn.value {
            Some(value) => writer.put(&txn.key, value).unwrap(),
            None => writer.delete(&txn.key).unwrap(),
        }
    }
    if absorption {
        for txn in &cohort[1..] {
            writers[txn.writer].park_for_absorption();
        }
    }
    // A crash rolls the readable image back to the durable one, so anything
    // committed after it is written on top of a file that no longer exists.
    // Stop at the first one, exactly as every other sweep here does.
    let mut outcomes = Vec::new();
    for txn in cohort {
        outcomes.push(writers[txn.writer].commit().unwrap());
        if shared.borrow().crashed() {
            break;
        }
    }
    outcomes
}

/// Replay `cohorts` against a fresh file and report every member's outcome
/// plus the final committed contents.
#[allow(clippy::type_complexity)]
fn replay(
    cohorts: &[Cohort],
    writers: usize,
    absorption: bool,
) -> Option<(Vec<Vec<CommitOutcome>>, BTreeMap<Vec<u8>, Vec<u8>>, u64)> {
    let (mut handles, shared, gate) =
        absorbing_writers(writers, FaultSchedule::script(&[]), absorption)?;
    let outcomes = cohorts
        .iter()
        .map(|cohort| run_cohort(&mut handles, cohort, absorption, &shared))
        .collect();
    handles[0].refresh().unwrap();
    let contents = handles[0]
        .scan()
        .unwrap()
        .into_iter()
        .map(|(key, value)| (key, value.into_vec()))
        .collect();
    let members = gate.borrow().members;
    Some((outcomes, contents, members))
}

/// A seeded workload of cohorts over `writers` handles, reusing a small key
/// space so both clean rebases and first-committer-wins conflicts happen.
fn cohort_workload(seed: u64, writers: usize, cohorts: usize) -> Vec<Cohort> {
    let mut rng = SeededRng::new(seed ^ 0x51E1_3F27_9AC4_0B11);
    (0..cohorts)
        .map(|_| {
            // At least two members, or there is no cohort to absorb.
            let size = 2 + (rng.next_u64() as usize) % (writers - 1).max(1);
            let mut used = Vec::new();
            let mut cohort = Vec::new();
            for _ in 0..size {
                let writer = (rng.next_u64() as usize) % writers;
                if used.contains(&writer) {
                    continue;
                }
                used.push(writer);
                let key = format!("shared-{}", rng.next_u64() % 6).into_bytes();
                let value = (!rng.next_u64().is_multiple_of(5))
                    .then(|| format!("v{:016x}", rng.next_u64()).into_bytes());
                cohort.push(Txn { writer, key, value });
            }
            cohort
        })
        .filter(|cohort: &Cohort| cohort.len() > 1)
        .collect()
}

/// **The parity test.** The same transactions, offered to the gate in the
/// same order, must commit to the same bytes and report the same per-
/// transaction outcomes whether the gate holder judged them or each writer
/// judged itself.
///
/// This is the test the brief (`docs/research/commit-group-logical.md`, §4)
/// calls the most load-bearing one in the design, because "conflict
/// semantics are unchanged" is the claim the whole slice rests on. It is a
/// checked property here rather than an argument in a doc comment.
#[test]
fn absorption_matches_serial_commit_order() {
    let mut absorbed_total = 0;
    for seed in 0..200u64 {
        let writers = 2 + (seed as usize % 3);
        let cohorts = cohort_workload(seed, writers, 12);
        if cohorts.is_empty() {
            continue;
        }
        let Some((serial_outcomes, serial_state, serial_absorbed)) =
            replay(&cohorts, writers, false)
        else {
            continue;
        };
        let Some((absorbed_outcomes, absorbed_state, absorbed)) = replay(&cohorts, writers, true)
        else {
            continue;
        };
        assert_eq!(
            serial_absorbed, 0,
            "seed {seed}: absorption ran with the flag off"
        );
        assert_eq!(
            serial_outcomes, absorbed_outcomes,
            "seed {seed}: a transaction's outcome changed under absorption"
        );
        assert_eq!(
            serial_state, absorbed_state,
            "seed {seed}: the committed state differs under absorption"
        );
        absorbed_total += absorbed;
    }
    // The equality above proves nothing if no cohort ever formed — the same
    // discipline `free_list_reuse_dst` uses for `pages_reused`.
    assert!(
        absorbed_total > 100,
        "cohorts barely formed ({absorbed_total} members judged); the parity assertion is vacuous"
    );
}

/// A follower whose rows overlap an earlier member of the same cohort gets
/// `Conflict`, exactly as it would if it had re-entered the gate itself —
/// including when the earlier member is another *follower*, which is the one
/// case the leader can only answer from the logical overlay, because that
/// member's post-rebase root does not exist yet.
#[test]
fn a_follower_conflicts_with_an_earlier_member_of_its_own_cohort() {
    for absorption in [false, true] {
        let (mut writers, _shared, gate) =
            absorbing_writers(4, FaultSchedule::script(&[]), absorption).unwrap();
        writers[0].put(b"x", b"1").unwrap();
        writers[0].put(b"y", b"1").unwrap();
        writers[0].commit().unwrap();
        for writer in writers.iter_mut().skip(1) {
            writer.refresh().unwrap();
        }

        let cohort = vec![
            // The leader takes `x`.
            Txn {
                writer: 0,
                key: b"x".to_vec(),
                value: Some(b"2".to_vec()),
            },
            // Overlaps the leader: conflicts against a root that exists.
            Txn {
                writer: 1,
                key: b"x".to_vec(),
                value: Some(b"3".to_vec()),
            },
            // Disjoint: clean, and it advances the chain for the members
            // after it.
            Txn {
                writer: 2,
                key: b"y".to_vec(),
                value: Some(b"4".to_vec()),
            },
            // Overlaps member 2, whose post-rebase root does not exist when
            // the leader judges this. Only the overlay can answer it.
            Txn {
                writer: 3,
                key: b"y".to_vec(),
                value: Some(b"5".to_vec()),
            },
        ];
        let outcomes = run_cohort(&mut writers, &cohort, absorption, &_shared);
        assert_eq!(
            outcomes,
            vec![
                CommitOutcome::Committed,
                CommitOutcome::Conflict,
                CommitOutcome::Committed,
                CommitOutcome::Conflict,
            ],
            "absorption = {absorption}"
        );
        writers[0].refresh().unwrap();
        let state: BTreeMap<Vec<u8>, Vec<u8>> = writers[0]
            .scan()
            .unwrap()
            .into_iter()
            .map(|(key, value)| (key, value.into_vec()))
            .collect();
        assert_eq!(
            state.get(b"x".as_slice()).map(Vec::as_slice),
            Some(&b"2"[..])
        );
        assert_eq!(
            state.get(b"y".as_slice()).map(Vec::as_slice),
            Some(&b"4"[..])
        );
        if absorption {
            assert_eq!(
                gate.borrow().members,
                3,
                "the whole cohort must be judged, including the members after the conflict"
            );
            // A conflicting member resolves its chain position without moving
            // the file's sequence number. If it moved it, member 3's seal
            // would not match and it would silently fall back — which would
            // still be correct, and would still hide the bug.
            assert_eq!(
                writers[2].absorbed_commits(),
                1,
                "the member after a conflicting one must still be able to use its decision"
            );
        }
    }
}

/// A decision is used only when the file's committed state is the one it was
/// computed against. An outsider committing between the leader and the
/// follower makes it something else, and the follower must fall back to the
/// full comparison rather than trust an answer to the wrong question.
#[test]
fn an_outsider_commit_invalidates_a_pending_decision() {
    let (mut writers, _shared, _gate) =
        absorbing_writers(4, FaultSchedule::script(&[]), true).unwrap();
    writers[0].put(b"x", b"1").unwrap();
    writers[0].commit().unwrap();
    for writer in writers.iter_mut().skip(1) {
        writer.refresh().unwrap();
    }

    // Member 1 parks behind the leader, and would be told `Clean`.
    writers[1].put(b"y", b"9").unwrap();
    writers[1].park_for_absorption();
    writers[0].put(b"z", b"9").unwrap();
    assert_eq!(writers[0].commit().unwrap(), CommitOutcome::Committed);

    // An outsider takes the gate first and writes the very key member 1 is
    // about to write. Its decision is now an answer about a state the file
    // is no longer in.
    writers[2].put(b"y", b"8").unwrap();
    assert_eq!(writers[2].commit().unwrap(), CommitOutcome::Committed);

    assert_eq!(writers[1].commit().unwrap(), CommitOutcome::Conflict);
    assert_eq!(
        writers[1].absorbed_commits(),
        0,
        "a stale decision must not be used"
    );
}

/// The chain itself, asserted value by value: what a leader publishes, and
/// how a committing member and a conflicting member each move it on.
///
/// The three seal fields are jointly a chain *identity* — "the file is at
/// cohort `C`, position `j`, sequence `s`" — and every one of them is
/// individually redundant while the "anything the leader did not predict
/// publishes `None`" rule holds everywhere. That rule is what
/// [`an_outsider_commit_invalidates_a_pending_decision`] pins; this pins the
/// arithmetic, which is the part that is easy to get subtly wrong: a
/// conflicting member advances the position and *not* the sequence number,
/// because it changed nothing on the file, and getting that backwards would
/// make every member after a conflict silently fall back — still correct,
/// still hiding the bug.
#[test]
fn the_absorption_chain_advances_one_position_per_member_and_one_sequence_per_commit() {
    let (mut writers, shared, gate) =
        absorbing_writers(4, FaultSchedule::script(&[]), true).unwrap();
    writers[0].put(b"x", b"1").unwrap();
    writers[0].commit().unwrap();
    for writer in writers.iter_mut().skip(1) {
        writer.refresh().unwrap();
    }

    let cohort = vec![
        Txn {
            writer: 0,
            key: b"x".to_vec(),
            value: Some(b"2".to_vec()),
        },
        // Conflicts with the leader.
        Txn {
            writer: 1,
            key: b"x".to_vec(),
            value: Some(b"3".to_vec()),
        },
        // Disjoint, so it commits.
        Txn {
            writer: 2,
            key: b"y".to_vec(),
            value: Some(b"4".to_vec()),
        },
    ];
    for txn in &cohort {
        let writer = &mut writers[txn.writer];
        writer.put(&txn.key, txn.value.as_deref().unwrap()).unwrap();
    }
    writers[1].park_for_absorption();
    writers[2].park_for_absorption();

    assert_eq!(writers[0].commit().unwrap(), CommitOutcome::Committed);
    let leader = gate
        .borrow()
        .seal()
        .expect("a leader with a cohort seals it");
    assert_eq!(
        (leader.index, leader.seq),
        (1, 2),
        "leader's own commit is seq 2"
    );

    assert_eq!(writers[1].commit().unwrap(), CommitOutcome::Conflict);
    let after_conflict = gate
        .borrow()
        .seal()
        .expect("a conflict resolves its position");
    assert_eq!(
        (
            after_conflict.cohort,
            after_conflict.index,
            after_conflict.seq
        ),
        (leader.cohort, 2, 2),
        "a conflict advances the position and leaves the file's sequence alone"
    );

    assert_eq!(writers[2].commit().unwrap(), CommitOutcome::Committed);
    assert_eq!(writers[2].absorbed_commits(), 1);
    let after_commit = gate
        .borrow()
        .seal()
        .expect("a committing member seals its successor");
    assert_eq!(
        (after_commit.cohort, after_commit.index, after_commit.seq),
        (leader.cohort, 3, 3),
        "a commit advances both"
    );

    // And a writer that was never in the cohort clears the chain outright.
    writers[3].refresh().unwrap();
    writers[3].put(b"z", b"5").unwrap();
    assert_eq!(writers[3].commit().unwrap(), CommitOutcome::Committed);
    assert_eq!(
        gate.borrow().seal(),
        None,
        "a commit nobody predicted must leave no chain for a stale decision to match"
    );
    assert!(!shared.borrow().crashed());
}

/// Crash at every sync of a cohort's run — the leader before its own sync,
/// each follower before its own — and check the two things the brief names:
/// recovery lands on a state the workload actually committed, and **no
/// member's rows appear unless that member's own `commit()` returned
/// `Committed`**. A leader that judged four followers and then died has
/// still written nothing on any of their behalf.
#[test]
fn a_crash_at_every_step_of_a_cohort_never_publishes_an_unacknowledged_member() {
    for crash_at in 0..12usize {
        let mut script = vec![inlaysql_core::sim::Fault::None; crash_at];
        script.push(inlaysql_core::sim::Fault::Crash);
        let Some((mut writers, shared, _gate)) =
            absorbing_writers(4, FaultSchedule::script(&script), true)
        else {
            continue;
        };
        let cohorts = cohort_workload(crash_at as u64, 4, 4);
        let mut committed: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut snapshots = vec![committed.clone()];
        let mut acknowledged: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
        'cohorts: for cohort in &cohorts {
            let outcomes = run_cohort(&mut writers, cohort, true, &shared);
            for (txn, outcome) in cohort.iter().zip(&outcomes) {
                if *outcome == CommitOutcome::Committed {
                    *acknowledged.entry(txn.key.clone()).or_default() += 1;
                    match &txn.value {
                        Some(value) => committed.insert(txn.key.clone(), value.clone()),
                        None => committed.remove(&txn.key),
                    };
                    snapshots.push(committed.clone());
                }
            }
            if shared.borrow().crashed() {
                break 'cohorts;
            }
        }

        let image = shared.borrow().disk().durable().to_vec();
        drop(writers);
        let reopened = CowBTree::open(SimDisk::with_image(BLOCK, &image))
            .unwrap_or_else(|err| panic!("crash_at {crash_at}: recovery failed: {err}"));
        let recovered: BTreeMap<Vec<u8>, Vec<u8>> = reopened
            .scan()
            .unwrap_or_else(|err| panic!("crash_at {crash_at}: scan failed: {err}"))
            .into_iter()
            .map(|(key, value)| (key, value.into_vec()))
            .collect();
        assert!(
            snapshots.contains(&recovered),
            "crash_at {crash_at}: recovered state is not a committed interleaving"
        );
        for key in recovered.keys() {
            assert!(
                acknowledged.contains_key(key),
                "crash_at {crash_at}: recovered a row for {key:?}, which no member was ever told it had committed"
            );
        }
    }
}

/// The multi-writer recovery sweep, with cohorts. One seed in three drives
/// the workload through parked cohorts instead of one writer at a time, so
/// the recovery chain a cohort produces is swept under the same fault
/// schedule as every other chain — which, since absorption changes no
/// record, no region and no wrap, it should be indistinguishable from.
fn sweep_multi_writer_absorbed(seed: u64) -> u64 {
    let Some((mut writers, shared, gate)) = absorbing_writers(
        inlaysql_core::wal::WAL_REGIONS,
        FaultSchedule::random_with(seed, 10, 10, 0),
        true,
    ) else {
        return 0;
    };
    let cohorts = cohort_workload(seed, writers.len(), BATCHES / 2);
    let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut snapshots = vec![expected.clone()];
    'cohorts: for cohort in &cohorts {
        let outcomes = run_cohort(&mut writers, cohort, true, &shared);
        for (txn, outcome) in cohort.iter().zip(&outcomes) {
            if *outcome == CommitOutcome::Committed {
                match &txn.value {
                    Some(value) => expected.insert(txn.key.clone(), value.clone()),
                    None => expected.remove(&txn.key),
                };
                snapshots.push(expected.clone());
            }
        }
        if shared.borrow().crashed() {
            break 'cohorts;
        }
    }
    let image = shared.borrow().disk().durable().to_vec();
    let members = gate.borrow().members;
    drop(writers);
    let reopened = CowBTree::open(SimDisk::with_image(BLOCK, &image))
        .unwrap_or_else(|err| panic!("absorbed seed {seed}: recovery failed: {err}"));
    let recovered: BTreeMap<Vec<u8>, Vec<u8>> = reopened
        .scan()
        .unwrap_or_else(|err| panic!("absorbed seed {seed}: scan failed: {err}"))
        .into_iter()
        .map(|(key, value)| (key, value.into_vec()))
        .collect();
    assert!(
        snapshots.contains(&recovered),
        "absorbed seed {seed}: recovered state is not a committed interleaving"
    );
    members
}

#[test]
fn absorbed_multi_writer_regions_recover_to_a_committed_interleaving() {
    let mut members = 0;
    for seed in (0..500u64).step_by(3) {
        members += sweep_multi_writer_absorbed(seed);
    }
    assert!(
        members > 500,
        "the absorbed sweep judged only {members} members; it is not testing absorption"
    );
}

#[test]
#[ignore = "expensive: run with --release -- --ignored, or in CI"]
fn thousands_of_absorbed_multi_writer_seeds_recover() {
    let mut members = 0;
    for seed in (0..3_000u64).step_by(3) {
        members += sweep_multi_writer_absorbed(seed);
    }
    assert!(members > 3_000, "only {members} members judged");
}
