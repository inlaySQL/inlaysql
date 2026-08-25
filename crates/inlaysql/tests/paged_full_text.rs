//! The paged BM25 index, driven through a real on-disk `Database`.
//!
//! `inlaysql-core`'s `bm25_paged` unit tests and `bm25_paged_agreement.rs`
//! prove the index against a storage double. What is here is what only the
//! shipped crate can show: that the postings really are in the file, that they
//! survive being closed and reopened without a rebuild, that they commit with
//! the rows they describe rather than beside them, and — the property the
//! whole exercise rests on — that `bm25_score` returns **the same numbers** as
//! the in-memory backend it replaces, through the entire SQL path.

use std::fs;
use std::path::{Path, PathBuf};

use inlaysql::{Database, EngineOptions, FileDevice, Value};

/// A directory of our own, removed when the test ends.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("inlaysql-paged-fts-{name}"));
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

fn open(path: &Path, paged: bool) -> Database {
    Database::open_on_with_options(
        FileDevice::open(path).unwrap(),
        EngineOptions {
            paged_text_indexes: paged,
            ..EngineOptions::default()
        },
    )
    .unwrap()
}

/// A Zipf-ish body over a small vocabulary, so terms really do repeat across
/// documents and MaxScore has something to demote.
fn body(seed: u64) -> String {
    const VOCABULARY: [(u64, &str); 5] = [
        (48, "alpha"),
        (72, "beta"),
        (88, "gamma"),
        (96, "delta"),
        (100, "epsilon"),
    ];
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut roll = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut body = String::new();
    for _ in 0..3 + roll() % 14 {
        let draw = roll() % 100;
        let (_, word) = VOCABULARY
            .iter()
            .find(|(bound, _)| draw < *bound)
            .expect("the last bound is 100");
        body.push_str(word);
        body.push(' ');
    }
    body
}

fn create(db: &mut Database) {
    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])
        .unwrap();
}

fn load(db: &mut Database, rows: u64) {
    db.begin().unwrap();
    for id in 1..=rows {
        db.execute(
            "INSERT INTO docs VALUES (?, ?)",
            &[Value::Integer(id as i64), Value::Text(body(id).into())],
        )
        .unwrap();
        if id % 40 == 0 {
            db.commit().unwrap();
            db.begin().unwrap();
        }
    }
    db.commit().unwrap();
}

/// `(row id, score)` for a query, which is what the agreement is about — the
/// score and not merely the order.
fn ranked(db: &mut Database, query: &str, k: usize) -> Vec<(i64, f64)> {
    let rows = db
        .query(
            &format!(
                "SELECT id, bm25_score(body, ?) AS score FROM docs \
                 ORDER BY score DESC, id ASC LIMIT {k}"
            ),
            &[Value::Text(query.into())],
        )
        .unwrap();
    rows.rows
        .into_iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Integer(id), Value::Real(score)) => (*id, *score),
            other => panic!("unexpected row shape: {other:?}"),
        })
        .collect()
}

const QUERIES: [&str; 6] = [
    "alpha",
    "epsilon",
    "alpha epsilon",
    "beta gamma delta",
    "alpha beta gamma delta epsilon",
    "nonesuch",
];

/// The property the whole feature rests on: turning the option on may not
/// change a single number a query returns.
///
/// Compared as `f64` bits, because these came back through the engine's
/// `Value::Real`: a difference in the last place is exactly the failure a
/// paged backend with slightly different corpus statistics would produce, and
/// it would be invisible to any comparison with a tolerance in it.
#[test]
fn the_paged_and_in_memory_indexes_return_identical_scores() {
    let workspace = Workspace::new("identical");
    const ROWS: u64 = 1_200;

    let mut resident = open(&workspace.path("resident.inlay"), false);
    create(&mut resident);
    load(&mut resident, ROWS);

    let mut paged = open(&workspace.path("paged.inlay"), true);
    create(&mut paged);
    load(&mut paged, ROWS);

    for query in QUERIES {
        for k in [1usize, 5, 50, 1_200] {
            let expected = ranked(&mut resident, query, k);
            let got = ranked(&mut paged, query, k);
            assert_eq!(got.len(), expected.len(), "`{query}` at k={k}: hit count");
            for (left, right) in got.iter().zip(&expected) {
                assert_eq!(left.0, right.0, "`{query}` at k={k}: row ids diverged");
                assert_eq!(
                    left.1.to_bits(),
                    right.1.to_bits(),
                    "`{query}` at k={k}: row {} scored {} paged and {} resident",
                    left.0,
                    left.1,
                    right.1
                );
            }
        }
    }
}

