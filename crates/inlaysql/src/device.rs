//! A real file behind the core's [`Device`] trait.
//!
//! The core crate is `no_std` and never touches a filesystem; this crate wires
//! its [`Device`] seam to an actual file using positional I/O. Reads and writes
//! are offset-addressed and never seek, which keeps the mapping trivial and
//! leaves the buffering/durability contract exactly as the core expects:
//! writes are visible immediately and become durable on [`FileDevice::sync`].

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use inlaysql_core::btree::Device;
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
    reserved: AtomicBool,
    next_region: AtomicUsize,
    /// How many commits have left the reservation gate on this file.
    ///
    /// This is the whole of [`Device::commit_generation`] for a real file, and
    /// it is authoritative for the reason the `_lock` below exists: while this
    /// coordinator is alive the file is held under an exclusive OS advisory
    /// lock, so no writer outside this process can exist, and every writer
    /// inside it shares this coordinator for its `(dev, ino)`.
    generation: AtomicU64,
    /// A ticket counter for group commit: incremented once for every
    /// [`FileDevice::sync`] call, *after* that call's writes have already
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
}

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

impl CommitCoordinator {
    /// Make everything written up to and including `ticket` durable, batching
    /// concurrent callers into as few real `fsync`/`F_FULLFSYNC` calls as
    /// possible without ever acknowledging a write that call could not have
    /// covered.
    ///
    /// `ticket` must be a value this coordinator's own [`Self::writes_completed`]
    /// has already reached — i.e. taken from the return of a `fetch_add` on it —
    /// and, critically, taken *after* every `write()` the caller's commit made,
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
            let target = self.writes_completed.load(Ordering::SeqCst);
            let result = sync();
            if result.is_ok() {
                self.durable_upto.fetch_max(target, Ordering::SeqCst);
            }
            return result;
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
}

impl Device for FileDevice {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        self.file
            .read_exact_at(buf, offset as u64)
            .map_err(io_error)
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        if self.coordinator.is_none() {
            return Err(self.read_only_error("write"));
        }
        self.file
            .write_all_at(data, offset as u64)
            .map_err(io_error)
    }

    /// Make every write this handle has issued durable — batched with any
    /// other handle committing concurrently on this file via group commit.
    ///
    /// The ticket is taken from [`CommitCoordinator::writes_completed`] here,
    /// on this call, which is what makes the batching sound: every caller
    /// reaches [`Device::sync`] only after its own commit's `write()` calls
    /// have already returned (see `CowBTree::commit` and `checkpoint`, the
    /// only callers), so counting the ticket at the top of this function — not
    /// any earlier — is what lets [`CommitCoordinator::make_durable`] promise
    /// that any `fsync` starting after this ticket is counted covers this
    /// handle's bytes, whoever's `fsync` it turns out to be. On macOS this
    /// still goes through [`File::sync_all`]'s `F_FULLFSYNC` barrier exactly as
    /// before — group commit only decides *which* handle's call performs it,
    /// never whether one happens.
    fn sync(&mut self) -> Result<()> {
        let Some(coordinator) = &self.coordinator else {
            return Err(self.read_only_error("sync"));
        };
        let ticket = coordinator.writes_completed.fetch_add(1, Ordering::SeqCst) + 1;
        let file = &self.file;
        coordinator.make_durable(ticket, || file.sync_all().map_err(io_error))
    }

    /// Refuses on a read-only handle (`coordinator` is `None`) rather than
    /// entering the gate, which is what makes [`FileDevice::end_commit`]
    /// genuinely unreachable there — see its doc comment.
    fn begin_commit(&self) -> Result<()> {
        let Some(coordinator) = &self.coordinator else {
            return Err(self.read_only_error("begin a commit"));
        };
        while coordinator
            .reserved
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::thread::yield_now();
        }
        Ok(())
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
        let generation = coordinator.generation.fetch_add(1, Ordering::AcqRel) + 1;
        coordinator.reserved.store(false, Ordering::Release);
        Some(generation)
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
        reserved: AtomicBool::new(false),
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
        _lock: lock_file,
        readers: Mutex::new(HashMap::new()),
        next_reader_token: AtomicU64::new(1),
    });
    registry.insert(file_id, Arc::downgrade(&coordinator));
    Ok(coordinator)
}

fn io_error(error: io::Error) -> inlaysql_core::Error {
    inlaysql_core::Error::Storage(error.to_string())
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
            reserved: AtomicBool::new(false),
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
            _lock: lock_file,
            readers: Mutex::new(HashMap::new()),
            next_reader_token: AtomicU64::new(1),
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
}
