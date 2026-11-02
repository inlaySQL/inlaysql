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

- generated: 2026-11-02T12:08:14Z
- commit: 6535ff8
- workflow: .github/workflows/benchmark.yml (schedule + manual)

## runner-points-repeat.txt

```
date:   2026-11-02T11:41:42Z
commit: 6535ff8
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1017-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T113907Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T114053Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T114117Z.txt

metrics: 46; disagreeing by 10% or more across runs: 13

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  251.3%         max       71.10µs       52.05µs      230.71µs  SQLite (journal, sync=FULL, fullfsync)
  124.0%         max       20.91µs       20.64µs       46.56µs  InlaySQL
   42.7%         max       30.19µs       29.95µs       42.83µs  SQLite (WAL, sync=NORMAL)
   27.2%         max        6.91ms        6.90ms        8.78ms  SQLite (journal, sync=FULL, fullfsync)
   27.2%         max        6.91ms        6.90ms        8.78ms  SQLite (journal, sync=FULL, fullfsync)
   23.5%         p99        2.98µs        2.93µs        3.63µs  SQLite (WAL, sync=NORMAL)
   20.9%         p99        1.63ms        1.43ms        1.77ms  SQLite (journal, sync=FULL, fullfsync)
   20.9%         p99        1.63ms        1.43ms        1.77ms  SQLite (journal, sync=FULL, fullfsync)
   20.1%         max        8.82ms        7.56ms        9.33ms  InlaySQL
   20.1%         max        8.82ms        7.56ms        9.33ms  InlaySQL
   18.8%         max        3.40ms        3.34ms        3.98ms  SQLite (WAL, sync=NORMAL)
   13.2%         p95      466.84µs      434.16µs      495.86µs  InlaySQL
   13.2%         p95      466.84µs      434.16µs      495.86µs  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== point workload: 20000 rows, 200000 lookups by primary key ===
(prepared statements on both sides; parse and plan happen once, outside the loop)

point write (one durable commit each)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                         3282   230.28µs   466.84µs     1.27ms     8.82ms
SQLite (journal, sync=FULL, fullfsync)           1317   701.95µs   938.24µs     1.63ms     6.91ms
SQLite (WAL, sync=NORMAL)                       78737     9.28µs    11.32µs    21.77µs     3.40ms
InlaySQL is 2.37x faster than SQLite (journal, sync=FULL, fullfsync)

batched write (many rows per commit)
engine                                          ops/s        p50        p95        p99        max
InlaySQL (batched)                             187260     3.07µs     3.52µs     6.81µs    11.47ms
InlaySQL                                         3282   230.28µs   466.84µs     1.27ms     8.82ms
SQLite (journal, sync=FULL, fullfsync)           1317   701.95µs   938.24µs     1.63ms     6.91ms
InlaySQL (batched) is 57.05x faster than InlaySQL

point read (by primary key)
engine                                          ops/s        p50        p95        p99        max
InlaySQL                                      1306488   711.00ns   811.00ns   892.00ns    20.91µs
SQLite (journal, sync=FULL, fullfsync)         126717     7.73µs     7.88µs    10.30µs    71.10µs
SQLite (WAL, sync=NORMAL)                      367602     2.63µs     2.72µs     2.98µs    30.19µs
InlaySQL is 10.41x faster than SQLite (journal, sync=FULL, fullfsync)
```

## runner-indexed-repeat.txt

