//! What another handle's commit costs the handles that did not make it.
//!
//! A handle re-reads the committed state at the start of every statement it
//! runs outside a transaction, so a writer on one connection is visible to a
//! reader on another. The question this file pins down is what that discovery
//! *costs*: a retrieval index is rebuilt from every row of its table when it
//! cannot be restored, and "somebody else committed a row" must not be a reason
//! to do that. On a server with `n` connections, one insert per second would
//! otherwise mean `n` full re-indexes per second.
//!
//! The assertions count calls to `FullTextIndex::insert` and
//! `VectorIndex::insert` rather than measuring time, for the same reason
//! `index_persistence.rs` counts scans: "the rows it did not touch were not
//! re-indexed" is the actual claim, and a call count states it exactly. A
//! timing threshold would state something weaker and flake.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use inlaysql_core::mem::{BruteForceVectorIndex, LogicalClock, MemFullTextIndex, MemStorage};
use inlaysql_core::row::RowBuf;
use inlaysql_core::traits::{
    Clock, FullTextIndex, IndexFactory, RowFilter, RowId, Scored, Storage, VectorIndex,
};
use inlaysql_core::{Engine, Value};

// --------------------------------------------------------------- the file

/// One `MemStorage` several engines share, standing in for one database file
/// opened by several handles.
///
/// [`Storage::refresh`] is the whole point of the harness. The default
/// implementation answers `false` — "nothing moved" — which is correct for a
/// backend only one handle can reach and which would make every test here
/// vacuous. This one answers from a commit counter the shared store keeps, the
/// way `CowBTree::refresh` answers from the device's commit generation: a
/// handle sees `true` exactly once per commit somebody *else* made.
#[derive(Clone)]
struct FileHandle {
    inner: Rc<RefCell<MemStorage>>,
    /// Commits made through any handle on this file.
    generation: Rc<Cell<u64>>,
    /// The generation this handle has already adopted. Not shared: that is
    /// what makes one handle's commit invisible to itself and visible to the
    /// others.
    seen: Rc<Cell<u64>>,
}

impl FileHandle {
    fn create() -> Self {
        Self {
            inner: Rc::new(RefCell::new(MemStorage::new())),
            generation: Rc::new(Cell::new(0)),
            seen: Rc::new(Cell::new(0)),
        }
    }

    /// A second handle on the same file: shared bytes, private read position.
    fn reopen(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
            generation: Rc::clone(&self.generation),
            seen: Rc::new(Cell::new(self.generation.get())),
        }
    }
}

impl Storage for FileHandle {
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
        self.inner.borrow_mut().commit()?;
        self.generation.set(self.generation.get() + 1);
        // A handle never has to refresh onto its own commit: it already holds
        // the state it just wrote.
        self.seen.set(self.generation.get());
        Ok(())
    }

    fn refresh(&mut self) -> inlaysql_core::Result<bool> {
        if self.seen.get() == self.generation.get() {
            return Ok(false);
        }
        self.seen.set(self.generation.get());
        Ok(true)
    }

    fn rollback(&mut self) -> inlaysql_core::Result<()> {
        self.inner.borrow_mut().rollback()
    }
}

// ------------------------------------------------------ counting backends

/// How many documents and embeddings one engine has fed to its index
/// backends since it was opened.
#[derive(Clone, Default)]
struct IndexWork {
    documents: Rc<Cell<usize>>,
    embeddings: Rc<Cell<usize>>,
}

impl IndexWork {
    fn documents(&self) -> usize {
        self.documents.get()
    }

    fn embeddings(&self) -> usize {
        self.embeddings.get()
    }
}

struct CountingFullText {
    inner: MemFullTextIndex,
    work: IndexWork,
}

impl FullTextIndex for CountingFullText {
    fn insert(&mut self, id: RowId, text: &str) -> inlaysql_core::Result<()> {
        self.work.documents.set(self.work.documents.get() + 1);
        self.inner.insert(id, text)
    }

    fn remove(&mut self, id: RowId) -> inlaysql_core::Result<()> {
        self.inner.remove(id)
    }

    fn commit(&mut self) -> inlaysql_core::Result<()> {
        self.inner.commit()
    }

    fn search(
        &self,
        query: &str,
        k: usize,
        filter: Option<&RowFilter>,
    ) -> inlaysql_core::Result<Vec<Scored>> {
        self.inner.search(query, k, filter)
    }

    fn save(&self) -> Option<Vec<u8>> {
        self.inner.save()
    }

    fn load(&mut self, bytes: &[u8]) -> inlaysql_core::Result<()> {
        self.inner.load(bytes)
    }
}

