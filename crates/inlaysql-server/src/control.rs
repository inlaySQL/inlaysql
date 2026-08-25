//! Stopping a statement: the per-connection cancellation signal, and the
//! registry `KILL` reaches it through.
//!
//! Everything else in this server is per connection and shares nothing but the
//! database file. This module is the one exception, and it exists because the
//! two things a shared SQL server needs — a statement timeout and a way to end
//! somebody else's runaway query — cannot be built anywhere else:
//!
//! * The engine core is `no_std`. It cannot read a clock and it cannot be
//!   interrupted by a thread, so it can only *ask* whether to carry on
//!   ([`inlaysql::Cancel`]). [`Control`] is this server's answer.
//! * `KILL` is one connection acting on another, so the flag has to be
//!   reachable from a thread that does not own the target's handle. That is
//!   the whole of [`Registry`].
//!
//! # What a killed statement leaves behind
//!
//! Nothing. The engine only notices cancellation while a statement is
//! producing or collecting rows, never while it is making them durable, so a
//! cancelled write leaves through the same statement-atomicity path a `CHECK`
//! violation leaves through: the buffered rows are discarded and the handle is
//! reloaded. `wire.rs` pins that with a cancelled `UPDATE` — the table is as
//! it was, and the connection's next statement works.

use std::collections::HashMap;
use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use inlaysql::{Cancel, Stopped};

use crate::errors::MysqlError;

/// What a `KILL` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillScope {
    /// `KILL QUERY <id>`: stop the statement, keep the connection.
    Query,
    /// `KILL [CONNECTION] <id>`: stop the statement and close the connection.
    Connection,
}

/// One connection's cancellation state.
///
/// Shared three ways, which is why every field is an atomic or a mutex: the
/// connection thread reads and writes it, the engine reads it through
/// [`Signal`] from inside its scan loops, and any *other* connection's thread
/// writes it through [`Registry::kill`].
pub struct Control {
    /// The connection id, as reported to the client and in `CONNECTION_ID()`.
    id: u32,
    /// The account this connection authenticated as, empty until it has. Read
    /// by `KILL` to decide whether the asker owns this connection.
    user: Mutex<String>,
    /// Stop the statement in flight. Cleared at the start of every statement
    /// (see [`Cancel::statement_began`]) unless `closing` is set, so a `KILL
    /// QUERY` that arrived while the connection was idle does not fall on the
    /// next statement the client happens to send.
    interrupted: AtomicBool,
    /// The connection itself is going away, so `interrupted` is not cleared
    /// and the command loop exits after the current statement.
    closing: AtomicBool,
    /// Milliseconds one statement may run for; `0` is no limit. Set from
    /// `--max-execution-time` and by `SET max_execution_time`.
    timeout_ms: AtomicU64,
    /// Nanoseconds since `base` at which the statement in flight must stop;
    /// `0` means it has no deadline. An absolute instant rather than a
    /// remaining budget, so the hot path is one atomic load and one comparison
    /// and never a subtraction against a moving number.
    deadline: AtomicU64,
    /// The zero this connection measures from. Per connection rather than per
    /// process because it only ever has to be consistent with itself, and a
    /// process-wide one would need a lazy static nothing else here wants.
    base: Instant,
    /// A second descriptor onto this connection's socket.
    ///
    /// `KILL CONNECTION` on a connection that is *idle* has nothing to
    /// interrupt — the thread is parked in `read`, not in the engine — so the
    /// flag alone would not be noticed until the client sent something or the
    /// socket's own `wait_timeout` expired, which is up to eight hours. A
    /// shutdown from the killing thread makes that `read` return at once,
    /// which is what makes `KILL` mean the same thing to an idle connection as
    /// to a busy one.
    socket: Mutex<Option<TcpStream>>,
}

impl Control {
    /// A control for connection `id` with `timeout_ms` as its statement
    /// timeout (`0` for none).
    pub fn new(id: u32, timeout_ms: u64) -> Self {
        Self {
            id,
            user: Mutex::new(String::new()),
            interrupted: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            timeout_ms: AtomicU64::new(timeout_ms),
            deadline: AtomicU64::new(0),
            base: Instant::now(),
            socket: Mutex::new(None),
        }
    }

