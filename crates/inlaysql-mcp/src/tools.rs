//! The tools an agent sees, and the limits it cannot talk its way past.

use inlaysql::{DataType, Database, Outcome, ResultSet, Value};
use serde_json::{json, Value as Json};

/// Caps on what a single tool call may return.
///
/// The client is a language model with a finite context and the database may
/// hold millions of rows, so "return everything the query matched" is not a
/// safe default. Both limits are applied: the row cap keeps a result
/// countable, the byte cap keeps one very wide row from defeating it.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Most rows any tool will return.
    pub max_rows: usize,
    /// Most bytes of serialised JSON any tool will return.
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rows: 200,
            max_bytes: 64 * 1024,
        }
    }
}

/// Why a tool call did not produce a result.
#[derive(Debug)]
pub enum ToolError {
    /// No tool by that name is available on this server.
    Unknown(String),
    /// A required argument was missing or the wrong type.
    Argument(String),
    /// The connection is read-only and the statement is not a read.
    ReadOnly(String),
    /// The database rejected the statement.
    Database(inlaysql::Error),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::Unknown(name) => write!(f, "no such tool: `{name}`"),
            ToolError::Argument(message) => write!(f, "invalid arguments: {message}"),
            ToolError::ReadOnly(message) => write!(
                f,
                "this database is open read-only: {message}. \
                 Start the server with --allow-writes to permit writes."
            ),
            ToolError::Database(error) => write!(f, "{error}"),
        }
    }
}

impl From<inlaysql::Error> for ToolError {
    fn from(error: inlaysql::Error) -> Self {
        ToolError::Database(error)
    }
}

type ToolResult = Result<String, ToolError>;

/// The tool list, as `tools/list` reports it.
///
/// `execute` is absent — not merely refused — without `--allow-writes`. A model
/// cannot be tempted by a tool it was never shown, and a client that lists
/// tools once at startup gets an accurate picture.
pub fn descriptors(allow_writes: bool) -> Vec<Json> {
    let mut tools = vec![
        json!({
            "name": "schema",
            "description":
                "List the tables in this database with their columns and types. \
                 Call this before writing SQL: the dialect is SQLite's, plus a \
                 VECTOR(n) column type.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "query",
            "description":
                "Run a read-only SQL statement and return the rows. Writes are \
                 refused here even when the server allows them; use `execute`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The SELECT statement to run." },
                    "params": {
                        "type": "array",
                        "description":
                            "Values bound to `?` placeholders, in order. Numbers, \
                             strings and null; an array of numbers is a vector.",
                        "items": {},
                    },
                },
                "required": ["sql"],
            },
        }),
        json!({
            "name": "hybrid_search",
            "description":
                "Rank rows by BM25 relevance over a text column, optionally fused \
                 with vector similarity over an embedding column. One ranking, \
                 computed in the engine.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "table": { "type": "string" },
                    "text_column": { "type": "string" },
                    "query": { "type": "string", "description": "The search terms." },
                    "vector_column": {
                        "type": "string",
                        "description": "A VECTOR column to fuse in. Omit for text-only search.",
                    },
                    "embedding": {
                        "type": "array",
                        "items": { "type": "number" },
                        "description": "The query embedding. Required with vector_column.",
                    },
                    "limit": { "type": "integer" },
                },
                "required": ["table", "text_column", "query"],
            },
        }),
        json!({
            "name": "changes",
            "description":
                "Committed row changes after a version, in commit order. Each says \
                 which row in which table was inserted, updated or deleted — read \
                 the row itself with `query` for its current contents. Check \
                 `lost` in the reply: true means the log moved past your position \
                 and you must resynchronise with a full read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": {
                        "type": "integer",
                        "description": "0 for the whole retained log, or the `version` from your last call.",
                    },
                },
            },
        }),
    ];

    if allow_writes {
        tools.push(json!({
            "name": "execute",
            "description":
                "Run a statement that changes the database (INSERT, UPDATE, DELETE, \
                 CREATE TABLE, CREATE INDEX, DROP INDEX) and return how many rows it wrote.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string" },
                    "params": { "type": "array", "items": {} },
                },
                "required": ["sql"],
            },
        }));
    }
    tools
}

