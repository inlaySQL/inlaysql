//! Deterministic core of InlaySQL.
//!
//! This crate holds the SQL front end, the planner, the executor and the
//! catalog. It deliberately contains **no I/O and no clock reads**: everything
//! the engine needs from the outside world arrives through the traits in
//! [`traits`].
//!
//! The guarantee is structural, not a convention: the crate is `no_std`, so it
//! cannot reach `std::fs`, `std::net`, `std::time` or thread APIs even by
//! accident. That is what makes deterministic simulation testing possible — a
//! test can drive the whole database from a seeded, in-memory environment (see
//! [`mem`]) and get byte-identical results on every run.
//!
//! Anything that genuinely needs the operating system (files, mmap, real
//! index libraries) lives in the `inlaysql` crate, which implements these
//! traits on top of redb, tantivy and an HNSW index.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Doc comments here explain the implementation to whoever is reading the
// source, so they link to private items on purpose: `[`CommitCoordinator`]`
// is the thing the sentence is about, whether or not a docs.rs reader can
// click it. Rustdoc's default is to reject those links in the docs of a
// public item, which would mean either deleting the reference or promoting
// an internal type to keep a sentence readable. Allowed instead; every other
// rustdoc lint stays denied; `AGENTS.md` documents the gate that runs them.
#![allow(rustdoc::private_intra_doc_links)]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod checksum;

pub mod bm25;
pub mod bm25_paged;
pub mod btree;
pub mod catalog;
pub mod cdc;
pub mod collation;
pub mod embedding;
pub mod engine;
pub mod error;
mod eval;
mod exec;
pub mod explain;
pub mod fusion;
pub mod hnsw;
pub mod hnsw_paged;
pub mod index;
pub mod json;
pub mod mem;
pub mod plan;
mod planner;
mod quantize;
pub mod row;
pub mod shared;
pub mod sim;
pub mod sql;
pub mod statement;
pub mod storage;
pub mod temp_storage;
pub mod traits;
pub mod value;
pub mod wal;

pub use btree::Durability;
pub use catalog::{
    is_reserved_table_name, Catalog, Column, Index, IndexKind, Table, RESERVED_TABLE_PREFIX,
};
pub use cdc::{Change, ChangeKind, Changes};
pub use collation::Collation;
pub use engine::{Engine, EngineOptions, Outcome, Reindexed, ResultSet};
pub use error::{Error, Result};
pub use plan::{ColumnInfo, TableAccess};
pub use shared::SharedStorage;
pub use statement::Statement;
pub use storage::TreeStorage;
pub use traits::{
    Cancel, Clock, FullTextIndex, IndexFactory, Rng, RowId, Scored, Stopped, Storage, VectorIndex,
    VectorTuning,
};
pub use value::{DataType, Value, ValueRef};
