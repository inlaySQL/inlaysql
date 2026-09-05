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

1. **Download** the library for your platform from the
   [releases page](https://github.com/inlaySQL/inlaysql/releases)
   (macOS Apple silicon or Linux x86_64 today — see the note on Windows at
   the end), and unpack it. Inside: the shared library, `inlaysql.h`, and a
   working example for your language.
2. **Copy the loader for your language** from the quickstarts below — 15 to
   60 lines, once, then never think about FFI again.
3. **Use SQL.** Same dialect as SQLite (vectors and retrieval functions are
   the additions); parameters bound as you would expect; results as plain
   data.

That is the whole integration. What follows is the per-language detail.

---

## PHP — quickstart

PHP 7.4+ has FFI built in (enable `ffi=on` in php.ini, or it is always on in
the CLI). Save this next to the unpacked library and run it:

```php
<?php
// inlaysql.php — the whole binding. Copy this file into your project.
// Library path: pass it in, or hardcode the unpacked .dylib/.so path.
define('INLAYSQL_LIB', $argv[1] ?? './libinlaysql_ffi.dylib');

final class InlaySQL
{
    private \FFI $ffi;
    /** @var \FFI\CData */
    private $handle;

    public function __construct(string $path)
    {
        $this->ffi = \FFI::cdef(<<<'C'
            typedef struct InlaysqlHandle InlaysqlHandle;
            InlaysqlHandle *inlaysql_open(const char *path);
            InlaysqlHandle *inlaysql_open_read_only(const char *path);
            void inlaysql_close(InlaysqlHandle *handle);
            int inlaysql_exec(InlaysqlHandle *handle, const char *sql,
                              const char *params, char **out_json);
            const char *inlaysql_last_error(void);
            void inlaysql_free_string(char *s);
            const char *inlaysql_version(void);
        C, INLAYSQL_LIB);

        $this->handle = $this->ffi->inlaysql_open($path);
        if (\FFI::isNull($this->handle)) {
            throw new RuntimeException('open failed: ' . $this->ffi->inlaysql_last_error());
        }
    }

    public function __destruct() { $this->ffi->inlaysql_close($this->handle); }

    /** Run one statement. Rows come back as objects keyed by column name. */
    public function run(string $sql, array $params = []): array
    {
        $out = $this->ffi->new('char *');
        $code = $this->ffi->inlaysql_exec(
            $this->handle, $sql,
            $params === [] ? null : json_encode($params, JSON_UNESCAPED_SLASHES),
            \FFI::addr($out),
        );
        if ($code !== 0) {
            throw new RuntimeException($this->ffi->inlaysql_last_error() . " — $sql");
        }
        $result = json_decode(\FFI::string($out), true, flags: JSON_THROW_ON_ERROR);
        $this->ffi->inlaysql_free_string($out);
        return $result;
    }
}

// ---- usage -------------------------------------------------------------
$db = new InlaySQL('app.inlay');
$db->run('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)');
$db->run('INSERT INTO users (name, email) VALUES (?, ?)', ['Ada', 'ada@example.org']);

$result = $db->run('SELECT id, name, email FROM users WHERE id = ?', [1]);
print_r($result['rows']);   // [[1, "Ada", "ada@example.org"]]
```

A complete runnable script is in the release archive (`poc.php`) and at
[`crates/inlaysql-ffi/examples/poc.php`](../crates/inlaysql-ffi/examples/poc.php) —
it also shows the error path. **Laravel:** the same database also works
through Eloquent over the MySQL wire (direction two below) — a stock
Laravel 11 skeleton migrates and serves against it.

## Python — quickstart

Standard library only. Save this as `inlaysql.py` next to the unpacked
library:

```python
# inlaysql.py — the whole binding. Copy this file into your project.
import ctypes, json

class InlaySQL:
    def __init__(self, lib_path, db_path):
        self.lib = ctypes.CDLL(lib_path)
        lib = self.lib
        lib.inlaysql_open.argtypes = [ctypes.c_char_p];  lib.inlaysql_open.restype = ctypes.c_void_p
        lib.inlaysql_exec.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                      ctypes.c_char_p, ctypes.POINTER(ctypes.c_char_p)]
        lib.inlaysql_exec.restype = ctypes.c_int
        lib.inlaysql_last_error.argtypes = []; lib.inlaysql_last_error.restype = ctypes.c_char_p
        lib.inlaysql_free_string.argtypes = [ctypes.c_char_p]; lib.inlaysql_free_string.restype = None
        lib.inlaysql_close.argtypes = [ctypes.c_void_p]; lib.inlaysql_close.restype = None

        self.handle = lib.inlaysql_open(db_path.encode())
        if not self.handle:
            raise RuntimeError(f"open failed: {lib.inlaysql_last_error().decode()}")

    def run(self, sql, params=None):
        out = ctypes.c_char_p()
        code = self.lib.inlaysql_exec(
            self.handle, sql.encode(),
            json.dumps(params).encode() if params is not None else None,
            ctypes.byref(out))
        if code != 0:
            raise RuntimeError(f"{self.lib.inlaysql_last_error().decode()} — {sql}")
        result = json.loads(out.value)
        self.lib.inlaysql_free_string(out)
        return result

    def close(self):
        self.lib.inlaysql_close(self.handle)

# ---- usage -------------------------------------------------------------
db = InlaySQL('./libinlaysql_ffi.so', 'app.inlay')
db.run('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)')
db.run('INSERT INTO users (name, email) VALUES (?, ?)', ['Ada', 'ada@example.org'])

result = db.run('SELECT id, name, email FROM users WHERE id = ?', [1])
print(result['rows'])   # [[1, 'Ada', 'ada@example.org']]
```

A complete runnable script ships as
[`poc.py`](../crates/inlaysql-ffi/examples/poc.py). SQLAlchemy's `sqlite`
dialect will **not** open this file — the format is InlaySQL's own — but a
thin `InlaySQL.run()` wrapper covers most of what an ORM is doing, and the
MySQL-wire direction below is the full-ORM path.

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

## What crosses the boundary

The result of every statement is JSON in one of three shapes, identical
across all of InlaySQL's foreign surfaces (WASM, MySQL wire, FFI):

```json
{"kind":"ddl"}                                    // schema changed
{"kind":"written","rows":1}                       // one row written
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
