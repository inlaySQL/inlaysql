//! MySQL packet framing, and the primitive encodings the payloads are built
//! from.
//!
//! Every message on the wire is a 4-byte header — a 3-byte little-endian
//! payload length and a 1-byte sequence id — followed by that many payload
//! bytes. A payload of exactly [`MAX_PAYLOAD`] bytes means "more follows", so a
//! large result row arrives as several packets that have to be stitched back
//! together; a message whose length is an exact multiple of [`MAX_PAYLOAD`] is
//! therefore terminated by an empty packet. Both directions of that rule live
//! here so nothing above this module has to think about it.
//!
//! The sequence id is shared by both directions and resets to zero with every
//! new command, which is why [`Stream`] owns it rather than the reader and the
//! writer owning one each: a reply is numbered from the request it answers.

use std::io::{self, BufReader, BufWriter, Read, Write};

/// The largest payload one packet can carry. A payload of exactly this size is
/// continued by the packet after it.
pub const MAX_PAYLOAD: usize = 0xff_ff_ff;

/// The largest message this server will reassemble from continuation packets.
///
/// A client can otherwise ask the server to allocate without bound simply by
/// never ending a sequence of maximum-size packets. Nothing a real client sends
/// comes close to this, so refusing past it costs nothing and removes the
/// cheapest denial-of-service this protocol offers.
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// How much of a packet's payload is read at a time.
///
/// The unit in which [`Stream::read_message`] grows its buffer, so it is also
/// the most a client can make this server hold per connection by declaring a
/// length it never sends. Sixty-four kibibytes is large enough that a real
/// sixteen-mebibyte message costs a couple of hundred reads from an already
/// buffered reader, and small enough that the claim itself buys nothing.
const READ_CHUNK: usize = 64 * 1024;

/// One connection's framed byte stream.
pub struct Stream<S: Read + Write> {
    reader: BufReader<S>,
    writer: BufWriter<S>,
    /// The id the next packet written will carry. Set from the last packet
    /// read, so a reply continues the exchange it answers.
    sequence: u8,
    /// Bytes framed in and out since [`Stream::take_traffic`] last ran, headers
    /// included, for `Bytes_received` and `Bytes_sent`.
    ///
    /// Plain `u64`s and not the shared atomics they end up in: a result set is
    /// one `write_message` per row, so an atomic here would put a contended
    /// read-modify-write on the per-row path in exchange for a number nobody
    /// can read until the statement finishes anyway. The connection drains
    /// these into [`crate::metrics::Metrics`] once per command instead — two
    /// atomic adds for a ten-million-row `SELECT`, the same two as for a
    /// `PING`.
    traffic: (u64, u64),
}

impl<S: Read + Write> Stream<S> {
    /// Wrap a pair of handles to the same connection.
    ///
    /// Two handles rather than one because the reader and the writer are
    /// buffered independently; for a `TcpStream` they come from
    /// `TcpStream::try_clone`.
    pub fn new(read_half: S, write_half: S) -> Self {
        Self {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
            sequence: 0,
            traffic: (0, 0),
        }
    }

    /// Upgrade both directions of this stream to TLS.
    ///
    /// Called from the handshake, after the client's `SSLRequest` has been
    /// read and before anything else is. Three things have to be true at once
    /// and each is handled here rather than assumed:
    ///
    /// * **Nothing buffered may be dropped.** The reader is buffered, so it may
    ///   already hold the first bytes of the client's TLS `ClientHello`. Those
    ///   bytes are handed to the session (see [`crate::tls::Prefixed`]);
    ///   losing them hangs the handshake.
    /// * **The writer's pending bytes must reach the client in the clear.** The
    ///   greeting is plaintext by definition, so the writer is flushed before
    ///   the socket is taken over.
    /// * **Both directions must share one session.** A TLS session is one state
    ///   machine over one byte stream; the second descriptor this stream was
    ///   built with is dropped, and the writer is pointed at the session the
    ///   reader negotiated.
    pub fn upgrade_to_tls(&mut self, config: &crate::tls::TlsConfig) -> io::Result<()>
    where
        S: crate::tls::Upgradable,
    {
        self.writer.flush()?;
        let buffered = self.reader.buffer().to_vec();
        let reader = std::mem::replace(&mut self.reader, BufReader::new(S::placeholder()));
        let mut inner = reader.into_inner();
        let shared = inner.upgrade_with(config, buffered)?;
        self.reader = BufReader::new(inner);
        let mut writer_half = S::placeholder();
        writer_half.adopt_session(shared);
        // The old write descriptor goes out of scope here, closing this
        // process's second handle on the socket while the session's own handle
        // keeps it open.
        self.writer = BufWriter::new(writer_half);
        Ok(())
    }

