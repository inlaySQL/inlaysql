//! The async API, driven the way an application would drive it.
//!
//! Originally written against the Tokio-backed `AsyncDatabase` from PR #14 and
//! carried over when that was replaced by the runtime-free one: the assertions
//! are about *behaviour*, not about which executor is underneath, so they
//! survived the change unaltered in intent.
//!
//! The one test that could not be carried over is the `blocking()` unwrap. That
//! API existed because the Tokio design held the engine in an
//! `Arc<Mutex<Engine>>` that could be unwrapped — and failed at runtime if any
//! clone was still alive. The current design gives the engine to a thread that
//! owns it, so there is nothing to unwrap; [`AsyncDatabase::with`] does the same
//! job (synchronous access to the same database) and cannot fail that way. The
//! last test here is the replacement.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use inlaysql::embedding::hashed_embedding;
use inlaysql::sqllogictest::{self, Summary};
use inlaysql::{block_on, AsyncDatabase, Database, Value};

const DIM: usize = 384;

fn test_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sqllogictest")
}

/// A directory of our own, so the single-file assertion has nothing else in it.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "inlaysql-async-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create workspace");
        Self { dir }
    }

    fn db_path(&self) -> PathBuf {
        self.dir.join("async.inlay")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn sqllogictest_files() -> Vec<(String, String)> {
    let mut files: Vec<PathBuf> = fs::read_dir(test_dir())
        .expect("read sqllogictest dir")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "test"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            (
                path.file_stem().unwrap().to_string_lossy().to_string(),
                fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}")),
            )
        })
        .collect()
}

#[test]
fn the_sqllogictest_subset_passes_on_the_async_api() {
    block_on(async {
        let mut all = Summary {
            total: 0,
            passed: 0,
            failures: Vec::new(),
        };

        // Each file starts from a fresh database, exactly like the synchronous
        // runner: files share table names.
        for (name, source) in sqllogictest_files() {
            let records = sqllogictest::parse(&source).unwrap_or_else(|e| panic!("{name}: {e}"));
            let db = AsyncDatabase::open_in_memory().await.expect("open");
            let summary = db
                .with(move |db| Ok(sqllogictest::run_on(db, &records)))
                .await
                .expect("run");
            all.total += summary.total;
            all.passed += summary.passed;
            all.failures.extend(summary.failures);
        }

        println!("async: {all}");
        assert!(
            all.failures.is_empty(),
            "SQL Logic Test failures on the async API:\n{:#?}",
            all.failures
        );
    });
}

#[test]
fn async_and_blocking_databases_agree() {
    block_on(async {
        let workspace = Workspace::new("agree");
        let corpus = [
            "embedded databases keep the whole engine inside your process",
            "rust gives you memory safety without a garbage collector",
            "an embedded database written in rust with vector retrieval",
        ];
        let schema = format!("CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR({DIM}))");
        let hybrid = "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
                      FROM docs ORDER BY score DESC LIMIT 3";
        let query = "embedded database";

        let db = AsyncDatabase::open(workspace.db_path())
            .await
            .expect("open");
        db.execute(&schema, &[]).await.expect("create");
        db.execute("CREATE INDEX docs_body ON docs (body)", &[])
            .await
            .expect("create body index");
        db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
            .await
            .expect("create embedding index");
        for (index, body) in corpus.iter().enumerate() {
            db.execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(index as i64 + 1),
                    Value::Text(body.to_string().into()),
                    Value::Vector(hashed_embedding(body, DIM)),
                ],
            )
            .await
            .expect("insert");
        }
        let from_async = db
            .query(
                hybrid,
                &[
                    Value::Vector(hashed_embedding(query, DIM)),
                    Value::Text(query.to_string().into()),
                ],
            )
            .await
            .expect("query");

        // A fresh synchronous database over the same rows must agree.
        let mut sync = Database::open_in_memory().expect("open sync");
        sync.execute(&schema, &[]).expect("create");
        sync.execute("CREATE INDEX docs_body ON docs (body)", &[])
            .expect("create body index");
        sync.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
            .expect("create embedding index");
        for (index, body) in corpus.iter().enumerate() {
            sync.execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(index as i64 + 1),
                    Value::Text(body.to_string().into()),
                    Value::Vector(hashed_embedding(body, DIM)),
                ],
            )
            .expect("insert");
        }
        let from_sync = sync
            .query(
                hybrid,
                &[
                    Value::Vector(hashed_embedding(query, DIM)),
                    Value::Text(query.to_string().into()),
                ],
            )
            .expect("query");

        assert_eq!(
            from_async, from_sync,
            "the async and synchronous APIs disagree about the same data"
        );
    });
}

