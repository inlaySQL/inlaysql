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

## Current state (2026-09-02, evening)

- **Every `run.sh` suite was regenerated at `7b20175`** (three gated runs,
  none contaminated; `bench/results/20260902T022325Z-repeat.txt`).
  `BENCHMARK.md`, `README.md` and the web copy carry the new figures; the
  sync checker is green. The joins table is withheld: see item 1.
- **The regeneration caught a real regression.** AHL-512's join cost model
  priced an outer row at one unit and drove from the larger table; the
  secondary-index full join went 3.7 ms → 14 ms. Fixed as AHL-524
  (`OUTER_ROW_COST` in `planner.rs`); both written orders now run
  users-driving at ~3.3 ms against SQLite's 10 ms and 30 ms (gate-off single
  run; the gated run is owed). Bisect in `PERF.md`.
- Landed the same day, each measured interleaved on `bin/profile` and
  recorded in `PERF.md`: AHL-521 (page-cache index is a hash, 1.11x on the
  `LIMIT` join), AHL-522 (raw scan reads sixteen pages per syscall, 1.26x on
  `aggregate` 100k, 1.17x full joins), AHL-523 (`GROUP BY` by hash table,
  1.12x), AHL-525 (`ORDER BY` + `LIMIT` joins may reorder; bare `LIMIT`
  still refuses). The point read did not move through any of them.
- **Evening, three parallel agents on the fresh per-shape profiles:**
  AHL-527 (point read stops allocating for its bookkeeping — cursor bounds
  borrowed, lazy statement clock, inline column mask; **points 1.23x, 8/8**),
  AHL-528a/b/c (streamed aggregate folds from the row bytes through one
  reused buffer, whole-leaf admission, bare-column fast path; **aggregate
  1.5x** on top of the morning's 1.44x). A third agent found the range
  shape's row ids were already fetched in rowid order and that a multi-slot
  point cursor measures *negative* on the published shapes — B3 is closed.
- 2026-09-03, 01:00–02:30: AHL-541 (leaf format change rejected by
  reading — the slot directory already exists; a shared inlined cell parser
  landed: +4–9% on every read shape), AHL-542 (**C7 landed**: pages stay
  decoded for the life of a transaction, encoded once at commit; batch
  insert 1.29–1.44x; five DST sweeps green), AHL-543 (C1 design brief,
  `docs/research/commit-group-logical.md`; first slice is rebase-only
  absorption).
- 2026-09-03, small hours: AHL-538 (streamed aggregate by callback off the
  borrowed leaf, walk stops at the last wanted column; aggregate 1.08–1.12x
  at every row width), AHL-539 (in-memory rows shared, 0 allocations per
  in-memory point read; A8 done), AHL-540 (A9 closed by measurement: the
  miss path's remaining copy is the kernel's; PERF.md only).
- Night: AHL-535 (borrowing row API `query_prepared_each_ref`, zero
  allocations per row on the point/range shapes; benches step rows on both
  sides; **points 1.56x, range 1.40x**), AHL-536 (leaf scan borrows the
  device's resident page, `Arc` throughout; aggregate 20k 1.14x), AHL-537
  (B4 brief: the fold is ~1% of the cost, decode/fetch is the floor — B4
  re-scoped to the decode). A merged tree was pushed before it was built
  once (`b55e7de`, fixed `619f5ba`): build before push, always.
- Late evening: AHL-532 (a limited scan's first batch is the limit, not
  32; `joins-limit` 1.2–1.4x). Three ideas were built, measured and dropped
  with the numbers recorded in `PERF.md`/root plan §9a: a per-statement
  join-plan cache (planning is 1.3–1.8% of the query), a dense-rowid leaf
  walk for range scans (the retained cursor already makes sorted ids one
  descent), a covering-index scan (owned index keys cost more than the
  fetch it removes; kept on `ahl-533-covering-index-wip`), and a 64 MiB
  shared read cache (the cost is the page copy, not the syscall). The
  range-scan and point-read remainders now point at one item: **A7, a
  borrowing result API / borrowed-entry index walk.**
- Earlier in the day: the allocation diet (AHL-517–520 plus four read-path
  commits) and aggregate streaming (AHL-513/514/515).
- **Still stale:** every `compare.sh`-sourced table (MySQL/PostgreSQL OLTP
  and aggregate rows, DuckDB/pgvector/Meilisearch, server-to-server) is
  from 2026-08-30. The published 3.4–6x aggregate loss predates everything
  above; the profile's aggregate shape is 1.44x faster since the morning's
  baseline, so it is very likely smaller, and unproven.

## Next work, in order

1. **`repeat-compare.sh` (MySQL 8.4 / PG 17) — queued behind the hold,
   Docker images pulled.** The joins table is current (three
   regenerations on 2026-09-02). Original text: The
   joins table must be replaced from a clean run (two attempts on 2026-09-02
   were `CONTAMINATED` by spikes just over the gate). Then the `compare.sh`
   tables, which have never had the repeat wrapper. Pre-build the bench
   binary before the run so its own compile cannot trip the gate; the quiet
   window on this machine is mid-morning.

   ```sh
   cargo build --release -p inlaysql-bench
   REPEATS=3 SUITE=joins ./bench/repeat.sh
   REPEATS=5 ./bench/repeat-compare.sh
   ```

2. **C1 first slice** (rebase-only absorption under the leader's gate,
   per `docs/research/commit-group-logical.md`), DST-gated. The insert
   path's remaining per-row costs are closed (AHL-545: linear split
   point, in-place encoders, the `UPDATE` hoist — ~1% of a statement
   that is 89% fsync, landed as the algorithmic fixes they are and
   measured flat, `PERF.md`). B4's cell iteration is closed (AHL-541: the
   format already has the offset table).

3. **`RangeCursor` extension (A3)** to `walk`/`scan_range_from` — the
   cheapest first step is `colliding_rows` using `scan_index_row_ids`
   (already cursor-backed, no per-key `Vec`); WITHOUT ROWID scans next.

4. **B2's last piece**: index-probe access paths do not reorder. B3 is
   closed (measured negative). **A7**: a borrowing result API, so the point
   read's answer is not an owned `Vec<Vec<Value>>` per query — 9% of what is
   left on that shape; API change, design first.

5. **Server posture F3/F4** — refuse-to-expose defaults; fuzz the packet path.

6. **Insert structural half (C7)**: dirty pages held as decoded `Node`s
   across a transaction, encoded once at commit. Both release DST sweeps
   mandatory.

7. **C1 commit-side logical group commit** — highest payoff, data-loss risk,
   full DST rigor; only when it can be done carefully.

8. **Serverless R13 brief → object-store prototype** — runs in parallel.

Standing lesson from today, now in the root plan's rules: a suite-level
number is not a per-shape number. A commit that changes a plan decision
quotes every shape it can touch.

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
