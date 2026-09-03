//! A small, explicit binary encoding for rows and catalog entries.
//!
//! The format is hand-rolled on purpose: it is fully specified here, has no
//! dependency that could change its output between versions, and encodes
//! identical input to identical bytes — which is what the deterministic
//! simulation tests compare.
//!
//! ```text
//! row    := u32 count, value*
//! value  := u8 tag, payload
//! tag 0  := NULL     (no payload)
//! tag 1  := INTEGER  (i64 little-endian)
//! tag 2  := REAL     (f64 bits little-endian)
//! tag 3  := TEXT     (u32 byte length, UTF-8 bytes)
//! tag 4  := BLOB     (u32 byte length, bytes)
//! tag 5  := VECTOR   (u32 dimension, f32 bits little-endian * dimension)
//! tag 6  := VECTOR_Q8 (u32 dimension, f32 scale, i8 * dimension)
//! ```

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::{Deref, Range};

use crate::error::{Error, Result};
use crate::quantize::Q8Vector;
use crate::value::{DataType, Text, Value, ValueRef};

/// A row's encoded bytes, shared out of the page cache rather than copied.
///
/// `AHL-478`'s starting point: `CowBTree::resolve_value_at` used to clone the
/// row bytes out of the cached, `Rc`-held page on *every* read — a `Vec<u8>`
/// copy paid for a row that might be filtered out a moment later. A page's
/// inline cell bytes now live behind an `Arc<[u8]>`
/// (`crate::btree::page::ValueRef::Inline`), so a cache **hit** clones this
/// instead: an `Arc` refcount bump, not a byte copy. Only the two cases that
/// were never zero-copy to begin with still allocate — a value spread across
/// an overflow chain has to be reassembled, and a value read out of an open
/// transaction's own uncommitted writes is a fresh insert, not a cached page.
///
/// This is the whole of the fix for the site `PERF.md` names as "untouched
/// and now largest": nothing downstream changed shape, because every consumer
/// reads through [`RowBuf::as_slice`] (or the `Deref` it backs), the same as
/// it read a `&[u8]` before.
#[derive(Debug, Clone)]
pub enum RowBuf {
    /// Bytes cloned or assembled outside the page cache: an overflow chain,
    /// or a row written by the open transaction and not yet committed.
    Owned(Vec<u8>),
    /// A byte range into a page's shared buffer, held alive by the `Arc` —
    /// since AHL-536 the device's own buffer, when the device has one.
    Shared {
        /// The page's shared buffer the row's bytes are a slice of.
        bytes: Arc<[u8]>,
        /// The row's byte range inside `bytes`.
        range: Range<usize>,
    },
}

impl RowBuf {
    /// Borrow the encoded bytes, whichever variant holds them.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            RowBuf::Owned(bytes) => bytes,
            RowBuf::Shared { bytes, range } => &bytes[range.clone()],
        }
    }

    /// Take ownership of the bytes, copying only when they were shared.
    ///
    /// For the write paths and anything crossing the public API: a caller
    /// that needs a plain, uniquely-owned `Vec<u8>` (to mutate, to store
    /// somewhere with its own lifetime, or to hand back through
    /// [`crate::Value`]) pays exactly the copy it would have paid before this
    /// type existed — nothing regresses, and a read that never reaches this
    /// method never pays it at all.
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            RowBuf::Owned(bytes) => bytes,
            RowBuf::Shared { bytes, range } => bytes[range].to_vec(),
        }
    }
}

