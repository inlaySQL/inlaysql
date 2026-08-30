//! A real file behind the core's [`Device`] trait.
//!
//! The core crate is `no_std` and never touches a filesystem; this crate wires
//! its [`Device`] seam to an actual file using positional I/O. Reads and writes
//! are offset-addressed and never seek, which keeps the mapping trivial and
//! leaves the buffering/durability contract exactly as the core expects:
//! writes are visible immediately and become durable on [`FileDevice::sync`].

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};

use inlaysql_core::btree::{Device, Durability};
use inlaysql_core::{Error, Result};

/// A byte-addressable file. Reads and writes are positional (`pread`/`pwrite`),
/// and a sync is `fsync`.
///
/// A device opened with [`FileDevice::open`] is read-write and holds
/// `coordinator: Some(_)`, sharing that process's exclusive OS advisory lock
/// on the file (see [`CommitCoordinator`]). A device opened with
/// [`FileDevice::open_read_only`] holds `coordinator: None` instead — it
/// takes no lock, does not participate in commit-generation tracking, and
/// [`Device::write`]/[`Device::sync`]/[`Device::begin_commit`] refuse rather
/// than run. `coordinator.is_none()` is therefore the one place that answers
/// "is this handle read-only?"; nothing else needs to duplicate that state.
pub struct FileDevice {
    file: File,
    coordinator: Option<Arc<CommitCoordinator>>,
    wal_region: usize,
    /// Ticket published by [`Device::commit_ready`] for this handle's normal
    /// commit, consumed by [`Device::sync_commit`]. Zero means no ticket is
    /// waiting; tickets start at one.
    pending_commit_ticket: AtomicU64,
    /// This handle's [`NormalCommitGuard`] between a successful
    /// [`FileDevice::begin_normal_commit`] and the matching
    /// [`FileDevice::end_normal_commit`] — see that guard's doc comment for
    /// why it lives here, as a field, rather than as a local in whichever
    /// function is between the two calls.
    normal_commit_guard: Mutex<Option<NormalCommitGuard>>,
    /// Kept only to name the file in an error message — [`FileDevice`] itself
    /// never needs to re-open or re-derive a path.
    path: PathBuf,
}

/// Process-local reservation state for handles opened on the same file.
///
/// It deliberately protects only conflict checking, sequence/page reservation
/// and WAL placement. `fsync` is outside this gate and may overlap across file
/// handles whose records live in different regions — see [`CommitCoordinator::make_durable`]
/// for how that overlap is turned into group commit: at most one handle
/// actually calls `fsync` at a time, and every other commit whose writes had
/// already reached the file before that call started is durable for free.
///
/// It also owns the process's OS-level advisory lock on the file (`_lock`):
/// one [`File::try_lock`] call, made once when the first [`FileDevice`] for a
/// given `(dev, ino)` is opened in this process, held for as long as any
/// `FileDevice` referencing this coordinator is alive. That is what lets a
/// second *process* opening the same file be refused, while every additional
/// `FileDevice` opened on the same file *within* this process shares the
/// already-held lock instead of contending for its own (an independent
/// `open()` on the same path is a distinct open file description, and OS
/// advisory locks are scoped to the open file description — not the process —
/// so locking per-`FileDevice` would deadlock same-process handles against
/// each other).
struct CommitCoordinator {
    /// Whether one handle is currently preparing and appending a commit.
    /// Waiters sleep on `reservation_done` instead of spinning while the
    /// owner writes its WAL record.
    reserved: Mutex<bool>,
    /// Wakes one waiter when the reservation gate becomes available.
    reservation_done: Condvar,
    /// Normal commits currently waiting to acquire the reservation gate.
    /// Checkpoint callers do not increment this, so a post-commit leader can
    /// distinguish a useful cohort from an in-gate checkpoint.
    normal_waiters: AtomicUsize,
    /// Normal commits that hold the reservation gate and have not released it
    /// yet. This is a bounded coalescing hint, never a durability ticket.
    normal_inflight: AtomicUsize,
    next_region: AtomicUsize,
    /// How many commits have left the reservation gate on this file.
    ///
    /// This is the whole of [`Device::commit_generation`] for a real file, and
    /// it is authoritative for the reason the `_lock` below exists: while this
    /// coordinator is alive the file is held under an exclusive OS advisory
    /// lock, so no writer outside this process can exist, and every writer
    /// inside it shares this coordinator for its `(dev, ino)`.
    generation: AtomicU64,
    /// A ticket counter for group commit: incremented once for every ordinary
    /// sync or successful normal commit, after the writes it covers have
    /// returned from `pwrite`. See [`CommitCoordinator::make_durable`].
    writes_completed: AtomicU64,
    /// The highest [`CommitCoordinator::writes_completed`] ticket known to be
    /// durable — covered by an `fsync`/`F_FULLFSYNC` that was issued after
    /// that ticket was handed out. Monotonic: a slower flush finishing after a
    /// faster one must never move this backwards, which is why it is only
    /// ever updated with [`AtomicU64::fetch_max`].
    durable_upto: AtomicU64,
    /// What the last commit through the reservation gate left behind, so the
    /// next one does not have to re-derive it from the file.
    ///
    /// This is [`Device::commit_point`]'s storage. Read it there for why a
    /// process-local answer is allowed to stand for the file's, and
    /// [`CommitCoordinator::generation`] above for the lock that earns it.
    ///
    /// A `Mutex` rather than atomics because the four fields have to move
    /// together: a reader that saw a new root beside an old append offset would
    /// place its record on top of a live one.
    gate: Mutex<GateCache>,
    /// Leader election for group commit: guards [`FlushState`] and gates the
    /// [`Condvar`] followers wait on.
    flush: Mutex<FlushState>,
    /// Followers wait here for the in-flight leader's flush to finish, then
    /// re-check [`CommitCoordinator::durable_upto`] — see
    /// [`CommitCoordinator::make_durable`].
    flush_done: Condvar,
    /// Diagnostic count of completed flushes. Printed only when
    /// `INLAYSQL_COMMIT_STATS` is set, so the benchmark can explain a
    /// throughput change without making statistics part of the storage API.
    flushes: AtomicU64,
    /// Diagnostic sum of durability tickets covered by completed flushes.
    tickets_flushed: AtomicU64,
    /// Diagnostic count/sum for leaders entered through `sync_commit`.
    normal_flushes: AtomicU64,
    normal_tickets_flushed: AtomicU64,
    /// Held only for its `Drop`: releases the OS lock when the last
    /// `FileDevice` sharing this coordinator goes away. Never read.
    _lock: File,
    /// Every live read-write `CowBTree` handle's watermark: the lowest
    /// sequence number it might still need, keyed by the token
    /// [`Device::register_reader`] handed it.
    ///
    /// This is what lets the free list (Phase 2 item 6) answer "does any
    /// live root in this process still reference page X" without a new
    /// out-of-band protocol: every read-write handle already funnels every
    /// root change through `CowBTree::commit`/`checkpoint`/`refresh`, so
    /// updating its entry here costs one `HashMap` write at exactly the same
    /// points that already pay for a gate acquisition or a device read. A
    /// read-only handle never reaches this map at all — see
    /// [`Device::register_reader`]'s doc comment on `FileDevice`, below.
    readers: Mutex<HashMap<u64, u64>>,
    /// Next token [`Device::register_reader`] hands out on this file.
    next_reader_token: AtomicU64,
    /// Raw data-area page bytes shared by every handle on this file, so a
    /// page is read from the device once per file rather than once per
    /// handle. See [`ReadCache`] for why only data-area pages are cacheable
    /// and how the reuse opt-in turns it off.
    read_cache: RwLock<ReadCache>,
    /// Set once, when any handle sharing this file opts into page reuse; from
    /// then on the raw cache is bypassed and stays off. See
    /// [`Device::note_page_reuse_enabled`] in the core for the contract this
    /// satisfies.
    reuse_enabled: AtomicBool,
    /// The [`Durability`] level [`Device::sync_commit`] uses for this file,
    /// shared by every handle this process has open on it.
    ///
    /// One of [`DURABILITY_UNSET`], [`DURABILITY_NORMAL`] or
    /// [`DURABILITY_FULL`] — never anything else. This is **strongest wins,
    /// for as long as this coordinator is alive**, the same one-way-trip
    /// shape as `reuse_enabled` above but ratcheting toward the *safer*
    /// value instead of always toward `true`: [`FileDevice::set_durability`]
    /// only ever raises it (`fetch_max`), so once any handle sharing this
    /// file has required [`Durability::Full`] — including simply being
    /// opened with the default — every commit on this file stays at `Full`
    /// until every handle sharing this coordinator closes and a fresh one is
    /// created on the next open. Relaxation to [`Durability::Normal`] takes
    /// effect only when every handle that has ever shared this coordinator
    /// asked for it explicitly. `DURABILITY_UNSET` (nobody has asked for
    /// anything, e.g. a caller that builds a `CowBTree` directly without the
    /// `EngineOptions` plumbing) reads as `Full`, so a handle that never
    /// calls [`Device::set_durability`] gets exactly the behaviour it always
    /// had. See `docs/recovery.md` for the justification for "strongest
    /// wins" over the alternative (refusing a second, disagreeing request).
    durability: AtomicU8,
}

/// [`CommitCoordinator::durability`]'s three legal values, encoded so
/// [`AtomicU8::fetch_max`] is the whole of the "strongest wins" ratchet: the
/// order `DURABILITY_UNSET < DURABILITY_NORMAL < DURABILITY_FULL` is exactly
/// "safer never loses to less safe". `DURABILITY_UNSET` and
/// `DURABILITY_FULL` deliberately both mean "use the full-strength barrier"
/// (see [`CommitCoordinator::effective_durability`]) — `UNSET` only exists
/// so a `Normal` request can tell "nobody has asked for anything yet" apart
/// from "somebody already required `Full`", which is what makes the ratchet
/// direction correct rather than a coin flip on registration order.
const DURABILITY_UNSET: u8 = 0;
const DURABILITY_NORMAL: u8 = 1;
const DURABILITY_FULL: u8 = 2;

/// The committed state and per-region append positions the reservation gate
/// would otherwise re-derive from the file, guarded by
/// [`CommitCoordinator::gate`].
///
/// `state` is one value for the whole file; `append` is one per WAL region,
/// because two handles assigned the same region both append to it and each has
/// to see where the other left off. Both start unknown, and either can go back
/// to unknown — see [`Device::set_commit_point`].
#[derive(Default)]
struct GateCache {
    state: Option<(u64, u64, u64)>,
    append: [Option<usize>; inlaysql_core::wal::WAL_REGIONS],
}

/// Group-commit leader-election state, guarded by [`CommitCoordinator::flush`].
struct FlushState {
    /// Whether some handle is currently inside the `fsync` call — i.e. is the
    /// leader of the current flush round.
    in_progress: bool,
    /// Bumped every time a flush round ends (success or failure), so a
    /// follower woken by [`CommitCoordinator::flush_done`] can tell a real
    /// completion from a spurious wakeup and from a *second* round starting
    /// before it got scheduled.
    epoch: u64,
}

