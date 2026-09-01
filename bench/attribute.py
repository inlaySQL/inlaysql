#!/usr/bin/env python3
"""Attribute a macOS `sample` profile's costs to the engine code that caused them.

    ./bench/attribute.py /tmp/indexed-range.sample
    ./bench/attribute.py /tmp/x.sample --symbol memcmp
    ./bench/attribute.py /tmp/x.sample --symbol malloc,free,_xzm

# Why this exists

`sample`'s own "top of stack" list says *what* the CPU was in, which for an
engine like this one is mostly `memcmp`, `memmove` and the allocator — three
answers that name a mechanism and no cause. The question worth asking is which
engine function asked for that work, and answering it by reading the call graph
by eye is where this repository has repeatedly guessed wrong:

* `PERF.md`'s residual-filter item (A1) estimated 15-20% recoverable by adding
  the filter's own cost to "a chosen index's `memcmp` share". Attributed, the
  `memcmp` was B-tree descent — `get_from` 8.5%, the walk helpers ~7% — and the
  filter's actual route to it, `Collation::compare`, was 0.9%. The item was
  rejected on that basis after a ceiling measurement confirmed it.
* Its replacement (A1a) assumed the point path pays a per-cell leaf decode.
  Attributed, `page::decode` did not appear in the profile at all.

Both were plausible, both were wrong, and both took minutes to disprove once
the samples were attributed rather than eyeballed. That is what this is for.

# What it prints

Three views, cheapest to read first:

* **By group** — every sample bucketed into engine subsystems, so the shape of
  a workload is visible before any detail.
* **By nearest engine caller** — for the symbols worth attributing (the
  default set is the allocator, `memcmp` and `memmove`), which engine function
  is nearest on the stack. This is the view that overturns assumptions.
* **Top leaves** — `sample`'s own answer, kept because it is still the right
  place to notice something unexpected.

Percentages are of the sampled thread's total, so they are directly
comparable across runs of different lengths.
"""

from __future__ import annotations

import argparse
import re
import sys

# One call-graph line: indentation made of `+ ! : |` and spaces, a sample
# count, then the symbol. The indentation width is the stack depth, which is
# what makes "nearest ancestor" answerable.
LINE = re.compile(r"^(?P<prefix>[\s+!:|]*?)(?P<count>\d+) (?P<symbol>.*)$")

# Rust's hash suffix carries no information here and splits one function into
# many rows if left on.
HASH_SUFFIX = re.compile(r"::h[0-9a-f]{16}$")

# The symbols worth asking "who called this?" about: they are the ones that
# name a mechanism rather than a cause.
DEFAULT_ATTRIBUTE = ("malloc", "free", "_xzm", "realloc", "memcmp", "memmove")

# Subsystem buckets, first match wins. Deliberately coarse: the point is the
# shape of a workload, not a taxonomy.
GROUPS = (
    ("eval/filter", ("eval::", "compare_cells", "affinity", "truth", "logical_", "evaluate")),
    ("btree/page", ("btree::", "page::", "PageCache")),
    ("row codec", ("row::", "ValueRef", "value::")),
    ("index/retrieval", ("index::", "hnsw", "bm25")),
    ("storage/device", ("Device", "storage::", "pread", "fsync")),
    ("allocator", ("malloc", "free", "_xzm", "realloc", "rust_alloc")),
    ("memcmp/memmove", ("memcmp", "memmove", "memcpy")),
    ("harness timer", ("mach_absolute_time",)),
)


def clean(symbol: str) -> str:
    """The symbol without its binary and without Rust's hash suffix."""
    return HASH_SUFFIX.sub("", symbol.split("  (in ")[0])


