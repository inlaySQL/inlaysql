//! `information_schema` queries, answered from [`Catalog`].
//!
//! An ORM discovers a schema by querying these views, so they have to work; the
//! engine has no schemas and no subqueries, so they cannot be forwarded to it.
//! What is here is a deliberately small evaluator over a handful of fixed
//! relations: a projection, a conjunction of simple comparisons, an optional
//! sort and an optional limit.
//!
//! # Why it refuses so much
//!
//! It would be easy to ignore a `WHERE` clause this evaluator cannot parse and
//! return every row. That is the one thing it must never do. A tool asking
//! "does column `email` exist on `users`?" and receiving every column of every
//! table will conclude the answer is yes. Every clause that is not understood
//! becomes an error naming the clause, so the caller finds out at the query
//! that failed rather than three migrations later.
//!
//! Bound parameters are resolved as [`Value`]s inside the comparison that uses
//! them. They are never spliced into SQL text, so there is no injection path
//! through this module even though it does its own parsing.

use inlaysql::{Catalog, DataType, Value};

use crate::errors::MysqlError;
use crate::session::Session;
use crate::shim::{rows_owned, schema_name, Intercepted};
use crate::sqltext::{
    count_placeholders, find_keyword, split_top_level, starts_with_keyword, strip_keyword,
    unquote_identifier, unquote_string,
};

/// `information_schema.TABLES`, in MySQL's column order.
const TABLES_COLUMNS: &[&str] = &[
    "TABLE_CATALOG",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "TABLE_TYPE",
    "ENGINE",
    "VERSION",
    "ROW_FORMAT",
    "TABLE_ROWS",
    "AVG_ROW_LENGTH",
    "DATA_LENGTH",
    "MAX_DATA_LENGTH",
    "INDEX_LENGTH",
    "DATA_FREE",
    "AUTO_INCREMENT",
    "CREATE_TIME",
    "UPDATE_TIME",
    "CHECK_TIME",
    "TABLE_COLLATION",
    "CHECKSUM",
    "CREATE_OPTIONS",
    "TABLE_COMMENT",
];

/// `information_schema.COLUMNS`.
const COLUMNS_COLUMNS: &[&str] = &[
    "TABLE_CATALOG",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "COLUMN_NAME",
    "ORDINAL_POSITION",
    "COLUMN_DEFAULT",
    "IS_NULLABLE",
    "DATA_TYPE",
    "CHARACTER_MAXIMUM_LENGTH",
    "CHARACTER_OCTET_LENGTH",
    "NUMERIC_PRECISION",
    "NUMERIC_SCALE",
    "DATETIME_PRECISION",
    "CHARACTER_SET_NAME",
    "COLLATION_NAME",
    "COLUMN_TYPE",
    "COLUMN_KEY",
    "EXTRA",
    "PRIVILEGES",
    "COLUMN_COMMENT",
    "GENERATION_EXPRESSION",
];

/// `information_schema.SCHEMATA`.
const SCHEMATA_COLUMNS: &[&str] = &[
    "CATALOG_NAME",
    "SCHEMA_NAME",
    "DEFAULT_CHARACTER_SET_NAME",
    "DEFAULT_COLLATION_NAME",
    "SQL_PATH",
];

/// `information_schema.STATISTICS`.
const STATISTICS_COLUMNS: &[&str] = &[
    "TABLE_CATALOG",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "NON_UNIQUE",
    "INDEX_SCHEMA",
    "INDEX_NAME",
    "SEQ_IN_INDEX",
    "COLUMN_NAME",
    "COLLATION",
    "CARDINALITY",
    "SUB_PART",
    "PACKED",
    "NULLABLE",
    "INDEX_TYPE",
    "COMMENT",
    "INDEX_COMMENT",
];

/// Answer an `information_schema` query.
pub fn query(sql: &str, params: &[Value], catalog: &Catalog, session: &Session) -> Intercepted {
    match evaluate(sql, params, catalog, session) {
        Ok(intercepted) => intercepted,
        Err(error) => Intercepted::Failed(error),
    }
}

