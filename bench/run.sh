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

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT="$RESULTS/$STAMP.txt"

{
  echo "date:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
  echo "dirty:  $(git -C "$ROOT" status --porcelain 2>/dev/null | head -c 1 | grep -q . && echo yes || echo no)"
  echo "rustc:  $(rustc --version)"
  echo "host:   $(uname -srm)"
  echo
  cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" -p inlaysql-bench -- \
    --suite "$SUITE" \
    --docs "$DOCS" --queries "$QUERIES" --seed "$SEED" --dim "$DIM" --limit "$LIMIT" \
    --rows "$ROWS" --lookups "$LOOKUPS" --payload "$PAYLOAD" \
    --writers "$WRITERS" --txns "$TXNS"
} | tee "$OUTPUT"

echo
echo "written to ${OUTPUT#"$ROOT"/}"