impl Deref for RowBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for RowBuf {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<Vec<u8>> for RowBuf {
    fn from(bytes: Vec<u8>) -> Self {
        RowBuf::Owned(bytes)
    }
}

impl From<Arc<[u8]>> for RowBuf {
    fn from(bytes: Arc<[u8]>) -> Self {
        let range = 0..bytes.len();
        RowBuf::Shared { bytes, range }
    }
}

impl PartialEq for RowBuf {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for RowBuf {}

impl PartialEq<Vec<u8>> for RowBuf {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl PartialEq<[u8]> for RowBuf {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_slice() == other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for RowBuf {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.as_slice() == other.as_slice()
    }
}

const TAG_NULL: u8 = 0;
const TAG_INTEGER: u8 = 1;
const TAG_REAL: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BLOB: u8 = 4;
const TAG_VECTOR: u8 = 5;
const TAG_VECTOR_Q8: u8 = 6;

/// Encode a row into its byte representation.
pub fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, values.len() as u32);
    for value in values {
        encode_value(&mut out, value);
    }
    out
}

/// Encode a row using the storage representation declared by each column.
///
/// Exact columns keep the byte layout written by older builds. Only a
/// `VECTOR(n, INT8)` column gets the new tag and payload.
pub(crate) fn encode_typed_row(values: &[Value], types: &[DataType]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_typed_row_into(&mut out, values, types);
    out
}

/// [`encode_typed_row`] into a buffer the caller owns, so a statement writing
/// many rows can keep one allocation instead of growing a fresh `Vec` from
/// empty for every row. `out` is cleared first; its capacity is what the
/// caller is reusing.
pub(crate) fn encode_typed_row_into(out: &mut Vec<u8>, values: &[Value], types: &[DataType]) {
    out.clear();
    put_u32(out, values.len() as u32);
    for (ordinal, value) in values.iter().enumerate() {
        if matches!(types.get(ordinal), Some(DataType::QuantizedVector(_))) {
            if let Value::Vector(vector) = value {
                encode_q8_vector(out, vector);
                continue;
            }
        }
        encode_value(out, value);
    }
}

fn encode_q8_vector(out: &mut Vec<u8>, vector: &[f32]) {
    let quantized = Q8Vector::from_f32(vector);
    out.push(TAG_VECTOR_Q8);
    put_u32(out, quantized.values.len() as u32);
    out.extend_from_slice(&quantized.scale.to_le_bytes());
    out.extend(quantized.values.iter().map(|value| *value as u8));
}

/// Decode a row previously produced by [`encode_row`].
pub fn decode_row(bytes: &[u8]) -> Result<Vec<Value>> {
    decode_row_masked(bytes, &ColumnMask::ALL)
}

/// How many ordinals fit in a mask without touching the heap. A table wider
/// than this is possible but rare enough that the spill is the cold path.
const INLINE_COLUMNS: usize = 128;

/// Which columns of a row a statement can actually observe.
///
/// The row format is a tag walk with no column directory, so "decode column 7"
/// is still `O(7)` — but walking past a column costs a length read and a cursor
/// bump, where decoding it costs a `String` or a `Vec` on the heap. That is the
/// whole of the win: `SELECT body FROM kv WHERE id = ?` stops allocating for
/// every column it never looks at. Adding a directory would make the walk `O(1)`
/// as well and is deliberately *not* done here — it is a storage-format change
/// (`docs/architecture.md`, decision D5) and the profile does not justify it yet.
///
/// [`ColumnMask::ALL`] is the safe answer and the one every caller that has not
/// proved otherwise should use: a column left out of the mask decodes as
/// `Value::Null`, so a mask that is too narrow is a wrong answer, not a slow
/// one. [`crate::engine`] builds masks from the plan and falls back to `ALL`
/// whenever it cannot enumerate what a statement reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMask {
    /// `true` when every column is wanted, whatever the bits say.
    everything: bool,
    /// How many ordinals the mask covers. Ordinals past it are wanted, so that
    /// a row wider than the mask is never silently truncated to nulls.
    width: usize,
    /// Wanted-ness of the first [`INLINE_COLUMNS`] ordinals, one bit each.
    ///
    /// A bitmap rather than a `Vec<bool>` because a mask is built once per
    /// statement and thrown away, and on a primary-key point read that
    /// `alloc::vec![false; width]` (plus the one [`ColumnMask::slice`] makes)
    /// was two mallocs and two frees for two columns' worth of information
    /// (`PERF.md`, AHL-527).
    inline: u128,
    /// Ordinals from [`INLINE_COLUMNS`] up. Empty for any narrower table,
    /// which is the only reason the inline word is worth having.
    spilled: Vec<bool>,
}

impl ColumnMask {
    /// Decode every column. The behaviour the engine had before masks existed.
    pub const ALL: Self = Self {
        everything: true,
        width: 0,
        inline: 0,
        spilled: Vec::new(),
    };

    /// A mask over `width` columns that wants none of them yet.
    pub fn none(width: usize) -> Self {
        Self {
            everything: false,
            width,
            inline: 0,
            spilled: match width.checked_sub(INLINE_COLUMNS) {
                Some(over) => alloc::vec![false; over],
                None => Vec::new(),
            },
        }
    }

    /// Mark one column as read.
    ///
    /// An ordinal past the mask's width widens it to everything rather than
    /// being dropped: an out-of-range reference means the caller's idea of the
    /// row's shape and this mask's disagree, and the only safe reading of that
    /// is "decode it all".
    pub fn add(&mut self, ordinal: usize) {
        if self.everything {
            return;
        }
        if ordinal >= self.width {
            self.widen();
            return;
        }
        match ordinal.checked_sub(INLINE_COLUMNS) {
            Some(over) => self.spilled[over] = true,
            None => self.inline |= 1u128 << ordinal,
        }
    }

