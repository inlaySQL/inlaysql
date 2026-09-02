# R3 — B4: does `GROUP BY` need a batch executor, or does it need a smaller row?

**Status: measurement only.** This is the standalone prototype the R3 brief
asked for before anyone builds a batch executor — nothing in
`inlaysql-core`'s execution path changed. The one new piece of production
surface is a single read-only shim
(`CowBTree::scan_leaves_raw`, `crates/inlaysql-core/src/btree/tree.rs`) that
hands a leaf's raw page bytes to a caller instead of resolving each row into a
`RowBuf` first — nothing upstream of it changed either.

## Question

`SELECT n, COUNT(*) FROM users GROUP BY n` (100k rows, 100 groups) runs
tuple-at-a-time today: leaf → cell → `decode_row` → evaluate → fold. `PERF.md`
(2026-09-02, end of AHL-528a/b/c) attributes the query's ~6 ms to
`stream_aggregate`'s loop (14%), `scan_leaf_cells` (~12%), `decode_row_masked`
(~10%), `GroupTable::find` (6.5%), and `pread`/`memmove` (19.5%). B4 is a
column-batch executor for this shape — decode a whole leaf into a
`Vec<i64>` and run the filter/fold over the vector instead of one row at a
time. Before building it: **how much of that 6 ms is actually payable back by
batching, on today's page format, with no format change?**

## Method

`crates/inlaysql-bench/src/bin/batch_proto.rs`. Same schema and row generator
as `bin/profile --suite aggregate` (`users(id, email, body, n)`, `n = id %
100`, `body` the payload column). Four shapes, `--reps` repetitions each
(20 below), medians reported as ns/row over the whole 100k-row table:

- **A** — `db.query_prepared(GROUP BY)`, today's engine path, for reference.
- **B** — hand-written row loop: `scan_leaf_cells` per leaf,
  `row::decode_value_at(bytes, 3)` per row (the same column-skipping decoder
  the engine's projection pushdown already uses — it walks past `body`
  without allocating, decodes only `n`), fold into a `HashMap<i64, u64>` as
  each row arrives.
- **C** — batch: per leaf, decode every `n` into one flat `Vec<i64>` (+ a
  validity bitmap) first, *then* group-count the vector, so the decode cost
  and the group cost are two separately timed phases:
  - `C: batch decode` — bytes → `Vec<i64>`.
  - `C: batch group (HashMap)` — the general-case grouping.
  - `C: batch group (array[100])` — sound only because `n`'s domain is known
    here; a real batch executor would only take this path with a
    planner-proven bound (a `CHECK` range, a dictionary column), never as the
    default.
- **D** — C's array grouping extended to fold `COUNT(*)`/`SUM(n)`/`MIN(n)`/
  `MAX(n)` per group in one pass, to price the extra fold work separately from
  the decode C already isolated.

Every shape's group counts are asserted against A's own answer before its
timing is trusted (`assert_groups_match`/`assert_array_groups_match` in the
binary) — same rows, same answers, every rep.

