//! Column types and runtime values.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

/// A column type in the InlaySQL dialect.
///
/// These are SQLite's five *affinities*, not its storage classes, because that
/// is what a declared column type actually decides — see
/// [`crate::sql`]'s type resolution. Four of them share a name with a storage
/// class; [`DataType::Numeric`] is the one that does not, and it is what every
/// type name SQLite does not recognise resolves to (`DECIMAL(8,2)`,
/// `DATETIME`, `BOOLEAN`, `JSON`).
///
/// `VECTOR(n)` is the one addition on top: a fixed-width `f32` array that the
/// planner can route to an approximate-nearest-neighbour index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// 64-bit signed integer.
    Integer,
    /// 64-bit float.
    Real,
    /// UTF-8 text. Text columns are full-text indexed (see the engine docs).
    Text,
    /// Opaque bytes.
    Blob,
    /// SQLite's `NUMERIC` affinity: a number when the value is one, and the
    /// value unchanged when it is not.
    ///
    /// This is the affinity every unrecognised type name gets, which is why it
    /// is the one a framework's migrations meet most often. It is deliberately
    /// *not* a storage class — a `NUMERIC` column holds integers, reals, text
    /// and blobs, exactly as SQLite's does.
    Numeric,
    /// Fixed-width `f32` embedding of the given dimension.
    Vector(usize),
    /// Fixed-width embedding stored with symmetric per-vector int8
    /// quantisation. Values cross the public API as `f32`; only the durable
    /// row and ANN index representations are quantised.
    QuantizedVector(usize),
    /// SQLite's `STRICT`-table `ANY` type: no affinity and no coercion at
    /// all, the column holds whatever storage class the value it was given
    /// already has. Only reachable inside a `STRICT` table — see
    /// [`crate::catalog::Table::strict`] — because it is the one type name
    /// SQLite's ordinary tables do not recognise as anything but `NUMERIC`.
    Any,
}

impl DataType {
    /// The dimension of either vector representation.
    pub fn vector_dim(self) -> Option<usize> {
        match self {
            Self::Vector(dim) | Self::QuantizedVector(dim) => Some(dim),
            _ => None,
        }
    }

    /// Whether this vector column opts into int8 storage.
    pub fn is_quantized_vector(self) -> bool {
        matches!(self, Self::QuantizedVector(_))
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Integer => f.write_str("INTEGER"),
            DataType::Real => f.write_str("REAL"),
            DataType::Text => f.write_str("TEXT"),
            DataType::Blob => f.write_str("BLOB"),
            DataType::Numeric => f.write_str("NUMERIC"),
            DataType::Vector(dim) => write!(f, "VECTOR({dim})"),
            DataType::QuantizedVector(dim) => write!(f, "VECTOR({dim}, INT8)"),
            DataType::Any => f.write_str("ANY"),
        }
    }
}

/// An owned text value that is cheap to clone.
///
/// `Value::Text` used to hold a `String`, so every clone — and the join
/// pipeline clones each inner row once per matching outer row — reallocated
/// and recopied the bytes. `Arc<str>` keeps exactly `String`'s semantics —
/// equality, ordering, hashing and formatting are all defined over the `str` —
/// while a clone is a refcount bump with no allocation. That is what
/// `PERF.md`'s "a projected row allocates once at the boundary" means in
/// practice: a decoded text is allocated once and shared everywhere it is
/// copied.
///
/// `Arc`, not `Rc`, because `Value` is held in a `static` (it must be `Sync`)
/// and the `inlaysql` crate may hand a value across its dedicated I/O thread.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Text(Arc<str>);

impl Text {
    /// Borrow the text as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Text {
    fn from(value: String) -> Self {
        Text(Arc::from(value.as_str()))
    }
}

impl From<&str> for Text {
    fn from(value: &str) -> Self {
        Text(Arc::from(value))
    }
}

impl From<Text> for String {
    fn from(value: Text) -> Self {
        value.0.as_ref().to_string()
    }
}

