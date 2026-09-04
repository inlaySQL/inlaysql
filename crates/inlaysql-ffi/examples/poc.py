#!/usr/bin/env python3
"""InlaySQL from Python, through the C ABI — a proof of concept.

This is the "SQLite-like adapter" direction: no server, the file opened
in-process through Python's built-in `ctypes`. It is the whole binding —
nothing else is needed, which is the point.

Build the library first (repo root):
    cargo build -p inlaysql-ffi --release

Run this:
    python3 examples/poc.py [path/to/libinlaysql.dylib|.so] [db path]

The shapes mirror examples/poc.php so the two read as one story.
"""

import ctypes
import json
import os
import sys
import tempfile

# The library path is argv[1]; the default matches the release archive's
# layout and the build target directory.
DEFAULT_LIB = os.path.join(
    os.path.dirname(__file__), "..", "..", "..", "target", "release", "libinlaysql_ffi.dylib"
)
lib_path = sys.argv[1] if len(sys.argv) > 1 else os.path.normpath(DEFAULT_LIB)
db_path = sys.argv[2] if len(sys.argv) > 2 else os.path.join(
    tempfile.gettempdir(), "inlaysql-python-poc.inlay"
)

if not os.path.exists(lib_path):
    sys.exit(f"library not found: {lib_path}\nbuild it: cargo build -p inlaysql-ffi --release")

lib = ctypes.CDLL(lib_path)

# The signatures come from include/inlaysql.h. Declaring them is what turns
# ctypes from "hope the ints line up" into a checked boundary.
lib.inlaysql_open.argtypes = [ctypes.c_char_p]
lib.inlaysql_open.restype = ctypes.c_void_p
lib.inlaysql_open_read_only.argtypes = [ctypes.c_char_p]
lib.inlaysql_open_read_only.restype = ctypes.c_void_p
lib.inlaysql_close.argtypes = [ctypes.c_void_p]
lib.inlaysql_close.restype = None
lib.inlaysql_exec.argtypes = [
    ctypes.c_void_p,
    ctypes.c_char_p,
    ctypes.c_char_p,
    ctypes.POINTER(ctypes.c_char_p),
]
lib.inlaysql_exec.restype = ctypes.c_int
lib.inlaysql_last_error.argtypes = []
lib.inlaysql_last_error.restype = ctypes.c_char_p
lib.inlaysql_free_string.argtypes = [ctypes.c_char_p]
lib.inlaysql_free_string.restype = None
lib.inlaysql_version.argtypes = []
lib.inlaysql_version.restype = ctypes.c_char_p

INLAYSQL_OK = 0
INLAYSQL_ERR_BAD_HANDLE = 2


def open_database(path):
    """Open (creating if needed); raise with the engine's message on failure."""
    handle = lib.inlaysql_open(path.encode())
    if not handle:
        raise RuntimeError(f"open failed: {lib.inlaysql_last_error().decode()}")
    return handle


def exec_sql(handle, sql, params=None):
    """Run one statement; return the decoded JSON, or raise the engine's error."""
    out = ctypes.c_char_p()
    code = lib.inlaysql_exec(
        handle,
        sql.encode(),
        json.dumps(params).encode() if params is not None else None,
        ctypes.byref(out),
    )
    if code == INLAYSQL_ERR_BAD_HANDLE:
        raise RuntimeError("bad handle")
    if code != INLAYSQL_OK:
        raise RuntimeError(f"{lib.inlaysql_last_error().decode()} — while running: {sql}")
    result = json.loads(out.value)
    lib.inlaysql_free_string(out)  # the ABI handed out the string; free it here
    return result


print(f"InlaySQL engine version {lib.inlaysql_version().decode()}")
print(f"database: {db_path}\n")

handle = open_database(db_path)
try:
    r = exec_sql(handle, "CREATE TABLE IF NOT EXISTS docs ("
                             "id INTEGER PRIMARY KEY, title TEXT, body TEXT)")
    print(f"create:      {r['kind']}")

    r = exec_sql(handle, "INSERT INTO docs (title, body) VALUES (?, ?)", [
        "Hello", "from Python over the C ABI — no server, the file is ours.",
    ])
    print(f"insert:      {r['kind']}, {r['rows']} row")

    r = exec_sql(handle, "SELECT id, title, body FROM docs ORDER BY id")
    print(f"select:      {r['rows'][0][0]} {r['rows'][0][1]}")
    print(f"             {r['rows'][0][2]}")

    r = exec_sql(handle, "SELECT COUNT(*) FROM docs")
    print(f"count:       {r['rows'][0][0]}")

    try:
        exec_sql(handle, "SELECT * FROM no_such_table")
    except RuntimeError as e:
        print(f"error path:  InlaySQL error: {e}")
finally:
    lib.inlaysql_close(handle)

print("\nOK — Python drove the engine in-process through the C ABI.")
