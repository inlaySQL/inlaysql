# InlaySQL performance plan

Companion to [`docs/architecture.md`](docs/architecture.md). `docs/architecture.md` says *what* to build and in what
order; this file is the performance argument underneath Phase 2 — where the time
actually goes today, what the floor is for each workload, and which change buys
which microseconds.

**The rule this file lives under, from `AGENTS.md`:** no number appears in
`README.md` or `bench/README.md` unless it regenerates from `bench/run.sh` or
`bench/compare.sh`. Targets below are targets, not claims. Nothing here is
published until a deliberate regeneration pass on a quiet machine produces it.

---

## 1. The goal, stated honestly

The ambition is that InlaySQL's published figures beat SQLite, MySQL and
PostgreSQL. Three of those are more winnable than the fourth, and it is worth
being precise about which is which before spending effort.

**Against MySQL and PostgreSQL — reads decisively, writes we lose.**
Both are servers, so a client pays a socket round trip before their engine
begins. On reads that is decisive, and the first measured run says so:
InlaySQL 959k ops/s against MySQL's 9.7k and PostgreSQL's 57k, roughly 99x and
17x. The honest framing — the one `bench/README.md` already uses for the
pgvector row — is that we win *including* their round trip, and that their
throughput under high concurrency on many cores is a different question nobody
here has measured.

**On writes this file predicted a win, and it was wrong — twice over.**

The first measurement put MySQL at 1,318 durable commits/s and PostgreSQL at
1,222 against InlaySQL's 188, an apparent 7x loss. That was not a fair
comparison: the containers fsync to a virtual disk inside a Linux VM that does
not pass a write barrier to the hardware, while InlaySQL called `F_FULLFSYNC`
on the host.

AHL-451 removed that asymmetry by measuring InlaySQL **in a container too**, on
the same class of volume, so every engine pays the same fsync semantics. The
result (provisional, load ~3.8–4.5, two consistent runs):

| Engine | write ops/s | read ops/s |
| --- | --- | --- |
| InlaySQL, host (real `F_FULLFSYNC`) | ~170 | ~1.1M |
| InlaySQL, containerised | ~330–341 | ~600–735k |
| MySQL 8 | ~719–794 | ~10.4–10.9k |
| PostgreSQL 17 | ~545–775 | ~60–64k |

Two things follow, and the second is the uncomfortable one.

The virtualised fsync is worth about **2x**, not the ~14x the original
cross-check implied — so most of the apparent 7x gap was real after all, not
measurement artifact. And **with the asymmetry removed, MySQL and PostgreSQL
still write 2–2.4x faster than we do.** That is a genuine loss, not an
artifact.

**Group commit closed most of it** (AHL-461/AHL-468, item 7 of section 5). The
current published run has InlaySQL containerised at 723.1 durable writes/s
against PostgreSQL's 730.9 — level — and MySQL's 780.7, **1.08x** rather than
2–2.4x. The paragraph above is kept because it is the reasoning that promoted
group commit, and because the sequential single-connection shape is still the
one place the ambition to beat both engines is not met.

The likely cause is that every InlaySQL commit pays its own `fsync` while both
servers batch commits from concurrent clients into shared flushes. **Group
commit (Phase 2 item 5) is the change aimed at it**, and this is the number to
re-measure afterwards. Reads are not affected: containerised InlaySQL still
reads ~60x MySQL and ~10x PostgreSQL.

Still unproven either way: Docker Desktop's virtual disk was never verified to
honour `fsync` as a barrier *for any* of the three engines, so "comparable" is
not "hardware-durable". And containerising us does not remove the structural
asymmetry — we are in-process and pay no socket round trip.

**Against SQLite — winnable on writes, and now on read latency too.** Durable
point writes were already ~2.2x faster (one fsync per commit against the
journal's several) and concurrent writers roughly 2.9x. Reads were the problem,
and the page cache (AHL-420) changed the picture:

| | p50 | ops/s | p95 |
| --- | --- | --- | --- |
| InlaySQL, after Phase 0 | 6.75 µs | ~136k | 11.25 µs |
| **InlaySQL, with the page cache** | **584–709 ns** | 594–637k | 8.0–8.3 µs |
| SQLite, journal + `sync=FULL` + `fullfsync` | 3.29–3.42 µs | 235–238k | 8.1–9.3 µs |
| SQLite, WAL + `sync=NORMAL` | 834 ns – 1.00 µs | 908k–1.08M | 1.17–1.42 µs |

Two runs of `SUITE=points ./bench/run.sh`, both engines measured in the same
run. **Caveat: load average was 4.6–6.1, not a quiet machine** — the relative
comparison is fair because both engines were measured under the same load, but
these are not final numbers and none of them belongs in `README.md` until a
quiet-machine regeneration pass produces them.

**The p95 tail in that table was cold start, not a steady-state defect.** The
suite's default is 5,000 lookups over 20,000 rows, and the tree has several
hundred pages — so a large fraction of those lookups are still *filling* the
cache, and the tail is measuring the miss path. Amortising the warmup by raising
the lookup count to 50,000 (`LOOKUPS=50000 SUITE=points ./bench/run.sh`) settles
it, over two runs:

| | p50 | p95 | ops/s |
| --- | --- | --- | --- |
| **InlaySQL** | **459–500 ns** | **750–834 ns** | **1.45–1.63M** |
| SQLite, WAL + `sync=NORMAL` | 833–958 ns | 1.00–1.42 µs | 0.90–1.12M |
| SQLite, journal + `sync=FULL` + `fullfsync` | 3.17–4.08 µs | 4.63–6.63 µs | 214–281k |

So warm, InlaySQL is ahead of SQLite on **all three** point-read measures, in
both of SQLite's configurations — roughly 1.7–2.1x on median latency and
1.3–1.8x on throughput against the fastest one. The tail collapsed from ~8 µs to
750–834 ns, which is what identifies it as warmup rather than eviction thrash.

**Both numbers are real and both should be reported.** The 5,000-lookup row is a
cold cache and the 50,000-lookup row is a warm one; an application that opens a
handle, does a handful of reads and exits sees something closer to the first.

The remaining gap is that our *miss* path is more expensive than SQLite's, so we
warm up more slowly. **This paragraph used to say the cause was a per-miss
allocation; the profile in section 2 says otherwise** — that allocation was
already gone (AHL-420 landed a reusable scratch buffer alongside the cache), and
a miss is a `pread` (~334 ns) plus a decode (~208 ns), with the buffer worth
under 10% even when it existed. Section 2 is measurement; this section is
summary, and where they disagree section 2 wins.

Raising the suite's default lookup count so the published number looks better
would be exactly the kind of methodology change this file exists to prevent.
Report both, and say which is which.

**Against pgvector on vector-only search** this paragraph recorded a ~4x loss
(0.17 ms vs 0.73 ms) until the tuning pass and the AHL-495 regeneration
reversed it: 78 µs here against 159 µs there, close rather than decisive
because their figure includes a socket round trip. Section 4.

---

## 2. Where a point read's time actually goes

Traced through `Engine::run` for `SELECT ... WHERE id = ?` on an
`INTEGER PRIMARY KEY`, over the `points` fixture (20,000 rows, 64-byte payload,
a fresh handle every 5,000 lookups so misses are paid for real).

**This section used to be inference from the code and is now measurement**
(AHL-422): a `sample(1)` call-graph profile at 1 ms over 6,591 samples of that
loop, plus batched micro-timings of each component on the same machine.
**It contradicted the code reading in three places**, which is the whole reason
`PERF.md` orders profiling before optimising. Load average was 5–9 while
sampling — contended, so treat the absolute nanoseconds as a noise band and the
*proportions* as the finding.

### What the profile says, by self time

| Bucket | Share | Where it comes from |
| --- | --- | --- |
| `malloc`/`free` | **~39%** | every owned `Vec`/`String` on the path — see below for which |
| `memcmp` | **~21%** | B-tree key comparison during descent: ~11% the leaf `binary_search_by`, ~9% `child_pointer` in internal nodes |
| `pread` | ~15% | one per *missed* page per level |
| `PageCache::get` | ~9% | `BTreeMap` lookup plus LRU list surgery, on every **hit** |
| `mach_absolute_time` | ~9% | the harness's own per-lookup timer, not engine work |
| `from_utf8` | ~3% | validating the `TEXT` column in `decode_row` |

Component costs, timed directly on the same machine:

| Component | Cost |
| --- | --- |
| one `pread` of a 4 KiB page, warm OS cache | ~334 ns (p50) |
| `page::decode` of one 4 KiB page | ~208 ns (p50) |
| `vec![0u8; 4096]` | ~50 ns |
| a whole cold lookup, cache on | ~460–580 ns (p50) |
| a whole lookup with the cache **off** | ~6 µs |

### The step table, corrected

| # | Step | Where | Measured cost |
| --- | --- | --- | --- |
| 1 | Snapshot refresh | `Engine::refresh_snapshot` | ~0 since AHL-403 — one atomic load |
| 2 | Schema-stamp validation | `Statement::check_schema` | ~24 ns → **~11 ns** since AHL-422 |
| 3 | Rowid extraction | `Engine::pinned_rowid` | walks the filter expression tree per execution; not yet isolated |
| 4 | Key encoding | `storage::row_key` | ~28 ns → **~1 ns** since AHL-422 |
| 5 | Tree descent, per **missed** level | `CowBTree::read_committed_node` | one `pread` (~334 ns) + a decode (~208 ns). **No allocation** — AHL-420 landed the reusable scratch buffer with the cache |
| 5b | Tree descent, per **hit** level | `CowBTree::committed_node` | `PageCache::get`: a `BTreeMap` lookup and LRU relink, ~9% of the path |
| 6 | Row decode | `row::decode_row_masked` | `Vec<Value>` + one `String`/`Vec` per text/blob/vector cell — but **only for the columns the plan can observe** since AHL-462; the rest are walked past |
| 7 | Projection | `Engine::project` | **moves** each output value into `ResultSet` since AHL-462, when every item is a plain column and none repeats; clones otherwise |

**The three corrections, stated plainly:**

1. **The per-miss page allocation was already gone**, and was never the big
   part of a miss anyway. `CowBTree::with_page_bytes` has read into a reusable
   `RefCell<Vec<u8>>` since the page cache itself (AHL-420). At ~50 ns against
   a ~334 ns `pread` and a ~208 ns decode it was under 10% of a missed level:
   a miss is a **syscall and a decode**, not an allocation.
2. **Schema validation was not dominated by the structural `Table` compare.**
   That compare is ~6 ns of the ~24 ns; the larger part was the `String`
   `Catalog::table` allocated to lowercase the name on every call. So the
   catalog-version-or-hash stamp this file used to recommend would have bought
   the *smaller* half — and it is the half that is hard to make sound, because
   `Engine::refresh_catalog` replaces the catalog wholesale from
   `Catalog::decode`, so a per-instance counter can repeat across instances
   holding different schemas. Not worth trading an `Error::Stale` guarantee for
   6 ns. Making the lookup allocation-free was sound and bought more.
3. **Key comparison is a first-class cost and was missing from this table
   entirely.** `memcmp` during descent is ~21% of the path — more than `pread`.
   A row key is `table\0` plus 8 big-endian bytes, so every comparison walks
   the table-name prefix again before it reaches the bytes that differ.

### What is left, in measured order

`malloc`/`free` was ~39% of the path when this was profiled, which is what made
step 6/7 — `ValueRef` and the streaming executor — the real remaining win rather
than anything smaller. After AHL-422 removed the key and catalog allocations,
the biggest sources left were `resolve_value_at` cloning the value out of the
cached page, `decode_row` allocating a `String` per text cell, and
`Engine::project` cloning it once more.

**AHL-462 removed the third and narrowed the second** (see "the structural fix"
below); the first is untouched and is now the largest. **These proportions
have not been re-profiled since**, so treat the ~39% as the pre-AHL-462 figure
and re-run the profile before choosing what comes after `ValueRef`.

The two structural candidates the profile newly justifies, neither of them
attempted yet:
- **A cheaper key comparison**, since `memcmp` is ~21%. Either a key layout
  whose discriminating bytes come first, or a comparison that skips the shared
  table prefix a descent has already matched.
- **A cheaper cache hit**, since `PageCache::get` is ~9%. The LRU relink runs on
  every hit; a clock or second-chance policy would not touch a list per hit.

## The join and range profile (AHL-472 step 1, 2026-08-19)

`BENCHMARK.md` publishes two losses to SQLite that the point-read table above
does not explain: a full join was 7.6–11.4x slower and a 50-row indexed range
2.3–3.7x when this was profiled (5.56–10.71x and 2.05–2.82x in the current
run, after AHL-478 and AHL-479), while a *single* indexed point probe wins. So
the cost is per row, not per query, and the `LIMIT` shapes narrowing — 2.3–3.8x
then, 1.86–3.56x now — agree.

Profiled with `sample(1)` over a 30 s window covering the query phase of
`inlaysql-bench --suite joins --rows 20000 --queries 100 --limit 10`, 20,532
samples on the main thread at `6a1eaac`. Self-time, grouped:

| Cost | Samples | Share | What it is |
| --- | --- | --- | --- |
| Allocation (`malloc`/`free` family) | ~4,380 | **21%** | per-row `Value`/`Vec` churn |
| `PageCache::get` | 2,570 | **13%** | one call per level per descent, LRU relink on every hit |
| `memcmp` (+ its stub) | ~2,380 | **12%** | key comparison during descent |
| Tree walk (`child_pointer`, `walk`, `starts_below`, `get_from`, `node_at`, `page::decode`) | ~1,400 | 7% | descent machinery |
| `memmove` | 646 | 3% | page and value copies |
| `NestedLoopJoin::next` | 277 | 1.4% | the join iterator itself |
| `decode_row_masked` / `eval::evaluate` / `from_utf8` | ~500 | 2.5% | decode and predicate |

What that says, in order:

1. **Allocation is still first**, at ~21% even after AHL-462 narrowed it. This
   is the `ValueRef` work above, and the join path is where it now pays most —
   every probed inner row is assembled, cloned into an `ExecRow`, and dropped.
   **This 21% predates AHL-478/AHL-455 (the `ValueRef` conversion, corrected
   2026-08-30 in "The structural fix: stop allocating per row" below) and
   should not be trusted to still localize the cost.** A fresh profile is
   owed; see that section's correction for what a follow-up audit found is
   still actually unconverted.
2. **`PageCache::get` at 13% is the single hottest function**, well above the
   ~9% this file recorded for point reads: a join descends per outer row, so the
   per-hit LRU relink is paid `depth × outer_rows` times. The clock /
   second-chance policy proposed above is no longer a marginal idea.
3. **`memcmp` at 12% is a *descent* cost**, and a join re-descends from the root
   for every outer row where SQLite reseeks a cursor. Retaining the probe
   position between outer rows attacks items 2 and 3 together, which is why it
   is the first thing to try.
4. The join operator itself is 1.4%. **Nothing is wrong with the join
   algorithm** — the per-row machinery underneath it is what costs.

One anomaly worth chasing separately: `wal::encode_record` (190) and
`PageCache::insert` (153) appear in a *read* window at all. Either the harness
commits between shapes or a refresh is doing more than it needs to; it is small,
but a write encoder has no business in a read profile.

### What the fix bought (AHL-472 step 2, same day)

Two changes, aimed at items 2 and 3 of that list:

- **Clock / second-chance page cache.** `PageCache::get` sets a `referenced`
  bit instead of relinking an intrusive LRU list; eviction sweeps a hand,
  clearing bits, bounded to two passes. The no-invalidation reasoning the
  cache rests on (a page id names one immutable page for the file's lifetime,
  which AHL-406's `adopt_next_page_id` guarantees) is untouched — only
  eviction *order* changed.
- **A retained leaf cursor.** A committed point lookup remembers the leaf it
  resolved and the key span that leaf answers for; the next one searches that
  leaf directly when the key falls inside, and falls back to a full descent
  otherwise. The cursor is keyed by the root, so a commit, refresh or rebase
  stops it matching without any explicit invalidation step. This is what an
  index probe's per-row `get_row` calls hit.

Measured interleaved A/B between two prebuilt binaries, medians of four rounds:

| Shape | Before | After |
| --- | --- | --- |
| PK-inner join, full | 80.7 ms | **56.1 ms** (~30% faster) |
| Secondary-inner join, LIMIT 10 | ~52k joins/s | ~61k joins/s (+17%) |
| 50-row indexed range | ~48.6k ops/s | ~54.5k ops/s (+12%) |
| Secondary-inner join, full | ~183 ms | ~178 ms (~3%) |
| Point read, point probe | flat | flat — **no regression** in the case the LRU was tuned for |

The secondary-inner full join barely moved because only its row *fetch*
benefits; the entry-range walk itself is unchanged, and that is where its time
goes. Both DST sweeps green, since this touches the cache and the walk.

**Allocation was deliberately not attempted** and is now almost certainly the
largest single remaining category — steps 1 and 2 removed a chunk of the
`PageCache::get` + `memcmp` share while leaving the ~21% allocation share
alone. A full `ValueRef` conversion is the next run. Whoever takes it should
build a cleaner profiling harness first: the post-fix re-profile attempted here
was contaminated by the benchmark's own setup writes and SQLite's locking in
the same process, so no post-fix percentages are published above — only
wall-clock A/B, which is trustworthy.

**Corrected 2026-08-30: both of the above are done.** The executor-level
`ValueRef` conversion landed (AHL-478/AHL-455) and `crates/inlaysql-bench/src/bin/profile.rs`
is the cleaner harness this paragraph asked for — it links no SQLite, emits a
`PHASE_MARKER` so a profiler attaches only after setup, and takes
`--page-cache-bytes` to force the miss path. See the correction in "The
structural fix: stop allocating per row" below for the evidence and for what
a fresh audit found is still actually open — this paragraph's ~21%
allocation figure predates that fix and should not be read as still pointing
here.

### The entry walk, and what is actually left in a join (AHL-479)

AHL-479 profiled the secondary-index-inner join — the shape AHL-472 and
AHL-478 had each moved only ~3% — and **ruled out three suspects by evidence
rather than argument**, which is worth as much as the fix:

- The entry-range walk does *not* re-descend per entry. `CowBTree::walk` is one
  recursive traversal per call, visiting each node once regardless of how many
  entries match.
- The memcomparable key decode is **0 samples** — it reads eight trailing bytes
  and there is no value payload.
- The row-id re-sort is **under 0.5%** at these group sizes.
- AHL-472's retained leaf cursor is reachable only from `get_from` (point
  lookups and row fetches), never from `walk` — so the entry walk never had it.

What it did convict: `scan_index_range` cloned every admitted entry's key and
resolved its (always empty) value, while both callers immediately took the row
id and discarded the rest. `scan_range_row_ids_from` reads the row id straight
out of the borrowed entry, sharing `WalkBounds` with the general walk so both
provably visit the same entries — and pinned to it by
`an_index_row_id_walk_agrees_with_the_general_entry_walk`, per the
fast-path/slow-path rule. Indexed range throughput +15–18%; the full-join
shapes ~4.5%, because their time is elsewhere.

**Where it actually is, and one number that settles a question:** the joins
workload's posts table plus index is roughly 18 MiB against a default page
cache of 8 MiB, so the full-join shapes are miss-bound. It would be tempting
to read that as under-provisioning — but `PRAGMA cache_size` on the SQLite
this benchmark links is 2000 *pages* at a 4 KiB `page_size`, which is the same
8 MiB. **Both engines get the same cache budget and we still lose, so the gap
is not provisioning — it is that our miss path is dearer than SQLite's**,
exactly what this file has said since AHL-420 and never yet fixed. That, not
the cache size, is the next thing to attack in a join.

The AHL-472 anomaly is also resolved: `wal::encode_record` does **not** appear
in a clean read profile. It was an artifact of the contaminated window, not a
write encoder running during reads.

### The `LIMIT 10` joins, profiled on their own — the raw scan never asked the cache

The two `LIMIT 10` join shapes are the standing loss `BENCHMARK.md` publishes,
and until now they had never been profiled, because they *cannot* be seen in
the `joins` profile: a full join takes ~11 ms and a `LIMIT 10` takes ~20 µs, so
cycling all four shapes evenly gives the two under investigation about one
sample in five hundred. `profile.rs` grew a `joins-limit` suite that runs only
those two, and 30 seconds of `sample` over it says:

| Category | Self time |
| --- | --- |
| `_platform_memmove` | **31.4%** |
| allocator (`malloc`/`free` family) | ~16.9% |
| `_platform_memcmp` (+ its stub) | 11.4% |
| `JoinInner::prepare` → `scan_index_ro` | ~12% |
| `PageCache::get` | 3.0% |

Attributing the `memmove` by caller is what located it:

| Caller | Share of `memmove` |
| --- | --- |
| `FileDevice::read`, beneath `walk_raw_row_values` | 62.9% |
| `page::decode` → `Rc<[u8]>::copy_from_slice` | 20.3% |
| `walk_raw_row_values` → `Rc<[u8]>::copy_from_slice` | 12.0% |

**Ninety-five percent of the copying is whole pages, and the cause is that the
raw scan had no cache in it.** `CowBTree::with_page_bytes` — the only way
`walk_raw_row_values` reads a page — calls `device.read` directly.
`committed_node`, the descent path, consults `PageCache` first; the raw scan
never did. So a *prepared* query re-`pread`ing and re-copying the same pages on
every single execution, then copying each leaf a second time into a fresh
`Rc<[u8]>` for the parsed rows to borrow from, and decoding every internal node
on its spine from scratch each time.

The fix uses machinery that was already there. Since AHL-455 a decoded `Node`
carries the page bytes its borrowed keys index into, so a cache hit hands back
exactly what the leaf scan wants — `Rc::clone`, no syscall, no copy, whatever
kind the page turns out to be. `walk_raw_row_values` now asks the cache first,
and inserts the internal nodes it decodes so the next execution walks the same
spine for free. Leaves are deliberately *not* inserted: they were never decoded
into a `Node` here, and decoding one purely to cache it would give back the
allocation this scan exists to avoid.

Measured with `REPEATS=3 SUITE=joins ./bench/repeat.sh` on both sides, median
of three runs each:

| Shape | Before | After | vs journal SQLite |
| --- | --- | --- | --- |
| PK inner, `LIMIT 10` | 17.46 µs | **10.21 µs** | 5.43x → **3.20x slower** |
| Secondary-index inner, `LIMIT 10` | 21.58 µs | **15.17 µs** | 5.85x → **4.09x slower** |
| PK inner, full join | 11.23 ms | 10.61 ms | ~1.1x slower |

**1.71x and 1.42x on the two published losses**, and about 6% on the PK full
join. The `indexed` suite is unchanged within its own noise (range p50 13.46 µs
against 14.08 µs, on a run whose spread report puts 12.9% on that row), which
is what should happen: the index entry walk is a different function and this
did not touch it.

What is still there, and is now the next thing: 11.4% in `memcmp` during
descent, ~17% in the allocator, and one index descent per outer row in
`JoinInner::prepare`. The last of those is what AHL-479 predicted and what the
retained-cursor idea in "the structural fix" below is aimed at.

**Why a cache in the read path is a correctness change, and what was run.**
Serving a page from a cache rather than from the device is exactly the class of
change AHL-406 came from — a database recovered to a state no commit ever
wrote, with no checksum failing anywhere — so the three fault-injection sweeps
were run against it rather than left to CI:

| Sweep | Schedules | Result |
| --- | --- | --- |
| `dst_sweep` (crash / torn write) | 10,000 seeds | pass, 117 s |
| `index_recovery_dst` | 10,000 schedules | pass, 364 s |
| `free_list_reuse_dst` (page id reuse) | 5,000 seeds | pass, 129 s |

**Correction, and it is the important part of this section.** The first
edition of these lines said the page-reuse sweep was "the one that matters for
this change", on the reasoning that a cache keyed by page id is only under test
where page ids are recycled. An independent review checked whether that sweep
reaches the changed code and it does not: `free_list_reuse_dst` verifies
through `db.scan()` → `scan_prefix` → `walk`, the *decoded* walk, and
`walk_raw_row_values` is never executed by it. So the three sweeps establish
that the change breaks nothing they cover, which is worth having and is not the
same claim. The path that does exercise the leaf cache hit is
`a_row_values_walk_agrees_with_the_general_walk`, whose batched resume loop
runs after a `scan_prefix` has warmed leaves into the cache — a unit test, not
a fault-injection sweep. **A fault-injection sweep that drives the raw scan
under page reuse does not exist and is owed.**

**Paid (2026-08-30).** `crates/inlaysql-core/tests/free_list_reuse_dst.rs`
gained `raw_scan_sweep`. `scan_prefix_row_values_raw_from` is crate-private,
so an external integration test can only reach it through the public
`Storage`/`RowScan` seam — `TreeStorage` plus `inlaysql_core::traits::scan_all`,
which is exactly `RowScan` → `Storage::scan_batch` →
`scan_prefix_row_values_raw_from`, the path a real `SELECT` uses — rather than
a raw `CowBTree`. It reuses this file's own `TrustedDevice` (the durability
and reader-watermark trust reclaim needs), turns page reuse on from creation,
and verifies through that raw-scan path twice: **live**, after every commit,
against the workload's own in-memory model, over a row space (96) wider than
`RowScan`'s first batch (32) so this genuinely drives more than one
`Storage::scan_batch` call per scan — "retained across calls" meaning
something, not one raw-leaf walk measured in isolation — and **after
recovery**, against every snapshot the workload actually committed, exactly
`sweep`'s own invariant. 300 seeds by default, 5,000 under `--ignored`
(`thousands_of_seeds_of_raw_scan_under_reuse_recover_to_a_committed_snapshot`,
now part of the same CI sweep job as `heavy_churn_with_reuse_on_recovers_to_a_committed_snapshot`).

