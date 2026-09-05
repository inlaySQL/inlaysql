"""Merge every engine's result file into one report.

Deliberately dumb: it prints what the drivers wrote and nothing else. No
engine is dropped for scoring badly, and each row carries the caveat its
driver attached, because a table of numbers without the caveats is how a
benchmark starts lying.

Three workloads share this merge step, and get separate tables because they
measure different things and are not meant to be read against each other:

* **retrieval** — recall and latency, `results-<engine>.json`.
* **OLTP** — throughput and latency for point reads/writes, one connection,
  `results-oltp-<engine>.json`.
* **server-to-server OLTP** — the same point reads/writes, over InlaySQL's
  own MySQL wire against MySQL's, at a couple of concurrency levels,
  `results-server-oltp-<engine>.json`.
"""

from __future__ import annotations

import glob
import json
import os
import sys


def duration(milliseconds: float) -> str:
    """A duration at a scale that can actually show it.

    Everything here used to print as `%.2f` milliseconds with an `m` suffix,
    which silently flattened the fastest rows to `0.00m`: a 500 ns point read —
    the whole result of the page cache work — rendered as zero, and so did a
    5 us one. A benchmark that prints the engine under test as zero is worse
    than one that prints nothing.
    """
    if milliseconds >= 1.0:
        return f"{milliseconds:.2f}ms"
    if milliseconds >= 0.001:
        return f"{milliseconds * 1_000:.2f}us"
    return f"{milliseconds * 1_000_000:.0f}ns"


def print_retrieval(manifest: dict, results: list[dict]) -> None:
    # Ours first, then the baselines alphabetically: the engine under test is
    # the one a reader is comparing everything else against.
    results = sorted(results, key=lambda r: (not r["engine"].startswith("InlaySQL"), r["engine"]))

    print(
        f"\n=== retrieval: {manifest['corpus']} docs, dim {manifest['dim']}, "
        f"{manifest['queries']} queries, top-{manifest['top_k']}, seed {manifest['seed']} ==="
    )
    header = f"{'engine':<34} {'recall@k':>9} {'p50':>9} {'p95':>9} | {'agree':>7} {'p50':>9} {'p95':>9} {'build':>8}"
    print(f"\n{'':34} {'--- vector search ---':^29} | {'--- hybrid (vector + text) ---':^36}")
    print(header)
    print("-" * len(header))
    for result in results:
        vector, hybrid = result["vector"], result["hybrid"]
        print(
            f"{result['engine']:<34} {vector['recall']:>9.3f} "
            f"{duration(vector['p50_ms']):>9} {duration(vector['p95_ms']):>9} | "
            f"{hybrid['agreement']:>7.3f} {duration(hybrid['p50_ms']):>9} "
            f"{duration(hybrid['p95_ms']):>9} {result['build_seconds']:>7.1f}s"
        )

    print("\nrecall@k is measured against exhaustive cosine similarity — an objective answer.")
    print("`agree` is overlap with InlaySQL's reference fusion (exact vector + exact BM25).")
    print("It is an agreement measure, not a quality score: an engine that ranks text with a")
    print("different function scores lower without being worse. Read the latencies as the result.")
    print("\nThe hybrid columns are not measuring the same amount of work. InlaySQL fuses inside")
    print("one SQL statement; the baselines have no fusion operator, so their driver runs two")
    print("queries and combines the ranks in Python. That is what using them for hybrid search")
    print("costs today, which is the comparison worth making — but it is not one query against")
    print("one query.\n")
    for result in results:
        print(f"  {result['engine']}: {result['notes']}")
    print()


