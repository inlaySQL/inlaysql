//! What is actually inside a WAL commit record, byte by byte (AHL-564).
//!
//! AHL-563 closed by naming the record's size as the next lever: a region is
//! `WAL_BLOCKS` (256) x `page_size` = 1 MiB, a single-row commit writes ~20
//! KiB of record, so a region wraps every ~52 commits and each wrap runs a
//! full `fsync` inside the reservation gate. Before proposing anything, the
//! record has to be broken down: how many pages, which ones, and how many
//! bytes of each are real.
//!
//! This is a read-only instrument. It puts a [`Device`] between the tree and
//! [`FileDevice`] that captures every byte appended into a WAL region,
//! decodes each record with [`wal::decode_record`], and decodes each page
//! image with [`page::decode`] — so the classification is the file's own,
//! not a guess. For every page it reports its kind, its cell count, the key
//! its cells carry (so a leaf can be attributed to the table, the catalog or
//! the free list), and how much of the 4 KiB image is the zero hole
//! `PageWriter` leaves between the slot directory and the packed cells.
//!
//! It also prices the *format-free* shrink: how big the same record would be
//! if each page image carried its prefix and suffix and let recovery
//! zero-fill the middle. That is not physiological logging — the record can
//! still rebuild the page's exact bytes — so it is measured here to say
//! whether it is worth a format version.
//!
//! # Usage
//!
//! ```sh
//! cargo build --release -p inlaysql-bench --bin record_anatomy
//! DIR=/tmp TXNS=400 record_anatomy            # single-row INSERT
//! DIR=/tmp TXNS=400 SHAPE=update record_anatomy
//! ```
//!
//! Env: `DIR` (where the database file goes), `TXNS`, `BATCH` (rows per
//! statement), `SHAPE` (`insert`, `update`, `delete`, `insert_indexed`),
//! `DETAIL` (print the first N commits page by page).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use inlaysql::{Database, FileDevice, Value};
use inlaysql_core::btree::device::{AbsorbResult, AbsorbTxn, CommitPoint, PendingOps};
use inlaysql_core::btree::{page, Device, PageId, DEFAULT_PAGE_SIZE};
use inlaysql_core::{wal, Durability, Result};

/// One page image lifted out of one commit record.
#[derive(Clone)]
struct PageFact {
    id: PageId,
    len: usize,
    kind: u8,
    cells: usize,
    /// Bytes before the zero hole: header + slot directory, as the page
    /// itself declares them.
    prefix: usize,
    /// Bytes after the zero hole: the packed cells.
    suffix: usize,
    /// The lowest key the page carries, for attribution. Empty for overflow.
    first_key: Vec<u8>,
    /// Every key, for a page small enough that listing them says something —
    /// a one- or two-cell metadata leaf is the case worth naming.
    keys: Vec<(Vec<u8>, usize)>,
}

/// One commit record, as it went to the log.
struct RecordFact {
    bytes: usize,
    pages: Vec<PageFact>,
}

#[derive(Default)]
struct Captured {
    records: Vec<RecordFact>,
    wraps: u64,
    wrap_bytes: u64,
}

/// A [`Device`] that forwards everything and keeps a copy of every commit
/// record that goes past.
struct Tapping<D: Device> {
    inner: D,
    seen: Arc<Mutex<Captured>>,
    /// The version in the file's header — a v5 file's records still copy
    /// whole pages, and decoding one as v6 would find garbage.
    format_version: u32,
    wal_start: usize,
    data_start: usize,
    region_len: usize,
}

impl<D: Device> Tapping<D> {
    fn new(inner: D, seen: Arc<Mutex<Captured>>, format_version: u32) -> Self {
        Self {
            inner,
            seen,
            format_version,
            wal_start: wal::wal_start(DEFAULT_PAGE_SIZE),
            data_start: wal::data_offset_for(
                DEFAULT_PAGE_SIZE,
                wal::MULTI_REGION_FORMAT_VERSION,
                0,
            ),
            region_len: wal::wal_region_len(DEFAULT_PAGE_SIZE),
        }
    }
}

/// Split one WAL write into the records it holds — a cohort leader writes its
/// own record and one per member back to back in a single `pwrite`
/// (`encode_record_onto`), and each keeps its own length prefix.
fn split_records(mut data: &[u8], format_version: u32) -> Vec<RecordFact> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let total = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if total == 0 || total > data.len() {
            break;
        }
        let Some(record) = wal::decode_record_for_version(&data[..total], format_version) else {
            break;
        };
        out.push(RecordFact {
            bytes: total,
            pages: record
                .pages
                .iter()
                .map(|(id, b)| page_fact(*id, b))
                .collect(),
        });
        data = &data[total..];
    }
    out
}

