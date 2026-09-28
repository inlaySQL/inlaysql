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

- generated: 2026-09-28T10:40:25Z
- commit: d61a724
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-09-28T10:11:30Z
commit: d61a724
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T100815Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T101015Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T101053Z.txt

metrics: 46; disagreeing by 10% or more across runs: 13

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  183.9%         max        5.35ms        4.23ms       14.07ms  SQLite (WAL, sync=NORMAL)
   85.9%         max       33.89ms       31.22ms       60.33ms  SQLite (journal, sync=FULL, fullfsync)
   85.9%         max       33.89ms       31.22ms       60.33ms  SQLite (journal, sync=FULL, fullfsync)
   72.8%         max       22.55µs       19.76µs       36.18µs  InlaySQL
   67.7%         p99        4.03µs        3.91µs        6.64µs  InlaySQL (batched)
   41.2%         max       14.34ms       14.11ms       20.02ms  InlaySQL (batched)
   34.1%         max       42.39µs       38.91µs       53.36µs  SQLite (WAL, sync=NORMAL)
   31.7%         max       56.56µs       47.44µs       65.38µs  SQLite (journal, sync=FULL, fullfsync)
   11.1%         p99        1.71ms        1.64ms        1.83ms  InlaySQL
   11.1%         p99        1.71ms        1.64ms        1.83ms  InlaySQL
   10.8%      engine        77.92x        74.49x        82.92x  InlaySQL (batched) is faster than InlaySQL
   10.6%         p99        4.35ms        4.22ms        4.68ms  SQLite (journal, sync=FULL, fullfsync)
   10.6%         p99        4.35ms        4.22ms        4.68ms  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         2288   365.12µs   773.18µs     1.71ms    16.24ms
SQLite (journal, sync=FULL, fullfsync)            791     1.14ms     1.80ms     4.35ms    33.89ms
SQLite (WAL, sync=NORMAL)                       72247     9.68µs    11.56µs    21.91µs     5.35ms
InlaySQL is 2.85x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             178291     3.06µs     3.51µs     4.03µs    14.34ms
InlaySQL                                         2288   365.12µs   773.18µs     1.71ms    16.24ms
SQLite (journal, sync=FULL, fullfsync)            791     1.14ms     1.80ms     4.35ms    33.89ms
InlaySQL (batched) is 77.92x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1322524   701.00ns   792.00ns   852.00ns    22.55µs
SQLite (journal, sync=FULL, fullfsync)         118849     8.26µs     8.43µs    12.01µs    56.56µs
SQLite (WAL, sync=NORMAL)                      348771     2.78µs     2.88µs     3.19µs    42.39µs
InlaySQL is 11.08x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-09-28T10:38:35Z
commit: d61a724
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T101130Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T102038Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T102939Z.txt

