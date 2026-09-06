# Using it — the Rust API

The API surface of the `inlaysql` crate: opening a database, prepared
statements, async without a runtime, choosing an I/O backend and a durability
level, the MCP server, the MySQL wire server, the browser and edge builds,
change data capture and online backup.

This was the README's `## Using it` section; it moved here so the README could
stay a landing page. The prose is unchanged.

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

## Prepared statements

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

## Async, without a runtime

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

## Choosing an I/O backend

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

## Choosing a durability level

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
torn or invented state. See [`docs/recovery.md`](recovery.md#durability-levels)
for the exact loss bound, the per-platform mapping (it is not the same trade
on macOS and Linux), and the cross-handle rule when two handles on one file
disagree, and `PERF.md` for the measured numbers.

## Handing the database to an agent

An InlaySQL file is an MCP tool. No glue code, no schema translation, no vector
store to keep in sync:

```sh
inlaysql serve --mcp app.inlay          # read-only; add --allow-writes to permit writes
```

The agent gets `schema`, `query`, `hybrid_search` and `changes`; `execute` is
not even advertised unless writes are allowed. Read-only is enforced by
*planning* the statement, results are capped by row count and by bytes, and
embeddings render as `<vector(384)>` rather than as 384 floats in the model's
context. See [`docs/mcp.md`](mcp.md).

## Speaking MySQL over the wire

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
[`docs/server.md`](server.md#full-text-search-match--against-translated-to-the-native-bm25-probe).

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
([`docs/server.md`](server.md#mysql-only-ddl-is-translated-not-invented)) —
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
not a stolen password list. **A `--bind` that reaches another machine is
refused, not warned about**: an empty bootstrap password, a database with no
accounts of its own, no certificate, or a certificate that is not required each
stop the start with a sentence naming the address, the fact and the flag that
fixes it. `--plaintext-network` relaxes the two TLS conditions on a segment it
checks is private — RFC1918 and friends, never a wildcard, never a routable
address — and never the other two. Accounts, `GRANT`/`REVOKE` and per-table
privileges live in the file itself; the `--user`/`--password` flags are the
whole credential only until the first `CREATE USER`, and
`inlaysql user add app.inlay --user app --password-env VAR --superuser`
creates that first account without starting a server.
[`docs/server.md`](server.md) has the full security posture, the
function-by-function mapping and the complete divergence list, each checked
against a real MySQL 8.4.11.

## In a browser

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

## On an edge runtime

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
sizes and what CI checks: [`docs/wasm.md`](wasm.md).

## Change data capture

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

## Online backup

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
[`docs/server.md`](server.md#backing-up-a-running-server).
