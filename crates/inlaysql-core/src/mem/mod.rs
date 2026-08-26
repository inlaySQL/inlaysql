//! A complete in-memory environment, for tests and deterministic simulation.
//!
//! These implementations are reference implementations, not fast ones: BM25 is
//! computed from plain postings lists and nearest neighbours by brute force.
//! Being obvious matters more here than being quick — when a simulation finds a
//! disagreement between this environment and the redb/tantivy/HNSW one, this
//! side is the oracle.
//!
//! Everything here is deterministic: no clock, no randomness that is not
//! seeded, and `BTreeMap` throughout so iteration order never varies.

mod bm25;
mod clock;
mod storage;
mod vector;

pub use bm25::MemFullTextIndex;
pub use clock::{LogicalClock, SeededRng};
pub use storage::MemStorage;
pub use vector::{cosine_similarity, BruteForceVectorIndex};

use alloc::boxed::Box;

use crate::engine::Engine;
use crate::error::Result;
use crate::hnsw::VectorMetric;
use crate::traits::{FullTextIndex, IndexFactory, VectorIndex};

/// Builds the in-memory index implementations.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemIndexFactory;

impl IndexFactory for MemIndexFactory {
    fn full_text(&self, _table: &str, _column: &str) -> Result<Box<dyn FullTextIndex>> {
        Ok(Box::new(MemFullTextIndex::new()))
    }

    fn vector(
        &self,
        _table: &str,
        _column: &str,
        dim: usize,
        metric: VectorMetric,
    ) -> Result<Box<dyn VectorIndex>> {
        Ok(Box::new(BruteForceVectorIndex::with_metric(dim, metric)))
    }
}

/// Open an engine backed entirely by memory.
///
/// The database disappears when it is dropped; nothing touches the filesystem.
pub fn engine() -> Result<Engine> {
    Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
}
