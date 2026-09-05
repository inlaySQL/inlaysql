#!/usr/bin/env bash
# The paired, interleaved measurement behind PERF.md's AHL-564 section: the
# reservation gate's hold and the file's throughput, before and after a commit
# record's page entries stopped copying each page's zero hole.
#
# Same harness as `bench/gate_hold.sh` (AHL-563) and `bench/flush_duty_cycle.sh`
# (AHL-562), and for the same reason: the concurrency suite inside the
# `inlaysql-oltp` compose service, on that service's own named btrfs volume,
# `Durability::Full`. Both arms are the *same binary*, one environment variable
# apart — `INLAYSQL_WHOLE_PAGE_WAL_RECORD` makes a newly created database a v5
# one, whose records copy whole page images — and the arm order flips every
# repetition so a position-in-round effect cancels instead of being attributed
# to the change.
#
# The switch works because the suite deletes and recreates its database file on
# every run, and because the format version lives in the file header: an
# existing database is never affected by it. See
# `inlaysql_core::wal::HOLE_ELIDED_FORMAT_VERSION`.
#
# Read the one-writer rows first. A single writer owns one region and never
# competes for the gate, so that pair is close to an A/A test of the harness
# itself — AHL-562's own A/A moved 1.34x and AHL-563's 1.13x, so any ratio
# smaller than the one-writer row's is noise. It is not a *pure* A/A here, the
# way AHL-563's was: a smaller record wraps a region less often even with one
# writer, so the one-writer row carries the part of the effect that does not
# need concurrency. That is stated rather than hidden, and it is why the
# deliverable is the eight- and sixteen-writer gate hold read against this row.
#
# Run it from the repository root:
#
#   docker compose -f bench/external/compose.yml --profile oltp-container \
#     run --rm --build -e REPS=6 --entrypoint bash inlaysql-oltp \
#     /workspace/bench/record_size.sh
#
# `INLAYSQL_GATE_PHASES=1` splits the hold by code phase, which is where this
# change is supposed to show: `wrap` (a region wrap runs an `fsync` inside the
# gate, and a smaller record wraps less often) and `encode` (fewer bytes to
# copy). The lines worth reading are `gate hold:`, `gate phases:`, `buckets:`
# and `barrier cycle:`.
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
  if [ $((rep % 2)) -eq 1 ]; then ORDER="whole elided"; else ORDER="elided whole"; fi
  for arm in $ORDER; do
    echo "=== rep $rep arm $arm ==="
    if [ "$arm" = "whole" ]; then
      export INLAYSQL_WHOLE_PAGE_WAL_RECORD=1
    else
      unset INLAYSQL_WHOLE_PAGE_WAL_RECORD
    fi
    WRITER_LEVELS=${WRITER_LEVELS:-1,2,4,8,16} "$BIN" --suite concurrency \
      --writers 16 --txns "${TXNS:-150}" 2>&1
  done
done
