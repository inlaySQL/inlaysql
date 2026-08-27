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

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS="$ROOT/bench/results"
mkdir -p "$RESULTS"

if [[ "$BENCH_MAX_LOAD_PER_CPU" != "off" ]]; then
  CPU_COUNT=""
  if command -v sysctl >/dev/null 2>&1; then
    CPU_COUNT="$(sysctl -n hw.logicalcpu 2>/dev/null || true)"
  fi
  if [[ -z "$CPU_COUNT" ]]; then
    CPU_COUNT="$(getconf _NPROCESSORS_ONLN 2>/dev/null || true)"
  fi
  LOAD_1="$(uptime | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+\.[0-9]+,?$/) { gsub(",", "", $i); print $i; exit } }')"
  if [[ ! "$CPU_COUNT" =~ ^[1-9][0-9]*$ || ! "$LOAD_1" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    echo "could not read logical CPU count/load average; set BENCH_MAX_LOAD_PER_CPU=off to override" >&2
    exit 3
  fi
  if awk -v load="$LOAD_1" -v cpus="$CPU_COUNT" -v max="$BENCH_MAX_LOAD_PER_CPU" \
    'BEGIN { exit !(load / cpus > max) }'; then
    echo "machine load ${LOAD_1}/${CPU_COUNT} exceeds BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU}; refusing benchmark" >&2
    echo "set BENCH_MAX_LOAD_PER_CPU=off only for a deliberate under-load run" >&2
    exit 3
  fi
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
  echo "load:   ${LOAD_1}/${CPU_COUNT} logical CPUs (max per CPU: ${BENCH_MAX_LOAD_PER_CPU})"
  echo
  cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" -p inlaysql-bench -- \
    --suite "$SUITE" \
    --docs "$DOCS" --queries "$QUERIES" --seed "$SEED" --dim "$DIM" --limit "$LIMIT" \
    --rows "$ROWS" --lookups "$LOOKUPS" --payload "$PAYLOAD" \
    --writers "$WRITERS" --txns "$TXNS"
} | tee "$OUTPUT"

echo
echo "written to ${OUTPUT#"$ROOT"/}"
