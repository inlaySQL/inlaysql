// InlaySQL — the C# / .NET client over the C ABI.
//
// One file, no NuGet package: copy it into your project and open a database
// like SQLite — no server, the file is yours. Needs nothing but the BCL
// (System.Text.Json is in the box). .NET 8+.
//
//   using var db = InlaySQL.Open("app.inlay");                        // creates if absent
//   db.Execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)");
//
//   long id = db.Insert("users", new() { ["name"] = "Ada", ["email"] = "ada@example.org" });
//
//   foreach (var user in db.Query("SELECT id, name FROM users WHERE id > :after", new() { ["after"] = 0 }))
//       Console.WriteLine(user["name"]);                               // rows as dictionaries
//   var ada   = db.First("SELECT * FROM users WHERE id = ?", id);      // one row, or null
//   var count = db.Value("SELECT COUNT(*) FROM users");                // one cell
//   var names = db.Column("SELECT name FROM users ORDER BY name");    // one column
//
//   var result = db.Execute("UPDATE users SET name = ? WHERE id = ?", "Ada L.", id);
//   result.RowsAffected;                                               // 1
//
//   db.Transaction(tx => {                                             // BEGIN … COMMIT; ROLLBACK
//       tx.Insert("users", new() { ["name"] = "Grace" });               // on throw; rerun on a
//       tx.Insert("users", new() { ["name"] = "Linus" });               // write conflict
//   });
//
// Parameters: positional `?` as params, or named `:name` with a dictionary.
// A double[] or float[] binds as a vector, so a retrieval call is
//
//   db.Query("SELECT id, vector_score(embedding, ?) AS s FROM docs ORDER BY s LIMIT 10", embedding);
//
// Errors: InlaySQLConstraintException (unique/not-null…),
// InlaySQLConflictException (another writer committed first — retry),
// InlaySQLUnsupportedException (a clause the engine refuses rather than
// ignores), and InlaySQLException for the rest; the engine's own message is
// the exception message.
//
// Read-only: InlaySQL.Open("app.inlay", readOnly: true) — the file must
// already exist and every write is refused.
//
// One handle is one thread at a time (open one per thread), and every one
// of them may write — concurrent commits to one file are what this engine
// does that SQLite does not. The library is resolved once per process:
// InlaySQL.LibraryPath (set before the first call), then INLAYSQL_LIB, then
// libinlaysql_ffi.dylib/.so beside the application, then the loader's own
// search. Vector cells come back as the placeholder "<vector(n)>" — the raw
// floats do not cross the boundary in JSON.
//
// Self-test: from a console project's Main, `InlaySQL.SelfTest.Run(lib)`;
// it makes the same checks the other clients' self-tests make. Run on
// .NET 10 against libinlaysql_ffi from inlaySQL/inlaysql v0.0.6, outside CI
// (there is no .NET runtime in this repository's CI yet); the C surface it
// wraps is documented in include/inlaysql.h beside this file.

using System.Collections;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

#nullable enable

/// <summary>Thrown with the engine's own message, verbatim — there are no numeric codes.</summary>
public class InlaySQLException : Exception
{
    public InlaySQLException(string message) : base(message) { }
}

/// <summary>UNIQUE, NOT NULL, CHECK, FOREIGN KEY — the row was refused.</summary>
public sealed class InlaySQLConstraintException : InlaySQLException
{
    public InlaySQLConstraintException(string message) : base(message) { }
}

/// <summary>Another handle committed first; nothing was written. Safe to retry.</summary>
public sealed class InlaySQLConflictException : InlaySQLException
{
    public InlaySQLConflictException(string message) : base(message) { }
}

/// <summary>A clause this engine refuses rather than silently ignores.</summary>
public sealed class InlaySQLUnsupportedException : InlaySQLException
{
    public InlaySQLUnsupportedException(string message) : base(message) { }
}

/// <summary>What a statement did. <see cref="Rows"/> is set only when it was a query.</summary>
public sealed record InlaySQLResult(
    long RowsAffected,
    /// <summary>The most recent INSERT's row id on this handle (SQLite's last_insert_rowid() contract), or null.</summary>
    long? LastInsertId,
    bool IsDdl,
    InlaySQLRows? Rows);

/// <summary>
/// A query's answer. Enumerates as dictionaries keyed by column name;
/// <see cref="Count"/> is the row count; the accessors answer the common
/// shapes without a loop.
/// </summary>
public sealed class InlaySQLRows : IReadOnlyCollection<Dictionary<string, object?>>
{
    public IReadOnlyList<string> Columns { get; }
    private readonly List<List<object?>> _rows;

