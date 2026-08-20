//! The retrieval indexes survive the fault schedules the storage engine does.
//!
//! `dst_sweep` proves the *rows* recover to a committed snapshot. This proves
//! the property that matters once indexes are written into the same file: after
//! a crash — including one in the middle of writing an index — the index the
//! database comes back with **agrees with the rows it comes back with**.
//!
//! It has to be here rather than in `inlaysql-core` because it drives the whole
//! engine over `TreeStorage`, which is where the core's `Storage` trait meets
//! the copy-on-write tree.
//!
//! The assertion is deliberately not "everything we inserted is still there":
//! a crash is supposed to lose the last commit. It is that every row the
//! recovered database can scan is also a row its indexes can find, and nothing
//! else is. A stale index that survived a crash it should not have would fail
//! this immediately.
//!
//! Every decision is a pure function of the seed, so a failure reproduces
//! exactly: `cargo test -p inlaysql --test index_recovery_dst -- <name>`.

use std::cell::RefCell;
use std::rc::Rc;

use inlaysql::{Database, EngineOptions, IndexKind, Value};
use inlaysql_core::btree::CowBTree;
use inlaysql_core::mem::SeededRng;
use inlaysql_core::sim::{FaultSchedule, SimDisk, Simulator};
use inlaysql_core::Rng;

const BLOCK: usize = 512;
const CAPACITY: usize = 8 << 20;
const DIM: usize = 4;
/// Documents each seed's workload tries to insert.
const DOCUMENTS: u64 = 24;

/// A distinctive term, so a BM25 search for it can only match one document.
fn body(id: u64) -> String {
    format!("document tagged uniquetoken{id} about embedded storage")
}

fn embedding(id: u64) -> Vec<f32> {
    let angle = id as f32 * 0.37;
    vec![angle.cos(), angle.sin(), (angle * 0.5).cos(), 0.25]
}

/// A value many documents share, so a B-tree equality probe returns a group
/// rather than a single row.
fn bucket(id: u64) -> i64 {
    (id % 5) as i64
}

/// A value no two documents share, so the unique index has something to
/// enforce and a probe has exactly one right answer.
fn label(id: u64) -> String {
    format!("label{id:04}")
}

/// A value whose *case* varies independently of its folded form, so a `NOCASE`
/// index groups rows a `BINARY` one keeps apart.
///
/// Three bases and two cases, and 3 and 2 are coprime, so over the 24
/// documents every base appears in both spellings — which is what makes a
/// probe of the folded key have a different right answer from a probe of the
/// exact one. Without that, a collated index and an uncollated one would agree
/// by accident and this would sweep nothing new (AHL-469).
fn name(id: u64) -> String {
    let base = format!("name{}", id % 3);
    if id.is_multiple_of(2) {
        base.to_ascii_uppercase()
    } else {
        base
    }
}

/// The B-tree indexes every shape declares. Unlike the retrieval ones they
/// have no in-memory or paged variant to sweep — the entries *are* the index —
/// so they are built on every schedule rather than in half of them.
///
/// The last two are collated (AHL-469), and they are here because that change
/// altered the *key encoding*: a `NOCASE` index writes the folded value, so a
/// crash between the row and its entries would leave the two disagreeing in a
/// way no uncollated index could show. `docs_label_nocase` is deliberately a
/// second index over a column that already has one, under the other collation,
/// so one row contributes two entries whose bytes differ.
const BTREE_INDEXES: [&str; 5] = [
    "CREATE INDEX docs_bucket ON docs (bucket)",
    "CREATE UNIQUE INDEX docs_label ON docs (label)",
    "CREATE INDEX docs_bucket_label ON docs (bucket, label)",
    "CREATE INDEX docs_name ON docs (name) USING BTREE",
    "CREATE INDEX docs_label_nocase ON docs (label COLLATE NOCASE) USING BTREE",
];

/// One shape of database to run a seed against.
#[derive(Clone, Copy)]
struct Shape {
    /// `VECTOR(4, INT8)` rather than `VECTOR(4)`.
    quantized: bool,
    /// The ANN graph lives in the database file rather than in memory.
    ///
    /// This is the case the property matters most for: an in-memory index is
    /// rebuilt from the rows on open and so cannot disagree with them by
    /// construction, whereas a paged graph is durable state of its own that a
    /// crash can catch mid-write. If a stale graph could ever survive, this is
    /// where it would show.
    paged: bool,
}

