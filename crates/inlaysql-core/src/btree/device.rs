//! The seam between the copy-on-write B-tree and a byte-addressable device.
//!
//! The tree never talks to a real disk directly. It reads and writes through a
//! [`Device`], which is the same trick the rest of the core uses for the clock
//! and the indexes: production wiring points at a real file, and the
//! deterministic test wiring points at [`crate::sim::SimDisk`] (or a
//! fault-injecting [`crate::sim::Simulator`]). That is what lets the whole
//! engine run, crash and recover under the simulation harness.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::btree::PageId;
use crate::error::Result;

/// The final logical mutation for every key one open transaction touched —
/// [`crate::btree::CowBTree`]'s `pending_ops`, named here because commit-side
/// absorption moves one across the [`Device`] seam. `None` is a delete.
pub type PendingOps = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// One parked writer's open transaction, as the gate holder sees it.
///
/// This is deliberately the *whole* of what a leader needs and nothing more:
/// the committed root that transaction was built against, and its logical
/// operations. Both are plain data — no `Rc`, no page, no tree — which is
/// what lets a transaction built on one thread be judged on another. See
/// `docs/research/commit-group-slice1.md` §1 for why the leader cannot be
/// handed the follower's `CowBTree` instead: it is `!Send`, and always will
/// be.
#[derive(Debug, Default)]
pub struct AbsorbTxn {
    /// The committed root the transaction was built against — the same value
    /// `rebase_pending`'s comparison reads as `self.root`.
    pub root: PageId,
    /// The transaction's logical mutations, moved out of the offering
    /// handle rather than copied, and moved back into it when it wakes.
    pub ops: PendingOps,
}

/// What a leader hands one member of its cohort back, once the whole cohort
/// is durable.
///
/// Plain `Copy` data, like everything else that crosses this seam: the leader
/// computed it on its own thread against its own tree, and the member's
/// `CowBTree` — `!Send`, and always will be — only ever adopts numbers out of
/// it. See `docs/research/commit-group-slice2.md` §1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbsorbResult {
    /// The leader rebased, encoded, appended and synced this transaction on
    /// its behalf. The member adopts these three values and reports
    /// [`crate::btree::CommitOutcome::Committed`].
    Committed {
        /// The committed root this transaction produced.
        root: PageId,
        /// The next free page id it left behind.
        next: PageId,
        /// The sequence number it committed at — this member's own, never the
        /// cohort's last, because the reader watermark it feeds has to stay
        /// conservative for the oldest live reader.
        seq: u64,
        /// The one generation the whole gate hold produced, or `None` from a
        /// device that does not count them.
        generation: Option<u64>,
    },
    /// First-committer-wins aborted it against an earlier member of the same
    /// cohort or against the state the leader found. The file is at the state
    /// named here and the member reloads it, exactly as a conflicting commit
    /// does today.
    Conflict {
        /// The committed root the member should adopt.
        root: PageId,
        /// The next free page id that state names.
        next: PageId,
        /// The highest committed sequence number at that state.
        seq: u64,
        /// See [`AbsorbResult::Committed::generation`].
        generation: Option<u64>,
    },
    /// The commit that was carrying this transaction failed after its record
    /// may already have reached the file. Reported to the caller as an error,
    /// never as `Fallback`: telling a member to commit again when its bytes
    /// may be on disk is how a transaction gets applied twice. This is the
    /// same ambiguity a solo commit whose own append or sync failed has
    /// always had.
    Failed(&'static str),
    /// Nobody judged it. Its operations are back in its handle and it commits
    /// exactly as it does with absorption off. Every device that does not
    /// absorb answers this, always.
    Fallback,
}

/// One member of a cohort as the queue holds it between the offer and the
/// answer.
#[derive(Debug)]
struct Parked {
    token: u64,
    /// The value [`AbsorbQueue::gate_generation`] had when this was offered.
    /// If it has moved and this entry is still parked, the gate hold it was
    /// offered into ended without taking it, and it must go back to its owner
    /// rather than wait for a leader that is never coming.
    gate_generation: u64,
    txn: AbsorbTxn,
}

/// Everything a commit's reservation gate would otherwise re-derive from the
/// file before it can reserve anything: the committed state, and where the
/// next record goes in one write-ahead-log region.
///
/// Deriving it costs a read of the state block plus a scan of **every** log
/// region, and a scan decodes each record whole — including a checksum over
/// every data page the record copied — because that is what recovery needs.
/// Under the reservation gate that cost is serialised across every writer on
/// the file and grows with the bytes committed since the last checkpoint, which
/// is what capped concurrent-writer throughput at one writer's worth of work
/// (AHL-468). A device that can prove it speaks for every writer answers from
/// memory instead; see [`Device::commit_point`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitPoint {
    /// The committed tree root (0 = empty tree).
    pub root: PageId,
    /// The next free page id the committed state names.
    pub next: PageId,
    /// The highest committed sequence number.
    pub seq: u64,
    /// Byte offset the next record appends at, in the region asked about.
    pub append_offset: usize,
}