    internal InlaySQLRows(List<string> columns, List<List<object?>> rows)
    {
        Columns = columns;
        _rows = rows;
    }

    public int Count => _rows.Count;

    public IEnumerator<Dictionary<string, object?>> GetEnumerator()
    {
        foreach (var row in _rows) yield return ToDictionary(row);
    }

    IEnumerator IEnumerable.GetEnumerator() => GetEnumerator();

    /// <summary>Every row as a dictionary.</summary>
    public List<Dictionary<string, object?>> All() => this.ToList();

    /// <summary>The rows exactly as the ABI returned them, positional.</summary>
    public IReadOnlyList<IReadOnlyList<object?>> Raw => _rows;

    /// <summary>First row as a dictionary, or null.</summary>
    public Dictionary<string, object?>? First() => _rows.Count == 0 ? null : ToDictionary(_rows[0]);

    /// <summary>The first cell of the first row, or null when there is no row.</summary>
    public object? Value() => _rows.Count == 0 || _rows[0].Count == 0 ? null : _rows[0][0];

    /// <summary>One column, by index, across every row.</summary>
    public List<object?> Column(int index)
    {
        if (index < 0 || index >= Columns.Count) throw new InlaySQLException($"no such column: {index}");
        return _rows.Select(row => row[index]).ToList();
    }

    /// <summary>One column, by name, across every row.</summary>
    public List<object?> Column(string name)
    {
        var index = ((List<string>)Columns).IndexOf(name);
        if (index < 0) throw new InlaySQLException($"no such column: {name}");
        return Column(index);
    }

    private Dictionary<string, object?> ToDictionary(List<object?> row)
    {
        var dict = new Dictionary<string, object?>(Columns.Count);
        for (var i = 0; i < Columns.Count; i++) dict[Columns[i]] = row[i];
        return dict;
    }
}

public sealed class InlaySQL : IDisposable
{
    private const int INLAYSQL_OK = 0;
    private const int INLAYSQL_ERR_BAD_HANDLE = 2;

    // ---- the C surface, verbatim from include/inlaysql.h --------------------

    private const string Lib = "libinlaysql_ffi";

    [DllImport(Lib)] private static extern IntPtr inlaysql_open([MarshalAs(UnmanagedType.LPUTF8Str)] string path);
    [DllImport(Lib)] private static extern IntPtr inlaysql_open_read_only([MarshalAs(UnmanagedType.LPUTF8Str)] string path);
    [DllImport(Lib)] private static extern void inlaysql_close(IntPtr handle);
    [DllImport(Lib)] private static extern int inlaysql_exec(IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string sql,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string? parameters,
        out IntPtr outJson);
    [DllImport(Lib)] private static extern IntPtr inlaysql_last_error();
    [DllImport(Lib)] private static extern void inlaysql_free_string(IntPtr ptr);
    [DllImport(Lib)] private static extern IntPtr inlaysql_version();

    /// <summary>
    /// An explicit path to libinlaysql_ffi.dylib/.so. Read at the first
    /// P/Invoke, so set it (or pass <c>lib:</c> to <see cref="Open"/>) before
    /// opening anything. INLAYSQL_LIB in the environment is the second choice,
    /// then the plain name beside the application, then the loader's search.
    /// </summary>
    public static string? LibraryPath { get; set; }

    static InlaySQL()
    {
        NativeLibrary.SetDllImportResolver(typeof(InlaySQL).Assembly, (name, assembly, searchPath) =>
        {
            if (name != Lib) return IntPtr.Zero;
            foreach (var candidate in Candidates())
            {
                if (NativeLibrary.TryLoad(candidate, out var handle)) return handle;
            }
            throw new InlaySQLException(
                $"could not load {PlatformName()} — set InlaySQL.LibraryPath or INLAYSQL_LIB, drop it beside the" +
                " application, or download it from https://github.com/inlaySQL/inlaysql/releases");
        });
    }

    private static string PlatformName() =>
        OperatingSystem.IsMacOS() ? "libinlaysql_ffi.dylib"
        : OperatingSystem.IsWindows() ? "inlaysql_ffi.dll"
        : "libinlaysql_ffi.so";

    private static IEnumerable<string> Candidates()
    {
        if (LibraryPath is not null) yield return LibraryPath;
        var env = Environment.GetEnvironmentVariable("INLAYSQL_LIB");
        if (!string.IsNullOrEmpty(env)) yield return env;
        yield return Path.Combine(AppContext.BaseDirectory, PlatformName());
        yield return Path.Combine(Directory.GetCurrentDirectory(), PlatformName());
        yield return PlatformName();
    }

    // ---- lifecycle -----------------------------------------------------------

