//! The async-first API, and the runtime-free machinery behind it.
//!
//! # Why an I/O thread rather than async syscalls
//!
//! A database file is not a socket. There is no portable way to await a page
//! read: `pread` on a regular file always "completes immediately" as far as the
//! kernel's readiness interfaces are concerned, and then blocks for a
//! millisecond in the middle of your event loop. Every embedded database that
//! offers an async API therefore does one of two things — move the blocking
//! call off the caller's thread, or use a completion-based interface such as
//! `io_uring`. InlaySQL does both: this module moves the work off your thread,
//! and [`Database::open_on`] chooses what happens once it gets there.
//!
//! [`AsyncDatabase`] owns a dedicated I/O thread that owns the [`Database`].
//! Statements are sent to it and their results come back through a future, so
//! the caller's executor is never blocked on a page fault, an `fsync` or an
//! index rebuild. The engine itself stays single-threaded and synchronous,
//! which is what keeps it deterministic-simulation-testable — the concurrency
//! lives entirely at this boundary.
//!
//! # No runtime dependency
//!
//! The futures here are plain [`Future`] values. They work on Tokio,
//! async-std, smol, or on [`block_on`] below, which is a complete (if
//! minimal) executor for one future. Nothing in this crate depends on a
//! runtime, so embedding InlaySQL never forces one on you.
//!
//! ```
//! use inlaysql::{block_on, AsyncDatabase, Value};
//!
//! block_on(async {
//!     let db = AsyncDatabase::open_in_memory().await?;
//!     db.execute("CREATE TABLE t (a INTEGER)", &[]).await?;
//!     db.execute("INSERT INTO t (a) VALUES (?)", &[Value::Integer(7)])
//!         .await?;
//!     let rows = db.query("SELECT a FROM t", &[]).await?;
//!     assert_eq!(rows.rows, vec![vec![Value::Integer(7)]]);
//!     Ok::<(), inlaysql::Error>(())
//! })
//! # .unwrap();
//! ```

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle};

use inlaysql_core::btree::Device;

use crate::{Database, Error, Outcome, Result, ResultSet, Statement, Value};

/// Work handed to the I/O thread.
type Job = Box<dyn FnOnce(&mut Database) + Send>;

/// A database with an async API, driven by its own I/O thread.
///
/// Statements take `&self`, not `&mut self`: the engine's mutable state lives
/// on the I/O thread, so an `AsyncDatabase` can be shared (`Arc`) across tasks
/// — and across OS threads, since `AsyncDatabase` is `Send + Sync` — and the
/// thread serializes them in arrival order. That is the same single-writer
/// discipline SQLite has, expressed as a queue instead of a lock.
///
/// `mpsc::Sender<Job>` is `Send` but not `Sync`, so the queue is wrapped in a
/// `Mutex` to make the whole type `Sync`. That mutex guards only the `send`
/// call in [`submit`](Self::submit) — enqueueing a job on an unbounded
/// channel — never the result: a task still awaits its own completion slot
/// (see [`Task`]), so callers on different threads are not serialized against
/// each other, only against the moment their job joins the queue.
pub struct AsyncDatabase {
    /// `None` only while dropping, when the queue is being closed.
    jobs: Option<Mutex<Sender<Job>>>,
    /// `None` only while dropping, when the thread is being joined.
    worker: Option<JoinHandle<()>>,
}

