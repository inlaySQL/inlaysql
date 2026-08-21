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
use alloc::string::String;
use alloc::vec::Vec;

use crate::collation::Collation;
use crate::error::Result;
use crate::eval::{self, Computed, Env};
use crate::index::KeyRange;
use crate::plan::{Expr, JoinKind};
use crate::row::{decode_row_masked, decode_row_ref_masked, ColumnMask, RowBuf};
use crate::traits::{RowId, RowScan, Storage};
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
    },
}

impl<'a> RowBytes<'a> {
    /// The rows `ids` name, in the order given, read lazily.
    ///
    /// A row id with no row is skipped rather than reported: an index entry
    /// that outlives its row would be a maintenance bug, and the scan path
    /// would not have seen the row either, so skipping keeps the two paths
    /// answering the same thing.
    pub fn indexed(storage: &'a dyn Storage, table: &str, ids: Vec<RowId>) -> Self {
        RowBytes::Indexed {
            storage,
            table: alloc::string::String::from(table),
            ids: ids.into_iter(),
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
            } => loop {
                let id = ids.next()?;
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
        }
    }

    /// The candidates [`JoinInner::prepare`] left.
    fn rows(&self) -> &[Vec<Value>] {
        match self {
            JoinInner::Materialised { rows, .. } => rows,
            JoinInner::Probe(probe) => probe.rows(),
        }
    }

    /// Take ownership of one candidate, cloning only when the row has to
    /// survive to be paired again.
    ///
    /// A materialised side (and a probe's table-scan fallback) is replayed
    /// once per outer row, so this clones, exactly as reading through
    /// [`JoinInner::rows`] and cloning always has. A probe's matched rows are
    /// rebuilt fresh by [`JoinInner::prepare`] for every outer row and never
    /// read again after this outer row's pairing loop moves past them — they
    /// were already decoded once, by `IndexProbe::fetch`, so cloning them a
    /// second time into the pairing buffer was pure waste. This takes instead:
    /// one decode per row, not one decode plus one clone.
    fn take_row(&mut self, index: usize) -> Vec<Value> {
        match self {
            JoinInner::Materialised { rows, .. } => rows[index].clone(),
            JoinInner::Probe(probe) => probe.take_row(index),
        }
    }

    /// How many columns an unmatched `LEFT JOIN` row is padded with.
    fn width(&self) -> usize {
        match self {
            JoinInner::Materialised { width, .. } => *width,
            JoinInner::Probe(probe) => probe.width,
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
            for row in RowScan::new(self.storage, &self.table) {
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
    ) -> Self {
        Self {
            outer,
            inner,
            kind,
            on,
            env,
            current: None,
            failed: false,
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
                let index = outer.next;
                outer.next += 1;
                // Truncate rather than reallocate: the outer half is already
                // there and only the inner half changes per pair. The inner
                // half is taken, not cloned — see `JoinInner::take_row` for
                // why that is sound here.
                outer.joined.truncate(outer.values.len());
                let inner = self.inner.take_row(index);
                outer.joined.extend(inner);
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