    private IntPtr _handle;
    private int _depth;

    private InlaySQL(IntPtr handle) => _handle = handle;

    /// <param name="path">the database file; created when absent</param>
    /// <param name="readOnly">open read-only; the file must already exist</param>
    /// <param name="lib">the library path; sets <see cref="LibraryPath"/> before the first call</param>
    public static InlaySQL Open(string path, bool readOnly = false, string? lib = null)
    {
        if (lib is not null) LibraryPath = lib;
        var handle = readOnly ? inlaysql_open_read_only(path) : inlaysql_open(path);
        if (handle == IntPtr.Zero) throw Error($"open failed: {LastError()}");
        return new InlaySQL(handle);
    }

    public string Version() => Marshal.PtrToStringUTF8(inlaysql_version()) ?? "";

    public void Dispose()
    {
        if (_handle == IntPtr.Zero) return;
        inlaysql_close(_handle);
        _handle = IntPtr.Zero;
    }

    // ---- statements ----------------------------------------------------------

    /// <summary>Run any statement. For a write, the result says how many rows and which id; for a query, prefer <see cref="Query(string, object?[])"/>.</summary>
    public InlaySQLResult Execute(string sql, params object?[] parameters) => ToResult(Run(sql, parameters));

    /// <summary><see cref="Execute(string, object?[])"/> with named <c>:name</c> parameters.</summary>
    public InlaySQLResult Execute(string sql, IDictionary<string, object?> parameters) => ToResult(Run(sql, parameters));

    /// <summary>Run a query. Enumerate the result, or ask it for All, First, Value, Column.</summary>
    public InlaySQLRows Query(string sql, params object?[] parameters)
    {
        var raw = Run(sql, parameters);
        if (!raw.TryGetProperty("columns", out _)) throw Error($"not a query: {sql}");
        return ToRows(raw);
    }

    /// <summary><see cref="Query(string, object?[])"/> with named <c>:name</c> parameters.</summary>
    public InlaySQLRows Query(string sql, IDictionary<string, object?> parameters)
    {
        var raw = Run(sql, parameters);
        if (!raw.TryGetProperty("columns", out _)) throw Error($"not a query: {sql}");
        return ToRows(raw);
    }

    /// <summary>First row as a dictionary, or null.</summary>
    public Dictionary<string, object?>? First(string sql, params object?[] parameters) => Query(sql, parameters).First();
    public Dictionary<string, object?>? First(string sql, IDictionary<string, object?> parameters) => Query(sql, parameters).First();

    /// <summary>The first cell of the first row, or null when there is no row.</summary>
    public object? Value(string sql, params object?[] parameters) => Query(sql, parameters).Value();
    public object? Value(string sql, IDictionary<string, object?> parameters) => Query(sql, parameters).Value();

    /// <summary>The first column across every row.</summary>
    public List<object?> Column(string sql, params object?[] parameters) => Query(sql, parameters).Column(0);
    public List<object?> Column(string sql, IDictionary<string, object?> parameters) => Query(sql, parameters).Column(0);

    /// <summary>Insert one row given as column → value; return its row id.</summary>
    public long Insert(string table, IDictionary<string, object?> row)
    {
        if (row.Count == 0) throw Error($"insert into {table}: no columns given");
        var columns = string.Join(", ", row.Keys.Select(QuoteIdentifier));
        var marks = string.Join(", ", row.Keys.Select(_ => "?"));
        var result = Execute($"INSERT INTO {QuoteIdentifier(table)} ({columns}) VALUES ({marks})", row.Values.ToArray());
        return result.LastInsertId ?? throw Error($"insert into {table} reported no row id");
    }

    // ---- transactions --------------------------------------------------------

    /// <summary>
    /// Run <paramref name="body"/> inside BEGIN … COMMIT and return its value. A
    /// throw rolls back and rethrows. A write conflict (another handle committed
    /// first) rolls back and reruns the body, up to <paramref name="retries"/>
    /// times, so it must be safe to repeat. Nested calls join the outer transaction.
    /// </summary>
    public T Transaction<T>(Func<InlaySQL, T> body, int retries = 3)
    {
        if (_depth > 0)
        {
            _depth++;
            try { return body(this); }
            finally { _depth--; }
        }
        var attempt = 0;
        while (true)
        {
            Run("BEGIN", Array.Empty<object?>());
            _depth = 1;
            try
            {
                var value = body(this);
                Run("COMMIT", Array.Empty<object?>());
                _depth = 0;
                return value;
            }
            catch (Exception e)
            {
                _depth = 0;
                RollbackQuietly();
                if (e is InlaySQLConflictException && attempt++ < retries) continue;
                throw;
            }
        }
    }

