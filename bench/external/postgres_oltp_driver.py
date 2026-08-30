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

**Commits-per-fsync.** The write phase also brackets
`pg_stat_database.xact_commit` and `pg_stat_wal.wal_sync` (PG14+; the pinned
`postgres:17` image has both) and reports their delta ratio — the mechanism
metric `SCOREBOARD.md` §6 names. Expected near 1.0 at this driver's one
connection; see `mysql_driver.py`'s matching note for why it becomes
interesting once `server_driver.py` reads it at concurrency instead.

Each read is preceded by `pg_stat_force_next_flush()` (PG15+) — found
necessary empirically (see `_force_stats_flush`'s docstring): PostgreSQL
batches a backend's own statistics updates and flushes them opportunistically
rather than at every commit, so reading immediately after a fast write phase
without forcing a flush first measured a stale `0/0` here even though the
rows had genuinely committed.
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


def _force_stats_flush(connection) -> None:
    """`pg_stat_force_next_flush()` (PG15+) — force this backend's pending
    statistics out before the read that follows.

    Found necessary empirically, not assumed: PostgreSQL's cumulative
    statistics system lets a backend batch its own pending counter updates
    and flush them opportunistically rather than synchronously at every
    transaction's commit, so a fast, back-to-back write phase followed
    immediately by a `pg_stat_database`/`pg_stat_wal` read on the same
    connection can see a stale snapshot — measured directly against this
    same container, a short write phase with no forced flush read back
    `commits: 0, fsyncs: 0` even though the rows were genuinely committed
    (confirmed by re-querying a couple of seconds later, once the backend's
    own timer let the flush through). Without this call the commits-per-fsync
    ratio would be silently wrong in the same "looks like a real answer, is
    actually a stale one" way `mysql_driver.py`'s `Com_commit` mistake was.
    """
    with connection.cursor() as cursor:
        cursor.execute("SELECT pg_stat_force_next_flush()")


def xact_commit(connection) -> int:
    """`pg_stat_database.xact_commit` for the connected database — the
    number of transactions committed here, cumulative since the cluster
    started (or the stats were last reset)."""
    _force_stats_flush(connection)
    with connection.cursor() as cursor:
        cursor.execute(
            "SELECT xact_commit FROM pg_stat_database WHERE datname = current_database()"
        )
        row = cursor.fetchone()
        assert row is not None, "pg_stat_database has no row for current_database()"
        return int(row[0])


def wal_sync(connection) -> int:
    """`pg_stat_wal.wal_sync` (PG14+) — the number of times WAL was synced to
    disk via `issue_xlog_fsync`, cluster-wide (there is one row, not one per
    database). Used with `xact_commit` above for the commits-per-fsync
    instrument, `SCOREBOARD.md` §6; both are exposed by `postgres:17` (the
    image this compose file pins) with no config change."""
    _force_stats_flush(connection)
    with connection.cursor() as cursor:
        cursor.execute("SELECT wal_sync FROM pg_stat_wal")
        row = cursor.fetchone()
        assert row is not None, "pg_stat_wal has no row"
        return int(row[0])


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
    commits_before = xact_commit(connection)
    syncs_before = wal_sync(connection)
    with connection.cursor() as cursor:
        started = time.perf_counter()
        for identifier, body in workload.rows:

            def run(identifier=identifier, body=body):
                cursor.execute(insert_sql, (identifier, body), prepare=True)

            write_timer.time(run)
        write_elapsed = time.perf_counter() - started
    commits_after = xact_commit(connection)
    syncs_after = wal_sync(connection)
    write_ops_s = len(workload.rows) / write_elapsed

    # The commits-per-fsync instrument (`SCOREBOARD.md` §6), PostgreSQL's
    # half: `Δxact_commit / Δwal_sync` over the write phase just timed. 1.0 is
    # expected at one connection; see `mysql_driver.py`'s matching note.
    commits_delta = commits_after - commits_before
    syncs_delta = syncs_after - syncs_before
    commit_stats = {
        "commits": commits_delta,
        "fsyncs": syncs_delta,
        "commits_per_fsync": commits_delta / syncs_delta if syncs_delta else 0.0,
    }

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
        "journal/sync=FULL/fullfsync row — see bench/README.md. commit_stats is the delta of "
        "pg_stat_database.xact_commit/pg_stat_wal.wal_sync bracketing the write phase; at one "
        "connection expect ~1.0 (nothing to batch with) — see SCOREBOARD.md §6",
        len(workload.rows),
        len(workload.lookup_keys),
        commit_stats,
    )


def main() -> None:
    workload = common.load_oltp()
    measure(workload)


if __name__ == "__main__":
    main()
