//! Indexes are written into the database and restored on open — and the
//! restore is a cache that is never trusted further than the write version it
//! was stamped with.
//!
//! The assertions count calls to [`Storage::scan`] rather than measuring time.
//! "Opening did not re-read every row" is the actual claim, and a call count
//! states it exactly.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::row::RowBuf;
use inlaysql_core::traits::{RowId, Storage};
use inlaysql_core::{Catalog, Column, DataType, Engine, Error, Table, Value};

/// A `MemStorage` several engines can share, so a database can be "reopened".
#[derive(Clone, Default)]
struct SharedStorage {
    inner: Rc<RefCell<MemStorage>>,
    scans: Rc<Cell<usize>>,
}

impl SharedStorage {
    fn scans(&self) -> usize {
        self.scans.get()
    }

    fn reset_scans(&self) {
        self.scans.set(0);
    }

    /// Corrupt (or remove) a persisted index, as a torn write would.
    fn damage_index(&self, key: &str, bytes: Option<&[u8]>) {
        let mut inner = self.inner.borrow_mut();
        match bytes {
            Some(bytes) => inner.put_meta(key, bytes).unwrap(),
            None => inner.put_meta(key, &[]).unwrap(),
        }
        inner.commit().unwrap();
    }

    fn meta(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.borrow().get_meta(key).unwrap()
    }
}

impl Storage for SharedStorage {
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> inlaysql_core::Result<()> {
        self.inner.borrow_mut().put_row(table, id, bytes)
    }

    fn get_row(&self, table: &str, id: RowId) -> inlaysql_core::Result<Option<RowBuf>> {
        self.inner.borrow().get_row(table, id)
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> inlaysql_core::Result<()> {
        self.inner.borrow_mut().delete_row(table, id)
    }

    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> inlaysql_core::Result<Vec<(RowId, RowBuf)>> {
        self.scans.set(self.scans.get() + 1);
        self.inner.borrow().scan_batch(table, after, limit)
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> inlaysql_core::Result<()> {
        self.inner.borrow_mut().put_meta(key, bytes)
    }

    fn get_meta(&self, key: &str) -> inlaysql_core::Result<Option<Vec<u8>>> {
        self.inner.borrow().get_meta(key)
    }

    fn put_index_entry(&mut self, key: &[u8]) -> inlaysql_core::Result<()> {
        self.inner.borrow_mut().put_index_entry(key)
    }

    fn delete_index_entry(&mut self, key: &[u8]) -> inlaysql_core::Result<()> {
        self.inner.borrow_mut().delete_index_entry(key)
    }

    fn scan_index_range(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> inlaysql_core::Result<Vec<Vec<u8>>> {
        self.inner.borrow().scan_index_range(start, end)
    }

    fn commit(&mut self) -> inlaysql_core::Result<()> {
        self.inner.borrow_mut().commit()
    }

    fn rollback(&mut self) -> inlaysql_core::Result<()> {
        self.inner.borrow_mut().rollback()
    }
}

fn open(storage: &SharedStorage) -> Engine {
    Engine::open(
        Box::new(storage.clone()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .expect("open")
}

const CORPUS: &[(&str, [f32; 3])] = &[
    ("embedded database engine", [1.0, 0.0, 0.0]),
    ("vector search index", [0.0, 1.0, 0.0]),
    ("embedded vector database", [0.9, 0.4, 0.0]),
    ("cast iron cooking", [0.0, 0.0, 1.0]),
];

/// A database with a text and a vector column, checkpointed so its indexes are
/// on disk.
fn seeded() -> SharedStorage {
    let storage = SharedStorage::default();
    let mut engine = open(&storage);
    engine
        .execute(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(3))",
            &[],
        )
        .unwrap();
    engine
        .execute("CREATE INDEX docs_body ON docs (body)", &[])
        .unwrap();
    engine
        .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .unwrap();
    for (index, (body, embedding)) in CORPUS.iter().enumerate() {
        engine
            .execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(index as i64 + 1),
                    Value::Text(body.to_string().into()),
                    Value::Vector(embedding.to_vec()),
                ],
            )
            .unwrap();
    }
    engine.checkpoint().unwrap();
    drop(engine);
    storage
}

/// The ids a hybrid query returns, in rank order.
fn hybrid_ids(engine: &mut Engine) -> Vec<i64> {
    engine
        .query(
            "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
             FROM docs ORDER BY score DESC LIMIT 4",
            &[
                Value::Vector(vec![1.0, 0.2, 0.0]),
                Value::Text("embedded database".to_string().into()),
            ],
        )
        .unwrap()
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            ref other => panic!("expected an integer id, got {other:?}"),
        })
        .collect()
}

