# Crash recovery in InlaySQL

This document describes how the Stage 2 storage engine survives crashes, power
loss, torn writes and reordered syncs, and how it recovers a consistent state
on the next open. It is the prose companion to the code in
`crates/inlaysql-core/src/wal.rs` and
`crates/inlaysql-core/src/btree/tree.rs`.

## On-disk layout

The device is divided into fixed, `page_size`-sized blocks:

```text
block 0                          header   (magic, page size, format version)
block 1                          state    (root, next page, checkpoint seq)
blocks [2, 2 + 4 * WAL_BLOCKS)   wal      (four append-only writer regions)
blocks [2 + 4 * WAL_BLOCKS, ...) data     (copy-on-write B-tree pages)
```

* **Header** — written once, at `create`, and never overwritten. It is the one
  structure that cannot be recovered if torn, because it names the page size
  everything else is addressed in.
* **State block** — the committed root, the next free page id, and the highest
  commit sequence number that has been *checkpointed*. Rewritten at each
  checkpoint; recoverable from the log if torn.
* **Write-ahead log** — four append-only regions of self-describing,
  checksummed commit records. Each native file handle is assigned a region;
  handles beyond four safely share a region because append placement is
  reserved under the same short commit gate. `WAL_BLOCKS` is 256 blocks per
  region.
* **Data area** — the copy-on-write B-tree pages, including overflow pages. A
  page is written once and never modified in place.

## Commit protocol (write-ahead)

A commit is one short reservation followed by a single `sync`:

1. Under the process-local commit gate, refresh the committed root and apply
   first-committer-wins. A stale transaction whose row keys are disjoint is
   rebased; an overlapping row change returns `Error::Conflict`. Reserve the
   next sequence/page range and append position, then leave the gate.
2. Write the transaction's dirty pages to the data area. They are fresh,
   never-before-used page ids, so a partial write can never clobber live data.
   Ids within one transaction are allocated consecutively, so this is normally
   one device write for the whole commit rather than one per page.
3. Append a commit record to this handle's WAL region. Besides its new root,
   next-free-page and copied pages, it names the exact predecessor sequence and
   root, making the cross-region order explicit.
4. `sync` the handle outside the gate. This is the commit point: the record and
   its pages become durable together. Other handles can sync their own regions
   at the same time.

Step 1's "refresh the committed root" is where the gate's cost lives, and it is
why the gate is where concurrent-writer throughput was decided. Deriving the
committed state from the file means reading the state block and scanning every
region, and a scan decodes each record *whole* — every data page the commit
copied, plus a checksum over all of it — because that is what recovery needs.
Doing that under an exclusive gate makes every writer pay for every byte
committed since the last checkpoint, which is exactly as serial as SQLite's file
lock and grows as the log fills. AHL-468 measured it at ~75% of the gate's hold
time, with the gate held ~99% of the wall clock at four writers and up.

So a device that can prove it speaks for every writer on the file answers from
memory instead (`Device::commit_point`). This is the same argument
`Device::commit_generation` already makes, one step further: that one lets a
handle skip a scan when *nothing* was committed since it last looked, this one
when something *was* — the only case that arises once writers share a file. It
rests on the same proof, `FileDevice::open`'s exclusive OS advisory lock, and it
has the same limit: a handle that admits a writer it cannot see must answer
`None`, and so must a fault-injecting simulation device, where a fault rolls the
*readable* image back to the durable one and the file's committed state really
can go backwards under a live handle. Real files cannot do that — a `pwrite`
that returned survives until the machine dies, and then so does the process — so
the deterministic sweeps keep exercising the derivation, and a separate test
(`a_cached_commit_point_is_always_what_reading_the_file_would_derive`) checks the
cached answer against a fresh derivation on every commit, checkpoint, refresh
and region wrap.

Only a commit or a checkpoint, inside the gate, may change that cached answer,
and either may instead *withdraw* it: a wrap or a checkpoint rewrites a region
from the start, and a failure part-way through leaves nobody knowing where the
next record goes. Withdrawing costs a scan; a wrong answer would place a record
on top of a live one.

Step 2's page ids are fresh with respect to every *committed* state, which is
not the same as fresh with respect to this transaction. A statement usually
issues several writes (the row, then the engine's row-id and change-version
metadata), and each one copies the root-to-leaf path. The second write's path
copies pages the first write allocated — pages that have never been on the
device and are reachable only from this transaction's own working root — so it
writes them back into the same slots rather than allocating again. Copy-on-write
is a rule about pages a *reader* could be holding; a page this transaction
allocated is not one. Doing otherwise left the superseded copies in the commit,
where they were written to the data area and copied into the record on their way
to being unreachable: roughly half the bytes of a single-row `INSERT`.