fn evaluate(
    sql: &str,
    params: &[Value],
    catalog: &Catalog,
    session: &Session,
) -> Result<Intercepted, MysqlError> {
    let from_at = find_keyword(sql, "from")
        .ok_or_else(|| MysqlError::parse("expected FROM in an information_schema query"))?;

    // Clause boundaries, in the order they may appear after FROM.
    let where_at = find_keyword(sql, "where");
    let group_at = find_clause(sql, "group");
    let having_at = find_keyword(sql, "having");
    let order_at = find_clause(sql, "order");
    let limit_at = find_keyword(sql, "limit");

    if group_at.is_some() || having_at.is_some() {
        return Err(MysqlError::unsupported(
            "GROUP BY and HAVING are not supported on information_schema queries",
        ));
    }

    let select_list = &sql[6..from_at];
    let from_end = [where_at, order_at, limit_at]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(sql.len());
    let from_clause = &sql[from_at + 4..from_end];

    let relation = relation_of(from_clause)?;
    let (columns, mut data) = build(relation, catalog, session);

    // WHERE
    if let Some(at) = where_at {
        let end = [order_at, limit_at]
            .into_iter()
            .flatten()
            .filter(|end| *end > at)
            .min()
            .unwrap_or(sql.len());
        let clause = &sql[at + 5..end];
        let base = count_placeholders(&sql[..at]);
        let predicates = parse_where(clause, columns, base, session)?;
        data.retain(|row| predicates.iter().all(|p| p.matches(row, params)));
    }

    // ORDER BY
    if let Some(at) = order_at {
        let end = limit_at.filter(|end| *end > at).unwrap_or(sql.len());
        let clause = &sql[at + 5..end];
        let clause = strip_keyword(clause, "BY").ok_or_else(|| {
            MysqlError::parse("expected BY after ORDER in an information_schema query")
        })?;
        let keys = parse_order_by(clause, columns)?;
        data.sort_by(|a, b| {
            for (index, ascending) in &keys {
                let ordering = sort_key(&a[*index]).cmp(&sort_key(&b[*index]));
                let ordering = if *ascending {
                    ordering
                } else {
                    ordering.reverse()
                };
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // LIMIT
    if let Some(at) = limit_at {
        let (limit, offset) = parse_limit(&sql[at + 5..])?;
        if offset > 0 {
            data = data.into_iter().skip(offset).collect();
        }
        data.truncate(limit);
    }

    project(select_list, columns, data)
}

/// `ORDER`/`GROUP` are only clause keywords when `BY` follows.
fn find_clause(sql: &str, keyword: &str) -> Option<usize> {
    let at = find_keyword(sql, keyword)?;
    let rest = &sql[at + keyword.len()..];
    if starts_with_keyword(rest, "BY") {
        Some(at)
    } else {
        None
    }
}

/// Which view is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Tables,
    Columns,
    Schemata,
    Statistics,
}

fn relation_of(from_clause: &str) -> Result<Relation, MysqlError> {
    let first = from_clause
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(',');
    let name = split_top_level(first, '.')
        .last()
        .map(|part| unquote_identifier(part))
        .unwrap_or_default()
        .to_ascii_lowercase();

    // A join between information_schema views would need a planner this shim
    // does not have; saying so beats returning one side of it.
    if from_clause.split_whitespace().count() > 2 || from_clause.contains(',') {
        return Err(MysqlError::unsupported(
            "only a single information_schema table may be queried at a time; \
             joins and subqueries against information_schema are not supported",
        ));
    }

    match name.as_str() {
        "tables" => Ok(Relation::Tables),
        "columns" => Ok(Relation::Columns),
        "schemata" => Ok(Relation::Schemata),
        "statistics" => Ok(Relation::Statistics),
        other => Err(MysqlError::unsupported(format!(
            "information_schema.{other} is not implemented; this server provides \
             TABLES, COLUMNS, SCHEMATA and STATISTICS"
        ))),
    }
}

fn build(
    relation: Relation,
    catalog: &Catalog,
    session: &Session,
) -> (&'static [&'static str], Vec<Vec<Value>>) {
    let schema = schema_name(session);
    let text = |value: &str| Value::Text(value.to_string());

    match relation {
        Relation::Schemata => (
            SCHEMATA_COLUMNS,
            vec![vec![
                text("def"),
                text(&schema),
                text("utf8mb4"),
                text("utf8mb4_general_ci"),
                Value::Null,
            ]],
        ),

        Relation::Tables => (
            TABLES_COLUMNS,
            catalog
                .tables()
                .map(|table| {
                    vec![
                        text("def"),
                        text(&schema),
                        text(&table.name),
                        text("BASE TABLE"),
                        text("InlaySQL"),
                        Value::Integer(10),
                        text("Dynamic"),
                        // Unknown rather than zero: the engine keeps no row
                        // count, and zero would read as "this table is empty".
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        text("utf8mb4_general_ci"),
                        Value::Null,
                        text(""),
                        text(""),
                    ]
                })
                .collect(),
        ),

        Relation::Columns => {
            let mut data = Vec::new();
            for table in catalog.tables() {
                let indexes = catalog.indexes_for(&table.name);
                for (position, column) in table.columns.iter().enumerate() {
                    let is_text = matches!(
                        column.ty,
                        DataType::Text | DataType::Vector(_) | DataType::QuantizedVector(_)
                    );
                    let key = if column.primary_key {
                        "PRI"
                    } else if indexes.iter().any(|index| {
                        index
                            .columns
                            .iter()
                            .any(|name| name.eq_ignore_ascii_case(&column.name))
                    }) {
                        "MUL"
                    } else {
                        ""
                    };
                    data.push(vec![
                        text("def"),
                        text(&schema),
                        text(&table.name),
                        text(&column.name),
                        Value::Integer(position as i64 + 1),
                        // No DEFAULT exists, because the engine refuses the
                        // syntax that would create one.
                        Value::Null,
                        text("YES"),
                        text(&data_type_name(column.ty)),
                        Value::Null,
                        Value::Null,
                        if matches!(column.ty, DataType::Integer) {
                            Value::Integer(19)
                        } else if matches!(column.ty, DataType::Real) {
                            Value::Integer(22)
                        } else {
                            Value::Null
                        },
                        if matches!(column.ty, DataType::Integer) {
                            Value::Integer(0)
                        } else {
                            Value::Null
                        },
                        Value::Null,
                        if is_text {
                            text("utf8mb4")
                        } else {
                            Value::Null
                        },
                        // The name of the collation the column really has,
                        // not a fixed one — see `shim::show_columns`, which
                        // reports the same fact through the other spelling.
                        if is_text {
                            text(mysql_collation_name(column.collation))
                        } else {
                            Value::Null
                        },
                        text(&column_type_name(column.ty)),
                        text(key),
                        text(""),
                        text("select,insert,update,references"),
                        text(""),
                        text(""),
                    ]);
                }
            }
            (COLUMNS_COLUMNS, data)
        }

        Relation::Statistics => {
            let mut data = Vec::new();
            for table in catalog.tables() {
                let mut push = |non_unique: i64, name: &str, column: &str, kind: &str| {
                    data.push(vec![
                        text("def"),
                        text(&schema),
                        text(&table.name),
                        Value::Integer(non_unique),
                        text(&schema),
                        text(name),
                        Value::Integer(1),
                        text(column),
                        text("A"),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        text("YES"),
                        text(kind),
                        text(""),
                        text(""),
                    ]);
                };
                if let Some(position) = table.rowid_alias() {
                    push(0, "PRIMARY", &table.columns[position].name, "BTREE");
                }
                for index in catalog.indexes_for(&table.name) {
                    let kind = match index.kind {
                        inlaysql::IndexKind::FullText => "FULLTEXT",
                        inlaysql::IndexKind::Vector => "VECTOR",
                        inlaysql::IndexKind::BTree => "BTREE",
                    };
                    // One row per column, as MySQL emits for a composite key.
                    for column in &index.columns {
                        push(i64::from(!index.unique), &index.name, column, kind);
                    }
                }
            }
            (STATISTICS_COLUMNS, data)
        }
    }
}

/// `DATA_TYPE`: the bare type name.
/// The MySQL collation name whose behaviour a column actually has.
///
/// Shared with `shim::show_columns` in spirit and stated once here: this is a
/// claim about *comparison*, so it has to follow the column's declared
/// collation rather than name a fixed one (AHL-469). `RTRIM` has no MySQL
/// counterpart — MySQL pads a `CHAR` rather than trimming a comparison — so it
/// is reported under a name no MySQL client will mistake for one of its own.
pub(crate) fn mysql_collation_name(collation: inlaysql::Collation) -> &'static str {
    match collation {
        inlaysql::Collation::NoCase => "utf8mb4_general_ci",
        inlaysql::Collation::Binary => "utf8mb4_bin",
        inlaysql::Collation::RTrim => "inlaysql_rtrim",
    }
}

fn data_type_name(ty: DataType) -> String {
    match ty {
        DataType::Integer => "bigint".to_string(),
        DataType::Real => "double".to_string(),
        DataType::Text => "text".to_string(),
        DataType::Blob => "blob".to_string(),
        // SQLite's NUMERIC *affinity*, not MySQL's fixed-point `DECIMAL` —
        // integer when the value is exactly one, double otherwise, decided per
        // row. `numeric` is the nearest true MySQL spelling; the precision and
        // scale a client may infer from it do not exist here. See
        // docs/server.md, "Divergences".
        DataType::Numeric => "numeric".to_string(),
        DataType::Vector(_) | DataType::QuantizedVector(_) => "vector".to_string(),
    }
}

/// `COLUMN_TYPE`: the type as it would be written in DDL.
fn column_type_name(ty: DataType) -> String {
    match ty {
        DataType::Vector(dim) => format!("vector({dim})"),
        DataType::QuantizedVector(dim) => format!("vector({dim},int8)"),
        other => data_type_name(other),
    }
}

// ------------------------------------------------------------- predicates

/// The right-hand side of a comparison.
#[derive(Debug, Clone)]
enum Operand {
    /// A literal written in the statement.
    Literal(String),
    /// A `?`, resolved from the bound parameters at evaluation time.
    Param(usize),
    /// SQL `NULL`.
    Null,
}

#[derive(Debug, Clone)]
enum Test {
    Equals(Operand),
    NotEquals(Operand),
    Like(Operand),
    In(Vec<Operand>),
    IsNull,
    IsNotNull,
}

#[derive(Debug, Clone)]
struct Predicate {
    column: usize,
    test: Test,
}

impl Predicate {
    fn matches(&self, row: &[Value], params: &[Value]) -> bool {
        let cell = &row[self.column];
        match &self.test {
            Test::IsNull => matches!(cell, Value::Null),
            Test::IsNotNull => !matches!(cell, Value::Null),
            Test::Equals(operand) => compare(cell, operand, params, false),
            Test::NotEquals(operand) => {
                !matches!(cell, Value::Null) && !compare(cell, operand, params, false)
            }
            Test::Like(operand) => compare(cell, operand, params, true),
            Test::In(operands) => operands
                .iter()
                .any(|operand| compare(cell, operand, params, false)),
        }
    }
}

/// Compare a cell against an operand.
///
/// `information_schema` values are identifiers and enumerations, which MySQL
/// compares under a case-insensitive collation; matching that here is what
/// makes `WHERE table_name = 'Users'` find `users`, as it would on MySQL.
fn compare(cell: &Value, operand: &Operand, params: &[Value], like: bool) -> bool {
    let expected = match operand {
        Operand::Null => return false,
        Operand::Literal(text) => text.clone(),
        Operand::Param(index) => match params.get(*index) {
            None | Some(Value::Null) => return false,
            Some(value) => render(value),
        },
    };
    let actual = match cell {
        Value::Null => return false,
        other => render(other),
    };
    if like {
        crate::sqltext::like_matches(&expected, &actual)
    } else {
        actual.eq_ignore_ascii_case(&expected)
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Text(text) => text.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Vector(_) => String::new(),
    }
}

fn sort_key(value: &Value) -> String {
    render(value).to_ascii_lowercase()
}

fn parse_where(
    clause: &str,
    columns: &[&str],
    base_placeholder: usize,
    session: &Session,
) -> Result<Vec<Predicate>, MysqlError> {
    if find_keyword(clause, "or").is_some() {
        return Err(MysqlError::unsupported(
            "OR in an information_schema WHERE clause is not supported",
        ));
    }
    let mut placeholder = base_placeholder;
    let mut predicates = Vec::new();

    for conjunct in split_on_keyword(clause, "and") {
        let conjunct = strip_outer_parens(conjunct.trim());
        if conjunct.is_empty() {
            continue;
        }
        predicates.push(parse_predicate(
            conjunct,
            columns,
            &mut placeholder,
            session,
        )?);
    }
    Ok(predicates)
}

fn parse_predicate(
    text: &str,
    columns: &[&str],
    placeholder: &mut usize,
    session: &Session,
) -> Result<Predicate, MysqlError> {
    let unsupported = || {
        MysqlError::unsupported(format!(
            "`{text}` is not a comparison this server can evaluate against \
             information_schema; supported forms are `col = x`, `col != x`, \
             `col LIKE x`, `col IN (...)` and `col IS [NOT] NULL`"
        ))
    };

    if let Some(at) = find_keyword(text, "is") {
        let column = resolve_column(&text[..at], columns)?;
        let rest = text[at + 2..].trim();
        return if starts_with_keyword(rest, "NOT") {
            Ok(Predicate {
                column,
                test: Test::IsNotNull,
            })
        } else {
            Ok(Predicate {
                column,
                test: Test::IsNull,
            })
        };
    }

    if let Some(at) = find_keyword(text, "in") {
        let column = resolve_column(&text[..at], columns)?;
        let list = text[at + 2..].trim();
        let list = list
            .strip_prefix('(')
            .and_then(|l| l.strip_suffix(')'))
            .ok_or_else(unsupported)?;
        let mut operands = Vec::new();
        for item in split_top_level(list, ',') {
            operands.push(parse_operand(item.trim(), placeholder, session)?);
        }
        return Ok(Predicate {
            column,
            test: Test::In(operands),
        });
    }

    if let Some(at) = find_keyword(text, "like") {
        let column = resolve_column(&text[..at], columns)?;
        let operand = parse_operand(text[at + 4..].trim(), placeholder, session)?;
        return Ok(Predicate {
            column,
            test: Test::Like(operand),
        });
    }

    for (token, negated) in [("<>", true), ("!=", true), ("=", false)] {
        if let Some(at) = text.find(token) {
            let column = resolve_column(&text[..at], columns)?;
            let operand = parse_operand(text[at + token.len()..].trim(), placeholder, session)?;
            return Ok(Predicate {
                column,
                test: if negated {
                    Test::NotEquals(operand)
                } else {
                    Test::Equals(operand)
                },
            });
        }
    }

    Err(unsupported())
}

fn parse_operand(
    text: &str,
    placeholder: &mut usize,
    session: &Session,
) -> Result<Operand, MysqlError> {
    // Clients sometimes force a binary comparison; the operand is unchanged.
    let text = strip_keyword(text, "BINARY").unwrap_or(text).trim();

    if text == "?" {
        let index = *placeholder;
        *placeholder += 1;
        return Ok(Operand::Param(index));
    }
    if text.eq_ignore_ascii_case("null") {
        return Ok(Operand::Null);
    }
    let call = text.to_ascii_lowercase().replace(' ', "");
    if call == "database()" || call == "schema()" {
        return Ok(Operand::Literal(schema_name(session)));
    }
    if let Some(literal) = unquote_string(text) {
        return Ok(Operand::Literal(literal));
    }
    if text.parse::<i64>().is_ok() || text.parse::<f64>().is_ok() {
        return Ok(Operand::Literal(text.to_string()));
    }
    Err(MysqlError::unsupported(format!(
        "`{text}` is not a value this server can compare against information_schema"
    )))
}

fn resolve_column(text: &str, columns: &[&str]) -> Result<usize, MysqlError> {
    let name = split_top_level(text.trim(), '.')
        .last()
        .map(|part| unquote_identifier(part))
        .unwrap_or_default();
    columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(&name))
        .ok_or_else(|| {
            MysqlError::bad_field(format!(
                "Unknown column '{}' in 'where clause'",
                name.trim()
            ))
        })
}

