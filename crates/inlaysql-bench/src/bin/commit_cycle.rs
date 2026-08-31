//! The library commit cycle, decomposed (PERF.md's commit-cycle
//! instrumentation task).
//!
//! Drives the in-process concurrent-writer workload — no server, no socket —
//! and reads the coordinator's per-segment timers from
//! [`FileDevice::commit_stats`] after every repetition, while the process and
//! its coordinator are still alive.
//!
//! Method, fixed before any result was seen:
//!
//! * Writer levels `WRITER_LEVELS` (default `1,4,16`), `REPS` repetitions
//!   each (default 5), `TXNS` single-row INSERT transactions per writer per
//!   repetition (default 1000).
//! * The full schedule of (level, repetition) pairs is shuffled with a
//!   seeded Fisher-Yates before anything runs, so no level is systematically
//!   first or last in wall-clock time — the same ordering/time-confound the
//!   byte-sweep in `PERF.md` fell into.
//! * Every repetition uses a fresh database file, and reads a stats snapshot
//!   immediately after schema creation so the CREATE TABLE's own commit is
//!   not counted.
//! * Every repetition verifies the committed row count through a fresh
//!   handle, the same lost-write check the concurrency suite runs.
//!
//! Env: `WRITER_LEVELS`, `REPS`, `TXNS`, `DIR` (where the database files go;
//! default a temporary directory — in a container, point this at the same
//! volume class the earlier barrier-rate measurements used).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use inlaysql::{Database, EngineOptions, Error, FileDevice, Value};

#[derive(Debug, Clone, Copy, Default)]
struct Segments {
    flushes: u64,
    normal_flushes: u64,
    tickets: u64,
    normal_tickets: u64,
    gate_wait_ns: u64,
    gate_waits: u64,
    gate_hold_ns: u64,
    gate_hold_racing_ns: u64,
    gate_hold_racing_count: u64,
    gate_hold_racing_start_ns: u64,
    gate_hold_racing_start_count: u64,
    follower_wait_ns: u64,
    follower_waits: u64,
    gather_spin_ns: u64,
    fsync_ns: u64,
    post_ns: u64,
    gap_ns: u64,
}

fn segments(stats: Option<inlaysql::CommitStats>) -> Segments {
    let s = stats.expect("a read-write handle always has a coordinator");
    Segments {
        flushes: s.flushes,
        normal_flushes: s.normal_flushes,
        tickets: s.tickets_flushed,
        normal_tickets: s.normal_tickets_flushed,
        gate_wait_ns: s.gate_wait_ns,
        gate_waits: s.gate_waits,
        gate_hold_ns: s.gate_hold_ns,
        gate_hold_racing_ns: s.gate_hold_racing_ns,
        gate_hold_racing_count: s.gate_hold_racing_count,
        gate_hold_racing_start_ns: s.gate_hold_racing_start_ns,
        gate_hold_racing_start_count: s.gate_hold_racing_start_count,
        follower_wait_ns: s.follower_wait_ns,
        follower_waits: s.follower_waits,
        gather_spin_ns: s.gather_spin_ns,
        fsync_ns: s.fsync_ns,
        post_ns: s.post_ns,
        gap_ns: s.gap_ns,
    }
}

