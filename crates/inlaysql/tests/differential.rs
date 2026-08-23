//! Differential testing against SQLite: the same random query, two engines,
//! one answer.
//!
//! # Why this exists next to the metamorphic tests
//!
//! `inlaysql-core/tests/logic_bugs.rs` compares the database against *itself*
//! — TLP and row retrieval — which catches a predicate that is inconsistent.
//! It cannot catch a predicate that is consistently wrong: if `a > 5` and
//! `NOT (a > 5)` both treat `NULL` the same wrong way, the partition still
//! holds and the test still passes.
//!
//! An oracle catches that, and the project already has one. The dialect's
//! stated baseline is SQLite compatibility, so SQLite *is* the specification;
//! a disagreement is a bug in InlaySQL by definition, which makes this the
//! sharpest logic-bug test available here. It is the same role SQLancer's
//! differential mode plays, run against the surface we actually have — see
//! `docs/sqlancer.md` for what real SQLancer would add and what it needs
//! first.
//!
//! # Keeping a disagreement meaningful
//!
//! The generator stays inside the ground both engines stand on:
//!
//! * **Mostly type-consistent comparisons** — integer columns against integer
//!   literals, text against text — **plus a deliberate cross-affinity arm**
//!   (AHL-486): `leaf()`'s arms 20-29 compare `a` (`INTEGER`) against a
//!   `TEXT` literal and `b` (`TEXT`) against an `INTEGER` one, on purpose.
//!   This is the shape 50,000 rounds over AHL-477 never generated, which is
//!   exactly why they never caught the missing comparison-affinity
//!   conversion: the grammar could not express `WHERE <typed column> <op>
//!   <literal of another storage class>` at all, so a bug only reachable
//!   through it had zero probability of being rolled. sqlite3 remains the
//!   oracle for the *outcome*, same as everywhere else in this file — the
//!   generator does not encode what the affinity rule should decide, only
//!   that both engines have to decide it the same way.
//! * **Only the implemented surface.** A test that fails for "unsupported" is
//!   noise. Joins, aggregates, subqueries and derived tables each have their
//!   own generator further down this file, all staying inside the same
//!   type-consistent (plus the one deliberate exception above), `NULL`-aware
//!   ground.
//! * **`NULL` everywhere it is allowed**, because three-valued logic is where
//!   the bugs are.
//! * **Row sets, ordered by primary key**, so no result depends on either
//!   engine's unspecified ordering.
//!
//! Every round is a pure function of the seed, and the seed is in the failure
//! message.

use inlaysql::{Database, Outcome, Value};
use inlaysql_core::mem::SeededRng;
use inlaysql_core::Rng;

/// Rounds per run. Each round is a fresh table and a fresh predicate.
///
/// Small enough to belong in every `cargo test`; a longer campaign is the same
/// test with `INLAYSQL_DIFFERENTIAL_ROUNDS` set, which is how CI runs it.
/// Seeds are consecutive from zero, so a longer run is a superset of a shorter
/// one and never re-rolls what already passed.
const DEFAULT_ROUNDS: u64 = 200;
/// Rows per generated table. Small on purpose: a failure a human has to read
/// should fit on a screen.
const ROWS: usize = 12;
/// Distinct integer values drawn from, so predicates match sometimes and miss
/// sometimes rather than always doing one or the other.
const VALUE_RANGE: u64 = 8;

const WORDS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

/// `LIKE` patterns worth generating: both wildcards, at every position, and
/// the ASCII case folding that is `LIKE`'s one real quirk.
const LIKE_PATTERNS: [&str; 14] = [
    "a%", "%a", "%a%", "_a%", "alpha", "ALPHA", "Al_ha", "%", "_", "", "delta", "DELT_", "%t%",
    "gamm",
];

/// One generated row: `(a, b)` where either may be `NULL`.
#[derive(Debug, Clone)]
struct Row {
    a: Option<i64>,
    b: Option<&'static str>,
}

fn generate_rows(rng: &mut SeededRng) -> Vec<Row> {
    (0..ROWS)
        .map(|_| Row {
            // One in five is NULL: often enough that a predicate meets one,
            // rare enough that the table is not mostly holes.
            a: (!rng.next_u64().is_multiple_of(5)).then(|| (rng.next_u64() % VALUE_RANGE) as i64),
            b: (!rng.next_u64().is_multiple_of(5)).then(|| WORDS[(rng.next_u64() % 4) as usize]),
        })
        .collect()
}

/// A random predicate over `a` and `b`, in SQL both engines parse identically.
fn predicate(rng: &mut SeededRng, depth: u32) -> String {
    // Leaves at the bottom, and mostly leaves above it, so expressions stay
    // readable when one of them fails.
    if depth == 0 || rng.next_u64().is_multiple_of(3) {
        return leaf(rng);
    }
    match rng.next_u64() % 4 {
        0 => format!(
            "({} AND {})",
            predicate(rng, depth - 1),
            predicate(rng, depth - 1)
        ),
        1 => format!(
            "({} OR {})",
            predicate(rng, depth - 1),
            predicate(rng, depth - 1)
        ),
        2 => format!("(NOT {})", predicate(rng, depth - 1)),
        _ => leaf(rng),
    }
}

fn leaf(rng: &mut SeededRng) -> String {
    let value = rng.next_u64() % VALUE_RANGE;
    let other = rng.next_u64() % VALUE_RANGE;
    let word = WORDS[(rng.next_u64() % 4) as usize];
    let second = WORDS[(rng.next_u64() % 4) as usize];
    let pattern = LIKE_PATTERNS[(rng.next_u64() as usize) % LIKE_PATTERNS.len()];
    match rng.next_u64() % 30 {
        0 => format!("a = {value}"),
        1 => format!("a <> {value}"),
        2 => format!("a < {value}"),
        3 => format!("a > {value}"),
        4 => format!("a <= {value}"),
        5 => format!("a >= {value}"),
        6 => format!("b = '{word}'"),
        7 => format!("b <> '{word}'"),
        8 => "a IS NULL".to_string(),
        9 => "b IS NULL".to_string(),
        10 => format!("b LIKE '{pattern}'"),
        11 => format!("b NOT LIKE '{pattern}'"),
        12 => format!("a IN ({value}, {other})"),
        13 => format!("a NOT IN ({value}, {other})"),
        14 => format!("a IN ({value}, NULL)"),
        15 => format!("b IN ('{word}', '{second}')"),
        16 => format!("b NOT IN ('{word}', NULL)"),
        17 => format!("a BETWEEN {value} AND {other}"),
        18 => format!("a NOT BETWEEN {value} AND {other}"),
        19 => format!("b BETWEEN '{word}' AND '{second}'"),
        // ------------------------------------------------------- AHL-486
        //
        // `a` is `INTEGER`, `b` is `TEXT` — every arm below compares one of
        // them against a literal of the *other* storage class, which is
        // exactly the shape 50,000 differential rounds over AHL-477 never
        // generated and so never caught the missing affinity conversion.
        // sqlite3 is still the oracle; nothing here assumes what the answer
        // should be, only that both engines have to agree on it.
        //
        // The well-formed numeral text a `TEXT` literal converts to a number
        // for: the exact shape the issue was filed over (`id = '1'`).
        20 => format!("a = '{value}'"),
        21 => format!("a <> '{value}'"),
        22 => format!("a < '{value}'"),
        // Leading/trailing whitespace is still "well-formed" — SQLite trims
        // it before deciding whether the text is a number.
        23 => format!("a = ' {value} '"),
        // Not well-formed: a trailing letter is not a number at all, so this
        // stays a class-order comparison (`a` never equals `TEXT`) rather
        // than converting — the corner that used to raise `Error::Type`
        // before AHL-477, then silently mismatch after it.
        24 => format!("a = '{value}x'"),
        // `b` has `TEXT` affinity: the *other* operand renders as text
        // instead, so this only matches a `b` that is literally the numeral
        // itself, not any `INTEGER` interpretation of it — `WORDS` never
        // looks like a number, so this exercises the conversion (and the
        // rendering) without ever coincidentally matching by accident.
        25 => format!("b = {value}"),
        26 => format!("b <> {value}"),
        // `IN` resolves every candidate under the probed expression's own
        // affinity — `a`'s, here — never the candidates' own, which is a
        // real divergence from a written `=` confirmed against sqlite3.
        27 => format!("a IN ('{value}', '{other}')"),
        28 => format!("a NOT IN ('{value}', '{other}')"),
        // `BETWEEN` resolves each bound against the probed expression
        // exactly as `=` would, unlike `IN`.
        _ => format!("a BETWEEN '{value}' AND '{other}'"),
    }
}

fn inlaysql_ids(rows: &[Row], where_clause: &str) -> Result<Vec<i64>, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
        &[],
    )?;
    for (index, row) in rows.iter().enumerate() {
        db.execute(
            "INSERT INTO t (id, a, b) VALUES (?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                row.a.map(Value::Integer).unwrap_or(Value::Null),
                row.b
                    .map(|text| Value::Text(text.to_string().into()))
                    .unwrap_or(Value::Null),
            ],
        )?;
    }
    let result = db.query(
        &format!("SELECT id FROM t WHERE {where_clause} ORDER BY id"),
        &[],
    )?;
    Ok(result
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("id came back as {other:?}"),
        })
        .collect())
}

fn sqlite_ids(rows: &[Row], where_clause: &str) -> rusqlite::Result<Vec<i64>> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
        [],
    )?;
    for (index, row) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO t (id, a, b) VALUES (?1, ?2, ?3)",
            rusqlite::params![index as i64 + 1, row.a, row.b],
        )?;
    }
    let mut statement = conn.prepare(&format!(
        "SELECT id FROM t WHERE {where_clause} ORDER BY id"
    ))?;
    let ids = statement
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

fn rounds() -> u64 {
    std::env::var("INLAYSQL_DIFFERENTIAL_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ROUNDS)
}

#[test]
fn random_predicates_agree_with_sqlite() {
    let total = rounds();
    let mut unsupported = 0;
    for seed in 0..total {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let clause = predicate(&mut rng, 3);

        let ours = match inlaysql_ids(&rows, &clause) {
            Ok(ids) => ids,
            // A predicate the dialect does not implement is a gap, not a
            // disagreement. Counted so that a generator drifting into
            // unimplemented ground cannot quietly stop testing anything.
            Err(inlaysql::Error::Unsupported(_)) | Err(inlaysql::Error::Parse(_)) => {
                unsupported += 1;
                continue;
            }
            Err(error) => panic!("seed {seed}: InlaySQL failed on `{clause}`: {error}"),
        };
        let theirs = sqlite_ids(&rows, &clause).expect("SQLite is the oracle and must answer");

        assert_eq!(
            ours, theirs,
            "seed {seed}: `SELECT id FROM t WHERE {clause}` disagreed with SQLite\n\
             rows: {rows:?}"
        );
    }

    assert!(
        unsupported * 4 < total,
        "{unsupported} of {total} generated predicates were unsupported: the generator has \
         drifted off the implemented dialect and is no longer testing much"
    );
}

// ---------------------------------------------------------------- joins

/// Canonical form of an InlaySQL value, so both engines' answers compare as
/// strings without either one's type system getting in the way. Reals are
/// pinned to six decimals: an `AVG` accumulates in double on both sides, and
/// tiny last-bit differences must not read as a disagreement.
fn canonical_inlaysql(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => format!("i:{i}"),
        Value::Real(r) => format!("f:{r:.6}"),
        Value::Text(s) => format!("t:{s}"),
        Value::Blob(bytes) => format!("b:{}", hex(bytes)),
        Value::Vector(v) => format!("v:{}", v.len()),
    }
}

/// Canonical form of a SQLite value, matching [`canonical_inlaysql`].
fn canonical_sqlite(value: rusqlite::types::ValueRef<'_>) -> String {
    match value {
        rusqlite::types::ValueRef::Null => "NULL".to_string(),
        rusqlite::types::ValueRef::Integer(i) => format!("i:{i}"),
        rusqlite::types::ValueRef::Real(r) => format!("f:{r:.6}"),
        rusqlite::types::ValueRef::Text(bytes) => format!("t:{}", String::from_utf8_lossy(bytes)),
        rusqlite::types::ValueRef::Blob(bytes) => format!("b:{}", hex(bytes)),
    }
}

/// Exact canonical form, for the scalar-expression oracle.
///
/// [`canonical_inlaysql`] rounds reals so that an `AVG` accumulated in a
/// different order does not read as a disagreement. Scalar expressions do no
/// accumulation, so nothing is allowed to differ: a `CAST` that lands one bit
/// away from SQLite's is a bug, and rounding would hide it. `{:?}` on an `f64`
/// is the shortest string that round-trips, so it is exact on both sides.
fn exact_inlaysql(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => format!("i:{i}"),
        Value::Real(r) => format!("f:{r:?}"),
        Value::Text(s) => format!("t:{s}"),
        Value::Blob(bytes) => format!("b:{}", hex(bytes)),
        Value::Vector(v) => format!("v:{}", v.len()),
    }
}

/// Exact canonical form of a SQLite value, matching [`exact_inlaysql`].
fn exact_sqlite(value: rusqlite::types::ValueRef<'_>) -> String {
    match value {
        rusqlite::types::ValueRef::Null => "NULL".to_string(),
        rusqlite::types::ValueRef::Integer(i) => format!("i:{i}"),
        rusqlite::types::ValueRef::Real(r) => format!("f:{r:?}"),
        rusqlite::types::ValueRef::Text(bytes) => format!("t:{}", String::from_utf8_lossy(bytes)),
        rusqlite::types::ValueRef::Blob(bytes) => format!("b:{}", hex(bytes)),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Two join keys, one per row of each table; `None` is a `NULL`.
fn generate_pairs(rng: &mut SeededRng) -> (Vec<Option<i64>>, Vec<Option<i64>>) {
    let mut xs = Vec::with_capacity(ROWS);
    let mut ys = Vec::with_capacity(ROWS);
    for _ in 0..ROWS {
        xs.push((!rng.next_u64().is_multiple_of(5)).then(|| (rng.next_u64() % VALUE_RANGE) as i64));
        ys.push((!rng.next_u64().is_multiple_of(5)).then(|| (rng.next_u64() % VALUE_RANGE) as i64));
    }
    (xs, ys)
}

/// Which access path the inner side of a generated join gets (AHL-464).
///
/// The `ON` and the data are the same in all three; only how many inner rows
/// InlaySQL *reads* differs, so SQLite's answer is the oracle for every one of
/// them and any disagreement between them is a bug in the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InnerPath {
    /// No index on the join key: the inner side is materialised.
    Scan,
    /// A B-tree index on `b.y`: an entry-range probe per outer row.
    Index,
    /// `ON a.x = b.id`, so the key is the `INTEGER PRIMARY KEY`: one descent.
    RowId,
}

impl InnerPath {
    /// The `ON` this path joins on.
    fn on(self) -> &'static str {
        match self {
            InnerPath::Scan | InnerPath::Index => "a.x = b.y",
            InnerPath::RowId => "a.x = b.id",
        }
    }
}

fn inlaysql_join(
    xs: &[Option<i64>],
    ys: &[Option<i64>],
    left: bool,
    path: InnerPath,
) -> Result<Vec<Vec<String>>, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", &[])?;
    db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, y INTEGER)", &[])?;
    if path == InnerPath::Index {
        db.execute("CREATE INDEX b_y ON b (y)", &[])?;
    }
    for (index, x) in xs.iter().enumerate() {
        db.execute(
            "INSERT INTO a (id, x) VALUES (?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                x.map(Value::Integer).unwrap_or(Value::Null),
            ],
        )?;
    }
    for (index, y) in ys.iter().enumerate() {
        db.execute(
            "INSERT INTO b (id, y) VALUES (?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                y.map(Value::Integer).unwrap_or(Value::Null),
            ],
        )?;
    }
    let kind = if left { "LEFT JOIN" } else { "JOIN" };
    let result = db.query(
        &format!("SELECT a.id, b.id FROM a {kind} b ON {}", path.on()),
        &[],
    )?;
    let mut rows: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(canonical_inlaysql).collect())
        .collect();
    // One `a.id` can match many `b.id`s, and the dialect orders by one key, so
    // compare as a set: the join is defined by which pairs exist, not their
    // order.
    rows.sort();
    Ok(rows)
}

