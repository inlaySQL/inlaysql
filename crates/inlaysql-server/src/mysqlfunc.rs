//! MySQL-named scalar functions, rewritten into the ones the engine has.
//!
//! This is decision **D1** in `docs/architecture.md` applied to the function library. The
//! engine's scalar functions carry SQLite's names and SQLite's semantics
//! (`length`, `substr`, `instr`, `datetime('now')`, `random`); a MySQL client
//! sends `CHAR_LENGTH`, `LEFT`, `LOCATE`, `NOW` and `RAND`. Nothing here adds a
//! function to the engine — each call site is rewritten, before the statement
//! reaches the engine, into an expression built only from what the engine
//! already has.
//!
//! # The rule this module is built around
//!
//! **A name is mapped only when the mapping was checked against a real MySQL
//! and found to be exact.** A wrong function result is the quietest failure a
//! database has: `LOCATE('a','banana')` answering `0` because the arguments
//! were passed in SQLite's order is not an error anyone sees, it is a row that
//! silently stops matching. So every name below is in exactly one of two lists:
//!
//! * **Mapped** — the rewritten expression was compared against MySQL 8.4 over
//!   a table of NULLs, empty strings, negative and zero arguments, positions
//!   past the end, and multi-byte UTF-8, and agreed on every one. Where it
//!   agrees on values but inherits an engine-wide difference — UTC instead of a
//!   session time zone, byte-for-byte instead of a case-insensitive collation,
//!   ASCII-only case folding — that difference is named in `docs/server.md`'s
//!   Divergences section, because it belongs to the engine and the mapping
//!   cannot remove it.
//! * **Refused** — a plausible mapping exists and is *wrong*. Each fails with
//!   `1235` and a message naming the function and the input that separates the
//!   two engines, so the refusal is a fact a reader can check rather than an
//!   apology.
//!
//! A name in neither list is left alone. The engine answers an unknown function
//! with `no such function: LPAD`, which is already a `1235` naming it — putting
//! a second copy of that list here would only give it somewhere else to rot.
//!
//! # Why these rewrites raise no warning
//!
//! [`crate::mysqlddl`] reports every clause it removes as a `1618` warning,
//! because dropping a clause changes what the statement asked for. A rewrite
//! here does not: the whole admission price for being in the mapped list is
//! that the expression means the same thing. Warning on every `NOW()` would put
//! noise in front of the warnings that do mean something.
//!
//! # Arguments are never duplicated
//!
//! Several mappings would be easy if the rewritten expression could mention an
//! argument twice — `RIGHT(s, n)` is `substr(s, -n, n)`. It cannot: an argument
//! may be a `?` placeholder, and a second `?` shifts every parameter index
//! after it. So a mapping that needs its argument twice is restricted to an
//! integer literal, which can be doubled at rewrite time, and refused
//! otherwise.

use crate::errors::MysqlError;
use crate::sqltext::{find_keyword, split_top_level, strip_keyword};

/// How deep a nest of calls this will rewrite before giving up.
///
/// The rewriter recurses through argument lists, so a pathological statement
/// could otherwise drive it into the stack guard. The limit is far above any
/// real query and the refusal is an ordinary `1235`.
const MAX_DEPTH: usize = 64;

/// Rewrite every MySQL-named scalar function call in `sql`.
///
/// `sql` must already have been through [`crate::sqltext::normalize`], so
/// comments are gone. A statement with no mapped call in it comes back
/// **byte-for-byte unchanged**: untouched spans are copied, not re-rendered, so
/// a bug in the mapping cannot reshape a statement it had no business touching.
pub fn rewrite(sql: &str) -> Result<String, MysqlError> {
    rewrite_span(sql, 0)
}

/// Rewrite one span of SQL text — a whole statement, or one argument of a call.
fn rewrite_span(text: &str, depth: usize) -> Result<String, MysqlError> {
    if depth > MAX_DEPTH {
        return Err(MysqlError::unsupported(
            "this statement nests function calls too deeply for the MySQL function shim",
        ));
    }

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    // The character before the identifier now being read, ignoring nothing:
    // `a.left(` is a qualified name and `@left(` is a variable, and neither is
    // a call to `LEFT`.
    let mut previous = '\0';

    while i < chars.len() {
        let c = chars[i];

        // Quoted spans are data, not syntax. A backtick-quoted `` `NOW` `` is a
        // column called NOW, and a string literal holding the text `NOW()` is a
        // value somebody is storing.
        if c == '\'' || c == '"' || c == '`' {
            let (span, next) = quoted_span(&chars, i);
            out.push_str(&span);
            i = next;
            previous = '\'';
            continue;
        }

        if is_word_start(c) {
            let start = i;
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // A word is a call when the next thing after it is `(`. MySQL
            // itself allows whitespace between the two — `CHAR_LENGTH ('abc')`
            // is 3 there — and so does the engine's parser, so the shim has to
            // as well or the two spellings would resolve differently.
            let mut open = i;
            while chars.get(open).is_some_and(|c| c.is_whitespace()) {
                open += 1;
            }
            // `a.left(...)` is a qualified name and `@left(...)` is a variable;
            // neither is a call to `LEFT`.
            let qualified = previous == '.' || previous == '@' || previous == '$';

            if chars.get(open) == Some(&'(') && !qualified {
                if let Some(end) = matching_paren(&chars, open) {
                    let inner: String = chars[open + 1..end].iter().collect();
                    if let Some(replacement) = map_call(&word, &inner, depth)? {
                        out.push_str(&replacement);
                        i = end + 1;
                        previous = ')';
                        continue;
                    }
                }
            }
            out.push_str(&word);
            previous = chars[i - 1];
            continue;
        }

        out.push(c);
        i += 1;
        if !c.is_whitespace() {
            previous = c;
        }
    }

    Ok(out)
}

/// Copy a quoted span whole, returning it and the index just past it.
fn quoted_span(chars: &[char], at: usize) -> (String, usize) {
    let quote = chars[at];
    let mut span = String::from(quote);
    let mut i = at + 1;
    while i < chars.len() {
        let c = chars[i];
        span.push(c);
        i += 1;
        // A backslash escape hides the next character, including a quote —
        // except inside backticks, where MySQL has no backslash escapes.
        if c == '\\' && quote != '`' && i < chars.len() {
            span.push(chars[i]);
            i += 1;
            continue;
        }
        if c == quote {
            // A doubled quote is an escaped quote, not the end of the span.
            if chars.get(i) == Some(&quote) {
                span.push(quote);
                i += 1;
                continue;
            }
            break;
        }
    }
    (span, i)
}

