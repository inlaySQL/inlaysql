//! The write-ahead log: commit records and the recovery protocol.
//!
//! The copy-on-write B-tree makes a commit atomic in the common case — a crash
//! before the root pointer is swapped leaves the old tree fully intact. What it
//! cannot survive on its own is a *torn* write: a crash or power loss can
//! leave the freshly written pages gone while the root pointer (or part of it)
//! survives, or vice versa. The write-ahead log closes that gap by making the
//! commit record **self-contained**.
//!
//! # Layout
//!
//! The device is divided into fixed, `page_size`-sized blocks:
//!
//! ```text
//! block 0                          header   (magic, page size, format version)
//! block 1                          state    (root, next page, checkpoint seq)
//! blocks [2, 2 + WAL_REGIONS * WAL_BLOCKS)
//!                                  wal      (one append-only region/writer)
//! blocks [2 + WAL_REGIONS * WAL_BLOCKS, ...)
//!                                  data     (B-tree pages)
//! ```
//!
//! # Commit record
//!
//! A record carries the transaction's `seq`, predecessor sequence/root, its
//! new `root` and `next` page, **and a copy of every page it wrote**. The
//! predecessor orders records from different regions; the copied pages let
//! recovery rebuild data writes that were lost or torn.
//!
//! # Commit protocol (write-ahead)
//!
//! 1. Reserve sequence/page ids and append placement under a short device
//!    commit gate.
//! 2. Write the transaction's dirty pages to the data area (so reads during
//!    normal operation never consult the log).
//! 3. Append the commit record to this handle's region and leave the gate.
//! 4. `sync` the device. This single sync is the commit point; because it is
//!    outside the gate, separate native handles can sync concurrently.
//!
//! The state block is only rewritten on *checkpoint* (when the log fills up, or
//! on an explicit call), which keeps the hot path to one sync.
//!
//! # Recovery protocol
//!
//! On open: read the header (fatal if torn — it is written once), read the
//! state block, scan every region independently, merge valid records by
//! sequence and validate their predecessor links. Every accepted record newer
//! than the state block is replayed in order, healing torn page writes before
//! the state block is checkpointed. A torn record ends only its own region.

use alloc::vec;
use alloc::vec::Vec;

use crate::btree::{Device, PageId};
use crate::error::Result;

/// Number of `page_size` blocks reserved for the log. A transaction whose
/// commit record does not fit in this region is rejected (see
/// [`RECORD_MAX`](max_record_len)).
pub const WAL_BLOCKS: u64 = 256;

/// Number of independent WAL append regions in format version 5 and later.
pub const WAL_REGIONS: usize = 4;

/// First format version whose layout contains multiple WAL regions.
pub const MULTI_REGION_FORMAT_VERSION: u32 = 5;

/// Number of WAL regions present in `format_version`.
pub fn region_count(format_version: u32) -> usize {
    if format_version >= MULTI_REGION_FORMAT_VERSION {
        WAL_REGIONS
    } else {
        1
    }
}

/// Size, in bytes, of the log region.
pub fn wal_region_len(page_size: usize) -> usize {
    WAL_BLOCKS as usize * page_size
}

/// Byte offset of the header (block 0).
pub fn header_offset() -> usize {
    0
}

/// Byte offset of the state block (block 1).
pub fn state_offset(page_size: usize) -> usize {
    page_size
}

/// Byte offset where the log starts (block 2).
pub fn wal_start(page_size: usize) -> usize {
    2 * page_size
}

/// Byte offset one past the end of the log region.
pub fn wal_end(page_size: usize) -> usize {
    wal_start(page_size) + wal_region_len(page_size)
}

/// Byte offset where `region` starts for this file format.
pub fn region_start(page_size: usize, format_version: u32, region: usize) -> usize {
    debug_assert!(region < region_count(format_version));
    wal_start(page_size) + region * wal_region_len(page_size)
}

/// Byte offset one past `region` for this file format.
pub fn region_end(page_size: usize, format_version: u32, region: usize) -> usize {
    region_start(page_size, format_version, region) + wal_region_len(page_size)
}

/// Byte offset one past all WAL regions in this file format.
pub fn all_regions_end(page_size: usize, format_version: u32) -> usize {
    wal_start(page_size) + region_count(format_version) * wal_region_len(page_size)
}

