# The competitive scoreboard: SQLite, MySQL, PostgreSQL

Companion to [`BENCHMARK.md`](BENCHMARK.md) (the numbers) and
[`PERF.md`](PERF.md) (the measurement floor those numbers are read against).
This file exists because the project's stated goal — "beat SQLite, MySQL and
PostgreSQL across the board" — is not yet a claim anyone can check: almost
every published figure so far is "Nx SQLite", which says nothing about two of
the three named opponents, and a "win" has never been defined precisely
enough to tell apart from this harness's own noise. `PERF.md` §4 spent a full
session establishing that noise floor (median CoV 4.0% on the main suite,
3.6% on the concurrency sweep, 7.3% on the single most scrutinised metric on
a quiet machine, ~20% under ordinary desktop load). This document is the
first attempt to hold every cell of the SQLite/MySQL/PostgreSQL matrix to
that same discipline, rather than letting an easy comparison in and a hard
one stay silently absent.

**What this mostly is not, with one dated exception.** A benchmark run.
Every number quoted below except the §3.5/§6 MySQL concurrent-commit cells
already existed in `BENCHMARK.md` before this edition, itself the only place
a headline figure is normally allowed to originate (`AGENTS.md`). The
exception: this edition (2026-08-31) *did* run the concurrent-commits-vs-MySQL
sweep — `SERVER_CONCURRENCY_LEVELS=1,4,16`, 5 repetitions, interleaved per
concurrency level rather than target-major, load-gated (1-minute average
2.1-3.3 of a 4.5 ceiling throughout) — because that cell was the task's
explicit subject and the machine was, on this occasion, quiet enough to
trust; everything else in this document still follows the "no new
measurement, cell stays UNKNOWN rather than a plausible number" rule the
previous edition stated here. Where the honest answer is "nobody has run
this comparison," the cell still says **UNKNOWN**. A second pass the same
day, once `Inlaysql_normal_commit_flushes`/`Inlaysql_normal_commit_tickets`
went live, reran the same sweep with `inlaysql-server`'s own commits-per-
fsync bracketed alongside MySQL's — §3.5/§6 below carry both passes'
numbers, not a silent replacement of one by the other.

Provenance: every figure below except the 2026-08-31 concurrent-commits
update traces to `BENCHMARK.md` as committed at `b825f2d` (the commit this
document was originally written against, confirmed clean before writing).
`BENCHMARK.md`'s own per-section provenance notes (which tables regenerated
this edition, which are carried forward, and from which commit) apply
transitively here and are not repeated in full — follow the section
reference given for each row. The concurrent-commits figures trace to
`BENCHMARK.md`'s own new "Server-to-server, extended" subsection, this same
edition.

