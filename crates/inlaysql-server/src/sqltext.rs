//! Text handling for the statements the shim answers itself.
//!
//! This is deliberately *not* a SQL parser. The engine has one, and everything
//! it understands is passed straight through to it. What lives here is the
//! small amount of lexical work needed to recognise the handful of session and
//! metadata statements a driver sends that the engine's dialect has no place
//! for — splitting on commas that are not inside a string, stripping the
//! comments clients wrap their probes in, and matching a `LIKE` pattern.
//!
//! Every function here is quote-aware, because the failure mode of a
//! quote-blind helper is not a parse error: it is finding a keyword inside a
//! string literal and acting on it.

/// Remove comments, outer whitespace and a trailing semicolon.
///
/// Clients prefix statements with comments constantly — connection attributes,
/// tracing ids, and MySQL's own `/*!40101 ... */` version gates. Stripping them
/// first means the recogniser below only ever sees the statement.
///
/// Version-gated comments are removed *with their contents*: they exist to hide
/// MySQL-specific session setup from other databases, and every one a driver
/// sends is a `SET` this server would no-op anyway. Executing their contents
/// would mean implementing more MySQL, not less.
pub fn normalize(sql: &str) -> String {
    let stripped = strip_comments(sql);
    let trimmed = stripped.trim();
    trimmed
        .strip_suffix(';')
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

/// Remove `--`, `#` and `/* */` comments that are not inside a quoted string.
pub fn strip_comments(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                let quote = c;
                out.push(c);
                i += 1;
                while i < chars.len() {
                    let inner = chars[i];
                    // A backslash escape inside a single- or double-quoted
                    // string hides the next character, including a quote.
                    if inner == '\\' && quote != '`' && i + 1 < chars.len() {
                        out.push(inner);
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    out.push(inner);
                    i += 1;
                    if inner == quote {
                        // A doubled quote is an escaped quote, not the end.
                        if i < chars.len() && chars[i] == quote {
                            out.push(quote);
                            i += 1;
                            continue;
                        }
                        break;
                    }
                }
            }
            // MySQL only treats `--` as a comment when whitespace follows, so
            // `a--b` stays an expression.
            '-' if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2].is_whitespace() => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push(' ');
            }
            '-' if i + 2 == chars.len() && chars[i + 1] == '-' => break,
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push(' ');
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
                out.push(' ');
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Strip a leading keyword, respecting word boundaries.
///
/// Returns the rest of the statement, or `None` if the keyword is not there.
/// The boundary check is what stops `SETTINGS` matching `SET`.
pub fn strip_keyword<'a>(sql: &'a str, keyword: &str) -> Option<&'a str> {
    let sql = sql.trim_start();
    if sql.len() < keyword.len() {
        return None;
    }
    let (head, rest) = sql.split_at(keyword.len());
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    match rest.chars().next() {
        None => Some(""),
        Some(c) if c.is_alphanumeric() || c == '_' || c == '$' => None,
        Some(_) => Some(rest.trim_start()),
    }
}

/// Whether the statement begins with `keyword`.
pub fn starts_with_keyword(sql: &str, keyword: &str) -> bool {
    strip_keyword(sql, keyword).is_some()
}

/// The first word, uppercased. Empty for an empty statement.
pub fn first_word(sql: &str) -> String {
    sql.trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Split on a separator that is not inside quotes or parentheses.
pub fn split_top_level(text: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            current.push(c);
            if c == '\\' && q != '`' {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            if c == q {
                if chars.peek() == Some(&q) {
                    current.push(chars.next().unwrap_or(q));
                } else {
                    quote = None;
                }
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                current.push(c);
            }
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            c if c == separator && depth <= 0 => {
                parts.push(current.trim().to_string());
                current = String::new();
            }
            _ => current.push(c),
        }
    }
    parts.push(current.trim().to_string());
    parts
}

/// Find a keyword outside quotes and parentheses, returning its byte offset.
///
/// Used to cut a `SELECT` into its clauses without parsing it. Parenthesised
/// occurrences are skipped so a keyword inside a function call is not mistaken
/// for a clause boundary.
pub fn find_keyword(text: &str, keyword: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let keyword: Vec<char> = keyword.to_ascii_lowercase().chars().collect();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut byte_offset = 0usize;

    for i in 0..chars.len() {
        let c = chars[i];
        let width = c.len_utf8();
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            byte_offset += width;
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {
                if depth == 0 && matches_at(&chars, i, &keyword) {
                    let before_ok = i == 0 || !is_word_char(chars[i - 1]);
                    let after = i + keyword.len();
                    let after_ok = after >= chars.len() || !is_word_char(chars[after]);
                    if before_ok && after_ok {
                        return Some(byte_offset);
                    }
                }
            }
        }
        byte_offset += width;
    }
    None
}

