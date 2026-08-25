#!/usr/bin/env bash
#
# Run the workspace's checks inside a container, on Linux, with the same
# toolchain CI installs — so a green run here means what a green CI run means,
# and nothing is written into the host's `target/` or its cargo cache.
#
#   ./docker/test.sh              # every gating job in ci.yml (check + fuzz + determinism)
#   ./docker/test.sh check        # just ci.yml's `check` job
#   ./docker/test.sh fuzz         # just ci.yml's `fuzz-targets` job
#   ./docker/test.sh determinism  # just ci.yml's `determinism` job
#   ./docker/test.sh sweep        # ci.yml's `sweep` job (minutes)
#   ./docker/test.sh all          # the gate plus the sweeps
#   ./docker/test.sh shell        # an interactive shell in the same image
#   ./docker/test.sh -- <cmd...>  # an arbitrary command in the same image
#
# Any mode takes `--toolchain X` first, e.g. `./docker/test.sh --toolchain
# 1.91.1 check`, to reproduce a run against a specific Rust rather than the
# `stable` CI follows.
#
# The compiled artifacts and the cargo registry live in named volumes, so the
# first run is slow and every run after it is not. The target volume is
# namespaced per checkout *and per toolchain*, so several worktrees can build
# at once without fighting over one target directory and a toolchain switch
# does not force a full rebuild of the other one; `docker volume ls | grep
# inlaysql` finds them and `docker volume rm` starts clean.
#
# Why this covers more than the fast command list: `ci.yml` gates a merge on
# four jobs, not one. `check` is the loud one, but `fuzz-targets` (a nightly
# build of `fuzz/`) and `determinism` (core stays `no_std` with no OS-facing
# dependency) fail merges just as hard, and neither was reachable from here
# until AHL-482. `sweep` is separate because CI itself only runs it on `main`,
# on a tag, or on a PR labelled `full-ci`.

set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

toolchain=stable
if [ "${1:-}" = "--toolchain" ]; then
    if [ -z "${2:-}" ]; then
        echo "--toolchain needs a value, e.g. --toolchain 1.91.1" >&2
        exit 2
    fi
    toolchain="$2"
    shift 2
fi

image="inlaysql-test:$toolchain"
# `stable` is the tag CI follows, and the `rust` image spells that as plain
# `rust:bookworm`; a version reaches it as `rust:1.91-bookworm`.
if [ "$toolchain" = stable ]; then
    rust_image="rust:bookworm"
else
    rust_image="rust:${toolchain}-bookworm"
fi

# Volumes are namespaced by the checkout they belong to. Several worktrees of
# this repo are often being built at once — one per agent — and a single shared
# target directory would make them fight: cargo takes a lock on it, so the runs
# serialise, and worse, they would rebuild over each other's artifacts because
# the sources differ. A hash of the path keeps the name stable per checkout and
# short enough to be a legal volume name. The toolchain joins the hash because
# artifacts built by two rustcs cannot share a directory.
slug="$(printf '%s@%s' "$repo" "$toolchain" | shasum | cut -c1-12)"
target_volume="inlaysql-target-$slug"
# The registry is a download cache, identical for every checkout and every
# toolchain, so this one is deliberately shared: it is append-mostly and cargo
# handles concurrent readers of it.
cargo_volume=inlaysql-cargo

if ! docker info >/dev/null 2>&1; then
    echo "docker does not appear to be running; start it and try again" >&2
    exit 1
fi

echo "==> building $image (cached after the first run)"
docker build -q -t "$image" \
    --build-arg "RUST_IMAGE=$rust_image" \
    "$repo/docker" >/dev/null

run() {
    # `--init` so Ctrl-C reaches the compiler rather than being swallowed by
    # PID 1. The source is mounted read-write because `cargo` writes lockfiles
    # and the benchmark writes results, but `CARGO_TARGET_DIR` in the image
    # keeps build output off the host.
    docker run --rm --init \
        -v "$repo:/workspace" \
        -v "$target_volume:/target" \
        -v "$cargo_volume:/usr/local/cargo/registry" \
        -w /workspace \
        "$@"
}

# `.github/workflows/ci.yml`'s `check` job, in its order. Kept in step with
# AGENTS.md's "Commands that match CI" by hand — if you change one, change all
# three.
check_job='
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    cargo run --example hybrid_search -p inlaysql
    cargo run -p inlaysql --bin sqllogictest -- crates/inlaysql/tests/sqllogictest/*.test
    cargo test -p inlaysql --test backends -- --nocapture
    cargo test -p inlaysql-mcp --test client
    cargo test -p inlaysql-core --test logic_bugs -- --nocapture
'

# `ci.yml`'s `fuzz-targets` job. The fuzzers themselves run in `trust.yml`;
# what this catches is a target that stopped compiling, which is a fuzzing gap
# nobody notices until the next campaign. `fuzz/` is excluded from the
# workspace and has its own lockfile, hence the `--manifest-path`.
fuzz_job='
    cargo +nightly check --manifest-path fuzz/Cargo.toml --all-targets
'

# `ci.yml`'s `determinism` job, character for character — the grep and the
# forbidden-dependency list are the actual gate, not a paraphrase of it.
determinism_job='
    grep -q "^#!\[no_std\]" crates/inlaysql-core/src/lib.rs
    deps="$(cargo tree -p inlaysql-core --edges normal --prefix none | awk "{print \$1}" | sort -u)"
    echo "$deps"
    for forbidden in libc redb tantivy instant-distance mio tokio socket2 io-uring rusqlite libsqlite3-sys; do
        if echo "$deps" | grep -qx "$forbidden"; then
            echo "inlaysql-core must not depend on $forbidden" >&2
            exit 1
        fi
    done
'

# `ci.yml`'s `sweep` job: thousands of randomised crash/torn-write schedules.
#
# All four steps, in the same order CI runs them. The page-reuse sweep is the
# one that recycles page ids — the other two answer "unknown, so never reclaim"
# to the free list and so never exercise reuse at all — which makes it the only
# place AHL-406's failure mode is under fault injection. Leaving it out here
# meant this script did not reproduce the job it claims to; the backup sweep is
# here from the day it was written for the same reason.
sweep_job='
    cargo test --release -p inlaysql-core --test dst_sweep -- --ignored
    cargo test --release -p inlaysql --test index_recovery_dst -- --ignored
    cargo test --release -p inlaysql-core --test free_list_reuse_dst -- --ignored
    cargo test --release -p inlaysql-core --test backup_dst -- --ignored
'

mode="${1:-gate}"

case "$mode" in
shell)
    run -it "$image" bash
    ;;
--)
    shift
    run "$image" "$@"
    ;;
check)
    run "$image" bash -euxc "$check_job"
    ;;
fuzz)
    run "$image" bash -euxc "$fuzz_job"
    ;;
determinism)
    run "$image" bash -euxc "$determinism_job"
    ;;
sweep)
    run "$image" bash -euxc "$sweep_job"
    ;;
gate)
    # Every job `ci.yml` gates a merge on, in the order that fails fastest:
    # determinism is seconds, the fuzz targets are a build, `check` is the
    # long one. The sweep is not here because CI does not run it on an
    # ordinary pull request either — use `sweep` or `all` for that.
    run "$image" bash -euxc "$determinism_job$fuzz_job$check_job"
    ;;
all)
    run "$image" bash -euxc "$determinism_job$fuzz_job$check_job$sweep_job"
    ;;
*)
    echo "unknown mode: $mode" >&2
    echo "expected gate, check, fuzz, determinism, sweep, all, shell, or --" >&2
    exit 2
    ;;
esac
