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

- generated: 2026-09-14T09:41:13Z
- commit: c576ac6
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-09-14T09:17:48Z
commit: c576ac6
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T091433Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T091637Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T091713Z.txt

metrics: 46; disagreeing by 10% or more across runs: 24

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  568.1%         max       22.98ms       13.33ms      143.89ms  SQLite (journal, sync=FULL, fullfsync)
  568.1%         max       22.98ms       13.33ms      143.89ms  SQLite (journal, sync=FULL, fullfsync)
  159.4%         max       48.13ms       32.79ms      109.51ms  InlaySQL
  159.4%         max       48.13ms       32.79ms      109.51ms  InlaySQL
   96.9%         p99        2.62ms        2.54ms        5.08ms  SQLite (journal, sync=FULL, fullfsync)
   96.9%         p99        2.62ms        2.54ms        5.08ms  SQLite (journal, sync=FULL, fullfsync)
   75.8%         max       20.66µs       17.42µs       33.08µs  SQLite (WAL, sync=NORMAL)
   59.3%         max       53.76µs       38.56µs       70.42µs  SQLite (journal, sync=FULL, fullfsync)
   50.4%         p50      359.14µs      316.05µs      496.96µs  InlaySQL
   50.4%         p50      359.14µs      316.05µs      496.96µs  InlaySQL
   50.2%         max       25.93µs       16.38µs       29.39µs  InlaySQL
   45.3%         p99        5.81µs        3.39µs        6.02µs  InlaySQL (batched)
   38.4%         p99        9.21µs        9.14µs       12.68µs  SQLite (WAL, sync=NORMAL)
   23.9%      engine       109.17x       103.23x       129.30x  InlaySQL (batched) is faster than InlaySQL
   21.8%       ops/s          1797          1520          1911  InlaySQL
   21.8%       ops/s          1797          1520          1911  InlaySQL
   17.4%         p95        1.32ms        1.26ms        1.49ms  SQLite (journal, sync=FULL, fullfsync)
   17.4%         p95        1.32ms        1.26ms        1.49ms  SQLite (journal, sync=FULL, fullfsync)
   12.8%         p95      630.39µs      597.13µs      677.89µs  InlaySQL
   12.8%         p95      630.39µs      597.13µs      677.89µs  InlaySQL
   12.4%       ops/s           858           758           864  SQLite (journal, sync=FULL, fullfsync)
   12.4%       ops/s           858           758           864  SQLite (journal, sync=FULL, fullfsync)
   12.4%         p99        1.70ms        1.68ms        1.89ms  InlaySQL
   12.4%         p99        1.70ms        1.68ms        1.89ms  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         1797   359.14µs   630.39µs     1.70ms    48.13ms
SQLite (journal, sync=FULL, fullfsync)            858     1.12ms     1.32ms     2.62ms    22.98ms
SQLite (WAL, sync=NORMAL)                       66005     4.39µs     5.24µs     9.21µs    15.83ms
InlaySQL is 2.01x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             196488     2.40µs     2.95µs     5.81µs    15.13ms
InlaySQL                                         1797   359.14µs   630.39µs     1.70ms    48.13ms
SQLite (journal, sync=FULL, fullfsync)            858     1.12ms     1.32ms     2.62ms    22.98ms
InlaySQL (batched) is 109.17x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1537426   606.00ns   753.00ns   842.00ns    25.93µs
SQLite (journal, sync=FULL, fullfsync)         304321     3.22µs     3.52µs     3.68µs    53.76µs
SQLite (WAL, sync=NORMAL)                      789979     1.21µs     1.35µs     1.42µs    20.66µs
InlaySQL is 4.93x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-09-14T09:39:30Z
commit: c576ac6
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T091748Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T092459Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T093217Z.txt

