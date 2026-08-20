//! An index far larger than one transaction still gets saved.
//!
//! Saving an index writes megabytes through a storage engine whose commits are
//! bounded by the write-ahead log — one log region per transaction — so the
//! engine commits the save in batches. Which batch size is safe used to be
//! decided by a byte budget over the *payload*, and that is not the quantity
//! that has to fit: copy-on-write dirties a whole root-to-leaf path per entry,
//! so on a large enough tree 64 KiB of chunks became a 1.1 MiB log record.
//!
//! That was a hard failure rather than a slow path — `Storage("transaction
//! does not fit the write-ahead log")` — on a database of roughly five
//! thousand indexed rows. It was found by pointing the benchmark harness at a
//! corpus bigger than the one the published numbers used, which is the whole
//! argument for having a benchmark harness.
//!
//! Two tests, because the reproduction is expensive and the property is not:
//!
//! * the property — a save spanning many transactions round-trips — runs on a
//!   simulated disk in the default test run;
//! * the original failure needs a real five-thousand-row database, so it is
//!   `#[ignore]`d and run explicitly. See `TESTING.md`.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql::{Database, Value};
use inlaysql_core::sim::SimDisk;

/// Rows for the fast test. The vectors alone are `ROWS * 256 * 4` bytes, over
/// 1 MiB, so the save spans a couple of dozen transactions.
const ROWS: usize = 1_200;
const DIM: usize = 256;

/// Rows for the reproduction. Below this the tree is shallow enough that a
/// 64 KiB batch fitted the log and the bug did not appear.
const REPRO_ROWS: usize = 5_000;
const REPRO_DIM: usize = 128;

/// Simulated disk. Generous because copy-on-write never reuses a page today:
/// every insert allocates fresh pages and the old ones are not reclaimed, so
/// the file grows with the number of writes rather than the amount of data.
const CAPACITY: usize = 192 * 1024 * 1024;

fn embedding(seed: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|index| ((seed * 31 + index * 17) % 1000) as f32 / 1000.0)
        .collect()
}

fn load(db: &mut Database, rows: usize, dim: usize) {
    db.execute(
        &format!("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR({dim}))"),
        &[],
    )
    .expect("create");
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])
        .expect("create body index");
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .expect("create embedding index");
    for id in 1..=rows {
        db.execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(id as i64),
                Value::Text(format!("document number {id} about vectors and storage")),
                Value::Vector(embedding(id, dim)),
            ],
        )
        .unwrap_or_else(|error| panic!("insert {id}: {error}"));
    }
}

fn hybrid_query(db: &mut Database, dim: usize) -> Vec<Vec<Value>> {
    db.query(
        "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
         FROM docs ORDER BY score DESC LIMIT 5",
        &[
            Value::Vector(embedding(42, dim)),
            Value::Text("document number 42".to_string()),
        ],
    )
    .expect("hybrid query")
    .rows
}

#[test]
fn an_index_larger_than_the_log_region_is_saved_and_restored() {
    let disk = Rc::new(RefCell::new(SimDisk::new(CAPACITY)));
    let mut db = Database::open_on(disk.clone()).expect("open");
    load(&mut db, ROWS, DIM);

    // Writes both indexes into the database; neither fits in one transaction.
    db.checkpoint().expect("checkpoint a multi-megabyte index");
    let before = hybrid_query(&mut db, DIM);
    drop(db);

    // Reopening restores the saved index rather than rebuilding it, so this
    // also checks that what was written back reads: a save split across
    // transactions that reassembled wrongly would answer differently here.
    let mut reopened = Database::open_on(disk).expect("reopen");
    let after = hybrid_query(&mut reopened, DIM);

    assert_eq!(
        before, after,
        "the restored index answered differently from the one that was saved"
    );
    assert!(!after.is_empty(), "the query returned nothing at all");
}

/// The original failure, at the size it was found: five thousand rows.
///
/// Ignored by default because it is five thousand durable commits — half a
/// minute in release, and it writes a database of a few hundred megabytes.
/// Run it with:
///
/// ```sh
/// cargo test --release -p inlaysql --test large_index -- --ignored
/// ```
#[test]
#[ignore = "five thousand durable commits; run explicitly"]
fn five_thousand_indexed_rows_can_be_checkpointed() {
    let path =
        std::env::temp_dir().join(format!("inlaysql-large-index-{}.inlay", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let mut db = Database::open(&path).expect("open");
    load(&mut db, REPRO_ROWS, REPRO_DIM);
    db.checkpoint()
        .expect("a five-thousand-row index does not fit one log record");
    let rows = hybrid_query(&mut db, REPRO_DIM);
    drop(db);

    let _ = std::fs::remove_file(&path);
    assert!(!rows.is_empty(), "the query returned nothing at all");
}
