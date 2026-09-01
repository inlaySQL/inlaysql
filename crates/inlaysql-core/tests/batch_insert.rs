//! A multi-row `INSERT` writes exactly what the same rows written one
//! statement at a time write.
//!
//! The write path resolves per *statement* what it used to resolve per row:
//! the table's index declarations. That is only safe because nothing can
//! change them between two rows of one statement, and the way it would fail
//! is not a compile error — a batch would maintain a set it resolved too
//! early, or carry state from a row that was rejected into the row after it.
//!
//! So every test here is a *comparison*, not an assertion about one engine.
//! Two engines get the same schema and the same rows; one takes them in a
//! single `VALUES` list, the other one statement per row. The stored rows,
//! every B-tree probe, the BM25 ranking and the ANN ranking all have to
//! agree — and where a constraint rejects a row, the error text has to agree
//! too, and the batch has to leave nothing at all behind.

use inlaysql_core::{mem, Engine, Error, Value};

/// Enough rows that the batch path is a batch, and more than one page of
/// them.
const ROWS: usize = 120;

/// The `body` vocabulary. Small on purpose: a term has to appear in many rows
/// for BM25 to rank them against each other rather than return each row once.
const WORDS: [&str; 6] = [
    "storage",
    "vector",
    "planner",
    "recovery",
    "catalog",
    "checkpoint",
];

struct Row {
    id: i64,
    slug: String,
    bucket: i64,
    title: String,
    body: String,
    embedding: Vec<f32>,
}

/// The corpus, derived from the row's own ordinal so both engines and every
/// test build byte-identical rows.
fn corpus(count: usize) -> Vec<Row> {
    (0..count)
        .map(|i| {
            let f = i as f32;
            Row {
                id: i as i64 + 1,
                slug: format!("slug-{i:04}"),
                // Seven buckets over 120 rows: every B-tree probe below
                // matches many rows, so a probe that read the wrong run of
                // entries returns a visibly different set rather than one
                // row's worth of difference.
                bucket: (i % 7) as i64,
                title: format!("title {i:04}"),
                body: format!(
                    "{} {} row {i}",
                    WORDS[i % WORDS.len()],
                    WORDS[(i / WORDS.len()) % WORDS.len()]
                ),
                embedding: vec![
                    (f % 7.0) / 7.0,
                    (f % 5.0) / 5.0,
                    (f % 3.0) / 3.0,
                    1.0 - (f % 11.0) / 11.0,
                ],
            }
        })
        .collect()
}

fn engine() -> Engine {
    let mut engine = mem::engine().expect("open in-memory engine");
    // One of each thing the insert path maintains per row: a `UNIQUE`
    // constraint (its own B-tree index), a `CHECK`, a secondary B-tree index
    // over an integer, a second one over text (`USING BTREE`, since a bare
    // `CREATE INDEX` on `TEXT` means BM25 here), a full-text index and a
    // vector index.
    run(
        &mut engine,
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, slug TEXT UNIQUE, \
         bucket INTEGER CHECK (bucket >= 0), title TEXT, body TEXT, embedding VECTOR(4))",
    );
    run(&mut engine, "CREATE INDEX docs_bucket ON docs (bucket)");
    run(
        &mut engine,
        "CREATE INDEX docs_title ON docs (title) USING BTREE",
    );
    run(
        &mut engine,
        "CREATE INDEX docs_body ON docs (body) USING FULLTEXT",
    );
    run(
        &mut engine,
        "CREATE INDEX docs_embedding ON docs (embedding)",
    );
    engine
}

fn run(engine: &mut Engine, sql: &str) {
    engine
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"));
}

const COLUMNS: &str = "(id, slug, bucket, title, body, embedding)";

/// One `INSERT` naming `count` rows, all placeholders.
fn batch_statement(count: usize) -> String {
    let mut sql = format!("INSERT INTO docs {COLUMNS} VALUES ");
    for i in 0..count {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str("(?, ?, ?, ?, ?, ?)");
    }
    sql
}

fn bind(row: &Row) -> Vec<Value> {
    vec![
        Value::Integer(row.id),
        Value::Text(row.slug.clone().into()),
        Value::Integer(row.bucket),
        Value::Text(row.title.clone().into()),
        Value::Text(row.body.clone().into()),
        Value::Vector(row.embedding.clone()),
    ]
}

fn bind_all(rows: &[Row]) -> Vec<Value> {
    rows.iter().flat_map(bind).collect()
}

/// Every row in one statement.
fn insert_batch(engine: &mut Engine, rows: &[Row]) -> Result<(), Error> {
    engine
        .execute(&batch_statement(rows.len()), &bind_all(rows))
        .map(|_| ())
}