    /// Give up on narrowing and decode every column.
    ///
    /// Leaves the mask byte-identical to [`ColumnMask::ALL`], so equality
    /// between a widened mask and the constant still holds.
    pub fn widen(&mut self) {
        self.everything = true;
        self.width = 0;
        self.inline = 0;
        self.spilled = Vec::new();
    }

    /// Whether the mask is the trivial "everything" one.
    pub fn is_all(&self) -> bool {
        self.everything
    }

    /// Whether column `ordinal` has to be decoded.
    pub fn wants(&self, ordinal: usize) -> bool {
        if self.everything || ordinal >= self.width {
            return true;
        }
        match ordinal.checked_sub(INLINE_COLUMNS) {
            Some(over) => self.spilled[over],
            None => self.inline & (1u128 << ordinal) != 0,
        }
    }

    /// How many leading columns of a `count`-column row have to be walked
    /// before every wanted column has been reached.
    ///
    /// The row format has no column directory, so column *k* is reached by
    /// stepping over columns `0..k`; but nothing past the last wanted column
    /// has to be stepped over at all. For `SELECT COUNT(*), MIN(id), MAX(id)`
    /// over a four-column row that is one column walked instead of four. The
    /// answer is `count` — walk everything — whenever the mask cannot narrow:
    /// it wants everything, or the row is wider than the mask, in which case
    /// every ordinal past the mask's width is wanted (see [`ColumnMask::wants`]).
    pub fn walk_len(&self, count: usize) -> usize {
        if self.everything || count > self.width {
            return count;
        }
        if let Some(last) = self.spilled.iter().rposition(|wanted| *wanted) {
            return (INLINE_COLUMNS + last + 1).min(count);
        }
        let inline = u128::BITS - self.inline.leading_zeros();
        (inline as usize).min(count)
    }

    /// The mask covering ordinals `start..start + width` of a joined row,
    /// rebased so that ordinal `0` is the sub-row's first column.
    ///
    /// A joined row is the concatenation of its tables, so a plan's ordinals
    /// index into the concatenation while the bytes being decoded are one
    /// table's. This is the translation between the two.
    pub fn slice(&self, start: usize, width: usize) -> Self {
        if self.everything {
            return Self::ALL;
        }
        let mut out = Self::none(width);
        for ordinal in 0..width {
            if self.wants(start + ordinal) {
                out.add(ordinal);
            }
        }
        out
    }
}

/// Decode a row, materialising only the columns `mask` asks for.
///
/// The result is always the same *width* as [`decode_row`] would produce —
/// skipped columns are `Value::Null` — so every ordinal a plan holds still
/// lands on the column it named.
///
/// A skipped `TEXT` column is not checked for valid UTF-8, because it is never
/// turned into a `str`. Structural corruption is still caught: the length is
/// read and the cursor refuses to run past the end of the buffer, so a row that
/// cannot be walked is still an [`Error::Corrupt`].
pub fn decode_row_masked(bytes: &[u8], mask: &ColumnMask) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    decode_row_masked_onto(bytes, mask, &mut values)?;
    Ok(values)
}

/// [`decode_row_masked`] appending onto a buffer the caller already owns.
///
/// Identical cells, identical mask semantics, identical errors — the only
/// difference is that the cells are pushed onto the end of `out` instead of
/// into a fresh `Vec`. A join's pairing buffer already holds the outer row and
/// wants the inner row's cells after it, so decoding into a temporary `Vec`
/// only to move it across was one heap allocation per candidate pair
/// (`PERF.md`, AHL-549).
///
/// `out` is *not* cleared: appending is the whole point. A decode that fails
/// part-way leaves the cells read before the failure on the end of it, exactly
/// as [`decode_row_ref_masked_into`] does, and every caller stops at the first
/// error or truncates before the next row.
pub fn decode_row_masked_onto(bytes: &[u8], mask: &ColumnMask, out: &mut Vec<Value>) -> Result<()> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.count(1)?;
    out.reserve(count);
    for ordinal in 0..count {
        if mask.wants(ordinal) {
            out.push(decode_value(&mut cursor)?);
        } else {
            skip_value(&mut cursor)?;
            out.push(Value::Null);
        }
    }
    Ok(())
}

