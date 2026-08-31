"""Read-shape harnesses for the UNKNOWN scoreboard cells: indexed range scan,
aggregate/GROUP BY, and two-table join, against MySQL and PostgreSQL, over a
matched unix-socket transport.

These fill the cells `SCOREBOARD.md` marks UNKNOWN for want of infrastructure.
The shapes are copied from the Rust suites that produced InlaySQL's side of
the comparison, so a cell is the same schema, the same queries and the same
query count on both engines:

* **range** — `indexed`'s table and range shape: `users (id, email, body)`,
  ROWS fixed-width email keys, an index on `email` built after the rows,
  `WHERE email >= ? AND email < ?` returning exactly RANGE_SIZE rows. The key
  sequence is generated with the same seeded xorshift64* the Rust harness
  uses (`crates/inlaysql-core/src/mem/clock.rs`), so both engines answer the
  identical queries in the identical order.
* **agg_group / agg_scalar** — no Rust suite exists (the scoreboard cell is
  UNKNOWN for every opponent), so the shape is defined here and the InlaySQL
  side measures it through `inlaysql-bench --bin sql_shapes` against the same
  table: GROUP BY over a 100-bucket column, and a whole-table scalar
  aggregate.
* **join_*** — `joins`' four shapes verbatim (both FROM orders, each with and
  without LIMIT), 8 posts per user assigned round-robin, index on
  `posts.user_id` built after the rows, cardinalities refreshed (ANALYZE)
  before preparing.

Rules, pre-fixed before any cell was filled:

* Unix-socket transport for both servers (compose mounts one socket volume
  into `mysql`, `postgres` and `drivers`) — the brief's matched-transport
  requirement. InlaySQL's side is in-process, which *favors* InlaySQL; a
  LOSS recorded under that asymmetry is conservative, a WIN is flagged.
* Durability: MySQL `innodb_flush_log_at_trx_commit=1`, PostgreSQL
  `synchronous_commit=on` — unchanged from the published OLTP tables. These
  workloads commit only during setup, so this mostly pins the load.
* REPS repetitions per shape (default 5), the (shape, rep) schedule shuffled
  Fisher-Yates with a fixed seed so no shape is systematically first.
* Medians and ranges over reps; every result row is a median, never a
  single run.

Env: TARGET (`mysql`|`postgres`), REPS, ROWS (range/aggregate rows, default
100000), QUERIES (queries per shape per rep, default 100), JOIN_ROWS (users
in the join, default 20000), RANGE_SIZE (default 50, `indexed`'s own
constant), OUT (JSON results file, optional).
"""

from __future__ import annotations

import json
import os
import time

import mysql.connector
import psycopg

TARGET = os.environ.get("TARGET", "mysql")
REPS = int(os.environ.get("REPS", "5"))
ROWS = int(os.environ.get("ROWS", "100000"))
QUERIES = int(os.environ.get("QUERIES", "100"))
JOIN_ROWS = int(os.environ.get("JOIN_ROWS", "20000"))
RANGE_SIZE = int(os.environ.get("RANGE_SIZE", "50"))
SEED = int(os.environ.get("SEED", "42"))
PAYLOAD = "x" * int(os.environ.get("PAYLOAD", "64"))
OUT = os.environ.get("OUT", "")

POSTS_PER_USER = 8  # joins.rs's own constant

MASK = (1 << 64) - 1


class SeededRng:
    """The exact xorshift64* from `crates/inlaysql-core/src/mem/clock.rs`."""

    def __init__(self, seed: int) -> None:
        self.state = seed if seed else 0x9E3779B97F4A7C15

    def next_u64(self) -> int:
        x = self.state
        x ^= x >> 12
        x ^= (x << 25) & MASK
        x ^= x >> 27
        self.state = x & MASK
        return (x * 0x2545F4914F6CDD1D) & MASK


def email(i: int) -> str:
    return f"user{i:012}@example.com"


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


def run(cur, sql: str, params=None):
    cur.execute(sql, params)
    # DDL produces no result set; only fetch when there is one.
    if cur.description is not None:
        return cur.fetchall()
    return []


