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

**What is owed.** Two measurements, and one of them is cheap:

1. Re-run the whole containerised comparison **in one session, interleaved**,
   InlaySQL and PostgreSQL and MySQL and the raw `pwrite`+`fsync` floor
   alternating, on a quiet machine. Until that exists, no ordering in that
   table should be believed — including the ones that flatter us.
2. If the write path is picked up again, the target is the **6.5 pages**, not
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
(the same AHL-488/493 allocation cost, still open, not this).

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
- `resolve_value_at` cloning the value out of the cached page — **untouched**.
  This is the one a real `ValueRef` is for.

Two more copies went with them: `aggregate` borrowed its group instead of
cloning every row into a second `Vec`, and `sort_rows` moves rows through the
keyed form instead of cloning them twice.

What is left is the invasive part, and it is still the single largest remaining
win: an internal borrowed `ValueRef<'a>` the executor uses, with owned `Value`
materialised only at the public API boundary. That turns step 6 from "allocate
per cell" into "slice into the cached page". `eval.rs`, `engine.rs` and `plan.rs`
all assume owned values, so it is a change of its own — and now that the
pipeline is iterators with one owner per row, it is a smaller one than it was.

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
the leader captured its flush target, and that is the next thing on this path.

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

---

## 4. Retrieval

Already winning where it is measured: ~15.8x over `sqlite-vec` at 100k vectors
(7.56x on the 2,000-vector suite `BENCHMARK.md` publishes), and ~60x over
DuckDB and ~74x over pgvector on hybrid, because hybrid is one statement here
and two queries plus client-side fusion there.

**The BM25 leg is no longer the expensive half.** It was 79% of the hybrid p50
(347.50 µs of 453.88 µs at 2,000 documents); an inverted-index layout with
dense document ordinals, a bounded top-`k` heap and a MaxScore walk took it to
47.75 µs of a 95.17 µs hybrid — 50%, with the vector leg now the larger share.
Scores and ranking are unchanged, ties included.

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
before anything is written; `bin/profile.rs` does not cover the retrieval suite
yet, and adding it is the first step.

The avenues below are the ones that would widen the pgvector margin, in order
of expected value, now reordered by the above:
1. **The traversal, not the kernel.** Half the query is heap and set
   bookkeeping around the distance calls. Profile it first.
2. **Memory layout.** Neighbour lists and vectors laid out for sequential access
   during a graph walk, so the prefetcher works for us.
3. **Quantised distance kernels.** `VECTOR(n, INT8)` already shrinks storage 4x;
   computing distances *in* int8, rather than converting to `f32` first, makes
   the memory-bandwidth win a compute win too. Note the int8 path currently
   measures *slower* than exact (155.21 µs against 88.29 µs on the published
   suite), so this is a repair before it is an optimisation.
4. **Quantised paged nodes**  — `PagedHnswIndex` stores exact
   `f32` even for an int8 column, so the paged path currently forfeits the 4x.
5. ~~**Filter-aware walks** instead of over-fetching.~~ — **done.** The
   `WHERE` is compiled into a row predicate and pushed into the index walk:
   rejected rows are traversed but neither returned nor counted, so a
   selective filter no longer widens the probe in geometric re-runs. See
   `Engine::retrieve_filtered`.

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
