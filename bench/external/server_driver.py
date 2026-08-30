"""Server-to-server: InlaySQL served over its own MySQL wire protocol
(`inlaysql serve --mysql`), driven by the *same* client library
(`mysql.connector`) that drives real MySQL in `mysql_driver.py` — the
apples-to-apples row `BENCHMARK.md` names as missing and `PLAN.md` Phase 5.2
asks for (AHL-489).

Every other OLTP row in this comparison has InlaySQL as a library linked
into the caller's own process, measured against MySQL's and PostgreSQL's
socket round trip — `bench/README.md`'s "structural asymmetry" section
states that plainly rather than hiding it. This driver is the one place
that asymmetry is removed on purpose: it never touches InlaySQL as a
library. It opens a `mysql.connector` connection to `inlaysql-server:3306`
the same way it opens one to `mysql:3306`, using the identical client code
path for parameter binding, prepared-statement handling and result
decoding, because `inlaysql-server` speaks the actual MySQL wire protocol
(`docs/server.md`) rather than an approximation of it.

**Sysbench-shaped, not sysbench.** Prepared point reads by primary key and
single-row durable writes — the same operations `mysql_driver.py` and
`postgres_oltp_driver.py` already measure — run at a couple of connection
counts instead of one, which is the one dimension those two single-connection
drivers do not exercise. The workload is a bounded prefix of the same
exported `oltp-rows.csv` / `oltp-lookup-keys.csv` files every other OLTP
driver reads (`SERVER_ROWS`/`SERVER_LOOKUPS`, defaulting far below the top-
level `ROWS`/`LOOKUPS`) rather than all of it: this driver measures two
engines at every concurrency level where the single-connection drivers
measure one engine once, so reusing the full size unchanged would multiply
an already durable, one-fsync-per-row write phase by
`len(CONCURRENCY_LEVELS) * len(TARGETS)` — minutes, at the top-level
defaults, inside the one hour `trust.yml`'s benchmarks job budgets for
`bench/run.sh` and the rest of `bench/compare.sh` too. Still not a second
experiment — the same rows and the same seeded lookup-key sequence, just not
every one of them.

**Durability.** MySQL is configured exactly as `mysql_driver.py` uses it:
`innodb_flush_log_at_trx_commit=1`, binary log disabled
(`bench/external/compose.yml`). InlaySQL has no separate durability knob —
every commit is synced before the statement returns, the same commit path
the host and containerised OLTP rows already measure; see bench/README.md.

**Driver isolation.** Each connection's workload is executed in a spawned
process rather than a Python thread. That keeps `mysql.connector`'s GIL out
of the connection-count comparison while leaving the server-side model
visible: `inlaysql-server` still owns one OS thread and one `Database` handle
per connection. Process creation and connection setup remain inside the
phase timer, so isolation does not hide startup cost.

**What is not, and cannot be, made comparable even here** — read
"Server-to-server" in bench/README.md before trusting a ratio between these
two engines:

* `inlaysql-server` is thread-per-connection: one OS thread and one
  `Database` handle per connection, blocking I/O, no thread pool
  (`docs/server.md`). MySQL schedules many connections onto a bounded
  worker pool. That is invisible at low concurrency and a real structural
  difference at high concurrency — not a tuning gap either engine can close
  by reconfiguring, and this driver does not try to hide it by picking
  concurrency levels that avoid showing it.
* Both sides here run with one shared user and one shared password, but
  that is this benchmark's own setup, not a capability InlaySQL is missing
  a knob for: InlaySQL has no user table, no grants and no per-table
  permission model at all, where MySQL's is a real feature this comparison
  simply never exercises.
* Neither side negotiates TLS in this comparison, but only one side could:
  MySQL's container has none configured; InlaySQL's wire protocol does not
  implement TLS at all yet and never advertises `CLIENT_SSL`
  (`docs/server.md`).
* PostgreSQL is absent from this table on purpose, not by oversight.
  InlaySQL speaks the MySQL wire protocol, not PostgreSQL's — there is no
  InlaySQL server to put on the other end of a `psycopg` connection, so the
  PostgreSQL row stays in `postgres_oltp_driver.py`'s table, measured
  in-process-vs-server the way every other InlaySQL-vs-PostgreSQL row is.

**Commits-per-fsync, both sides (2026-08-31: the InlaySQL-side instrument
gap this section used to describe is closed).** Each level's write phase
brackets `SHOW GLOBAL STATUS` counters for both targets — the mechanism
metric `SCOREBOARD.md` §6 names, and the one that turns a concurrency
sweep's throughput numbers into a statement about whether group commit is
actually amortising `fsync`s as writers are added, rather than just that
throughput moved.

For `mysql`: `Handler_commit`/`Innodb_os_log_fsyncs`. `Handler_commit`, not
`Com_commit` as originally specified: checked empirically against this same
container, `Com_commit` counts literal `COMMIT` statement text and never
moves under this driver's autocommit-per-statement writes, which would have
silently reported `0/N = 0.0` at every level — `Handler_commit` is the
storage-engine counter that increments on every commit, explicit or
autocommit-implicit, and is the one that actually tracks what this driver
does. See `mysql_driver.py`'s module docstring for the full check.

For `inlaysql-server`: `Inlaysql_normal_commit_tickets`/
`Inlaysql_normal_commit_flushes` (excludes checkpoint-triggered flushes —
the like-for-like pair against MySQL's, since neither MySQL number above
includes a checkpoint-analogous event either) as `commits`/`fsyncs`/
`commits_per_fsync`, plus the checkpoint-inclusive
`Inlaysql_commit_tickets`/`Inlaysql_commit_flushes` reported alongside as
`commits_all`/`fsyncs_all`/`commits_per_fsync_all` in case the two pairs
diverge materially. These four counters are a live `SHOW GLOBAL STATUS`
snapshot of the same `CommitCoordinator` flush/ticket counters
`INLAYSQL_COMMIT_STATS=1` used to only print on process `Drop` — a mechanism
that never fired for a long-running server killed by `SIGTERM`
(`crates/inlaysql-server/src/lib.rs` shares the same keeper handle used for
the file lock into every connection thread; `crates/inlaysql-server/src/
metrics.rs` exposes it). This closes the instrument gap this section used to
describe (`SCOREBOARD.md` §6, `PLAN.md` item 6): before this, the only
available number for InlaySQL's own batching ratio was the in-process
`WRITER_LEVELS` sweep, a different harness (library, real OS threads, no
wire protocol) that could only stand in for, not measure, the server's own
ratio.
"""

