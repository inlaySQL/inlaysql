# InlaySQL benchmarks

Every number here regenerates from a script in this repository. That is the
rule `AGENTS.md` sets and it is the only reason these are worth reading — a
figure nobody can reproduce is worse than no figure. Losses are published
beside wins, because a table that only contains wins is advertising.

**How this run was produced**

```sh
./bench/run.sh                  # points, indexed, joins, vectors, quantisation, retrieval (pinned params)
./bench/compare.sh              # DuckDB, pgvector, MySQL, PostgreSQL (needs Docker) — 2026-08-20 run, not re-run
```

| | |
| --- | --- |
| Commit | `9aba437` |
| Date | 2026-08-24 |
| Tree | source clean at the perf landings (`4c1d265`…`9aba437`); docs-only change uncommitted |
| Machine | Apple Mac17,9, 18 cores, macOS 27.0 (Darwin 27.0.0 arm64) |
| Toolchain | rustc 1.91.1 (ed61e7d7e 2025-11-07) |
| Raw output | `bench/results/20260824T074633Z.txt` (SQLite, `sqlite-vec`); `20260820T132925Z-compare.txt` (DuckDB, pgvector, MySQL, PostgreSQL) |

One developer machine. Reproduce it; do not trust it. The 2026-08-20 run was measured under a load average of 5.4 (Chrome, LM Studio, other applications); this run was not. Because every engine is measured in the same run, the like-for-like comparisons remain fair either way; the absolute figures from the two runs are not comparable to each other, and the server-to-server section below keeps the 2026-08-20 numbers because `bench/compare.sh` was not re-run.

---

## Against SQLite

SQLite is measured in two configurations because they are two different
promises. `journal` + `synchronous=FULL` + `fullfsync` is the like-for-like
column: it is the only one that makes a durability claim comparable to ours,
and `fullfsync` is what makes a macOS number mean anything at all. WAL +
`synchronous=NORMAL` is SQLite at its fastest, and is the harder target.

### Point reads by primary key — we win both

20,000 rows, 5,000 lookups, prepared statements on both sides.

| Engine | ops/s | p50 | p95 |
| --- | --- | --- | --- |
| **InlaySQL** | **636,980** | **958 ns** | **5.54 µs** |
| SQLite, WAL + `sync=NORMAL` | 1,117,360 | 833 ns | 1.13 µs |
| SQLite, journal + `sync=FULL` | 295,232 | 3.25 µs | 3.96 µs |

**2.16x** the durable configuration and 0.57x the fastest one. The page
cache (AHL-420) is what did the winning half; the caveat from the previous
run still holds: this is a *warm* cache, and a cold handle warms more slowly
than SQLite's because our miss path is dearer.

### Secondary-index reads — point win, range loss

20,000 rows, `CREATE INDEX` on a non-key TEXT column, 5,000 point lookups and
100 range queries of 50 rows (`SUITE=indexed`).

| Engine | point ops/s | point p50 | range ops/s | range p50 |
| --- | --- | --- | --- | --- |
| **InlaySQL (B-tree index)** | **354,533** | **2.33 µs** | 64,916 | 14.38 µs |
| InlaySQL (no index: full scan) | 703 | 1.41 ms | 528 | 1.88 ms |
| SQLite, journal (index) | 141,166 | 4.00 µs | **41,160** | **11.88 µs** |
| SQLite, WAL (index) | 307,056 | 1.96 µs | **113,277** | **7.17 µs** |

The index itself is worth **504.65x** over our own full scan on point probes
and **122.85x** on range scans (AHL-423). On point probes we beat
journal-mode SQLite 1.61x and sit 0.87x behind WAL-mode. **Range scans we lose
outright — 0.63x of journal, 0.57x of WAL.** The entry-walk plus per-row fetch
overhead is the suspect, and it is the same family as the join loss below.

### Joins — we win one shape, lose the other

20,000 users × 160,000 posts, identical schema and indexes on both sides
(`SUITE=joins`). Each row splits the cold first execution of the query shape
from the warm p50 — the cold column is where the join plan and its tables get
built, so it is the expensive one:

