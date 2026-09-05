//! The packet path's entry points, reachable from a fuzz target.
//!
//! Everything the wire touches lives in a private module — `packet` and
//! `connection` are `mod`, not `pub mod`, and that is right: the framing, the
//! handshake and the parameter decoders are this crate's business and nobody
//! else's. A fuzz target is the one caller that needs them anyway, because a
//! target that can only reach the public surface
//! (`MysqlError`, `SERVER_VERSION`, `tls`) fuzzes none of the code that reads
//! bytes off a socket.
//!
//! So the seam is a non-default feature rather than a widened API. Under
//! `--features fuzzing` this module is compiled and public; in every normal
//! build — every `cargo build`, every release, every dependent crate — the
//! feature is off, the module does not exist, and `inlaysql-server`'s public
//! API is exactly what it was. It is also compiled under `cfg(test)`, so the
//! wrappers below are built and exercised by an ordinary `cargo test` even
//! though the feature is off: a wrapper that stopped compiling would otherwise
//! only be discovered by the nightly fuzzing campaign, which is the gap
//! `fuzz/` being a separate workspace already leaves.
//!
//! Nothing here is stable. It is `#[doc(hidden)]`, it returns summaries rather
//! than the crate's own types, and it may change shape whenever the code it
//! wraps does.
//!
//! ## What the wrappers return, and why it is not the real type
//!
//! Each wrapper returns a *description* of what the parse produced — how many
//! bytes each field owns, how many messages were framed, how many times the
//! reader was asked for bytes — rather than the private struct itself. Two
//! reasons. The parsed handshake owns an authentication token, and a fuzz
//! target that printed one on failure would write it into a CI log and an
//! artifact file; `HandshakeResponse`'s hand-written `Debug` redacts it for
//! the same reason. And the invariants the targets assert are about *size and
//! work*, not about values: "this parse allocated no more than the bytes it
//! was given" needs a length, not a `String`.

use std::cell::Cell;
use std::io::{self, Read, Write};
use std::rc::Rc;

use inlaysql::Value;

use crate::connection;
use crate::errors::MysqlError;
use crate::packet::{Reader, Stream};
use crate::protocol::Command;

/// The largest message the framing layer will reassemble.
pub use crate::packet::MAX_MESSAGE;

/// The largest payload one packet can carry.
pub use crate::packet::MAX_PAYLOAD;

/// The capability bit a client must set to be served at all.
pub use crate::protocol::CLIENT_PROTOCOL_41;

/// The capability bit that asks for, or reports, a TLS upgrade.
pub use crate::protocol::CLIENT_SSL;

// ------------------------------------------------------------ framing

/// One end of a connection whose bytes are a slice, counting the reads.
///
/// The count is the loop-boundedness instrument: every iteration of every loop
/// in the framing path asks the reader for bytes, so a number of reads that is
/// not proportional to the input is an unbounded loop, and it is a *counter*
/// rather than a wall clock so the assertion means the same thing on a laptop
/// and on a loaded CI runner.
struct Wire<'a> {
    bytes: &'a [u8],
    at: usize,
    reads: Rc<Cell<usize>>,
}

impl Read for Wire<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.reads.set(self.reads.get() + 1);
        let available = &self.bytes[self.at.min(self.bytes.len())..];
        let take = available.len().min(out.len());
        out[..take].copy_from_slice(&available[..take]);
        self.at += take;
        Ok(take)
    }
}

impl Write for Wire<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What framing a slice produced.
#[derive(Debug)]
pub struct Framing {
    /// The whole messages the stream reassembled, in order.
    pub messages: Vec<Vec<u8>>,
    /// How many times the stream asked the socket for bytes.
    pub reads: usize,
    /// Whether the stream ran out of input at a message boundary rather than
    /// refusing something.
    pub clean_end: bool,
}

impl Framing {
    /// The payload bytes the framing handed back.
    ///
    /// A message is stitched out of bytes that arrived, so this can never
    /// exceed the length of the input: a larger number means the framing
    /// invented payload, which is the shape a pre-read allocation takes when
    /// it is handed back rather than dropped.
    pub fn payload_bytes(&self) -> usize {
        self.messages.iter().map(Vec::len).sum()
    }
}

