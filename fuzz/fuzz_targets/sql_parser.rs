//! Arbitrary text through the SQL front end.
//!
//! The property is not "it parses" — most inputs are nonsense — but that the
//! front end *always terminates with a Result*. A panic here is reachable by
//! anything that can send a query string, which for an MCP server is a
//! language model.
#![no_main]

use libfuzzer_sys::fuzz_target;

use inlaysql_core::{mem, Catalog, Column, DataType, Table};

fuzz_target!(|data: &str| {
    let mut catalog = Catalog::new();
    let _ = catalog.create_table(Table {
        name: "t".to_string(),
        columns: vec![
            Column::primary_key("id", DataType::Integer),
            Column::new("body", DataType::Text),
            Column::new("embedding", DataType::Vector(4)),
        ],
    });
    // Errors are the expected outcome; only a panic is a finding.
    let _ = inlaysql_core::sql::plan(data, &[], &catalog);

    // And the same text against a live engine, which reaches the executor for
    // the inputs that do parse.
    if let Ok(mut engine) = mem::engine() {
        let _ = engine.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[]);
        let _ = engine.execute(data, &[]);
    }
});
