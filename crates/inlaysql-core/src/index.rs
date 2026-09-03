//! Scalar secondary indexes: the key space they live in, and the
//! memcomparable encoding that makes `memcmp` on those keys mean the same
//! thing as comparing the values.
//!
//! An index entry is an ordinary row in the same copy-on-write tree as
//! everything else, under a reserved key prefix and with an empty value
//! (`docs/architecture.md`, decision **D3**). That is the whole point: the entries inherit
//! the write-ahead log, crash recovery, the MVCC rebase and the deterministic
//! simulation harness without a line of new storage code.
//!
//! ```text
//! \x01idx:<index name>\0 <encoded column values...> <row id, big-endian>
//! ```
//!
//! # Why the key names the *index* and not the table
//!
//! `docs/architecture.md`'s sketch is `idx:<table>:<index>`. It is keyed by index name alone
//! here, and the reason is `ALTER TABLE ... RENAME TO`: with the table name in
//! the key, a rename would have to rewrite every index entry in the table —
//! O(rows) work inside one transaction, in a statement that is O(1) today, and
//! one transaction has a hard size ceiling ([`Storage::transaction_is_nearly_full`](crate::Storage::transaction_is_nearly_full)),
//! so renaming a large table would simply fail. Index names are already unique
//! across the whole catalog ([`Catalog::create_index`](crate::Catalog)
//! rejects a duplicate, and a named `UNIQUE` constraint shares that namespace),
//! so the name alone is a sufficient key, and a rename becomes free again.
//!
//! # Why the prefix cannot collide
//!
//! `\x01` is the same trick the paged ANN index already uses for its graph
//! namespace: a row key begins with a table name and engine metadata keys begin
//! with `\0` ([`crate::storage::meta_key`]), so neither can produce a leading
//! `\x01`. The two `\x01` namespaces are then told apart by their tags —
//! `idx:` here, `ann:` there. `prefix_never_collides` in
//! [`crate::storage`]'s tests is the proof.
//!
//! # The encoding
//!
//! Each value contributes a one-byte class tag and then a payload that is
//! *self-delimiting*, so concatenating columns needs no separator and no length
//! field, and a multi-column index is the single-column one with more bytes.
//! The tags order the classes exactly as
//! [`eval::mem_cmp`](crate::eval) does: `NULL` < numbers < text < blobs.
//!
//! | class | tag | payload |
//! | --- | --- | --- |
//! | `NULL` | `0x01` | none |
//! | `INTEGER` / `REAL` | `0x02` | 8 bytes, the total-order transform of the value as `f64` |
//! | `TEXT` | `0x03` | the UTF-8 bytes, `\0` escaped as `\0\xff`, terminated by `\0\x01` |
//! | `BLOB` | `0x04` | the bytes, escaped and terminated the same way |
//!
//! **Integers and reals share one domain.** They are encoded as the
//! total-order transform of `value as f64`, so the integer `1` and the real
//! `1.0` produce identical bytes. That is not a shortcut, it is what
//! correctness requires: the engine's own comparison operator
//! (`eval::comparison`, which is what every `WHERE` goes through) compares
//! *all* numbers as `f64`, and `unique_key_collides` decides that the integer
//! `1` and the real `1.0` are the same unique key. An encoding that separated
//! them would make an index probe miss rows a scan finds, and would make a
//! `UNIQUE` index miss a duplicate the scan catches.
//!
//! The visible consequence is at the far end of the integer range: two
//! integers above 2^53 that round to the same `f64` encode to the same key.
//! They are then adjacent in the index in row-id order rather than in value
//! order. Nothing reads results out of index order — the executor re-applies
//! the `WHERE` filter to every candidate the index produces and sorts
//! afterwards — so this costs a few extra candidates and never an answer.
//! `eval::comparison` already treats those two integers as equal, so the index
//! and the filter agree.
//!
//! # `-0.0` and `NaN`
//!
//! **`-0.0` is canonicalised to `+0.0`.** IEEE's total order puts `-0.0` below
//! `+0.0`, but `-0.0 == 0.0` is true in every comparison this engine makes, so
//! keeping them apart would let `WHERE x = 0.0` miss a stored `-0.0`. One key
//! for one value.
//!
//! **`NaN` is refused, loudly.** [`encode_value`] returns
//! [`Error::Unsupported`] for it, so writing a `NaN` into an indexed column
//! fails rather than being indexed wrongly. This is not squeamishness: this
//! engine's `eval::comparison` falls back to `Ordering::Equal` when
//! `partial_cmp` returns `None`, which makes a stored `NaN` compare *equal* to
//! every number, under `=`, `<=` and `>=` alike. No ordered index can
//! reproduce that, so an index over a stored `NaN` would silently return
//! different rows than a scan. Refusing the write is the only answer that
//! cannot lie. (SQLite, the dialect baseline, does not store `NaN` at all — it
//! stores `NULL` — so nothing portable depends on it.) A `NaN` on the *probe*
//! side is handled by the planner, which declines to use an index for it.
//!
//! Infinities are ordinary: they compare normally as `f64` and encode
//! normally.
//!
//! # Collation folds *before* the encoding, not after
//!
//! A `TEXT` payload is the bytes [`Collation::fold`] produces, not the stored
//! bytes. For `BINARY` those are the same bytes and nothing changes; for
//! `NOCASE` the payload is the ASCII-lower-cased value, and for `RTRIM` it is
//! the value without its trailing spaces.
//!
//! This is the whole reason the catalog records an index's collation
//! ([`crate::catalog::Index::collations`]). The property the design rests on is
//! that `memcmp` of the encoding is the collation's own comparison, and
//! [`Collation::fold`] is exactly the transform that makes byte order equal
//! collated order — the same function the scan's comparison uses one level up,
//! so a probe and a scan cannot disagree.
//!
//! Two consequences worth naming, because both are visible:
//!
//! * **Two different strings can share one key.** `'Ada'` and `'ADA'` encode
//!   identically in a `NOCASE` index. They stay distinct rows — the row id is
//!   still on the end — and they are adjacent in row-id order rather than in
//!   value order, exactly as two integers above 2^53 that round to one `f64`
//!   already were.
//! * **The planner may only use an index whose collation *equals* the one the
//!   comparison resolved.** A `NOCASE` index cannot answer a `BINARY` `=`, and
//!   a `BINARY` index cannot answer a `NOCASE` one. That rule is SQLite's, it
//!   lives in [`crate::engine`], and it is what keeps the two access paths
//!   answering the same question.