def setup_range(cur) -> None:
    """`indexed`'s table at ROWS scale, plus the 100-bucket column the
    aggregate shapes read. The load and the index build are setup, not
    timing — the same one-transaction load the Rust suites use."""
    run(cur, "DROP TABLE IF EXISTS users")
    run(
        cur,
        "CREATE TABLE users ("
        "id BIGINT PRIMARY KEY, email VARCHAR(64), body VARCHAR(64), n INTEGER)",
    )
    # Batched multi-row inserts of 2000 rows keep the load to a few seconds
    # on either engine; ids are explicit, as in the Rust suites.
    batch: list[tuple] = []
    for id_ in range(1, ROWS + 1):
        batch.append((id_, email(id_), PAYLOAD, id_ % 100))
        if len(batch) == 2000:
            _insert_batch(cur, "users", ["id", "email", "body", "n"], batch)
            batch = []
    if batch:
        _insert_batch(cur, "users", ["id", "email", "body", "n"], batch)
    run(cur, "CREATE INDEX users_email ON users (email)")
    _analyze(cur, "users")


def _insert_batch(cur, table: str, cols: list[str], rows: list[tuple]) -> None:
    # 1000-row chunks: MySQL caps prepared placeholders (65536) and both
    # clients' per-statement parameter encoding degrades well before that.
    ph = "%s"
    for base in range(0, len(rows), 1000):
        chunk = rows[base : base + 1000]
        sql = f"INSERT INTO {table} ({', '.join(cols)}) VALUES " + ",".join(
            "(" + ", ".join([ph] * len(cols)) + ")" for _ in chunk
        )
        flat = [v for row in chunk for v in row]
        cur.execute(sql, flat)


def _analyze(cur, table: str) -> None:
    if TARGET == "mysql":
        run(cur, f"ANALYZE TABLE {table}")
    else:
        run(cur, f"ANALYZE {table}")


def setup_join(cur) -> None:
    """`joins`' two tables verbatim: round-robin user_ids, index built after
    the rows, cardinalities refreshed before prepare."""
    run(cur, "DROP TABLE IF EXISTS posts")
    run(cur, "DROP TABLE IF EXISTS jusers")
    run(cur, "CREATE TABLE jusers (id BIGINT PRIMARY KEY, name VARCHAR(64))")
    run(cur, "CREATE TABLE posts (id BIGINT PRIMARY KEY, user_id INTEGER, title VARCHAR(64))")
    users_rows = [(i, f"user{i}") for i in range(1, JOIN_ROWS + 1)]
    _insert_batch(cur, "jusers", ["id", "name"], users_rows)
    posts_rows = [
        (pid, 1 + ((pid - 1) % JOIN_ROWS), PAYLOAD) for pid in range(1, JOIN_ROWS * POSTS_PER_USER + 1)
    ]
    _insert_batch(cur, "posts", ["id", "user_id", "title"], posts_rows)
    run(cur, "CREATE INDEX posts_user_id ON posts (user_id)")
    _analyze(cur, "posts")
    _analyze(cur, "jusers")


