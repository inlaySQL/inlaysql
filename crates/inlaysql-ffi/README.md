# inlaysql-ffi — the C ABI

A C ABI over the file-backed engine: one shared library that any
FFI-capable language loads and drives in-process, no server. This is the
"SQLite-like adapter" boundary — the thing a `pdo_inlaysql` extension, a
Python `ctypes` module, a Ruby FFI gem, a .NET P/Invoke binding or a Java
FFM wrapper would be a thin skin over.

## Build

```sh
cargo build -p inlaysql-ffi --release
# → target/release/libinlaysql_ffi.dylib  (macOS)
# → target/release/libinlaysql_ffi.so     (Linux)
# → target/release/inlaysql_ffi.dll       (Windows)
```

`include/inlaysql.h` is the C header, hand-written to match `src/lib.rs`
(the surface is seven functions; a `cbindgen` build dependency would weigh
more than the header).

## The surface

| Function | Purpose |
| --- | --- |
| `inlaysql_open(path)` | Open (create if absent); returns a handle or NULL |
| `inlaysql_open_read_only(path)` | Open read-only; file must exist; writes refused |
| `inlaysql_exec(h, sql, params, &out)` | Run one statement; JSON result via `out` |
| `inlaysql_last_error()` | The engine's own message for the last failure on this thread |
| `inlaysql_free_string(s)` | Free a string the ABI handed out |
| `inlaysql_close(h)` | Close and free |
| `inlaysql_version()` | Engine version, static |

`params` is a JSON array string or NULL; a nested array of numbers is a
vector. The result shapes are identical to the WASM surface's
(`{"kind":"ddl"}`, `{"kind":"written","rows":n}`, `{"columns":…,"rows":…}`),
which is deliberate: documentation and demos written against one describe
the other, and both are pinned to it by tests.

## Working examples

- **PHP** — [`examples/poc.php`](examples/poc.php): PHP's built-in FFI ext
  loads the dylib, creates a table, inserts, selects, and prints an engine
  error verbatim. Run it:
  ```sh
  cargo build -p inlaysql-ffi --release
  php examples/poc.php ../../target/release/libinlaysql_ffi.dylib
  ```
  (Python `ctypes`, Ruby FFI, .NET P/Invoke and Java FFM all consume the
  same header with their own ~30-line loader; PRs for each are welcome.)

## Rules, stated where the caller reads them

- **One handle, one thread at a time.** The engine handle is `!Send` today;
  the header says so, and the same rule the Rust API documents (one handle
  per thread for concurrent access) applies unchanged.
- **Errors are text, not codes.** `INLAYSQL_ERR` plus
  `inlaysql_last_error()` carrying the engine's message verbatim. The
  project refuses to number errors — a C error enum would be a second
  vocabulary to keep in sync with the engine's.
- **Refusals are loud.** `:memory:` is refused by name, non-UTF-8 is
  refused (not U+FFFD'd), malformed params name what was seen, and a
  read-only handle's refusal names the statement. A failure this crate can
  produce is one its error message explains.

## Tests

`src/tests.rs` calls the exported functions through their C signatures with
raw pointers, exactly as an FFI consumer would: round trips, the result
shapes pinned literally against the WASM surface's, error paths, bad
handles, read-only refusals, and close/reopen.
