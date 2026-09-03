# C1 slice 1 — rebase-only absorption: the code plan (AHL-544)

**Status: the plan for the first landable slice of `docs/research/commit-group-logical.md`,
written before the code.** It fixes exactly what changes, what each step is
allowed to assume, and which DST scenarios have to exist before any of it is
believed. The brief (AHL-543) is the design; this is the diff.

The slice is the brief's **Section 6, Slice 2** ("rebase-only absorption, no
WAL change"). Everything Slice 3 and beyond would add — the leader owning
another handle's WAL bytes, per-follower outcome slots, ticket batching — is
explicitly *not* here, and the shape below is chosen so that none of it is
made harder later.

## 0. What lands, in one paragraph

A writer that reaches `CowBTree::commit` **offers** its open transaction (its
committed snapshot root and its `pending_ops` map, moved out of the handle,
not copied) to the device, then parks on the reservation gate exactly as it
does today. Whichever writer holds the gate, having already published its own
`CommitPoint`, takes every offered transaction in gate-arrival order and runs
**only `rebase_pending`'s comparison** over each — against its own
post-rebase root for the first, against that root plus every earlier
absorbed member's own operations for the ones after it. It writes back one
`Clean`/`Conflict` decision per member and releases the gate. Each follower
then wakes, re-enters the gate on its own as it always has, **validates that
the file's committed state is exactly the one its decision was computed
against**, and — if it is — skips the comparison it would otherwise redo and
goes straight into the unchanged replay/`finalize_free_list`/`encode`/
`write`/`commit_ready`/`sync_commit` path with its own record, its own
region, its own ticket and its own sync. If the validation fails for any
reason at all, the follower does the ordinary full `rebase_pending` and
nothing about its commit differs from today.

No WAL record changes. No change to who writes which region. No outcome is
acknowledged before that writer's own sync — every follower still runs its
own `sync_commit`. The whole thing is behind
`EngineOptions::commit_absorption`, **default `false`**.

## 1. Why this shape and not the brief's literal wording

The brief says the leader "rebases" the followers, writing the rebased result
back into each follower's own `CowBTree` state. That is not implementable as
written, and the reason is worth recording because it constrains Slice 3 too:
**a `CowBTree` is `!Send`.** It holds `Rc<Node>` in `dirty`, an
`Rc`-refcounted page cache, and three `RefCell` cursors. A leader thread
cannot touch a follower's handle at all, let alone mutate its `pending_root`
and `dirty`.

What the leader *can* touch is the part of a transaction that is plain data:
`root: PageId` and `pending_ops: BTreeMap<Vec<u8>, Option<Vec<u8>>>`, both
`Send`. And that is precisely the input to the half of `rebase_pending` that
is correctness-critical:

```rust
for key in self.pending_ops.keys() {
    if self.get_at(self.root, key)? != self.get_at(current_root, key)?
        && !mergeable_metadata_key(key) { return Ok(false); }
}
```

Both `get_at` calls are *committed* reads (`pending == false`) of roots that
live on the **shared file**. The leader's own handle can serve both. The
second half of `rebase_pending` — clearing `dirty`, adopting the new root and
replaying the ops through `put`/`delete` — needs the follower's tree and
stays on the follower's thread, where it always was.

So the split is: **the leader computes the decision; the follower performs
the rebase.** That is a strictly smaller change than the brief's wording and
delivers the brief's actual objective for this slice — "isolate the one
correctness-critical piece (conflict ordering) from the higher-risk pieces" —
because conflict ordering *is* the decision.

### The second problem: the base root a follower is rebased against does not exist yet

Member 2's decision must be evaluated against member 1's *post-rebase* root
(brief, I1). Under this slice member 1 has not committed when the leader
decides — no pages, no root. Materialising it would mean the leader running
the replay, which needs member 1's tree, which is the problem above.

It does not have to be materialised. `rebase_pending`'s replay applies
exactly `ops_1` on top of `root_L`, so for any key `k`:

```
get_at(root_1, k) == ops_1.get(k)          when k ∈ ops_1
get_at(root_1, k) == get_at(root_L, k)     otherwise
```

The leader therefore evaluates member *j* against a **logical overlay**: the
committed root it just published, with every earlier *clean* member's ops
layered over it in order. A conflicting member contributes nothing to the
overlay and does not advance the chain — the brief's step 5, unchanged.

**Why the overlay is exact and not an approximation**, key class by key
class:

| Keys in a member's `pending_ops` | Handled how |
| --- | --- |
| Ordinary rows, `\x01idx:` index entries | Overlay answers exactly what `get_at(root_j-1, k)` would: an earlier member's `put` stores the value verbatim and `get_at` returns it. |
| `\0next_row_id`, `\0write_version`, `\0cdc_floor`, `\0cdc:*` | `mergeable_metadata_key` skips them in the comparison, today and here. Their *value* under the overlay is never consulted, so `merge_monotonic_metadata`'s rewrite (which the follower still runs itself, unchanged, against the real root) cannot diverge. |
| `\x02free\0…` free-list rows | An earlier member writes these during its own `finalize_free_list`, which runs **after** its rebase — so they are in the real `root_j-1` and *not* in the overlay. A transaction's `pending_ops` never contains one at gate-arrival time (they are only ever added inside `commit`, and `pending_ops` is cleared when it returns), so the divergence is unobservable. **The implementation does not rely on that argument: any offered transaction carrying a `FREE_LIST_PREFIX` key is refused absorption outright and falls back.** A cheap, total guard beats a proof about who writes which key. |

### The third problem: the decision can go stale between the leader releasing the gate and the follower acquiring it

Nothing orders the gate handoff. A follower can be overtaken by a writer that
was never in the cohort, by a checkpoint, or by another cohort member; a
cohort member with a `Clean` decision can fail on a device error and never
commit at all. Any of these makes an already-computed decision an answer to a
question about a state the file is no longer in — the brief's I1 violated in
the only way this slice can violate it.

The fix is a **seal**: a three-field token, held by the device, written and
read only under the gate.

```rust
pub struct AbsorbSeal { pub cohort: u64, pub index: u32, pub seq: u64 }
```

* The leader publishes `{cohort: C, index: 1, seq: seq_L}` after its own
  `set_commit_point` — "the file's committed state is exactly cohort `C`
  through member 0, at sequence `seq_L`".
* Member *j* is handed `expect = {C, j, seq_after_member_(j-1)}` with its
  decision. On gate entry it uses the decision **only if
  `device.absorption_seal() == Some(expect)`**.
* A member that acted on its decision republishes `{C, j+1, seq}` — `seq+1`
  when it committed, unchanged when it conflicted (a conflict changes nothing
  on disk but does advance the chain position).
* **Every other event that can change the committed state publishes `None`**:
  a successful commit that did not act on a decision, a conflict that did not,
  a `checkpoint`, and any error inside the gate. `None` never matches, so the
  rest of the cohort falls back.

Equality of all three fields is what makes this exact rather than merely
likely. `seq` alone is not enough: a `Clean` member failing on a device error
while an outsider commits leaves the sequence number where the chain expected
it but the *content* somewhere else. The `(cohort, index)` pair pins the
identity of every commit in between, because only a member acting on cohort
`C`'s decision at position `j` can ever publish `{C, j+1, …}`.

## 2. The code plan

### `crates/inlaysql-core/src/btree/device.rs` — the seam

Five new `Device` methods, every one defaulted to "no absorption", so every
existing device (`SimDisk`, `Simulator`, the WASM device, `io_uring`, the
`Rc<RefCell<T>>` blanket impl) keeps today's protocol byte for byte without
saying anything:

```rust
fn absorb_offer(&self, root: PageId, ops: &mut Ops) -> Option<u64> { None }
fn absorb_claim(&self, token: u64, ops: &mut Ops) -> Option<AbsorbDecision> { None }
fn absorb_cohort(&self, seq: u64, decide: &mut dyn FnMut(&[AbsorbTxn]) -> Vec<AbsorbOutcome>) {}
fn absorption_seal(&self) -> Option<AbsorbSeal> { None }
fn set_absorption_seal(&self, seal: Option<AbsorbSeal>) {}
```

`absorb_offer` **moves** the ops out of the handle (`mem::take`) when the
device absorbs and leaves them untouched when it does not, so the OFF path
costs one `Option` return and no allocation. `absorb_claim` always puts them
back, decision or not. `absorb_cohort` hands the whole parked cohort to the
gate holder as one `&[AbsorbTxn]` slice — one call, so the leader can hold an
overlay of borrows across the whole cohort rather than cloning values out of
each member in turn.

Plus the plain-data types `AbsorbTxn { root, ops }`, `AbsorbOutcome
{ Clean, Conflict }`, `AbsorbDecision { expect: AbsorbSeal, outcome }`, and
`AbsorbSeal` above. All `Send`; none of them names a tree.

### `crates/inlaysql-core/src/btree/tree.rs` — the decision and its use

* `CowBTree::set_commit_absorption(bool)` beside `set_durability`, calling
  `Device::set_commit_absorption` — same "the device decides for the file"
  plumbing shape, so a caller that never asks gets exactly today's behaviour.
  (Sixth trait method; defaulted no-op.)
