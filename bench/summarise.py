#!/usr/bin/env python3
"""Summarise several ``bench/run.sh`` outputs into one median-and-spread report.

A single benchmark run on a developer laptop is worth about a factor of two.
Two editions of ``BENCHMARK.md`` in a row have carried figures that moved for
reasons no commit could explain — point reads halving on a path the commit did
not touch, one SQLite configuration rising while the other fell in the same
window. The fix is not a quieter machine, which nobody can promise; it is
running the thing more than once and publishing how far the runs disagreed.

    ./bench/summarise.py run-a.txt run-b.txt run-c.txt

It makes no attempt to understand the benchmark's output. It finds numbers by
shape, aligns the files line by line, and refuses to guess if they do not line
up: runs with identical parameters and an identical seed produce identical
*structure*, so two files that disagree about their shape differed in something
other than timing, and averaging them would invent a measurement rather than
take one.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field

# A number the report might contain, with the unit the harness printed it in.
# Bare integers and ratios count: ops/s, recall, `2.66x` and `0.0%` all move
# between runs and all deserve a spread.
VALUE = re.compile(r"^[+-]?\d+(?:\.\d+)?(?P<unit>ns|us|µs|ms|s|x|%)?$")

# Seconds per unit, so a metric printed as `958.00ns` in one run and `1.58µs`
# in the next is still one metric measured twice. Unitless numbers are left
# alone — an ops/s figure is not a duration.
SECONDS = {"ns": 1e-9, "us": 1e-6, "µs": 1e-6, "ms": 1e-3, "s": 1.0}
FRACTION = re.compile(r"^(\d+)/(\d+)$")

# Provenance, which is meant to differ between runs: dropped, because
# repeat.sh writes its own header for the combined report.
DROP = re.compile(r"^\s*(?:date|commit|dirty|rustc|host|docker|load|written to)\b")

# Banners, parenthesised notes and derived comparison sentences. Kept in the
# output because they are what makes the report readable, but never measured.
# In particular, the comparison can legitimately cross parity between noisy
# runs ("faster" in one and "slower" in another); its ratio is derived from
# the engine rows above, so treating that wording as benchmark structure both
# double-counts the metric and makes an otherwise valid summary fail.
PROSE = re.compile(r"^\s*===|^\s*\(|^InlaySQL is ")

# A rule between a table's header and its rows, or the `|` that divides one
# group of columns from another. Neither carries a number, so both used to be
# mistaken for the table header — and since the rule sits *below* the real
# header, every row under a ruled table got its columns named `col1`, `col2`
# and so on. That is most of `bench/compare.sh`'s output, whose tables are
# ruled where `bench/run.sh`'s are not, so the naming was worst exactly where
# the reader most needs to know whether a swinging figure is a `max` (expected)
# or a `p50` (the measurement falling apart).
SEPARATOR = re.compile(r"^[-=|+_\s]+$")

# `bench/run.sh` samples machine load throughout a run, not just before it
# starts, and marks the result file with this word, prominently, when a
# sample exceeded the configured threshold mid-run. It lives on a `load:`
# line, so `DROP` above would otherwise strip it out along with the rest of
# that provenance — which would make a contaminated run indistinguishable
# from a clean one the moment it is combined with others. Checked directly
# against the raw file text rather than through `parse()`, so it survives
# regardless of what `DROP`/`PROSE` do to the line it sits on.
CONTAMINATED = re.compile(r"\bCONTAMINATED\b")


@dataclass
class Slot:
    """One number in one position of the report, measured once per run."""

    unit: str
    column: str
    values: list[float] = field(default_factory=list)

    def median(self) -> float:
        ordered = sorted(self.values)
        middle = len(ordered) // 2
        if len(ordered) % 2:
            return ordered[middle]
        return (ordered[middle - 1] + ordered[middle]) / 2.0

    def spread(self) -> float:
        """Widest disagreement as a fraction of the median.

        Not a standard deviation. With three or five runs the useful question
        is not how they cluster but how bad it got, and a reader deciding
        whether to trust a published figure wants the worst case.
        """
        median = self.median()
        if median == 0.0:
            return 0.0
        return (max(self.values) - min(self.values)) / abs(median)


@dataclass
class Line:
    """One line of the report: its text, its shape, and its values.

    `text` and `spans` are kept so the median can be written back into the
    original line rather than re-joined from tokens — these tables are
    whitespace-aligned, and a summary that loses the columns is harder to read
    than the thing it summarises.
    """

    text: str
    shape: str
    values: list[tuple[float, str]]
    spans: list[tuple[int, int]]
    header: str


def split(line: str) -> tuple[str, list[tuple[float, str]], list[tuple[int, int]]]:
    """Return `line`'s shape, its values, and where in the text they sit."""
    shape, values, spans = [], [], []
    for match in re.finditer(r"\S+", line):
        token = match.group()
        core = token.strip("(),")
        number = VALUE.match(core)
        if not number:
            # A counter pair such as `20003/20465` is two values, not text:
            # `compare.sh`'s commits-per-fsync line prints the raw counters
            # beside their ratio, and every run's counters differ, so
            # treating them as shape made three otherwise identical runs
            # "disagree" and refused the whole summary (2026-09-03).
            pair = FRACTION.match(core)
            if pair:
                shape.append(token.replace(core, "<v>/<v>", 1))
                for group, offset in ((1, 0), (2, len(pair.group(1)) + 1)):
                    values.append((float(pair.group(group)), ""))
                    start = match.start() + token.index(core) + offset
                    spans.append((start, start + len(pair.group(group))))
                continue
            shape.append(token)
            continue
        unit = number.group("unit") or ""
        shape.append(token.replace(core, "<v>", 1))
        values.append((float(core[: len(core) - len(unit)] or core) * SECONDS.get(unit, 1.0), unit))
        start = match.start() + token.index(core)
        spans.append((start, start + len(core)))
    return " ".join(shape), values, spans


