//! Every join shape answers the same through all three row APIs.
//!
//! A join now has three consumers underneath, not two, and AHL-549 added the
//! third:
//!
//! * [`Database::query_prepared`] builds a `ResultSet`, which runs the join as
//!   an *iterator* (`NestedLoopJoin`'s `Iterator` impl) because there is no
//!   sink to push into;
//! * [`Database::query_prepared_each`] pushes owned rows, which runs
//!   `Engine::run_single_join_to` with an owned [`JoinSink`];
//! * [`Database::query_prepared_each_ref`] pushes *borrowed* rows, which for a
//!   probed inner side runs `exec::BorrowedJoin` — the outer row decoded out of
//!   its page into a reusable buffer, the probed inner row appended onto the
//!   end of it, the `ON` residual tested on those cells with
//!   `eval::evaluate_ref`, and nothing copied between the page and the caller.
//!
//! Which one runs is a *performance* decision no caller can see, and that
//! invisibility is the property under test — the standing rule from `PLAN.md`
//! §11, "every new fast path needs a test tying it to the slow path". So every
//! shape below runs all three over the same data and they must agree row for
//! row, cell for cell, **in order**.
//!
//! The shapes are chosen to reach each inner side the planner can pick (row-id
//! probe, index probe, hash, materialised) and each thing the borrowed operator
//! has to get right on its own: a residual `ON` beyond the probe key, a
//! `LEFT JOIN`'s padding, `LIMIT`/`OFFSET` counting, an expression in the
//! projection (which is *excluded* from the borrowed path and so must fall back
//! correctly), a repeated column, and a `WHERE` over both sides (which keeps
//! the general pipeline entirely).

use inlaysql::{Database, Result, Value};

/// One query's answer, as owned rows.
type Rows = Vec<Vec<Value>>;

/// The same query through all three APIs.
fn three_ways(db: &mut Database, sql: &str, params: &[Value]) -> Result<(Rows, Rows, Rows)> {
    let statement = db.prepare(sql)?;
    let owned = db.query_prepared(&statement, params)?.rows;

    let mut pushed: Rows = Vec::new();
    let delivered = db.query_prepared_each(&statement, params, |row| {
        pushed.push(row.to_vec());
        Ok(())
    })?;
    assert_eq!(
        delivered,
        pushed.len(),
        "`{sql}` reported a row count its owned callback did not see"
    );

    let mut borrowed: Rows = Vec::new();
    let delivered = db.query_prepared_each_ref(&statement, params, |row| {
        borrowed.push(row.iter().map(|cell| cell.to_owned_value()).collect());
        Ok(())
    })?;
    assert_eq!(
        delivered,
        borrowed.len(),
        "`{sql}` reported a row count its borrowing callback did not see"
    );
    Ok((owned, pushed, borrowed))
}

/// Assert all three APIs agree on `sql`.
fn agree(db: &mut Database, sql: &str, params: &[Value]) {
    let (owned, pushed, borrowed) =
        three_ways(db, sql, params).unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
    assert_eq!(
        owned, pushed,
        "`{sql}` disagreed between the ResultSet and the owned callback"
    );
    assert_eq!(
        owned, borrowed,
        "`{sql}` disagreed between the ResultSet and the borrowing callback"
    );
}

