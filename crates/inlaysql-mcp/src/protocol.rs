//! The JSON-RPC 2.0 envelope MCP rides on.

use serde_json::{json, Value};

/// One incoming message.
pub struct Request {
    /// `None` for a notification, which must not be answered.
    pub id: Option<Value>,
    /// The method name.
    pub method: String,
    /// The parameters, or `Value::Null` when absent.
    pub params: Value,
}

impl Request {
    /// Parse one line of JSON-RPC.
    pub fn parse(line: &str) -> Result<Self, String> {
        let message: Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Err("message has no `method`".to_string());
        };
        Ok(Self {
            // `null` as an explicit id is a notification too, per JSON-RPC.
            id: message.get("id").filter(|id| !id.is_null()).cloned(),
            method: method.to_string(),
            params: message.get("params").cloned().unwrap_or(Value::Null),
        })
    }
}

/// One outgoing message.
pub struct Response(Value);

impl Response {
    /// A successful result.
    pub fn result(id: Value, result: Value) -> Self {
        Self(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    /// An error against a known id.
    pub fn error(id: Value, code: i64, message: &str) -> Self {
        Self(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
    }

    /// JSON-RPC's parse error, which by definition has no id to answer to.
    pub fn parse_error(message: &str) -> Self {
        Self::error(Value::Null, -32700, message)
    }

    /// JSON-RPC's "method not found".
    pub fn method_not_found(id: Value, method: &str) -> Self {
        Self::error(id, -32601, &format!("unknown method `{method}`"))
    }
}

impl std::fmt::Display for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The `initialize` result: what we speak and what we can do.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": crate::PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "inlaysql",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions":
            "An InlaySQL database file. `schema` first, then `query` for SQL or \
             `hybrid_search` for retrieval that combines vector similarity with \
             BM25 in one ranking. `changes` streams committed row changes since \
             a version you supply.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_has_no_id() {
        let request = Request::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .expect("parse");
        assert!(request.id.is_none());
    }

    #[test]
    fn an_explicit_null_id_is_also_a_notification() {
        let request =
            Request::parse(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).expect("parse");
        assert!(request.id.is_none());
    }

    #[test]
    fn a_request_keeps_its_id_type() {
        // Ids may be numbers or strings, and the reply has to echo the same
        // one back — a client matching on `"1"` will not accept `1`.
        let numeric = Request::parse(r#"{"id":1,"method":"ping"}"#).unwrap();
        assert_eq!(numeric.id, Some(json!(1)));
        let string = Request::parse(r#"{"id":"a","method":"ping"}"#).unwrap();
        assert_eq!(string.id, Some(json!("a")));
    }

    #[test]
    fn a_message_without_a_method_is_rejected() {
        assert!(Request::parse(r#"{"id":1}"#).is_err());
    }

    #[test]
    fn malformed_json_is_rejected_not_panicked() {
        assert!(Request::parse("{not json").is_err());
        assert!(Request::parse("").is_err());
    }

    #[test]
    fn missing_params_read_as_null_rather_than_failing() {
        let request = Request::parse(r#"{"id":1,"method":"tools/list"}"#).unwrap();
        assert!(request.params.is_null());
        // Indexing into null yields null, which is what the tool layer expects
        // when it reads an absent argument.
        assert!(request.params["name"].is_null());
    }
}
