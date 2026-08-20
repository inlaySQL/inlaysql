//! The reference full-text index for the in-memory environment.
//!
//! BM25 is deterministic — there is no approximate/exact split as there is for
//! vector search — so the reference index *is* the in-engine implementation.
//! It is re-exported under the environment's name to keep the [`crate::mem`]
//! surface stable.

pub use crate::bm25::Bm25Index as MemFullTextIndex;
