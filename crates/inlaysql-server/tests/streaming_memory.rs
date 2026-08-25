//! Does one query's answer have to fit in the server?
//!
//! `docs/enterprise-readiness.md` blocker 8: the server used to build every row
//! of a result set in memory before the client could read the first one, so
//! `SELECT * FROM big_table` cost the server the size of `big_table` no matter
//! how the client read it. That is not a slow query, it is a dead process, and
//! a dead process takes every other connection with it.
//!
//! A row count proves nothing about that — the materialising path returns the
//! same rows. What proves it is **peak heap while the query runs**, so this
//! file installs a counting global allocator and measures it.
//!
//! # Why this is its own test binary
//!
//! The allocator is process-wide and `cargo test` runs a file's tests on
//! several threads at once, so any other test allocating at the same time lands
//! in the same number. One test, one binary, one measurement. That is also why
//! the client below is hand-rolled rather than shared with `wire.rs`: pulling
//! that module in would pull in its tests.
//!
//! # What the control is
//!
//! The comparison is not against a recorded number from before the change,
//! which would rot. It is against the materialising path *as it still exists*:
//! a column whose type the plan cannot know before the query runs — here a
//! computed `id + 0` — cannot be described in the column-definition packets
//! that must precede the first row, so that statement is still answered by
//! building the whole result set. Same table, same rows, same bytes on the
//! wire; the only difference is which path the server took. Before streaming
//! existed both queries measured the same, which is exactly what makes this a
//! test rather than a demonstration.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};

use inlaysql_server::{Server, ServerOptions};

// =====================================================================
// the measurement
// =====================================================================

/// Live heap bytes right now.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The high-water mark of [`LIVE`] since [`reset_peak`] was last called.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator with a running total around it.
///
/// Relaxed ordering throughout: the numbers are read once, after the threads
/// that produced them have been joined or have gone quiet, so there is nothing
/// for a stronger ordering to synchronise with and the counter must not become
/// the thing the measurement is measuring.
struct Measured;

unsafe impl GlobalAlloc for Measured {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded unchanged to the allocator this one
        // wraps, which is the same contract this method was called under.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `pointer` came from `alloc` above with this same `layout`,
        // which is what this method's own contract already requires.
        unsafe { System.dealloc(pointer, layout) }
    }
}

// `realloc` and `alloc_zeroed` are deliberately not overridden: their default
// implementations are written in terms of `alloc` and `dealloc` above, so they
// are already accounted for, and reimplementing them would only add a way for
// the accounting to disagree with itself.
#[global_allocator]
static ALLOCATOR: Measured = Measured;

/// Start counting from wherever the heap is now.
fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// The high-water mark since [`reset_peak`], less the level it started at.
fn peak_growth() -> usize {
    PEAK.load(Ordering::Relaxed)
        .saturating_sub(LIVE.load(Ordering::Relaxed))
}

// =====================================================================
// the smallest client that can ask a question
// =====================================================================

/// A client that authenticates with an empty password — the one case
/// `mysql_native_password` completes with an empty token, so nothing here
/// needs SHA-1 — and reads rows without keeping them.
struct Client {
    stream: TcpStream,
    sequence: u8,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("tcp connect");
        stream.set_nodelay(true).ok();
        let mut client = Self {
            stream,
            sequence: 0,
        };
        let greeting = client.read_packet().expect("handshake");
        assert_eq!(greeting.first(), Some(&10), "expected a v10 handshake");

        // CLIENT_LONG_PASSWORD | CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION
        // | CLIENT_PLUGIN_AUTH.
        let capabilities: u32 = 0x0000_0001 | 0x0000_0200 | 0x0000_8000 | 0x0008_0000;
        let mut payload = capabilities.to_le_bytes().to_vec();
        payload.extend_from_slice(&(16u32 * 1024 * 1024).to_le_bytes());
        payload.push(45); // utf8mb4
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(b"root\0");
        payload.push(0); // an empty auth response
        payload.extend_from_slice(b"mysql_native_password\0");
        client.write_packet(&payload);

