#!/usr/bin/env bash
#
# Run the InlaySQL benchmark and record the result with enough context to
# reproduce it. Parameters are pinned here rather than passed ad hoc, so two
# runs on two machines are comparing the same thing.
#
#   ./bench/run.sh                 # defaults
#   DOCS=20000 ./bench/run.sh      # override any parameter
#   SUITE=quantization DOCS=100000 QUERIES=50 ./bench/run.sh
#   SUITE=indexed ROWS=100000 ./bench/run.sh
#   SUITE=joins ROWS=20000 QUERIES=100 LIMIT=20 ./bench/run.sh
#
# Results land in bench/results/ and are not committed.
#
# This script covers the baselines that link into the harness: SQLite and
# sqlite-vec. DuckDB and pgvector cannot, so they live in ./bench/compare.sh,
# which needs Docker.

set -euo pipefail

DOCS=${DOCS:-2000}
QUERIES=${QUERIES:-100}
SEED=${SEED:-42}
DIM=${DIM:-384}
LIMIT=${LIMIT:-10}
ROWS=${ROWS:-20000}
LOOKUPS=${LOOKUPS:-5000}
PAYLOAD=${PAYLOAD:-64}
WRITERS=${WRITERS:-8}
TXNS=${TXNS:-200}
SUITE=${SUITE:-all}

# Refuse to produce a benchmark result on a busy machine. A one-minute load
# average below a fraction of the logical CPU count is not a proof that the
# machine is quiet, but it catches the obvious case that has repeatedly moved
# this repository's concurrency numbers by a factor of two. Set this to
# `off` only when the caller is deliberately measuring under load; the raw
# output records the override through the `load:` line below.
BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU:-0.25}

# A one-shot check before the run starts cannot catch a spike that arrives
# after it: PERF.md §4 measured the correlation between disclosed start-load
# and actual point-read throughput at r≈0.18 on runs that all passed the gate
# above, because nothing looked again once the run was under way. Sample
# every this-many seconds for the duration of the run instead, so the
# published result carries what the machine was actually doing throughout,
# not just at the starting gun.
BENCH_LOAD_SAMPLE_SECONDS=${BENCH_LOAD_SAMPLE_SECONDS:-5}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS="$ROOT/bench/results"
mkdir -p "$RESULTS"

# One-minute load average as a bare number, or empty if it could not be read.
# Shared by the pre-flight gate below and the in-flight sampler, so both read
# the load the same way.
read_load_1() {
  uptime | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+\.[0-9]+,?$/) { gsub(",", "", $i); print $i; exit } }'
}

CPU_COUNT=""
LOAD_1=""
LOAD_SAMPLES=""
SAMPLER_PID=""

cleanup_sampler() {
  if [[ -n "$SAMPLER_PID" ]]; then
    kill "$SAMPLER_PID" >/dev/null 2>&1 || true
    wait "$SAMPLER_PID" 2>/dev/null || true
  fi
  [[ -n "$LOAD_SAMPLES" ]] && rm -f "$LOAD_SAMPLES"
}
trap cleanup_sampler EXIT

