//! The merge gate: a client that speaks the MySQL wire protocol, against a
//! real server on a real socket.
//!
//! Everything here goes over TCP to a listener on an **ephemeral port** — the
//! server is asked for port 0 and told what it got — so several of these can
//! run at once, in a container or beside another agent's build, without
//! colliding on 3306. Nothing needs Docker, and nothing needs the network
//! beyond loopback.
//!
//! # Why the client is written out longhand
//!
//! Reusing the server's own framing to test the server's framing would prove
//! only that it agrees with itself. The reader below is written from the
//! protocol's description instead: packet headers, length-encoded integers,
//! result-set framing and the binary row layout are all decoded here
//! independently, so a test passes when the bytes are right rather than when
//! two copies of the same mistake line up.
//!
//! The one exception is SHA-1, which is the same algorithm on both sides by
//! definition. The library's copy is checked against the published RFC 3174
//! vectors in its own unit tests; what the copy here proves is the *wiring* —
//! that the challenge is read from the right offsets and the token is sent in
//! the right field.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use inlaysql_server::{Server, ServerOptions};

// =====================================================================
// harness
// =====================================================================

/// A database file that removes itself when the test ends.
struct TempDb {
    path: std::path::PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "inlaysql-wire-{name}-{}-{unique}.inlay",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A running server on a port the operating system chose.
struct TestServer {
    addr: SocketAddr,
    password: String,
    _temp: TempDb,
}

impl TestServer {
    fn start(name: &str) -> Self {
        Self::start_with(name, "s3cret", 16)
    }

    fn start_with(name: &str, password: &str, max_connections: usize) -> Self {
        Self::start_tuned(name, password, |options| {
            options.max_connections = max_connections;
        })
    }

    /// A server on loopback and an ephemeral port, with `tune` deciding
    /// whatever else the test is actually about. Everything not touched there
    /// is [`ServerOptions::default`], so a test reads as the one option it
    /// exercises rather than as a full option list.
    fn start_tuned(name: &str, password: &str, tune: impl FnOnce(&mut ServerOptions)) -> Self {
        let temp = TempDb::new(name);
        let mut options = ServerOptions {
            bind: "127.0.0.1".to_string(),
            // Port 0: the OS picks a free one, so nothing here assumes 3306 is
            // available or that this is the only server running.
            port: 0,
            user: "root".to_string(),
            password: password.to_string(),
            ..ServerOptions::default()
        };
        tune(&mut options);
        let server = Server::bind(&temp.path, &options).expect("bind");
        let addr = server.local_addr().expect("local_addr");
        std::thread::spawn(move || {
            let _ = server.run();
        });
        Self {
            addr,
            password: password.to_string(),
            _temp: temp,
        }
    }

    fn client(&self) -> Client {
        Client::connect(self.addr, "root", &self.password, None).expect("connect")
    }

    /// A client for some account other than the bootstrap one.
    fn client_as(&self, user: &str, password: &str) -> Client {
        Client::connect(self.addr, user, password, None)
            .unwrap_or_else(|error| panic!("{user} could not connect: {error:?}"))
    }

    /// The same, for a test whose subject is the login being refused.
    fn try_client_as(&self, user: &str, password: &str) -> Result<Client, ServerError> {
        Client::connect(self.addr, user, password, None)
    }

    /// A second server on the same database file, as an operator restarting
    /// one would get.
    ///
    /// Every connection opens its own [`Database`] handle (decision D2), so
    /// this really does re-read the account store off disk rather than sharing
    /// anything with the first server — which is the whole point of the tests
    /// that use it. The first server keeps running: there is no shutdown API,
    /// and it holds this process's advisory lock on the file, which a second
    /// handle *in the same process* shares by design.
    fn reopened(&self) -> SocketAddr {
        let options = ServerOptions {
            bind: "127.0.0.1".to_string(),
            port: 0,
            user: "root".to_string(),
            password: self.password.clone(),
            ..ServerOptions::default()
        };
        let server = Server::bind(self.path(), &options).expect("re-bind");
        let addr = server.local_addr().expect("local_addr");
        std::thread::spawn(move || {
            let _ = server.run();
        });
        addr
    }

    /// A client, retrying while the server still refuses at the connection
    /// cap. For a test whose subject is a slot being *released*: a thread ends
    /// when it ends, and polling for that is honest where sleeping a fixed
    /// interval and hoping is not.
    fn client_within(&self, timeout: Duration) -> Client {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match Client::connect(self.addr, "root", &self.password, None) {
                Ok(client) => return client,
                Err(error) if std::time::Instant::now() < deadline => {
                    assert_eq!(error.code, 1040, "refused, but not at the cap: {error:?}");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("no slot came back within {timeout:?}: {error:?}"),
            }
        }
    }

    /// The database file, for a test that asserts on what the server wrote
    /// rather than on what it answered.
    fn path(&self) -> &std::path::Path {
        &self._temp.path
    }
}

// =====================================================================
// a MySQL client, decoded from the protocol description
// =====================================================================

#[derive(Debug, PartialEq)]
enum Reply {
    Ok {
        affected: u64,
        last_insert_id: u64,
        /// The OK packet's warning count. Not decoration: it is how the server
        /// says "this succeeded, but not exactly as written".
        warnings: u16,
    },
    Rows(Rows),
}

#[derive(Debug, PartialEq, Default)]
struct Rows {
    columns: Vec<String>,
    types: Vec<u8>,
    rows: Vec<Vec<Option<String>>>,
}

impl Rows {
    /// One column's values, by header name.
    fn column(&self, name: &str) -> Vec<String> {
        let at = self
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("no column {name} in {:?}", self.columns));
        self.rows
            .iter()
            .map(|row| row[at].clone().unwrap_or_else(|| "NULL".to_string()))
            .collect()
    }

    fn cell(&self, row: usize, column: usize) -> String {
        self.rows[row][column]
            .clone()
            .unwrap_or_else(|| "NULL".to_string())
    }
}

#[derive(Debug, PartialEq)]
struct ServerError {
    code: u16,
    sqlstate: String,
    message: String,
}

impl Reply {
    fn rows(self) -> Rows {
        match self {
            Reply::Rows(rows) => rows,
            other => panic!("expected a result set, got {other:?}"),
        }
    }

    fn ok(self) -> (u64, u64) {
        match self {
            Reply::Ok {
                affected,
                last_insert_id,
                ..
            } => (affected, last_insert_id),
            other => panic!("expected an OK packet, got {other:?}"),
        }
    }

    fn warnings(&self) -> u16 {
        match self {
            Reply::Ok { warnings, .. } => *warnings,
            other => panic!("expected an OK packet, got {other:?}"),
        }
    }
}

/// A bound parameter for a prepared statement.
#[derive(Debug, Clone)]
enum Param {
    Int(i64),
    Str(String),
    /// A length-encoded payload under a type code the test chooses.
    ///
    /// An embedding has no type code of its own — see `decode_vector_param` —
    /// so the tests that bind one have to say which of the string codes a
    /// driver would have used, and the tests that bind one *wrongly* have to be
    /// able to name a code that is not a string at all.
    Bytes {
        ty: u8,
        bytes: Vec<u8>,
    },
    Null,
}

impl Param {
    /// An embedding packed the way a driver sends it: little-endian `f32`,
    /// tagged `MYSQL_TYPE_STRING`, which is what `mysql-connector-python` puts
    /// on a Python `bytes` value.
    fn vector(components: &[f32]) -> Self {
        let mut bytes = Vec::with_capacity(components.len() * 4);
        for value in components {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Param::Bytes { ty: 0xfe, bytes }
    }
}

/// `Debug` only so that `Result<Client, ServerError>::expect_err` compiles in
/// the tests whose subject is a login being refused. It prints nothing about
/// the connection: there is a credential behind every one of these.
#[derive(Debug)]
struct Client {
    stream: TcpStream,
    sequence: u8,
}

impl Client {
    fn connect(
        addr: SocketAddr,
        user: &str,
        password: &str,
        database: Option<&str>,
    ) -> Result<Self, ServerError> {
        let stream = TcpStream::connect(addr).expect("tcp connect");
        stream.set_nodelay(true).ok();
        let mut client = Self {
            stream,
            sequence: 0,
        };

        let greeting = client.read_packet().expect("handshake");
        // A server that will not serve this connection at all says so instead
        // of greeting it — "too many connections" arrives here, before any
        // handshake, and a client has to be ready for that.
        if greeting.first() == Some(&0xff) {
            return Err(parse_pre_handshake_error(&greeting));
        }
        let challenge = parse_handshake(&greeting);

        // CLIENT_LONG_PASSWORD | CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION
        // | CLIENT_PLUGIN_AUTH, plus CLIENT_CONNECT_WITH_DB when a schema is
        // named in the handshake.
        let mut capabilities: u32 = 0x0000_0001 | 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
        if database.is_some() {
            capabilities |= 0x0000_0008;
        }

        let mut payload = capabilities.to_le_bytes().to_vec();
        payload.extend_from_slice(&(16u32 * 1024 * 1024).to_le_bytes());
        payload.push(45); // utf8mb4
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(user.as_bytes());
        payload.push(0);

        let token = native_password_token(password, &challenge);
        payload.push(token.len() as u8);
        payload.extend_from_slice(&token);
        if let Some(name) = database {
            payload.extend_from_slice(name.as_bytes());
            payload.push(0);
        }
        payload.extend_from_slice(b"mysql_native_password\0");

        client.write_packet(&payload).expect("handshake response");
        match client.read_packet() {
            Ok(reply) if reply.first() == Some(&0xff) => Err(parse_error(&reply)),
            Ok(_) => Ok(client),
            Err(error) => panic!("no reply to the handshake: {error}"),
        }
    }

    /// Send a bad token on purpose, to exercise the rejection path.
    fn connect_with_bad_token(addr: SocketAddr, user: &str) -> Result<Self, ServerError> {
        let stream = TcpStream::connect(addr).expect("tcp connect");
        let mut client = Self {
            stream,
            sequence: 0,
        };
        let greeting = client.read_packet().expect("handshake");
        if greeting.first() == Some(&0xff) {
            return Err(parse_pre_handshake_error(&greeting));
        }
        let _ = parse_handshake(&greeting);

        let capabilities: u32 = 0x0000_0001 | 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
        let mut payload = capabilities.to_le_bytes().to_vec();
        payload.extend_from_slice(&(16u32 * 1024 * 1024).to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(user.as_bytes());
        payload.push(0);
        payload.push(20);
        payload.extend_from_slice(&[0x41u8; 20]);
        payload.extend_from_slice(b"mysql_native_password\0");

        client.write_packet(&payload).expect("handshake response");
        match client.read_packet() {
            Ok(reply) if reply.first() == Some(&0xff) => Err(parse_error(&reply)),
            Ok(_) => Ok(client),
            Err(error) => panic!("no reply to the handshake: {error}"),
        }
    }

    // ---------------------------------------------------- AHL-467: caching_sha2

    /// A basic handshake response, up to the auth-response field, shared by
    /// every connect variant below.
    fn handshake_response_prefix(user: &str) -> Vec<u8> {
        let capabilities: u32 = 0x0000_0001 | 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
        let mut payload = capabilities.to_le_bytes().to_vec();
        payload.extend_from_slice(&(16u32 * 1024 * 1024).to_le_bytes());
        payload.push(45); // utf8mb4
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(user.as_bytes());
        payload.push(0);
        payload
    }

    /// Connect as a `caching_sha2_password` client would: send the fast
    /// scramble as the very first response, the way every real client does,
    /// hoping the server has something to check it against — which this one
    /// always does, so a correct password completes in one round trip.
    fn connect_caching_sha2(
        addr: SocketAddr,
        user: &str,
        password: &str,
    ) -> Result<Self, ServerError> {
        let stream = TcpStream::connect(addr).expect("tcp connect");
        stream.set_nodelay(true).ok();
        let mut client = Self {
            stream,
            sequence: 0,
        };
        let greeting = client.read_packet().expect("handshake");
        if greeting.first() == Some(&0xff) {
            return Err(parse_pre_handshake_error(&greeting));
        }
        let challenge = parse_handshake(&greeting);

        let mut payload = Self::handshake_response_prefix(user);
        let token = caching_sha2_token(password, &challenge);
        payload.push(token.len() as u8);
        payload.extend_from_slice(&token);
        payload.extend_from_slice(b"caching_sha2_password\0");

        client.write_packet(&payload).expect("handshake response");
        client.finish_authentication()?;
        Ok(client)
    }

    /// Connect with an empty initial response — a client with nothing to
    /// attempt fast-auth with — which forces `caching_sha2_password`'s full
    /// authentication: the server asks for the cleartext password, and this
    /// sends it, NUL-terminated, over the same plaintext connection the rest
    /// of v1 already is.
    fn connect_caching_sha2_full_auth(
        addr: SocketAddr,
        user: &str,
        password: &str,
    ) -> Result<Self, ServerError> {
        let stream = TcpStream::connect(addr).expect("tcp connect");
        stream.set_nodelay(true).ok();
        let mut client = Self {
            stream,
            sequence: 0,
        };
        let greeting = client.read_packet().expect("handshake");
        if greeting.first() == Some(&0xff) {
            return Err(parse_pre_handshake_error(&greeting));
        }
        let _ = parse_handshake(&greeting);

        let mut payload = Self::handshake_response_prefix(user);
        payload.push(0); // no fast-auth attempt: an empty auth-response
        payload.extend_from_slice(b"caching_sha2_password\0");
        client.write_packet(&payload).expect("handshake response");

        let more = client.read_packet().expect("perform_full_authentication");
        assert_eq!(
            more,
            vec![0x01, 0x04],
            "expected AuthMoreData(perform_full_authentication)"
        );

        let mut cleartext = password.as_bytes().to_vec();
        cleartext.push(0);
        client.write_packet(&cleartext).expect("cleartext password");

        client.finish_authentication()?;
        Ok(client)
    }

    /// Reach `caching_sha2_password`'s full-authentication step and then ask
    /// for the server's RSA public key instead of sending a cleartext
    /// password — the request this server has no RSA implementation behind
    /// and refuses outright.
    fn connect_caching_sha2_requesting_rsa(
        addr: SocketAddr,
        user: &str,
    ) -> Result<Self, ServerError> {
        let stream = TcpStream::connect(addr).expect("tcp connect");
        stream.set_nodelay(true).ok();
        let mut client = Self {
            stream,
            sequence: 0,
        };
        let greeting = client.read_packet().expect("handshake");
        if greeting.first() == Some(&0xff) {
            return Err(parse_pre_handshake_error(&greeting));
        }
        let _ = parse_handshake(&greeting);

        let mut payload = Self::handshake_response_prefix(user);
        payload.push(0);
        payload.extend_from_slice(b"caching_sha2_password\0");
        client.write_packet(&payload).expect("handshake response");

        let more = client.read_packet().expect("perform_full_authentication");
        assert_eq!(more, vec![0x01, 0x04]);

        client.write_packet(&[0x02]).expect("request public key"); // CACHING_SHA2_REQUEST_PUBLIC_KEY

        match client.read_packet() {
            Ok(reply) if reply.first() == Some(&0xff) => Err(parse_error(&reply)),
            Ok(reply) => panic!("expected the RSA request to be refused, got {reply:?}"),
            Err(error) => panic!("no reply to the RSA request: {error}"),
        }
    }

    /// Connect offering a plugin this server does not speak directly, which
    /// must force `AuthSwitchRequest` onto `mysql_native_password` — the
    /// path a client whose own default is some other plugin name takes.
    fn connect_via_auth_switch(
        addr: SocketAddr,
        user: &str,
        password: &str,
    ) -> Result<Self, ServerError> {
        let stream = TcpStream::connect(addr).expect("tcp connect");
        stream.set_nodelay(true).ok();
        let mut client = Self {
            stream,
            sequence: 0,
        };
        let greeting = client.read_packet().expect("handshake");
        if greeting.first() == Some(&0xff) {
            return Err(parse_pre_handshake_error(&greeting));
        }
        let _ = parse_handshake(&greeting);

        let mut payload = Self::handshake_response_prefix(user);
        // A token computed for a plugin the server does not implement: it
        // must be discarded, not checked, once the server asks to switch.
        payload.push(4);
        payload.extend_from_slice(b"\0\0\0\0");
        payload.extend_from_slice(b"sha256_password\0");
        client.write_packet(&payload).expect("handshake response");

        let switch = client.read_packet().expect("AuthSwitchRequest");
        assert_eq!(switch.first(), Some(&0xfe), "expected AuthSwitchRequest");
        let name_end = switch[1..]
            .iter()
            .position(|&b| b == 0)
            .expect("NUL-terminated plugin name")
            + 1;
        assert_eq!(
            &switch[1..name_end],
            b"mysql_native_password",
            "the switch must offer the plugin this server actually completes"
        );
        // The challenge is 20 bytes, fixed by the protocol.
        let new_challenge = switch[name_end + 1..name_end + 1 + 20].to_vec();

        let token = native_password_token(password, &new_challenge);
        client.write_packet(&token).expect("switched token");

        client.finish_authentication()?;
        Ok(client)
    }

    /// Read packets until the authentication exchange either succeeds (any
    /// packet that is not `ERR` and not `AuthMoreData`) or fails (`ERR`).
    /// `AuthMoreData` — `caching_sha2_password`'s `fast_auth_success` — is
    /// skipped over rather than treated as the final reply, unlike
    /// [`Self::connect`]'s single-packet read, which never sees one because
    /// it always names `mysql_native_password` explicitly.
    fn finish_authentication(&mut self) -> Result<(), ServerError> {
        loop {
            let packet = self.read_packet().expect("auth reply");
            match packet.first() {
                Some(0xff) => return Err(parse_error(&packet)),
                Some(0x01) => continue,
                _ => return Ok(()),
            }
        }
    }

    // ---------------------------------------------------------- framing

    fn read_packet(&mut self) -> io::Result<Vec<u8>> {
        self.read_framed().map(|(_, payload)| payload)
    }

    /// One whole message, both as the bytes that carried it — headers,
    /// sequence ids, continuation packets and all — and as the payload they
    /// reassemble to.
    ///
    /// The raw half exists for the tests that compare two answers *as bytes*
    /// rather than as decoded rows: a column definition's charset, flags and
    /// declared length are on the wire and are not in [`Rows`], so a change to
    /// one of them would be invisible to every other test in this file.
    fn read_framed(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let mut raw = Vec::new();
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header)?;
            raw.extend_from_slice(&header);
            let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
            self.sequence = header[3].wrapping_add(1);
            let start = payload.len();
            payload.resize(start + length, 0);
            self.stream.read_exact(&mut payload[start..])?;
            raw.extend_from_slice(&payload[start..]);
            if length < 0xff_ff_ff {
                return Ok((raw, payload));
            }
        }
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

    fn command(&mut self, byte: u8, body: &[u8]) {
        // Every command starts a fresh exchange, numbered from zero.
        self.sequence = 0;
        let mut payload = vec![byte];
        payload.extend_from_slice(body);
        self.write_packet(&payload).expect("write command");
    }

    // --------------------------------------------------------- commands

    fn query(&mut self, sql: &str) -> Result<Reply, ServerError> {
        self.command(0x03, sql.as_bytes());
        self.read_reply(false)
    }

    /// A query that is expected to succeed.
    fn ok_query(&mut self, sql: &str) -> Reply {
        self.query(sql)
            .unwrap_or_else(|error| panic!("{sql} failed: {error:?}"))
    }

    fn ping(&mut self) -> Result<Reply, ServerError> {
        self.command(0x0e, &[]);
        self.read_reply(false)
    }

    fn init_db(&mut self, name: &str) -> Result<Reply, ServerError> {
        self.command(0x02, name.as_bytes());
        self.read_reply(false)
    }

    fn quit(&mut self) {
        self.command(0x01, &[]);
    }

    /// Whether the server is still there, asked with a `COM_PING`.
    ///
    /// Every other command here `expect`s its I/O, which is right when the
    /// subject is the answer; this is for the tests whose subject is the
    /// connection *ending*, where a killed connection answers nothing at all
    /// because its socket has been shut down under it.
    fn still_connected(&mut self) -> bool {
        self.sequence = 0;
        self.write_packet(&[0x0e]).is_ok() && self.read_packet().is_ok()
    }

    fn prepare(&mut self, sql: &str) -> Result<Prepared, ServerError> {
        self.command(0x16, sql.as_bytes());
        let packet = self.read_packet().expect("prepare reply");
        if packet.first() == Some(&0xff) {
            return Err(parse_error(&packet));
        }
        let id = u32::from_le_bytes([packet[1], packet[2], packet[3], packet[4]]);
        let columns = u16::from_le_bytes([packet[5], packet[6]]);
        let params = u16::from_le_bytes([packet[7], packet[8]]);

        let mut param_defs = Vec::with_capacity(params as usize);
        if params > 0 {
            for _ in 0..params {
                let packet = self.read_packet().expect("param def");
                param_defs.push(parse_column_definition(&packet));
            }
            self.read_packet().expect("param EOF");
        }
        let mut column_defs = Vec::with_capacity(columns as usize);
        if columns > 0 {
            for _ in 0..columns {
                let packet = self.read_packet().expect("column def");
                column_defs.push(parse_column_definition(&packet));
            }
            self.read_packet().expect("column EOF");
        }
        Ok(Prepared {
            id,
            param_count: params as usize,
            column_count: columns as usize,
            columns: column_defs,
            params: param_defs,
        })
    }

    fn execute(&mut self, stmt: &Prepared, params: &[Param]) -> Result<Reply, ServerError> {
        assert_eq!(
            params.len(),
            stmt.param_count,
            "the server asked for {} parameters",
            stmt.param_count
        );
        let mut body = stmt.id.to_le_bytes().to_vec();
        body.push(0); // flags: no cursor
        body.extend_from_slice(&1u32.to_le_bytes()); // iteration count

        if !params.is_empty() {
            let mut bitmap = vec![0u8; params.len().div_ceil(8)];
            for (index, param) in params.iter().enumerate() {
                if matches!(param, Param::Null) {
                    bitmap[index / 8] |= 1 << (index % 8);
                }
            }
            body.extend_from_slice(&bitmap);
            body.push(1); // types follow

            for param in params {
                match param {
                    Param::Int(_) => body.extend_from_slice(&[0x08, 0]),
                    Param::Str(_) => body.extend_from_slice(&[0xfe, 0]),
                    Param::Bytes { ty, .. } => body.extend_from_slice(&[*ty, 0]),
                    Param::Null => body.extend_from_slice(&[0x06, 0]),
                }
            }
            for param in params {
                match param {
                    Param::Int(value) => body.extend_from_slice(&value.to_le_bytes()),
                    Param::Str(value) => put_lenenc_bytes(&mut body, value.as_bytes()),
                    // Length-encoded for the string codes; written raw for
                    // anything else, so a test can bind an embedding under a
                    // fixed-width numeric code and see it refused.
                    Param::Bytes { ty, bytes } => {
                        if matches!(ty, 0x0f | 0xf9..=0xfc | 0xfd | 0xfe) {
                            put_lenenc_bytes(&mut body, bytes);
                        } else {
                            body.extend_from_slice(bytes);
                        }
                    }
                    Param::Null => {}
                }
            }
        }

        self.command(0x17, &body);
        self.read_reply(true)
    }

    fn close_statement(&mut self, stmt: &Prepared) {
        self.command(0x19, &stmt.id.to_le_bytes());
        // COM_STMT_CLOSE has no reply at all; a ping proves the connection is
        // still in step rather than one packet behind.
    }

    fn reset_statement(&mut self, stmt: &Prepared) -> Result<Reply, ServerError> {
        self.command(0x1a, &stmt.id.to_le_bytes());
        self.read_reply(false)
    }

    // ---------------------------------------------------------- replies

    fn read_reply(&mut self, binary: bool) -> Result<Reply, ServerError> {
        let first = self.read_packet().expect("reply");
        match first.first() {
            Some(0xff) => return Err(parse_error(&first)),
            // An OK packet is 0x00 with at least the two counters after it; a
            // result set starting with a column count of 0 is not legal, so
            // there is no ambiguity.
            Some(0x00) if first.len() >= 7 => {
                let mut cursor = Cursor::new(&first[1..]);
                let affected = cursor.lenenc().expect("affected_rows");
                let last_insert_id = cursor.lenenc().expect("last_insert_id");
                let _status = cursor.u16().expect("status flags");
                let warnings = cursor.u16().expect("warning count");
                return Ok(Reply::Ok {
                    affected,
                    last_insert_id,
                    warnings,
                });
            }
            _ => {}
        }

        let count = Cursor::new(&first).lenenc().expect("column count") as usize;
        let mut columns = Vec::with_capacity(count);
        let mut types = Vec::with_capacity(count);
        for _ in 0..count {
            let packet = self.read_packet().expect("column definition");
            let (name, ty) = parse_column_definition(&packet);
            columns.push(name);
            types.push(ty);
        }
        let eof = self.read_packet().expect("metadata EOF");
        assert_eq!(eof.first(), Some(&0xfe), "expected EOF after column defs");
        assert!(eof.len() < 9, "EOF packets are short");

        let mut rows = Vec::new();
        loop {
            let packet = self.read_packet().expect("row");
            if packet.first() == Some(&0xfe) && packet.len() < 9 {
                break;
            }
            rows.push(if binary {
                parse_binary_row(&packet, &types)
            } else {
                parse_text_row(&packet)
            });
        }
        Ok(Reply::Rows(Rows {
            columns,
            types,
            rows,
        }))
    }

    /// Every byte the server sent in answer to the command just written.
    ///
    /// Stops at whichever packet ends the exchange: an ERR, an OK, or a result
    /// set's terminating EOF — or an ERR *in place of* that EOF, which is how
    /// a result set that failed after its first row ends.
    fn raw_reply(&mut self) -> Vec<u8> {
        let mut raw = Vec::new();
        let (bytes, first) = self.read_framed().expect("reply");
        raw.extend_from_slice(&bytes);
        match first.first() {
            Some(0xff) => return raw,
            Some(0x00) if first.len() >= 7 => return raw,
            _ => {}
        }

        let count = Cursor::new(&first).lenenc().expect("column count") as usize;
        for _ in 0..=count {
            let (bytes, _) = self.read_framed().expect("column definition");
            raw.extend_from_slice(&bytes);
        }
        loop {
            let (bytes, payload) = self.read_framed().expect("row");
            raw.extend_from_slice(&bytes);
            let terminal = payload.first() == Some(&0xff)
                || (payload.first() == Some(&0xfe) && payload.len() < 9);
            if terminal {
                return raw;
            }
        }
    }

    /// [`Self::query`], answered as bytes rather than as rows.
    fn raw_query(&mut self, sql: &str) -> Vec<u8> {
        self.command(0x03, sql.as_bytes());
        self.raw_reply()
    }

    /// [`Self::execute`] with no parameters, answered as bytes.
    fn raw_execute(&mut self, stmt: &Prepared) -> Vec<u8> {
        assert_eq!(stmt.param_count, 0, "this helper binds nothing");
        let mut body = stmt.id.to_le_bytes().to_vec();
        body.push(0); // flags: no cursor
        body.extend_from_slice(&1u32.to_le_bytes()); // iteration count
        self.command(0x17, &body);
        self.raw_reply()
    }

    /// Run a query that is expected to fail *after* it has already sent rows,
    /// and report both halves: the rows that arrived, and the error that ended
    /// the result set in place of its final EOF.
    fn query_until_error(&mut self, sql: &str) -> (Vec<Vec<Option<String>>>, ServerError) {
        self.command(0x03, sql.as_bytes());
        let first = self.read_packet().expect("reply");
        assert!(
            first.first() != Some(&0xff),
            "this statement failed before any row: {:?}",
            parse_error(&first)
        );
        let count = Cursor::new(&first).lenenc().expect("column count") as usize;
        for _ in 0..count {
            self.read_packet().expect("column definition");
        }
        self.read_packet().expect("metadata EOF");

        let mut rows = Vec::new();
        loop {
            let packet = self.read_packet().expect("row");
            if packet.first() == Some(&0xff) {
                return (rows, parse_error(&packet));
            }
            assert!(
                !(packet.first() == Some(&0xfe) && packet.len() < 9),
                "the result set ended cleanly; it was expected to fail"
            );
            rows.push(parse_text_row(&packet));
        }
    }

    /// Run a query and count its rows without keeping any of them, for a
    /// result set large enough that keeping them would be the test's own
    /// memory problem rather than the server's.
    fn count_rows(&mut self, sql: &str) -> usize {
        self.command(0x03, sql.as_bytes());
        let first = self.read_packet().expect("reply");
        assert!(
            first.first() != Some(&0xff),
            "{sql} failed: {:?}",
            parse_error(&first)
        );
        let count = Cursor::new(&first).lenenc().expect("column count") as usize;
        for _ in 0..=count {
            self.read_packet().expect("column definition");
        }
        let mut rows = 0usize;
        loop {
            let packet = self.read_packet().expect("row");
            if packet.first() == Some(&0xfe) && packet.len() < 9 {
                return rows;
            }
            assert_ne!(packet.first(), Some(&0xff), "the result set failed midway");
            rows += 1;
        }
    }
}

#[derive(Debug)]
struct Prepared {
    id: u32,
    param_count: usize,
    column_count: usize,
    /// `(name, wire type)` for each column `COM_STMT_PREPARE_OK` reported —
    /// empty when `column_count` is `0`. See AHL-466.
    columns: Vec<(String, u8)>,
    /// `(name, wire type)` for each *parameter* definition, in `?` order. The
    /// server describes an embedding slot as a binary string rather than as
    /// text, which is the only place the reply says a parameter is special.
    params: Vec<(String, u8)>,
}

// --------------------------------------------------------------- decoding

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn u16(&mut self) -> Option<u16> {
        let bytes = self.take(2)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.at..self.at + n)?;
        self.at += n;
        Some(slice)
    }

    fn lenenc(&mut self) -> Option<u64> {
        match self.u8()? {
            0xfb => None,
            0xfc => {
                let b = self.take(2)?;
                Some(u16::from_le_bytes([b[0], b[1]]) as u64)
            }
            0xfd => {
                let b = self.take(3)?;
                Some(u32::from_le_bytes([b[0], b[1], b[2], 0]) as u64)
            }
            0xfe => {
                let b = self.take(8)?;
                Some(u64::from_le_bytes(b.try_into().ok()?))
            }
            small => Some(small as u64),
        }
    }

    /// A length-encoded string. `None` is the SQL NULL marker.
    fn lenenc_bytes(&mut self) -> Option<Option<Vec<u8>>> {
        if self.bytes.get(self.at) == Some(&0xfb) {
            self.at += 1;
            return Some(None);
        }
        let length = self.lenenc()? as usize;
        Some(Some(self.take(length)?.to_vec()))
    }
}

fn put_lenenc_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let length = bytes.len();
    if length < 0xfb {
        out.push(length as u8);
    } else {
        out.push(0xfc);
        out.extend_from_slice(&(length as u16).to_le_bytes());
    }
    out.extend_from_slice(bytes);
}

/// Parses an ERR packet that follows a successful handshake exchange, where
/// `CLIENT_PROTOCOL_41` has been negotiated and the SQLSTATE marker is always
/// present.
fn parse_error(packet: &[u8]) -> ServerError {
    let code = u16::from_le_bytes([packet[1], packet[2]]);
    assert_eq!(packet[3], b'#', "a 4.1 error carries a SQLSTATE marker");
    let sqlstate = String::from_utf8_lossy(&packet[4..9]).to_string();
    ServerError {
        code,
        sqlstate,
        message: String::from_utf8_lossy(&packet[9..]).to_string(),
    }
}

/// Parses an ERR packet that arrives **before** any handshake — the only
/// packet shape this server ever sends in that position (`--max-connections`,
/// or the database file itself failing to open). Nothing has negotiated
/// `CLIENT_PROTOCOL_41` yet at that point in the exchange, so — unlike
/// [`parse_error`] — this packet carries no SQLSTATE marker at all: just the
/// error code and the message, back to back.
fn parse_pre_handshake_error(packet: &[u8]) -> ServerError {
    let code = u16::from_le_bytes([packet[1], packet[2]]);
    ServerError {
        code,
        sqlstate: String::new(),
        message: String::from_utf8_lossy(&packet[3..]).to_string(),
    }
}

/// Pull the 20-byte challenge out of the greeting, from both of the places the
/// protocol splits it across.
fn parse_handshake(packet: &[u8]) -> Vec<u8> {
    let mut cursor = Cursor::new(packet);
    assert_eq!(cursor.u8(), Some(10), "protocol version 10");
    // Server version, NUL-terminated.
    while cursor.u8() != Some(0) {}
    cursor.take(4).expect("connection id");
    let part1 = cursor.take(8).expect("challenge part 1").to_vec();
    cursor.u8().expect("filler");
    let lower = cursor.take(2).expect("capabilities lower");
    cursor.u8().expect("charset");
    cursor.take(2).expect("status flags");
    let upper = cursor.take(2).expect("capabilities upper");

    let capabilities = u16::from_le_bytes([lower[0], lower[1]]) as u32
        | ((u16::from_le_bytes([upper[0], upper[1]]) as u32) << 16);
    // The server must not be advertising TLS: v1 is plaintext, and a client
    // must not be able to negotiate an encryption it will not get.
    assert_eq!(
        capabilities & 0x0000_0800,
        0,
        "CLIENT_SSL must not be offered"
    );

    let challenge_len = cursor.u8().expect("challenge length") as usize;
    cursor.take(10).expect("reserved");
    let part2 = cursor
        .take(challenge_len.saturating_sub(8).max(13) - 1)
        .expect("challenge part 2")
        .to_vec();

    let mut challenge = part1;
    challenge.extend_from_slice(&part2[..12.min(part2.len())]);
    challenge
}

