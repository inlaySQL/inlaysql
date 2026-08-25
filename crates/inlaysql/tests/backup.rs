//! End-to-end proof for online backup (`Database::backup_to`,
//! `inlaysql::backup`, `inlaysql backup` — `docs/enterprise-readiness.md`
//! blocker 2).
//!
//! The property that matters is not "the copy opens". It is **the copy is one
//! committed snapshot** — every row set in it is one a writer actually
//! committed, never a mix of two — so the workload here is a bank transfer,
//! whose committed states are enumerable in closed form: after `t` transfers
//! the four balances are exactly `expected_balances(t)`, and the copy carries
//! `t` in a second table. Reading `t` out of the copy and computing what the
//! balances must have been is a total check, not a sum invariant that a
//! two-commit mix could still satisfy by luck.
//!
//! Four situations, each proving something the others cannot:
//!
//! * **Another handle committing on another thread**, backed up repeatedly
//!   through an ordinary read-write handle. The everyday case.
//! * **Another *process* holding the file open for writing** — a running
//!   server — backed up through the lock-free read-only fallback
//!   `inlaysql::backup` picks. This is the whole point of the feature and the
//!   one case `vacuum` cannot do at all, since it needs the lock the server
//!   holds. Run in a child process by the same helper pattern
//!   `file_locking.rs` uses.
//! * **Page reuse on**, which is the one thing that could make the copy
//!   silently wrong: a reclaimed page is overwritten in place, so a page the
//!   copy is walking could become some other node underneath it. Proven the
//!   way it has to be — reclamation demonstrably firing
//!   (`CowBTree::pages_reused`), a handle pinned across all of it, and its
//!   copy byte-exact afterwards — and refused, loudly, in the one
//!   configuration where nothing can pin it.
//! * **BM25 and vector indexes**, because those persist blobs into the tree
//!   and a paged ANN index persists its whole graph as rows. A physical page
//!   copy carries them, but "carries them" has to mean the copy answers the
//!   same queries, saved blob or rebuilt.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use inlaysql::{Database, EngineOptions, Error, FileDevice, SourceAccess, Value};
use inlaysql_core::btree::{CowBTree, DEFAULT_PAGE_SIZE};

/// A directory that deletes itself and everything in it when the test ends,
/// whatever the outcome. A backup test makes many files, not one, so the
/// single-file `TempDb` the other tests in this crate use is the wrong shape
/// here — and this crate deliberately has no `tempfile` dependency.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-backup-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const ACCOUNTS: i64 = 4;
const OPENING_BALANCE: i64 = 100;

/// The only committed states this workload can produce: after `transfers`
/// transfers, transfer `t` having moved one unit from account `t % 4` to
/// account `(t + 1) % 4`.
///
/// This is what makes the assertion total rather than statistical. A copy
/// that mixed two commits — half a transfer applied — would still sum to
/// `ACCOUNTS * OPENING_BALANCE`, so a sum invariant alone would not see it;
/// the per-account balances implied by the copy's own transfer counter do.
fn expected_balances(transfers: i64) -> Vec<i64> {
    let mut balances = vec![OPENING_BALANCE; ACCOUNTS as usize];
    for t in 1..=transfers {
        balances[(t % ACCOUNTS) as usize] -= 1;
        balances[((t + 1) % ACCOUNTS) as usize] += 1;
    }
    balances
}

fn create_ledger(path: &Path) {
    let mut db = Database::open(path).expect("open");
    db.execute(
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL)",
        &[],
    )
    .expect("create accounts");
    db.execute(
        "CREATE TABLE ledger (id INTEGER PRIMARY KEY, transfers INTEGER NOT NULL)",
        &[],
    )
    .expect("create ledger");
    for id in 0..ACCOUNTS {
        db.execute(
            "INSERT INTO accounts (id, balance) VALUES (?, ?)",
            &[Value::Integer(id), Value::Integer(OPENING_BALANCE)],
        )
        .expect("open account");
    }
    db.execute("INSERT INTO ledger (id, transfers) VALUES (1, 0)", &[])
        .expect("open ledger");
}

