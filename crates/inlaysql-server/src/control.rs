//! Stopping a statement, and watching one: the per-connection cancellation
//! signal, and the registry `KILL` and `SHOW PROCESSLIST` both reach it through.
//!
//! Everything else in this server is per connection and shares nothing but the
//! database file. This module is the one exception, and it exists because the
//! three things a shared SQL server needs — a statement timeout, a way to end
//! somebody else's runaway query, and a way to *see* it before deciding to —
//! cannot be built anywhere else:
//!
//! * The engine core is `no_std`. It cannot read a clock and it cannot be
//!   interrupted by a thread, so it can only *ask* whether to carry on
//!   ([`inlaysql::Cancel`]). [`Control`] is this server's answer.
//! * `KILL` is one connection acting on another, so the flag has to be
//!   reachable from a thread that does not own the target's handle. That is
//!   the whole of [`Registry`].
//! * `SHOW PROCESSLIST` is the same reach, read-only: it asks every live
//!   connection what it is doing, and the answer has to come from state the
//!   asking thread can touch. It reads the *same* registry `KILL` writes to,
//!   rather than a second list beside it — two lists is how a `KILL` ends up
//!   naming an id the process list never showed.
//!
//! # What a killed statement leaves behind
//!
//! Nothing. The engine only notices cancellation while a statement is
//! producing or collecting rows, never while it is making them durable, so a
//! cancelled write leaves through the same statement-atomicity path a `CHECK`
//! violation leaves through: the buffered rows are discarded and the handle is
//! reloaded. `wire.rs` pins that with a cancelled `UPDATE` — the table is as
//! it was, and the connection's next statement works.
//!
//! # What the process list costs the connection being watched
//!
//! Two clock reads and two relaxed stores per command — one pair when it
//! starts, one when it finishes. That is the whole per-statement price of the
//! `Command` and `Time` columns, and it is paid unconditionally because a
//! process list without "how long has this been running" is the one column an
//! operator actually opens it for. The statement *text* is the exception: it is
//! an allocation and a mutex, so it is only recorded when
//! [`crate::ServerOptions::statement_text`] asks for it, which is also the
//! policy switch for whether user data may be held at all.

use std::collections::HashMap;
use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use inlaysql::{Cancel, Stopped};

use crate::errors::MysqlError;

/// What a connection is doing, in the words MySQL's `Command` column uses.
///
/// A `u8` on the wire between threads because it is written on the per-command
/// path and read by whoever is running `SHOW PROCESSLIST`; anything richer
/// would be a lock where a relaxed store will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Doing {
    /// Handshaking. The connection exists and has not authenticated yet, so it
    /// has no owner and only a superuser can see it.
    Connect = 0,
    /// Waiting for the client to send something. MySQL's word for idle.
    Sleep = 1,
    /// Running a `COM_QUERY`.
    Query = 2,
    /// Running a `COM_STMT_EXECUTE`.
    Execute = 3,
    /// Planning a `COM_STMT_PREPARE`.
    Prepare = 4,
    /// Switching default schema (`COM_INIT_DB`).
    InitDb = 5,
    /// Answering a `COM_PING`.
    Ping = 6,
    /// Discarding a prepared statement (`COM_STMT_CLOSE`).
    CloseStmt = 7,
    /// `COM_STMT_RESET`.
    ResetStmt = 8,
    /// `COM_PROCESS_KILL`, the older spelling of the `KILL` statement.
    Kill = 9,
    /// `COM_FIELD_LIST`, which this server refuses.
    FieldList = 10,
    /// A command byte this server does not implement.
    Other = 11,
}