/// `users` and `posts` — the published benchmark's schema, plus the columns a
/// wrong answer would show up in.
///
/// `posts.user_id` is indexed, so `users JOIN posts` is an index probe and
/// `posts JOIN users` is a row-id probe: the two shapes `BENCHMARK.md`
/// publishes, and between them both `ProbeKind`s. `notes.tag` is *not* indexed
/// and is not a key, so joining on it reaches the hash and materialised sides
/// instead.
///
/// A file-backed handle, because that is the one whose rows are a slice of a
/// cached page — the thing there is to borrow from. (`open_in_memory` answers
/// identically; `borrowed_rows.rs` covers it.)
struct Fixture {
    db: Database,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One database file per fixture. `cargo test` runs this file's tests on
/// several threads at once, and a wall-clock name alone collided.
static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn fixture() -> Fixture {
    let path = std::env::temp_dir().join(format!(
        "inlaysql-joins-borrowed-{}-{}.inlay",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path).expect("open");
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score REAL)",
        &[],
    )
    .expect("create users");
    db.execute(
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT, raw BLOB)",
        &[],
    )
    .expect("create posts");
    db.execute("CREATE TABLE notes (id INTEGER PRIMARY KEY, tag TEXT)", &[])
        .expect("create notes");

    let insert_user = db
        .prepare("INSERT INTO users (id, name, score) VALUES (?, ?, ?)")
        .expect("prepare user");
    let insert_post = db
        .prepare("INSERT INTO posts (id, user_id, title, raw) VALUES (?, ?, ?, ?)")
        .expect("prepare post");
    let insert_note = db
        .prepare("INSERT INTO notes (id, tag) VALUES (?, ?)")
        .expect("prepare note");
    db.begin().expect("begin");
    for id in 1..=240i64 {
        // User 7 is all the NULLs there are: a borrowed cell that is `Null`
        // because the column *is* null must read the same as one that is
        // `Null` because a `LEFT JOIN` padded it.
        let (name, score) = if id == 7 {
            (Value::Null, Value::Null)
        } else {
            (
                Value::Text(format!("user{id:03}").into()),
                Value::Real(0.25 * id as f64),
            )
        };
        db.execute_prepared(&insert_user, &[Value::Integer(id), name, score])
            .expect("insert user");
        db.execute_prepared(
            &insert_note,
            &[
                Value::Integer(id),
                Value::Text(format!("tag{}", id % 5).into()),
            ],
        )
        .expect("insert note");
    }
    for post_id in 1..=960i64 {
        // Round-robin over the first two hundred users, so users 201..=240 have no
        // posts at all — the rows a `LEFT JOIN` has to pad and an `INNER JOIN`
        // has to drop.
        let user_id = 1 + ((post_id - 1) % 200);
        // Every fifth post has a `NULL` key, which matches nothing including
        // another `NULL`.
        let key = if post_id % 5 == 0 {
            Value::Null
        } else {
            Value::Integer(user_id)
        };
        db.execute_prepared(
            &insert_post,
            &[
                Value::Integer(post_id),
                key,
                Value::Text("payload ".repeat((post_id % 7) as usize + 1).into()),
                Value::Blob(vec![post_id as u8; (post_id % 11) as usize + 1]),
            ],
        )
        .expect("insert post");
    }
    db.commit().expect("commit");
    db.execute(
        "CREATE INDEX posts_user_id ON posts (user_id) USING BTREE",
        &[],
    )
    .expect("create index");
    // Statistics, so the cost model is live rather than falling back to its
    // shape rules — which is what lets the hash side be reached at all.
    db.execute("ANALYZE", &[]).expect("analyze");
    Fixture { db, path }
}

/// The two published shapes and their `LIMIT`ed forms: a row-id probe and an
/// index probe, which is what the borrowed operator is built for.
#[test]
fn the_probed_join_shapes_answer_the_same_three_ways() {
    let mut fixture = fixture();
    let db = &mut fixture.db;
    let shapes = [
        // PK inner: `users.id` is the INTEGER PRIMARY KEY, so one descent.
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id",
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id LIMIT 10",
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id LIMIT 1",
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id LIMIT 0",
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id \
         LIMIT 7 OFFSET 5",
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id \
         LIMIT 5 OFFSET 1000",
        // Secondary-index inner: `posts.user_id` is a scalar B-tree index, so
        // an entry range per outer row, sorted back into row-id order.
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id",
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id LIMIT 10",
        "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id \
         LIMIT 3 OFFSET 9",
        // Every storage class out of both sides, including the BLOB and the
        // REAL, so a wrong slice reads as a wrong byte rather than comparing
        // equal.
        "SELECT users.id, users.name, users.score, posts.id, posts.title, posts.raw \
         FROM posts JOIN users ON posts.user_id = users.id LIMIT 12",
        // A repeated column: the borrowed projection selects cells out of a
        // buffer it re-reads for the next candidate, so a moved cell would read
        // `NULL` the second time.
        "SELECT users.name, users.name, posts.title, posts.title \
         FROM posts JOIN users ON posts.user_id = users.id LIMIT 6",
        // Only the outer side projected, and only the inner side projected.
        "SELECT posts.id FROM posts JOIN users ON posts.user_id = users.id LIMIT 6",
        "SELECT users.name FROM posts JOIN users ON posts.user_id = users.id LIMIT 6",
    ];
    for sql in shapes {
        agree(db, sql, &[]);
    }
}

