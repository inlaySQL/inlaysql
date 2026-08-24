#!/usr/bin/env bash
#
# Run ./bench/run.sh several times and report the median and the spread.
#
#   ./bench/repeat.sh                    # 3 runs, default parameters
#   REPEATS=5 ./bench/repeat.sh          # 5 runs
#   REPEATS=5 SUITE=retrieval ./bench/repeat.sh
#   REPEATS=5 SUITE=points DOCS=20000 ./bench/repeat.sh
#
# Every parameter bench/run.sh understands is passed straight through, because
# this script only sets REPEATS and then gets out of the way.
#
# Why this exists. One run of a latency benchmark on a developer laptop is
# worth about a factor of two: BENCHMARK.md has twice now carried figures that
# moved for reasons no commit could explain, in one case halving a point-read
# number on a path the commit did not touch. A figure nobody can reproduce is
# worse than no figure, and "reproduce" has to mean the same machine on a
# different afternoon, not just the same command. So: run it N times, publish
# the median, and publish how far the runs disagreed. A metric whose runs
# disagree by 30% does not get quoted to three digits.
#
# What it cannot fix: repeating a benchmark measures the machine's variance,
# not its bias. If something else on the machine is stealing a core for the
# whole sitting, every run pays it and the spread stays narrow while the median
# is wrong. Note what else was running; the spread is a floor on the error bar,
# not the whole of it.

set -euo pipefail

REPEATS=${REPEATS:-3}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS="$ROOT/bench/results"
mkdir -p "$RESULTS"

if [[ "$REPEATS" -lt 2 ]]; then
  echo "REPEATS=$REPEATS: nothing to compare — use ./bench/run.sh for a single run" >&2
  exit 2
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT="$RESULTS/$STAMP-repeat.txt"

runs=()
for attempt in $(seq 1 "$REPEATS"); do
  echo "=== run $attempt of $REPEATS ===" >&2
  # run.sh prints its own report and ends with "written to <path>"; that path
  # is the raw output for this attempt, kept so the summary can be recomputed
  # or audited without running anything again.
  transcript="$("$ROOT/bench/run.sh")"
  written="$(printf '%s\n' "$transcript" | sed -n 's/^written to //p' | tail -1)"
  if [[ -z "$written" ]]; then
    echo "run $attempt did not report where it wrote its results; giving up" >&2
    exit 1
  fi
  runs+=("$ROOT/$written")
done

{
  echo "date:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
  echo "dirty:  $(git -C "$ROOT" status --porcelain 2>/dev/null | head -c 1 | grep -q . && echo yes || echo no)"
  echo "rustc:  $(rustc --version)"
  echo "host:   $(uname -srm)"
  echo
  "$ROOT/bench/summarise.py" "${runs[@]}"
} | tee "$OUTPUT"

echo
echo "written to ${OUTPUT#"$ROOT"/}"