fn sqlite_join(
    xs: &[Option<i64>],
    ys: &[Option<i64>],
    left: bool,
    path: InnerPath,
) -> rusqlite::Result<Vec<Vec<String>>> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", [])?;
    conn.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, y INTEGER)", [])?;
    // Declared on the oracle too, so the two sides run the same schema. It
    // changes SQLite's plan and, as here, must not change its answer.
    if path == InnerPath::Index {
        conn.execute("CREATE INDEX b_y ON b (y)", [])?;
    }
    for (index, x) in xs.iter().enumerate() {
        conn.execute(
            "INSERT INTO a (id, x) VALUES (?1, ?2)",
            rusqlite::params![index as i64 + 1, x],
        )?;
    }
    for (index, y) in ys.iter().enumerate() {
        conn.execute(
            "INSERT INTO b (id, y) VALUES (?1, ?2)",
            rusqlite::params![index as i64 + 1, y],
        )?;
    }
    let kind = if left { "LEFT JOIN" } else { "JOIN" };
    let mut statement = conn.prepare(&format!(
        "SELECT a.id, b.id FROM a {kind} b ON {}",
        path.on()
    ))?;
    let columns = statement.column_count();
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(columns);
        for index in 0..columns {
            values.push(canonical_sqlite(row.get_ref(index)?));
        }
        out.push(values);
    }
    out.sort();
    Ok(out)
}

/// Every generated join, both kinds, against every access path its inner side
/// can be given.
///
/// The `path` is what AHL-464 added: the same rows and the same `ON`, answered
/// once by materialising the inner table, once by an index probe and once by a
/// row-id descent. SQLite is the oracle for all three, so this catches a probe
/// that loses a row, invents one, or reorders them — including the `NULL` key
/// case, which the generator produces in about one row in five and which
/// SQLite's own semantics ("`NULL` matches nothing, including `NULL`") are the
/// specification for.
fn joins_agree(left: bool) {
    let kind = if left { "LEFT JOIN" } else { "INNER JOIN" };
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let (xs, ys) = generate_pairs(&mut rng);
        for path in [InnerPath::Scan, InnerPath::Index, InnerPath::RowId] {
            let ours = inlaysql_join(&xs, &ys, left, path).expect("InlaySQL join must answer");
            let theirs = sqlite_join(&xs, &ys, left, path).expect("SQLite is the oracle");
            assert_eq!(
                ours, theirs,
                "seed {seed}: {kind} on {path:?} disagreed\nx: {xs:?}\ny: {ys:?}"
            );
        }
    }
}

#[test]
fn inner_joins_agree_with_sqlite() {
    joins_agree(false);
}

#[test]
fn left_joins_agree_with_sqlite() {
    joins_agree(true);
}

// ------------------------------------------------------------ aggregates

/// Generated group rows: a non-`NULL` grouping key and a possibly-`NULL` value.
fn generate_groups(rng: &mut SeededRng) -> Vec<(i64, Option<i64>)> {
    (0..ROWS)
        .map(|_| {
            let group = (rng.next_u64() % 4) as i64;
            let value =
                (!rng.next_u64().is_multiple_of(5)).then(|| (rng.next_u64() % VALUE_RANGE) as i64);
            (group, value)
        })
        .collect()
}

const AGGREGATE_QUERY: &str =
    "SELECT g, COUNT(*), SUM(v), MIN(v), MAX(v), AVG(v) FROM t GROUP BY g ORDER BY g";

fn inlaysql_aggregate(groups: &[(i64, Option<i64>)]) -> Result<Vec<Vec<String>>, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, v INTEGER)",
        &[],
    )?;
    for (index, (group, value)) in groups.iter().enumerate() {
        db.execute(
            "INSERT INTO t (id, g, v) VALUES (?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                Value::Integer(*group),
                value.map(Value::Integer).unwrap_or(Value::Null),
            ],
        )?;
    }
    let result = db.query(AGGREGATE_QUERY, &[])?;
    Ok(result
        .rows
        .iter()
        .map(|row| row.iter().map(canonical_inlaysql).collect())
        .collect())
}

fn sqlite_aggregate(groups: &[(i64, Option<i64>)]) -> rusqlite::Result<Vec<Vec<String>>> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, v INTEGER)",
        [],
    )?;
    for (index, (group, value)) in groups.iter().enumerate() {
        conn.execute(
            "INSERT INTO t (id, g, v) VALUES (?1, ?2, ?3)",
            rusqlite::params![index as i64 + 1, group, value],
        )?;
    }
    let mut statement = conn.prepare(AGGREGATE_QUERY)?;
    let columns = statement.column_count();
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(columns);
        for index in 0..columns {
            values.push(canonical_sqlite(row.get_ref(index)?));
        }
        out.push(values);
    }
    Ok(out)
}

#[test]
fn aggregates_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let groups = generate_groups(&mut rng);
        let ours = inlaysql_aggregate(&groups).expect("InlaySQL aggregate must answer");
        let theirs = sqlite_aggregate(&groups).expect("SQLite is the oracle");
        assert_eq!(
            ours, theirs,
            "seed {seed}: aggregates disagreed\ngroups: {groups:?}"
        );
    }
}

// --------------------------------------------------------- window functions
//
// AHL-494. Reuses `generate_groups`'s `(group, value)` rows and `t`'s exact
// schema, over the implemented window surface: the ranking family,
// `lag`/`lead`, `first_value`/`last_value`/`nth_value`, the aggregate family
// `OVER (...)`, an explicit `ROWS` frame, `FILTER` and a named window. Every
// template ends `ORDER BY id` — a window query's *un*ordered row order is
// unspecified by the standard and the two engines do not promise the same
// one (confirmed empirically: sqlite3 emits partition-sorted order when
// nothing else is asked for), so comparing without one would fail on an
// ordering difference that says nothing about whether the window functions
// themselves agree.
const WINDOW_QUERIES: [&str; 22] = [
    "SELECT id, row_number() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, row_number() OVER (ORDER BY v) FROM t ORDER BY id",
    "SELECT id, rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, dense_rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, ntile(3) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, sum(v) OVER (PARTITION BY g) FROM t ORDER BY id",
    "SELECT id, count(v) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, count(*) OVER (PARTITION BY g) FROM t ORDER BY id",
    "SELECT id, avg(v) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, min(v) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, max(v) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
     FROM t ORDER BY id",
    "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND \
     CURRENT ROW) FROM t ORDER BY id",
    "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN 5 PRECEDING AND 5 FOLLOWING) \
     FROM t ORDER BY id",
    "SELECT id, lag(v) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, lag(v, 2, -1) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, lead(v) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, lead(v, 1, 999) OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
    "SELECT id, first_value(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING \
     AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
    "SELECT id, last_value(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING \
     AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
    "SELECT id, nth_value(v, 2) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING \
     AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
];

fn inlaysql_window(
    groups: &[(i64, Option<i64>)],
    query: &str,
) -> Result<Vec<Vec<String>>, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, v INTEGER)",
        &[],
    )?;
    for (index, (group, value)) in groups.iter().enumerate() {
        db.execute(
            "INSERT INTO t (id, g, v) VALUES (?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                Value::Integer(*group),
                value.map(Value::Integer).unwrap_or(Value::Null),
            ],
        )?;
    }
    let result = db.query(query, &[])?;
    Ok(result
        .rows
        .iter()
        .map(|row| row.iter().map(canonical_inlaysql).collect())
        .collect())
}

fn sqlite_window(groups: &[(i64, Option<i64>)], query: &str) -> rusqlite::Result<Vec<Vec<String>>> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, v INTEGER)",
        [],
    )?;
    for (index, (group, value)) in groups.iter().enumerate() {
        conn.execute(
            "INSERT INTO t (id, g, v) VALUES (?1, ?2, ?3)",
            rusqlite::params![index as i64 + 1, group, value],
        )?;
    }
    let mut statement = conn.prepare(query)?;
    let columns = statement.column_count();
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(columns);
        for index in 0..columns {
            values.push(canonical_sqlite(row.get_ref(index)?));
        }
        out.push(values);
    }
    Ok(out)
}

#[test]
fn window_functions_agree_with_sqlite() {
    let total = rounds();
    for seed in 0..total {
        let mut rng = SeededRng::new(seed);
        let groups = generate_groups(&mut rng);
        let query = WINDOW_QUERIES[(rng.next_u64() % WINDOW_QUERIES.len() as u64) as usize];

        let ours = inlaysql_window(&groups, query)
            .unwrap_or_else(|error| panic!("seed {seed}: InlaySQL failed on `{query}`: {error}"));
        let theirs = sqlite_window(&groups, query).expect("SQLite is the oracle and must answer");

        assert_eq!(
            ours, theirs,
            "seed {seed}: `{query}` disagreed with SQLite\ngroups: {groups:?}"
        );
    }
}

// -------------------------------------------------- scalar expression values
//
// The predicate generator above compares *which rows survive a `WHERE`*, and
// that hides half of three-valued logic: `NULL` and `0` filter identically, so
// a `LIKE` that answers `0` where SQLite answers `NULL` passes unnoticed until
// someone writes `NOT`. This generator projects the expression instead and
// compares the value, which makes `NULL` visible — and it is where `CASE`,
// `CAST` and `||` can be checked at all, since none of them is a predicate.

/// Text worth running through `LIKE`, `CAST` and `||`: mixed case (`LIKE`
/// folds ASCII only), numeric-looking text with and without a valid tail
/// (`CAST` reads a prefix), an empty string, and leading whitespace.
const TEXTS: [&str; 10] = [
    "alpha", "ALPHA", "Beta", "gamma", "", "10", "10.5", "3x", " 7 ", "x",
];

/// Reals worth rendering as text: whole values (SQLite writes `2.0`, not `2`),
/// repeating fractions that fill the fifteen significant digits, and
/// magnitudes on both sides of the switch to exponential notation.
const REALS: [f64; 10] = [
    0.0,
    2.0,
    -2.5,
    0.1,
    1.0 / 3.0,
    1e15,
    1e-5,
    1.0e300,
    -0.0001,
    123456789012345.0,
];

/// Text worth handing to the date/time family: every spelling it parses, a
/// leap day, the epoch, and two strings that are not dates at all — a
/// date function answers `NULL` for those rather than failing, and that is
/// exactly the behaviour worth pinning.
const DATES: [&str; 10] = [
    "2024-02-29",
    "2024-02-29 13:45:56",
    "2024-02-29T13:45:56.789",
    "1970-01-01",
    "1999-12-31 23:59:59",
    "13:45:56",
    "2000-01-01 00:00:00Z",
    "2024-06-30 12:00:00+02:30",
    "not a date",
    "",
];

/// JSON documents worth handing to the json1 family (AHL-490): an object
/// with a mix of member types (including a nested object and a JSON
/// `null`), an array, a bare scalar, and an empty object — every one of
/// these is valid JSON on purpose. This generator's contract is that both
/// engines *succeed*, so a malformed document has no place here; the
/// malformed-JSON and bad-path refusals are pinned instead in
/// `crates/inlaysql/tests/sqllogictest/json.test`, where an expected error is
/// a first-class outcome rather than a panic.
const JSON_DOCS: [&str; 6] = [
    r#"{"color":"red","size":10,"tags":["a","b"],"meta":{"active":true},"note":null}"#,
    r#"{"color":"blue","size":20,"tags":[],"meta":{"active":false}}"#,
    r#"[1,2,3]"#,
    r#"[]"#,
    r#"5"#,
    r#"{}"#,
];

/// One row of the scalar-expression table.
#[derive(Debug, Clone)]
struct ScalarRow {
    a: Option<i64>,
    b: Option<&'static str>,
    r: Option<f64>,
    d: Option<&'static str>,
    j: Option<&'static str>,
}

fn generate_scalar_rows(rng: &mut SeededRng) -> Vec<ScalarRow> {
    (0..ROWS)
        .map(|_| ScalarRow {
            a: (!rng.next_u64().is_multiple_of(5)).then(|| (rng.next_u64() % VALUE_RANGE) as i64),
            b: (!rng.next_u64().is_multiple_of(5))
                .then(|| TEXTS[(rng.next_u64() as usize) % TEXTS.len()]),
            r: (!rng.next_u64().is_multiple_of(5))
                .then(|| REALS[(rng.next_u64() as usize) % REALS.len()]),
            d: (!rng.next_u64().is_multiple_of(5))
                .then(|| DATES[(rng.next_u64() as usize) % DATES.len()]),
            j: (!rng.next_u64().is_multiple_of(5))
                .then(|| JSON_DOCS[(rng.next_u64() as usize) % JSON_DOCS.len()]),
        })
        .collect()
}