`--payload` (16 / 64 / 256 bytes, the `body` column's width) measures how the
decode phase scales with row width, since `body` is always present and always
skipped to reach `n`.

**Reaching the raw page bytes.** Every existing `CowBTree` walk resolves each
row into a `RowBuf` one at a time; nothing hands out a whole leaf's bytes,
which a batch decode needs to operate over. `CowBTree::scan_leaves_raw` is the
one shim this needed — it decodes and navigates internal nodes exactly as the
production raw-scan path (`walk_raw_row_values`) does, and hands each leaf's
`Rc<[u8]>` to the caller undecoded. It is documented as a research seam, not a
second production read path.

**Setup vs. measurement.** `Database`'s storage is `Box<dyn Storage>` — it
does not expose a `CowBTree` handle — so shape A runs against a live
`Database` (identical to `bin/profile`'s own number), the writer handle is
then dropped, and the same file is reopened directly through
`inlaysql::TreeStorage` for B/C/D. That means A and B/C/D are not literally
interleaved instruction-by-instruction on one handle; each shape runs its own
20 repetitions back to back, in one process, on the same file, right after
the others. `uptime` is printed before and after each run.

```sh
cargo build --release -p inlaysql-bench --bin batch_proto
target/release/batch_proto --rows 100000 --payload 64 --reps 20
```

## Results

Load throughout (`uptime`, 4-core dev box, other agents running concurrently):
`load averages: ~2.9–3.4` before and after every run below — steady, not a
quiet machine, noted per the house rule that this box's numbers are directional
and CI is gating, not this.

| shape | payload=16 | payload=64 | payload=256 |
|---|---:|---:|---:|
| A: engine (`query_prepared`) | 52.3 ns/row | 56.5 ns/row | 76.5 ns/row |
| B: row loop (`HashMap`) | 54.6 ns/row | 73.3 ns/row | 123.8 ns/row |
| C: batch decode (bytes→`Vec<i64>`) | 46.5 ns/row | 63.9 ns/row | 124.2 ns/row |
| C: batch group (`HashMap`) | 5.1 ns/row | 5.1 ns/row | 5.0 ns/row |
| C: batch group (`array[100]`) | 0.3 ns/row | 0.3 ns/row | 0.3 ns/row |
| D: batch group+fold (`array`, count/sum/min/max) | 0.5 ns/row | 0.5 ns/row | 0.5 ns/row |

(20 reps each, medians; `payload` is the `body` column's width in bytes.)

## Reading it

**The decode phase is the floor, not the fold.** At every payload width, `C:
batch decode` alone (bytes → `Vec<i64>`) costs *more* than the group step
costs by two to three orders of magnitude — 46–124 ns/row to decode, 0.3–5.1
ns/row to group however it's grouped. Swapping `HashMap` for a direct array
saves real, measurable time in the group step (5.1 ns/row → 0.3 ns/row, a
17x cut) — but the group step is such a small share of the total that the
saving barely moves the total: at payload 64, `C: decode` (63.9) plus
`C: group (array)` (0.3) is 64.2 ns/row, against A's 56.5 ns/row engine number
— batching the *fold* step alone does not beat the row-at-a-time engine, and
D confirms it: extending the array group to `COUNT`/`SUM`/`MIN`/`MAX` costs
essentially nothing more (0.5 ns/row) than plain counting, because it too is
buried under decode.

**This directly answers PERF.md's own attribution.** `GroupTable::find` was
measured there at 6.5% of the query; `scan_leaf_cells` + `decode_row_masked` +
`pread`/`memmove` together were ~41.5%. This prototype's split — group cost
under 10% of decode cost, at every width tried — says the same thing a
different way: the fold was never where the six milliseconds went, and a
batch executor whose only change is "fold over a vector instead of a scalar"
would be optimizing the smaller of the two numbers.

**Decode cost scales with row width, not with what's actually decoded.**
`n` is one `i64` at a fixed ordinal; nothing about decoding it should scale
with `body`'s length, and `row::decode_value_at`'s `skip_value` never copies
a skipped `TEXT`'s bytes — it reads a length and bumps a cursor, O(1) work
regardless of how long the string is. But `C: batch decode` scales close to
linearly with payload (46.5 → 63.9 → 124.2 ns/row across 16/64/256 bytes),
and B tracks it too. The likely explanation, not yet isolated by this
prototype: `TreeStorage::open_on`'s default page cache is 8 MiB
(`DEFAULT_PAGE_CACHE_BYTES`), and a wider `body` shrinks rows-per-leaf, which
grows the table's total leaf-page footprint — at payload 256 the table's
raw bytes alone (100k × ~312-byte rows) are already ~31 MB, several times the
cache, so most of every one of the 20 timed repetitions is a genuine cache
miss and a `pread`, not `skip_value` CPU work. That lines up with `PERF.md`'s
own ~19.5% attribution to `pread`+`memmove` and is consistent with a batch
decode's cost being **I/O and copy bound, not parse bound** — but this
prototype did not separate the two (a `--page-cache-bytes` run sized to hold
the whole table, mirroring AHL-488's technique, is the natural next
experiment and was out of scope for this brief).

**B beat expectations poorly; A is not a low bar.** The hand-written row loop
(B) is *slower* than the real engine (A) at every width tested — 54.6 vs 52.3
at payload 16, 73.3 vs 56.5 at payload 64, 123.8 vs 76.5 at payload 256. This
prototype's B does a naive per-cell bound check
(`key < start || key >= end`) on every row; the production raw-scan path
(`WalkBounds::admits_whole_leaf`, AHL-528) decides admission for a whole leaf
from its two edge keys and skips the per-cell check entirely for every leaf
but the two at a scan's ends — exactly the win AHL-528 already landed and this
prototype's B does not have. That gap is itself informative: it means A —
the number this brief is trying to beat — already carries several
already-landed micro-optimizations (AHL-519's streaming fold, AHL-520's
allocation-free grouped-row path, AHL-528's whole-leaf admission) that a
from-scratch batch executor would have to either inherit or re-earn.

## Does today's page format suffice?

**Yes for this question.** The floor here is not "how many bytes wide is a
cell slot" or "can two adjacent `n` values be read together" — nothing about
the row-oriented, tag-per-value format stops a leaf's bytes from being decoded
into a column vector; this prototype does exactly that with zero format
changes and no core executor change. The floor is **decode-and-fetch cost per
row**, which the payload-width results suggest is mostly page I/O and
`memmove`, not the tag-walk itself. A columnar *page format* (values of one
column stored contiguously, decodable with a `memcpy` instead of a tag walk)
would help the *decode* phase specifically, but that is a strictly bigger
change than B4 asked to scope — a WAL/recovery format change per
`AGENTS.md`'s DST-sweep rule — and this measurement does not show the fold
step is the bottleneck a format change would need to justify.

## Recommendation for B4's first slice

**Do not build a general column-batch fold.** The group step is already 10-300x
cheaper than decode at 100k rows / 100 groups; a batch executor that only
changes "fold over `Vec<i64>` instead of a scalar accumulator" would spend
engineering effort optimizing 5-10% of the query's cost, mirroring what
AHL-519/520 already did for the scalar path.

**If B4 proceeds, the first slice should be the decode-and-fetch path, not the
fold path**: batch the *leaf → column* step itself (this prototype's `C:
decode`) so that a wider payload's cost is paid once per leaf's worth of rows
touched rather than once per row's worth of cursor/branch overhead, and pair
it with a cache-budget experiment (`--page-cache-bytes` sized to the working
set) to separate genuine parse cost from page-cache-miss cost before claiming
a multiple. Until that separation exists, the honest expected multiple for a
batch *fold* alone, on this shape, at this row count, is **~1.0x** — not a
loss, but not the win B4 was scoped to find. The real opportunity this
prototype surfaces is elsewhere: closing B's gap to A (whole-leaf admission)
and the pread/memmove share PERF.md already named, both of which are decode
and fetch, not fold.

## Reproduce

```sh
export SDKROOT=$(xcrun --show-sdk-path)   # macOS only
cargo build --release -p inlaysql-bench --bin batch_proto
target/release/batch_proto --rows 100000 --payload 16  --reps 20
target/release/batch_proto --rows 100000 --payload 64  --reps 20
target/release/batch_proto --rows 100000 --payload 256 --reps 20
```
