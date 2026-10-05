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

- generated: 2026-10-05T11:18:46Z
- commit: 79edf3a
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-10-05T10:51:58Z
commit: 79edf3a
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T104918Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T105107Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T105132Z.txt

metrics: 46; disagreeing by 10% or more across runs: 19

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
 1538.7%         max        5.84ms        5.22ms       95.08ms  SQLite (journal, sync=FULL, fullfsync)
 1538.7%         max        5.84ms        5.22ms       95.08ms  SQLite (journal, sync=FULL, fullfsync)
  112.8%         max       56.28µs       39.66µs      103.13µs  SQLite (WAL, sync=NORMAL)
   39.2%         p99       13.16µs       12.14µs       17.30µs  SQLite (journal, sync=FULL, fullfsync)
   39.2%         p99        1.94ms        1.83ms        2.59ms  SQLite (journal, sync=FULL, fullfsync)
   39.2%         p99        1.94ms        1.83ms        2.59ms  SQLite (journal, sync=FULL, fullfsync)
   31.1%         max       35.55µs       31.22µs       42.28µs  InlaySQL
   25.2%         p99        6.27µs        4.91µs        6.49µs  InlaySQL (batched)
   24.6%         p99        1.05µs        0.95µs        1.21µs  InlaySQL
   21.3%         max       14.55ms       12.58ms       15.68ms  InlaySQL (batched)
   19.9%         max      134.44µs      134.19µs      160.88µs  SQLite (journal, sync=FULL, fullfsync)
   18.8%         p99        1.38ms        1.15ms        1.41ms  InlaySQL
   18.8%         p99        1.38ms        1.15ms        1.41ms  InlaySQL
   18.3%         max        3.61ms        3.57ms        4.23ms  SQLite (WAL, sync=NORMAL)
   16.2%         p95      862.00ns      822.00ns      962.00ns  InlaySQL
   14.2%         p95        1.06ms        1.00ms        1.15ms  SQLite (journal, sync=FULL, fullfsync)
   14.2%         p95        1.06ms        1.00ms        1.15ms  SQLite (journal, sync=FULL, fullfsync)
   12.7%         max        8.57ms        7.75ms        8.84ms  InlaySQL
   12.7%         max        8.57ms        7.75ms        8.84ms  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         2954   266.82µs   516.93µs     1.38ms     8.57ms
SQLite (journal, sync=FULL, fullfsync)           1249   742.52µs     1.06ms     1.94ms     5.84ms
SQLite (WAL, sync=NORMAL)                       75332     9.99µs    11.77µs    22.56µs     3.61ms
InlaySQL is 2.53x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             170033     3.14µs     3.62µs     6.27µs    14.55ms
InlaySQL                                         2954   266.82µs   516.93µs     1.38ms     8.57ms
SQLite (journal, sync=FULL, fullfsync)           1249   742.52µs     1.06ms     1.94ms     5.84ms
InlaySQL (batched) is 57.07x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1296959   711.00ns   862.00ns     1.05µs    35.55µs
SQLite (journal, sync=FULL, fullfsync)         115631     8.44µs     8.81µs    13.16µs   134.44µs
SQLite (WAL, sync=NORMAL)                      331146     2.86µs     3.14µs     3.71µs    56.28µs
InlaySQL is 11.22x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-10-05T11:16:56Z
commit: 79edf3a
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T105158Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T110013Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T110834Z.txt

