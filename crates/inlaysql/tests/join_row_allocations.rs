//! "A probed join allocates nothing per outer row" — counted, not asserted.
//!
//! The join twin of `crates/inlaysql/tests/borrowed_row_allocations.rs`, and
//! for the same reason: AHL-549 claims that the `LIMIT 10` shapes
//! `crates/inlaysql-bench/src/joins.rs` publishes stopped decoding the probed
//! inner row into an owned `Vec<Value>` and copying its `TEXT` cell a second
//! time at projection. A claim of that shape is worth exactly as much as its
//! measurement, so this installs a counting global allocator and counts
//! `alloc` calls across a warm loop.
//!
//! # Why a whole test binary for it
//!
//! The allocator is process-wide and `cargo test` runs a file's tests on
//! several threads at once, so any other test allocating at the same moment
//! lands in this number. One file, one measurement — the same reason
//! `borrowed_row_allocations.rs` is its own file. Everything below therefore
//! lives in one `#[test]`.
//!
//! # What a failure means
//!
//! That an owned `String` or `Vec` came back onto the join's per-row path.
//! The number is not a style preference: `PERF.md`'s AHL-549 profile put
//! `JoinInner::prepare` at 54% of the query and the allocator at 10%, and both
//! of those were paid per candidate row.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use inlaysql::{Database, Value};

/// Allocation calls since the process started.
static CALLS: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting. Relaxed ordering: this binary is
/// single-threaded by construction, so there is nothing for a stronger
/// ordering to synchronise with and the counter must not become the thing
/// being measured.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `layout` is forwarded unchanged to the allocator this one
        // wraps, which is the contract this method was called under.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `alloc` above with this same `layout`,
        // which this method's own contract already requires.
        unsafe { System.dealloc(pointer, layout) }
    }
}

// `realloc` and `alloc_zeroed` keep their defaults, which are written in terms
// of `alloc` and `dealloc` above and so are already counted — `realloc` in
// particular, because a buffer that grows instead of being reused would
// otherwise slip past this.
#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn calls() -> usize {
    CALLS.load(Ordering::Relaxed)
}

/// Allocation calls made by `run`.
fn counted(run: impl FnOnce()) -> usize {
    let before = calls();
    run();
    calls() - before
}