/// How strong a barrier [`Device::sync_commit`] must use for an ordinary
/// user commit.
///
/// This governs [`Device::sync_commit`] **only**. [`Device::sync`] — used by
/// [`crate::btree::CowBTree`]'s state-block rewrite and by
/// [`crate::btree::CowBTree::checkpoint`] — is never affected by this enum;
/// see [`Device::sync`]'s doc comment for why weakening it would let a
/// checkpoint or WAL-region wrap roll back further than a level's own loss
/// bound promises, not merely lose the bound's own commits.
///
/// The exact syscall each level maps to is a real-device concern — see
/// `inlaysql::FileDevice`'s implementation and `docs/recovery.md`'s
/// "Durability levels" section for the full loss-bound argument and the
/// per-platform mapping. A device that has only one barrier strength (every
/// [`crate::sim`] device, the WASM in-memory device) is free to ignore the
/// level entirely; [`Device::set_durability`]'s default is a no-op for
/// exactly that reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Durability {
    /// Nothing committed is ever lost. The default, and the only level every
    /// caller got before this option existed — no existing behaviour changes
    /// unless a caller opts into [`Durability::Normal`] explicitly.
    #[default]
    Full,
    /// Survives a process crash and an OS crash with zero loss — the bytes
    /// have left the process and the kernel's page cache either way. Only a
    /// **power failure** can lose a commit at this level: bytes that reached
    /// the drive's own volatile write cache but not the platter. Loss is
    /// bounded to commits since the last checkpoint or WAL-region wrap, and
    /// — because commit-chain validation on reopen is file-wide, not
    /// per-handle — one writer's lost sync can roll back another writer's
    /// individually-synced commits on the same file too. Never torn or
    /// invented state either way: recovery always lands on a real past
    /// commit. See `docs/recovery.md`.
    Normal,
}

/// The bookkeeping behind commit-side absorption, with no lock of its own.
///
/// A device that absorbs holds one of these behind whatever it already uses
/// for interior mutability — a `Mutex` on the native file device, a `RefCell`
/// in the deterministic simulation — and forwards the `absorb_*` methods of
/// [`Device`] straight into it. That is deliberate: the parts of this
/// protocol that are easy to get wrong are the liveness rules in
/// [`AbsorbQueue::wait_step`], and a second copy of them in the simulation
/// would mean the deterministic sweeps proved a different implementation
/// correct than the one production runs.
///
/// Nothing here is `Send` or `Sync` by itself; the owning device supplies
/// that, along with the ordering.
///
/// # The three rules that make a parked writer's wait finite
///
/// 1. A leader resolves everything [`AbsorbQueue::take`] handed it, on every
///    exit path — including an unwind, which is what
///    [`AbsorbQueue::fail_in_flight`] is for.
/// 2. A member never taken is handed back when the gate hold it offered into
///    ends: [`AbsorbQueue::gate_released`] moves `gate_generation` on, and
///    [`AbsorbQueue::wait_step`] un-parks anything still waiting at an older
///    one.
/// 3. A member taken but not yet resolved keeps waiting, which is safe
///    because rule 1 guarantees an answer and rule 2 cannot fire for it.
///
/// See `docs/research/commit-group-slice2.md` §2.
#[derive(Debug, Default)]
pub struct AbsorbQueue {
    /// Whether any handle sharing this device asked for absorption. A device
    /// sets it once and never clears it: the gate belongs to the file, so
    /// "some writers on this file may be absorbed and some may not" is not a
    /// state worth having.
    pub enabled: bool,
    /// Whether a normal commit currently holds the reservation gate. Offers
    /// are only accepted while it is true, and it is read under the same lock
    /// the offer takes, so "I parked behind a leader that has already gone"
    /// is impossible rather than unlikely.
    gate_held: bool,
    next_token: u64,
    /// Offered and not yet taken by a leader, in gate-arrival order.
    parked: Vec<Parked>,
    /// Taken by a leader and not yet answered.
    in_flight: Vec<u64>,
    /// Answered, waiting for their own thread to pick the answer up. The
    /// operations ride along because a `Fallback` has to give them back, and
    /// the `bool` is whether the answer is complete — see
    /// [`AbsorbQueue::gate_released`].
    resolved: BTreeMap<u64, (AbsorbResult, PendingOps, bool)>,
    /// Moved on by every gate release. See the field on [`Parked`].
    gate_generation: u64,
    /// Cohorts a leader has taken over this device's lifetime, so a test can
    /// assert that absorption actually happened rather than assume it — the
    /// discipline `pages_reused` exists for on the free list.
    pub cohorts: u64,
    /// Transactions taken as part of one of those cohorts. See
    /// [`AbsorbQueue::cohorts`].
    pub members: u64,
    /// Of those, the ones a leader actually committed on their behalf.
    pub committed: u64,
}

impl AbsorbQueue {
    /// Most transactions one gate holder will commit in a single cohort.
    ///
    /// A bound rather than a tuning knob: the leader does every member's
    /// rebase, encode and page write inside its own gate hold, and the whole
    /// cohort's records go into one buffer that has to fit a write-ahead-log
    /// region. A writer arriving at a full cohort is simply not offered and
    /// commits exactly as it does today.
    pub const COHORT_MAX: usize = 32;

    /// Whether a normal commit holds the gate right now — the condition
    /// [`Device::absorb_offer`] checks before offering, under this queue's
    /// own lock.
    pub fn gate_held(&self) -> bool {
        self.gate_held
    }

    /// A normal commit has acquired the reservation gate and may lead.
    pub fn gate_acquired(&mut self) {
        self.gate_held = true;
    }