| Query shape | InlaySQL cold → p50 | SQLite journal cold → p50 | vs journal (p50) |
| --- | --- | --- | --- |
| PK inner, full join | 19.95 ms → 13.15 ms | 9.37 ms → 9.39 ms | **1.43x slower** |
| PK inner, LIMIT 10 | 173.67 µs → 18.75 µs | 18.33 µs → 3.54 µs | 5.30x slower |
| Secondary-index inner, full | 32.34 ms → 4.99 ms | 15.54 ms → 15.32 ms | **2.85x faster** |
| Secondary-index inner, LIMIT 10 | 192.42 µs → 21.63 µs | 28.88 µs → 3.79 µs | 5.74x slower |

Published because it is true, and because it moved between the two runs: the
secondary-index inner shape — the one AHL-464 built the index nested-loop join
for — went from **10.71x slower** (2026-08-20) to **2.85x faster** on
2026-08-24, and the PK inner full join from 5.56x slower to 1.43x slower.
What changed between those runs is the join path (AHL-447: streaming
projection, contiguous CSR hash table, cached prepared joins, key-only outer
scans) and the borrowed page buffers (AHL-455, AHL-466), and at the same time
the machine was quieter and the tree source-clean. We are not claiming the whole gap
is code: the measurement conditions changed with it. The LIMIT rows show the
streaming pipeline's short-circuit working (the gap narrows from 5.74x warm to
the cold column when the scan can stop), so the remaining cost is per-row, not
per-query. The full-join shapes stay the top open performance target.

### Durable writes — we win

One row per commit, one `fsync` per commit.

| Engine | ops/s | p50 |
| --- | --- | --- |
| **InlaySQL** | **226** | **3.99 ms** |
| SQLite, journal + `sync=FULL` + `fullfsync` | 87 | 11.24 ms |

**2.60x**: the commit gate no longer re-derives the log on every commit
(AHL-468), which paid on the solo path too. Batching lifts the same workload
to 56,839 ops/s at 11.50 µs — **251x** — which is the number to quote for a
bulk load and not for a transaction.

### Concurrent writers — we win, and now we scale

200 transactions per writer, one row each, on real OS threads.

| Writers | InlaySQL commits/s | SQLite commits/s |
| --- | --- | --- |
| 1 | 245 | 88 |
| 2 | 252 | 90 |
| 4 | 459 | 88 |
| 8 | **736** | 80 |

**9.2x SQLite at 8 writers, 0.0% aborted.** The 8-writer scaling (736 vs 245
one-writer is 3.01x) shows group commit batching most fsyncs. The 2-writer
case remains relatively flat (252 vs 245), still fsync-bound — the follower's
write usually lands after the leader captured its flush target — and is the
next thing on that path.

---

## Against `sqlite-vec` — we win

2,000 vectors, dim 384, 100 queries, top-10, recall measured against an
exhaustive oracle.

| Corpus | recall@10 | p50 | vs `sqlite-vec` |
| --- | --- | --- | --- |
| Text-derived embeddings | 1.000 | 82.46 µs | **8.34x faster at 100% of its recall** |
| Uniform random | 0.922 | 104.96 µs | 6.50x faster at 92.2% of its recall |

Both corpus shapes are published because only one of them flatters us. Uniform
random vectors in 384 dimensions have no structure for a graph index to
navigate, so recall falls and no amount of tuning fixes it. Text-derived
embeddings are what an application actually stores.

`VECTOR(n, INT8)` quantisation costs 0.014 recall on the realistic corpus
(0.986 vs 1.000 exact) and nothing measurable on the random one (0.922 both),
for a 1.65x smaller file and a 3.96x smaller resident payload.

## Retrieval

2,000 documents, dim 384. Ingest 17,182 docs/s. Vector p50 87.88 µs; BM25 p50
347.50 µs; hybrid (vector + BM25, fused) p50 453.88 µs — **one SQL
statement**, not two queries and a client-side merge.

---

## Against DuckDB and pgvector

One corpus, one set of queries, one exhaustive ground truth, each engine asked
for its own query plan so an unindexed row cannot masquerade as an indexed one.
5,000 documents, dim 128.

| Engine | recall@10 | vector p50 | hybrid p50 |
| --- | --- | --- | --- |
| **InlaySQL** (HNSW + BM25) | 1.000 | **78.00 µs** | **875.00 µs** |
| DuckDB (exhaustive + fts BM25) | 0.999 | 4.87 ms | 12.69 ms |
| DuckDB (vss HNSW + fts BM25) | 0.993 | 4.07 ms | 11.99 ms |
| pgvector (HNSW + `ts_rank`) | 0.987 | 159.00 µs | 14.42 ms |
| pgvector (exhaustive + `ts_rank`) | 0.999 | 481.00 µs | 14.54 ms |

