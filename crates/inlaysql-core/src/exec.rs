//! The streaming row pipeline: scan → filter → join → (aggregate/sort) → limit
//! → project.
//!
//! Every stage here is an [`Iterator`], so a row is pulled from storage, tested,
//! joined and handed to the caller before the next one is read. That is the
//! whole point: `SELECT ... LIMIT 10` over a million-row table used to
//! materialise a million `(RowId, Vec<u8>)` pairs, decode a million rows, and
//! then throw all but ten away (`docs/architecture.md`, gap G5). Now it stops after ten.
//!
//! # What still materialises, and why it must
//!
//! **Sort and aggregate.** Neither can emit its first row before it has seen its
//! last input row, so both are blocking by definition. They stay in
//! [`crate::engine`], operating on the `Vec` the pipeline collects into.
//!
//! **The inner side of a join, when nothing can narrow it.** A nested loop
//! replays the inner side once per outer row, so it has to be re-readable.
//! [`JoinInner::Materialised`] holds it as a `Vec`. The operator only ever asks
//! it for "the rows that could match *this* outer row", and since AHL-464 that
//! question has a second answer: [`JoinInner::Probe`] reads the inner rows one
//! outer key can match and nothing else (Phase 2 item 4). Which one a join gets
//! is the planner's decision, in [`crate::engine::Engine::join_probe`];
//! [`NestedLoopJoin`] cannot tell them apart.
//!
//! # Errors
//!
//! A stage that fails yields one `Err` and then stops. Collecting the pipeline
//! with `collect::<Result<Vec<_>>>()` therefore reports exactly the first
//! failure and reads nothing after it, which is what a `?` in the middle of the
//! old materialising loop did.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::collation::Collation;
use crate::error::Result;
use crate::eval::{self, Computed, Env};
use crate::index::KeyRange;
use crate::plan::{Expr, JoinKind};
use crate::row::{decode_row_masked, decode_row_ref_masked, ColumnMask, RowBuf};
use crate::traits::{Interrupt, RowId, RowScan, Storage};
use crate::value::{DataType, Value, ValueRef};

/// A decoded candidate row on its way to a result set: its row id, its
/// retrieval score (only present when the query scored), the decoded values,
/// the aggregate results already computed for its group (empty outside an
/// aggregate query), and the window function results already computed for
/// its frame (empty until the executor's window stage has run, and always
/// empty for a query with no window functions).
#[derive(Debug, Clone)]
pub(crate) struct ExecRow {
    /// The driving table's row id, used to break sort ties deterministically.
    pub id: RowId,
    /// The retrieval score, if the query scored.
    pub score: Option<f32>,
    /// Decoded values, the joined row.
    pub values: Vec<Value>,
    /// Per-group aggregate results, aligned with `SelectPlan::aggregates`.
    pub aggregates: Vec<Value>,
    /// Per-row window function results, aligned with `SelectPlan::windows`.
    pub windows: Vec<Value>,
}

impl ExecRow {
    /// A freshly scanned row, before any join, aggregation or window stage.
    pub fn scanned(id: RowId, values: Vec<Value>) -> Self {
        Self {
            id,
            score: None,
            values,
            aggregates: Vec::new(),
            windows: Vec::new(),
        }
    }

    /// What holding this row costs, for [`collect_bounded`].
    ///
    /// The three `Vec`s and everything their cells own. `aggregates` and
    /// `windows` are empty at the point the budget is checked — they are filled
    /// by stages that run *after* the input is collected — but they are counted
    /// anyway, because a row that acquires them later is the same row.
    fn resident_bytes(&self) -> usize {
        let mut bytes = core::mem::size_of::<Self>();
        for cells in [&self.values, &self.aggregates, &self.windows] {
            bytes = bytes.saturating_add(
                cells
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Value>()),
            );
            for value in cells {
                bytes = bytes.saturating_add(value.heap_bytes());
            }
        }
        bytes
    }
}