    public void Transaction(Action<InlaySQL> body, int retries = 3) =>
        Transaction<object?>(db => { body(db); return null; }, retries);

    public bool InTransaction => _depth > 0;

    private void RollbackQuietly()
    {
        try { Run("ROLLBACK", Array.Empty<object?>()); }
        catch (InlaySQLException) { /* a conflict at COMMIT already ended the transaction */ }
    }

    // ---- the raw call --------------------------------------------------------

    /// <summary>
    /// Run one statement and return the ABI's JSON: {"kind":"ddl"},
    /// {"kind":"written","rows":n,"last_insert_id":k}, or {"columns":[…],"rows":[[…],…]}.
    /// The typed methods are built on this; it stays public for callers that want the raw shape.
    /// </summary>
    public JsonElement Run(string sql, params object?[] parameters)
    {
        if (_handle == IntPtr.Zero) throw Error("the database is closed");
        var json = parameters is null || parameters.Length == 0 ? null : JsonSerializer.Serialize(parameters);
        if (inlaysql_exec(_handle, sql, json, out var outJson) != INLAYSQL_OK)
        {
            throw Error(LastError(), sql);
        }
        try
        {
            using var doc = JsonDocument.Parse(Marshal.PtrToStringUTF8(outJson)!);
            return doc.RootElement.Clone();
        }
        finally
        {
            inlaysql_free_string(outJson);
        }
    }

    public JsonElement Run(string sql, IDictionary<string, object?> parameters)
    {
        if (parameters.Count == 0) return Run(sql, Array.Empty<object?>());
        var (rewritten, values) = BindNamed(sql, parameters);
        return Run(rewritten, values);
    }

    // ---- helpers -------------------------------------------------------------

    /// <summary>Rewrite <c>:name</c> to <c>?</c> in order of appearance, skipping quoted runs.</summary>
    internal static (string, object?[]) BindNamed(string sql, IDictionary<string, object?> parameters)
    {
        var sb = new StringBuilder(sql.Length);
        var values = new List<object?>();
        int i = 0, n = sql.Length;
        while (i < n)
        {
            var c = sql[i];
            if (c is '\'' or '"' or '`')
            {
                var j = i + 1;
                while (j < n)
                {
                    if (sql[j] == c)
                    {
                        if (j + 1 < n && sql[j + 1] == c) { j += 2; continue; }
                        break;
                    }
                    j++;
                }
                sb.Append(sql, i, Math.Min(j + 1, n) - i);
                i = j + 1;
                continue;
            }
            if (c == ':' && i + 1 < n && (char.IsLetter(sql[i + 1]) || sql[i + 1] == '_'))
            {
                var j = i + 1;
                while (j < n && (char.IsLetterOrDigit(sql[j]) || sql[j] == '_')) j++;
                var name = sql.Substring(i + 1, j - i - 1);
                if (!parameters.TryGetValue(name, out var value)) throw Error($"no value bound for :{name}", sql);
                values.Add(value);
                sb.Append('?');
                i = j;
                continue;
            }
            sb.Append(c);
            i++;
        }
        return (sb.ToString(), values.ToArray());
    }

    private static string QuoteIdentifier(string name) => "\"" + name.Replace("\"", "\"\"") + "\"";

    private static InlaySQLResult ToResult(JsonElement raw)
    {
        if (raw.TryGetProperty("columns", out _)) return new InlaySQLResult(0, null, false, ToRows(raw));
        var rows = raw.TryGetProperty("rows", out var r) ? r.GetInt64() : 0;
        long? last = raw.TryGetProperty("last_insert_id", out var l) && l.ValueKind == JsonValueKind.Number ? l.GetInt64() : null;
        var ddl = raw.TryGetProperty("kind", out var k) && k.GetString() == "ddl";
        return new InlaySQLResult(rows, last, ddl, null);
    }

    private static InlaySQLRows ToRows(JsonElement raw)
    {
        var columns = raw.GetProperty("columns").EnumerateArray().Select(c => c.GetString()!).ToList();
        var rows = new List<List<object?>>();
        foreach (var row in raw.GetProperty("rows").EnumerateArray())
        {
            rows.Add(row.EnumerateArray().Select(Cell).ToList());
        }
        return new InlaySQLRows(columns, rows);
    }

    private static object? Cell(JsonElement value) => value.ValueKind switch
    {
        JsonValueKind.Null => null,
        // Cast each arm: without it the conditional unifies long and double to double.
        JsonValueKind.Number => value.TryGetInt64(out var i) ? (object)i : (object)value.GetDouble(),
        JsonValueKind.String => value.GetString(),
        JsonValueKind.True => true,
        JsonValueKind.False => false,
        _ => value.Clone(),
    };