/// Split on a keyword at every top-level occurrence.
fn split_on_keyword<'a>(text: &'a str, keyword: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut rest = text;
    let mut consumed = 0;
    while let Some(at) = find_keyword(&text[consumed..], keyword) {
        let absolute = consumed + at;
        parts.push(&text[consumed..absolute]);
        consumed = absolute + keyword.len();
        rest = &text[consumed..];
    }
    parts.push(rest);
    parts
}

fn strip_outer_parens(text: &str) -> &str {
    let trimmed = text.trim();
    if !trimmed.starts_with('(') || !trimmed.ends_with(')') {
        return trimmed;
    }
    // Only strip when the opening paren is closed by the final one.
    let mut depth = 0i32;
    for (i, c) in trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i + 1 != trimmed.len() {
                    return trimmed;
                }
            }
            _ => {}
        }
    }
    trimmed[1..trimmed.len() - 1].trim()
}

fn parse_order_by(clause: &str, columns: &[&str]) -> Result<Vec<(usize, bool)>, MysqlError> {
    let mut keys = Vec::new();
    for key in split_top_level(clause, ',') {
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let (name, ascending) = if let Some(rest) = strip_suffix_keyword(key, "DESC") {
            (rest, false)
        } else if let Some(rest) = strip_suffix_keyword(key, "ASC") {
            (rest, true)
        } else {
            (key, true)
        };
        keys.push((resolve_column(name, columns)?, ascending));
    }
    Ok(keys)
}

