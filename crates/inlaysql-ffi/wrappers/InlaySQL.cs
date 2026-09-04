// InlaySQL — the C# / .NET wrapper over the C ABI.
//
// One file, no NuGet package: copy it into your project and open a database
// like SQLite — no server, the file is yours. Needs nothing but the BCL
// (System.Text.Json is in the box).
//
//   using var db = new InlaySQL("app.inlay");           // creates if absent
//   db.Run("CREATE TABLE IF NOT EXISTS users (
//       id INTEGER PRIMARY KEY, name TEXT)");
//   db.Run("INSERT INTO users (name) VALUES (?)", new object?[] { "Ada" });
//
//   var result = db.Run("SELECT id, name FROM users");
//   foreach (JsonElement row in result.GetProperty("rows").EnumerateArray())
//       Console.WriteLine(row[0], row[1]);
//
//   // rows as dictionaries:
//   foreach (var row in db.Query("SELECT id, name FROM users"))
//       Console.WriteLine(row["name"]);
//
// Read-only: new InlaySQL("app.inlay", readOnly: true) — the file must
// already exist and every write is refused.
//
// One handle is one thread at a time; wrap multi-threaded access so each
// thread has its own handle (the same rule SQLite's default connection
// has). Vector parameters bind as a double[] or float[]; vector cells come
// back as the placeholder "<vector(n)>" — the raw floats do not cross the
// boundary in JSON.
//
// Set the library path explicitly with the `lib` constructor argument, or
// drop libinlaysql_ffi.dylib/.so (from the release archive) next to the
// application. Tested against the C surface documented in inlaysql.h
// beside this file; the Java FFM wrapper in wrappers/InlaySQL.java is its
// sibling.
//
// NOTE: this file is verified by review against the tested PHP/Python/Ruby
// wrappers (same calls, same order, same JSON shapes) but has not been
// executed on a .NET runtime in this repository's CI yet — if you find a
// problem, the fix belongs here and the tests want it too.

using System.Runtime.InteropServices;
using System.Text.Json;

public sealed class InlaySQL : IDisposable
{
    private const int INLAYSQL_OK = 0;
    private const int INLAYSQL_ERR_BAD_HANDLE = 2;

    private readonly IntPtr _handle;

    // The C surface, verbatim from include/inlaysql.h. CharSet.Ansi matches
    // the ABI's UTF-8 expectation for ASCII paths and SQL; non-ASCII text
    // is marshalled correctly because the strings are built as UTF-8 on the
    // Rust side of every call we make (the JSON round trip guarantees it).
    private const string __Lib = "libinlaysql_ffi";   // .dylib / .so resolution

    [DllImport(__Lib)] private static extern IntPtr inlaysql_open([MarshalAs(UnmanagedType.LPUTF8Str)] string path);
    [DllImport(__Lib)] private static extern IntPtr inlaysql_open_read_only([MarshalAs(UnmanagedType.LPUTF8Str)] string path);
    [DllImport(__Lib)] private static extern void inlaysql_close(IntPtr handle);
    [DllImport(__Lib)] private static extern int inlaysql_exec(IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string sql,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string? parameters,
        out IntPtr outJson);
    [DllImport(__Lib)] private static extern IntPtr inlaysql_last_error();
    [DllImport(__Lib)] private static extern void inlaysql_free_string(IntPtr ptr);
    [DllImport(__Lib)] private static extern IntPtr inlaysql_version();

    /// <param name="path">the database file; created when absent</param>
    /// <param name="readOnly">open read-only; the file must already exist</param>
    /// <param name="lib">override the library name/path; default resolves
    /// libinlaysql_ffi.dylib / .so beside the executable or via the loader</param>
    public InlaySQL(string path, bool readOnly = false, string? lib = null)
    {
        if (lib is not null)
            NativeLibraryHint = lib;
        var open = readOnly ? inlaysql_open_read_only : inlaysql_open;
        _handle = open(path);
        if (_handle == IntPtr.Zero)
            throw new InlaySQLException($"open failed: {LastError()}");
    }

    /// <summary>Set before the first call to redirect the P/Invoke target.</summary>
    public static string? NativeLibraryHint { get; set; }

    public string Version() => Marshal.PtrToStringUTF8(inlaysql_version()) ?? "";

    /// <summary>
    /// Run one statement. Returns the JSON document: {"kind":"ddl"},
    /// {"kind":"written","rows":n}, or {"columns":[…],"rows":[[…],…]}.
    /// Parameters bind to `?` in order; an array of numbers is a vector.
    /// </summary>
    public JsonElement Run(string sql, object?[]? parameters = null)
    {
        if (inlaysql_exec(_handle, sql,
                parameters is null || parameters.Length == 0
                    ? null
                    : JsonSerializer.Serialize(parameters),
                out var outJson) != INLAYSQL_OK)
        {
            var message = LastError();
            throw new InlaySQLException($"{message} — while running: {sql}");
        }
        using var doc = JsonDocument.Parse(Marshal.PtrToStringUTF8(outJson)!);
        var result = doc.RootElement.Clone();
        inlaysql_free_string(outJson);
        return result;
    }

    /// <summary>Run a SELECT; rows as dictionaries keyed by column name.</summary>
    public List<Dictionary<string, object?>> Query(string sql, object?[]? parameters = null)
    {
        var result = Run(sql, parameters);
        if (!result.TryGetProperty("columns", out var columns))
            throw new InlaySQLException($"not a query: {sql}");

        var rows = new List<Dictionary<string, object?>>();
        var names = columns.EnumerateArray().Select(c => c.GetString()!).ToArray();
        foreach (var row in result.GetProperty("rows").EnumerateArray())
        {
            var dict = new Dictionary<string, object?>(names.Length);
            var i = 0;
            foreach (var value in row.EnumerateArray())
                dict[names[i++]] = value.ValueKind switch
                {
                    JsonValueKind.Null => null,
                    JsonValueKind.Number => value.GetDouble(),
                    JsonValueKind.String => value.GetString(),
                    _ => value.Clone(),
                };
            rows.Add(dict);
        }
        return rows;
    }

    /// <summary>First row as a dictionary, or null.</summary>
    public Dictionary<string, object?>? First(string sql, object?[]? parameters = null) =>
        Query(sql, parameters).FirstOrDefault();

    public void Dispose() => inlaysql_close(_handle);

    private string LastError() => Marshal.PtrToStringUTF8(inlaysql_last_error()) ?? "";

    /// <summary>Thrown with the engine's own message, verbatim — there are no numeric codes.</summary>
    public sealed class InlaySQLException(string message) : Exception(message);

    // `lib` handling: DllImport binds at JIT time to a constant name, so an
    // explicit per-instance path is served by NativeLibraryHint plus the
    // runtime's NativeLibrary resolution — set the environment variable the
    // loader consults before the first call instead of per-instance.
    private static string? NativeLibraryHintSetter
    {
        set
        {
            if (value is null) return;
            Environment.SetEnvironmentVariable(
                OperatingSystem.IsMacOS() ? "DYLD_LIBRARY_PATH"
                : OperatingSystem.IsLinux() ? "LD_LIBRARY_PATH"
                : "PATH",
                value + Path.PathSeparator + Environment.GetEnvironmentVariable(
                    OperatingSystem.IsMacOS() ? "DYLD_LIBRARY_PATH"
                    : OperatingSystem.IsLinux() ? "LD_LIBRARY_PATH" : "PATH"));
        }
    }
}