def columns(header: str, spans: list[tuple[int, int]]) -> list[str]:
    """Name each value in a row from the table header sitting above it.

    Getting this right is most of the point: a row's `max` is one unlucky
    sample and is *expected* to swing, where the same swing in its `p50` is the
    measurement falling apart.

    Names are matched by *position*, not word order: every table these scripts
    print is right-aligned, so a value's right edge lines up with the right
    edge of the header word above it, and the header word whose edge is nearest
    is that value's name. Word order alone cannot do this. It breaks on a
    two-word column name (`write ops/s` above one value) by shifting every
    subsequent name one place left, and it breaks on `|`-divided column groups
    by counting the divider as a name — both of which are how `compare.sh`
    prints, and a *wrong* name is worse than no name, because the whole reason
    to print it is to tell the reader which swings matter.
    """
    words = [
        (match.group(), match.end())
        for match in re.finditer(r"\S+", header)
        if not SEPARATOR.match(match.group())
    ]
    if not words:
        return [f"col{index + 1}" for index in range(len(spans))]
    return [min(words, key=lambda word: abs(word[1] - end))[0] for _, end in spans]


def parse(path: str) -> list[Line | str]:
    """Read one result file, keeping every line so the layout survives."""
    out: list[Line | str] = []
    header = ""
    with open(path, encoding="utf-8") as handle:
        for raw in handle:
            line = raw.rstrip("\n")
            if DROP.search(line):
                continue
            if not line.strip() or PROSE.search(line):
                out.append(line)
                continue
            shape, values, spans = split(line)
            if not values:
                # No numbers: a table header, prose between tables, or the rule
                # under a header. A rule is not a header, and letting it become
                # one throws away the names of every column below it.
                if not SEPARATOR.match(line):
                    header = line
                out.append(line)
                continue
            out.append(Line(text=line, shape=shape, values=values, spans=spans, header=header))
    return out


def rewrite(line: Line, replacements: list[str]) -> str:
    """Put `replacements` back where `line`'s own values were.

    Right-aligned into the width the harness used, so the columns survive; a
    replacement wider than the original pushes the line out rather than
    truncating, because a wrong number is worse than a ragged one.
    """
    text = line.text
    for (start, end), value in reversed(list(zip(line.spans, replacements))):
        text = text[:start] + value.rjust(end - start) + text[end:]
    return text


def render(seconds: float, unit: str) -> str:
    """Print a value back in the unit the harness chose for it."""
    if unit in SECONDS:
        return f"{seconds / SECONDS[unit]:.2f}{unit}"
    if unit:
        return f"{seconds:.2f}{unit}"
    if seconds == int(seconds):
        return str(int(seconds))
    return f"{seconds:.2f}".rstrip("0").rstrip(".")


