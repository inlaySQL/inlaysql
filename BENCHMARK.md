# InlaySQL benchmarks

Every number here regenerates from a script in this repository. That is the
rule `AGENTS.md` sets and it is the only reason these are worth reading — a
figure nobody can reproduce is worse than no figure. Losses are published
beside wins, because a table that only contains wins is advertising.

**How this run was produced**

```sh
./bench/run.sh                  # points, indexed, joins, vectors, quantisation, retrieval (pinned params)
./bench/compare.sh              # DuckDB, pgvector, MySQL, PostgreSQL (needs Docker)
```

| | |
| --- | --- |
| Commit | `f8385c9` |
| Date | 2026-08-25 |
| Tree | source clean (`dirty: no` in both raw outputs) |
| Machine | Apple Mac17,9, 18 cores, macOS 27.0 (Darwin 27.0.0 arm64) |
| Toolchain | rustc 1.91.1 (ed61e7d7e 2025-11-07) |
| Raw output | `bench/results/20260825T103354Z.txt` and `20260825T104132Z.txt` (SQLite, `sqlite-vec`; two runs, median published); `20260825T110513Z-compare.txt` (DuckDB, pgvector, MySQL, PostgreSQL) |

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
| PK inner, full join | 22.95 ms → 11.47 ms | 10.03 ms → 9.68 ms | **1.20x slower** |
| PK inner, LIMIT 10 | 108.21 µs → 17.50 µs | 12.17 µs → 3.79 µs | 4.65x slower |
| Secondary-index inner, full | 26.10 ms → 3.85 ms | 16.07 ms → 14.85 ms | **3.65x faster** |
| Secondary-index inner, LIMIT 10 | 175.50 µs → 22.17 µs | 12.75 µs → 3.79 µs | 5.81x slower |

The last column is the harness's own throughput ratio (joins/s against joins/s),
which is what the raw output prints; it is close to but not identical with the
ratio of the two p50 columns beside it, because the p50 discards the cold run
the throughput figure includes.

Published because it is true, and because it keeps moving: the
secondary-index inner shape — the one AHL-464 built the index nested-loop join
for — went from **10.71x slower** (2026-08-20) to 2.85x faster (`9aba437`) to
**3.65x faster** here, and the PK inner full join from 5.56x slower to 1.43x to
**1.20x slower**. What changed across those runs is the join path (AHL-447:
streaming projection, contiguous CSR hash table, cached prepared joins,
key-only outer scans) and the borrowed page buffers (AHL-455, AHL-466). Nothing
in *this* commit touches joins, so the movement between the last edition and
this one is run-to-run variance and should be read as the width of the error
bar on these figures, not as progress.

The LIMIT rows are the standing loss and they did not improve: **4.65x and
5.81x slower** warm, against a cold column where the gap is far smaller. That
is the streaming pipeline's short-circuit working — the scan stops early — so
what remains is per-row cost, not per-query cost. The full-join shapes and
these two LIMIT shapes stay the top open performance targets.

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

## Against DuckDB and pgvector

One corpus, one set of queries, one exhaustive ground truth, each engine asked
for its own query plan so an unindexed row cannot masquerade as an indexed one.
5,000 documents, dim 128.

| Engine | recall@10 | vector p50 | hybrid p50 |
| --- | --- | --- | --- |
| **InlaySQL** (HNSW + BM25) | 1.000 | **84.00 µs** | **130.00 µs** |
| DuckDB (exhaustive + fts BM25) | 0.999 | 4.86 ms | 11.93 ms |
| DuckDB (vss HNSW + fts BM25) | 0.991 | 5.87 ms | 14.43 ms |
| pgvector (HNSW + `ts_rank`) | 0.988 | 164.00 µs | 14.24 ms |
| pgvector (exhaustive + `ts_rank`) | 0.999 | 499.00 µs | 14.42 ms |

