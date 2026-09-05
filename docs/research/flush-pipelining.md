# Flush pipelining — overlapping the gather with the barrier, and handing leadership over instead of re-electing it (AHL-562)

**Status: RETRACTED (AHL-566, 2026-09-06). The code is deleted; this document
is kept as the record of why.** It was built, it worked, it engaged on 83–92%
of barriers, and the duty cycle did not move (AHL-562). AHL-562 blamed the
reservation gate and left the code behind a default-off flag with a condition
for turning it on. AHL-563, AHL-564 and AHL-565 then met that condition —
the gate hold fell 0.263 → 0.085 ms, `gate_wait` 51.0% → 28.1%, commits per
barrier 8.29 → 12.84, the duty cycle 44.3% → 52.5% — and **AHL-566 re-ran this
experiment against a real A/A control (`bench/aa_floor.sh`) and it is still
flat**, while engaging on 93–98% of barriers with 95% of the gather running
under the previous `fsync`. The one effect outside the control's noise band is
the two-writer cohort truncation §2 predicted, which is a loss. `PERF.md`'s
AHL-566 section has the numbers and the floor.

Everything below is the design as it was written, unedited except for §7,
whose mutation table indexed tests that no longer exist. Read it for the
ticket-to-barrier proof in §3 — the clearest statement of the durability rule
in this repository, and true whether or not anyone ever pipelines the flush —
and read it before having this idea again.

---

**Status: the design, written before the code, for the lever AHL-561 priced
and did not build.** AHL-561 (`PERF.md`, 2026-09-05) measured both engines'
barriers on the same volume and found them the same price — InlaySQL's
`fsync` is 1.322 ms against MySQL's 1.215 ms, a 1.09x gap — and found the
whole of the difference in the **duty cycle: 51% against 96%**. InlaySQL's
2.575 ms barrier cycle accounts, to 99.5%, as

```
1.322 ms fsync + 0.617 ms gather + 0.592 ms inter-cycle gap + 0.03 ms post
```

so **half of every cycle has no flush in flight at all** while writers wait.
This document is the design that removes the two idle segments without
touching the durability rule, and — equally the point — the argument for why
it cannot break that rule, written down *before* any code, because the
failure mode of getting it wrong is silent data loss rather than a wrong
answer or a crash.

Four measured negatives stand behind it and none of them is repeated here.
AHL-544: cohorts form and throughput is flat, because every follower still
took the gate, appended and synced. AHL-547: the *commit-side* cohort
(`docs/research/commit-group-slice2.md`) removed all three and measured
**0.90x**, because the cohort leader's gate hold and the flush leader's
gather window compete for the same writers — the two group-commit layers
compose or they cannibalise. AHL-560: the seam it named was already closed.
AHL-561: the barrier is not the syscall's fault. This item is deliberately
the *opposite* direction from AHL-547 — it does not add a second cohort, it
makes the **existing** flush-side gather overlap the barrier that follows it.

## 1. The sequence today

Every reference below is to `crates/inlaysql/src/device.rs`. A normal user
commit runs, in order:

1. `Device::begin_normal_commit` — takes the reservation gate
   (`CommitCoordinator::reserved` + `reservation_done`), `normal_waiters` for
   the queue, `normal_inflight` for the hold.
2. The engine rebases, encodes its WAL record and `pwrite`s the record and
   its dirty data pages through `Device::write`.
3. `Device::commit_ready` — `ticket = writes_completed.fetch_add(1, SeqCst) + 1`,
   published **after** every `pwrite` for this commit returned, and while the
   gate is still held.
4. `Device::end_normal_commit` — bumps `generation`, releases the gate, wakes
   one queued writer.
5. `Device::sync_commit` → `CommitCoordinator::make_commit_durable(ticket, ..)`
   → `make_durable_with_cohort(ticket, coalesce_normal_commits = true, ..)`.

Step 5 is the whole of this document. It is a loop:

* If `durable_upto >= ticket`, return — somebody else's barrier already
  covered this commit, and it never touches the disk.
* Take `flush`. If `FlushState::in_progress`, become a **follower**: wait on
  `flush_done` until `in_progress` clears *and* `epoch` moves, then loop back
  and re-check.
* Otherwise become the **leader**: set `in_progress = true`, drop the lock,
  charge the elapsed time since the last cycle end to `gap_ns`, arm
  `LeaderGuard`, run `coalesce_normal_commits()` (charged to
  `gather_spin_ns`), load `durable_before` and `target = writes_completed`,
  call `sync()` (charged to `fsync_ns`), and on success
  `durable_upto.fetch_max(target)`.