metrics: 42; disagreeing by 10% or more across runs: 8

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  163.3%         max       72.20µs       63.85µs      181.75µs  SQLite (journal, sync=FULL, fullfsync) (index)
   38.9%         max       47.72µs       43.62µs       62.17µs  SQLite (WAL, sync=NORMAL) (index)
   29.9%         max       59.82µs       43.30µs       61.20µs  InlaySQL (B-tree index)
   18.3%         p99       25.07µs       23.65µs       28.23µs  SQLite (journal, sync=FULL, fullfsync) (index)
   15.9%       ops/s       144.17x       138.97x       161.92x  InlaySQL (B-tree index) is faster than InlaySQL (no index: full scan)
   15.2%         max        6.27ms        5.88ms        6.83ms  InlaySQL (no index: full scan)
   12.6%         p99       30.80µs       27.09µs       30.97µs  InlaySQL (B-tree index)
   10.5%         p95       21.14µs       19.26µs       21.47µs  InlaySQL (B-tree index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              228290     4.24µs     4.79µs     7.15µs    59.82µs
InlaySQL (no index: full scan)                          402     2.47ms     2.55ms     2.69ms     6.27ms
SQLite (journal, sync=FULL, fullfsync) (index)       100662     9.37µs    11.34µs    18.05µs    72.20µs
SQLite (WAL, sync=NORMAL) (index)                    237587     3.64µs     5.82µs     6.32µs    47.72µs
InlaySQL (B-tree index) is 567.83x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               52532    18.64µs    21.14µs    30.80µs    31.24µs
InlaySQL (no index: full scan)                          364     2.74ms     2.80ms     2.86ms     3.05ms
SQLite (journal, sync=FULL, fullfsync) (index)        53129    18.32µs    21.11µs    25.07µs    30.93µs
SQLite (WAL, sync=NORMAL) (index)                     76773    12.63µs    15.50µs    17.30µs    27.09µs
InlaySQL (B-tree index) is 144.17x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-10-05T11:18:27Z
commit: 79edf3a
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T111656Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T111727Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T111757Z.txt

metrics: 74; disagreeing by 10% or more across runs: 24

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   60.3%         p99       18.62µs        7.90µs       19.12µs  SQLite (WAL, sync=NORMAL) (index)
   36.1%         p95       10.39µs       10.01µs       13.76µs  InlaySQL
   32.0%         max       32.94µs       24.84µs       35.39µs  SQLite (journal, sync=FULL, fullfsync) (index)
   32.0%        cold       32.94µs       24.84µs       35.39µs  SQLite (journal, sync=FULL, fullfsync) (index)
   31.2%         max       12.93µs       10.90µs       14.93µs  SQLite (WAL, sync=NORMAL) (index)
   31.1%        cold       12.93µs       10.90µs       14.93µs  SQLite (WAL, sync=NORMAL) (index)
   31.1%        cold      125.24µs      121.67µs      160.60µs  InlaySQL
   31.1%         max      125.24µs      121.67µs      160.60µs  InlaySQL
   16.7%         max       64.48µs       56.36µs       67.12µs  InlaySQL
   16.7%        cold       64.48µs       56.35µs       67.11µs  InlaySQL
   15.8%         p50       15.29ms       13.80ms       16.22ms  InlaySQL
   14.1%     joins/s            64            60            69  InlaySQL
   14.0%         p50       14.81ms       13.40ms       15.48ms  InlaySQL
   12.9%         max       34.17µs       30.41µs       34.82µs  SQLite (journal, sync=FULL, fullfsync) (index)
   12.9%        cold       34.17µs       30.41µs       34.81µs  SQLite (journal, sync=FULL, fullfsync) (index)
   12.9%         max       93.52ms       83.81ms       95.87ms  InlaySQL
   12.9%        cold       93.52ms       83.81ms       95.87ms  InlaySQL
   12.8%         p99       80.48ms       77.99ms       88.33ms  SQLite (journal, sync=FULL, fullfsync) (index)
   12.8%         max       29.97ms       28.78ms       32.61ms  SQLite (journal, sync=FULL, fullfsync) (index)
   12.6%         p95       17.40µs       16.47µs       18.66µs  InlaySQL
   12.3%     joins/s            65            61            69  InlaySQL
   11.3%        cold       19.35µs       18.62µs       20.82µs  SQLite (WAL, sync=NORMAL) (index)
   11.3%         max       87.79ms       86.32ms       96.25ms  SQLite (journal, sync=FULL, fullfsync) (index)
   10.5%         p99       24.87µs       22.95µs       25.56µs  SQLite (journal, sync=FULL, fullfsync) (index)

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 64     51.81ms    15.29ms    16.88ms    17.21ms    51.81ms
SQLite (journal, sync=FULL, fullfsync) (index)           36     28.01ms    28.07ms    28.28ms    28.45ms    29.97ms
SQLite (WAL, sync=NORMAL) (index)                        35     28.04ms    28.16ms    28.39ms    29.39ms    30.75ms
InlaySQL is 1.70x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              94053    64.48µs     9.76µs    10.39µs    22.77µs    64.48µs
SQLite (journal, sync=FULL, fullfsync) (index)        95338    32.94µs    10.06µs    10.22µs    21.82µs    32.94µs
SQLite (WAL, sync=NORMAL) (index)                    213811    12.93µs     4.55µs     4.63µs     5.02µs    12.93µs
InlaySQL is 1.00x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 65     93.52ms    14.81ms    16.23ms    16.74ms    93.52ms
SQLite (journal, sync=FULL, fullfsync) (index)           13     77.52ms    77.64ms    78.23ms    80.48ms    87.79ms
SQLite (WAL, sync=NORMAL) (index)                        13     77.18ms    77.52ms    78.29ms    78.75ms    85.57ms
InlaySQL is 4.78x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              64002    125.24µs    13.76µs    17.40µs    28.27µs   125.24µs
SQLite (journal, sync=FULL, fullfsync) (index)        75885    34.17µs    12.80µs    12.96µs    24.87µs    34.17µs
SQLite (WAL, sync=NORMAL) (index)                    133599    19.35µs     7.24µs     7.37µs    18.62µs    19.77µs
InlaySQL is 1.18x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-10-05T11:18:36Z
commit: 79edf3a
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T111827Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T111830Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261005T111833Z.txt

metrics: 252; disagreeing by 10% or more across runs: 39

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  100.0%      lock.)             1             0             1  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   63.8%         max        2.98ms        2.94ms        4.84ms  SQLite (journal, sync=FULL, fullfsync)
   59.7%         max        3.03ms        2.56ms        4.37ms  SQLite (journal, sync=FULL, fullfsync)
   50.1%         max        3.97ms        3.03ms        5.02ms  SQLite (journal, sync=FULL, fullfsync)
   50.0%      lock.)          0.01             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   50.0%      lock.)         0.20%         0.20%         0.30%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   44.2%         max        6.08ms        5.70ms        8.39ms  InlaySQL (parallel WAL regions)
   42.9%      lock.)          0.14          0.13          0.19  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   39.7%         p99        1.99ms        1.23ms        2.02ms  InlaySQL (parallel WAL regions)
   38.2%         p99        1.12ms        0.99ms        1.42ms  SQLite (journal, sync=FULL, fullfsync)
   35.5%         p95        1.21ms        1.19ms        1.62ms  InlaySQL (parallel WAL regions)
   34.2%      lock.)           7.6             5           7.6  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   33.0%         max        9.27ms        8.47ms       11.53ms  InlaySQL (parallel WAL regions)
   32.2%         p95        0.89ms        0.76ms        1.05ms  SQLite (journal, sync=FULL, fullfsync)
   28.7%         p99        1.29ms        1.21ms        1.58ms  SQLite (journal, sync=FULL, fullfsync)
   25.0%      lock.)             0             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   25.0%      lock.)         0.40%         0.40%         0.50%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   23.3%         p99        4.34ms        3.83ms        4.84ms  InlaySQL (parallel WAL regions)
   20.8%      lock.)         5.30%         5.00%         6.10%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   18.5%         p99        1.30ms        1.20ms        1.44ms  SQLite (journal, sync=FULL, fullfsync)
   18.0%      lock.)          0.06          0.06          0.07  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   17.1%         p95        0.97ms        0.84ms        1.01ms  SQLite (journal, sync=FULL, fullfsync)
   15.6%   commits/s          1325          1217          1424  SQLite (journal, sync=FULL, fullfsync)
   14.3%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   14.1%      lock.)          0.09          0.08           0.1  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers,   3715 barriers/s — fsync  0.16 ms, interval  0.27 ms, idle  0.11 ms (39.90% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.15 ms/barrier
  buckets: 1 writers, busy 53.7 ms over 200 commits — gate_wait 0.00%, gate_hold 28.20%, follower_wait 0.00%, gather_spin 0.00%, fsync 60.80%, post 0.20%, pre-gate residual 10.60% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.08 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.03 ms (38.70%), residual  0.05 ms (61.30%), 0 commit-point misses
  barriers: 2 writers, 237 normal flushes over 400 commits ( 0.59 syncs/commit, 1.69 commits/sync)
  barrier cycle: 2 writers, 2276.2 barriers/s — fsync  0.24 ms, interval  0.44 ms, idle  0.21 ms (46.40% of the wall clock has no flush in flight); coordinator gather  0.09 post     0 gap  0.14 ms/barrier
  buckets: 2 writers, busy 203.6 ms over 400 commits — gate_wait 5.30%, gate_hold 17.80%, follower_wait 24.80%, gather_spin 10.70%, fsync 27.20%, post 0.40%, pre-gate residual 13.70% (402 gate waits, 211 racing holds)
  gate hold: 2 writers, 402 holds,  0.09 ms mean —              read  0.01 (960 calls), state     0 (1), wal  0.01 (401, 7.6 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.03 ms (33.00%), residual  0.06 ms (67.00%), 1 commit-point misses
  barriers: 4 writers, 217 normal flushes over 800 commits ( 0.27 syncs/commit,  3.7 commits/sync)
  barrier cycle: 4 writers, 1112.9 barriers/s — fsync  0.36 ms, interval   0.9 ms, idle  0.53 ms (59.60% of the wall clock has no flush in flight); coordinator gather  0.35 post  0.01 gap  0.18 ms/barrier
  buckets: 4 writers, busy 774.1 ms over 800 commits — gate_wait 19.10%, gate_hold 11.70%, follower_wait 40.40%, gather_spin 9.80%, fsync 10.40%, post 0.30%, pre-gate residual 8.00% (803 gate waits, 603 racing holds)
  gate hold: 4 writers, 803 holds,  0.11 ms mean —              read  0.01 (2857 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.04 ms (32.30%), residual  0.08 ms (67.70%), 3 commit-point misses
  barriers: 8 writers, 213 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.54 commits/sync)
  barrier cycle: 8 writers, 631.7 barriers/s — fsync   0.5 ms, interval  1.58 ms, idle  1.08 ms (68.60% of the wall clock has no flush in flight); coordinator gather  0.86 post  0.03 gap  0.23 ms/barrier
  buckets: 8 writers, busy 2680.2 ms over 1600 commits — gate_wait 25.30%, gate_hold 7.10%, follower_wait 50.80%, gather_spin 6.80%, fsync 4.00%, post 0.20%, pre-gate residual 5.70% (1604 gate waits, 1403 racing holds)
  gate hold: 8 writers, 1604 holds,  0.12 ms mean —              read  0.01 (6445 calls), state     0 (8), wal  0.01 (1608, 11.3 KiB),              data  0.01 (1600, 19.6 KiB), of which extend  0.01 (6 extensions);              device  0.04 ms (29.70%), residual  0.08 ms (70.30%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         3715          200       0.00%   230.41µs   395.69µs   482.63µs     1.83ms
InlaySQL (parallel WAL regions)                 2         3854          400       0.00%   475.73µs   730.10µs     1.99ms     3.05ms
InlaySQL (parallel WAL regions)                 4         4103          800       0.00%   888.94µs     1.21ms     4.34ms     6.08ms
InlaySQL (parallel WAL regions)                 8         4745         1600       0.00%     1.48ms     2.97ms     5.94ms     9.27ms
SQLite (journal, sync=FULL, fullfsync)          1         1325          200       0.00%   724.10µs     0.89ms     1.12ms     3.03ms
SQLite (journal, sync=FULL, fullfsync)          2         1300          400       0.00%   720.21µs     0.97ms     1.16ms     3.97ms
SQLite (journal, sync=FULL, fullfsync)          4         1348          800       0.00%   702.54µs   904.66µs     1.30ms     2.98ms
SQLite (journal, sync=FULL, fullfsync)          8         1335         1600       0.00%   712.68µs   884.23µs     1.29ms     4.18ms

InlaySQL at 8 writers does 1.28x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-10-05T10:56:06Z
commit: 79edf3a
dirty:  no
rustc:  rustc 1.99.0 (b940084d7 2026-09-28)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  146.00us  200.00us |   0.988  231.00us  295.00us     3.8s
DuckDB (exhaustive + fts BM25)         1.000    8.40ms   11.48ms |   0.966   20.60ms   23.42ms    37.2s
DuckDB (vss HNSW + fts BM25)           0.991    7.13ms    7.27ms |   0.961   19.34ms   22.11ms    37.1s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.999    2.51ms    3.37ms |   0.418    7.47ms    8.72ms     3.2s
pgvector (HNSW + ts_rank)              0.987  316.00us  678.00us |   0.454   24.68ms   36.60ms     0.9s
pgvector (exhaustive + ts_rank)        0.999  845.00us  888.00us |   0.465   24.54ms   36.55ms     0.3s

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
InlaySQL                                                              2070.7  245.00us  499.00us  994.00us |   1402426.5    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       2187.5  247.00us  480.00us    1.11ms |   1378214.5    1.00us    1.00us    1.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           2667.4  341.00us  455.00us  792.00us |      8747.9  109.00us  148.00us  178.00us
                                                                   commits-per-fsync: 20003/20445 = 0.98
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       4550.0  213.00us  269.00us  337.00us |     18421.9   53.00us   71.00us   81.00us
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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1725.9  444.00us  627.00us    1.19ms       0 |      6157.6   94.00us  112.00us  122.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      2565.9  907.00us    1.61ms    9.93ms       0 |      7439.5  105.00us  186.00us  248.00us
                                                                                      commits-per-fsync: 2007/690 = 2.91
                                                                                      commits-per-fsync (checkpoint-inclusive): 2014/697 = 2.89
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1739.9    2.47ms   22.25ms   42.50ms       0 |      2219.5  182.00us    1.42ms    3.94ms
                                                                                      commits-per-fsync: 2016/258 = 7.81
                                                                                      commits-per-fsync (checkpoint-inclusive): 2023/265 = 7.63
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2399.6  331.00us  440.00us  733.00us       0 |      5389.2  101.00us  124.00us  134.00us
                                                                                      commits-per-fsync: 2003/2044 = 0.98
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      6369.1  376.00us  660.00us  861.00us       0 |      7853.0   81.00us  165.00us  256.00us
                                                                                      commits-per-fsync: 2003/1219 = 1.64
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      3254.4  526.00us    1.45ms    2.80ms       0 |      2044.8   91.00us  250.00us  526.00us
                                                                                      commits-per-fsync: 2003/1053 = 1.90

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

