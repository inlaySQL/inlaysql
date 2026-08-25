//! The MySQL protocol's constants, and the packets built from them.
//!
//! Only what a client actually needs is here. Capabilities this server does not
//! implement are deliberately *not* advertised, because the negotiated set is
//! the intersection of both ends: a capability the server never offers is one
//! the client cannot ask for, which is how a small implementation stays honest
//! rather than promising a feature and mishandling it later.

use inlaysql::{DataType, Value};

use crate::packet::{put_lenenc_bytes, put_lenenc_int, put_lenenc_str, put_nul_str};

// ------------------------------------------------------------ capabilities

/// The client may use the 4.1 protocol. Everything here assumes it.
pub const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
/// `affected_rows` counts matched rows rather than changed ones.
pub const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
/// Column definitions carry 32-bit flags.
pub const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
/// The handshake may name a default schema.
pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
/// The 4.1 protocol: result-set metadata, SQLSTATE in errors, 20-byte auth.
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
/// The connection is interactive; only affects idle timeouts.
pub const CLIENT_INTERACTIVE: u32 = 0x0000_0400;
/// The server reports transaction status in OK and EOF packets.
pub const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
/// 4.1 authentication (the 20-byte challenge). Required by native password.
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
/// The handshake names an authentication plugin.
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
/// The handshake response may carry connection attributes.
pub const CLIENT_CONNECT_ATTRS: u32 = 0x0010_0000;
/// The auth response in the handshake is length-encoded.
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
/// The client asked for TLS. Never advertised — v1 is plaintext — so a client
/// cannot negotiate it, but the bit is named here because the handshake reader
/// checks that it was not set anyway.
pub const CLIENT_SSL: u32 = 0x0000_0800;

/// Everything this server offers.
///
/// Notably absent, each on purpose:
///
/// * `CLIENT_SSL` — v1 is plaintext (see `docs/server.md`). Not advertising it
///   means a client cannot start a TLS handshake this server would fail.
/// * `CLIENT_DEPRECATE_EOF` — result sets are terminated with EOF packets, the
///   form every client still understands, rather than supporting two framings.
/// * `CLIENT_MULTI_STATEMENTS` / `CLIENT_MULTI_RESULTS` — the engine runs
///   exactly one statement per call, so accepting `a; b` would mean silently
///   running only the first. Refusing the capability makes the client not send
///   it, which is a better failure than a half-executed batch.
/// * `CLIENT_LOCAL_FILES` — lets a server ask a client to upload a local file.
///   A database server has no business doing that.
pub const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_INTERACTIVE
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;

// ------------------------------------------------------------ status flags

/// A transaction is open.
pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
/// Autocommit is on.
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;

// ------------------------------------------------------------ commands

/// The command byte a client sends at the head of every message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Close the connection.
    Quit,
    /// Change the default schema.
    InitDb,
    /// Run a statement written as text.
    Query,
    /// List a table's fields — a pre-4.1 command this server refuses.
    FieldList,
    /// Are you alive?
    Ping,
    /// Parse a statement and keep it.
    StmtPrepare,
    /// Run a kept statement with bound parameters.
    StmtExecute,
    /// Forget a kept statement.
    StmtClose,
    /// Discard a kept statement's accumulated state.
    StmtReset,
    /// Ask the server to end another connection — the command form of `KILL`.
    ///
    /// Deprecated in MySQL in favour of the statement, but `mysqladmin kill`
    /// and older drivers still send it, and it is the same operation: the
    /// four-byte body is the connection id.
    ProcessKill,
    /// Anything else.
    Unknown(u8),
}

impl Command {
    /// Classify the first byte of a command message.
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            0x01 => Command::Quit,
            0x02 => Command::InitDb,
            0x03 => Command::Query,
            0x04 => Command::FieldList,
            0x0c => Command::ProcessKill,
            0x0e => Command::Ping,
            0x16 => Command::StmtPrepare,
            0x17 => Command::StmtExecute,
            0x19 => Command::StmtClose,
            0x1a => Command::StmtReset,
            other => Command::Unknown(other),
        }
    }
}

// ------------------------------------------------------------ column types