    /// Whether this connection is encrypted.
    pub fn is_encrypted(&self) -> bool
    where
        S: crate::tls::Upgradable,
    {
        self.reader.get_ref().encrypted()
    }

    /// The bytes read and written since this last ran, and reset.
    ///
    /// Taken rather than read so the caller can add them to a running total
    /// without keeping a second copy of what it has already counted.
    pub fn take_traffic(&mut self) -> (u64, u64) {
        std::mem::take(&mut self.traffic)
    }

    /// Read one whole message, following continuation packets.
    ///
    /// Returns `Ok(None)` at a clean end of stream — a client that closed the
    /// socket without a `COM_QUIT`, which is ordinary rather than an error.
    pub fn read_message(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            match read_full(&mut self.reader, &mut header)? {
                // Nothing at all: the peer hung up between messages.
                0 => return Ok(None),
                4 => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection ended part-way through a packet header",
                    ))
                }
            }
            let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
            self.sequence = header[3].wrapping_add(1);

            if payload.len() + length > MAX_MESSAGE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("message longer than the {MAX_MESSAGE} byte limit"),
                ));
            }
            read_payload(&mut self.reader, &mut payload, length)?;
            // Counted after the read succeeds, so `Bytes_received` is bytes
            // this server actually took delivery of rather than bytes it hoped
            // for. Header included, because that is what crossed the wire.
            self.traffic.0 += (4 + length) as u64;

            // Only a full-size packet promises another after it.
            if length < MAX_PAYLOAD {
                return Ok(Some(payload));
            }
        }
    }

    /// Write one message, splitting it across packets if it does not fit in one.
    pub fn write_message(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut rest = payload;
        loop {
            let take = rest.len().min(MAX_PAYLOAD);
            let mut header = [0u8; 4];
            header[..3].copy_from_slice(&(take as u32).to_le_bytes()[..3]);
            header[3] = self.sequence;
            self.sequence = self.sequence.wrapping_add(1);
            self.writer.write_all(&header)?;
            self.writer.write_all(&rest[..take])?;
            self.traffic.1 += (4 + take) as u64;
            rest = &rest[take..];
            // A message whose length is a multiple of the maximum needs a
            // trailing empty packet to say it has ended.
            if take < MAX_PAYLOAD {
                return Ok(());
            }
            if rest.is_empty() {
                let header = [0, 0, 0, self.sequence];
                self.sequence = self.sequence.wrapping_add(1);
                self.writer.write_all(&header)?;
                self.traffic.1 += 4;
                return Ok(());
            }
        }
    }

    /// Flush everything buffered to the peer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Read until `buf` is full or the stream ends, reporting how many bytes
/// arrived. Distinguishes "ended cleanly at a message boundary" from "ended
/// mid-header", which the caller treats very differently.
/// Read `length` bytes onto the end of `payload`, growing it as the bytes
/// arrive rather than as the header claims they will.
///
/// The header's three length bytes buy up to 16 MiB, and committing that with
/// one `resize` before any of it has been received is what made four bytes and
/// then silence cost this server 16 MiB per connection until the read timeout
/// — eight hours by default — with `MAX_CONNECTIONS` of them a gibibyte, all
/// of it reachable before authentication. [`MAX_MESSAGE`] bounds the
/// reassembled total and never bounded this.
///
/// Reading in [`READ_CHUNK`] steps directly into the tail of `payload` keeps
/// the memory proportional to what actually crossed the wire and leaves a
/// well-behaved client with byte-identical results. `payload` is borrowed
/// rather than returned so that a caller — and a test — can see how far it
/// grew even when the read fails.
fn read_payload(reader: &mut impl Read, payload: &mut Vec<u8>, length: usize) -> io::Result<()> {
    let mut filled = 0;
    while filled < length {
        let take = (length - filled).min(READ_CHUNK);
        let at = payload.len();
        payload.resize(at + take, 0);
        let read = read_full(reader, &mut payload[at..])?;
        filled += read;
        if read < take {
            payload.truncate(at + read);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection ended part-way through a packet payload",
            ));
        }
    }
    Ok(())
}

fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Ok(filled),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

// ------------------------------------------------------------ writing

/// Append a length-encoded integer.
pub fn put_lenenc_int(out: &mut Vec<u8>, value: u64) {
    match value {
        // 0xfb and 0xff are taken as packet markers (NULL, and the ERR
        // header), so the one-byte form stops short of them.
        0..=0xfa => out.push(value as u8),
        0xfb..=0xffff => {
            out.push(0xfc);
            out.extend_from_slice(&(value as u16).to_le_bytes());
        }
        0x1_0000..=0xff_ffff => {
            out.push(0xfd);
            out.extend_from_slice(&(value as u32).to_le_bytes()[..3]);
        }
        _ => {
            out.push(0xfe);
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// Append a length-encoded string.
pub fn put_lenenc_bytes(out: &mut Vec<u8>, value: &[u8]) {
    put_lenenc_int(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// Append a length-encoded string from text.
pub fn put_lenenc_str(out: &mut Vec<u8>, value: &str) {
    put_lenenc_bytes(out, value.as_bytes());
}

/// Append a NUL-terminated string.
///
/// Any interior NUL is dropped rather than truncating the field, so a value
/// carrying one cannot desynchronise everything after it in the packet.
pub fn put_nul_str(out: &mut Vec<u8>, value: &str) {
    out.extend(value.bytes().filter(|&b| b != 0));
    out.push(0);
}

// ------------------------------------------------------------ reading

/// A cursor over one packet payload.
///
/// Every accessor is bounds-checked and reports a short packet as an error, so
/// a truncated or hostile packet cannot panic the connection thread.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// Start reading `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Whether anything is left.
    ///
    /// Only the tests need this, and they need it a great deal: "the packet
    /// parsed and nothing was left over" is what proves an encoder and its
    /// decoder agree on a layout, rather than merely agreeing on its prefix.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    /// Take `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let end = self.at.checked_add(n).ok_or(Malformed)?;
        let slice = self.bytes.get(self.at..end).ok_or(Malformed)?;
        self.at = end;
        Ok(slice)
    }

    /// Take one byte.
    pub fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }

    /// Take a little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16, Malformed> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Take a little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, Malformed> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Take a little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, Malformed> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Take a length-encoded integer. `None` is the 0xfb NULL marker.
    pub fn lenenc_int(&mut self) -> Result<Option<u64>, Malformed> {
        match self.u8()? {
            0xfb => Ok(None),
            0xfc => Ok(Some(self.u16()? as u64)),
            0xfd => {
                let b = self.take(3)?;
                Ok(Some(u32::from_le_bytes([b[0], b[1], b[2], 0]) as u64))
            }
            0xfe => Ok(Some(self.u64()?)),
            small => Ok(Some(small as u64)),
        }
    }

    /// Take a length-encoded string. `None` is SQL `NULL`.
    pub fn lenenc_bytes(&mut self) -> Result<Option<&'a [u8]>, Malformed> {
        match self.lenenc_int()? {
            None => Ok(None),
            Some(length) => {
                let length = usize::try_from(length).map_err(|_| Malformed)?;
                Ok(Some(self.take(length)?))
            }
        }
    }

    /// Take a NUL-terminated string.
    pub fn nul_str(&mut self) -> Result<&'a str, Malformed> {
        let rest = self.bytes.get(self.at..).ok_or(Malformed)?;
        let end = rest.iter().position(|&b| b == 0).ok_or(Malformed)?;
        let text = std::str::from_utf8(&rest[..end]).map_err(|_| Malformed)?;
        self.at += end + 1;
        Ok(text)
    }
}