**Regenerated cells, 2026-09-05 (`b873f4e`) — the second gated and repeated
edition of the `compare.sh` numbers on this page, and the first clean gated
`repeat-compare.sh` since 2026-09-03.** `REPEATS=3
./bench/repeat-compare.sh` (load-gated, `dirty: no`, none `CONTAMINATED`,
per-run load max 3.23/4.25/3.66 of 18, median of three —
`bench/results/20260905T062213Z-repeat-compare.txt`, from
`bench/results/20260905T{062620,063102,063530}Z-compare.txt`) refilled
§3.1, §3.3, §3.5's 1/8-connection row and §3.8's pgvector cell, replacing
the `bdc64eb` figures of 2026-09-02/03 (published by `832f89e`) that those
cells carried. **Three verdicts move on this pass** and each says why in
place: §3.3's single-row durable write from LOSS to **TIE** against both
servers (the engine's attributable share of the move is AHL-553's measured
1.181x; the rest, and the ratio flip against MySQL, is the two servers'
own cells moving on unchanged code, and §3.3 splits it), §3.8's exact
vector cell from TIE to **WIN** on a second sitting that agrees in
direction, and §3.5's 1-connection server-to-server write staying a LOSS
but narrowing from ~0.64x to ~0.89x on the one cell in this document whose
two editions' ranges do not overlap. **The InlaySQL side of §3.5's stack
also changed and it is not an engine change**: `inlaysql serve --mysql`
now binds the compose service name rather than `0.0.0.0` and the driver
authenticates as the account `bench` created by `inlaysql user add` rather
than as `root` (Track F's compose change) — §3.5 names it where its read
column moved. The **earlier** gated pass, `REPS=5` runs of
`read_driver.py`/`batch_driver.py`/`sql_shapes` over the unix socket on a
quiet machine (`uptime` 1.47–2.36/18 before and after;
`bench/results/20260902T191343Z-scoreboard/`) refilled §3.2, §3.4, §3.6 and
§3.7, whose 2026-08-31 figures were taken under desktop load with the gate
overridden — those four sections are **still at `bdc64eb`** and were not
re-run on 2026-09-05. **The MySQL container is 8.4 (LTS) as of these runs; every
MySQL figure dated 2026-08-31 or earlier on this page was 8.0.x**, and no
edition-to-edition MySQL move is attributed to either engine. The quiet-
machine floors (§1) apply to the regenerated cells; §4.0's 20.2% desktop
floor now applies only to the 2026-08-31 figures kept as history. The
1/4/16-connection sweeps (§3.5, §3.10, §6) are still the 2026-08-31 runs.

---

## 1. Verdict rules

Fixed before any cell below was filled in, per the task this document
answers. Six possible verdicts, not four — the two the brief specified plus
two this project cannot honestly do without:

- **WIN** — InlaySQL is ahead by a margin that exceeds the measured A/A floor
  for the suite the figure comes from (`PERF.md` §4: 4.0% main suite / 3.6%
  concurrency / 7.3% point-reads-quiet / ~20% under desktop load), **and**
  the source table itself treats the gap as real rather than noise (most
  `run.sh`-sourced tables state a run-to-run range explicitly, and as of
  2026-09-02/03 so do the `compare.sh`-sourced ones, which are gated
  medians of three with their per-run figures published; a figure that is
  still a single run — `ann-benchmarks` — earns WIN only when
  `BENCHMARK.md`'s own text says the gap is too large for that table's
  noise to plausibly produce — e.g. the 12-67x read numbers below — never
  on a raw multiple alone).
- **LOSS** — the mirror image of WIN: an opponent is ahead by a margin that
  exceeds the same floor, by the same rule. A scoreboard that only records
  wins is marketing, not an audit; several cells below are LOSS, stated
  plainly, exactly as `BENCHMARK.md`'s own SQLite section already does for
  range scans and two of four join shapes.
- **TIE** — the gap is inside the floor. Not a win, not a loss, and not
  reported as either no matter how the raw numbers happen to round.
- **FLOOR-BOUND** — a *specific* reason to expect TIE, stronger than "the
  numbers happened to land close": every engine being compared pays the
  identical physical barrier (the same `fsync`/`F_FULLFSYNC` call on the
  same device, at the same durability level, with no group-commit
  opportunity for any of them because there is only ever one commit in
  flight). Marked separately from a plain TIE because the right response to
  a FLOOR-BOUND cell is "stop trying to win this one, the floor is the
  ceiling," which is a stronger and more useful statement than "the current
  measurement doesn't distinguish the two."
- **N/A** — an opponent has no comparable capability at all, stock or
  extended, and building a comparison would only be scoring a default win.
  Reserved for the *opponent* lacking the feature (e.g. MySQL 8.4 has no
  vector search of any kind). Where **InlaySQL** is the side missing a
  capability needed to run the comparison (e.g. no PostgreSQL wire
  protocol), the cell is UNKNOWN, not N/A — that is an engineering gap this
  project could close, not a fact about the opponent, and conflating the two
  would hide real missing work behind a label that says "nothing to do
  here."
- **UNKNOWN** — no comparison exists yet, the comparison that exists is not
  floor-qualified (a single, unrepeated run with no established spread —
  every `compare.sh` cell was in this state until 2026-09-02/03, and
  `ann-benchmarks` still is — and the gap is not large enough to invoke the
  WIN/LOSS exception above), or the workload has no matched-durability
  harness at all. The
  default for any cell this document cannot fill honestly, per the task's
  own instruction: **a cell that cannot be filled honestly stays UNKNOWN.
  No number is invented to complete the matrix.**

One rule cutting across all of the above: **every cell records the
durability configuration both sides ran under**, even for read-only
workloads where durability does not gate the operation itself — because the
underlying data was still written under some configuration, and because a
reader comparing this file against a future one needs to know whether the
configuration changed.

---

## 2. The matrix

| Workload | SQLite | MySQL 8.4 | PostgreSQL 17 |
| --- | --- | --- | --- |
| Point read by PK | WIN ~2-4x vs durable config; LOSS ~2x vs WAL/NORMAL | WIN ~200x (structural, §3.1) | WIN ~35x (structural, §3.1) |
| Indexed range scan | LOSS ~1.5x (durable) / ~2.5x (WAL) | WIN ~7x (§3.2) | WIN ~4.5x (§3.2) |
| Single-row insert (durable) | WIN ~2.7x | TIE (containerised; medians read 1.10x our way, but pairing the runs we are ahead in 1 of 3, 0.55–2.08x — was LOSS ~1.5x, §3.3) | TIE (containerised; medians read 0.90x, ahead in 1 of 3, 0.61–2.05x — was LOSS ~1.2x, §3.3) |
| Batch insert | UNKNOWN — no SQLite-batched comparison published | WIN ~1.2x like for like (containerised InlaySQL 67,484 rows/s vs 56,700; on the host, LOSS ~2.4x on the barrier, §3.4) | LOSS ~1.5x like for like (67,484 vs 99,212; on the host ~4.1x, §3.4) |
| Concurrent commits, 4/8/16 writers | WIN ~10-17x across 4/8/16 | LOSS @1,4,16 (~1.4-2.4x/~1.1-3.0x/~3.1-5.4x, widening with concurrency, 5 interleaved reps); LOSS @8 ~0.30x and LOSS @1 ~0.89x (gated median of 3 vs MySQL 8.4, 2026-09-05; batching at parity, barrier rate ~3.3x behind; the @1 loss narrowed from ~0.64x on AHL-553, §3.5) | UNKNOWN — no server exists (§3.4) |
| Two-table join | MIXED: WIN one shape (~5-9x), LOSS three shapes (~1.1-3.5x) | WIN all four shapes: ~4x on both full joins; several-x on `LIMIT`, on a smaller LIMIT than theirs (§3.6) | WIN all four shapes: ~2.7-2.9x on both full joins; several-x on `LIMIT`, same caveat (§3.6) |
| Aggregate / `GROUP BY` | UNKNOWN — no harness | WIN ~1.9x group / WIN ~6x scalar (§3.7) | WIN ~1.26x group / WIN ~5x scalar (§3.7) |
| Vector search, exact | N/A (stock) / WIN ~8-10x vs `sqlite-vec` ext., iso-recall | N/A — no vector capability | WIN ~1.7x — 93 vs 158 µs, ahead in 6 of 6 runs across two gated sittings; was TIE when one sitting's own spread swallowed the gap (§3.8) |
| Vector search, int8 | UNKNOWN — no cross-engine harness | N/A — no vector capability | UNKNOWN — no cross-engine harness |
| p99 commit latency | LOSS ~7-9x at high writer counts | TIE @1 (mixed sign, 4/5 reps); LOSS @4,16 (~1.5-4.5x/~2.4-8.9x, widening, 5 interleaved reps) | UNKNOWN — no server exists (§3.4) |

Fourteen UNKNOWN or N/A-for-missing-harness cells out of thirty when this
document was first written; two full cells and one partial one moved off that
count on 2026-08-31 (morning), once real, repeated MySQL data existed for 4
and 16 connections and for p99 commit latency — real numbers, not all of them
wins, replacing "nobody has run this" where the comparison was actually
possible to run — and **eight more cells moved the same day (afternoon)**,
when the read-shape and batch-insert harnesses (`bench/external/read_driver.py`,
`bench/external/batch_driver.py`, `inlaysql-bench --bin sql_shapes`) filled
the MySQL/PostgreSQL cells for indexed range scan, two-table join, aggregate
and batch insert (§3.2, §3.4, §3.6, §3.7). On 2026-09-02/03 every one of
those MySQL/PostgreSQL cells was regenerated gated and repeated, the
exact-vector PostgreSQL cell moved from UNKNOWN to TIE once a repeat
existed to judge it against, and two verdicts flipped on the engine rather
than the machine: `GROUP BY` from LOSS to WIN (§3.7) and the PK-inner full
join from LOSS-vs-PostgreSQL to WIN (§3.6). Five UNKNOWN cells remain: the
two SQLite cells that still have no harness (aggregate, batched insert),
the two int8 vector cells (explicitly out of scope until the commit path
and this red-cell survey were done), and the two PostgreSQL cells that
require a PG wire server that does not exist. That is the honest state of
"beat SQLite, MySQL and PostgreSQL across the board" today: the SQLite
column is mostly filled in (and mixed, not a sweep), and the
MySQL/PostgreSQL columns are filled and mixed too — reads, range, joins
and `GROUP BY` win; every write-side cell and the scalar aggregate lose.

**Updated 2026-09-05** (`b873f4e`, the second gated `repeat-compare.sh`):
three of those cells moved. The single-row durable write against both
servers is no longer a LOSS and is not a WIN either — it is a TIE, on a
sitting whose three runs put us ahead of each server exactly once (§3.3),
and the part of that move with a commit behind it is AHL-553's measured
1.181x, not the 1.41x the cell itself shows and not the ratio flip against
MySQL, both servers having moved substantially on unchanged code. The
exact-vector cell against pgvector moved TIE → WIN on a second sitting
agreeing in direction (§3.8). The server-to-server 1-connection write
narrowed from ~0.64x to ~0.89x and stays a LOSS (§3.5). Nothing moved from
UNKNOWN, and the five UNKNOWN cells above are unchanged.

---

## 3. Per-row detail

### 3.1 Point read by PK

**SQLite** (`BENCHMARK.md` "Point reads by primary key"): 522,562 ops/s
median vs 160,236 (journal + `sync=FULL` + `fullfsync`, **WIN, ~2-4x**, this
session's three individual ratios were 2.05x/2.91x/3.80x) and vs 1,118,819
(WAL + `sync=NORMAL`, **LOSS, ~2x**). Both gaps are far outside the
flagship point-read floor (7.3% quiet / 20.2% busy). Durability: read-only;
the underlying SQLite instance was populated and held open under the stated
config in each column; InlaySQL's own durability level is irrelevant to a
read.

**MySQL / PostgreSQL** (`BENCHMARK.md` "Against MySQL and PostgreSQL",
containerised row, gated median of three, 2026-09-05, `b873f4e`, MySQL
8.4): InlaySQL 2,018,526 ops/s (2,001,217–2,058,765 across the runs — a
2.9% spread, the tightest ops/s cell in the repeat) vs MySQL 10,103
(10,080–10,285; **WIN, ~200x**, 195–204x pairing the runs) and PostgreSQL
57,524 (54,168–69,376; **WIN, ~35x**, 30–37x). The previous gated edition
read ~67x/~12x, and all of the move is our own cell going 704,742 →
2,018,526 while both servers held (10,498 → 10,103, 58,415 → 57,524).
**Split, because only part of it is a commit**: AHL-552 stopped a commit
leaving the decoded page cache full of superseded pages, and this row's
tail moved exactly as `PERF.md` measured it interleaved (p95 4 → 1 µs,
p99 13 → 1 µs, and this driver's ops/s is 5,000 lookups' total wall clock,
so it pays the tail directly) — but `PERF.md` calls that suite's *ops/s*
mixed in sign rather than 2.9x, so the size is the sitting and the multiple
is published as a band, not a figure. The cell's spread collapsing from 40%
to 2.9% is the corroborating shape, and is what removing a tail looks like.
Far outside any floor even at the worst run. **Read this WIN in
context, not as an engine-side result**: it is InlaySQL as an in-process
library against a socket round trip on both servers — a structural
advantage, not a mechanism win, and already disclosed as such (§4.2).
Durability: MySQL `innodb_flush_log_at_trx_commit=1`, binlog off; PostgreSQL
`fsync=on`, `synchronous_commit=on`; reads themselves are not
durability-gated, but the row this reads was written under that
configuration.

### 3.2 Indexed range scan

**SQLite** (`BENCHMARK.md` "Secondary-index reads", range columns): 64,250
ops/s vs 124,662 (journal, **LOSS, ~2x**) and 182,357 (WAL, **LOSS, ~2.9x**).
Both essentially unchanged across editions and consistent in sign across all
three runs behind the median (9-21% individual spread), so read as real
losses, not noise. Durability: same as 3.1, read-only.

**MySQL / PostgreSQL** (2026-08-31, `bench/external/read_driver.py`, unix
socket, 5 shuffled reps — see §4.1's disclosure about this sitting's
desktop load): the harness rebuilds `SUITE=indexed`'s shape — `users (id,
email, body)` at 100,000 rows, index built after the rows, 100
`WHERE email >= ? AND email < ?` queries returning exactly 50 rows, key
sequence generated with the same seeded xorshift64* the Rust harness uses.
**Regenerated 2026-09-02/03, `REPS=5`, quiet machine, MySQL 8.4**:
InlaySQL 118,489 ops/s (99k–120k; the 2026-09-05 edition's gated `run.sh`
median of three at `be95cc3`, reused for this cell as the three editions
before it reused their own `run.sh` figures — a different sitting from,
and two engine editions later than, the server columns, disclosed) — with
SQLite, in-process on the same harness, at 144,622 ops/s (6.67 µs), i.e.
~10x MySQL and ~6.5x PostgreSQL itself, so this WIN is mostly the servers'
client and socket rather than a storage-engine gap — vs MySQL 8.4 14,330
ops/s (14,181–14,635, p50 67 µs) and PostgreSQL 21,824 ops/s
(9,009–22,931, one outlier rep; p50 44 µs). **WIN ~8x vs MySQL, WIN ~5.5x
vs PostgreSQL** — far outside the quiet floor. The `1f7921a` cell was
119,219 and this one is 118,489, −1% and inside its own 18% run-to-run
spread — flat, despite AHL-551 measuring a further 3–7% on this exact shape
interleaved, 6 of 6 non-overlapping, which a cell this noisy cannot
resolve. The `3cf0d85` cell was 97,624 (~7x/~4.5x), and the step from it is
AHL-550's compiled residual filter (1.22–1.36x interleaved on this shape);
the 2026-08-31 figures were 49,259 (desktop load) vs 13,124 (8.0.x) and
21,455 — the servers' columns barely moved across any step, so the wider
multiple is InlaySQL's own cell (a quiet sitting, AHL-535's measured 1.40x
and AHL-550's 1.22–1.36x on this shape), not the opponents getting
slower. Durability:
read-only after setup; MySQL
`innodb_flush_log_at_trx_commit=1`, PG `synchronous_commit=on`, both
servers on their named volumes, reached over unix sockets; InlaySQL
in-process.

### 3.3 Single-row insert (durable)

**SQLite** (`BENCHMARK.md` "Durable writes"): 240 ops/s vs 90 (journal +
`fullfsync`), **WIN, ~2.7x**, one of the tightest ratios in the whole
document (3.8%/8.9% individual spread). Both sides run one commit, one
`F_FULLFSYNC`, on the host filesystem — but this is *not* a FLOOR-BOUND cell,
even though both pay the identical hardware barrier, because they do not pay
it the same number of times: `PERF.md`'s AHL-496 count found InlaySQL at
~1.03 `fsync` calls per commit against SQLite's journal-mode protocol, which
syncs the rollback journal and then the database file separately. A shared
per-`fsync` hardware cost does not make a win physically unavailable when the
two engines don't do the same number of `fsync`s for the same row — that is
a real architectural difference, measured, not an artifact of the floor.
Durability: both `F_FULLFSYNC` on the host, full durability, one writer.

**MySQL / PostgreSQL** (`BENCHMARK.md` "Against MySQL and PostgreSQL",
gated median of three, 2026-09-05, `b873f4e`, MySQL 8.4): InlaySQL
containerised 876.0 ops/s (612.6–1,605.5) vs MySQL 8.4 797.2
(770.2–1,579.7) and PostgreSQL 17 977.4 (781.6–1,430.5). **TIE against
both**, and the reasoning matters more than the medians. Pairing the runs —
the only comparison in which the three engines were measured in the same
sitting — gives 612.6 vs 797.2/977.4, then 1,605.5 vs 770.2/781.6, then
876.0 vs 1,579.7/1,430.5: **InlaySQL ahead in 1 of 3 against each**,
0.55–2.08x against MySQL and 0.61–2.05x against PostgreSQL. The medians are
per column and independent, so the run supplying our median is also the run
in which both servers posted their best figures; by §1's rule a gap inside
the spread is a TIE no matter how the raw numbers round, and this one is
inside a spread of 113% (ours), 101% (MySQL's) and 66% (PostgreSQL's).
**This was LOSS ~1.5x / ~1.2x last edition** (InlaySQL 619.8 vs 910.3 and
762.8, ahead 0 of 3 against each).

**How the move splits, since a 1.41x cell and a flipped MySQL ratio invite
being banked as an engine win.** Only one thing in `bdc64eb..b873f4e` is on
this path: **AHL-553**, which stops a commit's barrier paying to grow the
file, measured after it landed at a **paired ratio median of 1.181x, 11 of
12 interleaved repetitions** on exactly this shape and this container volume
class (`PERF.md`, 2026-09-04). Apply that 1.181x to the previous edition's
619.8 and it lands at ~732 ops/s — **below MySQL's new 797.2 and well below
PostgreSQL's new 977.4**, so the engine's measured share does not on its own
turn either loss around. What did the rest: both servers moved
substantially on unchanged code, images, tuning and drivers — **MySQL
910.3 → 797.2 (−12%) and PostgreSQL 762.8 → 977.4 (+28%)**, in opposite
directions — and our own cell moved 619.8 → 876.0 (1.41x), of which 1.18x
is attributable and ~1.20x is the sitting. **1.18x is the claim this
document makes; the rest is the machine, and the flip of the MySQL ratio is
mostly the machine's, not ours.** Which server leads the other has now
changed in three consecutive editions and is noise; the floor-bound analysis
that follows was done on the 2026-08-30 interleaved rerun and stands.

**This looks like the textbook FLOOR-BOUND
case from the task brief — one writer, one commit in flight, no group commit
possible for any of the three engines, all writing to the same class of
Docker-virtualised volume — and it was checked against exactly that
hypothesis rather than assumed.** It does not hold up: the raw
`pwrite`+`fsync` floor probe run alongside these five repetitions had its
own spread of only 15.4%, far tighter than any engine's (50-81%), and the
correlation between the floor probe and each engine across the five
repetitions was weak (Pearson r: MySQL +0.51, PostgreSQL +0.46, InlaySQL
**-0.51**) — a genuinely floor-bound quantity would track the floor probe
closely and this one does not. `BENCHMARK.md`'s own conclusion, quoted rather
than re-derived: *"the floor does not explain most of this run's variance...
it is more likely the Python driver/connector overhead, `docker
exec`/process-spawn jitter, or the compose bridge network."* So this cell is
marked **TIE**, not FLOOR-BOUND, on the evidence — with two loud caveats:
(1) no multiple should be read off it at all this edition, given each
engine's own 66-113% spread across three gated runs, and
(2) this comparison structurally favours InlaySQL (§4.2: it skips a
transport tax MySQL and PostgreSQL both pay, of the same order of magnitude
as the entire gap), so a transport-matched rerun could plausibly narrow,
hold, or reverse this cell — the transport-matched row that exists (§3.5)
reads MySQL ~1.13x ahead at one connection on this same sitting, with that
advantage removed, which is the closest thing to a settled answer on this
workload and is a LOSS where the library row is a TIE. The host row
(257.9 ops/s, real `F_FULLFSYNC`) is **not comparable** to the containerised MySQL/PostgreSQL rows at all — different
hardware barrier classes entirely — and is excluded from this cell rather
than mixed in. Durability: InlaySQL containerised — same commit path as the
host, on a named Docker volume; MySQL `innodb_flush_log_at_trx_commit=1`;
PostgreSQL `fsync=on`/`synchronous_commit=on`; all three on a Docker named
volume of the same `local` driver class.

### 3.4 Batch insert

**SQLite: UNKNOWN** — unchanged. `BENCHMARK.md`'s "Durable writes" section
quotes "~240x" for InlaySQL's batched-vs-unbatched write rate (58,320 ops/s
batched against 240 ops/s single-row) — **that multiple is InlaySQL against
itself, not against SQLite.** No SQLite-batched-transaction number is
published anywhere in the current tables. (The 2026-08-31 afternoon
harnesses cover MySQL and PostgreSQL only; the brief scoped them that way.)
**Flagged as a documentation risk, not just a missing measurement**: the
"~240x" figure sits in a paragraph immediately after several real "vs
SQLite" ratios, with no explicit "against our own single-row rate" qualifier
in `README.md`'s prose — a reader skimming could misattribute it as a
competitive multiple. It isn't one. Recommend the prose in `README.md` and
`BENCHMARK.md` state the comparison basis explicitly wherever this figure is
quoted.

**MySQL / PostgreSQL** (regenerated 2026-09-02/03, `bench/external/batch_driver.py`
vs `inlaysql-bench --bin sql_shapes --mode batch`, unix socket, 5 reps,
quiet machine, MySQL 8.4): 100 rows per multi-row INSERT statement,
autocommitted, 100 statements per rep (10,000 rows per rep), explicit
ids. Durability aligned: MySQL `innodb_flush_log_at_trx_commit=1`, PG
`synchronous_commit=on`, InlaySQL `Durability::Full` — one commit, one
barrier per statement on every engine. **Correction to this section's
previous wording**: the servers run in containers on named volumes;
InlaySQL's cell runs `sql_shapes` on the host, and its barrier is the
host's `F_FULLFSYNC`. "All in the same container environment" was wrong
for InlaySQL's row and is withdrawn.

| Engine | rows/s (median, range) | commits/s | c/fsync |
| --- | --- | --- | --- |
| InlaySQL (host, `F_FULLFSYNC`) | 24,102 (23,219–24,736) | 241 | 1.00 |
| **InlaySQL (containerised, same volume class as the servers)** | **67,484 (60,453–70,943)** | 675 | 1.00 |
| MySQL 8.4 (containerised) | 56,700 (45,244–68,901) | 567 | 0.71 |
| PostgreSQL 17 (containerised) | 99,212 (93,776–100,749) | 992 | 1.00 |

**LOSS ~2.4x vs MySQL 8.4, LOSS ~4.1x vs PostgreSQL** (was ~1.6x/~3.1x on
2026-08-31: InlaySQL 26,254 (19,111–43,851), MySQL 8.0 42,933, PostgreSQL
81,229 under desktop load). The loss widened because the servers' side
rose on a quiet machine while InlaySQL's cannot: at 241 commits/s the
statement *is* the host barrier — 4.1 ms, the same `F_FULLFSYNC` the host
single-row write in §3.3's table pays at 257.9 ops/s — and 24,102 rows/s
is 98% of that barrier's ceiling. The servers commit against the Docker
volume's cheaper virtualised barrier, measured InlaySQL-against-itself in
`BENCHMARK.md`'s OLTP table at 3.4x cheaper at the medians and 2.4–6.2x
pairing the runs on 2026-09-05. `PERF.md`'s AHL-542
(2026-09-03) profiled this exact shape, found the per-row page round trip
at 32% of the statement, removed it, and measured 1.29–1.44x on the
engine's own batch-insert profile with `sync_commit` rising to 85% of the
statement; `bdc64eb` carries that fix and the published cell barely moved,
because on the host the barrier hides it. **The published ratio is the
barrier, not the engine.** The c/fsync column is the noise-resistant
metric and orders the same way: InlaySQL and PostgreSQL at exactly 1.00,
MySQL at 0.71 (InnoDB's log layer flushing ~1.4x per commit at this batch
size). What this cell owes as of 2026-09-05: a re-run on a build carrying
AHL-553. The neighbouring single-row containerised row has now had one
(§3.3) and AHL-553's own interleaved A/B measured 1.181x on it; this row
came from `sql_shapes`/`batch_driver.py` at `REPS=5`, which the 2026-09-05
`repeat-compare.sh` sitting did not regenerate, so it is still a
pre-AHL-553 measurement. The barrier is 84.4% of the hundred-row statement
against 89.8% of the single-row one (`PERF.md`'s containerised commit
split), so the single-row figure bounds what to expect here rather than
standing in for it, and no number is invented.

### 3.5 Concurrent commits at 4/8/16 writers

**SQLite** (`BENCHMARK.md` "Concurrent writers", `WRITER_LEVELS` sweep):
587/1209/1616 commits/s at 4/8/16 writers vs SQLite's flat 87-92 at every
level — **WIN, roughly 6.7x/13.7x/17.6x** respectively. Far outside the
concurrency-suite floor (3.6% core CoV); the 8-writer point is the tightest
in its own sweep (0.9% spread). Durability: full, both sides, real OS
threads, one `fsync`/`F_FULLFSYNC` per commit or per coalesced batch.

**MySQL, 1/4/16 connections (2026-08-31, `BENCHMARK.md` "Server-to-server,
extended"): LOSS at every level, widening with concurrency, properly
repeated this time.** `SERVER_CONCURRENCY_LEVELS=1,4,16`, 5 repetitions,
**interleaved per concurrency level** (MySQL then InlaySQL at 1 connection,
then MySQL then InlaySQL at 4, then at 16 — never all-of-one-engine-then-
all-of-the-other, the ordering `BENCHMARK.md`'s own corrections section
identifies as this project's worst past measurement error), load-gated
manually before every repetition (1-minute average 2.1-3.3 of an 18-CPU
box's 4.5 ceiling throughout — quiet by this repo's own standard). Median
write throughput and the full 5-repetition range:

| Connections | InlaySQL write ops/s | MySQL write ops/s | Ratio (median) |
| --- | ---: | ---: | ---: |
| 1 | 638.7 (590.1-934.6) | 1,363.1 (674.5-1,510.3) | **LOSS, ~2.1x** (~1.1-2.4x per-rep) |
| 4 | 1,075.0 (1,000.5-1,098.5) | 1,512.7 (1,181.2-3,161.8) | **LOSS, ~1.4x** (~1.1-3.0x per-rep) |
| 16 | 1,308.1 (1,242.4-1,377.3) | 6,120.7 (3,824.2-7,356.9) | **LOSS, ~4.7x** (~3.1-5.4x per-rep) |

**Read as LOSS, not TIE, despite MySQL's own huge run-to-run spread** (CoV
33%/47%/26% at 1/4/16 — far outside even this repo's ~20% busy-desktop
floor, `PERF.md` §4 — most likely Docker/host contention on the MySQL
container specifically, not measured further here): MySQL's write throughput
was ahead of InlaySQL's **in all 5 repetitions at all 3 connection counts**,
the same "consistent sign across repeats" standard `BENCHMARK.md`'s own
interleaved OLTP rerun uses to call a result real rather than noise. The
*exact* multiple should be read as the wide ranges above, not to two
significant figures — MySQL's conc=4 numbers in particular look bimodal
(two repetitions near 3,000-3,160 ops/s, three near 1,180-1,510) rather than
tightly scattered around one value, a pattern worth a future session's
attention rather than smoothed over here. **The loss widens with
concurrency** (median ratio roughly 2.1x → 1.4x → 4.7x — not monotonic step
to step given the conc=4 noise just described, but unambiguously worse at 16
than at 1), the same direction §3.10's p99 finding takes and the opposite of
what the in-process SQLite row above does.

**Reads: TIE at every level**, not the mild win/loss a median alone would
suggest. Median read ops/s: InlaySQL 9,880 vs MySQL 9,078 at 1 connection
(InlaySQL nominally ahead, but only 4 of 5 repetitions agree, not 5 of 5);
MySQL 9,278 vs InlaySQL 8,224 at 4 (MySQL ahead 5/5, but the ratio band is
1.01-1.26x against per-engine CoVs of 7-9% — the same order of magnitude as
the gap); MySQL 5,896 vs InlaySQL 5,209 at 16 (MySQL ahead 4/5, InlaySQL's
own CoV 21%). None of these three clears the bar §1 sets for a verdict other
than TIE — the kind of gap a careless read would call a small win or loss in
either direction, and isn't.

**Commits-per-fsync, MySQL only — see §6 for the full instrument writeup and
the `Handler_commit`/`Com_commit` defect this session found and fixed**:
median 0.98 (1 connection) → 1.99 (4) → 7.42 (16), CoV 0.2-3.3% — the
cleanest, least noisy number this entire sweep produced, and it is the
mechanism explanation for why MySQL's *throughput* lead widens with
concurrency: InnoDB's group commit is visibly amortising `fsync`s as writers
are added. **InlaySQL-server's own ratio could not be measured** — no live
counter, and the `INLAYSQL_COMMIT_STATS=1` exit-time diagnostic never fires
for a long-running server (§6) — a genuine instrument gap, not a claim that
InlaySQL's mechanism is worse. The best available (harness-mismatched)
comparison point is the in-process `WRITER_LEVELS` figure already published
(4.76-6.31x at 8/32 writers, real OS threads, no wire protocol) — the same
order of magnitude as MySQL's 7.42 at 16 connections, weak evidence that
InlaySQL's own batching mechanism is roughly competitive when it runs, which
would point the server's throughput and tail-latency loss at
`inlaysql-server`'s thread-per-connection design rather than at inferior
`fsync` batching — not confirmed, since the direct measurement this would
take is exactly the gap that could not be closed this session.

**Confirmed, same day, once the instrument gap below was closed: the
deficit is predominantly barrier rate, not batching.** `server_driver.py`
now brackets `inlaysql-server`'s own
`Inlaysql_normal_commit_tickets`/`Inlaysql_normal_commit_flushes` the same
way it brackets MySQL's `Handler_commit`/`Innodb_os_log_fsyncs`, run at the
same 1/4/16 connections, 5 repetitions, interleaved per level, load-gated
(1-minute average 2.3-3.3 of the 4.5 ceiling throughout). InlaySQL's own
commits-per-fsync: **median 1.00 (1 connection, CoV 0.0%) → 2.30 (4, CoV
2.8%) → 4.63 (16, CoV 1.1%)** — climbing with concurrency the same way
MySQL's does, and **at 1 and 4 connections it is tied with or ahead of
MySQL's own ratio** (MySQL/InlaySQL batching-ratio, paired per repetition:
median 0.98x at 1 connection — InlaySQL fractionally ahead, gap inside this
metric's own floor; **0.86x at 4 — InlaySQL ahead in all 5 of 5
repetitions**, range 0.85-0.92x). Only at 16 connections does MySQL's
batching pull ahead (median 1.61x, range 1.59-1.62x, 5/5 reps) — real, not
huge. The checkpoint-inclusive pair
(`Inlaysql_commit_tickets`/`Inlaysql_commit_flushes`) tracks within ~5% of
the like-for-like one at every level (1.00 vs 1.00 at 1 connection, exactly;
2.25 vs 2.30 at 4; 4.43 vs 4.63 at 16) — the two do not diverge materially,
so checkpoint traffic is not hiding a different story here.

**The implied `fsync` rate (`write ops/s ÷ commits-per-fsync`, both sides)
is where the gap actually lives.** InlaySQL's own barrier rate *falls* as
concurrency rises — median 660.9/s (1 connection) → 488.8/s (4) → 301.7/s
(16), monotonically, CoV 10.8%/1.8%/6.0% — while MySQL's stays in a flat,
noisy 620-1640/s band across the same three levels (medians 897.0 → 1594.4
→ 843.9/s, CoV 33-35% at 1 and 4, driven by the same throughput noise
§3.5 above already discloses). The MySQL/InlaySQL fsync-rate ratio, paired
per repetition: **median 1.43x (1 connection, range 1.16-2.14x) → 3.21x (4,
range 1.45-3.37x) → 2.78x (16, range 2.10-3.63x)** — MySQL ahead in all 15
of 15 (level, repetition) pairs, the same "sign never flips" standard this
document uses elsewhere to call a noisy-looking gap real. Multiplying the
batching-ratio and fsync-rate-ratio medians reproduces the write-throughput
ratio at each level to within rounding (0.98×1.43≈1.40 vs measured 1.40;
0.86×3.21≈2.76 vs measured 2.77; 1.61×2.78≈4.48 vs measured 4.43) — the
decomposition is internally consistent, not just two numbers that happen to
multiply out.

**Verdict: the deficit is barrier rate, not batching, at every connection
count measured, and it is the larger of the two factors even at 16
connections where batching also starts to matter.** InlaySQL's
commit-batching mechanism is not the weak link — it ties or beats InnoDB's
at low-to-moderate concurrency and only falls behind by ~1.6x at the highest
level tried, while its `fsync` cadence itself falls by more than half from 1
to 16 connections instead of holding flat or rising the way MySQL's does.
This points at *how often the server actually gets to attempt a flush*, not
*how well it batches once it does* — consistent with, and sharper than, the
`inlaysql-server` thread-per-connection hypothesis above: see "Task 2" in
`PERF.md`'s dated section for what was and was not checked to explain it.
This session's own write-throughput figures for this rerun (medians 660.9 →
1138.9 → 1393.7 ops/s InlaySQL, 882.5 → 3171.4 → 6214.3 ops/s MySQL) sit in
the same direction and rough order as the previously published sweep
(638.7/1075.0/1308.1 vs 1363.1/1512.7/6120.7) but are not identical to it —
expected run-to-run noise on an already-disclosed-noisy metric (MySQL's own
throughput CoV was 25-47% in the original sweep, 15-35% in this one), not a
contradiction; the published headline table above is left as is rather than
replaced by a second noisy instance of the same measurement.

**A candidate mechanism for the fsync-*rate* gap specifically was tested and
refuted (2026-08-31), and the instrument gap above is now closed** — see
`PERF.md`'s "The deferred-durability rejection, re-tested in-container" and
`PLAN.md` item 6 (`W3`) for the full write-up. The candidate: InnoDB's
commit `fsync` flushes a small redo-log tail while InlaySQL's flushes ~5
dirty B-tree pages (~20KB, confirmed in-container this session, matching
`PERF.md`'s host figure), and if `fsync` cost scaled with bytes that
difference alone could explain the ~238-vs-~825-fsyncs/s gap implied by this
section's ops/s and commits-per-fsync numbers. Measured directly, in the
same container this comparison runs in, on `inlaysql-server-data`'s own
named volume: the curve is flat (R²=0.017 after correcting an
order-of-measurement confound that first produced a spurious R²=0.91), not
sloped, over the full 0B-1MiB range spanning both engines' plausible
per-commit byte counts. The byte-count mechanism therefore explains
approximately none of the gap, not "a small fraction" — reinforcing rather
than replacing the thread-per-connection hypothesis above as the more likely
cause. Separately, `FileDevice::commit_stats()` now gives `inlaysql-server`
a live, `SHOW GLOBAL STATUS`-readable version of the counters this
paragraph's "could not be measured" referred to
(`Inlaysql_normal_commit_flushes`/`Inlaysql_normal_commit_tickets`); the
instrument exists and is tested, but the actual `SERVER_CONCURRENCY_LEVELS`
sweep using it to measure InlaySQL-server's own ratio directly — which would
upgrade "weak evidence" above to a real confirmation — was not run this
session.

Durability: MySQL `innodb_flush_log_at_trx_commit=1`, binlog off,
`innodb_buffer_pool_size=512M` (matched to PostgreSQL's `shared_buffers`
this session — §4.3 — though PostgreSQL has no row in this table); InlaySQL
server has no separate durability knob — every commit syncs before the
statement returns, the same path the host/containerised rows measure.

**The 1/8-connection table, regenerated gated and repeated (2026-09-05,
`b873f4e`, `REPEATS=3 ./bench/repeat-compare.sh`, MySQL 8.4,
`BENCHMARK.md` "Server-to-server"):** 1 connection, InlaySQL 789.5 ops/s
(759.9–849.6) vs MySQL 890.7 (863.5–1,341.8) — **LOSS, ~0.89x**
(0.59–0.95x per run, MySQL ahead 3/3); 8 connections, InlaySQL 1,456.5
(1,423.6–1,560.7) vs MySQL 4,837.8 (3,096.7–4,862.3) — **LOSS, ~0.30x**
(0.29–0.47x per run, 3/3). Both stay LOSS. **The 1-connection cell is the
cleanest engine result in this document**: it moved 668.9 → 789.5 with the
two gated editions' ranges not overlapping (663.2–694.6 then), which is
**1.18x** — AHL-553's own measured paired ratio of 1.181x on this shape and
volume class, to three digits, on a row whose write p50 fell with it (1.38
→ 1.16 ms). Stated as a coincidence of precision rather than a claim to
three digits: a non-overlapping ~1.2x on a shape where an independent
interleaved A/B found ~1.2x. **At eight connections the same change does
not show** (1,522.2 → 1,456.5, ranges overlapping — flat), and the
mechanism says why: the coordinator already rides ~3.9 commits on each
barrier there, so a cheaper barrier is amortised rather than paid per
commit. Commits-per-fsync at 8 connections, bracketed on both sides:
InlaySQL 3.89 (3.71–3.99) vs MySQL 3.90 (3.87–3.90) — batching at parity —
so the throughput gap still decomposes to barrier rate, ~374 vs ~1,241
fsyncs/s, a 3.3x deficit now reproduced in three gated sittings. Write p99
at 8 connections 21.33 ms vs 5.64 ms (ranges 19.31–25.22 vs 4.28–7.66,
non-overlapping). Scaling from 1 to 8 connections is 1.8x on our side
against MySQL's 5.4x. MySQL's own write column remains among the loudest on
the page (55%/57% spread at 1/8), so the multiples are bands, not digits.

**Reads on this table, and a client change a reader must know about.** At
one connection InlaySQL 9,386.1 (8,436.0–9,498.4) vs MySQL 8,904.1
(5,256.2–8,974.9) — ahead 3 of 3 but at ~1.05x on two of them, so **TIE**
by §1's rule rather than the WIN a median-of-ratios would suggest; at eight,
8,185.2 vs 7,890.1, mixed sign, **TIE**. Both InlaySQL read cells fell about
10% against the previous gated edition by more than either cell's own spread
(10,292.4 → 9,386.1, 9,067.7 → 8,185.2) while MySQL's held — **published as
a band and left unattributed**. Two things on that path changed at once and
neither was measured: the engine range (AHL-551/552 are read-path commits
and both measured flat-to-better, so they do not predict this direction),
and the stack — Track F's compose change has `inlaysql serve --mysql` bind
the compose service name rather than `0.0.0.0`, with `--plaintext-network`
checking it, and has the driver authenticate as the account `bench` created
by `inlaysql user add --superuser` rather than as `root` through
`--user`/`--password`, because a database whose only credential is
`--user`/`--password` is now refused a network bind. An account lookup on
connect and a privilege check per statement is a **candidate** mechanism for
a few per cent on a read row and is named as one, not as a cause: nobody has
run the two logins against each other. MySQL's side of this table is
untouched by that change.

**PostgreSQL: UNKNOWN**, and distinctly so — this is not a missing
measurement, it is a missing capability on **InlaySQL's** side, not
PostgreSQL's, so it is UNKNOWN rather than N/A per the rule in §1.
`inlaysql serve` speaks only the MySQL wire protocol; there is no InlaySQL
server to put a `psycopg` client against, and `PLAN.md`'s "Still closed"
list keeps a PostgreSQL wire protocol out of scope for now (an explicit,
reasoned non-goal, not an oversight). This is the exact gap the task brief
names — "the concurrent-writer sweep has no server-side equivalent" — and
`BENCHMARK.md`'s own "Against MySQL and PostgreSQL" section already says so
in nearly the same words.

### 3.6 Two-table join

**SQLite** (`BENCHMARK.md` "Joins"), four shapes, all vs journal-mode only
(no WAL-mode join row is published — itself a minor completeness gap):
PK-inner full **LOSS ~1.1-1.3x**; PK-inner `LIMIT 10` **LOSS ~2.8-3.5x**;
secondary-index-inner full **WIN ~5-9x**; secondary-index-inner `LIMIT 10`
**LOSS ~2.2-2.6x**. All four ratios sit on the same side of 1.0x across
every combination of the three runs' own spread (10-38%), which is why
`BENCHMARK.md` treats them as real despite the wide individual bands.
Durability: read-only; both engines built under `journal` + `sync=FULL` +
`fullfsync`.

**MySQL / PostgreSQL** (2026-08-31, `bench/external/read_driver.py`, unix
socket, 5 shuffled reps — §4.1's desktop-load disclosure applies): the
harness rebuilds `SUITE=joins`' exact shape — 20,000 users × 8 posts per
user round-robin (160,000 posts), index on `posts.user_id` built after the
rows, ANALYZE, all four AHL-464 shapes at `LIMIT 20`, 100 executions per
rep. Per the pre-fixed join rule, **both FROM orders are reported,
worst-first**, and the p50 medians are compared:

**Regenerated 2026-09-02/03, `REPS=5`, quiet machine, MySQL 8.4.** The
InlaySQL column is the 2026-09-05 edition's gated `run.sh` median of three
at `be95cc3` (a different sitting and two engine editions later than the
server columns — AHL-549 moved the two `LIMIT` cells from `3cf0d85`'s 4.25
/ 6.88 µs to `1f7921a`'s 3.75 / 5.79, and AHL-551 is the named direction
behind this edition's 3.46 / 5.54, at a size the gated row and the
interleaved A/B disagree about; and its `LIMIT` shapes are `LIMIT 10` where
the drivers run `LIMIT 20` — not the same shape, disclosed):

| Shape | InlaySQL p50 | MySQL 8.4 p50 (median, range) | PostgreSQL 17 p50 (median, range) |
| --- | --- | --- | --- |
| PK inner, full join | **3.26 ms** | 13.68 ms (13.64–13.71) | 9.36 ms (9.28–9.47) |
| Secondary-index inner, full join | **3.51 ms** | 13.71 ms (13.68–13.83) | 9.42 ms (9.30–9.49) |
| PK inner, LIMIT (ours 10, theirs 20) | 3.46 µs | 44 µs (42–44) | 29 µs (28–30) |
| Secondary-index inner, LIMIT (ours 10, theirs 20) | 5.54 µs | 51 µs (49–52) | 30 µs (28–30) |

**vs MySQL 8.4 — WIN all four** (~4.2x/~3.9x on the full joins; the `LIMIT`
rows several-x on a smaller LIMIT). **vs PostgreSQL — WIN all four**
(~2.9x/~2.7x full; `LIMIT` likewise). The 2026-08-31 edition's red cell —
PK-inner full join 13.04 ms, TIE vs MySQL (15.00 ms) and **LOSS ~1.24x vs
PostgreSQL** (10.49 ms), "the shape where PG's planner picked the better
order" — is gone for a named reason: AHL-524 (`PERF.md`, 2026-09-02) fixed
AHL-512's inverted join cost model so both written orders run the same
users-driving plan (9.34 → 3.21 ms single-run, 3.23 ms gated at `3cf0d85`,
3.25 ms at `1f7921a`, 3.26 ms at `be95cc3`). Both
opponents still hash-join either FROM order in ~13.7/~9.4 ms; their
columns moved from ~15.0/~10.5 on a quieter machine and, for MySQL, a
version change — unattributed. The planner-epic decision the previous
edition left to the human is answered for this shape by the measurement:
the fix was a cost-model sign, not a planner epic.

Methodology note, disclosed: the full-join shapes are timed as
server-side `SELECT COUNT(*) FROM (<join>) q` wrappers, because a Python
client fetching 160,000 rows per execution measures mysql-connector's
per-row cost (the drivers container sat at 100% CPU with the server idle
before this change), not the engine's join, while the InlaySQL side streams
rows through a near-zero-cost callback. The wrapper still produces and
discards every joined row server-side; the LIMIT/50-row/aggregates shapes
transfer their rows directly. The asymmetry favours InlaySQL — its own
published number *includes* row streaming — so a LOSS recorded here is
conservative.

### 3.7 Aggregate / `GROUP BY`

**SQLite: UNKNOWN** — no aggregate suite exists against SQLite; unchanged
this session.

**MySQL / PostgreSQL** (2026-08-31, `bench/external/read_driver.py` vs
`inlaysql-bench --bin sql_shapes --mode agg`, unix socket, 5 shuffled reps
— §4.1's desktop-load disclosure applies; this shape is defined *here*, no
Rust suite exists for either side): over `indexed`'s 100,000-row table with
a 100-bucket column added, two statements, 100 executions per rep.

**Regenerated 2026-09-02/03, `REPS=5`, quiet machine, MySQL 8.4** (the
InlaySQL side is `sql_shapes --mode agg` on the host, same sitting):

| Shape | InlaySQL | MySQL 8.4 | PostgreSQL 17 |
| --- | --- | --- | --- |
| `GROUP BY n` (100 groups) | **210/s** (207–212) | 110/s (109–110) | 167/s (165–167) |
| scalar (`COUNT/MIN/MAX`) | **1,914/s** (1,624–2,026; remeasured 15:26 at `8cd65c7`) | 300/s (289–301) | 362/s (358–366) |

**`GROUP BY`: WIN ~1.9x vs MySQL 8.4, WIN ~1.26x vs PostgreSQL** — 5/5
reps non-overlapping, and the 1.26x clears the quiet floor (it would not
have cleared 2026-08-31's 20.2% desktop floor, which is why this sitting's
quiet matters). **Scalar: WIN ~6x vs MySQL, ~5x vs PostgreSQL** since AHL-546/548
(`MIN`/`MAX` by descent, `COUNT(*)` from leaf cell counts) — at 03:15 the
same cell read 225/s, LOSS 0.75x/0.62x, 5/5 the other way. On 2026-08-31 this row read 29/s
(26–31) and 53/s (49–57) vs 98/147 and 275/317 — LOSS 3.4–6.0x, "the
single largest multiple against InlaySQL in the matrix outside points".
InlaySQL's cells moved 7.2x and 4.2x; the servers' moved 9–14% (the quiet
machine's share, roughly); the rest is the aggregate work of 2026-09-02/03
— AHL-513/514/515, 519/520, 521, 522, 523, 528, 536, 538, 541 — each step
measured interleaved in `PERF.md`, whose own running tally on the 100k
aggregate profile (85 → 210 ops/s) lands on the same 210 this harness
reads. Both opponents stream 1–100 result rows over a socket; InlaySQL is
in-process; the scalar LOSS is therefore conservative and is the executor
still paying per row where theirs do not. The aggregation half of the
planner-epic question is answered by measurement rather than by a decision:
the grouping pipeline was the cost, and it is no longer the worst cell.

### 3.8 Vector search, exact

**SQLite** (`BENCHMARK.md` "Against `sqlite-vec`"): stock SQLite has no
vector capability at all — **N/A** by the letter of the rule. `sqlite-vec`
is a third-party extension, the de facto standard one, and the published
comparison against it is real and floor-qualified (main-suite run, part of
the same three-run median as the points table): 78.96 µs p50 vs
`sqlite-vec`'s implied p50, **WIN ~8-10x** on the realistic corpus at
iso-recall (1.000 both sides), **WIN ~6.5-7x** on the uniform corpus at
non-matched recall (0.922 InlaySQL vs its own oracle). Presented as both
labels deliberately: N/A describes what stock SQLite can do, WIN describes
the extension comparison that is actually published and floor-qualified.

**MySQL: N/A.** MySQL 8.4 (the version this repo pins) has no vector type,
stock or extension, in this comparison. No container for one exists in
`bench/external/compose.yml`.

**PostgreSQL: TIE against pgvector-HNSW, now that a repeat exists to say
so** — this is the cell where the discipline matters most to apply
consistently. The previous edition held it UNKNOWN because
`BENCHMARK.md`'s DuckDB/pgvector/Meilisearch table was a single run
(135.00 vs 147.00 µs) with no floor of its own. It became a gated median
of three on 2026-09-02/03 (InlaySQL 129.00 µs p50, 88–134 µs, a 36% spread,
vs pgvector-HNSW 148.00 µs, 146–187 µs — ahead 3 of 3 but a 13% median gap
inside our own spread, so **TIE** by §1's rule), and a second gated median
of three on 2026-09-05 (`b873f4e`) reads **InlaySQL 93.00 µs p50 (89–101
µs, a 13% spread) vs pgvector-HNSW 158.00 µs (156–168 µs; recall 1.000 vs
0.988, not recall-matched) — ahead 3 of 3 by 1.54x, 1.70x and 1.89x**, a
median gap of 70% far outside either cell's spread. **WIN, ~1.7x**, and the
reason the verdict moves is the second sitting, not a commit: nothing in
`bdc64eb..b873f4e` touches a retrieval path, both cells landed inside the
ranges the first sitting had already measured, and pooling the two sittings
gives InlaySQL ahead in **6 of 6 gated runs** at 1.1–1.9x with the pooled
ranges (88–134 µs against 146–187 µs) not overlapping. That consistency of
direction across two independent sittings is what clears §1's bar; the
size of the margin is a band (1.1–1.9x), not the 1.7x median, and the
edition-to-edition move in either cell is unattributed. Against
pgvector-exhaustive (506.00 µs, 490–530, recall 0.999 — recall-matched) the
5.4x margin does clear the spread, but that is an exhaustive scan against
an index and `BENCHMARK.md` does not publish it as a like-for-like row. Both pgvector
rows include a socket round trip the library does not pay.

### 3.9 Vector search, int8

**All three: UNKNOWN.** The published int8 numbers (`bench/README.md`'s
"Scalar int8 quantisation") are InlaySQL exact-vs-InlaySQL-int8 only — no
cross-engine comparison exists. `sqlite-vec` supports its own binary/int8
quantisation and pgvector supports `halfvec`/`bit`, so a comparison is
buildable on both fronts in principle, but neither is measured here. MySQL
has no vector capability of any kind (as in 3.8), so a future int8 row
against it would be N/A regardless.

### 3.10 p99 commit latency

**SQLite** (`BENCHMARK.md` "Concurrent writers: the tail the commits/s table
hides"): at 32 writers, InlaySQL p99 121.08 ms vs SQLite's 15.35 ms — **LOSS,
roughly 8x** (7.3-9.4x across the sweep's three runs). p95/p99/max are
explicitly excluded from this project's "core columns" because they swing
far more than `ops/s`/`p50` (InlaySQL's own p95 at 1 writer swings 109% run
to run), so there is no formal CoV floor for this specific number the way
there is for throughput — but `BENCHMARK.md` treats the multiple as real
because every run in the sweep put it on the same side by a wide margin, and
this document follows that reasoning rather than manufacturing a false
UNKNOWN out of "no CoV computed." This is the honestly-disclosed flip side
of the concurrent-commits WIN in 3.5: InlaySQL's optimistic gather-window
design grows the cohort riding one `fsync` as contention rises, and the
writers gathered late in a big cohort sit in its tail; SQLite serialises at
a file lock instead, so its tail stays flat while its throughput does not
scale. Durability: same as 3.5.

**MySQL (2026-08-31): TIE at 1 connection, LOSS at 4 and 16, widening.**
`bench/external/common.py`'s `Timer.percentiles()` used to return only
`(p50, p95, max)` — fixed this session (a fourth return value, `p99`,
threaded through `write_oltp_result`/`write_server_oltp_result` and
`report.py`), closing the instrument gap this row used to cite. Measured
alongside §3.5's throughput sweep, same 5 interleaved repetitions:

| Connections | InlaySQL write p99 (median, range) | MySQL write p99 (median, range) | Verdict |
| --- | ---: | ---: | --- |
| 1 | 3.89ms (2.97-4.87ms) | 2.48ms (1.46-4.18ms) | **TIE** — ranges overlap, sign flips 1 of 5 reps |
| 4 | 15.59ms (14.32-17.22ms) | 5.68ms (3.25-10.50ms) | **LOSS, ~2.7x** (~1.5-4.5x per-rep, 5/5 reps) |
| 16 | 37.00ms (32.18-40.53ms) | 5.69ms (3.76-16.97ms) | **LOSS, ~6.5x** (~2.4-8.9x per-rep, 5/5 reps) |

At 4 and 16 connections the two engines' ranges do not overlap at all
(InlaySQL's *minimum* exceeds MySQL's *maximum*), the strongest form of
evidence this document uses anywhere for a verdict. This is the same story
§3.5 tells for throughput, sharper: InlaySQL's gather-window design grows the
cohort riding one `fsync` as contention rises (visible directly in §6's
commits-per-fsync numbers climbing for *MySQL*, which is the mechanism that
keeps its own tail flat while InlaySQL's grows), and `inlaysql-server`'s own
version of that cohort has no live counter to confirm the mechanism directly
(§6) — only the throughput and latency symptom, not the internal cause, is
measured here. **PostgreSQL: still UNKNOWN** — no InlaySQL server exists to
put a `psycopg` client against (§3.4).

---

## 4. Fairness audit

### 4.0 The 2026-08-31 afternoon sitting — desktop load, disclosed and floor-applied

**Superseded for the current cells, 2026-09-02/03: every cell §3.2, §3.4,
§3.6 and §3.7 carries was regenerated gated and repeated on a quiet
machine (`uptime` 1.47–2.36/18), and the quiet floors apply to them;
the paragraph below describes the 2026-08-31 figures those sections now
quote as history.** Every cell §3.2, §3.4, §3.6 and §3.7 filled that
afternoon was measured with `BENCH_MAX_LOAD_PER_CPU=off` — the host was a desktop in active use, 1-minute
load 4-10 of 18 CPUs for the whole sitting, and the quiet-machine gate
(§`PERF.md` §4) refused every attempt to run it clean. This is disclosed
rather than hidden, and handled the way the floor table already handles it:
**PERF.md §4's desktop-load A/A floor (20.2% CoV) is the floor applied to
every verdict in those cells**, not the quiet-machine one. Three implications,
each applied where it mattered:

- No verdict in those cells may be called a WIN inside 20%. The PK-inner
  full-join TIE vs MySQL (1.15x) is that rule firing.
- Both sides of each cell were measured in the same sitting, so the load is
  a common-mode penalty, not a differential one — except where a side's own
  range shows it hurt more (InlaySQL's batch-insert range is 1.8x wide; its
  best rep still does not beat MySQL's median, and the c/fsync column is
  unaffected either way).
- The clean back-tests these cells would ideally cite (`docs/PLAN.md`'s
  `REPEATS=3` guarded repeats) remain owed; re-deferred with a date there.
  Every raw file this sitting produced is in `bench/results/`
  (`20260831T06*-repeat.txt`, `20260831T06*.txt`).

### 4.1 Durability alignment — no mismatched barrier found

Checked directly against `bench/external/compose.yml`, `bench/compare.sh`,
and every OLTP driver:

| Engine | Setting | Verified where |
| --- | --- | --- |
| MySQL 8.4 (8.0.x before 2026-09-02) | `innodb-flush-log-at-trx-commit=1` (InnoDB's most durable, and already the default — set explicitly so the comparison doesn't silently depend on the default never changing), `skip-log-bin` | `compose.yml:99-100` |
| PostgreSQL 17 | `fsync=on`, `synchronous_commit=on` | `compose.yml:67-68` |
| InlaySQL (host + containerised) | No knob — every commit syncs (`F_FULLFSYNC` on macOS) before `execute_prepared` returns | `bench/README.md` "Durability" table |
| SQLite (reference) | `journal` + `synchronous=FULL` + `fullfsync` | `points` suite |

**No place was found comparing InlaySQL's full barrier against an
opponent's weaker one.** The one PostgreSQL container that *does* run
`fsync=off`/`synchronous_commit=off` (the `pgvector` service, `compose.yml`
lines 18-24) is not used for any durability-sensitive comparison — it
backs the vector-search retrieval rows only, which measure query latency,
not commit durability, and the compose file's own comment states the reason
plainly. This is the right call, correctly executed; flagged here only to
confirm the check was made, not because it found a problem.

### 4.2 Transport — TCP, not unix socket, and the tax is already quantified and disclosed

Confirmed by reading `compose.yml`'s `drivers` service environment and every
driver's DSN: MySQL is reached at `host=mysql, port=3306`, PostgreSQL at
`postgresql://postgres@postgres:5432/...` — both service hostnames resolved
over the Docker compose bridge network, both **TCP**, neither a unix socket.
A production-grade same-host comparison would use a unix socket for both,
which typically costs tens of microseconds less per round trip than a TCP
loopback, and materially less than a bridge-network hop.

**This is already measured, not just asserted.** `BENCHMARK.md`'s
"Correction" section uses InlaySQL as its own control: `inlaysql serve
--mysql` at one connection writes at 1,795.6 µs/commit over the same wire
protocol MySQL pays, while the containerised library row writes at
1,177.0-1,369.3 µs/commit — a **~420-620 µs/commit transport-and-driver tax
that InlaySQL's containerised row skips and both MySQL and PostgreSQL pay on
every statement**, the same order of magnitude as the entire published
PostgreSQL write gap (620 µs). **Direction: flatters InlaySQL, on the write
side, by roughly the whole size of the published gap** — meaning §3.3's LOSS
could plausibly narrow, vanish, or reverse under a transport-matched rerun.
It has been checked once, is disclosed as open rather than closed (§3.3,
§3.4 of `BENCHMARK.md`), and the one data point that exists (MySQL still
~1.35x ahead server-to-server) did not confirm the predicted reversal. On
the read side the same structural asymmetry applies (InlaySQL never pays a
round trip; MySQL/PostgreSQL always do) but no comparable per-operation tax
figure has been isolated for reads the way it has for the write path — the
gap there (12-67x) is large enough that the WIN verdict in §3.1 does not
depend on resolving that number, but a reader should not assume the 420-620
µs figure applies unchanged to a ~1-100 µs read.

### 4.3 Tuning — fixed (2026-08-31), not just recommended

`compose.yml`'s `postgres` service is started with `shared_buffers=512MB`
(line 69) — 4x PostgreSQL's own ~128MB stock default. The `mysql` service
used to get **no equivalent bump**: `--innodb-flush-log-at-trx-commit=1
--skip-log-bin` was the entire command, and `innodb_buffer_pool_size` sat
at MySQL 8's stock default (128MB). **A reviewer would immediately object to
this**: PostgreSQL was tuned, MySQL was not, in the same file, for the same
comparison. This was the first thing this session's task named and fixed
before taking any new measurement: `mysql`'s command now also carries
`--innodb-buffer-pool-size=512M` — the same absolute value as `postgres`'s
`shared_buffers`, and also the same *multiple* of each engine's own stock
default (both ~4x). Durability is untouched
(`innodb_flush_log_at_trx_commit=1` still stands): this is a cache-size
change only. Verified live against the running container:
`SHOW VARIABLES LIKE 'innodb_buffer_pool_size'` reports `536870912` (512MB)
post-fix. See `bench/README.md`'s "Tuning" subsection for the full note.

**Direction and likely magnitude on what was published before this fix:
small, and favoured neither side measurably** — the OLTP workload here is
20,000 rows of a `body TEXT` a few dozen bytes wide, comfortably resident in
either engine's *stock* buffer cache, let alone a bumped one, and every
write-path analysis in `PERF.md`/`BENCHMARK.md` found the commit path
barrier-dominated (87.8-97.1% `fsync`), leaving little room for a
buffer-pool difference to show up in a single-row-commit workload regardless
— consistent with this session's own new concurrent-commits numbers (§3.5)
moving in ways fully explained by connection count and container-level
noise, not by a sudden buffer-pool effect. **The fix matters going forward**
for any indexed-range-scan, join, or aggregate harness against these
servers, where a working set that overflows a stock 128MB but fits 512MB
would have made that comparison about the tuning choice, not the engine —
that asymmetry is now closed before any such harness exists to be confounded
by it. Still unmatched, and named for the same reason, because it cuts the
*other* way (against MySQL, not in its favour, so carries less urgency):
`innodb_flush_method` is left at MySQL's own default rather than `O_DIRECT`,
which costs MySQL a double-buffered write through the OS page cache a tuned
deployment would skip; PostgreSQL's `wal_buffers`,
`checkpoint_completion_target`, `max_wal_size`, and
`effective_io_concurrency` (all left stock).

### 4.4 Structural asymmetry — embedded vs. server, both directions

**In InlaySQL's favour:** every OLTP row against MySQL/PostgreSQL — reads
and, to the extent quantified in §4.2, writes — carries the library-vs-
socket advantage. This is the single largest lever in the whole matrix: it
is most of the 12-67x read WIN (§3.1) and, on the write side, is
of the same order of magnitude as the entire published LOSS (§3.3),
meaning that LOSS is likely an *understatement* of InlaySQL's write-path
disadvantage once transport is accounted for, not an overstatement. The one
place this asymmetry is removed by construction is the server-to-server
table (§3.5) — `inlaysql serve --mysql` against MySQL, same
`mysql.connector` client on both sides — and that is precisely where
InlaySQL's numbers get worse, not better (LOSS at both measured connection
counts, widening rather than narrowing with concurrency).

