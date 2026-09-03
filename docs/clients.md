# Using InlaySQL from PHP, Python, Ruby, Node.js, C#, Java — and Rust

InlaySQL reaches every common web language through **three directions**, and
which one applies is decided by what the language can load:

| Direction | How it works | Languages |
| --- | --- | --- |
| **Embed (library)** — the engine is a library in your process, the file opened directly | The engine compiles into your binary/runtime — or into a C shared library any FFI loads | Rust (native crate); any FFI language through [`inlaysql-ffi`](../crates/inlaysql-ffi/README.md): PHP, Python, Ruby, C#/.NET, Java |
| **Embed (WASM)** — the engine is the WASM module in your runtime | The `inlaysql-js` SDK wraps the module; no server, the bytes are yours | JavaScript / TypeScript (Node, Deno, Bun, browser, edge) |
| **MySQL wire** — the database is a server speaking MySQL's protocol | `inlaysql serve --mysql` opens the file; any MySQL driver connects | PHP, Python, Ruby, C#/.NET, Java, Node.js, Go — anything with a MySQL driver |

**The C ABI is the SQLite-like adapter.** `inlaysql-ffi` builds to
`libinlaysql.dylib`/`.so`/`.dll` with a seven-function C API
(`inlaysql_open` / `inlaysql_exec` / `inlaysql_close` …, header in
[`include/inlaysql.h`](../crates/inlaysql-ffi/include/inlaysql.h)). A
language that can call C functions can open the file in-process — no server
process, the deployment shape SQLite has. A working PHP FFI example ships in
[`crates/inlaysql-ffi/examples/poc.php`](../crates/inlaysql-ffi/examples/poc.php):

```php
$ffi = FFI::cdef('
    InlaysqlHandle *inlaysql_open(const char *path);
    int inlaysql_exec(InlaysqlHandle *h, const char *sql,
                      const char *params, char **out_json);
    // …
', 'libinlaysql.dylib');

$handle = $ffi->inlaysql_open('app.inlay');
$result = inlaysql_exec($ffi, $handle, 'SELECT id, title FROM docs');
```

One honest correction to a natural assumption, stated up front because it
saves an afternoon: **a SQLite driver cannot open an InlaySQL file.** The
project borrows SQLite's *model* — one file, no server, copy it around — not
its on-disk format. The format is InlaySQL's own, and it is what makes the
native vector/BM25 indexes and the MVCC writer possible. "SQLite
compatibility" here means the SQL dialect, not the bytes. For languages that
cannot load the engine as a library, the MySQL shim is the bridge — that is
what it exists for, and a stock Laravel 11 skeleton runs against it today
(see [`server.md`](server.md) for exactly how far that goes).

```sh
# The server side of direction two. One command, one file, one port.
inlaysql serve --mysql app.inlay --password-env INLAYSQL_PASSWORD
#   --tls-cert server.pem --tls-key key.pem          # enable TLS (recommended)
#   --tls-required                                    # refuse plaintext logins
#   --port 3306                                       # the default
```

