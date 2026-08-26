//! Asking for the index build, instead of ambushing whichever query is first.
//!
//! Index commits are deferred: a write leaves the work pending and the first
//! read that needs the index does all of it. That is the right trade for a
//! database taking a row at a time and the wrong one after a bulk load — the
//! ann-benchmarks run measured 258.7 s of a 294.9 s glove-25 load happening
//! inside one innocent `SELECT`. `REINDEX` (and `Engine::reindex`, and
//! `OPTIMIZE TABLE` over the wire) is how to ask for it up front instead.
//!
//! **The assertions count backend commits and the documents they carried, not
//! elapsed time.** "The first query no longer pays the build" is a claim about
//! *work*, and a call count states it exactly where a timing threshold would
//! state something weaker and flake — the same reason `foreign_commit_indexes.
//! rs` counts `insert` calls. The counting backend below also holds its
//! documents back until `commit`, so a build that did not happen is not merely
//! uncounted: the search comes back empty, and every "the handle is still
//! correct" assertion here is a real one.

use std::cell::Cell;
use std::rc::Rc;

use inlaysql_core::mem::{BruteForceVectorIndex, LogicalClock, MemFullTextIndex, MemStorage};
use inlaysql_core::traits::{
    Cancel, FullTextIndex, IndexFactory, RowFilter, RowId, Scored, Stopped, VectorIndex,
};
use inlaysql_core::{Engine, Error, Value};

// ------------------------------------------------------ counting backends

/// What the index backends of one engine have done since it was opened.
#[derive(Clone, Default)]
struct Work {
    /// Commits that had something to do.
    builds: Rc<Cell<usize>>,
    /// Documents those commits made searchable.
    documents: Rc<Cell<usize>>,
}

/// A full-text backend whose documents only become searchable on `commit`.
///
/// That is the shape of every backend whose build is worth deferring — an
/// HNSW graph, a paged postings list — reduced to the part these tests are
/// about. `MemFullTextIndex` on its own indexes on `insert` and has a no-op
/// `commit`, so it could not tell a finished build from a skipped one.
struct Batched {
    inner: MemFullTextIndex,
    staged: Vec<(RowId, String)>,
    work: Work,
}

impl FullTextIndex for Batched {
    fn insert(&mut self, id: RowId, text: &str) -> inlaysql_core::Result<()> {
        self.staged.push((id, text.to_string()));
        Ok(())
    }

    fn remove(&mut self, id: RowId) -> inlaysql_core::Result<()> {
        self.staged.retain(|(staged, _)| *staged != id);
        self.inner.remove(id)
    }

    fn commit(&mut self) -> inlaysql_core::Result<()> {
        if self.staged.is_empty() {
            return Ok(());
        }
        self.work.builds.set(self.work.builds.get() + 1);
        self.work
            .documents
            .set(self.work.documents.get() + self.staged.len());
        for (id, text) in self.staged.drain(..) {
            self.inner.insert(id, &text)?;
        }
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
}

/// Deliberately not persistable: `save` answering `None` keeps every test here
/// about the build rather than about a blob the engine could have restored
/// from instead.
#[derive(Clone, Default)]
struct BatchedFactory {
    work: Work,
}

impl IndexFactory for BatchedFactory {
    fn full_text(
        &self,
        _table: &str,
        _column: &str,
    ) -> inlaysql_core::Result<Box<dyn FullTextIndex>> {
        Ok(Box::new(Batched {
            inner: MemFullTextIndex::new(),
            staged: Vec::new(),
            work: self.work.clone(),
        }))
    }