use alloc::string::String;
use alloc::vec::Vec;

use crate::collation::{self, Collation};
use crate::error::{Error, Result};
use crate::traits::RowId;
use crate::value::Value;

/// The reserved namespace every scalar index entry lives under.
///
/// See the module docs for why nothing else can produce this prefix.
const INDEX_PREFIX: &[u8] = b"\x01idx:";

/// Class tags, ordered to match `eval::mem_cmp`'s storage-class order.
const TAG_NULL: u8 = 0x01;
const TAG_NUMBER: u8 = 0x02;
const TAG_TEXT: u8 = 0x03;
const TAG_BLOB: u8 = 0x04;

/// A `\0` inside a text or blob payload becomes `\0\xff`, and the payload ends
/// with `\0\x01`. `0xff` is above `0x01`, so a real (escaped) `\0` always sorts
/// above the terminator, which is what makes `"a"` sort below `"a\0"`.
const ESCAPE_BYTE: u8 = 0x00;
const ESCAPED_NUL: u8 = 0xff;
const TERMINATOR: u8 = 0x01;

/// The bytes every entry of one index shares.
///
/// Ends in a `\0` that no index name can contain, so one index's entries can
/// never be read as another's, however the names are spelled.
pub fn index_prefix(index: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(INDEX_PREFIX.len() + index.len() + 1);
    index_prefix_into(index, &mut out);
    out
}

