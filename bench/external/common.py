"""Shared plumbing for the containerised baselines.

Every engine reads the same corpus, the same queries and the same ground
truths, all produced once by `cargo run -p inlaysql-bench -- --export`. Nothing
in here generates data: if a driver needed to generate anything, the engines
would no longer be answering the same question.
"""

from __future__ import annotations

import csv
import json
import os
import sys
import time
from dataclasses import dataclass, field

# Reciprocal rank fusion's damping constant. The same value InlaySQL's `fuse()`
# uses (`inlaysql_core::fusion::DEFAULT_RRF_K`) — a different one here would
# make the hybrid comparison a comparison of constants.
RRF_K = 60.0

CORPUS = os.environ.get("CORPUS", "/corpus")


@dataclass
class Corpus:
    manifest: dict
    ids: list[int]
    bodies: list[str]
    embeddings: list[list[float]]
    # Query text and the matching embedding, in the exported order.
    query_texts: list[str] = field(default_factory=list)
    query_embeddings: list[list[float]] = field(default_factory=list)
    # qid -> ranked ids, for each ground truth.
    vector_truth: list[list[int]] = field(default_factory=list)
    hybrid_truth: list[list[int]] = field(default_factory=list)

    @property
    def dim(self) -> int:
        return int(self.manifest["dim"])

    @property
    def top_k(self) -> int:
        return int(self.manifest["top_k"])


def _vector(text: str) -> list[float]:
    return [float(part) for part in text.strip()[1:-1].split(",")]


def _ranking(path: str, queries: int) -> list[list[int]]:
    ranked: list[list[int]] = [[] for _ in range(queries)]
    with open(path, newline="") as handle:
        for row in csv.DictReader(handle):
            ranked[int(row["qid"])].append(int(row["id"]))
    return ranked


def load(directory: str = CORPUS) -> Corpus:
    with open(os.path.join(directory, "manifest.json")) as handle:
        manifest = json.load(handle)

    ids: list[int] = []
    bodies: list[str] = []
    embeddings: list[list[float]] = []
    with open(os.path.join(directory, "corpus.csv"), newline="") as handle:
        for row in csv.DictReader(handle):
            ids.append(int(row["id"]))
            bodies.append(row["body"])
            embeddings.append(_vector(row["embedding"]))

    query_texts: list[str] = []
    query_embeddings: list[list[float]] = []
    with open(os.path.join(directory, "queries.csv"), newline="") as handle:
        for row in csv.DictReader(handle):
            query_texts.append(row["text"])
            query_embeddings.append(_vector(row["embedding"]))

    queries = len(query_texts)
    return Corpus(
        manifest=manifest,
        ids=ids,
        bodies=bodies,
        embeddings=embeddings,
        query_texts=query_texts,
        query_embeddings=query_embeddings,
        vector_truth=_ranking(os.path.join(directory, "truth-vector.csv"), queries),
        hybrid_truth=_ranking(os.path.join(directory, "truth-hybrid.csv"), queries),
    )


@dataclass
class OltpWorkload:
    """The point-read/point-write workload, exactly as `oltp_export` wrote it.

    `rows` is the exact insert order (sequential ids) and `lookup_keys` is the
    exact lookup sequence `points::lookup_keys` generated for the in-process
    InlaySQL/SQLite comparison — the same operations in the same order, so a
    driver here answers the identical questions rather than a fresh random
    draw with the same distribution.
    """

    manifest: dict
    rows: list[tuple[int, str]]
    lookup_keys: list[int]


def load_oltp(directory: str = CORPUS) -> OltpWorkload:
    with open(os.path.join(directory, "oltp-manifest.json")) as handle:
        manifest = json.load(handle)

    rows: list[tuple[int, str]] = []
    with open(os.path.join(directory, "oltp-rows.csv"), newline="") as handle:
        for row in csv.DictReader(handle):
            rows.append((int(row["id"]), row["body"]))

    lookup_keys: list[int] = []
    with open(os.path.join(directory, "oltp-lookup-keys.csv"), newline="") as handle:
        for row in csv.DictReader(handle):
            lookup_keys.append(int(row["key"]))

    return OltpWorkload(manifest=manifest, rows=rows, lookup_keys=lookup_keys)


