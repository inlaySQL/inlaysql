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

- generated: 2026-10-19T09:34:31Z
- commit: 10bc3fc
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-10-19T09:18:41Z
commit: 10bc3fc
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T091629Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T091752Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T091816Z.txt

metrics: 46; disagreeing by 10% or more across runs: 18

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
 1651.0%         max       44.60ms       43.84ms      780.19ms  InlaySQL
 1651.0%         max       44.60ms       43.84ms      780.19ms  InlaySQL
  470.3%         max       82.76µs       62.36µs      451.57µs  SQLite (journal, sync=FULL, fullfsync)
  193.9%         max       38.97µs       34.27µs      109.83µs  SQLite (WAL, sync=NORMAL)
  109.6%         p99        1.77ms        1.44ms        3.38ms  SQLite (journal, sync=FULL, fullfsync)
  109.6%         p99        1.77ms        1.44ms        3.38ms  SQLite (journal, sync=FULL, fullfsync)
   93.2%         max       18.59ms       11.81ms       29.13ms  InlaySQL (batched)
   86.7%         max      128.01ms       31.39ms      142.37ms  SQLite (journal, sync=FULL, fullfsync)
   86.7%         max      128.01ms       31.39ms      142.37ms  SQLite (journal, sync=FULL, fullfsync)
   80.5%         p99        2.51µs        2.47µs        4.49µs  InlaySQL (batched)
   29.8%         p99        2.62ms        2.29ms        3.07ms  InlaySQL
   29.8%         p99        2.62ms        2.29ms        3.07ms  InlaySQL
   26.5%       ops/s          2804          2082          2824  InlaySQL
   26.5%       ops/s          2804          2082          2824  InlaySQL
   25.6%         max       16.52µs       14.53µs       18.76µs  InlaySQL
   23.8%      engine        94.64x        85.59x       108.07x  InlaySQL (batched) is faster than InlaySQL
   17.6%       ops/s        239991        224962        267286  InlaySQL (batched)
   11.0%         p95        2.18µs        2.14µs        2.38µs  InlaySQL (batched)

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         2804   164.32µs   335.75µs     2.62ms    44.60ms
SQLite (journal, sync=FULL, fullfsync)           1320   693.30µs   801.88µs     1.77ms   128.01ms
SQLite (WAL, sync=NORMAL)                       55750     7.29µs     8.62µs    13.43µs    12.34ms
InlaySQL is 1.68x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             239991     1.90µs     2.18µs     2.51µs    18.59ms
InlaySQL                                         2804   164.32µs   335.75µs     2.62ms    44.60ms
SQLite (journal, sync=FULL, fullfsync)           1320   693.30µs   801.88µs     1.77ms   128.01ms
InlaySQL (batched) is  94.64x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      2273240   401.00ns   480.00ns   551.00ns    16.52µs
SQLite (journal, sync=FULL, fullfsync)         155867     6.34µs     6.71µs     7.71µs    82.76µs
SQLite (WAL, sync=NORMAL)                      518676     1.87µs     1.98µs     2.13µs    38.97µs
InlaySQL is 15.57x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-10-19T09:33:08Z
commit: 10bc3fc
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T091841Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T092330Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T092821Z.txt

