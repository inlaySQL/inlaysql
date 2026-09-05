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
> hedged into mush. As of this edition the `compare.sh` tables (DuckDB/
> pgvector/Meilisearch, MySQL/PostgreSQL, server-to-server) are gated
> medians of three too, with their own ≥10% list disclosed; the read-shape
> and batch-insert drivers are medians of five. The one single-run table
> left is `ann-benchmarks`, which has no repeat of its own to measure a
> spread from; read its ratios as *less* certain than the gated tables
> here, not more precise for lacking a stated range.

**How this run was produced**

```sh
REPEATS=3 ./bench/repeat.sh                                        # points, indexed, joins, vectors, concurrency (1,2,4,8), retrieval — this edition
WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32 SUITE=concurrency ./bench/run.sh   # x3, median — wide sweep + tail latency (carried forward)
SUITE=quantization DOCS=100000 QUERIES=50 ./bench/run.sh           # x3, median — int8 spot-check at scale (carried forward)
REPEATS=3 ./bench/repeat-compare.sh                                # DuckDB, pgvector, Meilisearch, MySQL 8.4, PostgreSQL 17, server-to-server (needs Docker) — gated, median of three, carried forward from 2026-09-02/03
docker exec inlaysql-bench-drivers-1 sh -c 'TARGET=mysql    REPS=5 python /drivers/read_driver.py'   # range/aggregate/join, unix socket — carried forward from 2026-09-02/03
docker exec inlaysql-bench-drivers-1 sh -c 'TARGET=postgres REPS=5 python /drivers/read_driver.py'
docker exec inlaysql-bench-drivers-1 sh -c 'TARGET=mysql    REPS=5 python /drivers/batch_driver.py'  # batch insert + commits-per-fsync — carried forward from 2026-09-02/03
docker exec inlaysql-bench-drivers-1 sh -c 'TARGET=postgres REPS=5 python /drivers/batch_driver.py'
REPS=5 cargo run --release -p inlaysql-bench --bin sql_shapes -- --mode agg    # InlaySQL's aggregate side, host — carried forward
REPS=5 cargo run --release -p inlaysql-bench --bin sql_shapes -- --mode batch  # InlaySQL's batch-insert side, host — carried forward
```

| | |
| --- | --- |
| Commit | `run.sh` tables: `be95cc3` (engine source identical to `c422ed3`, the AHL-553 commit; the twelve commits between them touch only `crates/inlaysql-server`, the FFI wrappers, CI, one `#[cfg(test)]` module in `device.rs` and docs — nothing on any path these suites execute). `compare.sh`-sourced and driver-sourced tables (DuckDB/pgvector/Meilisearch, MySQL/PostgreSQL, server-to-server, read shapes, batch insert): `bdc64eb`, unchanged from the previous edition and **not regenerated this time** — engine identical to `edc8aed`, which changed only `bench/summarise.py`; `bdc64eb..be95cc3` is AHL-544 through AHL-553 (`PERF.md`, 2026-09-03 and 2026-09-04), so the two commits are not the same engine — the `run.sh` tables are two editions newer than the `compare.sh` ones — and every section says which one it came from. |
| Date | 2026-09-05 (`run.sh`, 02:00–02:23 UTC, i.e. 10:00–10:23 local); 2026-09-02/03 (`compare.sh` and the drivers, 18:53–19:15 UTC, i.e. 02:53–03:15 local on the 3rd) |
| Tree | source clean at measurement (`dirty: no` in all three `run.sh` raw outputs and in the `repeat.sh` summary). |
| Machine | Apple Mac17,9, 18 cores, macOS 27.0 (Darwin 27.0.0 arm64) |
| Toolchain | rustc 1.91.1 (ed61e7d7e 2025-11-07) |
| Raw output | **`run.sh`/SQLite/sqlite-vec/concurrency/retrieval, median of three** (`SUITE=all`: points, indexed, joins, vectors, concurrency 1/2/4/8, retrieval): `bench/results/20260905T020058Z-repeat.txt`, built from `bench/results/20260905T{020058,020830,021602}Z.txt`. Load, sampled every 5 s throughout the measured phases, min/median/max per run: 1.74/2.72/3.49, 2.22/2.84/3.77 and 1.21/2.23/3.22 of 18 CPUs against the gate's 0.25/CPU (4.5) ceiling; no run marked `CONTAMINATED`, `dirty: no` throughout, and all three runs came from the first attempt. This is the fifth full regeneration since 2026-09-02 and the first on the 5th; the fourth, at `1f7921a` on the evening of the 3rd (`bench/results/20260903T123928Z-repeat.txt`, load 1.6–4.3/18), is the "previous edition" every section below compares against, and the three before it — `3cf0d85` on the evening of the 2nd (`bench/results/20260902T124832Z-repeat.txt`), `4f8e5dd` that afternoon (`bench/results/20260902T062536Z-repeat.txt`) and `7b20175` that morning (`bench/results/20260902T022325Z-repeat.txt`) — are named where a section's history needs them. **No harness changed between the previous edition and this one**, for the second edition running: `1f7921a..be95cc3` touches `crates/inlaysql-bench` only in `bin/profile` (AHL-552's tail histogram and per-query diagnostics delta) and a new `commit_growth` binary (AHL-553), neither of which produces a figure on this page, so every `run.sh` suite here is the same measurement as the 3rd's with a different binary. **Carried forward from the 2026-08-30 edition at `2cb2539`, not regenerated this edition** (each section says so where it appears): the **concurrency wide sweep + tail latency** (`bench/results/20260830T{124155,124632,125240}Z.txt`, `WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`, median of three, load 2.9–3.6/18 — this regeneration ran only the default 1/2/4/8 levels, so the 1/2/4/8 table is fresh and the eleven-level sweep and its 32-writer tail row are not); the **quantisation spot-check at scale** (`bench/results/20260830T{125800,131326,132715}Z.txt`, `SUITE=quantization DOCS=100000 QUERIES=50`, median of three, load 2.3–4.8/18). **Regenerated for the first time under the gate, at `bdc64eb`, on the night of 2026-09-02/03** (each section says so where it appears): every **`compare.sh`-sourced table** — the **DuckDB/pgvector/Meilisearch retrieval** table, the **"Against MySQL and PostgreSQL"** OLTP table (host and containerised InlaySQL, MySQL **8.4**, PostgreSQL 17) and the **"Server-to-server"** 1/8-connection table — is now the median of three complete `REPEATS=3 ./bench/repeat-compare.sh` runs (`bench/results/20260902T185304Z-repeat-compare.txt`, built from `bench/results/20260902T{185718,190221,190724}Z-compare.txt`; load sampled every 5 s through the measured phases, min/median/max per run 0.85/1.81/2.49, 1.60/2.34/3.05 and 2.05/2.45/2.77 of 18 against the 4.5 ceiling; no run marked `CONTAMINATED`; the same four unrelated user containers as before were present and idle throughout; 30 s cooldown between repetitions; 53 of 146 metrics disagreed by 10% or more across the three, listed in the summary file); and the **read-shape and batch-insert** tables' MySQL/PostgreSQL columns and InlaySQL aggregate/batch cells are `REPS=5` medians with min–max from `bench/results/20260902T191343Z-scoreboard/` (`read-{mysql,postgres}.txt`, `batch-{mysql,postgres}.txt`, `sql-shapes-inlaysql.txt`, `sql-shapes-inlaysql-batch.txt`; `provenance.txt` records `uptime` before and after — load 1.47–2.36/18 — rather than a mid-run sampler, a weaker gate than `compare.sh`'s, disclosed). **The MySQL container is `mysql:8.4` (LTS) from `e7cc895` (2026-09-02) on; every "MySQL 8" figure this file published before 2026-09-02 was 8.0.x**, and the version changed underneath every MySQL edition-to-edition comparison below — none of those moves is attributed to either engine. The InlaySQL range and join cells of the read-shape table are reused from this edition's `run.sh` tables at `be95cc3`, as the previous three editions reused their own `run.sh` figures, and say so — those cells are now two engine editions *later* than the server columns beside them. **Carried forward from 2026-08-31, not regenerated**: the two "Server-to-server, extended" 1/4/16-connection sweeps (5 interleaved repetitions each, manually load-gated; raw JSON not retained). **Carried forward from earlier still**: the concurrent-writer old-vs-new A/B (`08f5fd4`, 2026-08-30, `bench/results/ab-head-run{1,2,3}-*.txt` and `ab-pre94d96a6-run{1,2,3}-*.txt`), and, as history only, the 2026-08-30 interleaved OLTP rerun at `b4798ce` (`bench/results/20260830T095714Z-interleaved-oltp-compare.txt`), superseded by the 2026-09-02/03 gated repeat. |

One developer machine. Reproduce it; do not trust it. Every `run.sh` table
on this page — points, indexed, joins, vectors, concurrency at 1/2/4/8
writers, retrieval — comes from `be95cc3`, measured fresh in one gated
sitting on the morning of 2026-09-05. Every `compare.sh` table and every
driver-sourced cell — DuckDB/pgvector/Meilisearch, MySQL 8.4/PostgreSQL 17
OLTP, server-to-server at 1/8 connections, the read shapes and batch insert
— still comes from `bdc64eb`, measured in one gated sitting on the night of
2026-09-02/03 and **not regenerated this edition**; it is now two engine
editions behind the tables above it, and each section that carries it says
so. The tables that remain carried forward (the wide concurrency sweep, the
quantisation spot-check, the 1/4/16-connection server sweeps, the writer
A/B) each state their own commit and date where they appear, so a reader
can always tell which build produced which number. What landed between the
previous edition (`1f7921a`) and this one, all in `PERF.md`'s 2026-09-04
sections and the only source any attribution below draws on: **AHL-551**
(a point lookup retains the path it descended and resumes from the lowest
ancestor whose separator span still admits the key, instead of walking from
the root every time; +3–7% on `indexed-range`, 6 of 6 interleaved and
non-overlapping, the published PK `LIMIT 10` p50 3.46 / 3.50 / 3.42 → 3.38
/ 3.33 / 3.29 µs 3 of 3, the secondary `LIMIT` shape flat, and `points`
flat — which was the gate it had to pass, not a bonus), **AHL-552** (the
commit path admits the pages it has just written into the decoded cache and
drops the ones it superseded: over the bench's own write phase the handle
went from **151,635 evictions to zero** and the read phase from 573 misses
to none, and the published `points` shape measured interleaved read p95
3.17 / 2.63 / 2.67 → 0.92 / 1.42 / 1.38 µs and p99 5.08 / 4.54 / 4.21 →
1.33 / 1.83 / 1.79 µs, **3 of 3 non-overlapping on each**, with the median
and the ops/s figure mixed in sign), AHL-553 (a commit's barrier stops
paying to grow the file — the data area is extended in geometric chunks and
zero-filled ahead of the writer; ~1.18x on a containerised single-row
commit, 11 of 12 interleaved repetitions, and **flat on the macOS host**,
where `F_FULLFSYNC` is 99% of a commit and the growth is lost inside it, so
it is not expected to move any table on this page — the one thing it does
move is the *size* of the files the `sqlite-vec` section reports, which
that section now says), AHL-554 (a row-id-shaped key comparator: built,
wired into every comparison the profile named, measured interleaved against
`85a8c1c` across all seven suites and reverted, because every suite's
ranges overlapped — no code landed, the commit is the record of the
measurement), AHL-555 (a connection thread's own time split three ways into
`SHOW GLOBAL STATUS` — `crates/inlaysql-server` instrumentation, on no path
any suite here runs), and three server security fixes (the F4 brief's
findings 1–3: a `TIME` parameter's day count, a packet payload read as it
arrives rather than as it is claimed, and the `SSLRequest` shortcut
following the handshake phase), all three in the wire protocol's packet
path, which none of these suites executes. Nothing on this page is withheld.

**Tooling correction, 2026-08-31, paid 2026-09-02/03.** Until the
2026-09-03 edition every `compare.sh`-sourced table on this page was a single ungated pass
where the `run.sh` tables were gated medians of three, and the 2026-08-31
edition said so and named the debt: `bench/load_gate.sh` (shared by
`run.sh` and `compare.sh` — same gate, same mid-run sampling, same
`CONTAMINATED` marking; `compare.sh` watches only its measured phases, not
its own container builds) and `REPEATS=N ./bench/repeat-compare.sh`
(median and spread through the same `bench/summarise.py`) existed but had
never been run for publication. **They have now**: every `compare.sh` table
below is `REPEATS=3`, gated, none contaminated, with its ≥10% list
disclosed per section — that sitting's figures are carried forward into
this edition unchanged — and the first thing the instrument found is that
`compare.sh`'s numbers swing at least as much as `run.sh`'s (53 of 146
metrics by ≥10%, against 109 of 343 on the main suite that night), with the MySQL
server-to-server p50 the widest row on the page at 241%. What is still not
addressed: interleaving the engines *within* one pass (`compare.sh`'s phase
order is fixed — retrieval, then OLTP, then server-to-server), which is the
half of the recommendation `bench/README.md` still carries.

