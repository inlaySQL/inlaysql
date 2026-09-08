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

- generated: 2026-09-08T02:41:08Z
- commit: 6556115
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-09-08T02:13:05Z
commit: 6556115
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T021032Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T021206Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T021236Z.txt

metrics: 46; disagreeing by 10% or more across runs: 17

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  295.0%         max       39.34ms       26.63ms      142.70ms  SQLite (journal, sync=FULL, fullfsync)
  295.0%         max       39.34ms       26.63ms      142.70ms  SQLite (journal, sync=FULL, fullfsync)
   64.2%         p99        3.44µs        3.26µs        5.47µs  InlaySQL (batched)
   54.9%         max       65.49ms       63.49ms       99.47ms  InlaySQL
   54.9%         max       65.49ms       63.49ms       99.47ms  InlaySQL
   53.2%         max       34.32ms       16.66ms       34.91ms  SQLite (WAL, sync=NORMAL)
   47.7%         max       35.92µs       29.04µs       46.16µs  SQLite (WAL, sync=NORMAL)
   32.4%         p99        1.79ms        1.35ms        1.93ms  SQLite (journal, sync=FULL, fullfsync)
   32.4%         p99        1.79ms        1.35ms        1.93ms  SQLite (journal, sync=FULL, fullfsync)
   20.3%         max       23.37ms       18.68ms       23.43ms  InlaySQL (batched)
   18.7%         max       24.65µs       23.35µs       27.96µs  InlaySQL
   18.3%      engine        82.75x        70.49x        85.66x  InlaySQL (batched) is faster than InlaySQL
   17.3%       ops/s        161812        151146        179106  InlaySQL (batched)
   12.5%         p95      896.20µs      883.87µs      996.01µs  SQLite (journal, sync=FULL, fullfsync)
   12.5%         p95      896.20µs      883.87µs      996.01µs  SQLite (journal, sync=FULL, fullfsync)
   11.1%       ops/s          1131          1090          1216  SQLite (journal, sync=FULL, fullfsync)
   11.1%       ops/s          1131          1090          1216  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         2091   233.20µs   433.99µs     3.25ms    65.49ms
SQLite (journal, sync=FULL, fullfsync)           1131   783.93µs   896.20µs     1.79ms    39.34ms
SQLite (WAL, sync=NORMAL)                       39379     9.35µs    10.95µs    17.65µs    34.32ms
InlaySQL is 1.79x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             161812     2.54µs     2.94µs     3.44µs    23.37ms
InlaySQL                                         2091   233.20µs   433.99µs     3.25ms    65.49ms
SQLite (journal, sync=FULL, fullfsync)           1131   783.93µs   896.20µs     1.79ms    39.34ms
InlaySQL (batched) is 82.75x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1700106   541.00ns   661.00ns   801.00ns    24.65µs
SQLite (journal, sync=FULL, fullfsync)         121170     8.11µs     8.46µs     10.12µs    52.78µs
SQLite (WAL, sync=NORMAL)                      371813     2.61µs     2.81µs     3.09µs    35.92µs
InlaySQL is 13.99x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-09-08T02:34:11Z
commit: 6556115
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T021306Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T022004Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T022708Z.txt

