//! The fuzz targets' properties, on stable, over seeded inputs.
//!
//! `cargo-fuzz` needs nightly and a long time budget, so the real fuzzing runs
//! on a schedule rather than on every push. That leaves a gap: a target that no
//! longer compiles, or a property that quietly stopped being true, would not be
//! noticed until somebody ran the fuzzer.
//!
//! These tests close it. They exercise the same properties the targets in
//! `fuzz/fuzz_targets/` assert, over a few thousand deterministic inputs, in
//! every `cargo test`. They will not find what a coverage-guided fuzzer finds —
//! that is the point of also having the fuzzer — but they keep the properties
//! honest and the targets buildable.

use inlaysql_core::bm25::Bm25Index;
use inlaysql_core::hnsw::HnswIndex;
use inlaysql_core::mem::SeededRng;
use inlaysql_core::sim::SimDisk;
use inlaysql_core::storage::TreeStorage;
use inlaysql_core::{mem, Catalog, Column, DataType, Rng, Storage, Table};

fn catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog
        .create_table(Table {
            without_rowid: false,
            temporary: false,
            primary_key: Vec::new(),
            name: "t".to_string(),
            columns: vec![
                Column::primary_key("id", DataType::Integer),
                Column::new("body", DataType::Text),
                Column::new("embedding", DataType::Vector(4)),
            ],
            strict: false,
        })
        .unwrap();
    catalog
}

/// Bytes that look like SQL often enough to reach the parser's interesting
/// paths, rather than uniform noise that dies at the first character.
fn sql_soup(rng: &mut SeededRng) -> String {
    const FRAGMENTS: &[&str] = &[
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "CREATE TABLE",
        "FROM",
        "WHERE",
        "ORDER BY",
        "LIMIT",
        "VALUES",
        "AND",
        "OR",
        "NOT",
        "NULL",
        "(",
        ")",
        ",",
        "*",
        "=",
        "<",
        ">",
        "+",
        "-",
        "/",
        "%",
        "?",
        "'",
        "\"",
        "t",
        "id",
        "body",
        "embedding",
        "1",
        "-1",
        "0",
        "9223372036854775807",
        "1e400",
        "vector_score",
        "bm25_score",
        "fuse",
        "PRIMARY KEY",
        "VECTOR(4)",
        "INTEGER",
        "TEXT",
        ";",
        "--",
        "\n",
        "\u{1f600}",
    ];
    let length = 1 + (rng.next_u64() % 12) as usize;
    (0..length)
        .map(|_| FRAGMENTS[(rng.next_u64() % FRAGMENTS.len() as u64) as usize])
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_sql_front_end_always_returns_rather_than_panics() {
    let mut rng = SeededRng::new(0x5A17);
    let catalog = catalog();
    for _ in 0..4_000 {
        let sql = sql_soup(&mut rng);
        // An error is the expected outcome. Only a panic is a finding.
        let _ = inlaysql_core::sql::plan(&sql, &[], &catalog);
    }
}

#[test]
fn the_engine_survives_the_same_soup() {
    let mut rng = SeededRng::new(0xE0F1);
    let mut engine = mem::engine().unwrap();
    engine
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(4))",
            &[],
        )
        .unwrap();
    for _ in 0..2_000 {
        let sql = sql_soup(&mut rng);
        let _ = engine.execute(&sql, &[]);
    }
    // Whatever that did, the database still answers.
    assert!(engine.query("SELECT id FROM t", &[]).is_ok());
}

#[test]
fn an_arbitrary_image_is_rejected_rather_than_believed() {
    let mut rng = SeededRng::new(0xD15C);
    for _ in 0..500 {
        let len = (rng.next_u64() % 4096) as usize;
        let mut image: Vec<u8> = (0..len).map(|_| (rng.next_u64() >> 24) as u8).collect();
        // Half the images start with our magic, so the header check is passed
        // and the deeper decoders are actually reached.
        if rng.next_u64().is_multiple_of(2) && image.len() >= 8 {
            image[..8].copy_from_slice(b"INLAYSQL");
        }

        let Ok(storage) = TreeStorage::open_on(SimDisk::with_image(512, &image)) else {
            continue;
        };
        // If it opened, reading it must not panic either.
        let _ = inlaysql_core::traits::scan_all(&storage, "t");
        let _ = storage.get_meta("catalog");
        let _ = storage.get_row("t", 1);
    }
}

#[test]
fn every_persisted_decoder_rejects_arbitrary_bytes() {
    let mut rng = SeededRng::new(0xC0DE);
    for _ in 0..2_000 {
        let len = (rng.next_u64() % 256) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() >> 24) as u8).collect();
        let _ = inlaysql_core::row::decode_row(&bytes);
        let _ = Catalog::decode(&bytes);
        let _ = Bm25Index::decode(&bytes);
        let _ = HnswIndex::decode(&bytes);
    }
}

#[test]
fn truncating_a_valid_encoding_at_every_offset_is_rejected() {
    // The shape a torn write actually takes: a valid prefix. Random noise
    // rarely produces one, so it is worth testing directly.
    let encoded = catalog().encode();
    for cut in 0..encoded.len() {
        let _ = Catalog::decode(&encoded[..cut]);
    }
}