fn strip_suffix_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = text.trim_end();
    if trimmed.len() <= keyword.len() {
        return None;
    }
    let (head, tail) = trimmed.split_at(trimmed.len() - keyword.len());
    if tail.eq_ignore_ascii_case(keyword) && head.ends_with(char::is_whitespace) {
        Some(head.trim_end())
    } else {
        None
    }
}

fn parse_limit(text: &str) -> Result<(usize, usize), MysqlError> {
    let text = text.trim();
    if let Some(at) = find_keyword(text, "offset") {
        let limit = parse_count(&text[..at])?;
        let offset = parse_count(&text[at + 6..])?;
        return Ok((limit, offset));
    }
    let parts = split_top_level(text, ',');
    match parts.len() {
        1 => Ok((parse_count(&parts[0])?, 0)),
        // `LIMIT offset, count` — the arguments are the other way round.
        2 => Ok((parse_count(&parts[1])?, parse_count(&parts[0])?)),
        _ => Err(MysqlError::parse("could not read the LIMIT clause")),
    }
}

fn parse_count(text: &str) -> Result<usize, MysqlError> {
    text.trim().parse().map_err(|_| {
        MysqlError::unsupported(format!("LIMIT `{}` must be a literal number", text.trim()))
    })
}

// ------------------------------------------------------------- projection

