<?php

/**
 * InlaySQL — the PHP client over the C ABI.
 *
 * One file, no Composer package needed: copy it into your project (or
 * `require` it from the release archive) and open a database like SQLite —
 * no server, the file is yours.
 *
 *   require 'inlaysql.php';
 *
 *   $db = InlaySQL::open('app.inlay');                         // creates if absent
 *   $db->execute('CREATE TABLE IF NOT EXISTS users (
 *       id INTEGER PRIMARY KEY, name TEXT, email TEXT)');
 *
 *   $id = $db->insert('users', ['name' => 'Ada', 'email' => 'ada@example.org']);
 *
 *   foreach ($db->query('SELECT id, name FROM users WHERE id > :after', ['after' => 0]) as $user) {
 *       echo $user['name'];                                     // rows as arrays
 *   }
 *   $ada   = $db->first('SELECT * FROM users WHERE id = ?', [$id]);   // one row or null
 *   $count = $db->value('SELECT COUNT(*) FROM users');                // one cell
 *   $names = $db->column('SELECT name FROM users ORDER BY name');    // one column
 *
 *   $result = $db->execute('UPDATE users SET name = ? WHERE id = ?', ['Ada L.', $id]);
 *   $result->rowsAffected;                                      // 1
 *
 *   $db->transaction(function (InlaySQL $db) {                 // BEGIN … COMMIT,
 *       $db->insert('users', ['name' => 'Grace']);              // ROLLBACK on throw,
 *       $db->insert('users', ['name' => 'Linus']);              // retried on a write
 *   });                                                         // conflict
 *
 * Parameters: positional `?` with a list, or named `:name` with a string-keyed
 * array. A PHP array of numbers binds as a vector, so a retrieval call is
 *
 *   $db->query('SELECT id, vector_score(embedding, ?) AS s FROM docs ORDER BY s LIMIT 10',
 *              [$embedding]);
 *
 * Errors are exceptions: `InlaySQLConstraintException` (unique/not-null…),
 * `InlaySQLConflictException` (another writer committed first — retry),
 * `InlaySQLUnsupportedException` (a clause the engine refuses rather than
 * ignores), and `InlaySQLException` for everything else; the engine's own
 * message is the exception message. PHP 8.1+ (FFI is built in; enable
 * `ffi.enable` for FPM).
 *
 * Read-only: `InlaySQL::open('app.inlay', readonly: true)` — the file must
 * already exist and every write is refused.
 *
 * One handle is one thread at a time; under PHP-FPM that is one handle per
 * worker, and every worker may write — concurrent commits to one file are
 * what this engine does that SQLite does not. The FFI definitions are bound
 * once per process and shared by every handle. With opcache preloading
 * (`ffi.enable=preload`), preload this file.
 *
 * Vector cells come back as the placeholder "<vector(n)>" — the raw floats do
 * not cross the boundary in JSON.
 *
 * Tested against libinlaysql_ffi from inlaySQL/inlaysql v0.0.6; the C surface
 * it wraps is documented in include/inlaysql.h beside this file.
 */

declare(strict_types=1);

final class InlaySQL
{
    private const CDEF = <<<'C'
        typedef struct InlaysqlHandle InlaysqlHandle;
        InlaysqlHandle *inlaysql_open(const char *path);
        InlaysqlHandle *inlaysql_open_read_only(const char *path);
        void inlaysql_close(InlaysqlHandle *handle);
        int inlaysql_exec(InlaysqlHandle *handle, const char *sql,
                          const char *params, char **out_json);
        const char *inlaysql_last_error(void);
        void inlaysql_free_string(char *s);
        const char *inlaysql_version(void);
    C;

    /** @var array<string, \FFI> one binding per library path, per process */
    private static array $bindings = [];

    private \FFI $ffi;
    /** @var \FFI\CData|null */
    private $handle;
    private int $depth = 0;

    /**
     * Open a database file, creating it when absent.
     *
     * @param string      $path     the database file
     * @param bool        $readonly open read-only; the file must already exist
     * @param string|null $lib      path to libinlaysql_ffi.dylib/.so — default:
     *                              this file's directory, its parent, then the
     *                              working directory
     */
    public static function open(string $path, bool $readonly = false, ?string $lib = null): self
    {
        return new self($path, $lib, $readonly);
    }