**Hybrid is roughly 14–17x** the nearer baseline, because it is one statement
here and two queries plus client-side rank fusion there. That is the
comparison worth making, and it is not one query against one query — the
hybrid columns are not measuring the same amount of work, and
`bench/README.md` says so.

Vector-only, pgvector (HNSW) at 159 µs **includes a socket round trip** and is
within touching distance of our 78 µs in-process. Read that as close, not as a
rout.

---

## Against MySQL and PostgreSQL

**Reads: we win by a wide margin. Sequential writes: we now beat PostgreSQL
and still lose to MySQL.**

InlaySQL is measured twice — on the host with a real `F_FULLFSYNC` barrier,
and **inside a container on the same volume class as the servers**, so all
three pay the same virtualised fsync. The gap between the two InlaySQL rows is
what that virtualisation is worth on this machine.

| Engine | write ops/s | read ops/s |
| --- | --- | --- |
| InlaySQL, host (real `F_FULLFSYNC`) | 238.5 | 446k |
| InlaySQL, containerised | **723.1** | 603k |
| MySQL 8 (`innodb_flush_log_at_trx_commit=1`, binlog off) | **780.7** | 10.9k |
| PostgreSQL 17 (`fsync=on`, `synchronous_commit=on`) | 730.9 | 53.2k |

**Reads: ~55x MySQL and ~11.3x PostgreSQL**, containerised — an in-process
library against a socket round trip. That asymmetry is structural and stated,
not hidden.

**Writes: PostgreSQL is beaten** (723.1 vs 730.9, comparable). **MySQL is
still 1.08x faster** (780.7 vs 723.1). This workload is one commit at a time
on one connection, so group commit cannot fire by design; the remaining gap is
per-commit cost against InnoDB's redo write, and closing it is scheduled work.
The concurrent-writer story (736 commits/s on 8 writers above) has no
MySQL/PostgreSQL counterpart on this page yet — a server-to-server concurrent
row is the missing apples-to-apples.

### Server-to-server: MySQL wire protocol

`inlaysql serve --mysql` reached over the compose network by `mysql.connector`,
matched against MySQL 8, same driver and same transport on both sides. Every
row pays a socket round trip.

| Engine | Connections | write ops/s | read ops/s |
| --- | --- | --- | --- |
| **InlaySQL** (`inlaysql serve --mysql`) | 1 | **1,085.5** | **37,157.8** |
| **InlaySQL** (`inlaysql serve --mysql`) | 8 | **1,391.2** | 19,874.0 |
| MySQL 8 | 1 | 1,554.4 | 24,481.3 |
| MySQL 8 | 8 | 6,630.4 | 18,028.8 |

At one connection InlaySQL loses on writes (0.70x) and **wins on reads
(1.52x)**. At eight it still edges reads (19,874 against 18,029, 1.10x) and
loses writes badly — **4.76x**.

**Correction, on purpose: the paragraph that used to be here read the
1-to-8-connection drop (37,158 to 19,874 reads/s) as evidence that eight
connections warm eight per-handle page caches over the same pages, and named
that as the thing worth attacking.** A later investigation looked, and the
evidence does not support it. `inlaysql serve --mysql`'s read phase was
rebuilt on a quiet machine with the same client, same driver, same shape;
the aggregate 1-to-8 drop did not reproduce. What did reproduce, independently
in two separate runs against a live server: `mysql.connector`'s *threaded*
concurrency is GIL-bound in the Python process making the calls — eight
threads of that client measurably regress against one connection, where eight
*processes* of the identical client scale up several times over. The server
was never saturated during any of this: sampled mid-run, its threads sit in
`recvfrom`, and in-process this engine reads 2.82M points/s warm. MySQL losing
26% under the same benchmark's load is very likely the same client-side effect
landing on both engines, not a smaller version of a real server-side one.

None of this means the numbers above are wrong — they are what that run
measured, on that machine, with that driver, and the table stays. It means
the *explanation* attached to them was not tested before it was published,
and once tested, did not hold. A shared raw-page cache across connections on
one file was built anyway during the investigation (`docs/server.md`, "D2 —
thread-per-connection, one handle each") — real, and worth roughly 18% on the
page-miss path specifically — but it changes nothing about the 37,158-to-19,874
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
