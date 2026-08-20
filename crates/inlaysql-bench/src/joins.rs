//! `users` × `posts` — the join an ORM emits for "show me this user's
//! posts" or "show me this post's author", InlaySQL against SQLite.
//!
//! # What is being compared
//!
//! The AHL-464 shape: `posts.user_id = users.id`, in both directions the
//! index nested-loop join rule (`Engine::join_probe`,
//! `crates/inlaysql-core/src/engine.rs`) actually takes:
//!
//! * **PK inner** — `FROM posts JOIN users ON posts.user_id = users.id`. The
//!   inner table is `users`, and the join key is `users.id`, its `INTEGER
//!   PRIMARY KEY`, so each outer row costs one tree descent.
//! * **Secondary-index inner** — `FROM users JOIN posts ON posts.user_id =
//!   users.id`. The inner table is `posts`, and the join key is
//!   `posts.user_id`, a scalar B-tree index (`CREATE INDEX posts_user_id ON
//!   posts (user_id)`), so each outer row costs an index entry-range read
//!   plus one descent per matched post. This is the exact query `PERF.md`
//!   names: "`SELECT ... FROM users JOIN posts ON posts.user_id = users.id`
//!   reads the posts one user has rather than the posts table."
//!
//! Each direction is measured with and without a `LIMIT`, because the probe
//! is a stage of the streaming pipeline (AHL-462) and a `LIMIT` on an
//! unindexed-order plan stops the outer scan as soon as it has enough rows —
//! `PERF.md` gives the counted case (`LIMIT 2` over a probed join fetches two
//! inner rows, not the whole inner table). Whether that shows up as a
//! wall-clock win over SQLite, which has no equivalent streaming guarantee, is
//! exactly what this row is for finding out rather than asserting.
//!
//! There is no unindexed/materialising row here — that fallback path (an
//! equality that is not on the inner table's key or a leading index column)
//! is exercised by `crates/inlaysql-core/tests/btree_index.rs` and
//! `tests/streaming.rs`, not by this harness; the point of this suite is the
//! access path the AHL-464 rule was built for, not the path it declines.
//!
//! # Making it a fair fight
//!
//! * **The same schema and the same index on both sides.** SQLite has no
//!   `USING` in `CREATE INDEX`; the InlaySQL side spells the same B-tree index
//!   `USING BTREE`, as `indexed` does.
//! * **Prepared statements on both sides**, parsed once outside the timed
//!   loop and re-run — neither query takes a bound parameter, so this is
//!   purely "parse and plan once, execute repeatedly," the same principle
//!   `points` and `indexed` apply where there is a parameter to bind.
//! * **Every user has exactly the same number of posts** (`POSTS_PER_USER`),
//!   assigned by round-robin rather than randomly, so the two directions
//!   answer the same total row count and neither engine gets a luckier key
//!   distribution than the other.
//! * **SQLite measured in both of the durability configurations `points`
//!   uses.** The join itself is read-only after the bulk load, so durability
//!   mostly affects the load, not the numbers below it — shown anyway, for
//!   the same reason `points` and `indexed` show both: so the row states its
//!   configuration instead of assuming one.

use std::path::Path;
use std::time::{Duration, Instant};

use inlaysql::{Database, Statement, Value};

use crate::points::{open_sqlite, remove_sqlite_files, Durability};
use crate::{percentiles, Config};

/// Posts per user, assigned round-robin so the distribution is exact and
/// reproducible without needing its own seeded RNG stream.
const POSTS_PER_USER: usize = 8;

/// One engine's result for one query shape.
struct Timing {
    label: String,
    elapsed: Duration,
    samples: Vec<Duration>,
}

impl Timing {
    fn per_second(&self, operations: usize) -> f64 {
        operations as f64 / self.elapsed.as_secs_f64().max(f64::EPSILON)
    }
}

/// One engine's four measured query shapes.
struct Shapes {
    pk_inner: Timing,
    pk_inner_limit: Timing,
    indexed_inner: Timing,
    indexed_inner_limit: Timing,
}

