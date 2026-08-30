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

**What this is not.** A benchmark run. Every number quoted below already
exists in `BENCHMARK.md`, itself the only place a headline figure is allowed
to originate (`AGENTS.md`). No new measurement was taken to produce this
document — see the note at the top of the task this file answers: the host
this was written on is a busy interactive desktop with the A/A floor
`PERF.md` §4 measured, and a long run on it would be neither trustworthy nor
likely to finish. Where the honest answer is "nobody has run this
comparison," the cell says **UNKNOWN**, not a plausible-looking number.

Provenance: every figure below traces to `BENCHMARK.md` as committed at
`b825f2d` (the commit this document was written against, confirmed clean
before writing). `BENCHMARK.md`'s own per-section provenance notes (which
tables regenerated this edition, which are carried forward, and from which
commit) apply transitively here and are not repeated in full — follow the
section reference given for each row.

---

## 1. Verdict rules

Fixed before any cell below was filled in, per the task this document
answers. Six possible verdicts, not four — the two the brief specified plus
two this project cannot honestly do without:

- **WIN** — InlaySQL is ahead by a margin that exceeds the measured A/A floor
  for the suite the figure comes from (`PERF.md` §4: 4.0% main suite / 3.6%
  concurrency / 7.3% point-reads-quiet / ~20% under desktop load), **and**
  the source table itself treats the gap as real rather than noise (most
  `run.sh`-sourced tables state a run-to-run range explicitly; where a
  figure comes from `compare.sh`, which has no repeat wrapper and therefore
  no measured floor of its own, WIN is only used when `BENCHMARK.md`'s own
  text says the gap is too large for that table's noise to plausibly
  produce — e.g. the 35-74x read numbers below — never on a raw multiple
  alone).
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
  Reserved for the *opponent* lacking the feature (e.g. MySQL 8 has no
  vector search of any kind). Where **InlaySQL** is the side missing a
  capability needed to run the comparison (e.g. no PostgreSQL wire
  protocol), the cell is UNKNOWN, not N/A — that is an engineering gap this
  project could close, not a fact about the opponent, and conflating the two
  would hide real missing work behind a label that says "nothing to do
  here."
- **UNKNOWN** — no comparison exists yet, the comparison that exists is not
  floor-qualified (single, unrepeated `compare.sh` run with no established
  spread, and the gap is not large enough to invoke the WIN/LOSS exception
  above), or the workload has no matched-durability harness at all. The
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

| Workload | SQLite | MySQL 8 | PostgreSQL 17 |
| --- | --- | --- | --- |
| Point read by PK | WIN ~2-4x vs durable config; LOSS ~2x vs WAL/NORMAL | WIN ~74x (structural, §3.1) | WIN ~35x (structural, §3.1) |
| Indexed range scan | LOSS ~2x (durable) / ~2.9x (WAL) | UNKNOWN — no harness | UNKNOWN — no harness |
| Single-row insert (durable) | WIN ~2.7x | LOSS ~1.4-1.8x (containerised; not floor-bound, §3.3) | LOSS ~1.4-1.8x (containerised; not floor-bound, §3.3) |
| Batch insert | UNKNOWN — no SQLite-batched comparison published | UNKNOWN — no harness | UNKNOWN — no harness |
| Concurrent commits, 4/8/16 writers | WIN ~10-17x across 4/8/16 | LOSS @1,8 (~0.71x/~0.41x, single run); UNKNOWN @4,16 | UNKNOWN — no server exists (§3.4) |
| Two-table join | MIXED: WIN one shape (~5-9x), LOSS three shapes (~1.1-3.5x) | UNKNOWN — no harness | UNKNOWN — no harness |
| Aggregate / `GROUP BY` | UNKNOWN — no harness | UNKNOWN — no harness | UNKNOWN — no harness |
| Vector search, exact | N/A (stock) / WIN ~8-10x vs `sqlite-vec` ext., iso-recall | N/A — no vector capability | UNKNOWN — not floor-qualified (single run, §3.6) |
| Vector search, int8 | UNKNOWN — no cross-engine harness | N/A — no vector capability | UNKNOWN — no cross-engine harness |
| p99 commit latency | LOSS ~7-9x at high writer counts | UNKNOWN — harness doesn't compute p99 (§6) | UNKNOWN — harness doesn't compute p99 (§6) |