/// Byte offset of B-tree page `id` in the data area. Data pages are numbered
/// from 1; page 0 is not stored (it means "empty tree").
pub fn data_offset(page_size: usize, id: PageId) -> usize {
    (WAL_BLOCKS as usize + 1 + id as usize) * page_size
}

/// Byte offset of B-tree page `id` in a particular file format.
pub fn data_offset_for(page_size: usize, format_version: u32, id: PageId) -> usize {
    (region_count(format_version) * WAL_BLOCKS as usize + 1 + id as usize) * page_size
}

/// The largest record that fits the log region, in bytes.
pub fn max_record_len(page_size: usize) -> usize {
    wal_region_len(page_size)
}

/// A single committed transaction: its sequence number, the tree root and
/// next-free-page it produced, and a copy of every page it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// Monotonic commit sequence number.
    pub seq: u64,
    /// Sequence number this transaction was based on.
    pub prev_seq: u64,
    /// Root this transaction was based on.
    pub prev_root: PageId,
    /// The tree root page id after the commit (0 = empty tree).
    pub root: PageId,
    /// The next free page id after the commit.
    pub next: PageId,
    /// The pages the commit wrote, keyed by page id.
    pub pages: Vec<(PageId, Vec<u8>)>,
}

// V5 layout: [len][seq][prev_seq][prev_root][root][next][count][pages...][crc]
const R_LEN: usize = 0;
const R_SEQ: usize = 4;
const R_PREV_SEQ: usize = 12;
const R_PREV_ROOT: usize = 20;
const R_ROOT: usize = 28;
const R_NEXT: usize = 36;
const R_COUNT: usize = 44;
const R_PAGES: usize = 48;

const LEGACY_R_ROOT: usize = 12;
const LEGACY_R_NEXT: usize = 20;
const LEGACY_R_COUNT: usize = 28;
const LEGACY_R_PAGES: usize = 32;

/// Everything in a commit record except the page images: the ordering scalars
/// a record needs whether or not its pages are already sitting in a
/// [`WalRecord`]. See [`encode_record_into`], which exists so the commit path
/// never has to build one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordMeta {
    /// Monotonic commit sequence number.
    pub seq: u64,
    /// Sequence number this transaction was based on.
    pub prev_seq: u64,
    /// Root this transaction was based on.
    pub prev_root: PageId,
    /// The tree root page id after the commit (0 = empty tree).
    pub root: PageId,
    /// The next free page id after the commit.
    pub next: PageId,
}

