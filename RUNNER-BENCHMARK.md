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

- generated: 2026-10-12T11:46:56Z
- commit: 60a0bda
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-10-12T11:20:23Z
commit: 60a0bda
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T111747Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T111933Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T111958Z.txt

metrics: 46; disagreeing by 10% or more across runs: 18

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   60.5%         p99        4.10µs        3.96µs        6.44µs  InlaySQL (batched)
   49.0%         max       29.71µs       24.95µs       39.50µs  InlaySQL
   44.3%         max       31.58µs       31.28µs       45.28µs  SQLite (WAL, sync=NORMAL)
   38.0%         p99     1280.00µs      963.59µs     1450.00µs  InlaySQL
   38.0%         p99     1280.00µs      963.59µs     1450.00µs  InlaySQL
   37.4%         max       59.84µs       51.53µs       73.91µs  SQLite (journal, sync=FULL, fullfsync)
   31.6%         max        5.07ms        4.09ms        5.69ms  SQLite (WAL, sync=NORMAL)
   20.7%         max       11.07ms        8.96ms       11.25ms  InlaySQL
   20.7%         max       11.07ms        8.96ms       11.25ms  InlaySQL
   20.3%         max       11.89ms       11.60ms       14.01ms  InlaySQL (batched)
   19.3%         p99        1.61ms        1.59ms        1.90ms  SQLite (journal, sync=FULL, fullfsync)
   19.3%         p99        1.61ms        1.59ms        1.90ms  SQLite (journal, sync=FULL, fullfsync)
   11.9%         p50      253.58µs      238.76µs      269.00µs  InlaySQL
   11.9%         p50      253.58µs      238.76µs      269.00µs  InlaySQL
   11.4%         p95      498.61µs      468.27µs      525.30µs  InlaySQL
   11.4%         p95      498.61µs      468.27µs      525.30µs  InlaySQL
   10.5%         max        9.77ms        9.34ms       10.37ms  SQLite (journal, sync=FULL, fullfsync)
   10.5%         max        9.77ms        9.34ms       10.37ms  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         3076   253.58µs   498.61µs   1280.00µs     11.07ms
SQLite (journal, sync=FULL, fullfsync)           1298   707.31µs     0.97ms     1.61ms     9.77ms
SQLite (WAL, sync=NORMAL)                       75990     9.82µs    11.76µs    21.99µs     5.07ms
InlaySQL is 2.48x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             182686     3.10µs     3.57µs     4.10µs    11.89ms
InlaySQL                                         3076   253.58µs   498.61µs   1280.00µs     11.07ms
SQLite (journal, sync=FULL, fullfsync)           1298   707.31µs     0.97ms     1.61ms     9.77ms
InlaySQL (batched) is 59.15x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1319536   701.00ns   801.00ns   872.00ns    29.71µs
SQLite (journal, sync=FULL, fullfsync)         117049     8.40µs     8.62µs    11.38µs    59.84µs
SQLite (WAL, sync=NORMAL)                      342694     2.84µs     2.92µs     3.30µs    31.58µs
InlaySQL is 11.28x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-10-12T11:45:08Z
commit: 60a0bda
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T112023Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T112841Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T113657Z.txt

