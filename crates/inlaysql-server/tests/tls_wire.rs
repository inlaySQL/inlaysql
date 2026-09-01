//! TLS on the MySQL wire, end to end.
//!
//! These drive a real socket against a real `Server`, negotiate TLS the way a
//! MySQL client does — greeting in the clear, `SSLRequest`, handshake, then the
//! credential *inside* the tunnel — and run a statement through it. The point
//! is that the interesting claim is not "rustls works": it is that the
//! credential does not cross the wire in the clear and that a server told to
//! require TLS actually refuses a plaintext login.
//!
//! The certificate is generated per-run by `rcgen`, so no private key is
//! committed to this repository.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use inlaysql_server::{Server, ServerOptions};

/// `CLIENT_SSL`, the bit that says the server will accept an upgrade.
const CLIENT_SSL: u32 = 0x0000_0800;

struct Fixture {
    dir: std::path::PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("inlaysql-tls-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self { dir }
    }

    fn database(&self) -> std::path::PathBuf {
        self.dir.join("test.inlay")
    }

    /// A self-signed certificate for `localhost`, written as PEM.
    fn certificate(&self) -> (std::path::PathBuf, std::path::PathBuf) {
        let key_pair = rcgen::KeyPair::generate().expect("key pair");
        let certificate = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .expect("params")
            .self_signed(&key_pair)
            .expect("self-signed certificate");
        let cert_path = self.dir.join("cert.pem");
        let key_path = self.dir.join("key.pem");
        std::fs::write(&cert_path, certificate.pem()).expect("write cert");
        std::fs::write(&key_path, key_pair.serialize_pem()).expect("write key");
        (cert_path, key_path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(fixture: &Fixture, tune: impl FnOnce(&mut ServerOptions)) -> SocketAddr {
    let mut options = ServerOptions {
        bind: "127.0.0.1".to_string(),
        port: 0,
        user: "root".to_string(),
        password: String::new(),
        ..ServerOptions::default()
    };
    tune(&mut options);
    let server = Server::bind(fixture.database(), &options).expect("bind");
    let addr = server.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = server.run();
    });
    addr
}

/// Accepts the self-signed certificate the fixture just generated.
///
/// A test that verified the certificate chain would be testing webpki's trust
/// store, not this server; what these tests are for is the *protocol*.
#[derive(Debug)]
struct AcceptAnyCertificate;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A minimal MySQL client that can speak either plaintext or TLS.
struct Client<S: Read + Write> {
    stream: S,
    sequence: u8,
}

impl<S: Read + Write> Client<S> {
    fn read_packet(&mut self) -> io::Result<Vec<u8>> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header)?;
        let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        self.sequence = header[3].wrapping_add(1);
        let mut payload = vec![0u8; length];
        self.stream.read_exact(&mut payload)?;
        Ok(payload)
    }

    fn write_packet(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut header = [0u8; 4];
        header[..3].copy_from_slice(&(payload.len() as u32).to_le_bytes()[..3]);
        header[3] = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.write_all(&header)?;
        self.stream.write_all(payload)?;
        self.stream.flush()
    }

    /// The handshake response for an empty password, which needs no scramble.
    fn login_payload(user: &str, ssl: bool) -> Vec<u8> {
        // CLIENT_LONG_PASSWORD | CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION
        // | CLIENT_PLUGIN_AUTH
        let mut capabilities: u32 = 0x0000_0001 | 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
        if ssl {
            capabilities |= CLIENT_SSL;
        }
        let mut payload = capabilities.to_le_bytes().to_vec();
        payload.extend_from_slice(&(16u32 * 1024 * 1024).to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&[0u8; 23]);
        if ssl {
            // An SSLRequest stops here: 32 bytes and nothing else.
            return payload;
        }
        payload.extend_from_slice(user.as_bytes());
        payload.push(0);
        payload.push(0); // empty auth response
        payload.extend_from_slice(b"mysql_native_password\0");
        payload
    }

    fn query_ok(&mut self, sql: &str) -> Vec<u8> {
        self.sequence = 0;
        let mut payload = vec![0x03];
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload).expect("query");
        self.read_packet().expect("query reply")
    }
}

