//! InlaySQL in a browser tab or an edge worker.
//!
//! # Why this is small
//!
//! Nothing had to be ported. `inlaysql-core` is `no_std` and reaches the
//! outside world only through traits, so it already compiled to `wasm32`
//! before this crate existed — the project rule about the core doing no I/O
//! and reading no clock is what made a WASM build a matter of supplying a
//! backend rather than a matter of rewriting the engine.
//!
//! This crate supplies that backend and a JavaScript surface.
//!
//! # Persistence
//!
//! The database lives in a `Vec<u8>` — the *same byte layout* the native
//! build writes to a file. [`Database::export`] hands those bytes to
//! JavaScript, and [`Database::open`] takes them back:
//!
//! ```js
//! // Save to the origin-private file system.
//! const root = await navigator.storage.getDirectory();
//! const file = await root.getFileHandle("app.inlay", { create: true });
//! const writable = await file.createWritable();
//! await writable.write(db.export());
//! await writable.close();
//!
//! // Load it again.
//! const bytes = new Uint8Array(await (await file.getFile()).arrayBuffer());
//! const db = Database.open(bytes);
//! ```
//!
//! Persistence is deliberately *not* implemented in Rust here. OPFS's
//! synchronous access handles only exist inside a worker, and binding them
//! would put a worker requirement on every embedder — including edge runtimes
//! that have no OPFS at all but do have a key-value store. Handing the bytes
//! across is six lines of JavaScript, works everywhere, and keeps the WASM
//! module free of any assumption about where its file lives.
//!
//! The bytes are a real InlaySQL database: one written in a browser opens in
//! the CLI, and one written by the CLI opens in a browser.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod device;

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory};
use inlaysql_core::{Engine, TreeStorage, Value};
use serde_json::{json, Value as Json};
use wasm_bindgen::prelude::*;

pub use device::MemoryDevice;

/// An InlaySQL database held in memory.
#[wasm_bindgen]
pub struct Database {
    engine: Engine,
    /// The same device the engine writes through, so the image can be handed
    /// back to JavaScript without unpicking the engine.
    device: Rc<RefCell<MemoryDevice>>,
}