fn parse_column_definition(packet: &[u8]) -> (String, u8) {
    let mut cursor = Cursor::new(packet);
    // catalog, schema, table, org_table, name, org_name
    let mut name = Vec::new();
    for field in 0..6 {
        let value = cursor
            .lenenc_bytes()
            .expect("column def field")
            .unwrap_or_default();
        if field == 4 {
            name = value;
        }
    }
    cursor.lenenc().expect("fixed block length");
    cursor.take(2).expect("charset");
    cursor.take(4).expect("length");
    let ty = cursor.u8().expect("type");
    (String::from_utf8_lossy(&name).to_string(), ty)
}

fn parse_text_row(packet: &[u8]) -> Vec<Option<String>> {
    let mut cursor = Cursor::new(packet);
    let mut values = Vec::new();
    while cursor.at < packet.len() {
        match cursor.lenenc_bytes().expect("text row value") {
            Some(bytes) => values.push(Some(String::from_utf8_lossy(&bytes).to_string())),
            None => values.push(None),
        }
    }
    values
}

/// Decode a binary row: header byte, NULL bitmap offset by two bits, then one
/// value per column encoded as the column's declared type.
fn parse_binary_row(packet: &[u8], types: &[u8]) -> Vec<Option<String>> {
    assert_eq!(packet[0], 0x00, "a binary row starts with 0x00");
    let bitmap_len = (types.len() + 7 + 2) / 8;
    let bitmap = &packet[1..1 + bitmap_len];
    let mut cursor = Cursor::new(&packet[1 + bitmap_len..]);

    let mut values = Vec::with_capacity(types.len());
    for (index, ty) in types.iter().enumerate() {
        let is_null = bitmap[(index + 2) / 8] & (1 << ((index + 2) % 8)) != 0;
        if is_null {
            values.push(None);
            continue;
        }
        let value = match ty {
            0x08 => {
                let b = cursor.take(8).expect("longlong");
                i64::from_le_bytes(b.try_into().unwrap()).to_string()
            }
            0x05 => {
                let b = cursor.take(8).expect("double");
                let value = f64::from_le_bytes(b.try_into().unwrap());
                let rendered = format!("{value}");
                rendered.strip_suffix(".0").unwrap_or(&rendered).to_string()
            }
            _ => {
                let bytes = cursor
                    .lenenc_bytes()
                    .expect("string value")
                    .unwrap_or_default();
                String::from_utf8_lossy(&bytes).to_string()
            }
        };
        values.push(Some(value));
    }
    values
}

// ------------------------------------------------------------------- auth

/// `SHA1(password) XOR SHA1(challenge || SHA1(SHA1(password)))`.
fn native_password_token(password: &str, challenge: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = sha1(password.as_bytes());
    let stage2 = sha1(&stage1);
    let mut salted = challenge.to_vec();
    salted.extend_from_slice(&stage2);
    let scrambled = sha1(&salted);
    stage1
        .iter()
        .zip(scrambled.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

/// `caching_sha2_password`'s fast-authentication scramble:
/// `XOR(SHA256(password), SHA256(SHA256(SHA256(password)) || scramble))`.
/// Note the concatenation order — the stage-two digest *before* the
/// scramble — which is the opposite of `mysql_native_password`'s own, and
/// wrong in a way that would still pass every test in this file if it were
/// reversed, since this file's copy and the server's would still agree with
/// each other. `auth::tests::a_caching_sha2_token_matches_an_independent_implementation`
/// in `src/auth.rs` is what actually pins the order against a second,
/// independent implementation.
fn caching_sha2_token(password: &str, scramble: &[u8]) -> Vec<u8> {
    let stage1 = sha256(password.as_bytes());
    let stage2 = sha256(&stage1);
    let mut salted = stage2.to_vec();
    salted.extend_from_slice(scramble);
    let stage3 = sha256(&salted);
    stage1
        .iter()
        .zip(stage3.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

fn sha1(message: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (word, bytes) in w.iter_mut().zip(chunk.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (slot, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *slot = word.to_be_bytes();
    }
    out
}

fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().take(16).zip(chunk.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (slot, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *slot = word.to_be_bytes();
    }
    out
}

// =====================================================================
// tests
// =====================================================================

/// The whole round trip: connect, authenticate, create a table, insert, read
/// it back. If only one test in this file runs, it should be this one.
#[test]
fn a_client_connects_authenticates_and_runs_ddl_dml_and_a_select() {
    let server = TestServer::start("round-trip");
    let mut client = server.client();

    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");

    let (affected, _) = client
        .ok_query("INSERT INTO kv (id, body) VALUES (1, 'one')")
        .ok();
    assert_eq!(affected, 1);

    let rows = client.ok_query("SELECT id, body FROM kv").rows();
    assert_eq!(rows.columns, vec!["id", "body"]);
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.cell(0, 0), "1");
    assert_eq!(rows.cell(0, 1), "one");

    // The column types are inferred and sent, not left as "string for
    // everything": an integer column has to arrive as one.
    assert_eq!(rows.types[0], 0x08, "id should be MYSQL_TYPE_LONGLONG");
    assert_eq!(rows.types[1], 0xfd, "body should be MYSQL_TYPE_VAR_STRING");

    client.quit();
}

#[test]
fn affected_rows_and_last_insert_id_come_back_in_the_ok_packet() {
    let server = TestServer::start("counters");
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");

    // An INSERT that lets the engine assign the key reports it.
    let (affected, insert_id) = client.ok_query("INSERT INTO kv (body) VALUES ('a')").ok();
    assert_eq!(affected, 1);
    assert_eq!(insert_id, 1, "the generated row id must be reported");

    let (_, second) = client.ok_query("INSERT INTO kv (body) VALUES ('b')").ok();
    assert_eq!(second, 2);

    // An INSERT that supplies its own key does not set LAST_INSERT_ID, which
    // is MySQL's rule and the engine's.
    let (affected, insert_id) = client
        .ok_query("INSERT INTO kv (id, body) VALUES (100, 'c')")
        .ok();
    assert_eq!(affected, 1);
    assert_eq!(insert_id, 0, "an explicit key does not generate an id");

    // And neither does anything that is not an INSERT.
    let (affected, insert_id) = client
        .ok_query("UPDATE kv SET body = 'z' WHERE id = 1")
        .ok();
    assert_eq!(affected, 1);
    assert_eq!(insert_id, 0);

    let (affected, _) = client.ok_query("DELETE FROM kv WHERE id = 100").ok();
    assert_eq!(affected, 1);

    // `SELECT LAST_INSERT_ID()` agrees with the OK packet.
    let rows = client.ok_query("SELECT LAST_INSERT_ID()").rows();
    assert_eq!(rows.cell(0, 0), "2");
}

/// Prepared statements end to end: parameters go out in the binary protocol
/// and rows come back in it.
#[test]
fn a_prepared_statement_binds_parameters_and_returns_a_binary_result_set() {
    let server = TestServer::start("prepared");
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT, weight REAL)");

    let insert = client
        .prepare("INSERT INTO kv (id, body, weight) VALUES (?, ?, ?)")
        .expect("prepare insert");
    assert_eq!(insert.param_count, 3, "three placeholders were counted");
    assert_eq!(insert.column_count, 0, "an INSERT returns no columns");

    for (id, body) in [(1i64, "one"), (2, "two"), (3, "three")] {
        let (affected, _) = client
            .execute(
                &insert,
                &[Param::Int(id), Param::Str(body.to_string()), Param::Null],
            )
            .expect("execute insert")
            .ok();
        assert_eq!(affected, 1);
    }

    let select = client
        .prepare("SELECT id, body, weight FROM kv WHERE id = ?")
        .expect("prepare select");
    assert_eq!(select.param_count, 1);
    // AHL-466: COM_STMT_PREPARE now carries the real column shape, not the
    // zero `docs/server.md` used to record — the engine's own Statement
    // knows its projection without running it.
    assert_eq!(select.column_count, 3);
    assert_eq!(
        select.columns,
        vec![
            ("id".to_string(), 0x08),     // MYSQL_TYPE_LONGLONG
            ("body".to_string(), 0xfd),   // MYSQL_TYPE_VAR_STRING
            ("weight".to_string(), 0x05), // MYSQL_TYPE_DOUBLE
        ]
    );

    let rows = client
        .execute(&select, &[Param::Int(2)])
        .expect("execute select")
        .rows();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.cell(0, 0), "2");
    assert_eq!(rows.cell(0, 1), "two");
    assert_eq!(rows.rows[0][2], None, "an unbound column stays NULL");

    // The same statement runs again with different values — the point of
    // preparing it at all.
    let rows = client
        .execute(&select, &[Param::Int(3)])
        .expect("re-execute")
        .rows();
    assert_eq!(rows.cell(0, 1), "three");

    // A string parameter really is compared as a string.
    let by_body = client
        .prepare("SELECT id FROM kv WHERE body = ?")
        .expect("prepare");
    let rows = client
        .execute(&by_body, &[Param::Str("one".to_string())])
        .expect("execute")
        .rows();
    assert_eq!(rows.cell(0, 0), "1");

    client.reset_statement(&select).expect("reset");
    client.close_statement(&select);
    // The connection is still usable and in step after a reply-less command.
    client.ping().expect("ping after close");
}

#[test]
fn errors_arrive_as_mysql_error_codes_rather_than_a_dropped_connection() {
    let server = TestServer::start("errors");
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    client.ok_query("INSERT INTO kv (id, body) VALUES (1, 'one')");

    // A missing table.
    let error = client.query("SELECT * FROM missing").unwrap_err();
    assert_eq!(error.code, 1146, "ER_NO_SUCH_TABLE");
    assert_eq!(error.sqlstate, "42S02");

    // A duplicate primary key — the code every ORM reads as "already exists".
    let error = client
        .query("INSERT INTO kv (id, body) VALUES (1, 'again')")
        .unwrap_err();
    assert_eq!(error.code, 1062, "ER_DUP_ENTRY");
    assert_eq!(error.sqlstate, "23000");

    // Syntax the parser rejects.
    let error = client.query("SELECT FROM WHERE").unwrap_err();
    assert_eq!(error.code, 1064, "ER_PARSE_ERROR");

    // SQL that parses but is not implemented must say so, distinctly from a
    // syntax error, so a caller can tell "rewrite this" from "not yet".
    //
    // This needs *some* construct the dialect does not have yet, and the
    // dialect is deliberately growing — this line already had to move
    // several times: from `SELECT DISTINCT` when AHL-411 implemented it,
    // from `UNION` when AHL-473 implemented set operations and CTEs, from
    // `ROW_NUMBER() OVER ()` when AHL-494 implemented window functions
    // (ranking, `lag`/`lead`, the aggregate family, `ROWS` frames, named
    // windows and `FILTER`), and from `percent_rank()`/`cume_dist()` once
    // those landed too. If an explicit `RANGE` frame lands and this starts
    // failing, that is the same good news: point it at whatever is still
    // refused rather than deleting the assertion. What is being tested is
    // the mapping of `Error::Unsupported` onto 1235, not this particular
    // statement.
    let error = client
        .query(
            "SELECT sum(id) OVER (ORDER BY id RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM kv",
        )
        .unwrap_err();
    assert_eq!(error.code, 1235, "ER_NOT_SUPPORTED_YET");

    // A missing column.
    let error = client.query("SELECT nope FROM kv").unwrap_err();
    assert_eq!(error.code, 1054);

    // After all of that the connection is still alive and correct.
    let rows = client.ok_query("SELECT id FROM kv").rows();
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn the_shim_answers_the_metadata_statements_a_driver_sends() {
    let server = TestServer::start("shim");
    let mut client = server.client();
    client.ok_query("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT)");
    client.ok_query("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT)");
    client.ok_query("CREATE INDEX posts_title ON posts (title)");

    // The connection-setup statements every driver sends.
    client.ok_query("SET NAMES utf8mb4").ok();
    client
        .ok_query("SET SESSION sql_mode = 'STRICT_TRANS_TABLES'")
        .ok();
    client.ok_query("SET autocommit=1").ok();

    let rows = client.ok_query("SELECT VERSION()").rows();
    assert!(rows.cell(0, 0).contains("inlaysql"), "{}", rows.cell(0, 0));

    let rows = client.ok_query("SHOW TABLES").rows();
    assert_eq!(
        rows.column(&rows.columns[0].clone()),
        vec!["posts", "users"]
    );

    let rows = client.ok_query("SHOW FULL COLUMNS FROM users").rows();
    assert_eq!(rows.column("Field"), vec!["id", "email"]);
    assert_eq!(rows.column("Type"), vec!["bigint", "text"]);
    assert_eq!(rows.column("Key"), vec!["PRI", ""]);

    let rows = client.ok_query("SHOW KEYS FROM posts").rows();
    assert_eq!(rows.column("Key_name"), vec!["PRIMARY", "posts_title"]);

    let rows = client.ok_query("SHOW VARIABLES LIKE 'version'").rows();
    assert_eq!(rows.column("Variable_name"), vec!["version"]);

    // information_schema, filtered.
    let rows = client
        .ok_query(
            "SELECT table_name, table_type FROM information_schema.tables \
             WHERE table_name = 'users'",
        )
        .rows();
    assert_eq!(rows.column("TABLE_NAME"), vec!["users"]);
    assert_eq!(rows.column("TABLE_TYPE"), vec!["BASE TABLE"]);

    let rows = client
        .ok_query(
            "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
             WHERE table_name = 'users' ORDER BY ordinal_position",
        )
        .rows();
    assert_eq!(rows.column("COLUMN_NAME"), vec!["id", "email"]);
    assert_eq!(rows.column("DATA_TYPE"), vec!["bigint", "text"]);

    // The same query through a prepared statement, with the table name bound —
    // which is how an ORM actually asks.
    let stmt = client
        .prepare(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = ? AND table_name = ?",
        )
        .expect("prepare");
    assert_eq!(stmt.param_count, 2);
    // A shim-answered statement still reports zero columns at prepare time
    // (AHL-466 only covers what `inlaysql::Statement` plans): the shim has
    // no equivalent "plan without running" step, and inventing a shape here
    // would be worse than deferring to execute time, same as before.
    assert_eq!(stmt.column_count, 0);
    let rows = client
        .execute(
            &stmt,
            &[
                Param::Str("inlaysql".to_string()),
                Param::Str("posts".to_string()),
            ],
        )
        .expect("execute")
        .rows();
    assert_eq!(rows.column("COLUMN_NAME"), vec!["id", "title"]);
}

/// A metadata filter the shim cannot evaluate must fail, not quietly answer as
/// though there were no filter at all.
#[test]
fn an_unparsable_metadata_filter_fails_instead_of_answering_wrongly() {
    let server = TestServer::start("honest-shim");
    let mut client = server.client();
    client.ok_query("CREATE TABLE users (id INTEGER PRIMARY KEY)");

    let error = client
        .query("SELECT table_name FROM information_schema.tables WHERE table_name > 'a'")
        .unwrap_err();
    assert_eq!(error.code, 1235);

    // The same question asked in a supported way still works, so the refusal
    // above is about the filter and not about the view.
    let rows = client
        .ok_query("SELECT table_name FROM information_schema.tables WHERE table_name = 'users'")
        .rows();
    assert_eq!(rows.rows.len(), 1);
}

/// The views an ORM reads for foreign-key discovery answer with real rows
/// over the wire — and the object views the engine cannot have answer zero
/// rows in the shapes MySQL 8 defines.
#[test]
fn information_schema_constraint_views_answer_over_the_wire() {
    let server = TestServer::start("infoschema-constraints");
    let mut client = server.client();
    client.ok_query("CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT)");
    client.ok_query(
        "CREATE TABLE books (\
           id INTEGER PRIMARY KEY, \
           author_id INTEGER REFERENCES authors(id), \
           title TEXT UNIQUE, \
           edition INTEGER, \
           UNIQUE (author_id, edition), \
           CHECK (edition > 0))",
    );
    client.ok_query("CREATE UNIQUE INDEX ux_authors_name ON authors (name)");

    // Foreign-key discovery, the query a migration tool sends. The unnamed
    // UNIQUE constraints carry the engine's generated index names, which is
    // exactly what STATISTICS reports as their INDEX_NAME.
    let rows = client
        .ok_query(
            "SELECT constraint_name, table_name, column_name, ordinal_position, \
                    position_in_unique_constraint, referenced_table_name, referenced_column_name \
             FROM information_schema.key_column_usage \
             WHERE table_name = 'books' ORDER BY constraint_name, ordinal_position",
        )
        .rows();
    assert_eq!(
        rows.column("CONSTRAINT_NAME"),
        vec![
            "__inlaysql_uniq_books_author_id_edition_1",
            "__inlaysql_uniq_books_author_id_edition_1",
            "__inlaysql_uniq_books_title_0",
            "books_ibfk_1",
            "PRIMARY",
        ]
    );
    assert_eq!(
        rows.column("REFERENCED_TABLE_NAME"),
        vec!["NULL", "NULL", "NULL", "authors", "NULL"]
    );
    assert_eq!(
        rows.column("REFERENCED_COLUMN_NAME"),
        vec!["NULL", "NULL", "NULL", "id", "NULL"]
    );
    assert_eq!(
        rows.column("POSITION_IN_UNIQUE_CONSTRAINT"),
        vec!["NULL", "NULL", "NULL", "1", "NULL"]
    );

    // ENFORCED names the engine's real behaviour: foreign keys are recorded
    // but never checked (README, TESTING.md), everything else really is.
    let rows = client
        .ok_query(
            "SELECT constraint_name, constraint_type, enforced \
             FROM information_schema.table_constraints \
             WHERE table_name = 'books' ORDER BY constraint_name",
        )
        .rows();
    assert_eq!(
        rows.column("CONSTRAINT_TYPE"),
        vec!["UNIQUE", "UNIQUE", "CHECK", "FOREIGN KEY", "PRIMARY KEY"]
    );
    assert_eq!(
        rows.column("ENFORCED"),
        vec!["YES", "YES", "YES", "NO", "YES"]
    );

    // OR across two tables answers both, which an AND could never do.
    let rows = client
        .ok_query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_name = 'authors' OR table_name = 'books' ORDER BY table_name",
        )
        .rows();
    assert_eq!(rows.column("TABLE_NAME"), vec!["authors", "books"]);

    // Object kinds the engine does not have answer zero rows, in the shapes
    // MySQL 8 defines.
    for view in ["views", "triggers", "routines"] {
        let rows = client
            .ok_query(&format!("SELECT * FROM information_schema.{view}"))
            .rows();
        assert_eq!(rows.rows.len(), 0, "{view} must be empty");
    }

    // And the refusal that keeps all of the above honest: an OR nested inside
    // an AND-group is refused rather than silently evaluated as something
    // else.
    let error = client
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE (table_name = 'authors' OR table_name = 'books') AND table_schema = 'inlaysql'",
        )
        .unwrap_err();
    assert_eq!(error.code, 1235);
}

#[test]
fn transactions_map_onto_the_engine_and_rollback_really_rolls_back() {
    let server = TestServer::start("transactions");
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");

    client.ok_query("BEGIN");
    client.ok_query("INSERT INTO kv (id, body) VALUES (1, 'kept')");
    client.ok_query("COMMIT");

    client.ok_query("START TRANSACTION");
    client.ok_query("INSERT INTO kv (id, body) VALUES (2, 'discarded')");
    client.ok_query("ROLLBACK");

    let rows = client.ok_query("SELECT id FROM kv").rows();
    assert_eq!(rows.column("id"), vec!["1"], "the rollback must have held");

    // Autocommit off: work is buffered until COMMIT.
    client.ok_query("SET autocommit=0");
    client.ok_query("INSERT INTO kv (id, body) VALUES (3, 'pending')");
    client.ok_query("COMMIT");
    client.ok_query("SET autocommit=1");

    let rows = client.ok_query("SELECT id FROM kv").rows();
    assert_eq!(rows.column("id"), vec!["1", "3"]);

    // A COMMIT with nothing open is a no-op, not an error — drivers send them.
    client.ok_query("COMMIT");
    client.ok_query("ROLLBACK");
}

/// **The divergence this whole item exists to close** (AHL-469).
///
/// Before it, a table created the way every ORM creates one — with
/// `COLLATE utf8mb4_unicode_ci` on the table — compared text byte for byte
/// here and case-insensitively in MySQL, so `WHERE name = 'ADA'` returned
/// *fewer rows* through this server than through MySQL, silently. Every other
/// divergence in this file fails loudly; this one did not.
#[test]
fn a_case_insensitive_table_collation_makes_equality_case_insensitive() {
    let server = TestServer::start("collation-ci");
    let mut client = server.client();

    let reply = client.ok_query(
        "create table `people` (\
           `id` bigint unsigned auto_increment primary key, \
           `name` varchar(255)\
         ) engine=InnoDB default charset=utf8mb4 collate=utf8mb4_unicode_ci",
    );
    // The mapping is reported, not silent: it is a *narrowing*, and a client
    // that reads `utf8mb4_unicode_ci` and gets ASCII-only folding has to be
    // told which part it did not get.
    assert!(reply.warnings() > 0);
    let warnings = client.ok_query("SHOW WARNINGS").rows();
    let text = warnings.column("Message").join("\n");
    assert!(
        text.contains("COLLATE NOCASE"),
        "the collation mapping must be named: {text}"
    );
    assert!(
        text.contains("accent-insensitive"),
        "the accent gap must be named, because NOCASE does not close it: {text}"
    );

    client.ok_query("insert into people (name) values ('ada')");
    client.ok_query("insert into people (name) values ('Grace')");
    client.ok_query("insert into people (name) values ('e\u{301}lodie')");

    // The bug, gone: MySQL matches this row and now so does this server.
    let rows = client
        .ok_query("select name from people where name = 'ADA'")
        .rows();
    assert_eq!(rows.column("name"), vec!["ada"]);

    // And through every other comparison the shim can reach.
    let rows = client
        .ok_query("select name from people where name in ('GRACE') order by name")
        .rows();
    assert_eq!(rows.column("name"), vec!["Grace"]);

    // `SHOW FULL COLUMNS` now reports a collation name that means what the
    // column does, rather than a fixed string.
    let rows = client.ok_query("SHOW FULL COLUMNS FROM people").rows();
    assert_eq!(
        rows.column("Collation"),
        vec!["NULL", "utf8mb4_general_ci"],
        "the key is not text and has no collation; the name is"
    );

    // What is *not* fixed, said out loud here as well as in `docs/server.md`:
    // NOCASE folds ASCII and nothing else, so an accent still separates two
    // strings MySQL's `utf8mb4_unicode_ci` would call equal.
    let rows = client
        .ok_query("select name from people where name = 'elodie'")
        .rows();
    assert!(
        rows.rows.is_empty(),
        "accent folding is a documented gap, not a silent one: {:?}",
        rows.rows
    );
}

/// A `*_bin` collation is exactly `BINARY`, so it is applied with no warning
/// at all — and the column really does compare byte for byte beside a
/// case-insensitive one in the same table.
#[test]
fn a_binary_column_keeps_its_case_sensitivity_inside_a_ci_table() {
    let server = TestServer::start("collation-bin");
    let mut client = server.client();

    client.ok_query(
        "create table `t` (`a` varchar(255) collate utf8mb4_bin, `b` varchar(255)) \
         collate=utf8mb4_unicode_ci",
    );
    client.ok_query("insert into t (a, b) values ('ada', 'ada')");

    let rows = client.ok_query("select a from t where a = 'ADA'").rows();
    assert!(rows.rows.is_empty(), "`a` is utf8mb4_bin: byte-wise");
    let rows = client.ok_query("select b from t where b = 'ADA'").rows();
    assert_eq!(rows.column("b"), vec!["ada"], "`b` inherited the table's");
}

/// A savepoint is how an ORM spells a nested transaction. The engine has none,
/// so this must fail loudly rather than appear to work and silently keep the
/// writes an inner rollback was supposed to discard.
#[test]
fn a_savepoint_is_refused_rather_than_silently_accepted() {
    let server = TestServer::start("savepoints");
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY)");
    client.ok_query("BEGIN");

    let error = client.query("SAVEPOINT trans1").unwrap_err();
    assert_eq!(error.code, 1235);
    let error = client.query("ROLLBACK TO SAVEPOINT trans1").unwrap_err();
    assert_eq!(error.code, 1235);

    client.ok_query("ROLLBACK");
}

#[test]
fn ping_init_db_and_quit_behave() {
    let server = TestServer::start("commands");
    let mut client = server.client();

    client.ping().expect("COM_PING");
    client.init_db("inlaysql").expect("COM_INIT_DB");

    let rows = client.ok_query("SELECT DATABASE()").rows();
    assert_eq!(rows.cell(0, 0), "inlaysql");

    // MySQL's own schemas are not pretended into existence.
    let error = client.init_db("mysql").unwrap_err();
    assert_eq!(error.code, 1044);

    // A schema named in the handshake is picked up the same way.
    let mut named = Client::connect(server.addr, "root", &server.password, Some("app"))
        .expect("connect with a database");
    let rows = named.ok_query("SELECT DATABASE()").rows();
    assert_eq!(rows.cell(0, 0), "app");

    client.quit();
}

#[test]
fn a_wrong_password_is_refused_with_access_denied() {
    let server = TestServer::start("auth");

    let error = Client::connect(server.addr, "root", "wrong", None)
        .expect_err("a wrong password must be refused");
    assert_eq!(error.code, 1045, "ER_ACCESS_DENIED_ERROR");
    assert_eq!(error.sqlstate, "28000");

    // A wrong user is refused the same way. The message names the user, as
    // MySQL's does, but nothing in either reply says *which* half was wrong —
    // so a guesser cannot use the difference to enumerate valid users.
    let other = Client::connect(server.addr, "nobody", &server.password, None)
        .expect_err("a wrong user must be refused");
    assert_eq!(other.code, 1045);
    assert_eq!(other.sqlstate, error.sqlstate);
    assert_eq!(
        other.message.replace("nobody", "USER"),
        error.message.replace("root", "USER"),
        "the two refusals must differ only in the user name they echo"
    );
    for reply in [&error, &other] {
        let lower = reply.message.to_lowercase();
        assert!(
            !lower.contains("unknown user")
                && !lower.contains("no such user")
                && !lower.contains("incorrect password"),
            "the refusal must not say which half was wrong: {}",
            reply.message
        );
    }

    // Rubbish in the token field is refused rather than accepted or crashing.
    let error = Client::connect_with_bad_token(server.addr, "root")
        .expect_err("a forged token must be refused");
    assert_eq!(error.code, 1045);

    // And the right password still works, so the check is not simply "no".
    let mut client = server.client();
    client.ping().expect("ping");
}

// =====================================================================
// AHL-467: caching_sha2_password
// =====================================================================

/// The fast path: a client sends `caching_sha2_password`'s scramble as its
/// very first response, exactly the way a real `mysql` CLI or PDO/mysqlnd
/// connection does, and this server — which already holds the plaintext
/// password — completes in one round trip. `docs/server.md` documents that
/// the handshake now advertises this plugin as the default.
#[test]
fn a_caching_sha2_client_authenticates() {
    let server = TestServer::start("caching-sha2");

    let mut client = Client::connect_caching_sha2(server.addr, "root", &server.password)
        .expect("caching_sha2_password fast-auth must succeed");
    client.ping().expect("ping");

    let (affected, _) = client
        .query("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)")
        .expect("create")
        .ok();
    assert_eq!(affected, 0);
}

/// The path a client with nothing cached takes: an empty first response,
/// then the cleartext password once the server asks for full
/// authentication. Acceptable here — and only here — because v1 is
/// documented plaintext-localhost already (`docs/server.md`), so a
/// cleartext password crossing this connection reveals nothing a network
/// observer could not already read directly off the wire.
#[test]
fn a_caching_sha2_client_completes_full_authentication() {
    let server = TestServer::start("caching-sha2-full");

    let mut client = Client::connect_caching_sha2_full_auth(server.addr, "root", &server.password)
        .expect("full authentication must succeed with the right password");
    client.ping().expect("ping");
}

/// The one piece of `caching_sha2_password` this server does not implement:
/// the RSA public-key exchange a real client falls back to on an
/// unencrypted connection with nothing cached, if it is not willing to send
/// a cleartext password. Refused with a clear reason rather than silently
/// mishandled — and the connection is still cleanly closed, not crashed.
#[test]
fn the_rsa_exchange_is_refused_with_a_clear_reason() {
    let server = TestServer::start("caching-sha2-rsa");

    let error = Client::connect_caching_sha2_requesting_rsa(server.addr, "root")
        .expect_err("the RSA request must be refused");
    assert_eq!(error.code, 1235, "ER_NOT_SUPPORTED_YET");
    let lower = error.message.to_lowercase();
    assert!(
        lower.contains("rsa"),
        "the refusal must name what it cannot do: {}",
        error.message
    );
    assert!(
        lower.contains("caching_sha2_password"),
        "the refusal must name the plugin: {}",
        error.message
    );
}

/// A client whose own default is a plugin this server does not speak
/// directly (older than `caching_sha2_password`, and not
/// `mysql_native_password` either) is switched onto
/// `mysql_native_password` — the plugin every driver already falls back to
/// — and completes normally from there.
#[test]
fn a_client_offering_an_unknown_plugin_is_switched_to_native_password() {
    let server = TestServer::start("auth-switch");

    let mut client = Client::connect_via_auth_switch(server.addr, "root", &server.password)
        .expect("the switched authentication must succeed");
    client.ping().expect("ping");
}

/// A wrong password is refused the same way under both plugins this server
/// completes directly — same code, same connection-still-usable-afterwards
/// property `a_wrong_password_is_refused_with_access_denied` already pins
/// for `mysql_native_password`.
#[test]
fn a_wrong_password_is_refused_under_every_plugin() {
    let server = TestServer::start("auth-wrong-everywhere");

    let fast = Client::connect_caching_sha2(server.addr, "root", "wrong")
        .expect_err("a wrong password must be refused over the fast path");
    assert_eq!(fast.code, 1045, "ER_ACCESS_DENIED_ERROR");
    assert_eq!(fast.sqlstate, "28000");

    let full = Client::connect_caching_sha2_full_auth(server.addr, "root", "wrong")
        .expect_err("a wrong password must be refused over full authentication");
    assert_eq!(full.code, 1045);
    assert_eq!(full.sqlstate, "28000");

    let switched = Client::connect_via_auth_switch(server.addr, "root", "wrong")
        .expect_err("a wrong password must be refused after switching plugins");
    assert_eq!(switched.code, 1045);
    assert_eq!(switched.sqlstate, "28000");

    // And the right password still works over every path, so none of the
    // three checks above is simply "no" regardless of what was sent.
    Client::connect_caching_sha2(server.addr, "root", &server.password)
        .expect("fast-auth must still work")
        .ping()
        .expect("ping");
    Client::connect_caching_sha2_full_auth(server.addr, "root", &server.password)
        .expect("full authentication must still work")
        .ping()
        .expect("ping");
    Client::connect_via_auth_switch(server.addr, "root", &server.password)
        .expect("the switched path must still work")
        .ping()
        .expect("ping");
}

/// Several connections at once, each with its own `Database` handle, writing
/// rows that do not overlap. This is decision D2 working: the handles settle
/// among themselves and no writer loses a row.
#[test]
fn concurrent_connections_write_disjoint_rows() {
    const WRITERS: i64 = 4;
    const PER_WRITER: i64 = 20;

    let server = TestServer::start("concurrent");
    let mut setup = server.client();
    setup.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, who INTEGER)");
    drop(setup);

    let addr = server.addr;
    let password = server.password.clone();
    std::thread::scope(|scope| {
        for writer in 0..WRITERS {
            let password = password.clone();
            scope.spawn(move || {
                let mut client = Client::connect(addr, "root", &password, None).expect("connect");
                for round in 0..PER_WRITER {
                    let id = round * WRITERS + writer + 1;
                    let sql = format!("INSERT INTO kv (id, who) VALUES ({id}, {writer})");
                    loop {
                        match client.query(&sql) {
                            Ok(_) => break,
                            // First-committer-wins surfaces as the deadlock
                            // code, whose documented remedy is to retry.
                            Err(error) if error.code == 1213 => continue,
                            Err(error) => panic!("writer {writer} failed on {id}: {error:?}"),
                        }
                    }
                }
            });
        }
    });

    let mut reader = server.client();
    let rows = reader.ok_query("SELECT id FROM kv ORDER BY id").rows();
    let ids: Vec<i64> = rows
        .column("id")
        .iter()
        .map(|value| value.parse().unwrap())
        .collect();
    assert_eq!(
        ids,
        (1..=WRITERS * PER_WRITER).collect::<Vec<_>>(),
        "every row from every writer must be present exactly once"
    );

    // Each writer's rows are all there, so nobody's work was lost wholesale.
    let rows = reader.ok_query("SELECT who FROM kv").rows();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for who in rows.column("who") {
        *counts.entry(who).or_default() += 1;
    }
    for writer in 0..WRITERS {
        assert_eq!(
            counts.get(&writer.to_string()),
            Some(&(PER_WRITER as usize))
        );
    }
}

