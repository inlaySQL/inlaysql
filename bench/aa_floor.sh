#!/usr/bin/env bash
# The harness measuring *itself*: two arms, same binary, same environment,
# same volume, same transaction count — differing in nothing at all.
#
# Every experiment in PERF.md's flush/gate area (AHL-561, AHL-562, AHL-563,
# AHL-564) has had to caveat its throughput column against a noise floor
# estimated from whichever of its own rows happened to be an accidental A/A —
# the one-writer row, when the change had nothing to do there. AHL-564 could
# not even do that, because a smaller WAL record helps a solo writer too. This
# script replaces the accident with a control: it is `bench/gate_hold.sh` and
# `bench/flush_duty_cycle.sh` with the independent variable removed, so the
# spread of its paired ratios *is* the floor, at every writer count rather
# than only at one.
#
# Arm order still flips every repetition, for the same reason it does in the
# real harnesses: a position-in-round effect (page cache, CPU frequency, a
# neighbour on the host) has to land on both labels equally or it would show
# up here as signal, which is exactly what this script exists to price.
#
# Run it from the repository root:
#
#   docker compose -f bench/external/compose.yml --profile oltp-container \
#     run --rm --build -e REPS=10 --entrypoint bash inlaysql-oltp \
#     /workspace/bench/aa_floor.sh
#
# Read the output with `bench/summarise_barrier.py`, which prints, per writer
# count, the paired b/a ratio of every metric — ops/s, gate hold, duty cycle,
# commits per barrier. A real experiment's ratio is only a result if it is
# outside the band this prints for the same metric at the same writer count.
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
REPS=${REPS:-10}
for rep in $(seq 1 "$REPS"); do
  if [ $((rep % 2)) -eq 1 ]; then ORDER="a b"; else ORDER="b a"; fi
  for arm in $ORDER; do
    echo "=== rep $rep arm $arm ==="
    # No environment difference here, deliberately, and no branch on $arm
    # beyond the label above: that absence is the whole experiment.
    WRITER_LEVELS=${WRITER_LEVELS:-1,2,4,8,16} "$BIN" --suite concurrency \
      --writers 16 --txns "${TXNS:-150}" 2>&1
  done
done