    fn vector(
        &self,
        _table: &str,
        _column: &str,
        dim: usize,
        metric: inlaysql_core::hnsw::VectorMetric,
    ) -> inlaysql_core::Result<Box<dyn VectorIndex>> {
        Ok(Box::new(BruteForceVectorIndex::with_metric(dim, metric)))
    }
}

// ------------------------------------------------------------- the fixture

const ROWS: i64 = 60;

fn open() -> (Engine, Work) {
    let factory = BatchedFactory::default();
    let work = factory.work.clone();
    let engine = Engine::open(
        Box::new(MemStorage::new()),
        Box::new(factory),
        Box::new(LogicalClock::default()),
    )
    .expect("open");
    (engine, work)
}

/// An engine holding `docs` and `notes`, each with a full-text index and
/// [`ROWS`] rows, and neither of them built yet.
fn loaded() -> (Engine, Work) {
    let (mut engine, work) = open();
    for table in ["docs", "notes"] {
        engine
            .execute(
                &format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, body TEXT)"),
                &[],
            )
            .expect("create");
        engine
            .execute(
                &format!("CREATE INDEX {table}_body ON {table} (body) USING FULLTEXT"),
                &[],
            )
            .expect("create index");
    }
    let insert_docs = engine
        .prepare("INSERT INTO docs (id, body) VALUES (?, ?)")
        .expect("prepare");
    let insert_notes = engine
        .prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
        .expect("prepare");
    engine.begin().expect("begin");
    for id in 1..=ROWS {
        engine
            .run(
                &insert_docs,
                &[
                    Value::Integer(id),
                    Value::Text(format!("alpha document {id}").into()),
                ],
            )
            .expect("insert");
        engine
            .run(
                &insert_notes,
                &[
                    Value::Integer(id),
                    Value::Text(format!("beta note {id}").into()),
                ],
            )
            .expect("insert");
    }
    engine.commit().expect("commit");
    (engine, work)
}

/// How many rows of `table` a full-text search for `term` finds.
fn hits(engine: &mut Engine, table: &str, term: &str) -> usize {
    engine
        .query(
            &format!("SELECT id, bm25_score(body, '{term}') AS s FROM {table} ORDER BY s DESC"),
            &[],
        )
        .expect("search")
        .rows
        .len()
}

// ------------------------------------------------------------- the claims

/// The point of the whole change: after an explicit build, the first query
/// costs nothing that the build already paid for.
///
/// Three moments are asserted, and the middle one is what makes the first and
/// last mean anything: the load itself builds nothing, the `REINDEX` builds
/// everything, and the query that follows builds *nothing at all*. A version
/// where `REINDEX` ran the build but failed to clear the pending state would
/// pass the second assertion and fail the third.
#[test]
fn an_explicit_build_moves_the_cost_off_the_first_query() {
    let (mut engine, work) = loaded();
    assert_eq!(
        work.documents.get(),
        0,
        "the load itself built an index; the deferral is gone"
    );

    engine.execute("REINDEX", &[]).expect("reindex");
    let built = work.documents.get();
    assert_eq!(
        built,
        2 * ROWS as usize,
        "REINDEX left documents unbuilt: {built}"
    );
    let builds = work.builds.get();

    assert_eq!(hits(&mut engine, "docs", "alpha"), ROWS as usize);
    assert_eq!(hits(&mut engine, "notes", "beta"), ROWS as usize);
    assert_eq!(
        (work.builds.get(), work.documents.get()),
        (builds, built),
        "the first query after REINDEX still paid for a build"
    );
}

/// The control, and the constraint the change had to keep: nothing about the
/// default moved. A load that never asks for a build still does not pay for
/// one, and the query that needs the index is still what triggers it.
#[test]
fn the_deferral_is_unchanged_for_a_loader_that_never_asks() {
    let (mut engine, work) = loaded();
    assert_eq!(
        (work.builds.get(), work.documents.get()),
        (0, 0),
        "a bulk load paid for an index build it did not ask for"
    );

    assert_eq!(hits(&mut engine, "docs", "alpha"), ROWS as usize);
    assert_eq!(
        work.documents.get(),
        2 * ROWS as usize,
        "the first query did not do the deferred build"
    );
}