impl Shape {
    fn options(self) -> EngineOptions {
        EngineOptions {
            implicit_indexes: false,
            paged_vector_indexes: self.paged,
            ..EngineOptions::default()
        }
    }
}

/// Every combination worth sweeping.
const SHAPES: [Shape; 4] = [
    Shape {
        quantized: false,
        paged: false,
    },
    Shape {
        quantized: true,
        paged: false,
    },
    Shape {
        quantized: false,
        paged: true,
    },
    Shape {
        quantized: true,
        paged: true,
    },
];

/// Run one seed: load documents under fault injection, then reopen the durable
/// image and check the indexes match the rows.
fn sweep(seed: u64, shape: Shape) {
    let quantized = shape.quantized;
    let simulator = Rc::new(RefCell::new(Simulator::with_disk(
        seed,
        SimDisk::with_block_size(BLOCK, CAPACITY),
        // Crash and torn writes, as in `dst_sweep`. Reordered syncs are
        // excluded for the same documented reason (docs/recovery.md).
        FaultSchedule::random_with(seed, 10, 10, 0),
    )));

    // If the very first sync faulted there is no database yet — nothing to
    // recover, and nothing this test can say.
    let Ok(mut db) = Database::open_on_with_options(simulator.clone(), shape.options()) else {
        return;
    };
    let vector_type = if quantized {
        "VECTOR(4, INT8)"
    } else {
        "VECTOR(4)"
    };
    if db
        .execute(
            &format!(
                "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, \
                 embedding {vector_type}, bucket INTEGER, label TEXT, \
                 name TEXT COLLATE NOCASE)"
            ),
            &[],
        )
        .is_err()
    {
        return;
    }
    if db
        .execute("CREATE INDEX docs_body ON docs (body)", &[])
        .is_err()
    {
        return;
    }
    if db
        .execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
        .is_err()
    {
        return;
    }
    for sql in BTREE_INDEXES {
        if db.execute(sql, &[]).is_err() {
            return;
        }
    }

    let mut rng = SeededRng::new(seed ^ 0x5DEE_CE66_D3A1_9F0B);
    for id in 1..=DOCUMENTS {
        if simulator.borrow().crashed() {
            break;
        }
        if db
            .execute(
                "INSERT INTO docs (id, body, embedding, bucket, label, name) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    Value::Integer(id as i64),
                    Value::Text(body(id)),
                    Value::Vector(embedding(id)),
                    Value::Integer(bucket(id)),
                    Value::Text(label(id)),
                    Value::Text(name(id)),
                ],
            )
            .is_err()
        {
            break;
        }

        // Checkpoint at seed-dependent moments, so some seeds crash while an
        // index is being written and some crash long after one was.
        if rng.next_u64().is_multiple_of(4) && db.checkpoint().is_err() {
            break;
        }
    }

    let image = simulator.borrow().disk().durable().to_vec();
    drop(db);

    // Reopen from what actually reached the platter.
    let Ok(mut recovered) =
        Database::open_on_with_options(SimDisk::with_image(BLOCK, &image), shape.options())
    else {
        // A crash before the header became durable leaves no database. That is
        // a legitimate outcome, not a failure.
        return;
    };
    let Ok(rows) = recovered.query("SELECT id FROM docs", &[]) else {
        return;
    };

    let surviving: Vec<u64> = rows
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id as u64,
            ref other => panic!("seed {seed}: expected an integer id, got {other:?}"),
        })
        .collect();

    // An index declaration is itself a durable write: a torn `CREATE INDEX`
    // can be lost while the table it indexes survives. The property under test
    // is that a *surviving* index agrees with the rows — an index that did not
    // survive can disagree with nothing. So derive which indexes made it from
    // the recovered catalog, and assert only on those.
    let indexes = recovered.catalog().indexes_for("docs");
    let has_text = indexes
        .iter()
        .any(|index| index.column() == "body" && index.kind == IndexKind::FullText);
    let has_vector = indexes
        .iter()
        .any(|index| index.column() == "embedding" && index.kind == IndexKind::Vector);

    for id in &surviving {
        if has_text {
            // The full-text index must find every surviving row by its unique
            // term — a stale index would have lost it, or never have had it.
            let hits = recovered
                .query(
                    "SELECT id, bm25_score(body, ?) AS score FROM docs ORDER BY score DESC LIMIT 1",
                    &[Value::Text(format!("uniquetoken{id}"))],
                )
                .unwrap_or_else(|e| panic!("seed {seed}: full-text search failed: {e}"));
            assert_eq!(
                hits.rows.first().map(|row| row[0].clone()),
                Some(Value::Integer(*id as i64)),
                "seed {seed}: the full-text index disagrees with the rows about document {id} \
                 (surviving rows: {surviving:?})"
            );
        }

        if has_vector {
            // And the vector index must know about it too.
            let hits = recovered
                .query(
                    "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC LIMIT 1",
                    &[Value::Vector(embedding(*id))],
                )
                .unwrap_or_else(|e| panic!("seed {seed}: vector search failed: {e}"));
            assert_eq!(
                hits.rows.first().map(|row| row[0].clone()),
                Some(Value::Integer(*id as i64)),
                "seed {seed}: the vector index disagrees with the rows about document {id}"
            );
        }
    }

    // Nothing the rows do not have: a document that was rolled back must not
    // still be reachable through a surviving index that outlived it.
    if has_text {
        let all = recovered
            .query(
                "SELECT id, bm25_score(body, ?) AS score FROM docs ORDER BY score DESC LIMIT 1000",
                &[Value::Text("document embedded storage".to_string())],
            )
            .unwrap_or_else(|e| panic!("seed {seed}: full-text sweep failed: {e}"));
        assert_surviving(&all, &surviving, seed);
    }
    if has_vector {
        let all = recovered
            .query(
                "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC LIMIT 1000",
                &[Value::Vector(embedding(1))],
            )
            .unwrap_or_else(|e| panic!("seed {seed}: vector sweep failed: {e}"));
        assert_surviving(&all, &surviving, seed);
    }

    assert_btree_indexes_agree(&mut recovered, &image, &surviving, seed);
}

