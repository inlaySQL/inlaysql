//! `information_schema` queries, answered from [`Catalog`].
//!
//! An ORM discovers a schema by querying these views, so they have to work; the
//! engine has no schemas and no subqueries, so they cannot be forwarded to it.
//! What is here is a deliberately small evaluator over a handful of fixed
//! relations: a projection, a disjunction of conjunctions of simple
//! comparisons (an OR-of-AND-groups `WHERE`), an optional sort and an optional
//! limit. `VIEWS`, `TRIGGERS` and `ROUTINES` answer zero rows because the
//! engine genuinely has none of those object types; `KEY_COLUMN_USAGE` and
//! `TABLE_CONSTRAINTS` describe the primary keys, `UNIQUE` constraints,
//! foreign keys and `CHECK` constraints the catalog really holds.
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

use inlaysql::{Catalog, DataType, Table, Value};

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

/// `information_schema.VIEWS`.
///
/// Always zero rows, and that is the truth rather than a stub: the engine has
/// no `CREATE VIEW` and cannot be holding one, so there is no view to
/// describe. The column list is MySQL 8's, so a client that probes it learns
/// "there are no views" in a shape it understands.
const VIEWS_COLUMNS: &[&str] = &[
    "TABLE_CATALOG",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "VIEW_DEFINITION",
    "CHECK_OPTION",
    "IS_UPDATABLE",
    "DEFINER",
    "SECURITY_TYPE",
    "CHARACTER_SET_CLIENT",
    "COLLATION_CONNECTION",
];

/// `information_schema.TRIGGERS`, always zero rows — the engine has no
/// triggers, so the empty answer is the correct one, not a placeholder.
const TRIGGERS_COLUMNS: &[&str] = &[
    "TRIGGER_CATALOG",
    "TRIGGER_SCHEMA",
    "TRIGGER_NAME",
    "EVENT_MANIPULATION",
    "EVENT_OBJECT_CATALOG",
    "EVENT_OBJECT_SCHEMA",
    "EVENT_OBJECT_TABLE",
    "ACTION_ORDER",
    "ACTION_CONDITION",
    "ACTION_STATEMENT",
    "ACTION_ORIENTATION",
    "ACTION_TIMING",
    "ACTION_REFERENCE_OLD_TABLE",
    "ACTION_REFERENCE_NEW_TABLE",
    "ACTION_REFERENCE_OLD_ROW",
    "ACTION_REFERENCE_NEW_ROW",
    "CREATED",
    "SQL_MODE",
    "DEFINER",
    "CHARACTER_SET_CLIENT",
    "COLLATION_CONNECTION",
    "DATABASE_COLLATION",
];

/// `information_schema.ROUTINES`, always zero rows — the engine has no stored
/// procedures or functions, so there is no routine to describe.
const ROUTINES_COLUMNS: &[&str] = &[
    "SPECIFIC_NAME",
    "ROUTINE_CATALOG",
    "ROUTINE_SCHEMA",
    "ROUTINE_NAME",
    "ROUTINE_TYPE",
    "DATA_TYPE",
    "CHARACTER_MAXIMUM_LENGTH",
    "CHARACTER_OCTET_LENGTH",
    "NUMERIC_PRECISION",
    "NUMERIC_SCALE",
    "DATETIME_PRECISION",
    "CHARACTER_SET_NAME",
    "COLLATION_NAME",
    "DTD_IDENTIFIER",
    "ROUTINE_BODY",
    "ROUTINE_DEFINITION",
    "EXTERNAL_NAME",
    "EXTERNAL_LANGUAGE",
    "PARAMETER_STYLE",
    "IS_DETERMINISTIC",
    "SQL_DATA_ACCESS",
    "SQL_PATH",
    "SECURITY_TYPE",
    "CREATED",
    "LAST_ALTERED",
    "SQL_MODE",
    "ROUTINE_COMMENT",
    "DEFINER",
    "CHARACTER_SET_CLIENT",
    "COLLATION_CONNECTION",
    "DATABASE_COLLATION",
];

