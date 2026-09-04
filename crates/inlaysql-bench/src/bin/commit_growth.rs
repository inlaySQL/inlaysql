//! Where a single-row durable commit's time goes, and whether growing the
//! file is any of it (AHL-553).
//!
//! Two questions, one harness, because they are the same run:
//!
//! 1. **Does preallocating the data area buy anything?** InlaySQL is
//!    copy-on-write: every commit allocates page ids past the end of the file
//!    and the file grows, so the commit's barrier flushes data *and* the
//!    metadata that extends the file. InnoDB and PostgreSQL fsync a
//!    preallocated log rewritten in place. If the growth is what the barrier
//!    is paying for, extending the file well ahead of the writer — outside
//!    the timed phase — is the whole lever. Three arms, interleaved, control
//!    re-run every repetition: `base` (as today), `sparse`
//!    (`File::set_len` only, which on most filesystems leaves a hole whose
//!    first write still allocates), `filled` (`set_len` plus real zero bytes
//!    written over the whole range and synced, which allocates for certain).
//!    Read `PERF.md`'s AHL-553 section for what this measured.
//!
//! 2. **What is the commit made of?** Every commit profile in `PERF.md`
//!    before this one was taken on the macOS host, where `F_FULLFSYNC` is
//!    85-97% of the sample and hides everything behind it. `WRAP=1` puts a
//!    counting [`Device`] between the tree and [`FileDevice`] that
//!    attributes every call by offset — write-ahead log, state block, data
//!    area — with byte counts and nanoseconds, and separates the barrier
//!    ([`Device::sync_commit`]) from everything else. Subtracting the device
//!    total from the wall clock leaves the engine work above the storage
//!    layer. The wrapper is two atomics and a `clock_gettime` per device
//!    call against a commit that costs a millisecond, but it is off by
//!    default anyway so the A/B in question 1 runs on the bare device.
//!
//! # Usage
//!
//! ```sh
//! cargo build --release -p inlaysql-bench --bin commit_growth
//! DIR=/data TXNS=1500 REPS=3 target/release/commit_growth        # the A/B
//! DIR=/data TXNS=1500 REPS=1 WRAP=1 ARMS=base target/release/commit_growth
//! ```
//!
//! Env: `DIR` (where the database file goes — in a container point this at
//! the same volume class `bench/external/compose.yml` gives MySQL and
//! PostgreSQL), `TXNS`, `REPS`, `ARMS` (comma-separated subset of
//! `base,sparse,filled`), `PREALLOC_MB`, `WRAP`, `BATCH` (rows per statement;
//! `1` is the OLTP shape, `100` the batch-insert one).

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use inlaysql::{CommitStats, Database, FileDevice, Value};
use inlaysql_core::btree::device::{AbsorbResult, AbsorbTxn, CommitPoint, PendingOps};
use inlaysql_core::btree::{Device, PageId, DEFAULT_PAGE_SIZE};
use inlaysql_core::{wal, Durability, Result};

/// Every counter the wrapping device keeps, in one place so a snapshot is a
/// single load per field and a report is a subtraction.
#[derive(Default)]
struct Counters {
    wrap_writes: AtomicU64,
    wrap_bytes: AtomicU64,
    wrap_ns: AtomicU64,
    wal_writes: AtomicU64,
    wal_bytes: AtomicU64,
    wal_ns: AtomicU64,
    state_writes: AtomicU64,
    state_bytes: AtomicU64,
    state_ns: AtomicU64,
    data_writes: AtomicU64,
    data_bytes: AtomicU64,
    data_ns: AtomicU64,
    reads: AtomicU64,
    read_ns: AtomicU64,
    syncs: AtomicU64,
    sync_ns: AtomicU64,
    commit_syncs: AtomicU64,
    commit_sync_ns: AtomicU64,
}

impl Counters {
    fn add(&self, field: &AtomicU64, by: u64) {
        field.fetch_add(by, Ordering::Relaxed);
    }
}

