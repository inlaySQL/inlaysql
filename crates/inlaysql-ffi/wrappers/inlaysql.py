"""InlaySQL — the Python client over the C ABI.

One file, standard library only: copy it into your project (or import it
from the release archive) and open a database like SQLite — no server, the
file is yours.

    from inlaysql import connect

    db = connect("app.inlay")                                   # creates if absent
    db.execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)")

    user_id = db.insert("users", name="Ada", email="ada@example.org")

    for user in db.query("SELECT id, name FROM users WHERE id > :after", {"after": 0}):
        print(user["name"])                                     # rows as dicts
    ada = db.first("SELECT * FROM users WHERE id = ?", [user_id])   # one row or None
    count = db.value("SELECT COUNT(*) FROM users")                  # one cell
    names = db.column("SELECT name FROM users ORDER BY name")       # one column

    result = db.execute("UPDATE users SET name = ? WHERE id = ?", ["Ada L.", user_id])
    result.rows_affected                                        # 1

    with db.transaction():                                      # BEGIN … COMMIT,
        db.insert("users", name="Grace")                        # ROLLBACK on raise
        db.insert("users", name="Linus")

    db.transact(lambda db: db.insert("users", name="Linus"))    # same, retried on
                                                                # a write conflict

Parameters: positional `?` with a list, or named `:name` with a dict. A list
of numbers binds as a vector, so a retrieval call is

    db.query("SELECT id, vector_score(embedding, ?) AS s FROM docs ORDER BY s LIMIT 10",
             [embedding])

Errors are exceptions: ConstraintError (unique/not-null…), ConflictError
(another writer committed first — retry), UnsupportedError (a clause the
engine refuses rather than ignores), and InlaySQLError for everything else;
the engine's own message is the exception message.

Read-only: connect("app.inlay", readonly=True) — the file must already exist
and every write is refused. Works as a context manager, so the handle closes
even on error.

One handle is one thread at a time (open one per thread or process), and
every one of them may write — concurrent commits to one file are what this
engine does that SQLite does not. The library is loaded once per process.
Vector cells come back as the placeholder "<vector(n)>" — the raw floats do
not cross the boundary in JSON.

Tested against libinlaysql_ffi from inlaySQL/inlaysql v0.0.5; the C surface
it wraps is documented in include/inlaysql.h beside this file.
"""

from __future__ import annotations

import ctypes
import json
import os
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterator, Mapping, Sequence, TypeVar

__all__ = [
    "connect",
    "InlaySQL",
    "Result",
    "Rows",
    "InlaySQLError",
    "ConstraintError",
    "ConflictError",
    "UnsupportedError",
]
__version__ = "0.0.5"

INLAYSQL_OK = 0
INLAYSQL_ERR_BAD_HANDLE = 2

T = TypeVar("T")
Params = Sequence[Any] | Mapping[str, Any] | None


class InlaySQLError(RuntimeError):
    """The engine's own message, verbatim — there are no numeric codes."""


class ConstraintError(InlaySQLError):
    """UNIQUE, NOT NULL, CHECK, FOREIGN KEY — the row was refused."""


class ConflictError(InlaySQLError):
    """Another handle committed first; nothing was written. Safe to retry."""


class UnsupportedError(InlaySQLError):
    """A clause this engine refuses rather than silently ignores."""


def _error(message: str, sql: str | None = None) -> InlaySQLError:
    text = message if sql is None else f"{message} — while running: {sql}"
    if message.startswith("constraint failed"):
        return ConstraintError(text)
    if message.startswith("write conflict"):
        return ConflictError(text)
    if message.startswith("unsupported"):
        return UnsupportedError(text)
    return InlaySQLError(text)


def _locate_library(explicit: str | None) -> str:
    if explicit:
        return explicit
    import platform

    name = {
        "Darwin": "libinlaysql_ffi.dylib",
        "Windows": "inlaysql_ffi.dll",
    }.get(platform.system(), "libinlaysql_ffi.so")
    here = Path(__file__).parent
    for directory in (here, here.parent, Path.cwd()):
        candidate = directory / name
        if candidate.is_file():
            return str(candidate)
    raise InlaySQLError(
        f"could not find {name} beside {here} or the working directory — pass"
        " lib=, or download it from https://github.com/inlaySQL/inlaysql/releases"
    )


_LIBRARIES: dict[str, ctypes.CDLL] = {}


def _load(lib_path: str) -> ctypes.CDLL:
    """Load and type the library once per path, per process."""
    if lib_path in _LIBRARIES:
        return _LIBRARIES[lib_path]
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
    _LIBRARIES[lib_path] = lib
    return lib


