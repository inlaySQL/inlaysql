//! Tests for the C ABI.
//!
//! These tests call the exported functions exactly as an FFI consumer would
//! — through their `extern "C"` signatures, with raw pointers — because the
//! ABI is the product here, not the Rust types behind it. The property under
//! test is the same one the WASM surface pins: a statement's JSON shape is
//! identical across the engine's foreign surfaces, so documentation and
//! demos written for one describe the others.

use std::ffi::{c_char, c_int, CStr, CString};

use crate::{
    inlaysql_close, inlaysql_exec, inlaysql_free_string, inlaysql_last_error, inlaysql_open,
    inlaysql_open_read_only, inlaysql_version, Handle, INLAYSQL_ERR, INLAYSQL_ERR_BAD_HANDLE,
    INLAYSQL_OK,
};

/// A tiny FFI client: what PHP's FFI or Python's ctypes would build.
struct Client {
    handle: *mut Handle,
}

impl Client {
    fn open(path: &std::path::Path) -> Self {
        let cpath = CString::new(path.to_str().unwrap()).unwrap();
        let handle = unsafe { inlaysql_open(cpath.as_ptr()) };
        assert!(!handle.is_null(), "open failed: {}", last_error());
        Self { handle }
    }

    fn exec(&self, sql: &str, params: Option<&str>) -> Option<String> {
        let sql = CString::new(sql).unwrap();
        let params = params.map(|p| CString::new(p).unwrap());
        let params_ptr = params.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
        let mut out: *mut c_char = std::ptr::null_mut();
        let code = unsafe { inlaysql_exec(self.handle, sql.as_ptr(), params_ptr, &mut out) };
        assert_eq!(
            code,
            INLAYSQL_OK,
            "exec({}) failed: {}",
            sql.to_string_lossy(),
            last_error()
        );
        if out.is_null() {
            return None;
        }
        let json = unsafe { CStr::from_ptr(out) }
            .to_string_lossy()
            .into_owned();
        unsafe { inlaysql_free_string(out) };
        Some(json)
    }

    fn exec_err(&self, sql: &str, params: Option<&str>) -> c_int {
        let sql = CString::new(sql).unwrap();
        let params = params.map(|p| CString::new(p).unwrap());
        let params_ptr = params.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
        unsafe { inlaysql_exec(self.handle, sql.as_ptr(), params_ptr, std::ptr::null_mut()) }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { inlaysql_close(self.handle) };
    }
}

