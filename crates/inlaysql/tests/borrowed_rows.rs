//! The borrowing result API answers exactly what the owned one does.
//!
//! [`Database::query_prepared_each_ref`] (AHL-535) exists to let a consumer
//! read a row without the engine allocating an owned copy of it first. It has
//! two implementations underneath — a borrowing pipeline for a single stored
//! table projected as bare columns, and a fallback that borrows out of the rows
//! the ordinary owned pipeline built — and which one runs is a *performance*
//! decision the caller cannot see.
//!
//! That invisibility is the property under test, and it is the standing rule
//! from `PLAN.md` §11: "every new fast path needs a test tying it to the slow
//! path", four bugs deep. So every shape below runs both APIs over the same
//! data and the same parameters and must agree row for row, cell for cell,
//! *in order* — including the shapes that deliberately fall back, because the
//! condition list that routes them is the thing most likely to drift.
//!
//! The columns are chosen so a wrong answer shows up rather than compares
//! equal: `TEXT` and `BLOB` are the two that borrow from the page (so a
//! lifetime mistake is a wrong byte, not a compile error), `NULL` and `REAL`
//! and a negative `INTEGER` cover the cells that are copied whole, and the
//! duplicate-column projections exercise the move-versus-clone choice
//! `run_borrowed_select` makes.

use inlaysql::{Database, Result, Value};

/// One query's answer, as the owned rows both APIs have to agree on.
type Rows = Vec<Vec<Value>>;

/// Both APIs' answers for one query, as owned rows.
fn both(db: &mut Database, sql: &str, params: &[Value]) -> Result<(Rows, Rows)> {
    let statement = db.prepare(sql)?;
    let owned = db.query_prepared(&statement, params)?.rows;

    let mut borrowed = Vec::new();
    let delivered = db.query_prepared_each_ref(&statement, params, |row| {
        borrowed.push(row.iter().map(|cell| cell.to_owned_value()).collect());
        Ok(())
    })?;
    assert_eq!(
        delivered,
        borrowed.len(),
        "`{sql}` reported a row count its callback did not see"
    );
    Ok((owned, borrowed))
}

/// Assert both APIs agree on `sql`, and report which pipeline ran.
fn agree(db: &mut Database, sql: &str, params: &[Value]) {
    let (owned, borrowed) = both(db, sql, params).unwrap_or_else(|error| {
        panic!("`{sql}` failed: {error}");
    });
    assert_eq!(owned, borrowed, "`{sql}` disagreed between the two APIs");
}

/// A table holding one of every storage class, plus a second table to join to
/// and a `WITHOUT ROWID` one, all with the same 12 ids so a shape can be
/// written against any of them.
fn fixture() -> Database {
    let mut db = Database::open_in_memory().expect("open");
    db.execute(
        "CREATE TABLE kv (id INTEGER PRIMARY KEY, email TEXT, body TEXT, weight REAL, raw BLOB)",
        &[],
    )
    .expect("create kv");
    db.execute("CREATE TABLE tags (kv_id INTEGER, tag TEXT)", &[])
        .expect("create tags");
    db.execute(
        "CREATE TABLE codes (code TEXT PRIMARY KEY, label TEXT) WITHOUT ROWID",
        &[],
    )
    .expect("create codes");

    let insert = db
        .prepare("INSERT INTO kv (id, email, body, weight, raw) VALUES (?, ?, ?, ?, ?)")
        .expect("prepare insert");
    let tag = db
        .prepare("INSERT INTO tags (kv_id, tag) VALUES (?, ?)")
        .expect("prepare tag insert");
    let code = db
        .prepare("INSERT INTO codes (code, label) VALUES (?, ?)")
        .expect("prepare code insert");
    for id in 1..=12i64 {
        // Row 7 is all the NULLs there are: a borrowed cell that is `Null`
        // because the column *is* null must read the same as one that is
        // `Null` because the column mask skipped it.
        let (email, body, weight, raw) = if id == 7 {
            (Value::Null, Value::Null, Value::Null, Value::Null)
        } else {
            (
                Value::Text(format!("user{id:04}@example.com").into()),
                Value::Text("payload ".repeat(id as usize).into()),
                Value::Real(-0.5 * id as f64),
                Value::Blob(vec![id as u8; id as usize]),
            )
        };
        db.execute_prepared(&insert, &[Value::Integer(id), email, body, weight, raw])
            .expect("insert");
        db.execute_prepared(
            &tag,
            &[
                Value::Integer(id),
                Value::Text(format!("t{}", id % 3).into()),
            ],
        )
        .expect("insert tag");
        db.execute_prepared(
            &code,
            &[
                Value::Text(format!("c{id:03}").into()),
                Value::Text(format!("label {id}").into()),
            ],
        )
        .expect("insert code");
    }
    db.execute("CREATE INDEX kv_email ON kv (email) USING BTREE", &[])
        .expect("create index");
    db
}

