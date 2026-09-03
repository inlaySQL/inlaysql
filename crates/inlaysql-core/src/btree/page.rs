//! On-page layout for the copy-on-write B-tree.
//!
//! A B-tree page is one fixed-size block. Every page begins with a small
//! header, then a slot directory that grows forward, while the cells
//! themselves are packed from the end of the page backwards. The free space
//! sits in the middle.
//!
//! ```text
//! +--------+----------------+            free space            +---------+
//! | header | slot directory | <------------------------------ |  cells  |
//! +--------+----------------+                                   +---------+
//! 0        HEADER_SIZE      free_start                          page_size
//! ```
//!
//! * **Leaf** cells hold a key/value pair. The value is either the bytes
//!   inline, or — when the pair cannot fit a page — a pointer to a chain of
//!   **overflow** pages that hold the bytes.
//! * **Internal** cells hold a separator key and the page id of the child to
//!   its right; the child to the left of the first separator is stored in the
//!   header as `leftmost`.
//! * **Overflow** pages hold a slice of one value plus a pointer to the next
//!   page of the same value.
//!
//! The separator in an internal cell is the smallest key in that child's
//! subtree, chosen at split time and never rewritten afterwards. Copy-on-write
//! means a split copies the node instead of mutating it, so this "fencepost"
//! separator stays valid even as keys are inserted and deleted beneath it.

use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;

use crate::error::{Error, Result};

/// A page number. Page 0 is the superblock; B-tree pages start at 1.
pub type PageId = u64;

/// Default page size in bytes.
pub const DEFAULT_PAGE_SIZE: usize = 4096;

/// Smallest page size the layout can represent. A node must fit a header, two
/// slots and at least one small cell.
pub const MIN_PAGE_SIZE: usize = 64;

/// Byte offset and width of each header field.
pub const OFF_KIND: usize = 0;
const OFF_CELL_COUNT: usize = 2;
const OFF_FREE_START: usize = 4;
const OFF_LEFTMOST: usize = 8;
const HEADER_SIZE: usize = 16;

/// Width of one slot-directory entry.
const SLOT_SIZE: usize = 2;

/// Leaf value tag: the value bytes follow inline.
const VALUE_INLINE: u8 = 0;
/// Leaf value tag: an overflow pointer (`first page u64 | length u64`) follows.
const VALUE_OVERFLOW: u8 = 1;

/// Overflow page layout: `kind u8 | next page u64 | payload`.
const OFF_OVERFLOW_NEXT: usize = 1;
/// Bytes of overhead an overflow page spends on its header.
const OVERFLOW_HEADER_SIZE: usize = 9;

/// A leaf page holds `(key, value)` pairs.
pub const KIND_LEAF: u8 = 0;
/// An internal page holds separator/child pairs.
pub const KIND_INTERNAL: u8 = 1;
/// An overflow page holds a slice of a value too large for a leaf, plus a
/// pointer to the next page of the same value.
pub const KIND_OVERFLOW: u8 = 2;

/// The value half of a leaf entry: either the bytes inline, or a pointer to a
/// chain of overflow pages that holds the bytes.
///
/// A leaf cell is `key_len u16 | key | tag u8 | body`. `tag 0` is inline
/// (`value_len u32 | value`); `tag 1` is overflow (`first page u64 | total
/// length u64`).
///
/// The inline case is a *borrowed byte range* into the page's shared buffer
/// when it came from a [`decode`] or the raw-leaf scan, and an owned `Arc<[u8]>`
/// when the write path materialised it. A decoded `Node` is cached behind its
/// own `Rc` (`btree::cache::PageCache`), and every read that hits the cache
/// used to clone these bytes byte-for-byte to hand a caller an owned `Vec<u8>`
/// — `CowBTree::resolve_value_at`, the specific site `PERF.md` names as
/// untouched and largest. Borrowing the range turns that into a refcount bump
/// of the one shared page buffer: one allocation per page decode, zero per
/// cell and zero on every cache hit after that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueRef {
    /// The value bytes, stored in the leaf cell itself, as a byte range into
    /// the page's shared buffer.
    Inline(Range<usize>),
    /// The value bytes, owned because the write path had to copy them out of a
    /// transient buffer (see [`ValueRef::Inline`]).
    Owned(Arc<[u8]>),
    /// The value lives in a chain of overflow pages starting at `first`, and is
    /// `len` bytes long in total.
    Overflow {
        /// The first overflow page of the chain.
        first: PageId,
        /// Total length of the value across the whole chain, in bytes.
        len: usize,
    },
}

impl ValueRef {
    /// The inline value bytes, borrowed from `bytes` or the owned value; `None`
    /// for an overflow pointer.
    pub fn inline_bytes<'a>(&'a self, bytes: &'a [u8]) -> Option<&'a [u8]> {
        match self {
            ValueRef::Inline(range) => Some(&bytes[range.clone()]),
            ValueRef::Owned(value) => Some(value),
            ValueRef::Overflow { .. } => None,
        }
    }

    /// The byte length of the inline value, or `0` for an overflow pointer.
    pub fn inline_len(&self) -> usize {
        match self {
            ValueRef::Inline(range) => range.len(),
            ValueRef::Owned(value) => value.len(),
            ValueRef::Overflow { .. } => 0,
        }
    }
}

/// A decoded key/value pair from a leaf page.
///
/// The key is either a byte range into the page's shared buffer or an owned
/// `Vec<u8>` for a key the write path has just produced. A page *decode*
/// always produces the borrowed form, so a cache miss allocates one shared
/// buffer plus a view per cell where it used to allocate one owned `Vec` per
/// key; the owned form exists only for keys a split/insert is about to encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The key.
    pub key: Key,
    /// The value — inline bytes, or a pointer to an overflow chain.
    pub value: ValueRef,
}

/// A decoded separator from an internal page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Separator {
    /// The smallest key in `child`'s subtree at split time.
    pub key: Key,
    /// The child page to the right of this separator.
    pub child: PageId,
}

/// A key, borrowed from a node's shared page buffer or owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// A byte range into the node's shared [`Node::bytes`].
    Borrowed(Range<usize>),
    /// A key the write path produced and has not yet encoded into a page.
    Owned(Vec<u8>),
}