impl AsyncDatabase {
    /// Open the database file at `path`, creating it if it does not exist.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        Self::spawn(move || Database::open(path)).await
    }

    /// Open a database that never touches the filesystem.
    pub async fn open_in_memory() -> Result<Self> {
        Self::spawn(Database::open_in_memory).await
    }

    /// Open a database on an already-constructed I/O backend.
    ///
    /// The device is moved onto the I/O thread and never leaves it, which is
    /// why it only has to be [`Send`] and not [`Sync`] — an `io_uring` ring is
    /// exactly that.
    pub async fn open_on<D: Device + Send + 'static>(device: D) -> Result<Self> {
        Self::spawn(move || Database::open_on(device)).await
    }

    /// Start the I/O thread and wait for the database to be open on it.
    async fn spawn(open: impl FnOnce() -> Result<Database> + Send + 'static) -> Result<Self> {
        let (jobs, inbox): (Sender<Job>, Receiver<Job>) = mpsc::channel();
        let (opened, ready) = channel::<Result<()>>();

        let worker = thread::Builder::new()
            .name("inlaysql-io".to_string())
            .spawn(move || {
                let mut db = match open() {
                    Ok(db) => {
                        opened.send(Ok(()));
                        db
                    }
                    // The thread ends here; queued jobs then fail fast with
                    // `Error::Storage` from `submit` rather than hanging.
                    Err(error) => {
                        opened.send(Err(error));
                        return;
                    }
                };
                while let Ok(job) = inbox.recv() {
                    job(&mut db);
                }
            })
            .map_err(|error| Error::Storage(format!("cannot start the I/O thread: {error}")))?;

        match ready.await {
            Ok(()) => Ok(Self {
                jobs: Some(Mutex::new(jobs)),
                worker: Some(worker),
            }),
            Err(error) => {
                let _ = worker.join();
                Err(error)
            }
        }
    }

    /// Run a statement, binding `?` placeholders from `params` in order.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Task<Result<Outcome>> {
        let (sql, params) = (sql.to_string(), params.to_vec());
        self.submit(move |db| db.execute(&sql, &params))
    }

    /// Run a statement that returns rows.
    pub fn query(&self, sql: &str, params: &[Value]) -> Task<Result<ResultSet>> {
        let (sql, params) = (sql.to_string(), params.to_vec());
        self.submit(move |db| db.query(&sql, &params))
    }

    /// Parse and plan a statement once, to run it many times.
    ///
    /// See [`Database::prepare`]. The returned [`Statement`] is
    /// reference-counted, so keeping it and handing clones to several tasks
    /// costs nothing beyond the refcount.
    ///
    /// ```
    /// use inlaysql::{block_on, AsyncDatabase, Value};
    ///
    /// block_on(async {
    ///     let db = AsyncDatabase::open_in_memory().await?;
    ///     db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])
    ///         .await?;
    ///
    ///     let insert = db.prepare("INSERT INTO kv (id, body) VALUES (?, ?)").await?;
    ///     for id in 1..=3 {
    ///         db.execute_prepared(&insert, &[Value::Integer(id), Value::Text("x".into())])
    ///             .await?;
    ///     }
    ///
    ///     let rows = db.query("SELECT id FROM kv", &[]).await?;
    ///     assert_eq!(rows.rows.len(), 3);
    ///     Ok::<(), inlaysql::Error>(())
    /// })
    /// # .unwrap();
    /// ```
    pub fn prepare(&self, sql: &str) -> Task<Result<Statement>> {
        let sql = sql.to_string();
        self.submit(move |db| db.prepare(&sql))
    }

    /// Run a prepared statement, binding `?` placeholders from `params`.
    pub fn execute_prepared(
        &self,
        statement: &Statement,
        params: &[Value],
    ) -> Task<Result<Outcome>> {
        let (statement, params) = (statement.clone(), params.to_vec());
        self.submit(move |db| db.execute_prepared(&statement, &params))
    }

    /// Run a prepared statement that returns rows.
    pub fn query_prepared(
        &self,
        statement: &Statement,
        params: &[Value],
    ) -> Task<Result<ResultSet>> {
        let (statement, params) = (statement.clone(), params.to_vec());
        self.submit(move |db| db.query_prepared(&statement, &params))
    }

    /// Write the retrieval indexes into the database file now.
    ///
    /// See [`Database::checkpoint`].
    pub fn checkpoint(&self) -> Task<Result<()>> {
        self.submit(|db| db.checkpoint())
    }

    /// Start an explicit transaction.
    ///
    /// See [`Database::begin`]. The transaction's state lives on the I/O
    /// thread, so statements submitted between `begin` and `commit` — from any
    /// task — take part in it, in arrival order. Await the begin before issuing
    /// the transaction's statements, and await the commit before issuing more.
    pub fn begin(&self) -> Task<Result<()>> {
        self.submit(|db| db.begin())
    }

    /// Commit the open transaction.
    ///
    /// See [`Database::commit`].
    pub fn commit(&self) -> Task<Result<()>> {
        self.submit(|db| db.commit())
    }

    /// Roll back the open transaction.
    ///
    /// See [`Database::rollback`].
    pub fn rollback(&self) -> Task<Result<()>> {
        self.submit(|db| db.rollback())
    }

    /// Committed row changes after `from`, in commit order.
    ///
    /// See [`Database::changes`].
    pub fn changes(&self, from: u64) -> Task<Result<crate::Changes>> {
        self.submit(move |db| db.changes(from))
    }

    /// Run an arbitrary closure against the database on the I/O thread.
    ///
    /// The escape hatch for anything the async surface does not cover yet
    /// (reading the catalog, a batch of statements that must not interleave
    /// with another task's).
    pub fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Task<Result<T>> {
        self.submit(f)
    }

    fn submit<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Task<Result<T>> {
        let (completion, task) = channel::<Result<T>>();
        // A second handle on the same slot, so the task can still be completed
        // if the job never reaches the I/O thread.
        let orphan = Completion {
            slot: task.slot.clone(),
        };
        let job: Job = Box::new(move |db| completion.send(f(db)));

        // The lock is held only long enough to enqueue `job` on the unbounded
        // channel — never across the awaited result — so concurrent callers
        // on different threads are serialized against the moment their job
        // joins the queue, not against each other's statements running.
        //
        // A poisoned lock is recovered from rather than propagated: the only
        // thing it guards is one `send`, which cannot leave the queue half
        // modified, so there is no invariant a panicking holder could have
        // broken. A database handle that refuses every later statement because
        // some unrelated task panicked would be the worse failure.
        let delivered = match &self.jobs {
            Some(jobs) => jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .send(job)
                .is_ok(),
            None => false,
        };
        if !delivered {
            // The I/O thread is gone (it failed while opening, or panicked in
            // an earlier job). Complete now rather than await forever.
            orphan.send(Err(Error::Storage(
                "the database I/O thread is no longer running".to_string(),
            )));
        }
        task
    }
}