/// Frame `data` as though it had arrived on a connection, to the end.
///
/// One input is a whole session's framing rather than a single packet: the
/// continuation rule ([`MAX_PAYLOAD`] means "more follows") is a loop, and a
/// loop is only interesting when it can run more than once.
pub fn read_messages(data: &[u8]) -> Framing {
    let reads = Rc::new(Cell::new(0));
    let read_half = Wire {
        bytes: data,
        at: 0,
        reads: Rc::clone(&reads),
    };
    let write_half = Wire {
        bytes: &[],
        at: 0,
        reads: Rc::new(Cell::new(0)),
    };
    let mut stream = Stream::new(read_half, write_half);

    let mut messages = Vec::new();
    let mut clean_end = false;
    loop {
        match stream.read_message() {
            Ok(Some(message)) => messages.push(message),
            Ok(None) => {
                clean_end = true;
                break;
            }
            // A refusal is the expected outcome for most inputs, and it ends
            // the session exactly as it does on a real connection.
            Err(_) => break,
        }
    }

    Framing {
        messages,
        reads: reads.get(),
        clean_end,
    }
}

// ------------------------------------------------------------ handshake

/// The shape of a parsed `HandshakeResponse41` — every field's size, and no
/// field's contents.
#[derive(Debug)]
pub struct Handshake {
    /// The capability bitmask the client sent.
    pub capabilities: u32,
    /// Bytes in the user name.
    pub username: usize,
    /// Bytes in the authentication token. The token itself is deliberately
    /// not carried here; see the module doc.
    pub auth_response: usize,
    /// Bytes in the requested database name, if one was sent.
    pub database: Option<usize>,
    /// Bytes in the requested authentication plugin's name.
    pub auth_plugin: usize,
}

impl Handshake {
    /// The heap the parsed response owns.
    ///
    /// Every field is a copy taken out of the packet, so this is bounded by
    /// the packet's own length; anything larger is a field read from somewhere
    /// the packet did not reach.
    pub fn owned_bytes(&self) -> usize {
        self.username + self.auth_response + self.database.unwrap_or(0) + self.auth_plugin
    }
}

/// Parse a client's handshake response.
///
/// `expect_ssl_request` is the handshake *phase*: before a TLS upgrade a
/// client that asks for one has sent 32 bytes and stopped, and after the
/// upgrade the same capability bit is still set on a full response. Both
/// phases are fuzzed, because reading the flag instead of the phase is exactly
/// the confusion that was fixed on 2026-09-05.
pub fn parse_handshake_response(
    data: &[u8],
    expect_ssl_request: bool,
) -> Result<Handshake, MysqlError> {
    let parsed = connection::parse_handshake_response(data, expect_ssl_request)?;
    Ok(Handshake {
        capabilities: parsed.capabilities,
        username: parsed.username.len(),
        auth_response: parsed.auth_response.len(),
        database: parsed.database.as_ref().map(String::len),
        auth_plugin: parsed.auth_plugin.len(),
    })
}

// ------------------------------------------------------------ parameters

/// How many placeholders a fuzzed `COM_STMT_EXECUTE` may declare.
///
/// A real `param_count` comes from the planned statement rather than from the
/// wire (`connection::decode_execute` reads it from the prepared statement),
/// so an unbounded count is not a shape a client can produce. Capped here so
/// the target fuzzes the decoders rather than the size of a loop the wire does
/// not control.
pub const MAX_FUZZED_PARAMS: usize = 4096;

/// What decoding a parameter list produced.
#[derive(Debug)]
pub struct Params {
    /// The heap each decoded value owns, in order.
    pub owned: Vec<usize>,
    /// How far into the body the decoder read.
    pub consumed: usize,
}

impl Params {
    /// The heap all the decoded values own together.
    pub fn owned_bytes(&self) -> usize {
        self.owned.iter().sum()
    }
}

