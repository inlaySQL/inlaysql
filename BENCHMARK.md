# InlaySQL benchmarks

Every number here regenerates from a script in this repository. That is the
rule `AGENTS.md` sets and it is the only reason these are worth reading — a
figure nobody can reproduce is worse than no figure. Losses are published
beside wins, because a table that only contains wins is advertising.

**How this run was produced**

```sh
./bench/run.sh                  # points, indexed, joins, vectors, quantisation, retrieval (pinned params)
./bench/compare.sh              # DuckDB, pgvector, Meilisearch, MySQL, PostgreSQL (needs Docker)
```

| | |
| --- | --- |
| Commit | `f8385c9` |
| Date | 2026-08-25 |
| Tree | source clean (`dirty: no` in both raw outputs) |
| Machine | Apple Mac17,9, 18 cores, macOS 27.0 (Darwin 27.0.0 arm64) |
| Toolchain | rustc 1.91.1 (ed61e7d7e 2025-11-07) |
| Raw output | `bench/results/20260825T103354Z.txt` and `20260825T104132Z.txt` (SQLite, `sqlite-vec`; two runs, median published); `20260825T110513Z-compare.txt` (DuckDB, pgvector, MySQL, PostgreSQL). Retrieval section regenerated 2026-08-29 (Meilisearch added): `20260829T084502Z-compare.txt`. Concurrent-writers section regenerated 2026-08-30 on base commit `63b6cb2` with an adaptive commit-coalesce window applied and uncommitted at capture time (committed immediately after regeneration — see PERF.md), median of three runs each: `20260830T031300Z.txt`/`20260830T032800Z.txt`/`20260830T032900Z.txt` (published 1/2/4/8 sweep) and `20260830T031500Z.txt`/`20260830T032100Z.txt`/`20260830T033600Z.txt` (wide sweep). Tail-latency table and the old-vs-new A/B regenerated 2026-08-30 on `08f5fd4` (which added the percentile columns), `WRITER_LEVELS=1,8,32`, median of three runs each: `bench/results/ab-head-run{1,2,3}-*.txt` (current adaptive window) and `bench/results/ab-pre94d96a6-run{1,2,3}-*.txt` (temporarily reverted to the pre-`94d96a6` fixed 8-yield window for the A/B only; not shipped). "Against MySQL and PostgreSQL"'s interleaved rerun regenerated 2026-08-30 on this same commit, 5 repetitions, load-gated: `bench/results/20260830T095714Z-interleaved-oltp-compare.txt` and `bench/results/20260830T095714Z-rep{1..5}-{inlaysql-container,mysql,postgres}.json` |

One developer machine. Reproduce it; do not trust it. Both runs are new this
edition, so — unlike the previous one — every table below comes from the same
commit.

**This edition uses the error bar the last one asked for.** The `run.sh` half
was run *twice* with `REPEATS=3 ./bench/repeat.sh` and the figures below are the
**median of two completed runs** — the third died on transient contention, and
`bench/summarise.py` refused to average a truncated run rather than quietly
including it. 56 of 285 metrics disagreed by 10% or more between the two runs,
which is the honest width of these numbers and is why the ratios matter more
than the digits. Both halves ran with eight unrelated containers on the machine
(load average ~2.5), so absolute figures are still not comparable to earlier
editions.

---

## Against SQLite

SQLite is measured in two configurations because they are two different
promises. `journal` + `synchronous=FULL` + `fullfsync` is the like-for-like
column: it is the only one that makes a durability claim comparable to ours,
and `fullfsync` is what makes a macOS number mean anything at all. WAL +
`synchronous=NORMAL` is SQLite at its fastest, and is the harder target.

### Point reads by primary key — we beat the durable configuration

20,000 rows, 5,000 lookups, prepared statements on both sides.

| Engine | ops/s | p50 | p95 |
| --- | --- | --- | --- |
| **InlaySQL** | **901,158** | **688 ns** | 3.42 µs |
| SQLite, WAL + `sync=NORMAL` | 1,157,945 | 813 ns | 1.06 µs |
| SQLite, journal + `sync=FULL` | 205,742 | 4.44 µs | 7.06 µs |

**4.57x** the durable configuration and 0.78x the fastest one — close enough to
WAL-mode SQLite, which makes no comparable durability claim, that the gap is now
within this benchmark's own spread. The page cache (AHL-420) is what does the
winning half; this is a *warm* cache, and a cold handle warms more slowly than
SQLite's because our miss path is dearer.

This row has now been published at 636,980, then 342,747, and now 901,158
ops/s across three editions, with nothing in any of those commits touching the
point-read path. That spread *is* the finding: the absolute figure on this
machine is worth about a factor of two either way, which is exactly why this
edition publishes a median of repeated runs and why the ratio against
journal-mode SQLite is the number to quote. `bench/compare.sh`'s own OLTP
driver, run in the same window, put the same host workload at 496,765 ops/s —
a different harness, and still inside that band.

### Secondary-index reads — point win, range loss

20,000 rows, `CREATE INDEX` on a non-key TEXT column, 5,000 point lookups and
100 range queries of 50 rows (`SUITE=indexed`).

| Engine | point ops/s | point p50 | range ops/s | range p50 |
| --- | --- | --- | --- | --- |
| **InlaySQL (B-tree index)** | **426,091** | **2.00 µs** | 74,294 | 12.63 µs |
| InlaySQL (no index: full scan) | 747 | 1.34 ms | 564 | 1.78 ms |
| SQLite, journal (index) | 257,514 | 3.75 µs | **124,249** | **7.67 µs** |
| SQLite, WAL (index) | 730,376 | 1.17 µs | **204,551** | **4.71 µs** |

The index itself is worth **570.26x** over our own full scan on point probes
and **131.72x** on range scans (AHL-423). On point probes we beat journal-mode
SQLite 1.65x and sit 0.58x behind WAL-mode. **Range scans we lose outright —
0.60x of journal, 0.36x of WAL.** The entry-walk plus per-row fetch overhead is
the suspect, and it is the same family as the join loss below.

### Joins — we win one shape, lose the other

20,000 users × 160,000 posts, identical schema and indexes on both sides
(`SUITE=joins`). Each row splits the cold first execution of the query shape
from the warm p50 — the cold column is where the join plan and its tables get
built, so it is the expensive one:

| Query shape | InlaySQL cold → p50 | SQLite journal cold → p50 | vs journal |
| --- | --- | --- | --- |
| PK inner, full join | 15.80 ms → 11.28 ms | 11.72 ms → 10.62 ms | **1.07x slower** |
| PK inner, LIMIT 10 | 61.96 µs → 11.88 µs | 12.54 µs → 3.58 µs | 3.27x slower |
| Secondary-index inner, full | 26.31 ms → 3.77 ms | 32.12 ms → 32.08 ms | **7.93x faster** |
| Secondary-index inner, LIMIT 10 | 69.96 µs → 11.25 µs | 27.21 µs → 4.63 µs | 2.41x slower |

The last column is the harness's own throughput ratio (joins/s against joins/s),
which is what the raw output prints; it is close to but not identical with the
ratio of the two p50 columns beside it, because the p50 discards the cold run
the throughput figure includes.

Published because it is true, and because it keeps moving: the
secondary-index inner shape — the one AHL-464 built the index nested-loop join
for — went from **10.71x slower** (2026-08-20) to 2.85x faster (`9aba437`) to
3.65x faster (`9b2f11e`, AHL-447) to **7.93x faster** here, and the PK inner
full join from 5.56x slower to 1.43x to 1.20x to **1.07x slower**. Two changes
since the last edition explain the movement: `1f0bdcb` let the raw leaf scan
read through the page cache instead of re-`pread`ing and re-copying the same
pages on every execution of a prepared query (the `LIMIT` rows' fix, below),
and `bfac72a` (AHL-479) retains the entry-range walk's leaf across calls the
same way AHL-472 already retained one for point lookups, removing the
one-descent-per-outer-row cost the secondary-index-inner shape was paying.
Both are real, reproducible fixes, not run-to-run variance — see `PERF.md`
for the profiles that motivated each.

The `LIMIT` rows are still a loss but a much smaller one than last published:
**3.27x and 2.41x slower** warm, down from 4.65x and 5.81x, moved by the same
two fixes above (the entry-range retained cursor cuts the per-outer-row
descent the secondary-index shape pays; the raw-scan cache cuts the
re-`pread`/re-copy every prepared execution used to pay on both shapes). What
is left, profiled fresh rather than assumed (`PERF.md`'s AHL-488/493
sections): the same page-decode allocation cost those sections already
diagnosed and, in AHL-493's case, already tried twice and rejected for
regressing point reads and small joins — not a new opportunity, a confirmed
one still open. The likelier next win is not in this hot path at all: these
two shapes are the same two tables in opposite `FROM` order, and the
7.93x-faster shape only wins because it drives the join from the smaller side
(20,000 outer iterations against a secondary-index range probe) rather than
the larger one (160,000 outer iterations against a primary-key point probe).
Nothing here chooses that automatically — `FROM posts JOIN users` and
`FROM users JOIN posts` get whichever physical order was written, and join
column ordinals are resolved against that written order at plan time, so
picking the cheaper side is not a local change: it needs the physical
iteration order decoupled from the logical column layout a plan's
expressions already reference. Scoped, not started.

### Durable writes — we win

One row per commit, one `fsync` per commit.

| Engine | ops/s | p50 |
| --- | --- | --- |
| **InlaySQL** | **240** | **4.01 ms** |
| SQLite, journal + `sync=FULL` + `fullfsync` | 90 | 11.13 ms |

**2.66x**: the commit gate no longer re-derives the log on every commit
(AHL-468), which paid on the solo path too. Batching lifts the same workload
to 61,025 ops/s at 10.75 µs — **254x** — which is the number to quote for a
bulk load and not for a transaction.

Every row above is full-durability, on both sides of every comparison, on
purpose — an opt-in relaxed-durability tier also exists
(`EngineOptions::durability`) and is measured separately, in `PERF.md`, not
mixed into these tables.

### Concurrent writers — the peak moved from eight to the mid-teens, and past it the win still shrinks

200 transactions per writer, one row each, on real OS threads. Median of
three runs each (`bench/results/20260830T031300Z.txt`,
`20260830T032800Z.txt`, `20260830T032900Z.txt`), load 3.2–3.7/18 throughout.

| Writers | InlaySQL commits/s | SQLite commits/s |
| --- | --- | --- |
| 1 | 246 | 90 |
| 2 | 394 | 91 |
| 4 | 615 | 91 |
| 8 | **1184** | 91 |

**13.0x SQLite at 8 writers, 0.0% aborted — up from the 8.1x this table
previously published, and not from a faster fsync.** The commit gate's
existing pre-`fsync` gather window (`coalesce_normal_commits`,
`crates/inlaysql/src/device.rs`) used to spend a fixed 8 scheduler yields
deciding whether another writer was about to arrive, which is roughly
200-250x too short — a `yield_now` costs ~135-145ns and a competing writer
needs ~30µs to reach the gate and publish its ticket — so the window almost
always closed before a second writer had a real chance to be gathered,
regardless of how many were waiting. It is now adaptive: it keeps yielding
while a normal commit is inflight or waiting and progress keeps happening,
closing on stalled progress instead of a fixed count. The 8-writer scaling
(1184 against 246 at one writer is 4.81x, up from 2.83x) shows more commits
riding each `fsync`. The 2-writer case is no longer nearly flat either
(394 against 246, 1.60x, up from 1.08x) but is still far from proportional —
see `PERF.md`'s new section for the `pwrite`-during-concurrent-`fsync`
mechanism this fixes and the full before/after. (This table previously
showed 768/8.9x from a spin-before-parking experiment that did not reproduce
and was reverted, then 694/8.1x for the shipped code before this fix; 1184 is
the current, three-times-measured figure and matches the wide sweep below.)

**Published because it is true, not because it flatters us: eight writers is
no longer the peak, but there still is one, and past it throughput still
falls.** A wider sweep the table above stops short of
(`WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`, same session, same machine,
median of three runs — `bench/results/20260830T031500Z.txt`,
`20260830T032100Z.txt`, `20260830T033600Z.txt`): 247 → 357 → 474 → 630 → 796
→ 978 → 1195 → **1519** → **1597** → 1325 → 988 commits/s from 1 to 32
writers. The peak is now a plateau across 12 and 16 writers rather than a
single point at 8 — 1519 and 1597 are close enough across three runs each
that which one nominally wins swaps run to run — and the falloff past it is
still real and still reproduces: 16 → 24 is a 17% drop, 24 → 32 a further
25%. **This is not the same regression re-appearing unfixed — it is the same
shape at roughly double the height.** Every point on the new curve, including
its declining tail, sits well above the old curve's own peak: 32 writers now
does 988 commits/s where the old published ceiling at *any* writer count was
694, and the old 32-writer figure was 516 — 1.91x higher even at the new
curve's worst point. SQLite's own row stays flat across the identical sweep
(89–92 throughout), so this remains specific to how this engine's writers
contend, not generic OS thread-count overhead.

The root cause of the *original* 8-then-falls shape is unchanged and is not
what this fix touches: every writer's whole commit *prepare* phase (conflict
check, WAL encode, page writes, WAL append) still serializes behind one
process-wide gate regardless of how many WAL regions exist; the regions only
let one writer's `fsync` overlap the *next* writer's turn at that gate,
confirmed by profiling (90.4% of samples parked waiting for it). The obvious
cheap fix — spin before parking, in case kernel wake latency rather than the
gate itself was the cost — was tried, measured clean, and reverted: no
change, at 100 or at 5,000 spin iterations. A follow-up idea (shrink the gate
to the conflict check and sequence/offset reservation only, move the encode
and writes after release) turned out to be unsafe, not just unscoped: the
conflict check walks the tree from the latest committed root, so it
structurally depends on the previous writer's pages already being landed.
Finer profiling also found the gate-held section was already cheap (under 6%
of the time; the rest is pure contention). What *did* move the needle this
time was a different, smaller lever: the fixed-yield gather window on the
*flush* side, described above — not the gate itself. See `PERF.md` for the
full investigation and the one lever still standing for the residual
regression above the new, higher peak: *commit-side* logical group commit
(one gate holder absorbing other waiting writers' whole transactions into
one prepare/encode/WAL-append pass, not just one `fsync` covering several
already-encoded ones), scoped but not started.

