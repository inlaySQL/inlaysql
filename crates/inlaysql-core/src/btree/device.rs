//! The seam between the copy-on-write B-tree and a byte-addressable device.
//!
//! The tree never talks to a real disk directly. It reads and writes through a
//! [`Device`], which is the same trick the rest of the core uses for the clock
//! and the indexes: production wiring points at a real file, and the
//! deterministic test wiring points at [`crate::sim::SimDisk`] (or a
//! fault-injecting [`crate::sim::Simulator`]). That is what lets the whole
//! engine run, crash and recover under the simulation harness.

use alloc::rc::Rc;
use core::cell::RefCell;

use crate::btree::PageId;
use crate::error::Result;

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

/// A byte-addressable, randomly-accessible durable store.
///
/// Offsets are in bytes. Implementations are expected to buffer writes until
/// [`Device::sync`] is called, exactly like the operating system buffers writes
/// in the page cache until `fsync`; the simulation harness relies on that
/// property to model crashes.
pub trait Device {
    /// Read `buf.len()` bytes starting at `offset`.
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()>;

    /// Write `data` at `offset`. Not necessarily durable until [`Device::sync`].
    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()>;

    /// Make all previously written bytes durable.
    fn sync(&mut self) -> Result<()>;

    /// Enter the short commit-reservation critical section.
    ///
    /// Implementations shared by genuinely parallel writers use this to make
    /// sequence/page allocation and WAL append placement atomic. The expensive
    /// [`Device::sync`] happens after this section has been left, so writers
    /// can still flush separate log regions concurrently.
    fn begin_commit(&self) -> Result<()> {
        Ok(())
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

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        self.borrow_mut().write(offset, data)
    }

    fn sync(&mut self) -> Result<()> {
        self.borrow_mut().sync()
    }

    fn begin_commit(&self) -> Result<()> {
        self.borrow().begin_commit()
    }

    fn end_commit(&self) -> Option<u64> {
        self.borrow().end_commit()
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
}
