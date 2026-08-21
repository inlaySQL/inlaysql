# MySQL server mode

```sh
inlaysql serve --mysql app.inlay --password-env INLAYSQL_PASSWORD
```

`inlaysql-server` speaks the MySQL wire protocol over one InlaySQL database
file, so a client that already knows how to talk to MySQL — `mysql`, PDO,
mysqli, JDBC, `mysql2` — can talk to this instead.

**Read [What does not work yet](#what-does-not-work-yet) before you plan
anything around it.** The protocol is complete enough for real clients; the SQL
surface underneath it still has real gaps. **A stock Laravel 11 skeleton now
runs for real**: `composer create-project laravel/laravel`, `.env` pointed at
`inlaysql serve --mysql`, `php artisan migrate` completes the default
`users`/`cache`/`jobs` migrations plus a `posts` table with a foreign key, and
ordinary Eloquent traffic afterward — `create`/`find`/`update`/`delete`,
`whereIn`, a raw `JOIN`, `whereHas`, `withCount`, eager loading, `paginate()`
and `upsert()` — all work. This found two real bugs in the shim, both fixed:
`EXISTS (SELECT ... FROM information_schema...)` (the exact shape
`hasTable()`/`hasColumn()` compile to) was misrouted by a heuristic the
subquery's own `schema()` call fooled, and a bare literal in an
`information_schema` projection (`SELECT 1 FROM ...`, the other existence-check
idiom) was rejected as an unresolvable column. It also confirmed a documented,
deliberate limitation is real in practice: Laravel's `->foreignId()->constrained()`
compiles to a standalone `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY`,
which this server cannot record after the fact — see
["ADD CONSTRAINT ... FOREIGN KEY" below](#mysql-only-ddl-is-translated-not-invented)
for why, and declare the foreign key inside the initial `Schema::create()`
if you need it recorded. [What does not work yet](#what-does-not-work-yet)
says which statements still stop, and [Divergences](#divergences) says where this server
accepts a statement and means something slightly different by it.

---

## Security

Four things, none of them optional reading.

**It is plaintext. There is no TLS in this version.** Every statement, every
result and every credential crosses the connection in the clear. `CLIENT_SSL`
is never advertised, so a client cannot negotiate encryption and then be
silently downgraded to none — it is told up front, and `SHOW VARIABLES LIKE
'have_ssl'` answers `DISABLED`. Do not run this across a network you do not
already trust.

**It binds `127.0.0.1` by default.** Reaching the network takes an explicit
`--bind`, and doing so prints a warning that names the risk. A database server
that defaults to every interface is a liability.

**There is one user and one password, and no user table.** No grants, no
per-table permissions, no roles. The password comes from `--password-env`
(preferred) or `--password` (visible in the machine's process list, and warned
about). It is never logged, and neither is any statement.

**Authentication is `caching_sha2_password` by default, or
`mysql_native_password` via `AuthSwitchRequest` (AHL-467).** MySQL 8+
clients default to the former; PHP's PDO and the `mysql` CLI still complete
the latter directly the moment they name it, so nothing that already worked
stops working. Both are challenge-response, so the password itself does not
cross the wire on the *fast* path even though the channel is unencrypted —
the challenge is 20 bytes of `/dev/urandom`, and the server refuses to start
a connection rather than fall back to a guessable one. That protects the
*password*. It does not protect the *data*, which is still in the clear —
and, on `caching_sha2_password`'s full-authentication fallback, the password
too: a client with nothing to attempt the fast scramble with sends the
password in the clear instead, over an already-plaintext connection.
Accepting that is a deliberate, narrow call — v1 is documented
plaintext-localhost from the top of this file down, so a cleartext password
crossing this connection reveals nothing to a network observer that every
other statement on it does not already reveal. The RSA public-key exchange a
real client would otherwise fall back to on an unencrypted connection is not
implemented; a client that asks for it is refused with `1235` naming what it
cannot do, rather than silently mishandled or hung.

A rejected login gets a proper `1045 / 28000` error rather than a closed
socket, and the message does not say which half of the credential was wrong.

---

## Options

| Flag | Default | What it does |
| --- | --- | --- |
| `--bind <addr>` | `127.0.0.1` | Address to listen on. Anything else warns. |
| `--port <n>` | `3306` | `0` asks the OS for a free port and prints it. |
| `--user <name>` | `root` | The single accepted user. |
| `--password <pw>` | empty | Visible to `ps`. Prefer the next one. |
| `--password-env <VAR>` | — | Read the password from the environment. |
| `--max-connections <n>` | `64` | Beyond this, clients get `1040`. |

An empty password means an empty password: any client that can reach the port
can read and write the database. The server says so at startup.

---

## How it is built

Two decisions from `docs/architecture.md`, and one dependency choice.

### D1 — the MySQL dialect is a shim, never a change to the engine

`inlaysql-core` speaks SQLite's dialect and keeps speaking it. Statements that
are MySQL-shaped rather than SQL-shaped — session `SET`s, `SHOW`,
`information_schema` — are recognised in this crate and answered from
`Catalog`. Nothing here adds syntax to the engine, and everything not
recognised is passed through for the engine to accept or refuse on its own
terms.

The rule the shim is built around: **a wrong metadata answer is worse than no
answer.** A tool asking "does column `email` exist on `users`?" and getting
every column of every table back will conclude that it does. So a filter the
shim cannot evaluate is an error naming the filter, never an ignored filter and
never an empty result standing in for "I did not understand".

### D2 — thread-per-connection, one handle each

`std::net::TcpListener`, one OS thread per connection, and each thread opens
its own `Database` on the same file. The engine is `!Send` by design, several
handles on one file already settle concurrent commits with first-committer-wins,
and a handle re-reads committed state at the start of every statement outside an
explicit transaction — so one connection sees another's commits with no
coordination in this crate at all.

**There is no async runtime here, and no `Send` bound anywhere.** The workspace
has zero async dependencies and this is not the crate that changes that.

A lost write race arrives at the client as `1213` (deadlock), whose documented
remedy is to retry the transaction — which is exactly the correct response to
first-committer-wins, and what every ORM's retry logic already recognises.

### The dependency choice: hand-rolled, not `msql-srv`

`msql-srv` is the obvious candidate and it was measured rather than assumed:

```
$ cargo add msql-srv
      Adding msql-srv v0.11.0
             Features: + rustls + tls
     Locking 190 packages
```

**190 packages** — rustls, chrono, regex, nom and the rest — for a protocol
subset that is a few hundred lines. That is the same trade `inlaysql-mcp`
already refused when it hand-rolled JSON-RPC rather than take the Tokio-based
MCP SDK, and for the same reason: this project's proposition is one file and a
small dependency tree. The framing here is written out instead, and
`crates/inlaysql-server/Cargo.toml` depends on `inlaysql` and nothing else.

SHA-1 and SHA-256 are written out for the same reason (`sha1`/`sha2` bring
several crates behind them each). Both are fixed, fully specified functions,
each checked against its own published FIPS/RFC vectors, and each is used
only as the key-derivation step its plugin specifies — not as a signature
primitive, where SHA-1's breaks would be indefensible.
`caching_sha2_password`'s scramble (AHL-467) —
`XOR(SHA256(password), SHA256(SHA256(SHA256(password)) || scramble))` — was
checked against MySQL's own server source
(`sql/auth/sha2_password_common.cc`) and an independent client implementation
(`go-sql-driver/mysql`) for the one detail that is easy to get quietly
backwards: the concatenation order, which is the *opposite* of
`mysql_native_password`'s own.

`inlaysql-core` gains nothing from any of this. Its dependency tree is
unchanged, which is what the `determinism` CI job polices.

---

## What is implemented

### Protocol

| Command | Status |
| --- | --- |
| Handshake v10 + `caching_sha2_password` (default) + `mysql_native_password` | yes, including `AuthSwitchRequest` and `caching_sha2_password`'s full-authentication fallback (AHL-467) |
| `COM_QUERY` | yes, text result sets |
| `COM_PING` | yes |
| `COM_INIT_DB` | yes |
| `COM_QUIT` | yes |
| `COM_STMT_PREPARE` | yes |
| `COM_STMT_EXECUTE` | yes, binary parameters and binary result sets |
| `COM_STMT_CLOSE` / `COM_STMT_RESET` | yes |
| `COM_FIELD_LIST` | refused with `1047` — use `SHOW COLUMNS` |
| TLS | no, and not advertised |
| Multi-statement / multi-result | no, and not advertised |
| `LOAD DATA LOCAL INFILE` | no, and not advertised |

OK packets carry `affected_rows`, `last_insert_id` and a **warning count**. An
`INSERT` that lets the engine assign the key reports it; one that supplies its
own does not, which is MySQL's rule as well as the engine's. The warning count
is how the server says "this succeeded, but not exactly as you wrote it" — see
[the DDL translation](#mysql-only-ddl-is-translated-not-invented) — and
`SHOW WARNINGS` lists them. Messages larger than one packet are split and
reassembled in both directions, including the empty terminating packet an exact
multiple needs.

Result-set column types are unified across every row before any of them are
sent — all integers make an integer column, integers and reals make a real one,
anything else falls back to text. The engine is dynamically typed the way SQLite
is, and the binary protocol has no room for a column that changes type halfway
down.

#### Prepared-statement column metadata (AHL-466)

`COM_STMT_PREPARE` reports the statement's real output columns now.
`inlaysql::Statement::columns()` exposes the projection `inlaysql-core`'s
planner already resolved at prepare time — names always, and a declared type
wherever an item projects a stored column directly (`None` for a computed
expression, a retrieval score, or a `SELECT` with no `FROM`, the same line
SQLite itself draws: `sqlite3_column_decltype` answers `NULL` for an
expression too). The prepare reply's column-definition packets are built
from it, with `None` falling back to the same "text represents anything"
default an all-`NULL` executed column gets.

This only covers what the engine plans. A statement the shim answers instead
— `SHOW`, `information_schema`, a session `SET` — has no equivalent "plan
without running it" step, so `COM_STMT_PREPARE` still reports zero columns
for those, and the real metadata still arrives with the result set at
execute time, which PDO (emulated and native), mysqli and the tests all
handle either way.

### The shim

Answered from `Catalog` and session state, never sent to the engine:

- `SET NAMES`, `SET CHARACTER SET`, `SET TRANSACTION`, and session/global/user
  variable assignment. Recorded and readable back; inert otherwise — **except
  `autocommit`, which really does change when work commits.**
- `SELECT VERSION()`, `DATABASE()`, `SCHEMA()`, `LAST_INSERT_ID()`,
  `CONNECTION_ID()`, `USER()`, `CURRENT_USER()`, `@@variables` in every
  spelling (`@@x`, `@@session.x`, `@@global.x`, `SESSION x`), `@user_variables`.
- `SHOW TABLES`, `SHOW FULL TABLES`, `SHOW COLUMNS` / `FIELDS`, `SHOW FULL
  COLUMNS`, `DESCRIBE` / `DESC`, `SHOW KEYS` / `INDEX`, `SHOW VARIABLES`,
  `SHOW STATUS`, `SHOW WARNINGS` / `ERRORS`, `SHOW DATABASES`, `SHOW ENGINES`,
  `SHOW TABLE STATUS`, `SHOW CREATE TABLE`, `SHOW CREATE DATABASE` — all with
  `LIKE` patterns, including escaped wildcards.
- `information_schema.TABLES`, `.COLUMNS`, `.SCHEMATA`, `.STATISTICS`, with a
  projection, a conjunction of `= != LIKE IN IS NULL` comparisons, `ORDER BY`,
  `LIMIT`/`OFFSET`, and `COUNT(*)`. Bound parameters resolve as values inside
  the comparison — they are never spliced into SQL text, so there is no
  injection path through the shim's own parsing.
- `USE`, `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK`, `DO`.
- Comments are stripped first, quote-aware, including MySQL's `/*!40101 ... */`
  version gates.

Things the shim refuses rather than fakes:

- **`SAVEPOINT`, `ROLLBACK TO SAVEPOINT`, `RELEASE SAVEPOINT`** → `1235`. This
  is how an ORM spells a nested transaction, and the engine has none. Answering
  OK would make an inner rollback silently keep its writes.
- `SHOW TABLES ... WHERE`, `information_schema` joins, `OR` in a shim `WHERE`,
  unknown `information_schema` views, comparisons it cannot evaluate → `1235`,
  naming what was not understood.
- `mysql`, `performance_schema` and `sys` as schema names → `1044`, rather than
  pretending they exist.

Reported honestly rather than flatteringly: `have_ssl` is `DISABLED`,
`foreign_key_checks` is `0` (there is no enforcement), every column is
`NULL`-able with no default (the engine refuses `NOT NULL` and `DEFAULT`), and
`TABLE_ROWS` is `NULL` — unknown — rather than `0`, which would read as empty.

### MySQL-named scalar functions are mapped, not guessed

`crates/inlaysql-server/src/mysqlfunc.rs`. The engine's scalar functions carry
SQLite's names and SQLite's semantics — `length`, `substr`, `instr`,
`datetime('now')`, `random`. A MySQL client sends `CHAR_LENGTH`, `LEFT`,
`LOCATE`, `NOW`, `RAND`. Each call site is rewritten, before the statement
reaches the engine, into an expression built only from functions the engine
already has. **Nothing here adds a function to the engine**, which is what D1
forbids.

**A name is mapped only when the mapping was checked against a real MySQL and
found to be exact.** Every mapping below was compared against **MySQL 8.4.11**
over a table of NULLs, empty strings, zero, negative and out-of-range
arguments, and multi-byte UTF-8, and agreed on every one.

| MySQL | Becomes | The corner that decided it |
| --- | --- | --- |
| `CONCAT(a, …)` | `('' \|\| a \|\| …)` | Both propagate NULL — `CONCAT('a',NULL,'c')` is NULL in MySQL, not `'ac'`. The leading `''` makes the result TEXT at every arity. |
| `CHAR_LENGTH` / `CHARACTER_LENGTH` | `length(x)` | Characters in both, including emoji: `CHAR_LENGTH('a😀b')` is 3. |
| `UCASE` / `LCASE` | `upper` / `lower` | See [Divergences](#divergences): ASCII-only folding. |
| `LOCATE(needle, hay)` | `instr(hay, needle)` | **The arguments are the other way round.** Also see Divergences: MySQL's `LOCATE` follows the argument's collation and `instr()` does not. |
| `POSITION(needle IN hay)` | `instr(hay, needle)` | Same swap. |
| `LEFT(s, n)` | `substr(s, 1, n)`, or `substr(s, 0, 0)` when `n <= 0` | `LEFT('hello',-1)` is `''`, and `substr(s, 1, -1)` would answer `'h'`. |
| `RIGHT(s, n)` | `substr(s, -n, n)`, or `substr(s, 0, 0)` when `n <= 0` | `RIGHT('hello',0)` is `''`, and `substr('hello', -0)` is the whole string. |
| `ISNULL(x)` | `(x IS NULL)` | `1`/`0` in both. |
| `IF(c, a, b)` | `CASE WHEN c THEN a ELSE b END` | MySQL's truthiness is the engine's: `IF('abc',…)` is false, `IF('1',…)` is true, `IF(NULL,…)` is false. |
| `COALESCE(x)` | `(x)` | MySQL takes one argument; the engine's wants two. |
| `RAND()` | `(abs(random()) / 9223372036854775808.0)` | A different generator with the same contract: a double in `[0, 1)`. |
| `NOW`, `LOCALTIME`, `LOCALTIMESTAMP`, `UTC_TIMESTAMP` | `datetime('now')` | See Divergences: the clock is UTC. |
| `CURDATE`, `UTC_DATE` | `date('now')` | |
| `CURTIME`, `UTC_TIME` | `time('now')` | |
| `UNIX_TIMESTAMP()` | `unixepoch('now')` | |
| `YEAR` `MONTH` `DAY` `DAYOFMONTH` `HOUR` `MINUTE` `SECOND` `DAYOFYEAR` | `CAST(strftime('%…', d) AS INTEGER)` | `strftime` answers zero-padded text; MySQL answers an integer, so `MONTH('2024-03-05')` must be `3` and not `'03'`. |
| `DAYOFWEEK(d)` | `(CAST(strftime('%w', d) AS INTEGER) + 1)` | MySQL counts Sunday as 1; SQLite's `%w` counts it as 0. |
| `WEEKDAY(d)` | `((CAST(strftime('%w', d) AS INTEGER) + 6) % 7)` | MySQL counts Monday as 0. A different shift from `DAYOFWEEK`'s. |
| `QUARTER(d)` | `((CAST(strftime('%m', d) AS INTEGER) + 2) / 3)` | |
| `LAST_DAY(d)` | `date(d, 'start of month', '+1 month', '-1 day')` | `LAST_DAY('2024-02-05')` is `2024-02-29`. |
| `TRIM([BOTH\|LEADING\|TRAILING] FROM s)` | `trim` / `ltrim` / `rtrim` | Only the forms with no string to remove; both engines strip spaces and nothing else. |

`IFNULL`, `TRIM(s)`, `LTRIM`, `RTRIM`, `REPLACE`, `INSTR`, `ABS`, `UPPER`,
`LOWER`, `CURRENT_TIMESTAMP`, `CURRENT_DATE` and `CURRENT_TIME` already
resolve in the engine under those exact names and are **not rewritten** — a
statement containing only those reaches the engine byte for byte. `UPPER`
and `LOWER` mean SQLite's function rather than MySQL's, and that difference
is written out in [Divergences](#divergences).

**`LENGTH`, `HEX`, `SUBSTRING` (and its `SUBSTR`/`MID` spellings) and
`NULLIF` are rewritten now too (AHL-465)**, even though the spelling is
identical in both dialects — every connection this server serves is a MySQL
client, so a bare `length(x)` came from one exactly as much as `LENGTH(x)`
did, and both are rewritten onto a primitive with MySQL's own measured
behaviour: `octet_length`, `mysql_hex`, `mysql_substr`, `mysql_nullif`.
`OCTET_LENGTH`/`BIT_LENGTH`, refused outright before for lack of a
byte-counting primitive, are real mappings onto `octet_length` now as well.
None of the five raise a warning, for the same reason nothing else in the
mapped table does: the expression means the same thing MySQL's spelling
does, which is the whole admission price for being rewritten.

**`ROUND` is rewritten only for a value written as a MySQL `DOUBLE`
literal — one with an exponent — or a negative digit count.** MySQL 8.4.11
ties a `DOUBLE` argument's halfway case to even and an exact `DECIMAL`
literal's away from zero, a distinction this engine cannot represent (there
is one real-number storage class, not two), so the shim can only fix the one
shape a **literal's text** proves: `ROUND(2.5e0)` is rewritten,
`ROUND(2.5)` — MySQL's own manual gives this as the *safe* spelling,
precisely because it is `DECIMAL` — is not, and neither is `ROUND(x)` over a
column or an expression, which this shim has no catalog access to classify.
`ROUND(x, d)` with a negative literal `d`, refused outright before because
`round()` clamps the digit count to zero, is a mapping too, regardless of
the first argument's shape: a real answer for the overwhelming majority of
values, where the previous behaviour was refusing every one of them. A
value that happens to land exactly on a negative-digit halfway boundary
(`ROUND(150, -2)`) still ties to even rather than away from zero, the same
uncorrectable gap as the positive-digit case.

**Refused, with the input that decided it.** Each of these has a mapping that
looks right and is not, so each fails with `1235` and a message naming the
function and the case where the two engines part company:

| MySQL | Why it is not mapped |
| --- | --- |
| `CONCAT_WS` | It *skips* NULL arguments — `CONCAT_WS('-','a',NULL,'c')` is `'a-c'` — and every concatenation the engine has propagates NULL. |
| `GREATEST`, `LEAST` | MySQL compares numerically as soon as one argument is a number — `GREATEST(2,'10')` is `2` and `LEAST(2,'10')` is `'10'` — where the engine's `max`/`min` compare by storage class and answer the other way round. |
| `MOD` | MySQL keeps the fraction: `MOD(5.5, 2)` is `1.5`. The engine's `%` truncates to integers and answers `1`. |
| `SYSDATE` | The clock at the moment of the call, not at the start of the statement. `datetime('now')` is the latter. |
| `FROM_UNIXTIME` | MySQL answers NULL outside `0 .. 32536771199`; `datetime(n,'unixepoch')` keeps counting — `FROM_UNIXTIME(-1)` is NULL there and `'1969-12-31 23:59:59'` here. |
| `UNIX_TIMESTAMP(date)` | Reads its argument in the session time zone, accepts formats `unixepoch()` does not, and answers `0` rather than NULL for one it cannot read. |
| `DATE_FORMAT`, `STR_TO_DATE`, `TIME_FORMAT` | MySQL's format specifiers are not `strftime`'s: `%i` is minutes there and nothing here, `%s` is seconds there and the Unix epoch here. |
| `DATEDIFF`, `TIMEDIFF`, `TIMESTAMPDIFF` | They need a day or second count between two moments, and the engine has no `julianday()` to subtract. |
| `DATE_ADD`, `DATE_SUB`, `ADDDATE`, `SUBDATE` | The `INTERVAL` argument is MySQL syntax with no expression behind it here. |
| `MONTHNAME`, `DAYNAME` | Locale-dependent names; `strftime` has none. |
| `WEEK`, `WEEKOFYEAR`, `YEARWEEK` | MySQL has eight week-numbering modes, selected by an argument and by `default_week_format`. `%W` is one of them. |
| `LOCATE(substr, str, pos)` | `instr()` has no third argument to start from. |
| `RAND(seed)` | It promises MySQL's own seeded sequence. |
| `NOW(p)`, `CURTIME(p)`, and the other precision forms | MySQL returns 0 to 6 fractional digits; the engine's clock renders whole seconds. |
| `TRIM(… 'xy' FROM s)` | MySQL removes the whole string `xy` from each end; the engine's second `trim()` argument is a *set of characters*. `TRIM(BOTH 'xy' FROM 'yxhixy')` is `'yxhi'` in MySQL and `'hi'` under a character set. |
| `LEFT(s, n)` / `RIGHT(s, n)` with a non-literal `n` | `LEFT(s, NULL)` is NULL in MySQL and `''` through `substr()`, and `RIGHT` needs the length twice — which would duplicate a `?` placeholder and shift every parameter after it. A literal length is mapped; anything else is refused. |

A name in neither list is left alone, and the engine answers it with
`no such function: LPAD`, which is already a `1235` naming it. `POW`, `SQRT`,
`CEIL`, `FLOOR`, `TRUNCATE`, `SIGN`, `LPAD`, `RPAD`, `REPEAT`, `REVERSE`,
`SPACE`, `SUBSTRING_INDEX`, `FIELD`, `FIND_IN_SET`, `FORMAT`, `MD5`, `SHA1`
and `UUID` all arrive that way: there is no engine primitive behind any of them.

**These rewrites raise no warning**, unlike the DDL translation below. Dropping
a DDL clause changes what the statement asked for; a mapping does not — the
whole admission price for being in the first table is that the expression means
the same thing. A warning on every `NOW()` would only put noise in front of the
warnings that do mean something.

### JSON functions (AHL-490)

`crates/inlaysql-core/src/json.rs` implements SQLite's json1 family under
SQLite's own names — `json_extract`, `json_set`, `json_insert`,
`json_replace`, `json_remove`, `json_valid`, `json_type`, `json_quote`,
`json_array`, `json_object`, `json_array_length`, `json()` — plus the
`->`/`->>` path operators SQLite added in 3.38.0. JSON is stored exactly as
SQLite stores it: ordinary `TEXT`, not a distinct storage or column class —
see AGENTS.md and D1, both of which this follows rather than adds to.

**Most of the family needs no shim at all.** `JSON_EXTRACT`, `JSON_SET`,
`JSON_INSERT`, `JSON_REPLACE`, `JSON_REMOVE`, `JSON_VALID`, `JSON_ARRAY`,
`JSON_OBJECT` and `JSON()` are spelled identically in MySQL and SQLite, and
the engine's own function lookup is already case-insensitive, so a MySQL
client's uppercase call reaches the SQLite-semantics implementation directly
with no rewrite — the same reason `IFNULL`/`TRIM`/`REPLACE`/`INSTR`/`ABS`
above need none either. What follows is only the two names that differ and
the four that look mappable and are not.

| MySQL | Becomes | The corner that decided it |
| --- | --- | --- |
| `JSON_LENGTH(x[, p])` | `json_array_length(x[, p])` | Exact for an array — `whereJsonLength`'s whole reason to exist — and documented, not refused, for the object/scalar case; see Divergences. |
| `JSON_CONTAINS_PATH(x, 'one', p)` | `(json_type(x, p) IS NOT NULL)` | The one shape Laravel's own query builder emits (`whereJsonContainsKey`, `MySqlGrammar::compileJsonContainsKey`: `ifnull(json_contains_path(field, 'one', path), 0)`), rewritten onto the identical rule Laravel's own **SQLite** grammar uses for the same clause (`SQLiteGrammar::compileJsonContainsKey`) — not a guess, since SQLite has no `json_contains_path` either. `'all'` mode or more than one path is refused: it would need one `json_type()` check per path, ANDed or ORed by the mode. |
| `JSON_UNQUOTE(JSON_EXTRACT(x, p))` | `(x ->> p)` | Laravel's `wrapJsonSelector` — a plain JSON path read, e.g. `orderBy('attributes->color')` — emits exactly this nested pair, never a bare `JSON_UNQUOTE`. The engine's `->>` is defined as the same "extract and unwrap" MySQL's own `->>` operator is, checked against sqlite3 and a real MySQL 8 container. |

**Refused, with the input that decided it:**

| MySQL | Why it is not mapped |
| --- | --- |
| `JSON_QUOTE(x)` | MySQL requires a string argument and *errors* otherwise — `JSON_QUOTE(1)` is `Incorrect type for argument 1 in function json_quote`, checked against a real MySQL 8 container — where `json_quote()` accepts any scalar and renders a number as a bare JSON number, not a quoted string. This shim has no catalog access to know a column's declared type, so it cannot tell the safe case from the one that should have errored. |
| `JSON_TYPE(x[, p])` | MySQL answers uppercase names with no exact overlap: `OBJECT`/`ARRAY`/`STRING`/`INTEGER`/`DOUBLE`/`BOOLEAN`/`NULL` where `json_type()` answers `object`/`array`/`text`/`integer`/`real`/`true`/`false`/`null` — and MySQL folds `true`/`false` into one `BOOLEAN` where SQLite keeps them apart, so no single rewrite recovers both directions. Left unmapped, `JSON_TYPE` would reach the engine's own same-named function under SQLite's different rules — refused instead, so the divergence is loud rather than a silently different string. |
| `JSON_CONTAINS(target, candidate[, p])` | Needs a set-membership test over a document's elements/members. There is no InlaySQL primitive for that without `json_each()`, which is table-valued — this engine has no mechanism for a function that returns rows in `FROM` (see "What does not work yet"). |
| `JSON_OVERLAPS(a, b)` | The same reason: a set-intersection test with no primitive behind it. |

A MySQL client calling `JSON_ARRAY_APPEND`, `JSON_ARRAY_INSERT`,
`JSON_MERGE_PATCH`, `JSON_MERGE_PRESERVE`, `JSON_SEARCH`, `JSON_DEPTH`,
`JSON_PRETTY`, `JSON_STORAGE_SIZE`, `JSON_TABLE` or `JSON_VALUE` reaches the
engine's own `no such function`, which is already a `1235` naming it — none
of these were considered mappable enough to earn a dedicated refusal
message, unlike the four above.

**`->`/`->>` accept any expression, which is a strict superset of MySQL's
own grammar** — MySQL restricts the left operand of both operators to a
column reference (`'{"a":1}' -> '$.a'` is a syntax error against a real
MySQL 8 container; `col -> '$.a'` is not), where SQLite's grammar, which
this engine follows, allows any expression there. This accepts strictly more
than MySQL does, never less, so it is not listed as a refusal.

### Window functions (AHL-494)

`crates/inlaysql-core/src/sql.rs`/`engine.rs` implement `OVER (PARTITION BY
... ORDER BY ... frame)` under SQLite's own grammar: `row_number`, `rank`,
`dense_rank`, `ntile`, `lag`/`lead`, `first_value`/`last_value`/`nth_value`,
the aggregate family (`sum`/`count`/`avg`/`min`/`max`/`group_concat`)
`OVER (...)`, `ROWS` frames (plus SQLite's implicit default frame), named
windows (`WINDOW w AS (...)`) and `FILTER (WHERE ...)` on an aggregate,
window or not.

**No shim work was needed, and none was added.** `SELECT ... FROM <table>`
already reaches the engine essentially as written — `mysqlddl::translate`
only has rewrites for DDL and specific DML shapes, so a plain `SELECT` falls
through untouched, and `mysqlfunc::rewrite`'s byte scan only ever acts on the
MySQL-named functions in its own mapped list (`LEFT`, `LOCATE`, `NOW`, …:
[MySQL-named scalar functions are mapped, not guessed](#mysql-named-scalar-functions-are-mapped-not-guessed)
above). `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`, `LAG`, `LEAD`,
`FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE`, `OVER`, `PARTITION`, `WINDOW` and
`FILTER` are not in that list — MySQL 8 spells every one of them identically
to standard SQL and to SQLite, so a MySQL client's window query reaches the
engine byte-for-byte. A MySQL-named function used *inside* a window's
`PARTITION BY`/`ORDER BY`/argument list (`SUM(x) OVER (PARTITION BY
YEAR(created_at))`) is rewritten exactly as it would be anywhere else in the
statement — the rewriter has no notion of clause structure, so there is
nothing OVER-specific for it to get wrong.

`FILTER (WHERE ...)` is worth naming explicitly: real MySQL has no `FILTER`
clause at all (it is a SQLite/PostgreSQL extension to the standard), so no
MySQL client library ever generates one. A hand-written query using it still
reaches the engine and works — this is SQLite's dialect being a strict
superset here, the same shape `->`/`->>` are in the JSON section above — but
it is not something a MySQL-wire client should be expected to send.

`percent_rank()` and `cume_dist()`, which SQLite also ships as window
functions, are not implemented and are refused with the ordinary `1235`
(`crates/inlaysql-server/tests/wire.rs`'s "still refused" probe uses exactly
this). An explicit `RANGE`/`GROUPS` frame is refused the same way — see
`crates/inlaysql/tests/sqllogictest/unsupported.test` and `WindowFrame`'s doc
in `plan.rs` for why a value-based frame is not silently treated as the
position-based `ROWS` this engine implements.

### MySQL-only DDL is translated, not invented

`crates/inlaysql-server/src/mysqlddl.rs`. `AUTO_INCREMENT`, `ENGINE=InnoDB`,
`DEFAULT CHARSET=utf8mb4` and the rest are MySQL spellings with no place in
SQLite's dialect, and they are in the first statement of nearly every
migration. They are recognised and removed **here**, before the statement
reaches the engine — never added to the engine's grammar, which is what
decision D1 forbids.

Every clause is in exactly one of *three* lists since AHL-469, because
`COLLATE` is now translated rather than removed — a MySQL collation name
becomes the engine's nearest one, and the `1618` says which and what the
mapping does not carry. See [Divergences](#divergences) for the table.

**Neutralised** — the clause describes something InlaySQL already does, or
something with no observable effect here, so removing it changes nothing about
the table that gets built:

| Clause | Why removing it is faithful |
| --- | --- |
| `AUTO_INCREMENT` on an `INTEGER PRIMARY KEY` | That column *is* the row id, and the engine already fills it from a monotonic counter when the `INSERT` omits it or passes `NULL` — verified against `Engine::insert`, which is also what makes `LAST_INSERT_ID()` work today. |
| `UNSIGNED` | See [Divergences](#divergences). This one has a cost, and it is stated there rather than buried. |
| `ENGINE = x` | There is one storage engine. |
| `ROW_FORMAT = x` | There is one on-disk row format. |
| `AUTO_INCREMENT = <n>` as a table option | See [Divergences](#divergences): the counter always starts at 1. |
| `DEFAULT CHARSET` / `CHARACTER SET`, on the table or a column | There is one encoding here, UTF-8, so there is nothing to select. |
| `ALGORITHM = x`, `LOCK = x` on `ALTER TABLE` / `CREATE INDEX` | They steer *how* MySQL performs a change online. There is nothing here to select between. |

**Refused** — the clause changes what the table means, and this server cannot
reproduce it. Each fails with `1235` and a message naming the clause:

| Clause | Why it is not dropped |
| --- | --- |
| `AUTO_INCREMENT` on a column that is not an `INTEGER PRIMARY KEY` | There is no counter for an ordinary column. Accepting it would give a column that silently stays `NULL`. |
| `ON UPDATE CURRENT_TIMESTAMP` (and `NOW()`, `LOCALTIME`, `SYSDATE`) | InlaySQL never writes a column the statement did not name. The value would silently stop tracking the row's last update. A foreign key's `ON UPDATE CASCADE` is a different clause and is left alone. |
| `ZEROFILL` | It promises every value comes back padded to the display width. This server returns the number as stored. |
| `KEY x (a)` / `INDEX x (a)` / `UNIQUE KEY x (a)` / `FULLTEXT KEY` inside `CREATE TABLE` | The engine would create no index, so the table would come back missing the index it declared. (It also mis-parses today, giving an error about a column type that does not exist.) |
| `USING BTREE` / `USING HASH` on `CREATE INDEX` | The engine picks the index kind from the column's type — BM25 for `TEXT`, HNSW for `VECTOR` — and has no B-tree or hash index on a scalar column. Dropping the hint would build a different index from the one asked for. |
| Any table option not in the list above (`PARTITION BY`, `COMMENT`, `TABLESPACE`, …) | Guessing is how a table nobody asked for gets built. A tail that begins in the *engine's* dialect instead — `STRICT`, `WITHOUT ROWID`, `AS SELECT` — is not MySQL's and is passed straight through, because the engine has a better answer for it than a MySQL translator could invent. |

**No drop is silent.** Every neutralised clause comes back as a MySQL warning
(`1618 ER_WARN_OPTION_IGNORED`): the OK packet carries the count and
`SHOW WARNINGS` lists them, with the clause as written and the reason. So

```
mysql> create table users (id bigint unsigned auto_increment primary key,
    ->   name varchar(255)) engine=InnoDB default charset=utf8mb4;
Query OK, 0 rows affected, 4 warnings

mysql> show warnings;
Warning | 1618 | `UNSIGNED` was ignored: InlaySQL integers are signed 64-bit…
Warning | 1618 | `auto_increment` was ignored: an INTEGER PRIMARY KEY is…
Warning | 1618 | `engine=InnoDB` was ignored: InlaySQL has one storage engine…
Warning | 1618 | `default charset=utf8mb4` was ignored: InlaySQL stores text…
```

**What the translation deliberately does *not* touch.** `NOT NULL`, `DEFAULT`,
`UNIQUE`, `CHECK`, a foreign key declared *inside* `CREATE TABLE`,
`DATETIME`/`TIMESTAMP`/`JSON` column types, and SQLite's own four `ALTER
TABLE` operations (`ADD COLUMN`, `RENAME TO`, `RENAME COLUMN`, `DROP COLUMN`)
are ordinary SQL, not MySQL decoration, so they are left exactly as written
and handed to the engine rather than rewritten here. Phase 1b (AHL-412) has
since implemented all of them in core — see
[What does not work yet](#what-does-not-work-yet) for what a stock migration
still hits past them. Translating around a gap in core would have moved a
failure somewhere harder to find and taken the pressure off fixing it
properly; the point of leaving this list alone stands regardless of whether
core has caught up with it yet.

A statement with no MySQL decoration in it is handed to the engine **byte for
byte**: the re-rendering only runs when a clause was actually removed, so a bug
in the translation cannot quietly reshape a statement it had no business
touching.

#### Post-creation index and constraint DDL is translated too (Phase 3, AHL-474)

The previous paragraph has one exception now. Laravel's schema builder — and
every ORM modelled on it — never inlines a fluent
`$table->string('email')->unique()` into `CREATE TABLE`; it compiles a
*separate* `ALTER TABLE ... ADD {INDEX|UNIQUE|CONSTRAINT}` right after, and
the same is true of `->index(...)` and `->foreign(...)`. Core has the target
syntax for the index cases (`CREATE INDEX`, `CREATE UNIQUE INDEX`,
`DROP INDEX`, all pre-existing) but no `ALTER TABLE` operation for any of
them — SQLite's own `ALTER TABLE` never had one. This was the actual first
wall a stock migration hit (AHL-471); `mysqlddl.rs`'s `alter_table` now
rewrites these shapes onto the free-standing statements core already runs,
before the engine's parser ever sees an operation it has no name for:

| Written | Becomes | Warned |
| --- | --- | --- |
| `ADD INDEX`/`ADD KEY [name] (cols)` | `CREATE INDEX [name] ON t (cols)` | no — nothing about the table changed, only which statement says so |
| `ADD UNIQUE [INDEX\|KEY] [name] (cols)`, `ADD CONSTRAINT name UNIQUE (cols)` | `CREATE UNIQUE INDEX name ON t (cols)` | no |
| `DROP INDEX`/`DROP KEY name` | the standalone `DROP INDEX name` | no |
| `ADD CONSTRAINT [name] FOREIGN KEY ...` | nothing — the operation is dropped, and the statement still answers OK | **yes** — nothing here can record it |
| `RENAME INDEX a TO b` | refused (`1235`) | — core has no rename for an index, only drop-and-recreate |

**The unnamed case is synthesised the way MySQL itself names it**: MySQL's
own manual, "CREATE TABLE Statement" → "Secondary Indexes", says an index
given no name of its own is named after its first column, with a numeric
suffix (`_2`, `_3`, …) appended to disambiguate it from one that already has
that name — checked here against both the catalog's real indexes and every
other unnamed index the same statement is adding, so two migrations that each
add an unnamed index on the same first column do not collide, and neither do
two unnamed indexes inside one multi-operation `ALTER TABLE`.

**`ADD CONSTRAINT ... FOREIGN KEY` is the one case that is OK rather than
refused with a `1235`, and it still never succeeds silently.** There is
nowhere in the catalog to record a foreign key added after the table already
exists — only `CREATE TABLE` can declare one. Answering `1235` would suggest
this server can enforce a foreign key it just has not gotten around to
implementing on `ALTER TABLE` yet; it cannot, on `ALTER TABLE` or on
`CREATE TABLE` either — foreign keys are recorded there but left unenforced,
SQLite's own long-standing default and not a gap (see
[What is implemented](#what-is-implemented) above). So this is OK, with a
`1618` naming exactly what was not recorded, the same honesty the rest of
this file insists on everywhere else.

**MySQL's comma-separated `ALTER TABLE` operations become one statement per
operation, and running them is not atomic.** `ALTER TABLE t ADD COLUMN x INT,
ADD INDEX (x)` is one MySQL statement; SQLite's `ALTER TABLE` accepts exactly
one operation, and the engine already refused more than one outright. The
shim splits the list and runs each resulting statement against the engine in
order. If the third of five fails, the first two already happened and the
last two never will — the client sees the failing operation's own error, and
nothing here undoes what already ran. Wrapping the whole `ALTER TABLE` in an
explicit transaction is the only way to get atomicity back, exactly as
sending the operations as separate statements over any MySQL connection would
require too.

Two more MySQL statement shapes core's SQLite dialect has no equivalent for,
translated the same way:

| Written | Becomes | Warned |
| --- | --- | --- |
| `TRUNCATE [TABLE] t` | `DELETE FROM t` | **yes** — InlaySQL's row id counter only ever moves forward and cannot be seeded or rewound (see Divergences below), so unlike MySQL's own `TRUNCATE` this does not restart it at 1 |
| `RENAME TABLE a TO b[, c TO d, ...]` | one `ALTER TABLE ... RENAME TO ...` per pair | no — a pure rename loses nothing |

### Error mapping

| Engine error | MySQL | SQLSTATE |
| --- | --- | --- |
| `Constraint` (duplicate key) | 1062 `ER_DUP_ENTRY` | 23000 |
| `Catalog` "no such table" | 1146 | 42S02 |
| `Catalog` "already exists" | 1050 / 1061 | 42S01 |
| `Catalog` "no column" | 1054 | 42S22 |
| `Parse` | 1064 | 42000 |
| `Unsupported` | 1235 | 42000 |
| `Type` | 1366 | HY000 |
| `Bind` | 1210 | HY000 |
| `Conflict` | 1213 (retryable) | 40001 |
| `Stale` | 1615 | HY000 |
| read-only handle | 1290 | HY000 |
| `Storage` | 1030 | HY000 |
| `Corrupt` | 1194 | HY000 |
| `FormatVersion` | 1112 | 42000 |

`Type` uses `HY000` rather than a `22xxx` class deliberately: PDO renders
SQLSTATE `22007` as "Invalid datetime format", so an integer/text mismatch was
reaching users as a date error.

### Client-side-escaped string literals are rewritten before the engine sees them

Found the same way as the Laravel migration work above — driving
`inlaysql serve --mysql` with two independent client libraries
(`mysql-connector-python` and `PyMySQL`) rather than this project's own test
client — and it was a real correctness gap, not a divergence: it could
silently corrupt a value or break a statement outright.

Real MySQL escapes special characters in a client-quoted text-protocol string
with a leading backslash (`NO_BACKSLASH_ESCAPES` is off by default), and every
client library that builds `COM_QUERY` text with client-side escaping relies
on the server understanding that: `mysql-connector-python`'s default
(non-`prepared=True`) cursor, all of `PyMySQL` (it has no true binary-protocol
prepared statements at all), and PHP's PDO with emulated prepares — a common
default, including for Laravel apps that have not explicitly set
`PDO::ATTR_EMULATE_PREPARES => false`. `inlaysql-core` parses every statement
with `sqlparser`'s `SQLiteDialect`, whose
`supports_string_literal_backslash_escape()` is `false` — correctly, for real
SQLite — so a client-escaped value used to reach the parser unrewritten: a
value containing `"` got a spurious backslash silently baked into the stored
data (send `{"role":"admin"}`, get back `{\"role\":\"admin\"}`), and a value
containing `'` broke the statement outright, the client's `\'` reading as a
real string terminator.

Fixed in the shim, not the engine, per D1: `rewrite_backslash_escapes`
(`crates/inlaysql-server/src/sqltext.rs`) decodes MySQL's escape table inside
every single-quoted run — accepting either spelling of an embedded quote, a
client's `\'` or the SQL-standard `''` — and re-emits it in the doubled-quote
form both MySQL and SQLite agree on, before the statement reaches
`shim::intercept` or the engine's parser either one. `\%`/`\_` are left as the
literal two-character sequence MySQL itself leaves them (a later `LIKE`
depends on it); double-quoted and backtick-quoted runs are left untouched
entirely, since a MySQL client would not send an escape-looking sequence
inside a SQLite *identifier*. A true binary-protocol prepared statement was
never affected — bound values arrive as typed binary data, never as escaped
text — and reconfirmed unaffected after the fix. Verified against a real,
independent `mysql-connector-python` session (not this project's own test
client) round-tripping `{"role":"admin"}`, `O'Brien` and `100%` correctly.

---

## Divergences

Where this server accepts a statement and means something a little different by
it.

The first group is a **dropped DDL clause**, and each one is reported at run
time as a `1618` warning naming the clause — but they are written down anyway,
because a warning is easy to miss and a wrong value is not. The second group is
a property of the **engine's function library**, not of any clause, so there is
nothing to warn about and this file is the only record. Everything in it was
checked against MySQL 8.4.11, and both columns of every table below are real
output from the two servers.

### `UNSIGNED` is accepted, and integers are signed 64-bit

MySQL's `BIGINT UNSIGNED` spans `0 .. 18446744073709551615`. InlaySQL's
`Integer` is a signed 64-bit value. So on a column declared `UNSIGNED`:

- **Nothing above `9223372036854775807` round-trips.** The top half of MySQL's
  `BIGINT UNSIGNED` range has no representation here.
- **A negative value is stored rather than refused.** MySQL rejects it; this
  server's constraints are real now (Phase 1b, AHL-412), but the shim drops
  `UNSIGNED` as bare decoration rather than synthesising a `CHECK (col >= 0)`
  from it (`mysqlddl.rs`: `create table t (id bigint unsigned primary key)`
  still becomes exactly `create table t (id bigint primary key)`, with nothing
  added), so nothing in the resulting table enforces it.

At every narrower width — `TINYINT`, `SMALLINT`, `MEDIUMINT`, `INT` — a signed
64-bit value covers the *entire* unsigned range, so only the second point
applies.

**Why accept it rather than refuse it.** Every ORM's primary key is
`bigint unsigned auto_increment`, so refusing `UNSIGNED` would leave the first
migration statement failing exactly as it did before, one error code further
along — no statement in this document would have moved. And the practical
exposure is narrow: keys the engine assigns start at 1 and increase, and a
count that reaches 9.2 quintillion is not the failure anyone is going to hit
first. The cost is real but bounded, it is stated on the wire and here, and
the alternative bought nothing.

### Collations are honoured for case, and not for accents (AHL-469)

**This was the most dangerous divergence in the project and it is now closed
for the case half.** Before AHL-469 InlaySQL compared text byte for byte
whatever the DDL said, so `WHERE name = 'ADA'` matched a stored `'ada'` in
MySQL and did not here — silently, returning fewer rows and reporting success.
Every other divergence in this document fails loudly; that one did not.

The engine has real collations now, in SQLite's spelling: `BINARY`, `NOCASE`
and `RTRIM`. This server maps MySQL's onto them in DDL.

| Written | Applied as | Warned |
| --- | --- | --- |
| `COLLATE utf8mb4_bin`, any `*_bin`, `binary` | `BINARY` | no — it is exact |
| any `*_ci` (`utf8mb4_unicode_ci`, `utf8mb4_general_ci`, `utf8mb4_0900_ai_ci`, …) | `NOCASE` | **yes**, naming what `NOCASE` does not fold |
| any `*_cs` (`utf8mb4_0900_as_cs`) | `BINARY` | **yes**, naming the ordering difference |
| anything else | dropped; `BINARY` stands | **yes**, naming the collation |
| `CHARACTER SET x` / `CHARSET x` | dropped | yes — there is one encoding here, UTF-8 |

**A table's `COLLATE` reaches its string columns.** MySQL applies a table-level
collation to every `CHAR`/`VARCHAR`/`TEXT`/`ENUM`/`SET` column that did not
write one of its own, and so does this — which is the step that makes the
mapping matter, because an ORM puts the collation on the table and never on the
columns. `create table users (...) default charset=utf8mb4 collate
utf8mb4_unicode_ci` therefore gives every `varchar` column `COLLATE NOCASE`,
and `WHERE name = 'ADA'` matches `'ada'` exactly as it does in MySQL.

`SHOW FULL COLUMNS`, `SHOW TABLE STATUS` and `information_schema.columns` now
report the MySQL collation whose behaviour the column actually has —
`utf8mb4_bin` for a `BINARY` column and `utf8mb4_general_ci` for a `NOCASE`
one — rather than a fixed string. A `RTRIM` column, which has no MySQL
counterpart at all, is reported as `inlaysql_rtrim` rather than under a name a
client would misread.

**What is still not honoured, and why.**

* **Accents.** `NOCASE` folds ASCII `A`–`Z` and nothing else, which is exactly
  what SQLite's `NOCASE` does. MySQL's `_ci` collations are accent-*insensitive*
  as well: `'é' = 'e'` is true under `utf8mb4_unicode_ci` and false here. Every
  `*_ci` mapping raises a `1618` that says so.
* **Non-ASCII case.** `'É' = 'é'` is true in MySQL and false here, for the same
  reason. Also named in the warning.
* **Ordering.** MySQL sorts by Unicode collation weights; this sorts by UTF-8
  code point. Two strings that are unequal under both engines can therefore
  come back in a different order.
* **There is no `CREATE COLLATION`.** These three are all there are, so a
  collation name outside them cannot be honoured however the server tried.

Fixing the first two means a Unicode case- and accent-folding table in
`inlaysql-core`, which is a different decision from this one: the differential
fuzzer's oracle is SQLite, and SQLite does not fold Unicode either. It would
have to be a fourth collation under its own name, not a change to `NOCASE`.

**The string functions.** `LOCATE('L','hello')` is `3` in MySQL and `0` here,
and `INSTR` and `REPLACE` are case-sensitive in both: none of the three
consults a collating sequence in this engine, where MySQL's `LOCATE` and
`INSTR` follow the argument's. `NULLIF` *does* follow it now — on a column the
shim mapped to `NOCASE`, `NULLIF(name,'ADA')` is NULL here as it is in MySQL —
because `mysql_nullif` is one of the three collation-aware scalars.

### The row id counter always starts at 1

`CREATE TABLE ... AUTO_INCREMENT = 1000` is accepted and the first row still
gets `1`. The keys are unique and increasing either way; they are simply not
the numbers that asked for. There is no way to seed the engine's counter today.

### The upsert's affected-rows count does not match MySQL's

MySQL reports **1** for a row this statement inserted, **2** for a row it
updated, and **0** for an update that wrote back the values already there.
This server reports **1 per row the statement wrote, insert or update
alike** — the same count SQLite's own `changes()` gives for an
`ON CONFLICT DO UPDATE`, which is what
`crates/inlaysql-server/src/mysqlddl.rs`'s `insert_on_duplicate_key_update`
(AHL-476) translates the clause onto. A three-row `upsert()` call that
inserts one row and updates two reports `affected_rows = 3` here and
`1 + 2 + 2 = 5` on real MySQL.

**Why this is not fixed rather than documented.** `Engine::insert` in
`inlaysql-core` counts one write per row; it has no per-row record of
whether that write was an insert, an update, or an update that changed
nothing, because SQLite's own execution model never needed to keep one.
Manufacturing MySQL's three-way count would mean teaching the engine a
MySQL-shaped reporting convention that is not a SQLite feature core is
missing — which is exactly what D1 rules out adding to `inlaysql-core`, and
the shim has no view into a statement's row-by-row outcome to derive it from
on its own, safely, after the fact. A caller that counts on the exact MySQL
number for this one statement shape should not rely on it here; every other
`INSERT`, `UPDATE` and `DELETE`'s `affected_rows` is unaffected; and this
divergence has nothing to do with what the rows end up containing, which
`crates/inlaysql-server/tests/wire.rs`'s upsert tests assert directly.

---

The rest are the engine's function library rather than a dropped clause.

### `UPPER`/`LOWER` fold ASCII only

The engine's `upper()` and `lower()` change `a`–`z` and nothing else, so
`UPPER('héllo')` is `HéLLO` here and `HÉLLO` in MySQL, and `LCASE('HÉLLO')` is
`hÉllo` here and `héllo` there. `UCASE`/`LCASE` are mapped straight onto them
and inherit it exactly; the divergence is the engine's, and was there under the
`UPPER`/`LOWER` spellings before the mapping existed. (MySQL is not doing full
Unicode either — `UPPER('straße')` is `STRAßE` there — but it does handle the
Latin-1 accents, and this does not.)

### The clock is UTC, and there is no session time zone

`NOW()`, `CURDATE()`, `CURTIME()`, `UNIX_TIMESTAMP()` and their `LOCALTIME`
spellings all resolve to the engine's clock, which reads UTC and has no time
zone to be set to anything else. `SET time_zone = ...` is recorded and inert.
**So `NOW()` here is `UTC_TIMESTAMP()`, and in MySQL it is the session time
zone** — the same value only when that session is itself in UTC. The format is
identical (`YYYY-MM-DD HH:MM:SS`), so nothing fails; the number is simply the
UTC one.

### Date functions read ISO-8601 strings, and nothing else

There is no `DATE` or `DATETIME` type: a date is TEXT. `YEAR`, `MONTH`, `DAY`,
`HOUR`, `MINUTE`, `SECOND`, `DAYOFWEEK`, `WEEKDAY`, `DAYOFYEAR`, `QUARTER` and
`LAST_DAY` are built on `strftime`, which parses SQLite's date formats. On an
ISO-8601 string the answers are MySQL's, checked value by value. Off it they
are not:

- **MySQL's looser input formats are NULL here.** `YEAR('2024/01/15')` is
  `2024` in MySQL and NULL here.
- **A number is read as a Julian day, not as a MySQL numeric date.**
  `YEAR(20240115)` is `2024` in MySQL; here `strftime` reads 20240115 as a
  Julian day number, which is outside the calendar, and answers NULL. The
  narrower `YEAR(2024)` is NULL in MySQL and **-4707** here, which is the one
  case in this list that is a wrong value rather than a missing one.
- **SQLite's magic strings resolve.** `DATE('now')` is NULL in MySQL and
  today's date here, because `date()` is the engine's own function passed
  straight through.
- **`HOUR` cannot exceed 23.** MySQL's `TIME` type spans ±838 hours, so
  `HOUR('100:20:30')` is `100` there and NULL here.

### `LENGTH`, `HEX`, `SUBSTRING` and `NULLIF` — closed (AHL-465)

Until AHL-465 these four resolved in the engine under names MySQL also uses,
passed through rather than mapped, meaning SQLite's function by each of them
rather than MySQL's. `inlaysql-core` now has primitives with MySQL's own
measured behaviour — `octet_length`, `mysql_hex`, `mysql_substr`,
`mysql_nullif` — and the shim rewrites `LENGTH`, `HEX`, `SUBSTRING` (and its
`SUBSTR`/`MID` spellings) and `NULLIF` onto them regardless of what their
arguments are, so none of the four is a divergence any more. The corners
that used to separate the two engines, checked against MySQL 8.4.11 and now
asserted over the wire in `crates/inlaysql-server/tests/wire.rs`:

| Written | MySQL | Now |
| --- | --- | --- |
| `LENGTH('héllo')` | `6` (bytes) | `6` |
| `HEX(255)` | `'FF'` (the number's value) | `'FF'` |
| `HEX(NULL)` | NULL | NULL |
| `SUBSTRING('hello',0)` | `''` | `''` |
| `SUBSTRING('hello',0,3)` | `''` | `''` |
| `SUBSTRING('hello',-10)` | `''` | `''` |
| `SUBSTRING('hello',2,-1)` | `''` | `''` |
| `SUBSTRING('hello',1,NULL)` | NULL | NULL |
| `SUBSTRING('hello',NULL,2)` | NULL | NULL |
| `NULLIF(1,'1')` | NULL | NULL |

`OCTET_LENGTH` and `BIT_LENGTH`, refused outright before for lack of a
byte-counting primitive, are real mappings onto `octet_length` now too (the
latter as `octet_length(x) * 8`, exact in both engines).

### `ROUND` — narrowed to the one shape a literal's text cannot prove (AHL-465)

MySQL 8.4.11 ties a halfway case to even for a `DOUBLE` argument and away
from zero for an exact `DECIMAL` one — a distinction its own type system
draws between how a value was written or declared, and one this engine
cannot represent, because there is one real-number storage class here, not
two. The shim can only act on what a literal's *text* proves, and an
exponent is the one form MySQL's own parser reads as `DOUBLE`: `ROUND(2.5e0)`
is rewritten onto `mysql_round()`, checked against MySQL 8.4.11, and ties to
even there and here now (`2`, not the `3` this used to answer).

`ROUND(2.5)` — no exponent — is `DECIMAL` to MySQL's parser and is left
exactly as written, reaching the engine's own `round()`, which ties away
from zero; the two agree (`3`), which is why MySQL's own manual gives this
as the *safe* spelling. The gap that is left, reasoned from MySQL's
documented type rules rather than diffed against a live server the way the
literal case was: a column or an expression whose value MySQL would also
treat as `DOUBLE` — a `DOUBLE`- or `FLOAT`-declared column, for instance —
still reaches `round()` unchanged, because this shim has no catalog access
to know a column's declared type and no way to tell a `DOUBLE` expression
from a `DECIMAL` one once both have been reduced to the same `Value::Real`.
`ROUND(x, d)` with a negative literal `d`, refused outright before because
`round()` clamps the digit count to zero, is a mapping regardless of the
first argument's shape — a real answer for the overwhelming majority of
values, in exchange for the same uncorrectable tie-breaking gap on the rare
one that lands exactly on a negative-digit halfway boundary.

### Comparison-time affinity (AHL-486)

SQLite's comparison rule has two stages, and until AHL-486 this engine only
had the second. **Stage one, affinity conversion:** if one operand is a
column (or a `CAST`) with `INTEGER`/`REAL`/`NUMERIC` affinity and the other
is `TEXT` and well-formed as a number, the `TEXT` operand converts to one
first; if instead one side has `TEXT` affinity and the other carries none of
its own, the other operand renders as text instead. **Stage two, storage-
class ordering** (AHL-477): whatever the pair is after stage one, a
still-cross-class comparison ranks `NULL` < numbers < `TEXT` < `BLOB` rather
than raising. `crates/inlaysql-core/src/eval.rs`'s `affinity_conversion`
implements the first stage and `compare_cells` runs it ahead of the second,
unconditionally; `crates/inlaysql-core/src/plan.rs`'s `CompareAffinity` is
what a plan resolves once, at `sql.rs`'s `compare_affinity`, from the two
operand *expressions* — a bare column reference or a `CAST(... AS type)`
carries affinity, an arithmetic expression, a function call or a literal
does not, confirmed against a real sqlite3 3.54 binary and pinned in
`affinity.test`.

Every corner below was checked against sqlite3 3.54, not derived from the
rule's prose:

* `id = '1'` matches an `INTEGER` column; `id = '1x'` and `id = 'abc'` do
  not — the text has to be well-formed as a number for its *entirety* (after
  trimming whitespace), a stricter bar than `CAST`'s prefix parse.
  `id = ' 1 '`, `id = '1.0'` and `id = '1e0'` all still convert.
* `s = 1` against a `TEXT` column does **not** convert `s` to a number; it
  renders `1` as `"1"` and compares as text, so it only matches a `s` that
  literally stores `'1'` — and `s = 1.0` renders as `"1.0"`, not `"1"`, so it
  matches a different row than `s = 1` does. The `INTEGER`/`REAL` distinction
  in that rendering is load-bearing, which is why the borrowed comparison
  path (`ValueRef`, `AHL-478`) reads it from `Cell::as_i64_cell` rather than
  going through the widening `as_f64_cell` every other accessor uses.
* A `BLOB` operand is never affinity-converted in either direction — not
  parsed as a number, and never the target a number renders into either.
* `REAL` and `NUMERIC` affinity columns convert exactly as `INTEGER` ones
  do — this is one `NUMERIC`-affinity conversion, not three.
* `IN (...)`: **the probed expression's own affinity alone decides**, the
  same rule its collation already follows — a candidate's own affinity is
  never consulted. `'1' = id` matches an `INTEGER` column; `'1' IN (id)`
  over a literal list does not, because `sqlite3ExprCodeIN`'s list path only
  ever asks the left operand. `'1' IN (SELECT id FROM ids)` **does** match,
  because a subquery's ephemeral index is built the way an ordinary `=`
  resolves affinity — combining both sides — not the literal-list way.
* `BETWEEN` and a simple `CASE`'s branches combine both sides, the same as
  `=`: a literal probe against typed bounds (`'2' BETWEEN lo AND hi` over
  `INTEGER` columns) still converts.
* A `JOIN`'s `ON` is an ordinary comparison and converts the same way a
  `WHERE` does.
* An indexed column answers identically to an unindexed one: a term whose
  literal needs converting never reaches an index probe in the first
  place — `indexable_probe` already declines a value that is not already the
  column's own storage class, so the query falls back to a full scan (or, for
  a join, the whole inner table) and the *filter* — which does apply
  affinity — is what decides every row, indexed or not.
  `crates/inlaysql-core/tests/btree_index.rs` pins this both for a `WHERE`
  and for a `JOIN`'s `ON`.

**A MySQL client benefits from this beyond plain correctness.** MySQL's own
manual documents the identical coercion for the common shape — a string
constant compared against an `INT`/`DECIMAL` column converts to a number
before comparing — which is exactly why PDO's default of binding a PHP
integer as a string, or mysqli's untyped bind, has never surfaced as a bug
against a real MySQL server: the server was coercing it all along. Before
AHL-486 that same query silently found nothing over this wire; now it finds
the row, matching what a MySQL connection already returned. That equivalence
is reasoned from MySQL's documented rules rather than diffed against a live
server, and it is not exact: MySQL's coercion is more permissive than
SQLite's — it converts a partial number like `'1abc'` too, with a warning,
where the rule this engine implements requires the whole string well-formed
and leaves `'1abc'` as text. The two rules agree on every well-formed number
a bound parameter actually produces, which is the shape that mattered here.

### JSON — formatting and two real behaviour gaps (AHL-490)

Checked against a real MySQL 8 container and a real sqlite3 3.54 binary
rather than assumed. The function/operator mapping itself lives above,
under "JSON functions"; what follows is what still differs once a query
reaches the engine, mapped or not.

* **MySQL formats the JSON text it returns; this engine never does.**
  `JSON_OBJECT('a',1)`/`JSON_ARRAY(1,2)` come back from a real MySQL 8 as
  `{"a": 1}`/`[1, 2]` — a space after every `:` and `,` — where
  `json_object`/`json_array` (and every other function that returns a JSON
  document as text) produce the minified `{"a":1}`/`[1,2]` SQLite always
  has. An application that decodes the result (`json_decode`, `json_decode`
  through Eloquent's cast) never sees this; one that compares the raw text
  byte-for-byte (`WHERE json_col = '{"a": 1}'`, a literal copied from a
  MySQL session) would not match here. This is not narrowed further: it
  would mean porting MySQL's own JSON printer, not fixing a corner.
* **MySQL's `JSON_SET`/`JSON_INSERT` auto-extend an array at *any*
  out-of-range numeric index by appending; `json_set()`/`json_insert()`
  only extend at `$[#]` (checked: `JSON_SET('[1,2,3]', '$[9]', 99)` is
  `[1,2,3,99]` in MySQL and `[1,2,3]` here).** Laravel's own JSON
  column-update path (`compileJsonUpdateColumn`) almost always targets an
  object key from a `->`-joined column selector, where the two engines
  agree exactly; a numeric array index reached through the same path is the
  shape this diverges on, and there is no rewrite that recovers MySQL's
  rule without knowing the array's current length, which is not visible to
  a text-level shim.
* **`->>`'s result type differs.** SQLite's `->>` (and `json_extract`)
  unwraps a JSON scalar to its native SQL type — a JSON integer becomes SQL
  `INTEGER`. MySQL's `->>`/`JSON_UNQUOTE(JSON_EXTRACT(...))` always answers
  a string, even for a JSON number (`JSON_UNQUOTE(JSON_EXTRACT('{"a":1}',
  '$.a'))` is the *string* `'1'` in MySQL). Comparing that string against a
  number relies on MySQL's implicit string-to-number coercion in a `WHERE`,
  which this engine's own comparison-affinity rule (above) only applies
  under narrower conditions. Most real usage decodes the JSON application-
  side rather than comparing the unwrapped text directly, which is why this
  is recorded rather than worked around.

---

## What does not work yet

The protocol is not the limit — the SQL surface is. The list below was
re-derived after the DDL translation landed, by running a corpus of real
Laravel migration and Eloquent statements through the shim and the engine, and
corrected again here (that corpus is not committed — see "Interop, checked by
hand" below — so this correction is targeted verification against specific
statements, cited by file, rather than a fresh full run of it).

**A `BLOB` column cannot be written from a plain quoted string literal**
over the text protocol — `coerce()` in `crates/inlaysql-core/src/sql.rs`
enforces `Value::Text` ≠ `Value::Blob` strictly, unlike real SQLite's weak
BLOB affinity (which stores `TEXT` into a `BLOB` column unchanged). The MySQL
wire's text protocol has no separate binary-literal syntax, so a `BLOB` column
is only writable through the true binary protocol today; this looks like a
deliberate strictness choice rather than an oversight, so it is recorded
rather than changed.

**A stock ORM's migrations get further now, but ordinary Eloquent traffic
after them gets further still — the wall has moved six times: past
`auto_increment` first, then past everything Phase 1b (AHL-412) implements,
then past the post-creation index and constraint DDL AHL-471 found waiting
right behind it, then past the qualified `UPDATE ... SET` target AHL-474
found waiting behind that and AHL-475 closed, then past MySQL's own
`ON DUPLICATE KEY UPDATE` upsert syntax AHL-475 found waiting behind *that*
and AHL-476 closed, then past `UNION`/`INTERSECT`/`EXCEPT` and non-recursive
`WITH`, which Phase 1c items 2 and 3 (AHL-473) landed in core the same day.**
`NOT NULL`,
`DEFAULT`, `UNIQUE`, `CHECK` and foreign keys are real constraints now,
column- and table-level alike — foreign keys are recorded and left
unenforced, which is SQLite's own default, not a gap (`constraints.test`).
`TIMESTAMP`, `DATETIME`, `JSON`, `LONGTEXT`, `MEDIUMTEXT`, `MEDIUMINT` and
every other declared type name now resolve under SQLite's five affinity rules
— there is no column type this server refuses any more (`affinity.test`;
`resolve_data_type` in `sql.rs` accepts any type name, exactly as SQLite
does). `DROP TABLE [IF EXISTS]`, `CREATE TABLE IF NOT EXISTS` and SQLite's
four `ALTER TABLE` operations (`ADD COLUMN`, `RENAME TO`, `RENAME COLUMN`,
`DROP COLUMN`) all work (`ddl.test`). Every upsert form (`INSERT OR
IGNORE`/`REPLACE`, `ON CONFLICT DO NOTHING`/`DO UPDATE` with `excluded`),
`INSERT ... SELECT` and `RETURNING` on `INSERT`/`UPDATE`/`DELETE` work
(`write_statements.test`, `returning.test`). The exact statement a schema
builder sends first — backticked identifiers, `unsigned auto_increment
primary key`, `not null`, `timestamp`, `engine=InnoDB default charset=utf8mb4
collate=...` — now runs over the wire and the table it builds actually works
(`crates/inlaysql-server/tests/wire.rs`:
`a_schema_builders_create_table_runs_and_the_table_it_makes_works`,
`the_full_migration_statement_now_runs`; core-level acceptance/refusal list:
`crates/inlaysql-core/tests/laravel_migrations.rs`). **And now (AHL-474) the
statement right after it — the separate `ALTER TABLE ... ADD
{INDEX|UNIQUE|CONSTRAINT}` a fluent `->unique()`/`->index()`/`->foreign()`
compiles to — runs too, along with `TRUNCATE TABLE` and the standalone
`RENAME TABLE`**; see
[Post-creation index and constraint DDL is translated too](#post-creation-index-and-constraint-ddl-is-translated-too-phase-3-ahl-474)
above and `a_realistic_laravel_migration_sequence_runs_end_to_end` in
`wire.rs` for the full sequence run end to end.

Subqueries landed in core with AHL-463, so `whereHas`, `withCount` and
`whereIn` with a sub-select now go through — in a `SELECT`, including the
query of an `INSERT ... SELECT`. `UNION`, `INTERSECT`, `EXCEPT` and
non-recursive `WITH` landed with AHL-473 (Phase 1c items 2 and 3) and now run
too — asserted end to end in `set_operations.test` and `ctes.test`, every
expectation there produced by the sqlite3 binary, not this engine. A subquery
in an `UPDATE`, `DELETE` or `INSERT ... VALUES` is still refused at prepare
time (`UPDATE`/`DELETE`/`INSERT ... VALUES` build their expression
environment and then take the engine mutably to write, so that environment
cannot hold the shared borrow reading a subquery needs), and so is
`WITH RECURSIVE`, a forward- or self-referencing non-recursive CTE, a
parenthesised compound arm, `INTERSECT ALL`/`EXCEPT ALL`, a bare `VALUES`
query, an arbitrary expression or a qualified column in a compound's
`ORDER BY`, and `WITH` in front of `UPDATE`/`DELETE`/`INSERT` rather than
`SELECT` (`unsupported.test` pins each). None of that is what a stock
migration or ordinary Eloquent CRUD reaches — `CREATE TABLE` with
decorations, the post-creation index and constraint DDL, a model save with a
qualified `updated_at`, `upsert()`'s own `ON DUPLICATE KEY UPDATE`, a
paginated `SELECT` with `COUNT(*)`, `whereIn`, an eager-load `JOIN`, a
`whereHas` subquery, a `withCount` subquery and `WHERE DATE(...)` all run
end-to-end without touching any of it. What is left, now that AHL-473 has
closed the `UNION`/CTE gap that used to be item 1 here and AHL-486 has closed
the comparison-affinity gap that used to be item 2:

1. **The unmapped MySQL-named scalar functions are refused (1235).** `NOW()`,
   `CONCAT()`, `RAND()`, `CHAR_LENGTH()`, `LOCATE()`, `LEFT()`, `IF()`,
   `YEAR()`, and — since AHL-465 — `LENGTH`, `HEX`, `SUBSTRING`, `NULLIF` and
   the one `ROUND` shape a literal's text proves, all work now; see
   [the mapped list](#mysql-named-scalar-functions-are-mapped-not-guessed).
   `GREATEST`, `MOD`, `CONCAT_WS`, `DATE_FORMAT`, `DATEDIFF`, `FROM_UNIXTIME`
   and the others in the refused list do not, and say why
   (`crates/inlaysql-server/src/mysqlfunc.rs`, still current). Nothing
   arithmetic beyond `ABS` and `ROUND` exists in core at all — no `POW`,
   `SQRT`, `CEIL`, `FLOOR`, `TRUNCATE`, `SIGN` — and no padding or hashing
   (`LPAD`, `REPEAT`, `MD5`). **Core's**, if they are wanted; a shim cannot
   invent an arithmetic primitive.
2. **`percent_rank()`/`cume_dist()` and a value-based `RANGE`/`GROUPS`
   window frame are refused (1235).** AHL-494 implemented the rest of the
   window function surface — see
   [Window functions](#window-functions-ahl-494) above — and both gaps are
   the engine's, not the shim's, the same way item 1 above is.

Those two items are the engine's to fix, not the shim's — every shape
this shim could plausibly translate has been, now. The shim's own
already-confirmed gaps in this list were the untranslated
`ALTER TABLE`/`TRUNCATE`/`RENAME TABLE` shapes, which is what AHL-474 closed;
a MySQL client's own qualified `UPDATE ... SET` target, which is what AHL-475
closed the same way; and MySQL's own `ON DUPLICATE KEY UPDATE` upsert syntax,
which is what AHL-476 closed, the same way again. None of it is a protocol
problem: every one arrives as a proper MySQL error code, on a connection that
stays usable afterwards.

**Fixed since this list was last written**, by Phase 1a rather than by this
change: `OFFSET`, `LIMIT ?`, `DISTINCT`, multi-key `ORDER BY`, `LIKE`, `IN`,
`BETWEEN`, `CASE`, `CAST`, `||`, `COUNT(DISTINCT)`, `GROUP_CONCAT`, and the
scalar and date/time function library. **Fixed by Phase 1b (AHL-412):** every
declared constraint, every column type via SQLite's affinity rules,
`DROP TABLE`/`CREATE TABLE IF NOT EXISTS`/SQLite's `ALTER TABLE`, and every
upsert/`INSERT ... SELECT`/`RETURNING` form — see the corrected intro above;
this is the correction the docs truth pass named. **Fixed by AHL-463:**
subqueries in every read position. **Fixed by AHL-474:** the `ALTER TABLE ...
ADD {INDEX|UNIQUE|CONSTRAINT}` shapes `->unique()`/`->index()`/`->foreign()`
compile to, `TRUNCATE TABLE` and standalone `RENAME TABLE` — the previous
version of this list's items 1 and 2. **Fixed by AHL-475:** a qualified
column on the left of `UPDATE ... SET` — `update users set name = ?,
users.updated_at = ?`, what Eloquent writes on every save of a model with
timestamps — the previous version of this list's item 1. Checked directly
against a real `sqlite3` binary first: SQLite's own grammar has no qualified
assignment target at all, right table name, wrong table, aliased or not, so
this was never a gap in core's SQLite dialect to begin with, and core still
refuses it, on purpose, with a clearer message
(`inlaysql_core::sql::assignment_target_column`). The fix is
`crates/inlaysql-server/src/mysqlddl.rs`'s `update_set`: a qualifier naming the
statement's own table, or its alias once one is given, is stripped before the
statement reaches core's parser; a qualifier naming anything else is refused
by name (`1109 ER_UNKNOWN_TABLE`, MySQL's own code and wording for exactly
this) rather than passed through for core's generic "no schemas" refusal.
**Fixed by AHL-476:** MySQL's own upsert syntax, `INSERT ... ON DUPLICATE KEY
UPDATE col = VALUES(col), ...` — what Eloquent's `upsert()` and a single-row
`updateOrCreate()`-shaped upsert both compile to — the previous version of
this list's item 2. Core still refuses the clause by name, on purpose
(`inlaysql_core::sql::resolve_on_conflict`: `ON DUPLICATE KEY UPDATE is MySQL
syntax; write ON CONFLICT ... DO UPDATE`), because it is real MySQL syntax
with no place in SQLite's dialect, the same reasoning as every other item D1
governs. The fix is `crates/inlaysql-server/src/mysqlddl.rs`'s
`insert_on_duplicate_key_update`: the clause is rewritten onto core's own
`ON CONFLICT DO UPDATE SET ...` before the statement reaches core's parser,
with `VALUES(col)` (bare or backtick-quoted — Eloquent's grammar quotes the
function name too, `` `values`(`col`) ``) and MySQL 8.0.20+'s row-alias form
(`... AS new ON DUPLICATE KEY UPDATE col = new.col`) both becoming
`excluded.col`. **No conflict target is added, and this needed checking
directly rather than assuming:** the obvious worry is that a table with more
than one unique constraint would need the target resolved from the catalog,
or the statement refused as ambiguous, because SQLite's `DO UPDATE` reads as
needing one. Checked directly against a real `sqlite3` binary first, that
worry does not hold — `ON CONFLICT DO UPDATE SET ...` with no `(target)` at
all is valid SQLite, not only for `DO NOTHING`, and it resolves against *any*
colliding constraint, primary key included. Checked against
`inlaysql-core` next: `resolve_conflict_target` in `sql.rs` returns `None`
for a target-less clause without consulting the catalog at all, and
`Engine::insert` in `engine.rs` honours a `None` target by answering for the
first constraint any candidate row collided on, whichever one that is. That
is exactly MySQL's own rule for this clause — it has no target either, and
fires on a collision with any unique or primary key — so a bare
`ON CONFLICT DO UPDATE` is not a narrower stand-in for MySQL's clause on a
table with several unique constraints; it is the same clause, and this
translation needs no catalog lookup and refuses nothing there. The one corner
this does refuse (`1235`) is a row-alias *column list*
(`AS new (a, b) ON DUPLICATE KEY UPDATE x = new.a`): resolving `new.a` needs
the real column each alias renames, which is a corner Eloquent never emits
and this shim does not chase into the catalog for.

**The affected-rows count for this translation is not MySQL's 0/1/2
convention, and that is a documented divergence, not a bug** — see
[The upsert's affected-rows count does not match MySQL's](#the-upserts-affected-rows-count-does-not-match-mysqls)
under Divergences below. Fixed by the shim since then: the
MySQL-named spellings of the scalar function library, in
[the mapped table](#mysql-named-scalar-functions-are-mapped-not-guessed), and
— since AHL-465 — `LENGTH`, `HEX`, `SUBSTRING`, `NULLIF` and `ROUND`'s one
literal-with-an-exponent shape too.

**Fixed by AHL-473:** `UNION`, `UNION ALL`, `INTERSECT` and `EXCEPT`, and
non-recursive `WITH` — Phase 1c items 2 and 3, and the previous version of
this list's item 1. No shim change was needed: this is a straight core
addition (`crates/inlaysql-core/src/sql.rs`, `set_operations.test`,
`ctes.test`), so a compound query or a CTE that already ran on a real MySQL
connection reaches the engine unchanged and now runs there too. `WITH
RECURSIVE`, a forward- or self-referencing non-recursive CTE, a
parenthesised compound arm, `INTERSECT ALL`/`EXCEPT ALL` and `WITH` in front
of a write statement are still refused — see the narrower ground listed
above, under [What does not work yet](#what-does-not-work-yet), for exactly
what stayed behind. AHL-477, landed the same day, is worth naming here too
even though it fixed a bug rather than closing a gap: three cross-storage-
class comparators (`ORDER BY`/`GROUP BY`, `MIN`/`MAX`, and `WHERE`'s own
`comparison`) each answered a `TEXT`-vs-`INTEGER`-shaped pair its own wrong
way — one of them silently, which is exactly the failure class this project
watches hardest for. AHL-477's fix was stage two of SQLite's comparison rule
only — see AHL-486 immediately below for stage one, which is what closed the
gap this same paragraph used to describe.

**Fixed by AHL-486:** comparing a bound parameter or literal against a
column now applies SQLite's comparison affinity, which is stage one of the
two-stage rule AHL-477 only implemented stage two of. Before AHL-486,
`WHERE id = '1'` against an `INTEGER` column raised `1366`,
`cannot compare INTEGER and TEXT`, until AHL-477 rewrote `comparison()` in
`crates/inlaysql-core/src/eval.rs` to defer to `mem_cmp`'s storage-class
order — which fixed a real bug (a comparator that answered "equal" for a
class pair it had no rule for, corrupting whatever it sorted) but, with
nothing applying affinity first, turned the loud `1366` into a **silent
wrong row count**: `id = '1'` matched zero rows instead of the one sqlite3
finds, because `INTEGER`'s storage class ranks below `TEXT`'s and the
comparison was simply false, never an error — a loud failure had become a
silent one, which is strictly worse. AHL-486 adds the missing stage:
`sql.rs` resolves each comparison's affinity at plan time from the column's
declared type (or a `CAST`'s target type — an expression carries no affinity
of its own), and `eval.rs`'s `affinity_conversion` applies it to the
opposing operand before `compare_cells`'s class-order ranking ever runs, so
a `TEXT` operand becomes a number when the other side has `INTEGER`/`REAL`/
`NUMERIC` affinity and is well-formed as one, and a numeric operand renders
as text when the other side has `TEXT` affinity and nothing else does.
`mem_cmp` itself is untouched — AHL-477's property tests, the index key
encoding and `DISTINCT`/set-operation dedup all still pin the identical total
order — this is a conversion *before* that order is asked, not a change to
it. That matters because **PDO binds PHP integers as strings by default**, so
the ordinary

```php
$stmt = $pdo->prepare('SELECT * FROM users WHERE id = ?');
$stmt->execute([1]);          // used to silently return 0 rows; now finds the row
```

now finds the row exactly as `$stmt->bindValue(1, 1, PDO::PARAM_INT)` and
mysqli's `bind_param('i', $id)` already did. **This is a wire-fidelity
improvement, not only a correctness fix**: MySQL's own manual documents the
identical coercion for this shape — comparing a string constant against an
`INT`/`DECIMAL` column converts the constant to a number before comparing —
so a client that has only ever spoken to a real MySQL server and leans on
that behaviour (which the PDO default above does, silently, by design) now
gets the answer it already expected, where it previously got a wrong one.
That equivalence is not exhaustive and was not checked against a live MySQL
server here: MySQL's own numeric coercion is more permissive than SQLite's
(it will coerce a *partial* number like `'1abc'` with a warning, where
SQLite's rule — the one this engine now implements — requires the whole
trimmed string to be well-formed, `'1abc'` included, and leaves it as text
otherwise), so the two are not the same rule in general, only equivalent on
the well-formed case a bound parameter actually produces. `Error::Type` is
still raised for exactly what it always was outside this shape — a write
that violates a column's enforced affinity, an arithmetic overflow, a
`VECTOR` compared against anything — so `1366` has not become unreachable,
only this particular route to it has closed. See
[Comparison-time affinity](#comparison-time-affinity-ahl-486) for the corners
verified against a real sqlite3 3.54 binary and where core's fix lives.

**Fixed by AHL-490:** JSON. `json_extract`/`json_set`/`json_insert`/
`json_replace`/`json_remove`/`json_valid`/`json_type`/`json_quote`/
`json_array`/`json_object`/`json_array_length`/`json()` and the `->`/`->>`
operators are real now (`crates/inlaysql-core/src/json.rs`, a hand-rolled
parser/serializer/path language — no new dependency, the same rule that
already governs the MySQL wire protocol and SHA-1/SHA-256 in this crate),
which is what Laravel's `casts => ['x' => 'array']` and `whereJson*` need —
see [JSON functions](#json-functions-ahl-490) above for the mapping and
[JSON — formatting and two real behaviour gaps](#json--formatting-and-two-real-behaviour-gaps-ahl-490)
under Divergences for what still differs. Still refused, and not planned as
part of this change: `json_each`/`json_tree` (table-valued — this engine has
no mechanism for a function that returns rows in `FROM` at all, the same
refusal any other table-valued function gets) and `json_patch` (not
implemented; falls through to the engine's ordinary `no such function`).
`JSON_CONTAINS`/`JSON_OVERLAPS` (`whereJsonContains`/`whereJsonOverlaps`)
have no InlaySQL primitive without `json_each()` either, so they are refused
by the shim rather than left to a worse error — see the refused-functions
table above.

---

## Testing

`crates/inlaysql-server/tests/wire.rs` is the merge gate: a MySQL client
written from the protocol description — its own packet framing, length-encoded
integers, result-set framing and binary row decoding — driven against a real
server on a real socket. Reusing the server's own framing to test the server's
framing would only prove it agrees with itself.

It runs in plain `cargo test`, needs no Docker, and binds an **ephemeral port**
(the server is asked for port 0 and reports what it got), so several copies can
run at once without assuming 3306 is free.

It covers: connect and authenticate under `caching_sha2_password` (both the
fast path and the full-authentication fallback), under `mysql_native_password`
via `AuthSwitchRequest`, and the RSA public-key exchange being refused rather
than attempted (AHL-467); DDL, insert, select; `affected_rows` and
`last_insert_id` in all four of their cases; prepared statements with bound
integer, string and NULL parameters, reused and reset and closed, and a
prepared `SELECT`'s real column definitions — names and wire types — rather
than the zero `COM_STMT_PREPARE` used to report, alongside a shim-answered
prepared statement still reporting zero (AHL-466); error codes for missing
table, duplicate key, syntax error, unsupported feature and missing column;
the shim's `SHOW`/`information_schema` answers, including through a prepared
statement with bound parameters; a metadata filter that must fail rather than
answer wrongly; transactions including a rollback that really rolls back and
`autocommit=0`; savepoints being refused; wrong password (under every plugin
this server completes), wrong user and a forged token; the connection cap;
four concurrent connections writing disjoint rows; one connection seeing
another's commits; and a 400 KB value that has to be split across packets and
put back together.

For the DDL translation it covers, over the wire: a schema builder's
`create table` — unsigned auto-increment key, `varchar(255)`, `ENGINE=InnoDB
DEFAULT CHARSET=utf8mb4 COLLATE=…` — running, reporting five warnings that name
every clause removed, and then **an `INSERT` that gets an auto-assigned key and
a `SELECT` that reads the rows back**, because a `CREATE TABLE` that parses and
builds the wrong table would pass a weaker test; the same statement written the
way a real migration writes it failing on `NOT NULL` and then on `TIMESTAMP`
rather than on `auto_increment`, and leaving no half-made table behind; each
refused clause arriving as `1235` with the clause named; the translation
applying identically on the prepared path; warnings being cleared by the next
statement but not by `SHOW WARNINGS` itself; and the online-DDL steering
(`ALGORITHM=`/`LOCK=`) coming off an `ALTER TABLE` before the operation
underneath it runs (`online_ddl_steering_is_removed_and_the_alter_runs`).

For the post-creation index and constraint DDL (AHL-474) it covers, over the
wire, each with an index that is actually usable afterwards rather than only
a statement that parses: `ADD INDEX`/`ADD KEY` and `ADD CONSTRAINT ... UNIQUE`
becoming a working `CREATE [UNIQUE] INDEX`, the unique one enforcing a real
duplicate-key `1062`
(`add_index_and_add_unique_create_indexes_that_actually_work`); a `->unique()`
compiled onto a column whose table carried `_ci` colliding `'Ada'` and `'ADA'`
the way `WHERE` already does since AHL-469
(`add_unique_on_a_nocase_column_collides_case_insensitively`); a composite
index named after its first column only
(`a_composite_add_index_is_named_after_its_first_column`); `ADD CONSTRAINT ...
FOREIGN KEY` answering OK with exactly one `1618` naming what was not
recorded and touching the engine not at all, and the constraint really
staying unenforced (`add_constraint_foreign_key_is_ok_with_a_warning_and_stays_unenforced`);
`DROP INDEX`/`DROP KEY` removing it while leaving the table usable
(`drop_index_removes_it_and_the_table_still_works`); `RENAME INDEX` refused
with the clause named, and the connection and the index both still there
afterwards (`rename_index_is_refused_over_the_wire`); a multi-operation
`ALTER TABLE` running every operation and proving the sequence is **not**
atomic — an earlier operation's effect survives a later operation's failure —
by making exactly that happen
(`a_multi_operation_alter_runs_each_operation_and_is_not_atomic_on_failure`);
`TRUNCATE TABLE`, with and without the `TABLE` keyword, deleting every row and
carrying the one `1618` that says the row id counter did not reset, checked
by inserting again and reading its id back
(`truncate_table_deletes_rows_and_does_not_reset_the_row_id_counter`,
`truncate_without_the_table_keyword_works_too`); standalone `RENAME TABLE`
renaming a table with data in it and no warning
(`standalone_rename_table_renames_it`); and the full sequence — `CREATE TABLE`
with decorations, `ADD INDEX`, `ADD UNIQUE`, `ADD CONSTRAINT ... FOREIGN KEY`,
`DROP INDEX` — run in the order a real Laravel migration sends them, with
every warning read back through `SHOW WARNINGS` and every index left standing
enforcing what it says
(`a_realistic_laravel_migration_sequence_runs_end_to_end`). The translation
itself — every rewrite and every refusal, clause by clause, including the
name-synthesis suffixing against both the catalog and the rest of the same
statement — is unit-tested directly in `mysqlddl.rs`, independent of the wire.

For MySQL's own upsert syntax (AHL-476) it covers, over the wire: Eloquent's
`upsert()` shape — a prepared, bound, multi-row `INSERT ... ON DUPLICATE KEY
UPDATE` with the backtick-quoted `` `values`(`col`) `` spelling Eloquent's
own grammar sends — running the insert path and the update path in the same
statement and leaving the right values in each row
(`eloquents_upsert_runs_the_insert_path_and_the_update_path_together`); a
single-row upsert shaped like `updateOrCreate()`, run once against an
existing key and once against a new one, checking both `affected_rows` and
`LAST_INSERT_ID()` for each path
(`a_single_row_upsert_updates_an_existing_key_and_inserts_a_new_one`); the
bare, unquoted `VALUES(col)` spelling and the MySQL 8.0.20+ row-alias form
both translating and running
(`the_mysql_8_0_20_row_alias_form_also_translates`); a statement whose
proposed row collides with nothing, which must insert rather than silently
doing nothing
(`on_duplicate_key_update_never_fires_without_a_real_collision`); the crux
this fix rests on — a table with **two separate unique constraints**,
neither named as a target, taking the upsert without an ambiguity refusal
(`a_table_with_two_unique_constraints_is_not_refused_as_ambiguous`); and an
empty `ON DUPLICATE KEY UPDATE` clause refused as a syntax error with the
connection still usable afterward
(`an_empty_on_duplicate_key_update_is_refused_over_the_wire`). The
translation itself — `VALUES(col)` in both spellings, the row-alias form,
the row-alias column-list refusal, and the catalog-independent multi-unique
case — is unit-tested directly in `mysqlddl.rs`, independent of the wire.

For the collation mapping it covers, over the wire: the same schema-builder
`create table` with `collate=utf8mb4_unicode_ci`, then `WHERE name = 'ADA'`
matching a stored `'ada'` — the divergence this document called the most
dangerous in the project, asserted at the level a client sees it; the `1618`
naming both the mapping and the accent gap; an accented value *not* matching,
so the documented gap is a tested one rather than a claim; a `utf8mb4_bin`
column keeping its case-sensitivity beside a case-insensitive one in the same
table; and `SHOW FULL COLUMNS` reporting a collation name that means what the
column does.

For the scalar function mapping it covers, over the wire: every mapped function
against the values a real MySQL answers with — NULLs, empty strings, zero,
negative and past-the-end lengths, and multi-byte UTF-8 — including the two
that would fail *silently* if the mapping were written from memory
(`LOCATE`'s reversed arguments, and `RIGHT(s, 0)`); the mappings over real
columns in `SELECT`, `WHERE`, `UPDATE` and `DELETE`, with a NULL row; a
prepared statement whose placeholder survives the rewrite; each refusal
arriving as `1235` with the input that decided it named in the message, and the
connection still usable afterwards; and a statement that *stores* the text
`CONCAT(1,2)` storing those characters rather than evaluating them.

The AHL-465 primitives get the same discipline: every corner
[the Divergences table above](#length-hex-substring-and-nullif--closed-ahl-465)
names — `LENGTH`, `HEX`, `SUBSTRING`, `NULLIF` and `ROUND`'s literal-with-an-
exponent shape — asserted over the wire against the value docs/server.md
records for it, plus `MID`, `OCTET_LENGTH` and `BIT_LENGTH` now answering
instead of refusing, and `ROUND` over a column or a plain decimal literal
staying exactly what it was before (`crates/inlaysql-server/tests/wire.rs`,
`the_five_previously_divergent_functions_now_answer_what_mysql_answers`).
`inlaysql-core`'s own unit tests cover the primitives directly, independent
of the shim's rewriting (`crates/inlaysql-core/src/eval.rs`).

JSON (AHL-490) gets the same two-layer discipline: the mapping and refusal
tables above are unit-tested directly in `mysqlfunc.rs`
(`json_length_maps_onto_json_array_length`,
`json_contains_path_maps_only_laravels_one_path_shape`,
`json_unquote_of_json_extract_becomes_the_arrow_operator`,
`json_quote_and_json_type_are_refused_not_left_to_diverge_silently`, and
`json_functions_spelled_the_same_reach_the_engine_directly` for the ones that
need no rewrite at all), and the Laravel-shaped queries run end to end over
the wire — `json_extract`/`->`/`->>` reads, `JSON_UNQUOTE(JSON_EXTRACT(...))`
(`wrapJsonSelector`), `JSON_LENGTH`/`JSON_CONTAINS_PATH`
(`whereJsonLength`/`whereJsonContainsKey`), a `json_set`-based column update,
and NULL propagation through all of it
(`crates/inlaysql-server/tests/wire.rs`,
`json_functions_answer_what_laravel_over_mysql_needs`) — and every refused
name answers `1235` on a connection that stays usable afterwards
(`refused_json_functions_answer_1235_not_a_dropped_connection`). The engine's
own hand-rolled JSON parser, path language and mutating functions are tested
independently of the shim in `crates/inlaysql-core/src/json.rs`,
`crates/inlaysql/tests/sqllogictest/json.test` (every expectation produced by
a real sqlite3 binary) and the differential suite against `rusqlite`
(`crates/inlaysql/tests/differential.rs`).

Alongside it, 230 unit tests cover the packet layer, SHA-1 and SHA-256 against
their published vectors, `caching_sha2_password`'s scramble cross-checked
against an independent implementation, the error map, the session, the text
utilities, the shim, the DDL translation clause by clause, and the function
mapping call by call — including that a statement with nothing to translate is
returned byte for byte.

### Interop, checked by hand

The wire test proves the server matches *a* reading of the protocol. It was
also driven from PHP 8.4 (mysqlnd) — PDO with emulated prepares, PDO with
native prepares, and mysqli — because a hand-rolled server and a hand-rolled
test client can share a misunderstanding, and a driver nobody here wrote cannot.
That is where the SQLSTATE mistake above was found. Those checks are manual and
not committed; a containerised client test is a separate piece of work.

**The function mappings were derived the same way, and that derivation is the
reason to believe them.** A `mysql:8` container (8.4.11, session time zone UTC)
and this server were driven from the *same* `mysql` client, over the same
statements, and the two outputs diffed. Every mapping in the table above came
out identical across roughly 280 expressions and rows — literals first, then the
same functions over a real table holding a NULL row, an empty string, a
negative number and multi-byte text, declared `COLLATE utf8mb4_bin` on the
MySQL side so the collation gap could not mask a mistake in the mapping. Three
functions failed that diff and are the reason three entries moved to the refused
list rather than the mapped one — `FROM_UNIXTIME` most instructively, whose
obvious mapping was written, diffed against `FROM_UNIXTIME(-1)`, and removed.
The values that survived are what the wire tests assert; the diff itself is a
derivation rather than a fixture, and is not committed.

The [failure order](#what-does-not-work-yet) above was re-derived the same way:
a corpus of the statements Laravel 11's own migrations and Eloquent emit, run
through the shim and the engine one at a time, against tables created in the
engine's own dialect so that a statement is never blocked merely by the table
its own migration failed to create. That corpus is not committed either — it
is a derivation, not an assertion, and pinning it as a test would freeze a list
whose whole purpose is to change. Re-derive it when the next phase lands rather
than trusting this copy.
