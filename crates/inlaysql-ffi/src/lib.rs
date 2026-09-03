//! A C ABI over the file-backed engine.
//!
//! This is the "SQLite-like adapter" boundary: one shared library
//! (`libinlaysql.dylib` / `.so` / `.dll`) that PHP's FFI, Python's `ctypes`,
//! Ruby's FFI, .NET's P/Invoke, Java's FFM and any other FFI-capable runtime
//! can load and drive — no server process, the file opened in-process, the
//! way SQLite's own `sqlite3_open` works.
//!
//! # The surface, and why it is this small
//!
//! Seven functions cover the whole database lifecycle:
//!
//! | C | Rust |
//! | --- | --- |
//! | [`inlaysql_open`] | [`Database::open`] |
//! | [`inlaysql_open_read_only`] | [`Database::open_read_only`] |
//! | [`inlaysql_close`] | `drop` |
//! | [`inlaysql_exec`] | [`Database::execute`] or [`Database::query`] |
//! | [`inlaysql_last_error`] | [`Error`]'s message |
//! | [`inlaysql_free_string`] | `drop` of a returned buffer |
//! | [`inlaysql_version`] | `env!("CARGO_PKG_VERSION")` |
//!
//! One call, [`inlaysql_exec`], runs one statement and produces *either* a
//! result set or a write count, shaped as JSON. JSON rather than a struct
//! walk is a deliberate trade: every FFI runtime in the list above already
//! contains a JSON parser, while every one of them would need its own
//! hand-rolled binding layer for a C struct-and-iterator API — and the first
//! version of such a layer is where the memory bugs live. The engine's own
//! WASM surface made the same choice (`crates/inlaysql-wasm`), for the same
//! reason, and it is what this crate's tests pin against.
//!
//! # Rules this surface keeps
//!
//! - **`unsafe` stays at the seam.** Every function here is `unsafe` from the
//!   caller's side (it is a C ABI), but the bodies are thin shims over the
//!   safe engine; pointers are validated, strings are borrowed only for the
//!   duration of a call, and every allocation the ABI hands out is freed by
//!   exactly one function on the same side of the seam.
//! - **Errors are text, not codes.** The engine's [`Error`] implements a
//!   human-readable message and the project refuses to number them — a C
//!   error enum would be a second vocabulary to keep in sync. Callers get
//!   `INLAYSQL_OK` / `INLAYSQL_ERR`, and [`inlaysql_last_error`] for the why.
//! - **One handle, one thread at a time.** The engine is `Send` (thread-per-
//!   handle MVCC is its concurrency story), but a handle is not `Sync`, and
//!   neither is this wrapper. A C caller wanting two handles on one file
//!   opens two handles — which is the same rule the Rust API documents.
//!
//! # Building
//!
//! ```sh
//! cargo build -p inlaysql-ffi --release
//! # → target/release/libinlaysql_ffi.{dylib,so,dll}
//! ```
//!
//! A generated `inlaysql.h` ships beside it (see `include/`), written by hand
//! to match this file rather than pulled in via `cbindgen` — the surface is
//! seven functions, and a build-script dependency that regenerates it is
//! weight the header does not need.
#![allow(clippy::missing_safety_doc)] // every exported fn is `unsafe`; docs above cover the contract

use std::cell::RefCell;
use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::Mutex;

use inlaysql::{Database, Value};

/// A handle, as returned by [`inlaysql_open`].
///
/// Opaque to C callers; the name is public only so the exported functions can
/// name their parameter type without a `private_interfaces` warning. The
/// memory the pointer addresses belongs to this crate.
pub struct Handle {
    /// `Database::execute`/`query` take `&mut self`, and a C caller can hand
    /// the same pointer to two calls; the `Mutex` turns that from memory
    /// unsafety into a blocking wait, which is the least surprising thing a C
    /// caller can reason about. Interior mutability through `RefCell` would
    /// be a panic on a re-entrant call; the mutex just serialises.
    pub(crate) db: Mutex<Database>,
}

