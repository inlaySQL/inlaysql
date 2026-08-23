//! Prepared statements over a real on-disk database, synchronous and async.
//!
//! `inlaysql-core`'s `tests/prepared.rs` covers the planner and the executor.
//! What is here is what only the shipped crate can show: that the file-backed
//! engine behaves the same, that a statement survives the trip to the async
//! I/O thread, and that a statement outliving the schema it was planned
//! against is an error rather than a wrong row.

use std::fs;
use std::path::PathBuf;

use inlaysql::{block_on, AsyncDatabase, Database, Error, Value};

/// A directory of our own, removed when the test ends.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "inlaysql-prepared-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create workspace");
        Self { dir }
    }

    fn db_path(&self) -> PathBuf {
        self.dir.join("prepared.inlay")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_prepared_statement_parses_once_against_a_real_file() {
    let workspace = Workspace::new("parse-once");
    let mut db = Database::open(workspace.db_path()).expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();

    let insert = db
        .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
        .unwrap();
    let lookup = db.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
    let baseline = db.statements_parsed();

    for id in 1..=50 {
        db.execute_prepared(
            &insert,
            &[Value::Integer(id), Value::Text(format!("row-{id}").into())],
        )
        .unwrap();
    }
    for id in 1..=50 {
        let rows = db.query_prepared(&lookup, &[Value::Integer(id)]).unwrap();
        assert_eq!(
            rows.rows,
            vec![vec![Value::Text(format!("row-{id}").into())]]
        );
    }

    assert_eq!(
        db.statements_parsed(),
        baseline,
        "a hundred executions of two prepared statements parsed something"
    );
}

#[test]
fn a_prepared_statement_reports_what_it_is() {
    let mut db = Database::open_in_memory().expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();

    let lookup = db.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
    assert_eq!(lookup.sql(), "SELECT body FROM kv WHERE id = ?");
    assert_eq!(lookup.parameter_count(), 1);
    assert!(lookup.is_read_only());

    let insert = db
        .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
        .unwrap();
    assert_eq!(insert.parameter_count(), 2);
    assert!(!insert.is_read_only());
}

#[test]
fn the_row_callback_matches_materialised_results_for_streaming_and_blocking_queries() {
    let mut db = Database::open_in_memory().expect("open");
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
        &[],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT)",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO users VALUES (1, 'one'), (17, 'seventeen'), (3, 'three')",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO posts VALUES (1, 17, 'b'), (2, 1, 'a'), (3, 17, 'c')",
        &[],
    )
    .unwrap();

    for sql in [
        "SELECT users.name, posts.title \
         FROM users JOIN posts ON posts.user_id = users.id",
        "SELECT users.name, posts.title \
         FROM users LEFT JOIN posts ON posts.user_id = users.id",
        "SELECT users.name || ':' || posts.title \
         FROM users JOIN posts ON posts.user_id = users.id \
         LIMIT 2 OFFSET 1",
        "SELECT users.name, posts.title \
         FROM users JOIN posts ON posts.user_id = users.id AND posts.title != 'b'",
        "SELECT users.name, posts.title \
         FROM users JOIN posts ON posts.user_id = users.id \
         ORDER BY posts.title DESC",
    ] {
        let statement = db.prepare(sql).unwrap();
        let expected = db.query_prepared(&statement, &[]).unwrap().rows;
        let mut streamed = Vec::new();
        let count = db
            .query_prepared_each(&statement, &[], |row| {
                streamed.push(row.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(count, expected.len());
        assert_eq!(streamed, expected);
    }
}

#[test]
fn a_row_callback_error_stops_and_is_returned() {
    let mut db = Database::open_in_memory().expect("open");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3)", &[])
        .unwrap();
    let statement = db.prepare("SELECT id FROM t").unwrap();
    let mut seen = 0;
    let error = db
        .query_prepared_each(&statement, &[], |_| {
            seen += 1;
            if seen == 2 {
                return Err(Error::Unsupported("consumer stopped".into()));
            }
            Ok(())
        })
        .expect_err("consumer error was swallowed");
    assert_eq!(seen, 2);
    assert!(matches!(error, Error::Unsupported(message) if message == "consumer stopped"));
}

#[test]
fn a_row_callback_refuses_writes_before_they_run() {
    let mut db = Database::open_in_memory().expect("open");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
        .unwrap();
    let insert = db.prepare("INSERT INTO t VALUES (1) RETURNING id").unwrap();
    let error = db
        .query_prepared_each(&insert, &[], |_| Ok(()))
        .expect_err("write reached the callback API");
    assert!(matches!(error, Error::Unsupported(_)));
    assert!(db.query("SELECT id FROM t", &[]).unwrap().rows.is_empty());
}

#[test]
fn a_cached_hash_build_refreshes_after_another_handle_commits() {
    let workspace = Workspace::new("hash-refresh");
    let path = workspace.db_path();
    let mut reader = Database::open(&path).expect("reader");
    reader
        .execute("CREATE TABLE users (id INTEGER PRIMARY KEY)", &[])
        .unwrap();
    reader
        .execute(
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER)",
            &[],
        )
        .unwrap();
    reader.execute("INSERT INTO users VALUES (1)", &[]).unwrap();
    reader
        .execute("INSERT INTO posts VALUES (1, 1)", &[])
        .unwrap();
    let join = reader
        .prepare("SELECT posts.id FROM users JOIN posts ON posts.user_id = users.id")
        .unwrap();
    assert_eq!(reader.query_prepared(&join, &[]).unwrap().rows.len(), 1);

    let mut writer = Database::open(&path).expect("writer");
    writer
        .execute("INSERT INTO posts VALUES (2, 1)", &[])
        .unwrap();

    let mut rows = Vec::new();
    reader
        .query_prepared_each(&join, &[], |row| {
            rows.push(row.to_vec());
            Ok(())
        })
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
}