/// `MYSQL_TYPE_LONGLONG`.
pub const TYPE_LONGLONG: u8 = 8;
/// `MYSQL_TYPE_DOUBLE`.
pub const TYPE_DOUBLE: u8 = 5;
/// `MYSQL_TYPE_VAR_STRING`.
pub const TYPE_VAR_STRING: u8 = 253;
/// `MYSQL_TYPE_BLOB`.
pub const TYPE_BLOB: u8 = 252;

/// The `utf8mb4_general_ci` collation id, used for every text column.
pub const CHARSET_UTF8MB4: u16 = 45;
/// The `binary` collation id, used for numbers and blobs.
pub const CHARSET_BINARY: u16 = 63;

/// Column flag: the value is binary rather than text.
pub const FLAG_BINARY: u16 = 0x0080;
/// Column flag: the value is numeric.
pub const FLAG_NUM: u16 = 0x8000;

/// One result-set column, as the wire describes it.
#[derive(Debug, Clone)]
pub struct ColumnDef {
    /// The name the client sees.
    pub name: String,
    /// The table it came from, empty for a computed column.
    pub table: String,
    /// One of the `TYPE_*` constants.
    pub ty: u8,
    /// The collation id.
    pub charset: u16,
    /// The `FLAG_*` set.
    pub flags: u16,
    /// The widest value the column can hold, in bytes. Advisory only.
    pub length: u32,
}

impl ColumnDef {
    /// A text column, the safe default for a value whose type is not fixed.
    pub fn text(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            table: String::new(),
            ty: TYPE_VAR_STRING,
            charset: CHARSET_UTF8MB4,
            flags: 0,
            length: 65535,
        }
    }

    /// An integer column.
    pub fn integer(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            table: String::new(),
            ty: TYPE_LONGLONG,
            charset: CHARSET_BINARY,
            flags: FLAG_NUM | FLAG_BINARY,
            length: 20,
        }
    }

    /// Encode a `ColumnDefinition41` packet.
    pub fn encode(&self, schema: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.name.len());
        put_lenenc_str(&mut out, "def");
        put_lenenc_str(&mut out, schema);
        put_lenenc_str(&mut out, &self.table);
        put_lenenc_str(&mut out, &self.table);
        put_lenenc_str(&mut out, &self.name);
        put_lenenc_str(&mut out, &self.name);
        // Length of the fixed-size block that follows.
        put_lenenc_int(&mut out, 0x0c);
        out.extend_from_slice(&self.charset.to_le_bytes());
        out.extend_from_slice(&self.length.to_le_bytes());
        out.push(self.ty);
        out.extend_from_slice(&self.flags.to_le_bytes());
        // Decimals, then two filler bytes.
        out.push(0);
        out.extend_from_slice(&[0, 0]);
        out
    }
}

/// Choose one wire type for a column of a materialised result set.
///
/// The engine is dynamically typed the way SQLite is, so a column can hold an
/// integer in one row and text in the next. The binary protocol has no room for
/// that — a value is decoded as whatever the column *said* it was — so the type
/// is unified across every row before any of them are sent: all integers make
/// an integer column, integers and reals together make a real one, and anything
/// else falls back to text, which can represent all of it. Scanning first costs
/// nothing here because the whole result set is already in memory.
///
/// The values decide it — except when there are no values to decide from, which
/// is every row `NULL` and, far more commonly, a result set with no rows at
/// all. Falling back to text there (what this did before) meant `SELECT id FROM
/// t` reported `VAR_STRING` when `t` happened to be empty and `LONGLONG` when
/// it was not, and that `COM_STMT_PREPARE` answered `LONGLONG` for that column
/// while the `COM_STMT_EXECUTE` that followed contradicted it. `declared` is
/// the plan's own answer (AHL-466's `Statement::columns`), which does not
/// depend on how many rows came back — and is what a *streamed* result set has
/// to use for every row, since it has none of them to look at. Preferring it
/// here is what makes the two paths agree byte for byte.
///
/// `None` for a result set with no plan behind it — the shim's `SHOW` and
/// `information_schema` answers — where the values really are the only
/// evidence there is, and the text fallback is all that is left.
pub fn column_def_for(
    name: String,
    declared: Option<DataType>,
    rows: &[Vec<Value>],
    index: usize,
) -> ColumnDef {
    match infer_column_type(rows, index) {
        Some(mut def) => {
            def.name = name;
            def
        }
        None => column_def_from_type(name, declared),
    }
}