impl core::ops::Deref for Text {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Text {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl core::borrow::Borrow<str> for Text {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Debug as the `str`, exactly as `String`'s `Debug` does, so `Value`'s derived
/// `Debug` output is unchanged.
impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

/// A runtime value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// Integer value.
    Integer(i64),
    /// Floating point value.
    Real(f64),
    /// Text value.
    Text(Text),
    /// Binary value.
    Blob(Vec<u8>),
    /// Embedding value.
    Vector(Vec<f32>),
}

impl Value {
    /// Human-readable name of the value's type, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Integer(_) => "INTEGER",
            Value::Real(_) => "REAL",
            Value::Text(_) => "TEXT",
            Value::Blob(_) => "BLOB",
            Value::Vector(_) => "VECTOR",
        }
    }

    /// Heap bytes this value owns, on top of the [`Value`] itself.
    ///
    /// For budgeting a working set that is being *built*, which is why it errs
    /// upward on the one case where it can: [`Value::Text`] is reference
    /// counted, so several rows can share one allocation and charging each
    /// holder over-counts. A ceiling whose job is to refuse before the
    /// allocator does should over-count rather than under-count, and the case
    /// that actually threatens a process — a wide scan whose text cells were
    /// each decoded separately — is counted exactly.
    ///
    /// A scalar owns nothing, so this is zero for `NULL`, integers and reals
    /// without touching memory to find out.
    pub fn heap_bytes(&self) -> usize {
        // The allocation header a Rust allocator keeps beside a growable
        // buffer is not observable from here, so two words per allocation
        // stands in for it — the same stand-in `HashJoinTable::resident_bytes`
        // has always used, kept identical so two budgets cannot disagree about
        // the size of the same row.
        const OVERHEAD: usize = 2 * core::mem::size_of::<usize>();
        match self {
            Value::Null | Value::Integer(_) | Value::Real(_) => 0,
            Value::Text(text) => text.len().saturating_add(OVERHEAD),
            Value::Blob(blob) => blob.capacity().saturating_add(OVERHEAD),
            Value::Vector(vector) => vector
                .capacity()
                .saturating_mul(core::mem::size_of::<f32>())
                .saturating_add(OVERHEAD),
        }
    }

    /// Borrow the value as text, if it is text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Borrow the value as an embedding, if it is one.
    pub fn as_vector(&self) -> Option<&[f32]> {
        match self {
            Value::Vector(v) => Some(v),
            _ => None,
        }
    }

    /// Read the value as an integer, if it is one.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Read the value as a float, widening integers.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Integer(i) => Some(*i as f64),
            Value::Real(r) => Some(*r),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Real(r) => write!(f, "{r}"),
            Value::Text(s) => f.write_str(s),
            Value::Blob(b) => write!(f, "<{} bytes>", b.len()),
            Value::Vector(v) => write!(f, "<vector dim={}>", v.len()),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Integer(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(Text::from(v))
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(Text::from(v))
    }
}

impl From<Vec<f32>> for Value {
    fn from(v: Vec<f32>) -> Self {
        Value::Vector(v)
    }
}