/// AHL-495 investigation: aggregate prepared point-read throughput over the
/// wire at 1 and 8 connections, on the machine the test runs on. Not a CI
/// test — ignored, run by hand with `cargo test --release -p inlaysql-server
/// --test wire -- --ignored --nocapture`.
#[test]
#[ignore]
fn read_throughput_one_vs_eight_connections() {
    const ROWS: i64 = 2000;
    let reads_per_conn: i64 = std::env::var("READS_PER_CONN")
        .map(|v| v.parse().expect("READS_PER_CONN"))
        .unwrap_or(1000);

    let server = TestServer::start("throughput");
    let mut setup = server.client();
    setup.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    for start in (1..=ROWS).step_by(100) {
        let end = (start + 99).min(ROWS);
        let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, 'body-{id}')"));
        }
        setup.ok_query(&sql);
    }
    drop(setup);

    for connections in [1usize, 8usize] {
        let addr = server.addr;
        let password = server.password.clone();
        let elapsed = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..connections {
                let password = password.clone();
                workers.push(scope.spawn(move || {
                    let mut client =
                        Client::connect(addr, "root", &password, None).expect("connect");
                    let stmt = client
                        .prepare("SELECT body FROM kv WHERE id = ?")
                        .expect("prepare");
                    for _ in 0..100 {
                        client
                            .execute(&stmt, &[Param::Int(1)])
                            .expect("warmup read");
                    }
                    let start = std::time::Instant::now();
                    for key in 0..reads_per_conn {
                        client
                            .execute(&stmt, &[Param::Int(key % ROWS + 1)])
                            .expect("read");
                    }
                    start.elapsed()
                }));
            }
            workers
                .into_iter()
                .fold(std::time::Duration::ZERO, |acc, w| {
                    acc.max(w.join().expect("worker"))
                })
        });
        let ops = reads_per_conn * connections as i64;
        let rate = ops as f64 / elapsed.as_secs_f64();
        println!("connections={connections}: {ops} reads in {elapsed:?} = {rate:.1} ops/s");
    }
}

/// AHL-495 investigation, part two: the shape a shared page cache is actually
/// for — many short-lived connections, each of which would otherwise re-read
/// the pages its first descent needs from the device. Every round opens a
/// fresh connection, runs one point read, and closes it again.
///
/// Run it twice to see the shared cache's effect: once as is, once with
/// `INLAYSQL_DISABLE_SHARED_READ_CACHE=1` — the knob
/// `inlaysql::device::shared_read_cache_budget` reads, which pins the cache
/// budget to zero. Two separate process runs are required, not one, because
/// the budget is fixed when the first handle on the file opens.
#[test]
#[ignore]
fn cold_connection_throughput_shared_cache_on_vs_off() {
    const ROWS: i64 = 2000;
    let rounds: i64 = std::env::var("COLD_ROUNDS")
        .map(|v| v.parse().expect("COLD_ROUNDS"))
        .unwrap_or(300);

    let server = TestServer::start("cold-connections");
    let mut setup = server.client();
    setup.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    for start in (1..=ROWS).step_by(100) {
        let end = (start + 99).min(ROWS);
        let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, 'body-{id}')"));
        }
        setup.ok_query(&sql);
    }
    drop(setup);

    let addr = server.addr;
    let password = server.password.clone();
    let start = std::time::Instant::now();
    for round in 0..rounds {
        let mut client = Client::connect(addr, "root", &password, None).expect("connect");
        let stmt = client
            .prepare("SELECT body FROM kv WHERE id = ?")
            .expect("prepare");
        let reply = client
            .execute(&stmt, &[Param::Int(round % ROWS + 1)])
            .expect("read");
        assert_eq!(reply.rows().rows.len(), 1);
        drop(client);
    }
    let elapsed = start.elapsed();
    println!(
        "cold connections: {rounds} in {elapsed:?} = {:.1} connections/s",
        rounds as f64 / elapsed.as_secs_f64()
    );
}

/// Concurrent readers, each on its own connection, all see the same committed
/// rows — the shape that exercises the shared raw-page cache: every
/// connection's descent reads pages another connection may have cached, and a
/// wrong or stale page served from that cache would surface here as a wrong
/// row, not as an error.
#[test]
fn concurrent_readers_share_pages_and_all_see_every_row() {
    const ROWS: i64 = 500;
    const READERS: i64 = 4;

    let server = TestServer::start("shared-reads");
    let mut setup = server.client();
    setup.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    for start in (1..=ROWS).step_by(100) {
        let end = (start + 99).min(ROWS);
        let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, 'body-{id}')"));
        }
        setup.ok_query(&sql);
    }
    drop(setup);

    let addr = server.addr;
    let password = server.password.clone();
    std::thread::scope(|scope| {
        for reader in 0..READERS {
            let password = password.clone();
            scope.spawn(move || {
                let mut client = Client::connect(addr, "root", &password, None).expect("connect");
                let stmt = client
                    .prepare("SELECT body FROM kv WHERE id = ?")
                    .expect("prepare");
                for round in 0..10 {
                    let id = (round * READERS + reader) % ROWS + 1;
                    let reply = client.execute(&stmt, &[Param::Int(id)]).expect("read");
                    let rows = reply.rows();
                    assert_eq!(
                        rows.column("body"),
                        vec![format!("body-{id}")],
                        "reader {reader} round {round}"
                    );
                }
            });
        }
    });
}

/// A brand-new connection — nothing warmed in its own decoded cache — reads
/// rows a connection that has since closed committed and read. Every page its
/// first descent needs comes out of the shared raw cache, so a stale entry
/// would show up here as a wrong row.
#[test]
fn a_fresh_connection_reads_pages_cached_by_a_closed_one() {
    let server = TestServer::start("fresh-reads");
    let mut first = server.client();
    first.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    for start in (1..=200).step_by(100) {
        let end = (start + 99).min(200);
        let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, 'body-{id}')"));
        }
        first.ok_query(&sql);
    }
    let rows = first.ok_query("SELECT body FROM kv WHERE id = 150").rows();
    assert_eq!(rows.column("body"), vec!["body-150"]);
    drop(first);

    let mut fresh = server.client();
    for id in [1, 50, 150, 200] {
        let rows = fresh
            .ok_query(&format!("SELECT body FROM kv WHERE id = {id}"))
            .rows();
        assert_eq!(rows.column("body"), vec![format!("body-{id}")]);
    }
}

/// One connection sees another's committed writes — the snapshot refresh that
/// makes thread-per-connection viable at all.
#[test]
fn one_connection_sees_another_connections_commits() {
    let server = TestServer::start("visibility");
    let mut writer = server.client();
    let mut reader = server.client();

    writer.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY)");
    // The reader has to be able to see a table it was not connected through.
    assert!(reader.ok_query("SELECT id FROM kv").rows().rows.is_empty());

    writer.ok_query("INSERT INTO kv (id) VALUES (1)");
    let rows = reader.ok_query("SELECT id FROM kv").rows();
    assert_eq!(rows.column("id"), vec!["1"]);
}

#[test]
fn connections_past_the_limit_are_refused_with_a_proper_error() {
    let server = TestServer::start_with("limit", "s3cret", 1);
    let _first = server.client();

    // The cap is one, so the next connection is told so rather than hanging or
    // having the socket closed under it.
    let error = Client::connect(server.addr, "root", &server.password, None)
        .expect_err("the second connection must be refused");
    assert_eq!(error.code, 1040, "ER_CON_COUNT_ERROR");
}

/// The `--max-connections` refusal is the one packet this server ever sends
/// before a handshake, so nothing has negotiated `CLIENT_PROTOCOL_41` yet,
/// and the SQLSTATE marker every other error packet carries must not be
/// there. This reads the raw bytes off the socket directly, independent of
/// this file's own `parse_pre_handshake_error` and `parse_error` helpers, so
/// the test does not just prove the two agree with each other — it proves
/// the wire format itself.
///
/// A real client cares: mysql-connector-python mis-parses a `#`-marked
/// packet at this point in the exchange, rendering `1040 (HY000):
/// #08004Too many connections` instead of a clean message, because it
/// (correctly) does not expect a SQLSTATE marker before any capability
/// negotiation has happened.
#[test]
fn the_connection_cap_refusal_carries_no_sqlstate_marker_on_the_wire() {
    let server = TestServer::start_with("limit-bytes", "s3cret", 1);
    let _first = server.client();

    let mut raw = TcpStream::connect(server.addr).expect("tcp connect");
    raw.set_nodelay(true).ok();
    let mut header = [0u8; 4];
    raw.read_exact(&mut header).expect("packet header");
    let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let mut payload = vec![0u8; length];
    raw.read_exact(&mut payload).expect("packet payload");

    assert_eq!(payload[0], 0xff, "an ERR packet");
    assert_eq!(
        u16::from_le_bytes([payload[1], payload[2]]),
        1040,
        "ER_CON_COUNT_ERROR"
    );
    // No `#` marker and no 5-byte SQLSTATE: the message starts immediately
    // after the 2-byte error code.
    assert_eq!(
        &payload[3..],
        b"Too many connections",
        "a pre-handshake ERR packet must not carry the 4.1 SQLSTATE marker"
    );
}

/// The cap a client is *told* is the cap that is enforced.
///
/// It used to report `max_connections=0` — no cap at all — while refusing the
/// connection past 64. A pool reads that number to size itself, so the lie is
/// not cosmetic: it says "open as many as you like" to something that will
/// then hit `1040` in production and have no way to have known better.
#[test]
fn the_reported_connection_cap_is_the_one_that_is_enforced() {
    let server = TestServer::start_with("cap-truth", "s3cret", 3);
    let mut client = server.client();

    assert_eq!(
        client
            .ok_query("SELECT @@max_connections")
            .rows()
            .cell(0, 0),
        "3"
    );
    let rows = client
        .ok_query("SHOW VARIABLES LIKE 'max_connections'")
        .rows();
    assert_eq!(rows.cell(0, 1), "3");

    // And the number is real: two more fill the cap, the fourth is refused.
    let _second = server.client();
    let _third = server.client();
    let error = Client::connect(server.addr, "root", &server.password, None)
        .expect_err("the fourth connection must be refused");
    assert_eq!(error.code, 1040, "ER_CON_COUNT_ERROR");
}

/// The reported `wait_timeout` is a socket read timeout that is really set,
/// and an idle client really does lose its slot.
///
/// This is the failure this closes: the cap is 64, so 64 clients that connect
/// and then say nothing are the entire server — no statement to log, no
/// credential to revoke, and nothing to do about it short of a restart. The
/// server named a `wait_timeout` for it and enforced nothing, which is worse
/// than naming none: a client tunes its idle-connection lifetime against that
/// number.
#[test]
fn an_idle_connection_loses_its_slot_after_the_wait_timeout_it_reports() {
    let server = TestServer::start_tuned("idle-timeout", "s3cret", |options| {
        options.max_connections = 1;
        options.wait_timeout_secs = 1;
    });

    let mut idle = server.client();
    // Reported and enforced are the same number, which is the whole point:
    // `net_read_timeout` reports it too, since one `SO_RCVTIMEO` is what
    // applies to both waiting for a command and reading one.
    let rows = idle
        .ok_query("SELECT @@wait_timeout, @@net_read_timeout")
        .rows();
    assert_eq!(rows.cell(0, 0), "1");
    assert_eq!(rows.cell(0, 1), "1");

    // The one slot is taken.
    let error = Client::connect(server.addr, "root", &server.password, None)
        .expect_err("the second connection must be refused while the first holds the slot");
    assert_eq!(error.code, 1040, "ER_CON_COUNT_ERROR");

    // Now say nothing. A generous client-side timeout so a server that never
    // hangs up fails this test as a timeout rather than hanging the suite.
    idle.stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .expect("client read timeout");
    let mut byte = [0u8; 1];
    match idle.stream.read(&mut byte) {
        // A clean FIN: the server closed it, which is what MySQL does on
        // `wait_timeout` — there is no error packet for "you went quiet".
        Ok(0) => {}
        Ok(n) => panic!("the server sent {n} bytes instead of closing an idle connection"),
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        Err(error) => panic!("the idle connection was still open: {error}"),
    }

    // And the slot came back. Retried briefly: the count is decremented as the
    // connection's thread unwinds, which is after the socket closes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match Client::connect(server.addr, "root", &server.password, None) {
            Ok(mut fresh) => {
                assert_eq!(fresh.ok_query("SELECT 1").rows().cell(0, 0), "1");
                break;
            }
            Err(error) if std::time::Instant::now() < deadline => {
                assert_eq!(error.code, 1040);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("the timed-out connection never released its slot: {error:?}"),
        }
    }
}

/// `--page-reuse` reaches every connection's handle, and the file stops
/// growing without bound under churn that leaves the live data size flat.
///
/// The option itself is one field on `EngineOptions`; what this test is
/// actually for is the server's own structure around it. Reclamation only
/// offers pages freed before `Device::min_reader_seq`, and every read-write
/// handle pins that watermark at the sequence it last read — so the process's
/// lock-keeping handle had to stop being a `Database` (which reads once at
/// startup and then never again, pinning the watermark for the life of the
/// server) and become a bare `FileDevice`, which holds the same OS lock and
/// registers no reader. With a `Database` keeper this same churn produced a
/// *larger* file with reuse on than with it off, because the free-list rows
/// accumulate and nothing ever draws them down.
#[test]
fn page_reuse_bounds_the_file_the_server_writes() {
    let off = churn_through_a_server("reuse-off", false);
    let on = churn_through_a_server("reuse-on", true);

    assert!(
        off > 0 && on > 0,
        "both servers should have written real data (off={off}, on={on})"
    );
    assert!(
        on < off * 3 / 4,
        "page reuse did not bound the file the server wrote: off = {off} bytes, \
         on = {on} bytes (expected the reuse-on file well below 3/4 of reuse-off)"
    );
}

/// Run the same write/delete/rewrite churn over the wire against a server with
/// page reuse `enabled`, and return the size of the file it wrote.
fn churn_through_a_server(name: &str, enabled: bool) -> u64 {
    const ROUNDS: usize = 10;
    const KEYS: usize = 40;

    let server = TestServer::start_tuned(name, "s3cret", |options| {
        options.page_reuse = enabled;
    });
    let mut client = server.client();
    client.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");

    for round in 0..ROUNDS {
        // Long enough that a row is a real share of a page, so the same pages
        // are superseded round after round rather than merely grown into.
        let body = round.to_string().repeat(150);
        for id in 0..KEYS {
            client.ok_query(&format!(
                "INSERT INTO t (id, v) VALUES ({id}, '{body}') \
                 ON CONFLICT (id) DO UPDATE SET v = excluded.v"
            ));
        }
        // Delete a rotating quarter, so their pages are genuinely freed and
        // the next round's inserts can draw them again.
        let start = (round * KEYS / 4) % KEYS;
        for offset in 0..KEYS / 4 {
            let id = (start + offset) % KEYS;
            client.ok_query(&format!("DELETE FROM t WHERE id = {id}"));
        }
    }

    std::fs::metadata(server.path()).expect("stat").len()
}

/// `--paged-text` reaches every connection's handle the same way
/// `--paged-vectors` does: it is one field on `EngineOptions`, and what this
/// test is actually for is that the flag really is wired all the way through
/// `Server::bind` to a connection that can build, query and remove from a
/// full-text index under it — and that what it built is really in the file,
/// not merely in this process, by reading it back from a second server bound
/// to the same database the way an operator's restart would.
#[test]
fn paged_text_indexes_flag_serves_full_text_queries_and_survives_a_restart() {
    let server = TestServer::start_tuned("paged-text", "s3cret", |options| {
        options.paged_text_indexes = true;
    });
    let mut client = server.client();
    client.ok_query("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)");
    client.ok_query("CREATE INDEX docs_body ON docs (body)");
    client.ok_query(
        "INSERT INTO docs (id, body) VALUES \
         (1, 'alpha alpha alpha'), (2, 'beta beta beta'), (3, 'alpha beta')",
    );

    // `bm25_score` is an index walk, not a per-row scalar over the whole
    // table — a document with zero occurrences of the term is not returned at
    // all, not merely scored zero. Doc 1 says "alpha" three times, doc 3 once,
    // doc 2 not at all, so only the first two come back, doc 1 ranked above
    // doc 3 by a separation wide enough that tie-breaking cannot matter.
    let ranked = client
        .ok_query(
            "SELECT id, bm25_score(body, 'alpha') AS score FROM docs \
             ORDER BY score DESC, id ASC",
        )
        .rows();
    assert_eq!(ranked.column("id"), vec!["1", "3"]);

    // A delete has to reach the postings too, not just the row.
    client.ok_query("DELETE FROM docs WHERE id = 1");
    let after_delete = client
        .ok_query(
            "SELECT id, bm25_score(body, 'alpha') AS score FROM docs \
             ORDER BY score DESC, id ASC",
        )
        .rows();
    assert_eq!(after_delete.column("id"), vec!["3"]);
    client.quit();

    // A second server on the same file, with the same flag, as an operator
    // restarting one would run it — not `TestServer::reopened`, which binds
    // plain defaults and would prove nothing about this flag. The postings
    // have to already be on disk: nothing on this path rebuilds an index from
    // the rows it describes unless its stamp says it must.
    let restart_options = ServerOptions {
        bind: "127.0.0.1".to_string(),
        port: 0,
        user: "root".to_string(),
        password: server.password.clone(),
        paged_text_indexes: true,
        ..ServerOptions::default()
    };
    let restarted = Server::bind(server.path(), &restart_options).expect("re-bind");
    let addr = restarted.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = restarted.run();
    });
    let mut after_restart = Client::connect(addr, "root", &server.password, None)
        .expect("the restarted server should still serve this database");
    let restarted_ranking = after_restart
        .ok_query(
            "SELECT id, bm25_score(body, 'alpha') AS score FROM docs \
             ORDER BY score DESC, id ASC",
        )
        .rows();
    assert_eq!(restarted_ranking.column("id"), vec!["3"]);
    after_restart.quit();
}

/// A statement larger than one packet has to be reassembled from its
/// continuations, and a row larger than one packet has to be split into them.
#[test]
fn oversized_statements_and_rows_survive_packet_splitting() {
    let server = TestServer::start("large");
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");

    // Comfortably larger than a single write, though not the 16 MiB packet
    // limit, which would make this test slow for no extra coverage.
    let big = "x".repeat(400_000);
    client.ok_query(&format!("INSERT INTO kv (id, body) VALUES (1, '{big}')"));

    let rows = client.ok_query("SELECT body FROM kv").rows();
    assert_eq!(rows.cell(0, 0).len(), big.len());
    assert_eq!(rows.cell(0, 0), big);
}

// =====================================================================
// MySQL DDL translation (AHL-431)
// =====================================================================

/// The statement a schema builder emits first, in the shape it emits it:
/// backticked identifiers, an unsigned auto-increment key, a `varchar(255)`,
/// and MySQL's table options. It used to be a `1064` syntax error, which is
/// why nothing got past it.
///
/// The test is not "it parses". It is that the table the client asked for is
/// the table it got: the key auto-assigns, the OK packet reports it, and the
/// rows read back.
#[test]
fn a_schema_builders_create_table_runs_and_the_table_it_makes_works() {
    let server = TestServer::start("mysql-ddl");
    let mut client = server.client();

    let reply = client.ok_query(
        "create table `users` (\
           `id` bigint unsigned auto_increment primary key, \
           `name` varchar(255), \
           `email` varchar(255)\
         ) engine=InnoDB default charset=utf8mb4 collate=utf8mb4_unicode_ci",
    );
    // Nothing was dropped quietly: the OK packet says how many clauses went.
    assert_eq!(
        reply.warnings(),
        5,
        "UNSIGNED, AUTO_INCREMENT, ENGINE, CHARSET and COLLATE were all removed"
    );

    // And `SHOW WARNINGS` says which, and why.
    let warnings = client.ok_query("SHOW WARNINGS").rows();
    assert_eq!(warnings.rows.len(), 5);
    assert!(warnings.column("Level").iter().all(|l| l == "Warning"));
    assert!(warnings.column("Code").iter().all(|c| c == "1618"));
    let text = warnings.column("Message").join("\n").to_ascii_lowercase();
    for clause in ["unsigned", "auto_increment", "engine", "charset", "collate"] {
        assert!(text.contains(clause), "no warning names {clause}: {text}");
    }
    // The one that costs something says what it costs.
    assert!(
        text.contains("9223372036854775807"),
        "the UNSIGNED warning must name the value that stops round-tripping: {text}"
    );

    // The table is real, and the key really does auto-assign.
    let (affected, insert_id) = client
        .ok_query("insert into `users` (`name`, `email`) values ('ada', 'ada@example.com')")
        .ok();
    assert_eq!(affected, 1);
    assert_eq!(insert_id, 1, "AUTO_INCREMENT's whole point");

    let (_, second) = client
        .ok_query("insert into `users` (`name`, `email`) values ('grace', 'grace@example.com')")
        .ok();
    assert_eq!(second, 2, "the counter keeps going");

    let rows = client
        .ok_query("select `id`, `name` from `users` order by `id` asc")
        .rows();
    assert_eq!(rows.columns, vec!["id", "name"]);
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.cell(0, 0), "1");
    assert_eq!(rows.cell(0, 1), "ada");
    assert_eq!(rows.cell(1, 0), "2");

    // And the shim's own metadata answers describe the table that was built.
    let columns = client.ok_query("SHOW COLUMNS FROM users").rows();
    assert_eq!(columns.column("Field"), vec!["id", "name", "email"]);
    assert_eq!(columns.column("Key"), vec!["PRI", "", ""]);
}