metrics: 42; disagreeing by 10% or more across runs: 16

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  316.6%         max       36.49µs       25.75µs      141.29µs  SQLite (WAL, sync=NORMAL) (index)
  169.7%         max       67.14µs       53.66µs      167.60µs  SQLite (journal, sync=FULL, fullfsync) (index)
   55.1%         max       50.62µs       35.96µs       63.86µs  InlaySQL (B-tree index)
   24.8%         p95       10.46µs       10.36µs       12.95µs  InlaySQL (B-tree index)
   24.4%         p99       18.64µs       15.25µs       19.79µs  SQLite (journal, sync=FULL, fullfsync) (index)
   22.1%         max       14.55µs       13.25µs       16.46µs  SQLite (WAL, sync=NORMAL) (index)
   20.2%         max        5.09ms        4.50ms        5.53ms  InlaySQL (no index: full scan)
   19.9%       ops/s       147.21x       126.03x       155.30x  InlaySQL (B-tree index) is faster than InlaySQL (no index: full scan)
   18.7%         p99       14.95µs       14.35µs       17.14µs  InlaySQL (B-tree index)
   18.2%         max        1.54ms        1.48ms        1.76ms  InlaySQL (no index: full scan)
   16.8%         p50        9.62µs        9.58µs       11.20µs  InlaySQL (B-tree index)
   14.3%         p99       10.72µs       10.54µs       12.07µs  SQLite (WAL, sync=NORMAL) (index)
   14.1%       ops/s        100510         87156        101347  InlaySQL (B-tree index)
   14.0%         max       19.99µs       17.25µs       20.05µs  SQLite (journal, sync=FULL, fullfsync) (index)
   10.5%         p95       13.71µs       12.86µs       14.30µs  SQLite (journal, sync=FULL, fullfsync) (index)
   10.1%       ops/s         82648         79462         87829  SQLite (journal, sync=FULL, fullfsync) (index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              408993     2.35µs     2.70µs     3.78µs    50.62µs
InlaySQL (no index: full scan)                          699     1.44ms     1.47ms     1.52ms     5.09ms
SQLite (journal, sync=FULL, fullfsync) (index)       137165     6.98µs     8.19µs     8.73µs    67.14µs
SQLite (WAL, sync=NORMAL) (index)                    345153     2.56µs     3.87µs     4.09µs    36.49µs
InlaySQL (B-tree index) is 580.26x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               100510     9.62µs    10.46µs    14.95µs    17.26µs
InlaySQL (no index: full scan)                          688     1.45ms     1.50ms     1.52ms     1.54ms
SQLite (journal, sync=FULL, fullfsync) (index)        82648    11.81µs    13.71µs    18.64µs    19.99µs
SQLite (WAL, sync=NORMAL) (index)                    129536     7.55µs     9.35µs    10.72µs    14.55µs
InlaySQL (B-tree index) is 147.21x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-10-19T09:34:12Z
commit: 10bc3fc
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T093308Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T093329Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T093350Z.txt

metrics: 74; disagreeing by 10% or more across runs: 30

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  123.0%         p99        4.82µs        4.60µs       10.53µs  SQLite (WAL, sync=NORMAL) (index)
   84.4%        cold       11.48µs       11.40µs       21.08µs  SQLite (journal, sync=FULL, fullfsync) (index)
   75.4%         p99        3.34µs        3.24µs        5.76µs  SQLite (WAL, sync=NORMAL) (index)
   62.3%         max       13.09µs       12.93µs       21.08µs  SQLite (journal, sync=FULL, fullfsync) (index)
   59.7%         p99       13.08ms        8.86ms       16.67ms  InlaySQL
   54.3%         p99       17.21µs       13.66µs       23.01µs  InlaySQL
   51.9%         p99       12.69µs        7.28µs       13.87µs  InlaySQL
   42.4%         max       48.64ms       43.09ms       63.71ms  SQLite (journal, sync=FULL, fullfsync) (index)
   39.5%         max       15.78ms       15.74ms       21.98ms  SQLite (WAL, sync=NORMAL) (index)
   33.9%         max       26.44ms       25.15ms       34.12ms  SQLite (journal, sync=FULL, fullfsync) (index)
   30.1%         p99       11.48µs       11.40µs       14.85µs  SQLite (journal, sync=FULL, fullfsync) (index)
   28.6%        cold       23.93µs       22.81µs       29.66µs  SQLite (journal, sync=FULL, fullfsync) (index)
   28.6%         max       23.94µs       22.81µs       29.66µs  SQLite (journal, sync=FULL, fullfsync) (index)
   27.0%         p95        9.09µs        8.51µs       10.96µs  SQLite (journal, sync=FULL, fullfsync) (index)
   26.3%         p99       17.27ms       16.74ms       21.29ms  SQLite (journal, sync=FULL, fullfsync) (index)
   24.5%         p95       10.49ms        8.53ms       11.10ms  InlaySQL
   23.0%        cold        7.06µs        5.76µs        7.38µs  SQLite (WAL, sync=NORMAL) (index)
   21.7%        cold      118.31µs      111.45µs      137.13µs  InlaySQL
   21.7%         max      118.30µs      111.45µs      137.13µs  InlaySQL
   18.7%        cold       46.72µs       39.31µs       48.06µs  InlaySQL
   18.7%         max       46.72µs       39.31µs       48.06µs  InlaySQL
   18.6%         p95        9.96µs        9.64µs       11.49µs  InlaySQL
   17.2%         p99       14.09µs       12.82µs       15.24µs  SQLite (journal, sync=FULL, fullfsync) (index)
   15.3%         p95       15.90ms       14.91ms       17.35ms  SQLite (journal, sync=FULL, fullfsync) (index)
   13.2%         p99       43.64ms       42.81ms       48.57ms  SQLite (journal, sync=FULL, fullfsync) (index)

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                119    32.48ms     8.12ms    10.49ms    13.08ms    32.48ms
SQLite (journal, sync=FULL, fullfsync) (index)           64     15.63ms    15.51ms    15.90ms    17.27ms    26.44ms
SQLite (WAL, sync=NORMAL) (index)                        65     15.04ms    15.39ms    15.56ms    15.78ms    15.78ms
InlaySQL is 1.65x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                             168363    46.72µs     5.47µs     5.63µs     12.69µs    46.72µs
SQLite (journal, sync=FULL, fullfsync) (index)       131266    11.48µs     7.49µs     7.55µs    11.48µs    13.09µs
SQLite (WAL, sync=NORMAL) (index)                    321924     7.06µs     3.02µs     3.08µs     3.34µs     7.38µs
InlaySQL is 1.27x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 97     60.37ms     9.76ms    11.40ms    13.10ms    60.37ms
SQLite (journal, sync=FULL, fullfsync) (index)           24     42.64ms    42.00ms    43.00ms    43.64ms    48.64ms
SQLite (WAL, sync=NORMAL) (index)                        24     42.41ms    42.00ms    43.63ms    43.79ms    43.94ms
InlaySQL is 3.86x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                             104213   118.31µs     8.34µs     9.96µs    17.21µs   118.30µs
SQLite (journal, sync=FULL, fullfsync) (index)       108180    23.93µs     8.89µs     9.09µs    14.09µs    23.94µs
SQLite (WAL, sync=NORMAL) (index)                    217792    12.99µs     4.41µs     4.50µs     4.82µs    12.99µs
InlaySQL is 1.11x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-10-19T09:34:21Z
commit: 10bc3fc
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T093412Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T093415Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261019T093418Z.txt

metrics: 252; disagreeing by 10% or more across runs: 62

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
 3776.3%         max       10.93ms        3.81ms      416.56ms  SQLite (journal, sync=FULL, fullfsync)
  441.8%         max        3.88ms        3.65ms       20.79ms  SQLite (journal, sync=FULL, fullfsync)
  342.9%      lock.)          0.02         -0.02          0.05  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
  314.9%         p95      789.93µs      762.28µs     3250.00µs  SQLite (journal, sync=FULL, fullfsync)
  275.7%      lock.)         7.00%        -7.50%        11.80%  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
  224.4%         p99        2.83ms        1.16ms        7.51ms  SQLite (journal, sync=FULL, fullfsync)
  143.0%         max        9.01ms        7.15ms       20.03ms  InlaySQL (parallel WAL regions)
  119.0%      lock.)          0.04          0.02          0.07  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
  109.8%         max        3.99ms        3.42ms        7.80ms  SQLite (journal, sync=FULL, fullfsync)
   83.3%      lock.)          0.01             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   82.2%         p99        0.99ms        0.91ms        1.73ms  SQLite (journal, sync=FULL, fullfsync)
   81.5%      lock.)        -9.20%       -14.30%        -6.80%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   80.0%      lock.)        10.00%         4.50%        12.50%  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   76.0%      lock.)          0.03          0.01          0.03  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   75.0%      lock.)         0.40%         0.20%         0.50%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   70.6%      lock.)           0.3          0.24          0.45  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   69.1%         p95        2.07ms        1.46ms        2.89ms  InlaySQL (parallel WAL regions)
   68.3%         p99      961.75µs      943.12µs     1600.00µs  InlaySQL (parallel WAL regions)
   64.8%      lock.)          0.09          0.06          0.12  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   64.8%      lock.)           767           487           984  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   64.1%         max       10.12ms        7.76ms       14.25ms  SQLite (journal, sync=FULL, fullfsync)
   62.5%      lock.)         1.60%         1.20%         2.20%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   59.4%      lock.)        3302.1        2202.1        4163.5  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   52.8%      lock.)         3.60%         2.20%         4.10%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   51.4%      lock.)         7.00%         4.10%         7.70%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 4070.6 barriers/s — fsync  0.22 ms, interval  0.25 ms, idle  0.03 ms (10.00% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.09 ms/barrier
  buckets: 1 writers, busy   49 ms over 200 commits — gate_wait 0.00%, gate_hold 17.90%, follower_wait 0.00%, gather_spin 0.00%, fsync 91.20%, post 0.10%, pre-gate residual -9.20% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.04 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.02 ms (39.10%), residual  0.03 ms (60.90%), 0 commit-point misses
  barriers: 2 writers, 255 normal flushes over 400 commits ( 0.64 syncs/commit, 1.57 commits/sync)
  barrier cycle: 2 writers, 3302.1 barriers/s — fsync  0.28 ms, interval   0.3 ms, idle   0.02 ms (7.00% of the wall clock has no flush in flight); coordinator gather  0.04 post     0 gap  0.09 ms/barrier
  buckets: 2 writers, busy 153.6 ms over 400 commits — gate_wait 3.60%, gate_hold 11.90%, follower_wait 31.70%, gather_spin 7.00%, fsync 47.10%, post 0.40%, pre-gate residual -3.50% (402 gate waits, 234 racing holds)
  gate hold: 2 writers, 402 holds,  0.05 ms mean —              read  0.01 (767 calls), state     0 (0), wal     0 (400,   5 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.02 ms (30.30%), residual  0.03 ms (69.70%), 1 commit-point misses
  barriers: 4 writers, 209 normal flushes over 800 commits ( 0.26 syncs/commit, 3.84 commits/sync)
  barrier cycle: 4 writers, 1131.1 barriers/s — fsync  0.58 ms, interval  0.88 ms, idle   0.3 ms (34.70% of the wall clock has no flush in flight); coordinator gather   0.2 post  0.01 gap  0.09 ms/barrier
  buckets: 4 writers, busy 736.2 ms over 800 commits — gate_wait 10.60%, gate_hold 7.50%, follower_wait 55.70%, gather_spin 5.90%, fsync 16.90%, post 0.20%, pre-gate residual 2.80% (803 gate waits, 603 racing holds)
  gate hold: 4 writers, 803 holds,  0.07 ms mean —              read  0.01 (2854 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.02 ms (33.10%), residual  0.05 ms (66.90%), 3 commit-point misses
  barriers: 8 writers, 211 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.62 commits/sync)
  barrier cycle: 8 writers, 602.5 barriers/s — fsync  1.05 ms, interval  1.66 ms, idle  0.59 ms (35.90% of the wall clock has no flush in flight); coordinator gather  0.51 post  0.02 gap  0.21 ms/barrier
  buckets: 8 writers, busy   2793 ms over 1600 commits — gate_wait 15.00%, gate_hold 4.20%, follower_wait 66.50%, gather_spin 4.00%, fsync 8.20%, post 0.10%, pre-gate residual 1.60% (1604 gate waits, 1401 racing holds)
  gate hold: 8 writers, 1604 holds,  0.07 ms mean —              read  0.01 (6348 calls), state     0 (8), wal  0.01 (1608, 11.4 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.02 ms (29.50%), residual  0.05 ms (70.50%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         4071          200       0.00%   149.27µs   274.17µs   961.75µs     8.15ms
InlaySQL (parallel WAL regions)                 2         5180          400       0.00%   253.64µs   448.90µs     5.84ms     9.01ms
InlaySQL (parallel WAL regions)                 4         4309          800       0.00%   481.22µs     2.07ms     9.24ms    41.59ms
InlaySQL (parallel WAL regions)                 8         4560         1600       0.00%   897.68µs     5.19ms    28.86ms    41.73ms
SQLite (journal, sync=FULL, fullfsync)          1         1339          200       0.00%   700.23µs   789.93µs     2.83ms     3.88ms
SQLite (journal, sync=FULL, fullfsync)          2         1400          400       0.00%   694.73µs   772.29µs   837.43µs     3.99ms
SQLite (journal, sync=FULL, fullfsync)          4         1364          800       0.00%   691.29µs   770.83µs     0.99ms     10.93ms
SQLite (journal, sync=FULL, fullfsync)          8         1398         1600       0.00%   683.51µs   749.15µs   898.98µs    10.12ms

InlaySQL at 8 writers does 1.13x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-10-19T09:26:43Z
commit: 10bc3fc
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 6.17.0-1022-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000  183.00us  267.00us |   0.988  300.00us  371.00us     3.9s
DuckDB (exhaustive + fts BM25)         1.000   17.07ms   22.54ms |   0.966   35.00ms   42.29ms    77.9s
DuckDB (vss HNSW + fts BM25)           0.994   15.49ms   18.41ms |   0.961   34.37ms   42.82ms    78.5s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.999    3.35ms    4.23ms |   0.418   11.02ms   14.12ms     3.8s
pgvector (HNSW + ts_rank)              0.988  421.00us  561.00us |   0.457   37.35ms   56.39ms     1.5s
pgvector (exhaustive + ts_rank)        0.999    1.25ms    1.35ms |   0.465   37.98ms   56.27ms     0.5s

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
InlaySQL                                                              3319.4  233.00us  430.00us  879.00us |    935999.1    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       3264.0  235.00us  450.00us    1.28ms |    910375.0    1.00us    1.00us    1.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           2507.1  380.00us  492.00us  793.00us |      4039.0  229.00us  300.00us  347.00us
                                                                   commits-per-fsync: 20003/20811 = 0.96
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       4130.4  236.00us  290.00us  372.00us |      8122.6  110.00us  153.00us  176.00us
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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      1720.4  516.00us  661.00us    1.56ms       0 |      3129.4  202.00us  249.00us  264.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      2492.5    1.10ms    2.03ms    4.95ms       0 |      4039.6  317.00us  521.00us  690.00us
                                                                                      commits-per-fsync: 2006/598 = 3.35
                                                                                      commits-per-fsync (checkpoint-inclusive): 2015/606 = 3.33
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1557.5    3.62ms   11.49ms   19.01ms       0 |      1167.4  342.00us    3.10ms    6.09ms
                                                                                      commits-per-fsync: 2017/252 = 8.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2024/258 = 7.84
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      2086.6  383.00us  547.00us  846.00us       0 |      2807.4  221.00us  297.00us  329.00us
                                                                                      commits-per-fsync: 2003/2100 = 0.95
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      4174.9  555.00us  825.00us    1.22ms       0 |      3395.9  313.00us  589.00us  751.00us
                                                                                      commits-per-fsync: 2003/1381 = 1.45
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      1992.3  867.00us    2.15ms    3.97ms       0 |      1092.0  307.00us    2.74ms    6.20ms
                                                                                      commits-per-fsync: 2003/1437 = 1.39

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