impl Key {
    /// The key bytes, borrowed from `bytes` or the owned key.
    pub fn resolve<'a>(&'a self, bytes: &'a [u8]) -> &'a [u8] {
        match self {
            Key::Borrowed(range) => &bytes[range.clone()],
            Key::Owned(key) => key,
        }
    }
}

/// A decoded B-tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A leaf holding its entries in key order.
    Leaf {
        /// The raw page bytes every borrowed key range indexes into, shared by
        /// the page cache so a cache hit is a refcount bump with no re-decode.
        bytes: Arc<[u8]>,
        /// The cells, in key order.
        entries: Vec<Entry>,
    },
    /// An internal node holding the leftmost child and separator cells.
    Internal {
        /// The raw page bytes every borrowed separator key range indexes into.
        bytes: Arc<[u8]>,
        /// The child to the left of every separator.
        leftmost: PageId,
        /// Separator cells, in key order.
        cells: Vec<Separator>,
    },
}

impl Node {
    /// The shared page bytes this node's borrowed keys index into.
    pub fn bytes(&self) -> &Arc<[u8]> {
        match self {
            Node::Leaf { bytes, .. } | Node::Internal { bytes, .. } => bytes,
        }
    }

    /// The key bytes, borrowed from the shared page buffer or the owned key.
    pub fn key<'a>(&'a self, key: &'a Key) -> &'a [u8] {
        match key {
            Key::Borrowed(range) => &self.bytes()[range.clone()],
            Key::Owned(bytes) => bytes,
        }
    }
}

/// Number of bytes a leaf page needs to hold `entries`.
///
/// `source` is the shared page bytes the entries' borrowed key ranges index
/// into.
pub fn leaf_size(source: &[u8], entries: &[Entry]) -> usize {
    HEADER_SIZE
        + SLOT_SIZE * entries.len()
        + entries
            .iter()
            .map(|e| leaf_cell_size(key_bytes(source, &e.key), &e.value))
            .sum::<usize>()
}

/// Whether a `(key, value)` pair fits a leaf page inline, without spilling the
/// value into an overflow chain. A key that does not even fit as an overflow
/// pointer (or as an internal separator) is rejected separately by the tree.
///
/// The ceiling is *half* a page, not a whole one. That is what keeps a split
/// correct: an inline cell can be nearly a page long, and an overflow pointer is
/// tiny, so a leaf mixing the two could reach a size no prefix/suffix split can
/// break into two fitting halves. Requiring every entry to fit in half a page
/// makes a split always possible — see `leaf_split_point`'s caller in
/// `btree/tree.rs`.
pub fn inline_entry_fits(page_size: usize, key: &[u8], value: &[u8]) -> bool {
    HEADER_SIZE + SLOT_SIZE + leaf_cell_size(key, &ValueRef::Owned(Arc::from(value)))
        <= page_size / 2
}

/// Bytes one leaf cell occupies on the page.
fn leaf_cell_size(key: &[u8], value: &ValueRef) -> usize {
    2 + key.len()
        + 1
        + match value {
            ValueRef::Inline(_) | ValueRef::Owned(_) => 4 + value.inline_len(),
            ValueRef::Overflow { .. } => 16,
        }
}

/// The key bytes for `key`, borrowed from `source` or the owned key.
fn key_bytes<'a>(source: &'a [u8], key: &'a Key) -> &'a [u8] {
    match key {
        Key::Borrowed(range) => &source[range.clone()],
        Key::Owned(bytes) => bytes,
    }
}

/// Number of bytes an internal page needs to hold `cells`.
///
/// `source` is the shared page bytes the cells' borrowed key ranges index into.
pub fn internal_size(source: &[u8], cells: &[Separator]) -> usize {
    HEADER_SIZE
        + SLOT_SIZE * cells.len()
        + cells
            .iter()
            .map(|c| internal_cell_size(key_bytes(source, &c.key)))
            .sum::<usize>()
}

/// The split point for an overfull leaf: the largest number of leading
/// entries that still fit a page, never the whole slice. Splitting here packs
/// the left half as full as possible, which keeps the right half small enough
/// to fit — important when entries have very different sizes (e.g. small
/// metadata rows next to large rows carrying a vector). Every entry fits
/// alone — either inline or as an overflow pointer (see `key_fits` in
/// `btree/tree.rs`) — so the result is always at least one for a slice of
/// two or more.
///
/// One pass over the cells, accumulating the size [`leaf_size`] would report
/// for each prefix (AHL-545). It used to call `leaf_size` once per candidate
/// prefix — quadratic in the cell count, and the only per-row cost the
/// batch-insert profile still charged to the split after AHL-542.
pub fn leaf_split_point(source: &[u8], entries: &[Entry], page_size: usize) -> usize {
    split_point(page_size, entries, |entry| {
        leaf_cell_size(key_bytes(source, &entry.key), &entry.value)
    })
}

/// The split point for an overfull internal node: the largest number of
/// leading separators that still fit a page, never the whole slice. See
/// [`leaf_split_point`].
pub fn internal_split_point(source: &[u8], cells: &[Separator], page_size: usize) -> usize {
    split_point(page_size, cells, |cell| {
        internal_cell_size(key_bytes(source, &cell.key))
    })
}

/// The largest `n < cells.len()` such that the first `n` cells fit a page.
///
/// The header and one slot per cell are counted exactly as [`leaf_size`] and
/// [`internal_size`] count them, so the answer is the one their per-prefix
/// caller used to compute, in one pass instead of one per prefix.
fn split_point<T>(page_size: usize, cells: &[T], cell_size: impl Fn(&T) -> usize) -> usize {
    let mut size = HEADER_SIZE;
    let mut split = 0;
    for cell in &cells[..cells.len().saturating_sub(1)] {
        size += SLOT_SIZE + cell_size(cell);
        if size > page_size {
            break;
        }
        split += 1;
    }
    split
}

/// Bytes one internal cell occupies on the page.
fn internal_cell_size(key: &[u8]) -> usize {
    2 + key.len() + 8
}

