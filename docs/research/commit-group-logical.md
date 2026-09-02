# C1 — Commit-side logical group commit: design brief (AHL-543)

**Status: design brief, not an implementation.** No engine code changes ride
with this document. It answers the six questions the plan item asks for and
records what would have to be true, measured, and DST-proven before any of
this lands. Per `PLAN.md`'s own framing (`docs/research/commit-scaling.md`,
`PERF.md`'s AHL-497 section): this is a commit-protocol redesign, a mistake
here is a data-loss bug, and it gets the same rigor as any WAL/recovery
change under `AGENTS.md` — full DST sweep, not a benchmark run.

## 0. Summary

- **Protocol.** The gate holder (leader), while still holding the single
  process-wide reservation gate, absorbs the pending transactions of writers
  already parked on that same gate. It rebases and conflict-checks each one,
  *in gate order*, against the same latest-committed root it just derived for
  its own transaction, exactly as `rebase_pending` does today — so
  first-committer-wins is unchanged, only now evaluated for N transactions by
  one thread instead of N transactions each re-entering the gate.
- **Record format decision.** One record per sub-transaction, appended
  back-to-back in the leader's own WAL region, under one WAL-region
  reservation — **not** a single record carrying N transactions' pages
  unioned together. Section 2 works the two options; the deciding argument is
  recovery: N separate self-contained records preserve `decode_record_for_version`
  and the `prev_seq`/`prev_root` chain-validation logic in
  `read_committed_state` (`crates/inlaysql-core/src/btree/tree.rs:4610-4677`)
  completely unchanged — a torn write during the group truncates the chain at
  whichever sub-record didn't survive, same as today. A unioned single record
  would need a new partial-acceptance rule inside one record (which pages
  belong to which of N transactions if the record is torn mid-page-list) that
  does not exist anywhere else in this format and would be a second, novel
  recovery path to prove.
- **Top three data-loss classes and their invariants** (full list in
  Section 3):
  1. *A follower's conflict check is skipped or evaluated against a stale
     root.* Invariant: every follower is rebased under the *same* gate hold
     the leader itself is in, against the *same* `current_root`/`current_seq`
     the leader observed — never against a state a still-earlier follower in
     the same batch produced without that follower's dirty pages having
     landed first (see the retracted "shrink the gate" counterexample,
     Section 1).
  2. *A follower is told `Committed` before its bytes are durable.* Invariant:
     a follower is only released with an outcome after its own record has
     been through the same `commit_ready`/`sync_commit` boundary a solo
     commit uses today — grouping changes who does the `pwrite`, never when a
     ticket is safe to publish.
  3. *The leader crashes mid-batch and a follower's WAL record is written but
     never gets a ticket published, or is acknowledged to the caller without
     ever having been appended.* Invariant: outcome delivery to a follower's
     waiting thread happens only after that follower's own record exists in
     the leader's region *and* the coordinator has assigned it a durability
     ticket the same way `Device::commit_ready` does for a solo commit today
     (`crates/inlaysql/src/device.rs:1332-1368`) — a follower is never told
     anything before that point, and the leader's own crash before finishing
     the batch simply leaves every not-yet-appended follower un-notified, to
     be recovered (or retried) the ordinary crash-recovery way.
- **Predicted commits/s at 32 writers.** Using the measured 16-writer commit
  cycle (`PERF.md`'s Task A/B accounting, ~3,270-3,451 µs cycle, 88-90% gate
  busy, 746-812 µs mean gate hold, 96-97% of that hold paying the
  `pwrite`-during-`fsync` penalty): Section 5's model predicts the *gate-hold*
  component collapses from O(writers) independent 700-800 µs holds to one
  amortized hold of roughly the same size covering the whole cohort, moving
  the ceiling from `1/775µs ≈ 1290 commits/s` (today's structural ceiling per
  `PERF.md`) toward something closer to the `fsync`-bound ceiling
  (`1/3300µs × cohort_size`). At 32 writers with an estimated cohort size of
  6-10 (bounded by how many writers pile up behind the gate during one
  `fsync`), the model predicts **roughly 1,800-2,600 commits/s**, i.e.
  1.9-2.7x today's ~974-988 commits/s at 32 writers — not the plan's original
  100x framing, and explicitly a model, not a measurement (Section 5 states
  the assumptions and where they could be wrong).
- **Sequential single-writer: no gain, and it should not gain.** A solo
  commit never has a cohort to absorb — `coalesce_normal_commits`'s emptiness
  check already fires before any yield in that case
  (`crates/inlaysql/src/device.rs:681-710`), and `PERF.md`'s single-writer
  accounting puts gate-hold at only 70-116 µs against a 1,080-1,405 µs
  `fsync` (gate busy 5-6%). Nothing in this design touches that path.
- **First implementation slice.** A same-process, same-tree, no-WAL-change
  prototype: teach the leader to rebase (not encode, not append) a bounded
  number of already-parked followers' pending operations under its own gate
  hold, still committing each as its own separate `CowBTree::commit` call
  once the leader releases the gate — i.e. move only the *rebase decision*
  earlier, changing nothing about WAL records or durability. This is small,
  reversible, and isolates the one correctness-critical piece (conflict
  ordering) from the higher-risk pieces (multi-record leader-owned append,
  outcome handoff) before touching either. See Section 6, Slice 1.

## 1. The current protocol, step by step, for two writers A and B

Both writers hold independent `CowBTree` handles (`crates/inlaysql-core/src/btree/tree.rs`)
on the same file, sharing one process-local `CommitCoordinator`
(`crates/inlaysql/src/device.rs:79-145`).

**A calls `commit()` first, B a moment later, both with non-conflicting rows
buffered.**

1. **`CowBTree::commit`** (`tree.rs:1223`) is entered by A. `has_pending` is
   true, so A calls `self.device.begin_normal_commit()`
   (`tree.rs:1227` → `device.rs:1391-1421`).
