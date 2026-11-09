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

- generated: 2026-11-09T12:33:05Z
- commit: 5e1eb91
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-11-09T12:06:41Z
commit: 5e1eb91
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1019-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T120356Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T120552Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T120617Z.txt

metrics: 46; disagreeing by 10% or more across runs: 11

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  271.5%         max       53.41µs       41.36µs      186.38µs  SQLite (WAL, sync=NORMAL)
  126.5%         max       17.16ms       14.93ms       36.64ms  InlaySQL
  126.5%         max       17.16ms       14.93ms       36.64ms  InlaySQL
   55.9%         max       57.65µs       28.27µs       60.50µs  InlaySQL
   29.5%         max        3.42ms        3.25ms        4.26ms  SQLite (WAL, sync=NORMAL)
   24.4%         max       63.14µs       51.59µs       66.97µs  SQLite (journal, sync=FULL, fullfsync)
   19.9%         p99       11.14µs       10.68µs       12.90µs  SQLite (journal, sync=FULL, fullfsync)
   17.7%         max       13.98ms       13.17ms       15.64ms  InlaySQL (batched)
   13.7%         p95       11.20µs       11.13µs       12.67µs  SQLite (WAL, sync=NORMAL)
   11.2%         max       18.81ms       17.36ms       19.46ms  SQLite (journal, sync=FULL, fullfsync)
   11.2%         max       18.81ms       17.36ms       19.46ms  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         3129   242.00µs   487.45µs     1.33ms    17.16ms
SQLite (journal, sync=FULL, fullfsync)           1316   693.80µs   948.84µs     1.37ms    18.81ms
SQLite (WAL, sync=NORMAL)                       78482     9.27µs    11.20µs    20.08µs     3.42ms
InlaySQL is 2.38x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             176024     3.04µs     3.61µs     6.69µs    13.98ms
InlaySQL                                         3129   242.00µs   487.45µs     1.33ms    17.16ms
SQLite (journal, sync=FULL, fullfsync)           1316   693.80µs   948.84µs     1.37ms    18.81ms
InlaySQL (batched) is 56.38x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1322525   701.00ns   812.00ns   961.00ns    57.65µs
SQLite (journal, sync=FULL, fullfsync)         122764     7.97µs     8.39µs    11.14µs    63.14µs
SQLite (WAL, sync=NORMAL)                      361940     2.60µs     2.84µs     3.34µs    53.41µs
InlaySQL is 10.68x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-11-09T12:31:13Z
commit: 5e1eb91
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1019-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T120641Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T121450Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T122302Z.txt

metrics: 42; disagreeing by 10% or more across runs: 12

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   81.4%         max       60.47µs       54.99µs      104.20µs  InlaySQL (B-tree index)
   36.1%         max       79.53µs       54.13µs       82.84µs  SQLite (journal, sync=FULL, fullfsync) (index)
   30.7%         max       43.53µs       40.62µs       54.00µs  SQLite (WAL, sync=NORMAL) (index)
   21.6%         max       34.18µs       27.59µs       34.96µs  InlaySQL (B-tree index)
   18.8%         p99       17.37µs       16.34µs       19.60µs  SQLite (WAL, sync=NORMAL) (index)
   17.4%         max       24.54µs       21.56µs       25.83µs  SQLite (WAL, sync=NORMAL) (index)
   14.1%         p99        7.58µs        7.50µs        8.57µs  InlaySQL (B-tree index)
   12.0%       ops/s       149.11x       144.70x       162.62x  InlaySQL (B-tree index) is faster than InlaySQL (no index: full scan)
   10.5%         p99       14.82µs       13.76µs       15.32µs  SQLite (journal, sync=FULL, fullfsync) (index)
   10.5%         p95       20.38µs       19.28µs       21.42µs  InlaySQL (B-tree index)
   10.5%         max        5.54ms        5.32ms        5.90ms  InlaySQL (no index: full scan)
   10.2%         max        3.03ms        2.84ms        3.15ms  InlaySQL (no index: full scan)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              226386     4.28µs     4.85µs     7.58µs    60.47µs
InlaySQL (no index: full scan)                          411     2.42ms     2.48ms     2.54ms     5.54ms
SQLite (journal, sync=FULL, fullfsync) (index)       107696     8.74µs    10.61µs    14.82µs    79.53µs
SQLite (WAL, sync=NORMAL) (index)                    247411     3.50µs     5.59µs     6.07µs    43.53µs
InlaySQL (B-tree index) is 552.12x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               54272    17.93µs    20.38µs    26.37µs    34.18µs
InlaySQL (no index: full scan)                          364     2.74ms     2.80ms     2.84ms     3.03ms
SQLite (journal, sync=FULL, fullfsync) (index)        55761    17.49µs    20.57µs    26.21µs    27.74µs
SQLite (WAL, sync=NORMAL) (index)                     78529    12.40µs    15.16µs    17.37µs    24.54µs
InlaySQL (B-tree index) is 149.11x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-11-09T12:32:44Z
commit: 5e1eb91
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1019-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T123113Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T123143Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T123214Z.txt