/// A build with nothing pending does nothing and says so. This is what makes
/// `REINDEX` safe to put in a cron job or run after every deploy.
#[test]
fn a_build_with_nothing_pending_is_a_no_op_that_reports_one() {
    let (mut engine, work) = loaded();

    let first = engine.reindex(None).expect("first build");
    assert_eq!(
        first.indexes,
        vec!["docs_body".to_string(), "notes_body".to_string()],
        "the report does not name what was built"
    );
    assert!(!first.is_empty());
    let after = (work.builds.get(), work.documents.get());

    let second = engine.reindex(None).expect("second build");
    assert!(
        second.is_empty(),
        "a second build claimed to rebuild {:?}",
        second.indexes
    );
    assert_eq!(
        (work.builds.get(), work.documents.get()),
        after,
        "a build with nothing pending still did work"
    );

    // And a third, through SQL rather than the method, for the same reason.
    engine.execute("REINDEX docs", &[]).expect("narrow build");
    assert_eq!((work.builds.get(), work.documents.get()), after);
}

/// `REINDEX <table>` builds that table and leaves the rest where they were —
/// still pending, not silently marked current. The failure this pins is the
/// one that would be invisible: clearing the engine's single dirty flag after
/// a narrowed build would tell the next read that `notes` was up to date, and
/// its search would answer nothing with no error anywhere.
#[test]
fn a_narrowed_build_leaves_the_other_tables_pending() {
    let (mut engine, work) = loaded();

    let narrowed = engine.reindex(Some("docs")).expect("narrow build");
    assert_eq!(narrowed.indexes, vec!["docs_body".to_string()]);
    assert_eq!(
        work.documents.get(),
        ROWS as usize,
        "a build narrowed to `docs` built more than `docs`"
    );

    // And it says so a second time round: `docs` is current now even though
    // `notes` is not, which a single engine-wide dirty flag could not have
    // told apart — it would have reported a rebuild of an index it knew had
    // nothing to do.
    let again = engine.reindex(Some("docs")).expect("second narrow build");
    assert!(
        again.is_empty(),
        "a second narrowed build claimed to rebuild {:?}",
        again.indexes
    );

    // `notes` was left pending, so the read that needs it does its build —
    // and finds every row.
    assert_eq!(hits(&mut engine, "notes", "beta"), ROWS as usize);
    assert_eq!(work.documents.get(), 2 * ROWS as usize);
}

/// The name in the statement is resolved, never assumed: a typo is a refusal
/// rather than a full-database rebuild, and an index name means that index
/// rather than its table's siblings.
#[test]
fn reindex_resolves_the_name_it_was_given() {
    let (mut engine, work) = loaded();

    let error = engine
        .execute("REINDEX nosuchthing", &[])
        .expect_err("an unknown name must be refused");
    assert!(
        matches!(&error, Error::Catalog(message) if message.contains("unable to identify")),
        "{error}"
    );
    assert_eq!(work.documents.get(), 0, "a refused REINDEX built something");

    engine
        .execute("REINDEX docs_body", &[])
        .expect("an index name");
    assert_eq!(
        work.documents.get(),
        ROWS as usize,
        "REINDEX <index> built more than the one index it names"
    );
}

/// `REINDEX` is recognised ahead of `sqlparser`, which has no such statement,
/// so it has to skip the same leading trivia every other statement does.
///
/// A comment in front of it is not a hypothetical: it is what a driver and an
/// ORM put in front of everything they send. Getting this wrong is a parse
/// error rather than a wrong answer, but it is still a statement that works
/// everywhere except behind a comment.
#[test]
fn reindex_is_found_behind_whatever_leads_the_statement() {
    for sql in [
        "REINDEX",
        "  reindex  ",
        "REINDEX;",
        "REINDEX docs ;",
        "/* app-name */ REINDEX",
        "-- a line comment\nREINDEX",
        "/* one */ /* two */ REINDEX docs",
        "REINDEX -- trailing",
        "REINDEX \"docs\"",
    ] {
        let (mut engine, _) = loaded();
        engine
            .execute(sql, &[])
            .unwrap_or_else(|error| panic!("`{sql}`: {error}"));
    }

    // And a statement that merely starts with the same letters is nobody's
    // business but the parser's.
    let (mut engine, _) = loaded();
    let error = engine
        .execute("REINDEXED docs", &[])
        .expect_err("not a statement");
    assert!(matches!(error, Error::Parse(_)), "{error}");
}

