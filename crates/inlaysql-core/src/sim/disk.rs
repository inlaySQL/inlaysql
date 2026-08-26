//! A simulated block device with a volatile and a durable half.
//!
//! Real storage splits every write across two places: the operating system's
//! page cache (volatile — lost on power failure) and the physical media
//! (durable — survives a power failure once the write has been synced). A power
//! loss in the middle of a sync can also leave a block *torn*: part of it holds
//! the new bytes and the rest still holds the old ones, because storage writes
//! are not atomic at the sector level.
//!
//! [`SimDisk`] reproduces these behaviours deterministically so the storage
//! engine can be "crashed" at any instruction and checked for corruption. It is
//! an in-memory model: it never touches a real file, which is what keeps the
//! core crate `no_std` and every run byte-for-byte reproducible.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::Cell;

use crate::btree::Device;
use crate::error::{Error, Result};

/// Default size of one block, in bytes. Pages in the storage engine will be a
/// multiple of this so torn writes can be modelled block by block.
pub const DEFAULT_BLOCK_SIZE: usize = 4096;

/// Default number of durable snapshots retained so a reordering fault can roll
/// the image back to an older sync.
pub const DEFAULT_SYNC_HISTORY: usize = 16;

/// A fault injected when the disk is asked to make pending writes durable.
///
/// Every variant maps onto a documented physical failure; see the individual
/// docs. The scheduler (`crate::sim::faults::FaultSchedule`) chooses which one
/// to apply at each sync, so a workload does not know when it will be crashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Nothing goes wrong; the sync completes normally.
    None,
    /// Power is lost: every write since the last completed sync disappears.
    Crash,
    /// Power is lost while the most recent write is only partially applied:
    /// the first `prefix` bytes of that write reach the durable image, the rest
    /// do not. Every other unsynced write is lost.
    TornWrite {
        /// How many leading bytes of the most recent write survived.
        prefix: usize,
    },
    /// The media reordered two syncs: the caller believes the sync committed,
    /// but the durable image rolls back to the snapshot from `syncs_ago` syncs
    /// earlier (0 is the most recent snapshot). Unsynced writes are lost.
    ReorderedSync {
        /// How many syncs back to roll the durable image.
        syncs_ago: usize,
    },
}

/// What happened to a [`SimDisk::sync`] request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The pending writes are now durable.
    Committed,
    /// The process lost power (or the fault implies one); the caller should
    /// stop issuing writes and run recovery instead.
    Crashed,
}

/// One recorded operation, kept so a replayed run can be compared event by
/// event as well as by final image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    /// A write of `len` bytes at `offset`.
    Write {
        /// Byte offset of the write.
        offset: usize,
        /// Number of bytes written.
        len: usize,
    },
    /// A sync request and its outcome.
    Sync {
        /// The fault that was applied to the sync.
        fault: Fault,
        /// The resulting outcome.
        outcome: SyncOutcome,
    },
}

/// A write that has not yet been made durable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingWrite {
    offset: usize,
    data: Vec<u8>,
}

/// A simulated block device.
///
/// Reads and writes are byte-addressed; blocks are an organisational unit that
/// fault injection reasons about, not a hard I/O boundary. The device keeps two
/// images plus the list of writes that separate them:
///
/// * `volatile` — the readable image, always up to date with every write;
/// * `durable` — what survives a crash, updated only by a successful sync;
/// * `pending` — writes issued since the last sync, in program order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimDisk {
    block_size: usize,
    durable: Vec<u8>,
    volatile: Vec<u8>,
    pending: Vec<PendingWrite>,
    /// Bounded snapshots of `durable`, newest last, for reordering faults.
    sync_history: VecDeque<Vec<u8>>,
    history_limit: usize,
    /// Every operation performed, for replay comparison.
    trace: Vec<TraceEvent>,
    writes: u64,
    syncs: u64,
    /// Whether a core handle has opted this shared device into page reuse.
    reuse_enabled: Cell<bool>,
}

impl SimDisk {
    /// An empty disk of `capacity` bytes using [`DEFAULT_BLOCK_SIZE`] blocks.
    pub fn new(capacity: usize) -> Self {
        Self::with_block_size(DEFAULT_BLOCK_SIZE, capacity)
    }