/// Move one unit for transfer number `t`, and record `t`, in one transaction.
///
/// One transaction is the point: the two balance changes and the counter have
/// to become visible together or the workload has no committed state for a
/// backup to be checked against.
fn transfer(db: &mut Database, t: i64) -> Result<(), Error> {
    db.begin()?;
    let result = (|| -> Result<(), Error> {
        db.execute(
            "UPDATE accounts SET balance = balance - 1 WHERE id = ?",
            &[Value::Integer(t % ACCOUNTS)],
        )?;
        db.execute(
            "UPDATE accounts SET balance = balance + 1 WHERE id = ?",
            &[Value::Integer((t + 1) % ACCOUNTS)],
        )?;
        db.execute(
            "UPDATE ledger SET transfers = ? WHERE id = 1",
            &[Value::Integer(t)],
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => db.commit(),
        Err(error) => {
            let _ = db.rollback();
            Err(error)
        }
    }
}

/// Open `path` and assert its balances are exactly the ones its own transfer
/// counter implies. Returns that counter, so a caller can prove the copies it
/// took actually spanned a moving database.
fn assert_is_a_committed_snapshot(path: &Path, what: &str) -> i64 {
    let mut copy = Database::open(path).unwrap_or_else(|e| panic!("{what} did not open: {e}"));
    let transfers = match copy
        .query("SELECT transfers FROM ledger WHERE id = 1", &[])
        .unwrap_or_else(|e| panic!("{what}: reading the ledger failed: {e}"))
        .rows
        .first()
        .and_then(|row| row.first().cloned())
    {
        Some(Value::Integer(t)) => t,
        other => panic!("{what}: ledger row is {other:?}"),
    };
    let balances: Vec<i64> = copy
        .query("SELECT balance FROM accounts ORDER BY id", &[])
        .unwrap_or_else(|e| panic!("{what}: reading accounts failed: {e}"))
        .rows
        .into_iter()
        .map(|row| match row.first() {
            Some(Value::Integer(b)) => *b,
            other => panic!("{what}: balance is {other:?}"),
        })
        .collect();
    assert_eq!(
        balances,
        expected_balances(transfers),
        "{what}: the balances are not the ones {transfers} committed transfers \
         produce — the copy is a mix of two commits, not a snapshot of one"
    );
    transfers
}

/// The everyday case: another handle is committing, on another thread, for
/// the whole time the backups are being taken.
///
/// Two `Database` handles on one file in one process is the shape
/// `concurrent_writers.rs` already proves is a genuine race — the reservation
/// gate orders their commits, but nothing orders a commit against a read.
#[test]
fn a_backup_taken_while_another_handle_commits_is_one_committed_snapshot() {
    let dir = TempDir::new("live");
    let source = dir.join("live.inlay");
    create_ledger(&source);

    let running = Arc::new(AtomicBool::new(true));
    let committed = Arc::new(AtomicI64::new(0));

    std::thread::scope(|scope| {
        let writer_running = Arc::clone(&running);
        let writer_committed = Arc::clone(&committed);
        let writer_path = source.clone();
        scope.spawn(move || {
            let mut db = Database::open(&writer_path).expect("writer opens");
            let mut t = 1i64;
            while writer_running.load(Ordering::Relaxed) && t <= 5_000 {
                match transfer(&mut db, t) {
                    Ok(()) => {
                        writer_committed.store(t, Ordering::Relaxed);
                        t += 1;
                    }
                    // First-committer-wins is not possible here (one writer),
                    // but retrying rather than panicking keeps a failure in
                    // this thread from being reported as a backup failure.
                    Err(Error::Conflict) => continue,
                    Err(error) => panic!("writer failed at transfer {t}: {error}"),
                }
            }
        });

        let mut db = Database::open(&source).expect("backup handle opens");
        let mut seen = Vec::new();
        for i in 0..20 {
            let destination = dir.join(&format!("copy-{i}.inlay"));
            let summary = db.backup_to(&destination).expect("backup");
            assert!(summary.pages > 0, "a populated database copied no pages");
            seen.push(assert_is_a_committed_snapshot(
                &destination,
                &format!("copy {i}"),
            ));
            // Give the writer room to land commits between copies, so the
            // copies actually span a moving database rather than twenty views
            // of one still one.
            std::thread::sleep(Duration::from_millis(5));
        }
        running.store(false, Ordering::Relaxed);

        assert!(
            seen.windows(2).all(|pair| pair[0] <= pair[1]),
            "backups went backwards in time: {seen:?}"
        );
        assert!(
            seen.last() > seen.first(),
            "no commit landed between the first and last backup, so nothing \
             here was actually concurrent: {seen:?}"
        );
    });
}

/// The copy is a database, not an archive: it opens with `Database::open` and
/// answers ordinary queries, including through an index it never rebuilt.
#[test]
fn the_backup_opens_and_answers_queries() {
    let dir = TempDir::new("queries");
    let source = dir.join("source.inlay");
    let destination = dir.join("copy.inlay");

    {
        let mut db = Database::open(&source).expect("open");
        db.execute(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, qty INTEGER)",
            &[],
        )
        .expect("create");
        db.execute("CREATE INDEX items_qty ON items (qty) USING BTREE", &[])
            .expect("create index");
        for id in 1..=200i64 {
            db.execute(
                "INSERT INTO items (id, name, qty) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id),
                    Value::Text(format!("item-{id}").into()),
                    Value::Integer(id % 17),
                ],
            )
            .expect("insert");
        }
        // A value far larger than a page, so the copy has an overflow chain to
        // carry. A walk that only followed B-tree nodes would produce a file
        // that opens and then fails on this one row.
        db.execute(
            "INSERT INTO items (id, name, qty) VALUES (?, ?, ?)",
            &[
                Value::Integer(9_999),
                Value::Text("x".repeat(40_000).into()),
                Value::Integer(1),
            ],
        )
        .expect("insert overflowing row");
        db.backup_to(&destination).expect("backup");
    }

    let mut copy = Database::open(&destination).expect("the copy opens");
    let rows = copy
        .query("SELECT COUNT(*) FROM items", &[])
        .expect("count")
        .rows;
    assert_eq!(rows[0][0], Value::Integer(201));

    let indexed = copy
        .query("SELECT id FROM items WHERE qty = 3 ORDER BY id", &[])
        .expect("indexed lookup")
        .rows;
    assert!(!indexed.is_empty(), "the copied index answered nothing");

    let big = copy
        .query("SELECT name FROM items WHERE id = 9999", &[])
        .expect("overflow row")
        .rows;
    assert_eq!(
        big[0][0],
        Value::Text("x".repeat(40_000).into()),
        "the overflow chain did not survive the copy"
    );

    // And it is writable: nothing about the copy is a special mode.
    copy.execute(
        "INSERT INTO items (id, name, qty) VALUES (7000, 'new', 1)",
        &[],
    )
    .expect("the copy accepts writes");
}

