//! The paged ANN index, driven through a real on-disk `Database`.
//!
//! `inlaysql-core`'s `hnsw_paged` unit tests prove the algorithm and the memory
//! bound against an in-memory backend. What is here is what only the shipped
//! crate can show: that the graph really is in the file, that it survives being
//! closed and reopened without a rebuild, that it commits with the rows it
//! describes rather than beside them, and that it answers the same queries as
//! the in-RAM index it replaces.

use std::fs;
use std::path::PathBuf;

use inlaysql::{Database, EngineOptions, Error, FileDevice, Value};

/// A directory of our own, removed when the test ends.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("inlaysql-paged-ann-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create workspace");
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

const DIM: usize = 16;

/// A deterministic unit vector that points mostly along axis `seed % DIM`.
fn vector(seed: u64) -> Vec<f32> {
    let mut values = vec![0.0f32; DIM];
    for (i, value) in values.iter_mut().enumerate() {
        // Small, seed-dependent, and reproducible: no clock, no RNG.
        *value = (((seed.wrapping_mul(2_654_435_761).wrapping_add(i as u64 * 97)) % 1000) as f32)
            / 1000.0;
    }
    values[(seed as usize) % DIM] += 4.0;
    values
}

fn create(db: &mut Database) {
    db.execute(
        &format!("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR({DIM}))"),
        &[],
    )
    .unwrap();
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .unwrap();
}

fn insert(db: &mut Database, id: i64) {
    db.execute(
        "INSERT INTO docs VALUES (?, ?, ?)",
        &[
            Value::Integer(id),
            Value::Text(format!("document {id}")),
            Value::Vector(vector(id as u64)),
        ],
    )
    .unwrap();
}

/// The ids a nearest-neighbour query returns, best first.
fn nearest(db: &mut Database, seed: u64, k: usize) -> Vec<i64> {
    let rows = db
        .query(
            &format!(
                "SELECT id, vector_score(embedding, ?) AS score
                 FROM docs ORDER BY score DESC LIMIT {k}"
            ),
            &[Value::Vector(vector(seed))],
        )
        .unwrap();
    rows.rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("expected an integer id, got {other:?}"),
        })
        .collect()
}

#[test]
fn the_graph_is_in_the_file_and_survives_a_reopen() {
    let workspace = Workspace::new("reopen");
    let path = workspace.path("docs.inlay");

    let expected = {
        let mut db = Database::open_paged(&path).unwrap();
        create(&mut db);
        for id in 1..=200 {
            insert(&mut db, id);
        }
        let expected = nearest(&mut db, 42, 5);
        assert_eq!(expected[0], 42, "the query vector's own row ranks first");
        db.checkpoint().unwrap();
        expected
    };

    // Reopened, the graph is read back out of the file rather than rebuilt, and
    // answers the same query.
    let mut db = Database::open_paged(&path).unwrap();
    assert_eq!(nearest(&mut db, 42, 5), expected);

    // And the same file opened with the in-RAM index — which rebuilds from the
    // rows — agrees, so the paged graph is not a private dialect.
    let mut in_ram = Database::open(&path).unwrap();
    assert_eq!(nearest(&mut in_ram, 42, 1), expected[..1].to_vec());
}

#[test]
fn the_graph_and_the_rows_commit_together() {
    let workspace = Workspace::new("atomic");
    let path = workspace.path("docs.inlay");

    let mut db = Database::open_paged(&path).unwrap();
    create(&mut db);
    for id in 1..=50 {
        insert(&mut db, id);
    }
    // A read commits the graph the writes left buffered.
    assert_eq!(nearest(&mut db, 7, 1), vec![7]);

    // Now a transaction that inserts and is rolled back. Its rows never happen,
    // and neither do the index writes that describe them — the index cannot end
    // up holding a row the table does not.
    db.begin().unwrap();
    for id in 51..=80 {
        insert(&mut db, id);
    }
    // Inside the transaction the writer sees its own rows.
    assert_eq!(nearest(&mut db, 60, 1), vec![60]);
    db.rollback().unwrap();

    let rows = db.query("SELECT id FROM docs WHERE id = 60", &[]).unwrap();
    assert!(rows.rows.is_empty(), "the rolled back row is gone");
    let neighbours = nearest(&mut db, 60, 5);
    assert!(
        !neighbours.contains(&60),
        "the index still ranks a row the rollback removed: {neighbours:?}"
    );

    // A committed transaction keeps both halves.
    db.begin().unwrap();
    for id in 51..=80 {
        insert(&mut db, id);
    }
    db.commit().unwrap();
    assert_eq!(nearest(&mut db, 60, 1), vec![60]);

    drop(db);
    let mut reopened = Database::open_paged(&path).unwrap();
    assert_eq!(nearest(&mut reopened, 60, 1), vec![60]);
}

