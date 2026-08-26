"""InlaySQL as an `ann-benchmarks` algorithm.

`ann-benchmarks` (github.com/erikbern/ann-benchmarks) is the field's common
ground for approximate nearest neighbour search: fixed datasets published as
HDF5 with **precomputed ground-truth neighbours**, one recall/QPS protocol, and
a plugin interface every engine implements. This file is InlaySQL's plugin.

Everything in `bench/` other than this directory is our harness, our corpus and
our oracle. That is stated plainly in `BENCHMARK.md`, and it is still a
limitation: a benchmark whose corpus, protocol and ground truth all come from
the engine's own authors cannot be checked by anyone who did not write it. This
directory exists to remove all three at once — external corpus, external ground
truth, external protocol.

The seam: the MySQL wire protocol
---------------------------------

`fit`/`query` reach the engine through `inlaysql serve --mysql` over a normal
MySQL client connection, running ordinary SQL:

    CREATE TABLE items (id INTEGER PRIMARY KEY, embedding VECTOR(<dim>))
    CREATE INDEX items_embedding ON items (embedding)
    INSERT INTO items (id, embedding) VALUES (0, vector('[...]')), ...
    SELECT id, vector_score(embedding, vector('[...]')) AS score
      FROM items ORDER BY score DESC LIMIT <n>

There is no private API here and no Rust written for the benchmark. That is the
point: the number has to be what a user gets, not what an internal entry point
can be made to do. It is also the only seam that reaches the engine from
Python — InlaySQL ships no Python binding and no C API, so the alternatives are
this or the MCP JSON-RPC server, which is row-limited and meant for language
models.

It is the same shape as `ann-benchmarks`' own `pgvector` module, which drives a
local PostgreSQL over `psycopg`. Both pay a loopback round trip per query that
an in-process index does not. See `bench/README.md` for what that costs here.

What the engine does not expose, and what this file does about it
----------------------------------------------------------------

* **Cosine only.** `vector_score` is cosine similarity and the SQL surface has
  no Euclidean or inner-product scorer. So of the standard datasets only the
  `-angular` ones can be answered at all; `sift-128-euclidean` and
  `fashion-mnist-784-euclidean` — the two usual starters — are refused here
  rather than run against a ground truth built with a metric the engine does
  not implement.
* **No `ef_search` knob.** pgvector's module sweeps `SET hnsw.ef_search`.
  InlaySQL has `HnswParams` in Rust but does not surface `m`,
  `ef_construction` or `ef_search` in SQL, in DDL, or as a session variable.
  The *one* query-time dial a SQL user has is the `LIMIT`, because the search
  width is `max(ef_search, k * ef_search_multiplier)` = `max(64, 2k)`
  (`inlaysql_core::hnsw::HnswParams::ef_for`). So `set_query_arguments` sweeps
  an **over-fetch factor**: ask for `n * over_fetch` rows, keep the first `n`.
  That is a real recall/latency curve and it is the only one reachable without
  recompiling, which is a finding about the engine, not about this file.
* **No parameter binding for embeddings.** The MySQL wire refuses a bound
  embedding — a string parameter into a `VECTOR` column fails with 1366
  (`column is VECTOR(4) but the value is TEXT`), and `vector_score(embedding,
  ?)` fails the same way. Every embedding therefore crosses the wire as a
  `vector('[...]')` decimal-text literal that the server re-parses. That is
  inside the timed region on purpose, because it is inside a user's timed
  region too, and `get_additional()` reports how much of the query time it is.

Nothing here is tuned to flatter the engine. If a number is bad it is reported.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

import numpy

try:  # inside an ann-benchmarks checkout
    from ann_benchmarks.algorithms.base.module import BaseANN
except ImportError:  # standalone, driven by bench/ann/run.py

    class BaseANN:  # minimal stand-in with the same contract
        def done(self) -> None:
            pass

        def get_memory_usage(self):
            return None

        def fit(self, X) -> None:
            pass

        def query(self, q, n):
            return []

        def batch_query(self, X, n) -> None:
            self.res = [self.query(q, n) for q in X]

        def get_batch_results(self):
            return self.res

        def get_additional(self):
            return {}

        def __str__(self) -> str:
            return self.name


# `%.9g` is the shortest decimal form that round-trips every `f32` exactly, so
# the corpus the engine indexes is bit-for-bit the corpus the ground truth was
# computed over. `repr()` would also round-trip but writes the `f64` widening
# (488 bytes/row at dim 25 against 300), and every one of those bytes is wire
# traffic the server then re-parses.
FLOAT_FORMAT = "%.9g"

# How much SQL text one `INSERT` carries. The server reports
# `max_allowed_packet` = 64 MiB; this is far below it, and large enough that
# per-statement overhead is not what the build measures.
BATCH_BYTES = 2 * 1024 * 1024

# How much a single transaction may *write*. This is an engine limit, not a
# choice: under autocommit each `INSERT` is its own transaction, and a
# transaction that dirties more than one WAL region is refused with
#
#     1030: transaction does not fit the write-ahead log (N > 1048576 bytes)
#
# `WAL_BLOCKS` (256) x `DEFAULT_PAGE_SIZE` (4096) = 1 MiB, in
# `inlaysql_core::wal`. There is no server flag for it. Bulk loading over the
# wire therefore has to be split, and the split has to be by *pages dirtied*,
# which is more than the payload — a row straddles pages, and the primary key's
# B-tree is written too. `_flush` targets this and halves on refusal rather
# than trusting the estimate, so a dimension this constant was never tried at
# still loads.
WAL_BUDGET_BYTES = 640 * 1024

# `inlaysql serve --mysql` prints the address it bound to on stderr. `--port 0`
# asks the OS for a free one, so this is the only way to learn it.
ADDRESS = re.compile(r"over the MySQL protocol on (\S+):(\d+)")


def _binary() -> str:
    """The `inlaysql` command, however this checkout has one."""
    explicit = os.environ.get("INLAYSQL_BIN")
    if explicit:
        return explicit
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    built = os.path.join(root, "target", "release", "inlaysql")
    if os.path.exists(built):
        return built
    found = shutil.which("inlaysql")
    if found:
        return found
    raise RuntimeError(
        "no `inlaysql` binary: set INLAYSQL_BIN, or run "
        "`SDKROOT=$(xcrun --show-sdk-path) cargo build --release -p inlaysql-mcp "
        "--bin inlaysql`"
    )


def _rss_kb(pid: int):
    """Resident set size of `pid`, in kilobytes, or None.

    The index lives in the *server's* address space, not this process's, so
    `BaseANN.get_memory_usage`'s `psutil.Process()` — which reads the caller —
    would report the size of a numpy array and call it an index. `ps` is used
    rather than psutil so this file has no dependency ann-benchmarks does not
    already install.
    """
    try:
        out = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True, timeout=10
        )
        return float(out.stdout.strip())
    except Exception:  # noqa: BLE001 — a missing number is reported as missing
        return None


class InlaySQL(BaseANN):
    """InlaySQL's HNSW index, reached over its own MySQL wire protocol."""

    def __init__(self, metric: str, method_param: dict | None = None):
        if metric != "angular":
            # Refused rather than approximated. `vector_score` is cosine
            # similarity; a euclidean dataset's `neighbors` array is the truth
            # for a different question, and scoring one against the other
            # would produce a number that looks like recall and is not.
            raise NotImplementedError(
                f"InlaySQL scores vectors with cosine similarity only, so it cannot answer a "
                f"'{metric}' dataset. Use an -angular dataset (glove-*-angular, "
                f"nytimes-256-angular, random-*-angular)."
            )
        method_param = method_param or {}
        unknown = set(method_param) - {"quantization"}
        if unknown:
            raise ValueError(f"unknown build parameters {sorted(unknown)}")
        # The one build-time knob the SQL surface has: `VECTOR(d)` against
        # `VECTOR(d, INT8)`. There is no `m` and no `ef_construction` to set.
        self.quantization = method_param.get("quantization", "exact")
        if self.quantization not in ("exact", "int8"):
            raise ValueError("quantization must be 'exact' or 'int8'")
        self.metric = metric
        self.over_fetch = 1
        self.name = f"InlaySQL(quantization={self.quantization}, over_fetch=1)"

        self._process = None
        self._directory = None
        self._connection = None
        self._cursor = None
        self._dim = None
        self._plan = None
        self._baseline_rss = None
        # Split out of ann-benchmarks' single `build_time`, which cannot show
        # which half dominates. Filled in by `fit`.
        self.load_seconds = 0.0
        self.graph_seconds = 0.0
        # True when INLAYSQL_HOST/PORT point at a server this object did not
        # start: there is no pid to read, so memory is reported as unknown
        # rather than as zero.
        self._external = False
        self._format_seconds = 0.0
        self._queries = 0

    # ------------------------------------------------------------------ server

    def _start_server(self) -> None:
        """Bring up `inlaysql serve --mysql` on a port the OS picks."""
        host = os.environ.get("INLAYSQL_HOST")
        port = os.environ.get("INLAYSQL_PORT")
        user = os.environ.get("INLAYSQL_USER", "root")
        password = os.environ.get("INLAYSQL_PASSWORD", "ann")
        if host and port:
            # Someone else is running the server (a container, a shell). Use it
            # and do not manage its lifetime.
            self._host, self._port = host, int(port)
            self._external = True
        else:
            self._directory = tempfile.mkdtemp(prefix="inlaysql-ann-")
            database = os.path.join(self._directory, "ann.inlay")
            environment = dict(os.environ, INLAYSQL_ANN_PASSWORD=password)
            self._process = subprocess.Popen(
                [
                    _binary(), "serve", "--mysql", database,
                    "--port", "0",
                    "--bind", "127.0.0.1",
                    "--user", user,
                    "--password-env", "INLAYSQL_ANN_PASSWORD",
                    # One connection, one statement at a time, and a build that
                    # can take minutes: the default 8-hour idle timeout is
                    # right and the default execution ceiling (none) is what a
                    # long ANN build needs.
                    "--max-connections", "4",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
            )
            self._host, self._port = self._await_address()
            # Keep draining stderr for the rest of the run. Nothing else reads
            # it after the address line, and a 64 KiB pipe buffer that fills up
            # would block the server mid-build with no other symptom.
            threading.Thread(target=self._drain, daemon=True).start()

        import mysql.connector

        last = None
        for _ in range(120):
            try:
                self._connection = mysql.connector.connect(
                    host=self._host,
                    port=self._port,
                    user=user,
                    password=password,
                    autocommit=True,  # DDL is refused inside a transaction
                )
                self._cursor = self._connection.cursor()
                # What an empty server costs, so `get_memory_usage` can report
                # what the *index* costs rather than that plus a fixed process.
                if self._process is not None:
                    self._baseline_rss = _rss_kb(self._process.pid)
                return
            except Exception as error:  # noqa: BLE001
                last = error
                time.sleep(0.5)
        raise RuntimeError(f"inlaysql server never accepted a connection: {last}")

    def _await_address(self) -> tuple[str, int]:
        deadline = time.time() + 60
        while time.time() < deadline:
            line = self._process.stderr.readline()
            if not line:
                if self._process.poll() is not None:
                    raise RuntimeError(
                        f"inlaysql server exited with {self._process.returncode} before binding"
                    )
                continue
            print(f"[inlaysql] {line.rstrip()}", file=sys.stderr)
            match = ADDRESS.search(line)
            if match:
                return match.group(1), int(match.group(2))
        raise RuntimeError("inlaysql server did not report a listening address within 60s")

    def _drain(self) -> None:
        try:
            for line in self._process.stderr:
                print(f"[inlaysql] {line.rstrip()}", file=sys.stderr)
        except Exception:  # noqa: BLE001 — the pipe closing is how this ends
            pass

    # -------------------------------------------------------------------- fit

    def fit(self, X: numpy.ndarray) -> None:
        X = numpy.ascontiguousarray(X, dtype=numpy.float32)
        self._dim = int(X.shape[1])
        self._start_server()

        column = (
            f"VECTOR({self._dim}, INT8)" if self.quantization == "int8" else f"VECTOR({self._dim})"
        )
        self._cursor.execute("DROP TABLE IF EXISTS items")
        self._cursor.execute(f"CREATE TABLE items (id INTEGER PRIMARY KEY, embedding {column})")
        self._cursor.execute("CREATE INDEX items_embedding ON items (embedding)")

        # Rows per statement, bounded by both the wire (SQL text) and the
        # engine (WAL bytes per transaction — see WAL_BUDGET_BYTES). The
        # per-row page cost is the payload plus the row header and the primary
        # key's B-tree entry; measured at ~1.75x the raw `f32` payload at dim
        # 20, so the estimate is deliberately generous and `_flush` halves if
        # it is still wrong.
        per_row_written = self._dim * 4 * 2 + 128
        rows_per_batch = max(1, WAL_BUDGET_BYTES // per_row_written)

        # The load and the graph build are timed apart because they are two
        # different costs with two different fixes, and ann-benchmarks' single
        # `build_time` hides which one dominates.
        loading = time.perf_counter()
        pending: list[str] = []
        size = 0
        for start in range(0, len(X), 4096):
            block = X[start : start + 4096]
            # One C-level pass per block rather than tens of millions of Python
            # format calls. Exact for f32 — see FLOAT_FORMAT.
            literals = numpy.char.mod(FLOAT_FORMAT, block)
            for offset, row in enumerate(literals):
                value = f"({start + offset},vector('[{','.join(row)}]'))"
                pending.append(value)
                size += len(value) + 1
                if len(pending) >= rows_per_batch or size >= BATCH_BYTES:
                    self._flush(pending)
                    pending, size = [], 0
        self._flush(pending)
        self.load_seconds = time.perf_counter() - loading

        # The engine builds the graph on the first *read*, not on insert. Left
        # out of `fit`, the whole build would land on ann-benchmarks' first
        # timed query as an outlier and `build_time` would be the load time
        # alone. This is the same warm-up `crates/inlaysql-bench` does — and
        # what an application has to do too, because there is no SQL that asks
        # for the build explicitly.
        building = time.perf_counter()
        probe = ",".join(numpy.char.mod(FLOAT_FORMAT, X[0]))
        self._cursor.execute(
            f"SELECT id, vector_score(embedding, vector('[{probe}]')) AS score "
            f"FROM items ORDER BY score DESC LIMIT 10"
        )
        self._cursor.fetchall()
        self.graph_seconds = time.perf_counter() - building

        # A row labelled HNSW that was actually a table scan is the most
        # misleading number a vector benchmark can publish, so the plan is read
        # rather than assumed — the check `bench/external/pgvector_driver.py`
        # already makes against PostgreSQL's planner.
        self._cursor.execute(
            f"EXPLAIN SELECT id, vector_score(embedding, vector('[{probe}]')) AS score "
            f"FROM items ORDER BY score DESC LIMIT 10"
        )
        self._plan = " / ".join(str(row[-1]) for row in self._cursor.fetchall())
        if "VECTOR INDEX" not in self._plan:
            raise RuntimeError(
                f"the vector index exists but the planner did not use it: {self._plan}"
            )

    def _flush(self, pending: list[str]) -> None:
        """One `INSERT`, split until it fits the write-ahead log.

        The engine refuses a transaction larger than one WAL region with 1030
        rather than growing the log, so an over-large batch is a size to
        correct, not a failure to report. `crates/inlaysql-bench`'s `batched()`
        does the same thing in Rust by catching `Error::Transaction` and
        committing early; this is that, over the wire.
        """
        if not pending:
            return
        import mysql.connector

        try:
            self._cursor.execute("INSERT INTO items (id, embedding) VALUES " + ",".join(pending))
        except mysql.connector.errors.DatabaseError as error:
            fits = "write-ahead log" in str(error)
            if not fits or len(pending) == 1:
                raise
            middle = len(pending) // 2
            self._flush(pending[:middle])
            self._flush(pending[middle:])

    # ------------------------------------------------------------------ query

    def set_query_arguments(self, over_fetch: int) -> None:
        """The only query-time dial a SQL user has. See the module docstring."""
        self.over_fetch = int(over_fetch)
        self.name = f"InlaySQL(quantization={self.quantization}, over_fetch={self.over_fetch})"
        self._format_seconds = 0.0
        self._queries = 0

    def query(self, v: numpy.ndarray, n: int):
        started = time.perf_counter()
        literal = ",".join(numpy.char.mod(FLOAT_FORMAT, numpy.asarray(v, dtype=numpy.float32)))
        self._format_seconds += time.perf_counter() - started
        self._queries += 1
        self._cursor.execute(
            f"SELECT id, vector_score(embedding, vector('[{literal}]')) AS score "
            f"FROM items ORDER BY score DESC LIMIT {n * self.over_fetch}"
        )
        # `[:n]` because the over-fetch is a way to widen the graph walk, not a
        # licence to return more than `n` answers: ann-benchmarks scores the
        # first `n` and a longer list would be scored as extra free guesses.
        return [row[0] for row in self._cursor.fetchall()[:n]]

    def batch_query(self, X: numpy.ndarray, n: int) -> None:
        """Serial, overriding `BaseANN`'s ThreadPool.

        The default fans queries out across a thread pool, and every one of
        them would reach for the same `mysql.connector` cursor — which is not
        safe to share and would interleave two clients' packets on one socket.
        Running it serially reports honestly that this adapter has no batch
        path rather than crashing or, worse, returning scrambled rows. A real
        one would open a connection per thread; `inlaysql-server` is
        thread-per-connection, so that is a measurement of the server's
        concurrency model and belongs with the server benchmarks in
        `bench/external/server_driver.py`, not here.
        """
        self.res = [self.query(query, n) for query in X]

    # ---------------------------------------------------------------- reporting

    def get_memory_usage(self):
        """Kilobytes the index costs, measured in the server, not here.

        ann-benchmarks records `index_size = get_memory_usage() after fit -
        get_memory_usage() before fit`, and calls the first one before there is
        a server at all. So this reports growth over an empty server rather
        than raw RSS: before `fit` that is 0, after it, the whole difference is
        the load. `BaseANN`'s default would have read *this* process's RSS,
        which holds a numpy array and no index.
        """
        if self._external:
            return None
        if self._process is None:
            # No server yet: nothing is held. ann-benchmarks calls this once
            # before `fit`, and a `None` there would lose the whole reading.
            return 0.0
        current = _rss_kb(self._process.pid)
        if current is None or self._baseline_rss is None:
            return None
        return max(0.0, current - self._baseline_rss)

    def server_rss_kb(self):
        """Raw resident size of the server process, for reporting alongside."""
        return _rss_kb(self._process.pid) if self._process is not None else None

    def get_additional(self) -> dict:
        formatting = (
            self._format_seconds / self._queries * 1e6 if self._queries else 0.0
        )
        return {
            # How much of each query was spent turning the embedding into the
            # decimal text the wire requires, since the engine will not bind
            # one. Reported, not subtracted: a user pays it.
            "inlaysql_literal_format_us": round(formatting, 3),
            "inlaysql_over_fetch": self.over_fetch,
            # `build_time`, split. The load is the wire and the storage engine;
            # the graph is one single-threaded HNSW build the engine defers to
            # the first read.
            "inlaysql_load_seconds": round(self.load_seconds, 3),
            "inlaysql_graph_seconds": round(self.graph_seconds, 3),
            "inlaysql_quantization": self.quantization,
            "inlaysql_plan": self._plan or "",
            "inlaysql_seam": "mysql-wire",
        }

    def done(self) -> None:
        for closer in (self._cursor, self._connection):
            try:
                if closer is not None:
                    closer.close()
            except Exception:  # noqa: BLE001
                pass
        self._cursor = self._connection = None
        if self._process is not None:
            self._process.terminate()
            try:
                self._process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self._process.kill()
            self._process = None
        if self._directory is not None:
            shutil.rmtree(self._directory, ignore_errors=True)
            self._directory = None

    def __str__(self) -> str:
        return self.name
