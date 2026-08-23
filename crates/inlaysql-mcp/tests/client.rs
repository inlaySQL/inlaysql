//! A scripted MCP client: the handshake, the tool list, a hybrid search and a
//! change event, over the real protocol.
//!
//! This is Stage 5's acceptance criterion, written as a test rather than a
//! shell script so it runs on every `cargo test` and fails with a diff rather
//! than a non-zero exit code.

use inlaysql::{Database, Value};
use inlaysql_mcp::{Limits, Server};
use serde_json::{json, Value as Json};

/// A client that talks to a server through the same line protocol a real one
/// would, so nothing here can accidentally bypass the transport.
struct Client {
    server: Server,
    next_id: i64,
}

impl Client {
    fn new(allow_writes: bool) -> Self {
        let mut db = Database::open_in_memory().expect("open");
        db.execute(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(3))",
            &[],
        )
        .unwrap();
        db.execute("CREATE INDEX notes_body ON notes (body)", &[])
            .unwrap();
        db.execute("CREATE INDEX notes_embedding ON notes (embedding)", &[])
            .unwrap();
        for (id, body, embedding) in [
            (
                1i64,
                "embedded databases keep the engine in your process",
                [1.0f32, 0.0, 0.0],
            ),
            (
                2,
                "vector search finds semantically similar text",
                [0.0, 1.0, 0.0],
            ),
            (
                3,
                "an embedded vector database written in rust",
                [0.9, 0.4, 0.0],
            ),
        ] {
            db.execute(
                "INSERT INTO notes (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Text(body.to_string().into()),
                    Value::Vector(embedding.to_vec()),
                ],
            )
            .unwrap();
        }
        Self {
            server: Server::new(db, allow_writes, Limits::default()),
            next_id: 1,
        }
    }

    /// Send a request and return its `result`, failing loudly on a JSON-RPC
    /// error.
    fn request(&mut self, method: &str, params: Json) -> Json {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let response = self
            .server
            .handle_line(&line.to_string())
            .unwrap_or_else(|| panic!("{method} got no response"));
        let response: Json = serde_json::from_str(&response).expect("valid JSON response");

        assert_eq!(response["jsonrpc"], json!("2.0"));
        assert_eq!(
            response["id"],
            json!(id),
            "response id did not match request"
        );
        assert!(
            response.get("error").is_none(),
            "{method} failed: {}",
            response["error"]
        );
        response["result"].clone()
    }

    /// Call a tool and return its text content, plus whether it reported an
    /// error.
    fn call(&mut self, name: &str, arguments: Json) -> (Json, bool) {
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} returned no text content: {result}"))
            .to_string();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        // A failing tool reports prose, not JSON, so only parse when it worked.
        let parsed = if is_error {
            json!(text)
        } else {
            serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{name} returned unparseable JSON: {e}\n{text}"))
        };
        (parsed, is_error)
    }
}

#[test]
fn the_handshake_reports_what_the_server_speaks() {
    let mut client = Client::new(false);
    let result = client.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" },
        }),
    );
    assert_eq!(
        result["protocolVersion"],
        json!(inlaysql_mcp::PROTOCOL_VERSION)
    );
    assert_eq!(result["serverInfo"]["name"], json!("inlaysql"));
    assert!(result["capabilities"]["tools"].is_object());
}

#[test]
fn a_notification_is_not_answered() {
    let mut client = Client::new(false);
    let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert!(
        client
            .server
            .handle_line(&notification.to_string())
            .is_none(),
        "answering a notification will wedge a client"
    );
}

#[test]
fn the_tool_list_describes_every_tool_with_a_schema() {
    let mut client = Client::new(false);
    let result = client.request("tools/list", json!({}));
    let tools = result["tools"].as_array().expect("tools array");

    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"schema"));
    assert!(names.contains(&"query"));
    assert!(names.contains(&"hybrid_search"));
    assert!(names.contains(&"changes"));

    for tool in tools {
        assert!(
            tool["description"].as_str().is_some_and(|d| d.len() > 20),
            "{} has no useful description",
            tool["name"]
        );
        assert_eq!(
            tool["inputSchema"]["type"],
            json!("object"),
            "{} has no object input schema",
            tool["name"]
        );
    }
}