/// Updates and deletes through SQL, which is where a paged postings list is
/// most likely to drift: the answer after churn has to be the answer a
/// database built from those same final rows would give.
#[test]
fn churn_through_sql_lands_where_a_fresh_build_lands() {
    let workspace = Workspace::new("churn");

    let mut churned = open(&workspace.path("churned.inlay"), true);
    create(&mut churned);
    load(&mut churned, 300);
    for round in 0..6u64 {
        for id in (1..=300).step_by(7) {
            churned
                .execute(
                    "UPDATE docs SET body = ? WHERE id = ?",
                    &[
                        Value::Text(body(id + round * 977).into()),
                        Value::Integer(id as i64),
                    ],
                )
                .unwrap();
        }
        churned
            .execute(
                "DELETE FROM docs WHERE id = ?",
                &[Value::Integer((round + 1) as i64 * 11)],
            )
            .unwrap();
    }

    // The same final rows, loaded once into a resident index.
    let mut fresh = open(&workspace.path("fresh.inlay"), false);
    create(&mut fresh);
    let rows = churned
        .query("SELECT id, body FROM docs ORDER BY id", &[])
        .unwrap();
    fresh.begin().unwrap();
    for (n, row) in rows.rows.iter().enumerate() {
        fresh
            .execute(
                "INSERT INTO docs VALUES (?, ?)",
                &[row[0].clone(), row[1].clone()],
            )
            .unwrap();
        if n % 40 == 39 {
            fresh.commit().unwrap();
            fresh.begin().unwrap();
        }
    }
    fresh.commit().unwrap();

    for query in QUERIES {
        assert_eq!(
            ranked(&mut churned, query, 20),
            ranked(&mut fresh, query, 20),
            "`{query}` diverged after churn"
        );
    }
}

/// The postings are in the file, so reopening does not rebuild them — and the
/// reopened handle answers exactly as the one that wrote them.
#[test]
fn the_postings_are_in_the_file_and_survive_a_reopen() {
    let workspace = Workspace::new("reopen");
    let path = workspace.path("db.inlay");

    let expected = {
        let mut db = open(&path, true);
        create(&mut db);
        load(&mut db, 400);
        QUERIES
            .iter()
            .map(|query| ranked(&mut db, query, 15))
            .collect::<Vec<_>>()
    };

    let mut reopened = open(&path, true);
    for (query, expected) in QUERIES.iter().zip(&expected) {
        assert_eq!(
            &ranked(&mut reopened, query, 15),
            expected,
            "reopened handle diverged on `{query}`"
        );
    }
}

/// A file written with the paged backend must open with the in-memory one and
/// answer the same, and the other way round. Neither is a format: the rows are
/// the source of truth and whichever index is asked for is derived from them.
#[test]
fn either_backend_can_open_a_file_the_other_wrote() {
    let workspace = Workspace::new("interchange");

    for (name, wrote_paged) in [("paged-first.inlay", true), ("resident-first.inlay", false)] {
        let path = workspace.path(name);
        let expected = {
            let mut db = open(&path, wrote_paged);
            create(&mut db);
            load(&mut db, 250);
            QUERIES
                .iter()
                .map(|query| ranked(&mut db, query, 10))
                .collect::<Vec<_>>()
        };

        let mut other = open(&path, !wrote_paged);
        for (query, expected) in QUERIES.iter().zip(&expected) {
            assert_eq!(
                &ranked(&mut other, query, 10),
                expected,
                "{name}: the other backend diverged on `{query}`"
            );
        }
    }
}