/// A [`Device`] that forwards everything to the one underneath and counts
/// what went past, split by where in the file it landed.
///
/// The split is by offset because that is what the file layout already means
/// (`inlaysql_core::wal`): block 0 is the header, block 1 the state block,
/// the next `WAL_REGIONS * WAL_BLOCKS` blocks are the log, and everything
/// after that is the data area. A commit writes the record into its region,
/// the dirty pages into the data area, and — only when the region wraps —
/// the state block.
struct Counting<D: Device> {
    inner: D,
    counters: Arc<Counters>,
    /// `Some` for the `chunked` arm only — see [`Growing`].
    grow: Option<Growing>,
    state_offset: usize,
    data_start: usize,
}

impl<D: Device> Counting<D> {
    fn new(inner: D, counters: Arc<Counters>, grow: Option<Growing>) -> Self {
        Self {
            inner,
            counters,
            grow,
            state_offset: wal::state_offset(DEFAULT_PAGE_SIZE),
            data_start: wal::data_offset_for(
                DEFAULT_PAGE_SIZE,
                wal::MULTI_REGION_FORMAT_VERSION,
                0,
            ),
        }
    }
}

impl<D: Device> Device for Counting<D> {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        let at = Instant::now();
        let out = self.inner.read(offset, buf);
        self.counters.add(&self.counters.reads, 1);
        self.counters
            .add(&self.counters.read_ns, at.elapsed().as_nanos() as u64);
        out
    }

    fn read_shared(&self, offset: usize, len: usize) -> Option<Arc<[u8]>> {
        self.inner.read_shared(offset, len)
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        let at = Instant::now();
        if let Some(grow) = &mut self.grow {
            grow.reserve(offset, data.len())
                .map_err(|err| inlaysql_core::Error::Storage(err.to_string()))?;
        }
        let out = self.inner.write(offset, data);
        let ns = at.elapsed().as_nanos() as u64;
        let counters = &self.counters;
        if offset >= self.data_start {
            counters.add(&counters.data_writes, 1);
            counters.add(&counters.data_bytes, data.len() as u64);
            counters.add(&counters.data_ns, ns);
        } else if offset == self.state_offset {
            counters.add(&counters.state_writes, 1);
            counters.add(&counters.state_bytes, data.len() as u64);
            counters.add(&counters.state_ns, ns);
        } else if data.len() == wal::wal_region_len(DEFAULT_PAGE_SIZE) {
            // The one write that is not a record: `CowBTree::commit` zeroes
            // the whole region before reusing it, so this is the wrap, and
            // folding it into the record bytes would triple them.
            counters.add(&counters.wrap_writes, 1);
            counters.add(&counters.wrap_bytes, data.len() as u64);
            counters.add(&counters.wrap_ns, ns);
        } else {
            counters.add(&counters.wal_writes, 1);
            counters.add(&counters.wal_bytes, data.len() as u64);
            counters.add(&counters.wal_ns, ns);
        }
        out
    }

    fn sync(&mut self) -> Result<()> {
        let at = Instant::now();
        let out = self.inner.sync();
        self.counters.add(&self.counters.syncs, 1);
        self.counters
            .add(&self.counters.sync_ns, at.elapsed().as_nanos() as u64);
        out
    }

    fn sync_commit(&mut self) -> Result<()> {
        let at = Instant::now();
        let out = self.inner.sync_commit();
        self.counters.add(&self.counters.commit_syncs, 1);
        self.counters.add(
            &self.counters.commit_sync_ns,
            at.elapsed().as_nanos() as u64,
        );
        out
    }

    fn commit_ready(&self) {
        self.inner.commit_ready();
    }

    fn set_durability(&self, durability: Durability) {
        self.inner.set_durability(durability);
    }

    fn begin_commit(&self) -> Result<()> {
        self.inner.begin_commit()
    }

    fn begin_normal_commit(&self) -> Result<()> {
        self.inner.begin_normal_commit()
    }

    fn end_commit(&self) -> Option<u64> {
        self.inner.end_commit()
    }

    fn end_normal_commit(&self) -> Option<u64> {
        self.inner.end_normal_commit()
    }

    fn commit_generation(&self) -> Option<u64> {
        self.inner.commit_generation()
    }

    fn commit_point(&self, region: usize) -> Option<CommitPoint> {
        self.inner.commit_point(region)
    }

    fn set_commit_point(&self, region: usize, point: Option<CommitPoint>) {
        self.inner.set_commit_point(region, point);
    }

    fn wal_region(&self) -> usize {
        self.inner.wal_region()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn register_reader(&self) -> Option<u64> {
        self.inner.register_reader()
    }

    fn update_reader(&self, token: u64, seq: u64) {
        self.inner.update_reader(token, seq);
    }

    fn release_reader(&self, token: u64) {
        self.inner.release_reader(token);
    }

    fn min_reader_seq(&self) -> Option<u64> {
        self.inner.min_reader_seq()
    }

    fn note_page_reuse_enabled(&self) {
        self.inner.note_page_reuse_enabled();
    }

    fn page_reuse_enabled(&self) -> bool {
        self.inner.page_reuse_enabled()
    }

    fn set_commit_absorption(&self, enabled: bool) {
        self.inner.set_commit_absorption(enabled);
    }

    fn absorb_offer(&self, root: PageId, ops: &mut PendingOps) -> Option<u64> {
        self.inner.absorb_offer(root, ops)
    }

    fn absorb_wait(&self, token: u64, ops: &mut PendingOps) -> AbsorbResult {
        self.inner.absorb_wait(token, ops)
    }

    fn absorb_take(&self) -> Vec<(u64, AbsorbTxn)> {
        self.inner.absorb_take()
    }

    fn absorb_resolve(&self, results: Vec<(u64, AbsorbResult, PendingOps)>) {
        self.inner.absorb_resolve(results);
    }

    fn absorb_fail_cohort(&self, reason: &'static str) {
        self.inner.absorb_fail_cohort(reason);
    }
}