#[test]
fn a_checkpointed_database_opens_without_re_reading_its_rows() {
    let storage = seeded();
    storage.reset_scans();

    let mut engine = open(&storage);
    assert_eq!(
        storage.scans(),
        0,
        "opening scanned the table instead of loading the saved indexes"
    );
    assert!(!hybrid_ids(&mut engine).is_empty());
}

#[test]
fn a_restored_index_answers_exactly_as_a_rebuilt_one() {
    let storage = seeded();

    let restored = hybrid_ids(&mut open(&storage));

    // Force the rebuild path by invalidating the saved copies, then ask again.
    storage.damage_index("index:docs:body", None);
    let rebuilt = hybrid_ids(&mut open(&storage));

    assert_eq!(
        restored, rebuilt,
        "the saved index and the rebuilt index disagree"
    );
}

#[test]
fn a_write_after_the_checkpoint_invalidates_the_saved_index() {
    let storage = seeded();
    {
        let mut engine = open(&storage);
        engine
            .execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(99),
                    Value::Text("late arriving embedded document".to_string().into()),
                    Value::Vector(vec![0.8, 0.1, 0.0]),
                ],
            )
            .unwrap();
        // Deliberately no checkpoint: the saved index is now behind the rows.
    }

    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(
        storage.scans(),
        1,
        "a stale saved index was trusted instead of rebuilt"
    );

    // And the new row is findable, which it would not be if the stale index
    // had been used.
    assert!(
        hybrid_ids(&mut engine).contains(&99),
        "the row inserted after the checkpoint is missing from the index"
    );
}

#[test]
fn a_corrupt_saved_index_is_discarded_rather_than_decoded() {
    let storage = seeded();
    let chunk = storage
        .meta("index:docs:embedding/0")
        .expect("saved index chunk");

    // The header stays valid and current, so nothing but the decoder can catch
    // this. Same length, so the reassembly check cannot catch it either.
    storage.damage_index("index:docs:embedding/0", Some(&vec![0xff; chunk.len()]));

    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(storage.scans(), 1, "the corrupt index was not rebuilt");
    assert_eq!(hybrid_ids(&mut engine).len(), CORPUS.len());
}

#[test]
fn a_missing_chunk_is_noticed_rather_than_silently_shortening_the_index() {
    let storage = seeded();
    storage.damage_index("index:docs:body/0", Some(&[]));

    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(storage.scans(), 1);
    assert_eq!(hybrid_ids(&mut engine).len(), CORPUS.len());
}

#[test]
fn a_truncated_header_is_discarded_rather_than_misread() {
    let storage = seeded();
    storage.damage_index("index:docs:body", Some(&[1, 2, 3]));

    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(storage.scans(), 1);
    assert_eq!(hybrid_ids(&mut engine).len(), CORPUS.len());
}