/// The heap one decoded value owns.
fn owned_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Integer(_) | Value::Real(_) => 0,
        Value::Text(text) => text.as_str().len(),
        Value::Blob(bytes) => bytes.len(),
        Value::Vector(components) => components.len() * 4,
    }
}

/// Decode the bound parameters of a `COM_STMT_EXECUTE` body.
///
/// This is `connection::decode_execute`'s parameter loop with the two pieces
/// of *server-side* state passed in rather than looked up: the declared types
/// and, per placeholder, the `VECTOR` dimension the planned statement says it
/// is. Neither comes off the wire, which is why the target supplies them as
/// structured input instead of parsing them out of the body — see
/// `decode_vector_param` for why the wire cannot say.
///
/// `body` starts at the NULL bitmap, i.e. after the statement id, the flags
/// and the iteration count that `decode_execute` reads first.
pub fn decode_params(
    body: &[u8],
    types: &[(u8, bool)],
    vector_dims: &[Option<usize>],
) -> Result<Params, MysqlError> {
    let malformed = || MysqlError::unknown("malformed COM_STMT_EXECUTE packet");
    let mut reader = Reader::new(body);
    let count = types.len();
    let null_bitmap = reader
        .take(count.div_ceil(8))
        .map_err(|_| malformed())?
        .to_vec();

    let mut owned = Vec::with_capacity(count);
    for (index, (ty, unsigned)) in types.iter().enumerate() {
        let is_null = null_bitmap
            .get(index / 8)
            .is_some_and(|byte| byte & (1 << (index % 8)) != 0);
        if is_null {
            owned.push(0);
            continue;
        }
        let value = match vector_dims.get(index).copied().flatten() {
            Some(dim) => connection::decode_vector_param(&mut reader, *ty, dim, index)?,
            None => connection::decode_binary_param(&mut reader, *ty, *unsigned)
                .map_err(|_| malformed())?,
        };
        owned.push(owned_bytes(&value));
    }

    Ok(Params {
        owned,
        consumed: reader.position(),
    })
}

// ------------------------------------------------------------ commands

/// What classifying and decoding one command message produced.
#[derive(Debug)]
pub struct Dispatched {
    /// The command byte, or `None` for an empty message.
    pub command: Option<u8>,
    /// The heap the body decoding owns: the lossy UTF-8 copy a text command
    /// makes of its body, or nothing for the commands that read a fixed-width
    /// id.
    pub owned: usize,
}