### Concurrent writers: the tail the commits/s table hides

`08f5fd4` added per-commit p50/p95/p99/max to `inlaysql-bench --suite
concurrency`, so this is the first edition that can show what the peak-shape
sweep above only implies: an average going up while some writers wait much
longer than the median. `WRITER_LEVELS=1,8,32`, 200 transactions per writer,
median of three runs each, load 2.7–5.5/18 across the runs (disclosed because
the 8- and 32-writer figures are the ones sensitive to it):

| Writers | InlaySQL commits/s | p50 | p95 | p99 | max | SQLite commits/s | p50 | p95 | p99 | max |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 250 | 3.99 ms | 4.71 ms | 7.88 ms | 8.30 ms | 88 | 11.60 ms | 12.87 ms | 13.17 ms | 13.33 ms |
| 8 | **1142** | 5.16 ms | 20.26 ms | 34.90 ms | 58.98 ms | 86 | 11.80 ms | 13.24 ms | 17.20 ms | 31.90 ms |
| 32 | **968** | 24.19 ms | 97.06 ms | **122.13 ms** | 154.96 ms | 85 | 11.71 ms | 13.15 ms | **17.82 ms** | 45.87 ms |

**Published beside the win because it is the same trade: at 32 writers
InlaySQL does 11.4x SQLite's committed throughput and loses p99 by 6.9x
against SQLite's own tail**, which stays inside roughly 13–18 ms at every
writer count measured here because SQLite serializes writers at its file
lock — the connection that's waiting pays in queueing, not in a longer
`fsync`. InlaySQL's optimistic design instead lets the gather window grow the
cohort riding one `fsync` as contention rises, and the writers gathered late
in a big cohort are the ones sitting in its p99: p50 at 32 writers (24.19 ms)
is not far past solo (3.99 ms, mostly one `fullfsync`), but p99 (122.13 ms)
is 5x that p50. This is a real cost of the concurrent-writer design, not
noise, and it belongs in the table, not a footnote.

**Whether the adaptive gather window (`94d96a6`) is *why*: no — the data says
the opposite.** Nobody had measured p99 before `08f5fd4` existed, so it was
an open question whether trading a fixed 8-`yield_now` gather window for an
adaptive one (up to `COMMIT_COALESCE_MAX_YIELDS` = 16,384, closing on
`COMMIT_COALESCE_STALL_YIELDS` = 1,500 yields of no progress —
`crates/inlaysql/src/device.rs`) had bought throughput at the tail's expense.
Three interleaved A/B pairs (old, new, old, new, old, new — "old" built by
temporarily setting `COMMIT_COALESCE_MAX_YIELDS` back to `8`, which is
provably identical to the pre-`94d96a6` fixed loop because
`COMMIT_COALESCE_STALL_YIELDS`, at 1,500, can never be reached within 8
iterations), same `WRITER_LEVELS=1,8,32`, median of three runs each
(`bench/results/ab-pre94d96a6-run{1,2,3}-*.txt`,
`bench/results/ab-head-run{1,2,3}-*.txt`):

| Writers | old commits/s | old p99 | new (`94d96a6`) commits/s | new p99 | Δ commits/s | Δ p99 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 247 | 7.94 ms | 250 | 7.88 ms | +1.2% (noise) | −0.8% (noise) |
| 8 | 576 | 45.01 ms | 1142 | 34.90 ms | **+98.3%** | **−22.5%** |
| 32 | 504 | 150.89 ms | 968 | 122.13 ms | **+92.1%** | **−19.1%** |

The adaptive window **roughly doubles throughput and lowers p99** at both 8
and 32 writers, and the direction was consistent across all three pairs at
both levels — not a one-off. The tail loss against SQLite above is real and
stays in the table, but it predates `94d96a6` and was *worse* under the old
fixed window (150.89 ms p99 at 32 writers, against 122.13 ms now) for barely
half the throughput. It is a structural cost of gathering more commits behind
fewer `fsync`s under contention, not something the adaptive window
introduced — so this edition does not re-tune `COMMIT_COALESCE_MAX_YIELDS` /
`COMMIT_COALESCE_STALL_YIELDS`. The data does not call for a trade that would
give back a p99 win already banked, and the shipped constants are unchanged.

---

## Against `sqlite-vec` — we win

2,000 vectors, dim 384, 100 queries, top-10, recall measured against an
exhaustive oracle.

| Corpus | recall@10 | p50 | vs `sqlite-vec` |
| --- | --- | --- | --- |
| Text-derived embeddings | 1.000 | 88.29 µs | **7.56x faster at 100% of its recall** |
| Uniform random | 0.922 | 95.29 µs | 6.70x faster at 92.2% of its recall |

Both corpus shapes are published because only one of them flatters us. Uniform
random vectors in 384 dimensions have no structure for a graph index to
navigate, so recall falls and no amount of tuning fixes it. Text-derived
embeddings are what an application actually stores.

