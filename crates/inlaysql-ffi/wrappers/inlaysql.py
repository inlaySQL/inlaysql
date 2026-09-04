"""InlaySQL — the Python wrapper over the C ABI.

One file, standard library only: copy it into your project (or import it
from the release archive) and open a database like SQLite — no server, the
file is yours.

    from inlaysql import connect

    db = connect("app.inlay")                     # creates if absent
    db.run("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)")
    db.run("INSERT INTO users (name) VALUES (?)", ["Ada"])

    result = db.run("SELECT * FROM users")        # {"columns": […], "rows": […]}
    for row in db.query("SELECT id, name FROM users"):
        print(row["name"])                        # rows as dicts

    with connect("app.inlay", readonly=True) as ro:
        ro.run("SELECT ...")                      # writes refused

Read-only mode: the file must already exist and every write is refused.
One handle is one thread at a time (open one per thread), the same rule
SQLite's default connection has. Vector parameters bind as a list or
Float32Array of numbers; vector cells come back as the placeholder
"<vector(n)>" — the raw floats do not cross the boundary in JSON.

Works as a context manager, so the handle closes even on error.

Tested against libinlaysql_ffi from inlaySQL/inlaysql v0.0.1; the C surface
it wraps is documented in include/inlaysql.h beside this file.
"""

from __future__ import annotations

import ctypes
import json
import os
from pathlib import Path
from typing import Any

__all__ = ["connect", "InlaySQLError", "InlaySQL"]
__version__ = "0.0.1"

INLAYSQL_OK = 0
INLAYSQL_ERR_BAD_HANDLE = 2


class InlaySQLError(RuntimeError):
    """The engine's own message, verbatim — there are no numeric codes."""


def _locate_library(explicit: str | None) -> str:
    if explicit:
        return explicit
    names = {
        "Darwin": "libinlaysql_ffi.dylib",
        "Windows": "inlaysql_ffi.dll",
    }.get(system := __import__("platform").system(), "libinlaysql_ffi.so")
    for directory in (Path(__file__).parent, Path.cwd()):
        candidate = directory / names
        if candidate.is_file():
            return str(candidate)
    raise InlaySQLError(
        f"could not find {names} beside {Path(__file__).parent} or the working"
        " directory — pass lib=, or download it from"
        " https://github.com/inlaySQL/inlaysql/releases"
    )


def _load(lib_path: str) -> ctypes.CDLL:
    lib = ctypes.CDLL(lib_path)
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
    return lib


class InlaySQL:
    """An open database. Get one from connect(); don't construct directly."""

    def __init__(self, lib: ctypes.CDLL, handle: int):
        self._lib = lib
        self._handle = handle

    @property
    def version(self) -> str:
        return self._lib.inlaysql_version().decode()

    def run(self, sql: str, params: list[Any] | None = None) -> dict[str, Any]:
        """Run one statement.

        Returns {"kind": "ddl"}, {"kind": "written", "rows": n}, or
        {"columns": […], "rows": [[…], …]} for a SELECT.
        """
        out = ctypes.c_char_p()
        code = self._lib.inlaysql_exec(
            self._handle,
            sql.encode(),
            json.dumps(params).encode() if params is not None else None,
            ctypes.byref(out),
        )
        if code == INLAYSQL_ERR_BAD_HANDLE:
            raise InlaySQLError("bad handle")
        if code != INLAYSQL_OK:
            raise InlaySQLError(f"{self._lib.inlaysql_last_error().decode()} — {sql}")
        result = json.loads(out.value)
        self._lib.inlaysql_free_string(out)
        return result

    def query(self, sql: str, params: list[Any] | None = None) -> list[dict[str, Any]]:
        """Run a SELECT; rows as dicts keyed by column name."""
        result = self.run(sql, params)
        if "columns" not in result:
            raise InlaySQLError(f"not a query: {sql}")
        return [dict(zip(result["columns"], row)) for row in result["rows"]]

    def first(self, sql: str, params: list[Any] | None = None) -> dict[str, Any] | None:
        """First row as a dict, or None."""
        return next(iter(self.query(sql, params)), None)

    def close(self) -> None:
        if getattr(self, "_handle", 0):
            self._lib.inlaysql_close(self._handle)
            self._handle = 0

    def __enter__(self) -> InlaySQL:
        return self

    def __exit__(self, *_) -> None:
        self.close()


def connect(path: str, *, lib: str | None = None, readonly: bool = False) -> InlaySQL:
    """Open the database file at `path`, creating it if it does not exist.

    readonly=True refuses every write and requires the file to already
    exist. lib= points at libinlaysql_ffi explicitly; by default it is
    looked up beside this file, then the working directory.
    """
    library = _load(_locate_library(lib))
    open_fn = library.inlaysql_open_read_only if readonly else library.inlaysql_open
    handle = open_fn(path.encode())
    if not handle:
        raise InlaySQLError(f"open failed: {library.inlaysql_last_error().decode()}")
    return InlaySQL(library, handle)


if __name__ == "__main__":  # a self-test, no arguments needed if the dylib is beside this file
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        db_path = os.path.join(tmp, "selftest.inlay")
        db = connect(db_path)
        assert db.version
        db.run("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        assert db.run("INSERT INTO t (name) VALUES (?)", ["Ada"]) == {
            "kind": "written", "rows": 1,
        }
        assert db.query("SELECT * FROM t") == [
            {"id": 1, "name": "Ada"},
        ]
        assert db.first("SELECT name FROM t WHERE id = ?", [1]) == {"name": "Ada"}
        db.close()
    print(f"inlaysql.py self-test passed (engine {__version__})")
