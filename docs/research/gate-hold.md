# The reservation gate's hold, measured and then shrunk (AHL-563)

**Status: the measurement, then the design for the one shrink it justifies,
written before the code.** AHL-562 (`PERF.md`, 2026-09-05) closed by naming
the reservation gate as the constraint above two writers: `gate_wait` is
20/37/51% of a writer's commit latency at 4/8/16 writers, and a serialized
0.263 ms gate hold caps the file at ~3,800 commits/s against 2,850 observed.
It could not say what was inside that 0.263 ms. This document is that
number, split, and the argument for what may be moved.

Five measured negatives stand behind this area — AHL-544's flat, AHL-547's
0.90x, AHL-560's "already done", AHL-561's "not the syscall", AHL-562's "not
the election either" — and none of them is re-run here.

## 1. What is inside the hold

Two instruments, landed first and separately (`329d59d`):

* **Device-call attribution.** Every `Device::read` and `Device::write` a
  handle issues between `begin_normal_commit` and `end_normal_commit` is
  timed at the syscall and bucketed by the file's own layout — below
  `wal_start` (the state block), inside the log regions (the record append
  and a wrap's zero fill), at or past the data area (the dirty pages) — plus
  the preallocation slow path. `gate_hold_ns` minus the sum of those is,
  by construction, the in-gate CPU work.
* **`Device::gate_phase`,** a no-op default the core calls at each internal
  boundary of `CowBTree::commit`'s critical section. The native device
  charges the elapsed time to the phase that ended. Off unless
  `INLAYSQL_GATE_PHASES` is set.

The harness is AHL-562's own: `bench/flush_duty_cycle.sh` in the
`inlaysql-oltp` compose service, on its named btrfs volume,
`Durability::Full`, `--txns 150`.

**The device-call split, and the first surprise.** At sixteen writers the
0.251 ms hold is **15.9% device I/O and 84.1% not**:

| writers | hold ms | read | state | log | data | of which extend | device | residual |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 0.062 | 0.005 | 0.000 | 0.009 | 0.012 | 0.006 | 0.026 (41.5%) | 0.036 |
| 4 | 0.151 | 0.010 | 0.000 | 0.008 | 0.011 | 0.005 | 0.029 (19.0%) | 0.123 |
| 8 | 0.180 | 0.010 | 0.000 | 0.008 | 0.010 | 0.004 | 0.027 (15.2%) | 0.153 |
| 16 | 0.251 | 0.017 | 0.000 | 0.011 | 0.011 | 0.006 | 0.040 (15.9%) | 0.211 |

Milliseconds per hold. The WAL record append and the dirty-page `pwrite`s —
AHL-561's ~40 KiB per commit, the writes the retracted "shrink the gate"
proposal wanted to move outside — cost **22 µs of a 251 µs hold**. Whatever
the gate is spending its time on, it is not writing.

**The phase split says what it is.** Same run, sixteen writers, milliseconds
per hold and share of the marked total:

| phase | ms | share |
| --- | --- | --- |
| `gate_entry` | 0.000 | 0.0% |
| `commit_point` | 0.000 | 0.0% |
| **`read_state`** | **0.101** | **40.2%** |
| `rebase` | 0.022 | 8.6% |
| `free_list` | 0.000 | 0.0% |
| `materialize` | 0.002 | 0.9% |
| `encode` | 0.028 | 11.2% |
| **`scan_region`** | **0.032** | **12.9%** |
| **`wrap`** | **0.043** | **17.0%** |
| `data_writes` | 0.015 | 6.0% |
| `cohort` | 0.000 | 0.0% |
| `wal_append` | 0.008 | 3.1% |
| `tail` | 0.000 | 0.0% |

`read_state` is the span in which `CowBTree::commit` turns `cached` into
`(current_root, current_next, current_seq)`, and `scan_region` is the span in
which it turns `cached` into an append offset. Both are free when
`Device::commit_point` answered — and both are a full re-derivation from the
file when it did not: `read_committed_state` reads the state block and
replays the log, `wal::scan_region` walks a whole 1 MiB region decoding every
record.

**And it almost always answers.** The same run counts **124 commit-point
misses out of 2,403 gate holds — 5.2%.** Those 124 holds carry
`0.101 + 0.032 = 0.133` ms × 2,403 = **320 ms**, which is **2.6 ms per
miss**: ten times the mean hold, inside the process-wide critical section,
with every other writer queued behind it.

`wrap` is the third block and it is a different animal. 0.043 ms × 2,403 =
103 ms over the 52 region wraps the run performed (`gate_state_writes`
counts them) — **2.0 ms per wrap** — and the reason is in
`CowBTree::write_state_values`, which is a state-block write followed by
`Device::sync()`. **A WAL region wrap runs a full `fsync` inside the
reservation gate**, and then writes a megabyte of zeros. It is the shape
AHL-547 measured as a disaster when it put a cohort's barrier there, arriving
by a different route.

## 2. Where the misses come from

They are not random, and they are not a property of concurrency. They are
manufactured, one for each of the other three WAL regions, every time any
region wraps.

`CowBTree::commit`'s wrap branch calls `Device::set_commit_point(region,
None)` before it rewrites the state block and zeroes the region — "forget
before the writes rather than after, so a failure part-way leaves *unknown*
rather than *wrong*", which is right. But on `FileDevice` that call means:

```rust
// Forgetting is deliberately total: the caller only knows that *its* region
// moved under it, but it stopped part-way through a sequence the committed
// state itself depends on, so the honest answer everywhere is "read the file".
None => *gate = GateCache::default(),
```

— it discards the file-wide `state` **and all four regions' append offsets**.
The wrapping writer republishes `state` and *its own* region's offset before
it leaves the gate. The other three are simply gone, and the next commit to
land in each of them pays a full re-derivation inside its own gate hold.

The arithmetic checks out at every writer count: 8 wraps → 22 misses, 20 →
51, 52 → 124. Between 2.4 and 2.8 induced misses per wrap, against a
theoretical 3.

Putting the two together, **one region wrap costs the file roughly
2.0 + 2.4 × 2.6 = 8.2 ms of serialized gate time**, and at sixteen writers
the 52 wraps account for `103 + 320 = 423` ms of the run's `2,403 × 0.251 =
603` ms of total gate hold — **70% of it.** The per-commit work the gate
exists for is the remaining 30%: rebase 0.022, encode 0.028, materialize
0.002, the writes 0.023, the append 0.008 — about 0.083 ms.

Wraps are frequent because InlaySQL's records are large: a single-row commit
writes ~20 KiB of WAL record (AHL-561), a region is 256 × 4 KiB = 1 MiB, so a
region wraps every ~52 commits and the file wraps every ~46. That record size
is the deeper problem and it is not this item's.

## 3. What genuinely needs the exclusion, and what is merely inside it

The gate orders three things, and only three:

1. **The commit sequence.** `current_seq + 1` must be unique and increasing.
2. **The root handoff.** Each commit builds on the root the previous one
   published, and publishes its own.
3. **`rebase_pending`'s comparison against the latest committed root.** This
   is first-committer-wins, and it walks pages reachable from `current_root`,
   so those pages must already be on the device.

(3) is what makes the retracted proposal unsafe, and that retraction stands
unconditionally here. `docs/research/commit-group-logical.md` §1 states the
counterexample: if a writer releases the gate before its dirty pages are
`pwrite`-landed, the next writer's `rebase_pending` walks a root naming page
ids whose bytes are not there yet, and answers a conflict question about a
tree that does not exist. **This design does not defer any write past gate
release, and it does not reserve an offset inside the gate to write outside
it either.** Both of those are proposals about the writes, and the
measurement above says the writes are 22 µs of a 251 µs hold — there is
nothing there worth the risk even if it were safe.

What is *merely* inside the gate is the third thing: **a process-local cache
invalidation whose scope is wider than the event that provoked it.** A wrap
of region *r* changes exactly one fact — where region *r*'s next record goes.
It does not change the committed root, the next page id, the sequence number,
or where any *other* region's next record goes. Discarding those is not
required by anything; it is a conservative default written for a different
caller.

## 4. The change

Add one `Device` method whose **default is today's behaviour**, so a device
that does not override it cannot be made less safe by this change:

```rust
/// Forget only where `region`'s next record goes, keeping the rest of any
/// cached commit point. The default is the total forget, so a device that
/// has not thought about the distinction keeps the conservative answer.
fn forget_append_offset(&self, region: usize) {
    self.set_commit_point(region, None);
}
```

`FileDevice` overrides it to clear `gate.append[region]` and nothing else.
`CowBTree::commit`'s wrap branch calls it instead of
`set_commit_point(region, None)`. **Every other `set_commit_point(_, None)`
call site is untouched**, including the two that matter most: the `!written`
failure path in `commit`, and `CowBTree::checkpoint`'s own forget. Those are
the cases the total forget was written for — a commit that stopped part-way
through a sequence the committed state depends on — and they stay total.

### Why it is safe

Four claims, each about a fact rather than an intention.

**(a) Nothing a reader can now see was invisible to it a moment earlier.**
During the window between the forget and the republish, a thread that is not
the gate holder can call `Device::commit_point` — `refill_free_candidates`
does, outside the gate. Today it sees `None`. After this change it sees
`state` and the three untouched regions' offsets: **exactly the values it
would have seen one instruction before the wrap began**, because a wrap
changes none of them. There is no value this makes observable that was not
already observable at the immediately preceding instant.

**(b) The one fact the wrap does change is still forgotten.** `append[r]`
is cleared, before the zeroing write, on the same instruction as today. A
reader in region *r* gets `None` and re-derives, which is the correct and
unchanged answer.

**(c) A failed wrap still ends in a total forget.** If either
`write_state_values` or the zeroing write fails, the `prepared` closure
returns `Err`, `written` is false, and `commit`'s existing
`set_commit_point(region, None)` runs before the gate is released. The
narrow forget is an *additional* narrowing inside a hold that still ends
totally on every failure path. This is the belt-and-braces property that
makes the change reviewable: the failure behaviour is bit-identical.

**(d) It cannot loosen the free list.** `refill_free_candidates` offers a
page only when `freed_at < min(point.seq, min_reader)`. `point.seq` after
this change is the pre-wrap committed sequence — the same number it was
before — so no page becomes eligible that was not eligible a moment earlier.
The change is a liveness difference (fewer declines), never a safety one.

### What it does not do

It does not remove the wrap's own 2.0 ms, which is a real `fsync` that must
happen before a region is reused (`FileDevice::sync`'s doc comment gives the
argument, and `docs/recovery.md` gives the loss bound it protects). Moving
*that* barrier out of the gate is the retracted proposal in a new costume and
is not proposed. What this change removes is the 2.4 extra re-derivations the
wrap causes in *other* regions, which is the larger of the two terms.

### The ceiling, computed rather than hoped

Hold `wrap` and the per-commit work fixed and drive the induced misses to
zero: the sixteen-writer hold goes 0.251 → 0.251 − 0.133 = **0.118 ms**, and
the serialized ceiling `1 / hold` goes 3,984 → **8,475 commits/s** against
2,653–2,908 measured. That is a ceiling on an idealisation, not a forecast —
AHL-562's finding was that the *flush* side becomes the constraint again once
the gate stops being one, and this item does not claim otherwise. The
deliverables are the hold, its split, and `gate_wait`'s share; throughput is
reported paired against an A/A row and is expected to move less than the
ceiling, or not at all.

## 5. Tests, and the mutation each one fails

| Test | What it pins | Mutation that fails it |
| --- | --- | --- |
| `forgetting_an_append_offset_leaves_the_other_regions_and_the_state` | the narrow forget clears one slot and nothing else | the total-forget fallback; clearing nothing; clearing the wrong region |
| `the_total_forget_is_still_total` | every failure path keeps the wide meaning | narrowing `set_commit_point(_, None)` itself |
| `a_region_wrap_costs_no_other_region_a_re_derivation` | after a wrap, a commit in a *different* region still finds a cached point — asserted on `gate_point_misses`, so it fails on the bug and not on a timing | the total-forget fallback |
| `a_wrap_that_fails_at_any_write_forgets_the_whole_cache` | a wrap broken at *every* write it issues leaves no cached point and a readable prefix | removing the `!written` total forget |
| `every_row_survives_repeated_region_wraps` | four handles, four regions, every region wrapped, reopened cold | data-safety backstop |
| `concurrent_writers` + all five DST sweeps, both arms | the whole protocol, thousands of seeds | any of the above |

**One mutation is checked and equivalent rather than missed**, and it is
worth writing down: deleting the wrap's forget *entirely* fails nothing. The
success path republishes the offset before the gate is released and the
failure path forgets everything, so the only thread that could see a stale
offset is an out-of-gate reader — and neither `refill_free_candidates` nor
`resolve_state_at_least` reads `append_offset`. The forget is kept as defence
in depth. Note also that **there is no crash injection at a new step because
there is no new step**: the wrap's write sequence is byte-identical before and
after, and the diff moves one in-memory cache line and nothing that reaches
the file.

## 6. Order of work

1. The instruments and this document. (`329d59d`, and this file.)
2. `Device::forget_append_offset`, the `FileDevice` override, the one call
   site.
3. The tests above, each mutation-checked.
4. The paired before/after measurement with its A/A row, and `PERF.md`.

**Outcome, added after the fact:** the gate hold halved at four writers and
up — 0.262 → 0.121 ms at sixteen, paired ratios 0.45–0.50, six of six —
commit-point misses went 132 → 3, and `read_state` and `scan_region` went to
exactly zero while every other phase held still. `bench/gate_hold.sh`
regenerates it and `PERF.md`'s AHL-563 section is the record, including the
one-writer A/A row (0 misses in *both* arms) that the ratios have to be read
against.
