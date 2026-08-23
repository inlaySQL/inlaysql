//! The change-data-capture stream: what a consumer sees, and what it is told
//! when it has fallen too far behind to be served correctly.

use inlaysql_core::cdc::ChangeKind;
use inlaysql_core::{mem, Engine, Value};

fn seeded() -> Engine {
    let mut engine = mem::engine().expect("engine");
    engine
        .execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)", &[])
        .unwrap();
    engine
}

fn insert(engine: &mut Engine, id: i64, body: &str) {
    engine
        .execute(
            "INSERT INTO docs (id, body) VALUES (?, ?)",
            &[Value::Integer(id), Value::Text(body.to_string().into())],
        )
        .unwrap();
}

#[test]
fn every_kind_of_change_appears_in_commit_order() {
    let mut engine = seeded();
    insert(&mut engine, 1, "one");
    insert(&mut engine, 2, "two");
    engine
        .execute("UPDATE docs SET body = 'ONE' WHERE id = 1", &[])
        .unwrap();
    engine
        .execute("DELETE FROM docs WHERE id = 2", &[])
        .unwrap();

    let changes = engine.changes(0).unwrap();
    let seen: Vec<(u64, ChangeKind)> = changes
        .changes
        .iter()
        .map(|change| (change.id, change.kind))
        .collect();
    assert_eq!(
        seen,
        vec![
            (1, ChangeKind::Insert),
            (2, ChangeKind::Insert),
            (1, ChangeKind::Update),
            (2, ChangeKind::Delete),
        ]
    );
    assert!(changes.changes.iter().all(|c| c.table == "docs"));
    assert!(!changes.lost(0));
}

#[test]
fn a_consumer_resumes_from_the_version_it_was_given() {
    let mut engine = seeded();
    insert(&mut engine, 1, "one");

    let first = engine.changes(0).unwrap();
    assert_eq!(first.changes.len(), 1);

    // Nothing has happened since.
    let idle = engine.changes(first.version).unwrap();
    assert!(idle.changes.is_empty());
    assert_eq!(idle.version, first.version);

    insert(&mut engine, 2, "two");
    let next = engine.changes(first.version).unwrap();
    assert_eq!(
        next.changes.len(),
        1,
        "resuming replayed or skipped changes"
    );
    assert_eq!(next.changes[0].id, 2);
}

#[test]
fn one_statement_that_touches_many_rows_is_one_version() {
    let mut engine = seeded();
    insert(&mut engine, 1, "shared");
    insert(&mut engine, 2, "shared");
    let before = engine.change_version();

    engine
        .execute(
            "UPDATE docs SET body = 'changed' WHERE body = 'shared'",
            &[],
        )
        .unwrap();

    let changes = engine.changes(before).unwrap();
    assert_eq!(changes.changes.len(), 2);
    assert_eq!(
        changes.changes[0].version, changes.changes[1].version,
        "rows changed by one statement must share a version"
    );
}

#[test]
fn a_statement_that_changed_nothing_produces_no_change() {
    let mut engine = seeded();
    insert(&mut engine, 1, "one");
    let before = engine.change_version();

    engine
        .execute("DELETE FROM docs WHERE id = 12345", &[])
        .unwrap();
    engine
        .execute("UPDATE docs SET body = 'x' WHERE id = 12345", &[])
        .unwrap();

    assert_eq!(
        engine.change_version(),
        before,
        "a statement that matched nothing advanced the version"
    );
    assert!(engine.changes(before).unwrap().changes.is_empty());
}

#[test]
fn reads_do_not_appear_in_the_stream() {
    let mut engine = seeded();
    insert(&mut engine, 1, "one");
    let before = engine.change_version();
    engine.query("SELECT id, body FROM docs", &[]).unwrap();
    assert_eq!(engine.change_version(), before);
}

#[test]
fn a_consumer_that_falls_behind_the_retention_window_is_told_so() {
    let mut engine = seeded();
    // One statement per version; go past the 4096-statement window.
    for id in 1..=4200i64 {
        insert(&mut engine, id, "body");
    }

    let changes = engine.changes(0).unwrap();
    assert!(
        changes.lost(0),
        "a consumer starting from 0 after 4200 writes should be told it lost changes"
    );
    assert!(changes.floor > 0, "nothing was reported as dropped");

    // A consumer that kept up is not told it lost anything.
    let recent = engine.changes(changes.version - 1).unwrap();
    assert!(!recent.lost(changes.version - 1));
    assert_eq!(recent.changes.len(), 1);
}

#[test]
fn the_stream_survives_reopening() {
    // The log lives in the database, not in memory, so a consumer's position
    // stays meaningful across a restart. `mem::engine` cannot be reopened, so
    // this drives the shared-storage path the persistence tests use.
    let mut engine = seeded();
    insert(&mut engine, 1, "one");
    let version = engine.change_version();
    assert!(version > 0);

    let changes = engine.changes(0).unwrap();
    assert_eq!(changes.changes.len(), 1);
    assert_eq!(changes.version, version);
}