        let reply = client.read_packet().expect("auth reply");
        assert_ne!(reply.first(), Some(&0xff), "authentication was refused");
        client
    }

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

    fn write_packet(&mut self, payload: &[u8]) {
        let mut header = [0u8; 4];
        header[..3].copy_from_slice(&(payload.len() as u32).to_le_bytes()[..3]);
        header[3] = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.write_all(&header).expect("header");
        self.stream.write_all(payload).expect("payload");
        self.stream.flush().expect("flush");
    }

    /// Run a statement that returns no rows.
    fn exec(&mut self, sql: &str) {
        self.sequence = 0;
        let mut payload = vec![0x03];
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload);
        let reply = self.read_packet().expect("reply");
        assert_eq!(
            reply.first(),
            Some(&0x00),
            "{sql} failed: {}",
            String::from_utf8_lossy(&reply)
        );
    }

    /// Run a query and count its rows without retaining any of them.
    ///
    /// Retaining them would put the answer back in memory on this side of the
    /// socket and measure nothing, since the allocator counts the whole
    /// process — the server runs on a thread of it.
    fn count_rows(&mut self, sql: &str) -> usize {
        self.sequence = 0;
        let mut payload = vec![0x03];
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload);

        let first = self.read_packet().expect("column count");
        assert!(
            first.first() != Some(&0xff),
            "{sql} failed: {}",
            String::from_utf8_lossy(&first)
        );
        let columns = first[0] as usize;
        for _ in 0..columns {
            self.read_packet().expect("column definition");
        }
        let eof = self.read_packet().expect("metadata EOF");
        assert_eq!(eof.first(), Some(&0xfe), "expected EOF after column defs");

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

/// A database file that removes itself when the test ends.
struct TempDb {
    path: std::path::PathBuf,
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// =====================================================================
// the test
// =====================================================================

/// The number of rows the "large" measurement returns. The "small" one returns
/// a quarter of them, which is what turns a peak into a slope.
const ROWS: usize = 40_000;

#[test]
fn a_streamed_result_set_costs_the_server_the_same_whatever_its_size() {
    let path = std::env::temp_dir().join(format!(
        "inlaysql-streaming-memory-{}.inlay",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let temp = TempDb { path };

    let options = ServerOptions {
        bind: "127.0.0.1".to_string(),
        port: 0,
        user: "root".to_string(),
        password: String::new(),
        ..ServerOptions::default()
    };
    let server = Server::bind(&temp.path, &options).expect("bind");
    let addr = server.local_addr().expect("local_addr");
    std::thread::spawn(move || {
        let _ = server.run();
    });

    let mut client = Client::connect(addr);
    client.exec("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)");
    for start in (1..=ROWS).step_by(500) {
        let end = (start + 499).min(ROWS);
        let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
        for id in start..=end {
            if id > start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({id}, 'row-{id}-padding-padding-padding')"));
        }
        client.exec(&sql);
    }

    // Both columns of this one have a declared type the engine enforces, so
    // the column-definition packets can be written before the first row and
    // the rest of the answer never has to exist all at once.
    let streamed = "SELECT id, body FROM kv";
    // `id + 0` is a computed column. Its type is whatever the values turn out
    // to be, which is not knowable until they exist, so this statement is
    // still answered by materialising it — the control.
    let materialised = "SELECT id + 0 AS id, body FROM kv";
    let quarter = format!(" LIMIT {}", ROWS / 4);

    // Warm first: the page cache, the catalog and every buffer that is sized
    // once and reused are resident memory the *first* query pays for and no
    // later one does, and none of it is the answer.
    for sql in [streamed, materialised] {
        assert_eq!(client.count_rows(sql), ROWS);
    }

    let measure = |client: &mut Client, sql: &str, expected: usize| {
        reset_peak();
        assert_eq!(client.count_rows(sql), expected, "{sql}");
        peak_growth()
    };

    let streamed_small = measure(&mut client, &format!("{streamed}{quarter}"), ROWS / 4);
    let streamed_large = measure(&mut client, streamed, ROWS);
    let materialised_small = measure(&mut client, &format!("{materialised}{quarter}"), ROWS / 4);
    let materialised_large = measure(&mut client, materialised, ROWS);

    println!(
        "streamed: {streamed_small} -> {streamed_large} bytes; \
         materialised: {materialised_small} -> {materialised_large} bytes"
    );

    // The property. Four times the rows must not be four times the memory: a
    // streamed result set holds one row and one write buffer, whichever end of
    // the table it is at.
    assert!(
        streamed_large < streamed_small * 2 + 64 * 1024,
        "streaming {} rows cost {streamed_large} bytes against {streamed_small} for {}: \
         peak memory is still following the size of the answer",
        ROWS,
        ROWS / 4
    );

    // The control does grow, which is what says the measurement can see growth
    // at all — without this, a broken meter reading zero would pass above.
    assert!(
        materialised_large > materialised_small * 2,
        "the materialising control did not grow with its answer \
         ({materialised_small} -> {materialised_large} bytes), so this file is \
         measuring nothing"
    );

    // And the two paths are worlds apart on the same rows. This is the
    // assertion that fails before the change, when there was only one path.
    assert!(
        materialised_large > streamed_large * 20,
        "the same {ROWS} rows cost {streamed_large} bytes streamed and \
         {materialised_large} bytes materialised; the streamed path is not \
         streaming"
    );
}
