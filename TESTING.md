# How InlaySQL is tested

Everything on this page reproduces from a checkout. Where something is *not*
covered, it says so — a page that only lists strengths is a marketing page.

```sh
cargo test --workspace          # everything below except the sweeps and the fuzzer
```

## The short version

| What | Where | Runs |
| --- | --- | --- |
| Deterministic simulation (crash / torn write) | `crates/inlaysql-core/tests/dst_sweep.rs` | 10,000 seeds on `main`, on tags, and on a PR labelled `full-ci` |
| Index recovery under the same faults | `crates/inlaysql/tests/index_recovery_dst.rs` | 10,000 schedules (2,500 seeds × 4 index shapes), in the same job |
| Free list page reuse under the same faults (opt-in, AHL-481) | `crates/inlaysql-core/tests/free_list_reuse_dst.rs` | 300 seeds every push; 5,000 in the same `sweep` job as the other two (`ci.yml`'s "Sweep page reuse") |
| Online backup is exactly the snapshot it was taken from, under the same faults | `crates/inlaysql-core/tests/backup_dst.rs` | 200 seeds every push; 5,000 in the same `sweep` job (`ci.yml`'s "Sweep online backup") |
| Online backup beside live writers, another process, and page reuse | `crates/inlaysql/tests/backup.rs` | every push |
| A churn workload stops growing the file once reuse is on (opt-in, AHL-481) | `crates/inlaysql/tests/free_list_growth.rs` | every push |
| The streaming executor stops early only when that is still the right answer | `crates/inlaysql-core/tests/streaming.rs` | every push |
| A cancelled statement leaves nothing behind, at every point it can be stopped | `crates/inlaysql-core/tests/cancellation.rs` | every push |
| SQL Logic Test subset | `crates/inlaysql/tests/sqllogictest/` | every push — **1154/1154** |
| Metamorphic logic-bug tests (SQLancer-style) | `crates/inlaysql-core/tests/logic_bugs.rs` | every push |
| Comparison is a genuine total order over every storage class, every collation | `eval.rs`, `engine.rs` | every push |
| Differential testing against SQLite | `crates/inlaysql/tests/differential.rs` | 200 rounds every push, 50,000 nightly |
| Fuzz-target properties on stable | `crates/inlaysql-core/tests/fuzz_smoke.rs` | every push |
| Coverage-guided fuzzing | `fuzz/` | nightly, 5 min per target, artifacts published |
| Concurrent writers: conflicts reported, nothing lost | `crates/inlaysql/tests/concurrent_writers.rs` | every push |
| An index larger than one transaction | `crates/inlaysql/tests/large_index.rs` | every push (the 5,000-row case nightly) |
| A foreign commit re-indexes only the rows it touched | `crates/inlaysql-core/tests/foreign_commit_indexes.rs` | every push |
| The raw leaf scan reads pages whose ids were handed out again | `crates/inlaysql/tests/raw_scan_reuse.rs` | every push |
| `INTEGER` comparison is exact above 2^53 | `crates/inlaysql/tests/large_integers.rs` | every push |
| A cancelled statement leaves the table and the handle untouched | `crates/inlaysql-core/tests/cancellation.rs` | every push |
| An explicit `REINDEX` moves the build off the first query | `crates/inlaysql-core/tests/reindex.rs` | every push |
| The paged BM25 index scores bit-identically to the in-memory one | `crates/inlaysql-core/tests/bm25_paged_agreement.rs`, `crates/inlaysql/tests/paged_full_text.rs` | every push |
| A blocking operator refuses rather than exhausting memory | `crates/inlaysql-core/tests/query_memory.rs`, `crates/inlaysql-server/tests/streaming_memory.rs` | every push |
| A statement too large for one WAL region is refused, not half-applied | `crates/inlaysql/tests/large_statements.rs` | every push |
| `:memory:` is refused rather than taken as a filename | `crates/inlaysql/tests/memory_path.rs` | every push |
| ANN recall does not slide as the corpus grows, **per metric** | `crates/inlaysql-core/src/hnsw.rs` | every push (the 25,600-node case nightly) |
| A vector index's distance metric, end to end | `crates/inlaysql-core/tests/vector_metrics.rs`, `crates/inlaysql/tests/vector_metrics.rs` | every push |
| Raising `ef_search` really does raise recall, and the reported `ef` is the enforced one | `crates/inlaysql/tests/ef_search.rs`, `crates/inlaysql-core/src/hnsw.rs` | every push |
| Cross-backend equivalence (blocking / io_uring / memory) | `crates/inlaysql/tests/backends.rs` | every push |
| Format portability (native ↔ WASM) | `crates/inlaysql-wasm/tests/portability.rs` | every push |
| The demo page in headless Chromium, incl. OPFS | `crates/inlaysql-wasm/browser/smoke.mjs` | every pull request |
| The engine on an edge runtime (`workerd`) | `crates/inlaysql-wasm/edge/smoke.mjs` | every pull request |
| MCP wire protocol | `crates/inlaysql-mcp/tests/client.rs` | every push |
| Benchmarks vs SQLite and sqlite-vec | `bench/run.sh` | nightly, results published |
| Benchmarks vs DuckDB and pgvector | `bench/compare.sh` | nightly where Docker exists |

Four workflows, because they answer different questions.
[`ci.yml`](.github/workflows/ci.yml) answers "is this change correct enough to
merge": `fmt`, `clippy`, the workspace tests, the SQL Logic Tests and the
`no_std` guards, on every pull request, in minutes.
[`wasm.yml`](.github/workflows/wasm.yml) answers "does it still run everywhere"
— it builds `wasm32`, drives the browser demo, runs the engine on an edge
runtime and publishes the demo from `main`.
[`trust.yml`](.github/workflows/trust.yml) answers
"what is true about `main` right now": it fuzzes, runs the long logic-bug
campaign and the benchmarks on every push to `main` and nightly, and uploads
the results as downloadable artifacts (`fuzz-status`, `benchmark-results`).
[`release.yml`](.github/workflows/release.yml) runs on a version tag and
packages the CLI and the WASM module into a GitHub Release.

The split that matters is fast versus slow. A reviewer waits for `ci.yml` and
nothing else, so the 10,000-schedule sweeps only run there on `main`, on a tag, or on
a pull request that opts in with the `full-ci` label — worth adding to anything
touching recovery, the write-ahead log or the storage format. The fuzzing
campaign, the 50,000-round differential run and the benchmarks are in
`trust.yml`, which gates nothing: it is allowed to take an hour and it is
allowed to go red, because a fuzz crash should be loud without blocking a merge.

Until `AHL-369` these workflows had never passed at all — the self-hosted runner
had no `rustup`, so every job died on its first step. It installs one now
(`.github/actions/rust`), which is why per-pull-request checks are back.
Everything on this page also reproduces locally with the commands given, which
is the property that actually matters.

## Instruments, not assertions

Three `#[ignore]`d tests exist to *measure* rather than to pass. They print a
table and assert only that the thing under test still answers, because a timing
threshold in CI fails on a busy machine and teaches everyone to ignore it.

```sh
cargo test --release -p inlaysql --test index_memory_cost -- --nocapture --ignored
cargo test --release -p inlaysql-core --test vector_query_cost -- --nocapture --ignored
cargo test --release -p inlaysql-core --test bm25_skipping_headroom -- --nocapture --ignored
```

They are how several published figures were corrected rather than argued about:
`index_memory_cost` found this project's own 10M-vector estimate understated by
2.3x, `vector_query_cost` established that the distance kernel is already
vectorised and that fusing it to `fmla` would buy 4% of a query while changing
what the index computes, and `bm25_skipping_headroom` is the measurement that
made block-max WAND get built, measured and then reverted.

## Deterministic simulation testing

The credibility centrepiece. `inlaysql-core` is `no_std`, so it cannot read a
clock or touch a file even by accident; everything it needs arrives through
traits. That is what makes it possible to run the *entire* engine against a
simulated disk that crashes, tears writes and reorders syncs on a schedule
derived from a seed.

```sh
cargo test --release -p inlaysql-core --test dst_sweep -- --ignored          # 10,000 seeds
cargo test --release -p inlaysql --test index_recovery_dst -- --ignored      # 10,000 schedules (2,500 seeds × 4 shapes)
cargo test --release -p inlaysql-core --test free_list_reuse_dst -- --ignored # 5,000 seeds
cargo test --release -p inlaysql-core --test backup_dst -- --ignored          # 5,000 seeds
```

`free_list_reuse_dst.rs` (AHL-481) is the same assertion over a fourth
device, purpose-built because the other two never actually exercise page
reuse: `Simulator`, as `dst_sweep.rs` and `index_recovery_dst.rs` use it,
answers `None` for both `Device::commit_point` and `Device::min_reader_seq`,
which is deliberately "unknown, so never reclaim" — the free list only draws
a candidate page id once a device can prove the freeing commit is durable
and no reader it can see is pinned to an older root. Its own `TrustedDevice`
gives an honest answer to both, over the same fault schedule, so this sweep
is the one place page id reuse itself is under DST rather than merely
compiled. A fast 300-seed pass runs in `cargo test --workspace`; the
5,000-seed pass above is `--ignored`, the same way `dst_sweep.rs` and
`index_recovery_dst.rs` are, and runs beside them in `ci.yml`'s `sweep` job
and in `docker/test.sh`'s `sweep`/`all` targets.

This paragraph used to say the opposite — that the 5,000-seed pass ran only
when somebody typed it by hand. That was half right and is recorded because
the half that was wrong is the dangerous kind: `ci.yml` had gained the step
("Sweep page reuse") without this file being updated, while `docker/test.sh`
genuinely had not, so the script that exists to reproduce CI's sweep job did
not reproduce it. Both are now the same three steps in the same order.
`crates/inlaysql/tests/free_list_growth.rs` checks
the property that motivates it directly rather than under fault injection: a
sustained write/delete/write/checkpoint workload stops growing the file once
`CowBTree::set_page_reuse(true)` is on, where the same workload with reuse
off — still the default — grows it without bound. The same file also proves
this through the public surface now, not just the storage layer directly:
`EngineOptions::page_reuse` reaches `CowBTree::set_page_reuse` through
`Database::open_on_with_options`, and a second test drives the identical
churn shape through ordinary `CREATE TABLE`/`INSERT ... ON CONFLICT DO UPDATE`/`DELETE`
rather than `CowBTree::put`/`delete`. `crates/inlaysql/tests/vacuum.rs` covers
whole-file compaction (`inlaysql vacuum <path>`) the same way: a real schema
covering every reconstruction shape survives with its data, its constraints
and its query behaviour intact, and the file measurably shrinks.

`backup_dst.rs` is the fourth sweep, and it asserts something the other three
cannot: not "the recovered state is *some* committed snapshot" but "the copy is
*exactly* the snapshot it was taken from". A backup is allowed no latitude —
it is taken from a root the handle is holding, and `&self` is what stops that
root moving — so the assertion is equality with the map the workload committed
at that instant, mid-workload after every fourth commit and once more after the
schedule's crash has been recovered from with `CowBTree::open`. That last one
is the composition nobody would write on its own: backing up a database that
has just come back from a crash. The failure this is really for is a *missed*
page — a subtree or a link in an overflow chain the walk did not follow — which
does not fail loudly: the copy opens and answers a query with a hole in it.
Removing either the overflow-chain walk or the leftmost-child push makes this
sweep fail on its first seed, which is how the assertion was checked for teeth.
`crates/inlaysql/tests/backup.rs` covers what a simulated disk cannot: a bank
transfer whose committed states are enumerable in closed form, copied while
another handle commits on another thread, while another *process* holds the
write lock (the live-server case, and the only path through
`inlaysql::backup`'s lock-free fallback), and while a writer with page reuse on
is demonstrably recycling pages — the last of which fails if the reader
watermark stops pinning, which is the whole of why backup is sound beside
reclamation. See
[the free list in `docs/recovery.md`](docs/recovery.md#the-free-list-and-page-reuse-phase-2-item-6-ahl-481)
for the design these hold to, and what remains true even now that the option
is public: reclamation can only prove liveness for readers this process's
reservation gate can see, so it is still unsound to enable beside a
concurrent `FileDevice::open_read_only` on the same file.

The assertion is **not** "everything we wrote is still there" — a crash is
supposed to lose the last commit. It is:

- **Rows:** the recovered database is byte-for-byte one of the states the
  workload actually committed. Never a mix of two, never a torn page.
- **Indexes:** every row the recovered database can scan is a row its indexes
  can find, and nothing else is. A stale index that outlived a rolled-back
  commit fails this immediately.

Every decision — the workload and the fault schedule — is a pure function of
the seed, so a failure reproduces exactly on any machine.

**Not covered:** reordered syncs are excluded from the sweep. They interact
with log truncation at a checkpoint in a way documented in
[`docs/recovery.md`](docs/recovery.md) as a hardening follow-up; recovery
detects the inconsistency rather than silently corrupting, but does not yet
reconstruct those commits.

### Format versions

The catalog carries its own on-disk version, separate from the B-tree's page
format. Version 2 added index declarations, 3 the `VECTOR(n, INT8)` tag, 4 —
AHL-412 — declared constraints and the `NUMERIC` affinity, 5 — AHL-423 —
scalar B-tree indexes (the first index declaration that can name more than
one column and the first that can be `UNIQUE`), and 6 — AHL-469 — declared
collations, on a column and on each column of an index.

Two rules keep a version bump from being a trap, and both are asserted in
`catalog.rs`'s own tests:

- **A catalog is written at the lowest version that can express it.** A
  database with no constraints and no `NUMERIC` column is still written as
  version 2, so opening and editing it does not make it unreadable to the build
  that created it. Only a table that actually uses a new feature forces the
  higher version — a `NUMERIC` column or a constraint forces 4, a B-tree index
  forces 5, and a declared collation that is not `BINARY` forces 6.
- **An older build refuses a newer catalog, and reads nothing.** The version
  check happens before the table section is decoded, so a build predating the
  catalog's version fails with `Error::FormatVersion` rather than parsing the
  tables and silently losing what follows them. That matters most at version
  6: a `NOCASE` index keys the *folded* value, so a build that decoded the
  index declaration without its collation would probe the unfolded bytes,
  find nothing, and answer `WHERE name = 'ADA'` with no rows while the table
  still held one — the same failure version 5 already guarded against for a
  B-tree index's declaration. Pre-1.0 the policy is *recreate, not migrate*;
  that is only safe if the failure is loud.

A catalog encoding change is squarely in DST territory — the catalog is
metadata in the same tree as the rows, and it is written inside the same
transaction — so both sweeps are a required gate for one, not an optional
extra.

## SQL compatibility

Measured against SQLite's
[SQL Logic Test](https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki)
corpus, in the standard format.

```sh
cargo test -p inlaysql --test sqllogictest                        # fail on any mismatch
cargo run -p inlaysql --bin sqllogictest -- \
  crates/inlaysql/tests/sqllogictest/*.test                       # print the pass rate
```

**1094/1094 (100%)** — over a *curated subset*, and the size of that subset is the
honest caveat. It covers what the dialect implements: `CREATE TABLE`, `INSERT`,
projection, `WHERE`, `ORDER BY`, `LIMIT`, type coercion, scalar expressions,
`UPDATE`/`DELETE`, `INTEGER PRIMARY KEY`, three-valued logic, `INNER JOIN` and
`LEFT JOIN`, the aggregate functions (`COUNT`, `SUM`, `MIN`, `MAX`, `AVG`) with
`GROUP BY` and `HAVING`, and — since AHL-410 — `LIKE`, `IN`, `BETWEEN`, `CASE`,
`CAST`, `||` and blob literals. Since AHL-411 it also covers the scalar function
library (`length`, `upper`, `lower`, `substr`, `trim`/`ltrim`/`rtrim`,
`replace`, `instr`, `abs`, `round`, `coalesce`, `ifnull`, `nullif`, the
two-argument `min`/`max`, `hex`, `random`), the date/time family (`date`,
`time`, `datetime`, `strftime`, `unixepoch`, `CURRENT_TIMESTAMP`), `DISTINCT`,
multi-key `ORDER BY` with `NULLS FIRST`/`NULLS LAST`, `OFFSET`, `LIMIT ?` as a
bound parameter, `COUNT(DISTINCT x)` and `GROUP_CONCAT`. Since AHL-412 it covers
the five files that phase added — `constraints.test`, `ddl.test`,
`write_statements.test`, `returning.test`, `affinity.test` — which is where the
subset roughly doubled: type affinity, `DEFAULT`, `NOT NULL`, `CHECK`, `UNIQUE`,
recorded foreign keys, `DROP TABLE`, `CREATE TABLE IF NOT EXISTS`, the four
`ALTER TABLE` operations, `INSERT ... SELECT`, the conflict clauses,
`RETURNING`, and `BEGIN`/`COMMIT`/`ROLLBACK`. Since AHL-423 it covers
`btree_index.test`; since AHL-464 `index_join.test` — the index nested-loop
join, including the shapes the planner rule declines, so the file pins the
*answers* rather than the access path; and since AHL-463 `subqueries.test`:
scalar `(SELECT ...)`, `IN (SELECT ...)`, `EXISTS`, derived tables
(`FROM (SELECT ...)`), the correlated form of each, and the nesting of one
inside another. Since AHL-469 it covers `collation.test`: `BINARY`, `NOCASE`
and `RTRIM`, the three collating sequences SQLite has. Since AHL-473 (Phase
1c items 2 and 3) it covers `set_operations.test` — `UNION`, `UNION ALL`,
`INTERSECT` and `EXCEPT`, including a compound chain mixing `INTEGER` and
`TEXT` arms — and `ctes.test`: non-recursive `WITH`, a CTE referencing an
earlier one in the same list, a CTE joined against itself, and a CTE
shadowing a real table. It still does not include `WITH RECURSIVE`, which
the dialect refuses on purpose (`unsupported.test`) rather than not having
parsed at all — see [`docs/server.md`](docs/server.md#what-does-not-work-yet)
for the narrower compound- and CTE-shaped ground that stays refused
alongside it. Since AHL-490 it covers `json.test`: SQLite's json1 family —
`json_extract`, `->`/`->>`, `json_valid`, `json_type`, `json_quote`,
`json_array`, `json_object`, `json_array_length`, `json_set`/`json_insert`/
`json_replace`/`json_remove` and `json()` — including a missing path, a path
into an array, a path into a scalar, an invalid path expression erroring
rather than answering `NULL`, `NULL` input, and the composition rule that
splices a nested `json_object()`/`json_extract()`/`->` call as raw JSON
instead of stringifying it. `json_each`/`json_tree` (table-valued — no
mechanism for a function that returns rows in `FROM`) and `json_patch` (not
implemented) are pinned as refusals in `unsupported.test` instead, not
included in this file's count. Since AHL-494 it covers
`window_functions.test`: the ranking family (`row_number`, `rank`,
`dense_rank`, `ntile`), `lag`/`lead`, `first_value`/`last_value`/`nth_value`,
the aggregate family `OVER (...)`, `ROWS` frames and SQLite's implicit default
frame, named windows and `FILTER (WHERE ...)` — with `percent_rank()`,
`cume_dist()` and explicit `RANGE`/`GROUPS` frames pinned as refusals in
`unsupported.test`. The number to watch is the subset growing, not the
percentage staying at 100.

Every expected value in `scalar_functions.test`, `datetime.test`,
`distinct.test`, `order_by_paging.test`, `constraints.test`, `ddl.test`,
`write_statements.test`, `returning.test`, `affinity.test`, `collation.test`,
`set_operations.test`, `ctes.test`, `json.test`, `window_functions.test`,
`index_join.test` and `subqueries.test` were produced by
running the same SQL through SQLite, not by recording what InlaySQL printed. An
expectation copied from the engine under test passes whatever that engine
happens to do, which is not a test of anything. For AHL-412's five files that
included the *refusals*: each `statement error` was replayed through the
`sqlite3` binary to confirm SQLite refuses it too, so the file records the
dialect rather than this engine's gaps.

Every constraint in `constraints.test` is asserted in both directions: the
violation is rejected, **and** the table is unchanged afterwards. Only the
second half catches the failure that matters. A constraint that raises the
right error and writes the row anyway passes a test that looks only at the
error, and the bug it hides — a partially-applied statement — is invisible
until something reads the table.

One file in the subset, `unsupported.test`, asserts *refusals* rather than
results. It exists because the failure it guards against is worse than a
missing feature: `INSERT ... ON CONFLICT`, `INSERT OR REPLACE`, `RETURNING` and
every `CREATE TABLE` constraint (`DEFAULT`, `NOT NULL`, `UNIQUE`, `CHECK`,
`REFERENCES`) used to parse and then be silently discarded, so a statement
reported success while quietly doing something else.

AHL-411 widened that file to the rest of the same bug class. A `WITH` clause
used to parse and be dropped, so the main `SELECT` ran and the CTE it was
written to use did nothing; `QUALIFY`, `WINDOW`, `CLUSTER BY`, `SORT BY`,
`DISTRIBUTE BY`, `PREWHERE`, `LATERAL VIEW`, `SELECT ... INTO` and `FETCH` were
all read and ignored the same way. `ORDER BY 1` was worse than ignored — it
planned as the constant `1` and sorted by nothing while reporting success.
`UNION`, `INTERSECT`, `EXCEPT`, subqueries in every position, and an unknown
scalar function were all refused explicitly, and `unsupported.test` held a
record for each. (Subqueries have since landed — AHL-463 — and their records
moved to `subqueries.test`; what stayed behind is the subquery ground that is
still refused, listed below.) A function nobody implemented must not answer `NULL`: that is
the same silent lie as a dropped clause, and harder to notice.

AHL-412 emptied most of that file by implementing what was in it, which is what
it was there for. What is left is what is still refused *on purpose*, each with
a reason rather than a to-do: `INSERT OR ROLLBACK`/`OR FAIL` and
`UPDATE OR ...`, because they promise something about partial writes and a
statement here is already atomic; `SAVEPOINT`, because it is a nested rollback
point and the storage engine buffers a transaction as one set of writes;
`WITHOUT ROWID`,
`STRICT`, `TEMPORARY` and `CREATE TABLE ... AS SELECT`, because they are
storage layouts this engine does not build; and an `ON CONFLICT` target naming
no uniqueness constraint, which would make the clause unreachable.

`COLLATE` was on that list until AHL-469 and is a real clause now, with the
three collating sequences SQLite has — `BINARY`, `NOCASE` and `RTRIM`. What
stays refused is a collation *name* this engine does not have, because there is
no `CREATE COLLATION` to add one with and accepting `utf8mb4_unicode_ci` would
mean comparing byte-wise under a name that promises otherwise. Also refused: a
`COLLATE` inside a `UNIQUE` or `PRIMARY KEY` column list, and a
`CREATE UNIQUE INDEX` keyed under a collation its column did not declare —
either would leave the two paths that enforce a uniqueness constraint (an index
probe when one covers it, a scan when none does) disagreeing about what a
duplicate is. Declaring the collation on the column keeps them in step.

The collation work has its own three layers, and they are worth naming because
the middle one is where this class of bug actually lives. `collation.test`
holds 71 records whose expected answers came out of the sqlite3 binary, not out
of this engine. `btree_index.rs` runs every collated shape twice — once over an
indexed table and once over an unindexed one — because a `NOCASE` index keys
the *folded* value, so a probe and a scan read different bytes for the same
query and only the comparison proves they mean the same thing. And
`differential.rs` generates `COLLATE` in DDL and in expressions and asks all
three (indexed, unindexed, SQLite) the same question. That third layer found
something on its first run worth writing down: **which spelling comes back from
a `DISTINCT` or `GROUP BY` over a case-insensitive collation is not
determined** — SQLite's own answer changes with the access path — so the test
compares the equivalence classes and the counts exactly and the representative
string folded.

AHL-463 added one more group, and it is worth saying plainly why: **a subquery
in a write statement is refused, at prepare time.** `UPDATE`, `DELETE` and
`INSERT ... VALUES` build their expression environment and then take the engine
mutably to write, so that environment cannot hold the shared borrow reading a
subquery needs. Refusing when the statement is planned is the difference
between a clear error and one raised halfway through a statement that had
already written rows. The query of an `INSERT ... SELECT` is *not* refused: it
runs to completion through the ordinary read path before any row is written.
The same section of `unsupported.test` also pins the narrower subquery
refusals — a subquery of the wrong width, a column alias list on a derived
table, a correlated derived table (SQLite has no `LATERAL`), a retrieval
function over a subquery's rows, a correlated `LIMIT`, and an `ORDER BY` or
`LIMIT` written outside a parenthesised query.

### Three known divergences from SQLite

The integer-overflow one is gone: AHL-412 made `+`, `-`, `*`, `/` and unary
minus promote to REAL on overflow the way SQLite does, and `SUM` raise
"integer overflow" rather than wrap, so `CAST(1e300 AS INTEGER) + 1` now agrees
and `differential.rs` generates it again. What is left, each named in
`differential.rs` where the grammar would otherwise produce it rather than
quietly left out:

- **Rendering a REAL as TEXT.** SQLite decodes floats with its own routine
  rather than the platform's `printf`, so that its output does not depend on
  libc — and that routine disagrees with a correctly-rounded conversion in the
  last digit. `CAST(1.0/3.0 AS TEXT)` is `0.33333333333333332` in SQLite,
  `0.333333333333333` here. Matching it means porting the decoder, not picking
  a precision.
- **Columns are typed, not merely affine.** SQLite's affinity is a
  *preference*: a value that does not convert is stored as it is, so
  `INSERT INTO t(n INTEGER) VALUES (2.5)` keeps the real and
  `INSERT INTO t(b TEXT) VALUES (X'00')` keeps the blob. Here, four of the five
  affinities are enforced as types and a value that does not fit is an
  `Error::Type`. The fifth, `NUMERIC` — the affinity every unrecognised type
  name resolves to, and so the one a framework's migrations meet most often —
  *is* faithful: it converts what converts and stores the rest unchanged. This
  is why the differential generators stay type-consistent, and it predates the
  affinity work; making it visible is what AHL-412 changed, not the behaviour.
- **The row-id counter is one counter per database, not per table.** SQLite
  assigns a new key from the highest in *that* table (or, with
  `AUTOINCREMENT`, from `sqlite_sequence`); InlaySQL keeps a single monotonic
  counter for the whole file, so the first key in a second table is one past
  the highest key assigned anywhere. The value is still the row's real key and
  `RETURNING id` still reports it truthfully — it is simply not the number
  SQLite would print. `returning.test` is its own file for this reason. The
  counter does match SQLite's `AUTOINCREMENT` in the ways that are about
  *behaviour* rather than numbering: it never reuses a key after a delete, a
  row a conflict clause skips still uses one up, and a statement that fails
  keeps none.

The first one has a second consequence worth stating, because it shapes the
generator rather than just the results: any function that renders a REAL as
text inherits it. So the grammar keeps the REAL column out of `length`, `hex`,
`upper`, `substr`, `instr` and `||`, and feeds it only to `abs`, `round`,
`min`, `max` and `coalesce`, which return a number. That is the same divergence
being avoided once, not six new ones.

Foreign keys are **not** on this list, because they are not a divergence:
SQLite has shipped with enforcement off by default since 3.6.19 and every
framework's migrations are written for that. InlaySQL records the declaration
in the catalog and does not enforce it, which is the same behaviour under the
same default. `constraints.test` asserts both halves — the declaration
survives, and a row that violates it is accepted.

Two date modifiers are refused rather than implemented, which is a deliberate
gap rather than a divergence: `localtime` and `utc` need the host's timezone
database, and `inlaysql-core` is `no_std` and cannot read one. Treating them as
UTC would be a wrong answer; `Error::Unsupported` is an honest one.

## Logic bugs

A crash announces itself; a `WHERE` clause that silently drops a row does not.
[SQLancer](https://github.com/sqlancer/sqlancer) solved that by comparing a
database against itself, and `logic_bugs.rs` applies the two techniques that
fit this dialect:

- **TLP** — every row satisfies exactly one of `p`, `NOT p`, `p IS NULL`, so
  those three result sets must partition the table. Checked over 300 random
  tables and predicates.
- **Row retrieval** — a row known to exist, and a predicate true of it by
  construction, must come back.

Writing these found that the dialect had neither `NOT` nor `IS NULL`: the
property could not be expressed without them. Both were added.

Comparing a database against itself has a blind spot: a predicate that is
consistently wrong. If `a > 5` and `NOT (a > 5)` both mishandle `NULL` the same
way, the partition still holds and the test still passes. Catching that needs a
second implementation, and the project already names one — the dialect's stated
baseline is SQLite compatibility, so SQLite *is* the specification.

```sh
cargo test -p inlaysql --test differential                          # 200 rounds
INLAYSQL_DIFFERENTIAL_ROUNDS=50000 cargo test --release -p inlaysql --test differential
```

`differential.rs` generates a random table — integers, text and `NULL` in both
— and a random predicate over `AND`, `OR`, `NOT`, `IS NULL` and comparisons,
then requires both engines to return the same rows. **50,000 seeds agree.** The
same file also generates random join keys for two tables (`INNER JOIN` and
`LEFT JOIN` compared as row sets) and random group rows for the aggregate
functions (`COUNT`, `SUM`, `MIN`, `MAX`, `AVG` over `GROUP BY`). Since AHL-464
each generated join is run three times over the same rows and the same `ON` —
inner side materialised, probed through a B-tree index, and probed by
`INTEGER PRIMARY KEY` — so SQLite is the oracle for the index nested-loop join
as well as for the scan, and a probe that loses, invents or reorders a row is a
failure rather than a faster wrong answer. **20,000 seeds agree**, which is
120,000 join comparisons. The
generator stays mostly type-consistent on purpose — a mismatch would usually
say more about the generator than about either engine — **with one
deliberate exception (AHL-486):** ten of `leaf()`'s arms compare the
`INTEGER` column against a `TEXT` literal and the `TEXT` column against an
`INTEGER` one, on purpose, because this is the shape 50,000 rounds never
generated before it and so never caught the missing comparison-affinity
conversion — the grammar could not express `WHERE <typed column> <op>
<literal of another storage class>` at all. SQLite is still the oracle for
what the answer should be; the generator only guarantees the shape gets
rolled.

AHL-411 added four more generators, and AHL-412 a fifth, run over the same
seeds:

| Generator | What it varies |
| --- | --- |
| `scalar_expressions_agree_with_sqlite` | now half operators, half scalar function calls, over a table with a REAL column, a date column that is sometimes not a date at all, and (since AHL-490) a JSON column drawing from a small pool of always-valid documents — this suite's contract is that both engines *succeed*, so the malformed-JSON/bad-path refusals are pinned in `json.test` instead, where an expected error is a first-class outcome |
| `query_shape_agrees_with_sqlite` | `DISTINCT`, multi-key `ORDER BY` with `ASC`/`DESC` and `NULLS FIRST`/`NULLS LAST`, `LIMIT`, `OFFSET`, and `LIMIT ?` bound as a parameter |
| `distinct_aggregates_and_group_concat_agree_with_sqlite` | `COUNT(DISTINCT x)`, `SUM(DISTINCT x)`, `GROUP_CONCAT` with and without a separator |
| `fixed_scalar_functions_...` / `fixed_date_and_time_functions_...` | fixed lists rather than random draws, for the edges a random walk rarely hits twice |
| `constrained_writes_agree_with_sqlite` | twelve random writes against a table carrying every constraint kind: plain inserts, the conflict clauses, upserts with and without a `WHERE`, updates, deletes and `RETURNING` |

AHL-463 added four more, over two tables so a subquery has somewhere else to
read from:

| Generator | What it varies |
| --- | --- |
| `subquery_values_agree_with_sqlite` | a subquery *projected* as a value: aggregated and `ORDER BY ... LIMIT 1` scalar subqueries, `EXISTS`/`NOT EXISTS`, `IN`/`NOT IN`, each with a correlated or uncorrelated inner `WHERE` |
| `subquery_predicates_agree_with_sqlite` | the same forms in a `WHERE`, plus one that nests a subquery inside a subquery whose innermost level names a column two levels out |
| `negated_subquery_predicates_agree_with_sqlite` | those predicates under `NOT`, which is where a `0` returned in place of a `NULL` stops being invisible |
| `derived_tables_agree_with_sqlite` | `FROM (SELECT ...)` projected, filtered, aggregated, joined in both positions, and nested inside another derived table |

Two deliberate limits on that grammar, both for the same reason the rest of the
file stays type-consistent. Every generated subquery is either aggregated or
`ORDER BY`-ed and limited to one row: a scalar subquery returning several rows
is not an error in SQLite — it takes the first — so an unordered multi-row one
would be comparing two engines' scan order rather than their semantics.
`subqueries.test` pins that first-row rule directly instead, where the order is
fixed by construction. And a derived table is never given a correlated filter,
because it cannot have one: SQLite has no `LATERAL`.

AHL-473 added two more, reusing the subquery generators' two-table setup:

| Generator | What it varies |
| --- | --- |
| `compound_queries_agree_with_sqlite` | `UNION`, `UNION ALL`, `INTERSECT` and `EXCEPT` chained two or three deep, either arm shape or both, across an `INTEGER`/`TEXT` class boundary |
| `cte_queries_agree_with_sqlite` | a single CTE, one CTE referencing an earlier one, a CTE joined against itself, a CTE shadowing the real table `t`, and a CTE whose own body is a compound |

`compound_queries_agree_with_sqlite` used to keep every arm one storage
class, the same discipline the rest of this file still follows, until AHL-477
fixed the bug that made mixing them unsafe to generate: `engine.rs`'s
`compare_values` — what a compound's dedup sorts by — answered "equal" for a
class pair it had no rule for instead of ranking it, which would have made a
mixed-class `UNION` compare wrong against SQLite for a reason that was the
comparator's fault, not the generator's. Fixed, both engines rank a
cross-class pair by SQLite's fixed storage-class order now (`NULL` < numbers
< `TEXT` < `BLOB`, `mem_cmp` in `eval.rs`), so the generator mixes
`INTEGER`/`TEXT` arms in one chain on purpose — the shape most likely to
catch a class-order regression — and 20,000 seeds agree. That fix is pinned
directly, not only through the generator: `mem_cmp_is_a_total_order_over_every_storage_class`
(`eval.rs`) and `compare_values_is_a_total_order_over_every_storage_class`
(`engine.rs`) each check reflexivity, antisymmetry and transitivity
exhaustively over a corpus spanning every storage class, under every
collation — a complete proof over that corpus, not a sampled one, and cheap
enough to run on every `cargo test`. Between them they cover the five call
sites a wrong total order can corrupt: `ORDER BY`, `GROUP BY`, `DISTINCT`,
set-operation dedup and `MIN`/`MAX`, which used to be four independent
copies of the same comparison and are one implementation now.

The last one is a different shape of test from the rest, because a constraint
is a different shape of thing. Every other generator compares what a `SELECT`
*reads*; a constraint has no value to project, only a decision about whether a
row is allowed, so the evidence is what a sequence of writes *leaves behind*.
Each round compares three things: which statements were accepted, what each
`RETURNING` gave back, and the whole table at the end. Error *messages* are not
compared — two engines phrasing the same refusal differently is not a
disagreement about behaviour — but which statements errored is.

It earned its place immediately. It found three things nobody had guessed, all
of them behaviour a hand-written test would have asserted wrongly:

- **An `ON CONFLICT (x)` target narrows which conflicts the clause answers
  for.** A row that collides on some *other* unique column is an ordinary
  violation, not an upsert. Where a row collides on both, the clause acts on
  the constraint it named and leaves the other alone.
- **`INSERT OR IGNORE` and `ON CONFLICT DO NOTHING` are not the same clause.**
  `OR IGNORE` is a conflict-resolution algorithm and SQLite applies it to every
  constraint, so a row failing a `CHECK` is skipped; `DO NOTHING` is the upsert
  clause and covers uniqueness only, so the same row is an error. Relatedly,
  `OR REPLACE` on a `NOT NULL` column substitutes the column's default rather
  than replacing a row, and does not absorb a failed `CHECK` at all.
- **A skipped row still uses up its key.** The row id is reserved when it is
  *resolved*, not when the row is written, so an `INSERT OR IGNORE` that skips
  one row makes the next assigned key skip a number.

The table it writes to declares `AUTOINCREMENT`, which is not decoration: a
plain SQLite row id reuses the highest key after a `DELETE` and this engine's
counter does not, so without it the two engines would disagree about assigned
keys for a reason that has nothing to do with what is being tested.

Every generated query in the query-shape test names a *total* order, ending in
the primary key. Without that, two engines can return different rows for the
same `LIMIT 3` and both be right, and the test would measure tie-breaking luck
rather than correctness. `GROUP_CONCAT` gets the same treatment from the other
side: SQLite documents the order of the concatenated values as arbitrary, so the
result is split and sorted before comparing — which still catches a dropped
value, a counted `NULL`, a wrong separator or a group that concatenated the
wrong rows.

`'now'` is deliberately absent from the date generators. The two engines read
different clocks, so comparing them would be a race rather than a test. The
clock path is covered instead by `inlaysql-core/tests/nondeterminism.rs`, which
injects a fixed reading through the `Clock` trait and asserts the formatted
result exactly — and asserts that `random()` replays from a seeded `Rng`, is
evaluated per row rather than folded to a constant, and that one statement sees
one instant. Those are the properties the DST replays rest on, so they are
tested directly rather than assumed.

**Not covered:** this is not SQLancer. SQLancer is a Java tool with years of
generator tuning, and it drives a database over JDBC — which InlaySQL, a
library with no wire protocol, does not have. That is the blocker, and
[`docs/sqlancer.md`](docs/sqlancer.md) states what a bridge would cost, what
SQLancer would add over what is here, and how a logic-bug finding is triaged
when one turns up.

## Fuzzing

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run sql_parser  -- -max_total_time=300
cargo +nightly fuzz run storage     -- -max_total_time=300
cargo +nightly fuzz run row_codec   -- -max_total_time=300
cargo +nightly fuzz run json_parser -- -max_total_time=300
```

Four targets:

| Target | Property |
| --- | --- |
| `sql_parser` | Arbitrary text always returns a `Result`. Reachable by anything that can send a query string — for an MCP server, a language model. |
| `storage` | Arbitrary bytes as a database image are rejected, not believed; and if one opens, reading it does not panic. |
| `row_codec` | Every decoder of persisted bytes (rows, catalog, BM25, HNSW) rejects garbage. |
| `json_parser` (AHL-490) | Arbitrary text through the hand-rolled JSON parser and path parser (`crates/inlaysql-core/src/json.rs`) always returns a `Result`, and a document it accepts round-trips through its own serializer — parsing the output again gives the identical value, and that is already a fixed point. |

`cargo-fuzz` needs nightly and a time budget, so it does not run on every push.
It runs **nightly and on every push to `main`**, five minutes per target, in
[`trust.yml`](.github/workflows/trust.yml); a longer campaign is one
`workflow_dispatch` away with the `fuzz_seconds` input. Every run uploads a
`fuzz-status` artifact holding each target's log, a one-line verdict per
target, and any crashing input libFuzzer found. A crash does not stop the other
targets: a run that reports one and hides two is worse than one that reports
three.

Between campaigns, `fuzz_smoke.rs` runs the same properties over a few thousand
seeded inputs on stable in every `cargo test`, so a target that stopped
compiling — or a property that quietly stopped being true — is caught on the
push that did it. It will not find what a coverage-guided fuzzer finds; it
keeps the targets honest in between.

A crash a real campaign already found is kept, not just fixed: `fuzz_regressions.rs`
vendors the exact bytes two `main`-branch `Trust` runs crashed on
(`crates/inlaysql-core/tests/fuzz-regressions/`) as permanent tests, on the
same reasoning as the exact-bytes DST replays above — a fixed crash that is
only described in prose, not pinned as a test, is a crash that can come back
silently. This is not the "committed corpus" the note below is about: these
two inputs are regression tests `cargo test` runs on every push, not seeds
`cargo-fuzz` reads from at the start of a campaign.

**Not covered:** no corpus is committed for `cargo-fuzz` itself, so every
campaign starts from nothing and re-derives the same shallow coverage before
it gets anywhere interesting. Committing a minimised corpus is the obvious
next step and has not been done.

## Concurrent writers

```sh
cargo test -p inlaysql --test concurrent_writers
```

Several writers on one file settle races by first-committer-wins, and the
property under test is not that the winner wins — it is that **the loser is
told**. Until `Error::Conflict` existed, the tree reported the lost race and
the layer above discarded the report, so `execute` returned `Ok` for a
transaction that had just been rolled back: ten inserts across two handles,
five rows in the file, no error anywhere. Three tests now pin it — the loser
gets an error, the conflicted handle still works, and a retry loop over four
writers loses no writes.

The concurrency benchmark (`SUITE=concurrency ./bench/run.sh`) re-checks the
same property on every run, and reports what optimistic concurrency currently
costs. It used to cost a great deal: commit compared the *root* it last saw
rather than write sets, so about half of all transactions aborted even though
the writers touched disjoint rows. `rebase_pending` closed that — a transaction
whose keys do not overlap the winner's is rebased onto the newer root instead
of being thrown away — and the benchmark now reports 0.0% aborts at 1, 2, 4 and
8 writers on its disjoint-row workload. Writers that genuinely overlap still
conflict, which is the point.

What is left is not aborts but `fsync`: eight writers do about 1.45x the work
of one, not eight times. `bench/README.md` has the numbers.

**Covered:** writers on separate OS threads, not just interleaved on one.
`parallel_file_handles_commit_disjoint_rows_without_false_conflicts` opens four
independent `Database::open` handles — four separate `Rc<RefCell<…>>` devices
on the same file, not one shared between threads — on four real OS threads,
each hammering disjoint rows and retrying on `Error::Conflict`. It asserts the
retries rebase cleanly (zero conflicts on genuinely disjoint writes), the full
row set lands, and CDC versions stay a gap-free commit order across threads.

**Covered:** a long-lived reader beside a writer does not go stale and stay
stale — a handle refreshes its snapshot at the start of every statement
outside an explicit transaction (AHL-400, cheap in the common case since
AHL-403: it answers from a commit counter the device already tracks rather
than reading the log, unless something actually moved). Four tests pin it:
`a_reader_opened_before_a_commit_sees_the_row_afterwards` (a reader taken
before the table had any rows still sees a writer's later commits, statement
by statement, with no reopen and no write of its own),
`a_reader_picks_up_a_table_another_handle_created` (the refresh has to
notice a catalog change as well as a row change, or a server connection
would answer "no such table" for a table its own migration had just created
on another connection), `a_handle_inside_a_transaction_keeps_its_pinned_snapshot`
(every statement inside an explicit transaction reads the state it was
pinned at, however much is committed elsewhere meanwhile — a pinned snapshot
is the point of `BEGIN`, not a gap) and `a_rolled_back_transaction_also_releases_the_pin`.

**Not covered:** the refresh is per `Database` handle, driven by a statement
boundary; a handle that runs one very long statement (a slow aggregate, a
large scan) does not see a commit that lands mid-statement, and is not meant
to — the pin for the duration of one statement is what keeps that statement's
own answer internally consistent.

## Streaming execution

```sh
cargo test -p inlaysql-core --test streaming
```

`crates/inlaysql-core/tests/streaming.rs` (`docs/architecture.md`, decision D5, gap G5)
checks the two properties a row-source layer has to hold at once, and they
pull in opposite directions. **It has to stop early:** a `LIMIT 10` over a
large table must not decode the whole table, measured rather than timed — the
storage wrapper counts the rows actually handed to the engine, so "stopped
early" is a number, not an impression. **It has to stop early only when that
is still the right answer:** `ORDER BY`, `GROUP BY` and `DISTINCT` all decide
which rows survive, so a pipeline that truncated the scan under any of them
would answer with the wrong ten rows — each of those gets a test that the
whole table is still read, and one that the answer matches the unlimited
query truncated by hand. Projection pushdown gets the same treatment from the
other side: a column the executor decides not to decode reads as `NULL`, so
every construct that can observe a column gets a query whose answer would
change if the mask missed it (`every_construct_that_reads_a_column_still_sees_it`,
`select_star_still_returns_every_column`).

The same two properties apply to a join's inner side once it is an index
probe rather than a materialised table (AHL-464): `a_probed_join_does_not_read_the_whole_inner_table`,
`a_limit_on_a_probed_join_short_circuits_the_inner_side`, and
`a_join_the_rule_declines_still_reads_the_inner_table` for the shapes the
planner rule does not rewrite — a probe an outer `LIMIT` should short-circuit
answers wrong just as easily as a scan does if it does not.

## Large indexes

```sh
cargo test -p inlaysql --test large_index                 # the property
cargo test --release -p inlaysql --test large_index -- --ignored   # the original failure
```

A saved index is megabytes; a commit is bounded by one write-ahead log region.
So the engine saves in batches — and how big a batch may be is now answered by
the storage backend rather than by a byte budget over the payload, because
copy-on-write dirties a whole root-to-leaf path per entry and those are not the
same quantity. On a five-thousand-row database, 64 KiB of chunks became a
1.1 MiB log record and the save failed outright.

The fast test loads 1,200 rows on a simulated disk and checks a multi-
transaction save round-trips. The ignored one is the original reproduction:
5,000 rows, half a minute of durable commits, run nightly.

## Large statements

```sh
cargo test -p inlaysql --test large_statements                          # the refusals
cargo test --release -p inlaysql --test large_statements -- --ignored --nocapture   # the row counts
```

The same one-region bound as above, met from the SQL side instead of the index
side: `DELETE FROM t`, `UPDATE t SET ...` and `INSERT INTO t SELECT ... FROM t`
are hard errors on a large table (`docs/enterprise-readiness.md` blocker 5).
The default tests assert the refusal *and* that the table is untouched
afterwards *and* that the handle still works — a refusal is an acceptable
state only while all three hold, and the third one did not until AHL-482.
The ignored one bisects each threshold and prints the table both that entry and
the test's own module doc quote; it is what to rerun after any change to the
record layout, the change log, or what a write dirties.

Note what it found about `DELETE`: it is bounded by the change-log record, not
by the rows, so its ceiling barely moves with row width. The default test
proves that by moving the threshold with nothing but the length of the table's
name, which is the one thing in the commit that only `cdc.rs` repeats per row.

## ANN recall

```sh
cargo test -p inlaysql-core --lib hnsw                              # the property
cargo test --release -p inlaysql-core --lib -- --ignored recall_holds   # over 64x
```

Recall used to *fall* as the corpus grew — 0.90 at five thousand vectors, 0.73
at twenty thousand (AHL-372). An approximation getting worse the more data it
has is not a tuning problem, it is a structural one, and the structure was the
layer distribution: `level_of` was geometric with ratio 1/2, so half the corpus
sat one layer up and the `ef = 1` greedy descent could not cross it.

Same split as above, and for the same reason — the cost is in the build, the
property is not. The fast test measures recall@10 against exhaustive search at
400 and 1,600 vectors and asserts it clears 0.95 at both and does not slide
between them; the ignored one runs the same assertion over a 64x range, to
25,600. Neither is the published measurement, which is `bench --suite vectors`
at dim 384 and 100,000 rows. They exist so that a change which reintroduces the
slope fails a pull request instead of a nightly benchmark nobody reads until
Monday.

**Once per metric, not once.** Recall is a comparison against the right answer,
and the right answer is only defined once a distance is — so `vector_l2_ops`
has its own run of the same assertion against its own exhaustive L2 oracle,
and the int8 and paged L2 paths have theirs. A number measured under cosine
says nothing about an L2 graph, which is why the oracle takes its metric from
the index rather than from an argument. `-- --nocapture` prints the table:

```sh
cargo test --release -p inlaysql-core --lib -- --nocapture recall
```

The benchmark also gained `--suite sweep`, which walks the `M` /
`ef_construction` / `ef_search` grid and prints the recall-latency curve behind
the defaults. It is not part of `--suite all`: one graph build per point.

## Backends

The I/O backend is a `Device` — four methods, no engine knowledge — so "the
same suite passes on every backend" should be a tautology. `backends.rs` exists
for the cases where it is not: a short read at end of file, an offset computed
differently, a sync that does not reach the platter.

```sh
cargo test -p inlaysql --test backends -- --nocapture
```

Blocking file I/O, in-memory, and (on Linux) `io_uring` each run the whole SQL
Logic Test subset plus a hybrid retrieval query, and a database written through
`io_uring` is reopened on the blocking backend — the file format is the
contract, not the I/O mechanism.

**Not covered on non-Linux hosts:** the `io_uring` case does not exist there.
CI logs a warning rather than passing silently. The code is compile-checked for
Linux from any host with
`cargo check --target x86_64-unknown-linux-gnu --workspace --all-targets`.

## WASM

```sh
./crates/inlaysql-wasm/build.sh --serve                       # build, report size, serve the demo
cd crates/inlaysql-wasm/browser && npm ci && npm run smoke    # headless Chromium
cd crates/inlaysql-wasm/edge    && npm ci && npm run smoke    # workerd
```

Three layers, because "it compiles to `wasm32`" is the least interesting part
of the claim:

1. **`portability.rs`** asserts what makes the WASM build worth having: a
   database built in memory opens natively from its bytes, and a file written
   by the CLI opens from memory. Same format, both directions. Native, because
   the thing under test is the byte layout rather than the bindings.
2. **`browser/smoke.mjs`** drives the actual demo page in headless Chromium —
   it seeds a database, ranks rows, runs ad-hoc SQL, and round-trips the file
   through OPFS. OPFS exists only in a browser, so this is the only place that
   path can be tested at all.
3. **`edge/smoke.mjs`** runs the worker on `workerd`, the runtime Cloudflare
   runs in production, from a database file the *native* build wrote — the
   portability claim exercised end to end by a real runtime rather than
   asserted. No account and nothing deployed: `wrangler dev` runs it locally,
   which is why it is a CI job rather than something someone tries occasionally.

Size, on the size-optimised profile:

| | Raw | Gzipped |
| --- | ---: | ---: |
| `.wasm` module | 2.0 MB | 661 KiB |
| edge database image | 1.4 MB | 18 KiB |

Both numbers are printed by every build and published to the CI job summary;
the job fails above 5 MB gzipped. Details in [`docs/wasm.md`](docs/wasm.md).

**Not covered:** only Chromium, so a Safari- or Firefox-specific OPFS or WASM
regression would not be caught. The worker is exercised on local `workerd`, not
on deployed Cloudflare infrastructure.

## MCP

```sh
cargo test -p inlaysql-mcp --test client
```

Driven through the real line protocol rather than by calling the functions
underneath: handshake, tool list, hybrid search, a write and the change event
it produces — plus the refusals (a write smuggled through `query`, an injected
identifier, an unknown method, malformed JSON, and answering a notification,
which would wedge a client).

## Benchmarks

Every published number regenerates from one of two scripts, and the split is
about what a machine needs:

```sh
./bench/run.sh                      # SQLite, sqlite-vec — needs only cargo
SUITE=points ./bench/run.sh         # vs SQLite
SUITE=indexed ROWS=100000 ./bench/run.sh          # secondary-index point/range, vs SQLite
SUITE=joins ROWS=20000 QUERIES=100 LIMIT=20 ./bench/run.sh  # index nested-loop join, vs SQLite
SUITE=vectors ./bench/run.sh        # vs sqlite-vec
SUITE=quantization DOCS=100000 QUERIES=50 ./bench/run.sh  # exact vs int8, both shapes
SUITE=concurrency ./bench/run.sh    # several writers on one file
SUITE=sweep DOCS=20000 ./bench/run.sh  # the M/ef_construction/ef_search recall-latency grid

./bench/compare.sh                  # DuckDB, pgvector — needs Docker
```

`compare.sh` generates the corpus, the queries and the correct answers **once**
and has every engine read those same files, so four engines cannot end up
answering four slightly different questions. Container images and library
versions are pinned.

[`bench/README.md`](bench/README.md) states how each comparison is kept fair —
matched schema, prepared statements on both sides, matched durability,
plans checked rather than assumed — and [`BENCHMARK.md`](BENCHMARK.md)
publishes the results we lose as well as the ones we win, regenerated on a
quiet machine (AHL-452) and kept in sync with the harness as suites are
added; the numbers below are a summary of that file, not a second source of
truth, and it is the one to trust if the two ever disagree. Today: 2.63x
faster on durable point writes than journal-mode SQLite, and 4.97x faster on
point reads (1.33x faster than WAL-mode SQLite) since the page cache — but
that is the *warm* number, and a cold handle is much weaker because our miss
path is dearer than SQLite's. A B-tree secondary index beats our own full
scan ~851x and beats journal-mode SQLite's indexed point reads 1.66x, but
sits 1.54x behind WAL-mode SQLite on the same point lookup, and **range scans
lose outright** (2.05x behind journal-mode, 2.82x behind WAL). Still losing:
**joins**, 1.86x to 10.71x slower than journal-mode SQLite depending on shape
— the top open performance target, published because it is true, even though
the index nested-loop join itself (AHL-464) beat this engine's own previous
executor 6.6–100x. Vector search beats `sqlite-vec` 6.88–9.52x depending on
corpus shape; against pgvector it is close rather than a rout — pgvector's
159 µs *includes* a socket round trip and is within touching distance of our
78 µs in-process. **Concurrent writers now scale rather than merely not
losing**: 8 writers on one file reach 692 commits/s against SQLite's flat 93
(**7.4x**, 0.0% aborted) — reworking the commit gate so it stops
re-deriving committed state on every commit (AHL-468) freed group commit
(AHL-461) to actually batch fsyncs, where before it had nothing to batch.
Against MySQL and PostgreSQL, containerised so all three pay the same
virtualised `fsync`: reads win by ~55x MySQL and ~11.3x PostgreSQL (in-process
against a socket round trip, an asymmetry stated rather than hidden);
sequential durable writes now match PostgreSQL (723.1 vs 730.9 ops/s) and
MySQL is 1.08x faster, single-connection so group commit cannot fire by
design. Server to server over the MySQL wire (AHL-495), where both sides pay a
socket round trip, we win reads 1.52x at one connection and 1.10x at eight,
and lose writes 1.43x and 4.76x respectively. On the other side: hybrid
retrieval roughly 14–17x faster than either DuckDB or pgvector, because it is
one statement rather than two queries and a fusion step in the client.

Both scripts run nightly in [`trust.yml`](.github/workflows/trust.yml), which
uploads every result file as a `benchmark-results` artifact.

**Not covered:** nothing tracks a benchmark result over time. Each run is a
snapshot; noticing a regression still means reading two files. The artifacts
are the raw material for that and not the thing itself.

## Reproducing a failure

Every randomized test is seeded, and the seed is in the failure message.

```sh
cargo test --release -p inlaysql-core --test dst_sweep -- --ignored     # prints the failing seed
cargo test -p inlaysql-core --test logic_bugs                           # prints the round and predicate
cargo test -p inlaysql --test differential                              # prints the seed, predicate and rows
```

A logic-bug finding needs more than a rerun: [`docs/sqlancer.md`](docs/sqlancer.md)
has the triage procedure — shrink it, decide which engine is right, turn it
into a named regression test, and only then change the engine.

If a test here fails on your machine and not in CI, that is a finding worth an
issue: everything on this page is supposed to be machine-independent.