/// The index of the `)` closing the `(` at `open`, skipping quoted spans.
fn matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' | '`' => {
                let (_, next) = quoted_span(chars, i);
                i = next;
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ------------------------------------------------------------------ mapping

/// Rewrite one call, or leave it to the engine.
///
/// `Ok(None)` means "not this module's business" — the name is not one it maps,
/// and the call is copied through for the engine to answer or refuse itself.
fn map_call(name: &str, inner: &str, depth: usize) -> Result<Option<String>, MysqlError> {
    let upper = name.to_ascii_uppercase();

    // Two forms carry a keyword inside the parentheses rather than a comma-
    // separated list, so they are split before the argument list is.
    match upper.as_str() {
        "TRIM" => return trim_call(inner, depth),
        "POSITION" => return position_call(inner, depth),
        // `JSON_UNQUOTE(JSON_EXTRACT(doc, path))` (AHL-490) is the one shape
        // worth recognising structurally rather than by name: Laravel's own
        // `wrapJsonSelector` emits exactly this pair, never a bare
        // `JSON_UNQUOTE`, so this looks for the nested call rather than
        // adding `JSON_UNQUOTE` to the ordinary name-keyed dispatch below.
        "JSON_UNQUOTE" => return json_unquote_call(inner, depth),
        _ => {}
    }

    if !is_mapped_name(&upper) {
        return Ok(None);
    }

    let raw = split_top_level_arguments(inner);
    // The decision to leave a call alone is taken from the arguments *as
    // written*, before any of them are rewritten. Taking it afterwards would
    // rewrite the list once here and once again when the scan re-descends into
    // the call it declined — so `ROUND(ROUND(ROUND(…)))` would cost 2^depth.
    if engine_already_has_it(&upper, &raw)? {
        return Ok(None);
    }

    let mut args = Vec::with_capacity(raw.len());
    for arg in raw {
        args.push(rewrite_span(&arg, depth + 1)?);
    }
    scalar_call(&upper, &args)
}

/// Whether the engine already has this call, spelled and behaving the same way.
///
/// `COALESCE` with the two-or-more arguments the engine's own already takes,
/// and `ROUND` — except in the two shapes AHL-465 gave a primitive for: a
/// negative digit count, which `round()` clamps to zero, and a value written
/// as a MySQL `DOUBLE` literal — one with an exponent, `2.5e0` rather than
/// `2.5` — whose halfway case MySQL 8.4.11 ties to even where `round()` ties
/// away from zero. Every other shape (a plain decimal literal, an integer, a
/// column, an expression) is left alone on purpose: this shim has no catalog
/// access to know a column's declared type, and rewriting those too would
/// trade a rare divergence for a much more common one — MySQL's own manual
/// gives `ROUND(2.5)` (no exponent) as the *safe* spelling precisely because
/// it is the one this leaves untouched.
fn engine_already_has_it(upper: &str, raw: &[String]) -> Result<bool, MysqlError> {
    Ok(match upper {
        "COALESCE" => raw.len() != 1,
        "ROUND" => {
            let negative_digits =
                raw.len() == 2 && integer_literal(&raw[1]).is_some_and(|digits| digits < 0);
            let approximate = raw.first().is_some_and(|arg| is_double_literal(arg));
            !(negative_digits || approximate)
        }
        _ => false,
    })
}

/// Whether `text` is a plain numeric literal MySQL's own parser reads as an
/// approximate `DOUBLE` rather than an exact `DECIMAL` — an exponent is what
/// decides it: `2.5e0` and `25E-1` are `DOUBLE`, `2.5` and `-3` are
/// `DECIMAL`. A literal is the only shape this can prove anything about at
/// rewrite time; a column's declared type is not visible from here.
fn is_double_literal(text: &str) -> bool {
    let bytes = text.trim().as_bytes();
    let mut i = 0;
    if let Some(&sign) = bytes.first() {
        if sign == b'+' || sign == b'-' {
            i += 1;
        }
    }
    let mut saw_digit = false;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        saw_digit = true;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            saw_digit = true;
        }
    }
    if !saw_digit || !matches!(bytes.get(i), Some(b'e' | b'E')) {
        return false;
    }
    i += 1;
    if matches!(bytes.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let exponent_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    i > exponent_start && i == bytes.len()
}

/// Whether this name is one the module has an opinion about.
///
/// Everything else is copied through untouched, including the SQLite-named
/// functions the engine already has.
fn is_mapped_name(upper: &str) -> bool {
    matches!(
        upper,
        // Mapped.
        "CONCAT"
            | "CHAR_LENGTH"
            | "CHARACTER_LENGTH"
            | "UCASE"
            | "LCASE"
            | "LOCATE"
            | "LEFT"
            | "RIGHT"
            | "ISNULL"
            | "IF"
            | "COALESCE"
            | "RAND"
            | "NOW"
            | "LOCALTIME"
            | "LOCALTIMESTAMP"
            | "UTC_TIMESTAMP"
            | "CURDATE"
            | "UTC_DATE"
            | "CURTIME"
            | "UTC_TIME"
            | "UNIX_TIMESTAMP"
            | "FROM_UNIXTIME"
            | "YEAR"
            | "MONTH"
            | "DAY"
            | "DAYOFMONTH"
            | "HOUR"
            | "MINUTE"
            | "SECOND"
            | "DAYOFWEEK"
            | "WEEKDAY"
            | "DAYOFYEAR"
            | "QUARTER"
            | "LAST_DAY"
            | "ROUND"
            // Onto the AHL-465 primitives: five spellings the engine already
            // resolves under SQLite's semantics, mapped so the MySQL spelling
            // gets MySQL's measured behaviour instead. `docs/server.md`'s
            // Divergences section named the corners; the primitives are in
            // `inlaysql-core`'s `eval.rs`, prefixed `mysql_` and documented as
            // shim-target-only.
            | "LENGTH"
            | "HEX"
            | "SUBSTR"
            | "SUBSTRING"
            | "NULLIF"
            // `MID` is MySQL's alias for `SUBSTRING`; it used to be refused
            // deliberately rather than given a second spelling for a
            // divergence, but `SUBSTRING` is not a divergence any more, so
            // there is nothing left for the refusal to guard.
            | "MID"
            // `OCTET_LENGTH` counts bytes and `BIT_LENGTH` counts bits — both
            // now expressible over `octet_length()`, which used to not exist.
            | "OCTET_LENGTH"
            | "BIT_LENGTH"
            // Refused with a reason, rather than left to the engine's plainer
            // `no such function`.
            | "CONCAT_WS"
            | "GREATEST"
            | "LEAST"
            | "MOD"
            | "SYSDATE"
            | "DATE_FORMAT"
            | "STR_TO_DATE"
            | "TIME_FORMAT"
            | "DATEDIFF"
            | "TIMEDIFF"
            | "TIMESTAMPDIFF"
            | "DATE_ADD"
            | "DATE_SUB"
            | "ADDDATE"
            | "SUBDATE"
            | "MONTHNAME"
            | "DAYNAME"
            | "WEEK"
            | "WEEKOFYEAR"
            | "YEARWEEK"
            // JSON (AHL-490). `JSON_EXTRACT`, `JSON_SET`, `JSON_INSERT`,
            // `JSON_REPLACE`, `JSON_REMOVE`, `JSON_VALID`, `JSON_ARRAY`,
            // `JSON_OBJECT` and `JSON()` are not in this list at all: they
            // are spelled identically in both dialects and the engine's own
            // function lookup is already case-insensitive
            // (`sql.rs::resolve_scalar_function`), so a MySQL client's
            // `JSON_SET(...)` reaches the same code a SQLite client's
            // `json_set(...)` does with no shim involvement — see
            // `docs/server.md`'s Divergences section for the corners that
            // are still worth naming (the array-append rule, and the
            // whitespace MySQL's own JSON functions add that these do not).
            // `JSON_LENGTH` and `JSON_CONTAINS_PATH` are mapped below.
            // `JSON_QUOTE` and `JSON_TYPE` *are* listed here specifically so
            // a MySQL client reaches this module's refusal instead of the
            // engine's own same-named function under SQLite's different
            // rules — the one shape this module exists to prevent.
            | "JSON_LENGTH"
            | "JSON_CONTAINS_PATH"
            | "JSON_QUOTE"
            | "JSON_TYPE"
            | "JSON_CONTAINS"
            | "JSON_OVERLAPS"
    )
}

/// Split an argument list, without rewriting any of it.
fn split_top_level_arguments(inner: &str) -> Vec<String> {
    if inner.trim().is_empty() {
        return Vec::new();
    }
    split_top_level(inner, ',')
}

/// The whole of `text` as an integer literal, if that is what it is.
///
/// This is what separates a mapping that can be proved at rewrite time from one
/// that cannot: `RIGHT(s, 3)` becomes `substr(s, -3, 3)` because `3` can be
/// negated and written twice, and `RIGHT(s, ?)` cannot become anything, because
/// a second `?` would shift every parameter after it and a `NULL` length means
/// something different in each engine.
fn integer_literal(text: &str) -> Option<i64> {
    let text = text.trim();
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1i64, rest.trim_start()),
        None => (1i64, text.strip_prefix('+').unwrap_or(text).trim_start()),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().map(|value| sign * value)
}

/// MySQL's own `ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT`, with MySQL's own wording —
/// checked against 8.4, which answers exactly this for `CHAR_LENGTH('a','b')`.
fn wrong_arity(name: &str) -> MysqlError {
    MysqlError::new(
        1582,
        "42000",
        format!("Incorrect parameter count in the call to native function '{name}'"),
    )
}

/// The `substr` call that yields MySQL's empty string while still answering
/// `NULL` for a `NULL` subject — `substr(s, 0, 0)`, checked against both.
fn empty_slice(subject: &str) -> String {
    format!("substr({subject}, 0, 0)")
}

/// Rewrite one comma-separated call, or leave it alone.
///
/// `Ok(None)` is "the engine already has this, spelled and behaving the same
/// way" — `ROUND(x)` and a two-or-more-argument `COALESCE` reach it. Returning
/// the call unchanged rather than re-rendering it keeps the promise that a
/// statement with nothing to translate arrives at the engine byte for byte.
fn scalar_call(upper: &str, args: &[String]) -> Result<Option<String>, MysqlError> {
    let n = args.len();
    let mapped =
        match upper {
            // ------------------------------------------------------------ strings

            // `CONCAT` is NULL-propagating in MySQL — `CONCAT('a', NULL, 'c')` is
            // NULL, not 'ac' — and so is `||`. The leading `''` is not decoration:
            // it makes the result TEXT for every arity, so `CONCAT(1.5)` is the
            // string '1.5' as it is in MySQL rather than the number 1.5.
            "CONCAT" => {
                if n == 0 {
                    return Err(wrong_arity("CONCAT"));
                }
                format!("('' || {})", args.join(" || "))
            }

            "CHAR_LENGTH" | "CHARACTER_LENGTH" => {
                if n != 1 {
                    return Err(wrong_arity(upper));
                }
                format!("length({})", args[0])
            }

            "UCASE" | "LCASE" => {
                if n != 1 {
                    return Err(wrong_arity(upper));
                }
                let target = if upper == "UCASE" { "upper" } else { "lower" };
                format!("{target}({})", args[0])
            }

            // The argument order is the whole point: MySQL's `LOCATE(needle,
            // haystack)` is the reverse of SQLite's `instr(haystack, needle)`, and
            // getting it backwards is a wrong answer with no error attached.
            "LOCATE" => {
                match n {
                    2 => format!("instr({}, {})", args[1], args[0]),
                    3 => return Err(MysqlError::unsupported(
                        "LOCATE(substr, str, pos) is not mapped: InlaySQL's instr() searches from \
                     the start and has no third argument to start from",
                    )),
                    _ => return Err(wrong_arity(upper)),
                }
            }

            "LEFT" | "RIGHT" => {
                if n != 2 {
                    return Err(wrong_arity(upper));
                }
                let Some(len) = integer_literal(&args[1]) else {
                    return Err(MysqlError::unsupported(format!(
                        "{upper}(str, len) is mapped only when `len` is an integer literal: MySQL \
                     answers NULL for a NULL length where InlaySQL's substr() answers the empty \
                     string, and RIGHT() needs the length twice, which would duplicate a `?` \
                     placeholder"
                    )));
                };
                if len <= 0 {
                    // MySQL returns the empty string for a zero or negative length,
                    // and NULL when the subject is NULL. `substr(s, 0, 0)` is both.
                    empty_slice(&args[0])
                } else if upper == "LEFT" {
                    format!("substr({}, 1, {len})", args[0])
                } else {
                    format!("substr({}, -{len}, {len})", args[0])
                }
            }

            // `LENGTH` counts bytes in MySQL and characters in SQLite's own
            // `length()` — `octet_length()` (AHL-465) is the byte count. Use
            // `CHAR_LENGTH`/`CHARACTER_LENGTH` for the character count.
            "LENGTH" => {
                if n != 1 {
                    return Err(wrong_arity(upper));
                }
                format!("octet_length({})", args[0])
            }

            // Both count bytes; `BIT_LENGTH` is the same count times eight —
            // exact in both engines, no corner to check.
            "OCTET_LENGTH" => {
                if n != 1 {
                    return Err(wrong_arity(upper));
                }
                format!("octet_length({})", args[0])
            }
            "BIT_LENGTH" => {
                if n != 1 {
                    return Err(wrong_arity(upper));
                }
                format!("(octet_length({}) * 8)", args[0])
            }

            // `hex()` answers `''` for `NULL` (it asks for the value's bytes,
            // and `NULL` has none) and renders a number as the bytes of its
            // *text*; MySQL's `HEX()` answers `NULL` for `NULL` and renders a
            // number as the hex of its *value* — `mysql_hex()` (AHL-465).
            "HEX" => {
                if n != 1 {
                    return Err(wrong_arity(upper));
                }
                format!("mysql_hex({})", args[0])
            }

            // `SUBSTRING`/`SUBSTR` position `0` (or anywhere out of range) is
            // the empty string in MySQL and the whole/clamped string under
            // SQLite's `substr()`; `MID` is MySQL's own alias for the same
            // function. `mysql_substr()` (AHL-465) is the primitive with
            // MySQL's rule.
            "SUBSTR" | "SUBSTRING" | "MID" => match n {
                2 => format!("mysql_substr({}, {})", args[0], args[1]),
                3 => format!("mysql_substr({}, {}, {})", args[0], args[1], args[2]),
                _ => return Err(wrong_arity(upper)),
            },

            // `nullif()` compares by storage class, so `NULLIF(1, '1')` is `1`
            // here; MySQL's `=` coerces a string against a number, so it is
            // `NULL` there. `mysql_nullif()` (AHL-465) has that comparison.
            "NULLIF" => {
                if n != 2 {
                    return Err(wrong_arity(upper));
                }
                format!("mysql_nullif({}, {})", args[0], args[1])
            }

            // Reached only for the two shapes `engine_already_has_it` picked
            // out: a negative digit count, or a value written as a MySQL
            // `DOUBLE` literal. Every other `ROUND` was already declined
            // before its arguments were even rewritten.
            "ROUND" => match n {
                1 => format!("mysql_round({})", args[0]),
                2 => format!("mysql_round({}, {})", args[0], args[1]),
                _ => return Err(wrong_arity(upper)),
            },

            // ------------------------------------------------------- conditionals
            "ISNULL" => {
                if n != 1 {
                    return Err(wrong_arity(upper));
                }
                format!("({} IS NULL)", args[0])
            }

            // MySQL's truthiness — `IF('abc', ...)` takes the false branch,
            // `IF('1', ...)` the true one, `IF(NULL, ...)` the false one — is
            // exactly what the engine's `CASE WHEN` already does, checked against
            // both.
            "IF" => {
                if n != 3 {
                    return Err(wrong_arity(upper));
                }
                format!(
                    "CASE WHEN {} THEN {} ELSE {} END",
                    args[0], args[1], args[2]
                )
            }

            // MySQL accepts a one-argument `COALESCE`; the engine's wants at
            // least two. With one argument it is the identity. Every other
            // arity was already settled by `engine_already_has_it`.
            "COALESCE" => match n {
                1 => format!("({})", args[0]),
                _ => return Ok(None),
            },

            // ------------------------------------------------------------ numeric

            // `random()` is SQLite's: a signed 64-bit integer, never `i64::MIN`.
            // Dividing its magnitude by 2^63 gives MySQL's contract, a double in
            // [0, 1). It is a different generator producing a different stream —
            // which is what `RAND()` promises — but the same range.
            "RAND" => match n {
                0 => "(abs(random()) / 9223372036854775808.0)".to_string(),
                _ => return Err(MysqlError::unsupported(
                    "RAND(seed) is not mapped: it promises MySQL's own seeded sequence, and no \
                     expression over InlaySQL's random() reproduces it",
                )),
            },

            // ---------------------------------------------------------- date/time

            // Every one of these reads the engine's clock, which is UTC. MySQL
            // reads the session time zone. See `docs/server.md`, "Divergences".
            "NOW" | "LOCALTIME" | "LOCALTIMESTAMP" | "UTC_TIMESTAMP" => {
                no_precision(upper, n)?;
                "datetime('now')".to_string()
            }
            "CURDATE" | "UTC_DATE" => {
                no_precision(upper, n)?;
                "date('now')".to_string()
            }
            "CURTIME" | "UTC_TIME" => {
                no_precision(upper, n)?;
                "time('now')".to_string()
            }

            "UNIX_TIMESTAMP" => match n {
                0 => "unixepoch('now')".to_string(),
                _ => return Err(MysqlError::unsupported(
                    "UNIX_TIMESTAMP(date) is not mapped: MySQL reads the argument in the session \
                     time zone, accepts formats InlaySQL's unixepoch() does not, and answers 0 \
                     rather than NULL for one it cannot read",
                )),
            },

            // The date parts. `strftime` answers text with a leading zero, so each
            // is cast back to the integer MySQL returns — and a `NULL` or an
            // unreadable date stays `NULL` all the way through the cast.
            "YEAR" => date_part(upper, args, "%Y")?,
            "MONTH" => date_part(upper, args, "%m")?,
            "DAY" | "DAYOFMONTH" => date_part(upper, args, "%d")?,
            "HOUR" => date_part(upper, args, "%H")?,
            "MINUTE" => date_part(upper, args, "%M")?,
            "SECOND" => date_part(upper, args, "%S")?,
            "DAYOFYEAR" => date_part(upper, args, "%j")?,

            // SQLite's `%w` is 0 for Sunday. MySQL's DAYOFWEEK is 1 for Sunday and
            // its WEEKDAY is 0 for Monday, so each needs its own shift.
            "DAYOFWEEK" => {
                one_date_argument(upper, args)?;
                format!("(CAST(strftime('%w', {}) AS INTEGER) + 1)", args[0])
            }
            "WEEKDAY" => {
                one_date_argument(upper, args)?;
                format!("((CAST(strftime('%w', {}) AS INTEGER) + 6) % 7)", args[0])
            }
            "QUARTER" => {
                one_date_argument(upper, args)?;
                format!("((CAST(strftime('%m', {}) AS INTEGER) + 2) / 3)", args[0])
            }
            "LAST_DAY" => {
                one_date_argument(upper, args)?;
                format!("date({}, 'start of month', '+1 month', '-1 day')", args[0])
            }

            // ---------------------------------------------------------------- JSON

            // MySQL's `JSON_LENGTH` counts an object's *members* as well as
            // an array's elements (`JSON_LENGTH('{"a":1}')` is `1`) and a
            // scalar's length is `1`, not `0` — checked against a real
            // MySQL 8 container. `json_array_length()` answers `0` for both
            // of those (verified against sqlite3), so this diverges for
            // anything but an array — documented in `docs/server.md` rather
            // than refused, because the array case (`whereJsonLength`'s
            // whole reason to exist) is exact, and refusing the mapping
            // outright would lose that for a divergence that only bites the
            // less common shape.
            "JSON_LENGTH" => match n {
                1 => format!("json_array_length({})", args[0]),
                2 => format!("json_array_length({}, {})", args[0], args[1]),
                _ => return Err(wrong_arity(upper)),
            },

            // `JSON_CONTAINS_PATH(doc, 'one'|'all', path, ...)` has no
            // InlaySQL primitive; the one shape mapped is the one Laravel's
            // own query builder emits (`whereJsonContainsKey`) — a single
            // path, `'one'` mode, wrapped by the caller's own
            // `ifnull(..., 0)` — and it is mapped onto exactly the
            // rewrite Laravel's *own* SQLite grammar uses for the same
            // clause (`SQLiteGrammar::compileJsonContainsKey`,
            // `json_type(field, path) is not null`), not a guess: SQLite
            // has no `json_contains_path` either. `'all'` mode or more than
            // one path would need one `json_type()` check per path, ANDed
            // or ORed by the mode — more than a name mapping — so those are
            // refused.
            "JSON_CONTAINS_PATH" => {
                if n != 3 {
                    return Err(MysqlError::unsupported(
                        "JSON_CONTAINS_PATH is mapped only for the exact shape Laravel's query \
                         builder emits: a document, the literal 'one', and a single path"
                            .to_string(),
                    ));
                }
                if args[1].trim() != "'one'" {
                    return Err(MysqlError::unsupported(
                        "JSON_CONTAINS_PATH is mapped only for 'one' mode, matching Laravel's \
                         own whereJsonContainsKey; 'all' mode needs every path to match, which \
                         is not one json_type() check"
                            .to_string(),
                    ));
                }
                format!("(json_type({}, {}) IS NOT NULL)", args[0], args[2])
            }

            // ---------------------------------------------------------- refusals
            //
            // Each of these has a mapping that looks right and is not. The input
            // that separates the two engines is in the message, so the refusal can
            // be checked rather than taken on trust.
            other => return Err(refused(other)),
        };

    Ok(Some(mapped))
}

