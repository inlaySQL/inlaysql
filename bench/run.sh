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

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS="$ROOT/bench/results"
mkdir -p "$RESULTS"

# The quiet-machine gate — refuse to produce a benchmark result on a busy
# machine, and keep watching for the run's whole duration rather than trusting
# the one reading taken before anything ran. `bench/compare.sh` sources the
# same module, so both sides of every published comparison are gated
# identically. `BENCH_MAX_LOAD_PER_CPU` and `BENCH_LOAD_SAMPLE_SECONDS` are
# documented there.
# shellcheck source=bench/load_gate.sh
. "$ROOT/bench/load_gate.sh"

trap load_gate_cleanup EXIT

load_gate_preflight
load_gate_start_sampler

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT="$RESULTS/$STAMP.txt"

{
  echo "date:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
  echo "dirty:  $(git -C "$ROOT" status --porcelain 2>/dev/null | head -c 1 | grep -q . && echo yes || echo no)"
  echo "rustc:  $(rustc --version)"
  echo "host:   $(uname -srm)"
  load_gate_start_line
  echo
  cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" -p inlaysql-bench -- \
    --suite "$SUITE" \
    --docs "$DOCS" --queries "$QUERIES" --seed "$SEED" --dim "$DIM" --limit "$LIMIT" \
    --rows "$ROWS" --lookups "$LOOKUPS" --payload "$PAYLOAD" \
    --writers "$WRITERS" --txns "$TXNS"
} | tee "$OUTPUT"

# Stop sampling and take one last reading right at the end, then fold
# start + in-flight + end into the min/median/max the result file publishes.
# This is what makes the load line honest about the whole run rather than just
# its first second; see PERF.md §4 for why that gap mattered.
load_gate_stop_sampler
load_gate_summary | tee -a "$OUTPUT"

echo
echo "written to ${OUTPUT#"$ROOT"/}"