def main() -> None:
    rng = SeededRng(SEED)
    bound = max(ROWS - RANGE_SIZE, 1)
    starts = [1 + rng.next_u64() % bound for _ in range(QUERIES)]
    range_pairs = [(email(s), email(s + RANGE_SIZE)) for s in starts]

    shapes = {
        "range": (
            setup_range,
            "SELECT id, body FROM users WHERE email >= %s AND email < %s",
            range_pairs,
            RANGE_SIZE,
        ),
        "agg_group": (
            None,
            "SELECT n, COUNT(*) FROM users GROUP BY n",
            [() for _ in range(QUERIES)],
            100,
        ),
        "agg_scalar": (
            None,
            "SELECT COUNT(*), MIN(id), MAX(id) FROM users",
            [() for _ in range(QUERIES)],
            1,
        ),
        # The full-join shapes are wrapped in a server-side COUNT(*): a
        # Python client fetching 160k rows per execution measures
        # mysql-connector's per-row cost, not the engine's join, while the
        # InlaySQL side streams rows through a near-zero-cost callback
        # (joins.rs's `query_prepared_each(.., |_| Ok(()))`). The wrapper
        # still produces and discards every joined row server-side — the
        # engine runs the whole plan — it just doesn't serialize them. The
        # small shapes (LIMIT 20, the 50-row range, the 1-100-row
        # aggregates) transfer their rows directly on both sides. Disclosed
        # asymmetry: InlaySQL's published full-join numbers *include* its
        # streaming, so a LOSS recorded for InlaySQL here is conservative.
        "join_pk": (
            None,
            "SELECT COUNT(*) FROM (SELECT posts.id, jusers.name FROM posts JOIN jusers ON posts.user_id = jusers.id) AS q",
            [() for _ in range(QUERIES)],
            1,
        ),
        "join_pk_limit": (
            None,
            f"SELECT posts.id, jusers.name FROM posts JOIN jusers ON posts.user_id = jusers.id LIMIT 20",
            [() for _ in range(QUERIES)],
            20,
        ),
        "join_idx": (
            None,
            "SELECT COUNT(*) FROM (SELECT jusers.name, posts.title FROM jusers JOIN posts ON posts.user_id = jusers.id) AS q",
            [() for _ in range(QUERIES)],
            1,
        ),
        "join_idx_limit": (
            None,
            f"SELECT jusers.name, posts.title FROM jusers JOIN posts ON posts.user_id = jusers.id LIMIT 20",
            [() for _ in range(QUERIES)],
            20,
        ),
    }

    conn = connect()
    # Server-side prepared statements on both engines: the Rust suites
    # prepare outside the timed loop, so re-parse cost must not land on
    # either side of these cells. psycopg3 auto-prepares after
    # `prepare_threshold` executions of the same statement; mysql.connector
    # gets an explicit prepared cursor.
    with conn.cursor(prepared=True) if TARGET == "mysql" else conn.cursor() as cur:
        started = time.perf_counter()
        setup_range(cur)
        print(f"range/aggregate setup: {time.perf_counter() - started:.1f}s", flush=True)
        started = time.perf_counter()
        setup_join(cur)
        print(f"join setup: {time.perf_counter() - started:.1f}s", flush=True)

        results: dict[str, dict[str, list[float]]] = {
            name: {"throughput": [], "p50": [], "p95": [], "p99": []} for name in shapes
        }
        schedules = [(name, rep) for rep in range(REPS) for name in shapes]
        # Fisher-Yates, same fixed seed as the Rust harness's sweeps.
        srng = SeededRng(0x5EED_B15C)
        for i in range(len(schedules) - 1, 0, -1):
            j = srng.next_u64() % (i + 1)
            schedules[i], schedules[j] = schedules[j], schedules[i]
        print(f"schedule: {schedules}", flush=True)

        for i, (name, rep) in enumerate(schedules):
            _, sql, params, expected = shapes[name]
            samples = []
            errors = 0
            for params_i in params:
                at = time.perf_counter()
                rows = run(cur, sql, params_i if params_i else None)
                samples.append(time.perf_counter() - at)
                if len(rows) != expected and errors == 0:
                    errors = len(rows)  # record once; shape miscount fails the rep
            if errors:
                raise SystemExit(
                    f"{name}: expected {expected} rows, got {errors} — refusing to time a wrong answer"
                )
            elapsed = sum(samples)
            ordered = sorted(samples)
            pick = lambda p: ordered[round((len(ordered) - 1) * p)]  # noqa: E731
            results[name]["throughput"].append(len(params) / elapsed)
            results[name]["p50"].append(pick(0.50) * 1000)
            results[name]["p95"].append(pick(0.95) * 1000)
            results[name]["p99"].append(pick(0.99) * 1000)
            print(
                f"[{i + 1}/{len(schedules)}] {name} rep {rep}: "
                f"{len(params) / elapsed:.0f}/s p50 {pick(0.50) * 1000:.3f}ms",
                flush=True,
            )

    def median(vals: list[float]) -> float:
        s = sorted(vals)
        n = len(s)
        return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2

    print(f"\n=== {TARGET}: medians (min–max) over {REPS} reps ===")
    summary = {}
    for name, vals in results.items():
        med = median(vals["throughput"])
        summary[name] = {
            "median_ops_s": med,
            "min": min(vals["throughput"]),
            "max": max(vals["throughput"]),
            "p50_ms": median(vals["p50"]),
            "p95_ms": median(vals["p95"]),
            "p99_ms": median(vals["p99"]),
        }
        print(
            f"{name:16} {med:>10.0f}/s  ({min(vals['throughput']):.0f}–{max(vals['throughput']):.0f})"
            f"  p50 {median(vals['p50']):.3f}ms  p95 {median(vals['p95']):.3f}ms"
        )

    if OUT:
        with open(OUT, "w") as handle:
            json.dump({"target": TARGET, "reps": REPS, "results": summary}, handle, indent=2)
        print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