/// Collect a blocking operator's whole input, refusing past `budget` bytes.
///
/// `ORDER BY`, `GROUP BY`, `DISTINCT` and window functions all have to hold
/// their entire input before they can emit anything — see the module docs
/// above for why that is not a shortcoming to be fixed. What *is* a
/// shortcoming is holding it without a bound: the only thing that then stops
/// one query is the operating system's out-of-memory killer, and that does not
/// stop the query, it stops the process, along with every other connection it
/// was serving. A refused statement is recoverable; a dead process is not.
///
/// `budget` of `0` means no ceiling, which is the old behaviour and is still
/// what an embedded caller may want — see
/// [`crate::EngineOptions::query_memory_bytes`].
///
/// The accounting is per row and conservative (see [`Value::heap_bytes`]), and
/// it charges for the row *before* pushing it, so the ceiling is never crossed
/// by more than one row's worth. It deliberately does not try to account for
/// what the fold, sort or projection downstream will allocate on top: this
/// bounds the dominant term, and pretending to a precision it does not have
/// would make the number harder to choose rather than safer.
pub(crate) fn collect_bounded(
    stream: RowStream<'_>,
    budget: usize,
    interrupt: &Interrupt,
) -> Result<Vec<ExecRow>> {
    if budget == 0 {
        // Still checked, even with the ceiling removed: an unbounded collect is
        // precisely the shape that runs longest, so the caller that opted out
        // of the memory limit is the one that most needs the time limit.
        let mut rows: Vec<ExecRow> = Vec::new();
        for row in stream {
            interrupt.check()?;
            rows.push(row?);
        }
        return Ok(rows);
    }
    let mut rows: Vec<ExecRow> = Vec::new();
    let mut held = 0usize;
    for row in stream {
        interrupt.check()?;
        let row = row?;
        held = held.saturating_add(row.resident_bytes());
        if held > budget {
            return Err(crate::error::Error::Memory(alloc::format!(
                "this statement has to hold its whole input before it can answer \
                 (ORDER BY, GROUP BY, DISTINCT or a window function), and at {} rows \
                 that is past the {budget}-byte per-statement ceiling. Narrow the \
                 `WHERE`, add a `LIMIT` that the sort can be pushed into, or raise \
                 `EngineOptions::query_memory_bytes`. Nothing was written.",
                rows.len() + 1
            )));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// One stage of the pipeline, boxed so stages can be stacked at run time.
///
/// A box per stage is one allocation per *statement*, against the per-row
/// allocations this module exists to remove.
pub(crate) type RowStream<'a> = Box<dyn Iterator<Item = Result<ExecRow>> + 'a>;

/// The bytes a scan pulls out of storage, before anything is decoded.
///
/// The three access paths this engine has. A filter that pins the row id
/// (`WHERE id = 42` on an `INTEGER PRIMARY KEY`) collapses to one tree descent
/// and yields at most one row; a filter a scalar B-tree index covers becomes an
/// index range probe and one descent per surviving row id (AHL-423); anything
/// else streams the table. [`crate::engine::Engine::candidate_bytes`] is where
/// the choice is made, and every one of them yields the *same* rows in the same
/// order — only the number of rows read differs.
pub(crate) enum RowBytes<'a> {
    /// A sequential scan, streamed in row-id order.
    Scan(RowScan<'a>),
    /// A single row a primary-key equality pinned, or nothing when it is absent.
    Point(Option<(RowId, RowBuf)>),
    /// The row ids a secondary-index range probe selected, each fetched by one
    /// tree descent as it is pulled.
    ///
    /// Why the ids are a `Vec` and the rows are not: the probe reads a
    /// contiguous run of index *entries*, which are keys with empty values and
    /// cost nothing to decode, and it has to read all of them to sort them back
    /// into row-id order (entries sort by value first). The rows themselves are
    /// the expensive part, and they are read one at a time — so `LIMIT 5` over
    /// an indexed filter fetches five rows, not the whole range.
    Indexed {
        storage: &'a dyn Storage,
        table: alloc::string::String,
        ids: alloc::vec::IntoIter<RowId>,
        /// Where a cancelled statement is noticed. A wide range is one tree
        /// descent per surviving id, so this loop can run as long as a table
        /// scan can and needs the same check [`RowScan::watched`] gives one.
        interrupt: &'a Interrupt,
    },
}

impl<'a> RowBytes<'a> {
    /// The rows `ids` name, in the order given, read lazily.
    ///
    /// A row id with no row is skipped rather than reported: an index entry
    /// that outlives its row would be a maintenance bug, and the scan path
    /// would not have seen the row either, so skipping keeps the two paths
    /// answering the same thing.
    pub fn indexed(
        storage: &'a dyn Storage,
        table: &str,
        ids: Vec<RowId>,
        interrupt: &'a Interrupt,
    ) -> Self {
        RowBytes::Indexed {
            storage,
            table: alloc::string::String::from(table),
            ids: ids.into_iter(),
            interrupt,
        }
    }
}

impl Iterator for RowBytes<'_> {
    type Item = Result<(RowId, RowBuf)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            RowBytes::Scan(scan) => scan.next(),
            RowBytes::Point(row) => row.take().map(Ok),
            RowBytes::Indexed {
                storage,
                table,
                ids,
                interrupt,
            } => loop {
                let id = ids.next()?;
                if let Err(error) = interrupt.check() {
                    *ids = Vec::new().into_iter();
                    return Some(Err(error));
                }
                match storage.get_row(table, id) {
                    Ok(Some(bytes)) => return Some(Ok((id, bytes))),
                    Ok(None) => continue,
                    Err(error) => {
                        // One `Err` and then done, as every other stage here
                        // behaves, so `collect::<Result<Vec<_>>>()` reports the
                        // first failure and reads nothing after it.
                        *ids = Vec::new().into_iter();
                        return Some(Err(error));
                    }
                }
            },
        }
    }
}

/// Decode the scanned bytes into rows, materialising only the columns the
/// statement can observe.
pub(crate) struct Decode<'a> {
    source: RowBytes<'a>,
    mask: &'a ColumnMask,
}

impl<'a> Decode<'a> {
    /// Decode `source` under `mask`. See [`ColumnMask`] for why a mask that is
    /// too narrow is a wrong answer rather than a slow one.
    pub fn new(source: RowBytes<'a>, mask: &'a ColumnMask) -> Self {
        Self { source, mask }
    }
}

impl Iterator for Decode<'_> {
    type Item = Result<ExecRow>;

    fn next(&mut self) -> Option<Self::Item> {
        let (id, bytes) = match self.source.next()? {
            Ok(row) => row,
            Err(error) => return Some(Err(error)),
        };
        Some(decode_row_masked(&bytes, self.mask).map(|values| ExecRow::scanned(id, values)))
    }
}

/// Drop the rows a predicate does not admit.
///
/// Used against an already-decoded [`RowStream`] — the residual `WHERE` a
/// join applies to its joined rows, where the operands are `Value`s a join
/// already materialised and there is no row-bytes source left to decode
/// lazily from. [`DecodeFilter`] is the fused, allocation-avoiding version of
/// this same idea for the single-table case, where there still is one.
pub(crate) struct Filter<'a> {
    input: RowStream<'a>,
    predicate: &'a Expr,
    env: &'a Env<'a>,
}

impl<'a> Filter<'a> {
    /// Keep only the rows `predicate` is true for. SQL's three-valued logic
    /// applies: `NULL` is not true, so a row it evaluates to is dropped.
    pub fn new(input: RowStream<'a>, predicate: &'a Expr, env: &'a Env<'a>) -> Self {
        Self {
            input,
            predicate,
            env,
        }
    }
}