```
date:   2026-11-02T12:06:29Z
commit: 6535ff8
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1017-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T114142Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T114955Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T115816Z.txt

metrics: 42; disagreeing by 10% or more across runs: 12

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   83.4%         max       33.84µs       29.20µs       57.42µs  SQLite (WAL, sync=NORMAL) (index)
   46.9%         max       28.98µs       28.92µs       42.52µs  InlaySQL (B-tree index)
   45.7%         max       57.28µs       44.56µs       70.75µs  InlaySQL (B-tree index)
   32.8%         p95       18.15µs       16.86µs       22.81µs  InlaySQL (B-tree index)
   32.4%         max       66.80µs       45.51µs       67.14µs  SQLite (journal, sync=FULL, fullfsync) (index)
   31.5%         max       24.64µs       16.99µs       24.76µs  SQLite (WAL, sync=NORMAL) (index)
   31.5%         p95        4.57µs        4.46µs        5.90µs  InlaySQL (B-tree index)
   26.7%         p99       29.31µs       21.48µs       29.32µs  SQLite (journal, sync=FULL, fullfsync) (index)
   24.4%         max        7.94ms        7.07ms        9.01ms  InlaySQL (no index: full scan)
   16.5%         p99       26.74µs       26.42µs       30.82µs  InlaySQL (B-tree index)
   15.6%         p99       15.66µs       15.63µs       18.07µs  SQLite (WAL, sync=NORMAL) (index)
   12.4%         p99        7.59µs        7.24µs        8.18µs  InlaySQL (B-tree index)

--- median of all runs, in the layout run.sh printed ---


=== indexed lookup: 20000 rows, 200000 point lookups + 100 range queries (range size 50) by a non-key column ===
(the unindexed row is the same engine on the same rows with no index to use: a full scan, so its cost grows with --rows)

indexed point lookup (WHERE email = ?)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                              234567     4.13µs     4.57µs     7.59µs    57.28µs
InlaySQL (no index: full scan)                          409     2.44ms     2.49ms     2.55ms     7.94ms
SQLite (journal, sync=FULL, fullfsync) (index)       108162     8.73µs    10.42µs    14.28µs    66.80µs
SQLite (WAL, sync=NORMAL) (index)                    242716     3.60µs     5.57µs     5.92µs    33.84µs
InlaySQL (B-tree index) is 573.81x faster than InlaySQL (no index: full scan)

indexed range lookup (WHERE email >= ? AND email < ?, RANGE_SIZE=50)
engine                                                ops/s        p50        p95        p99        max
InlaySQL (B-tree index)                               60857    15.89µs    18.15µs    26.74µs    28.98µs
InlaySQL (no index: full scan)                          357     2.79ms     2.85ms     2.88ms     2.91ms
SQLite (journal, sync=FULL, fullfsync) (index)        56437    17.20µs    19.59µs    29.31µs    30.34µs
SQLite (WAL, sync=NORMAL) (index)                     79726    12.21µs    14.68µs    15.66µs    24.64µs
InlaySQL (B-tree index) is 170.26x faster than InlaySQL (no index: full scan)
```

## runner-joins-repeat.txt