def overlap(got: list[int], truth: list[int]) -> float:
    """Fraction of `truth` present in `got`."""
    if not truth:
        return 1.0
    wanted = set(truth)
    return sum(1 for item in got if item in wanted) / len(truth)


def rrf(rankings: list[list[int]], k: int) -> list[int]:
    """Fuse ranked id lists exactly the way InlaySQL's `fuse()` does.

    Ranks only, never scores: the engines' raw scores are on incomparable
    scales, which is the whole reason reciprocal rank fusion exists. Ties break
    by ascending id so the output does not depend on dict ordering.
    """
    fused: dict[int, float] = {}
    for ranking in rankings:
        for rank, identifier in enumerate(ranking):
            fused[identifier] = fused.get(identifier, 0.0) + 1.0 / (RRF_K + rank + 1)
    ordered = sorted(fused.items(), key=lambda item: (-item[1], item[0]))
    return [identifier for identifier, _ in ordered[:k]]


class Timer:
    """Per-query wall-clock samples, summarised the way the Rust harness does."""

    def __init__(self) -> None:
        self.samples: list[float] = []

    def time(self, call):
        started = time.perf_counter()
        result = call()
        self.samples.append((time.perf_counter() - started) * 1000.0)
        return result

    def percentiles(self) -> tuple[float, float, float, float]:
        """p50, p95, p99, max — the same four points and the same
        nearest-rank formula `crates/inlaysql-bench/src/main.rs`'s own
        `percentiles()` uses, so a Python-driven row and a Rust-driven row
        land on directly comparable numbers rather than two engines'
        latencies being rounded two different ways. p99 was added
        (`SCOREBOARD.md` §3.10/§6) to close the MySQL/PostgreSQL tail-latency
        cells the Rust concurrency suite's own p99 already fills for SQLite.
        """
        if not self.samples:
            return (0.0, 0.0, 0.0, 0.0)
        ordered = sorted(self.samples)
        pick = lambda p: ordered[round((len(ordered) - 1) * p)]  # noqa: E731
        return (pick(0.50), pick(0.95), pick(0.99), ordered[-1])


def write_result(
    directory: str,
    slug: str,
    engine: str,
    build_seconds: float,
    vector_recall: float,
    vector_timer: Timer,
    hybrid_agreement: float,
    hybrid_timer: Timer,
    notes: str,
) -> None:
    """Write one engine's numbers in the shape `report.py` merges."""
    vp50, vp95, vp99, vmax = vector_timer.percentiles()
    hp50, hp95, hp99, hmax = hybrid_timer.percentiles()
    result = {
        "engine": engine,
        "build_seconds": round(build_seconds, 3),
        "vector": {
            "recall": round(vector_recall, 4),
            "p50_ms": round(vp50, 3),
            "p95_ms": round(vp95, 3),
            "p99_ms": round(vp99, 3),
            "max_ms": round(vmax, 3),
        },
        "hybrid": {
            "agreement": round(hybrid_agreement, 4),
            "p50_ms": round(hp50, 3),
            "p95_ms": round(hp95, 3),
            "p99_ms": round(hp99, 3),
            "max_ms": round(hmax, 3),
        },
        "notes": notes,
    }
    path = os.path.join(directory, f"results-{slug}.json")
    with open(path, "w") as handle:
        json.dump(result, handle, indent=2)
        handle.write("\n")
    print(f"wrote {path}", file=sys.stderr)