if [[ "$BENCH_MAX_LOAD_PER_CPU" != "off" ]]; then
  if command -v sysctl >/dev/null 2>&1; then
    CPU_COUNT="$(sysctl -n hw.logicalcpu 2>/dev/null || true)"
  fi
  if [[ -z "$CPU_COUNT" ]]; then
    CPU_COUNT="$(getconf _NPROCESSORS_ONLN 2>/dev/null || true)"
  fi
  LOAD_1="$(read_load_1)"
  if [[ ! "$CPU_COUNT" =~ ^[1-9][0-9]*$ || ! "$LOAD_1" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    echo "could not read logical CPU count/load average; set BENCH_MAX_LOAD_PER_CPU=off to override" >&2
    exit 3
  fi
  # `load` is a built-in function name in gawk.  Using it as an assigned
  # variable works with some awk implementations but makes the benchmark
  # guard itself fail before a run on GNU/Linux, which is where CI and the
  # published benchmark runner execute.
  if awk -v load_avg="$LOAD_1" -v cpus="$CPU_COUNT" -v max="$BENCH_MAX_LOAD_PER_CPU" \
    'BEGIN { exit !(load_avg / cpus > max) }'; then
    echo "machine load ${LOAD_1}/${CPU_COUNT} exceeds BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU}; refusing benchmark" >&2
    echo "set BENCH_MAX_LOAD_PER_CPU=off only for a deliberate under-load run" >&2
    exit 3
  fi

  # Passing the gate above only proves the machine was quiet before the run
  # started. Keep sampling for the run's whole duration and record the
  # spread, rather than trusting the one reading taken before anything ran.
  # Policy for a run whose load exceeds the threshold mid-flight: do not
  # abort it (a long suite can run for many minutes, and discarding it wastes
  # more than the contamination costs); finish it, but mark the result file
  # CONTAMINATED, loudly, in a form `bench/summarise.py` also surfaces when
  # it combines runs — the flag has to survive being combined with clean
  # runs, not get silently dropped along with the rest of the provenance.
  LOAD_SAMPLES="$(mktemp "${TMPDIR:-/tmp}/inlaysql-bench-load.XXXXXX")"
  echo "$LOAD_1" > "$LOAD_SAMPLES"
  (
    flagged=0
    while sleep "$BENCH_LOAD_SAMPLE_SECONDS"; do
      sample="$(read_load_1 2>/dev/null || true)"
      [[ "$sample" =~ ^[0-9]+([.][0-9]+)?$ ]] || continue
      echo "$sample" >> "$LOAD_SAMPLES"
      if [[ "$flagged" -eq 0 ]] && awk -v l="$sample" -v c="$CPU_COUNT" -v m="$BENCH_MAX_LOAD_PER_CPU" \
        'BEGIN { exit !(l / c > m) }'; then
        flagged=1
        echo "*** bench/run.sh: load spike detected mid-run (${sample}/${CPU_COUNT} exceeds BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU} at $(date -u +%H:%M:%SZ)) — letting the run finish rather than aborting it, but the result will be marked CONTAMINATED ***" >&2
      fi
    done
  ) &
  SAMPLER_PID=$!
else
  CPU_COUNT="unknown"
  LOAD_1="override"
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT="$RESULTS/$STAMP.txt"

{
  echo "date:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
  echo "dirty:  $(git -C "$ROOT" status --porcelain 2>/dev/null | head -c 1 | grep -q . && echo yes || echo no)"
  echo "rustc:  $(rustc --version)"
  echo "host:   $(uname -srm)"
  echo "load:   ${LOAD_1}/${CPU_COUNT} logical CPUs at start (max per CPU: ${BENCH_MAX_LOAD_PER_CPU})"
  echo
  cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" -p inlaysql-bench -- \
    --suite "$SUITE" \
    --docs "$DOCS" --queries "$QUERIES" --seed "$SEED" --dim "$DIM" --limit "$LIMIT" \
    --rows "$ROWS" --lookups "$LOOKUPS" --payload "$PAYLOAD" \
    --writers "$WRITERS" --txns "$TXNS"
} | tee "$OUTPUT"

# Stop sampling and take one last reading right at the end, then fold
# start + in-flight + end into the min/median/max the result file publishes.
# This block is what makes the load line honest about the whole run rather
# than just its first second; see PERF.md §4 for why that gap mattered.
if [[ -n "$SAMPLER_PID" ]]; then
  kill "$SAMPLER_PID" >/dev/null 2>&1 || true
  wait "$SAMPLER_PID" 2>/dev/null || true
  SAMPLER_PID=""
fi
if [[ -n "$LOAD_SAMPLES" ]]; then
  read_load_1 >> "$LOAD_SAMPLES" 2>/dev/null || true
  {
    if [[ -s "$LOAD_SAMPLES" ]]; then
      SORTED="$(sort -n "$LOAD_SAMPLES")"
      N="$(printf '%s\n' "$SORTED" | grep -c .)"
      MIN="$(printf '%s\n' "$SORTED" | head -1)"
      MAX="$(printf '%s\n' "$SORTED" | tail -1)"
      MEDIAN="$(printf '%s\n' "$SORTED" | awk -v n="$N" '{a[NR]=$1} END { if (n % 2 == 1) print a[(n+1)/2]; else print (a[n/2]+a[n/2+1])/2 }')"
      EXCEEDED="no"
      if awk -v m="$MAX" -v c="$CPU_COUNT" -v t="$BENCH_MAX_LOAD_PER_CPU" 'BEGIN { exit !(m / c > t) }'; then
        EXCEEDED="yes"
      fi
      echo
      echo "load:   sampled ${N} times every ${BENCH_LOAD_SAMPLE_SECONDS}s throughout the run — min ${MIN}/${CPU_COUNT} median ${MEDIAN}/${CPU_COUNT} max ${MAX}/${CPU_COUNT} (threshold ${BENCH_MAX_LOAD_PER_CPU} per CPU)"
      if [[ "$EXCEEDED" == "yes" ]]; then
        BANNER="load:   *** CONTAMINATED: load exceeded BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU} during this run (max ${MAX}/${CPU_COUNT}) — do not treat these numbers as clean; see PERF.md §4 ***"
        echo "$BANNER"
        echo "$BANNER" >&2
      fi
    fi
  } | tee -a "$OUTPUT"
fi

echo
echo "written to ${OUTPUT#"$ROOT"/}"