**One honest limitation, found while writing this.** Reading
`scan_range_row_values_raw_from` closely first: `RawScanCursor` retention is
already gated on `!self.device.page_reuse_enabled()` (`tree.rs`), so once a
device reports reuse enabled, the cursor is structurally never retained or
consulted — the staleness this debt worried about cannot occur through this
path, by construction. `raw_scan_sweep` therefore cannot manufacture a
stale-cursor read to prove a guard catches one; there is no such reachable
state. What it proves instead, and what was genuinely missing, is that the
raw leaf-parsing walk itself — the code that runs on every call whether or
not a cursor is retained — is correct under page reuse with fault injection,
live and after recovery, across thousands of seeds, and that the
generation-gate keeps holding as a standing regression guard: a future change
that broke it would show up here as a live-scan mismatch, immediately, not
only after a crash. It does not probe the one way the gate itself could be
defeated — a device reporting `page_reuse_enabled() == false` while reuse is
genuinely live — because no device in this workspace can be made to do that
today; that would need a harness change, not a test.

The two guards that make it safe, by reading: `cached_page` refuses the cache
when `pending && dirty.contains_key(&id)` — the same two-step `node_at` and
`committed_node` already perform, so a transaction still reads its own writes —
and `cache_committed` carries the identical guard, so a page read as *dirty*
bytes is never inserted as though it were committed. The leaf fast path is
equivalent to the raw one because `page::decode` refuses any buffer that is not
`page_size` and stores `Rc::from(bytes)` for the whole page, and
`scan_leaf_cells` re-checks that length itself.

That reading has since been checked independently, against all five of the
failure modes above plus the question of whether `scan_leaf_into` can diverge
between its two callers. Verdict: no defect, each item refuted with the code
that refutes it. The review is also what produced the correction above — the
author's own summary had claimed sweep coverage the sweep does not provide,
which is the failure mode a self-review is worst at catching.

One thing it surfaced that this change did **not** introduce, recorded because
it is real: `invalidate_for_reuse` clears a handle's decoded cache only when
that handle has `page_reuse` enabled, and `note_page_reuse_enabled` flushes
only the device's raw-page cache, never another handle's `PageCache`. Two
handles on one file in one process, one with reuse on and one with it off, and
the second can serve a reclaimed page's previous occupant from its own cache.
The decoded walk already had this; the raw scan now shares it.

### The write path: a commit was paying a second fsync (AHL-480)

Profiled with the harness's new `writes` suite — a single-connection durable
commit loop, warmed past `CDC_RETENTION` so the window is steady state.
19,217 samples, native host, real `F_FULLFSYNC`:

| Cost | Share |
| --- | --- |
| `F_FULLFSYNC`, the ordinary commit's barrier | **86.2%** |
| **A second `F_FULLFSYNC`, from a WAL-region wrap in the same commit path** | 2.9% |
| `wal::encode_record` (checksum + copy) | 1.3% |
| `CowBTree::put`/`insert_into` | ~4.7% |
| `pwrite`, `memmove`, allocation | ~3% |

So a durable commit is **~89% fsync and ~11% our own work** — fsync-bound, as
expected. But part of "our own work" was manufacturing a *second* barrier:
`Engine::trim_changes` expired exactly one change-log entry per commit, and
that entry sits at the opposite end of the retained `cdc:` key range from
everything else a commit touches. That forced a third copy-on-write path per
commit, inflating the WAL record enough to wrap the 1 MiB region every ~57
commits and pay a checkpoint-style sync mid-hot-path. Trimming in batches of
64 (`CDC_TRIM_BATCH`) pushed the wrap interval to ~283 commits, measured
directly. The retention bound stays a bound — the log runs at most 63 entries
past `CDC_RETENTION` before catching up.

**Treat the throughput figure as provisional.** The A/B showed durable writes
~224/194 → ~251/273 ops/s, but that is two rounds on a machine running other
agents, and ~25% is far more than the wrap-frequency change alone predicts
(~2–3%). Either `F_FULLFSYNC` on this host scales with bytes queued rather
than call count, or the measurement is noisy. The *mechanism* is solid and
directly measured; the magnitude needs a quiet machine before it goes into
`BENCHMARK.md`.

**What stays structural.** MySQL was 2.7x ahead on this shape when this was
written and is 1.08x ahead containerised in the current run (1.43x server to
server at one connection); this change does not close what remains. Every
InlaySQL WAL record embeds a full `page_size` copy of
each dirty page — that is what makes a record self-contained for recovery —
where InnoDB's redo log carries small physiological diffs. Per-commit bytes
are therefore structurally larger here, and that is a page-format question, not
something a correctness-preserving fix reaches.

### The durable commit, counted — and what the MySQL/PostgreSQL row measures (AHL-496)

AHL-480 profiled this loop once and found a real defect. This pass counted it
instead of sampling it, then went and measured the *device* the comparison runs
on. Both answers were surprises, and the second one is the important one.

**What a single-row `INSERT` actually costs.** Instrumented with a counting
`Device` wrapped around `FileDevice`, over 3,000 steady-state commits of
`profile --suite writes` (warmed past `CDC_RETENTION`, ~7,100 rows resident):

| Per durable commit | Measured |
| --- | --- |
| `sync` calls | **1.0257** |
| `write` calls | 2.051 |
| device `read` calls | 6.40 (26.2 KiB) |
| Dirty pages copied into the record | **6.45** (mode 7; 5 while the tree is one level shallower) |
| Bytes: WAL record | 26.5 KiB |
| Bytes: data-area copy of the same pages | 26.4 KiB |
| Bytes: WAL-region zero-fill, amortised | **26.9 KiB** |
| **Bytes total** | **79.9 KiB** |

Three things fall out of that table.

**The 1.0257.** The second `F_FULLFSYNC` AHL-480 named is still there, and its
frequency has moved the wrong way: the WAL region wraps once every **39**
commits, not the ~283 that section claims. The arithmetic says it cannot be
283 — a 1 MiB region cannot hold 283 records of 26.5 KiB — so treat the old
figure as measured on a database small enough to be one B-tree level shallower,
and this one as what a steady state costs. A wrap is not waste: it writes and
syncs the state block first, because records that a checkpoint has not yet
covered cannot be erased. What *is* waste is that it then writes a whole 1 MiB
of zeros, which is where a third of every commit's bytes goes.

**Six and a half pages for one row.** Mode 7, and it decomposes exactly: a
root-to-leaf path for the row (4 pages at this depth) plus a second, disjoint
root-to-leaf path for the metadata cluster — `write_version`, `next_row_id` and
the newest `cdc:` entry, which are adjacent to each other but nowhere near the
row — sharing only the root, so 3 more. **Roughly 43% of every commit's bytes
exist to maintain three counters and one change-log entry**, and that is a
consequence of rows and metadata living in one tree, not of the page format.

**Six and a half reads, too.** A commit writes 6.5 pages and the *next* commit
reads those same pages back off the device, because a committed page is dropped
from the handle rather than promoted into the decoded cache. For a write-only
workload the page cache is a 100% miss and the descent pays a `pread` and a
`page::decode` per level, every commit.

**Where the time goes, in two regimes.** `sample(1)` on the `writes` suite,
self time. The left column is a real `F_FULLFSYNC` against the internal SSD;
the right is the same binary with its file on an APFS RAM disk, which is a
stand-in for a container volume — cheap barrier, everything else identical.

| Cost | Host, real `F_FULLFSYNC` | Cheap `fsync` |
| --- | --- | --- |
| `fsync`, the commit's own barrier | **88.4%** | 42.3% |
| `fsync`, from a WAL-region wrap | 2.5% | 1.4% |
| `wal::encode_record` | 1.3% | **11.7%** |
| allocator | 2.1% | **14.6%** |
| `memmove` | 1.0% | 8.3% |
| `pwrite` | 1.5% | 3.6% |
| `pread` | 0.7% | 2.9% |
| throughput | 237 ops/s | 4,414 ops/s |

So on the host the answer is "91% fsync" and nothing we do to the CPU can move
it by more than ~9%. The interesting column is the right-hand one, because that
is the regime `BENCHMARK.md`'s containerised row runs in.

**What was fixed.** `wal::encode_record` was copying every dirty page **three**
times to emit one record — `bytes.clone()` into `WalRecord::pages` at the call
site, `extend_from_slice` into a `body` `Vec` that started at 128 bytes and
`realloc`'d its way to 26 KiB, then a third copy of the whole body into the
buffer the checksum ran over — plus a fresh 26 KiB allocation in
`write_dirty_pages` for the coalesced data write. `encode_record_into` does one
pass into a buffer the handle keeps (`record_buf`/`run_buf`, capped at 64 KiB of
retained capacity so a bulk load cannot leave a megabyte pinned per
connection). **The bytes on disk are unchanged**, which
`the_borrowed_encoder_matches_the_owned_one` pins byte for byte in both
layouts.

Measured interleaved, six rounds each, medians: **4,170 → 4,257 ops/s** in the
cheap-`fsync` regime (~+2%), non-`fsync` samples down 4.1%. On the host it is
invisible, as the 91% predicts (117/131/126 against 134/130/114 — noise). This
is a small win and is reported as one.

**What is left, and why it is not reachable from here.** With the copies gone,
the largest single item of our own work is the **FNV-1a checksum over the
record: ~15%** of a commit when `fsync` is cheap. Confirmed by stubbing it out
in a throwaway build — 4,363 → 4,987 and 4,358 → 5,078 ops/s, +14% and +16%.
FNV-1a is a serial xor-multiply chain, ~4 cycles a byte; there is no faster way
to compute *the same value*, and changing the value is a format break. The only
lever is the byte count, which is 26.5 KiB because the record carries 6.5 whole
page images. Every other cost in the table scales off the same number.

**And now the part that reframes the benchmark.** `BENCHMARK.md` reports
InlaySQL at 849.7 durable writes/s containerised against PostgreSQL's 1,612.8
and MySQL's 1,184.2, and calls trailing both "the finding". Measured on a
Docker named volume of the same class, from a container, with a loop that does
nothing but `pwrite` + `fsync`:

| Bytes per `fsync` | 4 KiB | 26 KiB | 80 KiB | 256 KiB |
| --- | --- | --- | --- | --- |
| Durable commits/s (round-robin, median of 6) | 850 | 846 | 836 | 875 |

**The volume's `fsync` cost is flat in bytes.** Our 80 KiB per commit costs
exactly what PostgreSQL's few hundred bytes cost. `fdatasync` is not cheaper
than `fsync` there (723 against 857), and extending the file is not dearer than
overwriting in place (741 against 818, inside the noise) — so neither the write
amplification nor the fact that we grow the file every commit is being paid for
on this storage. One durable commit costs ~1.18 ms and **nothing else matters**.

Against that floor, back to back in one session: **PostgreSQL 17, one client,
`fsync=on`/`synchronous_commit=on`, on its own named volume: 769–827 tps**, when
the raw floor was 836–850. InlaySQL containerised, run in the same session:
735/827/1,207 ops/s. Everyone is at the same wall.

And the wall moves. The same probe, same command, same machine, ninety minutes
apart: 1,777 commits/s and 846 commits/s — **a 2.1x drift with host load.**
1,612.8 is 91% of the fast reading; 849.7 is 100% of the slow one.

So the honest statement is: **the published row is measuring the volume's
`fsync` latency at two different moments, not two engines.** InlaySQL is already
at the one-`fsync`-per-commit floor of that device. Beating PostgreSQL there is
not a matter of doing less work per commit — at 1.18 ms of barrier against
~0.2 ms of our own work, the CPU side is ~15% of the number and the whole of
this section's fix is 2% of that 15%. It requires committing *less often than
once per commit*, which for a single sequential connection means either group
commit (which cannot fire — one writer, one commit in flight, by design) or a
durability relaxation this engine deliberately does not offer.

**What is owed.** Three measurements, two of them cheap:

1. ~~Re-run the whole containerised comparison **in one session, interleaved**,
   InlaySQL and PostgreSQL and MySQL and the raw `pwrite`+`fsync` floor
   alternating, on a quiet machine. Until that exists, no ordering in that
   table should be believed — including the ones that flatter us.~~ **Paid
   2026-08-30.** See "The containerised comparison, profiled instead of
   trusted" below for the sequential rerun that first confirmed the
   instability, and `BENCHMARK.md`'s "Interleaved, repeated, quiet-machine
   rerun" section for the fix: 5 repetitions, each InlaySQL/MySQL/PostgreSQL/
   floor run back to back, load-gated manually (`bench/compare.sh` still has
   no automated gate — see the recommendation there). Result: PostgreSQL led
   MySQL in 5/5 repetitions (the published table's ordering, not the flipped
   one the sequential rerun found), median multiples 1.81x/1.43x against the
   published 1.90x/1.39x, and the raw fsync floor's own spread (15.4%) was far
   smaller than any engine's (50-81%) and weakly/inconsistently correlated
   with them (Pearson r: MySQL +0.51, PostgreSQL +0.46, InlaySQL **-0.51**) —
   so on an already-warm, already-quiet stack the floor is not the dominant
   source of the remaining noise; something else (driver overhead, process-
   spawn jitter, or the compose network) is. Raw data:
   `bench/results/20260830T095714Z-interleaved-oltp-compare.txt`.
