#!/usr/bin/env bash
#
# Run ./bench/compare.sh several times and report the median and the spread.
#
#   ./bench/repeat-compare.sh                     # 3 runs, default parameters
#   REPEATS=5 ./bench/repeat-compare.sh           # 5 runs
#   REPEATS=5 ROWS=20000 LOOKUPS=5000 ./bench/repeat-compare.sh
#   COOLDOWN_SECONDS=0 ./bench/repeat-compare.sh  # no pause between repetitions
#
# Every parameter bench/compare.sh understands is passed straight through,
# because this script only sets REPEATS/COOLDOWN_SECONDS and then gets out of
# the way. This is `bench/repeat.sh` for the comparison half of the suite.
#
# Why this exists. `bench/repeat.sh` has wrapped `run.sh` for a long time, and
# BENCHMARK.md's SQLite rows carry a median and a spread because of it. Every
# row measured against MySQL, PostgreSQL, pgvector, DuckDB and Meilisearch —
# which is to say every row where we claim to beat another *engine* rather than
# another configuration of ourselves — came from a single run of `compare.sh`
# with no repetition and no spread. PERF.md, bench/README.md and SCOREBOARD.md
# each recorded that gap independently, and SCOREBOARD.md draws the direct
# conclusion: without this, no compare-sourced cell can earn a WIN or a LOSS,
# only UNKNOWN, however good the raw numbers look. The one time these engines
# *were* interleaved and repeated by hand (2026-08-30), the exercise moved a
# published multiple and killed a sequential-ordering claim that a single run
# had produced. Do that by script, or it does not get done.
#
# What it cannot fix, restated from repeat.sh because it is more true here:
# repeating a benchmark measures the machine's variance, not its bias. And this
# script repeats whole `compare.sh` invocations — it does not interleave the
# engines *within* one run, because compare.sh's own phase order is fixed. A
# known, separate, still-open effect lives in that order: running the
# server-to-server phase immediately after the MySQL driver's write burst
# reproduces a ~30% read-throughput drop that does not reproduce in isolation
# (PLAN.md, W5). COOLDOWN_SECONDS pauses *between repetitions*, which does not
# address that; it keeps one repetition's tail from landing on the next
# repetition's head.

set -euo pipefail

REPEATS=${REPEATS:-3}

# A pause between repetitions, so the machine is in the same state at the start
# of each one. Docker's own background work — image layer commits, volume
# teardown, the VM's page cache settling — outlives the `compose down` that
# ends a run, and the next repetition's first phase is the one that would pay
# for it.
COOLDOWN_SECONDS=${COOLDOWN_SECONDS:-30}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS="$ROOT/bench/results"
mkdir -p "$RESULTS"

if [[ "$REPEATS" -lt 2 ]]; then
  echo "REPEATS=$REPEATS: nothing to compare — use ./bench/compare.sh for a single run" >&2
  exit 2
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT="$RESULTS/$STAMP-repeat-compare.txt"

TRANSCRIPT=""
cleanup() {
  if [[ -n "$TRANSCRIPT" ]]; then
    rm -f "$TRANSCRIPT"
  fi
}
trap cleanup EXIT

runs=()
for attempt in $(seq 1 "$REPEATS"); do
  if [[ "$attempt" -gt 1 && "$COOLDOWN_SECONDS" -gt 0 ]]; then
    echo "=== cooling down ${COOLDOWN_SECONDS}s before run $attempt ===" >&2
    sleep "$COOLDOWN_SECONDS"
  fi
  echo "=== run $attempt of $REPEATS ===" >&2

  # Streamed *and* captured, unlike repeat.sh's command substitution: a
  # compare run is container builds and five drivers, tens of minutes, and a
  # wrapper that shows nothing until it finishes is a wrapper nobody waits
  # for. The transcript is still needed afterwards for the "written to" path.
  TRANSCRIPT="$(mktemp "${TMPDIR:-/tmp}/inlaysql-repeat-compare.XXXXXX")"
  status=0
  "$ROOT/bench/compare.sh" 2>&1 | tee "$TRANSCRIPT" || status=$?
  # With `set -o pipefail` the pipeline's status is compare.sh's when it is the
  # part that failed, which is the one this loop cares about.
  if [[ "$status" -ne 0 ]]; then
    echo "run $attempt of $REPEATS exited $status — see the output above" >&2
    # Exit 3 is the load gate refusing a busy machine. Say so, because the
    # answer is different from every other failure: wait, do not debug.
    if [[ "$status" -eq 3 ]]; then
      echo "that is the quiet-machine gate, not a benchmark failure — retry when the machine is idle" >&2
    fi
    exit "$status"
  fi

  written="$(sed -n 's/^written to //p' "$TRANSCRIPT" | tail -1)"
  if [[ -z "$written" ]]; then
    echo "run $attempt did not report where it wrote its results; giving up" >&2
    exit 1
  fi
  runs+=("$ROOT/$written")
  rm -f "$TRANSCRIPT"
  TRANSCRIPT=""
done

{
  echo "date:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
  echo "dirty:  $(git -C "$ROOT" status --porcelain 2>/dev/null | head -c 1 | grep -q . && echo yes || echo no)"
  echo "rustc:  $(rustc --version)"
  echo "host:   $(uname -srm)"
  echo "docker: $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo unknown)"
  echo "cooldown: ${COOLDOWN_SECONDS}s between repetitions"
  echo
  "$ROOT/bench/summarise.py" "${runs[@]}"
} | tee "$OUTPUT"

echo
echo "written to ${OUTPUT#"$ROOT"/}"
