# The SQL surface

The dialect in full: the additions to SQLite's SQL, scalar indexes and the
joins that use them, which distance a vector index uses, the `ef_search`
recall/latency trade, what a retrieval function means inside a join, why
fusion works on ranks rather than scores, and the measured SQL Logic Test
pass rate.

This was the README's `## The SQL surface` and `## SQL Logic Test` sections;
they moved here so the README could stay a landing page. The prose is
unchanged.

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

## Scalar indexes and joins that use them

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
[What this is not](../README.md#what-this-is-not).

## Which distance a vector index uses

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

## Choosing the recall/latency point: `ef_search`

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

## What a retrieval function means in a join

A retrieval index lives over one table's rows, so when a query joins tables the
retrieval expression may reference **only the driving table** (the first table
in `FROM`). The ranking is computed over that table's rows, then the join runs,
then `WHERE` and `LIMIT` apply to the joined result. An inner join can therefore
drop ranked rows, and a one-to-many join can expand them past `LIMIT`; the score
still reflects the driving table's rows only. A query that names a
non-driving table's column in `vector_score`/`bm25_score` is rejected at prepare
time rather than answered incorrectly. Retrieval and aggregation cannot be
combined in one query.

## Why fusion works on ranks, not scores

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
[`docs/server.md`](server.md) has that argument.

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
[`TESTING.md`](../TESTING.md) also names the three places the dialect knowingly
disagrees with SQLite: rendering a `REAL` as text, columns being *typed*
rather than merely affine (four of five affinities convert or reject where
SQLite's affinity is a preference that keeps a value it cannot convert), and
one row-id counter per database rather than per table. Integer overflow used
to be a fourth; AHL-412 made arithmetic promote to `REAL` on overflow the way
SQLite does, so that one is gone.

How everything else is tested — deterministic simulation, metamorphic and
differential logic-bug tests, fuzzing, cross-backend equivalence — and what is
*not* covered is in [`TESTING.md`](../TESTING.md). Benchmarks against SQLite,
`sqlite-vec`, DuckDB, pgvector, Meilisearch, MySQL and PostgreSQL, including
the ones we lose, are in [`bench/README.md`](../bench/README.md) and
[`BENCHMARK.md`](../BENCHMARK.md); every number in either one regenerates from
`./bench/run.sh` or `./bench/compare.sh`.