**Hybrid is roughly 92x** the nearest baseline (11.93 ms, DuckDB exhaustive)
and **110x** pgvector, up from 14–17x two editions ago and 60x in the last one.
The BM25 index rewrite is most of that: our hybrid p50 has gone 875 µs → 191 µs
→ 130 µs while every baseline stayed inside its own noise. It is still not one
query against one query — it is one statement here against two queries plus
client-side rank fusion there — and `bench/README.md` says so plainly.

**Vector-only is now a win rather than a near-miss.** 84 µs against pgvector's
164 µs, where the last edition had us behind at 147 µs to their 198 µs. Their
number includes a socket round trip that a library in your own process does not
pay, so read it as roughly 2x with an asterisk, not as a rout — and note our own
figure was 147 µs an edition ago on the same code path, which is the spread this
page keeps warning about.

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

| Engine | Connections | write ops/s | read ops/s |
| --- | --- | --- | --- |
| **InlaySQL** (`inlaysql serve --mysql`) | 1 | 1,018.1 | **28,918.3** |
| **InlaySQL** (`inlaysql serve --mysql`) | 8 | 1,473.7 | **20,851.8** |
| MySQL 8 | 1 | 986.4 | 23,723.1 |

At one connection InlaySQL now **edges MySQL on both** — writes 1.03x
(1,018 against 986) and reads 1.22x (28,918 against 23,723) — where the last
edition had writes at 0.54x. Over three editions this row has read 1.52x, then
1.03x, then 1.22x on reads, so treat the direction as unsettled rather than as a
trend.

**Correction, on purpose: the paragraph that used to be here read the
1-to-8-connection read drop as evidence that eight connections warm eight
per-handle page caches over the same pages, and named that as the thing worth
attacking.** The drop reproduces in this run too (26,270 to 17,628 reads/s),
and so does MySQL's alongside it. A later investigation looked, and the
evidence does not support it. `inlaysql serve --mysql`'s read phase was
rebuilt on a quiet machine with the same client, same driver, same shape;
the aggregate 1-to-8 drop did not reproduce. What did reproduce, independently
in two separate runs against a live server: `mysql.connector`'s *threaded*
concurrency is GIL-bound in the Python process making the calls — eight
threads of that client measurably regress against one connection, where eight
*processes* of the identical client scale up several times over. The server
was never saturated during any of this: sampled mid-run, its threads sit in
`recvfrom`, and in-process this engine reads 2.82M points/s warm. MySQL losing
31% of its own reads across the same two rows is very likely the same client-side effect
landing on both engines, not a smaller version of a real server-side one.

None of this means the numbers above are wrong — they are what that run
measured, on that machine, with that driver, and the table stays. It means
the *explanation* attached to them was not tested before it was published,
and once tested, did not hold. A shared raw-page cache across connections on
one file was built anyway during the investigation (`docs/server.md`, "D2 —
thread-per-connection, one handle each") — real, and worth roughly 18% on the
page-miss path specifically — but it changes nothing about the 1-to-8
gap, because the per-handle cache in this benchmark's own table already holds
the whole working set warm, so nothing shared below it is ever asked. The
honest next step is a re-run with a process-based driver on a quiet machine,
not more server-side work aimed at a gap that may not be where this pointed.
Both sides use disjoint id ranges per connection to avoid conflicts; retries
are zero on both. See "Server-to-server" in `bench/README.md` for the detailed
methodology, the concurrency-model/credential/TLS asymmetries that remain, and
why PostgreSQL has no row here.

What this still cannot prove: Docker Desktop's virtual disk was never
independently verified to honour `fsync` as a barrier for *any* of the three
engines. "Comparable" is not "hardware-durable".

---

## What is not measured here

- **No server-to-server numbers with a trustworthy driver.** The server-to-server
  table above (AHL-495) is the first such run this project has had. It is
  enough to retire the older "we win reads only by being in-process" caveat —
  those rows pay a socket round trip on both sides — but the 1-to-8-connection
  read comparison specifically has since turned out to depend on the driver's
  own concurrency shape (see the correction above): a repeat needs a
  process-based client, not just a quieter machine, before that comparison
  means what it was first read to mean.
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