/// The names this module considered and rejected, each with the input that
/// decided it.
///
/// These would all reach the engine as `no such function`, which is already a
/// `1235` naming them. They are refused here instead so the message says *why*
/// the obvious mapping is not there — otherwise the next reader re-derives the
/// same wrong mapping from the same documentation.
fn refused(upper: &str) -> MysqlError {
    let reason = match upper {
        "CONCAT_WS" => "it skips NULL arguments — CONCAT_WS('-','a',NULL,'c') is 'a-c' — and \
                        every concatenation InlaySQL has propagates NULL instead"
            .to_string(),
        "GREATEST" | "LEAST" => {
            "MySQL compares numerically as soon as one argument is a number — GREATEST(2, '10') \
             is 2 and LEAST(2, '10') is '10' — where InlaySQL's max()/min() compare by storage \
             class and would answer the other way round"
                .to_string()
        }
        "MOD" => "MySQL keeps the fraction — MOD(5.5, 2) is 1.5 — and InlaySQL's `%` operator \
                  truncates both sides to integers, which would answer 1"
            .to_string(),
        "SYSDATE" => "it is the clock at the moment of the call, where NOW() and InlaySQL's \
                      datetime('now') are both fixed for the whole statement — use NOW()"
            .to_string(),
        // The obvious mapping is `datetime(n, 'unixepoch')`, and it was written
        // and then taken out: the two disagree at both ends of the range, and
        // the argument is a column, so there is no rewrite-time check to make.
        "FROM_UNIXTIME" => {
            "MySQL answers NULL outside 0 .. 32536771199 and InlaySQL's datetime(n, 'unixepoch') \
             keeps counting — FROM_UNIXTIME(-1) is NULL in MySQL and '1969-12-31 23:59:59' here"
                .to_string()
        }
        "DATE_FORMAT" | "STR_TO_DATE" | "TIME_FORMAT" => {
            "MySQL's format specifiers are not strftime's — its %i is minutes where strftime's \
             is nothing, and its %s is seconds where strftime's is the Unix epoch"
                .to_string()
        }
        "DATEDIFF" | "TIMEDIFF" | "TIMESTAMPDIFF" => {
            "it needs a day or second count between two moments, and the engine has no \
             julianday() to subtract"
                .to_string()
        }
        "DATE_ADD" | "DATE_SUB" | "ADDDATE" | "SUBDATE" => {
            "its INTERVAL argument is MySQL syntax with no expression behind it here".to_string()
        }
        "MONTHNAME" | "DAYNAME" => {
            "it answers a locale-dependent name, and strftime() has no month or day names at all"
                .to_string()
        }
        "WEEK" | "WEEKOFYEAR" | "YEARWEEK" => {
            "MySQL has eight week-numbering modes selected by a mode argument and by \
             default_week_format, and strftime's %W is only one of them"
                .to_string()
        }
        "JSON_QUOTE" => "MySQL requires its argument to be a string and errors otherwise \
                         (`JSON_QUOTE(1)` is `Incorrect type for argument 1 in function \
                         json_quote`, checked against a real MySQL 8 container), where \
                         InlaySQL's json_quote() accepts any scalar and renders a number as a \
                         bare JSON number, not a quoted string — and this shim has no catalog \
                         access to know a column's declared type"
            .to_string(),
        "JSON_TYPE" => "MySQL answers uppercase names with no exact overlap with SQLite's — \
                        OBJECT/ARRAY/STRING/INTEGER/DOUBLE/BOOLEAN/NULL where json_type() \
                        answers object/array/text/integer/real/true/false/null — and MySQL's \
                        `true`/`false` collapse into one BOOLEAN where SQLite keeps them apart, \
                        so no rewrite recovers both at once"
            .to_string(),
        "JSON_CONTAINS" => "it asks whether one JSON document contains another as a value or \
                            subset, which needs a set-membership test over a document's \
                            elements/members; InlaySQL has no primitive for that without \
                            json_each(), which is table-valued and this engine has no \
                            mechanism for"
            .to_string(),
        "JSON_OVERLAPS" => "the same reason as JSON_CONTAINS: it needs a set-intersection test \
                            InlaySQL has no primitive for"
            .to_string(),
        other => unreachable!("{other} is listed as mapped but has no arm"),
    };
    MysqlError::unsupported(format!("{upper}() is not mapped: {reason}"))
}

