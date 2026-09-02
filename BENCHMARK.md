# InlaySQL benchmarks

Every number here regenerates from a script in this repository. That is the
rule `AGENTS.md` sets and it is the only reason these are worth reading — a
figure nobody can reproduce is worse than no figure. Losses are published
beside wins, because a table that only contains wins is advertising.

**For a workload x SQLite/MySQL/PostgreSQL matrix with a WIN/LOSS/TIE/
FLOOR-BOUND/N/A/UNKNOWN verdict per cell, and the fairness audit (durability,
transport, tuning, structural asymmetry) that verdict rests on, see
[`SCOREBOARD.md`](SCOREBOARD.md).**

> **Runner benchmarks are separate.** A GitHub-hosted runner cannot meet the
> quiet-machine, load-gated standard these numbers were measured under
> (`PERF.md` §4), so automated runs live in their own file:
> [`RUNNER-BENCHMARK.md`](RUNNER-BENCHMARK.md), regenerated weekly and on
> demand by `.github/workflows/benchmark.yml` from the same scripts these
> tables use. Read those numbers as trends against other runner runs — never
> against the tables below. This file remains the source of truth for
every headline figure; that one turns the figures below into a defined win/
lose/tie, cell by cell, and states plainly which cells nobody has filled in
yet.

> **Read this before trusting any number below: the measurement floor.**
> An A/A test — the identical binary, the identical data, measured against
> itself with no code change at all — moves this harness's own figures by up
> to 2.6x run to run. Measured directly (`PERF.md` §4, 2026-08-30): median
> CoV **4.0%** on the main suite's core columns (ops/s, p50, joins/s,
> commits/s, recall@k), **3.6%** on the concurrency wide sweep, **0.3%** on
> the quantisation spot-check, and **7.3%** on the single most scrutinised
> metric — point-read ops/s — repeated five times on one unrebuilt binary on
> a quiet, gated machine, rising to **20.2%** for that same metric on this
> same machine under its ordinary desktop load (Chrome, VS Code, an agent
> session — confirmed, not assumed). Even on the quiet, gated runs: **53 of
> 108 core main-suite metrics (49%) disagreed by 10% or more** across three
> runs of nothing changing. **No difference smaller than these floors is a
> result — it is this benchmark disagreeing with itself.** Every table below
> built from a repeated `run.sh`/`repeat.sh` invocation states its **median
> and the min–max range the actual runs produced**, not a single point value,
> and every multiple in the prose is rounded to the precision its own spread
> supports (`~3x`, not `3.26x`) — except where an effect is large enough to
> clear the floor by a wide margin, which is stated plainly rather than
> hedged into mush. Single-run tables (`compare.sh`, MySQL/PostgreSQL,
> server-to-server, `ann-benchmarks`) have no repeat of their own to measure a
> spread from; read their ratios as *less* certain than the gated `run.sh`
> tables here, not more precise for lacking a stated range.

**How this run was produced**

```sh
REPEATS=3 ./bench/repeat.sh                                        # points, indexed, joins, vectors, concurrency (1,2,4,8), retrieval — this edition
WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32 SUITE=concurrency ./bench/run.sh   # x3, median — wide sweep + tail latency (carried forward)
SUITE=quantization DOCS=100000 QUERIES=50 ./bench/run.sh           # x3, median — int8 spot-check at scale (carried forward)
./bench/compare.sh                                                 # DuckDB, pgvector, Meilisearch (needs Docker) — single run (carried forward)
```

| | |
| --- | --- |
| Commit | `7b20175` |
| Date | 2026-09-02 |
| Tree | source clean at measurement (`dirty: no` in all three `run.sh` raw outputs and in the `repeat.sh` summary). |
| Machine | Apple Mac17,9, 18 cores, macOS 27.0 (Darwin 27.0.0 arm64) |
| Toolchain | rustc 1.91.1 (ed61e7d7e 2025-11-07) |
| Raw output | **`run.sh`/SQLite/sqlite-vec/concurrency/retrieval, median of three** (`SUITE=all`: points, indexed, joins, vectors, concurrency 1/2/4/8, retrieval): `bench/results/20260902T022325Z-repeat.txt`, built from `bench/results/20260902T{022325,023047,023804}Z.txt`. Load, sampled every 5 s throughout the measured phases, min/median/max per run: 0.82/2.06/3.37, 2.03/2.72/4.04 and 1.36/2.03/3.13 of 18 CPUs against the gate's 0.25/CPU (4.5) ceiling; no run marked `CONTAMINATED`. **Carried forward from the 2026-08-30 edition at `2cb2539`, not regenerated this edition** (each section says so where it appears): the **concurrency wide sweep + tail latency** (`bench/results/20260830T{124155,124632,125240}Z.txt`, `WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`, median of three, load 2.9–3.6/18 — this regeneration ran only the default 1/2/4/8 levels, so the 1/2/4/8 table is fresh and the eleven-level sweep and its 32-writer tail row are not); the **quantisation spot-check at scale** (`bench/results/20260830T{125800,131326,132715}Z.txt`, `SUITE=quantization DOCS=100000 QUERIES=50`, median of three, load 2.3–4.8/18); the **DuckDB/pgvector/Meilisearch retrieval** table (`bench/results/20260830T134642Z-compare.txt`, single run, load 1.1–1.9/18, four unrelated Docker containers idle throughout — `compare.sh` was not run this edition, so every `compare.sh`-sourced table is still the single ungated run it was). **Carried forward from earlier still**: the "Against MySQL and PostgreSQL" table, its correction and its interleaved rerun (commit `b4798ce`, 2026-08-30, 5 repetitions, load-gated — `bench/results/20260830T095714Z-interleaved-oltp-compare.txt` and `bench/results/20260830T095714Z-rep{1..5}-{inlaysql-container,mysql,postgres}.json`); the "Server-to-server" table (process-based driver, 2026-08-29, `f8e29e9`); the concurrent-writer old-vs-new A/B (`08f5fd4`, 2026-08-30, `bench/results/ab-head-run{1,2,3}-*.txt` and `ab-pre94d96a6-run{1,2,3}-*.txt`). |

One developer machine. Reproduce it; do not trust it. Every `run.sh` table
on this page — points, indexed, vectors, concurrency at 1/2/4/8 writers,
retrieval — comes from `7b20175`, measured fresh in one gated sitting on
2026-09-02, the full regeneration the previous edition said it owed. Every
other table is an explicitly carried-forward section whose own commit and
date are stated where it appears, per table, so a reader can always tell
which build produced which number. **One exception, in the other direction:
the joins table is deliberately *not* updated to this sitting's figures.**
This run measured them, and what it measured was a regression on a published
winning row that `PERF.md`'s AHL-524 section bisects to a commit inside the
`2cb2539..7b20175` range; the fix landed after this run, and a gated
joins-only regeneration at that fix follows and will replace the table. Until
then the joins table still reads as the `2eeced7` edition and says so in
place.