/// The statement a real migration writes, verbatim — `not null` on every
/// column, `timestamp` for the timestamps, and MySQL's DDL decoration.
///
/// This test used to assert that it *failed*, on `NOT NULL`, and that the
/// failure was the engine's gap rather than a syntax error. AHL-412 filled
/// those gaps, so it now asserts what it was always aiming at: the statement
/// runs, and the table it builds is usable. If a future change breaks this,
/// the regression is a real one.
#[test]
fn the_full_migration_statement_now_runs() {
    let server = TestServer::start("mysql-ddl-full");
    let mut client = server.client();

    let reply = client.ok_query(
        "create table `users` (\
           `id` bigint unsigned not null auto_increment primary key, \
           `name` varchar(255) not null, \
           `email_verified_at` timestamp null\
         ) engine=InnoDB default character set utf8mb4 collate 'utf8mb4_unicode_ci'",
    );

    // Every MySQL-only clause it dropped is reported rather than swallowed.
    assert!(
        reply.warnings() > 0,
        "the dropped clauses must be reported, not silently discarded"
    );
    let warnings = client.ok_query("SHOW WARNINGS").rows();
    let text = warnings
        .rows
        .iter()
        .map(|row| row[2].clone().unwrap_or_default().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    for clause in ["unsigned", "engine", "character set", "collate"] {
        assert!(text.contains(clause), "no warning named {clause}: {text}");
    }

    // NOT NULL is now enforced, not ignored: the whole point of implementing
    // it rather than dropping it.
    let error = client
        .query("insert into users (id, name) values (1, NULL)")
        .expect_err("name is NOT NULL");
    assert_eq!(error.code, 1048, "ER_BAD_NULL_ERROR");

    // And the table works.
    let reply = client.ok_query("insert into users (name) values ('ada')");
    assert_eq!(reply.ok(), (1, 1), "one row, row id 1");
    let rows = client
        .ok_query("select id, name, email_verified_at from users")
        .rows();
    assert_eq!(rows.cell(0, 0), "1");
    assert_eq!(rows.cell(0, 1), "ada");
    assert_eq!(rows.cell(0, 2), "NULL", "a nullable timestamp stays NULL");
}

/// The refusals. Each of these changes what the table means, so each has to
/// fail rather than be quietly dropped — a `CREATE TABLE` that reports success
/// while building something else is the failure mode this layer exists to
/// prevent. `1235` is the code, because a client branches on it to tell "not
/// implemented" from "your SQL is wrong".
#[test]
fn ddl_that_cannot_be_honoured_is_refused_and_the_error_names_it() {
    let server = TestServer::start("mysql-ddl-refusals");
    let mut client = server.client();

    let cases: &[(&str, &str)] = &[
        (
            "not declared PRIMARY KEY",
            "create table t1 (id bigint primary key, counter bigint auto_increment)",
        ),
        (
            "VARCHAR",
            "create table t2 (id varchar(36) auto_increment primary key)",
        ),
        (
            "ON UPDATE",
            "create table t3 (id bigint primary key, u timestamp on update current_timestamp)",
        ),
        (
            "CREATE INDEX",
            "create table t4 (id bigint primary key, name varchar(255), key t_name (`name`))",
        ),
        (
            "partition",
            "create table t5 (id bigint primary key) partition by hash (id)",
        ),
        (
            "ZEROFILL",
            "create table t6 (id bigint primary key, n int unsigned zerofill)",
        ),
    ];

    for (needle, sql) in cases {
        let error = client.query(sql).expect_err(sql);
        assert_eq!(error.code, 1235, "{sql} -> {error:?}");
        assert!(
            error.message.contains(needle),
            "{sql}: the message must name what it could not do, got {}",
            error.message
        );
    }

    // Every refusal left the connection usable and the schema untouched.
    let tables = client.ok_query("SHOW TABLES").rows();
    assert!(tables.rows.is_empty(), "got {:?}", tables.rows);
}

/// The translation has to happen on the prepared path too, or a driver using
/// native prepares sees a different database from one using text queries.
#[test]
fn a_prepared_create_table_is_translated_the_same_way() {
    let server = TestServer::start("mysql-ddl-prepared");
    let mut client = server.client();

    let create = client
        .prepare("create table `t` (`id` bigint unsigned auto_increment primary key) engine=InnoDB")
        .expect("prepare");
    let reply = client.execute(&create, &[]).expect("execute");
    assert_eq!(reply.warnings(), 3, "reported at execute, as MySQL does");
    client.close_statement(&create);

    let (_, insert_id) = client.ok_query("insert into `t` (`id`) values (null)").ok();
    assert_eq!(insert_id, 1);

    // A refusal on the prepared path is an error at prepare time, not a
    // statement that succeeds and does the wrong thing.
    let error = client
        .prepare("create table `u` (`id` bigint primary key, n bigint auto_increment)")
        .expect_err("prepare must refuse it");
    assert_eq!(error.code, 1235);
}

/// Warnings belong to the statement that raised them and to no other.
#[test]
fn warnings_are_cleared_by_the_next_statement() {
    let server = TestServer::start("mysql-ddl-warnings");
    let mut client = server.client();

    let reply = client.ok_query("create table t (id bigint unsigned primary key) engine=InnoDB");
    assert_eq!(reply.warnings(), 2);
    assert_eq!(client.ok_query("SHOW WARNINGS").rows().rows.len(), 2);
    // `SHOW WARNINGS` reads the list; it does not consume it.
    assert_eq!(client.ok_query("SHOW WARNINGS").rows().rows.len(), 2);

    let reply = client.ok_query("insert into t (id) values (1)");
    assert_eq!(reply.warnings(), 0);
    assert!(client.ok_query("SHOW WARNINGS").rows().rows.is_empty());
}

/// MySQL's online-DDL steering comes off an `ALTER TABLE`, and the statement
/// underneath it runs.
///
/// This asserted a failure until AHL-412 implemented `ALTER TABLE`. What is
/// still being tested is the shim's half — that `algorithm=`/`lock=`, which
/// steer *how* MySQL performs a change online and have no counterpart here,
/// are removed and reported rather than passed through to be a syntax error.
#[test]
fn online_ddl_steering_is_removed_and_the_alter_runs() {
    let server = TestServer::start("mysql-ddl-alter");
    let mut client = server.client();
    client.ok_query("create table t (id bigint primary key)");

    let reply = client.ok_query("alter table `t` add column `n` int, algorithm=inplace, lock=none");
    assert!(
        reply.warnings() > 0,
        "the steering clauses must be reported as dropped"
    );
    let text = client
        .ok_query("SHOW WARNINGS")
        .rows()
        .rows
        .iter()
        .map(|row| row[2].clone().unwrap_or_default().to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(text.contains("algorithm"), "no warning named it: {text}");
    assert!(text.contains("lock"), "no warning named it: {text}");

    // The column is really there.
    client.ok_query("insert into t (id, n) values (1, 7)");
    let rows = client.ok_query("select n from t where id = 1").rows();
    assert_eq!(rows.cell(0, 0), "7");
}

// =====================================================================
// Post-creation index and constraint DDL (AHL-474)
//
// AHL-471 found the first wall past `CREATE TABLE`: Laravel's schema builder
// never inlines `->unique()`/`->index()`/`->foreign()` into `CREATE TABLE` —
// it compiles a *separate* `ALTER TABLE ... ADD ...` right after, and none of
// MySQL's shapes for that were translated. This section is that wall, moved.
// =====================================================================

/// `ADD INDEX` (unnamed) and `ADD CONSTRAINT ... UNIQUE` both reach the
/// engine as free-standing `CREATE [UNIQUE] INDEX` statements, and neither
/// carries a warning — nothing about the table is different, only which
/// statement says so.
#[test]
fn add_index_and_add_unique_create_indexes_that_actually_work() {
    let server = TestServer::start("alter-add-index");
    let mut client = server.client();
    client.ok_query("create table users (id integer primary key, email text, age integer)");

    // `ADD INDEX`, unnamed: MySQL names it after the first column.
    let reply = client.ok_query("alter table users add index (age)");
    assert_eq!(
        reply.warnings(),
        0,
        "a repositioning is not a dropped clause"
    );

    // Laravel's own compiled form for `->unique()`.
    let reply =
        client.ok_query("alter table users add constraint users_email_unique unique (email)");
    assert_eq!(reply.warnings(), 0);

    let keys = client.ok_query("SHOW KEYS FROM users").rows();
    let names = keys.column("Key_name");
    let non_unique = keys.column("Non_unique");
    let index_type = keys.column("Index_type");
    let age_at = names
        .iter()
        .position(|n| n == "age")
        .expect("age index listed");
    assert_eq!(non_unique[age_at], "1", "a plain index is not a constraint");
    assert_eq!(index_type[age_at], "BTREE");
    let unique_at = names
        .iter()
        .position(|n| n == "users_email_unique")
        .expect("unique index listed");
    assert_eq!(
        non_unique[unique_at], "0",
        "ADD CONSTRAINT ... UNIQUE really is one"
    );

    // The unique index is a real constraint, not cosmetic.
    client.ok_query("insert into users (id, email, age) values (1, 'ada@example.com', 30)");
    let error = client
        .query("insert into users (id, email, age) values (2, 'ada@example.com', 40)")
        .expect_err("duplicate email");
    assert_eq!(error.code, 1062, "ER_DUP_ENTRY");

    // And the plain index is a real B-tree, usable by an ordinary lookup.
    let rows = client
        .ok_query("select id from users where age = 30")
        .rows();
    assert_eq!(rows.column("id"), vec!["1"]);
}

/// A `->unique()` compiled onto a column whose *table* carried a `_ci`
/// collation has to fold case the same way `WHERE` already does (AHL-469) —
/// `plan_create_index` takes a `CREATE UNIQUE INDEX`'s collation from the
/// column by default, so nothing extra has to be written for this to be
/// true, but it has to be asserted rather than assumed.
#[test]
fn add_unique_on_a_nocase_column_collides_case_insensitively() {
    let server = TestServer::start("alter-unique-collation");
    let mut client = server.client();
    client.ok_query(
        "create table `people` (`id` bigint unsigned auto_increment primary key, \
         `name` varchar(255)) default charset=utf8mb4 collate=utf8mb4_unicode_ci",
    );

    let reply = client.ok_query("alter table people add unique `people_name_unique` (`name`)");
    assert_eq!(
        reply.warnings(),
        0,
        "the index carries the column's own collation; nothing here is lost"
    );

    client.ok_query("insert into people (name) values ('Ada')");
    // The exact bug AHL-469 closed for `WHERE`, proven again for a
    // constraint built after the table already existed: `'Ada'` and `'ADA'`
    // collide the way they do in MySQL under `utf8mb4_unicode_ci`.
    let error = client
        .query("insert into people (name) values ('ADA')")
        .expect_err("NOCASE carried into the unique index");
    assert_eq!(error.code, 1062, "ER_DUP_ENTRY");
}

/// A composite `ADD INDEX` still names itself after only the first column.
#[test]
fn a_composite_add_index_is_named_after_its_first_column() {
    let server = TestServer::start("alter-composite-index");
    let mut client = server.client();
    client.ok_query("create table events (id integer primary key, kind text, at text)");
    client.ok_query("alter table events add index (kind, at)");

    assert!(client
        .ok_query("SHOW KEYS FROM events")
        .rows()
        .column("Key_name")
        .contains(&"kind".to_string()));
}

/// `ADD CONSTRAINT ... FOREIGN KEY` has nowhere in core to be recorded once
/// the table already exists, so it answers OK rather than the `1235` every
/// other unrepresentable DDL clause gets — but never silently: a `1618`
/// names exactly what was not recorded, and the constraint really is
/// unenforced, SQLite's own long-standing default rather than a gap this
/// server introduced.
#[test]
fn add_constraint_foreign_key_is_ok_with_a_warning_and_stays_unenforced() {
    let server = TestServer::start("alter-add-fk");
    let mut client = server.client();
    client.ok_query("create table roles (id integer primary key, name text)");
    client.ok_query("create table users (id integer primary key, role_id integer)");

    let reply = client.ok_query(
        "alter table users add constraint users_role_id_foreign foreign key (role_id) \
         references roles (id)",
    );
    assert_eq!(
        reply.warnings(),
        1,
        "nothing here can be recorded, and that has to be said"
    );
    let warnings = client.ok_query("SHOW WARNINGS").rows();
    assert_eq!(warnings.rows.len(), 1);
    assert_eq!(warnings.column("Code"), vec!["1618"]);
    let message = warnings.column("Message")[0].to_ascii_lowercase();
    assert!(message.contains("foreign key"), "{message}");
    assert!(message.contains("not recorded"), "{message}");

    // Unenforced, exactly as a foreign key written inline at `CREATE TABLE`
    // already is: a row referencing a role that does not exist is accepted
    // rather than refused.
    let reply = client.ok_query("insert into users (id, role_id) values (1, 999)");
    assert_eq!(reply.ok(), (1, 0));
}

/// `DROP INDEX`/`DROP KEY` inside an `ALTER TABLE` becomes the standalone
/// `DROP INDEX` — no table qualifier, because SQLite's index names are
/// global and MySQL's per-table qualifier has nowhere to go.
#[test]
fn drop_index_removes_it_and_the_table_still_works() {
    let server = TestServer::start("alter-drop-index");
    let mut client = server.client();
    client.ok_query("create table users (id integer primary key, age integer)");
    client.ok_query("alter table users add index users_age_index (age)");
    assert!(client
        .ok_query("SHOW KEYS FROM users")
        .rows()
        .column("Key_name")
        .contains(&"users_age_index".to_string()));

    let reply = client.ok_query("alter table users drop index users_age_index");
    assert_eq!(reply.warnings(), 0);
    assert!(!client
        .ok_query("SHOW KEYS FROM users")
        .rows()
        .column("Key_name")
        .contains(&"users_age_index".to_string()));

    // The table is still usable; dropping the index did not touch the data.
    client.ok_query("insert into users (id, age) values (1, 30)");
    let rows = client.ok_query("select age from users where id = 1").rows();
    assert_eq!(rows.cell(0, 0), "30");
}

/// `RENAME INDEX` has no counterpart in core at all — only drop-and-recreate,
/// a different statement with a window where the index does not exist — so
/// it is refused outright rather than silently becoming two statements.
#[test]
fn rename_index_is_refused_over_the_wire() {
    let server = TestServer::start("alter-rename-index");
    let mut client = server.client();
    client.ok_query("create table users (id integer primary key, age integer)");
    client.ok_query("alter table users add index users_age_index (age)");

    let error = client
        .query("alter table users rename index users_age_index to users_age_idx")
        .expect_err("no rename path in core");
    assert_eq!(error.code, 1235);
    assert!(error.message.contains("RENAME INDEX"), "{}", error.message);

    // The connection, and the original index, are still there.
    assert!(client
        .ok_query("SHOW KEYS FROM users")
        .rows()
        .column("Key_name")
        .contains(&"users_age_index".to_string()));
}

/// MySQL's comma-separated `ALTER TABLE` operations become one statement per
/// operation, run in sequence — and that sequence is genuinely **not
/// atomic**: an earlier operation's effect survives a later one's failure,
/// exactly as if the client had sent them as separate statements itself.
#[test]
fn a_multi_operation_alter_runs_each_operation_and_is_not_atomic_on_failure() {
    let server = TestServer::start("alter-multi-op");
    let mut client = server.client();
    client.ok_query("create table users (id integer primary key)");

    // Every operation succeeds: the column really is there, and so is the
    // index built on it.
    let reply = client.ok_query("alter table users add column age int, add index (age)");
    assert_eq!(reply.warnings(), 0);
    client.ok_query("insert into users (id, age) values (1, 30)");
    let rows = client.ok_query("select age from users where id = 1").rows();
    assert_eq!(rows.cell(0, 0), "30");
    assert!(client
        .ok_query("SHOW KEYS FROM users")
        .rows()
        .column("Key_name")
        .contains(&"age".to_string()));

    // Now a statement whose *second* operation cannot succeed — an index on
    // a column that does not exist. The first operation's `ADD COLUMN`
    // already ran and committed before the second was even attempted, so it
    // stays.
    let error = client
        .query("alter table users add column score int, add index (nope)")
        .expect_err("no such column");
    assert_eq!(error.code, 1054, "{error:?}");

    // The column the first, successful operation added is still there —
    // proof this was not rolled back the way one atomic MySQL statement
    // would have been.
    let rows = client
        .ok_query("select score from users where id = 1")
        .rows();
    assert_eq!(rows.cell(0, 0), "NULL");
}

/// `TRUNCATE TABLE` becomes `DELETE FROM`, and always carries a `1618`: read
/// literally, `docs/server.md`'s row-id divergence note says the counter
/// cannot be seeded or rewound, so unlike MySQL's own `TRUNCATE` this does
/// not restart it at 1 — the next row keeps numbering from where the table
/// already was, exactly as a plain MySQL `DELETE` would too.
#[test]
fn truncate_table_deletes_rows_and_does_not_reset_the_row_id_counter() {
    let server = TestServer::start("truncate");
    let mut client = server.client();
    client.ok_query("create table users (id integer primary key, name text)");
    client.ok_query("insert into users (name) values ('ada')");
    client.ok_query("insert into users (name) values ('grace')");

    let reply = client.ok_query("truncate table users");
    assert_eq!(
        reply.warnings(),
        1,
        "the row-id divergence must be reported"
    );
    let warnings = client.ok_query("SHOW WARNINGS").rows();
    assert_eq!(warnings.column("Code"), vec!["1618"]);
    let message = warnings.column("Message")[0].to_ascii_lowercase();
    assert!(message.contains("row id"), "{message}");

    let rows = client.ok_query("select * from users").rows();
    assert!(rows.rows.is_empty(), "TRUNCATE removes every row");

    // The counter kept going rather than restarting at 1 — the one guarantee
    // that does not survive becoming a plain `DELETE`.
    let (_, insert_id) = client
        .ok_query("insert into users (name) values ('hopper')")
        .ok();
    assert_eq!(insert_id, 3, "not reset to 1");
}

/// MySQL also accepts `TRUNCATE` with no `TABLE` keyword.
#[test]
fn truncate_without_the_table_keyword_works_too() {
    let server = TestServer::start("truncate-no-table-keyword");
    let mut client = server.client();
    client.ok_query("create table users (id integer primary key)");
    client.ok_query("insert into users (id) values (1)");
    client.ok_query("truncate users");
    assert!(client
        .ok_query("select * from users")
        .rows()
        .rows
        .is_empty());
}

/// The standalone `RENAME TABLE` becomes `ALTER TABLE ... RENAME TO`, so
/// nothing here warns — a pure rename loses nothing.
#[test]
fn standalone_rename_table_renames_it() {
    let server = TestServer::start("rename-table");
    let mut client = server.client();
    client.ok_query("create table users (id integer primary key, name text)");
    client.ok_query("insert into users (name) values ('ada')");

    let reply = client.ok_query("rename table users to people");
    assert_eq!(reply.warnings(), 0, "a pure rename loses nothing");

    let tables = client
        .ok_query("SHOW TABLES")
        .rows()
        .column("Tables_in_inlaysql");
    assert!(!tables.contains(&"users".to_string()));
    assert!(tables.contains(&"people".to_string()));

    let rows = client.ok_query("select name from people").rows();
    assert_eq!(rows.cell(0, 0), "ada");
}

/// The corpus AHL-471 found refusing every one of these: `CREATE TABLE` with
/// its usual decoration, then a separate `ADD INDEX`, `ADD UNIQUE`, `ADD
/// CONSTRAINT ... FOREIGN KEY` and `DROP INDEX`, in the order a real Laravel
/// migration sends them. Every statement succeeds, every warning is visible
/// through `SHOW WARNINGS`, and what is left standing actually works.
#[test]
fn a_realistic_laravel_migration_sequence_runs_end_to_end() {
    let server = TestServer::start("laravel-migration");
    let mut client = server.client();

    // migrations/..._create_roles_table.php
    client.ok_query(
        "create table `roles` (`id` bigint unsigned auto_increment primary key, \
         `name` varchar(255) not null) engine=InnoDB default charset=utf8mb4 \
         collate=utf8mb4_unicode_ci",
    );

    // migrations/..._create_users_table.php
    let reply = client.ok_query(
        "create table `users` (`id` bigint unsigned auto_increment primary key, \
         `email` varchar(255) not null, `role_id` bigint unsigned not null) \
         engine=InnoDB default charset=utf8mb4 collate=utf8mb4_unicode_ci",
    );
    assert!(reply.warnings() > 0);

    // $table->index('role_id');
    let reply = client.ok_query("alter table `users` add index `users_role_id_index` (`role_id`)");
    assert_eq!(reply.warnings(), 0);

    // $table->unique('email');
    let reply = client.ok_query("alter table `users` add unique `users_email_unique` (`email`)");
    assert_eq!(reply.warnings(), 0);

    // $table->foreign('role_id')->references('id')->on('roles');
    let reply = client.ok_query(
        "alter table `users` add constraint `users_role_id_foreign` foreign key (`role_id`) \
         references `roles` (`id`)",
    );
    assert_eq!(reply.warnings(), 1);
    let warnings = client.ok_query("SHOW WARNINGS").rows();
    assert_eq!(warnings.column("Code"), vec!["1618"]);
    assert!(warnings.column("Message")[0]
        .to_ascii_lowercase()
        .contains("foreign key"));

    // A later migration drops the plain index and keeps the unique one.
    let reply = client.ok_query("alter table `users` drop index `users_role_id_index`");
    assert_eq!(reply.warnings(), 0);

    let keys = client
        .ok_query("SHOW KEYS FROM users")
        .rows()
        .column("Key_name");
    assert!(keys.contains(&"users_email_unique".to_string()));
    assert!(!keys.contains(&"users_role_id_index".to_string()));

    // Every index left standing is real and enforces what it says.
    client.ok_query("insert into roles (name) values ('admin')");
    client.ok_query("insert into users (email, role_id) values ('ada@example.com', 1)");
    let error = client
        .query("insert into users (email, role_id) values ('ada@example.com', 1)")
        .expect_err("unique email");
    assert_eq!(error.code, 1062);

    // The unenforced foreign key really is unenforced: a role that does not
    // exist is accepted, exactly as the warning said it would be.
    client.ok_query("insert into users (email, role_id) values ('grace@example.com', 999)");
}

/// AHL-474 closed the post-creation DDL wall; the statement right after it in
/// a stock Eloquent traffic sequence was this one — `UPDATE ... SET name = ?,
/// users.updated_at = ?`, a qualified column beside a bare one, which is what
/// Eloquent's query builder compiles for `$model->save()` on any model with
/// timestamps. Prepared and bound, the way a real client runs it, not sent as
/// a text query.
#[test]
fn an_eloquent_style_model_save_updates_the_qualified_column() {
    let server = TestServer::start("eloquent-save");
    let mut client = server.client();
    client.ok_query(
        "create table `users` (`id` bigint unsigned auto_increment primary key, \
         `name` varchar(255) not null, `updated_at` timestamp null) engine=InnoDB \
         default charset=utf8mb4",
    );
    client.ok_query(
        "insert into `users` (`name`, `updated_at`) values ('ada', '2024-01-01 00:00:00')",
    );

    let update = client
        .prepare("UPDATE users SET name = ?, users.updated_at = ? WHERE users.id = ?")
        .expect("prepare");
    assert_eq!(update.param_count, 3);
    let (affected, _) = client
        .execute(
            &update,
            &[
                Param::Str("grace".to_string()),
                Param::Str("2024-06-01 12:00:00".to_string()),
                Param::Int(1),
            ],
        )
        .expect("execute update")
        .ok();
    assert_eq!(affected, 1);

    let rows = client
        .ok_query("select `name`, `updated_at` from `users` where `id` = 1")
        .rows();
    assert_eq!(rows.cell(0, 0), "grace");
    assert_eq!(rows.cell(0, 1), "2024-06-01 12:00:00");
}

/// The backtick-quoted spelling a schema-builder-generated model actually
/// sends — Eloquent's MySQL grammar wraps every identifier.
#[test]
fn a_backtick_quoted_qualified_set_target_works_over_the_wire() {
    let server = TestServer::start("eloquent-save-backticks");
    let mut client = server.client();
    client.ok_query(
        "create table `users` (`id` bigint unsigned auto_increment primary key, \
         `name` varchar(255) not null, `updated_at` timestamp null)",
    );
    client.ok_query("insert into `users` (`name`) values ('ada')");

    let reply = client.ok_query(
        "update `users` set `name` = 'grace', `users`.`updated_at` = '2024-06-01 12:00:00' \
         where `users`.`id` = 1",
    );
    assert_eq!(reply.ok().0, 1);

    let rows = client
        .ok_query("select `name`, `updated_at` from `users`")
        .rows();
    assert_eq!(rows.cell(0, 0), "grace");
    assert_eq!(rows.cell(0, 1), "2024-06-01 12:00:00");
}

/// A qualifier naming a real table that is not the statement's own is
/// refused, by name, rather than silently mis-resolved or passed through for
/// core's generic "no schemas" error.
#[test]
fn a_qualified_set_target_naming_another_table_is_refused_over_the_wire() {
    let server = TestServer::start("eloquent-save-wrong-qualifier");
    let mut client = server.client();
    client.ok_query("create table `roles` (`id` bigint unsigned auto_increment primary key)");
    client.ok_query(
        "create table `users` (`id` bigint unsigned auto_increment primary key, \
         `name` varchar(255) not null)",
    );

    let error = client
        .query("update `users` set `name` = 'grace', `roles`.`name` = 'x' where `id` = 1")
        .unwrap_err();
    assert_eq!(error.code, 1109, "ER_UNKNOWN_TABLE");
    assert_eq!(error.sqlstate, "42S02");
    assert!(error.message.contains("roles"), "{}", error.message);
}

// =====================================================================
// INSERT ... ON DUPLICATE KEY UPDATE (AHL-476)
//
// AHL-475 closed the qualified `UPDATE ... SET` wall; the statement left
// standing behind it was MySQL's own upsert syntax, which core refused by
// name (`ON DUPLICATE KEY UPDATE is MySQL syntax; write ON CONFLICT ... DO
// UPDATE`). `crate::mysqlddl::insert_on_duplicate_key_update` rewrites it
// onto core's own `ON CONFLICT DO UPDATE SET ...` with `excluded.col`, with
// no conflict target at all — see that function's docs for why a bare
// `ON CONFLICT DO UPDATE` is the exact mapping, not a narrower one, verified
// against a real `sqlite3` binary and against `inlaysql-core` directly.
// =====================================================================

/// Eloquent's `upsert()` compiles to exactly this: a multi-row `INSERT`, one
/// `VALUES(col)` per updated column, and — because Eloquent's MySQL grammar
/// backtick-quotes every identifier it writes, including the function name —
/// `` `values`(`col`) ``, not the bare spelling. Prepared and bound, the way
/// a real client sends it. One proposed row is new (the insert path); one
/// collides on the table's unique key (the update path), so both of
/// `ON DUPLICATE KEY UPDATE`'s two outcomes run in the same statement.
#[test]
fn eloquents_upsert_runs_the_insert_path_and_the_update_path_together() {
    let server = TestServer::start("upsert-eloquent");
    let mut client = server.client();
    client.ok_query(
        "create table `products` (`id` bigint unsigned auto_increment primary key, \
         `sku` varchar(255) not null unique, `price` int not null)",
    );
    client.ok_query("insert into `products` (`sku`, `price`) values ('widget', 100)");

    let upsert = client
        .prepare(
            "insert into `products` (`sku`, `price`) values (?, ?), (?, ?) on duplicate key \
             update `price` = `values`(`price`)",
        )
        .expect("prepare upsert");
    assert_eq!(upsert.param_count, 4);

    let (affected, _) = client
        .execute(
            &upsert,
            &[
                Param::Str("widget".to_string()), // collides: update path
                Param::Int(150),
                Param::Str("gadget".to_string()), // new: insert path
                Param::Int(200),
            ],
        )
        .expect("execute upsert")
        .ok();
    // Not MySQL's 0/1/2 convention (see docs/server.md): this server reports
    // one count per row the engine actually wrote, insert or update alike —
    // one of each is 2 here, not MySQL's 2 (updated) + 1 (inserted) = 3.
    assert_eq!(affected, 2);

    let rows = client
        .ok_query("select `sku`, `price` from `products` order by `sku`")
        .rows();
    assert_eq!(rows.cell(0, 0), "gadget");
    assert_eq!(rows.cell(0, 1), "200");
    assert_eq!(rows.cell(1, 0), "widget");
    assert_eq!(rows.cell(1, 1), "150");
}

/// `updateOrCreate()`'s shape: a single-row upsert, run twice — once for a
/// key that already exists (the update path) and once for a key that does
/// not (the insert path) — with the bare, unquoted `VALUES(col)` spelling
/// every MySQL client can send even where Eloquent's own grammar happens to
/// quote it.
#[test]
fn a_single_row_upsert_updates_an_existing_key_and_inserts_a_new_one() {
    let server = TestServer::start("upsert-single-row");
    let mut client = server.client();
    client.ok_query(
        "create table `settings` (`id` bigint unsigned auto_increment primary key, \
         `name` varchar(255) not null unique, `value` varchar(255) not null)",
    );
    client.ok_query("insert into `settings` (`name`, `value`) values ('theme', 'light')");

    let upsert = client
        .prepare(
            "insert into `settings` (`name`, `value`) values (?, ?) on duplicate key update \
             `value` = VALUES(`value`)",
        )
        .expect("prepare upsert");
    assert_eq!(upsert.param_count, 2);

    // The update path: `theme` already exists.
    let (affected, insert_id) = client
        .execute(
            &upsert,
            &[
                Param::Str("theme".to_string()),
                Param::Str("dark".to_string()),
            ],
        )
        .expect("execute update path")
        .ok();
    assert_eq!(affected, 1);
    assert_eq!(insert_id, 0, "an updated row generates no id");
    assert_eq!(
        client
            .ok_query("select `value` from `settings` where `name` = 'theme'")
            .rows()
            .cell(0, 0),
        "dark"
    );

    // The insert path: `locale` does not exist yet.
    let (affected, insert_id) = client
        .execute(
            &upsert,
            &[
                Param::Str("locale".to_string()),
                Param::Str("en".to_string()),
            ],
        )
        .expect("execute insert path")
        .ok();
    assert_eq!(affected, 1);
    assert_ne!(insert_id, 0, "a freshly assigned key is reported");
    assert_eq!(
        client
            .ok_query("select `value` from `settings` where `name` = 'locale'")
            .rows()
            .cell(0, 0),
        "en"
    );
}

/// MySQL 8.0.20+'s row-alias spelling in place of `VALUES(col)`: `... AS new
/// ON DUPLICATE KEY UPDATE col = new.col`. Eloquent does not send this form,
/// but this server claims to translate it, so it is checked the same way.
#[test]
fn the_mysql_8_0_20_row_alias_form_also_translates() {
    let server = TestServer::start("upsert-row-alias");
    let mut client = server.client();
    client.ok_query("create table `counters` (`name` varchar(255) primary key, `n` int not null)");
    client.ok_query("insert into `counters` (`name`, `n`) values ('hits', 1)");

    let reply = client.ok_query(
        "insert into `counters` (`name`, `n`) values ('hits', 41) as new on duplicate key \
         update `n` = `n` + new.n",
    );
    assert_eq!(reply.ok().0, 1);
    assert_eq!(
        client
            .ok_query("select `n` from `counters` where `name` = 'hits'")
            .rows()
            .cell(0, 0),
        "42"
    );
}

/// No collision at all: `ON DUPLICATE KEY UPDATE` never fires, and the row
/// is inserted exactly as an ordinary `INSERT` would leave it.
#[test]
fn on_duplicate_key_update_never_fires_without_a_real_collision() {
    let server = TestServer::start("upsert-no-collision");
    let mut client = server.client();
    client.ok_query(
        "create table `products` (`id` bigint unsigned auto_increment primary key, \
         `sku` varchar(255) not null unique, `price` int not null)",
    );

    let (affected, insert_id) = client
        .ok_query(
            "insert into `products` (`sku`, `price`) values ('widget', 100) on duplicate key \
             update `price` = VALUES(`price`)",
        )
        .ok();
    assert_eq!(affected, 1);
    assert_ne!(insert_id, 0);
}

/// The crux this fix rests on, checked end to end: a table with *two*
/// separate unique constraints gets no ambiguity error. MySQL's own clause
/// has no conflict target either — it fires on a collision with any unique
/// or primary key — so this server's translation adds none, and core's own
/// targetless `ON CONFLICT DO UPDATE` answers for whichever one collided.
#[test]
fn a_table_with_two_unique_constraints_is_not_refused_as_ambiguous() {
    let server = TestServer::start("upsert-multi-unique");
    let mut client = server.client();
    client.ok_query(
        "create table `users` (`id` bigint unsigned auto_increment primary key, \
         `email` varchar(255) not null, `name` varchar(255) not null)",
    );
    client.ok_query("alter table `users` add unique `users_email_unique` (`email`)");
    client.ok_query("alter table `users` add unique `users_name_unique` (`name`)");
    client.ok_query("insert into `users` (`email`, `name`) values ('ada@example.com', 'ada')");

    // Collides on `email`; `name` is fresh. Neither constraint was named, and
    // this must not be refused as ambiguous.
    let reply = client.ok_query(
        "insert into `users` (`email`, `name`) values ('ada@example.com', 'countess') \
         on duplicate key update `name` = VALUES(`name`)",
    );
    assert_eq!(reply.ok().0, 1);
    assert_eq!(
        client
            .ok_query("select `name` from `users` where `email` = 'ada@example.com'")
            .rows()
            .cell(0, 0),
        "countess"
    );
}

/// `ON DUPLICATE KEY UPDATE` with nothing after it is a syntax error, not a
/// silently accepted no-op — and the connection stays usable afterward.
#[test]
fn an_empty_on_duplicate_key_update_is_refused_over_the_wire() {
    let server = TestServer::start("upsert-empty-clause");
    let mut client = server.client();
    client.ok_query("create table `t` (`id` bigint unsigned auto_increment primary key)");

    let error = client
        .query("insert into `t` (`id`) values (1) on duplicate key update")
        .unwrap_err();
    assert_eq!(error.code, 1064);

    // Still usable, and the refused statement wrote nothing.
    client.ok_query("insert into `t` (`id`) values (2)");
    let rows = client.ok_query("select count(*) from `t`").rows();
    assert_eq!(rows.cell(0, 0), "1");
}

// =====================================================================
// MySQL-named scalar functions
//
// Every expected value below was read off **MySQL 8.4.11** running the same
// statement, not off this server. An expectation copied from the engine under
// test would only prove it agrees with itself.
//
// The reference MySQL had its session time zone set to UTC and its table
// declared `COLLATE utf8mb4_bin`, so the two differences these mappings cannot
// remove — the engine's clock is UTC, and it compares text byte for byte — do
// not mask a mistake in the mapping itself. Both are named in
// `docs/server.md`, Divergences, and one of them is asserted below.
// =====================================================================

/// Two rows and a NULL, so every assertion below covers the NULL case as well
/// as the value case.
fn function_fixture(name: &str) -> (TestServer, Client) {
    let server = TestServer::start(name);
    let mut client = server.client();
    client.ok_query(
        "create table items (id integer primary key, name text, n integer, created text)",
    );
    client.ok_query(
        "insert into items (id, name, n, created) values (1, 'hello', 3, '2024-01-15 10:20:30')",
    );
    client.ok_query(
        "insert into items (id, name, n, created) values (2, 'héllo wörld', 0, '2024-02-29 00:00:00')",
    );
    client.ok_query("insert into items (id, name, n, created) values (3, NULL, NULL, NULL)");
    (server, client)
}

/// One value from a table-less `SELECT`, which is how MySQL's own answers were
/// taken.
fn value(client: &mut Client, expression: &str) -> String {
    client
        .ok_query(&format!("SELECT {expression}"))
        .rows()
        .cell(0, 0)
}

#[test]
fn the_mysql_named_string_functions_answer_what_mysql_answers() {
    let (_server, mut client) = function_fixture("mysql-fn-string");

    // CONCAT propagates NULL in MySQL too — CONCAT('a', NULL, 'c') is NULL,
    // not 'ac'. This is the corner the mapping exists to get right.
    assert_eq!(value(&mut client, "CONCAT('a','b','c')"), "abc");
    assert_eq!(value(&mut client, "CONCAT('a',NULL,'c')"), "NULL");
    assert_eq!(value(&mut client, "CONCAT(1,2)"), "12");
    assert_eq!(value(&mut client, "CONCAT('x')"), "x");
    assert_eq!(value(&mut client, "CONCAT('a','','b')"), "ab");

    assert_eq!(value(&mut client, "CHAR_LENGTH('hello')"), "5");
    assert_eq!(value(&mut client, "CHAR_LENGTH('héllo')"), "5");
    assert_eq!(value(&mut client, "CHAR_LENGTH('a😀b')"), "3");
    assert_eq!(value(&mut client, "CHAR_LENGTH('')"), "0");
    assert_eq!(value(&mut client, "CHAR_LENGTH(NULL)"), "NULL");
    assert_eq!(value(&mut client, "CHARACTER_LENGTH('abc')"), "3");

    assert_eq!(value(&mut client, "UCASE('hello')"), "HELLO");
    assert_eq!(value(&mut client, "LCASE('HELLO')"), "hello");
    assert_eq!(value(&mut client, "UCASE(NULL)"), "NULL");

    // LOCATE(needle, haystack) is the reverse of instr(haystack, needle). If
    // the swap is ever dropped, this is the assertion that catches it — and
    // note that it would otherwise fail *silently*, as a 0.
    assert_eq!(value(&mut client, "LOCATE('ll','hello')"), "3");
    assert_eq!(value(&mut client, "LOCATE('z','hello')"), "0");
    assert_eq!(value(&mut client, "LOCATE('','hello')"), "1");
    assert_eq!(value(&mut client, "LOCATE('a','')"), "0");
    assert_eq!(value(&mut client, "LOCATE(NULL,'hello')"), "NULL");
    assert_eq!(value(&mut client, "LOCATE('a',NULL)"), "NULL");
    assert_eq!(value(&mut client, "LOCATE('l','héllo')"), "3");
    assert_eq!(value(&mut client, "POSITION('ll' IN 'hello')"), "3");

    // LEFT and RIGHT at every boundary MySQL has one at.
    assert_eq!(value(&mut client, "LEFT('hello',3)"), "hel");
    assert_eq!(value(&mut client, "LEFT('hello',0)"), "");
    assert_eq!(value(&mut client, "LEFT('hello',-1)"), "");
    assert_eq!(value(&mut client, "LEFT('hello',100)"), "hello");
    assert_eq!(value(&mut client, "LEFT(NULL,3)"), "NULL");
    assert_eq!(value(&mut client, "LEFT('héllo',2)"), "hé");
    assert_eq!(value(&mut client, "RIGHT('hello',2)"), "lo");
    // The one that `substr(s, -n)` alone would get wrong: MySQL answers the
    // empty string here, and substr('hello', 0) is the whole string.
    assert_eq!(value(&mut client, "RIGHT('hello',0)"), "");
    assert_eq!(value(&mut client, "RIGHT('hello',-1)"), "");
    assert_eq!(value(&mut client, "RIGHT('hello',100)"), "hello");
    assert_eq!(value(&mut client, "RIGHT(NULL,3)"), "NULL");
    assert_eq!(value(&mut client, "RIGHT('héllo',4)"), "éllo");

    assert_eq!(value(&mut client, "TRIM('  hi  ')"), "hi");
    assert_eq!(value(&mut client, "TRIM(BOTH FROM '  hi  ')"), "hi");
    assert_eq!(value(&mut client, "TRIM(LEADING FROM '  hi  ')"), "hi  ");
    assert_eq!(value(&mut client, "TRIM(TRAILING FROM '  hi  ')"), "  hi");
    assert_eq!(value(&mut client, "TRIM(BOTH FROM NULL)"), "NULL");
}

#[test]
fn the_mysql_named_conditional_functions_answer_what_mysql_answers() {
    let (_server, mut client) = function_fixture("mysql-fn-cond");

    assert_eq!(value(&mut client, "ISNULL(NULL)"), "1");
    assert_eq!(value(&mut client, "ISNULL(1)"), "0");
    assert_eq!(value(&mut client, "ISNULL(NULL) + 1"), "2");

    // MySQL's truthiness, which is not a boolean's: 'abc' is false and '1' is
    // true, and a NULL condition takes the else branch.
    assert_eq!(value(&mut client, "IF(1,'y','n')"), "y");
    assert_eq!(value(&mut client, "IF(0,'y','n')"), "n");
    assert_eq!(value(&mut client, "IF(NULL,'y','n')"), "n");
    assert_eq!(value(&mut client, "IF('abc','y','n')"), "n");
    assert_eq!(value(&mut client, "IF('1','y','n')"), "y");
    assert_eq!(value(&mut client, "IF(2>1,'y','n')"), "y");

    assert_eq!(value(&mut client, "COALESCE('q')"), "q");
    assert_eq!(value(&mut client, "COALESCE(NULL)"), "NULL");
    assert_eq!(value(&mut client, "COALESCE(NULL,NULL,'z')"), "z");
    assert_eq!(value(&mut client, "IFNULL(NULL,'x')"), "x");
}

#[test]
fn the_mysql_named_date_functions_answer_what_mysql_answers() {
    let (_server, mut client) = function_fixture("mysql-fn-date");

    assert_eq!(value(&mut client, "YEAR('2024-01-15')"), "2024");
    assert_eq!(value(&mut client, "YEAR('2024-01-15 10:20:30')"), "2024");
    assert_eq!(value(&mut client, "YEAR(NULL)"), "NULL");
    // An integer, not the zero-padded text strftime answers with.
    assert_eq!(value(&mut client, "MONTH('2024-03-05')"), "3");
    assert_eq!(value(&mut client, "MONTH('2024-01-15') + 1"), "2");
    assert_eq!(value(&mut client, "DAY('2024-01-05')"), "5");
    assert_eq!(value(&mut client, "DAYOFMONTH('2024-01-15')"), "15");
    assert_eq!(value(&mut client, "HOUR('2024-01-15 10:20:30')"), "10");
    assert_eq!(value(&mut client, "MINUTE('2024-01-15 10:20:30')"), "20");
    assert_eq!(value(&mut client, "SECOND('2024-01-15 10:20:30')"), "30");

    // 2024-01-15 is a Monday. MySQL's DAYOFWEEK counts from Sunday = 1 and its
    // WEEKDAY counts from Monday = 0; SQLite's `%w` does neither.
    assert_eq!(value(&mut client, "DAYOFWEEK('2024-01-15')"), "2");
    assert_eq!(value(&mut client, "DAYOFWEEK('2024-01-14')"), "1");
    assert_eq!(value(&mut client, "DAYOFWEEK('2024-01-13')"), "7");
    assert_eq!(value(&mut client, "WEEKDAY('2024-01-15')"), "0");
    assert_eq!(value(&mut client, "WEEKDAY('2024-01-14')"), "6");
    assert_eq!(value(&mut client, "WEEKDAY('2024-01-13')"), "5");
    assert_eq!(value(&mut client, "DAYOFWEEK(NULL)"), "NULL");
    assert_eq!(value(&mut client, "WEEKDAY(NULL)"), "NULL");

    assert_eq!(value(&mut client, "DAYOFYEAR('2024-01-01')"), "1");
    assert_eq!(value(&mut client, "DAYOFYEAR('2024-03-01')"), "61");
    assert_eq!(value(&mut client, "DAYOFYEAR('2024-12-31')"), "366");

    assert_eq!(value(&mut client, "QUARTER('2024-01-31')"), "1");
    assert_eq!(value(&mut client, "QUARTER('2024-05-01')"), "2");
    assert_eq!(value(&mut client, "QUARTER('2024-09-30')"), "3");
    assert_eq!(value(&mut client, "QUARTER('2024-12-01')"), "4");
    assert_eq!(value(&mut client, "QUARTER(NULL)"), "NULL");

    assert_eq!(value(&mut client, "LAST_DAY('2024-02-05')"), "2024-02-29");
    assert_eq!(value(&mut client, "LAST_DAY('2023-02-05')"), "2023-02-28");
    assert_eq!(value(&mut client, "LAST_DAY('2024-12-31')"), "2024-12-31");
    assert_eq!(value(&mut client, "LAST_DAY(NULL)"), "NULL");
}

/// AHL-465: `LENGTH`, `HEX`, `SUBSTRING`, `NULLIF` and `ROUND` used to be
/// left alone because their spelling is identical in both dialects and the
/// shim had no way to tell which one a caller meant — so they resolved in
/// the engine under SQLite's own semantics. `docs/server.md`'s Divergences
/// section measured each corner against real MySQL 8.4.11; this test is
/// that same table, run over the wire, now that the shim rewrites all five
/// onto the primitives that give MySQL's answer instead.
#[test]
fn the_five_previously_divergent_functions_now_answer_what_mysql_answers() {
    let (_server, mut client) = function_fixture("mysql-fn-ahl465");

    // LENGTH counts bytes in MySQL, not characters: 'héllo' is 5 characters
    // and 6 bytes, because 'é' is two UTF-8 bytes.
    assert_eq!(value(&mut client, "LENGTH('héllo')"), "6");
    assert_eq!(value(&mut client, "LENGTH('hello')"), "5");
    assert_eq!(value(&mut client, "LENGTH(NULL)"), "NULL");
    // CHAR_LENGTH is unaffected — it already mapped onto the engine's own
    // character-counting length() — and still gives the character count.
    assert_eq!(value(&mut client, "CHAR_LENGTH('héllo')"), "5");

    // HEX(255) is the hex of the *value* in MySQL, not the bytes of the text
    // '255'; HEX(NULL) is NULL rather than the empty string.
    assert_eq!(value(&mut client, "HEX(255)"), "FF");
    assert_eq!(value(&mut client, "HEX(0)"), "0");
    assert_eq!(value(&mut client, "HEX(NULL)"), "NULL");
    assert_eq!(value(&mut client, "HEX('AB')"), "4142");

    // SUBSTRING's position-0 rule, a negative position past the start, a
    // non-positive length, and NULL propagation through either argument —
    // every row is a line of the Divergences table.
    assert_eq!(value(&mut client, "SUBSTRING('hello',0)"), "");
    assert_eq!(value(&mut client, "SUBSTRING('hello',0,3)"), "");
    assert_eq!(value(&mut client, "SUBSTRING('hello',-10)"), "");
    assert_eq!(value(&mut client, "SUBSTRING('hello',2,-1)"), "");
    assert_eq!(value(&mut client, "SUBSTRING('hello',1,NULL)"), "NULL");
    assert_eq!(value(&mut client, "SUBSTRING('hello',NULL,2)"), "NULL");
    // And the ordinary cases MySQL's own manual uses as examples.
    assert_eq!(value(&mut client, "SUBSTRING('hello',1)"), "hello");
    assert_eq!(value(&mut client, "SUBSTRING('hello',2,3)"), "ell");
    assert_eq!(value(&mut client, "SUBSTRING('hello',-3)"), "llo");
    // SUBSTR is the same function under MySQL's other name for it.
    assert_eq!(value(&mut client, "SUBSTR('hello',2,3)"), "ell");

    // NULLIF's comparison coerces a number and a numeric string the way
    // MySQL's `=` does; SQLite's own nullif() never would.
    assert_eq!(value(&mut client, "NULLIF(1,'1')"), "NULL");
    assert_eq!(value(&mut client, "NULLIF(1,2)"), "1");
    assert_eq!(value(&mut client, "NULLIF(NULL,1)"), "NULL");

    // ROUND on a value written with an exponent ties to even in MySQL
    // 8.4.11; the engine's own round() ties away from zero. The shim maps
    // only that literal shape — `ROUND(2.5)`, no exponent, is not provably
    // a MySQL DOUBLE from the text alone, so it reaches the engine's own
    // round() unchanged and is 3 in both, which is the "Use instead" MySQL's
    // own manual gives. Checked here rather than asserted from memory, so a
    // regression that started rewriting `ROUND(2.5)` too would fail loudly.
    assert_eq!(value(&mut client, "ROUND(2.5e0)"), "2");
    assert_eq!(value(&mut client, "ROUND(3.5e0)"), "4");
    assert_eq!(value(&mut client, "ROUND(2.5)"), "3");
    // And the refusal AHL-432 recorded — a negative digit count — is a real
    // answer now instead.
    assert_eq!(value(&mut client, "ROUND(1234.5678,-2)"), "1200");

    // MID is MySQL's alias for SUBSTRING, and OCTET_LENGTH/BIT_LENGTH were
    // refused outright for lack of a byte-counting primitive; all three are
    // real mappings now that `octet_length()` and `mysql_substr()` exist.
    assert_eq!(value(&mut client, "MID('hello',2,3)"), "ell");
    assert_eq!(value(&mut client, "OCTET_LENGTH('héllo')"), "6");
    assert_eq!(value(&mut client, "OCTET_LENGTH(NULL)"), "NULL");
    assert_eq!(value(&mut client, "BIT_LENGTH('héllo')"), "48");
}

/// JSON (AHL-490): the Laravel-shaped queries the MySQL wire actually needs
/// to answer — `casts => ['attributes' => 'array']` reads back through
/// `json_extract`/`->`/`->>`, and `whereJsonLength`/`whereJsonContainsKey`
/// compile onto `JSON_LENGTH`/`JSON_CONTAINS_PATH` (checked against
/// Laravel's own `MySqlGrammar` source, `docs/server.md`'s Divergences
/// section). `JSON_EXTRACT`/`JSON_SET`/`JSON_INSERT`/`JSON_REPLACE`/
/// `JSON_REMOVE`/`JSON_VALID`/`JSON_ARRAY`/`JSON_OBJECT` are not exercised
/// here specifically because they need no shim at all — same name, same
/// engine function, case-insensitive — which is exactly what makes them
/// safe to leave off this list rather than an oversight.
#[test]
fn json_functions_answer_what_laravel_over_mysql_needs() {
    let server = TestServer::start("mysql-fn-json");
    let mut client = server.client();
    client.ok_query("create table products (id integer primary key, attributes text)");
    client.ok_query(
        "insert into products (id, attributes) values (1, \
         '{\"color\":\"red\",\"tags\":[\"a\",\"b\"]}')",
    );
    client.ok_query("insert into products (id, attributes) values (2, NULL)");

    // A plain `json_extract`/`->`/`->>` read, spelled exactly as SQLite's
    // dialect has it — no shim rewrite needed, same as `json_extract` at
    // the top of the wire connection.
    assert_eq!(
        value(
            &mut client,
            "json_extract('{\"color\":\"red\"}', '$.color')"
        ),
        "red"
    );
    assert_eq!(
        value(&mut client, "'{\"color\":\"red\"}' -> '$.color'"),
        "\"red\""
    );
    assert_eq!(
        value(&mut client, "'{\"color\":\"red\"}' ->> '$.color'"),
        "red"
    );

    // `wrapJsonSelector` — `orderBy('attributes->color')` and similar reads —
    // compiles to `json_unquote(json_extract(...))` in MySqlGrammar.
    assert_eq!(
        value(
            &mut client,
            "JSON_UNQUOTE(JSON_EXTRACT('{\"color\":\"red\"}', '$.color'))"
        ),
        "red"
    );

    // `whereJsonLength('tags', '>', 1)` compiles to `json_length(...)`.
    assert_eq!(
        value(
            &mut client,
            "JSON_LENGTH('{\"tags\":[\"a\",\"b\"]}', '$.tags') > 1"
        ),
        "1"
    );

    // `whereJsonContainsKey('attributes->color')` compiles to
    // `ifnull(json_contains_path(field, 'one', path), 0)`.
    assert_eq!(
        value(
            &mut client,
            "ifnull(JSON_CONTAINS_PATH('{\"color\":\"red\"}', 'one', '$.color'), 0)"
        ),
        "1"
    );
    assert_eq!(
        value(
            &mut client,
            "ifnull(JSON_CONTAINS_PATH('{\"color\":\"red\"}', 'one', '$.missing'), 0)"
        ),
        "0"
    );

    // `->update(['attributes->color' => 'green'])` compiles to
    // `attributes = json_set(attributes, '$.color', ?)` — the column name
    // and the path/value are both spelled exactly as SQLite's own json_set
    // already takes them, so no shim rewrite is needed there either.
    client.ok_query(
        "update products set attributes = json_set(attributes, '$.color', 'green') where id = 1",
    );
    assert_eq!(
        client
            .ok_query("select json_extract(attributes, '$.color') from products where id = 1")
            .rows()
            .cell(0, 0),
        "green"
    );

    // Over the row where `attributes` is NULL, every one of these still
    // answers NULL rather than erroring — the ordinary NULL-propagation
    // rule, exercised over the wire rather than only in `eval.rs`.
    assert_eq!(
        client
            .ok_query("select json_extract(attributes, '$.color') from products where id = 2")
            .rows()
            .cell(0, 0),
        "NULL"
    );
}

/// The two shapes a real MySQL client library sent over this exact wire
/// before `rewrite_backslash_escapes` existed: a text-protocol `INSERT`
/// containing a client-side-escaped value. `mysql-connector-python`'s
/// default cursor and every driver that does not use a true binary-protocol
/// prepared statement build this SQL with a literal backslash in it, not
/// through this test's Rust source — the query strings below contain a real
/// `\` byte, the same one the client sent, not a Rust string escape.
#[test]
fn client_side_escaped_literals_round_trip_correctly() {
    let server = TestServer::start("backslash-escapes");
    let mut client = server.client();
    client.ok_query("create table t (id integer primary key, v text)");

    // A double quote silently corrupted the stored value: sent
    // `{"role":"admin"}`, stored `{\"role\":\"admin\"}` (one byte too many)
    // before this fix.
    client.ok_query("insert into t (id, v) values (1, '{\\\"role\\\":\\\"admin\\\"}')");
    assert_eq!(
        value(&mut client, "v from t where id = 1"),
        "{\"role\":\"admin\"}"
    );

    // A single quote broke the statement outright: the client's `\'` read as
    // a real string terminator and the server answered `1064 Unterminated
    // string literal` before this fix.
    client.ok_query("insert into t (id, v) values (2, 'O\\'Brien')");
    assert_eq!(value(&mut client, "v from t where id = 2"), "O'Brien");

    // Both spellings of an embedded quote mean the same thing, and a client
    // is free to use either — MySQL's `\'` and the SQL-standard `''`.
    client.ok_query("insert into t (id, v) values (3, 'it''s here')");
    assert_eq!(value(&mut client, "v from t where id = 3"), "it's here");

    // `\%`/`\_` are the one pair MySQL leaves as the literal two-byte
    // sequence, since they matter to a later LIKE — not decoded away here.
    client.ok_query("insert into t (id, v) values (4, '100\\%')");
    assert_eq!(value(&mut client, "v from t where id = 4"), "100\\%");
}

/// `JSON_QUOTE`/`JSON_TYPE`/`JSON_CONTAINS`/`JSON_OVERLAPS` are refused with
/// MySQL's own `ER_NOT_SUPPORTED_YET` (1235) rather than silently answering
/// under SQLite's different rules for the same name.
#[test]
fn refused_json_functions_answer_1235_not_a_dropped_connection() {
    let server = TestServer::start("mysql-fn-json-refused");
    let mut client = server.client();

    for sql in [
        "SELECT JSON_QUOTE(1)",
        "SELECT JSON_TYPE('{\"a\":1}')",
        "SELECT JSON_CONTAINS('{\"a\":1}', '1')",
        "SELECT JSON_OVERLAPS('[1]', '[1]')",
        "SELECT JSON_CONTAINS_PATH('{\"a\":1}', 'all', '$.a')",
    ] {
        let error = client.query(sql).unwrap_err();
        assert_eq!(error.code, 1235, "{sql}: ER_NOT_SUPPORTED_YET");
    }

    // `json_patch` and the table-valued `json_each`/`json_tree` are not
    // implemented at all (`unsupported.test` pins the refusal at the engine
    // level); over the wire they still answer a MySQL error code rather
    // than dropping the connection.
    let error = client.query("SELECT json_patch('{}', '{}')").unwrap_err();
    assert_eq!(error.code, 1235);
    let error = client
        .query("SELECT * FROM json_each('[1,2,3]')")
        .unwrap_err();
    assert_eq!(error.code, 1235);

    // The connection survives every refusal.
    assert_eq!(value(&mut client, "1 + 1"), "2");
}

/// The clock functions cannot be compared against a fixed string, so what is
/// checked is the shape MySQL returns and the relationships that hold between
/// them there: `LEFT(NOW(), 10)` is `CURDATE()`, and so on.
#[test]
fn the_clock_functions_have_mysqls_shape_and_agree_with_each_other() {
    let (_server, mut client) = function_fixture("mysql-fn-clock");

    assert_eq!(value(&mut client, "CHAR_LENGTH(NOW())"), "19");
    assert_eq!(value(&mut client, "CHAR_LENGTH(CURDATE())"), "10");
    assert_eq!(value(&mut client, "CHAR_LENGTH(CURTIME())"), "8");
    assert_eq!(value(&mut client, "LOCATE('-', NOW())"), "5");
    assert_eq!(value(&mut client, "LOCATE(':', NOW())"), "14");

    assert_eq!(value(&mut client, "LEFT(NOW(),10) = CURDATE()"), "1");
    assert_eq!(value(&mut client, "RIGHT(NOW(),8) = CURTIME()"), "1");
    assert_eq!(value(&mut client, "LOCALTIMESTAMP() = NOW()"), "1");
    assert_eq!(value(&mut client, "YEAR(NOW()) = YEAR(CURDATE())"), "1");
    assert_eq!(value(&mut client, "UNIX_TIMESTAMP() > 1700000000"), "1");

    // The engine's clock is UTC, so these hold here and hold in MySQL only
    // when its session time zone is UTC. See `docs/server.md`, Divergences.
    assert_eq!(value(&mut client, "NOW() = UTC_TIMESTAMP()"), "1");
    assert_eq!(value(&mut client, "CURDATE() = UTC_DATE()"), "1");

    // RAND() is a double in [0, 1), which is the contract MySQL's has.
    assert_eq!(value(&mut client, "RAND() >= 0"), "1");
    assert_eq!(value(&mut client, "RAND() < 1"), "1");
}

/// The mappings against real columns, including a NULL row — a literal-only
/// test would not exercise the path a client actually takes.
#[test]
fn the_mapped_functions_work_over_columns_in_every_clause() {
    let (_server, mut client) = function_fixture("mysql-fn-columns");

    let rows = client
        .ok_query("select id, CONCAT('<', name, '>'), CHAR_LENGTH(name) from items order by id")
        .rows();
    assert_eq!(rows.column("id"), vec!["1", "2", "3"]);
    assert_eq!(rows.cell(0, 1), "<hello>");
    assert_eq!(rows.cell(1, 1), "<héllo wörld>");
    assert_eq!(rows.cell(2, 1), "NULL", "CONCAT propagates a NULL column");
    assert_eq!(rows.cell(0, 2), "5");
    assert_eq!(rows.cell(1, 2), "11");
    assert_eq!(rows.cell(2, 2), "NULL");

    // In a WHERE clause.
    let rows = client
        .ok_query("select id from items where CHAR_LENGTH(name) = 5 order by id")
        .rows();
    assert_eq!(rows.column("id"), vec!["1"]);
    let rows = client
        .ok_query("select id from items where YEAR(created) = 2024 order by id")
        .rows();
    assert_eq!(rows.column("id"), vec!["1", "2"]);

    // In an UPDATE, and then read back.
    let (affected, _) = client
        .ok_query("update items set name = CONCAT(name, '!') where id = 1")
        .ok();
    assert_eq!(affected, 1);
    assert_eq!(
        client
            .ok_query("select name from items where id = 1")
            .rows()
            .cell(0, 0),
        "hello!"
    );

    // And in a DELETE.
    let (affected, _) = client
        .ok_query("delete from items where CHAR_LENGTH(name) = 6")
        .ok();
    assert_eq!(affected, 1);

    // A prepared statement is translated the same way, and the placeholder is
    // still the only one after the rewrite.
    let stmt = client
        .prepare("select id from items where LEFT(name, 5) = ?")
        .expect("prepare");
    assert_eq!(stmt.param_count, 1);
    let rows = client
        .execute(&stmt, &[Param::Str("héllo".to_string())])
        .expect("execute")
        .rows();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.cell(0, 0), "2");
}

/// A mapping this server cannot reproduce fails loudly, naming the function and
/// the input that separates the two engines. A refusal is visible; a wrong
/// answer is not, which is the whole reason these are not mapped.
#[test]
fn an_unmappable_function_is_refused_with_a_reason_rather_than_answered_wrongly() {
    let (_server, mut client) = function_fixture("mysql-fn-refusals");

    for (sql, needle) in [
        ("SELECT CONCAT_WS('-','a',NULL,'c')", "skips NULL"),
        ("SELECT GREATEST(2,'10')", "storage class"),
        ("SELECT LEAST(2,'10')", "storage class"),
        ("SELECT MOD(5.5,2)", "1.5"),
        ("SELECT SYSDATE()", "moment of the call"),
        ("SELECT DATE_FORMAT('2024-01-15','%Y')", "format specifiers"),
        ("SELECT DATEDIFF('2024-03-01','2024-02-01')", "julianday"),
        ("SELECT MONTHNAME('2024-01-15')", "locale-dependent"),
        ("SELECT WEEK('2024-01-15')", "week-numbering"),
        ("SELECT FROM_UNIXTIME(0)", "32536771199"),
        ("SELECT LOCATE('l','hello',4)", "no third argument"),
        ("SELECT RAND(42)", "seeded sequence"),
        ("SELECT NOW(3)", "fractional digits"),
        ("SELECT UNIX_TIMESTAMP('2024-01-15')", "session time zone"),
        ("SELECT TRIM(BOTH 'xy' FROM 'yxhixy')", "yxhi"),
        ("SELECT LEFT('hello', ?)", "integer literal"),
    ] {
        let error = client.query(sql).expect_err(&format!("{sql} must fail"));
        assert_eq!(error.code, 1235, "{sql}: {}", error.message);
        assert_eq!(error.sqlstate, "42000", "{sql}");
        assert!(
            error.message.contains(needle),
            "{sql}: the message must name `{needle}`, got: {}",
            error.message
        );
    }

    // A wrong argument count is MySQL's own 1582, not a generic refusal.
    let error = client
        .query("SELECT CHAR_LENGTH('a','b')")
        .expect_err("must fail");
    assert_eq!(error.code, 1582);
    assert_eq!(
        error.message,
        "Incorrect parameter count in the call to native function 'CHAR_LENGTH'"
    );

    // And the connection is still usable afterwards.
    assert_eq!(value(&mut client, "CHAR_LENGTH('abc')"), "3");
}

/// The rewriting must not reach into a value. A statement that *stores* the
/// text `CONCAT(1,2)` has to store those nine characters.
#[test]
fn a_function_name_inside_a_value_is_stored_not_evaluated() {
    let (_server, mut client) = function_fixture("mysql-fn-quoting");

    client.ok_query("insert into items (id, name) values (9, 'CONCAT(1,2) and NOW()')");
    let rows = client
        .ok_query("select name from items where id = 9")
        .rows();
    assert_eq!(rows.cell(0, 0), "CONCAT(1,2) and NOW()");

    // A column whose name is a mapped function's is still a column.
    client.ok_query("create table quirk (id integer primary key, `left` text, `if` text)");
    client.ok_query("insert into quirk (id, `left`, `if`) values (1, 'L', 'I')");
    let rows = client
        .ok_query("select CONCAT(`left`, `if`) from quirk")
        .rows();
    assert_eq!(rows.cell(0, 0), "LI");
}

// =====================================================================
// EXPLAIN
// =====================================================================

/// `EXPLAIN` over the wire: the engine's, passed straight through, with the
/// three columns it declares and the access path it chose.
///
/// The pair — indexed and unindexed, same query text — is the assertion worth
/// having here as well as in the engine's own tests: this is where a client
/// actually reads it, and an `EXPLAIN` that always says the same thing would
/// pass either half alone.
#[test]
fn explain_reports_the_access_path_over_the_wire() {
    let server = TestServer::start("explain");
    let mut client = server.client();

    client.ok_query("CREATE TABLE posts (id INTEGER PRIMARY KEY, author TEXT, year INTEGER)");
    client.ok_query("CREATE INDEX posts_year ON posts (year)");
    client.ok_query("INSERT INTO posts (id, author, year) VALUES (1, 'ada', 1843)");

    let rows = client
        .ok_query("EXPLAIN SELECT author FROM posts WHERE year = 1843")
        .rows();
    assert_eq!(
        rows.columns,
        vec!["id", "parent", "detail"],
        "EXPLAIN reports SQLite's node/parent/detail shape, not MySQL's column set"
    );
    assert_eq!(rows.types[0], 0x08, "id should be MYSQL_TYPE_LONGLONG");
    assert_eq!(
        rows.types[2], 0xfd,
        "detail should be MYSQL_TYPE_VAR_STRING"
    );
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.cell(0, 0), "1");
    assert_eq!(rows.cell(0, 1), "0");
    assert_eq!(
        rows.cell(0, 2),
        "SEARCH posts USING INDEX posts_year (year=?)"
    );

    // The column with no index is the control: same shape of query, and it
    // has to come back as a scan.
    let rows = client
        .ok_query("EXPLAIN SELECT year FROM posts WHERE author = 'ada'")
        .rows();
    assert_eq!(rows.cell(0, 2), "SCAN posts");

    client.quit();
}

