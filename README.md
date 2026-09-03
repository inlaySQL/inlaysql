<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="logo/InlaySQL_Logo-horizontal-dark.svg">
    <img alt="InlaySQL" src="logo/InlaySQL_Logo-horizontal.svg" width="420">
  </picture>
</div>

<p align="center">
  <a href="https://github.com/inlaySQL/inlaysql/actions/workflows/ci.yml"><img src="https://github.com/inlaySQL/inlaysql/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/inlaySQL/inlaysql/actions/workflows/wasm.yml"><img src="https://github.com/inlaySQL/inlaysql/actions/workflows/wasm.yml/badge.svg?branch=main" alt="WASM"></a>
  <a href="https://github.com/inlaySQL/inlaysql/releases"><img src="https://img.shields.io/badge/version-0.0.1--experimental-orange" alt="experimental"></a>
  <a href="https://github.com/inlaySQL/inlaysql/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-AGPLv3--or--commercial-blue" alt="license"></a>
</p>

<!-- Both CI badges are pinned to `main`, because a badge that reports whatever
     ran most recently on any branch is not reporting anything. `trust.yml` has
     no badge on purpose: it is allowed to go red when the fuzzer finds
     something, and its output is the artifacts, not a colour. -->

## InlaySQL

InlaySQL is an embedded, serverless SQL database in Rust: **SQLite's model —
one file, no server, plain SQL — with MVCC concurrent writers, and vector and
full-text search as first-class parts of the SQL dialect** rather than
extensions bolted on the side.