**This edition's spread is the widest of the five sittings since 2026-09-02
on both counts, on a machine that was quieter than any of them by peak
load — and the loudest rows are the ones every earlier edition named.** The
main `run.sh` suite (points/indexed/joins/vectors/concurrency/retrieval),
median of three complete runs at `be95cc3`: **128 of 343 metrics disagreed
by 10% or more** across the three, against 114 of 343 at `1f7921a` the
evening of the 3rd, 109 at `3cf0d85`, 114 at `4f8e5dd`, 106 at `7b20175`
and 196 on 2026-08-30, all counted by the same `bench/summarise.py` (the
summary file itself says 137 of 363: `edc8aed` taught the tool to read the
concurrency table's four `barriers:` counter lines as values, twenty
metrics no earlier edition counted, and the 343 here excludes them so the
sequence stays one measurement). On the columns that are the measurement
itself (ops/s, p50, joins/s, commits/s, recall@k — excluding
`max`/`p95`/`p99`/`cold`, which are one sample and expected to swing far
more) it is **29 of 134 (22%)**, against 22 of 135 the edition before, 10
at `3cf0d85` and 19 in the two sittings before that. That denominator moved
by one for a reason worth stating rather than smoothing: the tool names a
column by character position, and the small-corpus `file bytes:` line's
exact figure gained a digit under AHL-553's preallocation, so it no longer
lands under a column the tool has a name for. The 2026-08-30 edition's "53
of 108 (49%)" was counted over a slightly different column selection
(`PERF.md` §4), so compare the whole-suite
128-versus-114-versus-109-versus-114-versus-106-versus-196 and not the core
fractions digit for digit. Median load was 2.2–2.8/18 across the three runs
against 2.3–2.9 the edition before, and **no sample in this sitting reached
3.8** where the previous sitting's peak was 4.31 — the quietest of the five
by peak. It did not help the loudest row: the run with the *highest* median
load and the *highest* peak (`020830Z`, 2.84 and 3.77) again produced the
fastest point read by a factor of two, and the run with the lowest load
throughout (`021602Z`, 1.21–3.22) produced the middle one. `PERF.md`'s
AHL-552 section names that shape directly and does not explain it away: a
read phase of 2–10 ms that begins the instant eighty seconds of
barrier-bound commits end is measuring a frequency or core-placement ramp,
and a machine that is already busy is already clocked up. The tightest
tables here held within 0–6%: InlaySQL's durable write (ops/s 1%, p50
0.5%), all four InlaySQL join p50s (1–4%), the indexed point probe (4% and
2%), the BM25 p50 (0.5%), the hybrid p50 (2%) and both exact-HNSW vector
p50s (6% and 5%). The loud core columns, all named in their sections: the
point-read row again (InlaySQL ops/s 98%, p50 67% across three runs of one
binary — the runs read 695k, 1,583k and 911k ops/s at 1.00, 0.50 and 0.75
µs) and journal-mode SQLite's own point read beside it (p50 63%, ops/s
33%); SQLite WAL's secondary-index `LIMIT` join (joins/s 83%, p50 37% — the
loudest SQLite cell on this page for the second edition running); the
uniform corpus's ~1% filtered HNSW p50 (47%) and its ~0.1% one (11%); the
2-writer row (p50 29%, commits/s 15%) and the 8-writer p50 (23%); the
indexed range row (p50 23%, ops/s 18%); the batched write (p50 19%, ops/s
15%); SQLite's journal-mode durable-write ops/s (18%) and p50 (12%);
SQLite WAL's own point-*write* row (ops/s 15%, p50 10%); the two
index-versus-full-scan ratios (17% and 16%); the PK `LIMIT` join's WAL row
(p50 15%, joins/s 12%); and four SQLite journal join cells — both full
shapes' p50 (13% and 12%) and their joins/s (12% and 10%). The 2026-08-30
edition's history — first published as "worse than the last full edition's
56 of 285", then recomputed on the 266 metrics common to both editions as
54/266 (20.3%) then, 146/266 (54.9%) there — stands as written in `PERF.md`
§4. Read every
ratio in this document as approximate, not as three significant digits, and
read a "the previous edition's figure was X, this one is Y" sentence as this
benchmark's ordinary noise unless the text says otherwise and the movement
clears the floor stated at the top of this file. This session's machine
carried its usual mix of editor, browser and agent processes throughout
(disclosed per-phase below). The `compare.sh` tables are the 2026-09-02/03
sitting's, carried forward unchanged, and their own four unrelated, idle
Docker containers are described where they appear rather than repeated
here.

---

## Against SQLite

SQLite is measured in two configurations because they are two different
promises. `journal` + `synchronous=FULL` + `fullfsync` is the like-for-like
column: it is the only one that makes a durability claim comparable to ours,
and `fullfsync` is what makes a macOS number mean anything at all. WAL +
`synchronous=NORMAL` is SQLite at its fastest, and is the harder target.

### Point reads by primary key — we beat the durable configuration, and the tail is gone

20,000 rows, 5,000 lookups, prepared statements on both sides. Median of
three runs (`bench/results/20260905T{020058,020830,021602}Z.txt`, load
1.2–3.8/18 throughout, gate passed).

**The harness is the previous two editions', unchanged.** For the record,
because the six editions before those were on a different loop: since
AHL-535 (`f1b81c7`, `crates/inlaysql-bench/src/points.rs`) InlaySQL's side
steps each row through `query_prepared_each_ref` — `&[ValueRef]` borrowed
out of the page — instead of `query_prepared`, which built and dropped a
`Vec<Vec<Value>>` per lookup; SQLite's side reads its column through
`row.get_ref(0)?.as_str()` instead of `row.get::<String>(0)`, one
allocation per lookup removed from *SQLite's* loop; and both sides read the
`body` column of every row into a checksum the loop `black_box`es, where
before they counted rows. The first change helps us, the second helps
SQLite, the third adds work to both, and the bench module's own doc states
it as the comparison getting harder for InlaySQL rather than easier.

| Engine | ops/s (median, range) | p50 (median, range) | p95 (median)† | p99 (median)† |
| --- | --- | --- | --- | --- |
| **InlaySQL** | **910,788** (695k–1,583k) | **0.75 µs** (0.50–1.00 µs) | **2.17 µs** | **3.79 µs** |
| SQLite, WAL + `sync=NORMAL` | 1,225,127 (1,170k–1,276k) | 0.791 µs (0.750–0.792 µs) | 1.00 µs | 1.21 µs |
| SQLite, journal + `sync=FULL` | 238,965 (159k–239k) | 4.13 µs (3.00–5.58 µs) | 7.83 µs | 13.29 µs |

† `p95`, `p99` and `max` (not shown) are tail samples and swing far more run
to run than `ops/s` or `p50` — see the floor note at the top of this file —
so they are not given a range here. They are shown this edition because
they are the finding.

**The tail this row has carried since it was first published is gone, and
it has a name.** p95 6.50 → **2.17 µs** and p99 10.50 → **3.79 µs** against
the previous edition — three times better and nearly three times better —
on the one row where a named commit predicted exactly that. **AHL-552**
(`PERF.md`, 2026-09-04) instrumented every query in this shape into a log2
histogram with a `Database::diagnostics()` delta per slow query, and found
what no self-time profile could have shown: the bench's 20,000 single-row
durable commits leave the decoded page cache **full to the byte of dead
pages** — every commit copy-on-writes ~7.6 root-to-leaf pages, the next
commit re-reads what the previous one wrote, and the one after supersedes
it, so the handle entered the read phase with 1,431 resident superseded
versions of the same few paths and **151,635 evictions over the write
phase**, while the leaves holding the rows were not resident at all. The
5,000 lookups then paid for it: 573 misses (11.5%), 915 clock evictions,
**54% of the read phase's time in the 11% of queries that missed**, and 94%
of those slow queries carrying an eviction and a decode on their own
counters. The fix is two halves, each useless alone: `CowBTree::supersede`
drops a superseded page from the decoded cache the moment the transaction
replaces it, and the commit path admits the pages it has just written.
After it, the same write phase leaves 856 live pages and **zero evictions**,
and the read phase takes no misses at all. Measured interleaved on this
exact published shape, order alternated, three repetitions: p95 3.17 / 2.63
/ 2.67 → **0.92 / 1.42 / 1.38 µs** and p99 5.08 / 4.54 / 4.21 → **1.33 /
1.83 / 1.79 µs**, 3 of 3 non-overlapping on each. This gated median reads
2.17 and 3.79 µs — larger than the A/B's post-fix figures on a different
day's machine, and three times better than the edition it replaces, which
is the direction and roughly the size the mechanism predicts. **This is the
first attributed improvement this row has ever carried.**

**The median p50 went the other way, and that is published rather than
explained away.** 0.625 → 0.75 µs, +20%, on the same code that took the
tail apart. The A/B saw the same thing — its own medians moved 500 / 375 /
375 → 416 / 500 / 417 ns, mixed in sign — and `PERF.md` states plainly that
it does not know why the *median* of this loop moves 3x between reps on
either binary while SQLite WAL's, in the same process seconds later, sits
at 750–792 ns every time and the profile's own steady 4 s loop sits at 292
ns every time. Its best available account is that a 2–10 ms read phase
beginning the instant eighty seconds of barrier-bound commits end measures
a frequency or core-placement ramp, which is a harness property and not an
engine one. What this document can say without picking a side: the two
editions' p50s are 0.625 and 0.75 µs, the three runs behind each are 0.500
/ 0.625 / 0.916 and 0.500 / 0.750 / 1.00 µs, and every one of those six
figures sits inside the band this row has occupied all week. A 20% move
inside a metric whose own three runs disagree by 67% is not a result in
either direction, and it is not read as one here.

**Roughly 3-10x the durable configuration, and that range is the
measurement.** This session's own three individual-run ratios against
journal-mode SQLite were 2.91x, 9.93x and 3.81x (the harness's own "is Nx
faster" lines); the median run says 3.81x, and the 9.93x is one run in
which our fastest read and SQLite's slowest happened together — its
InlaySQL side read 1,582,654 ops/s while its SQLite journal side read
159,323, each the outlying end of its own column. A number whose InlaySQL
side swung 694,903 to 1,582,654 ops/s (98% of the median) and 0.50 to 1.00
µs on p50 (67%) across three runs of one unrebuilt binary cannot support
one significant figure, let alone two, and neither can journal-mode
SQLite's own side this time: 33% on ops/s and 63% on p50 (159,323 at 5.58
µs in the middle run against 238,965 and 238,976 at 4.13 and 3.00 µs in the
other two). Those four are again the widest core-column disagreements in
the whole suite. WAL's row held within 9% and to within 42 ns.

**The runs, in order** (`020058Z`, `020830Z`, `021602Z`): 694,903 /
1,582,654 / 910,788 ops/s at p50 1.00 / 0.500 / 0.750 µs, p95 2.17 / 1.08 /
2.25 µs, p99 8.04 / 2.04 / 3.79 µs, max 302.88 / 113.83 / 202.92 µs. The
middle run is the fast one, and it is again the run with the highest median
load (2.84 of 18) and the highest peak (3.77); the last run had the quietest
machine of the three by both measures and read in the middle. That is the
third sitting running in which the load samples do not explain the ops/s
column, and `PERF.md`'s AHL-552 section is the first account of it that
names a mechanism instead of a suspicion. What *is* new and is not run
selection: **every one of the three runs' p95 is below the previous
edition's best run's p95** (2.17 / 1.08 / 2.25 against 3.13 / 6.50 / 7.29),
and the same holds on p99 (8.04 / 2.04 / 3.79 against 5.08 / 10.50 /
11.79 — two of three, with the loud run's 8.04 still under the previous
edition's median).

**Against the previous edition the throughput went up 31% and the whole of
it is in the tail.** The 3rd's `1f7921a` edition read 692,893 ops/s at
0.625 µs (558k–1,169k, p95 6.50 µs, p99 10.50 µs); this one reads 910,788
at 0.75 µs (695k–1,583k, p95 2.17 µs, p99 3.79 µs) — +31% on the median
ops/s, +20% on the median p50, p95 3.0x tighter and p99 2.8x tighter, and
the worst run *better* than the previous edition's worst (695k against
558k) as well as the best being better (1,583k against 1,169k). ops/s is
5,000 lookups' total wall clock, so it pays the tail, and the tail is what
AHL-552 removed; a p50 that moved the other way inside its own spread while
p95 and p99 moved 3x is exactly the shape "the tail got fixed and the
median did not" produces. Four sittings on four binaries have now put the
median at 1,069k, 872k, 693k and 911k while the best run sat at 1,155k,
1,154k, 1,169k and 1,583k.

SQLite's own rows moved, on code that did not, and one of them moved a lot.
The journal row went 177,098 → 238,965 ops/s (+35%), 5.33 → 4.13 µs — a
move far outside the previous sitting's 13% spread but well inside this
one's own 33%, and five sittings have now put SQLite's durable point read
at 170k, 278k, 170k, 177k and 239k, a band 1.6x wide on unchanged code. The
WAL row went 1,233,552 → 1,225,127 (−1%), 0.750 → 0.791 µs, inside both
sittings' spreads. Nothing on SQLite's side is attributed either way.