/// MySQL spells `EXPLAIN <statement>` `DESCRIBE <statement>` too, and this
/// server used to refuse that outright. `DESCRIBE <table>` is still the shim's
/// column listing — the two must not have collapsed into one another.
#[test]
fn describe_answers_a_statement_with_a_plan_and_a_table_with_its_columns() {
    let server = TestServer::start("describe-explain");
    let mut client = server.client();

    client.ok_query("CREATE TABLE posts (id INTEGER PRIMARY KEY, body TEXT)");

    let plan = client.ok_query("DESCRIBE SELECT body FROM posts").rows();
    assert_eq!(plan.columns, vec!["id", "parent", "detail"]);
    assert_eq!(plan.cell(0, 2), "SCAN posts");

    let columns = client.ok_query("DESCRIBE posts").rows();
    assert_eq!(columns.column("Field"), vec!["id", "body"]);

    client.quit();
}

/// A prepared `EXPLAIN` has to describe its own result set at
/// `COM_STMT_PREPARE`, before it runs — and running it must still not touch
/// the rows.
#[test]
fn a_prepared_explain_reports_its_columns_and_writes_nothing() {
    let server = TestServer::start("explain-prepared");
    let mut client = server.client();

    client.ok_query("CREATE TABLE posts (id INTEGER PRIMARY KEY, body TEXT)");
    client.ok_query("INSERT INTO posts (id, body) VALUES (1, 'one')");

    let stmt = client
        .prepare("EXPLAIN DELETE FROM posts WHERE id = ?")
        .expect("prepare EXPLAIN");
    assert_eq!(stmt.param_count, 1);
    assert_eq!(
        stmt.columns
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["id", "parent", "detail"],
    );

    let rows = client
        .execute(&stmt, &[Param::Int(1)])
        .expect("execute EXPLAIN")
        .rows();
    assert_eq!(rows.cell(0, 2), "DELETE FROM posts");
    assert_eq!(
        rows.cell(1, 2),
        "SEARCH posts USING INTEGER PRIMARY KEY (rowid=?)"
    );

    // The row it described is still there: EXPLAIN never ran the DELETE.
    let remaining = client.ok_query("SELECT body FROM posts").rows();
    assert_eq!(remaining.rows.len(), 1);
    assert_eq!(remaining.cell(0, 0), "one");

    client.quit();
}

/// `EXPLAIN ANALYZE` would have to run the statement to answer. Refused by
/// name rather than answered with an ordinary plan.
#[test]
fn explain_analyze_is_refused_over_the_wire() {
    let server = TestServer::start("explain-analyze");
    let mut client = server.client();
    client.ok_query("CREATE TABLE posts (id INTEGER PRIMARY KEY)");

    let error = client
        .query("EXPLAIN ANALYZE SELECT id FROM posts")
        .expect_err("EXPLAIN ANALYZE must be refused");
    assert!(
        error.message.contains("ANALYZE"),
        "the refusal must name the clause, got: {}",
        error.message
    );

    // And the connection is still usable.
    let rows = client.ok_query("EXPLAIN SELECT id FROM posts").rows();
    assert_eq!(rows.cell(0, 2), "SCAN posts");

    client.quit();
}

// =====================================================================
// streaming result sets (docs/enterprise-readiness.md, blocker 8)
// =====================================================================

/// Seed `rows` rows of a two-column table, in batches, and hand back a client
/// on the same server.
fn seeded_server(name: &str, rows: i64) -> (TestServer, Client) {
    let server = TestServer::start(name);
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    for start in (1..=rows).step_by(500) {
        let end = (start + 499).min(rows);
        let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, 'body-{id}')"));
        }
        client.ok_query(&sql);
    }
    (server, client)
}

/// The headline case: a result set far larger than any buffer between the two
/// ends, over a real socket, arriving whole and in order.
#[test]
fn a_large_result_set_arrives_whole_over_a_real_socket() {
    const ROWS: i64 = 50_000;
    let (_server, mut client) = seeded_server("large-result", ROWS);

    assert_eq!(client.count_rows("SELECT id, body FROM kv"), ROWS as usize);

    // The ends of it, decoded, so "arrived" means the right rows in the right
    // order rather than the right *number* of packets.
    let head = client.ok_query("SELECT id, body FROM kv LIMIT 3").rows();
    assert_eq!(head.column("id"), vec!["1", "2", "3"]);
    assert_eq!(head.cell(0, 1), "body-1");
    let tail = client
        .ok_query(&format!("SELECT id, body FROM kv WHERE id > {}", ROWS - 2))
        .rows();
    assert_eq!(
        tail.column("id"),
        vec![(ROWS - 1).to_string(), ROWS.to_string()]
    );
    assert_eq!(tail.cell(1, 1), format!("body-{ROWS}"));

    client.quit();
}

/// Byte-for-byte, the two paths through the server answer the same statement
/// the same way.
///
/// The control is the materialising path *as it still exists*, not a recorded
/// golden: a computed or otherwise undeclared column cannot be described in
/// the column-definition packets that must precede the first row, so a query
/// projecting one is still answered by building the whole result set. A
/// derived table's columns are exactly that — the planner gives them no type,
/// because a subquery's projection has none to give — so `SELECT * FROM
/// (SELECT id, body FROM kv) AS t` returns the identical rows under the
/// identical labels by the other route.
///
/// Compared as raw bytes, which is the point: a column definition's charset,
/// flags and declared length never reach [`Rows`], and a streaming rewrite
/// that changed one of them would pass every other test in this file.
#[test]
fn a_streamed_result_set_is_byte_identical_to_a_materialised_one() {
    let (_server, mut client) = seeded_server("byte-identical", 40);

    let streamed = client.raw_query("SELECT id, body FROM kv WHERE id <= 20");
    let materialised =
        client.raw_query("SELECT * FROM (SELECT id, body FROM kv WHERE id <= 20) AS t");
    assert_eq!(
        streamed, materialised,
        "the streamed and materialised answers differ on the wire"
    );

    // The binary protocol too, where the column type decides how every value
    // is *encoded* rather than only how it is labelled — a type that drifted
    // would misframe the rows rather than mislabel them.
    let direct = client
        .prepare("SELECT id, body FROM kv WHERE id <= 20")
        .expect("prepare");
    let derived = client
        .prepare("SELECT * FROM (SELECT id, body FROM kv WHERE id <= 20) AS t")
        .expect("prepare");
    assert_eq!(
        client.raw_execute(&direct),
        client.raw_execute(&derived),
        "the binary-protocol answers differ on the wire"
    );

    client.quit();
}

/// `NULL`s, and the order they arrive in, survive the streamed path unchanged.
///
/// Same comparison as above and for the same reason, over a table where every
/// column has holes in it: the binary protocol carries `NULL` in a bitmap
/// whose bits are offset by two, and the text protocol carries it as a
/// reserved byte, so a streaming rewrite that lost the distinction between
/// "absent" and "empty" would show up here and almost nowhere else.
#[test]
fn nulls_and_row_order_survive_the_streamed_path() {
    let server = TestServer::start("streamed-nulls");
    let mut client = server.client();
    client.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT, r REAL)");
    client.ok_query(
        "INSERT INTO t (id, n, s, r) VALUES \
         (1, NULL, 'one', 1.5), (2, 20, NULL, NULL), (3, NULL, '', 0.0), (4, 40, 'four', -2.25)",
    );

    let streamed = client.raw_query("SELECT id, n, s, r FROM t");
    let materialised = client.raw_query("SELECT * FROM (SELECT id, n, s, r FROM t) AS x");
    assert_eq!(streamed, materialised);

    // And decoded, so the assertion above is anchored to something readable.
    let rows = client.ok_query("SELECT id, n, s, r FROM t").rows();
    assert_eq!(rows.column("id"), vec!["1", "2", "3", "4"]);
    assert_eq!(rows.column("n"), vec!["NULL", "20", "NULL", "40"]);
    assert_eq!(rows.rows[1][2], None, "a NULL is not an empty string");
    assert_eq!(
        rows.rows[2][2],
        Some(String::new()),
        "an empty string is not a NULL"
    );

    let prepared = client
        .prepare("SELECT id, n, s, r FROM t")
        .expect("prepare");
    let binary = client.execute(&prepared, &[]).expect("execute").rows();
    assert_eq!(binary.rows, rows.rows, "the binary protocol agrees");

    client.quit();
}

/// A result set with no rows still knows what its columns are.
///
/// Nothing can be inferred from an answer that has no values in it, and the
/// old materialising path called every such column `VAR_STRING` — so
/// `SELECT id FROM kv` described `id` as text when the table happened to be
/// empty and as an integer when it did not, and contradicted the
/// `COM_STMT_PREPARE` that had already answered for the same column. The plan
/// knows, and the plan does not depend on how many rows came back.
#[test]
fn an_empty_result_set_still_reports_its_declared_column_types() {
    let server = TestServer::start("empty-types");
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT, weight REAL)");

    let empty = client.ok_query("SELECT id, body, weight FROM kv").rows();
    assert!(empty.rows.is_empty());
    assert_eq!(
        empty.types,
        vec![0x08, 0xfd, 0x05],
        "LONGLONG, VAR_STRING, DOUBLE"
    );

    // The same three, from the prepare that precedes them: the two answers
    // have to agree, which is the whole reason the plan decides this.
    let prepared = client
        .prepare("SELECT id, body, weight FROM kv")
        .expect("prepare");
    let declared: Vec<u8> = prepared.columns.iter().map(|(_, ty)| *ty).collect();
    assert_eq!(declared, empty.types);

    // A column that is present but `NULL` all the way down is the same
    // question with rows in it.
    client.ok_query("INSERT INTO kv (id, body, weight) VALUES (1, NULL, NULL)");
    let nulls = client.ok_query("SELECT id, body, weight FROM kv").rows();
    assert_eq!(nulls.types, vec![0x08, 0xfd, 0x05]);
    assert_eq!(nulls.rows[0][1], None);

    client.quit();
}

/// The hard case: the query fails once rows are already on the wire.
///
/// They cannot be recalled — the protocol has no packet for "ignore what I
/// just sent" — so MySQL ends the row stream with an ERR packet where its
/// terminating EOF would have gone, and so does this. The rows that were
/// already sent stand; the client learns the answer is incomplete from the
/// error, which is the same packet it already watches for in place of the
/// *first* one.
///
/// The failure is a real one and arrives mid-scan: `replace()` refuses a
/// result past SQLite's `SQLITE_MAX_LENGTH` before it allocates it, and the
/// `CASE` keeps it from being evaluated until the fifth row.
#[test]
fn an_error_after_the_first_row_ends_the_result_set_with_an_err_packet() {
    let (_server, mut client) = seeded_server("mid-stream-error", 10);

    let padding = "a".repeat(1000);
    let blowup = format!(
        "length(replace(replace(replace(body, 'b', '{padding}'), 'a', '{padding}'), 'a', '{padding}'))"
    );
    let sql =
        format!("SELECT id, body FROM kv WHERE CASE WHEN id < 5 THEN 1 ELSE {blowup} END > 0");

    let (rows, error) = client.query_until_error(&sql);
    assert_eq!(
        rows.len(),
        4,
        "the rows produced before the failure must still have been sent"
    );
    assert_eq!(rows[0][0], Some("1".to_string()));
    assert_eq!(rows[3][0], Some("4".to_string()));
    assert!(
        error.message.contains("limit"),
        "the error must name what went wrong, got: {}",
        error.message
    );

    // And the connection is still in step: the ERR ended the exchange the way
    // an EOF would have, so the next command is answered normally rather than
    // one packet behind.
    let after = client.ok_query("SELECT id FROM kv WHERE id = 7").rows();
    assert_eq!(after.column("id"), vec!["7"]);

    client.quit();
}

/// A statement that fails *before* its first row has lost nothing, so it is
/// answered with a plain ERR packet and no result-set framing at all — the
/// same reply it would have had if it had never been streamable. This is what
/// the header-on-first-row ordering buys, and it is easy to lose.
#[test]
fn a_failure_before_the_first_row_is_a_plain_error_packet() {
    let (_server, mut client) = seeded_server("early-failure", 4);

    // `LIMIT` is resolved from its bound parameter only when the statement
    // runs, long after its columns were described, so a `LIMIT` that is not a
    // number fails at exactly the moment this is about: after the point of no
    // return for the column definitions, before the first row.
    let prepared = client
        .prepare("SELECT id, body FROM kv LIMIT ?")
        .expect("prepare");
    let error = client
        .execute(&prepared, &[Param::Str("not a number".to_string())])
        .expect_err("a non-numeric LIMIT must be refused");
    assert!(
        error.message.contains("LIMIT"),
        "the refusal must name the clause, got: {}",
        error.message
    );

    // No half-open result set was left behind.
    let rows = client.ok_query("SELECT id FROM kv LIMIT 2").rows();
    assert_eq!(rows.column("id"), vec!["1", "2"]);

    client.quit();
}

/// Everything a blocking operator does still happens, and still answers the
/// same, when its rows leave through the streamed path: `ORDER BY`, `GROUP
/// BY`, `DISTINCT` and `LIMIT`/`OFFSET` all decide *which* rows survive and in
/// what order, and the row callback the server writes from is fed after they
/// have run, not instead of them.
#[test]
fn ordering_grouping_and_paging_answer_the_same_through_the_streamed_path() {
    let server = TestServer::start("streamed-blocking");
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, grp TEXT, body TEXT)");
    client.ok_query(
        "INSERT INTO kv (id, grp, body) VALUES \
         (1, 'a', 'one'), (2, 'b', 'two'), (3, 'a', 'three'), (4, 'c', 'four'), (5, 'b', 'five')",
    );

    let sorted = client
        .ok_query("SELECT id, body FROM kv ORDER BY body")
        .rows();
    assert_eq!(
        sorted.column("body"),
        vec!["five", "four", "one", "three", "two"]
    );

    let distinct = client
        .ok_query("SELECT DISTINCT grp FROM kv ORDER BY grp")
        .rows();
    assert_eq!(distinct.column("grp"), vec!["a", "b", "c"]);

    let paged = client
        .ok_query("SELECT id, body FROM kv ORDER BY id LIMIT 2 OFFSET 2")
        .rows();
    assert_eq!(paged.column("id"), vec!["3", "4"]);

    // Byte-for-byte against the materialising path, sort included.
    let streamed = client.raw_query("SELECT id, body FROM kv ORDER BY body DESC LIMIT 3");
    let materialised =
        client.raw_query("SELECT * FROM (SELECT id, body FROM kv ORDER BY body DESC LIMIT 3) AS t");
    assert_eq!(streamed, materialised);

    client.quit();
}

