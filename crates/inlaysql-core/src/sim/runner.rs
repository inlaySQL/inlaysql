//! The simulator: a disk, a fault schedule and a workload's RNG, all driven by
//! one seed.
//!
//! A test describes a *workload* (the sequence of writes and syncs an engine
//! would issue) as a closure over a [`Simulator`]. The simulator supplies three
//! things the workload is allowed to touch:
//!
//! * the [`SimDisk`], for reads and writes;
//! * [`Simulator::sync`], which injects the next scheduled fault;
//! * a [`SeededRng`], so the workload itself can make deterministic choices.
//!
//! Because every one of those is a pure function of the seed, running the same
//! closure under the same seed produces the same [`SimOutcome`]. A regression
//! therefore becomes reproducible, and CI can sweep thousands of seeds.

use alloc::vec::Vec;

use crate::btree::Device;
use crate::error::Result;
use crate::mem::SeededRng;

use super::disk::{SimDisk, SyncOutcome, TraceEvent};
use super::faults::FaultSchedule;

/// Default capacity of a simulated disk, in bytes.
pub const DEFAULT_CAPACITY: usize = 1 << 20;

/// The observable result of a simulated run: enough to compare two runs for
/// equality and to debug a divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimOutcome {
    /// The durable image at the end of the run.
    pub durable: Vec<u8>,
    /// The readable image at the end of the run.
    pub volatile: Vec<u8>,
    /// The full event trace of the run.
    pub trace: Vec<TraceEvent>,
    /// How many writes the workload issued.
    pub writes: u64,
    /// How many syncs the workload requested.
    pub syncs: u64,
    /// Whether the final operation crashed the process.
    pub crashed: bool,
}

/// The result of [`run_seed`]: the workload's return value plus its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult<T> {
    /// The value the workload returned.
    pub value: T,
    /// The simulated disk's final state.
    pub outcome: SimOutcome,
}

/// The combined disk, schedule and RNG a workload runs against.
#[derive(Debug)]
pub struct Simulator {
    disk: SimDisk,
    schedule: FaultSchedule,
    rng: SeededRng,
    crashed: bool,
}

impl Simulator {
    /// A simulator over a default-sized disk with a random fault schedule.
    pub fn new(seed: u64) -> Self {
        Self::with_disk(
            seed,
            SimDisk::new(DEFAULT_CAPACITY),
            FaultSchedule::random(seed),
        )
    }

    /// A simulator over `disk` with `schedule` and a workload RNG, all seeded
    /// by `seed`.
    pub fn with_disk(seed: u64, disk: SimDisk, schedule: FaultSchedule) -> Self {
        Self {
            disk,
            schedule,
            rng: SeededRng::new(seed),
            crashed: false,
        }
    }

    /// The simulated disk. Read and write it directly.
    pub fn disk(&self) -> &SimDisk {
        &self.disk
    }

    /// The simulated disk, mutably.
    pub fn disk_mut(&mut self) -> &mut SimDisk {
        &mut self.disk
    }

    /// The workload's deterministic RNG.
    pub fn rng(&mut self) -> &mut SeededRng {
        &mut self.rng
    }

    /// Whether a previous sync crashed the process.
    pub fn crashed(&self) -> bool {
        self.crashed
    }

    /// Sync the disk, injecting the next fault from the schedule.
    ///
    /// After a crash the simulator remembers it so callers can decide whether
    /// to keep issuing writes (they should not).
    pub fn sync(&mut self) -> SyncOutcome {
        let fault = self.schedule.next_fault();
        let outcome = self.disk.sync(fault);
        if outcome == SyncOutcome::Crashed {
            self.crashed = true;
        }
        outcome
    }

    /// Force a crash immediately, bypassing the schedule. Useful for tests that
    /// crash at a specific instruction rather than a sync boundary.
    pub fn crash(&mut self) {
        self.disk.sync(crate::sim::disk::Fault::Crash);
        self.crashed = true;
    }

