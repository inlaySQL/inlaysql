//! Build the database the edge worker ships.
//!
//! ```sh
//! cargo run -p inlaysql-wasm --example edge_fixture
//! ```
//!
//! This is the edge story in one file: a retrieval index is **built once,
//! natively** — where you have a model, a corpus and as much time as you like —
//! and then shipped to the edge as a static asset, where the WASM module opens
//! it and answers hybrid queries with no database behind it.
//!
//! So the fixture is deliberately *not* written by the WASM build. It is
//! written by `inlaysql`, the ordinary file-backed database, and the worker
//! opening it is the portability claim being exercised through a real runtime
//! rather than asserted in a comment.
//!
//! It writes `crates/inlaysql-wasm/edge/assets/demo.inlay`, which is
//! git-ignored: it regenerates from `corpus.json` and must never be edited by
//! hand.

use std::path::{Path, PathBuf};

use inlaysql::embedding::hashed_embedding;
use inlaysql::{Database, Value};

/// The corpus the browser demo fetches, compiled in so the two demos hold the
/// same rows without anyone remembering to copy them across.
const CORPUS: &str = include_str!("../www/corpus.json");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (dim, docs) = parse(CORPUS)?;

    let path = fixture_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A fixture is built from scratch every time. Reopening a stale one would
    // append to it, and the worker would ship whatever the last run left.
    let _ = std::fs::remove_file(&path);

    let mut db = Database::open(&path)?;
    db.execute(
        &format!("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR({dim}))"),
        &[],
    )?;
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])?;
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])?;
    for (id, body) in &docs {
        db.execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(*id),
                Value::Text(body.clone().into()),
                Value::Vector(hashed_embedding(body, dim)),
            ],
        )?;
    }
    // Checkpoint so the shipped file carries its BM25 and ANN indexes. Without
    // this the worker rebuilds them on every cold start, which is exactly the
    // latency the edge deployment is trying not to pay.
    db.checkpoint()?;
    drop(db);

    let bytes = std::fs::metadata(&path)?.len();
    println!(
        "{}: {} rows, VECTOR({dim}), {bytes} bytes ({} KiB)",
        path.display(),
        docs.len(),
        bytes / 1024
    );
    Ok(())
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("edge")
        .join("assets")
        .join("demo.inlay")
}

/// Pull `dim` and the rows out of `corpus.json`.
///
/// Hand-rolled rather than `serde`: this crate's dependency list is a shipped
/// WASM module's dependency list, and a build-time example is not a reason to
/// add to it.
fn parse(json: &str) -> Result<(usize, Vec<(i64, String)>), String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| format!("corpus.json is not JSON: {error}"))?;

    let dim = value["dim"]
        .as_u64()
        .ok_or("corpus.json needs a numeric `dim`")? as usize;
    if dim == 0 {
        return Err("corpus.json `dim` must be positive".into());
    }

    let docs = value["docs"]
        .as_array()
        .ok_or("corpus.json needs a `docs` array")?
        .iter()
        .map(|doc| {
            let id = doc["id"].as_i64().ok_or("a doc has no integer `id`")?;
            let body = doc["body"].as_str().ok_or("a doc has no `body` string")?;
            Ok((id, body.to_string()))
        })
        .collect::<Result<Vec<_>, String>>()?;

    if docs.is_empty() {
        return Err("corpus.json holds no docs".into());
    }
    Ok((dim, docs))
}
