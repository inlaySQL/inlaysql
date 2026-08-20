//! An MCP (Model Context Protocol) server over an InlaySQL database file.
//!
//! ```sh
//! inlaysql serve --mcp app.inlay              # read-only
//! inlaysql serve --mcp app.inlay --allow-writes
//! ```
//!
//! # Why the protocol is implemented here rather than taken from the SDK
//!
//! The official Rust MCP SDK is built on Tokio and a sizeable async stack.
//! InlaySQL's entire proposition is that a database is one file and one small
//! dependency, and this server needs three JSON-RPC methods over stdio —
//! `initialize`, `tools/list`, `tools/call` — plus a handful of notifications
//! it can ignore. Speaking that directly costs a few hundred lines and one
//! dependency (`serde_json`); taking the SDK would cost the async runtime in
//! every build that wants the CLI. If the protocol grows past what is here, or
//! we need transports other than stdio, that trade should be revisited.
//!
//! # Guard rails
//!
//! The server is **read-only unless told otherwise**, because the thing on the
//! other end is a language model and the database is somebody's data:
//!
//! * `execute` is not even advertised without `--allow-writes`, so a model
//!   cannot decide to try it.
//! * Read-only is enforced by *planning* the statement and checking the plan is
//!   a read, not by looking at the first word of the SQL.
//! * Results are capped by row count and by serialised size, so one
//!   `SELECT * FROM` cannot blow up a context window or the process.
//! * Every embedding, row and error crosses the boundary as text; nothing here
//!   evaluates anything the client sends other than as SQL against the engine.
//! * Without `--allow-writes` the database itself is opened read-only
//!   ([`inlaysql::Database::open_read_only`]), not merely treated that way at
//!   this layer: no OS advisory lock is taken, so the server can sit beside an
//!   application that already has the file open for writing, and every
//!   statement pays a write-ahead-log scan instead of the fast path a
//!   read-write handle gets. `docs/mcp.md` has the numbers and the trade.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod protocol;
mod tools;

use std::io::{BufRead, Write};

use inlaysql::Database;

pub use protocol::{Request, Response};
pub use tools::{Limits, ToolError};

/// The MCP protocol revision this server speaks.
///
/// Advertised verbatim in the `initialize` result. A client that wants a
/// different revision is told what we have rather than being guessed at.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A server bound to one database file.
pub struct Server {
    db: Database,
    limits: Limits,
    /// Whether the `execute` tool exists at all.
    allow_writes: bool,
}

impl Server {
    /// Open `path` and serve it.
    ///
    /// Without `--allow-writes` this opens [`Database::open_read_only`]
    /// rather than [`Database::open`] — no OS advisory lock is taken, so the
    /// server can sit beside an application that already has the file open
    /// for writing, restoring the workflow `docs/mcp.md` describes. The
    /// `execute` tool is refused twice over in that mode: it is never
    /// advertised in `tools/list`, and even a direct call would be refused by
    /// the read-only handle itself. `--allow-writes` opens the same
    /// read-write handle as before, which takes the exclusive lock and
    /// refuses a second writing process.
    pub fn open(path: &str, allow_writes: bool, limits: Limits) -> inlaysql::Result<Self> {
        let db = if allow_writes {
            Database::open(path)?
        } else {
            Database::open_read_only(path)?
        };
        Ok(Self {
            db,
            limits,
            allow_writes,
        })
    }

    /// Serve an already-open database. Used by the tests, which drive an
    /// in-memory database rather than a file.
    pub fn new(db: Database, allow_writes: bool, limits: Limits) -> Self {
        Self {
            db,
            limits,
            allow_writes,
        }
    }

    /// Read newline-delimited JSON-RPC from `input` until it ends, writing each
    /// response to `output`.
    ///
    /// Notifications (a message with no `id`) get no reply, as the protocol
    /// requires — replying to one is a common way to wedge a client.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Some(response) = self.handle_line(&line) else {
                continue;
            };
            writeln!(output, "{response}")?;
            output.flush()?;
        }
        Ok(())
    }

    /// Handle one line, returning the response to write (if any).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let request = match Request::parse(line) {
            Ok(request) => request,
            // A message we cannot even parse has no id to answer to, so the
            // only correct reply is a null-id error.
            Err(message) => return Some(Response::parse_error(&message).to_string()),
        };
        let id = request.id.clone()?;
        Some(self.dispatch(&request, id).to_string())
    }

    fn dispatch(&mut self, request: &Request, id: serde_json::Value) -> Response {
        match request.method.as_str() {
            "initialize" => Response::result(id, protocol::initialize_result()),
            "ping" => Response::result(id, serde_json::json!({})),
            "tools/list" => Response::result(
                id,
                serde_json::json!({ "tools": tools::descriptors(self.allow_writes) }),
            ),
            "tools/call" => self.call_tool(request, id),
            other => Response::method_not_found(id, other),
        }
    }

    fn call_tool(&mut self, request: &Request, id: serde_json::Value) -> Response {
        let name = request.params["name"].as_str().unwrap_or_default();
        let arguments = &request.params["arguments"];

        match tools::call(
            &mut self.db,
            name,
            arguments,
            self.allow_writes,
            &self.limits,
        ) {
            Ok(text) => Response::result(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false,
                }),
            ),
            // A tool that fails is reported *inside* a successful result with
            // `isError`, which is what MCP asks for: the model is supposed to
            // read the failure and try something else, not have the transport
            // treat it as a protocol fault.
            Err(error) => Response::result(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": error.to_string() }],
                    "isError": true,
                }),
            ),
        }
    }
}