def print_oltp(directory: str, results: list[dict]) -> None:
    results = sorted(results, key=lambda r: (not r["engine"].startswith("InlaySQL"), r["engine"]))

    with open(os.path.join(directory, "oltp-manifest.json")) as handle:
        oltp_manifest = json.load(handle)

    print(
        f"\n=== OLTP: {oltp_manifest['rows']} rows, {oltp_manifest['lookups']} lookups by "
        f"primary key, seed {oltp_manifest['seed']} ==="
    )
    width = max(64, max(len(result["engine"]) for result in results) + 1)
    header = (
        f"{'engine':<{width}} {'write ops/s':>11} {'p50':>8} {'p95':>8} {'p99':>8} | "
        f"{'read ops/s':>11} {'p50':>8} {'p95':>8} {'p99':>8}"
    )
    print(
        f"\n{'':{width}} {'--- write (durable, one row/commit) ---':^38} | "
        f"{'--- read (point lookup) ---':^38}"
    )
    print(header)
    print("-" * len(header))
    for result in results:
        write, read = result["write"], result["read"]
        print(
            f"{result['engine']:<{width}} {write['ops_s']:>11.1f} "
            f"{duration(write['p50_ms']):>9} {duration(write['p95_ms']):>9} "
            f"{duration(write.get('p99_ms', write['p95_ms'])):>9} | "
            f"{read['ops_s']:>11.1f} {duration(read['p50_ms']):>9} "
            f"{duration(read['p95_ms']):>9} "
            f"{duration(read.get('p99_ms', read['p95_ms'])):>9}"
        )
        commit_stats = result.get("commit_stats")
        if commit_stats is not None:
            print(
                f"{'':{width}}   commits-per-fsync: {commit_stats['commits']}"
                f"/{commit_stats['fsyncs']} = {commit_stats['commits_per_fsync']:.2f}"
            )

    print("\nEvery row here is configured for real durability — fsync on every commit — matched")
    print("as closely as each engine allows. See bench/README.md for the exact settings and the")
    print("cases that could not be made genuinely comparable.")
    print("\nMySQL and PostgreSQL are servers reached over the compose network: every number here")
    print("includes a client/server round trip that InlaySQL, a library in the caller's own")
    print("process, does not pay. That asymmetry biases every server row toward looking slower")
    print("than it would be over a faster transport than a Docker bridge.")
    print("\nInlaySQL is measured twice. The first row runs on the host and fsyncs to the real")
    print("disk — F_FULLFSYNC on macOS, a genuine barrier — exactly like the points suite. The")
    print("second, containerised row runs inside this same compose network, off the same Linux")
    print("build docker/test.sh produces, with its database file on a named Docker volume of the")
    print("same class postgres-oltp-data and mysql-oltp-data are, so its fsync crosses whatever")
    print("boundary theirs does. That is what makes the *containerised* row comparable to the")
    print("MySQL/PostgreSQL rows, and the gap between the two InlaySQL rows is a direct")
    print("measurement, on this machine, of what that virtualised fsync costs.")
    print("\nWhat this does and does not prove: comparable is not the same as hardware-durable —")
    print("on Docker Desktop for macOS/Windows none of the three server rows' commits are proven")
    print("durable to the platter, only to whatever the virtualised disk promises, and every")
    print("engine here now pays that same promise rather than two of the three paying it and one")
    print("not. What does not disappear: InlaySQL stays in-process even in its own container, so")
    print("it still does not pay the socket round trip MySQL and PostgreSQL do — read the")
    print("containerised row as the fsync asymmetry removed, not the transport asymmetry, which")
    print("is structural. See bench/README.md for the full accounting.\n")
    for result in results:
        print(f"  {result['engine']}: {result['notes']}")
    print()