/// One scalar expression over `(a, b, r)`, in SQL both engines parse the same
/// way.
///
/// Comparisons stay type-consistent for the same reason the predicate
/// generator's do — see this file's header — so `IN` lists and `CASE` operands
/// match the column they are tested against.
fn scalar_expr(rng: &mut SeededRng) -> String {
    let value = rng.next_u64() % VALUE_RANGE;
    let other = rng.next_u64() % VALUE_RANGE;
    let word = TEXTS[(rng.next_u64() as usize) % TEXTS.len()];
    let second = TEXTS[(rng.next_u64() as usize) % TEXTS.len()];
    let pattern = LIKE_PATTERNS[(rng.next_u64() as usize) % LIKE_PATTERNS.len()];
    let escaped = ESCAPE_PATTERNS[(rng.next_u64() as usize) % ESCAPE_PATTERNS.len()];

    // Half operators, half function calls: the operator grammar was already
    // here and still finds things, and splitting the draw keeps each of the
    // two readable rather than growing one match to sixty arms.
    if rng.next_u64().is_multiple_of(2) {
        return scalar_function_expr(rng);
    }

    match rng.next_u64() % 36 {
        // LIKE, including on a non-text operand (SQLite renders it as text
        // first) and with an ESCAPE clause.
        0 => format!("b LIKE '{pattern}'"),
        1 => format!("b NOT LIKE '{pattern}'"),
        2 => format!("a LIKE '{value}%'"),
        3 => format!("b LIKE '{escaped}' ESCAPE '!'"),
        4 => format!("b NOT LIKE '{escaped}' ESCAPE '!'"),
        5 => format!("'{word}' LIKE b"),

        // IN: the NULL cases are the whole point.
        6 => format!("a IN ({value}, {other})"),
        7 => format!("a IN ({value}, NULL)"),
        8 => format!("a NOT IN ({value}, NULL)"),
        9 => format!("b IN ('{word}', '{second}')"),
        10 => format!("b IN ('{word}', NULL)"),
        11 => format!("b NOT IN ('{word}', '{second}')"),

        // BETWEEN, in both directions and with the bounds crossed.
        12 => format!("a BETWEEN {value} AND {other}"),
        13 => format!("a NOT BETWEEN {value} AND {other}"),
        14 => format!("b BETWEEN '{word}' AND '{second}'"),
        15 => format!("a BETWEEN NULL AND {other}"),

        // CASE, searched and simple, with and without ELSE.
        16 => {
            format!("CASE WHEN a > {value} THEN 'big' WHEN a IS NULL THEN 'none' ELSE 'small' END")
        }
        17 => format!("CASE WHEN a > {value} THEN 'big' END"),
        18 => format!("CASE a WHEN {value} THEN 'hit' WHEN {other} THEN 'other' ELSE 'miss' END"),
        19 => "CASE a WHEN NULL THEN 'never' ELSE 'else' END".to_string(),
        20 => format!("CASE b WHEN '{word}' THEN 1 END"),
        // A bare column as a searched condition: SQLite reads text as a
        // number here, so 'alpha' is false and '10' is true.
        21 => "CASE WHEN b THEN 'yes' ELSE 'no' END".to_string(),

        // CAST across every affinity.
        22 => "CAST(a AS TEXT)".to_string(),
        23 => "CAST(b AS INTEGER)".to_string(),
        24 => "CAST(b AS REAL)".to_string(),
        25 => "CAST(b AS NUMERIC)".to_string(),
        // `CAST(r AS TEXT)` — a REAL rendered as text — is deliberately NOT
        // generated, and this is a known divergence rather than an oversight.
        //
        // SQLite does not render a double with a correctly-rounded printf. It
        // decodes the float itself (`sqlite3FpDecode`) so that its output does
        // not depend on the platform's libc, and that decoder disagrees with a
        // correctly-rounded conversion in the last digit: for `1.0/3.0` it
        // emits `0.33333333333333332` where the correctly-rounded seventeen
        // significant digits are `0.33333333333333331`, and Rust's shortest
        // round-tripping form is `0.3333333333333333`. Matching SQLite here
        // means porting its decoder, not choosing a precision.
        //
        // Until that is done, InlaySQL renders REAL as text to fifteen
        // significant digits, which agrees with SQLite for values that need no
        // more and disagrees for values that do. Generating this case would
        // fail the suite for a formatting difference while hiding nothing —
        // so it is named here instead, and in TESTING.md, rather than being
        // silently dropped from the grammar.
        26 => "CAST(b AS TEXT)".to_string(),
        27 => "CAST(r AS INTEGER)".to_string(),
        28 => "CAST(a AS BLOB)".to_string(),

        // Concatenation and blob literals.
        29 => "b || '-' || CAST(a AS TEXT)".to_string(),
        30 => format!("'{word}' || X'2d' || b"),

        // Integer overflow. This case used to be excluded: `eval` wrapped
        // where SQLite promotes the result to REAL, so
        // `CAST(1e300 AS INTEGER) + 1` was `-9223372036854775808` here and
        // `9.223372036854776e18` there. AHL-412 fixed the promotion, and the
        // case is generated again — which is the only way the fix stays
        // fixed. The `r` column carries `1e300`, so these really do overflow.
        31 => "CAST(r AS INTEGER) + 1".to_string(),
        32 => "CAST(r AS INTEGER) * 2".to_string(),
        33 => "-CAST(r AS INTEGER) - 1".to_string(),
        34 => format!("CAST(r AS INTEGER) - {value}"),
        _ => format!("CAST(r AS INTEGER) / (0 - {})", value + 1),
    }
}

/// Date modifiers worth applying, including two SQLite cannot make sense of —
/// it answers `NULL` for those rather than failing, and so must this engine.
const MODIFIERS: [&str; 10] = [
    "+1 day",
    "-2 hours",
    "+1 month",
    "-1 year",
    "start of day",
    "start of month",
    "weekday 3",
    "+01:30",
    "not a modifier",
    "start of week",
];

/// One scalar *function* call over `(a, b, r, d)`.
///
/// `r` — the REAL column — is deliberately kept out of every function that
/// renders its argument as text (`length`, `hex`, `upper`, `substr`, `instr`,
/// `||`). That is not squeamishness about floats: it is the first of the two
/// divergences this file already documents, where SQLite decodes a double with
/// its own routine rather than a correctly-rounded `printf`. Feeding `r` to
/// `hex()` would fail the suite for that same formatting difference while
/// telling us nothing new about `hex`. `abs`, `round`, `min`, `max` and
/// `coalesce` do take `r`, because they return a number rather than text.
fn scalar_function_expr(rng: &mut SeededRng) -> String {
    let value = rng.next_u64() % VALUE_RANGE;
    let other = rng.next_u64() % VALUE_RANGE;
    let word = TEXTS[(rng.next_u64() as usize) % TEXTS.len()];
    let second = TEXTS[(rng.next_u64() as usize) % TEXTS.len()];
    let date = DATES[(rng.next_u64() as usize) % DATES.len()];
    // Indices that reach past both ends, so the clamping is exercised.
    let index = (rng.next_u64() % 9) as i64 - 3;
    let length = (rng.next_u64() % 9) as i64 - 3;
    let digits = rng.next_u64() % 4;

    match rng.next_u64() % 47 {
        // Strings.
        0 => "length(b)".to_string(),
        1 => "length(a)".to_string(),
        2 => "upper(b)".to_string(),
        3 => "lower(b)".to_string(),
        4 => format!("substr(b, {index})"),
        5 => format!("substr(b, {index}, {length})"),
        6 => format!("substr('{word}', {index}, {length})"),
        7 => "trim(b)".to_string(),
        8 => format!("trim(b, '{word}')"),
        9 => "ltrim(b)".to_string(),
        10 => "rtrim(b)".to_string(),
        11 => format!("replace(b, '{word}', '{second}')"),
        12 => format!("instr(b, '{word}')"),
        13 => "instr(b, b)".to_string(),
        14 => "hex(b)".to_string(),
        15 => "hex(a)".to_string(),

        // Numbers.
        16 => "abs(a)".to_string(),
        17 => "abs(r)".to_string(),
        18 => "abs(b)".to_string(),
        19 => "round(r)".to_string(),
        20 => format!("round(r, {digits})"),
        21 => format!("round(a, {digits})"),

        // NULL handling — the whole point of the NULL-heavy rows.
        22 => format!("coalesce(a, {value})"),
        23 => "coalesce(a, r, 0)".to_string(),
        24 => format!("ifnull(b, '{word}')"),
        25 => format!("nullif(a, {value})"),
        26 => format!("nullif(b, '{word}')"),
        27 => format!("min(a, {value}, {other})"),
        28 => format!("max(a, {value})"),
        29 => "min(b, 'delta')".to_string(),

        // Dates. `d` is text that is sometimes not a date at all, which is
        // where these functions answer NULL rather than failing.
        30 => "date(d)".to_string(),
        31 => "datetime(d, '+1 day')".to_string(),
        32 => "strftime('%Y/%m/%d %H:%M:%S', d)".to_string(),
        33 => format!(
            "unixepoch(d, '{}')",
            MODIFIERS[(rng.next_u64() as usize) % MODIFIERS.len()]
        ),
        // A literal date beside the column, so a modifier chain is exercised
        // even on the rows where `d` is NULL.
        34 => format!(
            "datetime('{date}', '{}')",
            MODIFIERS[(rng.next_u64() as usize) % MODIFIERS.len()]
        ),

        // JSON (AHL-490). `j` is always one of `JSON_DOCS` (or NULL), so
        // every one of these succeeds in both engines whichever row it
        // lands on — a scalar or array document under a dot-path, or an
        // object under a bracket-path, answers NULL/0 rather than erroring,
        // which is exactly the corner worth rolling repeatedly here rather
        // than only in the fixed list.
        35 => "json_valid(j)".to_string(),
        36 => "json_type(j)".to_string(),
        37 => "json_extract(j, '$.color')".to_string(),
        38 => "json_extract(j, '$.tags[0]')".to_string(),
        39 => "json_array_length(j)".to_string(),
        40 => "json_array_length(j, '$.tags')".to_string(),
        41 => "j -> '$.color'".to_string(),
        42 => "j ->> '$.color'".to_string(),
        43 => "json_set(j, '$.color', 'green')".to_string(),
        44 => "json_insert(j, '$.brand', 'x')".to_string(),
        45 => "json_replace(j, '$.color', 'green')".to_string(),
        46 => "json_remove(j, '$.color')".to_string(),
        // Composition over the plain columns: none of `a`/`b`/`r` is ever a
        // BLOB in this schema, so neither of these can hit "JSON cannot hold
        // BLOB values" — that corner is covered in `FIXED_EXPRESSIONS`
        // instead, where a BLOB-carrying expression is written directly.
        _ => "json_object('a', a, 'b', b, 'r', r)".to_string(),
    }
}

/// `LIKE` patterns for the `ESCAPE '!'` cases: an escaped wildcard, an escaped
/// escape, an escape in front of an ordinary character (which SQLite makes
/// literal), and a dangling escape at the end (which matches nothing at all
/// rather than erroring).
const ESCAPE_PATTERNS: [&str; 8] = ["a!%", "!%", "!_", "al!pha", "%!%%", "10!.5", "alpha!", "!!"];

fn inlaysql_scalar(rows: &[ScalarRow], expr: &str) -> Result<Vec<Vec<String>>, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, r REAL, d TEXT, j TEXT)",
        &[],
    )?;
    for (index, row) in rows.iter().enumerate() {
        db.execute(
            "INSERT INTO t (id, a, b, r, d, j) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                row.a.map(Value::Integer).unwrap_or(Value::Null),
                row.b
                    .map(|text| Value::Text(text.to_string().into()))
                    .unwrap_or(Value::Null),
                row.r.map(Value::Real).unwrap_or(Value::Null),
                row.d
                    .map(|text| Value::Text(text.to_string().into()))
                    .unwrap_or(Value::Null),
                row.j
                    .map(|text| Value::Text(text.to_string().into()))
                    .unwrap_or(Value::Null),
            ],
        )?;
    }
    let result = db.query(&format!("SELECT id, {expr} FROM t ORDER BY id"), &[])?;
    Ok(result
        .rows
        .iter()
        .map(|row| row.iter().map(exact_inlaysql).collect())
        .collect())
}

fn sqlite_scalar(rows: &[ScalarRow], expr: &str) -> rusqlite::Result<Vec<Vec<String>>> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, r REAL, d TEXT, j TEXT)",
        [],
    )?;
    for (index, row) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO t (id, a, b, r, d, j) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![index as i64 + 1, row.a, row.b, row.r, row.d, row.j],
        )?;
    }
    let mut statement = conn.prepare(&format!("SELECT id, {expr} FROM t ORDER BY id"))?;
    let columns = statement.column_count();
    let mut result = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = result.next()? {
        let mut values = Vec::with_capacity(columns);
        for index in 0..columns {
            values.push(exact_sqlite(row.get_ref(index)?));
        }
        out.push(values);
    }
    Ok(out)
}

#[test]
fn scalar_expressions_agree_with_sqlite() {
    let total = rounds();
    for seed in 0..total {
        let mut rng = SeededRng::new(seed);
        let rows = generate_scalar_rows(&mut rng);
        let expr = scalar_expr(&mut rng);

        // Unlike the predicate generator, this one only emits constructs the
        // dialect implements, so an error is a bug rather than a gap.
        let ours = inlaysql_scalar(&rows, &expr)
            .unwrap_or_else(|error| panic!("seed {seed}: InlaySQL failed on `{expr}`: {error}"));
        let theirs = sqlite_scalar(&rows, &expr).expect("SQLite is the oracle and must answer");

        assert_eq!(
            ours, theirs,
            "seed {seed}: `SELECT id, {expr} FROM t` disagreed with SQLite\nrows: {rows:?}"
        );
    }
}

// ------------------------------------------------ fixed scalar-function cases
//
// The random generator below covers these constructs over random rows, which
// is where the surprises are. This list covers the *edges* a random walk is
// unlikely to hit twice: the arity boundaries, the empty string, the negative
// index, and the handful of SQLite behaviours that read as accidents until you
// check them against the source — `hex(NULL)` being `''` rather than `NULL`,
// `instr` with an empty needle being `1`, `substr` with a negative length
// reading backwards.
//
// It runs in every `cargo test`, unlike the long random campaign, so a
// regression in one of them is caught on the fast loop.
const FIXED_EXPRESSIONS: &[&str] = &[
    // length
    "length('')",
    "length('abc')",
    "length(NULL)",
    "length(12345)",
    "length(-2.5)",
    "length(X'00ff10')",
    // upper/lower are ASCII-only in stock SQLite
    "upper('abcXYZ')",
    "lower('ABCxyz')",
    "upper(NULL)",
    "upper(123)",
    // substr, including the negative forms
    "substr('abcdef', 2)",
    "substr('abcdef', 2, 3)",
    "substr('abcdef', -2)",
    "substr('abcdef', -2, 1)",
    "substr('abcdef', 0)",
    "substr('abcdef', 0, 3)",
    "substr('abcdef', 2, -1)",
    "substr('abcdef', 4, -2)",
    "substr('abcdef', 10)",
    "substr('abcdef', 1, 100)",
    "substr(NULL, 1)",
    "substr('abcdef', 1, 0)",
    "substring('abcdef', 2, 2)",
    "substr(X'00ff10', 2)",
    "substr(X'00ff10', 2, 1)",
    // trim family
    "trim('  ab  ')",
    "ltrim('  ab  ')",
    "rtrim('  ab  ')",
    "trim('xxabxx', 'x')",
    "ltrim('xyxyab', 'xy')",
    "rtrim('abxyxy', 'xy')",
    "trim('abc', '')",
    "trim(NULL)",
    "trim('abc', NULL)",
    "trim('aaa', 'a')",
    // replace
    "replace('abcabc', 'b', 'Z')",
    "replace('abc', '', 'Z')",
    "replace('abc', 'x', 'Z')",
    "replace(NULL, 'a', 'b')",
    "replace('abc', 'a', NULL)",
    "replace('aaa', 'aa', 'b')",
    // instr
    "instr('abcabc', 'c')",
    "instr('abc', 'x')",
    "instr('abc', '')",
    "instr(NULL, 'a')",
    "instr('abc', NULL)",
    "instr(X'0011ff', X'11')",
    "instr(12345, '34')",
    // abs / round
    "abs(-5)",
    "abs(5)",
    "abs(-2.5)",
    "abs(NULL)",
    "abs('abc')",
    "abs('-3.5x')",
    "round(2.5)",
    "round(-2.5)",
    "round(2.4)",
    "round(2.345, 2)",
    "round(2.345, 0)",
    "round(-2.345, 1)",
    "round(NULL)",
    "round(2.5, NULL)",
    "round(3, 2)",
    "round('2.7')",
    // coalesce / ifnull / nullif
    "coalesce(NULL, NULL, 3)",
    "coalesce(NULL, 'a', 'b')",
    "coalesce(NULL, NULL)",
    "ifnull(NULL, 7)",
    "ifnull(1, 7)",
    "nullif(1, 1)",
    "nullif(1, 2)",
    "nullif(1, '1')",
    "nullif(1, 1.0)",
    "nullif(NULL, 1)",
    "nullif('a', 'a')",
    // scalar min/max, which are not the aggregates
    "min(3, 1, 2)",
    "max(3, 1, 2)",
    "min(1, NULL)",
    "max(1, NULL)",
    "min('b', 'a')",
    "max(1, 'a')",
    "min(1, 1.0)",
    // hex
    "hex('abc')",
    "hex(NULL)",
    "hex(X'00ff')",
    "hex(255)",
    "hex('')",
    // JSON (AHL-490) — every expression here is one both engines must
    // *succeed* on (this list's oracle panics if sqlite3 itself errors), so
    // the malformed-JSON/bad-path refusals live in
    // `crates/inlaysql/tests/sqllogictest/json.test` instead, where an
    // expected error is a first-class outcome.
    "json_extract('{\"a\":1,\"b\":{\"c\":2}}', '$.a')",
    "json_extract('{\"a\":1,\"b\":{\"c\":2}}', '$.b')",
    "json_extract('{\"a\":1}', '$.missing')",
    "json_extract(NULL, '$.a')",
    "json_extract('[1,2,3]', '$[1]')",
    "json_extract('{\"a\":1,\"b\":2}', '$.a', '$.b')",
    "json_extract('{\"a\":1}', NULL)",
    "'{\"a\":1}' -> '$.a'",
    "'{\"a\":1}' ->> '$.a'",
    "'{\"a\":\"str\"}' -> '$.a'",
    "'{\"a\":\"str\"}' ->> '$.a'",
    "'{\"a\":null}' -> '$.a'",
    "'{\"a\":null}' ->> '$.a'",
    "'{\"a\":true}' ->> '$.a'",
    "NULL -> '$.a'",
    "json_valid('{\"a\":1}')",
    "json_valid('not json')",
    "json_valid('')",
    "json_valid(NULL)",
    "json_type('{\"a\":1}')",
    "json_type('[1,2]')",
    "json_type('1')",
    "json_type('1.5')",
    "json_type('\"abc\"')",
    "json_type('true')",
    "json_type('null')",
    "json_type(NULL)",
    "json_type('{\"a\":1}', '$.a')",
    "json_type('{\"a\":1}', '$.z')",
    "json_quote('abc')",
    "json_quote(1)",
    "json_quote(1.5)",
    "json_quote(NULL)",
    "json_array(1, 2, 'three', NULL)",
    "json_array()",
    "json_array(json_object('x', 1))",
    "json_object('a', 1, 'b', 'two', 'c', NULL)",
    "json_object()",
    "json_array_length('[1,2,3]')",
    "json_array_length('{\"a\":1}')",
    "json_array_length('5')",
    "json_array_length(NULL)",
    "json_array_length('{\"a\":[1,2,3]}', '$.a')",
    "json_set('{\"a\":1,\"b\":2}', '$.a', 99)",
    "json_set('{\"a\":1}', '$.b', 99)",
    "json_set('{\"a\":1}', '$.a', NULL)",
    "json_set('[1,2,3]', '$[1]', 99)",
    "json_set('[1,2,3]', '$[#]', 99)",
    "json_set('[1,2,3]', '$[9]', 99)",
    "json_set('{}', '$.a.b.c', 1)",
    "json_set(NULL, '$.a', 1)",
    "json_insert('{\"a\":1}', '$.a', 99)",
    "json_insert('{\"a\":1}', '$.b', 99)",
    "json_insert('[1,2,3]', '$[#]', 99)",
    "json_replace('{\"a\":1}', '$.a', 99)",
    "json_replace('{\"a\":1}', '$.b', 99)",
    "json_remove('{\"a\":1,\"b\":2}', '$.a')",
    "json_remove('{\"a\":1}', '$.b')",
    "json_remove('[1,2,3,4]', '$[1]')",
    "json_remove('{\"a\":1,\"b\":2,\"c\":3}', '$.a', '$.c')",
    "json_remove(NULL, '$.a')",
    "json('{\"a\": 1  , \"b\":2}')",
    "json_set('{\"a\":1}', '$.a', json_object('x', 1))",
    "json_set('{\"z\":1}', '$.z', json_extract('{\"a\":\"str\"}', '$.a'))",
];

