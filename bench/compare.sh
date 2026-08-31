#!/usr/bin/env bash
#
# InlaySQL against DuckDB, pgvector and Meilisearch (retrieval), MySQL/
# PostgreSQL (plain OLTP point reads and writes) and, server-to-server,
# InlaySQL's own MySQL wire against MySQL's, on one corpus.
#
#   ./bench/compare.sh                     # defaults
#   DOCS=20000 DIM=384 ./bench/compare.sh  # override any parameter
#   ROWS=100 LOOKUPS=50 ./bench/compare.sh # override the OLTP workload size
#   SERVER_CONCURRENCY_LEVELS=1,4,16 ./bench/compare.sh  # override the server-to-server concurrency levels
#   SERVER_ROWS=500 SERVER_LOOKUPS=200 ./bench/compare.sh # override the server-to-server workload size (defaults well below ROWS/LOOKUPS — see bench/README.md)
#
# What it does, in order:
#
#   1. Generates the retrieval corpus and the OLTP workload once, and
#      measures InlaySQL on both, on the host (`--export` and
#      `--export-oltp`).
#   2. Starts pgvector, Meilisearch, plain PostgreSQL, MySQL and `inlaysql
#      serve --mysql` in containers and runs the DuckDB, pgvector,
#      Meilisearch, PostgreSQL-OLTP and MySQL drivers against those same
#      files.
#   3. Runs the server-to-server driver: the same `mysql.connector` client
#      library against both the MySQL container and the InlaySQL-server
#      container, at a couple of concurrency levels — the one row where
#      InlaySQL pays a socket round trip too. See "Server-to-server" in
#      bench/README.md.
#   4. Measures InlaySQL a *second* time, inside a container, on the same
#      class of Docker volume MySQL and PostgreSQL write to — so the OLTP
#      write column compares like fsync semantics against like instead of a
#      host barrier against a virtualised one. See bench/README.md.
#   5. Prints one report — a retrieval table, an OLTP table with both InlaySQL
#      rows, and a server-to-server table — and writes it to bench/results/.
#
# Everything except step 1 needs Docker. The engines are not linked into the
# harness — DuckDB is a separate runtime, and pgvector/Meilisearch/PostgreSQL/
# MySQL need a server — so a container is what makes their numbers reproducible instead of
# dependent on whatever happens to be installed on the machine. Steps 3 and 4
# reuse docker/Dockerfile to build this same workspace on Linux, the way
# docker/test.sh does, rather than a second image.
#
# The OLTP row's durability is matched to InlaySQL's own commit-per-statement
# write, not switched off for speed the way the pgvector container is for
# query-latency measurement — see bench/README.md for exactly what each
# engine is configured with and why.

set -euo pipefail

DOCS=${DOCS:-5000}
QUERIES=${QUERIES:-100}
SEED=${SEED:-42}
DIM=${DIM:-128}
LIMIT=${LIMIT:-10}
ROWS=${ROWS:-20000}
LOOKUPS=${LOOKUPS:-5000}
PAYLOAD=${PAYLOAD:-64}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORPUS_DIR=${CORPUS_DIR:-"$ROOT/target/bench-corpus"}
RESULTS="$ROOT/bench/results"
COMPOSE=("docker" "compose" "-f" "$ROOT/bench/external/compose.yml")

# The same quiet-machine gate `bench/run.sh` uses, on the script that produces
# every MySQL, PostgreSQL, pgvector and DuckDB row we publish. Its absence here
# is why PERF.md, bench/README.md and SCOREBOARD.md all record that no
# `compare.sh` figure can earn a WIN/LOSS verdict — a comparison taken while
# something else owned the CPU measures the machine, and this script's numbers
# decide who is faster than whom.
#
# Note the phase boundaries below: this script compiles the workspace and
# builds container images before it measures anything, and those phases
# saturate the machine on purpose. The sampler starts after them, so
# CONTAMINATED means the *measurement* was disturbed rather than "compare.sh
# built something", which is the only reading of the word that stays useful.
# shellcheck source=bench/load_gate.sh
. "$ROOT/bench/load_gate.sh"

load_gate_preflight