    /// A normal commit has released the reservation gate, producing
    /// `generation`.
    ///
    /// Three things happen here, and each closes a different hole.
    ///
    /// *Stamping.* A leader files its answers before it can know the
    /// generation its own gate release will produce, so it files them
    /// incomplete and this fills them in. A member is not allowed to see an
    /// answer until then: adopting `None` would be correct but would cost it
    /// a full log scan on its next statement, which is the cost
    /// [`Device::commit_generation`] exists to avoid.
    ///
    /// *Moving `gate_generation` on.* That is what lets a member nobody took
    /// stop waiting (rule 2 above).
    ///
    /// *Failing out anything still in flight.* The safety net for rule 1: a
    /// leader answers its cohort before it releases, so on every ordinary
    /// path there is nothing here to fail. It fires when the leader unwound,
    /// and it is reached because the same guard that releases the gate on a
    /// panic calls this too.
    pub fn gate_released(&mut self, generation: Option<u64>) {
        self.gate_held = false;
        self.gate_generation += 1;
        for (result, _, ready) in self.resolved.values_mut() {
            if *ready {
                continue;
            }
            match result {
                AbsorbResult::Committed { generation: g, .. }
                | AbsorbResult::Conflict { generation: g, .. } => *g = generation,
                AbsorbResult::Failed(_) | AbsorbResult::Fallback => {}
            }
            *ready = true;
        }
        self.fail_in_flight("the commit leading this cohort did not finish");
    }

    /// [`Device::absorb_offer`]. Leaves `ops` untouched when it answers
    /// `None`, so the disabled path costs nothing but the check.
    pub fn offer(&mut self, root: PageId, ops: &mut PendingOps) -> Option<u64> {
        if !self.enabled || self.parked.len() >= Self::COHORT_MAX {
            return None;
        }
        self.next_token += 1;
        let token = self.next_token;
        let ops = core::mem::take(ops);
        self.parked.push(Parked {
            token,
            gate_generation: self.gate_generation,
            txn: AbsorbTxn { root, ops },
        });
        Some(token)
    }

    /// [`Device::absorb_take`]: fix this cohort's membership and hand it to
    /// the leader.
    ///
    /// Membership is fixed by the drain, exactly as the flush side fixes its
    /// cohort by snapshotting `writes_completed` strictly before the barrier —
    /// a writer that parks after this point belongs to somebody else's cohort.
    /// The tokens stay behind in `in_flight` so an unwind can still answer
    /// them.
    pub fn take(&mut self) -> Vec<(u64, AbsorbTxn)> {
        if !self.enabled || self.parked.is_empty() {
            return Vec::new();
        }
        let cohort: Vec<(u64, AbsorbTxn)> = core::mem::take(&mut self.parked)
            .into_iter()
            .map(|parked| (parked.token, parked.txn))
            .collect();
        self.cohorts += 1;
        self.members += cohort.len() as u64;
        self.in_flight
            .extend(cohort.iter().map(|(token, _)| *token));
        cohort
    }

    /// [`Device::absorb_resolve`]: file one answer per member.
    ///
    /// `ops` is only read for [`AbsorbResult::Fallback`], which is the one
    /// answer that says "this transaction is still yours"; every other answer
    /// means the leader consumed it.
    pub fn resolve(&mut self, results: Vec<(u64, AbsorbResult, PendingOps)>) {
        for (token, result, ops) in results {
            self.in_flight.retain(|held| *held != token);
            if matches!(result, AbsorbResult::Committed { .. }) {
                self.committed += 1;
            }
            self.resolved.insert(token, (result, ops, false));
        }
    }

    /// Answer every member a leader took and never resolved with
    /// [`AbsorbResult::Failed`].
    ///
    /// `Failed` rather than `Fallback` on purpose: the leader may have got as
    /// far as writing the cohort's records, and a member told to try again
    /// with bytes already on the file would apply its transaction twice.
    pub fn fail_in_flight(&mut self, reason: &'static str) {
        for token in core::mem::take(&mut self.in_flight) {
            self.resolved.insert(
                token,
                (AbsorbResult::Failed(reason), PendingOps::new(), true),
            );
        }
    }

    /// One turn of [`Device::absorb_wait`]'s wait.
    ///
    /// `Some` is the final answer and `ops` has been restored if the answer
    /// needs them. `None` means "not yet" — a threaded device waits on its
    /// condvar and calls again; a single-threaded simulation can never see it,
    /// because there the leader has always already resolved by the time a
    /// follower's `commit()` is entered.
    pub fn wait_step(&mut self, token: u64, ops: &mut PendingOps) -> Option<AbsorbResult> {
        if self
            .resolved
            .get(&token)
            .is_some_and(|(_, _, ready)| *ready)
        {
            let (result, returned, _) = self
                .resolved
                .remove(&token)
                .expect("just checked that this token is resolved");
            *ops = returned;
            return Some(result);
        }
        let Some(index) = self.parked.iter().position(|parked| parked.token == token) else {
            // Taken by a leader and not answered yet. Rule 3.
            return None;
        };
        if self.parked[index].gate_generation == self.gate_generation && self.gate_held {
            return None;
        }
        // Rule 2: the gate hold this was offered into has ended without
        // taking it. Nothing was written on its behalf, so the ordinary
        // commit path is exactly right.
        let parked = self.parked.remove(index);
        *ops = parked.txn.ops;
        Some(AbsorbResult::Fallback)
    }
}

