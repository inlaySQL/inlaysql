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
//! * **One credential, from a flag or the environment; no user table.** The
//!   password is never logged, and a rejected login says only "access denied",
//!   without hinting which half was wrong.
//! * The `mysql_native_password` exchange is challenge-response, so the password
//!   itself is not sent even though the channel is unencrypted. That protects
//!   the password, not the data.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod auth;
mod connection;
mod errors;
mod infoschema;
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

use inlaysql::{Database, EngineOptions, FileDevice};

pub use connection::Credentials;
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

/// How a [`Server`] should be set up.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// The address to bind. Defaults to loopback.
    pub bind: String,
    /// The port to bind. Zero asks the operating system for a free one, which
    /// is what the tests use.
    pub port: u16,
    /// The single user name accepted.
    pub user: String,
    /// Its password. Empty means the server accepts an empty password, which
    /// is only ever appropriate on loopback.
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
    credentials: Credentials,
    /// What is enforced, and therefore what every session reports.
    limits: session::Limits,
    /// The engine options every connection's handle is opened with.
    engine: EngineOptions,
}

impl Server {
    /// Bind the listener and check the database can be opened.
    ///
    /// The file is opened and closed here so a bad path, a locked file or a
    /// database from another format version is reported at startup rather than
    /// separately to every client that connects.
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
        Database::open(&path).map_err(|error| {
            io::Error::other(format!("cannot open {}: {error}", path.display()))
        })?;

        let listener = TcpListener::bind((options.bind.as_str(), options.port))?;
        Ok(Self {
            listener,
            path,
            credentials: Credentials {
                user: options.user.clone(),
                password: options.password.clone(),
            },
            // The clamped cap, not the requested one: a session reports what
            // the accept loop below actually applies.
            limits: session::Limits {
                max_connections: options.max_connections.max(1),
                read_timeout_secs: options.wait_timeout_secs,
                write_timeout_secs: NET_WRITE_TIMEOUT_SECS,
            },
            engine: EngineOptions {
                page_reuse: options.page_reuse,
                query_memory_bytes: options.query_memory_bytes,
                ..EngineOptions::default()
            },
        })
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
            let id = next_id.fetch_add(1, Ordering::Relaxed).max(1);
            let path = self.path.clone();
            let credentials = self.credentials.clone();
            let live = live.clone();
            let limits = self.limits;
            let engine = self.engine;
            let max = limits.max_connections;

            if live.fetch_add(1, Ordering::SeqCst) >= max {
                live.fetch_sub(1, Ordering::SeqCst);
                refuse(stream, &MysqlError::too_many_connections());
                continue;
            }

            let owned = live.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("inlaysql-conn-{id}"))
                .spawn(move || {
                    let _slot = Slot(owned);
                    if let Err(error) =
                        serve_connection(stream, &path, &credentials, id, limits, engine)
                    {
                        // Never the statement, never the credentials — a
                        // server log is not the place for either.
                        eprintln!("inlaysql: connection {id} ended: {error}");
                    }
                });

            if let Err(error) = spawned {
                live.fetch_sub(1, Ordering::SeqCst);
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

fn serve_connection(
    stream: TcpStream,
    path: &Path,
    credentials: &Credentials,
    id: u32,
    limits: session::Limits,
    engine: EngineOptions,
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
    let db = match open_database(path, engine) {
        Ok(db) => db,
        Err(error) => {
            refuse(
                stream,
                &MysqlError::new(1049, "42000", format!("Cannot open database: {error}")),
            );
            return Ok(());
        }
    };

    let result = connection::Connection::new(stream, write_half, db, id, limits).serve(credentials);
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
    if options.password.is_empty() {
        writeln!(
            out,
            "inlaysql: WARNING: no password is set, so any client that can reach the port can \n\
             inlaysql:          read and write this database."
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
        assert!(text.contains("no password is set"), "{text}");
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
        assert!(!text.contains("no password is set"), "{text}");
        // The password must never appear in anything the server prints.
        assert!(!text.contains("hunter2"), "{text}");
    }
}
