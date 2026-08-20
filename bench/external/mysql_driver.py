"""MySQL on the exported OLTP workload: point reads and point writes by
primary key, matched for durability against InlaySQL's own commit-per-
statement write.

**It is a server.** Every latency here includes a client/server round trip
over the Docker network that InlaySQL — a library in the caller's own
process — does not pay. That is a genuine structural difference, not a
measurement artifact, and it biases every number in this file toward looking
slower than MySQL would over a faster transport than a container bridge.

**Durability.** The `mysql` service in `bench/external/compose.yml` runs with
`innodb_flush_log_at_trx_commit=1` (InnoDB's most durable setting — the log
buffer is fsynced at every commit, and is already MySQL's default; set
explicitly so this comparison does not silently depend on that default) and
the binary log disabled (`skip-log-bin`) rather than `sync_binlog=1`:
InlaySQL has no replication log, so a second fsync per commit for one would
be measuring a feature neither engine in this comparison uses. See
`bench/README.md` for the full rationale.
"""

from __future__ import annotations

import os
import time

import mysql.connector

import common

DSN = dict(
    host=os.environ.get("MYSQL_HOST", "mysql"),
    port=int(os.environ.get("MYSQL_PORT", "3306")),
    user=os.environ.get("MYSQL_USER", "root"),
    password=os.environ.get("MYSQL_PASSWORD", "root"),
    database=os.environ.get("MYSQL_DB", "bench"),
)


def connect():
    """Wait for the container to accept connections.

    The database starts in parallel with this driver, so a refused connection
    at second zero is expected and a refused connection at second sixty is a
    failure worth reporting.
    """
    retries = 60
    last: Exception | None = None
    for _ in range(retries):
        try:
            return mysql.connector.connect(**DSN)
        except Exception as error:  # noqa: BLE001
            last = error
            time.sleep(1)
    raise RuntimeError(f"MySQL never accepted a connection: {last}")


def measure(workload: common.OltpWorkload) -> None:
    connection = connect()
    # MySQL's default: every statement not inside an explicit
    # START TRANSACTION is its own transaction, committed (and, with
    # innodb_flush_log_at_trx_commit=1, fsynced) as it runs — the same
    # one-durable-commit-per-row shape as InlaySQL's non-batched write.
    connection.autocommit = True

    with connection.cursor() as cursor:
        cursor.execute("DROP TABLE IF EXISTS kv")
        cursor.execute("CREATE TABLE kv (id BIGINT PRIMARY KEY, body TEXT) ENGINE=InnoDB")

    # `prepared=True` asks the connector for a server-side prepared statement
    # (MySQL's binary protocol COM_STMT_PREPARE/EXECUTE) reused across calls
    # with the same SQL text — the same "prepare once, bind per iteration"
    # methodology `points.rs` uses for InlaySQL and SQLite, rather than
    # reparsing the statement text on every call.
    write_cursor = connection.cursor(prepared=True)
    insert_sql = "INSERT INTO kv (id, body) VALUES (%s, %s)"
    write_timer = common.Timer()
    started = time.perf_counter()
    for identifier, body in workload.rows:

        def run(identifier=identifier, body=body):
            write_cursor.execute(insert_sql, (identifier, body))

        write_timer.time(run)
    write_elapsed = time.perf_counter() - started
    write_ops_s = len(workload.rows) / write_elapsed
    write_cursor.close()

    read_cursor = connection.cursor(prepared=True)
    lookup_sql = "SELECT body FROM kv WHERE id = %s"
    read_timer = common.Timer()
    started = time.perf_counter()
    for key in workload.lookup_keys:

        def run(key=key):
            read_cursor.execute(lookup_sql, (key,))
            row = read_cursor.fetchone()
            assert row is not None, f"point read missed row {key}"

        read_timer.time(run)
    read_elapsed = time.perf_counter() - started
    read_ops_s = len(workload.lookup_keys) / read_elapsed
    read_cursor.close()

    connection.close()

    common.write_oltp_result(
        common.CORPUS,
        "mysql",
        "MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)",
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