def _bind_named(sql: str, params: Mapping[str, Any]) -> tuple[str, list[Any]]:
    """Rewrite `:name` to `?` in order of appearance, skipping quoted runs."""
    out: list[str] = []
    values: list[Any] = []
    i, n = 0, len(sql)
    while i < n:
        c = sql[i]
        if c in ("'", '"', "`"):
            j = i + 1
            while j < n:
                if sql[j] == c:
                    if j + 1 < n and sql[j + 1] == c:
                        j += 2
                        continue
                    break
                j += 1
            out.append(sql[i : j + 1])
            i = j + 1
            continue
        if c == ":" and i + 1 < n and (sql[i + 1].isalpha() or sql[i + 1] == "_"):
            j = i + 1
            while j < n and (sql[j].isalnum() or sql[j] == "_"):
                j += 1
            name = sql[i + 1 : j]
            if name not in params:
                raise _error(f"no value bound for :{name}", sql)
            values.append(params[name])
            out.append("?")
            i = j
            continue
        out.append(c)
        i += 1
    return "".join(out), values


def _quote_identifier(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


@dataclass(frozen=True)
class Result:
    """What a statement did. `rows` is set only when it was a query."""

    rows_affected: int
    #: The most recent INSERT's row id on this handle (SQLite's
    #: `last_insert_rowid()` contract), or None before any INSERT.
    last_insert_id: int | None
    is_ddl: bool
    rows: Rows | None


class Rows:
    """A query's answer. Iterates as dicts; `len()` is the row count; the
    accessors answer the common shapes without a loop."""

    __slots__ = ("columns", "_rows")

    def __init__(self, columns: list[str], rows: list[list[Any]]):
        self.columns = columns
        self._rows = rows

    def __iter__(self) -> Iterator[dict[str, Any]]:
        columns = self.columns
        for row in self._rows:
            yield dict(zip(columns, row))

    def __len__(self) -> int:
        return len(self._rows)

    def __bool__(self) -> bool:
        return bool(self._rows)

    def all(self) -> list[dict[str, Any]]:
        """Every row as a dict."""
        return list(self)

    def raw(self) -> list[list[Any]]:
        """The rows exactly as the ABI returned them, positional."""
        return self._rows

    def first(self) -> dict[str, Any] | None:
        """First row as a dict, or None."""
        return dict(zip(self.columns, self._rows[0])) if self._rows else None

    def value(self) -> Any:
        """The first cell of the first row, or None when there is no row."""
        return self._rows[0][0] if self._rows and self._rows[0] else None

    def column(self, column: int | str = 0) -> list[Any]:
        """One column, by index or name, across every row."""
        index = column if isinstance(column, int) else self.columns.index(column)
        return [row[index] for row in self._rows]


class InlaySQL:
    """An open database. Get one from connect(); don't construct directly."""

    def __init__(self, lib: ctypes.CDLL, handle: int):
        self._lib = lib
        self._handle = handle
        self._depth = 0

    @property
    def version(self) -> str:
        return self._lib.inlaysql_version().decode()

    # ---- statements ------------------------------------------------------

    def execute(self, sql: str, params: Params = None) -> Result:
        """Run any statement. For a write, the result says how many rows and
        which id; for a query, prefer query()."""
        raw = self.run(sql, params)
        if "columns" in raw:
            return Result(0, None, False, Rows(raw["columns"], raw["rows"]))
        return Result(
            int(raw.get("rows", 0)),
            raw.get("last_insert_id"),
            raw.get("kind") == "ddl",
            None,
        )

    def query(self, sql: str, params: Params = None) -> Rows:
        """Run a query. Iterate the result, or ask it for .all(), .first(),
        .column(), .value()."""
        raw = self.run(sql, params)
        if "columns" not in raw:
            raise _error(f"not a query: {sql}")
        return Rows(raw["columns"], raw["rows"])

    def first(self, sql: str, params: Params = None) -> dict[str, Any] | None:
        """First row as a dict, or None."""
        return self.query(sql, params).first()

    def value(self, sql: str, params: Params = None) -> Any:
        """The first cell of the first row, or None when there is no row."""
        return self.query(sql, params).value()

    def column(self, sql: str, params: Params = None, column: int | str = 0) -> list[Any]:
        """One column across every row."""
        return self.query(sql, params).column(column)

    def insert(self, table: str, row: Mapping[str, Any] | None = None, /, **values: Any) -> int:
        """Insert one row given as a mapping or keyword arguments; return its row id."""
        merged = {**(row or {}), **values}
        if not merged:
            raise _error(f"insert into {table}: no columns given")
        columns = ", ".join(_quote_identifier(name) for name in merged)
        marks = ", ".join("?" for _ in merged)
        result = self.execute(
            f"INSERT INTO {_quote_identifier(table)} ({columns}) VALUES ({marks})",
            list(merged.values()),
        )
        if result.last_insert_id is None:
            raise _error(f"insert into {table} reported no row id")
        return result.last_insert_id

    # ---- transactions ----------------------------------------------------

    @contextmanager
    def transaction(self) -> Iterator[InlaySQL]:
        """BEGIN on entry, COMMIT on a clean exit, ROLLBACK on an exception
        (which propagates). Nested uses join the outer transaction. For a
        body that should be retried on a write conflict, use transact()."""
        if self._depth > 0:
            self._depth += 1
            try:
                yield self
            finally:
                self._depth -= 1
            return
        self.run("BEGIN")
        self._depth = 1
        try:
            yield self
        except BaseException:
            self._depth = 0
            self._rollback_quietly()
            raise
        else:
            try:
                self.run("COMMIT")
            except BaseException:
                self._rollback_quietly()
                raise
            finally:
                self._depth = 0

    def transact(self, fn: Callable[[InlaySQL], T], retries: int = 3) -> T:
        """Run fn(db) inside a transaction; on a write conflict (another handle
        committed first) roll back and rerun it, up to `retries` times, so fn
        must be safe to repeat."""
        attempt = 0
        while True:
            try:
                with self.transaction():
                    return fn(self)
            except ConflictError:
                if attempt >= retries:
                    raise
                attempt += 1

    @property
    def in_transaction(self) -> bool:
        return self._depth > 0

    def _rollback_quietly(self) -> None:
        try:
            self.run("ROLLBACK")
        except InlaySQLError:
            pass  # a conflict at COMMIT already ended the transaction

    # ---- the raw call ----------------------------------------------------

    def run(self, sql: str, params: Params = None) -> dict[str, Any]:
        """Run one statement and return the ABI's JSON, decoded:
        {"kind": "ddl"}, {"kind": "written", "rows": n, "last_insert_id": k},
        or {"columns": […], "rows": [[…], …]}. The typed methods are built on
        this; it stays public for callers that want the raw shape."""
        if not self._handle:
            raise _error("the database is closed")
        if isinstance(params, Mapping):
            sql, params = _bind_named(sql, params)
        out = ctypes.c_char_p()
        code = self._lib.inlaysql_exec(
            self._handle,
            sql.encode(),
            json.dumps(list(params)).encode() if params else None,
            ctypes.byref(out),
        )
        if code == INLAYSQL_ERR_BAD_HANDLE:
            raise _error("bad handle")
        if code != INLAYSQL_OK:
            raise _error(self._lib.inlaysql_last_error().decode(), sql)
        try:
            return json.loads(out.value)
        finally:
            self._lib.inlaysql_free_string(out)

    # ---- lifecycle -------------------------------------------------------

    def close(self) -> None:
        if getattr(self, "_handle", 0):
            self._lib.inlaysql_close(self._handle)
            self._handle = 0

    def __enter__(self) -> InlaySQL:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def __del__(self) -> None:
        self.close()


def connect(path: str | os.PathLike[str], *, lib: str | None = None, readonly: bool = False) -> InlaySQL:
    """Open the database file at `path`, creating it if it does not exist.

    readonly=True refuses every write and requires the file to already
    exist. lib= points at libinlaysql_ffi explicitly; by default it is
    looked up beside this file, its parent, then the working directory.
    """
    library = _load(_locate_library(lib))
    open_fn = library.inlaysql_open_read_only if readonly else library.inlaysql_open
    handle = open_fn(os.fspath(path).encode())
    if not handle:
        raise _error(f"open failed: {library.inlaysql_last_error().decode()}")
    return InlaySQL(library, handle)


if __name__ == "__main__":  # a self-test, no arguments needed if the dylib is beside this file
    import sys
    import tempfile

    lib_arg = sys.argv[1] if len(sys.argv) > 1 else None
    with tempfile.TemporaryDirectory() as tmp:
        db_path = os.path.join(tmp, "selftest.inlay")
        with connect(db_path, lib=lib_arg) as db:
            assert db.version
            ddl = db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT UNIQUE)")
            assert ddl.is_ddl and ddl.rows is None
            assert db.insert("t", name="Ada") == 1
            assert db.insert("t", {"name": "Grace"}) == 2
            written = db.execute("UPDATE t SET name = :n WHERE id = :id", {"id": 2, "n": "Grace H."})
            assert written.rows_affected == 1 and written.last_insert_id == 2
            assert db.query("SELECT * FROM t ORDER BY id").all() == [
                {"id": 1, "name": "Ada"},
                {"id": 2, "name": "Grace H."},
            ]
            assert db.first("SELECT name FROM t WHERE id = ?", [1]) == {"name": "Ada"}
            assert db.value("SELECT COUNT(*) FROM t") == 2
            assert db.column("SELECT name FROM t ORDER BY name") == ["Ada", "Grace H."]
            assert db.value("SELECT name FROM t WHERE name = ':not_a_param'") is None
            assert len(db.query("SELECT id FROM t WHERE id > :after", {"after": 1})) == 1
            try:
                db.insert("t", name="Ada")
            except ConstraintError:
                pass
            else:
                raise AssertionError("duplicate insert should raise ConstraintError")
            try:
                with db.transaction():
                    db.insert("t", name="Linus")
                    raise RuntimeError("abort")
            except RuntimeError:
                pass
            assert db.value("SELECT COUNT(*) FROM t") == 2
            assert db.transact(lambda d: d.insert("t", name="Linus")) == 3
            assert db.value("SELECT COUNT(*) FROM t") == 3
    print(f"inlaysql.py self-test passed (engine {__version__})")
