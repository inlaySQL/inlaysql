//! `EXPLAIN`: the access path the executor would take, reported without
//! taking it.
//!
//! This engine has several plan shapes that look identical from the outside —
//! a full scan, a row-id point lookup, an index range scan, a hash join, an
//! index nested-loop join, and the three retrieval paths — and until this
//! module existed there was no way to tell which one a query got. That is the
//! whole feature: not "is it fast", which nobody can answer from a plan, but
//! "which of these did the planner pick", which is knowable and true.
//!
//! # What it deliberately does not report
//!
//! **No row counts, no costs, no selectivity.** There is no statistics system
//! here — no `ANALYZE`, no histograms, no cardinality anywhere in the
//! catalog — so every number of that kind would be invented. A fabricated
//! estimate is worse than none: it reads exactly like a measured one, and the
//! first thing anyone does with `rows` is compare it against reality. Every
//! access-path rule in [`crate::engine`] is a *rule* rather than a cost model
//! (`docs/architecture.md`, D6), so there is nothing to report a cost from
//! even in principle.
//!
//! # The output shape, and why this one
//!
//! Three columns — `id`, `parent`, `detail` — which is SQLite's
//! `EXPLAIN QUERY PLAN` shape minus its vestigial `notused`.
//!
//! The alternative was MySQL's column set (`select_type`, `type`,
//! `possible_keys`, `key`, `key_len`, `ref`, `rows`, `filtered`, `Extra`).
//! That was rejected on this repository's own rule: compatibility where it is
//! real, refusal where it is not. Of those columns, `rows`, `filtered` and
//! `key_len` cannot be filled here at all, and a client that read `rows` as a
//! placeholder zero would be misled in exactly the way `docs/server.md`'s
//! metadata rule exists to prevent. Wearing MySQL's column names while
//! filling half of them with constants is the same failure as a statement
//! that parses a clause and discards it.
//!
//! SQLite's shape has none of that problem, because it carries no numbers: it
//! is a tree of one text `detail` per node, which is precisely what this
//! engine can say truthfully. It also *is* a tree — a driving table, its
//! joins, its subqueries, the two arms of a compound — so a flat table would
//! have had to flatten something. `notused` is dropped because it exists only
//! for a legacy sqlite3 ABI and emitting a constant-zero column would be the
//! placeholder this module just refused.
//!
//! The wording of `detail` follows sqlite3's where sqlite3 has one (`SCAN t`,
//! `SEARCH t USING INDEX i (a=?)`), so that anyone who has read an
//! `EXPLAIN QUERY PLAN` reads this without a glossary, and uses this engine's
//! own vocabulary where sqlite3 has nothing to copy — hash joins and the
//! retrieval indexes, neither of which sqlite3 has.
//!
//! # Where the answers come from
//!
//! Every choice reported here is read back from the executor's own chooser —
//! `Engine::choose_index`, `Engine::join_probe`, `hash_join_key`,
//! `pinned_rowid`, `scan_shape` — never re-derived from the same inputs by
//! a second implementation of the same rule. A second implementation would
//! drift, and the way it would drift is the one that matters: an `EXPLAIN`
//! that says `USING INDEX` for a query that in fact scans is worse than no
//! `EXPLAIN` at all, because it ends the investigation.
//!
//! That is also why this runs at execution time rather than at prepare time.
//! The access path depends on the bound parameters (`WHERE id = ?` is a point
//! lookup only when `?` is an integer) and on the catalog as it stands now, so
//! a plan-time description would be describing a different execution than the
//! one the caller is about to get.
//!
//! # What it does not touch
//!
//! No rows are read and nothing is written, for any inner statement. The
//! catalog, the plan and the parameters are the only inputs — see
//! [`crate::plan::Plan::is_read_only`], which reports `EXPLAIN INSERT` as a
//! read.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::catalog::Table;
use crate::engine::{hash_join_key, pinned_rowid, scan_shape, Engine, ResultSet};
use crate::error::Result;
use crate::eval::Env;
use crate::exec::ProbeKind;
use crate::plan::{
    ColumnInfo, DeletePlan, Expr, InsertPlan, InsertSource, JoinKind, Plan, ScoreExpr, SelectItem,
    SelectPlan, SetOperationPlan, SubqueryBody, SubqueryOp, UpdatePlan,
};
use crate::value::{DataType, Value};