/// The server refuses the one statement that would have taken it down, and
/// goes on serving.
///
/// A blocking operator holds its whole input; unbounded, what ends it is the
/// out-of-memory killer, and that ends the process — every connection, not the
/// one that asked. With a ceiling the client gets `ER_OUT_OF_SORTMEMORY`
/// (1038, SQLSTATE HY001: a resource failure, which is what a driver has to
/// classify it as), the connection stays up, and the same rows still come back
/// through the streamed path, which the ceiling does not apply to because
/// nothing there holds more than a row.
#[test]
fn a_query_past_the_memory_ceiling_is_refused_and_the_server_keeps_serving() {
    const ROWS: i64 = 5_000;
    let server = TestServer::start_tuned("query-memory", "s3cret", |options| {
        // Far below what a sort over `ROWS` rows needs, and far below the
        // shipped default, so this test is about the ceiling rather than about
        // how big the rows happen to be.
        options.query_memory_bytes = 16 * 1024;
    });
    let mut client = server.client();
    client.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    for start in (1..=ROWS).step_by(500) {
        let end = (start + 499).min(ROWS);
        let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, 'body-{id}')"));
        }
        client.ok_query(&sql);
    }

    let error = client
        .query("SELECT id, body FROM kv ORDER BY body")
        .expect_err("a sort past the ceiling must be refused");
    assert_eq!(error.code, 1038, "ER_OUT_OF_SORTMEMORY");
    assert_eq!(error.sqlstate, "HY001", "a memory allocation error");
    assert!(
        error.message.contains("ceiling"),
        "the refusal must say what it hit, got: {}",
        error.message
    );

    // The connection is untouched: no half-written result set, nothing out of
    // step, and the same rows are still readable the way that does not block.
    assert_eq!(client.count_rows("SELECT id, body FROM kv"), ROWS as usize);

    // And a sort small enough to fit still sorts.
    let sorted = client
        .ok_query("SELECT id, body FROM kv WHERE id <= 5 ORDER BY body DESC")
        .rows();
    assert_eq!(sorted.column("id"), vec!["5", "4", "3", "2", "1"]);

    // A second connection was never at risk, which is the property the whole
    // ceiling exists for: one client's query does not decide whether the
    // others get served.
    let mut other = server.client();
    assert_eq!(other.count_rows("SELECT id FROM kv"), ROWS as usize);

    other.quit();
    client.quit();
}

/// A client that hangs up part-way through reading a result set costs the
/// server that connection and nothing else.
///
/// This is the failure the streamed path adds and the materialising one did
/// not have: the server is now writing rows while the engine is still
/// producing them, so a peer that disappears mid-answer is a write error in the
/// middle of a scan rather than at the end of one. It has to end that
/// connection, release its slot and leave the engine alone — and "release its
/// slot" is the part that would otherwise be invisible until a server that had
/// been up for a week refused every new connection.
///
/// The cap is one, so the slot is the assertion: if the aborted connection's
/// thread had not ended, nothing else could ever connect.
#[test]
fn a_client_that_hangs_up_mid_result_set_gives_its_slot_back() {
    const ROWS: i64 = 50_000;
    let server = TestServer::start_with("hangup", "s3cret", 1);

    {
        let mut setup = server.client();
        setup.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
        for start in (1..=ROWS).step_by(500) {
            let end = (start + 499).min(ROWS);
            let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
            for id in start..=end {
                if id > start {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("({id}, 'body-{id}')"));
            }
            setup.ok_query(&sql);
        }
        setup.quit();
    }

    {
        // Read only the metadata, then drop the socket with tens of thousands
        // of rows still to come.
        let mut deserter = server.client_within(Duration::from_secs(10));
        deserter.command(0x03, b"SELECT id, body FROM kv");
        let first = deserter.read_packet().expect("column count");
        let count = Cursor::new(&first).lenenc().expect("column count") as usize;
        for _ in 0..=count {
            deserter.read_packet().expect("column definition");
        }
    }

    // The slot came back, and the database is exactly as it was.
    let mut after = server.client_within(Duration::from_secs(10));
    assert_eq!(after.count_rows("SELECT id, body FROM kv"), ROWS as usize);
    after.quit();
}

// =====================================================================
// accounts and privileges (AHL-497)
// =====================================================================
//
// The negative tests are the ones that matter here. A privilege system is
// only worth the refusals it makes, so every verb below is tested by an
// account that does *not* hold it being turned away, not only by one that
// does being let through.

/// A superuser session with a table to hand out privileges on.
fn accounts_fixture(name: &str) -> (TestServer, Client) {
    let server = TestServer::start(name);
    let mut root = server.client();
    root.ok_query("CREATE TABLE posts (id INTEGER PRIMARY KEY, body TEXT)");
    root.ok_query("CREATE TABLE vault (id INTEGER PRIMARY KEY, secret TEXT)");
    root.ok_query("INSERT INTO posts (id, body) VALUES (1, 'hello')");
    root.ok_query("INSERT INTO vault (id, secret) VALUES (1, 'launch-codes')");
    (server, root)
}

/// The whole point: an account holding one privilege on one table is refused
/// every other verb, on that table and on every other.
#[test]
fn a_user_without_a_privilege_is_refused_it_for_every_verb() {
    let (server, mut root) = accounts_fixture("acl-negative");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("GRANT SELECT ON posts TO 'reader'");

    let mut reader = server.client_as("reader", "r-pass");
    // The one thing it may do.
    assert_eq!(reader.count_rows("SELECT id, body FROM posts"), 1);

    for (sql, verb) in [
        ("INSERT INTO posts (id, body) VALUES (2, 'x')", "INSERT"),
        ("UPDATE posts SET body = 'x' WHERE id = 1", "UPDATE"),
        ("DELETE FROM posts WHERE id = 1", "DELETE"),
        ("ALTER TABLE posts ADD COLUMN extra TEXT", "ALTER"),
        ("DROP TABLE posts", "DROP"),
        ("CREATE INDEX posts_body ON posts (body)", "CREATE"),
    ] {
        let error = reader.query(sql).expect_err(sql);
        assert_eq!(error.code, 1142, "{sql} should be ER_TABLEACCESS_DENIED");
        assert_eq!(error.sqlstate, "42000", "{sql}");
        assert!(
            error.message.contains(verb) && error.message.contains("reader"),
            "{sql}: the refusal must name the privilege and the account, said {}",
            error.message
        );
    }

    // A grant on `posts` says nothing about `vault`.
    let error = reader
        .query("SELECT secret FROM vault")
        .expect_err("no grant on vault");
    assert_eq!(error.code, 1142);
    assert!(error.message.contains("vault"), "{}", error.message);

    // Nothing above changed the table it was refused on.
    assert_eq!(reader.count_rows("SELECT id FROM posts"), 1);
    reader.quit();
    root.quit();
}

/// The bypass a text-based privilege check would have: a table named only
/// inside a subquery. This is why authorisation reads the *plan*.
#[test]
fn a_table_reached_only_through_a_subquery_is_still_checked() {
    let (server, mut root) = accounts_fixture("acl-subquery");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("GRANT SELECT ON posts TO 'reader'");
    let mut reader = server.client_as("reader", "r-pass");

    for sql in [
        "SELECT (SELECT secret FROM vault) AS leak FROM posts",
        "SELECT body FROM posts WHERE id IN (SELECT id FROM vault)",
        "SELECT body FROM posts WHERE EXISTS (SELECT 1 FROM vault)",
        "SELECT p.body FROM posts p JOIN vault v ON p.id = v.id",
        "SELECT body FROM posts UNION ALL SELECT secret FROM vault",
        "SELECT body FROM (SELECT secret AS body FROM vault) AS inner_query",
    ] {
        let error = reader.query(sql).expect_err(sql);
        assert_eq!(error.code, 1142, "{sql} must be refused");
        assert!(
            error.message.contains("vault"),
            "{sql}: the refusal must name the table it could not read, said {}",
            error.message
        );
    }
    reader.quit();
    root.quit();
}

/// A write that also reads needs both privileges, because it really does both.
#[test]
fn a_write_that_reads_needs_the_read_privilege_too() {
    let (server, mut root) = accounts_fixture("acl-read-in-write");
    root.ok_query("CREATE USER 'writer' IDENTIFIED BY 'w-pass'");
    root.ok_query("GRANT INSERT, UPDATE, DELETE ON posts TO 'writer'");
    let mut writer = server.client_as("writer", "w-pass");

    // A blind insert is only an insert.
    writer.ok_query("INSERT INTO posts (id, body) VALUES (2, 'from writer')");

    // Everything that has to find a row first needs SELECT as well, which is
    // MySQL's rule and not an approximation of it.
    for sql in [
        "UPDATE posts SET body = 'x' WHERE id = 2",
        "DELETE FROM posts WHERE id = 2",
        "INSERT INTO posts (id, body) SELECT id, body FROM posts",
    ] {
        let error = writer.query(sql).expect_err(sql);
        assert_eq!(error.code, 1142, "{sql}");
        assert!(error.message.contains("SELECT"), "{}", error.message);
    }

    // ...but an unfiltered write, which reads nothing, is allowed on the
    // privileges it really uses.
    writer.ok_query("UPDATE posts SET body = 'blanked'");
    writer.ok_query("DELETE FROM posts");
    writer.quit();
    root.quit();
}

/// The vulnerability `CREATE TABLE ... AS SELECT` would have been if its
/// plan did not record what its query reads: an account holding `CREATE`
/// could copy a table it has no `SELECT` on into one of its own.
#[test]
fn create_table_as_select_still_needs_select_on_what_it_reads() {
    let (server, mut root) = accounts_fixture("acl-ctas");
    root.ok_query("CREATE USER 'maker' IDENTIFIED BY 'm-pass'");
    root.ok_query("GRANT CREATE ON stolen TO 'maker'");
    root.ok_query("GRANT CREATE, SELECT ON copy_of_posts TO 'maker'");
    root.ok_query("GRANT SELECT ON posts TO 'maker'");

    let mut maker = server.client_as("maker", "m-pass");

    // `CREATE` alone is not enough to read `vault` through a new table.
    let error = maker
        .query("CREATE TABLE stolen AS SELECT secret FROM vault")
        .expect_err("no SELECT on vault");
    assert_eq!(error.code, 1142, "should be ER_TABLEACCESS_DENIED");
    assert!(error.message.contains("vault"), "{}", error.message);
    assert!(error.message.contains("SELECT"), "{}", error.message);

    // With `SELECT` on the source, the same shape is a working copy.
    maker.ok_query("CREATE TABLE copy_of_posts AS SELECT * FROM posts");
    assert_eq!(maker.count_rows("SELECT id, body FROM copy_of_posts"), 1);

    maker.quit();
    root.quit();
}

/// Accounts and grants are in the database file, so they outlive the process
/// that created them — and the password is not in there in any readable form.
#[test]
fn accounts_and_grants_survive_reopening_the_database() {
    let (server, mut root) = accounts_fixture("acl-durable");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'hunter2'");
    root.ok_query("GRANT SELECT ON posts TO 'reader'");
    root.quit();

    // A second server, binding the same file and opening its own handles.
    let reopened = server.reopened();
    let mut reader =
        Client::connect(reopened, "reader", "hunter2", None).expect("the account survived");
    assert_eq!(reader.count_rows("SELECT id FROM posts"), 1);
    let error = reader
        .query("SELECT secret FROM vault")
        .expect_err("and so did the shape of its grant");
    assert_eq!(error.code, 1142);
    // A wrong password is still a wrong password after the reopen.
    assert_eq!(
        Client::connect(reopened, "reader", "hunter3", None)
            .expect_err("wrong password")
            .code,
        1045
    );
    reader.quit();

    // And what is on disk is a verifier, not a password. Checked over the raw
    // bytes of the file rather than through any API that could be filtering.
    let bytes = std::fs::read(server.path()).expect("read the database file");
    assert!(
        !bytes.windows(7).any(|window| window == b"hunter2"),
        "the plaintext password must not appear anywhere in the file"
    );
    // What *is* there is the verifier, in MySQL's own `*HEX40` spelling —
    // `SHA1(SHA1("hunter2"))`, independently computed.
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("*58815970BE77B3720276F63DB198B1FA42E5CC02"),
        "the stored native verifier for `hunter2` should be in the file"
    );
}

/// A revoked privilege stops working on the offending session's next
/// statement — not at its next reconnection, which for a pooled connection
/// could be never.
#[test]
fn a_revoke_takes_effect_on_an_already_connected_session() {
    let (server, mut root) = accounts_fixture("acl-revoke-live");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("GRANT SELECT ON posts TO 'reader'");

    let mut reader = server.client_as("reader", "r-pass");
    assert_eq!(reader.count_rows("SELECT id FROM posts"), 1);

    // Revoked from another connection entirely, while this one sits idle.
    root.ok_query("REVOKE SELECT ON posts FROM 'reader'");

    let error = reader
        .query("SELECT id FROM posts")
        .expect_err("the very next statement must be refused");
    assert_eq!(error.code, 1142);

    // And a prepared statement is re-checked at every execution, so one
    // prepared while the grant held does not outlive it.
    root.ok_query("GRANT SELECT ON posts TO 'reader'");
    let stmt = reader.prepare("SELECT id FROM posts").expect("prepare");
    reader.execute(&stmt, &[]).expect("allowed while granted");
    root.ok_query("REVOKE SELECT ON posts FROM 'reader'");
    let error = reader
        .execute(&stmt, &[])
        .expect_err("the plan outlived the grant, the permission did not");
    assert_eq!(error.code, 1142);

    // Dropping the account takes the session with it, on the same terms.
    root.ok_query("GRANT SELECT ON posts TO 'reader'");
    assert_eq!(reader.count_rows("SELECT id FROM posts"), 1);
    root.ok_query("DROP USER 'reader'");
    let error = reader
        .query("SELECT id FROM posts")
        .expect_err("a dropped account may not keep working on an open socket");
    assert_eq!(error.code, 1045);
    assert!(error.message.contains("no longer exists"), "{error:?}");
    root.quit();
}

/// Administering accounts is the superuser's, and nobody else's.
#[test]
fn a_non_superuser_cannot_administer_accounts() {
    let (server, mut root) = accounts_fixture("acl-admin");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("GRANT ALL PRIVILEGES ON *.* TO 'reader'");

    // Every privilege there is, and still not the right to hand them out.
    let mut reader = server.client_as("reader", "r-pass");
    reader.ok_query("SELECT id FROM vault");
    for sql in [
        "CREATE USER 'sneak' IDENTIFIED BY 'x'",
        "DROP USER 'root'",
        "ALTER USER 'root' IDENTIFIED BY 'x'",
        "GRANT ALL PRIVILEGES ON *.* TO 'reader' WITH GRANT OPTION",
        "REVOKE SELECT ON posts FROM 'reader'",
        "SHOW GRANTS FOR 'root'",
        // The same statements wearing the decoration a real driver sends.
        // Authorisation and dispatch have to read the *same* text: a leading
        // comment moves the keyword, and a check that classified this by its
        // first byte while the dispatcher classified it by its first keyword
        // would let an ordinary account create itself a superuser.
        "/* migration 3 */ CREATE USER 'sneak' IDENTIFIED BY 'x'",
        "-- rotate\nGRANT ALL PRIVILEGES ON *.* TO 'reader' WITH GRANT OPTION",
        "GRANT ALL PRIVILEGES ON *.* TO 'reader' WITH GRANT OPTION;",
    ] {
        let error = reader.query(sql).expect_err(sql);
        assert_eq!(error.code, 1227, "{sql}");
    }
    // Its own grants and its own password are its own business.
    reader.ok_query("SHOW GRANTS");
    reader.ok_query("ALTER USER 'reader' IDENTIFIED BY 'r-pass-2'");
    reader.quit();

    assert_eq!(
        server
            .try_client_as("reader", "r-pass")
            .expect_err("the old password must stop working")
            .code,
        1045
    );
    server.client_as("reader", "r-pass-2").quit();

    // And `sneak` was never created.
    assert_eq!(
        server
            .try_client_as("sneak", "x")
            .expect_err("no such account")
            .code,
        1045
    );
    root.quit();
}

/// The account store is not a table anybody can reach through SQL, superuser
/// included — it holds password verifiers, and `SHOW GRANTS` is the supported
/// way to read what is in it.
#[test]
fn the_account_store_is_invisible_and_untouchable() {
    let (_server, mut root) = accounts_fixture("acl-hidden");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");

    let tables = root.ok_query("SHOW TABLES").rows();
    let names = tables.rows.iter().flatten().flatten().collect::<Vec<_>>();
    assert!(
        names.iter().all(|name| !name.starts_with("__inlaysql_")),
        "SHOW TABLES listed the account store: {names:?}"
    );
    let schema_tables = root
        .ok_query("SELECT table_name FROM information_schema.tables")
        .rows();
    assert!(
        schema_tables
            .column("table_name")
            .iter()
            .all(|name| !name.starts_with("__inlaysql_")),
        "information_schema listed the account store"
    );

    for sql in [
        "SELECT * FROM __inlaysql_user",
        "SELECT * FROM __INLAYSQL_USER",
        "SELECT native_auth FROM `__inlaysql_user`",
        "UPDATE __inlaysql_user SET privileges = 255",
        "DELETE FROM __inlaysql_grant",
        "DROP TABLE __inlaysql_user",
        "SELECT 1 FROM posts WHERE id IN (SELECT id FROM __inlaysql_user)",
    ] {
        let error = root.query(sql).expect_err(sql);
        assert_eq!(error.code, 1142, "{sql}");
        assert!(
            error.message.contains("account store"),
            "{sql} should say what it refused and why, said {}",
            error.message
        );
    }

    // `DESCRIBE` does not admit it exists either.
    let error = root
        .query("DESCRIBE __inlaysql_user")
        .expect_err("no such table");
    assert_eq!(error.code, 1146);
    root.quit();
}

/// `SHOW GRANTS` reports what is really held, in MySQL's own spelling.
#[test]
fn show_grants_reports_what_is_held() {
    let (server, mut root) = accounts_fixture("acl-show-grants");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");

    let mine = root.ok_query("SHOW GRANTS").rows();
    assert_eq!(mine.columns, vec!["Grants for root@%"]);
    assert_eq!(
        mine.cell(0, 0),
        "GRANT ALL PRIVILEGES ON *.* TO 'root'@'%' WITH GRANT OPTION"
    );

    // A brand-new account holds nothing, and MySQL spells that `USAGE`.
    let theirs = root.ok_query("SHOW GRANTS FOR 'reader'").rows();
    assert_eq!(theirs.cell(0, 0), "GRANT USAGE ON *.* TO 'reader'@'%'");

    root.ok_query("GRANT SELECT, INSERT ON posts TO 'reader'");
    root.ok_query("GRANT SELECT ON *.* TO 'reader'");
    let theirs = root.ok_query("SHOW GRANTS FOR 'reader'").rows();
    let lines = theirs.column("Grants for reader@%");
    assert_eq!(lines[0], "GRANT SELECT ON *.* TO 'reader'@'%'");
    assert_eq!(
        lines[1],
        "GRANT SELECT, INSERT ON `inlaysql`.`posts` TO 'reader'@'%'"
    );

    // A global grant covers a table with no grant of its own.
    let mut reader = server.client_as("reader", "r-pass");
    assert_eq!(reader.count_rows("SELECT id FROM vault"), 1);
    reader.quit();
    root.quit();
}

/// Nothing in the account model may leave the database with nobody able to
/// administer it — there is no way back in over the wire from there.
#[test]
fn the_last_superuser_cannot_be_removed() {
    let (_server, mut root) = accounts_fixture("acl-last-superuser");
    for sql in [
        "DROP USER 'root'",
        "REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'root'",
    ] {
        let error = root.query(sql).expect_err(sql);
        assert_eq!(error.code, 1227, "{sql}");
        assert!(error.message.contains("--reset-superuser"), "{error:?}");
    }

    // With a replacement in place, both are allowed.
    root.ok_query("CREATE USER 'admin2' IDENTIFIED BY 'a'");
    root.ok_query("GRANT ALL PRIVILEGES ON *.* TO 'admin2' WITH GRANT OPTION");
    root.ok_query("REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'root'");
    let error = root
        .query("CREATE USER 'x' IDENTIFIED BY 'y'")
        .expect_err("root demoted itself");
    assert_eq!(error.code, 1227);
    root.quit();
}

/// Both authentication plugins still complete for an account created without
/// a plugin named, which is what stops any existing client breaking. An
/// account pinned to one plugin is switched onto it instead of being refused.
#[test]
fn every_authentication_path_still_completes_against_a_stored_verifier() {
    let (server, mut root) = accounts_fixture("acl-plugins");
    root.ok_query("CREATE USER 'both' IDENTIFIED BY 'p'");
    root.ok_query("CREATE USER 'nativeonly' IDENTIFIED WITH mysql_native_password BY 'p'");
    root.ok_query("CREATE USER 'sha2only' IDENTIFIED WITH caching_sha2_password BY 'p'");
    root.ok_query("CREATE USER 'nopass' IDENTIFIED BY ''");

    // The default account completes every exchange this server implements.
    Client::connect(server.addr, "both", "p", None)
        .expect("native")
        .quit();
    Client::connect_caching_sha2(server.addr, "both", "p")
        .expect("caching_sha2 fast path")
        .quit();
    Client::connect_caching_sha2_full_auth(server.addr, "both", "p")
        .expect("caching_sha2 full authentication")
        .quit();
    Client::connect_via_auth_switch(server.addr, "both", "p")
        .expect("a third plugin switches to native")
        .quit();

    // A pinned account is switched onto the plugin it has a verifier for
    // rather than refused, which is what MySQL does with its own per-account
    // plugin.
    Client::connect_via_auth_switch(server.addr, "nativeonly", "p")
        .expect("switched to native")
        .quit();
    Client::connect_caching_sha2(server.addr, "sha2only", "p")
        .expect("its own plugin, directly")
        .quit();

    // A wrong password is refused on every one of them, and the message never
    // says which half was wrong.
    for outcome in [
        Client::connect(server.addr, "both", "wrong", None),
        Client::connect_caching_sha2(server.addr, "both", "wrong"),
        Client::connect_caching_sha2_full_auth(server.addr, "both", "wrong"),
        Client::connect(server.addr, "nosuchuser", "p", None),
    ] {
        let error = outcome.expect_err("must be refused");
        assert_eq!(error.code, 1045);
        assert_eq!(error.sqlstate, "28000");
        assert!(!error.message.contains("no such"), "{error:?}");
    }

    // An empty password means an empty password, as it always has.
    Client::connect(server.addr, "nopass", "", None)
        .expect("empty password")
        .quit();
    assert_eq!(
        Client::connect(server.addr, "nopass", "anything", None)
            .expect_err("and nothing else")
            .code,
        1045
    );
    root.quit();
}

/// The store is created on the first account statement, not at startup — so a
/// database nobody creates an account in is byte-for-byte what it was, and the
/// `--user`/`--password` credential keeps working exactly as before.
#[test]
fn the_account_store_appears_only_when_it_is_asked_for() {
    let server = TestServer::start("acl-lazy");
    let mut root = server.client();
    root.ok_query("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    // The row ids of a database with no accounts are untouched: this is the
    // whole reason the store is lazy, since every row in this engine draws
    // its id from one counter shared by every table.
    assert_eq!(
        root.ok_query("INSERT INTO kv (body) VALUES ('first')").ok(),
        (1, 1),
        "an account store created at startup would have made this row id 2"
    );
    assert!(root.ok_query("SHOW TABLES").rows().rows.len() == 1);

    // The bootstrap credential is a superuser, exactly as the single
    // `--password` user always was.
    assert_eq!(
        root.ok_query("SHOW GRANTS").rows().cell(0, 0),
        "GRANT ALL PRIVILEGES ON *.* TO 'root'@'%' WITH GRANT OPTION"
    );

    // Creating an account materialises the store, and `root` survives into it
    // — otherwise the operator's own credential would stop working halfway
    // through their first CREATE USER.
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    server.client().quit();
    server.client_as("reader", "r-pass").quit();
    root.quit();
}

/// `--reset-superuser` is the way back in after a lost password, and it is the
/// only thing that lets the flags overwrite what is in the file.
#[test]
fn the_stored_password_wins_over_the_flags_unless_a_reset_is_asked_for() {
    let server = TestServer::start_with("acl-reset", "original", 16);
    let mut root = server.client();
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("ALTER USER 'root' IDENTIFIED BY 'rotated'");
    root.quit();

    // A restart with the old flag does not reinstate the old password: the
    // file is the authority once it has accounts.
    let options = ServerOptions {
        bind: "127.0.0.1".to_string(),
        port: 0,
        user: "root".to_string(),
        password: "original".to_string(),
        ..ServerOptions::default()
    };
    let restarted = Server::bind(server.path(), &options).expect("re-bind");
    let addr = restarted.local_addr().expect("addr");
    assert!(
        restarted
            .notices()
            .iter()
            .any(|line| line.contains("were NOT used")),
        "the operator has to be told the flags did nothing: {:?}",
        restarted.notices()
    );
    std::thread::spawn(move || {
        let _ = restarted.run();
    });
    assert_eq!(
        Client::connect(addr, "root", "original", None)
            .expect_err("the flag must not override the file")
            .code,
        1045
    );
    Client::connect(addr, "root", "rotated", None)
        .expect("the stored password is the password")
        .quit();

    // ...and the escape hatch, which needs write access to the file and says
    // what it did.
    let reset = Server::bind(
        server.path(),
        &ServerOptions {
            port: 0,
            password: "recovered".to_string(),
            reset_superuser: true,
            ..options.clone()
        },
    )
    .expect("re-bind with a reset");
    let reset_addr = reset.local_addr().expect("addr");
    assert!(
        reset
            .notices()
            .iter()
            .any(|line| line.contains("--reset-superuser")),
        "{:?}",
        reset.notices()
    );
    std::thread::spawn(move || {
        let _ = reset.run();
    });
    let mut recovered =
        Client::connect(reset_addr, "root", "recovered", None).expect("the reset worked");
    // The reset changed one account's password and nothing else: `reader` is
    // still there, untouched.
    assert_eq!(
        recovered
            .ok_query("SHOW GRANTS FOR 'reader'")
            .rows()
            .rows
            .len(),
        1
    );
    recovered.quit();
}

/// The refusals that keep the model honest, over a real connection. Each of
/// these looks like something MySQL would accept, and each would mean less
/// than it says here.
#[test]
fn the_grants_this_server_cannot_enforce_are_refused_by_name() {
    let (_server, mut root) = accounts_fixture("acl-refusals");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");

    for (sql, expected) in [
        ("GRANT SELECT (body) ON posts TO 'reader'", "column-level"),
        (
            "GRANT SELECT ON posts TO 'reader'@'localhost'",
            "host-based",
        ),
        (
            "GRANT SELECT ON posts TO 'reader' WITH GRANT OPTION",
            "delegation",
        ),
        ("GRANT SELECT ON otherdb.posts TO 'reader'", "one schema"),
        ("CREATE USER 'open'", "no password"),
        ("SET PASSWORD FOR 'reader' = 'x'", "ALTER USER"),
        ("RENAME USER 'reader' TO 'writer'", "RENAME USER"),
    ] {
        let error = root.query(sql).expect_err(sql);
        assert!(
            error.message.contains(expected),
            "{sql} should name `{expected}`, said {}",
            error.message
        );
    }

    // None of them left a trace.
    assert_eq!(
        root.ok_query("SHOW GRANTS FOR 'reader'").rows().cell(0, 0),
        "GRANT USAGE ON *.* TO 'reader'@'%'"
    );
    assert_eq!(
        root.query("SHOW GRANTS FOR 'open'")
            .expect_err("never created")
            .code,
        1133
    );
    root.quit();
}

/// The one statement shape that reaches OK without ever touching the engine —
/// `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY`, which this server records
/// nowhere and answers with a warning. It is still authorised, because "it
/// happens to do nothing" is a property of today's translation rather than a
/// rule, and a statement that reaches OK with no privilege check is the hole
/// this whole design exists to close.
#[test]
fn even_the_statement_that_does_nothing_is_authorised() {
    let (server, mut root) = accounts_fixture("acl-noop-ddl");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("GRANT ALTER ON posts TO 'reader'");
    let sql = "ALTER TABLE posts ADD CONSTRAINT fk FOREIGN KEY (id) REFERENCES vault (id)";

    // A per-table grant is deliberately not enough: with nothing planned there
    // is no table to attribute the statement to, so only a global ALTER will
    // do — the default-deny direction.
    let mut reader = server.client_as("reader", "r-pass");
    let error = reader.query(sql).expect_err("no global ALTER");
    assert_eq!(error.code, 1227);
    reader.quit();

    root.ok_query("GRANT ALTER ON *.* TO 'reader'");
    let mut reader = server.client_as("reader", "r-pass");
    let reply = reader.ok_query(sql);
    assert_eq!(reply.warnings(), 1, "and it still says it recorded nothing");
    reader.quit();
    root.quit();
}

/// `DROP INDEX` names an index, not a table, so it is resolved through the
/// catalog before it is checked — and refused globally when it resolves to
/// nothing, rather than being waved through for want of a table to name.
#[test]
fn dropping_an_index_is_checked_against_the_table_it_belongs_to() {
    let (server, mut root) = accounts_fixture("acl-drop-index");
    root.ok_query("CREATE INDEX posts_body ON posts (body)");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("GRANT DROP ON vault TO 'reader'");

    // A `DROP` on some *other* table does not reach this index.
    let mut reader = server.client_as("reader", "r-pass");
    let error = reader
        .query("DROP INDEX posts_body")
        .expect_err("the index belongs to posts");
    assert_eq!(error.code, 1142);
    assert!(error.message.contains("posts"), "{}", error.message);

    // An index name the catalog has never heard of cannot be attributed to a
    // table at all, so only a global DROP will do.
    let error = reader
        .query("DROP INDEX nothing_like_this")
        .expect_err("unattributable");
    assert_eq!(error.code, 1227);
    reader.quit();

    root.ok_query("GRANT DROP ON posts TO 'reader'");
    let mut reader = server.client_as("reader", "r-pass");
    reader.ok_query("DROP INDEX posts_body");
    reader.quit();
    root.quit();
}

/// An account statement commits what came before it and cannot itself be
/// rolled back — MySQL's rule for DDL, and load-bearing here: a `REVOKE` a
/// later `ROLLBACK` could undo is a `REVOKE` that did not happen, after the
/// client was told it had.
#[test]
fn an_account_statement_is_not_undone_by_a_rollback() {
    let (server, mut root) = accounts_fixture("acl-txn");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("GRANT SELECT ON posts TO 'reader'");

    root.ok_query("BEGIN");
    root.ok_query("REVOKE SELECT ON posts FROM 'reader'");
    root.ok_query("ROLLBACK");

    let mut reader = server.client_as("reader", "r-pass");
    let error = reader
        .query("SELECT id FROM posts")
        .expect_err("the revoke stands");
    assert_eq!(error.code, 1142);
    reader.quit();
    root.quit();
}

// =====================================================================
// statement timeout and KILL (docs/enterprise-readiness.md, blocker 8)
//
// The other half of blocker 8. Streaming and the blocking-operator ceiling
// bounded how much *memory* one statement could take; nothing bounded how much
// *time* it could take, and nothing could end one that was already running. On
// a shared server that meant one statement could hold a connection slot
// indefinitely and the only remedy was restarting the process.
//
// The correctness bar these tests exist for is not "the statement stops". It
// is that a stopped statement leaves the database in the state an un-run one
// would, and leaves its connection able to take the next statement — the same
// bar `a_commit_refused_for_size_leaves_a_usable_handle` set for a refused
// commit, and the same class of bug.
// =====================================================================

/// A server, a client, and a table big enough that a self-join over it takes
/// far longer than any timeout these tests set.
///
/// `rows` squared is the number of pairs the join below evaluates, so it is
/// what decides how long "long" is: the point is that the statement is still
/// running when the deadline arrives, on a fast machine and a slow one.
fn timeout_fixture(name: &str, rows: i64, timeout_ms: u64) -> (TestServer, Client) {
    let server = TestServer::start_tuned(name, "s3cret", |options| {
        options.max_execution_time_ms = timeout_ms;
    });
    let mut client = server.client();
    seed_pairs(&mut client, rows);
    (server, client)
}

/// Rows enough that [`SLOW`] runs for seconds rather than milliseconds, in a
/// debug build and in a release one. It is squared, so this is 25 million
/// pairs; the timeouts these tests set are hundreds of milliseconds, and the
/// gap between the two is what keeps them from being a race.
const SLOW_ROWS: i64 = 5000;

/// `rows` rows of `(id, n, body)`, in batches small enough to stay well inside
/// one transaction's size ceiling.
fn seed_pairs(client: &mut Client, rows: i64) {
    client.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, body TEXT)");
    for start in (1..=rows).step_by(500) {
        let end = (start + 499).min(rows);
        let mut sql = String::from("INSERT INTO t (id, n, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, {id}, 'body-{id}')"));
        }
        client.ok_query(&sql);
    }
}

/// A statement with no end in sight. Not an equality, so the inner side is
/// materialised and replayed for every outer row: `rows` squared comparisons,
/// and a single row of output, so nothing here is a test of how fast a socket
/// is.
const SLOW: &str = "SELECT COUNT(*) FROM t a JOIN t b ON a.n > b.n";