2. Repeat the **transport-matched** comparison (`bench/external/
   server_driver.py`, the "Server-to-server" table, `inlaysql serve --mysql`
   against MySQL over the same `mysql.connector` client) interleaved, several
   repetitions, on a quiet machine — the same discipline item 1 just applied
   to the containerised comparison. The one matched-transport run that exists
   (2026-08-30, alongside the interleaved rerun above: InlaySQL 627.6 ops/s
   against MySQL's 849.4 at one connection) did not confirm this section's
   own prediction that a fair, transport-matched comparison would show
   InlaySQL doing worse, not better — the matched-transport gap (MySQL
   ~1.35x faster) came out *smaller* than the containerised one (MySQL
   1.43x faster, this rerun's own median), the opposite of the predicted
   direction. It is one run on a workload already shown to swing 50-81%, so
   it settles nothing on its own; see `BENCHMARK.md`'s "Transport-matched,
   single run" for the full arithmetic. This is now open, not closed.
3. If the write path is picked up again, the target is the **6.5 pages**, not
   the code around them. In descending order of what the count says: the second
   root-to-leaf path for the metadata cluster (~43% of the bytes), the 1 MiB
   zero-fill per wrap (~34% of the bytes — and it may not be needed at all,
   since `read_committed_state` already filters records at or below the
   checkpoint the wrap has just synced, but that argument is exactly the class
   AHL-406 punished and needs its own DST pass before anyone acts on it), and
   promoting freshly committed pages into the decoded cache so a commit stops
   re-reading what it just wrote.

### Why a miss is dearer than SQLite's, located (AHL-488)

`PERF.md` had asserted since AHL-420 that our miss path is dearer than SQLite's
without ever saying *why*. Profiled three ways with the harness's new
`--page-cache-bytes` flag, which forces the miss path independently of the row
count:

| Configuration | `pread` | alloc/free | `memcmp` | decode | cache admin |
| --- | --- | --- | --- | --- | --- |
| Cache disabled (`points`, 0 B) | 21.6% | **58.6%** | 1.1% | 4.5% | — |
| Forced thrash (`points`, 256 KiB) | 18.2% | **53.1%** | 3.4% | 3.1% | 4.7% |
| The miss-bound join (8 MiB cache, 18 MiB set) | 20.9% | **28.4%** | 15.5% | 0.6% | in nav |

**Allocation is the miss path**, at more than double `pread` in the pure cases
and still the largest single category in the realistic blended one.

**Corrected 2026-08-30: this 28.4–58.6% range predates the landed `ValueRef`
conversion (AHL-478/AHL-455)** — the page-decode representation this section
blames has since changed shape (see the correction in "The structural fix:
stop allocating per row" below). It is no longer safe to read these
percentages as still pointing at `page::decode`'s per-cell allocations; a
fresh profile is owed before anyone acts on this table.

The cause is exact: `btree::page::decode` eagerly materialises a `Vec<Entry>`
and `Vec<Separator>` for *every* cell on a page — a `Vec<u8>` per key and an
`Rc<[u8]>` per inline value, around 49 of each for a leaf in this fixture —
when a point descent needs precisely one of them.

Two suspects were **ruled out with evidence** rather than left as suspicions:
the per-miss page buffer is already pooled (AHL-420's scratch buffer in
`with_page_bytes`), and there is **no checksum on a data-page read at all** —
only WAL records and the header/state block carry one, so there was never
anything there to remove.

**Why nothing was fixed in that run, and what the fix costs.** The sound
version is to make `Entry.key`, `Separator.key` and `ValueRef::Inline` offset
views over one shared `Rc<[u8]>` per page instead of per-cell allocations. But
those types are also constructed throughout `tree.rs`'s insert, split, merge
and rebase paths, which have no page buffer to slice from — so it is a
representational change across most of the tree module, with its own DST pass,
not something to bolt onto a profiling run. The cheap-looking alternative —
decode only the cell a descent needs and skip caching that page — was checked
and rejected: both `PageCache`'s hit path and AHL-472's retained leaf cursor
require a *fully* decoded node, so it would defeat two shipped, measured wins
for every workload whose cache is not thrashing, with no way to tell in advance
which kind of miss you are looking at.

So the structural statement is now precise: **SQLite walks a raw page in place;
this engine decodes into owned, `Rc`-shared structures, which is what makes a
hit ~500 ns and what makes a miss expensive.** That trade is the join loss.

### The page-view attempt, twice, and why it did not land (AHL-493)

AHL-488's proposed fix — make `Entry.key`, `Separator.key` and
`ValueRef::Inline` offset views over one shared `Rc<[u8]>` per page instead of
per-cell allocations — was built, measured, revised once, and **not merged**.
It is written up here because the trade turns out to be structural, and the
next person to have this idea should start from the numbers rather than from
the theory.

All figures below are interleaved A/B between two prebuilt binaries, measured
independently of the implementing run:

| Path | main | View-only | View + 23-byte inline |
| --- | --- | --- | --- |
| Cold, cache disabled | 163–173k ops/s | 341–363k (2.2x) | **320–326k (1.9x)** |
| Warm point read | 1.89–1.93M | 1.49–1.63M (−18%) | **1.77–1.86M (−5.7%)** |
| Small cache-resident join (2,000 rows) | 318 ops/s | ~180 (−43%) | **187 (−41%)** |
| `joins` at 20,000 rows — the miss-bound target | 16 ops/s | 16 — flat | 16 — flat |

The allocation share on a miss falls from ~64% to ~19%, exactly as predicted,
and the cold path really does roughly double. Two things stop it:

1. **It never fixed the workload it was for.** docs/architecture.md item 1 exists to close
   the join loss to SQLite. The join number is identical before and after, in
   both attempts. What was built is a cold-path improvement that happens to
   live in the code the join profile pointed at.
2. **A small fully-cache-resident join loses 41%**, reproducing to the ops/s
   across rounds. That is not an exotic shape — small tables joined repeatedly
   is ordinary application traffic.

**Why the second attempt could not close it, which is the durable finding.**
The revision copies short keys inline (23 bytes, sized so `PageBytes` costs no
more than the `Vec<u8>` it replaced) so the hot comparison skips the `Rc`
indirection entirely. That works for a *row* key — `table\0` plus eight bytes.
It cannot work for an *index* key: `\x01idx:{name}\0` plus the encoded value
plus an eight-byte row id is 36 bytes for an integer index and 58 for a
30-character text index. Raising the cap to cover them closed most of the join
gap and made point reads measurably worse, because every leaf pays for a larger
`Entry` including the ones that already fit. **No single threshold helps both**,
and under `#![forbid(unsafe_code)]` there is no way to compare through the
shared buffer without the bounds-checked indirection that costs the 20.5 ns →
22.6 ns per probe.

So the cold path and the index-key path want opposite representations. Landing
this would trade a published claim — 1.36M point reads, 1.33x SQLite in WAL
mode — and a 41% cut to small joins, in exchange for a cold-path gain on a
workload nobody has reported as a problem. The branch
(`agent/phase2-page-decode-views`) is kept for whoever revisits it, most
plausibly if the WASM or edge story ever makes short-lived processes the
priority, since those are exactly the cold-start case.

### Prefix-skipping key comparison during descent, and why it was a wash

The remaining untried angle this document named — "a key layout whose
discriminating bytes come first, or a comparison that skips the shared table
prefix a descent has already matched" — was tried in its lower-risk form
(comparison, not layout: no on-disk format change) and **did not clear the
bar**. Written up for the same reason AHL-493 above is: so the next person
starts from the numbers.

The idea is a real, established technique ("prefix truncation during
search"): within one root-to-leaf descent, once a boundary key at one level
has matched N bytes of the search key, every key at every deeper level is
provably guaranteed to share those same N bytes — the tree structure proves
it — so a descent can track "bytes already known to match" and start each
comparison from that offset instead of byte 0. Implemented scoped to
`CowBTree::walk`/`walk_row_ids` (the recursive entry-range walk behind range
scans, index probes and join probes), leaving `get_from` (the point-read row-key
path) untouched.

Interleaved baseline/modified release binaries, 3 rounds, `inlaysql-bench
--suite joins --rows 20000 --queries 100 --limit 10` and `--suite indexed
--rows 100000`:

| Shape | Baseline | Modified | Verdict |
| --- | --- | --- | --- |
| PK-inner full join p50 | 63.4ms | 58.5ms | ~8% faster, but within the baseline's own 54.6–63.7ms spread — weak signal |
| PK-inner join, `LIMIT 10` p50 | 5.4µs | 6.7µs | **regression, ~24%, reproducible across all 3 rounds** |
| Secondary-index-inner full join p50 | 220.1ms | 220.1ms | flat |
| Secondary-index-inner join, `LIMIT 10` p50 | 13.0µs | 14.2µs | regression, ~9% |
| Indexed point probe / 50-row range | flat | flat | noise |

A `sample`(1) profile confirmed the mechanism itself is real — `memcmp`
self-time share dropped 16.2% → 12.0%, exactly as the theory predicts — but
the bookkeeping needed to compute the skip safely (threading a proven-match
count through the recursion) grew from 8.2% to 13.3% of the same profile,
erasing the saving: combined comparison-plus-bookkeeping share moved 24.4% →
25.3%, net flat to worse.

**Why it does not pay off here.** This technique's cost is roughly
`O(descent depth)` per walk call and its saving is `O(entries actually
compared)` — it pays off when one walk call examines many entries. This
join workload's dominant cost, named earlier in this document, is instead
*re-descending from the root once per outer row*: each descent touches very
few entries, which is the worst case for a technique with fixed per-node
overhead. Point reads and small joins were confirmed unaffected at the code
level (`get_from` was never touched), so this does not reproduce AHL-493's
regression shape — it simply does not help the workload it targeted.

**Done, since this section was written — not this one again if you are
reading it fresh.** `bfac72a` ("retain range leaf cursor", AHL-479) built
exactly the idea named here: `RangeCursor` retains the entry-range walk's
last leaf and the key span it answers for, keyed by root and device
generation the same way AHL-472's point-lookup cursor is, and
`CowBTree::scan_range_row_ids_from` (what backs `Storage::scan_index_row_ids`,
the join-probe and index-range path) checks it before falling back to
`walk_raw_row_ids`. It landed after this section's prose and was never
folded back in here — caught while re-profiling for W2 (2026-08-29): a fresh
`joins-limit` profile on the same shape this section describes shows the
secondary-index-inner join at 7.93x faster than journal SQLite (was 3.65x)
and its `LIMIT` shape at 2.41x slower (was 5.81x) — see `BENCHMARK.md`'s
Joins section for the regenerated table and what is left in the profile now
(the same AHL-488/493 allocation cost, still open, not this). A later
regeneration (2026-08-30, `2cb2539`, median of three runs, no code change to
either join path in between) found 7.23x and 2.95x — both within this
benchmark's own noise band of the figures above, not a further code-driven
move; `BENCHMARK.md` has the current numbers.

### The structural fix: stop allocating per row

The deepest issue is that `Value` owns its data — `Text(String)`, `Blob(Vec<u8>)`,
`Vector(Vec<f32>)`. Every row that crosses the executor is therefore a set of
heap allocations, and every clone through aggregate/sort/project is another set.
`docs/architecture.md` D5 calls for a streaming executor; the allocation story is the half of
it that matters most for point reads.

**The streaming executor landed (AHL-462) and took the *copies* out; the
*ownership* is still there.** Of the three sources named above:

- `decode_row` allocating a `String` per text cell — **narrowed, not removed**.
  A column the plan cannot observe is now walked past rather than decoded
  (`row::ColumnMask`), so `SELECT body FROM kv WHERE id = ?` allocates for
  `body` and for nothing else. A column the query *does* read still allocates.
- `Engine::project` cloning each output value again — **removed for the common
  shape**. When every output item is a plain column and no column is projected
  twice (`SELECT *`, `SELECT a, b`), the value is moved out of the row instead
  of cloned. A projection containing an expression still clones, because
  expressions are evaluated in item order and would read a column an earlier
  item had moved out.
- ~~`resolve_value_at` cloning the value out of the cached page — **untouched**.
  This is the one a real `ValueRef` is for.~~ — **done (AHL-478/AHL-455,
  corrected 2026-08-30).** See below.

Two more copies went with them: `aggregate` borrowed its group instead of
cloning every row into a second `Vec`, and `sort_rows` moves rows through the
keyed form instead of cloning them twice.

~~What is left is the invasive part, and it is still the single largest
remaining win: an internal borrowed `ValueRef<'a>` the executor uses, with
owned `Value` materialised only at the public API boundary. That turns step 6
from "allocate per cell" into "slice into the cached page". `eval.rs`,
`engine.rs` and `plan.rs` all assume owned values, so it is a change of its
own — and now that the pipeline is iterators with one owner per row, it is a
smaller one than it was.~~

**Corrected 2026-08-30: this is built, not still open.** A read-only audit
(`2d67a23`) verified by `git merge-base --is-ancestor` that all of the
following are ancestors of `main`'s HEAD, i.e. already shipped, not a plan:

- `ValueRef<'a>` with borrowed `Text(&'a str)`/`Blob(&'a [u8])` —
  `crates/inlaysql-core/src/value.rs:320-403`.
- `RowBuf` sharing page bytes via `Rc<[u8]>` instead of copying them —
  `crates/inlaysql-core/src/row.rs:29-109`. Its own doc comment names this
  section explicitly: "the whole of the fix for the site `PERF.md` names as
  'untouched and now largest'".
- `decode_row_ref_masked`, allocation-free for `Text`/`Blob` cells —
  `crates/inlaysql-core/src/row.rs:318-331`.
- `DecodeFilter`, which decodes borrowed and only materialises owned `Value`s
  for the rows that survive a predicate — `crates/inlaysql-core/src/exec.rs:341-391`.
- `resolve_value_at` itself — the exact clone this section named as the
  target — is now a refcount bump (`Rc::clone`) for the inline case, not a
  byte copy — `crates/inlaysql-core/src/btree/tree.rs:2810-2863`.

This landed as AHL-478/AHL-455. (A separate, page-*view* attempt at a related
idea, AHL-493, was built, measured and **rejected** — see "The page-view
attempt, twice, and why it did not land" above — and that section's "not
merged" framing is still accurate; it is a different change from the one
corrected here.)

**What a fresh audit found is still actually unconverted — flagged here so
the next person does not have to re-derive it, and does not act on the stale
21%/12% (join profile, above) or 28.4–58.6% (AHL-488, above) figures to find
it, since both predate this fix:**

- `IndexProbe::fetch` (`crates/inlaysql-core/src/exec.rs:663-665`) and its
  scan fallback (`crates/inlaysql-core/src/exec.rs:677`) still call the
  fully-owned `decode_row_masked` rather than the borrow-then-filter pattern
  `DecodeFilter` uses.
- `JoinInner::append_row_into` (`crates/inlaysql-core/src/exec.rs:468-474`)
  still deep-copies `Blob`/`Vector` cells (`.cloned()`) once per outer-row
  pairing, for both the materialised and hash-join branches.

Neither has been profiled since this fix landed. Whoever picks this up next
should re-run the profile first — the percentages elsewhere in this document
were all measured against the pre-fix decode path and cannot be trusted to
still size these two spots correctly.

Cheaper wins, in rough effort order:
- ~~**`row_key` without allocating**~~ — **done (AHL-422)**. `RowKeyBuf` builds
  the key in a stack buffer, so `TreeStorage`'s point paths allocate nothing.
  ~28 ns → ~1 ns.
- ~~**Cheapen `Statement::validate`**~~ — **done (AHL-422)**, but not the way
  this file predicted: see correction 2 above. The win was removing the
  `String` `Catalog::table` allocated, not replacing the structural compare.
  ~24 ns → ~11 ns, and the `Error::Stale` guarantee is untouched.
- **Cache the rowid decision on the `Statement`.** `pinned_rowid` re-derives the
  same answer from the same plan on every execution; it is a property of the
  plan, not of the parameters, so compute it once at prepare time. Not yet
  isolated in a profile — measure it before assuming it is worth the change.
- ~~**Projection pushdown.**~~ — **done (AHL-462)**. `row::ColumnMask` is the
  set of columns a plan can observe, built by walking every expression that can
  reach one; anything it cannot enumerate widens to "decode everything", so a
  mask that is too narrow is a compile error rather than a wrong answer. The row
  codec is unchanged — skipping a column is a length read and a cursor bump,
  which is far cheaper than allocating a `String` for it.

### Only if the above is not enough
A **column-offset directory or NULL bitmap in the row format** would make
"decode column 7" O(1) instead of O(7). That is a storage-format change: a
format version bump, a DST pass, and the pre-1.0 recreate policy. Do not reach
for it before the allocation work above, because it is the expensive kind of
change and the profile may show it is not where the time is.

### The raw-leaf scan borrows too (AHL-466)

The decoded-`Node` path was converted to borrow one shared page `Rc<[u8]>` per
page (AHL-455), but the **raw-leaf scan** — `CowBTree::walk_raw_row_values`,
the `RowScan` a join's outer side reads through — was the last path still
allocating per cell: `decode_leaf_cell_ref` materialised a fresh `Rc<[u8]>` per
cell (`ValueRef::Owned(Rc::from(...))`), and `resolve_value_at` re-cloned it.
The scan reads the page into a transient scratch buffer, so it had to copy.

AHL-466 folds it into the same pattern: the scan keeps the leaf behind one
`Rc<[u8]>`, the cells are `ValueRef::Inline` ranges into it, and each row's
value becomes a `RowBuf::Shared` by a refcount bump. One allocation per leaf
page instead of one per cell.

Measured interleaved on the join harness (`--suite joins --rows 20000`), the
PK-inner full shape — the one this file calls miss-bound — moved from ~19.6 ms
p50 and 1.37x slower than SQLite (journal) to ~14.1 ms p50 and parity (1.07x).
The row-at-a-time harness (`query_prepared_each`, unchanged) reports the outer
scan's allocations per projected row falling from ~1.7 to ~0.6.

---


## 3. The other write and scan paths

**Durable single-row write** is `fsync`-bound (~5 ms here). We already win by
doing one sync per commit where the journal does several. There is no
algorithmic win left; the remaining lever is not syncing more often than
required.

**Concurrent writers** read ~1.45x single-writer throughput at eight writers
when this paragraph was written, with 0% aborts on disjoint rows
(`rebase_pending` handles those). **Group commit** (Phase 2 item 5, AHL-461)
and the commit-gate rework behind it (AHL-468) lifted that to **2.8x** — 692
commits/s at eight writers against 246 at one, still 0% aborted. The two-writer
case stays flat (253 vs 246) because the follower's write usually lands after
the leader captured its flush target — still true today (259 vs 249,
2026-08-29), so this specific line has not gone stale the way several others
in this document had.

### Eight writers is not the ceiling — it is the peak, and past it throughput falls (AHL-497, 2026-08-29)

The "2.8x at eight writers" framing above answers "does adding writers help",
not "what happens if you keep adding them." It does not stay flat past eight
— it **falls**: a fresh sweep (`WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`, one
row per level, one machine, one session) reads 249 → 259 → 348 → 458 → 541 →
559 → **694** (peak) → 624 → 607 → 550 → 516 commits/s from 1 to 32 writers.
32 writers does *worse* than 8, not merely no better — a real, reproducible
regression (confirmed twice, `WRITER_LEVELS=8,32` alone: 693/493 and 586/499),
not noise, and **not mentioned anywhere in this document, `BENCHMARK.md`, or
`PLAN.md`'s W3 before now.** The control that rules out generic thread-count
overhead: SQLite's own row is flat across the identical sweep (85–93
commits/s at every level from 1 to 32) — same harness, same OS thread count,
same host — so whatever gets worse past 8 writers here is specific to this
engine's own concurrency mechanism, not just "more threads than cores."

**Root cause, read from the code before touching it.** `CommitCoordinator`
(`crates/inlaysql/src/device.rs`) has exactly one `reserved: Mutex<bool>` +
`Condvar` gate per file, shared by every writer regardless of
[`WAL_REGIONS`](../crates/inlaysql-core/src/wal.rs) (4). `CowBTree::commit`
holds that gate for its *entire* prepare phase — conflict/rebase check,
`finalize_free_list`, `encode_record_into`, `write_dirty_pages`, and the WAL
append write itself — and only releases it before the `fsync`. So the four
WAL regions parallelize exactly one thing: letting one writer's `fsync` (group
commit) overlap with the *next* writer's gate-held prepare phase. They do not
parallelize the prepare phase itself, which cannot be sharded by region
without also sharding the tree it commits into — and there is exactly one
copy-on-write tree, one root, shared by all four regions, so the "which root
did this transaction see, and what does its replacement look like" decision
is inherently one sequential stream regardless of how many regions exist to
hold the resulting bytes. Profiled directly (`sample`, 32 writers, the
harness's own release binary): **90.4% of samples are `__psynch_cvwait`** —
threads parked waiting for the gate — against a combined ~5% for the actual
encode/write/fsync work the gate protects, confirming the gate, not the
work, is where the time goes structurally. `decode_record_for_version`
appearing at all (~1%) in a pure-write benchmark is a second clue worth a
closer look later: some writers are missing the cached `commit_point` and
re-scanning their region from disk, on the hot path, under the gate.

**Tried, measured, and it did not move the number: spinning before parking.**
90% of samples being "parked waiting" looks like wasted park/wake overhead —
`std::sync::Mutex`+`Condvar`'s kernel round trip is expensive relative to a
critical section this short. So `begin_reservation` was changed to spin
(bounded `try_lock` polling, `core::hint::spin_loop()`) before falling back
to the existing blocking path, on the theory that most waits are shorter
than one park/wake round trip. Built, benchmarked (`WRITER_LEVELS=8,32`):
**no change** — 693/493 before, 768/499 and 586/499 after, inside the same
run-to-run noise band as the unmodified binary. Re-profiled to check whether
the spin was even taking effect rather than assume: `__psynch_cvwait` was
still 90.6% of samples, essentially identical to before. Raised the spin
budget 50x (100 → 5,000 iterations) as the obvious next question — still no
change (586/484). **Reverted; kept nothing from this attempt.** The
diagnosis this rules out: the cost is not park/wake overhead that a smarter
wait strategy can avoid. The diagnosis it leaves standing: with SQLite flat
across the same sweep as a control, and spinning-longer provably not
helping, the likely remaining explanations are (a) the gate-holding thread
itself getting preempted mid-critical-section under 32-way OS thread
oversubscription on an 18-core machine (6 P-cores), which no spin budget can
paper over since the wait is bounded by rescheduling latency, not by how
long the real work takes, or (b) `commit_point` cache misses (the
`decode_record_for_version` clue above) growing disproportionately more
frequent as more threads cycle through the same four regions, so the
*serialized work itself* — not just the wait for it — genuinely grows with
writer count. Neither was isolated further this round.

**The "shrink the gate, move writes after release" idea above was wrong, and
reading `rebase_pending` before writing it down would have caught that.**
`CowBTree::rebase_pending` (`crates/inlaysql-core/src/btree/tree.rs`,
~line 1822) detects conflicts by walking the tree from *the latest committed
root* — `self.get_at(self.root, key)? != self.get_at(current_root, key)?` —
for every key this transaction touched. `current_root` is whatever the
previous writer just published. That means the previous writer's dirty pages
must already be physically written and `pread`-visible *before* the next
writer's conflict check is safe to run. Deferring the encode/write past gate
release, as previously proposed, would let the next writer's `rebase_pending`
walk a root whose pages aren't landed yet — not a slow path, a correctness
hazard. The gate cannot shrink to "reservation only" the way this document
said it could; that proposal is retracted.

**Finer profiling shows the critical section was never the problem — the
contention was.** A second `sample` pass, read by full call-stack instead of
leaf symbol (leaf symbols lie here: `__psynch_cvwait` is the bottom frame for
*both* the reservation wait and the separate flush-follower wait, and only
the parent frames tell them apart), breaks `TreeStorage::commit`'s samples
down as: 2103/2404 (87.5%) parked in `begin_normal_commit`, waiting on
*another* writer's turn — pure contention, zero work done. Of the remaining
301, 165 (6.9% of the total) are a *second* wait — the group-commit
follower parked on the flush condvar inside `sync_commit`, not the gate.
That leaves only ~136 samples (~5.7%) as actual work: 64 running `fsync` as
flush leader, 37 as the `pwrite` of the WAL record itself, 12 in a second
small `Device::sync`, the rest scattered single digits. `rebase_pending`'s
own tree walk doesn't show up as a distinct bucket at all — it's fast.
So the gate-held critical section was already cheap; almost all the lost
time is 32 threads parking and waking on one mutex for a section that takes
next to no time once acquired — a lock convoy, not a slow critical section.
Shrinking an already-~2%-of-total critical section further has a low
ceiling even where it's safe to attempt, which per the paragraph above, the
encode/write portion isn't.

**Why the gate has to be global, and what a real fix would look like.**
There is one copy-on-write tree and one root; `rebase_pending` conflict-checks
against the latest committed root, so the "does this transaction still apply"
decision is inherently one sequential stream, not an accident of this
implementation. `WAL_REGIONS` parallelizes physical WAL-append layout and lets
one writer's `fsync` overlap the next writer's gate-held prepare — it was
never going to parallelize the prepare decision itself, and raising it still
won't. The only lever consistent with everything measured here is *logical*
group commit: let one writer, while holding the gate, absorb other waiting
writers' pending transactions into the same prepare/encode/WAL-append pass —
amortizing the fixed contention/wakeup cost over N transactions instead of
paying it once per transaction — mirroring the `fsync`-side group commit
this engine already does, just one layer up. That is a real redesign of the
commit protocol, not a tuning pass: it needs the same rigor every
catalog/storage-format change on this project gets — full DST sweep, not
just this suite — because a mistake here is a data-loss bug, not a slow
query. Scoped, not started, not attempted this session given the time
already spent correctly ruling out two smaller candidates first.

### The fixed 8-yield gather window was three orders of magnitude too short, and a `pwrite`-during-`fsync` penalty is why batching was stuck at ~2 (2026-08-30)

The section above named "logical group commit" — one gate holder absorbing
other writers' *transactions* into its own prepare/encode/append pass — as the
one lever left, and left it unstarted because it is a real commit-protocol
redesign. This is not that. It is the flush-side half of group commit that
already shipped (AHL-461/AHL-468): the leader that wins the reservation gate
still calls `coalesce_normal_commits` (`crates/inlaysql/src/device.rs`) to give
other writers a window to publish their post-write tickets before it captures
`target = writes_completed.load(..)` and calls `fsync`. That window existed
since AHL-461 and was never the suspect, because nobody had counted what it was
actually buying.

**Counted, with `INLAYSQL_COMMIT_STATS=1` printing `commit-stats: ...
normal_flushes=N normal_tickets=N` at exit.** The ratio
`normal_tickets / normal_flushes` is commits landed per `fsync` call — exactly
the number section 3's own "lifted that to 2.8x" line above and
`BENCHMARK.md`'s "8-writer scaling is 2.83x" line have both been implicitly
reporting all along without ever printing it directly. Instrumented
across the writer-count sweep, the ratio sat at **~2.0, essentially flat from 2
writers to 32**. A gather window that is doing its job should make that ratio
*rise* with writer count, the way `fsync` overlap already does one layer up.
It wasn't rising at all, which means the window was closing before a second
writer had any real chance to arrive — regardless of how many were waiting.

**Why: `pwrite` on this device gets ~18-23x slower while another handle's
`F_FULLFSYNC` is in flight on the same file.** Measured directly: an ordinary
`pwrite` costs ~30-40µs; the same call while a concurrent `F_FULLFSYNC` is
running against the same file costs ~600-800µs. A flush round holds the file
for its whole ~3.3ms `fsync`, and with several writers active there is nearly
always one running, so most commits' own writes land inside that penalized
window — inflating each commit's gate-hold time to ~1.2-1.9ms. That is exactly
what pins the ratio near 2: `ratio ≈ fsync_duration / gate_hold ≈
3300 / 1400 ≈ 2.3`, matching the measured ~2.0-2.8 without anyone having to
guess at a mechanism.

**And the gather window itself was closing about 200-250x too early to do
anything about it.** `yield_now` costs ~135-145ns; a competing writer needs
~30µs to get scheduled, pass the reservation gate and publish its ticket — a
~200-250x gap. The old `COMMIT_COALESCE_YIELDS = 8` spent a total of roughly
1.1-1.2µs yielding before declaring "no cohort arriving" and proceeding to
`fsync` — it had given up before a second thread had a realistic chance to
even be scheduled, which is why this window never gathered more than one or
two extra writers no matter how many were queued.

**The fix makes the window adaptive instead of fixed.**
`coalesce_normal_commits` now keeps yielding as long as a normal commit is
inflight or waiting *and* `writes_completed` keeps advancing, and only gives up
after `COMMIT_COALESCE_STALL_YIELDS = 1500` consecutive yields with no new
ticket observed (a hard ceiling, `COMMIT_COALESCE_MAX_YIELDS = 16384`, bounds
the worst case regardless). It costs nothing on the solo path — the emptiness
check fires before the first yield, exactly as before — and it works precisely
because *no `fsync` is in flight yet* during this pre-flush gather window: a
writer that arrives and publishes its ticket here pays the fast ~30-40µs rate,
not the penalized one, and its ticket is folded into the one upcoming `fsync`
instead of needing a flush of its own. Durability ordering is untouched: this
only ever delays the moment the flush target is captured, strictly before that
capture, so it can only grow the set of tickets one `fsync` covers, never
shrink it or acknowledge a ticket before its bytes are written — see the
function's own doc comment for the argument in full.

**Measured, three interleaved A/B pairs, 0.0% conflicts throughout:**

| Writers | Baseline commits/s | Patched commits/s | Ratio (tickets/flush) before → after |
| --- | --- | --- | --- |
| 1 (solo) | 246 / 247 / 296 | 246 / 244 / 246 | n/a — solo path takes zero yields either way |
| 8 | 629 / 677 / 778 | 1208 / 1194 / 1223 | 2.64-2.77 → 6.09-6.31 |
| 32 | 488 / 477 / 579 | 956 / 1009 / 986 | 1.84-1.95 → 4.76-5.07 |

Solo does not regress, and 8/32-writer throughput both roughly double with the
ratio moving the same direction, which is the causal chain this section
claims, not just a correlated number.

**The published sweep, regenerated on this machine (median of three runs each,
load 3.2-4.1/18 throughout — see `bench/results/20260830T03{13,28,29}00Z.txt`
for the published 1/2/4/8 sweep and `20260830T03{15,21,36}00Z.txt` for the wide
one):**

| Writers | InlaySQL commits/s | SQLite commits/s | vs SQLite |
| --- | --- | --- | --- |
| 1 | 246 | 90 | 2.73x |
| 2 | 394 | 91 | 4.33x |
| 4 | 615 | 91 | 6.76x |
| 8 | 1184 | 91 | 13.01x |

**A later regeneration (2026-08-30, `2cb2539`) found 244/304/587/1209 and
13.7x at 8 writers — no code changed the commit-coalesce path between the two
sessions, so read the difference as this benchmark's usual run-to-run
spread, not a further improvement.** `BENCHMARK.md` has the current table;
this section's own causal argument (the adaptive gather window, measured
against a reverted fixed-window baseline below) is unaffected either way.

**This also moves the shape this document has published since AHL-497, and
that section's own framing — "eight writers is the peak" — is now stale and is
corrected here rather than silently.** The wide sweep
(`WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`, same session, median of three):
247 → 357 → 474 → 630 → 796 → 978 → **1195** → **1519** → **1597** → 1325 →
988 commits/s from 1 to 32 writers, against SQLite's own flat 89-92 across the
same sweep (same control as AHL-497 used: not generic thread-count overhead).
The peak is no longer at 8 — it is a plateau across 12-16 writers (1519 and
1597, close enough across three runs each that which one nominally wins swaps
run to run), and the falloff past it is real and reproduces across all three
runs: 16 → 24 is a 17% drop, 24 → 32 a further 25%. So the AHL-497 finding
that throughput falls past a peak is **still true** — the peak just moved from
8 to roughly 12-16, and every point on the curve, including the declining
tail, is now well above where the old curve's peak used to be: 32 writers now
does 988-1088 commits/s against SQLite's ~90, 11-12x, where the old published
32-writer number was 516 commits/s, ~6x. See `BENCHMARK.md`'s Concurrent
writers section for the full regenerated table and prose.

**Why this session went looking at the flush side at all: two earlier,
correctly-executed attempts at the wait side had already come back as no-ops.**
Spinning before parking on the reservation gate (documented above, this
section) changed nothing at 8 or 32 writers across a 50x range of spin
budgets, ruling out park/wake overhead as the cost. Gate admission-capping —
the explicit normal-commit-ready ticket prototype (`962558d`, publish the
ticket before releasing the gate, PLAN.md's W3 baseline) — also measured flat,
508 commits/s at 32 writers against the pre-existing baseline, no stable
improvement across setup-separated and focused reruns. Both were real
experiments on the *wait* side of the gate, both were measured honestly, and
both correctly reported no effect — which is exactly why the next pass moved
to instrumenting the *flush* side directly with `INLAYSQL_COMMIT_STATS`
instead of proposing a third variation on the same idea.

**Two follow-ups this change surfaces, neither fixed here:**

1. A checkpoint holds the reservation gate while parked as a flush follower,
   which makes `normal_waiters` look like a live cohort that can never
   actually publish a ticket — from the coalescing leader's point of view,
   indistinguishable from a writer that is merely slow. The stall detector
   (`COMMIT_COALESCE_STALL_YIELDS`) breaks out of that reliably, in ~200µs
   worst case, so this is a bounded latency tax on the next normal flush, not
   a deadlock — but no test drives a checkpoint and a concurrent normal commit
   through this path at the same time to pin that bound directly;
   `normal_commits_and_checkpoints_use_separate_flush_paths` (`device.rs`)
   runs them sequentially, not concurrently.
2. `normal_inflight`/`normal_waiters` have no RAII scope guard the way the
   flush leader's own state does (`LeaderGuard`). A panic between
   `begin_normal_commit` and `end_normal_commit` already leaked one of these
   counters permanently before this change; it is a pre-existing gap, not
   introduced here. What this change does do is raise the price of that
   pre-existing bug: a leaked counter used to cost roughly one spurious
   `~1µs`-scale coalesce attempt per future flush, and now costs up to the
   full `COMMIT_COALESCE_MAX_YIELDS` ceiling, ~2.3ms, every time a future
   leader coalesces against a cohort that can never actually shrink.

**And one thing this change does not measure at all.** The concurrency
benchmark reports throughput and conflict rate, nothing else — there is no
latency-percentile output, so whether a wider gather window moves the tail
(some writers now wait up to ~2.3ms longer for their ticket to be gathered
before a slow leader gives up on them) is genuinely unknown. Widening
`COMMIT_COALESCE_MAX_YIELDS` for a throughput win without any visibility into
p99 commit latency is a real gap in what this document can honestly claim.

**All three of the above paid (2026-08-30).**

1. `a_checkpoint_concurrent_with_a_normal_commit_still_makes_progress`
   (`device.rs`, beside `normal_commits_and_checkpoints_use_separate_flush_paths`)
   now drives the exact interleaving instead of leaving it unproven. A fake
   flush leader — a real `CommitCoordinator::make_durable_with_cohort` call
   this test controls, the same technique the leader/follower tests above it
   already use — blocks mid-flush; a real checkpoint takes the reservation
   gate, and its own `sync()` must see the fake leader in progress and become
   a follower *without releasing the gate*; six real normal commits, each its
   own handle, are then started and confirmed (a bounded poll, not assumed)
   to have piled into `normal_waiters` behind that held gate; only then is
   the fake leader released. Every commit and the checkpoint succeed, every
   committed row survives a fresh handle, and `normal_inflight`,
   `normal_waiters` and `reserved` are all back at rest afterward.
   Deterministic — the only timing assertion is a deliberately loose overall
   ceiling (30s) meant to fail loudly on a genuine hang, not to bound the
   ~200µs stall itself.
2. `normal_inflight`/`normal_waiters` now have an RAII guard
   (`NormalCommitGuard`, `crates/inlaysql/src/device.rs`), covering the same
   kind of span `LeaderGuard` already covers for the flush leader's own
   state. Because the code that can actually panic between
   `begin_normal_commit` and `end_normal_commit` runs in `inlaysql-core`'s
   `CowBTree::commit`, on the other side of the `Device` trait, the guard is
   stashed in a `FileDevice` field rather than a local — reachable, and still
   dropped, when a panic there unwinds this handle's owning thread, which
   (nothing in this workspace catches such a panic and keeps a `FileDevice`
   alive past it) is already how such a thread ends. Dropping it releases
   the *entire* unfinished reservation, not just the counter: leaving
   `reserved` stuck at `true` would have deadlocked every later committer on
   the file outright, not merely taxed them the ~2.3ms this section
   describes.
   `a_panic_between_begin_and_end_normal_commit_does_not_leak_the_inflight_counter`
   proves it — a real `FileDevice` moved into a `catch_unwind`ing closure
   that begins a normal commit and panics before `end_normal_commit` ever
   runs, with the coordinator's counters and reservation gate both read back
   at rest afterward.
3. The concurrency suite now reports p50/p95/p99/max per commit, per writer
   level, alongside throughput and conflict rate
   (`crates/inlaysql-bench/src/concurrency.rs`), reusing the crate-root
   `percentiles` helper — extended to add p99 everywhere it is used —
   `indexed.rs`'s `report` already reused rather than reinventing. Measured
   per commit, not per attempt: a conflicted attempt's own duration is not
   counted, since the conflict rate already prices retries. `WRITER_LEVELS=1,8,32`,
   three runs, this host (range across the three; commits/s reproduces the
   ~246 / ~1,248-1,378 / ~994-1,000 baseline within its own noise at 1 and 32
   writers, and a little under it at 8 — see the gate report for this run):

   | Writers | commits/s | p50 | p95 | p99 | max |
   | --- | --- | --- | --- | --- | --- |
   | 1 | 244–266 | 3.85–4.09ms | 4.28–4.48ms | 5.95–7.95ms | 8.05–9.05ms |
   | 8 | 1,132–1,246 | 4.84–5.14ms | 18.88–23.00ms | 34.31–40.86ms | 50.91–63.14ms |
   | 32 | 976–1,003 | 20.70–23.94ms | 92.97–96.78ms | 114.89–125.27ms | 141.32–186.02ms |

   **The 2x throughput win did come at a real tail-latency price, and the
   price grows with writer count rather than staying flat.** p99 at 8 writers
   is roughly 4-7x p99 at 1 writer; at 32 writers roughly 14-21x. SQLite's own
   p99 stays roughly flat (~13-17ms) across the same sweep, because its
   writers serialize on a lock rather than gathering — so at 32 writers
   InlaySQL's throughput is ~10-11x SQLite's, but its p99 is now ~7-9x
   *worse* than SQLite's, a genuine trade the throughput number alone never
   showed and this document could not previously state. This run has no
   concurrent checkpoint, so item 1's fix is not itself a contributor to the
   tail measured here — what remains is the gather window's own cost: more
   writers means both a bigger cohort to gather before the leader's `fsync`
   and a longer queue behind the reservation gate, and both grow the tail
   directly, not just the median.

**Follow-up (2026-08-30): the "genuinely unknown" question above is now
answered, and "the 2x throughput win did come at a real tail-latency price"
overstates the causal link — the A/B says the adaptive window is not what the
tail price was paid for.** Three interleaved A/B pairs (old: temporarily
`COMMIT_COALESCE_MAX_YIELDS = 8`, provably identical to the pre-`94d96a6`
fixed loop since `COMMIT_COALESCE_STALL_YIELDS` — 1,500 — can never be
reached within 8 iterations; new: HEAD, unmodified), same
`WRITER_LEVELS=1,8,32`, median of three runs each:

| Writers | old p99 (pre-`94d96a6`) | new p99 (adaptive) | old commits/s | new commits/s |
| --- | --- | --- | --- | --- |
| 1 | 7.94 ms | 7.88 ms | 247 | 250 |
| 8 | 45.01 ms | **34.90 ms** | 576 | **1142** |
| 32 | 150.89 ms | **122.13 ms** | 504 | **968** |

The adaptive window **lowers p99 by 19-23% and roughly doubles throughput at
both 8 and 32 writers**, consistently across all three pairs — it is strictly
better on both axes than the fixed 8-yield window it replaced, not a
throughput-for-tail trade. The real trade this section's table above
correctly identifies — ~10-11x SQLite's throughput against ~7-9x worse p99 —
is a structural cost of gathering commits behind fewer `fsync`s under
contention, and it predates `94d96a6`: the old fixed window paid it too, at
150.89ms p99 for barely half the throughput. See `BENCHMARK.md`'s "Concurrent
writers: the tail the commits/s table hides" for the full published numbers.
`COMMIT_COALESCE_MAX_YIELDS`/`COMMIT_COALESCE_STALL_YIELDS` are unchanged by
this finding — there is no p99 regression here to trade away.

### Deferred/checkpointed page durability, measured before being built — Phase 0, and the redesign does not pay (2026-08-30)

`PLAN.md`'s §6 named deferred/checkpointed page durability — defer B-tree page
writes behind an async checkpoint so a commit's `fsync` only covers a small WAL
tail, the way InnoDB/PostgreSQL's redo log works — as the largest remaining
piece of work and the only way left to close the single-connection sequential
durable-write loss, once commit-side group commit (still unbuilt) is also
done. Before building it, Phase 0 measured whether it would actually help.
**It does not, on this platform, and the redesign should not be built.**
`F_FULLFSYNC` here is a fixed barrier almost independent of the bytes queued
behind it, and a real commit dirties too few pages for byte count to matter
even where the barrier did scale.

**Cross-reference (2026-08-31): this section's own "what stays true in
principle" caveat below was re-tested, not just repeated, in the Linux
container this project's benchmarks actually run in** — see "The
deferred-durability rejection, re-tested in-container" further down §3. That
section found the same flat, floor-dominated shape there too (a lower
absolute floor, ~1.0-1.2ms against this section's ~2.7-2.9ms, but the same
non-scaling curve), so this section's verdict is now confirmed on both
platforms this project measures on, not merely the host. The conclusion below
remains correctly scoped to `F_FULLFSYNC` on this Mac's APFS SSD *as
written*, but read it together with the in-container section: the
platform-scoping caveat this section itself raised (a byte-proportional
barrier "would be worth revisiting there") did not pan out on the other
platform tried either. A PLP-protected NVMe under bare-metal Linux — the
platform that caveat actually named — remains untested by either section.

**Finding 1 — `F_FULLFSYNC` is a fixed barrier, not a bytes-proportional
cost.** Standalone probe, 4096B pages, N ∈ {0,1,2,4,8,16,32,64,128,256} pages,
round-robin interleaved order, 95 timed reps per N after 5 warm-up reps, two
independent full runs, timing only the `sync_all` call:

| N (pages) | Bytes | p50 |
| --- | --- | --- |
| 1 | 4 KiB | 3,085–3,119 µs |
| 8 | 32 KiB | 3,085–3,141 µs |
| 64 | 256 KiB | 3,735–3,740 µs |
| 256 | 1 MiB | 3,658–3,754 µs |

Floor across every N: flat ~2.7–2.9 ms. A 256x increase in bytes queued moves
the median by only ~20% — inside this project's own stated ±10% run-to-run
noise band doubled, not a scaling curve. The N=0 (nothing dirty) case is
bimodal: 30–60% of calls return in ~5–15µs, the rest pay the same ~2.7–4ms,
apparently when the device's write cache had unrelated pending work — itself
evidence the barrier's cost is device-state-dependent, not byte-count-dependent.

**Finding 2 — a real commit dirties far too few pages for bytes to ever
matter, even on a platform where they did.** Instrumented `write_dirty_pages`:
single-row commits (200 samples) median **5 dirty pages (20,480 B)**, p95 5,
max 6 — consistent with AHL-496's 6.45-page count above, one B-tree level
shallower. The `points --rows 2000` suite (2,003 commits) also medians 5
pages, maxing at 60 pages (245,760 B) on structural B-tree reorganisation —
and 60 pages is still inside the flat region of Finding 1's curve.

**Finding 3 — fsync already dominates commit wall-clock, matching AHL-480/496
above.** Per-commit phase split, concurrency suite, n=201, µs:

| Phase | p50 | p95 | max |
| --- | --- | --- | --- |
| prepare (rebase/free-list/encode) | 63.1 | 88.4 | 4,132 |
| `write_dirty_pages` | 39.8 | 59.3 | 70.5 |
| WAL append | 8.2 | 10.2 | 233.3 |
| `fsync` (`F_FULLFSYNC`) | 3,727.5 | 3,998.7 | 4,269.1 |

`fsync` is **97.1%** of commit time. Removing `write_dirty_pages` from the
critical path entirely — which is the whole mechanism the redesign buys —
would save ~40µs of ~3.84ms, about **1%**.

**Finding 4 — the relaxed-durability ceiling is where the actual prize is.**
Single-writer commits/s, two reps each, barrier temporarily swapped (change
reverted, never shipped):

| Barrier | commits/s | vs today |
| --- | --- | --- |
| `F_FULLFSYNC` (today) | 246–247 | 1x |
| plain `fsync(2)` (weaker; Apple's own docs say it does not guarantee a media flush) | 7,839–7,894 | **32x** |
| no barrier (pure upper bound, never shippable) | 14,342–14,684 | 60x |

So the deferred-page-durability redesign targets the ~1% of commit time that
is not already `fsync`, and the already-named-but-unbuilt relaxed-durability
tier (§3 "Re-opened", R11 in `PLAN.md`) targets the 97% and is worth up to 32x
on this host path.

**The containerised caveat, stated plainly rather than glossed over.** All
four findings above are host measurements, where `fsync` really is 97% of
commit time on this Mac's internal SSD. `BENCHMARK.md`'s published loss to
MySQL (1.39x) and PostgreSQL (1.90x) is a *containerised* comparison, and this
file's own AHL-496 section already found containerised InlaySQL measuring
849.7 ops/s against ~253 on the host at the same shape — Docker's virtual disk
is already handing out a much weaker barrier there than `F_FULLFSYNC` gives on
the host. So in the comparison `BENCHMARK.md` actually publishes, `fsync` is
*not* 97% of commit time, and this Phase 0 result does not say a
relaxed-durability tier would close that specific 1.39x/1.90x gap. Whether it
would is a separate, currently unprofiled question — nobody has run
AHL-496-style phase-split instrumentation inside the container, and until that
exists the 32x above must not be quoted against the published containerised
numbers. Conflating the two would be exactly the kind of methodology error §6
of this file exists to prevent.

**What stays true in principle.** The redesign itself is not wrong as an
idea — it is wrong *for this platform's barrier semantics*. On a platform
where `fsync`/`fdatasync` cost genuinely scales with bytes written (a
PLP-protected NVMe under Linux, which R1 was scoped to go measure), deferring
page writes behind a checkpoint would shrink the barrier's own cost, not just
the ~1% of commit time sitting outside it, and would be worth revisiting
there. The verdict above is scoped to `F_FULLFSYNC` on this Mac's internal
APFS SSD, not to the architecture. **Update (2026-08-31):** the Docker
container this project's own benchmarks run in (further down §3, "The
deferred-durability rejection, re-tested in-container") is *not* that
platform either — same flat, non-scaling shape, just a lower floor. R1's
actual target, a PLP-protected NVMe under bare-metal Linux, is still
unmeasured by either section.

**Scans, joins and aggregates are the untested embarrassment**, and AHL-462 and
AHL-464 made them less embarrassing without making them measured. Joins are
still nested-loop in written order, and there is still no join reordering — but
the outer side streams, aggregation no longer copies every row a third time,
and since AHL-464 the *inner* side is a probe whenever the `ON` justifies one:
the rows one outer key can match, by a tree descent for an `INTEGER PRIMARY
KEY` or an index entry range for a scalar B-tree index. Only a join the rule
declines materialises its inner side, and then once per join rather than
re-cloned per outer row. A `LIMIT` on an unsorted plan ends the scan instead of
truncating the answer: `SELECT ... LIMIT 5` over a 2,000-row table reads 32
rows, and a probed join under a `LIMIT 2` fetches two inner rows out of 2,000 —
both counted deterministically in `crates/inlaysql-core/tests/streaming.rs`.

**The join and scan rows exist now** (Phase 2 item 7, AHL-470: `SUITE=joins`
and `SUITE=indexed`), and they publish losses — 5.56x on a full PK inner join
and 10.71x on the secondary-index shape, against journal-mode SQLite. That is
what the profile in "The join and range profile" above is chasing. **A hash
join still does not exist**, so a join workload whose `ON`
the rule declines — anything but an equality on a key or an indexed column —
is still O(n×m) and would still lose to MySQL or PostgreSQL, and deserve to.

**Secondary indexes (Phase 2 item 3) landed (AHL-423), and they are now a
stage of the streaming pipeline rather than a path beside it.** `WHERE email =
?` is an index range probe, not a full scan that decodes every row — which is
the query any framework-generated workload is dominated by. The probe reads its
run of index *entries* up front, because entries sort by value and have to be
put back into row-id order, and then feeds the row ids into the same pipeline a
scan feeds: rows are fetched one at a time, so a `LIMIT` over an indexed filter
stops fetching as soon as it has enough (`RowBytes::Indexed` in
`crates/inlaysql-core/src/exec.rs`; counted in
`crates/inlaysql-core/tests/btree_index.rs`).

The same probe is now a join's inner side as well (Phase 2 item 4, AHL-464), so
`SELECT ... FROM users JOIN posts ON posts.user_id = users.id` reads the posts
one user has rather than the posts table.

Still open on this path: the index cannot yet satisfy an `ORDER BY` — entries
are in key order, so an ordered range scan would remove the sort as well as the
scan, but the pipeline collects before `ORDER BY` today — and a join probe is
not cached across outer rows that repeat a key, so a many-to-many join re-reads
an entry range it has already read.

### Opt-in relaxed-durability tier, shipped (2026-08-30)

Phase 0 above (previous section) measured the barrier a normal commit's
`sync_commit` waits on — `F_FULLFSYNC` on this host — at 97.1% of commit
wall-clock, and found that swapping it (temporarily, reverted, never
shipped) for plain `fsync(2)` measured 32x single-writer throughput. This is
that swap, shipped as an actual opt-in: `EngineOptions::durability`
(`Durability::Full`, the unchanged default, or `Durability::Normal`), scoped
to `Device::sync_commit` only — `Device::sync` (checkpoints, the state
block) is never weakened at any level, so a relaxed file's checkpoint/wrap
truncation cannot roll back further than the level's own documented loss
bound. See `docs/recovery.md`'s "Durability levels" section for the exact
guarantees, the per-platform mapping, and the multi-writer coupling.

**Single-writer commits/s, real end-to-end `INSERT` transactions through the
concurrency suite (`inlaysql-bench --suite concurrency --writers 1 --txns
3000`), 5 repeated runs each, this host:**

| Level | commits/s (5 runs) | median | vs `Full` |
| --- | --- | --- | --- |
| `Durability::Full` (default) | 246, 248, 268, 270, 271 | 268 | 1x |
| `Durability::Normal` | 4,229, 4,299, 4,332, 4,377, 4,441 | 4,332 | **16.2x** |

Tighter and more reproducible than it first looked: an earlier pass at 200
and 2,000 transactions per run showed `Normal` swinging 1,000-3,600
commits/s run to run — a small-sample artefact this shared, loaded machine
exaggerates, exactly the ±10% (here, worse) run-to-run noise this file's own
measurement rules warn about. 3,000 transactions per run was enough for
`Full` to sit in its already-published 246-271 range and for `Normal` to
settle into a tight ~4,229-4,441 band (±2.4% of its median) across all 5
runs. Multi-writer throughput at the default level was re-measured
alongside this change (`WRITER_LEVELS=1,8,32`, 3 runs) to confirm no
regression: 246-249 / 1,248-1,378 / 994-1,000 commits/s at 1/8/32 writers,
0.0% conflicts throughout — the same shape as the committed baseline
(~246/1,184/988), since the default `Full` path's code is untouched by this
change (`FileDevice::sync`, which every existing call still reaches, is
identical to before this change; only `FileDevice::sync_commit`'s barrier
choice is new).

**16.2x, not the Phase 0 probe's 32x, and that gap is expected, not a
discrepancy to chase.** The Phase 0 probe timed the bare `sync_all()` call in
isolation; this measurement times a whole SQL `INSERT` transaction — parse,
plan, execute, the tree's copy-on-write page walk, WAL encode, and the
group-commit coordinator's own bookkeeping, none of which shrink when the
barrier does. At `Full` those costs are a rounding error next to a
~3.3-4ms `F_FULLFSYNC`; at `Normal` the barrier drops to tens of
microseconds and those other costs become the new floor, so the measured
speedup asymptotes below the pure-barrier ratio — precisely the caveat
Phase 0's own Finding 3 already put a number on (`fsync` at 97.1%, meaning
~2.9% of commit time was never going to shrink with the barrier). 16.2x of
the available headroom landing is a good outcome, not a shortfall.

Not published in `BENCHMARK.md`'s comparison tables — those are
full-durability-only on every side of every comparison, on purpose (see
`BENCHMARK.md`'s "Durable writes" section for the one-line pointer back
here).

### The containerised comparison, profiled instead of trusted — the predicted cause was wrong (2026-08-30)

AHL-496's "what is owed" list, above, named two things: an interleaved,
repeated, quiet-machine rerun of the containerised MySQL/PostgreSQL
comparison, and — if the write path were picked up again — the 6.5 pages.
Neither of those is what this section did. Instead it tested a specific,
falsifiable prediction this document had been implicitly making since
AHL-480/AHL-496: that in-container, where the barrier is weaker than the
host's `F_FULLFSYNC`, our own non-`fsync` commit cost would be a much larger
share of the total than the host's ~11% — large enough to be a real,
engine-side cause of the published MySQL/PostgreSQL loss. **It is not. The
prediction was wrong, and it is worth saying plainly rather than quietly
dropping it.**

**Measured, not assumed.** A temporary `TimingDevice` shim wrapping
`FileDevice` — built to answer this question, reverted after use, never
shipped — split a containerised commit into `prepare` / `write` (data + WAL)
/ `fsync`, on the same Docker named volume the published table's
containerised row runs on. Two runs:

| Phase | Run 1 p50 | Run 2 p50 |
| --- | --- | --- |
| prepare | 119.5 µs (8.6%) | 147.7 µs (9.5%) |
| write (data+WAL) | 17.0 µs (1.2%) | 22.5 µs (1.4%) |
| fsync | 1,239.1 µs (**89.1%**) | 1,371.8 µs (**87.8%**) |
| total | 1,391.7 µs | 1,561.6 µs |

Same barrier-dominated shape as the host's 97.1% (this section's Phase 0
above), just a smaller absolute barrier: `fsync` is 88-89% of a
containerised commit too, not the ~50-60% a weaker barrier could plausibly
have left room for. InlaySQL's own non-`fsync` work is ~11-12% of commit
time here — barely moved from the host's ~9%. So even a hypothetical
zero-cost commit path caps the achievable win over today's containerised
number at roughly 1.15x. That is nowhere near the 1.39-1.90x gap
`BENCHMARK.md` publishes against MySQL and PostgreSQL. **There is no
engine-side fix for this workload's gap** — not the checksum, not the page
count, not the encode path, all already measured and closed above. The gap
lives in the volume and the transport, not in this engine's code.

**The transport half, quantified.** `BENCHMARK.md`'s containerised InlaySQL
row is a library call — `bench/external/compose.yml`'s `inlaysql-oltp`
service runs `cargo run -p inlaysql-bench -- --oltp-replay`, in-process, no
socket — while `mysql_driver.py` and `postgres_oltp_driver.py` reach their
servers with `mysql.connector`/`psycopg` over the compose bridge network, a
socket round trip on every statement. That asymmetry favours InlaySQL, and
it is large enough to matter: `inlaysql serve --mysql` at one connection (the
Server-to-server table) writes at 1,795.6 µs/commit over the identical
protocol MySQL pays, against the containerised library row's 1,177.0 µs
(published) / 1,369.3 µs (a same-session rerun today) — **~420-620 µs of
transport/driver tax that InlaySQL's published row skips and both MySQL and
PostgreSQL pay on every statement**, the same order of magnitude as the
entire published PostgreSQL gap. A transport-matched comparison would very
likely reverse part of that gap, not just narrow it.

**And the comparison is not reproducible run to run, which AHL-496 already
warned about and this confirms directly.** A fresh, same-session rerun of
`bench/compare.sh`'s own OLTP drivers today (`ROWS=3000 LOOKUPS=1000`, host
load ~6.2/18 — disclosed, not quiet): InlaySQL host 240.9 ops/s, InlaySQL
containerised 730.4, MySQL 931.2 (**1.27x**), PostgreSQL 805.0 (**1.10x**) —
against the published 849.7 / 1,184.2 (1.39x) / 1,612.8 (1.90x).
**PostgreSQL is now slower than MySQL, where the published table has it
leading**, and both multiples shrank by about a third in one sequential
rerun. Root cause, measured directly rather than inferred: the Docker named
volume's own `fsync` cost drifted 1.5-1.8x within the same session — roughly
1,150 µs before the MySQL/PostgreSQL containers were up, 640-800 µs ten
minutes later with them running. This is AHL-496's own 2.1x/90-minute drift
finding, reproducing at a shorter timescale and inside a single benchmark
run rather than across two.

**What this settles, and what it does not.** It settles that the allocation
story (AHL-488/493) and this section's own checksum/page-count findings are
not where the containerised MySQL/PostgreSQL gap lives — that hypothesis is
now tested and rejected, not merely unconfirmed. It does not settle what a
fair, transport-matched, quiet, repeated comparison would show — only that
the published sequential single-run table should not be read as one.
AHL-496's "what is owed" item 1 — re-run interleaved, repeated, on a quiet
machine — was the *only* item on that list at the time, and it is now
**paid** (2026-08-30, same day): see the "What is owed" list above for the
summary and `BENCHMARK.md`'s "Interleaved, repeated, quiet-machine rerun"
section for the full table. Short version: 5 repetitions found the
MySQL/PostgreSQL ordering stable (PostgreSQL ahead, 5/5 — this section's own
sequential flip did not reproduce under interleaving) and the median
multiple close to the published one (1.81x/1.43x against 1.90x/1.39x), so
the sequential rerun above was the noisy measurement, not the published
table. There is no commit-path profiling still outstanding, in host or in
container. One new methodology item has since joined the list, and it is
not yet paid: the single transport-matched run taken alongside this rerun
(InlaySQL 627.6 ops/s against MySQL's 849.4 at one connection) did not
confirm the transport-asymmetry prediction above — the matched-transport
gap came out smaller, not larger, than the containerised one — so that
prediction's direction is open, not settled, until it gets the same
interleaved-repeat treatment item 1 just got. See the "What is owed" list's
new item 2.

Already winning where it is measured: ~15.8x over `sqlite-vec` at 100k vectors
(7.56x on the 2,000-vector suite `BENCHMARK.md` publishes), and ~60x over
DuckDB and ~74x over pgvector on hybrid, because hybrid is one statement here
and two queries plus client-side fusion there.

**The BM25 leg is no longer the expensive half.** It was 79% of the hybrid p50
(347.50 µs of 453.88 µs at 2,000 documents); an inverted-index layout with
dense document ordinals, a bounded top-`k` heap and a MaxScore walk took it to
47.75 µs of a 95.17 µs hybrid — 50%, with the vector leg now the larger share.
Scores and ranking are unchanged, ties included.

### The deferred-durability rejection, re-tested in-container: still flat, still rejected (2026-08-31)

`SCOREBOARD.md`/`BENCHMARK.md`'s server-to-server sweep (2026-08-31) found
InlaySQL losing to MySQL 8 by **4.7x at 16 connections** (1,308.1 vs 6,120.7
ops/s), with batching efficiency roughly comparable (InlaySQL's in-process
proxy ~4.76-6.31x, MySQL's measured 7.42x) but implied `fsync` *rate* not:
~238 fsyncs/s for InlaySQL against ~825/s for MySQL, on the same volume, in
the same container. The candidate explanation was that InnoDB's commit
`fsync` flushes a small sequential redo-log tail while InlaySQL's flushes
~5 dirty B-tree pages plus a WAL record — and that this would only matter on
a platform where `fsync` cost scales with bytes, which the Phase 0 section
above explicitly was not (macOS `F_FULLFSYNC`, `~2.7-2.9ms` flat regardless of
bytes queued). This section is that platform-scoped question, asked properly:
does `fsync` scale with bytes **in the Linux container this comparison
actually runs in**?

**Method, mirroring Phase 0's Finding 1 exactly, moved into the container.**
A standalone probe (not part of the workspace; written, compiled with the
container's own `rustc`, run, and discarded) writes N pages (4096 B — this
engine's `DEFAULT_PAGE_SIZE`, same as Phase 0's host probe) to a fixed offset
in an already-sized file, then calls `File::sync_all()`, timing only the
sync. `sync_all()` is what `FileDevice`'s `Durability::Full` path actually
calls on every platform — on Linux this resolves to plain `fsync(2)`, not
`F_FULLFSYNC`. N swept over {0,1,2,4,8,16,32,64,128,256} pages, 5 warm-up
rounds discarded, 55 timed rounds kept (≥ the 50-rep floor). Run on
`bench/external/compose.yml`'s own named volume for `inlaysql-server`
(`inlaysql-bench_inlaysql-server-data`, the exact volume the 4.7x loss above
was measured on), inside the same `docker/Dockerfile` image
(`inlaysql-bench-inlaysql-server:latest`), reached with a bare `docker run`
rather than `docker compose` (no need to bring up MySQL/PostgreSQL/etc. for a
single-file probe). Docker backend here is OrbStack, not Docker Desktop
(`docker version` → context `orbstack`); the volume is backed by a `btrfs`
filesystem on a virtio block device inside its Linux VM (`df -T /data` →
`/dev/vdb1 btrfs`). Machine load checked before every run: 1-minute average
3.4-4.6 of this 18-CPU box's 4.5 quiet-machine ceiling throughout (busy
interactive desktop, disclosed rather than forced past).

**First pass manufactured a spurious slope, and it is worth showing rather
than quietly fixing.** An initial design swept N in fixed ascending order
every round (0,1,2,4,...,256, repeat) — "round-robin", but not shuffled.
Averaged over 3 runs, this measured medians climbing from 1,119.4 µs at N=1
to 1,480.3 µs at N=256: a 32% increase, R²=0.91 against bytes. That looked
exactly like the hypothesis predicted. It was an artefact: N=256 is always
the *last* fsync of every round in a fixed-ascending sweep, so any drift over
the course of a round — write-buffer or journal pressure accumulating,
background container activity — lands disproportionately on the largest N
regardless of whether bytes have anything to do with it. Position-in-round
and byte-count were perfectly confounded by construction.

**Corrected: N shuffled independently every round** (Fisher-Yates over a
tiny dependency-free xorshift64* PRNG), so any temporal drift is spread
evenly across every N instead of concentrating on whichever one is swept
last. Three independent runs, averaged medians (µs), and the range each
individual run's median fell in:

| N (pages) | Bytes | p50 range across 3 runs | p95 range across 3 runs |
| --- | --- | --- | --- |
| 0 | 0 | 854.0–1,123.8 | 1,897.5–2,270.1 |
| 1 | 4 KiB | 979.1–1,120.3 | 1,595.5–2,000.2 |
| 8 | 32 KiB | 1,028.1–1,164.2 | 1,756.8–1,908.1 |
| 64 | 256 KiB | 1,043.4–1,156.0 | 1,571.7–1,848.9 |
| 256 | 1 MiB | 1,036.7–1,155.3 | 1,535.8–2,120.2 |

Averaged-median regression across all 9 non-zero N: slope ≈ **-0.007 µs/KB**
(sign flips run to run — indistinguishable from zero), **R² = 0.017** (was
0.91 with the confound), ratio of N=256's median to N=1's ranged 1.01-1.06x
across the 3 runs (average 1.03x) — inside this machine's own disclosed
noise floor for a busy desktop, nowhere near a scaling curve. The N=0 case,
which was bimodal on the host (sometimes ~10µs, sometimes the full barrier),
is *not* bimodal here once shuffled: it costs the same ~0.9-1.1ms as every
non-zero N, evidence the floor is the barrier/device round trip itself, not
anything proportional to what is queued behind it — a cleaner version of
Phase 0's own host conclusion, not a different one.

**Verdict: FLAT, not sloped — said loudly, because the hypothesis this
session set out to test was that it would slope.** The curve is the same
shape as the macOS host's (Phase 0 Finding 1): a fixed floor, ~1.0-1.2ms
here against ~2.7-2.9ms on the host — lower in absolute terms (matching
`BENCHMARK.md`'s already-published finding that this container's barrier is
weaker than `F_FULLFSYNC`), but not differently shaped. `fsync`
cost inside this container does not scale with dirty bytes over 0B-1MiB, a
range that comfortably spans both InlaySQL's own per-commit dirty-byte count
(below) and any plausible InnoDB redo-log-tail size.

**Task 2: what InlaySQL actually writes per commit, confirmed in-container.**
Temporary instrumentation (a small atomic histogram in
`CowBTree::write_dirty_pages`, `crates/inlaysql-core/src/btree/tree.rs`,
read back and printed from `inlaysql-bench`'s `oltp_export::replay`;
reverted after use, never shipped) counted dirty pages per commit for 2,001
commits (2,000 sequential single-row `INSERT`s plus the initial `CREATE
TABLE`) run through `inlaysql-oltp`'s own `--oltp-replay` path against
`inlaysql-bench_inlaysql-oltp-data` — the same volume class, same container
image, same code path `BENCHMARK.md`'s containerised InlaySQL row measures:

| Dirty pages | Commits | Share |
| --- | --- | --- |
| 1 | 27 | 1.3% |
| 3 | 29 | 1.4% |
| 4 | 25 | 1.2% |
| 5 | 1,849 | 92.4% |
| 6 | 70 | 3.5% |
| 7 | 1 | 0.05% |

Median **5 pages (20,480 B)**, mean 4.94 pages (~20,234 B), p95 5, max 7 —
matching Phase 0's host figure (median 5, p95 5, max 6 for single-row
commits) almost exactly. **Confirmed, not corrected**: the containerised
workload dirties the same ~5 pages / ~20 KB per commit the host does. (The
host's separate "points --rows 2000" figure maxed at 60 pages on a
structural B-tree reorganisation that this pure-sequential-insert run did
not happen to hit — not a discrepancy in the typical case, which both
measurements agree on.)

**How much of the 3.5x fsync-rate gap does the byte difference explain?**
Given the curve above is flat — no statistically real slope over the entire
0B-1MiB range — the honest quantitative answer is **essentially none of
it**. There is no reliable per-byte coefficient to multiply through:
R² = 0.017 means the regression slope is noise, not a measurement. Framed
against `SCOREBOARD.md`'s own numbers: MySQL's implied inter-fsync interval
is ~1.212ms (1/825 fsyncs/s) against InlaySQL-server's proxy ~4.20ms
(1/238 fsyncs/s) — a ~2.99ms gap per fsync. Nothing in the measured curve
moves by more than run-to-run noise (tens of µs) across the entire byte
range separating a plausible InnoDB redo-tail write from InlaySQL's ~20KB
commit, so the byte-count mechanism accounts for on the order of **0% of
that ~3ms gap, not merely "a small fraction"** — and even the first,
confound-inflated pass above (0.30 µs/KB, since retracted) would only have
put the difference between a 20KB and a ~1KB write at ~6µs, itself under 1%
of the gap. **The hypothesis's proposed mechanism is refuted by direct
in-container measurement, not merely unconfirmed.** The remaining ~100% of
the gap is unexplained by dirty-byte volume and must come from elsewhere —
`BENCHMARK.md`'s own instrument-gap section already points at
`inlaysql-server`'s thread-per-connection design (D2, no connection pool)
as the more likely locus, since the in-process commit-coordinator's own
batching ratio (4.76-6.31x) sits in the same order of magnitude as MySQL's
7.42x; this session's finding is consistent with that and does not change
it.

**Verdict on reopening the deferred/checkpointed-page-durability redesign:
do not.** Phase 0's rejection (above, host-scoped) holds in-container too,
on the specific volume and container backend (OrbStack, btrfs) this
project's own benchmark runs on. Phase 0's own caveat — "on a platform where
`fsync`/`fdatasync` cost genuinely scales with bytes... would be worth
revisiting there" — named a PLP-protected NVMe under Linux specifically,
which is not what was tested here (a virtualised block device inside a
Docker-alternative VM, not bare-metal Linux on real NVMe); that specific
platform remains formally untested, and this section does not close the
question for it. For the platform this project actually benchmarks on, the
one `SCOREBOARD.md`'s 4.7x figure was measured on, the answer is unambiguous:
flat, not sloped, and the redesign would not close this gap.

### Task 2 — the server-to-server barrier-rate gap, diagnosed but not fixed (2026-08-31)

The section above refuted the dirty-bytes explanation for InlaySQL-server's
lower implied `fsync` rate against MySQL. It left the thread-per-connection
design (`docs/server.md` D2) as the standing, unconfirmed suspect, backed
only by a harness-mismatched proxy (the in-process `WRITER_LEVELS` figure).
That proxy is no longer needed: `SCOREBOARD.md`/`BENCHMARK.md`'s same-day
follow-up gave `inlaysql-server` a live commits-per-fsync counter
(`Inlaysql_normal_commit_flushes`/`Inlaysql_normal_commit_tickets`, `SHOW
GLOBAL STATUS`) and measured it directly at 1/4/16 connections, 5
interleaved repetitions, load-gated (1-minute average 2.3-3.3/18
throughout). The result, in one sentence: **InlaySQL's commit-batching
mechanism ties or beats MySQL's at 1 and 4 connections and trails by only
~1.6x at 16, while its implied `fsync` rate falls from ~661/s to ~302/s as
connections go from 1 to 16 — MySQL's stays flat in a noisy 620-1640/s
band over the same range.** Full tables in `BENCHMARK.md`'s "Server-to-
server: InlaySQL's own commits-per-fsync, measured directly" section.

This section is the bounded diagnosis the task brief asked for once that
was known: candidate causes for *why* the barrier rate itself falls, each
labelled by how it was checked. **No fix is implemented or proposed here —
diagnosis only**, per the task's explicit instruction.

**Confirmed, by reading the code this session (not merely assumed):**

- **`TCP_NODELAY` is already set** (`stream.set_nodelay(true)?`,
  `crates/inlaysql-server/src/lib.rs:740`). Nagle's-algorithm-plus-delayed-
  ACK interaction — the classic cause of a request/response protocol
  crawling under concurrency — is not available as an explanation; it was
  checked, not assumed, and ruled out.
- **No evidence of an extra or duplicated barrier in the server path.** At 1
  connection, InlaySQL's own commits-per-fsync measured exactly **1.000
  across all 5 repetitions, CoV 0.0%** — one commit, one `fsync`, every
  time, the expected result if the server path shares `FileDevice`'s
  `CommitCoordinator` unmodified with the library path (it does:
  `crates/inlaysql-server`'s connections each open their own
  [`Database`](inlaysql::Database) on the same file and share its device,
  per D2's own doc comment). A doubled barrier per commit would show up
  here as a ratio of 0.5, not 1.0. It does not.
- **The adaptive gather window is a pure cooperative spin, not a timer**
  (`CommitCoordinator::coalesce_normal_commits`,
  `crates/inlaysql/src/device.rs:534-555`): `std::thread::yield_now()` in a
  loop, bounded at `COMMIT_COALESCE_MAX_YIELDS` (16,384) total turns and
  `COMMIT_COALESCE_STALL_YIELDS` (1,500) consecutive no-progress turns
  before giving up. It has no `sleep`, no condvar, no OS-level wakeup
  hint — it purely depends on the scheduler handing other, already-
  reserved commit threads a turn quickly enough for `writes_completed` to
  advance while this loop polls it. This code is identical between the
  server and the library path (both go through the same `FileDevice`), so
  it is not itself a server-specific defect, but its *effectiveness* is a
  function of how fast *other* threads can reach the point where they
  publish a ticket — which is where the server and library harnesses
  genuinely diverge, below.
- **The two harnesses are structurally different in exactly the way that
  would matter here.** The in-process `WRITER_LEVELS` sweep
  (`crates/inlaysql-bench/src/concurrency.rs:244-316`) drives each writer
  with a tight `std::thread::scope` loop calling `db.execute("INSERT ...")`
  directly on its own `Database` handle — no socket, no wire-protocol
  encode/decode, no separate OS process, nothing between one commit
  returning and the next one starting except a `Vec` push and a loop
  increment. `server_driver.py`'s server-to-server path instead round-trips
  each row through: a spawned Python OS process, `mysql.connector`'s own
  packet encode, a TCP send/recv pair over the compose network,
  `inlaysql-server`'s wire-protocol parse (`packet`/`protocol`/`shim`
  modules) and statement dispatch, then the same round trip back. Every one
  of those steps sits *before* a ticket ever reaches
  `coalesce_normal_commits`, and every one of them is absent from the
  in-process harness the "weak evidence" comparison used until this
  session.

**Plausible, consistent with the measurement, not directly profiled this
session (bounded effort, per the task):**

- **The likely mechanism**: round-trip cost specific to the server
  topology (network hop, wire-protocol parsing, and — critically — the
  scheduling latency of thread-per-connection with sixteen-plus blocking OS
  threads competing for turns, no pool) paces how fast new commit tickets
  *arrive* at the coordinator. If arrival rate degrades as more blocking
  threads are added — more context-switch overhead per useful unit of
  work, not more useful work — that would produce exactly the observed
  shape: a batching ratio that still climbs (more requests happen to be
  in flight when a leader's window opens) while the achieved `fsync` cadence
  falls (the gaps between one flush completing and the next leader having
  anything to gather widen). This is the sharper, evidence-backed version
  of the standing D2 hypothesis — it is no longer "batching might be fine,
  something else must be wrong," it is "batching *is* fine, the arrival
  rate upstream of it is not." **Not measured directly**: no
  socket-wait-time-vs-commit-wait-time breakdown was captured per thread
  this session; that profiling is the natural next diagnostic step and is
  explicitly out of this section's bounded scope.
- **Container network path as a contributing, not sole, factor.** Both
  engines cross the same `docker compose` bridge network and pay whatever
  latency OrbStack's virtualised networking adds, so this alone cannot
  explain an InlaySQL-vs-MySQL *asymmetry* — but MySQL's own connection
  handling (a bounded worker pool built for exactly this access pattern
  from the start) plausibly tolerates that added per-request latency
  structurally better than a cooperative-yield group-commit design tuned
  and validated primarily against a zero-latency in-process harness.
  Plausible, not measured; stated as a contributing factor worth a future
  session's profiling, not as a finding.

**What this changes going forward.** Any future work on this gap should
target *arrival rate into the commit coordinator* under the server's
connection model (a pool, batching reads across connections before
dispatch, or reducing per-statement wire-protocol overhead), not the
coordinator's own batching logic, which this session's direct measurement
shows is not the weak link. See `SCOREBOARD.md` §3.5/§6 and `BENCHMARK.md`'s
new server-to-server commits-per-fsync subsection for the numbers this
diagnosis is built on.

### Block-max WAND: built, measured, reverted

`PLAN.md`'s R6 names per-block impact bounds as the next step after MaxScore,
and there was real headroom to aim at. `tests/bm25_skipping_headroom.rs` counts
the documents a query still visits by counting filter calls:

| Corpus | visits at `k=10` | visits at `k=∞` | skipped |
| --- | --- | --- | --- |
| Flat vocabulary (the benchmark's) | 1,381 | 1,800 | 23.3% |
| Zipf-ish vocabulary | 1,103 | 1,943 | 43.2% |

So MaxScore leaves 1,371 visits that a perfect bound would remove. Block-max
was implemented against that — one `Impact` per 128 postings, rebuilt in
`commit` for terms written since the last one, with a stale term falling back
to its term-wide ceiling so a moved posting can never be bounded by a stale
block. It works, it is correct, and it does not pay:

| Block size | flat: visits | Zipf: visits |
| --- | --- | --- |
| none (MaxScore only) | 1,381 | 1,103 |
| 128 | 1,380 | 1,103 |
| 32 | 1,380 | 1,101 |
| 8 | 1,359 | 974 |

And the cost is real. Median of three `REPEATS=3 SUITE=retrieval` runs each
side: **BM25 p50 49.54 µs → 52.92 µs and hybrid 99.75 µs → 105.58 µs**, because
the per-candidate bound check is dearer than the 0.1% of visits it removes.

**Why it fails here is a property of the data, not the implementation.** These
documents are 8 to 32 terms long, so almost every term frequency is 1 or 2, so
a block's maximum frequency *is* the list's maximum frequency and the block
bound is the term bound. Block-max WAND earns its keep on long documents with
high term-frequency variance — web-scale text — and this corpus has neither.
The `k=∞` column is also the thing to notice: 1,800 of 2,000 documents match at
least one query term, because the vocabulary is twenty words. On a corpus where
a query term matches 1% of documents, both the headroom and the bounds would
look completely different.

So it is reverted, and the instrument stays. Anyone picking R6 back up should
run `bm25_skipping_headroom` on the corpus they actually care about first — and
if the answer is a realistic corpus with long documents, the implementation is
in this file's history rather than lost.

The next BM25 work is therefore not more skipping. It is the remaining
per-visit cost, and it has not been profiled: `bin/profile.rs` has no retrieval
suite.

**The pgvector vector-only loss is closed.** This section read "the open loss
is pgvector on vector-only search, ~4x" until the AHL-495 regeneration: the
current published pair is 147 µs here against pgvector's 198 µs, and the honest
reading is *close, not a rout* — their number includes a socket round trip and
ours does not.

### The exact-`f32` distance kernel is already vectorised, and it is half the query

`PLAN.md`'s W4 prescribes "SIMD distance kernels (NEON/AVX-512 behind a leaf
crate)" as the vector half of getting retrieval to 100x. That premise was never
checked against the compiled output, and it does not survive contact with it.

`crates/inlaysql-core/src/hnsw.rs`'s `distance` sums into eight explicit
accumulators specifically so the compiler may reassociate, and on aarch64 it
takes the offer. The inner loop, from `--emit asm`:

```
LBB279_4:
	ldp	q3, q2, [x14], #32     ; 8 floats of a
	ldp	q5, q4, [x15], #32     ; 8 floats of b
	fmul.4s	v3, v3, v5
	fmul.4s	v2, v2, v4
	fadd.4s	v0, v0, v2
	fadd.4s	v1, v1, v3
	subs	x13, x13, #1
	b.ne	LBB279_4
```

Eight floats per iteration, full-width NEON, with one scalar horizontal
reduction at the end of the call. Hand-written intrinsics would be writing out
what the compiler already emits.

**This survived the second metric.** `vector_l2_ops` (AHL-4xx) added Euclidean
distance, and the lane structure was extracted into one `lane_sum(a, b, term)`
rather than copied — the same move the BM25 scorer made for its two backends.
The closure is monomorphised and inlined, so re-running the check above finds
the cosine loop **byte-identical** (same instructions, same register
allocation, same `LBB` shape) and the L2 loop the same shape with the subtract
folded in:

```
LBB327_10:
	ldp	q2, q3, [x12], #32     ; 8 floats of a
	ldp	q4, q5, [x13], #32     ; 8 floats of b
	fsub.4s	v3, v3, v5
	fsub.4s	v2, v2, v4
	fmul.4s	v2, v2, v2
	fmul.4s	v3, v3, v3
	fadd.4s	v0, v0, v3
	fadd.4s	v1, v1, v2
	subs	x14, x14, #1
	b.ne	LBB327_10
```

Measured on uniformly random vectors (the ANN worst case), L2 costs 2–5% more
distance computations per query than cosine at the same `ef` — graph-shape
noise, not kernel cost — and recalls within ±0.03 of it at every corpus size
and dimension measured. The per-metric numbers are in `hnsw.rs`'s recall
tests, which print them under `-- --nocapture`.

`cargo test --release -p inlaysql-core --test vector_query_cost -- --nocapture
--ignored` measures what that leaves. On 2,000 vectors at dim 384, `k = 10`,
uniformly random directions (the ANN worst case) with held-out queries:

| | |
| --- | --- |
| Query mean | 57.16 µs |
| Distance calls per query | 1,318 |
| The dot products alone | 30.01 µs — **52% of the query** |
| The same, with `fmla` (`mul_add`) | 27.65 µs — 48% |

Three things follow, and they reorder the work:

1. **Kernel work has a ceiling of 52%**, and an infinitely fast kernel still
   leaves a 27 µs query. This is not where a 100x lives.
2. **Fusing to `fmla` is worth 4% of the query** and changes what the index
   computes — FMA rounds once where multiply-then-add rounds twice — so it
   trades bit-reproducible recall for four percent. Rejected on those terms.
3. **1,318 distance calls over a 2,000-vector corpus** is a graph doing two
   thirds of the work of the brute-force scan it replaced. The lever is doing
   *fewer* comparisons, not faster ones — and it is not `ef_search`, which is
   already priced correctly on this corpus:

| `ef_search` | calls/query | recall@10 | mean |
| --- | --- | --- | --- |
| 16 (floored to 20 by the multiplier) | 660 | 0.587 | 25.49 µs |
| 32 | 885 | 0.721 | 36.03 µs |
| **64 (shipped)** | 1,318 | 0.897 | 56.09 µs |
| 128 | 1,765 | 0.986 | 84.27 µs |

Halving the budget halves the time and costs a third of the recall. The
shipped default is on the curve, not above it.

So the remaining vector work is the **48% that is not arithmetic** — candidate
heaps, the visited set, neighbour-list fetches — and the graph's own
selectivity at small corpus sizes. Both need a profile of the query phase
before anything is written; ~~`bin/profile.rs` does not cover the retrieval
suite yet, and adding it is the first step~~ — **done, 2026-08-30.** See
"The retrieval suite, and where the non-kernel 48% actually goes" below.

The avenues below are the ones that would widen the pgvector margin, in order
of expected value, now reordered by the above:
1. ~~**The traversal, not the kernel.** Half the query is heap and set
   bookkeeping around the distance calls. Profile it first.~~ — **profiled,
   2026-08-30.** It is not one thing; see the breakdown below.
2. **Memory layout.** Neighbour lists and vectors laid out for sequential access
   during a graph walk, so the prefetcher works for us. Still open; the
   2026-08-30 profile names this as the likely explanation for the largest
   single bucket but a leaf sampler cannot confirm a cache stall directly —
   see below.
3. ~~**Quantised distance kernels.** `VECTOR(n, INT8)` already shrinks storage 4x;
   computing distances *in* int8, rather than converting to `f32` first, makes
   the memory-bandwidth win a compute win too. Note the int8 path currently
   measures *slower* than exact (155.21 µs against 88.29 µs on the published
   suite), so this is a repair before it is an optimisation.~~ — **diagnosed,
   2026-08-30, and it is not a small repair.** The kernel is already
   vectorised; the loss is structural. See below.
4. **Quantised paged nodes**  — `PagedHnswIndex` stores exact
   `f32` even for an int8 column, so the paged path currently forfeits the 4x.
5. ~~**Filter-aware walks** instead of over-fetching.~~ — **done.** The
   `WHERE` is compiled into a row predicate and pushed into the index walk:
   rejected rows are traversed but neither returned nor counted, so a
   selective filter no longer widens the probe in geometric re-runs. See
   `Engine::retrieve_filtered`.

### The retrieval suite, and where the non-kernel 48% actually goes (2026-08-30)

**Machine state, disclosed per section 6's rule:** `uptime` 1-minute load ran
1.5–5.2 over this session (four users logged in, nothing else identified as
consuming CPU), mostly 2–4. Every `sample` capture below was taken with 1-minute
load under 4.5; the one moment it touched 5.2 was between runs, not during a
capture. Treat absolute microseconds as a noise band, proportions as the
finding, same as every other profile in this file.

**The suite.** `crates/inlaysql-bench/src/bin/profile.rs` gained a `retrieval`
suite (`--suite retrieval`), mirroring `crates/inlaysql-bench/src/main.rs`'s
own retrieval workload byte-for-byte: the same corpus generator (`VOCABULARY`,
`synthetic_document`/`synthetic_query`, `hashed_embedding`), the same schema
(`docs (id INTEGER, body TEXT, embedding VECTOR(dim))`, indexed on both `body`
and `embedding`) and the same three query shapes (`vector_score`, `bm25_score`,
`fuse`). `--query vector|bm25|hybrid` picks one shape for the timed loop
instead of cycling all three — cycling would give the shape under
investigation about one sample in three, the same dilution `joins-limit`
exists to avoid for joins — and `--quantized true` switches the embedding
column to `VECTOR(dim, INT8)`, to profile int8 in isolation. Both indexes are
warmed (one `vector_score` query, one `bm25_score` query) before
`announce_query_phase()`, so neither the HNSW graph build nor the BM25 index
build leaks into the timed window.

**Method.** `--suite retrieval --rows 2000 --dim 384 --limit 10 --query vector`
(text-derived corpus, the realistic shape, matching what `BENCHMARK.md`'s
headline vector numbers use), `sample <pid> <seconds> -f <file>` attached after
`PROFILE_QUERY_PHASE_START`, then every leaf symbol traced up to its parent
frames before being trusted — this codebase has a standing example
(`__psynch_cvwait`) of one leaf meaning two different things, and it held here
too: `_platform_memcmp` and `PageCache::get` in the leaf table below turned out
to belong to a *different* part of the query than the graph walk (see "a
second finding" below), not to the HNSW code at all.

**The breakdown, as a share of `HnswIndex::search_with_ef` itself** — the same
scope `PERF.md`'s 52%-kernel figure used, so the two numbers are comparable —
from two independent samples (22s/15,325 samples and 15s/11,011 samples;
`evaluate_score`'s call into `search_with_ef` was 11,411 and 8,458 samples of
those totals respectively, confirmed by summing every `search_with_ef`
call-site node in the tree):

| Bucket | Share (2 runs) | Where |
| --- | --- | --- |
| Kernel (`stored_distance`, i.e. `lane_sum`) | **~40–42%** | `crates/inlaysql-core/src/hnsw.rs:1785` |
| Traversal bookkeeping (`search_layer` self time) | **~46–48%** | `crates/inlaysql-core/src/hnsw.rs:1219-1310` |
| Candidate/results heap `pop` (sift-down) | **~8–9%** | `BinaryHeap::pop` on the two heaps at `hnsw.rs:1242-1243` |
| Heap `push` growth (reallocation) | **~1.4%** | same two heaps — see below |
| Final `results.into_vec(); sort_unstable()` | **~1–1.3%** | `hnsw.rs:1307-1308` |
| `VectorMetric::prepare` (query normalisation) | **~0.5%** | `hnsw.rs:1511` |
| `Visited::new` allocation + `memset` | **~0.2%** | `hnsw.rs:1512`, `hnsw.rs:1609-1614` |

**This does not exactly reproduce the 52% figure, and that is disclosed rather
than papered over.** The kernel's measured share here (~40–42%) is lower than
the 57.16 µs/30.01 µs = 52% isolated-timing figure. Two things differ and
either could account for it: the corpus (this profile used the text-derived
shape the published suite reports; the 52% figure used the uniform-random
"ANN worst case" shape, which needs more distance calls per query at the same
`ef`) and the method (statistical leaf-sampling of the compiled kernel in situ
versus a hand-timed isolated loop of just the multiply-adds). The
**conclusion** the 52% figure supported — kernel work is a minority of the
query, not the majority, so it is not where a 100x lives — reproduces and is
if anything stronger here. The **specific number** should not be treated as
portable across corpora or measurement methods.

**Traversal bookkeeping (~46–48%), broken down by what is actually in it,**
since a sampler cannot split `search_layer`'s own self-time further than the
compiler's inlining left it: reading `hnsw.rs:1219-1310`, every visited node
pays `visited.visit(neighbor)` (`hnsw.rs:1277`, a bounds-checked array write —
cheap), a `Candidate` struct build and the `enters`/`admits` comparisons
(`hnsw.rs:1280-1296`, branchy float comparisons via `total_cmp`), a
non-growing heap `push` in the common case, and — the part the task asked
about by name — `neighbors_at(nodes, current.node, layer)` (`hnsw.rs:1276`,
`hnsw.rs:1317-1323`). That call walks `nodes[node].neighbors[layer]`, and
`Node` (`hnsw.rs:472-487`) stores `neighbors: Vec<Vec<usize>>` — a *separate
heap allocation per node per layer* — and `vector: StoredVector`, itself a
`Vec<f32>` or `Q8Vector { values: Vec<i8>, .. }`, another separate allocation.
Every neighbour a walk visits is therefore two more pointer chases (one for
its neighbour list, one for its vector) beyond the `Vec<Node>` index itself,
each potentially a cold cache line unless the allocator happened to place them
together. This is a plausible, structurally-grounded explanation for why
`search_layer`'s own bookkeeping — which on its face is a handful of
comparisons and an array write per node — costs as much as the arithmetic
kernel itself. It is **not a confirmed cache-miss count**: `sample`'s 1ms
statistical sampling reports where the instruction pointer was, not stall
cycles, and this machine has no hardware-counter profiler set up in this
session (Instruments' "Time Profiler w/ CPU counters" template or `perf stat`
would be the next step, not attempted here). Stated as what it is: the
strongest *available* explanation, not a measured one.

**The two `BinaryHeap`s reallocate during the hot per-query loop.** `frontier`
and `results` at `hnsw.rs:1242-1243` are both `BinaryHeap::new()` — zero
capacity — even though `ef` (the target beam width) is known at the top of
`search_layer`. The call tree shows real `alloc::raw_vec::RawVec::grow_one` →
`finish_grow` → `realloc`/`memmove` activity hanging off `search_layer`'s
`push` call sites, on *every* query, not just the first: roughly 1.4% of
`search_with_ef`'s time in the run above. `BinaryHeap::with_capacity(ef + 1)`
(or similar) for both heaps is a small, bounded, easy-to-A/B candidate — not
implemented here, this is reconnaissance.

**A second finding, outside the scope the task asked about but real and
measured on the same corpus:** through the *full SQL path* (as opposed to the
isolated `HnswIndex::search` call `PERF.md`'s 52% figure measured), roughly
18–20% of the *entire query's* wall time — separate from and in addition to
`search_with_ef`'s own ~74–75% share of the query — is spent in
`Engine::retrieve_rows` fetching each of the `LIMIT k` result rows from the
underlying B-tree by row id (`TreeStorage::get_row` → `CowBTree::get_from`,
`crates/inlaysql-core/src/btree/tree.rs`), after the HNSW search has already
returned the winning ids. This is where the `_platform_memcmp` and
`PageCache::get` leaf samples actually come from — B-tree key comparison and
page-cache lookups during ten point reads per query, not the vector index.
Naming it because "where vector search actually spends its time" through SQL
includes it even though it is not part of `HnswIndex` at all; not counted in
the 48% breakdown above, which is scoped to `search_with_ef` to stay
comparable with the 52% figure.

### The int8 path: diagnosed, and it is (b) not (a) (2026-08-30)

**Reproduced.** `--suite retrieval --rows 2000 --dim 384 --limit 10 --query
vector --quantized true`, three repeated 5s runs against three exact runs,
same corpus, same machine, interleaved: exact 18,283–18,570 ops/s (~54–55 µs),
int8 6,531–6,772 ops/s (~148–153 µs) — **int8 2.7–2.9x slower**, tight spread
(~2%) within each side. The direct published-suite protocol
(`inlaysql-bench --suite quantization`, text-derived corpus) reproduces the
same direction on four runs: exact p50 71.00–97.50 µs, int8 p50
153.13–171.71 µs — int8 1.76–2.16x slower, median around 2x. **The 155.21 µs
int8 figure holds up closely (measured 153–172 µs); the 88.29 µs exact figure
is on the high side of what this session measured (71–97.5 µs, median ~79)** —
the gap between exact and int8 is, if anything, a little worse today than
`BENCHMARK.md` currently states, not better. This session's machine load
(2–5, not idle) is disclosed as the likely source of the spread; call the
88.29 µs figure provisional rather than wrong, and note the direction (int8
slower than exact) is not in question either way.

**Settled, 2026-08-30, same commit: the full benchmark regeneration this
provisional note asked for.** Median of three complete `run.sh` runs
(`bench/results/20260830T{120941,122626,123414}Z.txt`, load 3.0–4.4/18):
exact p50 **78.96 µs**, int8 p50 **165.92 µs** — squarely inside the band this
session already measured (71–97.5 µs / 153–172 µs), and a ratio of 2.10x
slower, right at "median around 2x" above. `BENCHMARK.md`'s published figure
is now 78.96 µs, retiring the 88.29 µs figure this note flagged rather than
silently keeping it. The direction and rough magnitude both held; only the
precise exact-side number moved, as predicted here.

**Why, traced to the instruction level.** `vector_score` defaults to cosine,
so a query (kept exact `f32` — see `hnsw.rs:1506-1511`'s own comment on why:
quantising the query too would cost recall without saving resident memory)
against an int8-quantised corpus calls `stored_distance` →
`Q8Vector::dot_f32` (`crates/inlaysql-core/src/quantize.rs:42-48`). Reading the
source, this looked like an unvectorised scalar loop — no `LANES`-style
accumulator the way `lane_sum` (`hnsw.rs:1756-1774`) has. **That reading was
wrong**, and finding out required disassembly, not inference: `otool -tV -p`
on the compiled `stored_distance` symbol, at the exact call-site offset
`sample` attributed 9,390 of 13,067 `search_with_ef` samples to (71.9%, one
run), shows real NEON — `tbl.16b` (byte-lane shuffle), `scvtf.4s ... #0x18`
(vectorised int8→`f32` convert via a fixed-point trick), `fmul.4s` (four
lanes) — the compiler auto-vectorised `dot_f32` despite the plain
`.zip().map().sum()` source. **The kernel is vectorised. It is still the
majority of the query's time anyway**, because unpacking costs more per
element than the exact path pays: four `tbl`+`scvtf.4s`+`fmul.4s` groups to
convert and scale 16 packed `i8` bytes into four lanes of `f32` each, against
zero unpack instructions for `lane_sum`'s direct `f32` loads. Measured:
kernel share of `search_with_ef` went from ~40–42% (exact) to **75.4%**
(int8, 9,857/13,067 one run) — not because more distance calls ran (recall at
the shipped `ef` is within 0.014 of exact per `BENCHMARK.md`, consistent with
a similar call count), but because each call got dearer.

**The verdict: (b), an inherent property of the current structure, with one
small (a)-shaped detail riding along.**
- **(b), primarily.** Comparing a persisted int8 corpus against a
  full-precision `f32` query — the recall-preserving choice `hnsw.rs` already
  defends — means every query-time distance call *must* reconstruct `f32`
  from the corpus's packed bytes. There is no query-time comparison that
  avoids this without either quantising the query (rejected on recall
  grounds, and not attempted here either) or a different kernel strategy.
  `Q8Vector::dot_q8` (`quantize.rs:51-60`), the pure-integer path that
  *would* avoid the conversion, is never reached at query time — the query is
  never a `Q8Vector` — and, checked in the same disassembly pass, it is not
  even vectorised itself: `ldrsb`+`smaddl` in a scalar loop, not the `SDOT`
  instruction ARM NEON has for exactly this. PERF.md's own unimplemented
  avenue 3 ("computing distances *in* int8 ... makes the memory-bandwidth win
  a compute win too") describes what would actually close this gap, and nothing
  in the current code does it.
- **(a), a small piece.** `dot_f32` multiplies by `self.scale` on every
  element (`quantize.rs:46`) instead of factoring the constant out of the sum
  and applying it once at the end — algebraically safe for a dot product
  (`sum(code_i * scale * q_i) == scale * sum(code_i * q_i)`), and it is fused
  cheaply into the existing `fmul.4s` so the saving is one vector instruction
  per four-lane group, not the dominant cost. Bounded, low-risk, worth an
  A/B — but on its own it will not close a 1.76–2.9x gap whose majority is
  the unpack, not the scale multiply. `l2_f32`/`l2_q8` do not get this same
  fix for free: the delta `code*scale - query` does not let the scale factor
  out of the sum the way a pure product does.
- **Not (c).** The slowdown is not a measurement artefact — it reproduces
  across two harnesses (this session's `profile.rs retrieval` suite and the
  published `inlaysql-bench --suite quantization`), is directionally stable
  across seven total runs, and is explained down to the instruction level.

**Named candidates, not implemented — this was reconnaissance:**
1. Factor `self.scale` out of `dot_f32`'s (and `dot_q8`'s) summation —
   bounded, cheap to try, will not close most of the gap on its own.
2. `BinaryHeap::with_capacity(ef + 1)` for `search_layer`'s two heaps —
   bounded, ~1.4% of `search_with_ef` on this corpus, larger at bigger `ef`.
3. A query-time int8 comparison kernel that does not materialise `f32` at
   all — quantising the query transiently (never persisted, never affecting
   stored recall) and running a genuinely vectorised `i8`×`i8` dot product
   (ARM `SDOT`, which nothing in this codebase currently emits) instead of
   `dot_f32`'s convert-then-multiply. This is the real fix for the int8 loss
   and it is a redesign, not a patch — scope and A/B it separately.
4. Memory layout for `Node.neighbors`/`Node.vector` (candidate 2 in the list
   above) — plausible from the structure, not yet confirmed by a stall-cycle
   profiler.

---

### Task 3 — the library commit cycle, instrumented (2026-08-31)

Task 2 left the diagnosis at the container boundary: batching ties or beats
MySQL's at 1 and 4 connections and trails only ~1.6x at 16, so the deficit is
barrier *rate* (~661 → ~302 fsync/s from 1 to 16 connections), not batching.
The plan this section executes decomposed the library commit cycle itself —
no server, no socket — into timed segments to find where the non-fsync time
lives. Two tasks, reported in order: the static code reading (B), then the
instrumented run (A). The brief's pre-registered decision tree and
expectations are restated here before the data, and the branch taken is
labelled explicitly.

#### Task B — does the coordinator accept tickets while an fsync is in flight?

**Yes — intake is open during flush; but cohort membership is closed before
the barrier, and the gather window is a gate-drain wait.** Both halves matter,
and the second half is what makes the cycle serial:

- The reservation gate (`reserved: Mutex<bool>` +
  `reservation_done: Condvar`, `crates/inlaysql/src/device.rs:74-76`) is held
  only across a commit's in-gate work: acquired in `begin_reservation`
  (`device.rs:998-1015`), released in `release_normal_reservation`
  (`device.rs:707-722`) *before* durability. The fsync is called from
  `make_durable_with_cohort` with no gate held — `CowBTree::commit` calls
  `sync_commit` (`crates/inlaysql-core/src/btree/tree.rs:1210`) after
  `end_normal_commit` (`tree.rs:1154`), and the comment at `tree.rs:1206-1210`
  states the intent outright: durability is the operation parallel writers are
  allowed to overlap. So during flush N's barrier, other writers can acquire
  the gate, write their WAL records and dirty pages, and publish tickets
  (`Device::commit_ready`, `device.rs:1149-1160`). The write phase *is*
  pipelined.
- But a round's coverage set is snapshotted at
  `target = writes_completed.load()` (`device.rs:560`) strictly *before* the
  barrier (`device.rs:561`), so a ticket published while flush N is in flight
  is **not** covered by round N: its writer waits as a follower on
  `flush_done` (`device.rs:526-537`), then loops back and becomes the leader
  of round N+1. There is no moment at which a cohort is being gathered while
  a barrier is in flight — the gather window
  (`coalesce_normal_commits`, `device.rs:609-630`) only ever runs after a
  leader is elected and strictly before its own barrier.
- And the gather window's exit condition is the load-bearing detail the
  serial-cycle hypothesis needs revising for: it keeps yielding while
  `normal_inflight > 0 || normal_waiters > 0` (`device.rs:613-617`) — that
  is, **it waits for the reservation gate to drain** before the leader
  captures its target. The barrier is therefore positioned after the entire
  cohort's serialized gate work, not merely after the tickets that existed
  when the round began.

So the code reading *confirms* the serial-cycle structure (gather → flush →
gather, never overlapped) but *rejects* the implication that a two-stage
pipeline is the whole fix: the gather segment's duration is set by how long
the serialized gate takes to drain, and that same serialized gate is (per A,
below) the throughput ceiling. Both are reported so the epic scoping can see
them as one mechanism.

#### Task A — the cycle, decomposed

**Instrumentation.** `CommitCoordinator` grew per-segment nanosecond
accumulators (`gate_wait_ns`, `gate_hold_ns`, `gate_hold_racing_start_ns/_count`
+ end-state split, `follower_wait_ns`, `gather_spin_ns`, `fsync_ns`, `post_ns`,
`gap_ns`), each one relaxed `fetch_add` per event — timestamps at segment
boundaries only, no per-ticket records. They are readable without process
`Drop` through the existing `FileDevice::commit_stats()` snapshot (the
`SHOW GLOBAL STATUS` pattern's library-side counterpart; the keeper-handle
trick `inlaysql-server` uses is reproduced by the harness), and the
`INLAYSQL_COMMIT_STATS` drop print now includes them. Segment semantics:
*gather-wait* = gate queue wait + leader gather spin + follower barrier wait
(reported as three sub-segments, because they are three different mechanisms);
*WAL-write* = time inside the reservation gate (rebase, record encode, record
+ dirty-page `pwrite`s); *fsync* = the barrier; *post-work* = the leader
waking followers; *gap* = coordinator idle between one cycle's end and the
next leader's election.

**Harness.** `cargo run --release -p inlaysql-bench --bin commit_cycle` —
in-process OS threads (the concurrency suite's shape: one `Database` handle
per writer, disjoint keys, one-row INSERT transactions), 1/4/16 writers, 5
repetitions each, the full (level, rep) schedule Fisher-Yates-shuffled with a
fixed seed so no level is systematically first in wall-clock time, 2000
transactions per writer per rep, fresh file per rep, stats delta read through
a still-open keeper handle, and the lost-write verification the concurrency
suite runs. The database file lives on a named Docker volume
(`inlaysql-commitcycle-data`), the same volume class the barrier-rate and
fsync-floor measurements used.

**Expectations stated before the data landed** (session discipline): (i) at
16 writers, gather-wait would grow with writer count and dominate non-fsync
time, matching branch 1; (ii) single-writer would put its ~0.4 ms gap in
gate-hold; (iii) gate-hold would grow with writers because in-gate writes
race the in-flight barrier (the ~18-23x slowdown documented at
`device.rs`, macOS-flavoured, unverified on Linux). Prediction (iii) was the
one I expected to be wrong on this platform; it turned out to be the finding.

**Results.** Three runs of the full schedule on the container
(18-CPU Docker VM; host load averages 2.7-5.0 over the sitting, disclosed
because run 1 was the quietest and runs 2-3 visibly slower across *every*
level — machine drift, not code). Medians per repetition; all values µs
unless stated. Run 1 / run 2 / run 3:

| Segment (per cycle, µs) | 1 writer | 4 writers | 16 writers |
| --- | --- | --- | --- |
| cycle (= 1/fsync-rate) | 1205 / 1531 / 1503 | 2033 / 2338 / 2201 | 3270 / 3451 / 3411 |
| fsync | 1080 / 1405 / 1359 | 1299 / 1695 / 1546 | 1543 / 1898 / 1853 |
| gather (leader spin) | 0 / 0 / 0 | 370 / 300 / 305 | 934 / 896 / 892 |
| post (wake followers) | ~1 | 54-69 | 45-75 |
| gap (coordinator idle) | 117 / 125 / 144 | 459 / 447 / 418 | 733 / 698 / 691 |
| WAL-write (gate hold, per commit) | 70 / 79 / 87 | 373 / 414 / 380 | 746 / 812 / 775 |
| — of which acquired while flush in flight | 0% | 78-81% (388-469 µs) | 96-97% (737-904 µs) |
| gate queue wait (per commit) | 0 | 562-647 | 8230-9944 |
| follower barrier wait (per wait) | 0 | 1347-1769 | 2536-2817 |
| gate busy (commits/s × mean hold) | 5-6% | 44-47% | 88-90% |
| c/fsync | 1.00 | 2.47-2.93 | 3.69-4.08 |
| commits/s | 830 / 653 / 665 | 1348 / 1071 / 1204 | 1189 / 1101 / 1144 |

**Consistency checks, both pre-registered:**

- **(i) Segment sum vs measured cycle time.** The gap counter is
  double-entry bookkeeping for the residual: measured cycle − (fsync + gather
  + post) should equal gap/flushes. 1 writer: 124-144 vs 117-144 µs — agree
  to ~0.1%. 16 writers: 605-689 vs 656-733 µs — agree to 1.3-2.6% of cycle.
  4 writers: 287-370 vs 418-459 µs — the counter exceeds the residual by
  6.8-13% of cycle, marginally outside the ~5% band. The direction of the
  discrepancy is explained, not hand-waved: checkpoint flushes ride the same
  accumulators but not the `normal_flushes` denominator (measured share:
  flushes/normal_flushes = 1.02 at 1 writer, ~1.035 at 4, ~1.05 at 16), and
  the first cycle's pre-election time is in `elapsed` but not in the gap
  counter. Neither effect is large enough to change any interpretation below.
- **(ii) Derived fsync/s reproduces the earlier measurements.** Run 1
  (quietest): 830 / 492 / 306 fsync/s at 1/4/16 writers against the
  targets ~660 / ~490 / ~302 — 4 and 16 reproduce to 0.4% and 1.3%; 1 writer
  lands *above* the target for the reason the plan's own arithmetic predicts:
  the ~660 figure was server-derived, and the in-process path removes the
  wire (~23% at 16 connections per the plan; here the 1-writer gap is
  660→830, ~25%). Runs 2-3 (visibly busier host) landed 653-665 / 428-457 /
  271-297 — the same shape with the whole scale shifted by machine drift,
  which is exactly what the shuffled schedule is there to expose. Checks
  passed; nothing needed stopping for.

**Decision tree, branch taken: branch 2, with branch 1's antecedent true.**
Gather-wait *does* dominate non-fsync time and *does* grow with writer count
(0 → ~300 → ~890 µs) — the branch-1 trigger fires literally. But the
instrumentation resolves what the gather *is*: at 16 writers the reservation
gate is 88-90% busy, 96-97% of gate holds are acquired while a barrier is in
flight, and the mean hold inflates ~10x over solo (79 → 775 µs). The gather
spin is the leader waiting for that serialized, barrier-slowed gate queue to
drain — the exit condition requires `normal_inflight == 0`. The system's
throughput ceiling is the gate, not the barrier: 1/775 µs ≈ 1290 commits/s
bounds what any batching or pipelining schedule can push through the gate at
these hold costs, and measured throughput (992-1237 across runs) sits just
under it. A two-stage/pipelined group commit alone would therefore recover
at most the gap + gather overlap (~1.6 ms of a 3.4 ms cycle) before slamming
into the gate ceiling — worthwhile, but strictly second to making the gate
hold cheaper (moving dirty-page/WAL writes out of the serialized section, or
shrinking what the gate covers — InnoDB copies a small redo tail under a
latch and writes pages outside any commit-serializing lock). **Scope the epic
as both halves; pipelining without gate-work reduction buys ~12%.**

**Single-writer verdict: the one-mechanism-two-cells reading is refuted.**
In-process single-writer commits land at 1.21-1.53 ms against a bare barrier
floor the same reps measure at 1.08-1.41 ms (the floor itself drifts with
host load — see the floor-probe spread) — the library's own overhead above
the floor is ~0.2 ms and splits into gate-hold (70-116 µs) and the
inter-cycle gap (117-205 µs), with the gather spin structurally absent (the
solo emptiness check fires before any yield) and gate busy at 5-6%. The
published single-writer loss (1.51 ms vs MySQL's 1.11 ms, both server-side)
is therefore *not* the same mechanism as the 16-writer deficit: at one
connection the library is essentially at floor, ~0.2 ms of its non-fsync
time is gate + inter-cycle, and MySQL's own 1.11 ms is plausibly its floor
too. The remaining single-writer delta lives outside the library (wire +
per-connection handler), which the brief excluded from this batch's scope;
it is reported, not chased.

**Believed but not measured, separately marked:** (a) the 10x gate-hold
inflation is the pwrite-racing-fsync effect plus queueing on the VM's page
cache, not CPU starvation — 18 CPUs against 16 writer threads + leader says
the scheduler was not the constraint, but no stall-cycle profile was taken
inside the hold; (b) the barrier's own growth with writer count (1.08 →
1.54-1.90 ms) is plausibly the same racing-pwrite effect seen from the
flusher's side (the floor probe measured barriers with no concurrent
writes); (c) the first run's single 1329 commits/s outlier at 1 writer
(fsync 654 µs, below every floor measurement) was kept in the medians and
flagged rather than discarded.

---

### Residual-filter elision, measured before being built — and not worth its price (2026-09-01)

`PLAN.md`'s A1 proposed skipping the residual `WHERE` on an indexed range when
the index range was built from the same predicate, estimating **15-20%** of the
shape's engine time. The estimate came from adding the eval cluster (~11%) to
"a chosen index's `_platform_memcmp` share, because `compare_cells` on `TEXT`
calls it". Measured properly, that addition is wrong and the item is not worth
what it costs.

**Where `memcmp` actually goes.** Attributing every `memcmp` sample to its
nearest engine-level ancestor, rather than assuming: `CowBTree::get_from` 8.5%,
`WalkBounds::admits` 2.3%, `starts_below` 1.3%, `child_index` 1.2%,
`ReadCursor::admits` 1.2%, `walk_raw_row_ids` 1.1% — B-tree key comparison
during descent, every one of them. `Collation::compare`, the only path the
*filter's* text comparison can reach it by, is **0.9%**. The `memcmp` share is
descent cost that elision cannot touch; folding it into the filter's share
roughly doubled the apparent prize.

**The ceiling, measured rather than argued.** The residual filter was skipped
*entirely and unconditionally* on the indexed path — deliberately unsound, and
reverted — to put a number on the best case elision could ever reach:

| | ops/s (3 interleaved repetitions) |
| --- | --- |
| Baseline | 48,977 / 46,438 / 46,509 |
| Filter skipped entirely | 56,293 / 54,909 / 54,159 |

**1.18x**, non-overlapping. And that is the *ceiling*, on the friendliest
possible query: `WHERE email >= ? AND email < ?` against an index on `email`
binds both terms, so nothing residual remains and the skip is total. A correct
implementation fires only where it can prove equivalence, pays plan-time proof
cost, and is refused by every shape `collect_conjuncts` does not fully
recognise — so it lands below 1.18x by construction.

**What it would cost.** `Engine::candidate_bytes`'s doc comment states the
invariant elision breaks: "the filter is still evaluated over every row all
three yield, so this is purely a matter of how many rows are read — never of
which ones match... which is why choosing badly here is slow rather than
wrong." That property is what makes every future access-path change safe by
construction. Trading it for at most 1.18x on one shape — which still loses
~2x to SQLite journal afterwards, so the loss is not closed either — is a bad
trade, and this is a recommendation not to make it.

**Where the time actually is**, from the same profile (23,952 samples,
`--suite indexed-range`): B-tree/page work **21.1%**, allocator **20.9%**,
`memcmp` **16.7%** (descent, per above), eval/filter **12.3%**, harness timer
6.0%. The allocator share is the familiar one — `_xzm_xzone_malloc_tiny` 6.4%
plus `_xzm_free_main` 6.5%, the per-cell `Entry`/`Value` decode cost AHL-488
diagnosed and AHL-493 failed to remove with page views. The point path reaches
it through `node_at`, which decodes a leaf into `Node::Leaf { entries }` even
on a cursor hit.

**The lead worth following instead:** B1a fixed the *scan* path's version of
exactly this by caching page **bytes** rather than decoded nodes, and measured
1.38x with no regression anywhere. The point path has the same shape of cost
and no equivalent yet. That is a larger prize than 1.18x, on the same shape,
without touching an invariant.

### The uncached leaf, fixed by caching the bytes instead of the node (2026-09-01)

B1 (below) found that the raw row scan never caches the leaves it reads, so a
repeated prepared statement re-`pread`s and re-copies the same pages forever —
20.1% of engine time in `memmove` on the `LIMIT 10` join shapes. It also found
that the obvious fix does not work: caching the leaf as a `Node::Leaf` wins
1.42x on those shapes and loses ~10% on full scans, because a sweep pays the
per-cell `entries` decode for thousands of pages it never reuses. Three
admission rules failed to separate the two cases and the change was reverted.

The conclusion recorded then was "make insertion cheap, not admission clever",
and that is what this is. `RawLeafCache` (`btree/tree.rs`) holds up to 64
*undecoded* leaf pages as the `Rc<[u8]>` the scan already has in hand, so an
insert is a refcount bump with no decode and no allocation, and a sweep that
churns the whole cache pays nothing worth measuring. It is read and written
only by `walk_raw_row_values`, under exactly the guards `cached_page` and
`cache_committed` apply (D4's carve-outs: never a dirty page, never outside the
data area, never under page reuse), and it is cleared wherever the decoded cache
is.

Measured, interleaved, three repetitions:

| Shape | Before | After | |
| --- | ---: | ---: | --- |
| `joins-limit` (both `LIMIT 10` shapes) | 83,586 ops/s | **115,807** | **1.38x**, 3/3, non-overlapping |
| `joins` (full shapes dominate) | 49–50 ops/s | 49–50 ops/s | flat — the regression that killed B1 is gone, sign flips between reps |
| `points` | 1.78–1.85M ops/s | 1.80–1.87M ops/s | flat, sign flips |
| `indexed-range` | 66.2–66.3k ops/s | 67.8–69.7k ops/s | flat to slightly better |

**The first version of the test for this proved nothing, and mutation-testing
is the only reason that was caught** — the second time in two days, which is
now a pattern worth naming rather than an anecdote. Removing the insert
entirely left it green, because the pre-existing single-leaf
`row_scan_cursor` was serving the repeat. The test now scans across many
leaves (which one retained leaf cannot cover) and runs a scan of a different
span in between (which displaces that cursor), leaving the raw leaf cache as
the only thing that can answer; with the insert removed it reports 25 device
reads against the required 0.

One guard is documented as untestable rather than quietly assumed: the
dirty-page check cannot currently fire, because copy-on-write gives a modified
leaf a *new* page id, so the scan asks for an id the cache has never seen. It
mirrors the decoded cache's rule and stays for the day page ids stop being
unique per version; the test that covers writes says explicitly that it does
not cover that guard.

### A `REAL` join was 223x slower, and the obvious fix bought nothing (2026-09-01)

The other half of the join-key audit. `hash_join_key` allowed `INTEGER | TEXT |
BLOB`, so a `REAL` key fell to `Materialise` and a 2,000-row join took 77.7 ms
against 277 µs for the identical join on an `INTEGER` column.

The exclusion's stated reason was that an `INTEGER` next to a `REAL` "compares
as `f64` and would need normalisation the hash does not do" — but the function
already requires both sides to share a declared type, so that pair cannot reach
it, and `REAL`-to-`REAL` was being refused for a hazard that could not occur.
What makes a `REAL` key's values reliably `Value::Real` is write-side affinity:
`sql::coerce` converts an `INTEGER` bound into a `REAL` column on the way in,
and a derived column with no stored column behind it carries `DataType::Numeric`,
which is not hash-eligible either.

**Adding `REAL` to the list changed nothing, and the measurement is the only
reason that was noticed.** `EXPLAIN` reported `HASH JOIN` and the runtime stayed
at 78 ms — a hash join performing exactly like the nested loop it replaced. The
cause is the bucket index, which is taken from the *low* bits of the hash: an
`f64`'s mantissa sits in those low bits, and the values applications actually
store (`1.5`, `3.0`, prices, counts scaled by a constant) use only the top few
mantissa bits, so their bit patterns end in long runs of zeros. Multiplying by
an odd constant cannot repair that — a pattern with *k* trailing zeros still has
*k* trailing zeros after the multiply — so every key masked down to bucket zero
and the "hash join" was a linear scan per probe.

Mixing the bits down (SplitMix64's finaliser) fixes it. Measured, interleaved,
three repetitions:

| Shape | Before | After | |
| --- | ---: | ---: | --- |
| `REAL` join | 77.71 ms | **348 µs** | **223x**, 3/3, non-overlapping |
| `INTEGER` join (control) | 277 µs | 262 µs | unchanged |

**The test that was supposed to catch a wrong answer here could not have.** A
differential join test was added for both new key classes, and mutation-testing
it — deleting the collation fold, then the float normalisation, and re-running —
showed it passing against both mutants. The reason is bucket arithmetic, not the
oracle: the hash table sizes itself `max(16)` buckets, and FNV-1a's low four
bits are invariant under the ASCII case bit, so at the fixture's twelve rows
`'ada'` and `'ADA'` share a bucket whether the hash folds or not. It takes 64
buckets before the case bit reaches the mask. The fixture now uses 80 rows,
where the mutant does fail, and the invariant itself — equal keys hash alike —
is additionally asserted directly as a unit test, which holds at any table size.
The `-0.0`/`+0.0` normalisation is kept but is *not* load-bearing against the
current multiplier (the sign bit cannot reach the low bits either); it is there
for the next person who changes the hash, and the unit test says so rather than
implying the measurement proved it.

### A case-insensitive join was 204x slower than a case-sensitive one (2026-09-01)

Found by probing one join per key type rather than by profiling a known-slow
shape. `hash_join_key` refused any join key whose `ON` resolved a non-binary
collation, so `TEXT COLLATE NOCASE` had no hash path *and* — without a matching
index — no probe path either, and fell all the way to `JoinStrategy::Materialise`,
the replay-the-inner-side-per-outer-row plan.

The refusal was for a real reason, stated in the function's own doc comment: a
hash join's load-bearing invariant is "keys the `ON` calls equal hash to the
same bucket", and hashing the *stored* bytes of `'ADA'` and `'ada'` puts two
`NOCASE`-equal keys in different buckets, so the join would miss the pair. What
the comment also says is the way out: "a hash that *over*-groups is safe because
candidates still compare their keys". Hashing what the collation **compares**
rather than what the row **stores** can only over-group — `Collation::fold`
already exists for exactly this and borrows when the transform is the identity,
so a `BINARY` key still hashes its own bytes with no copy.

Both candidate paths had to fold too, not just the bucket: the general path
re-evaluates the `ON`, but the single-equality shortcut
(`hash_key_is_full_on`) compares keys directly and skips it, so a folded bucket
with an unfolded comparison would have produced the exact false-negative the
old refusal was preventing. The engine's retained hash-build cache also had to
take the collation into its identity — the bucket layout is built from the
folded hash, so a `NOCASE` build answering a `BINARY` probe would silently drop
pairs.

Measured, interleaved, three repetitions, 2,000 × 2,000 rows, `COUNT(*)` over
an inner join:

| Shape | Before | After | |
| --- | ---: | ---: | --- |
| `TEXT COLLATE NOCASE` | 96.53 ms | **473 µs** | **204x**, 3/3, non-overlapping |
| `TEXT` binary (control) | 467 µs | 399 µs | unchanged — the control did not regress |

Both sides returned the same 2,000 rows throughout. Correctness is tied down
three ways rather than asserted: a test that runs the same `NOCASE` join
through the hash plan, the index-probe plan and the materialising plan and
requires all three to agree; a second one for `RTRIM`, whose folding removes
bytes rather than changing them; and the expected pairs checked against a real
`sqlite3` 3.54 binary, which produces the identical five pairs and the identical
`RTRIM` grouping. The 20,000-round differential fuzzer against SQLite passes,
including its `collated_predicates`, `collated_orderings_and_groupings`,
`inner_joins` and `left_joins` targets.

**Why this shape matters more than its size suggests.** MySQL's own default
collation is case-insensitive, so an application pointed at `serve --mysql`
joins on a `NOCASE` key by default — this was the plan the wire-protocol story
hit first, and it was the worst plan the engine has.

One consequence worth stating: a `NOCASE` join with a matching index used to
probe that index and now hash-joins instead when the query is a full scan.
That is the faster plan (the same measurement puts an indexed `NOCASE` probe at
2.62 ms against the hash join's 473 µs), and the probe is still chosen when a
`LIMIT` makes the shape non-full-scan. The test that pins the probe's
index-collation rule now uses a `LIMIT` to keep testing the probe.

### The `LIMIT`-join overhead, profiled — and why the fix was reverted (2026-08-31)

`BENCHMARK.md`'s two `LIMIT 10` join rows lose 2.4–3.3x to SQLite, and every
edition of the plan carried them as "unexplained, unprofiled". Profiled here
with `bin/profile.rs --suite joins-limit` (which already existed for exactly
this and had never been run), release binary, `sample` over the query phase,
24,481 samples.

**Where the time goes.** By leaf symbol: `_platform_memmove` **21.4%**,
`PageCache::get` 13.9%, `_platform_memcmp` 10.2%, allocator 14.6%. Attributing
`memmove` to its callers is the finding: **20.1% of total engine time is
`memmove` beneath `CowBTree::walk_raw_row_values`** — 12.2% inside
`FileDevice::read` and 7.9% in `Rc::copy_from_slice` — on queries that return
ten rows.

**Why.** The raw leaf scan reads a leaf's bytes and never caches them. It says
so, deliberately (`tree.rs`, the `KIND_LEAF` arm): "Leaves are deliberately not
inserted — they were never decoded into a `Node` here, and decoding one purely
to cache it would give back the allocation this scan exists to avoid." The
consequence is that a *repeated prepared statement* re-`pread`s and re-copies
the same pages on every execution, forever. Not a capacity effect: raising the
page cache 8 MiB → 256 MiB (32x, well past the working set) moved the shares by
less than a point (memmove 21.4% → 20.7%).

**The fix works, and it is not shippable as written.** Caching the leaf —
decoding it once into `Node::Leaf` and inserting through the existing
`cache_committed`, so D4's carve-outs all still apply — measured, interleaved,
3 repetitions each:

| Shape | Before | Leaf caching | Verdict |
| --- | --- | --- | --- |
| `joins-limit` (both `LIMIT 10` shapes) | 86,006 ops/s | **121,424** | **1.42x**, 3/3, non-overlapping |
| `joins` (full shapes dominate) | 52–53 ops/s | **47** | **~10% regression**, 3/3 |
| `points` | 1.77–1.79M ops/s | 1.76–1.85M | flat |
| `indexed-range` | 67.1–68.3k ops/s | 66.6–67.1k | flat |

The regression is the mirror image of the win: a sweep reads every leaf once,
reuses none, and pays the per-cell `entries` decode (AHL-488's cost) for
thousands of pages that then evict each other. Three admission rules were tried
to separate the two cases, all measured interleaved:

1. **Cache the leaf the walk stops on** (`out.len() >= bounds.limit`) — flat,
   88,014 vs 85,057 ops/s, inside the noise floor. The pages actually re-read
   belong to an *index probe*, which asks for every row matching its key rather
   than a bounded count, so this excluded exactly the pages worth keeping.
2. **Cache while the walk is still short** (`out.len() < 256`) — kept the win
   (1.45x) but kept the regression too (48/47 vs 52/53). A batched scan starts
   every batch with an empty `out`, so the window never closes on a sweep.
3. **Admit on the second miss** (2Q-style, 512-entry window) — kept the win
   (1.44x) and *halved* the regression (50/48/50 vs 54/51/53, ~5.7%, still 3/3).
   Mechanism for the residue: a batch boundary falling inside a leaf makes the
   next batch re-read the page the last one finished on, which is itself a
   second touch. Requiring a third touch was built and measured, but that run's
   own control side spanned 37–53 ops/s — the measurement had fallen apart
   (well past `PERF.md` §4's floor), so it decides nothing.

**Reverted, deliberately.** A 1.4x win on one published losing row bought with
a ~6–10% regression on another published losing row is not a trade this project
takes, and the standing rule is that a fast path may not regress the slow path.
What is banked is the diagnosis, the size of the prize, and three rules that do
not work. The next attempt should make *insertion* cheap rather than admission
clever — the decode is the cost, and the scan already holds the page bytes it
would cache, so a cache that could hold an undecoded leaf (a `Node::RawLeaf`
variant, or bytes beside the node) would pay neither the decode nor the second
copy. That is a broader change to the `Node` enum and its match sites, which is
why it was not attempted at the end of a session rather than the start.

**Methodological note, recorded because it nearly published a wrong number.**
The first version of this measurement compared a "before" run to an "after" run
taken about thirty minutes apart and reported 1.31x. Re-measured interleaved in
one sitting, the same "before" binary produced 85–89k ops/s where it had
produced 68k — the machine, not the code. Every figure above is interleaved,
same sitting, control side re-measured in every repetition. `bench/repeat.sh`
exists for exactly this reason and `bin/profile.rs` has no equivalent; a
harness that interleaves A/B binaries would have caught it automatically.

### `PageCache::get` was a `BTreeMap` lookup; now it is a hash (AHL-521, 2026-09-02)

Profiled `bin/profile.rs --suite joins-limit` again (release, `sample` over
the query phase, 13,124 samples, machine at load 8–12/18 so shares not
wall-clock are the evidence). Leaf symbol: **`PageCache::get` 18.5%**, the
single hottest frame — 1,364 samples beneath `CowBTree::node_at` (the probe
descents, one lookup per level per outer row) and 1,050 beneath
`walk_raw_row_values` (the driving scan re-descending root-to-leaf on every
execution of the prepared statement). `_platform_memcmp` 13.5% is next, split
between `partition_point` (the separator search), `WalkBounds::admits` and
`get_from`. The root plan's A5 estimated this cost at ~9% on the range shape;
on the `LIMIT` join it is twice that, because that shape is nearly all descent.

**Cause.** The cache's page-id → slot index was a `BTreeMap<PageId, usize>`.
The hit path had already been made as cheap as a `BTreeMap` allows (clock
bits, no LRU relink) — what remained was the map itself: `log n` node visits
and a key compare in each, for a key that is one integer.

**Fix.** An open-addressing hash table in `cache.rs`: Fibonacci hash of the
page id, linear probing, backward-shift deletion (no tombstones), load held
at or under one half, `alloc` only. A hit is one multiply, one mask and one
compare. Pinned against a `BTreeMap` model under 20,000 scrambled
insert/remove/lookup steps across wrap-around.

**Measured**, interleaved, same sitting, control re-run in every repetition,
`--seconds 4..6`:

| Shape | Before | After | Verdict |
| --- | --- | --- | --- |
| `joins-limit` | 116.3k / 117.1k / 117.5k ops/s | **129.9k / 129.2k / 131.3k** | **1.11x**, 3/3, non-overlapping |
| `points` | 1.60 / 1.55 / 1.60 / 1.65 / 1.63 M ops/s | 1.67 / 1.58 / 1.61 / 1.67 / 1.61 M | flat; after wins 4/5 — the point read did not move |
| `joins` (full shapes) | 65 / 65 ops/s | 65 / 64 | flat |

The full-join shape is flat because its cost is elsewhere (it is the
`walk_raw_row_values` sweep and the hash build, not descents). The
wall-clock tables in `BENCHMARK.md` owe this the same regeneration they owe
AHL-512 through 520.

### A sweep reads sixteen pages per syscall, not one (AHL-522, 2026-09-02)

Profiled `bin/profile.rs --suite aggregate --rows 100000` on the AHL-521
binary (release, `sample` over the query phase, 9,656 samples, load 8–12/18
so shares are the evidence). Inclusive: **`FileDevice::read` 30.6%** of the
query — `pread` 19.7% self, `memmove` 8.3% — for a table the operating
system already held in memory. The 100k-row table is ~10 MB of leaves, over
the 8 MiB default of both the decoded page cache and the shared raw cache,
so every execution of the prepared statement re-read every leaf, one 4 KiB
`pread` per page, ~2,500 syscalls per query. The remaining top frames were
`BTreeMap::get_mut` 10.0% (the `GROUP BY` probe — separate item), the
allocator ~8%, `run_select_to` 5.3%, `mem_cmp` 4.8%.

**Fix, in the tree.** `CowBTree::with_raw_page` — the raw scan's only read
path — now keeps a read-ahead window: on a miss whose page id continues the
previous run it fetches four pages, and from the fourth consecutive miss
sixteen (64 KiB) in one `Device::read`, then serves the following pages from
the window. Two rules make it safe: only ids below the handle's committed
`next_page_id` are ever fetched (a committed copy-on-write page is immutable;
anything above that bound may still be being written by another handle —
pinned by a test with a device that counts reads past that offset, and the
test fails when the clamp is removed), and the window is dropped exactly where
the caches are dropped under page reuse. A failed wide read falls back to the
one-page read, so errors surface as before.

**And in the device.** The shared raw cache keyed one entry per read and
answered only a buffer of the same length, so a 64 KiB read was a miss and
a 64 KiB insert. Now: a wide read is served page by page when every page is
resident (copied under the read lock — collecting `Arc`s first cost a `Vec`
per read and measured as a 2% loss on a table that fits), and after a wide
device read each page is admitted **only while there is room, never by
evicting**. That last rule is the one that mattered: admitting by eviction
gave back most of the win (1.26x → 1.08x), because a sweep larger than the
budget then allocated, copied, evicted and freed every page it touched on
every execution.

**Widening only from the third sequential miss** is also measured, not
chosen: widening on the second leaf cost the 50-row `indexed-range` shape
2–8% (3/3) — a short range spanning two adjacent leaves paid a 16 KiB read
for its second leaf. From the third it is flat.

**Measured**, interleaved against the AHL-521 binary, control re-run in
every repetition, `--seconds 4..5`:

| Shape | AHL-521 | AHL-522 | Verdict |
| --- | --- | --- | --- |
| `aggregate`, 100k rows (does not fit the caches) | 85 / 83 / 85 ops/s | **109 / 99 / 110** | **1.26x**, 3/3, non-overlapping |
| `joins`, 20k rows (full-scan shapes) | 66 / 63 / 65 ops/s | **77 / 72 / 73** | **1.17x**, 3/3, non-overlapping |
| `aggregate`, 20k rows (fits the shared cache) | 579 / 555 / 567 | 571 / 573 / 526 | flat, mixed sign |
| `indexed-range` | 61.9 / 66.3 / 63.7k | 63.3 / 66.9 / 63.7k | flat |
| `indexed` | 403 / 405 / 401k | 403 / 405 / 407k | flat |
| `joins-limit` | 129.8 / 129.8 / 130.9k | 130.0 / 129.4 / 130.9k | flat |
| `points` | 1.64 / 1.72 / 1.61M | 1.69 / 1.58 / 1.66M | flat, mixed sign — the point read did not move |

**What it does not do.** Leaves rewritten across many small commits scatter
(copy-on-write moves a touched leaf to a fresh id), and a sweep over those
never forms a run, so it reads one page at a time as before — no loss, no
gain. A bulk-loaded or freshly rebuilt table is the sequential case. The
`memmove` share stays: the window's bytes are still copied once into the
per-leaf `Rc<[u8]>` the cells borrow from.

### The reorder swapped the fast join into the slow one; the bench caught it (AHL-524, 2026-09-02)

The first full `REPEATS=3 ./bench/repeat.sh` since 2026-08-30 landed at
`7b20175` (three runs, load 0.82–4.04/18 throughout, none `CONTAMINATED`;
`bench/results/20260902T022325Z-repeat.txt`). The PK-inner full join, published
as ~1.15x slower, now wins 1.17x. But the secondary-index full join — 5.85 ms
on 08-30, 3.71 ms on 09-01 at `2eeced7`, published as 7.5x faster — read
**14.03 ms**. A 3.8x regression on a published winning row.

**Bisected**, `SUITE=joins` single runs, gate off, same sitting (so relative,
not publishable; the gap is 3x and survives the noise):

| Commit | PK inner p50 | Secondary inner p50 |
| --- | --- | --- |
| `2eeced7` (published table) | 13.72 ms | **4.82 ms** |
| `894ecef` AHL-512, join reorder | 17.74 ms | **30.15 ms** |
| `1dbe18c` allocation diet | 12.60 ms | 18.24 ms |
| `7b20175` AHL-521/522 | 9.34 ms | 14.52 ms |

AHL-512 is the cause. Its cost function priced an outer row at one unit and
a hash-built inner row at two — `hash = 2·inner + outer` — so it preferred to
build the smaller table and drive from the larger one. For `users JOIN posts`
(20k × 160k, eight posts per user) that swapped the query into posts-driving:
160k outer rows, 160k hash probes, a 20k-row build. Written order was 20k
outer rows, 20k probes each yielding eight, a 160k-row build. Same 160k
output rows either way; the probes are what differ, and 140k extra probes at
~70 ns each is the 10 ms. Its "1.31x on the joins suite" was measured on
`bin/profile`'s `joins` suite, which cycles all four shapes in one number, so
the PK-inner win hid the secondary-inner loss. The lesson is the one
`BENCHMARK.md` already states: a suite-level number is not a per-shape one.

**Fix.** `OUTER_ROW_COST = 4` charged per outer row on both paths (so the
hash-versus-probe choice does not move on its own): `hash = 2·inner +
5·outer`, `probe = outer·(12 + group + 4)`. With that, driving from the
smaller table costs less, which is what the measurement says, and the reorder
now moves the *PK-inner* written order into users-driving rather than the
other way round. The three EXPLAIN pins in `tests/cost_planner.rs` that
asserted the inverted swap now assert the corrected one; a planner unit test
pins that users-driving costs less than posts-driving for the benchmark's
sizes.

**Measured**, `SUITE=joins`, gate off, single run at the fix (the gated
`REPEATS=3` regeneration is owed and follows):

| Shape | `7b20175` | AHL-524 | SQLite journal |
| --- | --- | --- | --- |
| PK inner, full | 9.34 ms | **3.21 ms** | 10.12 ms |
| Secondary-index inner, full | 14.52 ms | **3.47 ms** | 29.98 ms |
| PK inner, LIMIT 10 | 5.25 µs | 5.50 µs | 3.50 µs |
| Secondary-index inner, LIMIT 10 | 7.79 µs | 8.08 µs | 4.42 µs |

Both full shapes now run the same users-driving plan and land at ~3.3 ms —
below the 4.82 ms `2eeced7` had, because AHL-521/522's descent and syscall
savings apply on top. The `LIMIT` rows do not move: a `LIMIT` shape is
never reordered.
### `GROUP BY` finds its group by hash, not by walking an ordered map (AHL-523, 2026-09-02)

The aggregate profile (above, under AHL-522) had `BTreeMap::get_mut` at
10.0% of `SELECT n, COUNT(*) ... GROUP BY n` over 100k rows in 100 groups,
with `eval::mem_cmp` beneath it another 4.8%: seven key comparisons per row,
each a full `compare_values` under the column's collation, where one would do.
The root plan's B4a notes had already established that the ordered map was
not load-bearing for output order — `sort_rows` orders groups by
representative rowid afterwards, and the map's order survived only as the
stable sort's tie-break in two edge cases no test pins.

**Fix.** `GroupTable` in `engine.rs`: open addressing over entry indices,
entries in first-seen order, no deletion, load at or under one half; the
stored hash is compared before the key is. Both aggregate paths use it, so
the two still find the same groups. `hash_group_key` agrees with
`compare_group_keys` by construction — that is its whole contract, and a test
walks the pairs where the two could disagree: `1` and `1.0` are one group, so
integers hash through the same `f64` funnel reals do (lossy above 2^53
exactly as the comparison is); `-0.0` normalises; text hashes what its
collation *compares*, through `Collation::fold`, so `'Ada'`/`'ADA'` share a
bucket under `NOCASE` and `'a'`/`'a  '` under `RTRIM`; a vector compares by
length alone and hashes by length alone; `NULL`s are one group. A miss no
longer descends twice: the probe ends at the bucket the new group goes in.

Recorded divergence: the comparison calls `NaN` equal to every number, which
no hash can honour. Under the ordered map a `NaN` key's group depended on
insertion order; now it forms its own group unless it collides. Both
arbitrary; this one is stable.

**Measured**, interleaved against `7b20175`, control re-run every repetition:

| Shape | `7b20175` | AHL-523 | Verdict |
| --- | --- | --- | --- |
| `aggregate`, 100k rows / 100 groups | 110 / 108 / 110 ops/s | **122 / 128 / 116** | **1.12x**, 3/3 |
| `aggregate`, 20k rows | 484 / 547 ops/s (one 186 outlier) | 543 / 597 / 636 | ~1.15x |
| `points` | 1.64 / 1.68 / 1.57M | 1.75 / 1.75 / 1.48M | flat, mixed sign |

Stacked with AHL-521/522 on the same shape: 85 → 122 ops/s since the
morning's baseline, 1.44x, and the 10.28 ms scan-and-decode floor B4 owns is
now the majority of what is left.

### The streamed aggregate folds from the row bytes, not from a decoded row per row (AHL-528a, 2026-09-02)

Profiled `bin/profile.rs --suite aggregate --rows 100000` at `d1dbe4c` (release,
`sample` over the query phase, load 5–12/18 so shares are the evidence).
Inclusive: **`Decode::next` 63%**, of which `walk_raw_row_values` 36.5% (the
sweep: `scan_leaf_cells` 16%, `WalkBounds::admits` 7.9%, `FileDevice::read`
10% because a 100k-row table is larger than the 8 MiB shared cache) and
**`row::decode_row_masked` 19.7%** — a `Vec<Value>` per row —
**`drop_in_place<ExecRow>` 9.6%**, `GroupTable::find` 7.3%, `run_select_to`'s
own loop 7.3% self, `eval::evaluate` 4.6%, `skip_value` 4.4%, the allocator
~10% in total.

**Cause.** AHL-514/515 taught the aggregate to fold from the stream instead of
holding every row, and AHL-519/520 made the fold itself allocation-free for a
row that finds its group. What was left was the *stream*: `Decode` turned
every row's bytes into an owned `ExecRow` — the `Vec`, a `String` for every
wanted `TEXT` cell — handed it through the boxed iterator, and the fold
dropped it. `SELECT n, COUNT(*) ... GROUP BY n` keeps the first row of each of
its hundred groups; the other 99,900 were decoded and freed to be counted.

**Fix.** The single-table read path hands the fold its row *bytes*
(`exec::AggregateInput::Bytes`), and the fold decodes each row into one
borrowed buffer it reuses — `decode_row_ref_masked_into`, parked between rows
the way `DecodeFilter` parks its scratch — applies the `WHERE` on the
borrowed cells exactly as `DecodeFilter` does, evaluates the group key and the
aggregate arguments from them, and materialises the row only when it opens a
group. Every other source — a join, a derived table, a scored retrieval, a
`WITHOUT ROWID` table — still arrives decoded (`AggregateInput::Rows`) and
folds exactly as before. The per-row work is written once, in `Folder::step`,
over a two-impl `AggregateCells` trait (owned `Value`s, borrowed `ValueRef`s),
and both go through `AggFold::step` — the one-fold rule holds. The tail of a
blocking `SELECT` (windows, `DISTINCT`, `ORDER BY`, `OFFSET`/`LIMIT`,
projection) became `finish_blocking`, shared by the two ways `run_select_to`
now arrives at held rows.

**Measured**, interleaved, control re-run in every repetition. The machine
was shared with another benchmarking agent throughout (load 12–22/18 for the
first run, 5–7 for the second), which is why there are two runs of the target
shape and why `points` is wide.

| Shape | `d1dbe4c` | AHL-528a | Verdict |
| --- | --- | --- | --- |
| `aggregate`, 100k rows / 100 groups, `--seconds 5` (load 12–22) | 103 / 89 / 97 ops/s | **156 / 139 / 155** | **1.5x**, 3/3, non-overlapping |
| the same, re-run (load 5–7) | 102 / 102 / 100 | **151 / 147 / 151** | **1.48x**, 3/3, non-overlapping |
| `aggregate`, 20k rows (fits the cache) | 531 / 515 / 534 | **797 / 823 / 815** | **1.55x**, 3/3, non-overlapping |
| `joins`, 20k (full-scan shapes) | 45 / 47 / 45 | 48 / 42 / 46 | flat, mixed sign |
| `indexed-range`, 20k | 66.4 / 66.1 / 62.9k | 66.6 / 61.2 / 63.5k | flat, mixed sign |
| `points`, 20k | 1.42 / 0.77 / 1.40M | 0.90 / 0.87 / 1.65M | flat, mixed sign — contention-wide, and the point read does not touch this path |

Stacked on the day: 85 ops/s at the morning's baseline (before AHL-521) to
~150 now on this shape, 1.76x.

**Pinned.** `a_bytes_fed_aggregate_agrees_with_the_decoded_stream_and_the_collected_fold`
in `tests/cost_planner.rs` ties the three folds — bytes-fed (`FROM t`),
decoded-stream (`FROM (SELECT * FROM t)`), collected (`GROUP_CONCAT` forces
it) — to one answer over the aggregate shapes under six `WHERE` clauses,
including one through a `NOCASE` column's collation. Removing the `WHERE`
from the bytes-fed arm fails it on the first filtered shape (checked by
mutation). Reverting the whole change to the decoded stream is behaviour-
identical by design and passes; the profile is the only witness to that one.

## 4. The measurement floor (2026-08-30): A/A test, noise-growth recompute, and the point-read dissection

Every engine optimisation decision is frozen pending this section. The harness
has been observed to move up to 2.6x on unchanged code, which means it cannot
currently distinguish a real win from noise. This section measures the floor
rather than assuming it, states the project's acceptance criterion, and
dissects the flagship point-read swing (636,980 → 342,747 → 901,158 → 522,562
ops/s across four editions with no commit touching that path).

### The A/A floor, per suite — the acceptance criterion

An A/A test is the identical binary against identical data, repeated. Three of
the four suite groups below reuse `2cb2539`'s already-published, already
load-disclosed regeneration runs (`bench/results/20260830T{120941,122626,
123414}Z.txt`, `20260830T{124155,124632,125240}Z.txt`,
`20260830T{125800,131326,132715}Z.txt` — load 2.3–4.8/18 throughout, gated by
`bench/run.sh`'s own quiet-machine check, `dirty: no`, unrebuilt binary
throughout). This session adds a fourth, deliberately homogeneous group: the
flagship point-read row alone, isolated across five separate `run.sh`
invocations tonight, still on the identical unrebuilt `ee1a5c4` binary
(`20260830T{120941,122626,123414,151233,152024}Z.txt` — the last two are
tonight's, load 3.0–4.4/18 throughout, `dirty: no`).

| Suite | n | metrics | median spread | p95 spread | CoV (median) | CoV (p95) | ≥10% disagreement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Main (points/indexed/joins/vectors/concurrency-narrow/retrieval), all columns | 3 | 343 | 13.6% | 193.1%\* | 6.2% | 62.8% | 196/343 (57.1%) |
| Main, **core columns only** (ops/s, p50, joins/s, commits/s, recall@k — excludes `max`/`p95`/`p99`/`cold`, which are expected to swing) | 3 | 108 | 9.5% | 74.6% | 4.0% | 34.7% | 53/108 (49.1%) |
| Concurrency wide sweep (`WRITER_LEVELS=1,2,3,4,5,6,8,12,16,24,32`), core columns | 3 | 45 | 8.8% | 25.5% | 3.6% | 10.3% | 18/45 (40.0%) |
| Quantisation spot-check (`DOCS=100000`), core columns | 3 | 24 | 0.6% | 14.7% | 0.3% | 6.1% | 5/24 (20.8%) |
| **Flagship: point-read ops/s alone**, 5 independent runs, identical unrebuilt binary | 5 | 1 | 20.4% | — | 7.3% | — | 1/1 |

\* The 193.1%/24764.4%-class outliers are exclusively `max` columns — a single
unlucky tail sample in one of three runs (e.g. SQLite WAL's read `max` was
1.20ms in one run against 4.8µs and 3.9µs in the other two). `summarise.py`'s
own commentary calls this out: a `max` is one sample and is expected to swing;
a `p50` or an `ops/s` figure is the measurement itself. The core-columns rows
above are the ones to quote.

**The acceptance criterion, stated prominently: this project will not report
an A/B result smaller than the CoV shown above for its suite. The target for
a repaired harness is CoV < 3%. The harness does not meet it today** — even
on a `run.sh`-gated, disclosed-quiet machine, the median core-metric CoV is
3.6–4.0% and the p95 is 10–35%. On the single most scrutinised metric (point
read), five runs of the *same unrebuilt binary* on a nominally quiet machine
already produce a 20.4% max-min spread (7.3% CoV) — a floor that exists before
any rebuild, any edition change, or any code touches the read path at all.

**A second, directly measured number: the floor is worse on a real desktop.**
This machine is shared and was actively in interactive use throughout this
session (see the next section). Three more point-read samples, taken back to
back on the identical rebuilt binary while the machine's 1-minute load average
was confirmed at 7.7–31.4/18 (`bench/run.sh`'s gate would have refused all
three — they were run by invoking the binary directly, as a deliberate,
disclosed diagnostic, not published as an A/B result): 1,031,814 / 620,299 /
855,310 ops/s — a 48.1% max-min spread, **20.2% CoV, essentially triple the
quiet-machine figure for the same metric.** The floor is not one number; it is
"~7% CoV on a quiet, gated machine" and "~20% CoV on this machine as normally
used," and a reader should be told which one a future A/B claim is being
compared against.

### The noise-growth claim, recomputed

`BENCHMARK.md` currently states: "median of three complete runs: 196 of 343
metrics disagreed by 10% or more across the three — worse than the last full
edition's 56 of 285" — i.e., noise roughly tripled. Two problems with quoting
that comparison directly:

1. **The two figures were computed with two different versions of
   `bench/summarise.py`.** The old edition's 56/285 predates the commit that
   added `^InlaySQL is ` to the `PROSE` exclusion regex (`86d7a0c`), so it
   double-counted six derived comparison sentences ("InlaySQL is 2.68x faster
   than...") as if they were independent measurements. Re-running today's
   `summarise.py` against the exact two old files it cites
   (`bench/results/20260825T{103354,104132}Z.txt`) gives **54/279**, not
   56/285.
2. **The two editions do not measure the same metrics.** The new edition's
   `run.sh` added a `p99` column to every latency table and added latency
   percentiles to the concurrency table for the first time (it previously
   reported only `commits/s`/`committed`/`conflicts`). That is 77 of the new
   edition's 343 metrics (22%) that simply did not exist when the old
   edition's 285 were counted — inflating both denominators mechanically,
   independent of any real noise change.

Recomputing on **only the metrics measured in both editions** (matched by row
label and column name — 266 metric-instances in common, using today's
`summarise.py` for both sides so the tool is held constant):

| | old edition (2 runs, `20260825T{103354,104132}Z`) | new edition (3 runs, `20260830T{120941,122626,123414}Z`) |
| --- | --- | --- |
| Disagreement ≥10% on the 266 common metrics | **54/266 (20.3%)** | **146/266 (54.9%)** |

**The growth is real — 2.7x on an apples-to-apples metric set, essentially the
same magnitude as the originally published 2.9x (19.6%→57.1%).** It is not an
artifact of the new edition simply measuring more things. One caveat that
cuts the other way and should temper "tripled": the old figure comes from a
2-run pairwise difference while the new figure comes from a 3-run
max-min/median spread, and a 3-sample spread is expected to run somewhat wider
than a 2-sample one purely from having one extra draw of the tail, independent
of any true increase in per-run variance. The direction and rough size of the
growth hold up under recomputation; the exact "2.9x" or "2.7x" should be read
as "the noise roughly doubled to tripled," not to two significant figures.

### Environment diff

- **OS/kernel, reboot:** unchanged and ruled out. `sw_vers` reports macOS
  27.0, build 26A5416b, `Darwin 27.0.0` throughout. `sysctl kern.boottime`
  shows the machine booted 2026-08-18 and has been up 12+ days continuously —
  no reboot, no OS update, between any of the editions this file discusses
  (2026-08-25 through 2026-08-30).
- **Disk fill:** not implicated. `/System/Volumes/Data` is 74% full, 472GB
  free of 1.8TB — well short of the >90% range where APFS/SSD performance
  typically degrades. Cannot rule out a fuller state on the specific days of
  earlier editions (no historical `df` samples exist), but today's state is
  not tight.
- **CPU frequency / thermal state: not measured.** `powermetrics` (the
  standard way to read Apple Silicon P/E-core frequency residency) requires
  `sudo`, and `sudo -n true` fails ("a password is required") in this
  non-interactive session. `pmset -g therm` returns "no thermal warning level
  recorded" for all three of its readings — on Apple Silicon this is not
  evidence of *no* throttling, just evidence the lightweight query does not
  populate without dedicated tooling running. Substitute used: wall-clock
  duration vs. reported CPU-seconds per invocation (see the point-read
  dissection below) as an indirect proxy for scheduling delay, which is a
  different thing from frequency and does not resolve this dimension.
- **Background load: directly observed, not inferred.** During this session's
  measurement window the machine's 1-minute load average ranged from 2.4 to
  **459.9**/18 logical CPUs inside about ten minutes, then took over twenty
  minutes to settle back under `bench/run.sh`'s 4.5 gate. `ps aux` at the peak
  identified an interactive desktop session, not a runaway benchmark: `opencode`
  at 57.9% CPU, `WindowServer` at 42.8%, four Google Chrome renderer processes
  at ~41% combined, three VS Code processes at ~21% combined, plus (at a
  different point in the same window) an Xcode-beta/`CoreSimulator` iOS 17.4
  simulator boot (`diagnosticd`, `MobileCal` widget, `ibtoold`), Playwright
  headless-Chromium test workers, a `php artisan serve` dev server, and —
  notably — a **second, independent `inlaysql-bench --suite all` process**
  running with default parameters that this session did not start, confirming
  the box is shared with other concurrent work on this exact repository. One
  benchmark invocation crashed outright with `SqliteFailure(SystemIoFailure,
  "disk I/O error")` during this contention; it is recorded, not retried into
  invisibility.
- **Harness defect this directly exposes:** `bench/run.sh`'s load gate samples
  the 1-minute average **once, immediately before the run starts**, then never
  checks again. A run that begins during a lull and collides with a load
  spike seconds later completes anyway and publishes a contaminated number
  with no record that anything happened. Evidence this is a real gap, not a
  theoretical one: across the five quiet-gated, disclosed-load point-read runs
  above (start load 3.03–4.42/18), the Pearson correlation between disclosed
  start-load and measured point-read ops/s is **r ≈ 0.18** — weak and, at n=5,
  not distinguishable from no correlation at all — despite the same metric's
  CoV nearly tripling (7.3%→20.2%) when the *actual* load during measurement
  is confirmed heavy. The disclosed number and the real number can diverge
  freely under the current gate. **Recommendation:** sample load throughout
  the run (bracket each timed section, not just the invocation) and flag or
  discard sections whose load moved outside the gate's threshold mid-run,
  rather than trusting one snapshot taken before anything ran.

### The point-read 2.6x, dissected

Four candidate dimensions, tested rather than assumed:

1. **Page-cache / warm-vs-cold state — ruled out by code inspection.**
   `points.rs`'s `inlaysql_points` (and the SQLite equivalents) write all
   `rows` on one open handle, then immediately run all `lookups` on that same
   handle in the same process, with no close/reopen between. The data is
   necessarily resident in both InlaySQL's own page cache and the OS buffer
   cache when the timed read loop starts — there is no code path here that
   could measure a cold read. Whatever moves the point-read number, it is not
   this.
2. **CPU frequency scaling / thermal throttling — not measured.** As above:
   no `sudo`, no `powermetrics`, `pmset -g therm` uninformative on this
   hardware. This dimension is neither confirmed nor ruled out.
3. **Background load / scheduling contention — directly measured, and it
   moves both wall-clock and the timed metric.** One points-suite invocation,
   run while the machine's load average was 31.4/18, took 1:15.77 wall-clock
   for 4.12s of reported user+system CPU time — an 18x wall/CPU ratio, i.e.
   the process spent the overwhelming majority of its life descheduled,
   waiting for a core. That contention mostly lands outside the specifically
   *timed* read loop (each phase re-starts its own `Instant::now()`), which is
   why the reported ops/s under confirmed heavy load (620k–1.03M) was not
   catastrophically different in absolute terms from the quiet-machine
   figures (466k–572k at a different, larger row count) — but the *variance*
   of the timed metric itself still tripled (CoV 7.3%→20.2%, see above). This
   is the dimension with the clearest, directly-collected evidence that it
   tracks the noise.
4. **Code and memory layout (ASLR, link order) — plausible, not cleanly
   isolated.** One deliberate rebuild was performed (`touch
   crates/inlaysql-bench/src/main.rs && cargo build --release`, confirmed by
   MD5 that the resulting binary differs from the pre-rebuild one). The
   no-rebuild floor already established above (five runs of one unrebuilt
   binary, 20.4% spread / 7.3% CoV) shows meaningful process-level noise
   exists with zero rebuild involved — consistent with per-exec ASLR alone
   being a real contributor, exactly the Mytkowicz et al. mechanism the task
   brief cites. The three post-rebuild samples (620k–1.03M ops/s, 48.1%
   spread) were unfortunately confounded with the confirmed-heavy-load window
   above, so this session cannot cleanly separate "rebuilding added variance"
   from "this batch happened to run under much worse contention." Honest
   verdict: **not ruled out, not isolated — a clean rebuild-vs-no-rebuild
   comparison needs a quiet machine for both arms, which this session did not
   get.**

**Verdict:** of the four dimensions, background load/scheduling contention is
the one this session directly measured moving the metric (CoV roughly
tripling from confirmed-quiet to confirmed-busy); warm/cold cache is ruled
out by construction; CPU frequency and thermal state could not be measured at
all (no `sudo`); code/memory layout is plausible and contributes to the
already-nonzero no-rebuild floor but was not isolated from load in this
session's data. None of this contradicts the documented fsync drift being a
separate, already-explained cause for the *write*-side numbers — this section
is about the *read* path only, which does not fsync, matching the brief.

---

## 5. Order of work

1. **Profile first.** Section 2's ordering is inference from the code. A profile
   of the point-read path decides what is actually worth doing, and may well
   contradict the table above. Do not skip this to save an hour.
2. ~~Page cache~~ — **done (AHL-420)**. Warm, p50 6.75 µs → 459–500 ns, ahead of
   both SQLite configurations on latency *and* throughput. The reusable page
   buffer landed with it, so a miss allocates nothing already.
3. ~~The cheap allocation wins~~ — **done (AHL-422)**. `row_key` 28 ns → 1–2 ns,
   `check_schema` 24 ns → 11 ns, `Catalog::table` 16 ns → 4 ns: about 40 ns off
   a 500–600 ns path, **~7–8%**, which the benchmark cannot resolve cleanly —
   the before/after bands overlap. Nothing regressed. Recorded as a real but
   small win rather than a headline.

   Two things the profile changed about what comes next. **`memcmp` is ~21% of
   the path** — B-tree key comparison during descent, which was not in this
   file's original table at all, and is larger than `pread`. And
   **`PageCache::get` is ~9%**, paid on every *hit*: a `BTreeMap` lookup plus
   LRU list surgery. Those two are now the best-evidenced read-path targets.

   Also settled: a catalog **version stamp** for `check_schema` was considered
   and rejected. `Engine::refresh_catalog` replaces the catalog wholesale from
   `Catalog::decode`, so a per-instance counter can repeat across instances
   holding different schemas, and the fast path would then silently skip a real
   `Error::Stale`. Trading that guarantee for 6 ns is not a trade worth making.
4. ~~**Secondary B-tree indexes**~~ — **done (AHL-423)**, and it was the
   biggest *application-visible* win: `WHERE email = ?` is a range probe rather
   than a scan that decodes every row, which is what an ORM emits all day. The
   number is in `docs/architecture.md`; `bench/run.sh` grew the row that
   measures it with AHL-470 (`SUITE=indexed`), and `BENCHMARK.md` publishes
   both halves of it — the point probe wins, the range scan loses. Merging it with AHL-462
   put the probe *inside* the pipeline, so an indexed `LIMIT` fetches only the
   rows it returns.
5. ~~Cheaper key comparison~~ — **tried, and it was a wash; see "Prefix-skipping
   key comparison during descent, and why it was a wash" above.** The
   mechanism works (`memcmp` share did drop) but its own bookkeeping cost
   erases the gain on this join workload's dominant cost, which is
   re-descending per outer row rather than comparing many entries per descent.
   A cheaper cache hit (the ~9% `PageCache::get`) — the other candidate the
   AHL-422 profile justified — remains untried.
6. Index nested-loop join, then hash join.
7. ~~**Group commit.**~~ — **done (AHL-461, with the commit-gate rework of
   AHL-468 that gave it something to batch)**. Promoted on the evidence that
   with the container fsync asymmetry removed, MySQL and PostgreSQL wrote
   2–2.4x faster than we did (section 1). It paid where predicted: eight
   concurrent writers now reach 2.8x one writer rather than 1.45x, PostgreSQL
   is level (723.1 vs 730.9 ops/s containerised) and MySQL is 1.08x ahead
   rather than 2–2.4x. What group commit cannot touch is the single-connection
   shape, where there is nothing to batch by construction.
8. ~~Projection pushdown, then the streaming executor~~ — **done (AHL-462)**,
   and it moved the *scan* path rather than the point-read path. Point read is
   inside the noise band (`LOOKUPS=50000 SUITE=points ./bench/run.sh`, two runs
   each side on a contended machine: p50 542 ns before and after, throughput
   1.33–1.35M before against 1.42–1.49M after, p95 1.00–1.08 µs before against
   0.79–0.92 µs after — a real but small win, of the same size and the same
   unresolvable-by-the-benchmark shape as AHL-422's).

   What moved is what the benchmark suite does not measure: a `LIMIT` now ends
   the scan. `SELECT ... LIMIT 5` over a 2,000-row table reads **32 rows**
   rather than 2,000, and `LIMIT 500` reads 992 rather than 2,000 (the batch
   schedule doubles from 32 up to 512, so it overshoots by at most one batch).
   Those two are counted deterministically in
   `crates/inlaysql-core/tests/streaming.rs`, not timed. **There is still no
   scan, join or `LIMIT` row in `bench/run.sh`**, which is why this file quotes
   no timing for them — adding one is roadmap work, and this project's rule
   against publishing a join number before item 4 exists still stands.

   `ValueRef` proper is still open: see "the structural fix" in section 2 for
   what it did and did not remove.
9. Free list and vacuum — **after** the page cache, and it must version cache
   entries when it makes page ids reusable (`docs/architecture.md` D4).
10. Retrieval work from section 4.
11. One deliberate regeneration of every published number, on a quiet machine,
    in one sitting.

## 6. How to measure without fooling ourselves

- Benchmarks measured on a machine running other work are worthless. Check the
  load first; if it is contended, say the numbers are provisional.
- Report before *and* after from the same script on the same machine in the same
  session. Cross-session comparisons drift.
- Run each measurement more than once. One run is not a noise band — the Phase 0
  baseline spanned 6.50–8.63 µs on repeat runs of the identical binary.
- Publish losses. `README.md` already does this and it is the reason its numbers
  can be trusted at all.
- **Wait for the phase marker, don't time out past it.** `profile.rs`'s
  `PROFILE_QUERY_PHASE_START` protocol only works if the caller actually
  blocks until it appears; a polling loop with a fixed timeout that gives up
  and samples anyway will, on a large suite, sample the setup instead —
  bulk-loading 160,000 rows and building a B-tree index over them
  (`--suite joins`) took over 20 seconds locally, comfortably longer than a
  timeout that looked generous when it was written. The symptom is
  unmistakable once you check the call tree rather than trusting the
  top-of-stack summary: `fcntl`/`File::sync_all` (macOS's `F_FULLFSYNC`)
  dominating a profile of a *read-only* query loop is index-build commits,
  not query execution — a write encoder or a `CREATE INDEX`'s
  `build_btree_index` has no business in one, the same tell AHL-472's
  original contaminated profile had for a different reason. When in doubt,
  read the sample's actual call stack for the hot leaf before trusting its
  percentage.
- **Never report an A/B difference smaller than the suite's own A/A floor.**
  Section 4 measures that floor directly (CoV 3.6–7.3% on a quiet, gated
  machine; ~20% on this machine as normally shared) and it is the acceptance
  criterion this project now holds every future perf claim to. A one-minute
  load-average gate sampled only at the start of a run, as `bench/run.sh`
  does today, does not catch a spike that arrives mid-run — section 4's r≈0.18
  correlation between disclosed start-load and measured throughput is the
  evidence.
- **A "round-robin" sweep still confounds unless the order is re-randomised
  every round.** The in-container fsync curve above (§3, "The
  deferred-durability rejection, re-tested in-container") first swept N in
  fixed ascending order every round and measured a 32% increase, R²=0.91
  against bytes — exactly what the hypothesis under test predicted, and
  wrong: N=256 was always the *last* fsync of every round, so ordinary
  within-round drift landed on it every time and looked like a byte-count
  effect. Shuffling the order independently every round (not just varying it
  once per whole run) dropped R² to 0.017. Any sweep whose factor of interest
  also determines position-in-round needs the order re-randomised per round,
  not merely chosen once before the loop starts.
