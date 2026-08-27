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

# Provenance, which is meant to differ between runs: dropped, because
# repeat.sh writes its own header for the combined report.
DROP = re.compile(r"^\s*(?:date|commit|dirty|rustc|host|docker|load|written to)\b")

# Banners and parenthesised notes. Kept in the output because they are what
# makes the report readable, but never measured: the digits in them are the
# parameters the run was given, not results it produced.
PROSE = re.compile(r"^\s*===|^\s*\(")


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
            shape.append(token)
            continue
        unit = number.group("unit") or ""
        shape.append(token.replace(core, "<v>", 1))
        values.append((float(core[: len(core) - len(unit)] or core) * SECONDS.get(unit, 1.0), unit))
        start = match.start() + token.index(core)
        spans.append((start, start + len(core)))
    return " ".join(shape), values, spans


def columns(header: str, count: int) -> list[str]:
    """Name each value in a row from the table header sitting above it.

    A table header is the line before the rows with no numbers in it, and its
    trailing words are the column names — `engine ops/s p50 p95 max` over four
    values means the fourth value is a `max`. Getting this right is most of the
    point: a row's `max` is one unlucky sample and is *expected* to swing,
    where the same swing in its `p50` is the measurement falling apart.
    """
    words = header.split()
    return words[-count:] if len(words) >= count else [f"col{index + 1}" for index in range(count)]


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
                # No numbers: this is a table header, or prose between tables.
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


def main(paths: list[str]) -> int:
    if len(paths) < 2:
        print("usage: summarise.py <result.txt> <result.txt> [...]", file=sys.stderr)
        return 2

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
        names = columns(line.header, len(line.values))
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
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