fn project(
    select_list: &str,
    columns: &[&str],
    data: Vec<Vec<Value>>,
) -> Result<Intercepted, MysqlError> {
    let items: Vec<String> = split_top_level(select_list, ',')
        .into_iter()
        .filter(|item| !item.trim().is_empty())
        .collect();

    if items.is_empty() {
        return Err(MysqlError::parse("SELECT needs at least one column"));
    }

    // `SELECT COUNT(*)` — the shape an ORM uses to ask "does it exist?".
    if items.len() == 1 {
        let (expr, alias) = split_select_alias(&items[0]);
        if expr.to_ascii_lowercase().replace(' ', "") == "count(*)" {
            return Ok(rows_owned(
                vec![alias.unwrap_or_else(|| "COUNT(*)".to_string())],
                vec![vec![Value::Integer(data.len() as i64)]],
            ));
        }
    }

    let mut headers = Vec::new();
    let mut indexes = Vec::new();
    for item in &items {
        let (expr, alias) = split_select_alias(item);
        let bare = expr.trim();
        if bare == "*" || bare.ends_with(".*") {
            headers.extend(columns.iter().map(|c| c.to_string()));
            indexes.extend(0..columns.len());
            continue;
        }
        if bare.to_ascii_lowercase().replace(' ', "") == "count(*)" {
            return Err(MysqlError::unsupported(
                "COUNT(*) may only be selected on its own in an information_schema query",
            ));
        }
        let index = resolve_column(bare, columns).map_err(|_| {
            MysqlError::bad_field(format!("Unknown column '{bare}' in 'field list'"))
        })?;
        headers.push(alias.unwrap_or_else(|| columns[index].to_string()));
        indexes.push(index);
    }

    let projected = data
        .into_iter()
        .map(|row| indexes.iter().map(|i| row[*i].clone()).collect())
        .collect();
    Ok(rows_owned(headers, projected))
}

