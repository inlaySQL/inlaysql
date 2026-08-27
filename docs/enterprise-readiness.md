# Enterprise readiness

What stands between this engine and an organisation running it as a primary
SQL database with vector search. It is a gap list, ordered by what would
actually stop a production deployment, and it exists because the honest answer
to "can we run this?" is more useful than a feature table.

The same rule as `BENCHMARK.md` applies: a claim nobody can check is worse than
no claim. Every entry below cites the code it is about, and every entry carries
its **verification status**, because they were not all established the same way:

| Status | Means |
| --- | --- |
| **verified** | Read in the code, reproduced, or pinned by a test in this repo. |
| **reported** | Found by an audit pass and cited to a line, but not independently reproduced. Treat the shape as reliable and the details as needing a second look before anyone acts on them. |

Nothing here is a promise about when it changes. Some of it is deliberate — see
`README.md`'s "What this is not" — and stays.

## Where it stands

"Enterprise grade" is not a thing anyone can declare about their own database,
so this is the nearest checkable substitute: the blockers below, and whether
each is closed with evidence. It is a scoreboard, not a claim.

| # | Blocker | State |
| --- | --- | --- |
| 1 | Foreign commit forced a full retrieval-index rebuild | **closed** |
| 2 | No backup, restore or PITR | **backup closed**, PITR open (see 3) |
| 3 | Change log cannot become replication or PITR | open |
| 4 | Unbounded file growth in server mode | **closed** |
| 5 | ~1 MiB transaction and statement ceiling | measured; **open by choice** — see the entry |
| 6 | Fully resident retrieval indexes, per connection | measured; **both halves now have a lever**, and both are trades — see the entry |
| 7 | Integer comparison through `f64` above 2^53 | **closed** |
| 8 | No statement timeout; unbounded materialisation | **closed** — with two loops deliberately not interruptible, named in the entry |
| 9 | No TLS, one user, no grants | **accounts and privileges closed**, TLS open |
| 10 | Effectively no observability | **closed** — `EXPLAIN`, `SHOW PROCESSLIST`, `SHOW STATUS` and an opt-in slow-query log; no histograms and no audit log, named in the entry |

Closed means: reproduced first, fixed, and pinned by a test that fails against
the old code — not "a commit mentions it". Blocker 5 is the one deliberately
left open: the fix that would close it trades atomicity for capacity, and a
`DELETE FROM t` that half-applies after a crash is worse than one that refuses.

None of this says the engine is ready. It says which of the known reasons it
was not are gone, and it is the only version of that question this repository
can answer about itself.

---

## Blockers

### 1. A foreign commit forced a full retrieval-index rebuild — *fixed, verified*

**Confirmed and fixed.** It was real, and measured: a foreign single-row insert
into a table with *no retrieval index at all* cost another handle 40 re-indexed
documents — the whole table.

`catch_up_indexes` now replays the change log for the versions it missed and
reconciles only the rows the log names, declining to the old full rebuild
whenever it cannot prove that is safe (the catalog also moved, the log no
longer reaches back far enough, a record is missing, or a vector backend is
self-persisting).

Establishing that fix's precondition surfaced a worse bug, now also fixed: when
a transaction is rebased onto a concurrent commit at `COMMIT`, the handle ended
up holding the winner's root without `Storage::refresh` ever reporting a move —
so the winner's rows were committed underneath this handle's full-text index
and `bm25_score` **silently returned nothing for a committed, visible row**.
That is the stale-index-returns-wrong-answers failure mode, and it was
reachable without any of the above.

Both pinned by `crates/inlaysql-core/tests/foreign_commit_indexes.rs` and
`concurrent_writers.rs`, asserted as call counts rather than timings.

<details>
<summary>What the gap was, as originally reported</summary>

`Engine::refresh_snapshot` runs before every statement, and when another handle
has bumped the write version, `adopt_committed_state` clears the text and
vector indexes and calls `restore_indexes` (`crates/inlaysql-core/src/engine.rs`).
`restore_indexes` uses the persisted index blob only when its stamp matches the
current write version, and otherwise re-scans every row of every table. The
blob is saved every `INDEX_PERSIST_INTERVAL` (1024) mutations, so the
mismatched case is the ordinary one.

A mixed read/write server with a BM25 or vector index paid a full re-index on
every connection after every other connection's commit, which is the difference
between "concurrent" and "unusable". `docs/indexes.md` said skipping a save
"costs a rebuild on the next open and nothing else", and separately that
incremental catch-up "would need a change log the engine does not keep" — both
were wrong and both are corrected.

</details>

### 2. No backup or restore — *fixed, verified* (point-in-time recovery is item 3, and still open)

**The backup half is fixed.** `Database::backup_to(path)`, `inlaysql::backup`
and `inlaysql backup <database> <destination>` produce a consistent copy of a
live database without stopping or locking out the writer.

It is a physical page copy, not a dump, and that is the whole reason it is
small enough to trust: a committed root in the copy-on-write tree *is* an
immutable, consistent snapshot — the property `docs/architecture.md` (D4) and
`docs/recovery.md` describe and MVCC readers already rest on — so a backup pins
one and copies the pages it reaches. The copy is therefore never a mix of two
commits however many land while it runs, and never the subtler mix a
statement-at-a-time dump has to work to avoid, where two tables come from two
different snapshots because `refresh_snapshot` ran between them.
`crates/inlaysql-core/src/btree/backup.rs` carries the argument in full.

