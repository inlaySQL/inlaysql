// InlaySQL — the Java client over the C ABI.
//
// One file, JDK 22+ (the Foreign Function & Memory API is final there): copy
// it into your project and open a database like SQLite — no server, the file
// is yours. No JNI, no C compilation, no native packaging step, no dependency.
//
//   try (InlaySQL db = InlaySQL.open(Path.of("app.inlay"))) {      // creates if absent
//       db.execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, email TEXT)");
//
//       long id = db.insert("users", Map.of("name", "Ada", "email", "ada@example.org"));
//
//       for (Map<String, Object> user : db.query("SELECT id, name FROM users WHERE id > :after", Map.of("after", 0)))
//           System.out.println(user.get("name"));                  // rows as maps
//       Map<String, Object> ada = db.first("SELECT * FROM users WHERE id = ?", id);   // one row, or null
//       Object count = db.value("SELECT COUNT(*) FROM users");                        // one cell
//       List<Object> names = db.column("SELECT name FROM users ORDER BY name");       // one column
//
//       InlaySQL.Result r = db.execute("UPDATE users SET name = ? WHERE id = ?", "Ada L.", id);
//       r.rowsAffected();                                            // 1
//
//       db.inTransaction(tx -> {                                     // BEGIN … COMMIT; ROLLBACK
//           tx.insert("users", Map.of("name", "Grace"));             // on throw; rerun on a
//           tx.insert("users", Map.of("name", "Linus"));             // write conflict
//       });
//       long n = db.transaction(tx -> tx.insert("users", Map.of("name", "Ken")));   // the same, with a value
//   }
//
// Parameters: positional `?` as varargs, or named `:name` with a Map. A
// double[] or float[] binds as a vector, so a retrieval call is
//
//   db.query("SELECT id, vector_score(embedding, ?) AS s FROM docs ORDER BY s LIMIT 10", embedding);
//
// Errors are unchecked: InlaySQL.ConstraintException (unique/not-null…),
// InlaySQL.ConflictException (another writer committed first — retry),
// InlaySQL.UnsupportedException (a clause the engine refuses rather than
// ignores), and InlaySQL.InlaySQLException for the rest; the engine's own
// message is the exception message.
//
// Read-only: InlaySQL.open(path, true) — the file must already exist and
// every write is refused.
//
// One handle is one thread at a time (open one per thread), and every one
// of them may write — concurrent commits to one file are what this engine
// does that SQLite does not. The library is looked up once per process:
// the system property `inlaysql.lib`, then the environment variable
// `INLAYSQL_LIB`, then the plain library name beside the working directory
// or on the loader's path (java.library.path, DYLD_LIBRARY_PATH,
// LD_LIBRARY_PATH). Vector cells come back as the placeholder "<vector(n)>"
// — the raw floats do not cross the boundary in JSON.
//
// Self-test: java InlaySQL.java [path/to/lib]   (JDK 22+, single-file launch)
//
// Tested against libinlaysql_ffi from inlaySQL/inlaysql v0.0.5; the C
// surface it wraps is documented in include/inlaysql.h beside this file.

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Consumer;
import java.util.function.Function;

public final class InlaySQL implements AutoCloseable {

    private static final int INLAYSQL_OK = 0;
    private static final int INLAYSQL_ERR_BAD_HANDLE = 2;

    // ---- errors ------------------------------------------------------------

    /** Thrown with the engine's own message, verbatim — there are no numeric codes. */
    public static class InlaySQLException extends RuntimeException {
        public InlaySQLException(String message) { super(message); }
        public InlaySQLException(String message, Throwable cause) { super(message, cause); }
    }

    /** UNIQUE, NOT NULL, CHECK, FOREIGN KEY — the row was refused. */
    public static final class ConstraintException extends InlaySQLException {
        public ConstraintException(String message) { super(message); }
    }

    /** Another handle committed first; nothing was written. Safe to retry. */
    public static final class ConflictException extends InlaySQLException {
        public ConflictException(String message) { super(message); }
    }

    /** A clause this engine refuses rather than silently ignores. */
    public static final class UnsupportedException extends InlaySQLException {
        public UnsupportedException(String message) { super(message); }
    }