/// The unified type of `index` across `rows`, or `None` when every value there
/// is `NULL` or absent and the values therefore say nothing at all.
fn infer_column_type(rows: &[Vec<Value>], index: usize) -> Option<ColumnDef> {
    let mut saw_integer = false;
    let mut saw_real = false;
    let mut saw_other = false;
    let mut saw_blob = false;

    for row in rows {
        match row.get(index) {
            None | Some(Value::Null) => {}
            Some(Value::Integer(_)) => saw_integer = true,
            Some(Value::Real(_)) => saw_real = true,
            Some(Value::Blob(_)) => {
                saw_blob = true;
                saw_other = true;
            }
            Some(_) => saw_other = true,
        }
    }

    if saw_other {
        if saw_blob && !saw_integer && !saw_real {
            return Some(ColumnDef {
                name: String::new(),
                table: String::new(),
                ty: TYPE_BLOB,
                charset: CHARSET_BINARY,
                flags: FLAG_BINARY,
                length: u32::MAX,
            });
        }
        return Some(ColumnDef::text(""));
    }
    if saw_real {
        return Some(ColumnDef {
            name: String::new(),
            table: String::new(),
            ty: TYPE_DOUBLE,
            charset: CHARSET_BINARY,
            flags: FLAG_NUM | FLAG_BINARY,
            length: 22,
        });
    }
    if saw_integer {
        return Some(ColumnDef::integer(""));
    }
    None
}

/// The column definition for a column that will be *streamed*, or `None` if
/// this column cannot be described before its values exist.
///
/// A streamed result set has to name every column's type in the packets that
/// precede its first row, so there is no scan to unify over: the plan's
/// declared type is the only evidence available. That is trustworthy exactly
/// where the engine enforces the declaration — it refuses `INSERT`ing text into
/// an `INTEGER` column outright ("column `n` is INTEGER but the value is TEXT")
/// and coerces an integer written to a `REAL` one — so for those types the
/// answer here is provably the same one [`column_def_for`] would reach after
/// seeing every row.
///
/// `None` for the two cases where it is provably *not*:
///
/// * **`NUMERIC`** — SQLite's affinity column, which really does hold an
///   integer in one row and text in the next. Nothing but the values can say
///   what it is.
/// * **A column the plan gives no type at all** (`ty: None`) — a computed
///   expression, an aggregate, a retrieval score, a `UNION` arm, a derived
///   table's synthesised column, a `SELECT` with no `FROM`.
///
/// A `None` here is not a failure: it means this statement is answered by
/// materialising it, the way every statement was before. It costs memory
/// proportional to the answer, so it is worth knowing that it is the *shape of
/// the projection* that decides, not the size of the table.
pub fn streamed_column_def(name: String, declared: Option<DataType>) -> Option<ColumnDef> {
    match declared {
        Some(
            ty @ (DataType::Integer
            | DataType::Real
            | DataType::Text
            | DataType::Blob
            | DataType::Vector(_)
            | DataType::QuantizedVector(_)),
        ) => Some(column_def_from_type(name, Some(ty))),
        Some(DataType::Numeric) | None => None,
    }
}

/// Choose a wire type for a column whose declared type the plan already
/// knows — `COM_STMT_PREPARE`'s answer, built before a single row exists to
/// [`unify_column_type`] over. `None` (a computed expression, a retrieval
/// score, or a column of a `SELECT` with no `FROM`) gets the same text
/// fallback an all-`NULL` column does there: the widest type, and a client
/// already copes with the real metadata replacing it once the statement
/// actually runs (`docs/server.md`'s "One known gap" section, closed by
/// AHL-466 for the case this function *can* answer).
pub fn column_def_from_type(name: String, ty: Option<DataType>) -> ColumnDef {
    match ty {
        Some(DataType::Integer) => ColumnDef::integer(name),
        Some(DataType::Real) => ColumnDef {
            name,
            table: String::new(),
            ty: TYPE_DOUBLE,
            charset: CHARSET_BINARY,
            flags: FLAG_NUM | FLAG_BINARY,
            length: 22,
        },
        Some(DataType::Blob) => ColumnDef {
            name,
            table: String::new(),
            ty: TYPE_BLOB,
            charset: CHARSET_BINARY,
            flags: FLAG_BINARY,
            length: u32::MAX,
        },
        // `NUMERIC` holds whatever the value turns out to be (D7's affinity
        // rules), a vector renders as the JSON text `format_vector` builds,
        // and `None` covers everything the plan does not statically know
        // the type of. Text represents every one of them.
        Some(DataType::Text)
        | Some(DataType::Numeric)
        | Some(DataType::Vector(_))
        | Some(DataType::QuantizedVector(_))
        | None => ColumnDef::text(name),
    }
}