* `LeaderGuard::drop` (charged to `post_ns`) re-takes `flush`, clears
  `in_progress`, bumps `epoch`, drops the lock, `flush_done.notify_all()`,
  and stamps `last_cycle_end_ns`.

Checkpoints (`Device::sync`) reach the same function through `make_durable`
with `coalesce_normal_commits = false`: no gather window, because a
checkpoint may be *holding* the reservation gate and gathering would wait on
writers that cannot make progress.

**So where do AHL-561's two idle segments actually go?**

*The 0.617 ms gather* is `coalesce_normal_commits`. It yields the CPU while
a normal commit is inflight or queued **and** `writes_completed` keeps
advancing, closing after `COMMIT_COALESCE_STALL_YIELDS = 1500` consecutive
polls with no new ticket, or when nothing is inflight or queued, or after
`COMMIT_COALESCE_MAX_YIELDS = 16384` turns. It is deliberate and it is *not*
waste: it is what buys 3.7 commits per barrier at eight writers, and its doc
comment gives the reason it runs where it does — during this window no
`fsync` is in flight, so each gathered writer's `pwrite` runs at the fast
rate instead of the 18–23x slower rate a write pays racing a barrier on the
same file. At one connection it is **exactly zero** (AHL-561's one-writer
table), which confirms the solo fast path returns before taking a single
scheduler turn.