fn matches_at(haystack: &[char], at: usize, needle: &[char]) -> bool {
    if at + needle.len() > haystack.len() {
        return false;
    }
    haystack[at..at + needle.len()]
        .iter()
        .zip(needle)
        .all(|(h, n)| h.to_ascii_lowercase() == *n)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Strip backticks, double quotes or brackets from an identifier.
pub fn unquote_identifier(name: &str) -> String {
    let name = name.trim();
    let mut chars = name.chars();
    match (chars.next(), chars.next_back()) {
        (Some('`'), Some('`')) => chars.as_str().replace("``", "`"),
        (Some('"'), Some('"')) => chars.as_str().replace("\"\"", "\""),
        (Some('['), Some(']')) => chars.as_str().to_string(),
        _ => name.to_string(),
    }
}

/// Read a single-quoted SQL string literal, resolving its escapes.
///
/// Returns `None` if the text is not a quoted literal, which is how the caller
/// tells "this was a string" from "this was something I should refuse".
pub fn unquote_string(text: &str) -> Option<String> {
    let text = text.trim();
    let quote = text.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    if text.chars().count() < 2 || !text.ends_with(quote) {
        return None;
    }
    let inner: Vec<char> = text.chars().collect();
    let inner = &inner[1..inner.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let c = inner[i];
        if c == '\\' && i + 1 < inner.len() {
            out.push(match inner[i + 1] {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '0' => '\0',
                other => other,
            });
            i += 2;
            continue;
        }
        if c == quote && i + 1 < inner.len() && inner[i + 1] == quote {
            out.push(quote);
            i += 2;
            continue;
        }
        // An unescaped closing quote in the middle means this was not one
        // literal but several tokens.
        if c == quote {
            return None;
        }
        out.push(c);
        i += 1;
    }
    Some(out)
}

/// Count the `?` placeholders outside quoted strings.
///
/// This is how a statement the shim answers reports its parameter count at
/// prepare time, where the engine is never asked to plan it.
pub fn count_placeholders(text: &str) -> usize {
    let mut count = 0;
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == '\\' && q != '`' {
                chars.next();
                continue;
            }
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '?' => count += 1,
            _ => {}
        }
    }
    count
}

/// One element of a compiled `LIKE` pattern.
enum PatternItem {
    /// `%` — any run of characters.
    Any,
    /// `_` — exactly one character.
    One,
    /// A literal character, compared case-insensitively.
    Literal(char),
}