/// A document body whose count of the term the queries below search for
/// varies with `id`, so BM25 produces a ranking with real order in it rather
/// than 120 documents tied on one occurrence apiece.
fn body(id: usize) -> String {
    format!(
        "document {id} about rust databases {}",
        "vector ".repeat(id % 9 + 1)
    )
}

fn embedding(seed: usize, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|i| ((seed * 31 + i * 7) % 97) as f32 / 97.0)
        .collect()
}

/// Retrieval indexes are the case a physical copy has to be checked for
/// specifically: BM25 and in-memory HNSW persist a *blob* into the tree, and
/// the paged ANN index persists its whole graph as ordinary rows. Both are
/// copied like any other row — but "copied" only means something if the copy
/// answers the same query.
///
/// Run twice on purpose: once with `checkpoint` first, so the copy carries a
/// saved blob whose stamp still matches, and once without, so it carries a
/// stale one that the copy must *discard and rebuild* rather than trust. Both
/// have to give the same answer, which is the contract `crate::traits`
/// ("Persisting an index") states and the one a backup silently depends on.
#[test]
fn a_backup_carries_a_bm25_and_a_vector_index() {
    const DIM: usize = 16;

    for checkpoint_first in [true, false] {
        let dir = TempDir::new(if checkpoint_first {
            "retrieval-saved"
        } else {
            "retrieval-stale"
        });
        let source = dir.join("source.inlay");
        let destination = dir.join("copy.inlay");

        let expected = {
            let mut db = Database::open(&source).expect("open");
            db.execute(
                &format!(
                    "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, \
                     embedding VECTOR({DIM}))"
                ),
                &[],
            )
            .expect("create");
            db.execute("CREATE INDEX docs_body ON docs (body)", &[])
                .expect("create text index");
            db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
                .expect("create vector index");
            for id in 1..=120usize {
                db.execute(
                    "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                    &[
                        Value::Integer(id as i64),
                        Value::Text(body(id).into()),
                        Value::Vector(embedding(id, DIM)),
                    ],
                )
                .expect("insert");
            }
            if checkpoint_first {
                db.checkpoint().expect("persist the indexes");
            }
            let expected = (text_ranking(&mut db), hybrid(&mut db, DIM));
            db.backup_to(&destination).expect("backup");
            expected
        };
        let (expected_text, expected_hybrid) = expected;

        let mut copy = Database::open(&destination).expect("the copy opens");
        assert_eq!(
            text_ranking(&mut copy),
            expected_text,
            "checkpoint_first={checkpoint_first}: the copy ranked the full-text \
             query differently from the database it came from"
        );
        assert_eq!(
            nearest(&mut copy, DIM, 7),
            Value::Integer(7),
            "checkpoint_first={checkpoint_first}: the copy's vector index did \
             not return the row whose embedding is the query"
        );
        let hybrid_from_copy = hybrid(&mut copy, DIM);
        assert_eq!(
            hybrid_from_copy.len(),
            expected_hybrid.len(),
            "checkpoint_first={checkpoint_first}: the copy's fused query \
             returned a different number of rows"
        );
        assert!(
            hybrid_from_copy.contains(&Value::Integer(7)),
            "checkpoint_first={checkpoint_first}: the fused ranking lost the \
             exact vector match: {hybrid_from_copy:?}"
        );
        if checkpoint_first {
            // The copy loaded the very blob the source saved, so this is not
            // two graphs that ought to agree — it is one graph, and anything
            // other than an identical ranking means the blob did not survive
            // the copy intact. Not asserted for the rebuilt case: HNSW is a
            // pure function of insert order but its layer ceiling is
            // recomputed per commit (`hnsw`'s module doc), so a from-scratch
            // rebuild is allowed to place a rare node one layer differently
            // and reorder neighbours that are already near-ties.
            assert_eq!(
                hybrid_from_copy, expected_hybrid,
                "the copy loaded a saved index blob and still ranked differently"
            );
        }
    }
}