    public function __construct(string $path, ?string $lib = null, bool $readonly = false)
    {
        $this->ffi = self::binding($lib ?? self::locateLibrary());

        $open = $readonly ? 'inlaysql_open_read_only' : 'inlaysql_open';
        $handle = $this->ffi->{$open}($path);
        if (\FFI::isNull($handle)) {
            throw self::error('open failed: ' . $this->lastError());
        }
        $this->handle = $handle;
    }

    public function __destruct()
    {
        $this->close();
    }

    /** Close the handle. Safe to call twice; every later call throws. */
    public function close(): void
    {
        if ($this->handle !== null) {
            $this->ffi->inlaysql_close($this->handle);
            $this->handle = null;
        }
    }

    /** The engine's version string. */
    public function version(): string
    {
        return $this->ffi->inlaysql_version();
    }

    // ---- statements --------------------------------------------------------

    /**
     * Run any statement. For a write, the result says how many rows and which
     * id; for a query, prefer {@see query()}.
     *
     * @param list<mixed>|array<string, mixed> $params `?` in order, or `:name`
     */
    public function execute(string $sql, array $params = []): InlaySQLResult
    {
        $raw = $this->run($sql, $params);
        if (isset($raw['columns'])) {
            return new InlaySQLResult(0, null, false, new InlaySQLRows($raw['columns'], $raw['rows']));
        }
        return new InlaySQLResult(
            (int) ($raw['rows'] ?? 0),
            isset($raw['last_insert_id']) ? (int) $raw['last_insert_id'] : null,
            ($raw['kind'] ?? '') === 'ddl',
            null,
        );
    }

    /**
     * Run a query. Iterate the result, or ask it for `->all()`, `->first()`,
     * `->column()`, `->value()`.
     *
     * @param list<mixed>|array<string, mixed> $params
     */
    public function query(string $sql, array $params = []): InlaySQLRows
    {
        $raw = $this->run($sql, $params);
        if (!isset($raw['columns'])) {
            throw self::error("not a query: $sql");
        }
        return new InlaySQLRows($raw['columns'], $raw['rows']);
    }

    /** First row as an associative array, or null. */
    public function first(string $sql, array $params = []): ?array
    {
        return $this->query($sql, $params)->first();
    }

    /** The first cell of the first row, or null when there is no row. */
    public function value(string $sql, array $params = []): mixed
    {
        return $this->query($sql, $params)->value();
    }

    /** One column across every row. @return list<mixed> */
    public function column(string $sql, array $params = [], int|string $column = 0): array
    {
        return $this->query($sql, $params)->column($column);
    }

    /**
     * Insert one row given as `column => value` and return its row id.
     *
     * @param array<string, mixed> $row
     */
    public function insert(string $table, array $row): int
    {
        if ($row === []) {
            throw self::error("insert into $table: no columns given");
        }
        $columns = implode(', ', array_map([self::class, 'quoteIdentifier'], array_keys($row)));
        $marks = implode(', ', array_fill(0, count($row), '?'));
        $result = $this->execute(
            'INSERT INTO ' . self::quoteIdentifier($table) . " ($columns) VALUES ($marks)",
            array_values($row),
        );
        if ($result->lastInsertId === null) {
            throw self::error("insert into $table reported no row id");
        }
        return $result->lastInsertId;
    }

    // ---- transactions -----------------------------------------------------

