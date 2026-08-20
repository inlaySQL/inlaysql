//! Run SQL Logic Test `.test` files against the engine and print the pass rate.
//!
//! ```sh
//! cargo run -p inlaysql --bin sqllogictest -- tests/sqllogictest/*.test
//! ```

use std::fs;
use std::process::ExitCode;

use inlaysql::sqllogictest::{self, Summary};

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: sqllogictest <file.test> [more.test ...]");
        return ExitCode::FAILURE;
    }

    let mut all = Summary {
        total: 0,
        passed: 0,
        failures: Vec::new(),
    };
    let mut any_error = false;

    for path in paths {
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("{path}: {error}");
                any_error = true;
                continue;
            }
        };
        let records = match sqllogictest::parse(&source) {
            Ok(records) => records,
            Err(error) => {
                eprintln!("{path}: {error}");
                any_error = true;
                continue;
            }
        };
        let summary = sqllogictest::run(&records);
        println!("{path}: {summary}");
        for failure in &summary.failures {
            eprintln!("  record {}: {}", failure.index, failure.message);
        }
        all.total += summary.total;
        all.passed += summary.passed;
        all.failures.extend(summary.failures);
    }

    println!("TOTAL: {all}");
    if any_error || !all.failures.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