/// The proposed engine change, prototyped as a [`Device`] so it can be
/// measured before it is written.
///
/// `filled` above preallocates the whole run outside the timed phase, which
/// is an upper bound on what preallocation can be worth, not a design: a real
/// database does not know how big it will get. This one is the design —
/// extend the file by `chunk` bytes of real zeros whenever a write would
/// otherwise land past the end, so an ordinary commit's barrier never has to
/// grow the file, and the growth's own cost is *inside* the timed phase where
/// it belongs. `set_len` alone is not enough (that is the `sparse` arm, and it
/// is flat): a hole is not an extent, and the writer's first write into it
/// allocates exactly as growing the file did.
struct Growing {
    /// A second descriptor on the same file, opened without the advisory lock
    /// [`FileDevice`] holds — this one only ever appends zeros past the end.
    file: std::fs::File,
    /// How far the file has been extended and filled.
    watermark: u64,
    chunk: u64,
    zeros: Vec<u8>,
}

impl Growing {
    fn new(path: &Path, chunk: u64) -> std::io::Result<Self> {
        let file = OpenOptions::new().write(true).open(path)?;
        let watermark = file.metadata()?.len();
        Ok(Self {
            file,
            watermark,
            chunk,
            zeros: vec![0u8; 1 << 20],
        })
    }

    fn reserve(&mut self, offset: usize, len: usize) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let end = (offset + len) as u64;
        if end <= self.watermark {
            return Ok(());
        }
        let target = end.max(self.watermark + self.chunk);
        self.file.set_len(target)?;
        let mut at = self.watermark;
        while at < target {
            let n = self.zeros.len().min((target - at) as usize);
            self.file.write_all_at(&self.zeros[..n], at)?;
            at += n as u64;
        }
        self.watermark = target;
        Ok(())
    }
}