/// The published shapes, first: `bin/profile`'s `points` and `indexed-range`
/// are the two this API was built for, and they are the two whose harnesses
/// now call it.
#[test]
fn the_borrowing_path_ties_the_owned_one_on_the_profiled_shapes() {
    let mut db = fixture();
    for id in [1i64, 7, 12, 13] {
        agree(
            &mut db,
            "SELECT body FROM kv WHERE id = ?",
            &[Value::Integer(id)],
        );
    }
    agree(
        &mut db,
        "SELECT id, body FROM kv WHERE email >= ? AND email < ?",
        &[
            Value::Text("user0003@example.com".into()),
            Value::Text("user0009@example.com".into()),
        ],
    );
}

/// Everything the borrowing pipeline itself claims to handle: bare columns off
/// one stored table, under a `WHERE`, a `LIMIT` and an `OFFSET`, in every
/// combination that has an edge in it.
#[test]
fn the_borrowing_path_ties_the_owned_one_row_for_row() {
    let mut db = fixture();
    let shapes = [
        "SELECT * FROM kv",
        "SELECT id FROM kv",
        "SELECT raw, weight, body, email, id FROM kv",
        // The move-versus-clone choice: a repeated column may not be moved out
        // of the decoded row, because the second copy would read `NULL`.
        "SELECT id, id, body, body FROM kv",
        "SELECT id, body FROM kv WHERE weight < -3.0",
        "SELECT id FROM kv WHERE body IS NULL",
        "SELECT id FROM kv WHERE email IS NOT NULL",
        "SELECT id FROM kv WHERE id > 100",
        "SELECT id, email FROM kv LIMIT 0",
        "SELECT id, email FROM kv LIMIT 1",
        "SELECT id, email FROM kv LIMIT 5",
        "SELECT id, email FROM kv LIMIT 100",
        "SELECT id, email FROM kv LIMIT 3 OFFSET 4",
        "SELECT id, email FROM kv LIMIT 3 OFFSET 11",
        "SELECT id, email FROM kv LIMIT 3 OFFSET 100",
        "SELECT id, email FROM kv WHERE weight < -3.0 LIMIT 2 OFFSET 1",
        "SELECT id FROM kv WHERE id BETWEEN 3 AND 6 LIMIT 2 OFFSET 1",
    ];
    for sql in shapes {
        agree(&mut db, sql, &[]);
    }
}

/// A bound `LIMIT`/`OFFSET` is resolved at execution, not at planning, so the
/// two APIs have to resolve it the same way — including the pair that asks for
/// nothing.
#[test]
fn a_bound_limit_and_offset_land_the_same_way_on_both_paths() {
    let mut db = fixture();
    for (limit, offset) in [(0i64, 0i64), (1, 0), (4, 2), (100, 9), (2, 50)] {
        agree(
            &mut db,
            "SELECT id, body FROM kv LIMIT ? OFFSET ?",
            &[Value::Integer(limit), Value::Integer(offset)],
        );
    }
}

