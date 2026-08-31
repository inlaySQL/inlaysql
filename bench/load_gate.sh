#!/usr/bin/env bash
#
# The quiet-machine gate, shared by `bench/run.sh` and `bench/compare.sh`.
#
# Source it; do not execute it:
#
#   . "$ROOT/bench/load_gate.sh"
#   load_gate_preflight            # refuses to start on a busy machine
#   load_gate_start_sampler        # begin watching for mid-run spikes
#   ...                            # the measured work
#   load_gate_stop_sampler
#   load_gate_summary              # the `load:` line, and the banner if it spiked
#
# Why this is a module rather than two copies. `run.sh` grew the gate first;
# `compare.sh` — the script that produces every MySQL, PostgreSQL, pgvector and
# DuckDB row we publish — went without one for far longer, and PERF.md,
# bench/README.md and SCOREBOARD.md each independently recorded that absence as
# the reason a `compare.sh` figure can never earn better than an UNKNOWN
# verdict. Two copies of a gate is how one of them silently stops matching the
# other; the numbers on both sides of a comparison have to be gated the same
# way or the comparison is the thing being measured.
#
# Callers are expected to arrange their own EXIT trap and call
# `load_gate_cleanup` from it. This file does not install a trap, because the
# script that sources it usually has cleanup of its own (`compare.sh` has
# containers to bring down) and bash allows exactly one EXIT trap.

# A one-minute load average below a fraction of the logical CPU count is not a
# proof that the machine is quiet, but it catches the obvious case that has
# repeatedly moved this repository's concurrency numbers by a factor of two.
# Set this to `off` only when the caller is deliberately measuring under load;
# the result file records the override on its own `load:` line.
BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU:-0.25}

# A one-shot check before the run starts cannot catch a spike that arrives
# after it: PERF.md §4 measured the correlation between disclosed start-load
# and actual point-read throughput at r≈0.18 on runs that all passed the gate,
# because nothing looked again once the run was under way. Sample every
# this-many seconds for the duration instead.
BENCH_LOAD_SAMPLE_SECONDS=${BENCH_LOAD_SAMPLE_SECONDS:-5}

LOAD_GATE_CPU_COUNT=""
LOAD_GATE_LOAD_1=""
LOAD_GATE_SAMPLES=""
LOAD_GATE_SAMPLER_PID=""

# One-minute load average as a bare number, or empty if it could not be read.
# Shared by the pre-flight gate and the in-flight sampler, so both read the
# load the same way.
load_gate_read_1() {
  uptime | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]+\.[0-9]+,?$/) { gsub(",", "", $i); print $i; exit } }'
}

# True when the gate is switched off entirely.
load_gate_disabled() {
  [[ "$BENCH_MAX_LOAD_PER_CPU" == "off" ]]
}

