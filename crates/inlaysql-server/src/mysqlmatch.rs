//! MySQL full-text search: `MATCH (cols) AGAINST (query)` in, the engine's
//! native `bm25_score(cols, query)` out.
//!
//! This is a straight dialect translation, not an approximation, and the
//! reason it can be one is that `bm25_score`'s own design already names
//! MySQL's clause as its model — the core's planner accepts multi-column
//! probes (`bm25_score(title, body, ?)`, column order irrelevant), folds a
//! string-literal query at plan time into an index probe, binds `?` from the
//! positional parameters, and ranks higher-is-better exactly as MySQL's
//! relevance value does. So the rewrite is: swap the spelling, drop the
//! explicit default mode, refuse the modes whose semantics the BM25 index
//! does not implement.
//!
//! What is refused, loudly (house rule — a clause this project cannot honour
//! is refused, never accepted and ignored):
//!
//! * `IN BOOLEAN MODE` — the `+`, `-`, `*`, `>`, `<`, `"` operators change
//!   *which rows match*, not just how they rank; the BM25 index has no
//!   boolean operator surface.
//! * `WITH QUERY EXPANSION` (and `IN NATURAL LANGUAGE MODE WITH QUERY
//!   EXPANSION`) — a two-round retrieval this engine does not implement.
//!
//! `IN NATURAL LANGUAGE MODE`, the default, is accepted and dropped: natural
//! language mode *is* what the BM25 probe does.
//!
//! The pass runs after [`crate::mysqlfunc`] in [`crate::shim::translate`], on
//! text that has already been through [`crate::sqltext::normalize`] (comments
//! gone) and the backslash-escape rewrite (string literals are already in
//! the engine's spelling), and it copies every span it does not understand
//! byte-for-byte — a statement without `MATCH` comes back unchanged.

use crate::errors::MysqlError;