// ------------------------------------------------------------ values

/// A value's text-protocol rendering.
pub fn text_value(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Null => None,
        Value::Integer(i) => Some(i.to_string().into_bytes()),
        Value::Real(r) => Some(format_real(*r).into_bytes()),
        Value::Text(s) => Some(s.as_bytes().to_vec()),
        Value::Blob(b) => Some(b.clone()),
        Value::Vector(v) => Some(format_vector(v).into_bytes()),
    }
}

/// A real, rendered the way MySQL renders a `DOUBLE`: no trailing `.0`, and a
/// finite spelling for values IEEE has but SQL does not.
fn format_real(value: f64) -> String {
    if value.is_nan() {
        return "NULL".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            f64::MAX.to_string()
        } else {
            f64::MIN.to_string()
        };
    }
    let mut rendered = format!("{value}");
    if let Some(stripped) = rendered.strip_suffix(".0") {
        rendered = stripped.to_string();
    }
    rendered
}

/// An embedding, rendered as the JSON array `vector()` accepts back.
fn format_vector(values: &[f32]) -> String {
    let mut out = String::with_capacity(values.len() * 4 + 2);
    out.push('[');
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&value.to_string());
    }
    out.push(']');
    out
}

/// Append a value to a binary-protocol row, encoded as `ty` says it is.
///
/// The type has already been unified across the result set by
/// [`unify_column_type`], so the fallback arms here are unreachable for a value
/// that came from that path; they exist so a mismatch degrades to a correct
/// string rather than a misframed packet.
pub fn put_binary_value(out: &mut Vec<u8>, ty: u8, value: &Value) {
    match (ty, value) {
        (_, Value::Null) => {}
        (TYPE_LONGLONG, Value::Integer(i)) => out.extend_from_slice(&i.to_le_bytes()),
        (TYPE_DOUBLE, Value::Integer(i)) => out.extend_from_slice(&(*i as f64).to_le_bytes()),
        (TYPE_DOUBLE, Value::Real(r)) => out.extend_from_slice(&r.to_le_bytes()),
        _ => put_lenenc_bytes(out, &text_value(value).unwrap_or_default()),
    }
}

// ------------------------------------------------------------ packets

/// An OK packet.
///
/// `warnings` is not decoration. It is how a client is told that the statement
/// succeeded but not exactly as written — the shim removes MySQL-only DDL
/// clauses, and reporting a count here is what sends a reader to
/// `SHOW WARNINGS` instead of assuming nothing happened.
pub fn ok_packet(
    affected_rows: u64,
    last_insert_id: u64,
    status: u16,
    warnings: u16,
    info: &str,
) -> Vec<u8> {
    let mut out = vec![0x00];
    put_lenenc_int(&mut out, affected_rows);
    put_lenenc_int(&mut out, last_insert_id);
    out.extend_from_slice(&status.to_le_bytes());
    out.extend_from_slice(&warnings.to_le_bytes());
    out.extend_from_slice(info.as_bytes());
    out
}

/// An EOF packet, which ends a result set's metadata and its rows.
pub fn eof_packet(status: u16, warnings: u16) -> Vec<u8> {
    let mut out = vec![0xfe];
    out.extend_from_slice(&warnings.to_le_bytes());
    out.extend_from_slice(&status.to_le_bytes());
    out
}

/// An ERR packet.
pub fn err_packet(code: u16, sqlstate: &str, message: &str) -> Vec<u8> {
    let mut out = vec![0xff];
    out.extend_from_slice(&code.to_le_bytes());
    out.push(b'#');
    let mut state = sqlstate.as_bytes().to_vec();
    state.resize(5, b'0');
    out.extend_from_slice(&state);
    // A client displays this; keep it to one line so it cannot forge extra
    // protocol-looking output in somebody's terminal.
    out.extend(
        message
            .bytes()
            .map(|b| if b == b'\n' || b == b'\r' { b' ' } else { b }),
    );
    out
}