fn last_error() -> String {
    unsafe {
        let ptr = inlaysql_last_error();
        if ptr.is_null() {
            "<no error>".to_string()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

fn tmpdir(name: &str) -> temp::Temp {
    temp::Temp::new(name)
}

/// The smallest test-scoped temp directory, because a build dependency for
/// one `mkdtemp` is the same trade the hand-written header refuses.
mod temp {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub struct Temp {
        path: PathBuf,
        // The field exists so `drop` reads like the cleanup it is.
        _guard: RefCell<()>,
    }

    impl Temp {
        pub fn new(name: &str) -> Self {
            let id = NEXT.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir()
                .join(format!("inlaysql-ffi-{name}-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self {
                path,
                _guard: RefCell::new(()),
            }
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[test]
fn ddl_insert_and_select_round_trip_through_the_abi() {
    let dir = tmpdir("round-trip");
    let db_path = dir.path().join("app.inlay");
    let client = Client::open(&db_path);

    assert_eq!(
        client.exec(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, title TEXT, body TEXT)",
            None
        ),
        Some(r#"{"kind":"ddl"}"#.into()),
    );
    assert_eq!(
        client.exec(
            "INSERT INTO docs (title, body) VALUES (?, ?)",
            Some(r#"["Hello", "a body with \"quotes\" and newline\n"]"#),
        ),
        Some(r#"{"kind":"written","rows":1}"#.into()),
    );
    assert_eq!(
        client.exec("SELECT id, title, body FROM docs WHERE id = ?", Some("[1]")),
        Some(r#"{"columns":["id","title","body"],"rows":[[1,"Hello","a body with \"quotes\" and newline\n"]]}"#.into()),
    );
}

#[test]
fn the_result_shapes_match_the_wasm_surfaces_exactly() {
    let dir = tmpdir("shapes");
    let client = Client::open(&dir.path().join("app.inlay"));

    // The JSON shapes are the product here: docs and demos written against
    // the WASM surface describe this one, and vice versa. Pinned literally.
    client.exec(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, n REAL)",
        None,
    );
    assert_eq!(
        client.exec(
            "INSERT INTO t (name, n) VALUES (?, ?)",
            Some(r#"["ada", 1.5]"#)
        ),
        Some(r#"{"kind":"written","rows":1}"#.into()),
    );
    assert_eq!(
        client.exec("SELECT name, n, 2.0 * n AS doubled FROM t", None),
        Some(r#"{"columns":["name","n","doubled"],"rows":[["ada",1.5,3.0]]}"#.into()),
    );
}

#[test]
fn null_real_and_vector_cells_render_as_the_wasm_surface_does() {
    let dir = tmpdir("cells");
    let client = Client::open(&dir.path().join("app.inlay"));
    client.exec(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x REAL, embedding VECTOR(4))",
        None,
    );
    client.exec(
        "INSERT INTO t (x, embedding) VALUES (?, ?)",
        Some(r#"[null, [0.5, 0.25, 0.125, 0.0625]]"#),
    );
    assert_eq!(
        client.exec("SELECT x, embedding FROM t", None),
        Some(r#"{"columns":["x","embedding"],"rows":[[null,"<vector(4)>"]]}"#.into()),
    );
}

#[test]
fn failures_come_back_as_inlaysql_err_with_the_engines_message() {
    let dir = tmpdir("errors");
    let client = Client::open(&dir.path().join("app.inlay"));
    client.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)", None);

    assert_eq!(client.exec_err("SELECT * FROM missing", None), INLAYSQL_ERR,);
    assert!(
        last_error().contains("missing"),
        "the engine's own message must surface verbatim, got: {}",
        last_error()
    );

    // A syntax error and a parameter-shape error follow the same path — one
    // error channel, the engine's words.
    assert_eq!(client.exec_err("SELEC 1", None), INLAYSQL_ERR);
    assert_eq!(
        client.exec_err("INSERT INTO t (id) VALUES (?)", Some("[not-a-number]")),
        INLAYSQL_ERR,
    );
    assert!(!last_error().is_empty());
}

#[test]
fn a_bad_handle_is_inlaysql_err_bad_handle_not_a_crash() {
    let code = unsafe {
        inlaysql_exec(
            std::ptr::null_mut(),
            c"SELECT 1".as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(code, INLAYSQL_ERR_BAD_HANDLE);
}

#[test]
fn read_only_handles_refuse_writes_and_the_file_must_exist() {
    let dir = tmpdir("read-only");
    let db_path = dir.path().join("app.inlay");

    {
        let writer = Client::open(&db_path);
        writer.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)", None);
    }

    let cpath = CString::new(db_path.to_str().unwrap()).unwrap();
    let reader = unsafe { inlaysql_open_read_only(cpath.as_ptr()) };
    assert!(!reader.is_null(), "read-only open failed: {}", last_error());
    let reader = Client { handle: reader };
    assert!(reader.exec("SELECT COUNT(*) FROM t", None).is_some());
    assert_eq!(reader.exec_err("DELETE FROM t", None), INLAYSQL_ERR);
    assert!(
        last_error().contains("read"),
        "refusal names the read-only handle: {}",
        last_error()
    );

    // A path that does not exist is an error, not a silently empty database —
    // the Rust API's rule, kept at the seam.
    let missing = CString::new(dir.path().join("nope.inlay").to_str().unwrap()).unwrap();
    assert!(unsafe { inlaysql_open_read_only(missing.as_ptr()) }.is_null());
    assert!(!last_error().is_empty());
}

#[test]
fn out_json_may_be_null_for_statements_that_would_return_one() {
    let dir = tmpdir("null-out");
    let client = Client::open(&dir.path().join("app.inlay"));
    let sql = CString::new("SELECT 1").unwrap();
    let code = unsafe {
        inlaysql_exec(
            client.handle,
            sql.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(code, INLAYSQL_OK);
}

#[test]
fn a_null_or_non_utf8_string_argument_is_an_error_not_a_dereference() {
    let dir = tmpdir("null-args");
    let client = Client::open(&dir.path().join("app.inlay"));

    // NULL sql: refused, and last_error says what was wrong.
    let code = unsafe {
        inlaysql_exec(
            client.handle,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(code, INLAYSQL_ERR);
    assert!(last_error().contains("null"), "got: {}", last_error());

    // Invalid UTF-8 in a parameter: refused, not replaced with U+FFFD — a
    // silently mangled WHERE clause is the failure class this project refuses.
    let bad = unsafe { CString::from_vec_unchecked(vec![0xFF, 0xFE]) };
    let good = CString::new("SELECT 1").unwrap();
    let code = unsafe {
        inlaysql_exec(
            client.handle,
            good.as_ptr(),
            bad.as_ptr(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(code, INLAYSQL_ERR);
    assert!(last_error().contains("UTF-8"), "got: {}", last_error());
}

#[test]
fn params_the_reader_cannot_mean_are_refused_with_what_was_seen() {
    let dir = tmpdir("bad-params");
    let client = Client::open(&dir.path().join("app.inlay"));
    client.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)", None);

    for (params, why) in [
        ("{}", "an object is not a bind parameter"),
        ("[1, ", "ended"),
        ("[1 2]", "unexpected"),
        ("not json", "JSON array"),
    ] {
        assert_eq!(
            client.exec_err("SELECT 1", Some(params)),
            INLAYSQL_ERR,
            "params: {params}"
        );
        assert!(
            last_error().contains(why),
            "params {params:?}: expected {why:?} in {:?}",
            last_error()
        );
    }
}

#[test]
fn the_version_string_is_the_crates_version() {
    let version = unsafe { CStr::from_ptr(inlaysql_version()) }.to_string_lossy();
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn opening_a_memory_path_is_refused_by_name() {
    let memory = CString::new(":memory:").unwrap();
    assert!(unsafe { inlaysql_open(memory.as_ptr()) }.is_null());
    // The error is the Rust API's message, which tells the caller the call
    // they actually wanted — not a generic "cannot open".
    assert!(
        last_error().contains("open_in_memory"),
        "got: {}",
        last_error()
    );
}

#[test]
fn state_survives_close_and_reopen() {
    let dir = tmpdir("reopen");
    let db_path = dir.path().join("app.inlay");

    {
        let client = Client::open(&db_path);
        client.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT)", None);
        client.exec("INSERT INTO t (note) VALUES (?)", Some("[\"kept\"]"));
    } // closed here

    let reopened = Client::open(&db_path);
    assert_eq!(
        reopened.exec("SELECT note FROM t", None),
        Some(r#"{"columns":["note"],"rows":[["kept"]]}"#.into()),
    );
}
