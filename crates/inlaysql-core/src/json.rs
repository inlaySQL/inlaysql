//! A hand-rolled JSON document model, parser, serializer and path language —
//! SQLite's json1 extension, in the dialect this engine already speaks.
//!
//! `inlaysql-core` is `no_std` and takes no new dependencies (AGENTS.md), so
//! this is written from scratch the way the MySQL wire protocol, SHA-1,
//! SHA-256 and `inlaysql-mcp`'s JSON-RPC already are in this repo. JSON is
//! stored exactly as SQLite stores it: as ordinary `TEXT`, parsed and
//! re-serialized by the functions in [`crate::eval`] that call into this
//! module. There is no new storage class here and no catalog change.
//!
//! Every corner this module's grammar and functions rely on was checked
//! against a real `sqlite3` binary (3.54) rather than assumed — see
//! `crates/inlaysql/tests/sqllogictest/json.test`, whose expectations were
//! all produced the same way.
//!
//! # Grammar
//!
//! Document parsing (`parse`) accepts strict RFC 8259 JSON: no trailing
//! commas, no comments, no single-quoted strings, no unquoted keys, no
//! leading `+`, no hex numbers — every one of those is `malformed JSON`
//! against sqlite3 3.54, checked directly, not assumed to be JSON5 the way
//! SQLite's own parser optionally is.
//!
//! Path parsing (`parse_path`) accepts the subset of SQLite's path language
//! this module could verify:
//!
//! * `$` alone — the whole document.
//! * `.key` — an unquoted object key, which (checked against sqlite3) reads
//!   every character up to the next `.`, `[` or the end of the path, with no
//!   restriction to identifier characters: `$.a b` is a valid path to the key
//!   `"a b"`.
//! * `."quoted key"` — a double-quoted object key, sharing this module's JSON
//!   string escape rules, so a key containing `.`, `[` or `"` needs this form.
//! * `[N]` — the `N`th element of an array, zero-based.
//! * `[#]` — one past the last element of an array: the append position
//!   `json_set`/`json_insert` write a new element at.
//! * `[#-N]` — `N` elements back from the append position (`[#-1]` is the
//!   last element), checked against sqlite3's own documented extension.
//!
//! A negative literal index (`[-1]`), a key that does not start with `$`, and
//! anything else outside this grammar are `bad JSON path` errors — checked:
//! sqlite3 refuses `$[-1]` the same way.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// A parsed JSON value.
///
/// Object members keep insertion order and duplicate keys are never merged —
/// checked against sqlite3: `json_extract('{"a":1,"a":2}', '$.a')` is `1`,
/// the *first* occurrence, which is why lookups below scan from the front
/// rather than keeping a map.
///
/// `Int`/`Real` mirror SQLite's own split of JSON's single `number`
/// production: a JSON number that parses as an `i64` is `Int`, and one that
/// does not (has a `.`/exponent, or overflows) is `Real` — the same rule
/// `sql::bind_literal` uses for a SQL numeric literal, checked against
/// sqlite3's `json_type`/`typeof`.
///
/// `Real` carries its rendered text alongside the `f64` (AHL-492). Checked
/// against sqlite3: parsing a document and re-emitting it preserves a
/// number's exact spelling rather than normalising it through a float —
/// `json('3.7777777777777777')` is `3.7777777777777777`, not the
/// fifteen-significant-digit `3.77777777777778` that
/// [`crate::eval::real_to_text`] (correct for `CAST(x AS TEXT)`) would give,
/// and `json('1.50')`/`json('1E5')` keep their trailing zero and capital `E`
/// rather than being normalised. `parse_number` below fills this field from
/// the matched source span, so it survives untouched; `crate::eval::json_leaf`
/// — the SQL-value-to-JSON direction used by `json_quote`/`json_array`/
/// `json_object` and friends, which has no source text to preserve because
/// there was never document text to begin with — fills it from
/// `real_to_text` instead, which keeps that direction's fifteen-digit
/// rendering exactly as it was before this type grew a second field.
/// Equality (below) compares only the `f64`: the text is presentation,
/// carried so `write` can reproduce it, not part of what the number *is*.
#[derive(Debug, Clone)]
pub enum Json {
    /// JSON `null` — distinct from "no such value"; see the functions in
    /// [`crate::eval`] for where a missing path is `None` instead.
    Null,
    /// JSON `true`/`false`.
    Bool(bool),
    /// A JSON number with no fractional part or exponent that fits an `i64`.
    Int(i64),
    /// Every other JSON number, alongside the exact text it renders as — see
    /// the type's doc comment for why a second field is here at all.
    Real(f64, String),
    /// A JSON string.
    Text(String),
    /// A JSON array, in document order.
    Array(Vec<Json>),
    /// A JSON object, in document order, duplicates and all.
    Object(Vec<(String, Json)>),
}