/// Decode a row into borrowed cells, materialising only the columns `mask`
/// asks for and allocating nothing for a `TEXT`/`BLOB` cell.
///
/// The borrowed counterpart of [`decode_row_masked`]: same mask semantics
/// (a skipped column decodes as [`ValueRef::Null`], a mask narrower than the
/// row is never silently truncated), same early-exit skip over columns the
/// mask does not want — the only difference is that a wanted `TEXT`/`BLOB`
/// cell borrows its bytes from `bytes` instead of copying them into a
/// `String`/`Vec<u8>`. This is what lets a scanned or probed row that a
/// filter goes on to reject be built, tested and dropped without allocating
/// (`PERF.md`, "the structural fix").
///
/// A `VECTOR`/`VECTOR(n, INT8)` cell still allocates a `Vec<f32>` — see
/// [`ValueRef`]'s type-level doc for why a zero-copy view is not available in
/// safe Rust here.
pub fn decode_row_ref_masked<'a>(bytes: &'a [u8], mask: &ColumnMask) -> Result<Vec<ValueRef<'a>>> {
    let mut values = Vec::new();
    decode_row_ref_masked_into(bytes, mask, &mut values)?;
    Ok(values)
}

/// [`decode_row_ref_masked`] into a buffer the caller already owns.
///
/// Identical cells, identical mask semantics, identical errors — the only
/// difference is where the `Vec` comes from. Borrowing the cells removed the
/// per-cell allocation for `TEXT`/`BLOB`; the container they are pushed into
/// was what remained, and a filter that rejects most of what it reads pays it
/// once per *candidate* row rather than once per surviving row. A caller that
/// decodes row after row can hand the same buffer back here instead, which is
/// what [`crate::exec`]'s fused decode-and-filter does.
///
/// `out` is cleared first, so a caller never sees the previous row's cells. A
/// decode that fails part-way leaves the cells read before the failure in it;
/// every caller either stops at the first error — the pipeline's contract, see
/// [`crate::exec`] — or clears it again on the next row, so those are never
/// read.
pub fn decode_row_ref_masked_into<'a>(
    bytes: &'a [u8],
    mask: &ColumnMask,
    out: &mut Vec<ValueRef<'a>>,
) -> Result<()> {
    out.clear();
    decode_row_ref_masked_onto(bytes, mask, out)
}

/// [`decode_row_ref_masked_into`] that appends rather than replacing.
///
/// The borrowed twin of [`decode_row_masked_onto`], and for the same caller: a
/// join pairing an outer row against one probed inner row after another writes
/// the inner cells onto the end of a buffer whose first `outer_width` cells are
/// the outer row's, truncating back between candidates. Nothing is cleared
/// here, so the outer half survives.
pub fn decode_row_ref_masked_onto<'a>(
    bytes: &'a [u8],
    mask: &ColumnMask,
    out: &mut Vec<ValueRef<'a>>,
) -> Result<()> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.count(1)?;
    out.reserve(count);
    for ordinal in 0..count {
        if mask.wants(ordinal) {
            out.push(decode_value_ref(&mut cursor)?);
        } else {
            skip_value(&mut cursor)?;
            out.push(ValueRef::Null);
        }
    }
    Ok(())
}

/// [`decode_row_ref_masked_into`] that stops walking after the last wanted
/// column.
///
/// Same cells, same width, same mask semantics: a column the mask does not
/// want is [`ValueRef::Null`], the row is never truncated, and a row wider
/// than the mask decodes in full. The one difference is the contract on a
/// structurally corrupt column *after* the last wanted one — a `TEXT` whose
/// length runs past the row, say. [`decode_row_masked`] promises to catch it,
/// because it walks every column to the end; this does not walk them
/// ([`ColumnMask::walk_len`]) and so does not see them. The row's own column
/// count is still checked against its length, and every column up to the last
/// wanted one is walked under the same checks as before.
///
/// That is why this is only for a consumer that never hands the row on: the
/// streamed aggregate (`Engine::stream_aggregate`), which reads its group key
/// and its arguments from the cells and keeps at most one row per group. It
/// does not want `body` to fold `COUNT(*)`, and stepping over three columns
/// to reach the end of a row it is about to discard was measured at
/// `skip_value` 5.3% self of the aggregate profile (`PERF.md`, AHL-538). A
/// path that returns rows keeps [`decode_row_ref_masked_into`] and its
/// promise.
pub fn decode_row_ref_wanted_into<'a>(
    bytes: &'a [u8],
    mask: &ColumnMask,
    out: &mut Vec<ValueRef<'a>>,
) -> Result<()> {
    out.clear();
    let mut cursor = Cursor::new(bytes);
    let count = cursor.count(1)?;
    out.reserve(count);
    let walk = mask.walk_len(count);
    for ordinal in 0..walk {
        if mask.wants(ordinal) {
            out.push(decode_value_ref(&mut cursor)?);
        } else {
            skip_value(&mut cursor)?;
            out.push(ValueRef::Null);
        }
    }
    for _ in walk..count {
        out.push(ValueRef::Null);
    }
    Ok(())
}

