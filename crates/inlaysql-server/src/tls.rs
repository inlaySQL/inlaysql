//! TLS for the MySQL wire.
//!
//! # What this changes about the server's posture
//!
//! Before this, the server was plaintext with no way to be anything else: it
//! never advertised `CLIENT_SSL`, and a client that asked for TLS was refused
//! with an error saying so. That is why every document in this repository told
//! the reader not to put it on a network they do not own. With a certificate
//! configured the server advertises `CLIENT_SSL`, upgrades the socket when the
//! client asks, and — with [`TlsPolicy::Required`] — refuses any login that did
//! not upgrade.
//!
//! **The default is unchanged and still plaintext.** A server started without
//! a certificate behaves exactly as it did, because a database that silently
//! started refusing its existing clients would be a worse failure than the one
//! this module fixes.
//!
//! # Where the upgrade happens
//!
//! MySQL's TLS handshake is an upgrade in the middle of its own handshake, not
//! before it. The server sends its greeting in the clear; a client that wants
//! TLS replies with a 32-byte `SSLRequest` — the first half of an ordinary
//! handshake response, with `CLIENT_SSL` set and nothing after the reserved
//! bytes — and then both sides start a TLS handshake on the same socket. The
//! *real* handshake response, the one carrying the user name and the
//! password proof, is then sent inside TLS. That ordering is the entire point:
//! it is what keeps the credential out of the clear.
//!
//! [`MaybeTls`] exists because of that ordering. The connection is generic over
//! its stream, but a stream cannot change type half way through its life, so
//! the concrete type is this enum and the upgrade swaps what is inside it.
//!
//! # Scope
//!
//! Server certificates only. Client certificate authentication is not offered:
//! it is a second authentication system to design, document and get wrong, and
//! nothing asks for it yet.

use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// Whether a connection may, must, or cannot use TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsPolicy {
    /// No certificate configured. `CLIENT_SSL` is not advertised and a client
    /// asking for it is refused. The default, and what every existing
    /// deployment gets.
    #[default]
    Disabled,
    /// A certificate is configured. `CLIENT_SSL` is advertised, a client may
    /// upgrade, and a client that does not is still served — which is what
    /// makes turning TLS on a non-breaking change for existing clients.
    Available,
    /// A certificate is configured and plaintext logins are refused. This is
    /// the setting that makes "the credential cannot cross the network in the
    /// clear" a property of the server rather than a hope about its clients.
    Required,
}

impl TlsPolicy {
    /// Whether the greeting should advertise `CLIENT_SSL`.
    pub fn advertises(self) -> bool {
        !matches!(self, TlsPolicy::Disabled)
    }
}

/// A loaded server certificate chain and key, ready to accept connections.
#[derive(Clone)]
pub struct TlsConfig {
    config: Arc<ServerConfig>,
    policy: TlsPolicy,
    certificate: PathBuf,
}

impl fmt::Debug for TlsConfig {
    /// Deliberately says nothing about the key.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsConfig")
            .field("certificate", &self.certificate)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl TlsConfig {
    /// Load a PEM certificate chain and private key.
    ///
    /// Errors name the file and what was wrong with it. A server that cannot
    /// load its certificate must fail to start rather than fall back to
    /// plaintext: falling back is how an operator ends up believing a link is
    /// encrypted when it is not, which is the failure this whole module exists
    /// to prevent.
    pub fn load(certificate: &Path, key: &Path, policy: TlsPolicy) -> Result<Self, String> {
        let certificates = load_certificates(certificate)?;
        let private_key = load_key(key)?;

        // `ring` rather than the default provider — see the workspace manifest
        // for why — and installed per-config rather than process-globally, so
        // linking this crate never mutates a global another crate may also be
        // setting.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| format!("TLS: {error}"))?
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)
            .map_err(|error| {
                format!(
                    "TLS: {} and {} are not a usable certificate/key pair: {error}",
                    certificate.display(),
                    key.display()
                )
            })?;

        Ok(Self {
            config: Arc::new(config),
            policy,
            certificate: certificate.to_path_buf(),
        })
    }

    /// The policy this certificate was loaded under.
    pub fn policy(&self) -> TlsPolicy {
        self.policy
    }

    /// Begin a server-side TLS session.
    fn accept(&self) -> Result<ServerConnection, io::Error> {
        ServerConnection::new(Arc::clone(&self.config))
            .map_err(|error| io::Error::other(format!("TLS handshake could not start: {error}")))
    }
}

fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let file = File::open(path)
        .map_err(|error| format!("TLS: cannot read certificate {}: {error}", path.display()))?;
    let certificates: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(file))
            .collect::<Result<_, _>>()
            .map_err(|error| format!("TLS: {} is not valid PEM: {error}", path.display()))?;
    if certificates.is_empty() {
        return Err(format!(
            "TLS: {} contains no certificate. A PEM chain is expected, leaf first.",
            path.display()
        ));
    }
    Ok(certificates)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let file = File::open(path)
        .map_err(|error| format!("TLS: cannot read private key {}: {error}", path.display()))?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .map_err(|error| format!("TLS: {} is not valid PEM: {error}", path.display()))?
        .ok_or_else(|| {
            format!(
                "TLS: {} contains no private key. PKCS#8, PKCS#1 and SEC1 are all accepted.",
                path.display()
            )
        })
}

/// A socket with bytes already read out of it that the next reader still needs.
///
/// The MySQL handshake is framed and buffered, so by the time the `SSLRequest`
/// packet has been parsed the `BufReader` in front of the socket may also hold
/// the first bytes of the client's TLS `ClientHello` — a client is entitled to
/// send them immediately, and a fast one does. Handing the bare socket to
/// rustls at that point would lose exactly those bytes and hang the handshake.
/// This replays them first.
pub struct Prefixed<S> {
    pending: Vec<u8>,
    at: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    fn new(pending: Vec<u8>, inner: S) -> Self {
        Self {
            pending,
            at: 0,
            inner,
        }
    }
}

impl<S: Read> Read for Prefixed<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at < self.pending.len() {
            let take = (self.pending.len() - self.at).min(buf.len());
            buf[..take].copy_from_slice(&self.pending[self.at..self.at + take]);
            self.at += take;
            if self.at == self.pending.len() {
                self.pending = Vec::new();
            }
            return Ok(take);
        }
        self.inner.read(buf)
    }
}

impl<S: Write> Write for Prefixed<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The one TLS session a connection has, shared by its reader and its writer.
///
/// `Stream` buffers the two directions independently over two descriptors onto
/// the same socket, which is right for plaintext and impossible for TLS: a TLS
/// session is one state machine over one byte stream, and two of them over two
/// descriptors would each decrypt half a record. So the encrypted case keeps
/// one session behind a lock and points both directions at it. The lock is
/// uncontended in the request/response protocol this serves — a connection is
/// either reading or writing, never both — and its cost is invisible next to
/// the record encryption it guards.
pub type SharedTls<S> = Arc<Mutex<StreamOwned<ServerConnection, Prefixed<S>>>>;

/// What [`crate::packet::Stream`] needs of a stream in order to upgrade it.
///
/// A trait rather than a concrete type because `Stream` is generic and its
/// tests drive it over in-memory pipes that will never speak TLS. Only
/// [`MaybeTls`] implements it, and only the real server uses it.
pub trait Upgradable: Read + Write + Sized {
    /// The negotiated session, shared with the connection's other direction.
    type Session;

    /// A value to leave behind while the real stream is moved out. Reading or
    /// writing it fails; nothing may use one except in that instant.
    fn placeholder() -> Self;

    /// Negotiate TLS, consuming `buffered` first. See [`MaybeTls::upgrade`].
    fn upgrade_with(&mut self, config: &TlsConfig, buffered: Vec<u8>) -> io::Result<Self::Session>;

    /// Point this half at a session the other half negotiated.
    fn adopt_session(&mut self, session: Self::Session);

    /// Whether this half is encrypted.
    fn encrypted(&self) -> bool;
}

impl<S: Read + Write> Upgradable for MaybeTls<S> {
    type Session = SharedTls<S>;

    fn placeholder() -> Self {
        MaybeTls::Poisoned
    }

    fn upgrade_with(&mut self, config: &TlsConfig, buffered: Vec<u8>) -> io::Result<Self::Session> {
        self.upgrade(config, buffered)
    }

    fn adopt_session(&mut self, session: Self::Session) {
        self.adopt(session);
    }

    fn encrypted(&self) -> bool {
        self.is_encrypted()
    }
}

