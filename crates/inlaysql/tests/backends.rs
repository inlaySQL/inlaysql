//! The same query suite, run against every I/O backend.
//!
//! This is Stage 3's cross-backend acceptance criterion. The backend is a
//! [`Device`] — four methods, no engine knowledge — so "the same suite passes
//! on `io_uring` and on the blocking fallback" should be a tautology. Tests
//! exist for the cases where it is not: a short read at end of file, an offset
//! computed differently, a sync that does not actually reach the platter.
//!
//! On a non-Linux host the `io_uring` case is absent rather than skipped: the
//! backend does not exist there at all, so there is nothing to assert.

use std::fs;
use std::path::PathBuf;

use inlaysql::sqllogictest::{self, Summary};
use inlaysql::{Database, FileDevice, Value};

/// Every statement in the SQL Logic Test subset, plus a hybrid retrieval query
/// — the two things a backend could plausibly break.
///
/// `open` hands back a fresh database each time it is called; each `.test` file
/// assumes an empty schema, exactly as the upstream corpus does.
fn run_suite(backend: &str, mut open: impl FnMut(&str) -> Database) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sqllogictest");
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
        let stem = file.file_stem().unwrap().to_string_lossy().to_string();
        let source = fs::read_to_string(file).unwrap_or_else(|e| panic!("{file:?}: {e}"));
        let records = sqllogictest::parse(&source).unwrap_or_else(|e| panic!("{file:?}: {e}"));
        let mut db = open(&stem);
        let summary = sqllogictest::run_on(&mut db, &records);
        all.total += summary.total;
        all.passed += summary.passed;
        all.failures.extend(summary.failures);
    }
    println!("{backend}: {all}");
    assert!(
        all.failures.is_empty(),
        "{backend}: SQL Logic Test failures:\n{:#?}",
        all.failures
    );

    let mut db = open("hybrid");
    hybrid_query(&mut db, backend);
}

/// A hybrid retrieval query, which touches the parts of the engine the SQL
/// Logic Test format cannot express (float scores).
fn hybrid_query(db: &mut Database, backend: &str) {
    db.execute(
        "CREATE TABLE hybrid (id INTEGER, body TEXT, embedding VECTOR(4))",
        &[],
    )
    .unwrap();
    db.execute("CREATE INDEX hybrid_body ON hybrid (body)", &[])
        .unwrap();
    db.execute("CREATE INDEX hybrid_embedding ON hybrid (embedding)", &[])
        .unwrap();
    let corpus = [
        (1, "embedded database engine", [1.0f32, 0.0, 0.0, 0.0]),
        (2, "vector search index", [0.0, 1.0, 0.0, 0.0]),
        (3, "embedded vector database", [0.9, 0.4, 0.0, 0.0]),
    ];
    for (id, body, embedding) in corpus {
        db.execute(
            "INSERT INTO hybrid (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(id),
                Value::Text(body.to_string()),
                Value::Vector(embedding.to_vec()),
            ],
        )
        .unwrap();
    }

    let rows = db
        .query(
            "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
             FROM hybrid ORDER BY score DESC LIMIT 3",
            &[
                Value::Vector(vec![1.0, 0.2, 0.0, 0.0]),
                Value::Text("embedded database".to_string()),
            ],
        )
        .unwrap();

    let ids: Vec<i64> = rows
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("{backend}: expected an integer id, got {other:?}"),
        })
        .collect();
    assert_eq!(ids.len(), 3, "{backend}: hybrid query returned {ids:?}");
    assert_eq!(
        ids[0], 1,
        "{backend}: the row both retrievers like should rank first, got {ids:?}"
    );
}

/// A fresh, empty database file. Files are left behind on failure on purpose:
/// a corrupted image is the most useful thing to have when one of these fails.
fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inlaysql-backend-{name}-{}.inlay",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    path
}

#[test]
fn the_blocking_backend_passes_the_suite() {
    let mut paths = Vec::new();
    run_suite("blocking", |name| {
        let path = temp_path(&format!("blocking-{name}"));
        let db = Database::open_on(FileDevice::open(&path).unwrap()).unwrap();
        paths.push(path);
        db
    });
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

#[test]
fn the_in_memory_backend_passes_the_suite() {
    run_suite("in-memory", |_| Database::open_in_memory().unwrap());
}

#[cfg(target_os = "linux")]
#[test]
fn the_io_uring_backend_passes_the_same_suite() {
    use inlaysql_uring::UringDevice;

    // A container without `io_uring_setup` (seccomp, an old kernel) is a real
    // deployment target, and the fallback exists precisely for it. Skipping
    // loudly beats failing a build over a kernel policy.
    if let Err(error) = UringDevice::open(temp_path("io-uring-probe"), 8) {
        println!("skipping: io_uring is unavailable here: {error}");
        return;
    }

    let mut paths = Vec::new();
    run_suite("io_uring", |name| {
        let path = temp_path(&format!("io-uring-{name}"));
        let db = Database::open_on(UringDevice::open(&path, 32).unwrap()).unwrap();
        paths.push(path);
        db
    });
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

/// Data written through one backend must be readable through the other: the
/// file format is the contract, not the I/O mechanism.
#[cfg(target_os = "linux")]
#[test]
fn a_database_written_with_io_uring_reopens_on_the_blocking_backend() {
    use inlaysql_uring::UringDevice;

    let path = temp_path("interop");
    let device = match UringDevice::open(&path, 32) {
        Ok(device) => device,
        Err(error) => {
            println!("skipping: io_uring is unavailable here: {error}");
            return;
        }
    };
    {
        let mut db = Database::open_on(device).unwrap();
        db.execute("CREATE TABLE t (a INTEGER, b TEXT)", &[])
            .unwrap();
        db.execute(
            "INSERT INTO t (a, b) VALUES (?, ?)",
            &[
                Value::Integer(42),
                Value::Text("written by io_uring".into()),
            ],
        )
        .unwrap();
    }

    let mut db = Database::open(&path).unwrap();
    let rows = db.query("SELECT a, b FROM t", &[]).unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![
            Value::Integer(42),
            Value::Text("written by io_uring".into())
        ]]
    );
    let _ = fs::remove_file(&path);
}