/// Raw page bytes shared by every read-write handle on one file.
///
/// This is the read-side counterpart of the commit machinery above: several
/// `Database` handles on one file each keep their own decoded page cache, so
/// every connection re-reads and re-decodes the same pages. This cache holds
/// the *raw* bytes instead, keyed by data offset, so a decoded-cache miss in
/// any handle pays a lookup here before it pays a `pread` — the device read is
/// done once per file, not once per handle, and a freshly opened connection
/// warms up from RAM instead of from the device.
///
/// # What is cacheable, and why this needs no invalidation
///
/// Only the data area is cacheable — the header, the state block and the WAL
/// regions are rewritten in place at every checkpoint and commit, so caching
/// them would serve stale bytes. The boundary is derived from the on-disk
/// layout (page size + format version, learned from the header the tree reads
/// or writes through this device) with the core's own
/// [`inlaysql_core::btree::cache::data_area_page`] arithmetic, and an entry is
/// only ever looked up at or beyond it.
///
/// Within the data area the same argument the decoded cache rests on applies:
/// the tree is copy-on-write and, with page reuse off (the default, and the
/// only configuration this file's server exposes), a page id names one
/// immutable sequence of bytes for the lifetime of the file, so an entry can
/// never go stale and there is nothing to invalidate — no version, no
/// cross-handle protocol. This cache is therefore sound *only* while reuse is
/// off, and [`CommitCoordinator::reuse_enabled`] is the gate that keeps it
/// that way: the moment any handle opts in (a one-way trip), the cache is
/// flushed and bypassed on every lookup. The gate is checked on the read path
/// before any entry is trusted, and it is set before the first commit that
/// could reissue an id, so there is no window in which a stale entry can be
/// served.
///
/// # Cost
///
/// A hit is one shared read-lock acquisition, one map lookup and one
/// `Arc::clone`, paid only where the caller would otherwise issue a `pread`
/// syscall — the decoded cache in front of it means the hot, all-hits path of
/// a warmed handle never reaches here. Eviction is FIFO on the byte budget,
/// deliberately crude: recency ordering is the decoded cache's job, and this
/// cache's only contract is to bound memory.
struct ReadCache {
    /// `(page size, format version)`, learned from the first header this
    /// process's handles observe. `None` until then; nothing is cached before
    /// the boundary is known.
    layout: Option<(usize, u32)>,
    /// Byte budget over raw pages. `0` disables the cache entirely.
    budget: usize,
    /// Bytes currently resident.
    bytes: usize,
    /// Offset → raw page bytes.
    pages: HashMap<u64, Arc<[u8]>>,
    /// Insertion order of resident pages, for FIFO eviction.
    order: VecDeque<u64>,
    /// Lookups that found the offset resident, and lookups that did not —
    /// diagnostics, and how a test proves the cache is actually being used.
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ReadCache {
    fn new(budget: usize) -> Self {
        Self {
            layout: None,
            budget,
            bytes: 0,
            pages: HashMap::new(),
            order: VecDeque::new(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// The byte offset where the data area begins, or `None` until the layout
    /// is known.
    fn boundary(&self) -> Option<usize> {
        let (page_size, version) = self.layout?;
        Some(inlaysql_core::wal::all_regions_end(page_size, version))
    }

    /// Forget every entry and the layout they were read under.
    fn clear(&mut self) {
        self.layout = None;
        self.bytes = 0;
        self.pages.clear();
        self.order.clear();
    }

    /// The resident page at `offset`, if one covers exactly `buf`'s length —
    /// and if `offset` lies at or beyond the data area. The boundary is
    /// enforced here, on every lookup, because a hit below it would serve
    /// stale bytes for a region the tree rewrites in place; keeping the check
    /// beside the data makes a caller that forgets it fail closed.
    fn get(&self, offset: u64, buf: &[u8]) -> Option<Arc<[u8]>> {
        let boundary = self.boundary()?;
        if (offset as usize) < boundary {
            return None;
        }
        let bytes = self.pages.get(&offset)?;
        if bytes.len() != buf.len() {
            return None;
        }
        Some(Arc::clone(bytes))
    }

    /// Make `page` resident at `offset`, evicting FIFO until the budget holds.
    /// `false` (nothing cached) when the offset is below the data area, the
    /// budget is zero, the page is larger than the whole budget, or it is
    /// already resident.
    fn insert(&mut self, offset: u64, page: &[u8]) -> bool {
        if self.budget == 0 || page.is_empty() || page.len() > self.budget {
            return false;
        }
        let Some(boundary) = self.boundary() else {
            return false;
        };
        if (offset as usize) < boundary || self.pages.contains_key(&offset) {
            return false;
        }
        while self.bytes + page.len() > self.budget {
            let Some(evict) = self.order.pop_front() else {
                break;
            };
            if let Some(freed) = self.pages.remove(&evict) {
                self.bytes = self.bytes.saturating_sub(freed.len());
            }
        }
        if self.bytes + page.len() > self.budget {
            return false;
        }
        self.pages.insert(offset, Arc::from(page.to_vec()));
        self.order.push_back(offset);
        self.bytes += page.len();
        true
    }

    /// Record the layout observed in a header, forgetting resident pages if it
    /// disagrees with the layout they were read under.
    fn note_layout(&mut self, layout: (usize, u32)) {
        if self.layout != Some(layout) {
            self.clear();
            self.layout = Some(layout);
        }
    }
}

impl CommitCoordinator {
    /// Make everything written up to and including `ticket` durable, batching
    /// concurrent callers into as few real `fsync`/`F_FULLFSYNC` calls as
    /// possible without ever acknowledging a write that call could not have
    /// covered.
    ///
    /// `ticket` must be a value this coordinator's own [`Self::writes_completed`]
    /// has already reached — i.e. published by `sync` or `commit_ready` — and,
    /// critically, taken *after* every `write()` the caller's commit made,
    /// never before. That ordering is the entire contract: a real `pwrite` is a
    /// synchronous syscall, so by the time it returns its bytes are in the
    /// kernel's page cache and visible to *any* subsequent `fsync` on *any* file
    /// descriptor open on the same file, not only the one that wrote them. So
    /// once a ticket has been counted into `writes_completed`, any `fsync` that
    /// starts afterwards is guaranteed — by the same POSIX guarantee this
    /// file's whole overlapping-fsync design already leans on — to make that
    /// ticket's bytes durable too, whichever handle's `fsync` it is.
    ///
    /// One handle at a time is elected leader (`flush.in_progress`) and calls
    /// `sync`. Every other caller waiting on the same round is a follower: once
    /// the leader's flush completes, a follower whose ticket is `<=` the target
    /// the leader captured *before* calling `sync` is durable for free and
    /// returns without ever touching the disk. A follower whose ticket is
    /// higher — its write landed only after the leader had already committed to
    /// a target — was not covered and loops back to try again, either finding a
    /// new flush has since covered it or becoming the leader of the next round
    /// itself. Either way nothing is ever acknowledged before an `fsync` that
    /// started after its writes finished actually returns success.
    ///
    /// No solo-path cost beyond one ticket and one uncontended mutex lock: with
    /// no concurrent caller, this function's own call always becomes leader
    /// immediately and never reaches [`Condvar::wait`], so a single writer
    /// still fsyncs on its own turn with no batching delay or timeout.
    fn make_durable(&self, ticket: u64, sync: impl FnOnce() -> Result<()>) -> Result<()> {
        self.make_durable_with_cohort(ticket, false, sync)
    }

    /// The post-reservation variant used by a normal user commit. It may give
    /// other normal committers that are already active or queued a few
    /// scheduler turns to publish their tickets, but never waits on the
    /// reservation itself. Checkpoints use [`Self::make_durable`] because they
    /// may be syncing while holding that reservation.
    fn make_commit_durable(&self, ticket: u64, sync: impl FnOnce() -> Result<()>) -> Result<()> {
        self.make_durable_with_cohort(ticket, true, sync)
    }

    fn make_durable_with_cohort(
        &self,
        ticket: u64,
        coalesce_normal_commits: bool,
        sync: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        loop {
            if self.durable_upto.load(Ordering::SeqCst) >= ticket {
                return Ok(());
            }
            let mut flush = self
                .flush
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Re-check under the lock: another thread may have finished a
            // flush that already covers us between the lock-free check above
            // and taking the lock.
            if self.durable_upto.load(Ordering::SeqCst) >= ticket {
                return Ok(());
            }
            if flush.in_progress {
                // Follower: wait for the in-flight round to end, then loop
                // back and re-check. `epoch` distinguishes "this round ended"
                // from a spurious wakeup or a round that already moved on.
                let epoch = flush.epoch;
                while flush.in_progress && flush.epoch == epoch {
                    flush = self
                        .flush_done
                        .wait(flush)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                continue;
            }

            // Leader. Mark the round in progress before releasing the lock so
            // no second thread can also become leader for this round.
            flush.in_progress = true;
            drop(flush);
            // `LeaderGuard` clears `in_progress`, bumps `epoch` and wakes every
            // follower on drop — including on an early return through `?` or
            // an unwind out of `sync` — so a failed or panicking flush can
            // never leave every follower waiting forever.
            let _guard = LeaderGuard { coordinator: self };

            // Captured strictly after this round became the sole leader and
            // strictly before `sync` is called: every ticket counted here
            // already returned from its `write()`, and the `fsync` about to
            // run starts after this load, so it covers every one of them.
            // Our own ticket is always among them, because `writes_completed`
            // already counted it before this function was called.
            if coalesce_normal_commits {
                self.coalesce_normal_commits();
            }
            let durable_before = self.durable_upto.load(Ordering::SeqCst);
            let target = self.writes_completed.load(Ordering::SeqCst);
            let result = sync();
            if result.is_ok() {
                self.durable_upto.fetch_max(target, Ordering::SeqCst);
                let covered = target.saturating_sub(durable_before);
                self.flushes.fetch_add(1, Ordering::Relaxed);
                self.tickets_flushed.fetch_add(covered, Ordering::Relaxed);
                if coalesce_normal_commits {
                    self.normal_flushes.fetch_add(1, Ordering::Relaxed);
                    self.normal_tickets_flushed
                        .fetch_add(covered, Ordering::Relaxed);
                }
            }
            return result;
        }
    }

    /// Give normal commits that are already in the reservation pipeline an
    /// adaptive chance to publish their post-write tickets before this leader
    /// captures its flush target. No wait is taken on the reservation mutex:
    /// a checkpoint may own it and may itself be waiting for this flush to
    /// finish. If no normal commit is active or queued, the solo path takes
    /// no scheduler turn at all — the very first check below fires before any
    /// `yield_now`.
    ///
    /// Unlike a fixed yield count, this window stays open only while writers
    /// are actually still arriving: it keeps yielding as long as a normal
    /// commit is inflight or waiting *and* [`Self::writes_completed`] keeps
    /// advancing. That matters because no real `fsync` is in flight yet
    /// during this window, so every writer this gathers publishes its ticket
    /// at the fast, un-penalized `pwrite` rate instead of the ~18-23x slower
    /// rate a write pays while racing a concurrent `F_FULLFSYNC` on the same
    /// file — and every ticket gathered here is folded into the one upcoming
    /// flush instead of needing a flush of its own.
    ///
    /// It closes as soon as either progress stalls — no new ticket observed
    /// for [`COMMIT_COALESCE_STALL_YIELDS`] consecutive polls — or no normal
    /// commit remains inflight or queued, and it never spends more than
    /// [`COMMIT_COALESCE_MAX_YIELDS`] turns in total, so a cohort that never
    /// stops arriving cannot stall durability indefinitely.
    ///
    /// # Durability ordering
    ///
    /// This only ever *delays* the moment [`Self::make_durable_with_cohort`]
    /// captures its flush `target`, never the other way around — the call
    /// site captures `target = writes_completed.load(..)` strictly after this
    /// function returns. Waiting longer before that capture can only grow the
    /// set of tickets the upcoming `fsync` covers; it can never shrink it or
    /// let a ticket be acknowledged before its bytes were actually written.
    fn coalesce_normal_commits(&self) {
        let mut observed = self.writes_completed.load(Ordering::Acquire);
        let mut stalled = 0usize;
        for _ in 0..COMMIT_COALESCE_MAX_YIELDS {
            if self.normal_inflight.load(Ordering::Acquire) == 0
                && self.normal_waiters.load(Ordering::Acquire) == 0
            {
                return;
            }
            std::thread::yield_now();
            let next = self.writes_completed.load(Ordering::Acquire);
            if next != observed {
                observed = next;
                stalled = 0;
            } else {
                stalled += 1;
                if stalled >= COMMIT_COALESCE_STALL_YIELDS {
                    return;
                }
            }
        }
    }

    /// Raise [`CommitCoordinator::durability`] toward `level` — never lower
    /// it. See the field's doc comment for why this ratchet, rather than
    /// last-write-wins, is the only choice that keeps a handle's `Full`
    /// request meaningful when another handle on the same file asks for
    /// `Normal`, whichever order the two calls arrive in.
    fn set_durability(&self, level: Durability) {
        let target = match level {
            Durability::Full => DURABILITY_FULL,
            Durability::Normal => DURABILITY_NORMAL,
        };
        self.durability.fetch_max(target, Ordering::AcqRel);
    }

    /// The barrier strength [`Device::sync_commit`] should use right now.
    /// `DURABILITY_UNSET` (nobody has called
    /// [`CommitCoordinator::set_durability`] yet) reads as `Full` — see the
    /// field's doc comment.
    fn effective_durability(&self) -> Durability {
        match self.durability.load(Ordering::Acquire) {
            DURABILITY_NORMAL => Durability::Normal,
            _ => Durability::Full,
        }
    }
}

impl Drop for CommitCoordinator {
    fn drop(&mut self) {
        if std::env::var_os("INLAYSQL_COMMIT_STATS").is_some() {
            eprintln!(
                "commit-stats: flushes={} tickets={} normal_flushes={} normal_tickets={}",
                self.flushes.load(Ordering::Relaxed),
                self.tickets_flushed.load(Ordering::Relaxed),
                self.normal_flushes.load(Ordering::Relaxed),
                self.normal_tickets_flushed.load(Ordering::Relaxed),
            );
        }
    }
}

/// Clears [`FlushState::in_progress`] and wakes every follower waiting on
/// [`CommitCoordinator::flush_done`], on every exit path from the leader's
/// section of [`CommitCoordinator::make_durable`] — including a panic inside
/// `sync`, so a poisoned flush can never leave a follower waiting on a round
/// that will never end.
struct LeaderGuard<'a> {
    coordinator: &'a CommitCoordinator,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        let mut flush = self
            .coordinator
            .flush
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        flush.in_progress = false;
        flush.epoch = flush.epoch.wrapping_add(1);
        drop(flush);
        self.coordinator.flush_done.notify_all();
    }
}

/// The complete effect of leaving a normal commit's reservation: bump
/// [`CommitCoordinator::generation`], clear the gate, wake one waiter on
/// [`CommitCoordinator::reservation_done`], and remove this commit from the
/// coalescing hint ([`CommitCoordinator::normal_inflight`]).
///
/// This is its own function — rather than inlined at each call site — so
/// [`NormalCommitGuard::finish`] (the ordinary path, reached through
/// [`FileDevice::end_normal_commit`]) and [`NormalCommitGuard::drop`] (a
/// panic that skipped straight past it) can never disagree about what "done"
/// means. Doing only the counter half here and not the reservation half would
/// trade one bug for a worse one: every later committer on this file would
/// block on [`CommitCoordinator::reservation_done`] forever, since nothing
/// else ever clears [`CommitCoordinator::reserved`].
fn release_normal_reservation(coordinator: &CommitCoordinator) -> u64 {
    let generation = coordinator.generation.fetch_add(1, Ordering::AcqRel) + 1;
    let mut reserved = coordinator
        .reserved
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *reserved = false;
    drop(reserved);
    coordinator.normal_inflight.fetch_sub(1, Ordering::AcqRel);
    coordinator.reservation_done.notify_one();
    generation
}

/// RAII guard for a normal commit's residency in the reservation gate,
/// covering the span [`FileDevice::begin_normal_commit`] opens and
/// [`FileDevice::end_normal_commit`] ordinarily closes.
///
/// Unlike [`LeaderGuard`], which is constructed and dropped within one
/// function's stack frame in this file, the code that can actually panic
/// here — encoding and appending the write-ahead-log record, writing dirty
/// pages — runs in `inlaysql-core`'s `CowBTree::commit`, on the other side of
/// the [`inlaysql_core::btree::Device`] trait, in a different crate. This
/// trait's methods return by value rather than an RAII object, so there is no
/// borrowed-from-`begin_normal_commit` local for that caller to hold. Instead
/// this guard is stashed in [`FileDevice::normal_commit_guard`] — a field,
/// not a local — where it is still reachable, and still gets dropped, when a
/// panic in that other crate unwinds this handle's owning thread. That is not
/// a hypothetical fallback: nothing in this codebase catches such a panic and
/// keeps a `FileDevice` alive past it (there is no `catch_unwind` anywhere in
/// this workspace), so tearing the handle down *is* how a thread that panics
/// mid-commit ends today, and this field's `Drop` runs as part of that.
///
/// Before this guard existed, a panic in that window left
/// [`CommitCoordinator::normal_inflight`] incremented and
/// [`CommitCoordinator::reserved`] stuck at `true` for as long as the file's
/// shared [`CommitCoordinator`] stayed alive — on a long-running server, in
/// practice forever. The stuck reservation would have deadlocked every later
/// committer outright; the stuck counter alone (had the reservation been
/// released some other way) still cost every subsequent flush leader up to
/// the full [`COMMIT_COALESCE_MAX_YIELDS`], because
/// [`CommitCoordinator::coalesce_normal_commits`] reads a nonzero
/// `normal_inflight` as "a cohort is still arriving".
struct NormalCommitGuard {
    coordinator: Arc<CommitCoordinator>,
    finished: bool,
}

impl NormalCommitGuard {
    fn new(coordinator: Arc<CommitCoordinator>) -> Self {
        NormalCommitGuard {
            coordinator,
            finished: false,
        }
    }

    /// The ordinary path: release the reservation and report the generation
    /// it produced, exactly as leaving the gate always has. Consumes the
    /// guard (via `mut self`, marked finished first) so `Drop` never releases
    /// a second time.
    fn finish(mut self) -> u64 {
        self.finished = true;
        release_normal_reservation(&self.coordinator)
    }
}

impl Drop for NormalCommitGuard {
    fn drop(&mut self) {
        if !self.finished {
            release_normal_reservation(&self.coordinator);
        }
    }
}

type FileId = (u64, u64);

/// How many times a lock attempt is retried before the file is reported as
/// held by another process, and how long each retry waits. Sized for a
/// handover — the time it takes another thread to finish dropping a handle —
/// not for waiting out a process that means to keep the file.
///
/// This is defensive: the window it covers is narrow enough that it could not
/// be provoked in a test (see `tests/file_locking.rs`), so it rests on the
/// argument in `coordinator_for` rather than on a reproduction.
const LOCK_ATTEMPTS: u32 = 10;
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

/// Hard ceiling on scheduler turns the adaptive gather window in
/// [`CommitCoordinator::coalesce_normal_commits`] may spend, no matter how
/// many writers keep arriving. It is only entered when another normal commit
/// is active or waiting, and it is never a correctness dependency — a ticket
/// that misses this round simply takes the next flush — but without a ceiling
/// a cohort that never stops arriving could delay every fsync indefinitely.
const COMMIT_COALESCE_MAX_YIELDS: usize = 16384;

/// Consecutive scheduler turns with no new ticket observed before the
/// adaptive gather window in [`CommitCoordinator::coalesce_normal_commits`]
/// decides the cohort has stopped arriving and lets the leader proceed to
/// `fsync`. This is "has anyone just published a ticket", not a backoff
/// schedule, and it has to be large enough to survive real scheduling noise:
/// measurement (`INLAYSQL_COALESCE_DEBUG`, removed before landing) showed a
/// single `yield_now` costs a small fraction of a microsecond, so a small
/// stall count — e.g. the original fixed `8` — reads as "no progress" long
/// before another thread has actually had a turn, which is why the old fixed
/// window never gathered more than one or two extra writers in practice. This
/// value costs nothing when solo (the emptiness check above still fires
/// before any yield), and only ever adds latency when a real cohort is
/// present to amortize an `fsync` over.
const COMMIT_COALESCE_STALL_YIELDS: usize = 1500;

type CoordinatorRegistry = Mutex<HashMap<FileId, Weak<CommitCoordinator>>>;

static COORDINATORS: OnceLock<CoordinatorRegistry> = OnceLock::new();

impl FileDevice {
    /// Open (or create) the file at `path` for reading and writing.
    ///
    /// Takes this process's exclusive OS advisory lock on the file (shared
    /// across every same-process handle by [`coordinator_for`]); a second
    /// process attempting the same is refused. See [`CommitCoordinator`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        let key = (metadata.dev(), metadata.ino());
        let coordinator = coordinator_for(path, key)?;
        let wal_region = coordinator.next_region.fetch_add(1, Ordering::Relaxed)
            % inlaysql_core::wal::WAL_REGIONS;
        Ok(Self {
            file,
            coordinator: Some(coordinator),
            wal_region,
            pending_commit_ticket: AtomicU64::new(0),
            normal_commit_guard: Mutex::new(None),
            path: path.to_path_buf(),
        })
    }

    /// Open the file at `path` for reading only.
    ///
    /// Unlike [`FileDevice::open`] this never creates the file — a missing
    /// path is an error, not a silent empty database — and it takes **no OS
    /// advisory lock at all**. `try_lock`/`try_lock_shared` are scoped to
    /// processes that call them; a process that never calls either can still
    /// open and read the file underneath one that holds an exclusive lock.
    /// That is what lets a read-only handle coexist beside a read-write
    /// handle in another process (or this one), and beside any number of
    /// other read-only handles, with no sidecar file and no change to the
    /// one-file format.
    ///
    /// The trade is that this handle has no proof it is the only writer —
    /// because it never is one. [`Device::commit_generation`] therefore
    /// always answers `None` here (see its doc comment), which makes
    /// [`inlaysql_core::btree::CowBTree::refresh`] fall back to a full
    /// write-ahead-log scan on every statement (~236 µs measured, against
    /// ~7 µs for the fast path a read-write handle gets). That cost buys
    /// correctness: a reader that trusted a value it cannot prove would serve
    /// a snapshot a writer in another process had already moved past,
    /// silently and forever.
    ///
    /// [`Device::write`], [`Device::sync`] and [`Device::begin_commit`] all
    /// refuse with a clear [`inlaysql_core::Error::Storage`] naming the file
    /// rather than panicking or silently doing nothing — so does opening a
    /// file whose header needs write-ahead-log replay to open cleanly, since
    /// recovery is itself a write this handle cannot perform.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(false)
            .open(path)
            .map_err(io_error)?;
        Ok(Self {
            file,
            coordinator: None,
            wal_region: 0,
            pending_commit_ticket: AtomicU64::new(0),
            normal_commit_guard: Mutex::new(None),
            path: path.to_path_buf(),
        })
    }

    /// Whether the file has no bytes yet (a fresh database).
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.file.metadata().map_err(io_error)?.len() == 0)
    }

    fn read_only_error(&self, what: &str) -> Error {
        Error::Storage(format!(
            "{}: this handle is open read-only and cannot {what}",
            self.path.display(),
        ))
    }

    /// Serve `offset..offset+buf.len()` from the shared raw cache when it can
    /// and may. `true` only when `buf` was filled from the cache; every other
    /// outcome — disabled cache, unknown layout, an offset below the data
    /// area, a miss — falls through to a real device read.
    fn read_from_shared_cache(&self, offset: usize, buf: &mut [u8]) -> bool {
        let Some(coordinator) = self.coordinator.as_ref() else {
            return false;
        };
        if coordinator.reuse_enabled.load(Ordering::Acquire) || buf.is_empty() {
            return false;
        }
        let cache = coordinator
            .read_cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(boundary) = cache.boundary() else {
            return false;
        };
        if offset < boundary {
            return false;
        }
        let Some(bytes) = cache.get(offset as u64, buf) else {
            cache.misses.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        cache.hits.fetch_add(1, Ordering::Relaxed);
        drop(cache);
        buf.copy_from_slice(&bytes);
        true
    }

    /// Record a successful device read where it belongs: a header read teaches
    /// the layout; a data-area read may become resident in the shared cache.
    fn fill_shared_cache(&self, offset: usize, buf: &[u8]) {
        let Some(coordinator) = self.coordinator.as_ref() else {
            return;
        };
        if offset == inlaysql_core::wal::header_offset() {
            if let Ok(layout) = inlaysql_core::btree::tree::parse_header(buf) {
                self.note_layout(layout);
            }
            return;
        }
        if coordinator.reuse_enabled.load(Ordering::Acquire) || buf.is_empty() {
            return;
        }
        coordinator
            .read_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(offset as u64, buf);
    }

    /// Teach the shared cache the on-disk layout just observed in a header.
    fn note_layout(&self, layout: (usize, u32)) {
        if let Some(coordinator) = &self.coordinator {
            coordinator
                .read_cache
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .note_layout(layout);
        }
    }

    /// Acquire the shared reservation without assigning a commit kind. The
    /// caller records whether it is a normal commit separately so a flush
    /// leader can ignore checkpoints when deciding whether a cohort exists.
    fn begin_reservation(&self, coordinator: &CommitCoordinator) -> Result<()> {
        let mut reserved = coordinator
            .reserved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *reserved {
            reserved = coordinator
                .reservation_done
                .wait(reserved)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *reserved = true;
        Ok(())
    }

    /// Publish the reservation boundary and optionally remove one normal
    /// commit from the coalescing hint. The generation increment remains
    /// atomic with the boundary, as it was before the normal/checkpoint split.
    ///
    /// The `normal` branch hands off to whatever [`NormalCommitGuard`]
    /// `begin_normal_commit` stashed in [`FileDevice::normal_commit_guard`]
    /// rather than repeating the release inline, so this path and a panic
    /// that skips it both go through [`NormalCommitGuard::finish`] /
    /// [`release_normal_reservation`] and can never disagree.
    fn end_reservation(&self, coordinator: &CommitCoordinator, normal: bool) -> u64 {
        if normal {
            let guard = self
                .normal_commit_guard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            return match guard {
                Some(guard) => guard.finish(),
                None => {
                    // Defensive only: `begin_normal_commit` always stashes a
                    // guard on success, and this is the only place that ever
                    // takes it back out. Falling back to a direct release
                    // still leaves the coordinator correct instead of
                    // silently skipping the decrement if that invariant is
                    // ever broken.
                    release_normal_reservation(coordinator)
                }
            };
        }
        let generation = coordinator.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let mut reserved = coordinator
            .reserved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *reserved = false;
        drop(reserved);
        coordinator.reservation_done.notify_one();
        generation
    }
}

impl Device for FileDevice {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        if self.read_from_shared_cache(offset, buf) {
            return Ok(());
        }
        self.file
            .read_exact_at(buf, offset as u64)
            .map_err(io_error)?;
        self.fill_shared_cache(offset, buf);
        Ok(())
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        if self.coordinator.is_none() {
            return Err(self.read_only_error("write"));
        }
        self.file
            .write_all_at(data, offset as u64)
            .map_err(io_error)?;
        // Creating a fresh database writes the header, so this is where a
        // handle first learns the layout of a file it just made — the read
        // side never sees a header for one.
        if offset == inlaysql_core::wal::header_offset() {
            if let Ok(layout) = inlaysql_core::btree::tree::parse_header(data) {
                self.note_layout(layout);
            }
        }
        Ok(())
    }

    /// Make every write this handle has issued durable — batched with any
    /// other handle committing concurrently on this file via group commit.
    ///
    /// This is the ordinary path for initialization and checkpoints. Its
    /// ticket is taken here, after the caller's writes have returned, so a
    /// flush starting afterward covers those bytes. Normal user commits use
    /// [`Device::commit_ready`] plus [`Device::sync_commit`] instead, which
    /// lets the native coordinator distinguish them from an in-gate
    /// checkpoint. On macOS this still goes through [`File::sync_all`]'s
    /// `F_FULLFSYNC` barrier exactly as before — group commit only decides
    /// which handle's call performs it, never whether one happens.
    ///
    /// # This is never weakened by `Durability`, on purpose
    ///
    /// [`CowBTree::checkpoint`](inlaysql_core::btree::CowBTree::checkpoint)
    /// and the state-block rewrite both call this, immediately before
    /// zeroing or reusing a write-ahead-log region. If this used
    /// [`CommitCoordinator::effective_durability`] the way
    /// [`FileDevice::sync_commit`] does, a `Durability::Normal` file's
    /// checkpoint could zero a region before the writes it depends on had
    /// actually reached the platter — a later power loss would then roll
    /// recovery back past commits a *different* handle was individually told
    /// were durable, however that handle's own commits were synced. That
    /// breaks the documented loss bound instead of merely admitting the loss
    /// the bound already promises (`docs/recovery.md`). Always
    /// `file.sync_all()`, unconditionally — see [`Device::sync`]'s doc
    /// comment in the core for the same argument stated as the trait's
    /// contract.
    fn sync(&mut self) -> Result<()> {
        let Some(coordinator) = &self.coordinator else {
            return Err(self.read_only_error("sync"));
        };
        let ticket = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
        let file = &self.file;
        coordinator.make_durable(ticket, || file.sync_all().map_err(io_error))
    }

    /// Make the normal commit whose writes were marked by
    /// [`Device::commit_ready`] durable. The ticket is already published while
    /// the reservation gate was held, so a concurrent leader can cover this
    /// commit before this handle reaches the call. A missing ticket is a
    /// defensive fallback for a custom caller and keeps the old sync behavior.
    ///
    /// The barrier itself is [`CommitCoordinator::effective_durability`] —
    /// `Durability::Full`'s `file.sync_all()` unless every handle sharing
    /// this file has explicitly asked for `Durability::Normal`; see that
    /// method and [`Device::set_durability`]. This is the *only* place a
    /// `Durability` level changes which syscall runs — [`FileDevice::sync`],
    /// above, never varies.
    fn sync_commit(&mut self) -> Result<()> {
        let Some(coordinator) = &self.coordinator else {
            return Err(self.read_only_error("sync"));
        };
        let ticket = self.pending_commit_ticket.swap(0, Ordering::AcqRel);
        if ticket == 0 {
            return self.sync();
        }
        let file = &self.file;
        let level = coordinator.effective_durability();
        coordinator.make_commit_durable(ticket, || commit_barrier(file, level))
    }

    /// Publish a successful normal commit's durability ticket while its WAL
    /// and data writes are complete. This runs before the reservation gate is
    /// released so a leader already flushing can still cover the ticket.
    fn commit_ready(&self) {
        let Some(coordinator) = &self.coordinator else {
            unreachable!("a read-only FileDevice cannot publish a commit ticket");
        };
        let ticket = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
        self.pending_commit_ticket.store(ticket, Ordering::Release);
    }

    /// Refuses on a read-only handle (`coordinator` is `None`) rather than
    /// entering the gate, which is what makes [`FileDevice::end_commit`]
    /// genuinely unreachable there — see its doc comment.
    fn begin_commit(&self) -> Result<()> {
        let Some(coordinator) = &self.coordinator else {
            return Err(self.read_only_error("begin a commit"));
        };
        self.begin_reservation(coordinator)
    }

    /// Acquire the reservation for a normal user commit and advertise that a
    /// normal committer is active. Checkpoints continue to use `begin_commit`
    /// and therefore do not make a post-commit leader wait on them.
    ///
    /// `normal_waiters` is guarded by a small scope guard rather than the
    /// bare `fetch_add`/`fetch_sub` pair this replaced, so a panic inside
    /// `begin_reservation` — none is reachable today, but nothing here
    /// depends on that staying true — cannot leave it elevated either.
    /// `normal_inflight` gets the sturdier [`NormalCommitGuard`] instead of a
    /// matching scope guard because its region does not end inside this
    /// function; see that type's doc comment.
    fn begin_normal_commit(&self) -> Result<()> {
        let Some(coordinator) = &self.coordinator else {
            return Err(self.read_only_error("begin a commit"));
        };
        coordinator.normal_waiters.fetch_add(1, Ordering::AcqRel);
        struct WaiterGuard<'a>(&'a CommitCoordinator);
        impl Drop for WaiterGuard<'_> {
            fn drop(&mut self) {
                self.0.normal_waiters.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let waiter_guard = WaiterGuard(coordinator);
        let result = self.begin_reservation(coordinator);
        drop(waiter_guard);
        if result.is_ok() {
            coordinator.normal_inflight.fetch_add(1, Ordering::Release);
            *self
                .normal_commit_guard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(NormalCommitGuard::new(Arc::clone(coordinator)));
        }
        result
    }

    /// Count the commit, then release the gate — in that order.
    ///
    /// The record this commit appended was written inside the gate, so by the
    /// time the counter moves the bytes are already readable by any other
    /// handle. A reader that sees the old value has simply not been overtaken
    /// yet; a reader that sees the new one will scan and find the record.
    /// Incrementing on entry instead would invert that and let a reader scan
    /// the log *before* the record was written while recording the generation
    /// that promises it already had — the one failure mode that serves stale
    /// data rather than costing a scan.
    ///
    /// It counts commit *attempts*, not commits: a conflict and a checkpoint
    /// both pass through here and both move it. That is deliberate — an extra
    /// increment costs one wasted scan, a missing one is a wrong answer.
    ///
    /// Only reachable on a read-write handle: [`FileDevice::begin_commit`]
    /// refuses before this point on a read-only one (`coordinator` is
    /// `None`), so there is never a gate here to leave. `unreachable!` says so
    /// loudly rather than fabricating a generation for a commit that cannot
    /// have happened.
    fn end_commit(&self) -> Option<u64> {
        let Some(coordinator) = &self.coordinator else {
            unreachable!(
                "a read-only FileDevice's begin_commit always fails, so a commit \
                 can never reach end_commit"
            );
        };
        Some(self.end_reservation(coordinator, false))
    }

    /// Leave a normal user-commit reservation and remove it from the
    /// coalescing hint. The generation remains the same boundary used by
    /// readers and recovery.
    fn end_normal_commit(&self) -> Option<u64> {
        let Some(coordinator) = &self.coordinator else {
            unreachable!(
                "a read-only FileDevice's begin_normal_commit always fails, so a commit \
                 can never reach end_normal_commit"
            );
        };
        Some(self.end_reservation(coordinator, true))
    }

    /// See [`Device::commit_generation`]'s warning. On a read-write handle
    /// this is sound because [`coordinator_for`] holds an exclusive OS
    /// advisory lock on the file for as long as the handle is open: a second
    /// process is refused rather than admitted, so an in-process counter can
    /// speak for every writer there is.
    ///
    /// A read-only handle (`coordinator` is `None`, from
    /// [`FileDevice::open_read_only`]) takes no such lock — that is the
    /// entire point of it — so a writer in another process can commit at any
    /// moment without this counter ever moving. Answering `Some` there would
    /// let [`inlaysql_core::btree::CowBTree::refresh`] trust a snapshot that
    /// process has already moved past, forever and silently. So this returns
    /// `None` unconditionally for a read-only handle, which is exactly the
    /// case `coordinator.is_none()` selects: every statement re-derives the
    /// committed state from the file instead of trusting a counter that
    /// cannot speak for writers outside this process.
    fn commit_generation(&self) -> Option<u64> {
        self.coordinator
            .as_ref()
            .map(|coordinator| coordinator.generation.load(Ordering::Acquire))
    }

    /// Answers from [`CommitCoordinator::gate`] on a read-write handle, and
    /// `None` on a read-only one — the same split, for the same reason, as
    /// [`FileDevice::commit_generation`]: a handle that took no OS lock cannot
    /// speak for a writer in another process, and here the consequence of
    /// trying would not be a stale answer but a stale *tree*.
    fn commit_point(&self, region: usize) -> Option<inlaysql_core::btree::CommitPoint> {
        let coordinator = self.coordinator.as_ref()?;
        let gate = coordinator
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (root, next, seq) = gate.state?;
        let append_offset = *gate.append.get(region)?;
        Some(inlaysql_core::btree::CommitPoint {
            root,
            next,
            seq,
            append_offset: append_offset?,
        })
    }

    fn set_commit_point(&self, region: usize, point: Option<inlaysql_core::btree::CommitPoint>) {
        let Some(coordinator) = self.coordinator.as_ref() else {
            return;
        };
        let mut gate = coordinator
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match point {
            Some(point) => {
                gate.state = Some((point.root, point.next, point.seq));
                if let Some(slot) = gate.append.get_mut(region) {
                    *slot = Some(point.append_offset);
                }
            }
            // Forgetting is deliberately total: the caller only knows that
            // *its* region moved under it, but it stopped part-way through a
            // sequence the committed state itself depends on, so the honest
            // answer everywhere is "read the file".
            None => *gate = GateCache::default(),
        }
    }

    fn wal_region(&self) -> usize {
        self.wal_region
    }

    /// `true` for a handle from [`FileDevice::open_read_only`] — see
    /// [`Device::is_read_only`] for why [`CowBTree::open`] needs to know, and
    /// this doc comment for why every read-write device (the only kind that
    /// existed before AHL-405) is unaffected: `coordinator.is_none()` is
    /// exactly, and only, the read-only case.
    ///
    /// [`CowBTree::open`]: inlaysql_core::btree::CowBTree::open
    fn is_read_only(&self) -> bool {
        self.coordinator.is_none()
    }

    /// `Some(token)` on a read-write handle, `None` on a read-only one —
    /// deliberately the same split as [`FileDevice::commit_generation`], and
    /// for a related but distinct reason: a read-only handle takes no OS
    /// lock and answers nothing this process's reservation gate can prove,
    /// so it is invisible to the map [`CommitCoordinator::readers`] backs.
    /// That is not a gap this method can close — see
    /// [`Device::register_reader`]'s doc comment — it is why page reuse
    /// (`CowBTree`'s free list) must stay opt-in and documented as unsound
    /// beside a concurrent `FileDevice::open_read_only` on the same file.
    fn register_reader(&self) -> Option<u64> {
        let coordinator = self.coordinator.as_ref()?;
        let token = coordinator
            .next_reader_token
            .fetch_add(1, Ordering::Relaxed);
        coordinator
            .readers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(token, 0);
        Some(token)
    }

    fn update_reader(&self, token: u64, seq: u64) {
        let Some(coordinator) = &self.coordinator else {
            return;
        };
        coordinator
            .readers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(token, seq);
    }

    fn release_reader(&self, token: u64) {
        let Some(coordinator) = &self.coordinator else {
            return;
        };
        coordinator
            .readers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&token);
    }

    fn min_reader_seq(&self) -> Option<u64> {
        let coordinator = self.coordinator.as_ref()?;
        coordinator
            .readers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .copied()
            .min()
    }

    /// Flush the shared raw-page cache and gate it off — page ids may now be
    /// reissued, so every resident entry may describe a page's previous
    /// occupant. One-way: entries flushed here can never be trusted again,
    /// and nothing this process later learns can change that.
    fn note_page_reuse_enabled(&self) {
        let Some(coordinator) = &self.coordinator else {
            return;
        };
        coordinator.reuse_enabled.store(true, Ordering::Release);
        coordinator
            .read_cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    fn page_reuse_enabled(&self) -> bool {
        self.coordinator
            .as_ref()
            .is_none_or(|coordinator| coordinator.reuse_enabled.load(Ordering::Acquire))
    }

    /// Raise this file's [`Device::sync_commit`] barrier toward `durability`
    /// — see [`CommitCoordinator::durability`] for the "strongest wins,
    /// one-way for this coordinator's lifetime" ratchet this feeds, and
    /// `EngineOptions::durability`(inlaysql_core::EngineOptions) for the
    /// caller-facing argument for why that is the safe default reading of
    /// two handles on one file disagreeing.
    ///
    /// A no-op on a read-only handle: [`FileDevice::sync`] and
    /// [`FileDevice::sync_commit`] already refuse before either could reach
    /// a barrier, so there is nothing here for a read-only handle to affect.
    fn set_durability(&self, durability: Durability) {
        let Some(coordinator) = &self.coordinator else {
            return;
        };
        coordinator.set_durability(durability);
    }
}

