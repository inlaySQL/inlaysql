//! A MySQL wire-protocol server over an InlaySQL database file.
//!
//! ```sh
//! inlaysql serve --mysql app.inlay --user root --password-env INLAYSQL_PASSWORD
//! ```
//!
//! Point any MySQL client at it — `mysql`, PDO, JDBC, `mysql2` — and it talks
//! to one embedded database file. `docs/server.md` has the full picture,
//! including a plain account of which SQL does not work yet.
//!
//! # Where this sits
//!
//! Two decisions from `docs/architecture.md` shape everything here.
//!
//! **D1 — MySQL compatibility is a shim, not a dialect change.** `inlaysql-core`
//! speaks SQLite's dialect and keeps speaking it. The MySQL-shaped statements a
//! driver sends — `SET NAMES`, `SHOW TABLES`, `information_schema` queries — are
//! recognised in [`shim`](crate) and answered from the catalog, and the
//! MySQL-only DDL decoration an ORM's migrations carry — `AUTO_INCREMENT`,
//! `ENGINE=`, `DEFAULT CHARSET=` — is translated out of a statement before it
//! reaches the engine. Nothing in this crate adds syntax to the engine, and
//! nothing it cannot honour is accepted and ignored: a clause is either removed
//! and reported as a warning, or refused with a MySQL error code that names it.
//!
//! **D2 — Thread-per-connection, one handle each.** `std::net::TcpListener`, one
//! OS thread per connection, and each thread opens its own
//! [`Database`](inlaysql::Database) on the same file. The engine is `!Send` by
//! design, and several handles on one file already commit concurrently with
//! first-committer-wins, so this needs no locking of its own. What the handles
//! *share* is the file device's per-file raw-page read cache
//! (`FileDevice`'s `CommitCoordinator`), so a page is read from the device
//! once per file rather than once per connection — see `docs/server.md`, D2.
//! There is no async runtime anywhere in this crate, which is deliberate: the
//! workspace has zero async dependencies and this is not the crate that
//! changes that.
//!
//! # Security
//!
//! Read this before exposing it to anything.
//!
//! * **The listener binds `127.0.0.1` unless told otherwise.** A database that
//!   defaults to every interface is a liability, so reaching the network is an
//!   explicit act.
//! * **v1 is plaintext. There is no TLS.** `CLIENT_SSL` is never advertised, so
//!   a client cannot negotiate encryption and then be quietly downgraded — it is
//!   told. Statements, results and the whole session cross the wire in the
//!   clear. Do not run this across a network you do not trust.
//! * **Accounts and privileges live in the database file** (the `acl` module):
//!   `CREATE USER`, `GRANT`/`REVOKE`, `SELECT`/`INSERT`/`UPDATE`/`DELETE`/
//!   `CREATE`/`DROP`/`ALTER` globally or per table, and a superuser. Every
//!   statement is authorised at one choke point, from its *plan* rather than
//!   its text, and a statement whose requirement cannot be determined is
//!   refused. `--user`/`--password` are the whole account model until the
//!   first `CREATE USER` and are ignored from then on.
//! * **A password is never stored, and never logged.** What an account carries
//!   is the verifier each plugin's challenge-response is defined in terms of;
//!   see the `auth` module. A rejected login says only "access denied", without
//!   hinting which half was wrong.
//! * The `mysql_native_password` exchange is challenge-response, so the password
//!   itself is not sent even though the channel is unencrypted. That protects
//!   the password, not the data.
//! * **These privileges guard this server and nothing else.** Anything that can
//!   open the file — the embedded API, the CLI — bypasses all of them, because
//!   the file *is* the credential there.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod acl;
mod auth;
mod connection;
mod control;
mod errors;
mod infoschema;
mod metrics;
mod mysqlddl;
mod mysqlfunc;
mod packet;
mod protocol;
mod session;
mod shim;
mod sqltext;

use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use control::{Control, Registry};
use metrics::{Counter, Metrics};

use inlaysql::{Database, EngineOptions, FileDevice};

pub use errors::MysqlError;
pub use session::SERVER_VERSION;

/// The port MySQL uses, and the default here.
pub const DEFAULT_PORT: u16 = 3306;

/// The address the listener binds unless told otherwise.
///
/// Loopback, on purpose. Binding anywhere else has to be asked for.
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// How many connections are served at once before new ones are refused.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// How long a connection may go without the server hearing anything from it
/// before its socket read times out and the connection is closed.
///
/// MySQL's own `wait_timeout` default, and what this server has always
/// *reported*. It is now also what it enforces; see
/// [`ServerOptions::wait_timeout_secs`] for why the two have to be the same
/// number.
pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 28800;

/// How long one write to a client that has stopped reading may block before
/// the connection is dropped, reported as `net_write_timeout`.
///
/// `SO_SNDTIMEO` bounds a single blocked `write`, not a whole transfer, so a
/// slow but progressing client is unaffected; only one that has stopped
/// draining its socket entirely hits this. Not configurable: MySQL's own
/// default is the same 60 seconds, and a knob nobody turns is surface for
/// nothing.
pub const NET_WRITE_TIMEOUT_SECS: u64 = 60;