def measured(run: list[Line | str]) -> list[Line]:
    return [item for item in run if isinstance(item, Line)]


def contaminated(path: str) -> bool:
    """True if `path` flagged itself as taken under a mid-run load spike.

    This is a raw-text check, deliberately independent of `parse()`: the
    marker lives on a `load:` line, which `DROP` strips before a single
    `Line` is ever built, and a check built on top of `parse()`'s output
    would silently lose the one thing it is supposed to catch.
    """
    with open(path, encoding="utf-8") as handle:
        return any(CONTAMINATED.search(line) for line in handle)


def warn_contaminated(paths: list[str]) -> list[str]:
    flagged = [path for path in paths if contaminated(path)]
    if not flagged:
        return flagged
    print("!" * 72)
    print(f"CONTAMINATED: {len(flagged)} of {len(paths)} input run(s) recorded a load")
    print("spike mid-run (bench/run.sh's own load monitor, not this script). Every")
    print("figure below is built from at least one run taken while the machine was")
    print("busier than the gate allows. Treat this summary as unreliable until it is")
    print("re-run on a quiet machine throughout. See PERF.md §4.")
    for path in flagged:
        print(f"  {path}")
    print("!" * 72)
    print()
    return flagged


def main(paths: list[str]) -> int:
    if len(paths) < 2:
        print("usage: summarise.py <result.txt> <result.txt> [...]", file=sys.stderr)
        return 2

    flagged = warn_contaminated(paths)

    runs = [parse(path) for path in paths]
    reference = measured(runs[0])
    for path, run in zip(paths[1:], runs[1:]):
        other = measured(run)
        if len(other) != len(reference):
            print(
                f"{path} has {len(other)} measured lines and {paths[0]} has "
                f"{len(reference)}: these are not the same benchmark, and "
                f"averaging them would invent a measurement",
                file=sys.stderr,
            )
            return 1
        for index, (left, right) in enumerate(zip(reference, other)):
            if left.shape != right.shape:
                print(f"{paths[0]} and {path} disagree at measured line {index}:", file=sys.stderr)
                print(f"  {left.shape}", file=sys.stderr)
                print(f"  {right.shape}", file=sys.stderr)
                return 1

    slots: list[list[Slot]] = []
    for index, line in enumerate(reference):
        names = columns(line.header, line.spans)
        row = []
        for column, (_, unit) in enumerate(line.values):
            row.append(
                Slot(
                    unit=unit,
                    column=names[column],
                    values=[measured(run)[index].values[column][0] for run in runs],
                )
            )
        slots.append(row)

    print(f"runs:   {len(paths)}")
    for path in paths:
        print(f"        {path}")
    print()

    everything = [
        (slot.spread(), line.shape, slot)
        for line, row in zip(reference, slots)
        for slot in row
    ]
    loud = sorted((entry for entry in everything if entry[0] >= 0.10), reverse=True, key=lambda e: e[0])
    print(f"metrics: {len(everything)}; disagreeing by 10% or more across runs: {len(loud)}")
    if loud:
        print()
        print("Widest disagreement first. A figure listed here is not worth quoting to")
        print("three digits: the machine moved it further than that between runs. A `max`")
        print("column is one unlucky sample and is expected here; a `p50` or an ops/s")
        print("figure is the measurement itself, and swinging is what it is not supposed")
        print("to do.")
        print()
        print(f"{'spread':>8}  {'column':>10}  {'median':>12}  {'min':>12}  {'max':>12}  row")
        for spread, shape, slot in loud[:25]:
            label = shape.replace(" <v>", "").strip()
            print(
                f"{spread * 100:7.1f}%  {slot.column:>10}  "
                f"{render(slot.median(), slot.unit):>12}  "
                f"{render(min(slot.values), slot.unit):>12}  "
                f"{render(max(slot.values), slot.unit):>12}  {label}"
            )
    print()
    print("--- median of all runs, in the layout run.sh printed ---")

    index = 0
    for item in runs[0]:
        if isinstance(item, str):
            print(item)
            continue
        print(rewrite(item, [render(slot.median(), slot.unit) for slot in slots[index]]))
        index += 1

    if flagged:
        print()
        warn_contaminated(paths)

    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