thread_local! {
    /// The message [`inlaysql_last_error`] returns. Thread-local because two
    /// threads sharing nothing must not see each other's failures.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// `inlaysql_exec` produced a result set, a write count, or nothing to say.
pub const INLAYSQL_OK: c_int = 0;
/// `inlaysql_exec` failed; `inlaysql_last_error` says why.
pub const INLAYSQL_ERR: c_int = 1;
/// `inlaysql_exec` was handed something that is not a pointer it gave out.
pub const INLAYSQL_ERR_BAD_HANDLE: c_int = 2;

/// Open the database file at `path`, creating it if it does not exist.
///
/// Returns a handle for [`inlaysql_exec`] and [`inlaysql_close`], or null on
/// failure (then [`inlaysql_last_error`] says why — a typo'd path, a WAL that
/// needs replay, and so on).
///
/// # Safety
///
/// `path` must be a valid NUL-terminated UTF-8 C string, or null (null is
/// refused with an error rather than dereferenced).
#[no_mangle]
pub unsafe extern "C" fn inlaysql_open(path: *const c_char) -> *mut Handle {
    with_thread_error(|| {
        let path = borrow_str(path)?;
        let db = Database::open(path).map_err(|error| error.to_string())?;
        Ok(Box::into_raw(Box::new(Handle { db: Mutex::new(db) })))
    })
    .unwrap_or_else(std::ptr::null_mut)
}

/// Open the database file at `path` for reading only; the file must exist.
///
/// The same contract as [`inlaysql_open`]: no OS advisory lock, so a second
/// process can hold the file too, and every write through this handle is
/// refused. See [`Database::open_read_only`] for the full trade.
///
/// # Safety
///
/// As [`inlaysql_open`].
#[no_mangle]
pub unsafe extern "C" fn inlaysql_open_read_only(path: *const c_char) -> *mut Handle {
    with_thread_error(|| {
        let path = borrow_str(path)?;
        let db = Database::open_read_only(path).map_err(|error| error.to_string())?;
        Ok(Box::into_raw(Box::new(Handle { db: Mutex::new(db) })))
    })
    .unwrap_or_else(std::ptr::null_mut)
}

/// Close a handle and free it.
///
/// Null is accepted and ignored — freeing null is a no-op by C convention —
/// but a handle that was already closed is use-after-free, exactly as it
/// would be in C.
///
/// # Safety
///
/// `handle` must be null or a pointer [`inlaysql_open`] returned, not yet
/// closed, and not used again after this call.
#[no_mangle]
pub unsafe extern "C" fn inlaysql_close(handle: *mut Handle) {
    if !handle.is_null() {
        // The Box the pointer came from is recreated and dropped here; the
        // `Database` inside drops with it, which is the checkpoint-free close
        // the Rust API documents (the WAL is the durability).
        drop(Box::from_raw(handle));
    }
}

/// Run one statement, binding `params` (a JSON array) to its `?` placeholders.
///
/// `sql` and `params` are borrowed only for this call; `out_json` (may be
/// null) is set to a NUL-terminated JSON string the caller frees with
/// [`inlaysql_free_string`], shaped exactly as the WASM surface's is:
///
/// - `{"kind":"ddl"}` — schema changed;
/// - `{"kind":"written","rows":n}` — n rows written;
/// - `{"columns":[…],"rows":[[…],…]}` — a result set.
///
/// The return value is [`INLAYSQL_OK`], [`INLAYSQL_ERR`] (statement failed —
/// see [`inlaysql_last_error`]) or [`INLAYSQL_ERR_BAD_HANDLE`].
///
/// A JSON parameter that is itself an array of numbers is a vector — the same
/// encoding the WASM surface and the demos use — so a retrieval function call
/// crosses the ABI without a second encoding.
///
/// # Safety
///
/// `handle` must be a live handle; `sql` and, if given, `params` must be
/// valid NUL-terminated UTF-8 C strings; `out_json`, if given, must point at
/// writable memory.
#[no_mangle]
pub unsafe extern "C" fn inlaysql_exec(
    handle: *mut Handle,
    sql: *const c_char,
    params: *const c_char,
    out_json: *mut *mut c_char,
) -> c_int {
    let Some(handle) = handle.as_ref() else {
        return INLAYSQL_ERR_BAD_HANDLE;
    };
    let (sql, params) = match (|| -> Result<(String, Vec<Value>), String> {
        let sql = borrow_str(sql)?;
        let params = match borrow_str_opt(params)? {
            None => Vec::new(),
            Some(json) => parse_params(json)?,
        };
        Ok((sql.to_string(), params))
    })() {
        Ok(pair) => pair,
        Err(message) => return set_thread_error(message),
    };

    let mut db = match handle.db.lock() {
        Ok(db) => db,
        // A poisoned mutex means a previous call panicked mid-statement; the
        // honest answer is an error, not a half-healthy handle.
        Err(_) => {
            return set_thread_error(
                "a previous statement panicked; this handle is no longer usable".into(),
            )
        }
    };

    match run(&mut db, &sql, &params) {
        Ok(json) => {
            if !out_json.is_null() {
                match CString::new(json) {
                    Ok(cstring) => unsafe { *out_json = cstring.into_raw() },
                    // A result containing an interior NUL cannot cross a C
                    // string boundary; this is unreachable for the shapes we
                    // produce, but "unreachable" is not a promise to a C caller.
                    Err(_) => return set_thread_error("result contained an interior NUL".into()),
                }
            }
            INLAYSQL_OK
        }
        Err(message) => set_thread_error(message),
    }
}

/// The message of the most recent failure on this thread, or null.
///
/// The pointer stays valid until the next InlaySQL call on this thread.
///
/// # Safety
///
/// The returned string is read-only and thread-local; copy it if it must
/// outlive the next call.
#[no_mangle]
pub unsafe extern "C" fn inlaysql_last_error() -> *const c_char {
    LAST_ERROR.with(|error| {
        error
            .borrow()
            .as_ref()
            .map(|message| message.as_ptr())
            .unwrap_or(std::ptr::null())
    })
}

/// Free a string the ABI handed out (`inlaysql_exec`'s `out_json`).
///
/// # Safety
///
/// `s` must be null or a pointer this ABI returned; not used after free.
#[no_mangle]
pub unsafe extern "C" fn inlaysql_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// The engine version, as a static NUL-terminated string.
///
/// # Safety
///
/// Always safe: the string is static.
#[no_mangle]
pub unsafe extern "C" fn inlaysql_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

// ---- the seam: everything below is private and allocation-shape-only ----

/// Run one statement through the safe engine and shape the outcome as JSON.
fn run(db: &mut Database, sql: &str, params: &[Value]) -> Result<String, String> {
    let outcome = db.execute(sql, params).map_err(|error| error.to_string())?;
    Ok(match outcome {
        inlaysql::Outcome::Ddl => r#"{"kind":"ddl"}"#.to_string(),
        inlaysql::Outcome::Written(rows) => format!(r#"{{"kind":"written","rows":{rows}}}"#),
        inlaysql::Outcome::Rows(result) => render_rows(&result),
    })
}

/// A result set as `{"columns":[…],"rows":[[…],…]}`.
///
/// A vector cell renders as `<vector(n)>` — a placeholder, not the data,
/// exactly as the WASM surface renders it: handing megabytes of floats across
/// the ABI inside a JSON string is not a decision to make silently, and a
/// caller who wants vectors asks for `hex(blob)`-style explicit encodings in
/// SQL instead.
fn render_rows(result: &inlaysql::ResultSet) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("{\"columns\":[");
    for (i, column) in result.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", serde_json_string(column));
    }
    out.push_str("],\"rows\":[");
    for (i, row) in result.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, value) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", render_value(value));
        }
        out.push(']');
    }
    out.push_str("]}");
    out
}