/// [`index_prefix`] appending onto a buffer the caller already owns.
///
/// Same bytes; only the `Vec` changes hands. A join probe builds one of these
/// per outer row and can hand back the same buffer every time — see
/// [`KeyRange::equality_into`].
pub fn index_prefix_into(index: &str, out: &mut Vec<u8>) {
    out.reserve(INDEX_PREFIX.len() + index.len() + 1);
    out.extend_from_slice(INDEX_PREFIX);
    out.extend(index.as_bytes().iter().map(u8::to_ascii_lowercase));
    out.push(0);
}

/// Append the memcomparable encoding of one value under one collation.
///
/// See the module docs for the layout, for why `NaN` is an error here rather
/// than a key, and for why the collation folds the text before it is written
/// rather than being applied to the bytes afterwards. `collation` is consulted
/// for `TEXT` and for nothing else, which is where SQLite consults it too.
pub fn encode_value(out: &mut Vec<u8>, value: &Value, collation: Collation) -> Result<()> {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Integer(i) => {
            out.push(TAG_NUMBER);
            out.extend_from_slice(&total_order(*i as f64)?.to_be_bytes());
        }
        Value::Real(r) => {
            out.push(TAG_NUMBER);
            out.extend_from_slice(&total_order(*r)?.to_be_bytes());
        }
        Value::Text(text) => {
            out.push(TAG_TEXT);
            encode_bytes(out, &collation.fold(text.as_bytes()));
        }
        Value::Blob(bytes) => {
            out.push(TAG_BLOB);
            encode_bytes(out, bytes);
        }
        // A vector column cannot carry a scalar index — the catalog refuses
        // the declaration — so this is unreachable through SQL. Saying so is
        // cheaper than assuming it.
        Value::Vector(_) => {
            return Err(Error::Unsupported(String::from(
                "a VECTOR value has no ordering and cannot be a B-tree index key",
            )))
        }
    }
    Ok(())
}

/// Escape and terminate a byte payload so that `memcmp` on the result orders
/// exactly as `memcmp` on the input, and so that no encoding is a prefix of
/// another.
fn encode_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.reserve(bytes.len() + 2);
    for byte in bytes {
        out.push(*byte);
        if *byte == ESCAPE_BYTE {
            out.push(ESCAPED_NUL);
        }
    }
    out.push(ESCAPE_BYTE);
    out.push(TERMINATOR);
}

/// IEEE 754's total-order transform: the `u64` whose unsigned order is the
/// `f64`'s numeric order.
///
/// Positive values get their sign bit set; negative values are inverted whole,
/// which reverses the magnitude ordering that the sign-magnitude layout would
/// otherwise give. `-0.0` arrives here already folded to `+0.0`.
fn total_order(value: f64) -> Result<u64> {
    if value.is_nan() {
        return Err(Error::Unsupported(String::from(
            "NaN cannot be stored in an indexed column: this engine's comparison treats NaN as \
             equal to every number, which no ordered index can reproduce, so indexing it would \
             make a query answered from the index return different rows than a scan",
        )));
    }
    // `-0.0 == 0.0`, so this folds the two into one key. See the module docs.
    let value = if value == 0.0 { 0.0 } else { value };
    let bits = value.to_bits();
    Ok(if bits & (1 << 63) != 0 {
        !bits
    } else {
        bits | (1 << 63)
    })
}

/// The full key one row contributes to one index.
///
/// The row id goes last, big-endian, so entries that share a value are ordered
/// by row id and every row's entry is distinct from every other's — which is
/// what keeps two rows whose text folds to the same key from being one entry.
///
/// `collations` is the index's declaration, positionally; a position it does
/// not reach is `BINARY`, which is what every index written before catalog
/// version 6 was.
pub fn entry_key(
    index: &str,
    values: &[&Value],
    collations: &[Collation],
    id: RowId,
) -> Result<Vec<u8>> {
    let mut key = probe_prefix(index, values, collations)?;
    key.extend_from_slice(&id.to_be_bytes());
    Ok(key)
}

/// The prefix every entry whose leading columns equal `values` shares.
///
/// This is [`entry_key`] without the row id: scanning it visits exactly the
/// rows that agree on those columns *under `collations`*, in row-id order.
pub fn probe_prefix(index: &str, values: &[&Value], collations: &[Collation]) -> Result<Vec<u8>> {
    let mut key = index_prefix(index);
    for (position, value) in values.iter().enumerate() {
        encode_value(&mut key, value, collation::at(collations, position))?;
    }
    Ok(key)
}