**Against InlaySQL:** the same design has no equivalent to a server's
worker-pool scheduling or connection multiplexing. `inlaysql-server` is
thread-per-connection, one OS thread and one `Database` handle per
connection, no pool (`docs/server.md`'s D2) — a real, standing
architectural difference from MySQL's bounded worker pool, not a tuning gap
either side can close by reconfiguring. It is the leading suspect, though
not yet the confirmed cause (`BENCHMARK.md`'s "Server-to-server" section
ruled it out twice as the *specific* explanation for one read-throughput
drop, without ruling it out as a *general* ceiling on connection scaling),
for why §3.5's LOSS widens from 1 to 8 connections rather than holding
steady. Every cell in this matrix that depends on the server transport
(§3.5, and any future server-side range-scan/join/aggregate row) inherits
this disadvantage; every cell that stays a library call (§3.1, §3.2, §3.6,
§3.7) does not.

---

## 5. What is missing to fill the scoreboard, ranked by effort

1. **DONE (2026-08-31).** ~~(Cheap, config-only) Extend the existing
   server-to-server sweep to 4 and 16 connections.~~ Run at
   `SERVER_CONCURRENCY_LEVELS=1,4,16`, 5 interleaved, load-gated repetitions
   — §3.5 and `BENCHMARK.md`'s "Server-to-server, extended" have the results.
   Filled both of §3.5's UNKNOWN sub-cells (LOSS at both, widening with
   concurrency); the old 8-connection point was not re-measured and is
   flagged reduced-confidence rather than superseded outright.