/// The same rows, one statement each — the behaviour the batch has to match.
fn insert_one_at_a_time(engine: &mut Engine, rows: &[Row]) -> Result<(), Error> {
    let sql = format!("INSERT INTO docs {COLUMNS} VALUES (?, ?, ?, ?, ?, ?)");
    for row in rows {
        engine.execute(&sql, &bind(row))?;
    }
    Ok(())
}

/// Everything one row's index maintenance can be observed through: the stored
/// rows, a probe per B-tree index, the BM25 ranking and the ANN ranking.
///
/// Ordered and rendered as text so a mismatch names the row it is about
/// rather than reporting that two `Vec<Value>`s differ.
fn observed(engine: &mut Engine) -> Vec<(&'static str, Vec<Vec<String>>)> {
    vec![
        ("rows", rows(engine, "SELECT * FROM docs ORDER BY id", &[])),
        (
            "unique probe",
            rows(
                engine,
                "SELECT id, bucket FROM docs WHERE slug = 'slug-0042'",
                &[],
            ),
        ),
        (
            "btree equality probe",
            rows(
                engine,
                "SELECT id FROM docs WHERE bucket = 3 ORDER BY id",
                &[],
            ),
        ),
        (
            "btree range probe",
            rows(
                engine,
                "SELECT id, bucket FROM docs WHERE bucket >= 2 AND bucket < 5 ORDER BY id",
                &[],
            ),
        ),
        (
            "text btree probe",
            rows(
                engine,
                "SELECT id FROM docs WHERE title = 'title 0077'",
                &[],
            ),
        ),
        (
            "bm25",
            rows(
                engine,
                "SELECT id, bm25_score(body, ?) AS score FROM docs ORDER BY score DESC, id \
                 LIMIT 12",
                &[Value::Text("storage recovery".to_string().into())],
            ),
        ),
        (
            "ann",
            rows(
                engine,
                "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC, \
                 id LIMIT 12",
                &[Value::Vector(vec![0.5, 0.25, 0.75, 0.125])],
            ),
        ),
    ]
}

fn rows(engine: &mut Engine, sql: &str, params: &[Value]) -> Vec<Vec<String>> {
    engine
        .query(sql, params)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect())
        .collect()
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => format!("i:{i}"),
        Value::Real(r) => format!("f:{r}"),
        Value::Text(t) => format!("t:{t}"),
        Value::Blob(b) => format!("b:{b:?}"),
        // The floats themselves, not the length: a vector column that lost
        // its value would still have the right dimension.
        Value::Vector(v) => format!("v:{v:?}"),
    }
}

