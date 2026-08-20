//! The walking-skeleton demo, run against a real on-disk database.
//!
//! This is the test the Stage 1 acceptance criteria describe: open a
//! single-file database, declare a `VECTOR(384)` column, insert rows, and get
//! fused vector + BM25 results out of one SQL statement.

use std::fs;
use std::path::PathBuf;

use inlaysql::embedding::hashed_embedding;
use inlaysql::{Database, ResultSet, Value};

const DIM: usize = 384;

const CORPUS: &[(i64, &str)] = &[
    (
        1,
        "embedded databases keep the whole engine inside your process",
    ),
    (
        2,
        "rust gives you memory safety without a garbage collector",
    ),
    (
        3,
        "an embedded database written in rust with vector retrieval",
    ),
    (4, "cast iron skillet cornbread recipe with buttermilk"),
    (5, "approximate nearest neighbour search over embeddings"),
    (6, "a web framework for building sites quickly"),
];

const QUERY_TEXT: &str = "embedded database";
const QUERY_SUBJECT: &str = "embedded databases in rust";

/// A directory of our own, so the single-file assertion has nothing else in it.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "inlaysql-e2e-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create workspace");
        Self { dir }
    }

    fn db_path(&self) -> PathBuf {
        self.dir.join("demo.inlay")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn seed(db: &mut Database) {
    db.execute(
        "CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR(384))",
        &[],
    )
    .expect("create table");
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])
        .expect("create body index");
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .expect("create embedding index");

    for (id, body) in CORPUS {
        db.execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(*id),
                Value::Text(body.to_string()),
                Value::Vector(hashed_embedding(body, DIM)),
            ],
        )
        .expect("insert");
    }
}

fn ids(result: &ResultSet) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| row[0].as_i64().expect("id column"))
        .collect()
}

fn hybrid(db: &mut Database, limit: usize) -> ResultSet {
    db.query(
        &format!(
            "SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
             FROM docs ORDER BY score DESC LIMIT {limit}"
        ),
        &[
            Value::Vector(hashed_embedding(QUERY_SUBJECT, DIM)),
            Value::Text(QUERY_TEXT.to_string()),
        ],
    )
    .expect("hybrid query")
}

#[test]
fn a_database_is_one_file() {
    let workspace = Workspace::new("single-file");
    {
        let mut db = Database::open(workspace.db_path()).expect("open");
        seed(&mut db);
        hybrid(&mut db, 3);
    }

    let entries: Vec<_> = fs::read_dir(&workspace.dir)
        .expect("read workspace")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one file, got {entries:?}"
    );
    assert_eq!(entries[0], workspace.db_path());
}

#[test]
fn one_statement_returns_fused_results() {
    let workspace = Workspace::new("fused");
    let mut db = Database::open(workspace.db_path()).expect("open");
    seed(&mut db);

    let result = hybrid(&mut db, 3);
    assert_eq!(result.columns, vec!["id", "body", "score"]);
    assert_eq!(result.rows.len(), 3);

    let ranked = ids(&result);
    assert!(
        ranked.contains(&3),
        "the doc matching both retrievers should rank, got {ranked:?}"
    );
    assert!(
        !ranked.contains(&4),
        "the cornbread recipe should not rank, got {ranked:?}"
    );

    let scores: Vec<f64> = result
        .rows
        .iter()
        .map(|row| row[2].as_f64().expect("score"))
        .collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "not ranked: {scores:?}"
    );
}

#[test]
fn the_fused_order_is_rank_fusion_of_the_two_retrievers() {
    let workspace = Workspace::new("agrees");
    let mut db = Database::open(workspace.db_path()).expect("open");
    seed(&mut db);

    let vector = ids(&db
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs LIMIT 6",
            &[Value::Vector(hashed_embedding(QUERY_SUBJECT, DIM))],
        )
        .expect("vector query"));
    let text = ids(&db
        .query(
            "SELECT id, bm25_score(body, ?) AS score FROM docs LIMIT 6",
            &[Value::Text(QUERY_TEXT.to_string())],
        )
        .expect("text query"));

    let mut expected: Vec<(i64, f32)> = Vec::new();
    for list in [&vector, &text] {
        for (rank, id) in list.iter().enumerate() {
            let contribution = 1.0 / (60.0 + rank as f32 + 1.0);
            match expected.iter_mut().find(|(other, _)| other == id) {
                Some((_, score)) => *score += contribution,
                None => expected.push((*id, contribution)),
            }
        }
    }
    expected.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let expected: Vec<i64> = expected.into_iter().map(|(id, _)| id).take(3).collect();

    assert_eq!(ids(&hybrid(&mut db, 3)), expected);
}

#[test]
fn rows_and_rankings_survive_a_reopen() {
    let workspace = Workspace::new("reopen");
    let before = {
        let mut db = Database::open(workspace.db_path()).expect("open");
        seed(&mut db);
        hybrid(&mut db, 3)
    };

    // Reopening rebuilds both indexes from the stored rows; the ranking must
    // come back identical, not merely similar.
    let mut db = Database::open(workspace.db_path()).expect("reopen");
    assert_eq!(hybrid(&mut db, 3), before);
    assert_eq!(
        db.query("SELECT id FROM docs", &[])
            .expect("scan")
            .rows
            .len(),
        CORPUS.len()
    );
}