    /// A control with no accept loop behind it, for a [`crate::session::Session`]
    /// that is not being served over a socket — the unit tests here and in
    /// `shim`, `acl` and `infoschema`, which need a session to classify a
    /// statement against and have no connection at all.
    ///
    /// It is a real control, not a stub: nothing can `KILL` it because nothing
    /// registered it, and it enforces the timeout it is given like any other.
    #[cfg(test)]
    pub fn detached(id: u32) -> Arc<Self> {
        Arc::new(Self::new(id, 0))
    }

    /// [`Control::detached`] with a statement timeout.
    #[cfg(test)]
    pub fn detached_with_timeout(id: u32, timeout_ms: u64) -> Arc<Self> {
        Arc::new(Self::new(id, timeout_ms))
    }

    /// The connection id this controls.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Record the account this connection authenticated as.
    pub fn set_user(&self, user: &str) {
        if let Ok(mut held) = self.user.lock() {
            held.clear();
            held.push_str(user);
        }
    }

    /// Hand over the descriptor `KILL CONNECTION` shuts down. See `socket`.
    pub fn attach_socket(&self, socket: TcpStream) {
        if let Ok(mut held) = self.socket.lock() {
            *held = Some(socket);
        }
    }

    /// The statement timeout in milliseconds, `0` for none. This is the number
    /// `@@max_execution_time` reports, and it is read from here rather than
    /// from a copy so that what is reported cannot drift from what is applied.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.load(Ordering::Relaxed)
    }

    /// Change the statement timeout for this session.
    ///
    /// Takes effect on the *next* statement, because the deadline for the one
    /// in flight was fixed when it began — and the statement in flight is the
    /// `SET` itself.
    pub fn set_timeout_ms(&self, millis: u64) {
        self.timeout_ms.store(millis, Ordering::Relaxed);
    }

    /// Whether the connection has been told to close.
    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Relaxed)
    }

    /// Stop this connection, from another thread.
    fn kill(&self, scope: KillScope) {
        if scope == KillScope::Connection {
            self.closing.store(true, Ordering::Relaxed);
        }
        // After `closing`, so a statement that observes `interrupted` also
        // observes the reason it must not be cleared again.
        self.interrupted.store(true, Ordering::Relaxed);
        if scope == KillScope::Connection {
            if let Ok(socket) = self.socket.lock() {
                if let Some(socket) = socket.as_ref() {
                    // Best effort: a socket the peer has already closed answers
                    // `NotConnected` here, which is the outcome this wanted.
                    let _ = socket.shutdown(Shutdown::Both);
                }
            }
        }
    }
}

impl Cancel for Signal {
    fn statement_began(&self) {
        let control = &self.0;
        // A `KILL QUERY` only ever applies to a statement that was running when
        // it was issued. A `KILL CONNECTION` outlives every statement, so it is
        // not cleared.
        if !control.closing.load(Ordering::Relaxed) {
            control.interrupted.store(false, Ordering::Relaxed);
        }
        let millis = control.timeout_ms.load(Ordering::Relaxed);
        if millis == 0 {
            // No timeout configured means no clock read at all, per statement
            // or otherwise: this is the default, and it must cost nothing.
            control.deadline.store(0, Ordering::Relaxed);
            return;
        }
        let now = control.base.elapsed().as_nanos() as u64;
        // `max(1)`: zero is the "no deadline" sentinel, and a deadline that
        // landed exactly on the base instant would otherwise disable itself.
        let deadline = now.saturating_add(millis.saturating_mul(1_000_000)).max(1);
        control.deadline.store(deadline, Ordering::Relaxed);
    }

    fn stop(&self) -> Option<Stopped> {
        let control = &self.0;
        // The kill flag first, and unconditionally: it is one relaxed load, and
        // it is the answer that must arrive even on a connection with no
        // timeout configured.
        if control.interrupted.load(Ordering::Relaxed) {
            return Some(Stopped::Killed);
        }
        let deadline = control.deadline.load(Ordering::Relaxed);
        if deadline == 0 {
            return None;
        }
        // The engine has already amortised this down to one call per few
        // thousand rows, so a clock read here is fractions of a percent of the
        // work those rows cost.
        if control.base.elapsed().as_nanos() as u64 >= deadline {
            return Some(Stopped::Timeout);
        }
        None
    }
}

