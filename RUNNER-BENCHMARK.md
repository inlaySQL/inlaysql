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

- generated: 2026-09-21T09:47:34Z
- commit: a220e77
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-09-21T09:18:50Z
commit: a220e77
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T091622Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T091807Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T091828Z.txt

metrics: 46; disagreeing by 10% or more across runs: 17

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  102.3%         max       66.05µs       54.36µs      121.90µs  SQLite (journal, sync=FULL, fullfsync)
   75.2%         max        9.05ms        8.83ms       15.64ms  InlaySQL
   75.2%         max        9.05ms        8.83ms       15.64ms  InlaySQL
   68.5%         max       42.39µs       36.01µs       65.06µs  SQLite (WAL, sync=NORMAL)
   48.7%         p99        0.76ms        0.76ms        1.13ms  InlaySQL
   48.7%         p99        0.76ms        0.76ms        1.13ms  InlaySQL
   45.2%         p99        4.87µs        4.53µs        6.73µs  InlaySQL (batched)
   33.5%         max       28.80µs       25.89µs       35.55µs  InlaySQL
   28.9%         p99        1.21ms        1.19ms        1.54ms  SQLite (journal, sync=FULL, fullfsync)
   28.9%         p99        1.21ms        1.19ms        1.54ms  SQLite (journal, sync=FULL, fullfsync)
   22.0%         max        3.63ms        3.36ms        4.16ms  SQLite (WAL, sync=NORMAL)
   13.8%         p95      408.02µs      394.27µs      450.38µs  InlaySQL
   13.8%         p95      408.02µs      394.27µs      450.38µs  InlaySQL
   13.7%         max        9.96ms        9.03ms       10.39ms  SQLite (journal, sync=FULL, fullfsync)
   13.7%         max        9.96ms        9.03ms       10.39ms  SQLite (journal, sync=FULL, fullfsync)
   12.9%         p95      787.42µs      764.58µs      866.35µs  SQLite (journal, sync=FULL, fullfsync)
   12.9%         p95      787.42µs      764.58µs      866.35µs  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         3736   204.09µs   408.02µs     0.76ms     9.05ms
SQLite (journal, sync=FULL, fullfsync)           1552   608.62µs   787.42µs     1.21ms     9.96ms
SQLite (WAL, sync=NORMAL)                       66302    11.95µs    14.02µs    22.32µs     3.63ms
InlaySQL is 2.42x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             171620     3.30µs     3.80µs     4.87µs    13.26ms
InlaySQL                                         3736   204.09µs   408.02µs     0.76ms     9.05ms
SQLite (journal, sync=FULL, fullfsync)           1552   608.62µs   787.42µs     1.21ms     9.96ms
InlaySQL (batched) is 45.99x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1295215   711.00ns   842.00ns     1.03µs    28.80µs
SQLite (journal, sync=FULL, fullfsync)          94126    10.44µs    10.83µs    17.92µs    66.05µs
SQLite (WAL, sync=NORMAL)                      288815     3.36µs     3.58µs     3.96µs    42.39µs
InlaySQL is 13.72x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-09-21T09:45:48Z
commit: a220e77
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T091851Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T092749Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T093649Z.txt

metrics: 42; disagreeing by 10% or more across runs: 11

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  140.9%         max       53.97µs       44.51µs      120.55µs  SQLite (WAL, sync=NORMAL) (index)
   66.1%         max        3.01ms        2.98ms        4.97ms  InlaySQL (no index: full scan)
   57.6%         p99        2.97ms        2.97ms        4.68ms  InlaySQL (no index: full scan)
   54.6%         max       28.24µs       27.55µs       42.98µs  InlaySQL (B-tree index)
   36.2%         max       71.00µs       66.55µs       92.25µs  SQLite (journal, sync=FULL, fullfsync) (index)
   31.8%         max       55.48µs       42.24µs       59.90µs  InlaySQL (B-tree index)
   30.5%         p95        2.95ms        2.94ms        3.84ms  InlaySQL (no index: full scan)
   24.3%         p99       19.64µs       18.71µs       23.48µs  SQLite (WAL, sync=NORMAL) (index)
   18.5%         max        5.68ms        5.03ms        6.08ms  InlaySQL (no index: full scan)
   12.5%         p99       30.18µs       28.19µs       31.96µs  SQLite (journal, sync=FULL, fullfsync) (index)
   10.1%       ops/s       159.71x       157.95x       174.08x  InlaySQL (B-tree index) is faster than InlaySQL (no index: full scan)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              235799     4.10µs     4.61µs     6.78µs    55.48µs
