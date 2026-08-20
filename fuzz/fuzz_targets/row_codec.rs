//! Arbitrary bytes through the row, catalog and index decoders.
//!
//! Every one of these parses persisted bytes, so every one of them can be
//! handed a torn write. They must all fail rather than panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inlaysql_core::bm25::Bm25Index;
use inlaysql_core::hnsw::HnswIndex;
use inlaysql_core::Catalog;

fuzz_target!(|data: &[u8]| {
    let _ = inlaysql_core::row::decode_row(data);
    let _ = Catalog::decode(data);
    let _ = Bm25Index::decode(data);
    let _ = HnswIndex::decode(data);
});