**Tooling correction, 2026-08-31 — every `compare.sh`-sourced table below is
now owed a regeneration.** Several sections on this page disclose, correctly
for the numbers they carry, that `bench/compare.sh` had no quiet-machine load
gate and no repeat wrapper, so its figures are a single ungated pass where the
`run.sh` tables are gated medians of three. **Both now exist**:
`bench/load_gate.sh` is shared by `run.sh` and `compare.sh` (same gate, same
mid-run sampling, same `CONTAMINATED` marking — `compare.sh` watches only its
measured phases, not its own container builds), and
`REPEATS=5 ./bench/repeat-compare.sh` reports a median and spread through the
same `bench/summarise.py`. Nothing on this page has been re-measured with them
yet, so every such table still reads exactly as it did — a single run, stated
as such. The instrument existing does not change the number; it changes what
the *next* edition of these tables owes, which is a gated, repeated one. What
is still not addressed: interleaving the engines *within* one pass
(`compare.sh`'s phase order is fixed), which is the half of the recommendation
`bench/README.md` still carries.

**This edition's spread is narrower than the last full one, on the same
tool and the same 343 metrics — and still nowhere near the floor.** The main
`run.sh` suite (points/indexed/joins/vectors/concurrency/retrieval), median of
three complete runs at `7b20175`: **106 of 343 metrics disagreed by 10% or
more** across the three, against 196 of 343 in the 2026-08-30 edition, both
counted by the same `bench/summarise.py`. On the columns that are the
measurement itself (ops/s, p50, joins/s, commits/s, recall@k — excluding
`max`/`p95`/`p99`/`cold`, which are one sample and expected to swing far
more) it is 19 of 135 (14%); the previous edition's "53 of 108 (49%)" was
counted over a slightly different column selection (`PERF.md` §4), so
compare the whole-suite 106-versus-196 and not the two core fractions
digit for digit. The machine was quieter — median load 2.0–2.7/18 across the
three runs against 3.0–4.4 last time — and the tightest tables here (indexed
lookups, durable writes, retrieval) are tight enough that their second
digit is worth something for the first time. The loudest were the same ones
as always: the point-read row (InlaySQL ops/s 57%, p50 81% across three runs
of one binary), the 2-writer concurrency row (p50 54%, commits/s 26%), and
SQLite's own journal-mode point-read ops/s (33%). The previous edition's
history — first published as "worse than the last full edition's 56 of 285",
then recomputed on the 266 metrics common to both editions as 54/266 (20.3%)
then, 146/266 (54.9%) there — stands as written in `PERF.md` §4. Read every
ratio in this document as approximate, not as three significant digits, and
read a "the previous edition's figure was X, this one is Y" sentence as this
benchmark's ordinary noise unless the text says otherwise and the movement
clears the floor stated at the top of this file. This session's machine
carried its usual mix of editor, browser and agent processes throughout
(disclosed per-phase below). The carried-forward `compare.sh` run had four
unrelated, idle Docker containers present — quieter than an earlier edition's
eleven, still not a pristine machine, and stated rather than hidden.

---

## Against SQLite

SQLite is measured in two configurations because they are two different
promises. `journal` + `synchronous=FULL` + `fullfsync` is the like-for-like
column: it is the only one that makes a durability claim comparable to ours,
and `fullfsync` is what makes a macOS number mean anything at all. WAL +
`synchronous=NORMAL` is SQLite at its fastest, and is the harder target.

### Point reads by primary key — we beat the durable configuration

20,000 rows, 5,000 lookups, prepared statements on both sides. Median of
three runs (`bench/results/20260902T{022325,023047,023804}Z.txt`, load
0.8–4.0/18 throughout, gate passed).

| Engine | ops/s (median, range) | p50 (median, range) | p95 (median)† |
| --- | --- | --- | --- |
| **InlaySQL** | **533,943** (365k–672k) | **1.04 µs** (0.79–1.63 µs) | 6.54 µs |
| SQLite, WAL + `sync=NORMAL` | 1,257,756 (1,215k–1,264k) | 0.75 µs (0.75–0.79 µs) | 0.92 µs |
| SQLite, journal + `sync=FULL` | 170,234 (169k–224k) | 5.42 µs (4.38–5.46 µs) | 8.00 µs |

† `p95` (and `p99`/`max`, not shown) is one tail sample and swings far more
run to run than `ops/s` or `p50` — see the floor note at the top of this
file — so it is not given a range here.

**Roughly 2-3x the durable configuration.** This session's own three
individual-run ratios against journal-mode SQLite were 2.99x, 2.15x and
3.16x (the harness's own "is Nx faster" lines); the median run says 2.99x,
but a number whose InlaySQL side swung 57% (365,231 to 671,689 ops/s) across
three runs of one unrebuilt binary cannot support a second significant
figure. That spread is the inverse of last edition's, where SQLite's journal
row swung 75% and ours 12%: this time SQLite's journal ops/s held within 33%
and its WAL row within 4%, and it was InlaySQL's own row that moved — the
p50 itself ranged 0.79–1.63 µs, an 81% spread, the single widest core-column
disagreement in this whole run. We lose to WAL-mode SQLite by roughly 2-3.5x
(0.29x–0.55x per run), a wider loss than last edition's "roughly 2x" and,
given this row's spread, not distinguishable from it. The page cache
(AHL-420) is what does the winning half on a *warm* handle; a cold one warms
more slowly than SQLite's because our miss path is dearer.

This row has now been published at 636,980, then 342,747, then 901,158, then
522,562, and now 533,943 ops/s across five editions. The median barely moved
this time (+2%, well inside its own spread), and the commits that landed
between the last full edition and this one (`git log 2cb2539..7b20175`:
AHL-512 through AHL-522) were profiled against exactly this shape as they
landed — `PERF.md`'s AHL-521, AHL-522 and AHL-523 sections each report the
`points` profile flat, "mixed sign, the point read did not move" — so no
change is claimed here and none is visible. **The swing is no longer purely
mysterious: `PERF.md` §4 dissected this exact metric directly and found
background scheduling contention alone triples its CoV, from 7.3% on a quiet,
gated machine to 20.2% on this same machine under ordinary desktop load, on
five runs of one unrebuilt binary — no rebuild, no edition change, no code
touching the read path at all.** This gated sitting reproduced that: the
widest of its three runs is 1.8x the narrowest, on a machine that passed the
load gate throughout. That is the worst-measured floor of any row in this
document, which is why this edition publishes a median of repeated runs and
why the ratio against journal-mode SQLite — read as "roughly 2-3x," not to
three digits — is the number to quote, not the point value either side of
it.

### Secondary-index reads — point win, range loss

20,000 rows, `CREATE INDEX` on a non-key TEXT column, 5,000 point lookups and
100 range queries of 50 rows (`SUITE=indexed`). Same three runs as the point
reads above.

| Engine | point ops/s (median, range) | point p50 (median, range) | range ops/s (median, range) | range p50 (median, range) |
| --- | --- | --- | --- | --- |
| **InlaySQL (B-tree index)** | **421,625** (422k–431k) | **2.08 µs** (2.04–2.13 µs) | 77,559 (76k–81k) | 12.13 µs (11.75–12.33 µs) |
| InlaySQL (no index: full scan) | 821 (819–826) | 1.21 ms (1.20–1.21 ms) | 603 (600–609) | 1.65 ms (1.64–1.66 ms) |
| SQLite, journal (index) | 260,282 (256k–263k) | 3.63 µs (3.63–3.75 µs) | **121,531** (121k–143k) | **7.63 µs** (6.75–7.75 µs) |
| SQLite, WAL (index) | 749,148 (744k–761k) | 1.17 µs (1.13–1.17 µs) | **198,857** (197k–200k) | **4.79 µs** (4.79–4.92 µs) |

The index itself is worth **roughly 515x** over our own full scan on point
probes and **roughly 130x** on range scans (AHL-423; the harness's own
per-run figures were 511x/525x/515x and 125x/134x/129x — the previous
edition read ~550x/~130x, and the point-probe multiple fell because the full
scan got faster, 677 → 821 ops/s, not because the index got slower). **This
is the tightest this table has ever been**: every InlaySQL cell held within
6% across the three runs and every SQLite cell within 4% bar journal-mode's
range row (15-18%), against 57-74% swings on both sides last edition — so for
once the second digit here is real. **On point probes we beat journal-mode
SQLite by roughly 1.6x** (1.62–1.65x per run; last edition read roughly 1.5x
on far noisier data) **and trail WAL-mode at roughly 0.55x** (0.55–0.57x,
essentially flat). **Range scans we lose outright — roughly 0.6x of journal
and roughly 0.4x of WAL** (0.57–0.64x and 0.38–0.41x per run), a shade
better than last edition's 0.5x/0.35x: InlaySQL's range ops/s moved 64,250 →
77,559 (+21%) while both SQLite rows stayed put. That is outside this
table's own spread this time, but it is also the only column here whose
previous-edition range (25k–72k) was wide enough to contain the new median,
so it is stated as a likely, not a certain, improvement; `PERF.md`'s AHL-522
section measured `indexed-range` flat across the one change in this range
that touches the scan path, so nothing in `2cb2539..7b20175` is credited for
it. The entry-walk plus per-row fetch overhead named in the point-read
section is still the suspect for the range loss — the same family as the
join loss below.

### Joins — we win one shape, lose the other

20,000 users × 160,000 posts, identical schema and indexes on both sides
(`SUITE=joins`). Each row splits the cold first execution of the query shape
from the warm p50 — the cold column is where the join plan and its tables get
built, so it is the expensive one:

**Regenerated 2026-09-01 at `2eeced7`** (`SUITE=joins REPEATS=3`, median of
three, quiet-machine gate passed throughout and no run marked `CONTAMINATED`;
raw: `bench/results/20260901T032752Z-repeat.txt`). This table is now the
only one on this page *not* from the 2026-09-02 edition at `7b20175`: that
run's joins figures are deliberately withheld, because `PERF.md`'s AHL-524
section bisects what they showed to a commit inside `2cb2539..7b20175` whose
fix landed after the run, and a gated `SUITE=joins REPEATS=3` regeneration at
that fix follows and will replace this table. It was regenerated on its own
at `2eeced7` because three changes landed against exactly these shapes
(below) and the figures before it predated all of them.

| Query shape | InlaySQL cold → p50 (median) | SQLite journal cold → p50 (median) | vs journal |
| --- | --- | --- | --- |
| PK inner, full join | 16.29 ms → 11.72 ms | 10.99 ms → 9.99 ms | **~1.15x slower** |
| PK inner, LIMIT 10 | 68.50 µs → 6.13 µs | 8.58 µs → 3.33 µs | ~2.0x slower |
| Secondary-index inner, full | 36.59 ms → 3.71 ms | 30.24 ms → 30.26 ms | **~7.5x faster** |
| Secondary-index inner, LIMIT 10 | 72.38 µs → 8.75 µs | 15.25 µs → 4.33 µs | ~2.1x slower |

**Both `LIMIT` rows improved, and the reason is three landed changes rather
than a quieter afternoon.** They were 2.8-3.5x and 2.2-2.6x slower in the
previous edition and are 2.0x and 2.1x here. What changed: `e4086ad` gave the
raw leaf scan a cache for the *undecoded* pages it re-reads on every execution
of a prepared statement (1.38x on these two shapes, measured interleaved), and
`2da02a7`/`3e0eec1` let the hash join build on collated `TEXT` and on `REAL`
keys, which those shapes do not use but which removed the same class of
over-restriction. The `joins/s` and `p50` columns did not appear in this run's
own ≥10% disagreement list, so the second digit here is better supported than
this table's history would suggest; `cold`, `max` and `p99` are single samples
and swung 20-214%, which is what those columns always do.

The last column is the harness's own throughput ratio (joins/s against
joins/s); the range given is not a formatting flourish — InlaySQL's own
joins/s swung 10-38% and SQLite's 4-11% across the three runs measured for
this table, so the range is every combination of the two sides' own min and
max, not an invented error bar. All four ratios stay on the same side of 1.0x
across that whole range (nothing here flips from a win to a loss depending on
which run you look at), which is exactly why these are stated as real
findings rather than as noise, even though the second digit of each is not
reliable. It is close to but not identical with the ratio of the two p50
columns beside it, because the p50 discards the cold run the throughput
figure includes.

Published because it is true, and because it keeps moving: the
secondary-index inner shape — the one AHL-464 built the index nested-loop join
for — went from **10.71x slower** (2026-08-20) to 2.85x faster (`9aba437`) to
3.65x faster (`9b2f11e`, AHL-447) to roughly 8x faster (previous edition) to
**roughly 5-9x faster** here, and the PK inner full join from 5.56x slower to
1.43x to 1.20x to roughly 1.1x slower to **roughly 1.1-1.3x slower**. No code
touched either join path between the previous edition and this one (`git log`
shows only doc commits and unrelated feature/write-path work — see the
point-reads section above); both figures' point estimates moved by roughly
9-11% edition to edition — smaller than this table's own 10-38% run-to-run
spread, so **treat that edition-to-edition move as within noise**, not a
regression or an improvement. The two changes that produced the earlier,
larger jump are unchanged and still
the explanation for why these numbers are where they are at all: `1f0bdcb`
let the raw leaf scan read through the page cache instead of re-`pread`ing and
re-copying the same pages on every execution of a prepared query (the `LIMIT`
rows' fix, below), and `bfac72a` (AHL-479) retains the entry-range walk's leaf
across calls the same way AHL-472 already retained one for point lookups,
removing the one-descent-per-outer-row cost the secondary-index-inner shape
was paying. Both are real, reproducible fixes, not run-to-run variance — see
`PERF.md` for the profiles that motivated each.

The `LIMIT` rows are still a loss, essentially unchanged from last published:
**roughly 2.8-3.5x and 2.2-2.6x slower** warm, against roughly 3.3x and 2.4x
last edition — both within the noise band above, not a further improvement or
regression. What is left, profiled fresh rather than assumed (`PERF.md`'s
AHL-488/493
sections): the same page-decode allocation cost those sections already
diagnosed and, in AHL-493's case, already tried twice and rejected for
regressing point reads and small joins — not a new opportunity, a confirmed
one still open. The likelier next win is not in this hot path at all: these
two shapes are the same two tables in opposite `FROM` order, and the
7.93x-faster shape only wins because it drives the join from the smaller side
(20,000 outer iterations against a secondary-index range probe) rather than
the larger one (160,000 outer iterations against a primary-key point probe).
Nothing here chooses that automatically — `FROM posts JOIN users` and
`FROM users JOIN posts` get whichever physical order was written, and join
column ordinals are resolved against that written order at plan time, so
picking the cheaper side is not a local change: it needs the physical
iteration order decoupled from the logical column layout a plan's
expressions already reference. Scoped, not started.

### Durable writes — we win

One row per commit, one `fsync` per commit. Median of three runs, same
session as the tables above.

| Engine | ops/s (median, range) | p50 (median, range) |
| --- | --- | --- |
| **InlaySQL** | **241** (237–278) | **3.98 ms** (3.82–4.02 ms) |
| SQLite, journal + `sync=FULL` + `fullfsync` | 97 (96–98) | 10.76 ms (10.58–10.86 ms) |

**~2.5x** (2.44x, 2.47x and 2.90x, the harness's own per-run lines; median
run 2.47x) — down from the previous edition's ~2.7x, and the whole of that
move is SQLite's side: its journal-mode row went 90 → 97 ops/s (2% spread
this time) while InlaySQL's median stayed at 241. InlaySQL's 278 ops/s
maximum is one run (`023047Z`) whose p50 was 3.82 ms against 3.98–4.02 ms in
the other two, and is why the third significant figure of the ratio is not
offered. Still the most stable row in this document across editions: the
commit gate no longer re-derives the log on every commit (AHL-468), which
paid on the solo path too. Batching lifts the same workload to 60,990 ops/s
(60,444–61,936, 2.5% spread) at 10.63 µs (10.50–10.75 µs) — **~250x**
(217x, 257x and 257x per run; the 217x is the 278-ops/s run's denominator,
not a slower batch) — which is the number to quote for a bulk load and not
for a transaction.

Every row above is full-durability, on both sides of every comparison, on
purpose — an opt-in relaxed-durability tier also exists
(`EngineOptions::durability`) and is measured separately, in `PERF.md`, not
mixed into these tables.

### Concurrent writers — the peak sits at sixteen, and past it the win still shrinks

200 transactions per writer, one row each, on real OS threads. Median of
three runs at `7b20175` (`bench/results/20260902T{022325,023047,023804}Z.txt`,
the default `WRITER_LEVELS` of 1/2/4/8, load 0.8–4.0/18 throughout, gate
passed). The eleven-level wide sweep and the tail-latency table further down
were **not** re-run in this sitting and are carried forward from the
2026-08-30 sweep at `2cb2539`, as each says in place — so this page again
carries two concurrency sessions, and the two 8-writer figures (1148 here,
1209 there) are the same measurement three days apart, not a discrepancy to
resolve.

| Writers | InlaySQL commits/s (median, range) | SQLite commits/s (median, range) |
| --- | --- | --- |
| 1 | 244 (244–246) | 85 (83–87) |
| 2 | 393 (374–475) | 85 (83–91) |
| 4 | 605 (545–616) | 88 (83–90) |
| 8 | **1148** (1123–1206) | 87 (87–90) |

**Roughly 13x SQLite at 8 writers (12.8-13.9x across this run's own three
pairings, median 13.2x), 0.0% aborted — against the 13.7x the previous
edition's wide sweep published, itself up from 8.1x before the adaptive
gather window (`94d96a6`, unchanged since). The 8-writer InlaySQL row spread
7% this time (1123–1206) against 0.9% in the wide sweep, and its median sat
5% under that sweep's 1209 — inside this benchmark's ordinary noise for a
ratio that is nonetheless far outside any floor, and stated plainly for that
reason.** The commit gate's pre-`fsync` gather window
(`coalesce_normal_commits`, `crates/inlaysql/src/device.rs`) keeps yielding
while a normal commit is inflight or waiting and progress keeps happening,
closing on stalled progress instead of a fixed 8-yield count — see `PERF.md`
for the full mechanism, unchanged since it shipped; the only change to
`device.rs` in `2cb2539..7b20175` that goes near it is AHL-497's timing
counters *around* the gather window, not the window's logic. The 8-writer
scaling (1148 against 244 at one writer) is roughly 4.7x by the harness's
own line — 4.94x, 4.71x and 4.56x per run — against the previous edition's
roughly 5x, within noise. **The
2-writer case moved back up**: 393 against 244 is roughly 1.6x (1.53–1.95x
per run — this run's second-loudest core-column row, p50 spread 54%,
commits/s 26%), against last edition's 304 (roughly 1.25x) and the edition
before's 1.60x. Three editions have now put this one point at 1.60x, 1.25x
and 1.6x with no change to the coalescing code between any of them, which
is the clearest demonstration on this page that a two-writer ratio is a
noise measurement; the 8-writer figure is the one to trust.

**Published because it is true, not because it flatters us: eight writers is
still not the peak.** **Carried forward from the 2026-08-30 wide sweep at
`2cb2539` (`bench/results/20260830T{124155,124632,125240}Z.txt`,
`WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`, load 2.9–3.6/18), not re-run
this edition** — the figures from here to the end of the tail-latency table
below are that sweep's. Its 1/2/4/8 points differ from the fresh table
above by 0%, 29%, 3% and 5% respectively — the 2-writer point being the
noise measurement the paragraph above describes, the other three inside
the overlap of the two sessions' own ranges. All eleven levels (medians;
run-to-run
spread at each point ranges from 0.9% at the tightest, 8 writers, to 21.7% at
the loosest, 3 writers): 244 → 304 → 480 → 587 → 803 → 952 → 1209 → 1545 →
**1616** → 1307 → 974 commits/s from 1 to 32 writers. The peak is now clearly
at 16 writers, with 12 close behind (1545, 4.4% under the peak) — smaller
than either point's own run-to-run spread (16 writers 3.8%, 12 writers 6.6%),
so this table cannot actually distinguish "16 is the peak" from "12 and 16 are
tied"; the previous edition read them as an indistinguishable plateau
(1519/1597, swapping which nominally won across runs) and this sweep's
resolution to a single top may just be this edition's own noise landing a
different way, not a real narrowing. The falloff past the peak reproduces the
previous edition's shape closely: 16 → 24 is a 19% drop (was 17%), 24 → 32 a
further 26% (was 25%) — both larger than either endpoint's own spread (16w
3.8%, 24w 7.1%, 32w 4.0%), so this part of the shape is real. Every point
still sits well above the pre-`94d96a6` ceiling: 32 writers now does 974
commits/s (967–1006 across this sweep, was 988, essentially flat) against an
old ceiling of 694 at any writer count and an old 32-writer figure of 516 —
**roughly 1.9x higher even at the new curve's worst point** (was 1.91x, and
974's own 4.0% spread does not put this claim at any real risk). SQLite's own
row stays flat across the identical sweep (85–92 across the medians at each
writer count), so this remains specific to how this engine's writers
contend, not generic OS thread-count overhead.