    private static InlaySQLException error(String message, String sql) {
        String text = sql == null ? message : message + " — while running: " + sql;
        if (message.startsWith("constraint failed")) return new ConstraintException(text);
        if (message.startsWith("write conflict")) return new ConflictException(text);
        if (message.startsWith("unsupported")) return new UnsupportedException(text);
        return new InlaySQLException(text);
    }

    // ---- results -----------------------------------------------------------

    /**
     * What a statement did. {@code rows} is set only when it was a query.
     * {@code lastInsertId} is the most recent INSERT's row id on this handle
     * (SQLite's {@code last_insert_rowid()} contract), or null before any INSERT.
     */
    public record Result(long rowsAffected, Long lastInsertId, boolean isDdl, Rows rows) {}

    /**
     * A query's answer. Iterates as ordered maps keyed by column name;
     * {@link #size()} is the row count; the accessors answer the common
     * shapes without a loop.
     */
    public static final class Rows implements Iterable<Map<String, Object>> {
        private final List<String> columns;
        private final List<List<Object>> rows;

        Rows(List<String> columns, List<List<Object>> rows) {
            this.columns = columns;
            this.rows = rows;
        }

        public List<String> columns() { return columns; }

        public int size() { return rows.size(); }

        public boolean isEmpty() { return rows.isEmpty(); }

        @Override public Iterator<Map<String, Object>> iterator() {
            Iterator<List<Object>> inner = rows.iterator();
            return new Iterator<>() {
                @Override public boolean hasNext() { return inner.hasNext(); }
                @Override public Map<String, Object> next() { return toMap(inner.next()); }
            };
        }

        /** Every row as an ordered map. */
        public List<Map<String, Object>> all() {
            List<Map<String, Object>> out = new ArrayList<>(rows.size());
            for (List<Object> row : rows) out.add(toMap(row));
            return out;
        }

        /** The rows exactly as the ABI returned them, positional. */
        public List<List<Object>> raw() { return rows; }

        /** First row as an ordered map, or null. */
        public Map<String, Object> first() { return rows.isEmpty() ? null : toMap(rows.get(0)); }

        /** The first cell of the first row, or null when there is no row. */
        public Object value() {
            return rows.isEmpty() || rows.get(0).isEmpty() ? null : rows.get(0).get(0);
        }

        /** One column, by index, across every row. */
        public List<Object> column(int index) {
            if (index < 0 || index >= columns.size()) throw new InlaySQLException("no such column: " + index);
            List<Object> out = new ArrayList<>(rows.size());
            for (List<Object> row : rows) out.add(row.get(index));
            return out;
        }

        /** One column, by name, across every row. */
        public List<Object> column(String name) {
            int index = columns.indexOf(name);
            if (index < 0) throw new InlaySQLException("no such column: " + name);
            return column(index);
        }

        private Map<String, Object> toMap(List<Object> row) {
            Map<String, Object> map = new LinkedHashMap<>(columns.size() * 2);
            for (int i = 0; i < columns.size(); i++) map.put(columns.get(i), row.get(i));
            return map;
        }
    }

    // ---- the C surface -----------------------------------------------------

    private static final Linker LINKER = Linker.nativeLinker();
    private static final Arena LIBRARY_ARENA = Arena.global();
    private static volatile SymbolLookup lib;
    private static MethodHandle OPEN, OPEN_READ_ONLY, CLOSE, EXEC, LAST_ERROR, FREE, VERSION;

    private static String libraryName() {
        String os = System.getProperty("os.name").toLowerCase();
        if (os.contains("mac") || os.contains("darwin")) return "libinlaysql_ffi.dylib";
        if (os.contains("win")) return "inlaysql_ffi.dll";
        return "libinlaysql_ffi.so";
    }

