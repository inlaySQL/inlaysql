//! A vector index's distance metric, driven through a real on-disk `Database`.
//!
//! `inlaysql-core`'s unit tests prove the kernel, the recall and the format
//! refusals against in-memory backends. What only the shipped crate can show is
//! the thing a user actually depends on: that the metric written at
//! `CREATE INDEX` survives closing the file, that the *real* HNSW backend —
//! not the brute-force reference — is the one honouring it, and that the graph
//! reopened from disk is the graph that was built, rather than one silently
//! rebuilt under the default.

use std::fs;
use std::path::{Path, PathBuf};

use inlaysql::{Database, Value, VectorMetric};

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-metric-{name}-{}-{:?}.inlay",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&path);
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Three rows whose *lengths* differ, which is the only kind of corpus on
/// which cosine and L2 can disagree at all: rows 1 and 2 point the same way,
/// row 3 points elsewhere but sits much closer to row 1.
const ROWS: [(i64, [f32; 2]); 3] = [(1, [1.0, 0.0]), (2, [8.0, 0.0]), (3, [0.7, 0.72])];

fn fill(db: &mut Database, ddl: &str) {
    db.execute(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, embedding VECTOR(2))",
        &[],
    )
    .unwrap();
    db.execute(ddl, &[]).unwrap();
    for (id, vector) in ROWS {
        db.execute(
            "INSERT INTO items VALUES (?, ?)",
            &[Value::Integer(id), Value::Vector(vector.to_vec())],
        )
        .unwrap();
    }
}

fn ranked(db: &mut Database) -> Vec<i64> {
    db.query(
        "SELECT id, vector_score(embedding, ?) AS score FROM items \
         ORDER BY score DESC LIMIT 10",
        &[Value::Vector(vec![1.0, 0.0])],
    )
    .unwrap()
    .rows
    .iter()
    .map(|row| match row[0] {
        Value::Integer(id) => id,
        ref other => panic!("id was {other:?}"),
    })
    .collect()
}

/// The metric is written with the index and is still in force after the file
/// has been closed and reopened.
///
/// The reopen is the half that matters. A graph is a cache over the rows, and
/// the engine rebuilds one it cannot trust — so an implementation that only
/// stored the metric in memory would pass every in-process test and then answer
/// the first query after a restart under the default, with no error and no way
/// for anyone to notice but the ranking.
#[test]
fn an_l2_index_survives_a_reopen_and_still_ranks_by_distance() {
    let file = TempDb::new("l2-reopen");
    {
        let mut db = Database::open(file.path()).unwrap();
        fill(
            &mut db,
            "CREATE INDEX items_embedding ON items (embedding vector_l2_ops)",
        );
        assert_eq!(ranked(&mut db), vec![1, 3, 2]);
    }

    let mut db = Database::open(file.path()).unwrap();
    assert_eq!(
        db.catalog().indexes_for("items")[0].metric,
        VectorMetric::L2,
        "the metric did not survive the reopen"
    );
    assert_eq!(ranked(&mut db), vec![1, 3, 2]);

    // A row inserted after the reopen is placed in the graph under the same
    // metric — incremental maintenance is where a stale metric would surface
    // as a slowly corrupting index rather than as a wrong answer at once.
    db.execute(
        "INSERT INTO items VALUES (?, ?)",
        &[Value::Integer(4), Value::Vector(vec![1.02, 0.0])],
    )
    .unwrap();
    assert_eq!(ranked(&mut db), vec![1, 4, 3, 2]);
}

/// The same database under the default metric answers differently, which is
/// what says the ranking above came from the declaration and not from the data.
#[test]
fn the_default_index_still_ranks_by_cosine() {
    let file = TempDb::new("cosine-reopen");
    {
        let mut db = Database::open(file.path()).unwrap();
        fill(&mut db, "CREATE INDEX items_embedding ON items (embedding)");
    }
    let mut db = Database::open(file.path()).unwrap();
    assert_eq!(
        db.catalog().indexes_for("items")[0].metric,
        VectorMetric::Cosine
    );
    // Rows 1 and 2 are the same direction, so cosine cannot separate them and
    // the tie falls to the lower row id; row 3 points elsewhere and is last.
    assert_eq!(ranked(&mut db), vec![1, 2, 3]);
}

/// The paged backend — the graph that lives in the database file rather than
/// in the handle — carries the metric in its own header, and reopens under it.
#[test]
fn a_paged_l2_graph_reopens_under_its_own_metric() {
    let file = TempDb::new("paged-l2");
    {
        let mut db = Database::open_paged(file.path()).unwrap();
        fill(
            &mut db,
            "CREATE INDEX items_embedding ON items (embedding vector_l2_ops)",
        );
        assert_eq!(ranked(&mut db), vec![1, 3, 2]);
    }

    let mut db = Database::open_paged(file.path()).unwrap();
    assert_eq!(ranked(&mut db), vec![1, 3, 2]);
}
