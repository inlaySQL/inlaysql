//! End-to-end proof for `inlaysql::vacuum` (Phase 2 item 6): a real schema
//! covering the shapes `create_table_sql`/`index_statements` have to
//! reconstruct correctly — a primary key, `NOT NULL`, `DEFAULT`, a named and
//! an unnamed `UNIQUE`, a `CHECK`, a `FOREIGN KEY`, a `COLLATE NOCASE`
//! column, a full-text index and a vector index — survives a vacuum with its
//! data, its constraints and its query behaviour all intact, and the file
//! actually shrinks after a large delete.

use std::fs;
use std::path::{Path, PathBuf};

use inlaysql::{Database, EngineOptions, FileDevice, Value};

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-vacuum-{name}-{}.inlay",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

const DIM: usize = 8;

fn vector(seed: u64) -> Vec<f32> {
    (0..DIM)
        .map(|i| ((seed.wrapping_mul(31).wrapping_add(i as u64)) % 97) as f32 / 97.0)
        .collect()
}

/// Build the schema every reconstruction shape in `create_table_sql`/
/// `index_statements` has to get right, then populate and churn it so a
/// vacuum has both something to reconstruct and something to shrink.
fn build(path: &Path) {
    let device = FileDevice::open(path).expect("open");
    let mut db = Database::open_on_with_options(
        device,
        EngineOptions {
            // Deliberately off: this is the "one big delete already
            // happened, nothing since has reused those pages" case vacuum
            // exists for, not the steady-state-churn case page_reuse covers.
            page_reuse: false,
            ..EngineOptions::default()
        },
    )
    .expect("open with options");

    db.execute(
        "CREATE TABLE authors (\
           id INTEGER PRIMARY KEY, \
           name TEXT NOT NULL COLLATE NOCASE, \
           handle TEXT UNIQUE, \
           bio TEXT DEFAULT ('unknown'))",
        &[],
    )
    .expect("create authors");
    db.execute(
        "CREATE UNIQUE INDEX authors_name_idx ON authors (name)",
        &[],
    )
    .expect("create named unique index");

    db.execute(
        "CREATE TABLE docs (\
           id INTEGER PRIMARY KEY, \
           author_id INTEGER REFERENCES authors(id), \
           title TEXT, \
           rating INTEGER CHECK (rating BETWEEN 1 AND 5), \
           embedding VECTOR(8))",
        &[],
    )
    .expect("create docs");
    db.execute("CREATE INDEX docs_title_idx ON docs (title)", &[])
        .expect("create fulltext index");
    db.execute("CREATE INDEX docs_embedding_idx ON docs (embedding)", &[])
        .expect("create vector index");

    for i in 0..5 {
        db.execute(
            "INSERT INTO authors (id, name, handle, bio) VALUES (?, ?, ?, ?)",
            &[
                Value::Integer(i),
                Value::Text(format!("Author {i}").into()),
                Value::Text(format!("author{i}").into()),
                Value::Text(format!("bio {i}").into()),
            ],
        )
        .expect("insert author");
    }

    // Enough rows, and enough of them later deleted, that the file has real
    // bloat for `heavy_churn...`-style bloat to shrink away.
    const ROWS: i64 = 400;
    for i in 0..ROWS {
        db.execute(
            "INSERT INTO docs (id, author_id, title, rating, embedding) \
             VALUES (?, ?, ?, ?, ?)",
            &[
                Value::Integer(i),
                Value::Integer(i % 5),
                Value::Text(format!("Document number {i} about rust and databases").into()),
                Value::Integer((i % 5) + 1),
                Value::Vector(vector(i as u64)),
            ],
        )
        .expect("insert doc");
    }
    // Delete most of them — the shape vacuum exists for: a lot of freed
    // space that nothing since has reused, because `page_reuse` is off.
    db.execute("DELETE FROM docs WHERE id % 4 != 0", &[])
        .expect("bulk delete");
    db.checkpoint().expect("checkpoint");
}

/// Everything a caller might read back, snapshotted once so the pre- and
/// post-vacuum runs can be compared directly rather than eyeballed.
struct Snapshot {
    file_size: u64,
    author_count: i64,
    doc_count: i64,
    doc_titles: Vec<String>,
    embedding_of_id_0: Vec<f32>,
}

fn snapshot(path: &Path) -> Snapshot {
    let file_size = fs::metadata(path).expect("stat").len();
    let device = FileDevice::open(path).expect("open");
    let mut db = Database::open_on(device).expect("open");

    let author_count = scalar_int(&mut db, "SELECT COUNT(*) FROM authors");
    let doc_count = scalar_int(&mut db, "SELECT COUNT(*) FROM docs");

    let rows = db
        .query("SELECT title FROM docs ORDER BY id", &[])
        .expect("select titles");
    let doc_titles = rows
        .rows
        .into_iter()
        .map(|row| match &row[0] {
            Value::Text(t) => t.to_string(),
            other => panic!("expected text title, got {other:?}"),
        })
        .collect();

    // The vector column's own data, read straight back — not a nearest-
    // neighbour search (a separate, planner-recognised shape this test does
    // not need), just proof `VECTOR(8)` and its stored values round-trip.
    let embedding_of_id_0 = db
        .query("SELECT embedding FROM docs WHERE id = 0", &[])
        .expect("select embedding");
    let embedding_of_id_0 = match &embedding_of_id_0.rows[0][0] {
        Value::Vector(v) => v.clone(),
        other => panic!("expected a vector, got {other:?}"),
    };

    Snapshot {
        file_size,
        author_count,
        doc_count,
        doc_titles,
        embedding_of_id_0,
    }
}

