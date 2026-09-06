#!/usr/bin/env bash
# The interleaved re-measurement behind `BENCHMARK.md`'s batch-insert cell:
# InlaySQL, MySQL 8.4 and PostgreSQL 17, all three containerised, all three on
# the same class of named volume, 100 rows per autocommitted `INSERT`.
#
# Why this script exists rather than three commands in a comment. The published
# cell (`BENCHMARK.md`, "Batch insert — like for like") was assembled from
# `sql_shapes --mode batch` run once, `batch_driver.py TARGET=mysql` run once
# and `batch_driver.py TARGET=postgres` run once, in that order, in one
# sitting. Three engines measured one after another is exactly the shape
# `bench/profile_ab.sh` exists to refuse: the `LIMIT`-join cache was first
# measured at 1.31x that way and was really 1.42x, because the machine drifted
# between the arms and the drift landed on one of them. Here it is worse than
# it was there, because the InlaySQL row and the server rows came from
# *different sittings a day apart*. This script runs one repetition of each
# engine per round and rotates which engine goes first, so a drift lands on all
# three.
#
# Everything is a `REPS`-repetition median inside its own process, which is the
# published methodology; `ROUNDS` of those, which is the part that was missing.
# The spread across rounds for a single engine is this harness's own A/A
# figure, and every ratio below should be read against it.
#
# Prerequisites — the compose stack, without the retrieval services (pgvector,
# Meilisearch) or the wire server, none of which this workload touches:
#
#   mkdir -p target/bench-corpus
#   docker compose -f bench/external/compose.yml up -d --no-deps --build \
#     postgres mysql drivers
#
# Then, from the repository root:
#
#   ROUNDS=5 REPS=5 ./bench/batch_insert.sh | tee /tmp/batch-insert.txt
#
# Env: `ROUNDS` (interleaved rounds, default 5), `REPS` (repetitions inside
# each engine's own process, default 5 — the published cell's), `BATCH` (rows
# per statement, default 100), `STATEMENTS` (statements per rep, default 100).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# shellcheck source=bench/load_gate.sh
. "$ROOT/bench/load_gate.sh"

trap load_gate_cleanup EXIT

COMPOSE=(docker compose -f bench/external/compose.yml)
ROUNDS=${ROUNDS:-5}
REPS=${REPS:-5}
BATCH=${BATCH:-100}
STATEMENTS=${STATEMENTS:-100}

# The same per-checkout namespacing `bench/compare.sh` applies, and for the
# same reason: two worktrees measuring at once must not share one cargo lock.
INLAYSQL_OLTP_TARGET_VOLUME="inlaysql-bench-target-$(printf '%s' "$ROOT" | shasum | cut -c1-12)"
export INLAYSQL_OLTP_TARGET_VOLUME

echo "commit: $(git -C "$ROOT" rev-parse --short HEAD)"
echo "dirty: $(git -C "$ROOT" status --porcelain | grep -q . && echo yes || echo no)"
echo "rounds=$ROUNDS reps=$REPS batch=$BATCH statements=$STATEMENTS"
load_gate_preflight
load_gate_start_line

inlaysql_round() {
  "${COMPOSE[@]}" --profile oltp-container run --rm -T \
    -e "DIR=/data" -e "MODE=batch" -e "REPS=$REPS" -e "BATCH=$BATCH" \
    -e "STATEMENTS=$STATEMENTS" \
    --entrypoint /target/release/sql_shapes inlaysql-oltp
}

driver_round() {
  "${COMPOSE[@]}" exec -T \
    -e "TARGET=$1" -e "REPS=$REPS" -e "BATCH=$BATCH" -e "STATEMENTS=$STATEMENTS" \
    drivers python /drivers/batch_driver.py
}

# The build is a phase this script does not measure — `bench/compare.sh` makes
# the same separation, and for the same reason: compiling saturates the machine
# by design, so a load reading taken across it says nothing about the run.
"${COMPOSE[@]}" --profile oltp-container run --rm -T \
  --entrypoint bash inlaysql-oltp -c \
  'cd /workspace && cargo build --release --quiet -p inlaysql-bench --bin sql_shapes'

load_gate_start_sampler

for round in $(seq 1 "$ROUNDS"); do
  # Rotate the starting engine so no engine is systematically first.
  case $((round % 3)) in
    1) ORDER="inlaysql mysql postgres" ;;
    2) ORDER="mysql postgres inlaysql" ;;
    *) ORDER="postgres inlaysql mysql" ;;
  esac
  for engine in $ORDER; do
    echo "=== round $round engine $engine ==="
    case "$engine" in
      inlaysql) inlaysql_round ;;
      *) driver_round "$engine" ;;
    esac
  done
done

load_gate_stop_sampler
load_gate_summary