2. **DONE (2026-08-31).** ~~(Cheap, harness change) Add p99 to
   `bench/external/common.py`'s `Timer.percentiles()`.~~ Now returns
   `(p50, p95, p99, max)`, threaded through `write_result`,
   `write_oltp_result` and `write_server_oltp_result`, and through the
   equivalent Rust-side `oltp_result_json` in
   `crates/inlaysql-bench/src/oltp_export.rs` (which already computed p99
   and had been discarding it) for schema consistency between the two.
   `report.py` displays it. Closed §3.10's MySQL sub-cell (TIE @1, LOSS
   @4/16); PostgreSQL's stays UNKNOWN — no InlaySQL server exists to measure
   it against (§3.4).
3. **(Cheap, methodology) Give `bench/compare.sh` a repeat wrapper and load
   gate**, the way `bench/run.sh`/`bench/repeat.sh` already have. **Built,
   2026-08-31** — `bench/load_gate.sh` is now a module both `run.sh` and
   `compare.sh` source (one gate, not two copies that drift), and
   `REPEATS=5 ./bench/repeat-compare.sh` repeats `compare.sh` and reports the
   median and spread through the same `bench/summarise.py`. `compare.sh`
   samples only its measured phases, not its own container builds, so
   `CONTAMINATED` on a compare result means something else on the machine
   disturbed the run.

   **Run for publication 2026-09-02/03** (`REPEATS=3`, gated, none
   contaminated): every `compare.sh`-sourced cell on this page — §3.1,
   §3.3, §3.5's 1/8 row, §3.8 — is now a median of three with its per-run
   figures in `BENCHMARK.md`, and §3.8's vector cell moved from UNKNOWN to
   TIE on that basis. One gap the wrapper does *not* close, stated so
   nobody reads more into it: it repeats whole passes rather than
   interleaving the engines within one pass (`compare.sh`'s phase order is
   fixed).