pub fn run(config: &Config, dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let users = config.rows;
    let posts = users * POSTS_PER_USER;
    let repeats = config.queries;
    let limit = config.limit;
    println!(
        "\n=== joins: {users} users, {posts} posts ({POSTS_PER_USER}/user), {repeats} runs \
         per query shape, LIMIT {limit} ==="
    );
    println!(
        "(PK inner: FROM posts JOIN users ON posts.user_id = users.id; secondary-index inner: \
         FROM users JOIN posts ON posts.user_id = users.id — AHL-464's shape)"
    );

    let payload = "x".repeat(config.payload);

    let inlay = inlaysql_joins(
        &dir.join("joins-inlaysql.inlay"),
        users,
        repeats,
        limit,
        &payload,
    )?;
    let sqlite_journal = sqlite_joins(
        &dir.join("joins-sqlite-journal.db"),
        users,
        repeats,
        limit,
        &payload,
        Durability::JournalFull,
    )?;
    let sqlite_wal = sqlite_joins(
        &dir.join("joins-sqlite-wal.db"),
        users,
        repeats,
        limit,
        &payload,
        Durability::WalNormal,
    )?;

    report(
        "join, PK inner (FROM posts JOIN users ON posts.user_id = users.id)",
        repeats,
        &[
            &inlay.pk_inner,
            &sqlite_journal.pk_inner,
            &sqlite_wal.pk_inner,
        ],
    );
    report(
        &format!(
            "join, PK inner, LIMIT {limit} (FROM posts JOIN users ON posts.user_id = users.id)"
        ),
        repeats,
        &[
            &inlay.pk_inner_limit,
            &sqlite_journal.pk_inner_limit,
            &sqlite_wal.pk_inner_limit,
        ],
    );
    report(
        "join, secondary-index inner (FROM users JOIN posts ON posts.user_id = users.id)",
        repeats,
        &[
            &inlay.indexed_inner,
            &sqlite_journal.indexed_inner,
            &sqlite_wal.indexed_inner,
        ],
    );
    report(
        &format!(
            "join, secondary-index inner, LIMIT {limit} (FROM users JOIN posts ON \
             posts.user_id = users.id)"
        ),
        repeats,
        &[
            &inlay.indexed_inner_limit,
            &sqlite_journal.indexed_inner_limit,
            &sqlite_wal.indexed_inner_limit,
        ],
    );
    Ok(())
}

fn report(workload: &str, operations: usize, timings: &[&Timing]) {
    println!("\n{workload}");
    println!(
        "{:<46} {:>12} {:>10} {:>10} {:>10}",
        "engine", "joins/s", "p50", "p95", "max"
    );
    for timing in timings {
        let (p50, p95, max) = percentiles(&timing.samples);
        println!(
            "{:<46} {:>12.0} {:>10} {:>10} {:>10}",
            timing.label,
            timing.per_second(operations),
            format!("{p50:.2?}"),
            format!("{p95:.2?}"),
            format!("{max:.2?}")
        );
    }
    if let [ours, theirs, ..] = timings {
        let ratio = theirs.elapsed.as_secs_f64() / ours.elapsed.as_secs_f64().max(f64::EPSILON);
        if ratio >= 1.0 {
            println!("{} is {ratio:.2}x faster than {}", ours.label, theirs.label);
        } else {
            println!(
                "{} is {:.2}x slower than {}",
                ours.label,
                1.0 / ratio,
                theirs.label
            );
        }
    }
}

/// Load `users` and `users * POSTS_PER_USER` `posts`, index `posts.user_id`,
/// then time the four query shapes.
///
/// The rows go in inside explicit transactions, as `points` and `indexed` do:
/// the load is setup, not the measurement.
fn inlaysql_joins(
    path: &Path,
    users: usize,
    repeats: usize,
    limit: usize,
    payload: &str,
) -> Result<Shapes, Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let mut db = Database::open(path)?;
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
        &[],
    )?;
    db.execute(
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
        &[],
    )?;

    let insert_user = db.prepare("INSERT INTO users (id, name) VALUES (?, ?)")?;
    let insert_post = db.prepare("INSERT INTO posts (id, user_id, title) VALUES (?, ?, ?)")?;
    db.begin()?;
    for id in 1..=users as i64 {
        let bound = [Value::Integer(id), Value::Text(format!("user{id}"))];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert_user, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert_user, &bound)?;
        }
    }
    let total_posts = users * POSTS_PER_USER;
    for post_id in 1..=total_posts as i64 {
        // Round-robin over users 1..=users, so every user ends up with exactly
        // POSTS_PER_USER posts.
        let user_id = 1 + ((post_id - 1) % users as i64);
        let bound = [
            Value::Integer(post_id),
            Value::Integer(user_id),
            Value::Text(payload.to_string()),
        ];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert_post, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert_post, &bound)?;
        }
    }
    db.commit()?;

    // Built after the rows: the harder path, same as `indexed`.
    db.execute(
        "CREATE INDEX posts_user_id ON posts (user_id) USING BTREE",
        &[],
    )?;

    let pk_inner = db
        .prepare("SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id")?;
    let pk_inner_limit = db.prepare(&format!(
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id LIMIT {limit}"
    ))?;
    let indexed_inner = db.prepare(
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id",
    )?;
    let indexed_inner_limit = db.prepare(&format!(
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id LIMIT {limit}"
    ))?;

    let time = |db: &mut Database,
                stmt: &Statement,
                expected: usize,
                label: &str|
     -> Result<Timing, Box<dyn std::error::Error>> {
        let mut samples = Vec::with_capacity(repeats);
        let started = Instant::now();
        for _ in 0..repeats {
            let at = Instant::now();
            let result = db.query_prepared(stmt, &[])?;
            debug_assert_eq!(
                result.rows.len(),
                expected,
                "row count changed between runs"
            );
            samples.push(at.elapsed());
        }
        Ok(Timing {
            label: label.to_string(),
            elapsed: started.elapsed(),
            samples,
        })
    };

    let shapes = Shapes {
        pk_inner: time(&mut db, &pk_inner, total_posts, "InlaySQL")?,
        pk_inner_limit: time(&mut db, &pk_inner_limit, limit.min(total_posts), "InlaySQL")?,
        indexed_inner: time(&mut db, &indexed_inner, total_posts, "InlaySQL")?,
        indexed_inner_limit: time(
            &mut db,
            &indexed_inner_limit,
            limit.min(total_posts),
            "InlaySQL",
        )?,
    };

    let _ = std::fs::remove_file(path);
    Ok(shapes)
}