```
date:   2026-11-02T12:07:55Z
commit: 6535ff8
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1017-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T120629Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T120657Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T120726Z.txt

metrics: 74; disagreeing by 10% or more across runs: 15

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
   63.6%         p99       14.48µs        7.67µs       16.88µs  SQLite (WAL, sync=NORMAL) (index)
   27.2%        cold       43.35µs       42.49µs       54.29µs  InlaySQL
   27.2%         max       43.35µs       42.49µs       54.29µs  InlaySQL
   20.1%        cold       15.95µs       14.80µs       18.00µs  SQLite (journal, sync=FULL, fullfsync) (index)
   20.1%         p99       15.95µs       14.80µs       18.00µs  SQLite (journal, sync=FULL, fullfsync) (index)
   19.0%         p99        4.94µs        4.84µs        5.78µs  SQLite (WAL, sync=NORMAL) (index)
   16.1%         max       28.37ms       28.23ms       32.80ms  SQLite (WAL, sync=NORMAL) (index)
   15.1%        cold       15.87µs       14.48µs       16.88µs  SQLite (WAL, sync=NORMAL) (index)
   14.4%         p95        4.43µs        4.38µs        5.02µs  SQLite (WAL, sync=NORMAL) (index)
   12.9%        cold        9.07µs        8.96µs       10.13µs  SQLite (WAL, sync=NORMAL) (index)
   12.9%         max        9.07µs        8.96µs       10.13µs  SQLite (WAL, sync=NORMAL) (index)
   12.1%         p95       17.39µs       15.49µs       17.59µs  InlaySQL
   11.6%         max       17.75µs       15.87µs       17.93µs  SQLite (WAL, sync=NORMAL) (index)
   10.0%        cold       52.36ms       49.03ms       54.29ms  InlaySQL
   10.0%         max       52.36ms       49.03ms       54.29ms  InlaySQL

--- median of all runs, in the layout run.sh printed ---


=== joins: 20000 users, 160000 posts (8/user), 100 runs per query shape, LIMIT 10 ===
(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)

join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 85     52.36ms    11.19ms    12.10ms    13.14ms    52.36ms
SQLite (journal, sync=FULL, fullfsync) (index)           36     27.66ms    27.61ms    27.77ms    28.04ms    28.17ms
SQLite (WAL, sync=NORMAL) (index)                        36    27.66ms    27.76ms    27.88ms    28.30ms    28.37ms
InlaySQL is 2.41x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, PK inner, LIMIT 10 (FROM posts JOIN users ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              97850    43.35µs     9.68µs    10.10µs    22.15µs    43.35µs
SQLite (journal, sync=FULL, fullfsync) (index)       104052    15.95µs     9.35µs     9.71µs    15.95µs    19.79µs
SQLite (WAL, sync=NORMAL) (index)                    224457     9.07µs     4.35µs     4.43µs     4.94µs     9.07µs
InlaySQL is 1.05x slower than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                                 77     84.71ms    12.29ms    12.55ms    13.49ms    84.71ms
SQLite (journal, sync=FULL, fullfsync) (index)           13     75.39ms    75.12ms    75.85ms    76.23ms    78.59ms
SQLite (WAL, sync=NORMAL) (index)                        13     75.37ms    75.16ms    75.97ms    76.30ms    76.90ms
InlaySQL is 5.72x faster than SQLite (journal, sync=FULL, fullfsync) (index)

join, secondary-index inner, LIMIT 10 (FROM users JOIN posts ON posts.user_id = users.id)
engine                                              joins/s       cold        p50        p95        p99        max
InlaySQL                                              65929   116.47µs    13.60µs    17.39µs    27.54µs   116.47µs
SQLite (journal, sync=FULL, fullfsync) (index)        80584    22.38µs    12.09µs    12.72µs    22.38µs    23.47µs
SQLite (WAL, sync=NORMAL) (index)                    140113    15.87µs     6.97µs     7.05µs    14.48µs    17.75µs
InlaySQL is 1.21x slower than SQLite (journal, sync=FULL, fullfsync) (index)
```

## runner-concurrency-repeat.txt

