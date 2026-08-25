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

### 6. Fully resident retrieval indexes, per connection — *reported*

The in-memory HNSW holds the whole corpus, and BM25 has no paged variant at
all — `Bm25Index` keeps the term dictionary, every postings list, the per-document
term lists and the row-id map in RAM. `Database::open_paged` exists but the
MySQL server never calls it. Each connection also carries its own 8 MiB decoded
page cache.

A 10M-vector corpus at 384 dimensions is roughly 15 GB of `f32` per connection
before the graph. `PLAN.md`'s 10M-vector goal is not reachable through the
server on this path.

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

### 8. No statement timeout or cancellation; unbounded materialisation — *partly fixed*

**Still open:** there is no statement timeout and no `KILL`. Both need
executor-level cancellation and are a separate project; `docs/server.md` says
so in the protocol table rather than leaving it to be discovered.

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

### 9. No TLS, one user, no grants — *verified*

The MySQL-wire server is plaintext with a single user and password, no user
table, no grants and no per-table permissions. `docs/server.md` states this
accurately and bluntly; it is the deployment constraint the rest of the auth
design is correctly built around, not an oversight. It is still the first thing
a security review stops at.

What *is* there and is sound: `mysql_native_password` and
`caching_sha2_password` are both real challenge-response, the comparison is
constant-time, the scramble comes from OS entropy and fails rather than falling
back to something guessable, and the RSA public-key exchange is refused with a
clear error rather than faked.

### 10. Effectively no observability — *reported*

No metrics, no counters, no exporter, no `log` or `tracing` dependency. No
query log or slow-query log, deliberately — the server logs accept failures and
connection errors and "never the statement". No `SHOW PROCESSLIST`, no
`KILL`. `information_schema` covers nine relations and refuses joins and
subqueries.

`EXPLAIN` now exists (`EXPLAIN`/`EXPLAIN QUERY PLAN`/`DESCRIBE <statement>`,
over the wire as well as in the engine) and reports which access path the
executor chose — scan, row-id point lookup, index range, hash join, index
nested loop, or which retrieval index answered a `bm25_score`/`vector_score`/
`fuse`. It reports no row counts, costs or selectivity, because there is no
statistics system here to draw them from; see `crates/inlaysql-core/src/explain.rs`.

Some reported session variables are fiction: `wait_timeout` and the
`net_*_timeout`s are reported but never enforced, and `max_connections` reports
`0` while the real cap is 64. A reported timeout that is not honoured is worse
than none, because a client tunes against it.

---

## Major

- **SQL gaps that hit real ORMs and BI tools** — *reported*. No `SAVEPOINT`
  (which is how Laravel, Django and Rails implement nested transactions), no
  views, no triggers, no `WITH RECURSIVE`, no `RANGE`/`GROUPS` window frames,
  no `CREATE INDEX IF NOT EXISTS`. Foreign keys are recorded and never
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