/// Row ids in BM25 rank order. Exact rather than approximate — an inverted
/// index scores every matching document — so two databases holding the same
/// rows must produce the same ranking, whether one of them rebuilt its index
/// or loaded a saved one.
fn text_ranking(db: &mut Database) -> Vec<Value> {
    ids(db
        .query(
            "SELECT id, bm25_score(body, ?) AS score FROM docs \
             ORDER BY score DESC, id ASC LIMIT 10",
            &[Value::Text("vector".into())],
        )
        .expect("full-text query"))
}

/// The row an ANN search puts first for a query vector that is byte-identical
/// to one stored row's embedding. Any correct graph returns that row, however
/// it was built, which is what makes this checkable across a rebuild.
fn nearest(db: &mut Database, dim: usize, seed: usize) -> Value {
    ids(db
        .query(
            "SELECT id, vector_score(embedding, ?) AS score FROM docs \
             ORDER BY score DESC LIMIT 1",
            &[Value::Vector(embedding(seed, dim))],
        )
        .expect("vector query"))
    .remove(0)
}

/// One fused BM25 + vector query, as row ids in rank order.
fn hybrid(db: &mut Database, dim: usize) -> Vec<Value> {
    ids(db
        .query(
            "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
             FROM docs ORDER BY score DESC, id ASC LIMIT 10",
            &[
                Value::Vector(embedding(7, dim)),
                Value::Text("vector".into()),
            ],
        )
        .expect("hybrid query"))
}