/// Encode a leaf page.
///
/// `source` is the shared page bytes the entries' borrowed key ranges index
/// into; the encoded page copies those key bytes out of it.
pub fn encode_leaf(page_size: usize, source: &[u8], entries: &[Entry]) -> Result<Vec<u8>> {
    let contents = entries
        .iter()
        .map(|entry| encode_leaf_cell(source, entry))
        .collect::<Result<Vec<_>>>()?;
    encode_page(page_size, KIND_LEAF, 0, &contents)
}

/// Encode an internal page.
pub fn encode_internal(
    page_size: usize,
    source: &[u8],
    leftmost: PageId,
    cells: &[Separator],
) -> Result<Vec<u8>> {
    let contents = cells
        .iter()
        .map(|cell| encode_internal_cell(source, cell))
        .collect::<Vec<_>>();
    encode_page(page_size, KIND_INTERNAL, leftmost, &contents)
}

/// Decode any B-tree page, copying `bytes` into the node's shared buffer.
pub fn decode(page_size: usize, bytes: &[u8]) -> Result<Node> {
    decode_with(page_size, bytes, || Arc::from(bytes))
}

/// Decode any B-tree page out of a buffer that is already shared, so the
/// node's borrowed keys and values index straight into it and nothing is
/// copied. This is what a page handed out by `Device::read_shared` decodes
/// through (AHL-536); [`decode`] is the same routine over a transient slice.
pub fn decode_shared(page_size: usize, bytes: &Arc<[u8]>) -> Result<Node> {
    decode_with(page_size, bytes, || Arc::clone(bytes))
}

/// The one decoder behind [`decode`] and [`decode_shared`]: every check and
/// every cell is read from `bytes`, and `share` is asked for the buffer the
/// node keeps only once the page has proved well-formed.
fn decode_with(page_size: usize, bytes: &[u8], share: impl FnOnce() -> Arc<[u8]>) -> Result<Node> {
    if bytes.len() != page_size {
        return Err(Error::Corrupt(alloc::format!(
            "page is {} bytes, expected {page_size}",
            bytes.len()
        )));
    }
    let kind = bytes[OFF_KIND];
    let count = get_u16(bytes, OFF_CELL_COUNT)? as usize;
    let free_start = get_u16(bytes, OFF_FREE_START)? as usize;
    let leftmost = get_u64(bytes, OFF_LEFTMOST)?;

    if free_start > page_size {
        return Err(Error::Corrupt("free start past end of page".to_string()));
    }
    if HEADER_SIZE + SLOT_SIZE * count > free_start {
        return Err(Error::Corrupt(
            "slot directory overlaps cell area".to_string(),
        ));
    }

    let mut slots = Vec::with_capacity(count);
    for i in 0..count {
        slots.push(get_u16(bytes, HEADER_SIZE + SLOT_SIZE * i)? as usize);
    }

    match kind {
        KIND_LEAF => {
            let mut entries = Vec::with_capacity(count);
            for slot in slots {
                entries.push(decode_leaf_cell(bytes, slot)?);
            }
            Ok(Node::Leaf {
                bytes: share(),
                entries,
            })
        }
        KIND_INTERNAL => {
            let mut cells = Vec::with_capacity(count);
            for slot in slots {
                cells.push(decode_internal_cell(bytes, page_size, slot)?);
            }
            Ok(Node::Internal {
                bytes: share(),
                leftmost,
                cells,
            })
        }
        other => Err(Error::Corrupt(alloc::format!("unknown node kind {other}"))),
    }
}

// ---------------------------------------------------------------- encoding

/// Number of payload bytes one overflow page carries.
pub fn overflow_payload_size(page_size: usize) -> usize {
    page_size - OVERFLOW_HEADER_SIZE
}

/// Encode one overflow page: a pointer to the next page in the chain (0 ends
/// it) followed by `data` (at most [`overflow_payload_size`] bytes).
pub fn encode_overflow(page_size: usize, next: PageId, data: &[u8]) -> Result<Vec<u8>> {
    if OVERFLOW_HEADER_SIZE + data.len() > page_size {
        return Err(Error::Storage(alloc::format!(
            "overflow payload needs {} bytes, page holds {page_size}",
            OVERFLOW_HEADER_SIZE + data.len()
        )));
    }
    let mut buf = vec![0u8; page_size];
    buf[OFF_KIND] = KIND_OVERFLOW;
    buf[OFF_OVERFLOW_NEXT..OFF_OVERFLOW_NEXT + 8].copy_from_slice(&next.to_le_bytes());
    buf[OVERFLOW_HEADER_SIZE..OVERFLOW_HEADER_SIZE + data.len()].copy_from_slice(data);
    Ok(buf)
}

/// Decode one overflow page into its `(next page, payload)` pair.
pub fn decode_overflow(page_size: usize, bytes: &[u8]) -> Result<(PageId, Vec<u8>)> {
    Ok((
        overflow_next(page_size, bytes)?,
        bytes[OVERFLOW_HEADER_SIZE..].to_vec(),
    ))
}

/// The next page of an overflow chain, without copying the payload out of it.
///
/// Split out of [`decode_overflow`] for a caller that only wants the chain's
/// *shape*: [`super::backup`] walks every chain in a snapshot to learn which
/// pages to copy and never looks at a single payload byte, so paying a
/// page-sized `Vec` per link would allocate the whole database twice over on
/// the way to copying it once.
pub fn overflow_next(page_size: usize, bytes: &[u8]) -> Result<PageId> {
    if bytes.len() != page_size {
        return Err(Error::Corrupt(alloc::format!(
            "overflow page is {} bytes, expected {page_size}",
            bytes.len()
        )));
    }
    if bytes[OFF_KIND] != KIND_OVERFLOW {
        return Err(Error::Corrupt("expected an overflow page".to_string()));
    }
    get_u64(bytes, OFF_OVERFLOW_NEXT)
}

fn encode_leaf_cell(source: &[u8], entry: &Entry) -> Result<Vec<u8>> {
    let key = key_bytes(source, &entry.key);
    let mut out = Vec::with_capacity(2 + key.len() + 1 + 16);
    push_u16(&mut out, key.len())?;
    out.extend_from_slice(key);
    match &entry.value {
        ValueRef::Inline(range) => {
            let value = &source[range.clone()];
            out.push(VALUE_INLINE);
            push_u32(&mut out, value.len())?;
            out.extend_from_slice(value);
        }
        ValueRef::Owned(value) => {
            out.push(VALUE_INLINE);
            push_u32(&mut out, value.len())?;
            out.extend_from_slice(value);
        }
        ValueRef::Overflow { first, len } => {
            out.push(VALUE_OVERFLOW);
            push_u64(&mut out, *first);
            push_u64(&mut out, *len as u64);
        }
    }
    Ok(out)
}

