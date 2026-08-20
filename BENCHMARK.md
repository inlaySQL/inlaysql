# InlaySQL benchmarks

Every number here regenerates from a script in this repository. That is the
rule `AGENTS.md` sets and it is the only reason these are worth reading — a
figure nobody can reproduce is worse than no figure. Losses are published
beside wins, because a table that only contains wins is advertising.

**How this run was produced**

```sh
LOOKUPS=50000 ./bench/run.sh      # points, indexed, joins, concurrency, vectors, retrieval
./bench/compare.sh                # DuckDB, pgvector, MySQL, PostgreSQL (needs Docker)
```

| | |
| --- | --- |
| Commit | `2ce978a` (tree dirty with doc edits only) |
| Date | 2026-08-18 |
| Machine | Apple Mac17,9, 18 cores, macOS 27.0 (Darwin 27.0.0 arm64) |
| Toolchain | rustc 1.91.1 |
| Load average when measured | 2.5–3.8 across the runs |
| Raw output | `bench/results/20260818T13*.txt`, `20260818T143239Z.txt`, `20260818T131603Z-compare.txt` |

One developer machine. Reproduce it; do not trust it.

---

## Against SQLite

SQLite is measured in two configurations because they are two different
promises. `journal` + `synchronous=FULL` + `fullfsync` is the like-for-like
column: it is the only one that makes a durability claim comparable to ours,
and `fullfsync` is what makes a macOS number mean anything at all. WAL +
`synchronous=NORMAL` is SQLite at its fastest, and is the harder target.

### Point reads by primary key — we win both

20,000 rows, 50,000 lookups, prepared statements on both sides.

| Engine | ops/s | p50 | p95 |
| --- | --- | --- | --- |
| **InlaySQL** | **1,662,494** | **500 ns** | **625 ns** |
| SQLite, WAL + `sync=NORMAL` | 1,165,810 | 833 ns | 958 ns |
| SQLite, journal + `sync=FULL` | 305,370 | 3.08 µs | 3.83 µs |

**5.4x** the durable configuration and **1.43x** the fastest one. The page
cache (AHL-420) is what did this; the caveat from the previous run still
holds: this is a *warm* cache, and a cold handle warms more slowly than
SQLite's because our miss path is dearer.

### Secondary-index reads — point win, range loss

20,000 rows, `CREATE INDEX` on a non-key TEXT column, 50,000 point lookups and
100 range queries of 50 rows (`SUITE=indexed`, new in AHL-470).

| Engine | point ops/s | point p50 | range ops/s | range p50 |
| --- | --- | --- | --- | --- |
| **InlaySQL (B-tree index)** | **499,740** | **1.79 µs** | 48,915 | 19.25 µs |
| InlaySQL (no index: full scan) | 330 | 3.02 ms | 244 | 3.94 ms |
| SQLite, journal (index) | 223,877 | 4.33 µs | **112,915** | **8.42 µs** |
| SQLite, WAL (index) | 667,768 | 1.33 µs | **179,641** | **5.29 µs** |

The index itself is worth **~1,500x** over our own full scan (AHL-423). On
point probes we beat journal-mode SQLite 2.2x and sit 1.34x behind WAL-mode.
**Range scans we lose outright — 2.3x behind journal, 3.7x behind WAL.** The
entry-walk plus per-row fetch overhead is the suspect, and it is the same
family as the join loss below.

### Joins — we lose, and now it is measured

20,000 users × 160,000 posts, identical schema and indexes on both sides
(`SUITE=joins`, new in AHL-470). The index nested-loop join (AHL-464) beat our
own previous executor 6.6–100x; against SQLite it is not enough:

| Query shape | InlaySQL | SQLite journal | vs journal |
| --- | --- | --- | --- |
| PK inner, full join | 71.5 ms | 9.37 ms | **7.6x slower** |
| PK inner, LIMIT 10 | 6.75 µs | 3.54 µs | 2.3x slower |
| Secondary-index inner, full | 166.5 ms | 14.8 ms | **11.4x slower** |
| Secondary-index inner, LIMIT 10 | 13.4 µs | 3.83 µs | 3.8x slower |

Published because it is true. This is the top open performance target; the
LIMIT rows show the streaming pipeline's short-circuit working (the gap
narrows from 11x to 3.8x when the scan can stop), so the cost is per-row, not
per-query.

### Durable writes — we win, wider than before

One row per commit, one `fsync` per commit.

| Engine | ops/s | p50 |
| --- | --- | --- |
| **InlaySQL** | **241** | **3.99 ms** |
| SQLite, journal + `sync=FULL` + `fullfsync` | 93 | 10.86 ms |

**2.6x** (was 1.84x): the commit gate no longer re-derives the log on every
commit (AHL-468), which paid on the solo path too. Batching lifts the same
workload to 32,586 ops/s at 21.8 µs — **135x** — which is the number to quote
for a bulk load and not for a transaction.

### Concurrent writers — we win, and now we scale

200 transactions per writer, one row each, on real OS threads.

| Writers | InlaySQL commits/s | SQLite commits/s |
| --- | --- | --- |
| 1 | 246 | 95 |
| 2 | 277 | 95 |
| 4 | 473 | 92 |
| 8 | **832** | 92 |