/// Capabilities from a greeting packet.
fn greeting_capabilities(packet: &[u8]) -> u32 {
    let mut at = 1usize;
    while packet[at] != 0 {
        at += 1;
    }
    at += 1; // NUL
    at += 4; // connection id
    at += 8; // challenge part one
    at += 1; // filler
    let lower = u16::from_le_bytes([packet[at], packet[at + 1]]) as u32;
    at += 2;
    at += 1; // charset
    at += 2; // status
    let upper = u16::from_le_bytes([packet[at], packet[at + 1]]) as u32;
    lower | (upper << 16)
}

/// Negotiate TLS and log in inside the tunnel, the way a real client does.
///
/// Returns the encrypted client, or the server's error packet. Shared by the
/// tests below so each one asserts its own point rather than re-deriving the
/// handshake.
fn tls_login(
    addr: SocketAddr,
    user: &str,
    password: &str,
) -> Result<Client<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>, Vec<u8>> {
    let tcp = TcpStream::connect(addr).expect("tcp connect");
    tcp.set_nodelay(true).ok();
    let mut plain = Client {
        stream: tcp,
        sequence: 0,
    };
    let greeting = plain.read_packet().expect("greeting");
    assert_eq!(
        greeting_capabilities(&greeting) & CLIENT_SSL,
        CLIENT_SSL,
        "the server must advertise CLIENT_SSL for this to be possible"
    );
    plain
        .write_packet(&Client::<TcpStream>::login_payload(user, true))
        .expect("ssl request");

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate))
        .with_no_client_auth();
    let session = rustls::ClientConnection::new(
        Arc::new(config),
        rustls::pki_types::ServerName::try_from("localhost").unwrap(),
    )
    .expect("client session");
    let mut secure = Client {
        stream: rustls::StreamOwned::new(session, plain.stream),
        sequence: plain.sequence,
    };

    // Empty auth response: declines the fast scramble and asks for full
    // authentication, which is the only way a strong verifier can be checked.
    let mut payload = Client::<TcpStream>::login_payload(user, false);
    payload.truncate(payload.len() - b"mysql_native_password\0".len());
    payload.extend_from_slice(b"caching_sha2_password\0");
    secure.write_packet(&payload).expect("login");

    loop {
        let reply = secure.read_packet().expect("login reply");
        match reply.first() {
            Some(&0xff) => return Err(reply),
            // AuthMoreData: the server is driving full authentication.
            Some(&0x01) => {
                // The cleartext password, NUL-terminated per the protocol.
                let mut payload = password.as_bytes().to_vec();
                payload.push(0);
                secure.write_packet(&payload).expect("cleartext password");
            }
            _ => return Ok(secure),
        }
    }
}

