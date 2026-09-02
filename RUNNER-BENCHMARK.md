# Runner benchmarks — trend tracking, not published figures

Machine: a GitHub-hosted `ubuntu-latest` runner (4 shared vCPUs, Docker
for the container rows). These numbers are **not** the published
benchmarks — those come from load-gated runs on a quiet machine, per
`BENCHMARK.md` and `PERF.md` §4's A/A floor, which a shared runner
cannot meet. What these runs are for: catching regressions between
runner generations and across commits, on one consistent (if modest)
machine class, for free.

Read two of these against each other, never one of them against
`BENCHMARK.md`. A run-to-run swing under 20% on this machine class is
noise, not signal.

- generated: 2026-09-02T05:44:41Z
- commit: 4e1bb96
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-09-02T05:37:34Z
commit: 4e1bb96
dirty:  no
rustc:  rustc 1.98.0 (88d9e12ae 2026-08-18)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T053445Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T053641Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T053708Z.txt

metrics: 46; disagreeing by 10% or more across runs: 18

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  321.9%         max       22.64µs       17.41µs       90.29µs  SQLite (WAL, sync=NORMAL)
   78.5%         max       30.57µs       26.80µs       50.79µs  SQLite (journal, sync=FULL, fullfsync)
   54.9%         p99        4.50µs        4.34µs        6.81µs  SQLite (WAL, sync=NORMAL)
   43.0%         max        8.68ms        5.13ms        8.86ms  SQLite (journal, sync=FULL, fullfsync)
   43.0%         max        8.68ms        5.13ms        8.86ms  SQLite (journal, sync=FULL, fullfsync)
   22.1%         p50        1.31µs        1.21µs        1.50µs  InlaySQL
   16.0%         p95      774.45µs      751.32µs      875.06µs  InlaySQL
   16.0%         p95      774.45µs      751.32µs      875.06µs  InlaySQL
   14.3%         p95      853.43µs      777.92µs      900.02µs  SQLite (journal, sync=FULL, fullfsync)
   14.3%         p95      853.43µs      777.92µs      900.02µs  SQLite (journal, sync=FULL, fullfsync)
   14.1%         p99        1.14ms        0.99ms        1.15ms  SQLite (journal, sync=FULL, fullfsync)
   14.1%         p99        1.14ms        0.99ms        1.15ms  SQLite (journal, sync=FULL, fullfsync)
   14.0%         max        2.79ms        2.58ms        2.97ms  SQLite (WAL, sync=NORMAL)
   13.3%       ops/s        485472        460933        525502  InlaySQL
   11.1%         p50      544.19µs      502.94µs      563.16µs  InlaySQL
   11.1%         p50      544.19µs      502.94µs      563.16µs  InlaySQL
   10.6%       ops/s          1678          1624          1802  InlaySQL
   10.6%       ops/s          1678          1624          1802  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 5000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         1678   544.19µs   774.45µs     2.88ms     4.10ms
SQLite (journal, sync=FULL, fullfsync)           1456   659.22µs   853.43µs     1.14ms     8.68ms
SQLite (WAL, sync=NORMAL)                       65749    12.13µs    13.97µs    22.64µs     2.79ms
InlaySQL is 1.17x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                              25192    28.60µs    35.89µs    41.25µs    64.03ms
InlaySQL                                         1678   544.19µs   774.45µs     2.88ms     4.10ms
SQLite (journal, sync=FULL, fullfsync)           1456   659.22µs   853.43µs     1.14ms     8.68ms
InlaySQL (batched) is 15.34x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                       485472     1.31µs     6.76µs     10.12µs    45.57µs
SQLite (journal, sync=FULL, fullfsync)          92513    10.59µs    11.19µs    18.22µs    30.57µs
SQLite (WAL, sync=NORMAL)                      271595     3.50µs     4.09µs     4.50µs    22.64µs
InlaySQL is 5.68x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-09-02T05:38:42Z
commit: 4e1bb96
dirty:  no
rustc:  rustc 1.98.0 (88d9e12ae 2026-08-18)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T053734Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T053756Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T053819Z.txt