/// Scalar expressions with no `FROM`, compared exactly.
fn inlaysql_value(expr: &str) -> Result<String, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    let result = db.query(&format!("SELECT {expr}"), &[])?;
    Ok(exact_inlaysql(&result.rows[0][0]))
}

fn sqlite_value(expr: &str) -> rusqlite::Result<String> {
    let conn = rusqlite::Connection::open_in_memory()?;
    let mut statement = conn.prepare(&format!("SELECT {expr}"))?;
    let mut rows = statement.query([])?;
    let row = rows.next()?.expect("a scalar SELECT returns one row");
    Ok(exact_sqlite(row.get_ref(0)?))
}

#[test]
fn fixed_scalar_functions_agree_with_sqlite() {
    let mut mismatches = Vec::new();
    for expr in FIXED_EXPRESSIONS {
        let theirs = match sqlite_value(expr) {
            Ok(value) => value,
            Err(error) => panic!("SQLite is the oracle and must answer `{expr}`: {error}"),
        };
        match inlaysql_value(expr) {
            Ok(ours) if ours == theirs => {}
            Ok(ours) => mismatches.push(format!("{expr}: ours {ours}, SQLite {theirs}")),
            Err(error) => {
                mismatches.push(format!("{expr}: ours errored ({error}), SQLite {theirs}"))
            }
        }
    }
    // Report every disagreement at once: fixing them one panic at a time hides
    // how many there are.
    assert!(
        mismatches.is_empty(),
        "{} scalar function(s) disagreed with SQLite:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

// ------------------------------------------------------- fixed date/time cases
//
// `'now'` is deliberately absent: the two engines read different clocks, so a
// comparison would be a race rather than a test. What is compared is
// everything that follows from a *given* moment — parsing, the modifiers, and
// the formatting — which is where the algorithm can be wrong.
//
// The clock itself is covered in `inlaysql-core`'s own tests, where a fixed
// reading is injected through the `Clock` trait and the answer is asserted
// exactly.
const FIXED_DATETIME_EXPRESSIONS: &[&str] = &[
    // Parsing the accepted spellings.
    "date('2024-02-29')",
    "date('2024-02-29 13:45:56')",
    "date('2024-02-29T13:45:56')",
    "datetime('2024-02-29 13:45:56')",
    "datetime('2024-02-29T13:45:56.789')",
    "datetime('2024-02-29')",
    "time('2024-02-29 13:45:56')",
    "time('13:45')",
    "time('13:45:56')",
    "datetime('13:45:56')",
    "datetime('2024-02-29 13:45:56Z')",
    "datetime('2024-02-29 13:45:56+02:30')",
    "datetime('2024-02-29 13:45:56-05:00')",
    // A julian day number, and the unixepoch modifier.
    "datetime(2460370.5)",
    "date(2460370.5)",
    "datetime(0, 'unixepoch')",
    "datetime(1709214356, 'unixepoch')",
    "date(1709214356, 'unixepoch')",
    "unixepoch('2024-02-29 13:45:56')",
    "unixepoch('1970-01-01')",
    "unixepoch('1969-12-31 23:59:59')",
    // Invalid input is NULL, not an error.
    "date('not a date')",
    "date('2024-13-01')",
    "date('2024-02-30')",
    "date(NULL)",
    "datetime('')",
    "date('9999-12-31')",
    "date('10000-01-01')",
    // Modifiers.
    "date('2024-02-29', '+1 day')",
    "date('2024-02-29', '-1 day')",
    "date('2024-01-31', '+1 month')",
    "date('2024-01-31', '+1 month', 'floor')",
    "date('2024-02-29', '+1 year')",
    "date('2024-02-29', '+1 year', 'floor')",
    "datetime('2024-02-29 13:45:56', '+90 minutes')",
    "datetime('2024-02-29 13:45:56', '-3 hours')",
    "datetime('2024-02-29 13:45:56', '+1.5 seconds')",
    "date('2024-02-29', 'start of month')",
    "date('2024-02-29', 'start of year')",
    "datetime('2024-02-29 13:45:56', 'start of day')",
    "date('2024-02-29', 'weekday 0')",
    "date('2024-02-29', 'weekday 1')",
    "date('2024-02-29', 'weekday 4')",
    "datetime('2024-02-29', '+0001-02-03')",
    "datetime('2024-02-29', '-0001-02-03')",
    "datetime('2024-02-29 13:45:56', '+02:30')",
    "datetime('2024-02-29 13:45:56', '-02:30:15')",
    "date('2024-02-29', '+1 day', '+1 month', 'start of year')",
    // A modifier SQLite cannot make sense of is NULL, not an error.
    "date('2024-02-29', '+1 fortnight')",
    "date('2024-02-29', 'start of week')",
    "date('2024-02-29', NULL)",
    "date('2024-02-29', 'weekday 9')",
    // strftime, every specifier the bundled SQLite implements.
    "strftime('%Y-%m-%d', '2024-02-29')",
    "strftime('%d/%e/%F', '2024-02-29 13:45:56')",
    "strftime('%H:%M:%S', '2024-02-29 13:45:56')",
    "strftime('%f', '2024-02-29 13:45:56.789')",
    "strftime('%G %g', '2024-02-29')",
    "strftime('%I %l %p %P', '2024-02-29 13:45:56')",
    "strftime('%I %l %p %P', '2024-02-29 00:45:56')",
    "strftime('%j', '2024-02-29')",
    "strftime('%J', '2024-02-29')",
    "strftime('%k %R %T', '2024-02-29 03:45:56')",
    "strftime('%s', '2024-02-29 13:45:56')",
    "strftime('%u %w', '2024-02-29')",
    "strftime('%U %V %W', '2024-02-29')",
    "strftime('%U %V %W', '2021-01-01')",
    "strftime('%U %V %W', '2021-01-03')",
    "strftime('%%', '2024-02-29')",
    "strftime('literal', '2024-02-29')",
    "strftime('%Y', 'not a date')",
    // An unknown specifier makes the whole call NULL.
    "strftime('%Q', '2024-02-29')",
    "strftime('%', '2024-02-29')",
    "strftime(NULL, '2024-02-29')",
    // subsec / subsecond.
    "datetime('2024-02-29 13:45:56.789', 'subsec')",
    "time('2024-02-29 13:45:56.789', 'subsec')",
    "strftime('%s', '2024-02-29 13:45:56.789', 'subsec')",
    // Boundaries of the julian-day range SQLite calls a date.
    "datetime(0)",
    "datetime(5373484.4)",
    "datetime(5373485)",
    "datetime(-1)",
];

#[test]
fn fixed_date_and_time_functions_agree_with_sqlite() {
    let mut mismatches = Vec::new();
    for expr in FIXED_DATETIME_EXPRESSIONS {
        let theirs = match sqlite_value(expr) {
            Ok(value) => value,
            Err(error) => panic!("SQLite is the oracle and must answer `{expr}`: {error}"),
        };
        match inlaysql_value(expr) {
            Ok(ours) if ours == theirs => {}
            Ok(ours) => mismatches.push(format!("{expr}: ours {ours}, SQLite {theirs}")),
            Err(error) => {
                mismatches.push(format!("{expr}: ours errored ({error}), SQLite {theirs}"))
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} date/time expression(s) disagreed with SQLite:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

// --------------------------------------------------------------- query shape
//
// The generators above vary the *expression* and hold the query shape fixed.
// This one does the opposite: one small table, and a random `DISTINCT` /
// `ORDER BY` / `LIMIT` / `OFFSET` around it. The bugs it is looking for are the
// ones an expression oracle cannot see — a second sort key that never breaks a
// tie, `NULLS LAST` reversed by `DESC`, an `OFFSET` applied after `LIMIT`
// instead of before.
//
// Every generated query is given a total order, ending in the primary key.
// Without that, two engines could return different rows for the same
// `LIMIT 3` and both be right, and the test would be measuring nothing but
// tie-breaking luck.

/// One row of the query-shape table. `g` is a low-cardinality grouping key so
/// that `GROUP BY` makes several groups and `DISTINCT` has duplicates to fold.
#[derive(Debug, Clone)]
struct ShapeRow {
    g: Option<i64>,
    a: Option<i64>,
    b: Option<&'static str>,
}

fn generate_shape_rows(rng: &mut SeededRng) -> Vec<ShapeRow> {
    (0..ROWS)
        .map(|_| ShapeRow {
            g: (!rng.next_u64().is_multiple_of(5)).then(|| (rng.next_u64() % 3) as i64),
            a: (!rng.next_u64().is_multiple_of(5)).then(|| (rng.next_u64() % 4) as i64),
            b: (!rng.next_u64().is_multiple_of(5)).then(|| WORDS[(rng.next_u64() % 4) as usize]),
        })
        .collect()
}

const SHAPE_DDL: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, a INTEGER, b TEXT)";

fn shape_db(rows: &[ShapeRow]) -> Result<Database, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute(SHAPE_DDL, &[])?;
    for (index, row) in rows.iter().enumerate() {
        db.execute(
            "INSERT INTO t (id, g, a, b) VALUES (?, ?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                row.g.map(Value::Integer).unwrap_or(Value::Null),
                row.a.map(Value::Integer).unwrap_or(Value::Null),
                row.b
                    .map(|text| Value::Text(text.to_string().into()))
                    .unwrap_or(Value::Null),
            ],
        )?;
    }
    Ok(db)
}

fn shape_sqlite(rows: &[ShapeRow]) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute(SHAPE_DDL, [])?;
    for (index, row) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO t (id, g, a, b) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![index as i64 + 1, row.g, row.a, row.b],
        )?;
    }
    Ok(conn)
}

fn inlaysql_rows(
    db: &mut Database,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Vec<String>>, inlaysql::Error> {
    let result = db.query(sql, params)?;
    Ok(result
        .rows
        .iter()
        .map(|row| row.iter().map(exact_inlaysql).collect())
        .collect())
}

fn sqlite_rows(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[i64],
) -> rusqlite::Result<Vec<Vec<String>>> {
    let mut statement = conn.prepare(sql)?;
    let columns = statement.column_count();
    let bound = rusqlite::params_from_iter(params.iter());
    let mut rows = statement.query(bound)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(columns);
        for index in 0..columns {
            values.push(exact_sqlite(row.get_ref(index)?));
        }
        out.push(values);
    }
    Ok(out)
}

/// One `ORDER BY` term over the shape table, with a direction and an explicit
/// `NULLS` placement often enough that the non-default combinations come up.
fn order_term(rng: &mut SeededRng) -> String {
    let column = ["g", "a", "b"][(rng.next_u64() % 3) as usize];
    let direction = match rng.next_u64() % 3 {
        0 => "",
        1 => " ASC",
        _ => " DESC",
    };
    let nulls = match rng.next_u64() % 4 {
        0 => " NULLS FIRST",
        1 => " NULLS LAST",
        _ => "",
    };
    format!("{column}{direction}{nulls}")
}

/// A `SELECT` with a random `DISTINCT`, `ORDER BY`, `LIMIT` and `OFFSET`.
///
/// Returns the SQL and the parameters it binds, because `LIMIT ?` — a bound
/// row count, which used to be rejected outright — is one of the things under
/// test and cannot be generated as literal text.
fn shape_query(rng: &mut SeededRng) -> (String, Vec<i64>) {
    let mut params = Vec::new();

    let (projection, distinct) = match rng.next_u64() % 6 {
        0 => ("DISTINCT g, a", true),
        1 => ("DISTINCT b", true),
        2 => ("DISTINCT a, b", true),
        3 => ("id, g, a, b", false),
        4 => ("g, b", false),
        _ => ("a", false),
    };

    // With DISTINCT the primary key is gone, so the order has to be total over
    // what survives instead: every projected column, in order. Without it, the
    // key itself is the tie-breaker.
    let order = if distinct {
        let columns: Vec<&str> = projection
            .trim_start_matches("DISTINCT ")
            .split(", ")
            .collect();
        let mut terms = Vec::new();
        for column in columns {
            let nulls = match rng.next_u64() % 3 {
                0 => " NULLS FIRST",
                1 => " NULLS LAST",
                _ => "",
            };
            let direction = if rng.next_u64().is_multiple_of(2) {
                " DESC"
            } else {
                ""
            };
            terms.push(format!("{column}{direction}{nulls}"));
        }
        terms.join(", ")
    } else {
        let count = 1 + rng.next_u64() % 3;
        let mut terms: Vec<String> = (0..count).map(|_| order_term(rng)).collect();
        terms.push("id".to_string());
        terms.join(", ")
    };

    let mut sql = format!("SELECT {projection} FROM t ORDER BY {order}");

    match rng.next_u64() % 5 {
        0 => {}
        1 => sql.push_str(&format!(" LIMIT {}", rng.next_u64() % 6)),
        2 => {
            sql.push_str(&format!(
                " LIMIT {} OFFSET {}",
                rng.next_u64() % 6,
                rng.next_u64() % 6
            ));
        }
        // `LIMIT ?` and `LIMIT ? OFFSET ?`, bound rather than written.
        3 => {
            sql.push_str(" LIMIT ?");
            params.push((rng.next_u64() % 6) as i64);
        }
        _ => {
            sql.push_str(" LIMIT ? OFFSET ?");
            params.push((rng.next_u64() % 6) as i64);
            params.push((rng.next_u64() % 6) as i64);
        }
    }

    (sql, params)
}

