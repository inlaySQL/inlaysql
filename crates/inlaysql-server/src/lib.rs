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
//! first-committer-wins, so this needs no locking of its own. There is no async
//! runtime anywhere in this crate, which is deliberate: the workspace has zero
//! async dependencies and this is not the crate that changes that.
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

use inlaysql::Database;

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
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.to_string(),
            port: DEFAULT_PORT,
            user: "root".to_string(),
            password: String::new(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
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
    max_connections: usize,
}

impl Server {
    /// Bind the listener and check the database can be opened.
    ///
    /// The file is opened and closed here so a bad path, a locked file or a
    /// database from another format version is reported at startup rather than
    /// separately to every client that connects.
    pub fn bind(path: impl AsRef<Path>, options: &ServerOptions) -> io::Result<Self> {
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
            max_connections: options.max_connections.max(1),
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
    /// further handle is held open for as long as this runs, so the process
    /// keeps the file's advisory lock: a second `inlaysql serve` on the same
    /// file is refused at startup rather than at some later connection.
    pub fn run(&self) -> io::Result<()> {
        let _keeper = Database::open(&self.path).map_err(|error| {
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
            let max = self.max_connections;

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
                    if let Err(error) = serve_connection(stream, &path, &credentials, id) {
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
) -> io::Result<()> {
    // A request/response protocol gains nothing from waiting to coalesce small
    // writes, and loses a round trip's latency to it every time.
    stream.set_nodelay(true)?;
    let write_half = stream.try_clone()?;

    // Each connection opens the file for itself: this is the whole of D2. The
    // handles share this process's advisory lock and settle concurrent commits
    // between themselves with first-committer-wins.
    let db = match Database::open(path) {
        Ok(db) => db,
        Err(error) => {
            refuse(
                stream,
                &MysqlError::new(1049, "42000", format!("Cannot open database: {error}")),
            );
            return Ok(());
        }
    };

    connection::Connection::new(stream, write_half, db, id).serve(credentials)
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