**9.0x SQLite at 8 writers, 0.0% aborted.** The previous run's honest ceiling
— 1.45x of one writer, flat from 2 up — is gone. What removed it was not group
commit alone (AHL-461 landed first and moved nothing) but the measurement
behind AHL-468: the reservation gate was held ~100% of wall clock re-deriving
committed state on every commit, so no two commits ever overlapped in the sync
window and there was nothing to batch. With the gate down to ~0.9 ms, group
commit batches most fsyncs and 8 writers do 3.4x the work of one. The
2-writer case is still fsync-bound — the follower's write usually lands after
the leader captured its flush target — and is the next thing on that path.

---

## Against `sqlite-vec` — we win

2,000 vectors, dim 384, 100 queries, top-10, recall measured against an
exhaustive oracle.

| Corpus | recall@10 | p50 | vs `sqlite-vec` |
| --- | --- | --- | --- |
| Text-derived embeddings | 1.000 | 71.17 µs | **9.4x faster at 100% of its recall** |
| Uniform random | 0.922 | 98.50 µs | 6.8x faster at 92.2% of its recall |

Both corpus shapes are published because only one of them flatters us. Uniform
random vectors in 384 dimensions have no structure for a graph index to
navigate, so recall falls and no amount of tuning fixes it. Text-derived
embeddings are what an application actually stores.

`VECTOR(n, INT8)` quantisation costs 0.014 recall on the realistic corpus and
nothing measurable on the random one, for a 1.65x smaller file and a 4x
smaller resident payload.

## Retrieval

2,000 documents, dim 384. Ingest 14,119 docs/s. Vector p50 80.75 µs; BM25 p50
315.63 µs; hybrid (vector + BM25, fused) p50 377.67 µs — **one SQL
statement**, not two queries and a client-side merge.

---

## Against DuckDB and pgvector

One corpus, one set of queries, one exhaustive ground truth, each engine asked
for its own query plan so an unindexed row cannot masquerade as an indexed one.
5,000 documents, dim 128.

| Engine | recall@10 | vector p50 | hybrid p50 |
| --- | --- | --- | --- |
| **InlaySQL** (HNSW + BM25) | 1.000 | 0.073 ms | **0.85 ms** |
| DuckDB (`vss` HNSW + `fts`) | 0.993 | 3.89 ms | 11.69 ms |
| pgvector (HNSW + `ts_rank`) | 0.989 | 0.17 ms | 14.68 ms |

**Hybrid is roughly 14–17x** the nearer baseline, because it is one statement
here and two queries plus client-side rank fusion there. That is the
comparison worth making, and it is not one query against one query — the
hybrid columns are not measuring the same amount of work, and
`bench/README.md` says so.

Vector-only, pgvector's 0.17 ms **includes a socket round trip** and is within
touching distance of our 0.073 ms in-process. Read that as close, not as a
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
| InlaySQL, host (real `F_FULLFSYNC`) | 243 | 807k |
| InlaySQL, containerised | **529** | 675k |
| MySQL 8 (`innodb_flush_log_at_trx_commit=1`, binlog off) | **1,434** | 10.8k |
| PostgreSQL 17 (`fsync=on`, `synchronous_commit=on`) | 475 | 20.6k |

**Reads: ~62x MySQL and ~33x PostgreSQL**, containerised — an in-process
library against a socket round trip. That asymmetry is structural and stated,
not hidden.

**Writes: PostgreSQL is beaten** (529 vs 475, from ~330 vs ~545–775 in the
previous run — AHL-468's cheaper commit did that). **MySQL is still 2.7x
faster.** This workload is one commit at a time on one connection, so group
commit cannot fire by design; the remaining gap is per-commit cost against
InnoDB's redo write, and closing it is scheduled work. The concurrent-writer
story (832 commits/s above) has no MySQL/PostgreSQL counterpart on this page
yet — a server-to-server concurrent row is the missing apples-to-apples.

**The server-to-server harness now exists (AHL-489) and
has no numbers in this file yet.** `bench/external/server_driver.py` drives
both `inlaysql serve --mysql` and MySQL with the same `mysql.connector`
client, at a couple of connection counts, so every row it produces pays a
socket round trip on both sides — see "Server-to-server" in
`bench/README.md` for the workload shape, the matched durability, and the
concurrency-model/credential/TLS asymmetries that remain even so (PostgreSQL
has no row there at all: InlaySQL has no PostgreSQL-wire server to compare
against it with). It was smoke-tested on a machine with other agents running
on it, which is exactly the condition this repo's own rule says makes a
number unpublishable; a real table is a `./bench/compare.sh` run on a quiet
machine away.

What this still cannot prove: Docker Desktop's virtual disk was never
independently verified to honour `fsync` as a barrier for *any* of the three
engines. "Comparable" is not "hardware-durable".

---

## What is not measured here

- **No server-to-server numbers.** Everything InlaySQL wins on reads above it
  wins partly by being in-process. The harness to benchmark `inlaysql serve
  --mysql` against MySQL over the same wire, at the same client, now exists
  (AHL-489, `bench/external/server_driver.py`, "Server-to-server" in
  `bench/README.md`) and was smoke-tested, not measured — the number that
  would back a public claim is a `./bench/compare.sh` run on a quiet machine,
  still to be done.
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