#[test]
fn query_shape_agrees_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_shape_rows(&mut rng);
        let (sql, params) = shape_query(&mut rng);

        let mut db = shape_db(&rows).expect("InlaySQL must build the table");
        let bound: Vec<Value> = params.iter().copied().map(Value::Integer).collect();
        let ours = inlaysql_rows(&mut db, &sql, &bound)
            .unwrap_or_else(|error| panic!("seed {seed}: InlaySQL failed on `{sql}`: {error}"));

        let conn = shape_sqlite(&rows).expect("SQLite must build the table");
        let theirs = sqlite_rows(&conn, &sql, &params).expect("SQLite is the oracle");

        assert_eq!(
            ours, theirs,
            "seed {seed}: `{sql}` (params {params:?}) disagreed with SQLite\nrows: {rows:?}"
        );
    }
}

// ------------------------------------------------ DISTINCT and GROUP_CONCAT
//
// `COUNT(DISTINCT x)` has one right answer, so it is compared directly.
// `GROUP_CONCAT` does not: SQLite documents the order of the concatenated
// values as arbitrary, so comparing the strings would be asserting an
// implementation detail of whichever engine happened to be right. What *is*
// defined is the multiset of values and the separator between them, so the
// result is split and sorted before comparing — which still catches a dropped
// value, a counted NULL, a wrong separator, or a group that concatenated the
// wrong rows.

const GROUPED_AGGREGATE_QUERY: &str = "SELECT g, COUNT(DISTINCT a), COUNT(DISTINCT b), \
     SUM(DISTINCT a), GROUP_CONCAT(a), GROUP_CONCAT(b, '|'), GROUP_CONCAT(DISTINCT b) \
     FROM t GROUP BY g ORDER BY g";

/// Sort the pieces of every `GROUP_CONCAT` result so that an order SQLite
/// never promised cannot fail the comparison.
fn normalise_concat(rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .enumerate()
                .map(|(column, value)| {
                    // Columns 4, 5 and 6 are the GROUP_CONCAT results.
                    if column < 4 || value == "NULL" {
                        return value;
                    }
                    let separator = if column == 5 { '|' } else { ',' };
                    let mut parts: Vec<&str> = value.split(separator).collect();
                    parts.sort_unstable();
                    parts.join(&separator.to_string())
                })
                .collect()
        })
        .collect()
}

#[test]
fn distinct_aggregates_and_group_concat_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_shape_rows(&mut rng);

        let mut db = shape_db(&rows).expect("InlaySQL must build the table");
        let ours = normalise_concat(
            inlaysql_rows(&mut db, GROUPED_AGGREGATE_QUERY, &[])
                .unwrap_or_else(|error| panic!("seed {seed}: InlaySQL failed: {error}")),
        );

        let conn = shape_sqlite(&rows).expect("SQLite must build the table");
        let theirs = normalise_concat(
            sqlite_rows(&conn, GROUPED_AGGREGATE_QUERY, &[]).expect("SQLite is the oracle"),
        );

        assert_eq!(
            ours, theirs,
            "seed {seed}: distinct aggregates disagreed with SQLite\nrows: {rows:?}"
        );
    }
}

// ----------------------------------------------- constraints and write statements
//
// The generators above compare what a `SELECT` *reads*. This one compares what
// a sequence of writes *leaves behind*, which is the only way to test a
// constraint against an oracle: a constraint has no value to project, only a
// decision about whether a row is allowed, and the evidence for that decision
// is the table afterwards.
//
// So each round builds one table with every constraint kind on it, runs a
// random sequence of writes through both engines, and compares three things:
// which statements were accepted, what each `RETURNING` clause gave back, and
// the whole table at the end. The last one is what catches a constraint that
// rejects the right row and writes it anyway, or a rejection that leaves half
// a statement behind.
//
// Two deliberate choices about the table:
//
// * `AUTOINCREMENT`, so that a key the engine assigns is comparable at all.
//   SQLite's plain row id reuses the highest key after a `DELETE`; its
//   `AUTOINCREMENT` does not, and neither does InlaySQL's counter, which is
//   monotonic and persisted. Declaring it makes the two engines agree by
//   construction rather than by luck.
// * The error *messages* are not compared, only whether there was one. Two
//   engines phrasing the same refusal differently is not a disagreement about
//   behaviour, and asserting on the text would be testing the message.

const WRITE_DDL: &str = "CREATE TABLE t (\
     id INTEGER PRIMARY KEY AUTOINCREMENT, \
     e TEXT UNIQUE, \
     n INTEGER NOT NULL DEFAULT 0 CHECK (n >= 0), \
     m INTEGER)";

const WRITE_READBACK: &str = "SELECT id, e, n, m FROM t ORDER BY id";

/// How many statements one round runs. Long enough that the table accumulates
/// a history — conflicts need rows to conflict with — and short enough that a
/// failing sequence is readable.
const WRITE_STATEMENTS: usize = 12;

/// One generated write statement.
///
/// Every branch stays inside the ground both engines stand on, for the same
/// reason the expression generators do: type-consistent values, keys drawn
/// from a small range so collisions actually happen, and `n` sometimes
/// negative so the `CHECK` sometimes fires.
fn write_statement(rng: &mut SeededRng) -> String {
    let id = 1 + rng.next_u64() % 4;
    let word = WORDS[(rng.next_u64() % 4) as usize];
    let other = WORDS[(rng.next_u64() % 4) as usize];
    // Signed, so roughly one in five violates `CHECK (n >= 0)`.
    let n = (rng.next_u64() % 6) as i64 - 1;
    let m = (rng.next_u64() % 4) as i64;

    match rng.next_u64() % 18 {
        // Plain inserts: explicit key, assigned key, and the omitted column
        // that has to take its `DEFAULT`.
        0 => format!("INSERT INTO t (id, e, n, m) VALUES ({id}, '{word}', {n}, {m})"),
        1 => format!("INSERT INTO t (e, n) VALUES ('{word}', {n})"),
        2 => format!("INSERT INTO t (id, e) VALUES ({id}, '{word}')"),
        3 => format!("INSERT INTO t (e) VALUES ('{word}')"),
        // A `NOT NULL` column named and set to NULL: rejected, where the same
        // column omitted is not.
        4 => format!("INSERT INTO t (id, e, n) VALUES ({id}, '{word}', NULL)"),

        // The conflict clauses.
        5 => format!("INSERT OR IGNORE INTO t (id, e, n) VALUES ({id}, '{word}', {n})"),
        6 => format!("INSERT OR REPLACE INTO t (id, e, n) VALUES ({id}, '{word}', {n})"),
        7 => format!("REPLACE INTO t (id, e, n) VALUES ({id}, '{word}', {n})"),
        8 => {
            format!("INSERT INTO t (id, e, n) VALUES ({id}, '{word}', {n}) ON CONFLICT DO NOTHING")
        }
        9 => format!(
            "INSERT INTO t (id, e, n) VALUES ({id}, '{word}', {n}) \
             ON CONFLICT (id) DO UPDATE SET n = n + excluded.n"
        ),
        10 => format!(
            "INSERT INTO t (id, e, n) VALUES ({id}, '{word}', {n}) \
             ON CONFLICT (id) DO UPDATE SET e = excluded.e, m = {m} WHERE n < excluded.n"
        ),
        11 => format!(
            "INSERT INTO t (id, e, n) VALUES ({id}, '{word}', {n}) \
             ON CONFLICT (e) DO UPDATE SET m = coalesce(m, 0) + 1"
        ),

        // Updates and deletes, including one that moves the primary key.
        12 => format!("UPDATE t SET n = n + {n} WHERE id = {id}"),
        13 => format!("UPDATE t SET e = '{word}' WHERE e = '{other}'"),
        14 => format!("UPDATE t SET m = {m} WHERE n >= {m}"),
        15 => format!("DELETE FROM t WHERE id = {id}"),

        // `RETURNING`, whose rows are compared as well as its effect.
        16 => format!("INSERT INTO t (e, n) VALUES ('{word}', {n}) RETURNING id, e, n, m"),
        _ => format!("DELETE FROM t WHERE id = {id} RETURNING id, e, n"),
    }
}

/// Whether one statement was accepted, and the rows its `RETURNING` gave back.
type WriteOutcome = (bool, Vec<Vec<String>>);

fn inlaysql_writes(statements: &[String]) -> (Vec<WriteOutcome>, Vec<Vec<String>>) {
    inlaysql_writes_with(&[], statements)
}

/// The same, with extra DDL — index declarations — run after the table.
fn inlaysql_writes_with(
    extra: &[&str],
    statements: &[String],
) -> (Vec<WriteOutcome>, Vec<Vec<String>>) {
    let mut db = Database::open_in_memory().expect("open");
    db.execute(WRITE_DDL, &[])
        .expect("InlaySQL must build the table");
    for sql in extra {
        db.execute(sql, &[])
            .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }

    let mut outcomes = Vec::with_capacity(statements.len());
    for sql in statements {
        outcomes.push(match db.execute(sql, &[]) {
            Ok(Outcome::Rows(result)) => (
                true,
                result
                    .rows
                    .iter()
                    .map(|row| row.iter().map(exact_inlaysql).collect())
                    .collect(),
            ),
            Ok(_) => (true, Vec::new()),
            // A construct the dialect does not implement is a gap, and this
            // generator only emits implemented ones — so it is a bug, not a
            // refusal to absorb.
            Err(error @ (inlaysql::Error::Unsupported(_) | inlaysql::Error::Parse(_))) => {
                panic!("`{sql}` is not implemented: {error}")
            }
            Err(_) => (false, Vec::new()),
        });
    }
    let final_rows = inlaysql_rows(&mut db, WRITE_READBACK, &[]).expect("read back");
    (outcomes, final_rows)
}

fn sqlite_writes(statements: &[String]) -> (Vec<WriteOutcome>, Vec<Vec<String>>) {
    sqlite_writes_with(&[], statements)
}

fn sqlite_writes_with(
    extra: &[&str],
    statements: &[String],
) -> (Vec<WriteOutcome>, Vec<Vec<String>>) {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    conn.execute(WRITE_DDL, [])
        .expect("SQLite must build the table");
    for sql in extra {
        conn.execute(sql, [])
            .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }

    let mut outcomes = Vec::with_capacity(statements.len());
    for sql in statements {
        // `Statement::query` is what runs the statement, `RETURNING` or not:
        // a statement with no rows simply yields none on the first step.
        outcomes.push(match sqlite_rows(&conn, sql, &[]) {
            Ok(rows) => (true, rows),
            Err(_) => (false, Vec::new()),
        });
    }
    let final_rows = sqlite_rows(&conn, WRITE_READBACK, &[]).expect("read back");
    (outcomes, final_rows)
}

#[test]
fn constrained_writes_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let statements: Vec<String> = (0..WRITE_STATEMENTS)
            .map(|_| write_statement(&mut rng))
            .collect();

        let (ours, our_rows) = inlaysql_writes(&statements);
        let (theirs, their_rows) = sqlite_writes(&statements);

        // Report the first divergence with the statement that caused it,
        // rather than only the table at the end: a wrong decision on
        // statement 3 shows up as a different table twelve statements later,
        // and that is not where anyone would start looking.
        for (index, (ours, theirs)) in ours.iter().zip(theirs.iter()).enumerate() {
            assert_eq!(
                ours.0,
                theirs.0,
                "seed {seed}: statement {index} `{}` was {} here and {} in SQLite\n\
                 sequence: {statements:#?}",
                statements[index],
                if ours.0 { "accepted" } else { "rejected" },
                if theirs.0 { "accepted" } else { "rejected" },
            );
            assert_eq!(
                ours.1, theirs.1,
                "seed {seed}: statement {index} `{}` returned different rows\n\
                 sequence: {statements:#?}",
                statements[index]
            );
        }

        assert_eq!(
            our_rows, their_rows,
            "seed {seed}: the table disagreed after the sequence\n{statements:#?}"
        );
    }
}

// ------------------------------------------------------- secondary indexes
//
// An index is only allowed to change how many rows are *read*. Every test in
// this section therefore asserts the same query three ways: InlaySQL with the
// index, InlaySQL without it, and SQLite. The middle comparison is the one an
// oracle alone would miss — if both InlaySQL sides were wrong in the same way
// SQLite would catch it, but if only the indexed side is wrong, "indexed
// equals unindexed" is what names the culprit.
//
// The DDL is deliberately legal in both engines. SQLite has no `USING` in
// `CREATE INDEX`, and on a `TEXT` column InlaySQL's inferred kind is the BM25
// index, so the text index is spelled with `USING BTREE` here and plainly
// there — the same structure by two names.

const INDEX_DDL_OURS: &[&str] = &[
    "CREATE INDEX t_a ON t (a)",
    "CREATE INDEX t_b ON t (b) USING BTREE",
    "CREATE INDEX t_ab ON t (a, b)",
];

/// The predicate rows, run through an InlaySQL database that declares indexes
/// on both columns and on the pair.
fn inlaysql_indexed_ids(rows: &[Row], where_clause: &str) -> Result<Vec<i64>, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
        &[],
    )?;
    for sql in INDEX_DDL_OURS {
        db.execute(sql, &[])?;
    }
    for (index, row) in rows.iter().enumerate() {
        db.execute(
            "INSERT INTO t (id, a, b) VALUES (?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                row.a.map(Value::Integer).unwrap_or(Value::Null),
                row.b
                    .map(|text| Value::Text(text.to_string().into()))
                    .unwrap_or(Value::Null),
            ],
        )?;
    }
    let result = db.query(
        &format!("SELECT id FROM t WHERE {where_clause} ORDER BY id"),
        &[],
    )?;
    Ok(result
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("id came back as {other:?}"),
        })
        .collect())
}

/// The same generated predicates the unindexed test uses, now asked of a table
/// that has indexes — and of one that does not, and of SQLite.
#[test]
fn indexed_predicates_agree_with_sqlite_and_with_the_unindexed_table() {
    let mut unsupported = 0u64;
    let mut total = 0u64;
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let clause = predicate(&mut rng, 3);
        total += 1;

        let indexed = match inlaysql_indexed_ids(&rows, &clause) {
            Ok(ids) => ids,
            Err(inlaysql::Error::Unsupported(_)) | Err(inlaysql::Error::Parse(_)) => {
                unsupported += 1;
                continue;
            }
            Err(error) => panic!("seed {seed}: InlaySQL failed on `{clause}`: {error}"),
        };
        let plain = inlaysql_ids(&rows, &clause).expect("the unindexed side answered a moment ago");
        assert_eq!(
            indexed, plain,
            "seed {seed}: `SELECT id FROM t WHERE {clause}` returned different rows with the \
             index than without it\nrows: {rows:?}"
        );
        let theirs = sqlite_ids(&rows, &clause).expect("SQLite is the oracle and must answer");
        assert_eq!(
            indexed, theirs,
            "seed {seed}: `SELECT id FROM t WHERE {clause}` disagreed with SQLite\nrows: {rows:?}"
        );
    }
    assert!(
        unsupported * 4 < total,
        "{unsupported} of {total} generated predicates were unsupported"
    );
}