Step 2's "never-before-used" is load-bearing, and it is an invariant the code
has to maintain rather than a fact about copy-on-write. Step 1 adopts the
committed state, and the committed state can go *backwards* — a `sync` that
reported success without reaching the platter leaves the file naming an older
commit than this handle has already written pages for. The root must follow it
back; the page allocator must not, or the next commit hands out ids that are
already occupied and the page cache (whose only key is the page id) starts
serving the previous occupant. `CowBTree::adopt_next_page_id` is where that is
enforced: the allocator is monotonic per handle and never rewinds. This was
AHL-406, and it recovered a database to a tree made of two different commits at
once, with no checksum or decode failing anywhere.

Step 4 is where group commit lives (`FileDevice`'s `CommitCoordinator`, AHL-461).
A handle reaching `sync` takes a ticket immediately, after its own writes have
already returned from `pwrite` — a real POSIX guarantee, since `fsync` flushes
everything already written to a file's inode regardless of which descriptor
wrote it. One handle at a time becomes flush leader and calls the real
`fsync`/`F_FULLFSYNC`; a follower whose ticket the leader's flush target
already covers returns durable without touching the disk, and a follower whose
write only completed after the leader had already captured that target
fsyncs for itself instead. A solo commit always becomes its own leader
immediately, so the uncontended path pays one ticket and one uncontended lock,
never a wait or a timeout. This changes *which* handle's syscall makes a given
commit durable; it never changes when a commit is allowed to report success.

The record being self-contained is the crux. A torn write is modelled as
"only part of the most recent write survives, every other unsynced write is
lost" — so the record (written last) can survive while its data-area pages do
not. Because the record carries the pages, recovery can rebuild them. A record
that never survives a torn write is simply not a commit.

The state block is *not* rewritten on every commit. It is rewritten on
checkpoint (see below), which keeps the hot path to one sync.

## Checkpointing

A checkpoint refreshes and rewrites the state block (committed root/next/seq)
under the commit gate and syncs it, then lets the calling handle reuse its WAL
region from the start. Records left in the other regions are older than the
checkpoint sequence and therefore ignored. Each region is bounded; a
transaction whose record does not fit one region is rejected with an error.

## Recovery protocol

On `open`:

1. Read the header. A torn header is fatal — the database was never durably
   created, and there is nothing to recover.
2. Read the state block. If torn, it is treated as absent.
3. Scan each WAL region independently to its first empty, torn or corrupt
   record. A tear stops only that region; it cannot hide valid records in the
   others.
4. Merge valid records by sequence number and accept only the contiguous chain
   whose `prev_seq` and `prev_root` match the state reached so far. An
   incomplete branch can never splice pages into another transaction's state.
5. Replay every accepted record newer than the state block, in order, into the
   data area. This heals torn pages from an earlier region as well as from the
   newest record, then the state block is checkpointed.

The recovered tree is always a state the workload actually committed: never a
mix of two commits, never a torn page.

## Overflow pages

A value larger than a page cannot be stored inline in a leaf cell, so it spills
into an **overflow chain**. The leaf cell holds a pointer (the first page id and
the value's total length) instead of the bytes; each overflow page holds a
slice of the value plus a pointer to the next page, and the last page's pointer
is zero. Overflow pages are pages like any other: they are copy-on-write, so
they are never modified in place, and they enter a commit exactly as a B-tree
page does — written to the data area *and* copied into the commit record.

That self-contained record is what makes the chain crash-safe:

* **A crash before the commit syncs** loses the whole row, never half of it —
  the leaf pointer and every overflow page reach the device in the same sync.
* **A torn write that keeps the record but loses an overflow page** is healed
  on recovery: the record carries every page the commit wrote, overflow pages
  included, so the chain is rebuilt byte-for-byte.
* **A torn write that tears the record** is not a commit, exactly as for any
  other record.

The chain is read lazily: `get` and `scan` follow the pointer only when the
value is requested, and the total length the leaf stores lets the reader bound
the walk and detect a short or corrupt chain.

## Format versions

The header carries a format version, and it now means something:

* **1** — the walking skeleton (redb-backed).
* **2** — the copy-on-write B-tree with a write-ahead log.
* **3** — overflow pages: a leaf cell may point at an overflow chain instead of
  storing a value inline.
* **4** — opt-in scalar-quantised vectors: catalogs, rows and persisted ANN
  indexes may carry the `VECTOR(n, INT8)` encodings.
* **5** — four per-writer WAL regions; records carry predecessor sequence/root
  links and recovery merges the regions into one validated commit order.

Policy: this build creates **version 5** files and opens versions **3, 4 and
5**. Versions 3 and 4 retain their original single-region layout and remain
read/write compatible; they do not silently acquire the v5 layout.

Version 3 is the one compatibility exception to the pre-1.0 no-migration
policy: its page/WAL layout is identical, and its exact-vector catalog and row
tags remain unchanged. A v3 database opens and continues to read/write exact
columns, but `CREATE TABLE ... VECTOR(n, INT8)` is refused. The immutable v3
header is never silently made to describe v4-only values; recreate the file to
opt into quantisation.

* A file with a **newer** version is refused with an error that says it is
  newer — it is from a future build, not corrupt, and the message says to
  upgrade InlaySQL.
* A file with an **older** version (1 or 2) is refused with an error that says
  it is older. Pre-1.0 formats are **not migrated**: recreate the database.
  Once the format stabilises a migration may be written, but a database format
  change without a way to say "this file is older than you" is how a
  pre-release database becomes unopenable with no explanation.

A version mismatch is a distinct error (`Error::FormatVersion`), never
corruption. Tests prove both directions and that a v3 exact-vector file is
grandfathered.

### The catalog's version is its own

The catalog carries a *second* version, inside the value stored under the
`catalog` metadata key, and it moves independently of the header's — see
`crates/inlaysql-core/src/catalog.rs`. It is at **6** now: 2 added index
declarations, 3 the `VECTOR(n, INT8)` tag, 4 declared constraints and the
`NUMERIC` affinity, 5 scalar B-tree indexes, and 6 declared collations
(AHL-469), on a column and on each column of an index.

Two rules make that safe, and they are the same two the header's version has.

**A catalog is written at the lowest version that can express it.** A database
whose columns declare no collation is still written at 5, or 4, or 2, so
opening and editing it does not make it unreadable to the build that created
it. Only a column or index that actually declares something other than `BINARY`
forces version 6.

**Recreate, not migrate.** A build that predates version 6 refuses a version-6
catalog with `Error::FormatVersion` and reads nothing. That refusal is
load-bearing rather than cautious: a `NOCASE` index keys the *folded* value
(`crates/inlaysql-core/src/index.rs`), so a build that decoded the index
declaration without its collation would probe the unfolded bytes, find no
entry, and answer `WHERE name = 'ADA'` with nothing while the row was still in
the tree. Losing rows quietly is worse than refusing to open, which is the same
argument version 5 made for the B-tree index declarations themselves.

## Failure modes

| Fault | What the model does | Outcome |
| --- | --- | --- |
| Clean sync | writes become durable | commit is durable |
| Crash | all unsynced writes are lost | last commit (if not yet synced) is lost; earlier state intact |
| Torn write | last write's prefix survives, others lost | record survives → commit durable and its pages rebuilt; record torn → commit never happened |
| Reordered sync | durable rolls back to an older snapshot | the reordered-away commits are lost; recovery lands on a consistent earlier state |

## Deterministic simulation testing

The engine runs entirely on the simulation harness in `crates/inlaysql-core/src/sim`
(no wall clock, no syscalls — the core crate is `no_std`). A seed drives the
workload *and* the fault schedule, so a crash, torn write or reordered sync can
be injected at any sync and replayed byte-for-byte on any machine.

`crates/inlaysql-core/tests/dst_sweep.rs` sweeps thousands of seeds, each a
randomized workload with randomized faults, and asserts the recovered database
is byte-for-byte one of the states the workload committed. A separate sweep
interleaves four writers across all WAL regions, including real conflicts, and
asserts recovery always lands on a state produced by some committed
interleaving. Both run in CI.

## What lifting the one-region ceiling would take

"Each region is bounded; a transaction whose record does not fit one region is
rejected with an error" is one sentence in *Checkpointing* above, and it is the
whole of `docs/enterprise-readiness.md` blocker 5: `DELETE FROM t` on a large
table, a bulk `INSERT ... SELECT` and a wide `UPDATE` are hard errors rather
than slow paths. `crates/inlaysql/tests/large_statements.rs` pins where each
one breaks. This section records what the fix costs, because two of the three
obvious answers are wrong in ways that are only obvious once written down.

**Why the constant is load-bearing.** `WAL_BLOCKS` is not a tuning knob. The
data area begins at `(region_count × WAL_BLOCKS + 1) × page_size`
(`wal::data_offset_for`), so the region's size is baked into the address of
every page in the file: changing it relocates the entire database. That alone
makes it a format change, before anything about the protocol is touched.

**Why the record copies the pages.** Not redundancy for its own sake. Under the
torn-write model above, the record is written last and is the only thing
guaranteed to have a surviving prefix; the data-area pages it describes may be
gone. A record that cannot rebuild its own pages is not a commit. So the record
is O(bytes the commit wrote) by construction, and the ceiling follows.

**What does not work.**

* *Spilling one commit into the other three regions.* Ceiling 1 MiB → 4 MiB,
  paid for with the per-writer region ownership that keeps the reservation gate
  short: a writer would have to take three more regions' append reservations
  under the gate, and `Device::commit_point(region)` — the cached answer AHL-468
  exists to provide — would have to survive a foreign writer having moved a
  region it does not own. Recovery would need multi-part records that are
  accepted only when every part is present. All of that for 4×, which does not
  change the answer to "can I delete this table".
* *Committing a large statement in several durable batches.* This makes
  `DELETE FROM t` "succeed" by making it non-atomic: a crash halfway leaves it
  half-applied. A statement that reports success having done part of itself is
  strictly worse than one that refuses, and it is what architecture rule 5
  forbids. The engine does batch internally — `purge_index_entries`, the index
  save — but only where it owns the transaction and the thing being written is
  derived state it can rebuild. That argument does not reach a user's `DELETE`.
* *Spilling the pending write set out of memory.* The bound is not memory.
  `CowBTree::pending_record_len` is computed from the same `dirty` map that is
  the resident buffer, so the two are the same quantity and the WAL record
  refuses first.

**What would work.** Stop the record's size tracking the commit's size. The
page bytes in the record are, in the no-fault case, an exact duplicate of what
step 2 already wrote to the data area at *fresh, never-before-used* ids. Two
shapes follow from that:

1. **Spill the payload, name it from the record.** Write the page bytes to a
   run of fresh data pages with their own length and checksum, and append a
   small record naming `(page id, len, checksum)` per chunk. Recovery validates
   every chunk before accepting the commit; any chunk that fails makes the
   record not a commit, exactly as a torn record is not a commit today — and
   rejecting is safe for the same reason step 2's writes are safe, because
   nothing reachable from the previous committed root lives at those ids. The
   record shrinks to roughly 20 bytes per 4 KiB of commit, so one region covers
   a few hundred MiB. Cost: the spilled pages are consumed permanently (never
   recorded in the free list, reclaimed only by `vacuum`), and it is a new
   record kind — format version 6.
2. **Give every data page a checksum and name pages instead of copying them.**
   Strictly better where it applies — no spill pages, ~340× smaller records —
   and it retires the "a data page carries no checksum of its own" caveat that
   both `btree/backup.rs`'s argument and the page-reuse liveness proof above
   currently have to work around. Cost: the page header changes, so
   `page::decode` and every page ever written change with it, and recovery
   grows a validate-don't-replay path beside the existing replay path, because
   v5 records still carry bytes.

Either is a format version 6 change to the on-disk record layout **and** the
recovery protocol, so per architecture constraint 3 it needs a deterministic
simulation pass of its own — and specifically a sweep that asserts it actually
exercised the spilled path, the way `free_list_reuse_dst.rs` asserts
`pages_reused()` is nonzero. A sweep whose workloads are all small enough to fit
one region would pass without testing anything. The failure mode being guarded
against is recovery accepting a commit that is missing pages, which is silent.

One thing neither shape fixes: a whole-table `DELETE`'s record is dominated by
the change log, not by the rows. `cdc.rs` writes one entry per changed row,
repeating the table's name in each, so `DELETE FROM t` is bounded near tens of
thousands of rows however large the record may become. That needs a summary
form in the change log — and per `cdc.rs`'s own reasoning it must be an honest
one, since a silently truncated list is indistinguishable to a consumer from
nothing having happened.

## Known limitations

* **Reordered sync during a checkpoint truncation.** A reordered sync that
  lands exactly on the checkpoint that reuses the log region can roll back
  pages whose records were already overwritten. The engine does not silently
  corrupt data — recovery detects the inconsistency — but it does not yet
  reconstruct those commits. Hardening this interaction is a follow-up; the
  sweep currently injects crash and torn-write faults, the two primary
  crash-safety concerns.
* **Space reclamation is opt-in, and now reachable from the public API**
  (Phase 2 item 6, AHL-481) — see "The free list and page reuse" below for
  the design and its proof. `EngineOptions::page_reuse` reaches it through
  `Database::open_on_with_options`, and `inlaysql vacuum <path>` does
  whole-file compaction, deliberately as a copy into a fresh file plus an
  atomic rename rather than in-place page rewriting — see `crates/inlaysql/src/vacuum.rs`'s
  module doc for why that keeps it outside this file's crash-recovery
  surface entirely, needing none of the DST proof below. What remains true:
  reclamation can only prove liveness for readers this process's reservation
  gate can see, so it is unsound to enable `page_reuse` beside a concurrent
  `FileDevice::open_read_only` on the same file, in this process or any
  other — that mode takes no OS lock by design (AHL-405) and is therefore
  invisible to the proof, not merely untested. `vacuum` itself does not have
  this problem: see its module doc for why a lock-free reader is safe
  through it. Also still true: no build older than this one can refuse to
  open a file that has actually undergone page reuse — see "What this
  deliberately does not include, still" below for why that is not a live
  risk yet, and what would make it one.

## The free list and page reuse (Phase 2 item 6, AHL-481)

Every copy-on-write mutation replaces at least the root-to-leaf path it
touches, leaving the old pages unreachable from the new root. Until this
item, those pages were never tracked: the data area grew forever, and
`btree/tree.rs`'s allocator (`CowBTree::alloc_page`) was a monotonic counter
with no way to hand an id back out. That was deliberate, not an oversight —
`btree/cache.rs`'s module doc explains why: **a page id names one immutable
sequence of bytes for the lifetime of the file** is the entire reason the
page cache needs no invalidation protocol, and AHL-406 is what it costs to
violate that by accident (a rewound allocator after a lost checkpoint sync
handed out an id twice; the cache served the previous occupant with no
checksum or decode failure anywhere). Reusing a page id on purpose needs
every one of the following, in order, or it reopens that exact bug.

**1. The page cache and the retained read cursor are versioned first.**
`CowBTree::set_page_reuse(true)` is the only thing that changes this: every
point where a handle's committed root moves forward
(`commit`/`checkpoint`/`refresh`/a rebase) now also calls
`CowBTree::invalidate_for_reuse`, which clears the page cache
(`btree/cache.rs::PageCache`) and drops the retained leaf cursor (AHL-472)
before the new root is trusted. This is coarser than the per-entry
`(page id, commit sequence)` stamp `cache.rs`'s warning originally proposed —
a root change from *any* commit clears the cache while reuse is on, not only
one that actually reused something — deliberately: proving "the cache is
empty" is trivial, where proving "every lookup path checks a stamp
everywhere it could read one" is not, and an early, more precise design
(reading a durable "has anything been reused" counter to decide *whether* to
clear) turned out to be circular — the read needed to answer that question
would itself walk the tree through the very cache it was deciding whether to
trust. With reuse off (the default, and every existing caller), this method
does nothing, not even the two `RefCell` borrows — see `CowBTree::set_page_reuse`'s
doc comment. AHL-478's `Rc<[u8]>` row bytes need no separate fix: they are
only ever handed out from a cache lookup that has just been proven correct,
so the cache's own correctness is the whole of the audit for them.

**2. Superseded pages are recorded per commit, as ordinary rows.** Following
D3's precedent for secondary index entries, a freed page becomes a row under
a reserved key prefix (`\x02free\0`, disjoint from `\0`'s metadata keys and
`\x01idx:`'s index entries) in the *same* tree, keyed by `(freed-at sequence,
page id)`. That means no new WAL record shape, no state-block field and no
bespoke recovery path: the row rides the same commit, the same sync and the
same crash-atomicity guarantee as every other write in the transaction
(`CowBTree::finalize_free_list`, called from `commit` after the transaction's
own changes have settled). Overflow chains are freed the same way when a
value that used to overflow is replaced or its row deleted
(`CowBTree::free_overflow_chain`) — previously that left every page in the
chain as unrecorded garbage. All of this is gated on `reuse_enabled`: with it
off, `CowBTree::supersede` is exactly the no-op cleanup it always was, so a
database nobody opts into this feature for is byte-for-byte what it always
produced — no free-list row is ever written, which matters because an
unconditional one would show up in a raw `scan()` and change what every
existing DST seed and several unit tests compare against.

**3. A page is only reused once two separate proofs both clear it**
(`CowBTree::refill_free_candidates`): durability and liveness, and either
answering "unknown" declines the candidate rather than assuming it is safe.
  * *Durable* — `Device::commit_point`'s `seq` covers the freeing commit.
    Never the handle's own `self.checkpoint_seq`/`self.next_seq`: those are
    updated the instant a handle *believes* a sync succeeded, and on a
    fault-injecting device that belief can be exactly the shape of wrong
    AHL-406 was — a checkpoint's own sync reporting success without reaching
    the platter while a later, unrelated sync makes some other write durable
    regardless. `commit_point` is the one answer already built to be
    trustworthy here: `Some` only for a device that holds this process's
    exclusive OS lock (`FileDevice::open`) or is otherwise provably
    single-writer, `None` from every fault-injecting or read-only device (see
    its doc comment). A device that never answers `Some` here simply never
    reclaims a page — safe, only less space-efficient. (Working through why
    an ordinary commit's *pre-sync* population of `commit_point` is still
    sound — the `prev_seq`/`prev_root` chain validation recovery already does
    for every WAL record is what protects it — while a *checkpoint's*
    population is not equally protected on a real file only because a real
    `fsync` failure never lies: it returns `Err`, propagates out of
    `checkpoint` before the region is ever zeroed, and the commit point is
    never published. That argument is the same one `commit_generation`'s doc
    comment already makes for a real file; it is why the reuse DST sweep
    below needed a device that reports sync failures honestly instead of
    reusing `Simulator`'s own `Device` impl, which never does.)
  * *Live* — `Device::min_reader_seq` is at least that commit's sequence:
    no reader this device can see is pinned to an older root. Backed by a
    small per-file registry (`FileDevice`'s `CommitCoordinator::readers`)
    every read-write `CowBTree` handle updates at the same points it already
    updates its cache epoch. `None` here — the default, and the honest answer
    for any device that does not track readers — also declines every
    candidate, for the same reason `commit_point` does.

**The cross-process answer, stated plainly: reclamation cannot rule out a
read-only reader.** `FileDevice::open_read_only` (AHL-405) takes no OS lock
at all, by design, so it never registers with the coordinator above — in
this process or any other. There is no way to distinguish "no reader exists"
from "a reader exists that this device cannot see," so `set_page_reuse(true)`
is a handle-level opt-in with that constraint spelled out in its own doc
comment, not a default, and not something a future `EngineOptions` flag
should flip on without deciding how to communicate the same constraint to an
embedder turning it on for a `Database`.

That last sentence has since been answered once, and the answer is the
pattern to copy. The MySQL server exposes it as `serve --mysql --page-reuse`,
off by default, and communicates the constraint three times over: in
`ServerOptions::page_reuse`'s doc comment, in `docs/server.md`, and — because
the constraint is about *other processes*, which only the operator can rule
out — in a warning the server prints at startup naming `inlaysql serve --mcp`
(a read-only opener of the same file) as the concrete thing it forbids. The
server also had to stop holding its lock-keeping handle as a `Database`: an
idle read-write handle pins `min_reader_seq` at the sequence it last read, so
it would have silently declined every candidate for the life of the process,
turning the flag into pure cost. It holds a bare `FileDevice` instead, which
takes the same OS lock and registers no reader.

**DST coverage for reuse specifically**
(`crates/inlaysql-core/tests/free_list_reuse_dst.rs`) uses a purpose-built
`TrustedDevice` over the same `Simulator`/fault schedule the rest of this
suite runs — see its module doc for why `Simulator`'s own `Device` impl
cannot exercise this at all (`commit_point`/`min_reader_seq` both default to
`None`, so reclaim would silently never fire) and why the one change from it
(an honest `Err` from a drawn crash or torn write, instead of always `Ok`) is
what makes the trust argument above hold on that device too. A heavy-churn
workload over a narrow key space, asserting the same "recovered state is one
of the committed snapshots" property every other sweep does, plus one more:
`CowBTree::pages_reused()` must be nonzero somewhere across the sweep, or the
run proves nothing about reuse at all. This sweep found one real bug before
it started passing: a candidate page's own free-list row deletion, being
processed by `finalize_free_list`, could be re-offered by a *reentrant*
`refill_free_candidates` call (rewriting the free-list subtree needs a fresh
page too) while that very deletion was still in flight — handing the same id
out twice inside one commit and aliasing it to two logical nodes. Fixed by
tracking every id a transaction has *ever* drawn (`consumed_ever_this_txn`),
separately from the drain queue `finalize_free_list` pops from
(`consumed_this_txn`), so the "already spoken for" check stays accurate for
the whole transaction, not just until the first delete of it starts. It is
recorded here because it is exactly the class of bug this feature is
dangerous for: silent, and only reachable once genuine reuse is exercised.

**What this deliberately does not include, still.** `EngineOptions::page_reuse`
and `inlaysql vacuum` now exist (a later pass — see immediately above), so
"reuse is reachable only through `CowBTree::set_page_reuse` directly" is no
longer true, and it is worth being precise about what that changes and what
it does not.

**A version-gate that lets a build older than this one refuse to open a file
that has actually undergone page reuse is still not implemented.** That
build would trust the page-id-uniqueness invariant its own cache correctness
depends on, and a page-reused file makes that invariant false for it — a
silent-wrong-answer risk if it were ever opened by one, not merely a missing
feature. Two things keep this from being a live risk today rather than a
documented one: no build predating this one has ever been released (README:
"never been run in production by anyone"), and the project's own pre-1.0
policy is *recreate, not migrate* for every format change, not a guarantee
this one specifically lacks. That policy is doing real work here, not standing
in for the fix — the moment a real release makes "an older build might open a
newer file" a real scenario rather than a hypothetical one, this stops being
adequately covered and the version gate described in the AHL-481 report
becomes a blocker, not a follow-up: `Catalog::reuse_freed_pages`, a new
`CATALOG_VERSION_FREE_LIST`, `EngineOptions::reuse_freed_pages`, and the
`Engine::open` wiring between them.

A `VACUUM` statement now exists too (`inlaysql vacuum <path>`,
`crates/inlaysql/src/vacuum.rs`) — landed as a copy into a fresh file and an
atomic rename, deliberately outside the copy-on-write tree's own
crash-recovery surface, so it needed none of the DST proof above and carries
none of the page-reuse version-gate concern either: it never enables
`page_reuse` itself, and the file it produces is byte-for-byte an ordinary
one any build understands.

## Online backup, and the one place it meets page reuse

`CowBTree::backup_to` (`crates/inlaysql-core/src/btree/backup.rs`,
`Database::backup_to`, `inlaysql backup`) copies a committed snapshot out to
another device while writers keep committing. It needs no recovery of its own
and adds none: the copy is written with an empty log and a state block already
naming the root, so opening it replays nothing — which is why it is written
here rather than folded into the protocol above.

It does, however, depend directly on the reader watermark this section
introduced, and reading it as a *second* consumer of that mechanism is the
clearest way to see what the watermark is for. A backup taken through a handle
that registered one holds `min_reader_seq` at its own committed sequence for
the length of the copy (`&self`; `commit`/`refresh`/`checkpoint` all take
`&mut self`), and a page reachable from the root at sequence `S` cannot have
been superseded by any commit up to `S` — so it can only be freed later, and
`refill_free_candidates`'s strict `freed_at < min(commit_point.seq,
min_reader_seq)` declines it. That is a proof, not a race that is usually won:
**a read-write backup is sound with `page_reuse` on.**

And it fails in exactly the place "The cross-process answer, stated plainly"
above says it must. `FileDevice::open_read_only` registers nothing, so a
backup through it cannot be pinned and a writer elsewhere with reuse on could
recycle a page mid-copy — silently, since a data page carries no checksum of
its own. `backup_to` refuses there when the snapshot contains free-list rows,
which exist if and only if some handle has committed with reuse on; an empty
free list is not proof that reuse is off, so that refusal is a net rather than
the missing proof, and the constraint stands: do not take an unpinned backup
of a file a writer has reuse enabled for.

Coverage is `crates/inlaysql-core/tests/backup_dst.rs` — the same seeded
`Simulator`/`FaultSchedule` the sweeps above use, asserting each copy equals
the exact map its workload committed, including one taken from a database that
has just recovered from whatever fault the schedule drew.