`VECTOR(n, INT8)` quantisation costs 0.014 recall on the realistic corpus
(0.986 vs 1.000 exact) and nothing measurable on the random one (0.922 both),
for a 1.65x smaller file and a 3.96x smaller resident payload.

## Against an external benchmark — `ann-benchmarks`, glove-25-angular

Every other table on this page uses our corpus, our oracle and our harness.
This one uses none of them.
[`ann-benchmarks`](https://github.com/erikbern/ann-benchmarks) publishes fixed
datasets with **precomputed ground-truth neighbours** and one recall/QPS
protocol that every engine on its leaderboard is measured by;
`bench/ann/module.py` is InlaySQL's plugin for it, reaching the engine over
`inlaysql serve --mysql` with ordinary SQL, and `bench/ann/run.py` runs the
protocol without Docker. Methodology, the seam and every limitation the
exercise exposed are in
[`bench/README.md`](bench/README.md#ann-benchmarks--an-external-corpus-an-external-ground-truth-an-external-protocol).

```sh
bench/ann/.venv/bin/python bench/ann/run.py --dataset glove-25-angular
```

| | |
| --- | --- |
| Commit | `4d9f535` (tree dirty: the adapter was uncommitted when it ran) |
| Date | 2026-08-26 |
| Machine | Apple Mac17,9, macOS 27.0 (Darwin 27.0.0 arm64), rustc 1.91.1 |
| Dataset | `glove-25-angular` — 1,183,514 x dim 25, 10,000 queries, k = 10 |
| Recall against | the dataset's own `distances` array. **Not our oracle.** |

Exact `VECTOR(25)`, three runs, `QPS = 1 / best_search_time`:

| over-fetch | effective `ef` | recall@10 | QPS | p50 |
| --- | --- | --- | --- | --- |
| 1 | 64 | 0.9878 | 3,021 | 0.331 ms |
| 4 | 80 | 0.9996 | 1,179 | 0.850 ms |
| 8 | 160 | **1.0000** | 653 | 1.534 ms |
| 64 | 1,280 | 1.0000 | 104 | 9.647 ms |

Build: 294.9 s — 36.2 s loading over the wire and **258.7 s building the graph
on the first read**, which is the single largest cost in this table. It used to
be unaskable-for as well as slow: a user hit it as an unexplained multi-minute
stall on whichever query happened to be first, with no statement that could
have moved it. That half is closed — `OPTIMIZE TABLE t` over the wire,
`REINDEX` in SQL, `Database::reindex` embedded, all cancellable and all a no-op
when nothing is pending
([`docs/indexes.md`](docs/indexes.md#the-build-is-deferred-and-you-can-ask-for-it)).
The 258.7 s is unchanged; it is now a statement the loader runs on purpose
rather than a stall. Index: 1,047 MiB for a 112.9 MiB corpus (9.3x).

`VECTOR(25, INT8)` on the same data builds in 461.3 s (1.56x slower), holds
790 MiB (1.34x smaller), and **tops out at recall 0.9982** — a quantisation
floor no amount of over-fetching recovers, where exact reaches 1.0000. It is
also ~7% slower per query at `over_fetch = 1`. The smaller-memory half of that
trade is real; there is no faster half.

**Not comparable in absolute terms to the QPS figures on ann-benchmarks.com**,
which are run on a fixed cloud instance type. The curve is comparable, and it
is the first number on this page that somebody who has never seen this
repository can check.

Three things the adapter could not do, and they are engine limitations rather
than harness ones: ~~`vector_score` is cosine-only, so `sift-128-euclidean` and
`fashion-mnist-784-euclidean` cannot be answered at all~~ — **closed.** A
vector index now carries the distance it was built under, written at
`CREATE INDEX` with pgvector's operator-class spelling
(`CREATE INDEX ... ON items USING hnsw (embedding vector_l2_ops)`), and the
adapter maps the dataset's own metric onto it, so both `-euclidean` starters
run. The numbers in the table above are `glove-25-angular` and predate that
change; they are unaffected by it, because a cosine index writes and scores
exactly the bytes and the arithmetic it always did. `vector_ip_ops` is
deliberately absent — inner product is not a metric, and the reasoning is in
`crates/inlaysql-core/src/hnsw.rs`. The sweep above is an over-fetched `LIMIT`
and predates `SET inlaysql_hnsw_ef_search`, which the adapter now sweeps
instead — the same dial pgvector's own plugin sweeps as `SET hnsw.ef_search`;
`m` and `ef_construction` are still Rust-only. An embedding **can** now be bound
as a parameter over the wire (AHL-478) — `dim` little-endian `f32`s in a string
parameter, MySQL 9's own `VECTOR` storage format — where every one used to cross
as decimal text. Measured on `glove-25-angular`, that took the load from 363.9
MiB on the wire for a 112.9 MiB corpus (3.22x) to 127.9 MiB (1.13x) and from
41.20 s to 19.77 s, counted by the server's own `Bytes_received`. Recall is
untouched: the same `f32`s reach the index either way, and only their spelling
on the wire changed. See `bench/README.md` and `docs/server.md`.

## Retrieval — BM25 is no longer the expensive half

2,000 documents, dim 384, `LIMIT 10`. Ingest 14,063 docs/s.

| Workload | p50 | p95 | Previous edition (`9aba437`) |
| --- | --- | --- | --- |
| Vector only | 68.79 µs | 114.29 µs | 87.88 µs |
| BM25 only | **47.75 µs** | 60.63 µs | 347.50 µs |
| Hybrid (fused) | **95.17 µs** | 110.21 µs | 453.88 µs |

Hybrid is **one SQL statement**, not two queries and a client-side merge.

BM25 fell **7.3x** and hybrid **4.8x**, and that one is code: the full-text
index stopped being a map of maps and became an ordinary inverted index with
dense document ordinals, a bounded top-`k` heap instead of scoring and sorting
the whole corpus to keep ten rows, and a MaxScore walk that stops visiting
documents whose entire possible score cannot reach the `k`-th best already
found. Measured directly on the index over the same seed, before → after:

| Corpus | BM25 p50 | Hybrid p50 |
| --- | --- | --- |
| 2,000 docs | 319.79 µs → 51.75 µs | 374.21 µs → 104.29 µs |
| 5,000 docs | 802.92 µs → 105.67 µs | 880.29 µs → 166.50 µs |

The scores are unchanged bit for bit and the ranking is unchanged including
ties — skipping applies only to documents whose whole possible score is
*strictly* below the `k`-th best, because a document that merely equalled it
could still win the tie on the lower row id. `crates/inlaysql-core/src/bm25.rs`
carries the argument and the tests that pin it.

BM25 was 79% of the hybrid p50 before this; it is now 50%, and the vector leg
is the larger half. Per-block impact bounds (block-max WAND) are the next step
and are not implemented.

---

## Against DuckDB, pgvector and Meilisearch

One corpus, one set of queries, one exhaustive ground truth, each engine asked
for its own query plan so an unindexed row cannot masquerade as an indexed one.
5,000 documents, dim 128. **Meilisearch added 2026-08-29** — a dedicated
search engine, the other kind of thing this retrieval story competes with
beyond "a database with vectors added"; see `bench/README.md`'s
`bench/external/meilisearch_driver.py` notes for what it measures and does
not (its own hybrid mode is deliberately not used — this driver fuses with
the identical `common.rrf` every engine in this table is fused with, so the
fusion algorithm stays constant and only the raw rankings differ).

| Engine | recall@10 | vector p50 | hybrid p50 |
| --- | --- | --- | --- |
| **InlaySQL** (HNSW + BM25) | 1.000 | **126.00 µs** | **197.00 µs** |
| DuckDB (exhaustive + fts BM25) | 0.999 | 4.88 ms | 11.88 ms |
| DuckDB (vss HNSW + fts BM25) | 0.993 | 3.95 ms | 11.51 ms |
| Meilisearch (`arroy` ANN + its own ranking) | 0.997 | 1.22 ms | 4.04 ms |
| pgvector (HNSW + `ts_rank`) | 0.988 | 152.00 µs | 13.64 ms |
| pgvector (exhaustive + `ts_rank`) | 0.999 | 488.00 µs | 13.99 ms |

**Hybrid is roughly 20x** the nearest baseline now that one exists
(4.04 ms, Meilisearch) and **60–70x** DuckDB/pgvector, up from 14–17x two
editions ago and 60–92x in the last one (a same-session, same-run
regeneration — read the InlaySQL row's own movement, 84 µs/130 µs last
edition to 126 µs/197 µs here, as the noise band this page keeps warning
about, not a regression). The BM25 index rewrite is most of the multiple
against DuckDB/pgvector: our hybrid p50 has gone 875 µs → 191 µs → 130–197 µs
while those two baselines stayed inside their own noise. It is still not one
query against one query — it is one statement here against two queries plus
client-side rank fusion there, Meilisearch included — and `bench/README.md`
says so plainly.

**Vector-only stays a win against pgvector, and Meilisearch is the fastest
baseline recall-for-recall over a network.** 126 µs against pgvector's
152 µs (both include pgvector's socket round trip a library in your own
process does not pay, so read it as close rather than as a rout) and against
Meilisearch's 1.22 ms — not a fair fight in InlaySQL's favour so much as a
different product: Meilisearch's ANN search also runs its own typo-tolerance
and ranking pipeline, which pgvector's raw `<=>` operator does not.
Meilisearch's `agree` (0.419) sits in the same range as pgvector's
`ts_rank_cd` rows (0.457/0.465) for the same reason both are below DuckDB's
real BM25: neither ranks text with BM25 at all.

---

## Against MySQL and PostgreSQL

**Reads: we win by a wide margin. Sequential writes: we lose to both, and by
more than in the previous edition.**

InlaySQL is measured twice — on the host with a real `F_FULLFSYNC` barrier,
and **inside a container on the same volume class as the servers**, so all
three pay the same virtualised fsync. The gap between the two InlaySQL rows is
what that virtualisation is worth on this machine.

| Engine | write ops/s | read ops/s |
| --- | --- | --- |
| InlaySQL, host (real `F_FULLFSYNC`) | 253.2 | 497k |
| InlaySQL, containerised | 849.7 | **678k** |
| MySQL 8 (`innodb_flush_log_at_trx_commit=1`, binlog off) | 1,184.2 | 9.2k |
| PostgreSQL 17 (`fsync=on`, `synchronous_commit=on`) | **1,612.8** | 19.4k |

**Reads: ~74x MySQL and ~35x PostgreSQL**, containerised — an in-process
library against a socket round trip. That asymmetry is structural and stated,
not hidden.

**Writes: we lose to both.** PostgreSQL is 1.90x faster than the containerised
row (1,612.8 against 849.7) and MySQL 1.39x (1,184.2). Which of the two servers
leads has now flipped between editions while our own figure barely moved, so
read the ordering as noise and the fact that we trail both as the finding.
**This single run should not be read on its own — see "Interleaved, repeated,
quiet-machine rerun" below the correction that follows this section**: a
same-session sequential rerun found the ordering flipped and the multiple
shrunk by about a third, but a properly interleaved, repeated, load-gated
rerun found the opposite — PostgreSQL ahead of MySQL in 5 of 5 repetitions,
median multiples of 1.81x/1.43x, close to this table's own 1.90x/1.39x. Read
this table's ordering and multiple as closer to right than the sequential
check below suggested, not as replaced by it. The previous edition had
us beating PostgreSQL and within 1.08x of MySQL; our own figure improved
(723.1 → 847.2) and both servers improved more, on a run where they had eleven
unrelated containers for company and we did not control for it. So the ranking
here is real for this run and the size of the gap is not to be trusted. What is
structural, and unchanged: this workload is one commit at a time on one
connection, so group commit cannot fire by design, and the remaining cost is
per-commit against InnoDB's redo write. Closing it is scheduled work. The
concurrent-writer story (1184 commits/s on 8 writers above) has no
MySQL/PostgreSQL counterpart on this page yet — a server-to-server concurrent
row is the missing apples-to-apples.

### Correction (2026-08-30): this table is not apples-to-apples, and the asymmetry favours InlaySQL

The Reads line above already says the in-process-vs-socket asymmetry is
structural. It undersold it. Read as written, "we lose to both" on writes
reads like an engine-side finding against a matched comparison. It is not a
matched comparison, and a same-engine control shows the write row above is
already skipping roughly as much transport cost as the entire published gap
to PostgreSQL.

**The transport tax, measured with InlaySQL against itself.** The
containerised row above is a library call: `bench/external/compose.yml`'s
`inlaysql-oltp` service runs `cargo run -p inlaysql-bench --
--oltp-replay ...`, in-process, no socket. `mysql_driver.py` and
`postgres_oltp_driver.py` (below MySQL's and PostgreSQL's rows) reach their
servers with `mysql.connector`/`psycopg` over the compose bridge network — a
socket round trip on *every* statement. `inlaysql serve --mysql` at one
connection (the "Server-to-server" table below) writes at 556.7 ops/s —
1,795.6 µs/commit — over the identical wire protocol MySQL's own row pays.
The containerised library row writes at 1,177.0 µs/commit (849.7 ops/s,
published) or 1,369.3 µs/commit (730.4 ops/s, a same-session rerun today).
That is **~420-620 µs of transport/driver tax that InlaySQL's containerised
row skips and both MySQL and PostgreSQL pay on every statement** — the same
order of magnitude as the entire published PostgreSQL gap (620 µs). A reader
should come away understanding that this row flatters InlaySQL, and that a
transport-matched comparison would very likely reverse part of this gap, not
just narrow it.

**The numbers are unstable across runs, and the ordering flips.** A fresh,
same-session rerun of this table's own drivers today (`ROWS=3000
LOOKUPS=1000 ./bench/compare.sh`, host load ~6.2/18 — not quiet, disclosed
rather than hidden): InlaySQL host 240.9 ops/s, InlaySQL containerised 730.4,
MySQL 931.2 (**1.27x**), PostgreSQL 805.0 (**1.10x**) — against the published
849.7 / 1,184.2 (1.39x) / 1,612.8 (1.90x). **PostgreSQL is now slower than
MySQL, where the published table has it leading**, and the multiple against
both shrank by about a third in one sequential rerun. Root cause, measured
directly rather than inferred: the Docker named volume's own `fsync` cost
drifted 1.5-1.8x within the same session — roughly 1,150 µs before the
MySQL/PostgreSQL containers were up, 640-800 µs ten minutes later with them
running. This reproduces `PERF.md`'s AHL-496 finding of a 2.1x drift 90
minutes apart, at a shorter timescale and inside one benchmark run.

**And it is not our CPU path.** A hypothesis was tested and rejected: that
in-container, where the barrier is weaker than the host's `F_FULLFSYNC`,
hidden per-commit CPU cost would surface as a real, engine-side cause of the
gap. Measured directly (`PERF.md`'s 2026-08-30 section has the method and
both runs), `fsync` is **87.8-89.1%** of a containerised commit — the same
barrier-dominated shape as the host's 97.1%, just a smaller absolute
barrier. InlaySQL's own non-`fsync` work is only ~11-12% of commit time, so
even a zero-cost commit path caps the achievable win at roughly 1.15x —
nowhere near the published 1.39-1.90x gap. **There is no engine-side fix for
this workload's gap.** It lives in the volume's barrier and its drift, and
in the transport asymmetry above, not in this engine's code. The honest next
step is a methodology fix — interleaved, repeated, quiet-machine runs
publishing median and spread (`bench/README.md`'s "How many times to run
it") — not more profiling of the commit path.

### Interleaved, repeated, quiet-machine rerun (2026-08-30): AHL-496's "what is owed" item, paid

The correction above named the fix and did not do it: this section is that
rerun. One repetition is InlaySQL (containerised), MySQL and PostgreSQL, run
back to back against the same warm containers, immediately followed by a
control — a raw `pwrite`(80 KiB)+`fsync` loop (5 warm-up + 25 timed reps) on
`inlaysql-bench-floor-data`, a named Docker volume of the same `local`-driver
class as `postgres-oltp-data`/`mysql-oltp-data`/`inlaysql-oltp-data` but
written by nothing except the probe — so every repetition carries its own
reading of what the volume's barrier cost at that moment, the instrument
`PERF.md`'s AHL-496 section used to explain the original drift. The cycle
repeated 5 times. `ROWS=20000 LOOKUPS=5000` — unchanged from the published
table above.

**Load, disclosed.** `bench/compare.sh` has no load gate (see the
recommendation below), so the gate was manual, matching `bench/run.sh`'s
`BENCH_MAX_LOAD_PER_CPU=0.25` rule (18 logical CPUs → keep the 1-minute
average well under ~4.5): checked before every repetition, waiting and
rechecking rather than running and caveating. Two repetitions were caught
and discarded mid-run by processes with nothing to do with this benchmark —
an unrelated Xcode-beta build (dozens of parallel `clang` processes) spiked
the 1-minute average past 150, and later an unrelated codesigning/indexing
burst spiked it past 80 — and redone once the host settled back under 4. The
5 repetitions published below all started at a 1-minute load between 2.0 and
4.0 and stayed there throughout; the raw file
(`bench/results/20260830T095714Z-interleaved-oltp-compare.txt`) carries the
`uptime` reading before and after every repetition, including the two
discarded ones, rather than only the ones that looked clean.

**Write ops/s, median and spread (min-max) over the 5 repetitions:**

| Series | median | min | max | spread |
| --- | ---: | ---: | ---: | ---: |
| `pwrite`+`fsync` floor (control) | 985.2 | 854.0 | 1,005.6 | 15.4% |
| InlaySQL, containerised | 698.9 | 557.0 | 909.5 | 50.4% |
| MySQL 8 | 1,002.3 | 722.9 | 1,535.9 | 81.1% |
| PostgreSQL 17 | 1,265.7 | 954.2 | 1,621.5 | 52.7% |

**The honest multiple: PostgreSQL 1.81x, MySQL 1.43x** (median against
median) — close to the published 1.90x/1.39x, not the shrunken 1.10x/1.27x
the single sequential rerun above found. Read that as the sequential rerun
having been the noisy measurement, not this one: the published table's
multiple was closer to right than its own same-session sequential check
suggested.

**The MySQL/PostgreSQL ordering is stable, not flipped.** PostgreSQL beat
MySQL in **5 of 5 repetitions** (ratio 1.06-1.32x each time — see the raw
file for all five). The single sequential rerun's flip (PostgreSQL behind
MySQL) does not reproduce under interleaving; it looks like exactly the kind
of sequential-measurement artifact this rerun exists to catch, not a second
data point about which server is really faster.

**The floor does not explain most of this run's variance, which is itself a
finding.** The control's own spread (15.4%) is far smaller than any engine's
(50-81%), and the correlation between the floor and each engine across the 5
repetitions is weak: MySQL +0.51, PostgreSQL +0.46, InlaySQL **-0.51**
(Pearson r, n=5 — small enough to read the sign more than the precision).
On an already-warm, already-quiet stack, the raw fsync floor stayed close to
one value; the engines still swung 50-81%. That does not contradict the
1.5-2.1x floor drift `PERF.md`'s AHL-496 section and the correction above
both measured — those were captured while containers were cold-starting or
the host was still settling, exactly the conditions this rerun's load gate
and container-warm-up were designed to avoid. It does mean that once that
condition is controlled for, most of the remaining run-to-run noise in this
table is not the storage volume — it is more likely the Python driver/
connector overhead, `docker exec`/process-spawn jitter, or the compose
bridge network, none of which this rerun isolated further.

**This does not make the comparison fair, only quieter.** The transport
asymmetry the correction above quantifies (~420-620 µs/commit, the same
order as the entire published PostgreSQL gap) is untouched by any of this —
interleaving and repetition remove noise, not the library-vs-socket
asymmetry. If anything, a cleaner multiple that lands close to the original
1.39-1.90x is a *more* confident restatement of a comparison that already
favours InlaySQL by construction, not evidence that the engine got faster.

**Transport-matched, single run (cheap, not interleaved or repeated).**
`bench/external/server_driver.py` already exists (AHL-489, "Server-to-server"
below) and reaches both `inlaysql serve --mysql` and MySQL with the same
`mysql.connector` client, so it was run once more alongside this rerun
(`SERVER_ROWS=2000 SERVER_LOOKUPS=1000`, load ~3.8/18, disclosed, single run,
not median-of-N): at one connection, InlaySQL wrote 627.6 ops/s against
MySQL's 849.4 — **0.74x**, not the 1.43x loss the containerised library row
above shows. That is the same direction the correction above predicts: over
a matched transport, more of the published write gap closes than the
containerised row alone suggests. It is one run, not a repeated median, so
it is reported as a data point for feasibility rather than a replacement
number — see "Server-to-server" below for the fuller, disclosed-load
(also single-run, also not repeated) edition of this same comparison,
including concurrency levels this rerun did not touch.

**Raw data.** `bench/results/20260830T095714Z-interleaved-oltp-compare.txt`
(the full session: header, both discarded attempts with their `uptime`
readings and the reason each was thrown out, all 5 published repetitions,
the summary table above, and the transport-matched bonus run) and, per
repetition, `bench/results/20260830T095714Z-rep{1..5}-{inlaysql-container,
mysql,postgres}.json` — the exact files each driver/replay wrote, copied out
before the next repetition overwrote them. `bench/results/` is git-ignored
per this repo's convention; the filenames above are cited so the run is
traceable even though the files themselves are not committed.

**Recommended, not implemented.** This session's own experience argues for
it: two of the seven repetition attempts above were caught only because a
human was watching `uptime` between phases, and a load gate would have
refused both automatically instead. `bench/run.sh`'s check (`bench/run.sh`,
the `BENCH_MAX_LOAD_PER_CPU` block near the top) is a ~25-line, self-contained
`awk`/`uptime`/`sysctl` block that reads cleanly onto `compare.sh`'s own
preamble, before the corpus is generated. It was not ported here because
`compare.sh` is also run from `trust.yml`'s `benchmarks` job in CI
(`ubuntu-latest`, shared runners, 2-4 vCPUs), where `run.sh`'s identical gate
is already a known, tolerated flake (a `uptime`-format/CPU-count mismatch or
a genuinely busy shared runner exits 3) — verifying that adding the same
behaviour to `compare.sh` would not turn an already-accepted single flake
into two, or interact with the job's Docker-availability fallback, needs
someone to actually watch a CI run do it, not a static read of the workflow.
That verification did not happen in this session, so the port stays a
recommendation.

### Server-to-server: MySQL wire protocol

`inlaysql serve --mysql` reached over the compose network by `mysql.connector`,
matched against MySQL 8, same driver and same transport on both sides. Every
row pays a socket round trip.

**Regenerated 2026-08-29 with the process-based driver** (`f8e29e9`, built
2026-08-27, run for the first time here): each connection is a spawned OS
process, not a Python thread, so `mysql.connector`'s GIL — confirmed below to
have contaminated every earlier edition of this table — cannot be in this
run's numbers. Checked quiet beforehand (host load ~3/18 logical CPUs);
`bench/compare.sh` has no automated load gate the way `bench/run.sh` does, so
this is a disclosed manual check, not an enforced one.

| Engine | Connections | write ops/s | read ops/s |
| --- | --- | --- | --- |
| **InlaySQL** (`inlaysql serve --mysql`) | 1 | 556.7 | **9,033.3** |
| **InlaySQL** (`inlaysql serve --mysql`) | 8 | 1,255.5 | 6,294.3 |
| MySQL 8 | 1 | 787.7 | 7,400.6 |
| MySQL 8 | 8 | 3,092.7 | 7,931.1 |

At one connection InlaySQL writes 0.71x of MySQL and reads 1.22x. At eight
connections writes are 0.41x (MySQL's own throughput nearly quadruples,
787.7 → 3,092.7; InlaySQL's writes scale to only 2.25x, 556.7 → 1,255.5) and
reads are 0.79x — and unlike every earlier edition of this row, **that read
number is not a GIL artifact**: MySQL's own read throughput is flat across
the same 1-to-8 step (7,400.6 → 7,931.1), while InlaySQL's *falls* in
absolute terms (9,033.3 → 6,294.3, a real 30% drop, process-isolated driver
on both sides). Retries are zero on both engines at both concurrency levels,
so this is not lock contention between disjoint id ranges.

**This retires the open question the last three editions of this table
carried**, not by finding a fix but by finding out the diagnosis was
incomplete rather than wrong: the correction below (still accurate for what
it found) showed the *old*, larger read drop was substantially a threaded
Python client's GIL, not the server. It did not claim the drop would
vanish entirely once that confound was removed, and it has not — a smaller,
real one remains, process isolation and all. `inlaysql-server`'s
thread-per-connection model (`docs/server.md`'s D2) is the standing
architectural difference from MySQL's worker pool named below and was the
obvious next suspect — **checked separately, immediately after this run,
and it did not hold up either, twice**: the same driver against the same
server running directly on the host (no Docker) scales *up* with
concurrency at every workload size tried, profiled at 81.4% idle in
`recvfrom`; then isolated *inside* the compose network too —
`inlaysql-server` alone, and again with the full five-container stack
present but idle — and both still scale up cleanly. What does reproduce the
drop: running `mysql_driver.py` for real immediately before
`server_driver.py`, in that same idle stack, which is exactly `compare.sh`'s
own phase order. See "What is not measured here" below for the numbers.
`inlaysql-server`'s own concurrency handling is now the *less* likely place
this drop lives — two independent checks, on two different hosts (loopback
and compose network), say it has spare capacity at this concurrency — and a
preceding driver phase's burst of activity is the more likely one, though
the exact mechanism (MySQL's own background catch-up, `drivers`-container
state carried across phases, a Docker Desktop VM scheduler effect) is not
pinned down. Absolute
throughput here is well below every other edition of this table on both
engines — a busier host or a container resource change since the last
`compare.sh` run, not a regression in either database; only the relative,
same-run comparison is meaningful.

**History, kept because the reasoning is why the driver was rebuilt, not
because the numbers still stand.** Two editions ago this table showed a
1-to-8-connection read drop (26,270 → 17,628 reads/s) and named it as
evidence that eight connections warm eight per-handle page caches over the
same pages — a server-side diagnosis. A later investigation tested that
claim rather than trusting it and it did not hold: rebuilding the read phase
on a quiet machine with the same client, same driver, same shape did not
reproduce the aggregate drop, but running the client's concurrency as
*threads* instead of *processes* did, independently, twice — including on
MySQL's own row, which lost 31% of its reads across the identical two steps
purely from the client's GIL, while the server (sampled mid-run) sat idle in
`recvfrom`. A shared raw-page cache across connections was built anyway
during that investigation (`docs/server.md`'s D2) — real, worth roughly 18%
on the page-miss path — but it could not have been the fix even in
principle, because this benchmark's per-handle cache already holds the whole
working set warm before the shared one is ever asked. The conclusion that
investigation reached — *the numbers were real, the explanation attached to
them was not tested, and a process-based re-run was the honest next step* —
is exactly what the table and the numbered paragraph above this one now are.
See "Server-to-server" in `bench/README.md` for the detailed methodology,
the concurrency-model/credential/TLS asymmetries that remain, and why
PostgreSQL has no row here.

What this still cannot prove: Docker Desktop's virtual disk was never
independently verified to honour `fsync` as a barrier for *any* of the three
engines. "Comparable" is not "hardware-durable".

---

## What is not measured here

- **No external benchmark for text or hybrid ranking.** Vector search now has
  one — the `ann-benchmarks` table above is an external corpus, an external
  ground truth and an external protocol. BM25 and the hybrid fusion still have
  none: every quality figure for them on this page is scored against a
  reference ranking this repository computes. The counterpart would be a BEIR
  subset (`scifact`, `nfcorpus`) with its own `qrels` and nDCG@10, and
  `bench/README.md` records what building it needs — chiefly a decision about
  whether to match Lucene's analyzer, since BEIR's published BM25 baselines are
  Anserini's and a gap against them would otherwise be a tokenisation
  difference wearing a scoring result.
- **Now has a trustworthy driver, and the obvious server-side explanation
  did not survive checking, twice.** The server-to-server table above is
  process-isolated on both sides as of 2026-08-29, closing the "the client's
  own concurrency shape might be the real cause" question the last three
  editions carried: InlaySQL's read throughput really does drop from one
  connection to eight (9,033.3 → 6,294.3 ops/s), where MySQL's own is flat
  across the same step. First check: the same driver against `inlaysql
  serve --mysql` directly on the host (no Docker, a loopback socket) at
  matching and larger workload shapes never reproduces it, scaling *up*
  instead (1,180 → 9,980 ops/s at the Docker-matched shape), and a profile
  of the server during that load found it 81.4% idle in `recvfrom`, not
  CPU-bound. Second check, ruling out the container environment itself
  rather than just the host/loopback difference: `inlaysql-server` run
  *inside* the same compose network, alone and then with the full
  five-container stack present but idle, both scale up cleanly too
  (2,336 → 16,549 ops/s with every original container present and running,
  none of them sent a query). What *does* reproduce the drop: running
  `mysql_driver.py` for real immediately before `server_driver.py`, in that
  same idle stack — exactly this table's own generation order at the time
  (`bench/compare.sh` ran DuckDB, pgvector, PostgreSQL, MySQL, *then*
  server-to-server; Meilisearch was added to the sequence afterward, between
  pgvector and PostgreSQL). Not root-caused past that — plausibly MySQL's own
  post-write background work, plausibly something the `drivers` container
  carries across sequential phases, plausibly a Docker Desktop VM scheduler
  effect from a preceding burst that a 20-second gap did not clear — but
  `inlaysql-server`'s thread-per-connection model, the standing
  architectural difference from MySQL's worker pool and the obvious first
  suspect, is now the *less* likely explanation: two independent hosts
  (loopback and compose network) both say it has spare capacity at this
  concurrency. A worker-pool rewrite aimed at this number would very likely
  not fix it. See `PLAN.md`'s W5 for what is still open.
- **No server-to-server PostgreSQL row.** `inlaysql serve` speaks the MySQL
  wire protocol and nothing else, so there is no like-for-like transport to
  measure PostgreSQL over; `bench/README.md` says so under
  "Server-to-server".
- **No sustained or multi-core saturation workload.** Everything here is a
  latency-shaped micro-benchmark on one machine.
- **No controlled machine state, and no error bar on this edition.** The two
  halves ran about twenty minutes apart under visibly different load (idle,
  then Docker Desktop with eleven unrelated containers), and several figures
  moved by more than this commit could possibly explain — point reads by 1.86x
  in the losing direction, vector search by 1.88x in the compare half. Every
  number on this page is one run, so a single figure is worth roughly a factor
  of two and only the same-run ratios are worth reading closely.

  Half the fix now exists: `REPEATS=5 ./bench/repeat.sh` runs the whole suite
  five times and reports each number's median and how far the runs disagreed,
  and `bench/README.md`'s "How many times to run it" explains how to read the
  spread. **This edition predates it and does not use it** — the next one
  should, and should print the spread beside every figure it quotes. Pinning
  the machine state itself is still not done and probably cannot be, which is
  exactly why the spread has to be published instead.
- **Cold-cache reads.** The point-read rows are warm; an application that
  opens, reads a handful and exits sees worse, because our miss path is dearer
  than SQLite's.

## The correctness note, updated on purpose

The previous edition of this file carried an open crash-recovery bug
(AHL-406): one schedule in ten thousand recovered a database to a state no
commit produced. **It is fixed** — the page allocator could rewind after a
crash landed on a wrap's sync, re-issuing page ids and letting recovery mix
two timelines; `CowBTree::adopt_next_page_id` makes the allocator monotonic
per handle, the once-failing seed passes un-`#[ignore]`d, and both DST sweeps
(10,000 schedules) are green. The full account is in `docs/architecture.md` and
`docs/recovery.md`. It is recorded here because performance numbers from an
engine whose crash safety had a known hole deserved to be read in that light —
and because the fix is what the honesty rule is for.