/// How long one statement may run before the server stops it, in
/// milliseconds. `0` is no limit, and is the default.
///
/// MySQL's own default for `max_execution_time`, and off for the same reason:
/// a database cannot know what a legitimate statement costs on somebody else's
/// data, and a timeout that fires on a nightly report is worse than none. It
/// is a limit an operator sets knowing their workload — `--max-execution-time`
/// — or a session sets for itself with `SET max_execution_time`.
pub const DEFAULT_MAX_EXECUTION_TIME_MS: u64 = 0;

/// How a [`Server`] should be set up.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// The address to bind. Defaults to loopback.
    pub bind: String,
    /// The port to bind. Zero asks the operating system for a free one, which
    /// is what the tests use.
    pub port: u16,
    /// The bootstrap account name.
    ///
    /// This and [`ServerOptions::password`] are the *whole* account model on a
    /// database that has never had an account created in it — one name, one
    /// password, every privilege, exactly as before accounts existed. On a
    /// database that has accounts they are **not consulted at all**, unless
    /// [`ServerOptions::reset_superuser`] says to; the file is the authority
    /// once it has credentials of its own. [`Server::notices`] reports which
    /// of the two happened.
    pub user: String,
    /// The bootstrap account's password. Empty means an empty password, which
    /// is only ever appropriate on loopback — and [`Server::notices`] says so
    /// when it is the credential in use.
    ///
    /// Hashed into verifiers at [`Server::bind`] and dropped: no part of this
    /// process holds a plaintext password after that.
    pub password: String,
    /// The most connections served at once.
    pub max_connections: usize,
    /// How many seconds a connection may be silent before the server closes
    /// it, enforced as the socket read timeout and reported as `wait_timeout`.
    ///
    /// Must be greater than zero: [`Server::bind`] refuses `0` rather than
    /// inventing a spelling for "never", because every number this server
    /// reports has to be one it applies, and "never" has no honest
    /// representation in a variable whose MySQL range starts at 1. A caller
    /// that genuinely wants no idle timeout asks for a very large one
    /// (31536000 is MySQL's own maximum, a year).
    ///
    /// This is what stops [`ServerOptions::max_connections`] silent sockets
    /// holding every slot for ever.
    pub wait_timeout_secs: u64,
    /// Let each connection's handle reclaim pages a commit stopped using,
    /// instead of only ever growing the file
    /// ([`inlaysql::EngineOptions::page_reuse`]).
    ///
    /// **Off by default, and enabling it is a decision about the whole file,
    /// not about this server.** Without it the file's high-water mark only
    /// grows, even under steady-state churn where the live data size is flat,
    /// and the only way to reclaim that space is to stop the server and run
    /// `inlaysql vacuum` — which needs the exclusive lock this process holds
    /// for as long as it serves.
    ///
    /// # What turning it on costs
    ///
    /// **Nothing may open this file read-only while the server runs with it
    /// on.** `Database::open_read_only` takes no OS lock, by design, so a
    /// reclaimed page — physically overwritten with new content — could be one
    /// a lock-free reader is still looking at, in this process or any other.
    /// Reclamation can only prove liveness for readers this process's
    /// reservation gate can see, and a read-only handle is invisible to it.
    /// Concretely, that rules out `inlaysql serve --mcp` on the same file (it
    /// opens read-only by default, which is the whole workflow `docs/mcp.md`
    /// describes) and any other process reading the file live. This is the
    /// reason the engine defaults it off; see
    /// [`inlaysql::EngineOptions::page_reuse`] and `docs/recovery.md` for the
    /// full argument.
    ///
    /// It also turns off the shared raw-page read cache for the file, one-way
    /// and for every handle: that cache is keyed by page id and is sound only
    /// while a page id is never reissued.
    pub page_reuse: bool,
    /// Most bytes one statement may hold in a blocking operator
    /// ([`inlaysql::EngineOptions::query_memory_bytes`]).
    ///
    /// `ORDER BY`, `GROUP BY`, `DISTINCT` and window functions have to hold
    /// their whole input before they can answer, so one statement can ask for
    /// more memory than the machine has. Unbounded, what ends it is the
    /// out-of-memory killer, and that ends the *process* — which here means
    /// every other connection too. Past this, the one statement responsible is
    /// refused with `ER_OUT_OF_SORTMEMORY` and everything else keeps running.
    ///
    /// **Per statement, not per server.** [`ServerOptions::max_connections`]
    /// clients can each be holding this much at once, so the number to divide
    /// by the machine's memory is this one times the connection cap. The
    /// default is [`inlaysql::EngineOptions::default`]'s; `0` removes the
    /// ceiling.
    pub query_memory_bytes: usize,
    /// Keep each connection's vector indexes in the database file instead of
    /// in its own memory ([`inlaysql::EngineOptions::paged_vector_indexes`],
    /// which is what [`inlaysql::Database::open_paged`] sets).
    ///
    /// **Off by default, and it is a trade, not a free win.** The default
    /// in-memory index holds every embedding *twice* — the source map and the
    /// committed graph node each keep a copy — plus the graph's adjacency, and
    /// it holds all of it once per connection, because [`Server::run`] gives
    /// every connection its own handle. Measured at dimension 384 by
    /// `crates/inlaysql/tests/index_memory_cost.rs`: ~3.5 KB resident per
    /// vector, against the 1.5 KB the payload alone would suggest. This
    /// replaces that with a bounded node cache (~6 MiB at dimension 384) and
    /// puts the graph in the file.
    ///
    /// # What turning it on costs
    ///
    /// * **A search that misses the cache is a read from the file** rather
    ///   than a pointer chase. Recall is unchanged — the paged graph is the
    ///   same graph, from the same algorithm over the same insert sequence —
    ///   but latency is not.
    /// * **Another connection's commit costs this one a re-open of the
    ///   graph**, which walks its node records to rebuild the row-id map:
    ///   O(nodes) per foreign commit, where an in-memory index pays O(rows
    ///   that commit touched). See
    ///   `inlaysql_core`'s `Engine::adopt_self_persisting_vector_indexes`.
    /// * **It does nothing for the full-text index.** `Bm25Index` has no paged
    ///   backend at all, so the term dictionary, every postings list, the
    ///   per-document term lists and the row-id map stay resident, once per
    ///   connection — on a hybrid corpus, the larger half of the bill. See
    ///   `docs/enterprise-readiness.md`, blocker 6.
    ///
    /// Asked for rather than inherited, for the same reason
    /// [`ServerOptions::page_reuse`] is: which way the trade falls depends on
    /// the corpus and on what the operator is short of.
    pub paged_vector_indexes: bool,
    /// How long one statement may run before the server stops it, in
    /// milliseconds. `0` (the default) is no limit.
    ///
    /// Until this existed a statement that ran long could not be stopped by
    /// anyone — not the client, not the operator, not a timeout — so one
    /// `SELECT` over a cross join held a connection slot until the process was
    /// restarted. This is the ceiling that ends it, and `KILL` is the manual
    /// form of the same mechanism.
    ///
    /// # Three things to know before setting it
    ///
    /// * **It applies to every statement, not only to `SELECT`.** MySQL's
    ///   `max_execution_time` is read-only-`SELECT` only, because interrupting
    ///   a write there is expensive. Here a statement is atomic — a cancelled
    ///   `UPDATE` discards its buffered rows and the handle stays usable, the
    ///   same path a `CHECK` violation on the last row of a multi-row `INSERT`
    ///   already took — so the more useful reading is available and this takes
    ///   it. `docs/server.md` names the difference.
    /// * **It is per statement, not per transaction.** Ten statements inside
    ///   one `BEGIN` each get the full budget.
    /// * **It is a ceiling, not a guarantee of promptness.** The engine asks
    ///   whether to stop once per few thousand rows, so a statement overruns by
    ///   at most that much work — and a single retrieval-index walk
    ///   (`bm25_score`, `vector_score`) with no `WHERE` pushed into it is not
    ///   interruptible at all, because the index trait a third-party backend
    ///   implements takes no cancellation signal. See `docs/server.md`.
    ///
    /// A session may change its own with `SET max_execution_time = <ms>`, and
    /// `@@max_execution_time` reports the number actually in force — read off
    /// the same field the engine reads, so the two cannot disagree.
    pub max_execution_time_ms: u64,
    /// Set [`ServerOptions::user`]'s password from these options and make it a
    /// superuser, on a database that **already has** an account store.
    ///
    /// The recovery path, and the only one there is. Without it,
    /// `--user`/`--password` are consulted exactly once — when the store is
    /// created — because a flag that silently overwrote a stored password
    /// would turn a forgotten line in a service file into a way back into a
    /// database whose password had been rotated. With it, an operator who has
    /// lost the last superuser's password can get back in.
    ///
    /// It is not a privilege escalation: it needs write access to the database
    /// file, and anything with that can already read every row in it.
    pub reset_superuser: bool,
    /// Log a line to stderr for every statement that runs longer than this
    /// many milliseconds. `0` (the default) is off.
    ///
    /// Reported as `long_query_time` (in seconds, MySQL's unit) and
    /// `slow_query_log`, and read off this field so the reported threshold is
    /// the applied one. The line names the connection, the account, the
    /// elapsed time and the kind of statement — and the statement *text* only
    /// if [`ServerOptions::statement_text`] is also on, because text is user
    /// data and this server does not hold it by default.
    pub slow_query_log_ms: u64,
    /// Record the statement in flight, so `SHOW PROCESSLIST`'s `Info` column
    /// and the slow-query log can name it. **Off by default, and turning it on
    /// is a decision about user data, not about diagnostics.**
    ///
    /// This server's standing rule is that a statement is never logged and
    /// never retained: `docs/server.md` says so from its second paragraph, and
    /// the reason is that statement text carries whatever the client put in
    /// it — an email address in a `WHERE`, a token in an `INSERT`, a name in
    /// an `UPDATE`. None of that belongs in a process list a second account
    /// can read or in a log file that outlives the row.
    ///
    /// With it on:
    ///
    /// * `SHOW PROCESSLIST` reports `Info` for the statements the asking
    ///   account is allowed to see at all — its own always, everybody's with
    ///   the superuser. That is MySQL's `PROCESS` privilege, with this
    ///   server's superuser in its place.
    /// * The slow-query log, if enabled, includes the statement.
    /// * Each connection holds one copy of its current statement for as long
    ///   as it runs, and nothing longer.
    ///
    /// With it off — the default — no statement text is stored anywhere in
    /// this process beyond the buffer it was executed from, `Info` is `NULL`,
    /// and the slow-query log names the statement's *kind* rather than the
    /// statement.
    pub statement_text: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.to_string(),
            port: DEFAULT_PORT,
            user: "root".to_string(),
            password: String::new(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            wait_timeout_secs: DEFAULT_WAIT_TIMEOUT_SECS,
            // Durability-adjacent and file-wide: this one is asked for, never
            // inherited. See the field's doc comment.
            page_reuse: false,
            query_memory_bytes: EngineOptions::default().query_memory_bytes,
            // A trade between resident memory and per-search I/O, and one that
            // also changes what a foreign commit costs. See the field's doc.
            paged_vector_indexes: false,
            max_execution_time_ms: DEFAULT_MAX_EXECUTION_TIME_MS,
            // Overwriting a stored password is never something to do by
            // default; see the field's doc.
            reset_superuser: false,
            slow_query_log_ms: 0,
            // Holding statement text is a policy change about user data, so it
            // is asked for and never inherited. See the field's doc.
            statement_text: false,
        }
    }
}