It runs as a [Rust crate](#using-it) (`#![forbid(unsafe_code)]` outside one
Linux I/O backend), in [the browser as WebAssembly](https://inlaysql.github.io)
(the live demo, plus [framework examples](https://inlaysql.github.io/frameworks/)
for vanilla JS, jQuery, React and Vue), over the [MySQL wire
protocol](docs/server.md) so existing ORMs connect as-is, and as a CLI.

> [!WARNING]
> **Experimental — version 0.0.1, never run in production.** The on-disk
> format is pre-1.0 (the policy is *recreate the database*, not migrate —
> [`docs/recovery.md`](docs/recovery.md)); crash-safety is proven by
> deterministic simulation rather than years of real hardware; and the known
> gaps are listed, not hidden — see [What this is not](#what-this-is-not).
> Use it for experiments, prototypes and anything you can rebuild from source
> data. Found a bug? Please [open an issue](https://github.com/inlaySQL/inlaysql/issues)
> — it is genuinely useful to us. Security issues go to
> [`SECURITY.md`](SECURITY.md), privately.

## Installation

**Rust crate** (not on crates.io yet — the format is pre-1.0):

```toml
[dependencies]
inlaysql = { git = "https://github.com/inlaySQL/inlaysql" }
```

**CLI and MySQL-wire server:**

```sh
git clone https://github.com/inlaySQL/inlaysql
cd inlaysql && cargo build --release -p inlaysql-mcp
target/release/inlaysql serve --mysql app.inlay   # point your ORM at :3306
```

**Browser:** the WASM module is built by `./crates/inlaysql-wasm/build.sh`
from source; the compiled demo and the
[framework examples](https://inlaysql.github.io/frameworks/) run live without
any install.

## The demo

Hybrid retrieval — vector search and BM25 fused — is **one ordinary SQL
statement**:

```sql
SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
FROM docs
ORDER BY score DESC
LIMIT 3;
```

The planner recognises the retrieval functions, turns each into an index
probe, and fuses the two rankings. No separate vector store, no
application-side merge step, no second query language.

```sh
cargo run --example hybrid_search     # end to end, in one example
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

## Documentation

| | |
| --- | --- |
| [Using it](#using-it) | the API: sync, prepared statements, async without a runtime, I/O backends |
| [The SQL surface](#the-sql-surface) | the dialect, including `VECTOR`, `bm25_score`, `vector_score` and `fuse` |
| [Using it from a framework](crates/inlaysql-wasm/www/frameworks/README.md) | React, Vue, jQuery and plain JS against the WASM build |
| [`docs/server.md`](docs/server.md) | the MySQL wire server: accounts, TLS, limits, translated and refused SQL |
| [Performance](#performance) and [`SCOREBOARD.md`](SCOREBOARD.md) | every benchmark, wins and losses, and the verdict matrix with its fairness audit |
| [What this is not](#what-this-is-not) and [Next](#next) | the gaps, listed rather than hidden, and the build order |
| [`docs/architecture.md`](docs/architecture.md) | the load-bearing design decisions and what each rules out |

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

The cells in that slice are still owned `Value`s, so a `TEXT` column is a
`String` allocated and freed per row. `query_prepared_each_ref` hands the
callback borrowed cells instead — a `ValueRef::Text` is a `&str` into the page
the row was decoded from — so a consumer that only reads allocates nothing at
all:

```rust
let scan = db.prepare("SELECT id, body FROM docs WHERE id >= ?")?;
let mut bytes = 0;
let count = db.query_prepared_each_ref(&scan, &[Value::Integer(1)], |row| {
    bytes += row[1].as_str().map_or(0, str::len);
    Ok(())
})?;
```

`to_owned_value()` is the explicit copy, for the columns you do want to keep.
One stored table with `WHERE`, `LIMIT` and `OFFSET`, projected as bare columns,
runs a pipeline that allocates nothing per row; `ORDER BY`, `GROUP BY`,
`DISTINCT`, windows, joins and projections holding an expression all fall back
to building the row and borrowing out of it, because none of them can emit a
row before it has seen the whole input. The answer is identical either way.

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
probe instead of a full scan — worth roughly 500x on point probes and
roughly 150x on range scans over the engine's own unindexed scan
(`BENCHMARK.md`) — and `CREATE UNIQUE INDEX` enforces a
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

Every number below is [`BENCHMARK.md`](BENCHMARK.md): the SQLite and
`sqlite-vec` tables regenerated at commit `1f7921a` (2026-09-03) as gated
medians of three, and — for the first time gated and repeated — every
cross-engine table (DuckDB, pgvector, Meilisearch, MySQL **8.4**,
PostgreSQL 17) at `bdc64eb` the night before, medians of three or five.
One developer machine — reproduce it, do not trust it. Not every table
below is from the same sitting: `BENCHMARK.md`'s own provenance header
says exactly which tables come from which commit and which are still
carried forward from an earlier one with their own date stated, and this
summary follows that, not a single uniform run. The MySQL container moved
from 8.0.x to 8.4 (LTS) on 2026-09-02, so every MySQL figure below is
against 8.4 and no comparison with an earlier edition's MySQL number is a
comparison of engines. See [`bench/README.md`](bench/README.md) for how
each comparison is kept fair: matched schema, prepared statements on both
sides, matched durability (`fullfsync` on macOS, which is what makes these
numbers mean anything at all), and each engine's own query plan checked
rather than assumed.

**`BENCHMARK.md` measured this harness's own noise floor and it is not
small**: repeating the identical binary against identical data moves these
figures by a median 4.0-7.3% (worse under real desktop load) and roughly a
third of the metrics in the main suite disagree by 10% or more run to run —
and three full regenerations on the same day moved several rows by more
than any sitting's own spread, on unchanged code. The
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
| Point read by primary key | **692,893 ops/s**, 0.625 µs p50 | 177,098 ops/s (**~3-6.5x**) | 1,233,552 ops/s (0.56x on ops/s; our p50 is below its 0.750 µs) |
| Point read, secondary index | **481,918 ops/s**, 1.83 µs p50 | 264,677 ops/s (**~1.8x**) | 765,189 ops/s (we lose ~1.6x) |
| Indexed range scan, 50 rows | 119,219 ops/s, 8.25 µs p50 | 143,954 ops/s (we lose ~1.2x) | 237,624 ops/s (we lose ~2x) |
| Join, PK inner, full scan | **3.25 ms p50** | 10.33 ms p50 (we win ~3x) | — |
| Join, secondary-index inner, full scan | **3.63 ms p50** | 30.76 ms p50 (we win ~7-8x) | — |
| Durable write, one commit each | **248 ops/s**, 3.91 ms p50 | 99 ops/s (**~2.5x**) | — |
| Concurrent durable writers, 8 threads | **1,184 commits/s**, 0.0% aborted | 89 commits/s (**~13x**) | — |

Every row is the same harness as the previous edition's, so every move
since `3cf0d85` is either noise or attributed, and `BENCHMARK.md` says
which per row. (The first three rows' loop changed one edition earlier,
AHL-535: InlaySQL's side steps each row through the borrowing
`query_prepared_each_ref`, SQLite's side reads through `row.get_ref(..)`,
and both sides read every selected column of every row — the comparison
got harder for InlaySQL, not easier.)

A single indexed point probe wins — the index itself is worth roughly 400x
over the engine's own unindexed scan, down from 500x because the unindexed
scan got faster too. **Iterating rows is where we still lose, by less
again**: the 50-row range scan is behind both SQLite configurations
(roughly 1.2x and 2x, from 1.5x and 2.5x — AHL-550 compiled the residual
filter once per execution and measured 1.22-1.36x on this shape
interleaved), and the `LIMIT 10` form of the two join shapes is roughly
1.1x and 1.3-1.5x behind (from 1.2-1.3x and 1.5-1.6x at `3cf0d85` —
AHL-549 decodes the probed inner row once, where it is used, and measured
1.16x and 1.05x on the bench's own two shapes — 1.7-1.9x at `4f8e5dd`,
2.0-2.1x at `2eeced7`, 2.2-3.5x after the raw-leaf cache in `e4086ad`, and
4.7–5.8x before that); the PK shape's band now touches SQLite's, so it is
read as a few per cent behind at the floor, not as parity. Both *full*
joins win: a cost-based join reorder (AHL-512) landed with its cost model
backwards, the morning's regeneration caught the secondary-index join at
3.8x its published figure and withheld the table, and the fix (AHL-524)
lands both shapes on the same plan at ~3.2-3.6 ms — `BENCHMARK.md`'s joins
section tells it in full, including that AHL-549's own A/B read the full
shapes 3–10% behind and this gated regeneration read them +1% and +4%,
inside their spreads. Every multiple in this paragraph is stated to the
precision `BENCHMARK.md`'s own measured run-to-run spread supports, not to
three digits — see that file's opening note. The secondary `LIMIT` shape
and the range scan are still the open performance targets —
[`PERF.md`](PERF.md) has the profile (after AHL-549, ten full tree
descents for ten consecutive keys in one leaf are 37% of the `LIMIT` join
on their own), and index selection stops at the narrow rule in
[What this is not](#what-this-is-not).

The point-read row has now been published at 636,980, then 342,747, then
901,158, then 522,562, then 533,943, then 1,069,233, then 872,474, and now
692,893 ops/s across eight editions. This time the median went *down* 21%
on ops/s and *up* 7% on p50 on code that five interleaved A/Bs in
`3cf0d85..1f7921a` (AHL-541, 542, 549a, 549b, 550) each measured flat on
this exact shape; the best run of the last three sittings is the same
figure (1,155,913, 1,153,968, 1,168,645 at 0.50 µs), and what moved is the
tail, which two runs of three carried this time with no load sample to
name either. `BENCHMARK.md` states both instruments and does not pick
between them. Read the ratio against the durable configuration, not the
absolute figure — and read that ratio loosely too: the three individual
runs behind this edition's median disagreed with each other by 2.1x on a
machine that passed the load gate throughout, and a same-binary A/A test
on this exact metric alone (`PERF.md` §4) found a 20.4% max-min spread on
a quiet machine. `BENCHMARK.md` walks through why.

The point-read win is the page cache (AHL-420): caching decoded pages took
warm p50 from 6.75 µs to roughly 1 µs, and AHL-527 and AHL-535 to roughly
0.5-0.6 µs — past SQLite's *durable* configuration above and, on p50, past
WAL mode with `synchronous=NORMAL`, the fastest reading configuration
SQLite has (0.625 µs against 0.750 µs at the median; on throughput we are
0.56x of it, because our tail is longer). The cache needs no invalidation protocol because the tree is
copy-on-write and (until recently) never reused a page id; a free list that
reuses ids now exists inside the engine (AHL-481), versioning the cache the
way `crates/inlaysql-core/src/btree/cache.rs` warns it must, but it sits
behind a handle-level opt-in that nothing in the public API turns on yet. The
caveat that keeps the point-read row a *warm* number: our *miss* path — a
`pread` plus a decode — is still dearer than SQLite's, so a cold handle warms
up more slowly.

Durable writes win because we pay one `fsync` per commit against the
journal's several; batching the same workload into one commit per many rows
reaches 227,261 ops/s at 1.67 µs (**roughly 900x**) — a bulk-load number,
not the transaction one above, and 4x the previous edition's 56,501 because
a transaction's pages now stay decoded until it commits (AHL-542) instead
of being re-encoded after every statement. Concurrent writers scale well
past eight now: the adaptive commit-coalesce window (94d96a6) lets 8
writers do roughly 5x the work of one (five sessions have put the 8-writer
figure at 1,209, 1,148, 1,347, 1,228 and 1,184 commits/s with that code
unchanged, so read it as roughly 1,200 ±10%, not the point value). Eight is not the peak, though — the fuller sweep in
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
| Text-derived embeddings | 1.000 | 68.67 µs | **~9x faster at 100% of its recall** |
| Uniform random | 0.922 | 93.00 µs | ~7x faster at 92.2% of its recall |

Both corpus shapes are published because only one of them flatters us:
uniformly random vectors in 384 dimensions have no structure for a graph
index to navigate, so recall falls and no tuning fixes it — text-derived
embeddings are what an application actually stores. `VECTOR(n, INT8)`
quantisation costs 0.014 recall on the realistic corpus for a 3.96x smaller
resident vector payload.

Hybrid retrieval (vector + BM25, fused in one SQL statement) at 2,000
documents, `LIMIT 10`: ingest 17,527 docs/s, vector p50 74.04 µs, **BM25 p50
49.42 µs**, **hybrid p50 97.83 µs**. BM25 was 347.50 µs and hybrid 453.88 µs
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
| InlaySQL (HNSW + BM25) | 1.000 | **129.00 µs** | **192.00 µs** |
| DuckDB (vss HNSW + `fts`) | 0.993 | 4.01 ms | 11.14 ms |
| Meilisearch (`arroy` ANN + its own ranking) | 0.999 | 1.18 ms | 4.04 ms |
| pgvector (HNSW + `ts_rank`) | 0.987 | 148.00 µs | 13.38 ms |

**Hybrid is roughly 20x** the nearest baseline now that Meilisearch, a
dedicated search engine, is in the comparison, and roughly 60-70x
DuckDB/pgvector — because it is one statement here and two queries plus
client-side rank fusion there (Meilisearch's own hybrid mode included: it is
deliberately not used, so every engine in the table is fused the same way),
not a comparison of equal work either way, and `bench/README.md` says so.
This table is now a gated median of three (`REPEATS=3
./bench/repeat-compare.sh`, 2026-09-02/03), and the first thing the repeat
measured is that the baselines hold within 0-4% run to run while
InlaySQL's own two cells swing 23-36% (88-134 µs and 156-196 µs) — the
medians published are the slower two of three. Vector-only against
pgvector is a tie, not a win: 129 µs against 148 µs, both paying
pgvector's socket round trip a library in your process does not, on a row
whose own spread is wider than the gap; Meilisearch's 1.18 ms is doing
more per query (its own ranking pipeline runs alongside the ANN search),
so read that gap as two different products, not a rout.

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
virtualised fsync. **Regenerated at `bdc64eb` (2026-09-02/03) as a gated
median of three** — the first repeated edition of this table, and the
first against MySQL 8.4:

| Engine | write ops/s | read ops/s |
| --- | --- | --- |
| InlaySQL, host (real `F_FULLFSYNC`) | 246.8 | 1,028,190 |
| InlaySQL, containerised | 619.8 | **704,742** |
| MySQL 8.4 (`innodb_flush_log_at_trx_commit=1`, binlog off) | **910.3** | 10,498 |
| PostgreSQL 17 (`fsync=on`, `synchronous_commit=on`) | 762.8 | 58,415 |

**Reads: ~67x MySQL and ~12x PostgreSQL**, containerised — an in-process
library against a socket round trip, an asymmetry that is structural and
stated rather than hidden. The PostgreSQL multiple was ~35x in the last
edition's single run; the whole of the narrowing is PostgreSQL's own read
column moving 19.4k → 58.4k, which nothing in the driver or its
configuration explains and `BENCHMARK.md` records as unattributed.
InlaySQL's own read cells swing 40-85% across the three runs of one
binary, so hold the tens-of-x, not the digits. **Writes: we lose to both**
— MySQL 8.4 by ~1.5x and PostgreSQL by ~1.2x, MySQL ahead in 3 of 3 runs
and PostgreSQL in 3 of 3; which server leads the other flipped against
the 2026-08-30 interleaved rerun (1.43x/1.81x) on a MySQL version change
and is noise. `BENCHMARK.md` carries an extensive correction on this
table: the transport asymmetry above (no socket round trip) is worth
roughly as much as the entire PostgreSQL gap on its own. What is
structural regardless: this workload is one commit at a time on one
connection, so group commit cannot fire by design, and what is left is
per-commit cost against InnoDB's own redo write.

Every row above measures InlaySQL as a *library* against two servers, so the
reads win partly by paying no socket round trip. **Server to server, over the
wire, that advantage is smaller and still real** (AHL-489). Same gated
median-of-three sitting as the table above (`bdc64eb`, 2026-09-02/03) —
[`inlaysql serve --mysql`](#speaking-mysql-over-the-wire) reached over a
compose network by `mysql.connector`, against MySQL 8.4 on the same driver
and the same transport, each connection a spawned OS process rather than a
Python thread so the client's own GIL cannot contaminate the comparison,
and both engines' own commits-per-fsync counters bracketed around the write
phase:

| Engine | Connections | write ops/s | read ops/s | commits per fsync |
| --- | --- | --- | --- | --- |
| InlaySQL, `serve --mysql` | 1 | 668.9 | **10,292.4** | 1.00 |
| InlaySQL, `serve --mysql` | 8 | 1,522.2 | 9,067.7 | 4.06 |
| MySQL 8.4 | 1 | 1,041.8 | 8,789.2 | 0.98 |
| MySQL 8.4 | 8 | **4,992.0** | 8,344.8 | 3.90 |

Reads edge it at one connection (~1.2x, 3 of 3 runs) and tie at eight
(~1.1x, inside the floor) — and the 30% one-to-eight read drop the last
edition's single run found (9,033.3 → 6,294.3) did not reproduce in three
gated runs: the step is −4% to −12%, against MySQL's own −5%, and
`BENCHMARK.md` records it as not reproduced rather than fixed. Writes lose
at one connection (~0.64x) and badly at eight (~0.30x): MySQL's write
throughput scales 4.8x from one connection to eight where InlaySQL's
reaches 2.3x. The commits-per-fsync column says that is not a batching
gap — at eight connections our coordinator rides 4.06 commits per barrier
to InnoDB's 3.90 — it is barrier *rate*: ~375 fsyncs/s against ~1,280,
thread-per-connection against a worker pool, the same diagnosis the
1/4/16-connection sweeps reached on 2026-08-31. MySQL's own write column
is the loudest on the page (46-67% run to run), so read the multiples as
bands. `bench/README.md` has the full methodology and the remaining
asymmetries. PostgreSQL has no row because this server speaks only the
MySQL wire protocol.

**Read shapes and batch insert (regenerated 2026-09-02/03, gated, `REPS=5`,
unix socket).** Four workloads that had no harness on either side until
2026-08-31 — and they still do not all go our way:

| Shape | InlaySQL | MySQL 8.4 | PostgreSQL 17 |
| --- | --- | --- | --- |
| Indexed range scan, 50 rows | **119,219 ops/s** | 14,330 ops/s (**~8x**) | 21,824 ops/s (**~5.5x**) |
| Join, secondary-index inner, full | **3.63 ms** p50 | 13.71 ms (**~4x**) | 9.42 ms (**~2.6x**) |
| Join, PK inner, full | **3.25 ms** p50 | 13.68 ms (**~4x**) | 9.36 ms (**~2.9x**) |
| `GROUP BY n`, 100 groups | **210/s** | 110/s (**~1.9x**) | 167/s (**~1.26x**) |
| Scalar `COUNT/MIN/MAX`, 100k rows | **1,914/s** | 300/s (**~6x**) | 362/s (**~5x**) |
| Batch insert, 100 rows/statement, containerised like the servers | **67,484 rows/s** | 56,700 rows/s (**~1.2x**) | 99,212 rows/s (we lose ~1.5x) |
| Batch insert, same, InlaySQL on the host (`F_FULLFSYNC`) | 24,102 rows/s | (we lose ~2.4x) | (we lose ~4.1x) |

The range scan we lose to SQLite is a shape we *win* against both servers,
so "our row iteration is slow" is a statement about SQLite specifically,
not about every engine (the InlaySQL range and join cells are this
edition's gated `run.sh` figures from `1f7921a`, a different sitting and a
later build than the server columns — `BENCHMARK.md` says so). **The `GROUP BY` row was the
worst multiple we published against anyone** — 29/s, 3.4-5x slower than
both on 2026-08-31 — and is now a win against both, 5 of 5 repetitions
non-overlapping; that is the aggregate work of 2026-09-02/03 (AHL-513
through AHL-541, each step measured in `PERF.md`), not one commit, and
about a tenth of it is the quieter machine, since the servers' own cells
rose 9-14% too. **The scalar aggregate flipped the same day**: it read 225/s
(0.75x/0.62x, a loss) at 03:15; by 15:26 `MIN`/`MAX` of the rowid answer
by one descent each and `COUNT(*)` from the leaves' cell counts (AHL-546,
AHL-548 — SQLite's own optimisations, refused the moment a `WHERE` or a
`COUNT(col)` appears), and the cell reads 1,914/s. **Batch insert has two rows now.** Like for like — InlaySQL in a
container on the same volume class as the servers — it is ~1.2x MySQL 8.4
and ~0.68x PostgreSQL 17; on the host it loses 2.4x/4.1x, and
`BENCHMARK.md` says why: the host cell pays one `F_FULLFSYNC` per
statement — 241 commits/s, 4.1 ms each, 98% of that barrier's ceiling —
while the servers commit against the Docker volume's cheaper barrier (2.5x
cheaper on this machine, measured InlaySQL against
itself), and a quiet machine let their side rise. AHL-542 removed the
engine's own per-row page round trip from that statement (1.29-1.44x on
its own profile); what is published is the barrier, not the engine. The
PK-inner full join that was a loss to PostgreSQL on 2026-08-31 is a win
because AHL-524 fixed the inverted join cost model.

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

1. **The `LIMIT` join shapes and the range scan against SQLite** — the read
   losses that are left. The `LIMIT 10` form of the two join shapes is 1.12x
   and 1.38x behind journal-mode SQLite (the PK shape's band now touches
   SQLite's; AHL-549 took it there), the 50-row indexed range scan is 0.81x
   of it on p50 (and roughly 0.5x of WAL mode; AHL-550 took it from 0.67x),
   and the point read's *throughput* is 0.56x of SQLite WAL's even though
   its p50 is already ahead of it.
   Everything else on the read side wins now: both *full* joins beat SQLite by
   roughly 3x and 7-8x and both servers by roughly 2.6-4x, and the range scan
   we lose to SQLite is one we win against MySQL 8.4 and PostgreSQL 17 by
   roughly 8x and 5.5x — the loss is a statement about SQLite's row iteration
   specifically, not about ours in general. What is left is per-row rather
   than per-query, and it has been narrowed mostly by elimination: AHL-532
   found per-execution planning is about 5% of a `LIMIT` join (the plan cache
   it went in to build was measured unnecessary and never built) and sized a
   limited scan's first batch to the `LIMIT`; AHL-535's borrowing row API took
   the allocations out of the row loop entirely; AHL-549 took the decode copy
   and the bookkeeping allocations off the join probe; AHL-550 compiled the
   residual filter once per execution. After all four, the profile is ten
   full tree descents for ten consecutive keys in one leaf at 37% of the
   `LIMIT` join on their own, and on the range shape `memcmp` at 27% (the
   descent) with the compiled filter at 7%. The next
   angle is extending the point read's already-proven retained cursor to the
   entry-range walk itself (`walk`/`scan_range_from`; the cheapest first step
   is `colliding_rows` over the already cursor-backed `scan_index_row_ids`).
   [`PERF.md`](PERF.md) carries what was built, measured and dropped on the
   way here, each with the number that killed it: page/cell representation
   twice (AHL-493), prefix-skipping key comparison during descent, a
   per-statement join-plan cache, a dense-rowid leaf walk, a covering-index
   scan and a 64 MiB shared read cache.
2. **Write throughput at eight connections, server to server.** Over the wire
   against MySQL 8.4 on the same driver and the same transport, writes are
   ~0.64x at one connection and ~0.30x at eight: MySQL's write throughput
   scales 4.8x from one connection to eight where this engine's reaches 2.3x.
   Commit batching is not the gap — at eight connections our coordinator rides
   4.06 commits per barrier to InnoDB's 3.90 — it is barrier *rate*, roughly
   375 fsyncs/s against 1,280: thread-per-connection against a worker pool,
   the same diagnosis the 1/4/16-connection sweeps reached on 2026-08-31. The
   per-connection page cache this item used to be about is gone from it,
   because the diagnosis behind it did not survive two investigations: the
   read drop it cited was a GIL-bound threaded Python client, a process-based
   driver has replaced it, and the first gated, repeated edition of that table
   reads the 1-to-8 read step at −4% to −12%, inside the measurement floor —
   not reproduced, not claimed fixed. As a library, containerised, single-row
   durable writes are ~1.5x behind MySQL 8.4 and ~1.2x behind PostgreSQL 17,
   and batch insert like for like is ~1.2x MySQL and 0.68x PostgreSQL.
   `BENCHMARK.md`'s correction on the library figures is worth reading before
   trusting the size of any of them: the transport asymmetry that flatters the
   library rows (no socket round trip) is worth roughly as much as the entire
   published PostgreSQL gap on its own.
3. **Commit-side logical group commit (C1) — built, measured, and closed as a
   loss until the two layers compose.** Both slices are in behind
   `EngineOptions::commit_absorption`, off by default. Slice 1 (AHL-544) moved
   the first-committer-wins decision to the gate holder and measured flat,
   which its own plan predicted, because every follower still entered the gate
   to encode and append. Slice 2 (AHL-547) removed all three — one gate
   acquisition, one WAL append and one `fsync` per cohort, every member
   acknowledged only after the barrier — and it is **0.78x / 0.87x / 0.90x at
   8 / 16 / 32 writers, three runs of three, non-overlapping**. The mechanism
   is the finding: absorption runs *more* barriers, not fewer (0.140
   syncs/commit against 0.111 at 32 writers), because a commit-side cohort of
   ~5.6 members displaces the flush-side ticket gather that was already
   amortising each `fsync` over 6-9 commits — a follower under absorption
   never publishes a ticket for a flush leader to gather. The two layers
   compete for the same population and the earlier one gathers the smaller
   cohort. A related belief died in the same measurement: holding the gate
   across the cohort's barrier, which the design brief argued for, costs
   roughly half the throughput and 3-7x the p99. The next C1 item, if there
   is one, is one flush ticket per cohort gathered across leaders, plus
   cohorts that survive a WAL-region boundary — with a measurement gate
   before any code.
4. **Reordering past the leading join.** The planner exchanges which table
   drives a two-table inner join when a complete, current `ANALYZE` snapshot
   says the other side is cheaper, and an `ORDER BY` with a `LIMIT` reorders
   too. Nothing else does: three or more tables (a search problem, where this
   is one comparison), any join after the first, a join with a derived table
   on either side, an outer join, and any join whose driving table answers a
   retrieval score all keep their written order and fall back to the
   deterministic rule in
   [Scalar indexes and joins that use them](#scalar-indexes-and-joins-that-use-them).
5. **Deeper SQL Logic Test coverage, real SQLancer runs and continuous
   fuzzing** beyond what `trust.yml` runs today (see
   [`docs/sqlancer.md`](docs/sqlancer.md)).
6. **Server posture: refuse to expose, and fuzz the packet path.** `127.0.0.1`
   is the default bind and should stay the default; what is missing is the
   loud path — binding to anything else while TLS is off, or while the only
   credential is the bootstrap `--user`/`--password` pair, should *refuse*
   rather than warn, because a database that is easy to expose by accident is
   the failure mode the whole item exists to prevent. The wire parser is
   attacker-facing by construction and young; the fuzzer has already found one
   parser DoS in this project (AHL-500), and the server's own packet path
   deserves the same treatment before the documentation stops saying
   "localhost only".
7. **Read replicas over the existing CDC log**, and the serverless work that
   shares its shape. `cdc.rs` is already pull-based and bounded, so the work
   is shipping records and tracking replica position — the Turso model, no
   consensus and no fork. Two things have to be answered before any of it:
   the CDC log deliberately carries no row payloads, so there is nothing for a
   replica to apply yet, and `open_read_only` takes no OS lock by design, so a
   reader in another process cannot be proven absent — fine on one machine
   today, unavoidable once a second machine reads the same file. Durable
   storage/compute separation (an object-storage-backed `Device`, for corpora
   too large to ship as an edge asset) is the same category of work and starts
   as a research brief with measured S3 and R2 latencies, not as code.

Full Postgres parity is deliberately not on this list — see the last point in
[What this is not](#what-this-is-not).

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

- **Retrieval indexes are explicit, and a vector index is single-column on
  purpose.** A `TEXT` column is only full-text indexed after
  `CREATE INDEX idx ON t (body)` (or in a database written before
  `CREATE INDEX` existed, whose columns are grandfathered); the same for a
  `VECTOR` column and an ANN index. A BM25 index may span several columns —
  `CREATE INDEX idx ON docs (title, body) USING FULLTEXT` builds one combined
  index over the concatenation of every named column's text, MySQL's
  `FULLTEXT(title, body)`, so a term matching one column still ranks the row,
  and `bm25_score(title, body, ?)` finds it whichever order the columns are
  named in; a bare `CREATE INDEX idx ON docs (title, body)` with no `USING`
  still means a B-tree, exactly as it always has. `VECTOR` stays
  single-column, and that is a decision rather than a gap: two embedding
  columns are generally two different vector spaces, and there is no standard
  meaning for one HNSW graph over both — concatenated or weighted-sum
  embeddings are technically possible but not a default anyone should get
  without asking for it by name. A scalar index is a different structure
  again: `CREATE INDEX` on `INTEGER`/`REAL`/`TEXT` (`USING BTREE` on the last)
  is a real ordered B-tree, may be declared `UNIQUE`, and may span more than
  one column — see
  [Scalar indexes and joins that use them](#scalar-indexes-and-joins-that-use-them).
- **Join order is costed for one join, and only one.** `ANALYZE` records row
  counts and leading-index cardinalities, and a complete, current snapshot
  lets the planner choose between the hash-join and index-probe operators for
  each join (`docs/research/cost-planner.md`) *and* exchange which of a
  two-table inner join's tables drives (AHL-512, cost model corrected in
  AHL-524) — a plan rewrite with every ordinal remapped, so what runs is
  byte-for-byte the plan the same query written the other way round would have
  produced. An `ORDER BY` with a `LIMIT` may reorder as well (AHL-525); a
  `LIMIT` with no `ORDER BY` never does, because there a different order is a
  different result set. Everything past that keeps its written order: three or
  more tables, joins after the first, a derived table on either side, an outer
  join (`a LEFT JOIN b` is not `b LEFT JOIN a`) and any join whose driving
  table answers a retrieval score. Missing or stale stats fall back to the
  narrow rule that already existed: a retrieval expression is answered by its
  index, a top-level equality on `INTEGER PRIMARY KEY` or a scalar-indexed
  column by a tree descent or range probe — including as the inner side of a
  join (AHL-464) — a full-scan equi-join by a hash build, and everything else
  by a full scan. [Performance](#performance) publishes both full-join shapes,
  which win, and the `LIMIT` shapes, which lose.
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

## Development

```sh
cargo test --workspace          # unit, integration, sqllogictest, wire
./docker/test.sh                # every CI gate, in Linux containers
./docker/test.sh sweep          # the DST crash/torn-write sweeps
./bench/run.sh                  # the benchmark suites
```

CI gates a merge on four jobs: the check list above, fuzz targets, the
determinism job (the core stays `no_std` with no OS-facing dependency), and
the DST sweeps. The rules of the road — conventional commits, benchmarks only
from scripts, a clause that cannot be honoured is refused rather than ignored,
DST sweeps for storage changes — are in [`CONTRIBUTING.md`](CONTRIBUTING.md),
and what the tests cover and deliberately do not is in
[`TESTING.md`](TESTING.md).

## Security

Security issues go through the private disclosure flow in
[`SECURITY.md`](SECURITY.md) — never a public issue. That file also states the
threat model and its known limitations in plain language, including the
MySQL-wire server's deployment boundaries; the gap-by-gap engineering audit
it summarises is [`docs/enterprise-readiness.md`](docs/enterprise-readiness.md).

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