struct CountingVector {
    inner: BruteForceVectorIndex,
    work: IndexWork,
}

impl VectorIndex for CountingVector {
    fn insert(&mut self, id: RowId, embedding: &[f32]) -> inlaysql_core::Result<()> {
        self.work.embeddings.set(self.work.embeddings.get() + 1);
        self.inner.insert(id, embedding)
    }

    fn remove(&mut self, id: RowId) -> inlaysql_core::Result<()> {
        self.inner.remove(id)
    }

    fn commit(&mut self) -> inlaysql_core::Result<()> {
        self.inner.commit()
    }

    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&RowFilter>,
    ) -> inlaysql_core::Result<Vec<Scored>> {
        self.inner.search(query, k, filter)
    }

    fn save(&self) -> Option<Vec<u8>> {
        self.inner.save()
    }

    fn load(&mut self, bytes: &[u8]) -> inlaysql_core::Result<()> {
        self.inner.load(bytes)
    }
}

#[derive(Clone, Default)]
struct CountingFactory {
    work: IndexWork,
}

impl IndexFactory for CountingFactory {
    fn full_text(
        &self,
        _table: &str,
        _column: &str,
    ) -> inlaysql_core::Result<Box<dyn FullTextIndex>> {
        Ok(Box::new(CountingFullText {
            inner: MemFullTextIndex::new(),
            work: self.work.clone(),
        }))
    }

    fn vector(
        &self,
        _table: &str,
        _column: &str,
        dim: usize,
    ) -> inlaysql_core::Result<Box<dyn VectorIndex>> {
        Ok(Box::new(CountingVector {
            inner: BruteForceVectorIndex::new(dim),
            work: self.work.clone(),
        }))
    }
}

// ------------------------------------------------------------- the fixture

const ROWS: i64 = 40;

/// A deterministic clock, so nothing here depends on wall time.
fn clock() -> Box<dyn Clock> {
    Box::new(LogicalClock::new())
}

/// Open one handle on `file` with its own index-work counter.
fn open(file: &FileHandle) -> (Engine, IndexWork) {
    let factory = CountingFactory::default();
    let work = factory.work.clone();
    let engine = Engine::open(Box::new(file.clone()), Box::new(factory), clock()).expect("open");
    (engine, work)
}

fn embedding(id: i64) -> Vec<f32> {
    let angle = id as f32 * 0.37;
    vec![angle.sin(), angle.cos(), 0.25]
}

/// A file holding an indexed `docs` table and an unindexed `events` table.
fn seeded() -> FileHandle {
    let file = FileHandle::create();
    let (mut engine, _) = open(&file);
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
    engine
        .execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, n INTEGER)",
            &[],
        )
        .unwrap();
    for id in 1..=ROWS {
        engine
            .execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Text(format!("embedded document number {id} about storage").into()),
                    Value::Vector(embedding(id)),
                ],
            )
            .unwrap();
    }
    engine.checkpoint().unwrap();
    file
}