# Refuse to start on a busy machine. Exits 3 rather than returning, because
# every caller's answer to a failed gate is the same one and a benchmark that
# continues past its own refusal is worse than no gate at all.
load_gate_preflight() {
  if load_gate_disabled; then
    LOAD_GATE_CPU_COUNT="unknown"
    LOAD_GATE_LOAD_1="override"
    return 0
  fi

  if command -v sysctl >/dev/null 2>&1; then
    LOAD_GATE_CPU_COUNT="$(sysctl -n hw.logicalcpu 2>/dev/null || true)"
  fi
  if [[ -z "$LOAD_GATE_CPU_COUNT" ]]; then
    LOAD_GATE_CPU_COUNT="$(getconf _NPROCESSORS_ONLN 2>/dev/null || true)"
  fi
  LOAD_GATE_LOAD_1="$(load_gate_read_1)"
  if [[ ! "$LOAD_GATE_CPU_COUNT" =~ ^[1-9][0-9]*$ || ! "$LOAD_GATE_LOAD_1" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
    echo "could not read logical CPU count/load average; set BENCH_MAX_LOAD_PER_CPU=off to override" >&2
    exit 3
  fi
  # `load` is a built-in function name in gawk. Using it as an assigned
  # variable works with some awk implementations but makes the guard itself
  # fail before a run on GNU/Linux, which is where CI and the published
  # benchmark runner execute.
  if awk -v load_avg="$LOAD_GATE_LOAD_1" -v cpus="$LOAD_GATE_CPU_COUNT" -v max="$BENCH_MAX_LOAD_PER_CPU" \
    'BEGIN { exit !(load_avg / cpus > max) }'; then
    echo "machine load ${LOAD_GATE_LOAD_1}/${LOAD_GATE_CPU_COUNT} exceeds BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU}; refusing benchmark" >&2
    echo "set BENCH_MAX_LOAD_PER_CPU=off only for a deliberate under-load run" >&2
    exit 3
  fi
}

# The provenance line a result file carries in its header.
load_gate_start_line() {
  echo "load:   ${LOAD_GATE_LOAD_1}/${LOAD_GATE_CPU_COUNT} logical CPUs at start (max per CPU: ${BENCH_MAX_LOAD_PER_CPU})"
}

# Watch the load for as long as the measured work runs.
#
# Policy for a run whose load exceeds the threshold mid-flight: do not abort it
# (a long suite can run for many minutes, and discarding it wastes more than
# the contamination costs); finish it, but mark the result file CONTAMINATED,
# loudly, in a form `bench/summarise.py` also surfaces when it combines runs —
# the flag has to survive being combined with clean runs, not get silently
# dropped along with the rest of the provenance.
#
# Callers bracket only the *measured* phases with this. `compare.sh` builds
# containers and compiles the workspace before it measures anything, and those
# phases saturate the machine by design; sampling across them would flag every
# run as contaminated and teach the reader to ignore the word.
load_gate_start_sampler() {
  load_gate_disabled && return 0
  [[ -n "$LOAD_GATE_SAMPLER_PID" ]] && return 0

  LOAD_GATE_SAMPLES="$(mktemp "${TMPDIR:-/tmp}/inlaysql-bench-load.XXXXXX")"
  echo "$LOAD_GATE_LOAD_1" > "$LOAD_GATE_SAMPLES"
  (
    flagged=0
    while sleep "$BENCH_LOAD_SAMPLE_SECONDS"; do
      sample="$(load_gate_read_1 2>/dev/null || true)"
      [[ "$sample" =~ ^[0-9]+([.][0-9]+)?$ ]] || continue
      echo "$sample" >> "$LOAD_GATE_SAMPLES"
      if [[ "$flagged" -eq 0 ]] && awk -v l="$sample" -v c="$LOAD_GATE_CPU_COUNT" -v m="$BENCH_MAX_LOAD_PER_CPU" \
        'BEGIN { exit !(l / c > m) }'; then
        flagged=1
        echo "*** load spike detected mid-run (${sample}/${LOAD_GATE_CPU_COUNT} exceeds BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU} at $(date -u +%H:%M:%SZ)) — letting the run finish rather than aborting it, but the result will be marked CONTAMINATED ***" >&2
      fi
    done
  ) &
  LOAD_GATE_SAMPLER_PID=$!
}

load_gate_stop_sampler() {
  if [[ -n "$LOAD_GATE_SAMPLER_PID" ]]; then
    kill "$LOAD_GATE_SAMPLER_PID" >/dev/null 2>&1 || true
    wait "$LOAD_GATE_SAMPLER_PID" 2>/dev/null || true
    LOAD_GATE_SAMPLER_PID=""
  fi
}

# Fold start + in-flight + end samples into the min/median/max the result file
# publishes, and shout if any of them cleared the threshold. Prints nothing
# when the gate is off or the sampler never ran, so a caller can pipe this
# straight into its result file unconditionally.
load_gate_summary() {
  [[ -n "$LOAD_GATE_SAMPLES" && -s "$LOAD_GATE_SAMPLES" ]] || return 0

  load_gate_read_1 >> "$LOAD_GATE_SAMPLES" 2>/dev/null || true

  local sorted count min max median exceeded banner
  sorted="$(sort -n "$LOAD_GATE_SAMPLES")"
  count="$(printf '%s\n' "$sorted" | grep -c .)"
  min="$(printf '%s\n' "$sorted" | head -1)"
  max="$(printf '%s\n' "$sorted" | tail -1)"
  median="$(printf '%s\n' "$sorted" | awk -v n="$count" '{a[NR]=$1} END { if (n % 2 == 1) print a[(n+1)/2]; else print (a[n/2]+a[n/2+1])/2 }')"
  exceeded="no"
  if awk -v m="$max" -v c="$LOAD_GATE_CPU_COUNT" -v t="$BENCH_MAX_LOAD_PER_CPU" 'BEGIN { exit !(m / c > t) }'; then
    exceeded="yes"
  fi

  echo
  echo "load:   sampled ${count} times every ${BENCH_LOAD_SAMPLE_SECONDS}s throughout the measured phases — min ${min}/${LOAD_GATE_CPU_COUNT} median ${median}/${LOAD_GATE_CPU_COUNT} max ${max}/${LOAD_GATE_CPU_COUNT} (threshold ${BENCH_MAX_LOAD_PER_CPU} per CPU)"
  if [[ "$exceeded" == "yes" ]]; then
    banner="load:   *** CONTAMINATED: load exceeded BENCH_MAX_LOAD_PER_CPU=${BENCH_MAX_LOAD_PER_CPU} during this run (max ${max}/${LOAD_GATE_CPU_COUNT}) — do not treat these numbers as clean; see PERF.md §4 ***"
    echo "$banner"
    echo "$banner" >&2
  fi
}

# For the caller's own EXIT trap.
#
# An `if`, not a `[[ ]] &&` compound: with `set -e` a failing EXIT trap
# replaces the script's own exit status, and this runs on every
# `BENCH_MAX_LOAD_PER_CPU=off` run where `$LOAD_GATE_SAMPLES` is legitimately
# empty — every such run used to exit 1 after publishing its results.
load_gate_cleanup() {
  load_gate_stop_sampler
  if [[ -n "$LOAD_GATE_SAMPLES" ]]; then
    rm -f "$LOAD_GATE_SAMPLES"
  fi
}