fn coordinator_for(path: &Path, file_id: FileId) -> Result<Arc<CommitCoordinator>> {
    let registry = COORDINATORS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = registry.get(&file_id).and_then(Weak::upgrade) {
        // Another handle in this process already holds the coordinator (and
        // with it, the OS lock below) for this file. Share it rather than
        // taking a second, independent lock that would contend with our own.
        return Ok(existing);
    }

    // First handle on this `(dev, ino)` in this process: take the OS-level
    // advisory lock on a dedicated handle, held by the coordinator for as
    // long as any `FileDevice` referencing it is alive. A second process
    // attempting the same is refused rather than left to block forever.
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io_error)?;

    // `WouldBlock` does not always mean another process. This process can also
    // collide with *itself*: `Weak::upgrade` starts failing the moment the last
    // `Arc<CommitCoordinator>` drops its strong count to zero, which is before
    // the thread doing that drop has closed the coordinator's lock file. A
    // thread reopening the database in that window would be told another
    // process holds it, which is both wrong and confusing — and a server that
    // opens one handle per connection sits in exactly that window whenever the
    // connection count touches zero.
    //
    // A brief retry closes it. It also rides out the same handover between two
    // processes, where the answer would otherwise depend on scheduling.
    let mut attempts = 0;
    loop {
        match lock_file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if attempts < LOCK_ATTEMPTS => {
                attempts += 1;
                drop(registry);
                std::thread::sleep(LOCK_RETRY_DELAY);
                registry = COORDINATORS
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Another thread may have won the race and installed a
                // coordinator while the registry was unlocked.
                if let Some(existing) = registry.get(&file_id).and_then(Weak::upgrade) {
                    return Ok(existing);
                }
            }
            Err(TryLockError::WouldBlock) => {
                return Err(inlaysql_core::Error::Storage(format!(
                    "{}: another process already has this database file open for writing",
                    path.display(),
                )));
            }
            Err(TryLockError::Error(err)) => return Err(io_error(err)),
        }
    }

    let coordinator = Arc::new(CommitCoordinator {
        reserved: Mutex::new(false),
        reservation_done: Condvar::new(),
        normal_waiters: AtomicUsize::new(0),
        normal_inflight: AtomicUsize::new(0),
        next_region: AtomicUsize::new(0),
        generation: AtomicU64::new(0),
        writes_completed: AtomicU64::new(0),
        durable_upto: AtomicU64::new(0),
        gate: Mutex::new(GateCache::default()),
        flush: Mutex::new(FlushState {
            in_progress: false,
            epoch: 0,
        }),
        flush_done: Condvar::new(),
        flushes: AtomicU64::new(0),
        tickets_flushed: AtomicU64::new(0),
        normal_flushes: AtomicU64::new(0),
        normal_tickets_flushed: AtomicU64::new(0),
        _lock: lock_file,
        readers: Mutex::new(HashMap::new()),
        next_reader_token: AtomicU64::new(1),
        read_cache: RwLock::new(ReadCache::new(shared_read_cache_budget())),
        reuse_enabled: AtomicBool::new(false),
        durability: AtomicU8::new(DURABILITY_UNSET),
    });
    registry.insert(file_id, Arc::downgrade(&coordinator));
    Ok(coordinator)
}

