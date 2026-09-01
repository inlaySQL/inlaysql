# Benchmarks

```sh
./bench/run.sh                      # every suite, pinned parameters
SUITE=points ./bench/run.sh         # just the SQLite comparison
SUITE=indexed ./bench/run.sh        # just the secondary-index comparison
SUITE=joins ./bench/run.sh          # just the join comparison
SUITE=vectors ./bench/run.sh        # just the ANN comparison
SUITE=concurrency ./bench/run.sh    # just the concurrent-writer comparison
SUITE=retrieval ./bench/run.sh      # just the retrieval workload

# Focus on the writer counts being compared.
WRITER_LEVELS=1,32 SUITE=concurrency ./bench/run.sh
WRITER_LEVELS=1,128 SUITE=concurrency ./bench/run.sh

./bench/compare.sh                  # vs DuckDB, pgvector, Meilisearch, MySQL, PostgreSQL (needs Docker)

REPEATS=5 ./bench/repeat.sh         # run.sh five times, report the median and the spread
REPEATS=5 SUITE=retrieval ./bench/repeat.sh

REPEATS=5 ./bench/repeat-compare.sh # compare.sh five times, median and spread
COOLDOWN_SECONDS=0 ./bench/repeat-compare.sh   # no pause between repetitions

# Profiling, and comparing two builds of it honestly.
./bench/profile_ab.sh joins-limit HEAD~1   # interleaved A/B, medians and ranges
./bench/attribute.py /tmp/x.sample         # who asked for the memcmp/allocator work

# The HNSW parameter grid behind the shipped defaults. Not in `all`: it is
# one graph build per (M, ef_construction) point and takes minutes.
cargo run --release -p inlaysql-bench -- --suite sweep --docs 20000

# ann-benchmarks: somebody else's corpus, somebody else's ground truth,
# somebody else's protocol. See "ann-benchmarks" below.
bench/ann/.venv/bin/python bench/ann/run.py --dataset glove-25-angular
```

`bench/run.sh` and `bench/compare.sh` both refuse to *start* a run when the
one-minute load average is above `0.25` per logical CPU, because a busy host
has moved the concurrency rows by more than the code changes being measured.
Both source the same `bench/load_gate.sh`, deliberately: two copies of a gate
is how one of them silently stops matching the other, and the numbers on both
sides of a published comparison have to be gated the same way or the gate is
part of what is being compared. For a deliberate under-load experiment only,
override it explicitly:

```sh
BENCH_MAX_LOAD_PER_CPU=off SUITE=concurrency ./bench/run.sh
```

A gate checked once, before anything runs, cannot catch a spike that arrives
seconds later — `PERF.md` §4 measured the correlation between disclosed
start-load and actual point-read throughput at r≈0.18 across runs that all
passed this exact gate, because nothing was still watching once the run was
under way. `run.sh` now samples the load every `BENCH_LOAD_SAMPLE_SECONDS`
(default 5) for the run's whole duration, not just at the start, and folds
start + in-flight + end into a min/median/max the result file publishes
alongside the original start-of-run reading. **Policy for a spike mid-run: the
run is not aborted** (a long suite can take minutes, and discarding it wastes
more than the contamination costs) — it finishes, but the result file is
marked `CONTAMINATED`, loudly, on a `load:` line, and a matching warning is
printed to the terminal the moment the spike is seen. `bench/summarise.py`
checks every input file it is given for that marker independently of its
normal parsing (the marker lives on a `load:` line, which is provenance and
would otherwise be silently dropped along with the rest of that line) and
prints the same loud warning, both before and after the combined report, if
any run it combined was contaminated — the flag survives being averaged
together with clean runs precisely because it is never allowed to disappear
into the median. Set `BENCH_LOAD_SAMPLE_SECONDS` to change the sampling
interval; setting `BENCH_MAX_LOAD_PER_CPU=off` disables monitoring
altogether, exactly as it disables the start-of-run gate, since that variable
is the documented escape hatch for a deliberate under/over-load measurement.

The raw result records the observed load (start, and now the full sampled
range) and the threshold/override, so a later reader can tell whether a run
passed the quiet-machine gate throughout, not just at the starting gun.

`bench/compare.sh` has the same gate, with one difference that matters for
reading its output: it samples only the *measured* phases. That script
compiles the workspace and builds container images before it measures
anything, and those phases saturate the machine by design; sampling across
them would mark every run `CONTAMINATED` for work the script itself was
doing, which is how a warning stops being read. The sampler therefore starts
after the containers are up and built — the containerised InlaySQL image is
now built during setup for exactly this reason, rather than at its `run` step
between two driver phases — and stops after the last driver. `CONTAMINATED`
on a compare result means something *else* on the machine disturbed the
measurement.

Writes a timestamped file to `bench/results/` (git-ignored) containing the
toolchain, host and commit alongside the numbers, so a result is always
traceable to what produced it.

Two scripts, because of one line the project rules draw: every published number
has to regenerate from a checkout. SQLite and `sqlite-vec` link into the
harness, so `run.sh` needs nothing but `cargo`. DuckDB is a separate runtime,
and pgvector/Meilisearch/PostgreSQL/MySQL are servers, so `compare.sh` puts
all five (plus InlaySQL's own MySQL-wire server) in containers with pinned
versions — reproducible, but only on a machine with Docker. `compare.sh`
covers three workloads: the retrieval comparison (recall + latency, against
DuckDB, pgvector and Meilisearch), the OLTP comparison (point reads and
writes, against MySQL and plain PostgreSQL, InlaySQL as a library — see
"OLTP: MySQL and PostgreSQL, matched durability" below), and the
server-to-server OLTP comparison
(InlaySQL's own MySQL wire against MySQL's, same client, a couple of
concurrency levels — see "Server-to-server" below).

## How many times to run it

Once is not enough, and the project learned that the expensive way. Two
consecutive editions of `BENCHMARK.md` carried figures that moved for reasons
no commit could explain: point reads halved between two runs a few hours apart
on a path neither commit touched, while one SQLite configuration fell and the
other rose in the same window. Nothing was wrong with the harness. A
latency-shaped micro-benchmark on a laptop simply has an error bar of roughly a
factor of two, and publishing a single run to three significant digits pretends
otherwise.

### Profiling: two instruments, and the mistakes they exist to stop

`bin/profile` runs one suite in a loop so a sampler can attach to it. Two
things wrap it, and both were built after the thing they prevent had already
happened once:

`./bench/profile_ab.sh <suite> <git-ref>` builds the profile binary twice — the
working tree and `<git-ref>`, via a worktree so a committed change still has a
"before" to compare against — and runs them **alternately**, one repetition
each. The `LIMIT`-join cache was first measured at 1.31x by comparing a run to
another run taken half an hour later; interleaved, the same *before* binary
moved from 68k to 85-89k ops/s and the real figure was 1.42x. The machine had
drifted, and the error flattered the change. Interleaving does not quiet a
machine, it makes the drift land on both sides. The script prints both ranges
and says plainly when they overlap, because a ratio quoted from overlapping
ranges is not a result.

`./bench/attribute.py <sample-file>` answers "which engine function asked for
this work" instead of "what was the CPU in". A profile of this engine is mostly
`memcmp`, `memmove` and the allocator, three answers that name a mechanism and
no cause, and reading the call graph by eye has gone wrong twice: a
residual-filter optimisation was scoped at 15-20% by counting descent `memcmp`
as the filter's (attributed: 8.5% `get_from`, 0.9% the filter's actual route),
and its replacement assumed a per-cell leaf decode that the profile shows
never happens. Both took minutes to disprove once attributed.

`./bench/repeat.sh` is the answer to that. It runs `run.sh` N times with
identical parameters, keeps every raw file, and reports each number's median
across the runs together with its **spread** — how far the best and worst runs
disagreed, as a fraction of the median:

```
  spread      column        median           min           max  row
   80.7%         max       90.63µs       71.88µs      145.04µs  bm25
   10.4%         p50       50.75µs       50.42µs       55.71µs  bm25
```

Read the `max` columns and shrug: a `max` is one unlucky sample and is supposed
to swing. Read a wide `p50` or a wide ops/s as the measurement failing, not the
engine. **A figure whose spread is 10% or more should not be quoted to three
digits**, and `BENCHMARK.md` should say what the spread was rather than pick
the run that flattered us.

The alignment is positional and strict: identical parameters and an identical
seed produce identical structure, so if two runs disagree about their *shape*
rather than their numbers, `summarise.py` refuses rather than averaging two
different benchmarks together. `./bench/summarise.py a.txt b.txt c.txt` runs
the same comparison over result files you already have.

What repeating cannot fix: it measures the machine's variance, not its bias.
Something stealing a core for the whole sitting is paid by every run, so the
spread stays narrow while the median is wrong. Note what else was running.

## Suite: points — InlaySQL vs SQLite

The narrowest workload a storage engine has: one row by primary key, read and
written. Both engines use `id INTEGER PRIMARY KEY`, so both do one tree descent
per lookup, and **both use prepared statements**: each prepares once outside the
timed loop and binds the key per iteration — InlaySQL through
`Database::prepare` + `query_prepared`, SQLite through `Connection::prepare` +
`Statement::query_row`.

That is a change. This suite used to disable prepared statements on both sides,
because InlaySQL had none and driving SQLite through `Connection::query_row`
(which prepares per call) was the only way to keep the comparison level. Since
AHL-373 it has them, so the caveat is gone and both engines are measured the way
an application would actually use them.

The switch moved both engines, and it moved SQLite by less, because SQLite was
never paying as much for parsing. On one developer machine (Apple silicon,
20,000 rows, 5,000 lookups, seed 42), point-read p50:

| | before, neither prepared | after, both prepared |
| --- | --- | --- |
| InlaySQL | 15.04 µs | **10.92 µs** |
| SQLite (journal, `sync=FULL`, `fullfsync`) | 7.21 µs | 6.25 µs |
| SQLite (WAL, `sync=NORMAL`) | 1.79 µs | 1.04 µs |

