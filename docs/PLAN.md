# InlaySQL continuation plan

This is the durable, repository-tracked handoff for the next Codex task. The
larger working roadmap is the gitignored root `PLAN.md`; this file carries the
current state and execution order without relying on local conversation
history. Both files are open queues, not archives: landed work is deleted from
them and lives in `PERF.md`'s dated sections and `git log`.

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

## Current state (2026-09-04)

- **Published figures.** `BENCHMARK.md`'s current edition is the `run.sh`
  suites at `1f7921a` (2026-09-03, gated median of three) and every
  `compare.sh`- and driver-sourced table at `bdc64eb` (regenerated under the
  load gate and repeat wrapper for the first time on the night of
  2026-09-02/03). No table is carried forward from 2026-08-30 except the
  eleven-level concurrency sweep with its 32-writer tail row, and the
  quantisation spot-check at scale; both say so in place.
- **Where we win:** warm in-process reads ~67x MySQL 8.4 / ~12x PostgreSQL 17;
  point reads ~3.9x durable SQLite; both full-join shapes ~3x and ~7-8x SQLite
  and ~4x / ~2.6-2.9x the servers; range scan ~8x / ~5.5x the servers;
  `GROUP BY` 1.9x / 1.26x and the scalar aggregate ~6x / ~5x the servers (both
  were the worst multiples in the matrix a week ago); durable single-row writes
  ~2.5x SQLite and ~13x at eight concurrent writers; batch insert ~1.2x MySQL
  8.4 like for like; bulk load 227k rows/s; ~9x `sqlite-vec`, ~60-70x
  DuckDB/pgvector on hybrid retrieval.
- **Where we lose, and these five are the queue below:** `LIMIT 10` joins vs
  SQLite (1.1x / 1.3-1.5x slower), range scan vs SQLite (0.83x), point-read
  ops/s vs SQLite WAL (0.56x, while ahead on p50), batch insert vs PostgreSQL
  like for like (0.68x), and server-to-server writes at eight connections
  (0.30x). Also open and published: p99 commit latency at 32 writers, and
  sequential durable writes against both containerised servers.
- **Landed and not yet published** — do not quote these as published numbers;
  the next gated regeneration decides them: AHL-551 (a point lookup resumes
  from the ancestor it descended through), AHL-552 (the point read's tail was a
  decoded cache full of superseded pages; evictions over the write phase
  151,635 -> 0, p95/p99 at SQLite WAL's level in-process) and AHL-553 (the
  commit barrier stops paying to grow the file; containerised durable write
  ~1.18x).
- **Closed by measurement, not open work:** C1's commit-side absorption is
  built through slice 2, DST-clean, and measures 0.78-0.90x at 8-32 writers
  because commit-side cohorts displace the flush side's larger ones —
  `EngineOptions::commit_absorption` stays off. AHL-554's row-id-shaped
  comparator was reverted (self-time fell, the clock did not move). The full
  list of measured dead ends is root `PLAN.md` section 9.
- **The write-side picture, measured:** one barrier per commit, confirmed; the
  WAL wrap costs one extra barrier per 51.7 single-row commits (33.3 on the
  hundred-row shape); the first containerised commit split puts the barrier at
  89.8% and the engine above storage at 5.8%. On the server, at eight
  connections 69.8% of a connection thread's time is waiting behind other
  writers and 8.0% is the barrier — the single-writer gate is the wall
  (AHL-555).

## Next work, in order

Every item lands only with an interleaved A/B (3 reps, control re-run each rep,
3/3 non-overlapping) and touches no published row without a gated
regeneration.

1. **`LIMIT 10` joins vs SQLite (1.1x / 1.3-1.5x slower).** AHL-549 and
   AHL-551 are done; what is left is the descent itself — `get_from` is 35.5%
   of the shape and dominates. Gate: `joins-limit` 3/3, `joins` and `points`
   flat.
2. **Range scan vs SQLite (0.83x; the published cell already contains
   AHL-550's compiled filter, which moved it 97,624 -> 119,219 ops/s).** What
   is left is the descent: `memcmp` 24-27% is B-tree key comparison,
   `get_from` ~5%. Angle: a borrowed-entry index walk. Do not re-propose a
   dense rowid walk, a covering-index scan, or reading the filter-only `TEXT`
   column raw — all three were built and measured negative or flat.
3. **Point-read ops/s vs SQLite WAL (0.56x).** The tail that explained it is
   fixed (AHL-552). This row needs the next gated regeneration, not a fix,
   before anything else is proposed for it.
4. **Batch insert vs PostgreSQL like for like (0.68x).** ~1.0 ms of engine work
   per hundred-row statement in the container against PG's ~0.6 ms, and the
   host profile is 89% fsync and hides it. **Profile it in the container.** The
   state block is not a second barrier per commit — AHL-553 measured one wrap
   per 33.3 hundred-row commits — so the likely item is coalescing the WAL
   record and the dirty pages into one `pwritev`.
5. **Server-to-server writes at 8 connections (0.30x) — diagnosed (AHL-555),
   not fixed.** Next experiment, named not built: split plan-and-validate from
   take-the-gate-and-commit in `Connection::run_on_engine`, and see whether the
   unaccounted share moves into `gate_wait`/`follower_wait` (already
   overlapping, no gain) or `execute_ns` falls (real gain).

Still open, not a published loss: B2's index-probe reorder, A3's WITHOUT ROWID
cursor, B4's remaining kernel copy (an architecture decision — a recycled `Arc`
pool or an `mmap` device — or nothing), C1 (closed as a measured loss until
commit-side cohorts and the flush-side gather compose; slices 1-2 stay in
behind the default-off flag), an exact live row count (needs a delta-merge
metadata rule; `merge_max_counter` is a max-merge), F3/F4 server posture, and
the R13 serverless brief before any object-store code.

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