/// The headers `EXPLAIN` reports, in output order.
pub const COLUMN_NAMES: [&str; 3] = ["id", "parent", "detail"];

/// `EXPLAIN`'s output columns, as prepare-time metadata.
///
/// Typed, unlike most computed projections: these three columns are this
/// module's own and their storage classes are fixed by the code below, so
/// answering `None` — "the plan does not statically know" — would be the
/// false one of the two answers. A `COM_STMT_PREPARE` over the wire reports
/// them from here.
pub fn columns() -> Vec<ColumnInfo> {
    let ty = [DataType::Integer, DataType::Integer, DataType::Text];
    COLUMN_NAMES
        .iter()
        .zip(ty)
        .map(|(name, ty)| ColumnInfo {
            name: name.to_string(),
            ty: Some(ty),
        })
        .collect()
}

/// Describe `plan` as the rows `EXPLAIN` returns.
pub(crate) fn explain<'e>(
    engine: &'e Engine,
    plan: &Plan,
    params: &'e [Value],
) -> Result<ResultSet> {
    let mut explainer = Explainer {
        engine,
        env: engine.read_env(params),
        rows: Vec::new(),
        next: 1,
    };
    explainer.statement(plan, 0)?;
    Ok(ResultSet {
        columns: COLUMN_NAMES.iter().map(|name| name.to_string()).collect(),
        rows: explainer.rows,
    })
}

/// Builds the node tree, numbering nodes as it goes.
struct Explainer<'e> {
    engine: &'e Engine,
    /// The same environment the executor would run under, which is what makes
    /// `LIMIT ?` and `WHERE id = ?` resolve here exactly as they will there.
    env: Env<'e>,
    rows: Vec<Vec<Value>>,
    next: i64,
}

impl<'e> Explainer<'e> {
    /// Emit one node under `parent` and return its id, so its own children can
    /// name it. Node `0` is the (unemitted) root, which is what a top-level
    /// node's `parent` refers to.
    fn push(&mut self, parent: i64, detail: String) -> i64 {
        let id = self.next;
        self.next += 1;
        self.rows.push(alloc::vec![
            Value::Integer(id),
            Value::Integer(parent),
            Value::Text(detail.into()),
        ]);
        id
    }

    // ------------------------------------------------------------ statements

    fn statement(&mut self, plan: &Plan, parent: i64) -> Result<()> {
        match plan {
            Plan::Select(select) => self.select(select, parent, None),
            Plan::Scalar(_) => {
                // No `FROM`, so no row source to choose: one constant row,
                // which is sqlite3's own wording for it.
                self.push(parent, "SCAN CONSTANT ROW".to_string());
                Ok(())
            }
            Plan::SetOperation(compound) => self.compound(compound, parent, None),
            Plan::Insert(insert) => self.insert(insert, parent),
            Plan::Update(update) => self.update(update, parent),
            Plan::Delete(delete) => self.delete(delete, parent),
            // Refused at plan time (`sql.rs::plan_explain`), so reaching this
            // would mean the refusal and this match had drifted apart.
            other => Err(crate::error::Error::Unsupported(alloc::format!(
                "EXPLAIN cannot describe this statement: {}",
                statement_kind(other)
            ))),
        }
    }

    fn insert(&mut self, plan: &InsertPlan, parent: i64) -> Result<()> {
        let id = self.push(parent, alloc::format!("INSERT INTO {}", plan.table));
        match &plan.source {
            InsertSource::Values(rows) => {
                self.push(id, alloc::format!("VALUES ({} ROW(S))", rows.len()));
            }
            InsertSource::Select { query, .. } => self.body(query, id, None)?,
        }
        Ok(())
    }

    fn update(&mut self, plan: &UpdatePlan, parent: i64) -> Result<()> {
        let id = self.push(parent, alloc::format!("UPDATE {}", plan.table));
        self.write_candidates(&plan.table, plan.filter.as_ref(), id)
    }