/// The write path, with the indexes in place: every statement has to be
/// accepted or rejected the same way, return the same `RETURNING` rows, and
/// leave the same table — against SQLite and against the same engine with no
/// indexes at all.
///
/// This is where a maintenance bug shows up. An `UPDATE` that leaves the old
/// entry behind, or a `DELETE` that does not remove one, changes nothing about
/// the rows; it changes what a later predicate finds, and only a sequence long
/// enough to write and then read the same key catches it.
#[test]
fn indexed_writes_agree_with_sqlite_and_with_the_unindexed_table() {
    const EXTRA: &[&str] = &["CREATE INDEX t_n ON t (n)", "CREATE INDEX t_m ON t (m)"];
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let statements: Vec<String> = (0..WRITE_STATEMENTS)
            .map(|_| write_statement(&mut rng))
            // Read the table back through each index as well as through the
            // primary key, so a stale entry is visible.
            .chain([
                "SELECT id, e, n, m FROM t WHERE n >= 0 ORDER BY id".to_string(),
                "SELECT id, e, n, m FROM t WHERE e = 'alpha' ORDER BY id".to_string(),
                "SELECT id, e, n, m FROM t WHERE m IS NULL ORDER BY id".to_string(),
            ])
            .collect();

        let (ours, our_rows) = inlaysql_writes_with(EXTRA, &statements);
        let (plain, plain_rows) = inlaysql_writes(&statements);
        let (theirs, their_rows) = sqlite_writes_with(EXTRA, &statements);

        for (index, ((ours, plain), theirs)) in
            ours.iter().zip(plain.iter()).zip(theirs.iter()).enumerate()
        {
            assert_eq!(
                (ours.0, &ours.1),
                (plain.0, &plain.1),
                "seed {seed}: statement {index} `{}` behaved differently with the index than \
                 without it\nsequence: {statements:#?}",
                statements[index]
            );
            assert_eq!(
                (ours.0, &ours.1),
                (theirs.0, &theirs.1),
                "seed {seed}: statement {index} `{}` disagreed with SQLite\n\
                 sequence: {statements:#?}",
                statements[index]
            );
        }
        assert_eq!(
            our_rows, plain_rows,
            "seed {seed}: the table disagreed with the unindexed one\n{statements:#?}"
        );
        assert_eq!(
            our_rows, their_rows,
            "seed {seed}: the table disagreed with SQLite\n{statements:#?}"
        );
    }
}

/// `CREATE INDEX` over a table that already has rows has to describe them, and
/// every query has to answer identically before and after the index exists.
#[test]
fn building_an_index_over_existing_rows_changes_no_answer() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let clause = predicate(&mut rng, 3);

        let mut db = Database::open_in_memory().expect("open");
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
            &[],
        )
        .expect("create");
        for (index, row) in rows.iter().enumerate() {
            db.execute(
                "INSERT INTO t (id, a, b) VALUES (?, ?, ?)",
                &[
                    Value::Integer(index as i64 + 1),
                    row.a.map(Value::Integer).unwrap_or(Value::Null),
                    row.b
                        .map(|text| Value::Text(text.to_string().into()))
                        .unwrap_or(Value::Null),
                ],
            )
            .expect("insert");
        }

        let query = format!("SELECT id FROM t WHERE {clause} ORDER BY id");
        let before = match inlaysql_rows(&mut db, &query, &[]) {
            Ok(rows) => rows,
            Err(inlaysql::Error::Unsupported(_)) | Err(inlaysql::Error::Parse(_)) => continue,
            Err(error) => panic!("seed {seed}: `{clause}`: {error}"),
        };
        for sql in INDEX_DDL_OURS {
            db.execute(sql, &[]).expect("create index");
        }
        let after = inlaysql_rows(&mut db, &query, &[]).expect("the same query answered before");
        assert_eq!(
            before, after,
            "seed {seed}: `{query}` changed answer when the index was built\nrows: {rows:?}"
        );
    }
}

// ------------------------------------------------------------- subqueries
//
// Scalar `(SELECT ...)`, `IN (SELECT ...)`, `EXISTS (SELECT ...)`, derived
// tables, and the correlated forms of all three (AHL-463). Two tables so that
// a subquery has somewhere else to read from, and both are NULL-heavy for the
// same reason every generator here is: three-valued logic is where the bugs
// are, and `IN (SELECT ...)` has four separate NULL rules that a hand-written
// test is unlikely to hit in combination.
//
// Two deliberate choices about what is generated:
//
// * **Every subquery's result is either aggregated or ordered.** A scalar
//   subquery that returns several rows is not an error in SQLite — it takes
//   the first — so an unordered multi-row one would be comparing two engines'
//   scan order rather than their semantics. `subqueries.test` pins the
//   first-row rule directly instead, where the order is fixed by construction.
// * **Type-consistent comparisons**, as everywhere else in this file: the
//   integer columns are compared to integer columns and the text ones to text.

const SUB_DDL_T: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)";
const SUB_DDL_U: &str = "CREATE TABLE u (id INTEGER PRIMARY KEY, a INTEGER, k INTEGER, b TEXT)";

/// One row of the subquery generator's second table.
#[derive(Debug, Clone)]
struct SubRow {
    a: Option<i64>,
    /// A foreign-key-ish column drawn from `t`'s key range, so a correlated
    /// `u.k = t.id` matches sometimes and misses sometimes.
    k: Option<i64>,
    b: Option<&'static str>,
}

fn generate_sub_rows(rng: &mut SeededRng) -> Vec<SubRow> {
    (0..ROWS)
        .map(|_| SubRow {
            a: (!rng.next_u64().is_multiple_of(5)).then(|| (rng.next_u64() % VALUE_RANGE) as i64),
            k: (!rng.next_u64().is_multiple_of(6))
                .then(|| (rng.next_u64() % (ROWS as u64 + 2)) as i64),
            b: (!rng.next_u64().is_multiple_of(5)).then(|| WORDS[(rng.next_u64() % 4) as usize]),
        })
        .collect()
}

/// The `WHERE` of an inner query: sometimes correlated, sometimes not.
///
/// `""` is a subquery with no filter at all, which is the shape that reads the
/// whole inner table for every outer row.
fn inner_where(rng: &mut SeededRng) -> String {
    let value = rng.next_u64() % VALUE_RANGE;
    let word = WORDS[(rng.next_u64() % 4) as usize];
    match rng.next_u64() % 12 {
        0 => String::new(),
        1 => format!(" WHERE u.a > {value}"),
        2 => format!(" WHERE u.a <= {value}"),
        3 => " WHERE u.a IS NOT NULL".to_string(),
        4 => format!(" WHERE u.b = '{word}'"),
        5 => " WHERE u.k IS NULL".to_string(),
        // Correlated from here down.
        6 => " WHERE u.k = t.id".to_string(),
        7 => " WHERE u.a = t.a".to_string(),
        8 => " WHERE u.b = t.b".to_string(),
        9 => " WHERE u.a > t.a".to_string(),
        10 => format!(" WHERE u.k = t.id AND u.a <> {value}"),
        _ => " WHERE u.a IS NOT NULL AND u.k <= t.id".to_string(),
    }
}

/// A subquery expression over `t`'s row, projected as a value.
fn subquery_value(rng: &mut SeededRng) -> String {
    let filter = inner_where(rng);
    let value = rng.next_u64() % VALUE_RANGE;
    let word = WORDS[(rng.next_u64() % 4) as usize];
    match rng.next_u64() % 16 {
        // Aggregated scalar subqueries: one row by construction.
        0 => format!("(SELECT COUNT(*) FROM u{filter})"),
        1 => format!("(SELECT MAX(u.a) FROM u{filter})"),
        2 => format!("(SELECT MIN(u.a) FROM u{filter})"),
        3 => format!("(SELECT SUM(u.a) FROM u{filter})"),
        4 => format!("(SELECT COUNT(u.a) FROM u{filter})"),
        5 => format!("(SELECT MAX(u.b) FROM u{filter})"),
        // Ordered and limited to one row, which is the other way to make
        // "the first row" mean the same thing in both engines.
        6 => format!("(SELECT u.a FROM u{filter} ORDER BY u.a, u.id LIMIT 1)"),
        7 => format!("(SELECT u.b FROM u{filter} ORDER BY u.b DESC, u.id LIMIT 1)"),
        8 => format!("(SELECT u.id FROM u{filter} ORDER BY u.id LIMIT 1)"),
        // EXISTS, which is never NULL.
        9 => format!("EXISTS (SELECT 1 FROM u{filter})"),
        10 => format!("NOT EXISTS (SELECT 1 FROM u{filter})"),
        // IN, in both directions and against both column types.
        11 => format!("t.a IN (SELECT u.a FROM u{filter})"),
        12 => format!("t.a NOT IN (SELECT u.a FROM u{filter})"),
        13 => format!("t.b IN (SELECT u.b FROM u{filter})"),
        14 => format!("{value} IN (SELECT u.a FROM u{filter})"),
        _ => format!("'{word}' NOT IN (SELECT u.b FROM u{filter})"),
    }
}

/// A subquery predicate over `t`'s row, for a `WHERE`.
fn subquery_predicate(rng: &mut SeededRng) -> String {
    let filter = inner_where(rng);
    let value = rng.next_u64() % VALUE_RANGE;
    match rng.next_u64() % 14 {
        0 => format!("EXISTS (SELECT 1 FROM u{filter})"),
        1 => format!("NOT EXISTS (SELECT 1 FROM u{filter})"),
        2 => format!("t.a IN (SELECT u.a FROM u{filter})"),
        3 => format!("t.a NOT IN (SELECT u.a FROM u{filter})"),
        4 => format!("t.id IN (SELECT u.k FROM u{filter})"),
        5 => format!("t.id NOT IN (SELECT u.k FROM u{filter})"),
        6 => format!("t.b IN (SELECT u.b FROM u{filter})"),
        7 => format!("t.a > (SELECT MIN(u.a) FROM u{filter})"),
        8 => format!("t.a = (SELECT MAX(u.a) FROM u{filter})"),
        9 => format!("(SELECT COUNT(*) FROM u{filter}) > {value}"),
        10 => format!("EXISTS (SELECT 1 FROM u{filter}) AND t.a IS NOT NULL",),
        // The same uncorrelated subquery twice in one predicate, which is what
        // the executor's memo answers the second time without re-reading.
        11 => format!(
            "(SELECT COUNT(*) FROM u WHERE u.a IS NOT NULL) > {value} \
             OR (SELECT COUNT(*) FROM u WHERE u.a IS NOT NULL) = t.a"
        ),
        // A subquery inside a subquery, whose innermost level names a column
        // two levels out — the capture chain, which nothing else here reaches.
        12 => {
            let joiner = if filter.is_empty() { " WHERE" } else { " AND" };
            format!(
                "EXISTS (SELECT 1 FROM u{filter}{joiner} u.a IN \
                 (SELECT v.a FROM u AS v WHERE v.id <= t.id))"
            )
        }
        // A derived table inside a subquery, with a correlated subquery inside
        // *it*. The derived table starts a fresh scope chain while the capture
        // stack stays as deep as the subquery nesting, and confusing the two
        // put the inner capture in the outer subquery's list.
        _ => format!(
            "EXISTS (SELECT 1 FROM \
             (SELECT (SELECT COUNT(*) FROM u AS w WHERE w.a = x.a) AS c FROM u AS x) AS d \
             WHERE d.c > {value})"
        ),
    }
}

/// A whole query built around a derived table.
fn derived_query(rng: &mut SeededRng) -> String {
    let filter = inner_where(rng);
    // `inner_where` may be correlated, which a derived table may not be — it
    // is not `LATERAL`. Fall back to no filter in that case rather than
    // generating something both engines reject for the same reason.
    let filter = if filter.contains("t.") {
        String::new()
    } else {
        filter
    };
    let value = rng.next_u64() % VALUE_RANGE;
    match rng.next_u64() % 9 {
        0 => format!("SELECT n FROM (SELECT u.id AS n FROM u{filter}) ORDER BY n"),
        1 => format!("SELECT COUNT(*) FROM (SELECT u.id AS n FROM u{filter})"),
        2 => format!(
            "SELECT n, v FROM (SELECT u.id AS n, u.a AS v FROM u{filter}) WHERE v > {value} \
             ORDER BY n"
        ),
        3 => format!("SELECT SUM(v) FROM (SELECT u.a AS v FROM u{filter} ORDER BY u.id LIMIT 5)"),
        4 => format!(
            "SELECT d.n, t.a FROM (SELECT u.id AS n FROM u{filter}) AS d JOIN t ON t.id = d.n \
             ORDER BY d.n"
        ),
        5 => format!(
            "SELECT d.n, t.a FROM (SELECT u.id AS n FROM u{filter}) AS d LEFT JOIN t \
             ON t.a = d.n ORDER BY d.n, t.a"
        ),
        6 => format!(
            "SELECT m FROM (SELECT n AS m FROM (SELECT u.id AS n FROM u{filter})) ORDER BY m"
        ),
        7 => format!(
            "SELECT k, COUNT(*) FROM (SELECT u.k AS k FROM u{filter}) GROUP BY k ORDER BY k"
        ),
        // A correlated subquery inside a derived table, correlating to the
        // derived table's own source rather than to anything outside it. No
        // `inner_where` here: the source is aliased, so `u.` no longer names it.
        _ => "SELECT n, c FROM (SELECT x.id AS n, \
              (SELECT COUNT(*) FROM u AS w WHERE w.a = x.a) AS c FROM u AS x) ORDER BY n"
            .to_string(),
    }
}

fn subquery_db(rows: &[Row], sub: &[SubRow]) -> Result<Database, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute(SUB_DDL_T, &[])?;
    db.execute(SUB_DDL_U, &[])?;
    for (index, row) in rows.iter().enumerate() {
        db.execute(
            "INSERT INTO t (id, a, b) VALUES (?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                row.a.map(Value::Integer).unwrap_or(Value::Null),
                row.b
                    .map(|text| Value::Text(text.to_string().into()))
                    .unwrap_or(Value::Null),
            ],
        )?;
    }
    for (index, row) in sub.iter().enumerate() {
        db.execute(
            "INSERT INTO u (id, a, k, b) VALUES (?, ?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                row.a.map(Value::Integer).unwrap_or(Value::Null),
                row.k.map(Value::Integer).unwrap_or(Value::Null),
                row.b
                    .map(|text| Value::Text(text.to_string().into()))
                    .unwrap_or(Value::Null),
            ],
        )?;
    }
    Ok(db)
}

fn subquery_sqlite(rows: &[Row], sub: &[SubRow]) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute(SUB_DDL_T, [])?;
    conn.execute(SUB_DDL_U, [])?;
    for (index, row) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO t (id, a, b) VALUES (?1, ?2, ?3)",
            rusqlite::params![index as i64 + 1, row.a, row.b],
        )?;
    }
    for (index, row) in sub.iter().enumerate() {
        conn.execute(
            "INSERT INTO u (id, a, k, b) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![index as i64 + 1, row.a, row.k, row.b],
        )?;
    }
    Ok(conn)
}