fn encode_internal_cell(source: &[u8], cell: &Separator) -> Vec<u8> {
    let key = key_bytes(source, &cell.key);
    let mut out = Vec::with_capacity(2 + key.len() + 8);
    // The key length is a separator key, always small in practice; a u16 that
    // overflows here would already have failed leaf encoding.
    push_u16(&mut out, key.len()).expect("separator key too long");
    out.extend_from_slice(key);
    push_u64(&mut out, cell.child);
    out
}

/// Lay out a header, slot directory and cells inside one `page_size` page.
fn encode_page(
    page_size: usize,
    kind: u8,
    leftmost: PageId,
    contents: &[Vec<u8>],
) -> Result<Vec<u8>> {
    let total =
        HEADER_SIZE + SLOT_SIZE * contents.len() + contents.iter().map(|c| c.len()).sum::<usize>();
    if total > page_size {
        return Err(Error::Storage(alloc::format!(
            "node needs {total} bytes, page holds {page_size}"
        )));
    }

    let mut buf = vec![0u8; page_size];
    buf[OFF_KIND] = kind;
    write_u16(&mut buf[OFF_CELL_COUNT..OFF_CELL_COUNT + 2], contents.len())?;

    let mut cell_cursor = page_size;
    let mut slot_cursor = HEADER_SIZE;
    for content in contents {
        cell_cursor -= content.len();
        buf[cell_cursor..cell_cursor + content.len()].copy_from_slice(content);
        write_u16(&mut buf[slot_cursor..slot_cursor + 2], cell_cursor)?;
        slot_cursor += SLOT_SIZE;
    }
    write_u16(&mut buf[OFF_FREE_START..OFF_FREE_START + 2], cell_cursor)?;
    write_u64(&mut buf[OFF_LEFTMOST..OFF_LEFTMOST + 8], leftmost);

    Ok(buf)
}

// ---------------------------------------------------------------- decoding

/// The one leaf-cell parser behind [`decode_leaf_cell`] (the cached
/// [`Node`]), [`decode_leaf_cell_ref`] and [`scan_leaf_cells`] (the raw
/// scan): the key's byte range and the value, every field read through one
/// bounds-checked `get` on the page rather than a `Result`-returning helper
/// per field.
///
/// This is where a raw scan's per-cell cost lives, and all of it (AHL-541,
/// `docs/research/leaf-offset-table.md`): the page is already a slot
/// directory, so reaching cell *i* is one load and what a cell then costs is
/// what this function does. Written per field through `get_u16`/`get_u32`
/// and a check per bound, the walk was ~4 ns/cell; written this way it is
/// half that on the same bytes. The refusals are the same as before — a
/// slot, key, length or value running past the page, or an unknown tag —
/// and `both_leaf_parsers_agree_on_corrupt_pages` holds this parser to
/// [`decode`]'s verdict byte-flip by byte-flip. `bytes` is the whole page:
/// every caller has checked its length against the page size, so
/// `bytes.len()` is the bound here.
#[inline(always)]
fn parse_leaf_cell(bytes: &[u8], slot: usize) -> Result<(Range<usize>, ValueRef)> {
    let Some(head) = bytes.get(slot..slot + 2) else {
        return Err(Error::Corrupt(
            "leaf cell runs past end of page".to_string(),
        ));
    };
    let key_len = u16::from_le_bytes([head[0], head[1]]) as usize;
    let key_start = slot + 2;
    let key_end = key_start + key_len;
    match bytes.get(key_end) {
        Some(&VALUE_INLINE) => {
            let Some(len) = bytes.get(key_end + 1..key_end + 5) else {
                return Err(Error::Corrupt(
                    "leaf value length runs past end of page".to_string(),
                ));
            };
            let value_len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
            let value_end = key_end + 5 + value_len;
            if value_end > bytes.len() {
                return Err(Error::Corrupt(
                    "leaf value runs past end of page".to_string(),
                ));
            }
            Ok((key_start..key_end, ValueRef::Inline(key_end + 5..value_end)))
        }
        Some(&VALUE_OVERFLOW) => {
            let Some(pointer) = bytes.get(key_end + 1..key_end + 17) else {
                return Err(Error::Corrupt(
                    "overflow pointer runs past end of page".to_string(),
                ));
            };
            let mut first = [0u8; 8];
            first.copy_from_slice(&pointer[..8]);
            let mut len = [0u8; 8];
            len.copy_from_slice(&pointer[8..]);
            Ok((
                key_start..key_end,
                ValueRef::Overflow {
                    first: u64::from_le_bytes(first),
                    len: u64::from_le_bytes(len) as usize,
                },
            ))
        }
        Some(&other) => Err(Error::Corrupt(alloc::format!(
            "unknown leaf value tag {other}"
        ))),
        None => Err(Error::Corrupt("leaf key runs past end of page".to_string())),
    }
}

/// The key of the cell whose slot-directory entry is at `at`, without
/// decoding the cell's value: what [`leaf_edge_keys`] reads, twice per leaf,
/// to decide whether a whole leaf is admitted. Held to the slot and key
/// checks [`parse_leaf_cell`] makes; the value's are left to the scan that
/// always follows.
#[inline(always)]
fn leaf_key_at(bytes: &[u8], at: usize) -> Result<&[u8]> {
    let slot = get_u16(bytes, at)? as usize;
    let Some(head) = bytes.get(slot..slot + 2) else {
        return Err(Error::Corrupt(
            "leaf cell runs past end of page".to_string(),
        ));
    };
    let key_len = u16::from_le_bytes([head[0], head[1]]) as usize;
    bytes
        .get(slot + 2..slot + 2 + key_len)
        .ok_or_else(|| Error::Corrupt("leaf key runs past end of page".to_string()))
}