metrics: 74; disagreeing by 10% or more across runs: 26

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   46.9%         max       18.98µs       18.25µs       27.16µs  SQLite (WAL, sync=NORMAL) (index)
   40.8%         p95        7.45µs        7.32µs       10.36µs  SQLite (WAL, sync=NORMAL) (index)
   39.1%         p99       26.63µs       22.29µs       32.69µs  SQLite (journal, sync=FULL, fullfsync) (index)
   34.2%         max       30.13ms       27.81ms       38.10ms  SQLite (WAL, sync=NORMAL) (index)
   31.6%         max       26.80µs       22.02µs       30.49µs  SQLite (journal, sync=FULL, fullfsync) (index)
   28.5%        cold       22.02µs       20.53µs       26.80µs  SQLite (journal, sync=FULL, fullfsync) (index)
   22.1%         p99       17.31µs       16.44µs       20.26µs  SQLite (WAL, sync=NORMAL) (index)
   18.7%        cold      144.95µs      144.63µs      171.75µs  InlaySQL
   18.7%         max      144.95µs      144.63µs      171.75µs  InlaySQL
   18.7%         max       51.26µs       49.78µs       59.35µs  InlaySQL
   18.7%        cold       51.26µs       49.78µs       59.35µs  InlaySQL
   18.0%         p95       19.80µs       17.55µs       21.11µs  InlaySQL
   15.7%         max       81.92ms       81.56ms       94.46ms  SQLite (journal, sync=FULL, fullfsync) (index)
   15.6%         p99       27.90ms       27.78ms       32.12ms  SQLite (WAL, sync=NORMAL) (index)
   15.2%         max       33.39µs       33.17µs       38.23µs  SQLite (journal, sync=FULL, fullfsync) (index)
   14.3%        cold       11.53µs       10.82µs       12.46µs  SQLite (WAL, sync=NORMAL) (index)
   14.2%         max       11.53µs       10.82µs       12.46µs  SQLite (WAL, sync=NORMAL) (index)
   13.8%         p99       19.15µs       17.89µs       20.53µs  SQLite (journal, sync=FULL, fullfsync) (index)
   13.1%         p99       16.45ms       14.43ms       16.58ms  InlaySQL
   11.6%         p95       15.41ms       14.61ms       16.39ms  InlaySQL
   11.0%         max       54.01ms       52.16ms       58.10ms  InlaySQL
   11.0%        cold       54.01ms       52.16ms       58.10ms  InlaySQL
   10.7%         p50       13.75ms       13.62ms       15.09ms  InlaySQL
   10.6%        cold       18.98µs       18.25µs       20.26µs  SQLite (WAL, sync=NORMAL) (index)
   10.5%         p95       15.73ms       14.29ms       15.94ms  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 69     54.01ms    13.75ms    15.73ms    16.45ms    54.01ms
SQLite (journal, sync=FULL, fullfsync) (index)           37     27.13ms    27.22ms    27.36ms    27.90ms    28.40ms
SQLite (WAL, sync=NORMAL) (index)                        36    27.40ms    27.45ms    27.67ms    27.90ms    30.13ms
InlaySQL is 1.75x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              96576    51.26µs     9.61µs    11.79µs    20.61µs    51.26µs
SQLite (journal, sync=FULL, fullfsync) (index)       102500    22.02µs     9.46µs     9.61µs    19.15µs    26.80µs
SQLite (WAL, sync=NORMAL) (index)                    228986    11.53µs     4.25µs     4.34µs     4.93µs    11.53µs
InlaySQL is 1.06x slower than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 66     88.41ms    14.27ms    15.41ms    15.86ms    88.41ms
SQLite (journal, sync=FULL, fullfsync) (index)           13     79.19ms    79.52ms    80.20ms    80.86ms    81.92ms
SQLite (WAL, sync=NORMAL) (index)                        12     79.50ms    79.78ms    81.51ms    82.21ms    82.84ms
InlaySQL is 5.14x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              61205   144.95µs    14.16µs    19.80µs    25.75µs   144.95µs
SQLite (journal, sync=FULL, fullfsync) (index)        77261    33.17µs    12.45µs    12.73µs    26.63µs    33.39µs
SQLite (WAL, sync=NORMAL) (index)                    134057    18.98µs     7.18µs     7.45µs    17.31µs    18.98µs
InlaySQL is 1.28x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-11-09T12:32:54Z
commit: 5e1eb91
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1019-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T123244Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T123247Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261109T123251Z.txt