/// The headline: a statement past its deadline is stopped, with MySQL's own
/// code for it, and the connection that ran it is immediately usable again.
#[test]
fn a_statement_past_max_execution_time_is_stopped_and_the_connection_survives() {
    let (_server, mut client) = timeout_fixture("timeout-select", SLOW_ROWS, 500);

    let started = std::time::Instant::now();
    let error = client.query(SLOW).expect_err("the deadline must stop it");
    let elapsed = started.elapsed();

    // `ER_QUERY_TIMEOUT`, which is what `max_execution_time` raises in MySQL —
    // a driver classifies it as a resource condition rather than a bad
    // statement, which decides whether an ORM reports or retries.
    assert_eq!(error.code, 3024, "{error:?}");
    assert_eq!(error.sqlstate, "HY000");
    assert!(
        error
            .message
            .contains("maximum statement execution time exceeded"),
        "{error:?}"
    );
    // It says what happened to the data, because that is the first thing
    // anybody who sees this asks.
    assert!(error.message.contains("Nothing was written"), "{error:?}");

    // Stopped rather than merely reported after the fact. The bound is loose
    // on purpose — the engine asks once per few thousand rows, and this runs
    // in a debug build beside whatever else is on the machine — but it is far
    // below what the whole join would take.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the statement ran for {elapsed:?}, which is not a timeout"
    );

    // And the connection is not poisoned: the same connection answers the next
    // statement, including one that reads the table the stopped statement was
    // walking.
    assert_eq!(value(&mut client, "1 + 1"), "2");
    assert_eq!(
        client.ok_query("SELECT COUNT(*) FROM t").rows().cell(0, 0),
        SLOW_ROWS.to_string()
    );
    client.quit();
}

/// The correctness bar, at the wire level and on the statement that can break
/// it: an `UPDATE` stopped part-way must leave every row as it was.
///
/// The deadline is swept from very short to long enough to finish, so this
/// covers a stop during the candidate scan *and* a stop in the middle of the
/// write loop — the second of which is the one that would strand a
/// half-applied statement.
#[test]
fn a_timed_out_update_leaves_the_table_exactly_as_it_was() {
    // Seeded with no timeout — the sweep below sets its own per statement,
    // which is also what proves `SET max_execution_time` reaches the engine.
    let server = TestServer::start("timeout-update");
    let mut client = server.client();
    seed_pairs(&mut client, 4000);

    let before = client
        .ok_query("SELECT COUNT(*) FROM t WHERE body LIKE 'body-%'")
        .rows()
        .cell(0, 0);
    assert_eq!(before, "4000");

    let mut stopped = 0;
    for millis in [1u64, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 4096, 30_000] {
        client.ok_query(&format!("SET max_execution_time = {millis}"));
        match client.query("UPDATE t SET n = n + 1000, body = 'rewritten'") {
            Err(error) => {
                assert_eq!(error.code, 3024, "at {millis}ms: {error:?}");
                stopped += 1;
                // Nothing was written — checked by counting the rows that
                // still carry their original body, which a half-applied
                // update would have rewritten.
                client.ok_query("SET max_execution_time = 0");
                assert_eq!(
                    client
                        .ok_query("SELECT COUNT(*) FROM t WHERE body LIKE 'body-%'")
                        .rows()
                        .cell(0, 0),
                    "4000",
                    "a statement stopped at {millis}ms left a write behind"
                );
            }
            Ok(reply) => {
                assert_eq!(reply.ok().0, 4000, "at {millis}ms");
                assert!(
                    stopped > 0,
                    "the sweep never stopped the update, so it proves nothing"
                );
                client.ok_query("SET max_execution_time = 0");
                assert_eq!(
                    client
                        .ok_query("SELECT COUNT(*) FROM t WHERE body = 'rewritten'")
                        .rows()
                        .cell(0, 0),
                    "4000"
                );
                client.quit();
                return;
            }
        }
    }
    panic!("the update never completed; the sweep is too short");
}

/// Every number this server reports has to be one it applies — the rule
/// `Limits` exists for, after `wait_timeout` and the `net_*_timeout`s spent a
/// version being reported and never enforced.
///
/// So this checks the *pair*: `@@max_execution_time` and `SHOW VARIABLES` both
/// report the configured value, and a statement really is stopped at it; then
/// `SET max_execution_time = 0` and both report zero, and the same statement
/// really does run to completion.
#[test]
fn the_reported_max_execution_time_is_the_one_that_is_enforced() {
    // A table small enough that [`SLOW`] over it finishes in about a second,
    // which is what the "no longer enforced" half below has to wait for. The
    // configured 30 seconds is never reached by anything here — that the
    // *server's* number is enforced is what
    // `a_statement_past_max_execution_time_is_stopped_and_the_connection_survives`
    // establishes; this test is about the report matching, and about the
    // session's own number.
    let (_server, mut client) = timeout_fixture("timeout-reported", 700, 30_000);

    assert_eq!(value(&mut client, "@@max_execution_time"), "30000");
    let shown = client
        .ok_query("SHOW VARIABLES LIKE 'max_execution_time'")
        .rows();
    assert_eq!(shown.cell(0, 1), "30000");

    // Turned off by the session, reported as off, and no longer enforced.
    client.ok_query("SET max_execution_time = 0");
    assert_eq!(value(&mut client, "@@max_execution_time"), "0");
    assert_eq!(
        client
            .ok_query("SHOW VARIABLES LIKE 'max_execution_time'")
            .rows()
            .cell(0, 1),
        "0"
    );
    client.ok_query(SLOW);

    // Turned back on by the session, at a value the server was not started
    // with, and enforced at *that* — the same statement that just ran to
    // completion is now stopped.
    client.ok_query("SET SESSION max_execution_time = 1");
    assert_eq!(value(&mut client, "@@max_execution_time"), "1");
    assert_eq!(client.query(SLOW).expect_err("enforced at 1ms").code, 3024);

    // A value that is not a number is refused rather than recorded, because a
    // recorded one would be read back as if it had taken effect.
    let error = client
        .query("SET max_execution_time = 'soon'")
        .expect_err("not a number");
    assert_eq!(error.code, 1232, "{error:?}");
    assert_eq!(value(&mut client, "@@max_execution_time"), "1");
    client.quit();
}

/// The default is off, and off is reported as off. A server that shipped a
/// default timeout would break somebody's nightly report the day they
/// upgraded, which is why this one is a decision an operator makes.
#[test]
fn the_statement_timeout_is_off_unless_it_is_asked_for() {
    assert_eq!(ServerOptions::default().max_execution_time_ms, 0);
    let server = TestServer::start("timeout-default");
    let mut client = server.client();
    assert_eq!(value(&mut client, "@@max_execution_time"), "0");
    client.quit();
}

// ------------------------------------------- the ~1 MiB transaction ceiling

/// A fresh two-column table, on its own server, for the transaction-ceiling
/// tests below — the same shape `crates/inlaysql/tests/large_statements.rs`
/// uses to pin where each statement actually breaks, so the row counts that
/// module documents (buffered `INSERT`s inside `BEGIN`..`COMMIT` refused at
/// 884 for 512-byte bodies) are the numbers to expect here too.
fn ceiling_fixture(name: &str) -> (TestServer, Client) {
    let server = TestServer::start(name);
    let mut client = server.client();
    client.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)");
    (server, client)
}

/// The pairing the whole point of `@@inlaysql_max_transaction_bytes` rests on:
/// a number a client did not have to find by trial and error, and a refusal
/// that actually happens at it. Before this test could even be written the
/// refusal had nothing to check itself against — a client saw `1030` and had
/// no variable to size a batch by at all.
///
/// The margins are 10% either side of the reported budget, not the exact
/// byte, because "exact" is a property of the internal page and overflow-chain
/// encoding — `crates/inlaysql/tests/large_statements.rs` documents at length
/// why a byte count is not a clean function of a statement's shape — and that
/// is not this test's subject. Ten percent is an order of magnitude past the
/// actual per-row bookkeeping overhead (a handful of 4 KiB pages against a
/// payload approaching 1 MiB), so this does not go flaky the day that
/// bookkeeping grows by a page or two; it fails the day the reported number
/// stops being the one that gets enforced, which is the one thing worth
/// failing for.
#[test]
fn the_reported_transaction_ceiling_is_what_actually_gets_refused() {
    let (_server, mut client) = ceiling_fixture("txn-ceiling-reported");

    let budget: usize = value(&mut client, "@@inlaysql_max_transaction_bytes")
        .parse()
        .expect("a byte count");
    // `WAL_BLOCKS` (256) x `DEFAULT_PAGE_SIZE` (4096) — the one MiB
    // `docs/enterprise-readiness.md` blocker 5 documents, not a number this
    // test invented. Pinned so a change to either constant is a deliberate
    // edit here, not a silent drift between what is reported and what is
    // enforced.
    assert_eq!(budget, 256 * 4096, "@@inlaysql_max_transaction_bytes");
    assert_eq!(
        client
            .ok_query("SHOW VARIABLES LIKE 'inlaysql_max_transaction_bytes'")
            .rows()
            .cell(0, 1),
        budget.to_string()
    );

    // Comfortably under: succeeds outright, as one autocommit statement.
    let under = "x".repeat(budget - budget / 10);
    client.ok_query(&format!("INSERT INTO t (id, body) VALUES (1, '{under}')"));
    assert_eq!(
        client.ok_query("SELECT COUNT(*) FROM t").rows().cell(0, 0),
        "1"
    );

    // Comfortably over: refused, named as exactly this, and pointed back at
    // the number that predicted it.
    let over = "x".repeat(budget + budget / 10);
    let error = client
        .query(&format!("INSERT INTO t (id, body) VALUES (2, '{over}')"))
        .expect_err("a statement bigger than the reported ceiling");
    assert_eq!(error.code, 1197, "{error:?}");
    assert_eq!(error.sqlstate, "HY000");
    assert!(
        error.message.contains("inlaysql_max_transaction_bytes"),
        "no pointer back to the reported limit: {}",
        error.message
    );
    assert!(
        error.message.to_ascii_lowercase().contains("batch"),
        "no actionable fix named: {}",
        error.message
    );

    // Refused, not half-applied: still exactly the one row from before.
    assert_eq!(
        client.ok_query("SELECT COUNT(*) FROM t").rows().cell(0, 0),
        "1"
    );
    client.quit();
}

/// The other place this ceiling is enforced: mid-transaction, before the
/// statement that would overflow the record even runs
/// (`Engine::ensure_transaction_fits`). This is the shape the `ann-benchmarks`
/// bulk load actually hit (`bench/README.md`) — batched `INSERT`s inside one
/// `BEGIN`. It used to surface as `1568`, the same code every other
/// "transaction is in the wrong state" refusal gets (`ROLLBACK` with nothing
/// open, `CREATE INDEX` inside a transaction, ...), which told a client
/// nothing about which of those it had hit. It is now the same `1197` the
/// single-statement case above gets, because it is the same fact about the
/// same file caught at a different moment.
#[test]
fn a_buffered_transaction_over_the_ceiling_gets_the_same_actionable_error() {
    let (_server, mut client) = ceiling_fixture("txn-ceiling-buffered");
    client.ok_query("BEGIN");

    let body = "x".repeat(512);
    let mut id = 0i64;
    let error = loop {
        id += 1;
        assert!(
            id < 20_000,
            "no refusal after a whole region's worth of rows"
        );
        match client.query(&format!("INSERT INTO t (id, body) VALUES ({id}, '{body}')")) {
            Ok(_) => continue,
            Err(error) => break error,
        }
    };
    assert_eq!(error.code, 1197, "{error:?}");
    assert_eq!(error.sqlstate, "HY000");
    assert!(
        error.message.contains("inlaysql_max_transaction_bytes"),
        "no pointer back to the reported limit: {}",
        error.message
    );

    // The refusal arrived before the statement ran, so nothing it would have
    // written was ever buffered — what was already there still is, and still
    // commits. A caller draining a large import in batches depends on exactly
    // this.
    client.ok_query("COMMIT");
    assert_eq!(
        client.ok_query("SELECT COUNT(*) FROM t").rows().cell(0, 0),
        (id - 1).to_string()
    );
    client.quit();
}

// --------------------------------------------------- ANN recall/latency knob

/// A server with a four-dimensional vector column and a graph over it.
fn vector_fixture(name: &str) -> (TestServer, Client) {
    let server = TestServer::start(name);
    let mut client = server.client();
    client.ok_query("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR(4))");
    client.ok_query("CREATE INDEX docs_embedding ON docs (embedding)");
    for id in 1..=32 {
        let value = id as f32 / 32.0;
        client.ok_query(&format!(
            "INSERT INTO docs (id, embedding) VALUES ({id}, \
             vector('[{value}, {a}, {b}, 1.0]'))",
            a = 1.0 - value,
            b = value * value
        ));
    }
    (server, client)
}

/// The `EXPLAIN` line describing the vector search, which is where the
/// operating point is reported.
fn vector_plan(client: &mut Client, limit: usize) -> String {
    client
        .ok_query(&format!(
            "EXPLAIN SELECT id, vector_score(embedding, vector('[1,0,0,1]')) AS score \
             FROM docs ORDER BY score DESC LIMIT {limit}"
        ))
        .rows()
        .cell(0, 2)
}

/// Every number this server reports has to be one it applies — and this one is
/// the recall of the rows a client gets back, not just how long it waits for
/// them. `@@inlaysql_hnsw_ef_search` is read off the connection's own
/// [`Control`], and `EXPLAIN` reports the `ef` the *engine* will search with;
/// asserting the pair is what makes the two one number rather than two that
/// happen to agree today.
///
/// pgvector spells this `SET hnsw.ef_search`. A MySQL system variable cannot
/// hold a dot, and a bare `ef_search` would be a name MySQL does not have
/// sitting in MySQL's own namespace, so it carries this server's prefix — the
/// same decision `inlaysql_statement_text` records.
#[test]
fn the_reported_ef_search_is_the_one_the_engine_searches_with() {
    let (_server, mut client) = vector_fixture("ef-search-reported");

    // The default: nothing imposed, reported as `0`, and the plan shows the
    // index's own tuning — `max(ef_search = 64, 40 candidates * 2)`.
    assert_eq!(value(&mut client, "@@inlaysql_hnsw_ef_search"), "0");
    assert_eq!(
        client
            .ok_query("SHOW VARIABLES LIKE 'inlaysql_hnsw_ef_search'")
            .rows()
            .cell(0, 1),
        "0"
    );
    let untuned = vector_plan(&mut client, 10);
    assert!(
        untuned.contains("(ef=80)"),
        "the default operating point was not reported: {untuned}"
    );

    // Set by the session, reported as set, and *applied*: the plan now names
    // the session's number and not the index's.
    client.ok_query("SET inlaysql_hnsw_ef_search = 200");
    assert_eq!(value(&mut client, "@@inlaysql_hnsw_ef_search"), "200");
    assert_eq!(
        client
            .ok_query("SHOW VARIABLES LIKE 'inlaysql_hnsw_ef_search'")
            .rows()
            .cell(0, 1),
        "200"
    );
    let tuned = vector_plan(&mut client, 10);
    assert!(
        tuned.contains("(ef=200)"),
        "the session's ef_search did not reach the engine: {tuned}"
    );

    // `SET SESSION` is the same variable, and `DEFAULT` puts the index's own
    // tuning back — which is the only way out of a tuned session short of
    // reconnecting, and what a pool sends when it hands a connection on.
    client.ok_query("SET SESSION inlaysql_hnsw_ef_search = 512");
    assert_eq!(value(&mut client, "@@inlaysql_hnsw_ef_search"), "512");
    client.ok_query("SET inlaysql_hnsw_ef_search = DEFAULT");
    assert_eq!(value(&mut client, "@@inlaysql_hnsw_ef_search"), "0");
    assert_eq!(vector_plan(&mut client, 10), untuned);

    client.quit();
}

/// Two refusals, both of which the alternative would have made silent.
///
/// A value that is not a number is refused at the `SET` rather than recorded
/// as an inert session variable — recorded, a client would read `'wide'` back
/// as if it had taken effect while every search went on using something else.
/// A value narrower than the candidate list the query needs is refused when
/// that query runs, because a beam narrower than the answer cannot hold it and
/// the two silent alternatives are searching at a number the client did not
/// choose, or returning fewer rows than were asked for without saying so.
#[test]
fn an_ef_search_that_cannot_be_honoured_is_refused() {
    let (_server, mut client) = vector_fixture("ef-search-refused");

    client.ok_query("SET inlaysql_hnsw_ef_search = 128");
    let error = client
        .query("SET inlaysql_hnsw_ef_search = 'wide'")
        .expect_err("not a candidate-list size");
    assert_eq!(error.code, 1232, "{error:?}");
    // And the refusal left the previous value standing, rather than half
    // applying the statement.
    assert_eq!(value(&mut client, "@@inlaysql_hnsw_ef_search"), "128");

    // Five against a `LIMIT 10`. The `SET` is accepted — on its own it is a
    // perfectly good number, and which queries it can answer is not knowable
    // until one arrives — and the query is refused, naming the smallest value
    // that would have worked.
    client.ok_query("SET inlaysql_hnsw_ef_search = 5");
    assert_eq!(value(&mut client, "@@inlaysql_hnsw_ef_search"), "5");
    let error = client
        .query(
            "SELECT id, vector_score(embedding, vector('[1,0,0,1]')) AS score \
             FROM docs ORDER BY score DESC LIMIT 10",
        )
        .expect_err("a beam narrower than the answer must not be answered");
    assert!(
        error.message.contains("10"),
        "the refusal did not name the minimum that would work: {error:?}"
    );
    // The connection is unharmed: this is a statement error, not a protocol
    // one, and the next statement works.
    assert_eq!(value(&mut client, "@@inlaysql_hnsw_ef_search"), "5");

    // A beam exactly as wide as the answer is legal — the floor is the query's
    // `LIMIT`, not the candidate count, so the cheap end of the recall/latency
    // trade stays reachable.
    client.ok_query("SET inlaysql_hnsw_ef_search = 10");
    client.ok_query(
        "SELECT id, vector_score(embedding, vector('[1,0,0,1]')) AS score \
         FROM docs ORDER BY score DESC LIMIT 10",
    );

    client.quit();
}

// ------------------------------------------- AHL-478: bound embeddings

/// **The defect this whole item exists to close.**
///
/// A `VECTOR` was the one value type this server could not accept as a bound
/// parameter: a `?` into a vector column failed with 1366 whatever was bound to
/// it, so every embedding had to be inlined into the SQL as decimal text and
/// re-parsed. Measured on `glove-25-angular`, that was 363.9 MiB of SQL for a
/// 112.9 MiB corpus — 3.22x — plus 11-18 µs of client-side float formatting on
/// every query. Embeddings are the one thing in a database that is always
/// machine-generated and never typed by a human, so binding them is not a
/// convenience: it is how they are supposed to arrive.
///
/// The encoding is `dim` little-endian `f32`s bound as a string parameter,
/// which is MySQL 9's own `VECTOR` storage format and what every driver can
/// send as a byte string. Which `?` is an embedding comes from the *statement*
/// (`Statement::parameter_vector_dims`) and not from the packet, because the
/// packet cannot say — see `decode_vector_param`.
#[test]
fn an_embedding_binds_as_packed_f32_and_reads_back_unchanged() {
    let server = TestServer::start("bound-embedding");
    let mut client = server.client();
    client.ok_query("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR(4))");
    client.ok_query("CREATE INDEX docs_embedding ON docs (embedding)");

    let insert = client
        .prepare("INSERT INTO docs (id, embedding) VALUES (?, ?)")
        .expect("prepare insert");
    // The reply describes the embedding slot as a binary string of exactly the
    // width it must carry, rather than as the utf8mb4 text every other
    // parameter is described as. Nothing forces a client to read this, but a
    // server that advertised text for a slot it will only take packed floats
    // in would be saying something untrue.
    assert_eq!(
        insert.params,
        vec![
            ("?1".to_string(), 0xfd), // MYSQL_TYPE_VAR_STRING
            ("?2".to_string(), 0xfc), // MYSQL_TYPE_BLOB, 16 bytes wide
        ]
    );

    // Values chosen to be exactly representable in `f32` so that "unchanged"
    // below means bit-for-bit and not "close enough": a round trip that
    // silently rounded would still pass a tolerance test.
    let corpus: [[f32; 4]; 3] = [
        [0.5, 0.25, -0.125, 1.0],
        [1.0, 0.0, 0.0, 0.0],
        [-1.0, 0.75, 0.5, -0.0625],
    ];
    for (index, embedding) in corpus.iter().enumerate() {
        let (affected, _) = client
            .execute(
                &insert,
                &[Param::Int(index as i64 + 1), Param::vector(embedding)],
            )
            .expect("execute insert")
            .ok();
        assert_eq!(affected, 1);
    }

    // Read back through the binary protocol: a `VECTOR` column comes out as the
    // JSON text `vector()` accepts, rendered with `f32`'s shortest
    // round-tripping form, so no component moved on the way through.
    let select = client
        .prepare("SELECT id, embedding FROM docs ORDER BY id")
        .expect("prepare select");
    let rows = client.execute(&select, &[]).expect("execute select").rows();
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.cell(0, 1), "[0.5,0.25,-0.125,1]");
    assert_eq!(rows.cell(1, 1), "[1,0,0,0]");
    assert_eq!(rows.cell(2, 1), "[-1,0.75,0.5,-0.0625]");

    // The query side: `vector_score(column, ?)` takes the same packed form, so
    // a search no longer has to format its query embedding as decimal text
    // either. Row 2 is the exact corpus vector, so it scores first.
    let search = client
        .prepare(
            "SELECT id, vector_score(embedding, ?) AS score \
             FROM docs ORDER BY score DESC LIMIT 3",
        )
        .expect("prepare search");
    assert_eq!(search.params, vec![("?1".to_string(), 0xfc)]);
    let rows = client
        .execute(&search, &[Param::vector(&[1.0, 0.0, 0.0, 0.0])])
        .expect("execute search")
        .rows();
    assert_eq!(rows.cell(0, 0), "2", "the identical vector ranks first");

    // `UPDATE ... SET embedding = ?` binds the same way; so does the `SET` of
    // an upsert, which resolves its assignments through the same path.
    let update = client
        .prepare("UPDATE docs SET embedding = ? WHERE id = ?")
        .expect("prepare update");
    let (affected, _) = client
        .execute(
            &update,
            &[Param::vector(&[0.25, 0.25, 0.25, 0.25]), Param::Int(1)],
        )
        .expect("execute update")
        .ok();
    assert_eq!(affected, 1);
    let rows = client
        .execute(&select, &[])
        .expect("execute select after update")
        .rows();
    assert_eq!(rows.cell(0, 1), "[0.25,0.25,0.25,0.25]");

    let upsert = client
        .prepare(
            "INSERT INTO docs (id, embedding) VALUES (?, ?) \
             ON CONFLICT (id) DO UPDATE SET embedding = ?",
        )
        .expect("prepare upsert");
    let (affected, _) = client
        .execute(
            &upsert,
            &[
                Param::Int(1),
                Param::vector(&[0.0, 0.0, 0.0, 1.0]),
                Param::vector(&[0.5, 0.5, 0.5, 0.5]),
            ],
        )
        .expect("execute upsert")
        .ok();
    assert_eq!(affected, 1);
    let rows = client
        .execute(&select, &[])
        .expect("execute select after upsert")
        .rows();
    assert_eq!(rows.cell(0, 1), "[0.5,0.5,0.5,0.5]");

    // A NULL embedding is still a NULL, not a zero vector: the null bitmap is
    // read before the payload is, and the vector path never sees it.
    let (affected, _) = client
        .execute(&insert, &[Param::Int(9), Param::Null])
        .expect("execute insert null")
        .ok();
    assert_eq!(affected, 1);

    client.quit();
}

/// Every way a bound embedding can be wrong, and the refusal each one gets.
///
/// The alternative to refusing is worse than an error: an HNSW graph is built
/// once and queried forever, so a NaN that reaches it makes its own node
/// unreachable from every neighbour and silently drops that row out of every
/// search from then on. A short payload is a *different* embedding, not a
/// shorter one. None of these announce themselves later, so each is refused at
/// the point the bytes arrive, and the row count is checked afterwards to prove
/// nothing was written on the way to the error.
#[test]
fn a_malformed_embedding_parameter_is_refused_rather_than_indexed() {
    let server = TestServer::start("bad-embedding");
    let mut client = server.client();
    client.ok_query("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR(4))");
    client.ok_query("CREATE INDEX docs_embedding ON docs (embedding)");
    let insert = client
        .prepare("INSERT INTO docs (id, embedding) VALUES (?, ?)")
        .expect("prepare insert");

    let refused = |client: &mut Client, id: i64, param: Param, expected: &str| {
        let error = client
            .execute(&insert, &[Param::Int(id), param])
            .expect_err("a malformed embedding must not be accepted");
        assert_eq!(error.code, 1366, "{error:?}");
        assert!(
            error.message.contains(expected),
            "the refusal did not say why: {error:?}"
        );
    };

    // Too few and too many components. Both are exactly the case a length
    // check exists for: an index asked to compare a prefix would answer, and
    // answer wrongly.
    refused(
        &mut client,
        1,
        Param::vector(&[1.0, 2.0, 3.0]),
        "16 bytes of little-endian f32, but 12 bytes were bound",
    );
    refused(
        &mut client,
        2,
        Param::vector(&[1.0, 2.0, 3.0, 4.0, 5.0]),
        "but 20 bytes were bound",
    );

    // A payload that is not a whole number of floats at all — the truncated
    // write a client gets from a short read or a sliced buffer.
    refused(
        &mut client,
        3,
        Param::Bytes {
            ty: 0xfe,
            bytes: vec![0u8; 14],
        },
        "but 14 bytes were bound",
    );
    refused(
        &mut client,
        4,
        Param::Bytes {
            ty: 0xfe,
            bytes: Vec::new(),
        },
        "but 0 bytes were bound",
    );

    // Non-finite components, named individually so the client can find the one
    // its own pipeline produced.
    refused(
        &mut client,
        5,
        Param::vector(&[1.0, f32::NAN, 3.0, 4.0]),
        "component 1 is NaN",
    );
    refused(
        &mut client,
        6,
        Param::vector(&[1.0, 2.0, f32::INFINITY, 4.0]),
        "component 2 is inf",
    );
    refused(
        &mut client,
        7,
        Param::vector(&[f32::NEG_INFINITY, 2.0, 3.0, 4.0]),
        "component 0 is -inf",
    );

    // The decimal text that `vector('[...]')` takes is *not* the parameter
    // encoding, and the refusal says so rather than leaving the caller to
    // guess: this is the exact mistake the old inlining path trains a caller
    // to make.
    refused(
        &mut client,
        8,
        Param::Str("[1.0,2.0,3.0,4.0]".to_string()),
        "pack the floats instead",
    );

    // A type code that is not a string at all. Its payload is fixed-width
    // rather than length-encoded, so reading it as bytes would misframe every
    // parameter after it — refused on the code, before anything is read.
    refused(
        &mut client,
        9,
        Param::Bytes {
            ty: 0x08, // MYSQL_TYPE_LONGLONG
            bytes: 7i64.to_le_bytes().to_vec(),
        },
        "bound as MySQL type 0x08",
    );

    // Nothing above reached the table, and the connection is in step after
    // every one of them: these are statement errors, not protocol ones.
    let rows = client.ok_query("SELECT COUNT(*) FROM docs").rows();
    assert_eq!(
        rows.cell(0, 0),
        "0",
        "a refused embedding was still written"
    );
    let (affected, _) = client
        .execute(
            &insert,
            &[Param::Int(10), Param::vector(&[1.0, 0.0, 0.0, 0.0])],
        )
        .expect("a good embedding still binds after nine refusals")
        .ok();
    assert_eq!(affected, 1);

    client.quit();
}

/// The parameter path widened; the SQL text path did not.
///
/// Packed `f32` is what a *bound* embedding looks like, and it stays that way:
/// writing the same bytes as a blob literal into a vector column is still the
/// type error it always was, and a `?` that is not an embedding still arrives
/// as the text or bytes it was sent as. The two paths are separate on purpose —
/// a text spelling that accepted raw bytes would have no way to tell them from
/// a `BLOB` a caller meant literally.
#[test]
fn binding_an_embedding_does_not_widen_the_sql_text_path() {
    let server = TestServer::start("embedding-text-path");
    let mut client = server.client();
    client.ok_query(
        "CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR(4), payload BLOB, body TEXT)",
    );

    // A blob literal spelling the same 16 bytes is refused, as it was before.
    let error = client
        .query("INSERT INTO docs (id, embedding) VALUES (1, X'0000803F00000000000000000000803F')")
        .expect_err("a blob literal is not a vector literal");
    assert_eq!(error.code, 1366, "{error:?}");
    assert!(error.message.contains("VECTOR(4)"), "{error:?}");

    // And so is `vector(?)`, which would mean re-parsing decimal text on every
    // execution when the parameter can carry the floats themselves.
    let error = client
        .prepare("INSERT INTO docs (id, embedding) VALUES (?, vector(?))")
        .expect_err("vector() still takes a literal");
    assert_eq!(error.code, 1366, "{error:?}");

    // A `?` bound to a BLOB or TEXT column is untouched by any of this: the
    // same bytes that would be an embedding at a vector slot stay bytes here.
    let insert = client
        .prepare("INSERT INTO docs (id, payload, body) VALUES (?, ?, ?)")
        .expect("prepare");
    assert_eq!(
        insert.params.iter().map(|(_, ty)| *ty).collect::<Vec<_>>(),
        vec![0xfd, 0xfd, 0xfd],
        "no slot here takes an embedding, so none is described as binary"
    );
    let packed: Vec<u8> = [1.0f32, 0.0, 0.0, 1.0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    client
        .execute(
            &insert,
            &[
                Param::Int(1),
                Param::Bytes {
                    ty: 0xfe,
                    bytes: packed.clone(),
                },
                Param::Str("plain".to_string()),
            ],
        )
        .expect("a blob parameter is still a blob")
        .ok();
    let rows = client
        .ok_query("SELECT id, LENGTH(payload), body FROM docs")
        .rows();
    assert_eq!(rows.cell(0, 1), "16", "the blob kept its bytes");
    assert_eq!(rows.cell(0, 2), "plain");

    client.quit();
}

/// `KILL QUERY` stops the statement and leaves the connection standing — which
/// is the whole difference between it and `KILL CONNECTION`, and the reason a
/// pool can use it.
#[test]
fn kill_query_stops_a_running_statement_and_leaves_the_connection_usable() {
    let (server, mut victim) = timeout_fixture("kill-query", SLOW_ROWS, 0);
    let id: u32 = value(&mut victim, "CONNECTION_ID()").parse().expect("id");

    let running = std::thread::spawn(move || {
        let outcome = victim.query(SLOW);
        (victim, outcome)
    });

    // Long enough that the statement is certainly inside the join rather than
    // still being planned, so this tests interrupting work rather than racing
    // the statement's own start.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut killer = server.client();
    killer.ok_query(&format!("KILL QUERY {id}"));

    let (mut victim, outcome) = running.join().expect("the victim thread");
    let error = outcome.expect_err("the statement must have been stopped");
    // `ER_QUERY_INTERRUPTED`, not the timeout code: a client must be able to
    // tell "somebody stopped this" from "this ran too long", because the first
    // is a decision and the second is a limit.
    assert_eq!(error.code, 1317, "{error:?}");
    assert_eq!(error.sqlstate, "70100");
    assert!(error.message.contains("Nothing was written"), "{error:?}");

    // The connection is still there, and the kill does not carry over to the
    // next statement it is sent.
    assert_eq!(value(&mut victim, "1 + 1"), "2");
    assert_eq!(
        victim.ok_query("SELECT COUNT(*) FROM t").rows().cell(0, 0),
        SLOW_ROWS.to_string()
    );
    victim.quit();
    killer.quit();
}

/// A `KILL` of an idle connection has nothing to interrupt, so the flag alone
/// would not be noticed until the client sent something — up to `wait_timeout`,
/// which is eight hours. The socket is shut down instead, which is what makes
/// `KILL CONNECTION` mean the same thing to an idle connection as to a busy
/// one.
#[test]
fn kill_connection_ends_an_idle_connection_at_once() {
    let server = TestServer::start("kill-idle");
    let mut victim = server.client();
    let id: u32 = value(&mut victim, "CONNECTION_ID()").parse().expect("id");

    let mut killer = server.client();
    killer.ok_query(&format!("KILL CONNECTION {id}"));

    // Give the killed thread a moment to unwind; the shutdown is what makes
    // this a wait of milliseconds rather than of `wait_timeout`.
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !victim.still_connected(),
        "a killed connection answered a ping"
    );
    // The rest of the server is untouched.
    assert_eq!(value(&mut killer, "1 + 1"), "2");
    killer.quit();
}

/// `COM_PROCESS_KILL` is the same operation with an older spelling —
/// `mysqladmin kill` and several drivers still send it — so it goes through
/// the same registry and the same privilege check.
#[test]
fn com_process_kill_ends_a_connection_like_the_statement_does() {
    let server = TestServer::start("kill-command");
    let mut victim = server.client();
    let id: u32 = value(&mut victim, "CONNECTION_ID()").parse().expect("id");

    let mut killer = server.client();
    killer.command(0x0c, &id.to_le_bytes());
    killer.read_reply(false).expect("COM_PROCESS_KILL").ok();

    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !victim.still_connected(),
        "a connection killed by COM_PROCESS_KILL answered a ping"
    );
    killer.quit();
}

