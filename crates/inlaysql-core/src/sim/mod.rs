//! Deterministic simulation testing (DST) harness.
//!
//! The core crate cannot touch a real disk, so the failure modes a storage
//! engine must survive — crashes, torn writes, reordered syncs — are modelled
//! here against an in-memory block device. A workload plus a fault schedule,
//! both driven by a single seed, replays byte-for-byte on any machine.
//!
//! This is the harness the Stage 2 storage engine (copy-on-write B-tree, WAL,
//! MVCC) will run against. It ships first because it cannot be retrofitted:
//! the engine is written to run *entirely on the simulator* from day one.
//!
//! ```
//! # use inlaysql_core::sim;
//! let a = sim::run_seed(42, |sim| {
//!     sim.disk_mut().write(0, &[1, 2, 3]).unwrap();
//!     sim.sync();
//! });
//! let b = sim::run_seed(42, |sim| {
//!     sim.disk_mut().write(0, &[1, 2, 3]).unwrap();
//!     sim.sync();
//! });
//! assert_eq!(a, b);
//! ```

pub mod disk;
pub mod faults;
pub mod runner;

pub use disk::{Fault, SimDisk, SyncOutcome, TraceEvent};
pub use faults::FaultSchedule;
pub use runner::{run_seed, RunResult, SimOutcome, Simulator};
