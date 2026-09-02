//! AHL-537 (R3 brief, B4): a measurement, not an engine change.
//!
//! The `GROUP BY` shape `SELECT n, COUNT(*) FROM users GROUP BY n` (100k rows,
//! 100 groups — `bin/profile --suite aggregate --rows 100000`) is
//! tuple-at-a-time today: leaf -> cell -> `decode_row` -> evaluate -> fold. The
//! question this binary answers before anyone builds a batch executor: how
//! much faster is the same answer when one leaf page is decoded into a column
//! batch (a `Vec<i64>` for `n`, plus a validity bitmap) and the filter/fold run
//! over the batch, versus the row loop — on **today's page format**, with no
//! format change?
//!
//! Four shapes, over the whole table, `--reps` repetitions each, medians
//! reported:
//!
//! * **A** — today's engine path: `db.query_prepared(GROUP BY)`, for reference.
//! * **B** — a hand-written row loop over leaves: `scan_leaf_cells`, decode
//!   `n` per row with [`inlaysql_core::row::decode_value_at`] (the same
//!   column-skipping decoder the engine's own projection pushdown uses), fold
//!   into a `HashMap<i64, u64>` as each row arrives — the row-at-a-time shape,
//!   restated by hand rather than through the query engine's generality.
//! * **C** — batch: per leaf, decode every `n` value into a `Vec<i64>` (plus a
//!   validity bitmap) first, *then* group-count over the vector — measured as
//!   two separate phases (`decode`, `group`) so the split is visible. Grouped
//!   two ways, labelled separately: a `HashMap<i64, u64>` (the general case)
//!   and a direct `[u64; 100]` array (sound only because `n`'s domain is known
//!   to be `0..100` here — this is the shape a real batch executor would only
//!   take with a planner-proven bound).
//! * **D** — C's array grouping extended to fold `COUNT(*)`, `SUM(n)`, `MIN(n)`
//!   and `MAX(n)` per group in the same pass, to show the extra fold cost
//!   separately from the decode cost C already isolated.
//!
//! `--payload` (16, 64, 256 — the `body` column's width) measures how batch
//! decode scales with row width: `body` is always present and always skipped
//! (`skip_value`, reached indirectly through `decode_value_at`) to reach `n`.
//!
//! # Reaching the raw page bytes
//!
//! Every existing walk in `inlaysql_core::btree::CowBTree` hands back rows
//! already resolved into a `RowBuf`, one row at a time — nothing hands out a
//! whole leaf's bytes, which is what a batch decode needs to operate over.
//! `CowBTree::scan_leaves_raw` (`crates/inlaysql-core/src/btree/tree.rs`) is
//! the one minimal shim this prototype needed: it walks internal nodes exactly
//! as the production raw-scan path does and hands each leaf's raw bytes to a
//! callback undecoded, so B/C/D can run `page::scan_leaf_cells` and
//! `row::decode_value_at` over them directly. See its doc comment for why nothing
//! narrower would do the job and why it is not meant to gain a second caller.
//!
//! # Setup vs. measurement
//!
//! Building the table goes through `Database` (`inlaysql::Database`), the same
//! as `profile.rs`'s `aggregate` suite, so shape A is `bin/profile`'s own
//! number. B/C/D need a `CowBTree` handle, which `Database` does not expose
//! (its storage is `Box<dyn Storage>`), so after A is measured the writer
//! handle is dropped and the same file is reopened directly through
//! `inlaysql::TreeStorage` for the raw-leaf shapes. That means A and B/C/D are
//! not literally interleaved instruction-by-instruction on one handle — true
//! interleaving would need reaching further into `Database`'s internals than
//! the brief asked for — but every shape runs its own `--reps` repetitions
//! back to back in one process, on the same file, right after the others, so
//! machine load (noted via `uptime` before and after) applies to all of them
//! about equally.
//!
//! ```sh
//! cargo run --release -p inlaysql-bench --bin batch_proto -- --rows 100000 --payload 64 --reps 20
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use inlaysql::{Database, TreeStorage, Value};
use inlaysql_core::btree::page::scan_leaf_cells;
use inlaysql_core::row::decode_value_at;
use inlaysql_core::storage::table_prefix;