/// Numeric equality, not structural equality: two `Real`s with different
/// source text but the same `f64` — `1.50` parsed from a document and a
/// hand-built `1.5` — compare equal, because that is what every caller
/// comparing a `Json` means by it: `json_extract('{"a":1.50}','$.a') = 1.5`
/// is `1` against sqlite3, this module's own tests below compare parsed
/// documents against hand-built ones without reproducing the exact source
/// spelling, and `fuzz/fuzz_targets/json_parser.rs`'s round-trip assertion is
/// a semantic identity check, not a text diff. The text on `Real` exists
/// purely so `write` can reproduce it (see the type's doc comment) and
/// deliberately plays no part in equality.
impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Json::Null, Json::Null) => true,
            (Json::Bool(a), Json::Bool(b)) => a == b,
            (Json::Int(a), Json::Int(b)) => a == b,
            (Json::Real(a, _), Json::Real(b, _)) => a == b,
            (Json::Text(a), Json::Text(b)) => a == b,
            (Json::Array(a), Json::Array(b)) => a == b,
            (Json::Object(a), Json::Object(b)) => a == b,
            _ => false,
        }
    }
}

impl Json {
    /// SQLite's own lowercase `json_type()` name for this value's kind.
    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(true) => "true",
            Json::Bool(false) => "false",
            Json::Int(_) => "integer",
            Json::Real(_, _) => "real",
            Json::Text(_) => "text",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }
}

/// The parser and path parser only ever fail one way: the input did not
/// match the grammar. There is nothing more specific to report — the caller
/// already has the text that failed and writes its own message (`malformed
/// JSON`, `bad JSON path: '...'`) — so this carries no data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError;

/// One step of a parsed JSON path.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// `.key` or `."key"` — an object member.
    Key(String),
    /// `[..]` — an array element, in one of the three forms the module
    /// header documents.
    Index(IndexSpec),
}

/// The three spellings a bracketed path step can take.
#[derive(Debug, Clone, Copy, PartialEq)]
enum IndexSpec {
    /// `[N]` — a literal, non-negative index.
    Exact(usize),
    /// `[#]` — one past the last element: the append position.
    Append,
    /// `[#-N]` — `N` back from the append position.
    FromEnd(usize),
}

impl IndexSpec {
    /// The index this spec names in an array of length `len`, or `None` when
    /// it names nothing there — an out-of-range `[N]`, or `[#-N]` with `N`
    /// greater than `len`. The result may equal `len` itself (the append
    /// position); callers that only read an existing element reject that
    /// themselves.
    fn resolve(self, len: usize) -> Option<usize> {
        match self {
            IndexSpec::Exact(n) => Some(n),
            IndexSpec::Append => Some(len),
            IndexSpec::FromEnd(n) => len.checked_sub(n),
        }
    }
}

/// A parsed JSON path, ready to walk against a [`Json`] tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    steps: Vec<Step>,
}

/// Parse a JSON document from `text`, exactly as SQLite's `json1` functions
/// read their first argument: leading/trailing whitespace is skipped, the
/// text is one value and nothing else, and anything that does not parse is
/// `Err` — the caller turns that into `malformed JSON`, matching sqlite3's
/// own wording for `json_extract`/`->`/`->>`/`json()` (checked; `json_valid`
/// is the one caller that turns the same failure into `0` instead).
pub fn parse(text: &str) -> Result<Json, ParseError> {
    let bytes = text.as_bytes();
    let mut pos = skip_ws(bytes, 0);
    let (value, next) = parse_value(text, bytes, pos)?;
    pos = skip_ws(bytes, next);
    if pos != bytes.len() {
        return Err(ParseError);
    }
    Ok(value)
}