    /// An empty disk of `capacity` bytes divided into `block_size`-byte blocks.
    ///
    /// `capacity` is rounded up to a whole number of blocks.
    pub fn with_block_size(block_size: usize, capacity: usize) -> Self {
        assert!(block_size > 0, "block size must be positive");
        let capacity = capacity.div_ceil(block_size) * block_size;
        Self {
            block_size,
            durable: alloc::vec![0; capacity],
            volatile: alloc::vec![0; capacity],
            pending: Vec::new(),
            sync_history: VecDeque::new(),
            history_limit: DEFAULT_SYNC_HISTORY,
            trace: Vec::new(),
            writes: 0,
            syncs: 0,
            reuse_enabled: Cell::new(false),
        }
    }

    /// A disk whose durable and volatile images start from `image`.
    ///
    /// This models the moment after a reboot: the process re-reads whatever the
    /// physical media holds, with no pending writes and no sync history. Tests
    /// use it to reopen a "crashed" database from its surviving bytes.
    pub fn with_image(block_size: usize, image: &[u8]) -> Self {
        assert!(block_size > 0, "block size must be positive");
        let image = image.to_vec();
        Self {
            block_size,
            durable: image.clone(),
            volatile: image,
            pending: Vec::new(),
            sync_history: VecDeque::new(),
            history_limit: DEFAULT_SYNC_HISTORY,
            trace: Vec::new(),
            writes: 0,
            syncs: 0,
            reuse_enabled: Cell::new(false),
        }
    }

    /// The block size this disk was created with.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// The byte capacity of the disk.
    pub fn capacity(&self) -> usize {
        self.durable.len()
    }

    /// The current readable image (durable bytes plus unsynced writes).
    pub fn volatile(&self) -> &[u8] {
        &self.volatile
    }

    /// The durable image: what remains after a crash.
    pub fn durable(&self) -> &[u8] {
        &self.durable
    }

    /// The operations performed so far, in order.
    pub fn trace(&self) -> &[TraceEvent] {
        &self.trace
    }

    /// How many writes have been issued.
    pub fn write_count(&self) -> u64 {
        self.writes
    }

    /// How many syncs have been requested.
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    /// Read `buf.len()` bytes starting at `offset` from the volatile image.
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        if offset + buf.len() > self.volatile.len() {
            return Err(Error::Storage(alloc::format!(
                "read past end of disk: offset {offset} + {} > {}",
                buf.len(),
                self.volatile.len()
            )));
        }
        buf.copy_from_slice(&self.volatile[offset..offset + buf.len()]);
        Ok(())
    }

    /// Buffer a write at `offset`. It is visible to reads immediately but only
    /// durable after a successful [`SimDisk::sync`].
    pub fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        if offset + data.len() > self.volatile.len() {
            return Err(Error::Storage(alloc::format!(
                "write past end of disk: offset {offset} + {} > {}",
                data.len(),
                self.volatile.len()
            )));
        }
        self.volatile[offset..offset + data.len()].copy_from_slice(data);
        self.pending.push(PendingWrite {
            offset,
            data: data.to_vec(),
        });
        self.trace.push(TraceEvent::Write {
            offset,
            len: data.len(),
        });
        self.writes += 1;
        Ok(())
    }

    /// Make pending writes durable, applying `fault`.
    ///
    /// On a successful sync the pending writes are merged into `durable` and a
    /// snapshot is kept for reordering faults. On a crash the pending writes
    /// are lost — partially, in the case of a torn write — and the volatile
    /// image is rebuilt from whatever is now durable.
    pub fn sync(&mut self, fault: Fault) -> SyncOutcome {
        self.syncs += 1;

        let outcome = match fault {
            Fault::None => {
                self.apply_pending();
                SyncOutcome::Committed
            }
            Fault::Crash => {
                self.pending.clear();
                SyncOutcome::Crashed
            }
            Fault::TornWrite { prefix } => {
                if let Some(last) = self.pending.last() {
                    let keep = prefix.min(last.data.len());
                    self.durable[last.offset..last.offset + keep]
                        .copy_from_slice(&last.data[..keep]);
                }
                self.pending.clear();
                SyncOutcome::Crashed
            }
            Fault::ReorderedSync { syncs_ago } => {
                // The caller believes the sync committed, but the media has an
                // older image. Rolling back here simulates what a later crash
                // would reveal.
                if syncs_ago < self.sync_history.len() {
                    let index = self.sync_history.len() - 1 - syncs_ago;
                    self.durable.clone_from(&self.sync_history[index]);
                }
                self.pending.clear();
                SyncOutcome::Committed
            }
        };

        // Whatever the outcome, the readable image must now match the durable
        // image: pending writes are either applied or lost.
        self.volatile.clone_from(&self.durable);
        self.trace.push(TraceEvent::Sync { fault, outcome });
        outcome
    }

    /// Merge every pending write into the durable image and record a snapshot.
    fn apply_pending(&mut self) {
        for write in &self.pending {
            self.durable[write.offset..write.offset + write.data.len()]
                .copy_from_slice(&write.data);
        }
        self.pending.clear();
        self.sync_history.push_back(self.durable.clone());
        if self.sync_history.len() > self.history_limit {
            self.sync_history.pop_front();
        }
    }
}

