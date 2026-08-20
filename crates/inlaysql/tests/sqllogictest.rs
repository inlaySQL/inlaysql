//! Runs the vendored SQL Logic Test subset and fails the build on any mismatch.
//!
//! This is the measurable acceptance criterion for Stage 3: the pass rate over
//! SQLite's SQL Logic Test corpus, in the standard format. The subset lives in
//! `tests/sqllogictest/` and grows as the dialect matures.

use std::fs;
use std::path::PathBuf;

use inlaysql::sqllogictest::{self, Summary};

fn test_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sqllogictest")
}

#[test]
fn the_sqllogictest_subset_passes() {
    let dir = test_dir();
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read sqllogictest dir")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "test"))
        .collect();
    files.sort();

    assert!(!files.is_empty(), "no .test files found in {dir:?}");

    let mut all = Summary {
        total: 0,
        passed: 0,
        failures: Vec::new(),
    };

    for file in &files {
        let source = fs::read_to_string(file).unwrap_or_else(|e| panic!("{file:?}: {e}"));
        let records = sqllogictest::parse(&source).unwrap_or_else(|e| panic!("{file:?}: {e}"));
        let summary = sqllogictest::run(&records);
        println!("{}: {summary}", file.file_name().unwrap().to_string_lossy());
        for failure in &summary.failures {
            println!("  record {}: {}", failure.index, failure.message);
        }
        all.total += summary.total;
        all.passed += summary.passed;
        all.failures.extend(summary.failures);
    }

    println!("TOTAL: {all}");
    assert!(
        all.failures.is_empty(),
        "SQL Logic Test failures:\n{:#?}",
        all.failures
    );
}
