# MCP server mode

An InlaySQL database file is a tool an agent can use directly. No glue code, no
schema translation layer, no vector store to keep in sync.

```sh
cargo install --path crates/inlaysql-mcp     # provides the `inlaysql` binary
inlaysql serve --mcp app.inlay
```

The server speaks [MCP](https://modelcontextprotocol.io) as JSON-RPC over
stdin/stdout, so a client is wired up by pointing its command at the binary.

**Without `--allow-writes` the database is opened read-only, and that is true
at the file level, not just at the tool boundary.** `inlaysql serve --mcp
app.inlay` beside an application that already has `app.inlay` open for
writing works: the read-only handle takes no OS lock at all, so it can open
and read the file underneath a process that holds it exclusively for writes.
That is the whole point — an agent should be able to look at a database a
running application is using, without either process refusing the other.

The file has to exist already: this mode never creates one, so a typo'd path
fails loudly instead of quietly serving an empty database. Create the file
first — with a normal writing connection, or by running the server once with
`--allow-writes` — before pointing a read-only agent at it.

**What this does and does not protect against.** A read-only reader coexists
safely with *one writer process that is itself using InlaySQL's locking* —
i.e. another `inlaysql` process, or any process that opened the file with
[`FileDevice::open`](../crates/inlaysql/src/device.rs). It has no OS-level
protection against a process that writes to the same bytes without going
through that path at all (a text editor, `dd`, a script that truncates the
file) — nothing does, for any database that is one plain file. The one-file
design (`README.md`: "A database is **one file**") is what makes the
read-only mode possible without a sidecar lock file; it is also why the
guarantee is "coexists with a cooperating writer," not "immune to every other
process on the machine."

**The cost is a full log scan on every statement, and it is real.** A
read-only handle has no in-process proof that it is the only writer — it
never locks one out — so it cannot answer "did anything change since my last
statement?" without asking the file. Concretely: a read-write handle answers
that question from an in-process counter in a few nanoseconds and only
re-reads the file when the answer is "yes"; a read-only handle re-reads the
committed state and scans the write-ahead log on *every* statement, whether
anything changed or not. Measured on one machine: roughly 236 µs for the
read-only path against roughly 7 µs for the read-write fast path on the same
point read — a real difference for a chatty agent, not noise. An incremental
WAL scan (read only the records appended since the last look, instead of the
whole region) would close most of that gap; it does not exist yet — see the
follow-up noted in `CowBTree::refresh`'s doc comment in
`crates/inlaysql-core/src/btree/tree.rs`.

## Wiring it into a client

Claude Code, or any MCP client that takes a command:

```json
{
  "mcpServers": {
    "notes": {
      "command": "inlaysql",
      "args": ["serve", "--mcp", "/path/to/notes.inlay"]
    }
  }
}
```

Add `"--allow-writes"` to the args to let the agent change the data. Think
about that before you do — see below.

## The tools

| Tool | What it does |
| --- | --- |
| `schema` | Tables, columns, types. Call it first. |
| `query` | A read-only SQL statement. `?` placeholders bind from `params`. |
| `hybrid_search` | BM25 over a text column, optionally fused with vector similarity, in one ranking. |
| `changes` | Committed row changes after a version you supply. |
| `execute` | Statements that write. **Only present with `--allow-writes`.** |

`hybrid_search` exists because it is the thing an agent most wants and the
thing it is least likely to write correctly by hand:

```json
{
  "name": "hybrid_search",
  "arguments": {
    "table": "notes",
    "text_column": "body",
    "vector_column": "embedding",
    "query": "how does crash recovery work",
    "embedding": [0.12, -0.03, ...],
    "limit": 5
  }
}
```

which the server turns into one ordinary statement:

```sql
SELECT *, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
FROM notes ORDER BY score DESC LIMIT 5
```

Omit `vector_column` and `embedding` for text-only search. There is no separate
index to build and no second query language.

## Binding values, and vectors in particular

`query` and `execute` take a `params` array bound to `?` placeholders in order.
Numbers, strings and null bind as themselves; **an array of numbers binds as a
vector**, and that is the only way to write one:

```json
{
  "name": "execute",
  "arguments": {
    "sql": "INSERT INTO notes VALUES (?, ?, ?)",
    "params": [1, "crash recovery replays the write-ahead log", [0.1, 0.2, 0.3]]
  }
}
```

There is no vector literal in the SQL surface, so an embedding written inline —
`INSERT INTO notes VALUES (1, 'text', [0.1, 0.2, 0.3])` — is refused with
`expected a literal or ? placeholder`. A model that has only seen the SQL
dialect will reach for the inline form first; the failure names the fix, and
binding is the shape to reach for anyway.

## Guard rails

The client is a language model and the database is somebody's data, so the
defaults are the cautious ones.

**Read-only unless told otherwise, at every layer.** Without `--allow-writes`
the `execute` tool is not merely refused — it is not advertised in
`tools/list` at all, so a model cannot be tempted by a tool it never saw. The
database underneath is opened read-only too (see above), so even a `query`
call that smuggled in a write would be refused by the database handle itself,
not just by the tool dispatch.

**Read-only is enforced by planning, not by pattern-matching.** `query` runs
the statement through the planner and checks the resulting plan is a read
before anything touches the database. Looking at the first word of the SQL
would be theatre; a plan either reads or it does not.

**Identifiers are validated before interpolation.** `hybrid_search` builds SQL
from the table and column names it is given, so those names must be plain
identifiers — letters, digits, underscore, not starting with a digit. A table
name carrying a semicolon is refused.

**Results are capped twice.** `--max-rows` (default 200) keeps a result
countable; `--max-bytes` (default 65536) stops one very wide row from defeating
that. Both are reported in the response (`row_count`, `truncated`) rather than
silently applied, so the model knows it is looking at a prefix.

**Embeddings are summarised.** A `VECTOR(384)` renders as `<vector(384)>`.
Three hundred and eighty-four floats mean nothing to a reader and would swamp
the context window.

**A failing tool is a result, not a transport error.** MCP asks for failures to
come back inside a successful response with `isError: true`, so the model can
read what went wrong and try something else. A malformed query is the model's
problem to fix, not a reason to drop the connection.

## Change data capture

```sh
inlaysql changes app.inlay --from 0
```

```
41	insert	notes	17
41	insert	notes	18
42	update	notes	3
43	delete	notes	9
```

Columns are version, kind, table, row id. Every row changed by one statement
shares a version, and versions increase with commit order.

**A record says what changed, not what it became.** Read the row with `query`
to get its current contents. That is a deliberate trade:

- A row carrying an embedding is a kilobyte and a half. Copying every version
  of every row into a log inside the same file turns a bounded feature into an
  unbounded one.
- A consumer that is keeping up gets the current value, which is what an
  agent-memory pipeline wants.
- A consumer that has fallen behind cannot be served correctly either way — the
  row it missed has already been overwritten. Storing stale payloads would let
  it *believe* it was caught up, which is worse than telling it the truth.

**Check `lost`.** The log keeps the most recent 4096 statements. If your
position has fallen out of that window, `lost` is true and the CLI warns on
stderr; the only correct response is to resynchronise from a full read. A short
list returned silently would be indistinguishable from "nothing happened".

## Why the protocol is hand-written

The official Rust MCP SDK is built on Tokio and a sizeable async stack.
InlaySQL's whole proposition is that a database is one file and one small
dependency, and this server needs three JSON-RPC methods — `initialize`,
`tools/list`, `tools/call` — plus notifications it ignores. Speaking that
directly costs a few hundred lines and one dependency (`serde_json`); taking
the SDK would put an async runtime in every build that wants the CLI.

That is a trade, not a principle. If the protocol surface grows, or we need a
transport other than stdio, it should be revisited.

`crates/inlaysql-mcp/tests/client.rs` drives a client through the real line
protocol — handshake, tool list, hybrid search, a write, and the change event
it produces — so the wire format is checked on every `cargo test` rather than
by hand against one client.