/// The scalar indexes, checked the same two ways and one more.
///
/// A B-tree index is not a backend that is rebuilt from the rows on open — its
/// entries are durable rows written in the same transaction as the row they
/// describe. So there are three things a crash could break, and all three are
/// asserted:
///
/// 1. **Completeness.** Every surviving row is found by a probe on its own
///    values. A lost entry fails here.
/// 2. **Soundness.** A probe returns nothing the rows do not have. A stale
///    entry pointing at a rolled-back row fails here — but only if the row id
///    was reused, because the executor reads each candidate row and drops the
///    ones that are gone. Which is why the third check exists.
/// 3. **Exactly one entry per row per index**, read straight out of the tree.
///    This is the one that catches an orphan: an entry the executor would
///    silently ignore is still an entry, and its presence means the index and
///    the rows did not reach the log together.
///
/// # What check 3 found, and what it was (AHL-406)
///
/// Around one seed in two thousand, and **only** when a BM25 index, a paged
/// ANN graph and a `checkpoint()` were all in play, the recovered database
/// held a number of rows and a number of entries that no commit ever produced
/// together. That was never a property of index maintenance: a row and its
/// entries are written into one transaction with no engine code between them.
///
/// It was the storage layer recovering to a state that was never committed,
/// and the cause was a page id handed out twice. A crash on the sync that
/// publishes the state block during a WAL wrap left the file naming an older
/// commit than the handle had already written pages for; the handle rewound its
/// root, correctly, *and* its page allocator, which is not correct. The
/// allocator then reissued ids that were already occupied, and because a page
/// id is the page cache's whole key, later descents read the previous occupant
/// and grafted two timelines into one root. `CowBTree::adopt_next_page_id`
/// fixes it: the allocator is monotonic and never rewinds. See
/// `the_known_mixed_recovery_seed` below for the full sequence.
///
/// Every earlier durable structure was either rebuilt from the rows on open or
/// stamped with a write version and discarded when it did not match, so none of
/// them could ever testify to this. A B-tree index is the first that can, which
/// is why this assertion is the one that found it.
///
/// **The assertion stays as it is.** Weakening it would hide a storage defect
/// behind an index feature, and the index is exactly as crash-safe as the tree
/// underneath it — no more.
fn assert_btree_indexes_agree(
    recovered: &mut Database,
    image: &[u8],
    surviving: &[u64],
    seed: u64,
) {
    let declared: Vec<(String, Vec<String>)> = recovered
        .catalog()
        .indexes_for("docs")
        .iter()
        .filter(|index| index.kind == IndexKind::BTree)
        .map(|index| (index.name.clone(), index.columns.clone()))
        .collect();
    if declared.is_empty() {
        // Every scalar index was lost with the commit that declared it. Then
        // no entry of one may have survived either, which the count below
        // still checks.
        assert_eq!(
            btree_entry_count(image, seed),
            0,
            "seed {seed}: entries of a scalar index survived the declaration that made them"
        );
        return;
    }

    // The rows as the recovered database has them, read by a full scan on the
    // primary key — the answer every probe below is compared against.
    let scanned = recovered
        .query("SELECT id, bucket, label, name FROM docs ORDER BY id", &[])
        .unwrap_or_else(|e| panic!("seed {seed}: scan failed: {e}"));
    let stored: Vec<(u64, i64, String, String)> = scanned
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1], &row[2], &row[3]) {
            (Value::Integer(id), Value::Integer(bucket), Value::Text(label), Value::Text(name)) => {
                (*id as u64, *bucket, label.clone(), name.clone())
            }
            other => panic!("seed {seed}: unexpected row {other:?}"),
        })
        .collect();

    let ids = |result: inlaysql::ResultSet| -> Vec<u64> {
        let mut ids: Vec<u64> = result
            .rows
            .iter()
            .map(|row| match row[0] {
                Value::Integer(id) => id as u64,
                ref other => panic!("seed {seed}: expected an integer id, got {other:?}"),
            })
            .collect();
        ids.sort_unstable();
        ids
    };

    for (id, bucket, label, name) in &stored {
        // The unique index: one probe, one right answer.
        let found = ids(recovered
            .query(
                "SELECT id FROM docs WHERE label = ?",
                &[Value::Text(label.clone())],
            )
            .unwrap_or_else(|e| panic!("seed {seed}: label probe failed: {e}")));
        assert_eq!(
            found,
            vec![*id],
            "seed {seed}: the unique index disagrees with the rows about document {id} \
             (surviving rows: {surviving:?})"
        );

        // The non-unique one: the probe must return exactly the group the scan
        // says is there.
        let mut expected: Vec<u64> = stored
            .iter()
            .filter(|(_, other, _, _)| other == bucket)
            .map(|(id, _, _, _)| *id)
            .collect();
        expected.sort_unstable();
        let found = ids(recovered
            .query(
                "SELECT id FROM docs WHERE bucket = ?",
                &[Value::Integer(*bucket)],
            )
            .unwrap_or_else(|e| panic!("seed {seed}: bucket probe failed: {e}")));
        assert_eq!(
            found, expected,
            "seed {seed}: the scalar index disagrees with the rows about bucket {bucket}"
        );

        // And the composite, which is the two of them at once.
        let found = ids(recovered
            .query(
                "SELECT id FROM docs WHERE bucket = ? AND label = ?",
                &[Value::Integer(*bucket), Value::Text(label.clone())],
            )
            .unwrap_or_else(|e| panic!("seed {seed}: composite probe failed: {e}")));
        assert_eq!(
            found,
            vec![*id],
            "seed {seed}: the composite index disagrees"
        );

        // The collated index (AHL-469). Probed with the *upper-cased* spelling,
        // which no row necessarily holds, so only a folded key finds anything
        // at all — and it has to find every row whose name folds to the same
        // thing, not just the one whose bytes match.
        let mut expected: Vec<u64> = stored
            .iter()
            .filter(|(_, _, _, other)| other.eq_ignore_ascii_case(name))
            .map(|(id, _, _, _)| *id)
            .collect();
        expected.sort_unstable();
        let found = ids(recovered
            .query(
                "SELECT id FROM docs WHERE name = ?",
                &[Value::Text(name.to_ascii_uppercase())],
            )
            .unwrap_or_else(|e| panic!("seed {seed}: collated probe failed: {e}")));
        assert_eq!(
            found, expected,
            "seed {seed}: the NOCASE index disagrees with the rows about name `{name}`"
        );

        // And the second index over `label`, keyed under the *other*
        // collation: the same one right answer through different bytes.
        let found = ids(recovered
            .query(
                "SELECT id FROM docs WHERE label = ? COLLATE NOCASE",
                &[Value::Text(label.to_ascii_uppercase())],
            )
            .unwrap_or_else(|e| panic!("seed {seed}: second collated probe failed: {e}")));
        assert_eq!(
            found,
            vec![*id],
            "seed {seed}: the second index on `label` disagrees"
        );
    }

    // A range over the whole index has to be the whole table, and nothing more.
    let all = ids(recovered
        .query("SELECT id FROM docs WHERE bucket >= 0", &[])
        .unwrap_or_else(|e| panic!("seed {seed}: range scan failed: {e}")));
    let mut expected = surviving.to_vec();
    expected.sort_unstable();
    assert_eq!(
        all, expected,
        "seed {seed}: a range over the scalar index is not the table"
    );

    assert_eq!(
        btree_entry_count(image, seed),
        stored.len() * declared.len(),
        "seed {seed}: the recovered database holds {} rows and {} scalar indexes, so it must \
         hold exactly that many index entries — a different number means the rows and the \
         entries recovered to different moments.\n\
         \n\
         If this fires, read `assert_btree_indexes_agree`'s doc comment first. The one time \
         it has fired so far (AHL-406) it was **not** an index-maintenance bug: the rows and \
         the entries are written in one transaction and cannot be separated by the engine. It \
         was the storage layer recovering to a state that was never committed, because a \
         rewound page allocator handed out an id that was already occupied. Before suspecting \
         index maintenance, check whether `SELECT id FROM docs` alone returns a count no \
         commit produced, and log every commit boundary for the seed — a statement whose \
         writes really were split would show two records for one `INSERT`.",
        stored.len(),
        declared.len()
    );
}