impl Device for SimDisk {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        SimDisk::read(self, offset, buf)
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        SimDisk::write(self, offset, data)
    }

    fn sync(&mut self) -> Result<()> {
        SimDisk::sync(self, Fault::None);
        Ok(())
    }

    fn note_page_reuse_enabled(&self) {
        // `SimDisk` is normally wrapped in `Rc<RefCell<_>>` when multiple
        // handles share it, so the trait's shared-reference hook needs
        // interior mutability only for this one process-wide flag.
        self.reuse_enabled.set(true);
    }

    fn page_reuse_enabled(&self) -> bool {
        self.reuse_enabled.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_reflect_unsynced_writes() {
        let mut disk = SimDisk::new(32);
        disk.write(0, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 4];
        disk.read(0, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
        // Not durable yet.
        assert_eq!(&disk.durable()[..4], [0, 0, 0, 0]);
    }

    #[test]
    fn a_sync_makes_writes_durable() {
        let mut disk = SimDisk::new(32);
        disk.write(0, &[7, 8]).unwrap();
        assert_eq!(disk.sync(Fault::None), SyncOutcome::Committed);
        assert_eq!(&disk.durable()[..2], [7, 8]);
        assert_eq!(disk.volatile(), disk.durable());
    }

    #[test]
    fn a_crash_loses_unsynced_writes() {
        let mut disk = SimDisk::new(32);
        disk.write(0, &[1]).unwrap();
        disk.sync(Fault::None);
        disk.write(4, &[2]).unwrap();
        assert_eq!(disk.sync(Fault::Crash), SyncOutcome::Crashed);
        assert_eq!(disk.durable()[0], 1);
        assert_eq!(disk.durable()[4], 0);
    }

    #[test]
    fn a_torn_write_keeps_only_the_prefix() {
        let mut disk = SimDisk::new(32);
        disk.write(0, &[0xAA; 8]).unwrap();
        disk.sync(Fault::None);
        disk.write(0, &[0xBB; 8]).unwrap();
        assert_eq!(
            disk.sync(Fault::TornWrite { prefix: 3 }),
            SyncOutcome::Crashed
        );
        assert_eq!(&disk.durable()[..3], [0xBB; 3]);
        assert_eq!(&disk.durable()[3..8], [0xAA; 5]);
    }

    #[test]
    fn a_reordered_sync_rolls_back_to_an_older_snapshot() {
        let mut disk = SimDisk::new(32);
        disk.write(0, &[1]).unwrap();
        disk.sync(Fault::None);
        disk.write(0, &[2]).unwrap();
        disk.sync(Fault::None);
        disk.write(0, &[3]).unwrap();
        assert_eq!(
            disk.sync(Fault::ReorderedSync { syncs_ago: 1 }),
            SyncOutcome::Committed
        );
        // The image reverted to the first sync's value.
        assert_eq!(disk.durable()[0], 1);
    }

    #[test]
    fn writes_past_the_end_fail_without_side_effects() {
        let mut disk = SimDisk::with_block_size(16, 16);
        assert!(disk.write(15, &[1, 2]).is_err());
        let mut buf = [0u8; 1];
        assert!(disk.read(16, &mut buf).is_err());
        assert_eq!(disk.write_count(), 0);
    }

    #[test]
    fn every_operation_is_traced() {
        let mut disk = SimDisk::new(32);
        disk.write(0, &[1]).unwrap();
        disk.sync(Fault::None);
        assert_eq!(
            disk.trace(),
            &[
                TraceEvent::Write { offset: 0, len: 1 },
                TraceEvent::Sync {
                    fault: Fault::None,
                    outcome: SyncOutcome::Committed,
                },
            ]
        );
    }
}