#[test]
fn a_delete_stops_ranking_the_row_it_removed() {
    let workspace = Workspace::new("delete");
    let path = workspace.path("docs.inlay");

    let mut db = Database::open_paged(&path).unwrap();
    create(&mut db);
    for id in 1..=60 {
        insert(&mut db, id);
    }
    assert_eq!(nearest(&mut db, 30, 1), vec![30]);

    db.execute("DELETE FROM docs WHERE id = 30", &[]).unwrap();
    let neighbours = nearest(&mut db, 30, 5);
    assert!(!neighbours.contains(&30), "deleted row still ranked");

    drop(db);
    let mut reopened = Database::open_paged(&path).unwrap();
    let neighbours = nearest(&mut reopened, 30, 5);
    assert!(
        !neighbours.contains(&30),
        "the deletion did not survive the reopen: {neighbours:?}"
    );
}

#[test]
fn the_paged_and_in_ram_indexes_answer_the_same_queries() {
    let workspace = Workspace::new("parity");

    let mut paged = Database::open_paged(workspace.path("paged.inlay")).unwrap();
    let mut in_ram = Database::open(workspace.path("in-ram.inlay")).unwrap();
    create(&mut paged);
    create(&mut in_ram);
    for id in 1..=300 {
        insert(&mut paged, id);
        insert(&mut in_ram, id);
    }

    // The same graph algorithm on both sides, so this is equality, not a recall
    // threshold: the paging is a storage decision, not a different index.
    for seed in [1u64, 13, 42, 99, 250] {
        assert_eq!(
            nearest(&mut paged, seed, 10),
            nearest(&mut in_ram, seed, 10),
            "seed {seed}"
        );
    }
}

#[test]
fn a_hybrid_query_still_fuses_both_retrievers() {
    let workspace = Workspace::new("hybrid");
    let mut db = Database::open_paged(workspace.path("docs.inlay")).unwrap();
    create(&mut db);
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])
        .unwrap();
    for id in 1..=100 {
        insert(&mut db, id);
    }

    let rows = db
        .query(
            "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
             FROM docs ORDER BY score DESC LIMIT 5",
            &[Value::Vector(vector(17)), Value::Text("document 17".into())],
        )
        .unwrap();
    assert_eq!(rows.rows.len(), 5);
    assert_eq!(rows.rows[0][0], Value::Integer(17));
}

#[test]
fn the_index_does_not_hold_the_corpus_in_memory() {
    let workspace = Workspace::new("resident");
    let mut db = Database::open_paged(workspace.path("docs.inlay")).unwrap();
    create(&mut db);
    for id in 1..=500 {
        insert(&mut db, id);
    }
    assert_eq!(nearest(&mut db, 250, 1), vec![250]);

    // The in-RAM backend reports the embedding bytes it is holding. The paged
    // one holds a bounded cache instead of the corpus, so it reports nothing to
    // measure — which is the point of it.
    assert_eq!(db.vector_index_resident_bytes("docs", "embedding"), None);

    let mut in_ram = Database::open(workspace.path("in-ram.inlay")).unwrap();
    create(&mut in_ram);
    for id in 1..=500 {
        insert(&mut in_ram, id);
    }
    assert_eq!(nearest(&mut in_ram, 250, 1), vec![250]);
    let resident = in_ram
        .vector_index_resident_bytes("docs", "embedding")
        .expect("the in-RAM index measures itself");
    assert!(
        resident >= 500 * DIM * 4,
        "expected the in-RAM index to hold the whole corpus, got {resident} bytes"
    );
}

#[test]
fn the_options_constructor_reaches_the_same_place() {
    let workspace = Workspace::new("options");
    let path = workspace.path("docs.inlay");
    let device = FileDevice::open(&path).unwrap();
    let mut db = Database::open_on_with_options(
        device,
        EngineOptions {
            paged_vector_indexes: true,
            implicit_indexes: true,
            ..EngineOptions::default()
        },
    )
    .unwrap();

    // `implicit_indexes` means the VECTOR column is indexed without a
    // `CREATE INDEX`, and it is the paged backend that gets built.
    db.execute(
        &format!("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR({DIM}))"),
        &[],
    )
    .unwrap();
    for id in 1..=40 {
        insert(&mut db, id);
    }
    assert_eq!(nearest(&mut db, 11, 1), vec![11]);
    assert_eq!(db.vector_index_resident_bytes("docs", "embedding"), None);
}

#[test]
fn a_dimension_mismatch_is_still_an_error() {
    let workspace = Workspace::new("mismatch");
    let mut db = Database::open_paged(workspace.path("docs.inlay")).unwrap();
    create(&mut db);
    insert(&mut db, 1);

    let err = db
        .execute(
            "INSERT INTO docs VALUES (?, ?, ?)",
            &[
                Value::Integer(2),
                Value::Text("wrong width".into()),
                Value::Vector(vec![0.0; DIM + 1]),
            ],
        )
        .unwrap_err();
    assert!(matches!(err, Error::Type(_)), "unexpected error: {err:?}");
}