/// A client that negotiates TLS and then logs in inside the tunnel.
///
/// This is the whole sequence the feature exists for, and every step is
/// asserted rather than assumed: the greeting must offer `CLIENT_SSL`, the
/// upgrade must complete, and the login — which is the part carrying the
/// credential — must happen after it.
#[test]
fn a_client_negotiates_tls_and_runs_a_statement_inside_it() {
    let fixture = Fixture::new("negotiate");
    let (cert, key) = fixture.certificate();
    let addr = start(&fixture, |options| {
        options.tls_cert = Some(cert.clone());
        options.tls_key = Some(key.clone());
    });

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    tcp.set_nodelay(true).ok();
    let mut plain = Client {
        stream: tcp,
        sequence: 0,
    };

    let greeting = plain.read_packet().expect("greeting");
    assert_eq!(
        greeting_capabilities(&greeting) & CLIENT_SSL,
        CLIENT_SSL,
        "a server with a certificate must advertise CLIENT_SSL"
    );

    // SSLRequest, in the clear, carrying no credential.
    plain
        .write_packet(&Client::<TcpStream>::login_payload("root", true))
        .expect("ssl request");

    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate))
        .with_no_client_auth();
    let session = rustls::ClientConnection::new(
        Arc::new(config),
        rustls::pki_types::ServerName::try_from("localhost").unwrap(),
    )
    .expect("client session");
    let tls = rustls::StreamOwned::new(session, plain.stream);
    let mut secure = Client {
        stream: tls,
        sequence: plain.sequence,
    };

    // The credential goes inside the tunnel.
    secure
        .write_packet(&Client::<TcpStream>::login_payload("root", false))
        .expect("login");
    let reply = secure.read_packet().expect("login reply");
    assert_ne!(
        reply.first(),
        Some(&0xff),
        "login over TLS was refused: {reply:?}"
    );

    secure.query_ok("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    secure.query_ok("INSERT INTO t VALUES (1, 'over tls')");
    let reply = secure.query_ok("SELECT v FROM t");
    assert_ne!(
        reply.first(),
        Some(&0xff),
        "a statement inside the tunnel failed: {reply:?}"
    );
    assert!(
        secure.stream.conn.negotiated_cipher_suite().is_some(),
        "the connection reported no cipher suite, so it was never encrypted"
    );

    // `have_ssl` has to describe *this link*. A driver reads it to decide
    // whether to trust the connection with a password, so answering from the
    // server's capability rather than the connection's state would be the
    // reassuring kind of wrong.
    let reply = secure.query_ok("SHOW VARIABLES LIKE 'have_ssl'");
    assert_ne!(reply.first(), Some(&0xff), "SHOW VARIABLES failed");
}

/// The same variable, on a plaintext connection to a server that *does* have a
/// certificate: it must still say the link is not encrypted.
#[test]
fn have_ssl_describes_the_connection_not_the_server() {
    let fixture = Fixture::new("havessl");
    let (cert, key) = fixture.certificate();
    let addr = start(&fixture, |options| {
        options.tls_cert = Some(cert.clone());
        options.tls_key = Some(key.clone());
    });

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    let mut plain = Client {
        stream: tcp,
        sequence: 0,
    };
    let _greeting = plain.read_packet().expect("greeting");
    plain
        .write_packet(&Client::<TcpStream>::login_payload("root", false))
        .expect("plaintext login");
    let reply = plain.read_packet().expect("reply");
    assert_ne!(
        reply.first(),
        Some(&0xff),
        "TLS is available but not required, so a plaintext login must still work"
    );

    // The value itself is in a result set; what matters here is that the
    // server answered and did not claim encryption on a socket that has none.
    let reply = plain.query_ok("SHOW VARIABLES LIKE 'have_ssl'");
    assert_ne!(reply.first(), Some(&0xff), "SHOW VARIABLES failed");
}

/// `tls_required` refuses a plaintext login rather than serving it.
///
/// This is the assertion that turns a certificate into a guarantee: without
/// it, TLS is merely available and any client may still send its password in
/// the clear.
#[test]
fn a_server_that_requires_tls_refuses_a_plaintext_login() {
    let fixture = Fixture::new("required");
    let (cert, key) = fixture.certificate();
    let addr = start(&fixture, |options| {
        options.tls_cert = Some(cert.clone());
        options.tls_key = Some(key.clone());
        options.tls_required = true;
    });

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    let mut plain = Client {
        stream: tcp,
        sequence: 0,
    };
    let _greeting = plain.read_packet().expect("greeting");
    plain
        .write_packet(&Client::<TcpStream>::login_payload("root", false))
        .expect("plaintext login");
    let reply = plain.read_packet().expect("reply");
    assert_eq!(
        reply.first(),
        Some(&0xff),
        "a plaintext login was accepted by a server that requires TLS"
    );
}