metrics: 42; disagreeing by 10% or more across runs: 9

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   69.7%         max       63.09µs       33.28µs       77.27µs  SQLite (journal, sync=FULL, fullfsync) (index)
   52.5%         max       28.47µs       26.40µs       41.36µs  SQLite (WAL, sync=NORMAL) (index)
   41.2%         p99       20.84µs       20.28µs       28.86µs  InlaySQL (B-tree index)
   31.7%         max       44.15µs       42.86µs       56.86µs  InlaySQL (B-tree index)
   30.4%         max       23.51µs       22.46µs       29.60µs  InlaySQL (B-tree index)
   17.9%         max       17.08µs       15.53µs       18.59µs  SQLite (WAL, sync=NORMAL) (index)
   17.1%         max        4.22ms        4.12ms        4.84ms  InlaySQL (no index: full scan)
   15.4%         p99       13.94µs       12.68µs       14.82µs  SQLite (WAL, sync=NORMAL) (index)
   13.9%         p95       11.38µs       10.76µs       12.34µs  SQLite (WAL, sync=NORMAL) (index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              287414     3.39µs     3.86µs     5.14µs    44.15µs
InlaySQL (no index: full scan)                          465     2.12ms     2.32ms     2.35ms     4.22ms
SQLite (journal, sync=FULL, fullfsync) (index)       236728     4.05µs     5.01µs     5.51µs    63.09µs
SQLite (WAL, sync=NORMAL) (index)                    452104     1.93µs     3.20µs     3.59µs    28.47µs
InlaySQL (B-tree index) is 619.34x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               68596    14.22µs    15.37µs    20.84µs    23.51µs
InlaySQL (no index: full scan)                          423     2.33ms     2.53ms     2.57ms     2.65ms
SQLite (journal, sync=FULL, fullfsync) (index)        88519    10.93µs    12.50µs    16.46µs    16.83µs
SQLite (WAL, sync=NORMAL) (index)                    106225     8.92µs    11.38µs    13.94µs    17.08µs
InlaySQL (B-tree index) is 161.31x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-09-14T09:40:50Z
commit: c576ac6
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T093930Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T093957Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T094024Z.txt

metrics: 74; disagreeing by 10% or more across runs: 20

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   91.5%         p99        5.32µs        5.13µs       10.00µs  SQLite (journal, sync=FULL, fullfsync) (index)
   80.9%         p99       15.38µs       12.08µs       24.52µs  InlaySQL
   51.4%         p50        2.76µs        2.66µs        4.08µs  SQLite (WAL, sync=NORMAL) (index)
   45.5%         p99       23.23µs       22.34µs       32.92µs  InlaySQL
   38.5%         p99        3.95µs        3.11µs        4.63µs  SQLite (WAL, sync=NORMAL) (index)
   38.4%         p99       12.25µs        8.27µs       12.98µs  SQLite (journal, sync=FULL, fullfsync) (index)
   36.9%         p95        3.77µs        2.81µs        4.20µs  SQLite (WAL, sync=NORMAL) (index)
   36.4%     joins/s        325446        239750        358279  SQLite (WAL, sync=NORMAL) (index)
   21.3%         p95       16.70µs       14.41µs       17.96µs  InlaySQL
   21.2%         max       13.74µs       12.27µs       15.18µs  SQLite (WAL, sync=NORMAL) (index)
   21.2%        cold       13.74µs       12.27µs       15.18µs  SQLite (WAL, sync=NORMAL) (index)
   20.8%         p95        5.14µs        4.85µs        5.92µs  SQLite (WAL, sync=NORMAL) (index)
   18.6%         p99       15.30ms       13.54ms       16.38ms  InlaySQL
   17.7%         p99        6.32µs        5.92µs        7.04µs  SQLite (WAL, sync=NORMAL) (index)
   15.0%        cold       48.88µs       44.95µs       52.30µs  InlaySQL
   15.0%         max       48.88µs       44.96µs       52.30µs  InlaySQL
   12.4%         p50       11.48µs       11.36µs       12.78µs  InlaySQL
   10.8%     joins/s         75190         68999         77134  InlaySQL
   10.8%         p95        8.71µs        7.98µs        8.92µs  InlaySQL
   10.8%     joins/s        112086        109680        121771  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 92    46.32ms    10.53ms    11.00ms    11.65ms    46.32ms
SQLite (journal, sync=FULL, fullfsync) (index)           44    23.07ms    22.64ms    23.27ms    23.41ms    23.54ms
SQLite (WAL, sync=NORMAL) (index)                        44     22.80ms    22.75ms    23.39ms    23.73ms    23.76ms
InlaySQL is 2.05x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                             112086    48.88µs     8.39µs     8.71µs    15.38µs    48.88µs
SQLite (journal, sync=FULL, fullfsync) (index)       210856    10.29µs     4.60µs     4.71µs     5.32µs    10.29µs
SQLite (WAL, sync=NORMAL) (index)                    325446     6.83µs     2.76µs     3.77µs     3.95µs     6.83µs
InlaySQL is 1.92x slower than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 75     69.83ms    12.73ms    13.25ms    15.30ms    69.83ms
SQLite (journal, sync=FULL, fullfsync) (index)           17     58.67ms    58.16ms    59.26ms    62.14ms    63.08ms
SQLite (WAL, sync=NORMAL) (index)                        17     60.04ms    59.34ms    60.50ms    60.91ms    61.63ms
InlaySQL is 4.31x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              75190   136.46µs    11.48µs    16.70µs    23.23µs   136.46µs
SQLite (journal, sync=FULL, fullfsync) (index)       146579    15.55µs     6.62µs     6.88µs    12.25µs    15.55µs
SQLite (WAL, sync=NORMAL) (index)                    195495    13.74µs     4.70µs     5.14µs     6.32µs    13.74µs
InlaySQL is 1.93x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-09-14T09:41:04Z
commit: c576ac6
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T094050Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T094055Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20260914T094100Z.txt

metrics: 252; disagreeing by 10% or more across runs: 57

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  920.0%      lock.)         -0.01         -0.01          0.03  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
  713.3%      lock.)        -1.50%        -4.20%         6.50%  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
  560.0%      lock.)          0.01             0          0.03  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
  325.0%      lock.)         0.80%         0.60%         3.20%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
  264.9%      lock.)           148           123           515  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
  237.5%      lock.)         0.80%         0.60%         2.50%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
  227.8%      lock.)         1.80%         0.80%         4.90%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
  200.0%      lock.)             0             0             0  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
  115.9%      lock.)        -6.30%        -7.10%         0.20%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
  100.0%      lock.)         7.80%         7.30%        15.10%  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
  100.0%      lock.)         0.10%         0.10%         0.20%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   98.0%         p99        2.49ms        2.04ms        4.48ms  InlaySQL (parallel WAL regions)
   88.5%      lock.)          0.05          0.05           0.1  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   84.8%      lock.)          0.03          0.03          0.06  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   82.0%         max       10.71ms        6.02ms       14.80ms  InlaySQL (parallel WAL regions)
   78.9%         max        5.11ms        5.07ms        9.10ms  SQLite (journal, sync=FULL, fullfsync)
   76.8%         max        5.95ms        5.61ms       10.18ms  SQLite (journal, sync=FULL, fullfsync)
   75.0%      lock.)         0.40%         0.30%         0.60%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   73.8%         max        7.45ms        2.05ms        7.55ms  InlaySQL (parallel WAL regions)
   57.6%         max        5.54ms        5.44ms        8.63ms  SQLite (journal, sync=FULL, fullfsync)
   56.0%         p99        2.68ms        2.54ms        4.04ms  SQLite (journal, sync=FULL, fullfsync)
   53.6%         p95        5.04ms        3.78ms        6.48ms  InlaySQL (parallel WAL regions)
   50.0%      lock.)             0             0             0  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   50.0%      lock.)             0             0             0  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   48.5%         p99        2.02ms        1.37ms        2.35ms  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 2447.5 barriers/s — fsync  0.38 ms, interval  0.41 ms, idle  0.03 ms (7.80% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.11 ms/barrier
  buckets: 1 writers, busy 81.6 ms over 200 commits — gate_wait 0.00%, gate_hold 13.30%, follower_wait 0.00%, gather_spin 0.00%, fsync 93.20%, post 0.10%, pre-gate residual -6.30% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.05 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.02 ms (32.40%), residual  0.04 ms (67.60%), 0 commit-point misses
  barriers: 2 writers, 378 normal flushes over 400 commits ( 0.94 syncs/commit, 1.06 commits/sync)
  barrier cycle: 2 writers, 2808.6 barriers/s — fsync  0.36 ms, interval  0.36 ms, idle -0.01 ms (-1.50% of the wall clock has no flush in flight); coordinator gather  0.01 post     0 gap  0.05 ms/barrier
  buckets: 2 writers, busy 268.2 ms over 400 commits — gate_wait 0.80%, gate_hold 8.00%, follower_wait 35.70%, gather_spin 0.80%, fsync 51.20%, post 0.40%, pre-gate residual 1.80% (402 gate waits, 373 racing holds)
  gate hold: 2 writers, 402 holds,  0.06 ms mean —              read     0 (148 calls), state     0 (0), wal     0 (400,   5 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.01 ms (24.40%), residual  0.04 ms (75.60%), 1 commit-point misses
  barriers: 4 writers, 216 normal flushes over 800 commits ( 0.27 syncs/commit, 3.72 commits/sync)
  barrier cycle: 4 writers, 914.2 barriers/s — fsync  0.71 ms, interval  1.09 ms, idle  0.38 ms (33.50% of the wall clock has no flush in flight); coordinator gather  0.25 post  0.01 gap  0.13 ms/barrier
  buckets: 4 writers, busy   936 ms over 800 commits — gate_wait 10.80%, gate_hold 7.50%, follower_wait 54.60%, gather_spin 5.40%, fsync 17.00%, post 0.20%, pre-gate residual 4.30% (803 gate waits, 611 racing holds)
  gate hold: 4 writers, 803 holds,  0.09 ms mean —              read  0.01 (2851 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.03 ms (29.00%), residual  0.06 ms (71.00%), 3 commit-point misses
  barriers: 8 writers, 229 normal flushes over 1600 commits ( 0.14 syncs/commit, 7.02 commits/sync)
  barrier cycle: 8 writers,   551 barriers/s — fsync  1.08 ms, interval  1.81 ms, idle  0.72 ms (40.00% of the wall clock has no flush in flight); coordinator gather  0.61 post  0.02 gap  0.19 ms/barrier
  buckets: 8 writers, busy 3303.8 ms over 1600 commits — gate_wait 17.10%, gate_hold 5.10%, follower_wait 62.60%, gather_spin 4.20%, fsync 7.70%, post 0.10%, pre-gate residual 3.50% (1604 gate waits, 1422 racing holds)
  gate hold: 8 writers, 1604 holds,   0.1 ms mean —              read  0.01 (6456 calls), state     0 (8), wal  0.01 (1608, 11.4 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.03 ms (28.00%), residual  0.07 ms (72.00%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         2447          200       0.00%   284.44µs   570.98µs   1160.00µs     7.45ms
InlaySQL (parallel WAL regions)                 2         2972          400       0.00%   631.89µs   953.22µs     2.49ms    10.71ms
InlaySQL (parallel WAL regions)                 4         3402          800       0.00%   880.42µs     1.70ms     7.72ms    31.78ms
InlaySQL (parallel WAL regions)                 8         3850         1600       0.00%     1.42ms     5.04ms    26.19ms    28.18ms
SQLite (journal, sync=FULL, fullfsync)          1          869          200       0.00%     1.12ms     1.26ms     2.02ms     5.08ms
SQLite (journal, sync=FULL, fullfsync)          2          865          400       0.00%     1.11ms     1.24ms     2.68ms     5.11ms
SQLite (journal, sync=FULL, fullfsync)          4          857          800       0.00%     1.11ms     1.23ms     2.56ms     5.54ms
SQLite (journal, sync=FULL, fullfsync)          8          854         1600       0.00%     1.13ms     1.27ms     2.57ms     5.95ms

InlaySQL at 8 writers does 1.60x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-09-14T09:23:44Z
commit: c576ac6
dirty:  no
rustc:  rustc 1.98.1 (48a229cea 2026-09-01)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  181.00us  265.00us |   0.988  299.00us  375.00us     3.4s
DuckDB (exhaustive + fts BM25)         1.000   14.67ms   15.25ms |   0.966   31.59ms   38.88ms    65.7s
DuckDB (vss HNSW + fts BM25)           0.992   13.22ms   14.76ms |   0.962   30.12ms   35.43ms    67.0s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.997    3.17ms    3.51ms |   0.418   10.47ms   12.45ms     4.1s
pgvector (HNSW + ts_rank)              0.989  387.00us  517.00us |   0.458   34.42ms   52.75ms     1.4s
pgvector (exhaustive + ts_rank)        0.999    1.25ms    1.30ms |   0.465   35.84ms   54.58ms     0.5s

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
InlaySQL                                                              3856.8  200.00us  393.00us  688.00us |    926738.2    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       3769.3  203.00us  391.00us    1.07ms |    915664.0    1.00us    1.00us    1.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           3132.7  298.00us  417.00us  791.00us |      5440.6  160.00us  243.00us  286.00us
                                                                   commits-per-fsync: 20003/21398 = 0.93
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       5356.8  194.00us  228.00us  266.00us |     11824.3   69.00us  119.00us  140.00us
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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1992.5  426.00us  564.00us    1.26ms       0 |      3799.2  157.00us  222.00us  305.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2013/2013 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      2692.4  958.00us    1.34ms    5.21ms       0 |      5077.1  200.00us  226.00us  306.00us
                                                                                      commits-per-fsync: 2007/618 = 3.25
                                                                                      commits-per-fsync (checkpoint-inclusive): 2015/626 = 3.22
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1654.2    1.80ms    5.51ms   18.61ms       0 |      1514.2  391.00us    2.73ms    4.44ms
                                                                                      commits-per-fsync: 2009/766 = 2.62
                                                                                      commits-per-fsync (checkpoint-inclusive): 2025/781 = 2.59
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2667.4  291.00us  359.00us  624.00us       0 |      3207.5  218.00us  243.00us  254.00us
                                                                                      commits-per-fsync: 2003/2131 = 0.94
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      4836.7  460.00us  748.00us    1.09ms       0 |      4111.6  292.00us  466.00us  554.00us
                                                                                      commits-per-fsync: 2003/1326 = 1.51
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      2118.3  714.00us    2.06ms    3.66ms       0 |      1223.4  290.00us    2.55ms    5.40ms
                                                                                      commits-per-fsync: 2003/1476 = 1.36

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