*The 0.592 ms gap* is the re-election. `LeaderGuard::drop` wakes **every**
follower with `notify_all`; they all contend for the `flush` mutex; each
re-checks `durable_upto`; the ones the finished barrier covered return; the
ones it did not (their ticket exceeded that barrier's `target`) fall to the
bottom of the loop, one of them wins the election, and the rest go back to
sleep. Nothing is being flushed for any of it. AHL-561 measured this segment
at 0.165–0.190 ms with a *single* writer and no herd at all, and at
0.44–0.46 ms at four and 0.88–0.98 ms at eight — it is a fixed handoff cost
plus a thundering-herd cost that grows with concurrency.

Both segments sit **between** barriers. Neither is work that has to be there.

## 2. The sequence after

Two changes, one for each segment, and they are the same mechanism seen
twice: *the next leader should already exist, and should already be ready,
when the current barrier returns.*

**A. A successor is elected during the barrier, and gathers during it.**
A committer that arrives at `make_durable_with_cohort`, finds `in_progress`
set, and finds no successor claimed, claims the successor slot instead of
parking as a follower. It drops the `flush` lock, runs its gather window
**concurrently with the in-flight `fsync`**, then re-takes the lock and waits
for the handoff.

**B. Leadership is handed over, not re-contested.** `LeaderGuard::drop`, with
the `flush` lock in hand and before waking anybody, checks for a claimed
successor. If there is one it **keeps `in_progress` set** — the round is
reserved for the successor, so no other thread can elect itself and no gap
can open — sets `handoff`, bumps `epoch`, and wakes the successor on its own
condvar (`successor_wake`) as well as the followers on `flush_done`. The
successor wakes already gathered, consumes the handoff, captures its target
and calls `sync()` immediately. No election, no herd race, no second gather.

The cycle becomes:

```
| ------------- fsync N ------------- | wake | ---------- fsync N+1 ---------- |
        [ successor N+1 gathers ]              [ successor N+2 gathers ]
```

**One flush is in flight at a time, exactly as today.** Only the election and
the gather moved.

### The bound the gather needs, and why it is load-bearing

`coalesce_normal_commits` as written can run for up to 16,384 yields. Run
unmodified during a barrier it could easily outlive that barrier and *delay*
the next one — the exact opposite of the point. So the pipelined gather takes
a stop condition: it returns immediately once the handoff is pending. The
overlapped gather is therefore bounded by the in-flight barrier by
construction and can never extend the cycle. The un-pipelined caller passes a
stop condition that is always false, and its behaviour is byte-for-byte
today's.

### What this costs, named in advance

The writers gathered during an in-flight `fsync` pay the **18–23x slower
`pwrite` racing a concurrent flush** penalty that `coalesce_normal_commits`'s
own doc comment names and prices. That is the honest cost of moving the
gather under the barrier, and it is measured, not argued: the coordinator
already splits `gate_hold_racing_ns` and `gate_hold_racing_start_ns` out of
`gate_hold_ns` precisely to see it, and the concurrency suite prints commits
per barrier. **If the cohort collapses or the gate hold inflates enough to
eat the duty-cycle win, the answer is a negative and the design document is
what lands** — this is the fifth measurement in this area and it is written
to be falsifiable.

The second named cost is the truncated gather: a successor that claims late
in a barrier gathers for whatever is left of it, which may be nothing.
Today's leader would have gathered for a full stall window on the critical
path instead. The deliberate choice is **not to re-gather after the
handoff** — that would put the segment back where it was — and to let
commits-per-barrier fall if it must, because throughput, not cohort size, is
the deliverable. The fallback, if measurement demands one, is a floor: gather
after the handoff only if the overlapped window was shorter than some
fraction of the barrier. It is not in this design.

## 3. The ticket-to-barrier mapping — the part that must be right

This is the durability argument, in full, because it is the one thing whose
failure is silent.

**Definitions.** A ticket `t` is *published* by the `SeqCst`
`writes_completed.fetch_add` in `commit_ready` (or `FileDevice::sync`),
which happens strictly after every `pwrite` for that commit returned — a
`pwrite` is synchronous, so on return its bytes are in the kernel's page
cache and visible to any later `fsync` on any descriptor open on that file.
Call that moment `p(t)`. A barrier round `k` loads `target_k =
writes_completed.load(SeqCst)` at moment `c_k` and calls `sync()` at moment
`s_k`, returning at `e_k`; on success it runs
`durable_upto.fetch_max(target_k)`.

**Rule R (the one the engine promises).** `durable_upto` reaches `t` only via
some round `k` with `t <= target_k` and `p(t) < c_k < s_k` and `sync()`
returned `Ok` at `e_k`.

**Proof, and why pipelining does not disturb it.** `writes_completed` is
monotone and every publication is a `SeqCst` read-modify-write; `target_k` is
a `SeqCst` load. So `t <= target_k` implies `p(t)` precedes `c_k` in the
single total order over `SeqCst` operations. Within a round the program order
is fixed and unchanged by this design: **load `target`, then `sync()`, then
`fetch_max`** — pipelining does not move, reorder or duplicate any of those
three. `fetch_max` is what makes a slow round finishing after a fast one
unable to move the watermark backwards, and it too is unchanged. Therefore
every ticket the watermark covers was written before a barrier that started
after that write and returned success. ∎

**The four ways this design could have broken it, and why each cannot.**

1. *The successor gathers during round `k` — could its members be credited to
   round `k`?* No. Round `k` captured `target_k` at `c_k`, before `s_k`, and
   never reads `writes_completed` again. Every ticket published during round
   `k`'s `sync` is strictly greater than `target_k`, so
   `fetch_max(target_k)` cannot cover it. Those tickets are covered by round
   `k+1`, whose `c_{k+1}` is after the handoff, which is after `e_k`. This is
   already true today — the gather moving is what makes it *matter*, not what
   makes it true.
2. *The successor holds a reserved round (`in_progress` stays set across the
   handoff) — can it acknowledge anything on the strength of the round it
   inherited?* No. It inherits the *role*, never a target and never a
   watermark. It loads its own `target` after taking the handoff and syncs
   after that. The reserved `in_progress` is a mutual-exclusion token, not a
   durability claim.
3. *The successor finds itself already covered when it wakes — can it return
   `Ok` on the strength of the barrier it did not run?* Yes, and that is
   sound: `durable_upto >= ticket` is Rule R's own conclusion, established by
   whichever round moved the watermark. It is the same check every follower
   already makes. What it must additionally do is **release the reserved
   round**, which is a liveness obligation (§4), not a durability one.
4. *Could two barriers ever be in flight at once and race the watermark?* No
   — `in_progress` is never clear between the leader's drop and the
   successor's take, so the successor is the only thread that can be leading.
   And even if two ever were, `fetch_max` with per-round targets captured
   before each round's own `sync` keeps Rule R true; overlapping `fsync`s
   were already sound in this file's design and this change does not start
   relying on that.

**`Durability::Normal` and `Full` are untouched.** The barrier *strength* is
chosen in `sync_commit` from `effective_durability()` and passed as the
`sync` closure; this design changes only who runs the closure and when.
`Device::sync` (checkpoints) keeps `coalesce_normal_commits = false`, never
gathers, and may still be a successor or a follower — for a checkpoint the
successor path is a pure directed handoff with no gather, which is exactly
what it does today minus the election race.

## 4. Every crash and failure point, enumerated

"Crash" below means process death at that instant; "panic" means an unwind
out of the marked frame.

| # | Point | What has been acknowledged | Outcome |
| --- | --- | --- | --- |
| 1 | Crash while the successor is gathering | nothing from this round or the next | Recovery lands on the last WAL prefix whose records precede a completed barrier. Unchanged from today. |
| 2 | Crash after leader `sync()` returns, before `fetch_max` | nothing | Data is on the device but no caller was told so. Weaker than the truth, which is the safe direction. |
| 3 | Crash after the leader set `handoff`, before the successor takes it | nothing new | Nothing was acknowledged on the strength of the reserved round. |
| 4 | Crash after the successor loaded `target`, before its `sync` | nothing | As #1. |
| 5 | Panic of the successor while gathering | nothing | `SuccessorGuard::drop` clears the claim and, if the handoff had already been set, releases the reserved round (`in_progress = false`, `epoch += 1`, `notify_all`) so the next arrival elects normally. |
| 6 | Panic of the successor after taking the handoff | nothing | It holds a `LeaderGuard` from the moment it takes the round, so the round ends exactly as a panicking leader's does today. |
| 7 | Panic inside the leader's `sync` | nothing | `LeaderGuard::drop` runs on the unwind, and now performs the handoff as well — so a panicking leader hands a *live* round to a successor rather than stranding it. |
| 8 | Leader's `sync` returns `Err` | nothing | `durable_upto` does not move. The leader returns the error to its caller. Uncovered followers and the successor go on to run their own barrier, which starts after their writes — Rule R holds for them. Covered by the existing test `a_failed_leader_flush_still_wakes_followers_who_then_fsync_for_themselves`. |
| 9 | Successor wakes covered | its own commit, correctly (§3.3) | It releases the reserved round through its `LeaderGuard`, which wakes the next successor or follower. |
| 10 | No successor exists when the leader drops | — | Exactly today's path: clear `in_progress`, bump `epoch`, `notify_all`. |
| 11 | A checkpoint is the successor | nothing until its own barrier | No gather (its `coalesce_normal_commits` is false); pure handoff. |
| 12 | Lost wakeup: successor claims, leader drops before the successor parks | nothing | The successor re-takes the lock and checks `handoff` **before** waiting; the flag, not the wakeup, is the state. A wakeup that arrives before the wait is therefore not lost. |
| 13 | Successor claims, the round it meant to follow ends with no handoff | nothing | It clears its claim under the lock and loops back to the top, where it elects or follows normally. |

## 5. What lands, and behind what

The change is in `CommitCoordinator` only. No format change, no core change,
no `Device` trait change, no server change.

* `FlushState` gains `successor: bool` and `handoff: bool`; the coordinator
  gains a `successor_wake: Condvar`, a `handoff_pending: AtomicBool` (the
  gather's stop condition, readable without the mutex), and a
  `pipeline: AtomicBool`.
* `coalesce_normal_commits` gains a stop closure. Today's caller passes one
  that never fires and is unchanged.
* Two diagnostic counters: `overlap_gather_ns` (gather that ran inside a
  barrier — deliberately **not** added to `gather_spin_ns`, so that
  `gather + fsync + post + gap` stays a decomposition of one cycle and the
  AHL-561 accounting identity remains checkable) and `handoffs` (rounds
  entered by handoff rather than election). Both reach the benchmark through
  `CommitStats`.

**Default off, behind `INLAYSQL_FLUSH_PIPELINE=1`** (removed with the code
in AHL-566), read once per
coordinator at construction. The durability contract is identical either way
— that is §3 — but the *order in which concurrent writers are acknowledged*
can change, and a caller with two connections can observe that ordering.
That is enough to owe the flag. It is also what makes the measurement honest:
both arms are the same binary, so a paired, interleaved before/after is one
environment variable apart with no rebuild between them, which is what
AHL-561's §4 measurement floor demands.

## 6. How it will be judged

AHL-561 computed the ceiling rather than guessing it: hold `fsync` and
commits-per-barrier at their measured values and drive both idle segments to
zero, and the cycle goes **2.575 → 1.322 ms**, the barrier rate 388 → 756/s,
throughput 1,392 → ~2,710 ops/s, and the server-to-server gap 2.20x → 1.13x.
Removing only the gap and leaving the gather alone is the conservative half:
2.575 → 1.983 ms, ~1,807 ops/s, 2.20x → 1.70x.

Neither is a forecast. This design cannot reach the first, because a directed
handoff still costs one thread wakeup, and AHL-561 measured that floor
directly: **0.165–0.190 ms** at one writer, where there is no herd to race.
So the honest target is `1.322 + ~0.17 ≈ 1.49 ms` per cycle, a duty cycle
near **89%**, if and only if the overlapped gather keeps commits per barrier
where it is. Every one of those three numbers — cycle, duty, commits per
barrier — is printed by the `barrier cycle` line AHL-561 added to
`crates/inlaysql-bench/src/concurrency.rs`, at 1, 2, 4, 8 and 16 writers,
paired and interleaved, before and after.

The tests that have to pass before any of that is worth reading are in §3 and
§4: a property test that no ticket is ever reported durable by a barrier that
started before its write, an injected failure at every step of the handoff,
and the existing group-commit concurrency tests unchanged.