/// The engine's view of a [`Control`].
///
/// A separate type only because [`Cancel`] is defined in `inlaysql-core` and
/// `Arc` in `std`, so neither is local here and the impl needs a local type to
/// hang on. It owns a share of the control rather than borrowing it, because
/// the engine holds it for the life of the connection's handle.
pub struct Signal(Arc<Control>);

impl Signal {
    /// The engine-side half of `control`.
    pub fn new(control: Arc<Control>) -> Self {
        Self(control)
    }
}

/// Every live connection, by id.
///
/// Owned by the accept loop, which is the only thing that knows when a
/// connection begins and ends. A connection removes itself when its thread
/// exits, so a `KILL` against a stale id is `ER_NO_SUCH_THREAD` rather than a
/// write into freed state.
#[derive(Default)]
pub struct Registry {
    live: Mutex<HashMap<u32, Arc<Control>>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a connection.
    pub fn register(&self, control: &Arc<Control>) {
        if let Ok(mut live) = self.live.lock() {
            live.insert(control.id(), Arc::clone(control));
        }
    }

    /// Remove a connection, however its thread ended.
    pub fn forget(&self, id: u32) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(&id);
        }
    }

    /// How many connections are live — the honest source a future
    /// `SHOW PROCESSLIST` would read, and for now what proves registration and
    /// removal are symmetric.
    #[cfg(test)]
    pub fn live_count(&self) -> usize {
        self.live.lock().map(|live| live.len()).unwrap_or(0)
    }

    /// `KILL` connection `id`, asked for by `asker`.
    ///
    /// MySQL's rule, with this server's privilege model in place of
    /// `CONNECTION_ADMIN`: a connection may always kill itself and any other
    /// connection of the same account, and killing another account's
    /// connection needs the superuser. Anything else is
    /// `ER_KILL_DENIED_ERROR`, which says nothing about whether the id exists —
    /// but an id that does not exist is `ER_NO_SUCH_THREAD` either way, so
    /// there is no account enumeration to protect here beyond what
    /// `CONNECTION_ID()` already tells every client about itself.
    pub fn kill(&self, id: u32, scope: KillScope, asker: &Asker) -> Result<(), MysqlError> {
        let Ok(live) = self.live.lock() else {
            return Err(MysqlError::new(
                1094,
                "HY000",
                format!("Unknown thread id: {id}"),
            ));
        };
        let Some(control) = live.get(&id).map(Arc::clone) else {
            return Err(MysqlError::new(
                1094,
                "HY000",
                format!("Unknown thread id: {id}"),
            ));
        };
        // Dropped before the kill: `KILL CONNECTION` on *this* connection
        // shuts its own socket down, and holding the registry lock across that
        // would let one connection's teardown stall every other `KILL`.
        drop(live);

        let owner = control
            .user
            .lock()
            .map(|user| user.clone())
            .unwrap_or_default();
        let permitted = asker.superuser || id == asker.connection_id || owner == asker.user;
        if !permitted {
            return Err(MysqlError::new(
                1095,
                "HY000",
                format!("You are not owner of thread {id}"),
            ));
        }
        control.kill(scope);
        Ok(())
    }
}

/// Who is asking for a `KILL`.
pub struct Asker {
    /// The asking connection's own id, which it may always kill.
    pub connection_id: u32,
    /// The asking connection's account.
    pub user: String,
    /// Whether that account is a superuser.
    pub superuser: bool,
}