metrics: 42; disagreeing by 10% or more across runs: 11

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   49.7%         max       40.63µs       29.06µs       49.24µs  SQLite (WAL, sync=NORMAL) (index)
   43.4%         max       52.38µs       52.34µs       75.07µs  InlaySQL (B-tree index)
   27.8%         max       40.07µs       30.98µs       42.12µs  SQLite (journal, sync=FULL, fullfsync) (index)
   26.6%         max       24.66µs       24.16µs       30.71µs  SQLite (WAL, sync=NORMAL) (index)
   25.3%         p99        5.70ms        5.63ms        7.07ms  InlaySQL (no index: full scan)
   24.9%         max        5.71ms        5.68ms        7.10ms  InlaySQL (no index: full scan)
   23.0%         max        7.16ms        6.12ms        7.77ms  InlaySQL (no index: full scan)
   21.0%         p99        4.24ms        4.07ms        4.96ms  InlaySQL (no index: full scan)
   15.6%         p95        5.63ms        5.61ms        6.49ms  InlaySQL (no index: full scan)
   11.1%         p95       43.35µs       41.48µs       46.28µs  InlaySQL (B-tree index)
   10.2%       ops/s       137.75x       129.27x       143.29x  InlaySQL (B-tree index) is faster than InlaySQL (no index: full scan)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 5000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              158850     5.74µs     9.22µs    12.25µs    53.59µs
InlaySQL (no index: full scan)                          248     4.00ms     4.08ms     4.24ms     7.16ms
SQLite (journal, sync=FULL, fullfsync) (index)        81415    11.68µs    14.10µs    20.47µs    40.07µs
SQLite (WAL, sync=NORMAL) (index)                    192510     4.47µs     7.38µs     8.09µs    40.63µs
InlaySQL (B-tree index) is 637.17x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               24710    38.06µs    43.35µs    50.38µs    52.38µs
InlaySQL (no index: full scan)                          179     5.57ms     5.63ms     5.70ms     5.71ms
SQLite (journal, sync=FULL, fullfsync) (index)        43552    22.25µs    25.74µs    30.87µs    34.22µs
SQLite (WAL, sync=NORMAL) (index)                     62006    15.42µs    19.02µs    23.91µs    24.66µs
InlaySQL (B-tree index) is 137.75x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-09-02T05:41:13Z
commit: 4e1bb96
dirty:  no
rustc:  rustc 1.98.0 (88d9e12ae 2026-08-18)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T053842Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T053934Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T054023Z.txt

metrics: 74; disagreeing by 10% or more across runs: 18

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   84.3%         p99       14.71ms       14.31ms       26.71ms  InlaySQL
   83.3%         p99       12.10µs        5.72µs       15.80µs  SQLite (WAL, sync=NORMAL) (index)
   68.3%         max       14.35µs       13.34µs       23.14µs  SQLite (WAL, sync=NORMAL) (index)
   36.8%         p95       14.10ms       13.66ms       18.85ms  InlaySQL
   35.7%         p99       18.89µs       15.81µs       22.55µs  SQLite (WAL, sync=NORMAL) (index)
   30.3%        cold       23.55µs       22.71µs       29.85µs  SQLite (journal, sync=FULL, fullfsync) (index)
   30.3%         max       23.55µs       22.71µs       29.85µs  SQLite (journal, sync=FULL, fullfsync) (index)
   29.6%         max       29.97ms       28.92ms       37.78ms  SQLite (WAL, sync=NORMAL) (index)
   29.0%         p95       19.18µs       17.56µs       23.12µs  InlaySQL
   25.8%        cold       14.35µs       12.10µs       15.80µs  SQLite (WAL, sync=NORMAL) (index)
   18.5%         p99       15.69ms       15.60ms       18.50ms  InlaySQL
   16.4%        cold      101.42µs       90.91µs      107.50µs  InlaySQL
   16.4%         max      101.42µs       90.91µs      107.50µs  InlaySQL
   15.6%         max       23.30µs       22.81µs       26.45µs  SQLite (WAL, sync=NORMAL) (index)
   15.4%         max       36.06µs       34.49µs       40.05µs  SQLite (journal, sync=FULL, fullfsync) (index)
   15.4%        cold       36.06µs       34.49µs       40.05µs  SQLite (journal, sync=FULL, fullfsync) (index)
   15.1%         p99       29.58µs       27.84µs       32.30µs  InlaySQL
   14.1%         p99       28.84ms       28.78ms       32.86ms  SQLite (WAL, sync=NORMAL) (index)

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 76     53.30ms    12.68ms    14.10ms    14.71ms    53.30ms
SQLite (journal, sync=FULL, fullfsync) (index)           35     27.91ms    28.26ms    28.52ms    29.45ms    29.86ms
SQLite (WAL, sync=NORMAL) (index)                        35     28.23ms    28.49ms    28.71ms    28.84ms    29.97ms
InlaySQL is 2.17x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              56513   101.42µs    16.32µs    19.18µs    29.58µs   101.42µs
SQLite (journal, sync=FULL, fullfsync) (index)        80042    23.55µs    12.14µs    12.62µs    20.85µs    23.55µs
SQLite (WAL, sync=NORMAL) (index)                    190317    14.35µs     4.88µs     5.11µs    12.10µs    14.35µs
InlaySQL is 1.41x slower than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 67     89.18ms    14.03ms    15.03ms    15.69ms    89.18ms
SQLite (journal, sync=FULL, fullfsync) (index)           13     77.13ms    76.88ms    77.26ms    77.86ms    78.94ms
SQLite (WAL, sync=NORMAL) (index)                        13     78.26ms    77.85ms    78.30ms    79.05ms    79.76ms
InlaySQL is 5.12x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              37115   194.37µs    24.21µs    32.23µs    45.39µs   194.37µs
SQLite (journal, sync=FULL, fullfsync) (index)        66582    36.06µs    14.58µs    15.09µs    23.84µs    36.06µs
SQLite (WAL, sync=NORMAL) (index)                    126851    22.81µs     7.55µs     7.99µs    18.89µs    23.30µs
InlaySQL is 1.81x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-09-02T05:41:23Z
commit: 4e1bb96
dirty:  no
rustc:  rustc 1.98.0 (88d9e12ae 2026-08-18)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T054113Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T054116Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260902T054120Z.txt

