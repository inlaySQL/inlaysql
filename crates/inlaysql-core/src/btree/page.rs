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

use alloc::rc::Rc;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

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
/// The inline case is `Rc<[u8]>`, not `Vec<u8>` (`AHL-478`): a decoded `Node`
/// is cached behind its own `Rc` (`btree::cache::PageCache`), and every read
/// that hits the cache used to clone these bytes byte-for-byte to hand a
/// caller an owned `Vec<u8>` — `CowBTree::resolve_value_at`, the specific
/// site `PERF.md` names as untouched and largest. Reference-counting the
/// inline bytes too turns that clone into a refcount bump: still one
/// allocation the first time a page is decoded off the device, and zero on
/// every cache hit after that. Nothing about the on-disk format changes —
/// this is purely how the decoded bytes are held in memory once read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueRef {
    /// The value bytes, stored in the leaf cell itself, shared with the
    /// cached page they were decoded from.
    Inline(Rc<[u8]>),
    /// The value lives in a chain of overflow pages starting at `first`, and is
    /// `len` bytes long in total.
    Overflow {
        /// The first overflow page of the chain.
        first: PageId,
        /// Total length of the value across the whole chain, in bytes.
        len: usize,
    },
}

/// A decoded key/value pair from a leaf page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The key.
    pub key: Vec<u8>,
    /// The value — inline bytes, or a pointer to an overflow chain.
    pub value: ValueRef,
}

/// A decoded separator from an internal page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Separator {
    /// The smallest key in `child`'s subtree at split time.
    pub key: Vec<u8>,
    /// The child page to the right of this separator.
    pub child: PageId,
}

/// A decoded B-tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A leaf holding its entries in key order.
    Leaf(Vec<Entry>),
    /// An internal node holding the leftmost child and separator cells.
    Internal {
        /// The child to the left of every separator.
        leftmost: PageId,
        /// Separator cells, in key order.
        cells: Vec<Separator>,
    },
}

/// Number of bytes a leaf page needs to hold `entries`.
pub fn leaf_size(entries: &[Entry]) -> usize {
    HEADER_SIZE
        + SLOT_SIZE * entries.len()
        + entries
            .iter()
            .map(|e| leaf_cell_size(&e.key, &e.value))
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
/// makes a split always possible — see [`leaf_split_point`]'s caller in
/// `btree/tree.rs`.
pub fn inline_entry_fits(page_size: usize, key: &[u8], value: &[u8]) -> bool {
    HEADER_SIZE + SLOT_SIZE + leaf_cell_size(key, &ValueRef::Inline(Rc::from(value)))
        <= page_size / 2
}

/// Bytes one leaf cell occupies on the page.
fn leaf_cell_size(key: &[u8], value: &ValueRef) -> usize {
    2 + key.len()
        + 1
        + match value {
            ValueRef::Inline(bytes) => 4 + bytes.len(),
            ValueRef::Overflow { .. } => 16,
        }
}

/// Number of bytes an internal page needs to hold `cells`.
pub fn internal_size(cells: &[Separator]) -> usize {
    HEADER_SIZE + SLOT_SIZE * cells.len() + cells.iter().map(|c| 2 + c.key.len() + 8).sum::<usize>()
}

/// Encode a leaf page.
pub fn encode_leaf(page_size: usize, entries: &[Entry]) -> Result<Vec<u8>> {
    let contents = entries
        .iter()
        .map(encode_leaf_cell)
        .collect::<Result<Vec<_>>>()?;
    encode_page(page_size, KIND_LEAF, 0, &contents)
}

/// Encode an internal page.
pub fn encode_internal(page_size: usize, leftmost: PageId, cells: &[Separator]) -> Result<Vec<u8>> {
    let contents = cells.iter().map(encode_internal_cell).collect::<Vec<_>>();
    encode_page(page_size, KIND_INTERNAL, leftmost, &contents)
}

/// Decode any B-tree page.
pub fn decode(page_size: usize, bytes: &[u8]) -> Result<Node> {
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
                entries.push(decode_leaf_cell(bytes, page_size, slot)?);
            }
            Ok(Node::Leaf(entries))
        }
        KIND_INTERNAL => {
            let mut cells = Vec::with_capacity(count);
            for slot in slots {
                cells.push(decode_internal_cell(bytes, page_size, slot)?);
            }
            Ok(Node::Internal { leftmost, cells })
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
    if bytes.len() != page_size {
        return Err(Error::Corrupt(alloc::format!(
            "overflow page is {} bytes, expected {page_size}",
            bytes.len()
        )));
    }
    if bytes[OFF_KIND] != KIND_OVERFLOW {
        return Err(Error::Corrupt("expected an overflow page".to_string()));
    }
    let next = get_u64(bytes, OFF_OVERFLOW_NEXT)?;
    Ok((next, bytes[OVERFLOW_HEADER_SIZE..].to_vec()))
}