Security posture, in one paragraph: the protocol is **plaintext until you
give it a certificate**, and the server advertises that honestly rather than
allowing a silent downgrade. With `--tls-cert`/`--tls-key` a client that asks
upgrades before sending credentials; with `--strong-passwords` (needs TLS)
accounts store salted PBKDF2 instead of the wire protocol's fast-hash
verifiers and can only log in over TLS. For anything beyond localhost, turn
TLS on. Full detail: [`server.md`'s Security section](server.md#security).

---

## Direction 1 — Embed

### Rust (native)

```toml
[dependencies]
inlaysql = "0"          # the file-backed database
```

```rust
use inlaysql::{Database, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = Database::open("app.inlay")?;

    db.execute(
        "CREATE TABLE IF NOT EXISTS docs (
            id INTEGER PRIMARY KEY,
            title TEXT,
            body TEXT
        )",
        &[],
    )?;
    db.execute(
        "INSERT INTO docs (title, body) VALUES (?, ?)",
        &[Value::Text("Hello".into()), Value::Text("World".into())],
    )?;

    let rows = db.query("SELECT id, title FROM docs WHERE id = ?", &[Value::Integer(1)])?;
    for row in &rows.rows {
        println!("{:?}", row);
    }
    Ok(())
}
```

The crate is `#![forbid(unsafe_code)]`, thread-per-handle, and every handle
on the same file commits with MVCC — no writer lock around the whole
database. Vectors are a column type: `VECTOR(384)`, with
`vector_score`/`bm25_score`/`fuse` for retrieval (see
[`indexes.md`](indexes.md)).

### PHP / Python / Ruby / C# / Java — the C ABI (in-process, no server)

The same file, opened in-process, through the C ABI
([`inlaysql-ffi`](../crates/inlaysql-ffi/README.md)). Each language's
binding is a thin loader over the same seven functions — PHP's built-in FFI
ext needs ~40 lines, shown in
[`examples/poc.php`](../crates/inlaysql-ffi/examples/poc.php); Python's
`ctypes`, Ruby's `ffi` gem, .NET's `DllImport` and Java's FFM are each of
the same size. Build once:

```sh
cargo build -p inlaysql-ffi --release   # → libinlaysql_ffi.{dylib,so,dll}
```

```python
# Python (ctypes) — the whole binding:
import ctypes
lib = ctypes.CDLL("libinlaysql_ffi.so")
lib.inlaysql_exec.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                              ctypes.c_char_p, ctypes.POINTER(ctypes.c_char_p)]
h = lib.inlaysql_open(b"app.inlay")
out = ctypes.c_char_p()
lib.inlaysql_exec(h, b"SELECT id, title FROM docs", None, ctypes.byref(out))
print(out.value)  # {"columns":["id","title"],"rows":[[1,"Hello"]]}
```

The result JSON is the same shape the WASM and MySQL surfaces produce, so
what you learn binding one language applies to all of them.

### JavaScript / TypeScript (Node, Deno, Bun — and the browser)

The `inlaysql-js` SDK wraps the WASM build: typed results, storage adapters,
and an ORM whose `search()` generates the hybrid query. Plain ESM, zero
runtime dependencies, no build step.

```js
import { openDatabase } from "@inlaysql/core";
import { opfs } from "@inlaysql/storage";          // Node: memory() / bytes()
import { defineModel, field, install, repo } from "@inlaysql/orm";

const Page = defineModel("pages", {
  id: field.integer().primaryKey(),
  title: field.text(),
  body: field.text().index("bm25"),
  embedding: field.vector(384).index("hnsw").embedFrom("body"),
});

const db = await openDatabase({ source: opfs("app.inlay"), create: true });
await install(db, Page);

await repo(db, Page).insert({ title: "Hello", body: "World" }); // embedding computed

const hits = await repo(db, Page).search("hello world", { mode: "hybrid", limit: 8 });
console.log(hits.rows, hits.sql);  // the SQL is part of the result
```

Live, runnable versions of exactly this: the
[vanilla SDK demo](https://inlaysql.github.io/demo/js-sdk/) and the
[one-script-tag demo](https://inlaysql.github.io/demo/js-sdk/simple.html).
Repository: [inlaySQL/inlaysql-js](https://github.com/inlaySQL/inlaysql-js).

---

## Direction 2 — MySQL wire

Everything below assumes the server from the top of this file is running.
The database is the file the server opened — there is no `CREATE DATABASE`
and no schema selection client-side; the drivers below connect with whatever
default database name your tooling insists on and it is accepted.

### PHP (PDO — the Laravel/Eloquent path)

A stock Laravel 11 skeleton runs against the shim: `php artisan migrate`
completes, ordinary Eloquent traffic works. Outside Laravel, PDO speaks for
itself:

```php
$pdo = new PDO(
    'mysql:host=127.0.0.1;port=3306;dbname=app;charset=utf8mb4',
    'inlaysql',
    getenv('INLAYSQL_PASSWORD'),
    [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION],
);

$stmt = $pdo->prepare('INSERT INTO docs (title, body) VALUES (?, ?)');
$stmt->execute([$title, $body]);

foreach ($pdo->query('SELECT id, title FROM docs ORDER BY id LIMIT 20') as $row) {
    echo $row['id'], ' ', $row['title'], PHP_EOL;
}

// Upserts, RETURNING, CTEs and set operations all work (see server.md):
$pdo->prepare('INSERT INTO docs (id, title) VALUES (?, ?)
               ON CONFLICT (id) DO UPDATE SET title = excluded.title')
    ->execute([$id, $title]);
```

`mysqli` works the same way. PHP binds integers as strings by default; the
engine applies SQLite's comparison affinity, so `WHERE id = ?` with a string
`'1'` finds the integer row (this was a real bug once — AHL-486 — and is
covered by tests).

### Python (PyMySQL, mysqlclient, or SQLAlchemy's mysql dialect)

```python
import pymysql

conn = pymysql.connect(
    host="127.0.0.1", port=3306,
    user="inlaysql", password=os.environ["INLAYSQL_PASSWORD"],
    autocommit=True,
)

with conn.cursor() as cur:
    cur.execute("INSERT INTO docs (title, body) VALUES (%s, %s)", (title, body))
    cur.execute("SELECT id, title FROM docs WHERE body LIKE %s LIMIT 10", (f"%{q}%",))
    for row in cur.fetchall():
        print(row)
```

SQLAlchemy works through its `mysql` dialect for the same reason PDO does:
the wire is MySQL's, so the driver is MySQL's. Point SQLAlchemy's URL at the
server (`mysql+pymysql://...`) and ORM traffic runs — within the SQL surface
[`server.md`](server.md#what-does-not-work-yet) documents.

### Ruby (mysql2 gem)

```ruby
require "mysql2"

client = Mysql2::Client.new(
  host: "127.0.0.1", port: 3306,
  username: "inlaysql", password: ENV["INLAYSQL_PASSWORD"],
)

client.query("INSERT INTO docs (title, body) VALUES ('#{q}', '#{b}')") # bind in real code
results = client.query("SELECT id, title FROM docs ORDER BY id LIMIT 20")
results.each { |row| puts row["title"] }
```

### C# / .NET (MySqlConnector)

```csharp
using MySqlConnector;

await using var conn = new MySqlConnection(
    "Server=127.0.0.1;Port=3306;User ID=inlaysql;Password=...");
await conn.OpenAsync();

await using (var cmd = new MySqlCommand(
    "INSERT INTO docs (title, body) VALUES (@t, @b)", conn))
{
    cmd.Parameters.AddWithValue("@t", title);
    cmd.Parameters.AddWithValue("@b", body);
    await cmd.ExecuteNonQueryAsync();
}

await using var select = new MySqlCommand(
    "SELECT id, title FROM docs ORDER BY id LIMIT 20", conn);
await using var reader = await select.ExecuteReaderAsync();
while (await reader.ReadAsync())
    Console.WriteLine(reader.GetInt32("id"));
```

### Java (JDBC)

```java
try (Connection c = DriverManager.getConnection(
        "jdbc:mysql://127.0.0.1:3306/app", "inlaysql", password);
     PreparedStatement insert = c.prepareStatement(
        "INSERT INTO docs (title, body) VALUES (?, ?)");
     PreparedStatement select = c.prepareStatement(
        "SELECT id, title FROM docs ORDER BY id LIMIT 20")) {

    insert.setString(1, title);
    insert.setString(2, body);
    insert.executeUpdate();

    try (ResultSet rs = select.executeQuery()) {
        while (rs.next()) System.out.println(rs.getString("title"));
    }
}
```

Hibernate, jOOQ and friends ride the same wire; the same SQL-surface caveats
apply. JDBC's prepared statements use the binary protocol, which is also the
**only** way a `BLOB` column accepts a value today — the text protocol has no
binary literal, so send blobs through a binary-protocol driver (JDBC and
MySqlConnector do; see [`server.md`](server.md#what-does-not-work-yet)).

### Node.js (mysql2)

Node has both directions: the WASM SDK above for embedding, or `mysql2` over
the wire when you want the file owned by one server process:

```js
import mysql from "mysql2/promise";

const conn = await mysql.createConnection({
  host: "127.0.0.1", port: 3306, user: "inlaysql",
  password: process.env.INLAYSQL_PASSWORD,
});

await conn.execute("INSERT INTO docs (title, body) VALUES (?, ?)", [title, body]);
const [rows] = await conn.query("SELECT id, title FROM docs ORDER BY id LIMIT 20");
```

---

## What works over the wire — and what does not

The protocol is not the limit; the SQL surface underneath it is. What runs
today, from [`server.md`](server.md#what-does-not-work-yet):

- Full DML: `SELECT` (joins, subqueries, CTEs including `WITH RECURSIVE`,
  set operations), `INSERT` (upserts, `INSERT ... SELECT`), `UPDATE`,
  `DELETE`, `RETURNING` on all three.
- Full constraint DDL inside `CREATE TABLE`, plus the standalone
  `ALTER TABLE ADD {INDEX|UNIQUE|CONSTRAINT}`, `TRUNCATE`, `RENAME TABLE`,
  and SQLite's four `ALTER TABLE` operations.
- Any declared column type — MySQL names like `TIMESTAMP`, `JSON`,
  `LONGTEXT` resolve under SQLite's affinity rules.
- MySQL functions where they exist; the shim translates what it can and
  refuses loudly what it cannot — nothing is accepted and silently ignored.
  A clause the shim drops but cannot represent reports a `1618` warning
  naming it.

Known limits worth planning around:

- Foreign keys are recorded but **not enforced** — SQLite's own default.
- A foreign key declared *after* table creation via
  `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY` cannot be recorded
  (declare it inside the initial `CREATE TABLE`).
- `BLOB` writes need the binary protocol (see Java/C# above).

---

## Which direction, when

| Situation | Direction |
| --- | --- |
| Rust service, CLI, embedded app | Embed (Rust) |
| PHP/Python/Ruby/C#/Java wanting SQLite's deployment shape — no server | C ABI (`inlaysql-ffi`) |
| Browser or edge search, offline-capable app | Embed (WASM) |
| Laravel / Rails / Django / Spring / Entity Framework app, minimal glue | MySQL wire — the ORMs already speak it |
| One file serving several processes or machines | MySQL wire |
| Maximum performance, no server in the trust boundary | Embed (Rust or C ABI) |

The same file serves both: a Rust batch job can `Database::open` the file the
MySQL server is serving (use one writer at a time per the MVCC rules in
[`recovery.md`](recovery.md)), and the WASM build opens files the native
build wrote, byte for byte.
