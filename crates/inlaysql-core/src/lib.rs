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

extern crate alloc;

#[cfg(test)]
extern crate std;

mod checksum;

pub mod bm25;
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
mod quantize;
pub mod row;
pub mod shared;
pub mod sim;
pub mod sql;
pub mod statement;
pub mod storage;
pub mod traits;
pub mod value;
pub mod wal;

pub use catalog::{
    is_reserved_table_name, Catalog, Column, Index, IndexKind, Table, RESERVED_TABLE_PREFIX,
};
pub use cdc::{Change, ChangeKind, Changes};
pub use collation::Collation;
pub use engine::{Engine, EngineOptions, Outcome, ResultSet};
pub use error::{Error, Result};
pub use plan::{ColumnInfo, TableAccess};
pub use shared::SharedStorage;
pub use statement::Statement;
pub use storage::TreeStorage;
pub use traits::{
    Cancel, Clock, FullTextIndex, IndexFactory, Rng, RowId, Scored, Stopped, Storage, VectorIndex,
};
pub use value::{DataType, Value};