metrics: 42; disagreeing by 10% or more across runs: 10

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  427.8%         max      101.91µs       66.37µs      502.39µs  InlaySQL (B-tree index)
  278.6%         max       59.35µs       48.00µs      213.32µs  SQLite (journal, sync=FULL, fullfsync) (index)
  247.1%         max       42.26µs       33.10µs      137.52µs  SQLite (WAL, sync=NORMAL) (index)
   46.3%         p95       20.90µs       17.46µs       27.13µs  InlaySQL (B-tree index)
   27.0%         max       31.07µs       29.85µs       38.25µs  SQLite (journal, sync=FULL, fullfsync) (index)
   23.9%         p99       29.66µs       23.42µs       30.51µs  InlaySQL (B-tree index)
   22.0%         max       29.94µs       29.54µs       36.12µs  InlaySQL (B-tree index)
   19.5%         p99       29.22µs       24.18µs       29.87µs  SQLite (journal, sync=FULL, fullfsync) (index)
   18.8%         p99       17.24µs       16.67µs       19.91µs  SQLite (journal, sync=FULL, fullfsync) (index)
   11.9%         max       24.12µs       23.51µs       26.39µs  SQLite (WAL, sync=NORMAL) (index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              235057     4.13µs     4.51µs     7.29µs   101.91µs
InlaySQL (no index: full scan)                          407     2.45ms     2.50ms     2.57ms     6.63ms
SQLite (journal, sync=FULL, fullfsync) (index)       101487     9.29µs    11.10µs    17.24µs    59.35µs
SQLite (WAL, sync=NORMAL) (index)                    235833     3.70µs     5.73µs     6.15µs    42.26µs
InlaySQL (B-tree index) is 579.55x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               59002    16.16µs    20.90µs    29.66µs    29.94µs
InlaySQL (no index: full scan)                          370     2.70ms     2.74ms     2.77ms     2.92ms
SQLite (journal, sync=FULL, fullfsync) (index)        54024    18.04µs    20.53µs    29.22µs    31.07µs
SQLite (WAL, sync=NORMAL) (index)                     78204    12.42µs    15.02µs    16.73µs    24.12µs
InlaySQL (B-tree index) is 162.34x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-10-12T11:46:36Z
commit: 60a0bda
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T114508Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T114537Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T114606Z.txt

metrics: 74; disagreeing by 10% or more across runs: 21

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   71.6%         p95        7.32µs        7.29µs       12.53µs  SQLite (WAL, sync=NORMAL) (index)
   59.1%        cold       15.59µs       15.45µs       24.67µs  SQLite (journal, sync=FULL, fullfsync) (index)
   56.6%         p99       16.74µs        7.96µs       17.44µs  SQLite (WAL, sync=NORMAL) (index)
   40.0%         max       28.08ms       28.07ms       39.29ms  SQLite (journal, sync=FULL, fullfsync) (index)
   38.3%         p99       15.59µs       15.45µs       21.42µs  SQLite (journal, sync=FULL, fullfsync) (index)
   25.6%        cold        9.78µs        8.70µs       11.20µs  SQLite (WAL, sync=NORMAL) (index)
   25.6%         p99        9.78µs        8.70µs       11.20µs  SQLite (WAL, sync=NORMAL) (index)
   25.4%         max       32.52ms       30.18ms       38.44ms  SQLite (WAL, sync=NORMAL) (index)
   23.2%         p99       28.05ms       28.02ms       34.53ms  SQLite (journal, sync=FULL, fullfsync) (index)
   22.7%         max       21.00µs       19.90µs       24.67µs  SQLite (journal, sync=FULL, fullfsync) (index)
   20.2%         p99       16.45ms       13.89ms       17.22ms  InlaySQL
   18.2%        cold       31.14µs       27.00µs       32.68µs  SQLite (journal, sync=FULL, fullfsync) (index)
   18.2%         max       31.14µs       27.00µs       32.68µs  SQLite (journal, sync=FULL, fullfsync) (index)
   14.7%         max       90.50ms       79.30ms       92.57ms  SQLite (journal, sync=FULL, fullfsync) (index)
   14.6%         p99       28.63ms       28.23ms       32.42ms  SQLite (WAL, sync=NORMAL) (index)
   13.1%        cold       46.11µs       43.59µs       49.65µs  InlaySQL
   13.1%         max       46.11µs       43.59µs       49.65µs  InlaySQL
   13.1%         max      123.55µs      119.98µs      136.13µs  InlaySQL
   13.1%        cold      123.55µs      119.98µs      136.13µs  InlaySQL
   12.2%         p95       17.41µs       16.40µs       18.52µs  InlaySQL
   11.8%        cold       17.44µs       16.74µs       18.80µs  SQLite (WAL, sync=NORMAL) (index)

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 81     49.38ms    11.93ms    12.91ms    13.36ms    49.38ms
SQLite (journal, sync=FULL, fullfsync) (index)           36     27.69ms    27.74ms    27.92ms    28.05ms    28.08ms
SQLite (WAL, sync=NORMAL) (index)                        36     27.82ms    27.83ms    28.20ms    28.63ms    32.52ms
InlaySQL is 2.28x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              98973    46.11µs     9.49µs     9.87µs    22.69µs    46.11µs
SQLite (journal, sync=FULL, fullfsync) (index)        97110    15.59µs    10.07µs    10.18µs    15.59µs    21.00µs
SQLite (WAL, sync=NORMAL) (index)                    209757     9.78µs     4.55µs     4.62µs     9.78µs    14.23µs
InlaySQL is 1.03x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 76     82.06ms    12.34ms    13.21ms    16.45ms    82.06ms
SQLite (journal, sync=FULL, fullfsync) (index)           13     76.17ms    76.66ms    78.23ms    80.93ms    90.50ms
SQLite (WAL, sync=NORMAL) (index)                        13     76.12ms    76.47ms    77.30ms    78.88ms    79.25ms
InlaySQL is 5.80x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              65718   123.55µs    13.62µs    17.41µs    28.10µs   123.55µs
SQLite (journal, sync=FULL, fullfsync) (index)        75769    31.14µs    12.81µs    13.12µs    23.92µs    31.14µs
SQLite (WAL, sync=NORMAL) (index)                    134504    17.44µs     7.19µs     7.32µs    16.74µs    18.80µs
InlaySQL is 1.17x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-10-12T11:46:45Z
commit: 60a0bda
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T114636Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T114639Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261012T114642Z.txt

metrics: 252; disagreeing by 10% or more across runs: 56

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  200.0%      lock.)             0             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   88.0%         max        4.60ms        3.82ms        7.87ms  SQLite (journal, sync=FULL, fullfsync)
   76.9%         max        4.16ms        4.06ms        7.26ms  SQLite (journal, sync=FULL, fullfsync)
   71.8%         p95        0.75ms        0.69ms        1.23ms  InlaySQL (parallel WAL regions)
   63.6%         p99        1.07ms        1.06ms        1.74ms  SQLite (journal, sync=FULL, fullfsync)
   60.5%         p99     1200.00µs      533.84µs     1260.00µs  InlaySQL (parallel WAL regions)
   55.5%         p99        1.37ms        1.28ms        2.04ms  SQLite (journal, sync=FULL, fullfsync)
   54.8%         max       11.56ms        9.71ms       16.04ms  InlaySQL (parallel WAL regions)
   53.1%         max        3.11ms        1.75ms        3.40ms  InlaySQL (parallel WAL regions)
   51.0%      lock.)           5.1             5           7.6  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   49.4%         p99        4.09ms        3.33ms        5.35ms  InlaySQL (parallel WAL regions)
   49.4%         max        4.70ms        4.10ms        6.42ms  SQLite (journal, sync=FULL, fullfsync)
   49.0%         p99        1.92ms        1.90ms        2.84ms  InlaySQL (parallel WAL regions)
   46.3%      lock.)          0.27          0.26          0.38  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   44.8%      lock.)         5.80%         3.80%         6.40%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   44.2%         max        6.33ms        3.79ms        6.59ms  SQLite (journal, sync=FULL, fullfsync)
   34.8%      lock.)        46.50%        32.10%        48.30%  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   33.3%      lock.)          0.03          0.03          0.04  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   33.3%      lock.)        12.00%         9.00%        13.00%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   33.3%      lock.)         0.30%         0.20%         0.30%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   30.0%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   29.2%      lock.)          0.17          0.15           0.2  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   29.2%      lock.)        27.10%        26.60%        34.50%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   28.7%      lock.)        16.70%        13.30%        18.10%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   28.0%      lock.)        10.70%         8.30%        11.30%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 3614.8 barriers/s — fsync  0.17 ms, interval  0.28 ms, idle  0.11 ms (37.40% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.15 ms/barrier
  buckets: 1 writers, busy 55.2 ms over 200 commits — gate_wait 0.00%, gate_hold 26.60%, follower_wait 0.00%, gather_spin 0.00%, fsync 63.40%, post 0.30%, pre-gate residual 9.90% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.07 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.03 ms (38.80%), residual  0.05 ms (61.20%), 0 commit-point misses
  barriers: 2 writers, 228 normal flushes over 400 commits ( 0.57 syncs/commit, 1.75 commits/sync)
  barrier cycle: 2 writers, 1996.5 barriers/s — fsync  0.27 ms, interval   0.5 ms, idle  0.23 ms (46.50% of the wall clock has no flush in flight); coordinator gather   0.1 post     0 gap  0.17 ms/barrier
  buckets: 2 writers, busy 225.4 ms over 400 commits — gate_wait 5.80%, gate_hold 16.70%, follower_wait 26.60%, gather_spin 10.70%, fsync 27.10%, post 0.40%, pre-gate residual 12.00% (402 gate waits, 208 racing holds)
  gate hold: 2 writers, 402 holds,  0.09 ms mean —              read  0.01 (961 calls), state     0 (0), wal     0 (400, 5.1 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.03 ms (32.10%), residual  0.06 ms (67.90%), 1 commit-point misses
  barriers: 4 writers, 216 normal flushes over 800 commits ( 0.27 syncs/commit, 3.71 commits/sync)
  barrier cycle: 4 writers, 1163.4 barriers/s — fsync  0.37 ms, interval  0.86 ms, idle  0.49 ms (56.70% of the wall clock has no flush in flight); coordinator gather  0.34 post  0.01 gap  0.16 ms/barrier
  buckets: 4 writers, busy 761.2 ms over 800 commits — gate_wait 17.60%, gate_hold 11.50%, follower_wait 42.00%, gather_spin 9.70%, fsync 11.00%, post 0.30%, pre-gate residual 7.70% (802 gate waits, 604 racing holds)
  gate hold: 4 writers, 802 holds,  0.11 ms mean —              read  0.01 (2858 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.03 ms (31.40%), residual  0.07 ms (68.60%), 3 commit-point misses
  barriers: 8 writers, 213 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.55 commits/sync)
  barrier cycle: 8 writers, 642.1 barriers/s — fsync  0.49 ms, interval  1.56 ms, idle  1.08 ms (68.50% of the wall clock has no flush in flight); coordinator gather  0.85 post  0.03 gap  0.23 ms/barrier
  buckets: 8 writers, busy   2621 ms over 1600 commits — gate_wait 25.20%, gate_hold 7.30%, follower_wait 50.30%, gather_spin 6.90%, fsync 4.00%, post 0.20%, pre-gate residual 5.40% (1604 gate waits, 1400 racing holds)
  gate hold: 8 writers, 1604 holds,  0.12 ms mean —              read  0.01 (6459 calls), state     0 (8), wal  0.01 (1608, 11.2 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.04 ms (29.40%), residual  0.08 ms (70.60%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         3615          200       0.00%   223.28µs   414.10µs   1200.00µs     3.11ms
InlaySQL (parallel WAL regions)                 2         3499          400       0.00%   546.89µs     0.75ms     1.92ms     3.38ms
InlaySQL (parallel WAL regions)                 4         4174          800       0.00%   865.80µs     1.37ms     4.09ms     7.23ms
InlaySQL (parallel WAL regions)                 8         4846         1600       0.00%     1.45ms     2.92ms     5.91ms     11.56ms
SQLite (journal, sync=FULL, fullfsync)          1         1338          200       0.00%   697.95µs   864.55µs     1.07ms     6.33ms
SQLite (journal, sync=FULL, fullfsync)          2         1316          400       0.00%   710.66µs   877.13µs     1.07ms     4.60ms
SQLite (journal, sync=FULL, fullfsync)          4         1285          800       0.00%   728.40µs   940.77µs     1.56ms     4.16ms
SQLite (journal, sync=FULL, fullfsync)          8         1292         1600       0.00%   712.54µs   931.34µs     1.37ms     4.70ms

InlaySQL at 8 writers does 1.35x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-10-12T11:28:03Z
commit: 60a0bda
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  172.00us  239.00us |   0.988  288.00us  362.00us     3.7s
DuckDB (exhaustive + fts BM25)         1.000   17.40ms   22.80ms |   0.966   34.87ms   45.14ms    78.9s
DuckDB (vss HNSW + fts BM25)           0.992   15.74ms   17.70ms |   0.961   34.08ms   44.34ms    79.8s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.999    3.21ms    5.27ms |   0.418   10.51ms   12.37ms     3.8s
pgvector (HNSW + ts_rank)              0.987  466.00us  534.00us |   0.458   37.34ms   57.77ms     1.5s
pgvector (exhaustive + ts_rank)        0.999    1.26ms    1.33ms |   0.465   37.99ms   59.05ms     0.5s

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
InlaySQL                                                              3315.2  235.00us  439.00us    1.00ms |    917310.6    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       3236.5  238.00us  462.00us    1.26ms |    857982.6    1.00us    2.00us    2.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           2501.2  374.00us  519.00us  849.00us |      4031.1  227.00us  296.00us  345.00us
                                                                   commits-per-fsync: 20003/20696 = 0.97
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       4328.5  215.00us  279.00us  368.00us |      8159.6  108.00us  149.00us  174.00us
                                                                   commits-per-fsync: 20005/20000 = 1.00

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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1736.9  507.00us  650.00us    1.57ms       0 |      3258.7  192.00us  239.00us  250.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      2552.8    1.03ms    1.97ms    5.01ms       0 |      4873.7  209.00us  250.00us  275.00us
                                                                                      commits-per-fsync: 2009/578 = 3.48
                                                                                      commits-per-fsync (checkpoint-inclusive): 2016/585 = 3.45
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1507.4    3.08ms    8.66ms   24.53ms       0 |      1327.1  452.00us    2.35ms    4.55ms
                                                                                      commits-per-fsync: 2015/406 = 4.96
                                                                                      commits-per-fsync (checkpoint-inclusive): 2025/416 = 4.87
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2173.8  374.00us  448.00us  752.00us       0 |      2802.5  225.00us  334.00us  398.00us
                                                                                      commits-per-fsync: 2003/2094 = 0.96
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      4109.8  567.00us  947.00us    1.31ms       0 |      3289.1  332.00us  534.00us  662.00us
                                                                                      commits-per-fsync: 2003/1363 = 1.47
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      1864.1    1.01ms    3.00ms    5.34ms       0 |      1163.9  300.00us    2.65ms    4.35ms
                                                                                      commits-per-fsync: 2003/1392 = 1.44

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