def parse(path: str) -> tuple[int, list[tuple[int, int, str]]]:
    """`(total samples, [(depth, count, symbol)])` from the call graph."""
    with open(path, encoding="utf-8", errors="replace") as handle:
        lines = handle.read().split("\n")
    try:
        start = next(i for i, line in enumerate(lines) if line.startswith("Call graph:"))
    except StopIteration:
        raise SystemExit(f"{path}: no 'Call graph:' section — is this a `sample` output?")
    end = next(
        (i for i, line in enumerate(lines) if line.startswith("Total number in stack")),
        len(lines),
    )

    # The first line under the header is the sampled thread and carries the
    # total; every percentage below is a fraction of it.
    header = re.match(r"^\s*(\d+) Thread", lines[start + 1])
    if not header:
        raise SystemExit(f"{path}: could not read the thread total")
    total = int(header.group(1))

    rows = []
    for line in lines[start + 1 : end]:
        match = LINE.match(line)
        if match:
            rows.append(
                (len(match.group("prefix")), int(match.group("count")), clean(match.group("symbol")))
            )
    return total, rows


def leaves(path: str) -> dict[str, int]:
    """`sample`'s own top-of-stack tally."""
    with open(path, encoding="utf-8", errors="replace") as handle:
        lines = handle.read().split("\n")
    try:
        at = next(i for i, line in enumerate(lines) if line.startswith("Sort by top of stack"))
    except StopIteration:
        return {}
    out: dict[str, int] = {}
    for line in lines[at + 1 :]:
        stripped = line.strip()
        if not stripped:
            break
        match = re.match(r"^(.*?)\s+(\d+)$", stripped)
        if match:
            out[clean(match.group(1))] = int(match.group(2))
    return out


def attribute(rows, wanted: tuple[str, ...], engine: str) -> dict[str, int]:
    """For each `wanted` symbol, the nearest ancestor from `engine`'s code."""
    stack: dict[int, str] = {}
    callers: dict[str, int] = {}
    for depth, count, symbol in rows:
        # Frames deeper than this one belong to a branch already left.
        for gone in [d for d in stack if d > depth]:
            del stack[gone]
        stack[depth] = symbol
        if not any(term in symbol for term in wanted):
            continue
        ancestors = [stack[d] for d in sorted(stack) if d < depth]
        nearest = next((a for a in reversed(ancestors) if engine in a), "(outside the engine)")
        callers[nearest] = callers.get(nearest, 0) + count
    return callers


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("sample", help="a file written by `sample <pid> <seconds> -f <file>`")
    parser.add_argument(
        "--symbol",
        default=",".join(DEFAULT_ATTRIBUTE),
        help="comma-separated substrings to attribute (default: allocator, memcmp, memmove)",
    )
    parser.add_argument(
        "--engine",
        default="inlaysql",
        help="substring identifying our own frames (default: inlaysql)",
    )
    parser.add_argument("--top", type=int, default=12, help="rows per section (default 12)")
    args = parser.parse_args()

    total, rows = parse(args.sample)
    wanted = tuple(term for term in args.symbol.split(",") if term)
    print(f"{args.sample}: {total} samples on the profiled thread\n")

    print("--- by subsystem (every sample, first match wins) ---")
    tally = {name: 0 for name, _ in GROUPS}
    tally["other"] = 0
    for symbol, count in leaves(args.sample).items():
        for name, terms in GROUPS:
            if any(term in symbol for term in terms):
                tally[name] += count
                break
        else:
            tally["other"] += count
    for name, count in sorted(tally.items(), key=lambda item: -item[1]):
        if count:
            print(f"  {count / total * 100:5.1f}%  {name}")

    print(f"\n--- {args.symbol}, by nearest engine caller ---")
    print("  This is the view that answers 'who asked for this work'. A mechanism")
    print("  symbol attributed to the wrong cause is how an optimisation gets")
    print("  scoped against a number that was never there — see this file's docs.")
    callers = attribute(rows, wanted, args.engine)
    if not callers:
        print("  (nothing matched)")
    for caller, count in sorted(callers.items(), key=lambda item: -item[1])[: args.top]:
        print(f"  {count / total * 100:5.1f}%  {caller[:78]}")

    print("\n--- top leaves (`sample`'s own answer) ---")
    for symbol, count in sorted(leaves(args.sample).items(), key=lambda item: -item[1])[: args.top]:
        print(f"  {count / total * 100:5.1f}%  {symbol[:78]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