**Against WAL-mode SQLite this row still reads two ways, but both readings
moved toward us.** On p50, InlaySQL's 0.75 µs is a shade *below* WAL's
0.791 µs at the median — and per run it is one loss, one win and one tie
(1.00 against 0.792, 0.500 against 0.791, 0.750 against 0.750), which is a
weaker statement than the previous edition's two-of-three and is stated as
such. On throughput, InlaySQL's 910,788 ops/s is **0.74x** of WAL's
1,225,127 — per run 0.59x, 1.29x and 0.71x, so for the first time on this
page **one run of three crossed above SQLite WAL's throughput** (0.56x
median the edition before, with no run crossing; 0.69x and 0.92x before
that). The tail ratios are where the change is unambiguous: InlaySQL's p95
(2.17 µs) and p99 (3.79 µs) are **2.2x and 3.1x** WAL's (1.00 and 1.21 µs),
against 6.5x and 8.7x the edition before and 4.7x and 6.3x the one before
that. The typical lookup is still a shade faster than SQLite's fastest
configuration; the slow ones are no longer several times slower, and the
throughput figure is where that shows. The page cache (AHL-420) is what
does the winning half on a *warm* handle; AHL-552 is what stopped that same
cache filling with pages no root can reach; a cold handle still warms more
slowly than SQLite's because our miss path is dearer.

This row has now been published at 636,980, then 342,747, then 901,158, then
522,562, then 533,943, then 1,069,233, then 872,474, then 692,893, and now
910,788 ops/s across nine editions — the last three on a different harness
from the six before them, so the sequence is not one measurement. **The
swing is not mysterious in kind, only in size: `PERF.md` §4 dissected this
exact metric directly and found background scheduling contention alone
triples its CoV, from 7.3% on a quiet, gated machine to 20.2% on this same
machine under ordinary desktop load, on five runs of one unrebuilt binary —
no rebuild, no edition change, no code touching the read path at all.**
This gated sitting reproduced that: the widest of its three runs is 2.3x
the narrowest (2.1x the edition before), on a machine that passed the load
gate throughout and never reached 3.8 of 18. That is still the
worst-measured floor of any row in this document, which is why this edition
publishes a median of repeated runs with the runs beside it, and why the
ratio against journal-mode SQLite — read as "roughly 3-10x", not to three
digits and not to one — is the number to quote, not the point value either
side of it. The p95/p99 improvement above is the one claim on this row that
does *not* rest on the median: it is 3 of 3 non-overlapping in an
interleaved A/B, 3 of 3 against the previous edition's own three runs, and
it has a counter behind it that went from 151,635 to zero.

### Secondary-index reads — point win, range loss

20,000 rows, `CREATE INDEX` on a non-key TEXT column, 5,000 point lookups and
100 range queries of 50 rows (`SUITE=indexed`). Same three runs as the point
reads above.

**The harness is the previous two editions', unchanged** (AHL-535's loop,
`crates/inlaysql-bench/src/indexed.rs`: InlaySQL steps rows through
`query_prepared_each_ref`, SQLite reads through `row.get_ref(..)`, both
sides read *both* selected columns of every row into a checksum), so every
column here is the same measurement as the 3rd's with a different binary.

| Engine | point ops/s (median, range) | point p50 (median, range) | range ops/s (median, range) | range p50 (median, range) |
| --- | --- | --- | --- | --- |
| **InlaySQL (B-tree index)** | **473,401** (463k–482k) | **1.92 µs** (1.88–1.92 µs) | 118,489 (99k–120k) | 8.00 µs (7.79–9.63 µs) |
| InlaySQL (no index: full scan) | 1,329 (1,292–1,337) | 751.46 µs (744.33–769.67 µs) | 1,148 (1,132–1,149) | 873.96 µs (870.71–887.42 µs) |
| SQLite, journal (index) | 257,670 (252k–264k) | 3.67 µs (3.63–3.71 µs) | **144,622** (137k–146k) | **6.67 µs** (6.58–6.71 µs) |
| SQLite, WAL (index) | 762,089 (729k–768k) | 1.13 µs (1.13–1.17 µs) | **235,641** (230k–238k) | **4.04 µs** (4.00–4.13 µs) |

The index itself is worth **roughly 360x** over our own full scan on point
probes and **roughly 100x** on range scans (AHL-423; the harness's own
per-run figures were 356x/358x/361x and 105x/87x/103x, against ~360x/~100x
the edition before — both multiples are now flat, the unindexed rows having
made their move an edition ago). **This table is looser than the
previous edition's on the range row and tighter on the point row**: the
indexed point probe held within 4% on ops/s and 2% on p50 across the three
runs and is in no part of this run's ≥10% list, while the range row is in
it on both columns (ops/s 18%, p50 23% — the second run read 98,696 at 9.63
µs where the other two read 118–120k at 7.79–8.00). **On point probes we
beat journal-mode SQLite by roughly 1.8x** (1.84x median of medians; 1.79x,
1.80x and 1.91x per run; the edition before read roughly 1.8x too) **and
trail WAL-mode at roughly 0.6x** (0.61–0.66x, essentially flat). **Range
scans we still lose, by the same margin as last edition — roughly 0.8x of
journal and roughly 0.5x of WAL** (0.72–0.83x and 0.42–0.52x per run on
ops/s; on p50, 6.67 against 8.00 µs is 0.83x), against 0.81x and 0.5x the
edition before and 0.67x/0.4x the one before that.

**Both InlaySQL rows are flat against the previous edition, and one of them
has an interleaved A/B that says they should not be.** The point probe went
481,918 → 473,401 ops/s (−2%), 1.83 → 1.92 µs (+5%); the range row 119,219
→ 118,489 (−1%), 8.25 → 8.00 µs (−3%), with SQLite's own four cells flat
beside them (journal point 264,677 → 257,670, −3%; journal range 143,954 →
144,622, +0.5%; WAL point 765,189 → 762,089, 0%; WAL range 237,624 →
235,641, −1%) — so the sitting is not where anything came from either. What
`PERF.md` measured on the range shape is **AHL-551**, and it is not
nothing: the retained descent path, resumed from the lowest ancestor whose
separator span still admits the key, read `indexed-range` **6 of 6 ahead
and non-overlapping, +3–7%** (130.6 / 128.3 / 131.3 / 128.9 / 127.9 /
130.9k → 132.7 / 137.0 / 135.4 / 132.7 / 134.8 / 136.5k ops/s), interleaved
against a `0ce0953` binary with the control re-run every repetition and the
order alternated, and AHL-553's own read-shape control put `indexed-range`
at 136.0 / 136.4 / 134.1k against 137.4 / 137.1 / 136.8k, flat. The gated
median moved −1% on the same column, and −3% in the right direction on p50.
**A 3–7% effect cannot be seen by a row whose own three runs disagree by
18% on ops/s and 23% on p50**, and this document does not claim to have
seen it: the sign on p50 matches the A/B and the size does not separate,
which is recorded rather than resolved. It is the same two-instrument
disagreement the point-read section describes, at the scale where the
gated instrument simply has no resolution. `indexed`'s own A/B was flat by
design (AHL-551's `points`-adjacent bookkeeping had to not cost anything on
a hit), and the gated point row is flat, which is the one place the two
instruments agree without argument.

What the range loss is now, per AHL-550's after-profile as narrowed by
AHL-551: `_platform_memcmp` 27% (the descent), `get_from` 6%, `from_utf8`
5% (the item AHL-550 (a) built, measured flat and did not land),
`decode_value_ref` 5%, `decode_row_ref_masked_into` 4%, and the compiled
filter itself 7% — the per-row work of the entry walk, the same family as
the `LIMIT` join loss below. AHL-551 removed *comparisons* from that walk,
not the cost of each one, and AHL-554 then tried to make each one cheaper —
a comparator that reads a row key's eight-byte big-endian tail as a `u64`
rather than byte by byte, unconditionally identical in verdict to `Ord for
[u8]`, wired into every comparison the profile names. It was measured
across all seven suites interleaved and **reverted**: every suite's ranges
overlapped, and the self-time profile's real ~14% relative drop in `memcmp`
share did not reach the clock, because the shortened prefix compare still
calls the platform `memcmp` and Apple's is already near-optimal at these
lengths. `PERF.md` carries the whole measurement, and the item it leaves
standing is proving the shared prefix once per descent rather than once per
comparison.

### Joins — we win one shape, lose the other

20,000 users × 160,000 posts, identical schema and indexes on both sides
(`SUITE=joins`). Each row splits the cold first execution of the query shape
from the warm p50 — the cold column is where the join plan and its tables get
built, so it is the expensive one:

**Regenerated this edition at `be95cc3`** (`SUITE=all REPEATS=3`, median of
three, same three runs as every table above, quiet-machine gate passed
throughout and no run marked `CONTAMINATED`; raw:
`bench/results/20260905T020058Z-repeat.txt`). The joins harness did not
change — this table is the same measurement as the 3rd's with a different
binary, and so is every table above it.

| Query shape | InlaySQL cold → p50 (median) | SQLite journal cold → p50 (median) | vs journal |
| --- | --- | --- | --- |
| PK inner, full join | 21.44 ms → **3.26 ms** | 11.50 ms → 10.37 ms | **~3x faster** |
| PK inner, LIMIT 10 | 44.67 µs → 3.46 µs | 7.92 µs → 3.38 µs | ~1.15x slower |
| Secondary-index inner, full | 30.84 ms → **3.51 ms** | 30.53 ms → 30.62 ms | **~8x faster** |
| Secondary-index inner, LIMIT 10 | 57.83 µs → 5.54 µs | 22.83 µs → 4.38 µs | ~1.3-1.4x slower |

The last column is the harness's own throughput ratio (joins/s against
joins/s), the median of the three runs' own lines: 2.97x, 3.26x and 3.08x
for the PK inner full join; 7.97x, 8.66x and 8.05x for the secondary-index
full join; 1.18x, 1.15x and 1.13x slower for the PK `LIMIT` shape; 1.34x,
1.26x and 1.39x slower for the secondary `LIMIT` shape. **Every InlaySQL
column in this table is tight this edition**: the p50s held to 3.15–3.27 ms
and 3.51–3.64 ms on the two full shapes (4% each) and 3.46–3.50 µs and
5.54–5.71 µs on the two `LIMIT` shapes (1% and 3%), and the two full-join
`joins/s` cells, which were 12% and 14% last edition, are 5% and 4% here —
so for the first time none of InlaySQL's eight cells is in this run's ≥10%
list. What is: every `cold` cell (single samples, as always), SQLite
journal-mode's two full-join rows (p50 12% and 13%, joins/s 10% and 12% —
its second run read 11.55 ms and 34.26 ms where the other two read
10.32–10.37 and 30.39–30.62), and both SQLite WAL `LIMIT` rows, of which
the secondary one is the loudest cell on this page (joins/s 83%, p50 37% —
the runs read 160,331 / 309,518 / 416,306 joins/s at 3.42 / 3.08 / 2.29 µs,
with one 238.83 µs max sample in the first).

**Both `LIMIT` rows moved down again, and the instrument that predicted it
disagrees with the size.** 3.75 → 3.46 µs (−8%) and 5.79 → 5.54 µs (−4%),
both outside this sitting's own spreads (1% and 3%), against SQLite's own
3.54 → 3.38 µs (−5%) and 4.33 → 4.38 µs (+1%) on unchanged code — so about
half of the PK row's move is a sitting that moved SQLite the same way, and
the secondary row's is not. The name on the diff is **AHL-551**: a point
lookup now keeps the path it descended — up to eight `PathStep`s, each an
`Rc<Node>` refcount bump on a node the page cache already holds decoded,
plus the two separators bounding that node's subtree, no key bytes copied
and nothing heap-allocated — and a leaf miss climbs to the lowest retained
ancestor whose span still admits the key instead of walking from the root.
The premise was measured before the code was written, and it corrected the
belief this section carried last edition: AHL-549b's text said the ten
probes are "ten consecutive keys that live in one leaf", and a counter says
otherwise — on the PK shape four of ten lookups start from the retained
leaf, two from the root and **four from a deeper ancestor**; on the
secondary shape, whose row ids are 20,000 apart by construction, *none*
start from the leaf and half start from a level-1 or level-2 ancestor. In
the tree's own tests an ascending sweep of 5,000 keys is 4,501 leaf hits
and 499 crossings, and all 499 crossings resume from an ancestor, none from
the root; disable `PointCursor::deepest_admitting` and the identical run
reports 499 root descents and zero resumes, which is what makes that test a
measurement rather than a restatement. On the published bench's own two
shapes, interleaved against a `0ce0953` binary with the control re-run each
repetition: PK `LIMIT 10` **3.46 / 3.50 / 3.42 → 3.38 / 3.33 / 3.29 µs,
ahead 3 of 3 and non-overlapping**; secondary `LIMIT 10` 5.83 / 5.63 / 5.58
→ 5.96 / 5.58 / 6.63 µs, flat, with the 6.63 run also losing 13% of its
throughput. **Here is the disagreement, stated rather than smoothed:** this
gated median reads 3.46 µs with runs of 3.46 / 3.50 / 3.46 — which is
exactly where the A/B's *control* arm sat, not its post-fix arm — while the
edition this replaces published 3.75. So the gated instrument sees a move
about twice the size of the A/B's on a row where the A/B's own two arms are
9 ns apart from where this row now sits, and the two cannot both be
describing the same 3%. What is not in doubt is the direction on both
shapes and the mechanism's own profile: `bin/profile --suite joins-limit`
over 17,687 samples puts `Storage::get_row` → `CowBTree::get_from` at
**35.5%** of the query, down from 37.3%, of which the walk itself is 24.3%,
with `_platform_memcmp` 19.6% self (was 20.9%) and the new bookkeeping
visible and smaller than what it removes (`CursorPath::admits` 3.0% self,
`CursorPath::record` 1.8%).

