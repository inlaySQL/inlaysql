#!/usr/bin/env bash
# The paired, interleaved measurement behind PERF.md's AHL-563 section: the
# reservation gate's *hold*, before and after a WAL region wrap stopped
# forgetting the other three regions' append offsets.
#
# Same harness as `bench/flush_duty_cycle.sh` (AHL-562) and for the same
# reason: the concurrency suite inside the `inlaysql-oltp` compose service, on
# that service's own named btrfs volume, `Durability::Full`. Both arms are the
# *same binary*, one environment variable apart (`INLAYSQL_WIDE_WRAP_FORGET`
# puts the pre-change total forget back), and the arm order flips every
# repetition so a position-in-round effect cancels instead of being attributed
# to the change.
#
# Read the one-writer rows first. A single writer owns one region and never
# competes for the gate, so the change has nothing to do there and that pair is
# an A/A test of the harness itself — AHL-562's own A/A moved 1.34x, so any
# ratio smaller than the one-writer row's is noise.
#
# Run it from the repository root:
#
#   docker compose -f bench/external/compose.yml --profile oltp-container \
#     run --rm --build -e REPS=6 --entrypoint bash inlaysql-oltp \
#     /workspace/bench/gate_hold.sh
#
# `INLAYSQL_GATE_PHASES=1` additionally splits the hold by code phase; it is on
# here because the phase split *is* the deliverable. The lines worth reading
# are `gate hold:` (the mean hold, its device/residual split and the
# commit-point miss count), `gate phases:` (which part of `CowBTree::commit`
# the hold is), `buckets:` (gate_wait's share of a writer's own time) and
# `barrier cycle:` (duty cycle and interval).
set -e
cd /workspace
cargo build --release --quiet -p inlaysql-bench
BIN=/target/release/inlaysql-bench
# The database file has to land on the named volume, not on the bind-mounted
# workspace: the suite writes into `./target` relative to its cwd.
mkdir -p /data/run/target
cd /data/run
echo "FILESYSTEM: $(df -T /data | tail -1)"
export INLAYSQL_GATE_PHASES=${INLAYSQL_GATE_PHASES:-1}
REPS=${REPS:-6}
for rep in $(seq 1 "$REPS"); do
  if [ $((rep % 2)) -eq 1 ]; then ORDER="wide narrow"; else ORDER="narrow wide"; fi
  for arm in $ORDER; do
    echo "=== rep $rep arm $arm ==="
    if [ "$arm" = "wide" ]; then export INLAYSQL_WIDE_WRAP_FORGET=1; else unset INLAYSQL_WIDE_WRAP_FORGET; fi
    WRITER_LEVELS=${WRITER_LEVELS:-1,2,4,8,16} "$BIN" --suite concurrency \
      --writers 16 --txns "${TXNS:-150}" 2>&1
  done
done
