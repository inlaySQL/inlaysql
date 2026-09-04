<?php

/**
 * InlaySQL — the PHP wrapper over the C ABI.
 *
 * One file, no Composer package needed: copy it into your project (or
 * `require` it from the release archive) and open a database like SQLite —
 * no server, the file is yours.
 *
 *   require 'inlaysql.php';
 *   $db = new InlaySQL('app.inlay');                  // creates if absent
 *   $db->run('CREATE TABLE IF NOT EXISTS users (
 *       id INTEGER PRIMARY KEY, name TEXT)');
 *   $db->run('INSERT INTO users (name) VALUES (?)', ['Ada']);
 *   $rows = $db->run('SELECT * FROM users');          // ['columns'=>…, 'rows'=>…]
 *   foreach ($rows['rows'] as $row) { /* [1, 'Ada'] *\/ }
 *
 * Or use the object-row accessor:
 *   foreach ($db->query('SELECT id, name FROM users') as $user) {
 *       echo $user->name;                             // rows as objects
 *   }
 *
 * Read-only: `new InlaySQL('app.inlay', readonly: true)` — the file must
 * already exist and every write is refused.
 *
 * One handle is one thread at a time (open one per worker/thread), the same
 * rule SQLite's default connection has. Vector parameters bind as a plain
 * PHP array of numbers; vector cells come back as the placeholder
 * "<vector(n)>" — the raw floats do not cross the boundary in JSON.
 *
 * Tested against libinlaysql_ffi from inlaySQL/inlaysql v0.0.1; the C surface
 * it wraps is documented in include/inlaysql.h beside this file.
 */

declare(strict_types=1);

final class InlaySQL
{
    private \FFI $ffi;
    /** @var \FFI\CData */
    private $handle;

    /**
     * @param string $path     the database file; created when absent
     * @param string|null $lib path to libinlaysql_ffi.dylib/.so — default:
     *                         this file's directory, then the working
     *                         directory
     * @param bool $readonly  open read-only; the file must already exist
     */
    public function __construct(
        string $path,
        ?string $lib = null,
        bool $readonly = false,
    ) {
        $lib ??= self::locateLibrary();

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
        C, $lib);

        $open = $readonly ? 'inlaysql_open_read_only' : 'inlaysql_open';
        $this->handle = $this->ffi->{$open}($path);
        if (\FFI::isNull($this->handle)) {
            throw new InlaySQLException('open failed: ' . $this->lastError());
        }
    }

    public function __destruct()
    {
        $this->ffi->inlaysql_close($this->handle);
    }

    /** The engine's version string. */
    public function version(): string
    {
        return $this->ffi->inlaysql_version();
    }

    /**
     * Run one statement.
     *
     * @param array $params bound to `?` in order; an array of numbers is a
     *                      vector parameter
     *
     * @return array{"kind": "ddl"} |
     *               array{"kind": "written", "rows": int} |
     *               array{"columns": list<string>, "rows": list<list<mixed>>}
     */
    public function run(string $sql, array $params = []): array
    {
        $out = $this->ffi->new('char *');
        $code = $this->ffi->inlaysql_exec(
            $this->handle,
            $sql,
            $params === [] ? null : json_encode($params, JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR),
            \FFI::addr($out),
        );
        if ($code !== 0) {
            throw new InlaySQLException($this->lastError() . " — while running: $sql");
        }
        $result = json_decode(\FFI::string($out), true, flags: JSON_THROW_ON_ERROR);
        $this->ffi->inlaysql_free_string($out);
        return $result;
    }

    /**
     * Run a SELECT; rows as objects keyed by column name, so
     * `$row['name']`… actually `$row->name` works and a vector column is
     * omitted (its placeholder is not data).
     *
     * @return list<object>
     */
    public function query(string $sql, array $params = []): array
    {
        $result = $this->run($sql, $params);
        if (!isset($result['columns'])) {
            throw new InlaySQLException("not a query: $sql");
        }
        $rows = [];
        $columns = $result['columns'];
        foreach ($result['rows'] as $row) {
            $rows[] = (object) array_combine($columns, $row);
        }
        return $rows;
    }

    /** First row as an object, or null. */
    public function first(string $sql, array $params = []): ?object
    {
        return $this->query($sql, $params)[0] ?? null;
    }

    private function lastError(): string
    {
        return $this->ffi->inlaysql_last_error();
    }

    /** Find the library beside this file, then in the working directory. */
    private static function locateLibrary(): string
    {
        $names = PHP_OS_FAMILY === 'Darwin'
            ? ['libinlaysql_ffi.dylib']
            : (PHP_OS_FAMILY === 'Windows'
                ? ['inlaysql_ffi.dll']
                : ['libinlaysql_ffi.so']);
        foreach ($names as $name) {
            foreach ([__DIR__, getcwd() ?: '.'] as $dir) {
                $candidate = $dir . DIRECTORY_SEPARATOR . $name;
                if (is_file($candidate)) {
                    return $candidate;
                }
            }
        }
        throw new InlaySQLException(
            "could not find " . implode('|', $names) .
            " beside " . __DIR__ . " or the working directory — pass the path" .
            " as the second constructor argument, or download it from" .
            " https://github.com/inlaySQL/inlaysql/releases"
        );
    }
}

final class InlaySQLException extends RuntimeException
{
}
