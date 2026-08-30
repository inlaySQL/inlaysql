//! Deterministic simulation coverage for `Durability::Normal`, the relaxed
//! commit barrier — same shape as `dst_sweep.rs`, same invariant, one
//! deliberate difference in the fault schedule.
//!
//! # What this proves, and what it cannot
//!
//! `dst_sweep.rs` runs crash and torn-write faults only, and its own module
//! doc explains why it leaves `Fault::ReorderedSync` out: reordering
//! interacts with log truncation at a checkpoint in a way that needed its
//! own hardening (the monotonicity floor `resolve_state_at_least` adds,
//! documented in `docs/recovery.md`). This sweep turns `ReorderedSync` back
//! on, at the same 1% rate as every other fault, because it is the fault
//! shape that most resembles what `Durability::Normal` actually risks: bytes
//! the caller was told were durable turn out to have rolled back to an
//! earlier point once the platter is asked — exactly a write that reached
//! the drive's volatile cache and nothing further.
//!
//! **The honest limit of this, stated rather than glossed over:** `CowBTree`
//! is generic over any [`Device`], and this sweep drives it over
//! [`SimDisk`]/[`Simulator`], the same as `dst_sweep.rs`. Neither implements
//! [`Device::sync_commit`] differently from [`Device::sync`] — both inherit
//! the trait's default (`sync_commit` calls `sync`), and
//! [`Device::set_durability`]'s default is a no-op. So calling
//! [`CowBTree::set_durability`] with [`Durability::Normal`] here exercises
//! the *plumbing* (the option threads through without erroring, panicking,
//! or changing which bytes a commit writes) and proves the recovery
//! invariant holds under every fault the harness can express with that
//! plumbing wired in — it does **not** prove anything about the real
//! `F_FULLFSYNC`-vs-`fsync`-vs-`fdatasync` distinction, because the
//! simulator has no notion of two barrier strengths to begin with. That
//! distinction is real-syscall-level (`inlaysql::FileDevice`,
//! `crates/inlaysql/src/device.rs`) and is covered instead by
//! `crates/inlaysql/tests/durability.rs`'s clean-path round trip, the
//! white-box `CommitCoordinator` ratchet tests beside the real
//! implementation, and the measured commits/s difference in `PERF.md` — none
//! of which inject faults, because the fault model that would need to is
//! exactly the gap named above.
//!
//! A second, narrower gap, also named in `Fault::ReorderedSync`'s own doc
//! comment: it rolls the *whole* durable image back to a prior whole-sync
//! snapshot. It cannot express "an arbitrary subset of one unsynced batch
//! survived, in arbitrary order" — the more granular thing a real drive's
//! write cache could in principle do. Nothing in this workspace's harness
//! can express that today; this sweep does not claim to cover it either.
//!
//! # Why this workload stops driving the handle the instant a sync reorders
//!
//! `Fault::ReorderedSync`'s own doc comment says what it models: "the caller
//! believes the sync committed, but the durable image rolls back… simulates
//! what a *later crash* would reveal." That is a statement about recovery
//! after this process is gone, not about this same live handle continuing to
//! operate on top of an image that just silently regressed under it with no
//! signal (`SyncOutcome::Committed`, not `Crashed` — unlike `Fault::Crash`/
//! `Fault::TornWrite`, so the existing `db.device().crashed()` guard never
//! fires for it). Driving the same handle further after that is not the
//! scenario `Durability::Normal`'s loss bound describes either: a real
//! process that keeps running past a barrier that did not actually reach the
//! platter has not lost anything yet — the loss is only revealed on the
//! *next* open, after a real crash. Confirmed empirically while writing this
//! sweep: continuing the workload past a reordered sync reaches
//! `docs/recovery.md`'s already-documented "reordered sync during a
//! checkpoint truncation" limitation *without ever calling `checkpoint`* —
//! the same region-zeroing happens automatically inside `CowBTree::commit`
//! whenever a WAL region fills — and reproduces identically with
//! `set_durability` never called at all, i.e. on code this change does not
//! touch. So it is out of scope here for the same reason `dst_sweep.rs`
//! leaves reordering out entirely, and this sweep sidesteps it the same way
//! `dst_sweep.rs`'s own crash check does: stop issuing operations on the live
//! handle the moment a sync outcome it cannot act on further occurs, and let
//! only the *durable* image answer what recovery sees.
//!
//! The assertion is unchanged from every other sweep: the recovered database
//! is byte-for-byte one of the states the workload actually committed, never
//! torn, partial, or invented.
//!
//! One more wrinkle the first version of this sweep missed, worth recording:
//! a single [`CowBTree::commit`] call that also happens to wrap its WAL
//! region issues **two** syncs — the wrap's own `write_state_values` sync,
//! then the ordinary `sync_commit` for the record itself — so checking only
//! the *last* trace event after a commit can miss a reorder that hit the
//! first of the two. The check below scans every new trace entry since the
//! last check, not just the latest one.