/// Classify a command message and decode its body, without executing it.
///
/// Everything `connection::dispatch` does with a message before it reaches the
/// engine: `Command::from_byte`, which must totalise over all 256 bytes; the
/// four-byte id reads of `COM_STMT_CLOSE`, `COM_STMT_RESET` and
/// `COM_PROCESS_KILL`; the lossy UTF-8 copy `COM_QUERY`, `COM_STMT_PREPARE`
/// and `COM_INIT_DB` make of their bodies; and `check_database`. The engine is
/// deliberately not reached — a target that planned and ran the SQL would be
/// `sql_parser` with a one-byte prefix, and would spend its whole budget
/// there.
pub fn dispatch_stateless(data: &[u8]) -> Dispatched {
    let Some((&head, body)) = data.split_first() else {
        return Dispatched {
            command: None,
            owned: 0,
        };
    };
    let mut owned = 0;
    match Command::from_byte(head) {
        Command::InitDb => {
            let name = String::from_utf8_lossy(body).to_string();
            owned = name.len();
            let _ = connection::check_database(&name);
        }
        Command::Query | Command::StmtPrepare => {
            owned = String::from_utf8_lossy(body).to_string().len();
        }
        Command::StmtClose | Command::StmtReset | Command::ProcessKill => {
            let _ = Reader::new(body).u32();
        }
        Command::StmtExecute => {
            // The header `decode_execute` reads before the parameters; the
            // parameters themselves need a prepared statement, and are
            // `server_stmt_params`' subject.
            let mut reader = Reader::new(body);
            let _ = reader.u32();
            let _ = reader.u8();
            let _ = reader.u32();
        }
        Command::Quit | Command::Ping | Command::FieldList | Command::Unknown(_) => {}
    }
    Dispatched {
        command: Some(head),
        owned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framing property, over the inputs the corpus seeds:
    /// every one terminates, and none hands back more payload than it was
    /// given.
    #[test]
    fn framing_terminates_and_invents_no_payload() {
        for input in [
            vec![0xff, 0xff, 0xff, 0x00],
            b"\x05\x00\x00\x00hello".to_vec(),
            vec![0x00, 0x00, 0x00, 0x00],
            [0xff, 0xff, 0xff, 0x01].repeat(5),
            Vec::new(),
        ] {
            let framing = read_messages(&input);
            assert!(
                framing.payload_bytes() <= input.len(),
                "framing handed back {} payload bytes from {} input bytes",
                framing.payload_bytes(),
                input.len()
            );
            assert!(
                framing.reads <= 4 * input.len() + 64,
                "{} reads for {} bytes is not a bounded loop",
                framing.reads,
                input.len()
            );
        }
    }

    /// A header claiming sixteen mebibytes that never arrive is refused, and
    /// the refusal costs one read.
    #[test]
    fn a_claim_that_never_arrives_is_refused() {
        let framing = read_messages(&[0xff, 0xff, 0xff, 0x00]);
        assert!(framing.messages.is_empty());
        assert!(!framing.clean_end, "a truncated payload is not a clean end");
    }

    /// A handshake response never owns more than the packet it was read from.
    #[test]
    fn a_handshake_owns_no_more_than_its_packet() {
        for expect_ssl_request in [true, false] {
            for input in [
                Vec::new(),
                vec![0xff; 4],
                vec![0xff; 32],
                vec![0x00, 0x82, 0x00, 0x00],
            ] {
                if let Ok(handshake) = parse_handshake_response(&input, expect_ssl_request) {
                    assert!(
                        handshake.owned_bytes() <= input.len(),
                        "a {} byte handshake owns {} bytes",
                        input.len(),
                        handshake.owned_bytes()
                    );
                }
            }
        }
    }

    /// A `TIME` parameter whose day count is `u32::MAX` is a malformed packet
    /// rather than a panic — the fix of 2026-09-05, pinned where the fuzz
    /// target would find it.
    #[test]
    fn a_time_parameter_cannot_overflow_its_hours() {
        let mut body = vec![0x00]; // NULL bitmap, one placeholder, not null
        body.extend_from_slice(&[12, 0, 0xff, 0xff, 0xff, 0xff, 23, 59, 59, 0, 0, 0, 0]);
        let decoded = decode_params(&body, &[(0x0b, false)], &[]);
        assert!(decoded.is_err(), "a day count that does not fit is refused");
    }

    /// Every one of the 256 command bytes is classified and its body decoded
    /// without panicking, and none of them copies more than three bytes per
    /// input byte — the widest a lossy UTF-8 copy can be.
    #[test]
    fn every_command_byte_is_handled() {
        for byte in 0..=u8::MAX {
            for body in [Vec::new(), vec![0xff; 1024]] {
                let mut message = vec![byte];
                message.extend_from_slice(&body);
                let dispatched = dispatch_stateless(&message);
                assert_eq!(dispatched.command, Some(byte));
                assert!(dispatched.owned <= 3 * body.len());
            }
        }
        assert_eq!(dispatch_stateless(&[]).command, None);
    }

    /// A parameter decoder reads inside its body and nowhere else.
    #[test]
    fn a_parameter_list_reads_only_its_own_body() {
        let body = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        for ty in 0..=u8::MAX {
            if let Ok(params) = decode_params(&body, &[(ty, false)], &[]) {
                assert!(params.consumed <= body.len());
                // Plus the widest text a temporal decoder renders: five bytes
                // of `DATE` are the ten characters `2026-09-05`, which is the
                // engine having no temporal type rather than a finding. See
                // `fuzz/fuzz_targets/server_stmt_params.rs`.
                assert!(params.owned_bytes() <= params.consumed + 32);
            }
        }
    }
}