    /// Snapshot the disk's current state as a [`SimOutcome`].
    pub fn snapshot(&self) -> SimOutcome {
        SimOutcome {
            durable: self.disk.durable().to_vec(),
            volatile: self.disk.volatile().to_vec(),
            trace: self.disk.trace().to_vec(),
            writes: self.disk.write_count(),
            syncs: self.disk.sync_count(),
            crashed: self.crashed,
        }
    }
}

/// Run `workload` against a simulator built from `seed` and return its value
/// and the resulting [`SimOutcome`].
///
/// `workload` may return anything; the outcome is what a test compares to
/// prove determinism or a lack of corruption.
pub fn run_seed<T>(seed: u64, workload: impl FnOnce(&mut Simulator) -> T) -> RunResult<T> {
    let mut simulator = Simulator::new(seed);
    let value = workload(&mut simulator);
    RunResult {
        value,
        outcome: simulator.snapshot(),
    }
}

/// The simulator as a [`Device`]: a storage engine can run on it unmodified,
/// while every `sync` (and therefore every commit) draws the next fault from
/// the schedule.
impl Device for Simulator {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        self.disk.read(offset, buf)
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        self.disk.write(offset, data)
    }

    fn sync(&mut self) -> Result<()> {
        // The inherent method injects the next scheduled fault.
        Simulator::sync(self);
        Ok(())
    }

    fn note_page_reuse_enabled(&self) {
        self.disk.note_page_reuse_enabled();
    }

    fn page_reuse_enabled(&self) -> bool {
        self.disk.page_reuse_enabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::disk::Fault;
    use crate::traits::Rng;

    /// A representative workload: write a page, sync, write another page, sync,
    /// sometimes crash. Every branch is a function of the simulator only.
    fn workload(sim: &mut Simulator) -> u64 {
        let mut committed = 0;
        let page = [0xAB; 32];

        sim.disk_mut().write(0, &page).unwrap();
        if sim.sync() == SyncOutcome::Committed {
            committed += 1;
        }

        // A second page whose offset depends on the workload RNG.
        let offset = (sim.rng().next_u64() as usize) % 64 * 32;
        let page2 = [0xCD; 32];
        sim.disk_mut().write(offset, &page2).unwrap();
        if sim.sync() == SyncOutcome::Committed {
            committed += 1;
        }

        committed
    }

    #[test]
    fn the_same_seed_replays_identically() {
        let a = run_seed(42, workload);
        let b = run_seed(42, workload);
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_may_diverge() {
        // Not a strict guarantee of inequality, but overwhelmingly likely, and
        // the whole point of a seeded RNG is that the traces are independent.
        let a = run_seed(1, workload);
        let b = run_seed(2, workload);
        assert!(a.outcome.trace != b.outcome.trace || a.outcome.durable != b.outcome.durable);
    }

    #[test]
    fn a_crashing_workload_is_recorded_as_crashed() {
        // A script that crashes on the very first sync.
        let seed = 5;
        let mut sim = Simulator::with_disk(
            seed,
            SimDisk::new(DEFAULT_CAPACITY),
            FaultSchedule::script(&[Fault::Crash]),
        );
        sim.disk_mut().write(0, &[1, 2, 3]).unwrap();
        assert_eq!(sim.sync(), SyncOutcome::Crashed);
        assert!(sim.crashed());
        assert!(sim.snapshot().crashed);
        // Nothing was made durable.
        assert_eq!(&sim.disk().durable()[..3], [0, 0, 0]);
    }

    #[test]
    fn a_snapshot_is_a_point_in_time() {
        let mut sim = Simulator::new(7);
        sim.disk_mut().write(0, &[9]).unwrap();
        let before = sim.snapshot();
        sim.sync();
        let after = sim.snapshot();
        assert_ne!(before, after);
        assert_eq!(before.durable[0], 0);
        assert_eq!(after.durable[0], 9);
    }
}