fn ids(result: inlaysql::ResultSet) -> Vec<Value> {
    result
        .rows
        .into_iter()
        .map(|mut row| row.remove(0))
        .collect()
}

/// The paged ANN index keeps its graph in the database file as rows, so a
/// physical copy carries the index itself rather than a blob of it — and the
/// copy has to open *as* a paged database and find that graph where it left
/// it, without a rebuild.
#[test]
fn a_backup_carries_a_paged_vector_index() {
    const DIM: usize = 16;
    let dir = TempDir::new("paged");
    let source = dir.join("source.inlay");
    let destination = dir.join("copy.inlay");

    let expected = {
        let mut db = Database::open_paged(&source).expect("open paged");
        db.execute(
            &format!(
                "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR({DIM}))"
            ),
            &[],
        )
        .expect("create");
        db.execute("CREATE INDEX docs_body ON docs (body)", &[])
            .expect("create text index");
        db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
            .expect("create vector index");
        for id in 1..=120usize {
            db.execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id as i64),
                    Value::Text(body(id).into()),
                    Value::Vector(embedding(id, DIM)),
                ],
            )
            .expect("insert");
        }
        let expected = hybrid(&mut db, DIM);
        db.backup_to(&destination).expect("backup");
        expected
    };

    let mut copy = Database::open_paged(&destination).expect("the copy opens paged");
    assert_eq!(
        hybrid(&mut copy, DIM),
        expected,
        "the paged index's graph rows did not survive the copy"
    );
}

/// Page reuse is the one setting that can make a page copy silently wrong: a
/// reclaimed id is overwritten in place, so a page the walk is about to read
/// can become a different node underneath it.
///
/// This runs at the `CowBTree<FileDevice>` layer rather than through
/// `Database`, for the same reason `free_list_growth.rs` does: only there is
/// `CowBTree::pages_reused` visible, and without it this test could pass
/// having never reclaimed anything — proving nothing about the hazard it is
/// named for.
///
/// The shape is deliberately not two threads. The reader watermark pins from
/// the moment a handle last refreshed, so the sharpest version of the test is
/// sequential: pin a snapshot, then let the writer churn hard enough to
/// reclaim, then take the backup *at the pinned snapshot*. If the pin were
/// missing, that churn is exactly what would have recycled the pages this
/// backup is about to walk.
#[test]
fn a_pinned_handle_backs_up_correctly_while_a_reusing_writer_recycles_pages() {
    let dir = TempDir::new("reuse-pinned");
    let source = dir.join("source.inlay");
    let destination = dir.join("copy.inlay");

    let mut writer = CowBTree::open_or_create(
        FileDevice::open(&source).expect("writer device"),
        DEFAULT_PAGE_SIZE,
    )
    .expect("writer tree");
    writer.set_page_reuse(true);

    // Enough churn *before* the reader pins that the free list holds rows the
    // reader's watermark will not veto — otherwise reclamation could never
    // fire at all and the assertion below would be vacuous.
    churn(&mut writer, 0..8);

    let reader =
        CowBTree::open(FileDevice::open(&source).expect("reader device")).expect("reader tree");
    let pinned: Vec<(Vec<u8>, Vec<u8>)> = reader
        .scan()
        .expect("scan")
        .into_iter()
        .map(|(key, value)| (key, value.into_vec()))
        .collect();
    assert!(!pinned.is_empty(), "nothing was committed before the pin");

    // The writer now supersedes every page the pinned snapshot is made of.
    // Without the reader's watermark these would become reclaim candidates.
    churn(&mut writer, 8..40);
    assert!(
        writer.pages_reused() > 0,
        "the writer never reclaimed a page, so this test proves nothing about \
         backing up beside one that does"
    );

    let mut dest = FileDevice::open(&destination).expect("destination device");
    reader.backup_to(&mut dest).expect("backup");
    drop(dest);

    let copy = CowBTree::open(FileDevice::open(&destination).expect("reopen copy"))
        .expect("the copy opens");
    let copied: Vec<(Vec<u8>, Vec<u8>)> = copy
        .scan()
        .expect("scan the copy")
        .into_iter()
        .map(|(key, value)| (key, value.into_vec()))
        .collect();
    assert_eq!(
        copied, pinned,
        "the copy is not the snapshot the handle was pinned to — a page it \
         walked was recycled underneath it"
    );
}