impl Iterator for Filter<'_> {
    type Item = Result<ExecRow>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let row = match self.input.next()? {
                Ok(row) => row,
                Err(error) => return Some(Err(error)),
            };
            match eval::evaluate(self.predicate, &row.values, Computed::NONE, self.env) {
                Ok(value) => {
                    if eval::is_truthy(&value) {
                        return Some(Ok(row));
                    }
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

/// Decode row bytes into a row, testing a predicate before ever materialising
/// an owned cell — the fused replacement for [`Decode`] followed by [`Filter`]
/// on the single-table read path (`AHL-478`; `PERF.md`'s "structural fix").
///
/// The two used to be separate stages, so every scanned or probed row paid
/// `decode_row_masked`'s allocation whether or not `Filter` went on to keep
/// it. This decodes into borrowed [`ValueRef`] cells first — free for
/// `NULL`/`INTEGER`/`REAL`, a slice into the row bytes for `TEXT`/`BLOB` — and
/// tests the predicate against those with [`eval::evaluate_ref`]. Only a row
/// that survives is turned into owned [`Value`]s, once, for [`ExecRow`]: "a
/// projected row allocates once at the boundary."
///
/// A row with no predicate at all (`WHERE` absent) is not built with this —
/// `Decode` alone is used, since there is nothing to filter and the borrowed
/// intermediate would only cost a decode pass for no benefit.
pub(crate) struct DecodeFilter<'a> {
    source: RowBytes<'a>,
    mask: &'a ColumnMask,
    predicate: &'a Expr,
    env: &'a Env<'a>,
}

impl<'a> DecodeFilter<'a> {
    /// Decode `source` under `mask`, keeping only the rows `predicate` is
    /// true for. Same `ColumnMask` and three-valued-truth rules as
    /// [`Decode`] and [`Filter`] apply separately today.
    pub fn new(
        source: RowBytes<'a>,
        mask: &'a ColumnMask,
        predicate: &'a Expr,
        env: &'a Env<'a>,
    ) -> Self {
        Self {
            source,
            mask,
            predicate,
            env,
        }
    }
}

impl Iterator for DecodeFilter<'_> {
    type Item = Result<ExecRow>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (id, bytes) = match self.source.next()? {
                Ok(row) => row,
                Err(error) => return Some(Err(error)),
            };
            let cells = match decode_row_ref_masked(&bytes, self.mask) {
                Ok(cells) => cells,
                Err(error) => return Some(Err(error)),
            };
            match eval::evaluate_ref(self.predicate, &cells, Computed::NONE, self.env) {
                Ok(value) => {
                    if eval::is_truthy(&value) {
                        let values = cells.iter().map(ValueRef::to_owned_value).collect();
                        return Some(Ok(ExecRow::scanned(id, values)));
                    }
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

/// Where a join's inner rows come from.
///
/// The operator asks only [`JoinInner::prepare`] and [`JoinInner::rows`] — "the
/// rows that could match *this* outer row" — and both variants answer the same
/// question, so [`NestedLoopJoin`] cannot tell which it was handed.
///
/// The two answers differ only in how many inner rows are *read*, never in
/// which pairs survive: the `ON` predicate is re-evaluated over every candidate
/// either way, so a probe that returns a superset is slow rather than wrong and
/// one that returns a subset would be a bug. That is the same contract
/// [`crate::engine::Engine::candidate_bytes`] has for the single-table access
/// paths.
pub(crate) enum JoinInner<'a> {
    /// The whole inner table, decoded once and replayed per outer row.
    Materialised {
        /// Its rows, in row-id order.
        rows: Vec<Vec<Value>>,
        /// The inner table's declared width, which is what a `LEFT JOIN` pads
        /// an unmatched outer row with.
        width: usize,
    },
    /// The rows one outer key names, fetched per outer row (Phase 2 item 4).
    Probe(Box<IndexProbe<'a>>),
    /// The inner table keyed into a hash table, probed per outer row in O(1).
    Hash(HashJoin),
}

impl<'a> JoinInner<'a> {
    /// An index-probed inner side.
    pub fn probe(probe: IndexProbe<'a>) -> Self {
        JoinInner::Probe(Box::new(probe))
    }

    /// Narrow the inner side to the rows that could match `outer`.
    ///
    /// A materialised side cannot narrow anything; a probe does its whole
    /// job here.
    fn prepare(&mut self, outer: &[Value]) -> Result<()> {
        match self {
            JoinInner::Materialised { .. } => Ok(()),
            JoinInner::Probe(probe) => probe.prepare(outer),
            JoinInner::Hash(hash) => {
                hash.prepare(outer);
                Ok(())
            }
        }
    }

    /// The candidates [`JoinInner::prepare`] left.
    fn rows(&self) -> &[Vec<Value>] {
        match self {
            JoinInner::Materialised { rows, .. } => rows,
            JoinInner::Probe(probe) => probe.rows(),
            JoinInner::Hash(hash) => hash.rows(),
        }
    }

    /// Append one candidate's values into `out`, cloning only when the row has
    /// to survive to be paired again.
    ///
    /// A materialised side (and a probe's table-scan fallback) is replayed
    /// once per outer row, so this clones, exactly as reading through
    /// [`JoinInner::rows`] and cloning always has. A probe's matched rows are
    /// rebuilt fresh by [`JoinInner::prepare`] for every outer row and never
    /// read again after this outer row's pairing loop moves past them — they
    /// were already decoded once, by `IndexProbe::fetch`, so cloning them a
    /// second time into the pairing buffer was pure waste. Those are moved
    /// instead. A hash join's rows are shared across every outer row that
    /// reaches their bucket, so they clone, the same as the materialised side.
    ///
    /// Appending straight into the caller's buffer — rather than handing back a
    /// temporary `Vec<Value>` — is what saves one heap allocation per
    /// candidate: the caller's scratch is already sized for the outer plus
    /// inner width, and the temporary `Vec` this used to return was allocated
    /// only to be drained into that scratch.
    fn append_row_into(&mut self, index: usize, out: &mut Vec<Value>) {
        match self {
            JoinInner::Materialised { rows, .. } => out.extend(rows[index].iter().cloned()),
            JoinInner::Probe(probe) => out.extend(probe.take_row(index)),
            JoinInner::Hash(hash) => out.extend(hash.rows()[index].iter().cloned()),
        }
    }

    /// How many columns an unmatched `LEFT JOIN` row is padded with.
    fn width(&self) -> usize {
        match self {
            JoinInner::Materialised { width, .. } => *width,
            JoinInner::Probe(probe) => probe.width,
            JoinInner::Hash(hash) => hash.table.width(),
        }
    }

    /// Whether this side is narrowed by a hash build.
    pub fn is_hash(&self) -> bool {
        matches!(self, JoinInner::Hash(_))
    }

    /// For an exact hash-key `ON`, verify one candidate without invoking the
    /// generic expression evaluator. `None` means this is not a hash side.
    fn hash_candidate_matches(&self, index: usize, outer: &[Value]) -> Option<bool> {
        match self {
            JoinInner::Hash(hash) => Some(hash.candidate_matches(index, outer)),
            JoinInner::Materialised { .. } | JoinInner::Probe(_) => None,
        }
    }
}

/// How one outer row's key reaches the inner rows it could match.
pub(crate) enum ProbeKind {
    /// The inner table's `INTEGER PRIMARY KEY`. The key *is* the storage key,
    /// so the probe is one tree descent and yields at most one row.
    RowId,
    /// A scalar B-tree index, by name. The probe reads the run of entries whose
    /// leading column equals the key, then fetches the rows they name.
    ///
    /// A composite index qualifies on its *leading* column alone, for the
    /// reason [`crate::engine::index_probe`] gives: entries that agree on the
    /// leading column are contiguous, and nothing past the first unbound column
    /// is.
    Index(String),
}

/// A join's inner side, read one outer key at a time.
///
/// This is the operator half of Phase 2 item 4; the planner half — which joins
/// get one, and which fall back — is [`crate::engine::Engine::join_probe`].
///
/// # What it must not change
///
/// The candidates it produces are handed to the same `ON` predicate the
/// materialising path feeds, in the same order (row-id ascending), so the pairs
/// that survive and the order they arrive in are identical. Two rules keep the
/// candidate set from ever being *smaller* than the set the predicate would
/// admit:
///
/// * **A `NULL` key matches nothing**, including another `NULL`, so it yields
///   no candidates at all. This is not an optimisation that has to be argued
///   for separately: the probe's equality is a conjunct of the `ON`, and a
///   conjunct that is `NULL` makes the whole `ON` unable to be true. A
///   `LEFT JOIN` still pads the outer row, exactly as it would have.
/// * **A key of a class the column cannot hold falls back to the whole table.**
///   `eval::comparison` *errors* on a cross-class compare rather than returning
///   false, and an empty probe would have turned that error into an empty
///   result — the same reason [`crate::engine::indexable_probe`] exists for the
///   single-table path. The fallback is read once and kept, so the worst case is
///   the materialising path, reached one outer row later.
pub(crate) struct IndexProbe<'a> {
    storage: &'a dyn Storage,
    /// The inner table's name, as the catalog spells it.
    table: String,
    /// Which of the inner table's columns the statement can observe. A column
    /// left out decodes as `NULL`, so this is the same mask the materialising
    /// path would have decoded under and not a narrower one.
    mask: ColumnMask,
    /// The inner table's declared width — what a `LEFT JOIN` pads with.
    width: usize,
    /// The joined-row ordinal the key is read from. Always a column of a table
    /// the join has already produced: the planner only builds a probe when the
    /// equality has one side there and one side in the inner table.
    key: usize,
    /// The declared type of the inner column the probe binds, which is what
    /// decides whether a given key can be answered from the index at all.
    ty: DataType,
    /// The collating sequence the index is keyed under, which is also the one
    /// the `ON`'s equality resolved — [`crate::engine::Engine::join_probe`]
    /// only builds a probe when those two agree. The key is encoded under it,
    /// so a `NOCASE` index is probed with the folded key and finds the rows a
    /// `NOCASE` `=` would.
    collation: Collation,
    kind: ProbeKind,
    /// The candidates for the outer row [`IndexProbe::prepare`] last saw.
    ///
    /// Reused rather than reallocated: a join probes once per outer row, and
    /// the buffer is the one allocation that would otherwise be per-row.
    matched: Vec<Vec<Value>>,
    /// The whole inner table, for the keys the index cannot answer. `None`
    /// until the first such key, and read at most once.
    fallback: Option<Vec<Vec<Value>>>,
    /// Whether the outer row being paired is on the fallback.
    scanning: bool,
    /// Where a cancelled statement is noticed: the fallback below is a full
    /// table scan, and a probe of a low-selectivity index is a descent per id.
    interrupt: &'a Interrupt,
}

impl<'a> IndexProbe<'a> {
    /// A probe of `table` that reads its key from ordinal `key` of the joined
    /// row.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        storage: &'a dyn Storage,
        table: &str,
        mask: ColumnMask,
        width: usize,
        key: usize,
        ty: DataType,
        collation: Collation,
        kind: ProbeKind,
        interrupt: &'a Interrupt,
    ) -> Self {
        Self {
            storage,
            table: String::from(table),
            mask,
            width,
            key,
            ty,
            collation,
            kind,
            matched: Vec::new(),
            fallback: None,
            scanning: false,
            interrupt,
        }
    }

    /// Read the inner rows `outer`'s key could match.
    fn prepare(&mut self, outer: &[Value]) -> Result<()> {
        self.scanning = false;
        self.matched.clear();

        let key = match outer.get(self.key) {
            Some(key) => key,
            // A joined row narrower than the plan's ordinals reads as `NULL`
            // everywhere else in the engine, and `NULL` matches nothing.
            None => return Ok(()),
        };
        if *key == Value::Null {
            return Ok(());
        }
        if !crate::engine::indexable_probe(self.ty, key) {
            return self.fall_back();
        }

        match &self.kind {
            ProbeKind::RowId => {
                if let Some(id) = row_id_of(key) {
                    self.fetch(id)?;
                }
            }
            ProbeKind::Index(index) => {
                let range = KeyRange::equality(index, &[key], &[self.collation])?;
                // `scan_index_row_ids` (`AHL-479`), not `scan_index_range` plus
                // a per-entry decode: a join probe never looks at anything but
                // the row id an entry names, which is exactly the case the
                // row-id-only walk exists for — see its doc comment.
                let mut ids = self
                    .storage
                    .scan_index_row_ids(&range.start, range.end.as_deref())?;
                // Entries sort by value and only then by row id, so a probe of
                // a composite index's leading column is not in row-id order.
                // The materialising path replays the inner table in row-id
                // order and the pairs come out in that order, so this is what
                // keeps the two answers identical row for row.
                ids.sort_unstable();
                for id in ids {
                    self.interrupt.check()?;
                    self.fetch(id)?;
                }
            }
        }
        Ok(())
    }

    /// Fetch one row by id and keep it, skipping one that is not there.
    ///
    /// A row id with no row is skipped rather than reported, for the reason
    /// [`RowBytes::indexed`] gives: an index entry outliving its row would be a
    /// maintenance bug, and the materialising path would not have seen the row
    /// either.
    fn fetch(&mut self, id: RowId) -> Result<()> {
        if let Some(bytes) = self.storage.get_row(&self.table, id)? {
            self.matched.push(decode_row_masked(&bytes, &self.mask)?);
        }
        Ok(())
    }

    /// Read the whole inner table, once, and pair this outer row against all of
    /// it.
    fn fall_back(&mut self) -> Result<()> {
        self.scanning = true;
        if self.fallback.is_none() {
            let mut rows = Vec::new();
            for row in RowScan::watched(self.storage, &self.table, self.interrupt) {
                rows.push(decode_row_masked(&row?.1, &self.mask)?);
            }
            self.fallback = Some(rows);
        }
        Ok(())
    }

    /// The candidates the last [`IndexProbe::prepare`] left.
    fn rows(&self) -> &[Vec<Value>] {
        match (self.scanning, &self.fallback) {
            (true, Some(rows)) => rows,
            _ => &self.matched,
        }
    }

    /// Take ownership of one candidate at `index`.
    ///
    /// The fallback table scan is cached and replayed for every outer row
    /// that reaches it, so that clones, same as [`IndexProbe::rows`] always
    /// did. `self.matched` is rebuilt by [`IndexProbe::prepare`] for every
    /// outer row and each entry is read exactly once — by the pairing loop
    /// that calls this — so taking it is sound: nothing later in this outer
    /// row's iteration, or the next one, reads `self.matched[index]` again.
    fn take_row(&mut self, index: usize) -> Vec<Value> {
        match (self.scanning, &mut self.fallback) {
            (true, Some(rows)) => rows[index].clone(),
            _ => core::mem::take(&mut self.matched[index]),
        }
    }
}

/// A join's inner side, materialised once and keyed into a hash table.
///
/// The alternative to [`IndexProbe`] for a full-scan equi-join: instead of one
/// B-tree descent per outer row, the whole inner table is read once, bucketed
/// by the join key, and each outer row then reads its own bucket in O(1). That
/// trades an up-front O(inner) scan and build for O(outer) probes, which wins
/// whenever the outer side is large — the common ORM join — and loses when a
/// `LIMIT` would have let an index probe stop after a few outer rows, which is
/// why the planner (see [`crate::engine::Engine::join_inner`]) only hands one
/// out on a full scan.
///
/// # Correctness
///
/// The hash is a *candidate* narrowing, exactly like [`IndexProbe`]: the `ON`
/// is still re-evaluated over every row the bucket holds. For that to never
/// miss a pair, two keys the `ON`'s equality compares equal must hash to the
/// same bucket. The planner only builds one when the two key columns share a
/// declared storage class and the collation is binary, which is what makes
/// "equal ⇒ same bucket" hold without any cross-class or case-folding
/// normalisation here.
pub(crate) struct HashJoin {
    table: Rc<HashJoinTable>,
    /// The joined-row ordinal the outer key is read from.
    key: usize,
    /// The bucket the last [`HashJoin::prepare`] selected; `usize::MAX` is the
    /// empty sentinel (a `NULL` or missing key), which reads as an empty range.
    current: usize,
}

/// The immutable, expensive half of a hash join.
///
/// It is separate from [`HashJoin`]'s per-execution probe cursor so an engine
/// can retain one bounded prepared build across executions without sharing
/// mutable query state. Rows never change under one committed MVCC snapshot;
/// the engine versions and invalidates this object at that boundary.
pub(crate) struct HashJoinTable {
    /// Every inner row, contiguous and grouped by bucket, so a probe reads one
    /// cache-friendly run instead of chasing a bucket header per lookup. Rows
    /// are placed in scan order and the scan is row-id ascending, so each
    /// bucket's run stays row-id ascending — the order the materialising path
    /// would have replayed them in, so the pairs come out in the same order.
    rows: Vec<Vec<Value>>,
    /// `offsets[b]..offsets[b + 1]` is the run of `rows` bucket `b` holds; it
    /// has `mask + 2` entries so `offsets[b + 1]` is always in bounds.
    offsets: Vec<usize>,
    /// `offsets.len() - 2`, a power of two, so a hash indexes a bucket with a
    /// mask instead of a modulo.
    mask: usize,
    /// The inner row ordinal whose value selects a bucket.
    inner_key: usize,
    /// The inner table's declared width, what a `LEFT JOIN` pads with.
    width: usize,
    /// The collating sequence the `ON`'s `=` resolved, which both the bucket
    /// hash and the candidate comparison have to agree on. A build made under
    /// one collation cannot answer a probe under another — the bucket layout
    /// itself differs — so the engine's build cache keys on this too.
    collation: Collation,
}

impl HashJoin {
    /// Build the hash table by scanning `table` once, keying each decoded row
    /// on its `inner_key` column and remembering where the outer key is read
    /// from.
    ///
    /// The rows are laid out with a counting-sort (two passes: count each
    /// bucket, prefix-sum into `offsets`, then place every row at its bucket's
    /// next free slot) rather than as a `Vec` of bucket `Vec`s. That keeps the
    /// whole inner side in one contiguous allocation and groups each bucket's
    /// rows adjacently, so a probe walks a single run instead of a chain of
    /// small allocations.
    pub fn build_table(
        storage: &dyn Storage,
        table: &str,
        mask: ColumnMask,
        inner_key: usize,
        width: usize,
        collation: Collation,
        interrupt: &Interrupt,
    ) -> Result<Rc<HashJoinTable>> {
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for row in RowScan::watched(storage, table, interrupt) {
            rows.push(decode_row_masked(&row?.1, &mask)?);
        }
        let buckets_len = rows.len().next_power_of_two().max(16);
        let mask_bits = buckets_len - 1;

        // Counting sort into bucket-contiguous order. The placement pass walks
        // the rows in scan order, so each bucket's run stays row-id ascending.
        let mut counts = alloc::vec![0usize; buckets_len];
        for row in &rows {
            counts[(hash_value(&row[inner_key], collation) as usize) & mask_bits] += 1;
        }
        let mut offsets = Vec::with_capacity(buckets_len + 1);
        let mut running = 0usize;
        offsets.push(0);
        for &count in &counts {
            running += count;
            offsets.push(running);
        }
        // `counts` is repurposed as the per-bucket write cursor, seeded with
        // each bucket's start.
        for (cursor, start) in counts.iter_mut().zip(offsets.iter()) {
            *cursor = *start;
        }
        let mut placed: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
        placed.resize_with(rows.len(), Vec::new);
        for row in rows {
            let bucket = (hash_value(&row[inner_key], collation) as usize) & mask_bits;
            let slot = counts[bucket];
            placed[slot] = row;
            counts[bucket] = slot + 1;
        }
        Ok(Rc::new(HashJoinTable {
            rows: placed,
            offsets,
            mask: mask_bits,
            inner_key,
            width,
            collation,
        }))
    }

    /// Attach one execution's outer-key ordinal and probe cursor to a shared
    /// immutable build.
    pub fn from_table(table: Rc<HashJoinTable>, key: usize) -> Self {
        Self {
            table,
            key,
            current: usize::MAX,
        }
    }

    /// Narrow the inner side to the bucket `outer`'s key selects.
    fn prepare(&mut self, outer: &[Value]) {
        self.current = match outer.get(self.key) {
            Some(Value::Null) | None => usize::MAX,
            Some(value) => (hash_value(value, self.table.collation) as usize) & self.table.mask,
        };
    }

    /// The bucket [`HashJoin::prepare`] selected.
    pub fn rows(&self) -> &[Vec<Value>] {
        if self.current == usize::MAX {
            return &[];
        }
        let start = self.table.offsets[self.current];
        let end = self.table.offsets[self.current + 1];
        &self.table.rows[start..end]
    }

    /// Compare the exact key after the bucket narrowed candidates. Hash
    /// collisions remain possible, so equality cannot be omitted entirely.
    fn candidate_matches(&self, index: usize, outer: &[Value]) -> bool {
        let Some(outer) = outer.get(self.key) else {
            return false;
        };
        self.rows()
            .get(index)
            .and_then(|row| row.get(self.table.inner_key))
            .is_some_and(|inner| keys_equal(outer, inner, self.table.collation))
    }

    /// The outer joined-row ordinal the key is read from. For a single join
    /// this is the driving table's own column ordinal, which is what the
    /// key-only outer scan needs to decode just that column.
    pub fn key_ordinal(&self) -> usize {
        self.key
    }

    /// Narrow the inner side to the bucket `key` selects, without an outer row
    /// slice — the key-only outer scan reads the key as a bare `Value`.
    pub fn prepare_key(&mut self, key: &Value) {
        self.current = match key {
            Value::Null => usize::MAX,
            value => (hash_value(value, self.table.collation) as usize) & self.table.mask,
        };
    }

    /// [`HashJoin::candidate_matches`] for a bare key.
    pub fn candidate_matches_key(&self, index: usize, key: &Value) -> bool {
        self.rows()
            .get(index)
            .and_then(|row| row.get(self.table.inner_key))
            .is_some_and(|inner| keys_equal(key, inner, self.table.collation))
    }
}

impl HashJoinTable {
    /// Conservative resident-memory accounting for the cache budget.
    ///
    /// The outer `Vec` owns every row header, each row owns its `Value` array,
    /// and blob/vector/text payloads are counted as well. `Text` is an `Arc`,
    /// but each decoded inner cell creates its own allocation before any probe
    /// clones it, so charging its bytes here is the honest retained cost.
    pub fn resident_bytes(&self) -> usize {
        let mut bytes = core::mem::size_of::<Self>()
            .saturating_add(
                self.rows
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Vec<Value>>()),
            )
            .saturating_add(
                self.offsets
                    .capacity()
                    .saturating_mul(core::mem::size_of::<usize>()),
            );
        for row in &self.rows {
            bytes =
                bytes.saturating_add(row.capacity().saturating_mul(core::mem::size_of::<Value>()));
            for value in row {
                bytes = bytes.saturating_add(value.heap_bytes());
            }
        }
        bytes
    }

    /// The declared inner width used to pad an unmatched `LEFT JOIN`.
    fn width(&self) -> usize {
        self.width
    }
}

