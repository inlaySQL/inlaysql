# Contributing

Thanks for looking at the inside of this engine. A few things about how this
repository works, so a contribution lands well the first time.

## The short version

- **Bugs:** open an issue. Reproduction steps beat stack traces; a
  `sqllogictest`-shaped minimal case beats both. We credit reporters as
  co-author on the fix commit.
- **Feature ideas:** open an issue describing the *problem* before the
  design. A lot of what looks missing here is a deliberate sequence (see
  [`PLAN.md`](PLAN.md) and the [Next](README.md#next) section) — an idea may
  already be queued, refused for a written reason, or one message away from
  being queued.
- **Security:** never in a public issue — see [`SECURITY.md`](SECURITY.md).

## The rules of the road

These are not bureaucracy; each one exists because breaking it has cost this
repository real time. They are enforced by CI, and a PR that trips one is
wrong even when it is green.

1. **The engine's dialect is SQLite's.** `VECTOR(n)` and the retrieval
   functions (`vector_score`, `bm25_score`, `fuse`) are the addition on top;
   full Postgres parity is an explicit non-goal. MySQL compatibility lives in
   the wire-protocol shim (`crates/inlaysql-server`), never in the core — if
   a MySQL feature seems to need a core change, that is the signal to check
   whether it is really a *SQLite* feature the core is missing.
2. **A clause this project cannot honour is refused, never accepted and
   ignored.** This is the bug class that has cost the most: statements that
   parsed, silently discarded a clause, and reported success while doing
   something else. Prefer a loud `Error::Unsupported` with a test pinning the
   refusal.
3. **A number nobody can reproduce is worse than no number.** Every published
   benchmark figure regenerates from a script in `bench/` (`run.sh`,
   `repeat.sh`, `compare.sh`). Do not add a performance claim to the README
   or `BENCHMARK.md` that is not backed by one, and do not quote a figure to
   more digits than the run-to-run spread supports. Runner-generated trend
   numbers live in `RUNNER-BENCHMARK.md` and never in `BENCHMARK.md`.
4. **A change to the storage engine, WAL, recovery path or an index format
   needs a DST pass**, not just `cargo test`: thousands of seeded
   crash/torn-write schedules that must replay byte-for-byte.
   `./docker/test.sh sweep` runs them.
5. **`inlaysql-core` is `no_std` and `#![forbid(unsafe_code)]`**, and stays
   that way. Every effect arrives through traits; `unsafe` is confined to
   `inlaysql-uring` behind the `Device` seam.

## What a PR must pass

The same four jobs CI gates a merge on — run them before opening the PR, not
after:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
cargo test --workspace
python3 bench/test_summarise.py
cargo run --example hybrid_search -p inlaysql
cargo run -p inlaysql --bin sqllogictest -- crates/inlaysql/tests/sqllogictest/*.test
cargo test -p inlaysql --test backends -- --nocapture
cargo test -p inlaysql-mcp --test client
cargo test -p inlaysql-core --test logic_bugs -- --nocapture
```

Or all of it, on Linux, in containers:
[`./docker/test.sh`](docker/test.sh). If your change touches `btree`, `wal`,
`sim`, `hnsw`, `hnsw_paged` or `bm25`, also run `./docker/test.sh sweep`.

## Commit messages and PRs

Conventional Commits titles linking the driving issue key, e.g.
`feat(core): incremental HNSW maintenance instead of rebuilding (AHL-381)` —
it is the convention every commit on `main` already follows. One logical
change per PR; benchmark regeneration separate from behaviour changes.

## Docs carry the reasoning

The doc comments here are not API reference filler; they hold the argument
for why the code is shaped the way it is, and CI fails the build on a broken
`[`link`]`. If your change makes a doc comment's reasoning stale, updating it
is part of the change, not a follow-up.

## Tests are the specification

[`TESTING.md`](TESTING.md) explains what is covered and, just as important,
what is deliberately not. New behaviour needs a test that fails against the
old code — "closed" means reproduced first, fixed, and pinned. Bug fixes
regressions: the reported input becomes a fixture (see
`crates/inlaysql-core/tests/fuzz_regressions.rs` for the house pattern).
