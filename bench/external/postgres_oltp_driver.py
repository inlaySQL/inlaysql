"""Plain PostgreSQL (no pgvector) on the exported OLTP workload: point reads
and point writes by primary key, matched for durability against InlaySQL's
own commit-per-statement write.

This is a separate driver, against a separate container, from
`pgvector_driver.py` / the `pgvector` service — deliberately. That service
runs with `fsync=off` because it exists to measure query latency, and says so
in `bench/README.md`. Reusing it for a durability-sensitive write comparison
would silently make the least-durable configuration in this file stand in
for PostgreSQL's real answer, which is exactly the kind of thing this repo's
benchmark rules exist to catch. The `postgres` service this driver talks to
runs with `fsync=on` and `synchronous_commit=on` instead — see
`bench/external/compose.yml` and `bench/README.md` for the full rationale.

**It is a server.** Every latency here includes a client/server round trip
over the Docker network that InlaySQL — a library in the caller's own
process — does not pay. That is a genuine structural difference, not a
measurement artifact, and it biases every number in this file toward looking
slower than PostgreSQL would over a faster transport (a Unix socket, or a
real network rather than a container bridge).
"""

from __future__ import annotations

import os
import time

import psycopg

import common

DSN = os.environ.get("PG_OLTP_DSN", "postgresql://postgres:postgres@postgres:5432/bench")


def connect(retries: int = 60) -> psycopg.Connection:
    """Wait for the container to accept connections.

    The database starts in parallel with this driver, so a refused connection
    at second zero is expected and a refused connection at second sixty is a
    failure worth reporting.
    """
    last: Exception | None = None
    for _ in range(retries):
        try:
            return psycopg.connect(DSN, autocommit=True)
        except Exception as error:  # noqa: BLE001
            last = error
            time.sleep(1)
    raise RuntimeError(f"postgres never accepted a connection: {last}")


def measure(workload: common.OltpWorkload) -> None:
    connection = connect()
    with connection.cursor() as cursor:
        cursor.execute("DROP TABLE IF EXISTS kv")
        cursor.execute("CREATE TABLE kv (id BIGINT PRIMARY KEY, body TEXT)")

    insert_sql = "INSERT INTO kv (id, body) VALUES (%s, %s)"
    lookup_sql = "SELECT body FROM kv WHERE id = %s"

    # autocommit=True (set at connect time above) makes every statement its
    # own transaction, committed and synced before it returns — the same
    # one-durable-commit-per-row shape as InlaySQL's non-batched write and
    # the `points` suite's SQLite `journal, sync=FULL, fullfsync` row.
    #
    # `prepare=True` on each call asks psycopg to use a server-side prepared
    # statement rather than reparsing the SQL text every time — the same
    # "prepare once, bind per iteration" methodology `points.rs` uses for
    # InlaySQL and SQLite. psycopg caches the prepared statement handle after
    # the first execution of a given query text, so later calls bind and
    # execute without re-parsing.
    write_timer = common.Timer()
    with connection.cursor() as cursor:
        started = time.perf_counter()
        for identifier, body in workload.rows:

            def run(identifier=identifier, body=body):
                cursor.execute(insert_sql, (identifier, body), prepare=True)

            write_timer.time(run)
        write_elapsed = time.perf_counter() - started
    write_ops_s = len(workload.rows) / write_elapsed

    read_timer = common.Timer()
    with connection.cursor() as cursor:
        started = time.perf_counter()
        for key in workload.lookup_keys:

            def run(key=key):
                cursor.execute(lookup_sql, (key,), prepare=True)
                row = cursor.fetchone()
                assert row is not None, f"point read missed row {key}"

            read_timer.time(run)
        read_elapsed = time.perf_counter() - started
    read_ops_s = len(workload.lookup_keys) / read_elapsed

    connection.close()

    common.write_oltp_result(
        common.CORPUS,
        "postgres",
        "PostgreSQL 17 (fsync=on, synchronous_commit=on)",
        write_ops_s,
        write_timer,
        read_ops_s,
        read_timer,
        "client/server over the compose network: every number here includes a round trip "
        "InlaySQL does not pay; autocommit, so every statement is its own durable transaction, "
        "matched to InlaySQL's non-batched write and to the points suite's SQLite "
        "journal/sync=FULL/fullfsync row — see bench/README.md",
        len(workload.rows),
        len(workload.lookup_keys),
    )


def main() -> None:
    workload = common.load_oltp()
    measure(workload)


if __name__ == "__main__":
    main()