/// An ERR packet sent **before** any handshake has been exchanged — the only
/// way this server ever refuses a connection outright, at `--max-connections`
/// or when the database file itself cannot be opened.
///
/// The SQLSTATE marker in [`err_packet`] is a `CLIENT_PROTOCOL_41` feature
/// (see that capability's doc comment above): the client only asks for it in
/// its handshake *response*, and this packet is sent before that response
/// ever arrives, so nothing has negotiated the marker's presence yet. Real
/// MySQL's own pre-handshake refusal (`ER_CON_COUNT_ERROR`) is the old-style
/// packet this function writes; a `#`-marked one here reads as protocol
/// version confusion to a real client rather than as a clean error — checked
/// against mysql-connector-python, which mis-parses a `#`-marked packet at
/// this point in the exchange as `"1040 (HY000): #08004Too many
/// connections"` instead of the clean `"Too many connections"` this format
/// gives it.
pub fn err_packet_before_handshake(code: u16, message: &str) -> Vec<u8> {
    let mut out = vec![0xff];
    out.extend_from_slice(&code.to_le_bytes());
    out.extend(
        message
            .bytes()
            .map(|b| if b == b'\n' || b == b'\r' { b' ' } else { b }),
    );
    out
}

/// The initial handshake the server sends before anything else.
///
/// Advertises `caching_sha2_password` — MySQL 8+'s own default — as the
/// plugin a client should use if it has no preference of its own. A client
/// that already knows it wants `mysql_native_password` (PHP's PDO and the
/// `mysql` CLI both still complete it directly) says so itself and is never
/// asked to switch; see `connection::Connection::authenticate`.
pub fn handshake(connection_id: u32, scramble: &[u8], server_version: &str) -> Vec<u8> {
    let mut out = vec![10];
    put_nul_str(&mut out, server_version);
    out.extend_from_slice(&connection_id.to_le_bytes());
    // The challenge arrives in two pieces for backwards compatibility.
    out.extend_from_slice(&scramble[..8]);
    out.push(0);
    out.extend_from_slice(&SERVER_CAPABILITIES.to_le_bytes()[..2]);
    out.push(CHARSET_UTF8MB4 as u8);
    out.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
    out.extend_from_slice(&SERVER_CAPABILITIES.to_le_bytes()[2..]);
    // Total challenge length, counting the trailing NUL of part two.
    out.push(scramble.len() as u8 + 1);
    out.extend_from_slice(&[0u8; 10]);
    out.extend_from_slice(&scramble[8..]);
    out.push(0);
    put_nul_str(&mut out, crate::auth::CACHING_SHA2_PASSWORD);
    out
}

/// An `AuthSwitchRequest`, asking a client to authenticate with `plugin`
/// instead of whatever it offered.
///
/// Two reasons a client is asked to switch, and this packet is the same for
/// both: it named a plugin this server does not complete at all, or it named
/// one *this account* has no verifier for (`IDENTIFIED WITH ...` pins an
/// account to one plugin — see [`crate::acl`]). MySQL answers both the same
/// way, which is why the plugin is a parameter rather than the constant it
/// used to be.
pub fn auth_switch_request(plugin: &str, scramble: &[u8]) -> Vec<u8> {
    let mut out = vec![0xfe];
    put_nul_str(&mut out, plugin);
    out.extend_from_slice(scramble);
    out.push(0);
    out
}