/// The shared raw-page cache budget for this process's handles on one file —
/// [`inlaysql_core::btree::DEFAULT_PAGE_CACHE_BYTES`] unless the
/// `INLAYSQL_DISABLE_SHARED_READ_CACHE` environment variable is set, which
/// pins it to zero and restores the read-from-the-device-every-time behaviour.
///
/// The variable exists so the same benchmark binary can measure the cache on
/// and off across two process runs; it is a diagnostic knob, not a supported
/// configuration surface. Read once, when the first handle on a file is
/// opened.
fn shared_read_cache_budget() -> usize {
    if std::env::var_os("INLAYSQL_DISABLE_SHARED_READ_CACHE").is_some() {
        0
    } else {
        inlaysql_core::btree::DEFAULT_PAGE_CACHE_BYTES
    }
}

fn io_error(error: io::Error) -> inlaysql_core::Error {
    inlaysql_core::Error::Storage(error.to_string())
}

/// The syscall [`FileDevice::sync_commit`] runs for `level`.
///
/// `Durability::Full` is `file.sync_all()` — bit-for-bit the call every
/// existing caller already made, never touched by this function's `Normal`
/// arm. `Durability::Normal` is a strictly weaker, platform-specific
/// barrier; see `platform_normal_sync`'s per-`cfg` doc comments below for
/// the mapping and why each platform needs what it needs.
fn commit_barrier(file: &File, level: Durability) -> Result<()> {
    match level {
        Durability::Full => file.sync_all().map_err(io_error),
        Durability::Normal => platform_normal_sync(file),
    }
}