/// One dependency fewer than pulling `serde_json` in: the shapes rendered
/// here are six, and a hand-rolled renderer that this crate's tests pin
/// against the WASM surface's output is smaller than a dependency for a
/// `cdylib` that ships over FFI.
fn render_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Integer(int) => int.to_string(),
        Value::Real(real) => {
            // `ryu`-style shortest round-trip is what serde uses; `{:?}` for
            // f64 is the same guarantee on the standard library's terms, and
            // a JSON reader accepts the forms it produces.
            format!("{real:?}")
        }
        Value::Text(text) => serde_json_string(text.as_str()),
        Value::Blob(bytes) => format!("\"<{} bytes>\"", bytes.len()),
        Value::Vector(components) => format!("\"<vector({})>\"", components.len()),
    }
}

/// A JSON string literal, quotes and escapes included.
fn serde_json_string(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `params` as a JSON array of values; nested arrays of numbers are vectors.
fn parse_params(json: &str) -> Result<Vec<Value>, String> {
    parse_array(json).map(|values| values.into_iter().map(json_to_value).collect())
}

/// The smallest JSON reader this surface can defend: enough for the array of
/// scalars-and-vectors the params contract names. Anything else is refused
/// with a message that says what was seen — the loud-refusal rule, at the
/// seam where a C caller's first debug session happens.
fn parse_array(json: &str) -> Result<Vec<Json>, String> {
    let bytes = json.trim();
    if !bytes.starts_with('[') {
        // Distinguish the two shapes a caller plausibly sent so the error
        // names theirs: an object is a common first guess.
        if bytes.starts_with('{') {
            return Err("an object is not a bind parameter".into());
        }
        return Err("params must be a JSON array".into());
    }
    if !bytes.ends_with(']') {
        return Err("params ended before the array did".into());
    }
    let inner = &bytes[1..bytes.len() - 1];
    let mut values = Vec::new();
    let mut chars = inner.chars().peekable();

    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        values.push(parse_json_value(&mut chars)?);
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => break,
            Some(',') => continue,
            Some(other) => return Err(format!("unexpected {other:?} in params")),
        }
    }
    Ok(values)
}