/// Every scalar index entry in the durable image, counted straight out of the
/// tree rather than through the engine.
///
/// The engine reads each candidate row and drops the ones that are gone, so an
/// entry pointing at a row that no longer exists is invisible from SQL. It is
/// not invisible here.
fn btree_entry_count(image: &[u8], seed: u64) -> usize {
    let tree = CowBTree::open(SimDisk::with_image(BLOCK, image))
        .unwrap_or_else(|e| panic!("seed {seed}: reopening the tree directly failed: {e}"));
    tree.scan_prefix(b"\x01idx:")
        .unwrap_or_else(|e| panic!("seed {seed}: scanning the index namespace failed: {e}"))
        .len()
}

/// Every returned id must be a row the recovered database actually contains.
fn assert_surviving(result: &inlaysql::ResultSet, surviving: &[u64], seed: u64) {
    for row in &result.rows {
        let Value::Integer(id) = row[0] else {
            panic!("seed {seed}: expected an integer id");
        };
        assert!(
            surviving.contains(&(id as u64)),
            "seed {seed}: the index returned document {id}, which the rows do not contain \
             (surviving rows: {surviving:?})"
        );
    }
}

#[test]
fn indexes_recover_in_step_with_their_rows() {
    for seed in 0..60 {
        for (i, shape) in SHAPES.iter().enumerate() {
            // A different seed per shape, so 60 iterations cover 240 schedules
            // rather than the same 60 four times over.
            sweep(seed ^ (0xa118_0383u64.wrapping_mul(i as u64)), *shape);
        }
    }
}

