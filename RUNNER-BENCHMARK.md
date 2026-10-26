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

- generated: 2026-10-26T09:48:51Z
- commit: f6e3e78
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-10-26T09:22:26Z
commit: f6e3e78
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T091950Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T092136Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T092201Z.txt

metrics: 46; disagreeing by 10% or more across runs: 15

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  189.9%         max        3.76ms        3.23ms       10.37ms  SQLite (WAL, sync=NORMAL)
  126.3%         max      119.35µs       44.52µs      195.26µs  SQLite (WAL, sync=NORMAL)
   70.6%         max       28.24µs       26.66µs       46.61µs  InlaySQL
   60.2%         max       19.55ms       17.41ms       29.18ms  SQLite (journal, sync=FULL, fullfsync)
   60.2%         max       19.55ms       17.41ms       29.18ms  SQLite (journal, sync=FULL, fullfsync)
   42.1%         max       17.32ms       15.20ms       22.50ms  InlaySQL
   42.1%         max       17.32ms       15.20ms       22.50ms  InlaySQL
   40.7%         max       66.64µs       46.73µs       73.82µs  SQLite (journal, sync=FULL, fullfsync)
   28.2%         p99     1300.00µs      982.90µs     1350.00µs  InlaySQL
   28.2%         p99     1300.00µs      982.90µs     1350.00µs  InlaySQL
   18.7%         p99        3.90µs        3.87µs        4.60µs  InlaySQL (batched)
   12.1%         p99        3.55µs        3.30µs        3.73µs  SQLite (WAL, sync=NORMAL)
   11.4%         max       12.07ms       11.74ms       13.12ms  InlaySQL (batched)
   10.1%         p99        1.39ms        1.39ms        1.53ms  SQLite (journal, sync=FULL, fullfsync)
   10.1%         p99        1.39ms        1.39ms        1.53ms  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         2984   269.39µs   486.92µs   1300.00µs    17.32ms
SQLite (journal, sync=FULL, fullfsync)           1295   713.23µs   958.87µs     1.39ms    19.55ms
SQLite (WAL, sync=NORMAL)                       75866     9.75µs    11.61µs    22.25µs     3.76ms
InlaySQL is 2.27x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             185718     3.01µs     3.42µs     3.90µs    12.07ms
InlaySQL                                         2984   269.39µs   486.92µs   1300.00µs    17.32ms
SQLite (journal, sync=FULL, fullfsync)           1295   713.23µs   958.87µs     1.39ms    19.55ms
InlaySQL (batched) is 62.25x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1311756   702.00ns   811.00ns     1.00µs    28.24µs
SQLite (journal, sync=FULL, fullfsync)         119115     8.22µs     8.51µs    11.79µs    66.64µs
SQLite (WAL, sync=NORMAL)                      354271     2.72µs     2.86µs     3.55µs   119.35µs
InlaySQL is 11.01x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-10-26T09:47:05Z
commit: f6e3e78
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T092226Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T093039Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T093854Z.txt