/// Decode exactly one column of a row, skipping every other column.
///
/// The join fast path needs one column per outer row — the hash key — and
/// never the rest, so a full [`decode_row_masked`] would allocate a `Vec<Value>`
/// per row only to read a single cell out of it. This walks the tag format to
/// `ordinal` (still `O(ordinal)`, the same as [`decode_row_masked`], since the
/// format has no column directory — `docs/architecture.md` D5) but allocates no
/// container, and decodes no column it does not return.
pub fn decode_value_at(bytes: &[u8], ordinal: usize) -> Result<Value> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.count(1)?;
    if ordinal >= count {
        return Err(Error::Corrupt(
            "column ordinal past the end of the row".to_string(),
        ));
    }
    for _ in 0..ordinal {
        skip_value(&mut cursor)?;
    }
    decode_value(&mut cursor)
}

fn decode_value_ref<'a>(cursor: &mut Cursor<'a>) -> Result<ValueRef<'a>> {
    let tag = cursor.u8()?;
    match tag {
        TAG_NULL => Ok(ValueRef::Null),
        TAG_INTEGER => Ok(ValueRef::Integer(i64::from_le_bytes(cursor.array8()?))),
        TAG_REAL => Ok(ValueRef::Real(f64::from_bits(u64::from_le_bytes(
            cursor.array8()?,
        )))),
        TAG_TEXT => {
            let len = cursor.u32()? as usize;
            let bytes = cursor.take(len)?;
            let text = core::str::from_utf8(bytes)
                .map_err(|_| Error::Corrupt("text column is not valid UTF-8".to_string()))?;
            Ok(ValueRef::Text(text))
        }
        TAG_BLOB => {
            let len = cursor.u32()? as usize;
            Ok(ValueRef::Blob(cursor.take(len)?))
        }
        TAG_VECTOR => {
            let dim = cursor.count(4)?;
            let mut v = Vec::with_capacity(dim);
            for _ in 0..dim {
                v.push(f32::from_bits(u32::from_le_bytes(cursor.array4()?)));
            }
            Ok(ValueRef::Vector(v))
        }
        TAG_VECTOR_Q8 => {
            let dim = cursor.count(1)?;
            let scale = f32::from_le_bytes(cursor.array4()?);
            let values = cursor.take(dim)?.iter().map(|value| *value as i8).collect();
            Ok(ValueRef::Vector(Q8Vector { scale, values }.to_f32()))
        }
        other => Err(Error::Corrupt(alloc::format!("unknown value tag {other}"))),
    }
}

fn encode_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Integer(i) => {
            out.push(TAG_INTEGER);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Real(r) => {
            out.push(TAG_REAL);
            out.extend_from_slice(&r.to_bits().to_le_bytes());
        }
        Value::Text(s) => {
            out.push(TAG_TEXT);
            put_u32(out, s.len() as u32);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Blob(b) => {
            out.push(TAG_BLOB);
            put_u32(out, b.len() as u32);
            out.extend_from_slice(b);
        }
        Value::Vector(v) => {
            out.push(TAG_VECTOR);
            put_u32(out, v.len() as u32);
            for f in v {
                out.extend_from_slice(&f.to_bits().to_le_bytes());
            }
        }
    }
}

fn decode_value(cursor: &mut Cursor<'_>) -> Result<Value> {
    let tag = cursor.u8()?;
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_INTEGER => Ok(Value::Integer(i64::from_le_bytes(cursor.array8()?))),
        TAG_REAL => Ok(Value::Real(f64::from_bits(u64::from_le_bytes(
            cursor.array8()?,
        )))),
        TAG_TEXT => {
            let len = cursor.u32()? as usize;
            let bytes = cursor.take(len)?;
            let text = core::str::from_utf8(bytes)
                .map_err(|_| Error::Corrupt("text column is not valid UTF-8".to_string()))?;
            // Straight from the borrowed `str` into the `Arc<str>`: one
            // allocation and one copy. Going through `String` first allocated
            // and copied twice, because `Text::from(String)` reads the bytes
            // back out of the `String` to build the `Arc` and then drops it.
            Ok(Value::Text(Text::from(text)))
        }
        TAG_BLOB => {
            let len = cursor.u32()? as usize;
            Ok(Value::Blob(cursor.take(len)?.to_vec()))
        }
        TAG_VECTOR => {
            let dim = cursor.count(4)?;
            let mut v = Vec::with_capacity(dim);
            for _ in 0..dim {
                v.push(f32::from_bits(u32::from_le_bytes(cursor.array4()?)));
            }
            Ok(Value::Vector(v))
        }
        TAG_VECTOR_Q8 => {
            let dim = cursor.count(1)?;
            let scale = f32::from_le_bytes(cursor.array4()?);
            let values = cursor.take(dim)?.iter().map(|value| *value as i8).collect();
            Ok(Value::Vector(Q8Vector { scale, values }.to_f32()))
        }
        other => Err(Error::Corrupt(alloc::format!("unknown value tag {other}"))),
    }
}