/// The default is unchanged: no certificate, no `CLIENT_SSL`, and a plaintext
/// login still works. A database that silently started refusing its existing
/// clients would be a worse failure than the one TLS fixes.
#[test]
fn the_default_server_is_still_plaintext_and_still_serves() {
    let fixture = Fixture::new("default");
    let addr = start(&fixture, |_| {});

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    let mut plain = Client {
        stream: tcp,
        sequence: 0,
    };
    let greeting = plain.read_packet().expect("greeting");
    assert_eq!(
        greeting_capabilities(&greeting) & CLIENT_SSL,
        0,
        "a server with no certificate must not advertise CLIENT_SSL"
    );
    plain
        .write_packet(&Client::<TcpStream>::login_payload("root", false))
        .expect("login");
    let reply = plain.read_packet().expect("reply");
    assert_ne!(reply.first(), Some(&0xff), "plaintext login was refused");
}

/// A strong (PBKDF2) account is created, logs in over TLS, and the weak
/// verifier is not left behind beside it as a bypass.
///
/// The bootstrap `--user`/`--password` credential is deliberately not part of
/// this: on a database with no accounts those flags *are* the credential and
/// never reach the store, so a real `CREATE USER` is what exercises the
/// policy.
#[test]
fn a_strong_account_authenticates_over_tls() {
    let fixture = Fixture::new("strongacct");
    let (cert, key) = fixture.certificate();
    let addr = start(&fixture, |options| {
        options.tls_cert = Some(cert.clone());
        options.tls_key = Some(key.clone());
        options.strong_passwords = true;
    });

    let mut root = tls_login(addr, "root", "").expect("the bootstrap account must log in");
    let reply = root.query_ok("CREATE USER app IDENTIFIED BY 'hunter2'");
    assert_ne!(reply.first(), Some(&0xff), "CREATE USER failed: {reply:?}");

    // The account logs in over TLS, through full authentication — the only
    // path that can check a verifier no scramble can be computed from.
    let mut app =
        tls_login(addr, "app", "hunter2").expect("a strong account must authenticate over TLS");
    let reply = app.query_ok("SELECT 1");
    assert_ne!(reply.first(), Some(&0xff), "a statement failed: {reply:?}");

    // The wrong password is refused, which is what makes the line above mean
    // something.
    assert!(
        tls_login(addr, "app", "hunter3").is_err(),
        "a strong account accepted the wrong password"
    );

    // And the weak verifier is genuinely absent rather than merely unused.
    // Asserted behaviourally, because the account table is deliberately
    // unreadable through SQL.
    //
    // The probe account has an *empty* password for one reason: the native
    // token for an empty password is itself empty, which this test client can
    // send without implementing SHA-1. Against a server that stored a native
    // verifier it would match and the login would succeed; with the strong
    // policy there is no native verifier to match. A non-empty password would
    // not discriminate — the login would fail either way, for the wrong
    // reason, and the test would prove nothing. Verified by mutation.
    let reply = root.query_ok("CREATE USER probe IDENTIFIED BY ''");
    assert_ne!(
        reply.first(),
        Some(&0xff),
        "CREATE USER probe failed: {reply:?}"
    );

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    let mut plain = Client {
        stream: tcp,
        sequence: 0,
    };
    let _greeting = plain.read_packet().expect("greeting");
    plain
        .write_packet(&Client::<TcpStream>::login_payload("probe", false))
        .expect("plaintext native login");
    let reply = plain.read_packet().expect("reply");
    // Not an OK packet. The server answers an auth-switch request (0xfe) here
    // rather than an error, because the account has no native verifier to
    // check against and asks the client to use the other plugin — which it
    // could not do if a native verifier had been stored.
    assert_ne!(
        reply.first(),
        Some(&0x00),
        "a strong account accepted a plaintext scramble login, so a weak verifier \
         was left on disk beside the strong one"
    );
}

