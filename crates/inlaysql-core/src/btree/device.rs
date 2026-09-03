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

/// What the gate holder decided about one parked transaction: the same
/// answer `rebase_pending` returns, computed by somebody else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbsorbOutcome {
    /// No key this transaction touched changed under it. It may rebase.
    Clean,
    /// A key it touched was changed by an earlier committer, so
    /// first-committer-wins aborts it — exactly
    /// [`crate::btree::CommitOutcome::Conflict`].
    Conflict,
}

/// Where a decision sits in one leader's chain, and what the file's committed
/// state must be for that decision to still be an answer to the right
/// question.
///
/// A decision is computed under one gate hold and used under a later one, so
/// something has to rule out everything that can happen in between: an
/// outsider commits, a checkpoint lands, an earlier member of the same cohort
/// fails on a device error while somebody else takes the sequence number it
/// was going to use. All three fields together do it, and `seq` alone does
/// not — a `Clean` member that never commits while an outsider commits in its
/// place leaves the sequence number exactly where the chain expected it and
/// the *content* somewhere else. Only a member acting on cohort `cohort`'s
/// decision at position `index` ever publishes `index + 1`, so the pair pins
/// the identity of every commit in between, not merely how many there were.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsorbSeal {
    /// Which leader's cohort this position belongs to. Never reused.
    pub cohort: u64,
    /// How many members of that cohort have already resolved.
    pub index: u32,
    /// The file's highest committed sequence number at that point.
    pub seq: u64,
}

/// One parked transaction's decision, and the state it is an answer for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsorbDecision {
    /// The seal the device must be showing for [`AbsorbDecision::outcome`] to
    /// be used. Anything else — including `None` — means the file moved in a
    /// way the leader did not predict, and the follower does the ordinary
    /// full `rebase_pending` instead, which is always correct.
    pub expect: AbsorbSeal,
    /// What the leader decided.
    pub outcome: AbsorbOutcome,
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
/// in the deterministic simulation — and forwards the five `absorb_*`/seal
/// methods of [`Device`] straight into it. That is deliberate: the part of
/// this protocol that is easy to get wrong is the chain arithmetic in
/// [`AbsorbQueue::cohort`], and a second copy of it in the simulation would
/// mean the deterministic sweeps proved a different implementation correct
/// than the one production runs.
///
/// Nothing here is `Send` or `Sync` by itself; the owning device supplies
/// that, along with the ordering. Every method but [`AbsorbQueue::offer`] is
/// called by a thread holding the commit reservation gate, so they are
/// serialized against each other already; `offer` is called by a thread one
/// instant before it parks on that gate, and the owner's lock is what orders
/// it against a leader's [`AbsorbQueue::cohort`].
#[derive(Debug, Default)]
pub struct AbsorbQueue {
    /// Whether any handle sharing this device asked for absorption. A device
    /// sets it once and never clears it: the gate belongs to the file, so
    /// "some writers on this file may be judged and some may not" is not a
    /// state worth having.
    pub enabled: bool,
    next_token: u64,
    next_cohort: u64,
    /// Offered and not yet taken by a leader, in gate-arrival order.
    parked: Vec<(u64, AbsorbTxn)>,
    /// Taken and decided, waiting for their own thread to claim them back.
    ///
    /// An entry orphaned by a writer that panicked between offering and
    /// claiming stays here until the device is dropped. That is memory, not
    /// correctness: the operations it holds belong to a transaction whose
    /// handle is unwinding, and nothing else can ever name its token.
    judged: BTreeMap<u64, (AbsorbTxn, Option<AbsorbDecision>)>,
    seal: Option<AbsorbSeal>,
    /// Cohorts formed and members judged over this device's lifetime, so a
    /// test can assert that absorption actually happened rather than assume
    /// it — the discipline `pages_reused` exists for on the free list.
    pub cohorts: u64,
    /// Transactions judged as part of one of those cohorts. See
    /// [`AbsorbQueue::cohorts`].
    pub members: u64,
}

impl AbsorbQueue {
    /// Most transactions one gate holder will judge in a single cohort.
    ///
    /// A bound rather than a tuning knob: the leader runs one key-set
    /// comparison per member inside its own gate hold, so an unbounded cohort
    /// would let one leader hold the gate for arbitrarily long on other
    /// writers' behalf. A writer arriving at a full cohort is simply not
    /// offered and commits exactly as it does today.
    pub const COHORT_MAX: usize = 32;

    /// [`Device::absorb_offer`]. Leaves `ops` untouched when it answers
    /// `None`, so the disabled path costs nothing but the check.
    pub fn offer(&mut self, root: PageId, ops: &mut PendingOps) -> Option<u64> {
        if !self.enabled || self.parked.len() >= Self::COHORT_MAX {
            return None;
        }
        self.next_token += 1;
        let token = self.next_token;
        let ops = core::mem::take(ops);
        self.parked.push((token, AbsorbTxn { root, ops }));
        Some(token)
    }

    /// [`Device::absorb_claim`]. The operations come home whether or not
    /// anyone judged them — from `judged` if a leader took the offer, and
    /// otherwise straight back out of the queue this thread put them in.
    pub fn claim(&mut self, token: u64, ops: &mut PendingOps) -> Option<AbsorbDecision> {
        if let Some((txn, decision)) = self.judged.remove(&token) {
            *ops = txn.ops;
            return decision;
        }
        if let Some(index) = self.parked.iter().position(|(id, _)| *id == token) {
            let (_, txn) = self.parked.remove(index);
            *ops = txn.ops;
        }
        None
    }