fn skip_ws(bytes: &[u8], mut pos: usize) -> usize {
    while matches!(bytes.get(pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        pos += 1;
    }
    pos
}

fn parse_value(text: &str, bytes: &[u8], pos: usize) -> Result<(Json, usize), ParseError> {
    let pos = skip_ws(bytes, pos);
    match bytes.get(pos) {
        Some(b'{') => parse_object(text, bytes, pos),
        Some(b'[') => parse_array(text, bytes, pos),
        Some(b'"') => {
            let (s, next) = parse_string(text, bytes, pos)?;
            Ok((Json::Text(s), next))
        }
        Some(b't') => literal(bytes, pos, "true", Json::Bool(true)),
        Some(b'f') => literal(bytes, pos, "false", Json::Bool(false)),
        Some(b'n') => literal(bytes, pos, "null", Json::Null),
        Some(b'-') | Some(b'0'..=b'9') => parse_number(text, bytes, pos),
        _ => Err(ParseError),
    }
}

fn literal(bytes: &[u8], pos: usize, word: &str, value: Json) -> Result<(Json, usize), ParseError> {
    let end = pos + word.len();
    if bytes.get(pos..end) == Some(word.as_bytes()) {
        Ok((value, end))
    } else {
        Err(ParseError)
    }
}

fn parse_number(text: &str, bytes: &[u8], start: usize) -> Result<(Json, usize), ParseError> {
    let mut pos = start;
    if bytes.get(pos) == Some(&b'-') {
        pos += 1;
    }
    match bytes.get(pos) {
        Some(b'0') => pos += 1,
        Some(b'1'..=b'9') => {
            while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
                pos += 1;
            }
        }
        _ => return Err(ParseError),
    }
    if bytes.get(pos) == Some(&b'.') {
        let digits_start = pos + 1;
        let mut p = digits_start;
        while bytes.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        if p == digits_start {
            return Err(ParseError);
        }
        pos = p;
    }
    if matches!(bytes.get(pos), Some(b'e' | b'E')) {
        let mut p = pos + 1;
        if matches!(bytes.get(p), Some(b'+' | b'-')) {
            p += 1;
        }
        let digits_start = p;
        while bytes.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        if p == digits_start {
            return Err(ParseError);
        }
        pos = p;
    }
    let span = &text[start..pos];
    // The same "try an integer, fall back to a float" rule
    // `sql::bind_literal` uses for a SQL numeric literal — see the type's
    // doc comment. A `Real` keeps `span` verbatim (AHL-492) so `write` can
    // reproduce it exactly rather than renormalising through `f64`.
    let value = match span.parse::<i64>() {
        Ok(i) => Json::Int(i),
        Err(_) => Json::Real(
            span.parse::<f64>().map_err(|_| ParseError)?,
            span.to_string(),
        ),
    };
    Ok((value, pos))
}

fn parse_string(text: &str, bytes: &[u8], start: usize) -> Result<(String, usize), ParseError> {
    debug_assert_eq!(bytes.get(start), Some(&b'"'));
    let mut pos = start + 1;
    let mut out = String::new();
    loop {
        match bytes.get(pos) {
            None => return Err(ParseError),
            Some(b'"') => return Ok((out, pos + 1)),
            Some(b'\\') => {
                pos += 1;
                match bytes.get(pos) {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'/') => out.push('/'),
                    Some(b'b') => out.push('\u{8}'),
                    Some(b'f') => out.push('\u{c}'),
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'u') => {
                        let cp = hex4(bytes, pos + 1)?;
                        pos += 4;
                        // A UTF-16 surrogate pair, spelled as two `\u`
                        // escapes back to back — the only way JSON can name
                        // a codepoint outside the basic multilingual plane.
                        if (0xD800..=0xDBFF).contains(&cp) {
                            if bytes.get(pos + 1) != Some(&b'\\')
                                || bytes.get(pos + 2) != Some(&b'u')
                            {
                                return Err(ParseError);
                            }
                            let low = hex4(bytes, pos + 3)?;
                            pos += 6;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err(ParseError);
                            }
                            let combined = 0x10000
                                + (u32::from(cp) - 0xD800) * 0x400
                                + (u32::from(low) - 0xDC00);
                            out.push(char::from_u32(combined).ok_or(ParseError)?);
                        } else {
                            out.push(char::from_u32(u32::from(cp)).ok_or(ParseError)?);
                        }
                    }
                    _ => return Err(ParseError),
                }
                pos += 1;
            }
            // A raw control character is invalid inside a JSON string; every
            // other byte, ASCII or the continuation bytes of a multi-byte
            // UTF-8 sequence, is copied through — the input is already valid
            // `&str`, so this never splits a codepoint.
            Some(&c) if c < 0x20 => return Err(ParseError),
            Some(_) => {
                let ch_start = pos;
                let ch = text[pos..].chars().next().ok_or(ParseError)?;
                pos = ch_start + ch.len_utf8();
                out.push(ch);
            }
        }
    }
}