fn decode_leaf_cell(bytes: &[u8], slot: usize) -> Result<Entry> {
    let (key, value) = parse_leaf_cell(bytes, slot)?;
    Ok(Entry {
        key: Key::Borrowed(key),
        value,
    })
}

fn decode_internal_cell(bytes: &[u8], page_size: usize, slot: usize) -> Result<Separator> {
    if slot + 2 > page_size {
        return Err(Error::Corrupt(
            "internal cell runs past end of page".to_string(),
        ));
    }
    let key_len = get_u16(bytes, slot)? as usize;
    let key_end = slot + 2 + key_len;
    if key_end + 8 > page_size {
        return Err(Error::Corrupt(
            "internal key runs past end of page".to_string(),
        ));
    }
    Ok(Separator {
        key: Key::Borrowed(slot + 2..key_end),
        child: get_u64(bytes, key_end)?,
    })
}

// ----------------------------------------------------- borrowed raw-leaf scan

/// A leaf cell parsed with its key borrowed from the page bytes rather than
/// copied into an owned `Vec`.
///
/// This is the scan fast path's leaf-cell view: a sequential scan reads the row
/// id out of the key and tests it against the walk bounds, and never keeps the
/// key — so copying it (as [`decode_leaf_cell`] does for the cached [`Node`])
/// is an allocation a scan will immediately throw away. The value is still
/// materialised exactly as [`decode_leaf_cell`] materialises it.
pub struct LeafCellRef<'a> {
    /// The cell key, borrowed from the page bytes it was parsed from.
    pub key: &'a [u8],
    /// The value — inline bytes shared, or an overflow pointer.
    pub value: ValueRef,
}

/// Parse one leaf cell of a whole page, borrowing the key.
///
/// The same corruption checks as [`decode_leaf_cell`] — a slot or key or value
/// running past the end of the page is corruption, never a silent truncation
/// — because it is the same parser; only the key is a slice into `bytes`
/// instead of a range. `bytes` must be the whole page, as it is for every
/// caller of the raw scan; the cell is bounded by its length.
pub fn decode_leaf_cell_ref(bytes: &[u8], slot: usize) -> Result<LeafCellRef<'_>> {
    let (key, value) = parse_leaf_cell(bytes, slot)?;
    Ok(LeafCellRef {
        key: &bytes[key],
        value,
    })
}

/// The first and last keys of a leaf page, borrowed from it; `None` for an
/// empty leaf.
///
/// What a raw scan asks before it walks the cells: a leaf's keys are sorted,
/// so when both edges fall inside a walk's bounds every cell between them does
/// too, and the per-cell bound check can be skipped for the whole page
/// (`CowBTree::scan_leaf_into`). Held to the same header checks
/// [`scan_leaf_cells`] makes and to the same slot and key checks per cell, so
/// a page whose edge *keys* would fail the scan fails here first. The edge
/// cells' values are not decoded: the answer does not need them, reading
/// their lengths was most of this function's cost (AHL-541), and the scan
/// that always follows refuses a corrupt value on the same cell.
pub fn leaf_edge_keys(bytes: &[u8], page_size: usize) -> Result<Option<(&[u8], &[u8])>> {
    let count = check_leaf_header(bytes, page_size)?;
    if count == 0 {
        return Ok(None);
    }
    Ok(Some((
        leaf_key_at(bytes, HEADER_SIZE)?,
        leaf_key_at(bytes, HEADER_SIZE + SLOT_SIZE * (count - 1))?,
    )))
}

/// The header checks a raw leaf read repeats from [`decode`]: page length,
/// and the slot directory not overlapping the cell area. Returns the cell
/// count.
fn check_leaf_header(bytes: &[u8], page_size: usize) -> Result<usize> {
    if bytes.len() != page_size {
        return Err(Error::Corrupt(alloc::format!(
            "page is {} bytes, expected {page_size}",
            bytes.len()
        )));
    }
    let count = get_u16(bytes, OFF_CELL_COUNT)? as usize;
    let free_start = get_u16(bytes, OFF_FREE_START)? as usize;
    if free_start > page_size {
        return Err(Error::Corrupt("free start past end of page".to_string()));
    }
    if HEADER_SIZE + SLOT_SIZE * count > free_start {
        return Err(Error::Corrupt(
            "slot directory overlaps cell area".to_string(),
        ));
    }
    Ok(count)
}

/// Run `f` over every leaf cell of `bytes`, in key order, with each cell's key
/// borrowed from the page.
///
/// The header checks [`decode`] performs — page length, the slot directory not
/// overlapping the cell area — are repeated here, so a raw scan is held to the
/// same corruption standard as a decoded one. `f` is called while `bytes` is
/// borrowed, so it may not outlive the call.
pub fn scan_leaf_cells<'a>(
    bytes: &'a [u8],
    page_size: usize,
    mut f: impl FnMut(&'a [u8], ValueRef) -> Result<()>,
) -> Result<()> {
    let count = check_leaf_header(bytes, page_size)?;
    // In bounds by the header check: the directory ends at or before
    // `free_start`, which is at or before the page's end.
    let slots = &bytes[HEADER_SIZE..HEADER_SIZE + SLOT_SIZE * count];
    for slot in slots.chunks_exact(SLOT_SIZE) {
        let slot = u16::from_le_bytes([slot[0], slot[1]]) as usize;
        let (key, value) = parse_leaf_cell(bytes, slot)?;
        f(&bytes[key], value)?;
    }
    Ok(())
}

// --------------------------------------------------------- little-endian I/O

