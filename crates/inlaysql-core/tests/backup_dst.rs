//! Deterministic simulation coverage for online backup
//! (`CowBTree::backup_to`, `crates/inlaysql-core/src/btree/backup.rs`).
//!
//! A backup rests on one claim — *a committed root is already an immutable,
//! consistent snapshot, so copying the pages it reaches is enough* — and the
//! cheapest way to be wrong about it is to miss a page. A missed interior node
//! or a missed link in an overflow chain does not fail loudly: the copy opens,
//! and answers a query with a hole in it. So the assertion here is equality
//! with the exact map the workload committed, not merely that the copy opens.
//!
//! Two moments are covered, and they are different claims:
//!
//! * **Mid-workload**, after each successful commit, the copy must be
//!   *exactly* the state just committed. Stronger than the "recovered state is
//!   some committed snapshot" the recovery sweeps assert, and it can be: unlike
//!   recovery, a backup is not allowed to land on an earlier snapshot. It is
//!   taken from a root this handle holds, and `&self` is what stops that root
//!   moving under it.
//! * **After the fault schedule has done whatever it drew** — a crash or a
//!   torn write mid-commit — the durable image is recovered with
//!   `CowBTree::open` (write-ahead-log replay and all) and *that* is backed up.
//!   This is the composition that matters operationally and the one nobody
//!   would think to write separately: backing up a database that has just come
//!   back from a crash. The copy must equal the recovered tree, and the
//!   recovered tree must be one of the states the workload committed — the
//!   same property `dst_sweep.rs` and `free_list_reuse_dst.rs` assert, carried
//!   through the copy.
//!
//! The destination is a clean `SimDisk` with no fault schedule of its own.
//! That is deliberate: what is under test is whether the *copy* is complete
//! and consistent, and injecting faults into the destination as well would
//! only re-test `CowBTree::open`'s recovery, which has its own sweeps. A
//! backup whose own write fails is handled a level up, in `inlaysql`, by
//! writing to a temporary file and renaming only on success.
//!
//! Page reuse is off here, as it is in every sweep but `free_list_reuse_dst.rs`
//! — `SimDisk` answers `None` to `Device::min_reader_seq`, so nothing on it
//! ever reclaims a page and turning reuse on would exercise nothing. The
//! interaction between backup and reuse is proven where it is real, against a
//! `FileDevice` that genuinely reclaims: `crates/inlaysql/tests/backup.rs`.

use std::collections::BTreeMap;

use inlaysql_core::btree::{CowBTree, Device};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::sim::{FaultSchedule, SimDisk, Simulator};
use inlaysql_core::traits::Rng;

const PAGE: usize = 256;
const BLOCK: usize = 512;
const CAPACITY: usize = 8 << 20;

/// Commit batches per seed. Enough that the tree is several levels deep and
/// has been checkpointed past more than once before the schedule interrupts
/// it, so a backup is copying a real tree rather than a single leaf.
const BATCHES: usize = 60;

/// Distinct keys the workload cycles through. Wide enough to force splits,
/// narrow enough that deletes actually collapse nodes.
const KEY_SPACE: u64 = 60;

/// Every fourth commit is backed up mid-workload. Not every commit: the copy
/// is O(live pages) and this sweep runs hundreds of seeds.
const BACKUP_EVERY: usize = 4;

/// A value long enough to overflow a 256-byte page, so overflow chains — the
/// one part of the reachable set that is not a B-tree node, and the part a
/// naive walk silently drops — are in every seed's tree.
fn big_value(tag: u64) -> Vec<u8> {
    let mut value = format!("v{tag:016x}-").into_bytes();
    value.resize(600, b'x');
    value
}

fn contents<D: Device>(tree: &CowBTree<D>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    tree.scan()
        .expect("scan")
        .into_iter()
        .map(|(key, value)| (key, value.into_vec()))
        .collect()
}

/// Copy `tree`'s committed snapshot to a fresh disk, reopen it, and return
/// what it holds.
fn backup_contents<D: Device>(tree: &CowBTree<D>, seed: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut dest = SimDisk::with_block_size(BLOCK, CAPACITY);
    let summary = tree
        .backup_to(&mut dest)
        .unwrap_or_else(|err| panic!("seed {seed}: backup failed: {err}"));
    assert_eq!(
        summary.root,
        tree.root(),
        "seed {seed}: the copy must name the root it was taken from"
    );
    let copy = CowBTree::open(dest)
        .unwrap_or_else(|err| panic!("seed {seed}: the copy did not open: {err}"));
    // The copy's state block already names the snapshot and its log is empty,
    // so opening it must not have moved anything — if it did, the copy was
    // not self-describing and a reader would be trusting recovery to guess.
    assert_eq!(
        copy.root(),
        summary.root,
        "seed {seed}: opening the copy changed its root, so it was not a \
         complete snapshot on its own"
    );
    contents(&copy)
}