fn encode_leaf_cell(entry: &Entry) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(2 + entry.key.len() + 1 + 16);
    push_u16(&mut out, entry.key.len())?;
    out.extend_from_slice(&entry.key);
    match &entry.value {
        ValueRef::Inline(value) => {
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

fn encode_internal_cell(cell: &Separator) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + cell.key.len() + 8);
    // The key length is a separator key, always small in practice; a u16 that
    // overflows here would already have failed leaf encoding.
    push_u16(&mut out, cell.key.len()).expect("separator key too long");
    out.extend_from_slice(&cell.key);
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

fn decode_leaf_cell(bytes: &[u8], page_size: usize, slot: usize) -> Result<Entry> {
    if slot + 3 > page_size {
        return Err(Error::Corrupt(
            "leaf cell runs past end of page".to_string(),
        ));
    }
    let key_len = get_u16(bytes, slot)? as usize;
    let key_end = slot + 2 + key_len;
    if key_end + 1 > page_size {
        return Err(Error::Corrupt("leaf key runs past end of page".to_string()));
    }
    let key = bytes[slot + 2..key_end].to_vec();
    match bytes[key_end] {
        VALUE_INLINE => {
            if key_end + 5 > page_size {
                return Err(Error::Corrupt(
                    "leaf value length runs past end of page".to_string(),
                ));
            }
            let value_len = get_u32(bytes, key_end + 1)? as usize;
            let value_end = key_end + 5 + value_len;
            if value_end > page_size {
                return Err(Error::Corrupt(
                    "leaf value runs past end of page".to_string(),
                ));
            }
            Ok(Entry {
                key,
                value: ValueRef::Inline(Rc::from(&bytes[key_end + 5..value_end])),
            })
        }
        VALUE_OVERFLOW => {
            if key_end + 17 > page_size {
                return Err(Error::Corrupt(
                    "overflow pointer runs past end of page".to_string(),
                ));
            }
            let first = get_u64(bytes, key_end + 1)?;
            let len = get_u64(bytes, key_end + 9)? as usize;
            Ok(Entry {
                key,
                value: ValueRef::Overflow { first, len },
            })
        }
        other => Err(Error::Corrupt(alloc::format!(
            "unknown leaf value tag {other}"
        ))),
    }
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
        key: bytes[slot + 2..key_end].to_vec(),
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

/// Parse one leaf cell, borrowing the key.
///
/// The same corruption checks as [`decode_leaf_cell`] — a slot or key or value
/// running past the end of the page is corruption, never a silent truncation —
/// only the key is a slice into `bytes` instead of an owned copy.
pub fn decode_leaf_cell_ref<'a>(
    bytes: &'a [u8],
    page_size: usize,
    slot: usize,
) -> Result<LeafCellRef<'a>> {
    if slot + 3 > page_size {
        return Err(Error::Corrupt(
            "leaf cell runs past end of page".to_string(),
        ));
    }
    let key_len = get_u16(bytes, slot)? as usize;
    let key_end = slot + 2 + key_len;
    if key_end + 1 > page_size {
        return Err(Error::Corrupt("leaf key runs past end of page".to_string()));
    }
    let key = &bytes[slot + 2..key_end];
    match bytes[key_end] {
        VALUE_INLINE => {
            if key_end + 5 > page_size {
                return Err(Error::Corrupt(
                    "leaf value length runs past end of page".to_string(),
                ));
            }
            let value_len = get_u32(bytes, key_end + 1)? as usize;
            let value_end = key_end + 5 + value_len;
            if value_end > page_size {
                return Err(Error::Corrupt(
                    "leaf value runs past end of page".to_string(),
                ));
            }
            Ok(LeafCellRef {
                key,
                value: ValueRef::Inline(Rc::from(&bytes[key_end + 5..value_end])),
            })
        }
        VALUE_OVERFLOW => {
            if key_end + 17 > page_size {
                return Err(Error::Corrupt(
                    "overflow pointer runs past end of page".to_string(),
                ));
            }
            let first = get_u64(bytes, key_end + 1)?;
            let len = get_u64(bytes, key_end + 9)? as usize;
            Ok(LeafCellRef {
                key,
                value: ValueRef::Overflow { first, len },
            })
        }
        other => Err(Error::Corrupt(alloc::format!(
            "unknown leaf value tag {other}"
        ))),
    }
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
    for i in 0..count {
        let slot = get_u16(bytes, HEADER_SIZE + SLOT_SIZE * i)? as usize;
        let cell = decode_leaf_cell_ref(bytes, page_size, slot)?;
        f(cell.key, cell.value)?;
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

fn get_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| Error::Corrupt("short read for u16".to_string()))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn get_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| Error::Corrupt("short read for u32".to_string()))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

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
            key: key.to_vec(),
            value: ValueRef::Inline(Rc::from(value)),
        }
    }

    #[test]
    fn a_leaf_round_trips() {
        let entries = [
            entry(b"a", b"1"),
            entry(b"bb", b"22"),
            entry(b"ccc", b"333"),
        ];
        let encoded = encode_leaf(512, &entries).unwrap();
        assert_eq!(encoded.len(), 512);
        assert_eq!(decode(512, &encoded).unwrap(), Node::Leaf(entries.to_vec()));
    }

    #[test]
    fn an_internal_round_trips() {
        let cells = [
            Separator {
                key: b"m".to_vec(),
                child: 7,
            },
            Separator {
                key: b"zz".to_vec(),
                child: 99,
            },
        ];
        let encoded = encode_internal(512, 3, &cells).unwrap();
        assert_eq!(
            decode(512, &encoded).unwrap(),
            Node::Internal {
                leftmost: 3,
                cells: cells.to_vec()
            }
        );
    }

    #[test]
    fn a_node_that_does_not_fit_is_rejected() {
        let entries = [entry(&[0u8; 600], b"v")];
        assert!(encode_leaf(512, &entries).is_err());
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
            key: b"k".to_vec(),
            value: ValueRef::Overflow {
                first: 9,
                len: 1024,
            },
        }];
        let encoded = encode_leaf(512, &entries).unwrap();
        assert_eq!(decode(512, &encoded).unwrap(), Node::Leaf(entries.to_vec()));
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
}