impl ServerOptions {
    /// Whether the bind address is reachable from outside this machine.
    ///
    /// Used by the CLI to print a warning that names the risk, since the
    /// connection is unencrypted.
    pub fn is_public(&self) -> bool {
        match self.bind.parse::<IpAddr>() {
            Ok(address) => !address.is_loopback(),
            // A host name that is not an IP literal cannot be assumed to be
            // loopback.
            Err(_) => !self.bind.eq_ignore_ascii_case("localhost"),
        }
    }
}

/// A bound listener, ready to serve.
pub struct Server {
    listener: TcpListener,
    path: PathBuf,
    /// What is enforced, and therefore what every session reports.
    limits: session::Limits,
    /// The engine options every connection's handle is opened with.
    engine: EngineOptions,
    /// Lines an operator has to see once, about what happened to the account
    /// store at startup. See [`Server::notices`].
    notices: Vec<String>,
    /// `--user`/`--password`, hashed. The plaintext is dropped in
    /// [`Server::bind`] and no part of this process holds one afterwards.
    bootstrap: acl::Bootstrap,
}

impl Server {
    /// Bind the listener and check the database can be opened.
    ///
    /// The file is opened and closed here so a bad path, a locked file or a
    /// database from another format version is reported at startup rather than
    /// separately to every client that connects.
    ///
    /// It is also where the account store is created, seeded or reset — see
    /// `acl::install`, and [`Server::notices`] for what an operator is told
    /// about which of those happened. Doing it here rather than in the first
    /// connection means a database that cannot be given an account store fails
    /// at startup, where somebody is watching.
    pub fn bind(path: impl AsRef<Path>, options: &ServerOptions) -> io::Result<Self> {
        // Refused here rather than clamped silently: a zero would have to be
        // reported as some `wait_timeout` a client tunes against, and there is
        // no honest number for "never". See the field's doc comment.
        if options.wait_timeout_secs == 0 {
            return Err(io::Error::other(
                "wait_timeout must be at least 1 second; for effectively no idle timeout \
                 ask for a large one (31536000 is MySQL's own maximum)",
            ));
        }

        let path = path.as_ref().to_path_buf();
        let mut db = Database::open(&path).map_err(|error| {
            io::Error::other(format!("cannot open {}: {error}", path.display()))
        })?;
        let installed = acl::install(
            &mut db,
            &options.user,
            &options.password,
            options.reset_superuser,
        )
        .map_err(|error| {
            io::Error::other(format!(
                "cannot set up the account store in {}: {}",
                path.display(),
                error.message
            ))
        })?;
        // Closed before the listener opens: every connection opens its own
        // handle (D2), and holding one here would pin a reader watermark for
        // the life of the process — the same trap `run`'s `FileDevice` keeper
        // exists to avoid.
        drop(db);

        let listener = TcpListener::bind((options.bind.as_str(), options.port))?;
        Ok(Self {
            listener,
            path,
            notices: notices_for(&installed),
            bootstrap: acl::Bootstrap::new(&options.user, &options.password),
            // The clamped cap, not the requested one: a session reports what
            // the accept loop below actually applies.
            limits: session::Limits {
                max_connections: options.max_connections.max(1),
                read_timeout_secs: options.wait_timeout_secs,
                write_timeout_secs: NET_WRITE_TIMEOUT_SECS,
                max_execution_time_ms: options.max_execution_time_ms,
                slow_query_log_ms: options.slow_query_log_ms,
                statement_text: options.statement_text,
            },
            engine: EngineOptions {
                page_reuse: options.page_reuse,
                query_memory_bytes: options.query_memory_bytes,
                paged_vector_indexes: options.paged_vector_indexes,
                ..EngineOptions::default()
            },
        })
    }

