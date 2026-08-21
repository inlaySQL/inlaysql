//! Multi-column full-text (`FullText`) retrieval indexes.
//!
//! `CREATE INDEX idx ON docs (title, body) USING FULLTEXT` is MySQL's
//! `FULLTEXT(title, body)`: one combined BM25 score over the concatenation of
//! every named column's text, so a query term that only appears in one column
//! still ranks the row. These tests check that against an independently
//! constructed [`Bm25Index`] fed the same concatenated corpus by hand — the
//! same rigor `hybrid_query.rs` and `bm25.rs`'s own tests use — not just
//! "the query returned something".
//!
//! `vector_score`/multi-column `VECTOR` indexes are deliberately out of scope
//! here: two embedding columns are generally two different vector spaces, and
//! there is no single defensible meaning for one ANN graph over both (see
//! `Catalog::create_index`), so this stayed BM25-only.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql_core::bm25::Bm25Index;
use inlaysql_core::mem::{self, LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::row::RowBuf;
use inlaysql_core::traits::{FullTextIndex, RowId, Storage};
use inlaysql_core::{Engine, ResultSet, Value};

/// A `MemStorage` several engines can share, so a database can be "reopened"
/// — the same pattern `index_persistence.rs` uses.
#[derive(Clone, Default)]
struct SharedStorage {
    inner: Rc<RefCell<MemStorage>>,
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

/// (title, body). "quantum" appears only in doc 2's body; "falcon" only in
/// doc 3's title; "database" appears in both doc 1's title and doc 3's body,
/// so it exercises a row that contributes from both columns.
const DOCS: &[(&str, &str)] = &[
    (
        "Rust Database Engine",
        "A fast embedded storage layer written from scratch",
    ),
    (
        "Cooking With Cast Iron",
        "Skillet recipes for cornbread and quantum physics",
    ),
    (
        "Falcon Heavy Launch",
        "A database of every payload ever carried into orbit",
    ),
    (
        "Web Framework Basics",
        "Building sites with routes and middleware",
    ),
];

fn create_docs_table(engine: &mut Engine) {
    engine
        .execute(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, title TEXT, body TEXT)",
            &[],
        )
        .expect("create table");
}

fn insert_docs(engine: &mut Engine) {
    for (id, (title, body)) in DOCS.iter().enumerate() {
        engine
            .execute(
                "INSERT INTO docs (id, title, body) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id as i64 + 1),
                    Value::Text(title.to_string()),
                    Value::Text(body.to_string()),
                ],
            )
            .expect("insert");
    }
}

/// A table with a genuine multi-column `FullText` index over (title, body),
/// fully populated.
fn seeded_multi_column() -> SharedStorage {
    let storage = SharedStorage::default();
    let mut engine = open(&storage);
    create_docs_table(&mut engine);
    engine
        .execute(
            "CREATE INDEX docs_search ON docs (title, body) USING FULLTEXT",
            &[],
        )
        .expect("create multi-column full-text index");
    insert_docs(&mut engine);
    storage
}

/// The independent oracle: a `Bm25Index` built directly from the same rows,
/// concatenating title and body with a space exactly as the engine's own
/// `concatenated_full_text` is documented to.
fn oracle() -> Bm25Index {
    let mut index = Bm25Index::new();
    for (id, (title, body)) in DOCS.iter().enumerate() {
        index
            .insert(id as u64 + 1, &format!("{title} {body}"))
            .unwrap();
    }
    index
}

fn multi_column_query(engine: &mut Engine, sql: &str, term: &str) -> ResultSet {
    engine
        .query(sql, &[Value::Text(term.to_string())])
        .unwrap_or_else(|error| panic!("query `{sql}` failed: {error}"))
}

fn ids_and_scores(result: &ResultSet) -> Vec<(i64, f32)> {
    result
        .rows
        .iter()
        .map(|row| {
            let id = row[0].as_i64().expect("id column");
            let score = row[1].as_f64().expect("score column") as f32;
            (id, score)
        })
        .collect()
}

#[test]
fn a_term_that_only_appears_in_one_column_still_ranks_the_row() {
    let mut engine = open(&seeded_multi_column());

    // "quantum" is only in doc 2's body; nothing in any title mentions it.
    let hits = multi_column_query(
        &mut engine,
        "SELECT id, bm25_score(title, body, ?) AS score FROM docs \
         ORDER BY score DESC LIMIT 10",
        "quantum",
    );
    assert_eq!(
        ids_and_scores(&hits)[0].0,
        2,
        "a body-only term did not win the ranking: {:?}",
        ids_and_scores(&hits)
    );

    // "falcon" is only in doc 3's title; nothing in any body mentions it.
    let hits = multi_column_query(
        &mut engine,
        "SELECT id, bm25_score(title, body, ?) AS score FROM docs \
         ORDER BY score DESC LIMIT 10",
        "falcon",
    );
    assert_eq!(
        ids_and_scores(&hits)[0].0,
        3,
        "a title-only term did not win the ranking: {:?}",
        ids_and_scores(&hits)
    );
}

#[test]
fn relevance_matches_a_hand_built_bm25_index_over_the_concatenated_text() {
    let mut engine = open(&seeded_multi_column());
    let oracle = oracle();

    for term in ["quantum", "falcon", "database", "recipes", "unknown-term"] {
        let hits = multi_column_query(
            &mut engine,
            "SELECT id, bm25_score(title, body, ?) AS score FROM docs \
             ORDER BY score DESC LIMIT 10",
            term,
        );
        let engine_hits = ids_and_scores(&hits);

        let oracle_hits: Vec<(i64, f32)> = oracle
            .search(term, 10)
            .unwrap()
            .into_iter()
            .map(|scored| (scored.id as i64, scored.score))
            .collect();

        assert_eq!(
            engine_hits, oracle_hits,
            "query `{term}` diverged from the hand-built oracle"
        );
    }
}