/// `information_schema.KEY_COLUMN_USAGE`, in MySQL's column order.
const KEY_COLUMN_USAGE_COLUMNS: &[&str] = &[
    "CONSTRAINT_CATALOG",
    "CONSTRAINT_SCHEMA",
    "CONSTRAINT_NAME",
    "TABLE_CATALOG",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "COLUMN_NAME",
    "ORDINAL_POSITION",
    "POSITION_IN_UNIQUE_CONSTRAINT",
    "REFERENCED_TABLE_SCHEMA",
    "REFERENCED_TABLE_NAME",
    "REFERENCED_COLUMN_NAME",
];

/// `information_schema.TABLE_CONSTRAINTS`, in MySQL's column order.
///
/// No `TABLE_CATALOG` column: MySQL 8's TABLE_CONSTRAINTS does not have one.
const TABLE_CONSTRAINTS_COLUMNS: &[&str] = &[
    "CONSTRAINT_CATALOG",
    "CONSTRAINT_SCHEMA",
    "CONSTRAINT_NAME",
    "TABLE_SCHEMA",
    "TABLE_NAME",
    "CONSTRAINT_TYPE",
    "ENFORCED",
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
        let where_clause = parse_where(clause, columns, base, session)?;
        data.retain(|row| where_clause.matches(row, params));
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
    Views,
    Triggers,
    Routines,
    KeyColumnUsage,
    TableConstraints,
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
        "views" => Ok(Relation::Views),
        "triggers" => Ok(Relation::Triggers),
        "routines" => Ok(Relation::Routines),
        "key_column_usage" => Ok(Relation::KeyColumnUsage),
        "table_constraints" => Ok(Relation::TableConstraints),
        other => Err(MysqlError::unsupported(format!(
            "information_schema.{other} is not implemented; this server provides \
             TABLES, COLUMNS, SCHEMATA, STATISTICS, VIEWS, TRIGGERS, ROUTINES, \
             KEY_COLUMN_USAGE and TABLE_CONSTRAINTS"
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

        // The engine has no views, no triggers and no stored routines — there
        // is no object of these kinds to describe, so zero rows is the answer,
        // not a placeholder for work that was never done.
        Relation::Views => (VIEWS_COLUMNS, Vec::new()),
        Relation::Triggers => (TRIGGERS_COLUMNS, Vec::new()),
        Relation::Routines => (ROUTINES_COLUMNS, Vec::new()),

        Relation::TableConstraints => {
            let mut data = Vec::new();
            for table in catalog.tables() {
                let mut push = |name: &str, kind: &str, enforced: &str| {
                    data.push(vec![
                        text("def"),
                        text(&schema),
                        text(name),
                        text(&schema),
                        text(&table.name),
                        text(kind),
                        text(enforced),
                    ]);
                };
                // A primary key's constraint name is the literal `PRIMARY` —
                // MySQL never generates one for it.
                if table.rowid_alias().is_some() {
                    push("PRIMARY", "PRIMARY KEY", "YES");
                }
                if let Some(constraints) = catalog.constraints(&table.name) {
                    for (nth, group) in constraints.unique.iter().enumerate() {
                        push(
                            &unique_constraint_display_name(
                                catalog,
                                table,
                                group.name.as_deref(),
                                &group.columns,
                                nth,
                            ),
                            "UNIQUE",
                            "YES",
                        );
                    }
                    // The engine records foreign keys but never enforces them
                    // (README, TESTING.md — SQLite's own long-standing default),
                    // so ENFORCED says so instead of pretending it checks.
                    for (nth, _) in constraints.foreign_keys.iter().enumerate() {
                        push(
                            &format!("{}_ibfk_{}", table.name, nth + 1),
                            "FOREIGN KEY",
                            "NO",
                        );
                    }
                    // `{table}_chk_{n}`, InnoDB's own spelling for an unnamed
                    // CHECK constraint — and every CHECK here is unnamed.
                    for (nth, _) in constraints.checks.iter().enumerate() {
                        push(&format!("{}_chk_{}", table.name, nth + 1), "CHECK", "YES");
                    }
                }
            }
            (TABLE_CONSTRAINTS_COLUMNS, data)
        }

        Relation::KeyColumnUsage => {
            let mut data = Vec::new();
            for table in catalog.tables() {
                let mut push = |name: &str,
                                column: &str,
                                ordinal: i64,
                                position_in_unique: Option<i64>,
                                referenced_schema: Option<&str>,
                                referenced_table: Option<&str>,
                                referenced_column: Option<&str>| {
                    let nullable = |value: Option<&str>| match value {
                        Some(value) => text(value),
                        None => Value::Null,
                    };
                    data.push(vec![
                        text("def"),
                        text(&schema),
                        text(name),
                        text("def"),
                        text(&schema),
                        text(&table.name),
                        text(column),
                        Value::Integer(ordinal),
                        match position_in_unique {
                            Some(position) => Value::Integer(position),
                            None => Value::Null,
                        },
                        nullable(referenced_schema),
                        nullable(referenced_table),
                        nullable(referenced_column),
                    ]);
                };
                // The primary key is the rowid alias, as in STATISTICS; MySQL's
                // name for it is always the literal `PRIMARY`.
                if let Some(position) = table.rowid_alias() {
                    push(
                        "PRIMARY",
                        &table.columns[position].name,
                        1,
                        None,
                        None,
                        None,
                        None,
                    );
                }
                let Some(constraints) = catalog.constraints(&table.name) else {
                    continue;
                };
                for (nth, group) in constraints.unique.iter().enumerate() {
                    let name = unique_constraint_display_name(
                        catalog,
                        table,
                        group.name.as_deref(),
                        &group.columns,
                        nth,
                    );
                    for (ordinal, column) in group.columns.iter().enumerate() {
                        push(&name, column, ordinal as i64 + 1, None, None, None, None);
                    }
                }
                for (nth, foreign) in constraints.foreign_keys.iter().enumerate() {
                    let name = format!("{}_ibfk_{}", table.name, nth + 1);
                    // The unique key the FK points at, in key order, when the
                    // catalog can resolve it: the columns the FK names, or the
                    // referenced table's primary key when it names none. The
                    // engine records FKs without checking their target, so a
                    // dangling one resolves to `None` and the positions it
                    // would have answered are NULL rather than guessed.
                    let key = fk_referenced_key(catalog, &foreign.table, &foreign.referenced);
                    for (ordinal, column) in foreign.columns.iter().enumerate() {
                        // The referenced column: the one the FK names, or the
                        // primary-key column it points at when it names none.
                        let referenced_column = if !foreign.referenced.is_empty() {
                            foreign.referenced.get(ordinal).cloned()
                        } else {
                            key.as_ref().and_then(|key| key.get(ordinal).cloned())
                        };
                        let position_in_unique = referenced_column
                            .as_deref()
                            .zip(key.as_ref())
                            .and_then(|(column, key)| {
                                key.iter()
                                    .position(|c| c.eq_ignore_ascii_case(column))
                                    .map(|position| position as i64 + 1)
                            });
                        push(
                            &name,
                            column,
                            ordinal as i64 + 1,
                            position_in_unique,
                            Some(&schema),
                            Some(&foreign.table),
                            referenced_column.as_deref(),
                        );
                    }
                }
            }
            (KEY_COLUMN_USAGE_COLUMNS, data)
        }
    }
}

/// The name a `UNIQUE` constraint is reported under.
///
/// This must agree with `STATISTICS.INDEX_NAME` for the same key: MySQL
/// guarantees `KEY_COLUMN_USAGE.CONSTRAINT_NAME` equals `STATISTICS.INDEX_NAME`
/// for a `UNIQUE` key, and tools cross-reference the two views. A constraint
/// named by `CREATE UNIQUE INDEX` uses that name; an unnamed one is enforced
/// through the B-tree index the engine declared for it, so that index's name
/// is the display name. When no such index exists — a `UNIQUE` over a `VECTOR`
/// column, which no B-tree can cover, or a hand-built catalog — the name the
/// engine would have generated is reproduced. It is the same scheme
/// `inlaysql_core::catalog::auto_unique_index_name` uses; that function is
/// `pub(crate)`, so the spelling is restated here and must not drift.
fn unique_constraint_display_name(
    catalog: &Catalog,
    table: &Table,
    declared: Option<&str>,
    columns: &[String],
    nth: usize,
) -> String {
    if let Some(name) = declared {
        return name.to_string();
    }
    let collations: Vec<inlaysql::Collation> = columns
        .iter()
        .map(|column| {
            table
                .column(column)
                .map_or(inlaysql::Collation::Binary, |(_, definition)| {
                    definition.collation
                })
        })
        .collect();
    if let Some(index) = catalog.btree_index_on(&table.name, columns, &collations) {
        if index.unique {
            return index.name.clone();
        }
    }
    format!(
        "__inlaysql_uniq_{}_{}_{}",
        table.name.to_ascii_lowercase(),
        columns
            .iter()
            .map(|column| column.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join("_"),
        nth
    )
}

/// The columns of the key a foreign key references, in key order, when the
/// catalog can resolve them.
///
/// The engine records a foreign key without checking its target, and it does
/// not record *which* unique key was referenced — only which columns, or none
/// to mean the referenced table's primary key. Both are resolved here against
/// the catalog as it stands now; `None` means the target cannot be found (a
/// dangling foreign key, or one naming columns no unique key covers), and the
/// caller answers `NULL` for the positions it would have reported.
fn fk_referenced_key(catalog: &Catalog, table: &str, referenced: &[String]) -> Option<Vec<String>> {
    let target = catalog.table(table)?;
    let primary_key = || {
        target
            .rowid_alias()
            .map(|position| vec![target.columns[position].name.clone()])
    };
    if referenced.is_empty() {
        return primary_key();
    }
    if primary_key().is_some_and(|columns| {
        columns.len() == referenced.len()
            && columns
                .iter()
                .zip(referenced)
                .all(|(column, named)| column.eq_ignore_ascii_case(named))
    }) {
        return primary_key();
    }
    catalog.constraints(table).and_then(|constraints| {
        constraints.unique.iter().find_map(|group| {
            (group.columns.len() == referenced.len()
                && referenced.iter().all(|named| {
                    group
                        .columns
                        .iter()
                        .any(|column| column.eq_ignore_ascii_case(named))
                }))
            .then(|| group.columns.clone())
        })
    })
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

/// A parsed `WHERE` clause: a disjunction of conjunctions, which is the shape
/// the evaluator can check without becoming a planner.
#[derive(Debug, Clone)]
struct Disjunction {
    /// One OR-arm; each arm is a list of predicates that must all hold.
    /// A single empty arm (an empty WHERE clause) matches every row.
    arms: Vec<Vec<Predicate>>,
}

impl Disjunction {
    fn matches(&self, row: &[Value], params: &[Value]) -> bool {
        self.arms
            .iter()
            .any(|arm| arm.iter().all(|predicate| predicate.matches(row, params)))
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

/// Parse a WHERE clause into an OR-of-AND-groups.
///
/// Standard SQL precedence: AND binds tighter than OR, so the clause is split
/// on top-level OR first, then each side on AND. Enclosing parentheses around
/// a group are understood at every level (`a = 1 OR (b = 2 AND c = 3)`, and
/// the whole clause wrapped as `(a = 1 OR b = 2)` the way Django wraps an OR
/// of Q-objects). Anything more nested — an OR inside an AND-group, whatever
/// the parentheses — is refused with a message naming it rather than
/// flattened on a guess.
fn parse_where(
    clause: &str,
    columns: &[&str],
    base_placeholder: usize,
    session: &Session,
) -> Result<Disjunction, MysqlError> {
    let mut placeholder = base_placeholder;
    let mut arms = Vec::new();

    for or_part in split_on_keyword(strip_outer_parens_repeated(clause), "or") {
        let or_part = strip_outer_parens_repeated(or_part.trim());
        if or_part.is_empty() {
            continue;
        }
        let mut arm = Vec::new();
        for conjunct in split_on_keyword(or_part, "and") {
            let conjunct = strip_outer_parens_repeated(conjunct.trim());
            if conjunct.is_empty() {
                continue;
            }
            if find_keyword(conjunct, "and").is_some() || find_keyword(conjunct, "or").is_some() {
                return Err(MysqlError::unsupported(format!(
                    "`{conjunct}` nests an AND/OR group inside another; this server \
                     evaluates an information_schema WHERE clause as OR-of-AND-groups, \
                     and it will not guess at what the nesting means"
                )));
            }
            arm.push(parse_predicate(
                conjunct,
                columns,
                &mut placeholder,
                session,
            )?);
        }
        if !arm.is_empty() {
            arms.push(arm);
        }
    }
    if arms.is_empty() {
        arms.push(Vec::new());
    }
    Ok(Disjunction { arms })
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
             `col LIKE x`, `col IN (...)` and `col IS [NOT] NULL`, \
             combined with AND and OR as OR-of-AND-groups"
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

/// [`strip_outer_parens`], repeated until nothing more comes off — needed
/// because a whole `WHERE` clause can arrive multiply wrapped: Django emits
/// `WHERE (a = 1 OR b = 2)` for an OR of `Q` objects (one layer), and a
/// double-wrapped conjunct like `((a = 1))` should not be refused as an
/// unresolvable column merely because [`parse_where`] only peeled one layer
/// off before trying to parse what was left.
fn strip_outer_parens_repeated(text: &str) -> &str {
    let mut text = text;
    loop {
        let stripped = strip_outer_parens(text);
        if stripped.len() == text.len() {
            return stripped;
        }
        text = stripped;
    }
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
    let mut items_out: Vec<ProjectedItem> = Vec::new();
    for item in &items {
        let (expr, alias) = split_select_alias(item);
        let bare = expr.trim();
        if bare == "*" || bare.ends_with(".*") {
            headers.extend(columns.iter().map(|c| c.to_string()));
            items_out.extend((0..columns.len()).map(ProjectedItem::Column));
            continue;
        }
        if bare.to_ascii_lowercase().replace(' ', "") == "count(*)" {
            return Err(MysqlError::unsupported(
                "COUNT(*) may only be selected on its own in an information_schema query",
            ));
        }
        // `SELECT 1 FROM information_schema.tables WHERE ...` — the other
        // shape an existence check compiles to, alongside `EXISTS (...)`
        // above: a constant, projected once per matching row, not a column
        // reference. Every row gets the same value regardless of `columns`.
        if let Some(value) = literal_value(bare) {
            headers.push(alias.unwrap_or_else(|| bare.to_string()));
            items_out.push(ProjectedItem::Literal(value));
            continue;
        }
        let index = resolve_column(bare, columns).map_err(|_| {
            MysqlError::bad_field(format!("Unknown column '{bare}' in 'field list'"))
        })?;
        headers.push(alias.unwrap_or_else(|| columns[index].to_string()));
        items_out.push(ProjectedItem::Column(index));
    }

    let projected = data
        .into_iter()
        .map(|row| {
            items_out
                .iter()
                .map(|item| match item {
                    ProjectedItem::Column(i) => row[*i].clone(),
                    ProjectedItem::Literal(v) => v.clone(),
                })
                .collect()
        })
        .collect();
    Ok(rows_owned(headers, projected))
}

/// One item of a resolved projection list: a real column, or a constant
/// repeated for every row (see the `SELECT 1 FROM ...` case in [`project`]).
enum ProjectedItem {
    Column(usize),
    Literal(Value),
}

/// A bare literal in a projection: an integer, a quoted string, or `NULL`.
/// Anything else (an expression, a function call, `?`) is not recognised
/// here — `resolve_column` gets the next attempt, and its own error names
/// the field the way MySQL's does.
fn literal_value(text: &str) -> Option<Value> {
    if text.eq_ignore_ascii_case("null") {
        return Some(Value::Null);
    }
    if let Some(text) = unquote_string(text) {
        return Some(Value::Text(text));
    }
    if let Ok(n) = text.parse::<i64>() {
        return Some(Value::Integer(n));
    }
    if let Ok(n) = text.parse::<f64>() {
        return Some(Value::Real(n));
    }
    None
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
    use inlaysql::{Column, Database, Table};

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

    /// A catalog whose tables declare the constraints the constraint views
    /// exist to describe, built by running real DDL through the engine so the
    /// declarations — including the index names the engine generates for
    /// unnamed `UNIQUE` constraints — arrive exactly as a live database holds
    /// them.
    fn constrained_catalog() -> Catalog {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .unwrap();
        db.execute(
            "CREATE TABLE books (\
               id INTEGER PRIMARY KEY, \
               author_id INTEGER REFERENCES authors(id) ON DELETE CASCADE, \
               title TEXT UNIQUE, \
               edition INTEGER, \
               UNIQUE (author_id, edition), \
               CHECK (edition > 0))",
            &[],
        )
        .unwrap();
        // A foreign key that names no referenced column means "that table's
        // primary key", which is the other spelling the view must resolve.
        db.execute("CREATE TABLE publishers (id INTEGER PRIMARY KEY)", &[])
            .unwrap();
        db.execute(
            "CREATE TABLE series (\
               id INTEGER PRIMARY KEY, \
               publisher_id INTEGER REFERENCES publishers)",
            &[],
        )
        .unwrap();
        db.execute("CREATE UNIQUE INDEX ux_authors_name ON authors (name)", &[])
            .unwrap();
        db.catalog().clone()
    }

    fn session() -> Session {
        Session::new(1, "root", Some("app".to_string()))
    }

    fn run(sql: &str, params: &[Value]) -> Intercepted {
        run_on(&catalog(), sql, params)
    }

    fn run_on(catalog: &Catalog, sql: &str, params: &[Value]) -> Intercepted {
        query(&crate::sqltext::normalize(sql), params, catalog, &session())
    }

    fn result(sql: &str, params: &[Value]) -> inlaysql::ResultSet {
        match run(sql, params) {
            Intercepted::Rows(rows) => *rows,
            other => panic!("{sql} was not answered with rows: {other:?}"),
        }
    }

    fn result_on(catalog: &Catalog, sql: &str) -> inlaysql::ResultSet {
        match run_on(catalog, sql, &[]) {
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
            // OR is supported at the top level, but not nested inside an
            // AND-group; that shape is refused rather than flattened.
            "SELECT table_name FROM information_schema.tables \
             WHERE (table_name = 'users' OR table_name = 'posts') AND table_schema = 'app'",
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
        match run(
            "SELECT * FROM information_schema.referential_constraints",
            &[],
        ) {
            Intercepted::Failed(error) => {
                assert_eq!(error.code, 1235);
                assert!(
                    error.message.contains("KEY_COLUMN_USAGE"),
                    "{}",
                    error.message
                );
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

    /// `SELECT 1 FROM information_schema.tables WHERE ...` — the existence
    /// idiom some ORMs use directly, without wrapping it in `EXISTS`. `1` is
    /// a constant, projected once per matching row, not a column reference;
    /// before this was recognised it fell into `resolve_column` and refused
    /// with error 1054 ("Unknown column '1'"). One row present, one absent,
    /// and a literal alongside a real column, so a fix that only handled the
    /// single-item case would still be caught.
    #[test]
    fn a_bare_literal_projects_once_per_matching_row() {
        let rows = result(
            "SELECT 1 FROM information_schema.tables WHERE table_name = 'users'",
            &[],
        );
        assert_eq!(texts(&rows, "1"), vec!["1"]);

        let rows = result(
            "SELECT 1 FROM information_schema.tables WHERE table_name = 'nope'",
            &[],
        );
        assert!(rows.rows.is_empty());

        let rows = result(
            "SELECT table_name, 1, 'x' AS tag FROM information_schema.tables \
             WHERE table_name = 'posts'",
            &[],
        );
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["posts"]);
        assert_eq!(texts(&rows, "1"), vec!["1"]);
        assert_eq!(texts(&rows, "tag"), vec!["x"]);
    }

    // -------------------------------------------------------------- WHERE OR

    #[test]
    fn or_in_where_combines_groups() {
        // AND would match nothing here: no table has both names.
        let rows = result(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_name = 'users' OR table_name = 'posts'",
            &[],
        );
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["posts", "users"]);

        // AND binds tighter than OR, and a parenthesised AND-group is
        // understood on either side.
        let rows = result(
            "SELECT table_name FROM information_schema.tables \
             WHERE (table_name = 'users' AND table_schema = 'app') OR table_name = 'posts'",
            &[],
        );
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["posts", "users"]);

        // An arm whose conjunction cannot hold simply matches nothing.
        let rows = result(
            "SELECT table_name FROM information_schema.tables \
             WHERE (table_name = 'users' AND table_name = 'posts') OR table_name = 'posts'",
            &[],
        );
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["posts"]);

        // Bound parameters flow through an OR the way they do through an AND.
        let rows = result(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_name = ? OR table_name = ?",
            &[Value::Text("users".into()), Value::Text("absent".into())],
        );
        assert_eq!(texts(&rows, "TABLE_NAME"), vec!["users"]);
    }

    // ------------------------------------------- the empty object relations

    #[test]
    fn views_triggers_and_routines_answer_zero_rows_with_mysql_columns() {
        for (view, columns) in [
            ("views", VIEWS_COLUMNS),
            ("triggers", TRIGGERS_COLUMNS),
            ("routines", ROUTINES_COLUMNS),
        ] {
            let rows = result(&format!("SELECT * FROM information_schema.{view}"), &[]);
            let expected: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
            assert_eq!(rows.columns, expected, "{view}");
            assert!(rows.rows.is_empty(), "{view} must have no rows");
        }
    }

    // --------------------------------------------------- TABLE_CONSTRAINTS

    #[test]
    fn table_constraints_names_every_constraint_kind() {
        let catalog = constrained_catalog();
        let rows = result_on(
            &catalog,
            "SELECT constraint_name, constraint_type, enforced \
             FROM information_schema.table_constraints \
             WHERE table_name = 'books' ORDER BY constraint_name",
        );
        assert_eq!(
            texts(&rows, "CONSTRAINT_NAME"),
            vec![
                "__inlaysql_uniq_books_author_id_edition_1",
                "__inlaysql_uniq_books_title_0",
                "books_chk_1",
                "books_ibfk_1",
                "PRIMARY",
            ]
        );
        assert_eq!(
            texts(&rows, "CONSTRAINT_TYPE"),
            vec!["UNIQUE", "UNIQUE", "CHECK", "FOREIGN KEY", "PRIMARY KEY"]
        );
        // The engine records foreign keys but never enforces them (README,
        // TESTING.md); every other constraint kind really is checked.
        assert_eq!(
            texts(&rows, "ENFORCED"),
            vec!["YES", "YES", "YES", "NO", "YES"]
        );

        // A `CREATE UNIQUE INDEX` keeps its own name as the constraint name.
        let rows = result_on(
            &catalog,
            "SELECT constraint_name, constraint_type FROM information_schema.table_constraints \
             WHERE table_name = 'authors' ORDER BY constraint_name",
        );
        assert_eq!(
            texts(&rows, "CONSTRAINT_NAME"),
            vec!["PRIMARY", "ux_authors_name"]
        );
        assert_eq!(
            texts(&rows, "CONSTRAINT_TYPE"),
            vec!["PRIMARY KEY", "UNIQUE"]
        );
    }

    // --------------------------------------------------- KEY_COLUMN_USAGE

    #[test]
    fn key_column_usage_describes_primary_unique_and_foreign_keys() {
        let catalog = constrained_catalog();
        let rows = result_on(
            &catalog,
            "SELECT constraint_name, column_name, ordinal_position, \
                    position_in_unique_constraint, referenced_table_name, \
                    referenced_column_name \
             FROM information_schema.key_column_usage \
             WHERE table_name = 'books' ORDER BY constraint_name, ordinal_position",
        );
        assert_eq!(
            texts(&rows, "CONSTRAINT_NAME"),
            vec![
                "__inlaysql_uniq_books_author_id_edition_1",
                "__inlaysql_uniq_books_author_id_edition_1",
                "__inlaysql_uniq_books_title_0",
                "books_ibfk_1",
                "PRIMARY",
            ]
        );
        assert_eq!(
            texts(&rows, "COLUMN_NAME"),
            vec!["author_id", "edition", "title", "author_id", "id"]
        );
        assert_eq!(
            texts(&rows, "ORDINAL_POSITION"),
            vec!["1", "2", "1", "1", "1"]
        );
        // POSITION_IN_UNIQUE_CONSTRAINT is NULL for every row but the
        // foreign-key one, where it is the referenced column's ordinal in the
        // referenced key.
        assert_eq!(
            texts(&rows, "POSITION_IN_UNIQUE_CONSTRAINT"),
            vec!["", "", "", "1", ""]
        );
        assert_eq!(
            texts(&rows, "REFERENCED_TABLE_NAME"),
            vec!["", "", "", "authors", ""]
        );
        assert_eq!(
            texts(&rows, "REFERENCED_COLUMN_NAME"),
            vec!["", "", "", "id", ""]
        );
    }

    /// A foreign key that names no referenced column means "that table's
    /// primary key", and the view has to resolve the actual column name.
    #[test]
    fn a_foreign_key_with_no_referenced_columns_resolves_the_primary_key() {
        let catalog = constrained_catalog();
        let rows = result_on(
            &catalog,
            "SELECT referenced_table_name, referenced_column_name, \
                    position_in_unique_constraint \
             FROM information_schema.key_column_usage \
             WHERE table_name = 'series' AND constraint_name = 'series_ibfk_1'",
        );
        assert_eq!(texts(&rows, "REFERENCED_TABLE_NAME"), vec!["publishers"]);
        assert_eq!(texts(&rows, "REFERENCED_COLUMN_NAME"), vec!["id"]);
        assert_eq!(texts(&rows, "POSITION_IN_UNIQUE_CONSTRAINT"), vec!["1"]);
    }

    /// MySQL guarantees `KEY_COLUMN_USAGE.CONSTRAINT_NAME` for a `UNIQUE` key
    /// equals `STATISTICS.INDEX_NAME` for the same key, and a tool
    /// cross-references the two views. That agreement has to hold here for
    /// named and unnamed constraints alike.
    #[test]
    fn unique_constraint_names_agree_with_statistics_index_names() {
        let catalog = constrained_catalog();
        // REFERENCED_TABLE_NAME is projected too so the foreign-key rows can
        // be told apart from the unique-key rows: the agreement this test
        // asserts only applies to UNIQUE keys.
        let usage = result_on(
            &catalog,
            "SELECT table_name, constraint_name, column_name, referenced_table_name \
             FROM information_schema.key_column_usage",
        );
        let statistics = result_on(
            &catalog,
            "SELECT table_name, index_name, column_name \
             FROM information_schema.statistics",
        );
        let mut from_usage: Vec<Vec<String>> = usage
            .rows
            .iter()
            .filter(|row| row[3] == Value::Null)
            .map(|row| vec![render(&row[0]), render(&row[1]), render(&row[2])])
            .filter(|row| row[1] != "PRIMARY")
            .collect();
        let mut from_statistics: Vec<Vec<String>> = statistics
            .rows
            .iter()
            .map(|row| vec![render(&row[0]), render(&row[1]), render(&row[2])])
            .filter(|row| row[1] != "PRIMARY")
            .collect();
        from_usage.sort();
        from_statistics.sort();
        assert_eq!(from_usage, from_statistics);
    }
}