    /// What happened to the account store at startup, in lines to print.
    ///
    /// Printed rather than left to the docs because the difference decides
    /// whether `--user`/`--password` did anything at all, and an operator who
    /// assumes they did on a database that already had accounts would believe
    /// they had rotated a password they had not.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// The address actually bound, which is how a caller that asked for port
    /// zero finds out what it got.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections until the listener fails.
    ///
    /// Each connection gets an OS thread and its own [`Database`] handle. A
    /// bare [`FileDevice`] is held open for as long as this runs, so the
    /// process keeps the file's advisory lock even with no connections: a
    /// second `inlaysql serve` on the same file is refused at startup rather
    /// than at some later connection. (The lock belongs to the per-file
    /// `CommitCoordinator`, which lives as long as any `FileDevice` on that
    /// file does — see `crates/inlaysql/src/device.rs`.)
    ///
    /// **A device, not a `Database`, and that difference is load-bearing when
    /// [`ServerOptions::page_reuse`] is on.** Every read-write `CowBTree`
    /// registers as a reader and pins `Device::min_reader_seq` at the sequence
    /// it last read; reclamation only ever offers pages freed *before* that
    /// watermark. A `Database` held here would never read anything again, so
    /// it would pin the watermark at startup for the life of the process and
    /// no page freed afterwards could ever be reclaimed — measured, over the
    /// churn in `page_reuse_bounds_the_file_the_server_writes`: 4.3 MB with
    /// reuse on and this keeper, 13 MB with reuse off, and 15.5 MB with reuse
    /// on and a `Database` keeper, *worse* than not reclaiming at all because
    /// the free-list rows accumulate and nothing draws them down. A
    /// `FileDevice` opens no tree, so it registers no reader and holds only
    /// the lock it is here for.
    pub fn run(&self) -> io::Result<()> {
        let _keeper = FileDevice::open(&self.path).map_err(|error| {
            io::Error::other(format!("cannot open {}: {error}", self.path.display()))
        })?;

        let live = Arc::new(AtomicUsize::new(0));
        let next_id = AtomicU32::new(1);
        // Every live connection, so that one connection's `KILL` can reach
        // another's and `SHOW PROCESSLIST` can list them. The accept loop owns
        // it because it is the only thing that knows when a connection begins
        // and ends; each thread removes itself on the way out, however it ends.
        let registry = Arc::new(Registry::new());
        // The server's counters, from now. Started here rather than in `bind`
        // so `Uptime` is time spent serving rather than time since the file was
        // opened, which is what an operator comparing it against a connection
        // count means by it.
        let counters = Arc::new(Metrics::new());

        for incoming in self.listener.incoming() {
            let stream = match incoming {
                Ok(stream) => stream,
                // One failed accept is not a reason to stop serving the
                // connections that are already working.
                Err(error) => {
                    eprintln!("inlaysql: accept failed: {error}");
                    continue;
                }
            };
            // Counted here, before anything can refuse it: MySQL's
            // `Connections` is attempts, successful or not, and an operator
            // comparing it against `Aborted_connects` is asking exactly how
            // many of the attempts failed.
            counters.record(Counter::Connections);
            let id = next_id.fetch_add(1, Ordering::Relaxed).max(1);
            let path = self.path.clone();
            let bootstrap = self.bootstrap.clone();
            let live = live.clone();
            let limits = self.limits;
            let engine = self.engine;
            let max = limits.max_connections;
            // `unwrap_or` rather than a refusal: a peer address this platform
            // will not report is a process-list column with nothing in it, not
            // a reason to turn a client away.
            let host = stream
                .peer_addr()
                .map(|address| address.to_string())
                .unwrap_or_else(|_| "unknown".to_string());

            if live.fetch_add(1, Ordering::SeqCst) >= max {
                live.fetch_sub(1, Ordering::SeqCst);
                // Two counters, because they answer two different questions: a
                // rising `Aborted_connects` says logins are failing, and this
                // one says *why* — the cap, not a credential.
                counters.record(Counter::ConnectionErrorsMaxConnections);
                counters.record(Counter::AbortedConnects);
                refuse(stream, &MysqlError::too_many_connections());
                continue;
            }
            counters.record_max(
                Counter::MaxUsedConnections,
                registry.live_count() as u64 + 1,
            );

            // Built and registered on the *accept* thread, before the
            // connection thread is spawned. A `KILL` racing a brand-new
            // connection then either finds it or does not, rather than finding
            // a half-initialised entry: the control exists in full or not at
            // all. The same is true of `SHOW PROCESSLIST`, which is why a
            // connection appears in it while it is still handshaking.
            let control = Arc::new(Control::new(
                id,
                host,
                limits.max_execution_time_ms,
                limits.statement_text,
            ));
            match control::clone_socket(&stream) {
                Ok(socket) => control.attach_socket(socket),
                // `KILL CONNECTION` can still stop a running statement without
                // this; what it loses is the ability to unblock an *idle* one
                // before its socket timeout. A degradation worth naming, not a
                // reason to refuse the connection.
                Err(error) => eprintln!(
                    "inlaysql: connection {id}: no second socket descriptor, so KILL will not \
                     interrupt it while it is idle: {error}"
                ),
            }
            registry.register(&control);

            let owned = live.clone();
            let registry_for_thread = registry.clone();
            let counters_for_thread = counters.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("inlaysql-conn-{id}"))
                .spawn(move || {
                    let _slot = Slot(owned);
                    let _entry = Registered(registry_for_thread.clone(), id);
                    if let Err(error) = serve_connection(
                        stream,
                        &path,
                        control,
                        limits,
                        engine,
                        bootstrap,
                        registry_for_thread,
                        counters_for_thread,
                    ) {
                        // Never the statement, never the credentials — a
                        // server log is not the place for either.
                        eprintln!("inlaysql: connection {id} ended: {error}");
                    }
                });

            if let Err(error) = spawned {
                live.fetch_sub(1, Ordering::SeqCst);
                registry.forget(id);
                counters.record(Counter::AbortedConnects);
                eprintln!("inlaysql: could not start a thread for connection {id}: {error}");
            }
        }
        Ok(())
    }
}