/// The index and the rows are one commit or neither. A rolled-back
/// transaction may not leave its documents findable, and the postings a
/// committed transaction wrote must be there for the next handle.
#[test]
fn the_postings_and_the_rows_commit_together() {
    let workspace = Workspace::new("atomic");
    let path = workspace.path("db.inlay");

    let mut db = open(&path, true);
    create(&mut db);
    load(&mut db, 50);

    db.begin().unwrap();
    db.execute(
        "INSERT INTO docs VALUES (?, ?)",
        &[Value::Integer(9_001), Value::Text("quokka".into())],
    )
    .unwrap();
    // Read-your-writes inside the transaction: the row is there for this
    // handle before anything is durable.
    assert_eq!(
        ranked(&mut db, "quokka", 5)
            .into_iter()
            .map(|hit| hit.0)
            .collect::<Vec<_>>(),
        vec![9_001]
    );
    db.rollback().unwrap();

    assert!(
        ranked(&mut db, "quokka", 5).is_empty(),
        "a rolled-back document stayed findable"
    );
    let reopened_after_rollback = {
        let mut other = open(&path, true);
        ranked(&mut other, "quokka", 5)
    };
    assert!(
        reopened_after_rollback.is_empty(),
        "a rolled-back document reached the file"
    );

    db.execute(
        "INSERT INTO docs VALUES (?, ?)",
        &[Value::Integer(9_002), Value::Text("quokka".into())],
    )
    .unwrap();
    let mut reopened = open(&path, true);
    assert_eq!(
        ranked(&mut reopened, "quokka", 5)
            .into_iter()
            .map(|hit| hit.0)
            .collect::<Vec<_>>(),
        vec![9_002],
        "a committed document did not reach the file with its postings"
    );
}

/// Another handle's commit has to reach this one, and a self-persisting index
/// is brought up to date by *re-opening* it rather than by replaying the rows
/// the change log names — replaying would apply the writer's edit a second
/// time, as writes, from a handle that only read.
/// `Engine::adopt_self_persisting_text_indexes` is the piece that does it, and
/// this is the property it exists for: after a foreign commit, the reader
/// answers exactly as a handle opened fresh on the same file.
#[test]
fn a_foreign_commit_reaches_a_reader_without_replaying_rows_into_it() {
    let workspace = Workspace::new("foreign");
    let path = workspace.path("db.inlay");

    let mut writer = open(&path, true);
    create(&mut writer);
    load(&mut writer, 200);

    let mut reader = open(&path, true);
    let before = ranked(&mut reader, "alpha epsilon", 10);
    assert!(!before.is_empty());

    for id in 201..=260u64 {
        writer
            .execute(
                "INSERT INTO docs VALUES (?, ?)",
                &[Value::Integer(id as i64), Value::Text(body(id).into())],
            )
            .unwrap();
    }
    // The writer's own read is what makes its index writes durable, so do one.
    let _ = ranked(&mut writer, "alpha", 1);

    // A handle opened now sees everything: that is the oracle.
    let expected = {
        let mut fresh = open(&path, true);
        QUERIES
            .iter()
            .map(|query| ranked(&mut fresh, query, 20))
            .collect::<Vec<_>>()
    };
    for (query, expected) in QUERIES.iter().zip(&expected) {
        assert_eq!(
            &ranked(&mut reader, query, 20),
            expected,
            "the reader did not adopt the foreign commit for `{query}`"
        );
    }
    // And the writer agrees with both, which is what rules out the reader
    // having quietly written into the shared postings.
    for (query, expected) in QUERIES.iter().zip(&expected) {
        assert_eq!(
            &ranked(&mut writer, query, 20),
            expected,
            "the writer diverged after the reader caught up on `{query}`"
        );
    }
}

/// The default has not moved. Opening a database the ordinary way still gets
/// the in-memory backend, which is what makes this a trade a caller opts into
/// rather than one that happens to them.
#[test]
fn the_default_is_still_the_in_memory_backend() {
    let workspace = Workspace::new("default");
    let path = workspace.path("db.inlay");
    assert!(!EngineOptions::default().paged_text_indexes);

    let mut db = Database::open(&path).unwrap();
    create(&mut db);
    load(&mut db, 60);
    // The paged backend keeps its postings under a namespace no table can
    // name; the in-memory one saves a blob instead. What is checkable from
    // here is that the answers are right either way and that the option
    // really is off.
    assert!(!ranked(&mut db, "alpha", 5).is_empty());
}