/// `Durability::Normal` on macOS: plain `fsync(2)`, not `F_FULLFSYNC`.
///
/// `std::fs::File::sync_all()` and even `File::sync_data()` both route
/// through `fcntl(F_FULLFSYNC)` on this platform — measured directly, not
/// assumed: both cost ~3-4ms against this project's reference SSD, matching
/// `PERF.md`'s `F_FULLFSYNC` numbers, where a real plain `fsync(2)` costs
/// tens of microseconds. Rust's standard library does not expose the weaker
/// call at all here, deliberately — Apple's own documentation says plain
/// `fsync` does not guarantee a media flush, so std treats "sync" as meaning
/// the strong barrier on this platform. Getting the weaker, real `fsync(2)`
/// therefore needs the actual syscall, wrapped safely by `rustix` rather
/// than an `unsafe` block in this `#![forbid(unsafe_code)]` crate.
#[cfg(target_os = "macos")]
fn platform_normal_sync(file: &File) -> Result<()> {
    rustix::fs::fsync(file).map_err(|errno| io_error(errno.into()))
}

/// `Durability::Normal` on Linux: `fdatasync`, a real weaker barrier the
/// kernel supports directly (it skips the metadata update `fsync` also
/// flushes, when only the file's size and permissions are unchanged — true
/// for every write this engine issues in place). Routed through `rustix`
/// rather than `std::fs::File::sync_data()` so both platforms go through one
/// implementation instead of trusting two different standard-library
/// mappings — see the macOS arm's doc comment for why that trust would be
/// misplaced there.
#[cfg(target_os = "linux")]
fn platform_normal_sync(file: &File) -> Result<()> {
    rustix::fs::fdatasync(file).map_err(|errno| io_error(errno.into()))
}

