# C1 slice 2 — one append, one sync, one gate hold per cohort (AHL-547)

**Status: the plan for the second landable slice of
`docs/research/commit-group-logical.md`, written before the code.** It is the
brief's **Section 6, Slice 3** ("leader-owned encode/append for the cohort,
single-region"), and it is the slice the brief's whole payoff model rests on.
Slice 1 (`docs/research/commit-group-slice1.md`, AHL-544) proved the cohorts
are there — 5-9 members at 8-32 writers, 81-95% of commits absorbed — and
measured flat, exactly as it predicted, because moving the *decision* alone
leaves every follower taking the gate, appending its own record and running
its own `fsync`. This slice removes all three.

## 0. What lands, in one paragraph

A writer with a buffered transaction offers it to the device and then **waits
for an outcome instead of waiting for the gate**. Whichever writer holds the
gate becomes the leader: it commits its own transaction, then drains the
parked cohort and, still holding the gate, judges each member in gate-arrival
order with the unchanged `rebase_pending` comparison, **replays the clean
ones through its own tree**, writes every member's data-area pages, appends
every member's record — its own first, then the members' in order,
back-to-back in the leader's own region — as **one `device.write` of one
contiguous byte range**, publishes one durability ticket per member, and
**syncs once, still inside the gate**. Only then does it hand each member its
outcome and release the gate. A follower wakes holding `Committed { root,
next, seq }` to adopt, `Conflict`, an error, or `Fallback` — "nobody judged
you, commit the ordinary way" — and never touches the gate, the log or the
disk. All of it is behind the same `EngineOptions::commit_absorption`,
**default `false`**; every `Device` default means "no absorption", so `sim`,
`mem`, WASM and `io_uring` keep today's protocol byte for byte unless they
opt in. The deterministic simulator opts in, because the sweeps are the
gate this slice has to pass.

**The on-disk format does not change.** Not the record layout, not the
version, not the header. What changes is how many records one gate hold
appends and whose transactions they describe — which
`read_committed_state`/`scan_region` already cannot tell apart from four
independent writers that happened to share a region. §5 is the argument in
full.

## 1. The one thing the brief got wrong, and what replaces it

The task framing asks what a follower "hands over": *"its encoded record
bytes + data-area page writes, or its dirty page set as `(PageId, bytes)`
plus the record it would have written — encoded by the follower itself under
its own decision, then passed as `Send` plain data through the device seam."*

**Neither is possible, and the reason is structural rather than a detail of
this codebase.** A follower's dirty pages describe the tree it built *on its
own snapshot root*, with page ids drawn from *its own* `next_page_id`. The
moment the leader judges it `Clean`, `rebase_pending` throws every one of
those pages away:

```rust
self.dirty.clear();
...
self.root = current_root;
self.adopt_next_page_id(current_next);
...
for (key, value) in ops { match value { Some(v) => self.put(&key, &v)?, None => self.delete(&key)? } }
```

The pages a rebased transaction actually commits are produced by that replay,
against a root that *does not exist yet* when the follower parks — it is the
previous cohort member's post-rebase root. So a follower cannot encode its
record in advance under any decision, because the bytes it would encode are
not the bytes it commits. (The single exception — a follower whose own
snapshot root is already the state it is rebasing onto and which is first in
the cohort — is not worth a second code path.)

**What a follower hands over is therefore exactly what slice 1 already moves
across the seam and nothing more: `AbsorbTxn { root: PageId, ops:
BTreeMap<Vec<u8>, Option<Vec<u8>>> }`.** Both fields are plain `Send` data;
neither names an `Rc`, a page or a tree. That is the whole of the input
`rebase_pending`'s replay consumes.

**The leader materialises the pages itself, on its own tree.** After it has
committed its own transaction its handle sits at `root_L` with `dirty`
empty and its pages on the device, which is precisely the state a handle is
in at the top of an ordinary `commit()`. Applying member 1's ops to it
produces member 1's pages — allocated from the *leader's* `next_page_id`,
which sidesteps the brief's page-id-freshness caveat entirely: no follower's
page ids are ever used, so no two members can collide and no follower's
pre-gate allocation has to be trusted. The leader repeats that N times. What
it is doing is, line for line, N ordinary commits by one handle with the gate
acquired once and the barrier run once.

**What a follower gets back** is the plain-data mirror of that:

```rust
pub enum AbsorbResult {
    /// The leader committed this transaction. Adopt these and return
    /// `CommitOutcome::Committed`.
    Committed { root: PageId, next: PageId, seq: u64, generation: Option<u64> },
    /// First-committer-wins aborted it. The file is at this state.
    Conflict  { root: PageId, next: PageId, seq: u64, generation: Option<u64> },
    /// The commit that carried it failed. Same ambiguity a solo commit's
    /// failed append or failed sync already has, reported the same way.
    Failed(&'static str),
    /// Nobody judged it. Its `ops` came home; commit exactly as today.
    Fallback,
}
```

Every field is `Copy` plain data. `generation` is the one value
`end_normal_commit` produced for the whole gate hold — the brief's step 9,
and the reason `commit_generation` now means "one gate hold ended, having
covered 1..=N attempts" rather than "one attempt happened". Nothing reads it
as a count (`refresh` only asks whether it moved), and §8 tests that
explicitly.

## 2. The protocol, step by step, with who holds the gate

`F` is a follower, `L` the leader. Both start in `CowBTree::commit`.

| # | Who | Step | Gate |
| --- | --- | --- | --- |
| 1 | F | `park_for_absorption()` → `Device::absorb_offer(root, &mut pending_ops)`. Returns `None` (and touches nothing) when absorption is off, the handle is read-only, the cohort is full, or **`normal_inflight == 0`** — nobody holds the gate, so this writer is about to acquire it rather than park behind it. On `Some(token)` the ops have **moved** out of the handle. | not held |
| 2 | F | `Device::absorb_wait(token, &mut pending_ops)`. Blocks. **This replaces `begin_normal_commit` for an offered writer** — a follower no longer queues for the gate at all. Returns one `AbsorbResult`; the ops are back in the handle if and only if it is `Fallback`. | not held |
| 3 | L | `begin_normal_commit()` → acquires the gate. | **held** |
| 4 | L | Reads `commit_point(region)` (or re-derives), rebases itself, `finalize_free_list`, `materialize_dirty`, `encode_record_into` **into a cohort buffer**, region-wrap check, `write_dirty_pages()`, `dirty.clear()`. Its record is now bytes in the buffer, not yet on the device. | held |
| 5 | L | `Device::absorb_take()` — drains every parked offer into the leader's hands, **fixing cohort membership**. A writer that offers after this belongs to the next leader's cohort, the same rule the flush side uses when it snapshots `writes_completed` before the barrier. | held |
| 6 | L | For each member *j* in arrival order: set `self.root = txn.root`, `self.pending_ops = txn.ops`, `has_pending = true`, then `rebase_pending(root_{j-1}, next_{j-1}, seq_{j-1})` — **the unchanged comparison**, on the unchanged code path, against a root that is genuinely on the device because step 4 (and the previous member's own step 6) wrote its pages before this. `false` → `Conflict`; the chain does not advance and the leader restores `self.root = root_{j-1}`. `true` → `finalize_free_list(seq_j)`, `materialize_dirty`, `encode_record_into` **appended to the same cohort buffer**, `write_dirty_pages()`, `dirty.clear()`. Stops the cohort (everyone from here on is `Fallback`) if the buffer would overflow the region, if the record is over `max_record_len`, or on any device error. | held |
| 7 | L | **One** `device.write(append_offset, &cohort_buffer)` — every member's record, back to back, in the leader's region. Every page every record names is already on the device. | held |
| 8 | L | `set_commit_point(region, Some(final root/next/seq/append_offset))`. Once, for the whole cohort. | held |
| 9 | L | `commit_ready()` once per committed member (leader included) — N+1 tickets off `writes_completed`, the last of which is the one this handle will sync to. Per-member so `normal_tickets/normal_flushes` keeps meaning "transactions per barrier". | held |
| 10 | L | `Device::sync_commit_in_gate()` — the real `fsync`/`F_FULLFSYNC`, **inside the gate**. §3 is why. | held |
| 11 | L | `Device::absorb_resolve(results)` — files one `AbsorbResult` per token and wakes every waiter. **This is the first instant any member can be told anything, and it is strictly after the barrier of step 10 returned.** | held |
| 12 | L | `end_normal_commit()` → releases the gate, bumps `generation` once. | released |
| 13 | F | Wakes in step 2 with its result and does the post-gate bookkeeping `CowBTree::commit` already does: clear `dirty`/`pending_ops`/free-list scratch, `self.root = root`, `adopt_next_page_id(next)`, `next_seq = seq + 1`, `seen_generation = generation`, `invalidate_for_reuse()`, `update_watermark(seq)` **with its own seq, not the cohort's last**. Returns `Committed`/`Conflict`. | not held |

**The leader's own outcome.** It keeps its *own* `(root_L, next_L, seq_L)`
and adopts them, exactly as today — not the cohort's final state. Its
`next_page_id` stays wherever the members left it, which is legal and
required (`adopt_next_page_id` is monotonic and never rewinds; the ids the
members consumed must never be handed out twice). Its watermark is `seq_L`,
which is *more* conservative than the cohort's last seq and therefore always
safe for the free list's liveness proof. This keeps the leader's observable
contract identical to today's: a `commit()` that returns `Committed` leaves
the handle at the state its own transaction produced.

### The seal is gone, and why

Slice 1's `AbsorbSeal`/`AbsorbDecision` exist for one reason: the decision
was computed under one gate hold and *used* under a later one, so something
had to rule out everything that could happen in between. **Slice 2 has no
in-between.** Membership is fixed, judged, written, synced and resolved
inside a single gate hold by a single thread; there is no window for an
outsider, a checkpoint or a failed peer to slip into. Keeping the seal would
mean carrying a second protocol behind one flag and having the sweeps prove
a path production never takes. Both types, both `Device` methods
(`absorption_seal`, `set_absorption_seal`) and `rebase_pending_inner`'s
`check` parameter are removed; `rebase_pending` goes back to being one
function with one behaviour, which is a strictly stronger version of slice
1's invariant A3 ("the comparison itself is the unchanged one") — there is
now only one comparison and everyone runs it.

### Liveness: a follower can never wait forever

This is the hazard slice 1 explicitly did not have, because its followers
woke to acquire the gate and nothing else. Three rules close it, and
together they are exact:

1. **A leader resolves everything it took, on every exit path.** Success,
   conflict, device error, region overflow, cohort refusal — each produces a
   result for each token. Enforced by construction (the resolve call is the
   last thing before `end_normal_commit` on *all* paths) and, for a panic,
   by `release_normal_reservation` — the function `NormalCommitGuard::finish`
   and `NormalCommitGuard::drop` both funnel through — which fails out
   anything still in flight. That is the guard AHL-497 already built for
   `normal_inflight`, extended to one more thing that must not be left
   dangling.
2. **A member that was never taken un-parks itself when the gate hold it
   offered into ends.** `AbsorbQueue` carries a `gate_generation` bumped
   inside `release_normal_reservation` under the same lock `offer` takes.
   `absorb_wait`'s predicate is: *resolved → return it; still parked and the
   generation moved → take my ops back and return `Fallback`; otherwise
   wait.* A member offered after the leader's step 5 falls out here, commits
   the ordinary way and typically becomes the next leader. There is no lost
   wakeup, because both the offer and the bump take the same mutex.
3. **A member that was taken but not yet resolved keeps waiting**, which is
   correct: rule 1 guarantees the answer is coming, and rule 2 cannot fire
   for it because it is no longer in `parked`.

The only remaining way to park forever is a leader thread that neither
returns nor unwinds — a hang, not a bug this protocol can have — and that
already blocks every other writer today.

## 3. Why the sync is inside the gate

It is the one deliberate departure from the brief, which has the leader
release the gate before syncing (its step 9 before its step 10). Four
reasons, in order of weight:

1. **It is what "acknowledged only after the leader's sync" means with no
   window.** Resolving after `end_normal_commit` needs the resolve to be
   reachable from a panic in `sync_commit`, and the only structure that can
   promise that is a watchdog with a heuristic ("this cohort has been in
   flight across two gate releases, give up") — a timeout wearing a proof's
   clothing. In-gate, the existing RAII guard covers it exactly.
2. **`coalesce_normal_commits` would spin against us.** Its gather window
   yields while `normal_inflight != 0 || normal_waiters != 0`, and a leader
   syncing while holding the gate is itself `normal_inflight == 1` forever.
   The leader therefore uses a barrier that skips the gather
   (`sync_commit_in_gate` → `make_durable`'s non-coalescing path, the same
   one checkpoints already use for exactly this reason). No deadlock either
   way — the gather is bounded by `COMMIT_COALESCE_MAX_YIELDS` — but the
   yields are pure waste, and skipping them is honest rather than lucky.
3. **It removes the `pwrite`-during-`fsync` penalty this file's own
   profiling measures at 18-23x** (`PERF.md`, AHL-497 and the 2026-08-30
   gather-window section). Today 96-97% of gate holds at 16 writers are
   acquired while a barrier is in flight. Under this protocol no writer can
   be in the gate while the barrier runs, because the barrier runs *in* the
   gate. The work that used to race the `fsync` is now the *next* cohort's,
   done after it.
4. **The arithmetic still works.** The cycle becomes
   `N × per-transaction encode/write + one barrier` for `N` commits, against
   today's `N × (inflated gate hold) + barriers`. At the measured cohort size
   of 6-9 and a ~1.5-1.9 ms barrier this is the brief's model with the
   gate-acquisition term deleted rather than divided.

What it costs: a solo writer must not pay it, and does not — with an empty
cohort the leader takes today's path verbatim (`commit_ready`,
`end_normal_commit`, then `sync_commit` outside the gate). The in-gate
barrier happens only when there is a cohort to amortise it over. And it
serialises the barrier against the *next* cohort's gate work, which is the
trade being measured; if it does not pay, §11's STOP condition fires.

## 4. Seq assignment, and what happens to a follower's own region

**Seq.** One counter, one chain, assigned by the leader in arrival order:
the leader takes `seq_L = current_seq + 1`, member *j* takes `seq_{j-1} + 1`
if it commits and nothing if it conflicts. Each record carries
`prev_seq`/`prev_root` naming the member before it, so the cohort is a
contiguous run of the same globally-ordered chain every commit has always
produced. A conflicting member consumes no sequence number, writes no record
and leaves no gap — identical to a writer that conflicts today.

**A follower's own region is untouched for that commit.** It appends
nothing, so its region's own append offset does not move and its own record
chain is exactly what it was. That is not a special case the format has to
tolerate: `read_committed_state` merges every region's records by `seq` and
validates `prev_seq`/`prev_root` links **regardless of which physical region
a record lives in** (`docs/recovery.md`'s "explicitly ordered recovery
chain" was always a claim about logical order, never about one region ==
one writer), and `docs/recovery.md` already says handles beyond four share a
region. A region that receives no records for a while is the same thing as a
handle that does not commit for a while, which is the ordinary case. The
per-region `CommitPoint` cache is per-region and the leader only publishes
its own; a follower reads the cache for its own region on its next
unabsorbed commit and gets an *older* `append_offset` — which is right,
because its region really has not moved — and a `root`/`seq` that may be
behind. The wrap check and `rebase_pending` both handle an older cached
state today (that is what `resolve_state_at_least`'s floor exists for), so
this is not new ground; the leader's publish is what makes the *newest*
state available, and it publishes into its own region's slot, which is the
one the next leader reads.

**One consequence worth naming:** with absorption on, the log's records
concentrate in whichever regions lead. That is a distribution change, not a
correctness one, and it makes region wrap *more* frequent for a leading
region and less for a following one. §5's wrap rule is why that is safe.

## 5. The record chain, and what recovery does with it

Nothing in `crates/inlaysql-core/src/wal.rs` changes. `encode_record_into`
produces the same bytes; `decode_record_for_version` accepts the same bytes;
`scan_region` walks a region from its start, decodes each record whole and
`break`s at the first one that fails. A cohort is N of those records at
consecutive offsets, each with a length prefix and its own checksum, each
naming its predecessor.

**Recovery therefore validates a cohort record by record, and a torn tail
loses only from the tear onward.** Concretely, for a cohort of the leader
plus members 1..N written as one `pwrite`:

* Every byte landed → `scan_region` decodes N+1 records; the chain
  validation in `read_committed_state` links them by `prev_seq`/`prev_root`
  and the committed state is member N's.
* The write tore after member *k*'s record → records 0..k decode, the next
  one fails its length prefix or its checksum, `scan_region` stops, and the
  committed state is member *k*'s. Members *k+1*..N are not committed and
  never were acknowledged (step 11 is after step 10's barrier, so if the
  tear is visible the barrier did not return).
* The write tore inside the leader's own record → nothing from this cohort
  decodes, exactly as a torn solo commit is simply not a commit.

**The blast-radius statement, said plainly:** batching converts N
independent single-transaction torn-write exposures into one shared exposure
whose size scales with cohort size. It does not create a new *kind* of
loss — the brief's I4 — and it does not violate any documented bound, since
`Durability`'s bound was always "commits since the last checkpoint", never
"one commit". It does change the *typical* loss under `Durability::Normal`
from about one commit to about a cohort's worth. `docs/recovery.md` gets
that sentence.

**Region wrap closes the cohort (the brief's I5).** The leader's own record
may wrap, exactly as today, because at that point the cohort buffer holds
only the leader's own bytes and nothing has been judged. From the first
member onward, a record that would push the buffer past
`region_end(...)` **ends the cohort**: that member and every member after it
is resolved `Fallback` and commits the ordinary way, re-deriving state after
whatever wrap it then performs itself. A cohort therefore never straddles a
wrap, never rebases against a pre-wrap root while writing into a post-wrap
layout, and the "forget the cached commit point, republish only once the
wrap completes" rule keeps its existing scope of one gate hold.

**Format version stays 5.** Nothing about the record layout, the header, the
state block or the region geometry changes; only which handle's `pwrite`
produces a given record and how many of them one gate hold produces. Bumping
the version would be a claim that an older reader cannot read these files,
and it can — byte for byte, they are files four independent writers sharing
a region could have produced.

## 6. Free list, page reuse, watermarks, readers

**Page ids.** Every page in a cohort is allocated by the leader's own
`alloc_page`, from the leader's own monotonic counter and the leader's own
`free_candidates`. No follower's pre-gate allocation is used at all, which
deletes the brief's one open caveat here rather than arguing it away. Two
members cannot collide because they are two sequential transactions on one
handle, which is the case `consumed_ever_this_txn` (AHL-481) already covers.

**Free-list rows.** Each member runs `finalize_free_list(seq_j)` itself —
on the leader's handle, with the leader's `freed_this_txn`/
`consumed_this_txn`, cleared between members by `rebase_pending`'s own
reset. Because the whole cohort is one thread's sequential work, a page
freed by member 1 becomes a `\x02free\0…` row inside member 1's record and
is only *reusable* once `commit_point.seq` covers it and `min_reader_seq`
allows — neither of which can happen inside this gate hold. Slice 1's
blanket refusal of any offered transaction carrying a `FREE_LIST_PREFIX`
key is kept: those keys are only ever written from inside `commit`, so an
offered `pending_ops` cannot contain one, and a total guard is still cheaper
than a proof.

**Watermarks and readers.** Per member, with that member's own seq (step
13) — never the cohort's last. The brief is emphatic about this and it is
right: `min_reader_seq` feeds the free list's liveness proof and must stay
conservative for the *oldest* live reader. Collapsing every member's
watermark onto the newest seq would let `refill_free_candidates` reclaim a
page a member with an older seq is still entitled to see. The leader
likewise keeps `seq_L`, not the cohort's last.

**`seen_generation`.** Every member — and the leader — records the single
generation the gate hold produced. `refresh` only asks whether it moved, and
`commit_generation`'s doc comment already allows an increment without a
successful commit. The change in meaning ("one gate hold, covering 1..=N
attempts") is real and gets a doc-comment sentence and a test.

## 7. `Durability::Normal` vs `Full`, and interrupts

**Unaffected in kind.** The barrier a cohort runs is
`CommitCoordinator::effective_durability()`, the same choice a solo commit
makes, made once instead of N times. `Full` is `fsync`/`F_FULLFSYNC`;
`Normal` is the platform's weaker barrier. Neither level's loss bound moves.
What moves is the typical size of the loss at `Normal`: a cohort's records
are contiguous and share one barrier, so a lost barrier loses the cohort
rather than one commit. Within the documented bound; stated in
`docs/recovery.md` anyway (§5).

Checkpoints are unchanged and still use the unconditional `sync`, never
`effective_durability` — the argument in `FileDevice::sync`'s doc comment
is untouched by any of this. A checkpoint also never joins a cohort: it uses
`begin_commit`, not `begin_normal_commit`, so it never increments
`normal_inflight`, is never offered, and is never absorbed. A checkpoint
holding the gate means `normal_inflight == 0`, so `absorb_offer` declines
and a writer arriving behind a checkpoint parks on the gate exactly as
today.

**Interrupts.** There is still no cancellation or timeout anywhere in this
codebase that reaches a parked gate wait, and this slice does not add one.
A follower is blocked on one condvar it was not blocked on before
(`absorption_done` rather than `reservation_done`), which is the same
statement as slice 1's: the wait is moved, not added, and the pre-existing
gap is inherited rather than widened. The one thing this slice must not do
is make a future cancellation *impossible*, and it does not — an interrupted
`absorb_wait` would un-park and return `Fallback`, which is already a
supported answer.

## 8. Crash points, exhaustively

Every write/sync boundary in §2, what recovery lands on, and who was
acknowledged. "Acknowledged" means a `commit()` call returned `Committed` to
its caller.

| # | Crash at | On-disk after recovery | Acknowledged |
| --- | --- | --- | --- |
| 1 | Before step 4's `write_dirty_pages` | Nothing from this cohort. Recovery lands on the pre-cohort state. | Nobody. Every member is still blocked in `absorb_wait`, and the process is gone. |
| 2 | During step 4/6's page writes (any member) | Unreferenced pages in the data area, no record naming them. Recovery lands on the pre-cohort state; the pages are garbage a later `next_page_id` will overwrite. Identical to a solo commit that died between pages and record. | Nobody. |
| 3 | After every page write, before step 7's record write | Same as 2. **This is the row the task names: leader crash after followers' pages are written but before the sync — no follower is acknowledged and recovery drops the entire cohort tail.** | Nobody. |
| 4 | *During* step 7's single record write (torn) | `scan_region` decodes the longest whole-record prefix. Recovery lands on member *k* for whichever *k* survived; k+1..N are dropped. §5. | Nobody — step 11 has not run. |
| 5 | After step 7, before step 10's barrier returns | The records are in the page cache but not necessarily on the platter. A power loss may lose any suffix (or all) of them; a process crash loses none, because a returned `pwrite` outlives the process. Recovery lands on a whole-record prefix either way. | Nobody. |
| 6 | After step 10's barrier returns, before step 11 resolves anyone | **Every record is durable.** Recovery lands on member N. | Nobody, in this process. §9 is how a member finds out. |
| 7 | After step 11 resolved members 1..k, before k+1 | Same as 6 — durable through member N. | Members 1..k were told `Committed`, truthfully: their bytes were durable before step 11 began. | 
| 8 | After step 12 releases the gate | Durable through member N; every member acknowledged. Ordinary steady state. | Everyone. |

The invariant every row above is an instance of: **no member is acknowledged
before step 10's barrier returns, and once it has returned every member's
record is durable.** There is no crash point at which a member has been told
`Committed` and its bytes are not on the platter, and none at which a
member's rows are on the platter and a *different* member was told
`Committed` without its own being there too — the chain is a prefix, and a
prefix's members are exactly the ones resolved.

Compare row 3 against the pre-slice-2 protocol: today, N writers crashing at
the same instant lose between 0 and N commits depending on where each was;
here they lose all N. That is the blast-radius trade, and it is the correct
one, because "all N" is the only outcome in which no member was ever told
anything.

## 9. Leader crash after the sync but before acknowledgement (row 6), spelled out

The task asks explicitly how a follower discovers, on recovery or retry,
that its commit landed. There are two distinct failures wearing similar
names, and they have different answers.

**(a) The process died (a real crash).** Then the follower's thread died
too. There is no one to tell, and nothing in this protocol could have told
them: the caller's `commit()` never returned, so from the application's
point of view the transaction's outcome is *unknown*, which is the state
every durable system leaves a caller in when the machine dies mid-commit.
What the follower's transaction gets is the only thing that matters — **it
is durable, and a reopen sees it.** `read_committed_state` accepts the
cohort's whole chain (row 6), so the rows are simply there. This is
identical to a solo commit that crashed between its `fsync` returning and
its `Ok(Committed)` reaching the caller, which has always been possible.
**Tested:** `a_cohort_synced_before_the_crash_is_wholly_visible_after_reopen`
crashes the simulator immediately after the cohort's barrier and asserts the
reopened image contains every member's rows and no more, and that the
recovered `seq` is member N's.

**(b) The leader thread unwound (a panic).** The process lives, so the
followers' threads live and must be told something. They are:
`release_normal_reservation` — reached from `NormalCommitGuard::drop` on the
unwind — resolves every taken-but-unresolved member with
`AbsorbResult::Failed`, and their `commit()` calls return `Err`, not a
wrong outcome and not a hang. `Failed` is deliberately *not* `Fallback`: the
records may be on the file, and telling a member "commit again" would
double-apply. `Err` after a possible append is exactly the ambiguity a solo
commit already has when its own `sync_commit` fails, and the caller's
recourse is the same one — reopen or `refresh` and look. **Tested:**
`a_leader_panic_after_the_barrier_fails_every_member_rather_than_hanging`
(in `crates/inlaysql`, where there are threads) and, in the core, an
injected error at the same boundary.

## 10. The tests, and the mutation each one kills

**Core, `crates/inlaysql-core/tests/dst_sweep.rs`** — the existing
`AbsorbingDevice` harness gains the slice-2 methods; `park_for_absorption`
still stands in for a parked thread, and the sim's `absorb_wait` is a
non-blocking lookup because the leader has always already resolved by the
time a single-threaded sweep calls it.

1. **`absorption_matches_serial_commit_order`** (extended to 250 seeds) —
   the parity test. Same cohorts, flag off vs on: identical per-transaction
   outcome vectors and byte-identical final scans. *Kills:* judging out of
   order; judging member *j* against `root_L` instead of `root_{j-1}`;
   folding a conflicting member into the chain; letting a conflicting member
   consume a seq; dropping a member's ops; replaying ops in map order rather
   than through `put`/`delete`.
2. **`a_follower_conflicts_with_an_earlier_member_of_its_own_cohort`** —
   kept from slice 1, now asserting the leader really wrote the members'
   records (`cohorts`/`members`/`committed` counters) rather than everyone
   falling back. *Kills:* "a conflict stops the cohort"; silent wholesale
   fallback that would make every other assertion vacuous.
3. **`a_cohort_is_one_append_and_one_sync`** — a counting device wrapper
   asserts exactly one `write` in the WAL region range and exactly one
   `sync` per cohort, and that the number of records recovered equals the
   number of committed members. *Kills:* per-member appends sneaking back;
   per-member syncs; a leader that syncs before writing the blob.
4. **`the_cohort_chain_is_one_contiguous_prefix_validated_record_by_record`**
   — decode the leader's region after a cohort and assert N+1 records at
   consecutive offsets with linked `prev_seq`/`prev_root`. *Kills:* a wrong
   `prev_root` (the previous member's *pre*-rebase root, the easy mistake);
   a duplicated seq.
5. **`a_torn_record_in_the_middle_of_a_cohort_keeps_exactly_the_prefix`** —
   truncate the cohort blob after member *k*'s record for every *k*, reopen,
   assert the recovered state is member *k*'s exactly and that no member
   past *k* has any row present. *Kills:* recovery accepting a record whose
   predecessor is missing; a cohort written in an order that leaves a later
   record decodable after an earlier one is gone.
6. **`crash_at_every_step_of_a_cohort`** (`--ignored`) — the `FaultSchedule`
   drives a crash at every write/sync index inside one cohort. Asserts the
   recovered image is always a chain prefix, and that **a member's rows
   appear only when that member's own record is in the recovered chain**.
   *Kills:* the leader writing a member's record before that member's pages;
   the record blob written before the pages; a member acknowledged whose
   record is not in the prefix.
7. **`a_cohort_synced_before_the_crash_is_wholly_visible_after_reopen`** —
   §9(a). *Kills:* a barrier that does not actually cover every member's
   ticket.
8. **`a_region_wrap_closes_the_cohort`** — sized so the buffer overflows
   mid-cohort, asserting a wrap actually happened somewhere across the sweep
   (the `pages_reused` discipline) and that the members after the boundary
   fell back and committed correctly. *Kills:* wrapping mid-cohort; silently
   dropping the overflowing member.
9. **`a_device_error_mid_cohort_commits_nobody`** — inject a write failure
   at member *k*. Asserts every member (and the leader) reports an error or
   falls back, that nothing from the cohort is in the recovered image, and
   that a retry then succeeds. *Kills:* partial cohort acknowledgement;
   a leader that publishes a commit point for records it never wrote.
10. **`sweep_multi_writer` with absorption on for one seed in three**
    (`--ignored`) — the existing multi-writer sweep under fault injection,
    with cohorts. *Kills:* anything that produces a recovered state no
    interleaving could have produced.
11. **`generation_advances_once_per_gate_hold_not_once_per_member`** —
    pins the meaning change in §6. *Kills:* a `refresh` that starts
    depending on the counter's step size.

**`crates/inlaysql`, real threads** (`tests/concurrent_writers.rs`):

12. **`writers_agree_with_absorption_on`** — the existing threaded writer
    tests with the flag on, asserting cohorts actually formed
    (`absorption_stats`) and every row is present exactly once.
13. **`no_follower_is_acknowledged_before_the_barrier`** — a device wrapper
    records the barrier's completion time and each follower's
    `commit()` return time; asserts every return is after the barrier.
    *Kills:* resolving before the sync — the single most important
    invariant in this slice.
14. **`a_leader_panic_after_the_barrier_fails_every_member_rather_than_hanging`**
    — §9(b), with a bounded join timeout so a hang fails the test rather
    than hanging it. *Kills:* the missing RAII resolve.
15. **`a_checkpoint_concurrent_with_a_cohort_still_makes_progress`** —
    the existing checkpoint test shape, with absorption on. *Kills:* a
    checkpoint being absorbed; a leader deadlocking against one.

**Everything else is the gate:** `free_list_reuse_dst`, `backup_dst`,
`durability_dst`, `index_recovery_dst`, and the full workspace suite with
the flag off — which is the evidence that a device that never opts in is
byte-for-byte unchanged.

## 11. What would make this slice fail

The brief's STOP condition for this slice is a throughput one, and it is
inverted relative to slice 1's: slice 1 was allowed to be flat because its
value was the machinery. **This slice has no value if it is flat.** The
measurement is `--txns 150` at 1/8/16/32 writers, flag off vs on,
interleaved, three repetitions with the control re-run inside each, and the
prediction on record is **1.9-2.7x at 32 writers**.

* **Flat or negative at 16 and 32 writers** → the in-gate barrier's
  serialisation costs as much as the removed gate acquisitions bought, the
  flag stays off, the number is recorded, and the next question is whether
  the barrier belongs outside the gate after all (§3, with the watchdog
  problem to solve first).
* **Any DST scenario in §10 failing** → stop, regardless of the number. A
  mistake here is a data-loss bug.
* **p99 materially worse** → the in-gate barrier makes the tail the whole
  cohort's, so this is the risk to watch; a win in throughput bought with a
  tail worse than the already-disclosed ~8x-vs-SQLite figure is the brief's
  Slice 4 STOP condition arriving early.

And whatever the number: **the default stays `false`.** Turning it on is a
call about a protocol whose worst case is "lose a cohort instead of a
commit", and that is the user's to make, not the measurement's.

## 12. What the measurement said (added after the fact, 2026-09-03)

**0.90x at 32 writers, not 1.9-2.7x**, and two of this document's own
arguments were wrong. The full write-up, with the tables, is `PERF.md`'s
AHL-547 section; the corrections belong here, next to the claims they
falsify.

**§3 is retracted.** The in-gate barrier was argued four ways and measured at
787 commits/s against a 1,689 control at 16 writers, with p99 at 187 ms
against 49 ms. Holding the gate across the `fsync` stops the next cohort
doing its gate work while this one syncs, and that overlap is where this
engine's concurrent throughput comes from. The barrier belongs outside the
gate, and the watchdog §3 was written to avoid had to be built after all:
`CohortGuard`, an RAII guard in the leader's own `FileDevice` covering the
gate release and the barrier after it, which is exactly the span
`NormalCommitGuard` cannot reach.

**§1's `Failed`-for-a-record-that-does-not-fit was wrong too**, and not
rarely: the region's remaining room shrinks as a cohort's buffer grows, so at
16 writers the bench stopped outright on it. Nothing of such a member reaches
the disk, so it is refused and handed its transaction back instead — which
costs one copy of the offered map per member, the only copy this protocol
makes.

**The finding the measurement is actually for.** Cohorts form (4.3-5.8
members), the median commit really does get faster (28.2 ms against 36.0 ms
at 16 writers, exactly as §11 predicted the gate saving would look), and the
engine nonetheless runs **26% more barriers**. The flush-side group commit
was already amortising the `fsync` over 6-9 transactions by gathering tickets
from writers publishing them independently — and a follower under absorption
never reaches `sync_commit`, so it never publishes one. The commit-side
cohort is 5.6 transactions; the flush-side cohort it displaces is 9.2. **The
two group-commit layers are not composable as built; they compete for the
same population, and the earlier one gathers less.** Section 5 of the brief
priced the gate and never priced that.

So the flag stays off, and this is a "do not turn this on" result rather than
a "left to the caller" one. What would flip it is in `PERF.md`: the layers
have to compose (the brief's Slice 4, which this measurement reclassifies
from an optimisation on top of Slice 3 to a precondition for Slice 3 paying
at all), and a cohort has to survive a WAL-region boundary rather than losing
a third of its members to one.