```
date:   2026-11-02T12:08:05Z
commit: 6535ff8
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1017-azure x86_64

runs:   3
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T120755Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T120758Z.txt
        /home/runner/work/inlaysql/inlaysql/bench/results/20261102T120801Z.txt

metrics: 252; disagreeing by 10% or more across runs: 64

Widest disagreement first. A figure listed here is not worth quoting to
three digits: the machine moved it further than that between runs. A `max`
column is one unlucky sample and is expected here; a `p50` or an ops/s
figure is the measurement itself, and swinging is what it is not supposed
to do.

  spread      column        median           min           max  row
  100.0%      lock.)             1             0             1  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   83.7%         max        1.84ms        1.82ms        3.36ms  InlaySQL (parallel WAL regions)
   72.6%         p99        1.22ms        0.96ms        1.85ms  SQLite (journal, sync=FULL, fullfsync)
   72.3%         p95      398.78µs      364.80µs      653.20µs  InlaySQL (parallel WAL regions)
   68.2%         max        3.58ms        3.26ms        5.70ms  SQLite (journal, sync=FULL, fullfsync)
   62.1%         p99        3.96ms        2.99ms        5.45ms  InlaySQL (parallel WAL regions)
   55.8%         p99        1.47ms        1.03ms        1.85ms  SQLite (journal, sync=FULL, fullfsync)
   54.9%      lock.)          0.13          0.13           0.2  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   53.4%         p99      869.64µs      795.51µs     1260.00µs  InlaySQL (parallel WAL regions)
   50.0%         p95      729.77µs      635.05µs     1000.00µs  InlaySQL (parallel WAL regions)
   50.0%      lock.)          0.01             0          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   49.4%      lock.)          0.16          0.15          0.23  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   47.8%         max        4.39ms        4.13ms        6.23ms  SQLite (journal, sync=FULL, fullfsync)
   46.8%         max       11.24ms        9.68ms       14.94ms  InlaySQL (parallel WAL regions)
   41.6%      lock.)          0.24          0.22          0.32  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   39.7%         p95      974.25µs      853.14µs     1240.00µs  SQLite (journal, sync=FULL, fullfsync)
   39.6%      lock.)        11.10%         7.00%        11.40%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   34.2%      lock.)           7.6             5           7.6  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   33.3%      lock.)         0.30%         0.20%         0.30%  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)
   30.9%         max        4.31ms        3.60ms        4.93ms  InlaySQL (parallel WAL regions)
   30.8%      lock.)          0.01          0.01          0.02  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   30.0%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   28.6%      lock.)          0.01          0.01          0.01  gate hold: writers, holds, ms mean — read (<v> calls), state (<v>), wal (<v>, KiB), data (<v>, KiB), of which extend (<v> extensions); device ms (<v>), residual ms (<v>), commit-point misses
   28.4%      lock.)          0.27          0.25          0.33  barrier cycle: writers, barriers/s — fsync ms, interval ms, idle ms (<v> of the wall clock has no flush in flight); coordinator gather post gap ms/barrier
   28.2%      lock.)          53.5          50.5          65.6  buckets: writers, busy ms over commits — gate_wait, gate_hold, follower_wait, gather_spin, fsync, post, pre-gate residual (<v> gate waits, racing holds)

--- median of all runs, in the layout run.sh printed ---


=== concurrent writers: 200 transactions per writer, one row each, OS threads; levels [1, 2, 4, 8] ===
(InlaySQL writers flush separate WAL regions in parallel. SQLite's writers
still serialize at its file lock.)
  barriers: 1 writers, 200 normal flushes over 200 commits (    1 syncs/commit,    1 commits/sync)
  barrier cycle: 1 writers, 3729.5 barriers/s — fsync  0.16 ms, interval  0.27 ms, idle  0.11 ms (39.70% of the wall clock has no flush in flight); coordinator gather     0 post     0 gap  0.15 ms/barrier
  buckets: 1 writers, busy 53.5 ms over 200 commits — gate_wait 0.00%, gate_hold 27.40%, follower_wait 0.00%, gather_spin 0.00%, fsync 61.10%, post 0.30%, pre-gate residual 11.10% (202 gate waits, 0 racing holds)
  gate hold: 1 writers, 202 holds,  0.07 ms mean —              read  0.01 (748 calls), state     0 (0), wal     0 (200, 4.5 KiB),              data  0.01 (200, 15.7 KiB), of which extend  0.01 (2 extensions);              device  0.03 ms (37.50%), residual  0.05 ms (62.50%), 0 commit-point misses
  barriers: 2 writers, 234 normal flushes over 400 commits ( 0.58 syncs/commit, 1.71 commits/sync)
  barrier cycle: 2 writers, 2130.4 barriers/s — fsync  0.24 ms, interval  0.47 ms, idle  0.23 ms (47.80% of the wall clock has no flush in flight); coordinator gather  0.11 post     0 gap  0.13 ms/barrier
  buckets: 2 writers, busy 215.6 ms over 400 commits — gate_wait 5.00%, gate_hold 18.90%, follower_wait 23.80%, gather_spin 10.50%, fsync 27.30%, post 0.50%, pre-gate residual 13.80% (402 gate waits, 208 racing holds)
  gate hold: 2 writers, 402 holds,  0.09 ms mean —              read  0.01 (961 calls), state     0 (1), wal  0.01 (401, 7.6 KiB),              data  0.01 (400, 17.9 KiB), of which extend  0.01 (3 extensions);              device  0.03 ms (33.20%), residual  0.06 ms (66.80%), 1 commit-point misses
  barriers: 4 writers, 211 normal flushes over 800 commits ( 0.26 syncs/commit,  3.8 commits/sync)
  barrier cycle: 4 writers, 1131.8 barriers/s — fsync  0.36 ms, interval  0.88 ms, idle  0.52 ms (58.60% of the wall clock has no flush in flight); coordinator gather  0.35 post  0.01 gap  0.16 ms/barrier
  buckets: 4 writers, busy 744.5 ms over 800 commits — gate_wait 17.20%, gate_hold 11.70%, follower_wait 41.80%, gather_spin 9.90%, fsync 10.60%, post 0.30%, pre-gate residual 8.30% (802 gate waits, 602 racing holds)
  gate hold: 4 writers, 802 holds,  0.11 ms mean —              read  0.01 (2863 calls), state     0 (4), wal  0.01 (804, 10.5 KiB),              data  0.01 (800,   19 KiB), of which extend  0.01 (4 extensions);              device  0.04 ms (32.90%), residual  0.07 ms (67.10%), 3 commit-point misses
  barriers: 8 writers, 212 normal flushes over 1600 commits ( 0.13 syncs/commit, 7.58 commits/sync)
  barrier cycle: 8 writers, 641.7 barriers/s — fsync  0.48 ms, interval  1.56 ms, idle  1.09 ms (69.90% of the wall clock has no flush in flight); coordinator gather  0.86 post  0.03 gap  0.24 ms/barrier
  buckets: 8 writers, busy 2629.3 ms over 1600 commits — gate_wait 25.90%, gate_hold 7.50%, follower_wait 49.60%, gather_spin 6.80%, fsync 3.90%, post 0.20%, pre-gate residual 5.90% (1603 gate waits, 1398 racing holds)
  gate hold: 8 writers, 1603 holds,  0.12 ms mean —              read  0.01 (6474 calls), state     0 (8), wal  0.01 (1608, 11.5 KiB),              data  0.01 (1600, 19.7 KiB), of which extend  0.01 (6 extensions);              device  0.04 ms (30.10%), residual  0.09 ms (69.90%), 3 commit-point misses

engine                                    writers    commits/s    committed  conflicts        p50        p95        p99        max
InlaySQL (parallel WAL regions)                 1         3730          200       0.00%   219.92µs   398.78µs   869.64µs     1.84ms
InlaySQL (parallel WAL regions)                 2         3642          400       0.00%   513.54µs   729.77µs     2.38ms     4.31ms
InlaySQL (parallel WAL regions)                 4         4273          800       0.00%   846.05µs     1.31ms     3.96ms     5.82ms
InlaySQL (parallel WAL regions)                 8         4820         1600       0.00%     1.44ms     3.22ms     6.24ms     11.24ms
SQLite (journal, sync=FULL, fullfsync)          1         1391          200       0.00%   680.72µs   854.84µs     1.22ms     4.22ms
SQLite (journal, sync=FULL, fullfsync)          2         1287          400       0.00%   709.46µs   974.25µs     1.47ms     3.58ms
SQLite (journal, sync=FULL, fullfsync)          4         1359          800       0.00%   695.84µs   863.62µs     1.16ms     4.39ms
SQLite (journal, sync=FULL, fullfsync)          8         1329         1600       0.00%   705.43µs   937.48µs     1.33ms     4.75ms

InlaySQL at 8 writers does 1.25x the work of 1 writer, aborting 0.00% of transactions.
```