The full shapes: 3.25 → 3.26 ms (0%) and 3.63 → 3.51 ms (−3%), both inside
their own 4% spreads, with SQLite's own rows at 10.33 → 10.37 ms (0%) and
30.76 → 30.62 ms (0%). Nothing in `1f7921a..be95cc3` touches the hash inner
side an unlimited join over 20k users takes, and `PERF.md` ran no A/B on
this suite in the range beyond AHL-551's `joins` control, which was flat
with one run lost to load. Recorded as flat and unattributed, which is what
the previous edition said this row would settle into.

**Both full joins win, and the reason is one commit, found by this
benchmark in an earlier edition.** The story is told in full in
`PERF.md`'s AHL-524 section and summarised here because the table it
corrected was a published winning row. AHL-512 (`894ecef`, cost-based join
reordering) landed inside `2cb2539..7b20175`, and its cost model priced a
hash-built inner row at twice an outer row, which made the planner drive
`users JOIN posts` from the 160,000-row side and build the 20,000-row one —
140,000 extra probes at roughly 70 ns each. Its own measurement was a
suite-level "1.31x on joins" from a profile that cycles all four shapes in
one number, so the PK-inner win hid the secondary-inner loss. The full
regeneration at `7b20175` caught it: the secondary-index full join,
published at 3.71 ms from `2eeced7`, read **14.03 ms**, a 3.8x regression,
and that edition withheld this table rather than publish a number the code
should not produce. AHL-524 (`OUTER_ROW_COST = 4`, so an outer row is
charged on both paths and the smaller table drives) is the fix; the bisect
and the single-run measurement at the fix (3.21 ms / 3.47 ms, gate off) are
in `PERF.md`, and the four gated sittings since have landed both full
shapes at 3.56/3.78, 3.23/3.49, 3.25/3.63 and 3.26/3.51 ms — the same
users-driving plan for both, which is why they now sit within 8% of each
other having been 11.72 ms and 3.71 ms at `2eeced7`.

The full-join ratios' history, for the record: the secondary-index inner
shape — the one AHL-464 built the index nested-loop join for — went from
**10.71x slower** (2026-08-20) to 2.85x faster (`9aba437`) to 3.65x faster
(`9b2f11e`, AHL-447) to roughly 8x (2026-08-30) to roughly 7.5x (`2eeced7`)
to a withheld 2.2x (`7b20175`, the regression above) to roughly 8x
(`4f8e5dd`) to roughly 7-8x (`3cf0d85` and `1f7921a`) to **roughly 8x**
here (8.05x at the median, 7.97x at its floor) — and its p50 is 3.71 → 3.78
→ 3.49 → 3.63 → 3.51 ms across the five published editions after the
regression. The PK inner full join went from 5.56x slower to 1.43x to 1.20x
to roughly 1.1x to roughly 1.15x slower (`2eeced7`) to 1.17x faster in the
withheld run to roughly 3x faster at `4f8e5dd`, `3cf0d85`, `1f7921a` and
**roughly 3x** here — 11.72 → 8.77 → 3.56 → 3.23 → 3.25 → 3.26 ms — and the
step to 3.56 is the genuine, attributed improvement: it is the shape the
corrected reorder moves *into* users-driving, on top of AHL-522's
read-ahead window, which `PERF.md` measured at 1.17x on the full-scan join
shapes interleaved (AHL-521's page-cache hash was flat on them — its win is
on the `LIMIT` shapes). SQLite's own PK-inner p50 read 9.99 ms at
`2eeced7`, 11.03 ms in the afternoon of the 2nd, 10.42 ms that evening,
10.33 ms on the 3rd and 10.37 ms here (10.32–11.55 across the three runs),
a 6–11% band on code that did not change, which is the size of the
sitting-to-sitting noise to hold against every ratio in this table.

**The `LIMIT` rows are still a loss, and on the PK shape the two bands have
just come apart again.** 1.15x and 1.3-1.4x slower warm, against 1.1x and
1.3-1.5x the edition before, 1.2-1.3x and 1.5-1.6x before that, 1.7x and
1.9x the afternoon before that, 2.0x and 2.1x at `2eeced7`, and 4.7–5.8x
before the raw-leaf cache. On p50 alone the PK gap is the narrowest it has
been — 3.46 against 3.38 µs is 1.02x, from 1.06x — but the previous
edition could say the two run-to-run bands touched and this one cannot: our
3.46–3.50 µs sits wholly above SQLite's 3.33–3.38, three runs of each,
because both sides got faster and SQLite's own row is the tighter of the
two. That is a *narrower* gap described *more* firmly, and it is stated
that way rather than as either parity or a widening. The secondary shape is
a real loss still, 5.54 against 4.38 µs. A `LIMIT` shape is never reordered
(AHL-525 reorders one only under an `ORDER BY`), so AHL-524 has no part in
these two rows, and what is left after AHL-551 is in the profile above: the
eleven points between `get_from` (35.5%) and the walk itself (24.3%) are a
page-cache lookup for a leaf the cursor could simply have held, which is
the next slice and was deliberately not bundled; and `_platform_memcmp` at
19.6% self is the cost of each comparison rather than the number of them,
which is the ceiling AHL-554 tried and failed to lower.

### Durable writes — we win

One row per commit, one `fsync` per commit. Median of three runs, same
session as the tables above.

| Engine | ops/s (median, range) | p50 (median, range) |
| --- | --- | --- |
| **InlaySQL** | **245** (244–247) | **3.94 ms** (3.93–3.95 ms) |
| SQLite, journal + `sync=FULL` + `fullfsync` | 88 (88–104) | 11.19 ms (9.87–11.22 ms) |

**~2.8x** (2.78x, 2.79x and 2.38x, the harness's own per-run lines) — up
from the previous edition's ~2.5x, and once again the move is SQLite's: 99
→ 88 ops/s (−11%, 10.27 → 11.19 ms) on code that did not change, with its
own three runs at 88, 88 and 104 (18% spread, in this run's ≥10% list
beside its p50 at 12% — the third run read 9.87 ms where the other two read
11.19–11.22), while InlaySQL went 248 → 245 (−1%, 3.91 → 3.94 ms), inside
its 1% spread. **InlaySQL's side is flat, and this edition there is one
commit on its path to say that about.** AHL-553 put `FileDevice::extend_for`
into the commit's write path — a write at or past the data-area boundary
extends the file in geometric 1–8 MiB chunks and zero-fills the extension
before the tree writes into it — and it is a container-volume change: on
the macOS host, where `F_FULLFSYNC` is 99% of a commit, `PERF.md`'s own
four-arm host A/B is flat to a small loss (`base` 263.2 / `sparse` 250.5 /
`filled` 260.2 ops/s at 300 commits; the landed `chunked` design 209.5
against `base` 242.1 at 200, on the noisiest arm it ran). This row is the
host, it moved −1%, and the sign of the host A/B is disclosed rather than
buried: a real few-per-cent host cost would be invisible here and is not
ruled out. Five sittings have now put this row at 241, 256, 243, 248 and
245 ops/s, so its real sitting-to-sitting band is roughly 240–260 — tighter
than any other InlaySQL row's, and still wider than any one sitting's own
spread says. Still the most stable row in this document across editions:
the commit gate no longer re-derives the log on every commit (AHL-468),
which paid on the solo path too.

**Batching lifts the same workload to 227,431 ops/s (219,817–252,714, 15%
spread) at 1.75 µs (1.42–1.75 µs) — roughly 900x (1037x, 927x and 889x per
run) — which is the number to quote for a bulk load and not for a
transaction.** Against the previous edition's 227,261 at 1.67 µs that is
+0.1% on ops/s and +5% on p50: **flat, and the 4x step it took an edition
ago holds.** That step had a name and keeps it, **AHL-542** (C7): this row
inserts rows one prepared statement at a time inside one transaction until
the engine says the log would overflow, then commits and begins again —
thousands of rows per commit — and until AHL-542 a transaction's dirty
pages existed only as bytes between statements, so every insert decoded its
leaf and its path, mutated them and re-encoded them, and the next statement
decoded them again. Now the pages stay decoded until the commit encodes
each exactly once. `PERF.md` measured that at **1.29–1.44x, 3 of 3,
non-overlapping** on the hundred-row `INSERT ... VALUES (...) x100` shape
(19,487 / 19,508 / 16,835 → 25,123 / 26,110 / 24,289 rows/s); this row's
own shape amortises its `fsync` over far more rows and moved 4.0x when the
commit landed. This row is in the ≥10% list on both columns again (p50 19%,
ops/s 15%), because at 1.4–1.8 µs per row the loop is short enough to feel
the machine. AHL-552, which changed what the cache holds after exactly this
kind of batched load, measured the engine's own `points` and `aggregate`
suites flat and +5% respectively and was not A/B'd on this row; AHL-553's
zero fill is on its write path and is a host-side cost if it is anything.
Neither is attributed here, and neither needs to be: the row did not move.

Every row above is full-durability, on both sides of every comparison, on
purpose — an opt-in relaxed-durability tier also exists
(`EngineOptions::durability`) and is measured separately, in `PERF.md`, not
mixed into these tables.

### Concurrent writers — the peak sits at sixteen, and past it the win still shrinks

200 transactions per writer, one row each, on real OS threads. Median of
three runs at `be95cc3` (`bench/results/20260905T{020058,020830,021602}Z.txt`,
the default `WRITER_LEVELS` of 1/2/4/8, load 1.2–3.8/18 throughout, gate
passed). The eleven-level wide sweep and the tail-latency table further down
were **not** re-run in this sitting and are carried forward from the
2026-08-30 sweep at `2cb2539`, as each says in place — so this page again
carries two concurrency sessions.

| Writers | InlaySQL commits/s (median, range) | SQLite commits/s (median, range) |
| --- | --- | --- |
| 1 | 259 (253–263) | 87 (85–87) |
| 2 | 390 (376–436) | 88 (87–88) |
| 4 | 510 (500–524) | 88 (88–89) |
| 8 | **1145** (1035–1146) | 89 (88–90) |

**Roughly 13x SQLite at 8 writers (11.8-13.0x across this run's own three
pairings, median 12.9x), 0.0% aborted — against 13x in the previous
edition, 14x the evening before that, 14-15x the afternoon before, 13.2x
that morning and 13.7x in the 2026-08-30 wide sweep, all up from 8.1x
before the adaptive gather window (`94d96a6`, unchanged since).** The
8-writer InlaySQL row is 1145 here against 1184 the edition before (−3%),
1228 (−7%), 1347 (−15%), 1148 (0%) and 1209 in the wide sweep (−5%) —
inside this run's own 10% spread and squarely inside the roughly
±10%-around-1200 band the last three editions said to quote. **It is
unattributed, and the commit path did change under it again**: AHL-553's
`extend_for` runs on every writer's first write past the data-area
boundary, and `PERF.md` measured it flat to a small loss on the macOS host
(the durable-write section has the arms) and ~1.18x on a container volume,
which is not this table's platform. AHL-544 and AHL-547's commit-side
absorption remains behind `EngineOptions::commit_absorption`, default
`false`, so this table is still the flag-off engine. Six sessions have now
put this one point at 1209, 1148, 1347, 1228, 1184 and 1145 with the
coalescing window unchanged throughout; the band holds, and the band, not
the 1145, is what to quote. The commit gate's pre-`fsync` gather window
(`coalesce_normal_commits`, `crates/inlaysql/src/device.rs`) keeps
yielding while a normal commit is inflight or waiting and progress keeps
happening, closing on stalled progress instead of a fixed 8-yield count —
see `PERF.md` for the full mechanism, unchanged since it shipped. The
8-writer scaling (1145 against 259 at one writer) is **roughly 4.4x** by
the harness's own line — 4.10x, 4.36x and 4.42x per run — against the
previous edition's roughly 5x, the evening before's roughly 5x, the
afternoon's roughly 5.5x, the morning's roughly 4.7x and the 2026-08-30
sweep's roughly 5x; the one-writer row is the highest this page has
published (259 against 247 and 244) and the eight-writer row the lowest of
the six, which is the whole of that step and is inside both rows' own
bands. **The 2-writer case is the noise measurement it always was**: 390
against 259 is roughly 1.5x (1.48–1.68x per run; commits/s 376 / 390 / 436,
a 15% spread, and p50 4.26 / 5.34 / 4.12 ms, 29% — both still in this run's
core ≥10% list, though far below the 49% and 93% the previous sitting
produced), against 1.5x the edition before, 1.4x before that, 1.6x in the
two sittings before those, 1.25x on 2026-08-30 and 1.60x the edition
before. Seven sessions have now put this one point at 1.60x, 1.25x, 1.6x,
1.6x, 1.4x, 1.5x and 1.5x with no change to the coalescing code between any
of them, and the 8-writer figure — read as a band, above — is the one to
trust. SQLite's own rows sat at 87–89 at every level (85–90 across runs),
on the previous edition's 87–89 and the morning's 85–88, tight to within
3%. The barrier counters behind the rows, medians of three: 1.00
commits/sync at one writer, 1.41 at two, 2.14 at four and 5.23 at eight —
the same shape every edition has published, and the reason the 8-writer row
is 13x a SQLite that syncs once per commit.

**Published because it is true, not because it flatters us: eight writers is
still not the peak.** **Carried forward from the 2026-08-30 wide sweep at
`2cb2539` (`bench/results/20260830T{124155,124632,125240}Z.txt`,
`WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`, load 2.9–3.6/18), not re-run
this edition** — the figures from here to the end of the tail-latency table
below are that sweep's. Its 1/2/4/8 points differ from the fresh table
above by 6%, 28%, 13% and 5% respectively — the 2-writer point being the
noise measurement the paragraph above describes, the other three inside or
at the edge of the overlap of the two sessions' own ranges. All eleven levels (medians;
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
2026-09-05 run stopped at 8 writers and has no 32-writer row to put here.
For the record, that fresh run's own 1- and 8-writer tails (p50 / p95 / p99
/ max, medians of three): 3.82 / 4.28 / 6.95 / 8.08 ms at 1 writer and 4.85
/ 20.91 / 35.07 / 51.94 ms at 8 — the same shape as the rows below, the
8-writer p50 within 0.15 ms of it, p95 1.7 ms wider and p99 4 ms wider,
which is the kind of distance a one-sample tail column keeps between
sessions.

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
above (`bench/results/20260905T{020058,020830,021602}Z.txt`).

