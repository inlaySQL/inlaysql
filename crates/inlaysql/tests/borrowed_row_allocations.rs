//! "A borrowing consumer allocates nothing per row" — counted, not asserted.
//!
//! `PERF.md`'s AHL-527 section ends by naming what a point read still spends:
//! `drop_in_place<ResultSet>` at 9% and `ValueRef::to_owned_value` at 2%, "the
//! public API's cost, not the statement's". AHL-535 added
//! [`Database::query_prepared_each_ref`] to remove it, and a claim of that
//! shape is worth exactly as much as its measurement — so this installs a
//! counting global allocator and counts `alloc` calls across a warm loop.
//!
//! # Why a whole test binary for it
//!
//! The allocator is process-wide and `cargo test` runs a file's tests on
//! several threads at once, so any other test allocating at the same time
//! lands in this number. One file, one measurement — the same reason
//! `crates/inlaysql/tests/index_memory_cost.rs` and
//! `crates/inlaysql-server/tests/streaming_memory.rs` are their own files.
//! Everything below therefore lives in one `#[test]`.
//!
//! # What is counted, and what a failure means
//!
//! Calls, not bytes: an allocation that is immediately freed costs the same
//! `malloc`/`free` pair whatever its size, and the pair is what the profile
//! showed. The count is taken over a *warm* loop — the handle has already run
//! each query once, so the scratch buffers, the page cache and the prepared
//! statement are all in the state a steady-state application has them in.
//!
//! A failure here is a regression, not a style violation: it means an owned
//! `String`, `Vec` or `ResultSet` came back onto the read path, which is
//! precisely the thing four `PERF.md` sections were spent removing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use inlaysql::{Database, Value};

/// Allocation calls since the process started.
static CALLS: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting.
///
/// Relaxed ordering: this binary is single-threaded by construction (see the
/// module note), so there is nothing for a stronger ordering to synchronise
/// with and the counter must not become the thing being measured.
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

// `realloc` and `alloc_zeroed` are deliberately left to their defaults, which
// are written in terms of `alloc` and `dealloc` above and so are already
// counted. `realloc` in particular matters: a `Vec` that grows instead of
// being reused would otherwise slip past this.
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
fn a_borrowing_consumer_allocates_nothing_per_row() {
    // A **file-backed** handle, because that is the one the claim is about and
    // the one every benchmark measures: it reads a row as a `RowBuf::Shared`
    // slice of a cached page, which is what there is to borrow from.
    // `Database::open_in_memory`'s `MemStorage` copies each row out of a
    // `BTreeMap` into an owned `Vec` before anything downstream sees it, so it
    // allocates twice per lookup whatever the API above it does — a property of
    // that backend, not of this path.
    let path = std::env::temp_dir().join(format!(
        "inlaysql-borrowed-allocations-{}-{}.inlay",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
    ));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .expect("create");
    let insert = db
        .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
        .expect("prepare insert");
    let payload = "x".repeat(64);
    db.begin().expect("begin");
    for id in 1..=2_000i64 {
        db.execute_prepared(
            &insert,
            &[Value::Integer(id), Value::Text(payload.clone().into())],
        )
        .expect("insert");
    }
    db.commit().expect("commit");

    let point = db
        .prepare("SELECT body FROM kv WHERE id = ?")
        .expect("prepare point");
    let scan = db
        .prepare("SELECT id, body FROM kv WHERE id >= ? AND id < ?")
        .expect("prepare scan");

    // Warm: the first execution of anything fills the scratch buffers, the
    // page cache and the row-id-to-page path. Steady state is what is claimed
    // and steady state is what is counted.
    // Every id the counted loop will ask for is asked for here first: a page
    // the cache has not seen yet allocates a buffer to hold it, once, and that
    // is a miss being paid for rather than a row.
    let lookups = 200i64;
    let mut sink = 0usize;
    for _ in 0..3 {
        for id in 1..=lookups {
            db.query_prepared_each_ref(&point, &[Value::Integer(id)], |row| {
                sink += row[0].as_str().map_or(0, str::len);
                Ok(())
            })
            .expect("warm point");
        }
        db.query_prepared_each_ref(&scan, &[Value::Integer(1), Value::Integer(51)], |row| {
            sink += row[1].as_str().map_or(0, str::len);
            Ok(())
        })
        .expect("warm scan");
    }

    // --- the point read: one row, one query, and it must cost nothing ---
    let borrowed_points = counted(|| {
        for id in 1..=lookups {
            let delivered = db
                .query_prepared_each_ref(&point, &[Value::Integer(id)], |row| {
                    sink += row[0].as_str().map_or(0, str::len);
                    Ok(())
                })
                .expect("point");
            assert_eq!(delivered, 1);
        }
    });
    let owned_points = counted(|| {
        for id in 1..=lookups {
            let rows = db
                .query_prepared(&point, &[Value::Integer(id)])
                .expect("point");
            sink += rows.rows[0][0].as_str().map_or(0, str::len);
        }
    });
    assert_eq!(
        borrowed_points, 0,
        "{lookups} borrowed point reads made {borrowed_points} allocations; \
         the owned API made {owned_points} over the same lookups"
    );

    // --- the range scan: allocations must not grow with the rows delivered ---
    //
    // The same query shape twice, reading the same rows and *delivering* one
    // of them the first time and forty the second. The scan underneath is
    // identical — a `WHERE` on a range is not an access path here, so both
    // walk the table in the same batches — which is exactly what makes the
    // comparison clean: everything except the number of rows that reach the
    // callback is held fixed, so any difference in the count is a per-row
    // cost and nothing else. Zero difference across a fortyfold change is
    // what says there is none.
    let short = counted(|| {
        db.query_prepared_each_ref(&scan, &[Value::Integer(1), Value::Integer(2)], |row| {
            sink += row[1].as_str().map_or(0, str::len);
            Ok(())
        })
        .expect("short");
    });
    let long = counted(|| {
        db.query_prepared_each_ref(&scan, &[Value::Integer(1), Value::Integer(41)], |row| {
            sink += row[1].as_str().map_or(0, str::len);
            Ok(())
        })
        .expect("long");
    });
    assert_eq!(
        short, long,
        "delivering 40 rows allocated {long} times against {short} for one row \
         off the same scan: the per-row cost is back"
    );

    // --- and the owned path is the control, not a straw man ---
    //
    // It has to be *measurably* more, or the two counts above would pass just
    // as well against an engine that never allocated in the first place and
    // the API would be buying nothing. At least one allocation per row is the
    // floor for it: `SELECT id, body` builds a `String` for every `body` it
    // returns, before the `Vec`s around them.
    let owned_long = counted(|| {
        let rows = db
            .query_prepared(&scan, &[Value::Integer(1), Value::Integer(41)])
            .expect("owned long");
        for row in &rows.rows {
            sink += row[1].as_str().map_or(0, str::len);
        }
    });
    assert!(
        owned_long >= 40,
        "the owned path made only {owned_long} allocations for 40 rows, so the \
         borrowed path's {long} is not evidence of anything"
    );

    // `sink` is read so the compiler cannot elide the column reads above,
    // which would make every count here a measurement of nothing.
    assert!(sink > 0);
    println!(
        "point reads: borrowed {borrowed_points}, owned {owned_points} over {lookups} lookups; \
         40-row scan: borrowed {long}, owned {owned_long}"
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}