/// Refuse the fractional-second precision argument the clock functions take.
fn no_precision(upper: &str, n: usize) -> Result<(), MysqlError> {
    if n == 0 {
        return Ok(());
    }
    Err(MysqlError::unsupported(format!(
        "{upper}(precision) is not mapped: MySQL returns 0 to 6 fractional digits and the \
         engine's clock renders whole seconds"
    )))
}

fn one_date_argument(upper: &str, args: &[String]) -> Result<(), MysqlError> {
    if args.len() == 1 {
        Ok(())
    } else {
        Err(wrong_arity(upper))
    }
}

fn date_part(upper: &str, args: &[String], specifier: &str) -> Result<String, MysqlError> {
    one_date_argument(upper, args)?;
    Ok(format!(
        "CAST(strftime('{specifier}', {}) AS INTEGER)",
        args[0]
    ))
}

// --------------------------------------------------------- keyword-in-parens

/// `TRIM([{BOTH | LEADING | TRAILING}] [remstr] FROM str)`, and `TRIM(str)`.
///
/// The forms without a `remstr` are exact: MySQL and SQLite both strip spaces
/// and nothing else. The forms *with* one are refused, because MySQL removes
/// the whole substring from each end and SQLite's second argument is a set of
/// characters — `TRIM(BOTH 'xy' FROM 'yxhixy')` is `'yxhi'` in MySQL and
/// `'hi'` under a character set.
fn trim_call(inner: &str, depth: usize) -> Result<Option<String>, MysqlError> {
    let Some(at) = find_keyword(inner, "from") else {
        // No `FROM`: this is `trim(x)`, or the engine's own `trim(x, chars)`.
        // Both are already the engine's, so the call is left exactly as
        // written and the scan descends into it for nested calls.
        return Ok(None);
    };

    let head = inner[..at].trim();
    // `BOTH`/`LEADING`/`TRAILING` may be followed by the string to remove; a
    // head that is anything other than one of those three bare words has one.
    let target = match head.to_ascii_uppercase().as_str() {
        "" | "BOTH" => "trim",
        "LEADING" => "ltrim",
        "TRAILING" => "rtrim",
        _ => {
            return Err(MysqlError::unsupported(format!(
                "TRIM({head} FROM ...) is not mapped: MySQL removes that whole string from each \
                 end, and InlaySQL's trim() takes a *set of characters* instead — \
                 TRIM(BOTH 'xy' FROM 'yxhixy') is 'yxhi' in MySQL and 'hi' here"
            )))
        }
    };
    let subject = rewrite_span(inner[at + 4..].trim(), depth + 1)?;
    Ok(Some(format!("{target}({subject})")))
}

