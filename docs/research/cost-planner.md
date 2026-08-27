# R4 — A minimal cost-based planner

**Status: staged prototype landed; more research before production code.**
The current join rules are correct and already win one shape. The first R4
slice now adds explicit `ANALYZE` statistics and lets fresh, complete stats
choose between the existing hash and probe operators in written join order.
SQLite's own plan on the benchmark-sized workload remains the target for the
next access-path iteration; no join reorder or new row format has landed.

## Question

What is the smallest statistics and costing layer that fixes join ordering and
access-path selection while preserving SQLite semantics, deterministic output
where this engine promises it, and the current rule-based fallback when stats
are missing or stale?

## Current InlaySQL decision tree

The relevant code is deliberately simple today:

- [`sql.rs`](../../crates/inlaysql-core/src/sql.rs) resolves `FROM` sources and
  joins in written order.
- [`engine.rs::scan_shape`](../../crates/inlaysql-core/src/engine.rs) decides
  whether the driving side is a full scan from `LIMIT`, `OFFSET`, retrieval,
  and point-predicate shape. It does not inspect table cardinalities.
- [`engine.rs::join_inner`](../../crates/inlaysql-core/src/engine.rs) chooses a
  costed hash build or index probe when a current, complete stats snapshot
  covers the first join; otherwise it applies the old shape rule and hashes
  only a full-scan equi-join.
- [`engine.rs::join_probe`](../../crates/inlaysql-core/src/engine.rs) prefers
  the integer primary key, then the shortest applicable leading B-tree index.
  That choice narrows candidates safely but is not a cost comparison.

The rule-based choices cannot be wrong because the full `ON` expression still
checks every narrowed candidate. The gap is cost, not correctness: the engine
has no way to know that a 20,000-row side is a better driver than a 160,000-row
side, or that eight rows per indexed value makes an index probe cheap.

## SQLite evidence on the real binary

I created the exact join shape used by
[`crates/inlaysql-bench/src/joins.rs`](../../crates/inlaysql-bench/src/joins.rs):
20,000 `users`, 160,000 `posts`, eight posts per user, and
`posts_user_id(user_id)`. After populating the rows, I ran `ANALYZE` and then
`EXPLAIN QUERY PLAN` with `/usr/bin/sqlite3`:

```text
sqlite_stat1
posts | posts_user_id | 160000 8
users |               | 20000

SELECT posts.id, users.name
FROM posts JOIN users ON posts.user_id = users.id
SCAN users
SEARCH posts USING COVERING INDEX posts_user_id (user_id=?)

SELECT posts.id, users.name
FROM posts JOIN users ON posts.user_id = users.id LIMIT 10
SCAN users
SEARCH posts USING COVERING INDEX posts_user_id (user_id=?)

SELECT users.name, posts.title
FROM users JOIN posts ON posts.user_id = users.id
SCAN users
SEARCH posts USING INDEX posts_user_id (user_id=?)

SELECT users.name, posts.title
FROM users JOIN posts ON posts.user_id = users.id LIMIT 10
SCAN users
SEARCH posts USING INDEX posts_user_id (user_id=?)
```

The important finding is the first pair: SQLite changes the physical order of
`posts JOIN users` to scan the smaller `users` side and probe the indexed
`posts` side. The covering-index distinction is also real: the first query
does not need `posts.title`, while the second direction does.