4. **(Medium, new harness) Indexed range scan and two-table join against
   MySQL/PostgreSQL.** Needs a second table in the OLTP schema (or a
   parallel schema) with a secondary index and a joinable foreign key,
   matched row counts and query shapes to the existing SQLite `indexed`/
   `joins` suites, and — per §4.3 — matched buffer-pool/`shared_buffers`
   tuning before the numbers mean anything. This closes four UNKNOWN cells
   (§3.2, §3.6 x2 columns) at once.
5. **(Medium, new harness) `GROUP BY`/aggregate suite, all three engines.**
   ~~Does not exist against anyone today~~ — **MySQL and PostgreSQL cells
   filled 2026-08-31, regenerated gated 2026-09-02/03** (§3.7: `GROUP BY`
   now WIN 1.9x/1.26x, scalar WIN ~6x/~5x since 15:26; the shape is defined in
   `bench/external/read_driver.py` and measured InlaySQL-side through
   `inlaysql-bench --bin sql_shapes --mode agg`). **The SQLite cell is the
   remaining half of this item**: extend `read_driver.py` or add an
   aggregate suite to `inlaysql-bench` so the row reads "vs SQLite" too.
   The workload design decision (row count, cardinality, aggregate shape)
   this item asked for was made on 2026-08-31 — 100k rows, a 100-bucket
   `n` column, `GROUP BY n` plus a whole-table scalar aggregate — and is
   now pinned by the published cells above; keep it if the SQLite leg is
   added.