/// `Durability::Normal` on every other target: no validated weaker barrier
/// exists here, so fall back to the full-strength one rather than guess at a
/// syscall nobody has measured on this platform. See `docs/recovery.md`.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_normal_sync(file: &File) -> Result<()> {
    file.sync_all().map_err(io_error)
}

/// White-box tests of [`CommitCoordinator::make_durable`] — the ordering rule
/// group commit stands or falls on. These construct a bare `CommitCoordinator`
/// directly (no [`FileDevice`], no real WAL) and drive it with fake `sync`
/// closures whose only job is to record whether they ran, so the tests can
/// assert the *decision* — skip or fsync-for-yourself — independently of any
/// real file. The end-to-end proof that N real concurrent commits all survive
/// a reopen lives in `tests/concurrent_writers.rs`.
#[cfg(test)]
mod group_commit_tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// A `CommitCoordinator` with a throwaway lock file — no real advisory
    /// lock is taken, since these tests never go through [`coordinator_for`].
    fn test_coordinator(name: &str) -> CommitCoordinator {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-group-commit-test-{name}-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let _ = std::fs::remove_file(&path);
        CommitCoordinator {
            reserved: Mutex::new(false),
            reservation_done: Condvar::new(),
            normal_waiters: AtomicUsize::new(0),
            normal_inflight: AtomicUsize::new(0),
            next_region: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            writes_completed: AtomicU64::new(0),
            durable_upto: AtomicU64::new(0),
            gate: Mutex::new(GateCache::default()),
            flush: Mutex::new(FlushState {
                in_progress: false,
                epoch: 0,
            }),
            flush_done: Condvar::new(),
            flushes: AtomicU64::new(0),
            tickets_flushed: AtomicU64::new(0),
            normal_flushes: AtomicU64::new(0),
            normal_tickets_flushed: AtomicU64::new(0),
            _lock: lock_file,
            readers: Mutex::new(HashMap::new()),
            next_reader_token: AtomicU64::new(1),
            read_cache: RwLock::new(ReadCache::new(1 << 20)),
            reuse_enabled: AtomicBool::new(false),
            durability: AtomicU8::new(DURABILITY_UNSET),
        }
    }

    /// A write already covered by an earlier flush's target must return
    /// durable without ever calling its own `sync` — this is the batching
    /// half of the contract: a follower whose bytes were already on the file
    /// before some flush's `fsync` began does not pay for a second one.
    #[test]
    fn a_ticket_already_covered_by_an_earlier_flush_never_calls_its_own_sync() {
        let coordinator = test_coordinator("covered");

        // Two writes complete (their `write()` calls returned) before any
        // flush happens — modelling two commits whose pages/records are both
        // already in the file.
        let ticket_f = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
        let ticket_g = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!((ticket_f, ticket_g), (1, 2));

        // G leads a flush. Its target is captured after both writes above
        // completed, so it covers both.
        let g_flushed = AtomicBool::new(false);
        coordinator
            .make_durable(ticket_g, || {
                g_flushed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        assert!(g_flushed.load(Ordering::SeqCst));

        // F asks separately. Its ticket is already <= durable_upto, so its
        // closure must never run — proving the skip path is real, not just
        // that the call happens to return `Ok`.
        let f_flushed = AtomicBool::new(false);
        coordinator
            .make_durable(ticket_f, || {
                f_flushed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        assert!(
            !f_flushed.load(Ordering::SeqCst),
            "a write already covered by an earlier flush's target must not fsync again"
        );
    }

    /// The other half of the contract: a write that only lands *after* a
    /// leader has already captured its flush target must never be
    /// acknowledged on the strength of that flush — it has to fsync for
    /// itself (or wait for a later one). This is deterministic, not
    /// timing-dependent: channels pin the follower's ticket to strictly after
    /// the leader's target capture.
    #[test]
    fn a_write_that_lands_after_the_leader_captured_its_target_still_fsyncs_itself() {
        let coordinator = test_coordinator("not-covered");

        let (target_captured_tx, target_captured_rx) = mpsc::channel::<()>();
        let (release_leader_tx, release_leader_rx) = mpsc::channel::<()>();
        let leader_flushed = Arc::new(AtomicBool::new(false));
        let follower_flushed = Arc::new(AtomicBool::new(false));

        let ticket_leader = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(ticket_leader, 1);

        thread::scope(|scope| {
            // A `Copy`able shared reference, so every closure below can be
            // marked `move` (required once it also moves other captures,
            // like the channel endpoints) without fighting over ownership of
            // `coordinator` itself.
            let coordinator = &coordinator;
            let leader_flushed_inner = Arc::clone(&leader_flushed);
            let leader = scope.spawn(move || {
                coordinator.make_durable(ticket_leader, move || {
                    // Signal that the target has been captured (it is
                    // captured immediately before this closure runs, with
                    // nothing able to intervene) and block until told to
                    // finish, simulating a flush still in flight.
                    target_captured_tx.send(()).unwrap();
                    release_leader_rx.recv().unwrap();
                    leader_flushed_inner.store(true, Ordering::SeqCst);
                    Ok(())
                })
            });

            // Wait until the leader's target is captured. At this instant
            // `writes_completed == 1`: the follower's ticket does not exist
            // yet, so no flush target computed up to now can cover it.
            target_captured_rx.recv().unwrap();

            let ticket_follower = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
            assert_eq!(ticket_follower, 2);

            let follower_flushed_inner = Arc::clone(&follower_flushed);
            let follower = scope.spawn(move || {
                coordinator.make_durable(ticket_follower, move || {
                    follower_flushed_inner.store(true, Ordering::SeqCst);
                    Ok(())
                })
            });

            // Let the leader's flush finish. Its target was 1, so it cannot
            // satisfy the follower's ticket of 2.
            release_leader_tx.send(()).unwrap();

            leader.join().unwrap().unwrap();
            follower.join().unwrap().unwrap();
        });

        assert!(leader_flushed.load(Ordering::SeqCst));
        assert!(
            follower_flushed.load(Ordering::SeqCst),
            "a write that only completed after the leader had already captured \
             its fsync target must fsync for itself, never be acknowledged on \
             the strength of a flush that could not have covered it"
        );
    }

    /// A flush that fails must not poison the coordinator: every follower
    /// waiting on it wakes, finds it still uncovered, and gets a real chance
    /// to fsync (and fail, or succeed) for itself rather than hanging or
    /// being falsely told it is durable.
    #[test]
    fn a_failed_leader_flush_still_wakes_followers_who_then_fsync_for_themselves() {
        let coordinator = test_coordinator("failed-leader");

        let ticket_leader = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
        let leader_result = coordinator.make_durable(ticket_leader, || {
            Err(Error::Storage("simulated fsync failure".to_string()))
        });
        assert!(leader_result.is_err());

        // The failure must not have advanced durable_upto, and must not have
        // left the coordinator's flush marked in progress forever.
        let ticket_follower = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
        let follower_flushed = AtomicBool::new(false);
        coordinator
            .make_durable(ticket_follower, || {
                follower_flushed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        assert!(
            follower_flushed.load(Ordering::SeqCst),
            "a commit after a failed flush must still be able to become leader \
             and fsync for itself"
        );
    }

    /// Normal commits use the post-gate ticket path, while checkpoints keep
    /// using the ordinary in-gate sync. This is the boundary that prevents a
    /// post-commit cohort from ever waiting on a checkpoint that is waiting on
    /// the cohort's flush.
    #[test]
    fn normal_commits_and_checkpoints_use_separate_flush_paths() {
        use inlaysql_core::btree::{CowBTree, DEFAULT_PAGE_SIZE};

        let path = std::env::temp_dir().join(format!(
            "inlaysql-normal-commit-paths-{}-{}.inlay",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::remove_file(&path);

        let device = FileDevice::open(&path).expect("open");
        let coordinator = Arc::clone(device.coordinator.as_ref().expect("coordinator"));
        let mut tree = CowBTree::open_or_create(device, DEFAULT_PAGE_SIZE).expect("create");
        tree.put(b"key", b"value").expect("put");
        tree.commit().expect("normal commit");

        assert_eq!(
            coordinator.normal_flushes.load(Ordering::Relaxed),
            1,
            "the normal commit must use the post-gate flush path"
        );
        assert_eq!(
            coordinator.normal_inflight.load(Ordering::Acquire),
            0,
            "the normal reservation hint must be cleared after commit"
        );

        tree.checkpoint().expect("checkpoint");
        assert_eq!(
            coordinator.normal_flushes.load(Ordering::Relaxed),
            1,
            "the checkpoint's in-gate sync must not enter the normal cohort"
        );

        drop(tree);
        drop(coordinator);
        let _ = std::fs::remove_file(&path);
    }

    /// The test above proves normal commits and checkpoints use separate
    /// flush paths, but runs them sequentially — it never proves what
    /// happens when a checkpoint's own `write_state` → `device.sync()`
    /// arrives while a normal commit is *already* flush-leading.
    ///
    /// That interleaving matters because a checkpoint holds the reservation
    /// gate across its own in-gate sync (`CowBTree::checkpoint`, unlike a
    /// normal commit, does not release the gate until *after* its sync
    /// returns). If that sync sees `flush.in_progress` already set, it
    /// becomes a follower on `flush_done` — while still holding the gate.
    /// Every other normal commit that starts in that window piles into
    /// `normal_waiters`, which the real leader's `coalesce_normal_commits`
    /// reads as "cohort still arriving", even though none of those waiters
    /// can publish a ticket until the checkpoint releases the gate it is
    /// itself stuck behind. Not a deadlock — `coalesce_normal_commits`'s own
    /// stall detector (`COMMIT_COALESCE_STALL_YIELDS`) notices
    /// `writes_completed` has stopped moving and breaks the leader out,
    /// letting its real flush run, which wakes the checkpoint, which becomes
    /// leader of its own flush, which finally releases the gate — but until
    /// this test, nothing ever exercised it.
    ///
    /// Real `fsync` timing cannot be trusted to land this deterministically,
    /// so this pins the interleaving the same way the leader/follower tests
    /// above do: a fake "leader" drives `CommitCoordinator::make_durable_with_cohort`
    /// directly, through a closure this test controls, standing in for a real
    /// normal commit's `sync_commit`.
    ///
    /// 1. The fake leader takes `flush.in_progress` and blocks.
    /// 2. A real checkpoint starts, takes the reservation gate — confirmed by
    ///    a bounded poll before this test proceeds, so the next step can
    ///    never race it — then its own `sync()` must see the fake leader in
    ///    progress and become a follower without ever releasing the gate.
    /// 3. `WAITERS` real normal commits, each on its own handle, start only
    ///    after the gate is confirmed held, so every one of them is forced to
    ///    pile into `normal_waiters` rather than possibly slipping through
    ///    first depending on how the scheduler happens to run this test.
    /// 4. Once every one of them is actually counted — again a bounded poll,
    ///    not a fixed sleep — the fake leader is released, proving the
    ///    leader really did keep the cohort waiting on it and not the other
    ///    way around.
    ///
    /// The assertions that matter are on outcome, not timing: every commit
    /// and the checkpoint must succeed, every row a commit was told it
    /// committed must survive a fresh handle, and the coordinator's counters
    /// and reservation gate must be back at rest afterward — never a torn,
    /// partial or invented row, and never a counter or gate left stuck by the
    /// interleaving. The one timing assertion is a deliberately loose overall
    /// ceiling: generous enough not to be flaky on a loaded runner, but tight
    /// enough to fail loudly on a genuine hang instead of leaving the test
    /// binary to time out on its own with no useful message.
    #[test]
    fn a_checkpoint_concurrent_with_a_normal_commit_still_makes_progress() {
        use inlaysql_core::btree::{CommitOutcome, CowBTree, DEFAULT_PAGE_SIZE};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        const WAITERS: usize = 6;
        const POLL_TIMEOUT: Duration = Duration::from_secs(10);
        const OVERALL_CEILING: Duration = Duration::from_secs(30);

        let path = std::env::temp_dir().join(format!(
            "inlaysql-checkpoint-concurrent-commit-{}-{}.inlay",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::remove_file(&path);

        // Seeded and dropped before the race starts: `CowBTree` holds a
        // thread-local (`Rc`-based) page cache internally, so it is not
        // `Send` and cannot be built here and moved into a spawned thread —
        // every handle below, the checkpoint's included, is opened fresh on
        // the thread that uses it, exactly like the writers already have to
        // be.
        let seed_device = FileDevice::open(&path).expect("open seed handle");
        let coordinator = Arc::clone(seed_device.coordinator.as_ref().expect("coordinator"));
        let mut seed_tree =
            CowBTree::open_or_create(seed_device, DEFAULT_PAGE_SIZE).expect("create");
        seed_tree.put(b"seed", b"value").expect("seed put");
        seed_tree.commit().expect("seed commit");
        drop(seed_tree);

        let started = Instant::now();
        let (leading_tx, leading_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let leader_flushed = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let coordinator: &CommitCoordinator = &coordinator;
            let leader_flushed = &leader_flushed;

            // The fake leader: becomes flush leader for real, through the
            // exact `make_durable_with_cohort(_, coalesce_normal_commits:
            // true, _)` path a real normal commit's `sync_commit` uses, and
            // blocks there until this test releases it.
            let leader = scope.spawn(move || {
                let ticket = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
                coordinator.make_durable_with_cohort(ticket, true, || {
                    leading_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    leader_flushed.store(true, Ordering::SeqCst);
                    Ok(())
                })
            });

            leading_rx
                .recv()
                .expect("the fake leader must signal before this test proceeds");

            // The checkpoint: takes the reservation gate, then its own
            // `sync()` must see the fake leader in progress and become a
            // follower — without ever releasing the gate first.
            let checkpoint = scope.spawn({
                let path = path.clone();
                move || {
                    let device = FileDevice::open(&path).expect("open checkpoint handle");
                    let mut tree = CowBTree::open_or_create(device, DEFAULT_PAGE_SIZE)
                        .expect("open checkpoint tree");
                    tree.checkpoint()
                }
            });

            // Confirmed, not assumed: the checkpoint must actually be
            // holding the gate before a single writer is spawned below, or a
            // writer could slip through first depending on how the scheduler
            // happens to interleave the two spawns — which would make the
            // rest of this test flaky rather than deterministic.
            let gate_deadline = Instant::now() + POLL_TIMEOUT;
            loop {
                if *coordinator
                    .reserved
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                {
                    break;
                }
                assert!(
                    Instant::now() < gate_deadline,
                    "the checkpoint never took the reservation gate"
                );
                std::thread::yield_now();
            }

            // `WAITERS` real normal commits, each its own handle, piling
            // into `normal_waiters` behind the checkpoint's held gate.
            let mut writers = Vec::new();
            for index in 0..WAITERS {
                let path = path.clone();
                writers.push(scope.spawn(move || {
                    let device = FileDevice::open(&path).expect("open writer handle");
                    let mut tree = CowBTree::open_or_create(device, DEFAULT_PAGE_SIZE)
                        .expect("open writer tree");
                    let key = format!("writer-{index}").into_bytes();
                    tree.put(&key, b"value").expect("writer put");
                    let outcome = tree.commit().expect("writer commit");
                    (key, outcome)
                }));
            }

            // Wait for every one of them to actually be counted as waiting
            // on the gate the checkpoint holds — proving the pile-up really
            // happened, not just that the threads were spawned.
            let waiters_deadline = Instant::now() + POLL_TIMEOUT;
            loop {
                if coordinator.normal_waiters.load(Ordering::Acquire) >= WAITERS {
                    break;
                }
                assert!(
                    Instant::now() < waiters_deadline,
                    "writers never piled into normal_waiters behind the checkpoint's \
                     held reservation gate — the race this test exists to exercise \
                     never set up"
                );
                std::thread::yield_now();
            }

            // Everything downstream — the checkpoint waking as a follower,
            // then becoming its own leader once the fake leader's flush does
            // not cover its ticket, then releasing the gate, then each
            // writer taking its turn — happens on its own from here.
            release_tx.send(()).unwrap();

            let leader_result = leader.join().unwrap();
            assert!(
                leader_result.is_ok(),
                "the fake leader's flush must succeed"
            );
            assert!(
                leader_flushed.load(Ordering::SeqCst),
                "the fake leader's closure must actually have run"
            );

            checkpoint.join().unwrap().expect(
                "the checkpoint must succeed despite becoming a follower while \
                 holding the reservation gate",
            );

            let mut committed_keys = Vec::new();
            for writer in writers {
                let (key, outcome) = writer.join().unwrap();
                assert_eq!(
                    outcome,
                    CommitOutcome::Committed,
                    "every writer used a disjoint key and must never conflict"
                );
                committed_keys.push(key);
            }

            assert!(
                started.elapsed() < OVERALL_CEILING,
                "a checkpoint concurrent with normal commits took {:?} — the stall \
                 detector should break the leader out in microseconds, not this",
                started.elapsed()
            );

            // Progress and correctness: every committed key survives a fresh
            // handle — never torn, partial or invented.
            let reader_device = FileDevice::open(&path).expect("open reader");
            let reader = CowBTree::open_or_create(reader_device, DEFAULT_PAGE_SIZE)
                .expect("open reader tree");
            for key in &committed_keys {
                let value = reader.get(key).expect("read committed key").expect(
                    "a key a writer was told it committed must be readable \
                             from a fresh handle",
                );
                assert_eq!(&value[..], b"value");
            }
            drop(reader);

            // And the coordinator itself is back at rest.
            assert_eq!(
                coordinator.normal_inflight.load(Ordering::Acquire),
                0,
                "normal_inflight must return to zero once every commit has left the gate"
            );
            assert_eq!(
                coordinator.normal_waiters.load(Ordering::Acquire),
                0,
                "normal_waiters must return to zero once every commit has left the gate"
            );
            assert!(
                !*coordinator
                    .reserved
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                "the reservation gate must be free once every commit and the \
                 checkpoint have left it"
            );
        });

        drop(coordinator);
        let _ = std::fs::remove_file(&path);
    }

    /// Before [`NormalCommitGuard`] existed, a panic between
    /// `begin_normal_commit` and `end_normal_commit` left `normal_inflight`
    /// incremented and `reserved` stuck at `true` for as long as this file's
    /// shared `CommitCoordinator` stayed alive — every later committer would
    /// have deadlocked on the stuck reservation, and even had that not been
    /// true, `coalesce_normal_commits` would have read the stuck counter as
    /// "a cohort is still arriving" on every subsequent flush, forever.
    ///
    /// This reproduces exactly that window: `device` is moved into a closure
    /// that begins a normal commit and then panics before ever calling
    /// `end_normal_commit`, which is what a connection thread panicking
    /// mid-commit looks like in this codebase today — nothing here catches
    /// such a panic and keeps the handle alive past it, so the handle (and
    /// its stashed [`NormalCommitGuard`]) is torn down by the unwind. The
    /// white-box read of `normal_inflight` and `reserved` afterward proves
    /// the guard's `Drop` ran and put the coordinator back the way a
    /// successful `end_normal_commit` would have.
    ///
    /// (This test's panic message and backtrace on stderr are expected —
    /// `catch_unwind` still lets the default panic hook run before it
    /// catches the unwind.)
    #[test]
    fn a_panic_between_begin_and_end_normal_commit_does_not_leak_the_inflight_counter() {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-normal-commit-panic-{}-{}.inlay",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::remove_file(&path);

        let device = FileDevice::open(&path).expect("open");
        let coordinator = Arc::clone(device.coordinator.as_ref().expect("coordinator"));
        let coordinator_in_closure = Arc::clone(&coordinator);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            device.begin_normal_commit().expect("begin normal commit");
            assert_eq!(
                coordinator_in_closure
                    .normal_inflight
                    .load(Ordering::Acquire),
                1,
                "begin_normal_commit must mark the reservation held before this \
                 test panics on it"
            );
            assert!(
                *coordinator_in_closure
                    .reserved
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                "begin_normal_commit must hold the reservation gate before this \
                 test panics on it"
            );
            panic!(
                "simulated panic between begin_normal_commit and \
                 end_normal_commit — end_normal_commit is deliberately never \
                 reached"
            );
            // `device` — and the `NormalCommitGuard` it stashed — is dropped
            // right here as this closure's frame unwinds.
        }));
        assert!(
            result.is_err(),
            "the closure above must have actually panicked, or this test proves \
             nothing"
        );

        assert_eq!(
            coordinator.normal_inflight.load(Ordering::Acquire),
            0,
            "a panic between begin_normal_commit and end_normal_commit must not \
             leave normal_inflight incremented forever"
        );
        assert!(
            !*coordinator
                .reserved
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            "a panic between begin_normal_commit and end_normal_commit must not \
             leave the reservation gate held forever — every later committer on \
             this file would deadlock waiting for it"
        );

        drop(coordinator);
        let _ = std::fs::remove_file(&path);
    }

    /// Nobody has called [`CommitCoordinator::set_durability`] yet:
    /// `DURABILITY_UNSET` must read as `Durability::Full`, so a handle built
    /// without the `EngineOptions` plumbing (or any caller that predates this
    /// option) gets exactly the barrier it always got.
    #[test]
    fn an_untouched_coordinator_is_full_strength() {
        let coordinator = test_coordinator("untouched");
        assert_eq!(coordinator.effective_durability(), Durability::Full);
    }

    /// The whole point of the ratchet: a `Normal` request alone relaxes the
    /// file, but only while nothing else on it has required `Full`.
    #[test]
    fn a_lone_normal_request_relaxes_the_coordinator() {
        let coordinator = test_coordinator("lone-normal");
        coordinator.set_durability(Durability::Normal);
        assert_eq!(coordinator.effective_durability(), Durability::Normal);
    }

    /// Strongest wins, `Full`-after-`Normal`: once a second handle on the
    /// same file requires `Full`, the file stays at `Full` — the first
    /// handle's `Normal` request never downgrades it back.
    #[test]
    fn a_full_request_after_normal_pins_the_coordinator_to_full() {
        let coordinator = test_coordinator("normal-then-full");
        coordinator.set_durability(Durability::Normal);
        assert_eq!(coordinator.effective_durability(), Durability::Normal);
        coordinator.set_durability(Durability::Full);
        assert_eq!(coordinator.effective_durability(), Durability::Full);
        // And it stays pinned even if the `Normal` handle asks again.
        coordinator.set_durability(Durability::Normal);
        assert_eq!(
            coordinator.effective_durability(),
            Durability::Full,
            "a later Normal request must not undo an earlier Full pin"
        );
    }

    /// Strongest wins, `Full`-before-`Normal`: the order the two requests
    /// arrive in must not matter — this is what makes a default-`Full`
    /// handle opened first on a file safe against a second handle that later
    /// asks for `Normal`.
    #[test]
    fn a_full_request_before_normal_also_pins_the_coordinator_to_full() {
        let coordinator = test_coordinator("full-then-normal");
        coordinator.set_durability(Durability::Full);
        coordinator.set_durability(Durability::Normal);
        assert_eq!(coordinator.effective_durability(), Durability::Full);
    }
}

/// Tests of the shared raw-page read cache: `ReadCache` in isolation, then the
/// whole `FileDevice` path — the boundary discipline that keeps the header,
/// state block and WAL regions uncached, population through a real tree, and
/// the page-reuse gate that flushes and disables it.
#[cfg(test)]
mod shared_read_cache_tests {
    use super::*;
    use inlaysql_core::btree::{CowBTree, DEFAULT_PAGE_SIZE, FORMAT_VERSION};

    fn cache_with_budget(budget: usize) -> ReadCache {
        let mut cache = ReadCache::new(budget);
        cache.note_layout((DEFAULT_PAGE_SIZE, FORMAT_VERSION));
        cache
    }

    fn boundary(cache: &ReadCache) -> usize {
        cache.boundary().expect("layout must be set")
    }

    #[test]
    fn a_resident_data_area_page_is_served_again() {
        let mut cache = cache_with_budget(1 << 16);
        let page = vec![0xabu8; DEFAULT_PAGE_SIZE];
        let offset = boundary(&cache);
        assert!(
            cache.insert(offset as u64, &page),
            "data-area page must cache"
        );

        let mut buf = vec![0u8; DEFAULT_PAGE_SIZE];
        let served = cache
            .get(offset as u64, &buf)
            .expect("page must be resident");
        buf.copy_from_slice(&served);
        assert_eq!(buf, page);
    }

    #[test]
    fn nothing_below_the_data_area_is_ever_cached_or_served() {
        let mut cache = cache_with_budget(1 << 16);
        let offset = boundary(&cache);
        let page = vec![0u8; DEFAULT_PAGE_SIZE];
        // The header, the state block and every WAL region live below the
        // data area and are rewritten in place; serving them from a cache
        // would be exactly the stale-read bug this boundary exists for.
        for bad in [0usize, 1, offset - 1] {
            assert!(
                !cache.insert(bad as u64, &page),
                "offset {bad} must not cache"
            );
            assert!(
                cache.get(bad as u64, &page).is_none(),
                "offset {bad} must never be served"
            );
        }
        assert_eq!(cache.bytes, 0);
        assert!(cache.pages.is_empty());
    }

    #[test]
    fn a_zero_budget_caches_nothing() {
        let mut cache = cache_with_budget(0);
        assert!(!cache.insert(boundary(&cache) as u64, &vec![0u8; DEFAULT_PAGE_SIZE]));
        assert!(cache
            .get(boundary(&cache) as u64, &vec![0u8; DEFAULT_PAGE_SIZE])
            .is_none());
    }

    #[test]
    fn a_page_larger_than_the_whole_budget_is_not_cached() {
        let mut cache = cache_with_budget(128);
        let page = vec![0u8; DEFAULT_PAGE_SIZE];
        assert!(!cache.insert(boundary(&cache) as u64, &page));
        assert!(cache.pages.is_empty());
    }

    #[test]
    fn eviction_is_fifo_and_respects_the_budget() {
        let mut cache = cache_with_budget(2 * DEFAULT_PAGE_SIZE);
        let base = boundary(&cache) as u64;
        for i in 0..3u64 {
            let page = vec![i as u8; DEFAULT_PAGE_SIZE];
            assert!(cache.insert(base + i * DEFAULT_PAGE_SIZE as u64, &page));
        }
        assert!(cache.bytes <= cache.budget);
        // The oldest entry went first.
        assert!(cache.get(base, &vec![0u8; DEFAULT_PAGE_SIZE]).is_none());
        for i in 1..3u64 {
            assert!(cache
                .get(
                    base + i * DEFAULT_PAGE_SIZE as u64,
                    &vec![0u8; DEFAULT_PAGE_SIZE]
                )
                .is_some());
        }
    }

    #[test]
    fn a_layout_change_forgets_everything_read_under_the_old_layout() {
        let mut cache = cache_with_budget(1 << 16);
        let offset = boundary(&cache) as u64;
        assert!(cache.insert(offset, &vec![1u8; DEFAULT_PAGE_SIZE]));
        // The file at this identity was replaced by one with a different page
        // size: pages cached under the old layout must never be served.
        cache.note_layout((DEFAULT_PAGE_SIZE * 2, FORMAT_VERSION));
        assert!(cache.pages.is_empty());
        assert_eq!(cache.bytes, 0);
        assert!(cache.get(offset, &vec![0u8; DEFAULT_PAGE_SIZE]).is_none());
    }

    /// End to end: a tree populates the shared cache as it reads, and a
    /// second device handle on the same file is served from it instead of
    /// re-reading.
    #[test]
    fn the_tree_populates_the_shared_cache_and_a_second_handle_reads_from_it() {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-shared-cache-populate-{}.inlay",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        {
            let device = FileDevice::open(&path).expect("open");
            // Held for the whole test: the coordinator (and with it the OS
            // lock and the shared cache) lives only as long as some handle
            // references it — exactly the role `Server::run`'s keeper
            // `Database` plays for the MySQL server.
            let keeper = device.coordinator.clone().expect("rw handle");
            let mut tree = CowBTree::open_or_create(device, DEFAULT_PAGE_SIZE).expect("create");
            for key in 0..64 {
                tree.put(format!("key-{key}").as_bytes(), b"value")
                    .expect("put");
            }
            tree.commit().expect("commit");
            tree.get(b"key-7").expect("read populates the cache");
            drop(tree);
            {
                let cache = keeper.read_cache.read().unwrap();
                assert!(
                    cache.bytes > 0,
                    "the first handle's reads must have populated the shared cache"
                );
            }
        }

        let device = FileDevice::open(&path).expect("reopen");
        let mut header = [0u8; 24];
        device.read(0, &mut header).expect("header");
        let (page_size, version) =
            inlaysql_core::btree::tree::parse_header(&header).expect("layout");
        let coordinator = device.coordinator.as_ref().expect("rw handle");
        let offset = inlaysql_core::wal::data_offset_for(page_size, version, 1);
        let mut first = vec![0u8; page_size];
        device.read(offset, &mut first).expect("read");
        let cache = coordinator.read_cache.read().unwrap();
        assert_eq!(
            cache.hits.load(Ordering::Relaxed),
            0,
            "the first read of this offset by this handle is a miss"
        );
        drop(cache);

        let mut second = vec![0u8; page_size];
        device.read(offset, &mut second).expect("read");
        assert_eq!(first, second);
        let cache = coordinator.read_cache.read().unwrap();
        assert_eq!(
            cache.hits.load(Ordering::Relaxed),
            1,
            "the second read of the same offset must be served from the cache"
        );
        drop(cache);

        drop(device);
        let _ = std::fs::remove_file(&path);
    }

    /// The one event that invalidates the whole design — a handle opting into
    /// page reuse — flushes the shared cache and gates it off, so a later
    /// reissue of a page id can never be served its previous occupant.
    #[test]
    fn the_reuse_opt_in_flushes_and_gates_the_shared_cache() {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-shared-cache-reuse-{}.inlay",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let device = FileDevice::open(&path).expect("open");
        let coordinator = device.coordinator.clone().expect("rw handle");
        let mut tree = CowBTree::open_or_create(device, DEFAULT_PAGE_SIZE).expect("create");
        tree.put(b"key", b"value").expect("put");
        tree.commit().expect("commit");
        tree.get(b"key").expect("read populates the cache");
        {
            let cache = coordinator.read_cache.read().unwrap();
            assert!(
                cache.bytes > 0,
                "the read above must have populated the cache"
            );
        }

        tree.set_page_reuse(true);
        assert!(coordinator.reuse_enabled.load(Ordering::Acquire));
        {
            let cache = coordinator.read_cache.read().unwrap();
            assert_eq!(cache.bytes, 0, "the reuse opt-in must flush the cache");
            assert!(cache.pages.is_empty());
        }

        // A later data-area read through another handle must not repopulate it.
        let device2 = FileDevice::open(&path).expect("reopen");
        let mut header = [0u8; 24];
        device2.read(0, &mut header).expect("header");
        let (page_size, version) =
            inlaysql_core::btree::tree::parse_header(&header).expect("layout");
        let offset = inlaysql_core::wal::data_offset_for(page_size, version, 1);
        let mut buf = vec![0u8; page_size];
        device2.read(offset, &mut buf).expect("read");
        {
            let cache = coordinator.read_cache.read().unwrap();
            assert!(
                cache.pages.is_empty(),
                "once reuse is on, the cache must stay off"
            );
        }
        drop(device2);
        drop(tree);
        let _ = std::fs::remove_file(&path);
    }
}
