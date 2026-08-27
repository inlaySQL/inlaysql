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
            strict: false,
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

// ---------------------------------------------------------------- termination
//
// The `Trust` workflow's `sql_parser` run did not crash — it *timed out*, and
// the two findings below are what that turned out to be. Neither is a panic,
// so `fuzz_smoke.rs`'s property (a `Result`, never an abort) held while the
// process sat there; a timeout is the only signal an input like this gives.

/// 810 bytes of nested `replace()`.
///
/// Each level quadruples its subject, so forty levels ask for 4^40 bytes and
/// the engine spends the rest of its life trying to build them. libFuzzer's
/// default per-input timeout is 1200 seconds, which is why one input turned a
/// 300-second fuzz target into a 46-minute CI job.
///
/// The bound is SQLite's: `SQLITE_MAX_LENGTH`, and `sqlite3` 3.54.0 refuses
/// this same statement rather than building it. The length is computed from
/// the operand sizes *before* the allocation, so this is a refusal and not a
/// failed `Vec` growth.
#[test]
fn a_string_function_that_multiplies_is_refused_before_it_allocates() {
    let mut nested = String::from("'a'");
    for _ in 0..40 {
        nested = format!("replace({nested},'a','aaaa')");
    }
    let sql = format!("SELECT {nested}");
    assert!(sql.len() < 1_000, "fixture grew: {} bytes", sql.len());

    // Two guards now stand between this input and the allocation, and the
    // nesting one happens to reach it first — forty levels is past
    // `MAX_NESTING_DEPTH`. What the test pins is the property the fuzzer
    // measured, which is neither guard by name: the call *returns*.
    let start = std::time::Instant::now();
    let mut engine = inlaysql_core::mem::engine().expect("in-memory engine");
    engine
        .execute(&sql, &[])
        .expect_err("an unbounded string was built");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(30),
        "refusal took {:?}",
        start.elapsed()
    );

    // And the length bound itself, reached by amplification wide enough to
    // need only six levels: 64^6 bytes from a statement of a few hundred.
    let wide = "a".repeat(64);
    let mut amplifying = String::from("'a'");
    for _ in 0..6 {
        amplifying = format!("replace({amplifying},'a','{wide}')");
    }
    let error = engine
        .execute(&format!("SELECT {amplifying}"), &[])
        .expect_err("an unbounded string was built");
    assert!(
        matches!(error, inlaysql_core::Error::TooBig(_)),
        "expected a length refusal, got {error:?}"
    );
}

/// The same bound on the other functions whose output can outgrow their input.
///
/// Kept shallow on purpose: the nesting guard now refuses anything past
/// [`MAX_NESTING_DEPTH`], so amplification has to come from the width of each
/// step rather than the count of them — which is the honest shape of the
/// threat anyway, since one `replace()` can multiply by as much as its
/// replacement is long.
#[test]
fn every_growing_function_shares_one_length_bound() {
    let mut engine = inlaysql_core::mem::engine().expect("in-memory engine");

    // `hex()` only doubles, so within the nesting limit it cannot reach the
    // length bound at all — sixteen levels from eight bytes is half a
    // megabyte. That is worth pinning rather than asserting the opposite:
    // the two guards compose, and this is which one does the work here.
    let mut doubling = String::from("'aaaaaaaa'");
    for _ in 0..16 {
        doubling = format!("hex({doubling})");
    }
    assert!(
        engine.execute(&format!("SELECT {doubling}"), &[]).is_ok(),
        "sixteen doublings of hex() should be well inside the bound"
    );

    let wide = "a".repeat(64);
    let mut amplifying = String::from("'a'");
    for _ in 0..6 {
        amplifying = format!("replace({amplifying},'a','{wide}')");
    }
    assert!(
        matches!(
            engine.execute(&format!("SELECT {amplifying}"), &[]),
            Err(inlaysql_core::Error::TooBig(_))
        ),
        "six 64x replacements were not bounded"
    );
}

/// A value that stays under the bound is untouched by it.
#[test]
fn the_length_bound_does_not_reject_ordinary_strings() {
    let mut engine = inlaysql_core::mem::engine().expect("in-memory engine");
    for sql in [
        "SELECT replace('a rust database', 'rust', 'fast')",
        "SELECT hex('inlaysql')",
        "SELECT 'a' || 'b' || 'c'",
    ] {
        assert!(engine.execute(sql, &[]).is_ok(), "{sql} was refused");
    }
}

/// 4,000 `||` in a row: no parenthesis anywhere, so the nesting guard above
/// never sees it, and the left-leaning AST it builds took the planner's
/// recursion straight off the end of a 2 MiB thread stack — an abort, not an
/// error, reachable from the MCP server and the MySQL wire alike.
#[test]
fn a_flat_operator_chain_is_refused_rather_than_overflowing_the_stack() {
    let sql = format!("SELECT 'a'{}", "||'b'".repeat(4_000));
    let error = inlaysql_core::sql::plan(&sql, &[], &Catalog::new())
        .expect_err("a 4,000-operator chain was accepted");
    assert!(
        matches!(error, inlaysql_core::Error::Unsupported(ref m) if m.contains("chains")),
        "expected a chain-length refusal, got {error:?}"
    );
}

/// Every chaining shape, not just `||` — each one builds the same left spine.
#[test]
fn chain_length_is_bounded_whatever_the_operator() {
    for sql in [
        format!("SELECT 'a'{}", "||'b'".repeat(4_000)),
        format!("SELECT 1 WHERE 1=1{}", " OR 1=1".repeat(4_000)),
        format!("SELECT {}1", "-".repeat(4_000)),
        format!("SELECT {}1", "NOT ".repeat(4_000)),
    ] {
        assert!(
            inlaysql_core::sql::plan(&sql, &[], &Catalog::new()).is_err(),
            "an unbounded chain was accepted: {}…",
            &sql[..40]
        );
    }
}

/// The bound must not catch what an application actually writes. A wide
/// `VALUES` list and a long `IN (...)` are commas, which start a new
/// expression rather than extending one — this is the distinction that keeps
/// an ORM's bulk insert working.
#[test]
fn commas_are_not_a_chain() {
    let values = (0..2_000)
        .map(|i| format!("({i})"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("INSERT INTO t (a) VALUES {values}");
    assert!(
        !matches!(
            inlaysql_core::sql::plan(&sql, &[], &Catalog::new()),
            Err(inlaysql_core::Error::Unsupported(ref m)) if m.contains("chains")
        ),
        "a 2,000-row VALUES list was read as one expression chain"
    );

    let list = (0..2_000)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT a FROM t WHERE a IN ({list})");
    assert!(
        !matches!(
            inlaysql_core::sql::plan(&sql, &[], &Catalog::new()),
            Err(inlaysql_core::Error::Unsupported(ref m)) if m.contains("chains")
        ),
        "a 2,000-element IN list was read as one expression chain"
    );
}

/// And ordinary expressions are untouched.
#[test]
fn reasonable_chains_still_parse() {
    for sql in [
        "SELECT 1 + 2 * 3 - 4",
        "SELECT 'a' || 'b' || 'c'",
        "SELECT 1 = 1 AND 2 = 2 OR 3 = 3",
    ] {
        assert!(
            inlaysql_core::sql::plan(sql, &[], &Catalog::new()).is_ok(),
            "{sql} was refused"
        );
    }
}
