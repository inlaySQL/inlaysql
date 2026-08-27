# R8 — Commit-gate and MVCC scaling

**Status: more research.** The current coordinator is correct and already
amortises some flushes, but the 32-writer target is not met. This note records
the measured bottleneck and the shape a safe next experiment must take. It is
not a claim that InlaySQL has a 100× concurrent-write result.

## Question

Where does the commit path serialize beyond the durability barrier, and what
can remove that serialization at 32–128 disjoint writers without changing
first-committer-wins or the deterministic recovery model?

## Current protocol

The process-local `CommitCoordinator` in
[`crates/inlaysql/src/device.rs`](../../crates/inlaysql/src/device.rs) gives
all read-write handles for one file a shared reservation gate and a shared
flush coordinator:

1. `CowBTree::commit` enters the reservation gate.
2. While holding it, the writer rebases against the latest commit, reserves
   its sequence/page range, encodes the self-contained WAL record, writes its
   dirty data pages and appends the record.
3. `end_commit` publishes the new generation and releases the reservation.
4. The writer then enters `FileDevice::sync`. It takes a durability ticket
   only after its `pwrite` calls have returned.
5. One ticket holder becomes the flush leader. It loads the highest ticket
   visible before its `fsync`, and followers whose tickets are covered wait
   for that flush and return without another barrier.

The ordering is the important part. A leader may not acknowledge a ticket
that was issued after the leader's `fsync` began, and a follower may not be
counted merely because its transaction had entered the reservation gate. A
ticket is evidence of completed writes, not intent to write.

## Local evidence

The clean three-repeat run at commit `188e33c` used
`REPEATS=3 SUITE=concurrency WRITERS=32 TXNS=100 ./bench/repeat.sh`:

[`bench/results/20260827T031859Z-repeat.txt`](../../bench/results/20260827T031859Z-repeat.txt)

| Writers | InlaySQL commits/s | SQLite commits/s | Conflicts |
| ---: | ---: | ---: | ---: |
| 1 | 245 | 88 | 0% |
| 2 | 252 | 87 | 0% |
| 4 | 416 | 87 | 0% |
| 8 | 630 | 87 | 0% |
| 16 | 579 | 89 | 0% |
| 32 | 508 | 90 | 0% |

The 32-writer run reaches 2.08× its one-writer result, not the plan's 100×
target. The 16-writer row has the widest reported spread (565–696 commits/s,
22.6%); the 32-writer row spans 504–558 commits/s. The zero conflict rate
confirms that the workload is exposing coordination and durability scheduling,
not retry waste.

The first bounded ticket-yield experiment gave the flush leader up to eight
`thread::yield_now` calls before capturing its target. One controlled run
measured 490 commits/s at 32 writers, below the clean median above. It is
rejected: an unconditional delay is not a commit pipeline.

The explicit-ready prototype then separated normal commits from checkpoints:
the successful commit publishes its ticket before releasing the reservation,
and only a leader with another normal committer active or queued takes up to
eight scheduler turns. In the same three-repeat shape, the median moved to
522 commits/s at 32 writers while the one-writer median stayed at 245. The
spread was still 17.0% on the 32-writer row, so this is a measured prototype
win, not a publishable headline. A one-run 128-writer probe reached 536
commits/s with zero conflicts; it is directional only. The repeat output is
[`bench/results/20260827T041927Z-repeat.txt`](../../bench/results/20260827T041927Z-repeat.txt)
and the 128-writer output is
[`bench/results/20260827T043329Z.txt`](../../bench/results/20260827T043329Z.txt).
Both prototype outputs record base commit `4567457` with `dirty: yes`, because
the seam was being measured before this commit; they are evidence for the
decision, not published benchmark rows.

With `INLAYSQL_COMMIT_STATS=1`, the 32-writer run reported 3,252 normal
tickets covered by 1,350 normal flushes (about 2.4 tickets per flush). The
diagnostic is opt-in and printed when the shared coordinator is dropped; it
exists to show that the new path is actually grouping tickets, not merely
changing the timer's scheduling.