Fourteen UNKNOWN or N/A-for-missing-harness cells out of thirty. That ratio
is the honest state of "beat SQLite, MySQL and PostgreSQL across the board"
today: the SQLite column is mostly filled in (and mixed, not a sweep), and
the MySQL/PostgreSQL columns are mostly empty outside points and one
concurrency slice.

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
containerised row): InlaySQL 678k ops/s vs MySQL 9.2k (**WIN, ~74x**) and
PostgreSQL 19.4k (**WIN, ~35x**). `BENCHMARK.md` itself says a gap of this
size "is not the kind of thing this document's measurement floor could
plausibly manufacture," which is why it clears the bar for WIN despite
coming from `compare.sh`'s single, unrepeated run. **Read this WIN in
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

**MySQL / PostgreSQL**: **UNKNOWN.** The OLTP driver's schema is a bare `kv`
table (`id`, `body`), no secondary index, no range predicate —
`mysql_driver.py` and `postgres_oltp_driver.py` only exercise point read/point
write by primary key. There is no harness that could produce this cell today.

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

**MySQL / PostgreSQL** (`BENCHMARK.md` "Interleaved, repeated, quiet-machine
rerun"): InlaySQL containerised 698.9 ops/s (median of 5) vs MySQL 1,002.3
(**LOSS, ~1.43x**) and PostgreSQL 1,265.7 (**LOSS, ~1.81x**), PostgreSQL
ahead of MySQL 5/5 repetitions. **This looks like the textbook FLOOR-BOUND
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
marked **LOSS**, not FLOOR-BOUND, on the evidence — with two loud caveats:
(1) the multiple should be read as "roughly 1.4-1.8x," never to two
significant figures, given each engine's own 50-81% single-rep spread, and
(2) this comparison structurally favours InlaySQL (§4.2: it skips a
transport tax MySQL and PostgreSQL both pay, of the same order of magnitude
as the entire gap), so a transport-matched rerun could plausibly narrow,
hold, or reverse this LOSS — the one single-run transport-matched data point
that exists (§3.4) found MySQL still ~1.35x ahead even with that advantage
removed, which does not overturn the LOSS but does not confirm its exact
size either. The host row (253.2 ops/s, real `F_FULLFSYNC`) is **not
comparable** to the containerised MySQL/PostgreSQL rows at all — different
hardware barrier classes entirely — and is excluded from this cell rather
than mixed in. Durability: InlaySQL containerised — same commit path as the
host, on a named Docker volume; MySQL `innodb_flush_log_at_trx_commit=1`;
PostgreSQL `fsync=on`/`synchronous_commit=on`; all three on a Docker named
volume of the same `local` driver class.

### 3.4 Batch insert

**All three: UNKNOWN.** `BENCHMARK.md`'s "Durable writes" section quotes
"~240x" for InlaySQL's batched-vs-unbatched write rate (58,320 ops/s batched
against 240 ops/s single-row) — **that multiple is InlaySQL against itself,
not against SQLite.** No SQLite-batched-transaction number is published
anywhere in the current tables, and no batched/multi-row-insert comparison
exists against MySQL or PostgreSQL either (`bench/README.md`'s "Scope" note
explicitly excludes the `points` suite's batched-write row from the
MySQL/PostgreSQL comparison — "there is no natural equivalent on
MySQL/PostgreSQL without picking an arbitrary batch size for them too").
**Flagged as a documentation risk, not just a missing measurement**: the
"~240x" figure sits in a paragraph immediately after several real "vs
SQLite" ratios, with no explicit "against our own single-row rate" qualifier
in `README.md`'s prose — a reader skimming could misattribute it as a
competitive multiple. It isn't one. Recommend the prose in `README.md` and
`BENCHMARK.md` state the comparison basis explicitly wherever this figure is
quoted.

### 3.5 Concurrent commits at 4/8/16 writers

**SQLite** (`BENCHMARK.md` "Concurrent writers", `WRITER_LEVELS` sweep):
587/1209/1616 commits/s at 4/8/16 writers vs SQLite's flat 87-92 at every
level — **WIN, roughly 6.7x/13.7x/17.6x** respectively. Far outside the
concurrency-suite floor (3.6% core CoV); the 8-writer point is the tightest
in its own sweep (0.9% spread). Durability: full, both sides, real OS
threads, one `fsync`/`F_FULLFSYNC` per commit or per coalesced batch.

**MySQL** (`BENCHMARK.md` "Server-to-server"): at 1 connection InlaySQL
556.7 ops/s vs MySQL 787.7 (**LOSS, ~0.71x**); at 8, InlaySQL 1,255.5 vs
MySQL 3,092.7 (**LOSS, ~0.41x**, and MySQL's own throughput nearly
quadruples across the step where InlaySQL's only reaches 2.25x — the gap
widens with concurrency, the opposite of what the in-process SQLite row
shows). **4 and 16 connections: UNKNOWN** — `server_driver.py` defaults to
`SERVER_CONCURRENCY_LEVELS=1,8` and supports an override
(`SERVER_CONCURRENCY_LEVELS=1,4,16 ./bench/compare.sh`, per `compare.sh`'s
own usage comment) that has not been run and published. This is also a
single run with no repeated median — treat the 1x/8x figures above as
directional, not final, per `BENCHMARK.md`'s own caveat on this table.
Durability: MySQL `innodb_flush_log_at_trx_commit=1`, binlog off; InlaySQL
server has no separate durability knob — every commit syncs before the
statement returns, the same path the host/containerised rows measure.

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

**MySQL / PostgreSQL: UNKNOWN.** No join benchmark exists against either
server — the OLTP drivers' schema is a single `kv` table with no second
table to join against.

### 3.7 Aggregate / `GROUP BY`

**All three: UNKNOWN.** No `GROUP BY`/aggregate suite exists anywhere in
this repo's benchmark harness, against SQLite or the servers. This is a
complete gap, not a partial one — see §5.

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

**MySQL: N/A.** MySQL 8 (the version this repo pins) has no vector type,
stock or extension, in this comparison. No container for one exists in
`bench/external/compose.yml`.

**PostgreSQL: UNKNOWN, not WIN, despite favourable raw numbers** — this is
the cell where the discipline matters most to apply consistently.
`BENCHMARK.md`'s DuckDB/pgvector/Meilisearch table (`compare.sh`, single
run, host load 1.1-1.9/18, **no repeat wrapper exists for this table at
all**) shows InlaySQL at 135.00 µs p50 vs pgvector-HNSW's 147.00 µs
(recall 1.000 vs 0.987 — not recall-matched) and pgvector-exhaustive's
482.00 µs (recall 1.000 vs 0.999 — recall-matched, and a bigger gap). Read
naively, 135 vs 482 looks like a clear win. It is not floor-qualified: this
table has no established CoV the way the `run.sh` tables do, `BENCHMARK.md`
says explicitly that a single unrepeated run here is "*less* trustworthy...
not more" than the gated tables, and unlike the read-side 35-74x gap this
margin (8% against HNSW, ~3.6x against exhaustive) is well within the range
this project's own noise has been shown to produce elsewhere. Per the rule
in §1 (a `compare.sh`-sourced gap only earns WIN when `BENCHMARK.md` itself
says the gap is too large for that table's own noise to explain), this stays
**UNKNOWN** until a repeated, quiet-machine rerun of `compare.sh`'s vector
row exists — the same standard `run.sh`'s tables already meet and
`compare.sh`'s do not.

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

**MySQL / PostgreSQL: UNKNOWN**, and structurally so rather than just
unmeasured — `bench/external/common.py`'s `Timer.percentiles()` returns
only `(p50, p95, max)`; the server-to-server driver never computes a p99 at
all. This is the exact instrument gap §6 below names as worth fixing first.

---

## 4. Fairness audit

### 4.1 Durability alignment — no mismatched barrier found

Checked directly against `bench/external/compose.yml`, `bench/compare.sh`,
and every OLTP driver:

| Engine | Setting | Verified where |
| --- | --- | --- |
| MySQL 8 | `innodb-flush-log-at-trx-commit=1` (InnoDB's most durable, and already the default — set explicitly so the comparison doesn't silently depend on the default never changing), `skip-log-bin` | `compose.yml:99-100` |
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
gap there (35-74x) is large enough that the WIN verdict in §3.1 does not
depend on resolving that number, but a reader should not assume the 420-620
µs figure applies unchanged to a ~1-100 µs read.

### 4.3 Tuning — an asymmetry exists, likely inert for what's published, load-bearing for what isn't

`compose.yml`'s `postgres` service is started with `shared_buffers=512MB`
(line 69) — 4x PostgreSQL's own ~128MB stock default. The `mysql` service
gets **no equivalent bump**: `--innodb-flush-log-at-trx-commit=1
--skip-log-bin` is the entire command, and `innodb_buffer_pool_size` is left
at MySQL 8's stock default (128MB). **A reviewer would immediately object to
this**: PostgreSQL was tuned, MySQL was not, in the same file, for the same
comparison.

**Direction and likely magnitude on what's currently published: small, and
favours neither side measurably.** The OLTP workload here is 20,000 rows of
a `body TEXT` a few dozen bytes wide — comfortably resident in either
engine's *stock* buffer cache, let alone a bumped one — and every write-path
analysis in `PERF.md`/`BENCHMARK.md` found the commit path barrier-dominated
(87.8-97.1% `fsync`, depending on host vs. container), leaving little room
for a buffer-pool difference to show up in a single-row-commit workload
regardless. **The same asymmetry would matter a great deal more for the
currently-UNKNOWN cells** (indexed range scan, join, aggregate) if those
harnesses are ever built against MySQL/PostgreSQL — a working set that
doesn't fit a stock 128MB `innodb_buffer_pool_size` but does fit a tuned one
would make those comparisons about the tuning choice, not the engine, unless
both sides get matched. **Recommendation: either match `shared_buffers` and
`innodb_buffer_pool_size` (both to the same multiple of stock, or both left
at stock) before any range-scan/join/aggregate harness against these
servers ships, or size the corpus so it exceeds every configuration's buffer
cache and the mismatch becomes moot.** Also unmatched and worth naming for
the same future harness, though likely inert today for the same
barrier-dominance reason: `innodb_flush_method` (left at MySQL's own
default, not `O_DIRECT`, so MySQL here pays a double-buffered write through
the OS page cache a tuned deployment would skip — a reviewer's second likely
objection, and one that, if anything, *disfavours* MySQL slightly, i.e. cuts
in InlaySQL's favour, the opposite direction from the buffer-pool point
above); PostgreSQL's `wal_buffers`, `checkpoint_completion_target`,
`max_wal_size`, and `effective_io_concurrency` (all left stock).

### 4.4 Structural asymmetry — embedded vs. server, both directions

**In InlaySQL's favour:** every OLTP row against MySQL/PostgreSQL — reads
and, to the extent quantified in §4.2, writes — carries the library-vs-
socket advantage. This is the single largest lever in the whole matrix: it
is most of the 35-74x read WIN (§3.1) and, on the write side, is
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

1. **(Cheap, config-only) Extend the existing server-to-server sweep to 4
   and 16 connections.** `server_driver.py` already supports
   `SERVER_CONCURRENCY_LEVELS=1,4,16 ./bench/compare.sh` — it has simply
   never been run and published at those levels. Fills two of §3.5's three
   UNKNOWN sub-cells with no new code.
2. **(Cheap, harness change) Add p99 to `bench/external/common.py`'s
   `Timer.percentiles()`.** It returns `(p50, p95, max)` today; adding a
   fourth return value and threading it through `write_oltp_result` and
   `write_server_oltp_result` closes §3.10's MySQL/PostgreSQL UNKNOWN cells
   the next time `compare.sh` runs. This is the smallest fix with the
   highest matrix payoff — one field, two call sites, a low-risk `report.py`
   change to display it.
3. **(Cheap, methodology) Give `bench/compare.sh` a repeat wrapper and load
   gate**, the way `bench/run.sh`/`bench/repeat.sh` already have. This is
   recorded as a recommendation in both `PERF.md` and `bench/README.md`
   already, pending someone watching a CI run confirm it doesn't interact
   badly with `trust.yml`'s shared-runner tolerances. Without it, §3.8's
   vector-search UNKNOWN cell (and any future `compare.sh`-sourced cell)
   cannot become a WIN/LOSS no matter how the raw numbers look — there is no
   floor to check them against.
4. **(Medium, new harness) Indexed range scan and two-table join against
   MySQL/PostgreSQL.** Needs a second table in the OLTP schema (or a
   parallel schema) with a secondary index and a joinable foreign key,
   matched row counts and query shapes to the existing SQLite `indexed`/
   `joins` suites, and — per §4.3 — matched buffer-pool/`shared_buffers`
   tuning before the numbers mean anything. This closes four UNKNOWN cells
   (§3.2, §3.6 x2 columns) at once.
5. **(Medium, new harness) `GROUP BY`/aggregate suite, all three engines.**
   Does not exist against anyone today, InlaySQL-vs-SQLite included. Needs
   a workload design first (row count, cardinality, aggregate shape) before
   any engine gets measured — this is the one row in the matrix with zero
   existing infrastructure to extend.
6. **(Medium, new harness) Batch insert against MySQL/PostgreSQL.**
   `bench/README.md` already scoped why this needs a deliberate choice
   (an arbitrary batch size for the servers) rather than reusing the
   `points` suite's InlaySQL-only batched row — the design decision, not the
   implementation, is the work here.
7. **(Larger, new capability) Repeated, quiet-machine, transport-matched
   rerun of the single-row-insert comparison** (§3.3's "what is owed" item,
   already scoped in `PERF.md`/`BENCHMARK.md` but not executed at more than
   one run) — needed to turn the current provisional LOSS into a number
   trustworthy to a specific multiple, and to actually test the §4.2
   prediction that transport-matching narrows or reverses it.
8. **(Largest, explicit non-goal today) A PostgreSQL-wire InlaySQL
   server.** The only way §3.5's PostgreSQL cell moves from UNKNOWN to a
   real verdict. `PLAN.md`'s "Still closed" list keeps this out of scope
   deliberately (+30% reach for a second type system, MySQL-side gaps judged
   higher leverage) — listed last because it is large, not because it is
   unimportant; revisiting it is a project-level decision, not a benchmark
   task.

---

## 6. The commits-per-fsync instrument

The concurrent-commit cells (§3.5) currently report only throughput —
commits/s — which lets the easy opponent look like the whole story. SQLite
serialises writers at a file lock and has no group-commit mechanism at all;
beating it 13-17x on concurrent throughput is real, but it is also the
least surprising axis to win on, because SQLite isn't even trying to batch
`fsync`s. InnoDB and PostgreSQL both have mature group commit and are the
opponents this axis should actually be judged against, and the mechanism
metric that makes that judgment meaningful — not just believable, the same
metric that is what made this project's own 13x figure credible in the
first place — is **commits landed per `fsync` call**.

**InlaySQL already prints this.** `crates/inlaysql/src/device.rs:565-573`,
gated on `INLAYSQL_COMMIT_STATS=1`: on drop, `CommitCoordinator` writes
`commit-stats: flushes=N tickets=N normal_flushes=N normal_tickets=N` to
stderr. `normal_tickets / normal_flushes` is exactly commits-per-`fsync`,
excluding checkpoint-triggered flushes — the ratio `PERF.md`'s "fixed
8-yield gather window" section used to show the batching ratio was pinned
near 2.0 before the adaptive-window fix and rose to 4.76-6.31 after it, at 8
and 32 writers respectively.

**MySQL and PostgreSQL both expose the equivalent, by the same
before/after-delta method, and neither needs a config change to get it:**

- **MySQL:** `SHOW GLOBAL STATUS LIKE 'Innodb_os_log_fsyncs'` (the count of
  `fsync()` calls InnoDB has issued against the redo log) against `SHOW
  GLOBAL STATUS LIKE 'Com_commit'` (the count of `COMMIT` statements
  executed, which under this benchmark's autocommit setup is one per
  statement). Sample both counters before and after the timed window;
  `Δ Com_commit / Δ Innodb_os_log_fsyncs` is InnoDB's commits-per-`fsync`
  under group commit, the same standard method used to characterise
  InnoDB group-commit efficiency operationally.
- **PostgreSQL:** `pg_stat_wal.wal_sync` (PostgreSQL 14+; "number of times
  WAL files were synced to disk via `issue_xlog_fsync`", present in the
  `postgres:17` image this repo already pins) against `pg_stat_database.
  xact_commit` for the `bench` database. `Δ xact_commit / Δ wal_sync` is the
  matching ratio.

**What it would take to wire in:** a small addition to `mysql_driver.py`
and `postgres_oltp_driver.py`'s connect/measure functions (query the counter
before and after the timed write loop, same pattern as the existing
`Timer`), and a corresponding read from `INLAYSQL_COMMIT_STATS`'s stderr
output in whichever process launches the InlaySQL side (host export or
containerised replay) — none of this requires a config change to either
server, since both counters are exposed by default. This is scoped as
future harness work (§5 item 4-adjacent), not implemented in this session,
per the task's instruction not to run the full benchmark battery; it is
recorded here so the next person who touches §3.5 has the exact counters
named rather than needing to rediscover them.
