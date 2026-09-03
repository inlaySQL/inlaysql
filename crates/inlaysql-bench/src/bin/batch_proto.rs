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
//!
//! # `--cells`: the leaf cell walk on its own (AHL-541)
//!
//! `docs/research/leaf-offset-table.md` asked whether a per-leaf cell offset
//! table would make cell iteration cheaper. The page already has one — the
//! slot directory at `HEADER_SIZE` — so the question reduces to how much of
//! `scan_leaf_cells`'s per-cell cost is the *decoder* rather than the
//! layout. `--cells` runs the same leaves through four walks, `--reps` times
//! each, interleaved, and reports ns/cell:
//!
//! * **E0** — today's [`scan_leaf_cells`], with a callback that reads the
//!   trailing row id and the inline value's length.
//! * **E1** — the same slot-directory layout, walked by a decoder written as
//!   tight as the layout allows: the header checked once per leaf, then per
//!   cell one slot read, one `get` per length field, no `Result`-returning
//!   helpers. Same callback. If E1 is not clearly below E0, the format has
//!   nothing left to give the walk; if it is, the gap is decoder overhead
//!   and needs no format change to collect.
//! * **F0** — today's [`leaf_edge_keys`] per leaf (what `admits_whole_leaf`
//!   pays), reported per *leaf*.
//! * **F1** — the same two edge keys read without decoding the values.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant};

use inlaysql::{Database, TreeStorage, Value};
use inlaysql_core::btree::page::{leaf_edge_keys, scan_leaf_cells};
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
    /// Run only the cell-walk shapes (E0/E1/F0/F1); see the module doc.
    cells: bool,
}