/// A byte-addressable, randomly-accessible durable store.
///
/// Offsets are in bytes. Implementations are expected to buffer writes until
/// [`Device::sync`] is called, exactly like the operating system buffers writes
/// in the page cache until `fsync`; the simulation harness relies on that
/// property to model crashes.
pub trait Device {
    /// Read `buf.len()` bytes starting at `offset`.
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()>;

    /// The `len` bytes at `offset` as a buffer this device already holds, or
    /// `None` when it holds no such buffer.
    ///
    /// [`Device::read`] forces a copy by its type: the caller owns the buffer,
    /// so a device that has the page resident in a cache of its own can only
    /// `memcpy` it out. The raw leaf scan
    /// ([`crate::btree::CowBTree`]'s `walk_raw_row_values`) then keeps the
    /// page behind a shared `Arc<[u8]>` its rows borrow from — a second copy
    /// of the same bytes. On a full sweep of a table the operating system,
    /// the device cache and the tree all already held, those two copies were
    /// the cost: `pread + memmove` at 19.5% of the `GROUP BY` profile after
    /// AHL-528, and raising the device cache 8 → 64 MiB measured flat
    /// (`PERF.md`, AHL-536). This method is the seam that removes both: the
    /// device hands out its own `Arc`, the scan borrows straight from it, and
    /// nothing is copied.
    ///
    /// # Contract
    ///
    /// * **`None` means "read it the ordinary way".** That is the default, so
    ///   every existing device — the simulation disks, the WASM in-memory
    ///   device, `io_uring` — stays correct by saying nothing: the caller
    ///   falls back to [`Device::read`] and copies as before.
    /// * **A `Some` value must be exactly `len` bytes and must equal what
    ///   [`Device::read`] of the same range would fill at the same moment.**
    ///   The caller may hold the `Arc` for as long as it likes — rows borrow
    ///   from it and outlive the scan that read it — so the bytes behind it
    ///   must never change. A copy-on-write data page is exactly that
    ///   (`docs/architecture.md` D4: a committed page is immutable) **unless
    ///   page ids are reused**, which is why a device must answer `None` from
    ///   the moment [`Device::note_page_reuse_enabled`] is called, the same
    ///   rule its own cache already lives under. The tree gates its calls the
    ///   same way it gates its caches (`page_reuse_enabled`, data-area page,
    ///   not dirtied by the open transaction), so both sides refuse.
    /// * **Only a resident page is answered.** This is a lookup, not a read:
    ///   a device must not fetch on a miss, because the caller's fallback
    ///   reads ahead sixteen pages at a time (AHL-522) and a page-at-a-time
    ///   fetch here would silently undo that. The cost of a miss is one
    ///   lookup, and it is paid only where the alternative was a `pread`.
    ///
    /// `Arc` rather than `Rc` because a device shared across threads — the
    /// native file device behind `inlaysql serve` — holds its pages behind
    /// an `Arc`, and converting would be the copy this exists to remove; the
    /// atomic refcount is paid once per row and measured flat against the
    /// `Rc` it replaced (`PERF.md`, AHL-536).
    fn read_shared(&self, _offset: usize, _len: usize) -> Option<Arc<[u8]>> {
        None
    }

    /// Write `data` at `offset`. Not necessarily durable until [`Device::sync`].
    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()>;

    /// Make all previously written bytes durable.
    ///
    /// # This must always run at full strength — never relaxed by [`Durability`]
    ///
    /// [`crate::btree::CowBTree`] calls this for the state-block rewrite
    /// (`write_state_values`) and for [`crate::btree::CowBTree::checkpoint`],
    /// both of which can truncate or reuse a write-ahead-log region the
    /// instant they return. [`Device::sync_commit`] is the only method a
    /// [`Durability`] level may weaken. Weakening this one instead would let
    /// a checkpoint publish a state block, or a wrap zero a WAL region,
    /// before the writes they depend on are actually durable — so a later
    /// crash could roll recovery back past commits the *caller* was
    /// individually told were durable at whatever level they used, not just
    /// past the ones this handle's own relaxed level admits losing. That
    /// breaks the promised loss bound (though it still cannot corrupt: see
    /// [`Durability::Normal`]'s doc comment). Keep every implementation of
    /// this method at its platform's strongest barrier, unconditionally.
    fn sync(&mut self) -> Result<()>;

    /// Mark a normal commit's record and data pages ready for a grouped flush.
    ///
    /// Called after the commit's writes have returned, but before the short
    /// reservation gate is released. A native device may publish a durability
    /// ticket here so another commit's flush can cover it. The default is a
    /// no-op: devices that do not group normal commits keep the existing
    /// [`Device::sync_commit`] fallback without changing their semantics.
    fn commit_ready(&self) {}

    /// Make a normal commit durable after [`Device::end_normal_commit`].
    ///
    /// This is separate from [`Device::sync`] because checkpoints perform an
    /// in-gate sync. A native device that waits for other normal commits here
    /// must not accidentally wait on a checkpoint holding the reservation
    /// gate. The default preserves the old behavior exactly.
    fn sync_commit(&mut self) -> Result<()> {
        self.sync()
    }