/// The socket half [`Control::attach_socket`] wants, or an error naming why
/// there is none.
///
/// A `try_clone` that fails leaves `KILL CONNECTION` able to stop a *running*
/// statement and unable to unblock an idle one, which is a degradation rather
/// than a failure — so the caller logs it and serves the connection anyway.
pub fn clone_socket(stream: &TcpStream) -> io::Result<TcpStream> {
    stream.try_clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker(id: u32, user: &str, superuser: bool) -> Asker {
        Asker {
            connection_id: id,
            user: user.to_string(),
            superuser,
        }
    }

    /// The default is no timeout, and no timeout means the deadline is never
    /// armed — which is what keeps a clock read off the per-statement path for
    /// every server that did not ask for one.
    #[test]
    fn a_zero_timeout_arms_no_deadline() {
        let control = Arc::new(Control::new(1, 0));
        let signal = Signal::new(Arc::clone(&control));
        signal.statement_began();
        assert_eq!(control.deadline.load(Ordering::Relaxed), 0);
        assert_eq!(signal.stop(), None);
    }

    /// A deadline that has passed stops the statement, and says which of the
    /// two reasons it was.
    #[test]
    fn a_passed_deadline_reports_a_timeout() {
        let control = Arc::new(Control::new(1, 1));
        let signal = Signal::new(Arc::clone(&control));
        signal.statement_began();
        assert_ne!(control.deadline.load(Ordering::Relaxed), 0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(signal.stop(), Some(Stopped::Timeout));
    }

    /// `KILL QUERY` applies to the statement that was running when it was
    /// issued and to no other, so the next statement starts clean. Without
    /// this a killed connection would refuse every statement it was ever sent
    /// again.
    #[test]
    fn a_killed_query_does_not_carry_into_the_next_statement() {
        let control = Arc::new(Control::new(1, 0));
        let signal = Signal::new(Arc::clone(&control));
        signal.statement_began();
        control.kill(KillScope::Query);
        assert_eq!(signal.stop(), Some(Stopped::Killed));
        signal.statement_began();
        assert_eq!(signal.stop(), None);
        assert!(!control.is_closing());
    }

    /// `KILL CONNECTION` does outlive the statement: the command loop has to
    /// still see it after the statement it interrupted has been answered.
    #[test]
    fn a_killed_connection_stays_killed() {
        let control = Arc::new(Control::new(1, 0));
        let signal = Signal::new(Arc::clone(&control));
        signal.statement_began();
        control.kill(KillScope::Connection);
        assert_eq!(signal.stop(), Some(Stopped::Killed));
        signal.statement_began();
        assert_eq!(signal.stop(), Some(Stopped::Killed));
        assert!(control.is_closing());
    }

    #[test]
    fn an_unknown_thread_id_is_1094() {
        let registry = Registry::new();
        let error = registry
            .kill(9, KillScope::Query, &asker(1, "root", true))
            .unwrap_err();
        assert_eq!(error.code, 1094);
    }

    /// Own connection, own account, superuser: yes. Another account's, without
    /// the superuser: `ER_KILL_DENIED_ERROR`.
    #[test]
    fn only_the_owner_or_a_superuser_may_kill() {
        let registry = Registry::new();
        let mine = Arc::new(Control::new(1, 0));
        mine.set_user("alice");
        let theirs = Arc::new(Control::new(2, 0));
        theirs.set_user("bob");
        registry.register(&mine);
        registry.register(&theirs);
        assert_eq!(registry.live_count(), 2);

        // Alice kills her own connection, and another of her own.
        let also_mine = Arc::new(Control::new(3, 0));
        also_mine.set_user("alice");
        registry.register(&also_mine);
        assert!(registry
            .kill(1, KillScope::Query, &asker(1, "alice", false))
            .is_ok());
        assert!(registry
            .kill(3, KillScope::Query, &asker(1, "alice", false))
            .is_ok());

        // Alice may not touch bob's.
        let error = registry
            .kill(2, KillScope::Query, &asker(1, "alice", false))
            .unwrap_err();
        assert_eq!(error.code, 1095);

        // A superuser may.
        assert!(registry
            .kill(2, KillScope::Query, &asker(1, "root", true))
            .is_ok());

        registry.forget(2);
        assert_eq!(registry.live_count(), 2);
    }

    /// A session `SET` changes what the next statement gets, and the number
    /// reported is read straight off the control — so there is no second copy
    /// that could disagree with what is enforced.
    #[test]
    fn a_session_timeout_change_takes_effect_on_the_next_statement() {
        let control = Arc::new(Control::new(1, 0));
        let signal = Signal::new(Arc::clone(&control));
        signal.statement_began();
        assert_eq!(control.timeout_ms(), 0);
        control.set_timeout_ms(1);
        // The statement in flight keeps the deadline it began with.
        assert_eq!(control.deadline.load(Ordering::Relaxed), 0);
        assert_eq!(control.timeout_ms(), 1);
        signal.statement_began();
        assert_ne!(control.deadline.load(Ordering::Relaxed), 0);
    }
}