// --------------------------------------------------------- stopping one

/// A cancellation signal that trips on the `budget`-th question.
struct Trip {
    budget: Cell<Option<u64>>,
    asked: Cell<u64>,
}

struct Signal(Rc<Trip>);

impl Cancel for Signal {
    fn stop(&self) -> Option<Stopped> {
        self.0.asked.set(self.0.asked.get() + 1);
        match self.0.budget.get() {
            None => None,
            Some(0) => Some(Stopped::Killed),
            Some(left) => {
                self.0.budget.set(Some(left - 1));
                None
            }
        }
    }
}

/// A forced build can be stopped, and stopping it leaves the handle answering
/// exactly what an un-run build would have left it answering.
///
/// This is the boundary `restore_indexes` refuses to cross, approached from
/// the other side. That one runs with the indexes already cleared, so an early
/// return strands a handle whose `bm25_score` silently answers nothing. This
/// one runs from a known state and stops only *between* backends, so at every
/// point it can stop, each backend is either fully committed or untouched and
/// the pending flag is still set — which is what the sweep below asserts by
/// reading both tables back at every stopping point.
#[test]
fn a_cancelled_build_leaves_the_work_pending_and_the_handle_correct() {
    let mut stops = 0;
    for budget in 0..8 {
        let (mut engine, work) = loaded();
        let trip = Rc::new(Trip {
            budget: Cell::new(Some(budget)),
            asked: Cell::new(0),
        });
        engine.set_cancel(Box::new(Signal(Rc::clone(&trip))));

        let outcome = engine.execute("REINDEX", &[]);
        trip.budget.set(None);
        match outcome {
            Err(Error::Cancelled(reason)) => {
                assert_eq!(reason, Stopped::Killed, "budget {budget}");
                stops += 1;
            }
            Ok(_) => {
                // The budget outran the statement, which is what proves the
                // sweep covered all of it rather than stopping short.
                assert!(
                    stops > 1,
                    "the sweep found only {stops} stopping point(s); it cannot have covered \
                     both index commits"
                );
                assert_eq!(work.documents.get(), 2 * ROWS as usize);
                return;
            }
            Err(other) => panic!("budget {budget}: {other}"),
        }

        // The handle is not poisoned and no document was lost: whatever the
        // cancelled build had not finished is still pending, so the reads
        // below do it and find every row.
        assert_eq!(
            hits(&mut engine, "docs", "alpha"),
            ROWS as usize,
            "budget {budget}: `docs` lost documents to a cancelled build"
        );
        assert_eq!(
            hits(&mut engine, "notes", "beta"),
            ROWS as usize,
            "budget {budget}: `notes` lost documents to a cancelled build"
        );
        assert_eq!(
            work.documents.get(),
            2 * ROWS as usize,
            "budget {budget}: the work a cancelled build skipped was never done"
        );

        // And the handle still takes writes, which a `discard_failed_statement`
        // that had reloaded it half-way would not guarantee on its own.
        engine
            .execute(
                "INSERT INTO docs (id, body) VALUES (9999, 'alpha afterwards')",
                &[],
            )
            .expect("write after a cancelled build");
        assert_eq!(hits(&mut engine, "docs", "alpha"), ROWS as usize + 1);
    }
    panic!("the build never ran to completion; the sweep is too short");
}