The root cause of the *original* 8-then-falls shape is unchanged and is not
what this fix touches: every writer's whole commit *prepare* phase (conflict
check, WAL encode, page writes, WAL append) still serializes behind one
process-wide gate regardless of how many WAL regions exist; the regions only
let one writer's `fsync` overlap the *next* writer's turn at that gate,
confirmed by profiling (90.4% of samples parked waiting for it). The obvious
cheap fix — spin before parking, in case kernel wake latency rather than the
gate itself was the cost — was tried, measured clean, and reverted: no
change, at 100 or at 5,000 spin iterations. A follow-up idea (shrink the gate
to the conflict check and sequence/offset reservation only, move the encode
and writes after release) turned out to be unsafe, not just unscoped: the
conflict check walks the tree from the latest committed root, so it
structurally depends on the previous writer's pages already being landed.
Finer profiling also found the gate-held section was already cheap (under 6%
of the time; the rest is pure contention). What *did* move the needle this
time was a different, smaller lever: the fixed-yield gather window on the
*flush* side, described above — not the gate itself. See `PERF.md` for the
full investigation and the one lever still standing for the residual
regression above the new, higher peak: *commit-side* logical group commit
(one gate holder absorbing other waiting writers' whole transactions into
one prepare/encode/WAL-append pass, not just one `fsync` covering several
already-encoded ones), scoped but not started.

### Concurrent writers: the tail the commits/s table hides

`08f5fd4` added per-commit p50/p95/p99/max to `inlaysql-bench --suite
concurrency`. The wide sweep above already measured every writer level
with percentiles, so this table is a slice of it (`1, 8, 32`) rather than a
separate session — **and, like that sweep, it is carried forward from
2026-08-30 at `2cb2539`, not regenerated this edition**, because the
2026-09-02 run stopped at 8 writers and has no 32-writer row to put here.
For the record, that fresh run's own 1- and 8-writer tails (p50 / p95 / p99
/ max): 4.05 / 4.29 / 7.61 / 8.29 ms at 1 writer and 5.10 / 20.98 / 39.76 /
51.00 ms at 8 — the same shape as the rows below, with the 8-writer p99
9 ms higher, which is what a one-sample tail column does between sessions.

| Writers | InlaySQL commits/s | p50 | p95 | p99 | max | SQLite commits/s | p50 | p95 | p99 | max |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 244 | 4.11 ms | 4.37 ms | 7.94 ms | 8.93 ms | 85 | 11.15 ms | 13.04 ms | 14.03 ms | 15.12 ms |
| 8 | **1209** | 4.97 ms | 19.23 ms | 31.01 ms | 42.98 ms | 88 | 11.15 ms | 12.98 ms | 16.93 ms | 31.41 ms |
| 32 | **974** | 22.83 ms | 97.19 ms | **121.08 ms** | 157.00 ms | 88 | 11.19 ms | 13.02 ms | **15.35 ms** | 40.17 ms |

Medians only, deliberately: `commits/s` and `p50` are individually tight
across this sweep's three runs (0.6-15.9% spread at these three writer
counts), but `p95`/`p99`/`max` are not — InlaySQL's own p95 at 1 writer
swings 109% run to run, and SQLite's own max at 32 writers swings 381%. A
column-by-column range here would bury the finding this table exists to show
under noise wider than the effect; the *shape* (InlaySQL's tail growing with
writer count while SQLite's stays flat) is the trustworthy part, not any
single p99 figure to three digits.

**Published beside the win because it is the same trade: at 32 writers
InlaySQL does roughly 11x SQLite's committed throughput (10.2-11.8x across
this sweep's own runs) and loses p99 by roughly 8x against SQLite's own tail
(7.3-9.4x across the same runs)** (the previous edition read 11.4x/6.9x on a
different sweep; both numbers moved inside this benchmark's usual band for a
p99 figure). Both ratios are far enough from parity, in every run this sweep
measured, to state plainly rather than hedge. SQLite's own tail stays inside
roughly 11–16 ms at every writer count measured here because SQLite
serializes writers at its file lock — the connection that's waiting pays in
queueing, not in a longer `fsync`. InlaySQL's optimistic design instead lets
the gather window grow the cohort riding one `fsync` as contention rises, and
the writers gathered late in a big cohort are the ones sitting in its p99:
p50 at 32 writers (22.83 ms) is not far past solo (4.11 ms, mostly one
`fullfsync`), but p99 (121.08 ms) is roughly 5x that p50. This is a real cost
of the concurrent-writer design, not noise, and it belongs in the table, not
a footnote.

**Carried forward from `08f5fd4` (2026-08-30), not regenerated this
edition** — the A/B below deliberately reverts code to a pre-`94d96a6` state
for one side of the comparison, which this regeneration did not repeat since
no code changed that the experiment would need to re-measure.

**Whether the adaptive gather window (`94d96a6`) is *why*: no — the data says
the opposite.** Nobody had measured p99 before `08f5fd4` existed, so it was
an open question whether trading a fixed 8-`yield_now` gather window for an
adaptive one (up to `COMMIT_COALESCE_MAX_YIELDS` = 16,384, closing on
`COMMIT_COALESCE_STALL_YIELDS` = 1,500 yields of no progress —
`crates/inlaysql/src/device.rs`) had bought throughput at the tail's expense.
Three interleaved A/B pairs (old, new, old, new, old, new — "old" built by
temporarily setting `COMMIT_COALESCE_MAX_YIELDS` back to `8`, which is
provably identical to the pre-`94d96a6` fixed loop because
`COMMIT_COALESCE_STALL_YIELDS`, at 1,500, can never be reached within 8
iterations), same `WRITER_LEVELS=1,8,32`, median of three runs each
(`bench/results/ab-pre94d96a6-run{1,2,3}-*.txt`,
`bench/results/ab-head-run{1,2,3}-*.txt`):

| Writers | old commits/s (median, range) | old p99 (median, range) | new (`94d96a6`) commits/s (median, range) | new p99 (median, range) | Δ commits/s (range) | Δ p99 (range) |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 247 (239–257) | 7.94 ms (7.76–9.02 ms) | 250 (243–272) | 7.88 ms (6.99–7.92 ms) | +1.2% (noise) | −0.8% (noise) |
| 8 | 576 (537–703) | 45.01 ms (41.04–53.15 ms) | 1142 (1079–1193) | 34.90 ms (33.22–36.21 ms) | **+98.3%** (+54% to +122%) | **−22.5%** (−12% to −38%) |
| 32 | 504 (449–516) | 150.89 ms (142.12–152.84 ms) | 968 (898–987) | 122.13 ms (118.94–123.05 ms) | **+92.1%** (+74% to +120%) | **−19.1%** (−13% to −22%) |

The adaptive window **roughly doubles throughput and lowers p99** at both 8
and 32 writers, and the direction was consistent across all three pairs at
both levels — not a one-off, and not a coin flip either: even taking the
least favourable combination of each side's own three runs, throughput still
improves (+54% at worst, 8 writers) and p99 still falls (−12% at worst, 8
writers) at every writer count that matters. That is why this is stated as a
real win, not hedged — the range never crosses back to "no change." The tail
loss against SQLite above is real and stays in the table, but it predates
`94d96a6` and was *worse* under the old fixed window (150.89 ms p99 at 32
writers, against 122.13 ms now) for barely half the throughput. It is a
structural cost of gathering more commits behind fewer `fsync`s under
contention, not something the adaptive window introduced — so this edition
does not re-tune `COMMIT_COALESCE_MAX_YIELDS` / `COMMIT_COALESCE_STALL_YIELDS`.
The data does not call for a trade that would give back a p99 win already
banked, and the shipped constants are unchanged.

---

## Against `sqlite-vec` — we win

2,000 vectors, dim 384, 100 queries, top-10, recall measured against an
exhaustive oracle. Median of three runs, same session as the SQLite tables
above (`bench/results/20260902T{022325,023047,023804}Z.txt`).

| Corpus | recall@10 | p50 (median, range) | vs `sqlite-vec` (median, range across the 3 runs) |
| --- | --- | --- | --- |
| Text-derived embeddings | 1.000 | 69.54 µs (65.83–72.79 µs) | **~9-10x faster at 100% of its recall** (per-run ratio 9.13–9.92x, median 9.56x) |
| Uniform random | 0.922 | 92.08 µs (91.17–92.83 µs) | ~7x faster at 92.2% of its recall (6.77–7.69x, median 6.87x) |

The multiples are the median of the three runs' own per-run ratios (the
harness's "is Nx faster" lines), not the ratio of the two median p50s — this
time the two methods agree (664.79 / 69.54 = 9.56x either way, realistic;
628.58 / 92.08 = 6.83x against 6.87x, uniform) because `sqlite-vec`'s own p50
held within 10-13% run to run rather than last edition's 29%. Both InlaySQL
p50s moved down from the previous edition's 78.96 µs and 100.29 µs — 12% and
8% — and both moves are larger than the rows' own spreads this time (10% and
2%), but the realistic-corpus p50 was itself in this run's ≥10% list, and
the previous edition's range (74.42–82.54 µs) does not overlap this one's
(65.83–72.79 µs) by only 1.6 µs; read it as a probable improvement that
nothing in `2cb2539..7b20175` was measured to explain (no commit in that
range touches `hnsw.rs` or the distance kernels), not as a claimed one.
`sqlite-vec`'s own p50 barely moved (668.08 → 664.79 µs realistic), which is
what makes the ratio's move — 8.98x → 9.56x — mostly ours.

Both corpus shapes are published because only one of them flatters us. Uniform
random vectors in 384 dimensions have no structure for a graph index to
navigate, so recall falls and no amount of tuning fixes it. Text-derived
embeddings are what an application actually stores.

`VECTOR(n, INT8)` quantisation costs 0.014 recall on the realistic corpus
(0.986 vs 1.000 exact) and nothing measurable on the random one (0.922 both),
for a 1.65x smaller file and a 3.96x smaller resident payload — all four
figures identical across all three runs and identical to the previous
edition's. Its per-query cost at this scale is 154.71 µs (154.25–155.50 µs)
realistic and 241.79 µs (240.42–242.29 µs) uniform, roughly 2.2x and 2.6x
the exact index's p50 (the realistic-corpus multiple read 2.10x last
edition; it widened because exact's p50 fell, above, and both int8 rows are
tight to within 1% here).

**Spot-checked at scale, `SUITE=quantization DOCS=100000 QUERIES=50`, median
of three runs (`bench/results/20260830T{125800,131326,132715}Z.txt`, load
2.3–4.8/18 throughout) — carried forward from the 2026-08-30 edition at
`2cb2539`, not regenerated this edition; the 2026-09-02 sitting ran only the
default 2,000-document suite above.** Recall loss widens to 0.028 (realistic, 0.970 vs
0.998) and 0.014 (uniform, 0.104 vs 0.118) — both figures exact and identical
across all three runs (0% spread), a real and fully reproducible finding, not
subject to this section's usual hedging.

