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
* The rewrite is a plan rewrite: sources are exchanged and every column
  ordinal in the plan is remapped, producing exactly the plan the same query
  written the other way round would have produced. What executes is a shape
  the engine already ran, not a new one.
* **Full scans only.** Reordering changes the order rows come out of an
  unordered join — legal SQL, and what SQLite does — but under a `LIMIT` with
  no `ORDER BY` a different order is a different *set*, and a plan choice may
  not decide which rows a query returns. A limited join keeps its written
  order. Ties keep the written order too, so a plan does not move on
  estimation noise.

### D7 — Types follow SQLite affinity, not strict names
Replace the strict `resolve_data_type` whitelist with SQLite's affinity rules
(any type name accepted; INT→Integer affinity, CHAR/CLOB/TEXT→Text, BLOB→Blob,
REAL/FLOA/DOUB→Real, else Numeric) plus the InlaySQL extension `VECTOR(n[, INT8])`.
This is *more* SQLite-baseline than today, and it is the single change that makes
`DATETIME`, `BOOLEAN`, `JSON`, `ENUM(...)` DDL from Laravel migrations work.
Dates/times store as TEXT/INTEGER exactly as SQLite does; JSON stores as TEXT with
functions over it.

---