/// A cell value that may borrow from a row's encoded bytes instead of owning
/// them.
///
/// `AHL-478`'s "structural fix" (`PERF.md`): `Value` owns its data — `Text`
/// is a `String`, `Blob` is a `Vec<u8>` — so decoding a row has always meant
/// allocating one of those per text/blob column, whether or not the row
/// survives a filter. `ValueRef` is the same six cases with the two
/// variable-length ones borrowed from the row bytes they were decoded out of:
/// `row::decode_row_ref_masked` builds these by slicing, not allocating.
///
/// This began as an internal type for the hot read path inside
/// `exec.rs`/`eval.rs`: a row that a filter rejects can be built, tested and
/// dropped without a single heap allocation for its text or blob columns, and
/// a [`Value`] is only materialised for a row that survives — "a projected row
/// allocates once at the boundary," in `PERF.md`'s words.
///
/// **Since AHL-535 it also crosses the public API**, in exactly one place:
/// [`crate::Engine::run_query_each_ref`]'s callback, where the boundary is the
/// caller's rather than the engine's. A consumer that only reads a row can now
/// read it without the engine allocating an owned copy first. [`Value`] is
/// still what every other API — [`crate::Engine::run_query`],
/// [`crate::Engine::run_query_each`], every bound parameter — hands back, and
/// a borrowed cell is only valid for the callback call it arrived in.
///
/// [`DataType::Vector`]/[`DataType::QuantizedVector`] are the deliberate
/// exception: the row codec stores a vector as little-endian `f32` bytes, and
/// reinterpreting those bytes as `&[f32]` without copying would need
/// `unsafe` (an alignment- and endianness-dependent transmute) — precisely
/// what `inlaysql-core`'s `#![forbid(unsafe_code)]` rules out. A vector cell
/// materialises a `Vec<f32>` when decoded either way, so `ValueRef::Vector`
/// simply owns one; it is not the column type this pass targets; and this is
/// the boundary named in the design writeup rather than a bug.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueRef<'a> {
    /// SQL `NULL`.
    Null,
    /// Integer value. `Copy`, so borrowing would buy nothing.
    Integer(i64),
    /// Floating point value. `Copy`, so borrowing would buy nothing.
    Real(f64),
    /// Text value, borrowed from the row bytes it was decoded out of.
    Text(&'a str),
    /// Binary value, borrowed from the row bytes it was decoded out of.
    Blob(&'a [u8]),
    /// Embedding value. Owned — see the type-level doc for why.
    Vector(Vec<f32>),
}

impl<'a> ValueRef<'a> {
    /// Human-readable name of the value's type, for error messages — the same
    /// names [`Value::type_name`] uses.
    pub fn type_name(&self) -> &'static str {
        match self {
            ValueRef::Null => "NULL",
            ValueRef::Integer(_) => "INTEGER",
            ValueRef::Real(_) => "REAL",
            ValueRef::Text(_) => "TEXT",
            ValueRef::Blob(_) => "BLOB",
            ValueRef::Vector(_) => "VECTOR",
        }
    }

    /// Whether this is `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, ValueRef::Null)
    }

    /// Borrow the value as text, if it is text.
    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            ValueRef::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Borrow the value as bytes, if it is a blob.
    pub fn as_blob(&self) -> Option<&'a [u8]> {
        match self {
            ValueRef::Blob(b) => Some(b),
            _ => None,
        }
    }

    /// Read the value as a float, widening integers — the same rule
    /// [`Value::as_f64`] follows.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ValueRef::Integer(i) => Some(*i as f64),
            ValueRef::Real(r) => Some(*r),
            _ => None,
        }
    }

    /// Read the value as an integer, if it is one.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ValueRef::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Materialise an owned [`Value`], allocating for `Text`/`Blob`.
    ///
    /// This is the one crossing point back to the owned world: called once,
    /// at the boundary a row is kept rather than dropped.
    pub fn to_owned_value(&self) -> Value {
        match self {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => Value::Integer(*i),
            ValueRef::Real(r) => Value::Real(*r),
            ValueRef::Text(s) => Value::Text(Text::from(*s)),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
            ValueRef::Vector(v) => Value::Vector(v.clone()),
        }
    }
}

impl<'a> From<&'a Value> for ValueRef<'a> {
    /// Borrow an owned [`Value`] — used to fold a literal or a bound
    /// parameter into the same borrowed-comparison machinery a column
    /// reference uses, without cloning it.
    fn from(value: &'a Value) -> Self {
        match value {
            Value::Null => ValueRef::Null,
            Value::Integer(i) => ValueRef::Integer(*i),
            Value::Real(r) => ValueRef::Real(*r),
            Value::Text(s) => ValueRef::Text(s.as_str()),
            Value::Blob(b) => ValueRef::Blob(b.as_slice()),
            Value::Vector(v) => ValueRef::Vector(v.clone()),
        }
    }
}