#[test]
fn an_index_too_large_for_one_page_round_trips_across_chunks() {
    // 4 KiB pages, so a few hundred vectors is already many chunks. This is the
    // case that fails if the blob is stored as one value.
    let storage = SharedStorage::default();
    {
        let mut engine = open(&storage);
        engine
            .execute(
                "CREATE TABLE big (id INTEGER PRIMARY KEY, embedding VECTOR(64))",
                &[],
            )
            .unwrap();
        engine
            .execute("CREATE INDEX big_embedding ON big (embedding)", &[])
            .unwrap();
        for id in 1..=400i64 {
            let angle = id as f32 * 0.11;
            let embedding: Vec<f32> = (0..64).map(|i| ((angle + i as f32) * 0.31).sin()).collect();
            engine
                .execute(
                    "INSERT INTO big (id, embedding) VALUES (?, ?)",
                    &[Value::Integer(id), Value::Vector(embedding)],
                )
                .unwrap();
        }
        engine.checkpoint().unwrap();
    }

    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(storage.scans(), 0, "a chunked index was not restored");

    let probe: Vec<f32> = (0..64)
        .map(|i| ((7.0 * 0.11 + i as f32) * 0.31).sin())
        .collect();
    let hits = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM big ORDER BY score DESC LIMIT 1",
            &[Value::Vector(probe)],
        )
        .unwrap();
    assert_eq!(
        hits.rows.first().map(|row| row[0].clone()),
        Some(Value::Integer(7))
    );
}

#[test]
fn deletes_are_reflected_in_the_restored_index() {
    let storage = seeded();
    {
        let mut engine = open(&storage);
        engine
            .execute("DELETE FROM docs WHERE id = 1", &[])
            .unwrap();
        engine.checkpoint().unwrap();
    }

    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(
        storage.scans(),
        0,
        "the checkpoint after the delete was ignored"
    );
    assert!(
        !hybrid_ids(&mut engine).contains(&1),
        "the deleted row is still in the restored index"
    );
}

#[test]
fn a_table_with_no_indexable_column_still_opens_without_scanning() {
    let storage = SharedStorage::default();
    {
        let mut engine = open(&storage);
        engine
            .execute("CREATE TABLE nums (id INTEGER PRIMARY KEY, n INTEGER)", &[])
            .unwrap();
        engine
            .execute("INSERT INTO nums (id, n) VALUES (1, 10), (2, 20)", &[])
            .unwrap();
        engine.checkpoint().unwrap();
    }

    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(storage.scans(), 0);
    assert_eq!(
        engine.query("SELECT n FROM nums", &[]).unwrap().rows,
        vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
    );
}

#[test]
fn an_unindexed_text_column_never_builds_persists_or_rebuilds() {
    let storage = SharedStorage::default();
    {
        let mut engine = open(&storage);
        engine
            .execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)", &[])
            .unwrap();
        for id in 1..=3i64 {
            engine
                .execute(
                    "INSERT INTO docs (id, body) VALUES (?, ?)",
                    &[
                        Value::Integer(id),
                        Value::Text("document about embedded storage".into()),
                    ],
                )
                .unwrap();
        }
        engine.checkpoint().unwrap();
    }

    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(
        storage.scans(),
        0,
        "opening scanned a table whose columns carry no index"
    );
    let err = engine
        .execute(
            "SELECT id, bm25_score(body, 'embedded') FROM docs ORDER BY score DESC LIMIT 1",
            &[],
        )
        .unwrap_err();
    assert!(matches!(err, Error::Index(_)), "got {err}");
}

#[test]
fn create_index_builds_from_the_existing_rows() {
    let storage = SharedStorage::default();
    {
        let mut engine = open(&storage);
        engine
            .execute(
                "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(3))",
                &[],
            )
            .unwrap();
        for (index, (body, embedding)) in CORPUS.iter().enumerate() {
            engine
                .execute(
                    "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                    &[
                        Value::Integer(index as i64 + 1),
                        Value::Text(body.to_string().into()),
                        Value::Vector(embedding.to_vec()),
                    ],
                )
                .unwrap();
        }
        // Scoring fails before the index exists.
        assert!(engine
            .execute(
                "SELECT id, bm25_score(body, 'embedded') FROM docs ORDER BY score DESC LIMIT 1",
                &[],
            )
            .is_err());
        // The index is created after the rows, so it has to be built by a scan.
        engine
            .execute("CREATE INDEX docs_body ON docs (body)", &[])
            .unwrap();
        engine
            .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
            .unwrap();
        engine.checkpoint().unwrap();
    }

    // Reopening restores the index, and it knows the rows that predate it.
    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(storage.scans(), 0);
    assert_eq!(hybrid_ids(&mut engine).len(), CORPUS.len());
}