metrics: 252; disagreeing by 10% or more across runs: 55

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  310.8%         max        3.71ms        3.71ms       15.24ms  SQLite (journal, sync=FULL, fullfsync)
  275.0%         max        1.88ms        1.84ms        7.01ms  InlaySQL (parallel WAL regions)
  127.1%         p99        1.18ms        1.05ms        2.55ms  SQLite (journal, sync=FULL, fullfsync)
   84.3%         max       12.79ms        9.17ms       19.95ms  InlaySQL (parallel WAL regions)
   65.2%      lock.)         9.20%         4.70%        10.70%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   56.7%         p95        1.34ms        1.17ms        1.93ms  InlaySQL (parallel WAL regions)
   55.6%         max        6.58ms        5.68ms        9.34ms  InlaySQL (parallel WAL regions)
   53.2%         max       14.13ms        7.56ms       15.08ms  SQLite (journal, sync=FULL, fullfsync)
   52.0%      lock.)             5             5           7.6  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   33.8%         p99        1.48ms        1.20ms        1.70ms  SQLite (journal, sync=FULL, fullfsync)
   33.3%      lock.)         0.30%         0.30%         0.40%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   33.3%      lock.)         0.30%         0.20%         0.30%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   32.9%         p99        1.01ms        0.90ms        1.23ms  InlaySQL (parallel WAL regions)
   28.6%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   27.8%         max       16.30ms       11.98ms       16.51ms  SQLite (journal, sync=FULL, fullfsync)
   27.3%      lock.)          0.17          0.14          0.18  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   25.0%      lock.)             0             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   25.0%      lock.)         0.40%         0.40%         0.50%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   25.0%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   25.0%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   23.3%         p99        4.20ms        3.48ms        4.46ms  InlaySQL (parallel WAL regions)
   21.0%      lock.)        34.80%        32.50%        39.80%  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   20.1%         p95        3.04ms        2.90ms        3.51ms  InlaySQL (parallel WAL regions)
   19.2%      lock.)           0.1          0.09          0.11  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   18.9%         p99        1.27ms        1.27ms        1.51ms  SQLite (journal, sync=FULL, fullfsync)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 3627.5 barriers/s — fsync  0.18 ms, interval  0.28 ms, idle   0.1 ms (34.80% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.15 ms/barrier
  buckets: 1 writers, busy   55 ms over 200 commits — gate_wait 0.00%, gate_hold 26.60%, follower_wait 0.00%, gather_spin 0.00%, fsync 66.00%, post 0.30%, pre-gate residual 9.20% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.07 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.03 ms (38.80%), residual  0.04 ms (61.20%), 0 commit-point misses
  barriers: 2 writers, 220 normal flushes over 400 commits ( 0.55 syncs/commit, 1.82 commits/sync)
  barrier cycle: 2 writers,   2089 barriers/s — fsync  0.26 ms, interval  0.48 ms, idle  0.22 ms (43.90% of the wall clock has no flush in flight); coordinator gather   0.1 post     0 gap  0.17 ms/barrier
  buckets: 2 writers, busy   200 ms over 400 commits — gate_wait 5.40%, gate_hold 16.90%, follower_wait 26.50%, gather_spin 10.40%, fsync 28.50%, post 0.40%, pre-gate residual 12.00% (402 gate waits, 202 racing holds)
  gate hold: 2 writers, 402 holds,  0.08 ms mean —              read  0.01 (958 calls), state     0 (0), wal     0 (400,   5 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.03 ms (34.50%), residual  0.06 ms (65.50%), 1 commit-point misses
  barriers: 4 writers, 219 normal flushes over 800 commits ( 0.27 syncs/commit, 3.67 commits/sync)
  barrier cycle: 4 writers, 1130.6 barriers/s — fsync  0.38 ms, interval  0.88 ms, idle  0.51 ms (56.00% of the wall clock has no flush in flight); coordinator gather  0.33 post  0.01 gap  0.17 ms/barrier
  buckets: 4 writers, busy 743.4 ms over 800 commits — gate_wait 16.80%, gate_hold 11.10%, follower_wait 42.40%, gather_spin 9.30%, fsync 11.30%, post 0.30%, pre-gate residual 8.40% (803 gate waits, 605 racing holds)
  gate hold: 4 writers, 803 holds,  0.11 ms mean —              read  0.01 (2858 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.03 ms (32.30%), residual  0.07 ms (67.70%), 3 commit-point misses
  barriers: 8 writers, 213 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.54 commits/sync)
  barrier cycle: 8 writers, 644.2 barriers/s — fsync  0.48 ms, interval  1.55 ms, idle  1.06 ms (68.90% of the wall clock has no flush in flight); coordinator gather  0.84 post  0.03 gap  0.24 ms/barrier
  buckets: 8 writers, busy 2649.7 ms over 1600 commits — gate_wait 26.10%, gate_hold 7.30%, follower_wait 49.80%, gather_spin 6.80%, fsync 4.00%, post 0.20%, pre-gate residual 6.00% (1603 gate waits, 1394 racing holds)
  gate hold: 8 writers, 1603 holds,  0.12 ms mean —              read  0.01 (6503 calls), state     0 (8), wal  0.01 (1608, 11.4 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.03 ms (27.90%), residual  0.09 ms (72.10%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         3628          200       0.00%   231.48µs   415.42µs     1.01ms     1.88ms
InlaySQL (parallel WAL regions)                 2         3941          400       0.00%   461.40µs   669.27µs     2.14ms     3.40ms
InlaySQL (parallel WAL regions)                 4         4273          800       0.00%   843.72µs     1.34ms     4.20ms     6.58ms
InlaySQL (parallel WAL regions)                 8         4793         1600       0.00%     1.47ms     3.04ms     6.78ms    12.79ms
SQLite (journal, sync=FULL, fullfsync)          1         1270          200       0.00%   731.22µs   961.26µs     1.18ms     3.71ms
SQLite (journal, sync=FULL, fullfsync)          2         1286          400       0.00%   731.69µs   954.87µs     1.27ms     3.77ms
SQLite (journal, sync=FULL, fullfsync)          4         1257          800       0.00%   735.28µs   971.50µs     1.48ms    16.30ms
SQLite (journal, sync=FULL, fullfsync)          8         1256         1600       0.00%   749.74µs     1.01ms     1.35ms    14.13ms

InlaySQL at 8 writers does 1.36x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-11-09T12:10:24Z
commit: 5e1eb91
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1019-azure x86_64
docker: 29.4.2
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000   95.00us  243.00us |   0.988  165.00us  207.00us     3.0s
DuckDB (exhaustive + fts BM25)         1.000    8.97ms    9.40ms |   0.966   19.04ms   25.36ms    40.4s
DuckDB (vss HNSW + fts BM25)           0.993    7.84ms    8.19ms |   0.956   17.16ms   19.85ms    40.8s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.996    1.89ms    2.05ms |   0.418    6.21ms    8.10ms     2.4s
pgvector (HNSW + ts_rank)              0.988  217.00us  304.00us |   0.456   18.84ms   29.14ms     0.8s
pgvector (exhaustive + ts_rank)        0.999  680.00us  841.00us |   0.465   19.20ms   28.31ms     0.2s

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
InlaySQL                                                              2619.9  158.00us  380.00us    2.78ms |   1695772.1    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       2603.2  164.00us  324.00us    2.57ms |   1460604.9    1.00us    1.00us    1.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           4870.1  192.00us  247.00us  452.00us |     12917.5   72.00us   93.00us  124.00us
                                                                   commits-per-fsync: 20003/22129 = 0.90
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       7626.4  125.00us  157.00us  230.00us |     28159.8   35.00us   42.00us   55.00us
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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      2599.7  222.00us  369.00us  679.00us       0 |      7535.5   67.00us   80.00us   95.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      3036.1  627.00us    1.84ms   19.05ms       0 |      8260.3   83.00us  199.00us  273.00us
                                                                                      commits-per-fsync: 2012/575 = 3.50
                                                                                      commits-per-fsync (checkpoint-inclusive): 2014/577 = 3.49
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1871.6    2.27ms   16.67ms   38.12ms       0 |      2210.5  316.00us    1.22ms    2.84ms
                                                                                      commits-per-fsync: 2015/202 = 9.98
                                                                                      commits-per-fsync (checkpoint-inclusive): 2022/209 = 9.67
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      3883.7  197.00us  271.00us  506.00us       0 |      7214.5   70.00us   84.00us   97.00us
                                                                                      commits-per-fsync: 2003/2242 = 0.89
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      7155.6  305.00us  596.00us  949.00us       0 |      7112.3  129.00us  188.00us  242.00us
                                                                                      commits-per-fsync: 2003/1259 = 1.59
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      3076.2  554.00us    1.54ms    3.01ms       0 |      1839.6  122.00us  902.00us    3.25ms
                                                                                      commits-per-fsync: 2003/1112 = 1.80

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