* `rebase_pending` is split into `rebase_pending_inner(root, next, seq,
  check: bool)`, with `rebase_pending` kept as the `check: true` wrapper so
  the comparison the brief calls "unchanged" is literally the same code. The
  absorbed path calls it with `check: false`; nothing else about the replay,
  `merge_monotonic_metadata`, the cache invalidation, the watermark update or
  the counter adoption differs.
* `CowBTree::absorb_decisions(&self, base_root, txns) -> Result<Vec<AbsorbOutcome>>`
  — the overlay loop of Section 1. `&self` only; no device call beyond
  `get_at`. This is the function the parity test drives directly.
* `commit()` gains, in order:
  1. before `begin_normal_commit`: `let token = device.absorb_offer(self.root, &mut self.pending_ops)`;
  2. after it returns: `let decision = token.map(|t| device.absorb_claim(t, &mut self.pending_ops))` — the ops come back whatever happens, including on a `begin_normal_commit` error;
  3. inside the gate, in place of the single `rebase_pending` call: use the decision if `device.absorption_seal()` matches its `expect`, else fall back;
  4. after `set_commit_point`: publish the seal — the successor seal if this commit acted on a decision, otherwise `None` followed by an attempt to lead a cohort of its own (`device.absorb_cohort`);
  5. on the conflict return and on any in-gate error: publish the successor seal (conflict, chain intact) or `None` (error), before leaving the gate.
* `checkpoint()` publishes `None`.

A member that acted on a decision **does not lead a cohort of its own**. It
could, but a new cohort id would break the chain for every remaining member
of the old one; deferring them to the next leader is both simpler and
strictly less work.

### `crates/inlaysql/src/device.rs` — the coordinator

`CommitCoordinator` gains one `Mutex<Absorption>`:

```rust
struct Absorption {
    enabled: bool,          // any handle that asked; one-way, like reuse_enabled
    next_token: u64,
    next_cohort: u64,
    parked: Vec<(u64, AbsorbTxn)>,          // gate-arrival order
    slots: HashMap<u64, (AbsorbTxn, Option<AbsorbDecision>)>,
    seal: Option<AbsorbSeal>,
}
```

`absorb_offer` pushes onto `parked` (bounded — beyond `ABSORB_COHORT_MAX` a
writer simply is not offered and commits exactly as today) and returns the
token. `absorb_cohort` drains `parked`, calls `decide` on the drained slice,
and files each txn plus its decision into `slots`; a `decide` that returns
the wrong number of outcomes files them with no decision, which is the
fallback. `absorb_claim` takes the entry back out. `absorption_seal` /
`set_absorption_seal` read and write `seal`. Every one of these runs on a
thread that holds the reservation gate *except* `absorb_offer`, which by
construction runs on a thread about to park on it — the mutex is what orders
those two.

`normal_waiters` is not repurposed and the `Condvar` protocol is untouched:
followers still wake to *acquire the gate*, exactly as today. That is the
whole reason this slice cannot lose an acknowledgement — there is no new
wake path to lose it on.

### The flag

`EngineOptions::commit_absorption: bool` (default `false`) →
`TreeStorage::open_on_with_options`'s new parameter →
`CowBTree::set_commit_absorption` → `Device::set_commit_absorption` →
`Absorption::enabled`. `INLAYSQL_BENCH_ABSORPTION=1` selects it in the
concurrency suite, the same way `INLAYSQL_BENCH_DURABILITY` already selects
the durability level.

## 3. The invariants, and where each is checked

| | Invariant | Checked where |
| --- | --- | --- |
| **A1** | A decision is used only when the file's committed state is exactly the one it was computed against. | `absorption_seal() == expect`, read under the gate, all three fields. Every state-changing event that is not a chain member publishes `None`. |
| **A2** | Members are decided in strict gate-arrival order, each against the previous *clean* member's logical post-rebase state. | `parked` is a FIFO; `absorb_decisions` folds the overlay forward in slice order and skips conflicting members. |
| **A3** | The comparison itself is the unchanged one. | `rebase_pending`'s loop and `absorb_decisions`'s loop are the same predicate over the same two `get_at` calls; `mergeable_metadata_key` gates both. |
| **A4** | No outcome is acknowledged before that writer's own sync. | Nothing changes: a follower's `commit()` still runs its own `encode`/`write_dirty_pages`/append/`commit_ready`/`end_normal_commit`/`sync_commit`. There is no outcome-delivery path in this slice at all. |
| **A5** | A leader that fails part-way leaves no member wrongly resolved. | A leader failure publishes `seal = None` (in-gate error path) or simply never publishes a successor, so every member falls back. A leader that panics never publishes anything; the existing `NormalCommitGuard` releases the gate and the members wake to a stale seal, which does not match. |
| **A6** | Absorption never changes what is written or where. | The leader writes nothing on a follower's behalf. WAL records, regions, wrap handling and the free list are untouched, which is why the brief's I4/I5 are satisfied trivially here rather than argued. |
| **A7** | With the flag off, nothing is different. | Every trait default is inert; `absorb_offer` returns `None` without touching the ops map. |