2. **`begin_normal_commit`** increments `normal_waiters`, then calls
   `begin_reservation` (`device.rs:1152-1170`), which locks
   `coordinator.reserved: Mutex<bool>` and, finding it `false`, sets it `true`
   and returns immediately — A now holds the gate. `normal_inflight` is
   incremented, `gate_started_ns`/`gate_started_racing` are recorded on A's
   handle (`device.rs:1406-1418`), and a `NormalCommitGuard` is stashed so a
   panic mid-commit still releases the gate (`device.rs:1414-1418`, guard
   type documented at `PERF.md`'s AHL-497 follow-up #2).
3. **B calls `commit()` a moment later.** B also calls `begin_normal_commit`,
   which increments `normal_waiters` (now counting B as a waiter), then calls
   `begin_reservation`. `reserved` is `true`, so B calls
   `coordinator.reservation_done.wait(reserved)` (`device.rs:1158-1163`) —
   **B is now parked on the gate's `Condvar`, doing zero work**, until A
   releases it. This parked wait, aggregated across every writer at every
   writer count, is what `PERF.md`'s call-stack profiling measured at
   **87.5% of all `TreeStorage::commit` samples** (`begin_normal_commit`,
   `PERF.md` AHL-497 section, "Finer profiling shows the critical section was
   never the problem — the contention was").
4. **A, still holding the gate, does its in-gate prepare work**
   (`tree.rs:1240-1336`, the `prepared` closure):
   - Reads the committed state — from `Device::commit_point`'s process-local
     cache when populated (`tree.rs:1234`, `device.rs:1493-1507`), else a
     full re-derivation via `read_committed_state` (`tree.rs:1244-1246`,
     `tree.rs:4610-4677`).
   - Calls `self.rebase_pending(current_root, current_next, current_seq)`
     (`tree.rs:1255`, defined at `tree.rs:2251-2293`): for every key A's
     transaction touched, compares `self.get_at(self.root, key)` (A's own
     stale snapshot) against `self.get_at(current_root, key)` (the latest
     committed root A just read) — a mismatch on a non-mergeable key is a
     conflict; otherwise A's writes are replayed against `current_root`.
   - `finalize_free_list` (`tree.rs:2518-2543`), `encode_record_into`
     (`tree.rs:1273-1291`, into `wal::RecordMeta`), a region-wrap check
     (`tree.rs:1305-1318`), `write_dirty_pages` (`tree.rs:1319`,
     `tree.rs:2561-...`), then the WAL append itself (`tree.rs:1320`).
   - Publishes the new `CommitPoint` into the shared `gate.state`/`gate.append`
     cache (`tree.rs:1325-1333`, `device.rs:1509-1530`) so the *next* gate
     holder does not have to re-derive it from the file.
5. A calls `self.device.commit_ready()` (`tree.rs:1348` → `device.rs:1332-1368`):
   closes A's gate-hold timing segment, then takes a `writes_completed`
   ticket and stashes it on A's own `FileDevice` (`pending_commit_ticket`).
   This publish happens **before** the gate is released — a flush leader
   already mid-`fsync` can still cover A's bytes.
6. A calls `self.device.end_normal_commit()` (`tree.rs:1352` →
   `device.rs:1456-1464` → `end_reservation`, `device.rs:1181-1210`): bumps
   `coordinator.generation`, sets `reserved = false`, and calls
   `reservation_done.notify_one()`. **This is the only place B's wait ends.**
7. B wakes, finds `reserved` now `false` (or races another waiter for it —
   the `Condvar` loop in `begin_reservation` re-checks), sets it `true`, and
   B now runs the exact same in-gate sequence step 4 ran, against whatever
   root A's commit just published — including A's dirty pages, which are
   already `pwrite`-landed by this point (step 4's `write_dirty_pages`
   returned before A published its `CommitPoint`).
8. A calls `self.device.sync_commit()` outside the gate
   (`tree.rs:1408` → `device.rs:1316-1327`), which either finds A's ticket
   already covered by a concurrent flush (returns immediately) or becomes
   flush leader itself, calling `coalesce_normal_commits` then the real
   `fsync`/`F_FULLFSYNC` (`device.rs:559-...`, `681-710`). This is the
   *existing*, already-shipped group commit (AHL-461/AHL-468): it batches
   `fsync` calls across already-appended, already-encoded records. It does
   not touch the gate at all — this document's proposal is one layer
   earlier, on the prepare/encode/append side.

**What `rebase_pending` needs from the latest root, and why deferring past
gate release is unsafe (the retracted idea, restated).** `rebase_pending`
(`tree.rs:2251`) calls `self.get_at(current_root, key)` for every key the
transaction touched — a real tree walk that dereferences pages reachable from
`current_root`. `current_root` is whatever the *previous* writer just
published. For that walk to return the right answer, the previous writer's
dirty pages must already be `pwrite`-landed and readable, because
`current_root` names them. **The idea "shrink the gate to reservation-only,
move encode/write past gate release" — considered and rejected in `PERF.md`'s
AHL-497 section — was: let a writer take its sequence/page reservation under
the gate, then release the gate and do the encode/`pwrite`/append afterward,
off the critical path.** The counterexample that kills it: if writer A
releases the gate before its dirty pages are landed, writer B's
`rebase_pending` (now free to enter the gate immediately after A) walks
`current_root` — the root A's *reservation* claimed it would produce — before
A has actually written the pages that root's newly-allocated ids describe.
B's tree walk either reads garbage/unwritten bytes at those ids or (worse, on
a device that zero-fills) silently sees emptiness where A's rows should be,
and B's conflict check answers a question about a root that does not yet
exist on disk. This is not a slow path, it is `rebase_pending` returning a
wrong answer — a correctness hazard, not a performance one — and `PERF.md`
states explicitly: **"that proposal is retracted."** This design brief
inherits that constraint unconditionally: nothing in Section 2 defers a
follower's rebase, encode, or write past the point where the root it rebases
against is already durable-visible (landed on the device, not necessarily
`fsync`-synced — the existing protocol's own step 4 already only requires
`pwrite`-landed, not `fsync`-durable, for the *next* writer's `rebase_pending`
to be safe, since `commit_point`/`current_root` describe pages, not sync
state).

## 2. The proposed protocol

**Shape.** The gate holder becomes a *leader* the moment it observes
`coordinator.normal_waiters > 0` at the point it would otherwise begin its
own in-gate prepare work (step 4 above) — i.e. the same signal
`coalesce_normal_commits` already reads today, just checked one phase
earlier, before encode rather than before `fsync`. If no one is waiting, the
leader's path is byte-for-byte today's solo commit (Section 2 changes
nothing observable when the cohort size is 1).

**What a follower does while waiting.** Exactly what it does today up to and
including the wait itself: parked in `begin_reservation`'s `Condvar` loop
(`device.rs:1158-1163`), on `reservation_done`. The follower's *transaction*
— `self.dirty`, `self.pending_ops`, `self.pending_root`, `self.pending_next`
on the follower's own `CowBTree` — sits fully formed in the follower's own
handle, already built by the follower's own `put`/`delete` calls before it
ever called `commit()`. This is what makes leader-side absorption possible
without cross-thread mutable access to another handle's B-tree: the leader
does not need the follower's *tree*, it needs the follower's *pending_ops*
map (the same input `rebase_pending` already replays from,
`tree.rs:2263-2291`) and its *dirty* pages (the same input `encode_record_into`
already reads, `tree.rs:1283`). The follower's thread stays parked on the
gate `Condvar` — it does not run its own prepare code at all when it is
absorbed; it is woken only once with its outcome (Section "Follower outcome
delivery" below), not woken to re-enter the gate itself.

**What changes at the coordinator/gate boundary.** `begin_reservation`'s
`Condvar` wait needs a second wake condition beyond "the gate is free" — a
follower that was absorbed must wake knowing its result without re-acquiring
the gate to find out. Concretely: each parked follower needs a small
per-transaction outcome slot (a `Mutex<Option<FollowerOutcome>>` or
equivalent, keyed by the follower's own handle/thread, analogous to how
`FlushState`'s `epoch` lets a flush follower distinguish a real completion
from a spurious wakeup, `device.rs:330-339`) that the leader fills in before
calling `notify_all` (not `notify_one` — every absorbed follower and every
still-genuinely-unabsorbed waiter needs to be woken, since some are done and
some must now compete for the gate themselves, e.g. a follower that arrived
after the leader's absorption cutoff).

**The leader's in-gate sequence, one cohort:**

1. Leader reads `current_root`/`current_next`/`current_seq` exactly as today
   (`tree.rs:1241-1249`), once, from `commit_point` or a full re-derivation.
2. Leader snapshots the set of followers currently parked on the gate — a
   bounded read of `normal_waiters` plus whatever structure holds their
   pending-ops/dirty maps (this is new state; today nothing about a parked
   follower is visible to the gate holder except a count). **Cohort
   membership is fixed at this point**, mirroring the existing flush-side
   rule that `target = writes_completed.load()` is snapshotted strictly
   before the barrier (`device.rs:560-561`, `PERF.md`'s Task B write-up): a
   writer that parks on the gate *after* this snapshot is not in this batch,
   full stop — it competes for the gate again once this batch's leader
   releases it, becoming leader or follower of the *next* batch.
3. Leader rebases **itself** first against `current_root`
   (`rebase_pending`, unchanged), producing `seq_L = current_seq + 1`,
   `root_L`, `next_L`.
4. Leader rebases **each cohort follower in gate arrival order**, each one
   against the *previous* member's post-rebase root in the same batch — i.e.
   follower 1 rebases against `root_L`/`seq_L`, producing `root_1`/`seq_L+1`;
   follower 2 rebases against `root_1`/`(seq_L+1)`, and so on. This is the
   same `get_at(self.root, key) != get_at(current_root, key)` check
   `rebase_pending` already runs (`tree.rs:2263-2269`), run N times by one
   thread instead of once by each of N threads re-entering the gate — **the
   conflict semantics are identical to today's serialized-gate behavior**,
   because today's serialized behavior is *already* "each writer rebases
   against whatever the previous gate-holder just committed, in gate-arrival
   order." Logical group commit does not change *what* first-committer-wins
   compares against; it changes *who* runs the comparison and *when* the
   result becomes visible.
5. Each rebase that conflicts is recorded as `Conflict` for that follower
   immediately and drops out of the batch — it contributes no dirty pages,
   no WAL record, and does not advance `seq`/`root` for the next follower in
   line (identical to today: a follower whose commit conflicts under the old
   protocol never gets to publish a `CommitPoint` either).
6. For every follower that rebased cleanly, the leader calls
   `finalize_free_list`, `encode_record_into`, and the region-wrap check —
   **per follower, producing one self-contained `WalRecord` per follower**
   (this is the record-format decision below), appended back-to-back into
   the *leader's own region* at consecutive offsets, each carrying its own
   `prev_seq`/`prev_root` link to the one before it in the batch (leader's
   own record first, then follower 1's, then follower 2's, ...). The
   `append_offset`/`gate.append[region]` cache advances after each one, so a
   later member of the *same* batch reserves space correctly, exactly as it
   does across separate gate acquisitions today.
7. Leader writes every batch member's dirty pages via `write_dirty_pages`
   (per member, same fresh-page-id argument as today — see
   `docs/recovery.md`'s "Step 2's page ids are fresh with respect to every
   *committed* state" — each follower's dirty pages were allocated from that
   follower's own `alloc_page` calls before it ever entered the gate, using
   `next_page_id` counters that were valid against *that follower's own*
   snapshot; the leader's rebase in step 4 does not reallocate page ids, only
   re-validates row-level conflicts, so page-id freshness is unaffected by
   batching — see the free-list caveat below for the one place this needs a
   closer look).
8. Leader publishes one `commit_ready` ticket per successfully-batched
   member (each member's bytes are independently ticketable — a flush leader
   elsewhere in the coordinator does not need to know these came from one
   gate hold) — or, more precisely, since this is happening under the *same*
   `CommitCoordinator` as today, a batch of N successful commits should
   plausibly publish N tickets in sequence via the same `writes_completed`
   counter `commit_ready` already uses (`device.rs:1366-1367`), preserving
   the existing "ticket = evidence of completed writes" contract exactly, one
   ticket per transaction, not one ticket per gate hold.
9. Leader calls `end_normal_commit` **once** for the whole batch — the gate
   itself is held once, so it is released once, and `coordinator.generation`
   advances once per *gate acquisition*, not once per transaction it covered.
   This is a real, deliberate change to what `Device::commit_generation`
   means: today "generation advanced by 1" implies "one commit or checkpoint
   attempt happened"; under batching it implies "one gate hold ended, having
   covered 1..=N attempts." Nothing currently downstream depends on generation
   advancing by exactly 1 (`refresh`'s generation check, `tree.rs:1552-...`,
   only asks "did it move at all", and `commit_generation`'s doc comment
   already allows for checkpoints and conflicts to advance it without a
   successful commit, `device.rs:1443-1448`) — but this is exactly the kind
   of implicit assumption a DST differential test must probe explicitly
   (Section 4).
10. Leader wakes every batch member with its outcome (`Committed { seq, root,
    next }` or `Conflict`) via the per-follower outcome slot, then releases
    the gate as today.

**Follower outcome delivery and new snapshot.** A follower's own `CowBTree`
handle never ran its own `commit()` in-gate body — the leader did the rebase,
encode, and write on the follower's behalf, reading the follower's
`pending_ops`/`dirty` maps directly. On wake, the follower's `commit()` call
(still the *only* thing the follower's own thread is blocked inside) receives
the outcome the leader computed and performs exactly the bookkeeping
`CowBTree::commit` already does post-gate for a solo commit
(`tree.rs:1404-1428` for `Committed`, `tree.rs:1354-1401` for `Conflict`):
clears `dirty`/`pending_ops`, sets `self.root`/`self.next_page_id`/`self.next_seq`
from the values the leader computed for it, sets `self.seen_generation` to
the *one* generation value the whole batch advanced to, calls
`invalidate_for_reuse()`, and calls `update_watermark(seq)` with its own
`seq` (not the batch's last `seq` — each follower's reader watermark must
reflect its own commit point for `min_reader_seq`'s liveness proof in
"The free list and page reuse", `docs/recovery.md:626-657`, to stay correct).
The one thing that *cannot* happen on the follower's own thread post-wake is
`sync_commit()` running independently per follower the way it does today —
see the durability section below.

**WAL record format changes.** None to the record encoding itself
(`encode_record_into`, `wal.rs:214-237` is unchanged byte-for-byte) — the
change is entirely about *how many records one gate hold produces and where
they land*, not what one record contains. The **per-writer region invariant**
— "each native file handle is assigned a region... append placement is
reserved under the same short commit gate" (`docs/recovery.md:27-29`) —
already tolerates more than one handle sharing a region ("handles beyond four
safely share a region," same citation); this design pushes that further:
*one handle's region now durably carries other handles' commit records too*,
in gate order, indistinguishable on disk from four independent writers who
happened to all get assigned the leader's region and committed in that exact
sequence. **Recovery needs zero new logic for this**, because
`read_committed_state`/`scan_region`'s chain validation
(`tree.rs:4610-4677`, `wal.rs:347-396`) already merges records from all
regions by `seq` and validates `prev_seq`/`prev_root` links regardless of
which physical region a record lives in — "explicitly ordered recovery
chain" (`docs/recovery.md:383`) was already a claim about logical sequence
order, never about one region == one logical writer. The chain a batch
produces is simply N consecutive records with a tighter physical locality (a
contiguous region range) than today's four-interleaved-regions case, which
the existing multi-writer DST sweep already exercises the general shape of
(Section 4). **Torn multi-transaction record recovery**: because each
sub-transaction is still its own self-contained record with its own length
prefix and checksum (`decode_record_for_version`, `wal.rs:273-335`), a torn
write during a batch behaves exactly as `scan_region` already handles a torn
write today — it stops at the first record that fails to decode, keeping
every record before it (`wal.rs:365-376`, `break` on `None`). If the leader's
own record (first in the batch) tears, the whole batch is lost — same
blast radius as today's "a torn write that tears the record is not a commit"
(`docs/recovery.md:367`), just now potentially N transactions' worth instead
of one. If a follower's record (not the leader's) tears, everything before
it in the batch survives and is a real committed prefix; everything after it
in the *same batch* is lost even if those later records themselves would
have decoded cleanly, because `scan_region` stops at the first failure by
design — this needs no new mechanism, but it is a real answer worth stating
plainly: **batching converts "N independent single-transaction torn-write
exposures" into "one shared torn-write exposure whose blast radius scales
with cohort size."** This is examined as its own failure class in Section 3.

**The alternative considered and rejected: one WAL record carrying the union
of N transactions' dirty pages.** Pack every batch member's dirty pages into
one `WalRecord` (one `seq`, one `root`, one `next` — the batch's final
values), with each member's individual page set living inside it. This was
rejected for three reasons, in order of severity: (1) **partial-record
recovery has no defined answer.** If this single fat record tears, there is
no way to say "3 of the 5 sub-transactions in this record are recoverable" —
`decode_record_for_version` either accepts the whole record or rejects it
whole (`wal.rs:280-326`, checksum covers everything), so a tear anywhere in
a 5-transaction record loses all 5, where the per-record design above loses
only the transactions from the tear point onward. (2) **first-committer-wins
per-transaction visibility is lost**: a caller's `commit()` returning
`Committed` today means *that transaction's own row keys* are durable; a
union record still needs the same per-follower outcome delivery this design
already requires, so packing pages together buys nothing there while adding
a real recovery-format cost. (3) **it is a genuine format version bump**
(new record layout, new `count`-of-sub-transactions field, new per-
sub-transaction root/next/seq inside one record) where the per-record design
needs **zero format changes** — the existing v5 record shape, unchanged,
simply gets appended more than once per gate hold. Given `docs/recovery.md`'s
own stated bar for a format version bump ("What lifting the one-region
ceiling would take" section, `docs/recovery.md:459-542`, treats a record-
layout change as needing its own DST pass end to end), avoiding one entirely
when the safer alternative is no harder to implement is the correct call.

**Free list / page reuse interplay.** `alloc_page` (`tree.rs:2312-2331`)
draws either a fresh monotonic id or a reclaimed one from
`free_candidates`, populated by `refill_free_candidates`
(`tree.rs:2363-...`) using `Device::commit_point`'s `seq` (durability proof)
and `Device::min_reader_seq` (liveness proof) — both evaluated **before** the
transaction entered the gate, against whatever `commit_point`/watermark state
existed at that time. Under batching, a follower's dirty pages (and any
`free_candidates` it already drew) were fixed before it ever parked on the
gate — the leader does not reallocate them during the rebase in step 4, only
re-validates row-level *key* conflicts. This is safe as long as no follower
in the same batch could have reused a page another *earlier* batch member is
about to free — and that hazard already cannot arise, because
`refill_free_candidates`'s durability proof requires `commit_point.seq` to
already cover the freeing commit, and a batch member's freed pages
(`freed_this_txn`, turned into free-list rows by `finalize_free_list` at
encode time, step 7 above) do not become visible to `commit_point` until the
*leader* publishes them at the end of the whole batch — so a same-batch
follower could not have refilled from them even if it tried, because it drew
its candidates strictly earlier. **The one place this needs explicit
attention**: `refill_free_candidates`'s check against
`consumed_ever_this_txn` (`tree.rs:2380-...`, guards against the exact
AHL-481 bug — a page offered and consumed twice inside one transaction) is
per-transaction state on each follower's own handle, untouched by
batching — it does not need to become per-batch, because two *different*
transactions in the same batch drawing the same free-list candidate is
already impossible for the reason above (candidates are drawn pre-gate, and
the free-list rows a batch produces are not visible to any reader — including
a same-batch follower's earlier `refill_free_candidates` call, which already
happened before the batch existed — until the leader's own publish at the
very end). **Net: no new free-list invariant is needed; the existing
durability/liveness proof already treats "which transaction is currently
holding the gate" as irrelevant to when a freed page becomes reusable, and
batching does not change when the leader's publish happens relative to any
member's own free-list read.**

**`seen_generation`/`update_watermark`/reader visibility.** Every batch
member sets `seen_generation` to the *one* `coordinator.generation` value the
batch's single `end_normal_commit` call produces (see point 9 above) — a
follower's `seen_generation` after a batched commit is therefore identical to
every other batch member's, which is new: today two writers committing
back-to-back always observe two different generation values. This is safe
for `refresh`'s fast path (`tree.rs:1552`, doc comment
`tree.rs:1448-1551`) because `commit_generation`'s only contract is "did
*something* change since I last looked" — never "exactly what changed" — and
a follower that just committed has no reason to call `refresh` afterward
anyway (its own commit already advanced its state). `update_watermark`
(`tree.rs:800-804`) must still be called **per follower with that follower's
own `seq`**, not the batch's final `seq`, because `min_reader_seq`
(`device.rs:1591`, feeding the free-list liveness proof) is a per-reader
value the free-list proof needs to be conservative for the *oldest* live
reader — collapsing every batch member's watermark to the batch's newest
`seq` would let `refill_free_candidates` reclaim a page a batch member with
an *older* `seq` is still logically entitled to see as live, which is exactly
the class of bug `docs/recovery.md`'s free-list section exists to prevent.

**`Durability::Normal` vs `Full`.** Unaffected in kind, affected in blast
radius. `sync_commit`'s barrier choice (`device.rs:1316-1327`,
`CommitCoordinator::effective_durability`) is orthogonal to this design —
batching changes how many *records* one gate hold produces, not when or how
strongly they are synced; the existing flush-side group commit
(`make_durable_with_cohort`) already batches `fsync` calls across
independently-appended records and continues to do so unchanged for a
batch's worth of leader-appended records. What does change: under
`Durability::Normal`, "loss is bounded to commits since the last checkpoint
or WAL-region wrap" (`docs/recovery.md:185-194`) — a lost sync at this level
now can lose an entire *batch* (N transactions) rather than one, since a
batch's records are contiguous and share the leader's region placement. This
does not violate the documented bound (the bound was already "since the last
checkpoint," not "one transaction"), but it changes the *typical* loss size
under `Normal` from ~1 commit to ~cohort-size commits on a busy file, which
is worth stating in the eventual `docs/recovery.md` update rather than
leaving implicit.

**Interrupt/timeout behaviour.** Today, a parked follower on
`begin_reservation`'s `Condvar` has no timeout or cancellation path at all —
`reservation_done.wait(reserved)` blocks unconditionally
(`device.rs:1158-1163`); nothing in this codebase currently interrupts a
gate wait (confirmed by search: no `timeout`/`interrupt` handling touches
`begin_reservation`, `begin_normal_commit`, or the flush-follower wait in
`device.rs`). Batching does not need to add one, but it must not accidentally
remove the ability to add one later: a follower absorbed into a batch is
still, from its own thread's point of view, blocked on exactly one
`Condvar` wait it did not have before (today it waits to *acquire* the gate
itself; under batching it waits to be *told an outcome* by someone else) —
the same statement about "no existing cancellation reaches into a parked
gate wait" continues to hold verbatim, so this is a pre-existing gap this
design inherits rather than one it introduces or worsens.

## 3. Failure analysis

| # | Crash point | What recovery lands on | Invariant that prevents worse |
| --- | --- | --- | --- |
| 1 | Leader crashes before writing any batch member's pages/record | Nothing from this batch exists on disk; every member's transaction is simply lost from the caller's point of view (no different from today's solo-commit crash-before-write) | No record is ever appended before its pages are written (unchanged from today, `tree.rs:1319-1320`) |
| 2 | Leader crashes after writing pages/record for the leader's own transaction, before any follower's | Recovery replays the leader's own record (self-contained, `wal.rs`); followers never got a ticket, never got woken with `Committed`, so their `commit()` calls are still blocked or return an error on device failure — no follower can have been told success | Ticket publish (`commit_ready`) and outcome-wake both happen strictly after a member's own record is on disk (point 8/10 in Section 2); a follower is woken *only* after its own record exists |
| 3 | Leader crashes mid-batch, after follower *k*'s record is appended but before follower *k+1*'s | `scan_region` stops at the first torn/missing record past follower *k*'s (`wal.rs:365-376`); recovery accepts the leader's record plus followers 1..k, rejects k+1 onward | Same self-contained-record property that already protects a solo commit; batching does not weaken it, only extends the exposure window across more transactions per gate hold (named explicitly above) |
| 4 | Leader appends all N records and publishes all N tickets, then crashes before any of them syncs | Identical to today's "crash before sync" case, scaled by N: all N records are on disk but the file's own last `fsync` may predate them, so a torn-write/crash fault can lose the unsynced tail — bounded exactly as `Durability::Full`/`Normal` already bound it (`docs/recovery.md:176-194`), just with a bigger typical tail | `sync_commit`'s barrier is unaffected by batching (previous section); the loss bound named in `docs/recovery.md` is a statement about *unsynced* records, already true today for however many commits accumulate between syncs |
| 5 | **The data-loss class the plan explicitly warns about: a follower is acknowledged `Committed` before its bytes are durable, or before its own conflict check ran.** | If this invariant were violated, recovery could land on a state that never happened — a follower's row visible to *other in-process readers* (via `self.root` set on wake) while the bytes describing it are not yet on disk, or a follower's write accepted despite conflicting with a same-batch member ahead of it | Two invariants together prevent it: (a) outcome wake happens only after `commit_ready`'s ticket publish, which itself only happens after that member's `pwrite`s returned (point 8 mirrors `tree.rs:1343-1349`'s existing ordering exactly); (b) every batch member is rebased in strict gate-arrival order against the *previous* member's already-computed post-rebase root (point 4), never in parallel and never against a root a still-earlier member merely *intends* to produce |
| 6 | **A follower's committed acknowledgement is lost** — the leader computes and even writes the follower's record, but the follower's thread is never woken (leader panics between step 7 and step 10, or the outcome-slot write races the follower's wait registration) | Without a fix, the follower's `commit()` call could hang forever, or worse, return `Conflict`/an error for a transaction that actually made it to disk — the caller then retries a transaction that is already committed, and (if the retried transaction reuses the same row keys) could silently double-apply or, if it retries via `INSERT` with a uniqueness constraint, error confusingly | The `NormalCommitGuard`-style RAII pattern already used for `normal_inflight`/`normal_waiters` (`device.rs`, AHL-497 follow-up #2) must extend to the outcome slots: a leader that panics mid-batch must, on unwind, mark every not-yet-notified batch member with a distinguishable "leader failed, state unknown — re-derive from the file" outcome rather than leaving them parked forever or silently returning a wrong outcome. This is new code this design requires, not something inherited for free — see Slice 3, Section 6. |
| 7 | Region wrap lands mid-batch (the leader's own transaction needs to wrap the region between two batch members) | Existing wrap logic already forgets the cache, rewrites the state block, zeros the region, and republishes (`tree.rs:1305-1318`) — this must run as a *whole-batch* boundary: a wrap cannot be interleaved between "leader appended, follower rebased" and "follower encoded," because the followers already rebased against a root the wrap's own state-block rewrite is about to make canonical. Concretely: if any batch member's record would overflow the region, the simplest safe rule is closing the batch before that member (encode/append everyone before it, then treat the overflowing member and everyone after it as the seed of the *next* batch, which naturally re-derives state post-wrap) rather than trying to wrap mid-batch | The existing rule "forget the cached commit point before rewriting the state block, republish only once the wrap completes" (`tree.rs:1305-1318`, `docs/recovery.md`'s "A concurrent `refresh` could see a backup go backward in time" postmortem) must never be observed by a batch member mid-rebase; closing the batch at the wrap boundary keeps that rule's existing scope (one gate hold) intact instead of extending it to "one gate hold containing a wrap in the middle," which is new and unproven ground |

**Summary of the invariant set this design must hold, all restatements of
points already made above, gathered in one place for the DST design in
Section 4 to test directly:**

- **I1 (ordering).** Every batch member is rebased against the post-rebase
  state of the member immediately before it in gate-arrival order, never in
  parallel, never out of order.
- **I2 (no early ack).** A batch member's outcome is never delivered to its
  waiting thread before that member's own WAL record and dirty pages are
  `pwrite`-landed and its ticket is published.
- **I3 (no lost ack).** A leader that fails partway through a batch leaves
  every not-yet-notified member in a state its own thread can detect and
  safely resolve (re-derive from file), never a silent hang and never a
  wrong outcome.
- **I4 (self-contained records).** Batching never changes the fact that each
  sub-transaction is independently decodable and independently torn-write-
  safe; it only changes how many of them one gate hold produces.
- **I5 (no wrap-mid-batch observation).** A batch never straddles a
  WAL-region wrap in a way that lets a member rebase against a pre-wrap root
  while writing into a post-wrap region layout, or vice versa.

## 4. The DST plan

**What existing sweeps already cover.** `crates/inlaysql-core/tests/dst_sweep.rs`'s
`sweep_multi_writer`/`multi_writer_regions_recover_to_a_committed_interleaving`
(`dst_sweep.rs:229-299`) already drives multiple `CowBTree` handles across
all four WAL regions, with real conflicts (shared key space,
first-committer-wins), fault injection (`FaultSchedule::random_with`), and
asserts recovery lands on some state the workload actually committed. This
is the closest existing coverage to the *recovery-chain* half of this design
(I4, I5) — but it is important to be honest about its shape: **it drives one
writer at a time, sequentially, from a single thread** (`dst_sweep.rs:253`:
one `rng`-chosen writer per iteration, `put`/`delete` then `commit()`, no
concurrency). It proves the *format* can carry an interleaved multi-region
chain and recover it correctly; it does not, and cannot as written, exercise
the *coordinator*-level mechanics (`CommitCoordinator`, gate parking, leader
election, outcome delivery) at all, because those live in `inlaysql`'s
`FileDevice`/`CommitCoordinator` (a `std`-only, real-thread structure) —
**`inlaysql-core`'s `sim` harness is `no_std` and single-threaded by
construction** (`crates/inlaysql-core/src/sim/mod.rs`'s doc comment: "no
wall clock, no syscalls"). This is the honest limit stated the same way
`docs/recovery.md`'s own "honest limit of that coverage" section states the
`Durability` plumbing's simulator gap (`docs/recovery.md:299-319`): **the
core sim can prove the record format and recovery chain are correct for any
interleaving a batch could produce, but it cannot itself exercise the leader/
follower gate protocol, because that protocol does not exist inside the
`no_std` core at all — it is a `crates/inlaysql` (`std`) concern.**

**What this means for where new tests live.** Two distinct test surfaces are
needed, mirroring the split `docs/recovery.md`'s `Durability` coverage
already uses (`crates/inlaysql-core/tests/durability_dst.rs` for plumbing,
`crates/inlaysql/tests/durability.rs` for the real-syscall half):

1. **`inlaysql-core` sim/DST surface — the recovery-chain half.** Extend
   `dst_sweep.rs`'s `sweep_multi_writer` (or add a sibling sweep) to construct
   the on-disk *shape* a batch produces directly — N consecutive
   self-contained records in one region, sharing a `prev_seq`/`prev_root`
   chain, encoded exactly as `encode_record_into` already does — without
   needing to simulate the coordinator's threading at all, since the format
   is already region/writer-agnostic (Section 2's WAL-format-changes
   argument). This proves I4/I5 (self-contained records recover correctly
   under every existing fault the harness models, including a tear at each
   possible position within a batch) using the harness that already exists,
   with no new fault model needed.
2. **`crates/inlaysql` fake-coordinator/white-box tests — the coordinator
   half.** `docs/research/commit-scaling.md`'s own "Evidence required before
   landing code" list (item 1: "Fake-coordinator tests that pin: a ready
   follower joins; a ticket created after target capture does not join; a
   failed leader wakes followers; and a checkpoint-held reservation cannot
   deadlock the leader") names exactly the pattern this design needs, one
   level up: a controllable fake leader (the same technique
   `a_checkpoint_concurrent_with_a_normal_commit_still_makes_progress`
   already uses, `PERF.md`'s AHL-497 follow-up #1, `device.rs`) that can be
   paused mid-batch to deterministically test I1-I3 without depending on
   real thread-scheduling races.

**New sim scenarios needed (recovery-chain half, `inlaysql-core`):**

- A batch whose leader record survives but every follower record tears
  (I4): confirm recovery accepts exactly the leader's transaction and
  nothing past it — a direct test of "a torn write during a batch loses
  transactions from the tear point onward, not the whole batch, and not
  more."
- A batch that straddles a would-be region wrap (I5): confirm the wrap
  boundary closes the batch cleanly rather than letting a post-wrap
  member's record land inside a pre-wrap append offset, or vice versa —
  this needs a workload specifically sized to trigger a wrap mid-cohort,
  which the existing `sweep_multi_writer`'s workload (small, arbitrary
  key/value pairs) does not reliably do; borrow the sizing approach
  `docs/recovery.md`'s free-list DST note used to guarantee reuse actually
  fires (`docs/recovery.md:682-694`, "a heavy-churn workload... asserting
  ... `pages_reused()` is nonzero somewhere across the sweep") — here,
  assert a wrap actually occurred mid-batch somewhere across the sweep, or
  the sweep proves nothing about this scenario.
- A batch containing both conflicting and clean followers interleaved (I1):
  confirm a follower in the *middle* of a batch that conflicts does not
  corrupt the rebase chain for followers *after* it — i.e. a conflicting
  follower is excluded from the chain (does not advance `seq`/`root` for
  the next member) but does not stop the batch.

**New scenarios needed (coordinator half, `crates/inlaysql`, fake-leader
tests):**

- **"Leader crash mid-record with followers acknowledged?"** — explicitly
  the scenario the task names. The invariant under test (I2/I3) is that this
  can never happen: a fake leader that is stopped after appending follower
  *k*'s record but before publishing follower *k*'s ticket must leave
  follower *k*'s thread still parked (not woken with `Committed`), and a
  fresh handle reopening the file afterward must NOT see follower *k*'s
  transaction as committed (since its ticket, and therefore its
  `sync_commit` coverage, never happened) — matching crash point 3/6 in
  Section 3's table. Prove both halves: the on-disk chain stops correctly
  (recovery-chain half, item above) *and* the in-process follower was never
  told otherwise (coordinator half, this test).
- **"Followers must NOT be acknowledged before the sync"** — a fake leader
  paused *after* appending and ticketing every batch member but *before*
  calling `sync_commit`/`fsync`: confirm every follower's `commit()` call is
  still blocked (not yet returned to the caller) at that pause point, then
  released only once the fake leader's `sync_commit` completes — this is I2
  stated as its own explicit test rather than inferred from the ticket-order
  argument in Section 2.
- **A checkpoint arriving mid-batch** — extending the existing
  `a_checkpoint_concurrent_with_a_normal_commit_still_makes_progress` pattern
  (`PERF.md`'s AHL-497 follow-up #1) to a batch in progress: a checkpoint
  must not be absorbed into a batch (checkpoints do not increment
  `normal_waiters` today and must not start doing so), and a batch's leader
  must not deadlock waiting on a checkpoint the way the existing test already
  proves the flush leader does not.
- **A leader panic mid-batch** (I3 directly) — same shape as
  `a_panic_between_begin_and_end_normal_commit_does_not_leak_the_inflight_counter`
  (`PERF.md`'s AHL-497 follow-up #2), extended to assert every follower that
  was mid-absorption at panic time resolves (via the RAII-guard-driven
  fallback named in Section 3, row 6) rather than hanging.

**Differential/parity test.** The single most load-bearing test this design
needs, because it is the one that directly encodes "conflict semantics are
unchanged": generate a random sequence of N transactions (some conflicting,
some not, mirroring `sweep_multi_writer`'s workload shape) and commit it two
ways against two fresh files with the same seed:

1. **Serially, in gate order, with grouping disabled** — the exact behavior
   `dst_sweep.rs` already exercises today, one writer's `commit()` fully
   resolved before the next begins.
2. **Through the batching leader path**, with the *same* gate-arrival order
   forced deterministically (a test-only knob, not a production one) so the
   two runs are asked to agree on the same input ordering rather than
   comparing two runs whose orderings could legitimately differ.

Assert the two runs produce **byte-for-byte identical final states**
(scan comparison, same as every existing DST sweep's own assertion) and
**identical per-transaction outcomes** (same set of transactions marked
`Conflict`, in the same positions). Any divergence is a semantics bug, not a
timing artifact, because the test controls ordering explicitly. This is the
test that would have caught the record-format alternative's outcome-
visibility problem (Section 2) had it been built instead, and it is the test
that makes "first-committer-wins per transaction, in gate order" a checked
property rather than an assertion in a doc comment.

## 5. Expected payoff, with a model

**Inputs, all measured, not assumed** (`PERF.md`'s Task A/B commit-cycle
accounting, quoted verbatim above in Section 1's citations, and
`BENCHMARK.md`'s published concurrency table):

| Writers | Measured commits/s (BENCHMARK.md) | Cycle time | fsync | gather (leader spin) | gate hold (mean, per commit) | gate busy |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 244-247 | 1205-1531 µs | 1080-1405 µs | 0 | 70-116 µs | 5-6% |
| 4 (proxy for the table's absent "8" row shape) | — | 2033-2338 µs | 1299-1695 µs | 300-370 µs | 373-414 µs | 44-47% |
| 8 | 1209-1261 | — | — | — | — | — |
| 16 | — | 3270-3451 µs | 1543-1898 µs | 892-934 µs | 746-812 µs | 88-90% |
| 32 | 974-988 | — | — | — | — | — |

**The model.** `PERF.md`'s own conclusion from this table is the starting
point: "the system's throughput ceiling is the gate, not the barrier:
`1/775µs ≈ 1290 commits/s` bounds what any batching or pipelining schedule
can push through the gate at these hold costs" — but that statement is about
*today's* gate-hold cost, which is inflated **10x over solo** (79 → 775 µs at
16 writers) almost entirely because 96-97% of gate holds are acquired while
a barrier is in flight, paying the measured 18-23x `pwrite`-during-`fsync`
penalty (`PERF.md`, "Why: `pwrite` on this device gets ~18-23x slower while
another handle's `F_FULLFSYNC` is in flight"). **Logical group commit changes
the arithmetic of that penalty directly**: today, N writers queued behind the
gate each pay their own inflated gate-hold cost sequentially (N ×
inflated-hold); under batching, one leader pays the encode/write cost once
per transaction but the *fixed* per-gate-acquisition overhead (the contention
and wakeup cost the 87.5%-parked figure names, plus any `pwrite`-during-
`fsync` inflation that applies per gate *entry* rather than per byte written)
is paid once for the whole cohort instead of once per transaction.

Modeling this conservatively — assume the *per-transaction* write cost
(`pwrite` for pages + record, unaffected by batching, since the leader still
issues the same number of `pwrite` calls per transaction) stays at its
measured inflated rate, but the *gate-acquisition* overhead (contention,
wakeup, the 87.5%-parked cost) is paid once per batch rather than once per
transaction:

- At 16 writers, if a leader can absorb a cohort of size ~6-8 (bounded by how
  many writers accumulate behind the gate during roughly one `fsync`'s worth
  of wall-clock time — the same population `coalesce_normal_commits`
  already gathers on the flush side, `PERF.md`'s "gather (leader spin)" row
  growing to 892-934 µs at 16 writers is itself evidence that 6-8 followers
  are typically available to absorb by the time a leader would otherwise
  give up), the effective per-transaction gate cost drops from ~775 µs
  (today, paid per transaction) toward something closer to
  `775µs / cohort_size + per_txn_pwrite_cost`. Using the per-transaction
  `pwrite` component alone (WAL-write minus the gate-acquisition share,
  roughly the solo 70-116 µs figure as a floor since that is what one
  transaction's own encode+write costs with no contention at all) as the
  floor, the model predicts an effective per-transaction cost in the
  **150-300 µs** range at 16 writers — roughly **3,300-6,700 commits/s** as
  a *structural* ceiling, before the `fsync`-batching ceiling on the flush
  side (already shipped, unaffected by this change) is applied on top.
- **But the flush-side ceiling still applies and is the binding constraint at
  high cohort sizes**: the existing group-commit ratio (`normal_tickets /
  normal_flushes`) measured 4.76-6.31 at 32 writers post-AHL-497
  (`PERF.md`, "The published sweep, regenerated"), meaning even with
  gate-side batching removing the gate as the bottleneck, no more than
  roughly one `fsync`'s worth of transactions (empirically ~5-6 at 32
  writers today) actually gets covered by one flush round in practice —
  raising that ratio is a *different* lever (the flush-side gather window,
  already tuned) than this design touches. **The honest ceiling this design
  predicts is therefore bounded above by `fsync_rate × transactions_per_flush`**,
  where `fsync_rate ≈ 1/3300µs ≈ 303/s` and `transactions_per_flush` could
  plausibly grow from the current ~5-6 toward the batch cohort size (~6-10)
  once the *gate* is no longer forcing writers to queue up slower than the
  flush round can absorb them — giving **roughly 1,800-3,000 commits/s** at
  the point where gate contention stops being the limiter, consistent with
  the summary box's 1,800-2,600 estimate.
- At **32 writers**: applying the same reasoning with a somewhat larger
  cohort (more writers queued per unit time behind the gate at 32 than at
  16), the model predicts **roughly 1,800-2,600 commits/s**, a **1.9-2.7x**
  improvement over the measured 974-988 commits/s baseline — closing most
  but not all of the gap to the flush-side `fsync` ceiling (`~303/s × a
  plausible ~8-10 transactions/flush ≈ 2,400-3,000/s`), because gate
  contention does not vanish entirely (a batch still has a leader-election
  step, still has per-transaction encode/write cost inside the gate, and the
  cohort-snapshot rule in Section 2 means a batch cannot grow unboundedly —
  a writer arriving mid-batch waits for the *next* one).
- **At 8 writers**: the model predicts a smaller relative gain than at 16/32,
  because the measured gate-busy fraction at a proxy 4-writer point is only
  44-47% (well below saturation) — there is less queued contention for a
  leader to absorb, so the cohort sizes available to absorb are smaller and
  the win is correspondingly smaller. This is consistent with
  `docs/research/commit-scaling.md`'s own framing that the falloff this
  design targets is specifically the "32-writer falloff past the 12-16
  peak" — the plan names the region where this design should help *most*,
  not the region where it helps *least*.

**Sequential single-writer case: no gain, stated plainly.** At 1 writer,
`normal_waiters` is always 0 when the sole writer reaches the gate — there is
never a cohort to absorb. The model predicts **zero change** to the
single-writer number (244-247 commits/s stays 244-247), which matches
`PERF.md`'s own statement about the solo path already being at its `fsync`
floor with gate busy at only 5-6%. This is consistent with the plan's own
framing of the single-connection sequential-write loss as a *separate*,
already-investigated-and-rejected problem (`PERF.md`'s "Deferred/checkpointed
page durability... does not pay" section, and the "sequential-write loss to
MySQL/PG" symptom the plan names is about *group commit's absence* under a
single connection specifically — which by construction has no cohort to form
regardless of what mechanism forms it).

**Caveats on this model, stated honestly.** This is an analytical model built
from measured segment costs, not a measurement of the proposed design — the
actual numbers depend on: (a) how large a cohort actually forms in practice
once the leader begins absorbing rather than merely gathering already-
appended tickets (a currently-unmeasured quantity — `coalesce_normal_commits`
gathers *tickets*, not *pending transactions*, so its observed gather
population is not directly the same as this design's cohort size, only a
plausible proxy for it); (b) whether the per-transaction rebase-in-sequence
work (I1, Section 2) adds meaningful CPU cost of its own at cohort sizes of
6-10 — `PERF.md` states `rebase_pending`'s own tree walk "doesn't show up as
a distinct bucket at all — it's fast" for one transaction, but N sequential
tree walks under one gate hold is new load this model does not itself
account for; (c) whether outcome-slot bookkeeping (the per-follower wake
mechanism, Section 2) adds contention of its own that offsets some of the
gain. **Slice 1 in Section 6 exists specifically to measure (a) and (b)
before committing to the higher-risk slices that would make (c) real.**

## 6. Implementation slices, smallest first

Each slice is independently landable, independently DST-gated per
`AGENTS.md`'s rule for any change touching `btree`, `wal`, or `sim`, and each
one either directly informs whether the next slice is worth building or
reduces the risk surface of the next one.

**Slice 1 — measure cohort formation, no protocol change.** Add
instrumentation (mirroring `INLAYSQL_COMMIT_STATS`'s existing pattern,
`device.rs`) that counts, at the moment a writer would begin its in-gate
prepare work, how many *other* writers are currently parked on the gate
(`normal_waiters`, already tracked) — publish this as a histogram or simple
count/sum pair, not a protocol change. This directly measures Section 5's
model input (a) without writing a single line of leader/follower logic.
**Size: small, hours not days.** **DST gate: none needed — this is pure
read-side counting, no WAL/recovery/gate-semantics change at all,** so it
falls outside `AGENTS.md`'s DST-required category, though the change should
still be exercised under the existing concurrency benchmark to confirm it
adds no measurable overhead to the hot path. **STOP condition: if measured
cohort sizes at 16-32 writers come back near 1 (i.e. writers rarely find
company waiting when they reach the gate, meaning the queue drains faster
than gate-hold time would suggest), the whole premise of this design is
wrong and nothing past this slice should be built** — the model in Section 5
explicitly depends on cohorts of 6-10 actually forming, and if they do not,
logical group commit has no work to amortize.

**Slice 2 — rebase-only absorption, no WAL change (the summary box's "first
slice").** Teach the leader to rebase (I1: strict gate-arrival order,
against the previous member's post-rebase root) a bounded number of
already-parked followers' `pending_ops` under its own gate hold — writing
the *rebased result* back into each follower's own `CowBTree` state
(`root`, `pending_root`, `pending_ops` merged per `rebase_pending`'s
existing logic) — but then **releasing the gate and letting each follower
run its own encode/write/`commit_ready`/`sync_commit` independently**,
exactly as it does today, just skipping the redundant `rebase_pending` call
each follower would otherwise make (since the leader already did it). This
isolates the single correctness-critical piece — conflict-check ordering
under one thread instead of N re-entries — from every higher-risk piece
(shared WAL region ownership across transactions, outcome-slot wake
mechanics, ticket batching). **Size: medium — touches `CowBTree::commit`'s
public entry point and the gate/coordinator boundary, but no WAL format
code and no new on-disk shape.** **DST gate: yes — this changes commit
ordering semantics even though it does not change the WAL format, so the
differential/parity test from Section 4 applies here first, in its simplest
form** (this slice's serial-vs-batched comparison has an even easier
baseline to match, since "batched" here still commits each transaction
through the unmodified per-transaction encode/write path). **STOP condition:
if the differential/parity test cannot be made to pass deterministically —
i.e. if rebasing N followers under one thread in gate-arrival order produces
outcomes that provably cannot match N followers each re-entering the gate
and rebasing independently — the "conflict semantics are unchanged" claim
underpinning this whole design is false, and Section 2's protocol needs to
be rethought, not merely debugged.**

**Slice 3 — leader-owned encode/append for the cohort, single-region.**
Extend Slice 2 so the leader also encodes and appends each cohort member's
WAL record into its own region (per-record, back-to-back, as Section 2
specifies), publishes one ticket per member, and wakes each follower with a
`Committed`/`Conflict` outcome instead of letting the follower resume its own
encode/write. This is where I2/I3/I4/I5 all become live risks for the first
time — the leader is now the sole writer of another handle's WAL bytes, and
a leader crash mid-batch is a new crash class (Section 3's rows 2-3, 6-7).
**Size: large — new per-follower outcome-slot machinery, RAII-guarded
leader-failure handling (Section 3, row 6), and every new DST scenario named
in Section 4's coordinator half.** **DST gate: yes, the full new set —
recovery-chain sweeps (I4/I5) plus every fake-leader coordinator scenario
(I1-I3) named in Section 4, not a subset.** **STOP condition: if the
measured throughput at this slice does not exceed Slice 2's number by a
margin that justifies the added crash-class surface — i.e. if most of
Section 5's predicted gain turns out to come from the rebase-ordering
amortization alone (Slice 2) rather than from the encode/append batching
this slice adds — stop here and ship Slice 2 only.** This is the single most
important STOP condition in this brief: Slice 2 is far lower risk (no new
crash classes, no WAL region ownership change) than Slice 3, and if it
captures most of the win, Slice 3's additional data-loss surface is not
worth taking on for a small residual gain.

**Slice 4 — ticket-batch tuning against the flush side.** Once Slice 3 is
landed and DST-clean, tune the interaction between this design's cohort size
and the existing `coalesce_normal_commits` gather window (`device.rs:681-710`)
— Section 5's model names the flush-side `fsync` rate as the binding
constraint once gate contention is removed, so this slice is about making
sure the two group-commit layers (this one, and the already-shipped
`fsync`-side one) compose rather than fight each other for the same
`normal_waiters`/`normal_inflight` signals. **Size: small-medium, tuning
constants and cross-layer signal plumbing, not new protocol.** **DST gate:
re-run the full existing DST + Slice 3's new suite; no new scenarios are
expected to be needed unless tuning changes cohort-size bounds enough to
newly exercise Section 3's row 7 (wrap-mid-batch) in ways the fixed-size
testing in Slice 3 did not.** **STOP condition: if tuning cannot move the
measured 32-writer number meaningfully past Slice 3's own number without
increasing p99 tail latency by more than the roughly 8x-worse-than-SQLite
figure `BENCHMARK.md` already reports today (i.e. if the only way to gain
more throughput here is to make the tail materially worse than the existing,
already-disclosed trade), stop and publish Slice 3's number as the closed
result rather than chasing a worse trade for a smaller marginal gain.**

## What is cheaper and adjacent

**Promoting just-committed pages into the decoded cache is a smaller, lower-
risk change with real, already-measured payoff for the identical write-heavy
workload this design targets.** `PERF.md`'s AHL-496 section measured, on a
steady-state single-row `INSERT` workload (the same shape the concurrency
benchmark uses): **6.40 device `read` calls per durable commit (26.2 KiB)**,
because "a committed page is dropped from the handle rather than promoted
into the decoded cache... for a write-only workload the page cache is a
100% miss and the descent pays a `pread` and a `page::decode` per level,
every commit." This is orthogonal to gate contention entirely — it is pure
wasted I/O that happens *regardless* of whether one writer or a batch of
writers holds the gate, and it happens on the very same root-to-leaf
descents `rebase_pending` and `encode_record_into` walk during commit,
meaning it costs the same amount whether or not this design's batching ever
lands. Concretely: today, committing a row writes ~6.5 pages to the data
area and the *next* commit through the *same or a different handle* re-reads
those same pages off the device from scratch, because nothing about a
successful commit tells the page cache "these bytes you just wrote are also
now valid to serve from cache." Promoting a commit's own dirty pages into the
shared raw-page cache (`FileDevice`'s existing raw cache, `device.rs:326-360`,
already used for AHL-536's `read_shared`) the moment they are written would
turn every one of those 6.40 reads into a cache hit for the writer that
produced them and for the next writer whose descent touches the same pages
— a direct, measurable win on the exact metric AHL-496 already instruments.

**Quick measurement, done read-only, no engine change.** The number above
(6.40 reads/commit, 26.2 KiB, mode 7 dirty pages) is not re-derived in this
task — re-running `bin/profile.rs --suite writes` with the counting-`Device`
wrapper AHL-496 already built would reproduce it, but doing so is out of
scope for a design-brief task that is instructed to make no engine changes
and stay read-only; the existing, already-committed measurement is cited
instead, and it is recent enough (post the `encode_record_into` single-pass
fix that section documents) to be trustworthy without a rerun. The
qualitative claim this brief needs from it — "a write-only workload is a
100% page-cache miss, at 6.4 reads per commit, unrelated to gate
contention" — is already established evidence, not a new claim.

**Why this belongs in this brief rather than its own plan item.** It shares
the exact same profiling substrate (`PERF.md`'s AHL-496 instrumentation),
the exact same workload shape (steady-state single-row `INSERT`s, the
concurrency benchmark's own transaction shape), and — most importantly for
sequencing — it is a strictly *smaller, independently landable, and lower-
risk* change than any slice in Section 6: it touches only the read side of
an already-existing cache (`FileDevice`'s raw cache) with an existing
invalidation story (data-area pages are immutable once written, per
`docs/recovery.md`'s "a page id names one immutable sequence of bytes for
the lifetime of the file"), needs no new WAL format, no new recovery logic,
and no DST scenario beyond what the existing cache-correctness tests already
cover for that cache. If the epic this plan item belongs to needs a cheap
win to ship before Section 6's Slice 3 lands, this is the candidate — not a
substitute for logical group commit (it does not touch gate contention at
all, so it does not move the 87.5%-parked figure), but a genuinely
independent, complementary reduction in what a commit costs once it holds
the gate, which Section 5's model treats as a fixed cost this design
amortizes rather than reduces.