    /// [`Device::absorb_cohort`]: fix this cohort's membership, judge it, and
    /// file one decision per member.
    ///
    /// Membership is fixed by the drain, exactly as the flush side fixes its
    /// cohort by snapshotting `writes_completed` strictly before the barrier —
    /// a writer that parks after this point belongs to somebody else's cohort.
    pub fn cohort(&mut self, seq: u64, decide: &mut dyn FnMut(&[AbsorbTxn]) -> Vec<AbsorbOutcome>) {
        if !self.enabled || self.parked.is_empty() {
            return;
        }
        self.next_cohort += 1;
        let cohort = self.next_cohort;
        let (tokens, txns): (Vec<u64>, Vec<AbsorbTxn>) =
            core::mem::take(&mut self.parked).into_iter().unzip();
        let outcomes = decide(&txns);
        // An answer of the wrong length is "no decisions", and every member
        // falls back to the rebase it would have done anyway. That is also how
        // a caller declines a cohort it cannot judge, so getting this wrong
        // costs throughput and never correctness.
        let judged = outcomes.len() == txns.len();
        // The chain the leader is predicting. Only a member that *commits*
        // moves the file's sequence number on; a conflicting member resolves
        // its position without changing the file, which is the whole of the
        // difference between `index` and `chain_seq`.
        let mut index = 0u32;
        let mut chain_seq = seq;
        for (token, txn) in tokens.into_iter().zip(txns) {
            let decision = judged.then(|| {
                let outcome = outcomes[index as usize];
                index += 1;
                let expect = AbsorbSeal {
                    cohort,
                    index,
                    seq: chain_seq,
                };
                if outcome == AbsorbOutcome::Clean {
                    chain_seq += 1;
                }
                AbsorbDecision { expect, outcome }
            });
            self.judged.insert(token, (txn, decision));
        }
        if judged {
            self.cohorts += 1;
            self.members += u64::from(index);
        }
        // Published only once every member is filed, so a follower that wakes
        // early can never find a seal naming a decision that is not there yet.
        self.seal = judged.then_some(AbsorbSeal {
            cohort,
            index: 1,
            seq,
        });
    }

    /// [`Device::absorption_seal`].
    pub fn seal(&self) -> Option<AbsorbSeal> {
        self.seal
    }

    /// [`Device::set_absorption_seal`]. A device that was never asked for
    /// absorption keeps `None` forever, which matches no decision.
    pub fn set_seal(&mut self, seal: Option<AbsorbSeal>) {
        if self.enabled {
            self.seal = seal;
        }
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

    /// Take back the transaction offered under `token`, with the gate
    /// holder's decision if one was made.
    ///
    /// Called once, after [`Device::begin_normal_commit`] returns — success
    /// or failure — because the ops belong to the caller's handle and must
    /// come home either way. `None` means no leader reached this offer, and
    /// the caller runs the ordinary `rebase_pending`.
    fn absorb_claim(&self, _token: u64, _ops: &mut PendingOps) -> Option<AbsorbDecision> {
        None
    }

    /// Hand the gate holder every transaction currently parked for
    /// absorption, in gate-arrival order, and file the decision it makes for
    /// each.
    ///
    /// Called by a writer that has just published its own [`CommitPoint`] and
    /// still holds the gate; `seq` is the sequence number it committed at.
    /// The whole cohort arrives as one slice rather than one call per member
    /// so the caller can fold a *logical overlay* forward across it — member
    /// `j` has to be judged against member `j - 1`'s post-rebase root, which
    /// does not exist yet and never will under this slice, so it is answered
    /// from the earlier members' own operations instead. See
    /// `docs/research/commit-group-slice1.md` §1.
    ///
    /// `decide` returns one outcome per transaction, in the same order. A
    /// return of any other length is treated as "no decision" and every
    /// member falls back, so a caller that gets this wrong loses performance
    /// and never correctness.
    ///
    /// The implementation is responsible for assigning the cohort id and for
    /// publishing the leader's own [`AbsorbSeal`] — the caller does not know
    /// what cohort it just created.
    fn absorb_cohort(
        &self,
        _seq: u64,
        _decide: &mut dyn FnMut(&[AbsorbTxn]) -> Vec<AbsorbOutcome>,
    ) {
    }

    /// The absorption chain position the file's committed state is currently
    /// at, or `None` when the last thing to change it was not a chain member.
    ///
    /// Read under the reservation gate, and compared for equality against
    /// [`AbsorbDecision::expect`] — all three fields. Answering `None`, the
    /// default, is what makes every non-absorbing device refuse every
    /// decision it could never have been handed anyway.
    fn absorption_seal(&self) -> Option<AbsorbSeal> {
        None
    }

    /// Publish the absorption chain position this commit leaves behind.
    ///
    /// Called under the reservation gate by **every** commit that reaches it,
    /// and by [`crate::btree::CowBTree::checkpoint`]: `Some` from a member
    /// that acted on its own decision, `None` from everything else — an
    /// ordinary commit, a conflict that did not come from a decision, a
    /// checkpoint, an error inside the gate. That is what makes a stale
    /// decision impossible to use rather than merely unlikely: anything the
    /// leader did not predict clears the seal, and a cleared seal matches no
    /// `expect`.
    fn set_absorption_seal(&self, _seal: Option<AbsorbSeal>) {}

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

    fn absorb_claim(&self, token: u64, ops: &mut PendingOps) -> Option<AbsorbDecision> {
        self.borrow().absorb_claim(token, ops)
    }

    fn absorb_cohort(&self, seq: u64, decide: &mut dyn FnMut(&[AbsorbTxn]) -> Vec<AbsorbOutcome>) {
        self.borrow().absorb_cohort(seq, decide);
    }

    fn absorption_seal(&self) -> Option<AbsorbSeal> {
        self.borrow().absorption_seal()
    }

    fn set_absorption_seal(&self, seal: Option<AbsorbSeal>) {
        self.borrow().set_absorption_seal(seal);
    }
}
