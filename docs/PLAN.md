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

- 2026-09-03, 12:00–16:45: AHL-547 (C1 slice 2 — one WAL append and one
  sync per cohort, acknowledged after the barrier; DST-clean; **measured
  0.78–0.90x at 8–32 writers** because commit-side cohorts displace the
  flush-side ticket gather; flag stays off) and AHL-548 (`COUNT(*)` from
  leaf cell counts; the published scalar aggregate went 225 → 1,914/s,
  ~6x MySQL 8.4, ~5x PG 17).
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
- 2026-09-03, 10:00–11:00: AHL-545 (split point linear, cells written
  straight into the page, `UPDATE` encoder hoisted — landed as algorithmic
  fixes, no number claimed: the batch-insert statement is 89% fsync),
  AHL-546 (`MIN`/`MAX` on rowid/indexed column in one descent; the
  published scalar shape still scans for `COUNT(*)`), and the batch-insert
  row measured like for like in a container (67,484 rows/s: ~1.2x MySQL
  8.4, 0.68x PG 17).
- 2026-09-03, 01:00–02:30: AHL-541 (leaf format change rejected by
  reading — the slot directory already exists; a shared inlined cell parser
  landed: +4–9% on every read shape), AHL-542 (**C7 landed**: pages stay
  decoded for the life of a transaction, encoded once at commit; batch
  insert 1.29–1.44x; five DST sweeps green), AHL-543 (C1 design brief,
  `docs/research/commit-group-logical.md`; first slice is rebase-only
  absorption), AHL-544 (**C1 slice 1 landed behind an off-by-default
  flag**: the gate holder judges the writers parked behind it. Cohorts
  form — 5.4–8.9 members, 81–95% of commits absorbed at 8–32 writers, so
  the brief's premise holds — and it measures flat, which the slice's own
  plan predicted before the run, because every follower still enters the
  gate to encode and append. `EngineOptions::commit_absorption` stays off;
  the decision ordering, the chain seal and the crash sweeps are what
  Slice 3 needed proving first. `PERF.md` AHL-544).
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

Done items are removed, not struck through; `PERF.md`'s dated sections and
`git log` hold them. Every item below lands only with an interleaved A/B
(3 reps, control re-run each rep, 3/3 non-overlapping) and touches no
published row without a gated regeneration.

1. **`LIMIT 10` joins vs SQLite (1.27x / 1.59x slower).** Ten PK probes
   are ten descents; the inner row is decoded through the owned path and
   its column copied; each outer row's matches are collected into a fresh
   `Vec`. Angles: the inner side borrows (AHL-535's buffer applied to the
   join's probed row), the match buffer is reused across outer rows, and a
   probe that reseeks from the retained cursor's parent on a leaf miss.
2. **Range scan vs SQLite (0.67x).** After AHL-550: the residual filter
   is compiled per execution and is ~7% of the shape (1.22–1.36x on
   `indexed-range`, `PERF.md` 2026-09-03); reading the filter-only `TEXT`
   column raw to skip its `from_utf8` was built and measured flat — the
   removable share is ~2 points, the returned column's validation is the
   `&str` API's, and a second row walk costs more than it saves (same
   section; do not re-propose without fusing it into the decoder, and
   even then it is under §4's floor). What is left is the descent:
   `memcmp` ~24–27% is B-tree key comparison, `get_from` ~5%. Angle: a
   borrowed-entry index walk. Do not re-propose a dense walk or a
   covering scan (both measured negative) without that first.
3. **Point-read ops/s vs SQLite WAL (0.69x; p50 already ahead).** The gap
   is the tail (p95 4.67 vs 1.04 µs). First an instrument — a tail
   profiler that records stacks only for queries over 2 µs — then the fix
   it names (clock sweeps, `RawLeafCache`'s shift, table growth, allocator
   `madvise`, the shared cache's lock are the candidates).
4. **Batch insert vs PostgreSQL like for like (0.68x).** ~1.0 ms of
   engine work per hundred-row statement in the container vs PG's ~0.6 ms;
   the page path is tiny now. Profile *in the container*: if the state
   block's write + `sync` is a second barrier per commit, rewrite it lazily
   at checkpoint (the record already carries root/next/seq — recovery's
   chain must prove it); otherwise coalesce record + dirty pages into one
   `pwritev`.
5. **Server-to-server writes at 8 connections (0.30x MySQL 8.4; commits
   per fsync at parity, barrier rate 375/s vs 1,280/s).** The C2
   diagnostic first — socket-wait vs gate-wait vs commit-wait per
   connection thread — then pipelining the next gate holder's prepare with
   the current cohort's barrier; item 4's second barrier, if real, halves
   the rate on its own.

Still open, not a published loss: B2's index-probe reorder, A3's WITHOUT
ROWID cursor, B4's kernel copy (architecture: recycled `Arc` pool or an
`mmap` device), C1 (closed as a measured loss until commit-side cohorts
and the flush-side gather compose; slices 1–2 stay in behind the
default-off flag), the live row count (needs a delta-merge metadata rule),
F3/F4 server posture, the serverless brief.

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
