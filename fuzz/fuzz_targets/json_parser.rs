//! Arbitrary text through the hand-rolled JSON parser and path parser
//! (AHL-490).
//!
//! `crates/inlaysql-core/src/json.rs` is written from scratch, the same way
//! the MySQL wire protocol, SHA-1/SHA-256 and `inlaysql-mcp`'s JSON-RPC
//! already are in this repo (AGENTS.md forbids a new dependency in
//! `inlaysql-core`) — and a hand-rolled parser over untrusted input is
//! exactly the surface a fuzz target exists for. The property is not "it
//! parses" — most inputs are not JSON — but that parsing always terminates
//! with a `Result` rather than panicking, and that a document this parser
//! *does* accept round-trips through its own serializer into something it
//! accepts again with the same meaning.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inlaysql_core::json;

fuzz_target!(|data: &str| {
    // The bulk of the input is fed to the document parser directly. Errors
    // are the expected outcome for nonsense; only a panic is a finding.
    if let Ok(value) = json::parse(data) {
        // A document this parser accepted must serialize to something it
        // accepts again — the parser and the serializer disagreeing about
        // what "valid JSON" means would be a bug in one of them.
        let rendered = json::write(&value);
        let reparsed =
            json::parse(&rendered).unwrap_or_else(|_| panic!("{data:?} -> {rendered:?} did not reparse"));
        assert_eq!(
            value, reparsed,
            "{data:?} -> {rendered:?} reparsed to a different value"
        );
        // A second round trip must be a fixed point: the serializer's output
        // is already canonical, so parsing and re-serializing it again must
        // not move it any further.
        assert_eq!(json::write(&reparsed), rendered, "{rendered:?} is not a fixed point");
    }

    // The same text as a JSON *path* — a much smaller grammar sharing the
    // parser's string-escape code, exercised the same way.
    let _ = json::parse_path(data);

    // And the same text as a path evaluated against a couple of small, valid
    // documents, so `get`/`put`/`remove`'s tree walk sees fuzzed path shapes
    // against real trees rather than only well-formed ones from a unit test.
    if let Ok(path) = json::parse_path(data) {
        for doc_text in ["{\"a\":[1,2,3]}", "[1,{\"a\":1}]", "5", "null"] {
            let doc = json::parse(doc_text).expect("fixed documents are valid JSON");
            let _ = json::get(&doc, &path);
            let value = json::Json::Int(1);
            let _ = json::put(&doc, &path, &value, json::PutMode::Set);
            let _ = json::put(&doc, &path, &value, json::PutMode::Insert);
            let _ = json::put(&doc, &path, &value, json::PutMode::Replace);
            let _ = json::remove(&doc, &path);
        }
    }
});