    fn delete(&mut self, plan: &DeletePlan, parent: i64) -> Result<()> {
        let id = self.push(parent, alloc::format!("DELETE FROM {}", plan.table));
        self.write_candidates(&plan.table, plan.filter.as_ref(), id)
    }

    /// The rows a write statement reads before it changes them, which go
    /// through [`Engine::candidate_bytes`] exactly as a reader's do — so
    /// "did my `DELETE` use an index" is the same question, with the same
    /// answer, as it is for the matching `SELECT`.
    fn write_candidates(&mut self, table: &str, filter: Option<&Expr>, parent: i64) -> Result<()> {
        let Some(table) = self.engine.catalog().table(table) else {
            // Unreachable for a plan that resolved: the planner looked this
            // table up a moment ago.
            return Ok(());
        };
        let detail = self.stored_access(table, filter)?;
        self.push(parent, detail);
        Ok(())
    }

    // --------------------------------------------------------------- queries

    /// `cap` is the caller's own row budget, threaded exactly as
    /// [`Engine::run_body`]'s is: `EXISTS` and a scalar subquery want one row,
    /// and that budget reaches the access-path choice through
    /// [`ScanShape::full_scan`]. Reporting a hash join for an `EXISTS`
    /// subquery that in fact probes would be the drift this module exists to
    /// rule out.
    fn body(&mut self, body: &SubqueryBody, parent: i64, cap: Option<usize>) -> Result<()> {
        match body {
            SubqueryBody::Select(plan) => self.select(plan, parent, cap),
            SubqueryBody::Scalar(_) => {
                self.push(parent, "SCAN CONSTANT ROW".to_string());
                Ok(())
            }
            SubqueryBody::SetOp(plan) => self.compound(plan, parent, cap),
        }
    }

    /// Both arms of a compound run to completion before a single output row
    /// can be decided (`Engine::run_set_operation`), so they are siblings
    /// rather than a pipeline and neither one's `LIMIT` bounds the other.
    fn compound(
        &mut self,
        plan: &SetOperationPlan,
        parent: i64,
        _cap: Option<usize>,
    ) -> Result<()> {
        let id = self.push(parent, alloc::format!("COMPOUND QUERY ({})", plan.op));
        // Neither arm inherits the caller's budget: deduplication and set
        // membership both need every row either arm produced, so
        // `Engine::run_set_operation` runs both with no cap and applies the
        // budget to the combined result.
        self.body(&plan.left, id, None)?;
        self.body(&plan.right, id, None)
    }

