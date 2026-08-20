//! Column types and runtime values.

use alloc::string::{String, ToString};
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
        }
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
    Text(String),
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

    /// Borrow the value as text, if it is text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
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
        Value::Text(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
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
/// **This is an internal type.** It never crosses the public API —
/// [`Value`] is what every caller of [`crate::Statement`]/[`crate::Engine`]
/// sees, prepared or not. `ValueRef` exists for the hot read path inside
/// `exec.rs`/`eval.rs`: a row that a filter rejects can be built, tested and
/// dropped without a single heap allocation for its text or blob columns: a
/// [`Value`] is only materialised for a row that survives, at the point it is
/// copied into the result — "a projected row allocates once at the
/// boundary," in `PERF.md`'s words.
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
            ValueRef::Text(s) => Value::Text(String::from(*s)),
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

impl PartialEq<Value> for ValueRef<'_> {
    fn eq(&self, other: &Value) -> bool {
        self.to_owned_value() == *other
    }
}
