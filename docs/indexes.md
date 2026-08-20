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

There is no third case, and no attempt to work out *how far* behind an index
is. Incremental catch-up would need a change log the engine does not keep, and
getting it subtly wrong is precisely the failure this design refuses to allow.

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

The key is the *column*, never the index name: there is at most one index per
column (its kind is fixed by the column type), so a rename or a differently
spelled name cannot strand a saved index.

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

Skipping it is always safe. It costs a rebuild on the next open and nothing
else.

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

`PagedHnswIndex` is neither of the above: it keeps its graph *in the database*,
as ordinary rows under a namespace no table can name (`\u{1}ann:table.column`).
It answers `true` to `VectorIndex::is_self_persisting`, and the engine treats it
differently in four places.

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

**A rebuild empties it first.** `VectorIndex::reset` deletes the node records.
Without it, re-indexing every row on top of a graph that just restored itself
would tombstone each old node and roughly double the node count for nothing.

The property this all exists to protect is the one at the top of this document,
unchanged: the rows are the source of truth, and an index that cannot prove it
describes them is rebuilt rather than believed. The difference is only that this
backend can usually prove it — which is why opening a paged index costs nothing,
and why `index_recovery_dst.rs` sweeps it under the same fault schedules as
everything else.