#[wasm_bindgen]
impl Database {
    /// A new, empty database.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<Database, JsError> {
        Self::from_device(MemoryDevice::empty())
    }

    /// Open a database from bytes produced by [`Database::export`] (or by the
    /// native build — it is the same format).
    pub fn open(bytes: &[u8]) -> Result<Database, JsError> {
        Self::from_device(MemoryDevice::from_bytes(bytes))
    }

    fn from_device(device: MemoryDevice) -> Result<Database, JsError> {
        let device = Rc::new(RefCell::new(device));
        let storage = TreeStorage::open_on(device.clone()).map_err(to_js)?;
        Ok(Database {
            device,
            engine: Engine::open(
                Box::new(storage),
                Box::new(MemIndexFactory),
                // A logical clock, not `Date.now()`. Nothing in the engine
                // depends on wall-clock time for results, and a WASM module
                // that never asks the host for the time is one fewer thing for
                // an edge runtime to deny it.
                Box::new(LogicalClock::new()),
            )
            .map_err(to_js)?,
        })
    }

    /// Run a statement. `params` is a JSON array; an inner array of numbers is
    /// a vector.
    ///
    /// Returns `{ "kind": "ddl" }`, `{ "kind": "written", "rows": n }` or the
    /// same shape [`Database::query`] returns.
    pub fn execute(&mut self, sql: &str, params: Option<String>) -> Result<String, JsError> {
        let params = parse_params(params.as_deref())?;
        let outcome = self.engine.execute(sql, &params).map_err(to_js)?;
        Ok(match outcome {
            inlaysql_core::Outcome::Ddl => json!({ "kind": "ddl" }).to_string(),
            inlaysql_core::Outcome::Written(rows) => {
                json!({ "kind": "written", "rows": rows }).to_string()
            }
            inlaysql_core::Outcome::Rows(rows) => render(&rows),
        })
    }

    /// Run a statement that returns rows, as `{ columns, rows }`.
    pub fn query(&mut self, sql: &str, params: Option<String>) -> Result<String, JsError> {
        let params = parse_params(params.as_deref())?;
        let rows = self.engine.query(sql, &params).map_err(to_js)?;
        Ok(render(&rows))
    }

    /// Write the retrieval indexes into the database, so that reopening the
    /// exported bytes does not have to rebuild them.
    pub fn checkpoint(&mut self) -> Result<(), JsError> {
        self.engine.checkpoint().map_err(to_js)
    }

    /// Committed row changes after `from`, as JSON.
    pub fn changes(&self, from: u64) -> Result<String, JsError> {
        let changes = self.engine.changes(from).map_err(to_js)?;
        Ok(json!({
            "changes": changes.changes.iter().map(|change| json!({
                "version": change.version,
                "table": change.table,
                "id": change.id,
                "kind": change.kind.as_str(),
            })).collect::<Vec<_>>(),
            "version": changes.version,
            "lost": changes.lost(from),
        })
        .to_string())
    }

    /// The raw database bytes, for the embedder to persist however it likes.
    ///
    /// Checkpoints first, so the exported file carries its indexes rather than
    /// forcing whoever opens it to rebuild them.
    pub fn export(&mut self) -> Result<Vec<u8>, JsError> {
        self.engine.checkpoint().map_err(to_js)?;
        Ok(self.device.borrow().bytes().to_vec())
    }

    /// The tables this database knows about, as JSON.
    pub fn schema(&self) -> String {
        json!({
            "tables": self.engine.catalog().tables().map(|table| json!({
                "table": table.name,
                "columns": table.columns.iter().map(|column| json!({
                    "name": column.name,
                    "type": alloc_type_name(&column.ty),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
        .to_string()
    }
}

/// The stand-in embedder, so a demo needs no model.
///
/// This is `inlaysql_core::embedding::hashed_embedding` — the *same* function
/// the CLI and the benchmarks call, not a JavaScript lookalike. A database
/// seeded natively and queried here returns sensible neighbours only because
/// both sides bucket trigrams identically, so the browser gets the real one
/// rather than a reimplementation that drifts.
///
/// Real applications pass their own model's output straight into a `VECTOR`
/// column and never call this.
#[wasm_bindgen]
pub fn embed(text: &str, dim: usize) -> Result<Vec<f32>, JsError> {
    if dim == 0 {
        return Err(JsError::new("embedding dimension must be positive"));
    }
    Ok(inlaysql_core::embedding::hashed_embedding(text, dim))
}

fn alloc_type_name(ty: &inlaysql_core::DataType) -> String {
    match ty {
        inlaysql_core::DataType::Integer => "INTEGER".to_string(),
        inlaysql_core::DataType::Real => "REAL".to_string(),
        inlaysql_core::DataType::Text => "TEXT".to_string(),
        inlaysql_core::DataType::Blob => "BLOB".to_string(),
        inlaysql_core::DataType::Numeric => "NUMERIC".to_string(),
        inlaysql_core::DataType::Vector(dim) => format!("VECTOR({dim})"),
        inlaysql_core::DataType::QuantizedVector(dim) => format!("VECTOR({dim}, INT8)"),
    }
}

fn render(rows: &inlaysql_core::ResultSet) -> String {
    json!({
        "columns": rows.columns,
        "rows": rows.rows.iter().map(|row| {
            row.iter().map(render_value).collect::<Vec<_>>()
        }).collect::<Vec<_>>(),
    })
    .to_string()
}

fn render_value(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Integer(integer) => json!(integer),
        Value::Real(real) => json!(real),
        Value::Text(text) => json!(text.as_str()),
        Value::Blob(bytes) => json!(format!("<{} bytes>", bytes.len())),
        Value::Vector(embedding) => json!(format!("<vector({})>", embedding.len())),
    }
}

/// Parse the JSON parameter array.
///
/// An inner array of numbers is a vector, which is how an embedding crosses
/// the JavaScript boundary without a second encoding.
fn parse_params(params: Option<&str>) -> Result<Vec<Value>, JsError> {
    let Some(params) = params.filter(|params| !params.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let parsed: Json = serde_json::from_str(params)
        .map_err(|error| JsError::new(&format!("params is not JSON: {error}")))?;
    let Json::Array(values) = parsed else {
        return Err(JsError::new("params must be a JSON array"));
    };

    values
        .iter()
        .map(|value| match value {
            Json::Null => Ok(Value::Null),
            Json::Bool(flag) => Ok(Value::Integer(i64::from(*flag))),
            Json::String(text) => Ok(Value::Text(text.clone().into())),
            Json::Number(number) => match number.as_i64() {
                Some(integer) => Ok(Value::Integer(integer)),
                None => number
                    .as_f64()
                    .map(Value::Real)
                    .ok_or_else(|| JsError::new("a parameter is not a representable number")),
            },
            Json::Array(components) => {
                let mut embedding = Vec::with_capacity(components.len());
                for component in components {
                    let Some(component) = component.as_f64() else {
                        return Err(JsError::new(
                            "an array parameter is a vector and must hold only numbers",
                        ));
                    };
                    embedding.push(component as f32);
                }
                Ok(Value::Vector(embedding))
            }
            Json::Object(_) => Err(JsError::new("an object is not a bind parameter")),
        })
        .collect()
}

fn to_js(error: inlaysql_core::Error) -> JsError {
    JsError::new(&error.to_string())
}