impl Drop for AsyncDatabase {
    fn drop(&mut self) {
        // Dropping the sender ends the worker's `recv` loop; joining makes the
        // database file quiescent before `drop` returns, so a caller can reopen
        // the same path immediately afterwards.
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// ------------------------------------------------------------------ the future

/// A future for one submitted statement.
///
/// Completes when the I/O thread has run the statement. Dropping it is safe
/// and does not cancel the statement — the write has already been queued, and
/// pretending otherwise would be a lie about durability.
pub struct Task<T> {
    slot: Arc<Mutex<Slot<T>>>,
}

struct Slot<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

/// The sending half of a [`Task`].
struct Completion<T> {
    slot: Arc<Mutex<Slot<T>>>,
}

impl<T> Completion<T> {
    fn send(self, value: T) {
        let waker = {
            let mut slot = match self.slot.lock() {
                Ok(slot) => slot,
                // The only way the mutex is poisoned is a panic while a task
                // was polling. Nothing to wake, and nothing to fix.
                Err(_) => return,
            };
            slot.value = Some(value);
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

fn channel<T>() -> (Completion<T>, Task<T>) {
    let slot = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
    }));
    (Completion { slot: slot.clone() }, Task { slot })
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut slot = self
            .slot
            .lock()
            .expect("a task panicked while holding the result slot");
        match slot.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                // Always store the *current* waker: a future can be moved
                // between executor threads between polls.
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

// ---------------------------------------------------------------- the executor

/// Drive one future to completion on the current thread.
///
/// A minimal executor, so that a synchronous program (a CLI, a test, a
/// `main` that has no reason to be async) can use the async API without
/// pulling in a runtime. It parks the thread between wakeups rather than
/// spinning.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let signal = Arc::new(Signal::default());
    let waker = Waker::from(signal.clone());
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => signal.wait(),
        }
    }
}

#[derive(Default)]
struct Signal {
    woken: Mutex<bool>,
    change: Condvar,
}