/// Buckets `n` is drawn from — matches `profile.rs`'s `aggregate` suite
/// (`id % 100`) and `sql_shapes.rs`'s opponent schema.
const GROUPS: usize = 100;

/// Column ordinal of `n` in `users (id, email, body, n)`.
const N_ORDINAL: usize = 3;

struct Config {
    rows: usize,
    payload: usize,
    reps: usize,
    seed: u64,
}

impl Config {
    fn from_args() -> Self {
        let mut config = Config {
            rows: 100_000,
            payload: 64,
            reps: 20,
            seed: 42,
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        for pair in args.chunks(2) {
            let [flag, value] = pair else {
                eprintln!("ignoring trailing argument {pair:?}");
                continue;
            };
            match flag.as_str() {
                "--rows" => config.rows = value.parse().unwrap_or(config.rows),
                "--payload" => config.payload = value.parse().unwrap_or(config.payload),
                "--reps" => config.reps = value.parse().unwrap_or(config.reps),
                "--seed" => config.seed = value.parse().unwrap_or(config.seed),
                other => eprintln!("unknown flag {other}"),
            }
        }
        config
    }
}

fn email(id: i64) -> String {
    format!("user{id:012}@example.com")
}

/// A run of `count` durations, reported as a median and a per-row rate.
struct Timing {
    label: &'static str,
    durations: Vec<Duration>,
    rows_per_iteration: usize,
}

impl Timing {
    fn new(label: &'static str, rows_per_iteration: usize) -> Self {
        Self {
            label,
            durations: Vec::new(),
            rows_per_iteration,
        }
    }

    fn push(&mut self, d: Duration) {
        self.durations.push(d);
    }

    fn median(&self) -> Duration {
        let mut sorted = self.durations.clone();
        sorted.sort();
        sorted[sorted.len() / 2]
    }

    fn report(&self) {
        let median = self.median();
        let ns_per_row = median.as_nanos() as f64 / self.rows_per_iteration.max(1) as f64;
        println!(
            "{:<28} median={:>10.2?}  n={:<3} ns/row={:>8.1}",
            self.label,
            median,
            self.durations.len(),
            ns_per_row
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_args();
    let target = Path::new("target");
    std::fs::create_dir_all(target)?;
    let path = target.join(format!("batch-proto-{}.inlay", config.payload));
    let _ = std::fs::remove_file(&path);

    let load_before = uptime();
    println!(
        "batch_proto: rows={} payload={} reps={} pid={}",
        config.rows,
        config.payload,
        config.reps,
        std::process::id()
    );
    println!("load before: {load_before}");

    // --------------------------------------------------------------- setup
    let mut db = Database::open(&path)?;
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, body TEXT, n INTEGER)",
        &[],
    )?;
    let insert = db.prepare("INSERT INTO users (id, email, body, n) VALUES (?, ?, ?, ?)")?;
    let payload = "x".repeat(config.payload);

    db.begin()?;
    for id in 1..=config.rows as i64 {
        let bound = [
            Value::Integer(id),
            Value::Text(email(id).into()),
            Value::Text(payload.clone().into()),
            Value::Integer(id % GROUPS as i64),
        ];
        if let Err(inlaysql::Error::Transaction(_)) = db.execute_prepared(&insert, &bound) {
            db.commit()?;
            db.begin()?;
            db.execute_prepared(&insert, &bound)?;
        }
    }
    db.commit()?;
    db.execute("CREATE INDEX users_email ON users (email) USING BTREE", &[])?;
    db.execute("ANALYZE", &[])?;

    let group = db.prepare("SELECT n, COUNT(*) FROM users GROUP BY n")?;
    db.query_prepared(&group, &[])?; // warm

