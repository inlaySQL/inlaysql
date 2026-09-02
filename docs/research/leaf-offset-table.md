# AHL-541 — B4: does the leaf need a cell offset table, or does it already have one?

**Status: measurement first; format change rejected by reading and then by
number.** The layout the brief proposed — fixed-width cell offsets at the head
of the page, cells packed from the tail — is the layout `btree/page.rs` has
had since the tree was written. What is left in "cell iteration" is the
decoder, not the format, and the prototype below puts a number on how much of
it a tighter decoder on the *same* bytes collects. That is the slice this
brief recommends, and it needs no `FORMAT_VERSION` bump, no second decoder,
and no migration.

## Question

After AHL-538 (`PERF.md`, 2026-09-03) the `bin/profile --suite aggregate
--rows 100000` profile has cell iteration at 11.5% of self time
(`scan_leaf_cells` 3.3%, `decode_leaf_cell_ref` 3.0%, `get_u16` 1.9%,
`resolve_scanned_at` 2.2%), with `admits_whole_leaf` at 4.7% inclusive in
the pre-AHL-538 profile and `trailing_row_id` 1.3%. The hypothesis to test:
cells are found by walking variable-length headers sequentially, and a
SQLite-style offset table would make cell *i* addressable by arithmetic,
make the last key O(1), and make in-leaf binary search cheaper.

## The layout today (`crates/inlaysql-core/src/btree/page.rs`)

Every page is one `page_size` block (4096 by default). Byte offsets:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 1 | `kind` — `KIND_LEAF` 0, `KIND_INTERNAL` 1, `KIND_OVERFLOW` 2 |
| 1 | 1 | unused |
| 2 | 2 | `cell_count` u16 LE |
| 4 | 2 | `free_start` u16 LE — first byte of the cell area |
| 6 | 2 | unused |
| 8 | 8 | `leftmost` child u64 LE (internal pages; 0 on a leaf) |
| 16 | 2 × `cell_count` | **the slot directory**: `slot[i]` u16 LE = byte offset of cell *i* |
| `free_start` .. | | cells, packed from `page_size` backwards in encode order |

A leaf cell at `slot[i]` is `key_len u16 | key | tag u8 | body`, where `tag 0`
is inline (`value_len u32 | value`) and `tag 1` is an overflow pointer
(`first u64 | len u64`). Cells are written by `encode_page` from the tail
(`cell_cursor -= len`), the directory from the head, and the free space sits
between them — exactly SQLite's page shape.

So cell *i* is already `get_u16(bytes, 16 + 2*i)` — one load, no walk. The
last key is already `slot[count-1]` — `leaf_edge_keys` reads the first and
last slots directly and decodes two cells, not `count`. `child_index` is
already a `partition_point` over the decoded separator vector. There is no
sequential header walk anywhere in the leaf reader. The hypothesis is
refuted by reading: **the format already has the offset table.**

## What each reader does per cell today

`scan_leaf_cells(bytes, page_size, f)` (page.rs:690):

1. `check_leaf_header` once per leaf: page length, `free_start <= page_size`,
   `HEADER + 2*count <= free_start`.
2. Per cell: `get_u16(HEADER + 2*i)` → `Result<u16>` (a bounds-checked
   `bytes.get(..)` plus an `Err` arm that formats a `String` — the helper is
   not `#[inline]` and shows up as its own 1.9% frame).
3. `decode_leaf_cell_ref(bytes, page_size, slot)` → `Result<LeafCellRef>`:
   `slot + 3 > page_size` check; `get_u16(slot)` for `key_len`;
   `key_end + 1 > page_size` check; slice the key (a second bounds check on
   the same range); read the tag byte; `key_end + 5 > page_size` check;
   `get_u32(key_end + 1)`; `value_end > page_size` check; build a
   `ValueRef::Inline(Range)` and return the 40-byte struct by value through
   `Result`.
4. `f(key, value)` — in `scan_leaf_into`: `out.len() >= limit`, `!whole &&
   admits(key)`, `resolve_scanned_at` (a `match` on the `ValueRef` and an
   `Option` on the buffer, a `Range` clone), `trailing_row_id(key)` (a
   `get`, a `try_from`, a `Result`), `out.push`.