/// The `detail` column of every `EXPLAIN` node, joined.
fn plan(engine: &mut Engine, sql: &str, params: &[Value]) -> String {
    engine
        .query(&format!("EXPLAIN {sql}"), params)
        .unwrap_or_else(|e| panic!("`EXPLAIN {sql}`: {e}"))
        .rows
        .iter()
        .map(|row| render(&row[2]))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn refuse(engine: &mut Engine, sql: &str, params: &[Value]) -> Error {
    engine
        .execute(sql, params)
        .expect_err("the statement was accepted")
}

// ------------------------------------------------------------------ THE PAIR

#[test]
fn a_batch_insert_writes_what_one_statement_per_row_writes() {
    let corpus = corpus(ROWS);
    let mut batched = engine();
    let mut single = engine();

    insert_batch(&mut batched, &corpus).expect("batch insert");
    insert_one_at_a_time(&mut single, &corpus).expect("row-at-a-time insert");

    assert_eq!(observed(&mut batched), observed(&mut single));

    // The comparison alone is symmetric, and that is its weakness: a change
    // that emptied an index on *both* paths would still make the two agree.
    // So the same observations are checked against the corpus itself.
    let observed = observed(&mut batched);
    let section = |label: &str| {
        observed
            .iter()
            .find(|(name, _)| *name == label)
            .unwrap_or_else(|| panic!("no `{label}` section"))
            .1
            .clone()
    };
    assert_eq!(section("rows").len(), ROWS);
    assert_eq!(ids(&section("unique probe")), vec![43]);
    assert_eq!(ids(&section("text btree probe")), vec![78]);
    assert_eq!(
        ids(&section("btree equality probe")),
        expected_ids(&corpus, |row| row.bucket == 3)
    );
    assert_eq!(
        ids(&section("btree range probe")),
        expected_ids(&corpus, |row| (2..5).contains(&row.bucket))
    );

    // A BM25 index that had missed rows would still return twelve of them, so
    // the assertion is that the twelve it ranked highest are twelve that
    // actually hold a query term.
    let ranked = ids(&section("bm25"));
    assert_eq!(ranked.len(), 12);
    for id in ranked {
        let body = &corpus[id as usize - 1].body;
        assert!(
            body.contains("storage") || body.contains("recovery"),
            "row {id} (`{body}`) outranked every row holding a query term"
        );
    }
    assert_eq!(ids(&section("ann")).len(), 12);
}

/// The `id` column of a rendered result, which every section here projects
/// first.
fn ids(rows: &[Vec<String>]) -> Vec<i64> {
    rows.iter()
        .map(|row| {
            row[0]
                .strip_prefix("i:")
                .unwrap_or_else(|| panic!("`{}` is not an integer id", row[0]))
                .parse()
                .expect("id")
        })
        .collect()
}

fn expected_ids(corpus: &[Row], keep: impl Fn(&Row) -> bool) -> Vec<i64> {
    corpus
        .iter()
        .filter(|row| keep(row))
        .map(|row| row.id)
        .collect()
}

/// The probes above are only evidence about index maintenance if they are
/// actually served by the indexes. Asserted separately so the comparison test
/// keeps comparing rather than starting to assert access paths.
#[test]
fn the_probes_really_do_read_the_indexes() {
    let mut engine = engine();
    insert_batch(&mut engine, &corpus(ROWS)).expect("batch insert");

    for (sql, index) in [
        ("SELECT id FROM docs WHERE slug = 'slug-0042'", "docs_slug"),
        ("SELECT id FROM docs WHERE bucket = 3", "docs_bucket"),
        (
            "SELECT id FROM docs WHERE bucket >= 2 AND bucket < 5",
            "docs_bucket",
        ),
        (
            "SELECT id FROM docs WHERE title = 'title 0077'",
            "docs_title",
        ),
    ] {
        let detail = plan(&mut engine, sql, &[]);
        assert!(
            detail.contains("USING INDEX") && detail.contains(index),
            "`{sql}` planned as `{detail}`, not a search of `{index}`"
        );
    }

    let ann = plan(
        &mut engine,
        "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC LIMIT 12",
        &[Value::Vector(vec![0.5, 0.25, 0.75, 0.125])],
    );
    assert!(
        ann.contains("VECTOR INDEX"),
        "the ANN query planned as `{ann}`"
    );
}

// -------------------------------------------------------------- THE CONFLICT

#[test]
fn a_unique_violation_mid_batch_leaves_the_batch_behind_entirely() {
    let mut corpus = corpus(ROWS);
    // Row 60 collides with row 10 on `slug`, and on nothing else — so what
    // rejects it is the `UNIQUE` index the batch has been writing into, using
    // the very index set the statement resolved before its first row.
    corpus[60].slug = corpus[10].slug.clone();

    let mut batched = engine();
    let batch_error = refuse(
        &mut batched,
        &batch_statement(corpus.len()),
        &bind_all(&corpus),
    );

    // The same collision, reached one statement at a time, reports the same
    // thing — the batch must not report a different constraint, or a
    // different row's.
    let mut single = engine();
    let single_error = insert_one_at_a_time(&mut single, &corpus)
        .expect_err("the 61st single-row insert was accepted");
    assert_eq!(batch_error.to_string(), single_error.to_string());
    assert!(
        batch_error.to_string().contains("slug"),
        "expected the `slug` constraint, got `{batch_error}`"
    );

    // Nothing of the batch survives — not the 60 rows that were written
    // before the collision, and not their index entries.
    let empty: Vec<Vec<String>> = Vec::new();
    for (label, observed) in observed(&mut batched) {
        assert_eq!(observed, empty, "`{label}` survived the aborted batch");
    }

    // And the state the aborted statement resolved once does not leak into
    // the next one: the same engine, given the corpus with the collision
    // repaired, must land exactly where a fresh engine does.
    let mut repaired = corpus;
    repaired[60].slug = "slug-0060".to_string();
    insert_batch(&mut batched, &repaired).expect("batch insert after the abort");

    let mut reference = engine();
    insert_one_at_a_time(&mut reference, &repaired).expect("row-at-a-time insert");
    assert_eq!(observed(&mut batched), observed(&mut reference));
}

#[test]
fn a_check_violation_mid_batch_leaves_the_batch_behind_entirely() {
    let mut corpus = corpus(ROWS);
    corpus[60].bucket = -1;

    let mut batched = engine();
    let batch_error = refuse(
        &mut batched,
        &batch_statement(corpus.len()),
        &bind_all(&corpus),
    );

    let mut single = engine();
    let single_error = insert_one_at_a_time(&mut single, &corpus)
        .expect_err("the 61st single-row insert was accepted");
    assert_eq!(batch_error.to_string(), single_error.to_string());
    assert!(
        batch_error.to_string().contains("CHECK"),
        "expected the `CHECK` constraint, got `{batch_error}`"
    );

    let empty: Vec<Vec<String>> = Vec::new();
    for (label, observed) in observed(&mut batched) {
        assert_eq!(observed, empty, "`{label}` survived the aborted batch");
    }
}

/// A conflict the statement *answers* rather than aborts on: the rows it
/// skips must leave no index entry, and the rows after them must still be
/// indexed.
#[test]
fn a_batch_that_ignores_its_conflicts_indexes_only_what_it_wrote() {
    let corpus = corpus(ROWS);
    let sql = batch_statement(corpus.len()).replacen("INSERT INTO", "INSERT OR IGNORE INTO", 1);

    // Half the batch is already there, so half of it is skipped.
    let mut batched = engine();
    insert_one_at_a_time(&mut batched, &corpus[..60]).expect("the rows already present");
    batched
        .execute(&sql, &bind_all(&corpus))
        .expect("batch insert or ignore");

    let mut single = engine();
    insert_one_at_a_time(&mut single, &corpus[..60]).expect("the rows already present");
    let one = format!("INSERT OR IGNORE INTO docs {COLUMNS} VALUES (?, ?, ?, ?, ?, ?)");
    for row in &corpus {
        single.execute(&one, &bind(row)).expect("insert or ignore");
    }

    assert_eq!(observed(&mut batched), observed(&mut single));
    assert_eq!(observed(&mut batched)[0].1.len(), ROWS);
}

/// `DO UPDATE` reaches the write path that moves a row *and* its entries, and
/// it reaches it from inside the batch's own loop.
#[test]
fn a_batch_upsert_rewrites_what_one_statement_per_row_rewrites() {
    let original = corpus(ROWS);
    let mut updated = corpus(ROWS);
    for row in &mut updated {
        row.id += ROWS as i64;
        row.bucket = (row.bucket + 3) % 7;
        row.title = format!("re{}", row.title);
        row.body = format!("rewritten {}", row.body);
    }
    let clause = " ON CONFLICT (slug) DO UPDATE SET bucket = excluded.bucket, \
                   title = excluded.title, body = excluded.body, embedding = excluded.embedding";

    let mut batched = engine();
    insert_batch(&mut batched, &original).expect("the rows to be upserted over");
    batched
        .execute(
            &format!("{}{clause}", batch_statement(updated.len())),
            &bind_all(&updated),
        )
        .expect("batch upsert");

    let mut single = engine();
    insert_batch(&mut single, &original).expect("the rows to be upserted over");
    let one = format!("INSERT INTO docs {COLUMNS} VALUES (?, ?, ?, ?, ?, ?){clause}");
    for row in &updated {
        single.execute(&one, &bind(row)).expect("upsert");
    }

    assert_eq!(observed(&mut batched), observed(&mut single));
    assert_eq!(observed(&mut batched)[0].1.len(), ROWS);
}

// ---------------------------------------------------------- UPDATE AND DELETE

/// The same hoist covers `UPDATE`'s and `DELETE`'s row loops, so the same
/// comparison covers them: many rows in one statement against one row per
/// statement.
#[test]
fn a_multi_row_update_and_delete_match_one_statement_per_row() {
    let corpus = corpus(ROWS);

    let mut batched = engine();
    insert_batch(&mut batched, &corpus).expect("batch insert");
    run(
        &mut batched,
        "UPDATE docs SET bucket = bucket + 1, body = 'rewritten ' || body WHERE bucket < 4",
    );
    run(&mut batched, "DELETE FROM docs WHERE bucket = 5");

    let mut single = engine();
    insert_batch(&mut single, &corpus).expect("batch insert");
    for row in &corpus {
        single
            .execute(
                "UPDATE docs SET bucket = bucket + 1, body = 'rewritten ' || body \
                 WHERE id = ? AND bucket < 4",
                &[Value::Integer(row.id)],
            )
            .expect("update");
    }
    for row in &corpus {
        single
            .execute(
                "DELETE FROM docs WHERE id = ? AND bucket = 5",
                &[Value::Integer(row.id)],
            )
            .expect("delete");
    }

    assert_eq!(observed(&mut batched), observed(&mut single));
    assert!(observed(&mut batched)[0].1.len() < ROWS);
}