#[test]
fn inserts_after_a_reopen_are_immediately_searchable() {
    let workspace = Workspace::new("append");
    {
        let mut db = Database::open(workspace.db_path()).expect("open");
        seed(&mut db);
    }

    let mut db = Database::open(workspace.db_path()).expect("reopen");
    let body = "an embedded database with hybrid retrieval built in";
    db.execute(
        "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
        &[
            Value::Integer(7),
            Value::Text(body.to_string()),
            Value::Vector(hashed_embedding(body, DIM)),
        ],
    )
    .expect("insert");

    assert!(
        ids(&hybrid(&mut db, 3)).contains(&7),
        "the row added after reopening did not reach the indexes"
    );
}

#[test]
fn the_in_memory_database_behaves_like_the_on_disk_one() {
    let workspace = Workspace::new("memory");
    let mut disk = Database::open(workspace.db_path()).expect("open");
    let mut memory = Database::open_in_memory().expect("open in memory");
    seed(&mut disk);
    seed(&mut memory);

    assert_eq!(hybrid(&mut memory, 3), hybrid(&mut disk, 3));
}

#[test]
fn a_dimension_mismatch_is_rejected_at_insert() {
    let workspace = Workspace::new("dimension");
    let mut db = Database::open(workspace.db_path()).expect("open");
    seed(&mut db);

    let err = db
        .execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(99),
                Value::Text("wrong width".to_string()),
                Value::Vector(vec![0.0; 8]),
            ],
        )
        .expect_err("expected a type error");
    assert!(err.to_string().contains("VECTOR(384)"), "{err}");
}

/// A subquery evaluated *inside* a running pipeline must not double-borrow
/// storage (AHL-463).
///
/// This is the one failure mode the subquery work could have introduced that no
/// SQL-level assertion would catch: `SharedStorage` is an `Rc<RefCell<_>>`, and
/// a correlated subquery re-enters the executor while the outer `RowScan` is
/// mid-stream. It is safe because no `Storage` call holds a borrow past its own
/// return — but "safe by reasoning" is worth one test that would panic rather
/// than fail if the reasoning were wrong.
///
/// On disk rather than in memory, and with more rows than one scan batch, so
/// the outer scan really does go back to storage between rows; and with the
/// paged ANN index open, because that is the *other* holder of the same handle.
#[test]
fn a_correlated_subquery_does_not_double_borrow_storage() {
    let workspace = Workspace::new("subquery-borrow");
    let mut db = Database::open_paged(workspace.db_path()).expect("open");
    db.execute(
        "CREATE TABLE outer_rows (id INTEGER PRIMARY KEY, a INTEGER, embedding VECTOR(384))",
        &[],
    )
    .expect("create outer");
    db.execute(
        "CREATE INDEX outer_embedding ON outer_rows (embedding)",
        &[],
    )
    .expect("create embedding index");
    db.execute(
        "CREATE TABLE inner_rows (id INTEGER PRIMARY KEY, a INTEGER)",
        &[],
    )
    .expect("create inner");

    // Well past `RowScan`'s first batch of 32 and its 512-row ceiling, so the
    // outer scan resumes from storage many times over while subqueries run.
    const ROWS: i64 = 700;
    for id in 1..=ROWS {
        // Only the first few rows carry an embedding: the point is that the
        // paged index is *open* and holding the same storage handle, not that
        // it is large.
        let embedding = if id <= 16 {
            Value::Vector(hashed_embedding(&format!("row {id}"), DIM))
        } else {
            Value::Null
        };
        db.execute(
            "INSERT INTO outer_rows (id, a, embedding) VALUES (?, ?, ?)",
            &[Value::Integer(id), Value::Integer(id % 97), embedding],
        )
        .expect("insert outer");
        db.execute(
            "INSERT INTO inner_rows (id, a) VALUES (?, ?)",
            &[Value::Integer(id), Value::Integer(id % 97)],
        )
        .expect("insert inner");
    }

    // Correlated `EXISTS`: one inner scan per outer row, both streaming through
    // the same `SharedStorage`.
    let matched = db
        .query(
            "SELECT COUNT(*) FROM outer_rows WHERE EXISTS \
             (SELECT 1 FROM inner_rows WHERE inner_rows.a = outer_rows.a \
              AND inner_rows.id > 650)",
            &[],
        )
        .expect("correlated EXISTS");
    assert_eq!(matched.rows[0][0].as_i64(), Some(371));

    // A correlated scalar subquery beside an uncorrelated one, which the memo
    // answers without reading anything a second time.
    let counted = db
        .query(
            "SELECT COUNT(*) FROM outer_rows WHERE \
             (SELECT COUNT(*) FROM inner_rows WHERE inner_rows.a = outer_rows.a) \
             = (SELECT COUNT(*) FROM inner_rows WHERE a = 0)",
            &[],
        )
        .expect("correlated scalar subquery");
    assert_eq!(counted.rows[0][0].as_i64(), Some(532));

    // And a derived table, materialised while the paged index shares the handle.
    let derived = db
        .query(
            "SELECT COUNT(*) FROM (SELECT id FROM outer_rows WHERE a = 5) AS d",
            &[],
        )
        .expect("derived table");
    assert_eq!(derived.rows[0][0].as_i64(), Some(8));
}