from __future__ import annotations

import multiprocessing
import os
import time

import mysql.connector

import common

CONCURRENCY_LEVELS = [
    int(level)
    for level in os.environ.get("SERVER_CONCURRENCY_LEVELS", "1,8").split(",")
    if level.strip()
]

# A bounded slice of the exported workload, independent of the top-level
# ROWS/LOOKUPS `bench/compare.sh` uses for the single-connection OLTP row
# above. That row runs each engine once; this one runs two engines at every
# concurrency level, so reusing ROWS/LOOKUPS unchanged would multiply the
# single-connection row's already-durable, one-fsync-per-row write phase by
# `len(CONCURRENCY_LEVELS) * len(TARGETS)` — at the defaults (20,000 rows)
# that is minutes per phase, and `trust.yml`'s benchmarks job runs this
# alongside `bench/run.sh` and the rest of `compare.sh` inside one 60-minute
# timeout. Still the same exported `oltp-rows.csv` / `oltp-lookup-keys.csv`
# — the "generate the experiment once" rule this file's docstring names —
# just not all of it. Override independently with SERVER_ROWS/SERVER_LOOKUPS.
SERVER_ROWS = int(os.environ.get("SERVER_ROWS", "2000"))
SERVER_LOOKUPS = int(os.environ.get("SERVER_LOOKUPS", "1000"))

# Both targets are reached with the same client library and the same code
# below — only the connection parameters and the table DDL (MySQL's InnoDB
# needs `ENGINE=InnoDB` named explicitly; InlaySQL has one storage engine and
# no such clause) differ.
TARGETS = [
    dict(
        slug="mysql",
        engine="MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)",
        host=os.environ.get("MYSQL_HOST", "mysql"),
        port=int(os.environ.get("MYSQL_PORT", "3306")),
        user=os.environ.get("MYSQL_USER", "root"),
        password=os.environ.get("MYSQL_PASSWORD", "root"),
        database=os.environ.get("MYSQL_DB", "bench"),
        create_table="CREATE TABLE kv (id BIGINT PRIMARY KEY, body TEXT) ENGINE=InnoDB",
    ),
    dict(
        slug="inlaysql-server",
        engine="InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)",
        host=os.environ.get("INLAYSQL_SERVER_HOST", "inlaysql-server"),
        port=int(os.environ.get("INLAYSQL_SERVER_PORT", "3306")),
        user=os.environ.get("INLAYSQL_SERVER_USER", "root"),
        password=os.environ.get("INLAYSQL_SERVER_PASSWORD", "bench"),
        database=None,
        create_table="CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)",
    ),
]