if ! docker info >/dev/null 2>&1; then
  echo "docker is not running: this comparison needs it. The suites in" >&2
  echo "./bench/run.sh do not, and cover InlaySQL vs SQLite and sqlite-vec." >&2
  exit 1
fi

mkdir -p "$RESULTS"
rm -rf "$CORPUS_DIR"

echo "==> generating the corpus [retrieval + OLTP] and measuring InlaySQL"
cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" -p inlaysql-bench -- \
  --export "$CORPUS_DIR" --export-oltp "$CORPUS_DIR" \
  --docs "$DOCS" --queries "$QUERIES" --seed "$SEED" --dim "$DIM" --limit "$LIMIT" \
  --rows "$ROWS" --lookups "$LOOKUPS" --payload "$PAYLOAD"

# The drivers run inside the compose network so the service hostnames
# resolve. `down` first, in case a previous run left a container holding an
# old corpus. `--profile oltp-container` matters here: without it, `down`
# does not even know the profiled `inlaysql-oltp` service exists, so a
# container it left behind (this script failing between `run` starting it and
# `--rm` removing it) would survive an otherwise-clean `down`.
cleanup() {
  load_gate_cleanup
  CORPUS_DIR="$CORPUS_DIR" "${COMPOSE[@]}" --profile oltp-container down --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT
export CORPUS_DIR

# Namespaces the containerised InlaySQL build's target-directory volume by
# checkout path, the same reason docker/test.sh hashes its own: several
# worktrees running compare.sh at once would otherwise share one `cargo`
# lock and rebuild over each other's artifacts, since the sources differ.
export INLAYSQL_OLTP_TARGET_VOLUME="inlaysql-bench-target-$(printf '%s' "$ROOT" | shasum | cut -c1-12)"
# Same namespacing, for inlaysql-server's own target volume — see
# bench/external/compose.yml for why it is not the same volume as the one
# above.
export INLAYSQL_SERVER_TARGET_VOLUME="inlaysql-bench-server-target-$(printf '%s' "$ROOT" | shasum | cut -c1-12)"

echo "==> starting containers"
"${COMPOSE[@]}" up -d --build --wait

# Built here rather than at its `run` below, which sits between two measured
# phases: a container build is minutes of saturated CPU, and building it
# mid-measurement would put a compile inside the sampled window and flag every
# run CONTAMINATED by its own doing.
echo "==> building the containerised InlaySQL OLTP image"
"${COMPOSE[@]}" --profile oltp-container build inlaysql-oltp

# Everything above this line is setup — compiles and image builds, saturating
# by design. Everything below is measurement, and is watched.
load_gate_start_sampler

echo "==> DuckDB"
"${COMPOSE[@]}" exec -T drivers python duckdb_driver.py

echo "==> pgvector"
"${COMPOSE[@]}" exec -T drivers python pgvector_driver.py

echo "==> Meilisearch"
"${COMPOSE[@]}" exec -T drivers python meilisearch_driver.py

echo "==> PostgreSQL (OLTP, matched durability)"
"${COMPOSE[@]}" exec -T drivers python postgres_oltp_driver.py

echo "==> MySQL (OLTP, matched durability)"
"${COMPOSE[@]}" exec -T drivers python mysql_driver.py

echo "==> InlaySQL vs MySQL, server-to-server (MySQL wire, matched durability)"
"${COMPOSE[@]}" exec -T drivers python server_driver.py

echo "==> InlaySQL, containerised (same volume class as MySQL/PostgreSQL)"
# No `--build`: the image was built during setup, before the sampler started.
"${COMPOSE[@]}" --profile oltp-container run --rm -T inlaysql-oltp

load_gate_stop_sampler

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT="$RESULTS/$STAMP-compare.txt"
{
  echo "date:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
  echo "dirty:  $(git -C "$ROOT" status --porcelain 2>/dev/null | head -c 1 | grep -q . && echo yes || echo no)"
  echo "rustc:  $(rustc --version)"
  echo "host:   $(uname -srm)"
  echo "docker: $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo unknown)"
  load_gate_start_line
  echo
  "${COMPOSE[@]}" exec -T drivers python report.py
  load_gate_summary
} | tee "$OUTPUT"

echo "written to ${OUTPUT#"$ROOT"/}"