| Corpus | recall@10 | p50 (median, range) | vs `sqlite-vec` (median, range across the 3 runs) |
| --- | --- | --- | --- |
| Text-derived embeddings | 1.000 | 63.58 µs (60.33–64.17 µs) | **~10x faster at 100% of its recall** (per-run ratio 9.86–10.47x, median 10.46x) |
| Uniform random | 0.922 | 86.29 µs (85.46–89.71 µs) | ~7x faster at 92.2% of its recall (7.15–7.35x, median 7.35x) |

The multiples are the median of the three runs' own per-run ratios (the
harness's "is Nx faster" lines), not the ratio of the two median p50s; the
two methods agree to within half a turn on the realistic corpus (632.92 /
63.58 = 9.95x against 10.46x) and exactly on the uniform one (634.50 /
86.29 = 7.35x against 7.35x). **This table is the tightest it has been
since the gated editions began**: the realistic InlaySQL p50 held within 6%
across the three runs and the uniform one within 5%, neither in this run's
≥10% list, where the previous edition had the realistic p50 at 12.6% and
its ratio at 12%. Both InlaySQL medians moved down by the same 7% — 68.67 →
63.58 µs and 93.00 → 86.29 µs — which is at the edge of this sitting's own
spreads, and `sqlite-vec`'s own moved 634.00 → 632.92 µs realistic (0%) and
676.46 → 634.50 µs uniform (−6%), so the uniform ratio's 7.04x → 7.35x is
`sqlite-vec`'s column and the realistic 9.17x → 10.46x is ours. `PERF.md`
has no A/B on this suite across `1f7921a..be95cc3` — nothing in AHL-551
through AHL-555 touches `hnsw.rs` or the distance kernels — so the move is
**unattributed**, with one mechanism worth naming and not claiming:
AHL-552 makes the pages a batched load wrote resident when the first read
starts instead of read back through the device one at a time, and that is
the reason its own `aggregate` suite moved +5%, 3 of 3, where every other
suite was flat. Nobody ran the vector suite through that A/B. Six editions'
medians (78.96, 69.54, 75.17, 69.08, 68.67, 63.58 µs) are one figure
measured on six different sittings, so the honest quote is roughly 65-75 µs
and this sitting sits at the bottom of it.

Both corpus shapes are published because only one of them flatters us. Uniform
random vectors in 384 dimensions have no structure for a graph index to
navigate, so recall falls and no amount of tuning fixes it. Text-derived
embeddings are what an application actually stores.

`VECTOR(n, INT8)` quantisation costs 0.014 recall on the realistic corpus
(0.986 vs 1.000 exact) and nothing measurable on the random one (0.922 both)
for a **3.96x smaller resident payload** — all three figures identical across
all three runs and identical to the previous edition's. Its per-query cost at
this scale is 155.29 µs (153.13–163.83 µs) realistic and 242.17 µs
(238.63–243.88 µs) uniform, roughly 2.4x and 2.8x the exact index's p50 (both
int8 rows held within 7% and 2%; the medians moved 153.83 → 155.29 and
243.63 → 242.17 µs from the previous edition's, +1% and −1%, inside the
spreads).

**The file-size ratio moved and it is not about quantisation.** The same
line reads a **2.40x smaller file** here where every earlier edition read
1.65x, and the reason is **AHL-553**: the exact index's file went 8,568,832
→ 12,591,104 bytes and the int8 one 5,185,536 → 5,251,072, because the
commit path now extends and zero-fills the data area in geometric 1–8 MiB
chunks ahead of the writer, and the two databases round up to different
chunk boundaries. Nothing about what int8 stores changed — the resident
vector payload is 2.9 MiB against 0.7 MiB, 3.96x, exactly as it was — so
**the resident figure is the one to quote for what quantisation buys**, and
the file figure is now partly a statement about preallocation. The same
effect is visible in the paged-HNSW spot-check further down (exact
226,361,344 → 230,694,912 bytes realistic, 231,464,960 → 239,083,520
uniform), where it moves those ratios from 2.14x/2.16x to 2.04x/2.11x. This
is the only place on this page AHL-553 changed a published number, and it
changed a size, not a time.

**Spot-checked at scale, `SUITE=quantization DOCS=100000 QUERIES=50`, median
of three runs (`bench/results/20260830T{125800,131326,132715}Z.txt`, load
2.3–4.8/18 throughout) — carried forward from the 2026-08-30 edition at
`2cb2539`, not regenerated this edition; the 2026-09-05 sitting, like the
four before it, ran only the default 2,000-document suite above.** Recall loss widens to 0.028 (realistic, 0.970 vs
0.998) and 0.014 (uniform, 0.104 vs 0.118) — both figures exact and identical
across all three runs (0% spread), a real and fully reproducible finding, not
subject to this section's usual hedging.

The per-query slowdown this document and `PERF.md` diagnosed as structural at
2,000 docs (int8 2.10x slower at that edition; ~2.4x and ~2.8x in the
fresh table above) is gone at 100,000 docs on both corpora — but
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
multiple below at least as loosely as the gated tables above, not more
precisely for coming from an external corpus.

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

2,000 documents, dim 384, `LIMIT 10`. Ingest 16,640 docs/s (median of three
runs, 16,568–17,455; same session as the tables above —
`bench/results/20260905T{020058,020830,021602}Z.txt`).

| Workload | p50 (median, range) | p95 (median) | Previous edition (`9aba437`) |
| --- | --- | --- | --- |
| Vector only | 60.33 µs (59.54–61.13 µs) | 69.25 µs | 87.88 µs |
| BM25 only | **45.96 µs** (45.79–46.00 µs) | 59.38 µs | 347.50 µs |
| Hybrid (fused) | **93.83 µs** (93.29–94.83 µs) | 109.83 µs | 453.88 µs |

Hybrid is **one SQL statement**, not two queries and a client-side merge.

BM25 fell **roughly 7.5x** and hybrid **roughly 4.8x** against that
historical baseline (this session's own three runs give 7.55–7.59x and
4.79–4.87x — the tightest this table has ever been on both: BM25's p50 held
within 0.5% and hybrid's within 2%, and for the second edition running
neither is in the ≥10% list). Against the previous edition (49.42 / 97.83
µs) both medians are 4-7% *lower*, so six editions now read 51.21, 46.67,
50.50, 46.42, 49.42 and 45.96 µs on BM25 with no code touching `bm25.rs` in
any of the ranges between them and no A/B in `PERF.md` on this suite across
`1f7921a..be95cc3`: the move is unattributed, the band is roughly 46–51 µs,
and the ratio against the fixed `9aba437` baseline is roughly 7.5x on BM25
and roughly 4.8x on hybrid, to one digit. The ingest figure moved 17,527 →
16,640 docs/s (−5%), within its own 5% spread this time where the previous
edition's was 22% — a short phase that reads the machine, not attributed.
**The vector leg is the one row here that moved further than its own
spread**: 74.04 → 60.33 µs (−19%) against a 3% spread, and unlike the
previous edition it *is* mirrored by the `sqlite-vec` section's exact-HNSW
row above, which fell 7% on the same index code in the same runs. Six
editions have put this leg at 87.88, 74.67, 66.25, 74.04 and 60.33 µs. It
is unattributed for the same reason that row is — no A/B in the range ran
either suite — and the one mechanism in the range that could touch both is
AHL-552's resident post-load leaves, named in the `sqlite-vec` section and
not claimed here either. The underlying rewrite is still code, from the
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

BM25 was 79% of the hybrid p50 before this; it is now 49%, and the vector leg
is the larger half. Per-block impact bounds (block-max WAND) are the next step
and are not implemented.

---

## Against DuckDB, pgvector and Meilisearch

One corpus, one set of queries, one exhaustive ground truth, each engine asked
for its own query plan so an unindexed row cannot masquerade as an indexed one.
5,000 documents, dim 128. **Regenerated 2026-09-02/03 at `bdc64eb` via
`REPEATS=3 ./bench/repeat-compare.sh` — the first gated, repeated edition of
this table** (every earlier one was a single ungated pass): median of three
complete runs, load sampled every 5 s through the measured phases (0.85–3.05
of 18 across the three, none `CONTAMINATED`), the same four unrelated Docker
containers present and idle throughout as in the last edition
(`hkjc-citywide-redis`, `hkjc-citywide-db`, `linkmonitor-app-1`,
`estate-ops-postgres` — none touched). Raw output:
`bench/results/20260902T185304Z-repeat-compare.txt`, from
`bench/results/20260902T{185718,190221,190724}Z-compare.txt`.

| Engine | recall@10 | vector p50 (median, range) | hybrid p50 (median, range) |
| --- | --- | --- | --- |
| **InlaySQL** (HNSW + BM25) | 1.000 | **129.00 µs** (88–134 µs) | **192.00 µs** (156–196 µs) |
| DuckDB (exhaustive + fts BM25) | 0.999 | 4.79 ms (4.79–4.79 ms) | 11.93 ms (11.76–11.93 ms) |
| DuckDB (vss HNSW + fts BM25) | 0.993 | 4.01 ms (3.96–4.02 ms) | 11.14 ms (10.97–11.53 ms) |
| Meilisearch (`arroy` ANN + its own ranking) | 0.999 | 1.18 ms (1.16–1.19 ms) | 4.04 ms (4.00–4.11 ms) |
| pgvector (HNSW + `ts_rank`) | 0.987 | 148.00 µs (146–187 µs) | 13.38 ms (13.29–13.52 ms) |
| pgvector (exhaustive + `ts_rank`) | 0.999 | 479.00 µs (478–482 µs) | 13.81 ms (13.60–13.85 ms) |

**Hybrid is roughly 20x** the nearest baseline (4.04 ms, Meilisearch; 21x at
the medians, the same ~20x the last two single-run editions found) and
**roughly 60-70x** DuckDB/pgvector (58–62x DuckDB, 70–72x pgvector at the
medians; was "~55-70x" from a single run). What the repeat adds is the
spread, and it is lopsided: every baseline's p50 held within 0–4% across
three runs, while InlaySQL's own vector p50 moved 88 → 129 → 134 µs (36%,
on the ≥10% list) and its hybrid p50 156 → 192 → 196 µs (23%). The
published medians are the *slower* two of three on both InlaySQL cells;
the ratios above are computed on them, not on the fast run. Against the
last edition's single run (135.00 µs / 198.00 µs) the medians moved 4% and
3% — inside the spread just measured, so not a move at all. It is still
not one query against one query — it is one statement here against two
queries plus client-side rank fusion there, Meilisearch included — and
`bench/README.md` says so plainly.

**Vector-only against pgvector is a tie at the medians, not a win.** 129 µs
against pgvector-HNSW's 148 µs (both include pgvector's socket round trip a
library in your own process does not pay); per run the pair read 88 vs 148,
129 vs 146 and 134 vs 187 µs — InlaySQL ahead in all three, by 1.1x to
1.7x, on a row whose own spread is 36%. The previous single-run editions
put the pair at 135/147 and 126/152 µs and called it "close rather than a
rout"; with a spread finally measured, "close" is the finding and the 1.1x
median gap is inside it. Against Meilisearch's 1.18 ms it is not a fair
fight in InlaySQL's favour so much as a different product: Meilisearch's
ANN search also runs its own typo-tolerance and ranking pipeline, which
pgvector's raw `<=>` operator does not. Meilisearch's `agree` (0.419) sits
in the same range as pgvector's `ts_rank_cd` rows (0.456/0.465) for the
same reason both are below DuckDB's real BM25: neither ranks text with BM25
at all. Recall is the one column that did not move: every engine's
recall@10 landed within 0.004 of its last-edition figure.

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