#[test]
fn a_statement_outliving_its_schema_fails_loudly_rather_than_returning_wrong_rows() {
    let workspace = Workspace::new("stale");
    let path = workspace.db_path();

    // A statement that projects `body`, which is column 1 of this table.
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    let lookup = db.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
    assert!(db.query_prepared(&lookup, &[Value::Integer(1)]).is_ok());
    drop(db);

    // The database at that path is replaced by one whose `kv` has the same
    // column names in the other order. Nothing about the plan can tell: it
    // holds ordinals, so column 1 is now `id` and the statement would answer
    // with an integer where the caller asked for text.
    fs::remove_file(&path).unwrap();
    let mut db = Database::open(&path).expect("reopen");
    db.execute("CREATE TABLE kv (body TEXT, id INTEGER PRIMARY KEY)", &[])
        .unwrap();
    db.execute(
        "INSERT INTO kv (id, body) VALUES (?, ?)",
        &[Value::Integer(1), Value::Text("one".into())],
    )
    .unwrap();

    let error = db
        .query_prepared(&lookup, &[Value::Integer(1)])
        .expect_err("the stale statement returned rows");
    assert!(matches!(error, Error::Stale(_)), "got {error}");

    // Re-preparing is the documented recovery, and it works.
    let fresh = db.prepare("SELECT body FROM kv WHERE id = ?").unwrap();
    let rows = db.query_prepared(&fresh, &[Value::Integer(1)]).unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Text("one".into())]]);
}

#[test]
fn a_stale_statement_does_not_write_either() {
    let workspace = Workspace::new("stale-write");
    let path = workspace.db_path();

    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    let insert = db
        .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
        .unwrap();
    drop(db);

    fs::remove_file(&path).unwrap();
    let mut db = Database::open(&path).expect("reopen");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body BLOB)", &[])
        .unwrap();

    let error = db
        .execute_prepared(&insert, &[Value::Integer(1), Value::Text("one".into())])
        .expect_err("the stale insert was accepted");
    assert!(matches!(error, Error::Stale(_)), "got {error}");

    let rows = db.query("SELECT id FROM kv", &[]).unwrap();
    assert!(rows.rows.is_empty(), "a stale statement wrote a row");
}

#[test]
fn the_one_shot_api_is_unchanged() {
    // Most callers should not have to know prepared statements exist.
    let mut db = Database::open_in_memory().expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    db.execute(
        "INSERT INTO kv (id, body) VALUES (?, ?)",
        &[Value::Integer(1), Value::Text("one".into())],
    )
    .unwrap();
    let rows = db
        .query("SELECT body FROM kv WHERE id = ?", &[Value::Integer(1)])
        .unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Text("one".into())]]);
}

#[test]
fn a_prepared_statement_crosses_onto_the_async_io_thread() {
    block_on(async {
        let db = AsyncDatabase::open_in_memory().await.unwrap();
        db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
            .await
            .unwrap();

        let insert = db
            .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
            .await
            .unwrap();
        let lookup = db
            .prepare("SELECT body FROM kv WHERE id = ?")
            .await
            .unwrap();

        for id in 1..=10 {
            db.execute_prepared(
                &insert,
                &[Value::Integer(id), Value::Text(format!("row-{id}").into())],
            )
            .await
            .unwrap();
        }

        let rows = db
            .query_prepared(&lookup, &[Value::Integer(7)])
            .await
            .unwrap();
        assert_eq!(rows.rows, vec![vec![Value::Text("row-7".into())]]);

        // The handle is reference-counted, so the same statement can be held
        // by the caller and queued for the I/O thread at the same time.
        let parsed = db.with(|db| Ok(db.statements_parsed())).await.unwrap();
        assert_eq!(
            parsed, 3,
            "one CREATE and two prepares; the eleven executions parsed nothing"
        );
    });
}

#[test]
fn queued_prepared_statements_keep_their_arrival_order() {
    block_on(async {
        let db = AsyncDatabase::open_in_memory().await.unwrap();
        db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
            .await
            .unwrap();
        let insert = db
            .prepare("INSERT INTO kv (id, body) VALUES (?, ?)")
            .await
            .unwrap();

        // Queue without awaiting: each call took its own copy of the
        // parameters, so they must not interleave.
        let queued: Vec<_> = (1..=3)
            .map(|id| {
                db.execute_prepared(
                    &insert,
                    &[Value::Integer(id), Value::Text(format!("row-{id}").into())],
                )
            })
            .collect();
        for task in queued {
            task.await.unwrap();
        }

        let rows = db.query("SELECT body FROM kv", &[]).await.unwrap();
        assert_eq!(
            rows.rows,
            vec![
                vec![Value::Text("row-1".into())],
                vec![Value::Text("row-2".into())],
                vec![Value::Text("row-3".into())],
            ]
        );
    });
}
