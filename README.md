# InlaySQL

[![CI](https://github.com/inlaySQL/inlaysql/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/inlaySQL/inlaysql/actions/workflows/ci.yml?query=branch%3Amain)
[![WASM](https://github.com/inlaySQL/inlaysql/actions/workflows/wasm.yml/badge.svg?branch=main)](https://github.com/inlaySQL/inlaysql/actions/workflows/wasm.yml?query=branch%3Amain)

<!-- Both badges are pinned to `main`, because a badge that reports whatever ran
     most recently on any branch is not reporting anything. `trust.yml` has no
     badge on purpose: it is allowed to go red when the fuzzer finds something,
     and its output is the artifacts, not a colour. -->

**InlaySQL is an embedded database for Rust applications — the same shape as
SQLite: one file, no server, plain SQL. It adds the two things you otherwise
bolt on from outside: many writers at once, and search — vector and
full-text — that lives inside the database and is queried with ordinary SQL.**

> [!WARNING]
> **Experimental. Do not put data you care about in this yet.**
>
> InlaySQL is version `0.0.1` and has never been run in production by anyone.
> The short version of why: the on-disk format is pre-1.0 (the policy is
> *recreate the database*, not migrate — [`docs/recovery.md`](docs/recovery.md));
> crash-safety is proven by deterministic simulation rather than years of real
> hardware; and the known gaps are listed, not hidden — see
> [What this is not](#what-this-is-not). Use it for experiments, prototypes and
> anything you can rebuild from source data. If you find a bug, please open an
> issue — it is genuinely useful to us.

## Try it in 30 seconds

**In your browser, no install** — the real engine compiled to WebAssembly,
running in this page's own tab: **[inlaysql.github.io](https://inlaysql.github.io)**
— type SQL, run hybrid search, and save the database into your browser.

**On your machine:**

```sh
git clone https://github.com/inlaySQL/inlaysql
cd inlaysql
cargo run --example hybrid_search
```

```
keywords: "embedded database"
embedded query: "a storage engine that runs inside your application"

vector search only
  1. [1] 0.4183  embedded databases keep the whole engine inside your process
  2. [7] 0.2657  write ahead logging and crash recovery in storage engines
  3. [2] 0.1026  rust gives you memory safety without a garbage collector

BM25 only
  1. [3] 2.9030  an embedded database written in rust with vector retrieval
  2. [1] 1.2102  embedded databases keep the whole engine inside your process

hybrid (rank fusion)
  1. [1] 0.0325  embedded databases keep the whole engine inside your process
  2. [3] 0.0320  an embedded database written in rust with vector retrieval
  3. [7] 0.0161  write ahead logging and crash recovery in storage engines
```

The third ranking comes out of **one ordinary SQL statement**:

```sql
SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
FROM docs
ORDER BY score DESC
LIMIT 3;
```

No separate vector store, no application-side merge step, no second query
language. The planner recognises the retrieval functions, turns each into an
index probe, and fuses the two rankings.

## What you get

- **One file, no server.** Your whole database — tables, indexes, vectors,
  full text — is a single `.inlay` file you can copy, back up and ship.
- **Writers that do not queue.** MVCC with concurrent committers; eight
  writers do roughly 13x the work of one (measured, not claimed — see
  [Performance](#performance)).
- **Search is SQL.** `VECTOR(n)` columns, BM25 indexes, and a `fuse()` that
  ranks across both in one statement — no sidecar process to keep in sync.
- **Your ORM already speaks it.** `inlaysql serve --mysql` serves the
  database over the MySQL wire protocol, so existing MySQL clients and ORMs
  connect as-is.
- **Crash-safe by construction.** Every crash and torn-write schedule is
  replayed byte-for-byte in CI by a deterministic simulator, and the whole
  engine is `#![forbid(unsafe_code)]` outside one Linux I/O backend.
- **Async without a runtime.** Plain futures that Tokio, async-std, smol —
  or nothing at all — can drive.

## Where to go next

| If you want to… | Read |
| --- | --- |
| embed it in an application | [Using it](#using-it) |
| learn the SQL dialect | [The SQL surface](#the-sql-surface) |
| see the benchmark table, wins and losses | [Performance](#performance) |
| check what is missing before you depend on it | [What this is not](#what-this-is-not) |
| read the honest engineering plan | [Next](#next), [`docs/PLAN.md`](docs/PLAN.md) |

## Why

- **SQLite's model, not Postgres's.** One file, no server, a schema you
  already know — but with concurrent writers and native retrieval instead of
  the single writer and bolted-on extensions SQLite ships today.
- **Retrieval is SQL, not a second system.** `VECTOR` and BM25 are additions
  to the dialect the planner understands, not a separate vector store an
  application has to keep in sync and merge client-side.
- **Correct before fast.** `inlaysql-core` is deterministic-simulation-tested
  — thousands of seeded crash/torn-write schedules replay byte-for-byte in
  CI — before any number in this file gets trusted.
- **Honest about the trade.** Every benchmark below regenerates from a script
  in this repo, wins and losses both — see [Performance](#performance). What
  is not built yet is listed just as plainly in
  [What this is not](#what-this-is-not) and [Next](#next).

How the pieces fit together: [Using it](#using-it) for the API,
[The SQL surface](#the-sql-surface) for the dialect, and
[Layout](#layout) for how the crates and the `no_std` boundary are arranged.
For the current engineering sequence and cloud continuation handoff, see
[`docs/PLAN.md`](docs/PLAN.md).

## Using it

```rust
use inlaysql::{Database, Value};

let mut db = Database::open("app.inlay")?;

db.execute(
    "CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR(384))",
    &[],
)?;
db.execute("CREATE INDEX docs_body ON docs (body)", &[])?;
db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])?;

db.execute(
    "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
    &[
        Value::Integer(1),
        Value::Text("an embedded database written in rust".into()),
        Value::Vector(embedding),          // straight from your model
    ],
)?;

let results = db.query(
    "SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
     FROM docs ORDER BY score DESC LIMIT 5",
    &[Value::Vector(query_embedding), Value::Text("rust database".into())],
)?;
```

A database is **one file**. There is no server, no sidecar index directory and
nothing to deploy.

### Prepared statements

`execute` and `query` parse and plan from text on every call. For a statement
that runs in a loop, prepare it once and bind the parameters per execution:

```rust
let lookup = db.prepare("SELECT body FROM docs WHERE id = ?")?;

for id in ids {
    let row = db.query_prepared(&lookup, &[Value::Integer(id)])?;
}
```

For a large result that is consumed one row at a time, avoid retaining the
whole `ResultSet`:

```rust
let scan = db.prepare("SELECT id, body FROM docs")?;
let count = db.query_prepared_each(&scan, &[], |row| {
    send_row(row)?;
    Ok(())
})?;
```

The callback's slice is borrowed for that call only. Copy values you need to
keep; otherwise the engine reuses its projected-row allocation as it streams.

The same statement works on `AsyncDatabase` (`prepare(...).await`,
`execute_prepared`, `query_prepared`); the handle is reference-counted, so
holding one and sharing clones between tasks is free.

A plan holds column *ordinals*, so a statement carries the table definition it
was planned against and re-checks it on every execution. If that table has
changed underneath, the statement fails with `Error::Stale` — never with a value
read out of the wrong column — and re-preparing is the fix.

### Async, without a runtime

The same database behind an async API. Statements run on a dedicated I/O
thread, so your executor is never blocked on an `fsync`:

```rust
use inlaysql::{AsyncDatabase, Value};

let db = AsyncDatabase::open("app.inlay").await?;
db.execute("INSERT INTO docs (body) VALUES (?)", &[Value::Text(body)]).await?;
let results = db.query("SELECT id, body FROM docs WHERE id = ?", &[Value::Integer(7)]).await?;
```

These are plain futures — Tokio, async-std and smol all drive them, and
`inlaysql::block_on` drives one without any runtime at all. Nothing in the
crate depends on a runtime, so embedding InlaySQL never forces one on you.

### Choosing an I/O backend

A backend is a `Device`: read, write, sync, at byte offsets. `Database::open`
uses ordinary blocking file I/O; on Linux you can hand it an `io_uring` ring
instead.

```rust
use inlaysql::Database;
use inlaysql_uring::UringDevice;                 // Linux only

let db = Database::open_on(UringDevice::open("app.inlay", 32)?)?;
```

The engine above the seam is unchanged, and
`crates/inlaysql/tests/backends.rs` runs the same query suite against every
backend to keep it that way. The `unsafe` that `io_uring` submission requires
is confined to `inlaysql-uring`; `inlaysql` and `inlaysql-core` remain
`#![forbid(unsafe_code)]`.

### Choosing a durability level

Every commit's `fsync`/`F_FULLFSYNC` barrier is measured at 97% of a
single-writer commit's wall-clock time (`PERF.md`). `EngineOptions::durability`
is the opt-in to relax it:

```rust
use inlaysql::{Database, Durability, EngineOptions, FileDevice};

let db = Database::open_on_with_options(
    FileDevice::open("app.inlay")?,
    EngineOptions { durability: Durability::Normal, ..EngineOptions::default() },
)?;
```

`Durability::Full` (the default; nothing committed is ever lost) is
unaffected either way. `Durability::Normal` measured 32x this project's
single-writer commit throughput, trading a bounded, documented amount of
loss on **power failure only** — never on a process or OS crash, and never
torn or invented state. See [`docs/recovery.md`](docs/recovery.md#durability-levels)
for the exact loss bound, the per-platform mapping (it is not the same trade
on macOS and Linux), and the cross-handle rule when two handles on one file
disagree, and `PERF.md` for the measured numbers.

### Handing the database to an agent

An InlaySQL file is an MCP tool. No glue code, no schema translation, no vector
store to keep in sync:

```sh
inlaysql serve --mcp app.inlay          # read-only; add --allow-writes to permit writes
```

The agent gets `schema`, `query`, `hybrid_search` and `changes`; `execute` is
not even advertised unless writes are allowed. Read-only is enforced by
*planning* the statement, results are capped by row count and by bytes, and
embeddings render as `<vector(384)>` rather than as 384 floats in the model's
context. See [`docs/mcp.md`](docs/mcp.md).

### Speaking MySQL over the wire

```sh
inlaysql serve --mysql app.inlay --password-env INLAYSQL_PASSWORD
```

`inlaysql-server` speaks the MySQL wire protocol over one InlaySQL database
file, so a client that already knows how to talk to MySQL — `mysql`, PDO,
mysqli, JDBC, `mysql2` — can talk to this instead. `AUTO_INCREMENT`,
`ENGINE=`, `CHARSET`/`COLLATE`, `UNSIGNED` and MySQL's own DDL and upsert
syntax are translated in a shim that never touches the engine's SQLite
dialect (`inlaysql-core` gains nothing from this crate, which is what the
`determinism` CI job polices); a dropped clause is never silent — it comes
back as a MySQL `1618` warning naming it, visible in `SHOW WARNINGS`.

**`MATCH ... AGAINST` is the engine's own BM25, under MySQL's spelling.** A
client that writes MySQL's full-text syntax gets the native retriever rather
than an emulation: `MATCH (body) AGAINST (?)` is rewritten to
`bm25_score(body, ?)`, and used as a whole `WHERE` conjunct it *drives* the
query — the BM25 probe supplies the rows and the remaining predicates filter
inside its walk, which is the spelling Laravel Scout's database engine emits.
`CREATE FULLTEXT INDEX` and `ALTER TABLE ... ADD FULLTEXT INDEX` — what
Laravel's `$table->fullText()` compiles to — translate to
`CREATE INDEX ... USING FULLTEXT`. Boolean mode and query expansion are
refused by name (`1235`) rather than silently answered as if they were
natural-language mode, because a search that quietly ignores `+required
-excluded` is worse than one that says it cannot. Full translation table and
refusal list in
[`docs/server.md`](docs/server.md#full-text-search-match--against-translated-to-the-native-bm25-probe).

**A stock Laravel 11 app runs against this for real now** — not an
approximation of one. `composer create-project laravel/laravel`, `.env`
pointed at `inlaysql serve --mysql`, and `php artisan migrate` completes the
default `users`/`cache`/`jobs` migrations, plus a `posts` table with a foreign
key; ordinary Eloquent traffic afterward — `create`, `find`, a model save with
a qualified `updated_at`, `whereIn`, a raw `JOIN`, `whereHas`, `withCount`,
eager loading, `paginate()`, and `upsert()`'s own `ON DUPLICATE KEY UPDATE` —
all work. Running the real thing found two shim bugs that a hand-written
approximation of Laravel's SQL had missed for the same reason PLAN.md warned
it would: `EXISTS (SELECT ... FROM information_schema...)`, the exact shape
`hasTable()`/`hasColumn()` compile to, was misrouted by a heuristic the
subquery's own `schema()` call fooled; both are fixed. Laravel's
`->foreignId()->constrained()` still does not get its foreign key recorded —
it compiles to a standalone `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY`,
which is a documented, deliberate limitation
([`docs/server.md`](docs/server.md#mysql-only-ddl-is-translated-not-invented)) —
declare it inside the initial `Schema::create()` instead. Window functions
(`ROW_NUMBER() OVER (...)`) were the wall until
AHL-494 and now go through byte-for-byte, because MySQL 8 spells them the way
SQLite does and the shim has no reason to touch them. The collation mapping
still folds ASCII case only: `WHERE name = 'ADA'` matches a stored `'ada'`
under a `*_ci` collation the way MySQL does, but not an accent
(`'é' = 'e'`). It binds `127.0.0.1` by default and the wire is plaintext until
`--tls-cert`/`--tls-key` are given — `--tls-required` then refuses any login
that did not encrypt, and `--strong-passwords` stores salted PBKDF2 instead of
the MySQL plugins' unsalted two-hash verifiers, so a stolen database file is
not a stolen password list. Accounts, `GRANT`/`REVOKE` and per-table
privileges live in the file itself; the `--user`/`--password` flags are the
whole credential only until the first `CREATE USER`.
[`docs/server.md`](docs/server.md) has the full security posture, the
function-by-function mapping and the complete divergence list, each checked
against a real MySQL 8.4.11.

### In a browser

**<https://inlaysql.github.io/inlaysql/>** — the whole database, running
in your tab. Nothing is sent anywhere; there is no server behind the page. Or
run it yourself:

```sh
./crates/inlaysql-wasm/build.sh --serve
```

The engine compiles to `wasm32` — the core is `no_std` and trait-based, so this
was a matter of supplying a backend rather than porting anything. 661 KiB
gzipped. The database is a `Vec<u8>` in the *same format* the CLI reads, so a
database built in a browser tab saves to OPFS, downloads, and opens with
`inlaysql serve --mcp` — and back again.

### On an edge runtime

```sh
cd crates/inlaysql-wasm/edge && npm ci && npm run smoke
```

The same module, on Cloudflare Workers: a retrieval index built once natively
and shipped to the edge as a static asset, queried in the isolate that took the
request. No database to connect to, no pool, no region to be far from.

```
GET /search?q=embedded%20database&limit=3

{ "results": [
  { "id": 3, "body": "an embedded database written in rust with vector retrieval", "score": 0.0328 },
  { "id": 1, "body": "embedded databases keep the whole engine inside your process", "score": 0.0323 },
  { "id": 5, "body": "approximate nearest neighbour search over embeddings",        "score": 0.0159 }
] }
```

The file that worker opens was written by the *native* build. Both demos, the
sizes and what CI checks: [`docs/wasm.md`](docs/wasm.md).

### Change data capture

```sh
inlaysql changes app.inlay --from 41
```

```
41	insert	notes	17
42	update	notes	3
43	delete	notes	9
```

A record says *what* changed, not what it became — read the row for its current
contents. A consumer that has fallen outside the retention window is told so
(`lost`) rather than handed a silently short list.

### Online backup

```sh
inlaysql backup app.inlay app-2026-08-25.inlay
```

Takes a consistent copy while the database is being written to — including by
`inlaysql serve --mysql` in another process, which `inlaysql vacuum` cannot do
because it needs the exclusive lock the server holds. The copy is one committed
snapshot: never a mix of two commits, and never two tables read at two
different moments the way a statement-at-a-time dump can be. From code it is
`db.backup_to("app-2026-08-25.inlay")?`.

The result is an ordinary database file, so restoring is opening it or moving
it back — there is no restore command because there is nothing for one to do.
It refuses to overwrite an existing destination, and a failure leaves no file
at all, so a backup that exists is one that finished.

Nothing about this is compaction: page numbers are preserved, so a file that
grew large from deletes copies at its *live* size (holes, stored sparsely) but
reports its old size. Use `inlaysql vacuum` to actually shrink one. One
constraint, and it is real: a backup taken from outside the writing process
cannot be pinned against page reclamation, so do not take one of a database a
writer has `--page-reuse` on for — see
[`docs/server.md`](docs/server.md#backing-up-a-running-server).

## The SQL surface

SQLite's dialect is the baseline. Stage 1 implements a slice of it, plus:

| Addition | Meaning |
| --- | --- |
| `VECTOR(n)` | A column of fixed-width `f32` embeddings. |
| `VECTOR(n, INT8)` | The same SQL value with deterministic per-vector int8 scalar quantisation in rows and ANN storage (about 4x smaller, with measured recall loss). |
| `INTEGER PRIMARY KEY` | SQLite's row-id alias: the key *is* the row's address, so `WHERE id = 42` is one tree descent, not a scan. |
| `CREATE [UNIQUE] INDEX` / `DROP INDEX` | Declares an index. On `TEXT` it is a BM25 index by default (`USING BTREE` asks for a scalar one instead); on `VECTOR` it is ANN; on `INTEGER`/`REAL` it is a scalar B-tree, which may span more than one column and may be declared `UNIQUE`. |
| `vector_score(column, embedding)` | Approximate nearest neighbours over a `VECTOR` column, under the distance its index was built with. |
| `CREATE INDEX ... (embedding vector_l2_ops)` | pgvector's operator-class spelling, choosing the distance an ANN index is built and searched under: `vector_cosine_ops` (the default) or `vector_l2_ops`. |
| `SET inlaysql_hnsw_ef_search = <n>` | The ANN recall/latency trade, per session. `0` (the default) leaves the index's own tuning in force; `EXPLAIN` reports the `ef` each query will run at. |
| Binding a `VECTOR` parameter | Over the MySQL wire an embedding binds as packed little-endian `f32` — MySQL 9's own `VECTOR` layout — rather than travelling as decimal text inside the SQL. Measured on a 112.9 MiB corpus: 127.9 MiB on the wire instead of 363.9 MiB, and half the load time. |
| `bm25_score(column, 'terms')` | BM25 relevance over a `TEXT` column. |
| `fuse(a, b, ...)` (alias `rrf`) | Reciprocal rank fusion over the retrieval expressions inside it. |

Retrieval functions are not scalar functions evaluated per row — the planner
hoists them out and answers each from an index. An index exists only where a
`CREATE INDEX` declared it (or where a pre-`CREATE INDEX` database was
grandfathered): a query that scores an unindexed column is an error, not a
silent scan.

### Scalar indexes and joins that use them

`CREATE INDEX users_email ON users (email)` — or `pairs_ab ON pairs (a, b)`
for a composite key — builds an ordinary ordered B-tree over one or more
`INTEGER`/`REAL`/`TEXT` columns (AHL-423), living in the same copy-on-write
tree as the rows, so it gets WAL, crash recovery and MVCC rebase for free. A
top-level equality or range predicate on an indexed column becomes a range
probe instead of a full scan — worth 548.71x on point probes and 131.39x on
range scans over the engine's own unindexed scan (`BENCHMARK.md`) — and `CREATE UNIQUE INDEX` enforces a
uniqueness constraint at insert time. The same index also answers the inner
side of a join: `FROM posts JOIN users ON posts.user_id = users.id` probes
`users` by one tree descent per outer row instead of materialising and
scanning it, when the `ON` is a top-level equality on the inner table's
`INTEGER PRIMARY KEY` or an indexed column (AHL-464). A full-scan equi-join on same-storage-class keys builds a hash table over the
inner side rather than comparing every pair, and `ANALYZE` now lets the
planner cost a choice between that hash build and an index probe when it
holds a complete, current statistics snapshot for the join
(`docs/research/cost-planner.md`) — missing, corrupt or stale stats fall back
to the same shape rule. With those statistics the planner may also exchange
which of a two-table inner join's tables drives (AHL-512, cost model
corrected in AHL-524): the smaller table drives, as a plan rewrite with
every ordinal remapped. A `LIMIT` with no `ORDER BY` keeps its written
order, because there a different order is a different result set. See
[What this is not](#what-this-is-not).

### Which distance a vector index uses

`vector_score` scores with the metric its index was built under, chosen once at
`CREATE INDEX` and then fixed:

```sql
CREATE INDEX items_embedding ON items (embedding);                  -- cosine
CREATE INDEX items_embedding ON items (embedding vector_l2_ops);    -- Euclidean
CREATE INDEX items_embedding ON items USING hnsw (embedding vector_l2_ops);
```

The spelling is pgvector's **operator class**, and the third line is pgvector's
own statement running unchanged. Writing nothing means `vector_cosine_ops`, so
every database and every query that predates this is untouched, byte for byte
— a cosine index writes the same graph format and computes the same score it
always did.

Under cosine the score is the cosine similarity in `[-1, 1]`; under
`vector_l2_ops` it is the **negated** Euclidean distance, so `0` is an exact
hit, further is more negative, and `ORDER BY score DESC LIMIT k` is still the
`k` nearest. `EXPLAIN` names the metric, always — which distance ranked the
rows decides which rows came back:

```
SEARCH items USING VECTOR INDEX items_embedding (embedding vector_l2_ops) FOR vector_score
```

**The metric belongs to the index, not to the query.** An HNSW graph's
neighbour lists *are* the answer to "what is near what" under one distance, so
a graph built one way and searched another returns plausible, wrong rows with
no error anywhere. The metric therefore travels with the graph on disk, and a
graph whose metric does not match its declaration is rebuilt rather than
reused. It also decides what is stored: cosine L2-normalises on the way in so
the comparison is a dot product, and `vector_l2_ops` does not, because the
magnitude that would throw away is exactly what it measures. One column carries
one vector index, because `vector_score(embedding, ?)` names the column and not
the metric and could not say which of two it meant.

**There is no `vector_ip_ops`.** Inner product is not a metric — no triangle
inequality, and a vector is generally not its own nearest neighbour under it —
and every argument HNSW makes for a greedy walk over a diversity-pruned
neighbour list assumes one. pgvector and FAISS ship it as a known
approximation; this refuses it and says so, with the transformation that is
exact: for unit-length embeddings, cosine ranks identically to inner product.

### Choosing the recall/latency point: `ef_search`

The metric belongs to the index. The **candidate list** belongs to the query:
`ef` is how many candidates the graph walk may hold at once, and it is the only
thing that trades recall against latency at query time. pgvector spells it
`SET hnsw.ef_search`; a MySQL system variable cannot hold a dot, so here it is

```sql
SET inlaysql_hnsw_ef_search = 400;      -- more recall on the query that matters
SET inlaysql_hnsw_ef_search = 0;        -- back to the index's own tuning
```

and, embedded, `Database::set_vector_ef_search(Some(400))`. `EXPLAIN` reports
the number that will actually be used, because an operating point nobody can
see is one nobody can choose:

```
SEARCH items USING VECTOR INDEX items_embedding (embedding vector_cosine_ops) FOR vector_score (ef=400)
```

`0` is the default and means exactly what every query on this engine has always
done. That untuned point is not a constant — the shipped tuning widens the beam
with the number of candidates asked for, so the same index searches a
`LIMIT 10` at `ef = 80` and a `LIMIT 100` at `ef = 800`, which is why `EXPLAIN`
reports it per query rather than `@@inlaysql_hnsw_ef_search` reporting it once.

**A beam narrower than the answer is refused, not widened.** `ef` must be at
least the query's `LIMIT` — pgvector's rule for `hnsw.ef_search` as well —
because a walk holding fewer candidates than the answer cannot come back with
the answer. `SET inlaysql_hnsw_ef_search = 5` then `LIMIT 10` fails and names
both numbers. Widening it silently would search at a number the caller did not
choose while reporting the one they did; returning a short list would drop rows
without saying so.

`m` and `ef_construction` — the parameters that shape the stored graph rather
than one query — are not settable per index yet; every index is built at the
shipped `m = 16`, `ef_construction = 200`.

### What a retrieval function means in a join

A retrieval index lives over one table's rows, so when a query joins tables the
retrieval expression may reference **only the driving table** (the first table
in `FROM`). The ranking is computed over that table's rows, then the join runs,
then `WHERE` and `LIMIT` apply to the joined result. An inner join can therefore
drop ranked rows, and a one-to-many join can expand them past `LIMIT`; the score
still reflects the driving table's rows only. A query that names a
non-driving table's column in `vector_score`/`bm25_score` is rejected at prepare
time rather than answered incorrectly. Retrieval and aggregation cannot be
combined in one query.

### Why fusion works on ranks, not scores

Cosine similarity lives in `[-1, 1]`, a Euclidean score is an unbounded
negative distance, and BM25 is unbounded and depends on corpus statistics. Normalising one against the other needs calibration nobody has at
query time. Reciprocal rank fusion throws the raw scores away and combines
*positions*:

```
score(d) = Σ_retrievers 1 / (60 + rank(d))
```

That is why, in the demo above, the row both retrievers ranked well beats the
row that only one of them loved.

Also supported: `SELECT` with projections and `*`, `WHERE` filters over scalar
expressions (`column <op> value`, `AND`/`OR`, arithmetic and comparisons),
`DISTINCT`, multi-key `ORDER BY` (with `NULLS FIRST`/`NULLS LAST`) on a
column, a scalar expression or a projection alias, `LIMIT` and `OFFSET`
(both literal or a bound `?`), `?` bind parameters, `SELECT` without a `FROM`
clause over scalar expressions (`SELECT 1 + 2 * 3`, comparisons, `NULL` and
unary minus), `UPDATE` / `DELETE` with `WHERE` filters and expressions on the
right-hand side, `INSERT ... SELECT`, `INNER JOIN` and `LEFT JOIN` on an
equality predicate (nested-loop, with the inner side probed by index where
the rule above applies), the aggregate functions `COUNT`, `SUM`, `MIN`,
`MAX` and `AVG` with `GROUP BY`, `HAVING`, `COUNT(DISTINCT x)` and
`GROUP_CONCAT`, three-valued logic (`NOT`, `IS NULL`, `IS NOT NULL`), the
expression operators `LIKE` (with `ESCAPE`), `IN` over a literal list or a
subquery, `BETWEEN`, `CASE` in both its forms, `CAST`, `||`, blob literals
(`X'..'`) and `COLLATE` (SQLite's three collating sequences — `BINARY`,
`NOCASE`, `RTRIM` — column-level, expression-level and on an index's column
list), the scalar function library (`length`, `upper`, `lower`, `substr`,
`trim`/`ltrim`/`rtrim`, `replace`, `instr`, `abs`, `round`, `coalesce`,
`ifnull`, `nullif`, scalar `min`/`max`, `random`, `hex`) and the date/time
family (`date`, `time`, `datetime`, `strftime`, `unixepoch`,
`CURRENT_TIMESTAMP`), `BEGIN`/`COMMIT`/`ROLLBACK` as SQL, `SAVEPOINT`/
`RELEASE [SAVEPOINT]`/`ROLLBACK TO [SAVEPOINT]` (`savepoint.rs`) — the engine
has no partial in-place undo, so `ROLLBACK TO SAVEPOINT` is a full
`ROLLBACK` plus a deterministic replay of the transaction's own log up to
that point, not a nested transaction,
`DROP TABLE [IF EXISTS]`, `CREATE TABLE IF NOT EXISTS`, `ALTER TABLE` (`ADD COLUMN`,
`RENAME TO`, `RENAME COLUMN`, `DROP COLUMN`), `CREATE TABLE` constraints
(`DEFAULT`, `NOT NULL`, `UNIQUE`, `CHECK`; a foreign key is recorded and left
unenforced, SQLite's own long-standing default), `INSERT OR IGNORE`/
`OR REPLACE`, `ON CONFLICT DO NOTHING`/`DO UPDATE` (upsert), and `RETURNING`
on `INSERT`/`UPDATE`/`DELETE`.

Subqueries too, since AHL-463: a scalar `(SELECT ...)`, `IN (SELECT ...)`,
`EXISTS (SELECT ...)`, a derived table (`FROM (SELECT ...)`), and the
correlated form of each. They are not decorrelated — a correlated subquery is
re-evaluated per outer row — and one in an `UPDATE`, `DELETE` or
`INSERT ... VALUES` is refused rather than half-run.

Window functions too, since AHL-494: `OVER (PARTITION BY ... ORDER BY ...)`
under SQLite's own grammar — `row_number`, `rank`, `dense_rank`, `ntile`,
`lag`/`lead`, `first_value`/`last_value`/`nth_value`, `percent_rank`,
`cume_dist`, the aggregate family
(`sum`/`count`/`avg`/`min`/`max`/`group_concat`) `OVER (...)`, `ROWS`,
`RANGE` and `GROUPS` frames (and SQLite's own implicit default, itself
`RANGE`-shaped), named windows (`WINDOW w AS (...)`), and
`FILTER (WHERE ...)` on an aggregate whether or not it is windowed.
`RANGE`/`GROUPS` are not approximated with `ROWS` — a value-based `RANGE`
and a peer-group-counted `GROUPS` both answer a different question than a
position-based `ROWS` the moment `ORDER BY` has ties, so both reinterpret a
`CURRENT ROW` bound (start *or* end, unlike `ROWS`) as the current row's
whole peer group, and `RANGE`'s own `<n> PRECEDING`/`FOLLOWING` bounds
compare `ORDER BY` values rather than counting rows — legal only with
exactly one `ORDER BY` term, the same restriction sqlite3 has
(`window_functions.test`). They reach the MySQL server unchanged, since
MySQL 8 spells every one of them the same way —
[`docs/server.md`](docs/server.md) has that argument.

`UNION`/`INTERSECT`/`EXCEPT` and non-recursive `WITH`, since AHL-473. Every
compound operator shares one precedence and chains left-associatively; the
per-column comparison — for dedup and for the compound's own `ORDER BY` — is
always the *left* arm's collation, however many operators deep; `UNION`'s
dedup keeps the last-occurring row of a colliding group where
`INTERSECT`/`EXCEPT` keep the first, deduplicated. A `WITH` reference becomes
a derived table planned once, but a CTE referenced twice may *run* once per
reference rather than being shared — this engine's own choice, not a bug, and
pinned as such in `ctes.test`. `WITH RECURSIVE` (`recursive_cte.test`) runs by
semi-naive iteration rather than the plan-once/clone approach an ordinary CTE
gets: the seed runs once, then the recursive term runs repeatedly, each step
seeing only the previous step's *new* rows rather than the whole table so
far, until a step adds nothing new — the same algorithm SQLite's own VDBE
uses, verified against it including the trap a naive version falls into (a
row that repeats one already produced has to stop propagating too, under
`UNION`, or a cyclic recursive term never converges). The recursive term may
reference the CTE exactly once, in its own `FROM`, never in a subquery, and
never with an aggregate or window function over it — the last two are a real
limit of the algorithm, not only a SQLite restriction being matched: a step
only ever sees that step's new rows, never the whole table an aggregate would
need.

`CREATE TABLE ... WITHOUT ROWID` (`without_rowid.test`): the row is stored
under its own primary key's encoded bytes — the same collation-aware
ordered-byte encoding a scalar secondary index already used for its keys,
reused here as the *primary* storage key — rather than under a hidden,
engine-assigned row id, so the table's natural scan order is primary-key
order and there is no `rowid` pseudo-column to select. A lone
`INTEGER PRIMARY KEY` does not become a row id alias here the way it does on
an ordinary table: a `NULL` in it is a `NOT NULL` violation, not an
auto-assigned key, since there is no row id counter to assign from — for the
same reason `AUTOINCREMENT` is refused outright on one of these tables, not
merely ineffective. Two gaps are disclosed rather than silently dropped: a
secondary index (`CREATE INDEX`, or a `UNIQUE` constraint on anything but the
primary key itself) is refused, because an index entry points back to a row
by row id and this table has none; and joining one of these tables against
anything else in the same query is refused at plan time, because every join
strategy this engine has reads its inner side through a row-id-based
mechanism. `INSERT OR IGNORE`/`OR REPLACE`, `UPDATE`, `DELETE`, `DROP TABLE`,
`RETURNING` and aggregates all work, keyed by the primary key instead of a
row id throughout.

`CREATE TEMPORARY TABLE` (`CREATE TEMP TABLE` too, `temp_table.test`): an
ordinary, row-id-keyed table — nothing about how a row is addressed changes,
unlike `WITHOUT ROWID` above — routed by table name to an in-memory backend
instead of the durable one it would otherwise share, gone the moment this
engine closes and invisible to any other handle open on the same file in the
meantime, the same as sqlite3's own `TEMP` schema. It shadows a durable table
of the same name for as long as it exists (confirmed against sqlite3: a
durable and a temporary table of the same name coexist without colliding,
and an unqualified reference resolves to the temporary one). Because its rows
are ordinary row-id-keyed rows behind the same `Storage` methods every join
strategy already reads through, joining one against a durable table works
with no special-casing at all — the one gap `WITHOUT ROWID` has that this
does not. Disclosed rather than silent: `CREATE INDEX` on one (and a `UNIQUE`
beyond a single `INTEGER PRIMARY KEY`) is refused, for the same reason as
`WITHOUT ROWID`'s — a scalar index entry's key carries the *index's* name,
not the table's, so the storage router has nothing to route a `CREATE INDEX`
by; `ALTER TABLE` on one is refused outright; and creating or dropping one
inside an explicit transaction is refused, because its declaration is not
buffered the way an ordinary `CREATE TABLE`'s is, so `ROLLBACK` could not
undo it — row-level writes to one that already exists are unaffected by that
last restriction and are fully transactional.

Refused explicitly rather than silently ignored, and confirmed against
sqlite3 to be refused there too rather than a gap on a to-do list:
`DISTINCT` inside a window function's argument list (`SUM(DISTINCT x) OVER
(...)`, sqlite3: "DISTINCT is not supported for window functions", the exact
message this engine gives), `COUNT(DISTINCT *)` (not valid sqlite3 syntax at
all — a parse error there, a plan-time refusal here, same statement
refused), `GROUP_CONCAT(DISTINCT x, sep)` with an explicit separator
(sqlite3: "DISTINCT aggregates must have exactly one argument", since a
separator is not part of what is being deduplicated — the single-argument
form, `GROUP_CONCAT(DISTINCT x)`, works), `CREATE COLLATION` (not a SQL
statement sqlite3 has either — a collation is registered through its C API,
which a `CREATE TABLE`/`SELECT` surface has no equivalent of, so a name
outside `BINARY`/`NOCASE`/`RTRIM` is refused rather than silently compared
byte-wise under a name that promises otherwise, the same as sqlite3 refuses
an unregistered collation name), and the partial-write conflict resolutions
(`INSERT OR ROLLBACK`/`OR FAIL`, `UPDATE OR REPLACE`/`OR IGNORE` — a
statement here is already atomic, so they cannot mean what they say).

## SQL Logic Test

Compatibility is measured against SQLite's
[SQL Logic Test](https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki)
corpus, in the standard `statement ok` / `query <types>` format. The harness is
`inlaysql::sqllogictest`; a curated subset lives in
`crates/inlaysql/tests/sqllogictest/` and runs in CI on every push.

```sh
cargo test -p inlaysql --test sqllogictest          # fail on any mismatch
cargo run -p inlaysql --bin sqllogictest -- \
  crates/inlaysql/tests/sqllogictest/*.test          # print the pass rate
```

Current pass rate over the subset: **1307/1307 (100%)** — covering `CREATE TABLE`,
`INSERT`, projection, `WHERE`, `DISTINCT`, `ORDER BY` (column, expression,
alias, multi-key, `NULLS FIRST`/`LAST`), `LIMIT`/`OFFSET` (literal or bound),
type coercion and affinity, `SELECT`-without-`FROM` scalar expressions,
expressions in the projection and `WHERE` of `FROM` queries, `UPDATE`/
`DELETE`, `INSERT ... SELECT`, `INTEGER PRIMARY KEY`, three-valued logic
(`NOT`, `IS NULL`, `IS NOT NULL`), `INNER JOIN` and `LEFT JOIN`, including the
index nested-loop join, the aggregate functions (`COUNT`, `SUM`, `MIN`,
`MAX`, `AVG`) with `GROUP BY`, `HAVING`, `COUNT(DISTINCT x)` and
`GROUP_CONCAT`, scalar B-tree `CREATE INDEX` / `DROP INDEX` (including
`UNIQUE` and composite keys) alongside `CREATE INDEX` for BM25/ANN, `LIKE`,
`IN`, `BETWEEN`, `CASE`, `CAST`, `||` and blob literals, `COLLATE` with
SQLite's three collating sequences (`BINARY`, `NOCASE`, `RTRIM`) resolved by
SQLite's own rules, declared constraints (`DEFAULT`, `NOT NULL`, `UNIQUE`,
`CHECK`, recorded foreign keys), `DROP TABLE`, `ALTER TABLE`,
`BEGIN`/`COMMIT`/`ROLLBACK`, every conflict clause (`INSERT OR IGNORE`/
`REPLACE`, `ON CONFLICT DO NOTHING`/`DO UPDATE`) and `RETURNING`, subqueries
in every read position (scalar, `IN (SELECT ...)`, `EXISTS`, derived tables,
correlated and uncorrelated), `UNION`/`INTERSECT`/`EXCEPT`/`WITH` (recursive
and not — `ctes.test`, `recursive_cte.test`), `CREATE TABLE ... AS SELECT`,
`CREATE TABLE ... STRICT` (`strict.test`), `SAVEPOINT`/`RELEASE`/
`ROLLBACK TO SAVEPOINT` (`savepoint.test`), the window functions of AHL-494
including `percent_rank`/`cume_dist` and explicit `RANGE`/`GROUPS` frames
(`window_functions.test`), `CREATE TABLE ... WITHOUT ROWID`
(`without_rowid.test`), and `CREATE TEMPORARY TABLE`/`CREATE TEMP TABLE`
(`temp_table.test`). The
number is meant to grow (and be reported) as the dialect matures — it does not
yet include the parts of the *SQLite project's own* sqllogictest corpus that
exercise `WITH RECURSIVE`, which this subset has not pulled in and adapted,
even though the dialect now has the feature (verified against sqlite3
directly instead, in `recursive_cte.test`).

One file in that subset asserts **refusals** rather than results, because the
alternative was worse than a missing feature: `INSERT ... ON CONFLICT`,
`INSERT OR REPLACE`, `RETURNING` and every `CREATE TABLE` constraint (`DEFAULT`,
`NOT NULL`, `UNIQUE`, `CHECK`, `REFERENCES`) used to parse and then be silently
discarded — the statement reported success while doing something the caller did
not ask for. They are now refused explicitly until the dialect implements them.
[`TESTING.md`](TESTING.md) also names the three places the dialect knowingly
disagrees with SQLite: rendering a `REAL` as text, columns being *typed*
rather than merely affine (four of five affinities convert or reject where
SQLite's affinity is a preference that keeps a value it cannot convert), and
one row-id counter per database rather than per table. Integer overflow used
to be a fourth; AHL-412 made arithmetic promote to `REAL` on overflow the way
SQLite does, so that one is gone.

How everything else is tested — deterministic simulation, metamorphic and
differential logic-bug tests, fuzzing, cross-backend equivalence — and what is
*not* covered is in [`TESTING.md`](TESTING.md). Benchmarks against SQLite,
`sqlite-vec`, DuckDB, pgvector, Meilisearch, MySQL and PostgreSQL, including
the ones we lose, are in [`bench/README.md`](bench/README.md) and
[`BENCHMARK.md`](BENCHMARK.md); every number in either one regenerates from
`./bench/run.sh` or `./bench/compare.sh`.

## Layout

```
crates/
  inlaysql-core/    SQL + planner + executor + storage + retrieval  (no_std)
  inlaysql/         file-backed Device, Database and AsyncDatabase  (std)
  inlaysql-uring/   io_uring Device backend  (Linux)
  inlaysql-mcp/     MCP server mode and the `inlaysql` CLI
  inlaysql-server/  MySQL wire-protocol server mode, depends on inlaysql alone
  inlaysql-wasm/    the engine compiled to WebAssembly
    www/            the browser demo, published to GitHub Pages
    edge/           a Cloudflare Worker, smoke-tested on workerd in CI
    browser/        Playwright harness that drives www/ in headless Chromium
  inlaysql-bench/   benchmark harness, incl. the SQLite comparison
fuzz/               cargo-fuzz targets
bench/run.sh        reproducible benchmark run (SQLite, sqlite-vec)
bench/compare.sh    the same, against DuckDB, pgvector, Meilisearch, MySQL and PostgreSQL in containers
```

`inlaysql-core` is where the database actually lives. It is `no_std`, so it
**cannot** open a file, read the clock or start a thread even by accident —
everything it needs arrives through the traits in `inlaysql_core::traits`
(`Storage`, `FullTextIndex`, `VectorIndex`, `Clock`, `Rng`).

Stage 2 built the storage engine inside `inlaysql-core`: a copy-on-write B+ tree
(`btree`) with a write-ahead log (`wal`) that survives crashes, torn writes and
reordered syncs, recovered deterministically under the fault-injecting
simulation harness (`sim`), plus MVCC: snapshot reads and optimistic concurrent
writers with first-committer-wins. Native writers reserve commit order briefly,
append to four WAL regions and perform their durability syncs in parallel;
stale disjoint-key transactions rebase, while a real overlapping write is
reported as `Error::Conflict`. Stage 4 moved both retrieval indexes into the
engine: an in-engine HNSW ANN index (`hnsw`) and an Okapi BM25 full-text index
(`bm25`) replace the borrowed `instant-distance` and `tantivy` crates, and both
are written into the database file so opening it does not have to re-read every
row. See [`docs/recovery.md`](docs/recovery.md) for the crash-recovery protocol
and [`docs/indexes.md`](docs/indexes.md) for how a saved index stays honest
about what it describes.
`redb` remains behind the traits for comparison and benchmarks.

That is not a style preference. It is what makes deterministic simulation
testing possible: `inlaysql_core::mem` provides a complete in-memory
environment — `BTreeMap` storage, a reference BM25 implementation, brute-force
nearest neighbours, a logical clock and a seeded PRNG — so an entire workload
replays byte for byte on any machine. The multi-writer sweep drives all four
WAL regions through crash/torn-write schedules and checks that recovery is
always one committed interleaving.

```rust
let mut engine = inlaysql_core::mem::engine()?;   // no files, no clock, no threads
```

CI enforces the boundary: it fails if `#![no_std]` disappears from core or if an
OS-facing crate turns up in its dependency tree.

## Performance

```sh
./bench/run.sh                  # points, indexed, joins, vectors, quantisation, retrieval (pinned params)
./bench/compare.sh              # DuckDB, pgvector, Meilisearch, MySQL, PostgreSQL (needs Docker)
REPEATS=5 ./bench/repeat.sh     # run.sh five times: median plus how far the runs disagreed
```

Every number below is [`BENCHMARK.md`](BENCHMARK.md), regenerated at commit
`4f8e5dd` (2026-09-02) on a developer machine, most of it a median of three
runs. One
developer machine — reproduce it, do not trust it. Not every table below is
from the same sitting: `BENCHMARK.md`'s own provenance header says exactly
which tables were regenerated this edition and which are carried forward from
an earlier commit with their own date stated, and this summary follows that,
not a single uniform run. See [`bench/README.md`](bench/README.md) for how
each comparison is kept fair: matched schema, prepared statements on both
sides, matched durability (`fullfsync` on macOS, which is what makes these
numbers mean anything at all), and each engine's own query plan checked
rather than assumed.

**`BENCHMARK.md` measured this harness's own noise floor and it is not
small**: repeating the identical binary against identical data moves these
figures by a median 4.0-7.3% (worse under real desktop load) and roughly a
third of the metrics in the main suite disagree by 10% or more run to run —
and two full regenerations four hours apart on the same day moved several
rows by more than either sitting's own spread, on unchanged code. The
multiples below are rounded to what that floor supports (`~2-4x`, not
`3.26x`) — see `BENCHMARK.md`'s opening note and its §4 reference into
`PERF.md` before quoting any of these to more digits than that.

### Against SQLite

SQLite is measured two ways, because they are two different promises:
`journal` + `synchronous=FULL` + `fullfsync` is the durability InlaySQL
always gives; WAL + `synchronous=NORMAL` is SQLite at its fastest, and the
harder target.

| Workload | InlaySQL | SQLite, durable | SQLite, fastest |
| --- | --- | --- | --- |
| Point read by primary key | **1,069,233 ops/s**, 0.54 µs p50 | 277,517 ops/s (**~2.5-4x**) | 1,153,879 ops/s (0.93x on ops/s; our p50 is below its 0.83 µs) |
| Point read, secondary index | **398,866 ops/s**, 2.21 µs p50 | 272,115 ops/s (**~1.5x**) | 666,800 ops/s (we lose ~1.7x) |
| Indexed range scan, 50 rows | 66,798 ops/s, 13.67 µs p50 | 134,439 ops/s (we lose ~2x) | 182,454 ops/s (we lose ~2.7x) |
| Join, PK inner, full scan | **3.56 ms p50** | 11.03 ms p50 (we win ~3x) | — |
| Join, secondary-index inner, full scan | **3.78 ms p50** | 32.60 ms p50 (we win ~8x) | — |
| Durable write, one commit each | **256 ops/s**, 3.88 ms p50 | 92 ops/s (**~2.8x**) | — |
| Concurrent durable writers, 8 threads | **1,347 commits/s**, 0.0% aborted | 92 commits/s (**~14-15x**) | — |

A single indexed point probe wins — the index itself is worth roughly 500x
over the engine's own unindexed scan. **Iterating rows is where we lose**:
the 50-row range scan is behind both SQLite configurations (roughly 2x and
2.7x), and the `LIMIT 10` form of both join shapes stays roughly 1.7-1.9x
behind (down from 2.0-2.1x at `2eeced7`, 2.2-3.5x after the raw-leaf cache
in `e4086ad`, and 4.7–5.8x before that), which is what pins the remaining
cost as per-row rather than per-query. Both *full* joins now win, and the
PK-inner one by a lot more than it did: a cost-based join reorder (AHL-512)
landed with its cost model backwards, the morning's regeneration caught the
secondary-index join at 3.8x its published figure and withheld the table,
and the fix (AHL-524) lands both shapes on the same plan at ~3.6-3.8 ms —
`BENCHMARK.md`'s joins section tells it in full. Every multiple in this
paragraph is stated to the precision `BENCHMARK.md`'s own measured
run-to-run spread supports, not to three digits — see that file's opening
note. The `LIMIT` shapes are the top open performance target —
[`PERF.md`](PERF.md) has the profile, and index selection stops at the
narrow rule in [What this is not](#what-this-is-not).

The point-read row has now been published at 636,980, then 342,747, then
901,158, then 522,562, then 533,943, and now 1,069,233 ops/s across six
editions. This time part of the move has a name: AHL-527 stopped the point
read allocating for its own bookkeeping and measured 1.23x interleaved,
8 of 8 repetitions; the rest of the 2.0x is unattributed, and SQLite's own
durable row moved 63% between the same two sittings on code that did not
change. Read the ratio against the durable configuration, not the absolute
figure — and read that ratio loosely too: the three individual runs behind
this edition's median disagreed with each other by 1.5x on a machine that
passed the load gate throughout, and a same-binary A/A test on this exact
metric alone (`PERF.md` §4) found a 20.4% max-min spread on a quiet
machine. `BENCHMARK.md` walks through why.

The point-read win is the page cache (AHL-420): caching decoded pages took
warm p50 from 6.75 µs to roughly 1 µs, and AHL-527 to roughly 0.5 µs — past
SQLite's *durable* configuration above and, on p50, past WAL mode with
`synchronous=NORMAL`, the fastest reading configuration SQLite has (0.54 µs
against 0.83 µs at the median; on throughput we are still 0.93x of it,
because our tail is longer). The cache needs no invalidation protocol because the tree is
copy-on-write and (until recently) never reused a page id; a free list that
reuses ids now exists inside the engine (AHL-481), versioning the cache the
way `crates/inlaysql-core/src/btree/cache.rs` warns it must, but it sits
behind a handle-level opt-in that nothing in the public API turns on yet. The
caveat that keeps the point-read row a *warm* number: our *miss* path — a
`pread` plus a decode — is still dearer than SQLite's, so a cold handle warms
up more slowly.

Durable writes win because we pay one `fsync` per commit against the
journal's several; batching the same workload into one commit per many rows
reaches 57,131 ops/s at 11.46 µs (**~220x**) — a bulk-load number, not the
transaction one above. Concurrent writers scale well past eight now: the
adaptive commit-coalesce window (94d96a6) lets 8 writers do roughly 5.5x the
work of one (three sessions have put the 8-writer figure at 1,209, 1,148 and
1,347 commits/s with that code unchanged, so read it as roughly 1,200 ±10%,
not the point value). Eight is not the peak, though — the fuller sweep in
`BENCHMARK.md` (carried forward from 2026-08-30; this edition re-ran only
1/2/4/8 writers) finds it at 16 writers (1,616 commits/s) — and past the peak
throughput falls (1,307 commits/s at 24 writers, 974 at 32) because every
writer's whole commit prepare phase, not only its `fsync`, still serializes
behind one gate — `BENCHMARK.md` has the fuller sweep, including the p99 tail
latency, and `PERF.md` the profile.

### Against `sqlite-vec`, DuckDB and pgvector

2,000 vectors, dim 384, 100 queries, top-10, recall measured against an
exhaustive oracle:

| Corpus | recall@10 | InlaySQL p50 | vs `sqlite-vec` |
| --- | --- | --- | --- |
| Text-derived embeddings | 1.000 | 75.17 µs | **~9x faster at 100% of its recall** |
| Uniform random | 0.922 | 106.88 µs | ~6-7x faster at 92.2% of its recall |

Both corpus shapes are published because only one of them flatters us:
uniformly random vectors in 384 dimensions have no structure for a graph
index to navigate, so recall falls and no tuning fixes it — text-derived
embeddings are what an application actually stores. `VECTOR(n, INT8)`
quantisation costs 0.014 recall on the realistic corpus for a 3.96x smaller
resident vector payload.

Hybrid retrieval (vector + BM25, fused in one SQL statement) at 2,000
documents, `LIMIT 10`: ingest 15,961 docs/s, vector p50 74.67 µs, **BM25 p50
50.50 µs**, **hybrid p50 102.79 µs**. BM25 was 347.50 µs and hybrid 453.88 µs
two commits ago: the full-text index stopped being a map of maps, top-`k`
became a bounded heap instead of scoring and sorting the whole corpus to keep
ten rows, and a MaxScore walk now skips documents whose entire possible score
cannot reach the `k`-th best found so far. Scores are unchanged bit for bit and
ranking is unchanged including ties. BM25 used to be 79% of the hybrid p50; it
is now 50%, and the vector leg is the larger half.

Against DuckDB, pgvector and Meilisearch, one corpus and one exhaustive
ground truth shared by all four engines — see
[`bench/README.md`](bench/README.md#benchcomparesh--duckdb-pgvector-meilisearch-mysql-and-postgresql)
for the full methodology. 5,000 documents, dim 128, 100 queries, top-10:

| Engine | recall@10 | vector p50 | hybrid p50 |
| --- | --- | --- | --- |
| InlaySQL (HNSW + BM25) | 1.000 | **135.00 µs** | **198.00 µs** |
| DuckDB (vss HNSW + `fts`) | 0.992 | 3.98 ms | 11.12 ms |
| Meilisearch (`arroy` ANN + its own ranking) | 0.996 | 1.17 ms | 3.97 ms |
| pgvector (HNSW + `ts_rank`) | 0.987 | 147.00 µs | 13.40 ms |

**Hybrid is roughly 20x** the nearest baseline now that Meilisearch, a
dedicated search engine, is in the comparison, and roughly 55-70x
DuckDB/pgvector — because it is one statement here and two queries plus
client-side rank fusion there (Meilisearch's own hybrid mode included: it is
deliberately not used, so every engine in the table is fused the same way),
not a comparison of equal work either way, and `bench/README.md` says so.
This table is a single run with no repeat wrapper (`bench/compare.sh` has
none), so its own multiples carry no measured spread of their own — read
them as less precise than the SQLite table above, not more. Vector-only
stays ahead of pgvector: 135 µs against 147 µs, both paying pgvector's socket
round trip a library in your process does not; Meilisearch's 1.17 ms is
doing more per query (its own ranking pipeline runs alongside the ANN
search), so read that gap as two different products, not a rout.

Recall on uniformly random vectors is a structural, not a tuning, problem: on
text-derived embeddings recall@10 stays flat across a 20x range of corpus
sizes (0.998 at 5,000 rows, 1.000 at 20,000, 0.998 at 100,000); on uniformly
random unit vectors in 384 dimensions it falls to 0.12 at a hundred thousand,
because every pairwise distance in that corpus concentrates to within about a
percent of the rest — there is no downhill for a graph to walk. Both numbers
are in `bench/README.md`, reproduced with `SUITE=vectors ./bench/run.sh`.

### Against MySQL and PostgreSQL

Reads win by a wide margin; sequential writes lose to both. InlaySQL is
measured twice — on the host with a real `F_FULLFSYNC` barrier, and inside a
container on the same volume class as the servers, so all three pay the same
virtualised fsync. **This table is carried forward from commit `b4798ce`
(2026-08-30), not regenerated this edition** — `BENCHMARK.md`'s own section
explains why a fresh single sequential run would be a worse measurement than
reusing the interleaved, repeated, quiet-machine rerun already done for it:

| Engine | write ops/s | read ops/s |
| --- | --- | --- |
| InlaySQL, host (real `F_FULLFSYNC`) | 253.2 | 497,000 |
| InlaySQL, containerised | 849.7 | **678,000** |
| MySQL 8 (`innodb_flush_log_at_trx_commit=1`, binlog off) | **1,184.2** | 9,200 |
| PostgreSQL 17 (`fsync=on`, `synchronous_commit=on`) | **1,612.8** | 19,400 |

**Reads: ~74x MySQL and ~35x PostgreSQL**, containerised — an in-process
library against a socket round trip, an asymmetry that is structural and
stated rather than hidden. **Writes: we lose to both** — PostgreSQL by 1.90x
and MySQL by 1.39x. `BENCHMARK.md` carries an extensive correction on this
table: the transport asymmetry above (no socket round trip) is worth roughly
as much as the entire published PostgreSQL gap on its own, and an interleaved,
repeated, quiet-machine rerun of the same comparison found closely matching
multiples (1.81x/1.43x), so read the ranking and the rough size of the gap as
real, not as an artifact of a busy machine. What is structural regardless:
this workload is one commit at a time on one connection, so group commit
cannot fire by design, and what is left is per-commit cost against InnoDB's
own redo write.

Every row above measures InlaySQL as a *library* against two servers, so the
reads win partly by paying no socket round trip. **Server to server, over the
wire, that advantage is smaller and still real** (AHL-489). Also carried
forward from `BENCHMARK.md` (process-based driver, 2026-08-29, `f8e29e9`) —
[`inlaysql serve --mysql`](#speaking-mysql-over-the-wire) reached over a
compose network by `mysql.connector`, against MySQL 8 on the same driver and
the same transport, each connection a spawned OS process rather than a Python
thread so the client's own GIL cannot contaminate the comparison:

| Engine | Connections | write ops/s | read ops/s |
| --- | --- | --- | --- |
| InlaySQL, `serve --mysql` | 1 | 556.7 | **9,033.3** |
| InlaySQL, `serve --mysql` | 8 | 1,255.5 | 6,294.3 |
| MySQL 8 | 1 | 787.7 | 7,400.6 |
| MySQL 8 | 8 | **3,092.7** | 7,931.1 |

Reads edge it at one connection (1.22x) but fall behind at eight (0.79x) —
and unlike an earlier, thread-based edition of this table, that is not a
client-side GIL artifact: MySQL's own reads stay flat across the same step
(7,400.6 → 7,931.1) while InlaySQL's fall in absolute terms (9,033.3 →
6,294.3), with zero retries on either side. Writes lose at one connection
(0.71x) and badly at eight (0.41x), which is thread-per-connection against a
worker pool — MySQL's own write throughput nearly quadruples from one
connection to eight where InlaySQL's only reaches 2.25x. `BENCHMARK.md`'s
"Server-to-server" section has the fuller investigation into the read drop,
including two checks that ruled out `inlaysql-server`'s own concurrency model
as the more likely cause; `bench/README.md` has the full methodology and the
remaining asymmetries. PostgreSQL has no row because this server speaks only
the MySQL wire protocol.

**Read shapes and batch insert (2026-08-31).** Four workloads that had no
harness on either side until now — and they do not all go our way:

| Shape | InlaySQL | MySQL 8 | PostgreSQL 17 |
| --- | --- | --- | --- |
| Indexed range scan, 50 rows | **49,259 ops/s** | 13,124 ops/s (**~3.7x**) | 21,455 ops/s (**~2.3x**) |
| Join, secondary-index inner, full | **4.77 ms** p50 | 15.01 ms (**~3.1x**) | 10.49 ms (**~2.2x**) |
| Join, PK inner, full | 13.04 ms p50 | 15.00 ms (tie) | 10.49 ms (we lose ~1.24x) |
| `GROUP BY n`, 100 groups | 29/s | 98/s (we lose ~3.4x) | 147/s (we lose ~5.0x) |
| Batch insert, 100 rows/statement | 26,254 rows/s | 42,933 rows/s (we lose ~1.6x) | 81,229 rows/s (we lose ~3.1x) |

The range scan we lose to SQLite is a shape we *win* against both servers, so
"our row iteration is slow" is a statement about SQLite specifically, not
about every engine. **The aggregate row is the worst multiple we publish
against anyone**: 3.4-6.0x slower than both, consistent in sign across every
repetition, and not a transport artifact — both opponents stream their result
rows over a socket while InlaySQL is in-process. That is the tuple-at-a-time
executor priced honestly, and the batch execution path that would fix it is
scoped but unbuilt. This sitting ran under deliberate desktop load with the
quiet-machine gate overridden (it refused every clean attempt), both sides of
every cell measured in the same sitting, and `SCOREBOARD.md` applies the wider
20.2% desktop-load noise floor to every verdict above.

What none of this proves: Docker Desktop's virtual disk was never
independently verified to honour `fsync` as a barrier for any of the engines
measured in a container. Also not measured anywhere here: sustained or
multi-core saturation, and cold-cache reads — the point-read rows throughout
this section are warm, and an application that opens a handle, reads a handful
of rows and exits sees something weaker, because our miss path is dearer than
SQLite's.

## Next

Roughly in order of value. Every line here is a gap already named in
[What this is not](#what-this-is-not) or in the benchmarks above — nothing
below is a surprise to the project, it is the honest state of it:

1. **The join and range miss path** — the last measured read loss to
   SQLite, and still the biggest one. AHL-479 found the entry-range walk
   itself was not the bottleneck — reading admitted entries without cloning
   their keys moved the indexed-range case +15–18%, but a full join barely
   moved (~4.5%), because the join workload's table plus index (~18 MiB)
   exceeds the default 8 MiB page cache. [`PERF.md`](PERF.md) has the
   profile and the cache-budget arithmetic that settled it, plus two
   rejected attempts at the obvious fix (page/cell representation, AHL-493)
   and why each traded the point-read win or a small-join regression for a
   cold-path gain nobody asked for. A later, narrower fix — a probed join's
   inner row was being cloned twice, once by the probe and again by the
   pairing loop — landed a small, safe (zero risk to the point-read path by
   construction), consistent-but-modest ~3–5% win on both full-join shapes,
   measured on a quiet machine; it is not what closes this gap. A third
   attempt — prefix-skipping key comparison during descent (`memcmp` is
   12–21% of the miss path's self-time) — was also tried and was also a
   wash: the mechanism measurably works, but its own bookkeeping cost erases
   the gain, because this workload's dominant cost is re-descending from the
   root once per outer row rather than comparing many entries per descent.
   See [`PERF.md`](PERF.md) for the numbers and the next, different angle:
   extending the point-read path's already-proven retained-cursor technique
   to the entry-range walk itself, to attack the re-descend cost directly.
2. ~~**The server's per-connection page cache.**~~ — **investigated twice,
   and the diagnosis behind it did not hold up either time.** The
   1-to-8-connection read drop this item used to cite (26,271 → 17,628, an
   even earlier edition) could not be reproduced on a quiet machine with the
   same client and driver; what did reproduce, independently, twice: the
   Python MySQL client's *threaded* concurrency is GIL-bound, and that alone
   explained a comparable-looking drop with nothing server-side involved. A
   process-based driver now exists (2026-08-29) and closes that question for
   good — but it finds a real, smaller drop of its own (9,033.3 → 6,294.3
   reads/s, table above), where MySQL's own reads stay flat across the same
   step. Two checks ruled out `inlaysql-server`'s thread-per-connection model
   as the more likely cause (it scales *up* cleanly both on the host and
   inside the compose network at matching and larger workload shapes); what
   does reproduce it is running the MySQL driver immediately before the
   server-to-server one, in this benchmark's own generation order — not
   root-caused past that. See `BENCHMARK.md`'s "Server-to-server" section and
   `PLAN.md`'s W5 for what is still open.
3. ~~**The sequential-commit gap to MySQL.**~~ — as a library the write gap is
   1.39x containerised against MySQL and 1.90x against PostgreSQL; over the
   wire (server-to-server, process-based) it is 1.41x at one connection and
   2.46x at eight, thread-per-connection against a worker pool, and group
   commit cannot fire on a single connection by design. This remains open.
   `BENCHMARK.md`'s correction on the library figure is worth reading before
   trusting the size of that gap: the transport asymmetry (no socket round
   trip) that flatters the library rows is worth roughly as much as the
   entire published PostgreSQL gap on its own.
4. ~~**Wiring the free list into the public API.**~~ — **done.** The free
   list and page reuse landed inside the engine (AHL-481); `EngineOptions::page_reuse`
   now reaches it, and `inlaysql vacuum <path>` does whole-file compaction —
   a copy into a fresh file and an atomic rename, the same algorithm real
   SQLite's own `VACUUM` uses, so it never touches the copy-on-write tree's
   crash-recovery path at all. See `docs/recovery.md` for what is and is not
   done at the storage layer underneath it.
5. ~~**A cost-based join planner.**~~ — **partially done.** `ANALYZE` records
   table row counts and leading B-tree index cardinalities; given a complete,
   current snapshot the planner costs a choice between the existing
   hash-join and index-probe operators per join, still in written order
   (`docs/research/cost-planner.md`). Missing, corrupt or stale stats fall
   back to the old rule in
   [Scalar indexes and joins that use them](#scalar-indexes-and-joins-that-use-them),
   and join reordering is not implemented — that is what the join losses in
   [Performance](#performance) whose fix needs a different physical order are
   still waiting on.
6. **A server-to-server benchmark with a corrected driver, on a quiet
   machine.** The first server-to-server table exists (AHL-495, in
   [Performance](#performance)), but item 2 above found its own driver
   (threaded Python client concurrency) is a confound serious enough that a
   repeat needs a process-based driver before the number can be trusted,
   not just a quieter machine.
7. ~~**Multi-column and composite retrieval indexes.**~~ — **the BM25 half is
   done; ANN is scoped out, on purpose.** `CREATE INDEX idx ON docs (title,
   body) USING FULLTEXT` now builds one combined BM25 index over the
   concatenation of every named column's text — MySQL's `FULLTEXT(title,
   body)`: a query term that only matches one column still ranks the row.
   `bm25_score(title, body, ?)` finds it regardless of which order the
   columns are named in, and a bare `CREATE INDEX idx ON docs (title, body)`
   (no `USING`) still means a B-tree, exactly as it always has — inferring
   `FullText` for it the way a single `TEXT` column already does would have
   silently changed that long-standing default. This needed no on-disk
   format change of its own: the multi-column column-list encoding the
   scalar B-tree index already forces (`Catalog::required_version`) was
   never B-tree-specific, and a single-column retrieval index's persisted key
   (`index:<table>:<column>`) is untouched — a multi-column index's key is
   additive, built so it can never collide with it. `VECTOR` stays
   single-column: two embedding columns are generally two different vector
   spaces, and there is no standard meaning for one HNSW graph over both —
   concatenated or weighted-sum embeddings are technically possible but not a
   default anyone should get without asking for it by name — so this was
   left undone rather than guessed at.
8. ~~**Filter-aware graph walks.**~~ — **done.** A restrictive `WHERE` on a
   retrieval query is now pushed into the index walk rather than answered by
   over-fetching: rejected rows are traversed but not returned or counted,
   for the vector and BM25 indexes alike and on both sides of `fuse`.
   Per-value sub-indexes — a further speedup on top of pushdown, for a
   filter selective enough that even a single filtered walk is more work
   than probing a per-value structure directly — remain later-stage work.
9. ~~**Quantised paged index nodes.**~~ — **done.** `PagedHnswIndex` stored
   exact `f32` even for an `INT8` column; it now shares the same
   `Q8Vector`/`VectorEncoding` quantisation the in-memory index already had.
   Measured: 2.14–2.16x smaller file, 3.96x smaller resident cache payload
   (the same ratio the in-memory index publishes) — the file-size win is
   larger than the in-memory index's 1.65x because every paged node stores
   its vector inline, where the in-memory index recomputes a live node's
   vector from its own embeddings map instead.
10. **Deeper SQL Logic Test coverage, real SQLancer runs and continuous
    fuzzing** beyond what `trust.yml` runs today (see
    [`docs/sqlancer.md`](docs/sqlancer.md)).
11. **Read replicas over the existing CDC log.** `cdc.rs` is already
    pull-based and bounded, so the shape of the work is shipping records and
    tracking replica position — the Turso model, no consensus and no fork.
    The blocker to design around first: `open_read_only` takes no OS lock by
    design, so a reader in another process cannot be proven absent — fine
    on one machine today, unavoidable to answer once a second machine is
    reading the same file. Durable storage/compute separation (an
    object-storage-backed device, for corpora too large to ship as an edge
    asset) is the same category of work, longer.

Full Postgres parity is deliberately not on this list — see the last point in
[What this is not](#what-this-is-not). Window functions and `WITH RECURSIVE`
were in this paragraph until AHL-494 and semi-naive iteration implemented
them, respectively.

## What this is not

Explicit non-goals for the current stage, all of them scheduled work rather
than oversights.

If the question is specifically "could our organisation run this in
production?", [`docs/enterprise-readiness.md`](docs/enterprise-readiness.md)
answers it directly: the gaps that would stop a deployment, ranked by
deployment risk, each one citing the code it is about and marked with whether
it was verified in this repository or reported by an audit and not yet
reproduced. It is a less flattering document than this section and a more
useful one.

- **Retrieval indexes are explicit and single-column.** A `TEXT` column is
  only full-text indexed after `CREATE INDEX idx ON t (body)` (or in a
  database written before `CREATE INDEX` existed, whose columns are
  grandfathered); the same for a `VECTOR` column and an ANN index. Composite
  and multi-column *retrieval* indexes are not supported (see
  [Next](#next)). A scalar index is a different structure: `CREATE INDEX` on
  `INTEGER`/`REAL`/`TEXT` (`USING BTREE` on the last) is a real ordered
  B-tree, may be declared `UNIQUE`, and may span more than one column — see
  [Scalar indexes and joins that use them](#scalar-indexes-and-joins-that-use-them).
- **Join order is still written order; only the operator choice is now
  costed.** `ANALYZE` records row counts and leading-index cardinalities, and
  a complete, current snapshot lets the planner choose between the existing
  hash-join and index-probe operators for each join
  (`docs/research/cost-planner.md`); missing or stale stats fall back to the
  narrow rule that already existed: a retrieval expression is answered by its
  index, a top-level equality on `INTEGER PRIMARY KEY` or a scalar-indexed
  column by a tree descent or range probe — including as the inner side of a
  join (AHL-464) — a full-scan equi-join by a hash build, and everything else
  by a full scan. A two-table inner join may run with its tables exchanged
  when statistics say the other side should drive (AHL-512/524/525); a
  `LIMIT` without an `ORDER BY` never is. [Performance](#performance)
  publishes both full-join shapes and the `LIMIT` shapes, wins and losses.
- **Recall on uniformly random vectors is poor, and cannot be fixed by
  tuning.** On text-derived embeddings recall@10 stays flat across a 20x
  range of corpus sizes tested (0.998 at 5,000 rows, 1.000 at 20,000, 0.998
  at 100,000); on uniformly random unit vectors in 384 dimensions it is 0.12
  at a hundred thousand. That is not a defect in the index — distances in
  that corpus concentrate to within about a percent of each other, so there
  is no structure for a graph to navigate, and holding recall fixed there
  costs an `ef_search` that grows with the corpus. `bench/README.md`
  measures both and explains the difference; the first is what an
  application sees.
- **Filtered retrieval is pushed into the index walk.** A `WHERE` on a
  retrieval query is compiled into a row predicate and pushed into the
  retriever itself: a row the filter rejects is excluded from the result set
  and from the candidate budget but is still traversed, so its neighbours
  stay reachable and a selective filter cannot sever the graph (the classic
  filtered-ANN connectivity trap). The walk keeps going until enough rows
  pass or the index is genuinely exhausted, so a filter too selective for any
  bounded probe degrades to scanning every row the index can rank — correct,
  at the cost of the full walk — rather than to a partial answer. The
  unfiltered path is untouched: passing no filter is exactly the old search,
  behaviour and cost included.
- **Vector quantisation is explicit per column.** `VECTOR(n)` remains exact;
  `VECTOR(n, INT8)` reduces row and HNSW vector payloads by about 4x using a
  symmetric per-vector scale. Queries stay `f32`, and the vectors benchmark
  publishes recall, file size and resident vector bytes for both corpus shapes.
- **The in-memory ANN index is still the default, and holds the whole corpus.**
  `Database::open` uses `HnswIndex`, which keeps every embedding and its
  normalised copy in RAM — roughly twice the corpus bytes for exact columns or
  half the original `f32` corpus bytes for int8 columns, before the graph.
  `Database::open_paged` opens `inlaysql_core::hnsw_paged::PagedHnswIndex`
  instead: the graph is stored as ordinary rows in the same database file and
  read through a bounded LRU cache, so the resident working set is the cache
  rather than the corpus. It writes through the engine's own transaction, so the
  graph and the rows it describes reach the log together, it carries the write
  version it describes and is rebuilt rather than trusted if that stamp goes
  stale, and it goes through the same fault-injection sweep as everything else
  (`crates/inlaysql/tests/index_recovery_dst.rs`). It is not the default because
  the trade is real: opening is instant where the in-memory index rebuilds, but
  every cache miss during a search is a read from the file. The file format is
  the same either way, so one database can be opened both ways.
  `bench/README.md` reports the measured memory bound.
- **The in-memory BM25 index is still the default, and holds the whole
  corpus too.** `Bm25Index` keeps the term dictionary, every postings list and
  a per-document term list in RAM — measured at ~1,800 bytes per document once
  the dictionary saturates, so ten million documents is ~17 GiB per connection
  (`crates/inlaysql/tests/index_memory_cost.rs`).
  `EngineOptions::paged_text_indexes` opens
  `inlaysql_core::bm25_paged::PagedBm25Index` instead, which puts all three in
  the file and reads them through a bounded cache, on the same protocol as the
  paged ANN index: written inside the engine's transaction, stamped with the
  write version it describes, rebuilt rather than trusted when that stamp goes
  stale. **The scores are identical to the in-memory backend, bit for bit**,
  which is the hard part rather than a detail — BM25's `idf` and length
  normalisation are corpus-relative, so a backend whose statistics differ in
  the last place silently reranks. It is asserted against a freshly built
  index over six corpus shapes, and again through the whole SQL path. It is not
  the default because the trade is real and it is not the ANN one: writes cost
  a page per distinct term of the document, so a bulk load grows the file by
  hundreds of kilobytes per document. `docs/indexes.md` has the layout and the
  full cost; `inlaysql serve --mysql --paged-text` is the server flag for it,
  documented in `docs/server.md` alongside `--paged-vectors`.
- **A paged index stores exact `f32` vectors even for an int8 column.**
  `VECTOR(n, INT8)` shrinks the row and the in-memory graph; the paged graph's
  node records do not quantise yet, so on an int8 column the paged index trades
  away the 4x the quantisation was for. Results are unaffected — queries are
  `f32` on both paths.
- **No clustering or multi-node replication.** InlaySQL runs in one process
  against one file — no leader election, no consensus, no built-in read
  replica. This is not the same gap as serverless: [On an edge
  runtime](#on-an-edge-runtime) is delivered today and does not need any of
  the above, because a retrieval index is built once, shipped as a static
  asset, and answered from the isolate that took the request — there is no
  node to be a replica of. Multi-node deployment (read replicas over the
  existing CDC log; durable storage/compute separation for corpora too large
  to ship as an asset) is later-stage work — see [Next](#next).
- **No point-in-time recovery.** [Online backup](#online-backup) takes a full
  consistent copy of a live database, which is a different thing: the states
  you can restore to are the ones you took a copy at, not any instant in
  between. Rolling forward from one needs a log carrying row payloads, and the
  CDC log deliberately carries none — see
  [`docs/enterprise-readiness.md`](docs/enterprise-readiness.md). Incremental
  backup is not implemented either.
- **Full Postgres parity is not a goal**, now or later.

## Licence

**Dual licensed: AGPLv3, or commercial.**

- **[GNU AGPL v3.0](LICENSE)** — free of charge, on the AGPL's terms. Note
  section 13: if users reach a modified version *over a network*, you owe those
  users the corresponding source. For an embedded database that is the clause
  worth reading before you adopt it.
- **[Commercial licence](LICENSE-COMMERCIAL.md)** — removes those obligations.
  Contact info@solutionforest.net.

[`LICENSE-COMMERCIAL.md`](LICENSE-COMMERCIAL.md) has a plain-language guide to
which one you need, and what we ask of contributors so that dual licensing
remains possible.