/// What one repetition of one arm measured.
struct Run {
    ops_s: f64,
    p50_ms: f64,
    p99_ms: f64,
    len_before: u64,
    len_after: u64,
    stats: CommitStats,
    counters: Option<Arc<Counters>>,
    commits: u64,
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Grow `path` to `bytes`, and — for the `filled` arm — actually put bytes in
/// the hole `set_len` leaves.
///
/// `set_len` alone is the cheap thing to reach for and may buy nothing: on a
/// filesystem that supports holes it moves `i_size` and allocates no extent,
/// so the writer's first write into the range still allocates one. That is
/// why the two arms are separate rather than one "preallocate" arm — telling
/// them apart *is* part of the result.
fn preallocate(path: &Path, bytes: u64, fill: bool) -> std::io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    // Only ever *past* what the engine has already written. Filling from
    // offset zero would zero the header and the log the schema commit just
    // left there, which is a corrupt file, not a preallocated one.
    let from = file.metadata()?.len();
    if bytes <= from {
        return Ok(());
    }
    file.set_len(bytes)?;
    if fill {
        use std::os::unix::fs::FileExt;
        let chunk = vec![0u8; 1 << 20];
        let mut at = from;
        while at < bytes {
            let len = chunk.len().min((bytes - at) as usize);
            file.write_all_at(&chunk[..len], at)?;
            at += len as u64;
        }
    }
    file.sync_all()
}

fn percentile(sorted: &[Duration], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx].as_secs_f64() * 1000.0
}

/// One arm, one repetition: fresh file, schema, optional preallocation, then
/// the timed loop of durable statements.
fn measure(
    dir: &Path,
    arm: &str,
    txns: usize,
    batch: usize,
    prealloc_bytes: u64,
    chunk_bytes: u64,
    wrap: bool,
) -> std::result::Result<Run, Box<dyn std::error::Error>> {
    let path: PathBuf = dir.join(format!("commit-growth-{arm}.inlay"));
    let _ = fs::remove_file(&path);

    // Schema first, and on its own handle, so the preallocation below happens
    // to a file the engine has already finished creating and no handle holds.
    {
        let mut db = Database::open(&path)?;
        db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
    }
    match arm {
        "base" | "chunked" => {}
        "sparse" => preallocate(&path, prealloc_bytes, false)?,
        "filled" => preallocate(&path, prealloc_bytes, true)?,
        other => return Err(format!("unknown arm {other}").into()),
    }
    let len_before = fs::metadata(&path)?.len();

    let chunked = arm == "chunked";
    let counters = (wrap || chunked).then(|| Arc::new(Counters::default()));
    // The keeper is a second handle on the same file, held only so the
    // coordinator's counters survive `db`'s drop long enough to be read —
    // exactly what `commit_cycle` does, and for the same reason.
    let keeper = FileDevice::open(&path)?;
    let mut db = match &counters {
        Some(counters) => Database::open_on(Counting::new(
            FileDevice::open(&path)?,
            Arc::clone(counters),
            chunked
                .then(|| Growing::new(&path, chunk_bytes))
                .transpose()?,
        ))?,
        None => Database::open(&path)?,
    };

    let payload = "x".repeat(64);
    let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
    for row in 0..batch {
        if row > 0 {
            sql.push_str(", ");
        }
        sql.push_str("(?, ?)");
    }
    let insert = db.prepare(&sql)?;

    let before = keeper.commit_stats().expect("a read-write handle has one");
    let mut samples = Vec::with_capacity(txns);
    let started = Instant::now();
    let mut next_id: i64 = 1;
    let mut args: Vec<Value> = Vec::with_capacity(batch * 2);
    for _ in 0..txns {
        args.clear();
        for _ in 0..batch {
            args.push(Value::Integer(next_id));
            args.push(Value::Text(payload.clone().into()));
            next_id += 1;
        }
        let at = Instant::now();
        db.execute_prepared(&insert, &args)?;
        samples.push(at.elapsed());
    }
    let elapsed = started.elapsed();
    let after = keeper.commit_stats().expect("a read-write handle has one");
    drop(db);
    let len_after = fs::metadata(&path)?.len();
    let _ = fs::remove_file(&path);

    samples.sort_unstable();
    Ok(Run {
        counters: if wrap { counters } else { None },
        ops_s: txns as f64 / elapsed.as_secs_f64(),
        p50_ms: percentile(&samples, 0.50),
        p99_ms: percentile(&samples, 0.99),
        len_before,
        len_after,
        stats: delta(after, before),
        commits: txns as u64,
    })
}