/// Recover the row id an entry key ends with.
pub fn row_id_from_entry(key: &[u8]) -> Result<RowId> {
    let bytes = key
        .get(key.len().wrapping_sub(8)..)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .ok_or_else(|| Error::Corrupt(String::from("index entry key is too short")))?;
    Ok(RowId::from_be_bytes(bytes))
}

/// The first key that does *not* start with `prefix`, or `None` when every key
/// at or above `prefix` does.
///
/// The same rule the tree's prefix walk uses; it lives here too so a range can
/// be built without reaching into the tree.
pub fn upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    while let Some(last) = upper.pop() {
        if last != u8::MAX {
            upper.push(last + 1);
            return Some(upper);
        }
    }
    None
}

/// A half-open range of index keys, `start` inclusive and `end` exclusive.
///
/// `end` of `None` means "to the end of the key space", which for an index
/// prefix that is not all `0xff` never happens — it is here so the type cannot
/// lie about the one case where it could.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    /// First key to read, inclusive.
    pub start: Vec<u8>,
    /// First key not to read.
    pub end: Option<Vec<u8>>,
}

impl KeyRange {
    /// Every entry whose leading columns equal `values` under `collations`.
    pub fn equality(index: &str, values: &[&Value], collations: &[Collation]) -> Result<Self> {
        let start = probe_prefix(index, values, collations)?;
        Ok(Self {
            end: upper_bound(&start),
            start,
        })
    }

