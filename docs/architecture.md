# Architecture decisions

The load-bearing design choices behind InlaySQL, and the reasoning that
produced each one. Source comments throughout the engine cite these by number
— `D1` in `inlaysql-server`, `D4` in `btree/cache.rs`, and so on — so this file
is the thing they point at rather than background reading.

Each decision is recorded with what it rules out as well as what it chooses,
because the constraint is usually the useful half.

---

## The constraints every decision works inside

These are enforced by CI, not by convention, and nothing below relaxes them.

1. **`inlaysql-core` is `no_std` and `#![forbid(unsafe_code)]`.** The
   `determinism` job fails the build if the attribute disappears or if an
   OS-facing crate enters core's dependency tree. All networking therefore
   lives in a separate crate, never in core.
2. **SQLite's dialect is the baseline.** MySQL compatibility is a translation
   shim in `inlaysql-server` (D1), not MySQL-isms in the core parser. Full
   PostgreSQL parity is an explicit non-goal.
3. **A change to `btree`, `wal`, `sim`, `hnsw`, `hnsw_paged`, `bm25`, the row
   codec or the catalog encoding requires a deterministic-simulation pass**,
   not just `cargo test`.
4. **No benchmark number is published anywhere unless it regenerates from
   `bench/run.sh` or `bench/compare.sh`.**
5. **A clause this engine cannot honour is refused, never accepted and
   ignored.** This is the bug class that has cost the project the most.

---

## 2. Architecture decisions

### D1 — MySQL compatibility is a shim, not a dialect change
Core keeps the SQLite dialect. A new crate `inlaysql-server` speaks the MySQL wire
protocol and owns a **translation layer**:
- Passes ordinary DML/DDL through (SQLite's dialect is a near-superset
  of what ORMs emit; backtick identifiers already parse under sqlparser's SQLite dialect).
- Intercepts and emulates session/system statements ORMs send: `SET NAMES`,
  `SET sql_mode`, `SELECT VERSION()`, `SELECT DATABASE()`, `SHOW TABLES`,
  `SHOW FULL COLUMNS`, `SHOW KEYS`, `SHOW VARIABLES`, `information_schema.tables` /
  `.columns` queries — answered from `Catalog`, never sent to the SQL engine.
- Maps error cases to MySQL error codes (`Error::Constraint` dup-key → 1062, etc.).
This keeps rule "SQLite dialect is the baseline" true and gives frameworks what they
probe for. Postgres wire can follow later behind the same seam (it also unlocks real
SQLancer over JDBC — `docs/sqlancer.md` names a server mode as the most faithful option).

### D2 — Thread-per-connection, one `Database` handle per connection
The engine is `!Send` by design, and multiple handles on one file already commit
concurrently with first-committer-wins (proven in `concurrent_writers.rs`). So the
server is boring std: `std::net::TcpListener`, one OS thread per connection, each
thread opens its own `Database` on the same file. **No tokio, no async, anywhere.**
This matches the repo's existing zero-runtime ethos (the MCP server made the same
call). This required the snapshot-refresh fix first — without it every connection
would read stale data forever.

### D3 — Secondary indexes live inside the existing CoW tree
New scalar B-tree indexes are **not** a new storage structure. Index entries are rows
in the same tree under a reserved key prefix
(`idx:<table>:<index>\0<memcomparable-encoded-value>\0<be rowid>` → empty value),
following the existing `table\0<be u64>` key discipline (`storage.rs:21-30`). That
buys WAL, crash recovery, MVCC rebase, and DST coverage for free — the only new
storage-layer artifact is a memcomparable encoding for `Integer/Real/Text` (order-
preserving byte encoding; standard f64 sign-flip trick, no floats compared in code).
UNIQUE = key-prefix collision check at insert. Catalog encoding bumps to v4
(new `IndexKind::BTree`), which is allowed pre-1.0 (recreate, not migrate).

### D4 — Page cache exploits CoW immutability
Committed data-area pages are never rewritten in place and page ids are never reused
today (monotonic allocator), so a per-handle LRU page cache needs **no invalidation
protocol at all**: cache key = page id, always valid. Two
carve-outs: never cache WAL-region or state/header blocks, and when the free
list *reuses* page ids, cache entries must be versioned by commit seq (do the
free list strictly after the cache, and gate reuse on an epoch check). Cache lives
behind the `Device`/tree seam in core (`no_std`-safe: plain `alloc` LRU), so DST
exercises it under fault injection like everything else.

### D5 — Executor goes streaming; row format gains lazy decode
Replace materialise-everything with an iterator pipeline (scan → filter → join →
aggregate/sort → limit → project), pushing LIMIT into non-sorted plans and decoding
only projected columns. Keep the row codec's tag-walk format (early-exit decode is
cheap); a column-offset directory is a format bump to consider **only if** profiling
still shows decode dominating after streaming lands.