InlaySQL (no index: full scan)                          375     2.66ms     2.70ms     2.75ms     5.68ms
SQLite (journal, sync=FULL, fullfsync) (index)        82942    11.47µs    13.70µs    20.04µs    71.00µs
SQLite (WAL, sync=NORMAL) (index)                    199722     4.36µs     6.90µs     7.43µs    53.97µs
InlaySQL (B-tree index) is 630.38x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               54917    17.89µs    20.31µs    24.74µs    28.24µs
InlaySQL (no index: full scan)                          343     2.91ms     2.95ms     2.97ms     3.01ms
SQLite (journal, sync=FULL, fullfsync) (index)        47338    20.56µs    23.74µs    30.18µs    32.23µs
SQLite (WAL, sync=NORMAL) (index)                     72534    13.47µs    16.54µs    19.64µs    24.70µs
InlaySQL (B-tree index) is 159.71x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-09-21T09:47:17Z
commit: a220e77
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T094548Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T094617Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T094647Z.txt

metrics: 74; disagreeing by 10% or more across runs: 17

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   78.6%         p99        8.22µs        5.74µs       12.20µs  SQLite (WAL, sync=NORMAL) (index)
   74.5%         p99       16.76µs        9.05µs       21.54µs  SQLite (WAL, sync=NORMAL) (index)
   43.5%        cold       21.00µs       20.31µs       29.44µs  SQLite (journal, sync=FULL, fullfsync) (index)
   39.8%         max       21.18µs       21.00µs       29.44µs  SQLite (journal, sync=FULL, fullfsync) (index)
   33.2%         p99       13.54ms       13.10ms       17.59ms  InlaySQL
   26.0%         max       21.24µs       21.16µs       26.68µs  SQLite (WAL, sync=NORMAL) (index)
   18.9%         p95       12.91ms       12.59ms       15.03ms  InlaySQL
   16.9%         max       30.98ms       29.80ms       35.03ms  SQLite (journal, sync=FULL, fullfsync) (index)
   16.1%        cold      136.16µs      130.49µs      152.47µs  InlaySQL
   16.1%         max      136.16µs      130.49µs      152.47µs  InlaySQL
   16.1%         max       78.34ms       78.09ms       90.71ms  SQLite (WAL, sync=NORMAL) (index)
   15.8%         p95       15.82µs       15.49µs       17.99µs  InlaySQL
   14.5%         max       79.87ms       77.79ms       89.41ms  SQLite (journal, sync=FULL, fullfsync) (index)
   12.9%         p99       20.32µs       20.31µs       22.94µs  SQLite (journal, sync=FULL, fullfsync) (index)
   10.5%        cold       12.57µs       12.20µs       13.52µs  SQLite (WAL, sync=NORMAL) (index)
   10.2%         p50       12.12ms       11.75ms       12.99ms  InlaySQL
   10.1%     joins/s            79            74            82  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 79     51.15ms    12.12ms    12.91ms    13.54ms    51.15ms
SQLite (journal, sync=FULL, fullfsync) (index)           35     28.24ms    28.32ms    28.48ms    30.61ms    30.98ms
SQLite (WAL, sync=NORMAL) (index)                        35     28.19ms    28.31ms    28.53ms    29.83ms    30.07ms
InlaySQL is 2.13x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                             100936    50.77µs     9.29µs     9.90µs    19.96µs    50.77µs
SQLite (journal, sync=FULL, fullfsync) (index)        80335    21.00µs    12.14µs    12.28µs    20.32µs    21.18µs
SQLite (WAL, sync=NORMAL) (index)                    191269    12.57µs     5.09µs     5.19µs     8.22µs    13.47µs
InlaySQL is 1.26x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 72     87.33ms    13.16ms    13.85ms    14.10ms    87.33ms
SQLite (journal, sync=FULL, fullfsync) (index)           13     76.28ms    76.36ms    76.90ms    77.69ms    79.87ms
SQLite (WAL, sync=NORMAL) (index)                        13     76.97ms    76.58ms    76.92ms    78.16ms    78.34ms
InlaySQL is 5.45x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              64713   136.16µs    13.84µs    15.82µs    24.79µs   136.16µs
SQLite (journal, sync=FULL, fullfsync) (index)        66655    35.11µs    14.62µs    15.14µs    23.04µs    35.11µs
SQLite (WAL, sync=NORMAL) (index)                    126153    21.24µs     7.54µs     7.98µs    16.76µs    21.24µs
InlaySQL is 1.02x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-09-21T09:47:26Z
commit: a220e77
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T094717Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T094720Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260921T094723Z.txt