impl Signal {
    fn wait(&self) {
        let mut woken = self.woken.lock().expect("signal mutex poisoned");
        while !*woken {
            woken = self.change.wait(woken).expect("signal mutex poisoned");
        }
        *woken = false;
    }
}

impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Set the flag *before* notifying: a wake that arrives while the
        // executor is polling (not yet waiting) must not be lost.
        let mut woken = match self.woken.lock() {
            Ok(woken) => woken,
            Err(_) => return,
        };
        *woken = true;
        self.change.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_run_on_the_io_thread_and_results_come_back() {
        block_on(async {
            let db = AsyncDatabase::open_in_memory().await.unwrap();
            db.execute("CREATE TABLE t (a INTEGER)", &[]).await.unwrap();
            db.execute("INSERT INTO t (a) VALUES (1), (2)", &[])
                .await
                .unwrap();
            let rows = db
                .query("SELECT a FROM t ORDER BY a DESC", &[])
                .await
                .unwrap();
            assert_eq!(
                rows.rows,
                vec![vec![Value::Integer(2)], vec![Value::Integer(1)]]
            );
        });
    }

    #[test]
    fn the_caller_thread_is_never_the_one_running_the_statement() {
        block_on(async {
            let db = AsyncDatabase::open_in_memory().await.unwrap();
            let here = thread::current().id();
            let there = db.with(move |_| Ok(thread::current().id())).await.unwrap();
            assert_ne!(here, there, "the statement ran on the caller's thread");
        });
    }

    #[test]
    fn an_error_from_opening_is_reported_not_swallowed() {
        let outcome = block_on(AsyncDatabase::open("/definitely/not/a/directory/db.inlay"));
        assert!(outcome.is_err(), "opening an unwritable path should fail");
    }

    #[test]
    fn statements_are_serialized_in_arrival_order() {
        block_on(async {
            let db = AsyncDatabase::open_in_memory().await.unwrap();
            db.execute("CREATE TABLE t (a INTEGER)", &[]).await.unwrap();

            // Queue three inserts without awaiting: the I/O thread must apply
            // them in the order they were submitted.
            let first = db.execute("INSERT INTO t (a) VALUES (1)", &[]);
            let second = db.execute("INSERT INTO t (a) VALUES (2)", &[]);
            let third = db.execute("INSERT INTO t (a) VALUES (3)", &[]);
            for task in [first, second, third] {
                task.await.unwrap();
            }

            let rows = db.query("SELECT a FROM t", &[]).await.unwrap();
            assert_eq!(
                rows.rows,
                vec![
                    vec![Value::Integer(1)],
                    vec![Value::Integer(2)],
                    vec![Value::Integer(3)]
                ]
            );
        });
    }

    #[test]
    fn a_dropped_task_does_not_cancel_the_write() {
        block_on(async {
            let db = AsyncDatabase::open_in_memory().await.unwrap();
            db.execute("CREATE TABLE t (a INTEGER)", &[]).await.unwrap();
            drop(db.execute("INSERT INTO t (a) VALUES (9)", &[]));
            let rows = db.query("SELECT a FROM t", &[]).await.unwrap();
            assert_eq!(rows.rows, vec![vec![Value::Integer(9)]]);
        });
    }

    #[test]
    fn a_transaction_commits_and_rolls_back_over_the_async_surface() {
        block_on(async {
            let db = AsyncDatabase::open_in_memory().await.unwrap();
            db.execute("CREATE TABLE t (a INTEGER)", &[]).await.unwrap();

            db.begin().await.unwrap();
            db.execute("INSERT INTO t (a) VALUES (1)", &[])
                .await
                .unwrap();
            db.execute("INSERT INTO t (a) VALUES (2)", &[])
                .await
                .unwrap();
            db.commit().await.unwrap();
            let rows = db.query("SELECT a FROM t", &[]).await.unwrap();
            assert_eq!(rows.rows.len(), 2);

            db.begin().await.unwrap();
            db.execute("INSERT INTO t (a) VALUES (3)", &[])
                .await
                .unwrap();
            db.rollback().await.unwrap();
            let rows = db.query("SELECT a FROM t", &[]).await.unwrap();
            assert_eq!(rows.rows.len(), 2);
        });
    }
}