fn hex4(bytes: &[u8], pos: usize) -> Result<u16, ParseError> {
    let digits = bytes.get(pos..pos + 4).ok_or(ParseError)?;
    let mut value: u16 = 0;
    for &b in digits {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return Err(ParseError),
        };
        value = value * 16 + u16::from(digit);
    }
    Ok(value)
}

fn parse_array(text: &str, bytes: &[u8], start: usize) -> Result<(Json, usize), ParseError> {
    debug_assert_eq!(bytes.get(start), Some(&b'['));
    let mut pos = skip_ws(bytes, start + 1);
    let mut items = Vec::new();
    if bytes.get(pos) == Some(&b']') {
        return Ok((Json::Array(items), pos + 1));
    }
    loop {
        let (value, next) = parse_value(text, bytes, pos)?;
        items.push(value);
        pos = skip_ws(bytes, next);
        match bytes.get(pos) {
            Some(b',') => pos = skip_ws(bytes, pos + 1),
            Some(b']') => return Ok((Json::Array(items), pos + 1)),
            _ => return Err(ParseError),
        }
    }
}

fn parse_object(text: &str, bytes: &[u8], start: usize) -> Result<(Json, usize), ParseError> {
    debug_assert_eq!(bytes.get(start), Some(&b'{'));
    let mut pos = skip_ws(bytes, start + 1);
    let mut members = Vec::new();
    if bytes.get(pos) == Some(&b'}') {
        return Ok((Json::Object(members), pos + 1));
    }
    loop {
        if bytes.get(pos) != Some(&b'"') {
            return Err(ParseError);
        }
        let (key, next) = parse_string(text, bytes, pos)?;
        pos = skip_ws(bytes, next);
        if bytes.get(pos) != Some(&b':') {
            return Err(ParseError);
        }
        pos = skip_ws(bytes, pos + 1);
        let (value, next) = parse_value(text, bytes, pos)?;
        members.push((key, value));
        pos = skip_ws(bytes, next);
        match bytes.get(pos) {
            Some(b',') => pos = skip_ws(bytes, pos + 1),
            Some(b'}') => return Ok((Json::Object(members), pos + 1)),
            _ => return Err(ParseError),
        }
    }
}

// --------------------------------------------------------------- serialize

/// Render `value` the way sqlite3 does: the shortest valid JSON, no
/// whitespace, `"`/`\`/control characters escaped (control characters below
/// `0x20` that have a short escape — `\b\f\n\r\t` — use it; the rest use
/// `\u00XX`), and everything else — including non-ASCII text and `0x7F` —
/// copied through raw. Checked against sqlite3: `json_quote('a/b')` keeps the
/// `/` unescaped, and `json_quote(char(127))` keeps the byte unescaped too.
///
/// A `Real` is rendered from its stored text, not recomputed from its `f64`
/// (AHL-492) — see [`Json`]'s doc comment for why the text, not the float, is
/// the source of truth here.
pub fn write(value: &Json) -> String {
    let mut out = String::new();
    write_into(value, &mut out);
    out
}