#[test]
fn column_order_in_the_call_does_not_change_the_answer() {
    let mut engine = open(&seeded_multi_column());

    for term in ["quantum", "falcon", "database"] {
        let forward = multi_column_query(
            &mut engine,
            "SELECT id, bm25_score(title, body, ?) AS score FROM docs \
             ORDER BY score DESC LIMIT 10",
            term,
        );
        let reversed = multi_column_query(
            &mut engine,
            "SELECT id, bm25_score(body, title, ?) AS score FROM docs \
             ORDER BY score DESC LIMIT 10",
            term,
        );
        assert_eq!(
            ids_and_scores(&forward),
            ids_and_scores(&reversed),
            "naming the columns in a different order changed the answer for `{term}`"
        );
    }
}

#[test]
fn a_coexisting_single_column_index_is_still_the_one_a_single_column_call_uses() {
    // `body` gets both a single-column index of its own and is also part of
    // the multi-column (title, body) index — the catalog now allows a column
    // to be named by more than one `FullText` index at once.
    let storage = SharedStorage::default();
    let mut engine = open(&storage);
    create_docs_table(&mut engine);
    engine
        .execute("CREATE INDEX docs_body ON docs (body)", &[])
        .expect("create single-column index");
    engine
        .execute(
            "CREATE INDEX docs_search ON docs (title, body) USING FULLTEXT",
            &[],
        )
        .expect("create multi-column index");
    insert_docs(&mut engine);

    // "falcon" is title-only. The single-column `body` index has never heard
    // of it and must answer with nothing, even though the combined index
    // ranks doc 3 top for the same term.
    let single = multi_column_query(
        &mut engine,
        "SELECT id, bm25_score(body, ?) AS score FROM docs ORDER BY score DESC LIMIT 10",
        "falcon",
    );
    assert!(
        single.rows.is_empty(),
        "the single-column `body` index found a title-only term: {:?}",
        ids_and_scores(&single)
    );

    let combined = multi_column_query(
        &mut engine,
        "SELECT id, bm25_score(title, body, ?) AS score FROM docs \
         ORDER BY score DESC LIMIT 10",
        "falcon",
    );
    assert_eq!(ids_and_scores(&combined)[0].0, 3);
}

#[test]
fn a_bare_multi_column_create_index_is_still_a_b_tree_by_default() {
    // Regression: two `TEXT` columns named without `USING FULLTEXT` must keep
    // meaning what it has always meant here — an ordered scalar index — not
    // silently become a full-text one just because both columns are `TEXT`.
    let mut engine = mem::engine().expect("engine");
    create_docs_table(&mut engine);
    engine
        .execute("CREATE INDEX docs_pair ON docs (title, body)", &[])
        .expect("bare multi-column CREATE INDEX");
    insert_docs(&mut engine);

    let index = engine
        .catalog()
        .indexes_for("docs")
        .into_iter()
        .find(|index| index.name.eq_ignore_ascii_case("docs_pair"))
        .expect("index declared");
    assert_eq!(index.kind, inlaysql_core::catalog::IndexKind::BTree);

    // And there is still no full-text index over (title, body) to answer a
    // combined score with.
    let err = engine
        .query(
            "SELECT id, bm25_score(title, body, ?) AS score FROM docs",
            &[Value::Text("falcon".to_string())],
        )
        .unwrap_err();
    assert!(matches!(err, inlaysql_core::Error::Index(_)), "got {err}");
}

#[test]
fn using_fulltext_refuses_a_non_text_column() {
    let mut engine = mem::engine().expect("engine");
    engine
        .execute(
            "CREATE TABLE mixed (id INTEGER, title TEXT, score INTEGER)",
            &[],
        )
        .expect("create table");
    let err = engine
        .execute(
            "CREATE INDEX bad ON mixed (title, score) USING FULLTEXT",
            &[],
        )
        .unwrap_err();
    assert!(matches!(err, inlaysql_core::Error::Type(_)), "got {err}");
}

#[test]
fn a_restored_multi_column_index_answers_exactly_as_a_freshly_built_one() {
    // Build once, checkpoint, drop the engine and reopen on the same
    // storage — the restore path (`Engine::load_saved_indexes`) rather than
    // the rebuild-from-rows path.
    let storage = seeded_multi_column();
    {
        let mut engine = open(&storage);
        engine.checkpoint().expect("checkpoint");
    }
    let mut restored = open(&storage);

    // A fresh, never-persisted engine over the identical corpus is the
    // reference: if the restore round-tripped correctly, the two must answer
    // identically for every query below.
    let mut fresh = mem::engine().expect("engine");
    create_docs_table(&mut fresh);
    fresh
        .execute(
            "CREATE INDEX docs_search ON docs (title, body) USING FULLTEXT",
            &[],
        )
        .unwrap();
    insert_docs(&mut fresh);

    for term in ["quantum", "falcon", "database", "unknown-term"] {
        let sql = "SELECT id, bm25_score(title, body, ?) AS score FROM docs \
                    ORDER BY score DESC LIMIT 10";
        let restored_hits = ids_and_scores(&multi_column_query(&mut restored, sql, term));
        let fresh_hits = ids_and_scores(&multi_column_query(&mut fresh, sql, term));
        assert_eq!(
            restored_hits, fresh_hits,
            "restored index disagreed with a freshly built one for `{term}`"
        );
    }
}