**What was actually hard about it** is the interaction with page reuse, and it
is worth recording because getting it wrong produces a silently corrupt backup,
which is worse than no backup: a reclaimed page is overwritten in place and a
page carries no checksum of its own, so a copy that walked one mid-recycle
would decode cleanly and be wrong. The answer is the reader watermark the free
list already maintains for exactly this question. A backup taken through a
read-write handle holds `Device::min_reader_seq` at its own committed sequence
for the whole copy — `&self` is what stops the root moving, since `commit`,
`refresh` and `checkpoint` all take `&mut self` — and a page reachable from
that root can only have been freed at a later sequence, which
`refill_free_candidates` declines. **So a read-write backup is sound even with
`page_reuse` on.** The one handle that cannot pin is
`Database::open_read_only`, which takes no lock by design and is invisible to
that proof in this process or any other; a backup through it *refuses* when the
source records reclaimable pages (free-list rows exist if and only if some
handle committed with reuse on), and is documented as unsound beside a writer
that has reuse enabled but has not freed anything yet.

Verified by `crates/inlaysql-core/tests/backup_dst.rs` (seeded fault schedules;
each copy must equal the exact map its workload committed, including after
crash recovery) and `crates/inlaysql/tests/backup.rs` — a bank-transfer
workload whose committed states are enumerable in closed form, backed up while
another handle commits on another thread, while another *process* holds the
write lock, and while a writer with page reuse on is demonstrably recycling
pages.

**Restore is deliberately not a command.** The file this produces is an
ordinary database: open it, or move it back. A `restore` subcommand whose body
is `fs::rename` would imply it knows something about the file that it does not.

<details>
<summary>What the gap was, as originally reported</summary>

There is no backup, dump or snapshot API anywhere in the engine or the CLI.
`README.md` says the quiet part out loud: "Keep a backup you can restore from
something else."

`inlaysql vacuum` is compaction, not backup, and it opens read-write and holds
the exclusive advisory lock for the whole copy-and-rename — so it cannot run
against a live server, which holds a keeper handle for its lifetime.
`mysqldump` does not work either: `LOCK TABLES`, `FLUSH TABLES` and
`SHOW MASTER STATUS` are not in the wire shim's intercept set and reach the
core, which refuses them.

</details>

**Point-in-time recovery is still absent**, and nothing above moves it: a full
copy is a full copy. Restoring to an arbitrary instant needs a log with row
payloads in it, which is blocker 3 — and `mysqldump` still does not work, for
the reasons above.

### 3. The change log cannot become replication or PITR as built — *reported*

`crates/inlaysql-core/src/cdc.rs` records `(version, table, row id, operation)`
and deliberately carries **no row data**. Retention is 4096 statements; a
consumer that falls behind is told `lost`. DDL is invisible to it.

That is a sound design for what it is — cache and search-index invalidation,
"re-read these row ids" — and it is not a replication log: no payloads, no
schema events, no consumer-managed retention. The only consumer surface opens
the database read-write, so it is locked out while the server runs.

### 4. Unbounded file growth in server mode — *fixed, verified*

**Confirmed and fixed**, and the naive fix would have made it worse. There is
now a `ServerOptions::page_reuse` and a `--page-reuse` flag, default off,
threaded to every connection.

Turning the flag on alone was not enough: reclamation only offers pages freed
before the reader watermark, and every read-write handle pins that watermark at
the sequence it last read. The server's keeper handle read once at startup and
never again, pinning it for the process lifetime — so nothing freed afterwards
was reclaimable while free-list rows accumulated as overhead. Measured at the
SQL level: **64 MB** with reuse off, **4.5 MB** with reuse on, **109 MB** with
reuse on and a `Database` keeper. The keeper is now a bare `FileDevice`, which
holds the same lock but opens no tree and registers no reader.

It stays off by default and is not presented as free: page reuse is unsound
with cross-process read-only handles (`docs/recovery.md`), so the flag's
documentation and a startup warning name the concrete thing it forbids —
`inlaysql serve --mcp` against the same file.

### 5. A hard transaction and statement ceiling near 1 MiB — *verified, still open*

**Confirmed, and it is not what this entry used to say it was.** One commit
record must fit one WAL region — `WAL_BLOCKS` (256) × `DEFAULT_PAGE_SIZE`
(4096) = 1 MiB (`crates/inlaysql-core/src/wal.rs`) — and the record carries a
copy of every page the commit wrote, so the quantity that has to fit is the
transaction's copy-on-write dirty set in bytes. An explicit transaction is
refused once it passes half of that
(`Storage::transaction_is_nearly_full`, checked before each statement); a
single autocommit statement that exceeds it fails at commit
(`btree/tree.rs`). So a bulk `INSERT ... SELECT`, a wide `UPDATE` or
`DELETE FROM t` is a hard error rather than a slow path, and the same bound
caps a single value at about 1 MiB.

The old entry cited "roughly 5,000 rows" from
`crates/inlaysql/tests/large_index.rs`. That number is real but it is about the
*index save* path, not about DML, and quoting it here made the ceiling look
like a row count. It is not one.
`crates/inlaysql/tests/large_statements.rs` now pins where each statement
actually breaks, on a two-column table with nothing else running:

| statement | 8-byte bodies | 512-byte bodies |
| --- | --- | --- |
| `UPDATE t SET body = 'x'` | 17,000 ok / 17,500 refused | 1,687 ok / 1,750 refused |
| `INSERT INTO t (body) SELECT body FROM t` | 16,500 ok / 17,000 refused | 1,687 ok / 1,750 refused |
| `DELETE FROM t` | 68,750 ok / 70,625 refused | 3,000 ok |
| buffered `INSERT`s inside `BEGIN`..`COMMIT` | refused at 11,340 | refused at 884 |

Regenerated by `the_row_counts_where_each_statement_breaks`, per the same rule
`BENCHMARK.md` applies to numbers: nothing here is quoted that the repo cannot
reproduce on demand.