fn write_into(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Int(i) => {
            out.push_str(&i.to_string());
        }
        Json::Real(_, text) => out.push_str(text),
        Json::Text(s) => write_string(s, out),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_into(item, out);
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (i, (key, value)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_into(value, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ------------------------------------------------------------------- path

/// Parse a JSON path from `text`. See the module header for the accepted
/// grammar; anything outside it is `Err` — the caller turns that into
/// `bad JSON path: '<text>'`, sqlite3's own wording.
pub fn parse_path(text: &str) -> Result<Path, ParseError> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'$') {
        return Err(ParseError);
    }
    let mut pos = 1;
    let mut steps = Vec::new();
    while pos < bytes.len() {
        match bytes[pos] {
            b'.' => {
                pos += 1;
                if bytes.get(pos) == Some(&b'"') {
                    let (key, next) = parse_string(text, bytes, pos)?;
                    steps.push(Step::Key(key));
                    pos = next;
                } else {
                    let start = pos;
                    while pos < bytes.len() && bytes[pos] != b'.' && bytes[pos] != b'[' {
                        pos += 1;
                    }
                    if pos == start {
                        return Err(ParseError);
                    }
                    steps.push(Step::Key(text[start..pos].to_string()));
                }
            }
            b'[' => {
                pos += 1;
                let spec = if bytes.get(pos) == Some(&b'#') {
                    pos += 1;
                    if bytes.get(pos) == Some(&b'-') {
                        pos += 1;
                        let (n, next) = digits(bytes, pos)?;
                        pos = next;
                        IndexSpec::FromEnd(n)
                    } else {
                        IndexSpec::Append
                    }
                } else {
                    let (n, next) = digits(bytes, pos)?;
                    pos = next;
                    IndexSpec::Exact(n)
                };
                if bytes.get(pos) != Some(&b']') {
                    return Err(ParseError);
                }
                pos += 1;
                steps.push(Step::Index(spec));
            }
            _ => return Err(ParseError),
        }
    }
    Ok(Path { steps })
}

fn digits(bytes: &[u8], start: usize) -> Result<(usize, usize), ParseError> {
    let mut pos = start;
    while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
        pos += 1;
    }
    if pos == start {
        return Err(ParseError);
    }
    let n = core::str::from_utf8(&bytes[start..pos])
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or(ParseError)?;
    Ok((n, pos))
}

/// Whether this path is `$` alone.
pub fn is_root(path: &Path) -> bool {
    path.steps.is_empty()
}