So prepared statements took about a quarter off InlaySQL's point read and left
it still the slower engine — the remaining gap is the executor and the tree, not
the parser, and the next person to work on this should start there rather than
on the front end.

SQLite is measured in two configurations, and the difference between them
matters more than either number:

| Configuration | What it means |
| --- | --- |
| `journal`, `synchronous=FULL`, `fullfsync` | Every commit is durable through the drive's write cache. The like-for-like column: InlaySQL always does this. |
| WAL, `synchronous=NORMAL` | What most applications actually run. Faster, and a power cut can lose recent commits. |

`fullfsync` is not decoration. On macOS, Rust's `File::sync_all` issues
`F_FULLFSYNC` while SQLite's default `fsync` returns before the data reaches
the platter — without the pragma, the comparison is a durable engine against a
hopeful one. It is a no-op on Linux.

InlaySQL is compiled against a `bundled` SQLite, so the baseline is a known
version rather than whatever the host happens to ship.

### Batched writes

Since AHL-374 there is a second write row: the same per-row `INSERT` loop,
wrapped in an explicit `begin`/`commit` transaction. The engine batches rows
into one commit until the write-ahead log is nearly full, commits, and starts a
fresh transaction — so thousands of rows pay one `fsync` per batch instead of
one per row. On one developer machine (Apple silicon, 20,000 rows, seed 42):

| | ops/s |
| --- | --- |
| InlaySQL, one commit per row | 153 |
| InlaySQL, batched | **1,788** (11.7x) |

The batch boundary is the engine's own limit, not a magic number: when a
transaction is about to overflow the log the engine refuses the next statement
with a clear `Error::Transaction` *before* running it, the harness commits what
is buffered and starts a new transaction. The same discipline is what makes the
retrieval suite's `ingest` number.

## Suite: indexed — InlaySQL vs SQLite

```sh
SUITE=indexed ROWS=100000 ./bench/run.sh
```

`WHERE email = ?` and a small `WHERE email >= ? AND email < ?` range (50 rows,
`RANGE_SIZE` in `indexed.rs`) on a non-key column — the query an ORM emits all
day, and the one where a secondary index changes the *shape* of the answer,
not just its constant factor. Four rows per query shape:

- **InlaySQL, B-tree index** — `CREATE INDEX users_email ON users (email)
  USING BTREE`, built after the rows are loaded (the harder path for the
  engine, since the index has to describe a table that already exists).
- **InlaySQL, no index** — the identical engine, rows and query, with no index
  for the planner to use: a full scan. Measured in the same process, on the
  same rows, in the same run, so a before/after ratio needs no cross-machine
  caveat. This is the row that makes the AHL-423 figure (scalar B-tree
  indexes landed at ~3,800x over a 100k-row scan) regenerate from a checkout
  instead of standing as an unreproduced assertion — rerun with `ROWS=100000`
  to reproduce that shape.
- **SQLite, journal + WAL** — the same index, both of the durability
  configurations the `points` suite uses.

Both engines use prepared statements, bound per iteration, as `points` does.
The point-lookup sequence and the range starts are drawn once from the seed,
so every engine answers the identical questions in the identical order; the
range bound relies on `email`'s id being zero-padded to a fixed width, so
lexicographic order on the column equals numeric order on the id and every
range is exactly `RANGE_SIZE` rows on both engines.

**What this deliberately does not claim:** the unindexed row's cost is a
property of `--rows`, not a fixed number — it is expected to get worse as
`--rows` grows, and reading it without `--rows` alongside it is reading it
wrong. Nothing here measures `ORDER BY` pushdown through the index (open per
`PERF.md`), a composite index, or an index on more than one column.

## Suite: joins — InlaySQL vs SQLite

```sh
SUITE=joins ROWS=20000 QUERIES=100 LIMIT=20 ./bench/run.sh
```

`users` x `posts`, in both directions the planner can drive it:

- **PK inner** — `FROM posts JOIN users ON posts.user_id = users.id`. The
  inner table's join key is its `INTEGER PRIMARY KEY`.
- **Secondary-index inner** — `FROM users JOIN posts ON posts.user_id =
  users.id`. The inner table's join key is `posts.user_id`, a scalar B-tree
  index. This is the exact shape `PERF.md` names.

The setup runs `ANALYZE` on both engines before preparing the statements. With
fresh stats, full scans use the costed hash path for this row-at-a-time engine;
with `LIMIT`, the planner can keep the index-probe path so it can stop before
paying a full build. The cost constants are calibrated to InlaySQL's measured
probe/descent cost, not copied from SQLite's physical implementation. A
repeated prepared execution may reuse its immutable inner build while the
committed row version is unchanged. Both engines consume and discard one
projected row at a time (`query_prepared_each` and `query_map`, respectively);
neither retains an answer-sized result container merely to count it.

Each direction runs with and without a `LIMIT` (`--limit`, default 10),
because the probe is a stage of the streaming pipeline and a `LIMIT` on an
unindexed-order plan stops the outer scan once it has enough rows — whether
that shows up as a wall-clock win over SQLite, which has no equivalent
streaming guarantee, is what these two rows are for finding out. Every user
has exactly the same number of posts (`POSTS_PER_USER = 8`, round-robin
assignment), so neither direction gets a luckier key distribution, and both
engines run against the identical schema and the identical
`CREATE INDEX posts_user_id ON posts (user_id)`. Both engines prepare each
query once, outside the timed loop; neither query takes a bound parameter, so
this measures repeated prepared execution, including cache validation but not
the parser or a lookup key.

**What this deliberately does not claim:** there is no unindexed/materialising
row. The fallback the join-key rules decline — an equality the hash or probe
cannot reproduce — is exercised by
`crates/inlaysql-core/tests/btree_index.rs` and `tests/streaming.rs`, not by
this harness, and `PERF.md` already states plainly that a join the rule
declines is still O(n×m) and would still lose. This suite measures the access
paths the rules were built for, not join reordering, and says nothing about a
join with an `OR` or a composite `ON` whose equality is not usable as a key.

## Suite: vectors — InlaySQL vs sqlite-vec