    /// Request a [`Durability`] level for this handle's future
    /// [`Device::sync_commit`] calls.
    ///
    /// Called by [`crate::btree::CowBTree::set_durability`] once, at open
    /// (unlike [`Device::note_page_reuse_enabled`], this is called for
    /// *every* level, including [`Durability::Full`] — a device shared by
    /// several handles needs to see a `Full` request even when it is the
    /// default, so it can tell "nobody has asked for anything yet" apart
    /// from "somebody explicitly needs the strongest barrier"; see
    /// `inlaysql::FileDevice`'s `CommitCoordinator` for why that distinction
    /// is exactly what makes cross-handle "strongest wins" possible).
    ///
    /// The default is a no-op: a device with only one barrier strength
    /// (every [`crate::sim`] device, the WASM in-memory device, the
    /// `io_uring` backend, which does not yet implement this split) has
    /// nothing to relax, and every existing implementation stays correct by
    /// doing nothing.
    fn set_durability(&self, _durability: Durability) {}

    /// Enter the short commit-reservation critical section.
    ///
    /// Implementations shared by genuinely parallel writers use this to make
    /// sequence/page allocation and WAL append placement atomic. The expensive
    /// [`Device::sync`] happens after this section has been left, so writers
    /// can still flush separate log regions concurrently.
    fn begin_commit(&self) -> Result<()> {
        Ok(())
    }

    /// Enter a normal user-commit reservation. Devices that do not distinguish
    /// it from a checkpoint use the same short reservation by default.
    fn begin_normal_commit(&self) -> Result<()> {
        self.begin_commit()
    }

    /// Leave the critical section entered by [`Device::begin_commit`], and
    /// report the [`Device::commit_generation`] this commit produced.
    ///
    /// Returning the value here rather than letting the caller read it back is
    /// what makes it usable: the read is atomic with the increment, so the
    /// answer is "everything committed up to and including mine, and nothing
    /// after". A caller that instead called [`Device::commit_generation`] on
    /// the line after leaving the gate could be overtaken by another writer in
    /// between, record *that* writer's generation, and then never look at the
    /// log again — silently serving a snapshot that is missing a commit.
    ///
    /// `None` from a device that does not count commits, matching the default
    /// [`Device::commit_generation`].
    fn end_commit(&self) -> Option<u64> {
        None
    }

    /// Leave a normal user-commit reservation. The default shares the ordinary
    /// commit boundary because the device has no separate grouping protocol.
    fn end_normal_commit(&self) -> Option<u64> {
        self.end_commit()
    }

    /// A counter that changes whenever a commit by *any* handle on this device
    /// has become visible — or `None` when this device cannot say.
    ///
    /// This exists so a reader can answer "has anything been committed since I
    /// last looked?" without reading the device at all. A handle between
    /// statements has to ask that question constantly (see
    /// [`crate::btree::CowBTree::refresh`]) and the honest answer is almost
    /// always "no"; finding that out by re-reading the state block and scanning
    /// every write-ahead log region is what made an unchanged snapshot cost
    /// more than the query it was refreshed for.
    ///
    /// # Contract
    ///
    /// * **`None` means "assume something changed".** That is the default, and
    ///   it is why every existing implementation stays correct without doing
    ///   anything: a device that says nothing is re-scanned every time, exactly
    ///   as before.
    /// * **A `Some` value must change on every commit that becomes visible to
    ///   another handle, and must change only after that commit's bytes are
    ///   readable.** Changing it too often costs an unnecessary scan. Changing
    ///   it too rarely — or too early — serves stale data, which is why
    ///   [`Device::end_commit`], not [`Device::begin_commit`], is where an
    ///   implementation increments it.
    ///
    /// # WARNING — this is only sound while writers cannot be out of process
    ///
    /// An in-process counter can only speak for in-process writers. Every
    /// device that returns `Some` today is one where that is guaranteed: the
    /// native file device's read-write handle (`FileDevice::open`) holds an
    /// **exclusive OS advisory lock** on the file for as long as it is open,
    /// so a second process is refused rather than allowed to write behind our
    /// back, and the WASM device owns its bytes outright.
    ///
    /// **A device that admits a writer it cannot see must return `None`.**
    /// This was not a hypothetical: `FileDevice::open_read_only` is exactly
    /// that device. It takes no OS lock at all — that is what lets it coexist
    /// beside a writer in another process, the whole reason it exists — which
    /// means a writer there can commit at any moment without this process's
    /// counter moving. It returns `None` unconditionally, which is what makes
    /// [`crate::btree::CowBTree::refresh`] fall back to re-reading the state
    /// block and scanning the write-ahead log on every statement instead of
    /// trusting a counter that cannot speak for a writer it never locked out.
    /// That scan is the cost of the correctness this method exists to protect;
    /// see `docs/mcp.md` for the measured number. A `Some` here for such a
    /// device would silently serve a snapshot the other process had already
    /// moved past, forever.
    fn commit_generation(&self) -> Option<u64> {
        None
    }