/// Decrements the live-connection count however the thread ends.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Removes a connection from the `KILL` registry however the thread ends —
/// including a panic, which is the case that matters: an entry left behind
/// would let a later `KILL` of a recycled id write into a control nothing is
/// reading, would leave a dead connection in `SHOW PROCESSLIST`, and would
/// leave `Threads_connected` counting it for ever, since that number is the
/// size of this map.
struct Registered(Arc<Registry>, u32);

impl Drop for Registered {
    fn drop(&mut self) {
        self.0.forget(self.1);
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_connection(
    stream: TcpStream,
    path: &Path,
    control: Arc<Control>,
    limits: session::Limits,
    engine: EngineOptions,
    bootstrap: acl::Bootstrap,
    registry: Arc<Registry>,
    counters: Arc<Metrics>,
) -> io::Result<()> {
    // A request/response protocol gains nothing from waiting to coalesce small
    // writes, and loses a round trip's latency to it every time.
    stream.set_nodelay(true)?;

    // The two numbers reported as `wait_timeout` and `net_write_timeout`,
    // applied. Without them a client that connects and then says nothing holds
    // its slot until the process ends, so `max_connections` silent sockets are
    // the entire server — no statement, no credential, nothing to log, and no
    // way to get the slot back short of a restart. Set before `try_clone`
    // deliberately: the clone is a second descriptor onto the *same* socket, so
    // both halves get these without setting them twice.
    stream.set_read_timeout(Some(Duration::from_secs(limits.read_timeout_secs)))?;
    stream.set_write_timeout(Some(Duration::from_secs(limits.write_timeout_secs)))?;
    let write_half = stream.try_clone()?;

    // Each connection opens the file for itself: this is the whole of D2. The
    // handles share this process's advisory lock and settle concurrent commits
    // between themselves with first-committer-wins.
    let mut db = match open_database(path, engine) {
        Ok(db) => db,
        Err(error) => {
            // A connection that never got a database never authenticated
            // either, so it counts where every other failed login counts.
            counters.record(Counter::AbortedConnects);
            refuse(
                stream,
                &MysqlError::new(1049, "42000", format!("Cannot open database: {error}")),
            );
            return Ok(());
        }
    };

    // The engine's only way to be told to stop. Installed before the first
    // statement can run, so there is no window in which a connection is
    // serving and cannot be killed.
    db.set_cancel(Box::new(control::Signal::new(Arc::clone(&control))));

    let result = connection::Connection::new(
        stream, write_half, db, control, limits, bootstrap, registry, counters,
    )
    .serve();
    // A socket timeout arrives as a bare `WouldBlock`/`TimedOut` from whatever
    // read or write was in flight, which in a log reads as an unexplained
    // "Resource temporarily unavailable". Name it instead: this is the one
    // connection ending the operator configured, not a fault.
    match result {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "closed after making no progress for its socket timeout \
                     (wait_timeout={}s, net_write_timeout={}s)",
                    limits.read_timeout_secs, limits.write_timeout_secs
                ),
            ))
        }
        other => other,
    }
}