/// What one page image in a record is: its kind, its cells, and where the
/// zero hole `PageWriter` leaves sits.
fn page_fact(id: PageId, bytes: &[u8]) -> PageFact {
    let kind = bytes.first().copied().unwrap_or(255);
    // Measured rather than derived from the header, so an overflow page — whose
    // layout is `kind u8 | next u64 | payload` and has no slot directory — is
    // described by the same two numbers as a leaf.
    let prefix = bytes
        .iter()
        .rposition(|b| *b != 0)
        .map(|_| leading_used(bytes))
        .unwrap_or(0);
    let suffix = trailing_used(bytes, prefix);
    let mut cells = 0;
    let mut first_key = Vec::new();
    let mut keys = Vec::new();
    if let Ok(node) = page::decode(bytes.len(), bytes) {
        match &node {
            page::Node::Leaf { entries, .. } => {
                cells = entries.len();
                if let Ok(Some((low, _))) = page::leaf_edge_keys(bytes, bytes.len()) {
                    first_key = low.to_vec();
                }
                if cells <= 8 {
                    let _ = page::scan_leaf_cells(bytes, bytes.len(), |key, value| {
                        keys.push((key.to_vec(), value.inline_len()));
                        Ok(())
                    });
                }
            }
            page::Node::Internal { cells: c, .. } => cells = c.len(),
        }
    }
    PageFact {
        id,
        len: bytes.len(),
        kind,
        cells,
        prefix,
        suffix,
        first_key,
        keys,
    }
}

/// The longest run of zeros in the middle: everything before it is the
/// prefix, everything after it the suffix. This is the exact quantity a
/// hole-eliding record entry would have to carry.
fn leading_used(bytes: &[u8]) -> usize {
    let (start, _) = longest_zero_run(bytes);
    start
}

fn trailing_used(bytes: &[u8], _prefix: usize) -> usize {
    let (start, len) = longest_zero_run(bytes);
    bytes.len() - start - len
}

fn longest_zero_run(bytes: &[u8]) -> (usize, usize) {
    let (mut best_at, mut best_len) = (0usize, 0usize);
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] != 0 {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at] == 0 {
            at += 1;
        }
        if at - start > best_len {
            best_at = start;
            best_len = at - start;
        }
    }
    (best_at, best_len)
}

impl<D: Device> Device for Tapping<D> {
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
        self.inner.read(offset, buf)
    }

    fn read_shared(&self, offset: usize, len: usize) -> Option<Arc<[u8]>> {
        self.inner.read_shared(offset, len)
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        if offset >= self.wal_start && offset < self.data_start {
            let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            if data.len() == self.region_len {
                seen.wraps += 1;
                seen.wrap_bytes += data.len() as u64;
            } else {
                seen.records
                    .extend(split_records(data, self.format_version));
            }
        }
        self.inner.write(offset, data)
    }

    fn sync(&mut self) -> Result<()> {
        self.inner.sync()
    }

    fn sync_commit(&mut self) -> Result<()> {
        self.inner.sync_commit()
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

    fn forget_append_offset(&self, region: usize) {
        self.inner.forget_append_offset(region);
    }

    fn create_format_version(&self) -> u32 {
        self.inner.create_format_version()
    }

    fn gate_phase(&self, phase: u32) {
        self.inner.gate_phase(phase);
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

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        page::KIND_LEAF => "leaf",
        page::KIND_INTERNAL => "internal",
        page::KIND_OVERFLOW => "overflow",
        _ => "?",
    }
}

