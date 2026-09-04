//! The copy-on-write B+ tree: the durable core of the Stage 2 storage engine.
//!
//! The tree stores byte keys and byte values in a page-based B+ tree. It never
//! overwrites a page in place — every mutation copies the path from the root
//! to the affected leaf into fresh pages, and committing appends a record to
//! the write-ahead log ([`crate::wal`]) and syncs. That single discipline
//! gives two things at once:
//!
//! * **MVCC snapshot reads** — any reader holding an old root sees an
//!   immutable, consistent snapshot, because old pages are never touched.
//! * **Crash safety** — a crash before the commit record is synced leaves the
//!   old root (and therefore the old data) fully intact; a torn metadata write
//!   is recovered by replaying the log.
//!
//! The tree is written against the [`Device`] trait, so it runs unchanged on a
//! real file *and* on the deterministic, fault-injecting simulation harness in
//! [`crate::sim`].

pub mod backup;
pub mod cache;
pub mod device;
pub mod page;
pub mod tree;

pub use backup::BackupSummary;
pub use cache::{PageCache, DEFAULT_PAGE_CACHE_BYTES};
pub use device::{
    AbsorbQueue, AbsorbResult, AbsorbTxn, CommitPoint, Device, Durability, PendingOps,
};
pub use page::{PageId, DEFAULT_PAGE_SIZE, MIN_PAGE_SIZE};
pub use tree::{CommitOutcome, CowBTree, Diagnostics, FORMAT_VERSION};