impl Doing {
    /// MySQL's own spelling, which is what a client prints verbatim.
    ///
    /// Every string here is a value real MySQL's `Command` column can take, so
    /// a tool that recognises them is not surprised by one it has never seen.
    /// `Daemon` is MySQL's own catch-all for a thread running something that is
    /// not a client command, which is the nearest true thing to say about a
    /// command byte this server does not implement.
    pub fn name(self) -> &'static str {
        match self {
            Doing::Connect => "Connect",
            Doing::Sleep => "Sleep",
            Doing::Query => "Query",
            Doing::Execute => "Execute",
            Doing::Prepare => "Prepare",
            Doing::InitDb => "Init DB",
            Doing::Ping => "Ping",
            Doing::CloseStmt => "Close stmt",
            Doing::ResetStmt => "Reset stmt",
            Doing::Kill => "Kill",
            Doing::FieldList => "Field List",
            Doing::Other => "Daemon",
        }
    }

    /// Whether this connection is executing something rather than waiting.
    /// `Threads_running` is the count of these.
    pub fn is_running(self) -> bool {
        !matches!(self, Doing::Connect | Doing::Sleep)
    }

    fn from_u8(byte: u8) -> Self {
        match byte {
            0 => Doing::Connect,
            1 => Doing::Sleep,
            2 => Doing::Query,
            3 => Doing::Execute,
            4 => Doing::Prepare,
            5 => Doing::InitDb,
            6 => Doing::Ping,
            7 => Doing::CloseStmt,
            8 => Doing::ResetStmt,
            9 => Doing::Kill,
            10 => Doing::FieldList,
            // Unreachable while the only writer is `command_began`, which
            // always stores a discriminant from this enum. Mapped rather than
            // panicked because a process list is a diagnostic, and a diagnostic
            // that can take the server down is worse than one that says
            // "Daemon".
            _ => Doing::Other,
        }
    }
}

/// One live connection, as an operator sees it.
///
/// A snapshot, not a handle. Every field is copied out while the registry lock
/// is held — it has to be, or a connection could end between reading its id and
/// reading what it is doing — but the lock is released the moment the copying
/// is done and before any of it becomes a result set. What a concurrent `KILL`
/// can therefore be made to wait for is bounded by `max_connections` string
/// clones, not by writing rows to a socket.
#[derive(Debug, Clone)]
pub struct Process {
    /// The connection id — the same number `CONNECTION_ID()` reports and
    /// `KILL` takes.
    pub id: u32,
    /// The account it authenticated as, or `None` while it is still
    /// handshaking.
    pub user: Option<String>,
    /// The peer address, `host:port`, as MySQL's `Host` column spells it.
    pub host: String,
    /// The default schema it last selected.
    pub db: Option<String>,
    /// What it is doing now.
    pub command: Doing,
    /// How many seconds it has been doing it.
    pub time_secs: u64,
    /// The statement in flight, when this server was started with
    /// [`crate::ServerOptions::statement_text`]. `None` otherwise — including
    /// when the connection is idle, because then there is no statement in
    /// flight to name.
    pub info: Option<String>,
}

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
    /// The peer address this connection came from, `host:port`. Fixed for the
    /// life of the connection, so it needs no synchronisation.
    host: String,
    /// The default schema this connection last selected, mirrored off the
    /// session. Mirrored rather than read from the session because the session
    /// belongs to the connection's own thread and `SHOW PROCESSLIST` runs on
    /// somebody else's — the same reason `user` is here.
    database: Mutex<Option<String>>,
    /// What this connection is doing, as a [`Doing`] discriminant.
    doing: AtomicU8,
    /// Nanoseconds since `base` at which `doing` last changed. The `Time`
    /// column is the difference between this and now.
    doing_since: AtomicU64,
    /// The statement in flight, when the server was started with
    /// [`crate::ServerOptions::statement_text`]; `None` otherwise, and `None`
    /// between statements either way.
    ///
    /// **Behind `record_info` because holding it is a decision about user
    /// data, not a performance tuning knob.** Statement text carries whatever
    /// the client put in it — an email address, a token, a name — and this
    /// server's default is to hold none of it anywhere, including here. When
    /// the flag is off nothing ever locks this mutex.
    info: Mutex<Option<String>>,
    /// Whether `info` is maintained at all. See it.
    record_info: bool,
}

