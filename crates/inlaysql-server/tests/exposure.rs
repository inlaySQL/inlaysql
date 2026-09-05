//! What `Server::bind` refuses to serve to the network, and what it says.
//!
//! These assert the *exact* sentence an operator reads, not that some error
//! happened. A refusal is a piece of user interface: it is read once, under
//! time pressure, by somebody who wants the server up — so what it names (the
//! address, the fact, the flag that fixes it, the way back to safe) is the
//! feature, and a test that only checks `is_err()` would let all four of those
//! rot away.
//!
//! Nothing here binds a socket. The refusal happens before the listener, which
//! is the point, so `192.168.1.10` never has to be an address this machine
//! actually has.

use std::path::PathBuf;

use inlaysql_server::{Server, ServerOptions};

/// A private-network address that reaches other machines and that no test
/// here ever binds.
const OFF_LOOPBACK: &str = "192.168.1.10";

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("inlaysql-exposure-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self { dir }
    }

    fn database(&self) -> PathBuf {
        self.dir.join("test.inlay")
    }

    /// A database that has accounts of its own, so C1 and C2 no longer apply
    /// and the TLS conditions are what is left.
    fn with_accounts(&self) -> PathBuf {
        let path = self.database();
        inlaysql_server::add_account(&path, "admin", "s3cret", true).expect("first account");
        path
    }

    fn certificate(&self) -> (PathBuf, PathBuf) {
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

fn options(bind: &str) -> ServerOptions {
    ServerOptions {
        bind: bind.to_string(),
        port: 3306,
        user: "root".to_string(),
        password: String::new(),
        ..ServerOptions::default()
    }
}

fn refusal(path: &std::path::Path, options: &ServerOptions) -> String {
    match Server::bind(path, options) {
        Ok(_) => panic!("this configuration must not reach a socket"),
        Err(error) => error.to_string(),
    }
}

/// C1 — an empty password on an address other machines can reach.
///
/// First of the four on purpose: it is the loudest fact about the
/// configuration, and an operator who reads "no certificate" first goes and
/// gets a certificate for a database anybody can already log into.
#[test]
fn an_empty_password_on_a_network_address_is_refused_and_the_message_says_so() {
    let fixture = Fixture::new("c1");
    let message = refusal(&fixture.database(), &options(OFF_LOOPBACK));

    assert_eq!(
        message,
        "refusing to start: --bind 192.168.1.10 is reachable from other machines and the \
         account `root` has an EMPTY password, so any host that can reach port 3306 can read \
         and write this database. Set one with --password-env, or drop --bind to stay on \
         127.0.0.1."
    );
}

/// C1 catches a `--reset-superuser` to an empty password too, which C2 cannot:
/// that database *has* an account store, so nothing else here would fire.
#[test]
fn a_reset_to_an_empty_password_is_refused_the_same_way() {
    let fixture = Fixture::new("c1-reset");
    let path = fixture.with_accounts();
    let message = refusal(
        &path,
        &ServerOptions {
            user: "admin".to_string(),
            reset_superuser: true,
            ..options(OFF_LOOPBACK)
        },
    );

    assert!(
        message.contains("`admin` has an EMPTY password"),
        "{message}"
    );
    assert!(message.contains("--password-env"), "{message}");
}

/// C2 — the flags are the whole account model.
///
/// The remedy it names has to be performable without a server, which is what
/// `inlaysql user add` is for; `wire.rs` pins that it works.
#[test]
fn a_database_with_no_accounts_is_refused_and_told_how_to_get_one() {
    let fixture = Fixture::new("c2");
    let message = refusal(
        &fixture.database(),
        &ServerOptions {
            password: "s3cret".to_string(),
            ..options(OFF_LOOPBACK)
        },
    );

    assert_eq!(
        message,
        "refusing to start: --bind 192.168.1.10 is reachable from other machines and this \
         database has no accounts of its own, so `root` from --user/--password is the whole \
         credential and a forgotten flag on any restart is a way back in. Run `inlaysql user \
         add <database>` once, then restart with --bind. Or drop --bind to stay on 127.0.0.1."
    );
}

/// C3 — no certificate. The remedy is to get one.
#[test]
fn a_network_bind_without_a_certificate_is_refused_and_named_as_plaintext() {
    let fixture = Fixture::new("c3");
    let path = fixture.with_accounts();
    let message = refusal(
        &path,
        &ServerOptions {
            password: "s3cret".to_string(),
            ..options(OFF_LOOPBACK)
        },
    );

    assert_eq!(
        message,
        "refusing to start: --bind 192.168.1.10 is reachable from other machines and no \
         certificate is configured, so every statement, result and credential would cross the \
         network in the clear. Serve it with --tls-cert <pem> --tls-key <pem> --tls-required. \
         Drop --bind to stay on 127.0.0.1."
    );
}

/// C4 — a certificate that is only *available*. The remedy is one more flag,
/// and it is a different sentence for that reason: an operator sent to buy a
/// certificate they already have has been sent to the wrong place.
#[test]
fn a_certificate_that_is_not_required_is_refused_with_its_own_remedy() {
    let fixture = Fixture::new("c4");
    let path = fixture.with_accounts();
    let (cert, key) = fixture.certificate();
    let message = refusal(
        &path,
        &ServerOptions {
            password: "s3cret".to_string(),
            tls_cert: Some(cert),
            tls_key: Some(key),
            ..options(OFF_LOOPBACK)
        },
    );

    assert_eq!(
        message,
        "refusing to start: --bind 192.168.1.10 is reachable from other machines and TLS is \
         available but NOT required, so a client that does not ask for it still sends its \
         credential in the clear and an on-path attacker need only decline to offer it. Add \
         --tls-required. Drop --bind to stay on 127.0.0.1."
    );
}

/// All four satisfied: real accounts, a certificate, TLS required. The bind
/// then fails only because this machine does not have `192.168.1.10` — which
/// is exactly the failure `TcpListener::bind` gives, and not a refusal of
/// ours. A test that cannot bind a public address any other way still gets to
/// assert the important half: the conditions stopped applying.
#[test]
fn a_required_certificate_and_real_accounts_get_past_every_condition() {
    let fixture = Fixture::new("allowed");
    let path = fixture.with_accounts();
    let (cert, key) = fixture.certificate();
    let message = refusal(
        &path,
        &ServerOptions {
            password: "s3cret".to_string(),
            tls_cert: Some(cert),
            tls_key: Some(key),
            tls_required: true,
            ..options(OFF_LOOPBACK)
        },
    );

    assert!(
        !message.contains("refusing to start"),
        "this configuration is the one F3 exists to make possible: {message}"
    );
}

/// The default path is untouched, and that is the whole design: none of the
/// four conditions look at anything until the bind reaches another machine.
/// An empty password and no account store on `127.0.0.1` is what every
/// existing `serve --mysql` is, and it keeps starting.
#[test]
fn loopback_is_refused_nothing() {
    let fixture = Fixture::new("loopback");
    let server = Server::bind(
        fixture.database(),
        &ServerOptions {
            port: 0,
            ..options("127.0.0.1")
        },
    )
    .expect("the default posture must keep starting");
    drop(server);
}

/// A wildcard is not a container hint. It is every interface, including
/// whichever public one this host has, and it is the single most common way a
/// database ends up on the internet by accident.
#[test]
fn the_wildcards_are_treated_as_reaching_the_network() {
    for wildcard in ["0.0.0.0", "::"] {
        let fixture = Fixture::new(&format!("wildcard-{}", wildcard.len()));
        let message = refusal(&fixture.database(), &options(wildcard));
        assert!(
            message.starts_with(&format!(
                "refusing to start: --bind {wildcard} is reachable"
            )),
            "{message}"
        );
    }
}