## runner-compare.txt

```
date:   2026-11-02T11:45:14Z
commit: 6535ff8
dirty:  no
rustc:  rustc 1.99.1 (58cf8f9c3 2026-10-13)
host:   Linux 7.0.0-1017-azure x86_64
docker: 28.0.4
load:   override/unknown logical CPUs at start (max per CPU: off)


=== retrieval: 5000 docs, dim 128, 100 queries, top-10, seed 42 ===

                                       --- vector search ---     |    --- hybrid (vector + text) ---   
engine                              recall@k       p50       p95 |   agree       p50       p95    build
-------------------------------------------------------------------------------------------------------
InlaySQL (HNSW + BM25)                 1.000   98.00us  178.00us |   0.988  175.00us  218.00us     3.2s
DuckDB (exhaustive + fts BM25)         1.000    8.66ms    9.60ms |   0.966   18.42ms   20.89ms    39.4s
DuckDB (vss HNSW + fts BM25)           0.993    7.94ms    9.80ms |   0.956   18.52ms   21.87ms    40.0s
Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)     0.998    2.19ms    3.02ms |   0.418    6.48ms    7.43ms     2.4s
pgvector (HNSW + ts_rank)              0.988  196.00us  305.00us |   0.456   19.47ms   28.81ms     0.9s
pgvector (exhaustive + ts_rank)        0.999  639.00us  906.00us |   0.465   19.89ms   29.29ms     0.3s

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
InlaySQL                                                              2581.0  155.00us  363.00us    3.01ms |   1562295.8    1.00us    1.00us    1.00us
InlaySQL (containerised, same volume class as MySQL/PostgreSQL)       2636.8  161.00us  346.00us    2.44ms |   1609552.0    1.00us    1.00us    1.00us
MySQL 8 (innodb_flush_log_at_trx_commit=1, binlog disabled)           4661.4  192.00us  311.00us  571.00us |     12937.2   73.00us   94.00us  122.00us
                                                                   commits-per-fsync: 20003/22071 = 0.91
PostgreSQL 17 (fsync=on, synchronous_commit=on)                       7569.8  127.00us  158.00us  203.00us |     28107.6   34.00us   42.00us   56.00us
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
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    1      2437.6  236.00us  415.00us    1.14ms       0 |      7563.7   64.00us   80.00us  176.00us
                                                                                      commits-per-fsync: 2000/2000 = 1.00
                                                                                      commits-per-fsync (checkpoint-inclusive): 2012/2012 = 1.00
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                    4      3121.9  606.00us    1.88ms   20.23ms       0 |      6984.3   80.00us  108.00us  141.00us
                                                                                      commits-per-fsync: 2008/585 = 3.43
                                                                                      commits-per-fsync (checkpoint-inclusive): 2014/591 = 3.41
InlaySQL (server, its own MySQL wire — inlaysql serve --mysql)                   16      1785.7    2.26ms   17.39ms   43.35ms       0 |      2182.4  217.00us    1.01ms    3.34ms
                                                                                      commits-per-fsync: 2013/184 = 10.94
                                                                                      commits-per-fsync (checkpoint-inclusive): 2026/193 = 10.50
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     1      3576.9  187.00us  235.00us  477.00us       0 |      6625.9   81.00us   95.00us  102.00us
                                                                                      commits-per-fsync: 2003/2243 = 0.89
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)     4      7667.8  288.00us  465.00us  695.00us       0 |      7202.0  105.00us  137.00us  174.00us
                                                                                      commits-per-fsync: 2003/1260 = 1.59
MySQL 8 (server-to-server, innodb_flush_log_at_trx_commit=1, binlog disabled)    16      3029.1  451.00us    1.41ms    3.06ms       0 |      1839.2  112.00us  644.00us    2.79ms
                                                                                      commits-per-fsync: 2003/1233 = 1.62

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