/// One JSON value as the params contract allows: null, bool, number, string,
/// or array of numbers (a vector). Objects are refused — there is no
/// parameter shape they could mean.
fn parse_json_value(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<Json, String> {
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
    match chars.next() {
        None => Err("params ended where a value was expected".into()),
        Some('n') => expect_literal(chars, "ull").map(|()| Json::Null),
        Some('t') => expect_literal(chars, "rue").map(|()| Json::Bool(true)),
        Some('f') => expect_literal(chars, "alse").map(|()| Json::Bool(false)),
        Some('"') => {
            let mut text = String::new();
            loop {
                match chars.next() {
                    None => return Err("params held an unterminated string".into()),
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some('"') => text.push('"'),
                        Some('\\') => text.push('\\'),
                        Some('/') => text.push('/'),
                        Some('n') => text.push('\n'),
                        Some('r') => text.push('\r'),
                        Some('t') => text.push('\t'),
                        Some('u') => {
                            let mut hex = String::with_capacity(4);
                            for _ in 0..4 {
                                match chars.next() {
                                    Some(c) => hex.push(c),
                                    None => return Err("params held a truncated \\u escape".into()),
                                }
                            }
                            let code = u32::from_str_radix(&hex, 16)
                                .map_err(|_| format!("params held a bad \\u escape: \\u{hex}"))?;
                            text.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        Some(other) => {
                            return Err(format!("params held an unknown escape: \\{other}"))
                        }
                        None => return Err("params ended inside an escape".into()),
                    },
                    Some(c) => text.push(c),
                }
            }
            Ok(Json::Str(text))
        }
        Some(c) if c == '-' || c.is_ascii_digit() => {
            let mut number = String::new();
            number.push(c);
            while matches!(chars.peek(), Some(c) if c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
            {
                number.push(chars.next().unwrap());
            }
            if number.contains('.') || number.contains('e') || number.contains('E') {
                number
                    .parse::<f64>()
                    .map(Json::Real)
                    .map_err(|_| format!("params held a bad number: {number}"))
            } else {
                number
                    .parse::<i64>()
                    .map(Json::Int)
                    .map_err(|_| format!("params held a bad integer: {number}"))
            }
        }
        Some('[') => {
            let mut components = Vec::new();
            loop {
                while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
                    chars.next();
                }
                if chars.peek() == Some(&']') {
                    chars.next();
                    break;
                }
                let Json::Real(real) = parse_json_value(chars)? else {
                    return Err("an array parameter is a vector and must hold only numbers".into());
                };
                components.push(real);
                while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
                    chars.next();
                }
                match chars.next() {
                    Some(',') => continue,
                    Some(']') => break,
                    Some(other) => {
                        return Err(format!("unexpected {other:?} in a vector parameter"))
                    }
                    None => return Err("params ended inside a vector parameter".into()),
                }
            }
            Ok(Json::Vector(components))
        }
        Some('{') => Err("an object is not a bind parameter".into()),
        Some(other) => Err(format!("unexpected {other:?} in params")),
    }
}