The current benchmark timer also includes each worker's `Database::open` and
file-lock handoff. That is part of the existing harness boundary, but a
steady-state follow-up should report it separately before a small difference
is attributed to the coordinator.

## What the external protocols imply

PostgreSQL documents two separate synchronous techniques. With
`commit_delay = 0`, a flush can still serve sessions that reach the flush
point while a previous flush is in progress; an explicit delay widens the
joining window but can hurt total throughput when it is too large. Its
`commit_siblings` guard is the relevant idea for us: only delay when there is
evidence that another transaction can join. See the [PostgreSQL WAL
configuration](https://www.postgresql.org/docs/current/wal-configuration.html).

PostgreSQL's asynchronous commit is a different contract: it reports success
before the WAL is durable and limits the risk to recent transaction loss, not
corruption. It must not be smuggled into the default synchronous path; a
future named durability tier would need its own API and DST assertion. See
[PostgreSQL asynchronous commit](https://www.postgresql.org/docs/current/wal-async-commit.html).

Silo's optimistic protocol is the closest concurrency-control analogue: work
is collected in thread-local read/write sets and validation/commit ordering is
made explicit rather than hidden behind a global transaction lock. It is a
useful model for separating local preparation from the short publication
step, but its in-memory epoch assumptions do not provide a durability proof
for this file format. See [Speedy Transactions in Multicore In-Memory
Databases](https://wzheng.github.io/silo.pdf).

MySQL's strict `innodb_flush_log_at_trx_commit=1` setting also flushes logs at
each transaction commit; its relaxed settings trade that guarantee for a
bounded loss window. That makes it a useful comparison for the durability
tiers question, not evidence that a synchronous commit can skip its barrier.
See [MySQL's InnoDB startup options and system
variables](https://dev.mysql.com/doc/refman/8.0/en/innodb-parameters.html).

## Safe next design

The prototype adds the explicit *normal-commit-ready* signal. The remaining
implementation work is to harden and tune it without turning a small, noisy
win into an assumed guarantee:

- Keep the reservation gate responsible for sequence/page allocation, WAL
  placement and `prev_seq`/`prev_root` ordering.
- Distinguish a normal commit's post-gate durability handoff from a
  checkpoint's in-gate sync. A flush leader must never wait on the reservation
  while a checkpoint is holding it and waiting for a flush.
- Publish a durability ticket only after the normal commit's record and data
  pages have been issued. The leader may wait for a bounded cohort of tickets
  that are demonstrably ready, then load the final target immediately before
  `fsync`.
- Keep `durable_upto` monotonic and advance it only after a successful flush.
  A failed leader must wake followers, which then obtain their own chance to
  flush; a leader must never turn a later ticket into an earlier flush's
  promise.
- Close the cohort when there are no ready arrivals, or at a small hard cap.
  A future time-based tail fallback must be measured and named; it must not
  become an implicit durability relaxation.

The separate `commit_sync` seam now exists in the `Device` trait, and its
default implementation remains a direct `sync`, keeping `inlaysql-core`
deterministic and preserving every non-native backend.

## Evidence required before landing code

1. Fake-coordinator tests that pin: a ready follower joins; a ticket created
   after target capture does not join; a failed leader wakes followers; and a
   checkpoint-held reservation cannot deadlock the leader.
2. A real-file test that reopens after repeated WAL-region wraps and verifies
   every committed row, including when a follower returns through the grouped
   path.
3. A benchmark diagnostic that reports flush count and the distribution of
   tickets per flush, alongside commits/s and conflicts. A throughput increase
   without a corresponding durability explanation is not evidence.
4. A clean 32- and 128-writer repeat with setup cost reported separately. The
   result must be compared with the current script-generated baseline before
   any number is moved into `BENCHMARK.md`.
5. The storage/recovery gate: the normal workspace tests plus the release DST
   sweeps required by `AGENTS.md` for a change that touches WAL or commit
   ordering.

Items 2 and 5 are green for the prototype: the focused concurrent-writer and
transaction tests pass, and both release DST sweeps pass. Items 3 and 4 still
need a steady-state benchmark boundary and a quieter 32/128-writer sitting
before this becomes a closed W3 result.