    /**
     * Run `$fn($this)` inside BEGIN … COMMIT. A throw rolls back and rethrows.
     * A write conflict (another handle committed first) rolls back and reruns
     * `$fn`, up to `$retries` times, so `$fn` must be safe to repeat. Nested
     * calls join the outer transaction.
     *
     * @template T
     * @param callable(InlaySQL): T $fn
     * @return T
     */
    public function transaction(callable $fn, int $retries = 3): mixed
    {
        if ($this->depth > 0) {
            $this->depth++;
            try {
                return $fn($this);
            } finally {
                $this->depth--;
            }
        }

        $attempt = 0;
        while (true) {
            $this->run('BEGIN');
            $this->depth = 1;
            try {
                $value = $fn($this);
                $this->run('COMMIT');
                $this->depth = 0;
                return $value;
            } catch (\Throwable $e) {
                $this->depth = 0;
                $this->rollbackQuietly();
                if ($e instanceof InlaySQLConflictException && $attempt++ < $retries) {
                    continue;
                }
                throw $e;
            }
        }
    }

    /** Whether a `transaction()` is open on this handle. */
    public function inTransaction(): bool
    {
        return $this->depth > 0;
    }

    private function rollbackQuietly(): void
    {
        try {
            $this->run('ROLLBACK');
        } catch (\Throwable) {
            // A conflict at COMMIT already ended the transaction; nothing to undo.
        }
    }

    // ---- the raw call -----------------------------------------------------

    /**
     * Run one statement and return the ABI's JSON, decoded:
     * `{"kind":"ddl"}`, `{"kind":"written","rows":n,"last_insert_id":k}`, or
     * `{"columns":[…],"rows":[[…],…]}`. The typed methods above are built on
     * this; it stays public for callers that want the raw shape.
     *
     * @param list<mixed>|array<string, mixed> $params
     */
    public function run(string $sql, array $params = []): array
    {
        if ($this->handle === null) {
            throw self::error('the database is closed');
        }
        if ($params !== [] && !array_is_list($params)) {
            [$sql, $params] = self::bindNamed($sql, $params);
        }

        $out = $this->ffi->new('char *');
        $code = $this->ffi->inlaysql_exec(
            $this->handle,
            $sql,
            $params === [] ? null : json_encode($params, JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR),
            \FFI::addr($out),
        );
        if ($code !== 0) {
            throw self::error($this->lastError(), $sql);
        }
        try {
            return json_decode(\FFI::string($out), true, flags: JSON_THROW_ON_ERROR);
        } finally {
            $this->ffi->inlaysql_free_string($out);
        }
    }

    // ---- helpers ----------------------------------------------------------

    /**
     * Rewrite `:name` placeholders to `?` in the order they appear, skipping
     * string literals and quoted identifiers, and order the values to match.
     *
     * @param array<string, mixed> $params
     * @return array{string, list<mixed>}
     */
    private static function bindNamed(string $sql, array $params): array
    {
        $out = '';
        $values = [];
        $len = strlen($sql);
        $i = 0;
        while ($i < $len) {
            $c = $sql[$i];
            if ($c === "'" || $c === '"' || $c === '`') {
                // Copy a quoted run verbatim; a doubled quote stays inside it.
                $j = $i + 1;
                while ($j < $len) {
                    if ($sql[$j] === $c) {
                        if ($j + 1 < $len && $sql[$j + 1] === $c) {
                            $j += 2;
                            continue;
                        }
                        break;
                    }
                    $j++;
                }
                $out .= substr($sql, $i, $j - $i + 1);
                $i = $j + 1;
                continue;
            }
            if ($c === ':' && $i + 1 < $len && (ctype_alpha($sql[$i + 1]) || $sql[$i + 1] === '_')) {
                $j = $i + 1;
                while ($j < $len && (ctype_alnum($sql[$j]) || $sql[$j] === '_')) {
                    $j++;
                }
                $name = substr($sql, $i + 1, $j - $i - 1);
                if (!array_key_exists($name, $params)) {
                    throw self::error("no value bound for :$name", $sql);
                }
                $values[] = $params[$name];
                $out .= '?';
                $i = $j;
                continue;
            }
            $out .= $c;
            $i++;
        }
        return [$out, $values];
    }

    private static function quoteIdentifier(string $name): string
    {
        return '"' . str_replace('"', '""', $name) . '"';
    }

    private function lastError(): string
    {
        return $this->ffi->inlaysql_last_error();
    }