/// Run one generated query through both engines and compare, exactly.
///
/// An `Unsupported` or `Parse` from this engine is a failure rather than a
/// skip: unlike the predicate generator, every branch of the subquery grammar
/// is a form that is implemented, so a refusal means the grammar and the
/// planner have drifted apart.
fn assert_subquery_agrees(seed: u64, sql: &str, rows: &[Row], sub: &[SubRow]) {
    let mut db = subquery_db(rows, sub).expect("InlaySQL must build the tables");
    let ours = inlaysql_rows(&mut db, sql, &[])
        .unwrap_or_else(|error| panic!("seed {seed}: InlaySQL failed on `{sql}`: {error}"));
    let conn = subquery_sqlite(rows, sub).expect("SQLite must build the tables");
    let theirs = sqlite_rows(&conn, sql, &[]).expect("SQLite is the oracle and must answer");
    assert_eq!(
        ours, theirs,
        "seed {seed}: `{sql}` disagreed with SQLite\nt: {rows:?}\nu: {sub:?}"
    );
}

#[test]
fn subquery_values_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let sub = generate_sub_rows(&mut rng);
        let expr = subquery_value(&mut rng);
        let sql = format!("SELECT t.id, {expr} FROM t ORDER BY t.id");
        assert_subquery_agrees(seed, &sql, &rows, &sub);
    }
}

#[test]
fn subquery_predicates_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let sub = generate_sub_rows(&mut rng);
        let clause = subquery_predicate(&mut rng);
        let sql = format!("SELECT t.id FROM t WHERE {clause} ORDER BY t.id");
        assert_subquery_agrees(seed, &sql, &rows, &sub);
    }
}

/// The same predicates under `NOT`, which is where a `0` returned in place of
/// a `NULL` stops being invisible — the reason this file has a value oracle as
/// well as a row oracle.
#[test]
fn negated_subquery_predicates_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let sub = generate_sub_rows(&mut rng);
        let clause = subquery_predicate(&mut rng);
        let sql = format!("SELECT t.id FROM t WHERE NOT ({clause}) ORDER BY t.id");
        assert_subquery_agrees(seed, &sql, &rows, &sub);
    }
}

#[test]
fn derived_tables_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let sub = generate_sub_rows(&mut rng);
        let sql = derived_query(&mut rng);
        assert_subquery_agrees(seed, &sql, &rows, &sub);
    }
}

/// SQLite creates an index for a `UNIQUE` constraint too, so the two engines
/// can be compared on the one thing an index-backed constraint could get
/// wrong: which rows it considers a collision.
#[test]
fn unique_index_collisions_agree_with_sqlite() {
    const DDL: &str = "CREATE TABLE u (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)";
    const INDEX: &str = "CREATE UNIQUE INDEX u_ab ON u (a, b)";
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);

        let mut db = Database::open_in_memory().expect("open");
        db.execute(DDL, &[]).expect("create");
        db.execute(INDEX, &[]).expect("index");
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute(DDL, []).expect("create");
        conn.execute(INDEX, []).expect("index");

        for (index, row) in rows.iter().enumerate() {
            let id = index as i64 + 1;
            let ours = db
                .execute(
                    "INSERT INTO u (id, a, b) VALUES (?, ?, ?)",
                    &[
                        Value::Integer(id),
                        row.a.map(Value::Integer).unwrap_or(Value::Null),
                        row.b
                            .map(|text| Value::Text(text.to_string().into()))
                            .unwrap_or(Value::Null),
                    ],
                )
                .is_ok();
            let theirs = conn
                .execute(
                    "INSERT INTO u (id, a, b) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, row.a, row.b],
                )
                .is_ok();
            assert_eq!(
                ours,
                theirs,
                "seed {seed}: row {id} {row:?} was {} here and {} in SQLite",
                if ours { "accepted" } else { "rejected" },
                if theirs { "accepted" } else { "rejected" },
            );
        }

        let ours = inlaysql_rows(&mut db, "SELECT id, a, b FROM u ORDER BY id", &[]).expect("read");
        let theirs = sqlite_rows(&conn, "SELECT id, a, b FROM u ORDER BY id", &[]).expect("read");
        assert_eq!(ours, theirs, "seed {seed}: the table disagreed");
    }
}

// ---------------------------------------------- set operations and CTEs (AHL-473)
//
// `UNION [ALL]` / `INTERSECT` / `EXCEPT` and non-recursive `WITH`, reusing the
// same `t`/`u` schema and generators the subquery grammar above already
// built. One thing this generator deliberately does *not* mix, on top of the
// file's usual type-consistent-comparison rule:
//
// * **Every compound is ordered before being read as a value list**, same
//   discipline as the subquery grammar just above: without a fixed order,
//   `LIMIT`/`OFFSET` and row-set comparison would be comparing the two
//   engines' unspecified internal ordering rather than their semantics.
//
// **Mixed-shape chains (AHL-477).** `compound_arm_int` and `compound_arm_text`
// used to never appear in the same chain: a chain feeds into `ORDER BY 1`,
// `=` and (before this) `IN (...)` in the queries below, and mixing
// INTEGER/TEXT there used to hit two real, pre-existing bugs outside this
// phase's original code — `engine.rs::compare_values` (`ORDER BY` over a
// genuinely mixed-class column could misorder or even misplace rows past the
// pair it mishandled, since a broken comparator corrupts a whole sort) and
// `eval.rs::comparison` (`=` *errored* instead of answering false for a
// cross-class pair SQLite just ranks apart). Both are fixed now — the
// comparator is a genuine total order over every storage class, confirmed
// against sqlite3 and pinned by an exhaustive property test
// (`engine.rs::tests::compare_values_is_a_total_order_over_every_storage_class`)
// — so `compound_chain` mixes the two shapes freely below. This is not the
// file's usual "type-inconsistent comparison" concession (that rule is about
// SQLite's idiosyncratic type-*affinity* conversion, which this project does
// not claim to reproduce): SQLite's storage-*class* ordering for an
// already-materialised value is a small, fixed, fully-specified rule, and
// this generator now exercises the real thing against the real oracle rather
// than working around a bug in it. `IN (...)`'s own right-hand side
// (`compound_arm_int` only, line ~50 below) stays unmixed on purpose — `t.a`/
// `t.id` are declared `INTEGER`, so a `TEXT` value there is a genuinely
// different, still-untested shape (an `IN (...)` list that itself mixes
// classes), not the same bug this phase fixed.

/// One arm shape, or the other, or both — every combination `UNION`/
/// `INTERSECT`/`EXCEPT` can chain across a class boundary now that both
/// comparators agree with sqlite3's fixed class order.
fn compound_arm(rng: &mut SeededRng) -> String {
    if rng.next_u64().is_multiple_of(2) {
        compound_arm_int(rng)
    } else {
        compound_arm_text(rng)
    }
}

/// One arm of a compound query, INTEGER-shaped (`a`, `id`, or `k`).
fn compound_arm_int(rng: &mut SeededRng) -> String {
    let filter = inner_where(rng);
    let filter = if filter.contains("t.") {
        String::new()
    } else {
        filter
    };
    let value = rng.next_u64() % VALUE_RANGE;
    match rng.next_u64() % 5 {
        0 => "SELECT a FROM t".to_string(),
        1 => format!("SELECT a FROM t WHERE a > {value}"),
        2 => "SELECT id FROM t WHERE a IS NOT NULL".to_string(),
        3 => format!("SELECT u.a FROM u{filter}"),
        _ => format!("SELECT u.k FROM u{filter}"),
    }
}

/// One arm of a compound query, TEXT-shaped (`b`).
fn compound_arm_text(rng: &mut SeededRng) -> String {
    let filter = inner_where(rng);
    let filter = if filter.contains("t.") {
        String::new()
    } else {
        filter
    };
    match rng.next_u64() % 2 {
        0 => "SELECT b FROM t".to_string(),
        _ => format!("SELECT u.b FROM u{filter}"),
    }
}

/// A chain of two or three arms, each built by `arm` — a fixed shape if the
/// caller passes `compound_arm_int` (`IN (...)` needs the shape that matches
/// the probe on the outer side) or a per-arm random one via `compound_arm`
/// (everywhere else is happy with either, and with both in one chain) —
/// joined by a random mix of `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT` — the
/// shape that exercises left-associative, equal-precedence chaining
/// (`sql.rs::flatten_compound`) across every combination of adjacent
/// operators.
fn compound_chain_of(rng: &mut SeededRng, arm: fn(&mut SeededRng) -> String) -> String {
    const OPS: [&str; 4] = ["UNION", "UNION ALL", "INTERSECT", "EXCEPT"];
    let mut sql = arm(rng);
    let extra_arms = 1 + (rng.next_u64() % 2);
    for _ in 0..extra_arms {
        let op = OPS[(rng.next_u64() % OPS.len() as u64) as usize];
        sql = format!("{sql} {op} {}", arm(rng));
    }
    sql
}

/// The same, with the shape randomised *per arm* (AHL-477) — every position
/// below except `IN (...)` (which needs to match the probe's own type) is
/// happy with a chain that crosses INTEGER/TEXT mid-chain now that both
/// comparators rank a cross-class pair instead of mishandling it.
fn compound_chain(rng: &mut SeededRng) -> String {
    compound_chain_of(rng, compound_arm)
}

/// A whole query built around a compound: bare and ordered, `LIMIT`/`OFFSET`
/// over the whole thing, or embedded in one of the positions AHL-463 already
/// supports for an ordinary subquery — `IN (...)`, `EXISTS (...)`, and a
/// derived table.
fn compound_query(rng: &mut SeededRng) -> String {
    match rng.next_u64() % 6 {
        0 => format!("{} ORDER BY 1", compound_chain(rng)),
        1 => format!("{} ORDER BY 1 LIMIT 5 OFFSET 1", compound_chain(rng)),
        // `t.a`/`t.id` are both INTEGER, so the chain on the right of `IN`
        // has to stay INTEGER-shaped too — a TEXT arm here is exactly the
        // type-inconsistent comparison this file's opening comment rules
        // out, not a new concern this phase introduces.
        2 => format!(
            "SELECT t.id FROM t WHERE t.a IN ({}) ORDER BY t.id",
            compound_chain_of(rng, compound_arm_int)
        ),
        3 => format!(
            "SELECT t.id FROM t WHERE t.id IN ({}) ORDER BY t.id",
            compound_chain_of(rng, compound_arm_int)
        ),
        4 => format!(
            "SELECT t.id FROM t WHERE EXISTS ({}) ORDER BY t.id",
            compound_chain(rng)
        ),
        _ => format!("SELECT * FROM ({}) AS d ORDER BY 1", compound_chain(rng)),
    }
}

#[test]
fn compound_queries_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let sub = generate_sub_rows(&mut rng);
        let sql = compound_query(&mut rng);
        assert_subquery_agrees(seed, &sql, &rows, &sub);
    }
}

/// A `WITH` clause and a query using it: a single CTE, one CTE referencing an
/// earlier one, the same CTE joined against itself (its planned body cloned
/// into two `FromItem`s — see `sql.rs::CteEntry`'s doc), a CTE shadowing the
/// real table `t`, and a CTE whose own body is a compound.
fn cte_query(rng: &mut SeededRng) -> String {
    let value = rng.next_u64() % VALUE_RANGE;
    match rng.next_u64() % 6 {
        0 => format!("WITH x AS (SELECT a FROM t WHERE a > {value}) SELECT a FROM x ORDER BY a"),
        1 => "WITH x(p) AS (SELECT id FROM t WHERE a IS NOT NULL) SELECT p FROM x ORDER BY p"
            .to_string(),
        2 => format!(
            "WITH y AS (SELECT a FROM t), x AS (SELECT a FROM y WHERE a > {value}) SELECT a \
             FROM x ORDER BY a"
        ),
        3 => "WITH x AS (SELECT id, a FROM t WHERE a IS NOT NULL) SELECT x1.id, x2.id FROM x AS \
              x1, x AS x2 WHERE x1.a = x2.a AND x1.id < x2.id ORDER BY x1.id, x2.id"
            .to_string(),
        4 => "WITH t AS (SELECT a FROM u WHERE u.k IS NOT NULL) SELECT a FROM t ORDER BY a"
            .to_string(),
        _ => format!(
            "WITH x AS ({}) SELECT * FROM x ORDER BY 1",
            compound_chain(rng)
        ),
    }
}

#[test]
fn cte_queries_agree_with_sqlite() {
    for seed in 0..rounds() {
        let mut rng = SeededRng::new(seed);
        let rows = generate_rows(&mut rng);
        let sub = generate_sub_rows(&mut rng);
        let sql = cte_query(&mut rng);
        assert_subquery_agrees(seed, &sql, &rows, &sub);
    }
}

// ------------------------------------------------------------- collations
//
// A collation changes what `=` *means*, so a mistake here is invisible until
// the row counts differ — the failure mode `docs/server.md` calls the most
// dangerous divergence in the project (AHL-469). SQLite is the oracle for all
// three of the collations this engine has, and the DDL below is legal in both
// engines unchanged, so nothing is being compared against a translation.

/// Words chosen so that case and trailing spaces both decide something, and so
/// that `BINARY` and `NOCASE` order them differently: upper-case letters sort
/// below lower-case ones byte-wise and beside them folded.
const COLLATED_WORDS: [&str; 9] = ["ada", "ADA", "Ada", "grace", "GRACE", "a", "a  ", "", "Z"];

/// The values a generated comparison is written against. A superset of
/// [`COLLATED_WORDS`], so a predicate can also name something no row holds.
const COLLATED_LITERALS: [&str; 11] = [
    "ada", "ADA", "Ada", "grace", "GRACE", "a", "a  ", "", "Z", "b", "aDa",
];

/// The three columns, one per collation, so every comparison can be written
/// against a column whose declared collation is the interesting one.
const COLLATED_COLUMNS: [&str; 3] = ["nc", "bin", "rt"];

/// An explicit `COLLATE`, or nothing. Generated on either side of a comparison
/// so the resolution rules — explicit beats implicit, leftmost first — are
/// exercised rather than assumed.
const COLLATE_SUFFIXES: [&str; 4] = ["", " COLLATE NOCASE", " COLLATE BINARY", " COLLATE RTRIM"];

const COLLATED_DDL: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, nc TEXT COLLATE NOCASE, \
                            bin TEXT, rt TEXT COLLATE RTRIM)";

/// The indexes, in each engine's spelling. SQLite has no `USING` on
/// `CREATE INDEX`, and on a `TEXT` column InlaySQL's inferred kind is the BM25
/// index — the same structure under two names, exactly as the integer/text
/// index test above already does it.
///
/// `bin` carries two: one keyed under `BINARY` and one under `NOCASE`. That is
/// the case the selection rule exists for, and generating explicit `COLLATE`
/// on either side of a comparison is what reaches both.
const COLLATED_INDEX_OURS: &[&str] = &[
    "CREATE INDEX t_nc ON t (nc) USING BTREE",
    "CREATE INDEX t_bin ON t (bin) USING BTREE",
    "CREATE INDEX t_bin_nc ON t (bin COLLATE NOCASE) USING BTREE",
    "CREATE INDEX t_rt ON t (rt) USING BTREE",
];

const COLLATED_INDEX_THEIRS: &[&str] = &[
    "CREATE INDEX t_nc ON t (nc)",
    "CREATE INDEX t_bin ON t (bin)",
    "CREATE INDEX t_bin_nc ON t (bin COLLATE NOCASE)",
    "CREATE INDEX t_rt ON t (rt)",
];

/// One generated row: three text columns, each of which may be `NULL`.
#[derive(Debug, Clone)]
struct CollatedRow {
    nc: Option<&'static str>,
    bin: Option<&'static str>,
    rt: Option<&'static str>,
}

fn generate_collated_rows(rng: &mut SeededRng) -> Vec<CollatedRow> {
    let pick = |rng: &mut SeededRng| {
        (!rng.next_u64().is_multiple_of(6))
            .then(|| COLLATED_WORDS[(rng.next_u64() as usize) % COLLATED_WORDS.len()])
    };
    (0..ROWS)
        .map(|_| CollatedRow {
            nc: pick(rng),
            bin: pick(rng),
            rt: pick(rng),
        })
        .collect()
}