fn push_u16(buf: &mut Vec<u8>, value: usize) -> Result<()> {
    let value =
        u16::try_from(value).map_err(|_| Error::Corrupt("value exceeds u16".to_string()))?;
    buf.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn push_u32(buf: &mut Vec<u8>, value: usize) -> Result<()> {
    let value =
        u32::try_from(value).map_err(|_| Error::Corrupt("value exceeds u32".to_string()))?;
    buf.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn push_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn write_u16(buf: &mut [u8], value: usize) -> Result<()> {
    let value =
        u16::try_from(value).map_err(|_| Error::Corrupt("value exceeds u16".to_string()))?;
    buf.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u64(buf: &mut [u8], value: u64) {
    buf.copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn get_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| Error::Corrupt("short read for u16".to_string()))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

#[inline]
fn get_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| Error::Corrupt("short read for u64".to_string()))?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(slice);
    Ok(u64::from_le_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &[u8], value: &[u8]) -> Entry {
        Entry {
            key: Key::Owned(key.to_vec()),
            value: ValueRef::Owned(Arc::from(value)),
        }
    }

    #[test]
    fn a_leaf_round_trips() {
        let entries = [
            entry(b"a", b"1"),
            entry(b"bb", b"22"),
            entry(b"ccc", b"333"),
        ];
        // Every key is owned, so no shared page buffer is indexed here.
        let encoded = encode_leaf(512, &[], &entries).unwrap();
        assert_eq!(encoded.len(), 512);
        let node = decode(512, &encoded).unwrap();
        let Node::Leaf {
            entries: decoded, ..
        } = &node
        else {
            panic!("not a leaf");
        };
        assert_eq!(decoded.len(), 3);
        for (entry, original) in decoded.iter().zip(entries.iter()) {
            assert_eq!(node.key(&entry.key), original.key.resolve(&[]));
            assert_eq!(
                entry.value.inline_bytes(node.bytes()),
                original.value.inline_bytes(&[])
            );
        }
    }

    #[test]
    fn an_internal_round_trips() {
        let cells = [
            Separator {
                key: Key::Owned(b"m".to_vec()),
                child: 7,
            },
            Separator {
                key: Key::Owned(b"zz".to_vec()),
                child: 99,
            },
        ];
        let encoded = encode_internal(512, &[], 3, &cells).unwrap();
        let node = decode(512, &encoded).unwrap();
        let Node::Internal {
            leftmost,
            cells: decoded,
            ..
        } = &node
        else {
            panic!("not an internal node");
        };
        assert_eq!(*leftmost, 3);
        assert_eq!(decoded.len(), 2);
        for (cell, original) in decoded.iter().zip(cells.iter()) {
            assert_eq!(node.key(&cell.key), original.key.resolve(&[]));
            assert_eq!(cell.child, original.child);
        }
    }

    #[test]
    fn a_node_that_does_not_fit_is_rejected() {
        let entries = [entry(&[0u8; 600], b"v")];
        assert!(encode_leaf(512, &[], &entries).is_err());
    }

    #[test]
    fn a_truncated_page_is_rejected() {
        assert!(decode(512, &[0u8; 100]).is_err());
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let mut bytes = vec![0u8; 512];
        bytes[OFF_KIND] = 42;
        assert!(decode(512, &bytes).is_err());
    }

    #[test]
    fn an_overflow_pointer_round_trips() {
        let entries = [Entry {
            key: Key::Owned(b"k".to_vec()),
            value: ValueRef::Overflow {
                first: 9,
                len: 1024,
            },
        }];
        let encoded = encode_leaf(512, &[], &entries).unwrap();
        let node = decode(512, &encoded).unwrap();
        let Node::Leaf {
            entries: decoded, ..
        } = &node
        else {
            panic!("not a leaf");
        };
        assert_eq!(decoded.len(), 1);
        assert_eq!(node.key(&decoded[0].key), b"k");
        assert_eq!(decoded[0].value, entries[0].value);
    }

    #[test]
    fn an_overflow_page_round_trips() {
        let encoded = encode_overflow(512, 7, b"hello").unwrap();
        assert_eq!(encoded.len(), 512);
        assert_eq!(
            decode_overflow(512, &encoded).unwrap(),
            (7, {
                let mut payload = b"hello".to_vec();
                payload.resize(512 - OVERFLOW_HEADER_SIZE, 0);
                payload
            })
        );
    }

    #[test]
    fn an_overflow_page_of_the_wrong_kind_is_rejected() {
        let mut bytes = vec![0u8; 512];
        bytes[OFF_KIND] = KIND_LEAF;
        assert!(decode_overflow(512, &bytes).is_err());
    }

    // ------------------------------------- the two leaf parsers are one parser

    /// What one leaf page's cells look like once parsed, in a form both
    /// parsers can be compared in: the key copied out, and the value exactly
    /// as each returned it.
    type Parsed = Result<Vec<(Vec<u8>, ValueRef)>>;

    /// Read one leaf page both ways and return what each made of it.
    ///
    /// [`decode`] materialises a whole [`Node`] through [`decode_leaf_cell`];
    /// [`scan_leaf_cells`] walks the same page through the raw-scan path.
    /// Since AHL-541 both reach one cell parser (`parse_leaf_cell`), but the
    /// header checks and the slot walk that lead to it are still two
    /// implementations — `decode_with`'s and `check_leaf_header`'s — and
    /// this is what keeps them agreeing.
    fn both_ways(page_size: usize, bytes: &[u8]) -> (Parsed, Parsed) {
        let decoded = decode(page_size, bytes).and_then(|node| match &node {
            Node::Leaf { entries, .. } => Ok(entries
                .iter()
                .map(|entry| (node.key(&entry.key).to_vec(), entry.value.clone()))
                .collect()),
            Node::Internal { .. } => Err(Error::Corrupt("not a leaf".to_string())),
        });

        let mut scanned = Vec::new();
        let walk = scan_leaf_cells(bytes, page_size, |key, value| {
            scanned.push((key.to_vec(), value));
            Ok(())
        })
        .map(|()| scanned);

        (decoded, walk)
    }

    /// The two parsers must admit exactly the same cells, on any page.
    ///
    /// This is the project's fast-path/slow-path rule applied to a pair that
    /// did not have it: `decode_leaf_cell_ref`'s doc *claims* "the same
    /// corruption checks as `decode_leaf_cell`", and nothing enforced the
    /// claim. It became load-bearing when the raw leaf scan started reading
    /// through the page cache, because a cached page is now parsed by
    /// `decode_leaf_cell` and an uncached one by `decode_leaf_cell_ref` — the
    /// same page, either parser, decided only by whether it happened to be
    /// resident. A divergence would show up as one query returning different
    /// rows depending on cache state, which is the worst shape a bug can take:
    /// it would not reproduce.
    #[test]
    fn both_leaf_parsers_agree_on_well_formed_pages() {
        let shapes: Vec<Vec<Entry>> = vec![
            vec![],
            vec![entry(b"k", b"v")],
            vec![entry(b"", b"")],
            vec![entry(b"a", b"1"), entry(b"b", b"2"), entry(b"c", b"3")],
            vec![entry(b"long-ish key", &[7u8; 200])],
            vec![Entry {
                key: Key::Owned(b"spilled".to_vec()),
                value: ValueRef::Overflow {
                    first: 42,
                    len: 100_000,
                },
            }],
            vec![
                entry(b"inline", b"short"),
                Entry {
                    key: Key::Owned(b"overflowed".to_vec()),
                    value: ValueRef::Overflow { first: 9, len: 1 },
                },
            ],
        ];

        for entries in shapes {
            let page = encode_leaf(512, &[], &entries).unwrap();
            let (decoded, scanned) = both_ways(512, &page);
            assert_eq!(
                decoded.unwrap(),
                scanned.unwrap(),
                "the two parsers disagree on a well-formed page"
            );
        }
    }

    /// The edge keys are the first and last cells, in slot order — what the
    /// whole-leaf admission check reads instead of every cell.
    #[test]
    fn leaf_edge_keys_are_the_first_and_last_cells() {
        let empty = encode_leaf(512, &[], &[]).unwrap();
        assert_eq!(leaf_edge_keys(&empty, 512).unwrap(), None);

        let one = encode_leaf(512, &[], &[entry(b"only", b"v")]).unwrap();
        assert_eq!(
            leaf_edge_keys(&one, 512).unwrap(),
            Some((&b"only"[..], &b"only"[..]))
        );

        let three = encode_leaf(
            512,
            &[],
            &[entry(b"a", b"1"), entry(b"b", b"2"), entry(b"c", b"3")],
        )
        .unwrap();
        assert_eq!(
            leaf_edge_keys(&three, 512).unwrap(),
            Some((&b"a"[..], &b"c"[..]))
        );

        // Held to the scan's header checks: a page of the wrong length is
        // refused here exactly as `scan_leaf_cells` refuses it.
        assert!(leaf_edge_keys(&three[..511], 512).is_err());

        // And to the cell's slot and key checks: an edge key that runs past
        // the page is refused here, before the scan would refuse it.
        let last_slot = HEADER_SIZE + SLOT_SIZE * 2;
        let last_cell = get_u16(&three, last_slot).unwrap() as usize;
        let mut long_key = three.clone();
        long_key[last_cell..last_cell + 2].copy_from_slice(&0xffffu16.to_le_bytes());
        assert!(leaf_edge_keys(&long_key, 512).is_err());
        assert!(scan_leaf_cells(&long_key, 512, |_, _| Ok(())).is_err());

        // The edge cells' *values* are not decoded here — the answer does not
        // need them — so a corrupt value length on an edge cell is left to the
        // scan that always follows, which does refuse it.
        let mut long_value = three.clone();
        let value_len_at = last_cell + 2 + 1 + 1;
        long_value[value_len_at..value_len_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            leaf_edge_keys(&long_value, 512).unwrap(),
            Some((&b"a"[..], &b"c"[..]))
        );
        assert!(scan_leaf_cells(&long_value, 512, |_, _| Ok(())).is_err());
    }

    /// Each refusal the shared cell parser makes, pinned on its own.
    ///
    /// `both_leaf_parsers_agree_on_corrupt_pages` below ties the two *paths*
    /// together, but since AHL-541 they share `parse_leaf_cell`, so a check
    /// dropped from the parser would be dropped from both and the tie would
    /// still hold. This is the test that fails when one goes: a key length,
    /// an inline value length, an overflow pointer that runs past the page,
    /// and an unknown tag, each refused by `decode` and by `scan_leaf_cells`.
    #[test]
    fn a_corrupt_leaf_cell_is_refused_by_both_parsers() {
        let clean = encode_leaf(512, &[], &[entry(b"k", b"v")]).unwrap();
        let cell = get_u16(&clean, HEADER_SIZE).unwrap() as usize;
        // `key_len u16 | key (1) | tag | value_len u32 | value (1)`
        let tag_at = cell + 2 + 1;
        let value_len_at = tag_at + 1;

        let mut long_key = clean.clone();
        long_key[cell..cell + 2].copy_from_slice(&0xffffu16.to_le_bytes());
        let mut long_value = clean.clone();
        long_value[value_len_at..value_len_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        // The cell sits at the page's tail with five bytes after its tag, so
        // an overflow pointer (sixteen) cannot fit.
        let mut short_pointer = clean.clone();
        short_pointer[tag_at] = VALUE_OVERFLOW;
        let mut bad_tag = clean.clone();
        bad_tag[tag_at] = 7;

        for (what, page) in [
            ("key past the page", long_key),
            ("value past the page", long_value),
            ("overflow pointer past the page", short_pointer),
            ("unknown tag", bad_tag),
        ] {
            let (decoded, scanned) = both_ways(512, &page);
            assert!(decoded.is_err(), "{what}: decode accepted the page");
            assert!(
                scanned.is_err(),
                "{what}: scan_leaf_cells accepted the page"
            );
        }
    }

    /// And they must *fail* together too. A parser that accepts a corrupt page
    /// the other rejects is the same divergence wearing a different hat: the
    /// cached path would serve rows the uncached path calls corruption.
    #[test]
    fn both_leaf_parsers_agree_on_corrupt_pages() {
        let entries = vec![
            entry(b"alpha", b"one"),
            entry(b"beta", b"two"),
            Entry {
                key: Key::Owned(b"gamma".to_vec()),
                value: ValueRef::Overflow { first: 3, len: 40 },
            },
        ];
        let clean = encode_leaf(512, &[], &entries).unwrap();

        // Walk a byte at a time through the header and slot directory — where
        // a flip changes counts and offsets rather than payload — and a sample
        // of the cell area beyond it.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut probes: Vec<usize> = (0..64).collect();
        for _ in 0..192 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            probes.push((state % 512) as usize);
        }

        for at in probes {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut page = clean.clone();
                page[at] ^= flip;
                // Only leaves reach `scan_leaf_cells` — `walk_raw_row_values`
                // dispatches on the kind byte first — so a flip that turns the
                // page into another kind is out of contract, not a divergence.
                if page[OFF_KIND] != KIND_LEAF {
                    continue;
                }
                let (decoded, scanned) = both_ways(512, &page);
                assert_eq!(
                    decoded.is_ok(),
                    scanned.is_ok(),
                    "byte {at} flipped by {flip:#04x}: one parser accepted the page and the \
                     other rejected it"
                );
                if let (Ok(decoded), Ok(scanned)) = (decoded, scanned) {
                    assert_eq!(
                        decoded, scanned,
                        "byte {at} flipped by {flip:#04x}: both parsers accepted the page and \
                         read different cells out of it"
                    );
                }
            }
        }
    }
    /// The split points as they were before AHL-545 — `leaf_size`/
    /// `internal_size` called once per candidate prefix — kept verbatim so
    /// the one-pass versions can be held to their answer. Nothing outside the
    /// tests reaches them.
    mod legacy {
        use super::super::*;

        pub fn leaf_split_point(bytes: &[u8], entries: &[Entry], page_size: usize) -> usize {
            let mut split = 1;
            while split < entries.len() && leaf_size(bytes, &entries[..split]) <= page_size {
                split += 1;
            }
            split - 1
        }

        pub fn internal_split_point(bytes: &[u8], cells: &[Separator], page_size: usize) -> usize {
            let mut split = 1;
            while split < cells.len() && internal_size(bytes, &cells[..split]) <= page_size {
                split += 1;
            }
            split - 1
        }
    }

    /// A small deterministic generator for the parity tests: the shapes have
    /// to cover cells near the page's ceiling as well as tiny ones, and a
    /// fixed seed makes a failure reproducible.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// A page of `count` leaf cells to borrow from, plus entries that borrow
    /// their keys and values from it — the shape a decoded page has — mixed
    /// with owned keys, owned values and overflow pointers, the shapes the
    /// write path produces.
    fn mixed_entries(rng: &mut Lcg, page_size: usize, count: usize) -> (Vec<u8>, Vec<Entry>) {
        let borrowed = (0..count)
            .map(|i| {
                let key = alloc::format!("k{i:05}").into_bytes();
                let value = alloc::vec![i as u8; rng.below(24)];
                entry(&key, &value)
            })
            .collect::<Vec<_>>();
        let source = encode_leaf(page_size, &[], &borrowed).unwrap();
        let Node::Leaf { entries, .. } = decode(page_size, &source).unwrap() else {
            panic!("not a leaf");
        };
        let entries = entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| match rng.below(4) {
                0 => e,
                1 => Entry {
                    key: Key::Owned(e.key.resolve(&source).to_vec()),
                    value: e.value,
                },
                2 => Entry {
                    key: e.key,
                    value: ValueRef::Owned(Arc::from(alloc::vec![0xAB; rng.below(40)])),
                },
                _ => Entry {
                    key: Key::Owned(alloc::format!("o{i:05}").into_bytes()),
                    value: ValueRef::Overflow {
                        first: 1000 + i as u64,
                        len: 100_000 + i,
                    },
                },
            })
            .collect();
        (source, entries)
    }

    /// Every prefix length of a random sequence of cell sizes — tiny cells,
    /// cells near half a page, overflow pointers — splits where the
    /// per-prefix `leaf_size` loop split it.
    #[test]
    fn the_split_point_is_the_one_the_per_prefix_loop_chose() {
        let mut rng = Lcg(3);
        for page_size in [MIN_PAGE_SIZE * 4, 4096] {
            let ceiling = page_size / 2 - (HEADER_SIZE + SLOT_SIZE + 2 + 1 + 4);
            for round in 0..40 {
                let entries = (0..(1 + rng.below(60)))
                    .map(|i| {
                        let key = alloc::vec![i as u8; 1 + rng.below(8)];
                        match rng.below(3) {
                            // Near the limit: the value fills the cell to half
                            // a page, give or take a few bytes.
                            0 => entry(&key, &alloc::vec![1; ceiling - key.len() - rng.below(4)]),
                            1 => entry(&key, &alloc::vec![2; rng.below(64)]),
                            _ => Entry {
                                key: Key::Owned(key),
                                value: ValueRef::Overflow {
                                    first: 9,
                                    len: 1 << 20,
                                },
                            },
                        }
                    })
                    .collect::<Vec<_>>();
                for prefix in 0..=entries.len() {
                    let slice = &entries[..prefix];
                    assert_eq!(
                        leaf_split_point(&[], slice, page_size),
                        legacy::leaf_split_point(&[], slice, page_size),
                        "page_size {page_size}, round {round}, prefix {prefix}"
                    );
                }
                let cells = (0..(1 + rng.below(60)))
                    .map(|i| Separator {
                        key: Key::Owned(alloc::vec![
                            i as u8;
                            match rng.below(3) {
                                0 => page_size / 2 - (HEADER_SIZE + SLOT_SIZE + 2 + 8) - rng.below(4),
                                _ => 1 + rng.below(16),
                            }
                        ]),
                        child: i as u64,
                    })
                    .collect::<Vec<_>>();
                for prefix in 0..=cells.len() {
                    let slice = &cells[..prefix];
                    assert_eq!(
                        internal_split_point(&[], slice, page_size),
                        legacy::internal_split_point(&[], slice, page_size),
                        "page_size {page_size}, round {round}, prefix {prefix}"
                    );
                }
            }
        }
    }

    /// The chosen prefix fits and the next one does not: the property the
    /// tree's split relies on, stated without reference to either encoder.
    #[test]
    fn the_split_point_is_the_largest_fitting_prefix() {
        let mut rng = Lcg(5);
        let page_size = 1024;
        let (source, entries) = mixed_entries(&mut rng, 16384, 300);
        let split = leaf_split_point(&source, &entries, page_size);
        assert!(split >= 1 && split < entries.len());
        assert!(leaf_size(&source, &entries[..split]) <= page_size);
        assert!(leaf_size(&source, &entries[..split + 1]) > page_size);
        assert_eq!(leaf_split_point(&source, &entries[..1], page_size), 0);
        assert_eq!(leaf_split_point(&source, &[], page_size), 0);
    }
}