/// Overwrite every key, then delete and reinsert a rotating slice, so old
/// pages are genuinely superseded rather than grown into. Checkpointing is
/// what makes a freed page's row durable enough to be drawn from.
fn churn(tree: &mut CowBTree<FileDevice>, rounds: std::ops::Range<usize>) {
    const KEYS: usize = 120;
    for round in rounds {
        for i in 0..KEYS {
            let key = format!("k{i:06}").into_bytes();
            let value: Vec<u8> = (0..300)
                .map(|b| ((round * 31 + i * 7 + b) % 251) as u8)
                .collect();
            tree.put(&key, &value).expect("put");
        }
        tree.commit().expect("commit");
        for i in (round % 4..KEYS).step_by(4) {
            tree.delete(format!("k{i:06}").as_bytes()).expect("delete");
        }
        tree.commit().expect("commit");
        for i in (round % 4..KEYS).step_by(4) {
            let key = format!("k{i:06}").into_bytes();
            tree.put(&key, b"small").expect("put");
        }
        tree.commit().expect("commit");
        tree.checkpoint().expect("checkpoint");
    }
}

/// The configuration nothing can pin: a lock-free read-only handle on a file
/// some writer has page reuse on for. There is no way to tell that writer this
/// reader exists, so the copy could be recycled underneath it — and a page
/// carries no checksum, so the result would decode cleanly and be wrong.
///
/// Refusing is the only honest answer, and refusing on evidence from the file
/// itself (free-list rows exist if and only if some handle committed with
/// reuse on) is what makes it possible at all from a handle that can see
/// nothing else about the writer.
#[test]
fn a_lock_free_backup_of_a_page_reuse_database_is_refused() {
    let dir = TempDir::new("reuse-unpinned");
    let source = dir.join("source.inlay");

    {
        let mut writer = CowBTree::open_or_create(
            FileDevice::open(&source).expect("device"),
            DEFAULT_PAGE_SIZE,
        )
        .expect("tree");
        writer.set_page_reuse(true);
        churn(&mut writer, 0..4);
    }

    let mut reader = Database::open_read_only(&source).expect("open read-only");
    let error = reader
        .backup_to(dir.join("copy.inlay"))
        .expect_err("an unpinned backup of a reusing database must be refused");
    let message = error.to_string();
    assert!(
        message.contains("reclaimable pages"),
        "the refusal must say why, naming page reuse: {message}"
    );
    assert!(
        !dir.join("copy.inlay").exists(),
        "a refused backup must leave no file behind"
    );

    // The same file, through a handle that *can* pin, is fine — the refusal is
    // about the handle, not about the database being unbackupable.
    let mut writable = Database::open(&source).expect("open read-write");
    writable
        .backup_to(dir.join("pinned.inlay"))
        .expect("a pinned backup of the same database must succeed");
}

/// A lock-free read-only handle is how a backup runs beside a live server, and
/// it must work — the refusal above is specific to page reuse, not to
/// read-only handles.
#[test]
fn a_read_only_handle_backs_up_a_database_another_handle_is_writing() {
    let dir = TempDir::new("read-only");
    let source = dir.join("source.inlay");
    create_ledger(&source);

    let mut writer = Database::open(&source).expect("writer");
    for t in 1..=25 {
        transfer(&mut writer, t).expect("transfer");
    }

    let mut reader = Database::open_read_only(&source).expect("read-only handle");
    let destination = dir.join("copy.inlay");
    reader.backup_to(&destination).expect("backup");
    assert_eq!(
        assert_is_a_committed_snapshot(&destination, "read-only copy"),
        25,
        "a read-only backup must see the commits made before it ran"
    );
}