metrics: 42; disagreeing by 10% or more across runs: 11

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   34.8%         max        2.67ms        2.31ms        3.24ms  InlaySQL (no index: full scan)
   29.9%         p95        3.58µs        3.57µs        4.64µs  InlaySQL (B-tree index)
   28.7%         max       45.67µs       40.55µs       53.64µs  InlaySQL (B-tree index)
   22.7%         max       23.93µs       23.51µs       28.94µs  SQLite (journal, sync=FULL, fullfsync) (index)
   22.4%         max       49.93µs       41.13µs       52.32µs  SQLite (WAL, sync=NORMAL) (index)
   20.9%         p99       16.06µs       14.47µs       17.82µs  SQLite (WAL, sync=NORMAL) (index)
   19.9%         p99       23.50µs       19.63µs       24.30µs  SQLite (journal, sync=FULL, fullfsync) (index)
   18.2%         max       18.73µs       17.68µs       21.09µs  SQLite (WAL, sync=NORMAL) (index)
   14.9%         p99        2.35ms        2.31ms        2.66ms  InlaySQL (no index: full scan)
   12.9%         p99        5.13µs        5.00µs        5.66µs  InlaySQL (B-tree index)
   10.3%         p99       22.03µs       19.94µs       22.20µs  InlaySQL (B-tree index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              305786     3.16µs     3.58µs     5.13µs    45.67µs
InlaySQL (no index: full scan)                          478     2.09ms     2.12ms     2.15ms     4.58ms
SQLite (journal, sync=FULL, fullfsync) (index)       106684     8.93µs    10.62µs    12.79µs    62.69µs
SQLite (WAL, sync=NORMAL) (index)                    259249     3.36µs     5.33µs     5.68µs    49.93µs
InlaySQL (B-tree index) is 635.07x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               73560    13.20µs    15.58µs    22.03µs    22.74µs
InlaySQL (no index: full scan)                          439     2.26ms     2.30ms     2.35ms     2.67ms
SQLite (journal, sync=FULL, fullfsync) (index)        61339    15.84µs    18.78µs    23.50µs    23.93µs
SQLite (WAL, sync=NORMAL) (index)                     93520    10.48µs    12.83µs    16.06µs    18.73µs
InlaySQL (B-tree index) is 166.42x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-09-08T02:35:33Z
commit: 6556115
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T023411Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T023439Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T023507Z.txt

metrics: 74; disagreeing by 10% or more across runs: 12

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   97.1%         p99        4.50µs        4.38µs        8.75µs  SQLite (WAL, sync=NORMAL) (index)
   22.3%         max       10.14µs        8.83µs       11.09µs  SQLite (WAL, sync=NORMAL) (index)
   15.6%        cold        8.83µs        8.75µs       10.13µs  SQLite (WAL, sync=NORMAL) (index)
   14.1%         max      125.22µs      119.10µs      136.76µs  InlaySQL
   14.1%        cold      125.22µs      119.11µs      136.76µs  InlaySQL
   13.3%         max       17.32µs       16.84µs       19.15µs  SQLite (journal, sync=FULL, fullfsync) (index)
   13.3%        cold       17.32µs       16.84µs       19.15µs  SQLite (journal, sync=FULL, fullfsync) (index)
   13.3%         p95       12.28µs       12.02µs       13.65µs  InlaySQL
   10.7%         p99       11.07ms       10.93ms       12.11ms  InlaySQL
   10.5%         p99       20.10µs       19.12µs       21.23µs  SQLite (journal, sync=FULL, fullfsync) (index)
   10.3%         max       17.85µs       16.06µs       17.89µs  SQLite (WAL, sync=NORMAL) (index)
   10.2%        cold       17.85µs       16.06µs       17.89µs  SQLite (WAL, sync=NORMAL) (index)

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                107     40.22ms     9.00ms     9.16ms     9.66ms    40.22ms
SQLite (journal, sync=FULL, fullfsync) (index)           45     21.73ms    22.25ms    22.36ms    22.46ms    22.72ms
SQLite (WAL, sync=NORMAL) (index)                        45     22.16ms    22.30ms    22.39ms    22.48ms    22.75ms
InlaySQL is 2.40x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                             129566    41.66µs     7.21µs     7.66µs    15.77µs    41.66µs
SQLite (journal, sync=FULL, fullfsync) (index)       103276    17.32µs     9.46µs     9.70µs    16.29µs    17.32µs
SQLite (WAL, sync=NORMAL) (index)                    244413     8.83µs     4.01µs     4.12µs     4.50µs     10.14µs
InlaySQL is 1.25x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 91     70.74ms    10.38ms    10.93ms    11.07ms    70.74ms
SQLite (journal, sync=FULL, fullfsync) (index)           17     58.73ms    58.88ms    59.26ms    59.72ms    60.07ms
SQLite (WAL, sync=NORMAL) (index)                        17     59.26ms    59.40ms    59.73ms    59.98ms    60.00ms
InlaySQL is 5.53x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              81410   125.22µs    10.83µs    12.28µs    19.16µs   125.22µs
SQLite (journal, sync=FULL, fullfsync) (index)        84875    29.12µs    11.40µs    11.81µs    20.10µs    29.12µs
SQLite (WAL, sync=NORMAL) (index)                    164325    17.85µs     5.84µs     6.19µs    13.48µs    17.85µs
InlaySQL is 1.03x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-09-08T02:35:44Z
commit: 6556115
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T023533Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T023537Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260908T023540Z.txt

metrics: 252; disagreeing by 10% or more across runs: 53

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  270.3%         max        5.22ms        2.39ms       16.50ms  SQLite (journal, sync=FULL, fullfsync)
  181.3%         p99        1.12ms        1.09ms        3.12ms  SQLite (journal, sync=FULL, fullfsync)
  155.5%         max       13.07ms        7.39ms       27.72ms  SQLite (journal, sync=FULL, fullfsync)
  120.0%      lock.)         1.50%        -0.10%         1.70%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
  108.5%         max        4.66ms        0.92ms        5.98ms  SQLite (journal, sync=FULL, fullfsync)
   82.8%      lock.)         2.90%         0.80%         3.20%  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   81.8%      lock.)          0.01             0          0.01  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   75.9%         max       11.92ms        4.36ms       13.41ms  SQLite (journal, sync=FULL, fullfsync)
   71.8%         p99       16.15ms       16.00ms       27.59ms  InlaySQL (parallel WAL regions)
   67.6%         p99       12.73ms        7.59ms       16.19ms  InlaySQL (parallel WAL regions)
   60.8%      lock.)          0.18          0.17          0.28  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   44.8%         p99        5.83ms        3.56ms        6.17ms  InlaySQL (parallel WAL regions)
   43.0%         max       26.98ms       26.72ms       38.31ms  InlaySQL (parallel WAL regions)
   40.0%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   39.6%         p99        3.03ms        1.96ms        3.16ms  InlaySQL (parallel WAL regions)
   39.1%         p95      617.41µs      591.52µs      833.19µs  InlaySQL (parallel WAL regions)
   38.9%         p99        1.13ms        1.12ms        1.56ms  SQLite (journal, sync=FULL, fullfsync)
   38.3%         max       28.01ms       27.92ms       38.66ms  InlaySQL (parallel WAL regions)
   35.5%      lock.)        -6.20%        -7.60%        -5.40%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   33.3%      lock.)             0             0             0  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   33.3%      lock.)             0             0             0  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   32.0%      lock.)         2.50%         2.40%         3.20%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   30.0%      lock.)         2.00%         2.00%         2.60%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   28.6%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   25.0%      lock.)         0.40%         0.40%         0.50%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 3065.3 barriers/s — fsync  0.28 ms, interval  0.33 ms, idle  0.04 ms (13.30% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.12 ms/barrier
  buckets: 1 writers, busy 65.1 ms over 200 commits — gate_wait 0.00%, gate_hold 18.50%, follower_wait 0.00%, gather_spin 0.00%, fsync 87.70%, post 0.20%, pre-gate residual -6.20% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.06 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.02 ms (38.30%), residual  0.04 ms (61.70%), 0 commit-point misses
  barriers: 2 writers, 300 normal flushes over 400 commits ( 0.75 syncs/commit, 1.33 commits/sync)
  barrier cycle: 2 writers,   2779 barriers/s — fsync  0.35 ms, interval  0.36 ms, idle  0.01 ms (2.90% of the wall clock has no flush in flight); coordinator gather  0.04 post     0 gap  0.08 ms/barrier
  buckets: 2 writers, busy 227.6 ms over 400 commits — gate_wait 2.00%, gate_hold 12.90%, follower_wait 29.00%, gather_spin 5.00%, fsync 49.10%, post 0.40%, pre-gate residual 1.50% (402 gate waits, 267 racing holds)
  gate hold: 2 writers, 402 holds,  0.07 ms mean —              read  0.01 (538 calls), state     0 (0), wal     0 (400,   5 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.02 ms (31.00%), residual  0.05 ms (69.00%), 1 commit-point misses
  barriers: 4 writers, 206 normal flushes over 800 commits ( 0.26 syncs/commit, 3.89 commits/sync)
  barrier cycle: 4 writers, 988.2 barriers/s — fsync   0.6 ms, interval  1.01 ms, idle   0.4 ms (40.40% of the wall clock has no flush in flight); coordinator gather  0.28 post  0.01 gap  0.13 ms/barrier
  buckets: 4 writers, busy 822.7 ms over 800 commits — gate_wait 13.00%, gate_hold 8.40%, follower_wait 51.50%, gather_spin 6.80%, fsync 15.40%, post 0.20%, pre-gate residual 4.70% (803 gate waits, 603 racing holds)
  gate hold: 4 writers, 803 holds,  0.09 ms mean —              read  0.01 (2867 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.03 ms (31.70%), residual  0.06 ms (68.30%), 3 commit-point misses
  barriers: 8 writers, 216 normal flushes over 1600 commits ( 0.14 syncs/commit, 7.44 commits/sync)
  barrier cycle: 8 writers, 546.7 barriers/s — fsync  1.05 ms, interval  1.83 ms, idle  0.78 ms (42.50% of the wall clock has no flush in flight); coordinator gather  0.67 post  0.02 gap  0.18 ms/barrier
  buckets: 8 writers, busy 3128.1 ms over 1600 commits — gate_wait 17.80%, gate_hold 5.20%, follower_wait 62.30%, gather_spin 4.60%, fsync 7.40%, post 0.10%, pre-gate residual 2.50% (1604 gate waits, 1401 racing holds)
  gate hold: 8 writers, 1604 holds,   0.1 ms mean —              read  0.01 (6339 calls), state     0 (8), wal  0.01 (1608, 11.4 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.03 ms (28.40%), residual  0.07 ms (71.60%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         3065          200       0.00%   230.07µs   425.12µs     3.03ms     5.88ms
InlaySQL (parallel WAL regions)                 2         3498          400       0.00%   440.04µs   617.41µs     5.83ms    13.02ms
InlaySQL (parallel WAL regions)                 4         3875          800       0.00%   707.85µs     1.90ms    12.73ms    26.98ms
InlaySQL (parallel WAL regions)                 8         4068         1600       0.00%     1.21ms     5.96ms    16.15ms    28.01ms
SQLite (journal, sync=FULL, fullfsync)          1         1223          200       0.00%   791.87µs   857.04µs     0.97ms     4.66ms
SQLite (journal, sync=FULL, fullfsync)          2         1222          400       0.00%   797.83µs   949.35µs     1.12ms     5.22ms
SQLite (journal, sync=FULL, fullfsync)          4         1201          800       0.00%   795.53µs   892.84µs     1.39ms     11.92ms
SQLite (journal, sync=FULL, fullfsync)          8         1199         1600       0.00%   796.47µs   891.49µs     1.13ms    13.07ms

InlaySQL at 8 writers does 1.36x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-09-08T02:19:48Z
commit: 6556115
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  172.00us  234.00us |   0.988  302.00us  371.00us     3.5s
DuckDB (exhaustive + fts BM25)         1.000   14.83ms   16.30ms |   0.965   31.22ms   37.55ms    65.9s
DuckDB (vss HNSW + fts BM25)           0.991   13.22ms   17.75ms |   0.961   30.17ms   37.06ms    68.5s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.998    3.30ms    3.68ms |   0.418   10.91ms   13.70ms     4.1s
pgvector (HNSW + ts_rank)              0.988  391.00us  497.00us |   0.457   34.84ms   52.15ms     1.5s
pgvector (exhaustive + ts_rank)        0.999    1.26ms    1.36ms |   0.465   36.22ms   54.07ms     0.5s

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
InlaySQL                                                              3796.8  202.00us  403.00us  704.00us |    909706.5    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       3686.4  207.00us  407.00us    1.09ms |    877624.5    1.00us    1.00us    2.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           3094.3  310.00us  400.00us  633.00us |      5261.4  168.00us  250.00us  294.00us
                                                                   commits-per-fsync: 20003/21504 = 0.93
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       5383.2  174.00us  235.00us  274.00us |     11452.9   71.00us  125.00us  144.00us
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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1944.3  466.00us  584.00us  977.00us       0 |      3353.6  189.00us  221.00us  291.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      2559.1    1.03ms    1.95ms    4.97ms       0 |      4518.8  271.00us  415.00us  494.00us
                                                                                      commits-per-fsync: 2011/593 = 3.39
                                                                                      commits-per-fsync (checkpoint-inclusive): 2015/597 = 3.38
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1670.3    2.79ms    6.41ms   12.23ms       0 |      1414.2  399.00us    1.77ms    3.89ms
                                                                                      commits-per-fsync: 2013/420 = 4.79
                                                                                      commits-per-fsync (checkpoint-inclusive): 2022/429 = 4.71
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2552.8  308.00us  387.00us  634.00us       0 |      3265.8  203.00us  247.00us  255.00us
                                                                                      commits-per-fsync: 2003/2122 = 0.94
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      4487.7  481.00us  709.00us  934.00us       0 |      3843.6  317.00us  577.00us  876.00us
                                                                                      commits-per-fsync: 2003/1443 = 1.39
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      1942.6  699.00us    2.37ms    4.53ms       0 |      1250.5  276.00us    2.15ms    3.34ms
                                                                                      commits-per-fsync: 2003/1629 = 1.23

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