# MySQL's own error number for a rolled-back transaction (`ER_LOCK_DEADLOCK`).
# `docs/server.md` maps InlaySQL's first-committer-wins conflict onto exactly
# this code, on purpose, so a client's existing retry-on-1213 logic already
# does the right thing against this server without knowing it is not MySQL.
DEADLOCK_ERRNO = 1213


def connect(target: dict) -> mysql.connector.MySQLConnection:
    """Wait for the container to accept connections.

    Both services start in parallel with this driver (`inlaysql-server` also
    has to finish a cold `cargo build` the first time its volume is created),
    so a refused connection early on is expected and one after several
    minutes is a failure worth reporting.
    """
    retries = 300
    last: Exception | None = None
    for _ in range(retries):
        try:
            kwargs = dict(
                host=target["host"],
                port=target["port"],
                user=target["user"],
                password=target["password"],
            )
            if target["database"]:
                kwargs["database"] = target["database"]
            return mysql.connector.connect(**kwargs)
        except Exception as error:  # noqa: BLE001
            last = error
            time.sleep(1)
    raise RuntimeError(f"{target['slug']} never accepted a connection: {last}")


def global_status(target: dict, name: str) -> int:
    """One `SHOW GLOBAL STATUS` counter, as an int, read over a fresh
    connection to `target`.

    Used for both targets' commits-per-fsync instrument (`SCOREBOARD.md`
    §6): `mysql`'s `Handler_commit`/`Innodb_os_log_fsyncs` pair
    (`Handler_commit`, not `Com_commit` — see the module docstring above for
    why), and `inlaysql-server`'s `Inlaysql_normal_commit_tickets`/
    `Inlaysql_normal_commit_flushes` (plus the checkpoint-inclusive
    `Inlaysql_commit_tickets`/`Inlaysql_commit_flushes`) — a live counter as
    of 2026-08-31 (`crates/inlaysql-server/src/metrics.rs`), where it used to
    have none: the underlying `CommitCoordinator` flush/ticket counters
    (`crates/inlaysql/src/device.rs`) used to be printed only once, on
    process `Drop`, gated on `INLAYSQL_COMMIT_STATS=1` — which never fired
    for a long-running server killed by the container runtime's `SIGTERM`
    (no signal handler dropped the `Database` gracefully). That gap is now
    closed; see the module docstring above.
    """
    connection = connect(target)
    try:
        with connection.cursor() as cursor:
            cursor.execute(f"SHOW GLOBAL STATUS LIKE '{name}'")
            row = cursor.fetchone()
            assert row is not None, f"{target['slug']}: SHOW GLOBAL STATUS has no {name!r}"
            return int(row[1])
    finally:
        connection.close()


def setup_schema(target: dict) -> None:
    connection = connect(target)
    connection.autocommit = True
    with connection.cursor() as cursor:
        cursor.execute("DROP TABLE IF EXISTS kv")
        cursor.execute(target["create_table"])
    connection.close()


def chunks(items: list, n: int) -> list[list]:
    """Split `items` into `n` contiguous, near-equal, deterministic slices.

    Contiguous rather than round-robin so each thread's write range is
    disjoint from every other's — the shape the concurrency suite in
    `bench/README.md` already uses to keep first-committer-wins conflicts at
    zero rather than measuring lock contention this workload is not about.
    """
    if n <= 1:
        return [list(items)]
    size = len(items) // n
    result = []
    start = 0
    for i in range(n):
        end = start + size if i < n - 1 else len(items)
        result.append(list(items[start:end]))
        start = end
    return result


def run_writes(
    target: dict,
    rows_chunk: list[tuple[int, str]],
) -> tuple[list[float], int]:
    connection = connect(target)
    connection.autocommit = True
    cursor = connection.cursor(prepared=True)
    insert_sql = "INSERT INTO kv (id, body) VALUES (%s, %s)"
    timer = common.Timer()
    retried = 0
    for identifier, body in rows_chunk:
        while True:
            try:

                def run(identifier=identifier, body=body):
                    cursor.execute(insert_sql, (identifier, body))

                timer.time(run)
                break
            except mysql.connector.errors.DatabaseError as error:
                if getattr(error, "errno", None) == DEADLOCK_ERRNO:
                    # A first-committer-wins conflict, retried the way
                    # docs/server.md says a 1213 should be — the disjoint id
                    # ranges above should make this unreachable, so a nonzero
                    # count here is itself worth reporting rather than hiding.
                    retried += 1
                    continue
                raise
    cursor.close()
    connection.close()
    return timer.samples, retried