    // ------------------------------------------------------------- shape A
    let reference = reference_groups(&mut db, &group)?;
    let mut a = Timing::new("A: engine (query_prepared)", config.rows);
    for _ in 0..config.reps {
        let started = Instant::now();
        let result = db.query_prepared(&group, &[])?;
        a.push(started.elapsed());
        assert_eq!(result.rows.len(), GROUPS, "A: wrong group count");
    }
    a.report();

    // Release the write handle's lock before reopening the same file
    // read/write through `TreeStorage` directly — see the module doc for why
    // `Database` cannot hand out its `CowBTree`.
    drop(db);

    let storage = TreeStorage::open_on(inlaysql::FileDevice::open(&path)?)?;
    let tree = storage.tree();
    let start = table_prefix("users");
    // `table_prefix` always ends in the single `0x00` separator byte
    // (`crates/inlaysql-core/src/storage.rs`), so incrementing it is the
    // range's exclusive upper bound: every row key is `"users\x00" + 8 bytes`,
    // which sorts below `"users\x01"` and above nothing else in this schema.
    let mut end = start.clone();
    *end.last_mut().expect("table_prefix is never empty") += 1;

    // ------------------------------------------------------------- shape B
    let mut b = Timing::new("B: row loop (HashMap)", config.rows);
    let mut groups_b: HashMap<i64, u64> = HashMap::new();
    for _ in 0..config.reps {
        groups_b.clear();
        let started = Instant::now();
        tree.scan_leaves_raw(&start, Some(&end), |leaf_bytes| {
            let page_size = leaf_bytes.len();
            scan_leaf_cells(leaf_bytes, page_size, |key, value| {
                if key < start.as_slice() || key >= end.as_slice() {
                    return Ok(());
                }
                let row_bytes = value.inline_bytes(leaf_bytes).expect(
                    "row overflowed a leaf cell; batch_proto does not follow overflow chains",
                );
                let n = decode_value_at(row_bytes, N_ORDINAL)?;
                let Value::Integer(n) = n else {
                    panic!("column n decoded as {n:?}, expected an integer");
                };
                *groups_b.entry(n).or_insert(0) += 1;
                Ok(())
            })
        })?;
        b.push(started.elapsed());
        assert_groups_match("B", &groups_b, &reference);
    }
    b.report();

    // ------------------------------------------------------- shape C decode
    // The batch: every leaf's `n` column decoded into one flat `Vec<i64>`
    // (plus a validity bitmap) before any grouping happens, so the decode
    // cost and the group cost can be timed apart.
    let mut c_decode = Timing::new("C: batch decode (bytes->Vec<i64>)", config.rows);
    let mut batch: Vec<i64> = Vec::with_capacity(config.rows);
    let mut valid: Vec<bool> = Vec::with_capacity(config.rows);
    for _ in 0..config.reps {
        batch.clear();
        valid.clear();
        let started = Instant::now();
        tree.scan_leaves_raw(&start, Some(&end), |leaf_bytes| {
            let page_size = leaf_bytes.len();
            scan_leaf_cells(leaf_bytes, page_size, |key, value| {
                if key < start.as_slice() || key >= end.as_slice() {
                    return Ok(());
                }
                let row_bytes = value.inline_bytes(leaf_bytes).expect(
                    "row overflowed a leaf cell; batch_proto does not follow overflow chains",
                );
                match decode_value_at(row_bytes, N_ORDINAL)? {
                    Value::Integer(n) => {
                        batch.push(n);
                        valid.push(true);
                    }
                    Value::Null => {
                        batch.push(0);
                        valid.push(false);
                    }
                    other => panic!("column n decoded as {other:?}, expected an integer"),
                }
                Ok(())
            })
        })?;
        c_decode.push(started.elapsed());
        assert_eq!(batch.len(), config.rows, "C: decoded row count mismatch");
    }
    c_decode.report();

    // --------------------------------------------------- shape C: group (hash)
    let mut c_group_hash = Timing::new("C: batch group (HashMap)", config.rows);
    let mut groups_c_hash: HashMap<i64, u64> = HashMap::new();
    for _ in 0..config.reps {
        groups_c_hash.clear();
        let started = Instant::now();
        for (&n, &ok) in batch.iter().zip(valid.iter()) {
            if ok {
                *groups_c_hash.entry(n).or_insert(0) += 1;
            }
        }
        c_group_hash.push(started.elapsed());
        assert_groups_match("C (hash)", &groups_c_hash, &reference);
    }
    c_group_hash.report();