/// Run one tool call.
pub fn call(
    db: &mut Database,
    name: &str,
    arguments: &Json,
    allow_writes: bool,
    limits: &Limits,
) -> ToolResult {
    match name {
        "schema" => schema(db),
        "query" => query(db, arguments, limits),
        "hybrid_search" => hybrid_search(db, arguments, limits),
        "changes" => changes(db, arguments, limits),
        "execute" if allow_writes => execute(db, arguments),
        // Naming a tool that was never advertised is a read-only refusal, not
        // an unknown tool: saying "no such tool" would be misleading.
        "execute" => Err(ToolError::ReadOnly(
            "`execute` is not available".to_string(),
        )),
        other => Err(ToolError::Unknown(other.to_string())),
    }
}

fn schema(db: &Database) -> ToolResult {
    let tables: Vec<Json> = db
        .catalog()
        .tables()
        .map(|table| {
            json!({
                "table": table.name,
                "columns": table.columns.iter().map(|column| json!({
                    "name": column.name,
                    "type": type_name(&column.ty),
                    "primary_key": column.primary_key,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "tables": tables }).to_string())
}

fn type_name(ty: &DataType) -> String {
    match ty {
        DataType::Integer => "INTEGER".to_string(),
        DataType::Real => "REAL".to_string(),
        DataType::Text => "TEXT".to_string(),
        DataType::Blob => "BLOB".to_string(),
        DataType::Numeric => "NUMERIC".to_string(),
        DataType::Vector(dim) => format!("VECTOR({dim})"),
        DataType::QuantizedVector(dim) => format!("VECTOR({dim}, INT8)"),
    }
}

fn query(db: &mut Database, arguments: &Json, limits: &Limits) -> ToolResult {
    let sql = required_str(arguments, "sql")?;
    let params = bind_params(arguments)?;

    // Planned, not pattern-matched on the text. A statement is a read because
    // its plan is a read.
    if !db.is_read_only(&sql, &params)? {
        return Err(ToolError::ReadOnly(format!(
            "`{}` is not a read",
            first_words(&sql)
        )));
    }

    let rows = db.query(&sql, &params)?;
    Ok(render_rows(&rows, limits))
}

fn execute(db: &mut Database, arguments: &Json) -> ToolResult {
    let sql = required_str(arguments, "sql")?;
    let params = bind_params(arguments)?;
    let outcome = db.execute(&sql, &params)?;
    Ok(match outcome {
        Outcome::Ddl => json!({ "ok": true, "kind": "ddl" }).to_string(),
        Outcome::Written(count) => json!({ "ok": true, "rows_written": count }).to_string(),
        Outcome::Rows(rows) => json!({
            "ok": true,
            "note": "this statement returned rows; `query` is the tool for that",
            "row_count": rows.rows.len(),
        })
        .to_string(),
    })
}

fn hybrid_search(db: &mut Database, arguments: &Json, limits: &Limits) -> ToolResult {
    let table = required_str(arguments, "table")?;
    let text_column = required_str(arguments, "text_column")?;
    let terms = required_str(arguments, "query")?;
    let limit = arguments["limit"]
        .as_u64()
        .map(|limit| limit as usize)
        .unwrap_or(10)
        .min(limits.max_rows);

    // Identifiers are interpolated, so they have to be identifiers. Rejecting
    // anything else is what keeps a table name from carrying SQL with it.
    check_identifier(&table)?;
    check_identifier(&text_column)?;

    let vector_column = arguments["vector_column"].as_str().map(str::to_string);
    let embedding = arguments["embedding"].as_array().map(|values| {
        values
            .iter()
            .filter_map(|value| value.as_f64().map(|value| value as f32))
            .collect::<Vec<f32>>()
    });

    let (sql, params) = match (vector_column, embedding) {
        (Some(column), Some(embedding)) => {
            check_identifier(&column)?;
            (
                format!(
                    "SELECT *, fuse(vector_score({column}, ?), bm25_score({text_column}, ?)) \
                     AS score FROM {table} ORDER BY score DESC LIMIT {limit}"
                ),
                vec![Value::Vector(embedding), Value::Text(terms)],
            )
        }
        (Some(_), None) => {
            return Err(ToolError::Argument(
                "vector_column was given without an embedding".to_string(),
            ))
        }
        (None, _) => (
            format!(
                "SELECT *, bm25_score({text_column}, ?) AS score \
                 FROM {table} ORDER BY score DESC LIMIT {limit}"
            ),
            vec![Value::Text(terms)],
        ),
    };

    let rows = db.query(&sql, &params)?;
    Ok(render_rows(&rows, limits))
}

fn changes(db: &mut Database, arguments: &Json, limits: &Limits) -> ToolResult {
    let from = arguments["from"].as_u64().unwrap_or(0);
    let changes = db.changes(from)?;
    let lost = changes.lost(from);

    let listed: Vec<Json> = changes
        .changes
        .iter()
        .take(limits.max_rows)
        .map(|change| {
            json!({
                "version": change.version,
                "table": change.table,
                "id": change.id,
                "kind": change.kind.as_str(),
            })
        })
        .collect();

    Ok(json!({
        "changes": listed,
        "version": changes.version,
        "truncated": changes.changes.len() > listed.len(),
        "lost": lost,
        "note": if lost {
            "Changes were dropped before you read them. Resynchronise with a full read; \
             the log cannot tell you what you missed."
        } else {
            "Pass `version` back as `from` on your next call."
        },
    })
    .to_string())
}

// ------------------------------------------------------------------- helpers

fn required_str(arguments: &Json, key: &str) -> Result<String, ToolError> {
    arguments[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| ToolError::Argument(format!("`{key}` is required and must be a string")))
}

/// Map JSON bind parameters onto engine values.
///
/// An array of numbers becomes a vector, which is what makes an embedding
/// expressible in JSON without a second encoding.
fn bind_params(arguments: &Json) -> Result<Vec<Value>, ToolError> {
    let Some(params) = arguments.get("params") else {
        return Ok(Vec::new());
    };
    if params.is_null() {
        return Ok(Vec::new());
    }
    let Some(params) = params.as_array() else {
        return Err(ToolError::Argument("`params` must be an array".to_string()));
    };

    params
        .iter()
        .map(|param| match param {
            Json::Null => Ok(Value::Null),
            Json::Bool(flag) => Ok(Value::Integer(i64::from(*flag))),
            Json::String(text) => Ok(Value::Text(text.clone())),
            Json::Number(number) => match number.as_i64() {
                Some(integer) => Ok(Value::Integer(integer)),
                None => number
                    .as_f64()
                    .map(Value::Real)
                    .ok_or_else(|| ToolError::Argument(format!("{number} is not a number"))),
            },
            Json::Array(values) => {
                let mut embedding = Vec::with_capacity(values.len());
                for value in values {
                    let Some(component) = value.as_f64() else {
                        return Err(ToolError::Argument(
                            "an array parameter is a vector and must hold only numbers".to_string(),
                        ));
                    };
                    embedding.push(component as f32);
                }
                Ok(Value::Vector(embedding))
            }
            Json::Object(_) => Err(ToolError::Argument(
                "an object is not a bind parameter".to_string(),
            )),
        })
        .collect()
}

/// Identifiers are interpolated into SQL, so they must be plain identifiers.
fn check_identifier(name: &str) -> Result<(), ToolError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err(ToolError::Argument(format!(
            "`{name}` is not a valid identifier"
        )))
    }
}

/// The first few words of a statement, for an error message that does not echo
/// an arbitrarily long query back at the model.
fn first_words(sql: &str) -> String {
    let head: Vec<&str> = sql.split_whitespace().take(3).collect();
    head.join(" ")
}

/// Render a result set, applying both limits and saying so when either bit.
fn render_rows(rows: &ResultSet, limits: &Limits) -> String {
    let mut kept: Vec<Json> = Vec::new();
    let mut bytes = 0;
    let mut truncated = rows.rows.len() > limits.max_rows;

    for row in rows.rows.iter().take(limits.max_rows) {
        let rendered: Vec<Json> = row.iter().map(render_value).collect();
        let row = Json::Array(rendered);
        bytes += row.to_string().len();
        if bytes > limits.max_bytes {
            truncated = true;
            break;
        }
        kept.push(row);
    }

    json!({
        "columns": rows.columns,
        "rows": kept,
        "row_count": rows.rows.len(),
        "truncated": truncated,
    })
    .to_string()
}

/// Render one value for a model to read.
fn render_value(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Integer(integer) => json!(integer),
        Value::Real(real) => json!(real),
        Value::Text(text) => json!(text),
        Value::Blob(bytes) => json!(format!("<{} bytes>", bytes.len())),
        // An embedding is hundreds of floats that mean nothing to a reader and
        // would swamp the response. The dimension is the useful part.
        Value::Vector(embedding) => json!(format!("<vector({})>", embedding.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_checked_before_interpolation() {
        assert!(check_identifier("docs").is_ok());
        assert!(check_identifier("body_2").is_ok());
        assert!(check_identifier("docs; DROP TABLE docs").is_err());
        assert!(check_identifier("docs\"").is_err());
        assert!(check_identifier("").is_err());
        assert!(check_identifier("2fast").is_err());
        assert!(check_identifier(&"x".repeat(65)).is_err());
    }

    #[test]
    fn an_array_parameter_becomes_a_vector() {
        let params = bind_params(&json!({ "params": [[1.0, 2.0, 3.0]] })).unwrap();
        assert_eq!(params, vec![Value::Vector(vec![1.0, 2.0, 3.0])]);
    }

    #[test]
    fn mixed_arrays_are_refused_rather_than_silently_dropped() {
        assert!(bind_params(&json!({ "params": [[1.0, "two"]] })).is_err());
    }

    #[test]
    fn absent_params_are_not_an_error() {
        assert!(bind_params(&json!({ "sql": "SELECT 1" }))
            .unwrap()
            .is_empty());
        assert!(bind_params(&json!({ "params": null })).unwrap().is_empty());
    }

    #[test]
    fn scalar_parameters_keep_their_types() {
        let params = bind_params(&json!({ "params": [1, 1.5, "text", null, true] })).unwrap();
        assert_eq!(
            params,
            vec![
                Value::Integer(1),
                Value::Real(1.5),
                Value::Text("text".into()),
                Value::Null,
                Value::Integer(1),
            ]
        );
    }

    #[test]
    fn execute_is_hidden_until_writes_are_allowed() {
        let names = |allow| {
            descriptors(allow)
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(!names(false).contains(&"execute".to_string()));
        assert!(names(true).contains(&"execute".to_string()));
    }

    #[test]
    fn a_vector_is_summarised_rather_than_dumped() {
        let rendered = render_value(&Value::Vector(vec![0.0; 384]));
        assert_eq!(rendered, json!("<vector(384)>"));
    }

    #[test]
    fn the_row_cap_is_reported_not_hidden() {
        let rows = ResultSet {
            columns: vec!["a".to_string()],
            rows: (0..10).map(|i| vec![Value::Integer(i)]).collect(),
        };
        let limits = Limits {
            max_rows: 3,
            max_bytes: 64 * 1024,
        };
        let rendered: Json = serde_json::from_str(&render_rows(&rows, &limits)).unwrap();
        assert_eq!(rendered["rows"].as_array().unwrap().len(), 3);
        assert_eq!(rendered["row_count"], json!(10));
        assert_eq!(rendered["truncated"], json!(true));
    }

    #[test]
    fn the_byte_cap_catches_what_the_row_cap_does_not() {
        let rows = ResultSet {
            columns: vec!["a".to_string()],
            rows: (0..10)
                .map(|_| vec![Value::Text("x".repeat(1000))])
                .collect(),
        };
        let limits = Limits {
            max_rows: 100,
            max_bytes: 2500,
        };
        let rendered: Json = serde_json::from_str(&render_rows(&rows, &limits)).unwrap();
        assert!(rendered["rows"].as_array().unwrap().len() < 10);
        assert_eq!(rendered["truncated"], json!(true));
    }
}