**Regenerated 2026-09-02/03 at `bdc64eb` — gated, repeated, and against
MySQL 8.4 for the first time.** Every earlier edition of this table was a
single ungated `compare.sh` pass, and the last two editions carried it
forward from `b4798ce` (2026-08-30) on the reasoning that a fresh single
sequential run would be a worse measurement than the interleaved rerun
already done for it. This edition retires that reasoning by doing what it
asked for: `REPEATS=3 ./bench/repeat-compare.sh`, load gate on, every
sample under the 4.5 ceiling (per-run max 2.49/3.05/2.77 of 18), none
`CONTAMINATED`, 30 s cooldown between repetitions, median of three
published with each run's own figure. **Two things changed underneath the
MySQL column at once**: the container is now `mysql:8.4` (LTS) where every
earlier row was 8.0.x (`e7cc895`), and it runs `--innodb-buffer-pool-size=512M`
(the 2026-08-31 tuning fix above) where the `b4798ce` row did not. So no
MySQL edition-to-edition move here is attributed to anything. `bdc64eb`
now sits two `run.sh` editions behind the tables above it: the
`3cf0d85..bdc64eb` engine range (AHL-538 through AHL-542) touches scans,
aggregates and the batch-insert write path, and `bdc64eb..be95cc3`
(AHL-544 through AHL-553) the insert path, the join probe, the residual
filter, the descent cursor and the decoded page cache, plus commit-side
absorption behind a default-off flag. One of those *is* on this section's
write path and is now known to move it: **AHL-553 stops a commit's barrier
paying to grow the file, and `PERF.md` measured it at ~1.18x on a
containerised single-row durable commit, 11 of 12 interleaved repetitions
on this very volume class** — so the containerised InlaySQL write row below
is measured on a build that predates it and is expected to be better than
published, by an amount nobody has remeasured. The host row is not affected
(the same A/B is flat to a small loss on `F_FULLFSYNC`). Otherwise nothing
in either range was measured to move the single-row durable commit
(`PERF.md`: `writes` flat across AHL-542 and AHL-545) or a point read
(every `points` control in both ranges flat, and AHL-552's own gain is in
the tail of a *read phase that follows a write phase*, which this driver's
read column does not have in the same shape). The interleaved rerun of 2026-08-30 stays below as one paragraph of
history; the "Correction" stays because its transport-tax accounting is
still the right way to read this table.

**Reads: we win by a wide margin, and the margin against PostgreSQL is
smaller than any earlier edition found. Sequential writes: we lose to both.**

InlaySQL is measured twice — on the host with a real `F_FULLFSYNC` barrier,
and **inside a container on the same volume class as the servers**, so all
three pay the same virtualised fsync. The gap between the two InlaySQL rows is
what that virtualisation is worth on this machine.

| Engine | write ops/s (median; runs) | read ops/s (median; runs) | read p50 |
| --- | --- | --- | --- |
| InlaySQL, host (real `F_FULLFSYNC`) | 246.8 (242.6 / 246.8 / 248.8) | 1,028,190 (1,306,066 / 423,297 / 1,028,190) | 1 µs |
| InlaySQL, containerised | 619.8 (600.8 / 619.8 / 622.1) | **704,742** (576,889 / 861,858 / 704,742) | 1 µs |
| MySQL 8.4 (`innodb_flush_log_at_trx_commit=1`, binlog off) | **910.3** (974.0 / 814.0 / 910.3) | 10,498 (10,503 / 10,498 / 10,488) | 95 µs |
| PostgreSQL 17 (`fsync=on`, `synchronous_commit=on`) | 762.8 (762.8 / 819.6 / 707.3) | 58,415 (47,502 / 58,415 / 68,177) | 14 µs |

Commits-per-fsync, bracketed around the write phase: MySQL 0.97 (0.96–0.98),
PostgreSQL 1.00 in all three — one durable barrier per commit on both
servers, as the single-connection shape requires.

**Reads: ~67x MySQL 8.4 and ~12x PostgreSQL 17 at the medians**,
containerised — an in-process library against a socket round trip, an
asymmetry that is structural and stated, not hidden. Across the three runs'
extremes the pair is 55–82x and 8–18x. The last edition's single run read
~74x/~35x (678k against 9.2k and 19.4k); the MySQL side is unchanged in
kind (its read column held within 0.2% across three runs — the tightest
cells on this page), and the whole of the narrowing against PostgreSQL is
PostgreSQL's own column moving 19.4k → 58.4k (p50 14 µs, 47.5k–68.2k
across the runs). Nothing in `b4798ce..bdc64eb` touches the PostgreSQL
OLTP driver's read path (the diff adds commits-per-fsync bracketing to its
write phase), the image and `shared_buffers` are unchanged, and the
2026-08-30 figure was a single run under eleven idle containers, so the
move is recorded as unattributed rather than explained. InlaySQL's own read
cells are the loud ones: the host row spans 423k–1,306k (85%, the widest
ops/s cell in the repeat) and the containerised row 577k–862k (40%) — the
same binary, same data, three runs — which is exactly the shape the
point-read section above found on `run.sh` the same evening, and why a
reader should hold the *tens-of-x* and not the two digits.

**Writes: we lose to both — MySQL 8.4 by ~1.5x, PostgreSQL by ~1.2x — and
which server leads is not settled.** 619.8 against 910.3 (per run 1.62x,
1.31x, 1.46x — MySQL ahead 3 of 3) and against 762.8 (1.27x, 1.32x, 1.14x
— PostgreSQL ahead 3 of 3). MySQL led PostgreSQL in two runs of three here,
where PostgreSQL led MySQL 8.0 in five of five interleaved repetitions on
2026-08-30 and 1.90x/1.39x in the `b4798ce` single run; with the MySQL
version and its buffer pool both changed in between and the two servers'
own per-run write figures spanning 814–974 and 707–820, the ordering is
noise and the fact that we trail both is the finding. Our own containerised
figure moved 849.7 → 619.8 and the host figure 253.2 → 246.8 — the host
row is the one with a real barrier under it and it did not move; the
containerised drop is most plausibly the volume's virtualised fsync reading
dearer that night than on 2026-08-30 (the "Correction" below measured that
same cost drifting 1.5–1.8x *within* a session) rather than a commit-path
change, since nothing in the intervening engine range touches the commit —
an inference from the host row holding still, not a measurement of the
volume that night. What is structural, and
unchanged: this workload is one commit at a time on one connection, so
group commit cannot fire by design, and the remaining cost is per-commit
against InnoDB's redo write. The host-versus-container gap is 2.5x
(246.8 vs 619.8) — that is what the host's `F_FULLFSYNC` costs against the
volume's barrier on this machine that night, and it is the number the
batch-insert section further down needs. **One thing about the
containerised row is now known and not remeasured**: AHL-553 landed after
this sitting and stops a commit's barrier paying to grow the file, worth
~1.18x on exactly this shape and volume class in 11 of 12 interleaved
repetitions (`PERF.md`, 2026-09-04), so 619.8 understates the engine as it
stands today — by how much, nobody has run the driver again to say, and no
number is invented here. The concurrent-writer story (the
"Concurrent writers" section above) has its server-to-server counterpart in
the 1/8-connection table below and the 1/4/16 sweeps after it.

### Correction (2026-08-30): this table is not apples-to-apples, and the asymmetry favours InlaySQL

*History, kept because the accounting is still right: every figure in this
subsection is the `b4798ce` (2026-08-30, MySQL 8.0.x) edition's — 849.7 /
1,184.2 / 1,612.8 write ops/s — not the table above. The transport
tax it measures and the fsync-share it profiles apply unchanged to the new
numbers.*

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

### Interleaved, repeated, quiet-machine rerun (2026-08-30): history, superseded by the gated repeat above

Kept as one paragraph because it is what taught this page how to measure
the table above, and because the 2026-08-31 `SCOREBOARD.md` cells cite it.
At `b4798ce`, against MySQL 8.0.x at its stock buffer pool, one repetition
was InlaySQL (containerised), MySQL and PostgreSQL run back to back against
warm containers, followed by a control — a raw `pwrite`(80 KiB)+`fsync`
loop on `inlaysql-bench-floor-data`, a named Docker volume of the same
`local`-driver class as the engines' — repeated 5 times, manually
load-gated (two attempts discarded mid-run at 1-minute loads past 80 and
150, redone under 4), `ROWS=20000 LOOKUPS=5000`. Write ops/s, median
(min–max) over the five: floor control 985.2 (854.0–1,005.6, 15.4%
spread); InlaySQL containerised 698.9 (557.0–909.5, 50.4%); MySQL 8.0
1,002.3 (722.9–1,535.9, 81.1%); PostgreSQL 17 1,265.7 (954.2–1,621.5,
52.7%) — PostgreSQL 1.81x and MySQL 1.43x ahead at the medians, PostgreSQL
ahead of MySQL in 5 of 5 repetitions, and the floor probe's spread far
smaller than any engine's with only a weak correlation to each (Pearson r
+0.51 / +0.46 / −0.51, n=5), so most of the run-to-run noise was not the
volume but the Python driver, `docker exec` jitter or the bridge network.
A single transport-matched bonus run (`server_driver.py`, one connection,
load ~3.8/18) put InlaySQL at 627.6 against MySQL's 849.4 ops/s (0.74x) —
the opposite direction from the "Correction" above's prediction, and too
thin to settle it. Raw: `bench/results/20260830T095714Z-interleaved-oltp-
compare.txt` and `bench/results/20260830T095714Z-rep{1..5}-{inlaysql-
container,mysql,postgres}.json` (git-ignored, cited for traceability).
What it recommended and what happened to the recommendation: an automated
load gate and a repeat wrapper for `compare.sh` (both landed 2026-08-31,
both used for the first time in the table above), and interleaving the
engines within one pass (still not done — `compare.sh`'s phase order is
fixed, so that repeat is three sequential passes, gated and
cooled-down, not an interleaved one).

### Server-to-server: MySQL wire protocol

`inlaysql serve --mysql` reached over the compose network by `mysql.connector`,
matched against MySQL 8.4, same driver and same transport on both sides. Every
row pays a socket round trip.

**Regenerated 2026-09-02/03 at `bdc64eb`, as the last phase of the same
`REPEATS=3 ./bench/repeat-compare.sh` sitting as the two tables above** —
the first repeated, gated edition of this 1/8-connection table. The
process-based driver (`f8e29e9`, 2026-08-27: each connection a spawned OS
process, not a Python thread, so `mysql.connector`'s GIL cannot be in
these numbers) is unchanged; what changed underneath is the MySQL
container (8.4 LTS, `innodb_buffer_pool_size=512M`, where the 2026-08-29
table it replaces was 8.0.x at stock) and that both engines' own
commits-per-fsync counters are now bracketed around every level's write
phase (`Inlaysql_normal_commit_tickets`/`_flushes` on our side,
`Handler_commit`/`Innodb_os_log_fsyncs` on MySQL's — live since
2026-08-31). The workload is the driver's default — 2,000 durable
single-row writes per connection level (the bracketed commit counters,
2,000–2,044 per level, are the check; the raw file's header line prints
the OLTP phase's 20,000/5,000, not this phase's) — the same shape every
earlier edition of this table used.

| Engine | Connections | write ops/s (median; runs) | write p50 / p99 | read ops/s (median; runs) | commits-per-fsync |
| --- | --- | --- | --- | --- | --- |
| **InlaySQL** (`inlaysql serve --mysql`) | 1 | 668.9 (663.2 / 668.9 / 694.6) | 1.38 ms / 3.35 ms | **10,292.4** (9,627.5 / 10,292.4 / 10,772.5) | 1.00 |
| **InlaySQL** (`inlaysql serve --mysql`) | 8 | 1,522.2 (1,397.6 / 1,522.6 / 1,522.2) | 2.78 ms / 22.30 ms | 9,067.7 (8,956.7 / 9,067.7 / 10,384.4) | 4.06 (3.98–4.10) |
| MySQL 8.4 | 1 | 1,041.8 (724.5 / 1,041.8 / 1,206.8) | 0.92 ms / 1.90 ms | 8,789.2 (5,592.1 / 8,789.2 / 9,199.3) | 0.98 |
| MySQL 8.4 | 8 | **4,992.0** (2,708.6 / 6,075.1 / 4,992.0) | 1.14 ms / 4.02 ms | 8,344.8 (8,068.6 / 8,412.0 / 8,344.8) | 3.90 (3.82–3.92) |

Retries were zero on both engines at both levels in all three runs.

**Writes: we lose at one connection (~0.64x) and badly at eight (~0.30x),
and MySQL's column is the loudest on this page.** Per run the 1-connection
ratio was 0.92x, 0.64x and 0.58x and the 8-connection ratio 0.52x, 0.25x
and 0.30x — MySQL ahead in 6 of 6 pairs, so the sign is not in doubt, but
MySQL's own write figures span 724.5–1,206.8 (46%) and 2,708.6–6,075.1
(67%) at 1 and 8 connections, and its 1-connection p50 spans 0.82–1.07 ms
with one 8-connection p50 at 1.72 ms against 0.97 and 1.14 — the widest
`p50` row in the whole repeat (241% on its p50 tail, per the summary
file). Read the multiples as the per-run bands, not the medians. From one
connection to eight InlaySQL's writes scale 2.3x (668.9 → 1,522.2) and
MySQL's 4.8x (1,041.8 → 4,992.0). The commits-per-fsync column says why
that is not a batching gap: at eight connections InlaySQL's coordinator
rides 4.06 commits on each barrier and InnoDB's group commit 3.90 — the
same, or a shade better, on our side — so the whole of the throughput gap
is barrier *rate*: 1,522.2 / 4.06 ≈ 375 fsyncs/s against 4,992.0 / 3.90 ≈
1,280, a 3.4x difference in how often each server gets to flush at all.
That is the same decomposition the 1/4/16-connection sweep below reached
on 2026-08-31 (a ~1.6x batching deficit only at 16 connections, and a
2.8–3.2x barrier-rate deficit everywhere) and `PERF.md`'s "Task 2" runs the
diagnosis; that repeat reproduced it on a fresh build, a new MySQL
version and a different sitting. The p99 column says it a third way: at
eight connections InlaySQL's write tail is 22.30 ms against MySQL's 4.02 ms
(5.5x; 21.36–24.04 ms against 2.35–5.42 ms across the runs, ranges not
overlapping), where at one connection the two are 3.35 vs 1.90 ms.

