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

- generated: 2026-09-07T08:46:48Z
- commit: 36cd9e3
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-09-07T08:39:57Z
commit: 36cd9e3
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T083711Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T083902Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T083930Z.txt

metrics: 46; disagreeing by 10% or more across runs: 16

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   99.5%         max        4.20ms        3.58ms        7.76ms  SQLite (WAL, sync=NORMAL)
   95.2%         max       22.57µs       21.01µs       42.49µs  SQLite (WAL, sync=NORMAL)
   37.5%         max       13.75ms       13.65ms       18.81ms  InlaySQL (batched)
   36.3%         p99        1.79ms        1.73ms        2.38ms  SQLite (journal, sync=FULL, fullfsync)
   36.3%         p99        1.79ms        1.73ms        2.38ms  SQLite (journal, sync=FULL, fullfsync)
   30.8%         max       31.10µs       26.83µs       36.42µs  SQLite (journal, sync=FULL, fullfsync)
   28.1%         max       15.97ms       13.34ms       17.82ms  InlaySQL
   28.1%         max       15.97ms       13.34ms       17.82ms  InlaySQL
   25.7%         max       26.40µs       25.40µs       32.19µs  InlaySQL
   18.6%         p99       11.98µs       10.03µs       12.26µs  SQLite (journal, sync=FULL, fullfsync)
   18.0%         p99        4.39µs        4.02µs        4.81µs  InlaySQL (batched)
   14.0%         p95        1.14ms        1.13ms        1.29ms  SQLite (journal, sync=FULL, fullfsync)
   14.0%         p95        1.14ms        1.13ms        1.29ms  SQLite (journal, sync=FULL, fullfsync)
   13.5%         p50      332.38µs      300.38µs      345.21µs  InlaySQL
   13.5%         p50      332.38µs      300.38µs      345.21µs  InlaySQL
   13.3%         p99        3.91µs        3.78µs        4.30µs  SQLite (WAL, sync=NORMAL)

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 5000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         2479   332.38µs   641.88µs     1.22ms    15.97ms
SQLite (journal, sync=FULL, fullfsync)           1105   833.31µs     1.14ms     1.79ms    16.05ms
SQLite (WAL, sync=NORMAL)                       73401     9.90µs    11.78µs    22.56µs     4.20ms
InlaySQL is 2.32x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             174441     3.10µs     3.58µs     4.39µs    13.75ms
InlaySQL                                         2479   332.38µs   641.88µs     1.22ms    15.97ms
SQLite (journal, sync=FULL, fullfsync)           1105   833.31µs     1.14ms     1.79ms    16.05ms
InlaySQL (batched) is 70.75x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1096273   772.00ns     1.37µs     1.93µs    26.40µs
SQLite (journal, sync=FULL, fullfsync)         116447     8.38µs     8.89µs    11.98µs    31.10µs
SQLite (WAL, sync=NORMAL)                      338215     2.82µs     3.29µs     3.91µs    22.57µs
InlaySQL is 9.65x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-09-07T08:40:37Z
commit: 36cd9e3
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T083957Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T084010Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T084023Z.txt