/// Step the cursor past one value without materialising it.
///
/// The tag says how long the payload is, which is what makes an early-exit
/// decode possible at all in a format with no column directory. Every arm here
/// has to agree with the matching arm of [`decode_value`]; the round-trip test
/// below walks a row of every type twice and compares.
fn skip_value(cursor: &mut Cursor<'_>) -> Result<()> {
    let tag = cursor.u8()?;
    let len = match tag {
        TAG_NULL => 0,
        TAG_INTEGER | TAG_REAL => 8,
        TAG_TEXT | TAG_BLOB => cursor.u32()? as usize,
        // A dimension, then four bytes per component.
        TAG_VECTOR => cursor.count(4)?.saturating_mul(4),
        // A dimension, then a f32 scale, then one byte per component.
        TAG_VECTOR_Q8 => {
            let dim = cursor.count(1)?;
            dim.saturating_add(4)
        }
        other => return Err(Error::Corrupt(alloc::format!("unknown value tag {other}"))),
    };
    cursor.take(len)?;
    Ok(())
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

/// Reads primitives out of a byte slice, refusing to run past the end.
pub(crate) struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| Error::Corrupt("length overflow".to_string()))?;
        if end > self.bytes.len() {
            return Err(Error::Corrupt("unexpected end of buffer".to_string()));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Bytes left to read.
    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    /// Read a count that will be used to size an allocation.
    ///
    /// A decoder that does `Vec::with_capacity(cursor.u32()?)` will happily try
    /// to reserve gigabytes for a four-byte file — the coverage-guided fuzzer
    /// found exactly that, and on any host with a memory limit it is an abort
    /// rather than an error. Since every element costs at least one byte,
    /// **a count larger than the bytes remaining is provably corrupt**, so it
    /// can be rejected before anything is allocated.
    ///
    /// `bytes_per_item` is the smallest number of bytes one element can
    /// occupy; pass 1 when an element could in principle be a single byte.
    pub(crate) fn count(&mut self, bytes_per_item: usize) -> Result<usize> {
        let count = self.u32()? as usize;
        let smallest = count.saturating_mul(bytes_per_item.max(1));
        if smallest > self.remaining() {
            return Err(Error::Corrupt(alloc::format!(
                "declared {count} item(s) but only {} byte(s) remain",
                self.remaining()
            )));
        }
        Ok(count)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array4()?))
    }

    pub(crate) fn array4(&mut self) -> Result<[u8; 4]> {
        let slice = self.take(4)?;
        let mut buf = [0u8; 4];
        buf.copy_from_slice(slice);
        Ok(buf)
    }

    pub(crate) fn array8(&mut self) -> Result<[u8; 8]> {
        let slice = self.take(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(slice);
        Ok(buf)
    }

    pub(crate) fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| Error::Corrupt("string is not valid UTF-8".to_string()))
    }
}