/// The refusals. A `KILL` is one connection reaching into another, so the two
/// answers that matter are "there is no such connection" and "that one is not
/// yours" — and the second must hold for an ordinary account against another
/// account's connection, while a superuser may.
#[test]
fn killing_another_account_needs_the_superuser() {
    let server = TestServer::start("kill-privileges");
    let mut root = server.client();
    root.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY)");
    root.ok_query("CREATE USER 'alice' IDENTIFIED BY 'a-pass'");
    root.ok_query("CREATE USER 'bob' IDENTIFIED BY 'b-pass'");
    root.ok_query("GRANT SELECT ON t TO 'alice'");
    root.ok_query("GRANT SELECT ON t TO 'bob'");

    let mut alice = server.client_as("alice", "a-pass");
    let mut bob = server.client_as("bob", "b-pass");
    let alice_id: u32 = value(&mut alice, "CONNECTION_ID()").parse().expect("id");
    let bob_id: u32 = value(&mut bob, "CONNECTION_ID()").parse().expect("id");

    // An id nobody is using: `ER_NO_SUCH_THREAD`.
    let error = bob
        .query("KILL QUERY 999999")
        .expect_err("no such connection");
    assert_eq!(error.code, 1094, "{error:?}");

    // Somebody else's, without the superuser: `ER_KILL_DENIED_ERROR`, and
    // alice is untouched.
    let error = bob
        .query(&format!("KILL QUERY {alice_id}"))
        .expect_err("not bob's connection");
    assert_eq!(error.code, 1095, "{error:?}");
    assert_eq!(value(&mut alice, "1 + 1"), "2");

    // Bob's own is always allowed — `KILL QUERY` on an idle connection stops
    // nothing, so the connection is still there afterwards.
    bob.ok_query(&format!("KILL QUERY {bob_id}"));
    assert_eq!(value(&mut bob, "1 + 1"), "2");

    // A superuser may kill anybody's.
    root.ok_query(&format!("KILL CONNECTION {alice_id}"));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!alice.still_connected(), "the superuser's kill did nothing");

    // And an argument that is not a plain id is refused rather than guessed
    // at, because guessing here ends the wrong connection.
    let error = root
        .query("KILL QUERY (SELECT CONNECTION_ID())")
        .expect_err("not a plain id");
    assert_eq!(error.code, 1235, "{error:?}");

    bob.quit();
    root.quit();
}

/// A `KILL QUERY` that lands while the connection is idle applies to no
/// statement at all — it must not fall on whatever the client sends next.
/// Without this a killed connection would refuse every statement it was ever
/// sent again, which is a worse outcome than not honouring the kill.
#[test]
fn a_kill_query_does_not_fall_on_the_next_statement() {
    let server = TestServer::start("kill-not-next");
    let mut victim = server.client();
    let id: u32 = value(&mut victim, "CONNECTION_ID()").parse().expect("id");

    let mut killer = server.client();
    killer.ok_query(&format!("KILL QUERY {id}"));
    std::thread::sleep(std::time::Duration::from_millis(100));

    assert_eq!(value(&mut victim, "1 + 1"), "2");
    assert_eq!(value(&mut victim, "2 + 2"), "4");
    victim.quit();
    killer.quit();
}

// =====================================================================
// observability: SHOW PROCESSLIST and SHOW STATUS
// =====================================================================
//
// `docs/enterprise-readiness.md`, blocker 10. The two questions an operator
// could not ask this server before these existed are "what is it doing right
// now" and "what has it been doing", and the tests below are written against
// those two sentences rather than against the implementation: what a client
// sees over a socket, and who is allowed to see it.

/// One `SHOW STATUS` value, by name. Panics rather than defaulting: a counter
/// this server does not report is a different failure from one reporting zero,
/// and a test that silently accepted the first would pass against a server
/// that reported nothing at all.
fn status(client: &mut Client, scope: &str, name: &str) -> u64 {
    let rows = client
        .ok_query(&format!("SHOW {scope} STATUS LIKE '{name}'"))
        .rows();
    assert_eq!(
        rows.rows.len(),
        1,
        "SHOW {scope} STATUS LIKE '{name}' matched {} rows",
        rows.rows.len()
    );
    rows.cell(0, 1)
        .parse()
        .unwrap_or_else(|_| panic!("{name} is not a number: {}", rows.cell(0, 1)))
}

/// The whole process list, as this client sees it.
fn processlist(client: &mut Client, full: bool) -> Rows {
    let sql = if full {
        "SHOW FULL PROCESSLIST"
    } else {
        "SHOW PROCESSLIST"
    };
    client.ok_query(sql).rows()
}

/// The columns are MySQL's eight, in MySQL's order, because `mysqladmin
/// processlist` and every admin UI read them positionally. And the connection
/// asking is in its own list, doing the thing it is doing.
#[test]
fn show_processlist_reports_mysqls_columns_for_this_connection() {
    let server = TestServer::start("processlist-columns");
    let mut client = Client::connect(server.addr, "root", &server.password, Some("app"))
        .expect("connect with a schema");
    let id = value(&mut client, "CONNECTION_ID()");

    let rows = processlist(&mut client, false);
    assert_eq!(
        rows.columns,
        vec!["Id", "User", "Host", "db", "Command", "Time", "State", "Info"],
        "the column list a positional client reads"
    );

    let mine = rows
        .rows
        .iter()
        .find(|row| row[0].as_deref() == Some(id.as_str()))
        .unwrap_or_else(|| panic!("connection {id} is not in its own process list: {rows:?}"));
    assert_eq!(mine[1].as_deref(), Some("root"));
    assert!(
        mine[2]
            .as_deref()
            .is_some_and(|host| host.contains("127.0.0.1")),
        "Host should be the peer address, got {:?}",
        mine[2]
    );
    assert_eq!(mine[3].as_deref(), Some("app"), "db follows the schema");
    assert_eq!(
        mine[4].as_deref(),
        Some("Query"),
        "the connection asking is running a query — its own"
    );
    assert!(
        mine[5]
            .as_deref()
            .is_some_and(|time| time.parse::<u64>().is_ok()),
        "Time should be seconds, got {:?}",
        mine[5]
    );
    // `State` is NULL and stays NULL: this engine has no per-stage execution
    // tracking, and "Sending data" would be invented. See `process_list`.
    assert_eq!(mine[6], None, "State must be NULL rather than a guess");
    // And `Info` is NULL, because this server holds no statement text unless
    // it was asked to.
    assert_eq!(mine[7], None, "Info must be NULL by default");

    // `USE` moves the `db` column, because the process list reads the same
    // value the session would use rather than a copy taken at connect.
    client.ok_query("USE other");
    let rows = processlist(&mut client, false);
    let mine = rows
        .rows
        .iter()
        .find(|row| row[0].as_deref() == Some(id.as_str()))
        .expect("still there");
    assert_eq!(mine[3].as_deref(), Some("other"));
    client.quit();
}

/// **The privilege rule.** An ordinary account sees its own connections and
/// nothing else; a superuser sees them all. Exactly `KILL`'s rule, so an id in
/// the list is always an id the viewer could act on — see
/// `killing_another_account_needs_the_superuser` for the other half.
#[test]
fn a_non_superuser_sees_only_its_own_connections() {
    let server = TestServer::start("processlist-privileges");
    let mut root = server.client();
    root.ok_query("CREATE USER 'alice' IDENTIFIED BY 'a-pass'");
    root.ok_query("CREATE USER 'bob' IDENTIFIED BY 'b-pass'");

    let mut alice = server.client_as("alice", "a-pass");
    let mut alice_too = server.client_as("alice", "a-pass");
    let mut bob = server.client_as("bob", "b-pass");
    let root_id = value(&mut root, "CONNECTION_ID()");
    let alice_id = value(&mut alice, "CONNECTION_ID()");
    let alice_too_id = value(&mut alice_too, "CONNECTION_ID()");
    let bob_id = value(&mut bob, "CONNECTION_ID()");

    // Alice: her own two connections, and neither of the others'.
    let seen = processlist(&mut alice, false).column("Id");
    assert!(
        seen.contains(&alice_id),
        "alice cannot see herself: {seen:?}"
    );
    assert!(
        seen.contains(&alice_too_id),
        "alice cannot see her own second connection: {seen:?}"
    );
    assert!(
        !seen.contains(&bob_id),
        "alice can see bob's connection: {seen:?}"
    );
    assert!(
        !seen.contains(&root_id),
        "alice can see the superuser's connection: {seen:?}"
    );
    // Nothing about another account leaks through the other columns either.
    for user in processlist(&mut alice, false).column("User") {
        assert_eq!(user, "alice", "another account's name appeared in the list");
    }

    // Bob: only his own.
    let seen = processlist(&mut bob, false).column("Id");
    assert_eq!(seen, vec![bob_id.clone()], "bob saw more than his own");

    // The superuser: everybody's.
    let seen = processlist(&mut root, false).column("Id");
    for id in [&root_id, &alice_id, &alice_too_id, &bob_id] {
        assert!(seen.contains(id), "the superuser cannot see {id}: {seen:?}");
    }

    // Every id alice was shown is one she may `KILL`, which is the property
    // that makes showing it useful rather than merely safe.
    for id in processlist(&mut alice, false).column("Id") {
        if id == alice_id {
            continue; // killing her own connection would end this test's client
        }
        alice.ok_query(&format!("KILL QUERY {id}"));
    }
    // And an id she was not shown is still refused.
    let error = alice
        .query(&format!("KILL QUERY {bob_id}"))
        .expect_err("bob's connection is not alice's");
    assert_eq!(error.code, 1095, "{error:?}");

    root.quit();
    alice.quit();
    bob.quit();
}

/// A superuser watching a long statement is the thing a process list exists
/// for: the connection is `Query`, not `Sleep`, and the `Time` column is what
/// tells an operator whether to reach for `KILL`.
#[test]
fn a_running_statement_is_visible_to_a_superuser_while_it_runs() {
    let (server, mut victim) = timeout_fixture("processlist-running", SLOW_ROWS, 0);
    let id: u32 = value(&mut victim, "CONNECTION_ID()").parse().expect("id");

    let running = std::thread::spawn(move || {
        let outcome = victim.query(SLOW);
        (victim, outcome)
    });
    // Long enough that the statement is inside the join rather than still
    // being planned — the same wait `kill_query_stops_a_running_statement`
    // takes, and for the same reason.
    std::thread::sleep(Duration::from_millis(300));

    let mut watcher = server.client();
    let rows = processlist(&mut watcher, false);
    let victim_row = rows
        .rows
        .iter()
        .find(|row| row[0].as_deref() == Some(id.to_string().as_str()))
        .unwrap_or_else(|| panic!("the running connection is not listed: {rows:?}"));
    assert_eq!(
        victim_row[4].as_deref(),
        Some("Query"),
        "a connection running a statement must not read as Sleep"
    );
    assert_eq!(
        status(&mut watcher, "GLOBAL", "Threads_running"),
        2,
        "the victim's statement and this one"
    );

    // And the list is what `KILL` acts on: the id it showed stops the
    // statement it said was running.
    watcher.ok_query(&format!("KILL QUERY {id}"));
    let (mut victim, outcome) = running.join().expect("the victim thread");
    assert_eq!(outcome.expect_err("stopped").code, 1317);

    // Once it is over the same connection reads as idle, with no statement.
    std::thread::sleep(Duration::from_millis(100));
    let rows = processlist(&mut watcher, false);
    let victim_row = rows
        .rows
        .iter()
        .find(|row| row[0].as_deref() == Some(id.to_string().as_str()))
        .expect("still connected");
    assert_eq!(victim_row[4].as_deref(), Some("Sleep"));
    victim.quit();
    watcher.quit();
}

/// **The policy.** Statement text is user data, so the default is that this
/// server holds none — `Info` is `NULL` even for a statement that is running.
/// `--statement-text` is the explicit, documented way to change that, and
/// `@@inlaysql_statement_text` reports which it is.
#[test]
fn processlist_info_is_null_unless_statement_text_was_asked_for() {
    // Off: the default.
    let server = TestServer::start("processlist-info-off");
    let mut client = server.client();
    assert_eq!(value(&mut client, "@@inlaysql_statement_text"), "OFF");
    let rows = processlist(&mut client, true);
    assert!(
        rows.column("Info").iter().all(|info| info == "NULL"),
        "a default server must not report statement text: {rows:?}"
    );
    client.quit();

    // On: the connection's own statement is reported, and it is the statement
    // that is running — the `SHOW FULL PROCESSLIST` itself.
    let server = TestServer::start_tuned("processlist-info-on", "s3cret", |options| {
        options.statement_text = true;
    });
    let mut client = server.client();
    assert_eq!(value(&mut client, "@@inlaysql_statement_text"), "ON");
    let id = value(&mut client, "CONNECTION_ID()");
    let rows = processlist(&mut client, true);
    let mine = rows
        .rows
        .iter()
        .find(|row| row[0].as_deref() == Some(id.as_str()))
        .expect("in its own list");
    assert_eq!(mine[7].as_deref(), Some("SHOW FULL PROCESSLIST"));

    // Without FULL the statement is cut at a hundred characters, MySQL's own
    // number, so one connection running a generated 40 KB INSERT does not cost
    // the operator every other row. The padding is a trailing comment, which
    // is part of the text the client sent and therefore part of what `Info`
    // reports — the statement in flight is the connection's own `SHOW`.
    let padding = "x".repeat(200);
    let info_for = |client: &mut Client, sql: &str| -> String {
        let rows = client.ok_query(sql).rows();
        rows.rows
            .iter()
            .find(|row| row[0].as_deref() == Some(id.as_str()))
            .expect("in its own list")[7]
            .clone()
            .expect("Info")
    };
    let short = info_for(&mut client, &format!("SHOW PROCESSLIST -- {padding}"));
    assert_eq!(short.chars().count(), 100, "truncated to MySQL's 100");
    assert!(short.starts_with("SHOW PROCESSLIST -- xx"), "{short}");
    let whole = info_for(&mut client, &format!("SHOW FULL PROCESSLIST -- {padding}"));
    assert_eq!(
        whole,
        format!("SHOW FULL PROCESSLIST -- {padding}"),
        "FULL must report the whole statement"
    );

    // An idle connection has no statement in flight, so nothing lingers: a
    // second connection asking sees the first as Sleep with a NULL Info.
    let mut watcher = server.client();
    std::thread::sleep(Duration::from_millis(50));
    let rows = processlist(&mut watcher, true);
    let theirs = rows
        .rows
        .iter()
        .find(|row| row[0].as_deref() == Some(id.as_str()))
        .expect("the idle connection");
    assert_eq!(theirs[4].as_deref(), Some("Sleep"));
    assert_eq!(theirs[7], None, "a sleeping connection has no statement");
    client.quit();
    watcher.quit();
}

/// The shim's standing rule applies to these two as well: a filter it cannot
/// evaluate is refused by name, never dropped. A `SHOW STATUS ... WHERE` that
/// silently returned every counter, or a `SHOW PROCESSLIST WHERE` that silently
/// returned every connection, is the exact failure this shim exists to avoid.
#[test]
fn a_filter_these_cannot_evaluate_is_refused_rather_than_ignored() {
    let server = TestServer::start("observability-refusals");
    let mut client = server.client();

    let error = client
        .query("SHOW STATUS WHERE Variable_name = 'Questions'")
        .expect_err("WHERE is not evaluated here");
    assert_eq!(error.code, 1235, "{error:?}");
    assert!(error.message.contains("LIKE"), "{error:?}");

    let error = client
        .query("SHOW PROCESSLIST WHERE Id = 1")
        .expect_err("PROCESSLIST takes no filter");
    assert_eq!(error.code, 1235, "{error:?}");

    // And the spellings that do work still do.
    client.ok_query("SHOW STATUS LIKE 'Questions'");
    client.ok_query("SHOW PROCESSLIST");
    client.ok_query("SHOW FULL PROCESSLIST");
    client.quit();
}

/// A connection that has authenticated and sent nothing is `Sleep`, not
/// `Connect`. `Connect` is the state of a connection still handshaking, and an
/// idle pooled connection showing it is a row an operator would go and
/// investigate for nothing.
#[test]
fn an_authenticated_idle_connection_reads_as_sleep() {
    let server = TestServer::start("processlist-idle");
    let mut idle = server.client();
    let idle_id = value(&mut idle, "CONNECTION_ID()");
    // ...and then says nothing more.

    let mut watcher = server.client();
    let rows = processlist(&mut watcher, false);
    let theirs = rows
        .rows
        .iter()
        .find(|row| row[0].as_deref() == Some(idle_id.as_str()))
        .unwrap_or_else(|| panic!("the idle connection is not listed: {rows:?}"));
    assert_eq!(theirs[4].as_deref(), Some("Sleep"));
    idle.quit();
    watcher.quit();
}

/// `information_schema.processlist` is refused, and the refusal names the
/// spelling that works. The shim's standing rule is that a metadata answer it
/// cannot give is an error naming what it could not do, never an empty result
/// a caller reads as "there is nothing there".
#[test]
fn information_schema_processlist_names_the_spelling_that_works() {
    let server = TestServer::start("processlist-infoschema");
    let mut client = server.client();
    let error = client
        .query("SELECT * FROM information_schema.processlist")
        .expect_err("not implemented");
    assert_eq!(error.code, 1235, "{error:?}");
    assert!(
        error.message.contains("SHOW [FULL] PROCESSLIST"),
        "the refusal has to say what to use instead: {error:?}"
    );
    client.quit();
}

/// `SHOW STATUS` used to be a two-column result set with no rows in it,
/// because no counters were kept. These are the counters, and the test is that
/// they *move* — a counter that is reported and never updated is the failure
/// this server has already shipped twice.
#[test]
fn show_status_counts_the_statements_a_session_ran() {
    let server = TestServer::start("status-statements");
    let mut client = server.client();

    let before = status(&mut client, "SESSION", "Questions");
    client.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)");
    client.ok_query("INSERT INTO t VALUES (1, 'a')");
    client.ok_query("INSERT INTO t VALUES (2, 'b')");
    client.ok_query("UPDATE t SET body = 'c' WHERE id = 1");
    client.ok_query("DELETE FROM t WHERE id = 2");
    client.ok_query("SELECT * FROM t");
    assert!(
        status(&mut client, "SESSION", "Questions") >= before + 6,
        "Questions did not move"
    );
    assert_eq!(status(&mut client, "SESSION", "Com_insert"), 2);
    assert_eq!(status(&mut client, "SESSION", "Com_update"), 1);
    assert_eq!(status(&mut client, "SESSION", "Com_delete"), 1);
    assert_eq!(status(&mut client, "SESSION", "Com_create_table"), 1);

    // A statement the shim answers from the catalog is still a SELECT to
    // whoever asked for it, and a `SHOW` is counted apart from one.
    let selects = status(&mut client, "SESSION", "Com_select");
    client.ok_query("SELECT 1");
    assert_eq!(status(&mut client, "SESSION", "Com_select"), selects + 1);

    // A prepared statement is counted where it executes, and its prepare is
    // counted as a prepare rather than as a question — nothing ran. (Every
    // `SHOW STATUS` below is itself a question, and counts itself: the reading
    // is taken before the statement being counted and compared after, so the
    // only question between the two is the reading.)
    let questions = status(&mut client, "SESSION", "Questions");
    let stmt = client
        .prepare("SELECT body FROM t WHERE id = ?")
        .expect("prepare");
    assert_eq!(
        status(&mut client, "SESSION", "Questions"),
        questions + 1,
        "the prepare ran nothing, so only this SHOW STATUS is a new question"
    );
    assert_eq!(status(&mut client, "SESSION", "Com_stmt_prepare"), 1);
    client.execute(&stmt, &[Param::Int(1)]).expect("execute");
    assert_eq!(status(&mut client, "SESSION", "Com_stmt_execute"), 1);
    assert_eq!(status(&mut client, "SESSION", "Com_select"), selects + 2);
    client.close_statement(&stmt);
    assert_eq!(status(&mut client, "SESSION", "Com_stmt_close"), 1);

    // A `PING` is a command, not a statement.
    let questions = status(&mut client, "SESSION", "Questions");
    client.ping().expect("ping");
    assert_eq!(status(&mut client, "SESSION", "Questions"), questions + 1);
    assert_eq!(status(&mut client, "SESSION", "Com_ping"), 1);
    client.quit();
}

/// Session and global are two different numbers, and confusing them is the
/// easiest way to make a status counter useless. A second connection's work
/// must show in the server's total and not in this one's.
#[test]
fn session_status_is_this_connection_and_global_status_is_the_server() {
    let server = TestServer::start("status-scope");
    let mut first = server.client();
    first.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY)");

    let mine = status(&mut first, "SESSION", "Com_insert");
    let everyones = status(&mut first, "GLOBAL", "Com_insert");

    let mut second = server.client();
    for id in 1..=3 {
        second.ok_query(&format!("INSERT INTO t VALUES ({id})"));
    }

    assert_eq!(
        status(&mut first, "SESSION", "Com_insert"),
        mine,
        "another connection's inserts landed on this session's counter"
    );
    assert_eq!(
        status(&mut first, "GLOBAL", "Com_insert"),
        everyones + 3,
        "another connection's inserts are missing from the server's counter"
    );

    // A bare `SHOW STATUS` means SESSION, as it does in MySQL.
    let bare = first.ok_query("SHOW STATUS LIKE 'Com_insert'").rows();
    assert_eq!(bare.cell(0, 1), mine.to_string());

    // The counts of connections are global however they are asked for, and
    // they come off the same registry the process list reads.
    assert_eq!(status(&mut first, "SESSION", "Threads_connected"), 2);
    assert_eq!(status(&mut first, "GLOBAL", "Threads_connected"), 2);
    assert!(status(&mut first, "GLOBAL", "Connections") >= 2);
    first.quit();
    second.quit();
}

/// Errors are bucketed by what an operator would do about them, because a
/// single total cannot tell "a credential is wrong" from "the workload is
/// contending" from "something upstream is generating SQL we do not take".
#[test]
fn show_status_counts_errors_by_class() {
    let server = TestServer::start("status-errors");
    let mut client = server.client();
    client.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY)");
    client.ok_query("INSERT INTO t VALUES (1)");

    let total = status(&mut client, "SESSION", "Inlaysql_errors_total");
    client
        .query("SELECT * FROM nope")
        .expect_err("no such table");
    client
        .query("SELECT FROM WHERE")
        .expect_err("not valid SQL");
    client
        .query("INSERT INTO t VALUES (1)")
        .expect_err("duplicate key");
    client.query("SAVEPOINT s").expect_err("unsupported");

    assert_eq!(
        status(&mut client, "SESSION", "Inlaysql_errors_total"),
        total + 4
    );
    assert_eq!(
        status(&mut client, "SESSION", "Inlaysql_errors_no_such_object"),
        1
    );
    assert_eq!(status(&mut client, "SESSION", "Inlaysql_errors_syntax"), 1);
    assert_eq!(
        status(&mut client, "SESSION", "Inlaysql_errors_constraint"),
        1
    );
    assert_eq!(
        status(&mut client, "SESSION", "Inlaysql_errors_unsupported"),
        1
    );

    // A refused login is not a statement error: it is `Aborted_connects`, and
    // it is global, because the connection it belonged to never existed.
    let aborted = status(&mut client, "GLOBAL", "Aborted_connects");
    Client::connect(server.addr, "root", "wrong", None).expect_err("bad password");
    // The refusing thread records it as it unwinds, which is not synchronous
    // with this connection; poll rather than sleep a fixed interval and hope.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while status(&mut client, "GLOBAL", "Aborted_connects") == aborted {
        assert!(
            std::time::Instant::now() < deadline,
            "a refused login was never counted"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    client.quit();
}

/// Bytes in and out are the numbers that say whether a client is asking for
/// more than it can drink, and they have to count the whole of a result set
/// rather than the statement that asked for it.
#[test]
fn show_status_counts_the_bytes_that_crossed_the_socket() {
    let server = TestServer::start("status-bytes");
    let mut client = server.client();
    client.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)");
    for id in 1..=200 {
        client.ok_query(&format!(
            "INSERT INTO t VALUES ({id}, '{}')",
            "x".repeat(200)
        ));
    }

    let sent = status(&mut client, "SESSION", "Bytes_sent");
    let received = status(&mut client, "SESSION", "Bytes_received");
    let rows = client.ok_query("SELECT * FROM t").rows();
    assert_eq!(rows.rows.len(), 200);

    let now_sent = status(&mut client, "SESSION", "Bytes_sent");
    assert!(
        now_sent - sent > 200 * 200,
        "a 40 KB result set moved Bytes_sent by only {}",
        now_sent - sent
    );
    assert!(
        status(&mut client, "SESSION", "Bytes_received") > received,
        "Bytes_received did not move"
    );
    client.quit();
}

/// The slow-query log is off by default and reports itself as off; turned on,
/// it counts what it logged, and `long_query_time` reports the threshold that
/// is actually compared against.
#[test]
fn the_slow_query_log_is_off_by_default_and_counts_what_it_logs() {
    let server = TestServer::start("slow-log-off");
    let mut client = server.client();
    assert_eq!(value(&mut client, "@@slow_query_log"), "OFF");
    assert_eq!(value(&mut client, "@@long_query_time"), "0.000000");
    client.ok_query("CREATE TABLE t (id INTEGER PRIMARY KEY)");
    assert_eq!(status(&mut client, "GLOBAL", "Slow_queries"), 0);
    client.quit();

    // On, at a threshold every real statement here will cross.
    let server = TestServer::start_tuned("slow-log-on", "s3cret", |options| {
        options.slow_query_log_ms = 1;
    });
    let mut client = server.client();
    assert_eq!(value(&mut client, "@@slow_query_log"), "ON");
    assert_eq!(value(&mut client, "@@long_query_time"), "0.001000");
    seed_pairs(&mut client, 400);
    client.ok_query("SELECT COUNT(*) FROM t a JOIN t b ON a.n < b.n");
    assert!(
        status(&mut client, "SESSION", "Slow_queries") > 0,
        "a statement over a quarter of a million pairs was not slow at 1ms"
    );
    client.quit();
}

/// Everything `SHOW STATUS` reports has to be reachable by name, and a name
/// this server invented has to say so — an operator must not mistake this
/// server's error buckets for a MySQL variable their dashboard understands.
#[test]
fn every_reported_status_name_is_mysqls_or_marked_as_this_servers() {
    let server = TestServer::start("status-names");
    let mut client = server.client();
    let rows = client.ok_query("SHOW GLOBAL STATUS").rows();
    assert!(rows.rows.len() > 20, "only {} counters", rows.rows.len());

    for name in rows.column("Variable_name") {
        let mysqls_own = name.starts_with("Com_")
            || matches!(
                name.as_str(),
                "Questions"
                    | "Bytes_received"
                    | "Bytes_sent"
                    | "Slow_queries"
                    | "Connections"
                    | "Aborted_connects"
                    | "Connection_errors_max_connections"
                    | "Max_used_connections"
                    | "Threads_connected"
                    | "Threads_running"
                    | "Uptime"
            );
        assert!(
            mysqls_own || name.starts_with("Inlaysql_"),
            "`{name}` is neither a MySQL status variable nor marked as this server's own"
        );
    }
    // A LIKE pattern filters it, as it does every other SHOW here.
    let errors = client
        .ok_query("SHOW GLOBAL STATUS LIKE 'Inlaysql_errors_%'")
        .rows();
    assert!(errors.rows.len() >= 10, "{errors:?}");
    client.quit();
}

// =====================================================================
// OPTIMIZE TABLE
// =====================================================================

/// A server holding a `docs` table with a full-text index and rows that have
/// not been indexed yet — the shape a bulk load leaves behind, where the whole
/// build is still waiting for whichever query arrives first.
fn optimize_fixture(name: &str) -> (TestServer, Client) {
    let server = TestServer::start(name);
    let mut client = server.client();
    client.ok_query("CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT)");
    client.ok_query("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)");
    client.ok_query("CREATE INDEX docs_body ON docs (body)");
    client.ok_query("CREATE INDEX notes_body ON notes (body)");
    for id in 1..=5 {
        client.ok_query(&format!(
            "INSERT INTO docs (id, body) VALUES ({id}, 'alpha document {id}')"
        ));
        client.ok_query(&format!(
            "INSERT INTO notes (id, body) VALUES ({id}, 'beta note {id}')"
        ));
    }
    (server, client)
}

/// `OPTIMIZE TABLE` answers with MySQL's four-column result set, and the
/// `Msg_text` distinguishes a build that happened from one that had nothing to
/// do.
///
/// The second half is the assertion worth having. MySQL's own wording for a
/// table that needed nothing is `Table is already up to date`, and answering
/// `OK` there — or answering an OK *packet* instead of a result set — would be
/// a maintenance statement telling an operator it did work it did not do.
#[test]
fn optimize_table_reports_what_it_built_and_what_it_did_not() {
    let (_server, mut client) = optimize_fixture("optimize-report");

    let built = client.ok_query("OPTIMIZE TABLE docs").rows();
    assert_eq!(built.columns, ["Table", "Op", "Msg_type", "Msg_text"]);
    assert_eq!(built.column("Table"), ["inlaysql.docs"]);
    assert_eq!(built.column("Op"), ["optimize"]);
    assert_eq!(built.column("Msg_type"), ["status"]);
    assert_eq!(
        built.column("Msg_text"),
        ["OK; rebuilt docs_body"],
        "the report does not name the index it built"
    );

    // Nothing has changed since, so there is nothing to build.
    let again = client.ok_query("OPTIMIZE TABLE docs").rows();
    assert_eq!(again.column("Msg_text"), ["Table is already up to date"]);

    // And the index it built really answers — the build was the work, not a
    // flag flip.
    assert_eq!(
        client.count_rows("SELECT id, bm25_score(body, 'alpha') AS s FROM docs ORDER BY s DESC"),
        5
    );

    // A write puts the table back in the pending state, and the next
    // `OPTIMIZE` says so rather than repeating itself.
    client.ok_query("INSERT INTO docs (id, body) VALUES (6, 'alpha again')");
    let after_write = client.ok_query("OPTIMIZE TABLE docs").rows();
    assert_eq!(after_write.column("Msg_text"), ["OK; rebuilt docs_body"]);
    client.quit();
}

/// Every spelling a client actually sends, and one row per table in the order
/// the client named them.
#[test]
fn optimize_table_takes_mysqls_whole_form() {
    let (_server, mut client) = optimize_fixture("optimize-forms");

    let rows = client
        .ok_query("OPTIMIZE NO_WRITE_TO_BINLOG TABLE `notes`, docs")
        .rows();
    assert_eq!(
        rows.column("Table"),
        ["inlaysql.notes", "inlaysql.docs"],
        "the rows are not in the order the tables were named"
    );
    assert_eq!(rows.rows.len(), 2);

    // `LOCAL` is MySQL's synonym for the same modifier, and a qualified name
    // is how a client that has selected no schema writes one.
    client.ok_query("INSERT INTO notes (id, body) VALUES (6, 'beta again')");
    let rows = client
        .ok_query("OPTIMIZE LOCAL TABLE inlaysql.notes")
        .rows();
    assert_eq!(rows.column("Msg_text"), ["OK; rebuilt notes_body"]);

    // A prepared `OPTIMIZE` runs the same way — the shim owns it at
    // `COM_STMT_PREPARE` too, so it cannot mean one thing sent and another
    // prepared.
    client.ok_query("INSERT INTO notes (id, body) VALUES (7, 'beta once more')");
    let prepared = client.prepare("OPTIMIZE TABLE notes").expect("prepare");
    let rows = client.execute(&prepared, &[]).expect("execute").rows();
    assert_eq!(rows.column("Msg_text"), ["OK; rebuilt notes_body"]);
    client.quit();
}

/// A table this server does not have is a refusal, not a row claiming success
/// — and a form of the statement it cannot honour is refused by name rather
/// than accepted and partly ignored.
#[test]
fn optimize_table_refuses_what_it_cannot_do() {
    let (_server, mut client) = optimize_fixture("optimize-refusals");

    let error = client
        .query("OPTIMIZE TABLE nosuchtable")
        .expect_err("no such table");
    assert_eq!(error.code, 1146, "{}", error.message);

    // One bad name in a list refuses the whole statement: there is no way to
    // build half of it and report the rest.
    let error = client
        .query("OPTIMIZE TABLE docs, nosuchtable")
        .expect_err("no such table");
    assert_eq!(error.code, 1146, "{}", error.message);

    for sql in [
        // MySQL's other `OPTIMIZE` targets, which this server does not have.
        "OPTIMIZE PARTITION p0",
        "OPTIMIZE TABLE",
    ] {
        let error = client.query(sql).expect_err(sql);
        assert!(
            error.message.contains("OPTIMIZE") || error.message.contains("TABLE"),
            "`{sql}` was refused without saying what it could not do: {}",
            error.message
        );
    }
    client.quit();
}

/// The privilege is MySQL's for this statement — SELECT and INSERT on every
/// table named — and it is checked per table, from the same parse that will
/// run it.
#[test]
fn optimize_table_needs_mysqls_privileges_on_every_table_it_names() {
    let (server, mut root) = optimize_fixture("optimize-acl");
    root.ok_query("CREATE USER 'reader' IDENTIFIED BY 'r-pass'");
    root.ok_query("GRANT SELECT ON docs TO 'reader'");

    let mut reader = server.client_as("reader", "r-pass");
    let error = reader
        .query("OPTIMIZE TABLE docs")
        .expect_err("SELECT alone is not enough");
    assert_eq!(error.code, 1142, "{}", error.message);

    root.ok_query("GRANT INSERT ON docs TO 'reader'");
    let mut reader = server.client_as("reader", "r-pass");
    reader.ok_query("OPTIMIZE TABLE docs");

    // The grant is per table, so the table it was not granted on is still
    // refused — including when it is named alongside one that was.
    let error = reader
        .query("OPTIMIZE TABLE docs, notes")
        .expect_err("no grant on notes");
    assert_eq!(error.code, 1142, "{}", error.message);
    assert!(error.message.contains("notes"), "{}", error.message);
    reader.quit();
    root.quit();
}
