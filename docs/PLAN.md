# InlaySQL continuation plan

This is the durable, repository-tracked handoff for the next Codex task. The
larger working roadmap is the gitignored root `PLAN.md`; this file contains the
current execution order without relying on local conversation history.

## Repository and invariants

- Engine repository: `https://github.com/inlaySQL/inlaysql`.
- Work directly on `main` for this project; after a completed slice, run the
  relevant gates, commit with a Conventional Commit title, and push `main`.
- Read [`AGENTS.md`](../AGENTS.md) before changing code. In particular,
  `inlaysql-core` stays `no_std` and `forbid(unsafe_code)`, SQLite remains the
  core dialect, unsupported clauses are refused, and benchmark claims must
  regenerate from `bench/run.sh` or `bench/compare.sh`.
- Storage/WAL/recovery/index-format changes require both release DST sweeps;
  planner-only changes still need the ordinary workspace and differential
  gates.

## Current state

- W1 raw row-id and bounded raw-leaf cursor slices are landed; range scans
  remain a measured loss and should wait for stronger profile evidence.
- W3's explicit normal-commit-ready seam, setup-separated concurrency timing,
  focused writer selector and quiet-machine guard are landed. The focused
  rerun was effectively flat against the old baseline, so cohort tuning is a
  no-go for publication until a new pipeline hypothesis exists.
- R4's staged prototype is landed. `ANALYZE` persists exact table row counts
  and leading B-tree-key cardinalities with data/schema version stamps.
  Complete current stats cost only the existing hash and index-probe join
  operators in written order; missing, corrupt or stale stats use the old
  rule-based path. `EXPLAIN` reports the costed choice. Join reordering is not
  implemented.
- The joins harness runs `ANALYZE` for both InlaySQL and SQLite before timing.
  The cost constants are calibrated to InlaySQL's row-at-a-time probe cost;
  the full secondary-index shape should remain on the hash path unless new
  evidence changes that conclusion.

## Next work, in order

1. ~~Run clean, guarded benchmark repeats when the host is quiet~~ — **owed,
   re-deferred 2026-08-31 with a date: retry on or before 2026-09-07, in the
   next quiet window.** Three attempts this date, all refused by the quiet
   machine gate (1-minute load 4-10/18, desktop in active use). A
   `BENCH_MAX_LOAD_PER_CPU=off` same-sitting variant of both commands *was*
   run the same day as disclosed under-load data for the new
   MySQL/PostgreSQL join/range cells (`BENCHMARK.md` "Read shapes and batch
   insert against MySQL and PostgreSQL", `SCOREBOARD.md` §4.0) — but those
   are **not** the clean back-tests this item owes and do not close it. The
   pass criteria are unchanged: clean gate, three runs, spread within the
   suite's floor.

   ```sh
   REPEATS=3 SUITE=joins ROWS=20000 QUERIES=100 LIMIT=20 ./bench/repeat.sh
   REPEATS=3 SUITE=indexed ROWS=100000 QUERIES=100 ./bench/repeat.sh
   ```

   Keep the raw files in `bench/results/`; do not publish a row if the quiet
   machine gate refuses or the repeat spread is too wide. `BENCH_MAX_LOAD_PER_CPU=off`
   is for explicitly labelled diagnostics only.

   Side effect worth knowing before the retry: this session fixed a
   `bench/run.sh` bug where any `BENCH_MAX_LOAD_PER_CPU=off` run exited 1
   *after* publishing its results (an `[[ ]] &&` compound in the EXIT trap's
   cleanup returning 1 under `set -e`), so pre-fix override runs in
   `bench/results/` may show a spurious failure in their caller's log while
   the result files themselves are complete.

2. Compare the post-`ANALYZE` join result and `EXPLAIN` paths with SQLite. If
   the clean data confirms the access-path constants, record the result in
   `docs/research/cost-planner.md` and leave the published baseline unchanged.

3. Decide the next W2 stage-1 slice from evidence: either improve the losing
   outer scan/materialisation path (W1/W2 machinery) or add a narrowly tested
   access-path refinement. Do not add join reordering until output-order,
   `LEFT JOIN`, `ORDER BY`, stale-stat and differential-result proofs exist.

4. Keep W3, R9/server comparison, W4 retrieval validation and S4 scale work
   parallel. Do not let a noisy W3 result block the planner work.

## Acceptance and handoff

Before committing a code slice, run at least:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run --example hybrid_search -p inlaysql
cargo run -p inlaysql --bin sqllogictest -- crates/inlaysql/tests/sqllogictest/*.test
cargo test -p inlaysql --test backends -- --nocapture
cargo test -p inlaysql-mcp --test client
cargo test -p inlaysql-core --test logic_bugs -- --nocapture
```

When the work is finished, verify `git status --short --branch` is clean and
`git rev-list --left-right --count HEAD...origin/main` is `0 0`.

## Cloud continuation prompt

Paste this into a new Codex cloud task after connecting the private GitHub
repository and creating its Rust/Cargo environment:

> Continue the InlaySQL plan from `docs/PLAN.md`. Read `AGENTS.md`, inspect the
> current `main` branch and existing diff before acting. Start with the first
> unchecked item, preserve the listed invariants, run the acceptance checks,
> and commit/push completed work to `main`. Do not repeat completed work or
> publish an unverified benchmark claim. If a benchmark is blocked by the
> quiet-machine guard, leave the evidence and move to the next safe task.

The UI work is a separate private repository,
`https://github.com/inlaySQL/inlaysql-studio`; use its own README and plan when
continuing the spreadsheet-style database viewer.