/// A `LEFT JOIN` pads an unmatched outer row, and the padding has to be the
/// inner table's declared width whichever operator produced it.
#[test]
fn a_left_join_pads_the_same_three_ways() {
    let mut fixture = fixture();
    let db = &mut fixture.db;
    let shapes = [
        "SELECT posts.id, users.name FROM posts LEFT JOIN users ON posts.user_id = users.id",
        "SELECT posts.id, users.name FROM posts LEFT JOIN users ON posts.user_id = users.id \
         LIMIT 10",
        // Users 201..=240 have no posts, so this is the padding case on the
        // index-probe side.
        "SELECT users.id, users.name, posts.id, posts.title \
         FROM users LEFT JOIN posts ON posts.user_id = users.id",
        "SELECT users.id, posts.title FROM users LEFT JOIN posts ON posts.user_id = users.id \
         LIMIT 40 OFFSET 60",
        // A residual `ON` that rejects every candidate for some outer rows,
        // which is the only way an outer row with candidates still pads.
        "SELECT users.id, posts.id FROM users LEFT JOIN posts \
         ON posts.user_id = users.id AND posts.id > 900",
    ];
    for sql in shapes {
        agree(db, sql, &[]);
    }
}

/// An `ON` with more in it than the probe key. The probe narrows to candidates
/// and the residual decides; on the borrowed path that residual is evaluated
/// against borrowed cells by `eval::evaluate_ref`, and it has to reach the same
/// three-valued verdict the owned `eval::evaluate` does.
#[test]
fn a_residual_on_filters_the_same_three_ways() {
    let mut fixture = fixture();
    let db = &mut fixture.db;
    let shapes = [
        "SELECT posts.id, users.name FROM posts JOIN users \
         ON posts.user_id = users.id AND users.id > 10",
        "SELECT posts.id, users.name FROM posts JOIN users \
         ON posts.user_id = users.id AND users.name IS NOT NULL LIMIT 20",
        "SELECT posts.id, users.name FROM posts JOIN users \
         ON posts.user_id = users.id AND users.score > 2.0 LIMIT 8",
        // A residual outside `evaluate_ref`'s borrowed sublanguage, so the
        // borrowed path materialises the row for it and must still agree.
        "SELECT posts.id, users.name FROM posts JOIN users \
         ON posts.user_id = users.id AND users.name LIKE 'user01%'",
        "SELECT posts.id, users.name FROM posts JOIN users \
         ON posts.user_id = users.id AND users.id IN (2, 4, 6, 8)",
        "SELECT posts.id, users.name FROM posts JOIN users \
         ON posts.user_id = users.id AND (users.id % 3) = 0 LIMIT 15",
        // An `ON` that is never true, and one that is always true.
        "SELECT posts.id, users.name FROM posts JOIN users \
         ON posts.user_id = users.id AND users.id < 0",
        "SELECT posts.id, users.id FROM posts JOIN users ON 1 = 1 LIMIT 30",
    ];
    for sql in shapes {
        agree(db, sql, &[]);
    }
}

/// The inner sides the borrowed operator deliberately does *not* take: a hash
/// build and a materialised replay. They keep the owned nested loop, and this
/// is where a routing mistake would show as a wrong answer rather than as a
/// silent slowdown.
#[test]
fn the_hash_and_materialised_inner_sides_answer_the_same_three_ways() {
    let mut fixture = fixture();
    let db = &mut fixture.db;
    let shapes = [
        // `notes.tag` is neither a key nor indexed, so an equi-join on it is a
        // hash build or a materialised replay depending on what the costs say.
        "SELECT users.name, notes.tag FROM users JOIN notes ON notes.id = users.id",
        "SELECT users.name, notes.tag FROM users JOIN notes ON notes.tag = users.name LIMIT 10",
        "SELECT users.id, notes.id FROM users LEFT JOIN notes ON notes.tag = users.name",
        // A non-equi `ON` cannot be probed or hashed at all: the inner side is
        // materialised and replayed for every outer row.
        "SELECT users.id, notes.id FROM users JOIN notes ON notes.id > users.id LIMIT 25",
        "SELECT users.id, notes.id FROM users LEFT JOIN notes ON notes.id > users.id + 20",
        // A cross join: no `ON` at all.
        "SELECT users.id, notes.id FROM users JOIN notes LIMIT 40 OFFSET 10",
    ];
    for sql in shapes {
        agree(db, sql, &[]);
    }
}