/// One connection's handle on the database file.
///
/// The options are the whole reason this is not `Database::open`: that opens
/// with [`EngineOptions::default`], where page reuse is off and the query
/// memory ceiling is the shipped one, and there is no other way to reach
/// either from here.
fn open_database(path: &Path, engine: EngineOptions) -> inlaysql::Result<Database> {
    Database::open_on_with_options(FileDevice::open(path)?, engine)
}

/// Send one error packet to a client that will not be served, and hang up.
///
/// This always runs before any handshake packet is sent, so nothing has
/// negotiated `CLIENT_PROTOCOL_41` yet — the packet must not carry the
/// SQLSTATE marker that capability implies, or a real client mis-parses it
/// (checked against mysql-connector-python: a marked packet here comes back
/// as the SQLSTATE's five bytes glued onto the front of the message rather
/// than a clean error). See [`protocol::err_packet_before_handshake`].
fn refuse(stream: TcpStream, error: &MysqlError) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut framed = packet::Stream::new(stream, write_half);
    let _ = framed.write_message(&protocol::err_packet_before_handshake(
        error.code,
        &error.message,
    ));
    let _ = framed.flush();
}

/// What an operator is told about the account store, once, at startup.
///
/// The empty-password warning lives here rather than in
/// [`print_exposure_warning`] because `--password` only means something when
/// it actually seeds or resets an account: on a database that already has one,
/// warning about an empty `--password` would be warning about a flag that had
/// no effect.
fn notices_for(installed: &acl::Installed) -> Vec<String> {
    let mut out = Vec::new();
    match installed {
        acl::Installed::Bootstrap {
            user,
            empty_password,
        } => {
            out.push(format!(
                "this database has no accounts in it, so --user/--password are the whole of \
                 them: `{user}` is a superuser and nothing has been written. The first \
                 CREATE USER or GRANT creates the account store, and from that point the file \
                 is the authority and these flags are never read again."
            ));
            if *empty_password {
                out.push(format!(
                    "WARNING: `{user}` has an EMPTY password, so any client that can reach the \
                     port can read and write this database"
                ));
            }
        }
        acl::Installed::Existing => out.push(
            "this database has accounts of its own, so --user and --password were NOT used — \
             credentials come from the file. Lost the last superuser's password? Restart once \
             with --reset-superuser."
                .to_string(),
        ),
        acl::Installed::Reset {
            user,
            empty_password,
        } => {
            out.push(format!(
                "--reset-superuser: `{user}`'s password was set from --user/--password and the \
                 account was made a superuser"
            ));
            if *empty_password {
                out.push(format!(
                    "WARNING: `{user}` was reset to an EMPTY password, so any client that can \
                     reach the port can read and write this database"
                ));
            }
        }
    }
    out
}

