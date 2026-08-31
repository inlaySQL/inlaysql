"""Batch-insert harness for the UNKNOWN scoreboard cells: B rows per
statement, autocommitted, against MySQL and PostgreSQL over a unix socket,
with commits-per-fsync recorded alongside throughput.

The scoreboard's own rule for this machine: commits-per-fsync is the
noise-resistant metric, so it is bracketed with the same SHOW GLOBAL STATUS /
pg_stat_wal instrumentation the published OLTP tables use:

* MySQL: `Innodb_os_log_fsyncs` (the counter the `Handler_commit`-not-
  `Com_commit` lesson already vetted for autocommit workloads).
* PostgreSQL: `pg_stat_wal.wal_sync`, with `pg_stat_force_next_flush()`
  before each read — the stale-stats defect found and fixed for the OLTP
  driver applies here too.

The InlaySQL side of this cell is measured by
`cargo run --release -p inlaysql-bench --bin sql_shapes -- --mode batch`
against the same BATCH and STATEMENTS-per-rep shape, with c/fsync read from
the coordinator counters.

Durability is aligned: MySQL `innodb_flush_log_at_trx_commit=1`, PostgreSQL
`synchronous_commit=on`, InlaySQL `Durability::Full` — one commit, one
barrier, every statement.

Env: TARGET (`mysql`|`postgres`), REPS (default 5), BATCH (rows per
statement, default 100), STATEMENTS (statements per rep, default 100),
OUT (JSON results file, optional).
"""

from __future__ import annotations

import json
import os
import time

import mysql.connector
import psycopg

TARGET = os.environ.get("TARGET", "mysql")
REPS = int(os.environ.get("REPS", "5"))
BATCH = int(os.environ.get("BATCH", "100"))
STATEMENTS = int(os.environ.get("STATEMENTS", "100"))
OUT = os.environ.get("OUT", "")


def connect():
    if TARGET == "mysql":
        return mysql.connector.connect(
            unix_socket="/sockets/mysqld.sock",
            user="root",
            password="root",
            database="bench",
            autocommit=True,
        )
    return psycopg.connect(
        host="/sockets", dbname="bench", user="postgres", password="postgres", autocommit=True
    )


def status_fsyncs(cur) -> int:
    if TARGET == "mysql":
        cur.execute("SHOW GLOBAL STATUS LIKE 'Innodb_os_log_fsyncs'")
        return int(cur.fetchall()[0][1])
    cur.execute("SELECT wal_sync FROM pg_stat_wal")
    return int(cur.fetchall()[0][0])


def pg_force_flush(cur) -> None:
    if TARGET == "postgres":
        cur.execute("SELECT pg_stat_force_next_flush()")


def main() -> None:
    conn = connect()
    with conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS batch")
        cur.execute("CREATE TABLE batch (id BIGINT PRIMARY KEY, n BIGINT)")
        if TARGET == "mysql":
            # ANALYZE TABLE returns a result set the C-extension cursor
            # refuses to step over ("Unread result found") — consume it.
            cur.execute("ANALYZE TABLE batch")
            if cur.description is not None:
                cur.fetchall()

        placeholders = ",".join(["(%s, %s)"] * BATCH)
        sql = f"INSERT INTO batch (id, n) VALUES {placeholders}"

        print(
            f"batch-insert: {TARGET}, {BATCH} rows/statement, {STATEMENTS} statements/rep, "
            f"{REPS} reps, {BATCH * STATEMENTS} rows per rep",
            flush=True,
        )

        rows_per_rep: list[float] = []
        stmt_rates: list[float] = []
        cfsyncs: list[float] = []

        for rep in range(REPS):
            base = rep * BATCH * STATEMENTS + 1
            started = time.perf_counter()
            fsyncs_before = status_fsyncs(cur)
            for s in range(STATEMENTS):
                first = base + s * BATCH
                flat = []
                for r in range(first, first + BATCH):
                    flat.extend((r, r % 1000))
                cur.execute(sql, flat)
            pg_force_flush(cur)
            fsyncs_after = status_fsyncs(cur)
            elapsed = time.perf_counter() - started

            rows = BATCH * STATEMENTS
            fsyncs = fsyncs_after - fsyncs_before
            rows_per_rep.append(rows / elapsed)
            stmt_rates.append(STATEMENTS / elapsed)
            cfs = STATEMENTS / max(fsyncs, 1)
            cfsyncs.append(cfs)
            print(
                f"rep {rep}: {rows / elapsed:,.0f} rows/s  {STATEMENTS / elapsed:,.0f} commits/s  "
                f"c/fsync {STATEMENTS}/{fsyncs} = {cfs:.2f}",
                flush=True,
            )

        # Lost-write check through a fresh statement on the same connection:
        # the row count has to be exactly what the reps claimed to commit.
        cur.execute("SELECT COUNT(*) FROM batch")
        count = int(cur.fetchall()[0][0])
        expected = REPS * BATCH * STATEMENTS
        if count != expected:
            raise SystemExit(f"lost writes: expected {expected} rows, table holds {count}")

    def median(vals):
        s = sorted(vals)
        n = len(s)
        return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2

    print(
        f"\nMEDIANS {TARGET}: rows/s {median(rows_per_rep):,.0f} "
        f"({min(rows_per_rep):,.0f}–{max(rows_per_rep):,.0f})  "
        f"commits/s {median(stmt_rates):,.0f}  c/fsync {median(cfsyncs):.2f}"
    )

    if OUT:
        with open(OUT, "w") as handle:
            json.dump(
                {
                    "target": TARGET,
                    "batch": BATCH,
                    "reps": REPS,
                    "rows_per_s_median": median(rows_per_rep),
                    "commits_per_s_median": median(stmt_rates),
                    "c_per_fsync_median": median(cfsyncs),
                },
                handle,
                indent=2,
            )
        print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