/// A leaf's key prefix, rendered so the keyspace it belongs to is legible.
/// Printable ASCII stays as itself; anything else becomes an escape.
fn render(key: &[u8], max: usize) -> String {
    let mut out = String::new();
    for b in key.iter().take(max) {
        if b.is_ascii_graphic() {
            out.push(*b as char);
        } else {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    if key.len() > max {
        out.push_str("..");
    }
    out
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir: PathBuf = std::env::var("DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    fs::create_dir_all(&dir)?;
    let txns = env_usize("TXNS", 400);
    let batch = env_usize("BATCH", 1);
    let detail = env_usize("DETAIL", 3);
    let shape = std::env::var("SHAPE").unwrap_or_else(|_| "insert".to_string());
    let warmup = env_usize("WARMUP", 200);

    let path = dir.join("record-anatomy.inlay");
    let _ = fs::remove_file(&path);
    {
        let mut db = Database::open(&path)?;
        db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, body TEXT)", &[])?;
        if shape == "insert_indexed" {
            db.execute("CREATE INDEX kv_body ON kv (body)", &[])?;
        }
    }

    let seen = Arc::new(Mutex::new(Captured::default()));
    let format_version = {
        let probe = FileDevice::open(&path)?;
        let mut header = vec![0u8; DEFAULT_PAGE_SIZE];
        probe.read(wal::header_offset(), &mut header)?;
        inlaysql_core::btree::tree::parse_header(&header)?.1
    };
    println!("file format version {format_version}");
    let mut db = Database::open_on(Tapping::new(
        FileDevice::open(&path)?,
        Arc::clone(&seen),
        format_version,
    ))?;

    let payload = "x".repeat(64);
    let mut sql = String::from("INSERT INTO kv (id, body) VALUES ");
    for row in 0..batch {
        if row > 0 {
            sql.push_str(", ");
        }
        sql.push_str("(?, ?)");
    }
    let insert = db.prepare(&sql)?;
    let update = db.prepare("UPDATE kv SET body = ? WHERE id = ?")?;
    let delete = db.prepare("DELETE FROM kv WHERE id = ?")?;

    // The steady state is the thing being measured, not the first commits
    // into an empty tree: a one-page tree dirties one page and says nothing
    // about a root-to-leaf path. Warm up, then clear what was captured.
    let mut next_id: i64 = 1;
    let mut args: Vec<Value> = Vec::with_capacity(batch * 2);
    let mut fill = |db: &mut Database, n: usize, next_id: &mut i64| -> Result<()> {
        for _ in 0..n {
            args.clear();
            for _ in 0..batch {
                args.push(Value::Integer(*next_id));
                args.push(Value::Text(payload.clone().into()));
                *next_id += 1;
            }
            db.execute_prepared(&insert, &args)?;
        }
        Ok(())
    };
    fill(&mut db, warmup, &mut next_id)?;
    let seeded = next_id;
    seen.lock()
        .unwrap_or_else(|p| p.into_inner())
        .records
        .clear();
    seen.lock().unwrap_or_else(|p| p.into_inner()).wraps = 0;

    match shape.as_str() {
        "insert" | "insert_indexed" => fill(&mut db, txns, &mut next_id)?,
        "update" => {
            for i in 0..txns {
                let id = 1 + (i as i64 % (seeded - 1));
                db.execute_prepared(
                    &update,
                    &[Value::Text(payload.clone().into()), Value::Integer(id)],
                )?;
            }
        }
        "delete" => {
            for i in 0..txns.min((seeded - 1) as usize) {
                db.execute_prepared(&delete, &[Value::Integer(1 + i as i64)])?;
            }
        }
        other => return Err(format!("unknown shape {other}").into()),
    }
    drop(db);
    let captured = seen.lock().unwrap_or_else(|p| p.into_inner());
    let records = &captured.records;
    if records.is_empty() {
        return Err("no records captured".into());
    }

    println!(
        "shape={shape} txns={txns} batch={batch} warmup={warmup} page_size={DEFAULT_PAGE_SIZE}"
    );
    println!(
        "region {} bytes ({} blocks x {} B), {} regions; data area at {} B",
        wal::wal_region_len(DEFAULT_PAGE_SIZE),
        wal::WAL_BLOCKS,
        DEFAULT_PAGE_SIZE,
        wal::WAL_REGIONS,
        wal::data_offset_for(DEFAULT_PAGE_SIZE, wal::MULTI_REGION_FORMAT_VERSION, 0),
    );
    println!();

    // --- 1. The record, per commit ------------------------------------------
    let n = records.len() as f64;
    let total_bytes: usize = records.iter().map(|r| r.bytes).sum();
    let total_pages: usize = records.iter().map(|r| r.pages.len()).sum();
    let page_bytes: usize = records
        .iter()
        .flat_map(|r| r.pages.iter())
        .map(|p| p.len)
        .sum();
    // Per-page framing is the id and the length; the record's own scalars are
    // everything else.
    let framing = total_pages * 12;
    println!("== 1. record size ==");
    println!(
        "{} records captured, {:.0} B/record, {:.2} pages/record, {:.0} B of page images, \
         {:.0} B of per-page framing, {:.0} B of record scalars+checksum",
        records.len(),
        total_bytes as f64 / n,
        total_pages as f64 / n,
        page_bytes as f64 / n,
        framing as f64 / n,
        (total_bytes as f64 - page_bytes as f64 - framing as f64) / n,
    );
    let wrap_every = if captured.wraps > 0 {
        records.len() as f64 / captured.wraps as f64
    } else {
        f64::INFINITY
    };
    println!(
        "{} region wraps over {} commits — one every {:.1} commits per region \
         (region holds {:.1} records)",
        captured.wraps,
        records.len(),
        wrap_every,
        wal::wal_region_len(DEFAULT_PAGE_SIZE) as f64 / (total_bytes as f64 / n),
    );
    println!();

    // --- 2. What the pages are ----------------------------------------------
    // Attribution is by the key the leaf carries: the whole engine shares one
    // keyspace, so the first byte(s) of a leaf's lowest key say which logical
    // tree the page belongs to. Internal pages are attributed by kind only —
    // they carry separators, not rows.
    let mut by_bucket: BTreeMap<String, (usize, usize, usize, usize)> = BTreeMap::new();
    for record in records {
        for p in &record.pages {
            let bucket = match p.kind {
                page::KIND_INTERNAL => "internal (spine)".to_string(),
                page::KIND_OVERFLOW => "overflow".to_string(),
                page::KIND_LEAF => format!("leaf {}", render(&p.first_key, 6)),
                _ => "unknown".to_string(),
            };
            let e = by_bucket.entry(bucket).or_default();
            e.0 += 1;
            e.1 += p.len;
            e.2 += p.prefix + p.suffix;
            e.3 += p.cells;
        }
    }
    println!("== 2. which pages ==");
    println!(
        "{:<34} {:>8} {:>10} {:>10} {:>9} {:>8}",
        "bucket (leaf shown by lowest key)", "per cmt", "B/cmt", "used B/cmt", "used %", "cells"
    );
    let mut buckets: Vec<_> = by_bucket.iter().collect();
    buckets.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    for (name, (count, bytes, used, cells)) in buckets {
        println!(
            "{:<34} {:>8.2} {:>10.0} {:>10.0} {:>8.1}% {:>8.1}",
            name,
            *count as f64 / n,
            *bytes as f64 / n,
            *used as f64 / n,
            100.0 * *used as f64 / *bytes as f64,
            *cells as f64 / *count as f64,
        );
    }
    println!();

    // --- 3. What a hole-eliding record would cost ---------------------------
    // Every dirty page is a whole `page_size` image whose middle is the zero
    // hole `PageWriter` leaves between the slot directory and the packed
    // cells. A record entry that carried `prefix | suffix` and let recovery
    // zero-fill the middle rebuilds the same bytes — it is still physical
    // logging, so nothing about recovery's assumptions changes — and costs
    // four more bytes of framing per page.
    let elided: usize = records
        .iter()
        .map(|r| {
            r.bytes
                + r.pages.len() * 4
                + r.pages
                    .iter()
                    .map(|p| (p.prefix + p.suffix).wrapping_sub(p.len))
                    .map(|d| d as isize)
                    .sum::<isize>() as usize
        })
        .sum();
    let used_total: usize = records
        .iter()
        .flat_map(|r| r.pages.iter())
        .map(|p| p.prefix + p.suffix)
        .sum();
    println!("== 3. what the hole costs ==");
    // Only meaningful on a v5 file, whose entries still carry the hole. On a
    // v6 file the record already elides it and this projection would be
    // subtracting the same bytes twice.
    let whole_page = std::env::var_os("INLAYSQL_WHOLE_PAGE_WAL_RECORD").is_some();
    println!(
        "page images {:.0} B/commit, of which {:.0} B ({:.1}%) is the zero hole",
        page_bytes as f64 / n,
        (page_bytes - used_total) as f64 / n,
        100.0 * (page_bytes - used_total) as f64 / page_bytes as f64,
    );
    if !whole_page {
        println!(
            "(this file is v6: the record already elides the hole, so it is \
             {:.0} B/commit against {:.0} B of page image)",
            total_bytes as f64 / n,
            page_bytes as f64 / n,
        );
        println!();
        print_detail(records, detail);
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    println!(
        "hole-eliding record: {:.0} B/commit vs {:.0} B — {:.2}x smaller, \
         one region would hold {:.0} records instead of {:.0}",
        elided as f64 / n,
        total_bytes as f64 / n,
        total_bytes as f64 / elided as f64,
        wal::wal_region_len(DEFAULT_PAGE_SIZE) as f64 / (elided as f64 / n),
        wal::wal_region_len(DEFAULT_PAGE_SIZE) as f64 / (total_bytes as f64 / n),
    );
    println!();

    print_detail(records, detail);

    let _ = fs::remove_file(&path);
    Ok(())
}

/// A few commits, page by page.
fn print_detail(records: &[RecordFact], detail: usize) {
    println!("== 4. the first {detail} commits, page by page ==");
    for (i, record) in records.iter().take(detail).enumerate() {
        println!(
            "commit {i}: {} B, {} pages",
            record.bytes,
            record.pages.len()
        );
        for p in &record.pages {
            println!(
                "  page {:>6} {:<9} {:>4} cells  {:>5} B image  {:>5} B used \
                 (prefix {:>4} + suffix {:>4})  key {}",
                p.id,
                kind_name(p.kind),
                p.cells,
                p.len,
                p.prefix + p.suffix,
                p.prefix,
                p.suffix,
                render(&p.first_key, 12),
            );
            for (key, value_len) in &p.keys {
                println!("        cell {:<40} value {} B", render(key, 40), value_len);
            }
        }
    }
}