metrics: 68; disagreeing by 10% or more across runs: 16

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  371.5%         max        1.79ms        1.77ms        8.42ms  SQLite (journal, sync=FULL, fullfsync)
   69.8%         p99      982.04µs      854.52µs     1540.00µs  SQLite (journal, sync=FULL, fullfsync)
   53.4%         p95        1.01ms        0.81ms        1.35ms  InlaySQL (parallel WAL regions)
   52.6%         p99        0.85ms        0.75ms        1.20ms  SQLite (journal, sync=FULL, fullfsync)
   28.9%         max        1.80ms        1.34ms        1.86ms  SQLite (journal, sync=FULL, fullfsync)
   25.4%         p99        0.93ms        0.85ms        1.09ms  SQLite (journal, sync=FULL, fullfsync)
   24.8%         p95      822.85µs      765.61µs      969.99µs  SQLite (journal, sync=FULL, fullfsync)
   23.7%         p99        1.07ms        0.86ms        1.11ms  SQLite (journal, sync=FULL, fullfsync)
   21.2%         p95        2.17ms        1.88ms        2.34ms  InlaySQL (parallel WAL regions)
   18.4%         p99        3.81ms        3.57ms        4.27ms  InlaySQL (parallel WAL regions)
   16.9%         max        6.64ms        6.32ms        7.44ms  InlaySQL (parallel WAL regions)
   16.8%         p95      585.81µs      566.75µs      665.42µs  InlaySQL (parallel WAL regions)
   13.4%         max       32.70ms       30.05ms       34.43ms  InlaySQL (parallel WAL regions)
   12.4%     writers         1.21x         1.09x         1.24x  InlaySQL at writers does the work of writer, aborting of transactions.
   12.1%         p99       30.98ms       27.90ms       31.66ms  InlaySQL (parallel WAL regions)
   10.8%   commits/s          1984          1904          2118  InlaySQL (parallel WAL regions)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         1984          200       0.00%   482.67µs   585.81µs     1.86ms     2.05ms
InlaySQL (parallel WAL regions)                 2         2534          400       0.00%   657.05µs     1.01ms     3.81ms     6.64ms
InlaySQL (parallel WAL regions)                 4         2443          800       0.00%     1.06ms     2.17ms    20.88ms    32.70ms
InlaySQL (parallel WAL regions)                 8         2362         1600       0.00%     1.81ms    15.99ms    30.98ms    41.45ms
SQLite (journal, sync=FULL, fullfsync)          1         1524          200       0.00%   636.34µs   717.02µs     0.85ms     1.56ms
SQLite (journal, sync=FULL, fullfsync)          2         1483          400       0.00%   653.41µs   788.63µs     1.07ms     1.80ms
SQLite (journal, sync=FULL, fullfsync)          4         1473          800       0.00%   659.10µs   788.22µs     0.93ms     1.60ms
SQLite (journal, sync=FULL, fullfsync)          8         1453         1600       0.00%   667.19µs   822.85µs   982.04µs     1.79ms