/// Write the warning the CLI prints, so the text lives beside the behaviour it
/// describes rather than in an argument parser.
pub fn print_exposure_warning(options: &ServerOptions, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "inlaysql: the MySQL protocol is served in PLAINTEXT — there is no TLS in this version."
    )?;
    if options.is_public() {
        writeln!(
            out,
            "inlaysql: WARNING: bound to {}, which is reachable from other machines. Every \n\
             inlaysql:          statement, result and credential crosses the network in the clear.",
            options.bind
        )?;
    }
    if options.page_reuse {
        // Said at startup, not only in the docs: the constraint is about other
        // processes, so the person who typed the flag is the only one in a
        // position to know whether it holds.
        writeln!(
            out,
            "inlaysql: page reuse is ON: reclaimed pages are overwritten in place, so NOTHING \n\
             inlaysql:          may open this file read-only while this server runs — including \n\
             inlaysql:          `inlaysql serve --mcp`, which opens read-only by default. A \n\
             inlaysql:          lock-free reader cannot be seen, so it cannot be waited for."
        )?;
    }
    if options.statement_text {
        // The default is that this server holds no statement text anywhere.
        // Turning that off is a decision about *user data* — the values a
        // statement carries, not the statement's shape — so it is said out
        // loud at startup, where the person who typed the flag can still
        // reconsider, and not left to whoever later reads a process list.
        writeln!(
            out,
            "inlaysql: statement text recording is ON: the statement each connection is \n\
             inlaysql:          running is held in memory, shown in SHOW PROCESSLIST's Info \n\
             inlaysql:          column to that connection's own account and to any superuser, \n\
             inlaysql:          and written to the slow-query log if one is enabled. Statement \n\
             inlaysql:          text contains whatever the client put in it."
        )?;
    }
    if options.slow_query_log_ms > 0 {
        writeln!(
            out,
            "inlaysql: the slow-query log is ON at {} ms; it writes one line to stderr per \n\
             inlaysql:          statement over that, {}.",
            options.slow_query_log_ms,
            if options.statement_text {
                "including the statement text"
            } else {
                "naming the statement's kind but not its text"
            }
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_bind_is_loopback() {
        let options = ServerOptions::default();
        assert_eq!(options.bind, "127.0.0.1");
        assert!(!options.is_public());
    }

    #[test]
    fn a_public_bind_is_recognised_as_public() {
        for address in ["0.0.0.0", "192.168.1.10", "::", "example.com"] {
            let options = ServerOptions {
                bind: address.to_string(),
                ..ServerOptions::default()
            };
            assert!(options.is_public(), "{address} should count as public");
        }
        for address in ["127.0.0.1", "::1", "localhost"] {
            let options = ServerOptions {
                bind: address.to_string(),
                ..ServerOptions::default()
            };
            assert!(!options.is_public(), "{address} should count as loopback");
        }
    }

    /// Both defaults that a running server's behaviour depends on, pinned:
    /// page reuse is a decision about the whole file (see the field's doc), and
    /// the idle timeout is MySQL's own default and the number the server has
    /// always reported.
    #[test]
    fn the_defaults_are_the_conservative_ones() {
        let options = ServerOptions::default();
        assert!(!options.page_reuse);
        // A paged vector index trades resident memory for per-search I/O and
        // for an O(nodes) re-open on every other connection's commit, so it is
        // a decision an operator makes about their corpus, not a default.
        assert!(!options.paged_vector_indexes);
        assert_eq!(options.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(options.wait_timeout_secs, DEFAULT_WAIT_TIMEOUT_SECS);
        assert_eq!(options.wait_timeout_secs, 28800);
    }

    /// `0` would have to be reported as some `wait_timeout`, and there is no
    /// honest number for "never" — so it is refused at bind, where an operator
    /// sees it, rather than turned into a lie a client tunes against.
    #[test]
    fn a_zero_wait_timeout_is_refused_rather_than_reported_as_something_else() {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-zero-wait-timeout-{}.inlay",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let outcome = Server::bind(
            &path,
            &ServerOptions {
                port: 0,
                wait_timeout_secs: 0,
                ..ServerOptions::default()
            },
        );
        let Err(error) = outcome else {
            panic!("a zero wait_timeout must be refused, not bound");
        };
        assert!(error.to_string().contains("wait_timeout"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    /// Turning page reuse on is a statement about every other process that
    /// might touch the file, so the person who typed the flag is told at
    /// startup — not only in `docs/server.md`, which they may never open.
    #[test]
    fn page_reuse_warns_about_the_readers_it_cannot_see() {
        let mut out = Vec::new();
        print_exposure_warning(&ServerOptions::default(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("page reuse"), "{text}");

        let mut out = Vec::new();
        print_exposure_warning(
            &ServerOptions {
                page_reuse: true,
                ..ServerOptions::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("page reuse is ON"), "{text}");
        assert!(text.contains("read-only"), "{text}");
        assert!(text.contains("--mcp"), "{text}");
    }

    #[test]
    fn the_warning_always_says_plaintext_and_flags_the_risky_defaults() {
        let mut out = Vec::new();
        print_exposure_warning(&ServerOptions::default(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("PLAINTEXT"), "{text}");
        assert!(!text.contains("reachable from other machines"), "{text}");

        let mut out = Vec::new();
        print_exposure_warning(
            &ServerOptions {
                bind: "0.0.0.0".to_string(),
                password: "hunter2".to_string(),
                ..ServerOptions::default()
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("reachable from other machines"), "{text}");
        // The password must never appear in anything the server prints.
        assert!(!text.contains("hunter2"), "{text}");
    }

    /// The empty-password warning moved out of [`print_exposure_warning`] and
    /// into the startup notices, because `--password` only means anything on a
    /// database that has no accounts of its own. It still has to be said — an
    /// open database nobody was told about is the whole failure this feature
    /// exists to stop — and it still must not carry the password itself.
    #[test]
    fn an_empty_bootstrap_password_is_warned_about_and_a_set_one_is_not() {
        let bootstrap = |empty| {
            notices_for(&acl::Installed::Bootstrap {
                user: "root".to_string(),
                empty_password: empty,
            })
            .join("\n")
        };
        assert!(
            bootstrap(true).contains("EMPTY password"),
            "{}",
            bootstrap(true)
        );
        assert!(
            !bootstrap(false).contains("EMPTY password"),
            "{}",
            bootstrap(false)
        );
        // And it says what state the database is actually in, which is the
        // part an operator has to know before they trust the other flags.
        assert!(bootstrap(false).contains("no accounts in it"));
        assert!(bootstrap(false).contains("nothing has been written"));

        // On a database that does have accounts, the flags are named as having
        // done nothing, with the way back out if they were the only copy of
        // the password.
        let existing = notices_for(&acl::Installed::Existing).join("\n");
        assert!(existing.contains("were NOT used"), "{existing}");
        assert!(existing.contains("--reset-superuser"), "{existing}");
    }
}