**Reads: a ~1.2x win at one connection, parity at eight, and the
1-to-8 read drop this table carried for three editions is now inside the
noise.** At one connection InlaySQL read 10,292.4 against 8,789.2 (per run
1.72x, 1.17x, 1.17x — ahead 3 of 3, but MySQL's own 1-connection read
column spans 5,592–9,199, so the 1.72x is MySQL's bad run, not our good
one). At eight, 9,067.7 against 8,344.8 (1.11x, 1.08x, 1.24x — ahead 3 of
3, every one inside this page's ~20% desktop floor and the 8-connection
InlaySQL read cell's own 16% spread, so a tie). InlaySQL's reads from one
connection to eight moved 10,292.4 → 9,067.7 at the medians — −7%, −12%
and −4% per run — where the 2026-08-29 single run found a real-looking 30%
fall (9,033.3 → 6,294.3) with MySQL's flat; MySQL's own step here is
8,789.2 → 8,344.8 (−5%). The phase order suspected of producing the old
drop (the MySQL driver's write burst running immediately before
`server_driver.py`) is unchanged in `compare.sh`, so this is not the
mechanism being removed; it is three gated runs failing to reproduce a
drop that one ungated run showed, which is what the "What is not measured
here" list below now records instead of an open investigation.

**History, kept because the reasoning is why the driver was rebuilt, not
because the numbers still stand.** The 2026-08-29 single run at `f8e29e9`
(MySQL 8.0.x, stock buffer pool, manually checked quiet at ~3/18) read
InlaySQL 556.7 / 1,255.5 write ops/s and 9,033.3 / 6,294.3 read ops/s at
1 / 8 connections against MySQL's 787.7 / 3,092.7 and 7,400.6 / 7,931.1 —
0.71x and 0.41x on writes, 1.22x and 0.79x on reads. Two editions before
that, a 26,270 → 17,628 read drop was published as evidence that eight
connections warm eight per-handle page caches — a server-side diagnosis
that did not survive testing: rebuilding the read phase on a quiet machine
did not reproduce it, running the client's concurrency as *threads*
instead of *processes* did, twice, including on MySQL's own row, which lost
31% of its reads purely from the client's GIL while the server sat idle in
`recvfrom`. A shared raw-page cache across connections was built anyway
(`docs/server.md`'s D2; real, ~18% on the page-miss path) but could not
have been the fix even in principle, because this benchmark's per-handle
cache already holds the whole working set. The process-based driver was
the honest next step; its first run found the smaller 30% drop just
described, two checks (the same driver against the server on the host
loopback, then inside the compose network alone and with the full stack
idle) found the server scaling *up* cleanly in both, and the repeat above
is the first time the drop has been measured more than once. See
"Server-to-server" in `bench/README.md` for the methodology, the
concurrency-model/credential/TLS asymmetries that remain, and why
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

## Read shapes and batch insert against MySQL and PostgreSQL (regenerated 2026-09-02/03, gated; first measured 2026-08-31)

Four workloads that had **no harness on either side** until 2026-08-31 —
indexed range scan, two-table join, aggregate/`GROUP BY`, batch insert —
measured against both servers, filling the eight UNKNOWN MySQL/PostgreSQL
cells `SCOREBOARD.md` carried since it was written. Harnesses:
`bench/external/read_driver.py` (range/aggregate/join, `TARGET=mysql|postgres`),
`bench/external/batch_driver.py` (batch insert with commits-per-fsync
bracketing), `inlaysql-bench --bin sql_shapes` (InlaySQL's side for the two
shapes with no Rust suite); `compose.yml`'s shared unix-socket volume so
both servers are reached over the same transport.

**Regenerated at `bdc64eb` on the night of 2026-09-02/03, and this time the
machine was quiet.** The 2026-08-31 sitting ran under desktop load (1-minute
load 4–10 of 18, gate deliberately overridden, `SCOREBOARD.md` §4.0
applied the 20.2% desktop-load floor to every verdict). Tonight: `REPS=5`
on every server cell, the `(shape, rep)` schedule Fisher-Yates-shuffled
from a fixed seed, medians with min–max published; `uptime` read before
and after (load 1.47–2.36/18 — these drivers have no mid-run sampler, so
this is a weaker gate than `compare.sh`'s, disclosed rather than
promoted). Raw: `bench/results/20260902T191343Z-scoreboard/`
(`read-{mysql,postgres}.txt`, `batch-{mysql,postgres}.txt`,
`sql-shapes-inlaysql.txt`, `sql-shapes-inlaysql-batch.txt`,
`provenance.txt`). **The MySQL column is 8.4 (LTS) in that sitting and was 8.0.x on
2026-08-31**, so no MySQL edition-to-edition move below is attributed to
either engine. InlaySQL's aggregate and batch-insert cells are
`sql_shapes` at `REPS=5` from this sitting, on the host; its range and
join cells are reused from this edition's gated `run.sh` tables at
`be95cc3` (2026-09-05 morning, median of three — the previous three
editions likewise reused their own `run.sh` figures for those cells), which
means three disclosures: they are a different sitting from the server
columns, they are a *later* build than the server columns' `bdc64eb` by
AHL-544 through AHL-553 (of which AHL-549, AHL-550 and AHL-551 moved the
`LIMIT` join and range cells, as the sections above say), and the `run.sh`
join suite's `LIMIT` shapes are `LIMIT 10` where the drivers run
`LIMIT 20`. InlaySQL
runs in-process throughout; the servers
sit behind a unix socket — an asymmetry that favours InlaySQL, so every
LOSS recorded here is conservative and every WIN is partly the transport.

### Indexed range scan — WIN both

`SUITE=indexed`'s shape: `users (id, email, body)`, 100,000 rows, index built
after the rows, 100 range queries of exactly 50 rows each, the key sequence
generated with the same seeded xorshift64* the Rust suite uses.

| Engine | ops/s (median, range) | p50 (median) |
| --- | --- | --- |
| **InlaySQL** (`run.sh` at `be95cc3`, gated median of three) | 118,489 (99k–120k) | 8.00 µs |
| *SQLite, journal — reference, same in-process harness as InlaySQL* | *144,622* | *6.67 µs* |
| PostgreSQL 17 | 21,824 (9,009–22,931) | 44 µs |
| MySQL 8.4 | 14,330 (14,181–14,635) | 67 µs |

**Read the SQLite row first.** It is the same statement over the same data,
and it is *not* comparable to the server rows either: it runs in this
process, as InlaySQL does, while MySQL and PostgreSQL answer a Python client
over a unix socket. SQLite is ~10x MySQL 8.4 and ~6.5x PostgreSQL here, which
is the same band InlaySQL is in — so **the multiple against the servers on
this shape is mostly the client and the round trip, not the storage engine**,
and the honest reading of the three numbers together is: InlaySQL is 1.20x
slower than SQLite at reading a fifty-row range (the loss the point-read
section owns), and both in-process engines are far ahead of two servers being
asked the same question over a socket. A reader who sees "loses to SQLite,
beats the servers by 8x" and suspects one of the two is broken is right to
ask: neither is — both harnesses assert the row count before timing and
refuse to publish a wrong answer (`indexed.rs`'s `debug_assert_eq!` on both
sides, `read_driver.py`'s "refusing to time a wrong answer") — they simply
measure different things.

**~8x MySQL 8.4 and ~5.5x PostgreSQL at the medians** (8.3x and 5.4x; was
~8x/~5.5x with the `1f7921a` cell, ~7x/~4.5x with `3cf0d85`'s, ~3.7x/~2.3x
on 2026-08-31). The servers' columns are the 2026-09-02/03 sitting's and
did not move, and this edition InlaySQL's cell did not either: 119,219 →
118,489, −1%, inside its own 18% spread. The step before it, 97,624 →
119,219, is AHL-550's residual filter compiled once per execution
(1.22–1.36x interleaved on this exact shape) over AHL-541's 1.04x; AHL-551
measured a further 3–7% on this shape interleaved, 6 of 6, which this cell
cannot see and does not claim. The step before it, 49,259 → 97,624, was two things at once: the
2026-08-31 figure was a same-sitting median under desktop load against a
published-clean 64,250, and AHL-535 measured 1.40x on this shape and
changed the harness under it. So read the growth in the multiple since
2026-08-31 as part measurement conditions and part two attributed engine
changes, and the WIN itself as the unchanged finding: the range scan
InlaySQL loses to SQLite is a shape it wins against both servers.

### Two-table join — WIN all four shapes against both servers

`SUITE=joins`' exact shape: 20,000 users × 8 round-robin posts, index on
`posts.user_id` built after the rows, ANALYZE, 100 executions per rep, p50
medians compared, both FROM orders reported worst-first per
`SCOREBOARD.md`'s pre-fixed join rule.

| Shape | InlaySQL p50 (`be95cc3`) | MySQL 8.4 p50 (median, range) | PostgreSQL 17 p50 (median, range) |
| --- | --- | --- | --- |
| PK inner, full join | **3.26 ms** | 13.68 ms (13.64–13.71 ms) | 9.36 ms (9.28–9.47 ms) |
| Secondary-index inner, full join | **3.51 ms** | 13.71 ms (13.68–13.83 ms) | 9.42 ms (9.30–9.49 ms) |
| PK inner, LIMIT (ours 10, theirs 20) | 3.46 µs | 44 µs (42–44 µs) | 29 µs (28–30 µs) |
| Secondary-index inner, LIMIT (ours 10, theirs 20) | 5.54 µs | 51 µs (49–52 µs) | 30 µs (28–30 µs) |

Both servers hash-join either FROM order in ~13.7/~9.4 ms — the
iteration-side asymmetry that used to split InlaySQL's own two full-join
shapes does not exist for them, and as of AHL-524 it no longer exists for
InlaySQL either. **Full joins: ~4x MySQL 8.4 and ~2.7-2.9x PostgreSQL on
both shapes.** The 2026-08-31 edition had the PK-inner full join at 13.04
ms — a TIE against MySQL and the one red cell against PostgreSQL (LOSS
~1.24x), "the shape where PG's planner picked the better order". That cell
is gone for a named reason: AHL-524 (`PERF.md`, 2026-09-02) fixed AHL-512's
inverted cost model so both written orders run the same users-driving
plan, measured 9.34 → 3.21 ms on this shape in a single run, 3.23 ms in
the gated `3cf0d85` median, 3.25 ms at `1f7921a` and 3.26 ms in this
edition's. The servers' own
full-join columns moved 15.00/15.01 → 13.68/13.71 ms (MySQL, version
change and quiet machine) and 10.49 → 9.36/9.42 ms (PostgreSQL, quiet
machine) — the direction of a quieter sitting, unattributed. **The `LIMIT`
rows are not the same shape on both sides** — `LIMIT 10` from `run.sh`
against the drivers' `LIMIT 20` — so the arithmetic (9–12x MySQL, 5–8x
PostgreSQL; the InlaySQL cells moved 3.75 → 3.46 and 5.79 → 5.54 µs this
edition, per the joins section, which attributes the direction to AHL-551
and disputes the size) overstates a like-for-like comparison by
up to the per-row cost of ten more rows; read those two rows as "several
times faster, on a smaller LIMIT", not as the digits. The 2026-08-31
edition's InlaySQL `LIMIT 20` cells (14.08 / 13.38 µs) came from a
same-sitting run under desktop load and are not reused.

Full-join methodology, disclosed: the full-join shapes are timed as
server-side `SELECT COUNT(*) FROM (<join>) q` wrappers, because a Python
client fetching 160,000 rows per execution measures the connector's
per-row cost (the drivers container sat at 100% CPU with the server idle
before this change), not the engine's join; the wrapper still produces and
discards every joined row server-side, and the LIMIT/range/aggregate shapes
transfer their rows directly. The asymmetry favours InlaySQL — its own
number includes row streaming — so the servers' full-join figures are, if
anything, flattered.

### Aggregate / GROUP BY — WIN on `GROUP BY`, LOSS on the scalar aggregate

A shape defined on 2026-08-31 (no Rust suite exists on either side; the
InlaySQL side runs through `sql_shapes --mode agg`, on the host, in-process):
`indexed`'s 100,000-row table with a 100-bucket column added; 100 executions
per rep, 5 reps, median (min–max).

| Shape | InlaySQL | MySQL 8.4 | PostgreSQL 17 |
| --- | --- | --- | --- |
| `GROUP BY n` (100 groups) | **210/s** (207–212) | 110/s (109–110) | 167/s (165–167) |
| scalar `COUNT/MIN/MAX` | **1,914/s** (1,624–2,026) | 300/s (289–301) | 362/s (358–366) |

**`GROUP BY`: WIN 1.9x MySQL 8.4 and 1.26x PostgreSQL** — every InlaySQL
rep above every server rep, 5 of 5, ranges not overlapping; the 1.26x
clears the quiet-machine floor and would not clear the desktop-load one,
which is a reason this sitting's quiet is worth having. **Scalar aggregate:
WIN ~6x vs MySQL 8.4, ~5x vs PostgreSQL** — remeasured 2026-09-03 15:26
at `8cd65c7` (`sql_shapes`, REPS=5, load 4.3 with a build agent on the
host, so its 21% spread is that sitting's; raw
`bench/results/20260902T191343Z-scoreboard/sql-shapes-inlaysql-agg-8cd65c7.txt`).
Earlier the same day this cell read **225/s (221–226), a LOSS of 0.75x and
0.62x**, 5 of 5 the other way: a whole-table `COUNT/MIN/MAX` over 100,000
rows at 4.4 ms, ~44 ns per row, the executor paying per row where the
servers' do not. Two landings turned it: AHL-546 answers `MIN`/`MAX` of the
rowid (or an indexed leading column) by one descent to each end of the
tree, and AHL-548 answers a bare `COUNT(*)` from the leaves' cell counts —
every leaf's slot directory is its row count, so the walk borrows pages and
decodes nothing, and is exact under an open transaction because the
pending tree's decoded dirty leaves answer directly. Both are SQLite's own
optimisations; both refuse and fall back the moment a `WHERE`, `GROUP BY`,
`DISTINCT`, `FILTER`, join, `COUNT(col)` or `WITHOUT ROWID` table is in the
statement, and `EXPLAIN` names them. `PERF.md` has each step interleaved:
the `aggregate-scalar` profile shape went 209 → ~2,000/s, 3 of 3.
The servers' cells are the 03:15 sitting's; only InlaySQL's was rerun.

**What moved, and why it is not one commit.** On 2026-08-31 these cells
read 29/s (26–31) and 53/s (49–57) — "the worst multiples in the matrix",
LOSS 3.4–6.0x against both. InlaySQL's `GROUP BY` cell is now 7.2x that
figure and the scalar 4.2x. The servers' own cells moved too — MySQL 98 →
110 and 275 → 300, PostgreSQL 147 → 167 and 317 → 362 — by 9–14%, which is
roughly the share of the move the sitting (desktop load then, quiet now)
accounts for; the rest is the engine, and it is **the aggregate work of
2026-09-02/03, each step measured interleaved in `PERF.md`**, not any one
of them: AHL-513/514/515 (the aggregate streams instead of holding every
row), AHL-519/520 (`SUM/AVG/MIN/MAX` fold as values arrive; a grouped row
that finds its group allocates nothing), AHL-521 (page-cache hash), AHL-522
(sixteen-page read-ahead; 1.26x on this shape), AHL-523 (hash `GROUP BY`;
1.12x), AHL-528a/b/c (fold from the row bytes, whole-leaf admission, bare
column read; 1.5x, 1.05x, 1.04x), AHL-536 (borrowed leaf page; 1.05x),
AHL-538 (rows by callback, stop at the last column read; 1.12x) and
AHL-541 (the leaf's cell offset table; 1.05x). `PERF.md`'s own running
tally on the engine's 100k aggregate profile is 85 ops/s before AHL-521 to
210 after AHL-541 — the same 210 `sql_shapes` reads here, on a different
harness, which is as close to a cross-check as this page has.

### Batch insert — like for like, a WIN against MySQL 8.4 and a LOSS against PostgreSQL; on the host, the barrier

100 rows per multi-row `INSERT` statement, autocommitted, 100 statements per
rep (10,000 rows per rep), explicit ids, 5 reps, durability aligned (MySQL
`innodb_flush_log_at_trx_commit=1`, PostgreSQL `synchronous_commit=on`,
InlaySQL `Durability::Full` — one commit, one barrier per statement
everywhere). **Correction to the previous edition's wording**: it said all
three engines ran "in the same container environment on the same volume
class". The servers do; InlaySQL's cell does not and never did — `sql_shapes`
runs on the host, in-process, and its barrier is the host's `F_FULLFSYNC`.
That is the asymmetry this row is about.

| Engine | rows/s (median, range) | commits/s | c/fsync |
| --- | --- | --- | --- |
| InlaySQL (host, `F_FULLFSYNC`) | 24,102 (23,219–24,736) | 241 | 1.00 |
| **InlaySQL (containerised, same volume class as the servers)** | **67,484** (60,453–70,943) | 675 | 1.00 |
| MySQL 8.4 (containerised) | 56,700 (45,244–68,901) | 567 | 0.71 (0.62–0.76) |
| PostgreSQL 17 (containerised) | **99,212** (93,776–100,749) | 992 | 1.00 |

**Like for like — the containerised row against the containerised servers
— InlaySQL is ~1.2x MySQL 8.4 and ~0.68x PostgreSQL 17**, on a build that
predates AHL-553; that commit stops a commit's barrier paying to grow the
file and is worth ~1.18x on the containerised single-row shape, so this row
too is expected to be better than published and has not been remeasured.
That row was
measured on 2026-09-03 at 10:16 (load 2.88 before, 6.36 after — three
build agents were running on the host, so its spread of 17% is wider than
the servers' sitting; five reps, medians): `sql_shapes` in the
`inlaysql-oltp` service of `bench/external/compose.yml`, the database on the
`inlaysql-oltp-data` named volume, `DIR=/data MODE=batch REPS=5`, raw
`bench/results/20260902T191343Z-scoreboard/sql-shapes-inlaysql-batch-container.txt`.
**On the host the row is LOSS ~2.4x vs MySQL 8.4, ~4.1x vs PostgreSQL**
(was ~1.6x/~3.1x on 2026-08-31), and **that ratio is the barrier, not the
engine.**
InlaySQL's 241 commits/s is 4.1 ms per statement, and the host single-row
write in the OLTP table above pays the same barrier at 246.8 ops/s, 3.91 ms
p50 — a hundred-row statement costs what a one-row statement costs, because
both are one `F_FULLFSYNC`. The servers commit against the Docker volume's
cheaper virtualised barrier, and the OLTP table above measures that
difference on this machine that night, InlaySQL against itself: 246.8 on the
host against 619.8 containerised, 2.5x. What the engine's own share of this
row was, and is: `PERF.md`'s AHL-542 (2026-09-03) profiled exactly this
shape and found the per-row root-to-leaf page round trip at 32% of the
statement — a hundred-row `INSERT` decoded, cloned and re-encoded each of
its ~3 path pages ~100 times to write them once — and removed it (each
dirty page held decoded, encoded once at commit), measured interleaved at
**1.29–1.44x on the engine's own batch-insert profile**, 3 of 3
non-overlapping, with `sync_commit`'s share rising from 60.8% to 85.2% of
the statement. `bdc64eb` has that fix. The published rows/s still reads
24,102 against the previous edition's 26,254 (19,111–43,851, under
desktop load) because at 4.1 ms per barrier the ceiling for this row is
~245 statements/s, 24.5k rows/s, and 24,102 is 98% of it — the
2026-08-31 median already sat near that ceiling on its good reps, and the
engine work that AHL-542 removed was hidden under the barrier on the host
where it was not on the profile. The loss widened because the servers'
side rose on a quiet machine — MySQL 42,933 → 56,700 (and a version
change), PostgreSQL 81,229 → 99,212 — while a barrier-bound row cannot.
MySQL's c/fsync of 0.71 is InnoDB's log layer flushing ~1.4 times per
commit at this batch size, its background flush counted, as before.

The containerised row above is what this section used to owe. The
arithmetic said a 2.5x cheaper barrier at an 85% barrier share should land
it well above the host's and short of PostgreSQL's; it landed at 2.8x the
host's (67,484 against 24,102) and 0.68x PostgreSQL's, which is the same
statement with a number on it. What separates it from PostgreSQL now is
not the barrier: both pay one per statement (c/fsync 1.00 on both sides);
it is the ~1.0 ms of engine work per hundred-row statement that remains
after AHL-542, against PostgreSQL's ~0.6 ms, and that is a per-row engine
item again — the insert path's remaining costs the root plan lists
(`leaf_split_point`, `encode_leaf`, index maintenance).

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
- **The server-to-server 1-to-8-connection read drop: three gated runs did
  not reproduce it.** The table above is process-isolated on both sides as
  of 2026-08-29, closing the "the client's own concurrency shape might be
  the real cause" question three editions carried; that single run then
  found a smaller, real-looking drop of its own (9,033.3 → 6,294.3 ops/s,
  30%, MySQL flat across the same step). Two checks at the time found the
  server scaling *up* cleanly at this concurrency — the same driver against
  `inlaysql serve --mysql` on the host loopback (1,180 → 9,980 ops/s, the
  server 81.4% idle in `recvfrom`), and inside the compose network alone
  and with the full five-container stack present but idle (2,336 → 16,549
  ops/s) — and what did reproduce it was running `mysql_driver.py`
  immediately before `server_driver.py`, which is `compare.sh`'s own phase
  order. Tonight's `REPEATS=3` repeat runs that same phase order and reads
  the step as −7%, −12% and −4% across the three runs (10,292.4 → 9,067.7
  at the medians), against MySQL's own −5% — inside the floor, three
  times. Not root-caused, and not claimed fixed: nothing changed in the
  server's connection model or the driver between the run that showed it
  and the three that did not, so the honest record is that a drop one
  ungated run showed is not visible in three gated ones. `inlaysql-server`'s
  thread-per-connection model was already the less likely explanation and
  stays so. What *is* still open on this table, and measured three times
  the same way, is the write side: a barrier-rate deficit of ~3.4x at eight
  connections with batching at parity — see `PERF.md`'s "Task 2" and
  `PLAN.md`'s W5.
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
  disclosed per section: 128 of 343 metrics moved by 10% or more across the
  three main-suite runs at `be95cc3` (29 of 134 on core columns alone — see
  the spread note at the top of this file), and, in the carried-forward
  2026-08-30 sessions, 63 of 180 in the wide concurrency sweep and 25 of 64
  in the quantisation spot-check. A same-binary A/A test (`PERF.md` §4,
  2026-08-30) puts a number on what "spread" means here: median CoV 4.0% on
  the main suite's core columns, 3.6% on the concurrency sweep, 0.3% on the
  quantisation spot-check, and 7.3-20.2% on the single most scrutinised
  metric depending on how busy the machine was — the acceptance target (CoV
  under 3%) is not met today. This edition's whole-suite spread (128 of 343)
  is the widest of the five gated sittings (106 at `7b20175`, 114 at
  `4f8e5dd`, 109 at `3cf0d85`, 114 at `1f7921a`) and is still well under the
  2026-08-30 edition's (196 of 343), all on the same tool and the same
  metric list; that 2026-08-30 edition's was in turn wider than the one
  before it — 54/266 (20.3%) then, 146/266 (54.9%) there, recomputed on the
  metrics common to both — see the spread note at the top of this file and
  `PERF.md` §4 for the full measurement, including why the originally
  published "56 of 285" comparison overstated it. What four full
  regenerations in three days add to that picture: several rows moved
  between sittings by more than any sitting's own three-run spread — the
  durable write on both sides, the 8-writer commits/s, both vector p50s,
  the BM25 and hybrid p50s, the retrieval vector leg, the ingest rate,
  SQLite's own durable point read (170k, 278k, 170k, 177k, 239k) and its
  durable write (92, 91, 99, 88) — on code that did not change between
  them, so a sitting's own min–max is a floor on the noise, not the whole
  of it, and each section says so where it applies. This is the second
  edition running on which every `run.sh` table is the same harness as the
  edition before, so every edition-to-edition move on this page is a noise
  measurement or an attributed one, and each section says which — and this
  edition is the first on which the point-read row's own move is
  attributed, to AHL-552, on the tail rather than the median. Read every
  ratio in this document as approximate rather than exact — the point-reads
  section above is the extreme case, where the individual runs' own ratios
  against journal-mode SQLite ranged from 2.91x to 9.93x. `bench/compare.sh` carried
  none of the gated machinery when its tables were first measured — no
  repeat wrapper, and load sampled once before the run rather than
  throughout it. **Both landed 2026-08-31** (`bench/load_gate.sh`, shared
  with `run.sh`, and `bench/repeat-compare.sh`), the `trust.yml` question
  that had this recorded as a recommendation rather than a change is
  answered (the gate did fail the shared-runner benchmarks job on its
  baseline load, run 33396108404, and the override is now job-level so
  both entrances agree), and **the 2026-09-03 edition was the first to use
  them for publication**: every `compare.sh` table (DuckDB/pgvector/
  Meilisearch, MySQL 8.4/PostgreSQL 17 OLTP, server-to-server 1/8) is a
  gated `REPEATS=3` median with 53 of 146 metrics on its ≥10% list, and the
  read-shape/batch-insert drivers are `REPS=5` medians with `uptime`
  bracketing rather than a mid-run sampler. This edition carries all of
  those forward unchanged rather than re-running them, so they are now two
  engine editions old and each says so. What is still not repeated:
  the 1/4/16-connection server sweeps (5 interleaved repetitions on
  2026-08-31, manually gated, not rerun since) and `ann-benchmarks`.
  Pinning the machine state itself is still not done and probably cannot
  be, which is why the spread is published instead.
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