/// The bucket a key selects.
///
/// [`HashJoin`] is only built for a same-class, binary-collation key, so this
/// matches on the value's class and no two keys the `ON`'s equality compares
/// equal can land in different buckets.
fn hash_value(value: &Value, collation: Collation) -> u64 {
    match value {
        Value::Integer(integer) => (*integer as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        // Hash what the collation *compares*, not the stored bytes. Under
        // `NOCASE`, `'KEY'` and `'key'` are equal, so they have to land in one
        // bucket or the join would miss the pair entirely — the invariant this
        // whole structure rests on is "equal ⇒ same bucket". `Collation::fold`
        // is the same transform the comparison uses and borrows when it is the
        // identity, so a `BINARY` key still hashes its own bytes with no copy.
        Value::Text(text) => fnv1a(&collation.fold(text.as_bytes())),
        // A `BLOB` is compared byte-wise whatever collation the `ON` resolved —
        // collations apply to text — so it hashes its bytes unfolded.
        Value::Blob(bytes) => fnv1a(bytes),
        // `-0.0 == 0.0` is true, so the two have to land in one bucket even
        // though their bit patterns differ; adding `0.0` normalises the sign
        // of zero and leaves every other value, including infinities,
        // untouched. `NaN` needs no special case here: it is equal to nothing,
        // not even itself, so whichever bucket it lands in the candidate
        // comparison rejects it — a hash may over-group, it may not claim a
        // match. See `hash_join_key` for why a `REAL` key's values are always
        // `Value::Real` by the time they reach this.
        Value::Real(real) => mix64((real + 0.0).to_bits()),
        // A `NULL`/`VECTOR` key never reaches here: `NULL` is handled before
        // the hash is asked, and the planner refuses vectors.
        Value::Null | Value::Vector(_) => 0,
    }
}

/// Whether two join keys are equal under `collation`.
///
/// The bucket only narrows candidates; this is what decides a pair, both for
/// the general path and for the single-equality shortcut that skips
/// re-evaluating the `ON`. It has to agree with `Collation::fold`'s grouping in
/// one direction only: folding may put unequal keys in the same bucket (a
/// collision, or two `RTRIM`-equal texts), and this rejects them. What it must
/// never do is call two keys unequal that the `ON`'s `=` would call equal.
fn keys_equal(outer: &Value, inner: &Value, collation: Collation) -> bool {
    if matches!(outer, Value::Null) || matches!(inner, Value::Null) {
        return false;
    }
    match (outer, inner) {
        (Value::Text(outer), Value::Text(inner)) if !collation.is_binary() => {
            collation.fold(outer.as_bytes()) == collation.fold(inner.as_bytes())
        }
        _ => outer == inner,
    }
}

/// Spread a 64-bit pattern across all of its bits (SplitMix64's finaliser).
///
/// A plain multiply is not enough for an `f64`'s bit pattern, and the bucket
/// index is taken from the *low* bits. A double's mantissa occupies those low
/// bits, and the values an application actually stores — `1.5`, `3.0`, `4.5`,
/// prices, counts scaled by a constant — use only the top few mantissa bits, so
/// their patterns end in long runs of zeros. Multiplying by an odd constant
/// cannot fix that: if the input has *k* trailing zero bits the product still
/// does, so every such key masks down to bucket zero and the hash join
/// degenerates into the linear scan it exists to avoid.
///
/// Measured, before this was here: a 2,000-row `REAL` join built a hash table
/// whose keys all landed in one bucket and ran at 78 ms — indistinguishable
/// from the `Materialise` plan it had just replaced. With the mix, 464 µs.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// FNV-1a, chosen because it is deterministic and `no_std`: the hash only has
/// to be stable and well-spread, not cryptographic.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// The row id a key names, when it names one.
///
/// Only reached for a key [`crate::engine::indexable_probe`] admitted against
/// an `INTEGER` column, so it is an integer or a non-`NaN` real. `None` means
/// no stored row can match: an `INTEGER PRIMARY KEY` is a positive integer, and
/// `eval::comparison` compares numbers as `f64`, so a key that is negative or
/// not integer-valued equals no row id — which is the empty candidate set, not
/// a fallback.
fn row_id_of(key: &Value) -> Option<RowId> {
    match key {
        Value::Integer(key) => RowId::try_from(*key).ok(),
        // `1.0` names row 1, because that is what `=` says about them. The
        // round trip is the exactness test: an `f64` cast to `i64` saturates,
        // so a real too large to be a row id fails it rather than aliasing the
        // largest one.
        Value::Real(real) => {
            let whole = *real as i64;
            if whole as f64 == *real {
                RowId::try_from(whole).ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The outer row being paired, and the scratch it is paired into.
struct OuterRow {
    id: RowId,
    score: Option<f32>,
    /// The outer row's own values, kept so the scratch can be rebuilt.
    values: Vec<Value>,
    /// The outer row's values followed by one inner row: the buffer the `ON`
    /// predicate is evaluated against.
    ///
    /// Reusing it is what takes the per-*pair* cost down to cloning the inner
    /// row alone. The old code built a fresh `Vec` per pair and cloned the
    /// outer row into it whether or not the pair survived, so a selective join
    /// paid for every row it rejected.
    joined: Vec<Value>,
    /// How far into the inner side this outer row has got.
    next: usize,
    /// Whether anything matched, which is what a `LEFT JOIN` pads for.
    matched: bool,
}

/// Nested-loop join: pair every outer row with every inner row the `ON`
/// predicate admits, streaming the outer side.
pub(crate) struct NestedLoopJoin<'a> {
    outer: RowStream<'a>,
    inner: JoinInner<'a>,
    kind: JoinKind,
    on: Option<&'a Expr>,
    env: &'a Env<'a>,
    current: Option<OuterRow>,
    /// Set once the join has failed, so it yields exactly one error.
    failed: bool,
    /// Where a cancelled statement is noticed.
    ///
    /// The outer side is a checked stream already, so this is here for the
    /// *inner* loop: one outer row against a large materialised or hash inner
    /// side runs for as many pairs as that side has rows without ever pulling
    /// from the outer stream, and a cross join is that shape squared. The
    /// check has to be where the pairs are, not where the rows come in.
    interrupt: &'a Interrupt,
}

impl<'a> NestedLoopJoin<'a> {
    /// Join `outer` against `inner`. Nothing is read until the first
    /// [`Iterator::next`].
    pub fn new(
        outer: RowStream<'a>,
        inner: JoinInner<'a>,
        kind: JoinKind,
        on: Option<&'a Expr>,
        env: &'a Env<'a>,
        interrupt: &'a Interrupt,
    ) -> Self {
        Self {
            outer,
            inner,
            kind,
            on,
            env,
            current: None,
            failed: false,
            interrupt,
        }
    }

    /// Start the scratch buffer for a newly pulled outer row.
    fn begin(&mut self, row: ExecRow) -> Result<()> {
        self.inner.prepare(&row.values)?;
        let mut joined = Vec::with_capacity(row.values.len() + self.inner.width());
        joined.extend(row.values.iter().cloned());
        self.current = Some(OuterRow {
            id: row.id,
            score: row.score,
            values: row.values,
            joined,
            next: 0,
            matched: false,
        });
        Ok(())
    }

    /// Consume a join into a borrowed-row callback, reusing the outer row's
    /// value buffer for every candidate it produces.
    ///
    /// The iterator API has to hand ownership of every matching `Vec<Value>`
    /// to its caller, so it cannot reclaim that allocation on the next
    /// `next()`. A callback finishes with the slice before this method resumes:
    /// truncate back to the outer width, append the next inner candidate, and
    /// emit again. SQL filtering and `LEFT JOIN` padding are identical to the
    /// iterator implementation below; only ownership changes.
    pub fn try_for_each_borrowed(
        mut self,
        hash_key_is_full_on: bool,
        mut each: impl FnMut(RowId, Option<f32>, &[Value]) -> Result<bool>,
    ) -> Result<()> {
        for row in self.outer.by_ref() {
            let row = row?;
            self.inner.prepare(&row.values)?;
            let id = row.id;
            let score = row.score;
            let mut joined = row.values;
            let outer_width = joined.len();
            let mut matched = false;

            for index in 0..self.inner.rows().len() {
                self.interrupt.check()?;
                joined.truncate(outer_width);
                self.inner.append_row_into(index, &mut joined);
                let keep = match (hash_key_is_full_on, self.on) {
                    (true, Some(_)) => self
                        .inner
                        .hash_candidate_matches(index, &joined[..outer_width])
                        .unwrap_or(false),
                    (false, Some(on)) => {
                        eval::is_truthy(&eval::evaluate(on, &joined, Computed::NONE, self.env)?)
                    }
                    (_, None) => true,
                };
                if keep {
                    matched = true;
                    if !each(id, score, &joined)? {
                        return Ok(());
                    }
                }
            }

            if !matched && self.kind == JoinKind::Left {
                joined.truncate(outer_width);
                joined.extend(core::iter::repeat_n(Value::Null, self.inner.width()));
                if !each(id, score, &joined)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Consume an exact-key hash join as borrowed outer/inner pairs.
    ///
    /// This is the fused form for a direct-column projection: the hash key is
    /// the complete `ON`, so after rejecting a rare bucket collision there is
    /// no expression that needs a contiguous joined row. The consumer can
    /// address the two slices by ordinal and the operator never clones the
    /// inner row into a temporary joined buffer.
    pub fn try_for_each_hash_pair(
        mut self,
        mut each: impl FnMut(RowId, Option<f32>, &[Value], Option<&[Value]>) -> Result<bool>,
    ) -> Result<()> {
        debug_assert!(self.inner.is_hash());
        for row in self.outer.by_ref() {
            let row = row?;
            self.inner.prepare(&row.values)?;
            let id = row.id;
            let score = row.score;
            let mut matched = false;
            for index in 0..self.inner.rows().len() {
                self.interrupt.check()?;
                if !self
                    .inner
                    .hash_candidate_matches(index, &row.values)
                    .unwrap_or(false)
                {
                    continue;
                }
                matched = true;
                let inner = &self.inner.rows()[index];
                if !each(id, score, &row.values, Some(inner))? {
                    return Ok(());
                }
            }
            if !matched && self.kind == JoinKind::Left && !each(id, score, &row.values, None)? {
                return Ok(());
            }
        }
        Ok(())
    }
}

impl Iterator for NestedLoopJoin<'_> {
    type Item = Result<ExecRow>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.failed {
                return None;
            }
            if self.current.is_none() {
                match self.outer.next()? {
                    Ok(row) => {
                        if let Err(error) = self.begin(row) {
                            self.failed = true;
                            return Some(Err(error));
                        }
                    }
                    Err(error) => {
                        self.failed = true;
                        return Some(Err(error));
                    }
                }
            }

            let count = self.inner.rows().len();
            let outer = self.current.as_mut()?;
            while outer.next < count {
                if let Err(error) = self.interrupt.check() {
                    self.failed = true;
                    return Some(Err(error));
                }
                let index = outer.next;
                outer.next += 1;
                // Truncate rather than reallocate: the outer half is already
                // there and only the inner half changes per pair. The inner
                // values are appended straight into the scratch, not cloned
                // into a temporary row first — see `JoinInner::append_row_into`.
                outer.joined.truncate(outer.values.len());
                self.inner.append_row_into(index, &mut outer.joined);
                let keep = match self.on {
                    Some(on) => match eval::evaluate(on, &outer.joined, Computed::NONE, self.env) {
                        Ok(value) => eval::is_truthy(&value),
                        Err(error) => {
                            self.failed = true;
                            return Some(Err(error));
                        }
                    },
                    None => true,
                };
                if keep {
                    outer.matched = true;
                    let values = core::mem::take(&mut outer.joined);
                    // The scratch was handed to the caller, so rebuild it — one
                    // clone of the outer row per *emitted* row, where the old
                    // code paid one per candidate pair.
                    outer.joined = Vec::with_capacity(values.len());
                    outer.joined.extend(outer.values.iter().cloned());
                    return Some(Ok(ExecRow {
                        id: outer.id,
                        score: outer.score,
                        values,
                        aggregates: Vec::new(),
                        windows: Vec::new(),
                    }));
                }
            }

            // The inner side is exhausted for this outer row.
            let outer = self.current.take()?;
            if !outer.matched && self.kind == JoinKind::Left {
                let mut values = outer.values;
                values.extend(core::iter::repeat_n(Value::Null, self.inner.width()));
                return Some(Ok(ExecRow {
                    id: outer.id,
                    score: outer.score,
                    values,
                    aggregates: Vec::new(),
                    windows: Vec::new(),
                }));
            }
        }
    }
}

#[cfg(test)]
mod hash_key_tests {
    use super::{hash_value, keys_equal};
    use crate::collation::Collation;
    use crate::value::Value;

    /// The invariant the whole hash join rests on, asserted directly rather
    /// than through a join's answer.
    ///
    /// Going through a join cannot test this reliably at a small table size:
    /// the bucket is `hash & (buckets - 1)`, the table has at least 16 buckets,
    /// and FNV-1a's low four bits are invariant under the ASCII case bit — so
    /// `'ada'` and `'ADA'` share a bucket even when the hash does *not* fold
    /// the collation, and a join test only catches the missing fold once the
    /// table is large enough for the case bit to reach the mask. That is a
    /// property of the bucket count, not of the rule being tested. These
    /// assertions hold at any size.
    #[test]
    fn equal_keys_hash_alike() {
        let upper = Value::Text("ADA".into());
        let lower = Value::Text("ada".into());
        assert_eq!(
            hash_value(&upper, Collation::NoCase),
            hash_value(&lower, Collation::NoCase),
            "NOCASE-equal texts must share a bucket"
        );
        assert!(keys_equal(&upper, &lower, Collation::NoCase));

        // ... and must not be forced together under a collation that calls
        // them different, which is what keeps BINARY joins as selective as
        // they were.
        assert!(!keys_equal(&upper, &lower, Collation::Binary));

        let padded = Value::Text("a  ".into());
        let bare = Value::Text("a".into());
        assert_eq!(
            hash_value(&padded, Collation::RTrim),
            hash_value(&bare, Collation::RTrim),
            "RTRIM-equal texts must share a bucket"
        );
        assert!(keys_equal(&padded, &bare, Collation::RTrim));

        // `-0.0 == 0.0` is true, so the two have to hash alike. With the
        // current multiplier they would collide even unnormalised — the sign
        // bit cannot reach the low bits through a multiply — so this assertion
        // is what keeps that true if the hash is ever changed to mix high bits
        // downward, which is exactly when it would silently stop being true.
        assert_eq!(
            hash_value(&Value::Real(-0.0), Collation::Binary),
            hash_value(&Value::Real(0.0), Collation::Binary),
            "-0.0 and 0.0 are equal under `=` and must share a bucket"
        );
        assert!(keys_equal(
            &Value::Real(-0.0),
            &Value::Real(0.0),
            Collation::Binary
        ));
    }

    /// `NaN` is equal to nothing, including itself, so it may hash anywhere as
    /// long as the comparison refuses it. `NULL` likewise never matches.
    #[test]
    fn keys_that_match_nothing_are_refused() {
        let nan = Value::Real(f64::NAN);
        assert!(!keys_equal(&nan, &nan, Collation::Binary));
        assert!(!keys_equal(&Value::Null, &Value::Null, Collation::Binary));
        assert!(!keys_equal(
            &Value::Null,
            &Value::Integer(1),
            Collation::Binary
        ));
    }
}