#[test]
#[ignore = "expensive: run with --release -- --ignored, or in CI"]
fn thousands_of_seeds_recover_in_step() {
    // Half the seeds, four times the shapes: ten thousand schedules either way,
    // now spread over the paged backend as well rather than concentrated on two
    // configurations of the same in-memory one.
    for seed in 0..2_500 {
        for (i, shape) in SHAPES.iter().enumerate() {
            sweep(seed ^ (0xa118_0383u64.wrapping_mul(i as u64)), *shape);
        }
    }
}

#[test]
fn the_same_seed_replays_identically() {
    // The whole harness is worthless if a failure cannot be reproduced.
    let image = |seed: u64| {
        let simulator = Rc::new(RefCell::new(Simulator::with_disk(
            seed,
            SimDisk::with_block_size(BLOCK, CAPACITY),
            FaultSchedule::random_with(seed, 10, 10, 0),
        )));
        let mut db = Database::open_on(simulator.clone()).unwrap();
        let _ = db.execute(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR(4))",
            &[],
        );
        for id in 1..=8u64 {
            let _ = db.execute(
                "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
                &[
                    Value::Integer(id as i64),
                    Value::Text(body(id)),
                    Value::Vector(embedding(id)),
                ],
            );
        }
        let _ = db.checkpoint();
        let bytes = simulator.borrow().disk().durable().to_vec();
        drop(db);
        bytes
    };
    assert_eq!(image(7), image(7), "the same seed produced two disk images");
    assert_eq!(DIM, 4, "the embedding helper and the schema must agree");
}

