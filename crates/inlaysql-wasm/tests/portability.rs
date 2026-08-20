//! A database written in the browser opens in the CLI, and the other way
//! round.
//!
//! This is the claim that makes the WASM build worth having rather than a
//! second, parallel database that happens to speak the same SQL. It runs
//! natively — both sides of it are ordinary Rust — because the thing under test
//! is the byte format, not the JavaScript bindings.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql::{Database as NativeDatabase, Value};
use inlaysql_wasm::MemoryDevice;

#[test]
fn a_database_built_in_memory_opens_natively_from_its_bytes() {
    let device = Rc::new(RefCell::new(MemoryDevice::empty()));
    {
        let mut db = NativeDatabase::open_on(device.clone()).unwrap();
        db.execute(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(3))",
            &[],
        )
        .unwrap();
        db.execute("CREATE INDEX docs_body ON docs (body)", &[])
            .unwrap();
        db.execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(1),
                Value::Text("written in a browser tab".into()),
                Value::Vector(vec![1.0, 0.0, 0.0]),
            ],
        )
        .unwrap();
        db.checkpoint().unwrap();
    }

    // The exported image, as `Database::export` would hand it to JavaScript.
    let image = device.borrow().bytes().to_vec();
    assert!(image.starts_with(b"INLAYSQL"), "not an InlaySQL file");

    // Open it again from those bytes alone.
    let mut reopened = NativeDatabase::open_on(MemoryDevice::from_bytes(&image)).unwrap();
    let rows = reopened
        .query(
            "SELECT id, bm25_score(body, ?) AS score FROM docs ORDER BY score DESC LIMIT 1",
            &[Value::Text("browser".into())],
        )
        .unwrap();
    assert_eq!(rows.rows[0][0], Value::Integer(1));
}

#[test]
fn a_file_written_by_the_native_build_opens_from_memory() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "inlaysql-wasm-portability-{}.inlay",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    {
        let mut db = NativeDatabase::open(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
            .unwrap();
        db.execute(
            "INSERT INTO t (id, body) VALUES (?, ?)",
            &[Value::Integer(7), Value::Text("written by the CLI".into())],
        )
        .unwrap();
        db.checkpoint().unwrap();
    }

    let bytes = std::fs::read(&path).unwrap();
    let mut db = NativeDatabase::open_on(MemoryDevice::from_bytes(&bytes)).unwrap();
    let rows = db.query("SELECT body FROM t WHERE id = 7", &[]).unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![Value::Text("written by the CLI".into())]]
    );

    let _ = std::fs::remove_file(&path);
}