/// The node at `path` within `doc`, or `None` when the path does not match —
/// a missing object key, an out-of-range or non-array-typed `[N]`, dot
/// notation against an array, or bracket notation against an object all
/// answer `None` here rather than an error; checked against sqlite3, which
/// answers `NULL` for every one of those, never an error.
pub fn get<'a>(doc: &'a Json, path: &Path) -> Option<&'a Json> {
    let mut current = doc;
    for step in &path.steps {
        current = match (step, current) {
            (Step::Key(key), Json::Object(members)) => &members.iter().find(|(k, _)| k == key)?.1,
            (Step::Index(spec), Json::Array(items)) => {
                let i = spec.resolve(items.len())?;
                items.get(i)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// The three mutating members of the `json_set`/`json_insert`/
/// `json_replace` family — they share one tree walk and differ only in
/// whether the final path element must already be there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutMode {
    /// `json_set` — write regardless of whether the target already exists.
    Set,
    /// `json_insert` — write only when the target does not already exist.
    Insert,
    /// `json_replace` — write only when the target already exists.
    Replace,
}

/// Write `value` at `path` within `doc`, following `mode`'s existence rule,
/// and return the new document.
///
/// A path that cannot be satisfied — an out-of-range array index that is not
/// `[#]`/`[#-N]`, dot notation against a non-object, `Replace` against a
/// missing target, `Insert` against an existing one — leaves `doc`
/// byte-for-byte unchanged, including any intermediate object/array this
/// call would otherwise have created: the walk is all-or-nothing, checked
/// against sqlite3 with `json_set('{}', '$.a[5]', 1)`, which creates
/// *nothing*, not even `$.a`, because `[5]` never resolves on the empty
/// array `$.a` would have to start as.
pub fn put(doc: &Json, path: &Path, value: &Json, mode: PutMode) -> Json {
    apply_put(Some(doc), &path.steps, value, mode).unwrap_or_else(|| doc.clone())
}

fn apply_put(current: Option<&Json>, steps: &[Step], value: &Json, mode: PutMode) -> Option<Json> {
    match steps.split_first() {
        None => match (current, mode) {
            (Some(_), PutMode::Insert) => None,
            (None, PutMode::Replace) => None,
            _ => Some(value.clone()),
        },
        Some((Step::Key(key), rest)) => match current {
            Some(Json::Object(members)) => {
                if let Some(pos) = members.iter().position(|(k, _)| k == key) {
                    let child = apply_put(Some(&members[pos].1), rest, value, mode)?;
                    let mut members = members.clone();
                    members[pos].1 = child;
                    Some(Json::Object(members))
                } else if mode == PutMode::Replace {
                    None
                } else {
                    let child = apply_put(None, rest, value, mode)?;
                    let mut members = members.clone();
                    members.push((key.clone(), child));
                    Some(Json::Object(members))
                }
            }
            None if mode != PutMode::Replace => {
                let child = apply_put(None, rest, value, mode)?;
                Some(Json::Object(alloc::vec![(key.clone(), child)]))
            }
            _ => None,
        },
        Some((Step::Index(spec), rest)) => match current {
            Some(Json::Array(items)) => {
                let len = items.len();
                let i = spec.resolve(len)?;
                if i < len {
                    let child = apply_put(Some(&items[i]), rest, value, mode)?;
                    let mut items = items.clone();
                    items[i] = child;
                    Some(Json::Array(items))
                } else if i == len && mode != PutMode::Replace {
                    let child = apply_put(None, rest, value, mode)?;
                    let mut items = items.clone();
                    items.push(child);
                    Some(Json::Array(items))
                } else {
                    None
                }
            }
            None if mode != PutMode::Replace && spec.resolve(0) == Some(0) => {
                let child = apply_put(None, rest, value, mode)?;
                Some(Json::Array(alloc::vec![child]))
            }
            _ => None,
        },
    }
}

/// Remove the node at `path` within `doc`, and return the new document, or
/// `None` when the path does not match anything (`doc` is unchanged) —
/// checked against sqlite3: `json_remove('{"a":1}', '$.b')` is `{"a":1}`,
/// not an error.
pub fn remove(doc: &Json, path: &Path) -> Option<Json> {
    apply_remove(doc, &path.steps)
}

fn apply_remove(current: &Json, steps: &[Step]) -> Option<Json> {
    match steps.split_first() {
        // `$` alone has no parent to remove itself from; the one caller that
        // reaches this (`json_remove(doc, '$')`) special-cases it into a
        // `NULL` result instead of calling this function — see `eval.rs`.
        None => None,
        Some((Step::Key(key), rest)) => match current {
            Json::Object(members) => {
                let pos = members.iter().position(|(k, _)| k == key)?;
                if rest.is_empty() {
                    let mut members = members.clone();
                    members.remove(pos);
                    Some(Json::Object(members))
                } else {
                    let child = apply_remove(&members[pos].1, rest)?;
                    let mut members = members.clone();
                    members[pos].1 = child;
                    Some(Json::Object(members))
                }
            }
            _ => None,
        },
        Some((Step::Index(spec), rest)) => match current {
            Json::Array(items) => {
                let len = items.len();
                let i = spec.resolve(len)?;
                if i >= len {
                    return None;
                }
                if rest.is_empty() {
                    let mut items = items.clone();
                    items.remove(i);
                    Some(Json::Array(items))
                } else {
                    let child = apply_remove(&items[i], rest)?;
                    let mut items = items.clone();
                    items[i] = child;
                    Some(Json::Array(items))
                }
            }
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Json {
        parse(text).unwrap_or_else(|_| panic!("{text} should have parsed"))
    }

    fn roundtrip(text: &str) {
        assert_eq!(write(&ok(text)), text);
    }

    #[test]
    fn scalars_round_trip() {
        roundtrip("null");
        roundtrip("true");
        roundtrip("false");
        roundtrip("0");
        roundtrip("-5");
        roundtrip("123456789012345");
        roundtrip("\"abc\"");
        roundtrip("[]");
        roundtrip("{}");
        roundtrip("[1,2,3]");
        roundtrip("{\"a\":1,\"b\":2}");
    }

    #[test]
    fn numbers_split_the_sqlite_way() {
        assert_eq!(ok("5"), Json::Int(5));
        assert_eq!(ok("-5"), Json::Int(-5));
        assert_eq!(ok("1.5"), Json::Real(1.5, "1.5".to_string()));
        assert_eq!(ok("1e3"), Json::Real(1000.0, "1e3".to_string()));
        // Overflows i64: falls back to Real, matching sqlite3's
        // `json_extract('99999999999999999999','$')`.
        assert!(matches!(ok("99999999999999999999"), Json::Real(_, _)));
    }

    // AHL-492: `json::write` used to re-render a `Real` through
    // `eval::real_to_text` — SQLite's fifteen-significant-digit `CAST(x AS
    // TEXT)` rule — which reparses to a *different* `f64` for a number that
    // needs more than fifteen digits to round-trip. `fuzz/fuzz_targets/
    // json_parser.rs` found this within seconds of CI's first run on this
    // 18-byte input: `3.7777777777777777` parses, but reserializing it
    // produced `3.77777777777778`, a different number. Every expectation
    // below was checked against a real sqlite3 3.54 binary, the same way
    // `crates/inlaysql/tests/sqllogictest/json.test` is: `json('<text>')` for
    // each of these is `<text>`, unchanged — SQLite preserves a parsed
    // number's exact source spelling rather than normalising it through a
    // float, and `write` now does the same.
    #[test]
    fn a_number_that_needs_more_than_fifteen_digits_round_trips_exactly() {
        // The 18-byte fuzz reproducer itself.
        roundtrip("3.7777777777777777");
    }

    #[test]
    fn json_source_spelling_survives_a_round_trip() {
        // A trailing zero is not noise — checked against sqlite3:
        // `json('1.50')` is `1.50`, not `1.5`.
        roundtrip("1.50");
        // A capital exponent marker is preserved too — `json('1E5')` is
        // `1E5`, not `1e5` or `100000`.
        roundtrip("1E5");
        // A three-digit exponent at the edge of `f64`'s range.
        roundtrip("1e308");
        // Negative zero keeps its sign — `json('-0.0')` is `-0.0`, not `0.0`.
        roundtrip("-0.0");
        // A few more shapes the fuzzer would plausibly turn up: a bare
        // exponent with no fractional part, an explicit `+` on the exponent,
        // a long run of trailing fractional zeros, and a subnormal.
        roundtrip("1e0");
        roundtrip("1E+5");
        roundtrip("2.50000000000000000");
        roundtrip("5e-324");
        // Nested inside a document, not just as the whole input — this is
        // the shape `json_extract`/`json_set` actually walk.
        roundtrip("[3.7777777777777777,1.50,-0.0]");
        roundtrip("{\"a\":1E5,\"b\":1e308}");
    }

    #[test]
    fn real_equality_is_numeric_not_textual() {
        // `1.50` and `1.5` are the same `f64` and this module's equality
        // says so — see `impl PartialEq for Json`'s doc comment — even
        // though `write` reproduces each one's own spelling.
        assert_eq!(ok("1.50"), ok("1.5"));
        assert_eq!(ok("1E5"), Json::Real(100_000.0, "100000".to_string()));
        assert_eq!(write(&ok("1.50")), "1.50");
        assert_eq!(write(&ok("1.5")), "1.5");
    }

    #[test]
    fn strings_escape_the_way_sqlite_does() {
        assert_eq!(write(&Json::Text("a/b".to_string())), "\"a/b\"");
        assert_eq!(write(&Json::Text("a\"b".to_string())), "\"a\\\"b\"");
        assert_eq!(write(&Json::Text("a\\b".to_string())), "\"a\\\\b\"");
        assert_eq!(
            write(&Json::Text("\t\n\r\0".to_string())),
            "\"\\t\\n\\r\\u0000\""
        );
        assert_eq!(write(&Json::Text("héllo".to_string())), "\"héllo\"");
        // 0x7F is not in JSON's mandatory escape range and sqlite3 leaves it
        // raw — `json_quote(char(127))`.
        assert_eq!(write(&Json::Text("\u{7f}".to_string())), "\"\u{7f}\"");
    }

    #[test]
    fn strict_json_rejects_json5() {
        assert!(parse("+5").is_err());
        assert!(parse("[1,2,]").is_err());
        assert!(parse("'abc'").is_err());
        assert!(parse("{a:1}").is_err());
        assert!(parse("0x1F").is_err());
        assert!(parse("not json").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn duplicate_keys_keep_the_first_on_lookup() {
        let doc = ok("{\"a\":1,\"a\":2}");
        let path = parse_path("$.a").unwrap();
        assert_eq!(get(&doc, &path), Some(&Json::Int(1)));
    }

    #[test]
    fn unquoted_key_reads_to_the_next_delimiter() {
        let doc = ok("{\"a b\":1}");
        let path = parse_path("$.a b").unwrap();
        assert_eq!(get(&doc, &path), Some(&Json::Int(1)));
    }

    #[test]
    fn bracket_and_dot_do_not_cross_types() {
        let arr = ok("[1,2,3]");
        assert_eq!(get(&arr, &parse_path("$.a").unwrap()), None);
        let obj = ok("{\"0\":1}");
        assert_eq!(get(&obj, &parse_path("$[0]").unwrap()), None);
        assert_eq!(get(&obj, &parse_path("$.0").unwrap()), Some(&Json::Int(1)));
    }

    #[test]
    fn negative_literal_index_is_a_bad_path() {
        assert!(parse_path("$[-1]").is_err());
    }

    #[test]
    fn append_and_from_end_resolve_against_the_length() {
        let arr = ok("[1,2,3]");
        let hash = parse_path("$[#]").unwrap();
        assert_eq!(get(&arr, &hash), None); // one past the end: nothing there yet
        let last = parse_path("$[#-1]").unwrap();
        assert_eq!(get(&arr, &last), Some(&Json::Int(3)));
    }

    #[test]
    fn set_replaces_an_existing_key_and_creates_a_missing_one() {
        let doc = ok("{\"a\":1,\"b\":2}");
        let path = parse_path("$.a").unwrap();
        let out = put(&doc, &path, &Json::Int(99), PutMode::Set);
        assert_eq!(write(&out), "{\"a\":99,\"b\":2}");

        let doc = ok("{\"a\":1}");
        let path = parse_path("$.b").unwrap();
        let out = put(&doc, &path, &Json::Int(99), PutMode::Set);
        assert_eq!(write(&out), "{\"a\":1,\"b\":99}");
    }

    #[test]
    fn insert_never_overwrites_and_replace_never_creates() {
        let doc = ok("{\"a\":1}");
        let existing = parse_path("$.a").unwrap();
        let missing = parse_path("$.b").unwrap();

        let out = put(&doc, &existing, &Json::Int(99), PutMode::Insert);
        assert_eq!(write(&out), "{\"a\":1}");
        let out = put(&doc, &missing, &Json::Int(99), PutMode::Insert);
        assert_eq!(write(&out), "{\"a\":1,\"b\":99}");

        let out = put(&doc, &existing, &Json::Int(99), PutMode::Replace);
        assert_eq!(write(&out), "{\"a\":99}");
        let out = put(&doc, &missing, &Json::Int(99), PutMode::Replace);
        assert_eq!(write(&out), "{\"a\":1}");
    }

    #[test]
    fn array_append_needs_hash_and_out_of_range_is_a_no_op() {
        let doc = ok("[1,2,3]");
        let out = put(
            &doc,
            &parse_path("$[9]").unwrap(),
            &Json::Int(99),
            PutMode::Set,
        );
        assert_eq!(write(&out), "[1,2,3]"); // unchanged: not `[#]`
        let out = put(
            &doc,
            &parse_path("$[#]").unwrap(),
            &Json::Int(99),
            PutMode::Set,
        );
        assert_eq!(write(&out), "[1,2,3,99]");
    }

    #[test]
    fn a_failed_write_creates_nothing_even_a_valid_prefix() {
        let doc = ok("{}");
        let out = put(
            &doc,
            &parse_path("$.a[5]").unwrap(),
            &Json::Int(1),
            PutMode::Set,
        );
        // `$.a` would have to be created as an empty array first, and index
        // 5 never resolves on that — checked against sqlite3, nothing is
        // created at all.
        assert_eq!(write(&out), "{}");
    }

    #[test]
    fn set_auto_vivifies_a_chain_but_never_crosses_a_scalar() {
        let doc = ok("{}");
        let out = put(
            &doc,
            &parse_path("$.a.b.c").unwrap(),
            &Json::Int(1),
            PutMode::Set,
        );
        assert_eq!(write(&out), "{\"a\":{\"b\":{\"c\":1}}}");

        let doc = ok("{\"a\":1}");
        let out = put(
            &doc,
            &parse_path("$.a.b").unwrap(),
            &Json::Int(99),
            PutMode::Set,
        );
        assert_eq!(write(&out), "{\"a\":1}"); // `a` is a scalar, not an object
    }

    #[test]
    fn remove_deletes_a_member_or_element_and_no_ops_a_miss() {
        let doc = ok("{\"a\":1,\"b\":2}");
        let out = remove(&doc, &parse_path("$.a").unwrap()).unwrap();
        assert_eq!(write(&out), "{\"b\":2}");
        assert_eq!(remove(&doc, &parse_path("$.z").unwrap()), None);

        let doc = ok("[1,2,3,4]");
        let out = remove(&doc, &parse_path("$[1]").unwrap()).unwrap();
        assert_eq!(write(&out), "[1,3,4]");
    }

    #[test]
    fn quoted_key_carries_characters_that_would_otherwise_be_path_syntax() {
        let doc = ok("{\"a.b[0]\":1}");
        let path = parse_path("$.\"a.b[0]\"").unwrap();
        assert_eq!(get(&doc, &path), Some(&Json::Int(1)));
    }

    #[test]
    fn a_path_not_starting_with_dollar_is_bad() {
        assert!(parse_path("a.b").is_err());
        assert!(parse_path("").is_err());
    }
}