    /** The engine's message, as the exception class its prefix names. */
    private static function error(string $message, ?string $sql = null): InlaySQLException
    {
        $text = $sql === null ? $message : "$message — while running: $sql";
        return match (true) {
            str_starts_with($message, 'constraint failed') => new InlaySQLConstraintException($text),
            str_starts_with($message, 'write conflict') => new InlaySQLConflictException($text),
            str_starts_with($message, 'unsupported') => new InlaySQLUnsupportedException($text),
            default => new InlaySQLException($text),
        };
    }

    private static function binding(string $lib): \FFI
    {
        return self::$bindings[$lib] ??= \FFI::cdef(self::CDEF, $lib);
    }

    /** Find the library beside this file, its parent, then the working directory. */
    private static function locateLibrary(): string
    {
        $name = match (PHP_OS_FAMILY) {
            'Darwin' => 'libinlaysql_ffi.dylib',
            'Windows' => 'inlaysql_ffi.dll',
            default => 'libinlaysql_ffi.so',
        };
        foreach ([__DIR__, dirname(__DIR__), getcwd() ?: '.'] as $dir) {
            $candidate = $dir . DIRECTORY_SEPARATOR . $name;
            if (is_file($candidate)) {
                return $candidate;
            }
        }
        throw new InlaySQLException(
            "could not find $name beside " . __DIR__ . " or the working directory" .
            " — pass the path as `lib:`, or download it from" .
            " https://github.com/inlaySQL/inlaysql/releases"
        );
    }
}

/** What a statement did. `rows` is set only when the statement was a query. */
final class InlaySQLResult
{
    public function __construct(
        public readonly int $rowsAffected,
        /** The most recent INSERT's row id on this handle (SQLite's `last_insert_rowid()`), or null. */
        public readonly ?int $lastInsertId,
        public readonly bool $isDdl,
        public readonly ?InlaySQLRows $rows,
    ) {
    }
}

/**
 * A query's answer. Iterates as associative arrays; `count()` is the row
 * count; the accessors below answer the common shapes without a loop.
 *
 * @implements \IteratorAggregate<int, array<string, mixed>>
 */
final class InlaySQLRows implements \IteratorAggregate, \Countable
{
    /**
     * @param list<string>      $columns
     * @param list<list<mixed>> $rows
     */
    public function __construct(
        public readonly array $columns,
        private readonly array $rows,
    ) {
    }

    public function getIterator(): \Generator
    {
        foreach ($this->rows as $row) {
            yield array_combine($this->columns, $row);
        }
    }

    public function count(): int
    {
        return count($this->rows);
    }

    /** Every row as an associative array. @return list<array<string, mixed>> */
    public function all(): array
    {
        return iterator_to_array($this, false);
    }

    /** Every row as an object (`$row->name`). @return list<object> */
    public function objects(): array
    {
        return array_map(static fn (array $row): object => (object) $row, $this->all());
    }

    /** The rows exactly as the ABI returned them, positional. @return list<list<mixed>> */
    public function raw(): array
    {
        return $this->rows;
    }

    /** First row as an associative array, or null. */
    public function first(): ?array
    {
        return isset($this->rows[0]) ? array_combine($this->columns, $this->rows[0]) : null;
    }

    /** The first cell of the first row, or null when there is no row. */
    public function value(): mixed
    {
        return $this->rows[0][0] ?? null;
    }

    /** One column, by index or name, across every row. @return list<mixed> */
    public function column(int|string $column = 0): array
    {
        $index = is_int($column) ? $column : array_search($column, $this->columns, true);
        if ($index === false || !isset($this->columns[$index])) {
            throw new InlaySQLException("no such column: $column");
        }
        return array_column($this->rows, $index);
    }
}

class InlaySQLException extends RuntimeException
{
}

/** `UNIQUE`, `NOT NULL`, `CHECK`, `FOREIGN KEY` — the row was refused. */
final class InlaySQLConstraintException extends InlaySQLException
{
}

/** Another handle committed first; nothing was written. Safe to retry. */
final class InlaySQLConflictException extends InlaySQLException
{
}

/** A clause this engine refuses rather than silently ignores. */
final class InlaySQLUnsupportedException extends InlaySQLException
{
}
