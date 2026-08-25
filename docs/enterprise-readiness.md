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

### 1. A foreign commit may force a full retrieval-index rebuild — *reported*

`Engine::refresh_snapshot` runs before every statement, and when another handle
has bumped the write version, `adopt_committed_state` clears the text and
vector indexes and calls `restore_indexes` (`crates/inlaysql-core/src/engine.rs`).
`restore_indexes` uses the persisted index blob only when its stamp matches the
current write version, and otherwise re-scans every row of every table. The
blob is saved every `INDEX_PERSIST_INTERVAL` (1024) mutations, so the
mismatched case is the ordinary one.

If that reading holds, a mixed read/write server with a BM25 or vector index
pays a full re-index on every connection after every other connection's commit,
which is the difference between "concurrent" and "unusable". `docs/indexes.md`
says skipping a save "costs a rebuild on the next open and nothing else", which
would be contradicted by it.

### 2. No backup, restore or point-in-time recovery — *verified*

There is no backup, dump or snapshot API anywhere in the engine or the CLI.
`README.md` says the quiet part out loud: "Keep a backup you can restore from
something else."

`inlaysql vacuum` is compaction, not backup, and it opens read-write and holds
the exclusive advisory lock for the whole copy-and-rename — so it cannot run
against a live server, which holds a keeper handle for its lifetime.
`mysqldump` does not work either: `LOCK TABLES`, `FLUSH TABLES` and
`SHOW MASTER STATUS` are not in the wire shim's intercept set and reach the
core, which refuses them.

### 3. The change log cannot become replication or PITR as built — *reported*

`crates/inlaysql-core/src/cdc.rs` records `(version, table, row id, operation)`
and deliberately carries **no row data**. Retention is 4096 statements; a
consumer that falls behind is told `lost`. DDL is invisible to it.

That is a sound design for what it is — cache and search-index invalidation,
"re-read these row ids" — and it is not a replication log: no payloads, no
schema events, no consumer-managed retention. The only consumer surface opens
the database read-write, so it is locked out while the server runs.

### 4. Unbounded file growth in server mode — *reported*

`EngineOptions::page_reuse` defaults to false, and `ServerOptions` has no way
to turn it on. Space is therefore reclaimed only by stopping the server and
running `vacuum`. Note that page reuse is itself documented as unsound with
cross-process read-only handles (`docs/recovery.md`), so this is a real
trade-off rather than a switch someone forgot to wire up.

### 5. A hard transaction and statement ceiling near 1 MiB — *reported*

One commit record must fit one WAL region: `WAL_BLOCKS` (256) ×
`DEFAULT_PAGE_SIZE` (4096) = 1 MiB. An explicit transaction is refused once it
passes half of that; a single autocommit statement that exceeds it fails at
commit. `crates/inlaysql/tests/large_index.rs` records this biting at roughly
5,000 rows.

So a bulk `INSERT ... SELECT`, a wide `UPDATE`, or `DELETE FROM t` on a large
table is a hard error rather than a slow path, and the same bound caps a single
value at about 1 MiB.

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

### 8. No statement timeout, no cancellation, unbounded materialisation — *reported*

Sort and aggregate materialise with no spill and no memory budget, and the
server materialises an entire result set before writing a byte. There is no
statement timeout, no `KILL`, and no socket read or write timeout. One
`SELECT *` against a large table takes the process down and nothing can stop
it.

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
connection errors and "never the statement". No `EXPLAIN`. No
`SHOW PROCESSLIST`, no `KILL`. `information_schema` covers nine relations and
refuses joins and subqueries.

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
- **No idle-connection timeout against a 64-slot cap** — *reported*. Sixty-four
  idle clients exhaust the server permanently.
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