**What the refusal does *not* do is apply half the statement.** Every row of
that table was checked both ways: the statement is refused, and the table is
exactly what it was. A `DELETE FROM t` that removed some rows and reported an
error would be a data-loss bug and a much worse entry than this one; it is not
what happens.

Two things this verification pass found that change what fixing it costs.

**`DELETE FROM t` is bounded by a change-log record the caller never asked
for.** Deleting rows is nearly free in pages — `CowBTree::supersede` drops a
page from the dirty set when the transaction supersedes it again, so a
collapsing tree leaves the record as fast as it is walked — but
`crates/inlaysql-core/src/cdc.rs` writes one record per *statement* holding
`(table name, row id, kind)` per *row*, repeating the table name in every
entry. That is why `DELETE`'s threshold barely moves with row width, and it is
provable without reading any code: rename the table to something 62 characters
long and the same delete over the same 20,000 identical rows goes from
committing to refused. Whatever happens to the WAL record, a whole-table
`DELETE` cannot exceed a few tens of thousands of rows until the change log has
a summary form — and `cdc.rs`'s own argument says what that form must be, since
silently truncating the list would let a consumer believe it was caught up when
it was not.

**A `COMMIT` refused for size used to strand the write set** — found here,
fixed here. `Engine::commit` marked the transaction over and returned
the error while the storage backend still held every buffered page, so
`rollback` refused ("rollback with no transaction open") and the next
autocommit statement was left to make the abandoned writes durable along with
its own. That is exactly the silent-durability failure
`Engine::discard_failed_statement` exists to prevent; the explicit-`COMMIT`
path never reached it, because `Plan::is_read_only` answers `true` for
`Plan::Commit` — the right answer for the read-only connection guard that
question is really asked for, and the wrong proxy for "left nothing to clean
up". Here the consequence was self-limiting, because this particular error is
permanent and the *next* failure's discard eventually cleared it. A transient
failure in the same place would not have been.
`a_commit_refused_for_size_leaves_a_usable_handle` pins it.

#### What lifting the ceiling requires, and why it is not lifted here

The region size is load-bearing for recovery, not an arbitrary constant — see
`docs/recovery.md`, which now carries the design in full. In short: the record
copies the pages because a record that cannot rebuild its own pages is not a
commit under the torn-write model, and the data area begins at
`(region_count × WAL_BLOCKS + 1) × page_size`, so the region's size is baked
into the address of every page in the file.