fn split_select_alias(item: &str) -> (String, Option<String>) {
    let item = item.trim();
    if let Some(at) = find_keyword(item, "as") {
        return (
            item[..at].trim().to_string(),
            Some(unquote_identifier(item[at + 2..].trim())),
        );
    }
    (item.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlaysql::{Column, Table};

    fn catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                name: "users".to_string(),
                columns: vec![
                    Column::primary_key("id", DataType::Integer),
                    Column::new("email", DataType::Text),
                ],
            })
            .unwrap();
        catalog
            .create_table(Table {
                name: "posts".to_string(),
                columns: vec![Column::new("title", DataType::Text)],
            })
            .unwrap();
        catalog
    }

    fn session() -> Session {
        Session::new(1, "root", Some("app".to_string()))
    }

    fn run(sql: &str, params: &[Value]) -> Intercepted {
        query(
            &crate::sqltext::normalize(sql),
            params,
            &catalog(),
            &session(),
        )
    }

    fn result(sql: &str, params: &[Value]) -> inlaysql::ResultSet {
        match run(sql, params) {
            Intercepted::Rows(rows) => *rows,
            other => panic!("{sql} was not answered with rows: {other:?}"),
        }
    }

    fn texts(rows: &inlaysql::ResultSet, column: &str) -> Vec<String> {
        let at = rows
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(column))
            .unwrap_or_else(|| panic!("no column {column} in {:?}", rows.columns));
        rows.rows.iter().map(|row| render(&row[at])).collect()
    }

    #[test]
    fn tables_lists_the_catalog() {
        let rows = result("SELECT table_name FROM information_schema.tables", &[]);
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["posts", "users"]);
    }

    #[test]
    fn a_star_projection_returns_every_column() {
        let rows = result("SELECT * FROM information_schema.tables", &[]);
        assert_eq!(rows.columns.len(), TABLES_COLUMNS.len());
        assert_eq!(rows.rows.len(), 2);
    }

    #[test]
    fn columns_describes_every_column_of_every_table() {
        let rows = result(
            "SELECT table_name, column_name, data_type, ordinal_position \
             FROM information_schema.columns WHERE table_name = 'users'",
            &[],
        );
        assert_eq!(texts(&rows, "COLUMN_NAME"), vec!["id", "email"]);
        assert_eq!(texts(&rows, "DATA_TYPE"), vec!["bigint", "text"]);
        assert_eq!(texts(&rows, "ORDINAL_POSITION"), vec!["1", "2"]);
    }

    /// The filter that matters most: a bound parameter has to actually filter,
    /// because this is how every ORM asks whether one table exists.
    #[test]
    fn a_bound_parameter_filters() {
        let rows = result(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = ? AND table_name = ?",
            &[Value::Text("app".into()), Value::Text("users".into())],
        );
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["users"]);

        let none = result(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = ? AND table_name = ?",
            &[Value::Text("app".into()), Value::Text("absent".into())],
        );
        assert!(none.rows.is_empty());
    }

    #[test]
    fn database_resolves_to_the_current_schema() {
        let rows = result(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = database()",
            &[],
        );
        assert_eq!(rows.rows.len(), 2);

        let none = result(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'elsewhere'",
            &[],
        );
        assert!(none.rows.is_empty());
    }

    #[test]
    fn count_star_answers_the_existence_probe() {
        let rows = result(
            "SELECT COUNT(*) AS aggregate FROM information_schema.tables \
             WHERE table_name = 'users' AND table_type = 'BASE TABLE'",
            &[],
        );
        assert_eq!(rows.columns, vec!["aggregate"]);
        assert_eq!(rows.rows[0][0], Value::Integer(1));
    }

    #[test]
    fn like_in_and_is_null_all_filter() {
        assert_eq!(
            texts(
                &result(
                    "SELECT table_name FROM information_schema.tables WHERE table_name LIKE 'us%'",
                    &[]
                ),
                "TABLE_NAME"
            ),
            vec!["users"]
        );
        assert_eq!(
            texts(
                &result(
                    "SELECT table_name FROM information_schema.tables \
                     WHERE table_name IN ('users', 'nope')",
                    &[]
                ),
                "TABLE_NAME"
            ),
            vec!["users"]
        );
        // TABLE_ROWS is NULL because no row count is kept.
        assert_eq!(
            result(
                "SELECT table_name FROM information_schema.tables WHERE table_rows IS NULL",
                &[]
            )
            .rows
            .len(),
            2
        );
        assert!(result(
            "SELECT table_name FROM information_schema.tables WHERE table_rows IS NOT NULL",
            &[]
        )
        .rows
        .is_empty());
    }

    #[test]
    fn order_by_and_limit_apply() {
        let rows = result(
            "SELECT table_name FROM information_schema.tables ORDER BY table_name DESC",
            &[],
        );
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["users", "posts"]);

        let limited = result(
            "SELECT table_name FROM information_schema.tables ORDER BY table_name LIMIT 1",
            &[],
        );
        assert_eq!(texts(&limited, "TABLE_NAME"), vec!["posts"]);

        let offset = result(
            "SELECT table_name FROM information_schema.tables ORDER BY table_name LIMIT 1 OFFSET 1",
            &[],
        );
        assert_eq!(texts(&offset, "TABLE_NAME"), vec!["users"]);
    }

    #[test]
    fn comparisons_are_case_insensitive_as_they_are_in_mysql() {
        let rows = result(
            "SELECT table_name FROM information_schema.tables WHERE table_name = 'USERS'",
            &[],
        );
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["users"]);
    }

    // ------------------------------------------------------ the refusals

    /// The central promise of this module. A filter it cannot parse must not
    /// quietly become "no filter", which would answer "yes, that exists" to
    /// every question.
    #[test]
    fn an_unparsable_filter_is_refused_rather_than_ignored() {
        for sql in [
            "SELECT table_name FROM information_schema.tables WHERE table_name > 'a'",
            "SELECT table_name FROM information_schema.tables WHERE LENGTH(table_name) = 5",
            "SELECT table_name FROM information_schema.tables WHERE a = 1 OR b = 2",
        ] {
            match run(sql, &[]) {
                Intercepted::Failed(error) => {
                    assert!(
                        error.code == 1235 || error.code == 1054,
                        "{sql} gave {error:?}"
                    );
                }
                other => panic!("{sql} should have been refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unknown_column_is_1054_in_both_the_field_list_and_the_filter() {
        match run("SELECT wibble FROM information_schema.tables", &[]) {
            Intercepted::Failed(error) => assert_eq!(error.code, 1054),
            other => panic!("{other:?}"),
        }
        match run(
            "SELECT table_name FROM information_schema.tables WHERE wibble = 'x'",
            &[],
        ) {
            Intercepted::Failed(error) => assert_eq!(error.code, 1054),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unimplemented_view_says_which_ones_exist() {
        match run("SELECT * FROM information_schema.routines", &[]) {
            Intercepted::Failed(error) => {
                assert_eq!(error.code, 1235);
                assert!(error.message.contains("TABLES"), "{}", error.message);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_join_against_information_schema_is_refused() {
        match run(
            "SELECT * FROM information_schema.tables t JOIN information_schema.columns c",
            &[],
        ) {
            Intercepted::Failed(error) => assert_eq!(error.code, 1235),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn statistics_reports_the_primary_key() {
        let rows = result(
            "SELECT index_name, column_name FROM information_schema.statistics \
             WHERE table_name = 'users'",
            &[],
        );
        assert_eq!(texts(&rows, "INDEX_NAME"), vec!["PRIMARY"]);
        assert_eq!(texts(&rows, "COLUMN_NAME"), vec!["id"]);
    }

    #[test]
    fn schemata_names_this_database() {
        let rows = result("SELECT schema_name FROM information_schema.schemata", &[]);
        assert_eq!(texts(&rows, "SCHEMA_NAME"), vec!["app"]);
    }
}