    /**
     * Bind the C functions once. {@code explicit} wins; then the system
     * property {@code inlaysql.lib}; then {@code INLAYSQL_LIB}; then the plain
     * name beside the working directory; then the loader's own search.
     */
    private static synchronized void attach(String explicit) {
        if (lib != null) return;
        String path = explicit != null ? explicit
            : System.getProperty("inlaysql.lib") != null ? System.getProperty("inlaysql.lib")
            : System.getenv("INLAYSQL_LIB") != null ? System.getenv("INLAYSQL_LIB")
            : Files.isRegularFile(Path.of(libraryName())) ? Path.of(libraryName()).toAbsolutePath().toString()
            : libraryName();
        SymbolLookup lookup;
        try {
            lookup = path.contains("/") || path.contains("\\")
                ? SymbolLookup.libraryLookup(Path.of(path), LIBRARY_ARENA)
                : SymbolLookup.libraryLookup(path, LIBRARY_ARENA);
        } catch (IllegalArgumentException e) {
            throw new InlaySQLException("could not load " + path
                + " — pass the path to open(), set -Dinlaysql.lib or INLAYSQL_LIB, or download it from"
                + " https://github.com/inlaySQL/inlaysql/releases", e);
        }
        OPEN = handle(lookup, "inlaysql_open", FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        OPEN_READ_ONLY = handle(lookup, "inlaysql_open_read_only", FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        CLOSE = handle(lookup, "inlaysql_close", FunctionDescriptor.ofVoid(ValueLayout.ADDRESS));
        EXEC = handle(lookup, "inlaysql_exec", FunctionDescriptor.of(ValueLayout.JAVA_INT,
            ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        LAST_ERROR = handle(lookup, "inlaysql_last_error", FunctionDescriptor.of(ValueLayout.ADDRESS));
        FREE = handle(lookup, "inlaysql_free_string", FunctionDescriptor.ofVoid(ValueLayout.ADDRESS));
        VERSION = handle(lookup, "inlaysql_version", FunctionDescriptor.of(ValueLayout.ADDRESS));
        lib = lookup;
    }

    private static MethodHandle handle(SymbolLookup lookup, String name, FunctionDescriptor descriptor) {
        MemorySegment symbol = lookup.find(name).orElseThrow(() ->
            new InlaySQLException("symbol not found in " + libraryName() + ": " + name));
        return LINKER.downcallHandle(symbol, descriptor);
    }

    /** A C string the ABI returned, which may be unbounded in length. */
    private static String cString(MemorySegment s) {
        return s.address() == 0 ? "" : s.reinterpret(Long.MAX_VALUE).getString(0);
    }

    private static String lastError() {
        try {
            return cString((MemorySegment) LAST_ERROR.invoke());
        } catch (Throwable t) {
            throw new InlaySQLException("inlaysql_last_error failed", t);
        }
    }

    // ---- lifecycle ---------------------------------------------------------

    private MemorySegment handle;
    private int depth;

    private InlaySQL(MemorySegment handle) { this.handle = handle; }

    /** Open the database file at {@code path}, creating it if it does not exist. */
    public static InlaySQL open(Path path) { return open(path, false, null); }

    /** Open read-only; the file must already exist, and every write is refused. */
    public static InlaySQL open(Path path, boolean readOnly) { return open(path, readOnly, null); }

    /** Open with an explicit library path (first call only; later calls reuse the bound library). */
    public static InlaySQL open(Path path, boolean readOnly, String lib) {
        attach(lib);
        try (Arena call = Arena.ofConfined()) {
            MemorySegment cpath = call.allocateFrom(path.toString());
            MemorySegment h = (MemorySegment) (readOnly ? OPEN_READ_ONLY : OPEN).invoke(cpath);
            if (h.address() == 0) throw error("open failed: " + lastError(), null);
            return new InlaySQL(h);
        } catch (InlaySQLException e) {
            throw e;
        } catch (Throwable t) {
            throw new InlaySQLException("inlaysql_open failed", t);
        }
    }

    public String version() {
        try {
            return cString((MemorySegment) VERSION.invoke());
        } catch (Throwable t) {
            throw new InlaySQLException("inlaysql_version failed", t);
        }
    }

    @Override public void close() {
        if (handle == null) return;
        try {
            CLOSE.invoke(handle);
        } catch (Throwable t) {
            throw new InlaySQLException("inlaysql_close failed", t);
        } finally {
            handle = null;
        }
    }

    // ---- statements --------------------------------------------------------

    /** Run any statement. For a write, the result says how many rows and which id; for a query, prefer {@link #query}. */
    public Result execute(String sql, Object... params) { return toResult(run(sql, params)); }

    /** {@link #execute} with named {@code :name} parameters. */
    public Result execute(String sql, Map<String, ?> params) { return toResult(run(sql, params)); }

    private Result toResult(JsonObject raw) {
        if (raw.members.containsKey("columns")) return new Result(0, null, false, toRows(raw));
        Object rows = prim(raw, "rows");
        Object last = prim(raw, "last_insert_id");
        return new Result(rows == null ? 0 : ((Number) rows).longValue(),
            last == null ? null : ((Number) last).longValue(),
            "ddl".equals(prim(raw, "kind")), null);
    }

    /** Run a query. Iterate the result, or ask it for {@code all()}, {@code first()}, {@code value()}, {@code column()}. */
    public Rows query(String sql, Object... params) {
        JsonObject raw = run(sql, params);
        if (!raw.members.containsKey("columns")) throw error("not a query: " + sql, null);
        return toRows(raw);
    }

    /** {@link #query} with named {@code :name} parameters. */
    public Rows query(String sql, Map<String, ?> params) {
        JsonObject raw = run(sql, params);
        if (!raw.members.containsKey("columns")) throw error("not a query: " + sql, null);
        return toRows(raw);
    }

    /** First row as an ordered map, or null. */
    public Map<String, Object> first(String sql, Object... params) { return query(sql, params).first(); }
    public Map<String, Object> first(String sql, Map<String, ?> params) { return query(sql, params).first(); }

    /** The first cell of the first row, or null when there is no row. */
    public Object value(String sql, Object... params) { return query(sql, params).value(); }
    public Object value(String sql, Map<String, ?> params) { return query(sql, params).value(); }

    /** The first column across every row. */
    public List<Object> column(String sql, Object... params) { return query(sql, params).column(0); }
    public List<Object> column(String sql, Map<String, ?> params) { return query(sql, params).column(0); }

    /** Insert one row given as {@code column -> value}; return its row id. */
    public long insert(String table, Map<String, ?> row) {
        if (row.isEmpty()) throw error("insert into " + table + ": no columns given", null);
        StringBuilder columns = new StringBuilder();
        StringBuilder marks = new StringBuilder();
        Object[] values = new Object[row.size()];
        int i = 0;
        for (Map.Entry<String, ?> entry : row.entrySet()) {
            if (i > 0) { columns.append(", "); marks.append(", "); }
            columns.append(quoteIdentifier(entry.getKey()));
            marks.append('?');
            values[i++] = entry.getValue();
        }
        Result result = execute("INSERT INTO " + quoteIdentifier(table)
            + " (" + columns + ") VALUES (" + marks + ")", values);
        if (result.lastInsertId() == null) throw error("insert into " + table + " reported no row id", null);
        return result.lastInsertId();
    }

    // ---- transactions ------------------------------------------------------

    /**
     * Run {@code body} inside BEGIN … COMMIT and return its value. A throw rolls
     * back and rethrows. A write conflict (another handle committed first)
     * rolls back and reruns {@code body}, up to {@code retries} times, so it
     * must be safe to repeat. Nested calls join the outer transaction.
     */
    public <T> T transaction(Function<InlaySQL, T> body, int retries) {
        if (depth > 0) {
            depth++;
            try {
                return body.apply(this);
            } finally {
                depth--;
            }
        }
        int attempt = 0;
        while (true) {
            run("BEGIN", new Object[0]);
            depth = 1;
            try {
                T value = body.apply(this);
                run("COMMIT", new Object[0]);
                depth = 0;
                return value;
            } catch (Throwable t) {
                depth = 0;
                rollbackQuietly();
                if (t instanceof ConflictException && attempt++ < retries) continue;
                throw t;
            }
        }
    }

    public <T> T transaction(Function<InlaySQL, T> body) { return transaction(body, 3); }

    /** {@link #transaction(Function)} for a body with no value. */
    public void inTransaction(Consumer<InlaySQL> body) {
        transaction(db -> { body.accept(db); return null; }, 3);
    }

    public boolean isInTransaction() { return depth > 0; }

    private void rollbackQuietly() {
        try {
            run("ROLLBACK", new Object[0]);
        } catch (InlaySQLException ignored) {
            // A conflict at COMMIT already ended the transaction; nothing to undo.
        }
    }

    // ---- the raw call ------------------------------------------------------

    /**
     * Run one statement and return the ABI's JSON text:
     * {@code {"kind":"ddl"}}, {@code {"kind":"written","rows":n,"last_insert_id":k}},
     * or {@code {"columns":[…],"rows":[[…],…]}}. The typed methods are built on
     * this; it stays public for callers that want the raw shape.
     */
    public String runRaw(String sql, Object... params) {
        return exec(sql, params == null || params.length == 0 ? null : toJson(params));
    }

    private JsonObject run(String sql, Object[] params) {
        String text = exec(sql, params == null || params.length == 0 ? null : toJson(params));
        return (JsonObject) new Parser(text).parseValue();
    }

    private JsonObject run(String sql, Map<String, ?> params) {
        Object[] bound = new Object[0];
        if (params != null && !params.isEmpty()) {
            List<Object> values = new ArrayList<>();
            sql = bindNamed(sql, params, values);
            bound = values.toArray();
        }
        return run(sql, bound);
    }

    private String exec(String sql, String paramsJson) {
        if (handle == null) throw error("the database is closed", null);
        try (Arena call = Arena.ofConfined()) {
            MemorySegment csql = call.allocateFrom(sql);
            MemorySegment cparams = paramsJson == null ? MemorySegment.NULL : call.allocateFrom(paramsJson);
            MemorySegment out = call.allocate(ValueLayout.ADDRESS);
            int code = (int) EXEC.invoke(handle, csql, cparams, out);
            if (code == INLAYSQL_ERR_BAD_HANDLE) throw error("bad handle", null);
            if (code != INLAYSQL_OK) throw error(lastError(), sql);
            MemorySegment json = out.get(ValueLayout.ADDRESS, 0);
            try {
                return cString(json);
            } finally {
                FREE.invoke(json);
            }
        } catch (InlaySQLException e) {
            throw e;
        } catch (Throwable t) {
            throw new InlaySQLException("inlaysql_exec failed", t);
        }
    }

    // ---- helpers -----------------------------------------------------------

    /** Rewrite {@code :name} to {@code ?} in order of appearance, skipping quoted runs. */
    static String bindNamed(String sql, Map<String, ?> params, List<Object> values) {
        StringBuilder out = new StringBuilder(sql.length());
        int i = 0, n = sql.length();
        while (i < n) {
            char c = sql.charAt(i);
            if (c == '\'' || c == '"' || c == '`') {
                int j = i + 1;
                while (j < n) {
                    if (sql.charAt(j) == c) {
                        if (j + 1 < n && sql.charAt(j + 1) == c) { j += 2; continue; }
                        break;
                    }
                    j++;
                }
                out.append(sql, i, Math.min(j + 1, n));
                i = j + 1;
                continue;
            }
            if (c == ':' && i + 1 < n && (Character.isLetter(sql.charAt(i + 1)) || sql.charAt(i + 1) == '_')) {
                int j = i + 1;
                while (j < n && (Character.isLetterOrDigit(sql.charAt(j)) || sql.charAt(j) == '_')) j++;
                String name = sql.substring(i + 1, j);
                if (!params.containsKey(name)) throw error("no value bound for :" + name, sql);
                values.add(params.get(name));
                out.append('?');
                i = j;
                continue;
            }
            out.append(c);
            i++;
        }
        return out.toString();
    }

    private static String quoteIdentifier(String name) {
        return '"' + name.replace("\"", "\"\"") + '"';
    }

    private static Rows toRows(JsonObject raw) {
        List<String> columns = new ArrayList<>();
        for (JsonValue c : ((JsonArray) raw.members.get("columns")).items) columns.add((String) ((JsonPrimitive) c).value);
        List<List<Object>> rows = new ArrayList<>();
        for (JsonValue row : ((JsonArray) raw.members.get("rows")).items) {
            List<Object> cells = new ArrayList<>();
            for (JsonValue cell : ((JsonArray) row).items) cells.add(((JsonPrimitive) cell).value);
            rows.add(cells);
        }
        return new Rows(columns, rows);
    }

    private static Object prim(JsonObject obj, String key) {
        JsonValue v = obj.members.get(key);
        return v instanceof JsonPrimitive p ? p.value : null;
    }

    // ---- the smallest JSON writer and reader the result shapes need, so this
    // ---- file keeps the engine's zero-dependency rule.

    private static String toJson(Object[] params) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < params.length; i++) {
            if (i > 0) sb.append(',');
            toJsonValue(params[i], sb);
        }
        return sb.append(']').toString();
    }

    private static void toJsonValue(Object value, StringBuilder sb) {
        switch (value) {
            case null -> sb.append("null");
            case Integer i -> sb.append(i);
            case Long l -> sb.append(l);
            case Short s -> sb.append(s);
            case Byte b -> sb.append(b);
            case Double d -> sb.append(d);
            case Float f -> sb.append(f);
            case Boolean b -> sb.append(b);
            case String s -> sb.append(quote(s));
            case double[] v -> {
                sb.append('[');
                for (int i = 0; i < v.length; i++) { if (i > 0) sb.append(','); sb.append(v[i]); }
                sb.append(']');
            }
            case float[] v -> {
                sb.append('[');
                for (int i = 0; i < v.length; i++) { if (i > 0) sb.append(','); sb.append(v[i]); }
                sb.append(']');
            }
            case List<?> list -> {
                sb.append('[');
                for (int i = 0; i < list.size(); i++) { if (i > 0) sb.append(','); toJsonValue(list.get(i), sb); }
                sb.append(']');
            }
            default -> throw new InlaySQLException("unsupported parameter type: " + value.getClass().getName());
        }
    }

    private static String quote(String s) {
        StringBuilder sb = new StringBuilder("\"");
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"' -> sb.append("\\\"");
                case '\\' -> sb.append("\\\\");
                case '\n' -> sb.append("\\n");
                case '\r' -> sb.append("\\r");
                case '\t' -> sb.append("\\t");
                default -> {
                    if (c < 0x20) sb.append(String.format("\\u%04x", (int) c));
                    else sb.append(c);
                }
            }
        }
        return sb.append('"').toString();
    }

    private sealed interface JsonValue permits JsonArray, JsonObject, JsonPrimitive {}
    private record JsonArray(List<JsonValue> items) implements JsonValue {}
    private record JsonObject(Map<String, JsonValue> members) implements JsonValue {}
    private record JsonPrimitive(Object value) implements JsonValue {}

    private static final class Parser {
        private final String text;
        private int pos;

        Parser(String text) { this.text = text; }

        JsonValue parseValue() {
            skipWhitespace();
            char c = next("a value");
            switch (c) {
                case '[': {
                    List<JsonValue> items = new ArrayList<>();
                    skipWhitespace();
                    if (peek() == ']') { pos++; return new JsonArray(items); }
                    while (true) {
                        items.add(parseValue());
                        skipWhitespace();
                        char sep = next("',' or ']'");
                        if (sep == ']') return new JsonArray(items);
                        if (sep != ',') throw error("expected , or ]");
                    }
                }
                case '{': {
                    Map<String, JsonValue> members = new LinkedHashMap<>();
                    skipWhitespace();
                    if (peek() == '}') { pos++; return new JsonObject(members); }
                    while (true) {
                        skipWhitespace();
                        String key = parseString();
                        skipWhitespace();
                        if (next("':'") != ':') throw error("expected :");
                        members.put(key, parseValue());
                        skipWhitespace();
                        char sep = next("',' or '}'");
                        if (sep == '}') return new JsonObject(members);
                        if (sep != ',') throw error("expected , or }");
                    }
                }
                case '"': pos--; return new JsonPrimitive(parseString());
                case 'n': expect("ull"); return new JsonPrimitive(null);
                case 't': expect("rue"); return new JsonPrimitive(Boolean.TRUE);
                case 'f': expect("alse"); return new JsonPrimitive(Boolean.FALSE);
                default: {
                    if (c != '-' && (c < '0' || c > '9')) throw error("unexpected character");
                    StringBuilder number = new StringBuilder().append(c);
                    while (pos < text.length()
                        && (Character.isDigit(text.charAt(pos)) || "eE.+-".indexOf(text.charAt(pos)) >= 0)) {
                        number.append(text.charAt(pos++));
                    }
                    String n = number.toString();
                    return new JsonPrimitive(n.contains(".") || n.contains("e") || n.contains("E")
                        ? (Object) Double.parseDouble(n)
                        : (Object) Long.parseLong(n));
                }
            }
        }

        private String parseString() {
            if (next("a string") != '"') throw error("expected string");
            StringBuilder sb = new StringBuilder();
            while (true) {
                char c = next("string contents");
                if (c == '"') return sb.toString();
                if (c == '\\') {
                    char e = next("an escape");
                    switch (e) {
                        case '"' -> sb.append('"');
                        case '\\' -> sb.append('\\');
                        case '/' -> sb.append('/');
                        case 'b' -> sb.append('\b');
                        case 'f' -> sb.append('\f');
                        case 'n' -> sb.append('\n');
                        case 'r' -> sb.append('\r');
                        case 't' -> sb.append('\t');
                        case 'u' -> sb.append((char) Integer.parseInt(text.substring(pos, pos += 4), 16));
                        default -> throw error("unknown escape \\" + e);
                    }
                } else {
                    sb.append(c);
                }
            }
        }

        private void expect(String rest) {
            for (char expected : rest.toCharArray()) {
                if (next(rest) != expected) throw error("expected " + rest);
            }
        }

        private void skipWhitespace() {
            while (pos < text.length() && Character.isWhitespace(text.charAt(pos))) pos++;
        }

        private char peek() {
            if (pos >= text.length()) throw error("unexpected end");
            return text.charAt(pos);
        }

        private char next(String what) {
            if (pos >= text.length()) throw error("expected " + what + ", found end of input");
            return text.charAt(pos++);
        }

        private InlaySQLException error(String what) {
            return new InlaySQLException("malformed engine result: " + what + " at offset " + pos + " in: " + text);
        }
    }

    // ---- self-test: java InlaySQL.java [path/to/lib] ---------------------------

    public static void main(String[] args) throws Exception {
        String lib = args.length > 0 ? args[0] : null;
        Path dir = Files.createTempDirectory("inlaysql-java");
        Path file = dir.resolve("selftest.inlay");
        try (InlaySQL db = InlaySQL.open(file, false, lib)) {
            check(!db.version().isEmpty(), "version");
            Result ddl = db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT UNIQUE)");
            check(ddl.isDdl() && ddl.rows() == null, "ddl");
            check(db.insert("t", Map.of("name", "Ada")) == 1, "insert 1");
            check(db.insert("t", Map.of("name", "Grace")) == 2, "insert 2");
            Result written = db.execute("UPDATE t SET name = :n WHERE id = :id", Map.of("id", 2, "n", "Grace H."));
            check(written.rowsAffected() == 1 && written.lastInsertId() == 2, "affected");
            List<Map<String, Object>> all = db.query("SELECT * FROM t ORDER BY id").all();
            check(all.size() == 2 && all.get(1).get("name").equals("Grace H.") && all.get(0).get("id").equals(1L), "all");
            int n = 0;
            for (Map<String, Object> row : db.query("SELECT id FROM t")) n += ((Long) row.get("id")).intValue();
            check(n == 3, "iterate");
            check(db.first("SELECT name FROM t WHERE id = ?", 1).get("name").equals("Ada"), "first");
            check(db.first("SELECT name FROM t WHERE id = ?", 99) == null, "first none");
            check(db.value("SELECT COUNT(*) FROM t").equals(2L), "value");
            check(db.column("SELECT name FROM t ORDER BY name").equals(List.of("Ada", "Grace H.")), "column");
            check(db.query("SELECT id, name FROM t ORDER BY id").column("name").equals(List.of("Ada", "Grace H.")), "column by name");
            check(db.value("SELECT name FROM t WHERE name = ':not_a_param'") == null, "literal");
            check(db.query("SELECT id FROM t WHERE id > :after", Map.of("after", 1)).size() == 1, "named");
            try {
                db.insert("t", Map.of("name", "Ada"));
                check(false, "no constraint");
            } catch (ConstraintException expected) { /* ok */ }
            try {
                db.inTransaction(tx -> {
                    tx.insert("t", Map.of("name", "Linus"));
                    throw new IllegalStateException("abort");
                });
            } catch (IllegalStateException expected) { /* ok */ }
            check(db.value("SELECT COUNT(*) FROM t").equals(2L), "rolled back");
            check(!db.isInTransaction(), "not in txn");
            check(db.transaction(tx -> tx.insert("t", Map.of("name", "Linus"))) == 3L, "txn value");
            check(db.transaction(tx -> tx.transaction(x -> x.value("SELECT COUNT(*) FROM t"))).equals(3L), "nested");
            try {
                db.execute("SELECT * FROM t WHERE name = :missing", Map.of("other", 1));
                check(false, "missing param");
            } catch (InlaySQLException expected) { /* ok */ }
        } finally {
            Files.deleteIfExists(file);
            Files.deleteIfExists(dir);
        }
        System.out.println("InlaySQL.java self-test passed");
    }

    private static void check(boolean ok, String what) {
        if (!ok) throw new AssertionError("self-test failed: " + what);
    }
}
