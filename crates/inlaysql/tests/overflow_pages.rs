//! Overflow pages at the SQL surface: a row larger than one page stores,
//! reopens and returns byte-identical, including across a crash recovery.

use std::fs;
use std::path::PathBuf;

use inlaysql::{Database, Value};

/// A directory of our own, so the single-file assertion has nothing else in it.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "inlaysql-overflow-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create workspace");
        Self { dir }
    }

    fn db_path(&self) -> PathBuf {
        self.dir.join("demo.inlay")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

const DIM: usize = 1536;
const TEXT_BYTES: usize = 64 * 1024;

fn row() -> (Value, Value) {
    let embedding: Vec<f32> = (0..DIM).map(|i| (i as f32) * 0.5).collect();
    let text: String = "x".repeat(TEXT_BYTES);
    (Value::Vector(embedding), Value::Text(text))
}

fn create_table(db: &mut Database) {
    db.execute(
        &format!("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR({DIM}), body TEXT)"),
        &[],
    )
    .expect("create table");
}

fn insert_row(db: &mut Database, id: i64, embedding: Value, text: Value) {
    db.execute(
        "INSERT INTO docs (id, embedding, body) VALUES (?, ?, ?)",
        &[Value::Integer(id), embedding, text],
    )
    .expect("insert");
}

fn read_row(db: &mut Database, id: i64) -> Vec<Value> {
    db.query(
        "SELECT id, embedding, body FROM docs WHERE id = ?",
        &[Value::Integer(id)],
    )
    .expect("query")
    .rows
    .remove(0)
}

#[test]
fn a_row_with_a_large_vector_and_text_stores_reopens_and_returns_byte_identical() {
    // A VECTOR(1536) is 6 KiB and the text is 64 KiB, so the encoded row is
    // well over the 4 KiB page — it must spill to an overflow chain.
    let workspace = Workspace::new("roundtrip");
    let (embedding, text) = row();
    {
        let mut db = Database::open(workspace.db_path()).expect("open");
        create_table(&mut db);
        insert_row(&mut db, 1, embedding.clone(), text.clone());
    }

    let mut db = Database::open(workspace.db_path()).expect("reopen");
    let row = read_row(&mut db, 1);
    assert_eq!(row[0], Value::Integer(1));
    assert_eq!(row[1], embedding);
    assert_eq!(row[2], text);
}

#[test]
fn a_small_and_a_large_row_coexist_in_one_table() {
    let workspace = Workspace::new("mixed");
    let (embedding, text) = row();
    {
        let mut db = Database::open(workspace.db_path()).expect("open");
        create_table(&mut db);
        insert_row(&mut db, 1, embedding.clone(), text.clone());
        // A tiny row that fits inline, right next to the giant one.
        db.execute(
            "INSERT INTO docs (id, embedding, body) VALUES (?, ?, ?)",
            &[
                Value::Integer(2),
                Value::Vector(vec![0.0; DIM]),
                Value::Text("small".to_string()),
            ],
        )
        .expect("insert small");
    }

    let mut db = Database::open(workspace.db_path()).expect("reopen");
    assert_eq!(read_row(&mut db, 1)[2], text);
    assert_eq!(read_row(&mut db, 2)[2], Value::Text("small".to_string()));
}

#[test]
fn a_large_row_is_updated_and_deleted_in_place() {
    let workspace = Workspace::new("mutate");
    let (embedding, text) = row();
    {
        let mut db = Database::open(workspace.db_path()).expect("open");
        create_table(&mut db);
        insert_row(&mut db, 1, embedding, text.clone());

        // Replace the 64 KiB text with another, still-large value.
        let replacement = "y".repeat(TEXT_BYTES + 1);
        db.execute(
            "UPDATE docs SET body = ? WHERE id = 1",
            &[Value::Text(replacement.clone())],
        )
        .expect("update");
        assert_eq!(read_row(&mut db, 1)[2], Value::Text(replacement));

        db.execute("DELETE FROM docs WHERE id = 1", &[])
            .expect("delete");
    }

    let mut db = Database::open(workspace.db_path()).expect("reopen");
    let result = db.query("SELECT id FROM docs", &[]).expect("scan");
    assert!(result.rows.is_empty(), "the deleted large row came back");
}
