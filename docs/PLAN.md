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

## Current state (2026-09-02)

- Cost-based join *reorder* landed as AHL-512: a two-table INNER join may run
  with its sources exchanged when the cost model scores that cheaper, as a
  plan rewrite with every ordinal remapped. Measured 1.31x on the joins
  suite, interleaved. Bounded to full scans; `LIMIT` shapes refuse by design
  (a different order is a different result set without `ORDER BY`).
- Aggregate streaming landed as AHL-513/514/515: `GROUP BY` folds as rows
  arrive through an ordered map, holding one representative row and one
  accumulator set per group; ungrouped aggregates fold from the stream.
- The raw-leaf cache and the collation-/`REAL`-keyed hash joins landed
  earlier (see `PERF.md` 2026-08-31/09-01 sections).
- **Every `BENCHMARK.md` table except joins predates all of the above**
  (`2cb2539`, 2026-08-30). The published PK-inner-join and aggregate losses
  may already be smaller than printed; nothing is claimed until regenerated.
- A 2026-09-02 three-path code audit (root plan A4/A5/B4a/C7) attributed the
  remaining read-, aggregate- and insert-path losses to specific line-item
  allocation churn; that is the work queue below.

## Next work, in order

1. **Run clean, guarded benchmark repeats when the host is quiet — still
   owed, still first.** Now carries AHL-512/513/514/515, all unpublished.
   Pass criteria unchanged: clean gate, three runs, spread within the
   suite's floor.

   ```sh
   REPEATS=3 SUITE=joins ROWS=20000 QUERIES=100 LIMIT=20 ./bench/repeat.sh
   REPEATS=3 SUITE=indexed ROWS=100000 QUERIES=100 ./bench/repeat.sh
   ```

   Keep the raw files in `bench/results/`; do not publish a row if the quiet
   machine gate refuses or the repeat spread is too wide.
   `BENCH_MAX_LOAD_PER_CPU=off` is for explicitly labelled diagnostics only.

2. ~~**The allocation diet**~~ — **landed 2026-09-02** as eight commits
   (`84e62a5..aa42cd5`, AHL-517–520 plus four read-path commits), all gates
   green including both release DST sweeps on the index-path change.
   Corrections the build produced: the `PartialEq<Value> for ValueRef`
   allocation was unreachable from the executor (landed as a cheaper public
   impl, but it is not a read-path finding), and `moving_projection` was
   already wired into `project_stream`. Wall-clock for every item is
   **unmeasured and owed to item 1's quiet window** — no published number
   changes until then. Left open, recorded in the root plan: `UPDATE`'s
   per-row `encode_table_row`, the collecting aggregate path's per-row key,
   and the miss path's second map descent (per group, accepted).

3. **Join-reorder remainder**: the `LIMIT`-shape output-order argument, then
   index-probe access paths. The published `LIMIT`-join rows never benefit
   from AHL-512 until this exists.

4. **`RangeCursor` extension** to `walk`/`scan_range_from` (WITHOUT ROWID
   scans, `UNIQUE` collision check) — bounded, read-path-only.

5. **Batch executor (B4)** — owns the remaining aggregate floor (scan+decode,
   10.28 ms of the 18.38 ms shape). R3 brief first; point read must not move.

6. **Server posture F3/F4** — refuse-to-expose defaults; fuzz the packet path.

7. **Insert structural half (C7)**: dirty pages held as decoded `Node`s
   across a transaction, encoded once at commit (today every row re-decodes,
   deep-clones and re-encodes every page on its path — `tree.rs:2597`).
   Behind the `Storage` seam but changes what `rebase_pending` walks: both
   release DST sweeps mandatory.

8. **C1 commit-side logical group commit** — highest payoff, data-loss risk,
   full DST rigor; only when it can be done carefully.

9. **Serverless R13 brief → object-store prototype** — runs in parallel.

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