def print_server_oltp(directory: str, results: list[dict]) -> None:
    """The server-to-server table: InlaySQL's own MySQL wire against
    MySQL's, both reached with `mysql.connector`, at a couple of connection
    counts. Sibling to `print_oltp` above, not a merge into it: that table
    measures one connection; this one measures how each engine's numbers
    move as connections are added, which needs its own `concurrency` column
    and its own caveats.
    """
    results = sorted(results, key=lambda r: (not r["engine"].startswith("InlaySQL"), r["engine"]))

    with open(os.path.join(directory, "oltp-manifest.json")) as handle:
        oltp_manifest = json.load(handle)

    print(
        f"\n=== server-to-server: {oltp_manifest['rows']} rows, {oltp_manifest['lookups']} "
        f"lookups by primary key, seed {oltp_manifest['seed']} — mysql.connector on both sides ==="
    )
    width = max(56, max(len(result["engine"]) for result in results) + 1)
    header = (
        f"{'engine':<{width}} {'conn':>4} {'write ops/s':>11} {'p50':>8} {'p95':>8} {'p99':>8} "
        f"{'retries':>7} | {'read ops/s':>11} {'p50':>8} {'p95':>8} {'p99':>8}"
    )
    print(
        f"\n{'':{width}} {'':4} {'--- write (durable, one row/commit) ---':^38} {'':>7} | "
        f"{'--- read (point lookup) ---':^38}"
    )
    print(header)
    print("-" * len(header))
    for result in results:
        for level in result["levels"]:
            write, read = level["write"], level["read"]
            print(
                f"{result['engine']:<{width}} {level['concurrency']:>4} {write['ops_s']:>11.1f} "
                f"{duration(write['p50_ms']):>9} {duration(write['p95_ms']):>9} "
                f"{duration(write.get('p99_ms', write['p95_ms'])):>9} "
                f"{level['write_retries']:>7} | {read['ops_s']:>11.1f} "
                f"{duration(read['p50_ms']):>9} {duration(read['p95_ms']):>9} "
                f"{duration(read.get('p99_ms', read['p95_ms'])):>9}"
            )
            commit_stats = level.get("commit_stats")
            if commit_stats is not None:
                print(
                    f"{'':{width}} {'':4}   commits-per-fsync: {commit_stats['commits']}"
                    f"/{commit_stats['fsyncs']} = {commit_stats['commits_per_fsync']:.2f}"
                )
                if "commits_per_fsync_all" in commit_stats:
                    print(
                        f"{'':{width}} {'':4}   commits-per-fsync (checkpoint-inclusive): "
                        f"{commit_stats['commits_all']}/{commit_stats['fsyncs_all']} = "
                        f"{commit_stats['commits_per_fsync_all']:.2f}"
                    )

    print("\nThis is the row bench/README.md calls the missing apples-to-apples number: InlaySQL")
    print("here is never a library call, it is `inlaysql serve --mysql`, reached over the compose")
    print("network by the same mysql.connector client code path that reaches MySQL above — every")
    print("number in this table, on every row, pays an identical socket round trip.")
    print("\nWhat still is not comparable, even here. inlaysql-server is thread-per-connection, one")
    print("OS thread and one Database handle per connection with no thread pool; MySQL schedules")
    print("connections onto a bounded worker pool — a structural difference in what adding a")
    print("connection costs each engine, not a tuning gap, so read a widening gap at the higher")
    print("concurrency level that way rather than as a regression. Both sides share one user and")
    print("one password as configured here, and on both sides that is this benchmark's own setup:")
    print("InlaySQL has accounts, GRANT/REVOKE and per-table privileges in the database file, and")
    print("neither engine's grant system is exercised here. Neither side negotiates TLS either,")
    print("though both could — InlaySQL's server runs with --plaintext-network on the compose")
    print("bridge, because a TLS handshake measured against MySQL's plaintext socket would be")
    print("measuring the wrong thing. PostgreSQL has no row here on purpose — InlaySQL has no")
    print("PostgreSQL-wire server to put on the other end of one. See bench/README.md.")
    print("\n`retries` counts a write this engine rolled back and retried on its own")
    print("first-committer-wins conflict response (MySQL error 1213) rather than one that failed;")
    print("disjoint id ranges per connection should keep this at zero, and a nonzero count is")
    print("reported rather than folded into the ops/s figure.\n")
    for result in results:
        print(f"  {result['engine']}: {result['notes']}")
    print()


def main() -> int:
    directory = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("CORPUS", "/corpus")

    retrieval_results = []
    oltp_results = []
    server_oltp_results = []
    for path in sorted(glob.glob(os.path.join(directory, "results-*.json"))):
        with open(path) as handle:
            result = json.load(handle)
        name = os.path.basename(path)
        if name.startswith("results-server-oltp-"):
            server_oltp_results.append(result)
        elif name.startswith("results-oltp-"):
            oltp_results.append(result)
        else:
            retrieval_results.append(result)

    if not retrieval_results and not oltp_results and not server_oltp_results:
        print("no results to report", file=sys.stderr)
        return 1

    if retrieval_results:
        with open(os.path.join(directory, "manifest.json")) as handle:
            manifest = json.load(handle)
        print_retrieval(manifest, retrieval_results)
    else:
        print("no retrieval results found", file=sys.stderr)

    if oltp_results:
        print_oltp(directory, oltp_results)
    else:
        print("no OLTP results found", file=sys.stderr)

    if server_oltp_results:
        print_server_oltp(directory, server_oltp_results)
    else:
        print("no server-to-server OLTP results found", file=sys.stderr)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