#[test]
fn drop_index_removes_the_declaration_and_the_saved_copy() {
    let storage = SharedStorage::default();
    {
        let mut engine = open(&storage);
        engine
            .execute(
                "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(3))",
                &[],
            )
            .unwrap();
        engine
            .execute("CREATE INDEX docs_body ON docs (body)", &[])
            .unwrap();
        engine
            .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
            .unwrap();
        for (index, (body, embedding)) in CORPUS.iter().enumerate() {
            engine
                .execute(
                    "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                    &[
                        Value::Integer(index as i64 + 1),
                        Value::Text(body.to_string().into()),
                        Value::Vector(embedding.to_vec()),
                    ],
                )
                .unwrap();
        }
        engine.checkpoint().unwrap();
        engine.execute("DROP INDEX docs_body", &[]).unwrap();
    }

    // The dropped index is gone, and opening does not rebuild it.
    storage.reset_scans();
    let mut engine = open(&storage);
    assert_eq!(storage.scans(), 0, "opening rebuilt a dropped index");
    let err = engine
        .execute(
            "SELECT id, bm25_score(body, 'embedded') FROM docs ORDER BY score DESC LIMIT 1",
            &[],
        )
        .unwrap_err();
    assert!(matches!(err, Error::Index(_)), "got {err}");
    // The surviving vector index still answers.
    let hits = engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC LIMIT 4",
            &[Value::Vector(vec![1.0, 0.2, 0.0])],
        )
        .unwrap();
    assert_eq!(hits.rows.len(), CORPUS.len());
}

#[test]
fn open_implicit_indexes_everything_as_before() {
    let storage = SharedStorage::default();
    let mut engine = Engine::open_implicit(
        Box::new(storage.clone()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::new()),
    )
    .unwrap();
    engine
        .execute(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(3))",
            &[],
        )
        .unwrap();
    // No CREATE INDEX — the implicit mode declared them at CREATE TABLE time.
    for (index, (body, embedding)) in CORPUS.iter().enumerate() {
        engine
            .execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(index as i64 + 1),
                    Value::Text(body.to_string().into()),
                    Value::Vector(embedding.to_vec()),
                ],
            )
            .unwrap();
    }
    assert_eq!(hybrid_ids(&mut engine).len(), CORPUS.len());
}

#[test]
fn a_database_written_before_create_index_is_grandfathered() {
    let storage = SharedStorage::default();

    // A version-1 catalog: the table list only, no index declarations and no
    // magic prefix. This is what a pre-CREATE-INDEX binary wrote. Reconstructed
    // from a version-2 encoding by stripping the magic + version prefix and the
    // trailing (empty) index section.
    let mut catalog = Catalog::new();
    catalog
        .create_table(Table {
            name: "docs".to_string(),
            columns: vec![
                Column::primary_key("id", DataType::Integer),
                Column::new("body", DataType::Text),
                Column::new("embedding", DataType::Vector(3)),
            ],
            strict: false,
        })
        .unwrap();
    let v2 = catalog.encode();
    let v1 = &v2[8..v2.len() - 4];
    storage.damage_index("catalog", Some(v1));

    {
        let mut engine = open(&storage);
        // Grandfathering keeps the automatic indexes, so a hybrid query answers
        // with no CREATE INDEX anywhere.
        for (index, (body, embedding)) in CORPUS.iter().enumerate() {
            engine
                .execute(
                    "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                    &[
                        Value::Integer(index as i64 + 1),
                        Value::Text(body.to_string().into()),
                        Value::Vector(embedding.to_vec()),
                    ],
                )
                .unwrap();
        }
        engine.checkpoint().unwrap();
    }

    storage.reset_scans();
    let mut engine = open(&storage);
    assert!(
        !hybrid_ids(&mut engine).is_empty(),
        "a grandfathered database no longer answers its queries"
    );
}