## 4. The DST and test plan

All new tests, and which mutation each one kills:

1. **`absorption_matches_serial_commit_order` (core, `dst_sweep.rs`)** — the
   parity test. For each of many seeds, build the same random transaction
   set twice against two fresh sim files: once serially with absorption off,
   once through the absorbed path with the same forced gate order. Assert
   the final scans are identical **and** the per-transaction outcome vectors
   are identical. *Kills:* dropping the overlay (member 2 stops seeing member
   1's writes → an outcome flips from `Conflict` to `Committed`); folding a
   conflicting member into the overlay; comparing against `root_L` instead of
   the overlay; reversing cohort order.
2. **`an_absorbed_follower_conflicts_exactly_where_a_serial_one_does`
   (core)** — a hand-built three-member cohort where member 2 overlaps
   member 1's key and member 3 does not. Asserts `Conflict, Clean` in that
   order and that member 3's own commit still lands. *Kills:* "conflict stops
   the batch" (member 3 would be refused); "conflict advances the chain seq"
   (member 3's seal would mismatch and it would silently fall back —
   detected by asserting the fallback counter is zero).
3. **`a_stale_seal_falls_back_to_the_full_rebase` (core)** — an outsider
   commits between the leader and member 1. Asserts member 1 still commits
   with the correct outcome and that the decision was *not* used. *Kills:*
   removing the seal check; comparing only `seq`; comparing only `cohort`.
4. **`a_clean_member_that_never_commits_does_not_validate_the_next_one`
   (core)** — member 1 is decided `Clean` and then rolled back rather than
   committed while an outsider commits in its place, so the sequence number
   lands where the chain expected it and the content does not. Asserts member
   2 falls back. *Kills:* seal reduced to `seq` (the exact case Section 1
   names).
5. **`sweep_multi_writer` extended (core, `dst_sweep.rs`, `--ignored`)** —
   one seed in three drives cohorts: two to four writers each buffer a
   transaction, park for absorption, then commit in cohort order under the
   existing `FaultSchedule` fault injection, with the existing assertion that
   the recovered state is one the workload actually committed. *Kills:*
   anything that lets an absorbed commit write a record the recovery chain
   does not accept, and any ordering bug that produces a state no
   interleaving could have produced.
6. **Crash-at-every-step (core, `dst_sweep.rs`, `--ignored`)** — the leader
   crashes after publishing decisions and before/after its own sync, at every
   step index of a cohort. Asserts recovery lands on a committed state and
   that **no follower's rows appear without that follower's own record**
   (checked by scanning the recovered image for each member's key and
   requiring it only where the member's own commit had returned
   `Committed`). *Kills:* any future attempt to let the leader write a
   follower's bytes without this slice's structure noticing.
7. **`crates/inlaysql` threaded tests** — `writers`-style concurrency tests
   run with the flag on and off; a cohort-formation counter asserts cohorts
   actually form at 8 writers, so the sweep is not silently proving nothing
   (the same "assert the thing you are testing actually happened" discipline
   `free_list_reuse_dst` uses for `pages_reused`).
8. **Flag-off parity** — the full existing suite is the test: with
   `commit_absorption: false` no offer is ever made, and every existing DST
   sweep passing unchanged is the evidence.

The measurement follows in `PERF.md`: 1/8/16/32 writers, both flag states,
interleaved, three repetitions, control re-run each rep.

## 5. What would make this slice fail, stated in advance

The brief's STOP condition for Slice 2 is about the parity test: if serial
and absorbed orders cannot be made to agree, the design is wrong rather than
buggy. That is the one this plan is built to answer.

There is a second, weaker outcome worth naming now so it is not a surprise
later: **this slice cannot, by construction, reduce the number of gate
acquisitions.** Every follower still enters the gate to encode, append and
publish its own record; all absorption removes from a follower's own gate
hold is the `rebase_pending` comparison — which `PERF.md` already measures as
too fast to appear as its own bucket. The leader pays that comparison for the
whole cohort *inside* its own hold instead. The honest prediction is
**flat to slightly negative throughput**, and the value of the slice is the
proven decision-ordering machinery Slice 3 needs, not a number. If the
measurement says flat, that is the expected result: the flag stays off, the
number is recorded in `PERF.md`, and the decision about Slice 3 is taken on
its own merits.