/// The schedule that exposed AHL-406, kept as its regression test — it runs in
/// a tenth of a second instead of the six minutes the sweep takes.
///
/// # What it used to show
///
/// Recovery produced a database holding **no rows**, 19 entries for one scalar
/// index, 4 for another, none for the third, and 19 change records — a
/// combination no commit ever wrote. Index entries are ordinary rows in the
/// same copy-on-write tree, so one tree is one root is one moment; a root
/// cannot hold nineteen entries describing rows it does not have.
///
/// # The root cause: a page id handed out twice
///
/// It was never a split statement, and never index maintenance. Logging every
/// commit boundary for this seed showed each `INSERT` reaching the log as
/// exactly one record carrying the row, its three index entries, the change
/// record and the counters — atomic, as designed. What was not atomic was the
/// *file*:
///
/// 1. Commit `seq=38` found WAL region 0 full, so it took the wrap path in
///    `CowBTree::commit`: publish the committed state `(root 498, next 499,
///    seq 37)` to the state block, sync, then truncate the region and reuse it.
/// 2. **The scheduled crash landed on exactly that sync.** The state block
///    never reached the platter, so it still read `(root 251, next 252,
///    seq 28)`. `Device::sync` cannot report this — a power loss is modelled as
///    the process being dead — so the wrap carried on, zeroed region 0 (which
///    held records 29 through 37, the only durable trace of those commits) and
///    synced successfully.
/// 3. The next commit read the committed state and found `(251, 252, 28)`:
///    record 38 no longer chains onto 28, so the walk stops there. The handle
///    correctly **rewound** its root to 251 — those commits really are gone.
///    It also rewound `next_page_id` from 533 to 252.
/// 4. From there `alloc_page` handed out 252, 253, 254 … — page ids the
///    abandoned commits had already written. That breaks the one assumption
///    `btree/cache.rs` is built on: *a page id names one immutable sequence of
///    bytes for the lifetime of the file*, so the cache needs no invalidation.
///    The cache still held the abandoned pages 252 and 253, the following
///    commits descended into them, and the root recovery landed on reached
///    leaves from **two different timelines at once** — the index entries and
///    change records of one, the (absent) rows of the other. Nothing errors:
///    no checksum fails and no decode fails, because both timelines wrote
///    well-formed pages.
///
/// The three preconditions follow from that and are not a coincidence: the
/// paged ANN graph is what makes commits large enough to wrap the region
/// mid-workload, the BM25 index is what makes `checkpoint()` issue further
/// commits after the lost sync, and `checkpoint()` is what runs them.
///
/// # The fix
///
/// `CowBTree::adopt_next_page_id` — the page allocator is monotonic per handle
/// and never rewinds, however far back the committed state goes. A root can go
/// backwards; a page id must not be reused. The pages the abandoned commits
/// left behind are unreachable from any committed root and are simply skipped.
/// `a_rewound_committed_state_never_recycles_a_page_id` in `btree/tree.rs` pins
/// the invariant directly.
///
/// # What had been ruled out along the way, and was right to be
///
/// * **Not the torn-state-block fallback.** The state block parses cleanly, so
///   `read_committed_state` takes its verified path.
/// * **Not lost log records**, **not a scan that hides them**, and **not
///   `checkpoint` outrunning its pages** — each disproved by experiment; see the
///   history of this file.
/// * **Not the paged ANN backend committing inside another component's
///   transaction.** That was the last standing hypothesis and it was also
///   wrong: the commit log shows `VectorIndex::prepare_commit` and
///   `Engine::refresh_indexes` producing ordinary standalone commits that never
///   share a record with a row.
#[test]
fn the_known_mixed_recovery_seed() {
    sweep(1282u64 ^ (0xa118_0383u64.wrapping_mul(2)), SHAPES[2]);
}
