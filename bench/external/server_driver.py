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
"""

from __future__ import annotations

import os
import threading
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
    out: dict,
    index: int,
) -> None:
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
    out[index] = (timer, retried)


def run_reads(target: dict, keys_chunk: list[int], out: dict, index: int) -> None:
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
    out[index] = timer


def measure_concurrency(target: dict, workload: common.OltpWorkload, concurrency: int) -> dict:
    setup_schema(target)

    write_out: dict[int, tuple[common.Timer, int]] = {}
    threads = [
        threading.Thread(target=run_writes, args=(target, chunk, write_out, i))
        for i, chunk in enumerate(chunks(workload.rows, concurrency))
    ]
    started = time.perf_counter()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    write_elapsed = time.perf_counter() - started
    write_ops_s = len(workload.rows) / write_elapsed
    write_timer = common.Timer()
    write_retries = 0
    for timer, retried in write_out.values():
        write_timer.samples.extend(timer.samples)
        write_retries += retried

    read_out: dict[int, common.Timer] = {}
    threads = [
        threading.Thread(target=run_reads, args=(target, chunk, read_out, i))
        for i, chunk in enumerate(chunks(workload.lookup_keys, concurrency))
    ]
    started = time.perf_counter()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    read_elapsed = time.perf_counter() - started
    read_ops_s = len(workload.lookup_keys) / read_elapsed
    read_timer = common.Timer()
    for timer in read_out.values():
        read_timer.samples.extend(timer.samples)

    return dict(
        concurrency=concurrency,
        write_ops_s=write_ops_s,
        write_timer=write_timer,
        write_retries=write_retries,
        read_ops_s=read_ops_s,
        read_timer=read_timer,
    )


def measure(target: dict, workload: common.OltpWorkload) -> None:
    levels = [measure_concurrency(target, workload, n) for n in CONCURRENCY_LEVELS]

    notes = (
        "client/server over the compose network, mysql.connector on both sides of this "
        "table — the same client library and code path drives MySQL and InlaySQL here, so "
        "this is the one OLTP row where every engine pays an identical socket round trip; "
        "each connection is its own OS thread with its own prepared statement, autocommit, "
        "one durable commit per row; concurrency levels are disjoint contiguous id/key "
        "ranges per connection, not a shared queue. See bench/README.md's Server-to-server "
        "section for the concurrency-model, credential and TLS asymmetries that remain even "
        "so, and for why PostgreSQL has no row in this table."
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
    for target in TARGETS:
        measure(target, workload)


if __name__ == "__main__":
    main()