/// Append a length-prefixed string. Shared with the catalog encoding.
pub(crate) fn put_string(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// Append a `u32`. Shared with the catalog encoding.
pub(crate) fn put_len(out: &mut Vec<u8>, n: usize) {
    put_u32(out, n as u32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn round_trips_every_value_kind() {
        let row = vec![
            Value::Null,
            Value::Integer(-42),
            Value::Real(1.5),
            Value::Text("héllo".to_string().into()),
            Value::Blob(vec![0, 1, 2, 255]),
            Value::Vector(vec![0.25, -0.5, 1.0]),
        ];
        let bytes = encode_row(&row);
        assert_eq!(decode_row(&bytes).unwrap(), row);
    }

    /// The property the whole projection pushdown rests on: skipping a column
    /// has to land the cursor exactly where decoding it would have. Every tag
    /// is exercised, in every position, so a `skip_value` arm that disagrees
    /// with its `decode_value` arm shows up as a wrong *later* column rather
    /// than as a decode error.
    #[test]
    fn skipping_a_column_lands_where_decoding_it_would_have() {
        let row = vec![
            Value::Null,
            Value::Integer(-42),
            Value::Real(1.5),
            Value::Text("héllo".to_string().into()),
            Value::Blob(vec![0, 1, 2, 255]),
            Value::Vector(vec![0.25, -0.5, 1.0]),
        ];
        let bytes = encode_row(&row);
        for skipped in 0..row.len() {
            let mut mask = ColumnMask::none(row.len());
            for ordinal in 0..row.len() {
                if ordinal != skipped {
                    mask.add(ordinal);
                }
            }
            let decoded = decode_row_masked(&bytes, &mask).unwrap();
            let mut expected = row.clone();
            expected[skipped] = Value::Null;
            assert_eq!(decoded, expected, "skipping column {skipped}");
        }
    }

    /// A quantised vector is a different tag with a different payload shape, so
    /// it gets its own skip test — against the encoding a `VECTOR(n, INT8)`
    /// column actually writes.
    #[test]
    fn a_quantised_vector_can_be_skipped_too() {
        let bytes = encode_typed_row(
            &[
                Value::Vector(vec![0.5, -0.25, 1.0]),
                Value::Integer(9),
                Value::Text("after".to_string().into()),
            ],
            &[DataType::QuantizedVector(3)],
        );
        let mut mask = ColumnMask::none(3);
        mask.add(1);
        mask.add(2);
        let decoded = decode_row_masked(&bytes, &mask).unwrap();
        assert_eq!(decoded[0], Value::Null);
        assert_eq!(decoded[1], Value::Integer(9));
        assert_eq!(decoded[2], Value::Text("after".to_string().into()));
    }

    /// A mask narrower than the stored row must not silently null the columns
    /// it does not know about — a row can be wider than the plan expects after
    /// an `ALTER TABLE`, and inventing `NULL`s there would be a wrong answer.
    #[test]
    fn columns_past_the_mask_are_decoded_rather_than_dropped() {
        let row = vec![
            Value::Integer(1),
            Value::Text("kept".to_string().into()),
            Value::Integer(3),
        ];
        let bytes = encode_row(&row);
        let mut mask = ColumnMask::none(1);
        mask.add(0);
        assert_eq!(decode_row_masked(&bytes, &mask).unwrap(), row);
    }

    /// The early-exit decode agrees with the full walk on every mask over a
    /// well-formed row: same cells, same width, same nulls — including a mask
    /// narrower than the row, where nothing may be skipped at all.
    #[test]
    fn the_wanted_decode_ties_the_masked_decode_on_every_mask() {
        let row = vec![
            Value::Integer(7),
            Value::Text("email".to_string().into()),
            Value::Blob(vec![1, 2, 3]),
            Value::Real(2.5),
            Value::Null,
            Value::Vector(vec![0.5, 1.0]),
        ];
        let bytes = encode_row(&row);
        for bits in 0..(1u32 << row.len()) {
            let mut mask = ColumnMask::none(row.len());
            for ordinal in 0..row.len() {
                if bits & (1 << ordinal) != 0 {
                    mask.add(ordinal);
                }
            }
            let mut full = Vec::new();
            decode_row_ref_masked_into(&bytes, &mask, &mut full).unwrap();
            let mut wanted = Vec::new();
            decode_row_ref_wanted_into(&bytes, &mask, &mut wanted).unwrap();
            assert_eq!(wanted, full, "mask bits {bits:#b}");
            assert_eq!(wanted.len(), row.len(), "mask bits {bits:#b}");
        }
        // Narrower than the row: every ordinal past the mask is wanted, so the
        // walk runs to the end and only the one unwanted ordinal inside the
        // mask is nulled.
        let mut narrow = ColumnMask::none(2);
        narrow.add(0);
        assert_eq!(narrow.walk_len(row.len()), row.len());
        let mut wanted = Vec::new();
        decode_row_ref_wanted_into(&bytes, &narrow, &mut wanted).unwrap();
        let owned: Vec<Value> = wanted.iter().map(ValueRef::to_owned_value).collect();
        let mut expected = row.clone();
        expected[1] = Value::Null;
        assert_eq!(owned, expected);
        // `ALL` walks everything; a mask wanting nothing walks nothing.
        assert_eq!(ColumnMask::ALL.walk_len(4), 4);
        assert_eq!(ColumnMask::none(4).walk_len(4), 0);
    }

    /// The contract the early exit trades, pinned both ways: a corrupt column
    /// *after* the last wanted one is caught by the full walk and is not seen
    /// by the wanted walk; a corrupt column *at or before* it is caught by
    /// both.
    #[test]
    fn a_corrupt_trailing_column_is_caught_by_the_full_walk_and_not_the_wanted_one() {
        let row = vec![
            Value::Integer(1),
            Value::Text("kept".to_string().into()),
            Value::Text("trailing".to_string().into()),
        ];
        let mut bytes = encode_row(&row);
        // The trailing TEXT's length, made to run past the row: count (4) +
        // integer (1 + 8) + text (1 + 4 + 4) puts its tag at 22 and its
        // length at 23.
        assert_eq!(bytes[22], TAG_TEXT);
        bytes[23..27].copy_from_slice(&u32::MAX.to_le_bytes());

        let mut mask = ColumnMask::none(3);
        mask.add(1);
        let mut cells = Vec::new();
        assert!(matches!(
            decode_row_ref_masked_into(&bytes, &mask, &mut cells),
            Err(Error::Corrupt(_))
        ));
        decode_row_ref_wanted_into(&bytes, &mask, &mut cells).unwrap();
        assert_eq!(
            cells,
            vec![ValueRef::Null, ValueRef::Text("kept"), ValueRef::Null]
        );

        // Wanted, or before a wanted column: both walks fail the same way.
        let mut mask = ColumnMask::none(3);
        mask.add(2);
        assert!(matches!(
            decode_row_ref_wanted_into(&bytes, &mask, &mut cells),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            decode_row_ref_masked_into(&bytes, &mask, &mut cells),
            Err(Error::Corrupt(_))
        ));
    }

    /// An out-of-range reference widens the mask instead of being dropped.
    #[test]
    fn an_out_of_range_ordinal_widens_the_mask() {
        let mut mask = ColumnMask::none(2);
        mask.add(5);
        assert!(mask.is_all());
        assert!(mask.wants(0));
    }

    /// A table wider than one inline word keeps working, and the seam between
    /// the inline bits and the spilled tail is where an off-by-one would live.
    #[test]
    fn a_mask_wider_than_its_inline_word_still_answers_per_ordinal() {
        let width = INLINE_COLUMNS * 2 + 3;
        let mut mask = ColumnMask::none(width);
        // The last inline ordinal, the first spilled one, and the far end.
        for ordinal in [0, INLINE_COLUMNS - 1, INLINE_COLUMNS, width - 1] {
            mask.add(ordinal);
        }
        assert!(!mask.is_all());
        for ordinal in 0..width {
            let wanted = matches!(ordinal, 0 | INLINE_COLUMNS)
                || ordinal == INLINE_COLUMNS - 1
                || ordinal == width - 1;
            assert_eq!(mask.wants(ordinal), wanted, "ordinal {ordinal}");
        }
        // Ordinals past the width are wanted, never silently null.
        assert!(mask.wants(width));
        // And a slice that straddles the seam rebases onto the inline word.
        let across = mask.slice(INLINE_COLUMNS - 1, 2);
        assert!(across.wants(0));
        assert!(across.wants(1));
        assert!(!across.is_all());
    }

    /// A joined row's ordinals are offsets into the concatenation, so the mask
    /// each table decodes under is a rebased slice of the whole.
    #[test]
    fn a_slice_of_a_mask_rebases_its_ordinals() {
        let mut mask = ColumnMask::none(5);
        mask.add(0);
        mask.add(3);
        let right = mask.slice(2, 3);
        assert!(!right.wants(0));
        assert!(right.wants(1));
        assert!(!right.wants(2));
        assert!(!mask.slice(2, 3).is_all());
        assert!(ColumnMask::ALL.slice(2, 3).is_all());
    }

    #[test]
    fn encoding_is_byte_stable() {
        let row = vec![Value::Integer(7), Value::Text("a".to_string().into())];
        assert_eq!(encode_row(&row), encode_row(&row.clone()));
    }

    #[test]
    fn truncated_input_is_rejected_not_panicked() {
        let bytes = encode_row(&[Value::Text("abc".to_string().into())]);
        let err = decode_row(&bytes[..bytes.len() - 2]).unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)));
    }

    #[test]
    fn quantized_vector_round_trips_with_four_times_less_payload() {
        let vector: Vec<f32> = (0..384).map(|i| i as f32 / 383.0 - 0.5).collect();
        let exact = encode_row(&[Value::Vector(vector.clone())]);
        let quantized = encode_typed_row(
            &[Value::Vector(vector.clone())],
            &[DataType::QuantizedVector(384)],
        );
        let decoded = decode_row(&quantized).unwrap();
        let Value::Vector(decoded) = &decoded[0] else {
            panic!("decoded value is not a vector");
        };
        let max_error = vector
            .iter()
            .zip(decoded)
            .map(|(before, after)| (before - after).abs())
            .fold(0.0f32, f32::max);
        assert!(max_error < 0.0021, "max error was {max_error}");
        assert_eq!(exact.len(), 4 + 1 + 4 + 384 * 4);
        assert_eq!(quantized.len(), 4 + 1 + 4 + 4 + 384);
        assert!(exact.len() * 100 > quantized.len() * 388);
    }
}