InlaySQL at 8 writers does 1.21x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-09-02T05:44:08Z
commit: 4e1bb96
dirty:  no
rustc:  rustc 1.98.0 (88d9e12ae 2026-08-18)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  185.00us  393.00us |   0.988  298.00us  356.00us     5.9s
DuckDB (exhaustive + fts BM25)         1.000   16.65ms   20.35ms |   0.966   34.11ms   44.99ms    76.9s
DuckDB (vss HNSW + fts BM25)           0.993   15.02ms   22.18ms |   0.956   32.90ms   44.54ms    77.5s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.998    3.12ms    3.62ms |   0.418   10.09ms   14.81ms     3.9s
pgvector (HNSW + ts_rank)              0.988  428.00us  622.00us |   0.456   36.88ms   55.50ms     1.5s
pgvector (exhaustive + ts_rank)        0.999    1.25ms    1.33ms |   0.465   38.04ms   58.28ms     0.5s

recall@k is measured against exhaustive cosine similarity — an objective answer.
`agree` is overlap with InlaySQL's reference fusion (exact vector + exact BM25).
It is an agreement measure, not a quality score: an engine that ranks text with a
different function scores lower without being worse. Read the latencies as the result.

The hybrid columns are not measuring the same amount of work. InlaySQL fuses inside
one SQL statement; the baselines have no fusion operator, so their driver runs two
queries and combines the ranks in Python. That is what using them for hybrid search
costs today, which is the comparison worth making — but it is not one query against
one query.

  InlaySQL (HNSW + BM25): one process, no server; embeddings are hashed bag-of-words, so text and vector agree and hybrid means something — easier for ANN than the random vectors in the `vectors` suite
  DuckDB (exhaustive + fts BM25): in-process, no server; exhaustive scan, so any recall shortfall is tie-breaking, not approximation; plan: sequential scan
  DuckDB (vss HNSW + fts BM25): in-process; approximate index, like-for-like against InlaySQL's HNSW; plan: HNSW index scan
  Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver): client/server: latency includes a round trip; single ANN configuration, no exhaustive-scan option in the search API; text ranked by Meilisearch's own rule chain, not BM25; hybrid fusion is this driver's own RRF (common.rrf), not Meilisearch's built-in semanticRatio blend, so every engine in this comparison is fused the same way
  pgvector (HNSW + ts_rank): client/server: latency includes a round trip; approximate index, like-for-like vs ours; plan: HNSW index scan
  pgvector (exhaustive + ts_rank): client/server: latency includes a round trip; exhaustive, so a recall shortfall is tie-breaking; text ranked by ts_rank_cd, not BM25; plan: sequential scan


=== OLTP: 20000 rows, 5000 lookups by primary key, seed 42 ===

                                                                 --- write (durable, one row/commit) --- |      --- read (point lookup) ---      
engine                                                           write ops/s      p50      p95      p99 |  read ops/s      p50      p95      p99
------------------------------------------------------------------------------------------------------------------------------------------------
InlaySQL                                                              1369.6  660.00us    1.12ms    3.23ms |    567695.0    1.00us    5.00us    8.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       1277.8  686.00us    1.37ms    3.23ms |    585403.9    1.00us    5.00us    7.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           2326.1  396.00us  548.00us  983.00us |      3889.8  244.00us  304.00us  343.00us
                                                                   commits-per-fsync: 20003/20720 = 0.97
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       3964.1  244.00us  308.00us  400.00us |      7747.2  125.00us  159.00us  192.00us
                                                                   commits-per-fsync: 20010/20002 = 1.00

Every row here is configured for real durability — fsync on every commit — matched
as closely as each engine allows. See bench/README.md for the exact settings and the
cases that could not be made genuinely comparable.

MySQL and PostgreSQL are servers reached over the compose network: every number here
includes a client/server round trip that InlaySQL, a library in the caller's own
process, does not pay. That asymmetry biases every server row toward looking slower
than it would be over a faster transport than a Docker bridge.

