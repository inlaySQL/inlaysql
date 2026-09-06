<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="logo/InlaySQL_Logo-horizontal-dark.svg">
    <img alt="InlaySQL" src="logo/InlaySQL_Logo-horizontal.svg" width="420">
  </picture>
</div>

<p align="center">
  <a href="https://github.com/inlaySQL/inlaysql/actions/workflows/ci.yml"><img src="https://github.com/inlaySQL/inlaysql/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/inlaySQL/inlaysql/actions/workflows/wasm.yml"><img src="https://github.com/inlaySQL/inlaysql/actions/workflows/wasm.yml/badge.svg?branch=main" alt="WASM"></a>
  <a href="https://github.com/inlaySQL/inlaysql/releases"><img src="https://img.shields.io/badge/version-0.0.1.beta-orange" alt="v0.0.1.beta"></a>
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
extensions bolted on the side. It runs as a
[Rust crate](docs/using.md) (`#![forbid(unsafe_code)]` outside one Linux I/O
backend), in [the browser as WebAssembly](https://inlaysql.github.io), over the
[MySQL wire protocol](docs/server.md) so existing ORMs connect as-is, and as a
CLI.

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

## Install and run

Download the library for your platform from the
[releases page](https://github.com/inlaySQL/inlaysql/releases)
([`v0.0.1.beta`](https://github.com/inlaySQL/inlaysql/releases/tag/v0.0.1.beta);
macOS Apple silicon and Linux x86_64 today — the file layer is Unix-only, and
the WASM module runs anywhere a browser or Node does), copy the ~40-line loader
for your language from [`docs/clients.md`](docs/clients.md#the-5-minute-version)
(PHP, Python, Ruby, C# and Java each have a tested one), and open the file:

```php
// PHP (FFI is built in) — Ruby and Python are the same shape
$db = new InlaySQL('app.inlay');
$db->run('CREATE TABLE IF NOT EXISTS docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(384))');
$db->run('CREATE INDEX docs_body ON docs (body)');            // BM25
$db->run('CREATE INDEX docs_embedding ON docs (embedding)');  // HNSW
$db->run('INSERT INTO docs (body, embedding) VALUES (?, ?)', ['an embedded database in rust', $vec]);

// Hybrid retrieval — vector search and BM25 fused — is one ordinary statement.
// The planner turns each retrieval function into an index probe and fuses the
// two rankings: no separate vector store, no application-side merge.
$rows = $db->run(
  'SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
   FROM docs ORDER BY score DESC LIMIT 3',
  [$queryVec, 'rust database']
);
```

Rust, the CLI and the MySQL-wire server:

```toml
[dependencies]                          # not on crates.io yet — the format is pre-1.0
inlaysql = { git = "https://github.com/inlaySQL/inlaysql" }
```

```sh
git clone https://github.com/inlaySQL/inlaysql
cd inlaysql && cargo build --release -p inlaysql-mcp
target/release/inlaysql serve --mysql app.inlay   # point your ORM at :3306
cargo run --example hybrid_search                 # the query above, end to end
```

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
- **Your ORM already speaks it.** `inlaysql serve --mysql` talks the MySQL
  wire protocol, and a stock Laravel 11 app migrates and runs against it
  ([`docs/server.md`](docs/server.md)).
- **It runs where your code runs.** The same file opens natively, from
  WebAssembly in a browser tab, and on an edge runtime, byte for byte
  ([`docs/wasm.md`](docs/wasm.md)).
- **Honest about the trade.** Every benchmark regenerates from a script in
  this repo, wins and losses both — see [Performance](#performance) below,
  and what is not built yet in [What this is not](#what-this-is-not).

## Documentation

| | |
| --- | --- |
| [`docs/using.md`](docs/using.md) | the Rust API: prepared statements, async without a runtime, I/O backends, durability, CDC, backup |
| [`docs/sql.md`](docs/sql.md) | the dialect in full, the index rules, and the SQL Logic Test pass rate |
| [`docs/clients.md`](docs/clients.md) | PHP, Python, Ruby, C#, Java and Node loaders, tested |
| [`docs/server.md`](docs/server.md) | the MySQL wire server: accounts, TLS, limits, translated and refused SQL |
| [Framework examples](crates/inlaysql-wasm/www/frameworks/README.md) | React, Vue, jQuery and plain JS against the WASM build |
| [`BENCHMARK.md`](BENCHMARK.md) and [`SCOREBOARD.md`](SCOREBOARD.md) | every benchmark, wins and losses, and the verdict matrix with its fairness audit |
| [`docs/architecture.md`](docs/architecture.md) | the load-bearing design decisions, the crate layout, and what each rules out |
| [`docs/PLAN.md`](docs/PLAN.md) | what is being built next, in order, and why |
| [`TESTING.md`](TESTING.md) and [`docs/enterprise-readiness.md`](docs/enterprise-readiness.md) | what is covered, what is not, and the gaps that would stop a deployment |

## Performance

```sh
./bench/run.sh        # points, indexed, joins, vectors, quantisation, retrieval
./bench/compare.sh    # DuckDB, pgvector, Meilisearch, MySQL, PostgreSQL (needs Docker)
```

| Workload | InlaySQL | Compared with |
| --- | --- | --- |
| Point read by primary key | **991,539 ops/s**, 0.75 µs p50 | SQLite journal, durable: 164,448 ops/s (**~3-11x**); SQLite WAL: 1,161,418 ops/s (0.85x on throughput, our p50 below its 0.792 µs in all three runs) |
| Point read, secondary index | **527,067 ops/s**, 1.75 µs p50 | SQLite journal, durable: 265,615 ops/s (**~2x**) |
| Join, secondary-index inner, full scan | **3.60 ms p50** | SQLite: 31.30 ms p50 (**~8x**) |
| Durable write, one commit each | **250 ops/s**, 3.89 ms p50 | SQLite journal, durable: 89 ops/s (**~2.8x**) |
| Concurrent durable writers, 8 threads | **1,110 commits/s**, 0.0% aborted | SQLite journal, durable: 88 commits/s (**~13x**) |
| Hybrid retrieval, one SQL statement | **167.00 µs p50** | DuckDB 11.37 ms, pgvector 14.11 ms, Meilisearch 4.15 ms (**~25-90x**) |

One developer machine — reproduce it, do not trust it. Repeating the identical
binary against identical data moves these figures by a median 4.0-7.3%, so the
multiples are rounded to what that floor supports (`~2-4x`, not `3.26x`).
[`BENCHMARK.md`](BENCHMARK.md) is the full set with its provenance header,
its opening note on precision and every table these six rows are drawn from;
[`SCOREBOARD.md`](SCOREBOARD.md) is the win/loss matrix and the fairness audit;
[`bench/README.md`](bench/README.md) is how each comparison is kept fair.

**Where we lose.** A page that lists only wins is advertising:

- **Indexed range scan, 50 rows: ~1.15x behind journal-mode SQLite** (7.29 µs
  against 6.58 µs p50), from 1.20x — and a win of ~5.8-8.8x against MySQL 8.4
  and PostgreSQL 17 on the same shape, which is a statement about the socket
  they pay, not about our row loop —
  [`BENCHMARK.md`](BENCHMARK.md#secondary-index-reads--point-win-range-loss-both-narrowing).
- **The `LIMIT 10` join shapes.** The secondary-index one is ~1.13x behind
  SQLite on p50, from 1.26x; the PK one is no longer a p50 loss (3.25 against
  3.50 µs, ahead in all three runs) but is still ~1.08x behind on throughput,
  which pays our dearer cold first execution —
  [`BENCHMARK.md`](BENCHMARK.md#joins--we-win-both-full-shapes-and-the-pk-limit-shape-is-no-longer-a-loss-on-p50).
- **Batch insert, 100 rows per statement: ~0.68x PostgreSQL 17** like for like
  in a container, ~1.2x MySQL 8.4; on the host it loses 2.4x/4.1x, where every
  statement pays one `F_FULLFSYNC` —
  [`BENCHMARK.md`](BENCHMARK.md#batch-insert--like-for-like-a-win-against-mysql-84-and-a-loss-against-postgresql-on-the-host-the-barrier).
- **Server-to-server writes at 8 connections: ~0.30x MySQL 8.4.** Commit
  batching is at parity (3.89 commits per barrier against InnoDB's 3.90); the
  gap is barrier *rate*, a single-writer gate —
  [`BENCHMARK.md`](BENCHMARK.md#server-to-server-mysql-wire-protocol).
- **Single-row durable writes against the servers are a tie inside an enormous
  spread**, not a win: containerised, 876.0 ops/s against MySQL 8.4's 797.2
  and PostgreSQL 17's 977.4, ahead in one run of three against each —
  [`BENCHMARK.md`](BENCHMARK.md#against-mysql-and-postgresql).
- **Recall on uniformly random vectors is 0.12 at a hundred thousand rows**,
  and no tuning fixes it; on text-derived embeddings it is 0.998-1.000 across
  a 20x range of corpus sizes — [`bench/README.md`](bench/README.md).
- Not measured anywhere here: sustained or multi-core saturation, cold-cache
  reads (every point-read row is warm, and our miss path is dearer than
  SQLite's), and whether Docker Desktop's virtual disk honours `fsync` as a
  barrier for any containerised engine in these tables.

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
| Binding a `VECTOR` parameter | Over the MySQL wire an embedding binds as packed little-endian `f32` — MySQL 9's own `VECTOR` layout — rather than travelling as decimal text inside the SQL. |
| `bm25_score(column, 'terms')` | BM25 relevance over a `TEXT` column. |
| `fuse(a, b, ...)` (alias `rrf`) | Reciprocal rank fusion over the retrieval expressions inside it. |

Retrieval functions are not scalar functions evaluated per row — the planner
hoists them out and answers each from an index. An index exists only where a
`CREATE INDEX` declared it: a query that scores an unindexed column is an
error, not a silent scan. The full dialect, the index rules, the join rules
and the SQL Logic Test pass rate (**1307/1307**) are in
[`docs/sql.md`](docs/sql.md).

## What this is not

Explicit non-goals for the current stage, all of them scheduled work rather
than oversights. Each is argued in full in
[`docs/architecture.md`](docs/architecture.md#4-non-goals--what-this-is-not-in-full);
if the question is specifically "could our organisation run this in
production?", [`docs/enterprise-readiness.md`](docs/enterprise-readiness.md)
answers it directly, and less flatteringly than this list.

- **Retrieval indexes are explicit, and a vector index is single-column on
  purpose.** No column is full-text or ANN indexed until a `CREATE INDEX` says
  so; a BM25 index may span several columns, a `VECTOR` one may not, because
  two embedding columns are generally two different vector spaces.
- **Join order is costed for one join, and only one.** A complete `ANALYZE`
  snapshot lets the planner exchange which table drives a two-table inner
  join; three or more tables, joins after the first, derived tables, outer
  joins and retrieval-driven joins all keep their written order.
- **The default retrieval indexes hold the whole corpus in RAM.** `HnswIndex`
  and `Bm25Index` are in-memory (BM25 measured at ~1,800 bytes per document);
  paged backends that live in the file behind a bounded cache exist
  (`open_paged`, `EngineOptions::paged_text_indexes`) and are not the default,
  because every cache miss during a search becomes a read —
  [`docs/indexes.md`](docs/indexes.md).
- **No clustering, no multi-node replication and no point-in-time recovery.**
  One process, one file — no leader election, no consensus, no read replica.
  Online backup takes a full consistent copy of a live database, so the states
  you can restore to are the ones you took a copy at, not any instant between;
  rolling forward needs a log carrying row payloads and the CDC log
  deliberately carries none.
- **Full Postgres parity is not a goal**, now or later.

What is being built next, in order and with the measurement that gates each
item, is [`docs/PLAN.md`](docs/PLAN.md).

## Layout

`crates/inlaysql-core/` is where the database actually lives — SQL, planner,
executor, storage and retrieval, all `no_std`, so it **cannot** open a file,
read the clock or start a thread even by accident; everything it needs arrives
through the traits in `inlaysql_core::traits`. Around it: `inlaysql/` (the
file-backed `Database` and `AsyncDatabase`), `inlaysql-uring/`, `inlaysql-ffi/`
(the C ABI), `inlaysql-mcp/` (MCP mode and the CLI), `inlaysql-server/` (the
MySQL wire server), `inlaysql-wasm/` (the WebAssembly build, the browser demo,
the Cloudflare Worker) and `inlaysql-bench/`. CI fails if `#![no_std]`
disappears from core or if an OS-facing crate turns up in its dependency tree.
The full map and the reasoning are in
[`docs/architecture.md`](docs/architecture.md#3-repository-layout-and-the-no_std-boundary).

## Development

```sh
cargo test --workspace          # unit, integration, sqllogictest, wire
./docker/test.sh                # every CI gate, in Linux containers
./docker/test.sh sweep          # the DST crash/torn-write sweeps
./bench/run.sh                  # the benchmark suites
```

CI gates a merge on four jobs: the check list above, fuzz targets, the
determinism job, and the DST sweeps. The rules of the road are in
[`CONTRIBUTING.md`](CONTRIBUTING.md), and what the tests cover and deliberately
do not is in [`TESTING.md`](TESTING.md).

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