    // -------------------------------------------------- shape C: group (array)
    // Sound only because `n`'s domain is known here to be `0..GROUPS` — a
    // real batch executor would only take this path with a planner-proven
    // bound (a `NOT NULL CHECK` range, a dictionary-encoded low-cardinality
    // column), never as the general case.
    let mut c_group_array = Timing::new("C: batch group (array[100])", config.rows);
    let mut groups_c_array;
    for _ in 0..config.reps {
        groups_c_array = [0u64; GROUPS];
        let started = Instant::now();
        for (&n, &ok) in batch.iter().zip(valid.iter()) {
            if ok {
                groups_c_array[n as usize] += 1;
            }
        }
        c_group_array.push(started.elapsed());
        assert_array_groups_match(&groups_c_array, &reference);
    }
    c_group_array.report();

    // ------------------------------------------------------------- shape D
    // C's array grouping, extended to fold COUNT(*)/SUM(n)/MIN(n)/MAX(n) per
    // group in the same pass, to show the fold cost the array-count shape
    // above does not pay.
    #[derive(Clone, Copy)]
    struct Fold {
        count: u64,
        sum: i64,
        min: i64,
        max: i64,
    }
    let empty_fold = Fold {
        count: 0,
        sum: 0,
        min: i64::MAX,
        max: i64::MIN,
    };
    let mut d = Timing::new("D: batch group+fold (array)", config.rows);
    let mut folds;
    for _ in 0..config.reps {
        folds = [empty_fold; GROUPS];
        let started = Instant::now();
        for (&n, &ok) in batch.iter().zip(valid.iter()) {
            if ok {
                let slot = &mut folds[n as usize];
                slot.count += 1;
                slot.sum += n;
                slot.min = slot.min.min(n);
                slot.max = slot.max.max(n);
            }
        }
        d.push(started.elapsed());
        for (n, fold) in folds.iter().enumerate() {
            let expected = *reference.get(&(n as i64)).unwrap_or(&0);
            assert_eq!(fold.count, expected, "D: group {n} count mismatch");
        }
    }
    d.report();

    let load_after = uptime();
    println!("load after:  {load_after}");
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// The reference answer, from today's engine — every other shape's group
/// counts are checked against this.
fn reference_groups(
    db: &mut Database,
    group: &inlaysql::Statement,
) -> Result<HashMap<i64, u64>, Box<dyn std::error::Error>> {
    let result = db.query_prepared(group, &[])?;
    let mut map = HashMap::new();
    for row in result.rows {
        let Value::Integer(n) = row[0] else {
            panic!("GROUP BY column 0 is not an integer: {:?}", row[0]);
        };
        let Value::Integer(count) = row[1] else {
            panic!("COUNT(*) is not an integer: {:?}", row[1]);
        };
        map.insert(n, count as u64);
    }
    Ok(map)
}

fn assert_groups_match(label: &str, actual: &HashMap<i64, u64>, reference: &HashMap<i64, u64>) {
    assert_eq!(
        actual.len(),
        reference.len(),
        "{label}: wrong number of groups"
    );
    for (n, count) in reference {
        assert_eq!(
            actual.get(n),
            Some(count),
            "{label}: group {n} count mismatch"
        );
    }
}

fn assert_array_groups_match(actual: &[u64; GROUPS], reference: &HashMap<i64, u64>) {
    for (n, count) in actual.iter().enumerate() {
        let expected = *reference.get(&(n as i64)).unwrap_or(&0);
        assert_eq!(*count, expected, "group {n} count mismatch");
    }
}

/// `uptime`'s load averages, for noting machine load per run — this binary
/// makes no timing decision based on it, it is reported for the reader.
fn uptime() -> String {
    std::process::Command::new("uptime")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|e| format!("<uptime unavailable: {e}>"))
}