/// An `AuthMoreData` packet: plugin-specific data during authentication,
/// wrapped in the `0x01` header byte that distinguishes it from an
/// `ERR`/`OK`/`AuthSwitchRequest` packet. `caching_sha2_password` uses this
/// for its single-byte status codes
/// ([`crate::auth::CACHING_SHA2_FAST_AUTH_SUCCESS`] and
/// [`crate::auth::CACHING_SHA2_PERFORM_FULL_AUTHENTICATION`]).
pub fn auth_more_data(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x01];
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Reader;

    #[test]
    fn the_handshake_parses_back_the_way_a_client_reads_it() {
        let scramble: Vec<u8> = (1..=20).collect();
        let packet = handshake(7, &scramble, "8.0.35-inlaysql");
        let mut reader = Reader::new(&packet);

        assert_eq!(reader.u8().unwrap(), 10, "protocol version");
        assert_eq!(reader.nul_str().unwrap(), "8.0.35-inlaysql");
        assert_eq!(reader.u32().unwrap(), 7, "connection id");
        assert_eq!(reader.take(8).unwrap(), &scramble[..8]);
        assert_eq!(reader.u8().unwrap(), 0, "filler");

        let lower = reader.u16().unwrap() as u32;
        assert_eq!(reader.u8().unwrap() as u16, CHARSET_UTF8MB4);
        assert_eq!(reader.u16().unwrap(), SERVER_STATUS_AUTOCOMMIT);
        let upper = reader.u16().unwrap() as u32;
        assert_eq!(lower | (upper << 16), SERVER_CAPABILITIES);

        assert_eq!(reader.u8().unwrap(), 21, "challenge length");
        assert_eq!(reader.take(10).unwrap(), &[0u8; 10]);
        assert_eq!(reader.take(12).unwrap(), &scramble[8..]);
        assert_eq!(reader.u8().unwrap(), 0);
        assert_eq!(
            reader.nul_str().unwrap(),
            crate::auth::CACHING_SHA2_PASSWORD,
            "AHL-467: caching_sha2_password is the default plugin now"
        );
        assert!(reader.is_empty(), "the handshake had trailing bytes");
    }

    #[test]
    fn auth_switch_names_whichever_plugin_it_is_given() {
        let scramble: Vec<u8> = (1..=20).collect();
        for plugin in [
            crate::auth::NATIVE_PASSWORD,
            crate::auth::CACHING_SHA2_PASSWORD,
        ] {
            let packet = auth_switch_request(plugin, &scramble);
            let mut reader = Reader::new(&packet);
            assert_eq!(reader.u8().unwrap(), 0xfe);
            assert_eq!(reader.nul_str().unwrap(), plugin);
            assert_eq!(reader.take(20).unwrap(), &scramble[..]);
            assert_eq!(reader.u8().unwrap(), 0);
            assert!(reader.is_empty());
        }
    }

    #[test]
    fn auth_more_data_wraps_its_payload_in_the_0x01_header() {
        assert_eq!(auth_more_data(&[0x03]), vec![0x01, 0x03]);
        assert_eq!(auth_more_data(&[0x04]), vec![0x01, 0x04]);
        assert_eq!(auth_more_data(&[]), vec![0x01]);
    }

    /// The capabilities left out are left out on purpose; a regression that
    /// quietly advertises TLS or multi-statement support should fail here.
    #[test]
    fn dangerous_capabilities_are_not_advertised() {
        assert_eq!(SERVER_CAPABILITIES & CLIENT_SSL, 0, "v1 has no TLS");
        assert_eq!(
            SERVER_CAPABILITIES & 0x0001_0000,
            0,
            "CLIENT_MULTI_STATEMENTS would mean silently running only the first"
        );
        assert_eq!(
            SERVER_CAPABILITIES & 0x0000_0080,
            0,
            "CLIENT_LOCAL_FILES lets a server read the client's disk"
        );
        assert_eq!(
            SERVER_CAPABILITIES & 0x0100_0000,
            0,
            "CLIENT_DEPRECATE_EOF changes result-set framing"
        );
    }

    #[test]
    fn an_error_message_cannot_carry_newlines_onto_a_terminal() {
        let packet = err_packet(1064, "42000", "bad\nthings\rhappened");
        let text = String::from_utf8(packet[9..].to_vec()).unwrap();
        assert_eq!(text, "bad things happened");
    }

    #[test]
    fn a_short_sqlstate_is_padded_rather_than_truncating_the_message() {
        let packet = err_packet(1105, "42", "oops");
        assert_eq!(&packet[4..9], b"42000");
        assert_eq!(&packet[9..], b"oops");
    }

    /// A column definition, from values alone — the shim's case, where no plan
    /// declared anything.
    fn undeclared(rows: &[Vec<Value>], index: usize) -> ColumnDef {
        column_def_for(String::new(), None, rows, index)
    }

    #[test]
    fn a_column_type_is_unified_across_every_row() {
        let integers = vec![vec![Value::Integer(1)], vec![Value::Integer(2)]];
        assert_eq!(undeclared(&integers, 0).ty, TYPE_LONGLONG);

        // One real in the column makes the whole column real.
        let mixed = vec![vec![Value::Integer(1)], vec![Value::Real(2.5)]];
        assert_eq!(undeclared(&mixed, 0).ty, TYPE_DOUBLE);

        // A number beside text has to fall back to text.
        let ragged = vec![vec![Value::Integer(1)], vec![Value::Text("x".into())]];
        assert_eq!(undeclared(&ragged, 0).ty, TYPE_VAR_STRING);

        // NULLs never decide the type.
        let nulls = vec![vec![Value::Null], vec![Value::Integer(3)]];
        assert_eq!(undeclared(&nulls, 0).ty, TYPE_LONGLONG);
        assert_eq!(undeclared(&[vec![Value::Null]], 0).ty, TYPE_VAR_STRING);

        let blobs = vec![vec![Value::Blob(vec![1, 2])]];
        assert_eq!(undeclared(&blobs, 0).ty, TYPE_BLOB);
    }

    /// The declared type only ever speaks where the values are silent — an
    /// empty result set, or a column that is `NULL` all the way down. A
    /// `NUMERIC` column really does hold different classes in different rows,
    /// so where there *are* values they still win.
    #[test]
    fn a_declared_type_answers_only_where_the_values_cannot() {
        let empty: Vec<Vec<Value>> = Vec::new();
        assert_eq!(
            column_def_for("id".into(), Some(DataType::Integer), &empty, 0).ty,
            TYPE_LONGLONG,
            "an empty result set still knows what its columns are"
        );
        let nulls = vec![vec![Value::Null], vec![Value::Null]];
        assert_eq!(
            column_def_for("b".into(), Some(DataType::Blob), &nulls, 0).ty,
            TYPE_BLOB
        );
        assert_eq!(
            column_def_for("x".into(), None, &nulls, 0).ty,
            TYPE_VAR_STRING,
            "nothing declared and nothing observed leaves only the widest type"
        );
        let integers = vec![vec![Value::Integer(1)]];
        assert_eq!(
            column_def_for("n".into(), Some(DataType::Numeric), &integers, 0).ty,
            TYPE_LONGLONG,
            "an affinity column is described by what it actually held"
        );
    }

    /// The types [`streamed_column_def`] accepts are exactly the ones the
    /// engine enforces on write, so a streamed column definition is the one
    /// [`column_def_for`] would have reached after seeing every row. This is
    /// the property that makes a streamed result set byte-identical to a
    /// materialised one; if a new [`DataType`] is added, it belongs on one
    /// side of this test or the other, deliberately.
    #[test]
    fn a_streamed_column_type_matches_what_the_values_would_have_said() {
        let cases = [
            (DataType::Integer, Value::Integer(1)),
            (DataType::Real, Value::Real(1.5)),
            (DataType::Text, Value::Text("x".into())),
            (DataType::Blob, Value::Blob(vec![1])),
            (DataType::Vector(2), Value::Vector(vec![0.5, 0.25])),
            (DataType::QuantizedVector(2), Value::Vector(vec![0.5, 0.25])),
        ];
        for (declared, value) in cases {
            let streamed = streamed_column_def("c".into(), Some(declared))
                .unwrap_or_else(|| panic!("{declared:?} should stream"));
            let rows = vec![vec![value.clone()], vec![Value::Null]];
            let materialised = column_def_for("c".into(), Some(declared), &rows, 0);
            assert_eq!(streamed.ty, materialised.ty, "{declared:?} type");
            assert_eq!(
                streamed.charset, materialised.charset,
                "{declared:?} charset"
            );
            assert_eq!(streamed.flags, materialised.flags, "{declared:?} flags");
            assert_eq!(streamed.length, materialised.length, "{declared:?} length");
        }

        // The two that genuinely cannot be answered without the values.
        assert!(streamed_column_def("c".into(), Some(DataType::Numeric)).is_none());
        assert!(streamed_column_def("c".into(), None).is_none());
    }

    #[test]
    fn reals_render_the_way_mysql_renders_them() {
        assert_eq!(format_real(1.0), "1");
        assert_eq!(format_real(1.5), "1.5");
        assert_eq!(format_real(-0.25), "-0.25");
        // IEEE has values SQL cannot spell; none may produce a token a client
        // would fail to parse as a number.
        assert_eq!(format_real(f64::NAN), "NULL");
        assert!(format_real(f64::INFINITY).parse::<f64>().is_ok());
    }

    #[test]
    fn a_vector_renders_as_the_literal_that_reconstructs_it() {
        assert_eq!(format_vector(&[0.5, -1.5]), "[0.5,-1.5]");
        assert_eq!(format_vector(&[]), "[]");
    }
}