### D6 — Planner stays rule-based, gets real rules (cost model is staged)
Before a cost model, add the rules that pay: equality/range predicate → B-tree index
probe (G4's new indexes), an index nested-loop join when the `ON` is an equality on the
inner table's PK or an indexed column, and a hash join of the inner table for a
full-scan equi-join on same-storage-class keys, LIMIT/projection pushdown, and
`COUNT(*)` fast path. The join split is by shape, not size: a `LIMIT` or a
point-pinning `WHERE` keeps the probe (few outer rows → few descents), while a full
scan prefers the hash table (one O(inner) build amortised over every outer row).
The first staged cost layer now exists after `ANALYZE`: with a complete,
current statistics snapshot it may choose between the existing hash and probe
operators by cardinality. Missing or stale stats fall back to these shape
rules. Do not build a broader cost model before the access paths.

**Join reordering landed 2026-09-01, for full scans only.** A two-table inner
join may now be executed with its sources exchanged when the same cost
function scores that cheaper — measured at 1.31x on the `joins` suite,
interleaved. Three things bound it, and the third is the output-order proof
this decision used to defer:

* Two stored tables, `INNER`, no derived source and no retrieval score. An
  outer join is not commutative and a scored query answers from its driving
  table by definition.
* **The smaller table drives (AHL-524, 2026-09-02).** The first costing
  priced an outer row at one unit and a hash-built inner row at two, so it
  preferred to build the smaller table and drive from the larger one — and
  swapped the 20k-users × 160k-posts join into posts-driving, which measured
  3x slower (4.8 ms → 14.5 ms) for the same 160k output rows. Every outer
  row pays the join loop itself, whichever inner path answers it; the cost
  model now charges that (`OUTER_ROW_COST` in `planner.rs`), and both
  written orders of that join run users-driving at ~3.3 ms. `PERF.md`,
  2026-09-02, has the bisect.
* The rewrite is a plan rewrite: sources are exchanged and every column
  ordinal in the plan is remapped, producing exactly the plan the same query
  written the other way round would have produced. What executes is a shape
  the engine already ran, not a new one.
* **No `LIMIT`, or an `ORDER BY` (AHL-525 widened this from "full scans
  only", 2026-09-02).** Reordering changes the order rows come out of an
  unordered join — legal SQL, and what SQLite does — but under a `LIMIT`
  with no `ORDER BY` a different order is a different *set*, and a plan
  choice may not decide which rows a query returns. With an `ORDER BY` the
  sort decides the order afterwards and the `LIMIT` truncates the sorted
  answer, so the reordered plan returns the same rows; only ties under the
  `ORDER BY` may come out differently, exactly as they may in SQLite. A
  limited join with no `ORDER BY` keeps its written order. Ties in the
  *cost* keep the written order too, so a plan does not move on estimation
  noise.

### D7 — Types follow SQLite affinity, not strict names
Replace the strict `resolve_data_type` whitelist with SQLite's affinity rules
(any type name accepted; INT→Integer affinity, CHAR/CLOB/TEXT→Text, BLOB→Blob,
REAL/FLOA/DOUB→Real, else Numeric) plus the InlaySQL extension `VECTOR(n[, INT8])`.
This is *more* SQLite-baseline than today, and it is the single change that makes
`DATETIME`, `BOOLEAN`, `JSON`, `ENUM(...)` DDL from Laravel migrations work.
Dates/times store as TEXT/INTEGER exactly as SQLite does; JSON stores as TEXT with
functions over it.

---

## 3. Repository layout, and the `no_std` boundary

Moved here from the README's `## Layout` section, unchanged.

```
crates/
  inlaysql-core/    SQL + planner + executor + storage + retrieval  (no_std)
  inlaysql/         file-backed Device, Database and AsyncDatabase  (std)
  inlaysql-uring/   io_uring Device backend  (Linux)
  inlaysql-ffi/     the C ABI: libinlaysql.{dylib,so} for FFI languages
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
row. See [`docs/recovery.md`](recovery.md) for the crash-recovery protocol
and [`docs/indexes.md`](indexes.md) for how a saved index stays honest
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


## 4. Non-goals — what this is not, in full

Moved here from the README's `## What this is not` section, unchanged. The
README keeps a one-line summary of each; this is the reasoning behind them.

Explicit non-goals for the current stage, all of them scheduled work rather
than oversights.

If the question is specifically "could our organisation run this in
production?", [`docs/enterprise-readiness.md`](enterprise-readiness.md)
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
  [Scalar indexes and joins that use them](sql.md#scalar-indexes-and-joins-that-use-them).
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
  by a full scan. [Performance](../BENCHMARK.md) publishes both full-join shapes,
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
  runtime](using.md#on-an-edge-runtime) is delivered today and does not need any of
  the above, because a retrieval index is built once, shipped as a static
  asset, and answered from the isolate that took the request — there is no
  node to be a replica of. Multi-node deployment (read replicas over the
  existing CDC log; durable storage/compute separation for corpora too large
  to ship as an asset) is later-stage work, and none of it is started; the CDC
  log's missing row payloads (`enterprise-readiness.md`) gate all of it.
- **No point-in-time recovery.** [Online backup](using.md#online-backup) takes a full
  consistent copy of a live database, which is a different thing: the states
  you can restore to are the ones you took a copy at, not any instant in
  between. Rolling forward from one needs a log carrying row payloads, and the
  CDC log deliberately carries none — see
  [`docs/enterprise-readiness.md`](enterprise-readiness.md). Incremental
  backup is not implemented either.
- **Full Postgres parity is not a goal**, now or later.