/// The shapes that fall back. Each one names a condition in
/// `borrowed_projection`; if one is dropped from that list the borrowing
/// pipeline would run a query it cannot answer, and this is where that shows
/// up as a wrong answer rather than as a silent speedup.
#[test]
fn every_shape_that_falls_back_still_answers_identically() {
    let mut db = fixture();
    let shapes = [
        // ORDER BY, and ORDER BY under a LIMIT — the pair that decides *which*
        // rows survive, so it must materialise.
        "SELECT id, email FROM kv ORDER BY email DESC",
        "SELECT id, email FROM kv ORDER BY weight LIMIT 4 OFFSET 2",
        // GROUP BY, aggregates, HAVING.
        "SELECT COUNT(*), MIN(id), MAX(id) FROM kv",
        "SELECT id % 3, COUNT(*) FROM kv GROUP BY id % 3",
        "SELECT id % 3, COUNT(*) FROM kv GROUP BY id % 3 HAVING COUNT(*) > 3",
        // DISTINCT.
        "SELECT DISTINCT id % 4 FROM kv",
        // A window function.
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM kv",
        // A projection holding an expression, which has nowhere to borrow from.
        "SELECT id + 1 FROM kv",
        "SELECT upper(email) FROM kv WHERE id < 5",
        "SELECT id, length(body) FROM kv LIMIT 3",
        // A derived table. (A *join* no longer falls back — since AHL-549 a
        // single non-blocking one is answered by a borrowed-cell operator,
        // and `crates/inlaysql/tests/joins_borrowed.rs` is where every join
        // shape is tied to the owned pipeline. It stays in this list because
        // the shape still has to answer identically, which is what this file
        // is for.)
        "SELECT kv.id, tags.tag FROM kv JOIN tags ON tags.kv_id = kv.id LIMIT 6",
        "SELECT id FROM (SELECT id FROM kv WHERE id < 6)",
        // A WITHOUT ROWID table, whose scan is a different source entirely.
        "SELECT code, label FROM codes",
        "SELECT label FROM codes WHERE code = 'c004'",
        // A subquery in the WHERE — the filter is borrowed-cell evaluated, and
        // a correlated subquery re-enters the engine from inside it.
        "SELECT id FROM kv WHERE id IN (SELECT kv_id FROM tags WHERE tag = 't1')",
        // A set operation.
        "SELECT id FROM kv WHERE id < 3 UNION SELECT id FROM kv WHERE id > 10",
    ];
    for sql in shapes {
        agree(&mut db, sql, &[]);
    }
}

/// The refusals are the same on both APIs, which is the point of
/// `begin_row_callback` being one function.
#[test]
fn the_borrowing_api_refuses_a_write_exactly_as_the_owned_one_does() {
    let mut db = fixture();
    let write = db.prepare("DELETE FROM kv WHERE id = 1").expect("prepare");
    let owned = db.query_prepared_each(&write, &[], |_| Ok(()));
    let borrowed = db.query_prepared_each_ref(&write, &[], |_| Ok(()));
    assert!(matches!(owned, Err(inlaysql::Error::Unsupported(_))));
    assert!(matches!(borrowed, Err(inlaysql::Error::Unsupported(_))));
    // And the refusal really did refuse: the row is still there.
    let count = db.query("SELECT COUNT(*) FROM kv", &[]).expect("count");
    assert_eq!(count.rows[0][0], Value::Integer(12));
}

/// A callback that fails stops the scan and reports its own error, on the
/// borrowing path as on the owned one — the reason both refuse writes.
#[test]
fn a_failing_callback_stops_the_borrowed_scan() {
    let mut db = fixture();
    let scan = db.prepare("SELECT id, body FROM kv").expect("prepare");
    let mut seen = 0usize;
    let outcome = db.query_prepared_each_ref(&scan, &[], |_| {
        seen += 1;
        if seen == 3 {
            Err(inlaysql::Error::Unsupported("stop".to_string()))
        } else {
            Ok(())
        }
    });
    assert!(matches!(outcome, Err(inlaysql::Error::Unsupported(_))));
    assert_eq!(seen, 3, "the scan carried on after the callback failed");
}

/// The borrowed cells really are the row's bytes: a `TEXT` cell reads as the
/// text that was stored, and a `BLOB` as the bytes, with no copy having been
/// made in between. A lifetime that were wrong here would not compile; a
/// *slice* that were wrong would read as garbage, which is what this pins.
#[test]
fn a_borrowed_cell_reads_the_stored_bytes() {
    let mut db = fixture();
    let scan = db
        .prepare("SELECT email, raw FROM kv WHERE id = ?")
        .expect("prepare");
    db.query_prepared_each_ref(&scan, &[Value::Integer(5)], |row| {
        assert_eq!(row[0].as_str(), Some("user0005@example.com"));
        assert_eq!(row[1].as_blob(), Some([5u8; 5].as_slice()));
        Ok(())
    })
    .expect("query");
}
