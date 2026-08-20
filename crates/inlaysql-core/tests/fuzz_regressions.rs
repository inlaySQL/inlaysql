//! The inputs the coverage-guided fuzzer actually crashed on, kept as tests.
//!
//! `fuzz_smoke.rs` asserts the same *properties* over seeded inputs and missed
//! all three of these — which is the honest argument for running a real
//! fuzzer, and the reason these exact bytes are vendored here rather than
//! described. A crash that has been fixed but not pinned comes back.
//!
//! The first two were found by the `Trust` workflow on `main`, run
//! 31878375386. The third (AHL-492) was `json_parser`'s own crash: the
//! `Trust` workflow's first run against the JSON parser/serializer added in
//! AHL-490 found it within 300 seconds.

use std::fs;
use std::path::PathBuf;

use inlaysql_core::bm25::Bm25Index;
use inlaysql_core::hnsw::HnswIndex;
use inlaysql_core::{Catalog, Column, DataType, Table};

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fuzz-regressions")
        .join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"))
}

/// `0a 0a 2c 2c` — four bytes that declare 741,281,802 elements.
///
/// Every decoder read that count and handed it straight to
/// `Vec::with_capacity`, asking for gigabytes. On a host with a memory limit
/// — a fuzzer, a container, an edge runtime — that is an abort, not an error.
/// It is reachable from `Database::open` on any file you did not write.
#[test]
fn a_tiny_input_declaring_a_huge_count_is_rejected_not_allocated() {
    let bytes = fixture("row_codec-oversized-count.bin");
    assert_eq!(bytes.len(), 4, "fixture changed");

    // Each must return an error rather than trying to reserve the count.
    assert!(inlaysql_core::row::decode_row(&bytes).is_err());
    assert!(Catalog::decode(&bytes).is_err());
    assert!(Bm25Index::decode(&bytes).is_err());
    assert!(HnswIndex::decode(&bytes).is_err());
}

/// The same shape, generalised: for any four-byte prefix, a declared count can
/// never exceed the bytes that follow it.
#[test]
fn no_declared_count_can_exceed_the_bytes_that_remain() {
    for count in [u32::MAX, u32::MAX / 2, 741_281_802, 100_000] {
        let bytes = count.to_le_bytes();
        assert!(
            inlaysql_core::row::decode_row(&bytes).is_err(),
            "a 4-byte input declaring {count} values was accepted"
        );
        assert!(Catalog::decode(&bytes).is_err());
    }
}

/// A well-formed encoding still round-trips — the bound rejects the impossible,
/// not the merely large.
#[test]
fn the_bound_does_not_reject_legitimate_data() {
    let mut catalog = Catalog::new();
    catalog
        .create_table(Table {
            name: "docs".to_string(),
            columns: vec![
                Column::primary_key("id", DataType::Integer),
                Column::new("body", DataType::Text),
                Column::new("embedding", DataType::Vector(384)),
            ],
        })
        .unwrap();
    assert_eq!(Catalog::decode(&catalog.encode()).unwrap(), catalog);

    let mut index = Bm25Index::new();
    for id in 1..=200 {
        index_document(&mut index, id);
    }
    assert_eq!(Bm25Index::decode(&index.encode()).unwrap().len(), 200);
}

fn index_document(index: &mut Bm25Index, id: u64) {
    use inlaysql_core::traits::FullTextIndex;
    index
        .insert(id, "an embedded database written in rust with retrieval")
        .unwrap();
}

/// 428 bytes of `(`.
///
/// `sqlparser`'s own recursion limit is compiled out for us: the core depends
/// on it with `default-features = false` to stay `no_std`, which drops
/// `recursive-protection` and leaves `RecursionCounter::try_decrease` a stub
/// that always succeeds. The parser then recurses until the stack is gone.
///
/// Reachable from the MCP server, which hands a language model's text straight
/// to the planner — so this is a way to kill the process from the far side of
/// a tool call.
#[test]
fn deeply_nested_parentheses_are_refused_rather_than_overflowing_the_stack() {
    let sql = String::from_utf8_lossy(&fixture("sql_parser-deep-nesting.sql")).into_owned();
    assert!(sql.len() > 400, "fixture changed");

    let error = inlaysql_core::sql::plan(&sql, &[], &Catalog::new())
        .expect_err("deeply nested input was accepted");
    assert!(
        matches!(error, inlaysql_core::Error::Unsupported(ref m) if m.contains("nests")),
        "expected a nesting-depth refusal, got {error:?}"
    );
}

/// The generalised version: depth alone is enough, whatever else is in the
/// statement.
#[test]
fn nesting_is_bounded_regardless_of_what_is_nested() {
    for depth in [200usize, 5_000, 50_000] {
        let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
        assert!(
            inlaysql_core::sql::plan(&sql, &[], &Catalog::new()).is_err(),
            "{depth} levels of nesting was accepted"
        );
    }
}

/// Parentheses inside a string literal are text, not structure — the guard
/// must not reject them.
#[test]
fn parentheses_inside_a_literal_do_not_count_as_nesting() {
    let sql = format!("SELECT '{}'", "(".repeat(500));
    let plan = inlaysql_core::sql::plan(&sql, &[], &Catalog::new());
    assert!(
        plan.is_ok(),
        "a long literal was mistaken for nesting: {plan:?}"
    );
}

/// And ordinary nesting still works.
#[test]
fn reasonable_nesting_still_parses() {
    let sql = "SELECT ((1 + 2) * (3 - 4)) / (((5)))";
    assert!(inlaysql_core::sql::plan(sql, &[], &Catalog::new()).is_ok());
}

/// 18 bytes: `3.7777777777777777`.
///
/// `json_parser`'s own round-trip property — a document this parser accepts
/// must reserialize to something that reparses to the same value — failed on
/// this input. `json::write` rendered a parsed `Real` through
/// `eval::real_to_text`, SQLite's fifteen-significant-digit `CAST(x AS TEXT)`
/// rule, which is correct for that but not for JSON: `3.7777777777777777`
/// came back out as `3.77777777777778`, a different `f64` once reparsed.
///
/// The fix (AHL-492) carries a `Real`'s source text on the `Json` value
/// itself and re-emits it verbatim — see `inlaysql_core::json::Json`'s doc
/// comment — so this fixture now round-trips byte for byte, not just to an
/// equal value.
#[test]
fn a_json_number_that_needs_more_than_fifteen_digits_round_trips_byte_for_byte() {
    let bytes = fixture("json_parser-number-precision.json");
    assert_eq!(bytes.len(), 18, "fixture changed");
    let text = String::from_utf8(bytes).expect("fixture is UTF-8");

    let value = inlaysql_core::json::parse(&text).expect("valid JSON was rejected");
    let rendered = inlaysql_core::json::write(&value);
    // Not just equal in value — the exact source spelling, unlike the
    // fifteen-digit rendering this bug used to produce.
    assert_eq!(
        rendered, text,
        "the number's source spelling was not preserved"
    );

    let reparsed = inlaysql_core::json::parse(&rendered).expect("rendered output must reparse");
    assert_eq!(value, reparsed, "reparsing changed the value");
    assert_eq!(
        inlaysql_core::json::write(&reparsed),
        rendered,
        "a second round trip was not a fixed point"
    );
}
