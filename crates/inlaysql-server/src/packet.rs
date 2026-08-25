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
            let start = payload.len();
            payload.resize(start + length, 0);
            self.reader.read_exact(&mut payload[start..])?;
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