InlaySQL is measured twice. The first row runs on the host and fsyncs to the real
disk — F_FULLFSYNC on macOS, a genuine barrier — exactly like the points suite. The
second, containerised row runs inside this same compose network, off the same Linux
build docker/test.sh produces, with its database file on a named Docker volume of the
same class postgres-oltp-data and mysql-oltp-data are, so its fsync crosses whatever
boundary theirs does. That is what makes the *containerised* row comparable to the
MySQL/PostgreSQL rows, and the gap between the two InlaySQL rows is a direct
measurement, on this machine, of what that virtualised fsync costs.

What this does and does not prove: comparable is not the same as hardware-durable —
on Docker Desktop for macOS/Windows none of the three server rows' commits are proven
durable to the platter, only to whatever the virtualised disk promises, and every
engine here now pays that same promise rather than two of the three paying it and one
not. What does not disappear: InlaySQL stays in-process even in its own container, so
it still does not pay the socket round trip MySQL and PostgreSQL do — read the
containerised row as the fsync asymmetry removed, not the transport asymmetry, which
is structural. See bench/README.md for the full accounting.

  InlaySQL: in-process, no server, so this number pays no client/server round trip the MySQL/ PostgreSQL rows do; one durable commit per statement (no batching), matched to MySQL's innodb_flush_log_at_trx_commit=1 and PostgreSQL's fsync=on / synchronous_commit=on rows here; measured on the host filesystem, so its fsync is whatever barrier the host honours — see bench/README.md for the full durability rationale, the containerised row below it, and the asymmetries that remain
  InlaySQL (containerised, same volume class as MySQL/PostgreSQL): runs inside the same compose network and off the same docker/Dockerfile image as MySQL and PostgreSQL, and its database file lives on a named Docker volume like theirs rather than a host bind mount, so its fsync crosses the same virtualised-disk boundary theirs does; still in-process, so it pays no client/server round trip — that asymmetry is structural and remains. Compare against the host InlaySQL row to see what the virtualised fsync itself costs on this machine — see bench/README.md
  MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled): client/server over the compose network: every number here includes a round trip InlaySQL does not pay; autocommit, so every statement is its own durable transaction, matched to InlaySQL's non-batched write and to the points suite's SQLite journal/sync=FULL/fullfsync row — see bench/README.md. commit_stats is the delta of Handler_commit/Innodb_os_log_fsyncs bracketing the write phase; at one connection expect ~1.0 (nothing to batch with) — see SCOREBOARD.md §6
  PostgreSQL 17 (fsync=on, synchronous_commit=on): client/server over the compose network: every number here includes a round trip InlaySQL does not pay; autocommit, so every statement is its own durable transaction, matched to InlaySQL's non-batched write and to the points suite's SQLite journal/sync=FULL/fullfsync row — see bench/README.md. commit_stats is the delta of pg_stat_database.xact_commit/pg_stat_wal.wal_sync bracketing the write phase; at one connection expect ~1.0 (nothing to batch with) — see SCOREBOARD.md §6


=== server-to-server: 20000 rows, 5000 lookups by primary key, seed 42 — mysql.connector on both sides ===

                                                                                    --- write (durable, one row/commit) ---         |      --- read (point lookup) ---      
engine                                                                         conn write ops/s      p50      p95      p99 retries |  read ops/s      p50      p95      p99
---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1080.2  804.00us    1.16ms    2.03ms       0 |      2464.9  221.00us  232.00us  256.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2041/2041 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      1533.7    1.48ms    8.24ms   15.98ms       0 |      2568.5  250.00us  432.00us  532.00us
                                                                                      commits-per-fsync: 2026/861 = 2.35
                                                                                      commits-per-fsync (checkpoint-inclusive): 2038/873 = 2.33
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16       834.8    7.33ms   32.66ms   46.45ms       0 |       697.2  269.00us    3.32ms    6.46ms
                                                                                      commits-per-fsync: 2026/538 = 3.77
                                                                                      commits-per-fsync (checkpoint-inclusive): 2050/557 = 3.68
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2099.1  382.00us  513.00us  846.00us       0 |      2088.1  284.00us  295.00us  317.00us
                                                                                      commits-per-fsync: 2003/2072 = 0.97
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      3934.9  602.00us    1.05ms    1.52ms       0 |      2073.6  368.00us  555.00us  718.00us
                                                                                      commits-per-fsync: 2003/1342 = 1.49
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      1837.4  789.00us    2.75ms    4.80ms       0 |       597.5  314.00us    2.94ms    4.67ms
                                                                                      commits-per-fsync: 2003/1593 = 1.26