impl Control {
    /// A control for connection `id` from `host`, with `timeout_ms` as its
    /// statement timeout (`0` for none) and `record_info` deciding whether the
    /// statement in flight is recorded for `SHOW PROCESSLIST`.
    pub fn new(id: u32, host: String, timeout_ms: u64, record_info: bool) -> Self {
        Self {
            id,
            user: Mutex::new(String::new()),
            interrupted: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            timeout_ms: AtomicU64::new(timeout_ms),
            deadline: AtomicU64::new(0),
            base: Instant::now(),
            socket: Mutex::new(None),
            host,
            database: Mutex::new(None),
            // A connection exists before it authenticates, and the process list
            // says so rather than showing it as an idle session of nobody's.
            doing: AtomicU8::new(Doing::Connect as u8),
            doing_since: AtomicU64::new(0),
            info: Mutex::new(None),
            record_info,
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
        Arc::new(Self::new(id, "unix".to_string(), 0, false))
    }

    /// [`Control::detached`] with a statement timeout.
    #[cfg(test)]
    pub fn detached_with_timeout(id: u32, timeout_ms: u64) -> Arc<Self> {
        Arc::new(Self::new(id, "unix".to_string(), timeout_ms, false))
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

    /// Mirror the session's default schema, so the process list's `db` column
    /// is the schema this connection would actually use.
    pub fn set_database(&self, database: Option<&str>) {
        if let Ok(mut held) = self.database.lock() {
            *held = database.map(str::to_string);
        }
    }

    /// Note that this connection is now waiting for its client, and has been
    /// since this instant.
    ///
    /// Used once, when the handshake completes: until then the connection is
    /// [`Doing::Connect`], and a connection that had logged in and sent no
    /// statement yet would otherwise sit in the process list under the state
    /// of one still handshaking.
    pub fn now_idle(&self) {
        self.command_began(Doing::Sleep);
    }

    /// Note that a command has begun, and return the reading of this
    /// connection's clock it began at.
    ///
    /// One clock read and one relaxed store, per command. The reading is
    /// handed back rather than kept somewhere a second reader could see a
    /// half-updated pair: the caller passes it to [`Control::command_ended`],
    /// which is the only thing that needs the difference.
    pub fn command_began(&self, doing: Doing) -> u64 {
        let now = self.base.elapsed().as_nanos() as u64;
        self.doing_since.store(now, Ordering::Relaxed);
        self.doing.store(doing as u8, Ordering::Relaxed);
        now
    }

    /// Note that the command that began at `began` has finished, and return
    /// how many nanoseconds it took.
    ///
    /// Also drops the recorded statement text, if any: an idle connection has
    /// no statement in flight, and leaving the last one behind would make the
    /// process list report a statement that had already answered.
    pub fn command_ended(&self, began: u64) -> u64 {
        let now = self.base.elapsed().as_nanos() as u64;
        self.doing_since.store(now, Ordering::Relaxed);
        self.doing.store(Doing::Sleep as u8, Ordering::Relaxed);
        if self.record_info {
            if let Ok(mut held) = self.info.lock() {
                *held = None;
            }
        }
        now.saturating_sub(began)
    }

    /// Record the statement now in flight, when this server records statement
    /// text at all. A no-op — not even a lock — when it does not.
    pub fn set_info(&self, sql: &str) {
        if !self.record_info {
            return;
        }
        if let Ok(mut held) = self.info.lock() {
            *held = Some(sql.to_string());
        }
    }

    /// The statement in flight, for the slow-query log to name. `None` when
    /// statement text is not recorded, which is the default.
    pub fn info(&self) -> Option<String> {
        if !self.record_info {
            return None;
        }
        self.info.lock().ok().and_then(|held| held.clone())
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

    /// The account this connection belongs to, empty until it has
    /// authenticated. Read by `KILL` and by `SHOW PROCESSLIST` to decide
    /// whether the asker owns it.
    fn owner(&self) -> String {
        self.user
            .lock()
            .map(|user| user.clone())
            .unwrap_or_default()
    }

    /// Everything the process list reports about this connection, copied out.
    fn process(&self) -> Process {
        let doing = Doing::from_u8(self.doing.load(Ordering::Relaxed));
        let since = self.doing_since.load(Ordering::Relaxed);
        let now = self.base.elapsed().as_nanos() as u64;
        let owner = self.owner();
        Process {
            id: self.id,
            user: if owner.is_empty() { None } else { Some(owner) },
            host: self.host.clone(),
            db: self.database.lock().ok().and_then(|held| held.clone()),
            command: doing,
            // Saturating because `since` and `now` are two separate reads of a
            // moving clock: a command that began between them would otherwise
            // wrap to eighteen billion seconds of uptime.
            time_secs: now.saturating_sub(since) / 1_000_000_000,
            // Only while something is running. A `Sleep` row with a statement
            // on it would read as a query that had been going for an hour.
            info: if doing.is_running() {
                self.info()
            } else {
                None
            },
        }
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

    /// How many connections are live. This is `Threads_connected`, and it is
    /// counted here rather than kept alongside because a second counter is a
    /// second thing to keep in step: a connection is in this map for exactly as
    /// long as its thread exists, panic included (see `crate::Registered`).
    pub fn live_count(&self) -> usize {
        self.live.lock().map(|live| live.len()).unwrap_or(0)
    }

    /// How many connections are executing something rather than waiting for
    /// their client. This is `Threads_running`, and it is derived from the
    /// same per-connection state `SHOW PROCESSLIST`'s `Command` column shows —
    /// so the number and the list cannot disagree, and neither costs the
    /// connection being counted anything beyond the store it already makes.
    pub fn running_count(&self) -> usize {
        let Ok(live) = self.live.lock() else {
            return 0;
        };
        live.values()
            .filter(|control| Doing::from_u8(control.doing.load(Ordering::Relaxed)).is_running())
            .count()
    }

    /// Every live connection `viewer` is allowed to see, newest id last.
    ///
    /// **The privilege rule is `KILL`'s, exactly**: your own connection always,
    /// any other connection of your own account, and everybody else's only
    /// with the superuser. Written that way on purpose — a list that showed
    /// more than `KILL` would act on invites an operator to try an id they
    /// will be refused, and one that showed less would hide a connection they
    /// could already end.
    ///
    /// A connection that has not authenticated yet has no account, so it
    /// belongs to nobody and only a superuser sees it. That is the strict
    /// reading: the alternative — showing it to everyone — would let any
    /// account watch logins arrive.
    pub fn snapshot(&self, viewer: &Asker) -> Vec<Process> {
        let mut out: Vec<Process> = {
            let Ok(live) = self.live.lock() else {
                return Vec::new();
            };
            // Filtered inside the lock so nothing is copied that will not be
            // shown, and formatted outside it: see [`Process`].
            live.values()
                .filter(|control| {
                    viewer.superuser
                        || control.id == viewer.connection_id
                        || (!viewer.user.is_empty() && control.owner() == viewer.user)
                })
                .map(|control| control.process())
                .collect()
        };
        // A stable order, because a result set with none is a result set that
        // looks different every time it is run for no reason. Id ascending is
        // connection age ascending, which is the order MySQL happens to give.
        out.sort_by_key(|process| process.id);
        out
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

        let owner = control.owner();
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

/// Who is asking — for a `KILL`, or for the process list.
///
/// One type for both because they answer the same question about the same
/// three facts, and the privilege rule they apply is deliberately identical:
/// you may see exactly the connections you may end. It is built fresh from the
/// account store on every statement that needs it (see
/// `Connection::asker`), never cached at login, so a revoked superuser stops
/// seeing other people's connections on its very next statement.
pub struct Asker {
    /// The asking connection's own id, which it may always kill and always
    /// see.
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

    /// A control with a plausible peer address and no statement-text
    /// recording — this server's default posture.
    fn control(id: u32, timeout_ms: u64) -> Arc<Control> {
        Arc::new(Control::new(
            id,
            format!("127.0.0.1:5{id:04}"),
            timeout_ms,
            false,
        ))
    }

    /// The default is no timeout, and no timeout means the deadline is never
    /// armed — which is what keeps a clock read off the per-statement path for
    /// every server that did not ask for one.
    #[test]
    fn a_zero_timeout_arms_no_deadline() {
        let control = control(1, 0);
        let signal = Signal::new(Arc::clone(&control));
        signal.statement_began();
        assert_eq!(control.deadline.load(Ordering::Relaxed), 0);
        assert_eq!(signal.stop(), None);
    }

    /// A deadline that has passed stops the statement, and says which of the
    /// two reasons it was.
    #[test]
    fn a_passed_deadline_reports_a_timeout() {
        let control = control(1, 1);
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
        let control = control(1, 0);
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
        let control = control(1, 0);
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
        let mine = control(1, 0);
        mine.set_user("alice");
        let theirs = control(2, 0);
        theirs.set_user("bob");
        registry.register(&mine);
        registry.register(&theirs);
        assert_eq!(registry.live_count(), 2);

        // Alice kills her own connection, and another of her own.
        let also_mine = control(3, 0);
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
        let control = control(1, 0);
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

    // ------------------------------------------------- the process list

    /// The rule that makes the process list safe to expose at all: an account
    /// sees its own connections and nobody else's, and a superuser sees the
    /// lot. Deliberately the same rule `only_the_owner_or_a_superuser_may_kill`
    /// pins for `KILL`, because a list that disagreed with what `KILL` will act
    /// on is a list that lies about the server.
    #[test]
    fn a_process_list_shows_only_what_the_viewer_could_kill() {
        let registry = Registry::new();
        let alice = control(1, 0);
        alice.set_user("alice");
        let bob = control(2, 0);
        bob.set_user("bob");
        let alice_again = control(3, 0);
        alice_again.set_user("alice");
        // Still handshaking: no account, so it belongs to nobody.
        let anonymous = control(4, 0);
        for control in [&alice, &bob, &alice_again, &anonymous] {
            registry.register(control);
        }

        let seen = |viewer: &Asker| -> Vec<u32> {
            registry
                .snapshot(viewer)
                .into_iter()
                .map(|process| process.id)
                .collect()
        };

        assert_eq!(seen(&asker(1, "alice", false)), vec![1, 3]);
        assert_eq!(seen(&asker(2, "bob", false)), vec![2]);
        assert_eq!(seen(&asker(9, "root", true)), vec![1, 2, 3, 4]);

        // And every id a non-superuser was shown is one it may `KILL`, which
        // is the property the two rules being one rule is worth having.
        for id in seen(&asker(1, "alice", false)) {
            assert!(
                registry
                    .kill(id, KillScope::Query, &asker(1, "alice", false))
                    .is_ok(),
                "alice was shown connection {id} but may not kill it"
            );
        }
    }

    /// A connection that has not authenticated is nobody's, and an account
    /// name that is empty must not match it — otherwise every session before
    /// its handshake completed would be visible to every other one.
    #[test]
    fn an_unauthenticated_connection_belongs_to_nobody() {
        let registry = Registry::new();
        let anonymous = control(1, 0);
        registry.register(&anonymous);

        let list = registry.snapshot(&asker(2, "", false));
        assert!(
            list.is_empty(),
            "an empty account name matched an unauthenticated connection: {list:?}"
        );
        let list = registry.snapshot(&asker(9, "root", true));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].user, None);
        assert_eq!(list[0].command, Doing::Connect);
    }

    /// `Command`, `Time` and `Threads_running` all come off the same two
    /// stores, so the list and the counter cannot drift apart.
    #[test]
    fn a_running_command_is_visible_and_counted_until_it_ends() {
        let registry = Registry::new();
        let control = control(1, 0);
        control.set_user("alice");
        control.set_database(Some("app"));
        registry.register(&control);

        // Idle: one connection, none running.
        assert_eq!(registry.live_count(), 1);
        assert_eq!(registry.running_count(), 0);

        let began = control.command_began(Doing::Query);
        let list = registry.snapshot(&asker(1, "alice", false));
        assert_eq!(list[0].command, Doing::Query);
        assert_eq!(list[0].db.as_deref(), Some("app"));
        assert!(list[0].host.starts_with("127.0.0.1:"));
        assert_eq!(registry.running_count(), 1);

        let elapsed = control.command_ended(began);
        assert!(elapsed > 0, "a command that ran took no time at all");
        let list = registry.snapshot(&asker(1, "alice", false));
        assert_eq!(list[0].command, Doing::Sleep);
        assert_eq!(registry.running_count(), 0);
    }

    /// The default is that no statement text is held anywhere, so `Info` is
    /// `NULL` even while a statement is running. Turning the flag on is what
    /// changes that, and nothing else does.
    #[test]
    fn statement_text_is_recorded_only_when_it_was_asked_for() {
        for record in [false, true] {
            let control = Arc::new(Control::new(1, "127.0.0.1:1".into(), 0, record));
            let began = control.command_began(Doing::Query);
            control.set_info("SELECT secret FROM vault");
            let registry = Registry::new();
            control.set_user("alice");
            registry.register(&control);

            let list = registry.snapshot(&asker(1, "alice", false));
            assert_eq!(
                list[0].info.is_some(),
                record,
                "statement text with record_info = {record}"
            );

            // And it never outlives the statement, whichever way the flag is
            // set: a sleeping connection has nothing in flight to name.
            control.command_ended(began);
            let list = registry.snapshot(&asker(1, "alice", false));
            assert_eq!(list[0].info, None);
        }
    }
}