    /// The committed state and `region`'s append position, without reading the
    /// file — or `None` when this device cannot say.
    ///
    /// This is [`Device::commit_generation`]'s argument taken one step further,
    /// and it rests on exactly the same proof. That method lets a handle skip a
    /// log scan when *nothing* has been committed since it last looked. This
    /// one lets a handle skip the scan when something *has* — which is the only
    /// case that matters once several writers share a file, because then every
    /// commit is preceded by somebody else's.
    ///
    /// # Contract
    ///
    /// * **`None` means "derive it from the file".** That is the default, so
    ///   every existing implementation stays correct by saying nothing: it is
    ///   re-derived on every commit, exactly as before.
    /// * **A `Some` value must equal what a read of the state block plus a scan
    ///   of every log region would derive, at the moment it is returned.** It is
    ///   read under the reservation gate and believed without checking.
    /// * **Only [`Device::set_commit_point`], called under the gate, may change
    ///   it** — see that method.
    ///
    /// # WARNING — this is only sound while writers cannot be out of process
    ///
    /// Read [`Device::commit_generation`]'s warning first; it applies here
    /// unchanged and with more force. A cached committed state is a claim that
    /// nothing outside this process can move the file's committed state, and a
    /// device that admits a writer it cannot see would serve a root that writer
    /// has already superseded — not a stale *answer* that costs a scan, but a
    /// stale *tree*, built on and committed forward. `FileDevice::open`'s
    /// exclusive OS advisory lock is what earns the right to answer `Some`;
    /// `FileDevice::open_read_only`, which takes no lock, answers `None`.
    ///
    /// A fault-injecting simulation device must also answer `None`, for a
    /// second reason: a simulated fault rolls the *readable* image back to the
    /// durable one, so there the file's committed state genuinely can go
    /// backwards under a live handle (this is the schedule AHL-406 reproduces).
    /// A real file cannot do that — a `pwrite` that returned is in the kernel's
    /// page cache until the machine dies, and if it dies the process does too —
    /// which is why the deterministic sweeps keep exercising the derivation
    /// path rather than this one.
    fn commit_point(&self, _region: usize) -> Option<CommitPoint> {
        None
    }

    /// Record the committed state and `region`'s append position, or forget
    /// them.
    ///
    /// `Some` publishes; `None` forgets **everything** this device had cached,
    /// every region included, so the next commit derives from the file again.
    /// Forgetting is what an error inside the gate must do: a commit that fails
    /// part-way through zeroing a wrapped region has changed where records live
    /// without establishing what the new answer is, and a cache that is merely
    /// *unknown* costs a scan where a cache that is *wrong* loses a commit.
    ///
    /// Called only from inside the reservation gate, and only after the writes
    /// the new value describes have already been issued — the same ordering
    /// rule [`Device::end_commit`] follows, for the same reason.
    fn set_commit_point(&self, _region: usize, _point: Option<CommitPoint>) {}

    /// The WAL region assigned to this device handle.
    ///
    /// Single-region and simulation devices use region zero. Native file
    /// handles distribute writers across the format's fixed region set.
    fn wal_region(&self) -> usize {
        0
    }

    /// Whether [`Device::write`] refuses every call — `false` by default, so
    /// no existing device (every one of them writable) has to say anything.
    ///
    /// [`crate::btree::CowBTree::open`] needs this, and *only* this: replaying
    /// the write-ahead-log records the state block is behind means healing
    /// the data area and folding the log forward into a fresh checkpoint,
    /// which is itself a write. On a device that can genuinely never write —
    /// `FileDevice::open_read_only`, in the `inlaysql` crate — attempting
    /// that heal would fail every open that follows any uncheckpointed commit
    /// (the ordinary case; a checkpoint is otherwise only forced when a WAL
    /// region fills), which is to say almost every open. `CowBTree::open`
    /// skips the write and adopts the replayed root, next-page and sequence
    /// counters directly instead — they are already computed by walking the
    /// log records forward, which is a read — and leaves the state block
    /// exactly as behind as it was. Nothing is lost: a data-area page a real
    /// crash left torn (the case the heal exists for) still surfaces safely
    /// as [`crate::error::Error::Corrupt`] on the read that touches it,
    /// exactly as it does when [`crate::btree::CowBTree::refresh`] skips the
    /// same replay for the same reason — see that method's doc comment,
    /// "Why the log records are not replayed here".
    ///
    /// This is deliberately a different question from
    /// [`Device::commit_generation`] returning `None`: a fault-injecting
    /// simulation device also returns `None` there, and it must still take
    /// the ordinary heal-and-checkpoint path on open — that path *is* the
    /// crash recovery the simulation harness exists to exercise. Conflating
    /// the two would silently turn off recovery testing for every `None`
    /// device instead of only the one that is genuinely read-only.
    fn is_read_only(&self) -> bool {
        false
    }

    /// Register this handle as a live reader of the committed tree for as
    /// long as it stays open, returning a token to update or release it
    /// with — or `None` when this device cannot track readers at all.
    ///
    /// This exists for exactly one purpose: the Phase 2 item 6 free list
    /// (`CowBTree`'s page-reuse path) must never hand out a page id some
    /// live root could still reference. "Live" can only be answered for
    /// readers this device can see, which — like every other method on this
    /// trait that claims to speak for the whole file — means in-process
    /// **read-write** handles sharing this device's reservation gate; see the
    /// warning on [`Device::commit_generation`], which applies here with the
    /// same force. A read-only handle (`FileDevice::open_read_only`) takes no
    /// lock and is invisible to this registry by design, in this process or
    /// any other — reclamation must treat that as unprovable, not as "no
    /// readers", and the caller that turns page reuse on is the one
    /// responsible for ruling it out. Every default here is `None`/no-op, so
    /// no existing device has to do anything to stay correct: a device that
    /// says nothing simply never has a page reclaimed on its account.
    fn register_reader(&self) -> Option<u64> {
        None
    }