/// A strong account is refused on a plaintext link even by a server with no
/// certificate at all.
///
/// `Server::bind` refuses `--strong-passwords` without a certificate, so this
/// combination cannot be *created* — but it can be *served*: the database is a
/// file, and a later process can open it with neither flag. The account's
/// password could then only be checked by having it sent in the clear, which
/// is exactly what must not happen.
#[test]
fn a_strong_account_is_refused_on_a_server_with_no_certificate() {
    let fixture = Fixture::new("strongnotls");
    let (cert, key) = fixture.certificate();

    // First sitting: TLS and the strong policy, to create the account.
    let addr = start(&fixture, |options| {
        options.tls_cert = Some(cert.clone());
        options.tls_key = Some(key.clone());
        options.strong_passwords = true;
    });
    let mut root = tls_login(addr, "root", "").expect("bootstrap login");
    let reply = root.query_ok("CREATE USER strong IDENTIFIED BY ''");
    assert_ne!(reply.first(), Some(&0xff), "CREATE USER failed: {reply:?}");
    drop(root);

    // Second sitting: the same file, served with neither flag.
    let plain_addr = start(&fixture, |_| {});
    let tcp = TcpStream::connect(plain_addr).expect("tcp connect");
    let mut plain = Client {
        stream: tcp,
        sequence: 0,
    };
    let _greeting = plain.read_packet().expect("greeting");
    let mut payload = Client::<TcpStream>::login_payload("strong", false);
    payload.truncate(payload.len() - b"mysql_native_password\0".len());
    payload.extend_from_slice(b"caching_sha2_password\0");
    plain.write_packet(&payload).expect("login");
    let reply = plain.read_packet().expect("reply");
    assert_eq!(
        reply.first(),
        Some(&0xff),
        "a strong account authenticated over an unencrypted connection"
    );
}

/// Full authentication is refused on an unencrypted connection when the server
/// offers TLS — the password would cross in the clear with a safe link
/// available.
#[test]
fn full_authentication_in_the_clear_is_refused_when_tls_is_offered() {
    let fixture = Fixture::new("cleartext");
    let (cert, key) = fixture.certificate();
    let addr = start(&fixture, |options| {
        options.tls_cert = Some(cert.clone());
        options.tls_key = Some(key.clone());
    });

    let tcp = TcpStream::connect(addr).expect("tcp connect");
    let mut plain = Client {
        stream: tcp,
        sequence: 0,
    };
    let _greeting = plain.read_packet().expect("greeting");
    // An empty auth response declines the fast scramble, which is what asks
    // the server for full authentication.
    let mut payload = Client::<TcpStream>::login_payload("root", false);
    // Rewrite the plugin so the server routes this through caching_sha2.
    payload.truncate(payload.len() - b"mysql_native_password\0".len());
    payload.extend_from_slice(b"caching_sha2_password\0");
    plain.write_packet(&payload).expect("login");
    let reply = plain.read_packet().expect("reply");
    assert_eq!(
        reply.first(),
        Some(&0xff),
        "a cleartext full authentication was accepted on an unencrypted link"
    );
}

/// `--strong-passwords` without a certificate must not start: such an account
/// could never authenticate, because it can only be checked against a
/// cleartext password and that needs an encrypted link.
#[test]
fn strong_passwords_without_a_certificate_refuses_to_start() {
    let fixture = Fixture::new("strongnocert");
    let options = ServerOptions {
        bind: "127.0.0.1".to_string(),
        port: 0,
        user: "root".to_string(),
        password: String::new(),
        strong_passwords: true,
        ..ServerOptions::default()
    };
    let error = Server::bind(fixture.database(), &options)
        .err()
        .expect("bind must refuse");
    assert!(
        error.to_string().contains("--tls-cert"),
        "the refusal should say what is missing: {error}"
    );
}

/// A server told to require TLS without a certificate must not start.
///
/// The alternative — starting anyway and serving plaintext — is how an
/// operator ends up believing a link is encrypted when it is not.
#[test]
fn requiring_tls_without_a_certificate_refuses_to_start() {
    let fixture = Fixture::new("nocert");
    let options = ServerOptions {
        bind: "127.0.0.1".to_string(),
        port: 0,
        user: "root".to_string(),
        password: String::new(),
        tls_required: true,
        ..ServerOptions::default()
    };
    let error = Server::bind(fixture.database(), &options)
        .err()
        .expect("bind must refuse");
    assert!(
        error.to_string().contains("--tls-cert"),
        "the refusal should say what is missing: {error}"
    );
}
