// InlaySQL — the Java wrapper over the C ABI.
//
// One file, JDK 22+ (the Foreign Function & Memory API is standard): copy it
// into your project and open a database like SQLite — no server, the file is
// yours. No JNI, no C compilation, no native packaging step.
//
//   try (InlaySQL db = InlaySQL.open(Path.of("app.inlay"))) {  // creates if absent
//       db.run("CREATE TABLE IF NOT EXISTS users (
//           id INTEGER PRIMARY KEY, name TEXT)");
//       db.run("INSERT INTO users (name) VALUES (?)", "Ada");
//
//       String json = db.run("SELECT id, name FROM users");
//       // {"columns":["id","name"],"rows":[[1,"Ada"]]}
//
//       for (List<Object> row : db.query("SELECT id, name FROM users"))
//           System.out.println(row.get(1));
//   }
//
// Read-only: InlaySQL.open(path, true) — the file must already exist and
// every write is refused.
//
// One handle is one thread at a time (open one per thread); the same rule
// SQLite's default connection has. Vector parameters bind as a double[] or
// float[]; vector cells come back as the placeholder "<vector(n)>" — the raw
// floats do not cross the boundary in JSON.
//
// JSON marshalling uses the JDK alone (a small JSONArray/JSONValue reader
// for the three shapes the engine returns), so there is no dependency at
// all — the same zero-dependency rule the engine keeps.
//
// NOTE: this file is verified by review against the tested PHP/Python/Ruby
// wrappers (same calls, same order, same JSON shapes) but has not been
// executed on a JDK in this repository's CI yet — if you find a problem,
// the fix belongs here and the tests want it too.

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public final class InlaySQL implements AutoCloseable {

    private static final int INLAYSQL_OK = 0;
    private static final int INLAYSQL_ERR_BAD_HANDLE = 2;

    private static final Linker LINKER = Linker.nativeLinker();
    private static final Arena LIBRARY_ARENA = Arena.global();
    private static final SymbolLookup LIB = SymbolLookup.libraryLookup(libraryName(), LIBRARY_ARENA);

    private static String libraryName() {
        String os = System.getProperty("os.name").toLowerCase();
        if (os.contains("mac") || os.contains("darwin")) return "libinlaysql_ffi.dylib";
        if (os.contains("win")) return "inlaysql_ffi.dll";
        return "libinlaysql_ffi.so";
    }

    private static MemorySegment symbol(String name) {
        return LIB.find(name).orElseThrow(() ->
            new InlaySQLException("symbol not found in " + libraryName() + ": " + name
                + " — download it from https://github.com/inlaySQL/inlaysql/releases"));
    }

    private static final MethodHandle OPEN = LINKER.downcallHandle(symbol("inlaysql_open"),
        FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    private static final MethodHandle OPEN_READ_ONLY = LINKER.downcallHandle(symbol("inlaysql_open_read_only"),
        FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    private static final MethodHandle CLOSE = LINKER.downcallHandle(symbol("inlaysql_close"),
        FunctionDescriptor.ofVoid(ValueLayout.ADDRESS));
    private static final MethodHandle EXEC = LINKER.downcallHandle(symbol("inlaysql_exec"),
        FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS,
            ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    private static final MethodHandle LAST_ERROR = LINKER.downcallHandle(symbol("inlaysql_last_error"),
        FunctionDescriptor.of(ValueLayout.ADDRESS));
    private static final MethodHandle FREE = LINKER.downcallHandle(symbol("inlaysql_free_string"),
        FunctionDescriptor.ofVoid(ValueLayout.ADDRESS));
    private static final MethodHandle VERSION = LINKER.downcallHandle(symbol("inlaysql_version"),
        FunctionDescriptor.of(ValueLayout.ADDRESS));

    private final MemorySegment handle;
    private final Arena arena;

    private InlaySQL(MemorySegment handle, Arena arena) {
        this.handle = handle;
        this.arena = arena;
    }

    /** Open the database file at {@code path}, creating it if it does not exist. */
    public static InlaySQL open(Path path) throws Throwable {
        return open(path, false);
    }

    /** Open read-only; the file must already exist, and every write is refused. */
    public static InlaySQL open(Path path, boolean readOnly) throws Throwable {
        Arena arena = Arena.ofConfined();
        try {
            MemorySegment cpath = arena.allocateUtf8(path.toString());
            MethodHandle open = readOnly ? OPEN_READ_ONLY : OPEN;
            MemorySegment handle = (MemorySegment) open.invoke(cpath);
            if (handle.address() == 0) {
                throw new InlaySQLException("open failed: " + lastError());
            }
            return new InlaySQL(handle, arena);
        } catch (Throwable t) {
            arena.close();
            throw t;
        }
    }

    public String version() throws Throwable {
        MemorySegment s = (MemorySegment) VERSION.invoke();
        return s.getUtf8String(0);
    }

    /**
     * Run one statement. Returns the JSON text in one of the engine's three
     * shapes: {"kind":"ddl"}, {"kind":"written","rows":n}, or
     * {"columns":[…],"rows":[[…],…]}. Parameters bind to {@code ?} in order;
     * a {@code double[]} or {@code float[]} is a vector parameter.
     */
    public String run(String sql, Object... params) throws Throwable {
        try (Arena call = Arena.ofConfined()) {
            MemorySegment csql = call.allocateUtf8(sql);
            MemorySegment cparams = (params == null || params.length == 0)
                ? MemorySegment.NULL
                : call.allocateUtf8(toJson(params));
            MemorySegment out = call.allocate(ValueLayout.ADDRESS);
            int code = (int) EXEC.invoke(handle, csql, cparams, out);
            if (code == INLAYSQL_ERR_BAD_HANDLE) {
                throw new InlaySQLException("bad handle");
            }
            if (code != INLAYSQL_OK) {
                throw new InlaySQLException(lastError() + " — while running: " + sql);
            }
            MemorySegment json = out.get(ValueLayout.ADDRESS, 0);
            String text = json.getUtf8String(0);
            FREE.invoke(json);
            return text;
        }
    }

    /** Run a SELECT; rows as ordered maps keyed by column name. */
    public List<Map<String, Object>> query(String sql, Object... params) throws Throwable {
        String text = run(sql, params);
        JsonValue parsed = Json.parse(text);
        if (!(parsed instanceof JsonObject obj) || !obj.members.containsKey("columns")) {
            throw new InlaySQLException("not a query: " + sql);
        }
        List<Object> columns = ((JsonArray) obj.members.get("columns")).items;
        List<Map<String, Object>> rows = new ArrayList<>();
        for (JsonValue row : ((JsonArray) obj.members.get("rows")).items) {
            List<Object> cells = ((JsonArray) row).items;
            Map<String, Object> map = new LinkedHashMap<>();
            for (int i = 0; i < columns.size(); i++) {
                map.put((String) columns.get(i), cells.get(i));
            }
            rows.add(map);
        }
        return rows;
    }

    /** First row as an ordered map, or {@code null}. */
    public Map<String, Object> first(String sql, Object... params) throws Throwable {
        List<Map<String, Object>> rows = query(sql, params);
        return rows.isEmpty() ? null : rows.get(0);
    }

    @Override public void close() throws Throwable {
        CLOSE.invoke(handle);
        arena.close();
    }

    private static String lastError() throws Throwable {
        MemorySegment s = (MemorySegment) LAST_ERROR.invoke();
        return s.address() == 0 ? "<no error>" : s.getUtf8String(0);
    }

    /** Thrown with the engine's own message, verbatim — there are no numeric codes. */
    public static final class InlaySQLException extends RuntimeException {
        public InlaySQLException(String message) { super(message); }
    }

    // ---- the smallest JSON writer and reader the three result shapes need,
    // ---- so this file keeps the engine's zero-dependency rule.

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
            case Double d -> sb.append(d);
            case Float f -> sb.append(f);
            case Boolean b -> sb.append(b);
            case String s -> sb.append(quote(s));
            case double[] v -> {
                sb.append('[');
                for (int i = 0; i < v.length; i++) {
                    if (i > 0) sb.append(',');
                    sb.append(v[i]);
                }
                sb.append(']');
            }
            case float[] v -> {
                sb.append('[');
                for (int i = 0; i < v.length; i++) {
                    if (i > 0) sb.append(',');
                    sb.append(v[i]);
                }
                sb.append(']');
            }
            default -> throw new InlaySQLException(
                "unsupported parameter type: " + value.getClass().getName());
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

    /** A JSON reader for the engine's three shapes; no dependency. */
    private static final class Json {
        static JsonValue parse(String text) {
            return new Parser(text).parseValue();
        }
    }

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
                case '"': return new JsonPrimitive(parseString());
                case 'n': expect("null"); return new JsonPrimitive(null);
                case 't': expect("true"); return new JsonPrimitive(Boolean.TRUE);
                case 'f': expect("false"); return new JsonPrimitive(Boolean.FALSE);
                default: {
                    if (c != '-' && (c < '0' || c > '9')) throw error("unexpected character");
                    StringBuilder number = new StringBuilder().append(c);
                    while (pos < text.length()
                        && (Character.isDigit(text.charAt(pos))
                            || "eE.+-".indexOf(text.charAt(pos)) >= 0)) {
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

        private void expect(String literal) {
            for (char expected : literal.toCharArray()) {
                if (next(literal) != expected) throw error("expected " + literal);
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
            return new InlaySQLException("malformed engine result: " + what
                + " at offset " + pos + " in: " + text);
        }
    }
}