6. ~~**(Medium, new harness) Batch insert against MySQL/PostgreSQL.**~~
   **Done 2026-08-31** (§3.4): `bench/external/batch_driver.py` vs
   `inlaysql-bench --bin sql_shapes --mode batch`, 100 rows per statement,
   c/fsync bracketed per rep — LOSS ~1.6x / ~3.1x then, ~2.4x / ~4.1x on
   the gated 2026-09-02/03 rerun, and §3.4 says why the host barrier, not
   the engine, sets that number. The deliberate design
   choice this item asked for was made and is pinned by the published cell:
   100 rows/statement because a single InlaySQL commit's WAL record must fit
   its region format at any batch size, and 100 keeps every engine's commit
   count (and therefore the c/fsync metric) comparable at 1.00.
   The SQLite leg was not in scope and remains UNKNOWN.
7. **(Larger, new capability) Repeated, quiet-machine, transport-matched
   rerun of the single-row-insert comparison** (§3.3's "what is owed" item,
   already scoped in `PERF.md`/`BENCHMARK.md` but not executed at more than
   one run) — needed to turn the current provisional LOSS into a number
   trustworthy to a specific multiple, and to actually test the §4.2
   prediction that transport-matching narrows or reverses it. **Partly
   paid, twice**: the server-to-server 1-connection row (§3.5) is now a
   gated median of three on the matched transport in two sittings —
   InlaySQL 668.9 vs MySQL 8.4 1,041.8 (~0.64x) on 2026-09-02/03, and
   789.5 vs 890.7 (~0.89x, MySQL ahead 3/3) on 2026-09-05 after AHL-553.
   So the loss has not reversed on the matched transport even though the
   library row (§3.3) is now a TIE, which is the more informative of the
   two answers and the reason §3.3 declines to read a multiple off its own
   medians. Still not interleaved within a pass, and three runs rather than
   five.
