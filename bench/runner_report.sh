#!/usr/bin/env bash
#
# Assemble the runner benchmark report.
#
#   ./bench/runner_report.sh out.md
#
# Takes the result files the benchmark workflows produce — `bench/repeat.sh`'s
# `*-repeat.txt` medians and `bench/compare.sh`'s `-compare.txt` — and wraps
# them in one markdown file with the disclosure those numbers need: a GitHub
# runner is a shared, 4-vCPU machine, so these figures are for trend tracking
# against previous runner runs, never a replacement for the quiet-machine
# numbers in BENCHMARK.md. The header says so, because a number nobody knows
# the provenance of is worse than no number.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:?usage: runner_report.sh <output.md> <section-file...>}"
shift  # the remaining arguments are section file names, not the output

RESULTS="$ROOT/bench/results"
STAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not a checkout')"

{
  echo "# Runner benchmarks — trend tracking, not published figures"
  echo
  echo "Machine: a GitHub-hosted \`ubuntu-latest\` runner (4 shared vCPUs, Docker"
  echo "for the container rows). These numbers are **not** the published"
  echo "benchmarks — those come from load-gated runs on a quiet machine, per"
  echo "\`BENCHMARK.md\` and \`PERF.md\` §4's A/A floor, which a shared runner"
  echo "cannot meet. What these runs are for: catching regressions between"
  echo "runner generations and across commits, on one consistent (if modest)"
  echo "machine class, for free."
  echo
  echo "Read two of these against each other, never one of them against"
  echo "\`BENCHMARK.md\`. A run-to-run swing under 20% on this machine class is"
  echo "noise, not signal."
  echo
  echo "- generated: $STAMP"
  echo "- commit: $COMMIT"
  echo "- workflow: .github/workflows/benchmark.yml (schedule + manual)"
  echo
  for section in "$@"; do
    file="$RESULTS/$section"
    if [ -f "$file" ]; then
      echo "## $section"
      echo
      echo '```'
      cat "$file"
      echo '```'
      echo
    else
      echo "## $section"
      echo
      echo "_missing: $file (that stage failed — see the workflow logs)_"
      echo
    fi
  done
} > "$OUT"

echo "wrote $OUT"
