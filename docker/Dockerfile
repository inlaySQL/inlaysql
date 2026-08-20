# The workspace's test environment, matching CI rather than whatever the
# developer's machine happens to have.
#
# Two reasons this exists beyond tidiness:
#
#   * `inlaysql-bench` builds SQLite and sqlite-vec from bundled C sources, so a
#     workspace test run needs a C toolchain. On macOS that means whichever SDK
#     `xcode-select` currently points at, and a beta SDK breaks the build in a
#     way that has nothing to do with this repo.
#   * CI is Ubuntu. Testing on Linux here means the `determinism`, `backends`
#     and DST jobs are exercised on the platform that actually gates a merge —
#     including `io_uring`, which does not exist on macOS at all.
#
# The build cache and the compiled artifacts live in named volumes, never in
# the host's `target/`, so a Linux build here cannot collide with a macOS build
# there. See `docker/test.sh`.

# `RUST_IMAGE` tracks what `.github/actions/rust` installs, which is `stable`
# — whatever `stable` means on the day the job runs. `rust:bookworm` is the
# latest stable release, so it moves the same way.
#
# This image used to pin `rust:1.91-bookworm`, and that pin was the reason a
# green local run stopped meaning a green CI run. Clippy gains lints with every
# release; on 2026-08-19 a `stable` that had moved to 1.97 failed `-D warnings`
# on a `match` that 1.91's clippy had never objected to, so CI went red on a
# commit whose `docker/test.sh` run had been green. A sandbox that is *more*
# stable than CI does not reproduce CI — it hides exactly the class of failure
# it exists to catch.
#
# So the default floats, on purpose, and the trade is stated: a rebuild can
# start failing on a lint that a previous rebuild did not, which is CI's
# behaviour and therefore the point. Pass `--build-arg
# RUST_IMAGE=rust:1.91-bookworm` (or `./docker/test.sh --toolchain 1.91 ...`)
# to reproduce an older run exactly, e.g. when bisecting whether a failure is
# the code or the toolchain.
ARG RUST_IMAGE=rust:bookworm
FROM ${RUST_IMAGE}

# `cc` and `make` for the bundled SQLite; `pkg-config` because some crates look
# for it even when they do not need it. `--no-install-recommends` keeps the
# image from growing a desktop.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# `rustfmt` and `clippy` for the `check` job. The nightly toolchain is for
# `ci.yml`'s `fuzz-targets` job, which runs `cargo +nightly check` against
# `fuzz/Cargo.toml` — that job is part of the merge gate, so a sandbox that
# cannot run it is a sandbox that cannot answer "will CI pass".
RUN rustup component add rustfmt clippy \
    && rustup toolchain install nightly --profile minimal

# Out of the mounted source tree on purpose: the host's `target/` stays a macOS
# build directory and this stays a Linux one.
ENV CARGO_TARGET_DIR=/target

# CI sets this workflow-wide in `ci.yml`'s `env:`, so every compilation there —
# not just the clippy step — treats a warning as an error. Without it here, a
# `cargo test --workspace` could pass in this container and fail on the runner.
ENV RUSTFLAGS="-D warnings"

WORKDIR /workspace