    private static string LastError() => Marshal.PtrToStringUTF8(inlaysql_last_error()) ?? "";

    /// <summary>The engine's message, as the exception class its prefix names.</summary>
    private static InlaySQLException Error(string message, string? sql = null)
    {
        var text = sql is null ? message : $"{message} — while running: {sql}";
        if (message.StartsWith("constraint failed", StringComparison.Ordinal)) return new InlaySQLConstraintException(text);
        if (message.StartsWith("write conflict", StringComparison.Ordinal)) return new InlaySQLConflictException(text);
        if (message.StartsWith("unsupported", StringComparison.Ordinal)) return new InlaySQLUnsupportedException(text);
        return new InlaySQLException(text);
    }

    // ---- self-test -------------------------------------------------------------

    /// <summary>
    /// The same checks the other clients' self-tests make. Call from a console
    /// project's Main: <c>InlaySQL.SelfTest.Run(args.Length > 0 ? args[0] : null);</c>
    /// </summary>
    public static class SelfTest
    {
        public static void Run(string? lib = null)
        {
            var dir = Directory.CreateTempSubdirectory("inlaysql-cs");
            var file = Path.Combine(dir.FullName, "selftest.inlay");
            try
            {
                using var db = Open(file, lib: lib);
                Check(db.Version().Length > 0, "version");
                var ddl = db.Execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT UNIQUE)");
                Check(ddl.IsDdl && ddl.Rows is null, "ddl");
                Check(db.Insert("t", new Dictionary<string, object?> { ["name"] = "Ada" }) == 1, "insert 1");
                Check(db.Insert("t", new Dictionary<string, object?> { ["name"] = "Grace" }) == 2, "insert 2");
                var written = db.Execute("UPDATE t SET name = :n WHERE id = :id",
                    new Dictionary<string, object?> { ["id"] = 2, ["n"] = "Grace H." });
                Check(written.RowsAffected == 1 && written.LastInsertId == 2, "affected");
                var all = db.Query("SELECT * FROM t ORDER BY id").All();
                Check(all.Count == 2 && (string?)all[1]["name"] == "Grace H." && (long?)all[0]["id"] == 1, "all");
                Check(db.Query("SELECT id FROM t").Count == 2, "count");
                Check((string?)db.First("SELECT name FROM t WHERE id = ?", 1)!["name"] == "Ada", "first");
                Check(db.First("SELECT name FROM t WHERE id = ?", 99) is null, "first none");
                Check((long?)db.Value("SELECT COUNT(*) FROM t") == 2, "value");
                Check(db.Column("SELECT name FROM t ORDER BY name").SequenceEqual(new object?[] { "Ada", "Grace H." }), "column");
                Check(db.Query("SELECT id, name FROM t ORDER BY id").Column("name").SequenceEqual(new object?[] { "Ada", "Grace H." }), "column by name");
                Check(db.Value("SELECT name FROM t WHERE name = ':not_a_param'") is null, "literal");
                Check(db.Query("SELECT id FROM t WHERE id > :after", new Dictionary<string, object?> { ["after"] = 1 }).Count == 1, "named");
                try { db.Insert("t", new Dictionary<string, object?> { ["name"] = "Ada" }); Check(false, "no constraint"); }
                catch (InlaySQLConstraintException) { }
                try
                {
                    db.Transaction(tx =>
                    {
                        tx.Insert("t", new Dictionary<string, object?> { ["name"] = "Linus" });
                        throw new InvalidOperationException("abort");
                    });
                }
                catch (InvalidOperationException) { }
                Check((long?)db.Value("SELECT COUNT(*) FROM t") == 2, "rolled back");
                Check(!db.InTransaction, "not in txn");
                Check(db.Transaction(tx => tx.Insert("t", new Dictionary<string, object?> { ["name"] = "Linus" })) == 3, "txn value");
                Check((long?)db.Transaction(tx => tx.Transaction(x => x.Value("SELECT COUNT(*) FROM t"))) == 3, "nested");
                try
                {
                    db.Execute("SELECT * FROM t WHERE name = :missing", new Dictionary<string, object?> { ["other"] = 1 });
                    Check(false, "missing param");
                }
                catch (InlaySQLException) { }
            }
            finally
            {
                try { dir.Delete(true); } catch { }
            }
            Console.WriteLine("InlaySQL.cs self-test passed");
        }

        private static void Check(bool ok, string what)
        {
            if (!ok) throw new InvalidOperationException("self-test failed: " + what);
        }
    }
}
