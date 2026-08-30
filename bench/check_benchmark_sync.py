#!/usr/bin/env python3
"""Check that headline benchmark figures in README.md and the wasm demo page
have not drifted from BENCHMARK.md.

Why this exists: `README.md` was found carrying benchmark figures two
editions stale (GIL-contaminated OLTP numbers, a superseded server-to-server
table) because they are hand-copied from `BENCHMARK.md` and nothing checks
that the copy stays current. This script is that check. It is meant to run
in CI, against committed files only — it never runs a benchmark itself, so
it cannot flake the way a live measurement can.

    ./bench/check_benchmark_sync.py

Scope, deliberately narrow: this compares *numbers*, not prose. It does not
try to verify that a "3.3x" in one document and a "roughly 2-4x" in another
describe the same measurement the same way — `BENCHMARK.md`'s own precision
rules (see its opening note, and `PERF.md` §4) mean two documents are allowed
to hedge the very same ratio in different words or to a different number of
digits, and forcing them to match word-for-word would fight that honesty
rule rather than support it. What must not drift is the *measured figure*
underneath the hedge: an ops/s count, a latency, a commits/s number. Every
such figure quoted in README.md's "## Performance" section, or in the wasm
demo page's benchmark blocks, must appear *somewhere* in one of
BENCHMARK.md's own tables — formatting differences (thousands separators,
"k" abbreviation) are normalised away, but the number itself has to be found.

Known blind spot, stated rather than hidden: this is a presence check, not a
row-by-row diff, and only markdown *table* rows are read as authoritative —
prose is excluded on both sides. That is deliberate: BENCHMARK.md's own
point-read section keeps a running sentence of superseded figures ("published
at 636,980, then 342,747, ... and now 522,562 ops/s"), and a presence check
that read prose as ground truth would treat every number that sentence has
ever mentioned as still current, forever. Restricting the reference set to
table rows means a figure only counts as "current" if some BENCHMARK.md table
still carries it today.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# A number, optionally comma-grouped and/or decimal, optionally paired with
# a second number across a hyphen ("2-4", "55-70" — a hedged range), followed
# (with at most one space) by an optional suffix. The suffix decides how the
# token is handled: "k"/"K" scales the value, "%"/"x"/"X"/"×" (the
# multiplication sign) mark a ratio or a percentage — and a ratio range like
# "~55-70×" must be excluded *as a whole*, both numbers, not just the one
# touching the "×" — see the module docstring for why ratios are out of
# scope.
TOKEN_RE = re.compile(
    r"(\d[\d,]*(?:\.\d+)?)(?:[-–](\d[\d,]*(?:\.\d+)?))?( ?)(k|K|%|x|X|×)?"
)

RATIO_SUFFIXES = {"%", "x", "X", "×"}


def _parse(num: str, suffix: str | None) -> float:
    value = float(num.replace(",", ""))
    if suffix in ("k", "K"):
        value *= 1000
    return value


def _distinctive(num: str) -> bool:
    """True if `num` is specific enough to be worth checking.

    Bare single-digit integers (writer counts, connection counts, a `1.000`
    recall's leading `1`) are excluded — they are common enough elsewhere in
    a benchmark table that a match would not mean much, and their absence
    would not mean drift. A comma, a decimal point, or two-plus bare digits
    is the bar; every ops/s, latency and commits/s figure this script cares
    about clears it easily.
    """
    if "," in num or "." in num:
        return True
    return len(num) >= 2


def extract(text: str) -> dict[float, set[str]]:
    """Map each distinctive numeric value found in `text` to the raw
    substrings that produced it, so a mismatch can be reported with the
    text a human actually typed, not just a float."""
    found: dict[float, set[str]] = {}
    for match in TOKEN_RE.finditer(text):
        num, second_num, _, suffix = match.groups()
        if suffix in RATIO_SUFFIXES:
            # A ratio, or a hedged range of them ("~55-70x"): both numbers
            # are excluded together, not just the one touching the suffix.
            continue
        for candidate in (num, second_num):
            if candidate is None or not _distinctive(candidate):
                continue
            value = round(_parse(candidate, suffix), 6)
            found.setdefault(value, set()).add(candidate)
    return found


def table_lines(text: str) -> str:
    """Every markdown table row in `text` — a line whose first non-blank
    character is `|` — joined back together. Prose, headings and code
    fences are excluded on purpose; see the module docstring."""
    return "\n".join(line for line in text.splitlines() if line.strip().startswith("|"))


def slice_between(text: str, start_marker: str, end_pattern: re.Pattern[str]) -> str:
    start = text.index(start_marker)
    match = end_pattern.search(text, start + len(start_marker))
    end = match.start() if match else len(text)
    return text[start:end]


def slice_block(text: str, start_marker: str, end_marker: str) -> str:
    start = text.index(start_marker)
    end = text.index(end_marker, start + len(start_marker)) + len(end_marker)
    return text[start:end]


def benchmark_reference(path: Path) -> dict[float, set[str]]:
    return extract(table_lines(path.read_text(encoding="utf-8")))


def readme_figures(path: Path) -> dict[float, set[str]]:
    text = path.read_text(encoding="utf-8")
    performance = slice_between(text, "## Performance", re.compile(r"^## ", re.MULTILINE))
    return extract(table_lines(performance))


TD_VALUE_RE = re.compile(r'<td class="(?:num|win|loss)">(.*?)</td>', re.DOTALL)
STAT_RE = re.compile(r"<(?:b|em)>(.*?)</(?:b|em)>", re.DOTALL)


def wasm_demo_figures(path: Path) -> dict[float, set[str]]:
    text = path.read_text(encoding="utf-8")
    # The "figures" strip has one non-benchmark stat mixed in (a test-suite
    # pass count, in its own `<div class="figure">`) — stop at that div
    # rather than teach `extract` to tell a benchmark figure from a test
    # count. Cutting at the *marker text* inside that div would not be
    # early enough, because the div's own `<b>` value comes first in the
    # source order.
    figures_start = text.index('<div class="figures">')
    test_count_div = text.index("SQL Logic Tests passing", figures_start)
    figure_div_start = text.rindex('<div class="figure">', figures_start, test_count_div)
    figures = text[figures_start:figure_div_start]
    bench_table = slice_block(text, '<table class="bench">', "</table>")
    # Within the figures strip, only `<b>`/`<em>` hold values — `<span>`
    # holds the row's plain-English label. Within the table, only cells
    # classed num/win/loss hold values — the unclassed `Workload` and
    # `Compared with` cells are row labels and may legitimately describe a
    # corpus size ("20k x 160k rows") that is not itself a measured result.
    stats = " ".join(m.group(1) for m in STAT_RE.finditer(figures))
    cells = " ".join(m.group(1) for m in TD_VALUE_RE.finditer(bench_table))
    return extract(stats + " " + cells)


def check(label: str, figures: dict[float, set[str]], reference: dict[float, set[str]]) -> list[str]:
    problems = []
    for value, raw_forms in sorted(figures.items()):
        if value not in reference:
            shown = ", ".join(sorted(raw_forms))
            problems.append(f"  {label}: {shown} (parsed as {value!r}) not found in any BENCHMARK.md table")
    return problems


def main() -> int:
    benchmark_path = ROOT / "BENCHMARK.md"
    readme_path = ROOT / "README.md"
    wasm_path = ROOT / "crates" / "inlaysql-wasm" / "www" / "index.html"

    try:
        reference = benchmark_reference(benchmark_path)
        problems = []
        problems += check("README.md", readme_figures(readme_path), reference)
        problems += check(
            "crates/inlaysql-wasm/www/index.html",
            wasm_demo_figures(wasm_path),
            reference,
        )
    except (OSError, ValueError) as exc:
        # A missing anchor (a renamed heading, a restructured table) means
        # this script's assumptions about the file layout are stale, not
        # that the benchmark figures are wrong. Fail loudly either way — a
        # check that cannot find what it is looking for is not a pass.
        print(f"could not check benchmark figures: {exc}", file=sys.stderr)
        return 2

    if problems:
        print("Benchmark figures have drifted from BENCHMARK.md:", file=sys.stderr)
        print(file=sys.stderr)
        for problem in problems:
            print(problem, file=sys.stderr)
        print(file=sys.stderr)
        print(
            "Every number above needs to either match a figure in one of "
            "BENCHMARK.md's own tables, or BENCHMARK.md needs regenerating "
            "first. See BENCHMARK.md's opening note on precision before "
            "picking a replacement value.",
            file=sys.stderr,
        )
        return 1

    print(
        f"OK: benchmark figures in README.md and "
        f"crates/inlaysql-wasm/www/index.html all trace back to a "
        f"BENCHMARK.md table."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