Three ways out were evaluated. **Spilling one commit across the other three
regions** buys 4× for the cost of the per-writer region ownership that makes
concurrent writers cheap; it moves where `DELETE FROM t` breaks rather than
fixing it. **Committing a large statement in several durable batches** would
make `DELETE FROM t` "work" by making it non-atomic — a crash halfway leaves it
half-applied — which is the one outcome worse than the refusal, and it is
refused on the same grounds as architecture rule 5. (The engine already does
batch internally in `purge_index_entries` and the index save, where what is
being rebuilt is derived state the engine owns and can reconstruct; that
argument does not extend to a user's `DELETE`.) **Spilling the pending write
set to disk** does not apply at all: `pending_record_len` is computed from the
same dirty map that is the memory bound, so the WAL record and the resident
buffer are the same quantity, and the WAL record is the one that refuses first.

That leaves the real fix, which is to stop the record's size tracking the
commit's size — either by spilling the record's page payload into fresh data
pages that a small header names and checksums, or by giving every data page a
checksum so the record can name pages instead of copying them (which would also
retire the "a data page carries no checksum of its own" caveat that blocker 2's
backup argument and the page-reuse proof both work around). Either is a format
version 6 change to the on-disk record layout and the recovery protocol, and
per architecture constraint 3 it needs its own deterministic-simulation pass —
including a sweep that *proves it exercised the new path*, the lesson
`free_list_reuse_dst.rs` records. Landing it half-proven would put the one
property the engine's recovery story rests on at risk to remove a limit that
today refuses honestly. It is written down rather than shipped.

### 6. Fully resident retrieval indexes, per connection — *verified and measured; partly fixed*

**Confirmed, and worse than this entry used to claim.** It said "roughly 15 GB
of `f32` per connection" for 10M vectors at 384 dimensions, which is
`10M × 384 × 4`. That counts one copy of the payload, and one copy of the
payload is not what the process holds.

`crates/inlaysql/tests/index_memory_cost.rs` measures it instead of estimating
it: a counting global allocator, live heap after the builder's scratch is
freed, at four corpus sizes so the per-unit slope can be told apart from the
fixed cost. It is `#[ignore]`d, like every instrument here — run it with
`cargo test --release -p inlaysql --test index_memory_cost -- --nocapture
--ignored`.

**Per vector, dimension 384** (flat from 2,000 to 32,000 vectors, which is what
makes it a constant rather than a reading):

| encoding | held per vector | payload alone | 10M vectors |
| --- | --- | --- | --- |
| exact `f32` | **2,018 B** (was 3,554) | 1,536 B | **18.8 GiB** (was 33.1) |
| `VECTOR(n, INT8)` | **866 B** (was 1,250) | 388 B | **8.1 GiB** (was 11.6) |

The reduction is `HnswIndex` no longer holding every embedding twice — a source
map *and* each committed node's own prepared copy. Recall and distance-call
counts are bit-identical across the change (0.587 / 0.721 / 0.897 / 0.986 at
`ef` 8 / 32 / 64 / 128, 1,318 calls per query), so the graph is unchanged;
query mean moved 57.16 µs to 59.89 µs, which is inside this machine's noise and
is recorded rather than claimed as free.

The factor of 2.3 is not overhead in the usual sense. `HnswIndex` holds each
embedding **twice** — `embeddings` is the source of truth and every committed
`Node` carries its own normalised copy — and the graph's per-layer adjacency
(`Vec<Vec<usize>>`) is on top of that. So the honest number for 10M exact
vectors is 33 GiB per connection, not 15.

**Per document, BM25.** The old entry did not put a number on this half at all.
For 120-token chunks drawn Zipf-ian from a 200,000-word vocabulary — stated
because the cost is dominated by *distinct* terms per document, and by how fast
the dictionary saturates:

| documents | held per document | distinct terms |
| --- | --- | --- |
| 2,000 | 4,474 B | 54,875 |
| 8,000 | 3,126 B | 121,212 |
| 32,000 | 2,270 B | 186,781 |
| 128,000 | 1,859 B | 199,952 |

It falls as the dictionary saturates and then flattens near 1,800 B/document,
so **10M documents is roughly 17 GiB** — resident, per connection.

**The same corpora through `PagedBm25Index`**, which now exists (see below).
The figure to read is not the slope, it is that there is no slope:

| documents | held | of that, corpus | file on disk |
| --- | --- | --- | --- |
| 2,000 | 15.9 MiB | none | 1,260 MiB |
| 8,000 | 15.9 MiB | none | 3,255 MiB |

Identical at both sizes, because what is held is a bounded entry cache plus
this handle's 8 MiB page cache and nothing per document at all. The two tables
are not quite like for like and the difference favours the paged one — a
`Bm25Index` carries no storage handle, so ~8 MiB of the 15.9 is a page cache
the other never had. The file column is the price, and it is discussed under
"what it costs" below.

**Per connection, end to end.** 8,000 rows at dimension 384 with both indexes,
opened the way `serve_connection` opens one:

| paged | first handle | each additional handle | of that, ANN payload |
| --- | --- | --- | --- |
| neither (the default) | 64.8 MiB | 56.5 MiB | 23.4 MiB |
| vectors | 43.2 MiB | 35.0 MiB | 3.7 MiB |
| text | 43.6 MiB | 35.5 MiB | 23.4 MiB |
| **both** | **20.9 MiB** | **12.8 MiB** | 3.7 MiB |

Each lever is worth about 21 MiB per connection here and they compose: a second
connection costs 56.5 MiB by default and 12.8 MiB with both on, a factor of 4.4.
The number that matters is not the factor, though — it is that what remains does
not grow with the corpus. 12.8 MiB is two page caches, a catalog and an engine.

**What is fixed — the vector half.** The server can now be told to use the
paged vector index: `ServerOptions::paged_vector_indexes` / `inlaysql serve
--mysql --paged-vectors`, off by default and documented as a trade in
`docs/server.md`.

Turning it on was not a matter of setting the flag. `catch_up_indexes` declined
outright whenever any vector backend kept itself in the database, and declining
means the whole table is rebuilt from every row — so `--paged-vectors` would
have reintroduced blocker 1 for the *full-text* index, on every connection, on
every other connection's commit. Measured, in
`crates/inlaysql-core/tests/foreign_commit_indexes.rs`: one foreign insert into
a 40-row table cost **41 re-indexed documents**, and the "reader" wrote **843
rows** into the shared graph while doing it — which on a `Database::open_read_only`
handle is not slow, it is an error.

A self-persisting index is not caught up by replaying rows into it: the writer
already applied them, in the file, so a replay applies them twice and does it
as writes from a handle that only read. What makes it current is re-opening it.
`Engine::adopt_self_persisting_vector_indexes` does that, holds the re-opened
graph to the same stamp test a saved blob gets, and leaves the row-level replay
to the indexes that actually need it. Both halves are pinned by tests that fail
against the old code, and the answers — scores, not merely ranking — are
compared against a handle opened fresh from the file.

**What is fixed — the BM25 half.** `PagedBm25Index`
(`EngineOptions::paged_text_indexes`, default off) puts the term dictionary,
the postings and the per-document term lists in the file and reads them through
a bounded cache, the way `PagedHnswIndex` already did for the graph. The layout
and the argument are in `docs/indexes.md`; three things about it belong here —
what it gets right, what it costs, and what building it found in the backend
that was already shipped.

**The scores are identical, bit for bit, and that is the whole difficulty.**
BM25's `idf` and its length normalisation are corpus-relative, so a backend
whose statistics differ in the last place does not fail — it returns a
plausible ranking with two hits transposed, or the same ranking with different
numbers, and the number is what `fuse()` and a user's `ORDER BY
bm25_score(...)` consume. The four arithmetic steps are therefore *called* by
both backends rather than transcribed twice (floating-point is not
associative), and the corpus statistics move on exactly the events they move on
in the in-memory index. Asserted rather than argued:
`crates/inlaysql-core/tests/bm25_paged_agreement.rs` compares whole result sets
— ids and score **bits** — against a freshly built `Bm25Index` over six corpus
shapes, every query shape, six limits and two filters, and
`crates/inlaysql/tests/paged_full_text.rs` does the same through the whole SQL
path on the `f64` that comes back from `bm25_score`. A crash sweep stops the
build after every *n*th storage write and requires that a stamped index is
always a complete one, and that whatever survived rebuilds to the right answer.

**What it costs is writes, and the bill is large.** An inverted index update
touches one chunk per *distinct* term of the document — around a hundred for a
120-token chunk of English — and the first time each term is seen it costs a
dictionary bucket and a term record as well. Those land on different leaf pages
because the terms are scattered across the key space, so under copy-on-write one
document dirties a few hundred pages. Consequences, in order of how much they
hurt:

* **The file grows by hundreds of kilobytes per document on a bulk load** —
  measured at 1,260 MiB for 2,000 documents and 3,255 MiB for 8,000 — because
  every superseded page is abandoned rather than reclaimed with `page_reuse`
  off, which is the default (blocker 4). This is the number that decides
  whether the trade is worth taking, and it is not small.
* **With `page_reuse` on, the build was refused for size** — a finding in its
  own right, *not* specific to this index, and now fixed.
  `Storage::transaction_is_nearly_full` answered from the dirty set as it
  stood; committing with reuse on then writes free-list rows of its own, so a
  batch that was under the ceiling when it was last asked was over it by the
  time the record was built. Measured: refused at 1,076,352 bytes against a
  1,048,576-byte region, having last been asked at 524,288. **Any batched
  writer that trusted that method was exposed to it** — the index-save path in
  `persist_indexes` asks the same question for the same reason — and the worst
  case was far past a factor of two: deleting rows whose values live in
  overflow chains supersedes a chain of pages per row while barely moving the
  dirty set, which was measured at 2 dirty pages when last asked and 187,903 by
  the time the record was built. The backend now answers with
  `CowBTree::projected_record_len`, which adds one record entry per free-list
  row the commit still owes, so the answer covers the work committing will do
  rather than only the work already done. Over-reserving costs one extra
  commit; under-reserving stranded the transaction.
  `the_size_question_covers_the_free_list_rows_committing_will_add`
  (`crates/inlaysql/tests/free_list_growth.rs`) fails against the code without
  it.
* **A read inside an open transaction, after many documents, can be refused.**
  Index commits are deferred to the first read that needs them, and that read
  is normally outside any transaction, where the backend commits itself in
  batches — so the ordinary path is fine. Inside one it may not commit, so the
  whole batch has to fit one record.

So this closes the *memory* half of the entry and opens a file-size and
write-amplification question in its place. The real answer to that is the
segment-and-merge design every production full-text engine uses — postings
written once as immutable runs and merged in the background, instead of
read-modify-written in place — and it is a project of its own.

**What building it found in `PagedHnswIndex` — since fixed.** Two `Database`
handles on one database hold two objects over the *same* structure in the file.
When one of them rebuilds — which is what any handle does on opening to a stamp
that is not current — it rewrites that structure and reassigns its internal
indices underneath the other, **without changing a row**, so nothing moves the
`write_version` `adopt_committed_state` watches and the other handle never
notices. For BM25 that showed up as an arithmetic underflow on the live
document count and, worse and silently, as two handles handing the same term
ordinal to two different words;
`PagedBm25Index::adopt_stored_statistics` closes it by re-reading the header on
every commit and every search instead of remembering it.

`PagedHnswIndex` had the identical exposure with node indices, and it was
reachable rather than theoretical: a handle left behind by another handle's
rebuild answered a query with a completely different set of rows — no error,
no count to underflow, just the walk starting at whatever row now occupies the
remembered entry point. `PagedHnswIndex::adopt_stored_graph` closes it the same
way, re-reading the header on every commit and every search and dropping the
node cache when it moved; the resident `RowId -> node` map is rebuilt lazily on
the next maintenance call, because it is the one `O(nodes)` step and a search
never consults it. Both regression tests —
`a_rebuild_by_another_handle_is_adopted_rather_than_overwritten`, one per
module — fail against the code without their fix, and the vector one asserts on
returned ids against an oracle rather than on internal state, because ids are
where the damage showed.

**What is not fixed, and what the measurement says about it.**

* Re-opening the graph is O(nodes) per foreign commit. Bounded by the graph
  rather than by a rebuild of every index on the table, but not free. The BM25
  side does not have this problem: re-opening a paged BM25 index reads its
  header and nothing else, because it keeps no resident row-id map, so
  adopting another handle's commit is O(1).
* The paged BM25 index is now wired to a server flag the same way
  `--paged-vectors` is: `ServerOptions::paged_text_indexes` /
  `inlaysql serve --mysql --paged-text`, off by default and documented as a
  trade in `docs/server.md`.
* Each connection still carries its own 8 MiB decoded page cache
  (`DEFAULT_PAGE_CACHE_BYTES`), on top of the 8 MiB raw-page cache shared per
  file. With both indexes paged this is most of what a connection holds.

**Why "share one immutable index between connections" is not the answer**, even
though it looks like the obvious one. Four things stand in the way, and only
the first is a plumbing problem:

1. Nothing in the core is `Send`. `SharedStorage` is an `Rc<RefCell<_>>` by an
   explicit decision (`crates/inlaysql-core/src/shared.rs`), and no index or
   storage trait carries a `Send`/`Sync` bound. Adding one pushes it through
   every trait in the core, which is exactly what the simulation harness (it
   shares a fault-injecting disk as an `Rc`) and the single-threaded WASM build
   cannot take.
2. **An index holds uncommitted rows.** `Engine::index_row_for_index` runs as
   part of the row write, inside the open transaction, so a shared index would
   show one connection's uncommitted `INSERT` to every other connection's
   `bm25_score` — a dirty read through the retrieval path.
3. **A `ROLLBACK` rebuilds it.** `Engine::rollback` calls `reload`, which clears
   every retrieval index and re-derives it from the committed rows. On a shared
   index that is one connection discarding everyone's.
4. **BM25 scores are corpus-relative**, so sharing is not enough — it would have
   to be *versioned*. `idf` is a function of the live document count and the
   normalisation of the mean document length, so a reader on an older snapshot
   would score its rows against a newer corpus's statistics. Sharing the index
   without MVCC over it changes answers, not just visibility.

The mechanism this codebase already has for sharing between connections is not
an `Arc` — it is the file. `FileDevice` keeps one raw-page read cache per file
(`crates/inlaysql/src/device.rs`), shared by every handle in the process and
sound with no invalidation protocol at all because the tree is copy-on-write
and a data-area page id names immutable bytes. That is why the paged ANN index
gets cross-connection sharing for free: its graph *is* pages. It is also the
argument for solving the BM25 half the same way rather than by reaching for
threads.

**Where `PLAN.md`'s 10M-vector goal actually stands**, which the old entry got
directionally right and quantitatively wrong:

* 10M vectors, **no text index**: 33 GiB per connection by default — not
  reachable. With `--paged-vectors`, the resident cost is the node cache plus
  the page caches, on the order of tens of MiB per connection whatever the
  corpus size. **Reachable.**
* 10M vectors **and** 10M documents — the hybrid case that is the whole claim.
  By default this is ~33 GiB plus ~17 GiB per connection and is not reachable.
  With both indexes paged, **memory is no longer what stops it**: the resident
  cost is two bounded caches and does not move with the corpus, measured flat
  at both corpus sizes it was measured at.

  That is a real change and it should not be overstated. What now stands in the
  way is the file and the write path, not the heap: this backend's bulk load
  grew the file by hundreds of kilobytes per document, so 10M documents is a
  question about terabytes of write amplification rather than about gigabytes
  of RAM. The claim "vector + BM25 + SQL in one file at scale" has a memory
  answer for the first time; it does not yet have an ingest answer, and the
  honest state is that the ceiling moved rather than went away.

### 7. Integer comparison through `f64` above 2^53 — *fixed, verified*

`compare_cells` (the `WHERE` path) and `unique_key_collides` (the `UNIQUE`
check) widened two `INTEGER`s to `f64` before comparing. An `f64` cannot
represent consecutive integers above 2^53, so `WHERE id > 9007199254740992`
silently dropped the row holding `…993`, and inserting two adjacent external
ids raised a duplicate-key error on data that was not duplicated.

Both now compare `i64` to `i64`. Pinned by
`crates/inlaysql/tests/large_integers.rs`, and checked against SQLite at 50,000
differential rounds. Listed here because it is the shape of bug this document
is for: silent, plausible, and aimed squarely at Snowflake ids and epoch
nanoseconds.

### 8. No statement timeout or cancellation; unbounded materialisation — *fixed, verified*

**Fixed — a statement can now be stopped.** The missing piece was a
cancellation signal the executor honours, and it could not live in the engine:
`inlaysql-core` is `no_std`, so it can neither read a clock nor own a thread
that would interrupt one. So it is a trait beside the `Clock` one it resembles
— `traits::Cancel`, answering "why must the statement in flight stop", with
`Stopped::Timeout` and `Stopped::Killed` as the only two answers — and the core
asks it from inside every loop that can run long: the batch loop of every
sequential scan, the per-id loop of an index-range fetch, a join's pairing
loop, the collect a blocking operator fills, the sort/group/window/distinct
passes, and the write loops of `INSERT`, `UPDATE` and `DELETE`.

The question is amortised over a fixed amount of *work* — a thousand rows,
counted in rows rather than in calls so a scan that reads five hundred rows in
one batch spends five hundred — and a handle with no signal installed pays one
null branch and no call at all. The point-read path asks nothing whatever: it
is a single tree descent with no loop in it, which
`a_short_statement_is_never_asked` pins by counting the questions two hundred
point reads produce and requiring zero. Measured as well as argued —
`SUITE=points ./bench/run.sh`, four alternating pairs against the same commit
without this change, throughput within a percent either way and `p50` inside
the timer's own resolution.

**Fixed — a statement timeout, and `KILL`.** `serve --mysql
--max-execution-time <ms>` gives every statement a deadline (`0`, the default,
is MySQL's own and means none), and `SET max_execution_time` lets a session
change its own. `@@max_execution_time` and `SHOW VARIABLES` report it by
reading the field the engine enforces, not a copy — the server learned that
lesson once already, when it advertised `wait_timeout` values it never applied.
`KILL [CONNECTION | QUERY] <id>` and `COM_PROCESS_KILL` end somebody else's,
through a registry of live connections the accept loop owns; own account
always, another account's with the superuser, `1094`/`1095` otherwise. A
`KILL CONNECTION` shuts the target's socket down as well as setting the flag,
so an idle connection goes at once instead of at its `wait_timeout`.

**What a stopped statement leaves behind is nothing, and that is the part that
was tested hardest.** Cancellation is noticed only while a statement is
producing or collecting rows, never while it is making them durable, so it
leaves through the same statement-atomicity path a `CHECK` violation leaves
through. `crates/inlaysql-core/tests/cancellation.rs` does not check one
convenient stopping point: it sweeps every one, tripping the signal on the
first question, then the second, and so on until the statement completes, and
after each stop asserts the table is byte-for-byte what it was and the handle
still answers. `crates/inlaysql-server/tests/wire.rs` does the same over a
socket against a live `UPDATE`, and pins the two error codes, the privilege
rules and the idle-connection kill.

**One thing is deliberately not interruptible**, and it is named rather than
left to be found: a `bm25_score`/`vector_score` walk with no filter to push
into it. `FullTextIndex::search` and `VectorIndex::search` are traits any
backend implements and neither takes a signal, so closing that case means
widening the trait everybody implements. A *filtered* retrieval query is
interruptible, because the filter the engine pushes into the walk runs once per
candidate and carries the check. The other is index rebuild on open or after
another handle's commit, which is refused for a sharper reason: it runs with
the indexes already cleared, so stopping it half-way would leave a handle whose
`bm25_score` silently returns nothing for committed, visible rows — blocker 1's
failure, reintroduced.

**Fixed — the server no longer holds the answer.** A `SELECT` whose columns the
plan can describe before it runs is written to the socket as the engine
produces it, so `SELECT * FROM big_table` costs the server one row and one
write buffer whatever the table's size. Measured rather than asserted:
`crates/inlaysql-server/tests/streaming_memory.rs` counts peak heap through a
global allocator and holds four times the rows to the same number. What decides
it is the shape of the projection, not the size of the table — a computed
column, an aggregate or a `UNION` arm has no type until it has a value, and the
MySQL protocol needs every column's type in packets that precede the first row,
so those statements are still materialised. Both paths are byte-identical on
the wire, which `wire.rs` compares as raw bytes rather than as decoded rows.

**Fixed — a blocking operator now has a ceiling.** `ORDER BY`, `GROUP BY`,
`DISTINCT` and window functions still materialise, because none of them can
answer before reading their last input row, and there is still no spilling to
disk. What has changed is what happens when the input does not fit:
`EngineOptions::query_memory_bytes` (512 MiB by default, `serve --mysql
--query-memory`, `0` to remove it) refuses that one statement with
`ER_OUT_OF_SORTMEMORY` and leaves the handle usable, instead of allocating
until the out-of-memory killer ends the process and every other connection with
it. It is a per-statement ceiling, so a server's real exposure is this number
times `max_connections`.

**Not fixed by either:** the inner side of a nested-loop join over a derived
table, a hash-join build, and a `UNION`'s arms all still materialise without a
ceiling of their own.

**Fixed:** the server no longer reports limits it does not enforce. It used to
advertise `wait_timeout=28800` and `net_*_timeout=60` while never setting a
socket timeout, and `max_connections=0` against a real cap of 64 — a reported
timeout that is not honoured is worse than none, because a client tunes against
it. The reported numbers are now the enforced ones, socket read and write
timeouts really are set (`--wait-timeout`), and a zero timeout is refused at
bind rather than quietly clamped. That also closes the idle-connection hole,
where 64 idle clients could hold all 64 slots forever.

### 9. No TLS, one user, no grants — *accounts closed, TLS open*

**The accounts half is closed (AHL-497).** The MySQL-wire server has a durable
account store in the database file, seven privileges grantable globally or per
table, a superuser, and `CREATE USER` / `ALTER USER` / `DROP USER` / `GRANT` /
`REVOKE` / `SHOW GRANTS` to manage them. `docs/server.md`'s "Accounts and
privileges" is the whole model, including the list of what it leaves out.

Four things about it that a security review will want to check, each pinned by
a test in `crates/inlaysql-server/tests/wire.rs`:

* **A password is never stored.** An account carries the verifier each plugin's
  challenge-response is defined in terms of, and a login is checked by running
  the exchange backwards. `accounts_and_grants_survive_reopening_the_database`
  greps the raw database file for the plaintext and asserts it is not there.
  The verifiers are unsalted — the plugins' own definitions fix that — so a
  stolen file is a stolen password list offline; `docs/server.md` argues why
  the salted alternative would be worse here rather than better.
* **Authorisation reads the plan, not the statement text.** A table named only
  inside a subquery, a join, a `UNION` arm or a derived table is checked like
  any other (`inlaysql::Statement::table_access`;
  `a_table_reached_only_through_a_subquery_is_still_checked`). A statement
  whose requirement cannot be determined is refused, not allowed.
* **A revoke takes effect on the next statement**, including on an
  already-connected session and on a statement prepared while the grant still
  held (`a_revoke_takes_effect_on_an_already_connected_session`). The one
  window left is an explicit transaction, whose snapshot is pinned by design.
* **The store is not reachable through SQL**, superuser included, and is
  filtered out of every metadata answer here and in the MCP server
  (`the_account_store_is_invisible_and_untouchable`).

**What is still open under this heading.**

* **TLS.** Unchanged: the wire is plaintext, `CLIENT_SSL` is never advertised,
  and a client that asks is told rather than downgraded. Accounts make the
  server usable by more than one party; they do not make the link safe to run
  across a network. This is still the first thing a security review stops at.
* **Metadata is not hidden.** Any authenticated account can `SHOW TABLES` and
  `DESCRIBE` anything. Real MySQL shows only what you hold a privilege on.
* **No column-level or row-level privileges, no host-based access control, no
  roles, no account locking, password expiry, login throttling or audit log.**
  Each of the first three is *refused* where it can be written down rather than
  accepted and ignored.
* **These privileges guard the wire server only.** Anything that can open the
  file — the embedded API, the CLI, `serve --mcp` — bypasses all of them,
  because the file is the credential there.

What was already there and remains sound: `mysql_native_password` and
`caching_sha2_password` are both real challenge-response, every secret
comparison is constant-time, the scramble comes from OS entropy and fails
rather than falling back to something guessable, and the RSA public-key
exchange is refused with a clear error rather than faked.

### 10. Effectively no observability — *fixed, verified*

**It was real.** No metrics, no counters, no exporter, no `log` or `tracing`
dependency, no `SHOW PROCESSLIST`, no slow-query log. An operator could not
answer either of the two questions anyone has about a running database — what
is it doing right now, and what has it been doing.

`EXPLAIN` closed the first half earlier (`EXPLAIN`/`EXPLAIN QUERY PLAN`/
`DESCRIBE <statement>`, over the wire as well as in the engine): it reports
which access path the executor chose — scan, row-id point lookup, index range,
hash join, index nested loop, or which retrieval index answered a
`bm25_score`/`vector_score`/`fuse`. It reports no row counts, costs or
selectivity, because there is no statistics system here to draw them from; see
`crates/inlaysql-core/src/explain.rs`.

**What is fixed.** Three things, all over the MySQL protocol and none of them a
new dependency — `crates/inlaysql-server/src/metrics.rs` is `std::sync::atomic`
and nothing else, and the process list reads the `KILL` registry that already
existed rather than a second list beside it.

* **`SHOW [FULL] PROCESSLIST`.** MySQL's eight columns for every live
  connection: id, user, host, db, command, time, state, info. **The privilege
  rule is `KILL`'s, character for character** — your own connections and your
  own account's always, anybody else's only with the superuser, and a
  connection still handshaking belongs to nobody so only a superuser sees it.
  One rule on purpose: an id in the list is always an id the viewer could act
  on. It does not widen the documented metadata gap (`docs/server.md`, "What is
  deliberately left out"), which is about table and column *names* being
  readable by every account; this is filtered by account.
* **`SHOW [SESSION | GLOBAL] STATUS`.** Statements by kind, wire commands,
  bytes in and out, errors bucketed by what an operator would do about them
  (access denied, syntax, unsupported, constraint, write conflict, timeout,
  interrupted, no such object), connections accepted, aborted, refused at the
  cap, the high-water mark, uptime, and the two thread counts. Session and
  global are two different numbers, as they are in MySQL. `Threads_connected`
  and `Threads_running` are not counted at all — they are derived from the same
  registry the process list reads, so the list and the count cannot disagree.
* **A slow-query log**, `--slow-query-log <ms>`, off by default, one stderr
  line per statement over the threshold, counted as `Slow_queries` and reported
  as `slow_query_log`/`long_query_time`.

**The statement-text policy was changed explicitly, not by accident.** This
server logs and holds no statement anywhere, and that is stated from the second
paragraph of `docs/server.md`. `SHOW PROCESSLIST`'s `Info` and a useful
slow-query log both want the statement, so there is now one flag —
`--statement-text`, **off by default** — that turns statement retention on for
both, warns at startup when it is on, and is reported as
`@@inlaysql_statement_text` so it is checkable from a client. With it off no
statement text is stored in the process at all and `Info` is `NULL`. Nothing
changed about passwords or verifiers: those are never logged under any flag.

**Every number is maintained, or it is not reported.** Some session variables
used to be fiction — `wait_timeout` and the `net_*_timeout`s were reported and
never enforced, `max_connections` reported `0` against a real cap of 64 — and
that is closed under blocker 8: every number the server reports is read from
the thing that applies it. The counters follow the same rule, and so does the
naming: a counter whose meaning is MySQL's carries MySQL's name, and one this
server invented is prefixed `Inlaysql_` so nobody's dashboard can mistake it
for a variable it already understands. Pinned by
`every_reported_status_name_is_mysqls_or_marked_as_this_servers` and
`a_name_this_server_invented_is_prefixed`.

**What it costs.** Two clock reads and two relaxed stores per command for
`Command` and `Time`, a handful of relaxed `fetch_add`s, and an
allocation-free scan of the leading keyword. Bytes are accumulated in plain
`u64`s in the packet framer and pushed into the shared counters once per
command, so a ten-million-row result set costs the same two adds as a `PING`.
Measured over a real socket, 200,000 prepared point reads by primary key, five
alternating runs per arm: **28,410 ns/op before, 28,486 ns/op after** — 0.3%,
against a run-to-run spread several times larger. (The published `points`
benchmark cannot see this change at all: `inlaysql-bench` links `inlaysql` and
`inlaysql-core` and not `inlaysql-server`.)

**What is still missing**, and named rather than left to be discovered: no
histograms or percentiles — these are counters, and a counter cannot describe a
latency distribution. No per-table or per-index statistics. No audit log;
`Inlaysql_com_account` counts privilege statements but nothing records which.
No `information_schema.processlist` (refused with `1235` naming the spelling
that works), no `performance_schema`, and no HTTP exporter — deliberately: this
workspace ships no HTTP server, and `SHOW STATUS` is what every agent that
scrapes MySQL already speaks.

Verified over a real socket in `crates/inlaysql-server/tests/wire.rs`, including
that a non-superuser sees only its own connections
(`a_non_superuser_sees_only_its_own_connections`) and that every id it was shown
is one it may `KILL`.

---

## Major

- **SQL gaps that hit real ORMs and BI tools** — *reported*. `SAVEPOINT` is
  supported (Laravel, Django and Rails all use it to implement nested
  transactions), but no views, no triggers, no `WITH RECURSIVE`, no
  `RANGE`/`GROUPS` window frames, no `CREATE INDEX IF NOT EXISTS`. Foreign
  keys are recorded and never
  enforced, and unlike SQLite there is no `PRAGMA` to switch enforcement on, so
  the "SQLite's own default" framing in `README.md` overstates the parity.
- **Silently ignored statements** — *reported*. `SET TRANSACTION ISOLATION
  LEVEL <anything>` returns OK, and `USE <any name>` is accepted as a cosmetic
  label over the same single file — so an application pointed at `staging`
  reads production. Both violate this project's own "refuse, never ignore"
  rule (`docs/architecture.md`).
- **Optimistic concurrency only** — *verified*. First-committer-wins surfaces
  as MySQL error 1213 for the client to retry, and `SELECT ... FOR UPDATE` is
  refused. Hot-row OLTP will spend its time retrying.
- **One writer process, enforced by an advisory lock** — *verified*. A second
  process is refused cleanly. The lock is advisory, so a process that does not
  ask can still write the file, and it is unreliable on NFS and SMB. There is
  no read replica and no scale-out; the deployment is one box, one process.
- **The SQL Logic Test figure is a subset** — *verified*. The published
  pass rate is over a self-curated subset, and is a regression gate rather than
  a compatibility score. `README.md` says so; it is repeated here because the
  number is easy to quote out of context.

---

## What this list is not

It is not a roadmap, and the ordering is by deployment risk rather than by
effort — several blockers above are small changes and at least two are
projects. It is also not exhaustive: it came from reading this repository, so
anything the code does not reveal about behaviour under real load is still
unknown, and the "no sustained or multi-core saturation workload" caveat in
`BENCHMARK.md` applies to every performance claim that touches it.
