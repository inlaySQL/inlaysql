#!/usr/bin/env bash
# The paired, interleaved duty-cycle measurement behind PERF.md's AHL-562
# section: the concurrency suite run inside the `inlaysql-oltp` compose
# service, on that service's own named volume (btrfs, the same volume class
# every containerised row in BENCHMARK.md is measured on), with flush
# pipelining off and on.
#
# Both arms are the *same binary*, one environment variable apart
# (`INLAYSQL_FLUSH_PIPELINE`), and the arm order flips every repetition, so a
# position-in-round effect cancels instead of being attributed to the flag.
# Read the one-writer rows first: the pipeline records zero handoffs there, so
# that pair is an A/A test of the harness itself.
#
# Run it the way PERF.md's numbers were produced — from the repository root:
#
#   docker compose -f bench/external/compose.yml --profile oltp-container \
#     run --rm --build -e REPS=6 --entrypoint bash inlaysql-oltp \
#     /workspace/bench/flush_duty_cycle.sh
#
# The lines worth reading out of the output are `barrier cycle:` (barriers/s,
# fsync, interval, idle, and the pipeline's handoff share), `barriers:`
# (commits per barrier) and `buckets:` (where a writer's own time goes).
set -e
cd /workspace
cargo build --release --quiet -p inlaysql-bench
BIN=/target/release/inlaysql-bench
# The database file has to land on the named volume, not on the bind-mounted
# workspace: the suite writes into `./target` relative to its cwd.
mkdir -p /data/run/target
cd /data/run
echo "FILESYSTEM: $(df -T /data | tail -1)"
REPS=${REPS:-6}
for rep in $(seq 1 "$REPS"); do
  if [ $((rep % 2)) -eq 1 ]; then ORDER="off on"; else ORDER="on off"; fi
  for arm in $ORDER; do
    echo "=== rep $rep arm $arm ==="
    if [ "$arm" = "on" ]; then export INLAYSQL_FLUSH_PIPELINE=1; else unset INLAYSQL_FLUSH_PIPELINE; fi
    WRITER_LEVELS=${WRITER_LEVELS:-1,2,4,8,16} "$BIN" --suite concurrency \
      --writers 16 --txns "${TXNS:-150}" 2>&1
  done
done