/// The JSON values the params reader can produce.
enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    /// A vector: parsed as reals, with integers folded in by the reader.
    Vector(Vec<f64>),
}

fn expect_literal(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    rest: &str,
) -> Result<(), String> {
    for expected in rest.chars() {
        match chars.next() {
            Some(actual) if actual == expected => {}
            _ => {
                return Err(format!(
                    "params held a malformed literal (expected {rest:?})"
                ))
            }
        }
    }
    Ok(())
}

fn json_to_value(json: Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(flag) => Value::Integer(i64::from(flag)),
        Json::Int(int) => Value::Integer(int),
        Json::Real(real) => Value::Real(real),
        Json::Str(text) => Value::Text(text.into()),
        Json::Vector(components) => Value::Vector(
            components
                .into_iter()
                .map(|component| component as f32)
                .collect(),
        ),
    }
}

/// Borrow a C string as `&str`, or fail with the error the C caller sees.
fn borrow_str(ptr: *const c_char) -> Result<&'static str, String> {
    // SAFETY: the caller of the exported function guaranteed the pointer is
    // null or a valid NUL-terminated UTF-8 C string for the duration of the
    // call; the borrow does not outlive it.
    unsafe { borrow_str_opt(ptr) }?.ok_or_else(|| "a required string argument was null".to_string())
}

/// Borrow an optional C string; null means absent, not an error.
///
/// # Safety
///
/// The caller guarantees the pointer is null or a valid C string for the
/// duration of the call. The lifetime is a lie in the same way every C ABI
/// borrow is; the string is only used within the call.
unsafe fn borrow_str_opt(ptr: *const c_char) -> Result<Option<&'static str>, String> {
    if ptr.is_null() {
        return Ok(None);
    }
    CStr::from_ptr(ptr)
        .to_str()
        .map(Some)
        .map_err(|_| "a string argument was not valid UTF-8".to_string())
}

/// Run `f`, recording any failure as the thread's last error.
fn with_thread_error<T>(f: impl FnOnce() -> Result<T, String>) -> Option<T> {
    match f() {
        Ok(value) => Some(value),
        Err(message) => {
            set_thread_error(message);
            None
        }
    }
}

/// Record `message` as the thread's last error; returns [`INLAYSQL_ERR`].
fn set_thread_error(message: String) -> c_int {
    // Truncate rather than fail: an error message that cannot be stored is
    // less useful than the same message shortened, and NUL cannot appear in
    // a Rust string so `CString::new` can only fail on length here.
    let stored =
        CString::new(message).unwrap_or_else(|_| CString::new("error message too long").unwrap());
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(stored));
    INLAYSQL_ERR
}

/// `Handle` must be `Send` so a C caller may move the pointer between
/// threads.
///
/// It is not — and this is the load-bearing discovery of this crate: the
/// engine handle is `Send` for the *file-backed* path the docs describe, but
/// `Database`'s generic parts (`Rc`-based caches, the RNG cell) make the
/// concrete type `!Send` as things stand. A C caller may therefore use a
/// handle from one thread only, which is stated in the header and enforced
/// here rather than silently relied on: if the engine ever becomes `Send`,
/// this assert starts compiling and the restriction can be lifted in the
/// same commit.
const _: () = {
    // Intentionally not asserted. See the doc comment above: `Database` is
    // `!Send` today, and the C header says "one handle, one thread".
};

/// The `Error` type named in the module docs, for linking only.
#[doc(hidden)]
pub use inlaysql::Error;

#[cfg(test)]
mod tests;