metrics: 252; disagreeing by 10% or more across runs: 49

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  110.5%         max        6.36ms        1.90ms        8.93ms  SQLite (journal, sync=FULL, fullfsync)
  100.0%      lock.)             1             0             1  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   96.1%         p99        1.54ms        1.06ms        2.54ms  SQLite (journal, sync=FULL, fullfsync)
   90.8%         p99      761.99µs      588.31µs     1280.00µs  InlaySQL (parallel WAL regions)
   85.7%      lock.)          0.01             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   77.5%         p99        1.33ms        0.94ms        1.97ms  SQLite (journal, sync=FULL, fullfsync)
   73.4%         max        5.33ms        4.83ms        8.74ms  SQLite (journal, sync=FULL, fullfsync)
   48.1%         p99        5.51ms        5.44ms        8.09ms  InlaySQL (parallel WAL regions)
   46.8%         p95      970.49µs      835.70µs     1290.00µs  SQLite (journal, sync=FULL, fullfsync)
   44.9%         max       10.80ms        7.11ms       11.96ms  InlaySQL (parallel WAL regions)
   43.9%         max        2.28ms        1.59ms        2.59ms  SQLite (journal, sync=FULL, fullfsync)
   43.2%      lock.)          0.15          0.14           0.2  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   37.9%      lock.)          0.42          0.39          0.55  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   34.2%      lock.)           7.6             5           7.6  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   30.1%         p50      219.15µs      189.09µs      254.97µs  InlaySQL (parallel WAL regions)
   28.5%      lock.)          0.17          0.13          0.18  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   27.8%      lock.)         3.60%         3.30%         4.30%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   27.6%     writers         1.34x         1.09x         1.46x  InlaySQL at writers does the work of writer, aborting of transactions.
   26.9%         max        6.06ms        5.01ms        6.64ms  InlaySQL (parallel WAL regions)
   26.0%         p95        1.23ms        1.09ms        1.41ms  InlaySQL (parallel WAL regions)
   25.5%      lock.)         5.10%         4.70%         6.00%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   23.9%         p95      443.62µs      379.46µs      485.49µs  InlaySQL (parallel WAL regions)
   22.1%         max        2.08ms        1.79ms        2.25ms  SQLite (journal, sync=FULL, fullfsync)
   21.1%      lock.)        3730.8        3463.7        4252.3  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   21.1%   commits/s          3731          3464          4252  InlaySQL (parallel WAL regions)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 3730.8 barriers/s — fsync  0.17 ms, interval  0.27 ms, idle   0.1 ms (38.50% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.15 ms/barrier
  buckets: 1 writers, busy 53.5 ms over 200 commits — gate_wait 0.00%, gate_hold 28.40%, follower_wait 0.00%, gather_spin 0.00%, fsync 62.30%, post 0.30%, pre-gate residual 9.10% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.07 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.03 ms (38.30%), residual  0.05 ms (61.70%), 0 commit-point misses
  barriers: 2 writers, 211 normal flushes over 400 commits ( 0.53 syncs/commit,  1.9 commits/sync)
  barrier cycle: 2 writers, 2203.5 barriers/s — fsync  0.23 ms, interval  0.45 ms, idle  0.23 ms (50.20% of the wall clock has no flush in flight); coordinator gather   0.1 post     0 gap  0.15 ms/barrier
  buckets: 2 writers, busy 188.5 ms over 400 commits — gate_wait 6.00%, gate_hold 18.60%, follower_wait 24.80%, gather_spin 11.70%, fsync 25.80%, post 0.40%, pre-gate residual 12.40% (402 gate waits, 200 racing holds)
  gate hold: 2 writers, 402 holds,  0.09 ms mean —              read  0.01 ( 974 calls), state     0 (1), wal  0.01 (401, 7.6 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.03 ms (33.30%), residual  0.06 ms (66.70%), 1 commit-point misses
  barriers: 4 writers, 208 normal flushes over 800 commits ( 0.26 syncs/commit, 3.86 commits/sync)
  barrier cycle: 4 writers, 1130.9 barriers/s — fsync  0.34 ms, interval  0.88 ms, idle  0.53 ms (60.90% of the wall clock has no flush in flight); coordinator gather  0.37 post  0.01 gap  0.17 ms/barrier
  buckets: 4 writers, busy 729.5 ms over 800 commits — gate_wait 18.30%, gate_hold 12.30%, follower_wait 40.70%, gather_spin 10.40%, fsync 10.00%, post 0.30%, pre-gate residual 7.90% (803 gate waits, 600 racing holds)
  gate hold: 4 writers, 803 holds,  0.11 ms mean —              read  0.01 (2869 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.04 ms (33.70%), residual  0.07 ms (66.30%), 3 commit-point misses
  barriers: 8 writers, 211 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.62 commits/sync)
  barrier cycle: 8 writers, 657.7 barriers/s — fsync  0.42 ms, interval  1.52 ms, idle  1.12 ms (72.00% of the wall clock has no flush in flight); coordinator gather  0.88 post  0.03 gap  0.24 ms/barrier
  buckets: 8 writers, busy 2546.5 ms over 1600 commits — gate_wait 26.70%, gate_hold 7.60%, follower_wait 49.60%, gather_spin 7.20%, fsync 3.60%, post 0.20%, pre-gate residual 5.10% (1604 gate waits, 1394 racing holds)
  gate hold: 8 writers, 1604 holds,  0.12 ms mean —              read  0.01 (6451 calls), state     0 (8), wal  0.01 (1608, 11.2 KiB),              data  0.01 (1600, 19.6 KiB), of which extend  0.01 (6 extensions);              device  0.04 ms (30.00%), residual  0.08 ms (70.00%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         3731          200       0.00%   219.15µs   443.62µs   761.99µs     1.69ms
InlaySQL (parallel WAL regions)                 2         4190          400       0.00%   461.42µs   634.26µs     1.41ms     3.12ms
InlaySQL (parallel WAL regions)                 4         4350          800       0.00%   862.59µs     1.23ms     3.44ms     6.06ms
InlaySQL (parallel WAL regions)                 8         4987         1600       0.00%     1.45ms     2.60ms     5.51ms     10.80ms
SQLite (journal, sync=FULL, fullfsync)          1         1364          200       0.00%   683.46µs   970.49µs     1.33ms     2.28ms
SQLite (journal, sync=FULL, fullfsync)          2         1381          400       0.00%   674.43µs   929.07µs     1.18ms     2.08ms
SQLite (journal, sync=FULL, fullfsync)          4         1327          800       0.00%   686.42µs   967.44µs     1.52ms     6.36ms
SQLite (journal, sync=FULL, fullfsync)          8         1383         1600       0.00%   672.09µs   919.28µs     1.54ms     5.33ms

InlaySQL at 8 writers does 1.34x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-09-21T09:26:59Z
commit: a220e77
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  189.00us  349.00us |   0.988  332.00us  402.00us     4.2s
DuckDB (exhaustive + fts BM25)         1.000   17.12ms   24.26ms |   0.966   35.98ms   43.98ms    77.8s
DuckDB (vss HNSW + fts BM25)           0.993   15.44ms   18.62ms |   0.956   34.16ms   40.01ms    78.2s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.999    3.55ms    4.47ms |   0.418   11.36ms   13.38ms     4.0s
pgvector (HNSW + ts_rank)              0.987  421.00us  668.00us |   0.456   37.23ms   56.76ms     1.4s
pgvector (exhaustive + ts_rank)        0.999    1.30ms    2.16ms |   0.465   39.47ms   59.07ms     0.5s

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
InlaySQL                                                              2655.4  314.00us  619.00us    1.22ms |    896995.3    1.00us    1.00us    2.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       2760.2  264.00us  560.00us    1.49ms |    809116.4    1.00us    2.00us    2.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           2374.1  390.00us  547.00us  964.00us |      4000.9  231.00us  300.00us  350.00us
                                                                   commits-per-fsync: 20003/20885 = 0.96
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       3925.5  240.00us  318.00us  491.00us |      8116.1  109.00us  151.00us  179.00us
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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1541.4  573.00us  796.00us    2.32ms       0 |      3014.6  236.00us  253.00us  278.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2013/2013 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      2337.0    1.14ms    2.69ms    5.22ms       0 |      4135.2  274.00us  508.00us  624.00us
                                                                                      commits-per-fsync: 2008/578 = 3.47
                                                                                      commits-per-fsync (checkpoint-inclusive): 2015/585 = 3.44
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1485.2    3.70ms   11.04ms   17.46ms       0 |      1267.7  528.00us    2.36ms    5.63ms
                                                                                      commits-per-fsync: 2016/290 = 6.95
                                                                                      commits-per-fsync (checkpoint-inclusive): 2023/297 = 6.81
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2061.0  383.00us  532.00us  941.00us       0 |      2852.6  220.00us  288.00us  298.00us
                                                                                      commits-per-fsync: 2003/2085 = 0.96
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      4158.3  548.00us  972.00us    1.36ms       0 |      3554.3  348.00us  562.00us  672.00us
                                                                                      commits-per-fsync: 2003/1249 = 1.60
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      1778.4    1.04ms    3.57ms    6.58ms       0 |      1112.6  303.00us    1.90ms    3.72ms
                                                                                      commits-per-fsync: 2003/1395 = 1.44

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

