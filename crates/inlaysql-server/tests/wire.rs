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
        let temp = TempDb::new(name);
        let options = ServerOptions {
            bind: "127.0.0.1".to_string(),
            // Port 0: the OS picks a free one, so nothing here assumes 3306 is
            // available or that this is the only server running.
            port: 0,
            user: "root".to_string(),
            password: password.to_string(),
            max_connections,
        };
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
    Null,
}

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
            return Err(parse_error(&greeting));
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
            return Err(parse_error(&greeting));
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
            return Err(parse_error(&greeting));
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
            return Err(parse_error(&greeting));
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
            return Err(parse_error(&greeting));
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
            return Err(parse_error(&greeting));
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
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header)?;
            let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
            self.sequence = header[3].wrapping_add(1);
            let start = payload.len();
            payload.resize(start + length, 0);
            self.stream.read_exact(&mut payload[start..])?;
            if length < 0xff_ff_ff {
                return Ok(payload);
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

    fn prepare(&mut self, sql: &str) -> Result<Prepared, ServerError> {
        self.command(0x16, sql.as_bytes());
        let packet = self.read_packet().expect("prepare reply");
        if packet.first() == Some(&0xff) {
            return Err(parse_error(&packet));
        }
        let id = u32::from_le_bytes([packet[1], packet[2], packet[3], packet[4]]);
        let columns = u16::from_le_bytes([packet[5], packet[6]]);
        let params = u16::from_le_bytes([packet[7], packet[8]]);

        if params > 0 {
            for _ in 0..params {
                self.read_packet().expect("param def");
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
                    Param::Null => body.extend_from_slice(&[0x06, 0]),
                }
            }
            for param in params {
                match param {
                    Param::Int(value) => body.extend_from_slice(&value.to_le_bytes()),
                    Param::Str(value) => put_lenenc_bytes(&mut body, value.as_bytes()),
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
}

#[derive(Debug)]
struct Prepared {
    id: u32,
    param_count: usize,
    column_count: usize,
    /// `(name, wire type)` for each column `COM_STMT_PREPARE_OK` reported —
    /// empty when `column_count` is `0`. See AHL-466.
    columns: Vec<(String, u8)>,
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
    // dialect is deliberately growing — this line already had to move once,
    // from `SELECT DISTINCT` when AHL-411 implemented it, again from `UNION`
    // when AHL-473 implemented set operations and CTEs, and again from
    // `ROW_NUMBER() OVER ()` when AHL-494 implemented window functions
    // (ranking, `lag`/`lead`, the aggregate family, `ROWS` frames, named
    // windows and `FILTER`). If `percent_rank`/`cume_dist` land too and this
    // starts failing, that is the same good news: point it at whatever is
    // still refused rather than deleting the assertion. What is being tested
    // is the mapping of `Error::Unsupported` onto 1235, not this particular
    // statement.
    let error = client
        .query("SELECT percent_rank() OVER (ORDER BY id) FROM kv")
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
        .err()
        .expect("a wrong password must be refused");
    assert_eq!(error.code, 1045, "ER_ACCESS_DENIED_ERROR");
    assert_eq!(error.sqlstate, "28000");

    // A wrong user is refused the same way. The message names the user, as
    // MySQL's does, but nothing in either reply says *which* half was wrong —
    // so a guesser cannot use the difference to enumerate valid users.
    let other = Client::connect(server.addr, "nobody", &server.password, None)
        .err()
        .expect("a wrong user must be refused");
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
        .err()
        .expect("a forged token must be refused");
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
        .err()
        .expect("the RSA request must be refused");
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
        .err()
        .expect("a wrong password must be refused over the fast path");
    assert_eq!(fast.code, 1045, "ER_ACCESS_DENIED_ERROR");
    assert_eq!(fast.sqlstate, "28000");

    let full = Client::connect_caching_sha2_full_auth(server.addr, "root", "wrong")
        .err()
        .expect("a wrong password must be refused over full authentication");
    assert_eq!(full.code, 1045);
    assert_eq!(full.sqlstate, "28000");

    let switched = Client::connect_via_auth_switch(server.addr, "root", "wrong")
        .err()
        .expect("a wrong password must be refused after switching plugins");
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
        .err()
        .expect("the second connection must be refused");
    assert_eq!(error.code, 1040, "ER_CON_COUNT_ERROR");
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