fn collated_column(rng: &mut SeededRng) -> &'static str {
    COLLATED_COLUMNS[(rng.next_u64() as usize) % COLLATED_COLUMNS.len()]
}

fn collated_literal(rng: &mut SeededRng) -> &'static str {
    COLLATED_LITERALS[(rng.next_u64() as usize) % COLLATED_LITERALS.len()]
}

fn collate_suffix(rng: &mut SeededRng) -> &'static str {
    COLLATE_SUFFIXES[(rng.next_u64() as usize) % COLLATE_SUFFIXES.len()]
}

const COMPARISONS: [&str; 6] = ["=", "<>", "<", "<=", ">", ">="];

/// A random predicate over the three collated columns.
fn collated_predicate(rng: &mut SeededRng, depth: u32) -> String {
    if depth == 0 || rng.next_u64().is_multiple_of(3) {
        return collated_leaf(rng);
    }
    match rng.next_u64() % 4 {
        0 => format!(
            "({} AND {})",
            collated_predicate(rng, depth - 1),
            collated_predicate(rng, depth - 1)
        ),
        1 => format!(
            "({} OR {})",
            collated_predicate(rng, depth - 1),
            collated_predicate(rng, depth - 1)
        ),
        2 => format!("(NOT {})", collated_predicate(rng, depth - 1)),
        _ => collated_leaf(rng),
    }
}

fn collated_leaf(rng: &mut SeededRng) -> String {
    let column = collated_column(rng);
    let other = collated_column(rng);
    let word = collated_literal(rng);
    let second = collated_literal(rng);
    let op = COMPARISONS[(rng.next_u64() as usize) % COMPARISONS.len()];
    let left = collate_suffix(rng);
    let right = collate_suffix(rng);
    let pattern = LIKE_PATTERNS[(rng.next_u64() as usize) % LIKE_PATTERNS.len()];
    match rng.next_u64() % 16 {
        // The shape the whole item exists for, with and without an explicit
        // collation on either operand.
        0 => format!("{column}{left} {op} '{word}'{right}"),
        1 => format!("'{word}'{left} {op} {column}{right}"),
        // Column against column: the left one's collation wins, so the two
        // orders are genuinely different queries.
        2 => format!("{column}{left} {op} {other}{right}"),
        3 => format!("{other} {op} {column}"),
        // `IN` takes its collation from the left operand alone.
        4 => format!("{column}{left} IN ('{word}', '{second}')"),
        5 => format!("{column} NOT IN ('{word}'{right}, '{second}')"),
        6 => format!("'{word}'{left} IN ({column}, {other})"),
        // `BETWEEN` resolves its two bounds separately.
        7 => format!("{column} BETWEEN '{word}'{left} AND '{second}'{right}"),
        8 => format!("{column} NOT BETWEEN '{word}' AND '{second}'{right}"),
        // `LIKE` uses no collating sequence at all, whatever the column
        // declared — the one comparison that looks like it should and does not.
        9 => format!("{column} LIKE '{pattern}'"),
        10 => format!("{column} IS NULL"),
        // `CAST` is transparent to a column's collation; `||` is not, unless an
        // explicit `COLLATE` inside it propagates out.
        11 => format!("CAST({column} AS TEXT) {op} '{word}'"),
        12 => format!("({column}{left} || '') {op} '{word}'"),
        // The three collation-aware scalars.
        13 => format!("nullif({column}{left}, '{word}') IS NULL"),
        14 => format!("min({column}{left}, '{word}'{right}) {op} '{second}'"),
        // A simple `CASE`, which is one `=` per branch and resolves each on its
        // own.
        _ => format!(
            "(CASE {column} WHEN '{word}'{left} THEN 1 WHEN '{second}'{right} THEN 2 ELSE 0 END) \
             > 0"
        ),
    }
}

fn collated_db(rows: &[CollatedRow], indexes: &[&str]) -> Result<Database, inlaysql::Error> {
    let mut db = Database::open_in_memory()?;
    db.execute(COLLATED_DDL, &[])?;
    for sql in indexes {
        db.execute(sql, &[])?;
    }
    let cell = |value: Option<&'static str>| {
        value
            .map(|text| Value::Text(text.to_string().into()))
            .unwrap_or(Value::Null)
    };
    for (index, row) in rows.iter().enumerate() {
        db.execute(
            "INSERT INTO t (id, nc, bin, rt) VALUES (?, ?, ?, ?)",
            &[
                Value::Integer(index as i64 + 1),
                cell(row.nc),
                cell(row.bin),
                cell(row.rt),
            ],
        )?;
    }
    Ok(db)
}

fn collated_sqlite(
    rows: &[CollatedRow],
    indexes: &[&str],
) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute(COLLATED_DDL, [])?;
    for sql in indexes {
        conn.execute(sql, [])?;
    }
    for (index, row) in rows.iter().enumerate() {
        conn.execute(
            "INSERT INTO t (id, nc, bin, rt) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![index as i64 + 1, row.nc, row.bin, row.rt],
        )?;
    }
    Ok(conn)
}

/// The generated predicate, asked of an InlaySQL table with the indexes, of one
/// without them, and of SQLite.
///
/// All three have to agree. The middle one is what catches a collated index
/// probe reading a different run of bytes than the scan would have — the
/// divergence-by-access-path class this repository treats as the worst kind.
#[test]
fn collated_predicates_agree_with_sqlite_and_with_the_unindexed_table() {
    let total = rounds();
    let mut unsupported = 0u64;
    for seed in 0..total {
        let mut rng = SeededRng::new(seed);
        let rows = generate_collated_rows(&mut rng);
        let clause = collated_predicate(&mut rng, 3);
        // Half the rounds ask the same predicate of a *derived* table over the
        // same rows. A derived table's synthetic columns carry the projected
        // expressions' collations (SQLite's
        // `sqlite3SelectAddColumnTypeAndCollation`), so this is where a
        // collation would be lost by nesting — silently, and only for the
        // shape nobody thought to write a case for.
        let query = if rng.next_u64().is_multiple_of(2) {
            format!("SELECT id FROM t WHERE {clause} ORDER BY id")
        } else {
            format!(
                "SELECT id FROM (SELECT id, nc, bin, rt FROM t) AS t WHERE {clause} ORDER BY id"
            )
        };

        let indexed = match collated_db(&rows, COLLATED_INDEX_OURS)
            .and_then(|mut db| db.query(&query, &[]).map(|result| ids_of(&result)))
        {
            Ok(ids) => ids,
            Err(inlaysql::Error::Unsupported(_)) | Err(inlaysql::Error::Parse(_)) => {
                unsupported += 1;
                continue;
            }
            Err(error) => panic!("seed {seed}: InlaySQL failed on `{query}`: {error}"),
        };
        let plain = collated_db(&rows, &[])
            .and_then(|mut db| db.query(&query, &[]).map(|result| ids_of(&result)))
            .expect("the unindexed side answered a moment ago");
        assert_eq!(
            indexed, plain,
            "seed {seed}: `{query}` returned different rows with the index than without \
             it\nrows: {rows:?}"
        );

        let conn = collated_sqlite(&rows, COLLATED_INDEX_THEIRS).expect("SQLite fixture");
        let mut statement = conn.prepare(&query).expect("SQLite is the oracle");
        let theirs: Vec<i64> = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .expect("SQLite is the oracle")
            .collect::<rusqlite::Result<Vec<i64>>>()
            .expect("SQLite is the oracle");
        assert_eq!(
            indexed, theirs,
            "seed {seed}: `{query}` disagreed with SQLite\nrows: {rows:?}"
        );
    }

    assert!(
        unsupported * 4 < total,
        "{unsupported} of {total} generated collated predicates were unsupported: the generator \
         has drifted off the implemented dialect and is no longer testing much"
    );
}

/// The id column of a result set, which every collated query above projects.
fn ids_of(result: &inlaysql::ResultSet) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("id came back as {other:?}"),
        })
        .collect()
}

/// The shapes that fold or order by a collation rather than filtering on one:
/// `ORDER BY`, `GROUP BY`, `DISTINCT` and the aggregates that compare values.
///
/// # Why the values are compared folded and the counts are not
///
/// Under a collation that is not `BINARY`, `DISTINCT` and `GROUP BY` collapse
/// several *different* strings into one row and nothing says which of them
/// comes back. That is not a gap in this engine's determinism — **SQLite's own
/// answer changes with the access path**, which the first run of this test
/// found:
///
/// ```text
/// -- rt is declared COLLATE RTRIM; the rows hold 'grace' at id 7 and
/// -- 'GRACE' at ids 10 and 12.
/// SELECT DISTINCT rt COLLATE NOCASE FROM t ORDER BY 1;
///   with an index on rt: ..., GRACE, Z
///   without one:         ..., grace, Z
/// ```
///
/// So the *set of equivalence classes*, the *number* of them and the *order*
/// they come back in are all determined and are compared exactly; the
/// representative string is not, and comparing it would be asserting something
/// neither engine promises. The folded comparison still catches a wrong class
/// — two values grouped that should not have been, or one group where there
/// should be two — which is the whole question a collation answers.
#[test]
fn collated_orderings_and_groupings_agree_with_sqlite() {
    let total = rounds();
    for seed in 0..total {
        let mut rng = SeededRng::new(seed);
        let rows = generate_collated_rows(&mut rng);
        let column = collated_column(&mut rng);
        let suffix = collate_suffix(&mut rng);
        let direction = if rng.next_u64().is_multiple_of(2) {
            ""
        } else {
            " DESC"
        };
        // The collation the query resolves: an explicit `COLLATE` if it wrote
        // one, otherwise the column's own declaration.
        let effective = if suffix.is_empty() {
            match column {
                "nc" => " COLLATE NOCASE",
                "rt" => " COLLATE RTRIM",
                _ => "",
            }
        } else {
            suffix
        };

        // `exact` queries project row ids or counts, which are determined.
        // `folded` queries project a collated value, whose spelling is not.
        let exact = [
            // Ends in `id`, so the order is total and a tie cannot read as a
            // disagreement.
            format!("SELECT id FROM t ORDER BY {column}{suffix}{direction}, id"),
            format!(
                "SELECT COUNT(*) FROM (SELECT {column}{suffix} FROM t GROUP BY {column}{suffix})"
            ),
            format!("SELECT COUNT(DISTINCT {column}{suffix}) FROM t"),
            // The same three questions one level down, where the collation has
            // to have survived the derived table's synthetic column.
            format!("SELECT id FROM (SELECT id, {column}{suffix} AS s FROM t) d ORDER BY d.s, id"),
            format!(
                "SELECT COUNT(*) FROM (SELECT DISTINCT s FROM (SELECT {column}{suffix} AS s \
                 FROM t) d)"
            ),
            format!(
                "SELECT COUNT(*) FROM (SELECT s FROM (SELECT {column}{suffix} AS s FROM t) d \
                 GROUP BY s)"
            ),
        ];
        let folded = [
            format!("SELECT DISTINCT {column}{suffix} FROM t ORDER BY 1"),
            format!("SELECT MIN({column}{suffix}), MAX({column}{suffix}) FROM t"),
        ];

        let mut ours = collated_db(&rows, COLLATED_INDEX_OURS).expect("fixture");
        let mut plain = collated_db(&rows, &[]).expect("fixture");
        let conn = collated_sqlite(&rows, COLLATED_INDEX_THEIRS).expect("SQLite fixture");

        for (query, fold) in exact
            .iter()
            .map(|q| (q, ""))
            .chain(folded.iter().map(|q| (q, effective)))
        {
            let indexed = fold_cells(render_rows(&ours.query(query, &[]).expect("query")), fold);
            let unindexed = fold_cells(render_rows(&plain.query(query, &[]).expect("query")), fold);
            assert_eq!(
                indexed, unindexed,
                "seed {seed}: `{query}` differed with the index\nrows: {rows:?}"
            );

            let mut statement = conn.prepare(query).expect("SQLite is the oracle");
            let width = statement.column_count();
            let theirs: Vec<Vec<String>> = statement
                .query_map([], |row| {
                    Ok((0..width)
                        .map(|i| exact_sqlite(row.get_ref_unwrap(i)))
                        .collect())
                })
                .expect("SQLite is the oracle")
                .collect::<rusqlite::Result<Vec<Vec<String>>>>()
                .expect("SQLite is the oracle");
            assert_eq!(
                indexed,
                fold_cells(theirs, fold),
                "seed {seed}: `{query}` disagreed with SQLite\nrows: {rows:?}"
            );
        }
    }
}

/// Fold every rendered text cell under `collation`, so that two spellings of
/// one collated value compare equal. `""` leaves the cells alone.
///
/// The folds are the collations' own, written out here rather than reached for
/// through the engine: a harness that folded with the code under test could
/// not catch that code folding wrongly.
fn fold_cells(rows: Vec<Vec<String>>, collation: &str) -> Vec<Vec<String>> {
    if collation.is_empty() || collation == " COLLATE BINARY" {
        return rows;
    }
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell.strip_prefix("t:") {
                    Some(text) if collation == " COLLATE NOCASE" => {
                        format!("t:{}", text.to_ascii_lowercase())
                    }
                    Some(text) => format!("t:{}", text.trim_end_matches(' ')),
                    None => cell,
                })
                .collect()
        })
        .collect()
}

/// Every cell of a result set in the exact form both engines are compared in.
fn render_rows(result: &inlaysql::ResultSet) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|row| row.iter().map(exact_inlaysql).collect())
        .collect()
}

/// `UNIQUE` on a collated column, through the write path: 'Ada' and 'ADA' are
/// one key on a `NOCASE` column and two on a `BINARY` one, and every engine
/// has to accept and refuse the same sequence of writes.
#[test]
fn collated_unique_writes_agree_with_sqlite() {
    const DDL: &str = "CREATE TABLE u (id INTEGER PRIMARY KEY, nc TEXT COLLATE NOCASE UNIQUE, \
                       bin TEXT UNIQUE)";
    let total = rounds();
    for seed in 0..total {
        let mut rng = SeededRng::new(seed);
        let statements: Vec<String> = (0..WRITE_STATEMENTS)
            .map(|n| {
                let nc = collated_literal(&mut rng);
                let bin = collated_literal(&mut rng);
                match rng.next_u64() % 4 {
                    0 => format!("DELETE FROM u WHERE nc = '{nc}'"),
                    1 => format!("UPDATE u SET nc = '{nc}' WHERE id = {}", n + 1),
                    _ => format!("INSERT INTO u VALUES ({}, '{nc}', '{bin}')", n + 1),
                }
            })
            .collect();

        let mut ours = Database::open_in_memory().expect("open");
        ours.execute(DDL, &[]).expect("ddl");
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute(DDL, []).expect("ddl");

        for statement in &statements {
            let ok_here = ours.execute(statement, &[]).is_ok();
            let ok_there = conn.execute(statement, []).is_ok();
            assert_eq!(
                ok_here, ok_there,
                "seed {seed}: `{statement}` was accepted by one engine and refused by the other"
            );
        }

        let readback = "SELECT id, nc, bin FROM u ORDER BY id";
        let here = render_rows(&ours.query(readback, &[]).expect("readback"));
        let mut statement = conn.prepare(readback).expect("readback");
        let there: Vec<Vec<String>> = statement
            .query_map([], |row| {
                Ok((0..3)
                    .map(|i| exact_sqlite(row.get_ref_unwrap(i)))
                    .collect())
            })
            .expect("readback")
            .collect::<rusqlite::Result<Vec<Vec<String>>>>()
            .expect("readback");
        assert_eq!(
            here, there,
            "seed {seed}: the tables differ after the same accepted writes\n{statements:#?}"
        );
    }
}