/// Match a MySQL `LIKE` pattern, case-insensitively.
///
/// `%` matches any run, `_` exactly one character, and a backslash escapes
/// either — which is not decoration: `SHOW TABLES LIKE 'user\_roles'` means a
/// literal underscore, and treating it as a wildcard returns the wrong tables.
pub fn like_matches(pattern: &str, text: &str) -> bool {
    let mut items = Vec::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                items.push(PatternItem::Literal(lower(chars[i + 1])));
                i += 2;
            }
            '%' => {
                items.push(PatternItem::Any);
                i += 1;
            }
            '_' => {
                items.push(PatternItem::One);
                i += 1;
            }
            other => {
                items.push(PatternItem::Literal(lower(other)));
                i += 1;
            }
        }
    }

    let text: Vec<char> = text.chars().map(lower).collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume from if a `%` turns out to have consumed too little.
    let mut star: Option<(usize, usize)> = None;

    while ti < text.len() {
        match items.get(pi) {
            Some(PatternItem::One) => {
                pi += 1;
                ti += 1;
            }
            Some(PatternItem::Literal(c)) if *c == text[ti] => {
                pi += 1;
                ti += 1;
            }
            Some(PatternItem::Any) => {
                star = Some((pi, ti));
                pi += 1;
            }
            _ => match star {
                Some((star_pi, star_ti)) => {
                    pi = star_pi + 1;
                    ti = star_ti + 1;
                    star = Some((star_pi, star_ti + 1));
                }
                None => return false,
            },
        }
    }
    items[pi.min(items.len())..]
        .iter()
        .all(|item| matches!(item, PatternItem::Any))
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Squash runs of whitespace, for assertions that care about tokens rather
    /// than spacing.
    fn collapse(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn comments_are_stripped_in_every_form() {
        assert_eq!(normalize("SELECT 1 -- trailing"), "SELECT 1");
        assert_eq!(normalize("SELECT 1 # trailing"), "SELECT 1");
        assert_eq!(normalize("/* lead */ SELECT 1").trim(), "SELECT 1");
        assert_eq!(normalize("SELECT 1;"), "SELECT 1");
        // A removed comment leaves whitespace behind rather than joining the
        // tokens either side of it, which is the part that matters; how much
        // whitespace is not.
        assert_eq!(collapse(&normalize("SELECT /* mid */ 1")), "SELECT 1");
        assert_eq!(collapse(&normalize("SELECT a/* x */b")), "SELECT a b");
    }

    /// The reason this is quote-aware: a comment marker inside a string is
    /// data, and removing it would silently change the statement.
    #[test]
    fn a_comment_marker_inside_a_string_is_left_alone() {
        assert_eq!(
            normalize("SELECT '-- not a comment'"),
            "SELECT '-- not a comment'"
        );
        assert_eq!(
            normalize("SELECT '/* nor this */'"),
            "SELECT '/* nor this */'"
        );
        assert_eq!(normalize("SELECT '# nor this'"), "SELECT '# nor this'");
        assert_eq!(normalize("SELECT `a--b`"), "SELECT `a--b`");
    }

    #[test]
    fn a_version_gated_comment_is_removed_with_its_contents() {
        assert_eq!(
            normalize("/*!40101 SET NAMES utf8 */ SELECT 1").trim(),
            "SELECT 1"
        );
    }

    #[test]
    fn double_dash_needs_whitespace_to_start_a_comment() {
        assert_eq!(normalize("SELECT 1--2"), "SELECT 1--2");
        assert_eq!(normalize("SELECT 1 -- x"), "SELECT 1");
    }

    #[test]
    fn keywords_respect_word_boundaries() {
        assert_eq!(strip_keyword("SET NAMES utf8", "SET"), Some("NAMES utf8"));
        assert_eq!(strip_keyword("SETTINGS x", "SET"), None);
        assert_eq!(strip_keyword("set names utf8", "SET"), Some("names utf8"));
        assert_eq!(strip_keyword("SET", "SET"), Some(""));
        assert_eq!(strip_keyword("SET=1", "SET"), Some("=1"));
    }

    #[test]
    fn top_level_splitting_ignores_separators_inside_quotes_and_parens() {
        assert_eq!(split_top_level("a, b, c", ','), vec!["a", "b", "c"]);
        assert_eq!(split_top_level("f(a, b), c", ','), vec!["f(a, b)", "c"]);
        assert_eq!(split_top_level("'a, b', c", ','), vec!["'a, b'", "c"]);
        assert_eq!(split_top_level("`a, b`, c", ','), vec!["`a, b`", "c"]);
    }

    #[test]
    fn find_keyword_skips_quotes_and_parentheses() {
        assert_eq!(find_keyword("SELECT a FROM t", "from"), Some(9));
        assert_eq!(find_keyword("SELECT 'from' FROM t", "from"), Some(14));
        assert_eq!(find_keyword("SELECT f(x from y)", "from"), None);
        assert_eq!(find_keyword("SELECT fromage", "from"), None);
        assert_eq!(find_keyword("SELECT a FROMAGE", "from"), None);
    }

    #[test]
    fn identifiers_lose_every_quoting_style() {
        assert_eq!(unquote_identifier("`users`"), "users");
        assert_eq!(unquote_identifier("\"users\""), "users");
        assert_eq!(unquote_identifier("[users]"), "users");
        assert_eq!(unquote_identifier("users"), "users");
        assert_eq!(unquote_identifier("`we``ird`"), "we`ird");
    }

    #[test]
    fn string_literals_resolve_their_escapes() {
        assert_eq!(unquote_string("'abc'").as_deref(), Some("abc"));
        assert_eq!(unquote_string("'a\\'b'").as_deref(), Some("a'b"));
        assert_eq!(unquote_string("'a''b'").as_deref(), Some("a'b"));
        assert_eq!(unquote_string("'a\\nb'").as_deref(), Some("a\nb"));
        assert_eq!(unquote_string("notquoted"), None);
        // Two adjacent literals are not one literal.
        assert_eq!(unquote_string("'a' 'b'"), None);
    }

    #[test]
    fn like_handles_wildcards_and_anchoring() {
        assert!(like_matches("%", "anything"));
        assert!(like_matches("user%", "users"));
        assert!(like_matches("%ers", "users"));
        assert!(like_matches("u_ers", "users"));
        assert!(like_matches("users", "USERS"), "LIKE is case-insensitive");
        assert!(!like_matches("user", "users"));
        assert!(!like_matches("u_er", "users"));
        assert!(like_matches("%s%r%", "sugar"));
        assert!(like_matches("", ""));
        assert!(!like_matches("", "x"));
    }

    /// The case that returns the wrong tables if escapes are ignored.
    #[test]
    fn like_treats_an_escaped_wildcard_as_a_literal() {
        assert!(like_matches("user\\_roles", "user_roles"));
        assert!(!like_matches("user\\_roles", "userXroles"));
        assert!(like_matches("100\\%", "100%"));
        assert!(!like_matches("100\\%", "100abc"));
    }

    /// Backtracking has to actually backtrack, not take the first match.
    #[test]
    fn like_backtracks_when_a_wildcard_took_too_little() {
        assert!(like_matches("%abc", "xxabcxxabc"));
        assert!(like_matches("a%b%c", "aXXbYYc"));
        assert!(!like_matches("%abc", "xxabcxx"));
    }
}