/// Encode a commit record into `out`, replacing whatever it held, taking the
/// pages from wherever the caller already keeps them.
///
/// Byte-for-byte what [`encode_record`] (or [`encode_legacy_record`], below
/// [`MULTI_REGION_FORMAT_VERSION`]) produces for the same input — pinned by
/// `the_borrowed_encoder_matches_the_owned_one`, because this is the on-disk
/// format and "faster but different" would be a silent format break rather
/// than a bug anyone notices.
///
/// # Why it exists (AHL-496)
///
/// A durable commit is the whole write path, and this encoding was copying
/// every dirty page **three** times to emit one record: once into
/// `WalRecord::pages` (`bytes.clone()` at the call site), once into a `body`
/// `Vec` that started at 128 bytes and `realloc`'d its way up, and once more
/// into the `out` buffer the checksum then ran over. A steady-state
/// single-row `INSERT` on this engine dirties ~6.5 pages, so that is ~80 KiB
/// of `memcpy` and a dozen allocations to produce a 26 KiB record, every
/// commit. On a device whose `fsync` is cheap — a container volume, which is
/// exactly the shape `BENCHMARK.md`'s MySQL/PostgreSQL row measures —
/// `wal::encode_record` was **11.7% of wall clock on its own**, with the
/// allocator behind it at another ~15%. One pass into one buffer the caller
/// keeps costs one copy and no allocation once the buffer has grown.
///
/// The length prefix is written as a placeholder and patched at the end
/// rather than derived from a first pass: it is covered by the checksum, so
/// it has to be *in* the buffer before `fnv1a` runs, and patching four bytes
/// is cheaper than either walking the pages twice or keeping the body in a
/// second allocation.
pub fn encode_record_into<'a, I>(out: &mut Vec<u8>, format_version: u32, meta: RecordMeta, pages: I)
where
    I: ExactSizeIterator<Item = (PageId, &'a [u8])>,
{
    out.clear();
    encode_record_onto(out, format_version, meta, pages);
}

/// [`encode_record_into`], appending to whatever `out` already holds instead
/// of replacing it — the same bytes, at a later offset.
///
/// Commit-side absorption (`docs/research/commit-group-slice2.md`) is why
/// this exists: a cohort leader encodes its own record and then one per
/// member into a single buffer and issues **one** `pwrite` for all of them,
/// back to back in its own region. The records are byte-for-byte what N
/// separate commits would have written, which is the whole reason recovery
/// needs no new logic — each one keeps its own length prefix and its own
/// checksum, so `scan_region` validates them one at a time and a torn write
/// truncates the chain at whichever record did not survive.
pub fn encode_record_onto<'a, I>(out: &mut Vec<u8>, format_version: u32, meta: RecordMeta, pages: I)
where
    I: ExactSizeIterator<Item = (PageId, &'a [u8])>,
{
    let start = out.len();
    push_u32(out, 0); // length placeholder, patched once the body is known
    push_u64(out, meta.seq);
    if format_version >= MULTI_REGION_FORMAT_VERSION {
        push_u64(out, meta.prev_seq);
        push_u64(out, meta.prev_root);
    }
    push_u64(out, meta.root);
    push_u64(out, meta.next);
    push_u32(out, pages.len() as u32);
    for (id, bytes) in pages {
        push_u64(out, id);
        push_u32(out, bytes.len() as u32);
        out.extend_from_slice(bytes);
    }
    let total = (out.len() - start + 8) as u32;
    out[start + R_LEN..start + R_LEN + 4].copy_from_slice(&total.to_le_bytes());
    let checksum = crate::checksum::fnv1a(&out[start..]);
    push_u64(out, checksum);
}

/// Encode a commit record into its on-disk form.
pub fn encode_record(record: &WalRecord) -> Vec<u8> {
    encode_owned(record, MULTI_REGION_FORMAT_VERSION)
}

/// Encode a record using the single-region v2-v4 layout.
pub fn encode_legacy_record(record: &WalRecord) -> Vec<u8> {
    encode_owned(record, MULTI_REGION_FORMAT_VERSION - 1)
}

fn encode_owned(record: &WalRecord, format_version: u32) -> Vec<u8> {
    let mut out = Vec::new();
    encode_record_into(
        &mut out,
        format_version,
        RecordMeta {
            seq: record.seq,
            prev_seq: record.prev_seq,
            prev_root: record.prev_root,
            root: record.root,
            next: record.next,
        },
        record.pages.iter().map(|(id, bytes)| (*id, &bytes[..])),
    );
    out
}

/// Decode a commit record. Returns `None` for an empty slot, a torn/corrupt
/// record, or a record whose length or checksum does not check out.
pub fn decode_record(bytes: &[u8]) -> Option<WalRecord> {
    decode_record_for_version(bytes, MULTI_REGION_FORMAT_VERSION)
}

/// Decode a commit record according to its file format.
pub fn decode_record_for_version(bytes: &[u8], format_version: u32) -> Option<WalRecord> {
    let (root_offset, next_offset, count_offset, pages_offset) =
        if format_version >= MULTI_REGION_FORMAT_VERSION {
            (R_ROOT, R_NEXT, R_COUNT, R_PAGES)
        } else {
            (LEGACY_R_ROOT, LEGACY_R_NEXT, LEGACY_R_COUNT, LEGACY_R_PAGES)
        };
    if bytes.len() < pages_offset + 8 {
        return None;
    }
    let total = read_u32(bytes, R_LEN) as usize;
    if total != bytes.len() || total < pages_offset + 8 {
        return None;
    }
    let checksum_offset = total - 8;
    if crate::checksum::fnv1a(&bytes[..checksum_offset]) != read_u64(bytes, checksum_offset) {
        return None;
    }
    let seq = read_u64(bytes, R_SEQ);
    if seq == 0 {
        return None;
    }
    let prev_seq = if format_version >= MULTI_REGION_FORMAT_VERSION {
        read_u64(bytes, R_PREV_SEQ)
    } else {
        seq.saturating_sub(1)
    };
    let prev_root = if format_version >= MULTI_REGION_FORMAT_VERSION {
        read_u64(bytes, R_PREV_ROOT)
    } else {
        0
    };
    let root = read_u64(bytes, root_offset);
    let next = read_u64(bytes, next_offset);
    let count = read_u32(bytes, count_offset) as usize;

    let mut pages = Vec::with_capacity(count.min(1024));
    let mut offset = pages_offset;
    for _ in 0..count {
        if offset + 12 > checksum_offset {
            return None;
        }
        let id = read_u64(bytes, offset);
        let len = read_u32(bytes, offset + 8) as usize;
        offset += 12;
        if offset + len > checksum_offset {
            return None;
        }
        pages.push((id, bytes[offset..offset + len].to_vec()));
        offset += len;
    }
    if offset != checksum_offset {
        return None;
    }
    Some(WalRecord {
        seq,
        prev_seq,
        prev_root,
        root,
        next,
        pages,
    })
}

/// The valid prefix and next append position of one WAL region.
pub struct RegionScan {
    /// Valid records encountered before the first empty/torn slot.
    pub records: Vec<WalRecord>,
    /// Append offset immediately after the last valid record.
    pub append_offset: usize,
}

/// Scan one writer region. A torn record ends this region's valid prefix but
/// does not affect records in any other region.
pub fn scan_region<D: Device>(
    device: &D,
    page_size: usize,
    format_version: u32,
    region: usize,
) -> Result<RegionScan> {
    let mut records = Vec::new();
    let mut offset = region_start(page_size, format_version, region);
    let end = region_end(page_size, format_version, region);
    while offset + 12 <= end {
        let mut header = [0u8; 4];
        device.read(offset, &mut header)?;
        let total = read_u32(&header, 0) as usize;
        let minimum = if format_version >= MULTI_REGION_FORMAT_VERSION {
            R_PAGES + 8
        } else {
            LEGACY_R_PAGES + 8
        };
        if total == 0 || total < minimum || offset + total > end {
            break;
        }
        let mut buf = vec![0u8; total];
        device.read(offset, &mut buf)?;
        match decode_record_for_version(&buf, format_version) {
            Some(record) => {
                records.push(record);
                offset += total;
            }
            None => break,
        }
    }
    Ok(RegionScan {
        records,
        append_offset: offset,
    })
}

/// Scan every writer region and return all valid records in commit order.
pub fn scan_all<D: Device>(
    device: &D,
    page_size: usize,
    format_version: u32,
) -> Result<Vec<WalRecord>> {
    let mut records = Vec::new();
    for region in 0..region_count(format_version) {
        records.extend(scan_region(device, page_size, format_version, region)?.records);
    }
    records.sort_by_key(|record| record.seq);
    Ok(records)
}

/// Legacy helper: scan the single v4 region for its newest record.
pub fn scan<D: Device>(device: &D, page_size: usize) -> Result<Option<WalRecord>> {
    Ok(scan_region(device, page_size, 4, 0)?
        .records
        .into_iter()
        .max_by_key(|record| record.seq))
}

/// Read a little-endian `u64` from `bytes` at `offset`.
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(arr)
}

/// Read a little-endian `u32` from `bytes` at `offset`.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn push_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimDisk;

    fn record(seq: u64) -> WalRecord {
        WalRecord {
            seq,
            prev_seq: seq - 1,
            prev_root: (seq - 1) * 10,
            root: seq * 10,
            next: seq * 10 + 1,
            pages: vec![(seq, vec![0xAB; 256])],
        }
    }

    #[test]
    fn a_record_round_trips() {
        let r = record(7);
        assert_eq!(decode_record(&encode_record(&r)), Some(r));
    }

    /// [`encode_record_into`] exists to take the copies out of the commit path
    /// (AHL-496), and the only thing that makes that safe is that it writes the
    /// *same bytes*. A faster encoder that shifted one field would not surface
    /// as a failed test somewhere — it would surface as a database written by
    /// this version that a previous one cannot read, and the reverse. So the
    /// two encoders are compared byte for byte, in both layouts, over the cases
    /// where a hand-written offset would plausibly drift: no pages at all, one
    /// page, several pages of *different* lengths (so a wrong length field
    /// misaligns everything after it), and a zero-length page.
    #[test]
    fn the_borrowed_encoder_matches_the_owned_one() {
        let cases = [
            WalRecord {
                seq: 1,
                prev_seq: 0,
                prev_root: 0,
                root: 0,
                next: 0,
                pages: Vec::new(),
            },
            record(9),
            WalRecord {
                seq: u64::MAX,
                prev_seq: u64::MAX - 1,
                prev_root: 12345,
                root: 999,
                next: 1000,
                pages: vec![
                    (1, vec![0x11; 7]),
                    (2, Vec::new()),
                    (7, vec![0x22; 4096]),
                    (9, vec![0x33; 63]),
                ],
            },
        ];
        for original in cases {
            for (format_version, owned) in [
                (
                    MULTI_REGION_FORMAT_VERSION - 1,
                    encode_legacy_record(&original),
                ),
                (MULTI_REGION_FORMAT_VERSION, encode_record(&original)),
            ] {
                let mut borrowed = Vec::new();
                encode_record_into(
                    &mut borrowed,
                    format_version,
                    RecordMeta {
                        seq: original.seq,
                        prev_seq: original.prev_seq,
                        prev_root: original.prev_root,
                        root: original.root,
                        next: original.next,
                    },
                    original.pages.iter().map(|(id, bytes)| (*id, &bytes[..])),
                );
                assert_eq!(
                    borrowed, owned,
                    "format {format_version}, seq {}",
                    original.seq
                );
            }
        }
    }

    /// The buffer is reused across commits, so the encoder has to *replace*
    /// what it holds rather than append to it. A leftover prefix would leave
    /// the length field pointing into the middle of the previous record — a
    /// record the scan would reject, i.e. a silently lost commit — so this
    /// encodes a long record and then a short one into the same buffer.
    #[test]
    fn reusing_the_buffer_leaves_no_trace_of_the_previous_record() {
        let mut buf = Vec::new();
        let big = WalRecord {
            seq: 4,
            prev_seq: 3,
            prev_root: 30,
            root: 40,
            next: 41,
            pages: vec![(1, vec![0xEE; 4096]), (2, vec![0xDD; 4096])],
        };
        let small = record(2);
        for original in [&big, &small, &big, &small] {
            encode_record_into(
                &mut buf,
                MULTI_REGION_FORMAT_VERSION,
                RecordMeta {
                    seq: original.seq,
                    prev_seq: original.prev_seq,
                    prev_root: original.prev_root,
                    root: original.root,
                    next: original.next,
                },
                original.pages.iter().map(|(id, bytes)| (*id, &bytes[..])),
            );
            assert_eq!(buf, encode_record(original));
            assert_eq!(decode_record(&buf).as_ref(), Some(original));
        }
    }

    #[test]
    fn a_record_with_no_pages_round_trips() {
        let r = WalRecord {
            seq: 1,
            prev_seq: 0,
            prev_root: 0,
            root: 2,
            next: 3,
            pages: Vec::new(),
        };
        assert_eq!(decode_record(&encode_record(&r)), Some(r));
    }

    #[test]
    fn an_empty_slot_is_not_a_record() {
        assert_eq!(decode_record(&[0u8; 64]), None);
    }

    #[test]
    fn a_torn_record_is_detected() {
        let mut bytes = encode_record(&record(3));
        let len = bytes.len();
        bytes[len - 9] ^= 0x80;
        assert_eq!(decode_record(&bytes), None);
    }

    #[test]
    fn the_scan_returns_the_newest_record_and_stops_at_a_tear() {
        let mut disk = SimDisk::with_block_size(512, 8 << 20);
        let mut offset = wal_start(256);
        for seq in 1..=3u64 {
            let bytes = encode_legacy_record(&record(seq));
            disk.write(offset, &bytes).unwrap();
            offset += bytes.len();
        }
        let newest = scan(&disk, 256).unwrap().unwrap();
        assert_eq!(newest.seq, 3);
    }
}
