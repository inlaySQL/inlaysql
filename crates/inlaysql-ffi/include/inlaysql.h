/*
 * inlaysql.h — the C ABI over the InlaySQL engine.
 *
 * Hand-written to match crates/inlaysql-ffi/src/lib.rs rather than generated:
 * the surface is seven functions, and a generator dependency is weight the
 * header does not need. If the Rust surface changes, change this file in the
 * same commit.
 *
 * Build the library:
 *   cargo build -p inlaysql-ffi --release
 *     → target/release/libinlaysql_ffi.dylib  (macOS)
 *     → target/release/libinlaysql_ffi.so     (Linux)
 *     → target/release/inlaysql_ffi.dll       (Windows)
 *
 * The one contract to internalise: a handle is usable from ONE THREAD AT A
 * TIME. The engine's concrete handle is !Send today, and this header states
 * that rather than letting an FFI caller discover it as data corruption.
 * Concurrent access to one file = open one handle per thread.
 */

#ifndef INLAYSQL_H
#define INLAYSQL_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Return codes for inlaysql_exec. */
#define INLAYSQL_OK            0  /* statement ran; out_json holds the result */
#define INLAYSQL_ERR           1  /* statement failed; inlaysql_last_error says why */
#define INLAYSQL_ERR_BAD_HANDLE 2 /* handle was NULL */

/* An open database. Opaque; one thread at a time. */
typedef struct InlaysqlHandle InlaysqlHandle;

/*
 * Open the database file at `path` (UTF-8, NUL-terminated), creating it if
 * it does not exist. Returns NULL on failure — call inlaysql_last_error().
 * `:memory:` is refused: use a real path, or the Rust API for in-memory.
 */
InlaysqlHandle *inlaysql_open(const char *path);

/*
 * Open for reading only; the file must already exist. Writes are refused.
 * Takes no OS advisory lock, so another process may hold the file too.
 */
InlaysqlHandle *inlaysql_open_read_only(const char *path);

/* Close a handle. NULL is accepted and ignored. */
void inlaysql_close(InlaysqlHandle *handle);

/*
 * Run one statement, binding `params` (a JSON array string, or NULL for no
 * parameters) to its `?` placeholders. A nested JSON array of numbers is a
 * vector parameter, e.g. '[0.1, 0.2, 0.3]'.
 *
 * On INLAYSQL_OK, *out_json (if out_json is not NULL) receives a
 * NUL-terminated JSON string, freed with inlaysql_free_string:
 *
 *   {"kind":"ddl"}                      — schema changed
 *   {"kind":"written","rows":N}         — N rows written
 *   {"columns":["a","b"],"rows":[[1,"x"],[2,"y"]]}  — a result set
 *
 * Vector cells render as "<vector(n)>", blob cells as "<N bytes>" —
 * placeholders, not the data; ask for vectors explicitly in SQL if you need
 * their contents.
 */
int inlaysql_exec(InlaysqlHandle *handle,
                  const char *sql,
                  const char *params,
                  char **out_json);

/*
 * The message of the most recent failure on the calling thread, or NULL.
 * Valid until the next InlaySQL call on that thread; copy it if it must
 * outlive that.
 */
const char *inlaysql_last_error(void);

/* Free a string the ABI handed out (inlaysql_exec's out_json). */
void inlaysql_free_string(char *s);

/* The engine version, e.g. "0.0.1". Static; never freed. */
const char *inlaysql_version(void);

#ifdef __cplusplus
}
#endif

#endif /* INLAYSQL_H */
