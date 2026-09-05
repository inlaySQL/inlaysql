#!/usr/bin/env python3
"""Summarise a paired, interleaved concurrency-suite log into medians and ratios.

`bench/flush_duty_cycle.sh`, `bench/gate_hold.sh`, `bench/record_size.sh` and
`bench/aa_floor.sh` all emit the same thing: the concurrency suite's output,
repeated, tagged with `=== rep N arm X ===`. Reading those by eye is how
AHL-562, AHL-563 and AHL-564 were tabulated, and it is why each of them
quoted a slightly different set of metrics. This turns one of those logs into
the table PERF.md wants — per writer count, each arm's median, and the *paired*
ratio, one per repetition, so the spread is visible next to the median rather
than summarised away.

    python3 bench/summarise_barrier.py --numerator on --denominator off log.txt

With `bench/aa_floor.sh`'s log the two arms differ in nothing, so what it
prints is the harness's noise floor: the band inside which a real experiment's
ratio means nothing.
"""

from __future__ import annotations

import argparse
import re
import statistics
import sys
from pathlib import Path

# One entry per metric: the column header, and whether a ratio below 1.0 is
# the improvement (times, holds) or above it (rates, shares of work done).
METRICS = (
    ("ops/s", "ops"),
    ("commits/barrier", "commits_per_barrier"),
    ("fsync ms", "fsync_ms"),
    ("interval ms", "interval_ms"),
    ("duty %", "duty"),
    ("gather ms", "gather_ms"),
    ("overlap ms", "overlap_ms"),
    ("handoff %", "handoff_pct"),
    ("hold ms", "hold_ms"),
    ("gate_wait %", "gate_wait_pct"),
    ("misses", "misses"),
)

ARM = re.compile(r"^=== rep (\d+) arm (\S+) ===")
# The pipeline clause is optional: AHL-562 added it, AHL-566 removed it again
# with the pipeline itself, and logs from both eras have to keep parsing — a
# retraction that made the retracted experiment's own log unreadable would
# make its numbers unauditable.
CYCLE = re.compile(
    r"barrier cycle: (\d+) writers, [\d.]+ barriers/s — fsync ([\d.]+) ms, "
    r"interval ([\d.]+) ms, idle ([\d.]+) ms \(([\d.]+)% .*?"
    r"coordinator gather ([\d.]+) post [\d.]+ gap ([\d.]+) ms/barrier"
    r"(?:; pipeline (\d+) handoffs \((\d+)% of barriers\), "
    r"overlapped gather ([\d.]+) ms/barrier)?"
)
BARRIERS = re.compile(
    r"barriers: (\d+) writers, .*?\(([\d.]+) syncs/commit, ([\d.]+) commits/sync\)"
)
BUCKETS = re.compile(r"buckets: (\d+) writers, .*?— gate_wait ([-\d.]+)%")
HOLD = re.compile(
    r"gate hold: (\d+) writers, \d+ holds, ([\d.]+) ms mean —.*?"
    r"(\d+) commit-point misses"
)
OPS = re.compile(r"^InlaySQL \(parallel WAL regions\)\s+(\d+)\s+(\d+)\s")


def parse(text: str) -> dict[tuple[int, str, int], dict[str, float]]:
    """Map (rep, arm, writers) to that run's metrics."""
    runs: dict[tuple[int, str, int], dict[str, float]] = {}
    rep, arm = 0, "?"

    def slot(writers: int) -> dict[str, float]:
        return runs.setdefault((rep, arm, writers), {})

    for line in text.splitlines():
        header = ARM.match(line.strip())
        if header:
            rep, arm = int(header.group(1)), header.group(2)
            continue
        if rep == 0:
            continue
        cycle = CYCLE.search(line)
        if cycle:
            row = slot(int(cycle.group(1)))
            row["fsync_ms"] = float(cycle.group(2))
            row["interval_ms"] = float(cycle.group(3))
            row["duty"] = 100.0 - float(cycle.group(5))
            row["gather_ms"] = float(cycle.group(6))
            if cycle.group(9) is not None:
                row["handoff_pct"] = float(cycle.group(9))
                row["overlap_ms"] = float(cycle.group(10))
            continue
        barriers = BARRIERS.search(line)
        if barriers:
            slot(int(barriers.group(1)))["commits_per_barrier"] = float(
                barriers.group(3)
            )
            continue
        buckets = BUCKETS.search(line)
        if buckets:
            slot(int(buckets.group(1)))["gate_wait_pct"] = float(buckets.group(2))
            continue
        hold = HOLD.search(line)
        if hold:
            row = slot(int(hold.group(1)))
            row["hold_ms"] = float(hold.group(2))
            row["misses"] = float(hold.group(3))
            continue
        ops = OPS.match(line.strip())
        if ops:
            slot(int(ops.group(1)))["ops"] = float(ops.group(2))
    return runs


def ratios(
    runs: dict[tuple[int, str, int], dict[str, float]],
    numerator: str,
    denominator: str,
    key: str,
    writers: int,
) -> list[float]:
    """The paired numerator/denominator ratio, one per repetition that has both.

    Pairing is the point: a repetition where either arm is missing the metric
    contributes nothing, rather than being averaged against an arm from some
    other moment of the run.
    """
    out = []
    reps = sorted({rep for (rep, _, _) in runs})
    for rep in reps:
        top = runs.get((rep, numerator, writers), {}).get(key)
        bottom = runs.get((rep, denominator, writers), {}).get(key)
        if top is None or bottom is None or bottom == 0:
            continue
        out.append(top / bottom)
    return out


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--numerator", required=True)
    parser.add_argument("--denominator", required=True)
    args = parser.parse_args(argv)

    runs = parse(args.log.read_text())
    if not runs:
        print("no `=== rep N arm X ===` sections found", file=sys.stderr)
        return 1
    levels = sorted({writers for (_, _, writers) in runs})

    print(f"| writers | arm | {' | '.join(name for name, _ in METRICS)} |")
    print("| --- " * (len(METRICS) + 2) + "|")
    for writers in levels:
        for arm in (args.denominator, args.numerator):
            cells = []
            for _, key in METRICS:
                values = [
                    row[key]
                    for (_, a, w), row in runs.items()
                    if a == arm and w == writers and key in row
                ]
                cells.append(f"{statistics.median(values):.3f}" if values else "—")
            print(f"| {writers} | {arm} | {' | '.join(cells)} |")

    print()
    print(f"Paired {args.numerator}/{args.denominator} ratios, one per repetition:")
    print()
    print("| writers | metric | median | min | max | per rep |")
    print("| --- | --- | --- | --- | --- | --- |")
    for writers in levels:
        for name, key in METRICS:
            series = ratios(runs, args.numerator, args.denominator, key, writers)
            if not series:
                continue
            spread = " ".join(f"{value:.2f}" for value in series)
            print(
                f"| {writers} | {name} | {statistics.median(series):.3f} "
                f"| {min(series):.2f} | {max(series):.2f} | {spread} |"
            )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