    fn select(&mut self, plan: &SelectPlan, parent: i64, cap: Option<usize>) -> Result<()> {
        let shape = scan_shape(plan, &self.env, cap)?;
        let driving = &plan.from[0];

        // The driving table: the one a retrieval index, a point lookup or a
        // range probe can answer for. Everything after it is a join.
        let detail = match (&driving.derived, &plan.score) {
            (Some(_), _) => alloc::format!("SCAN {} (SUBQUERY, MATERIALISED)", driving.table.name),
            (None, Some(score)) => {
                let mut detail = self.score_access(&driving.table, score)?;
                if plan.filter.is_some() {
                    // `Engine::retrieve_filtered`: the `WHERE` runs inside the
                    // index walk, so a selective filter keeps searching rather
                    // than under-filling a fixed candidate budget.
                    detail.push_str(" (WHERE PUSHED INTO RETRIEVAL)");
                }
                detail
            }
            (None, None) => self.stored_access(&driving.table, plan.filter.as_ref())?,
        };
        let first = self.push(parent, detail);
        if let Some(body) = &driving.derived {
            // A derived table is materialised in full before the outer
            // pipeline starts, so the outer `LIMIT` does not reach it.
            self.body(body, first, None)?;
        }
        if let (None, Some(score)) = (&driving.derived, &plan.score) {
            self.score_children(&driving.table, score, first)?;
        }

        // Joins, in the order the executor nests them: `from[i + 1]` is the
        // inner side of `joins[i]`, and `offset_of` tracks where its columns
        // begin in the joined row, which is what the plan's ordinals are held
        // against.
        let mut offset_of = driving.table.columns.len();
        for (index, join) in plan.joins.iter().enumerate() {
            let inner = &plan.from[index + 1];
            let side = if inner.derived.is_some() {
                // A derived table has no index and no declared column classes
                // to hash on, so the probe chooser is not even consulted for
                // it — it is materialised, once, and replayed per outer row.
                alloc::format!(
                    "NESTED LOOP JOIN {} (SUBQUERY, MATERIALISED)",
                    inner.table.name
                )
            } else {
                self.join_side(plan, index, offset_of, shape.full_scan)
            };
            let detail = match join.kind {
                JoinKind::Left => alloc::format!("LEFT {side}"),
                JoinKind::Inner => side,
            };
            let id = self.push(parent, detail);
            if let Some(body) = &inner.derived {
                self.body(body, id, None)?;
            }
            offset_of += inner.table.columns.len();
        }

        // What happens after the rows are joined. Each of these has to see the
        // whole input before it can emit a row, which is exactly why they are
        // worth reporting: they are where a query stops streaming.
        if !plan.group_by.is_empty() {
            self.push(parent, "SORT FOR GROUP BY".to_string());
        }
        if !plan.windows.is_empty() {
            self.push(
                parent,
                alloc::format!("EVALUATE {} WINDOW FUNCTION(S)", plan.windows.len()),
            );
        }
        if plan.distinct {
            self.push(parent, "FOLD FOR DISTINCT".to_string());
        }
        if !plan.order.is_empty() {
            self.push(parent, "SORT FOR ORDER BY".to_string());
        }
        // Whether the `LIMIT` ends the scan or only truncates the answer: the
        // difference between reading ten rows and reading the table.
        if plan.limit.is_some() {
            let detail = match shape.stop_after {
                Some(rows) => alloc::format!("LIMIT {rows} PUSHED INTO SCAN"),
                None => "LIMIT APPLIED AFTER MATERIALISING".to_string(),
            };
            self.push(parent, detail);
        }

        self.subqueries(plan, parent)
    }

    // ---------------------------------------------------------- access paths

    /// How the rows of one stored table are reached, given the filter that
    /// applies to it — the three paths [`Engine::candidate_bytes`] chooses
    /// between, asked in the same order it asks them: point, index, scan.
    fn stored_access(&self, table: &Table, filter: Option<&Expr>) -> Result<String> {
        let params = self.env.params();
        if pinned_rowid(table, filter, params).is_some() {
            return Ok(alloc::format!(
                "SEARCH {} USING INTEGER PRIMARY KEY (rowid=?)",
                table.name
            ));
        }
        match self.engine.choose_index(table, filter, params)? {
            Some((index, probe)) => {
                let mut key = String::new();
                for position in 0..probe.equalities {
                    if !key.is_empty() {
                        key.push_str(" AND ");
                    }
                    key.push_str(&index.columns[position]);
                    key.push_str("=?");
                }
                // At most one column past the equalities is bounded, and it may
                // be bounded on either side or both. `>?` for a `>=` is
                // sqlite3's own shorthand: the walk covers the whole group of
                // entries that encode equal to the bound either way, and the
                // filter rejects what does not belong.
                if probe.lower || probe.upper {
                    let column = &index.columns[probe.equalities];
                    for (applies, op) in [(probe.lower, '>'), (probe.upper, '<')] {
                        if applies {
                            if !key.is_empty() {
                                key.push_str(" AND ");
                            }
                            key.push_str(column);
                            key.push(op);
                            key.push('?');
                        }
                    }
                }
                Ok(alloc::format!(
                    "SEARCH {} USING INDEX {} ({key})",
                    table.name,
                    index.name
                ))
            }
            None => Ok(alloc::format!("SCAN {}", table.name)),
        }
    }

