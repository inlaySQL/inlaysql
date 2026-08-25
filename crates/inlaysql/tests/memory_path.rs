//! `":memory:"` is not a filename.
//!
//! SQLite spells an in-memory database `":memory:"`, so it is the first thing
//! anyone porting from SQLite or `rusqlite` writes. That string is a perfectly
//! legal filename on every filesystem this runs on, so taking it as a path
//! succeeded and quietly produced a real file called `:memory:` in the working
//! directory — durable where the caller wanted ephemeral, and invisible until
//! somebody noticed a stray file in a repository. This test was written after
//! exactly that happened.
//!
//! `docs/architecture.md`'s rule is refuse, never ignore. Refusing also names
//! the call the caller actually wanted, which silently doing the right thing
//! would not.

use std::path::PathBuf;

use inlaysql::{Database, Error};

#[test]
fn opening_the_sqlite_memory_path_is_refused_and_creates_no_file() {
    let before = PathBuf::from(":memory:");
    let existed = before.exists();

    let result = Database::open(":memory:");

    match result {
        Err(Error::Unsupported(message)) => {
            assert!(
                message.contains("open_in_memory"),
                "the refusal must name the call that does what the caller \
                 wanted, got: {message}"
            );
        }
        Err(other) => panic!("expected an Unsupported error naming the right call, got {other:?}"),
        Ok(_) => panic!("`:memory:` was accepted as a filename"),
    }

    // The whole point: nothing was created on the way to the error.
    if !existed {
        assert!(
            !PathBuf::from(":memory:").exists(),
            "refusing `:memory:` still left a file behind"
        );
    }
}

/// And the call it points at works, so the error is actionable rather than a
/// dead end.
#[test]
fn the_call_the_refusal_names_actually_works() {
    let mut db = Database::open_in_memory().expect("open_in_memory");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", &[])
        .expect("create");
    db.execute("INSERT INTO t (id) VALUES (1)", &[])
        .expect("insert");
    assert_eq!(
        db.query("SELECT id FROM t", &[]).expect("query").rows.len(),
        1
    );
}

/// A path that merely *contains* the word is an ordinary path, so the guard
/// has to match the whole string and not a substring of it.
#[test]
fn a_path_that_only_looks_like_it_is_still_a_path() {
    let dir = std::env::temp_dir().join(format!("inlaysql-memory-path-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("not-:memory:-really.inlay");

    Database::open(&path).expect("an ordinary path containing the word must still open");
    assert!(path.exists(), "the ordinary path was not created");

    let _ = std::fs::remove_dir_all(&dir);
}