#[test]
fn a_probed_join_allocates_nothing_per_row() {
    // A **file-backed** handle, because that is the one the benchmark
    // measures: a row is a `RowBuf::Shared` slice of a cached page, which is
    // what there is to borrow from.
    let path = std::env::temp_dir().join(format!(
        "inlaysql-join-allocations-{}-{}.inlay",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
    ));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path).expect("open");
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
        &[],
    )
    .expect("create users");
    db.execute(
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
        &[],
    )
    .expect("create posts");

    let users = 500i64;
    let posts_per_user = 8i64;
    let insert_user = db
        .prepare("INSERT INTO users (id, name) VALUES (?, ?)")
        .expect("prepare user");
    let insert_post = db
        .prepare("INSERT INTO posts (id, user_id, title) VALUES (?, ?, ?)")
        .expect("prepare post");
    let payload = "x".repeat(64);
    db.begin().expect("begin");
    for id in 1..=users {
        db.execute_prepared(
            &insert_user,
            &[Value::Integer(id), Value::Text(format!("user{id}").into())],
        )
        .expect("insert user");
    }
    for post_id in 1..=(users * posts_per_user) {
        let user_id = 1 + ((post_id - 1) % users);
        let bound = [
            Value::Integer(post_id),
            Value::Integer(user_id),
            Value::Text(payload.clone().into()),
        ];
        // The write-ahead log bounds one transaction, exactly as
        // `crates/inlaysql-bench/src/joins.rs` handles it: commit and start
        // another rather than shrinking the fixture.
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert_post, &bound) {
            db.commit().expect("commit");
            db.begin().expect("begin");
            db.execute_prepared(&insert_post, &bound)
                .expect("insert post");
        }
    }
    db.commit().expect("commit");
    db.execute(
        "CREATE INDEX posts_user_id ON posts (user_id) USING BTREE",
        &[],
    )
    .expect("create index");
    db.execute("ANALYZE", &[]).expect("analyze");

    // The two shapes `BENCHMARK.md` publishes, verbatim except that the
    // `LIMIT` is bound rather than literal: the inner side is `users`'s
    // `INTEGER PRIMARY KEY` in the first and `posts`'s secondary B-tree index
    // in the second, so between them they cover both `ProbeKind`s.
    //
    // **A bound `LIMIT` is what makes the comparison clean.** One prepared
    // statement, one plan, one access path, one scan batch — the only thing
    // that changes between the two counted runs below is how many rows reach
    // the callback. Any difference in the allocation count is therefore a
    // per-row cost and nothing else, which is the same control
    // `borrowed_row_allocations.rs` uses for its scan.
    let pk_inner = db
        .prepare(
            "SELECT posts.id, users.name FROM posts JOIN users ON posts.user_id = users.id \
             LIMIT ?",
        )
        .expect("prepare pk join");
    let indexed_inner = db
        .prepare(
            "SELECT users.name, posts.title FROM users JOIN posts ON posts.user_id = users.id \
             LIMIT ?",
        )
        .expect("prepare indexed join");

    let few = [Value::Integer(1)];
    let many = [Value::Integer(40)];
    let mut sink = 0usize;

    // Warm: the first execution of anything fills the scratch buffers, the
    // page cache and the descent path. Steady state is what is claimed and
    // steady state is what is counted.
    for _ in 0..5 {
        for stmt in [&pk_inner, &indexed_inner] {
            for bound in [&few, &many] {
                db.query_prepared_each_ref(stmt, bound, |row| {
                    sink += row
                        .iter()
                        .filter_map(|cell| cell.as_str())
                        .map(str::len)
                        .sum::<usize>();
                    Ok(())
                })
                .expect("warm");
            }
        }
    }

    let count_join =
        |db: &mut Database, stmt: &inlaysql::Statement, bound: &[Value], rows: usize| {
            let mut seen = 0usize;
            let mut bytes = 0usize;
            let calls = counted(|| {
                let delivered = db
                    .query_prepared_each_ref(stmt, bound, |row| {
                        bytes += row[1].as_str().map_or(0, str::len);
                        seen += 1;
                        Ok(())
                    })
                    .expect("join");
                assert_eq!(delivered, rows);
            });
            assert_eq!(seen, rows);
            (calls, bytes)
        };

    // One row against forty, off one prepared statement and one plan.
    //
    // Forty rather than four hundred because the cost model picks the *hash*
    // join above roughly that many — `join_strategy` costs a probe per outer
    // row against one build — and the hash side is not what this measures.
    // Both counted runs below are therefore the probed nested loop, which is
    // the operator AHL-549 changed.
    let (pk_few, a) = count_join(&mut db, &pk_inner, &few, 1);
    let (pk_many, b) = count_join(&mut db, &pk_inner, &many, 40);
    sink += a + b;
    assert!(
        (pk_many - pk_few) * 4 < 39,
        "the PK-inner probe cost {pk_many} allocations for 40 rows against {pk_few} for \
         one row off the same scan — {} more for 39 more rows. Before AHL-549 that \
         difference was 160; anything approaching one per row is the per-row cost back",
        pk_many - pk_few
    );

    let (indexed_few, a) = count_join(&mut db, &indexed_inner, &few, 1);
    let (indexed_many, b) = count_join(&mut db, &indexed_inner, &many, 40);
    sink += a + b;
    assert!(
        (indexed_many - indexed_few) * 4 < 39,
        "the secondary-index probe cost {indexed_many} allocations for 40 rows against \
         {indexed_few} for one row off the same scan — {} more for 39 more rows. Before \
         AHL-549 that difference was 100",
        indexed_many - indexed_few
    );

    // What is left in those two counts is the driving *scan* — one batch
    // buffer and one bound key per leaf it crosses, `O(rows / leaf)` and not
    // `O(rows)` — plus the per-*query* plan and column-name allocations
    // `PERF.md`'s AHL-532 section measured at ~5% of a `LIMIT 10` join. None
    // of it is the join operator, which is what the two assertions above are
    // for and what a profile of this binary confirms frame by frame.

    // The owned path is the control, not a straw man: it has to be
    // *measurably* more, or the two bounds above would pass just as well
    // against an engine that never allocated in the first place. One `String`
    // per returned `TEXT` cell is its floor, before the `Vec`s around them.
    let pk_owned = counted(|| {
        let rows = db.query_prepared(&pk_inner, &many).expect("pk join owned");
        for row in &rows.rows {
            sink += row[1].as_str().map_or(0, str::len);
        }
    });
    assert!(
        pk_owned >= 40,
        "the owned path made only {pk_owned} allocations for 40 joined rows, so the \
         borrowed path's {pk_many} against {pk_few} is not evidence of anything"
    );

    // `sink` is read so the compiler cannot elide the column reads above,
    // which would make every count here a measurement of nothing.
    assert!(sink > 0);
    println!(
        "PK inner: {pk_few} allocations for 1 row, {pk_many} for 40, {pk_owned} owned for 40; \
         secondary-index inner: {indexed_few} for 1 row, {indexed_many} for 40"
    );

    drop(db);
    let _ = std::fs::remove_file(&path);
}
