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

- 2026-09-05: the fifth `run.sh` edition published (point read p95 6.50 →
  2.17 µs, p99 10.50 → 3.79, AHL-552's tail fix visible at last), and
  **Track F's F3 and F4 landed**. Binding off loopback now refuses under
  four conditions before the socket is bound; `--plaintext-network`
  relaxes only the plaintext pair and refuses itself on a wildcard or a
  routable address; `inlaysql user add` creates the account store without
  serving. The packet path gained four fuzz targets with 79 seeds, and
  reading it first found three defects, all fixed: a wire-reachable
  overflow panic, a 16 MiB pre-authentication allocation from four bytes,
  and a `CLIENT_SSL` shortcut that meant a real MySQL client could never
  have logged in over TLS.
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

1. **`LIMIT 10` joins vs SQLite (1.1x / 1.3-1.5x slower).** AHL-549, AHL-551
   and AHL-559 are done — the last of those took `joins-limit` +13% and
   `points` +25% by making the key comparison call-free, and closed `memcmp`
   as an angle (42.7% of the `points` sample to 2.3%). What is left in the
   descent is the *search*, not the comparison: per probe a `Key::resolve`
   match and a bounds-checked slice, which wants a cell layout the search can
   read a key out of without resolving it. Do not re-propose a cheaper
   comparator (AHL-554, AHL-559) or a per-descent prefix proof (AHL-559
   measured it flat, and 4% behind on this very suite).
2. **Range scan vs SQLite (0.83x; the published cell already contains
   AHL-550's compiled filter, which moved it 97,624 -> 119,219 ops/s, and now
   AHL-559's call-free comparator, +14% on `indexed-range` and +15% on
   `indexed`).** The `memcmp` share this row was scoped against is gone.
   Angle: a borrowed-entry index walk. Do not re-propose a dense rowid walk,
   a covering-index scan, reading the filter-only `TEXT` column raw, or a
   cheaper key comparator — all four were built and measured negative, flat,
   or (the comparator) already landed.
3. **Point-read ops/s vs SQLite WAL (0.56x).** The tail that explained it is
   fixed (AHL-552). This row needs the next gated regeneration, not a fix,
   before anything else is proposed for it.
4. **Batch insert vs PostgreSQL like for like (0.68x).** ~1.0 ms of engine work
   per hundred-row statement in the container against PG's ~0.6 ms, and the
   host profile is 89% fsync and hides it. **Profile it in the container.** The
   state block is not a second barrier per commit — AHL-553 measured one wrap
   per 33.3 hundred-row commits — so the likely item is coalescing the WAL
   record and the dirty pages into one `pwritev`.
5. **Three clean nightly fuzz campaigns** — the written trigger for
   deleting the site's localhost bullet. Track F is otherwise complete:
   F3, F4 and `user list` (AHL-558) all landed 2026-09-05.

6. **Server-to-server writes: the deficit is barrier cost (AHL-560).** The
   statement was never inside the gate — `begin_normal_commit` has one call
   site and `end_write` is the last thing every write path does — so the
   pre-gate residual measures −1.2% to +2.1% and the named experiment had a
   ceiling of 1.02x against a 3.28x gap. What the numbers say instead: our
   barrier costs 2.67 ms against MySQL's 0.78 ms on the same volume class,
   and barrier rate is pinned at one over that. **AHL-561 measured the
   barrier and inverted that:** our `fsync` is 1.322 ms against MySQL's
   1.215 — 1.09x — and the difference is the duty cycle, 51% against 96%.
   Half our cycle is gather and idle gap, not flushing. `fdatasync` is
   1.01x of `fsync` here, so the syscall is not the lever; pipelining the flush cycle was priced at
   2.20x → ~1.13x and **AHL-562 built it and measured nothing**: the
   successor gathers under the in-flight fsync on 83–92% of barriers and
   the duty cycle does not move, because the writers are queued at the
   reservation gate — 20/37/51% of commit latency at 4/8/16 writers, a
   serialized 0.263 ms hold capping ~3,800 commits/s. **AHL-563 halved the hold** and showed the
   writes were never the problem: 22 µs of a 251 µs hold. The cost was a
   region wrap invalidating all four regions' append offsets when it had
   moved one, manufacturing 124 cache misses at 2.6 ms each. Hold 0.262 →
   0.121 ms, throughput 1.54–1.70x at 8–16 writers. **AHL-564 then made wraps rarer**: 73% of
   that ~20 KiB record was the zero hole a page carries between its slot
   directory and its cells, so format v6 elides the longest zero run and
   names it. Same physical log, one encoding tighter — the entry rebuilds
   the page byte-for-byte, so recovery assumes nothing new, and v5 files
   keep being written as v5. **20,747 → 5,725 B/commit; wraps every 50 →
   every 150 commits; hold 0.166 → 0.113 ms at 16 writers.** Throughput
   (1.14–1.22x at 8–16 writers) is the weak column and is published as one.
   **Next:** re-run AHL-562's flush pipeline, which the gate was
   suppressing, and chase the superlinear free-list growth the old record
   ceiling was masking (`free_list_reuse_dst` 187 s → 1,902 s).
   Superseded: diagnosed (AHL-555),
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