/// Run one seed's workload, backing up as it goes and once more after
/// recovery, and return how many mid-workload backups it managed.
fn sweep(seed: u64) -> usize {
    let sim = Simulator::with_disk(
        seed,
        SimDisk::with_block_size(BLOCK, CAPACITY),
        FaultSchedule::random_with(seed, 8, 8, 0),
    );
    let mut db = match CowBTree::create(sim, PAGE) {
        Ok(db) => db,
        Err(_) => return 0,
    };
    if db.device().crashed() {
        return 0;
    }

    let mut rng = SeededRng::new(seed ^ 0x5EED_BACC_0FFE_E123);
    let mut snapshots: Vec<BTreeMap<Vec<u8>, Vec<u8>>> = vec![BTreeMap::new()];
    let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut live_backups = 0usize;

    'workload: for batch in 0..BATCHES {
        let ops = 1 + (rng.next_u64() % 5) as usize;
        for _ in 0..ops {
            let key = format!("k{:04}", rng.next_u64() % KEY_SPACE).into_bytes();
            match rng.next_u64() % 5 {
                0 => {
                    expected.remove(&key);
                    db.delete(&key).expect("delete");
                }
                // One value in five overflows a page, so the tree carries
                // overflow chains without being made entirely of them.
                1 => {
                    let value = big_value(rng.next_u64());
                    expected.insert(key.clone(), value.clone());
                    db.put(&key, &value).expect("put");
                }
                _ => {
                    let value = format!("v{:016x}-{batch}", rng.next_u64()).into_bytes();
                    expected.insert(key.clone(), value.clone());
                    db.put(&key, &value).expect("put");
                }
            }
        }

        let commit_result = db.commit();
        // Recorded before the fault check, exactly as the other sweeps do: a
        // torn write can leave the whole record durable, so this commit may
        // have survived whether or not the caller was told the sync failed.
        snapshots.push(expected.clone());
        if commit_result.is_err() || db.device().crashed() {
            break 'workload;
        }

        if batch % BACKUP_EVERY == 0 {
            assert_eq!(
                backup_contents(&db, seed),
                expected,
                "seed {seed}, batch {batch}: a backup of a live database must be \
                 exactly the state its handle has committed"
            );
            live_backups += 1;
        }

        if rng.next_u64().is_multiple_of(7) && (db.checkpoint().is_err() || db.device().crashed()) {
            break 'workload;
        }
    }

    let image = db.device().disk().durable().to_vec();
    drop(db);

    // Backing up a database that has just recovered from whatever the schedule
    // drew. `CowBTree::open` here is the real recovery path — state block,
    // cross-region log merge, page replay, checkpoint — and the copy is taken
    // from the root it lands on.
    let recovered = CowBTree::open(SimDisk::with_image(BLOCK, &image))
        .unwrap_or_else(|err| panic!("seed {seed}: recovery failed: {err}"));
    let recovered_contents = contents(&recovered);
    assert!(
        snapshots.contains(&recovered_contents),
        "seed {seed}: recovered state is not any committed snapshot"
    );
    assert_eq!(
        backup_contents(&recovered, seed),
        recovered_contents,
        "seed {seed}: a backup of a recovered database must be exactly what it \
         recovered to"
    );

    live_backups
}

#[test]
fn a_backup_is_exactly_the_snapshot_it_was_taken_from_under_faults() {
    let mut total = 0usize;
    for seed in 0..200u64 {
        total += sweep(seed);
    }
    // If this ever reaches zero, every seed crashed on its first commit and
    // the mid-workload assertion above never ran — the sweep would still be
    // green while testing nothing, which is the failure mode this guards.
    assert!(
        total > 0,
        "no seed ever reached a mid-workload backup — this sweep is testing nothing"
    );
}

#[test]
#[ignore = "expensive: run with --release -- --ignored, or in CI"]
fn thousands_of_seeds_of_backups_are_exactly_the_snapshots_they_came_from() {
    let mut total = 0usize;
    for seed in 0..5_000u64 {
        total += sweep(seed);
    }
    assert!(total > 0, "no seed ever reached a mid-workload backup");
}
