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

`--dir vendor/inlaysql` puts them elsewhere; `VERSION=v0.0.6` pins a
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

One gem: `gem install ffi`. Copy `inlaysql.rb` from the release (or
[`crates/inlaysql-ffi/wrappers/inlaysql.rb`](../crates/inlaysql-ffi/wrappers/inlaysql.rb))
next to the library. Same surface as the PHP and Python clients:

```ruby
require 'inlaysql'

InlaySQL.connect('app.inlay') do |db|                          # creates if absent; closed at block end
  db.execute 'CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT UNIQUE)'

  id = db.insert('users', name: 'Ada', email: 'ada@example.org')      # 1

  db.query('SELECT id, name FROM users WHERE id > :after', after: 0).each do |user|
    puts user['name']                                            # rows as hashes
  end
  ada   = db.first('SELECT * FROM users WHERE id = ?', [id])     # one row, or nil
  count = db.value('SELECT COUNT(*) FROM users')                 # one cell
  names = db.column('SELECT name FROM users ORDER BY name')     # one column

  result = db.execute('UPDATE users SET name = ? WHERE id = ?', ['Ada L.', id])
  result.rows_affected                                           # 1

  db.transaction do |tx|                                         # BEGIN … COMMIT; ROLLBACK on raise;
    tx.insert('users', name: 'Grace', email: 'g@x')              # rerun on a write conflict
    tx.insert('users', name: 'Linus', email: 'l@x')
  end
rescue InlaySQL::ConstraintError => e
  puts e.message                                                 # the engine's own words
end
```

`execute` → `Result` (`rows_affected`, `last_insert_id`, `ddl?`, `rows`);
`query` → `Rows` (Enumerable of hashes, `size`, `all`, `first`, `value`,
`column(name_or_index)`, `raw`); `insert(table, hash | **kw)` → row id;
`transaction(retries: 3)`; `run` is the raw JSON. Named parameters are a
hash or keyword arguments. Errors: `InlaySQL::ConstraintError`,
`ConflictError`, `UnsupportedError`, `Error`. `ruby inlaysql.rb
[path/to/lib]` runs its self-test.

## C# / .NET — quickstart

.NET 8+, nothing beyond the BCL. Copy `InlaySQL.cs` from the release (or
[`crates/inlaysql-ffi/wrappers/InlaySQL.cs`](../crates/inlaysql-ffi/wrappers/InlaySQL.cs))
into your project and drop the library beside the application (or set
`InlaySQL.LibraryPath` / `INLAYSQL_LIB`):

```csharp
using var db = InlaySQL.Open("app.inlay");                       // creates if absent
db.Execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT UNIQUE)");

long id = db.Insert("users", new Dictionary<string, object?> { ["name"] = "Ada", ["email"] = "ada@example.org" });

foreach (var user in db.Query("SELECT id, name FROM users WHERE id > :after", new Dictionary<string, object?> { ["after"] = 0 }))
    Console.WriteLine(user["name"]);                             // rows as dictionaries
var ada   = db.First("SELECT * FROM users WHERE id = ?", id);    // one row, or null
var count = db.Value("SELECT COUNT(*) FROM users");              // one cell (long)
var names = db.Column("SELECT name FROM users ORDER BY name");  // one column

var result = db.Execute("UPDATE users SET name = ? WHERE id = ?", "Ada L.", id);
result.RowsAffected;                                             // 1

db.Transaction(tx => {                                           // BEGIN … COMMIT; ROLLBACK on throw;
    tx.Insert("users", new Dictionary<string, object?> { ["name"] = "Grace", ["email"] = "g@x" });   // rerun on a write conflict
});

try { db.Insert("users", new Dictionary<string, object?> { ["name"] = "Ada", ["email"] = "ada@example.org" }); }
catch (InlaySQLConstraintException e) { Console.WriteLine(e.Message); }
```

`Execute` → `InlaySQLResult` (`RowsAffected`, `LastInsertId`, `IsDdl`,
`Rows`); `Query` → `InlaySQLRows` (enumerable of dictionaries, `Count`,
`All()`, `First()`, `Value()`, `Column(nameOrIndex)`, `Raw`); `Insert` →
row id; `Transaction(Func|Action, retries)`; `Run` is the raw
`JsonElement`. Integers come back as `long`, reals as `double`. Errors:
`InlaySQLConstraintException`, `InlaySQLConflictException`,
`InlaySQLUnsupportedException`, `InlaySQLException`. `InlaySQL.SelfTest.Run()`
from a console `Main` runs the same checks the other clients make.

## Java — quickstart

JDK 22+ (the Foreign Function & Memory API is final there), no dependency.
Copy `InlaySQL.java` from the release (or
[`crates/inlaysql-ffi/wrappers/InlaySQL.java`](../crates/inlaysql-ffi/wrappers/InlaySQL.java))
into your project; run with `--enable-native-access=ALL-UNNAMED` (or the
module's name) to silence the FFM warning:

```java
try (InlaySQL db = InlaySQL.open(Path.of("app.inlay"))) {        // creates if absent
    db.execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT UNIQUE)");

    long id = db.insert("users", Map.of("name", "Ada", "email", "ada@example.org"));

    for (Map<String, Object> user : db.query("SELECT id, name FROM users WHERE id > :after", Map.of("after", 0)))
        System.out.println(user.get("name"));                    // rows as ordered maps
    Map<String, Object> ada = db.first("SELECT * FROM users WHERE id = ?", id);   // one row, or null
    Object count = db.value("SELECT COUNT(*) FROM users");                        // one cell (Long)
    List<Object> names = db.column("SELECT name FROM users ORDER BY name");       // one column

    InlaySQL.Result r = db.execute("UPDATE users SET name = ? WHERE id = ?", "Ada L.", id);
    r.rowsAffected();                                            // 1

    db.inTransaction(tx -> {                                     // BEGIN … COMMIT; ROLLBACK on throw;
        tx.insert("users", Map.of("name", "Grace", "email", "g@x"));   // rerun on a write conflict
    });
    long n = db.transaction(tx -> tx.insert("users", Map.of("name", "Linus", "email", "l@x")));
} catch (InlaySQL.ConstraintException e) {
    System.out.println(e.getMessage());
}
```

`execute` → `Result` record (`rowsAffected`, `lastInsertId`, `isDdl`,
`rows`); `query` → `Rows` (`Iterable<Map>`, `size`, `all`, `first`, `value`,
`column(nameOrIndex)`, `raw`); `insert(table, Map)` → row id;
`transaction(Function)` / `inTransaction(Consumer)`; `runRaw` is the raw
JSON text. Named parameters are a `Map`. Integers come back as `Long`,
reals as `Double`. Errors are unchecked: `InlaySQL.ConstraintException`,
`ConflictException`, `UnsupportedException`, `InlaySQLException`. The
library is found through `-Dinlaysql.lib`, `INLAYSQL_LIB`, the working
directory, or the loader's path. `java InlaySQL.java [path/to/lib]` runs its
self-test.

---

## The client surface, for every language

All five files above implement one surface. A client is complete when it
has, in the language's own idiom:

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