    /// Which retrieval index answers one leaf of a `bm25_score`/`vector_score`/
    /// `fuse` expression. Resolved through the engine's own resolvers, so a
    /// query with no matching index fails here with the same error it would
    /// have failed with when run — an `EXPLAIN` that described a plan the
    /// engine refuses would be describing nothing.
    fn score_access(&self, table: &Table, score: &ScoreExpr) -> Result<String> {
        Ok(match score {
            ScoreExpr::Text { columns, .. } => {
                let index = self.engine.resolve_full_text_index(table, columns)?;
                alloc::format!(
                    "SEARCH {} USING FULL-TEXT INDEX {} ({}) FOR bm25_score",
                    table.name,
                    index.name,
                    index.columns.join(", ")
                )
            }
            ScoreExpr::Vector { column, .. } => {
                let index = self.engine.resolve_vector_index(table, *column)?;
                alloc::format!(
                    "SEARCH {} USING VECTOR INDEX {} ({}) FOR vector_score",
                    table.name,
                    index.name,
                    index.columns.join(", ")
                )
            }
            ScoreExpr::Fuse { parts, k } => alloc::format!(
                "FUSE {} RANKED LIST(S) BY RECIPROCAL RANK (k={k})",
                parts.len()
            ),
        })
    }

    /// A `fuse(...)`'s children — one node per ranked list it combines, which
    /// is the only way to see that a hybrid query really did run both
    /// retrievers.
    fn score_children(&mut self, table: &Table, score: &ScoreExpr, parent: i64) -> Result<()> {
        let ScoreExpr::Fuse { parts, .. } = score else {
            return Ok(());
        };
        for part in parts {
            let detail = self.score_access(table, part)?;
            let id = self.push(parent, detail);
            self.score_children(table, part, id)?;
        }
        Ok(())
    }

    /// Which of the three inner-side strategies one join gets. This mirrors
    /// [`Engine::join_inner`] arm for arm, and asks the same two functions in
    /// the same order — a hash key first, but only under a full scan, then an
    /// index probe, then the materialising fallback.
    fn join_side(
        &self,
        plan: &SelectPlan,
        join_index: usize,
        offset_of: usize,
        full_scan: bool,
    ) -> String {
        let inner_index = join_index + 1;
        let inner = &plan.from[inner_index].table;
        let on = plan.joins[join_index].on.as_ref();

        if full_scan {
            if let Some((key, _)) = hash_join_key(&plan.from, inner_index, offset_of, on) {
                return alloc::format!(
                    "HASH JOIN {} (BUILD ON {}.{})",
                    inner.name,
                    inner.name,
                    inner.columns[key.inner].name
                );
            }
        }

        match self.engine.join_probe(inner, offset_of, on) {
            Some((_, _, _, ProbeKind::RowId)) => alloc::format!(
                "INDEX NESTED LOOP JOIN {} USING INTEGER PRIMARY KEY (rowid=?)",
                inner.name
            ),
            Some((_, _, _, ProbeKind::Index(name))) => {
                // The probe reads the run of entries whose *leading* column
                // equals the key; nothing past it is contiguous, so that one
                // column is the whole of what the index answers here.
                let leading = self
                    .engine
                    .catalog()
                    .indexes_for(&inner.name)
                    .into_iter()
                    .find(|index| index.name == name)
                    .and_then(|index| index.columns.first().cloned())
                    .unwrap_or_else(|| "?".to_string());
                alloc::format!(
                    "INDEX NESTED LOOP JOIN {} USING INDEX {name} ({leading}=?)",
                    inner.name
                )
            }
            None => alloc::format!(
                "NESTED LOOP JOIN {} (MATERIALISED: no index or hash key applies)",
                inner.name
            ),
        }
    }

    // ------------------------------------------------------------ subqueries

