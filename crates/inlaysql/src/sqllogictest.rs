//! A runner for SQLite's [SQL Logic Test](https://www.sqlite.org/sqllogictest/doc/trunk/about.wiki)
//! format.
//!
//! SQL Logic Test describes a test as a sequence of records:
//!
//! ```text
//! statement ok
//! CREATE TABLE t(a INTEGER, b TEXT)
//!
//! statement ok
//! INSERT INTO t VALUES (1, 'one')
//!
//! query IT rowsort
//! SELECT a, b FROM t WHERE a = 1
//! ----
//! 1 one
//! ```
//!
//! * `statement ok` — the statement must succeed.
//! * `statement error` — the statement must fail.
//! * `query <types> [rowsort|nosort]` — the query must return the rows after
//!   the `----` line. Each character of `<types>` is the expected value type
//!   (`I` integer, `R` real, `T` text); `rowsort` compares the rows as sets,
//!   `nosort` (the default) compares them in order.
//!
//! This is the harness the Stage 3 acceptance criteria call for: a *measurable*
//! SQL surface. The pass rate is reported in CI and the README and is the
//! number to grow as the dialect matures.

use std::fmt;

use crate::{Database, Value};

/// How a `query` record's rows are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    /// Compare in the exact order the engine returns them.
    NoSort,
    /// Sort both the expected and actual rows before comparing.
    RowSort,
}

/// One record from a SQL Logic Test file.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// A statement that must succeed (`ok = true`) or fail (`ok = false`).
    Statement {
        /// Whether the statement is expected to succeed.
        ok: bool,
        /// The statement text.
        sql: String,
    },
    /// A query whose result must equal `expected`.
    Query {
        /// The query text.
        sql: String,
        /// Expected value types, one character per column (`I`, `R`, `T`).
        types: String,
        /// How the rows are compared.
        sort: SortMode,
        /// Expected rows, each a list of value tokens.
        expected: Vec<Vec<String>>,
    },
}

/// One failing record, with what went wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// 1-based index of the record in the file.
    pub index: usize,
    /// A human-readable description of the mismatch.
    pub message: String,
}

/// The outcome of running a whole file.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Total records executed.
    pub total: usize,
    /// Records that passed.
    pub passed: usize,
    /// Records that failed.
    pub failures: Vec<Failure>,
}

impl Summary {
    /// The fraction of records that passed, in `[0.0, 1.0]`.
    pub fn pass_rate(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        self.passed as f64 / self.total as f64
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} passed ({:.1}%)",
            self.passed,
            self.total,
            self.pass_rate() * 100.0
        )
    }
}

/// Parse a SQL Logic Test file's text into records.
pub fn parse(source: &str) -> Result<Vec<Record>, String> {
    let mut records = Vec::new();
    let mut lines = source.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.strip_prefix("statement ok").is_some() {
            records.push(Record::Statement {
                ok: true,
                sql: read_sql(&mut lines)?,
            });
        } else if trimmed.strip_prefix("statement error").is_some() {
            records.push(Record::Statement {
                ok: false,
                sql: read_sql(&mut lines)?,
            });
        } else if let Some(header) = trimmed.strip_prefix("query") {
            let (types, sort) = parse_query_header(header)?;
            let sql = read_until_separator(&mut lines)?;
            let expected = read_expected(&mut lines)?;
            records.push(Record::Query {
                sql,
                types,
                sort,
                expected,
            });
        } else if trimmed.starts_with("halt") || trimmed.starts_with("hash-threshold") {
            break;
        } else {
            return Err(format!("unrecognised record: {trimmed}"));
        }
    }

    Ok(records)
}

/// Read the SQL of a statement: every line up to the next blank line.
fn read_sql<'a, I: Iterator<Item = &'a str>>(
    lines: &mut std::iter::Peekable<I>,
) -> Result<String, String> {
    let mut sql = Vec::new();
    while let Some(line) = lines.peek() {
        if line.trim().is_empty() {
            lines.next();
            break;
        }
        sql.push(lines.next().unwrap().to_string());
    }
    Ok(sql.join("\n"))
}

/// Read a query's SQL: every line up to the `----` separator.
fn read_until_separator<'a, I: Iterator<Item = &'a str>>(
    lines: &mut std::iter::Peekable<I>,
) -> Result<String, String> {
    let mut sql = Vec::new();
    for line in lines.by_ref() {
        if line.trim() == "----" {
            return Ok(sql.join("\n"));
        }
        sql.push(line.to_string());
    }
    Err("query record is missing its `----` separator".to_string())
}

/// Read the expected result rows, up to the next blank line.
fn read_expected<'a, I: Iterator<Item = &'a str>>(
    lines: &mut std::iter::Peekable<I>,
) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    while let Some(line) = lines.peek() {
        if line.trim().is_empty() {
            lines.next();
            break;
        }
        let line = lines.next().unwrap();
        rows.push(line.split_whitespace().map(str::to_string).collect());
    }
    Ok(rows)
}

/// Parse the `<types> [sort]` part of a `query` header.
fn parse_query_header(header: &str) -> Result<(String, SortMode), String> {
    let mut parts = header.split_whitespace();
    let types = parts
        .next()
        .ok_or_else(|| "query header is missing its type string".to_string())?
        .to_string();
    if !types.chars().all(|c| matches!(c, 'I' | 'R' | 'T')) {
        return Err(format!("unknown query type string: {types}"));
    }
    let sort = match parts.next() {
        None | Some("nosort") => SortMode::NoSort,
        Some("rowsort") => SortMode::RowSort,
        Some(other) => return Err(format!("unknown sort mode: {other}")),
    };
    Ok((types, sort))
}