8. **(Largest, explicit non-goal today) A PostgreSQL-wire InlaySQL
   server.** The only way §3.5's PostgreSQL cell moves from UNKNOWN to a
   real verdict. `PLAN.md`'s "Still closed" list keeps this out of scope
   deliberately (+30% reach for a second type system, MySQL-side gaps judged
   higher leverage) — listed last because it is large, not because it is
   unimportant; revisiting it is a project-level decision, not a benchmark
   task.

---

## 6. The commits-per-fsync instrument

**Wired in and run (2026-08-31), superseding the "not implemented this
session" note this section used to carry.** The concurrent-commit cells
(§3.5) used to report only throughput — commits/s — which lets the easy
opponent look like the whole story. SQLite serialises writers at a file lock
and has no group-commit mechanism at all; beating it 13-17x on concurrent
throughput is real, but it is also the least surprising axis to win on,
because SQLite isn't even trying to batch `fsync`s. InnoDB and PostgreSQL
both have mature group commit and are the opponents this axis should
actually be judged against, and the mechanism metric that makes that
judgment meaningful — not just believable, the same metric that is what made
this project's own 13x figure credible in the first place — is **commits
landed per `fsync` call**.

**InlaySQL prints this, but only for a process that exits normally.**
`crates/inlaysql/src/device.rs:565-573`, gated on `INLAYSQL_COMMIT_STATS=1`:
on drop, `CommitCoordinator` writes `commit-stats: flushes=N tickets=N
normal_flushes=N normal_tickets=N` to stderr — `normal_tickets /
normal_flushes` is commits-per-`fsync`, excluding checkpoint-triggered
flushes, and it is the ratio `PERF.md`'s "fixed 8-yield gather window"
section already used to show the in-process batching ratio was pinned near
2.0 before the adaptive-window fix and rose to 4.76-6.31 after it, at 8 and
32 writers respectively (real OS threads, one process, `bench/run.sh`'s
`WRITER_LEVELS` sweep). **That mechanism does not reach `inlaysql-server`**:
a long-running server is never dropped by a normal return from `main`, and
no signal handler in `crates/inlaysql-server` drops the `Database`
gracefully on `SIGTERM` — confirmed by reading the crate, not assumed — so
the one InlaySQL row this section's own task cares about most (the server,
§3.5's actual subject) has no live counter to sample and no exit-time
diagnostic that ever fires. This is a genuine, disclosed instrument gap, not
a claim that the server's own batching does not exist; see §3.5 below for
what stands in for it.