    /// [`KeyRange::equality`] into two buffers the caller already owns, both
    /// cleared and refilled.
    ///
    /// `false` means the range has no upper bound — the [`KeyRange::end`] of
    /// `None` this type carries — and `end` is then empty and meaningless.
    ///
    /// Identical bytes to [`KeyRange::equality`]; the difference is who owns
    /// the two `Vec`s. A join probe builds one range per *outer row*
    /// (`crate::exec::IndexProbe`), so the returning form was three heap
    /// allocations per outer row — the prefix, the value appended onto it, and
    /// the upper bound's copy — for keys that die with the row (`PERF.md`,
    /// AHL-549).
    pub fn equality_into(
        index: &str,
        values: &[&Value],
        collations: &[Collation],
        start: &mut Vec<u8>,
        end: &mut Vec<u8>,
    ) -> Result<bool> {
        start.clear();
        index_prefix_into(index, start);
        for (position, value) in values.iter().enumerate() {
            encode_value(start, value, collation::at(collations, position))?;
        }
        end.clear();
        end.extend_from_slice(start);
        // `upper_bound`'s rule, in place: strip trailing `0xff`s and bump the
        // last byte that is not one. An all-`0xff` prefix has no upper bound,
        // which for an index prefix never happens but is answered rather than
        // assumed.
        while let Some(last) = end.pop() {
            if last != u8::MAX {
                end.push(last + 1);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every entry of one index, in key order.
    pub fn whole(index: &str) -> Self {
        let start = index_prefix(index);
        Self {
            end: upper_bound(&start),
            start,
        }
    }

    /// Narrow this range's lower edge to `value`, inclusively.
    ///
    /// Always inclusive, even for a strict `>`: including the whole group of
    /// entries that encode equal to `value` costs one group of candidates and
    /// removes every question about what happens at the boundary when two
    /// distinct values share an encoding. The `WHERE` filter decides which of
    /// them actually match.
    pub fn with_lower(
        mut self,
        index: &str,
        leading: &[&Value],
        collations: &[Collation],
        value: &Value,
    ) -> Result<Self> {
        let mut start = probe_prefix(index, leading, collations)?;
        encode_value(&mut start, value, collation::at(collations, leading.len()))?;
        if start > self.start {
            self.start = start;
        }
        Ok(self)
    }

    /// Narrow this range's upper edge to `value`, inclusively — the same
    /// deliberate over-scan as [`KeyRange::with_lower`].
    pub fn with_upper(
        mut self,
        index: &str,
        leading: &[&Value],
        collations: &[Collation],
        value: &Value,
    ) -> Result<Self> {
        let mut bound = probe_prefix(index, leading, collations)?;
        encode_value(&mut bound, value, collation::at(collations, leading.len()))?;
        let end = upper_bound(&bound);
        self.end = match (self.end.take(), end) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;
    use core::cmp::Ordering;

    fn encode(value: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        encode_value(&mut out, value, Collation::Binary).expect("encodable");
        out
    }

    #[test]
    fn the_class_tags_order_null_below_numbers_below_text_below_blobs() {
        let null = encode(&Value::Null);
        let number = encode(&Value::Integer(i64::MIN));
        let text = encode(&Value::Text(String::new().into()));
        let blob = encode(&Value::Blob(Vec::new()));
        assert!(null < number);
        assert!(number < text);
        assert!(text < blob);
    }

    #[test]
    fn an_integer_and_the_equal_real_share_one_key() {
        assert_eq!(encode(&Value::Integer(1)), encode(&Value::Real(1.0)));
        assert_eq!(encode(&Value::Integer(0)), encode(&Value::Real(-0.0)));
        assert_eq!(encode(&Value::Real(0.0)), encode(&Value::Real(-0.0)));
    }

    #[test]
    fn nan_is_refused_rather_than_encoded() {
        let mut out = Vec::new();
        let err = encode_value(&mut out, &Value::Real(f64::NAN), Collation::Binary).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
        assert!(err.to_string().contains("NaN"), "{err}");
    }

    #[test]
    fn infinities_encode_at_the_ends_of_the_numeric_range() {
        assert!(encode(&Value::Real(f64::NEG_INFINITY)) < encode(&Value::Integer(i64::MIN)));
        assert!(encode(&Value::Integer(i64::MAX)) < encode(&Value::Real(f64::INFINITY)));
    }

    #[test]
    fn a_text_prefix_sorts_below_the_longer_string() {
        assert!(
            encode(&Value::Text("a".to_string().into()))
                < encode(&Value::Text("ab".to_string().into()))
        );
        assert!(
            encode(&Value::Text("a".to_string().into()))
                < encode(&Value::Text("a\0".to_string().into()))
        );
        assert!(
            encode(&Value::Text(String::new().into()))
                < encode(&Value::Text("\0".to_string().into()))
        );
        assert!(
            encode(&Value::Text("a\u{1}".to_string().into()))
                > encode(&Value::Text("a".to_string().into()))
        );
    }

    /// The escape has to keep one column from spilling into the next: `("a",
    /// "b")` and `("a\0b", "")` must not produce the same bytes.
    #[test]
    fn one_columns_encoding_cannot_be_read_as_another_columns() {
        let pair = |a: &str, b: &str| {
            let mut out = Vec::new();
            encode_value(
                &mut out,
                &Value::Text(a.to_string().into()),
                Collation::Binary,
            )
            .unwrap();
            encode_value(
                &mut out,
                &Value::Text(b.to_string().into()),
                Collation::Binary,
            )
            .unwrap();
            out
        };
        assert_ne!(pair("a", "b"), pair("a\0b", ""));
        assert_ne!(pair("a", ""), pair("", "a"));
    }

    #[test]
    fn an_entry_key_round_trips_its_row_id() {
        for id in [0u64, 1, 42, RowId::MAX] {
            let key = entry_key("i", &[&Value::Integer(7)], &[], id).unwrap();
            assert_eq!(row_id_from_entry(&key).unwrap(), id);
            assert!(key.starts_with(&probe_prefix("i", &[&Value::Integer(7)], &[]).unwrap()));
        }
    }

    #[test]
    fn index_names_are_folded_and_terminated() {
        assert_eq!(index_prefix("Idx"), index_prefix("idx"));
        assert_eq!(index_prefix("i"), b"\x01idx:i\0".to_vec());
        // One index's prefix is never a prefix of another's, however the names
        // nest, because the `\0` terminates it.
        assert!(!index_prefix("ab").starts_with(&index_prefix("a")));
    }

    #[test]
    fn an_equality_range_covers_exactly_its_group() {
        let range = KeyRange::equality("i", &[&Value::Integer(5)], &[]).unwrap();
        let inside = entry_key("i", &[&Value::Integer(5)], &[], 9).unwrap();
        let above = entry_key("i", &[&Value::Integer(6)], &[], 0).unwrap();
        let below = entry_key("i", &[&Value::Integer(4)], &[], u64::MAX).unwrap();
        assert!(inside >= range.start);
        assert!(inside < *range.end.as_ref().unwrap());
        assert!(above >= *range.end.as_ref().unwrap());
        assert!(below < range.start);
    }

    #[test]
    fn a_bounded_range_keeps_both_edges_inclusive() {
        let range = KeyRange::whole("i")
            .with_lower("i", &[], &[], &Value::Integer(2))
            .unwrap()
            .with_upper("i", &[], &[], &Value::Integer(4))
            .unwrap();
        for id in [0u64, 7] {
            for value in [2i64, 3, 4] {
                let key = entry_key("i", &[&Value::Integer(value)], &[], id).unwrap();
                assert!(key >= range.start, "{value} below start");
                assert!(key < *range.end.as_ref().unwrap(), "{value} above end");
            }
            assert!(entry_key("i", &[&Value::Integer(1)], &[], id).unwrap() < range.start);
            assert!(
                entry_key("i", &[&Value::Integer(5)], &[], id).unwrap()
                    >= *range.end.as_ref().unwrap()
            );
        }
    }

    /// The property the whole design rests on: byte order equals value order,
    /// for the order the engine's own comparison operator uses.
    ///
    /// That operator is `eval::comparison` — every `WHERE` goes through it —
    /// and it compares numbers as `f64`, text under the resolved collation and
    /// blobs as bytes. The class order between them is `eval::mem_cmp`'s,
    /// which is what an index has to use to keep the classes apart.
    fn reference_order(left: &Value, right: &Value) -> Ordering {
        reference_order_under(left, right, Collation::Binary)
    }

    fn reference_order_under(left: &Value, right: &Value, collation: Collation) -> Ordering {
        fn class(value: &Value) -> u8 {
            match value {
                Value::Null => 0,
                Value::Integer(_) | Value::Real(_) => 1,
                Value::Text(_) => 2,
                Value::Blob(_) => 3,
                Value::Vector(_) => 4,
            }
        }
        let ordering = class(left).cmp(&class(right));
        if ordering != Ordering::Equal {
            return ordering;
        }
        match (left, right) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Text(a), Value::Text(b)) => collation.compare(a, b),
            (Value::Blob(a), Value::Blob(b)) => a.as_slice().cmp(b.as_slice()),
            // Exactly what `eval::comparison` does once it has ruled out text
            // and blobs: both sides as `f64`.
            _ => match (left.as_f64(), right.as_f64()) {
                (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
                _ => Ordering::Equal,
            },
        }
    }

    /// Every boundary worth naming, plus a generated spread, checked in both
    /// directions: encode-then-`memcmp` must agree with [`reference_order`] for
    /// every pair.
    fn corpus() -> Vec<Value> {
        let mut values = vec![
            Value::Null,
            Value::Integer(i64::MIN),
            Value::Integer(i64::MIN + 1),
            Value::Integer(-9_007_199_254_740_993),
            Value::Integer(-1),
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(9_007_199_254_740_992),
            Value::Integer(i64::MAX - 1),
            Value::Integer(i64::MAX),
            Value::Real(f64::NEG_INFINITY),
            Value::Real(-f64::MAX),
            Value::Real(-1.5),
            Value::Real(-f64::MIN_POSITIVE),
            Value::Real(-0.0),
            Value::Real(0.0),
            Value::Real(f64::MIN_POSITIVE),
            Value::Real(1.5),
            Value::Real(f64::MAX),
            Value::Real(f64::INFINITY),
            Value::Text(String::new().into()),
            Value::Text("\0".to_string().into()),
            Value::Text("\0\0".to_string().into()),
            Value::Text("\u{1}".to_string().into()),
            Value::Text("A".to_string().into()),
            Value::Text("a".to_string().into()),
            Value::Text("a\0".to_string().into()),
            Value::Text("ab".to_string().into()),
            Value::Text("abc".to_string().into()),
            Value::Text("é".to_string().into()),
            Value::Text("日本語".to_string().into()),
            Value::Text("\u{10ffff}".to_string().into()),
            Value::Blob(Vec::new()),
            Value::Blob(vec![0]),
            Value::Blob(vec![0, 0]),
            Value::Blob(vec![0, 255]),
            Value::Blob(vec![1]),
            Value::Blob(vec![255]),
            Value::Blob(vec![255, 0]),
        ];
        // A deterministic spread over the whole 64-bit space, so the property
        // is not only checked at the boundaries somebody thought of.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..600 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            values.push(Value::Integer(state as i64));
            let real = f64::from_bits(state);
            if !real.is_nan() {
                values.push(Value::Real(real));
            }
            let bytes: Vec<u8> = state.to_le_bytes()[..(state % 8) as usize + 1].to_vec();
            values.push(Value::Blob(bytes.clone()));
            values.push(Value::Text(
                String::from_utf8_lossy(&bytes).into_owned().into(),
            ));
        }
        values
    }

    #[test]
    fn encode_then_memcmp_agrees_with_the_engines_value_order() {
        let values = corpus();
        let encoded: Vec<Vec<u8>> = values.iter().map(encode).collect();
        for (i, left) in values.iter().enumerate() {
            for (j, right) in values.iter().enumerate() {
                let expected = reference_order(left, right);
                let actual = encoded[i].cmp(&encoded[j]);
                assert_eq!(
                    actual, expected,
                    "encode({left:?}) vs encode({right:?}): bytes said {actual:?}, values say \
                     {expected:?}"
                );
            }
        }
    }

    /// The same property for a two-column key: concatenation must order
    /// lexicographically by column, which is what makes a composite index a
    /// composite index rather than two indexes stapled together.
    #[test]
    fn a_composite_key_orders_by_column_in_turn() {
        let values = corpus();
        // A representative slice; the full square of the whole corpus twice
        // over is minutes, and the property is about composition, not about
        // the per-value encoding the test above already covers exhaustively.
        let sample: Vec<&Value> = values.iter().step_by(37).collect();
        for a in &sample {
            for b in &sample {
                for c in &sample {
                    for d in &sample {
                        let mut left = Vec::new();
                        encode_value(&mut left, a, Collation::Binary).unwrap();
                        encode_value(&mut left, b, Collation::Binary).unwrap();
                        let mut right = Vec::new();
                        encode_value(&mut right, c, Collation::Binary).unwrap();
                        encode_value(&mut right, d, Collation::Binary).unwrap();
                        let expected = reference_order(a, c).then(reference_order(b, d));
                        assert_eq!(
                            left.cmp(&right),
                            expected,
                            "({a:?}, {b:?}) vs ({c:?}, {d:?})"
                        );
                    }
                }
            }
        }
    }

    /// Appending the row id must not disturb the value order: the point of a
    /// self-delimiting encoding is that the eight bytes on the end can never be
    /// read as part of the last column.
    #[test]
    fn the_row_id_suffix_never_reorders_two_different_values() {
        let values = corpus();
        for (i, left) in values.iter().enumerate().step_by(11) {
            for right in values.iter().skip(i + 1).step_by(13) {
                let expected = reference_order(left, right);
                if expected == Ordering::Equal {
                    continue;
                }
                for (a, b) in [(0u64, RowId::MAX), (RowId::MAX, 0), (7, 7)] {
                    let ka = entry_key("i", &[left], &[], a).unwrap();
                    let kb = entry_key("i", &[right], &[], b).unwrap();
                    assert_eq!(ka.cmp(&kb), expected, "{left:?}@{a} vs {right:?}@{b}");
                }
            }
        }
    }

    // ------------------------------------------------------------ collations

    /// The same property as
    /// [`encode_then_memcmp_agrees_with_the_engines_value_order`], for each
    /// collation: byte order under the folded encoding must be the collation's
    /// own order. This is what makes an index probe and a scan agree, and it is
    /// the only thing that does.
    #[test]
    fn a_collated_encoding_orders_exactly_as_the_collation_compares() {
        let values = corpus();
        for collation in [Collation::NoCase, Collation::RTrim] {
            let encoded: Vec<Vec<u8>> = values
                .iter()
                .map(|value| {
                    let mut out = Vec::new();
                    encode_value(&mut out, value, collation).unwrap();
                    out
                })
                .collect();
            for (i, left) in values.iter().enumerate() {
                for (j, right) in values.iter().enumerate() {
                    let expected = reference_order_under(left, right, collation);
                    assert_eq!(
                        encoded[i].cmp(&encoded[j]),
                        expected,
                        "{collation} on {left:?} vs {right:?}"
                    );
                }
            }
        }
    }

    /// The motivating case, at the level of bytes: `'Ada'` and `'ADA'` are one
    /// key in a `NOCASE` index and two keys in a `BINARY` one, so an equality
    /// probe for `'ADA'` finds both under the first and neither stored spelling
    /// but its own under the second.
    #[test]
    fn nocase_folds_the_key_and_binary_does_not() {
        let ada = Value::Text("Ada".to_string().into());
        let shouty = Value::Text("ADA".to_string().into());
        let nocase = &[Collation::NoCase];

        let folded = KeyRange::equality("i", &[&shouty], nocase).unwrap();
        let stored = entry_key("i", &[&ada], nocase, 1).unwrap();
        assert!(stored >= folded.start && stored < *folded.end.as_ref().unwrap());

        let exact = KeyRange::equality("i", &[&shouty], &[Collation::Binary]).unwrap();
        let stored = entry_key("i", &[&ada], &[Collation::Binary], 1).unwrap();
        assert!(stored < exact.start || stored >= *exact.end.as_ref().unwrap());
    }

    /// Two rows whose text folds to one key are still two entries: the row id
    /// on the end is what keeps a `NOCASE` index from losing a row.
    #[test]
    fn two_rows_that_fold_together_are_still_two_entries() {
        let nocase = &[Collation::NoCase];
        let one = entry_key("i", &[&Value::Text("Ada".to_string().into())], nocase, 1).unwrap();
        let two = entry_key("i", &[&Value::Text("ADA".to_string().into())], nocase, 2).unwrap();
        assert_ne!(one, two);
        assert_eq!(one[..one.len() - 8], two[..two.len() - 8]);
        assert_eq!(row_id_from_entry(&one).unwrap(), 1);
        assert_eq!(row_id_from_entry(&two).unwrap(), 2);
    }

    /// A collation applies to the column it was declared on and to no other,
    /// which for a composite key means the fold has to be positional.
    #[test]
    fn a_composite_key_folds_each_column_under_its_own_collation() {
        let collations = &[Collation::NoCase, Collation::Binary];
        let key = |a: &str, b: &str| {
            probe_prefix(
                "i",
                &[
                    &Value::Text(a.to_string().into()),
                    &Value::Text(b.to_string().into()),
                ],
                collations,
            )
            .unwrap()
        };
        assert_eq!(key("ADA", "x"), key("ada", "x"));
        assert_ne!(key("ada", "X"), key("ada", "x"));
    }

    /// `RTRIM` in a range: the bounds are folded too, so a stored `'a  '` is
    /// inside a range built from `'a'`.
    #[test]
    fn an_rtrim_range_includes_the_padded_spellings() {
        let rtrim = &[Collation::RTrim];
        let range =
            KeyRange::equality("i", &[&Value::Text("a".to_string().into())], rtrim).unwrap();
        for stored in ["a", "a ", "a    "] {
            let key = entry_key("i", &[&Value::Text(stored.to_string().into())], rtrim, 3).unwrap();
            assert!(
                key >= range.start && key < *range.end.as_ref().unwrap(),
                "{stored:?} fell outside its own range"
            );
        }
    }
}