This is the row bench/README.md calls the missing apples-to-apples number: InlaySQL
here is never a library call, it is `inlaysql serve --mysql`, reached over the compose
network by the same mysql.connector client code path that reaches MySQL above — every
number in this table, on every row, pays an identical socket round trip.

What still is not comparable, even here. inlaysql-server is thread-per-connection, one
OS thread and one Database handle per connection with no thread pool; MySQL schedules
connections onto a bounded worker pool — a structural difference in what adding a
connection costs each engine, not a tuning gap, so read a widening gap at the higher
concurrency level that way rather than as a regression. Both sides share one user and
one password as configured here, but InlaySQL has no user table, no grants and no
per-table permissions at all, a capability gap this benchmark does not exercise either
way. Neither side negotiates TLS here, but only MySQL's could: InlaySQL's wire protocol
does not implement it yet. PostgreSQL has no row here on purpose — InlaySQL has no
PostgreSQL-wire server to put on the other end of one. See bench/README.md.

`retries` counts a write this engine rolled back and retried on its own
first-committer-wins conflict response (MySQL error 1213) rather than one that failed;
disjoint id ranges per connection should keep this at zero, and a nonzero count is
reported rather than folded into the ops/s figure.

  InlaySQL (server, its own MySQL wire — inlaysql serve --mysql): client/server over the compose network, mysql.connector on both sides of this table — the same client library and code path drives MySQL and InlaySQL here, so this is the one OLTP row where every engine pays an identical socket round trip; each connection is a spawned process in this driver, with its own prepared statement and autocommit session, one durable commit per row; concurrency levels are disjoint contiguous id/key ranges per connection, not a shared queue. See bench/README.md's Server-to-server section for the concurrency-model, credential and TLS asymmetries that remain even so, and for why PostgreSQL has no row in this table. Where present, commit_stats is the delta of each engine's own commit/fsync counters bracketing that level's write phase — the commits-per-fsync instrument, SCOREBOARD.md §6: a ratio rising with concurrency says group commit is amortising fsyncs across writers, not just that throughput moved. For MySQL: Handler_commit/Innodb_os_log_fsyncs (Handler_commit, not Com_commit, which never moves under autocommit-implicit writes — see mysql_driver.py). For inlaysql-server (live as of 2026-08-31, closing this section's former instrument gap): commits/fsyncs/commits_per_fsync are Inlaysql_normal_commit_tickets/Inlaysql_normal_commit_flushes (excludes checkpoint-triggered flushes, the like-for-like pair against MySQL's); commits_all/fsyncs_all/commits_per_fsync_all are the checkpoint-inclusive Inlaysql_commit_tickets/Inlaysql_commit_flushes, reported alongside in case the two diverge materially — see global_status's docstring and SCOREBOARD.md.
  MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled): client/server over the compose network, mysql.connector on both sides of this table — the same client library and code path drives MySQL and InlaySQL here, so this is the one OLTP row where every engine pays an identical socket round trip; each connection is a spawned process in this driver, with its own prepared statement and autocommit session, one durable commit per row; concurrency levels are disjoint contiguous id/key ranges per connection, not a shared queue. See bench/README.md's Server-to-server section for the concurrency-model, credential and TLS asymmetries that remain even so, and for why PostgreSQL has no row in this table. Where present, commit_stats is the delta of each engine's own commit/fsync counters bracketing that level's write phase — the commits-per-fsync instrument, SCOREBOARD.md §6: a ratio rising with concurrency says group commit is amortising fsyncs across writers, not just that throughput moved. For MySQL: Handler_commit/Innodb_os_log_fsyncs (Handler_commit, not Com_commit, which never moves under autocommit-implicit writes — see mysql_driver.py). For inlaysql-server (live as of 2026-08-31, closing this section's former instrument gap): commits/fsyncs/commits_per_fsync are Inlaysql_normal_commit_tickets/Inlaysql_normal_commit_flushes (excludes checkpoint-triggered flushes, the like-for-like pair against MySQL's); commits_all/fsyncs_all/commits_per_fsync_all are the checkpoint-inclusive Inlaysql_commit_tickets/Inlaysql_commit_flushes, reported alongside in case the two diverge materially — see global_status's docstring and SCOREBOARD.md.

```