**MySQL and PostgreSQL both expose the equivalent, and neither needed a
config change — but one of the two counters this section originally named
was wrong, caught only by running it against a live container:**

- **MySQL: `Handler_commit`, not `Com_commit`.** This section previously
  named `Δ Com_commit / Δ Innodb_os_log_fsyncs`. Measured directly against
  `bench/external/compose.yml`'s `mysql` service: `Com_commit` counts literal
  `COMMIT` statement text and does not move at all under this benchmark's
  autocommit-per-statement writes (verified with a plain `INSERT` against a
  real table — `Com_commit` stayed at `0`, `Handler_commit` moved by exactly
  one). Using `Com_commit` as originally specified would have silently
  reported `commits: 0, fsyncs: N, commits_per_fsync: 0.0` at every
  concurrency level — a wrong number that looks like a real one, not a
  missing one, which is worse than the instrument gap above. `Handler_commit`
  is the storage-engine-level counter that increments on every commit,
  explicit or autocommit-implicit, and is what `mysql_driver.py` and
  `server_driver.py` now actually query. `Innodb_os_log_fsyncs` (the count of
  `fsync()` calls InnoDB has issued against the redo log) was correct as
  specified.
- **PostgreSQL: the counters were right, the timing was not.**
  `Δ pg_stat_database.xact_commit / Δ pg_stat_wal.wal_sync` is the correct
  pair, but PostgreSQL's cumulative statistics system lets a backend batch
  its own pending counter updates and flush them opportunistically rather
  than synchronously at every commit. Measured directly: a fast write phase
  (a few hundred rows) followed immediately by a read of both counters on the
  same connection, with no forced flush, read back `commits: 0, fsyncs: 0`
  even though the rows had genuinely committed — confirmed by re-querying a
  couple of seconds later and seeing the real values appear.
  `postgres_oltp_driver.py`'s `xact_commit`/`wal_sync` helpers now call
  `pg_stat_force_next_flush()` (PG15+, present in the pinned `postgres:17`
  image) immediately before each read, which fixed it.

Both mistakes have the same shape: a plausible-sounding counter name or a
plausible-sounding "no config change needed" claim that produced a
confidently wrong `0.0` instead of an honestly missing measurement, caught
only by running the query against a live container rather than trusting the
API name. That is exactly the failure mode this repo's own instrumentation
rules (`crates/inlaysql-server/src/metrics.rs`'s "every number is
maintained, or it is not reported") exist to catch, applied here to a
borrowed counter instead of one this project owns.

**What running it found, at MySQL server-to-server (§3.5), 5 interleaved
repetitions:**

| Connections | MySQL commits-per-fsync (median, range) |
| --- | --- |
| 1 | 0.98 (0.98-0.99) |
| 4 | 1.99 (1.96-1.99) |
| 16 | 7.42 (6.98-7.59) |

Near-1.0 at one connection (nothing to batch with, as expected), then
climbing close to linearly with connection count — InnoDB's group commit is
visibly amortising `fsync`s as writers are added, exactly the behaviour this
instrument exists to detect. **It is also, itself, the single most stable
number this session measured**: CoV 0.2%/0.7%/3.3% at 1/4/16 connections,
tighter than the *quiet-machine* concurrency floor `PERF.md` §4 established
(3.6%) even though the wall-clock write-throughput figures gathered
alongside it in the same repetitions swung 25-47% CoV on this same,
nominally-quiet machine (see §3.5) — direct evidence for the task's own
premise that this metric is more robust to machine noise than throughput,
not just an assertion of it.

InlaySQL-server's own ratio could not be measured (see the instrument-gap
paragraph above). The closest available evidence is the in-process
`WRITER_LEVELS` figure already published (4.76-6.31x at 8/32 writers) —
a different harness (library, real OS threads, no wire protocol, no
`inlaysql-server` connection-handling in the loop) and not a substitute
measurement, but the two numbers sit in the same order of magnitude as
MySQL's 7.42 at 16 connections. Read that as weak, harness-mismatched
evidence that InlaySQL's own commit-batching *mechanism* is roughly
competitive with InnoDB's when it gets the chance to run, which — combined
with §3.5's finding that the server's throughput and tail latency are what
actually lose, badly — points at `inlaysql-server`'s thread-per-connection
design (no pool, `docs/server.md`'s D2) as the more likely place this
project's server-side concurrent-write loss lives, not the underlying
commit coordinator's batching logic. Not confirmed; the instrument gap above
is exactly what would need closing to confirm it directly.

**The instrument gap is closed, 2026-08-31 — and so is the confirming
measurement, same day.** `inlaysql::FileDevice::commit_stats()`
(`crates/inlaysql/src/device.rs`) is a live snapshot of the same
`normal_flushes`/`normal_tickets_flushed` counters `INLAYSQL_COMMIT_STATS`
prints on `Drop`, now readable from a running server too:
`Server::run`'s existing keeper handle (already held open for the file lock,
`crates/inlaysql-server/src/lib.rs`) is shared into every connection thread
and read on `SHOW GLOBAL STATUS` as `Inlaysql_normal_commit_flushes`/
`Inlaysql_normal_commit_tickets` (plus the checkpoint-inclusive
`Inlaysql_commit_flushes`/`Inlaysql_commit_tickets`). Tested end-to-end
(`show_global_status_reports_the_commit_batching_counters`,
`crates/inlaysql-server/tests/wire.rs`) — a second connection's commits move
the count, `SESSION`/`GLOBAL` agree since the counter is process-wide.
`server_driver.py` now brackets these counters alongside MySQL's
`Handler_commit`/`Innodb_os_log_fsyncs`, and the `SERVER_CONCURRENCY_LEVELS`
sweep this paragraph used to call owed has been run: **"weak,
harness-mismatched evidence" is now a real confirmation, and it says the
opposite of what a reader might guess from the throughput loss alone —
InlaySQL's own commit-batching mechanism ties or beats InnoDB's at 1 and 4
connections and trails by only ~1.6x at 16, while its `fsync` rate itself
falls from ~661/s to ~302/s as connections go from 1 to 16 (MySQL's stays in
a flat 620-1640/s band over the same range).** The thread-per-connection
design named above is the more likely place the throughput loss lives, and
this session's numbers sharpen why: not because the coordinator batches
`fsync`s worse than InnoDB does, but because something upstream of the
coordinator throttles how often a flush gets attempted at all as writers are
added, the opposite of the direction batching alone would predict. Full
numbers and the per-repetition ratio ranges are in §3.5 above; a bounded
search for *why* the barrier rate itself falls is in `PERF.md`'s dated
"Task 2" section — diagnosis only, no fix implemented or proposed as
production-ready. See `PLAN.md` item 6/9 for the same note in planning
form.