def write_oltp_result(
    directory: str,
    slug: str,
    engine: str,
    write_ops_s: float,
    write_timer: Timer,
    read_ops_s: float,
    read_timer: Timer,
    notes: str,
    rows: int,
    lookups: int,
    commit_stats: dict | None = None,
) -> None:
    """Write one engine's OLTP numbers in the shape `report.py` merges.

    Sibling to `write_result` (vector/hybrid, for the retrieval comparison)
    rather than a reuse of it: this workload measures reads and writes, not
    recall and agreement, so the fields differ, but the file lands in the
    same `results-*.json` family — under the `results-oltp-` prefix so
    `report.py` can tell the two workloads' files apart — and is merged by
    the same script.

    `write_ops_s`/`read_ops_s` are wall-clock throughput over the whole loop
    (operations divided by total elapsed time), computed by the caller rather
    than derived from the per-operation samples in `write_timer`/
    `read_timer`: summing per-operation samples would drop whatever time the
    loop itself spends between operations, and the Rust harness this is
    matched against times the whole loop the same way.

    `commit_stats`, when given, is `{"commits": int, "fsyncs": int,
    "commits_per_fsync": float}` — the delta of a server's own commit and
    `fsync` counters (MySQL's `Com_commit`/`Innodb_os_log_fsyncs`,
    PostgreSQL's `pg_stat_database.xact_commit`/`pg_stat_wal.wal_sync`)
    sampled immediately before and after the write phase timed above. See
    `SCOREBOARD.md` §6: this is the mechanism metric, not just throughput —
    it is the number that says whether group commit is amortising `fsync`s
    across concurrent writers or paying one per commit regardless.
    """
    wp50, wp95, wp99, wmax = write_timer.percentiles()
    rp50, rp95, rp99, rmax = read_timer.percentiles()
    result = {
        "engine": engine,
        "rows": rows,
        "lookups": lookups,
        "write": {
            "ops_s": round(write_ops_s, 1),
            "p50_ms": round(wp50, 3),
            "p95_ms": round(wp95, 3),
            "p99_ms": round(wp99, 3),
            "max_ms": round(wmax, 3),
        },
        "read": {
            "ops_s": round(read_ops_s, 1),
            "p50_ms": round(rp50, 3),
            "p95_ms": round(rp95, 3),
            "p99_ms": round(rp99, 3),
            "max_ms": round(rmax, 3),
        },
        "notes": notes,
    }
    if commit_stats is not None:
        result["commit_stats"] = commit_stats
    path = os.path.join(directory, f"results-oltp-{slug}.json")
    with open(path, "w") as handle:
        json.dump(result, handle, indent=2)
        handle.write("\n")
    print(f"wrote {path}", file=sys.stderr)


def write_server_oltp_result(
    directory: str,
    slug: str,
    engine: str,
    levels: list[dict],
    notes: str,
    rows: int,
    lookups: int,
) -> None:
    """Write one engine's server-to-server OLTP numbers (`server_driver.py`).

    Same family as [`write_oltp_result`] above, but one write/read pair *per
    concurrency level* instead of one — the whole point of this workload is
    how each engine's numbers move as connections are added, which a single
    pair cannot show. Lands under `results-server-oltp-<slug>.json`, a
    distinct prefix from the single-connection `results-oltp-<slug>.json`
    family, so `report.py` never merges the two into one table: they measure
    different things (one connection vs several) and are not meant to be
    read against each other. See bench/README.md.

    `levels` is a list of dicts with `concurrency`, `write_ops_s`,
    `write_timer`, `write_retries`, `read_ops_s`, `read_timer`, and
    optionally `commit_stats` (see `write_oltp_result`'s docstring for its
    shape — the delta commit/`fsync` counters bracketing this level's write
    phase, where the target exposes them) — the shape
    `server_driver.py::measure_concurrency` returns.
    """
    encoded_levels = []
    for level in levels:
        wp50, wp95, wp99, wmax = level["write_timer"].percentiles()
        rp50, rp95, rp99, rmax = level["read_timer"].percentiles()
        encoded = {
            "concurrency": level["concurrency"],
            "write": {
                "ops_s": round(level["write_ops_s"], 1),
                "p50_ms": round(wp50, 3),
                "p95_ms": round(wp95, 3),
                "p99_ms": round(wp99, 3),
                "max_ms": round(wmax, 3),
            },
            "write_retries": level["write_retries"],
            "read": {
                "ops_s": round(level["read_ops_s"], 1),
                "p50_ms": round(rp50, 3),
                "p95_ms": round(rp95, 3),
                "p99_ms": round(rp99, 3),
                "max_ms": round(rmax, 3),
            },
        }
        if level.get("commit_stats") is not None:
            encoded["commit_stats"] = level["commit_stats"]
        encoded_levels.append(encoded)
    result = {
        "engine": engine,
        "rows": rows,
        "lookups": lookups,
        "levels": encoded_levels,
        "notes": notes,
    }
    path = os.path.join(directory, f"results-server-oltp-{slug}.json")
    with open(path, "w") as handle:
        json.dump(result, handle, indent=2)
        handle.write("\n")
    print(f"wrote {path}", file=sys.stderr)