def run_reads(target: dict, keys_chunk: list[int]) -> list[float]:
    connection = connect(target)
    connection.autocommit = True
    cursor = connection.cursor(prepared=True)
    lookup_sql = "SELECT body FROM kv WHERE id = %s"
    timer = common.Timer()
    for key in keys_chunk:

        def run(key=key):
            cursor.execute(lookup_sql, (key,))
            row = cursor.fetchone()
            assert row is not None, f"point read missed row {key}"

        timer.time(run)
    cursor.close()
    connection.close()
    return timer.samples


def measure_concurrency(target: dict, workload: common.OltpWorkload, concurrency: int) -> dict:
    setup_schema(target)

    # Use independent processes rather than threads. `mysql.connector` keeps
    # enough Python-side work in its prepared-cursor path that threaded
    # concurrency can be GIL-bound; process isolation is the property this
    # comparison needs to measure. `spawn` is explicit so the benchmark does
    # not inherit a connector, socket, or partially initialised module state
    # on Unix, and it is also the portable start method for the Linux image.
    context = multiprocessing.get_context("spawn")
    row_chunks = chunks(workload.rows, concurrency)
    key_chunks = chunks(workload.lookup_keys, concurrency)

    # Commits-per-fsync (`SCOREBOARD.md` §6), both targets as of 2026-08-31
    # — see `global_status`'s docstring and the module docstring above for
    # which counters back which target, and for the instrument-gap history.
    # `counter_names` is `(commits, fsyncs)` for the like-for-like ratio;
    # `counter_names_all`, only present for `inlaysql-server`, is the
    # checkpoint-inclusive pair reported alongside it. Read over a
    # connection outside the worker pool so it brackets the phase without
    # adding a process to the concurrency level being measured.
    counter_names = {
        "mysql": ("Handler_commit", "Innodb_os_log_fsyncs"),
        "inlaysql-server": (
            "Inlaysql_normal_commit_tickets",
            "Inlaysql_normal_commit_flushes",
        ),
    }.get(target["slug"])
    counter_names_all = (
        ("Inlaysql_commit_tickets", "Inlaysql_commit_flushes")
        if target["slug"] == "inlaysql-server"
        else None
    )

    commit_stats = None
    if counter_names is not None:
        commits_name, fsyncs_name = counter_names
        commits_before = global_status(target, commits_name)
        fsyncs_before = global_status(target, fsyncs_name)
        if counter_names_all is not None:
            commits_all_name, fsyncs_all_name = counter_names_all
            commits_all_before = global_status(target, commits_all_name)
            fsyncs_all_before = global_status(target, fsyncs_all_name)

    started = time.perf_counter()
    with context.Pool(processes=concurrency) as pool:
        write_results = pool.starmap(
            run_writes,
            [(target, chunk) for chunk in row_chunks],
        )
    write_elapsed = time.perf_counter() - started

    if counter_names is not None:
        commits_after = global_status(target, commits_name)
        fsyncs_after = global_status(target, fsyncs_name)
        commits_delta = commits_after - commits_before
        fsyncs_delta = fsyncs_after - fsyncs_before
        commit_stats = {
            "commits": commits_delta,
            "fsyncs": fsyncs_delta,
            "commits_per_fsync": commits_delta / fsyncs_delta if fsyncs_delta else 0.0,
        }
        if counter_names_all is not None:
            commits_all_after = global_status(target, commits_all_name)
            fsyncs_all_after = global_status(target, fsyncs_all_name)
            commits_all_delta = commits_all_after - commits_all_before
            fsyncs_all_delta = fsyncs_all_after - fsyncs_all_before
            commit_stats["commits_all"] = commits_all_delta
            commit_stats["fsyncs_all"] = fsyncs_all_delta
            commit_stats["commits_per_fsync_all"] = (
                commits_all_delta / fsyncs_all_delta if fsyncs_all_delta else 0.0
            )

    write_ops_s = len(workload.rows) / write_elapsed
    write_timer = common.Timer()
    write_retries = 0
    for samples, retried in write_results:
        write_timer.samples.extend(samples)
        write_retries += retried

    started = time.perf_counter()
    with context.Pool(processes=concurrency) as pool:
        read_results = pool.starmap(
            run_reads,
            [(target, chunk) for chunk in key_chunks],
        )
    read_elapsed = time.perf_counter() - started
    read_ops_s = len(workload.lookup_keys) / read_elapsed
    read_timer = common.Timer()
    for samples in read_results:
        read_timer.samples.extend(samples)

    return dict(
        concurrency=concurrency,
        write_ops_s=write_ops_s,
        write_timer=write_timer,
        write_retries=write_retries,
        read_ops_s=read_ops_s,
        read_timer=read_timer,
        commit_stats=commit_stats,
    )