/// A packet that ended sooner than its own contents said it would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("malformed packet")
    }
}

impl std::error::Error for Malformed {}

impl From<Malformed> for io::Error {
    fn from(_: Malformed) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, "malformed packet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback that a `Stream` can be pointed at: what gets written is what
    /// comes back out, so framing is tested against its own parser.
    struct Loopback {
        buffer: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        read_at: usize,
    }

    impl Read for Loopback {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let buffer = self.buffer.borrow();
            let available = &buffer[self.read_at.min(buffer.len())..];
            let n = available.len().min(out.len());
            out[..n].copy_from_slice(&available[..n]);
            self.read_at += n;
            Ok(n)
        }
    }

    impl Write for Loopback {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffer.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn loopback() -> (Loopback, Loopback) {
        let buffer = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        (
            Loopback {
                buffer: buffer.clone(),
                read_at: 0,
            },
            Loopback { buffer, read_at: 0 },
        )
    }

    fn round_trip(payload: &[u8]) -> Vec<u8> {
        let (read_half, write_half) = loopback();
        let mut stream = Stream::new(read_half, write_half);
        stream.write_message(payload).unwrap();
        stream.flush().unwrap();
        stream.read_message().unwrap().expect("a message")
    }

    /// A reader that hands out a fixed script and then reports end of
    /// stream, counting how many bytes were ever asked for. Enough to say
    /// what a client's *claim* costs a server that never receives it.
    struct Truncated {
        bytes: Vec<u8>,
        at: usize,
        served: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Read for Truncated {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let take = (self.bytes.len() - self.at).min(buf.len());
            buf[..take].copy_from_slice(&self.bytes[self.at..self.at + take]);
            self.at += take;
            self.served.set(self.served.get() + take);
            Ok(take)
        }
    }

    impl Write for Truncated {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A header claiming a payload the client never sends costs the bytes
    /// that arrived, not the bytes it claimed.
    ///
    /// The three length bytes can ask for sixteen mebibytes. Committing that
    /// with one `resize` before any of it has been received — which is what
    /// this did until 2026-09-05 — held 16 MiB per connection until the read
    /// timeout, eight hours by default, and `MAX_CONNECTIONS` of them a
    /// gibibyte, all of it reachable without authenticating. `MAX_MESSAGE`
    /// bounds the reassembled total and never bounded this.
    ///
    /// The buffer is passed in rather than returned precisely so this can be
    /// asserted: after the failure it must have grown by about what arrived,
    /// not by what was claimed. Against the old one-shot `resize` the
    /// capacity assertion fails with sixteen mebibytes.
    #[test]
    fn a_claimed_payload_that_never_arrives_costs_what_arrived() {
        let served = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut reader = Truncated {
            bytes: b"0123456789".to_vec(),
            at: 0,
            served: std::rc::Rc::clone(&served),
        };

        let mut payload = Vec::new();
        let error = read_payload(&mut reader, &mut payload, MAX_PAYLOAD)
            .expect_err("a payload that never arrives is not a payload");

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(payload.len(), 10, "the ten bytes that existed are kept");
        assert_eq!(served.get(), 10, "and only those were ever asked for");
        assert!(
            payload.capacity() <= READ_CHUNK * 2,
            "a claim of {MAX_PAYLOAD} bytes grew the buffer to {} — the \
             client's claim was committed before its bytes arrived",
            payload.capacity()
        );
    }

    /// The same read, whole: every byte claimed arrives, so the payload is
    /// exactly what was sent and nothing was truncated at a chunk boundary.
    #[test]
    fn a_payload_larger_than_one_chunk_is_stitched_from_its_chunks() {
        let sent: Vec<u8> = (0..READ_CHUNK * 2 + 7).map(|i| (i % 251) as u8).collect();
        let served = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut reader = Truncated {
            bytes: sent.clone(),
            at: 0,
            served,
        };

        let mut payload = vec![0xaa];
        read_payload(&mut reader, &mut payload, sent.len()).expect("every byte arrived");
        assert_eq!(payload[0], 0xaa, "what was already there is untouched");
        assert_eq!(&payload[1..], &sent[..]);
    }

    #[test]
    fn a_small_message_round_trips() {
        assert_eq!(round_trip(b"hello"), b"hello");
    }

    #[test]
    fn an_empty_message_round_trips() {
        assert_eq!(round_trip(b""), b"");
    }

    /// The continuation rule, in both directions: a payload larger than one
    /// packet is split on write and stitched back together on read.
    #[test]
    fn an_oversized_message_round_trips() {
        let payload: Vec<u8> = (0..MAX_PAYLOAD + 1000).map(|i| (i % 251) as u8).collect();
        assert_eq!(round_trip(&payload), payload);
    }

    /// The case the continuation rule is easy to get wrong: an exact multiple
    /// of the maximum needs a trailing empty packet, or the reader waits for
    /// bytes that never come.
    #[test]
    fn a_message_of_exactly_the_maximum_round_trips() {
        let payload: Vec<u8> = (0..MAX_PAYLOAD).map(|i| (i % 251) as u8).collect();
        assert_eq!(round_trip(&payload), payload);
    }

    /// `Bytes_sent` and `Bytes_received` count what crossed the socket,
    /// headers included — and count a continued message once per packet, not
    /// once per message, or a large result set would be reported as a small
    /// one.
    #[test]
    fn traffic_counts_every_packet_header_and_payload() {
        let (read_half, write_half) = loopback();
        let mut stream = Stream::new(read_half, write_half);
        stream.write_message(b"hello").unwrap();
        stream.flush().unwrap();
        assert_eq!(stream.take_traffic(), (0, 9), "4 header + 5 payload");
        // Taken, so the next reading starts from zero.
        assert_eq!(stream.take_traffic(), (0, 0));

        stream.read_message().unwrap().expect("a message");
        assert_eq!(stream.take_traffic(), (9, 0));

        // A message that needs a continuation packet is counted as both.
        let payload: Vec<u8> = (0..MAX_PAYLOAD + 10).map(|i| (i % 251) as u8).collect();
        stream.write_message(&payload).unwrap();
        assert_eq!(
            stream.take_traffic(),
            (0, (MAX_PAYLOAD + 10 + 8) as u64),
            "two packets, so two headers"
        );
    }

    #[test]
    fn a_closed_stream_reads_as_no_message() {
        let (read_half, write_half) = loopback();
        let mut stream = Stream::new(read_half, write_half);
        assert_eq!(stream.read_message().unwrap(), None);
    }

    #[test]
    fn lenenc_ints_round_trip_at_every_width() {
        for value in [
            0u64,
            0xfa,
            0xfb,
            0xff,
            0x1234,
            0xffff,
            0x1_0000,
            0xff_ffff,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            put_lenenc_int(&mut out, value);
            let mut reader = Reader::new(&out);
            assert_eq!(reader.lenenc_int().unwrap(), Some(value), "value {value}");
            assert!(reader.is_empty(), "value {value} left bytes behind");
        }
    }

    #[test]
    fn a_short_packet_is_an_error_rather_than_a_panic() {
        let mut reader = Reader::new(&[0xfc, 0x01]);
        assert_eq!(reader.lenenc_int(), Err(Malformed));
        assert_eq!(Reader::new(&[1, 2]).u32(), Err(Malformed));
        assert_eq!(Reader::new(b"no terminator").nul_str(), Err(Malformed));
    }

    #[test]
    fn a_nul_marker_reads_as_null() {
        let mut reader = Reader::new(&[0xfb]);
        assert_eq!(reader.lenenc_bytes().unwrap(), None);
    }

    #[test]
    fn an_interior_nul_cannot_truncate_a_nul_terminated_field() {
        let mut out = Vec::new();
        put_nul_str(&mut out, "a\0b");
        put_nul_str(&mut out, "after");
        let mut reader = Reader::new(&out);
        assert_eq!(reader.nul_str().unwrap(), "ab");
        assert_eq!(reader.nul_str().unwrap(), "after");
    }
}