fn sub(a: Segments, b: Segments) -> Segments {
    Segments {
        flushes: a.flushes - b.flushes,
        normal_flushes: a.normal_flushes - b.normal_flushes,
        tickets: a.tickets - b.tickets,
        normal_tickets: a.normal_tickets - b.normal_tickets,
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

type WriterResult = Result<(usize, usize, Vec<Duration>), Error>;

struct Rep {
    writers: usize,
    committed: usize,
    conflicts: usize,
    elapsed: Duration,
    latency_mean: Duration,
    latency_p50: Duration,
    seg: Segments,
}

fn open_db(path: &Path) -> Result<Database, inlaysql::Error> {
    Database::open_on_with_options(
        FileDevice::open(path)?,
        EngineOptions {
            ..EngineOptions::default()
        },
    )
}

fn run_rep(path: &Path, writers: usize, txns: usize) -> Result<Rep, Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    // A long-lived keeper handle on the same file, the pattern inlaysql-server
    // uses for `SHOW GLOBAL STATUS` (its `lib.rs` "keeper"): every read-write
    // handle on the path shares one coordinator, so the keeper's snapshot is
    // the whole file's, and it can be read after the writers finish while the
    // coordinator is still alive.
    let keeper = FileDevice::open(path)?;
    let mut creator = open_db(path)?;
    creator.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, n INTEGER)", &[])?;
    let baseline = segments(keeper.commit_stats());
    drop(creator);

    let ready = Arc::new(Barrier::new(writers + 1));
    let start = Arc::new(Barrier::new(writers + 1));
    let (results, elapsed): (Vec<WriterResult>, Duration) = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for index in 0..writers {
            let path = path.to_path_buf();
            let ready = ready.clone();
            let start = start.clone();
            handles.push(
                scope.spawn(move || -> Result<(usize, usize, Vec<Duration>), Error> {
                    let mut db = open_db(&path)?;
                    ready.wait();
                    start.wait();
                    let mut committed = 0usize;
                    let mut conflicts = 0usize;
                    let mut samples = Vec::with_capacity(txns);
                    for round in 0..txns {
                        let id = (round * writers + index + 1) as i64;
                        loop {
                            let attempted = Instant::now();
                            match db.execute(
                                "INSERT INTO kv (id, n) VALUES (?, ?)",
                                &[Value::Integer(id), Value::Integer(id)],
                            ) {
                                Ok(_) => {
                                    samples.push(attempted.elapsed());
                                    committed += 1;
                                    break;
                                }
                                Err(Error::Conflict) => conflicts += 1,
                                Err(other) => return Err(other),
                            }
                        }
                    }
                    Ok((committed, conflicts, samples))
                }),
            );
        }
        ready.wait();
        let started = Instant::now();
        start.wait();
        let results: Vec<WriterResult> = handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect();
        (results, started.elapsed())
    });

    let mut committed = 0usize;
    let mut conflicts = 0usize;
    let mut samples: Vec<Duration> = Vec::new();
    for r in results {
        let (c, f, s) = r?;
        committed += c;
        conflicts += f;
        samples.extend(s);
    }
    samples.sort();

    // Stats read from the keeper while the coordinator is still alive: the
    // writers' scopes have joined, but the counters live on the shared
    // coordinator, not on any writer's handle.
    let seg = sub(segments(keeper.commit_stats()), baseline);
    drop(keeper);

    // Lost-write check through a fresh handle, exactly like the concurrency
    // suite: an open writer answers from its own snapshot and would count
    // other writers' commits as lost.
    let mut verify = Database::open(path)?;
    let rows = verify.query("SELECT id FROM kv", &[])?.rows.len();
    if rows != committed {
        return Err(format!(
            "lost writes: {committed} transactions committed, {rows} rows in the file"
        )
        .into());
    }
    drop(verify);
    let _ = std::fs::remove_file(path);

    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    let p50 = samples[samples.len() / 2];
    Ok(Rep {
        writers,
        committed,
        conflicts,
        elapsed,
        latency_mean: mean,
        latency_p50: p50,
        seg,
    })
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if values.len() % 2 == 1 {
        values[values.len() / 2]
    } else {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let process_start = Instant::now();
    let levels: Vec<usize> = std::env::var("WRITER_LEVELS")
        .unwrap_or_else(|_| "1,4,16".to_string())
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    let reps: usize = std::env::var("REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let txns: usize = std::env::var("TXNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let dir: PathBuf = match std::env::var("DIR") {
        Ok(d) => d.into(),
        Err(_) => std::env::temp_dir().join("inlaysql-commit-cycle"),
    };
    std::fs::create_dir_all(&dir)?;

    // Seeded Fisher-Yates over the whole (level, rep) schedule: no level is
    // systematically run first/last, so machine drift cannot masquerade as a
    // writer-count effect (the ordering confound PERF.md's byte sweep hit).
    let mut schedule: Vec<(usize, usize)> = (0..reps)
        .flat_map(|rep| levels.iter().map(move |&w| (w, rep)))
        .collect();
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for i in (1..schedule.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        schedule.swap(i, j);
    }
    let seed = 0x2545_F491_4F6C_DD1Du64;
    println!(
        "commit-cycle: levels {levels:?}, reps {reps}, txns/writer {txns}, dir {}",
        dir.display()
    );
    println!("schedule order: {schedule:?} (seed {seed:#x})");

    let mut results: Vec<Rep> = Vec::new();
    for (i, &(writers, _)) in schedule.iter().enumerate() {
        let path = dir.join(format!("cycle-w{writers}.inlay"));
        let rep = run_rep(&path, writers, txns)?;
        println!(
            "[{}/{}] w={:>2} commits={:>6} conflicts={:>4} elapsed={:>8.2?} \
             commits/s={:>7.0} p50={:>7.2?} flushes={:>5}/{:>5} c/fsync={:>5.2}",
            i + 1,
            schedule.len(),
            rep.writers,
            rep.committed,
            rep.conflicts,
            rep.elapsed,
            rep.committed as f64 / rep.elapsed.as_secs_f64(),
            rep.latency_p50,
            rep.seg.flushes,
            rep.seg.normal_flushes,
            rep.seg.normal_tickets as f64 / rep.seg.normal_flushes.max(1) as f64,
        );
        println!(
            "         gate_hold_racing(start)={:.0}us over {} holds ({:.0}% of commits);              racing(end)={:.0}us over {} holds; gate_busy={:.0}% ({} parallelism)",
            rep.seg.gate_hold_racing_start_ns as f64 / 1e3
                / rep.seg.gate_hold_racing_start_count.max(1) as f64,
            rep.seg.gate_hold_racing_start_count,
            100.0 * rep.seg.gate_hold_racing_start_count as f64 / rep.committed.max(1) as f64,
            rep.seg.gate_hold_racing_ns as f64 / 1e3
                / rep.seg.gate_hold_racing_count.max(1) as f64,
            rep.seg.gate_hold_racing_count,
            100.0
                * rep.committed as f64
                * (rep.seg.gate_hold_ns as f64 / rep.committed.max(1) as f64 / 1e9)
                / rep.elapsed.as_secs_f64(),
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        );
        let _ = process_start;
        results.push(rep);
    }

    for &w in &levels {
        let rs: Vec<&Rep> = results.iter().filter(|r| r.writers == w).collect();
        println!("\n=== writers = {w} (per-repetition, then medians) ===");
        println!(
            "{:>4} {:>10} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
            "rep",
            "cycle_us",
            "fsync_us",
            "gather_us",
            "post_us",
            "gap_us",
            "gatew_us",
            "gateh_us",
            "race_us",
            "follow_us",
            "c/fsync",
            "fsync/s"
        );
        for (i, r) in rs.iter().enumerate() {
            let nf = r.seg.normal_flushes.max(1) as f64;
            println!(
                "{:>4} {:>10.0} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>9.2} {:>9.0}",
                i,
                r.elapsed.as_secs_f64() * 1e6 / nf,
                r.seg.fsync_ns as f64 / 1e3 / nf,
                r.seg.gather_spin_ns as f64 / 1e3 / nf,
                r.seg.post_ns as f64 / 1e3 / nf,
                r.seg.gap_ns as f64 / 1e3 / nf,
                r.seg.gate_wait_ns as f64 / 1e3 / r.seg.gate_waits.max(1) as f64,
                r.seg.gate_hold_ns as f64 / 1e3 / r.committed.max(1) as f64,
                r.seg.gate_hold_racing_ns as f64 / 1e3
                    / r.seg.gate_hold_racing_count.max(1) as f64,
                r.seg.follower_wait_ns as f64 / 1e3 / r.seg.follower_waits.max(1) as f64,
                r.seg.normal_tickets as f64 / nf,
                r.seg.normal_flushes as f64 / r.elapsed.as_secs_f64(),
            );
        }
        let per_cycle = |pick: fn(&Segments) -> u64, denom: fn(&Rep) -> f64| -> f64 {
            median(
                &mut rs
                    .iter()
                    .map(|r| pick(&r.seg) as f64 / 1e3 / denom(r))
                    .collect::<Vec<_>>(),
            )
        };
        let flushes = |r: &Rep| r.seg.normal_flushes.max(1) as f64;
        let cycles: Vec<f64> = rs
            .iter()
            .map(|r| r.elapsed.as_secs_f64() * 1e6 / flushes(r))
            .collect();
        let cfs: Vec<f64> = rs
            .iter()
            .map(|r| r.seg.normal_tickets as f64 / flushes(r))
            .collect();
        let rates: Vec<f64> = rs
            .iter()
            .map(|r| r.seg.normal_flushes as f64 / r.elapsed.as_secs_f64())
            .collect();
        let gateh: Vec<f64> = rs
            .iter()
            .map(|r| r.seg.gate_hold_ns as f64 / 1e3 / r.committed.max(1) as f64)
            .collect();
        let gatew: Vec<f64> = rs
            .iter()
            .map(|r| r.seg.gate_wait_ns as f64 / 1e3 / r.seg.gate_waits.max(1) as f64)
            .collect();
        let follow: Vec<f64> = rs
            .iter()
            .map(|r| r.seg.follower_wait_ns as f64 / 1e3 / r.seg.follower_waits.max(1) as f64)
            .collect();
        let lat: Vec<f64> = rs
            .iter()
            .map(|r| r.latency_mean.as_secs_f64() * 1e6)
            .collect();
        let commit_rate: Vec<f64> = rs
            .iter()
            .map(|r| r.committed as f64 / r.elapsed.as_secs_f64())
            .collect();
        println!("MEDIANS w={w}: cycle={:.0}us fsync={:.0}us gather={:.0}us post={:.0}us gap={:.0}us gate_wait={:.0}us gate_hold={:.0}us follower_wait={:.0}us c/fsync={:.2} fsync/s={:.0} commits/s={:.0} latency_mean={:.0}us",
            median(&mut cycles.clone()),
            per_cycle(|s| s.fsync_ns, flushes),
            per_cycle(|s| s.gather_spin_ns, flushes),
            per_cycle(|s| s.post_ns, flushes),
            per_cycle(|s| s.gap_ns, flushes),
            median(&mut gatew.clone()),
            median(&mut gateh.clone()),
            median(&mut follow.clone()),
            median(&mut cfs.clone()),
            median(&mut rates.clone()),
            median(&mut commit_rate.clone()),
            median(&mut lat.clone()),
        );
        // The residual identity: measured cycle time minus the three
        // in-cycle coordinator segments should agree with the gap segment
        // (double-entry bookkeeping for the cycle decomposition).
        let residual: Vec<f64> = rs
            .iter()
            .map(|r| {
                let nf = flushes(r);
                (r.elapsed.as_secs_f64() * 1e9
                    - (r.seg.fsync_ns + r.seg.gather_spin_ns + r.seg.post_ns) as f64)
                    / 1e3
                    / nf
            })
            .collect();
        println!(
            "CHECK w={w}: cycle={:.0}us sum(fsync+gather+post)={:.0}us gap_counter={:.0}us residual={:.0}us",
            median(&mut cycles.clone()),
            per_cycle(|s| s.fsync_ns, flushes)
                + per_cycle(|s| s.gather_spin_ns, flushes)
                + per_cycle(|s| s.post_ns, flushes),
            per_cycle(|s| s.gap_ns, flushes),
            median(&mut residual.clone()),
        );
    }
    Ok(())
}