def publish(target: dict, levels: list[dict], workload: common.OltpWorkload) -> None:
    """Write one target's already-measured `levels` list in the shape
    `report.py` merges. Split from the measurement loop in `main()` below so
    that loop can interleave *which target is measured* at each concurrency
    level, rather than measuring one target across every level before
    starting the other — see `main()`'s own comment for why that ordering
    matters here.
    """
    notes = (
        "client/server over the compose network, mysql.connector on both sides of this "
        "table — the same client library and code path drives MySQL and InlaySQL here, so "
        "this is the one OLTP row where every engine pays an identical socket round trip; "
        "each connection is a spawned process in this driver, with its own prepared "
        "statement and autocommit session, one durable commit per row; concurrency levels "
        "are disjoint contiguous id/key ranges per connection, not a shared queue. See "
        "bench/README.md's Server-to-server section for the concurrency-model, credential "
        "and TLS asymmetries that remain even so, and for why PostgreSQL has no row in "
        "this table. Where present, commit_stats is the delta of each engine's own commit/"
        "fsync counters bracketing that level's write phase — the commits-per-fsync "
        "instrument, SCOREBOARD.md §6: a ratio rising with concurrency says group commit is "
        "amortising fsyncs across writers, not just that throughput moved. For MySQL: "
        "Handler_commit/Innodb_os_log_fsyncs (Handler_commit, not Com_commit, which never "
        "moves under autocommit-implicit writes — see mysql_driver.py). For inlaysql-server "
        "(live as of 2026-08-31, closing this section's former instrument gap): "
        "commits/fsyncs/commits_per_fsync are Inlaysql_normal_commit_tickets/"
        "Inlaysql_normal_commit_flushes (excludes checkpoint-triggered flushes, the "
        "like-for-like pair against MySQL's); commits_all/fsyncs_all/commits_per_fsync_all "
        "are the checkpoint-inclusive Inlaysql_commit_tickets/Inlaysql_commit_flushes, "
        "reported alongside in case the two diverge materially — see global_status's "
        "docstring and SCOREBOARD.md."
    )
    common.write_server_oltp_result(
        common.CORPUS,
        target["slug"],
        target["engine"],
        levels,
        notes,
        len(workload.rows),
        len(workload.lookup_keys),
    )


def main() -> None:
    full = common.load_oltp()
    # A bounded prefix of the rows, not a fresh draw: still the exact rows
    # `oltp_export` wrote, just not all of them — see SERVER_ROWS above. The
    # lookup-key sequence is `oltp_export`'s own seeded draw across the *full*
    # exported row count, so it is filtered down to the keys that land inside
    # this smaller inserted range (in the sequence's original order) rather
    # than sliced positionally — a positional slice would hand later threads
    # keys for rows this driver never inserted, and every one of them would
    # be a genuine miss, not a measurement. The realised count can come in
    # under SERVER_LOOKUPS when the range shrinks a lot; the written result's
    # own `lookups` field always reports what actually ran.
    rows = full.rows[:SERVER_ROWS]
    inserted = {row_id for row_id, _ in rows}
    lookup_keys = [key for key in full.lookup_keys if key in inserted][:SERVER_LOOKUPS]
    workload = common.OltpWorkload(manifest=full.manifest, rows=rows, lookup_keys=lookup_keys)

    # Interleaved by concurrency level, not target-major (MySQL at every
    # level, then InlaySQL at every level): a same-session drift in the
    # Docker volume's fsync cost or in whichever container the host happens
    # to be favouring at that moment lands on both targets' *same* level
    # this way, instead of piling entirely onto whichever target's turn came
    # second. `BENCHMARK.md`'s own corrections section is why — a
    # target-major (there, driver-major) measurement order produced this
    # project's worst measurement error, traced to exactly this kind of
    # drift. Each level's results are still whichever target ran the pool of
    # OS processes for that level; nothing about the concurrency measurement
    # itself changes, only the order targets take their turn.
    levels_by_target: dict[str, list[dict]] = {target["slug"]: [] for target in TARGETS}
    for concurrency in CONCURRENCY_LEVELS:
        for target in TARGETS:
            levels_by_target[target["slug"]].append(
                measure_concurrency(target, workload, concurrency)
            )
    for target in TARGETS:
        publish(target, levels_by_target[target["slug"]], workload)


if __name__ == "__main__":
    main()