/// A stream that may be upgraded from plaintext to TLS once, in place.
///
/// The MySQL handshake decides whether to encrypt *after* the connection is
/// already reading and writing packets, so the connection's stream type is
/// fixed before anyone knows the answer. This enum is that fixed type.
pub enum MaybeTls<S: Read + Write> {
    /// Not encrypted: either TLS is off, or the client did not ask for it.
    Plain(S),
    /// Encrypted: the client asked and the handshake completed. Shared with
    /// this connection's other direction — see [`SharedTls`].
    Tls(SharedTls<S>),
    /// Held only for the instant the upgrade is swapping the inner stream out.
    /// Reachable outside that instant only if the upgrade panicked part way,
    /// in which case every later read and write fails rather than silently
    /// continuing in the clear.
    Poisoned,
}

impl<S: Read + Write> MaybeTls<S> {
    /// Wrap a freshly accepted socket, before any upgrade.
    pub fn plain(stream: S) -> Self {
        MaybeTls::Plain(stream)
    }

    /// Whether this connection is encrypted, which is what the login path
    /// checks before honouring [`TlsPolicy::Required`].
    pub fn is_encrypted(&self) -> bool {
        matches!(self, MaybeTls::Tls(_))
    }

    /// Complete a server-side TLS handshake, returning the session for this
    /// connection's *other* direction to share.
    ///
    /// `buffered` is whatever the caller's read buffer had already taken off
    /// the socket — see [`Prefixed`] for why losing it would hang the
    /// handshake.
    ///
    /// On failure the stream is left [`MaybeTls::Poisoned`] rather than
    /// reverted to plaintext. Reverting would mean a client that asked for TLS,
    /// and whose handshake failed, carries on unencrypted and sends its
    /// password in the clear — the exact outcome a failed handshake must
    /// prevent.
    pub fn upgrade(&mut self, config: &TlsConfig, buffered: Vec<u8>) -> io::Result<SharedTls<S>> {
        let plain = match std::mem::replace(self, MaybeTls::Poisoned) {
            MaybeTls::Plain(stream) => stream,
            MaybeTls::Tls(stream) => {
                *self = MaybeTls::Tls(stream);
                return Err(io::Error::other("TLS was already negotiated"));
            }
            MaybeTls::Poisoned => return Err(io::Error::other("stream is poisoned")),
        };
        let session = config.accept()?;
        let mut stream = StreamOwned::new(session, Prefixed::new(buffered, plain));
        // Driven to completion here rather than by the first packet read, so a
        // failure is reported as a TLS failure at the point it happened.
        while stream.conn.is_handshaking() {
            stream.conn.complete_io(&mut stream.sock)?;
        }
        let shared: SharedTls<S> = Arc::new(Mutex::new(stream));
        *self = MaybeTls::Tls(Arc::clone(&shared));
        Ok(shared)
    }

    /// Point this half at a session its sibling already negotiated.
    pub fn adopt(&mut self, shared: SharedTls<S>) {
        *self = MaybeTls::Tls(shared);
    }
}

/// A lock held across a TLS read or write.
///
/// A poisoned mutex means the other direction panicked mid-record, so the
/// session's state is unknown; refusing is the only safe answer.
fn locked<S: Read + Write, T>(
    shared: &SharedTls<S>,
    with: impl FnOnce(&mut StreamOwned<ServerConnection, Prefixed<S>>) -> io::Result<T>,
) -> io::Result<T> {
    let mut guard = shared
        .lock()
        .map_err(|_| io::Error::other("TLS session was poisoned by a panic"))?;
    with(&mut guard)
}

impl<S: Read + Write> Read for MaybeTls<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            MaybeTls::Plain(stream) => stream.read(buf),
            MaybeTls::Tls(shared) => locked(shared, |stream| stream.read(buf)),
            MaybeTls::Poisoned => Err(io::Error::other("TLS handshake failed on this connection")),
        }
    }
}

impl<S: Read + Write> Write for MaybeTls<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            MaybeTls::Plain(stream) => stream.write(buf),
            MaybeTls::Tls(shared) => locked(shared, |stream| stream.write(buf)),
            MaybeTls::Poisoned => Err(io::Error::other("TLS handshake failed on this connection")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            MaybeTls::Plain(stream) => stream.flush(),
            MaybeTls::Tls(shared) => locked(shared, |stream| stream.flush()),
            MaybeTls::Poisoned => Err(io::Error::other("TLS handshake failed on this connection")),
        }
    }
}