    /// Every subquery in an expression position, and — the part worth
    /// reporting — whether it is correlated. An uncorrelated subquery is
    /// evaluated once per statement and memoised by
    /// [`crate::plan::Subquery::id`]; a correlated one runs once per outer
    /// row, which is the difference between a constant and a nested loop.
    fn subqueries(&mut self, plan: &SelectPlan, parent: i64) -> Result<()> {
        let mut found = Vec::new();
        for item in &plan.items {
            if let SelectItem::Expr { expr, .. } = item {
                collect_subqueries(expr, &mut found);
            }
        }
        for join in &plan.joins {
            if let Some(on) = &join.on {
                collect_subqueries(on, &mut found);
            }
        }
        if let Some(filter) = &plan.filter {
            collect_subqueries(filter, &mut found);
        }
        if let Some(having) = &plan.having {
            collect_subqueries(having, &mut found);
        }

        for expr in found {
            let Expr::Subquery { op, query } = expr else {
                continue;
            };
            // The row budget each shape asks for, matching `eval.rs`'s three
            // `subquery_rows` call sites: a scalar reads the first row and an
            // `EXISTS` asks only whether there is one, while `IN` needs the
            // whole candidate set.
            let (kind, cap) = match op {
                SubqueryOp::Scalar => ("SCALAR SUBQUERY", Some(1)),
                SubqueryOp::Exists { .. } => ("EXISTS SUBQUERY", Some(1)),
                SubqueryOp::In { .. } => ("LIST SUBQUERY", None),
            };
            let detail = if query.captures.is_empty() {
                alloc::format!("{kind} {} (RUN ONCE)", query.id)
            } else {
                alloc::format!("CORRELATED {kind} {} (RUN PER ROW)", query.id)
            };
            let id = self.push(parent, detail);
            self.body(&query.body, id, cap)?;
        }
        Ok(())
    }
}

/// Collect the subqueries one expression holds, outermost first.
///
/// Deliberately does *not* descend into a subquery's own body: that body is
/// described by its own recursive call, where its own nodes get their own
/// parent. Descending here too would list a nested subquery twice, once under
/// the wrong parent.
fn collect_subqueries<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::Subquery { op, .. } => {
            // The `IN` probe is evaluated in the *enclosing* scope, so a
            // subquery inside it belongs to this level, not to the body.
            if let SubqueryOp::In { probe, .. } = op {
                collect_subqueries(probe, out);
            }
            out.push(expr);
        }
        Expr::Literal(_)
        | Expr::Param(_)
        | Expr::Column(_)
        | Expr::Outer(_)
        | Expr::Agg(_)
        | Expr::Window(_) => {}
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::Collate { expr, .. } => {
            collect_subqueries(expr, out)
        }
        Expr::Binary { left, right, .. } => {
            collect_subqueries(left, out);
            collect_subqueries(right, out);
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            collect_subqueries(expr, out);
            collect_subqueries(pattern, out);
            if let Some(escape) = escape {
                collect_subqueries(escape, out);
            }
        }
        Expr::InList { expr, list, .. } => {
            collect_subqueries(expr, out);
            for item in list {
                collect_subqueries(item, out);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_subqueries(expr, out);
            collect_subqueries(low, out);
            collect_subqueries(high, out);
        }
        Expr::Case {
            operand,
            branches,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                collect_subqueries(operand, out);
            }
            for (when, then) in branches {
                collect_subqueries(when, out);
                collect_subqueries(then, out);
            }
            if let Some(else_result) = else_result {
                collect_subqueries(else_result, out);
            }
        }
        Expr::Func { args, .. } => {
            for arg in args {
                collect_subqueries(arg, out);
            }
        }
    }
}

/// How a statement `EXPLAIN` cannot describe is named in the refusal. Only
/// reachable if [`crate::sql`]'s refusal list and [`Explainer::statement`]
/// ever disagree, which is why it names the shape rather than saying
/// "unsupported".
fn statement_kind(plan: &Plan) -> &'static str {
    match plan {
        Plan::CreateTable(_) => "CREATE TABLE",
        Plan::DropTable(_) => "DROP TABLE",
        Plan::AlterTable(_) => "ALTER TABLE",
        Plan::CreateIndex(_) | Plan::CreateUniqueIndex(_) => "CREATE INDEX",
        Plan::DropIndex(_) => "DROP INDEX",
        Plan::Begin => "BEGIN",
        Plan::Commit => "COMMIT",
        Plan::Rollback => "ROLLBACK",
        Plan::Explain(_) => "EXPLAIN",
        Plan::Select(_)
        | Plan::Scalar(_)
        | Plan::SetOperation(_)
        | Plan::Insert(_)
        | Plan::Update(_)
        | Plan::Delete(_) => "this statement",
    }
}