fn scalar_int(db: &mut Database, sql: &str) -> i64 {
    match &db.query(sql, &[]).expect("query").rows[0][0] {
        Value::Integer(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn schema_data_and_constraints_all_survive_a_vacuum() {
    let db_file = TempDb::new("survives");
    build(db_file.path());

    let before = snapshot(db_file.path());
    assert_eq!(before.author_count, 5);
    assert_eq!(
        before.doc_count, 100,
        "one quarter of 400 survives the delete"
    );

    inlaysql::vacuum(db_file.path()).expect("vacuum");

    let after = snapshot(db_file.path());
    assert_eq!(after.author_count, before.author_count);
    assert_eq!(after.doc_count, before.doc_count);
    assert_eq!(
        after.doc_titles, before.doc_titles,
        "row content and order preserved"
    );
    assert_eq!(
        after.embedding_of_id_0, before.embedding_of_id_0,
        "VECTOR column data round-trips exactly"
    );
    assert!(
        after.file_size < before.file_size,
        "vacuum should shrink a file with this much deleted, unreused space: \
         before = {} bytes, after = {} bytes",
        before.file_size,
        after.file_size
    );

    // The constraints reconstructed inside `CREATE TABLE` are still real,
    // not just present in the schema text.
    let device = FileDevice::open(db_file.path()).expect("open");
    let mut db = Database::open_on(device).expect("open");

    let dup_name = db.execute(
        "INSERT INTO authors (id, name, handle, bio) VALUES (99, 'author 0', 'zzz', NULL)",
        &[],
    );
    assert!(
        dup_name.is_err(),
        "COLLATE NOCASE unique index should still reject 'author 0' against 'Author 0'"
    );

    let dup_handle = db.execute(
        "INSERT INTO authors (id, name, handle, bio) VALUES (98, 'Someone', 'author0', NULL)",
        &[],
    );
    assert!(
        dup_handle.is_err(),
        "the unnamed UNIQUE on handle should still be enforced"
    );

    let null_name = db.execute(
        "INSERT INTO authors (id, name, handle, bio) VALUES (97, NULL, 'zzz2', NULL)",
        &[],
    );
    assert!(
        null_name.is_err(),
        "NOT NULL on name should still be enforced"
    );

    let bad_rating = db.execute(
        "INSERT INTO docs (id, author_id, title, rating, embedding) \
         VALUES (9999, 0, 'x', 9, ?)",
        &[Value::Vector(vector(9999))],
    );
    assert!(
        bad_rating.is_err(),
        "CHECK (rating BETWEEN 1 AND 5) should still hold"
    );

    let default_bio = db
        .query(
            "INSERT INTO authors (id, name, handle) VALUES (96, 'Defaulted', 'defaulted') \
             RETURNING bio",
            &[],
        )
        .expect("insert with default");
    assert_eq!(
        default_bio.rows[0][0],
        Value::Text("unknown".to_string().into()),
        "DEFAULT ('unknown') should still apply"
    );
}

/// A database vacuum never touched — no delete, so nothing to reclaim — must
/// come out with exactly the same query answers, proving the reconstruction
/// is not merely *plausible* on the deleted-data case above but faithful in
/// general.
#[test]
fn a_vacuum_with_nothing_to_reclaim_still_reproduces_the_database_exactly() {
    let db_file = TempDb::new("no-op");
    let device = FileDevice::open(db_file.path()).expect("open");
    let mut db = Database::open_on(device).expect("open");
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
        &[],
    )
    .expect("create table");
    for i in 0..20 {
        db.execute(
            "INSERT INTO t (id, v) VALUES (?, ?)",
            &[Value::Integer(i), Value::Text(format!("row {i}").into())],
        )
        .expect("insert");
    }
    db.checkpoint().expect("checkpoint");
    drop(db);

    let before = snapshot_simple(db_file.path());
    inlaysql::vacuum(db_file.path()).expect("vacuum");
    let after = snapshot_simple(db_file.path());
    assert_eq!(before, after);
}

fn snapshot_simple(path: &Path) -> Vec<(i64, String)> {
    let device = FileDevice::open(path).expect("open");
    let mut db = Database::open_on(device).expect("open");
    db.query("SELECT id, v FROM t ORDER BY id", &[])
        .expect("select")
        .rows
        .into_iter()
        .map(|row| {
            let id = match &row[0] {
                Value::Integer(n) => *n,
                other => panic!("expected integer, got {other:?}"),
            };
            let v = match &row[1] {
                Value::Text(t) => t.to_string(),
                other => panic!("expected text, got {other:?}"),
            };
            (id, v)
        })
        .collect()
}

/// `Database::open` creates a missing file rather than erroring — the right
/// default in general, the wrong one for `vacuum`: a typo'd path must not
/// silently "vacuum" a database that never existed into being.
#[test]
fn a_missing_path_is_refused_rather_than_silently_created() {
    let db_file = TempDb::new("missing");
    assert!(
        !db_file.path().exists(),
        "TempDb must not have created the file yet"
    );

    let result = inlaysql::vacuum(db_file.path());
    assert!(
        result.is_err(),
        "vacuum on a path that does not exist must fail, not create an empty database"
    );
    assert!(
        !db_file.path().exists(),
        "vacuum must not have created the file as a side effect of refusing"
    );
}