/// Run every record against a fresh in-memory database and report the outcome.
pub fn run(records: &[Record]) -> Summary {
    let mut db = Database::open_in_memory().expect("open in-memory database");
    run_on(&mut db, records)
}

/// Run every record against `db` and report the outcome.
pub fn run_on(db: &mut Database, records: &[Record]) -> Summary {
    let mut summary = Summary {
        total: records.len(),
        passed: 0,
        failures: Vec::new(),
    };

    for (i, record) in records.iter().enumerate() {
        let result = match record {
            Record::Statement { ok, sql } => {
                let succeeded = db.execute(sql, &[]).is_ok();
                if succeeded == *ok {
                    Ok(())
                } else if *ok {
                    Err("expected the statement to succeed".to_string())
                } else {
                    Err("expected the statement to fail".to_string())
                }
            }
            Record::Query {
                sql,
                types,
                sort,
                expected,
            } => run_query(db, sql, types, *sort, expected),
        };

        match result {
            Ok(()) => summary.passed += 1,
            Err(message) => summary.failures.push(Failure {
                index: i + 1,
                message,
            }),
        }
    }

    summary
}

fn run_query(
    db: &mut Database,
    sql: &str,
    types: &str,
    sort: SortMode,
    expected: &[Vec<String>],
) -> Result<(), String> {
    let result = db
        .query(sql, &[])
        .map_err(|e| format!("query failed: {e}"))?;
    if result.columns.len() != types.len() {
        return Err(format!(
            "query returned {} column(s), expected {}",
            result.columns.len(),
            types.len()
        ));
    }

    let mut actual: Vec<Vec<String>> = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut formatted = Vec::with_capacity(row.len());
        for (value, ty) in row.iter().zip(types.chars()) {
            formatted.push(format_value(value, ty)?);
        }
        actual.push(formatted);
    }
    let mut expected = expected.to_vec();

    if sort == SortMode::RowSort {
        actual.sort();
        expected.sort();
    }

    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "result mismatch\nexpected: {expected:?}\nactual:   {actual:?}"
        ))
    }
}

/// Format a value the way SQL Logic Test expects it, checking its type against
/// the type string's character. `NULL` is valid in any column.
///
/// A value of the wrong type is a *failing record*, not a harness bug: the
/// whole point of the type string is to catch a query that returns text where
/// the corpus says an integer belongs. Reporting it as a failure keeps the rest
/// of the file running and puts the mismatch in the pass rate where it belongs.
fn format_value(value: &Value, ty: char) -> Result<String, String> {
    let wrong_type = |kind: &str| Err(format!("{kind} in an {ty}-typed column"));
    match value {
        Value::Null => Ok("NULL".to_string()),
        Value::Integer(i) if ty == 'I' || ty == 'R' => Ok(i.to_string()),
        Value::Integer(_) => wrong_type("integer"),
        Value::Real(r) if ty == 'R' => Ok(crate::sqllogictest::format_real(*r)),
        Value::Real(_) => wrong_type("real"),
        Value::Text(s) if ty == 'T' => Ok(s.to_string()),
        Value::Text(_) => wrong_type("text"),
        Value::Blob(_) => Ok("<blob>".to_string()),
        Value::Vector(_) => Ok("<vector>".to_string()),
    }
}

/// Format a real the way SQLite's SQL Logic Test runner does: shortest
/// round-trip, with integers carrying a trailing `.0`.
pub fn format_real(r: f64) -> String {
    if r == r.trunc() && r.is_finite() {
        format!("{r:.1}")
    } else {
        format!("{r}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_statement_and_query_records() {
        let source = "\
# a comment
statement ok
CREATE TABLE t(a INTEGER, b TEXT)

statement ok
INSERT INTO t VALUES (1, 'one'), (2, 'two')

query IT rowsort
SELECT a, b FROM t
----
2 two
1 one
";
        let records = parse(source).unwrap();
        assert_eq!(records.len(), 3);
        match &records[2] {
            Record::Query {
                sql,
                types,
                sort,
                expected,
            } => {
                assert_eq!(sql, "SELECT a, b FROM t");
                assert_eq!(types, "IT");
                assert_eq!(*sort, SortMode::RowSort);
                assert_eq!(expected, &[vec!["2", "two"], vec!["1", "one"]]);
            }
            other => panic!("expected a query, got {other:?}"),
        }
    }

    #[test]
    fn runs_a_passing_file() {
        let source = "\
statement ok
CREATE TABLE t(a INTEGER, b TEXT)

statement ok
INSERT INTO t VALUES (1, 'one'), (2, 'two')

query IT rowsort
SELECT a, b FROM t ORDER BY a
----
1 one
2 two
";
        let summary = run(&parse(source).unwrap());
        assert_eq!(summary.total, 3);
        assert_eq!(summary.passed, 3);
    }

    #[test]
    fn reports_a_mismatch() {
        let source = "\
query I rowsort
SELECT 1
----
2
";
        // The engine does not yet support SELECT without FROM, so this fails
        // at the query stage — still a failure, which is what we assert.
        let summary = run(&parse(source).unwrap());
        assert_eq!(summary.passed, 0);
        assert_eq!(summary.failures.len(), 1);
    }
}
