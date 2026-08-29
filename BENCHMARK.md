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
| Raw output | `bench/results/20260825T103354Z.txt` and `20260825T104132Z.txt` (SQLite, `sqlite-vec`; two runs, median published); `20260825T110513Z-compare.txt` (DuckDB, pgvector, MySQL, PostgreSQL). Retrieval section regenerated 2026-08-29 (Meilisearch added): `20260829T084502Z-compare.txt` |

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

### Concurrent writers — we win, and now we scale

200 transactions per writer, one row each, on real OS threads.

| Writers | InlaySQL commits/s | SQLite commits/s |
| --- | --- | --- |
| 1 | 245 | 87 |
| 2 | 265 | 87 |
| 4 | 434 | 87 |
| 8 | **768** | 86 |

**8.9x SQLite at 8 writers, 0.0% aborted.** The 8-writer scaling (768 against
245 at one writer is 3.14x) shows group commit batching most fsyncs. The
2-writer case remains relatively flat (265 against 245), still fsync-bound —
the follower's write usually lands after the leader captured its flush target —
and is the next thing on that path.

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
read the ordering as noise and the fact that we trail both as the finding. The previous edition had
us beating PostgreSQL and within 1.08x of MySQL; our own figure improved
(723.1 → 847.2) and both servers improved more, on a run where they had eleven
unrelated containers for company and we did not control for it. So the ranking
here is real for this run and the size of the gap is not to be trusted. What is
structural, and unchanged: this workload is one commit at a time on one
connection, so group commit cannot fire by design, and the remaining cost is
per-commit against InnoDB's redo write. Closing it is scheduled work. The
concurrent-writer story (768 commits/s on 8 writers above) has no
MySQL/PostgreSQL counterpart on this page yet — a server-to-server concurrent
row is the missing apples-to-apples.

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