Recall and latency for approximate nearest-neighbour search, against
[`sqlite-vec`](https://github.com/asg017/sqlite-vec), which is what an InlaySQL
user would otherwise reach for.

**Recall is printed before latency, and neither means anything alone.** An
approximate index can be made arbitrarily fast by being arbitrarily wrong, so
both engines are scored against the same oracle — exhaustive cosine similarity
computed in Rust — and the fraction of the true top-k each returned is in the
table. `sqlite-vec`'s `vec0` tables scan exhaustively, so its recall is 1.0 by
construction and its latency grows linearly with the corpus.

For the exact/int8 acceptance comparison without the full suite's unrelated
paged and incremental rebuilds:

```sh
SUITE=quantization DOCS=100000 QUERIES=50 ./bench/run.sh
```

### Two corpora, because the data decides the answer

Since AHL-372 this suite runs twice, over two shapes of corpus, and the gap
between them is the most useful thing it reports.

What an ANN index can achieve is not a property of the index. It is a property
of the data — specifically its **intrinsic dimensionality**. A graph index
answers a query by walking downhill towards it, and that only ends near the
right answer if there is a downhill to walk.

**Uniformly random unit vectors have none.** In 384 dimensions every pair is
near-orthogonal and every distance concentrates: the 10th nearest neighbour and
the 1000th are about a percent apart. Nothing can navigate that, and holding
recall fixed as the corpus grows costs an `ef_search` that grows *with the
corpus* — a linear scan wearing a graph. This is the worst case any ANN index
can be handed, and it is what this suite used to measure exclusively.

**Real embeddings are the opposite.** A few hundred nominal dimensions with an
intrinsic dimensionality in the tens, because meaning clusters. The corpus for
this shape is the same text-derived generator the `retrieval` suite and
`compare.sh` already use.

Publishing only the first number describes a workload nobody has. Publishing
only the second hides the worst case. Both run.

### The numbers

On one developer machine (Apple silicon), dim 384, top-10, 50 queries, seed 42:

| Corpus | Shape | recall@10 | InlaySQL p50 | sqlite-vec p50 | |
| --- | --- | --- | --- | --- | --- |
| 5,000 | realistic | 0.998 | 0.70 ms | 2.62 ms | **3.7x faster** |
| 20,000 | realistic | 1.000 | 1.30 ms | 9.78 ms | **7.5x faster** |
| 100,000 | realistic | 0.998 | 3.08 ms | 48.57 ms | **15.8x faster** |
| 5,000 | uniform | 0.720 | 0.77 ms | 2.34 ms | 3.0x faster |
| 20,000 | uniform | 0.384 | 1.41 ms | 9.72 ms | 6.9x faster |
| 100,000 | uniform | 0.118 | 3.47 ms | 48.27 ms | 13.9x faster |

Read the realistic rows as the engine's number and the uniform rows as its
floor. **On realistic data recall is flat across a 20x range of corpus sizes**
— 0.998, 1.000, 0.998 — which is the property AHL-372 existed to restore, and
the crossover against exhaustive scan has moved below the smallest size
measured. On uniform data recall falls, and no tuning changes that.

### Scalar int8 quantisation

`VECTOR(n)` is exact. `VECTOR(n, INT8)` opts one column into symmetric
per-vector scalar quantisation: one `f32` scale and `n` signed bytes in both row
storage and the HNSW embedding/node payloads. Query vectors remain `f32`.

The dedicated command above measured 100,000 vectors, dim 384, top-10, 50
queries, seed 42 on the same Apple-silicon development machine:

| Shape | exact recall | int8 recall | loss | exact / int8 build | exact / int8 p50 | exact / int8 resident vector payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| realistic | 0.998 | 0.970 | 0.028 | 252.49 s / 121.31 s | 4.95 ms / 2.77 ms | 293.0 MiB / 74.0 MiB |
| uniform worst case | 0.118 | 0.104 | 0.014 | 311.61 s / 133.43 s | 5.59 ms / 4.33 ms | 293.0 MiB / 74.0 MiB |

The resident vector payload is **3.96x smaller** on both shapes. The complete
database file is 5,283,348,480 bytes exact and 3,524,169,728 bytes int8 — only
**1.50x smaller**, not four. The difference is real and important: this B-tree
is append-only copy-on-write today, so page history, WAL space, adjacency and
other non-vector bytes dominate a 100k image. Quantisation delivers the claimed
constant factor on the bytes it owns; it does not disguise unrelated file
amplification as vector data.

Recall loss is likewise measured rather than assumed. The realistic corpus
loses 2.8 percentage points while remaining at 0.970 recall@10. The uniform
corpus loses 1.4 points, but its exact index was already at 0.118 for the
intrinsic-dimensionality reason above. Int8 also built 2.08–2.34x faster and
reduced query p50 in this run because its working set fits caches better.

### What it used to be, and why it is not comparable

| | recall@10 at 5,000 | at 20,000 |
| --- | --- | --- |
| before AHL-372 | 0.899 | 0.733 |
| after, realistic corpus | 0.998 | 1.000 |
| after, uniform corpus | 0.720 | 0.384 |

The old row is **not** a like-for-like baseline, for three reasons, and the
third is the awkward one:

1. It was measured at dim 128; these are dim 384.
2. It was measured before the index was tuned. See the commit for AHL-372: the
   layer distribution was geometric with ratio 1/2, so half the corpus sat one
   layer up and the greedy descent could not cross it. That is what made recall
   *fall* as the corpus grew.
3. **It was measured on a corpus that was neither of the two above.** The
   generator's comment said "centred on zero so the vectors spread over the
   sphere"; the code divided a 24-bit value by `2^23`, putting every component
   in `[-0.5, 1.5)`. Every vector leaned on the all-ones diagonal at a mean
   pairwise cosine of 0.43. It was an accident, it was easier than uniform and
   harder than real embeddings, and the published recall was a number about it.

The divisor is fixed. The old numbers are kept here because deleting a number
you have published is worse than explaining it.

### Tuning, and the curve behind the defaults

```sh
cargo run --release -p inlaysql-bench -- --suite sweep --docs 20000
```

The sweep walks `M` x `ef_construction` x `ef_search` over both corpora against
the same oracle, and prints recall and latency for every point — the argument
for a default rather than an assertion of one. It drives the index directly
rather than through SQL, because the engine's insert path costs milliseconds a
row and would swamp the graph build.

Shipped: `M = 16`, `ef_construction = 200`, `ef_search = max(64, 2k)`. What the
grid said, measuring the index directly at dim 384, top-10:

| | realistic corpus | uniform corpus |
| --- | --- | --- |
| `M = 16`, `ef = 64`, 5,000 | 0.998 @ 55 µs | 0.682 @ 94 µs |
| `M = 16`, `ef = 64`, 20,000 | 1.000 @ 165 µs | 0.318 @ 296 µs |
| `M = 16`, `ef = 64`, 100,000 | 0.998 @ 320 µs | 0.102 @ 648 µs |
| `M = 16`, `ef = 2048`, 100,000 | — | 0.884 @ 12.2 ms |
| `M = 32`, `ef = 2048`, 100,000 | — | 0.978 @ 18.5 ms |

Two things decide the default. On the realistic corpus `M = 16` at `ef = 64` is
already at the ceiling and nothing above it buys anything. On the uniform corpus
nothing short of `M = 32` and `ef = 2048` gets near 0.95, and that costs 18.5 ms
a query and 12 minutes of build at 100,000 rows — detuning every real workload
by 30x to chase a corpus nobody has. `ef_construction` above 200 moved recall by
less than 0.02 on either corpus while costing 15% more build time.

Anyone who *does* have uniformly random vectors can buy the recall back.
`ef_search` is the query-time dial and is reachable from SQL —
`SET inlaysql_hnsw_ef_search = 2048`, or `Database::set_vector_ef_search`
embedded — so the recall column of this grid is choosable per query without
rebuilding anything. `M` and `ef_construction` shape the stored graph and are
still Rust-only (`HnswIndex::with_params`, `set_params`), which is why the
other two columns are not.

`ef_search` scales with `k` because a fixed candidate list is a different amount
of headroom for `k = 1` than for `k = 100`. The engine also over-fetches
candidates 4x for fusion, so `k` arrives at the index already multiplied.

pgvector and DuckDB are not in this suite because neither links into the
harness. They are in `./bench/compare.sh` instead, on a corpus generated once
and shared by all four engines.

### The filtered cases

After the unfiltered comparison, the suite runs the same corpus three more
times with a `WHERE` pushed into the probe, at three filter selectivities:
`WHERE tenant % ? = ?` with each bucket owning ~10%, ~1% and ~0.1% of the
rows. Each query is pinned to a bucket and scored against that bucket's own
exhaustive top-k, so the recall column measures the filter, not the
approximation.

This is the workload AHL-379 existed for: a fixed candidate budget
over-fetched for fusion contains essentially none of one tenant, so filtering
*after* retrieval returns nothing. The engine used to answer it by doubling
the candidate budget each round and re-running the search from scratch until
the filter admitted `LIMIT` rows — geometrically re-walking the graph. It now
compiles the `WHERE` into a row predicate and pushes it into the index walk
itself: a rejected row is traversed (so its neighbours stay reachable) but
neither returned nor counted, and the walk keeps going until its candidate
beam fills with matching rows or the graph is exhausted — one walk, where the
old path paid one per doubling round. The ~0.1% bucket is the pathological
end: it admits fewer rows than the `LIMIT`, so the walk drains the whole
graph and answers exactly — the case where the old loop re-walked the graph
several times before giving up. The unfiltered row above is the permissive
end: a filter that admits everything costs one walk, and the engine-level
test `a_permissive_filter_answers_like_the_unfiltered_query` pins
filtered-everything to unfiltered exactly.

`sqlite-vec` is absent from these cases on purpose. Its `vec0` tables take an
optional `WHERE` on metadata columns, but wiring that into the harness would
compare two different questions; the filtered cases are about InlaySQL's own
filtered-walk cost, not a third-party comparison.

### Incremental maintenance

Since AHL-381 the graph is not rebuilt from scratch on every commit. An insert
appends one node through the same greedy search queries use; a delete leaves a
tombstone that is skipped by search and dropped by a later rebuild; and a full
rebuild happens only when tombstones outnumber live rows or the graph-shaping
parameters are retuned. This suite measures that maintenance directly against
`HnswIndex`, because the SQL path defers index commits to the first read and so
cannot show a per-row cost.

On one developer machine (Apple silicon), dim 384, 20,000 nodes, seed 42:

| Corpus | full rebuild | one incremental insert | distance computations (rebuild vs one insert) |
| --- | --- | --- | --- |
| realistic | 14.1 s | 1.04 ms | 255M vs 14,301 |
| uniform | 20.2 s | 1.31 ms | 430M vs 22,385 |

The distance computations are the guarantee, not the timing: one insert is
bounded by `ef_construction * M` and the layer count, independent of the corpus
size, where a full rebuild re-inserts every node. That count stays ~14,000 at
100,000 vectors too — the pin is the ignored unit test
`an_incremental_insert_into_a_large_graph_touches_a_fraction`, which asserts it
counts, not times.

### A corpus larger than RAM

The table above measures the in-RAM `HnswIndex`, which holds every embedding and
its normalised copy — the memory ceiling AHL-382 exists to remove. A paged
backend (`inlaysql_core::hnsw_paged::PagedHnswIndex`) stores each node's vector
and adjacency in the backing store and reads them through a bounded LRU cache on
demand, so the resident working set is the cache, not the corpus.

The suite now runs it directly (as it runs `HnswIndex` for incremental
maintenance, since the SQL path cannot show a per-query working set) and prints:

```
=== corpus larger than RAM: paged HNSW (direct) ===
corpus: N vectors x dim D = X MiB of f32
cache bound: 256 nodes (~Y MiB working set); peak resident: 256 nodes
resident / corpus: 256 / N nodes (Z% held in memory)
recall@10 vs exhaustive: R (shape)
```

Two numbers matter. The **resident/corpus ratio** is the memory claim: at the
default 2,000 documents the paged index holds 12.8% of the corpus in memory, and
at 20,000 it holds 1.3%, whatever `dim` is — the cache is fixed and the corpus
grows. The **recall@10** is the quality claim, reported against the same
exhaustive oracle both corpus shapes already use, and it is unchanged from the
in-RAM index's number because the paged graph is the same graph. The unit test
`a_corpus_larger_than_the_cache_is_searchable_with_bounded_memory` pins the same
bound as a count, which is the project's convention for a number that has to
survive a noisy machine.

The caveat: this backend is measured against `MemStorage` today, because the
copy-on-write `TreeStorage` deliberately does not expose its buffered writes to
reads within a transaction. Wiring the paged index onto `TreeStorage` — so it
inherits the write-ahead log and crash recovery — needs the engine to commit the
index's writes in bounded batches and to give the index a read-your-writes
overlay for the batch in flight. That is the follow-on step; the memory bound
and the recall are established here.

## Suite: concurrency — InlaySQL vs SQLite

"MVCC with multiple concurrent writers" is the headline claim against SQLite,
which permits one writer. This is the suite that has to be able to embarrass us,
so it runs real OS threads rather than simulating contention in a turn-taking
loop.

Several writers, one file, one row per transaction. Each thread opens its own
file handle and gets one of four WAL regions before the timed phase begins. A
short process-local gate orders conflict decisions, sequence/page reservations,
dirty-page/WAL writes and append positions; a normal commit publishes its
ready ticket before leaving the gate, and only the expensive `fsync` remains
outside it. Writers beyond four share a region safely because append placement
remains reserved.

The keys are disjoint. A stale transaction compares its touched rows against
the newer root and rebases when none changed; two writers touching the same row
still get first-committer-wins and the loser receives `Error::Conflict`.

On one developer machine, 200 transactions per writer:

| Writers | InlaySQL commits/s | Conflict rate | SQLite (journal, `sync=FULL`) |
| --- | --- | --- | --- |
| 1 | 173 | 0% | 78 |
| 2 | 237 | 0% | 79 |
| 4 | 221 | 0% | 78 |

Two things to read there:

**We are ~2.8x faster at four writers.** One `fsync` per InlaySQL commit,
overlapped across file handles, against SQLite's serialized journal-mode round
trip.

**Adding writers raises throughput.** Four writers do 1.28x the work of one on
this machine, with no false conflicts. Scaling is deliberately reported rather
than assumed: multiple `fsync` calls on one inode still share a filesystem and
storage device, so four regions are not expected to mean a perfect 4x.

The suite verifies, on every run, that the file ends up holding exactly the rows
the writers were told they committed. That check is not decoration: before
`Error::Conflict` existed, the engine reported a rolled-back transaction as
committed, and this suite would have printed a throughput number for writes that
were silently dropped. Two writers, ten inserts, five rows in the file, no error
anywhere.

**Also not covered:** a handle's snapshot only moves when it commits. An open
handle that never writes keeps answering from the state it opened on, so a
reader beside a writer goes stale and stays stale. The suite counts rows through
a freshly opened handle for that reason.

## Suite: retrieval

- **ingest** — rows per second, batched: the single-row `INSERT` loop runs
  inside explicit transactions, so it pays one `fsync` per batch rather than one
  per row (see the points suite's batched-write row). On one developer machine,
  2,000 docs ingest at ~1,650 docs/s.
- **index build on first read** — index commits are deferred until something
  reads, so the first query after a load pays for the whole batch. Reported on
  its own rather than folded into the query numbers.
- **query latency** — p50/p95/max for vector-only, BM25-only and hybrid
  queries.

The corpus and the queries are generated from a seeded PRNG, so `--seed 42`
asks exactly the same questions on every machine. The external comparison of
the same workload — against DuckDB and pgvector — is `./bench/compare.sh`.

## ./bench/compare.sh — DuckDB, pgvector, Meilisearch, MySQL and PostgreSQL

```sh
./bench/compare.sh                      # 5,000 docs, dim 128, 100 queries
DOCS=20000 DIM=384 ./bench/compare.sh   # override any parameter
ROWS=100 LOOKUPS=50 ./bench/compare.sh  # override the OLTP workload size
SERVER_CONCURRENCY_LEVELS=1,4,16 ./bench/compare.sh  # override the server-to-server concurrency levels
```

The whole design of this script is one idea: **generate the experiment once**.
The corpus, the queries and the correct answers are written to disk by the Rust
harness (`--export`), and every engine — including InlaySQL — then reads those
same files. It is far too easy to end up with four engines answering four
slightly different questions and to publish the difference as a performance
result. The OLTP workload below (`--export-oltp`) follows the identical idea:
the rows to load and the exact primary-key lookup sequence are written once,
and MySQL, PostgreSQL and InlaySQL all replay the same operations in the same
order. InlaySQL replays them *twice* — once on the host, once inside a
container — so the OLTP table below carries two InlaySQL rows; see "InlaySQL,
measured twice" under the OLTP section for why.

| | |
| --- | --- |
| `corpus.csv` | id, body, embedding as `[0.1,0.2,…]` — pgvector's own input format and a cast DuckDB accepts, so nothing is converted per engine |
| `queries.csv` | the query text and its embedding |
| `truth-vector.csv` | exhaustive cosine similarity: an objective answer |
| `truth-hybrid.csv` | RRF over exact vector and exact BM25: our reference fusion |
| `results-*.json` | one retrieval result file per engine, merged by `report.py` |
| `oltp-manifest.json` | rows, lookups, payload size, seed, write order, schema |
| `oltp-rows.csv` | the exact rows to load — sequential `id`, same payload every driver inserts |
| `oltp-lookup-keys.csv` | the exact primary-key lookup sequence, in order — the same seeded draw the `points` suite uses in-process |
| `results-oltp-*.json` | one OLTP result file per engine, merged by `report.py` — including `results-oltp-inlaysql.json` (host) and `results-oltp-inlaysql-container.json` (containerised), both written by the same binary against the same files |

Embeddings are written to six decimal places and InlaySQL is measured on the
values read back from that text, not on the full-precision originals —
otherwise part of any recall difference would just be the export format.

On one developer machine — 5,000 documents, dim 128, 100 queries, top-10:

| Engine | recall@10 | vector p50 | hybrid p50 | agree |
| --- | --- | --- | --- | --- |
| InlaySQL (HNSW + BM25) | 1.000 | 126 µs | **197 µs** | 0.988 |
| DuckDB (exhaustive + `fts` BM25) | 0.999 | 4.88 ms | 11.88 ms | 0.966 |
| DuckDB (`vss` HNSW + `fts` BM25) | 0.993 | 3.95 ms | 11.51 ms | 0.958 |
| Meilisearch (`arroy` ANN + its own ranking) | 0.997 | 1.22 ms | 4.04 ms | 0.419 |
| pgvector (HNSW + `ts_rank`) | 0.988 | 152 µs | 13.64 ms | 0.457 |
| pgvector (exhaustive + `ts_rank`) | 0.999 | 488 µs | 13.99 ms | 0.465 |

**We win hybrid by roughly 20x** against the nearest baseline (Meilisearch,
the dedicated search engine in this table) and by 60–70x against DuckDB and
pgvector, because it is one statement here and two queries plus client-side
fusion everywhere else — Meilisearch included, since its own built-in hybrid
mode is deliberately not what this table measures (see "Reading the table"
below). Against DuckDB/pgvector that multiple was ~10x when this table was
first written and ~14–17x an edition ago; most of the later jump is the BM25
index rewrite (`crates/inlaysql-core/src/bm25.rs`), which took our hybrid p50
from 875 µs to 191–197 µs while every baseline stayed put.

**Meilisearch's vector search is the fastest baseline recall-for-recall over
a network** — 1.22 ms against pgvector's 152 µs is not close, but pgvector's
number does not include building the equivalent of Meilisearch's typo
tolerance and ranking pipeline; read the two as different products, not two
points on one line. Its hybrid `agree` (0.419) lands in the same range as
pgvector's `ts_rank_cd` (0.457/0.465), for the reason "Reading the table"
below gives: neither ranks text with BM25.

**The vector-only loss to pgvector is gone, and was never a rout.** This table
used to read "pgvector beats us on vector search by 4x — over a network"; it is
now 198 µs to our 147 µs, on a run where the machine was busy and our own idle
measurement of the same index was 68.79 µs. Their number still includes a
client round trip a library in your own process does not pay, so read the
current gap as close in our favour rather than as a win worth quoting.

### Reading the table

**`recall@k` is a quality score.** It is measured against exhaustive cosine
similarity, which is not any engine's opinion.

**`agree` is not.** It is overlap with our reference fusion, and an engine that
ranks text with a different function scores lower without being worse.
PostgreSQL ranks with `ts_rank_cd` rather than BM25 and lands around 0.46 for
exactly that reason; Meilisearch ranks with its own rule chain (typo
tolerance, proximity, attribute, exactness — no BM25 in it at all) and lands
at 0.419, in the same range for the same reason. DuckDB's `fts` extension
does implement Okapi BM25, which is why it sits much higher. Read the
latencies as the result.

**The hybrid latencies are not measuring equal work.** InlaySQL fuses inside one
SQL statement. No baseline here has a fusion operator of its own that this
comparison uses — Meilisearch does have a built-in hybrid mode
(`semanticRatio`), but using it would score its *fusion algorithm* against
ours rather than isolating retrieval quality, so `meilisearch_driver.py`
runs vector-only and text-only as two separate requests and fuses them with
the identical `common.rrf` every other driver uses. So every baseline's
driver runs two queries and combines the ranks in Python, InlaySQL runs one
statement — what hybrid search costs the comparison way today, not one query
against one query.

Both DuckDB and pgvector are asked for their query plan, and what the plan
actually said — index scan or sequential scan — is printed with the row. An
"HNSW" row that was really a sequential scan would be the most misleading
number in the table, and it happens easily: DuckDB only rewrites `ORDER BY
array_cosine_distance(…)` into an index scan, never the equivalent
`array_cosine_similarity(…) DESC`. Meilisearch has no equivalent plan to
check — its vector search always goes through its own `arroy` ANN index,
with no exhaustive-scan option in the search API — which is also why it has
one row here instead of two.

### What it costs to reproduce

Docker, and pinned images: `pgvector/pgvector:pg17`, `getmeili/meilisearch:v1.53`,
`postgres:17`, `mysql:8`, `python:3.12-slim`, `duckdb==1.1.3`,
`mysql-connector-python==9.1.0`, `requests==2.34.2`, plus
`docker/Dockerfile`'s `rust:1.91-bookworm` — the same pinned Linux build image
`docker/test.sh` uses, reused rather than a second one, for the containerised
InlaySQL OLTP row below *and* for `inlaysql-server`, the container that runs
`inlaysql serve --mysql` for the server-to-server comparison further down.
The `pgvector` container runs with `fsync=off` on a tmpfs, because *that* row
measures query latency rather than PostgreSQL's durability — the InlaySQL
numbers it is compared against there are query latencies too. That is a
different container from the `postgres` service the OLTP section below talks
to, which is configured for real durability. The `points` suite is the other
durability comparison, in-process against SQLite, with real barriers on both
sides.

## OLTP: MySQL and PostgreSQL, matched durability

```sh
./bench/compare.sh                       # 20,000 rows, 5,000 lookups by default
ROWS=2000 LOOKUPS=500 ./bench/compare.sh # override the workload size
```

> [!NOTE]
> **This section used to carry a warning that the write column could not be
> trusted at all, because InlaySQL was measured on the host and MySQL/
> PostgreSQL inside containers, which on Docker Desktop for macOS or Windows
> fsync to a virtualised disk that does not generally pass a write barrier
> through to the hardware.** That is still true of the *host* InlaySQL row
> below — it is not a criticism of the row, it is what makes it the
> like-for-like column against the `points` suite's SQLite row. The fix was
> not to make InlaySQL's host fsync cheaper — that would defeat the point —
> but to add a second InlaySQL row, measured inside a container on the same
> class of Docker volume MySQL and PostgreSQL already write to, so the write
> column compares like fsync semantics against like. See "InlaySQL, measured
> twice" below for exactly what that does and does not prove.

The `points` suite (above) measures the narrowest OLTP workload — one row by
primary key, read and written — against SQLite, in-process. MySQL and
PostgreSQL cannot link into the harness, so the same workload is exported
once (`--export-oltp`, written alongside the retrieval corpus) and replayed
by a driver against each server inside `bench/compare.sh`, exactly the way
DuckDB and pgvector already are for retrieval.

**Scope.** Only the workload that has a durable, one-commit-per-statement
counterpart on a server: sequential inserts (`id` 1..=rows, in that order)
and the point-lookup key sequence — the same rows and the same seeded
lookup draw the `points` suite uses for its non-batched write and read rows.
The `points` suite's *batched*-write row (many rows folded into one
transaction) is InlaySQL-specific and stays out of this comparison: there is
no natural equivalent on MySQL/PostgreSQL without picking an arbitrary batch
size for them too, which would make the comparison about the chosen batch
size rather than about the engines.

### Durability — the thing that decides whether this comparison means anything

Comparing our durable commit against a server told not to be durable would be
the most misleading number in this repo, so every engine here is configured
for real durability, matched as closely as each engine allows:

| Engine | Setting | Why |
| --- | --- | --- |
| InlaySQL (host) | No knob — every commit is synced before `execute_prepared` returns | This is the baseline everything else is matched to. Runs on the host filesystem, before the containers come up. |
| InlaySQL (containerised) | The same commit path, same binary, same workload — the only difference is that this process and its database file run inside the `inlaysql-oltp` container, on the named Docker volume `inlaysql-oltp-data` | See "InlaySQL, measured twice" below. This row exists to be comparable to the MySQL/PostgreSQL rows, which also pay whatever the virtualised disk charges. |
| SQLite (reference, from the `points` suite) | `journal`, `synchronous=FULL`, `fullfsync` | The like-for-like column the `points` suite already establishes; repeated here so the OLTP row is read against the same standard. |
| PostgreSQL | `fsync=on`, `synchronous_commit=on` | PostgreSQL's real durability: every commit is confirmed only after WAL is on disk. This is the opposite configuration from the `pgvector` container above, which explicitly disables both to measure query latency — see the note above. |
| MySQL | `innodb-flush-log-at-trx-commit=1` | InnoDB's most durable setting: the log buffer is written and fsynced to disk at every transaction commit. This is already MySQL's default; it is set explicitly in `compose.yml` so the comparison does not silently depend on that default never changing. |
| MySQL binary log | Disabled (`--skip-log-bin`), not `sync_binlog=1` | InlaySQL has no replication log to compare against. Enabling the binlog with `sync_binlog=1` would add a second fsync per commit for a durability feature neither engine in this comparison uses — that would be measuring replication safety, not the plain commit path this row is about. If a future comparison specifically targets replication-safe MySQL, that is a different, explicitly-labelled row, not this one. |

**Not tmpfs.** The `postgres`, `mysql` and `inlaysql-oltp` services in
`compose.yml` all write to normal named Docker volumes (`postgres-oltp-data`,
`mysql-oltp-data`, `inlaysql-oltp-data`), not tmpfs — a durable write to a RAM
disk is not a durable write. This is the opposite choice from the `pgvector`
container above, which uses tmpfs on purpose because that row is explicitly
not a durability measurement.

### InlaySQL, measured twice: host and containerised

`oltp_export::run` (the `--export-oltp` step `compare.sh` runs before the
containers come up) measures InlaySQL on the host filesystem — that row is
the like-for-like column against the `points` suite's SQLite row, and its
`fsync` is whatever barrier the host actually honours. `oltp_export::replay`,
run by the `inlaysql-oltp` service after the corpus exists, measures InlaySQL
a second time: identical rows, identical lookup-key sequence, identical
commit path, but this process runs inside a container built from
`docker/Dockerfile` — the same pinned Linux image `docker/test.sh` uses — and
its database file lives on the named Docker volume `inlaysql-oltp-data`,
deliberately not the `/corpus` bind mount the exported workload files are
read from. `postgres-oltp-data` and `mysql-oltp-data` are the same kind of
volume, so the containerised InlaySQL row's `fsync` crosses whatever boundary
theirs does.

**What that buys.** Before this, only two of the three engines in the write
column paid the virtualised-disk cost the note above describes; the
comparison was between a barrier and something that might not be one. Now
every engine in the OLTP table pays the same disk, whatever it turns out to
cost — the comparison is *internally consistent* in a way it was not before,
even though nobody here has independently confirmed what that disk promises.

**What it does not buy.** Comparable is not the same as hardware-durable. The
containerised InlaySQL row is not proven to survive a power cut any more than
the MySQL/PostgreSQL rows are, on Docker Desktop for macOS or Windows — this
repo cannot verify that a container's `fsync` reaches the platter, only that
whatever it does reach, all three engines now reach identically. Nor does the
container remove the structural asymmetry described below: InlaySQL stays a
library linked into its own process even inside its own container, so it
never pays the socket round trip MySQL and PostgreSQL do. Crediting the
containerised row with beating MySQL/PostgreSQL because it removed the fsync
gap would be honest; crediting it for removing the transport gap would not
be — that gap is structural and this change does not touch it.

**The gap between the two InlaySQL rows is itself the measurement that
justifies trusting the containerised one.** If the host row is close to the
containerised row, the virtualised disk on this machine is not doing much
work and the earlier concern was overstated for this Docker configuration. If
the containerised row is many times faster than the host row — the shape the
original 188 vs. 1,318/1,222 gap suggested — that is direct, on-this-machine
evidence of what the virtualised fsync costs, measured with the *same*
engine on both sides of the boundary rather than inferred by comparing two
different engines. Report both rows; do not report only the faster one.

**Where the match is not exact, and can't be, without changing the OS.**
InlaySQL's `File::sync_all` issues `F_FULLFSYNC` on macOS — a real barrier
through the drive's write cache, per the `points` suite's own note above.
Whether the PostgreSQL and MySQL server processes inside their containers
reach an equivalent barrier depends on the container runtime and the host
filesystem backing the Docker volume, not on anything this repo controls;
`fsync=on` and `innodb-flush-log-at-trx-commit=1` are the strongest settings
each engine exposes, and that is what is asked for and documented, but this
repo cannot verify the host honours `fsync` as a real barrier the way it can
verify its own `F_FULLFSYNC` call. State this plainly rather than implying a
guarantee the harness cannot make.

### The structural asymmetry that cannot be removed

InlaySQL is a library in the caller's own process — in both of its rows, host
and containerised. MySQL and PostgreSQL are servers, reached over the Docker
compose network — every number the `mysql_driver.py` and
`postgres_oltp_driver.py` drivers report includes a client/server round trip
that neither InlaySQL number pays. That biases every MySQL/PostgreSQL row
toward looking slower than either engine would be reached over a faster
transport (a Unix socket, or a real network rather than a container bridge)
— the same caveat the pgvector retrieval row above already carries, and for
the same reason. It is a genuine difference between the two designs, not a
measurement artifact, and running InlaySQL inside a container alongside the
servers does not touch it: it cannot be engineered away without giving
InlaySQL a server to compare against instead of a library call — which is
exactly what "Server-to-server" below now does, over `inlaysql serve --mysql`,
so both sides pay a socket round trip and the asymmetry disappears from that
table alone.

**Quantified, 2026-08-30, with InlaySQL as its own control.** `inlaysql
serve --mysql` at one connection (the "Server-to-server" section below)
writes at 1,795.6 µs/commit over the same wire protocol MySQL's row pays;
the containerised library row above writes at 1,177.0-1,369.3 µs/commit
across two runs. That gap, ~420-620 µs, is transport and driver overhead
this section's library row does not pay and both MySQL and PostgreSQL do,
on every statement — the same order of magnitude as `BENCHMARK.md`'s entire
published PostgreSQL write gap. It is large enough that a transport-matched
comparison would very likely reverse part of the published write ordering,
not just narrow it. See `BENCHMARK.md`'s "Against MySQL and PostgreSQL"
correction and `PERF.md`'s 2026-08-30 section for the full measurement,
including why it is not this engine's commit-path CPU cost either (`fsync`
is 88-89% of a containerised commit, matching the host's 97.1%).

### Tuning — now matched

Found auditing `compose.yml` for `SCOREBOARD.md` (2026-08-31): the `postgres`
service runs with `shared_buffers=512MB` (`compose.yml`), roughly 4x
PostgreSQL's own ~128MB stock default, while the `mysql` service got no
equivalent bump — its command was exactly
`--innodb-flush-log-at-trx-commit=1 --skip-log-bin`, so
`innodb_buffer_pool_size` sat at MySQL 8's stock 128MB. A reviewer would
reasonably ask why one server was tuned and the other was not, in the same
file, for the same comparison.

**Fixed the same day it was found**, not just recommended: `mysql`'s command
now also carries `--innodb-buffer-pool-size=512M` — the same absolute value
as `postgres`'s `shared_buffers=512MB`, which is also the same *multiple* of
each engine's own stock default (both ~128MB, so both are now ~4x stock).
Matching the multiple rather than picking two independently-reasonable
numbers is the point: a reader comparing this file against a future one
should be able to tell "both tuned the same way" from the values alone
without re-deriving what each engine's stock baseline was. Durability is
untouched — `innodb-flush-log-at-trx-commit=1` still stands, so this is a
cache-size change only, not a durability change.

**Likely inert for the numbers published today, for the same reason the
transport tax above is not this engine's CPU cost:** the OLTP workload is
20,000 rows of a short `body TEXT`, comfortably resident in either engine's
*stock* buffer cache, let alone a bumped one, and every write-path profile in
this document and `PERF.md` found the commit path `fsync`-dominated (88-97%
of commit time), leaving little room for a buffer-pool difference to move a
single-row-commit number — so this fix is not expected to move any figure
already published against these servers. **It matters for any future
indexed-range-scan, join, or aggregate harness against these servers** — a
working set that overflows a stock 128MB but fits a tuned 512MB would have
made that comparison about the tuning choice, not the engine, which is
exactly the asymmetry this closes before such a harness is ever built. Still
unmatched, and named for the same reason, because it cuts the *other* way
(against MySQL, not in its favour, so it does not need the same urgency):
`innodb_flush_method` is left at MySQL's own default rather than `O_DIRECT`,
which costs MySQL a double-buffered write through the OS page cache a tuned
deployment would skip. See `SCOREBOARD.md` §4.3 for the fuller fairness audit
this was found during.

### What today's rerun found, and what a fair comparison needs

A same-session rerun of this section's own drivers, done to check whether
the published table reproduces, did not reproduce it: the ordering between
MySQL and PostgreSQL flipped and the multiple against both shrank by about a
third, traced to the Docker volume's own `fsync` cost drifting 1.5-1.8x
within one sitting (`PERF.md`'s 2026-08-30 section has the numbers and the
timing). **The recommended fix is methodological, not a bigger sample of the
same method:** interleave InlaySQL, MySQL and PostgreSQL within one session
rather than running each to completion in turn, repeat several times, on a
quiet machine, and publish the median and the spread the same way
`REPEATS=5 ./bench/repeat.sh` already does for `run.sh`.

**Half of that recommendation is now shipped, and half is not.** `compare.sh`
has the quiet-machine gate (`bench/load_gate.sh`, shared with `run.sh`), and
`REPEATS=5 ./bench/repeat-compare.sh` runs it repeatedly and reports the
median and the spread through the same `bench/summarise.py`. What is *not*
addressed: interleaving the engines **within** one run. `compare.sh`'s phase
order is fixed — each engine's driver runs to completion in turn — so the
repeat wrapper repeats whole passes rather than alternating engines, and the
`fsync`-cost drift described above happens on a timescale that a single pass
still straddles. The numbers on this page predate both scripts and are still
a single ungated sequential pass; they are owed a regeneration, not a
footnote.

Both drivers prepare their statements once, outside the timed loop, and bind
per iteration (MySQL via the connector's binary-protocol prepared cursor,
PostgreSQL via psycopg's server-side `prepare=True`) — the same "prepare
once" methodology the `points` suite already uses for InlaySQL and SQLite, so
none of the three engines is paying a parse cost the others are not.

### Numbers

Not included here. Per this repo's rule, a number only belongs in this file
once it regenerates from `./bench/compare.sh` on a machine that is not
otherwise under load — the methodology above, including the containerised
InlaySQL row, was verified end to end (both InlaySQL rows land, MySQL and
PostgreSQL land, `report.py` merges all four without special-casing) while
other work was running concurrently on the development machine, which makes
any timing from that run meaningless to publish. Run `./bench/compare.sh`
yourself, on a quiet machine, to produce the first real OLTP table — it will
have four rows: InlaySQL (host), InlaySQL (containerised), MySQL, PostgreSQL.

## Server-to-server: InlaySQL's own MySQL wire against MySQL's (AHL-489)

```sh
./bench/compare.sh                                    # concurrency 1, 8; 2,000 rows, 1,000 lookups by default
SERVER_CONCURRENCY_LEVELS=1,4,16 ./bench/compare.sh    # override the concurrency levels
SERVER_ROWS=500 SERVER_LOOKUPS=200 ./bench/compare.sh  # override the workload size
```

Every row above this one, including the OLTP section's, measures InlaySQL as
a library linked into the caller's own process against MySQL's and
PostgreSQL's socket round trip — the "structural asymmetry" section above
states that plainly rather than hiding it, and `BENCHMARK.md` calls it the
missing apples-to-apples number. `inlaysql serve --mysql` exists now
(`docs/server.md`) and a stock ORM's migrations and CRUD run over it
(AHL-474/475/476), so this section removes the asymmetry for the one
comparison it is possible to remove it for: **InlaySQL never appears as a
library here.** `bench/external/server_driver.py` reaches
`inlaysql-server:3306` with `mysql.connector` — the identical client library,
the identical prepared-statement/parameter-binding/result-decoding code path
`mysql_driver.py` already uses to reach `mysql:3306` — because `inlaysql
serve --mysql` speaks the real MySQL wire protocol rather than an
approximation of it, so nothing about the client has to change to point it
somewhere else.

### What is measured

A sysbench-shaped point read/write mix, not sysbench itself: prepared point
reads by primary key, and single-row durable writes (`INSERT`, autocommit,
one commit per row) — the same two operations the OLTP section above already
measures, over the same exported workload (`oltp-rows.csv`,
`oltp-lookup-keys.csv`), run at **a couple of connection counts** instead of
one. Each concurrency level opens that many **spawned OS processes**, one
`mysql.connector` connection and one prepared statement per process; writers
get disjoint, contiguous id ranges (process *i* writes a contiguous slice of
the row list, never a shared queue) and readers get disjoint, contiguous
slices of the lookup-key sequence, so raising the connection count changes
how many connections are open, not which rows or keys any one of them touches.
Process isolation removes `mysql.connector`'s Python-thread GIL from the
concurrency measurement. Process creation, connection setup and teardown are
inside each phase's wall-clock span, matching the earlier threaded driver's
boundary rather than hiding startup overhead. Throughput is total operations
over the wall-clock span of the whole concurrent phase, matching how
`write_oltp_result`'s ops/s is computed above. Default levels are **1 and 8**;
override with `SERVER_CONCURRENCY_LEVELS` (comma-separated).

The earlier threaded measurement (AHL-495) is retired as a baseline: it could
not distinguish server scheduling from the client library's own GIL-bound
threading. Results produced by the current driver are process-based and
should not be compared numerically with that retired row.

**The workload size here is deliberately smaller than the OLTP section's own
`ROWS`/`LOOKUPS`, and is its own separate knob** (`SERVER_ROWS`/
`SERVER_LOOKUPS`, defaulting to 2,000 rows and 1,000 lookups against the OLTP
section's 20,000/5,000). This driver measures **two engines at every
concurrency level**, where the single-connection drivers above measure one
engine once — reusing the full workload size unchanged would multiply an
already-durable, one-fsync-per-row write phase by
`len(concurrency levels) × 2 engines`, which is minutes at the top-level
defaults and does not fit inside `trust.yml`'s benchmarks job, which budgets
one hour for `bench/run.sh` and the whole of `bench/compare.sh` together.
`server_driver.py` still reads the identical exported `oltp-rows.csv`
(a bounded prefix of it) and the identical seeded `oltp-lookup-keys.csv`
sequence (filtered down to the keys that land inside that prefix, in their
original order, not resliced) — the same "generate the experiment once"
rows and questions the OLTP section's drivers already use, just not every
one of them. The written result's own `lookups` field always reports the
count that actually ran, since filtering can leave it below `SERVER_LOOKUPS`
when the range shrinks a lot.

### Durability — matched the same way the section above matches it

| Engine | Setting | Why |
| --- | --- | --- |
| MySQL | `innodb-flush-log-at-trx-commit=1`, binlog off | Unchanged from the OLTP section above — this is the same `mysql` container, reached a second way. |
| InlaySQL (server) | No knob — every commit is synced before the statement's `COM_STMT_EXECUTE` reply is sent | The server has no durability option of its own to set (`docs/server.md`): every connection opens the same `Database` the library API opens, and every commit takes the same synced-before-return path the host and containerised OLTP rows already measure. This *is* InlaySQL's most durable setting, because it is InlaySQL's only setting. |

PostgreSQL has no row in this table. InlaySQL speaks the MySQL wire
protocol, not PostgreSQL's (`docs/server.md`), so there is no InlaySQL server
to put on the other end of a `psycopg` connection — the PostgreSQL row stays
in the OLTP section above, in-process against a socket, the only comparison
that exists to make.

### What still is not comparable, even server-to-server

Removing the library-vs-socket asymmetry does not remove every difference
between these two servers. Four remain, stated here rather than left for a
reader to discover in a suspicious ratio:

- **The concurrency model is not the same shape.** `inlaysql-server` is
  thread-per-connection — one OS thread and one `Database` handle per
  connection, blocking I/O, no thread pool, capped by `--max-connections`
  (`docs/server.md`, `crates/inlaysql-server/src/lib.rs`). MySQL schedules
  many connections onto a bounded worker pool. At low concurrency (the
  default's first level, 1) that difference is invisible. At high
  concurrency it is real: an OS thread per connection has scheduling and
  context-switch costs a pooled server does not pay, and nothing in this
  harness — or in InlaySQL — currently amortises them. **This is a
  structural property of the two designs, not a tuning gap**, and a widening
  gap between concurrency levels in this table should be read that way,
  not as evidence InlaySQL was misconfigured.
- **One shared credential, on both sides, but for different reasons.** Both
  containers here are configured with a single username and password. For
  MySQL that is this benchmark's own setup — a real multi-user grant system
  sitting unused. For InlaySQL it is the whole of what exists: there is no
  user table, no per-table permission, no grants at all
  (`docs/server.md`). This table cannot measure that gap because neither
  side is asked to exercise per-user permissions; it is named here so the
  matched-credentials setup is not mistaken for the two servers having
  equivalent auth models.
- **No TLS, on either side, for different reasons again.** MySQL's
  container has no TLS configured for this comparison. InlaySQL's wire
  protocol does not implement TLS at all yet and never advertises
  `CLIENT_SSL`, so a client cannot negotiate it even if asked
  (`docs/server.md`). Both sides are plaintext here; only one side has the
  option not to be.
- **A single shared database file, opened by every connection.** Every
  `inlaysql-server` connection in this table opens the same file
  (`/data/bench-oltp-server.inlay`), matching how a real deployment would
  point every client at one server. Disjoint write ranges keep
  first-committer-wins conflicts at (or near) zero in this workload — see
  `write_retries` in the output, which counts them rather than silently
  retrying them away — but a workload with real key contention would see
  MySQL's row-lock waiting and InlaySQL's optimistic-retry-on-conflict
  behave differently under the same concurrency number, and this table does
  not exercise that shape.

### Numbers

Not included here, for the same reason the OLTP section above withholds its
own: this file describes methodology, and `BENCHMARK.md` is where a
regenerated number lives. The current run is published there — reads 1.03x
MySQL at one connection and a dead heat at eight, writes 0.54x and 0.17x —
measured with eleven unrelated containers on the machine, which is stated on
that page and is the reason a repeat on a quiet machine is still worth doing.
The first such run (AHL-495) read 1.52x and 1.10x on reads under a load average
of 5.4; neither run had a controlled machine, and the read margin is inside the
spread between them. Run `./bench/compare.sh` yourself to reproduce it.

## Read shapes and batch insert: MySQL and PostgreSQL, unix socket (2026-08-31)

```sh
docker compose -f bench/external/compose.yml up -d postgres mysql drivers
docker exec inlaysql-bench-drivers-1 \
  sh -c 'TARGET=mysql REPS=5 python /drivers/read_driver.py'     # range + aggregate + join
docker exec inlaysql-bench-drivers-1 \
  sh -c 'TARGET=postgres REPS=5 python /drivers/batch_driver.py' # batch insert + c/fsync
cargo run --release -p inlaysql-bench --bin sql_shapes           # InlaySQL's side (agg|batch)
```

`read_driver.py` and `batch_driver.py` fill the MySQL/PostgreSQL scoreboard
cells for indexed range scan, two-table join, aggregate and batch insert —
the workloads whose InlaySQL numbers came from `SUITE=indexed`/`SUITE=joins`
(range, join) and from `sql_shapes` (aggregate, batch: the two shapes that
had no Rust suite on *any* side, so the shape is defined by these drivers and
pinned by the published cells). Transport is a shared unix-socket volume
(`db-sockets` in `compose.yml`) mounted into `mysql`, `postgres` and
`drivers` — matched transport for both servers, per the scoreboard's
cell-filling rules.

Method, pre-fixed: REPS repetitions (default 5) with the full `(shape, rep)`
schedule Fisher-Yates-shuffled from a fixed seed; medians and ranges
published, never a single run; row counts asserted before anything is timed
(a shape returning the wrong row count refuses to time rather than timing a
wrong answer); durability aligned (`innodb_flush_log_at_trx_commit=1`,
`synchronous_commit=on`, InlaySQL `Durability::Full`). InlaySQL is
in-process while the servers sit behind a socket — the asymmetry favours
InlaySQL, so its losses here are conservative. The full-join shapes are
timed as server-side `COUNT(*)` wrappers because a Python client fetching
160,000 rows per execution measures mysql-connector's per-row cost, not the
engine; see `BENCHMARK.md` for the full disclosure.

`sql_shapes` deliberately duplicates none of `indexed`/`joins`' shapes —
those cells' InlaySQL numbers come from the Rust suites, and this binary
exists only for aggregate and batch insert so the two sides cannot drift.

## ann-benchmarks — an external corpus, an external ground truth, an external protocol

Everything above this line is ours. Our harness, our corpus, our oracle, our
machine. `BENCHMARK.md` says so on every page, and saying so does not fix it:
a benchmark whose data, protocol and definition of "correct" all come from the
engine's own authors cannot be checked by anyone who did not write the engine.
Two of this repository's own numbers have already been wrong in exactly that
way — a published 10M-vector memory estimate that understated by 2.3x, and a
"we trail on writes" row that turned out to be measuring `fsync` policy rather
than engines.

[`ann-benchmarks`](https://github.com/erikbern/ann-benchmarks) is the field's
common ground for approximate nearest neighbour search. It removes all three at
once:

* **An external corpus.** Fixed datasets published as HDF5 (`glove-*-angular`,
  `nytimes-256-angular`, `sift-128-euclidean`, ...), downloaded byte-for-byte
  from `ann-benchmarks.com`. Nothing here generates data.
* **An external ground truth.** Each file carries its own `neighbors` and
  `distances` arrays — the exact answer, computed by somebody else. The
  `vectors` suite above scores InlaySQL against an oracle this repository
  computes; this one never does.
* **An external protocol.** recall@k against those arrays, QPS as
  `1 / best_search_time`, and a parameter sweep, all defined upstream. Every
  other engine on the ann-benchmarks leaderboard was measured this way, so the
  output is comparable to their published runs without a translation step.

```sh
python3 -m venv bench/ann/.venv
bench/ann/.venv/bin/pip install -r bench/ann/requirements.txt
SDKROOT=$(xcrun --show-sdk-path) cargo build --release -p inlaysql-mcp --bin inlaysql

bench/ann/.venv/bin/python bench/ann/run.py --dataset random-xs-20-angular   # ~1 min, smoke
bench/ann/.venv/bin/python bench/ann/run.py --dataset glove-25-angular       # the real one
bench/ann/.venv/bin/python bench/ann/run.py --dataset glove-25-angular --quantization int8
```

The dataset downloads on first use into `bench/ann/data/` and the results land
in `bench/ann/results/<dataset>/<k>/inlaysql/` — both git-ignored, both in
`ann-benchmarks`' own layout, so an `ann-benchmarks` checkout can plot or
export these files next to every other engine's without converting anything.

### Inside ann-benchmarks proper

`bench/ann/run.py` is the protocol in one file so the adapter can be run from a
checkout of *this* repository. The adapter itself is a plain `ann-benchmarks`
plugin and belongs upstream:

```sh
git clone https://github.com/erikbern/ann-benchmarks && cd ann-benchmarks
mkdir -p ann_benchmarks/algorithms/inlaysql
cp /path/to/inlaysql/bench/ann/{__init__.py,module.py,config.yml,Dockerfile} \
   ann_benchmarks/algorithms/inlaysql/
python install.py --algorithm inlaysql          # builds bench/ann/Dockerfile
python run.py --dataset glove-25-angular --algorithm inlaysql
python plot.py --dataset glove-25-angular
```

Four files, not the directory: `run.py`, `requirements.txt`, the downloaded
corpora and the virtualenv are this repository's scaffolding for running the
same adapter without `ann-benchmarks` installed, and none of them belongs in
`ann_benchmarks/algorithms/`.

`bench/ann/Dockerfile` pins the engine by revision (`--build-arg
INLAYSQL_REV=...`) so a rebuilt image measures the same engine.

### The seam: the MySQL wire protocol

`bench/ann/module.py` reaches the engine through `inlaysql serve --mysql` over
an ordinary MySQL client connection, running ordinary SQL:

```sql
CREATE TABLE items (id INTEGER PRIMARY KEY, embedding VECTOR(25));
CREATE INDEX items_embedding ON items (embedding);
INSERT INTO items (id, embedding) VALUES (0, vector('[...]')), ...;
SELECT id, vector_score(embedding, vector('[...]')) AS score
  FROM items ORDER BY score DESC LIMIT 10;
```

No private entry point, and no Rust written for the benchmark — the number has
to be what a user gets, not what an internal API can be made to do. It is also
the only seam that reaches the engine from Python at all: InlaySQL ships no
Python binding and no C API, so the alternative was the MCP JSON-RPC server,
which is row-limited and built for language models. The same shape as
`ann-benchmarks`' own `pgvector` plugin, which drives a local PostgreSQL over
`psycopg`; both pay a loopback round trip per query that an in-process index
does not. Measured on this machine, that round trip is **0.037 ms** (`SELECT 1`
p50 over 3,000 calls on the same connection) against a 0.331 ms glove query —
about 11%, plus 11 µs of client-side embedding formatting. Real, disclosed, and
not subtracted.

The adapter spawns the server itself on a port the OS picks, so there is no
daemon to manage and no port to collide with. `INLAYSQL_HOST`/`INLAYSQL_PORT`
point it at one that is already running instead.

### Numbers: glove-25-angular

1,183,514 vectors x dim 25, 10,000 queries, k = 10, three runs, on one
Apple-silicon developer machine. **Recall is against the dataset's own
`distances` array, not ours.**

Exact `VECTOR(25)` — build 294.9 s (36.2 s loading over the wire, **258.7 s
building the graph on the first read**), index 1,047 MiB (928 B/vector, 9.3x
the raw `f32` corpus), server RSS 1,056 MiB:

```
over_fetch     ef   recall@10        QPS    p50 ms    p95 ms
         1     64      0.9878     3021.1     0.331     0.357
         2     64      0.9974     1974.2     0.507     0.551
         4     80      0.9996     1178.5     0.850     0.938
         8    160      1.0000      653.2     1.534     1.699
        16    320      1.0000      357.5     2.794     3.116
        32    640      1.0000      195.2     5.114     5.674
        64   1280      1.0000      103.6     9.647    10.745
```

Quantised `VECTOR(25, INT8)` — build 461.3 s, server RSS after build 790 MiB:

```
over_fetch     ef   recall@10        QPS    p50 ms    p95 ms
         1     64      0.9860     2823.3     0.356     0.390
         2     64      0.9955     1835.6     0.548     0.615
         4     80      0.9978     1103.8     0.911     1.040
         8    160      0.9982      618.6     1.622     1.878
        16    320      0.9982      342.0     2.924     3.398
        32    640      0.9982      187.4     5.330     6.180
        64   1280      0.9982      101.1     9.880    11.433
```

The two together are the more interesting result. On external data, int8 costs
**1.56x the build time** (461 s against 295 s) and buys **1.34x less resident
memory** (790 MiB against 1,056 MiB), and its recall **stops at 0.9982** — the
quantisation error floor, which no amount of over-fetching gets back, where
exact reaches 1.0000 at `over_fetch = 8`. At the operating point most people
would pick (`over_fetch = 1`, recall ~0.987) it is also ~7% *slower* per query,
because the graph walk is the cost and int8 does not shorten it. The "int8 is
smaller" half of the trade holds on this corpus; nothing about it is faster.

The exact run above is a repeat: an earlier run of the same command built in
294.7 s, reported the identical recall to four decimals at every point, and QPS
within 1.7%. The curve is stable on this machine.

`random-xs-20-angular` (9,000 x 20, ann-benchmarks' own smoke dataset) is
recall 1.0000 at every point, 9,926 QPS at `over_fetch = 1`, 0.8 s to build. It
proves the wiring, not the index.

**These are not directly comparable in absolute terms to the QPS numbers on
ann-benchmarks.com.** Those are run on a fixed cloud instance type; this is a
laptop. What *is* comparable is the shape — the recall/QPS curve, measured the
same way, on the same data, against the same truth — and anyone can regenerate
it on their own machine and put both on the same axes.

### What the exercise exposed

Every one of these is a finding about the engine, reported rather than routed
around.

* ~~**Cosine only, so most of the standard datasets cannot be run at all.**~~
  **Closed.** It was true when this was written: `vector_score` was cosine
  similarity and the SQL surface had no other scorer, so
  `sift-128-euclidean` and `fashion-mnist-784-euclidean` — the two datasets
  everyone starts with — were refused by the constructor rather than scored
  against a ground truth built with a metric the engine did not implement,
  and only the `-angular` datasets were answerable. That is why
  `glove-25-angular` is the headline here.

  A vector index now carries the distance it was built under, chosen at
  `CREATE INDEX` with pgvector's operator class
  (`... ON items USING hnsw (embedding vector_l2_ops)`), and `module.py` maps
  the dataset's own metric onto it — so the `-euclidean` datasets run. The
  headline number above is unchanged and was not re-run: a cosine index writes
  the same graph format and computes the same score, bit for bit, that it did
  before the metric existed. There is still no `vector_ip_ops`: inner product
  is not a metric, an HNSW graph on it is a known approximation, and this
  engine refuses it with that reason rather than shipping it quietly. Anything
  that is neither angular nor euclidean — `hamming`, `jaccard` — is still
  refused by the constructor.
* ~~**No `ef_search` knob outside Rust.**~~ **Closed for `ef_search`; still
  open for `m` and `ef_construction`.** It was true when this was written:
  `HnswParams` had all four knobs and **none of them was reachable from SQL**,
  so the sweep had to be an **over-fetch factor** — ask for `k * over_fetch`
  rows, keep the first `k`. That produced a real curve but a blunt one, and the
  tables above show why: `over_fetch` 1 and 2 land on the same graph walk at
  `k = 10`, because the walk is `max(ef_search, 2k)` and both clamp to
  `ef = 64`. Two sweep points, one measurement.

  `set_query_arguments` now sweeps `SET inlaysql_hnsw_ef_search`, which is the
  same dial the `pgvector` plugin sweeps as `SET hnsw.ef_search`, at the same
  values, so every point is a different walk and the two engines' curves are
  sampled at the same operating points. `EXPLAIN` reports the `ef` each query
  will run at, so the number in the results is checkable against the engine
  rather than derived from a formula in the harness — which is what the `ef`
  column above was.

  **The tables above were measured with the old over-fetch sweep and have not
  been re-run.** They are still true readings of the engine at those operating
  points; they are indexed by a factor rather than by `ef`, and the `ef` column
  is the harness's arithmetic rather than the engine's answer.

  `m` and `ef_construction` remain Rust-only. Unlike `ef_search` they shape the
  stored graph rather than one query, so reaching them from `CREATE INDEX`
  needs a catalog format change to record them per index and a rebuild to
  apply them. That is why there is still no graph-shape grid in `config.yml`,
  where pgvector's varies `M`.
* **Embeddings are bound as parameters — fixed (AHL-478).** This used to read
  "embeddings cannot be bound": a string parameter into a `VECTOR` column was
  `1366: column is VECTOR(n) but the value is TEXT` whatever was bound to it,
  and `vector_score(embedding, ?)` failed the same way, so every embedding had
  to cross as a `vector('[...]')` decimal-text literal the server re-parsed.

  An embedding now binds as `dim` little-endian `f32`s in a string parameter —
  `numpy`'s own buffer, sent as `bytes`, which is MySQL 9's `VECTOR` storage
  format. Which `?` is an embedding comes from the statement rather than from
  the packet, because the MySQL protocol has no vector type code any driver in
  the field emits; see `docs/server.md`, "Binding a `VECTOR` parameter".

  Both halves measured on glove-25 (1,183,514 x 25, 112.9 MiB of raw `f32`),
  same machine, same batching, bytes read off the **server's** own
  `Bytes_received` counter rather than estimated from the SQL the client built:

  | Load path | Bytes on the wire | vs corpus | Load time |
  | --- | --- | --- | --- |
  | `vector('[...]')` decimal text | 363.9 MiB | 3.22x | 41.20 s |
  | bound packed `f32` | 127.9 MiB | **1.13x** | **19.77 s** |

  2.85x fewer bytes and 2.08x faster to load. The 363.9 MiB figure is the same
  one this section reported before the fix, reproduced by re-running the old
  path. Per query, formatting the query embedding fell from 11-18 µs of decimal
  formatting to about 0.3 µs of `tobytes()` — reported as
  `inlaysql_embedding_pack_us`, still inside the timed region because it is
  inside a user's timed region too. `run.py` prints the wire figure for every
  run, so the claim stays measured rather than remembered.

  **The recall/QPS tables above were measured with the old inlining path and
  have not been re-run.** They are still true readings: the bytes stored and the
  graph built are identical either way — the change is how the same `f32`s reach
  the server — so recall is unaffected, and the QPS numbers are if anything
  pessimistic by the 11-18 µs of formatting that is no longer paid.
* **A transaction may not write more than 1 MiB.** `WAL_BLOCKS` (256) x
  `DEFAULT_PAGE_SIZE` (4096), in `inlaysql_core::wal`, with no server flag to
  raise it. A bulk load over the wire has to be split into batches that fit, or
  it is refused. The in-process harness hides this — `crates/inlaysql-bench`'s
  `batched()` catches `Error::Transaction` and commits early — so it only
  becomes visible to a user writing SQL. `module.py` sizes batches for it and
  halves on refusal.

  It used to be visible only as `1030: Got error from storage engine:
  transaction does not fit the write-ahead log` — a generic code naming no
  limit, however the ceiling was hit. A SQL client now gets `1197`
  (`ER_TRANS_CACHE_FULL`) either way, with the byte counts in the message and
  `@@inlaysql_max_transaction_bytes` to size the next batch against — a session
  variable read off the same formula the storage backend measures a commit
  against, so it cannot report a ceiling that is not the one enforced. See
  `docs/server.md`, "The ~1 MiB transaction ceiling".
* **88% of the build is one single-threaded stall on the first read.** Of
  294.9 s for 1.18M vectors, 36.2 s was the load and **258.7 s was the graph**,
  built when the index is first *read* rather than when the rows are written.
  So the query that happens to be first blocks for four and a half minutes,
  and there is no SQL that asks for the build explicitly. Left out of `fit()`
  it would have landed on ann-benchmarks' first timed query as an outlier, so
  the adapter forces it with a warm-up query — which is exactly what an
  application has to do too. `bench/ann/run.py` reports the two halves
  separately for this reason; ann-benchmarks' own `build_time` is the sum and
  cannot show which dominates.
* **928 bytes of RAM per 100-byte vector.** 1,047 MiB of index — measured as
  the server's RSS growth over an empty server, ann-benchmarks' `index_size` —
  for a raw `f32` corpus of 112.9 MiB at dim 25. 9.3x. The in-memory index holds
  the embedding, a normalised copy and the graph, once *per connection*.
  `--paged-vectors` is the lever for that and is off by default; this run did
  not use it.
* **The planner was checked, not trusted.** `fit()` runs `EXPLAIN` and fails if
  the plan is not `SEARCH ... USING VECTOR INDEX`. A row labelled HNSW that was
  really a table scan is the most misleading number a vector benchmark can
  publish — the same check `bench/external/pgvector_driver.py` makes against
  PostgreSQL's planner.

One methodology bug worth recording, because it produced a plausible-looking
wrong answer: `ann-benchmarks` defines *angular* distance as
`scipy.spatial.distance.cosine`, i.e. `1 - cos` — not the Euclidean distance
between normalised vectors. The two rank identically, so the returned
neighbours are the same either way; only the *values* differ, and the recall
test compares values against `true_distances[k-1] + 1e-3`. Written the wrong
way round it scored a perfect answer as recall 0.0000. `run.py` names the check
against the published file that settles it.

### BM25 quality has no equivalent here yet

There is no ann-benchmarks for text ranking. The counterpart is
[BEIR](https://github.com/beir-cellar/beir) — an external corpus, external
relevance judgements and an external metric (nDCG@10), which is the same three
things this section buys for vectors. It is **not** built, and it is a larger
job than this one, because BEIR asks for something the vector side does not:

* a subset with a public `qrels` file and a manageable size — `scifact` (5K
  documents, 300 queries) or `nfcorpus` (3.6K, 323) are the usual starters;
* an `nDCG@10` implementation matching `pytrec_eval`'s, since BEIR's headline
  numbers are graded relevance, not the binary recall this file computes;
* a decision about what is being measured. BM25 is a *ranking function*, and
  BEIR's published BM25 baselines are Anserini/Lucene's — with Lucene's
  analyzer, stemming and tokenisation. InlaySQL's `bm25_score` has its own
  tokeniser, so a gap against that baseline would be a tokenisation difference
  and not a scoring one unless the analysis pipeline is matched or the
  difference is measured separately. That is the whole design question, and it
  has to be answered before a number from it means anything.

## What these numbers are not

They are wall-clock measurements on one machine. What they are for is catching
a regression and knowing where we stand — not for a marketing page.

Two costs still dominate and are both scheduled to change:

- **One durable commit per statement.** There is no batch-insert path, so
  loading N rows costs N `fsync`s — that is what the multi-second "build"
  column in the vector suite is measuring, not index construction.
- **Index commits are deferred to the first read**, so a load-then-query
  sequence pays for the whole batch at once. Before AHL-381 that batch was a
  full graph rebuild; now it is a one-time cold-load build and every later read
  only reconciles the rows that changed. `checkpoint()` writes the indexes
  into the file so the *next* open does not have to rebuild them.

When we publish a number we lose, we publish it. The `points` suite currently
loses on reads and wins on durable writes; both are in the output.