#[test]
fn schema_reports_the_columns_and_their_types() {
    let mut client = Client::new(false);
    let (schema, is_error) = client.call("schema", json!({}));
    assert!(!is_error);

    let table = &schema["tables"][0];
    assert_eq!(table["table"], json!("notes"));
    let columns = table["columns"].as_array().unwrap();
    assert_eq!(columns[0]["name"], json!("id"));
    assert_eq!(columns[0]["primary_key"], json!(true));
    assert_eq!(columns[2]["type"], json!("VECTOR(3)"));
}

#[test]
fn a_hybrid_search_fuses_both_retrievers_in_one_call() {
    let mut client = Client::new(false);
    let (result, is_error) = client.call(
        "hybrid_search",
        json!({
            "table": "notes",
            "text_column": "body",
            "vector_column": "embedding",
            "query": "embedded database",
            "embedding": [1.0, 0.2, 0.0],
            "limit": 3,
        }),
    );
    assert!(!is_error, "hybrid_search failed: {result}");

    let rows = result["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 3);
    // The row both retrievers like ranks first.
    assert_eq!(rows[0][0], json!(1));
    // Embeddings are summarised rather than dumped into the model's context.
    assert_eq!(rows[0][2], json!("<vector(3)>"));
}

#[test]
fn a_text_only_search_needs_no_embedding() {
    let mut client = Client::new(false);
    let (result, is_error) = client.call(
        "hybrid_search",
        json!({ "table": "notes", "text_column": "body", "query": "rust" }),
    );
    assert!(!is_error, "{result}");
    assert_eq!(result["rows"].as_array().unwrap()[0][0], json!(3));
}

#[test]
fn a_vector_column_without_an_embedding_is_refused_clearly() {
    let mut client = Client::new(false);
    let (message, is_error) = client.call(
        "hybrid_search",
        json!({
            "table": "notes",
            "text_column": "body",
            "vector_column": "embedding",
            "query": "rust",
        }),
    );
    assert!(is_error);
    assert!(
        message.as_str().unwrap().contains("embedding"),
        "unhelpful message: {message}"
    );
}

#[test]
fn a_read_only_server_refuses_writes_and_does_not_perform_them() {
    let mut client = Client::new(false);

    // `execute` is not advertised at all...
    let tools = client.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"execute"));

    // ...and calling it anyway is refused.
    let (message, is_error) = client.call("execute", json!({ "sql": "DELETE FROM notes" }));
    assert!(is_error);
    assert!(message.as_str().unwrap().contains("read-only"));

    // ...as is smuggling a write through `query`.
    let (message, is_error) = client.call("query", json!({ "sql": "DELETE FROM notes" }));
    assert!(is_error, "a write through `query` was not refused");
    assert!(message.as_str().unwrap().contains("read-only"));

    // And nothing was deleted.
    let (result, _) = client.call("query", json!({ "sql": "SELECT id FROM notes" }));
    assert_eq!(
        result["row_count"],
        json!(3),
        "the refused write happened anyway"
    );
}

#[test]
fn an_identifier_that_is_not_an_identifier_is_refused() {
    let mut client = Client::new(false);
    let (message, is_error) = client.call(
        "hybrid_search",
        json!({
            "table": "notes; DROP TABLE notes",
            "text_column": "body",
            "query": "rust",
        }),
    );
    assert!(is_error);
    assert!(message.as_str().unwrap().contains("identifier"));

    // The table is still there.
    let (result, _) = client.call("query", json!({ "sql": "SELECT id FROM notes" }));
    assert_eq!(result["row_count"], json!(3));
}