    /// Record that the reader named by `token` (from
    /// [`Device::register_reader`]) now needs nothing older than `seq`.
    ///
    /// Called every time a handle's committed root actually moves forward
    /// (`CowBTree::commit`, `checkpoint`, `refresh`, a rebase) — never on the
    /// unchanged-nothing-moved fast path, so this costs nothing on the hot
    /// read loop [`Device::commit_generation`] exists to keep cheap.
    fn update_reader(&self, _token: u64, _seq: u64) {}

    /// This reader is gone; forget its watermark. Called once, from
    /// `CowBTree`'s `Drop`.
    fn release_reader(&self, _token: u64) {}

    /// The lowest sequence number any currently-registered reader might
    /// still need, or `None` when nothing is registered on this device, or
    /// this device does not track readers at all.
    ///
    /// `None` here must be read as "unproven", never as "no readers" — see
    /// [`Device::register_reader`]. `CowBTree`'s reclaim logic treats it that
    /// way: nothing is ever reclaimed on the strength of an absent answer.
    fn min_reader_seq(&self) -> Option<u64> {
        None
    }

    /// A handle on this device has opted into page reuse
    /// ([`crate::btree::CowBTree::set_page_reuse`]), so data-area page ids may
    /// now be reissued with new content.
    ///
    /// Any device-level cache of data-area pages keyed by page id or offset
    /// must flush and stay off from this moment: with reuse possible, an
    /// entry can describe the previous occupant of a page, and a lookup that
    /// trusts it serves the wrong bytes with no error anywhere (`super::cache`'s
    /// free-list warning, one level below the decoded cache). This is a
    /// one-way trip — disabling reuse on the handle must not re-enable the
    /// device cache, because it may already hold stale entries.
    ///
    /// The default is to do nothing, so no existing device has to change: a
    /// device that never caches data pages has nothing to flush. Called by
    /// [`crate::btree::CowBTree::set_page_reuse`] exactly when a handle turns
    /// reuse on, before that handle's first commit could possibly reissue an
    /// id, so "check this flag before serving, set it before reuse is
    /// possible" is enough of an ordering proof to need no per-entry version.
    fn note_page_reuse_enabled(&self) {}

    /// A handle on this device has asked for commit-side absorption
    /// (`EngineOptions::commit_absorption`), so a writer that reaches the
    /// reservation gate may offer its open transaction to whoever holds it.
    ///
    /// The same "the device decides for the file" plumbing
    /// [`Device::set_durability`] uses, and for the same reason: absorption
    /// is a property of the gate every handle on the file shares, not of one
    /// handle. The default is a no-op, so every device that says nothing
    /// keeps today's commit protocol exactly — [`Device::absorb_offer`] then
    /// never returns a token and nothing else here is ever reached.
    fn set_commit_absorption(&self, _enabled: bool) {}

    /// Offer this handle's open transaction to whichever writer holds the
    /// reservation gate, returning a token to claim the answer with.
    ///
    /// Called immediately before [`Device::begin_normal_commit`] parks this
    /// thread on the gate, so a leader that sees the offer is seeing a writer
    /// that has already committed to waiting. `ops` is **moved out** when the
    /// device takes the offer and left untouched when it does not, which is
    /// what makes the default free: no clone, no allocation, one `Option`
    /// returned.
    ///
    /// `None` means "not absorbed" — the default, and also what a device that
    /// absorbs answers when the flag is off, when its cohort is already full,
    /// or when it has nothing to gain. The caller then commits exactly as it
    /// always has.
    fn absorb_offer(&self, _root: PageId, _ops: &mut PendingOps) -> Option<u64> {
        None
    }

    /// Block until the leader that this transaction was offered to has an
    /// answer for it, and take the transaction back if the answer needs it.
    ///
    /// This is what an offered writer does **instead of**
    /// [`Device::begin_normal_commit`]: a follower under commit-side
    /// absorption never queues for the reservation gate at all. It is woken
    /// once, with an outcome, strictly after the leader's own barrier
    /// returned — so no member is ever told it committed before the bytes it
    /// depends on are durable.
    ///
    /// The default answers [`AbsorbResult::Fallback`] without waiting, which
    /// is unreachable for a device whose [`Device::absorb_offer`] never hands
    /// out a token — every device that does not absorb.
    ///
    /// `ops` is restored if and only if the answer is
    /// [`AbsorbResult::Fallback`]; every other answer means the leader
    /// consumed the transaction.
    fn absorb_wait(&self, _token: u64, _ops: &mut PendingOps) -> AbsorbResult {
        AbsorbResult::Fallback
    }

    /// Hand the gate holder every transaction currently parked for
    /// absorption, in gate-arrival order, and fix this cohort's membership.
    ///
    /// Called by a writer that holds the gate and has just finished its own
    /// in-gate work. Everything returned here **must** be answered through
    /// [`Device::absorb_resolve`] before the gate is released; the device is
    /// entitled to assume that, and to treat anything left over as a failed
    /// leader.
    ///
    /// The default returns nothing, which is what keeps every non-absorbing
    /// device on today's protocol.
    fn absorb_take(&self) -> Vec<(u64, AbsorbTxn)> {
        Vec::new()
    }