metrics: 42; disagreeing by 10% or more across runs: 12

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   50.1%         max       46.89µs       38.79µs       62.30µs  InlaySQL (B-tree index)
   41.7%         p99       16.87µs       16.65µs       23.68µs  SQLite (WAL, sync=NORMAL) (index)
   31.4%         max       45.40µs       37.50µs       51.74µs  SQLite (WAL, sync=NORMAL) (index)
   28.2%         p99       30.28µs       24.68µs       33.22µs  InlaySQL (B-tree index)
   16.0%         max       32.37µs       30.07µs       35.26µs  SQLite (journal, sync=FULL, fullfsync) (index)
   15.9%         max        3.45ms        3.15ms        3.70ms  InlaySQL (no index: full scan)
   15.4%         p95       19.86µs       19.26µs       22.31µs  InlaySQL (B-tree index)
   14.7%         max       56.70µs       52.15µs       60.51µs  SQLite (journal, sync=FULL, fullfsync) (index)
   14.3%         p99        3.29ms        3.08ms        3.55ms  InlaySQL (no index: full scan)
   10.4%         max       32.95µs       31.77µs       35.21µs  InlaySQL (B-tree index)
   10.4%         p50       18.03µs       17.54µs       19.41µs  InlaySQL (B-tree index)
   10.1%       ops/s         53130         49778         55170  InlaySQL (B-tree index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              244390     3.98µs     4.38µs     6.67µs    46.89µs
InlaySQL (no index: full scan)                          373     2.68ms     2.72ms     2.77ms     5.32ms
SQLite (journal, sync=FULL, fullfsync) (index)       103479     9.15µs    10.86µs    14.70µs    56.70µs
SQLite (WAL, sync=NORMAL) (index)                    241607     3.64µs     5.57µs     5.97µs    45.40µs
InlaySQL (B-tree index) is 652.16x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               53130    18.03µs    19.86µs    30.28µs    32.95µs
InlaySQL (no index: full scan)                          329     3.03ms     3.10ms     3.29ms     3.45ms
SQLite (journal, sync=FULL, fullfsync) (index)        54876    17.67µs    20.22µs    30.25µs    32.37µs
SQLite (WAL, sync=NORMAL) (index)                     78185    12.44µs    15.58µs    16.87µs    24.73µs
InlaySQL (B-tree index) is 161.66x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-09-28T10:40:01Z
commit: d61a724
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T103835Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T103904Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T103933Z.txt

metrics: 74; disagreeing by 10% or more across runs: 17

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   70.6%         p95       10.68µs       10.24µs       17.78µs  InlaySQL
   66.8%         p99       27.45µs       27.24µs       45.58µs  InlaySQL
   60.0%         p99       17.64µs        7.61µs       18.19µs  SQLite (WAL, sync=NORMAL) (index)
   32.1%         p95       20.24µs       15.14µs       21.64µs  InlaySQL
   23.4%         p99       17.12µs       15.98µs       19.98µs  SQLite (journal, sync=FULL, fullfsync) (index)
   23.3%        cold       17.12µs       15.98µs       19.98µs  SQLite (journal, sync=FULL, fullfsync) (index)
   22.7%         p99       13.97ms       13.24ms       16.41ms  InlaySQL
   21.7%        cold       10.55µs        8.83µs       11.11µs  SQLite (WAL, sync=NORMAL) (index)
   21.6%         p99       10.55µs        8.83µs       11.11µs  SQLite (WAL, sync=NORMAL) (index)
   20.2%        cold       49.19µs       40.77µs       50.73µs  InlaySQL
   20.2%         max       49.19µs       40.77µs       50.72µs  InlaySQL
   16.7%         p99       16.81ms       14.11ms       16.91ms  InlaySQL
   16.2%        cold      122.79µs      115.67µs      135.61µs  InlaySQL
   16.2%         max      122.79µs      115.67µs      135.61µs  InlaySQL
   16.2%         p99       28.64ms       28.51ms       33.15ms  SQLite (journal, sync=FULL, fullfsync) (index)
   16.1%         max       30.09ms       28.63ms       33.46ms  SQLite (journal, sync=FULL, fullfsync) (index)
   11.9%         p95       13.76ms       12.76ms       14.40ms  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 80     48.40ms    11.95ms    13.76ms    16.81ms    48.40ms
SQLite (journal, sync=FULL, fullfsync) (index)           35     27.95ms    28.19ms    28.42ms    28.64ms    30.09ms
SQLite (WAL, sync=NORMAL) (index)                        35     28.07ms    28.27ms    28.36ms    28.60ms    28.92ms
InlaySQL is 2.25x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              93697    49.19µs    10.01µs    10.68µs    23.53µs    49.19µs
SQLite (journal, sync=FULL, fullfsync) (index)        98337    17.12µs     9.94µs    10.11µs    17.12µs    20.33µs
SQLite (WAL, sync=NORMAL) (index)                    214271    10.55µs     4.48µs     4.55µs     10.55µs    13.94µs
InlaySQL is 1.11x slower than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 76     79.68ms    12.24ms    13.25ms    13.97ms    79.68ms
SQLite (journal, sync=FULL, fullfsync) (index)           14    73.52ms    73.82ms    74.16ms    75.67ms    77.03ms
SQLite (WAL, sync=NORMAL) (index)                        14     74.32ms    73.58ms    74.16ms    75.63ms    76.68ms
InlaySQL is 5.56x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              63976   122.79µs    13.64µs    20.24µs    27.45µs   122.79µs
SQLite (journal, sync=FULL, fullfsync) (index)        76676    31.17µs    12.60µs    13.24µs    23.73µs    31.17µs
SQLite (WAL, sync=NORMAL) (index)                    136353    18.28µs     7.07µs     7.21µs    17.64µs    18.28µs
InlaySQL is 1.22x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-09-28T10:40:16Z
commit: d61a724
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T104002Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T104006Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260928T104011Z.txt

metrics: 252; disagreeing by 10% or more across runs: 64

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  127.0%      lock.)         7.40%        -1.90%         7.50%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
  115.9%         p99        2.51ms        2.35ms        5.26ms  SQLite (journal, sync=FULL, fullfsync)
  109.0%         max        3.99ms        3.75ms        8.10ms  InlaySQL (parallel WAL regions)
  104.9%         max       18.88ms        5.22ms       25.03ms  SQLite (journal, sync=FULL, fullfsync)
  101.0%         max        2.00ms        1.72ms        3.74ms  InlaySQL (parallel WAL regions)
   99.7%         max        6.05ms        5.72ms       11.75ms  SQLite (journal, sync=FULL, fullfsync)
   80.8%         max       13.49ms        6.58ms       17.48ms  SQLite (journal, sync=FULL, fullfsync)
   76.3%         p99        1.94ms        1.87ms        3.35ms  InlaySQL (parallel WAL regions)
   75.0%      lock.)             0             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   69.0%         p99        7.36ms        5.16ms       10.24ms  InlaySQL (parallel WAL regions)
   62.0%         max        7.02ms        5.54ms        9.89ms  InlaySQL (parallel WAL regions)
   55.4%         max       14.47ms        7.28ms       15.29ms  InlaySQL (parallel WAL regions)
   52.0%      lock.)             5             5           7.6  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   48.9%      lock.)          0.19          0.12          0.21  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   48.6%         p99        0.91ms        0.77ms        1.21ms  InlaySQL (parallel WAL regions)
   44.8%         p99        4.84ms        3.62ms        5.79ms  InlaySQL (parallel WAL regions)
   42.4%      lock.)          0.09          0.06           0.1  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   41.2%      lock.)        1885.7        1560.5          2337  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   40.2%      lock.)          0.53          0.43          0.64  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   37.5%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   35.8%      lock.)          0.11          0.07          0.11  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   35.5%      lock.)          0.34          0.31          0.43  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   34.7%      lock.)        28.50%        19.70%        29.60%  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   33.3%      lock.)         0.30%         0.30%         0.40%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   33.1%      lock.)          0.13          0.12          0.17  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 2729.3 barriers/s — fsync  0.28 ms, interval  0.37 ms, idle  0.11 ms (28.50% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.15 ms/barrier
  buckets: 1 writers, busy 73.1 ms over 200 commits — gate_wait 0.00%, gate_hold 20.50%, follower_wait 0.00%, gather_spin 0.00%, fsync 72.30%, post 0.20%, pre-gate residual 7.40% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.08 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.03 ms (37.90%), residual  0.05 ms (62.10%), 0 commit-point misses
  barriers: 2 writers, 226 normal flushes over 400 commits ( 0.56 syncs/commit, 1.77 commits/sync)
  barrier cycle: 2 writers, 1885.7 barriers/s — fsync  0.34 ms, interval  0.53 ms, idle  0.19 ms (33.20% of the wall clock has no flush in flight); coordinator gather  0.09 post     0 gap  0.13 ms/barrier
  buckets: 2 writers, busy 238.1 ms over 400 commits — gate_wait 3.70%, gate_hold 13.50%, follower_wait 30.30%, gather_spin 7.60%, fsync 34.60%, post 0.30%, pre-gate residual 9.80% (402 gate waits, 215 racing holds)
  gate hold: 2 writers, 402 holds,  0.08 ms mean —              read  0.01 (868 calls), state     0 (0), wal     0 (400,   5 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.03 ms (30.80%), residual  0.06 ms (69.20%), 1 commit-point misses
  barriers: 4 writers, 222 normal flushes over 800 commits ( 0.28 syncs/commit, 3.62 commits/sync)
  barrier cycle: 4 writers,  990.3 barriers/s — fsync  0.51 ms, interval  1.01 ms, idle  0.49 ms (48.10% of the wall clock has no flush in flight); coordinator gather  0.32 post  0.01 gap  0.16 ms/barrier
  buckets: 4 writers, busy 873.6 ms over 800 commits — gate_wait 15.10%, gate_hold 9.80%, follower_wait 46.90%, gather_spin 8.00%, fsync 13.20%, post 0.20%, pre-gate residual 6.90% (802 gate waits, 611 racing holds)
  gate hold: 4 writers, 802 holds,  0.11 ms mean —              read  0.01 (2827 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.03 ms (31.20%), residual  0.07 ms (68.80%), 3 commit-point misses
  barriers: 8 writers, 213 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.54 commits/sync)
  barrier cycle: 8 writers,   572 barriers/s — fsync  0.66 ms, interval  1.75 ms, idle  1.04 ms (62.00% of the wall clock has no flush in flight); coordinator gather  0.82 post  0.02 gap  0.23 ms/barrier
  buckets: 8 writers, busy 2929.2 ms over 1600 commits — gate_wait 22.90%, gate_hold 6.50%, follower_wait 54.20%, gather_spin 6.10%, fsync 4.90%, post 0.20%, pre-gate residual 5.20% (1603 gate waits, 1398 racing holds)
  gate hold: 8 writers, 1603 holds,  0.12 ms mean —              read  0.01 (6385 calls), state     0 (8), wal  0.01 (1608, 11.3 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.03 ms (29.00%), residual  0.08 ms (71.00%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         2729          200       0.00%   356.31µs   605.95µs     0.91ms     2.00ms
InlaySQL (parallel WAL regions)                 2         3337          400       0.00%   562.84µs   929.38µs     1.94ms     3.99ms
InlaySQL (parallel WAL regions)                 4         3643          800       0.00%   969.77µs     1.66ms     4.84ms     7.02ms
InlaySQL (parallel WAL regions)                 8         4337         1600       0.00%     1.61ms     3.16ms     7.36ms    14.47ms
SQLite (journal, sync=FULL, fullfsync)          1          768          200       0.00%     1.19ms     1.86ms     2.51ms     6.05ms
SQLite (journal, sync=FULL, fullfsync)          2          767          400       0.00%     1.19ms     1.74ms     3.92ms     13.49ms
SQLite (journal, sync=FULL, fullfsync)          4          794          800       0.00%     1.14ms     1.75ms     4.58ms    18.88ms
SQLite (journal, sync=FULL, fullfsync)          8          805         1600       0.00%     1.12ms     1.74ms     4.27ms    15.93ms

InlaySQL at 8 writers does 1.64x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-09-28T10:14:28Z
commit: d61a724
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000   96.00us  173.00us |   0.988  176.00us  224.00us     3.7s
DuckDB (exhaustive + fts BM25)         1.000    8.58ms    9.35ms |   0.966   17.78ms   21.44ms    39.2s
DuckDB (vss HNSW + fts BM25)           0.990    7.81ms    8.20ms |   0.959   16.88ms   20.31ms    39.6s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.998    1.85ms    2.12ms |   0.418    6.09ms    7.95ms     2.4s
pgvector (HNSW + ts_rank)              0.987  194.00us  277.00us |   0.456   18.87ms   28.64ms     0.8s
pgvector (exhaustive + ts_rank)        0.999  650.00us    1.00ms |   0.465   19.34ms   29.51ms     0.3s

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


=== OLTP: 20000 rows, 200000 lookups by primary key, seed 42 ===

                                                                 --- write (durable, one row/commit) --- |      --- read (point lookup) ---      
engine                                                           write ops/s      p50      p95      p99 |  read ops/s      p50      p95      p99
------------------------------------------------------------------------------------------------------------------------------------------------
InlaySQL                                                              2381.3  155.00us  340.00us    3.11ms |   1535996.2    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       2592.5  154.00us  314.00us    3.00ms |   1563977.9    1.00us    1.00us    1.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           4755.5  196.00us  251.00us  472.00us |     12206.0   77.00us   98.00us  118.00us
                                                                   commits-per-fsync: 20003/22042 = 0.91
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       6714.4  130.00us  156.00us  194.00us |     26666.7   35.00us   43.00us   54.00us
                                                                   commits-per-fsync: 20010/20001 = 1.00

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


=== server-to-server: 20000 rows, 200000 lookups by primary key, seed 42 — mysql.connector on both sides ===

                                                                                    --- write (durable, one row/commit) ---         |      --- read (point lookup) ---      
engine                                                                         conn write ops/s      p50      p95      p99 retries |  read ops/s      p50      p95      p99
---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      2668.8  218.00us  358.00us  862.00us       0 |      7254.4   79.00us   89.00us   97.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      3024.8  620.00us    1.35ms   16.60ms       0 |      7955.0  124.00us  218.00us  289.00us
                                                                                      commits-per-fsync: 2005/587 = 3.42
                                                                                      commits-per-fsync (checkpoint-inclusive): 2015/597 = 3.38
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      2046.9    1.48ms    8.99ms   38.97ms       0 |      2385.2  189.00us    1.15ms    3.21ms
                                                                                      commits-per-fsync: 2011/513 = 3.92
                                                                                      commits-per-fsync (checkpoint-inclusive): 2021/522 = 3.87
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      3977.6  192.00us  290.00us  476.00us       0 |      6596.1   88.00us   99.00us  105.00us
                                                                                      commits-per-fsync: 2003/2211 = 0.91
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      8110.2  278.00us  469.00us  615.00us       0 |      7591.6  101.00us  170.00us  209.00us
                                                                                      commits-per-fsync: 2003/1281 = 1.56
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      3345.9  407.00us    1.19ms    2.92ms       0 |      2009.7  112.00us  935.00us    4.05ms
                                                                                      commits-per-fsync: 2003/1333 = 1.50

This is the row bench/README.md calls the missing apples-to-apples number: InlaySQL
here is never a library call, it is `inlaysql serve --mysql`, reached over the compose
network by the same mysql.connector client code path that reaches MySQL above — every
number in this table, on every row, pays an identical socket round trip.

What still is not comparable, even here. inlaysql-server is thread-per-connection, one
OS thread and one Database handle per connection with no thread pool; MySQL schedules
connections onto a bounded worker pool — a structural difference in what adding a
connection costs each engine, not a tuning gap, so read a widening gap at the higher
concurrency level that way rather than as a regression. Both sides share one user and
one password as configured here, and on both sides that is this benchmark's own setup:
InlaySQL has accounts, GRANT/REVOKE and per-table privileges in the database file, and
neither engine's grant system is exercised here. Neither side negotiates TLS either,
though both could — InlaySQL's server runs with --plaintext-network on the compose
bridge, because a TLS handshake measured against MySQL's plaintext socket would be
measuring the wrong thing. PostgreSQL has no row here on purpose — InlaySQL has no
PostgreSQL-wire server to put on the other end of one. See bench/README.md.

`retries` counts a write this engine rolled back and retried on its own
first-committer-wins conflict response (MySQL error 1213) rather than one that failed;
disjoint id ranges per connection should keep this at zero, and a nonzero count is
reported rather than folded into the ops/s figure.

  InlaySQL (server, its own MySQL wire — inlaysql serve --mysql): client/server over the compose network, mysql.connector on both sides of this table — the same client library and code path drives MySQL and InlaySQL here, so this is the one OLTP row where every engine pays an identical socket round trip; each connection is a spawned process in this driver, with its own prepared statement and autocommit session, one durable commit per row; concurrency levels are disjoint contiguous id/key ranges per connection, not a shared queue. See bench/README.md's Server-to-server section for the concurrency-model difference that remains even so, for the credential and TLS choices this harness makes on both sides, and for why PostgreSQL has no row in this table. Where present, commit_stats is the delta of each engine's own commit/fsync counters bracketing that level's write phase — the commits-per-fsync instrument, SCOREBOARD.md §6: a ratio rising with concurrency says group commit is amortising fsyncs across writers, not just that throughput moved. For MySQL: Handler_commit/Innodb_os_log_fsyncs (Handler_commit, not Com_commit, which never moves under autocommit-implicit writes — see mysql_driver.py). For inlaysql-server (live as of 2026-08-31, closing this section's former instrument gap): commits/fsyncs/commits_per_fsync are Inlaysql_normal_commit_tickets/Inlaysql_normal_commit_flushes (excludes checkpoint-triggered flushes, the like-for-like pair against MySQL's); commits_all/fsyncs_all/commits_per_fsync_all are the checkpoint-inclusive Inlaysql_commit_tickets/Inlaysql_commit_flushes, reported alongside in case the two diverge materially — see global_status's docstring and SCOREBOARD.md.
  MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled): client/server over the compose network, mysql.connector on both sides of this table — the same client library and code path drives MySQL and InlaySQL here, so this is the one OLTP row where every engine pays an identical socket round trip; each connection is a spawned process in this driver, with its own prepared statement and autocommit session, one durable commit per row; concurrency levels are disjoint contiguous id/key ranges per connection, not a shared queue. See bench/README.md's Server-to-server section for the concurrency-model difference that remains even so, for the credential and TLS choices this harness makes on both sides, and for why PostgreSQL has no row in this table. Where present, commit_stats is the delta of each engine's own commit/fsync counters bracketing that level's write phase — the commits-per-fsync instrument, SCOREBOARD.md §6: a ratio rising with concurrency says group commit is amortising fsyncs across writers, not just that throughput moved. For MySQL: Handler_commit/Innodb_os_log_fsyncs (Handler_commit, not Com_commit, which never moves under autocommit-implicit writes — see mysql_driver.py). For inlaysql-server (live as of 2026-08-31, closing this section's former instrument gap): commits/fsyncs/commits_per_fsync are Inlaysql_normal_commit_tickets/Inlaysql_normal_commit_flushes (excludes checkpoint-triggered flushes, the like-for-like pair against MySQL's); commits_all/fsyncs_all/commits_per_fsync_all are the checkpoint-inclusive Inlaysql_commit_tickets/Inlaysql_commit_flushes, reported alongside in case the two diverge materially — see global_status's docstring and SCOREBOARD.md.

```