metrics: 42; disagreeing by 10% or more across runs: 14

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  174.8%         max       59.76µs       52.05µs      156.49µs  SQLite (journal, sync=FULL, fullfsync) (index)
   60.6%         max       62.67µs       51.13µs       89.11µs  InlaySQL (B-tree index)
   52.0%         p99       17.49µs       16.61µs       25.70µs  SQLite (WAL, sync=NORMAL) (index)
   31.7%         max        7.56ms        5.67ms        8.07ms  InlaySQL (no index: full scan)
   28.6%         p99       28.21µs       23.81µs       31.88µs  SQLite (journal, sync=FULL, fullfsync) (index)
   20.6%         p95       17.49µs       17.30µs       20.91µs  InlaySQL (B-tree index)
   19.8%         p99       16.01µs       15.79µs       18.96µs  SQLite (journal, sync=FULL, fullfsync) (index)
   18.4%         max       30.48µs       30.40µs       36.00µs  InlaySQL (B-tree index)
   17.2%         max       33.73µs       30.20µs       36.00µs  SQLite (journal, sync=FULL, fullfsync) (index)
   16.7%         max       43.42µs       41.42µs       48.68µs  SQLite (WAL, sync=NORMAL) (index)
   16.5%         p99       27.32µs       26.61µs       31.13µs  InlaySQL (B-tree index)
   11.6%       ops/s       171.21x       154.27x       174.08x  InlaySQL (B-tree index) is faster than InlaySQL (no index: full scan)
   11.0%         p50       15.69µs       15.65µs       17.38µs  InlaySQL (B-tree index)
   10.3%       ops/s         61397         55464         61811  InlaySQL (B-tree index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              234992     4.13µs     4.54µs     7.02µs    62.67µs
InlaySQL (no index: full scan)                          409     2.43ms     2.53ms     2.62ms     7.56ms
SQLite (journal, sync=FULL, fullfsync) (index)       103962     9.09µs    10.82µs    16.01µs    59.76µs
SQLite (WAL, sync=NORMAL) (index)                    244082     3.59µs     5.55µs     5.96µs    43.42µs
InlaySQL (B-tree index) is 574.40x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               61397    15.69µs    17.49µs    27.32µs    30.48µs
InlaySQL (no index: full scan)                          360     2.77ms     2.84ms     2.91ms     2.96ms
SQLite (journal, sync=FULL, fullfsync) (index)        52829    18.32µs    20.87µs    28.21µs    33.73µs
SQLite (WAL, sync=NORMAL) (index)                     76270    12.58µs    15.05µs    17.49µs    26.97µs
InlaySQL (B-tree index) is 171.21x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-10-26T09:48:31Z
commit: f6e3e78
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T094705Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T094734Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T094802Z.txt

metrics: 74; disagreeing by 10% or more across runs: 10

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   55.0%         p99        8.42µs        4.90µs        9.53µs  SQLite (WAL, sync=NORMAL) (index)
   44.2%         max       14.08µs        9.17µs       15.39µs  SQLite (WAL, sync=NORMAL) (index)
   21.4%         p99       28.48µs       26.68µs       32.77µs  InlaySQL
   19.2%         max      115.90µs      112.77µs      135.03µs  InlaySQL
   19.2%        cold      115.90µs      112.77µs      135.03µs  InlaySQL
   16.1%         p99       23.23µs       21.31µs       25.05µs  InlaySQL
   12.5%        cold       23.57µs       20.90µs       23.86µs  SQLite (journal, sync=FULL, fullfsync) (index)
   12.1%        cold        9.17µs        8.41µs        9.53µs  SQLite (WAL, sync=NORMAL) (index)
   11.9%        cold       15.43µs       14.35µs       16.19µs  SQLite (journal, sync=FULL, fullfsync) (index)
   11.9%         p99       15.43µs       14.35µs       16.19µs  SQLite (journal, sync=FULL, fullfsync) (index)

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 87     47.34ms    11.10ms    11.78ms    12.34ms    47.34ms
SQLite (journal, sync=FULL, fullfsync) (index)           37     27.26ms    27.31ms    27.56ms    27.69ms    27.90ms
SQLite (WAL, sync=NORMAL) (index)                        36    27.31ms    27.43ms    27.59ms    27.76ms    27.82ms
InlaySQL is 2.36x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              100447    42.29µs     9.40µs     9.87µs    23.23µs    42.29µs
SQLite (journal, sync=FULL, fullfsync) (index)        99894    15.43µs     9.79µs     9.90µs    15.43µs    21.17µs
SQLite (WAL, sync=NORMAL) (index)                    220175     9.17µs     4.36µs     4.50µs     8.42µs     14.08µs
InlaySQL is 1.00x slower than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 77     79.41ms    12.33ms    12.86ms    13.36ms    79.41ms
SQLite (journal, sync=FULL, fullfsync) (index)           14     73.96ms    73.51ms    73.80ms    74.27ms    77.23ms
SQLite (WAL, sync=NORMAL) (index)                        14     73.96ms    73.54ms    73.85ms    74.02ms    77.69ms
InlaySQL is 5.74x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              68049   115.90µs    13.20µs    15.95µs    28.48µs   115.90µs
SQLite (journal, sync=FULL, fullfsync) (index)        78204    23.57µs    12.45µs    12.79µs    23.57µs    24.30µs
SQLite (WAL, sync=NORMAL) (index)                    139217    15.29µs     6.91µs     7.14µs    15.29µs    18.31µs
InlaySQL is 1.19x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-10-26T09:48:40Z
commit: f6e3e78
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T094831Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T094834Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261026T094837Z.txt

metrics: 252; disagreeing by 10% or more across runs: 54

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  178.9%         max        5.59ms        3.15ms       13.15ms  SQLite (journal, sync=FULL, fullfsync)
  124.5%         max        9.29ms        8.86ms       20.43ms  InlaySQL (parallel WAL regions)
  108.7%         max       12.56ms        4.18ms       17.83ms  SQLite (journal, sync=FULL, fullfsync)
  100.0%      lock.)             1             0             1  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   71.8%         max        6.28ms        5.65ms       10.16ms  InlaySQL (parallel WAL regions)
   47.4%         p99        7.18ms        4.90ms        8.30ms  InlaySQL (parallel WAL regions)
   46.2%         p99        1.30ms        1.09ms        1.69ms  SQLite (journal, sync=FULL, fullfsync)
   40.4%         max        3.69ms        3.32ms        4.81ms  InlaySQL (parallel WAL regions)
   40.0%      lock.)          0.01             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   35.3%         p95        1.16ms        1.05ms        1.46ms  InlaySQL (parallel WAL regions)
   33.6%         max        5.21ms        3.78ms        5.53ms  SQLite (journal, sync=FULL, fullfsync)
   32.9%      lock.)           7.6           5.1           7.6  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   30.0%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   25.2%         p99        4.08ms        3.74ms        4.77ms  InlaySQL (parallel WAL regions)
   25.0%      lock.)         0.40%         0.40%         0.50%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   24.6%      lock.)          0.07          0.05          0.07  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   23.0%      lock.)           0.1          0.08           0.1  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   21.4%      lock.)         5.60%         4.80%         6.00%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   20.5%         p99        1.66ms        1.59ms        1.93ms  InlaySQL (parallel WAL regions)
   19.8%      lock.)          0.16          0.16          0.19  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   19.4%      lock.)          0.24          0.19          0.24  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   19.4%      lock.)          0.03          0.03          0.03  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   18.3%         p95      908.10µs      894.21µs     1060.00µs  SQLite (journal, sync=FULL, fullfsync)
   17.7%         max        3.27ms        3.16ms        3.74ms  SQLite (journal, sync=FULL, fullfsync)
   16.7%         p50      507.56µs      438.14µs      523.09µs  InlaySQL (parallel WAL regions)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 3721.3 barriers/s — fsync  0.16 ms, interval  0.27 ms, idle  0.11 ms (39.60% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.15 ms/barrier
  buckets: 1 writers, busy 53.6 ms over 200 commits — gate_wait 0.00%, gate_hold 27.80%, follower_wait 0.00%, gather_spin 0.00%, fsync 61.20%, post 0.30%, pre-gate residual 10.60% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.07 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.03 ms (38.20%), residual  0.05 ms (61.80%), 0 commit-point misses
  barriers: 2 writers, 227 normal flushes over 400 commits ( 0.57 syncs/commit, 1.76 commits/sync)
  barrier cycle: 2 writers,   2156 barriers/s — fsync  0.23 ms, interval  0.46 ms, idle  0.24 ms (50.50% of the wall clock has no flush in flight); coordinator gather  0.11 post     0 gap  0.14 ms/barrier
  buckets: 2 writers, busy 206.9 ms over 400 commits — gate_wait 5.60%, gate_hold 19.00%, follower_wait 23.40%, gather_spin 11.80%, fsync 25.40%, post 0.40%, pre-gate residual 14.10% (402 gate waits, 198 racing holds)
  gate hold: 2 writers, 402 holds,   0.1 ms mean —              read  0.01 (967 calls), state     0 (1), wal  0.01 (401, 7.6 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.03 ms (31.50%), residual  0.07 ms (68.50%), 1 commit-point misses
  barriers: 4 writers, 212 normal flushes over 800 commits ( 0.27 syncs/commit, 3.79 commits/sync)
  barrier cycle: 4 writers, 1119.8 barriers/s — fsync  0.36 ms, interval  0.89 ms, idle  0.53 ms (59.80% of the wall clock has no flush in flight); coordinator gather  0.35 post  0.01 gap  0.17 ms/barrier
  buckets: 4 writers, busy 752.5 ms over 800 commits — gate_wait 18.30%, gate_hold 11.80%, follower_wait 41.50%, gather_spin 10.00%, fsync 10.20%, post 0.30%, pre-gate residual 8.00% (802 gate waits, 603 racing holds)
  gate hold: 4 writers, 802 holds,  0.11 ms mean —              read  0.01 (2858 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.04 ms (31.90%), residual  0.08 ms (68.10%), 3 commit-point misses
  barriers: 8 writers, 212 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.58 commits/sync)
  barrier cycle: 8 writers, 639.3 barriers/s — fsync  0.47 ms, interval  1.56 ms, idle  1.09 ms (70.00% of the wall clock has no flush in flight); coordinator gather  0.86 post  0.02 gap  0.25 ms/barrier
  buckets: 8 writers, busy 2636.4 ms over 1600 commits — gate_wait 26.00%, gate_hold 7.30%, follower_wait 50.10%, gather_spin 6.90%, fsync 3.80%, post 0.20%, pre-gate residual 5.70% (1603 gate waits, 1396 racing holds)
  gate hold: 8 writers, 1603 holds,  0.12 ms mean —              read  0.01 (6424 calls), state     0 (8), wal  0.01 (1608, 11.4 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.04 ms (29.60%), residual  0.09 ms (70.40%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         3721          200       0.00%   231.81µs   412.06µs   598.04µs     1.88ms
InlaySQL (parallel WAL regions)                 2         3799          400       0.00%   507.56µs   686.54µs     1.66ms     3.69ms
InlaySQL (parallel WAL regions)                 4         4226          800       0.00%   859.91µs     1.16ms     4.08ms     6.28ms
InlaySQL (parallel WAL regions)                 8         4825         1600       0.00%     1.43ms     3.19ms     7.18ms     9.29ms
SQLite (journal, sync=FULL, fullfsync)          1         1299          200       0.00%   718.93µs     0.95ms     1.14ms     3.27ms
SQLite (journal, sync=FULL, fullfsync)          2         1277          400       0.00%   731.06µs   908.10µs     1.30ms     5.59ms
SQLite (journal, sync=FULL, fullfsync)          4         1260          800       0.00%   739.68µs   961.14µs     1.28ms    12.56ms
SQLite (journal, sync=FULL, fullfsync)          8         1286         1600       0.00%   729.20µs   947.50µs     1.25ms     5.21ms

InlaySQL at 8 writers does 1.33x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-10-26T09:27:59Z
commit: f6e3e78
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  163.00us  258.00us |   0.988  267.00us  330.00us     4.2s
DuckDB (exhaustive + fts BM25)         1.000   10.96ms   14.29ms |   0.966   26.94ms   30.42ms    47.5s
DuckDB (vss HNSW + fts BM25)           0.990    9.50ms   10.82ms |   0.959   25.42ms   28.42ms    48.9s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.995    3.18ms    3.70ms |   0.418    9.83ms   11.38ms     3.7s
pgvector (HNSW + ts_rank)              0.988  358.00us  513.00us |   0.456   32.65ms   49.80ms     1.3s
pgvector (exhaustive + ts_rank)        0.999    1.09ms    1.20ms |   0.465   33.13ms   49.00ms     0.5s

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
InlaySQL                                                              2003.1  217.00us  410.00us    3.50ms |   1183016.2    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       2057.2  231.00us  451.00us    3.40ms |   1112339.5    1.00us    1.00us    1.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           3294.8  278.00us  389.00us  681.00us |      8078.1  115.00us  160.00us  211.00us
                                                                   commits-per-fsync: 20003/21735 = 0.92
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       5117.5  188.00us  244.00us  334.00us |     17329.8   53.00us   76.00us   95.00us
                                                                   commits-per-fsync: 20005/20001 = 1.00

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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1713.6  415.00us  634.00us    1.43ms       0 |      5043.3  109.00us  138.00us  158.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      2133.6  979.00us    2.13ms   13.92ms       0 |      5640.1  128.00us  275.00us  343.00us
                                                                                      commits-per-fsync: 2009/658 = 3.05
                                                                                      commits-per-fsync (checkpoint-inclusive): 2014/663 = 3.04
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1455.8    3.12ms   17.01ms   45.76ms       0 |      1648.6  190.00us    2.73ms    5.13ms
                                                                                      commits-per-fsync: 2013/253 = 7.96
                                                                                      commits-per-fsync (checkpoint-inclusive): 2021/260 = 7.77
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2659.7  278.00us  371.00us  694.00us       0 |      4783.5  113.00us  131.00us  146.00us
                                                                                      commits-per-fsync: 2003/2197 = 0.91
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      5476.6  385.00us  718.00us    1.14ms       0 |      5810.8  111.00us  140.00us  277.00us
                                                                                      commits-per-fsync: 2003/1270 = 1.58
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      2628.0  540.00us    1.62ms    3.33ms       0 |      1442.7  125.00us  765.00us    2.66ms
                                                                                      commits-per-fsync: 2003/1142 = 1.75

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