The per-query slowdown this document and `PERF.md` diagnosed as structural at
2,000 docs (int8 2.10x slower at that edition; ~2.2x in the fresh table
above) is gone at 100,000 docs on both corpora — but
**"gone" means "within this table's own noise of parity," not "reversed to a
reliable win," and an earlier draft of this paragraph overstated the second
half of that.** Paired per run rather than median-against-median: realistic-
corpus ratios (int8 p50 ÷ exact p50) were 0.97x, 1.04x and 1.08x across the
three runs — one run reads int8 faster, two read it slower. Uniform-corpus
ratios were 0.97x, 0.90x and 1.05x — two runs read int8 faster, one reads it
slower. **Both corpora's per-query cost at 100,000 docs sits within noise of
parity, with the direction itself flipping run to run, not a settled "int8 is
now N% faster."** An earlier version of this paragraph quoted one specific
cross-run pairing (run three's int8 figure against the *overall median*
exact figure, which happens to equal run one's own exact value) as "int8 4%
faster, consistent across all three runs" — that specific consistency claim
does not survive pairing the runs correctly and is corrected here rather than
repeated. What does hold, correctly paired: the 2,000-doc structural
slowdown is real at that scale and is not present at 100,000-doc scale on
either corpus — a genuine, scale-dependent finding, just not the tidy
"reverses to 4% faster" the previous draft claimed.

Build time, by contrast, is not noisy here and does reverse from what
`bench/README.md`'s own dedicated-command table has published: exact now
builds *faster* than int8 (95.23 s vs 140.09 s, realistic; 138.03 s vs
201.13 s, uniform — each individually consistent within 2.8-3.5% across all
three runs), where that table's 252.49 s/121.31 s reads the opposite way.
`bench/README.md`'s table is not regenerated by this edition — it is a
different document with its own publishing rule — so this paragraph
discloses the discrepancy rather than resolving it: it was not root-caused
here, and the true story is more likely "the per-query slowdown depends on
corpus scale in a way the small-corpus figure does not capture" than "the
small-corpus figure was wrong".

## Against an external benchmark — `ann-benchmarks`, glove-25-angular

Every other table on this page uses our corpus, our oracle and our harness.
This one uses none of them.
[`ann-benchmarks`](https://github.com/erikbern/ann-benchmarks) publishes fixed
datasets with **precomputed ground-truth neighbours** and one recall/QPS
protocol that every engine on its leaderboard is measured by;
`bench/ann/module.py` is InlaySQL's plugin for it, reaching the engine over
`inlaysql serve --mysql` with ordinary SQL, and `bench/ann/run.py` runs the
protocol without Docker. Methodology, the seam and every limitation the
exercise exposed are in
[`bench/README.md`](bench/README.md#ann-benchmarks--an-external-corpus-an-external-ground-truth-an-external-protocol).

```sh
bench/ann/.venv/bin/python bench/ann/run.py --dataset glove-25-angular
```

| | |
| --- | --- |
| Commit | `4d9f535` (tree dirty: the adapter was uncommitted when it ran) |
| Date | 2026-08-26 |
| Machine | Apple Mac17,9, macOS 27.0 (Darwin 27.0.0 arm64), rustc 1.91.1 |
| Dataset | `glove-25-angular` — 1,183,514 x dim 25, 10,000 queries, k = 10 |
| Recall against | the dataset's own `distances` array. **Not our oracle.** |

This table follows `ann-benchmarks`' own convention (best of 3 runs per
point, not this document's usual median-plus-range) because it is meant to
be checked against that leaderboard's own numbers; it is a single session on
this machine, with no independent measurement of its own floor, so read every
multiple below as loosely as the single-run tables above, not more precisely
for coming from an external corpus.

Exact `VECTOR(25)`, three runs, `QPS = 1 / best_search_time`:

| over-fetch | effective `ef` | recall@10 | QPS | p50 |
| --- | --- | --- | --- | --- |
| 1 | 64 | 0.9878 | 3,021 | 0.331 ms |
| 4 | 80 | 0.9996 | 1,179 | 0.850 ms |
| 8 | 160 | **1.0000** | 653 | 1.534 ms |
| 64 | 1,280 | 1.0000 | 104 | 9.647 ms |

Build: 294.9 s — 36.2 s loading over the wire and **258.7 s building the graph
on the first read**, which is the single largest cost in this table. It used to
be unaskable-for as well as slow: a user hit it as an unexplained multi-minute
stall on whichever query happened to be first, with no statement that could
have moved it. That half is closed — `OPTIMIZE TABLE t` over the wire,
`REINDEX` in SQL, `Database::reindex` embedded, all cancellable and all a no-op
when nothing is pending
([`docs/indexes.md`](docs/indexes.md#the-build-is-deferred-and-you-can-ask-for-it)).
The 258.7 s is unchanged; it is now a statement the loader runs on purpose
rather than a stall. Index: 1,047 MiB for a 112.9 MiB corpus (9.3x).

`VECTOR(25, INT8)` on the same data builds in 461.3 s (1.56x slower), holds
790 MiB (1.34x smaller), and **tops out at recall 0.9982** — a quantisation
floor no amount of over-fetching recovers, where exact reaches 1.0000. It is
also ~7% slower per query at `over_fetch = 1`. The smaller-memory half of that
trade is real; there is no faster half.

**Not comparable in absolute terms to the QPS figures on ann-benchmarks.com**,
which are run on a fixed cloud instance type. The curve is comparable, and it
is the first number on this page that somebody who has never seen this
repository can check.

Three things the adapter could not do, and they are engine limitations rather
than harness ones: ~~`vector_score` is cosine-only, so `sift-128-euclidean` and
`fashion-mnist-784-euclidean` cannot be answered at all~~ — **closed.** A
vector index now carries the distance it was built under, written at
`CREATE INDEX` with pgvector's operator-class spelling
(`CREATE INDEX ... ON items USING hnsw (embedding vector_l2_ops)`), and the
adapter maps the dataset's own metric onto it, so both `-euclidean` starters
run. The numbers in the table above are `glove-25-angular` and predate that
change; they are unaffected by it, because a cosine index writes and scores
exactly the bytes and the arithmetic it always did. `vector_ip_ops` is
deliberately absent — inner product is not a metric, and the reasoning is in
`crates/inlaysql-core/src/hnsw.rs`. The sweep above is an over-fetched `LIMIT`
and predates `SET inlaysql_hnsw_ef_search`, which the adapter now sweeps
instead — the same dial pgvector's own plugin sweeps as `SET hnsw.ef_search`;
`m` and `ef_construction` are still Rust-only. An embedding **can** now be bound
as a parameter over the wire (AHL-478) — `dim` little-endian `f32`s in a string
parameter, MySQL 9's own `VECTOR` storage format — where every one used to cross
as decimal text. Measured on `glove-25-angular`, that took the load from 363.9
MiB on the wire for a 112.9 MiB corpus (3.22x) to 127.9 MiB (1.13x) and from
41.20 s to 19.77 s, counted by the server's own `Bytes_received`. Recall is
untouched: the same `f32`s reach the index either way, and only their spelling
on the wire changed. See `bench/README.md` and `docs/server.md`.

## Retrieval — BM25 is no longer the expensive half

2,000 documents, dim 384, `LIMIT 10`. Ingest 13,930 docs/s (median of three
runs, 13,159–14,319; same session as the tables above —
`bench/results/20260902T{022325,023047,023804}Z.txt`).

| Workload | p50 (median, range) | p95 (median) | Previous edition (`9aba437`) |
| --- | --- | --- | --- |
| Vector only | 68.42 µs (67.88–78.46 µs) | 116.58 µs | 87.88 µs |
| BM25 only | **46.67 µs** (46.13–46.79 µs) | 62.21 µs | 347.50 µs |
| Hybrid (fused) | **95.54 µs** (94.88–95.71 µs) | 115.25 µs | 453.88 µs |

Hybrid is **one SQL statement**, not two queries and a client-side merge.

BM25 fell **roughly 7.4-7.5x** and hybrid **roughly 4.7-4.8x** against that
historical baseline (this session's own three runs give those ranges, and
they are narrow: the BM25 and hybrid p50s held within 1.4% and 0.9% across
the three runs, the two tightest latency rows on this page). Against the
2026-08-30 edition's 51.21 µs / 102.46 µs both are 7-9% lower, a move that
sits inside that edition's own ranges (47.33–51.25 µs, 95.21–112.21 µs) —
no code has touched `bm25.rs` since, so read it as this benchmark's usual
noise on a ratio against a fixed old number, not an improvement. The vector
leg is the noisy one here (p50 spread 15%, the only retrieval figure in this
run's ≥10% list). The underlying rewrite is still code, from the
same earlier edition as before: the full-text
index stopped being a map of maps and became an ordinary inverted index with
dense document ordinals, a bounded top-`k` heap instead of scoring and sorting
the whole corpus to keep ten rows, and a MaxScore walk that stops visiting
documents whose entire possible score cannot reach the `k`-th best already
found. Measured directly on the index over the same seed, before → after:

| Corpus | BM25 p50 | Hybrid p50 |
| --- | --- | --- |
| 2,000 docs | 319.79 µs → 51.75 µs | 374.21 µs → 104.29 µs |
| 5,000 docs | 802.92 µs → 105.67 µs | 880.29 µs → 166.50 µs |

The scores are unchanged bit for bit and the ranking is unchanged including
ties — skipping applies only to documents whose whole possible score is
*strictly* below the `k`-th best, because a document that merely equalled it
could still win the tie on the lower row id. `crates/inlaysql-core/src/bm25.rs`
carries the argument and the tests that pin it.

BM25 was 79% of the hybrid p50 before this; it is now 50%, and the vector leg
is the larger half. Per-block impact bounds (block-max WAND) are the next step
and are not implemented.

---

## Against DuckDB, pgvector and Meilisearch

One corpus, one set of queries, one exhaustive ground truth, each engine asked
for its own query plan so an unindexed row cannot masquerade as an indexed one.
5,000 documents, dim 128. Regenerated 2026-08-30 via `./bench/compare.sh`,
single run — no repeat wrapper existed for this table when it was measured;
one does now (`bench/repeat-compare.sh`, 2026-08-31), and this table has not
been re-measured with it — host load 1.1–1.9/18 throughout, four unrelated
Docker containers present and idle for the session's whole duration
(`hkjc-citywide-redis`, `hkjc-citywide-db`, `linkmonitor-app-1`,
`estate-ops-postgres` — none touched during the run). Raw output:
`bench/results/20260830T134642Z-compare.txt`.

| Engine | recall@10 | vector p50 | hybrid p50 |
| --- | --- | --- | --- |
| **InlaySQL** (HNSW + BM25) | 1.000 | **135.00 µs** | **198.00 µs** |
| DuckDB (exhaustive + fts BM25) | 0.999 | 4.91 ms | 11.96 ms |
| DuckDB (vss HNSW + fts BM25) | 0.992 | 3.98 ms | 11.12 ms |
| Meilisearch (`arroy` ANN + its own ranking) | 0.996 | 1.17 ms | 3.97 ms |
| pgvector (HNSW + `ts_rank`) | 0.987 | 147.00 µs | 13.40 ms |
| pgvector (exhaustive + `ts_rank`) | 0.999 | 482.00 µs | 13.60 ms |

**Hybrid is roughly 20x** the nearest baseline (3.97 ms, Meilisearch,
essentially unchanged from the previous edition's ~20x) and **roughly
55-70x** DuckDB/pgvector (was ~60-70x). This table is a single run, measured
before a repeat wrapper for it existed (see the tooling correction at the top
of this file) — it has no
internal spread to measure at all, which makes it *less* trustworthy to two
significant figures than the gated `run.sh` tables above, not more; read the
edition-to-edition move here as unmeasured rather than as a real narrowing.
It is still not one query against one query — it is one statement here
against two queries plus client-side rank fusion there, Meilisearch
included — and `bench/README.md` says so plainly.

**Vector-only stays a win against pgvector, and Meilisearch is the fastest
baseline recall-for-recall over a network.** 135 µs against pgvector's
147 µs (both include pgvector's socket round trip a library in your own
process does not pay, so read it as close rather than as a rout — the margin
narrowed from the previous edition's 126 µs/152 µs, still a single-run
comparison on both sides) and against Meilisearch's 1.17 ms — not a fair
fight in InlaySQL's favour so much as a different product: Meilisearch's ANN
search also runs its own typo-tolerance and ranking pipeline, which
pgvector's raw `<=>` operator does not. Meilisearch's `agree` (0.419) sits in
the same range as pgvector's `ts_rank_cd` rows (0.456/0.465) for the same
reason both are below DuckDB's real BM25: neither ranks text with BM25 at
all.

---

## Against MySQL and PostgreSQL

**Tuning asymmetry, found auditing this section for `SCOREBOARD.md`
(2026-08-31) and fixed the same day:** `compose.yml`'s `postgres` service
runs `shared_buffers=512MB`, roughly 4x PostgreSQL's own stock default; the
`mysql` service used to get no equivalent bump to
`innodb_buffer_pool_size`, which sat at MySQL 8's stock 128MB. `mysql`'s
command now also carries `--innodb-buffer-pool-size=512M` — the same value,
and the same multiple of stock, as `postgres`'s `shared_buffers`; durability
(`innodb_flush_log_at_trx_commit=1`) is untouched. Likely inert for the
single-row-commit numbers in this section — this workload's 20,000 short
rows fit either engine's *stock* cache, and the commit path is
`fsync`-dominated (88-97% of commit time) regardless, consistent with the
concurrent-commits numbers below moving in ways fully explained by
connection count and container noise, not a sudden buffer-pool effect — but
it was a real inconsistency a reviewer would have flagged, and matters going
forward for any range-scan/join/aggregate row against these servers, where a
bigger working set could make the comparison about the tuning choice rather
than the engine. See `bench/README.md`'s "Tuning" subsection (below "The
structural asymmetry that cannot be removed") for the full note, and
`SCOREBOARD.md` §4.3 for the audit this was found during.

**Carried forward from `b4798ce` (2026-08-30), not regenerated this
edition.** This whole section — the table immediately below, its
correction, and the interleaved rerun further down — predates this edition's
HEAD (`7b20175`) by the whole `b4798ce..7b20175` range; `compare.sh` was
not run in the 2026-09-02 sitting, so these are still the same single-run
figures from their own stated dates. Of the commits in that range, the
read-path work (AHL-512 through AHL-522: join reorder, aggregate streaming,
the allocation diet, the page-cache hash, the read-ahead window) touches
scans, joins and aggregates, not the single-row durable commit this
section's write column measures, and its point-read column is the same path
the point-reads section above found flat. The interleaved, repeated, quiet-machine rerun below
is reused rather than redone: it was already done properly (5 interleaved
repetitions against a `pwrite`+`fsync` floor control, load-gated), and a
single fresh sequential run would be a *worse* measurement of the same
comparison, not a better one. The plain sequential table immediately below is
carried forward with it rather than replaced by a fresh single run, because
this section's own "Correction" already establishes that a sequential,
non-interleaved run of this comparison is the less trustworthy measurement —
regenerating a new one here would add a fourth, equally-suspect data point to
a section whose entire point is that this method is unreliable, not a fix.
The "Server-to-server" subsection at the end is likewise carried forward, from
`f8e29e9` (2026-08-29) — see that subsection for its own date and reasoning.

**Reads: we win by a wide margin. Sequential writes: we lose to both, and by
more than in the previous edition.**

InlaySQL is measured twice — on the host with a real `F_FULLFSYNC` barrier,
and **inside a container on the same volume class as the servers**, so all
three pay the same virtualised fsync. The gap between the two InlaySQL rows is
what that virtualisation is worth on this machine.

| Engine | write ops/s | read ops/s |
| --- | --- | --- |
| InlaySQL, host (real `F_FULLFSYNC`) | 253.2 | 497k |
| InlaySQL, containerised | 849.7 | **678k** |
| MySQL 8 (`innodb_flush_log_at_trx_commit=1`, binlog off) | 1,184.2 | 9.2k |
| PostgreSQL 17 (`fsync=on`, `synchronous_commit=on`) | **1,612.8** | 19.4k |

**Reads: ~74x MySQL and ~35x PostgreSQL**, containerised — an in-process
library against a socket round trip. This table is a single run with no
measured spread of its own, but an effect this size (tens-of-x, not a few
percent) is not the kind of thing this document's measurement floor could
plausibly manufacture — stated plainly for that reason. That asymmetry is
structural and stated, not hidden.

**Writes: we lose to both.** PostgreSQL is 1.90x faster than the containerised
row (1,612.8 against 849.7) and MySQL 1.39x (1,184.2). Which of the two servers
leads has now flipped between editions while our own figure barely moved, so
read the ordering as noise and the fact that we trail both as the finding.
**This single run should not be read on its own — see "Interleaved, repeated,
quiet-machine rerun" below the correction that follows this section**: a
same-session sequential rerun found the ordering flipped and the multiple
shrunk by about a third, but a properly interleaved, repeated, load-gated
rerun found the opposite — PostgreSQL ahead of MySQL in 5 of 5 repetitions,
median multiples of 1.81x/1.43x, close to this table's own 1.90x/1.39x. Read
this table's ordering and multiple as closer to right than the sequential
check below suggested, not as replaced by it. The previous edition had
us beating PostgreSQL and within 1.08x of MySQL; our own figure improved
(723.1 → 847.2) and both servers improved more, on a run where they had eleven
unrelated containers for company and we did not control for it. So the ranking
here is real for this run and the size of the gap is not to be trusted. What is
structural, and unchanged: this workload is one commit at a time on one
connection, so group commit cannot fire by design, and the remaining cost is
per-commit against InnoDB's redo write. Closing it is scheduled work. The
concurrent-writer story (1148 commits/s on 8 writers above, regenerated this
edition — see "Concurrent writers" above) has no
MySQL/PostgreSQL counterpart on this page yet — a server-to-server concurrent
row is the missing apples-to-apples.

### Correction (2026-08-30): this table is not apples-to-apples, and the asymmetry favours InlaySQL

The Reads line above already says the in-process-vs-socket asymmetry is
structural. It undersold it. Read as written, "we lose to both" on writes
reads like an engine-side finding against a matched comparison. It is not a
matched comparison, and a same-engine control shows the write row above is
already skipping roughly as much transport cost as the entire published gap
to PostgreSQL.

**The transport tax, measured with InlaySQL against itself.** The
containerised row above is a library call: `bench/external/compose.yml`'s
`inlaysql-oltp` service runs `cargo run -p inlaysql-bench --
--oltp-replay ...`, in-process, no socket. `mysql_driver.py` and
`postgres_oltp_driver.py` (below MySQL's and PostgreSQL's rows) reach their
servers with `mysql.connector`/`psycopg` over the compose bridge network — a
socket round trip on *every* statement. `inlaysql serve --mysql` at one
connection (the "Server-to-server" table below) writes at 556.7 ops/s —
1,795.6 µs/commit — over the identical wire protocol MySQL's own row pays.
The containerised library row writes at 1,177.0 µs/commit (849.7 ops/s,
published) or 1,369.3 µs/commit (730.4 ops/s, a same-session rerun today).
That is **~420-620 µs of transport/driver tax that InlaySQL's containerised
row skips and both MySQL and PostgreSQL pay on every statement** — the same
order of magnitude as the entire published PostgreSQL gap (620 µs). A reader
should come away understanding that this row flatters InlaySQL, and that a
transport-matched comparison would very likely reverse part of this gap, not
just narrow it. **That prediction is so far unconfirmed.** The one
transport-matched run that exists (see "Transport-matched, single run"
under the interleaved rerun below) points the other way — treat the
predicted direction as open, not settled, until a repeated, interleaved
transport-matched run checks it.

**The numbers are noisy run-to-run — but this sequential rerun's own
apparent ordering flip and shrunk multiple turned out to be the artifact,
not the finding.** A fresh, same-session rerun of this table's own drivers
today (`ROWS=3000 LOOKUPS=1000 ./bench/compare.sh`, host load ~6.2/18 — not
quiet, disclosed rather than hidden, and part of why this specific rerun
turned out unreliable): InlaySQL host 240.9 ops/s, InlaySQL containerised
730.4, MySQL 931.2 (**1.27x**), PostgreSQL 805.0 (**1.10x**) — against the
published 849.7 / 1,184.2 (1.39x) / 1,612.8 (1.90x). Read at face value, this
run says PostgreSQL fell behind MySQL and the multiple against both shrank
by about a third. **It does not hold up.** The "Interleaved, repeated,
quiet-machine rerun" section below repeats this same comparison five times
on a quiet, load-gated machine and finds PostgreSQL ahead of MySQL in 5 of 5
repetitions, with median multiples (1.81x/1.43x) close to the published
table's own (1.90x/1.39x) — not to this sequential check's shrunken ones.
This same-session sequential rerun was itself the unreliable measurement;
see that section for the full data before drawing a conclusion from this
one. What genuinely does stand from this rerun: the numbers ARE noisy
run-to-run (the interleaved section below measures engine spreads of
50-81% across five repetitions) and the volume's `fsync` cost really does
drift within a session. Root cause, measured directly rather than inferred:
the Docker named volume's own `fsync` cost drifted 1.5-1.8x within the same
session — roughly 1,150 µs before the MySQL/PostgreSQL containers were up,
640-800 µs ten minutes later with them running. This reproduces `PERF.md`'s
AHL-496 finding of a 2.1x drift 90 minutes apart, at a shorter timescale and
inside one benchmark run.

**And it is not our CPU path.** A hypothesis was tested and rejected: that
in-container, where the barrier is weaker than the host's `F_FULLFSYNC`,
hidden per-commit CPU cost would surface as a real, engine-side cause of the
gap. Measured directly (`PERF.md`'s 2026-08-30 section has the method and
both runs), `fsync` is **87.8-89.1%** of a containerised commit — the same
barrier-dominated shape as the host's 97.1%, just a smaller absolute
barrier. InlaySQL's own non-`fsync` work is only ~11-12% of commit time, so
even a zero-cost commit path caps the achievable win at roughly 1.15x —
nowhere near the published 1.39-1.90x gap. **There is no engine-side fix for
this workload's gap.** It lives in the volume's barrier and its drift, and
in the transport asymmetry above, not in this engine's code. The honest next
step is a methodology fix — interleaved, repeated, quiet-machine runs
publishing median and spread (`bench/README.md`'s "How many times to run
it") — not more profiling of the commit path.

### Interleaved, repeated, quiet-machine rerun (2026-08-30): AHL-496's "what is owed" item, paid

The correction above named the fix and did not do it: this section is that
rerun. One repetition is InlaySQL (containerised), MySQL and PostgreSQL, run
back to back against the same warm containers, immediately followed by a
control — a raw `pwrite`(80 KiB)+`fsync` loop (5 warm-up + 25 timed reps) on
`inlaysql-bench-floor-data`, a named Docker volume of the same `local`-driver
class as `postgres-oltp-data`/`mysql-oltp-data`/`inlaysql-oltp-data` but
written by nothing except the probe — so every repetition carries its own
reading of what the volume's barrier cost at that moment, the instrument
`PERF.md`'s AHL-496 section used to explain the original drift. The cycle
repeated 5 times. `ROWS=20000 LOOKUPS=5000` — unchanged from the published
table above.

**Load, disclosed.** `bench/compare.sh` had no load gate when this ran (it has
one since 2026-08-31), so the gate was manual, matching `bench/run.sh`'s
`BENCH_MAX_LOAD_PER_CPU=0.25` rule (18 logical CPUs → keep the 1-minute
average well under ~4.5): checked before every repetition, waiting and
rechecking rather than running and caveating. Two repetitions were caught
and discarded mid-run by processes with nothing to do with this benchmark —
an unrelated Xcode-beta build (dozens of parallel `clang` processes) spiked
the 1-minute average past 150, and later an unrelated codesigning/indexing
burst spiked it past 80 — and redone once the host settled back under 4. The
5 repetitions published below all started at a 1-minute load between 2.0 and
4.0 and stayed there throughout; the raw file
(`bench/results/20260830T095714Z-interleaved-oltp-compare.txt`) carries the
`uptime` reading before and after every repetition, including the two
discarded ones, rather than only the ones that looked clean.

**Write ops/s, median and spread (min-max) over the 5 repetitions:**

| Series | median | min | max | spread |
| --- | ---: | ---: | ---: | ---: |
| `pwrite`+`fsync` floor (control) | 985.2 | 854.0 | 1,005.6 | 15.4% |
| InlaySQL, containerised | 698.9 | 557.0 | 909.5 | 50.4% |
| MySQL 8 | 1,002.3 | 722.9 | 1,535.9 | 81.1% |
| PostgreSQL 17 | 1,265.7 | 954.2 | 1,621.5 | 52.7% |

**The honest multiple: PostgreSQL 1.81x, MySQL 1.43x** (median against
median) — close to the published 1.90x/1.39x, not the shrunken 1.10x/1.27x
the single sequential rerun above found. Read that as the sequential rerun
having been the noisy measurement, not this one: the published table's
multiple was closer to right than its own same-session sequential check
suggested.

**The MySQL/PostgreSQL ordering is stable, not flipped.** PostgreSQL beat
MySQL in **5 of 5 repetitions** (ratio 1.06-1.32x each time — see the raw
file for all five). The single sequential rerun's flip (PostgreSQL behind
MySQL) does not reproduce under interleaving; it looks like exactly the kind
of sequential-measurement artifact this rerun exists to catch, not a second
data point about which server is really faster.

**The floor does not explain most of this run's variance, which is itself a
finding.** The control's own spread (15.4%) is far smaller than any engine's
(50-81%), and the correlation between the floor and each engine across the 5
repetitions is weak: MySQL +0.51, PostgreSQL +0.46, InlaySQL **-0.51**
(Pearson r, n=5 — small enough to read the sign more than the precision).
On an already-warm, already-quiet stack, the raw fsync floor stayed close to
one value; the engines still swung 50-81%. That does not contradict the
1.5-2.1x floor drift `PERF.md`'s AHL-496 section and the correction above
both measured — those were captured while containers were cold-starting or
the host was still settling, exactly the conditions this rerun's load gate
and container-warm-up were designed to avoid. It does mean that once that
condition is controlled for, most of the remaining run-to-run noise in this
table is not the storage volume — it is more likely the Python driver/
connector overhead, `docker exec`/process-spawn jitter, or the compose
bridge network, none of which this rerun isolated further.

**This does not make the comparison fair, only quieter.** The transport
asymmetry the correction above quantifies (~420-620 µs/commit, the same
order as the entire published PostgreSQL gap) is untouched by any of this —
interleaving and repetition remove noise, not the library-vs-socket
asymmetry. If anything, a cleaner multiple that lands close to the original
1.39-1.90x is a *more* confident restatement of a comparison that already
favours InlaySQL by construction, not evidence that the engine got faster.

**Transport-matched, single run (cheap, not interleaved or repeated).**
`bench/external/server_driver.py` already exists (AHL-489, "Server-to-server"
below) and reaches both `inlaysql serve --mysql` and MySQL with the same
`mysql.connector` client, so it was run once more alongside this rerun
(`SERVER_ROWS=2000 SERVER_LOOKUPS=1000`, load ~3.8/18, disclosed, single run,
not median-of-N): at one connection, InlaySQL wrote 627.6 ops/s against
MySQL's 849.4 — **0.74x**, meaning MySQL is ~1.35x faster over this matched
transport (849.4 / 627.6), against the 1.43x MySQL leads by in the
containerised library row above (1,002.3 / 698.9, median). **That is not the
direction the correction above predicts — it points the other way.** The
correction predicted that a fair, transport-matched comparison would very
likely show InlaySQL doing *worse*, not better, once both engines pay the
same socket round trip; here InlaySQL's loss shrank (1.43x → 1.35x) instead
of growing. This single run does not confirm that prediction. It is also
weak evidence on its own: one run, not a repeated median, on a workload the
interleaved section above measured swinging 50-81% run to run — nowhere near
enough to overturn the correction by itself. The structural claim underneath
the correction — the containerised row is an in-process library call while
MySQL/PostgreSQL pay a socket round trip per statement, quantified at
~420-620 µs by reading the drivers — is unaffected and stands regardless;
what is now in question is only its predicted *net effect on the multiple*,
which needs a repeated, interleaved, transport-matched run to settle — see
"Server-to-server" below for the fuller, disclosed-load (also single-run,
also not repeated) edition of this same comparison, including concurrency
levels this rerun did not touch.

**Raw data.** `bench/results/20260830T095714Z-interleaved-oltp-compare.txt`
(the full session: header, both discarded attempts with their `uptime`
readings and the reason each was thrown out, all 5 published repetitions,
the summary table above, and the transport-matched bonus run) and, per
repetition, `bench/results/20260830T095714Z-rep{1..5}-{inlaysql-container,
mysql,postgres}.json` — the exact files each driver/replay wrote, copied out
before the next repetition overwrote them. `bench/results/` is git-ignored
per this repo's convention; the filenames above are cited so the run is
traceable even though the files themselves are not committed.

**Recommended, not implemented.** This session's own experience argues for
it: two of the seven repetition attempts above were caught only because a
human was watching `uptime` between phases, and a load gate would have
refused both automatically instead. `bench/run.sh`'s check has since grown
past a single pre-flight gate: it now samples load throughout a run, not
just before it starts, and marks a result `CONTAMINATED` (loudly, in the
result file and in `bench/summarise.py`'s combined report) rather than
trusting one reading taken before anything ran — see `bench/README.md`. Both
halves — the original pre-flight `awk`/`uptime`/`sysctl` block and the
newer throughout-the-run sampler — read cleanly onto `compare.sh`'s own
preamble, before the corpus is generated. Neither was ported here because
`compare.sh` is also run from `trust.yml`'s `benchmarks` job in CI
(`ubuntu-latest`, shared runners, 2-4 vCPUs), where `run.sh`'s identical
pre-flight gate is already a known, tolerated flake (a `uptime`-format/
CPU-count mismatch or a genuinely busy shared runner exits 3) — verifying
that adding the same behaviour to `compare.sh` would not turn an
already-accepted single flake into two, or interact with the job's
Docker-availability fallback, needs someone to actually watch a CI run do
it, not a static read of the workflow. That verification did not happen in
this session, so the port stays a recommendation.

### Server-to-server: MySQL wire protocol

`inlaysql serve --mysql` reached over the compose network by `mysql.connector`,
matched against MySQL 8, same driver and same transport on both sides. Every
row pays a socket round trip.

**Regenerated 2026-08-29 with the process-based driver** (`f8e29e9`, built
2026-08-27, run for the first time here): each connection is a spawned OS
process, not a Python thread, so `mysql.connector`'s GIL — confirmed below to
have contaminated every earlier edition of this table — cannot be in this
run's numbers. Checked quiet beforehand (host load ~3/18 logical CPUs);
`bench/compare.sh` had no automated load gate the way `bench/run.sh` does when
this ran, so this is a disclosed manual check, not an enforced one; the gate
landed 2026-08-31 and would enforce it on a re-measurement.

| Engine | Connections | write ops/s | read ops/s |
| --- | --- | --- | --- |
| **InlaySQL** (`inlaysql serve --mysql`) | 1 | 556.7 | **9,033.3** |
| **InlaySQL** (`inlaysql serve --mysql`) | 8 | 1,255.5 | 6,294.3 |
| MySQL 8 | 1 | 787.7 | 7,400.6 |
| MySQL 8 | 8 | 3,092.7 | 7,931.1 |

This table is a single run with no repeated-run spread of its own to check
these ratios against; the 0.71-1.22x figures below sit close enough to parity
that a repeat could plausibly move which side leads, and should be read that
way. The larger drops (eight-connection writes at 0.41x, and the within-engine
30% absolute fall named below) are big enough relative to anything this
document's measured floors would predict from noise alone that they are kept
as findings, not folded into the same caveat.

At one connection InlaySQL writes 0.71x of MySQL and reads 1.22x. At eight
connections writes are 0.41x (MySQL's own throughput nearly quadruples,
787.7 → 3,092.7; InlaySQL's writes scale to only 2.25x, 556.7 → 1,255.5) and
reads are 0.79x — and unlike every earlier edition of this row, **that read
number is not a GIL artifact**: MySQL's own read throughput is flat across
the same 1-to-8 step (7,400.6 → 7,931.1), while InlaySQL's *falls* in
absolute terms (9,033.3 → 6,294.3, a real 30% drop, process-isolated driver
on both sides). Retries are zero on both engines at both concurrency levels,
so this is not lock contention between disjoint id ranges.

**This retires the open question the last three editions of this table
carried**, not by finding a fix but by finding out the diagnosis was
incomplete rather than wrong: the correction below (still accurate for what
it found) showed the *old*, larger read drop was substantially a threaded
Python client's GIL, not the server. It did not claim the drop would
vanish entirely once that confound was removed, and it has not — a smaller,
real one remains, process isolation and all. `inlaysql-server`'s
thread-per-connection model (`docs/server.md`'s D2) is the standing
architectural difference from MySQL's worker pool named below and was the
obvious next suspect — **checked separately, immediately after this run,
and it did not hold up either, twice**: the same driver against the same
server running directly on the host (no Docker) scales *up* with
concurrency at every workload size tried, profiled at 81.4% idle in
`recvfrom`; then isolated *inside* the compose network too —
`inlaysql-server` alone, and again with the full five-container stack
present but idle — and both still scale up cleanly. What does reproduce the
drop: running `mysql_driver.py` for real immediately before
`server_driver.py`, in that same idle stack, which is exactly `compare.sh`'s
own phase order. See "What is not measured here" below for the numbers.
`inlaysql-server`'s own concurrency handling is now the *less* likely place
this drop lives — two independent checks, on two different hosts (loopback
and compose network), say it has spare capacity at this concurrency — and a
preceding driver phase's burst of activity is the more likely one, though
the exact mechanism (MySQL's own background catch-up, `drivers`-container
state carried across phases, a Docker Desktop VM scheduler effect) is not
pinned down. Absolute
throughput here is well below every other edition of this table on both
engines — a busier host or a container resource change since the last
`compare.sh` run, not a regression in either database; only the relative,
same-run comparison is meaningful.

**History, kept because the reasoning is why the driver was rebuilt, not
because the numbers still stand.** Two editions ago this table showed a
1-to-8-connection read drop (26,270 → 17,628 reads/s) and named it as
evidence that eight connections warm eight per-handle page caches over the
same pages — a server-side diagnosis. A later investigation tested that
claim rather than trusting it and it did not hold: rebuilding the read phase
on a quiet machine with the same client, same driver, same shape did not
reproduce the aggregate drop, but running the client's concurrency as
*threads* instead of *processes* did, independently, twice — including on
MySQL's own row, which lost 31% of its reads across the identical two steps
purely from the client's GIL, while the server (sampled mid-run) sat idle in
`recvfrom`. A shared raw-page cache across connections was built anyway
during that investigation (`docs/server.md`'s D2) — real, worth roughly 18%
on the page-miss path — but it could not have been the fix even in
principle, because this benchmark's per-handle cache already holds the whole
working set warm before the shared one is ever asked. The conclusion that
investigation reached — *the numbers were real, the explanation attached to
them was not tested, and a process-based re-run was the honest next step* —
is exactly what the table and the numbered paragraph above this one now are.
See "Server-to-server" in `bench/README.md` for the detailed methodology,
the concurrency-model/credential/TLS asymmetries that remain, and why
PostgreSQL has no row here.

What this still cannot prove: Docker Desktop's virtual disk was never
independently verified to honour `fsync` as a barrier for *any* of the three
engines. "Comparable" is not "hardware-durable".

### Server-to-server, extended: 1/4/16 connections, repeated and interleaved, with commits-per-fsync (2026-08-31)

The table above was a single, unrepeated run at 1 and 8 connections.
`server_driver.py` already supported `SERVER_CONCURRENCY_LEVELS=1,4,16` —
it had simply never been run and published at those levels, and never
repeated. This section is that sweep, run properly: 5 repetitions,
**interleaved by concurrency level rather than target-major** (MySQL then
InlaySQL at 1 connection, then MySQL then InlaySQL at 4, then at 16 — the
ordering this document's own "Interleaved, repeated, quiet-machine rerun"
section above identifies as the fix for this project's worst past
measurement error, now applied to the server-to-server driver too;
`server_driver.py`'s `main()` was restructured to loop concurrency levels
outer and targets inner rather than the reverse). Load-gated manually before
every repetition (`bench/compare.sh` still has no automated gate — 1-minute
average 2.1–3.3 of this 18-logical-CPU box's 4.5 ceiling throughout, quiet
by this document's own standard).

**Also fixed first: the MySQL/PostgreSQL tuning asymmetry** named above —
`mysql`'s container now runs `--innodb-buffer-pool-size=512M`, matching
`postgres`'s `shared_buffers=512MB` — before any of the numbers below were
taken, per the task this session answers.

**Write throughput, median of 5 and the full range:**

| Connections | InlaySQL write ops/s | MySQL write ops/s | Ratio |
| --- | ---: | ---: | ---: |
| 1 | 638.7 (590.1–934.6) | 1,363.1 (674.5–1,510.3) | MySQL ahead, ~1.1-2.4x per rep |
| 4 | 1,075.0 (1,000.5–1,098.5) | 1,512.7 (1,181.2–3,161.8) | MySQL ahead, ~1.1-3.0x per rep |
| 16 | 1,308.1 (1,242.4–1,377.3) | 6,120.7 (3,824.2–7,356.9) | MySQL ahead, ~3.1-5.4x per rep |

**MySQL wins every repetition at every level (5 of 5 at all three), and the
loss widens with concurrency — the opposite of the in-process SQLite row's
shape, and worse than the old 1/8-connection table found.** Read the
multiples as the ranges above, not to two significant figures: MySQL's own
run-to-run spread is large (CoV 33%/47%/26% at 1/4/16 — far outside even
this document's ~20% busy-desktop floor), and the 4-connection numbers in
particular look bimodal (two repetitions near 3,000-3,160 ops/s, three near
1,180-1,510) rather than smoothly scattered — a pattern worth a future
session's attention, most likely Docker/host contention specific to the
`mysql` container, not chased further here. What is not in doubt, because
the sign never once flipped across 15 measured (level, repetition) pairs:
MySQL is faster on writes at every connection count tried, and more so at
16 than at 1.

**Reads: TIE at every level**, which a glance at medians alone would not
show. 1 connection: InlaySQL 9,880 vs MySQL 9,078 ops/s (InlaySQL ahead, but
only 4 of 5 repetitions, not 5 of 5). 4 connections: MySQL 9,278 vs InlaySQL
8,224 (MySQL ahead 5 of 5, but the per-rep ratio band, 1.01-1.26x, is the
same order of magnitude as both engines' own 7-9% CoV). 16 connections:
MySQL 5,896 vs InlaySQL 5,209 (MySQL ahead 4 of 5, InlaySQL's own CoV 21%).
None of these clears this document's own bar for a real win or loss; read
all three as parity, not as a small win for either side.

**p99 write latency — the harness gap this document used to name is now
closed** (`bench/external/common.py`'s `Timer.percentiles()` returns
`(p50, p95, p99, max)` as of this session, threaded through both the
single-connection and server-to-server result writers):

| Connections | InlaySQL p99 (median, range) | MySQL p99 (median, range) | Verdict |
| --- | ---: | ---: | --- |
| 1 | 3.89ms (2.97–4.87ms) | 2.48ms (1.46–4.18ms) | parity — ranges overlap, sign flips 1 of 5 reps |
| 4 | 15.59ms (14.32–17.22ms) | 5.68ms (3.25–10.50ms) | InlaySQL worse, ~1.5-4.5x per rep, 5 of 5 |
| 16 | 37.00ms (32.18–40.53ms) | 5.69ms (3.76–16.97ms) | InlaySQL worse, ~2.4-8.9x per rep, 5 of 5 |

At 4 and 16 connections the two engines' five-repetition ranges do not
overlap at all — InlaySQL's *lowest* p99 across 5 runs still exceeds MySQL's
*highest*. This is the sharpest evidence this whole page has for any
verdict, anywhere, and it says the same thing the write-throughput table
above says, more starkly: InlaySQL's tail grows with contention where
MySQL's does not.

**Commits-per-fsync — the mechanism metric, MySQL side, and the two
counter-naming defects that would have silently reported it as `0.0`.**
`SHOW GLOBAL STATUS`'s `Handler_commit` and `Innodb_os_log_fsyncs`,
bracketed around each level's write phase:

| Connections | MySQL commits-per-fsync (median, range) |
| --- | --- |
| 1 | 0.98 (0.98–0.99) |
| 4 | 1.99 (1.96–1.99) |
| 16 | 7.42 (6.98–7.59) |

Near 1.0 at one connection, as expected (nothing to batch with), then
climbing close to linearly with connection count: InnoDB's group commit is
visibly amortising `fsync`s as writers pile up, and it is doing so *while
MySQL's own throughput measurement above swings 25-47% CoV* — this ratio's
own CoV is 0.2%/0.7%/3.3%, tighter than this document's quiet-machine
concurrency floor (3.6%, `PERF.md` §4). That gap between a noisy throughput
number and a clean mechanism number, on the same runs, is the practical case
for measuring commits-per-fsync at all, not just this document's assertion
of one.

**Getting there required fixing two counter mistakes, caught only by
running the query against a live container rather than trusting the API
name:**

- **MySQL: the task originally specified `Δ Com_commit`. `Com_commit` does
  not move at all under this benchmark's autocommit-per-statement writes** —
  it counts literal `COMMIT` statement text, and nothing here ever sends
  one. Verified directly: a plain `INSERT` against a real table left
  `Com_commit` at `0` and moved `Handler_commit` by exactly one.
  `Handler_commit` — the storage-engine counter that increments on every
  commit, explicit or autocommit-implicit — is what `mysql_driver.py` and
  `server_driver.py` now query. Using `Com_commit` as originally specified
  would have silently reported `commits: 0, fsyncs: N,
  commits_per_fsync: 0.0` at every level: a wrong number shaped exactly like
  a real one, not a missing one.
- **PostgreSQL (wired for the single-connection OLTP row, `postgres_oltp_
  driver.py`; no server exists to extend this to a concurrency sweep, so it
  is not in the table above): the counters were right, the timing was not.**
  PostgreSQL's cumulative statistics system lets a backend batch its own
  pending updates and flush them opportunistically rather than at every
  commit. A fast write phase read back `xact_commit`/`wal_sync` deltas of
  `0/0` immediately afterward even though the rows had genuinely committed
  — confirmed by re-querying a couple of seconds later and seeing the real
  values land. Fixed by calling `pg_stat_force_next_flush()` (PG15+,
  present in the pinned `postgres:17` image) immediately before each read.

**InlaySQL-server's own commits-per-fsync could not be measured — a
disclosed instrument gap, not a claim its batching is worse.**
`crates/inlaysql/src/device.rs`'s `CommitCoordinator` only ever prints its
flush/ticket counters (`INLAYSQL_COMMIT_STATS=1`) on `Drop`, which fires
when a one-shot process exits normally — the host `--export-oltp` run and
the containerised `inlaysql-oltp` replay both do this, which is how §"Durable
writes" above already reports InlaySQL's own commit-batching ratio when it
is available. It never fires for `inlaysql-server`: a long-running server is
never dropped by a normal return from `main`, and no signal handler in
`crates/inlaysql-server` drops the `Database` gracefully on `SIGTERM`
(confirmed by reading the crate). `SHOW GLOBAL STATUS` on the InlaySQL side
reports its own `Com_commit`/`Handler_commit` (`docs/server.md`) but nothing
analogous to `Innodb_os_log_fsyncs` — there is no live counter for its
`fsync` count to sample at all. The nearest available evidence, harness
mismatch disclosed rather than hidden: the in-process `WRITER_LEVELS`
concurrency sweep's own commits-per-fsync figure, already published above
("Concurrent writers"), was 4.76-6.31x at 8/32 writers — real OS threads,
one process, no wire protocol in the loop at all. That sits in the same
order of magnitude as MySQL's 7.42 at 16 connections here. Read that as weak
evidence that InlaySQL's own commit-batching *mechanism* is roughly
competitive with InnoDB's group commit when it gets to run, which would
point this section's throughput and p99 losses at `inlaysql-server`'s
thread-per-connection design (one OS thread and one `Database` handle per
connection, no pool — `docs/server.md`'s D2) rather than at the underlying
commit coordinator being outclassed. **Not confirmed** — the direct
measurement that would confirm or refute it is exactly the instrument gap
just described, and closing it (a live status counter for the server's own
flush/ticket count) is future work, not done this session.

**Superseded, same day: the instrument gap is closed and the direct
measurement is run — see "Server-to-server: InlaySQL's own commits-per-fsync,
measured directly" below.** It confirms the batching half of the "weak
evidence" read above and sharpens the diagnosis: InlaySQL's own
commits-per-fsync ties or beats MySQL's at 1 and 4 connections and trails by
only ~1.6x at 16, so the thread-per-connection hypothesis should be read as
being about how *often* the server gets to flush, not how well it batches
once it does — see below for the number that actually carries the gap.

Durability: MySQL `innodb_flush_log_at_trx_commit=1`, binlog off,
`innodb_buffer_pool_size=512M` (this session's tuning fix); InlaySQL server
has no separate durability knob, same commit path as every other row on
this page. Raw per-repetition JSON
(`results-server-oltp-{mysql,inlaysql-server}.json` per repetition) was not
retained as committed artifacts — `bench/results/` is git-ignored per this
repo's convention, the same as the interleaved OLTP rerun's own raw files
above.

### Server-to-server: InlaySQL's own commits-per-fsync, measured directly (2026-08-31)

The subsection above named the exact gap this one closes: InlaySQL-server's
own batching ratio had no live counter to sample, so the concurrency sweep
could only report MySQL's mechanism number. `SCOREBOARD.md`'s §6 traced
that to `CommitCoordinator`'s flush/ticket counters printing only on
process `Drop` (`INLAYSQL_COMMIT_STATS=1`), which never fires for a server
stopped by `SIGTERM`. That is now fixed —
`Inlaysql_normal_commit_flushes`/`Inlaysql_normal_commit_tickets` (plus the
checkpoint-inclusive `Inlaysql_commit_flushes`/`Inlaysql_commit_tickets`)
are live `SHOW GLOBAL STATUS` variables on a running server — and
`server_driver.py` now brackets them the same way it brackets MySQL's
`Handler_commit`/`Innodb_os_log_fsyncs`. This section is that sweep, run
properly: `SERVER_CONCURRENCY_LEVELS=1,4,16`, 5 repetitions, interleaved per
concurrency level (same discipline as "Server-to-server, extended" above),
load-gated (1-minute average 2.3-3.3 of the 18-logical-CPU box's 4.5
ceiling throughout — quiet).

**Commits-per-fsync, both engines, median and 5-repetition range. InlaySQL's
own like-for-like pair is `Inlaysql_normal_commit_tickets` /
`Inlaysql_normal_commit_flushes` — excludes checkpoint-triggered flushes,
the fair comparison against MySQL's `Handler_commit`/`Innodb_os_log_fsyncs`,
neither of which counts a checkpoint-analogous event either:**

| Connections | InlaySQL commits-per-fsync | MySQL commits-per-fsync | Ratio (MySQL/InlaySQL, paired per rep) |
| --- | ---: | ---: | ---: |
| 1 | 1.00 (1.00–1.00), CoV 0.0% | 0.98 (0.97–0.99), CoV 0.7% | 0.98x (0.97–0.99x) — tied, gap inside floor |
| 4 | 2.30 (2.16–2.34), CoV 2.8% | 1.99 (1.99–2.00), CoV 0.2% | **0.86x (0.85–0.92x) — InlaySQL ahead, 5/5 reps** |
| 16 | 4.63 (4.55–4.69), CoV 1.1% | 7.47 (7.34–7.59), CoV 1.2% | **1.61x (1.59–1.62x) — MySQL ahead, 5/5 reps** |

The checkpoint-inclusive pair (`Inlaysql_commit_tickets`/
`Inlaysql_commit_flushes`) tracks within about 5% of the like-for-like one
at every level (medians 1.00 / 2.25 / 4.43 at 1/4/16 connections) — the two
do not diverge materially, so this is reported for completeness rather than
because it changes anything above.

**Implied `fsync` rate — measured throughput divided by measured
commits-per-fsync, both sides, median and range:**

| Connections | InlaySQL implied fsync/s | MySQL implied fsync/s | Ratio (MySQL/InlaySQL, paired per rep) |
| --- | ---: | ---: | ---: |
| 1 | 660.9 (522.0–736.3), CoV 10.8% | 897.0 (748.0–1561.6), CoV 32.5% | 1.43x (1.16–2.14x) |
| 4 | 488.8 (478.1–504.1), CoV 1.8% | 1594.4 (695.0–1641.9), CoV 35.4% | **3.21x (1.45–3.37x)** |
| 16 | 301.7 (260.4–311.4), CoV 6.0% | 843.9 (618.6–945.8), CoV 14.3% | **2.78x (2.10–3.63x)** |

MySQL was ahead on implied fsync rate in all 15 of 15 (level, repetition)
pairs — the same "sign never flips" standard this document already applies
to the noisier throughput and p99 numbers above. Multiplying the
batching-ratio and fsync-rate-ratio medians reproduces the measured
write-throughput ratio at each level to within rounding (0.98×1.43≈1.40 vs
this rerun's own measured 1.40x; 0.86×3.21≈2.76 vs 2.77x; 1.61×2.78≈4.48 vs
4.43x) — the decomposition is internally consistent, not two numbers that
happen to multiply out.

**The headline: InlaySQL's deficit against MySQL on concurrent writes is
predominantly a barrier-rate problem, not a batching problem, at every
connection count tried.** InlaySQL's own commit-batching mechanism ties
MySQL's at 1 connection and beats it at 4; it only falls behind, by a real
but modest ~1.6x, at 16. Meanwhile InlaySQL's actual `fsync` cadence *falls*
as connections are added — from ~661/s at 1 connection to ~302/s at 16,
roughly halving — while MySQL's holds in a flat, noisy 620-1640/s band over
the same range. That is the opposite of what a "batching is fine, keep
adding writers" story would predict: more writers should mean bigger
cohorts riding a *steady or rising* fsync cadence, not one that collapses.
This reframes the earlier "our group commit is worse" reading: the commit
coordinator is not the weak link measured here — something upstream of it
is throttling how often it gets the chance to run at all. `PERF.md`'s dated
"Task 2" section runs a bounded diagnosis of why, without implementing a
fix. This rerun's own write-throughput numbers (medians 660.9/1138.9/1393.7
ops/s InlaySQL, 882.5/3171.4/6214.3 ops/s MySQL at 1/4/16 connections) sit in
the same direction and rough order as the "Server-to-server, extended"
table above (638.7/1075.0/1308.1 vs 1363.1/1512.7/6120.7) without matching
it exactly — expected noise on an already-disclosed-noisy metric (MySQL's
own throughput CoV was 25-47% there, 15-35% here), not a contradiction; the
table above is left as the published headline rather than replaced by a
second noisy instance of the same measurement.

Durability: identical to "Server-to-server, extended" above — MySQL
`innodb_flush_log_at_trx_commit=1`, binlog off,
`innodb_buffer_pool_size=512M`; InlaySQL server has no separate durability
knob. Raw per-repetition JSON was not retained as a committed artifact, same
convention as above.

---

## Read shapes and batch insert against MySQL and PostgreSQL (2026-08-31 afternoon)

Four workloads that previously had **no harness on either side** — indexed
range scan, two-table join, aggregate/`GROUP BY`, batch insert — measured in
one sitting against both servers, filling the eight UNKNOWN MySQL/PostgreSQL
cells `SCOREBOARD.md` carried since it was written. Harnesses (new):
`bench/external/read_driver.py` (range/aggregate/join, `TARGET=mysql|postgres`),
`bench/external/batch_driver.py` (batch insert with commits-per-fsync
bracketing), `inlaysql-bench --bin sql_shapes` (InlaySQL's side for the two
shapes with no Rust suite); `compose.yml` gained one shared unix-socket
volume so both engines are reached over the same transport.

**Disclosure, read before the tables: this sitting ran under desktop load.**
The quiet-machine gate refused every clean attempt (1-minute load 4-10 of 18
CPUs throughout — the host was in active desktop use), so
`BENCH_MAX_LOAD_PER_CPU=off` was used deliberately, both sides of every cell
were measured in the same sitting, and `SCOREBOARD.md` §4.0 applies PERF.md's
**20.2% desktop-load A/A floor** (not the quiet one) to every verdict. Medians
of 5 repetitions, `(shape, rep)` schedule Fisher-Yates-shuffled with a fixed
seed so no shape was systematically first; raw files in `bench/results/`
(`20260831T06*-repeat.txt` and the `read-*`/`batch-*` outputs quoted below).
InlaySQL runs in-process; MySQL/PostgreSQL run over unix sockets — an
asymmetry that favours InlaySQL, so every LOSS recorded here is conservative.

### Indexed range scan — WIN both

`SUITE=indexed`'s shape: `users (id, email, body)`, 100,000 rows, index built
after the rows, 100 range queries of exactly 50 rows each, the key sequence
generated with the same seeded xorshift64* the Rust suite uses.

| Engine | ops/s (median, range) | p50 (median) |
| --- | --- | --- |
| **InlaySQL** | 49,259 (same-sitting median of 3; published clean median 64,250) | 19.42 µs |
| PostgreSQL 17 | 21,455 (8,479-23,347) | 40 µs |
| MySQL 8 | 13,124 (11,359-13,360) | 69 µs |

InlaySQL is ~3.7x MySQL and ~2.3x PostgreSQL on the same-sitting numbers and
*more* ahead (4.9x/3.0x) on its own published clean median. The published
loss against SQLite's WAL configuration (~2.9x) stands unchanged — the range
scan is a shape InlaySQL wins against the servers and loses to SQLite.

### Two-table join — worst-first: vs MySQL TIE/WIN/WIN/WIN, vs PG LOSS/WIN/WIN/WIN

`SUITE=joins`' exact shape at `LIMIT 20`: 20,000 users × 8 round-robin posts,
index on `posts.user_id` built after the rows, ANALYZE, 100 executions per
rep, p50 medians compared, both FROM orders reported worst-first per
`SCOREBOARD.md`'s pre-fixed join rule.

| Shape | InlaySQL p50 | MySQL 8 p50 | PostgreSQL 17 p50 |
| --- | --- | --- | --- |
| PK inner, full join | 13.04 ms | 15.00 ms | **10.49 ms** |
| Secondary-index inner, full join | 4.77 ms | 15.01 ms | 10.49 ms |
| PK inner, LIMIT 20 | 14.08 µs | 39.7 µs | 27 µs |
| Secondary-index inner, LIMIT 20 | 13.38 µs | 48 µs | 29 µs |

Both servers hash-join either FROM order in ~15.0/~10.5 ms — the
iteration-side asymmetry that splits InlaySQL's own two full-join shapes
(~7.9x, see `PERF.md`) does not exist for them. InlaySQL's index
nested-loop join wins the secondary-inner shape outright (2.2-3.1x) and both
LIMIT shapes (1.9-3.6x); the PK-inner full join is a TIE vs MySQL (1.15x,
inside the 20% floor) and a **LOSS ~1.24x vs PostgreSQL** — the red cell
where PG's planner picked the better order, recorded as exactly the
"planner epic: yes/no for the human" decision `SCOREBOARD.md` scopes.

Full-join methodology, disclosed: the full-join shapes are timed as
server-side `SELECT COUNT(*) FROM (<join>) q` wrappers, because a Python
client fetching 160,000 rows per execution measures the connector's
per-row cost (the drivers container sat at 100% CPU with the server idle
before this change), not the engine's join; the wrapper still produces and
discards every joined row server-side, and the LIMIT/range/aggregate shapes
transfer their rows directly. The asymmetry favours InlaySQL — its own
number includes row streaming — so the PG LOSS above is conservative.

### Aggregate / GROUP BY — LOSS both, the worst multiples in the matrix

A shape defined this session (no Rust suite exists on either side; the
InlaySQL side runs through `sql_shapes --mode agg`): `indexed`'s 100,000-row
table with a 100-bucket column added; 100 executions per rep.

| Shape | InlaySQL | MySQL 8 | PostgreSQL 17 |
| --- | --- | --- | --- |
| `GROUP BY n` (100 groups) | 29/s (26-31) | 98/s (96-104) | 147/s (143-148) |
| scalar `COUNT/MIN/MAX` | 53/s (49-57) | 275/s (245-284) | 317/s (299-328) |

**LOSS ~3.4-6.0x, consistent in sign across every rep** — outside any floor
by a wide margin, and not explicable by transport: both opponents stream
1-100 result rows over a socket while InlaySQL is in-process. This is the
engine's grouping/aggregate pipeline, priced honestly for the first time.

### Batch insert — LOSS both

100 rows per multi-row INSERT statement, autocommitted, 100 statements per
rep (10,000 rows per rep), explicit ids, all three engines in the same
container environment on the same volume class, durability aligned (MySQL
`innodb_flush_log_at_trx_commit=1`, PG `synchronous_commit=on`, InlaySQL
`Durability::Full` — one commit, one barrier per statement everywhere).

| Engine | rows/s (median, range) | commits/s | c/fsync |
| --- | --- | --- | --- |
| InlaySQL | 26,254 (19,111-43,851) | 263 | 1.00 |
| MySQL 8 | 42,933 (39,543-44,379) | 429 | 0.64 |
| PostgreSQL 17 | 81,229 (73,881-91,918) | 812 | 1.00 |

**LOSS ~1.6x vs MySQL, ~3.1x vs PostgreSQL.** InlaySQL's wide rows/s range
(1.8x min-to-max) is the desktop load showing up on the slowest side; even
its best rep (43,851) does not reach MySQL's median, and the
noise-resistant metric — commits/s, and c/fsync — orders the engines the
same way. InlaySQL's ~263 commits/s at c/fsync 1.00 is the same ~1.2-1.5 ms
single-writer commit cycle `PERF.md` Task 3 measured, so this cell prices
that same mechanism per row rather than per commit.

---

## What is not measured here

- **No external benchmark for text or hybrid ranking.** Vector search now has
  one — the `ann-benchmarks` table above is an external corpus, an external
  ground truth and an external protocol. BM25 and the hybrid fusion still have
  none: every quality figure for them on this page is scored against a
  reference ranking this repository computes. The counterpart would be a BEIR
  subset (`scifact`, `nfcorpus`) with its own `qrels` and nDCG@10, and
  `bench/README.md` records what building it needs — chiefly a decision about
  whether to match Lucene's analyzer, since BEIR's published BM25 baselines are
  Anserini's and a gap against them would otherwise be a tokenisation
  difference wearing a scoring result.
- **Now has a trustworthy driver, and the obvious server-side explanation
  did not survive checking, twice.** The server-to-server table above is
  process-isolated on both sides as of 2026-08-29, closing the "the client's
  own concurrency shape might be the real cause" question the last three
  editions carried: InlaySQL's read throughput really does drop from one
  connection to eight (9,033.3 → 6,294.3 ops/s), where MySQL's own is flat
  across the same step. First check: the same driver against `inlaysql
  serve --mysql` directly on the host (no Docker, a loopback socket) at
  matching and larger workload shapes never reproduces it, scaling *up*
  instead (1,180 → 9,980 ops/s at the Docker-matched shape), and a profile
  of the server during that load found it 81.4% idle in `recvfrom`, not
  CPU-bound. Second check, ruling out the container environment itself
  rather than just the host/loopback difference: `inlaysql-server` run
  *inside* the same compose network, alone and then with the full
  five-container stack present but idle, both scale up cleanly too
  (2,336 → 16,549 ops/s with every original container present and running,
  none of them sent a query). What *does* reproduce the drop: running
  `mysql_driver.py` for real immediately before `server_driver.py`, in that
  same idle stack — exactly this table's own generation order at the time
  (`bench/compare.sh` ran DuckDB, pgvector, PostgreSQL, MySQL, *then*
  server-to-server; Meilisearch was added to the sequence afterward, between
  pgvector and PostgreSQL). Not root-caused past that — plausibly MySQL's own
  post-write background work, plausibly something the `drivers` container
  carries across sequential phases, plausibly a Docker Desktop VM scheduler
  effect from a preceding burst that a 20-second gap did not clear — but
  `inlaysql-server`'s thread-per-connection model, the standing
  architectural difference from MySQL's worker pool and the obvious first
  suspect, is now the *less* likely explanation: two independent hosts
  (loopback and compose network) both say it has spare capacity at this
  concurrency. A worker-pool rewrite aimed at this number would very likely
  not fix it. See `PLAN.md`'s W5 for what is still open.
- **No server-to-server PostgreSQL row.** `inlaysql serve` speaks the MySQL
  wire protocol and nothing else, so there is no like-for-like transport to
  measure PostgreSQL over; `bench/README.md` says so under
  "Server-to-server".
- **No sustained or multi-core saturation workload.** Everything here is a
  latency-shaped micro-benchmark on one machine.
- **No controlled machine state, still — but this edition measured the floor
  instead of assuming it, and every table above states its own error bar.**
  Every `run.sh`-derived table (points, indexed, joins, vectors, concurrency,
  retrieval) is a median of three complete runs, published as median-plus-
  range rather than a bare point value, with the disagreement between runs
  disclosed per section: 106 of 343 metrics moved by 10% or more across the
  three main-suite runs at `7b20175` (19 of 135 on core columns alone — see
  the spread note at the top of this file), and, in the carried-forward
  2026-08-30 sessions, 63 of 180 in the wide concurrency sweep and 25 of 64
  in the quantisation spot-check. A same-binary A/A test (`PERF.md` §4,
  2026-08-30) puts a number on what "spread" means here: median CoV 4.0% on
  the main suite's core columns, 3.6% on the concurrency sweep, 0.3% on the
  quantisation spot-check, and 7.3-20.2% on the single most scrutinised
  metric depending on how busy the machine was — the acceptance target (CoV
  under 3%) is not met today. This edition's whole-suite spread (106 of 343)
  is narrower than the 2026-08-30 edition's (196 of 343) on the same tool
  and the same metric list, and that edition's was in turn wider than the
  one before it — 54/266 (20.3%) then, 146/266 (54.9%) there, recomputed on
  the metrics common to both — see the spread note at the top of this file
  and `PERF.md` §4 for the full measurement, including why the originally
  published "56 of 285" comparison overstated it. Read every ratio in this
  document as approximate
  rather than exact — the point-reads section above is the extreme case,
  where the individual runs' own ratios against journal-mode SQLite ranged
  from 2.05x to 3.80x. `bench/compare.sh` carried none of the gated
  machinery when the tables below were measured — no repeat wrapper, and load
  sampled once before the run rather than throughout it. **Both landed
  2026-08-31** (`bench/load_gate.sh`, shared with `run.sh`, and
  `bench/repeat-compare.sh`), and the `trust.yml` question that had this
  recorded as a recommendation rather than a change is answered: the gate did
  fail the shared-runner benchmarks job on its baseline load (run
  33396108404), and the override is now job-level so both entrances agree.
  None of the tables below have been re-measured with any of it. So the
  DuckDB/pgvector/Meilisearch table and the
  carried-forward MySQL/PostgreSQL section remain single runs or, for the
  interleaved rerun, a 5-repetition median done specifically for that
  comparison — not a `REPEATS=N` sweep. Pinning the machine state itself is
  still not done and probably cannot be, which is why the spread is published
  instead.
- **Cold-cache reads.** The point-read rows are warm; an application that
  opens, reads a handful and exits sees worse, because our miss path is dearer
  than SQLite's.

## The correctness note, updated on purpose

The previous edition of this file carried an open crash-recovery bug
(AHL-406): one schedule in ten thousand recovered a database to a state no
commit produced. **It is fixed** — the page allocator could rewind after a
crash landed on a wrap's sync, re-issuing page ids and letting recovery mix
two timelines; `CowBTree::adopt_next_page_id` makes the allocator monotonic
per handle, the once-failing seed passes un-`#[ignore]`d, and both DST sweeps
(10,000 schedules) are green. The full account is in `docs/architecture.md` and
`docs/recovery.md`. It is recorded here because performance numbers from an
engine whose crash safety had a known hole deserved to be read in that light —
and because the fix is what the honesty rule is for.