#[test]
fn a_database_written_asynchronously_is_one_file() {
    block_on(async {
        let workspace = Workspace::new("one-file");
        {
            let db = AsyncDatabase::open(workspace.db_path())
                .await
                .expect("open");
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
                .await
                .expect("create");
            db.execute(
                "INSERT INTO t (id, body) VALUES (?, ?)",
                &[Value::Integer(1), Value::Text("one".into())],
            )
            .await
            .expect("insert");
        }

        let entries: Vec<String> = fs::read_dir(&workspace.dir)
            .expect("read workspace")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["async.inlay".to_string()], "not one file");
    });
}

#[test]
fn synchronous_access_to_the_same_database_is_available() {
    // The replacement for the old `blocking()` unwrap: `with` hands the very
    // same `Database` to synchronous code, without the "fails if a clone is
    // alive" failure mode the unwrap had.
    block_on(async {
        let db = AsyncDatabase::open_in_memory().await.expect("open");
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
            .await
            .expect("create");

        // A batch of synchronous work on the async handle's own database.
        let written = db
            .with(|db: &mut Database| {
                for id in 1..=3i64 {
                    db.execute(
                        "INSERT INTO t (id, body) VALUES (?, ?)",
                        &[Value::Integer(id), Value::Text(format!("row {id}").into())],
                    )?;
                }
                Ok(db.query("SELECT id FROM t", &[])?.rows.len())
            })
            .await
            .expect("with");
        assert_eq!(written, 3);

        // And the async side sees it — it is one database, not a copy.
        let rows = db
            .query("SELECT body FROM t WHERE id = 2", &[])
            .await
            .unwrap();
        assert_eq!(rows.rows, vec![vec![Value::Text("row 2".into())]]);
    });
}

/// Compile-time proof that `AsyncDatabase` really is `Send + Sync`, not just
/// documented as such. This only compiles if the bound holds — no `unsafe
/// impl`, no wishful thinking, just the type checker.
fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn async_database_is_send_and_sync() {
    assert_send_sync::<AsyncDatabase>();
}

#[test]
fn an_arc_wrapped_database_runs_statements_from_several_os_threads() {
    // The doc on `AsyncDatabase` claims it can be shared across threads, not
    // just tasks on one executor. Put one behind an `Arc`, hand clones to
    // several `std::thread::spawn`ed threads — no async runtime in sight —
    // and have each run its own statement concurrently with the others.
    const WRITERS: i64 = 8;

    block_on(async {
        let db = Arc::new(AsyncDatabase::open_in_memory().await.expect("open"));
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)", &[])
            .await
            .expect("create");

        let threads: Vec<_> = (0..WRITERS)
            .map(|id| {
                let db = Arc::clone(&db);
                thread::spawn(move || {
                    block_on(db.execute(
                        "INSERT INTO t (id, body) VALUES (?, ?)",
                        &[Value::Integer(id), Value::Text(format!("row {id}").into())],
                    ))
                    .expect("insert from another thread")
                })
            })
            .collect();

        for thread in threads {
            thread.join().expect("writer thread panicked");
        }

        let rows = db
            .query("SELECT id FROM t ORDER BY id", &[])
            .await
            .expect("query");
        let ids: Vec<i64> = rows
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Integer(id) => *id,
                other => panic!("unexpected value in id column: {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            (0..WRITERS).collect::<Vec<_>>(),
            "a writer's row is missing"
        );
    });
}