fn search(engine: &mut Engine, term: &str) -> Vec<i64> {
    engine
        .query(
            "SELECT id, bm25_score(body, ?) AS score FROM docs
             ORDER BY score DESC LIMIT 5",
            &[Value::Text(term.to_string().into())],
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

fn nearest(engine: &mut Engine, probe: &[f32]) -> Vec<i64> {
    engine
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs
             ORDER BY score DESC LIMIT 3",
            &[Value::Vector(probe.to_vec())],
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

// ------------------------------------------------------------------ tests

#[test]
fn a_foreign_commit_to_another_table_re_indexes_nothing() {
    let file = seeded();
    let (mut reader, work) = open(&file);
    let (mut writer, _) = open(&file.reopen());

    // Warm the reader: whatever restoring the indexes on open costs, it has
    // been paid before the counter is read.
    assert!(!search(&mut reader, "embedded").is_empty());
    let indexed = work.documents();
    let embedded = work.embeddings();

    writer
        .execute("INSERT INTO events (id, n) VALUES (1, 1)", &[])
        .unwrap();

    // The reader's next statement adopts the writer's commit.
    assert!(!search(&mut reader, "embedded").is_empty());

    assert_eq!(
        work.documents(),
        indexed,
        "a commit to a table with no retrieval index re-indexed {} documents",
        work.documents() - indexed
    );
    assert_eq!(
        work.embeddings(),
        embedded,
        "a commit to a table with no retrieval index re-indexed {} embeddings",
        work.embeddings() - embedded
    );
}

#[test]
fn a_foreign_commit_re_indexes_only_the_row_it_changed() {
    let file = seeded();
    let (mut reader, work) = open(&file);
    let (mut writer, _) = open(&file.reopen());

    assert!(!search(&mut reader, "embedded").is_empty());
    let indexed = work.documents();

    writer
        .execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(ROWS + 1),
                Value::Text("a late arriving embedded manuscript".into()),
                Value::Vector(embedding(ROWS + 1)),
            ],
        )
        .unwrap();

    let hits = search(&mut reader, "manuscript");

    assert_eq!(
        hits,
        vec![ROWS + 1],
        "the reader did not see the row the writer committed"
    );
    assert_eq!(
        work.documents() - indexed,
        1,
        "one foreign insert cost {} re-indexed documents, not one",
        work.documents() - indexed
    );
}

#[test]
fn a_foreign_update_and_delete_are_reflected_exactly() {
    let file = seeded();
    let (mut reader, _work) = open(&file);
    let (mut writer, _) = open(&file.reopen());

    assert!(search(&mut reader, "embedded").contains(&1));

    writer
        .execute(
            "UPDATE docs SET body = ? WHERE id = ?",
            &[
                Value::Text("rewritten into an unrelated marmalade".into()),
                Value::Integer(1),
            ],
        )
        .unwrap();
    writer
        .execute("DELETE FROM docs WHERE id = ?", &[Value::Integer(2)])
        .unwrap();

    // The rewritten row is findable under its new text and gone from its old
    // one; the deleted row is gone from both. A stale posting left behind by a
    // catch-up that skipped the row would show up as either.
    assert_eq!(search(&mut reader, "marmalade"), vec![1]);
    assert!(
        !search(&mut reader, "embedded").contains(&1),
        "the old text of a foreign-updated row is still in the index"
    );
    assert!(
        !search(&mut reader, "embedded").contains(&2),
        "a foreign-deleted row is still in the index"
    );

    // And the same for the vector half: the deleted row must not come back as
    // a neighbour of its own embedding.
    let probe = embedding(2);
    assert!(
        !nearest(&mut reader, &probe).contains(&2),
        "a foreign-deleted row is still in the vector index"
    );
}

#[test]
fn a_foreign_commit_and_a_rebuild_leave_the_same_index() {
    let file = seeded();
    let (mut reader, _) = open(&file);
    let (mut writer, _) = open(&file.reopen());

    assert!(!search(&mut reader, "embedded").is_empty());
    for id in 1..=6i64 {
        writer
            .execute(
                "UPDATE docs SET body = ?, embedding = ? WHERE id = ?",
                &[
                    Value::Text(format!("revision {id} of a stored embedded record").into()),
                    Value::Vector(embedding(id + 100)),
                    Value::Integer(id),
                ],
            )
            .unwrap();
    }
    writer
        .execute("DELETE FROM docs WHERE id = 7", &[])
        .unwrap();

    let caught_up = search(&mut reader, "revision stored embedded");
    let neighbours = nearest(&mut reader, &embedding(103));

    // A handle opened now has no choice but to build from the rows, so it is
    // the oracle for what the caught-up index should contain.
    let (mut fresh, _) = open(&file.reopen());
    assert_eq!(
        caught_up,
        search(&mut fresh, "revision stored embedded"),
        "the caught-up full-text index disagrees with one built from the rows"
    );
    assert_eq!(
        neighbours,
        nearest(&mut fresh, &embedding(103)),
        "the caught-up vector index disagrees with one built from the rows"
    );
}

#[test]
fn a_foreign_schema_change_still_rebuilds() {
    let file = seeded();
    let (mut reader, _) = open(&file);
    let (mut writer, _) = open(&file.reopen());

    assert!(!search(&mut reader, "embedded").is_empty());

    // A new index exists nowhere in the reader's memory, so there is nothing
    // to catch up: the reader has to build it from the rows.
    writer
        .execute(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, note TEXT)",
            &[],
        )
        .unwrap();
    writer
        .execute("CREATE INDEX notes_note ON notes (note)", &[])
        .unwrap();
    writer
        .execute(
            "INSERT INTO notes (id, note) VALUES (1, 'a foreign marmalade note')",
            &[],
        )
        .unwrap();

    let hits = reader
        .query(
            "SELECT id, bm25_score(note, ?) AS score FROM notes ORDER BY score DESC LIMIT 3",
            &[Value::Text("marmalade".into())],
        )
        .unwrap();
    assert_eq!(
        hits.rows.first().map(|row| row[0].clone()),
        Some(Value::Integer(1))
    );
}
