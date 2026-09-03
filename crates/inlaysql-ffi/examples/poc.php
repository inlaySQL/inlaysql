<?php

/**
 * InlaySQL from PHP, through the C ABI — a proof of concept.
 *
 * This is the "SQLite-like adapter" direction: no server, the file opened
 * in-process through PHP's built-in FFI ext. It is what a `pdo_inlaysql`
 * extension would wrap, and what the Laravel driver in the docs would call.
 *
 * Build the library first (repo root):
 *   cargo build -p inlaysql-ffi --release
 *
 * Run this:
 *   php examples/ffi/poc.php [path/to/libinlaysql.dylib|.so|.dll] [db path]
 *
 * Requires PHP 7.4+ with the FFI extension (bundled, not enabled by default).
 * Preload/FPM needs ffi.enabled=preload with the definitions in a preloaded
 * file; the CLI here uses the simple embed mode.
 */

if ($argc < 2) {
    fwrite(STDERR, "usage: php poc.php <libinlaysql> [db-path]\n");
    fwrite(STDERR, "  <libinlaysql>  path to libinlaysql.dylib / .so / .dll\n");
    exit(1);
}

$lib = $argv[1];
$dbPath = $argv[2] ?? sys_get_temp_dir() . '/inlaysql-php-poc.inlay';

if (!file_exists($lib)) {
    fwrite(STDERR, "library not found: $lib\n");
    exit(1);
}

// The definitions mirror crates/inlaysql-ffi/include/inlaysql.h. Kept in one
// place here; an extension or packaged driver would ship them as a .h beside
// the library.
$ffi = FFI::cdef(<<<'C'
    typedef struct InlaysqlHandle InlaysqlHandle;

    InlaysqlHandle *inlaysql_open(const char *path);
    InlaysqlHandle *inlaysql_open_read_only(const char *path);
    void inlaysql_close(InlaysqlHandle *handle);

    int inlaysql_exec(InlaysqlHandle *handle,
                      const char *sql,
                      const char *params,
                      char **out_json);

    const char *inlaysql_last_error(void);
    void inlaysql_free_string(char *s);
    const char *inlaysql_version(void);
C, $lib);

/** Run one statement; returns the decoded JSON, or throws with the engine's error. */
function inlaysql_exec(object $ffi, object $handle, string $sql, array $params = []): array
{
    $paramsJson = $params === [] ? null : json_encode($params, JSON_UNESCAPED_SLASHES);
    $out = $ffi->new('char *');

    $code = $ffi->inlaysql_exec(
        $handle,
        $sql,
        $paramsJson,
        FFI::addr($out),
    );

    if ($code !== 0) {
        // `inlaysql_last_error` returns `const char*`; PHP's FFI surfaces a
        // plain `char*` return as a native string, so no FFI::string dance.
        $err = $ffi->inlaysql_last_error();
        $message = $code === 2
            ? 'bad handle'
            : (is_string($err) ? $err : FFI::string($err));
        throw new RuntimeException("InlaySQL error ($code): $message — while running: $sql");
    }

    // `out` however is a `char**` we own, so its pointee arrives as FFI\CData.
    $json = is_string($out) ? $out : FFI::string($out);
    $ffi->inlaysql_free_string($out);

    return json_decode($json, true, flags: JSON_THROW_ON_ERROR);
}

// `inlaysql_version` returns a static `const char*`; PHP's FFI surfaces a
// return of that type as a raw string, not a pointer — `FFI::string` is only
// for pointers we own an FFI\CData view of.
$version = $ffi->inlaysql_version();
echo "InlaySQL engine version $version\n";
echo "database: $dbPath\n\n";

// Open (creating if needed). Close is PHP-GC driven: when $handle dies the
// FFI handle is not automatically freed, so a real driver pairs this with an
// object that calls inlaysql_close in __destruct.
$handle = $ffi->inlaysql_open($dbPath);
if (FFI::isNull($handle)) {
    fwrite(STDERR, 'open failed: ' . FFI::string($ffi->inlaysql_last_error()) . "\n");
    exit(1);
}

try {
    $r = inlaysql_exec($ffi, $handle, 'CREATE TABLE IF NOT EXISTS docs (
        id INTEGER PRIMARY KEY,
        title TEXT,
        body TEXT
    )');
    echo "create:      {$r['kind']}\n";

    $r = inlaysql_exec($ffi, $handle, 'INSERT INTO docs (title, body) VALUES (?, ?)', [
        'Hello', 'from PHP over the C ABI — no server, no MySQL, the file is ours.',
    ]);
    echo "insert:      {$r['kind']}, {$r['rows']} row\n";

    $r = inlaysql_exec($ffi, $handle, 'SELECT id, title, body FROM docs ORDER BY id');
    echo "select:      {$r['rows'][0][0]} {$r['rows'][0][1]}\n";
    echo "             {$r['rows'][0][2]}\n";

    // Bound parameters carry the same rules as every other surface: strings,
    // numbers, null — and nested number arrays as vectors.
    $r = inlaysql_exec($ffi, $handle, 'SELECT COUNT(*) AS n FROM docs');
    echo "count:       {$r['rows'][0][0]}\n";

    // And errors surface the engine's own words:
    try {
        inlaysql_exec($ffi, $handle, 'SELECT * FROM no_such_table');
    } catch (RuntimeException $e) {
        echo "error path:  {$e->getMessage()}\n";
    }
} finally {
    $ffi->inlaysql_close($handle);
}

echo "\nOK — PHP drove the engine in-process through the C ABI.\n";