    /// Answer every member of the cohort [`Device::absorb_take`] handed over,
    /// and wake them.
    ///
    /// Called under the reservation gate and, for a cohort that reached the
    /// disk, strictly after the leader's barrier returned. The third element
    /// of each tuple is that member's operations, moved back for an
    /// [`AbsorbResult::Fallback`] and empty for every other answer.
    fn absorb_resolve(&self, _results: Vec<(u64, AbsorbResult, PendingOps)>) {}

    /// [`Device::sync_commit`]'s barrier, for a leader that is still holding
    /// the reservation gate.
    ///
    /// One thing separates it from [`Device::sync_commit`]: it must not wait
    /// for other normal committers to publish their tickets. A leader syncing
    /// inside the gate is itself the only normal committer that can be in
    /// flight, and the gather window `inlaysql`'s coordinator opens on the
    /// ordinary path would spin against its own caller until its yield budget
    /// ran out. Checkpoints already take a non-coalescing barrier for exactly
    /// this reason.
    ///
    /// The default forwards to [`Device::sync_commit`], which is right for
    /// every device that has only one barrier.
    fn sync_commit_in_gate(&mut self) -> Result<()> {
        self.sync_commit()
    }

    /// Whether page ids may already be reused on this device.
    ///
    /// The conservative default is `true`: a device that cannot report a
    /// file-wide reuse state must not let a handle trust a decoded page cache
    /// or retained cursor across another handle's page reuse. Implementations
    /// that own or coordinate the device may return `false` until
    /// [`Device::note_page_reuse_enabled`] is called.
    fn page_reuse_enabled(&self) -> bool {
        true
    }
}

/// A device shared by many trees through reference counting and interior
/// mutability.
///
/// This is what lets several writers (or a writer and a reader) open the same
/// database at once: each tree holds a clone of the `Rc` and sees the same
/// bytes. It is single-threaded (`Rc` is `!Send`); a multi-threaded process
/// would use `Arc<Mutex<…>>` and the same trait. The borrow is held only for
/// the duration of each operation, so a commit's conflict check and its writes
/// are serialized by `RefCell` across trees.
impl<T: Device> Device for Rc<RefCell<T>> {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        self.borrow().read(offset, buf)
    }

    fn read_shared(&self, offset: usize, len: usize) -> Option<Arc<[u8]>> {
        self.borrow().read_shared(offset, len)
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        self.borrow_mut().write(offset, data)
    }

    fn sync(&mut self) -> Result<()> {
        self.borrow_mut().sync()
    }

    fn begin_normal_commit(&self) -> Result<()> {
        self.borrow().begin_normal_commit()
    }

    fn commit_ready(&self) {
        self.borrow().commit_ready();
    }

    fn sync_commit(&mut self) -> Result<()> {
        self.borrow_mut().sync_commit()
    }

    fn set_durability(&self, durability: Durability) {
        self.borrow().set_durability(durability);
    }

    fn begin_commit(&self) -> Result<()> {
        self.borrow().begin_commit()
    }

    fn end_commit(&self) -> Option<u64> {
        self.borrow().end_commit()
    }

    fn end_normal_commit(&self) -> Option<u64> {
        self.borrow().end_normal_commit()
    }

    fn commit_generation(&self) -> Option<u64> {
        self.borrow().commit_generation()
    }

    fn commit_point(&self, region: usize) -> Option<CommitPoint> {
        self.borrow().commit_point(region)
    }

    fn set_commit_point(&self, region: usize, point: Option<CommitPoint>) {
        self.borrow().set_commit_point(region, point);
    }

    fn wal_region(&self) -> usize {
        self.borrow().wal_region()
    }

    fn is_read_only(&self) -> bool {
        self.borrow().is_read_only()
    }

    fn register_reader(&self) -> Option<u64> {
        self.borrow().register_reader()
    }

    fn update_reader(&self, token: u64, seq: u64) {
        self.borrow().update_reader(token, seq);
    }

    fn release_reader(&self, token: u64) {
        self.borrow().release_reader(token);
    }

    fn min_reader_seq(&self) -> Option<u64> {
        self.borrow().min_reader_seq()
    }

    fn note_page_reuse_enabled(&self) {
        self.borrow().note_page_reuse_enabled();
    }

    fn page_reuse_enabled(&self) -> bool {
        self.borrow().page_reuse_enabled()
    }

    fn set_commit_absorption(&self, enabled: bool) {
        self.borrow().set_commit_absorption(enabled);
    }

    fn absorb_offer(&self, root: PageId, ops: &mut PendingOps) -> Option<u64> {
        self.borrow().absorb_offer(root, ops)
    }

    fn absorb_wait(&self, token: u64, ops: &mut PendingOps) -> AbsorbResult {
        self.borrow().absorb_wait(token, ops)
    }

    fn absorb_take(&self) -> Vec<(u64, AbsorbTxn)> {
        self.borrow().absorb_take()
    }

    fn absorb_resolve(&self, results: Vec<(u64, AbsorbResult, PendingOps)>) {
        self.borrow().absorb_resolve(results);
    }

    fn sync_commit_in_gate(&mut self) -> Result<()> {
        self.borrow_mut().sync_commit_in_gate()
    }
}
