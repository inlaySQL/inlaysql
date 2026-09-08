# Using InlaySQL like SQLite — from PHP, Python, Ruby, Node.js, C#, Java, and Rust

InlaySQL keeps SQLite's deal: **one file, no server, you open it directly in
your process.** From any language that can call C functions — which PHP,
Python, Ruby, C# and Java all can, through their built-in FFI — using it is
a download, one small loader, and SQL you already know.

```php
$handle = $ffi->inlaysql_open('app.inlay');          // the file; created if absent
$rows    = exec_sql($ffi, $handle, 'SELECT ...');    // plain SQL, JSON back
```

This document leads with that path. The MySQL-wire server (direction two,
below) is there for when you want a server process — ORMs, several machines,
one file — but you do not need it to get started.

## The 5-minute version

One command puts the engine library for this machine and the client file
for your language in the current directory, checksums verified:

```sh
curl -fsSL https://github.com/inlaySQL/inlaysql/releases/latest/download/get-inlaysql.sh | sh -s -- php
#                                                                                        python | ruby | csharp | java
```

`--dir vendor/inlaysql` puts them elsewhere; `VERSION=v0.0.5` pins a
release; `--dry-run` only prints the URLs. Or take the two files by hand
from the [releases page](https://github.com/inlaySQL/inlaysql/releases) —
every release attaches each of them on its own, beside a `.sha256`, and
`releases/latest/download/<file>` always names the newest:

| file | what |
| --- | --- |
| `libinlaysql_ffi-aarch64-apple-darwin.dylib`, `libinlaysql_ffi-x86_64-unknown-linux-gnu.so` | the engine (macOS Apple silicon, Linux x86_64 — see the note on Windows at the end) |
| `inlaysql.php`, `inlaysql.py`, `inlaysql.rb`, `InlaySQL.cs`, `InlaySQL.java` | the whole client for that language, one file |
| `inlaysql-ffi-<version>-<target>.tar.gz` | the same things in one archive, with `inlaysql.h` |

Then **use SQL**: same dialect as SQLite (vectors and retrieval functions
are the additions), parameters bound as you would expect, results as plain
data. The client looks for the library beside itself, so the two files stay
together and nothing is configured.

That is the whole integration. What follows is the per-language detail.

---

## PHP — quickstart

PHP 8.1+ has FFI built in (`ffi.enable=true` for FPM; it is on in the CLI).
Copy `wrappers/inlaysql.php` from the release archive (or
[`crates/inlaysql-ffi/wrappers/inlaysql.php`](../crates/inlaysql-ffi/wrappers/inlaysql.php))
next to the unpacked library and `require` it. That file is the whole
client — no Composer package, no extension to build.

```php
<?php
require 'inlaysql.php';

$db = InlaySQL::open('app.inlay');                          // creates if absent
$db->execute('CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY, name TEXT, email TEXT UNIQUE)');

$id = $db->insert('users', ['name' => 'Ada', 'email' => 'ada@example.org']);   // 1

foreach ($db->query('SELECT id, name FROM users WHERE id > :after', ['after' => 0]) as $user) {
    echo $user['name'];                                      // rows as arrays
}
$ada   = $db->first('SELECT * FROM users WHERE id = ?', [$id]);   // one row, or null
$count = $db->value('SELECT COUNT(*) FROM users');                // one cell
$names = $db->column('SELECT name FROM users ORDER BY name');    // one column

$result = $db->execute('UPDATE users SET name = ? WHERE id = ?', ['Ada L.', $id]);
$result->rowsAffected;                                       // 1
$result->lastInsertId;                                       // the last INSERT's id, as SQLite

$db->transaction(function (InlaySQL $db) {                  // BEGIN … COMMIT; ROLLBACK
    $db->insert('users', ['name' => 'Grace', 'email' => 'g@x']);   // on throw; rerun on a
    $db->insert('users', ['name' => 'Linus', 'email' => 'l@x']);   // write conflict
});

try {
    $db->insert('users', ['name' => 'Ada', 'email' => 'ada@example.org']);
} catch (InlaySQLConstraintException $e) {                   // UNIQUE refused it
    echo $e->getMessage();                                   // the engine's own words
}
```

What the class gives you, in one table:

| Call | Returns |
| --- | --- |
| `InlaySQL::open($path, readonly: false, lib: null)` | a handle; `readonly: true` refuses writes and needs the file to exist |
| `execute($sql, $params)` | `InlaySQLResult` — `rowsAffected`, `lastInsertId`, `isDdl`, `rows` (set only for a query) |
| `query($sql, $params)` | `InlaySQLRows` — iterate as arrays; `count()`, `->all()`, `->objects()`, `->first()`, `->value()`, `->column($nameOrIndex)`, `->raw()` |
| `first` / `value` / `column` | the same three shapes without the object |
| `insert($table, ['col' => $v, …])` | the new row id |
| `transaction(fn, retries: 3)` | `fn`'s return; nested calls join the outer transaction; a write conflict rolls back and reruns `fn` |
| `run($sql, $params)` | the raw JSON shape, decoded — what everything above is built on |

Parameters are positional `?` with a list, or `:name` with a string-keyed
array (the rewrite skips string literals, so `':not_a_param'` is left
alone). A PHP array of numbers binds as a vector. Exceptions:
`InlaySQLConstraintException`, `InlaySQLConflictException` (another
handle committed first — safe to retry, and `transaction()` does),
`InlaySQLUnsupportedException` (a clause the engine refuses rather than
silently ignores), and `InlaySQLException` for the rest.

Under PHP-FPM every worker opens its own handle and every one of them may
write — concurrent commits to one file are the thing this engine does that
SQLite does not. The FFI definitions bind once per process; with opcache
preloading (`ffi.enable=preload`) preload `inlaysql.php`.

A minimal script that shows the raw ABI without the class is
[`crates/inlaysql-ffi/examples/poc.php`](../crates/inlaysql-ffi/examples/poc.php).
**Laravel:** the class is what a `DB::connection('inlaysql')` driver would
call, and that driver is queued (`PLAN.md`); today, Eloquent and migrations
run over the MySQL wire (direction two below) — a stock Laravel 11 skeleton
migrates and serves against it — while the class serves raw SQL in-process.

## Python — quickstart

Standard library only. Copy `wrappers/inlaysql.py` from the release archive
(or [`crates/inlaysql-ffi/wrappers/inlaysql.py`](../crates/inlaysql-ffi/wrappers/inlaysql.py))
next to the unpacked library. Same surface as the PHP class, spelt in Python:

```python
from inlaysql import connect, ConstraintError

db = connect("app.inlay")                                    # creates if absent
db.execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT UNIQUE)")

user_id = db.insert("users", name="Ada", email="ada@example.org")      # 1

for user in db.query("SELECT id, name FROM users WHERE id > :after", {"after": 0}):
    print(user["name"])                                      # rows as dicts
ada = db.first("SELECT * FROM users WHERE id = ?", [user_id])   # one row, or None
count = db.value("SELECT COUNT(*) FROM users")                  # one cell
names = db.column("SELECT name FROM users ORDER BY name")       # one column

result = db.execute("UPDATE users SET name = ? WHERE id = ?", ["Ada L.", user_id])
result.rows_affected, result.last_insert_id                  # 1, 1

with db.transaction():                                       # BEGIN … COMMIT; ROLLBACK on raise
    db.insert("users", name="Grace", email="g@x")
    db.insert("users", name="Linus", email="l@x")

db.transact(lambda db: db.insert("users", name="Ken", email="k@x"))   # same, rerun on a write conflict

try:
    db.insert("users", name="Ada", email="ada@example.org")
except ConstraintError as e:
    print(e)                                                 # the engine's own words
```

`connect(path, readonly=False, lib=None)` returns a context manager.
`execute` → `Result(rows_affected, last_insert_id, is_ddl, rows)`; `query`
→ `Rows` (iterate as dicts, `len()`, `.all()`, `.first()`, `.value()`,
`.column(name_or_index)`, `.raw()`); `insert(table, mapping | **values)` →
row id; `transaction()` is a context manager and `transact(fn, retries=3)`
its retrying form; `run` is the raw JSON. Errors: `ConstraintError`,
`ConflictError`, `UnsupportedError`, `InlaySQLError`. Run the file itself
(`python inlaysql.py [path/to/lib]`) for its self-test.

A minimal script that shows the raw ABI without the class is
[`poc.py`](../crates/inlaysql-ffi/examples/poc.py). SQLAlchemy's `sqlite`
dialect will **not** open this file — the format is InlaySQL's own — and the
MySQL-wire direction below is the full-ORM path.

---

## Ruby — quickstart

One gem: `gem install ffi`. Save this as `inlaysql.rb` next to the unpacked
library:

```ruby
# inlaysql.rb — the whole binding. Copy this file into your project.
require 'ffi'
require 'json'

class InlaySQL
  INLAYSQL_OK = 0
  INLAYSQL_ERR_BAD_HANDLE = 2

  module Native
    extend FFI::Library
    ffi_lib File.expand_path('./libinlaysql_ffi.dylib', __dir__)  # .so on Linux
    attach_function :inlaysql_open, [:string], :pointer
    attach_function :inlaysql_close, [:pointer], :void
    attach_function :inlaysql_exec, [:pointer, :string, :string, :pointer], :int
    attach_function :inlaysql_last_error, [], :string
    attach_function :inlaysql_free_string, [:pointer], :void
  end

  def initialize(db_path)
    @handle = Native.inlaysql_open(db_path)
    raise "open failed: #{Native.inlaysql_last_error}" if @handle.null?
  end

  def run(sql, params = nil)
    out = FFI::MemoryPointer.new(:pointer)
    code = Native.inlaysql_exec(@handle, sql, params && JSON.generate(params), out)
    case code
    when INLAYSQL_OK
      result = JSON.parse(out.read_pointer.read_string)
      Native.inlaysql_free_string(out.read_pointer)
      result
    when INLAYSQL_ERR_BAD_HANDLE then raise 'bad handle'
    else raise "#{Native.inlaysql_last_error} — #{sql}"
    end
  end

  def close = Native.inlaysql_close(@handle)
end

# ---- usage -------------------------------------------------------------
db = InlaySQL.new('app.inlay')
db.run('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)')
db.run('INSERT INTO users (name, email) VALUES (?, ?)', ['Ada', 'ada@example.org'])

result = db.run('SELECT id, name, email FROM users WHERE id = ?', [1])
p result['rows']   # [[1, "Ada", "ada@example.org"]]
```

A complete runnable script ships as
[`poc.rb`](../crates/inlaysql-ffi/examples/poc.rb).

## C# / .NET — quickstart

`DllImport` over the same seven functions; the JSON result comes back as a
string you decode with `System.Text.Json`. The full program:

```csharp
using System.Runtime.InteropServices;
using System.Text.Json;

class InlaySQL : IDisposable
{
    const string LIB = "libinlaysql_ffi.so";   // "libinlaysql_ffi.dylib" on macOS

    [DllImport(LIB)] static extern IntPtr inlaysql_open(string path);
    [DllImport(LIB)] static extern void inlaysql_close(IntPtr handle);
    [DllImport(LIB)] static extern int inlaysql_exec(IntPtr handle, string sql,
        string? parameters, out IntPtr outJson);
    [DllImport(LIB)] static extern IntPtr inlaysql_last_error();
    [DllImport(LIB)] static extern void inlaysql_free_string(IntPtr ptr);

    readonly IntPtr _handle;
    public InlaySQL(string path) =>
        (_handle = inlaysql_open(path)) != IntPtr.Zero
            ? true : throw new Exception(Marshal.PtrToStringAnsi(inlaysql_last_error()));

    public JsonElement Run(string sql, object?[]? parameters = null)
    {
        if (inlaysql_exec(_handle, sql,
                parameters is null ? null : JsonSerializer.Serialize(parameters),
                out var outJson) != 0)
            throw new Exception($"{Marshal.PtrToStringAnsi(inlaysql_last_error())} — {sql}");
        using var doc = JsonDocument.Parse(Marshal.PtrToStringUTF8(outJson)!);
        var result = doc.RootElement.Clone();
        inlaysql_free_string(outJson);
        return result;
    }

    public void Dispose() => inlaysql_close(_handle);
}

// ---- usage -------------------------------------------------------------
using var db = new InlaySQL("app.inlay");
db.Run("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)");
db.Run("INSERT INTO users (name, email) VALUES (?, ?)", new object?[] { "Ada", "ada@example.org" });

var rows = db.Run("SELECT id, name, email FROM users WHERE id = ?", new object?[] { 1 });
Console.WriteLine(rows.GetProperty("rows"));   // [[1,"Ada","ada@example.org"]]
```

**Entity Framework** over this file is future work; the MySQL-wire direction
below (Pomelo provider against `inlaysql serve --mysql`) is the EF path
today.

## Java — quickstart

Java 22+ has the Foreign Function & Memory API (`java.lang.foreign`) in the
JDK — no JNI C compilation:

```java
// The whole binding. Java 22+, standard JDK.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;

public final class InlaySQL implements AutoCloseable {
    static final Linker LINKER = Linker.nativeLinker();
    static final SymbolLookup LIB = SymbolLookup.libraryLookup("libinlaysql_ffi.so",
        Arena.global());                                   // .dylib on macOS
    static final Arena ARENA = Arena.ofShared();

    static final MethodHandle OPEN = linkerDowncall("inlaysql_open",
        FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    static final MethodHandle EXEC = linkerDowncall("inlaysql_exec",
        FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS,
            ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    static final MethodHandle LAST_ERROR = linkerDowncall("inlaysql_last_error",
        FunctionDescriptor.of(ValueLayout.ADDRESS));
    static final MethodHandle FREE = linkerDowncall("inlaysql_free_string",
        FunctionDescriptor.ofVoid(ValueLayout.ADDRESS));

    final MemorySegment handle;

    static MethodHandle linkerDowncall(String name, FunctionDescriptor d) {
        return LINKER.downcallHandle(LIB.find(name).orElseThrow(), d);
    }

    public InlaySQL(String path) throws Throwable {
        var cpath = ARENA.allocateUtf8(path);
        handle = (MemorySegment) OPEN.invoke(cpath);
        if (handle.address() == 0)
            throw new IllegalStateException((String) LAST_ERROR.invoke());
    }

    /** Run one statement; params is a Java array marshalled to JSON. */
    public String run(String sql, Object... params) throws Throwable {
        var csql = ARENA.allocateUtf8(sql);
        var cparams = params.length == 0 ? MemorySegment.NULL
            : ARENA.allocateUtf8(new com.google.gson.Gson().toJson(params));
        var out = ARENA.allocate(ValueLayout.ADDRESS);
        int code = (int) EXEC.invoke(handle, csql, cparams, out);
        if (code != 0)
            throw new IllegalStateException((String) LAST_ERROR.invoke() + " — " + sql);
        var json = out.get(ValueLayout.ADDRESS, 0);
        var text = json.getUtf8String(0);
        FREE.invoke(json);
        return text;
    }

    @Override public void close() throws Throwable {
        LINKER.downcallHandle(LIB.find("inlaysql_close").orElseThrow(),
            FunctionDescriptor.ofVoid(ValueLayout.ADDRESS)).invoke(handle);
    }

    public static void main(String[] args) throws Throwable {
        try (var db = new InlaySQL("app.inlay")) {
            db.run("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)");
            db.run("INSERT INTO users (name, email) VALUES (?, ?)", "Ada", "ada@example.org");
            System.out.println(db.run("SELECT id, name, email FROM users WHERE id = ?", 1));
            // {"columns":["id","name","email"],"rows":[[1,"Ada","ada@example.org"]]}
        }
    }
}
```

The JSON-marshalling line is the only piece a real binding replaces (with
Jackson, for instance). Older JDKs use JNI over the same header.

## The client surface, for every language

The PHP and Python files above implement one surface, and the Ruby, C# and
Java files are being brought up to it (they carry `run`/`query`/`first`
today). A client is complete when it has, in the language's own idiom:

| Piece | Contract |
| --- | --- |
| `open(path, readonly, lib)` | creates the file unless read-only; the library is located beside the client file, its parent, then the working directory, and loaded once per process |
| `execute(sql, params) → Result` | `rows_affected`, `last_insert_id` (SQLite's `last_insert_rowid()` contract), `is_ddl`, and `rows` when the statement was a query |
| `query(sql, params) → Rows` | iterable rows keyed by column name; `count`, `all`, `first`, `value` (first cell), `column(name or index)`, `raw` (positional, as the ABI sent it) |
| `first` / `value` / `column` | the three common shapes as one call |
| `insert(table, row) → id` | column names quoted; the row id from the result |
| `transaction(fn)` | `BEGIN` / `COMMIT` / `ROLLBACK`; nested calls join; a write conflict rolls back and reruns `fn` up to a retry count |
| parameters | positional `?` with a list; named `:name` with a map, rewritten outside string literals and quoted identifiers |
| errors | one base exception, plus `Constraint`, `Conflict`, `Unsupported` subclasses chosen by the engine message's prefix (`constraint failed`, `write conflict`, `unsupported`) |
| `run(sql, params)` | the raw JSON, decoded — kept public so nothing above is a ceiling |

One rule the surface keeps: `query()` on a statement that is not a query
**runs it** and then complains — the ABI has one entry point and the client
cannot know the shape before the engine answers. Use `execute()` when the
statement may write.

## What crosses the boundary

The result of every statement is JSON in one of three shapes, identical
across all of InlaySQL's foreign surfaces (WASM, MySQL wire, FFI):

```json
{"kind":"ddl"}                                    // schema changed
{"kind":"written","rows":1,"last_insert_id":1}    // one row written; id as SQLite's last_insert_rowid()
{"columns":["id","name"],"rows":[[1,"Ada"]]}      // a SELECT
```

| Passing in (`params`) | Meaning |
| --- | --- |
| `["Ada", 1, null, 2.5]` | bound to `?` in order |
| `[[0.1, 0.2, 0.3]]` | a **vector** — bind to a `VECTOR(n)` comparison or retrieval function |
| omitted / `null` | no parameters |

Errors: `inlaysql_exec` returns non-zero and `inlaysql_last_error()` holds
the engine's message verbatim. There are no numeric error codes to learn.

Two things the boundary does not do, stated so nobody discovers them in a
debugger: a **vector** value comes back in `rows` as the placeholder
`"<vector(n)>"` (ask for what you need in SQL; the raw floats do not cross
in JSON), and **one handle is one thread at a time** — open one handle per
thread for concurrent access, which is also SQLite's model.

---

## Direction two: the MySQL wire — when you want a server

The same file can be served instead of opened. `inlaysql serve --mysql`
speaks MySQL's wire protocol, so every ORM that already talks MySQL works
with no new driver — this is the path for Laravel, Rails, Django, Spring and
Entity Framework today, and for one file shared by several processes.

```sh
inlaysql serve --mysql app.inlay --password-env INLAYSQL_PASSWORD

# Beyond localhost, these are not advice — a --bind that reaches another
# machine is refused without them:
inlaysql user add app.inlay --user app --password-env INLAYSQL_PASSWORD --superuser
inlaysql serve --mysql app.inlay --bind 10.0.1.14 \
  --tls-cert server.pem --tls-key key.pem --tls-required
```

Then it is your framework's normal database configuration:

| Language | Connect with |
| --- | --- |
| **PHP (Laravel/Eloquent)** | `.env`: `DB_CONNECTION=mysql`, host `127.0.0.1` — a stock Laravel 11 skeleton migrates and serves against it |
| **Python (Django/SQLAlchemy)** | the `mysql` backend, `pymysql` driver |
| **Ruby (Rails)** | `database.yml`, the `mysql2` adapter |
| **C# (.NET)** | `MySqlConnector` / Pomelo EF provider |
| **Java (Spring/Hibernate)** | `jdbc:mysql://127.0.0.1:3306/app` |
| **Node.js** | `mysql2` |

Full detail — security posture, what works, the honest gaps — is in
[`server.md`](server.md). The connection is plaintext until you give it a
certificate, and the server will not let you skip that quietly: binding
anywhere that reaches another machine is refused unless the database has
accounts of its own, the bootstrap password is not empty, and `--tls-cert` plus
`--tls-required` are given. On a private segment where you accept plaintext,
`--plaintext-network` says so — and the server checks the address really is
private before believing you.

## Which shape, when

| You want | Take |
| --- | --- |
| Zero servers, zero gems, the file is yours | **C ABI** (this page's quickstarts) |
| Your ORM's migrations and models, today | **MySQL wire** |
| A browser tab, offline app, edge worker | **WASM** — the [inlaysql-js SDK](https://github.com/inlaySQL/inlaysql-js) |
| A Rust service or CLI | the `inlaysql` crate directly |

The C-ABI and MySQL shapes share the same file safely (the engine holds an
OS advisory lock, so one writer process at a time; readers can open
`inlaysql_open_read_only` alongside). See [`recovery.md`](recovery.md).

## Notes and limits

- **Not a SQLite file.** The dialect is SQLite's; the bytes are InlaySQL's —
  that is what makes the native vector/BM25 indexes possible. A `sqlite3`
  driver cannot open it.
- **No Windows library yet.** The file layer is Unix-only today; the WASM
  module runs anywhere a browser or Node does, Windows included.
- **Pre-1.0 format.** A database written by this version may not open in a
  later one — recreate, not migrate.
- `:memory:` is refused by name (`inlaysql_open` returns NULL and says so);
  use a real path, or the Rust crate's `Database::open_in_memory()`.