/// `POSITION(substr IN str)` — the same swap `LOCATE` needs.
///
/// Anything without a top-level `IN` is not this form, so it is left for the
/// engine to refuse in its own words rather than guessed at here.
fn position_call(inner: &str, depth: usize) -> Result<Option<String>, MysqlError> {
    let Some(at) = find_keyword(inner, "in") else {
        return Ok(None);
    };
    let needle = rewrite_span(inner[..at].trim(), depth + 1)?;
    let haystack = rewrite_span(inner[at + 2..].trim(), depth + 1)?;
    Ok(Some(format!("instr({haystack}, {needle})")))
}

/// `JSON_UNQUOTE(JSON_EXTRACT(doc, path))` (AHL-490) — the shape Laravel's
/// `wrapJsonSelector` emits for a plain JSON path selector — becomes
/// `(doc ->> path)`: the same node, unwrapped to its SQL value, which is
/// exactly what `JSON_UNQUOTE(JSON_EXTRACT(...))` means in MySQL (its own
/// `->>` is defined as that same pair). A `JSON_UNQUOTE` call that is not
/// wrapping exactly one `JSON_EXTRACT(doc, path)` call is refused: MySQL's
/// own `JSON_UNQUOTE` on an arbitrary value strips one layer of JSON string
/// quoting if the value looks like a quoted JSON string and leaves it alone
/// otherwise, and InlaySQL has no primitive for that in isolation.
fn json_unquote_call(inner: &str, depth: usize) -> Result<Option<String>, MysqlError> {
    if let Some(rest) = strip_keyword(inner, "JSON_EXTRACT") {
        if rest.starts_with('(') {
            let chars: Vec<char> = rest.chars().collect();
            if let Some(end) = matching_paren(&chars, 0) {
                let trailing: String = chars[end + 1..].iter().collect();
                if trailing.trim().is_empty() {
                    let call_inner: String = chars[1..end].iter().collect();
                    let call_args = split_top_level_arguments(&call_inner);
                    if call_args.len() == 2 {
                        let doc = rewrite_span(&call_args[0], depth + 1)?;
                        let path = rewrite_span(&call_args[1], depth + 1)?;
                        return Ok(Some(format!("({doc} ->> {path})")));
                    }
                }
            }
        }
    }
    Err(MysqlError::unsupported(
        "JSON_UNQUOTE(x) is not mapped outside JSON_UNQUOTE(JSON_EXTRACT(doc, path)) — the \
         shape Laravel's query builder emits for a JSON path selector — because a bare \
         JSON_UNQUOTE has no InlaySQL primitive that strips exactly one level of JSON string \
         quoting from an arbitrary value"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rewritten statement, or a panic naming the refusal.
    fn out(sql: &str) -> String {
        rewrite(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error}"))
    }

    /// The error of a refusal, or a panic if there was not one.
    fn refusal(sql: &str) -> MysqlError {
        match rewrite(sql) {
            Err(error) => error,
            Ok(rewritten) => panic!("{sql} was rewritten to {rewritten}, not refused"),
        }
    }

    /// Assert a refusal is a `1235` whose message contains `needle`.
    fn refuses(sql: &str, needle: &str) {
        let error = refusal(sql);
        assert_eq!(error.code, 1235, "{sql}: {}", error.message);
        assert!(
            error.message.contains(needle),
            "{sql}: the message must name `{needle}`, got: {}",
            error.message
        );
    }

    // ------------------------------------------------------- nothing to do

    /// The promise the whole module rests on: a statement with no mapped call
    /// in it is not re-rendered, it is handed back exactly as it arrived.
    #[test]
    fn a_statement_with_nothing_to_map_comes_back_byte_for_byte() {
        for sql in [
            "SELECT 1",
            "SELECT id, body FROM docs WHERE id = ?",
            "INSERT INTO docs (id) VALUES (1)",
            "UPDATE docs SET body = 'x' WHERE id = 2",
            "DELETE FROM docs",
            "CREATE TABLE t (a INTEGER)",
            // The engine's own function library keeps its own spelling —
            // `instr` has no divergence to fix, unlike `length`/`substr`
            // (AHL-465), which are mapped now regardless of what their
            // argument is: see
            // `length_hex_substring_nullif_and_round_map_onto_the_ahl465_primitives`.
            "SELECT instr(body, 'x') FROM docs",
            "SELECT lower(a), upper(b), trim(c), ltrim(d), rtrim(e) FROM t",
            "SELECT coalesce(a, b), ifnull(a, b) FROM t",
            // `round(x)` over a column is still the engine's own — AHL-465
            // maps `ROUND` only for a `DOUBLE` literal or a negative digit
            // count, neither of which this is. See
            // `round_is_mapped_only_for_a_double_literal_or_a_negative_digit_count`.
            "SELECT round(x), round(x, 2) FROM t",
            "SELECT datetime('now'), date('now'), random() FROM t",
            "SELECT CURRENT_TIMESTAMP, CURRENT_DATE, CURRENT_TIME",
            // `LEFT`/`RIGHT` as join keywords are not calls.
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id",
            "SELECT * FROM a RIGHT OUTER JOIN b ON a.id = b.id",
            "SELECT * FROM t WHERE a IN (1, 2, 3)",
            // JSON (AHL-490): spelled identically in both dialects, so the
            // engine's own case-insensitive function lookup already answers
            // a MySQL client's uppercase spelling with no shim involvement —
            // see `json_functions_spelled_the_same_reach_the_engine_directly`.
            "SELECT json_extract(doc, '$.a') FROM t",
            "SELECT JSON_SET(doc, '$.a', 1) FROM t",
        ] {
            assert_eq!(out(sql), sql, "{sql} should have been left alone");
        }
    }

    /// `JSON_EXTRACT`/`JSON_SET`/`JSON_INSERT`/`JSON_REPLACE`/`JSON_REMOVE`/
    /// `JSON_VALID`/`JSON_ARRAY`/`JSON_OBJECT`/`JSON` are not in this
    /// module's mapped list at all, on purpose: they are spelled exactly the
    /// same in MySQL and SQLite, and the engine's function lookup is
    /// case-insensitive, so a MySQL client's uppercase call already reaches
    /// the engine's own SQLite-semantics implementation directly.
    #[test]
    fn json_functions_spelled_the_same_reach_the_engine_directly() {
        for sql in [
            "SELECT JSON_EXTRACT('{\"a\":1}', '$.a')",
            "SELECT Json_Set('{\"a\":1}', '$.a', 2)",
            "SELECT JSON_INSERT('{\"a\":1}', '$.b', 2)",
            "SELECT JSON_REPLACE('{\"a\":1}', '$.a', 2)",
            "SELECT JSON_REMOVE('{\"a\":1}', '$.a')",
            "SELECT JSON_VALID('{\"a\":1}')",
            "SELECT JSON_ARRAY(1, 2)",
            "SELECT JSON_OBJECT('a', 1)",
            "SELECT JSON('{\"a\":1}')",
        ] {
            assert_eq!(out(sql), sql, "{sql} should have been left alone");
        }
    }

    /// `JSON_LENGTH` renames onto `json_array_length` — exact for an array,
    /// which is what `whereJsonLength` targets; the object/scalar divergence
    /// is documented in `docs/server.md`, not refused.
    #[test]
    fn json_length_maps_onto_json_array_length() {
        assert_eq!(
            out("SELECT JSON_LENGTH(doc) FROM t"),
            "SELECT json_array_length(doc) FROM t"
        );
        assert_eq!(
            out("SELECT JSON_LENGTH(doc, '$.tags') FROM t"),
            "SELECT json_array_length(doc, '$.tags') FROM t"
        );
    }

    /// `JSON_CONTAINS_PATH` is mapped only for the single-path `'one'` mode
    /// shape Laravel's own `whereJsonContainsKey` emits, onto the same
    /// rewrite Laravel's own SQLite grammar uses for that clause.
    #[test]
    fn json_contains_path_maps_only_laravels_one_path_shape() {
        assert_eq!(
            out("SELECT ifnull(JSON_CONTAINS_PATH(doc, 'one', '$.a'), 0) FROM t"),
            "SELECT ifnull((json_type(doc, '$.a') IS NOT NULL), 0) FROM t"
        );
        refuses("SELECT JSON_CONTAINS_PATH(doc, 'all', '$.a')", "'one' mode");
        refuses(
            "SELECT JSON_CONTAINS_PATH(doc, 'one', '$.a', '$.b')",
            "exact shape",
        );
    }

    /// `JSON_UNQUOTE(JSON_EXTRACT(doc, path))` — Laravel's `wrapJsonSelector`
    /// — becomes `(doc ->> path)`; anything else under `JSON_UNQUOTE` is
    /// refused rather than guessed at.
    #[test]
    fn json_unquote_of_json_extract_becomes_the_arrow_operator() {
        assert_eq!(
            out("SELECT JSON_UNQUOTE(JSON_EXTRACT(doc, '$.a')) FROM t"),
            "SELECT (doc ->> '$.a') FROM t"
        );
        assert_eq!(
            out("SELECT json_unquote( json_extract(doc, '$.a') ) FROM t"),
            "SELECT (doc ->> '$.a') FROM t"
        );
        refuses("SELECT JSON_UNQUOTE(doc)", "a bare JSON_UNQUOTE");
        refuses(
            "SELECT JSON_UNQUOTE(JSON_EXTRACT(doc, '$.a') + 1)",
            "a bare JSON_UNQUOTE",
        );
    }

    /// `JSON_QUOTE`, `JSON_TYPE`, `JSON_CONTAINS` and `JSON_OVERLAPS` are
    /// refused rather than left alone, even though the engine has same-named
    /// functions: leaving them alone would answer a MySQL client with
    /// SQLite's different rules under an identical name, which is exactly
    /// the silent-wrong-answer shape this module exists to prevent.
    #[test]
    fn json_quote_and_json_type_are_refused_not_left_to_diverge_silently() {
        refuses("SELECT JSON_QUOTE(1)", "Incorrect type");
        refuses("SELECT JSON_TYPE(doc)", "OBJECT/ARRAY");
        refuses("SELECT JSON_CONTAINS(doc, '1')", "set-membership");
        refuses("SELECT JSON_OVERLAPS(doc, '[1]')", "set-intersection");
    }

    /// A mapped name is only a call when a `(` follows it. Everything else is
    /// an identifier, and rewriting one would rename somebody's column.
    #[test]
    fn a_mapped_name_that_is_not_a_call_is_left_alone() {
        for sql in [
            "SELECT concat FROM t",
            "SELECT t.left, t.right FROM t",
            "SELECT now FROM t ORDER BY now",
            "SELECT `if`, `day`, `year` FROM t",
            "SELECT a.left(1) FROM t",
            "SELECT @concat(1)",
        ] {
            assert_eq!(out(sql), sql, "{sql} should have been left alone");
        }
    }

    /// A function name inside a string literal or a quoted identifier is data.
    /// Rewriting one would change a value the client asked to store.
    #[test]
    fn quoted_spans_are_never_rewritten() {
        for sql in [
            "SELECT 'NOW() is not a call'",
            "INSERT INTO t (a) VALUES ('CONCAT(1,2)')",
            "SELECT `CONCAT` FROM t",
            "SELECT \"LEFT(x, 1)\" FROM t",
            "SELECT 'it''s CONCAT( unbalanced'",
            "SELECT 'a\\'CONCAT('",
        ] {
            assert_eq!(out(sql), sql, "{sql} should have been left alone");
        }
    }

    // ------------------------------------------------------------- strings

    #[test]
    fn concat_becomes_a_null_propagating_pipe_chain() {
        // Both engines answer NULL when any argument is NULL — checked against
        // MySQL 8.4, where CONCAT('a', NULL, 'c') is NULL and not 'ac'.
        assert_eq!(out("SELECT CONCAT(a, b)"), "SELECT ('' || a || b)");
        assert_eq!(out("SELECT CONCAT(a)"), "SELECT ('' || a)");
        assert_eq!(
            out("SELECT CONCAT('a', 'b', 'c')"),
            "SELECT ('' || 'a' || 'b' || 'c')"
        );
        // The leading `''` is what makes the result TEXT at every arity.
        assert_eq!(out("SELECT CONCAT(1.5)"), "SELECT ('' || 1.5)");
    }

    #[test]
    fn the_length_and_case_functions_map_onto_the_engines() {
        assert_eq!(out("SELECT CHAR_LENGTH(a)"), "SELECT length(a)");
        assert_eq!(out("SELECT CHARACTER_LENGTH(a)"), "SELECT length(a)");
        assert_eq!(out("SELECT UCASE(a)"), "SELECT upper(a)");
        assert_eq!(out("SELECT LCASE(a)"), "SELECT lower(a)");
        assert_eq!(out("select ucase(a)"), "select upper(a)");
    }

    /// The mapping that is wrong in exactly one way if it is written from
    /// memory: MySQL's `LOCATE(needle, haystack)` is the reverse of SQLite's
    /// `instr(haystack, needle)`, and getting it backwards answers 0 for every
    /// match, with no error at all.
    #[test]
    fn locate_swaps_its_arguments_and_position_does_too() {
        assert_eq!(
            out("SELECT LOCATE('ll', 'hello')"),
            "SELECT instr('hello', 'll')"
        );
        assert_eq!(
            out("SELECT LOCATE(needle, hay)"),
            "SELECT instr(hay, needle)"
        );
        assert_eq!(
            out("SELECT POSITION('ll' IN 'hello')"),
            "SELECT instr('hello', 'll')"
        );
        assert_eq!(out("SELECT POSITION(a IN b)"), "SELECT instr(b, a)");
    }

    /// `LEFT`/`RIGHT` are mapped only with a literal length, and the zero and
    /// negative cases become MySQL's empty string rather than SQLite's
    /// character arithmetic. The expectations are MySQL 8.4's:
    /// `RIGHT('hello', 0)` is `''`, where `substr('hello', -0)` is `'hello'`.
    #[test]
    fn left_and_right_become_substr_with_the_arithmetic_already_done() {
        assert_eq!(out("SELECT LEFT(a, 3)"), "SELECT substr(a, 1, 3)");
        assert_eq!(out("SELECT RIGHT(a, 3)"), "SELECT substr(a, -3, 3)");
        assert_eq!(out("SELECT LEFT(a, 0)"), "SELECT substr(a, 0, 0)");
        assert_eq!(out("SELECT RIGHT(a, 0)"), "SELECT substr(a, 0, 0)");
        assert_eq!(out("SELECT LEFT(a, -1)"), "SELECT substr(a, 0, 0)");
        assert_eq!(out("SELECT RIGHT(a, -1)"), "SELECT substr(a, 0, 0)");
        assert_eq!(out("SELECT LEFT(a, +2)"), "SELECT substr(a, 1, 2)");
    }

    /// The reason `LEFT`/`RIGHT` insist on a literal. A second `?` would shift
    /// every parameter index after it, and a NULL length is the empty string
    /// to `substr()` and NULL to MySQL.
    #[test]
    fn left_and_right_refuse_a_length_they_cannot_prove() {
        for sql in [
            "SELECT LEFT(a, ?)",
            "SELECT RIGHT(a, ?)",
            "SELECT LEFT(a, n)",
            "SELECT RIGHT(a, 1 + 1)",
            "SELECT LEFT(a, NULL)",
            "SELECT LEFT(a, '3')",
        ] {
            refuses(sql, "integer literal");
        }
        // And a mapping that does go through leaves the placeholder count alone.
        assert_eq!(
            out("SELECT LEFT(a, 3) FROM t WHERE b = ?"),
            "SELECT substr(a, 1, 3) FROM t WHERE b = ?"
        );
    }

    /// AHL-465: five spellings MySQL and SQLite share, that used to resolve
    /// in the engine under SQLite's own semantics because the shim could not
    /// tell the two dialects apart from the text alone. Every connection to
    /// this server speaks the MySQL wire protocol, so a bare `length(x)` came
    /// from a MySQL client exactly as much as `LENGTH(x)` did — MySQL's own
    /// function names are themselves case-insensitive — and both are mapped
    /// onto the primitives now, regardless of case.
    #[test]
    fn length_hex_substring_nullif_and_round_map_onto_the_ahl465_primitives() {
        assert_eq!(out("SELECT LENGTH(a)"), "SELECT octet_length(a)");
        assert_eq!(out("SELECT length(a)"), "SELECT octet_length(a)");

        assert_eq!(out("SELECT HEX(a)"), "SELECT mysql_hex(a)");
        assert_eq!(out("SELECT hex(a)"), "SELECT mysql_hex(a)");

        assert_eq!(out("SELECT SUBSTRING(a, 1)"), "SELECT mysql_substr(a, 1)");
        assert_eq!(
            out("SELECT SUBSTRING(a, 1, 2)"),
            "SELECT mysql_substr(a, 1, 2)"
        );
        assert_eq!(
            out("SELECT SUBSTR(a, 1, 2)"),
            "SELECT mysql_substr(a, 1, 2)"
        );
        assert_eq!(
            out("SELECT substring(a, 1, 2)"),
            "SELECT mysql_substr(a, 1, 2)"
        );

        assert_eq!(out("SELECT NULLIF(a, b)"), "SELECT mysql_nullif(a, b)");
        assert_eq!(out("SELECT nullif(a, b)"), "SELECT mysql_nullif(a, b)");

        // `ROUND` is deliberately *not* rewritten here: see
        // `round_is_mapped_only_for_a_double_literal_or_a_negative_digit_count`
        // for why `ROUND(x)` over a column must stay the engine's own.

        // A wrong argument count is still MySQL's own `1582`.
        let error = refusal("SELECT LENGTH(a, b)");
        assert_eq!(error.code, 1582);
        let error = refusal("SELECT NULLIF(a)");
        assert_eq!(error.code, 1582);
    }

    /// `MID` used to be refused deliberately (a second spelling for a
    /// divergence was not worth having), and `OCTET_LENGTH`/`BIT_LENGTH`
    /// were refused because the engine had no byte-counting primitive at
    /// all. AHL-465 removes both reasons.
    #[test]
    fn mid_octet_length_and_bit_length_are_mapped_now_that_the_primitives_exist() {
        assert_eq!(out("SELECT MID(a, 1, 2)"), "SELECT mysql_substr(a, 1, 2)");
        assert_eq!(out("SELECT MID(a, 1)"), "SELECT mysql_substr(a, 1)");
        assert_eq!(out("SELECT OCTET_LENGTH(a)"), "SELECT octet_length(a)");
        assert_eq!(out("SELECT BIT_LENGTH(a)"), "SELECT (octet_length(a) * 8)");
    }

    // -------------------------------------------------------- conditionals

    #[test]
    fn the_conditional_functions_become_engine_expressions() {
        assert_eq!(out("SELECT ISNULL(a)"), "SELECT (a IS NULL)");
        assert_eq!(
            out("SELECT IF(a > 1, 'y', 'n')"),
            "SELECT CASE WHEN a > 1 THEN 'y' ELSE 'n' END"
        );
        // MySQL takes one argument here; the engine wants at least two.
        assert_eq!(out("SELECT COALESCE(a)"), "SELECT (a)");
        assert_eq!(out("SELECT COALESCE(a, b)"), "SELECT COALESCE(a, b)");
    }

    // ------------------------------------------------------------- numeric

    #[test]
    fn rand_becomes_a_double_in_the_unit_interval() {
        assert_eq!(
            out("SELECT RAND()"),
            "SELECT (abs(random()) / 9223372036854775808.0)"
        );
    }

    /// `ROUND` used to be the engine's own function under its own name, left
    /// alone except for one shape that was refused outright: a negative
    /// digit count, which `round()` clamps to zero. AHL-465 gave the shim a
    /// primitive for that shape, and a second one this test also pins: a
    /// value written as a MySQL `DOUBLE` literal (one with an exponent)
    /// ties to even in MySQL 8.4.11 and away from zero in `round()`. Neither
    /// shape can be proven for a column or an expression — no catalog
    /// access from here, and no distinct "this looked like an exponent"
    /// fact survives past a literal — so those are still the engine's own,
    /// on purpose: MySQL's own manual gives `ROUND(2.5)`, no exponent, as
    /// the *safe* spelling, which is exactly the case this leaves alone.
    #[test]
    fn round_is_mapped_only_for_a_double_literal_or_a_negative_digit_count() {
        // The negative digit count: was refused, is a mapping now.
        assert_eq!(
            out("SELECT ROUND(1234.5678, -2)"),
            "SELECT mysql_round(1234.5678, -2)"
        );
        // A value written with an exponent: was silently wrong, is a
        // mapping now.
        assert_eq!(out("SELECT ROUND(2.5e0)"), "SELECT mysql_round(2.5e0)");
        assert_eq!(out("SELECT ROUND(25E-1)"), "SELECT mysql_round(25E-1)");
        assert_eq!(
            out("SELECT ROUND(2.5e0, 1)"),
            "SELECT mysql_round(2.5e0, 1)"
        );

        // A plain decimal literal, an integer, a column, and an expression:
        // none of these is provably a `DOUBLE` to MySQL's own parser, so
        // all four are left exactly as written — the engine's own `round()`,
        // ties away from zero, unchanged from before AHL-465.
        for sql in [
            "SELECT ROUND(2.5)",
            "SELECT ROUND(2)",
            "SELECT ROUND(x)",
            "SELECT ROUND(x, 2)",
            "SELECT ROUND(1.5 + 1)",
            "SELECT round(x)",
        ] {
            assert_eq!(out(sql), sql, "{sql} is not provably a DOUBLE literal");
        }
    }

    // ----------------------------------------------------------- date/time

    #[test]
    fn the_clock_functions_read_the_engines_clock() {
        assert_eq!(out("SELECT NOW()"), "SELECT datetime('now')");
        assert_eq!(out("SELECT LOCALTIME()"), "SELECT datetime('now')");
        assert_eq!(out("SELECT LOCALTIMESTAMP()"), "SELECT datetime('now')");
        assert_eq!(out("SELECT UTC_TIMESTAMP()"), "SELECT datetime('now')");
        assert_eq!(out("SELECT CURDATE()"), "SELECT date('now')");
        assert_eq!(out("SELECT UTC_DATE()"), "SELECT date('now')");
        assert_eq!(out("SELECT CURTIME()"), "SELECT time('now')");
        assert_eq!(out("SELECT UTC_TIME()"), "SELECT time('now')");
        assert_eq!(out("SELECT UNIX_TIMESTAMP()"), "SELECT unixepoch('now')");
    }

    /// Each date part is cast back to the integer MySQL answers with, and the
    /// two that renumber the week carry their own shift. The shifts come from
    /// MySQL 8.4: 2024-01-15 is a Monday, whose `DAYOFWEEK` is 2 and whose
    /// `WEEKDAY` is 0, against SQLite's `%w` of 1.
    #[test]
    fn the_date_parts_become_a_cast_strftime() {
        for (sql, expected) in [
            (
                "SELECT YEAR(d)",
                "SELECT CAST(strftime('%Y', d) AS INTEGER)",
            ),
            (
                "SELECT MONTH(d)",
                "SELECT CAST(strftime('%m', d) AS INTEGER)",
            ),
            ("SELECT DAY(d)", "SELECT CAST(strftime('%d', d) AS INTEGER)"),
            (
                "SELECT DAYOFMONTH(d)",
                "SELECT CAST(strftime('%d', d) AS INTEGER)",
            ),
            (
                "SELECT HOUR(d)",
                "SELECT CAST(strftime('%H', d) AS INTEGER)",
            ),
            (
                "SELECT MINUTE(d)",
                "SELECT CAST(strftime('%M', d) AS INTEGER)",
            ),
            (
                "SELECT SECOND(d)",
                "SELECT CAST(strftime('%S', d) AS INTEGER)",
            ),
            (
                "SELECT DAYOFYEAR(d)",
                "SELECT CAST(strftime('%j', d) AS INTEGER)",
            ),
            (
                "SELECT DAYOFWEEK(d)",
                "SELECT (CAST(strftime('%w', d) AS INTEGER) + 1)",
            ),
            (
                "SELECT WEEKDAY(d)",
                "SELECT ((CAST(strftime('%w', d) AS INTEGER) + 6) % 7)",
            ),
            (
                "SELECT QUARTER(d)",
                "SELECT ((CAST(strftime('%m', d) AS INTEGER) + 2) / 3)",
            ),
            (
                "SELECT LAST_DAY(d)",
                "SELECT date(d, 'start of month', '+1 month', '-1 day')",
            ),
        ] {
            assert_eq!(out(sql), expected, "{sql}");
        }
    }

    #[test]
    fn trim_maps_only_the_forms_with_nothing_to_remove() {
        assert_eq!(out("SELECT TRIM(BOTH FROM a)"), "SELECT trim(a)");
        assert_eq!(out("SELECT TRIM(LEADING FROM a)"), "SELECT ltrim(a)");
        assert_eq!(out("SELECT TRIM(TRAILING FROM a)"), "SELECT rtrim(a)");
        assert_eq!(out("SELECT TRIM(FROM a)"), "SELECT trim(a)");
        // Already the engine's, and left exactly as written.
        assert_eq!(out("SELECT TRIM(a)"), "SELECT TRIM(a)");
        assert_eq!(out("SELECT trim(a, 'xy')"), "SELECT trim(a, 'xy')");
    }

    // ------------------------------------------------------------ refusals

    /// Every name this module considered and rejected, with the fragment of
    /// its reason a reader can check. A refusal that stops explaining itself is
    /// how the next reader re-derives the same wrong mapping.
    #[test]
    fn each_rejected_mapping_refuses_with_the_input_that_decided_it() {
        for (sql, needle) in [
            ("SELECT CONCAT_WS('-', a, b)", "'a-c'"),
            ("SELECT GREATEST(a, b)", "GREATEST(2, '10') is 2"),
            ("SELECT LEAST(a, b)", "LEAST(2, '10') is '10'"),
            ("SELECT MOD(a, b)", "MOD(5.5, 2) is 1.5"),
            ("SELECT SYSDATE()", "moment of the call"),
            ("SELECT DATE_FORMAT(d, '%Y')", "format specifiers"),
            ("SELECT STR_TO_DATE(d, '%Y')", "format specifiers"),
            ("SELECT DATEDIFF(a, b)", "julianday"),
            ("SELECT TIMESTAMPDIFF(DAY, a, b)", "julianday"),
            ("SELECT DATE_ADD(d, INTERVAL 1 DAY)", "INTERVAL"),
            ("SELECT MONTHNAME(d)", "locale-dependent"),
            ("SELECT DAYNAME(d)", "locale-dependent"),
            ("SELECT WEEK(d)", "week-numbering"),
            ("SELECT YEARWEEK(d)", "week-numbering"),
            ("SELECT FROM_UNIXTIME(n)", "1969-12-31 23:59:59"),
            ("SELECT LOCATE('a', b, 2)", "no third argument"),
            ("SELECT RAND(42)", "seeded sequence"),
            ("SELECT NOW(3)", "fractional digits"),
            ("SELECT CURTIME(6)", "fractional digits"),
            ("SELECT UNIX_TIMESTAMP(d)", "session time zone"),
            ("SELECT TRIM(BOTH 'xy' FROM a)", "'yxhi'"),
            ("SELECT TRIM(LEADING 'x' FROM a)", "set of characters"),
        ] {
            refuses(sql, needle);
        }
    }

    /// A wrong argument count is MySQL's own `1582`, with MySQL's own message —
    /// checked against 8.4, which answers exactly this for
    /// `CHAR_LENGTH('a','b')`.
    #[test]
    fn a_wrong_argument_count_is_the_code_mysql_uses() {
        for (sql, name) in [
            ("SELECT CHAR_LENGTH(a, b)", "CHAR_LENGTH"),
            ("SELECT CONCAT()", "CONCAT"),
            ("SELECT UCASE()", "UCASE"),
            ("SELECT IF(a, b)", "IF"),
            ("SELECT LEFT(a)", "LEFT"),
            ("SELECT YEAR(a, b)", "YEAR"),
            ("SELECT ISNULL(a, b)", "ISNULL"),
        ] {
            let error = refusal(sql);
            assert_eq!(error.code, 1582, "{sql}");
            assert_eq!(error.sqlstate, "42000", "{sql}");
            assert_eq!(
                error.message,
                format!("Incorrect parameter count in the call to native function '{name}'")
            );
        }
    }

    // ------------------------------------------------------------- nesting

    #[test]
    fn calls_nest_in_both_directions() {
        assert_eq!(
            out("SELECT CONCAT(UCASE(LEFT(a, 1)), LCASE(b))"),
            "SELECT ('' || upper(substr(a, 1, 1)) || lower(b))"
        );
        // A mapped call inside one that was left alone is still rewritten,
        // because the scan descends through the call it did not take.
        // `ROUND(x, 2)` over a column is still declined (AHL-465 only maps
        // it for a `DOUBLE` literal or a negative digit count), and
        // `COALESCE` with two-or-more arguments always is.
        assert_eq!(
            out("SELECT ROUND(CHAR_LENGTH(a), 2)"),
            "SELECT ROUND(length(a), 2)"
        );
        assert_eq!(
            out("SELECT COALESCE(CHAR_LENGTH(a), 0)"),
            "SELECT COALESCE(length(a), 0)"
        );
        assert_eq!(
            out("SELECT count(*) FROM t WHERE YEAR(d) = 2024"),
            "SELECT count(*) FROM t WHERE CAST(strftime('%Y', d) AS INTEGER) = 2024"
        );
        // And a refusal inside a nest is still a refusal.
        refuses("SELECT CONCAT(a, GREATEST(b, c))", "GREATEST");
    }

    /// MySQL allows whitespace between a built-in's name and its `(`, and so
    /// does the engine's parser, so the shim has to as well — otherwise
    /// `UPPER (x)` would work and `UCASE (x)` would not.
    #[test]
    fn whitespace_between_the_name_and_the_parenthesis_is_still_a_call() {
        assert_eq!(out("SELECT CHAR_LENGTH ('abc')"), "SELECT length('abc')");
        assert_eq!(out("SELECT CONCAT\t(a, b)"), "SELECT ('' || a || b)");
    }

    /// A nest of calls the module *declines* must not cost `2^depth`.
    ///
    /// It did: the argument list was rewritten to decide whether to rewrite it,
    /// then thrown away and rewritten again when the scan re-descended into the
    /// call. Thirty nested `ROUND`s took a billion steps. AHL-465 changed *why*
    /// `ROUND(x)` over a column is declined — a column is not provably a
    /// `DOUBLE` literal, where it used to be simply "the engine's own" — but
    /// it is still declined, so it is still exactly the regression case. The
    /// decision is taken from the arguments as written, and this test is a
    /// wall clock rather than an assertion about output — thirty levels
    /// finishes instantly when it is linear and never when it is not.
    #[test]
    fn a_nest_of_declined_calls_stays_linear() {
        for depth in [20usize, 30] {
            let sql = format!("SELECT {}x{}", "ROUND(".repeat(depth), ")".repeat(depth));
            assert_eq!(out(&sql), sql, "a declined nest is also left alone");
        }
        // The same shape through the other name that declines itself.
        let sql = format!("SELECT {}x{}", "COALESCE(a, ".repeat(25), ")".repeat(25));
        assert_eq!(out(&sql), sql);
    }

    /// The recursion has a floor, and reaching it is an ordinary refusal
    /// rather than a stack overflow.
    #[test]
    fn a_pathological_nest_is_refused_rather_than_crashing() {
        let depth = MAX_DEPTH + 5;
        let sql = format!("SELECT {}a{}", "CONCAT(".repeat(depth), ")".repeat(depth));
        let error = refusal(&sql);
        assert_eq!(error.code, 1235);
        assert!(error.message.contains("too deeply"), "{}", error.message);
    }
}