metrics: 42; disagreeing by 10% or more across runs: 10

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   54.3%         max        3.37ms        3.16ms        4.99ms  InlaySQL (no index: full scan)
   47.6%         p99       18.31µs       17.33µs       26.04µs  SQLite (WAL, sync=NORMAL) (index)
   40.8%         max        3.21ms        3.04ms        4.35ms  InlaySQL (no index: full scan)
   36.1%         max       23.98µs       18.21µs       26.87µs  SQLite (WAL, sync=NORMAL) (index)
   29.9%         max       36.35µs       33.26µs       44.13µs  InlaySQL (B-tree index)
   21.6%         max       37.50µs       33.39µs       41.50µs  SQLite (journal, sync=FULL, fullfsync) (index)
   16.7%         max       31.21µs       30.45µs       35.66µs  SQLite (journal, sync=FULL, fullfsync) (index)
   14.3%         max       25.09µs       23.98µs       27.56µs  SQLite (WAL, sync=NORMAL) (index)
   12.9%         p99        2.94ms        2.94ms        3.32ms  InlaySQL (no index: full scan)
   10.5%         max       32.49µs       31.98µs       35.38µs  InlaySQL (B-tree index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 5000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              217366     4.30µs     6.32µs     9.13µs    36.35µs
InlaySQL (no index: full scan)                          404     2.47ms     2.51ms     2.57ms     3.37ms
SQLite (journal, sync=FULL, fullfsync) (index)        101282     9.30µs    11.23µs    19.79µs    37.50µs
SQLite (WAL, sync=NORMAL) (index)                    235327     3.70µs     5.81µs     6.32µs    23.98µs
InlaySQL (B-tree index) is 537.49x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               52389    18.44µs    23.03µs    31.59µs    32.49µs
InlaySQL (no index: full scan)                          350     2.85ms     2.91ms     2.94ms     3.21ms
SQLite (journal, sync=FULL, fullfsync) (index)        52728    18.22µs    21.39µs    29.52µs    31.21µs
SQLite (WAL, sync=NORMAL) (index)                     75618    12.46µs    15.25µs    18.31µs    25.09µs
InlaySQL (B-tree index) is 148.87x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-09-07T08:42:05Z
commit: 36cd9e3
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T084037Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T084106Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T084136Z.txt

metrics: 74; disagreeing by 10% or more across runs: 16

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   37.5%         p99       10.08µs       10.06µs       13.84µs  SQLite (WAL, sync=NORMAL) (index)
   37.5%        cold       10.08µs       10.06µs       13.84µs  SQLite (WAL, sync=NORMAL) (index)
   22.9%         max      122.92µs      113.81µs      141.98µs  InlaySQL
   22.9%        cold      122.92µs      113.81µs      141.97µs  InlaySQL
   16.3%        cold       48.82µs       46.69µs       54.66µs  InlaySQL
   16.3%         max       48.82µs       46.69µs       54.66µs  InlaySQL
   15.5%         p99       29.20µs       26.55µs       31.09µs  InlaySQL
   15.2%         max       31.64ms       29.98ms       34.79ms  SQLite (WAL, sync=NORMAL) (index)
   15.0%         p99       29.85ms       28.99ms       33.46ms  SQLite (WAL, sync=NORMAL) (index)
   14.3%         p95       17.11µs       16.27µs       18.72µs  InlaySQL
   13.9%         p99       17.95µs       16.15µs       18.64µs  SQLite (WAL, sync=NORMAL) (index)
   13.9%        cold       18.00µs       16.15µs       18.64µs  SQLite (WAL, sync=NORMAL) (index)
   13.3%         p99       13.94ms       13.33ms       15.18ms  InlaySQL
   11.7%        cold       32.53µs       31.01µs       34.83µs  SQLite (journal, sync=FULL, fullfsync) (index)
   11.7%         p95       10.16µs        9.99µs       11.18µs  InlaySQL
   11.7%         max       32.53µs       31.01µs       34.82µs  SQLite (journal, sync=FULL, fullfsync) (index)

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 78     49.40ms    12.41ms    13.33ms    13.83ms    49.40ms
SQLite (journal, sync=FULL, fullfsync) (index)           36     27.54ms    27.88ms    28.20ms    28.93ms    29.92ms
SQLite (WAL, sync=NORMAL) (index)                        36     27.49ms    27.77ms    28.31ms    29.85ms    31.64ms
InlaySQL is 2.11x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              97734    48.82µs     9.58µs    10.16µs    22.41µs    48.82µs
SQLite (journal, sync=FULL, fullfsync) (index)        97172    22.15µs    10.01µs    10.10µs    20.80µs    22.15µs
SQLite (WAL, sync=NORMAL) (index)                    214100    10.08µs     4.46µs     4.60µs    10.08µs    14.85µs
InlaySQL is 1.00x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 72     81.93ms    13.23ms    13.76ms    13.94ms    81.93ms
SQLite (journal, sync=FULL, fullfsync) (index)           13     76.81ms    76.51ms    77.58ms    77.92ms    78.54ms
SQLite (WAL, sync=NORMAL) (index)                        13     76.32ms    76.44ms    77.33ms    78.38ms    78.64ms
InlaySQL is 5.51x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              64862   122.92µs    13.83µs    17.11µs    29.20µs   122.92µs
SQLite (journal, sync=FULL, fullfsync) (index)        75916    32.53µs    12.78µs    13.00µs    24.27µs    32.53µs
SQLite (WAL, sync=NORMAL) (index)                    133637    18.00µs     7.20µs     7.29µs    17.95µs    18.07µs
InlaySQL is 1.16x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-09-07T08:42:16Z
commit: 36cd9e3
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T084205Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T084209Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260907T084213Z.txt

metrics: 252; disagreeing by 10% or more across runs: 51

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  195.8%         max        4.71ms        3.86ms       13.08ms  SQLite (journal, sync=FULL, fullfsync)
  173.6%         max        1.97ms        1.87ms        5.29ms  InlaySQL (parallel WAL regions)
  100.0%      lock.)             1             0             1  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   98.3%         max        7.45ms        5.72ms       13.04ms  SQLite (journal, sync=FULL, fullfsync)
   69.1%         max       12.99ms        5.62ms       14.59ms  SQLite (journal, sync=FULL, fullfsync)
   60.0%      lock.)          0.01             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   46.7%         p99        1.32ms        0.76ms        1.38ms  InlaySQL (parallel WAL regions)
   45.7%         p50      305.90µs      265.66µs      405.39µs  InlaySQL (parallel WAL regions)
   44.7%         p99        2.19ms        1.74ms        2.72ms  InlaySQL (parallel WAL regions)
   36.0%         max       11.64ms        9.97ms       14.16ms  InlaySQL (parallel WAL regions)
   34.2%      lock.)           7.6             5           7.6  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   30.8%         p99        2.11ms        1.93ms        2.58ms  SQLite (journal, sync=FULL, fullfsync)
   28.5%         p99        1.86ms        1.42ms        1.95ms  SQLite (journal, sync=FULL, fullfsync)
   27.6%         p99        1.92ms        1.78ms        2.31ms  SQLite (journal, sync=FULL, fullfsync)
   26.3%      lock.)          0.16          0.14          0.19  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   25.0%      lock.)          0.06          0.06          0.07  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   24.7%         p95        1.58ms        1.49ms        1.88ms  InlaySQL (parallel WAL regions)
   22.4%      lock.)          0.09          0.08           0.1  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   21.0%         max        4.76ms        4.43ms        5.43ms  SQLite (journal, sync=FULL, fullfsync)
   20.7%      lock.)          0.03          0.03          0.03  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   20.2%      lock.)          0.23          0.21          0.26  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   18.9%         p99        4.76ms        3.92ms        4.82ms  InlaySQL (parallel WAL regions)
   18.2%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   17.3%      lock.)         5.20%         5.10%         6.00%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   16.9%         p99        1.77ms        1.54ms        1.84ms  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers,   2834 barriers/s — fsync  0.24 ms, interval  0.35 ms, idle  0.11 ms (30.60% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.16 ms/barrier
  buckets: 1 writers, busy 70.4 ms over 200 commits — gate_wait 0.00%, gate_hold 22.00%, follower_wait 0.00%, gather_spin 0.00%, fsync 70.20%, post 0.20%, pre-gate residual 7.70% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.08 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.03 ms (38.30%), residual  0.05 ms (61.70%), 0 commit-point misses
  barriers: 2 writers, 214 normal flushes over 400 commits ( 0.54 syncs/commit, 1.87 commits/sync)
  barrier cycle: 2 writers, 1856.8 barriers/s — fsync  0.31 ms, interval  0.54 ms, idle  0.23 ms (42.40% of the wall clock has no flush in flight); coordinator gather   0.1 post     0 gap  0.16 ms/barrier
  buckets: 2 writers, busy 224.7 ms over 400 commits — gate_wait 5.20%, gate_hold 15.30%, follower_wait 27.90%, gather_spin 9.80%, fsync 29.80%, post 0.40%, pre-gate residual 11.70% (402 gate waits, 202 racing holds)
  gate hold: 2 writers, 402 holds,  0.09 ms mean —              read  0.01 (950 calls), state     0 (1), wal  0.01 (401, 7.6 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.03 ms (32.70%), residual  0.06 ms (67.30%), 1 commit-point misses
  barriers: 4 writers, 212 normal flushes over 800 commits ( 0.27 syncs/commit, 3.79 commits/sync)
  barrier cycle: 4 writers, 993.6 barriers/s — fsync  0.46 ms, interval  1.01 ms, idle  0.54 ms (54.00% of the wall clock has no flush in flight); coordinator gather  0.36 post  0.01 gap  0.17 ms/barrier
  buckets: 4 writers, busy 848.6 ms over 800 commits — gate_wait 16.20%, gate_hold 10.50%, follower_wait 44.80%, gather_spin 9.00%, fsync 11.80%, post 0.30%, pre-gate residual 7.00% (803 gate waits, 601 racing holds)
  gate hold: 4 writers, 803 holds,  0.11 ms mean —              read  0.01 (2848 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.04 ms (31.40%), residual  0.08 ms (68.60%), 3 commit-point misses
  barriers: 8 writers, 212 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.58 commits/sync)
  barrier cycle: 8 writers, 605.6 barriers/s — fsync  0.57 ms, interval  1.65 ms, idle  1.08 ms (65.20% of the wall clock has no flush in flight); coordinator gather  0.86 post  0.02 gap  0.23 ms/barrier
  buckets: 8 writers, busy   2807 ms over 1600 commits — gate_wait 23.80%, gate_hold 6.80%, follower_wait 52.70%, gather_spin 6.40%, fsync 4.50%, post 0.20%, pre-gate residual 5.50% (1603 gate waits, 1396 racing holds)
  gate hold: 8 writers, 1603 holds,  0.12 ms mean —              read  0.01 (6431 calls), state     0 (8), wal  0.01 (1608, 11.4 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.04 ms (29.80%), residual  0.08 ms (70.20%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         2834          200       0.00%   305.90µs   554.00µs     1.32ms     1.97ms
InlaySQL (parallel WAL regions)                 2         3487          400       0.00%   543.55µs   817.86µs     2.19ms     3.26ms
InlaySQL (parallel WAL regions)                 4         3750          800       0.00%   982.66µs     1.58ms     4.76ms     5.51ms
InlaySQL (parallel WAL regions)                 8         4507         1600       0.00%     1.55ms     3.14ms     6.40ms     11.64ms
SQLite (journal, sync=FULL, fullfsync)          1         1020          200       0.00%   872.65µs     1.21ms     2.11ms     4.71ms
SQLite (journal, sync=FULL, fullfsync)          2         1058          400       0.00%   882.08µs     1.17ms     1.86ms     7.45ms
SQLite (journal, sync=FULL, fullfsync)          4         1059          800       0.00%   878.47µs     1.21ms     1.77ms     4.76ms
SQLite (journal, sync=FULL, fullfsync)          8         1073         1600       0.00%   849.13µs     1.20ms     1.92ms    12.99ms

InlaySQL at 8 writers does 1.62x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-09-07T08:46:19Z
commit: 36cd9e3
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  182.00us  229.00us |   0.988  293.00us  360.00us     4.1s
DuckDB (exhaustive + fts BM25)         1.000   17.11ms   22.59ms |   0.966   34.76ms   43.78ms    78.1s
DuckDB (vss HNSW + fts BM25)           0.993   14.96ms   17.56ms |   0.956   33.16ms   41.51ms    78.9s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.997    3.13ms    3.48ms |   0.418   10.27ms   11.66ms     3.9s
pgvector (HNSW + ts_rank)              0.988  455.00us  616.00us |   0.456   39.43ms   60.45ms     1.5s
pgvector (exhaustive + ts_rank)        0.999    1.25ms    2.21ms |   0.465   40.73ms   63.24ms     0.5s

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
InlaySQL                                                              2718.5  285.00us  582.00us    1.10ms |    823530.4    1.00us    2.00us    2.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       2673.9  294.00us  595.00us    1.48ms |    789846.7    1.00us    2.00us    2.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           2374.4  394.00us  540.00us  879.00us |      3968.2  236.00us  300.00us  371.00us
                                                                   commits-per-fsync: 20003/20681 = 0.97
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       3937.3  245.00us  304.00us  423.00us |      7903.5  116.00us  152.00us  175.00us
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


=== server-to-server: 20000 rows, 5000 lookups by primary key, seed 42 — mysql.connector on both sides ===

                                                                                    --- write (durable, one row/commit) ---         |      --- read (point lookup) ---      
engine                                                                         conn write ops/s      p50      p95      p99 retries |  read ops/s      p50      p95      p99
---------------------------------------------------------------------------------------------------------------------------------------------------------------------------
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1577.4  583.00us  798.00us    1.82ms       0 |      2346.5  215.00us  300.00us  349.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      2296.0    1.16ms    2.40ms    5.46ms       0 |      2585.1  277.00us  471.00us  576.00us
                                                                                      commits-per-fsync: 2009/581 = 3.46
                                                                                      commits-per-fsync (checkpoint-inclusive): 2016/587 = 3.43
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1521.2    3.71ms    9.52ms   20.23ms       0 |       734.6  375.00us    3.17ms    5.04ms
                                                                                      commits-per-fsync: 2019/235 = 8.59
                                                                                      commits-per-fsync (checkpoint-inclusive): 2026/242 = 8.37
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2013.1  393.00us  562.00us  925.00us       0 |      2063.2  271.00us  297.00us  315.00us
                                                                                      commits-per-fsync: 2003/2113 = 0.95
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      3942.6  588.00us  994.00us    1.91ms       0 |      2230.3  340.00us  570.00us  710.00us
                                                                                      commits-per-fsync: 2003/1393 = 1.44
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      1876.4  787.00us    2.41ms    4.71ms       0 |       616.1  303.00us    2.11ms    5.52ms
                                                                                      commits-per-fsync: 2003/1575 = 1.27

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