fn delta(a: CommitStats, b: CommitStats) -> CommitStats {
    CommitStats {
        flushes: a.flushes - b.flushes,
        tickets_flushed: a.tickets_flushed - b.tickets_flushed,
        normal_flushes: a.normal_flushes - b.normal_flushes,
        normal_tickets_flushed: a.normal_tickets_flushed - b.normal_tickets_flushed,
        gate_wait_ns: a.gate_wait_ns - b.gate_wait_ns,
        gate_waits: a.gate_waits - b.gate_waits,
        gate_hold_ns: a.gate_hold_ns - b.gate_hold_ns,
        gate_hold_racing_ns: a.gate_hold_racing_ns - b.gate_hold_racing_ns,
        gate_hold_racing_count: a.gate_hold_racing_count - b.gate_hold_racing_count,
        gate_hold_racing_start_ns: a.gate_hold_racing_start_ns - b.gate_hold_racing_start_ns,
        gate_hold_racing_start_count: a.gate_hold_racing_start_count
            - b.gate_hold_racing_start_count,
        follower_wait_ns: a.follower_wait_ns - b.follower_wait_ns,
        follower_waits: a.follower_waits - b.follower_waits,
        gather_spin_ns: a.gather_spin_ns - b.gather_spin_ns,
        fsync_ns: a.fsync_ns - b.fsync_ns,
        post_ns: a.post_ns - b.post_ns,
        gap_ns: a.gap_ns - b.gap_ns,
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir: PathBuf = std::env::var("DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    fs::create_dir_all(&dir)?;
    let txns = env_usize("TXNS", 1500);
    let reps = env_usize("REPS", 3);
    let batch = env_usize("BATCH", 1);
    let prealloc_mb = env_usize("PREALLOC_MB", 256) as u64;
    let chunk_mb = env_usize("CHUNK_MB", 8) as u64;
    let wrap = std::env::var("WRAP").is_ok_and(|v| v != "0");
    let arms: Vec<String> = std::env::var("ARMS")
        .unwrap_or_else(|_| "base,sparse,filled,chunked".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    println!(
        "dir={} txns={txns} reps={reps} batch={batch} prealloc={prealloc_mb}MiB \
         chunk={chunk_mb}MiB wrap={wrap} arms={}",
        dir.display(),
        arms.join(",")
    );
    println!(
        "data area starts at {} bytes; WAL region is {} bytes ({} regions)",
        wal::data_offset_for(DEFAULT_PAGE_SIZE, wal::MULTI_REGION_FORMAT_VERSION, 0),
        wal::wal_region_len(DEFAULT_PAGE_SIZE),
        wal::region_count(wal::MULTI_REGION_FORMAT_VERSION),
    );

    // Shuffled every repetition, not swept in a fixed order. `PERF.md`'s
    // in-container byte sweep manufactured a 32% slope out of nothing by
    // holding the order fixed: whichever arm is last in a round absorbs
    // whatever drifted during it, and on a shared desktop something always
    // drifts. Fisher-Yates over a dependency-free xorshift64*, seeded so the
    // schedule replays.
    let mut rng: u64 = env_usize("SEED", 0x5153_5153) as u64 | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    // One throwaway repetition before anything is recorded. The first run
    // inside a freshly started container pays for a cold page cache and a
    // cold btrfs allocator, and it showed up as a 3x outlier on the first
    // arm of the first round — a position effect the shuffle cannot spread
    // because it only ever lands on whatever runs first.
    let warmup = arms.first().expect("at least one arm").clone();
    measure(
        &dir,
        &warmup,
        txns / 4 + 1,
        batch,
        prealloc_mb << 20,
        chunk_mb << 20,
        wrap,
    )?;

    for rep in 1..=reps {
        let mut order: Vec<&String> = arms.iter().collect();
        for i in (1..order.len()).rev() {
            order.swap(i, (next() % (i as u64 + 1)) as usize);
        }
        for arm in order {
            let run = measure(
                &dir,
                arm,
                txns,
                batch,
                prealloc_mb << 20,
                chunk_mb << 20,
                wrap,
            )?;
            let s = &run.stats;
            println!(
                "rep {rep} arm {arm:<7} {:>9.1} ops/s  p50 {:.3} ms  p99 {:.3} ms  \
                 file {:.1} -> {:.1} MiB  grew {:.1} MiB",
                run.ops_s,
                run.p50_ms,
                run.p99_ms,
                run.len_before as f64 / 1048576.0,
                run.len_after as f64 / 1048576.0,
                (run.len_after.saturating_sub(run.len_before)) as f64 / 1048576.0,
            );
            println!(
                "         barriers: {} total, {} from sync_commit, {} state-block syncs \
                 (one per {:.1} commits); {:.3} ms/barrier in the barrier itself; \
                 gate hold {:.3} ms/commit",
                s.flushes,
                s.normal_flushes,
                s.flushes - s.normal_flushes,
                if s.flushes > s.normal_flushes {
                    run.commits as f64 / (s.flushes - s.normal_flushes) as f64
                } else {
                    f64::INFINITY
                },
                s.fsync_ns as f64 / 1e6 / (s.flushes.max(1)) as f64,
                s.gate_hold_ns as f64 / 1e6 / run.commits as f64,
            );
            if let Some(c) = &run.counters {
                let per = |v: &AtomicU64| v.load(Ordering::Relaxed) as f64 / run.commits as f64;
                let ms =
                    |v: &AtomicU64| v.load(Ordering::Relaxed) as f64 / 1e6 / run.commits as f64;
                println!(
                    "         per statement: WAL record {:.2} writes / {:.0} B / {:.3} ms | \
                     wrap zeroing {:.4} writes / {:.0} B / {:.3} ms | \
                     data {:.2} writes / {:.0} B / {:.3} ms | state {:.3} writes / {:.3} ms | \
                     reads {:.2} / {:.3} ms | sync {:.3} calls / {:.3} ms | \
                     sync_commit {:.3} calls / {:.3} ms",
                    per(&c.wal_writes),
                    per(&c.wal_bytes),
                    ms(&c.wal_ns),
                    per(&c.wrap_writes),
                    per(&c.wrap_bytes),
                    ms(&c.wrap_ns),
                    per(&c.data_writes),
                    per(&c.data_bytes),
                    ms(&c.data_ns),
                    per(&c.state_writes),
                    ms(&c.state_ns),
                    per(&c.reads),
                    ms(&c.read_ns),
                    per(&c.syncs),
                    ms(&c.sync_ns),
                    per(&c.commit_syncs),
                    ms(&c.commit_sync_ns),
                );
                let device_ns = c.wal_ns.load(Ordering::Relaxed)
                    + c.wrap_ns.load(Ordering::Relaxed)
                    + c.data_ns.load(Ordering::Relaxed)
                    + c.state_ns.load(Ordering::Relaxed)
                    + c.read_ns.load(Ordering::Relaxed)
                    + c.sync_ns.load(Ordering::Relaxed)
                    + c.commit_sync_ns.load(Ordering::Relaxed);
                let total_ns = 1e9 / run.ops_s * run.commits as f64;
                println!(
                    "         device {:.3} ms of {:.3} ms per statement — {:.1}% device, \
                     {:.1}% engine above the storage layer",
                    device_ns as f64 / 1e6 / run.commits as f64,
                    total_ns / 1e6 / run.commits as f64,
                    100.0 * device_ns as f64 / total_ns,
                    100.0 * (total_ns - device_ns as f64) / total_ns,
                );
            }
        }
    }
    Ok(())
}