That is roughly eight bounds checks and four `Result` returns per cell
before the row is handed on. Every one of them is on the *decoder*, none on
the layout.

`leaf_edge_keys` (page.rs:647): the header checks, two slot reads, and two
full `decode_leaf_cell_ref`s — each of which decodes the *value* (tag, u32
length, range check) to hand back a key that does not need it. `admits_whole_leaf`
then does four key comparisons on the result.

`trailing_row_id` (tree.rs:4004): the last eight bytes of the key, big-endian.
Already O(1) and layout-independent; its 1.3% is the `Result` plumbing and
`try_from`, not a search.

`leaf_split_point` (tree.rs:4482): `leaf_size(entries[..split])` for `split =
1..n` — quadratic in the leaf's cell count on the *write* path. Layout-
independent (it sizes decoded `Entry`s), and not on any read profile. Noted
and left alone; it belongs to the insert-path work running concurrently.

The encoders (`encode_leaf_cell`, `encode_page`) build one `Vec<u8>` per
cell then copy each into the page from the tail. Also write-path, also not
in this profile.

## The proposed layout, and why there is nothing to propose

Fixed-width offsets at the head, cells from the tail: that is the table
above. The only layout tweak that changes the per-cell arithmetic at all
would be storing cell *ends* as well as starts (so a cell's length is known
without reading `key_len` and `value_len`), which saves one u16 and one u32
load per cell and costs two bytes per slot — and both lengths are needed
anyway to split the cell into key and value. Not worth a format version.

## Cost model

Per cell, today (E0) versus the same layout read by a decoder that checks
the header once and then does one `get` per field with no helper calls (E1):

| | E0 today | E1 tight | Removed |
| --- | --- | --- | --- |
| Slot load | `get_u16` call → `Result` | `chunks_exact(2)` over the directory slice | 1 call, 1 bounds check, 1 `Result` |
| `slot + 3` check | explicit | folded into the `key_len` `get` | 1 compare |
| `key_len` | `get_u16` call → `Result` | `get(slot..slot+2)` | 1 call, 1 `Result` |
| `key_end + 1` check + key slice | 2 bounds checks | one `get(slot+2..key_end)` | 1 compare |
| Tag byte | index (panics on OOB — cannot, but checked) | `get(key_end)` | — |
| `key_end + 5` check + `value_len` | compare + `get_u32` call → `Result` | one `get(key_end+1..key_end+5)` | 1 call, 1 compare, 1 `Result` |
| `value_end` check | compare | compare | — |
| Return | `Result<LeafCellRef>` by value (40 bytes) | closure call with `&[u8]` and `Range` | 1 move |

Every corruption check today's decoder makes is still made in E1; a slot,
key or value that runs past the page is refused with the same error. The
difference is purely in how many function calls and `Result` constructions
it takes to make them.

## The gate: measured upper bound before building

