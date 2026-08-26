# Persisting the retrieval indexes

InlaySQL's full-text and vector indexes live in the same file as the rows.
This is the protocol that keeps them from ever telling a lie about what they
describe. Scalar B-tree indexes (`CREATE INDEX ... USING BTREE`, or the
default for an ordinary column) live in the same file too, but under a
different protocol — see
[Scalar B-tree indexes are a different kind of index](#scalar-b-tree-indexes-are-a-different-kind-of-index)
below for why "The one rule" that governs the rest of this document does not
apply to them.

## Indexes are explicit

A `TEXT` column is full-text indexed only where a `CREATE INDEX` declared it
(a `VECTOR` column likewise for an ANN index), and the index kind is inferred
from the column type. A query that scores a column with no index is an error,
never a silent scan. `Engine::open_implicit` (and `Database::open_implicit`)
restore the pre-`CREATE INDEX` index-everything behaviour as a per-table
default, for the demo and for anyone who wants it.

### Migration

A database written before `CREATE INDEX` existed has a version-1 catalog that
records no index declarations. On open it is **grandfathered**: every indexable
column of every existing table keeps an implicit index, materialised as an
ordinary declaration. New tables created after that opt in like any other.
Nothing is silently dropped.

## A vector index's distance is part of what it is

An ANN index is built and searched under one distance —
`inlaysql_core::hnsw::VectorMetric` — chosen at `CREATE INDEX` with pgvector's
operator-class spelling and fixed from then on:

```sql
CREATE INDEX items_embedding ON items (embedding);                 -- vector_cosine_ops
CREATE INDEX items_embedding ON items (embedding vector_l2_ops);   -- Euclidean
```

**This is not a query-time argument and cannot be.** An HNSW graph's neighbour
lists are the answer to "what is near what" under one particular distance;
searched under another they route the greedy walk by the wrong geometry and
return plausible, wrong rows with no error anywhere. So the metric is written
in three places and checked at each boundary between them:

| Where | What it is | What a mismatch does |
| --- | --- | --- |
| The catalog (version 7) | one tag per index declaration, written only when the metric is not cosine | an older build refuses the catalog outright (`Error::FormatVersion`) rather than rebuilding the graph as cosine |
| `HnswIndex::encode` (version 5) | one tag after the version byte, written only by a non-cosine index | `HnswIndex::load` returns `Error::Corrupt`, and `Engine::load_saved_indexes` falls through to a rebuild from the rows |
| The paged graph header | one trailing byte, appended only when the metric is not cosine | `PagedHnswIndex::restore` purges the namespace and comes back empty, so the caller's usual staleness handling rebuilds it — the same route a foreign *encoding* takes |

Each of the three is written **only when the metric is not the default**, so a
cosine database is byte for byte the database it was before any of this
existed. That is the same "lowest version that can express it" rule the catalog
already followed for constraints, B-tree indexes and collations.

The metric also decides what is *stored*, not only how two stored vectors are
compared: cosine L2-normalises on the way in so the comparison reduces to a
dot product, and L2 does not, because the magnitude that would discard is what
it measures. Both halves live on the one enum
(`VectorMetric::prepare` and the `distance` kernel), which is what makes
"normalised for cosine, compared as L2" unwritable rather than merely
untested.

One column carries one vector index. Unlike a B-tree index, where the planner
picks by the comparison's collation, `vector_score(embedding, ?)` names the
column and not the metric, so a second graph over the same column would be one
nobody could ask for — and the backend map, keyed by `(table, columns)`, could
not hold it beside the first. The catalog refuses it with that reason.

## The candidate list *is* a query-time argument

The distance is fixed for the life of the graph. The **candidate list** — `ef`,
how many candidates the walk may hold at once — is the opposite: it changes
nothing about the graph and everything about how much of it one query is
willing to look at. It is the whole recall/latency trade, and it is per query:

```sql
SET inlaysql_hnsw_ef_search = 400;   -- more recall on the query that matters
SET inlaysql_hnsw_ef_search = 0;     -- back to the index's own tuning
```

Embedded, that is `Database::set_vector_ef_search(Some(400))`; the core seam is
`VectorTuning`, which is a *handle* the host installs rather than a number
copied into the engine, for the same reason `Cancel` is: the value the server
reports as `@@inlaysql_hnsw_ef_search` and the value the walk uses have to be
one load of one field, or a session ends up told it is searching at an `ef` it
is not searching at.

Three properties, and the reasons they are what they are:

* **Unset changes nothing.** The default is the tuning the index was built with
  (`HnswParams::DEFAULT`, chosen by `bench --suite sweep`), and that is not a
  constant: `ef_for(k)` widens the beam with the number of candidates asked for,
  so the same index searches a `LIMIT 10` at `ef = 80` and a `LIMIT 100` at
  `ef = 800`.
* **`EXPLAIN` reports the effective number**, as `(ef=N)` on the vector-search
  node, because an operating point nobody can see is one nobody can choose —
  and because the untuned number is per query, `@@inlaysql_hnsw_ef_search` on
  its own could not tell you what a given query will do.
* **A beam narrower than the answer is refused.** `ef` must be at least the
  query's `LIMIT`, which is pgvector's rule too: a walk holding fewer
  candidates than the answer cannot come back with the answer. The floor is
  the *row budget* and not the candidate count — the engine over-fetches
  candidates fourfold so a fused ranking has more than the bare minimum, and
  an `ef` below that is merely a narrower beam, which is exactly what a caller
  asking for less latency is asking for. Widening a too-narrow value silently
  would search at a number the caller did not choose while reporting the one
  they did; returning a short list would drop rows without saying so. So the
  query fails and names the smallest `ef` that works.

**`m` and `ef_construction` are not settable per index yet.** They shape the
stored graph rather than one query, so unlike `ef_search` they cannot be
changed without a catalog format change to record them and a rebuild to apply
them. Every index is built at the shipped `m = 16`, `ef_construction = 200`.

## Scalar B-tree indexes are a different kind of index

Everything above and below this section is about the full-text and vector
indexes — the ones that are scored (`bm25_score`, `vector_score`) rather than
probed for equality or a range. A scalar index over an ordinary column
(`CREATE INDEX ON t (a)`, `IndexKind::BTree`, AHL-423) exists too, and its
persistence story is the opposite of everything "The one rule" below
describes, for a structural reason worth stating plainly.

**A B-tree index entry is an ordinary row in the same copy-on-write tree as
the table's own rows** — not a serialized blob rebuilt on open. Each entry is
a memcomparable encoding of the indexed column values followed by the row id,
under a reserved key prefix (`\x01idx:<index name>\0`, disjoint from a table
row's key and from the paged ANN index's own `\x01ann:` namespace); see
`crates/inlaysql-core/src/index.rs`'s module docs for the exact byte layout.
Because an entry is a row, it is written in the *same* commit as the table
row that justifies it, inherits the same write-ahead log, the same crash
recovery and the same MVCC rebase, and can never go stale the way a saved
BM25 or HNSW blob can — there is no `write_version` stamp to compare on open
and nothing to rebuild, because the index was never allowed to fall behind
the rows in the first place. `index_persistence.rs` and the staleness table
under [Write versions](#write-versions) below do not apply to it at all.

Two properties this same design gives the planner, both worth naming because
they are easy to get quietly wrong:

- **A probe may only answer a term whose collation the index is keyed
  under, and this is enforced, not assumed** (AHL-469). A `NOCASE` index
  holds folded keys, so probing it for a `BINARY` comparison would return
  rows the filter then has to re-check, and the half that actually matters —
  probing a `BINARY` index for a `NOCASE` comparison — would look up the
  unfolded bytes and silently miss every row that differs only in case: the
  same query answering differently depending on which access path the
  planner happened to pick. `index_probe`/`Term::collation` in `engine.rs`
  refuse that by construction, matching SQLite's own rule, and
  `btree_index.rs` runs every collated shape twice — once over an indexed
  table and once over an unindexed one — to prove the two access paths never
  disagree.
- **A join's inner side is probed through a B-tree index when its `ON`
  justifies one, not materialised** (`join_inner`/`join_probe` in
  `engine.rs`, decision D6, AHL-464). This is the index nested-loop join:
  the outer side still scans, but each outer row seeks the inner index
  instead of the whole inner table being pulled into memory once per join.
  Not every `ON` shape earns this — the planner declines a probe it cannot
  justify and falls back to materialising the inner side, and
  `index_join.test` pins the *answers* both ways rather than the access
  path, so a shape the rule declines still has to agree with one it accepts.
  `differential.rs`'s join generator asks SQLite the same question three
  ways — inner side materialised, probed through a B-tree index, and probed
  by `INTEGER PRIMARY KEY` — for the same reason.

**The walk itself got cheaper without changing what it walks** (AHL-479).
`CowBTree::scan_range_row_ids_from` is a row-id-only sibling of the general
entry walk: because every key this tree stores under the engine's own
encodings ends in its row id as eight big-endian bytes, a probe that only
ever wanted the row id can read those eight bytes straight out of a borrowed
entry instead of cloning the whole key into an owned `Vec<u8>` and resolving
its (always-empty) value first, the way the general-purpose walk has to.
`Storage::scan_index_row_ids` is the one caller; a test
(`an_index_row_id_walk_agrees_with_the_general_entry_walk`) pins it against
the general walk plus the ordinary decode, rather than trusting the two to
stay in step by inspection.

## The build is deferred, and you can ask for it

A write does not build the index it belongs to. It stages the document or the
embedding in the backend and marks the table dirty; the *first read that needs
the index* commits every backend, which for a graph index is where the graph is
actually built. That is the right trade row at a time — writes stay cheap and
the cost is paid once per read-after-write — and the wrong one after a bulk
load, where it hides the whole build inside one innocent `SELECT`. The
ann-benchmarks run measured it: of a 294.9 s glove-25 load, **258.7 s was the
graph build happening inside whichever query arrived first**, with nothing in
the statement to explain the wait.

So the build can be asked for:

| | |
| --- | --- |
| SQL | `REINDEX` — every table; `REINDEX <table>`; `REINDEX <index>` |
| Embedded | `Database::reindex(None)` / `Database::reindex(Some("docs"))` |
| MySQL wire | `OPTIMIZE TABLE docs [, notes]` |

All three run the same code. Three things are worth knowing:

- **The default did not change.** A loader that never queries still never pays
  for a build. This is a request, not a policy.
- **Nothing pending is a no-op**, decided per *table*, so a second `REINDEX
  docs` in a row does nothing and says so — over the wire in MySQL's own words,
  `Table is already up to date`. Safe to put in a cron job.
- **It can be stopped**, by a statement timeout or a `KILL`, between one index
  and the next. A stopped build leaves the work pending exactly as if it had
  never been asked for, so the next read does it and no search ever sees a
  half-built index. It is *not* stoppable inside one index's commit — that call
  is opaque to the engine, and a backend that could be interrupted half-way
  through its own build would have to be able to put its pending set back,
  which none of the graph backends can. On a database with one index, that
  means the check happens before the build starts and not again.

`REINDEX` is SQLite's spelling and is answered in SQLite's terms: it returns no
rows. `OPTIMIZE TABLE` is MySQL's, and is answered with MySQL's four-column
result set, on the MySQL side of the seam ([`docs/server.md`](server.md)) —
the engine's dialect gains nothing MySQL-shaped. What this engine's `OPTIMIZE
TABLE` does *not* do is MySQL's other half, rebuilding the table to reclaim
free space; that is why the `Msg_text` names what happened rather than always
saying `OK`.

## The one rule

**The rows are the source of truth. A saved index is a cache.**

Everything below follows from that. There is no repair path, no partial
rebuild, no "trust it and hope": every way a saved index can be wrong ends in
the same place — throw it away and rebuild from the rows. The worst outcome a
corrupt, torn or stale index can produce is a slower open.

That matters because the alternative is subtle and awful. An index that
silently disagrees with the table does not crash; it returns a plausible
ranking that quietly omits rows. There is no assertion that catches that in
production, so the design has to make it impossible rather than detectable.

## Write versions

The engine keeps a counter, `write_version`, in engine metadata. Every
statement that changes a row increments it **in the same storage commit as the
change**, so the counter and the data are atomic with respect to each other:
there is no crash that leaves one without the other.

A saved index records the `write_version` it was taken at. On open, the engine
compares that stamp to the committed counter:

| Stamp | Meaning | Action |
| --- | --- | --- |
| equal | the index describes exactly these rows | load it |
| different | rows changed after the index was saved | rebuild |
| absent / unparseable | no usable index | rebuild |

There is no third case for a *saved* index, and no attempt to work out how far
behind a blob is: the bytes on disk carry one stamp for the whole index, so
"how far" is not a question they can answer.

### A live handle is a different question

A handle that is already open is not reading a blob — it holds a live index it
built itself, and it knows exactly which version that index describes. When
another handle commits, the per-statement snapshot refresh
(`Engine::adopt_committed_state`) has a source the open path does not: the
change log (`crates/inlaysql-core/src/cdc.rs`), which names every row that
changed and is written in the same commit as the change. So the gap is replayed
rather than rebuilt (`Engine::catch_up_indexes`): each row the log names is
dropped from its table's retrieval indexes and re-derived from the committed
row, and nothing else is touched.

This is not a micro-optimisation. Without it, *every* connection paid a full
re-index of every table on its next statement after *any* other connection
committed a row, because the saved blob's stamp is stale for all but one commit
in `INDEX_PERSIST_INTERVAL` (1024). On a server with `n` connections that is
`n` full rebuilds per write.

It declines, and falls back to the wholesale rebuild, in exactly the cases
where replaying would be a guess rather than a derivation:

| Situation | Why replay is not available |
| --- | --- |
| the catalog also moved | an index this handle has never opened has no incremental form |
| the log no longer reaches back that far | past `CDC_RETENTION` (4096 statements) the record is gone; this also bounds the replay |
| a record in the range is missing or empty | the handle cannot know what changed, which is when guessing must not be an option |
| a vector backend keeps itself in the database | its graph, live set and entry point moved in the file underneath the copy this handle holds in memory; only re-opening re-reads them |

`crates/inlaysql-core/tests/foreign_commit_indexes.rs` counts calls to
`FullTextIndex::insert`/`VectorIndex::insert` to pin the cost, and asserts the
caught-up index answers identically to one built from the rows — the property,
not a timing threshold.

### The commit that rebases

One case is not a refresh at all. If handle B commits while handle A's
transaction is open, A's disjoint transaction is *rebased* onto B's root at
`COMMIT` (see `merge_monotonic_metadata`), so A ends up holding a root
containing B's rows without the committed state ever moving from A's point of
view — `Storage::refresh` has nothing to report. B's rows are then committed
underneath an index that was never told about them.

`Engine::indexed_version` is what catches it: it records the version the live
indexes actually describe, and a rebase leaves it behind the counter, which is
what makes the next statement replay the gap.
`a_rebased_commit_does_not_leave_the_other_handles_row_out_of_the_index` in
`crates/inlaysql/tests/concurrent_writers.rs` asserts it with a search rather
than a `SELECT`, because the row store gets this right either way and only the
index does not.

## On-disk layout

A saved index is too big to be one value: the B-tree holds values inside a
page, which is 4 KiB by default, while a vector index over twenty thousand
embeddings is tens of megabytes. So each index is split.

```
index:<table>:<column>        header: u64 write_version, u64 chunks, u64 length
index:<table>:<column>/0      first 2 KiB of the encoded index
index:<table>:<column>/1      next 2 KiB
...
```

The key is the *column*, never the index name: a rename or a differently
spelled name cannot strand a saved index. This is a single-column index's
on-disk identity, and it is untouched by multi-column `FullText` indexes
(`CREATE INDEX idx ON docs (title, body) USING FULLTEXT` — MySQL's
`FULLTEXT(title, body)`, the one retrieval kind that can name more than one
column; see the README's Next list): a multi-column index's key cannot be
just the column, since more than one is named, so it gets a key of its own
that is built to never collide with a single-column one no matter what the
columns are called — the third segment begins with a `\u{2}` control byte,
which (like the `\u{1}` [`vector_index_namespace`](#backends-that-persist-themselves)
already relies on) a real column's name cannot. Nothing about the
single-column format above moved to make room for it, and it needed no
catalog format change either — `Catalog::required_version`'s multi-column
encoding was never B-tree-specific, so a multi-column `FullText` index forces
the same version bump a multi-column B-tree index already does, for the same
"an older build must refuse this, not misread it" reason.

Reading checks the header's version first, then reassembles the chunks and
checks the total length matches. A chunk that went missing shortens the
payload, the length check notices, and the index is rebuilt.

## Writing it: header last

Saving an index is not one transaction. Under copy-on-write, every entry
written copies its root-to-leaf path into fresh pages, and the write-ahead log
has to hold all of them — a ten-megabyte index in one transaction overflows a
one-megabyte log by two orders of magnitude. So the save is committed in
bounded batches, and the order is what makes that safe:

1. **Clear the header** and commit. From here on there is no saved index.
2. **Write the chunks**, committing every 64 KiB.
3. **Write the header** and commit. This is the moment the chunks become an
   index.

A crash anywhere in step 2 leaves a header that does not parse, so the next
open rebuilds. A crash in step 1 or 3 lands on one side or the other of a
single committed write. There is no window in which a valid-looking header
points at chunks from two different saves.

## When it happens

- Automatically, once `INDEX_PERSIST_INTERVAL` (1024) row mutations have
  accumulated, at the next point where the indexes are made searchable.
  Saving costs time proportional to the index's *size*, not to the change, so
  doing it per statement would make a row-at-a-time load quadratic.
- Explicitly, on `Database::checkpoint()`. Worth calling after a bulk load and
  before closing.

Skipping it is always safe. It costs a rebuild on the next *open* — and
nothing else: a handle that is already open catches its live indexes up from
the change log instead of consulting the stale blob (see
[A live handle is a different question](#a-live-handle-is-a-different-question)),
so the interval governs open time alone rather than what every other
connection pays per commit.

## What the tests assert

`crates/inlaysql-core/tests/index_persistence.rs` counts calls to
`Storage::scan_batch`, because "opening did not re-read every row" is the actual
claim and a call count states it exactly. It covers the restore path, the
stale-stamp path, a corrupt chunk, a missing chunk, a truncated header, and an
index large enough to span many chunks.

`crates/inlaysql/tests/index_recovery_dst.rs` runs the whole engine over the
fault-injecting simulator across thousands of seeds. After each crash schedule
it reopens the surviving image and asserts the property that matters: **every
row the database can scan is a row its indexes can find, and nothing else is.**
A stale index that outlived a rolled-back commit fails that immediately.

## Backends that cannot persist

`FullTextIndex::save` and `VectorIndex::save` return `Option<Vec<u8>>`. A
backend that returns `None` is simply rebuilt every time — no configuration, no
error. That keeps the trait implementable by anything, which is the point of it
being a trait.

## Backends that persist themselves

`PagedHnswIndex` and `PagedBm25Index` are neither of the above: they keep their
structure *in the database*, as ordinary rows under namespaces no table can
name (`\u{1}ann:table.column` and `\u{1}fts:table\u{1}column\u{1}`). They
answer `true` to `VectorIndex::is_self_persisting` /
`FullTextIndex::is_self_persisting`, and the engine treats them differently in
five places.

Both are opt-in and both default to off: `EngineOptions::paged_vector_indexes`
and `EngineOptions::paged_text_indexes`. The in-memory backends are faster, and
what paging buys is a memory bound rather than speed — see
[The paged BM25 index](#the-paged-bm25-index) for what it costs.

**It writes through the engine's transaction.** The engine's storage is a
`SharedStorage` — one `Rc<RefCell<_>>` handle the index holds a clone of — so
the graph's node writes land in whatever transaction the rows did. `Engine`
tells it what is happening with `prepare_commit(write_version, may_commit)`
before each commit. Inside a caller's transaction `may_commit` is false and the
index leaves the durable commit to `Database::commit`, which is what makes the
rows and the index atomic: both, or neither. Outside one, the rows are already
durable and the index may commit — and must be free to, because one build can be
far larger than a single write-ahead-log record. It commits in batches as
`Storage::transaction_is_nearly_full` says so.

**It reads its own writes.** Building the graph means reading back neighbours
that earlier inserts in the same batch just wrote. That works because
`CowBTree::get` and `CowBTree::scan_prefix` resolve pages out of the open
transaction's dirty set — a writer sees its own transaction, as it does in any
SQL database. `CowBTree::get_at` is the read that deliberately does not, and is
what a pinned snapshot uses.

**It stamps, and the stamp is the whole currency check.** The write version goes
into the graph header, and *only* on the commit that completes the graph. A
header written between batches carries no stamp at all, so a crash mid-build
leaves a graph that is structurally sound but visibly not current. On open the
engine compares `VectorIndex::stored_write_version` with the committed write
version, exactly as it compares a saved blob's — same table, same outcomes:

| Stamp | Means | Engine does |
| --- | --- | --- |
| equal to the committed version | the graph describes these rows | use it as it is |
| different | rows changed under it, or another binary wrote them | rebuild |
| absent | a crash caught it mid-build, or it is new | rebuild |

**Another handle's commit is adopted by re-reading it, not by replaying rows
into it.** This is the one that is easy to get backwards. For an in-memory
backend, this handle's copy *is* the index, so `catch_up_indexes` brings it up
to date by reconciling the rows the change log names. For a self-persisting one
the index is in the file and the committing handle already updated it there —
every node record, the entry point, the live set and the stamp. Replaying rows
on top would apply that change a second time (`remove` tombstoning a node the
graph still has live, `insert` adding a duplicate) and would do it as *writes*,
from a handle that only read. `Engine::adopt_self_persisting_vector_indexes`
re-opens the backend instead, and holds it to the same stamp test a saved blob
gets: a graph whose stamp is not the committed write version is not a catch-up,
it is a rebuild.

The cost is honest and worth stating: re-opening walks the graph's node records
to rebuild the row-id map, so a foreign commit is O(nodes) here where an
in-memory index pays O(rows that commit touched). That is why `docs/server.md`
presents `--paged-vectors` as a trade. It replaces something far worse —
declining rebuilt the *whole table*, which re-tokenised every document into the
full-text index as well, measured at 41 re-indexed documents for one foreign
insert into a 40-row table (`tests/foreign_commit_indexes.rs`).

**A rebuild empties it first.** `VectorIndex::reset` deletes the node records.
Without it, re-indexing every row on top of a graph that just restored itself
would tombstone each old node and roughly double the node count for nothing.

The property this all exists to protect is the one at the top of this document,
unchanged: the rows are the source of truth, and an index that cannot prove it
describes them is rebuilt rather than believed. The difference is only that this
backend can usually prove it — which is why opening a paged index costs nothing,
and why `index_recovery_dst.rs` sweeps it under the same fault schedules as
everything else.

## The paged BM25 index

`PagedBm25Index` (`EngineOptions::paged_text_indexes`, default off) is the
full-text half of the same idea, and it exists for a measured reason: the
in-memory `Bm25Index` costs ~1,800 bytes per document once the dictionary
saturates, so ten million documents is ~17 GiB **per connection** with nowhere
to put it (`docs/enterprise-readiness.md` blocker 6,
`crates/inlaysql/tests/index_memory_cost.rs`). It follows every rule above —
self-persisting, stamped, reset before a rebuild, adopted by re-opening — so
what is worth writing down separately is the layout, the one property that is
harder here than for a graph, and what it costs.

### What is in the file

Row keys are `(namespace, u64)`, so each structure is a `u64`-keyed table under
a namespace no SQL identifier can spell. Four of them, from one base
(`\u{1}fts:<table>\u{1}<column>\u{1}…`):

```
<base>          documents,  key = row id
    doc     := u32 length, u32 term_count, u32 * term_count   (term ordinals)
<base>\u{1}d  dictionary, key = FNV-1a 64 of the term
    bucket  := u32 count, (string term, u32 ordinal)*
<base>\u{1}x  term records, key = term ordinal
    term    := string term, u32 document_frequency, u32 max_frequency,
               u32 min_length, u32 next_slot, u32 chunk_count, chunk*
    chunk   := u32 slot, u64 greatest row id in that chunk
<base>\u{1}p  postings chunks, key = (term ordinal << 32) | slot
    chunk   := u32 count, posting*
    posting := u64 row id, u32 frequency, u32 document length
```

Four things in there are decisions rather than details.

- **Documents are row ids, not dense ordinals.** The in-memory index assigns
  ordinals so a length or a row id is an array index. On disk there are no
  arrays, so an ordinal buys nothing and costs a resident `RowId -> ordinal`
  map that grows with the corpus — the very thing being removed. Walk order
  becomes row-id order as a result, which cannot change the answer: the answer
  is the top `k` under a total order on `(score, row id)`, so it is a function
  of the *set* of documents scored and never of the order they were reached in.
- **A posting carries its document's length.** Otherwise scoring costs a second
  keyed read per document reached, which is the dominant cost of a query. Four
  bytes per posting on disk, nothing in memory, and it cannot go stale because
  re-indexing a document rewrites every posting it has.
- **A term's chunks are found through a directory, not by scanning.** The
  directory is a skip list: a MaxScore cursor that has been demoted and is
  thousands of postings behind advances over whole chunks without reading them.
  It is also what makes a mid-list write cheap — a re-indexed document rewrites
  the one chunk holding its row id, not the list.
- **The dictionary is hashed, not sorted.** A sorted dictionary would need a
  resident block index that grows with the vocabulary; a hash bucket is one
  point read and nothing resident at all.

What stays in memory: the header scalars, the documents buffered since the last
commit, and a bounded LRU of decoded entries (`DEFAULT_CACHE_ENTRIES`). Not the
dictionary, not the postings, not the per-document term lists. The one entry
that is not `O(1)` in bytes is a very common term's record, which carries that
term's chunk directory.

### The hard part is not the layout, it is that the scores must be identical

BM25 is corpus-relative in a way an ANN graph is not. `idf` is a function of the
live document count and a term's document frequency; the length normalisation
divides by the mean document length. A backend that computed any of those
slightly differently would not fail — it would return a plausible ranking with
two hits transposed, or the same ranking with scores differing in the last
place. The second is worse, because `fuse()` and a user's
`ORDER BY bm25_score(...)` both consume the number rather than the rank.

So the arithmetic is **not transcribed twice**: `bm25::idf`,
`bm25::average_length`, `bm25::length_normalisation` and `bm25::contribution`
are called by both backends, because floating-point arithmetic is not
associative and a second copy that grouped a multiplication differently would
agree to a printed decimal and disagree as bits. The corpus statistics move on
exactly the events they move on in the in-memory index. A document's
contributions are summed in query order in both.

Skipping is the one place the two are allowed to differ, and it cannot change
the answer: MaxScore only declines to visit a document whose *entire* possible
score is strictly below the `k`-th best already held, so a different-but-valid
bound prunes a different amount of work and the same set of results.

`crates/inlaysql-core/tests/bm25_paged_agreement.rs` asserts this rather than
arguing it — whole result sets, ids and score *bits*, against a freshly built
`Bm25Index`, over six corpus shapes (inside one chunk, across many, sparse row
ids, a wide vocabulary, degenerate documents, and churn), every query shape, six
limits and two filters. `crates/inlaysql/tests/paged_full_text.rs` does the same
through the whole SQL path, comparing the `f64` bits that come back from
`bm25_score`.

### What it costs

**Writes, and the bill is large.** An inverted index update touches one chunk
per *distinct term* of the document — around a hundred for a 120-token chunk of
English — and the first time each term is seen it also costs a dictionary bucket
and a term record. Those land on different leaf pages, because the terms are
scattered across the whole key space, so under copy-on-write one document can
dirty a few hundred pages. A commit record carries every page it copied and must
fit one write-ahead-log region, which is 1 MiB (blocker 5). Three consequences,
and only the first is comfortable:

- **The ordinary path is fine.** Index commits are deferred to the first read
  that needs them (`refresh_indexes`), and that read is normally outside any
  explicit transaction, so the batch is applied with `may_commit` true and the
  index commits itself as `Storage::transaction_is_nearly_full` says so. That
  check runs after **every row write** rather than per document: a hundred new
  terms is already past the ceiling, so checking per document is checking after
  the damage.
- **A read *inside* an open transaction, after many documents, is not.** There
  `may_commit` is false — committing would make the caller's buffered rows
  durable at a moment it did not choose — so the whole batch has to fit one
  commit record, and with a wide vocabulary it may not. The statement is
  refused rather than half-applied, which is the same answer blocker 5 gives
  everywhere else, but it is a shape the in-memory backend would have taken.
- **The file grows fast**, because each of those superseded pages is abandoned
  unless `page_reuse` is on: measured at tens of kilobytes *per document* on a
  bulk load with reuse off (`index_memory_cost.rs` prints the file size beside
  the memory figure for exactly this reason). Blocker 4's flag is not optional
  company for this one.

A batch is applied **term-major** — all pending edits grouped by term, so a term
fifty of the documents mention is rewritten once rather than fifty times — which
blunts the bulk-load case without removing the per-transaction ceiling. The real
fix is the segment-and-merge design every production full-text engine uses, and
it is a project of its own.

**Reads** cost a tree descent per postings chunk instead of a pointer chase. The
directory keeps a demoted MaxScore cursor from paying for the postings it skips.

One thing is *cheaper* than the vector half: re-opening a paged BM25 index reads
its header and nothing else, because there is no resident row-id map to rebuild.
So adopting another handle's commit is `O(1)` here where
`adopt_self_persisting_vector_indexes` is `O(nodes)`.

### Two handles, one structure — the hazard this design introduces

Sharing through the file is what makes a paged index cheap and it is also the
one thing about it that is genuinely harder than an in-memory backend. Two
`Database` handles on one database hold two `PagedBm25Index` objects over the
*same* namespaces. When one of them rebuilds — which is what any handle does on
opening to a stamp that is not current — it rewrites the document records and
reassigns every term ordinal underneath the other, **without changing a row**,
so nothing moves the `write_version` the engine watches on the other handle's
behalf and `adopt_committed_state` returns early.

What the second handle then holds is wrong in two ways, and the quiet one is
worse:

- `live` and `total_length` are what `idf` and the length normalisation are
  computed from, so a stale pair rescores the whole corpus. Its only visible
  symptom is that the retire step tries to subtract a document that handle
  never counted — which is how this was found.
- **Term ordinals come from a counter in the header.** Two handles that both
  believe the next ordinal is 5 give it to two different terms, and each then
  reads the other's postings under it. A wrong answer, with no error anywhere.

`PagedBm25Index::adopt_stored_statistics` closes it: the statistics and the
ordinal counter are re-read from the header on every commit and every search
rather than remembered, and the decoded cache is dropped whenever they moved.
One metadata read per commit and per search, and always consistent with the
document records that handle can see, because both are written into the same
transaction and the header is written last.
`a_rebuild_by_another_handle_is_adopted_rather_than_overwritten` pins it and
fails against the code without it.

**`PagedHnswIndex` had the same exposure and now closes it the same way.** A
rebuild reassigns node indices exactly the way a BM25 rebuild reassigns term
ordinals, and it was reachable: a handle left behind by another handle's
rebuild answered a query with an entirely different set of rows.
`PagedHnswIndex::adopt_stored_graph` re-reads the header on every commit and
every search, drops the node cache when it moved, and marks the resident
`RowId -> node` map for rebuilding. That last part is deferred to the next
`&mut self` call rather than done on the spot, because it is the one `O(nodes)`
step and no *read* needs it — a search answers out of the records it walks, so
a `SELECT` that happens to be the first thing to notice a foreign rebuild pays
one metadata read, not a scan of the graph.

The symptom is worth naming, because it is not BM25's. There is no count to
underflow and nothing loud: `entry` and `entry_level` are where a walk
*starts*, so a stale pair starts it at whatever row now holds that index and
returns the wrong neighbours; a stale `node_count` either refuses a good record
as corrupt (too low) or lets an insert overwrite a node the live graph still
uses (too high). `hnsw_paged.rs`'s
`a_rebuild_by_another_handle_is_adopted_rather_than_overwritten` asserts on
returned ids against an oracle that ran the identical insert sequence on
storage nobody else touched, and fails against the code without the fix.

### The trait change

Making this work needed four methods on `FullTextIndex` —
`is_self_persisting`, `reset`, `prepare_commit`, `stored_write_version` — the
same four `VectorIndex` already had. **All four are defaulted**, so a full-text
backend outside this repository implements `insert`, `remove`, `commit` and
`search` exactly as before and gets the pre-existing behaviour, because each
default spells out what the engine assumed before the method existed. That is
the smallest form the change could take; nothing else about the trait moved.
