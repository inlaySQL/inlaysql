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
WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32 SUITE=concurrency ./bench/run.sh   # x3, median via bench/summarise.py — the eleven-level sweep + tail latency, THIS edition
REPEATS=3 ./bench/repeat.sh                                        # points, indexed, joins, vectors, retrieval — carried forward from the seventh edition at 72d7ab1 (same binary: no crates/ change since)
SUITE=quantization DOCS=100000 QUERIES=50 ./bench/run.sh           # x3, median — int8 spot-check at scale (carried forward)
REPEATS=3 ./bench/repeat-compare.sh                                # DuckDB, pgvector, Meilisearch, MySQL 8.4, PostgreSQL 17, server-to-server (needs Docker) — gated, median of three, regenerated 2026-09-05
docker exec inlaysql-bench-drivers-1 sh -c 'TARGET=mysql    REPS=5 python /drivers/read_driver.py'   # range/aggregate/join, unix socket — carried forward from 2026-09-02/03
docker exec inlaysql-bench-drivers-1 sh -c 'TARGET=postgres REPS=5 python /drivers/read_driver.py'
docker exec inlaysql-bench-drivers-1 sh -c 'TARGET=mysql    REPS=5 python /drivers/batch_driver.py'  # batch insert + commits-per-fsync — carried forward from 2026-09-02/03
docker exec inlaysql-bench-drivers-1 sh -c 'TARGET=postgres REPS=5 python /drivers/batch_driver.py'
REPS=5 cargo run --release -p inlaysql-bench --bin sql_shapes -- --mode agg    # InlaySQL's aggregate side, host — carried forward
REPS=5 cargo run --release -p inlaysql-bench --bin sql_shapes -- --mode batch  # InlaySQL's batch-insert side, host — carried forward
ROUNDS=5 ./bench/batch_insert.sh                                   # the containerised batch-insert cell only: all three engines, five interleaved gated rounds with the engine order rotated — THIS edition, 2026-09-07, and NOT compare.sh
```

| | |
| --- | --- |
| Commit | `run.sh` tables: `72d7ab1` (the seventh full regeneration; `ea1712c..72d7ab1` carries **AHL-563**, **AHL-564** and **AHL-565**, three commit-path changes that have never been published here, plus **AHL-566**, which deleted AHL-562's flush pipeline after re-measuring it flat against a real A/A control — the range is itemised under the table). **`compare.sh`-sourced tables (DuckDB/pgvector/Meilisearch, MySQL/PostgreSQL, server-to-server): still `b873f4e` of 2026-09-05, not regenerated this edition** — `repeat-compare.sh` was not run, and each of those sections says so. Driver-sourced tables (read shapes, batch insert): still `bdc64eb`, **not regenerated this time** either, and each of those sections says so — **with one cell excepted**: the batch-insert table's *containerised InlaySQL* row is `bcbc9d4` of 2026-09-07, re-measured by `bench/batch_insert.sh` (AHL-570), a **different harness from `compare.sh`** and from the drivers. That row moved from 67,484 to 88,456 rows/s and the like-for-like verdict from 0.68x/1.19x to 0.88x/1.64x; the MySQL, PostgreSQL and host cells beside it are still `bdc64eb`. No engine change was made for it — AHL-570 built nothing, and the section says so. **Nothing in `ea1712c..72d7ab1` is on a read path**: AHL-563 narrows a cache invalidation inside the commit gate, AHL-564 shrinks the WAL commit record, AHL-565 removes a free-list rescan that only the DST sweep exercises, and AHL-566 removes code that was compiled out behind a default-off flag in every run this page has ever published. |
| Date | 2026-09-06 (`run.sh`, 07:01–07:22 UTC, i.e. 15:01–15:22 local); 2026-09-05 (`compare.sh`, 06:26–06:36 UTC, i.e. 14:26–14:36 local, carried forward); 2026-09-02/03 (the read-shape and batch-insert drivers, 19:13 UTC); **2026-09-07 (`bench/batch_insert.sh`, the containerised batch-insert cell alone, five interleaved rounds, load 3.23/3.34/3.43 of 18, no round `CONTAMINATED`)**; **2026-09-07/08 (the point-read method change, T0.1: one gated `run.sh` points run at `340467b`, 200k lookups + warm-up, load 2.42–3.89/18, `bench/results/20260907T101202Z.txt`, plus the first CI runner corroboration at the same method — see the point-read section, which now carries both)** |
| Tree | source clean at measurement (`dirty: no` in all three `run.sh` raw outputs and in the `repeat.sh` summary). |
| Machine | Apple Mac17,9, 18 cores, macOS 27.0 (Darwin 27.0.0 arm64) |
| Toolchain | rustc 1.91.1 (ed61e7d7e 2025-11-07) |
| Raw output | **The concurrency suite is the only one regenerated this edition, and it is the eleven-level sweep** (`WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32 SUITE=concurrency ./bench/run.sh`, x3, `bcbc9d4`): `bench/results/20260906T150302Z-repeat.txt`, combined by `bench/summarise.py` from `bench/results/20260906T{150302,162311,172332}Z.txt`. Load, sampled every 5 s throughout, min/median/max per run: 1.42/2.19/3.58, 1.35/2.60/3.77 and 1.52/2.72/3.88 of 18 CPUs against the gate's 0.25/CPU (4.5) ceiling; **no run marked `CONTAMINATED`**, `dirty: no` throughout. **This is the eighth edition, and the first since 2026-08-30 in which the wide writer sweep is not carried forward** — it replaces both the seventh edition's fresh 1/2/4/8 table and the `2cb2539` eleven-level sweep that sat beside it, so the concurrency section is now one session and one build rather than two. It took four attempts across five hours to get three clean runs on this machine: the suite's own peak load sits ~2.4 above the machine's baseline, so with an 18-CPU ceiling of 4.5 a run only finishes clean if the baseline stays under ~2.0 for its whole 4.5 minutes, and `repeat.sh` was abandoned for three separate gated `run.sh` invocations plus `bench/summarise.py` (the path `bench/README.md` documents) precisely because `repeat.sh` discards a whole set when one run refuses at the start gate. **Every other `run.sh` suite is carried forward from the seventh edition at `72d7ab1`** (`bench/results/20260906T070128Z-repeat.txt`, built from `bench/results/20260906T{070128,070835,071517}Z.txt`) — **legitimately, because the engine did not change**: `git diff 72d7ab1..bcbc9d4 -- crates/` touches only the wasm demo page's HTML, and the four commits between them are documentation. The seventh edition's own provenance for those suites follows. Load, sampled every 5 s throughout the measured phases, min/median/max per run: 1.36/2.33/3.73, 1.81/2.59/3.73 and 1.65/2.50/3.75 of 18 CPUs against the gate's 0.25/CPU (4.5) ceiling; no run marked `CONTAMINATED`, `dirty: no` throughout, and all three runs came from the first attempt. **This sitting is quiet but not the quietest**: its median load (2.33–2.59) is the lowest of the seven, and its peak (3.75) is fractionally above the previous sitting's 3.67 — so the "quietest of the six" claim the previous edition made on both counts is not repeated here, and where a figure moved this edition the load samples are not offered as the reason. This is the seventh full regeneration since 2026-09-02; the sixth, at `ea1712c` on the evening of the 5th (`bench/results/20260905T133420Z-repeat.txt`, load 1.2–3.7/18), is the "previous edition" every section below compares against, and the ones before it — `be95cc3` that morning (`bench/results/20260905T020058Z-repeat.txt`), `1f7921a` on the evening of the 3rd (`bench/results/20260903T123928Z-repeat.txt`), `3cf0d85` on the evening of the 2nd (`bench/results/20260902T124832Z-repeat.txt`), `4f8e5dd` that afternoon (`bench/results/20260902T062536Z-repeat.txt`) and `7b20175` that morning (`bench/results/20260902T022325Z-repeat.txt`) — are named where a section's history needs them. **The harness changed again this edition, and again only in what the concurrency suite prints**: `ea1712c..72d7ab1` adds AHL-563's `gate hold:` line to `crates/inlaysql-bench/src/concurrency.rs` — the reservation gate's hold split into read, state, WAL, data, extend, device and residual, with a commit-point-miss count — and removes AHL-562's two pipeline counters from the `barrier cycle:` line along with the pipeline itself (AHL-566). **No suite's measurement changed**: every number in every table below is produced by the same timed code as the previous edition's. What it does change is the summariser's denominator again — 116 diagnostic counter values became 184 — and the ≥10% paragraph below counts like with like rather than quoting the raw total against an older one. **Carried forward from the 2026-08-30 edition at `2cb2539`, not regenerated this edition** (each section says so where it appears): the **quantisation spot-check at scale** (`bench/results/20260830T{125800,131326,132715}Z.txt`, `SUITE=quantization DOCS=100000 QUERIES=50`, median of three, load 2.3–4.8/18). **Carried forward from the gated sitting at `b873f4e` on 2026-09-05, not regenerated this edition** (each section says so where it appears; `repeat-compare.sh` was not run this time, so every one of these figures is the previous edition's unchanged): every **`compare.sh`-sourced table** — the **DuckDB/pgvector/Meilisearch retrieval** table, the **"Against MySQL and PostgreSQL"** OLTP table (host and containerised InlaySQL, MySQL **8.4**, PostgreSQL 17) and the **"Server-to-server"** 1/8-connection table — is the median of three complete `REPEATS=3 ./bench/repeat-compare.sh` runs (`bench/results/20260905T062213Z-repeat-compare.txt`, built from `bench/results/20260905T{062620,063102,063530}Z-compare.txt`; `dirty: no`; load sampled every 5 s through the measured phases, min/median/max per run 1.66/2.80/3.23, 2.13/3.30/4.25 and 1.58/2.35/3.66 of 18 against the 4.5 ceiling; no run marked `CONTAMINATED`; 30 s cooldown between repetitions; **58 of 146 metrics disagreed by 10% or more** across the three, listed in the summary file — more than the 53 the previous edition found, and the OLTP write column is most of the difference). The edition it replaces is `bdc64eb` (`bench/results/20260902T185304Z-repeat-compare.txt`, published by `832f89e`), which every one of those three sections compares against by name. **One thing about the server-to-server table's stack changed between the two editions and is not an engine change**: `inlaysql serve --mysql` now binds the compose service name rather than `0.0.0.0` and the driver authenticates as the account `bench`, created by `inlaysql user add`, rather than as `root` through `--user`/`--password` (Track F's compose change, verified working before this run) — the section says so where its read column moved. Still at `bdc64eb`, **not regenerated this time**: the **read-shape and batch-insert** tables' MySQL/PostgreSQL columns and InlaySQL aggregate/batch cells are `REPS=5` medians with min–max from `bench/results/20260902T191343Z-scoreboard/` (`read-{mysql,postgres}.txt`, `batch-{mysql,postgres}.txt`, `sql-shapes-inlaysql.txt`, `sql-shapes-inlaysql-batch.txt`; `provenance.txt` records `uptime` before and after — load 1.47–2.36/18 — rather than a mid-run sampler, a weaker gate than `compare.sh`'s, disclosed). **Not the containerised InlaySQL batch cell, which is this edition's one regenerated driver-side figure**: `bench/batch_insert.sh` at `bcbc9d4` on 2026-09-07, `ROUNDS=5`, one `REPS=5` repetition of each engine per round with the engine order rotated, the two server volumes recreated first, `bench/load_gate.sh` sampling across the measured phases only — load 3.23/3.34/3.43 min/median/max of 18 against the 4.5 ceiling, no round `CONTAMINATED`. It is a different harness from both `compare.sh` and the hand-run drivers, so it regenerates that one cell and nothing else; `PERF.md`'s AHL-570 has the five rounds and the per-statement split. **The MySQL container is `mysql:8.4` (LTS) from `e7cc895` (2026-09-02) on; every "MySQL 8" figure this file published before 2026-09-02 was 8.0.x**, and the version changed underneath every MySQL edition-to-edition comparison below — none of those moves is attributed to either engine. The InlaySQL range and join cells of the read-shape table are reused from this edition's `run.sh` tables at `ea1712c`, as the previous four editions reused their own `run.sh` figures, and say so — those cells are now three engine editions *later* than the server columns beside them, and AHL-559 moved every one of them. **Carried forward from 2026-08-31, not regenerated**: the two "Server-to-server, extended" 1/4/16-connection sweeps (5 interleaved repetitions each, manually load-gated; raw JSON not retained). **Carried forward from earlier still**: the concurrent-writer old-vs-new A/B (`08f5fd4`, 2026-08-30, `bench/results/ab-head-run{1,2,3}-*.txt` and `ab-pre94d96a6-run{1,2,3}-*.txt`), and, as history only, the 2026-08-30 interleaved OLTP rerun at `b4798ce` (`bench/results/20260830T095714Z-interleaved-oltp-compare.txt`), superseded by the 2026-09-02/03 gated repeat. |

One developer machine. Reproduce it; do not trust it. Every `run.sh` table
on this page — points, indexed, joins, vectors, concurrency at 1/2/4/8
writers, retrieval — comes from `72d7ab1`, measured fresh in one gated
sitting on the afternoon of 2026-09-06. Every `compare.sh` table —
DuckDB/pgvector/Meilisearch, MySQL 8.4/PostgreSQL 17 OLTP, server-to-server
at 1/8 connections — still comes from `b873f4e` on the afternoon of
2026-09-05 and is **not regenerated this edition**; each section that
carries one says so. The driver-sourced cells — the read shapes and batch
insert — still come from `bdc64eb` on the night of 2026-09-02/03 and are
**not regenerated this edition** either. The tables that remain carried
forward (the wide concurrency sweep, the quantisation spot-check, the
1/4/16-connection server sweeps, the writer A/B) each state their own
commit and date where they appear, so a reader can always tell which build
produced which number. What landed between the previous edition (`ea1712c`)
and this one, all in `PERF.md`'s 2026-09-05/06 sections and the only source
any attribution below draws on — **all three of the engine changes are on
the commit path and none of them is on a read path**:

- **AHL-563** — a WAL region wrap called `Device::set_commit_point(region,
  None)`, which on `FileDevice` discarded the file-wide committed state
  *and all four regions' append offsets*; the wrapping writer republished
  its own and the other three each paid a full `read_committed_state` plus
  `wal::scan_region` inside their own gate hold. Narrowing the
  invalidation to the wrapping region took the reservation gate's
  serialized hold from **0.262 ms to 0.121 ms** and measured **1.54–1.70x
  on throughput at 8–16 writers, 6 of 6**, on `PERF.md`'s own containerised
  harness.
- **AHL-564** — a commit record carries whole dirty pages, and a page is a
  fixed-size block with a hole in the middle. Eliding each logged page's
  zero hole cut the record **3.62x**, so a 1 MiB WAL region now wraps
  roughly every **184 commits instead of every 51**. Its own throughput
  claim at sixteen writers was 1.22x median (1.02–1.55), which `PERF.md`
  called "suggestive and no more" and which AHL-566's floor confirms as
  inside the noise.
- **AHL-565** — `refill_free_candidates` rescanned free-list rows it had
  already rejected, which AHL-564's smaller record exposed by letting the
  DST free-list sweep run its seeds to completion instead of breaking out
  on a record-ceiling refusal. Removing the rescan is **13.3x** on that
  path and took the sweep from 1,902 s back to 183 s. **No suite on this
  page runs that path**, and no figure below is attributed to it.
- **AHL-566** — measured the harness's own noise floor with a true A/A
  control (`bench/aa_floor.sh`: same binary, same volume, arms differing in
  nothing, order flipped every repetition, ten repetitions), re-ran
  AHL-562's flush pipeline against it, found it flat again now that the
  gate had widened 3.1x, and **deleted the pipeline** rather than leave a
  falsified maybe behind a flag. Every run this page has published was the
  flag-off engine, so nothing measured here changed; what did change is
  that the flag-off engine is now the only engine. The floor it measured is
  the arbiter the concurrency section below uses, and it runs the opposite
  way from how every earlier edition read it — see that section.

Track F's server work in the same range is in `crates/inlaysql-server` and
the CLI, on no path any suite here runs. Nothing on this page is withheld.

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

**This edition is the tightest of the seven on the core columns, and the
loudest table is one no earlier edition had at the top of this list.** The
main `run.sh` suite
(points/indexed/joins/vectors/concurrency/retrieval), median of three
complete runs at `72d7ab1`: **100 of 343 metrics disagreed by 10% or more**
across the three, against 109 of 343 at `ea1712c` eighteen hours earlier,
128 at `be95cc3`, 114 at `1f7921a`, 109 at `3cf0d85`, 114 at `4f8e5dd`, 106
at `7b20175` and 196 on 2026-08-30, all counted by the same
`bench/summarise.py`. **The summary file itself says 175 of 527, and that
is not the same denominator**: the concurrency suite gained AHL-563's `gate
hold:` line this edition, taking its diagnostic counters from 116 values to
184, of which 75 are themselves in the ≥10% list — so the 343 here excludes
them exactly as the last four editions excluded the `barrier cycle:`,
`buckets:` and `barriers:` values from the same denominator, and the
sequence stays one measurement. On the columns that are the measurement
itself (ops/s, p50, joins/s, commits/s, recall@k — excluding
`max`/`p95`/`p99`/`cold`, which are one sample and expected to swing far
more) it is **18 of 133 (14%)**, against 21 of 134 the edition before, 29
of 134 before that, 22 of 135, 10 at `3cf0d85` and 19 in the two sittings
before those. The 2026-08-30 edition's "53 of 108 (49%)" was counted over a
slightly different column selection (`PERF.md` §4), so compare the
whole-suite
100-versus-109-versus-128-versus-114-versus-109-versus-114-versus-106-versus-196
and not the core fractions digit for digit. Median load was 2.33–2.59/18
across the three runs against 1.85–2.89 the edition before, and the peak
was 3.75 against 3.67 — so this sitting is **the flattest of the seven on
the median and fractionally the loudest of the two most recent on the
peak**, and the previous edition's "quietest of the six on both counts" is
not a claim this one inherits. It did not settle the loudest row either,
and this time the load samples do not even line up in one direction: the
run with the lowest median load (`070128Z`, 2.33) read 1,125,587 point-read
ops/s, the one with the highest median (`070835Z`, 2.59) read 932,452, and
the one with the highest peak (`071517Z`, 3.75) read the fastest of the
three at 1,168,327. Five sittings have now failed to find a relation
between this column and the load sampler, and `PERF.md`'s AHL-552 section
still carries the only named mechanism for its shape. The tightest tables
here held within 0–3%: both InlaySQL full-join p50s (0.3% on the PK shape,
1.8% on the secondary-index one — the tightest core cells on the page), the
indexed point row (0.5% on ops/s, 2.3% on p50), the indexed range row (0.9%
and 0.7%), SQLite WAL's point read (2.5% on ops/s, 0% on p50) and its
indexed point row (1.2% and 0%), the durable write's p50 (2.3%) and both
uniform-corpus recalls (0%). **The loud core columns are mostly the write
and concurrency side this time, which is the side the three new commits are
on** — read that as the reason to quote those rows as bands, not as
evidence for them: the concurrency table's p50s at 4 and 2 writers (67.9%
and 46.7%) and its commits/s at 4, 2 and 8 (21.8%, 18.1%, 14.1%); the
point-read row again (InlaySQL ops/s 21.0%, p50 53.3% — the runs read
1,126k, 932k and 1,168k ops/s at 0.625, 0.917 and 0.584 µs) and
journal-mode SQLite's own point read beside it (ops/s 20.2%, p50 12.4%);
the durable write's ops/s (18.4%, on a row the previous edition published
at 0%) and the batched write's p50 (10.8%); the retrieval suite's
vector-only p50 (32.6%); **InlaySQL's** own PK `LIMIT` join (joins/s 17.7%,
p50 14.0%) where the previous edition's loudest cell on that shape was
SQLite's, whose four cells are all under 10% this time; and on the vector
side the uniform corpus only — its 10%-bucket filtered p50 (17.1%), its
ratio against `sqlite-vec` (13.5%) and its exact-HNSW p50 (12.1%), with
every realistic-corpus cell outside the list. Read every ratio in this
document as approximate, not as three significant digits, and read a "the
previous edition's figure was X, this one is Y" sentence as this
benchmark's ordinary noise unless the text says otherwise and the movement
clears the floor stated at the top of this file. This session's machine
carried its usual mix of editor, browser and agent processes throughout.
The `compare.sh` tables are the 2026-09-05 afternoon sitting's at
`b873f4e`, carried forward unchanged and not re-measured this edition, and
their own four unrelated, idle Docker containers are described where they
appear rather than repeated here.

---

## Against SQLite

SQLite is measured in two configurations because they are two different
promises. `journal` + `synchronous=FULL` + `fullfsync` is the like-for-like
column: it is the only one that makes a durability claim comparable to ours,
and `fullfsync` is what makes a macOS number mean anything at all. WAL +
`synchronous=NORMAL` is SQLite at its fastest, and is the harder target.

### Point reads by primary key — the harness was the number; with a real read window the row is a WIN (2.10x WAL, one gated run; runner trend 4.6x)

**The read-window method change (2026-09-07/08, T0.1) — this section's
figures below it are the old 5,000-lookup method and are retained as
history; the new method's numbers follow.** The bench's read phase now runs
**200,000 lookups** with a discarded 10,000-lookup warm-up on both sides
(`bench/results/20260907T101202Z.txt`, one gated run at load 2.42–3.89/18,
no contamination, commit `340467b`): InlaySQL **2,577,421 ops/s**, p50
**0.334 µs**, p95 0.541 µs, p99 0.625 µs, against SQLite WAL **1,227,266
ops/s**, p50 0.791 µs, p95 0.875 µs — **2.10x on ops/s, p50 2.4x, p95
1.6x, p99 1.7x, all our side ahead**. The p95/p50 tail falls from 2.6x to
1.6x and the mean finally sits beside the median, which is exactly what a
window four hundred times longer than the timer tick should show. One run,
not a median of three: the repeat sitting started the same evening was
contaminated twice and refused once (a 100%-CPU OCR server and Docker
helpers landing mid-run), and the third attempt likewise, so the gated
median-of-three on the new method is still owed — this run is published
now because three independent later measurements agree with its direction.

**First corroboration: the CI runner (2026-09-08,
`RUNNER-BENCHMARK.md`, gate off, shared 4-vCPU box — trend evidence only
by its own header).** Same 200k method, median of three:
InlaySQL **1,700,106 ops/s**, p50 0.541 µs, p95 0.661 µs against SQLite
WAL 371,813 ops/s, p50 2.61 µs, p95 2.81 µs — **4.57x**, with SQLite WAL's
side carrying the absolute-overhead wall a 4-vCPU shared runner adds
(p50 2.61 µs against the 0.750–0.792 µs the same arm shows on the quiet
Mac). The direction matches on every column that is not the runner's own
offset, and the runner's indexed point read moves the same way:
**305,786 vs WAL 259,249 = 1.18x**, where the table below publishes 0.70x
— the indexed row's two per-probe allocations (T1.3) are the remaining gap
on both machines, not a platform difference.

The two measurements, side by side:

| Point read, 200k window | ops/s | p50 | p95 | p99 |
| --- | --- | --- | --- | --- |
| **InlaySQL (quiet Mac, one gated run, `340467b`)** | **2,577,421** | **0.334 µs** | 0.541 µs | 0.625 µs |
| **InlaySQL (CI runner, median of 3, gate off)** | 1,700,106 | 0.541 µs | 0.661 µs | 0.801 µs |
| SQLite, WAL + `sync=NORMAL` (quiet Mac) | 1,227,266 | 0.791 µs | 0.875 µs | 1.08 µs |
| SQLite, journal + `sync=FULL` (quiet Mac) | 341,023 | 2.88 µs | 3.21 µs | 3.75 µs |
| SQLite, WAL + `sync=NORMAL` (CI runner) | 371,813 | 2.61 µs | 2.81 µs | 3.09 µs |

**What the old method's tables are worth now.** Every table below with
"5,000 lookups" in it measures a ~4 ms window — a few dozen timer ticks,
started at the wake-up edge of an 80 s fsync-bound write phase. That is why
this row published at 636,980, 342,747, 901,158, 522,562, 533,943,
1,069,233, 872,474, 692,893, 910,788, 991,539 and 1,125,587 ops/s across
eleven editions while five A/Bs measured the path flat: the window, not
the engine. The tables are kept below unedited because each edition's
*relative* statements were true of what was measured; absolute point-read
figures from them are obsolete. The other suites' tables are untouched by
this — only the points and indexed suites read `--lookups`.

The original median-of-three table, method unchanged since AHL-535
(20,000 rows, **5,000 lookups**, `bench/results/20260906T{070128,070835,071517}Z.txt`,
load 1.4–3.8/18, gate passed):


**The harness is the previous four editions', unchanged.** For the record,
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
| **InlaySQL** | **1,125,587** (932k–1,168k) | **0.625 µs** (0.584–0.917 µs) | 1.50 µs | 2.88 µs |
| SQLite, WAL + `sync=NORMAL` | 1,273,101 (1,263k–1,294k) | 0.750 µs (0.750 µs) | 0.958 µs | 1.17 µs |
| SQLite, journal + `sync=FULL` | 168,505 (158k–193k) | 5.42 µs (4.75–5.42 µs) | 8.79 µs | 11.50 µs |

† `p95`, `p99` and `max` (not shown) are tail samples and swing far more run
to run than `ops/s` or `p50` — see the floor note at the top of this file —
so they are not given a range here.

*Everything from here to the Secondary-index section header is the old
method's history, kept for the record it is; the new method's figures are
the two paragraphs at the top of this section.*

**Nothing in this edition's range touches this path, and the row moved
anyway.** `ea1712c..72d7ab1` is AHL-563, AHL-564, AHL-565 and AHL-566: a
gate-hold invalidation, a WAL record encoding, a free-list scan and a
deletion of code that was already compiled out. None of them is executed by
a point read. **Every movement in this table is therefore published
unattributed**, and the previous edition's AHL-559 remains the last named
cause this row has.

**The claim the previous edition made on p50 does not survive this
sitting.** That edition's heading said "faster than SQLite's fastest
configuration, in every run", on 0.375/0.750, 0.750/0.833 and 0.750/0.792
µs. This sitting pairs 0.625 against 0.750, **0.917 against 0.750** and
0.584 against 0.750: **ahead 2 of 3, behind in the middle run**. The
medians still read our way, 0.625 against 0.750, and the median is not the
claim — the claim was "every run" and it is withdrawn. On this row's own
53% p50 spread, two of three is what this instrument can say, and the
honest verdict is the one the scoreboard already carries: a tie.

**On throughput the gap widened slightly and in SQLite's favour.**
InlaySQL's 1,125,587 ops/s is **0.88x** WAL's 1,273,101 — per run 0.87x,
0.74x and 0.92x, so **no run crossed above it** this time, where the
previous edition had one of three cross (at 1.63x) and the two before that
had none. Read together with the p50 pairing, the position is unchanged in
substance: our typical lookup is at or below SQLite WAL's and five thousand
of them back to back are not. The tail ratios: InlaySQL's p95 (1.50 µs) and
p99 (2.88 µs) are **1.57x and 2.5x** WAL's (0.958 and 1.17 µs), against
1.85x and 3.3x the edition before — better, and on tail samples, so not
claimed.

**Against journal-mode SQLite, roughly 5-7x, and the range is the
measurement.** This session's three individual-run ratios were 7.10x, 4.84x
and 6.93x (the harness's own "is Nx faster" lines); the median run says
6.93x. The previous edition's spread on the same pairing was 6.03–11.04x,
so the two editions' bands overlap across most of their width and neither
supports a figure. Journal-mode SQLite's own side moved 164,448 → 168,505
ops/s (+2%) and 5.46 → 5.42 µs, flat this time on code that did not change,
after moving 31% the sitting before; seven sittings have now put SQLite's
durable point read at 170k, 278k, 170k, 177k, 239k, 164k and 169k, a band
1.7x wide.

**The runs, in order** (`070128Z`, `070835Z`, `071517Z`): 1,125,587 /
932,452 / 1,168,327 ops/s at p50 0.625 / 0.917 / 0.584 µs, p95 1.67 / 1.46
/ 1.50 µs, p99 3.21 / 2.75 / 2.88 µs, max 235.54 / 235.29 / 184.13 µs. The
slow run is the middle one, and it is the run with the highest median load
(2.59 of 18) but not the highest peak; the fastest run has the sitting's
highest peak (3.75). **That is a third distinct pattern in three sittings,
which is the point**: the load sampler does not explain this column.

Our own median moved 991,539 → 1,125,587 ops/s (+14%) and 0.75 → 0.625 µs
on a row whose own three runs disagree by 21% and 53% — **inside its own
spread, with no commit in the range on its path, so published as movement
and attributed to nothing.** This row has now been published at 636,980,
then 342,747, then 901,158, then 522,562, then 533,943, then 1,069,233,
then 872,474, then 692,893, then 910,788, then 991,539, and now 1,125,587
ops/s across eleven editions — the last five on a different harness from
the six before them, so the sequence is not one measurement. **The swing is
not mysterious in kind, only in size: `PERF.md` §4 dissected this exact
metric directly and found background scheduling contention alone triples
its CoV, from 7.3% on a quiet, gated machine to 20.2% on this same machine
under ordinary desktop load, on five runs of one unrebuilt binary — no
rebuild, no edition change, no code touching the read path at all.** That
is still the worst-measured floor of any row in this document, which is why
this edition publishes a median of repeated runs with the runs beside it,
and why the ratio against journal-mode SQLite — read as "roughly 5-7x", not
to three digits and not to one — is the number to quote, not the point
value either side of it.

### Secondary-index reads — point win, range loss, both roughly where the previous edition left them

20,000 rows, `CREATE INDEX` on a non-key TEXT column, 5,000 point lookups and
100 range queries of 50 rows (`SUITE=indexed`). Same three runs as the point
reads above.

**The harness is the previous four editions', unchanged** (AHL-535's loop,
`crates/inlaysql-bench/src/indexed.rs`: InlaySQL steps rows through
`query_prepared_each_ref`, SQLite reads through `row.get_ref(..)`, both
sides read *both* selected columns of every row into a checksum), so every
column here is the same measurement as the previous edition's with a
different binary.

| Engine | point ops/s (median, range) | point p50 (median, range) | range ops/s (median, range) | range p50 (median, range) |
| --- | --- | --- | --- | --- |
| **InlaySQL (B-tree index)** | **535,879** (535k–537k) | **1.71 µs** (1.67–1.71 µs) | 128,383 (128k–129k) | 7.13 µs (7.08–7.13 µs) |
| InlaySQL (no index: full scan) | 1,332 (1,323–1,339) | 745.88 µs (744.33–753.50 µs) | 1,168 (1,162–1,184) | 861.54 µs (844.42–865.54 µs) |
| SQLite, journal (index) | 266,073 (258k–266k) | 3.58 µs (3.54–3.67 µs) | **144,788** (138k–149k) | **6.63 µs** (6.29–6.71 µs) |
| SQLite, WAL (index) | 762,031 (755k–764k) | 1.13 µs (1.13 µs) | **242,057** (240k–242k) | **3.96 µs** (3.96–4.00 µs) |

**This is the tightest this table has ever been**: all four InlaySQL
columns held inside 2.3% across the three runs (0.5% and 2.3% on the point
row, 0.9% and 0.7% on the range row), and not one of the eight InlaySQL
cells is in this run's ≥10% list, where the previous edition had the range
row at 19% on ops/s and 13% on p50.

The index itself is worth **roughly 400x** over our own full scan on point
probes and **roughly 110x** on range scans (AHL-423; the harness's own
per-run figures were 405x/399x/404x and 110x/111x/108x, against ~420x/~110x
the edition before). **On point probes we beat journal-mode SQLite by
roughly 2x** (2.08x, 2.01x and 2.02x per run, against roughly 2x the
edition before) **and trail WAL-mode at roughly 0.70x** (0.700–0.710x, from
0.67–0.75x). **Range scans we still lose, and by about what we lost by last
edition: roughly 1.13x of journal and roughly 1.9x of WAL** (0.887x and
0.530x on ops/s; per run 0.930x, 0.887x and 0.854x against journal, so
behind 3 of 3; on p50, 6.63 against 7.13 µs is 1.075x).

**Nothing in `ea1712c..72d7ab1` is on this path, and the table reads that
way.** The point probe went 527,067 → 535,879 ops/s (+1.7%), 1.75 → 1.71 µs
(−2%); the range row 126,183 → 128,383 (+1.7%), 7.29 → 7.13 µs (−2%).
SQLite's four cells beside them are equally flat (journal point 265,615 →
266,073, journal range 145,719 → 144,788, WAL point 770,283 → 762,031, WAL
range 238,663 → 242,057, every one inside ±1%). **Every one of those eight
movements is inside the cell's own spread and none is attributed.** The
last named cause on this row is still the previous edition's AHL-559, the
comparator that stopped calling `memcmp` (+15% on `indexed` and +14% on
`indexed-range`, interleaved and non-overlapping); this edition adds
nothing to it and takes nothing away.

What the range loss is, unchanged from the previous edition's diagnosis and
repeated here because the loss is still published: before AHL-559,
`bin/profile --suite indexed-range` over 17,469 samples put
`_platform_memcmp` at **26.4%** of the query, split across the retained
leaf's search inside `reseek` (10.2%), `WalkBounds::admits` (5.6%), the
leaf binary search (4.5%), `CursorPath::admits` (2.5%), `starts_below`
(2.5%) and routing (1.7%). AHL-559 deleted the call from every one of those
sites. **The half of AHL-559 that did not land is the more useful record**:
proving the shared prefix once per node — sound, and the argument is in
`PERF.md` — was built, measured against the landed comparator with the
proof forced off, and **contributed nothing on `indexed-range` and was 3 of
3 behind by ~4% on `joins-limit`**, because two prefix scans per node cost
more than the word compare they save. It is not in the tree.

### Joins — we win both full shapes, the PK `LIMIT` shape is ahead on p50 and no longer behind on throughput, and the secondary `LIMIT` shape is still a loss

20,000 users × 160,000 posts, identical schema and indexes on both sides
(`SUITE=joins`). Each row splits the cold first execution of the query shape
from the warm p50 — the cold column is where the join plan and its tables get
built, so it is the expensive one:

**Regenerated this edition at `72d7ab1`** (`SUITE=all REPEATS=3`, median of
three, same three runs as every table above, quiet-machine gate passed
throughout and no run marked `CONTAMINATED`; raw:
`bench/results/20260906T070128Z-repeat.txt`). The joins harness did not
change — this table is the same measurement as the previous edition's with a
different binary, and so is every table above it.

| Query shape | InlaySQL cold → p50 (median) | SQLite journal cold → p50 (median) | vs journal |
| --- | --- | --- | --- |
| PK inner, full join | 21.24 ms → **3.12 ms** | 11.19 ms → 10.33 ms | **~3x faster** |
| PK inner, LIMIT 10 | 26.71 µs → **3.29 µs** | 9.83 µs → 3.46 µs | **p50 ahead 3 of 3**; joins/s ~1.02x *faster* at the median pairing — a wash, see below |
| Secondary-index inner, full | 33.22 ms → **3.38 ms** | 31.27 ms → 30.72 ms | **~8x faster** |
| Secondary-index inner, LIMIT 10 | 58.71 µs → 5.00 µs | 16.96 µs → 4.54 µs | ~1.1x slower |

The last column is the ratio of the two engines' `joins/s` rows, paired
within each run: 3.00x, 3.09x and 3.21x for the PK inner full join; 8.27x,
8.35x and 7.80x for the secondary-index full join; 1.24x, 1.15x and 1.21x
slower for the secondary `LIMIT` shape. The PK `LIMIT` shape is the
exception and is split in two, for a reason given below. **The two full
shapes are the tightest cells on the page this edition** — 3.12–3.13 ms and
3.37–3.43 ms across the three runs, 0.3% and 1.8% — and the reversal from
the previous edition is on the `LIMIT` shapes: **InlaySQL's** PK `LIMIT`
row is now the loud one (joins/s 17.7%, p50 14.0%) where the previous
edition's loudest cell on the page was SQLite's on the same shape, and all
four of SQLite's cells here are under 10% this time. Every `cold` cell is a
single sample and swings, as always.

**The PK `LIMIT 10` shape: the p50 win holds, and the throughput loss this
page has published for six editions is gone rather than reversed.** Paired
within each run, where both engines met the same machine, we read 3.54
against 3.63, 3.08 against 3.46 and 3.29 against 3.42 µs — **ahead 3 of
3**, as in the previous edition. On `joins/s` the pairings are 0.90x,
1.03x and 1.02x, so the median pairing (284,629 against 278,261) is
**1.02x our way** where the previous edition's was 1.08x SQLite's. **This
is not published as a win.** Our own `joins/s` on this shape moved 17.7%
between three runs of one binary, 1.02x is inside that by a wide margin,
and one of the three runs is still behind. The correct statement is that
the shape is a wash on throughput and a win on p50, and the loss line this
page and the README carried is withdrawn rather than turned around.

The cold cell is why the two columns ever disagreed and it has not changed
in kind: 26.71 µs against SQLite's 9.83 — a plan and its hash side built
once — which is 8% of our hundred-run wall clock against 3% of SQLite's,
where the previous edition's 62.71 µs was 16% against 2%. That cold cell is
a single sample (26.63–33.21 µs across the runs) and moved further than
anything else in the row, so **the throughput flip is most plausibly the
cold cell landing lucky rather than the warm path getting faster** —
nothing in this edition's range is on either. Published, named as
unattributed, and not claimed.

One caution about the raw file, because it is easy to misread: the median
summary's line for this shape prints "InlaySQL is 1.11x slower", which is
the *first run's* sentence carried through — `bench/summarise.py` classifies
"InlaySQL is Nx faster …" as prose, deliberately, because the wording can
cross parity between noisy runs and medianing it would double-count a ratio
derived from rows it already medianed. The per-run pairings above are the
figures to read.

**Nothing in `ea1712c..72d7ab1` is on any of these four paths.** The full
shapes moved 3.27 → 3.12 ms (−4.6%) and 3.60 → 3.38 ms (−6.1%) with
SQLite's own rows moving 10.54 → 10.33 (−2%) and 31.30 → 30.72 (−1.9%) on
code that did not change; the secondary `LIMIT` p50 moved 5.25 → 5.00 µs
(−5%) against SQLite's 4.63 → 4.54 (−2%). Those are sitting-to-sitting
movements of the size the previous edition already documented for SQLite's
own PK-inner p50 (a 6–11% band across six sittings on unchanged code), they
are in the same direction on both engines, and **every one of them is
published unattributed**. The last named cause on these rows is the
previous edition's AHL-559 (+13% on `joins-limit`, 3 of 3 interleaved).

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
in `PERF.md`, and the six gated sittings since have landed both full
shapes at 3.56/3.78, 3.23/3.49, 3.25/3.63, 3.26/3.51, 3.27/3.60 and
3.12/3.38 ms — the same users-driving plan for both, which is why they now
sit within 10% of each other having been 11.72 ms and 3.71 ms at
`2eeced7`.

The full-join ratios' history, for the record: the secondary-index inner
shape — the one AHL-464 built the index nested-loop join for — went from
**10.71x slower** (2026-08-20) to 2.85x faster (`9aba437`) to 3.65x faster
(`9b2f11e`, AHL-447) to roughly 8x (2026-08-30) to roughly 7.5x (`2eeced7`)
to a withheld 2.2x (`7b20175`, the regression above) to roughly 8x
(`4f8e5dd`) to roughly 7-8x (`3cf0d85`, `1f7921a`, `be95cc3` and
`ea1712c`) to **roughly 8x** here (8.27x at the median pairing, 7.80x at
its floor) — and its p50 is 3.71 → 3.78 → 3.49 → 3.63 → 3.51 → 3.60 → 3.38
ms across the seven published editions after the regression. The PK inner
full join went from 5.56x slower to 1.43x to 1.20x to roughly 1.1x to
roughly 1.15x slower (`2eeced7`) to 1.17x faster in the withheld run to
roughly 3x faster at `4f8e5dd`, `3cf0d85`, `1f7921a`, `be95cc3`, `ea1712c`
and **roughly 3x** here — 11.72 → 8.77 → 3.56 → 3.23 → 3.25 → 3.26 → 3.27 →
3.12 ms — and the step to 3.56 is the genuine, attributed improvement: it
is the shape the corrected reorder moves *into* users-driving, on top of
AHL-522's read-ahead window, which `PERF.md` measured at 1.17x on the
full-scan join shapes interleaved (AHL-521's page-cache hash was flat on
them — its win is on the `LIMIT` shapes). SQLite's own PK-inner p50 read
9.99 ms at `2eeced7`, 11.03 ms in the afternoon of the 2nd, 10.42 ms that
evening, 10.33 ms on the 3rd, 10.37 ms at `be95cc3`, 10.54 ms at `ea1712c`
and 10.33 ms here (10.32–10.57 across the three runs), a 6–11% band on code
that did not change, which is the size of the sitting-to-sitting noise to
hold against every ratio in this table.

**One `LIMIT` row is still a loss, and it is the secondary one.** 5.00
against 4.54 µs is 1.10x on p50 and 1.21x on the harness's throughput line
— against 1.13x and 1.2x the edition before, 1.26x and 1.3-1.4x before
that, 1.5-1.6x before that, 1.9x the afternoon before that, 2.1x at
`2eeced7`, and 4.7–5.8x before the raw-leaf cache. Paired within runs its
p50 loses 3 of 3 (4.96 against 4.33, 5.00 against 4.63, 5.21 against 4.54),
so it is a real loss, narrowing. A `LIMIT` shape is never reordered
(AHL-525 reorders one only under an `ORDER BY`), so AHL-524 has no part in
either row. What is left on both after AHL-559 deleted the comparison's
call is the *number* of descents and the page-cache lookup for a leaf the
cursor could have held — the slice `PERF.md` names next, and the one it now
says explicitly is not a cheaper comparator: "the comparison is now one or
two word compares and there is nothing left to shave off it."

### Durable writes — we win, and the row got noisier

One row per commit, one `fsync` per commit. Median of three runs, same
session as the tables above.

| Engine | ops/s (median, range) | p50 (median, range) |
| --- | --- | --- |
| **InlaySQL** | **255** (254–301) | **3.87 ms** (3.78–3.87 ms) |
| SQLite, journal + `sync=FULL` + `fullfsync` | 90 (90–98) | 11.10 ms (10.75–11.10 ms) |

**~2.8x** (2.83x, 3.08x and 2.85x, the harness's own per-run lines) — the
same ratio as the previous edition's ~2.8x. **What is not the same is the
spread.** The previous edition published this as "the tightest row this
page has ever published", every run reading the same 250 and the same 89.
This edition's own three runs read 254 / 301 / 255 ops/s, an 18.4% spread
that puts it in the ≥10% list for the first time, and SQLite's side beside
it read 90 / 98 / 90 (8.9%). The middle run is fast on both engines at
once, which is what a machine moving looks like rather than an engine
changing. **So the row is a band again — roughly 250–300 ops/s — and the
previous edition's tightness claim is not repeated.**

The medians moved 250 → 255 ops/s (+2%) and 3.89 → 3.87 ms (−0.5%), both
far inside that spread. **Two commits in `ea1712c..72d7ab1` are on this
path and neither is claimed here**: AHL-564 shrank the WAL commit record
3.62x, which makes a region wrap every ~184 commits instead of every ~51 —
a real effect, and one whose benefit to a *solo* writer is bounded by the
fact that a solo writer pays one `fsync` per commit regardless, which is
97.6% of its busy time by the concurrency suite's own bucket split below;
and AHL-563 narrowed an invalidation inside a gate that a single writer
never contends for. `PERF.md`'s AHL-564 section says the same thing about
its own one-writer arm, and AHL-566's A/A floor makes the point
quantitatively: **one writer is the noisiest point on this harness, paired
ops/s [0.42, 1.98]**, so a +2% edition-to-edition move on a solo durable
write is not a measurement of anything. Seven sittings have now put this
row at 241, 256, 243, 248, 245, 250 and 255 ops/s, so its real
sitting-to-sitting band is roughly 240–260 at the median with individual
runs to 300. Still the most stable row in this document across editions:
the commit gate no longer re-derives the log on every commit (AHL-468),
which paid on the solo path too.

**Batching lifts the same workload to 238,768 ops/s (232,794–241,125, 3.5%
spread) at 1.58 µs (1.46–1.63 µs) — roughly 900x (939x, 801x and 913x per
run) — which is the number to quote for a bulk load and not for a
transaction.** Against the previous edition's 247,194 at 1.50 µs that is
−3% on ops/s and +5% on p50, inside the p50's own 10.8% spread and
unattributed; the multiple's own move, ~1,000x → ~900x, is mostly the
single-row denominator moving, since that row is the one with the 18%
spread. The 4x step it took four editions ago holds, and keeps its name,
**AHL-542** (C7): this row inserts rows one prepared statement at a time
inside one transaction until the engine says the log would overflow, then
commits and begins again — thousands of rows per commit — and until AHL-542
a transaction's dirty pages existed only as bytes between statements, so
every insert decoded its leaf and its path, mutated them and re-encoded
them, and the next statement decoded them again. Now the pages stay decoded
until the commit encodes each exactly once. `PERF.md` measured that at
**1.29–1.44x, 3 of 3, non-overlapping** on the hundred-row
`INSERT ... VALUES (...) x100` shape (19,487 / 19,508 / 16,835 → 25,123 /
26,110 / 24,289 rows/s); this row's own shape amortises its `fsync` over
far more rows and moved 4.0x when the commit landed.

Every row above is full-durability, on both sides of every comparison, on
purpose — an opt-in relaxed-durability tier also exists
(`EngineOptions::durability`) and is measured separately, in `PERF.md`, not
mixed into these tables.

### Concurrent writers — the sweep is fresh and wide again, the 16-writer row clears its A/A floor, and the falloff past the peak is gone

200 transactions per writer, one row each, on real OS threads. **Median of
three gated runs at `bcbc9d4`**
(`bench/results/20260906T{150302,162311,172332}Z.txt`, combined by
`bench/summarise.py` into `bench/results/20260906T150302Z-repeat.txt`;
`WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`, `SUITE=concurrency`, `dirty: no`;
load sampled every 5 s throughout, min/median/max per run 1.42/2.19/3.58,
1.35/2.60/3.77 and 1.52/2.72/3.88 of 18 CPUs against the gate's 0.25/CPU
(4.5) ceiling; **no run marked `CONTAMINATED`**). **This replaces both tables
the previous edition carried**: its fresh 1/2/4/8 table *and* the eleven-level
wide sweep that had been carried forward from 2026-08-30 at `2cb2539`. For
the first time since AHL-563 landed, every level from 1 to 32 is measured on
one build, in one sitting, and this page carries **one** concurrency session
rather than two.

**Why this is a concurrency-only regeneration and not a full `run.sh`
edition.** The engine is bit-identical between the seventh edition's
`72d7ab1` and this `bcbc9d4`: the four commits between them touch only
`README.md`, `docs/`, `BENCHMARK.md` and the demo page, and `git diff
72d7ab1..bcbc9d4 -- crates/` returns nothing but the wasm page's HTML. So
every other suite's table on this page is still a measurement of this exact
binary and is left where the seventh edition put it; only the concurrency
suite needed re-running, and it was re-run the way the sweep it replaces
always was — `WRITER_LEVELS=... SUITE=concurrency ./bench/run.sh`, three
times, medians and spreads from `bench/summarise.py`. **209 of this run's 686
metric values disagreed by 10% or more across the three runs**; that
denominator is not comparable with the previous edition's 184, because this
run is one suite at eleven levels rather than six suites at four.

| Writers | InlaySQL commits/s (median, range) | spread | SQLite commits/s (median, range) | multiple |
| --- | --- | --- | --- | --- |
| 1 | 258 (255–272) | 6.6% | 88 (87–90) | ~2.9x |
| 2 | 361 (344–378) | 9.4% | 90 (88–90) | ~4x |
| 3 | 522 (485–573) | 16.9% | 91 (90–91) | ~5.7x |
| 4 | 841 (646–957) | 37.0% | 92 (90–92) | ~9x |
| 5 | 917 (882–1085) | 22.1% | 92 (91–93) | ~10x |
| 6 | 1319 (1111–1348) | 18.0% | 90 (90–91) | ~15x |
| 8 | 1555 (1501–1563) | 4.0% | 90 (90–90) | ~17x |
| 12 | 2146 (2007–2192) | 8.6% | 90 (89–91) | ~24x |
| 16 | **2649** (2591–2675) | 3.2% | 90 (90–90) | ~29x |
| 24 | **3346** (3283–3442) | 4.8% | 89 (89–90) | ~38x |
| 32 | **3529** (3340–3633) | 8.3% | 89 (89–89) | ~40x |

0.0% aborted at every level. SQLite's own column is flat at 88–92 across all
eleven levels (87–93 across the runs) — it serializes writers at its file
lock, so writer count buys it nothing — which is why the multiple in the last
column is almost entirely our column moving.

**The four levels this page already measured reproduce the previous
edition, which is what makes the seven new ones readable.** The seventh
edition's fresh 1/2/4/8 table was the same engine in a different sitting, so
those four rows are an accidental A/A between the two sittings:

| Writers | seventh edition (`72d7ab1`) | this edition (`bcbc9d4`) | ratio | A/A floor | verdict |
| --- | --- | --- | --- | --- | --- |
| 1 | 272 | 258 | 0.95x | [0.42, 1.98] | inside — same engine, as expected |
| 2 | 404 | 361 | 0.89x | [0.45, 3.58] | inside — same engine, as expected |
| 4 | 761 | 841 | 1.10x | [0.45, 2.50] | inside — same engine, as expected |
| 8 | 1541 | 1555 | 1.01x | [0.77, 1.48] | inside — same engine, as expected |

Same binary, two sittings a day apart, four ratios between 0.89x and 1.10x
and all four well inside their own floors. That is the control this table
gets for free, and it is the reason the rows below are quoted at all.

#### Against the sweep this replaces: the 16-writer row is the one that clears its floor

The eleven-level sweep carried on this page until now was measured on
2026-08-30 at `2cb2539` (`bench/results/20260830T{124155,124632,125240}Z.txt`,
load 2.9–3.6/18), a build that predates AHL-563, AHL-564, AHL-565 and
AHL-566. Level for level, old sweep against this one, each ratio quoted
against **its own** A/A band from `bench/aa_floor.sh` (AHL-566, in `PERF.md`):

| Writers | `2cb2539` (2026-08-30) | `bcbc9d4` (this edition) | ratio | A/A floor at that writer count | verdict |
| --- | --- | --- | --- | --- | --- |
| 1 | 244 | 258 | 1.06x | [0.42, 1.98] | inside the floor |
| 2 | 304 | 361 | 1.19x | [0.45, 3.58] | inside the floor |
| 3 | 480 | 522 | 1.09x | **no floor measured** | unbounded — not a result |
| 4 | 587 | 841 | 1.43x | [0.45, 2.50] | inside the floor |
| 5 | 803 | 917 | 1.14x | **no floor measured** | unbounded — not a result |
| 6 | 952 | 1319 | 1.39x | **no floor measured** | unbounded — not a result |
| 8 | 1209 | 1555 | 1.29x | [0.77, 1.48] | inside the floor |
| 12 | 1545 | 2146 | 1.39x | **no floor measured** | unbounded — not a result |
| 16 | 1616 | **2649** | **1.64x** | [0.81, 1.27] | **outside the floor** |
| 24 | 1307 | **3346** | **2.56x** | **no floor measured** | unbounded |
| 32 | 974 | **3529** | **3.62x** | **no floor measured** | unbounded |

**`bench/aa_floor.sh` published bands at 1, 2, 4, 8 and 16 writers and at no
other level.** This edition did not run it at 3, 5, 6, 12, 24 or 32, so those
six cells have **no floor at all** and are printed above as unbounded rather
than dressed up: a 3.62x at 32 writers with nothing to price its noise
against is a direction, not a magnitude. Sixteen writers is the *quietest*
point the A/A control measured ([0.81, 1.27], the narrowest of the five) and
one writer the noisiest, so the one row here that clears its band is also the
row whose band is most worth clearing. **1.64x at sixteen writers is
therefore the single wall-clock figure on this page that a commit-path claim
can be hung on — and it is exactly one figure.**

**What that 1.64x does and does not attribute.** It prices the whole window
`2cb2539..bcbc9d4`, which is a month of commit-path work and not two commits:
AHL-552 and AHL-553 are both on the write path and both landed inside it,
AHL-563 and AHL-564 are the last two, and AHL-562's flush pipeline was
deleted by AHL-566 while AHL-544/547's commit absorption has been default-off
in every run this page has ever published. **So the honest statement is that
the window containing AHL-563 and AHL-564 moved the 16-writer row 1.64x past
its own noise floor, not that those two commits did it.** That the figure
lands inside AHL-563's own paired claim of **1.54–1.70x at 8–16 writers** is
worth noticing and is not proof: that claim was measured on a different
harness (`PERF.md`'s containerised `inlaysql-oltp` service, against a paired
control, 6 of 6), and two numbers agreeing across two harnesses and two
methods is a coincidence with a plausible cause, which is weaker than an
interleaved A/B and is not being upgraded here. **AHL-564's own claim was
1.22x at sixteen writers (1.02–1.55), which `PERF.md` already called
"suggestive and no more"; nothing on this page changes that.** **AHL-565 is
on the free-list path, exercised by the DST sweep and by no suite here**, so
it is not expected and not looked for.

**Read the 8-writer row as the honest negative.** AHL-563's claim was made at
*8–16* writers, and at 8 this table gives 1.29x against a floor of [0.77,
1.48] — **inside**, and so attributing nothing, exactly as the previous
edition found. The win this suite can see grows with writer count and only
becomes a result at the top of the range AHL-563 claimed, not across it.

#### The peak moved, and the falloff past it is gone

This is the largest shape change the section has ever carried, and it is the
reason the wide sweep was worth regenerating rather than carrying forward
again. **The old curve peaked at 16 writers and then fell hard** — 16 → 24
was a 19% drop and 24 → 32 a further 26%, both larger than either endpoint's
spread, so the falloff was real on that build. **This curve does not fall at
all.** It rises monotonically across all eleven levels, and the only thing
that happens past 24 is that it flattens: 3346 → 3529 is +5.5%, against a
24-writer spread of 4.8% and a 32-writer spread of 8.3%, so **this table
cannot distinguish "32 is the peak" from "24 and 32 are a plateau" — and it
places the peak at or beyond 32 writers, which is past the last level
measured.** Where the peak actually sits is once again not something this
page answers; what it can now say is that it is not at 8, not at 16, and not
below 32. By the harness's own scaling line, **32 writers does 13.69x the
work of one writer** (13.68x, 13.71x and 14.26x per run), against roughly
4.0x on the old sweep.

The old build's 32-writer figure was 974 commits/s and the pre-`94d96a6`
ceiling was 694 at any writer count; **3529 is roughly 5x that old ceiling
and 3.6x the old build's own 32-writer row.** Both of those comparisons are
unbounded — there is no A/A band at 32 writers — and both are stated as
directions.

#### Where the counters land, now that they cover eleven levels

The within-run counters are where `PERF.md` says a result on this path lives:
a ratio of two quantities measured inside the same run rather than against
the wall clock. Medians of the same three runs:

| At N writers | 1 | 8 | 16 | 24 | 32 |
| --- | --- | --- | --- | --- | --- |
| commits per `fsync` | 1.00 | 7.14 | 13.38 | 18.77 | 21.40 |
| `gate_wait` share of a writer's busy time | 0.0% | 8.5% | 17.0% | 23.3% | 28.1% |
| `gate_hold` share | 2.6% | 2.5% | 2.1% | 2.0% | 1.7% |
| `follower_wait` share | 0.0% | 75.6% | 73.2% | 69.1% | 65.4% |
| `fsync` share | 97.2% | 10.8% | 4.9% | 3.1% | 2.3% |
| reservation-gate hold, mean | 0.10 ms | 0.12 ms | 0.13 ms | 0.14 ms | 0.15 ms |
| barrier rate | 257.8/s | 216.7/s | 195.9/s | 177.8/s | 165.4/s |

**The 8-writer column reproduces the seventh edition's to within its own
spread** — commits per `fsync` 7.11 → 7.14, `gate_wait` 9.0% → 8.5%,
`gate_hold` 2.6% → 2.5%, `follower_wait` 74.6% → 75.6%, `fsync` 10.8% →
10.8% — which is the counter-side version of the accidental A/A above, and
the reason the seven new columns are quoted. The seventh edition established
those five against the pre-AHL-563/564 build (4.88 → 7.11 commits per
`fsync`, `gate_wait` 18.7% → 9.0%) and that finding stands unchanged; what
is new here is the shape across writer count.

That shape says plainly where the remaining ceiling is. **Commits per `fsync`
keeps rising to 21.4 at 32 writers while the barrier rate falls only 257.8 →
165.4/s**, which is why throughput rises rather than falls: the coalescing
window absorbs writers faster than the barrier slows down. **What grows
instead is `gate_wait` — 0% at one writer to 28.1% at 32** — and that is the
same process-wide reservation gate this section has named as the wall since
`94d96a6`. The gate's *hold* stays cheap and nearly flat (0.10 → 0.15 ms
mean, and its `gate_hold` share actually *falls* from 2.6% to 1.7% as writers
are added); it is queueing for the gate, not holding it, that eats a third of
a writer's time at 32. **`fsync` share collapsing from 97.2% to 2.3% is the
clearest single statement of what the parallel WAL regions bought**: at one
writer the workload is one `fullfsync` after another, and at 32 the barrier
is almost free per commit.

**Caveats stated rather than buried.** These counters are not A/A-controlled
except for commits-per-barrier, whose band at 8 writers is [0.87, 1.22];
AHL-566's floor covers ops/s, duty cycle, gate hold, commits/barrier and the
`fsync` mean, and does not cover the bucket shares, at any writer count. The
barrier-cycle idle share (3.8% at 1 writer rising to 28.6% at 32) is in this
run's ≥10% list at several levels and is quoted to one digit for that reason.
And the whole `bench/aa_floor.sh` band set was measured on the containerised
`inlaysql-oltp` harness, not on `run.sh` against SQLite — using it to price a
`run.sh` ratio assumes the two harnesses have comparable noise, which is an
assumption this page is making and has not measured.

The 4-writer row is this table's noise measurement, the way the 2-writer row
used to be: 841 with a 37.0% spread across three runs (646 / 841 / 957) is
the loosest cell in the sweep, and its 1.43x against the old build sits well
inside a [0.45, 2.50] floor. The 3- and 5-writer rows are nearly as loose
(16.9% and 22.1%) and have no floor at all. **The tight cells are the ones
high in the sweep** — 16 writers at 3.2% and 8 at 4.0% are the two tightest
in the table — which is the same ordering `bench/aa_floor.sh` found when it
measured this harness against itself, and the opposite of how every edition
of this page before the seventh read its own noise.

The commit gate's pre-`fsync` gather window (`coalesce_normal_commits`,
`crates/inlaysql/src/device.rs`) keeps yielding while a normal commit is
inflight or waiting and progress keeps happening, closing on stalled progress
instead of a fixed 8-yield count — see `PERF.md` for the full mechanism,
unchanged since it shipped. AHL-544 and AHL-547's commit-side absorption
remains behind `EngineOptions::commit_absorption`, default `false`, and
AHL-562's flush pipeline is no longer in the tree at all.

The root cause of the *original* 8-then-falls shape is the gate named above,
and it is worth recording that this edition's curve no longer has the "falls"
half: every writer's whole commit *prepare* phase (conflict check, WAL encode,
page writes, WAL append) still serializes behind one process-wide gate
regardless of how many WAL regions exist, and the `gate_wait` row above is
that serialization priced directly. The regions only let one writer's `fsync`
overlap the *next* writer's turn at that gate. The obvious cheap fix — spin
before parking, in case kernel wake latency rather than the gate itself was
the cost — was tried, measured clean, and reverted: no change, at 100 or at
5,000 spin iterations. A follow-up idea (shrink the gate to the conflict
check and sequence/offset reservation only, move the encode and writes after
release) turned out to be unsafe, not just unscoped: the conflict check walks
the tree from the latest committed root, so it structurally depends on the
previous writer's pages already being landed. The one lever still standing is
*commit-side* logical group commit (one gate holder absorbing other waiting
writers' whole transactions into one prepare/encode/WAL-append pass, not just
one `fsync` covering several already-encoded ones), scoped but not started —
and with `gate_wait` now measured at 28.1% of a writer's time at 32 writers,
this sweep is the first to put a number on what that lever is worth.

### Concurrent writers: the tail the commits/s table hides — and it hides much less than it used to

`08f5fd4` added per-commit p50/p95/p99/max to `inlaysql-bench --suite
concurrency`. The wide sweep above measured every writer level with
percentiles, so this table is a slice of it (`1, 8, 32`) rather than a
separate session — **and this edition it is finally a slice of a *fresh*
sweep**, the same three gated runs at `bcbc9d4`, rather than the 2026-08-30
figures at `2cb2539` that stood here through the six previous editions.

| Writers | InlaySQL commits/s | p50 | p95 | p99 | max | SQLite commits/s | p50 | p95 | p99 | max |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 258 | 3.87 ms | 4.22 ms | 4.32 ms | 5.90 ms | 88 | 11.28 ms | 12.61 ms | 13.06 ms | 16.15 ms |
| 8 | **1555** | 4.82 ms | 8.05 ms | 13.92 ms | 20.12 ms | 90 | 11.13 ms | 12.41 ms | 13.06 ms | 22.85 ms |
| 32 | **3529** | 7.30 ms | 18.15 ms | **23.05 ms** | 33.14 ms | 89 | 11.16 ms | 12.73 ms | **14.15 ms** | 30.52 ms |

Medians, as before, and for the same reason: `commits/s`, `p50` and `p95` are
individually tight across this sweep's three runs (0–18% at these three
writer counts, and 1–3% on most cells), but `max` is not — InlaySQL's own max
at 1 writer swings 57% run to run and SQLite's at 8 writers swings 40%. Read
the `max` column as one unlucky sample. The p99 column, unusually for this
table, is now among the tight ones: 7%, 15% and 13% at 1, 8 and 32 writers.

**The tail loss this section has carried since it was created is largely
gone.** On the old build, 32 writers cost roughly **8x** SQLite's p99 (7.3–9.4x
across that sweep's runs, 121.08 ms against 15.35 ms). On this build the same
comparison is **roughly 1.7x** (1.63x, 1.71x and 1.83x pairing the runs within
themselves, 23.05 ms against 14.15 ms) — and at 8 writers InlaySQL's p99 is
**at parity** with SQLite's (0.97x, 0.99x, 1.15x), while at 1 writer it is
**roughly 3x better** (0.27–0.35x, 4.32 ms against 13.06 ms). So the trade
this section existed to disclose — buy throughput at 32 writers, pay for it
at the tail — now reads: **~40x the committed throughput for ~1.7x the p99**,
where it used to read ~11x for ~8x.

**This is not offered as an attributed win, for the same reason the
throughput table above is careful.** `bench/aa_floor.sh` measured a floor for
ops/s, duty cycle, gate hold, commits-per-barrier and the `fsync` mean, and
**not for a latency percentile at any writer count**, so there is no control
that says how far a p99 moves on its own between two sittings. What is on
offer is that the two sittings' ranges do not come close to overlapping
(121.08 ms against 22.77–25.86 ms), that the direction matches the
commits-per-`fsync` counter (7.14 → 21.40 at 32 writers means a writer waits
behind far fewer forced barriers), and that the window is the same month of
commit-path work the 1.64x above prices. That is a named mechanism and a
wide gap, which is weaker than a measured floor.

The shape the table still shows is real and unchanged in kind: InlaySQL's
tail grows with writer count (4.32 → 13.92 → 23.05 ms p99) while SQLite's
stays flat (13.06 → 13.06 → 14.15 ms), because SQLite serializes writers at
its file lock and the connection that is waiting pays in queueing rather than
in a longer `fsync`. InlaySQL's optimistic design instead lets the gather
window grow the cohort riding one `fsync` as contention rises, and the
writers gathered late in a big cohort are the ones sitting in its p99: p50 at
32 writers (7.30 ms) is under 2x solo (3.87 ms, mostly one `fullfsync`) and
p99 is roughly 3x that p50. **The cost is still a real cost of the
concurrent-writer design and stays in the table** — it is simply much smaller
than it was, and it is now smaller than the throughput multiple by more than
an order of magnitude rather than by a factor of one and a half.
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
above (`bench/results/20260906T{070128,070835,071517}Z.txt`).

| Corpus | recall@10 | p50 (median, range) | vs `sqlite-vec` (median, range across the 3 runs) |
| --- | --- | --- | --- |
| Text-derived embeddings | 1.000 | 58.04 µs (58.00–62.38 µs) | **~11x faster at 100% of its recall** (per-run ratio 10.13–10.93x, median 10.84x) |
| Uniform random | 0.922 | 86.79 µs (85.96–96.46 µs) | ~7x faster at 92.2% of its recall (6.57–7.56x, median 7.33x) |

The multiples are the median of the three runs' own per-run ratios (the
harness's "is Nx faster" lines), not the ratio of the two median p50s; the
two methods agree to within a tenth of a turn on both corpora (631.79 /
58.04 = 10.89x against 10.84x realistic; 636.54 / 86.79 = 7.33x against
7.33x uniform). **The realistic row is again the tight one and the uniform
row again is not**: the realistic InlaySQL p50 held within 7.5% across the
three runs and is outside this run's ≥10% list, where the uniform one moved
12.1%, its ratio 13.5% and its 10%-bucket filtered p50 17.1% — all three in
the list. `sqlite-vec`'s own two p50s are inside 1% and 2.6% this time,
where the previous edition had its uniform cell at 13%.

**Nothing in `ea1712c..72d7ab1` touches `hnsw.rs`, the distance kernels or
any read path, and every cell here moved by less than the previous
edition's cells did.** The realistic InlaySQL median moved 59.42 → 58.04 µs
(−2%) inside its own 7.5% spread, `sqlite-vec`'s beside it 666.79 → 631.79
µs (−5%), and the ratio 11.22x → 10.84x. The uniform row moved 87.75 →
86.79 µs (−1%) with `sqlite-vec` at 638.88 → 636.54 (−0.4%). **All four are
published unattributed.** Eight editions' realistic medians (78.96, 69.54,
75.17, 69.08, 68.67, 63.58, 59.42, 58.04 µs) are one figure measured on
eight different sittings, so the honest quote is roughly 58-70 µs and this
sitting sits at the bottom of it.

Both corpus shapes are published because only one of them flatters us. Uniform
random vectors in 384 dimensions have no structure for a graph index to
navigate, so recall falls and no amount of tuning fixes it. Text-derived
embeddings are what an application actually stores.

`VECTOR(n, INT8)` quantisation costs 0.014 recall on the realistic corpus
(0.986 vs 1.000 exact) and nothing measurable on the random one (0.922 both)
for a **3.96x smaller resident payload** — all three figures identical across
all three runs and identical to the previous two editions'. Its per-query
cost at this scale is 150.46 µs (149.42–153.50 µs) realistic and 237.67 µs
(236.96–239.63 µs) uniform, roughly 2.6x and 2.7x the exact index's p50 (both
int8 rows held within 2.7% and 1.1%, the tightest this pair has been; the
medians moved 154.92 → 150.46 and 239.29 → 237.67 µs from the previous
edition's, −3% and −0.7%, and neither is attributed).

**The file-size ratio is still not about quantisation.** The same
line reads a **2.40x smaller file** here, as it did the previous two editions,
where every edition before that read 1.65x, and the reason is **AHL-553**: the exact index's file went 8,568,832
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
is the only place on this page AHL-553 changed a published number, it
changed a size and not a time, and every byte figure in this paragraph is
identical in all three of this edition's runs and identical to the previous
edition's.

**Spot-checked at scale, `SUITE=quantization DOCS=100000 QUERIES=50`, median
of three runs (`bench/results/20260830T{125800,131326,132715}Z.txt`, load
2.3–4.8/18 throughout) — carried forward from the 2026-08-30 edition at
`2cb2539`, not regenerated this edition; this sitting, like the five before
it, ran only the default 2,000-document suite above.** Recall loss widens to 0.028 (realistic, 0.970 vs
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

2,000 documents, dim 384, `LIMIT 10`. Ingest 19,771 docs/s (median of three
runs, 19,305–20,875; same session as the tables above —
`bench/results/20260906T{070128,070835,071517}Z.txt`).

| Workload | p50 (median, range) | p95 (median) | Historical baseline (`9aba437`) |
| --- | --- | --- | --- |
| Vector only | 63.08 µs (59.08–79.63 µs) | 70.79 µs | 87.88 µs |
| BM25 only | **48.67 µs** (44.92–48.83 µs) | 61.92 µs | 347.50 µs |
| Hybrid (fused) | **100.58 µs** (92.58–101.67 µs) | 117.67 µs | 453.88 µs |

Hybrid is **one SQL statement**, not two queries and a client-side merge.

BM25 fell **roughly 7x** and hybrid **roughly 4.5x** against that
historical baseline (this session's own three runs give 7.12–7.74x and
4.46–4.90x). **All three legs moved against us this edition and all three
moves are inside or at the edge of their own spread**: BM25 44.67 → 48.67
µs (+9%, spread 8.0%), hybrid 94.00 → 100.58 µs (+7%, spread 9.0%), vector
59.54 → 63.08 µs (+6%, spread 32.6% — the loudest core cell on the page
this edition, its three runs reading 59.08, 79.63 and 63.08 µs). Nothing in
`ea1712c..72d7ab1` is on any of the three paths, so **all three are
published unattributed and none is called a regression**. Eight editions
now read 51.21, 46.67, 50.50, 46.42, 49.42, 45.96, 44.67 and 48.67 µs on
BM25 with no code touching `bm25.rs` in any of the ranges between them: the
band is roughly 45–51 µs and the ratio against the fixed `9aba437` baseline
is roughly 7-8x on BM25 and roughly 4.5-5x on hybrid, to one digit. The
ingest figure moved 19,074 → 19,771 docs/s (+4%) against a 7.9% spread,
inside it, after the previous edition's unexplained +15%. Seven editions
have put the vector leg at 87.88, 74.67, 66.25, 74.04, 60.33, 59.54 and
63.08 µs. The underlying rewrite is still code, from the
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

BM25 was 79% of the hybrid p50 before this; it is now 48%, and the vector leg
is the larger half. Per-block impact bounds (block-max WAND) are the next step
and are not implemented.

---

## Against DuckDB, pgvector and Meilisearch

One corpus, one set of queries, one exhaustive ground truth, each engine asked
for its own query plan so an unindexed row cannot masquerade as an indexed one.
5,000 documents, dim 128. **Carried forward unchanged into the 2026-09-06
edition: `compare.sh` was not re-run at `72d7ab1`, so every figure in this
table is the 2026-09-05 sitting's.** It was regenerated then, at `b873f4e`,
via `REPEATS=3 ./bench/repeat-compare.sh` — the second gated, repeated
edition of this table, and the first clean gated `repeat-compare.sh` since the one it
replaces: median of three complete runs, `dirty: no`, none `CONTAMINATED`,
load sampled every 5 s through the measured phases and disclosed per run
(run 1 min 1.66 / median 2.80 / max 3.23, run 2 min 2.13 / median 3.30 /
max 4.25, run 3 min 1.58 / median 2.35 / max 3.66, all of 18 and all under
this box's 4.5 ceiling — run 2 is the loudest sitting of the three and it
is the run that produced several of the widest cells below), 30 s cooldown
between repetitions. Raw output:
`bench/results/20260905T062213Z-repeat-compare.txt`, from
`bench/results/20260905T{062620,063102,063530}Z-compare.txt`. The edition it
replaces is `bdc64eb` (`bench/results/20260902T185304Z-repeat-compare.txt`,
published by `832f89e`), and every "previous edition" below means that one.
Nothing in `bdc64eb..b873f4e` touches a retrieval path: the engine range is
AHL-551 (the point cursor's retained descent path), AHL-552 (the decoded
page cache's dead pages), AHL-553 (the commit barrier no longer growing the
file), AHL-554 (measured and *not* landed), AHL-555 (server-side
instrumentation only), three Track F security fixes and F3's refusal path
and `user list` — so no move in this table is attributed to the engine.

| Engine | recall@10 | vector p50 (median, range) | hybrid p50 (median, range) |
| --- | --- | --- | --- |
| **InlaySQL** (HNSW + BM25) | 1.000 | **93.00 µs** (89–101 µs) | **167.00 µs** (157–171 µs) |
| DuckDB (exhaustive + fts BM25) | 0.999 | 4.93 ms (4.82–4.97 ms) | 12.35 ms (12.29–12.88 ms) |
| DuckDB (vss HNSW + fts BM25) | 0.993 | 4.00 ms (3.96–4.04 ms) | 11.37 ms (11.25–11.50 ms) |
| Meilisearch (`arroy` ANN + its own ranking) | 0.999 | 1.24 ms (1.22–1.43 ms) | 4.15 ms (4.12–4.47 ms) |
| pgvector (HNSW + `ts_rank`) | 0.988 | 158.00 µs (156–168 µs) | 14.11 ms (13.81–14.50 ms) |
| pgvector (exhaustive + `ts_rank`) | 0.999 | 506.00 µs (490–530 µs) | 14.13 ms (13.72–14.27 ms) |

**Hybrid is roughly 25x** the nearest baseline (4.15 ms, Meilisearch;
24–28x per run) and **roughly 70-90x** DuckDB/pgvector (67–79x DuckDB,
82–92x pgvector across the three runs' own pairings). The previous edition
read ~20x and ~60-70x, and **the whole of that difference is our own cell
landing lower inside the spread that edition had already measured**: our
hybrid p50 moved 192 → 167 µs, and 167 sits well inside the 156–196 µs
band the last repeat published for it, while every baseline moved 2–4% the
other way (Meilisearch 4.04 → 4.15 ms, DuckDB-vss 11.14 → 11.37, pgvector-
HNSW 13.38 → 14.11). Nothing in `bdc64eb..b873f4e` touches a retrieval
path, so **the ratio is published as a band and the move is unattributed**
— a reader comparing the two editions should read "roughly 25x, was
roughly 20x, both inside one cell's own spread", not a gain. The spread
itself is tighter than last time on our side and looser on Meilisearch's:
our vector p50 ran 89 / 93 / 101 µs (13%, against 36% last edition) and
our hybrid p50 157 / 167 / 171 µs (9%, against 23%), while Meilisearch's
vector p50 ran 1.22 / 1.24 / 1.43 ms (17%) where it held within 3% before.
It is still not one query against one query — it is one statement here
against two queries plus client-side rank fusion there, Meilisearch
included — and `bench/README.md` says so plainly.

**Vector-only against pgvector: ahead 3 of 3 by 1.5–1.9x, where the last
edition called the same pair a tie.** 93 µs against pgvector-HNSW's 158 µs
(both include pgvector's socket round trip a library in your own process
does not pay); per run the pair read 89 vs 168, 101 vs 156 and 93 vs 158 µs
— 1.89x, 1.54x, 1.70x, every one of them clear of both cells' own spreads
this sitting. Read that as the previous edition's "close" resolving in our
direction on this sitting, **not as the engine getting faster**: 93 µs is
inside the 88–134 µs band that edition measured for our cell and 158 µs is
inside its 146–187 µs band for pgvector's, and no commit in
`bdc64eb..b873f4e` is on this path. Two gated sittings now disagree about
whether this pair is a tie or a 1.7x win, which is itself the finding. Against Meilisearch's 1.18 ms it is not a fair
fight in InlaySQL's favour so much as a different product: Meilisearch's
ANN search also runs its own typo-tolerance and ranking pipeline, which
pgvector's raw `<=>` operator does not. Meilisearch's `agree` (0.419) sits
in the same range as pgvector's `ts_rank_cd` rows (0.457/0.465) for the
same reason both are below DuckDB's real BM25: neither ranks text with BM25
at all. Recall is again the one column that did not move: every engine's
recall@10 landed within 0.001 of its last-edition figure (pgvector-HNSW
0.987 → 0.988; every other row identical to three digits).

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

**Carried forward unchanged into the 2026-09-06 edition: `compare.sh` was
not re-run at `72d7ab1`, so every figure below is the 2026-09-05 sitting's.
Regenerated then at `b873f4e` — gated, repeated, `dirty: no`, median of
three, and the first edition whose containerised write row is measured on a
build that carries AHL-553.** `REPEATS=3
./bench/repeat-compare.sh`, load gate on, every sample under the 4.5
ceiling (per-run max 3.23 / 4.25 / 3.66 of 18), none `CONTAMINATED`, 30 s
cooldown between repetitions, median of three published with each run's own
figure. Raw: `bench/results/20260905T062213Z-repeat-compare.txt`, from
`bench/results/20260905T{062620,063102,063530}Z-compare.txt`. It replaces
the `bdc64eb` edition of 2026-09-02/03
(`bench/results/20260902T185304Z-repeat-compare.txt`, published by
`832f89e`), which is what "the previous edition" means throughout this
section. **Nothing changed underneath the MySQL or PostgreSQL columns**:
same `mysql:8.4` (LTS) image with `--innodb-buffer-pool-size=512M`, same
`postgres:17` with `shared_buffers=512MB`, same drivers, same compose file
for those two services — so any move in their cells between these two
editions is the machine, and this section says so where it happens.
**One engine change is on this section's write path and it is the reason
this edition exists**: `bdc64eb..b873f4e` contains AHL-551 (the point
cursor keeps its descent path), AHL-552 (a commit stops leaving the decoded
page cache full of superseded pages), **AHL-553 (a commit's barrier no
longer pays to grow the file — the data area is extended, and zero-filled,
eight mebibytes ahead of the writer)**, AHL-554 (measured and deliberately
not landed), AHL-555 (server-side counters only), three Track F security
fixes, and F3's refusal path and `inlaysql user list`. AHL-553 is the only
one of those on the single-row durable commit, and `PERF.md` (2026-09-04)
measured it at a **paired ratio median of 1.181x over 12 interleaved
repetitions, 11 of 12 wins on throughput and 11 of 12 on p50**, on this
exact shape and this exact container volume class — that is the
attributable figure, and the Writes paragraph below spends it carefully.
The host row is not expected to move (the same A/B is flat to a small loss
on `F_FULLFSYNC`, where the barrier is 99% of the commit). AHL-551 and
AHL-552 are read paths; AHL-552's effect on this table's read column is
discussed under Reads. The interleaved rerun of 2026-08-30 stays below as
one paragraph of history; the "Correction" stays because its transport-tax
accounting is still the right way to read this table.

**Reads: we win by a very wide margin, wider than any earlier edition
found. Sequential writes: the published loss to both servers is gone, and
what replaced it is a tie inside a spread nobody should read past.**

InlaySQL is measured twice — on the host with a real `F_FULLFSYNC` barrier,
and **inside a container on the same volume class as the servers**, so all
three pay the same virtualised fsync. The gap between the two InlaySQL rows is
what that virtualisation is worth on this machine.

| Engine | write ops/s (median; runs) | read ops/s (median; runs) | read p50 |
| --- | --- | --- | --- |
| InlaySQL, host (real `F_FULLFSYNC`) | 257.9 (257.9 / 260.2 / 257.6) | 712,657 (1,467,047 / 630,501 / 712,657) | 1 µs |
| InlaySQL, containerised | 876.0 (612.6 / 1,605.5 / 876.0) | **2,018,526** (2,001,217 / 2,058,765 / 2,018,526) | 0 ns |
| MySQL 8.4 (`innodb_flush_log_at_trx_commit=1`, binlog off) | 797.2 (797.2 / 770.2 / 1,579.7) | 10,103 (10,285 / 10,103 / 10,080) | 98 µs |
| PostgreSQL 17 (`fsync=on`, `synchronous_commit=on`) | **977.4** (977.4 / 781.6 / 1,430.5) | 57,524 (54,168 / 69,376 / 57,524) | 15 µs |

The per-run figures are printed in run order, and on the write column they
are the whole story: read the medians only after the Writes paragraph
below, which pairs the runs.

Commits-per-fsync, bracketed around the write phase: MySQL 0.96
(0.96–0.99), PostgreSQL 1.00 in all three — one durable barrier per commit
on both servers, as the single-connection shape requires, unchanged from
the previous edition.

**Reads: ~200x MySQL 8.4 and ~35x PostgreSQL 17 at the medians**,
containerised — an in-process library against a socket round trip, an
asymmetry that is structural and stated, not hidden. Pairing the runs the
pair is 195–204x and 30–37x. The previous edition read ~67x/~12x, **and
every bit of that move is our own containerised read cell**: 704,742 →
2,018,526 ops/s, 2.9x, while both servers held (MySQL 10,498 → 10,103, its
own three runs inside 2%; PostgreSQL 58,415 → 57,524, runs 54.2k–69.4k).

**Part of that 2.9x has a commit behind it and part of it does not, and the
two are separable here.** The attributable part is the tail. AHL-552 found
that this driver's own shape — 20,000 single-row durable commits, then
5,000 point lookups — left the decoded page cache full to the byte of
*superseded* pages while the leaves holding the rows were not resident at
all, and made a commit admit the pages it just wrote and drop the ones it
superseded; `PERF.md` measured that interleaved on this exact shape at p95
3.17 / 2.63 / 2.67 → 0.92 / 1.42 / 1.38 µs and p99 5.08 / 4.54 / 4.21 →
1.33 / 1.83 / 1.79 µs, 3 of 3 non-overlapping each. This table's
containerised row moved the same way and further: p95 4 µs → 1 µs, p99
13 µs → 1 µs, p50 1 µs → 0 ns (the driver's resolution floor). Since this
driver's ops/s is five thousand lookups' total wall clock, it pays that
tail directly, so the direction is the commit's. **The size is not**:
`PERF.md`'s own A/B calls this suite's *ops/s* mixed in sign, not 2.9x, so
the excess is the sitting and is published as a band — 2.00M–2.06M this
edition against 577k–862k last — with the multiples read as *hundreds of
x* and *tens of x*, never as digits. One corroborating shape change rather
than a number: the containerised read cell's own spread collapsed from 40%
to **2.9%**, making it the tightest ops/s cell in this repeat where it was
among the loudest, which is what removing a tail looks like and is not what
a machine getting luckier looks like. InlaySQL's *host* read row is the
loud one now: 1,028,190 → 712,657 at the medians, spanning 630k–1,467k
(117%, the widest ops/s cell in this repeat) on the same binary and the
same data — recorded as unattributed, exactly as the point-read section
above records its own swings.

**Writes: the published loss to both servers is gone, and what replaced it
is not a win — it is a tie inside a spread nobody should read past.** At
the medians we read 876.0 against MySQL 8.4's 797.2 (1.10x, our way) and
against PostgreSQL 17's 977.4 (0.90x, theirs). Pair the runs — the only
comparison in which the three engines were measured in the same sitting —
and the picture is not that at all: 612.6 vs 797.2 / 977.4, then 1,605.5 vs
770.2 / 781.6, then 876.0 vs 1,579.7 / 1,430.5. **InlaySQL is ahead in 1 of
3 against each server**, by 0.55x–2.08x against MySQL and 0.61x–2.05x
against PostgreSQL. The medians are taken per column, independently, so the
run that supplied our median (876.0) is also the run in which both servers
posted their best figures — the 1.10x over MySQL is partly that
arithmetic, and it is published here as a band and a tie, not as a result.
This is the *third* consecutive edition in which which engine leads this
row changes, and each time both engines' cells span more than the gap.

**How the move splits between the engine and the sitting, since the
temptation to bank it as a 1.4x engine win is real.** Two things happened
between `bdc64eb` and `b873f4e` and only one of them is code.

1. *The engine.* AHL-553 stops a commit's barrier paying to grow the file:
   the data area is extended, and zero-filled, in geometric chunks up to
   eight mebibytes ahead of the writer, so the barrier no longer has to
   flush an extent allocation and a new inode size along with the data.
   `PERF.md` (2026-09-04) measured it after it landed, two binaries from
   one source tree differing only in `crates/inlaysql/src/device.rs`,
   alternating process by process with the order flipped every repetition,
   1,500 durable single-row commits each on this same container volume:
   **paired ratio median 1.181x, 11 of 12 wins on throughput and 11 of 12
   on p50 (1.359 → 1.069 ms)**. That is the attributable number, and it is
   the only engine change in the range on this path.
2. *The sitting.* Both servers' cells moved substantially on code, images,
   tuning and drivers that did not change at all: **MySQL 910.3 → 797.2
   (−12%) and PostgreSQL 762.8 → 977.4 (+28%)**, in opposite directions,
   with per-run spans of 770–1,580 and 782–1,431. Our own containerised
   cell moved 619.8 → 876.0, which is 1.41x.

Put those together rather than either alone. 1.181x applied to the previous
cell lands at roughly **732 ops/s** — still below MySQL's *new* 797.2 and
well below PostgreSQL's *new* 977.4. So the engine's measured share does
not, on its own, turn either published loss into a win; the remaining
~1.20x on our own cell and the whole of both servers' movement are the
machine. **What is honestly claimable is 1.18x, measured, interleaved, 11
of 12 — not the 1.41x this cell shows and not the ratio flip against
MySQL, and most of why the ratios flipped is that the servers moved
underneath us.** The engine's share is real and is not buried; the rest is
sitting variance and is not banked.

Our host row barely moved: 246.8 → 257.9 ops/s (+4.5%) at p50 3.91 →
3.90 ms — the barrier is the same barrier, AHL-553's own host A/B is flat
to a small loss because `F_FULLFSYNC` is 99% of a host commit, and this
cell's three runs (257.6 / 257.9 / 260.2) are the tightest on the page, so
the +4.5% is recorded as a small unattributed move rather than as a gain.
What is structural, and unchanged: this workload is one commit at a time
on one connection, so group commit cannot fire by design, and the remaining
cost is per-commit against InnoDB's redo write. The host-versus-container
gap is now **3.4x at the medians (257.9 vs 876.0) and 2.4–6.2x per run** —
that is what the host's `F_FULLFSYNC` costs against the volume's barrier on
this machine, it is wider than the 2.5x the previous edition measured, and
AHL-553 is expected to widen exactly this gap since it pays on the
virtualised barrier and not on the host's. It is the number the
batch-insert section further down needs, and it is a band this time.
The concurrent-writer story (the
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

**Carried forward unchanged into the 2026-09-06 edition: `compare.sh` was
not re-run at `72d7ab1`.** It was regenerated on 2026-09-05 at `b873f4e`,
as the last phase of the same `REPEATS=3 ./bench/repeat-compare.sh` sitting
as the two tables above —
the second repeated, gated edition of this 1/8-connection table, replacing
the `bdc64eb` edition of 2026-09-02/03. The process-based driver
(`f8e29e9`, 2026-08-27: each connection a spawned OS process, not a Python
thread, so `mysql.connector`'s GIL cannot be in these numbers) is
unchanged, and so is the MySQL side of the stack (8.4 LTS,
`innodb_buffer_pool_size=512M`). The workload is the driver's default —
2,000 durable single-row writes per connection level (the bracketed commit
counters, 2,000–2,044 per level, are the check; the raw file's header line
prints the OLTP phase's 20,000/5,000, not this phase's) — the same shape
every earlier edition of this table used.

**The client changed on the InlaySQL side of this table, and a reader
comparing editions needs to know it before comparing a single cell.** Track
F's compose change (verified working before this run) altered how this
table's InlaySQL rows are reached: `inlaysql serve --mysql` now binds
**the compose service name** rather than `0.0.0.0`, so the listener sits on
the RFC1918 bridge address and `--plaintext-network` checks that rather
than taking it on trust; and the driver **logs in as `bench`, an account
created by `inlaysql user add --superuser` against the database file before
`serve` starts, rather than as `root` via `--user`/`--password`** — because
a database whose only credential is `--user`/`--password` is now refused a
network bind under every flag. MySQL's side of the table is untouched by
this. Nothing here was measured for its cost, and it is named rather than
attributed: the InlaySQL rows below are reached by a different login and a
different bind than the previous edition's, and the read column moved in a
direction this document cannot separate from that.

| Engine | Connections | write ops/s (median; runs) | write p50 / p99 | read ops/s (median; runs) | commits-per-fsync |
| --- | --- | --- | --- | --- | --- |
| **InlaySQL** (`inlaysql serve --mysql`) | 1 | 789.5 (849.6 / 759.9 / 789.5) | 1.16 ms / 3.17 ms | **9,386.1** (9,386.1 / 9,498.4 / 8,436.0) | 1.00 |
| **InlaySQL** (`inlaysql serve --mysql`) | 8 | 1,456.5 (1,423.6 / 1,456.5 / 1,560.7) | 2.66 ms / 21.33 ms | 8,185.2 (7,463.7 / 8,185.2 / 8,401.2) | 3.89 (3.71–3.99) |
| MySQL 8.4 | 1 | 890.7 (890.7 / 863.5 / 1,341.8) | 1.02 ms / 2.36 ms | 8,904.1 (8,974.9 / 8,904.1 / 5,256.2) | 0.97 (0.95–0.99) |
| MySQL 8.4 | 8 | **4,837.8** (4,837.8 / 3,096.7 / 4,862.3) | 1.21 ms / 5.64 ms | 7,890.1 (7,055.5 / 8,981.5 / 7,890.1) | 3.90 (3.87–3.90) |

Retries were zero on both engines at both levels in all three runs, as in
the previous edition. InlaySQL's checkpoint-inclusive commits-per-fsync
(the `Inlaysql_commit_tickets`/`_flushes` pair) reads 1.00 at one
connection and 3.76 (3.57–3.81) at eight, alongside the normal-commit
column above.

**Writes: still a loss at both levels — ~0.89x at one connection and
~0.30x at eight — but the one-connection cell is this edition's cleanest
engine result on any page.** Per run the 1-connection ratio was 0.95x,
0.88x and 0.59x and the 8-connection ratio 0.29x, 0.47x and 0.32x — MySQL
ahead in 6 of 6 pairs, so the sign is not in doubt, but MySQL's own write
figures span 863.5–1,341.8 (55%) and 3,096.7–4,862.3 (57%), so read the
multiples as those per-run bands and not as the medians.

**The 1-connection InlaySQL cell moved 668.9 → 789.5 ops/s, and its two
editions' ranges do not overlap** (663.2–694.6 then, 759.9–849.6 now):
that is **1.18x**, which is AHL-553's own measured paired ratio of 1.181x
to three digits, on a row whose server is the same binary family, reached
over the same bridge, writing to a named Docker volume of the same class
the `PERF.md` A/B used. Of everything in this edition it is the figure
most cleanly attributable to a commit, and it is stated as a coincidence of
precision rather than a claim to three digits: what is measured is a
non-overlapping ~1.2x on a shape where an interleaved A/B independently
found ~1.2x, and the write p50 fell with it (1.38 → 1.16 ms). It does not
change the verdict — 789.5 against 890.7 is still a loss — it narrows it,
from ~0.64x to ~0.89x at the medians. **At eight connections the same
change does not show**: 1,522.2 → 1,456.5 with the ranges overlapping
(1,397.6–1,522.6 then, 1,423.6–1,560.7 now), which is flat, and the
mechanism says why it should be: at eight connections the coordinator
already rides ~3.9 commits on each barrier, so a cheaper barrier is
amortised across four writers instead of being paid per commit.

From one connection to eight InlaySQL's writes now scale 1.8x (789.5 →
1,456.5, down from 2.3x, because the one-connection end got faster) and
MySQL's 5.4x (890.7 → 4,837.8). The commits-per-fsync column says the gap
is still not a batching gap: at eight connections InlaySQL's coordinator
rides 3.89 commits on each barrier and InnoDB's group commit 3.90 — parity
— so the whole of the throughput gap is barrier *rate*: 1,456.5 / 3.89 ≈
374 fsyncs/s against 4,837.8 / 3.90 ≈ 1,241, a **3.3x** difference in how
often each server gets to flush at all, reproducing the 3.4x (≈375 vs
≈1,280) the previous edition measured and the 2.8–3.2x the 1/4/16 sweep
below found on 2026-08-31. That is now three gated sittings agreeing, and
`PERF.md`'s AHL-555 built the server-side instrument for it. The p99
column says it a third way: at eight connections InlaySQL's write tail is
21.33 ms against MySQL's 5.64 ms (3.8x; 19.31–25.22 ms against
4.28–7.66 ms across the runs, ranges not overlapping), where at one
connection the two are 3.17 vs 2.36 ms.

**Reads: a small win at one connection, a tie at eight, and both cells
moved down about 10% on a path where the client changed.** At one
connection InlaySQL read 9,386.1 against 8,904.1 (per run 1.05x, 1.07x,
1.61x — ahead 3 of 3, but the 1.61x is MySQL's one bad run of 5,256.2
against its own 8,904–8,975, not our good one, so the finding is ~1.05x
and not the median-of-ratios). At eight, 8,185.2 against 7,890.1 (1.06x,
0.91x, 1.06x — mixed sign, a tie). **Both InlaySQL read cells fell against
the previous edition and the fall is larger than either cell's own
spread**: 10,292.4 → 9,386.1 at one connection (9,627.5–10,772.5 then,
8,436.0–9,498.4 now — barely non-overlapping) and 9,067.7 → 8,185.2 at
eight (8,956.7–10,384.4 then, 7,463.7–8,401.2 now — not overlapping at
all), while MySQL's own read cells held (8,789.2 → 8,904.1 and 8,344.8 →
7,890.1, both inside their spreads). **It is published as a band and left
unattributed**, because the honest state is that two things on that path
changed at once and neither was measured: the engine range
(AHL-551/552 are read-path commits and both were measured *flat to
better* on point reads, so they do not predict this direction), and the
client change described above — the server now binds its service name and
the driver authenticates as a real account, `bench`, created by
`inlaysql user add`, where it used to authenticate as `root` through
`--user`/`--password`. An account lookup on connect and a privilege check
per statement is a plausible mechanism for a few per cent on a read row
and is named here as a *candidate*, not a cause: nobody has run the two
logins against each other, and until someone does, ~10% on this row is
unexplained. InlaySQL's reads from one connection to eight moved 9,386.1 →
8,185.2 at the medians — −20%, −14% and 0% per run, against MySQL's own
8,904.1 → 7,890.1 (−11%) — which continues not to reproduce the 30% fall
one ungated run showed in 2026-08-29 in the way that run described it, on
a step where both engines now move together.

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
`72d7ab1` (2026-09-06 afternoon, median of three — the previous five
editions likewise reused their own `run.sh` figures for those cells), which
means three disclosures: they are a different sitting from the server
columns, they are a *later* build than the server columns' `bdc64eb` by
AHL-544 through AHL-566 (of which AHL-549, AHL-550, AHL-551 and AHL-559
moved the `LIMIT` join and range cells, as the sections above say — AHL-559
by +14% and +13% on its own interleaved A/B — while nothing in
`ea1712c..72d7ab1` is on a read path at all), and the `run.sh`
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
| **InlaySQL** (`run.sh` at `72d7ab1`, gated median of three) | 128,383 (128k–129k) | 7.13 µs |
| *SQLite, journal — reference, same in-process harness as InlaySQL* | *144,788* | *6.63 µs* |
| PostgreSQL 17 | 21,824 (9,009–22,931) | 44 µs |
| MySQL 8.4 | 14,330 (14,181–14,635) | 67 µs |

**Read the SQLite row first.** It is the same statement over the same data,
and it is *not* comparable to the server rows either: it runs in this
process, as InlaySQL does, while MySQL and PostgreSQL answer a Python client
over a unix socket. SQLite is ~10x MySQL 8.4 and ~6.5x PostgreSQL here, which
is the same band InlaySQL is in — so **the multiple against the servers on
this shape is mostly the client and the round trip, not the storage engine**,
and the honest reading of the three numbers together is: InlaySQL is 1.08x
slower than SQLite at reading a fifty-row range on p50 (the loss the
secondary-index section owns, 1.13x on ops/s), and both in-process engines are far ahead of two servers being
asked the same question over a socket. A reader who sees "loses to SQLite,
beats the servers by 8x" and suspects one of the two is broken is right to
ask: neither is — both harnesses assert the row count before timing and
refuse to publish a wrong answer (`indexed.rs`'s `debug_assert_eq!` on both
sides, `read_driver.py`'s "refusing to time a wrong answer") — they simply
measure different things.

**~9.0x MySQL 8.4 and ~5.9x PostgreSQL at the medians** (was ~8.8x/~5.8x
with the `ea1712c` cell, ~8.3x/~5.4x with `be95cc3`'s, ~8x/~5.5x with
`1f7921a`'s, ~7x/~4.5x with `3cf0d85`'s, ~3.7x/~2.3x on 2026-08-31). The
servers' columns are the 2026-09-02/03 sitting's and did not move;
InlaySQL's cell moved 126,183 → 128,383 (+1.7%) — larger than this
sitting's own 0.9% run-to-run spread and far smaller than the previous
sitting's 19%, which is what sitting-to-sitting movement looks like.
Nothing in `ea1712c..72d7ab1` is on a read path, so it is published
unattributed. The previous edition's
step from 118,489 is the one with a name, **AHL-559** (+14% interleaved on
this exact shape, 4 of 4 non-overlapping). The step before it, 97,624 →
119,219, is AHL-550's residual filter compiled once per execution
(1.22–1.36x interleaved on this exact shape) over AHL-541's 1.04x; AHL-551
measured a further 3–7% on this shape interleaved, 6 of 6, which this cell
could not see and did not claim. The step before it, 49,259 → 97,624, was two things at once: the
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

| Shape | InlaySQL p50 (`72d7ab1`) | MySQL 8.4 p50 (median, range) | PostgreSQL 17 p50 (median, range) |
| --- | --- | --- | --- |
| PK inner, full join | **3.12 ms** | 13.68 ms (13.64–13.71 ms) | 9.36 ms (9.28–9.47 ms) |
| Secondary-index inner, full join | **3.38 ms** | 13.71 ms (13.68–13.83 ms) | 9.42 ms (9.30–9.49 ms) |
| PK inner, LIMIT (ours 10, theirs 20) | 3.29 µs | 44 µs (42–44 µs) | 29 µs (28–30 µs) |
| Secondary-index inner, LIMIT (ours 10, theirs 20) | 5.00 µs | 51 µs (49–52 µs) | 30 µs (28–30 µs) |

Both servers hash-join either FROM order in ~13.7/~9.4 ms — the
iteration-side asymmetry that used to split InlaySQL's own two full-join
shapes does not exist for them, and as of AHL-524 it no longer exists for
InlaySQL either. **Full joins: ~4.1-4.4x MySQL 8.4 and ~2.8-3.0x PostgreSQL on
both shapes.** The 2026-08-31 edition had the PK-inner full join at 13.04
ms — a TIE against MySQL and the one red cell against PostgreSQL (LOSS
~1.24x), "the shape where PG's planner picked the better order". That cell
is gone for a named reason: AHL-524 (`PERF.md`, 2026-09-02) fixed AHL-512's
inverted cost model so both written orders run the same users-driving
plan, measured 9.34 → 3.21 ms on this shape in a single run, 3.23 ms in
the gated `3cf0d85` median, 3.25 ms at `1f7921a`, 3.26 ms at `be95cc3` and
3.27 ms at `ea1712c` and 3.12 ms in this edition's. The servers' own
full-join columns moved 15.00/15.01 → 13.68/13.71 ms (MySQL, version
change and quiet machine) and 10.49 → 9.36/9.42 ms (PostgreSQL, quiet
machine) — the direction of a quieter sitting, unattributed. **The `LIMIT`
rows are not the same shape on both sides** — `LIMIT 10` from `run.sh`
against the drivers' `LIMIT 20` — so the arithmetic (10–14x MySQL, 6–9x
PostgreSQL; the InlaySQL cells moved 3.25 → 3.29 and 5.25 → 5.00 µs this
edition, per the joins section, which attributes neither move to anything —
no commit in `ea1712c..72d7ab1` is on a read path) overstates a like-for-like comparison by
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

### Batch insert — like for like, a WIN against MySQL 8.4 and a TIE against PostgreSQL (0.88x inside a 21% A/A band); on the host, the barrier

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
| InlaySQL (host, `F_FULLFSYNC`, `bdc64eb`) | 24,102 (23,219–24,736) | 241 | 1.00 |
| **InlaySQL (containerised, same volume class as the servers; `bench/batch_insert.sh`, `bcbc9d4`, 2026-09-07)** | **88,456** (76,176–94,459) | 885 (rows/s ÷ 100) | 1.00 |
| MySQL 8.4 (containerised, `bdc64eb`) | 56,700 (45,244–68,901) | 567 | 0.71 (0.62–0.76) |
| PostgreSQL 17 (containerised, `bdc64eb`) | **99,212** (93,776–100,749) | 992 | 1.00 |

**Like for like — the containerised row against the containerised servers —
InlaySQL is 1.64x MySQL 8.4 and 0.88x PostgreSQL 17.** The PostgreSQL cell is
restated 2026-09-07 (T0.6) as a **TIE, not a LOSS**: 0.88x sits inside
InlaySQL's own 21% round-to-round spread on this workload, and a delta inside
its band is not a result. The verdict is re-measured at the next gated
regeneration with the band attached, not edited again by hand. The previous
edition
published 1.19x and 0.68x. **Nothing was built to move it.** AHL-570 changed
no engine code; it changed the harness. What it re-measured is `bcbc9d4`, the
build every `run.sh` table in this edition already comes from, against a
published cell taken at `bdc64eb` — four commit-path changes earlier
(AHL-553, AHL-563, AHL-564, AHL-566) and, worse, taken in a way that could
not have detected them. Read this as a better measurement of the same
engine, not as a fix that landed.

**The containerised row's provenance, and it is not this edition's other
provenance.** It comes from `bench/batch_insert.sh` (added by AHL-570), which
runs one `REPS=5` repetition of each of the three engines per round and
**rotates which engine goes first**, so a drift lands on all three arms
instead of one. Five rounds, 2026-09-07, `bcbc9d4`, all three engines in
containers on named volumes (`Durability::Full` /
`innodb_flush_log_at_trx_commit=1` / `synchronous_commit=on`), the server
volumes recreated first, `bench/load_gate.sh`'s sampler across the measured
phases only: load 3.23/3.34/3.43 min/median/max of 18 against the 4.5
ceiling, **no round marked `CONTAMINATED`**. **This is a different harness
from `compare.sh`**, so this is not a `repeat-compare.sh` edition and nothing
else on this page moved with it: the neighbouring driver-sourced rows — the
read shapes, and this table's own MySQL, PostgreSQL and host cells — are
still `bdc64eb` of 2026-09-02/03.

**Why the published cell could not have detected them.** It was assembled
from `sql_shapes --mode batch` run once, `batch_driver.py TARGET=mysql` run
once and `TARGET=postgres` run once — and its InlaySQL cell came from a
*different sitting a day after* its two server cells, under three concurrent
build agents, with the 17% spread it disclosed. That is the reading
`bench/profile_ab.sh` exists to refuse.

The five interleaved rounds, and the A/A spread beside the ratio because the
ratio is not much bigger than it:

| | InlaySQL | MySQL 8.4 | PostgreSQL 17 |
| --- | --- | --- | --- |
| median of five rounds | **88,456** | **52,935** | **100,305** |
| round-to-round range | 76,176–94,459 | 51,097–58,045 | 99,074–103,694 |
| spread as % of median | 21% | 13.1% | 4.6% |
| same rounds vs the published cell | +31% | 0.93x | 1.01x |
| ratio to ours | — | 1.64x | 0.88x |

**A 21% A/A spread does not by itself carry a 1.31x move, and it is not
offered as if it did.** What carries the move is the control. Both opponents
reproduce their published cells in the same rounds — PostgreSQL 100,305
against 99,212 (1.01x), MySQL 52,935 against 56,700 (0.93x) — so the machine
has not moved; ours has. **All five of our round medians clear the published
run's maximum** (76,176 > 70,943), 5 of 5, which is the non-overlap rule
`PERF.md` §4 asks for. No attempt is made here to split the +31% among the
four changes that sit between the two builds.

**On the host the row is LOSS ~2.4x vs MySQL 8.4, ~4.1x vs PostgreSQL**
(was ~1.6x/~3.1x on 2026-08-31), and **that ratio is the barrier, not the
engine.** InlaySQL's 241 commits/s is 4.1 ms per statement, and the host
single-row write in the OLTP table above pays the same barrier at 257.9
ops/s, 3.90 ms p50 — a hundred-row statement costs what a one-row statement
costs, because both are one `F_FULLFSYNC`. The servers commit against the
Docker volume's cheaper virtualised barrier, and the OLTP table above
measures that difference, InlaySQL against itself: 257.9 on the host against
876.0 containerised, **3.4x at the medians and 2.4–6.2x pairing the runs**.
The host cell is context for the containerised one, never a verdict against a
containerised server. `PERF.md`'s AHL-542 (2026-09-03) profiled exactly this
shape and found the per-row root-to-leaf page round trip at 32% of the
statement — a hundred-row `INSERT` decoded, cloned and re-encoded each of its
~3 path pages ~100 times to write them once — and removed it, measured
interleaved at **1.29–1.44x on the engine's own batch-insert profile**, 3 of
3 non-overlapping. The host row barely moved for it (24,102 against the
previous edition's 26,254) because at 4.1 ms per barrier its ceiling is ~245
statements/s, 24.5k rows/s, and 24,102 is 98% of it. MySQL's c/fsync of 0.71
is InnoDB's log layer flushing ~1.4 times per commit at this batch size, its
background flush counted, as before.

**What separates us from PostgreSQL is not engine work, and the figure this
section used to carry for it was wrong.** Previous editions said "~1.0 ms of
engine work per hundred-row statement against PostgreSQL's ~0.6 ms". That
was a subtraction from throughput, never a measurement, and it is wrong by
6.5x. AHL-570 measured the statement with `commit_growth`'s device wrapper,
on this cell's own two-integer table rather than the 64-byte-body table every
earlier profile used: total **1.093 ms**, of which the barrier is **0.899 ms
— 82.3%**, the WAL record's `pwrite` 0.004 ms (**0.4%**), the data area's
`pwrite` 0.013 ms (**1.2%**), and **the engine above the storage layer 0.154
ms — 14.1%**. Measured against PostgreSQL's own instrument (`pg_stat_wal`
with `track_wal_io_timing=on`, bracketed as `batch_driver.py` brackets its
sync), PostgreSQL's non-barrier work on the same statement is **~0.24 ms**
against our 0.154: **we do less engine work per statement than PostgreSQL
does.** The residual is bytes through the barrier — **43.5 KiB per statement
(17.1 KiB of record plus 26.4 KiB of data area) against PostgreSQL's 13.7
KiB**, into a file that grows against a segment PostgreSQL preallocates and
recycles in place. Closing that needs physiological logging or deferred
durability; both are priced and declined (`docs/recovery.md`, `PERF.md`
AHL-564 §2 and AHL-570 §6).

Two things the same profile found, and both are byte questions rather than
syscall ones. **AHL-564's 3.62x WAL-record shrink did not carry to this
shape**: it is 1.56x here (27,265 B to 17,505 B), because a hundred-row
commit's pages are 49–78% used where a single-row commit's are 1–54%, and
what AHL-564 removed was the zero hole. And **69% of a hundred-row commit's
record is not the rows** — 47% is 3.09 spine pages at 49% used (12,670
B/commit) and 23% is the per-statement change log (6,144 B/commit); the rows
themselves are 8,315 B across 2.03 leaves. Those two are the open levers on
this row.

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
  order. The 2026-09-05 `REPEATS=3` repeat runs that same phase order and
  reads the step as −20%, −14% and −0% across the three runs (9,386.1 →
  8,185.2 at the medians), against MySQL's own −11% over the same step —
  both engines moving together, where the 2026-08-29 run had MySQL flat.
  The gated repeat before it read −7%, −12% and −4%. Not root-caused, and not claimed fixed: nothing changed in the
  server's connection model or the driver between the run that showed it
  and the three that did not, so the honest record is that a drop one
  ungated run showed is not visible in three gated ones. `inlaysql-server`'s
  thread-per-connection model was already the less likely explanation and
  stays so. What *is* still open on this table, and measured three times
  the same way, is the write side: a barrier-rate deficit of ~3.3x at eight
  connections with batching at parity, reproduced in three gated sittings — see `PERF.md`'s "Task 2" and
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
  disclosed per section: 100 of 343 metrics moved by 10% or more across the
  three main-suite runs at `72d7ab1` (18 of 133 on core columns alone — see
  the spread note at the top of this file, which also explains why the raw
  summary's own count is 175 of 527 and not comparable), and, in the carried-forward
  2026-08-30 sessions, 63 of 180 in the wide concurrency sweep and 25 of 64
  in the quantisation spot-check. A same-binary A/A test (`PERF.md` §4,
  2026-08-30) puts a number on what "spread" means here: median CoV 4.0% on
  the main suite's core columns, 3.6% on the concurrency sweep, 0.3% on the
  quantisation spot-check, and 7.3-20.2% on the single most scrutinised
  metric depending on how busy the machine was — the acceptance target (CoV
  under 3%) is not met today. This edition's whole-suite spread (100 of 343)
  is the narrowest of the seven gated sittings (106 at `7b20175`, 114 at
  `4f8e5dd`, 109 at `3cf0d85`, 114 at `1f7921a`, 128 at `be95cc3`, 109 at
  `ea1712c`) and well under the
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