The official descriptions of this machinery are [SQLite's
`ANALYZE`](https://www.sqlite.org/lang_analyze.html), [the query-planner
overview](https://www.sqlite.org/optoverview.html), [the next-generation query
planner](https://www.sqlite.org/queryplanner-ng.html), and [the
`EXPLAIN QUERY PLAN` format](https://www.sqlite.org/eqp.html).

## The current loss this should explain

The clean join repeat at commit `188e33c` is
[`bench/results/20260827T030542Z-repeat.txt`](../../bench/results/20260827T030542Z-repeat.txt):

| Shape | InlaySQL | SQLite journal | InlaySQL relative result |
| --- | ---: | ---: | ---: |
| PK inner, full | 86 joins/s | 105 joins/s | 1.24× slower |
| PK inner, `LIMIT 10` | 87,896 joins/s | 251,152 joins/s | 2.86× slower |
| Secondary-index inner, full | 251 joins/s | 69 joins/s | 3.63× faster |
| Secondary-index inner, `LIMIT 10` | 91,761 joins/s | 259,096 joins/s | 2.78× slower |

The current full-scan rule builds a hash table for both full equi-joins. That
is why the secondary-index direction wins: it hashes the 160,000-row inner
side once and scans 20,000 users. The PK direction still loses despite a
valid hash plan, so planner work will not be enough by itself; the outer scan,
row materialisation and page-miss costs remain W1/W2 work. The point of R4 is
to stop choosing an obviously poor physical order before attacking those
remaining costs.

## Proposed statistics

Add an optional SQLite-shaped statistics record, derived state rather than a
source of truth:

```text
table: row_count, leaf_pages
index: table, key_prefix_columns, entry_count, distinct_prefix_count,
       average_group_size, covering_columns
```

The landed first implementation stores the leading prefix of every scalar
B-tree index and the table row count under the `planner_stats` metadata key,
with format/schema versions and the committed data version it describes. It
uses an explicit full scan for now; a bounded deterministic sample for large
tables is still research. A missing, corrupt, or stale record falls back to
today's rule-based plan; statistics can make a query slower, never change its
result.
`ANALYZE` is the explicit refresh operation and refuses unsupported sampling,
partition and column-list options rather than silently changing their meaning.

The sample must use the same key order the index already exposes. For an index
whose `sqlite_stat1`-style second number is `8`, an equality on its leading
column estimates eight candidate rows. No new page format is needed, and no
statistics row should be maintained synchronously on every user write in the
first version.

## Cost model v1

Keep costs in integer work units so the planner is deterministic and easy to
test. The constants are calibration knobs, not user-visible promises:

```text
scan(T)       = leaf_pages(T) + row_count(T) * decode_cost
hash(T, key)  = scan(T) + row_count(T) * hash_cost
probe(I, N)   = N * (descent_cost(I) + group_size(I) * row_fetch_cost)
materialise(T, N)
              = scan(T) + N * row_count(T) * row_compare_cost
```

For one equality join, compare:

1. scan the current outer and probe the inner PK/index;
2. scan the current outer and hash the inner;
3. when the join is an eligible inner-join reorder, scan the smaller side and
   probe the side with the narrower leading index, then restore the promised
   output order if the public API requires it.

Use saturating integer arithmetic. If any input is unknown, retain the current
rule. The landed prototype chooses only between the existing hash and
index-probe paths; it introduces no batch executor, new join operator, or
storage-format change. It reports the chosen integer work units in `EXPLAIN`
after a costed choice, so the executor and the plan report share one chooser.

## Join ordering boundary

The SQL standard does not promise row order without `ORDER BY`, but this engine
and its tests currently emit driving rows in a stable row-id order. Therefore
R4 is being advanced in two stages:

1. **Access-path costing without reordering.** Use stats to choose hash vs
   probe/materialise for the written order. This is result-order preserving
   and is the staged prototype now landed; its benchmark impact still needs
   to be regenerated on a quiet machine.
2. **Inner-join reorder only with an explicit order-preservation plan.** For
   an inner-only chain of at most six tables, dynamic programming can enumerate
   connected subsets. The executor must either emit in the original stable
   order or add a deterministic final ordering step where the query's contract
   requires it. `LEFT JOIN`, `ORDER BY`, `DISTINCT`, window functions, and
   correlated references stay on the existing path until each has its own
   proof.

This keeps the first parity change small and makes a plan choice visible in
`EXPLAIN` rather than silently changing row order.

## Verification plan

- The landed planner test loads the `users`/`posts` cardinalities above and
  asserts the chosen physical path for both directions and both LIMIT shapes.
  It also checks result rows and that the stats survive reopening the engine.
- Compare every costed result with the existing rule-based executor and with
  SQLite's answers; use `ORDER BY` in the timing harness when measuring an
  intentionally reordered plan.
- The landed stale-stat test proves a row write removes the costed choice and
  restores the current rule. The malformed-blob decoder and reopen-after-
  corruption tests prove the fallback does not fail database opening.
- Regenerate `SUITE=joins` and `SUITE=indexed` through `bench/run.sh`; do not
  move a result into `BENCHMARK.md` until the repeat spread is below the
  repository's reporting bar.
- Keep the batch-execution question separate: R3 must still decide whether a
  second morsel path is needed for the aggregate/wide-scan target.

## Recommendation

The staged prototype is a **go**: it directly explains the measured
`posts JOIN users` choice, does not touch the `no_std`/WAL/page format, and has
a safe fallback. **No-go** remains on reordering or a broad statistics schema
until output-order and stale-stat behavior have explicit tests. R4 is not
closed until the current losing join shapes each have a predicted plan and the
benchmark rerun shows whether planning, rather than row machinery, moves their
result.