/// The shapes that keep the general pipeline: a `WHERE` over both sides, an
/// expression in the projection, and the blocking operators. Each one names a
/// condition in `borrowable_join` or in `run_single_join_to`'s own choice; if
/// one is dropped, a borrowed sink would reach an operator that cannot serve it
/// and this is where that shows up.
#[test]
fn the_shapes_that_keep_the_general_pipeline_answer_the_same_three_ways() {
    let mut fixture = fixture();
    let db = &mut fixture.db;
    let shapes = [
        // A `WHERE` over both sides — `run_single_join_to` is only reached
        // without one.
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id \
         WHERE users.id > 5 AND posts.id < 50",
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id \
         WHERE users.name IS NULL",
        "SELECT posts.id, users.name FROM posts LEFT JOIN users ON posts.user_id = users.id \
         WHERE users.name IS NULL LIMIT 5",
        // An expression in the projection: the borrowed operator refuses it,
        // because the value it produces is in no page.
        "SELECT posts.id + 1, upper(users.name) FROM posts JOIN users \
         ON posts.user_id = users.id LIMIT 10",
        "SELECT length(posts.title), users.id * 2 FROM posts JOIN users \
         ON posts.user_id = users.id LIMIT 10",
        "SELECT users.name || '!' FROM posts JOIN users ON posts.user_id = users.id LIMIT 4",
        // The blocking operators, which have to see the last input row before
        // the first output row.
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id \
         ORDER BY users.name DESC, posts.id LIMIT 10",
        "SELECT DISTINCT users.name FROM posts JOIN users ON posts.user_id = users.id",
        "SELECT users.id, COUNT(*) FROM posts JOIN users ON posts.user_id = users.id \
         GROUP BY users.id",
        "SELECT COUNT(*) FROM posts JOIN users ON posts.user_id = users.id",
        // A derived table on either side.
        "SELECT d.n, users.name FROM (SELECT posts.user_id AS n FROM posts) AS d \
         JOIN users ON d.n = users.id LIMIT 10",
    ];
    for sql in shapes {
        agree(db, sql, &[]);
    }
}

/// A bound `LIMIT`/`OFFSET` is resolved at execution, not at planning, so all
/// three APIs have to resolve it the same way — including the pair that asks
/// for nothing and the one that asks past the end.
#[test]
fn a_bound_limit_and_offset_land_the_same_way_on_every_join_api() {
    let mut fixture = fixture();
    let db = &mut fixture.db;
    for (limit, offset) in [
        (0i64, 0i64),
        (1, 0),
        (10, 0),
        (5, 7),
        (1_000, 3),
        (2, 5_000),
    ] {
        for sql in [
            "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id \
             LIMIT ? OFFSET ?",
            "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id \
             LIMIT ? OFFSET ?",
            "SELECT posts.id, users.name FROM posts LEFT JOIN users ON posts.user_id = users.id \
             LIMIT ? OFFSET ?",
        ] {
            agree(db, sql, &[Value::Integer(limit), Value::Integer(offset)]);
        }
    }
}

/// A callback that stops early leaves the same rows behind whichever API it
/// stopped, and a callback that *fails* reports its own error rather than a
/// failed statement — the borrowed join loop returns through the same seam the
/// owned one does.
#[test]
fn a_failing_callback_stops_a_borrowed_join() {
    let mut fixture = fixture();
    let db = &mut fixture.db;
    let join = db
        .prepare("SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id")
        .expect("prepare");
    let mut seen = 0usize;
    let outcome = db.query_prepared_each_ref(&join, &[], |_| {
        seen += 1;
        if seen == 4 {
            Err(inlaysql::Error::Unsupported("stop".to_string()))
        } else {
            Ok(())
        }
    });
    assert!(matches!(outcome, Err(inlaysql::Error::Unsupported(_))));
    assert_eq!(seen, 4, "the join carried on after the callback failed");

    // And the handle is still usable: the buffers the loop lends out per row
    // were dropped rather than left in a borrowed state.
    agree(
        db,
        "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id LIMIT 3",
        &[],
    );
}

/// The borrowed cells really are the row's bytes on both sides of the join: the
/// outer row's `TEXT`/`BLOB` come out of the driving scan's page and the inner
/// row's out of the probed page, and a slice that were wrong would read as
/// garbage rather than fail to compile.
#[test]
fn a_joined_borrowed_cell_reads_the_stored_bytes() {
    let mut fixture = fixture();
    let db = &mut fixture.db;
    let join = db
        .prepare(
            "SELECT posts.raw, users.name FROM posts JOIN users ON posts.user_id = users.id \
             LIMIT 1",
        )
        .expect("prepare");
    let mut seen = 0usize;
    db.query_prepared_each_ref(&join, &[], |row| {
        // Post 1 has `user_id` 1 and a two-byte blob of `1`s; user 1 is
        // `user001`.
        assert_eq!(row[0].as_blob(), Some([1u8, 1].as_slice()));
        assert_eq!(row[1].as_str(), Some("user001"));
        seen += 1;
        Ok(())
    })
    .expect("query");
    assert_eq!(seen, 1);
}