#[test]
fn a_writable_server_executes_and_the_change_shows_up_in_the_stream() {
    let mut client = Client::new(true);

    let (before, _) = client.call("changes", json!({ "from": 0 }));
    let version = before["version"].as_u64().unwrap();

    let (written, is_error) = client.call(
        "execute",
        json!({
            "sql": "INSERT INTO notes (id, body, embedding) VALUES (?, ?, ?)",
            "params": [4, "a new note about embedded storage", [0.5, 0.5, 0.0]],
        }),
    );
    assert!(!is_error, "execute failed: {written}");
    assert_eq!(written["rows_written"], json!(1));

    let (after, _) = client.call("changes", json!({ "from": version }));
    let changes = after["changes"].as_array().expect("changes");
    assert_eq!(changes.len(), 1, "expected exactly the one insert: {after}");
    assert_eq!(changes[0]["kind"], json!("insert"));
    assert_eq!(changes[0]["table"], json!("notes"));
    assert_eq!(changes[0]["id"], json!(4));
    assert_eq!(after["lost"], json!(false));
    assert!(after["version"].as_u64().unwrap() > version);

    // And the row it points at is readable, which is the whole contract: the
    // log says what changed, the database says what it is.
    let (row, _) = client.call(
        "query",
        json!({ "sql": "SELECT body FROM notes WHERE id = ?", "params": [4] }),
    );
    assert_eq!(
        row["rows"][0][0],
        json!("a new note about embedded storage")
    );
}

#[test]
fn an_inline_vector_literal_is_refused_and_binding_is_the_way() {
    // A model that has only read the SQL dialect reaches for the inline form
    // first. `docs/mcp.md` promises the refusal names the fix, so the promise
    // is asserted here rather than left to rot.
    let mut client = Client::new(true);

    let (refused, is_error) = client.call(
        "execute",
        json!({ "sql": "INSERT INTO notes (id, body, embedding) VALUES (5, 'inline', [0.1, 0.2, 0.3])" }),
    );
    assert!(is_error, "an inline vector literal must not be accepted");
    let message = refused.as_str().expect("prose error");
    assert!(
        message.contains("placeholder"),
        "the refusal must point at binding, got: {message}"
    );

    // The same row, bound, goes in.
    let (written, is_error) = client.call(
        "execute",
        json!({
            "sql": "INSERT INTO notes (id, body, embedding) VALUES (?, ?, ?)",
            "params": [5, "inline", [0.1, 0.2, 0.3]],
        }),
    );
    assert!(!is_error, "the bound form failed: {written}");
    assert_eq!(written["rows_written"], json!(1));
}

#[test]
fn an_unknown_method_gets_a_json_rpc_error_not_a_crash() {
    let mut client = Client::new(false);
    let line = json!({ "jsonrpc": "2.0", "id": 99, "method": "nope/nope" });
    let response: Json =
        serde_json::from_str(&client.server.handle_line(&line.to_string()).unwrap()).unwrap();
    assert_eq!(response["error"]["code"], json!(-32601));
    assert_eq!(response["id"], json!(99));
}

#[test]
fn malformed_input_gets_a_parse_error_not_a_crash() {
    let mut client = Client::new(false);
    let response: Json =
        serde_json::from_str(&client.server.handle_line("{ this is not json").unwrap()).unwrap();
    assert_eq!(response["error"]["code"], json!(-32700));
    assert_eq!(response["id"], Json::Null);
}

#[test]
fn a_broken_statement_is_reported_to_the_model_not_to_the_transport() {
    let mut client = Client::new(false);
    let (message, is_error) = client.call("query", json!({ "sql": "SELECT FROM WHERE" }));
    assert!(is_error);
    assert!(!message.as_str().unwrap().is_empty());
}

#[test]
fn a_whole_session_runs_over_the_line_protocol() {
    // The same conversation a client actually has, driven through `serve` end
    // to end rather than message by message.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    db.execute(
        "INSERT INTO t (id, body) VALUES (?, ?)",
        &[Value::Integer(1), Value::Text("hello".into())],
    )
    .unwrap();
    let mut server = Server::new(db, false, Limits::default());

    let session = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query","arguments":{"sql":"SELECT body FROM t"}}}"#,
    ]
    .join("\n");

    let mut output = Vec::new();
    server
        .serve(std::io::Cursor::new(session), &mut output)
        .expect("serve");

    let lines: Vec<&str> = std::str::from_utf8(&output)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(lines.len(), 3, "the notification was answered: {lines:?}");

    let last: Json = serde_json::from_str(lines[2]).unwrap();
    let text = last["result"]["content"][0]["text"].as_str().unwrap();
    let rows: Json = serde_json::from_str(text).unwrap();
    assert_eq!(rows["rows"][0][0], json!("hello"));
}