impl Config {
    fn from_args() -> Self {
        let mut config = Config {
            rows: 100_000,
            payload: 64,
            reps: 20,
            seed: 42,
            cells: false,
        };
        let args: Vec<String> = std::env::args()
            .skip(1)
            .filter(|arg| {
                if arg == "--cells" {
                    config.cells = true;
                    false
                } else {
                    true
                }
            })
            .collect();
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
        "batch_proto: rows={} payload={} reps={} cells={} pid={}",
        config.rows,
        config.payload,
        config.reps,
        config.cells,
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

    if config.cells {
        drop(db);
        run_cells(&config, &path)?;
        println!("load after:  {}", uptime());
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }

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

/// The `--cells` shapes: every leaf of `users` collected once, then E0/E1
/// and F0/F1 run over the same `Vec<Arc<[u8]>>` interleaved, so the fetch and
/// the tree walk are out of the number entirely and only the cell walk is
/// timed.
fn run_cells(config: &Config, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let storage = TreeStorage::open_on(inlaysql::FileDevice::open(path)?)?;
    let tree = storage.tree();
    let start = table_prefix("users");
    let mut end = start.clone();
    *end.last_mut().expect("table_prefix is never empty") += 1;

    let mut leaves: Vec<std::sync::Arc<[u8]>> = Vec::new();
    tree.scan_leaves_raw(&start, Some(&end), |leaf| {
        leaves.push(std::sync::Arc::clone(leaf));
        Ok(())
    })?;
    let cells: usize = leaves
        .iter()
        .map(|leaf| u16::from_le_bytes([leaf[2], leaf[3]]) as usize)
        .sum();
    println!(
        "leaves={} cells={} cells/leaf={:.1} page={}",
        leaves.len(),
        cells,
        cells as f64 / leaves.len().max(1) as f64,
        leaves.first().map_or(0, |l| l.len())
    );

    // The reference: what E0 computes, so E1 is checked to agree cell for
    // cell on every repetition.
    let reference = walk_today(&leaves)?;

    let mut e0 = Timing::new("E0: scan_leaf_cells (today)", cells);
    let mut e1 = Timing::new("E1: tight walk, same layout", cells);
    let mut f0 = Timing::new("F0: leaf_edge_keys (today)", leaves.len());
    let mut f1 = Timing::new("F1: edge keys, key-only", leaves.len());
    for rep in 0..config.reps {
        // Alternate the order each repetition so neither shape always runs
        // with the leaves warm from the other.
        let order: [u8; 2] = if rep % 2 == 0 { [0, 1] } else { [1, 0] };
        for shape in order {
            if shape == 0 {
                let started = Instant::now();
                let got = walk_today(&leaves)?;
                e0.push(started.elapsed());
                assert_eq!(got, reference, "E0 drifted from itself");
            } else {
                let started = Instant::now();
                let got = walk_tight(&leaves)?;
                e1.push(started.elapsed());
                assert_eq!(got, reference, "E1 disagrees with E0");
            }
        }
        let started = Instant::now();
        let mut acc = 0u64;
        for leaf in &leaves {
            if let Some((first, last)) = leaf_edge_keys(leaf, leaf.len())? {
                acc = acc
                    .wrapping_add(row_id_of(first))
                    .wrapping_add(row_id_of(last));
            }
        }
        f0.push(started.elapsed());
        let f0_acc = acc;
        let started = Instant::now();
        let mut acc = 0u64;
        for leaf in &leaves {
            if let Some((first, last)) = edge_keys_only(leaf)? {
                acc = acc
                    .wrapping_add(row_id_of(first))
                    .wrapping_add(row_id_of(last));
            }
        }
        f1.push(started.elapsed());
        assert_eq!(acc, f0_acc, "F1 disagrees with F0");
    }
    e0.report();
    e1.report();
    f0.report();
    f1.report();
    Ok(())
}

/// The row id a table key ends in — the same eight big-endian bytes
/// `CowBTree`'s scan reads per cell (`trailing_row_id`).
fn row_id_of(key: &[u8]) -> u64 {
    let tail = &key[key.len() - 8..];
    u64::from_be_bytes(tail.try_into().expect("eight bytes"))
}

/// What both walks fold per cell: the row id and the inline value length,
/// which is what the streamed aggregate's scan needs from a cell before it
/// hands the row on.
fn fold_cell(acc: &mut (u64, u64, u64), key: &[u8], value: Range<usize>) {
    acc.0 = acc.0.wrapping_add(row_id_of(key));
    acc.1 = acc.1.wrapping_add(value.len() as u64);
    acc.2 += 1;
}

/// E0: today's decoder.
fn walk_today(leaves: &[std::sync::Arc<[u8]>]) -> Result<(u64, u64, u64), inlaysql::Error> {
    let mut acc = (0u64, 0u64, 0u64);
    for leaf in leaves {
        scan_leaf_cells(leaf, leaf.len(), |key, value| {
            match value {
                inlaysql_core::btree::page::ValueRef::Inline(range) => {
                    fold_cell(&mut acc, key, range);
                }
                other => panic!("batch_proto does not follow overflow chains: {other:?}"),
            }
            Ok(())
        })?;
    }
    Ok(acc)
}

/// E1: the same slot-directory layout, walked as tightly as it allows. Every
/// check today's decoder makes is still made — a slot, key or value running
/// past the page is refused — but as a `get` on the slice rather than a call
/// into a `Result`-returning helper per field.
fn walk_tight(leaves: &[std::sync::Arc<[u8]>]) -> Result<(u64, u64, u64), inlaysql::Error> {
    let mut acc = (0u64, 0u64, 0u64);
    for leaf in leaves {
        tight_leaf_cells(leaf, |key, value| fold_cell(&mut acc, key, value))?;
    }
    Ok(acc)
}

fn corrupt(what: &str) -> inlaysql::Error {
    inlaysql::Error::Corrupt(what.to_string())
}

/// Layout constants, copied from `btree/page.rs` (they are private there and
/// this binary is a measurement, not a second reader of the format).
const HEADER_SIZE: usize = 16;
const OFF_CELL_COUNT: usize = 2;
const OFF_FREE_START: usize = 4;

fn tight_leaf_cells(
    bytes: &[u8],
    mut f: impl FnMut(&[u8], Range<usize>),
) -> Result<(), inlaysql::Error> {
    let page_size = bytes.len();
    let count = u16::from_le_bytes([bytes[OFF_CELL_COUNT], bytes[OFF_CELL_COUNT + 1]]) as usize;
    let free_start =
        u16::from_le_bytes([bytes[OFF_FREE_START], bytes[OFF_FREE_START + 1]]) as usize;
    if free_start > page_size || HEADER_SIZE + 2 * count > free_start {
        return Err(corrupt("slot directory overlaps cell area"));
    }
    let slots = &bytes[HEADER_SIZE..HEADER_SIZE + 2 * count];
    for slot in slots.as_chunks::<2>().0 {
        let slot = u16::from_le_bytes([slot[0], slot[1]]) as usize;
        let Some(head) = bytes.get(slot..slot + 2) else {
            return Err(corrupt("leaf cell runs past end of page"));
        };
        let key_len = u16::from_le_bytes([head[0], head[1]]) as usize;
        let key_end = slot + 2 + key_len;
        let Some(key) = bytes.get(slot + 2..key_end) else {
            return Err(corrupt("leaf key runs past end of page"));
        };
        match bytes.get(key_end) {
            Some(0) => {
                let Some(len) = bytes.get(key_end + 1..key_end + 5) else {
                    return Err(corrupt("leaf value length runs past end of page"));
                };
                let value_len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
                let value_end = key_end + 5 + value_len;
                if value_end > page_size {
                    return Err(corrupt("leaf value runs past end of page"));
                }
                f(key, key_end + 5..value_end);
            }
            Some(1) => panic!("batch_proto does not follow overflow chains"),
            _ => return Err(corrupt("unknown leaf value tag")),
        }
    }
    Ok(())
}

/// The two edge keys of a leaf, borrowed from it; `None` for an empty leaf.
type EdgeKeys<'a> = Option<(&'a [u8], &'a [u8])>;

/// F1: the first and last keys, read without decoding either cell's value.
fn edge_keys_only(bytes: &[u8]) -> Result<EdgeKeys<'_>, inlaysql::Error> {
    let page_size = bytes.len();
    let count = u16::from_le_bytes([bytes[OFF_CELL_COUNT], bytes[OFF_CELL_COUNT + 1]]) as usize;
    let free_start =
        u16::from_le_bytes([bytes[OFF_FREE_START], bytes[OFF_FREE_START + 1]]) as usize;
    if free_start > page_size || HEADER_SIZE + 2 * count > free_start {
        return Err(corrupt("slot directory overlaps cell area"));
    }
    if count == 0 {
        return Ok(None);
    }
    let key_at = |slot_index: usize| -> Result<&[u8], inlaysql::Error> {
        let at = HEADER_SIZE + 2 * slot_index;
        let slot = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
        let Some(head) = bytes.get(slot..slot + 2) else {
            return Err(corrupt("leaf cell runs past end of page"));
        };
        let key_len = u16::from_le_bytes([head[0], head[1]]) as usize;
        bytes
            .get(slot + 2..slot + 2 + key_len)
            .ok_or_else(|| corrupt("leaf key runs past end of page"))
    };
    Ok(Some((key_at(0)?, key_at(count - 1)?)))
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