#[test]
fn a_backup_refuses_to_overwrite_an_existing_file() {
    let dir = TempDir::new("overwrite");
    let source = dir.join("source.inlay");
    create_ledger(&source);
    let occupied = dir.join("occupied.inlay");
    fs::write(&occupied, b"precious").expect("write");

    let mut db = Database::open(&source).expect("open");
    let error = db
        .backup_to(&occupied)
        .expect_err("overwriting must be refused");
    assert!(
        error.to_string().contains("already exists"),
        "the refusal must say what it found: {error}"
    );
    assert_eq!(
        fs::read(&occupied).expect("read"),
        b"precious",
        "the refusal must not have touched the file"
    );

    // Including the live database itself, which is the mistake that would
    // otherwise be carried out perfectly and destroy it.
    let error = db
        .backup_to(&source)
        .expect_err("backing up over the source");
    assert!(error.to_string().contains("already exists"), "{error}");
}

#[test]
fn backing_up_a_missing_database_is_refused_rather_than_creating_one() {
    let dir = TempDir::new("missing");
    let source = dir.join("nothing-here.inlay");
    let error = inlaysql::backup(&source, dir.join("copy.inlay"))
        .expect_err("a missing source is an error");
    assert!(error.to_string().contains("does not exist"), "{error}");
    assert!(!source.exists(), "the source must not have been created");
}

#[test]
fn an_in_memory_database_says_plainly_that_it_cannot_be_backed_up() {
    let dir = TempDir::new("in-memory");
    let mut db = Database::open_in_memory().expect("open");
    db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY)", &[])
        .expect("create");
    let error = db
        .backup_to(dir.join("copy.inlay"))
        .expect_err("an in-memory database has no device to copy");
    assert!(
        matches!(error, Error::Unsupported(_)),
        "expected Unsupported, got {error:?}"
    );
    assert!(
        !dir.join("copy.inlay").exists(),
        "a refused backup must leave no file behind"
    );
}

#[test]
fn a_backup_is_refused_inside_a_transaction() {
    let dir = TempDir::new("in-transaction");
    let source = dir.join("source.inlay");
    create_ledger(&source);

    let mut db = Database::open(&source).expect("open");
    db.begin().expect("begin");
    db.execute("UPDATE accounts SET balance = 0 WHERE id = 0", &[])
        .expect("update");
    let error = db
        .backup_to(dir.join("copy.inlay"))
        .expect_err("a backup inside a transaction must be refused");
    assert!(
        matches!(error, Error::Transaction(_)),
        "expected Transaction, got {error:?}"
    );
    db.rollback().expect("rollback");
}

// ------------------------------------------------------------ two processes

const HELPER_PATH_VAR: &str = "INLAYSQL_BACKUP_HELPER_PATH";
const HELPER_READY: &str = "WRITING";