/// Compare a borrowed cell with an owned one without materialising the
/// borrowed side.
///
/// This used to go through [`ValueRef::to_owned_value`], which allocates a
/// `Text` or a `Vec<u8>` for every `TEXT`/`BLOB` comparison — including the
/// ones about to answer "no". The allocation was never the answer, only the
/// route to it: the payloads compare as `&str` and `&[u8]` directly. Nothing
/// inside this workspace reaches this impl today — the executor compares
/// through `eval`'s `Cell`, which has SQL's rules — so it is the borrowed
/// half of a `pub` type's ordinary equality, kept cheap for the caller that
/// does reach it rather than a hot path being fixed.
///
/// Bit-identical to what the old route computed, which is [`Value`]'s own
/// derived `PartialEq`: same variant compares the payloads, and *every* other
/// pairing is unequal. In particular there is no numeric coercion here —
/// `Integer(1) != Real(1.0)`, exactly as `Value` says today — because this
/// operator is `Value`'s equality reached through a borrow, not SQL's `=`.
/// SQL's comparison rules, with their storage-class ordering and their
/// collations, live in `crate::eval`; a coercion invented here would quietly
/// disagree with them. `borrowed_equality_matches_the_owned_one` pins every
/// ordered pair of variants against the old route.
impl PartialEq<Value> for ValueRef<'_> {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (ValueRef::Null, Value::Null) => true,
            (ValueRef::Integer(left), Value::Integer(right)) => left == right,
            (ValueRef::Real(left), Value::Real(right)) => left == right,
            (ValueRef::Text(left), Value::Text(right)) => *left == right.as_str(),
            (ValueRef::Blob(left), Value::Blob(right)) => *left == right.as_slice(),
            (ValueRef::Vector(left), Value::Vector(right)) => left == right,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Value, ValueRef};
    use alloc::vec;
    use alloc::vec::Vec;

    /// One value per storage class, with the payloads that have ever made an
    /// equality subtle: both zeroes, `NaN`, the empty text and the empty blob,
    /// two spellings that differ only in case, and a vector holding each of
    /// those floats.
    fn menagerie() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(-1),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Real(0.0),
            Value::Real(-0.0),
            Value::Real(1.0),
            Value::Real(f64::NAN),
            Value::Real(f64::INFINITY),
            Value::Text("".into()),
            Value::Text("a".into()),
            Value::Text("A".into()),
            Value::Text("näme".into()),
            Value::Blob(Vec::new()),
            Value::Blob(vec![0]),
            Value::Blob(vec![0, 255]),
            Value::Vector(Vec::new()),
            Value::Vector(vec![0.0]),
            Value::Vector(vec![-0.0]),
            Value::Vector(vec![f32::NAN]),
            Value::Vector(vec![1.0, 2.0]),
        ]
    }

    /// Which storage class a value is, as an index — so the test can prove it
    /// covered every ordered pair of them rather than assume it.
    fn class(value: &Value) -> usize {
        match value {
            Value::Null => 0,
            Value::Integer(_) => 1,
            Value::Real(_) => 2,
            Value::Text(_) => 3,
            Value::Blob(_) => 4,
            Value::Vector(_) => 5,
        }
    }

    /// The comparison this impl replaced: materialise the borrowed side, then
    /// use `Value`'s own derived equality. Every answer below is checked
    /// against this, so "bit-identical to what it did before" is asserted
    /// rather than argued.
    fn through_an_owned_value(left: &ValueRef<'_>, right: &Value) -> bool {
        left.to_owned_value() == *right
    }

    #[test]
    fn borrowed_equality_matches_the_owned_one() {
        let values = menagerie();
        let mut covered = [[false; 6]; 6];
        for left in &values {
            for right in &values {
                let borrowed = ValueRef::from(left);
                assert_eq!(
                    borrowed == *right,
                    through_an_owned_value(&borrowed, right),
                    "{left:?} == {right:?}"
                );
                covered[class(left)][class(right)] = true;
            }
        }
        assert!(
            covered.iter().all(|row| row.iter().all(|seen| *seen)),
            "some pair of storage classes was never compared"
        );
    }

    /// The three answers a reader is most likely to expect the other way
    /// round, written out rather than left to the sweep above.
    ///
    /// No numeric coercion: `Value`'s equality does not have it, so neither
    /// does this. `NaN` equals nothing, including the same `NaN`. The two
    /// zeroes do equal each other, because IEEE-754 says so and `f64`'s
    /// `PartialEq` follows it.
    #[test]
    fn the_pairings_that_look_like_they_might_coerce_do_not() {
        assert!(ValueRef::Integer(1) != Value::Real(1.0));
        assert!(ValueRef::Real(1.0) != Value::Integer(1));
        assert!(ValueRef::Text("1") != Value::Integer(1));
        assert!(ValueRef::Blob(b"a") != Value::Text("a".into()));
        assert!(ValueRef::Null != Value::Integer(0));

        assert!(ValueRef::Real(f64::NAN) != Value::Real(f64::NAN));
        assert!(ValueRef::Real(0.0) == Value::Real(-0.0));
    }
}