`crates/inlaysql-bench/src/bin/batch_proto.rs --cells` (this brief's
addition to AHL-537's binary): the 3,730 leaves of the 100k-row `users`
table are collected once into a `Vec<Arc<[u8]>>`, then E0 and E1 walk them
40 times each, order alternated per repetition, with an identical callback
(sum of the trailing row id, sum of the inline value length, a count) whose
answer is asserted equal between the two on every repetition. F0/F1 do the
same for the per-leaf edge-key read. Medians, three process runs, on a
machine at load 42–43 (two other agents benchmarking, so the absolute
numbers are inflated and the *ratio* is the evidence):

| Run | E0 `scan_leaf_cells` | E1 tight, same layout | ratio | F0 `leaf_edge_keys` / leaf | F1 keys only / leaf | ratio |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 10.7 ns/cell | 5.9 | 0.55 | 93.4 ns | 34.7 | 0.37 |
| 2 | 4.7 | 2.4 | 0.51 | 23.4 | 8.1 | 0.35 |
| 3 | 3.8 | 2.1 | 0.55 | 15.7 | 6.7 | 0.43 |

Two things this says:

1. **The whole cell walk is ~4 ns/cell** at its quietest — about 8% of the
   query's 50 ns/row (`PERF.md`, AHL-538), which agrees with the profile's
   11.5% for cell iteration including `resolve_scanned_at`. That is the
   *ceiling* for anything a layout change could remove, and the layout is
   already the proposed one. A format change is rejected: it would cost a
   version bump, a second decoder kept alive for v3..=5 pages, a migration
   path, a recovery-path audit and both DST sweeps, to move at most a few
   percent — and the prototype says the same bytes give up half of that
   walk without any of it.
2. **The decoder is half the walk.** E1 is 0.51–0.55x E0 per cell, and the
   key-only edge read is 0.35–0.43x `leaf_edge_keys`. At ~2 ns/cell saved
   over 100k rows that is ~0.2 ms of a ~5 ms query, i.e. ~4% — the same
   order as AHL-538's "change one" (+3%), which was worth landing.

## Verdict

- **Do not change the format.** `FORMAT_VERSION` stays 5,
  `MIN_READABLE_FORMAT_VERSION` stays 3, `docs/recovery.md` is unchanged,
  no page a v5 writer emits is different from before. Nothing in
  `hnsw_paged`, `bm25_paged`, `wal` or `backup` parses leaf cells (checked
  by grep for `scan_leaf_cells`/`decode_leaf`/`KIND_LEAF`), so there is no
  second reader to keep in step either way.
- **Land the tight decoder** in `page.rs` behind the same three public
  functions (`scan_leaf_cells`, `decode_leaf_cell_ref`, `leaf_edge_keys`),
  keeping every corruption error and the tests that pin them, and giving
  `leaf_edge_keys` a key-only path that does not decode the values. The
  little-endian helpers are marked `#[inline]` so a cross-codegen-unit call
  does not stand between the scan and a two-byte load (the release profile
  has no LTO; `get_u16` is a frame in the profile because of that).
- **Not pursued:** the write-side items this reading noticed
  (`leaf_split_point`'s quadratic sizing, the per-cell `Vec` in
  `encode_leaf`) are on the insert path another agent is on.

## Test plan for the decoder change

No format change means no version-dispatch tests. What has to hold:

- `both_leaf_parsers_agree_on_corrupt_pages` (page.rs) is the pin: it
  flips every header and slot-directory byte and 192 sampled cell bytes of
  a leaf three ways each and requires `decode` and `scan_leaf_cells` to
  accept or reject together and read the same cells when both accept. A
  tightened scan that drops a check the decoder keeps fails it, and that is
  the mutation to run. `both_leaf_parsers_agree_on_well_formed_pages` ties
  the two on every cell shape including overflow pointers.
- `leaf_edge_keys_are_the_first_and_last_cells` (page.rs), including its
  short-page refusal, pins the key-only edge path to the same header checks;
  it gains a corrupt-cell case so the key-only path is held to the cell
  checks too, not just the header's.
- `crates/inlaysql/tests/raw_scan_reuse.rs`-style parity — the raw scan
  agrees with the decoded walk — and `a_callback_scan_hands_out_the_rows_a_batch_returns`
  (tree.rs) cover the scan through `scan_leaf_into`.
- Both DST sweeps, because `btree/page.rs` is touched at all.
- `bin/profile`, base vs branch, interleaved, three repetitions, control
  re-run each repetition: `aggregate` 100k and 20k, `joins`, `points`,
  `indexed`, `indexed-range`, `joins-limit`, `writes`. `points` must be flat
  or better; it decodes one leaf through `decode`, not this path, so it is
  the control.

## Outcome

Landed as the decoder change above, no format change (`PERF.md`,
2026-09-03, AHL-541). Interleaved against `48b4ef5`, 3/3 non-overlapping:
`aggregate` 100k 1.05x, 20k 1.06x; `indexed` 1.09x; `indexed-range` 1.04x;
`joins-limit` 1.04x; `points`, `joins`, `writes` flat. Both DST sweeps pass.