fn sqlite_joins(
    path: &Path,
    users: usize,
    repeats: usize,
    limit: usize,
    payload: &str,
    durability: Durability,
) -> Result<Shapes, Box<dyn std::error::Error>> {
    remove_sqlite_files(path);
    let conn = open_sqlite(path, durability)?;
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", [])?;
    conn.execute(
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
        [],
    )?;

    conn.execute("BEGIN", [])?;
    {
        let mut insert_user = conn.prepare("INSERT INTO users (id, name) VALUES (?1, ?2)")?;
        for id in 1..=users as i64 {
            insert_user.execute(rusqlite::params![id, format!("user{id}")])?;
        }
        let mut insert_post =
            conn.prepare("INSERT INTO posts (id, user_id, title) VALUES (?1, ?2, ?3)")?;
        let total_posts = users * POSTS_PER_USER;
        for post_id in 1..=total_posts as i64 {
            let user_id = 1 + ((post_id - 1) % users as i64);
            insert_post.execute(rusqlite::params![post_id, user_id, payload])?;
        }
    }
    conn.execute("COMMIT", [])?;
    conn.execute("CREATE INDEX posts_user_id ON posts (user_id)", [])?;

    let total_posts = users * POSTS_PER_USER;
    let label = durability.label();

    let mut pk_inner = conn
        .prepare("SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id")?;
    let mut pk_inner_limit = conn.prepare(&format!(
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id LIMIT {limit}"
    ))?;
    let mut indexed_inner = conn.prepare(
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id",
    )?;
    let mut indexed_inner_limit = conn.prepare(&format!(
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id LIMIT {limit}"
    ))?;

    let time = |stmt: &mut rusqlite::Statement,
                expected: usize|
     -> Result<Timing, Box<dyn std::error::Error>> {
        let mut samples = Vec::with_capacity(repeats);
        let started = Instant::now();
        for _ in 0..repeats {
            let at = Instant::now();
            let rows_returned = stmt
                .query_map([], |row| {
                    let a: rusqlite::types::Value = row.get(0)?;
                    let b: rusqlite::types::Value = row.get(1)?;
                    Ok((a, b))
                })?
                .count();
            debug_assert_eq!(rows_returned, expected, "row count changed between runs");
            samples.push(at.elapsed());
        }
        Ok(Timing {
            label: format!("{label} (index)"),
            elapsed: started.elapsed(),
            samples,
        })
    };

    let shapes = Shapes {
        pk_inner: time(&mut pk_inner, total_posts)?,
        pk_inner_limit: time(&mut pk_inner_limit, limit.min(total_posts))?,
        indexed_inner: time(&mut indexed_inner, total_posts)?,
        indexed_inner_limit: time(&mut indexed_inner_limit, limit.min(total_posts))?,
    };

    drop(pk_inner);
    drop(pk_inner_limit);
    drop(indexed_inner);
    drop(indexed_inner_limit);
    drop(conn);
    remove_sqlite_files(path);
    Ok(shapes)
}