/// Not a real test: the body a child process runs for
/// `a_backup_runs_against_a_database_another_process_holds_open_for_writing`.
/// Gated behind `#[ignore]` so `cargo test` never runs it on its own, and
/// behind the env var so an `--ignored` run without it is a harmless no-op —
/// the same pattern, for the same reasons, as `file_locking.rs`.
///
/// It opens the database read-write (taking the OS advisory lock the parent
/// must then fail over from), reports ready, and commits transfers until the
/// parent closes its stdin.
#[test]
#[ignore = "subprocess helper for the two-process backup test, not a standalone test"]
fn backup_helper_process() {
    let Some(path) = std::env::var_os(HELPER_PATH_VAR) else {
        return;
    };
    let mut db = match Database::open(&path) {
        Ok(db) => db,
        Err(error) => {
            println!("FAILED: {error}");
            let _ = std::io::stdout().flush();
            return;
        }
    };
    println!("{HELPER_READY}");
    let _ = std::io::stdout().flush();

    // A blocking read on its own thread, so the commit loop below is the
    // thing that decides how often it checks for the stop signal rather than
    // the other way round.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::clone(&stop);
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        stop_signal.store(true, Ordering::Relaxed);
    });

    let mut t = 1i64;
    // Bounded so a parent that dies without closing stdin cannot leave this
    // process committing forever.
    while !stop.load(Ordering::Relaxed) && t <= 100_000 {
        match transfer(&mut db, t) {
            Ok(()) => t += 1,
            Err(Error::Conflict) => continue,
            Err(error) => {
                eprintln!("helper failed at transfer {t}: {error}");
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The headline case: a *running server* holds the file open for writing, and
/// a backup is taken anyway, from a separate process, without stopping it.
///
/// This is exactly what `vacuum` cannot do — it needs the exclusive lock the
/// writer is holding — and it is the only path that exercises
/// `inlaysql::backup`'s fall back to a lock-free read-only handle, since two
/// handles *inside* one process share the lock and never contend.
#[test]
fn a_backup_runs_against_a_database_another_process_holds_open_for_writing() {
    let dir = TempDir::new("two-process");
    let source = dir.join("source.inlay");
    create_ledger(&source);

    let exe = std::env::current_exe().expect("current test binary path");
    let mut child = Command::new(&exe)
        .arg("backup_helper_process")
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .env(HELPER_PATH_VAR, &source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper process");

    let stdout = child.stdout.take().expect("helper stdout piped");
    let ready = read_marker_line_with_timeout(stdout, Duration::from_secs(30));
    assert_eq!(
        ready.as_deref(),
        Some(HELPER_READY),
        "the helper process did not take the file for writing"
    );

    // The helper holds the exclusive lock, so a read-write open from here is
    // refused and `backup` has to fall back — which is the thing under test.
    assert!(
        Database::open(&source).is_err(),
        "the helper process should be holding the file's write lock"
    );

    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut i = 0;
    // Keep copying until two copies disagree about how many transfers had
    // committed: that is the proof the copies really were taken from a moving
    // database rather than a quiescent one.
    while std::time::Instant::now() < deadline
        && (seen.len() < 2 || seen[0] == seen[seen.len() - 1])
    {
        let destination = dir.join(&format!("copy-{i}.inlay"));
        let outcome = inlaysql::backup(&source, &destination).expect("backup");
        assert_eq!(
            outcome.access,
            SourceAccess::LockFree,
            "with another process holding the write lock the backup must have \
             fallen back to a lock-free read-only handle"
        );
        seen.push(assert_is_a_committed_snapshot(
            &destination,
            &format!("copy {i}"),
        ));
        i += 1;
        std::thread::sleep(Duration::from_millis(20));
    }

    // Let the helper finish before the temp directory goes away.
    drop(child.stdin.take());
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    assert!(status.is_some(), "the helper process did not exit");

    assert!(
        seen.len() >= 2 && seen[0] < seen[seen.len() - 1],
        "no commit from the other process landed between two backups, so this \
         never tested a live database: {seen:?}"
    );
}

fn read_marker_line_with_timeout(
    stdout: impl std::io::Read + Send + 'static,
    timeout: Duration,
) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            if trimmed == HELPER_READY || trimmed.starts_with("FAILED:") {
                let _ = tx.send(trimmed.to_string());
                return;
            }
        }
    });
    rx.recv_timeout(timeout).ok()
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `EngineOptions` is used only to prove the option surface still compiles
/// against a backup — a database opened with a non-default page cache is
/// backed up the same way.
#[test]
fn engine_options_do_not_change_what_a_backup_contains() {
    let dir = TempDir::new("options");
    let source = dir.join("source.inlay");
    create_ledger(&source);

    let mut db = Database::open_on_with_options(
        FileDevice::open(&source).expect("device"),
        EngineOptions {
            page_cache_bytes: 0,
            ..EngineOptions::default()
        },
    )
    .expect("open");
    for t in 1..=10 {
        transfer(&mut db, t).expect("transfer");
    }
    let destination = dir.join("copy.inlay");
    db.backup_to(&destination).expect("backup");
    assert_eq!(
        assert_is_a_committed_snapshot(&destination, "copy with the cache off"),
        10
    );
}