use std::collections::BTreeMap;

use inlaysql_core::btree::{CowBTree, Durability};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::sim::{Fault, FaultSchedule, SimDisk, Simulator, TraceEvent};
use inlaysql_core::traits::Rng;

const PAGE: usize = 256;
const BLOCK: usize = 512;
const CAPACITY: usize = 8 << 20;

/// Same batch count as `dst_sweep.rs`'s `sweep`, for a comparable sweep cost.
const BATCHES: usize = 64;

/// Whether any sync in `disk.trace()[since..]` was reordered — see the
/// module doc, "Why this workload stops driving the handle the instant a
/// sync reorders". Scans every new trace entry rather than only the last
/// one: a single `CowBTree::commit` that also wraps its WAL region issues
/// two syncs, and the first of the two can be the one that reordered while
/// the second (the ordinary `sync_commit`) comes back clean. `CowBTree` has
/// no hook for "which fault fired" at all; the trace is `SimDisk`'s own
/// record of exactly that.
fn any_sync_was_reordered_since(disk: &SimDisk, since: usize) -> bool {
    disk.trace()[since..].iter().any(|event| {
        matches!(
            event,
            TraceEvent::Sync {
                fault: Fault::ReorderedSync { .. },
                ..
            }
        )
    })
}

/// Run one seed at `Durability::Normal` and assert the recovered state is a
/// committed snapshot — the same invariant `dst_sweep::sweep` asserts at the
/// default level, with `Fault::ReorderedSync` turned back on (see the module
/// doc for why that fault, specifically, is the closest match to what
/// `Normal` actually risks).
fn sweep_normal_durability(seed: u64) {
    // Crash, torn-write *and* reordered-sync faults, 1% each per sync — the
    // one deliberate difference from `dst_sweep::sweep`, which sets
    // reorder to 0.
    let sim = Simulator::with_disk(
        seed,
        SimDisk::with_block_size(BLOCK, CAPACITY),
        FaultSchedule::random_with(seed, 10, 10, 10),
    );
    let mut db = CowBTree::create(sim, PAGE).unwrap();

    // If create's own sync faulted, the header never became durable and the
    // database simply does not exist yet — nothing to recover.
    let durable = db.device().disk().durable();
    if durable.len() < 8 || &durable[..8] != b"INLAYSQL" {
        return;
    }
    let mut trace_cursor = db.device().disk().trace().len();
    if any_sync_was_reordered_since(db.device().disk(), 0) {
        return;
    }

    // The plumbing under test: every commit this handle makes from here on
    // asks for the relaxed level. `CowBTree::durability()` is asserted once
    // so a future refactor that silently drops the request fails loudly here
    // rather than only in the (much quieter) absence of a throughput change.
    db.set_durability(Durability::Normal);
    assert_eq!(db.durability(), Durability::Normal);

    let mut rng = SeededRng::new(seed ^ 0xA5A5_5A5A_1234_5678);
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
        // Record this commit's intended state *before* checking for a fault:
        // a torn or reordered sync can still leave the whole record durable
        // (the commit survived), so it belongs in the recoverable set.
        snapshots.push(expected.clone());
        let reordered = any_sync_was_reordered_since(db.device().disk(), trace_cursor);
        trace_cursor = db.device().disk().trace().len();
        if db.device().crashed() || reordered {
            break 'workload;
        }
        // Deliberately no checkpoint here — the automatic in-commit
        // WAL-region wrap already exercises the same region-zeroing path;
        // an explicit checkpoint would only add another way to reach it.
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
fn hundreds_of_seeds_recover_to_a_committed_snapshot_at_normal_durability() {
    for seed in 0..500u64 {
        sweep_normal_durability(seed);
    }
}

#[test]
#[ignore = "expensive: run with --release -- --ignored, or in CI"]
fn thousands_of_seeds_recover_to_a_committed_snapshot_at_normal_durability() {
    for seed in 0..10_000u64 {
        sweep_normal_durability(seed);
    }
}