/// Rewrite every `MATCH (cols) AGAINST (query)` in `sql` into
/// `bm25_score(cols, query)`.
pub fn rewrite(sql: &str) -> Result<String, MysqlError> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    // `a.match (` is a qualified name and `@match (` a variable; neither is
    // the clause. (MySQL itself has no bare `match` value, but a column named
    // `match` is spelled `` `match` `` — a quoted span, copied below — and an
    // unquoted one would be refused by the engine anyway, never silently
    // accepted.)
    let mut previous = '\0';

    while i < chars.len() {
        let c = chars[i];

        // Quoted spans are data, not syntax: a string literal containing the
        // text `MATCH (body) AGAINST ('x')` is a value somebody is storing.
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
            let qualified = previous == '.' || previous == '@' || previous == '$';

            if word.eq_ignore_ascii_case("match")
                && !qualified
                && next_non_space_is(&chars, i, '(')
            {
                // The column list: everything inside the parentheses,
                // verbatim — backticks, whitespace and all.
                let open = next_non_space(&chars, i);
                let Some(close) = matching_paren(&chars, open) else {
                    return Err(MysqlError::parse(
                        "MATCH is missing its closing ')'",
                    ));
                };
                let columns: String = chars[open + 1..close].iter().collect();

                // `AGAINST` — MySQL's grammar has nothing else after the
                // MATCH column list, and inventing a translation for a
                // statement that is not the clause would be the quiet kind
                // of wrong.
                let after = next_non_space(&chars, close + 1);
                let against = read_word(&chars, after);
                if !against.word.eq_ignore_ascii_case("against") {
                    return Err(MysqlError::parse(
                        "MATCH must be followed by AGAINST: the engine has no other full-text spelling",
                    ));
                }

                let arg_open = next_non_space(&chars, against.end);
                if chars.get(arg_open) != Some(&'(') {
                    return Err(MysqlError::parse(
                        "AGAINST must be followed by '('",
                    ));
                }
                let Some(arg_close) = matching_paren(&chars, arg_open) else {
                    return Err(MysqlError::parse(
                        "AGAINST is missing its closing ')'",
                    ));
                };

                let (query, mode_end) =
                    read_against_argument(&chars, arg_open + 1, arg_close)?;

                out.push_str("bm25_score(");
                out.push_str(columns.trim());
                out.push_str(", ");
                out.push_str(&query);
                out.push(')');

                i = mode_end;
                previous = ')';
                continue;
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

/// Parse what sits between `AGAINST (` and its `)`: the query — a string
/// literal or a `?` placeholder, copied verbatim — and an optional trailing
/// mode specifier. Returns the query text and the index just past the
/// closing parenthesis.
fn read_against_argument(
    chars: &[char],
    mut i: usize,
    close: usize,
) -> Result<(String, usize), MysqlError> {
    while chars.get(i).is_some_and(|c| c.is_whitespace()) {
        i += 1;
    }

    match chars.get(i) {
        Some('\'') | Some('"') => {
            let (span, next) = quoted_span(chars, i);
            finish_against(chars, next, close, span)
        }
        Some('?') => finish_against(chars, i + 1, close, "?".to_string()),
        _ => Err(MysqlError::parse(
            "AGAINST takes a string literal or a placeholder as its search query",
        )),
    }
}

/// After the query: whitespace, an optional mode specifier, then the `)` that
/// closes `AGAINST (`. `IN NATURAL LANGUAGE MODE` is the default and is what
/// the BM25 probe does — accepted and dropped. The other two modes change
/// what the clause means and are refused with their names in the message.
fn finish_against(
    chars: &[char],
    mut i: usize,
    close: usize,
    query: String,
) -> Result<(String, usize), MysqlError> {
    while chars.get(i).is_some_and(|c| c.is_whitespace()) {
        i += 1;
    }
    if i >= close {
        return Ok((query, close + 1));
    }

    let word = read_word(chars, i);
    if !word.word.eq_ignore_ascii_case("in") && !word.word.eq_ignore_ascii_case("with") {
        return Err(MysqlError::parse(
            "unexpected token inside AGAINST ()",
        ));
    }

    let rest: String = chars[i..close].iter().collect();
    let rest_upper = rest.to_ascii_uppercase();
    if rest_upper.contains("WITH QUERY EXPANSION") {
        return Err(MysqlError::unsupported(
            "MATCH ... AGAINST WITH QUERY EXPANSION is not supported: two-round query \
             expansion is not something the BM25 index implements",
        ));
    }
    if rest_upper.contains("BOOLEAN MODE") {
        return Err(MysqlError::unsupported(
            "MATCH ... AGAINST IN BOOLEAN MODE is not supported: the boolean operators \
             (+, -, *, >, <, \") change which rows match, and the BM25 index has no \
             boolean surface — use natural language mode (the default) or bm25_score() \
             directly",
        ));
    }
    if rest_upper.trim().starts_with("IN NATURAL LANGUAGE MODE") {
        // The default mode, spelled out. Same semantics as dropping it.
        return Ok((query, close + 1));
    }

    Err(MysqlError::parse(format!(
        "unexpected mode specifier in AGAINST (): {rest:?}"
    )))
}

// ----------------------------------------------------------------- helpers

fn next_non_space(chars: &[char], mut i: usize) -> usize {
    while chars.get(i).is_some_and(|c| c.is_whitespace()) {
        i += 1;
    }
    i
}

fn next_non_space_is(chars: &[char], mut i: usize, expected: char) -> bool {
    i = next_non_space(chars, i);
    chars.get(i) == Some(&expected)
}

fn is_word_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

struct Word {
    word: String,
    end: usize,
}

fn read_word(chars: &[char], mut i: usize) -> Word {
    let start = i;
    while chars.get(i).is_some_and(|c| is_word_char(*c)) {
        i += 1;
    }
    Word {
        word: chars[start..i].iter().collect(),
        end: i,
    }
}

/// Copy a quoted span whole, returning it and the index just past it. The
/// same rule `mysqlfunc` uses: a doubled quote is an escape, a backslashed
/// quote is an escape, and everything else is inside the string.
fn quoted_span(chars: &[char], at: usize) -> (String, usize) {
    let quote = chars[at];
    let mut span = String::from(quote);
    let mut i = at + 1;
    while i < chars.len() {
        let c = chars[i];
        span.push(c);
        if c == '\\' {
            if let Some(next) = chars.get(i + 1) {
                span.push(*next);
                i += 2;
                continue;
            }
        }
        i += 1;
        if c == quote {
            if chars.get(i) == Some(&quote) {
                span.push(quote);
                i += 1;
                continue;
            }
            return (span, i);
        }
    }
    (span, i)
}

/// Index of the `)` matching the `(` at `open`, ignoring nested parens and
/// quoted spans.
fn matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(sql: &str) -> String {
        rewrite(sql).expect("rewrite failed")
    }

    #[test]
    fn a_simple_match_translates_to_bm25_score() {
        assert_eq!(
            rewritten("SELECT id FROM docs WHERE MATCH (body) AGAINST ('rust database')"),
            "SELECT id FROM docs WHERE bm25_score(body, 'rust database')"
        );
    }

    #[test]
    fn the_translation_is_case_insensitive_and_whitespace_tolerant() {
        assert_eq!(
            rewritten("select id from docs where match(body) against('x')"),
            "select id from docs where bm25_score(body, 'x')"
        );
        assert_eq!(
            rewritten("SELECT id FROM docs WHERE\n  MATCH  ( body )\n  AGAINST ( 'x' )"),
            "SELECT id FROM docs WHERE\n  bm25_score(body, 'x')"
        );
    }

    #[test]
    fn multi_column_match_maps_onto_the_multi_column_probe() {
        assert_eq!(
            rewritten("SELECT id FROM docs WHERE MATCH (title, body) AGAINST ('x')"),
            "SELECT id FROM docs WHERE bm25_score(title, body, 'x')"
        );
    }

    #[test]
    fn backticked_columns_and_parenthesised_surroundings_survive() {
        assert_eq!(
            rewritten("SELECT * FROM docs WHERE (MATCH (`body`) AGAINST ('x') AND id > 1)"),
            "SELECT * FROM docs WHERE (bm25_score(`body`, 'x') AND id > 1)"
        );
    }

    #[test]
    fn a_placeholder_is_copied_so_parameter_ordinals_do_not_move() {
        assert_eq!(
            rewritten("SELECT id FROM docs WHERE MATCH (body) AGAINST (?)"),
            "SELECT id FROM docs WHERE bm25_score(body, ?)"
        );
    }

    #[test]
    fn a_literal_with_escaped_quotes_is_copied_verbatim() {
        assert_eq!(
            rewritten("SELECT id FROM docs WHERE MATCH (body) AGAINST ('it\\'s here')"),
            "SELECT id FROM docs WHERE bm25_score(body, 'it\\'s here')"
        );
    }

    #[test]
    fn relevance_in_the_projection_translates_the_same_way() {
        assert_eq!(
            rewritten(
                "SELECT id, MATCH (body) AGAINST ('x') AS relevance FROM docs \
                 ORDER BY relevance DESC LIMIT 10"
            ),
            "SELECT id, bm25_score(body, 'x') AS relevance FROM docs \
             ORDER BY relevance DESC LIMIT 10"
        );
    }

    #[test]
    fn the_default_mode_spelled_out_is_accepted_and_dropped() {
        assert_eq!(
            rewritten(
                "SELECT id FROM docs WHERE MATCH (body) AGAINST ('x' IN NATURAL LANGUAGE MODE)"
            ),
            "SELECT id FROM docs WHERE bm25_score(body, 'x')"
        );
    }

    #[test]
    fn boolean_mode_is_refused_by_name() {
        let error = rewrite("SELECT id FROM docs WHERE MATCH (body) AGAINST ('+rust -php' IN BOOLEAN MODE)")
            .expect_err("boolean mode was accepted");
        assert_eq!(error.code, 1235);
        assert!(error.message.contains("BOOLEAN MODE"), "{error:?}");
    }

    #[test]
    fn query_expansion_is_refused_by_name() {
        for sql in [
            "SELECT id FROM docs WHERE MATCH (body) AGAINST ('x' WITH QUERY EXPANSION)",
            "SELECT id FROM docs WHERE MATCH (body) AGAINST ('x' IN NATURAL LANGUAGE MODE WITH QUERY EXPANSION)",
        ] {
            let error = rewrite(sql).expect_err("query expansion was accepted");
            assert_eq!(error.code, 1235);
            assert!(error.message.contains("QUERY EXPANSION"), "{error:?}");
        }
    }

    #[test]
    fn malformed_clauses_are_parse_errors_naming_the_shape() {
        for sql in [
            "SELECT id FROM docs WHERE MATCH (body) ('x')",
            "SELECT id FROM docs WHERE MATCH (body",
            "SELECT id FROM docs WHERE MATCH (body) AGAINST (body)",
        ] {
            let error = rewrite(sql).expect_err("malformed MATCH was accepted");
            assert_eq!(error.code, 1064, "{error:?}");
        }
    }

    #[test]
    fn text_that_only_looks_like_the_clause_is_untouched() {
        // Inside a string literal: data, not syntax.
        assert_eq!(
            rewritten("INSERT INTO notes VALUES ('MATCH (body) AGAINST (\\'x\\')')"),
            "INSERT INTO notes VALUES ('MATCH (body) AGAINST (\\'x\\')')"
        );
        // A qualified or prefixed name is not the clause.
        assert_eq!(
            rewritten("SELECT t.match FROM t"),
            "SELECT t.match FROM t"
        );
        // No MATCH anywhere: byte-for-byte unchanged.
        let untouched = "SELECT id FROM docs WHERE id = 3";
        assert_eq!(rewritten(untouched), untouched);
    }
}
