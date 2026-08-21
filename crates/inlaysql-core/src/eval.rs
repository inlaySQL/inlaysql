//! Scalar expression evaluation, with SQLite's semantics.
//!
//! This is the expression evaluator behind every predicate and projected
//! expression: literals, unary minus, arithmetic, comparison, `LIKE`, `IN`,
//! `BETWEEN`, `CASE`, `CAST` and `||`. The rules follow SQLite:
//!
//! * `NULL` propagates through arithmetic and comparisons (`1 + NULL` is
//!   `NULL`, `1 > NULL` is `NULL`).
//! * `+`, `-`, `*` stay integer when both operands are integers and widen to
//!   real otherwise; `/` is integer division when both operands are integers.
//! * Division and modulo by zero yield `NULL`, not an error.
//! * Comparison yields integer `1` or `0` (or `NULL`).
//! * `LIKE` is case-insensitive over ASCII `A`–`Z` and case-sensitive
//!   everywhere else — the quirk, not an accident.
//! * `IN` is three-valued in both directions: `NULL IN (1,2)` is `NULL`,
//!   `1 IN (1,NULL)` is `1`, `1 IN (2,NULL)` is `NULL`, and an empty list is
//!   `0` even for `NULL`.
//! * A value used as a truth value is read the way SQLite reads it: text and
//!   blobs convert to a number first, so `'abc'` is false and `'1x'` is true.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use core::cmp::Ordering;

use crate::collation::Collation;
use crate::error::{Error, Result};
use crate::json::{self, Json, PutMode};
use crate::plan::{
    AggFunc, Aggregate, BinaryOp, CastType, CompareAffinity, Expr, ScalarFunc, Subquery,
    SubqueryBody, SubqueryOp, UnaryOp,
};
use crate::traits::Rng;
use crate::value::{Value, ValueRef};

/// Everything an expression needs that is not the row it is evaluated over.
///
/// The parameter binding, the time, and the source of randomness. The last two
/// are the reason this type exists at all: `inlaysql-core` is `no_std` and
/// cannot read a clock or draw a random number, so `datetime('now')` and
/// `random()` have to reach the evaluator through the injected
/// [`crate::traits::Clock`] and [`crate::traits::Rng`] rather than through the
/// host. Anything else would break the deterministic simulation, where the
/// whole point is that a workload replays byte for byte.
///
/// `now_micros` is captured **once per statement**, not once per row. SQLite
/// does the same (`sqlite3StmtCurrentTime` caches for the statement), so a
/// query that reads `'now'` in two places sees one instant.
pub struct Env<'a> {
    params: &'a [Value],
    /// Microseconds since the Unix epoch, as the injected clock reported them
    /// when the statement started.
    now_micros: i64,
    /// Shared rather than borrowed so that an environment can be built while
    /// the engine holding the generator is borrowed mutably — which is every
    /// write statement.
    rng: Rc<RefCell<Box<dyn Rng>>>,
    /// The values the enclosing query captured for this subquery, read by
    /// [`Expr::Outer`]. Empty at the top level and for any uncorrelated
    /// subquery.
    outer: &'a [Value],
    /// Who runs a subquery. `None` outside the read path, where a subquery is
    /// refused at plan time rather than half-evaluated here.
    runner: Option<&'a dyn SubqueryRunner>,
    /// Uncorrelated subquery results, by [`Subquery::id`], shared with every
    /// nested environment so that one statement evaluates each of them once.
    memo: SubqueryMemo,
}

/// The generator an [`Env`] draws from, shared with whoever injected it.
pub type SharedRng = Rc<RefCell<Box<dyn Rng>>>;

/// The rows one evaluation of a subquery produced.
///
/// Behind an [`Rc`] because an uncorrelated subquery's rows are handed to every
/// outer row that asks, and copying an `IN (SELECT ...)` list per row is the
/// cost this memo exists to remove.
pub type SubqueryRows = Rc<Vec<Vec<Value>>>;

/// Per-statement cache of uncorrelated subquery results.
type SubqueryMemo = Rc<RefCell<BTreeMap<usize, SubqueryRows>>>;

/// Runs the query inside a subquery.
///
/// The evaluator cannot: it has a row and an environment, not a storage
/// backend. [`crate::Engine`] implements this and hands itself to the
/// environment on the read path, which is what makes a subquery re-entrant into
/// the executor.
pub trait SubqueryRunner {
    /// Run `body` under `env` — whose [`Env::outer`] already holds this
    /// subquery's captured values — and return its rows.
    ///
    /// `max_rows` is a hint, not a filter: `EXISTS` and a scalar subquery need
    /// only the first row, and passing that down lets the inner pipeline stop
    /// there. An implementation that ignores it is still correct.
    fn run(
        &self,
        body: &SubqueryBody,
        env: &Env<'_>,
        max_rows: Option<usize>,
    ) -> Result<Vec<Vec<Value>>>;
}

impl<'a> Env<'a> {
    /// Build an environment for one statement execution.
    pub fn new(params: &'a [Value], now_micros: i64, rng: SharedRng) -> Self {
        Self {
            params,
            now_micros,
            rng,
            outer: &[],
            runner: None,
            memo: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }

    /// Let this environment evaluate subqueries, through `runner`.
    ///
    /// Only the read path does this. A write statement's environment is built
    /// while the engine is about to be borrowed mutably, so it cannot hold a
    /// shared borrow of the engine — which is why a subquery in an `UPDATE`,
    /// `DELETE` or `INSERT ... VALUES` is refused in the planner instead.
    pub fn with_subqueries(mut self, runner: &'a dyn SubqueryRunner) -> Self {
        self.runner = Some(runner);
        self
    }

    /// The environment a subquery runs in: the same parameters, clock,
    /// generator, runner and memo, with `outer` as its captured row.
    pub fn nested<'b>(&'b self, outer: &'b [Value]) -> Env<'b>
    where
        'a: 'b,
    {
        Env {
            params: self.params,
            now_micros: self.now_micros,
            rng: Rc::clone(&self.rng),
            outer,
            runner: self.runner,
            memo: Rc::clone(&self.memo),
        }
    }

    /// The bound parameters, which the planner's index-probe rules also read.
    pub fn params(&self) -> &[Value] {
        self.params
    }

    /// The next pseudo-random word, from the injected generator.
    fn next_u64(&self) -> u64 {
        self.rng.borrow_mut().next_u64()
    }
}

/// The per-row values already folded for this statement, that [`Expr::Agg`]
/// and [`Expr::Window`] read from rather than being evaluated afresh: an
/// aggregate is folded once per group and a window function once per frame,
/// never once per reference.
///
/// A plain pair of slices rather than two separate parameters threaded
/// through every evaluator function, so that adding [`Expr::Window`] beside
/// [`Expr::Agg`] touched one type instead of the signature of everything
/// that forwards it. `Copy` because it is just two borrows.
#[derive(Debug, Clone, Copy)]
pub struct Computed<'a> {
    /// Aligned with the plan's aggregate list; empty outside an aggregate
    /// query.
    pub aggregates: &'a [Value],
    /// Aligned with the plan's window-function list; empty until the
    /// executor's window stage has run, and always empty for a query with no
    /// window functions.
    pub windows: &'a [Value],
}

impl Computed<'_> {
    /// Neither an aggregate nor a window value is available — the row has not
    /// reached either stage, or the query has neither. Every expression this
    /// is handed is one [`Expr::Agg`]/[`Expr::Window`] cannot legally appear
    /// in, which the planner enforces the same way it refuses one in `WHERE`.
    pub const NONE: Computed<'static> = Computed {
        aggregates: &[],
        windows: &[],
    };

    /// Aggregate values with no window values alongside them — the shape
    /// every call site had before [`Expr::Window`] existed.
    pub fn aggregates(aggregates: &[Value]) -> Computed<'_> {
        Computed {
            aggregates,
            windows: &[],
        }
    }
}

/// Evaluate a scalar expression against a row and an environment.
///
/// Column references read from `row` by ordinal; a `SELECT` without `FROM`
/// passes an empty row and never produces an [`Expr::Column`]. `?` placeholders
/// read from `env` by position — a plan keeps them unresolved so that one
/// plan can serve many bindings. [`Expr::Agg`]/[`Expr::Window`] references
/// read from `computed`, which the executor fills from the current group or
/// frame; pass [`Computed::NONE`] for a plain row.
pub fn evaluate(
    expr: &Expr,
    row: &[Value],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<Value> {
    let params = env.params;
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Param(index) => params.get(*index).cloned().ok_or_else(|| {
            Error::Bind(alloc::format!(
                "statement needs a parameter at position {}, but only {} were bound",
                index + 1,
                params.len()
            ))
        }),
        Expr::Column(index) => row.get(*index).cloned().ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "expression references column {index}, but the row has {} value(s)",
                row.len()
            ))
        }),
        Expr::Outer(index) => env.outer.get(*index).cloned().ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "a subquery reads outer value {index}, but only {} were captured",
                env.outer.len()
            ))
        }),
        Expr::Subquery { op, query } => subquery(op, query, row, computed, env),
        Expr::Agg(index) => computed.aggregates.get(*index).cloned().ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "expression references aggregate {index}, but only {} were computed",
                computed.aggregates.len()
            ))
        }),
        Expr::Window(index) => computed.windows.get(*index).cloned().ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "expression references window function {index}, but only {} were computed",
                computed.windows.len()
            ))
        }),
        Expr::Unary { op, expr } => {
            let value = evaluate(expr, row, computed, env)?;
            match op {
                UnaryOp::Neg => negate(value),
                // Three-valued, so `NOT` of an unknown stays unknown. Getting
                // this wrong is how a `WHERE` clause and its negation stop
                // partitioning a table.
                UnaryOp::Not => Ok(logical_not(&value)),
                UnaryOp::IsNull => Ok(Value::Integer(i64::from(value == Value::Null))),
                UnaryOp::IsNotNull => Ok(Value::Integer(i64::from(value != Value::Null))),
            }
        }
        Expr::Binary {
            op,
            left,
            right,
            collation,
            affinity,
        } => {
            let left = evaluate(left, row, computed, env)?;
            let right = evaluate(right, row, computed, env)?;
            match op {
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                    arithmetic(*op, left, right)
                }
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq => comparison(*op, left, right, *collation, *affinity),
                BinaryOp::And => Ok(logical_and(left, right)),
                BinaryOp::Or => Ok(logical_or(left, right)),
                BinaryOp::Concat => concat(left, right),
                BinaryOp::JsonExtractJson | BinaryOp::JsonExtractText => {
                    json_arrow(*op, &left, &right)
                }
            }
        }
        Expr::Like {
            negated,
            expr,
            pattern,
            escape,
        } => {
            let value = evaluate(expr, row, computed, env)?;
            let pattern = evaluate(pattern, row, computed, env)?;
            // SQLite validates the escape before it looks at the operands, so
            // a bad escape is an error even when the operands are `NULL`.
            let escape = match escape {
                Some(escape) => match evaluate(escape, row, computed, env)? {
                    Value::Null => return Ok(Value::Null),
                    other => Some(single_escape_char(&other)?),
                },
                None => None,
            };
            if value == Value::Null || pattern == Value::Null {
                return Ok(Value::Null);
            }
            let matched = like_matches(&as_text(&pattern)?, &as_text(&value)?, escape);
            Ok(Value::Integer(i64::from(matched != *negated)))
        }
        Expr::InList {
            negated,
            expr,
            list,
            collation,
            affinity,
        } => {
            // An empty list is false whatever is on the left — `NULL` included.
            // SQLite is explicit about this and it is the one place `IN` is
            // not three-valued.
            if list.is_empty() {
                return Ok(Value::Integer(i64::from(*negated)));
            }
            let value = evaluate(expr, row, computed, env)?;
            if value == Value::Null {
                return Ok(Value::Null);
            }
            let mut saw_null = false;
            for candidate in list {
                let candidate = evaluate(candidate, row, computed, env)?;
                if candidate == Value::Null {
                    saw_null = true;
                    continue;
                }
                if comparison(
                    BinaryOp::Eq,
                    value.clone(),
                    candidate,
                    *collation,
                    *affinity,
                )? == Value::Integer(1)
                {
                    return Ok(Value::Integer(i64::from(!*negated)));
                }
            }
            // A `NULL` in the list means "there might have been a match we
            // could not see", so a miss is unknown rather than false.
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Integer(i64::from(*negated))
            })
        }
        Expr::Between {
            negated,
            expr,
            low,
            high,
            low_collation,
            high_collation,
            low_affinity,
            high_affinity,
        } => {
            let value = evaluate(expr, row, computed, env)?;
            let low = evaluate(low, row, computed, env)?;
            let high = evaluate(high, row, computed, env)?;
            let lower = comparison(
                BinaryOp::GtEq,
                value.clone(),
                low,
                *low_collation,
                *low_affinity,
            )?;
            let upper = comparison(BinaryOp::LtEq, value, high, *high_collation, *high_affinity)?;
            let within = logical_and(lower, upper);
            Ok(if *negated {
                logical_not(&within)
            } else {
                within
            })
        }
        Expr::Case {
            operand,
            branches,
            else_result,
            branch_collations,
            branch_affinities,
        } => {
            let operand = match operand {
                Some(operand) => Some(evaluate(operand, row, computed, env)?),
                None => None,
            };
            for (nth, (condition, result)) in branches.iter().enumerate() {
                let condition = evaluate(condition, row, computed, env)?;
                let matched = match &operand {
                    // `CASE x WHEN v` compares with `=`, so a `NULL` operand
                    // matches nothing — not even `WHEN NULL`. Each branch has
                    // its own collation and affinity, resolved against that
                    // branch's `WHEN`.
                    Some(operand) => {
                        comparison(
                            BinaryOp::Eq,
                            operand.clone(),
                            condition,
                            crate::collation::at(branch_collations, nth),
                            branch_affinities
                                .get(nth)
                                .copied()
                                .unwrap_or(CompareAffinity::None),
                        )? == Value::Integer(1)
                    }
                    None => is_truthy(&condition),
                };
                if matched {
                    return evaluate(result, row, computed, env);
                }
            }
            match else_result {
                Some(result) => evaluate(result, row, computed, env),
                // No branch and no `ELSE` is `NULL`, not an error.
                None => Ok(Value::Null),
            }
        }
        Expr::Cast { expr, to } => {
            let value = evaluate(expr, row, computed, env)?;
            cast(value, *to)
        }
        // `COLLATE` is a plan-time annotation, not a run-time operation: it
        // changed which collation the comparisons above resolved, and here it
        // is the identity.
        Expr::Collate { expr, .. } => evaluate(expr, row, computed, env),
        Expr::Func {
            func,
            args,
            collation,
        } => call(*func, args, row, computed, env, *collation),
    }
}

// ---------------------------------------------------------- borrowed rows

/// Evaluate a scalar expression against a row of borrowed cells.
///
/// The borrowed counterpart of [`evaluate`], for the executor's filter stage
/// (`AHL-478`; `PERF.md`'s "structural fix"). It covers exactly the
/// sublanguage a hot `WHERE`/`ON` predicate is built from — comparisons,
/// `AND`/`OR`, `NOT`, `IS [NOT] NULL`, a bare column, a literal, a parameter,
/// an outer (correlated-subquery) reference — so that a comparison whose
/// operand is a plain column reference never materialises the cell it reads:
/// [`compare_operands`] compares [`ValueRef`]s directly. Everything else —
/// `LIKE`, `IN`, `BETWEEN`, `CASE`, arithmetic, `||`, scalar functions,
/// subqueries, computed — needs either to build genuinely new data or
/// machinery a borrowed row cannot help with, so it materialises the row
/// once and falls back to [`evaluate`], which computes the identical answer.
///
/// A row that this rejects therefore never allocates for its own sake: the
/// only allocation on the fast path is [`Value::clone`] of a literal or a
/// bound parameter, exactly as many bytes as [`evaluate`] already clones for
/// them today.
pub fn evaluate_ref<'r>(
    expr: &Expr,
    row: &[ValueRef<'r>],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<Value> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Param(index) => env.params.get(*index).cloned().ok_or_else(|| {
            Error::Bind(alloc::format!(
                "statement needs a parameter at position {}, but only {} were bound",
                index + 1,
                env.params.len()
            ))
        }),
        Expr::Column(index) => row
            .get(*index)
            .map(ValueRef::to_owned_value)
            .ok_or_else(|| {
                Error::Corrupt(alloc::format!(
                    "expression references column {index}, but the row has {} value(s)",
                    row.len()
                ))
            }),
        Expr::Outer(index) => env.outer.get(*index).cloned().ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "a subquery reads outer value {index}, but only {} were captured",
                env.outer.len()
            ))
        }),
        Expr::Agg(index) => computed.aggregates.get(*index).cloned().ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "expression references aggregate {index}, but only {} were computed",
                computed.aggregates.len()
            ))
        }),
        Expr::Window(index) => computed.windows.get(*index).cloned().ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "expression references window function {index}, but only {} were computed",
                computed.windows.len()
            ))
        }),
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => Ok(logical_not(&evaluate_ref(expr, row, computed, env)?)),
        Expr::Unary {
            op: UnaryOp::IsNull,
            expr,
        } => {
            let operand = eval_operand(expr, row, computed, env)?;
            Ok(Value::Integer(i64::from(operand.is_null_cell())))
        }
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr,
        } => {
            let operand = eval_operand(expr, row, computed, env)?;
            Ok(Value::Integer(i64::from(!operand.is_null_cell())))
        }
        Expr::Binary {
            op:
                op @ (BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq),
            left,
            right,
            collation,
            affinity,
        } => compare_operands(*op, left, right, row, computed, env, *collation, *affinity),
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
            ..
        } => {
            let left = evaluate_ref(left, row, computed, env)?;
            let right = evaluate_ref(right, row, computed, env)?;
            Ok(logical_and(left, right))
        }
        Expr::Binary {
            op: BinaryOp::Or,
            left,
            right,
            ..
        } => {
            let left = evaluate_ref(left, row, computed, env)?;
            let right = evaluate_ref(right, row, computed, env)?;
            Ok(logical_or(left, right))
        }
        // Everything past here needs new data (arithmetic, `||`, a function
        // result) or machinery a borrowed row cannot help with (subqueries,
        // `CASE`'s branch selection). Materialise once and hand off to
        // `evaluate`, which is the same code that already computes the exact
        // answer for these — a fallback, not a second implementation.
        _ => {
            let owned: Vec<Value> = row.iter().map(ValueRef::to_owned_value).collect();
            evaluate(expr, &owned, computed, env)
        }
    }
}

/// Either half of a comparison [`evaluate_ref`] is still deciding: borrowed
/// straight from the row, or already owned because it came from somewhere
/// else (a literal, a parameter, a nested computation).
enum Operand<'r> {
    Ref(ValueRef<'r>),
    Owned(Value),
}

/// Evaluate one comparison operand, borrowing when `expr` is a bare column
/// reference and materialising only when it has to.
fn eval_operand<'r>(
    expr: &Expr,
    row: &[ValueRef<'r>],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<Operand<'r>> {
    match expr {
        Expr::Column(index) => row.get(*index).cloned().map(Operand::Ref).ok_or_else(|| {
            Error::Corrupt(alloc::format!(
                "expression references column {index}, but the row has {} value(s)",
                row.len()
            ))
        }),
        _ => Ok(Operand::Owned(evaluate_ref(expr, row, computed, env)?)),
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_operands<'r>(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    row: &[ValueRef<'r>],
    computed: Computed<'_>,
    env: &Env<'_>,
    collation: Collation,
    affinity: CompareAffinity,
) -> Result<Value> {
    let left = eval_operand(left, row, computed, env)?;
    let right = eval_operand(right, row, computed, env)?;
    compare_cells(op, &left, &right, collation, affinity)
}

/// A comparable cell: [`Value`], [`ValueRef`] and [`Operand`] all implement
/// this, so [`compare_cells`] is written once and [`comparison`] (the owned
/// path every other caller in this module still uses) and [`compare_operands`]
/// (the borrowed path) share it rather than keep two copies of SQLite's
/// comparison rules in sync by hand.
trait Cell {
    fn is_null_cell(&self) -> bool;
    fn as_text_cell(&self) -> Option<&str>;
    fn as_blob_cell(&self) -> Option<&[u8]>;
    fn as_f64_cell(&self) -> Option<f64>;
    /// `Some` only for an exact `INTEGER`, never widening a `REAL` the way
    /// [`Self::as_f64_cell`] does. [`affinity_conversion`]'s `TEXT`-affinity
    /// branch needs this: rendering a numeric cell through `as_f64_cell` alone
    /// would turn `INTEGER` `1` into `"1.0"` rather than `"1"`, and a `TEXT`
    /// column holding `'1'` and one holding `'1.0'` are different rows,
    /// confirmed against a real sqlite3 3.54 binary (`s = 1` and `s = 1.0`
    /// match different stored strings).
    fn as_i64_cell(&self) -> Option<i64>;
    fn type_name_cell(&self) -> &'static str;
}

impl Cell for Value {
    fn is_null_cell(&self) -> bool {
        *self == Value::Null
    }

    fn as_text_cell(&self) -> Option<&str> {
        self.as_str()
    }

    fn as_blob_cell(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(bytes) => Some(bytes),
            _ => None,
        }
    }

    fn as_f64_cell(&self) -> Option<f64> {
        self.as_f64()
    }

    fn as_i64_cell(&self) -> Option<i64> {
        self.as_i64()
    }

    fn type_name_cell(&self) -> &'static str {
        self.type_name()
    }
}

impl Cell for ValueRef<'_> {
    fn is_null_cell(&self) -> bool {
        self.is_null()
    }

    fn as_text_cell(&self) -> Option<&str> {
        self.as_str()
    }

    fn as_blob_cell(&self) -> Option<&[u8]> {
        self.as_blob()
    }

    fn as_f64_cell(&self) -> Option<f64> {
        self.as_f64()
    }

    fn as_i64_cell(&self) -> Option<i64> {
        self.as_i64()
    }

    fn type_name_cell(&self) -> &'static str {
        self.type_name()
    }
}

impl Cell for Operand<'_> {
    fn is_null_cell(&self) -> bool {
        match self {
            Operand::Ref(value) => value.is_null_cell(),
            Operand::Owned(value) => value.is_null_cell(),
        }
    }

    fn as_text_cell(&self) -> Option<&str> {
        match self {
            Operand::Ref(value) => value.as_text_cell(),
            Operand::Owned(value) => value.as_text_cell(),
        }
    }

    fn as_blob_cell(&self) -> Option<&[u8]> {
        match self {
            Operand::Ref(value) => value.as_blob_cell(),
            Operand::Owned(value) => value.as_blob_cell(),
        }
    }

    fn as_f64_cell(&self) -> Option<f64> {
        match self {
            Operand::Ref(value) => value.as_f64_cell(),
            Operand::Owned(value) => value.as_f64_cell(),
        }
    }

    fn as_i64_cell(&self) -> Option<i64> {
        match self {
            Operand::Ref(value) => value.as_i64_cell(),
            Operand::Owned(value) => value.as_i64_cell(),
        }
    }

    fn type_name_cell(&self) -> &'static str {
        match self {
            Operand::Ref(value) => value.type_name_cell(),
            Operand::Owned(value) => value.type_name_cell(),
        }
    }
}

/// A comparison operator's verdict, under the collating sequence and
/// affinity conversion the planner resolved for it. The shared body of
/// [`comparison`] and [`compare_operands`] — see [`Cell`] for why this is
/// written once.
///
/// `collation` is consulted for a `TEXT` pair and for nothing else — numbers
/// compare as numbers and blobs as bytes however the operands were declared,
/// which is SQLite's rule and the reason `COLLATE NOCASE` on an `INTEGER`
/// column is accepted and inert.
///
/// `affinity` is stage one of SQLite's two-stage comparison rule (AHL-486):
/// [`affinity_conversion`] runs against each operand *before* the
/// class-order ranking below, which is stage two and always runs regardless
/// of what stage one did or did not convert.
fn compare_cells<L: Cell, R: Cell>(
    op: BinaryOp,
    left: &L,
    right: &R,
    collation: Collation,
    affinity: CompareAffinity,
) -> Result<Value> {
    if left.is_null_cell() || right.is_null_cell() {
        return Ok(Value::Null);
    }

    // Stage one (AHL-486). A converted operand becomes a real owned `Value`;
    // one `affinity` does not touch stays the original cell, unconverted —
    // the overwhelmingly common case (including a plain `id = 5` against an
    // `INTEGER` column: `id`'s affinity resolves to `Numeric`, but neither
    // side is `TEXT` for `affinity_conversion` to parse) and costs nothing
    // beyond the accessor calls stage two below already makes.
    let left_converted = affinity_conversion(left, affinity);
    let right_converted = affinity_conversion(right, affinity);
    let left: &dyn Cell = match &left_converted {
        Some(value) => value,
        None => left,
    };
    let right: &dyn Cell = match &right_converted {
        Some(value) => value,
        None => right,
    };

    // Stage two. Storage classes rank the way SQLite's do, and the way
    // [`mem_cmp`] does — numbers < text < blobs — so a pair still cross-class
    // after stage one answers by class instead of raising. Keeping this in
    // step with `mem_cmp` is what stops a borrowed fast-path answer from
    // differing from the owned one, the same way AHL-477 stopped an indexed
    // answer from differing from a scanned one.
    fn class(cell: &dyn Cell) -> Option<u8> {
        if cell.as_f64_cell().is_some() {
            Some(1)
        } else if cell.as_text_cell().is_some() {
            Some(2)
        } else if cell.as_blob_cell().is_some() {
            Some(3)
        } else {
            // A vector: not a SQLite storage class, and not comparable here.
            None
        }
    }

    let (Some(left_class), Some(right_class)) = (class(left), class(right)) else {
        return Err(Error::Type(alloc::format!(
            "cannot compare {} and {}",
            left.type_name_cell(),
            right.type_name_cell()
        )));
    };

    let ordering = match left_class.cmp(&right_class) {
        Ordering::Equal => match (left.as_text_cell(), right.as_text_cell()) {
            (Some(a), Some(b)) => collation.compare(a, b),
            _ => match (left.as_blob_cell(), right.as_blob_cell()) {
                (Some(a), Some(b)) => a.cmp(b),
                _ => match (left.as_f64_cell(), right.as_f64_cell()) {
                    (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
                    _ => unreachable!("equal storage classes compare within the class"),
                },
            },
        },
        class_ordering => class_ordering,
    };
    let result = match op {
        BinaryOp::Eq => ordering == Ordering::Equal,
        BinaryOp::NotEq => ordering != Ordering::Equal,
        BinaryOp::Lt => ordering == Ordering::Less,
        BinaryOp::LtEq => ordering != Ordering::Greater,
        BinaryOp::Gt => ordering == Ordering::Greater,
        BinaryOp::GtEq => ordering != Ordering::Less,
        _ => unreachable!("non-comparison operator in comparison"),
    };
    Ok(Value::Integer(i64::from(result)))
}

/// Stage one of SQLite's comparison rule (AHL-486): the value `cell`
/// becomes under `affinity`, or `None` when nothing changes.
///
/// `None` is the common case, not the exception: a `NULL`, a `BLOB`, a cell
/// `affinity` does not apply to at all ([`CompareAffinity::None`]), or —
/// this is the one worth naming — a cell already of the class `affinity`
/// targets. That last case is why a plain `id = 5` against an `INTEGER`
/// column never allocates here: `id`'s own affinity resolves to
/// [`CompareAffinity::Numeric`], but `5` and whatever `id` holds are already
/// numbers, so there is no `TEXT` for the `Numeric` arm to parse.
///
/// The two arms are SQLite's `applyAffinity`, restricted to what a
/// comparison operand can actually be:
///
/// * [`CompareAffinity::Numeric`] parses a `TEXT` cell the same way a
///   `NUMERIC`-affinity column coerces one at insert time
///   ([`numeric_affinity_of_text`]) — a well-formed number converts, a
///   `TEXT` cell that merely contains one (`'1x'`) does not, and neither does
///   a `BLOB`.
/// * [`CompareAffinity::Text`] renders an `INTEGER`/`REAL` cell as text —
///   `as_i64_cell` first, so an `INTEGER` renders as `"1"` and not `"1.0"`;
///   see [`Cell::as_i64_cell`]'s doc for why that distinction is load-bearing
///   here. A `TEXT` or `BLOB` cell is untouched, confirmed against a real
///   sqlite3 3.54 binary: `TEXT` affinity never turns a blob's bytes into
///   characters.
fn affinity_conversion<C: Cell>(cell: &C, affinity: CompareAffinity) -> Option<Value> {
    match affinity {
        CompareAffinity::None => None,
        CompareAffinity::Numeric => numeric_affinity_of_text(cell.as_text_cell()?),
        CompareAffinity::Text => {
            if cell.as_text_cell().is_some() || cell.as_blob_cell().is_some() {
                return None;
            }
            let rendered = match cell.as_i64_cell() {
                Some(integer) => integer.to_string(),
                None => real_to_text(cell.as_f64_cell()?),
            };
            Some(Value::Text(rendered))
        }
    }
}

/// Turn a subquery's rows into the value the enclosing expression wanted.
fn subquery(
    op: &SubqueryOp,
    query: &Subquery,
    row: &[Value],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<Value> {
    match op {
        // SQLite: no rows is `NULL`, and a subquery that returns more than one
        // row is *not* an error — every row after the first is ignored.
        // (`lang_expr.html`: "If the SELECT statement returns more than one
        // result row, all rows after the first are ignored.") Matching that
        // rather than raising is deliberate; `subqueries.test` pins it.
        SubqueryOp::Scalar => {
            let rows = subquery_rows(query, Some(1), row, computed, env)?;
            Ok(rows
                .first()
                .and_then(|first| first.first())
                .cloned()
                .unwrap_or(Value::Null))
        }
        // `EXISTS` asks one question of the row set and is never `NULL`, even
        // when the row it found is entirely `NULL`s.
        SubqueryOp::Exists { negated } => {
            let rows = subquery_rows(query, Some(1), row, computed, env)?;
            let found = !rows.is_empty();
            Ok(Value::Integer(i64::from(found != *negated)))
        }
        // Exactly the rules [`Expr::InList`] follows, over rows instead of a
        // written list: an empty result is false even for a `NULL` probe, a
        // `NULL` probe against a non-empty result is unknown, and a miss with a
        // `NULL` among the candidates is unknown rather than false. The
        // differential fuzzer generates all four cases.
        SubqueryOp::In {
            negated,
            probe,
            collation,
            affinity,
        } => {
            let rows = subquery_rows(query, None, row, computed, env)?;
            if rows.is_empty() {
                return Ok(Value::Integer(i64::from(*negated)));
            }
            let value = evaluate(probe, row, computed, env)?;
            if value == Value::Null {
                return Ok(Value::Null);
            }
            let mut saw_null = false;
            for candidate in rows.iter() {
                let candidate = match candidate.first() {
                    Some(value) => value,
                    // A width of one is checked in the planner, so this cannot
                    // happen; answering `NULL` rather than panicking keeps it
                    // harmless if it ever does.
                    None => return Ok(Value::Null),
                };
                if *candidate == Value::Null {
                    saw_null = true;
                    continue;
                }
                if comparison(
                    BinaryOp::Eq,
                    value.clone(),
                    candidate.clone(),
                    *collation,
                    *affinity,
                )? == Value::Integer(1)
                {
                    return Ok(Value::Integer(i64::from(!*negated)));
                }
            }
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Integer(i64::from(*negated))
            })
        }
    }
}

/// Run a subquery, or hand back the rows a previous evaluation of it produced.
///
/// An uncorrelated subquery — one with no captures — is a constant for the
/// whole statement, so it is memoised by [`Subquery::id`] and a million-row
/// outer scan runs it once. A correlated one depends on the outer row and is
/// re-run for every one of them: **there is no decorrelation here**, by
/// decision rather than by oversight (`docs/architecture.md`, Phase 1c item 1). A correlated
/// subquery over a table with no useful index is O(outer × inner), and the
/// honest place to fix that is a semi-join rewrite in the planner, not a cache
/// that would have to know when the inner table changed.
fn subquery_rows(
    query: &Subquery,
    max_rows: Option<usize>,
    row: &[Value],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<SubqueryRows> {
    let uncorrelated = query.captures.is_empty();
    if uncorrelated {
        if let Some(cached) = env.memo.borrow().get(&query.id) {
            return Ok(Rc::clone(cached));
        }
    }

    let mut captured = Vec::with_capacity(query.captures.len());
    for capture in &query.captures {
        captured.push(evaluate(capture, row, computed, env)?);
    }

    let runner = env.runner.ok_or_else(|| {
        Error::Unsupported(
            "a subquery cannot be evaluated here; subqueries are supported in SELECT (including \
             the query of an INSERT ... SELECT), not in UPDATE, DELETE or INSERT ... VALUES"
                .to_string(),
        )
    })?;
    // The nested environment borrows `captured`, so it must not outlive this
    // frame — which it does not: the runner returns owned rows.
    let rows = Rc::new(runner.run(&query.body, &env.nested(&captured), max_rows)?);

    if uncorrelated {
        env.memo.borrow_mut().insert(query.id, Rc::clone(&rows));
    }
    Ok(rows)
}

/// Whether a value is true in a `WHERE` clause: non-zero and non-`NULL`.
pub fn is_truthy(value: &Value) -> bool {
    matches!(truth(value), Some(true))
}

/// Compute one aggregate over a group of rows.
///
/// The group is the set of joined rows a `GROUP BY` bucket collects (a single
/// bucket containing every row when there is no `GROUP BY`, which may itself
/// be empty). Semantics follow SQLite:
///
/// * `COUNT(*)` counts rows; `COUNT(expr)` counts non-`NULL` values.
/// * `SUM` ignores `NULL`s and stays integer when every value is an integer.
/// * `AVG` ignores `NULL`s and always yields a real.
/// * `MIN`/`MAX` ignore `NULL`s and compare by SQLite's sort order.
/// * `GROUP_CONCAT` joins the non-`NULL` values, rendered as text, with a
///   separator (`,` unless one is supplied).
///
/// A `SUM`/`AVG`/`MIN`/`MAX`/`GROUP_CONCAT` over no non-`NULL` values is
/// `NULL`. With `DISTINCT`, equal argument values are folded into one before
/// any of that happens — equal by SQLite's storage-class ordering, so integer
/// `1` and real `1.0` are one value.
/// The group is borrowed rather than owned: the executor holds the rows in the
/// pipeline's `Vec` and used to clone every one of them into a second `Vec` to
/// call this, which `PERF.md` counts as the third full copy of every row.
pub fn evaluate_aggregate(
    aggregate: &Aggregate,
    group: &[&[Value]],
    env: &Env<'_>,
) -> Result<Value> {
    // `FILTER (WHERE ...)` narrows the group before anything else sees it —
    // including `COUNT(*)`, which is why this runs ahead of the no-argument
    // early return rather than after it. Three-valued: a row the predicate
    // does not answer `true` for is dropped, the same rule `WHERE` itself
    // uses.
    let filtered_storage: Vec<&[Value]>;
    let group: &[&[Value]] = match &aggregate.filter {
        Some(filter) => {
            let mut kept = Vec::with_capacity(group.len());
            for row in group {
                if is_truthy(&evaluate(filter, row, Computed::NONE, env)?) {
                    kept.push(*row);
                }
            }
            filtered_storage = kept;
            &filtered_storage
        }
        None => group,
    };

    // `COUNT(*)` has no argument to fold, and counts rows rather than values.
    let Some(arg) = &aggregate.arg else {
        return match aggregate.func {
            AggFunc::Count => Ok(Value::Integer(group.len() as i64)),
            _ => Err(Error::Type(
                "SUM/AVG/MIN/MAX/GROUP_CONCAT require an argument".to_string(),
            )),
        };
    };

    let mut values = Vec::with_capacity(group.len());
    for row in group {
        values.push(evaluate(arg, row, Computed::NONE, env)?);
    }

    if aggregate.distinct {
        values = distinct_values(values, aggregate.collation);
    }

    match aggregate.func {
        AggFunc::Count => Ok(Value::Integer(
            values.iter().filter(|value| **value != Value::Null).count() as i64,
        )),
        // `sumStep` accumulates in an `i64` until something forces it not to —
        // a real argument, or an addition that overflows. An overflow is an
        // error in SQLite, not a wrapped answer and not a silent promotion,
        // because the exact sum of integers is what `SUM` promised.
        AggFunc::Sum => {
            let mut any = false;
            let mut approximate = false;
            let mut overflowed = false;
            let mut integer = 0i64;
            let mut real = 0.0f64;
            for value in values {
                match value {
                    Value::Null => {}
                    Value::Integer(i) => {
                        any = true;
                        if approximate {
                            real += i as f64;
                        } else {
                            match integer.checked_add(i) {
                                Some(sum) => integer = sum,
                                None => {
                                    overflowed = true;
                                    approximate = true;
                                    real = integer as f64 + i as f64;
                                }
                            }
                        }
                    }
                    Value::Real(r) => {
                        any = true;
                        if !approximate {
                            approximate = true;
                            real = integer as f64;
                        }
                        real += r;
                    }
                    other => return Err(numeric_error(other.type_name())),
                }
            }
            if overflowed {
                return Err(Error::Type("integer overflow".to_string()));
            }
            Ok(if !any {
                Value::Null
            } else if approximate {
                Value::Real(real)
            } else {
                Value::Integer(integer)
            })
        }
        AggFunc::Avg => {
            let mut sum = 0.0f64;
            let mut count = 0i64;
            for value in values {
                match value {
                    Value::Null => {}
                    Value::Integer(i) => {
                        sum += i as f64;
                        count += 1;
                    }
                    Value::Real(r) => {
                        sum += r;
                        count += 1;
                    }
                    other => return Err(numeric_error(other.type_name())),
                }
            }
            Ok(if count == 0 {
                Value::Null
            } else {
                Value::Real(sum / count as f64)
            })
        }
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<Value> = None;
            for value in values {
                if value == Value::Null {
                    continue;
                }
                let take = match &best {
                    None => true,
                    Some(current) => {
                        // `mem_cmp` — the same total order `ORDER BY`,
                        // `DISTINCT` and index keys use — not the old
                        // `value_cmp`, which had the identical
                        // fall-back-to-equal bug `engine.rs::compare_values`
                        // did for a cross-storage-class pair (`TEXT` vs
                        // `INTEGER`, say): a wrong "equal" here would have
                        // let a later value silently overwrite the running
                        // `MIN`/`MAX` instead of losing the comparison.
                        let ordering = mem_cmp(&value, current, aggregate.collation);
                        match aggregate.func {
                            AggFunc::Min => ordering == Ordering::Less,
                            _ => ordering == Ordering::Greater,
                        }
                    }
                };
                if take {
                    best = Some(value);
                }
            }
            Ok(best.unwrap_or(Value::Null))
        }
        AggFunc::GroupConcat => {
            // The separator is an expression per SQLite's signature, but it is
            // read once, from the first row of the group: a separator that
            // varied per row would have no defined meaning, and SQLite reads
            // whatever the first invocation supplied.
            let separator = match &aggregate.separator {
                Some(expr) => {
                    let Some(first) = group.first() else {
                        return Ok(Value::Null);
                    };
                    match evaluate(expr, first, Computed::NONE, env)? {
                        // SQLite stops concatenating once the separator is
                        // NULL; with one row that leaves the value itself.
                        Value::Null => return Ok(Value::Null),
                        other => as_text(&other)?,
                    }
                }
                None => ",".to_string(),
            };
            let mut out = String::new();
            let mut any = false;
            for value in values {
                if value == Value::Null {
                    continue;
                }
                if any {
                    out.push_str(&separator);
                }
                out.push_str(&as_text(&value)?);
                any = true;
            }
            Ok(if any { Value::Text(out) } else { Value::Null })
        }
    }
}

/// Fold values that are equal under SQLite's storage-class ordering into one,
/// keeping the first of each run in the order they were produced.
fn distinct_values(values: Vec<Value>, collation: Collation) -> Vec<Value> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| mem_cmp(&values[*a], &values[*b], collation).then(a.cmp(b)));

    let mut keep = alloc::vec![false; values.len()];
    let mut previous: Option<usize> = None;
    for index in order {
        let first = !matches!(
            previous,
            Some(previous)
                if mem_cmp(&values[previous], &values[index], collation) == Ordering::Equal
        );
        if first {
            keep[index] = true;
            previous = Some(index);
        }
    }

    values
        .into_iter()
        .zip(keep)
        .filter_map(|(value, keep)| keep.then_some(value))
        .collect()
}

/// SQLite's `sqlite3MemCompare`: the total order that `DISTINCT`, `NULLIF`,
/// the scalar `min`/`max` and index keys all share.
///
/// Storage class decides first — `NULL` below every number, numbers below
/// text, text below blobs — and only values of the same class are compared on
/// their contents. This is *not* [`comparison`], which applies affinity and is
/// three-valued; the difference is why `nullif(1, '1')` is `1`.
///
/// `collation` decides how two `TEXT` values compare, and nothing else: the
/// storage-class order above it is fixed.
pub(crate) fn mem_cmp(left: &Value, right: &Value, collation: Collation) -> Ordering {
    fn class(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::Integer(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
            // Not a SQLite storage class; ordered last so the comparison
            // stays total rather than declaring unrelated values equal.
            Value::Vector(_) => 4,
        }
    }

    let ordering = class(left).cmp(&class(right));
    if ordering != Ordering::Equal {
        return ordering;
    }
    match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
        (Value::Text(a), Value::Text(b)) => collation.compare(a, b),
        (Value::Blob(a), Value::Blob(b)) => a.cmp(b),
        (Value::Vector(a), Value::Vector(b)) => a.len().cmp(&b.len()),
        // One side is a real: compare as f64, which is what SQLite does once
        // it has ruled out the two-integer case.
        _ => match (left.as_f64(), right.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
            _ => Ordering::Equal,
        },
    }
}

fn numeric_error(type_name: &str) -> Error {
    Error::Type(alloc::format!(
        "aggregate argument must be numeric, got {type_name}"
    ))
}

/// SQL's three-valued truth: `None` is `NULL`.
///
/// Text and blobs follow SQLite rather than the "non-empty is true" rule most
/// languages use: the value is converted to a number first, so `'abc'` is
/// false, `''` is false and `'1x'` is true. `WHERE b` and
/// `CASE WHEN b THEN ...` both go through here, so getting it wrong would put
/// InlaySQL and SQLite on different sides of the same row.
fn truth(value: &Value) -> Option<bool> {
    match value {
        Value::Null => None,
        Value::Integer(i) => Some(*i != 0),
        Value::Real(r) => Some(*r != 0.0),
        Value::Text(s) => Some(text_to_real(s) != 0.0),
        Value::Blob(bytes) => Some(text_to_real(&String::from_utf8_lossy(bytes)) != 0.0),
        Value::Vector(_) => Some(true),
    }
}

/// `NOT`, in three-valued logic.
fn logical_not(value: &Value) -> Value {
    match truth(value) {
        Some(truth) => Value::Integer(i64::from(!truth)),
        None => Value::Null,
    }
}

fn logical_and(left: Value, right: Value) -> Value {
    match (truth(&left), truth(&right)) {
        (Some(false), _) | (_, Some(false)) => Value::Integer(0),
        (Some(true), Some(true)) => Value::Integer(1),
        _ => Value::Null,
    }
}

fn logical_or(left: Value, right: Value) -> Value {
    match (truth(&left), truth(&right)) {
        (Some(true), _) | (_, Some(true)) => Value::Integer(1),
        (Some(false), Some(false)) => Value::Integer(0),
        _ => Value::Null,
    }
}

/// Unary minus.
///
/// SQLite has no negate opcode: `-X` compiles to `0 - X`, so it inherits the
/// overflow rule below — `-(-9223372036854775808)` is the REAL
/// `9.223372036854776e18`, not the same integer back again.
fn negate(value: Value) -> Result<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Integer(i) => Ok(match i.checked_neg() {
            Some(negated) => Value::Integer(negated),
            None => Value::Real(-(i as f64)),
        }),
        Value::Real(r) => Ok(Value::Real(-r)),
        other => Err(Error::Type(alloc::format!(
            "cannot negate a {} value",
            other.type_name()
        ))),
    }
}

/// Integer arithmetic that promotes to REAL on overflow, as SQLite does.
///
/// `sqlite3AddInt64` and friends report overflow rather than wrapping, and the
/// VDBE's answer to a report is to redo the operation in floating point (`OP_Add`
/// falls through to `fp_math`). Wrapping instead was InlaySQL's one arithmetic
/// divergence from SQLite, found by the differential suite and documented in
/// `TESTING.md`; this is the fix, and `differential.rs` generates
/// `CAST(r AS INTEGER) + 1` again to keep it fixed.
fn arithmetic(op: BinaryOp, left: Value, right: Value) -> Result<Value> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    match (&left, &right) {
        (Value::Integer(a), Value::Integer(b)) => {
            let checked = match op {
                BinaryOp::Add => a.checked_add(*b),
                BinaryOp::Sub => a.checked_sub(*b),
                BinaryOp::Mul => a.checked_mul(*b),
                // `x / 0` is NULL, and `i64::MIN / -1` is the one division
                // that overflows: SQLite sends it to floating point too.
                BinaryOp::Div => {
                    if *b == 0 {
                        return Ok(Value::Null);
                    }
                    a.checked_div(*b)
                }
                // `x % 0` is NULL and `x % -1` is 0 — SQLite rewrites the
                // divisor to 1 rather than overflowing.
                BinaryOp::Mod => return Ok(int_mod(*a, *b)),
                _ => unreachable!("non-arithmetic operator in arithmetic"),
            };
            Ok(match checked {
                Some(result) => Value::Integer(result),
                None => real_arithmetic(op, *a as f64, *b as f64),
            })
        }
        _ => {
            let (Some(a), Some(b)) = (left.as_f64(), right.as_f64()) else {
                return Err(Error::Type(
                    "arithmetic operands must be numeric".to_string(),
                ));
            };
            // SQLite coerces `%` to integers; we only reach here for real
            // operands, so match its truncation behaviour.
            if op == BinaryOp::Mod {
                return Ok(int_mod(a as i64, b as i64));
            }
            Ok(real_arithmetic(op, a, b))
        }
    }
}

/// The floating-point half of [`arithmetic`], shared by real operands and by
/// integer operands whose exact result did not fit.
fn real_arithmetic(op: BinaryOp, a: f64, b: f64) -> Value {
    match op {
        BinaryOp::Add => Value::Real(a + b),
        BinaryOp::Sub => Value::Real(a - b),
        BinaryOp::Mul => Value::Real(a * b),
        BinaryOp::Div => {
            if b == 0.0 {
                Value::Null
            } else {
                Value::Real(a / b)
            }
        }
        _ => unreachable!("non-arithmetic operator in arithmetic"),
    }
}

fn int_mod(a: i64, b: i64) -> Value {
    match b {
        0 => Value::Null,
        // `sqlite3VdbeExec`'s `OP_Remainder` rewrites a divisor of -1 to 1
        // rather than letting `i64::MIN % -1` overflow. The answer is 0 either
        // way; saying so here is what keeps it from being an accident.
        -1 => Value::Integer(0),
        _ => Value::Integer(a.wrapping_rem(b)),
    }
}

/// A comparison operator's verdict, under the collating sequence and
/// affinity conversion the planner resolved for it.
///
/// This is SQLite's two-stage rule in full (AHL-486):
///
/// 1. **Affinity conversion.** If `affinity` says so, a `TEXT` operand
///    becomes a number, or a numeric one becomes text — see
///    [`affinity_conversion`]. `id = '1'` matches an `INTEGER` column because
///    of this stage; before it existed, the missing conversion was invisible
///    because stage two below still answered *something* for the
///    now-cross-class pair, just the wrong one.
/// 2. **Storage-class ordering**, unconditional and run on whatever stage one
///    left behind. `collation` is consulted for a `TEXT` pair and for nothing
///    else — numbers compare as numbers and blobs as bytes however the
///    operands were declared, which is SQLite's rule and the reason `COLLATE
///    NOCASE` on an `INTEGER` column is accepted and inert. A pair still
///    crossing `NULL`/numeric/`TEXT`/`BLOB` after stage one — bare `1 = 'a'`,
///    say, where neither side carries an affinity for stage one to touch —
///    **used to raise [`Error::Type`]**; SQLite never does, so this shares
///    [`mem_cmp`]'s class order (`NULL` < numbers < `TEXT` < `BLOB`,
///    confirmed against a real sqlite3 3.54 binary) — the same one
///    [`crate::engine`]'s `ORDER BY`/`GROUP BY` comparator, `DISTINCT` and
///    index keys already use — instead of refusing. `NULL` is excluded above,
///    so `mem_cmp`'s class 0 is never actually reached here; three-valued
///    logic still owns `NULL`, not the class order.
///
/// `VECTOR` is not one of SQLite's storage classes — it is this engine's own
/// addition — so it keeps raising [`Error::Type`] here rather than picking up
/// a class ranking SQLite has no opinion on; `mem_cmp` only orders it (last)
/// because a sort has to answer *something* for every pair, which a `WHERE`
/// clause does not.
fn comparison(
    op: BinaryOp,
    left: Value,
    right: Value,
    collation: Collation,
    affinity: CompareAffinity,
) -> Result<Value> {
    compare_cells(op, &left, &right, collation, affinity)
}

// -------------------------------------------------------------- text and casts

/// `a || b`: `NULL` if either side is, otherwise both sides rendered as text.
fn concat(left: Value, right: Value) -> Result<Value> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let mut text = as_text(&left)?;
    text.push_str(&as_text(&right)?);
    Ok(Value::Text(text))
}

/// SQLite's text rendering of a non-`NULL` value.
///
/// This is one function because `CAST(x AS TEXT)`, `||` and `LIKE` all have to
/// agree about what a number looks like as text — a difference between them
/// would be invisible until a query used two of them on one value.
fn as_text(value: &Value) -> Result<String> {
    match value {
        // Every caller checks for `NULL` first, because `NULL` propagates
        // rather than rendering. Reaching here would be a bug, so say so.
        Value::Null => Err(Error::Type(
            "NULL has no text form; it propagates instead".to_string(),
        )),
        Value::Integer(i) => Ok(i.to_string()),
        Value::Real(r) => Ok(real_to_text(*r)),
        Value::Text(s) => Ok(s.clone()),
        // SQLite reads a blob's bytes as text without validating them.
        Value::Blob(bytes) => Ok(String::from_utf8_lossy(bytes).into_owned()),
        Value::Vector(_) => Err(Error::Type("a VECTOR value has no text form".to_string())),
    }
}

/// SQLite's `%!.15g` rendering of a real.
///
/// Fifteen significant digits, trailing zeros stripped, and never a form that
/// could be mistaken for an integer: `2.0` stays `2.0` and `1e300` prints as
/// `1.0e+300`. That last rule is the `!` in the format string, and it is why
/// Rust's `{}` cannot be used directly.
pub(crate) fn real_to_text(value: f64) -> String {
    format_g(value, 15, true)
}

/// C's `%g` with `precision` significant digits.
///
/// `force_fraction` is the `!` flag SQLite adds for rendering a REAL as text:
/// it keeps the result from ever reading as an integer, so `2.0` stays `2.0`
/// and `1e300` prints as `1.0e+300`. `strftime`'s `%J` wants plain `%.16g`
/// instead, which is the same routine without that rule.
fn format_g(value: f64, precision: i32, force_fraction: bool) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Inf" } else { "-Inf" }.to_string();
    }

    // `%e` with `precision - 1` decimals is exactly how `%g` decides which
    // form to print: the exponent it produces is the one the rule tests.
    let scientific = alloc::format!("{:.*e}", (precision - 1).max(0) as usize, value);
    let (mantissa, exponent) = match scientific.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().unwrap_or(0)),
        None => (scientific.as_str(), 0),
    };

    // `%g` prints the exponential form outside this window, the fixed form
    // inside it.
    if !(-4..precision).contains(&exponent) {
        alloc::format!(
            "{}e{}{:02}",
            trim_fraction(mantissa, force_fraction),
            if exponent < 0 { '-' } else { '+' },
            exponent.unsigned_abs()
        )
    } else {
        let decimals = (precision - 1 - exponent).max(0) as usize;
        trim_fraction(&alloc::format!("{value:.decimals$}"), force_fraction)
    }
}

/// Strip trailing zeros from a fixed-point rendering, keeping one digit after
/// the point when the caller asked for a result that never reads as an
/// integer.
fn trim_fraction(text: &str, force_fraction: bool) -> String {
    match text.split_once('.') {
        Some((whole, fraction)) => {
            let fraction = fraction.trim_end_matches('0');
            if fraction.is_empty() {
                if force_fraction {
                    alloc::format!("{whole}.0")
                } else {
                    whole.to_string()
                }
            } else {
                alloc::format!("{whole}.{fraction}")
            }
        }
        None if force_fraction => alloc::format!("{text}.0"),
        None => text.to_string(),
    }
}

/// The numeric prefix of `text`, the way SQLite's `sqlite3AtoF` reads one.
///
/// Returns the prefix itself, whether it looked like a float (it carried a
/// decimal point or an exponent) and whether anything but whitespace followed
/// it. `None` means there was no number to read at all.
fn scan_number(text: &str) -> Option<(&str, bool)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let start = index;
    if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
        index += 1;
    }

    let mut digits = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
        digits += 1;
    }

    let mut fractional = false;
    if index < bytes.len() && bytes[index] == b'.' {
        let mut after = index + 1;
        let mut fraction_digits = 0;
        while after < bytes.len() && bytes[after].is_ascii_digit() {
            after += 1;
            fraction_digits += 1;
        }
        if digits > 0 || fraction_digits > 0 {
            fractional = true;
            digits += fraction_digits;
            index = after;
        }
    }
    if digits == 0 {
        return None;
    }

    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        let mut after = index + 1;
        if after < bytes.len() && (bytes[after] == b'+' || bytes[after] == b'-') {
            after += 1;
        }
        let mut exponent_digits = 0;
        while after < bytes.len() && bytes[after].is_ascii_digit() {
            after += 1;
            exponent_digits += 1;
        }
        if exponent_digits > 0 {
            fractional = true;
            index = after;
        }
    }

    Some((&text[start..index], fractional))
}

/// Text as a real, SQLite-style: the longest numeric prefix, or `0.0`.
fn text_to_real(text: &str) -> f64 {
    match scan_number(text) {
        Some((prefix, _)) => prefix.parse::<f64>().unwrap_or(0.0),
        None => 0.0,
    }
}

/// Text as an integer, SQLite-style.
///
/// `sqlite3Atoi64` reads digits only, so it stops at a decimal point or an
/// exponent: `'3.9'` is `3` and `'1e3'` is `1`. Overflow saturates, as it does
/// there.
fn text_to_integer(text: &str) -> i64 {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let negative = match bytes.get(index) {
        Some(b'-') => {
            index += 1;
            true
        }
        Some(b'+') => {
            index += 1;
            false
        }
        _ => false,
    };

    let mut magnitude: u64 = 0;
    let mut overflow = false;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        let digit = u64::from(bytes[index] - b'0');
        magnitude = match magnitude.checked_mul(10).and_then(|m| m.checked_add(digit)) {
            Some(value) => value,
            None => {
                overflow = true;
                magnitude
            }
        };
        index += 1;
    }

    if negative {
        if overflow || magnitude > i64::MAX as u64 + 1 {
            i64::MIN
        } else {
            (magnitude as i64).wrapping_neg()
        }
    } else if overflow || magnitude > i64::MAX as u64 {
        i64::MAX
    } else {
        magnitude as i64
    }
}

/// Text under `NUMERIC` affinity: an integer when the text describes one,
/// a real otherwise, and `0` when it describes no number at all.
fn text_to_numeric(text: &str) -> Value {
    let Some((prefix, fractional)) = scan_number(text) else {
        return Value::Integer(0);
    };
    if !fractional {
        // Integer-looking text is an integer when it fits, a real when it
        // does not.
        return match prefix.parse::<i64>() {
            Ok(integer) => Value::Integer(integer),
            Err(_) => Value::Real(prefix.parse::<f64>().unwrap_or(0.0)),
        };
    }
    let real = prefix.parse::<f64>().unwrap_or(0.0);
    // Float-looking text collapses to an integer only when the round trip is
    // lossless, which SQLite bounds at 51 bits rather than 64 so that the
    // conversion cannot lose precision either way.
    const LOSSLESS: i64 = 1 << 51;
    let truncated = real as i64;
    if real == truncated as f64 && (-LOSSLESS..=LOSSLESS).contains(&truncated) {
        Value::Integer(truncated)
    } else {
        Value::Real(real)
    }
}

/// `CAST(value AS ...)`, following SQLite's conversion rules.
fn cast(value: Value, to: CastType) -> Result<Value> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if let Value::Vector(_) = value {
        return Err(Error::Type(alloc::format!(
            "a VECTOR value cannot be CAST to {to:?}"
        )));
    }

    Ok(match to {
        CastType::Text => Value::Text(as_text(&value)?),
        CastType::Blob => match value {
            Value::Blob(bytes) => Value::Blob(bytes),
            other => Value::Blob(as_text(&other)?.into_bytes()),
        },
        CastType::Integer => match value {
            Value::Integer(i) => Value::Integer(i),
            // `as` saturates in Rust exactly where SQLite's `doubleToInt64`
            // clamps, and truncates toward zero the same way.
            Value::Real(r) => Value::Integer(r as i64),
            Value::Text(s) => Value::Integer(text_to_integer(&s)),
            Value::Blob(bytes) => Value::Integer(text_to_integer(&String::from_utf8_lossy(&bytes))),
            Value::Null | Value::Vector(_) => unreachable!("handled above"),
        },
        CastType::Real => match value {
            Value::Integer(i) => Value::Real(i as f64),
            Value::Real(r) => Value::Real(r),
            Value::Text(s) => Value::Real(text_to_real(&s)),
            Value::Blob(bytes) => Value::Real(text_to_real(&String::from_utf8_lossy(&bytes))),
            Value::Null | Value::Vector(_) => unreachable!("handled above"),
        },
        // Casting a number to NUMERIC is a no-op, even when a real would fit
        // an integer exactly.
        CastType::Numeric => match value {
            Value::Integer(i) => Value::Integer(i),
            Value::Real(r) => Value::Real(r),
            Value::Text(s) => text_to_numeric(&s),
            Value::Blob(bytes) => text_to_numeric(&String::from_utf8_lossy(&bytes)),
            Value::Null | Value::Vector(_) => unreachable!("handled above"),
        },
    })
}

// ---------------------------------------------------------------------- LIKE

/// The single character an `ESCAPE` clause supplies.
fn single_escape_char(value: &Value) -> Result<char> {
    let text = as_text(value)?;
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(escape), None) => Ok(escape),
        _ => Err(Error::Type(
            "ESCAPE expression must be a single character".to_string(),
        )),
    }
}

/// One element of a compiled `LIKE` pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    /// `%` — any run of characters, including none.
    Any,
    /// `_` — exactly one character.
    One,
    /// A character that must match itself (modulo ASCII case).
    Literal(char),
}

/// Compile a `LIKE` pattern, resolving the escape character.
///
/// `None` means the pattern ends with a dangling escape character, which
/// SQLite treats as matching nothing at all rather than as an error.
fn compile_pattern(pattern: &str, escape: Option<char>) -> Option<Vec<Token>> {
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            // The escape makes the *next* character literal, whatever it is —
            // including `%`, `_` and the escape character itself.
            tokens.push(Token::Literal(chars.next()?));
        } else if c == '%' {
            tokens.push(Token::Any);
        } else if c == '_' {
            tokens.push(Token::One);
        } else {
            tokens.push(Token::Literal(c));
        }
    }
    Some(tokens)
}

/// SQLite's case folding for `LIKE`: ASCII `A`–`Z` only.
///
/// This is the quirk that makes `'É' LIKE 'é'` false while `'E' LIKE 'e'` is
/// true. `to_ascii_lowercase` leaves every non-ASCII character alone, which is
/// precisely SQLite's `GlobUpperToLower`.
fn like_char_eq(pattern: char, text: char) -> bool {
    pattern == text || pattern.eq_ignore_ascii_case(&text)
}

/// Whether `text` matches `pattern` under SQLite's `LIKE`.
fn like_matches(pattern: &str, text: &str, escape: Option<char>) -> bool {
    let Some(tokens) = compile_pattern(pattern, escape) else {
        return false;
    };
    let text: Vec<char> = text.chars().collect();

    // Iterative backtracking on `%` rather than recursion: a pathological
    // pattern from a language model must not be able to exhaust the stack.
    let mut token = 0;
    let mut position = 0;
    let mut wildcard: Option<(usize, usize)> = None;

    while position < text.len() {
        match tokens.get(token) {
            Some(Token::Any) => {
                wildcard = Some((token, position));
                token += 1;
                continue;
            }
            Some(Token::One) => {
                token += 1;
                position += 1;
                continue;
            }
            Some(Token::Literal(c)) if like_char_eq(*c, text[position]) => {
                token += 1;
                position += 1;
                continue;
            }
            _ => {}
        }
        // No match here: give the last `%` one more character and retry.
        match wildcard {
            Some((wildcard_token, wildcard_position)) => {
                token = wildcard_token + 1;
                position = wildcard_position + 1;
                wildcard = Some((wildcard_token, position));
            }
            None => return false,
        }
    }

    tokens[token..].iter().all(|token| *token == Token::Any)
}

// ----------------------------------------------------------- scalar functions

/// Evaluate a scalar function call.
///
/// Arity was checked when the call was planned, so the shapes matched here are
/// the only ones that can arrive. Semantics follow SQLite's implementations
/// closely enough that the differential suite can use SQLite as the oracle —
/// including the parts that look like accidents, such as `hex(NULL)` being the
/// empty string rather than `NULL`, and `abs(-9223372036854775808)` being an
/// error rather than a value.
fn call(
    func: ScalarFunc,
    args: &[Expr],
    row: &[Value],
    computed: Computed<'_>,
    env: &Env<'_>,
    collation: Collation,
) -> Result<Value> {
    // `coalesce` and `ifnull` short-circuit, so their arguments are not
    // evaluated up front: SQLite does not evaluate what it does not need, and
    // an argument can fail.
    if matches!(func, ScalarFunc::Coalesce | ScalarFunc::IfNull) {
        for arg in args {
            let value = evaluate(arg, row, computed, env)?;
            if value != Value::Null {
                return Ok(value);
            }
        }
        return Ok(Value::Null);
    }

    // `json_set`/`json_insert`/`json_replace`'s `(path, value)` pairs and
    // `json_array`/`json_object`'s elements need the unevaluated `args` —
    // see `json_composed_value` — so they bypass the generic `values` pass
    // below entirely, the same way `coalesce`/`ifnull` do above.
    if matches!(
        func,
        ScalarFunc::JsonSet | ScalarFunc::JsonInsert | ScalarFunc::JsonReplace
    ) {
        return json_put_fn(func, args, row, computed, env);
    }
    if func == ScalarFunc::JsonArray {
        return json_array_fn(args, row, computed, env);
    }
    if func == ScalarFunc::JsonObject {
        return json_object_fn(args, row, computed, env);
    }

    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        values.push(evaluate(arg, row, computed, env)?);
    }

    match func {
        ScalarFunc::Coalesce | ScalarFunc::IfNull => unreachable!("handled above"),
        ScalarFunc::Length => Ok(match &values[0] {
            Value::Null => Value::Null,
            // Characters for text, bytes for everything else — and for a
            // number, the bytes of the text SQLite would render it as.
            Value::Text(s) => Value::Integer(s.chars().count() as i64),
            Value::Blob(bytes) => Value::Integer(bytes.len() as i64),
            other => Value::Integer(as_text(other)?.len() as i64),
        }),
        ScalarFunc::Upper | ScalarFunc::Lower => Ok(match &values[0] {
            Value::Null => Value::Null,
            other => {
                let text = as_text(other)?;
                Value::Text(if func == ScalarFunc::Upper {
                    text.to_ascii_uppercase()
                } else {
                    text.to_ascii_lowercase()
                })
            }
        }),
        ScalarFunc::Substr => substr(&values),
        ScalarFunc::Trim | ScalarFunc::LTrim | ScalarFunc::RTrim => trim(func, &values),
        ScalarFunc::Replace => replace(&values),
        ScalarFunc::Instr => instr(&values),
        ScalarFunc::Abs => match &values[0] {
            Value::Null => Ok(Value::Null),
            Value::Integer(i) => {
                if *i == i64::MIN {
                    // SQLite raises rather than returning the same negative
                    // number back, which is what two's complement would do.
                    Err(Error::Type("integer overflow".to_string()))
                } else {
                    Ok(Value::Integer(i.abs()))
                }
            }
            // `-0.0` stays `-0.0`: SQLite negates only when the value is
            // strictly less than zero, and `f64::abs` would not.
            other => {
                let r = value_double(other);
                Ok(Value::Real(if r < 0.0 { -r } else { r }))
            }
        },
        ScalarFunc::Round => round(&values),
        // `nullif` and the scalar `min`/`max` are SQLite's three
        // `SQLITE_FUNC_NEEDCOLL` scalars: the comparison they make is under the
        // collation of the first argument that has one, which the planner
        // resolved into `collation`.
        ScalarFunc::NullIf => Ok(
            if mem_cmp(&values[0], &values[1], collation) == Ordering::Equal {
                Value::Null
            } else {
                values[0].clone()
            },
        ),
        ScalarFunc::Min | ScalarFunc::Max => {
            // Any `NULL` argument wins outright — the scalar forms are not the
            // computed, which skip `NULL`s instead.
            if values.contains(&Value::Null) {
                return Ok(Value::Null);
            }
            let mut best = 0;
            for index in 1..values.len() {
                let ordering = mem_cmp(&values[best], &values[index], collation);
                let take = match func {
                    ScalarFunc::Min => ordering != Ordering::Less,
                    _ => ordering == Ordering::Less,
                };
                if take {
                    best = index;
                }
            }
            Ok(values[best].clone())
        }
        ScalarFunc::Hex => Ok(Value::Text(hex_of(&values[0])?)),
        ScalarFunc::OctetLength => Ok(match &values[0] {
            Value::Null => Value::Null,
            Value::Text(s) => Value::Integer(s.len() as i64),
            Value::Blob(bytes) => Value::Integer(bytes.len() as i64),
            other => Value::Integer(as_text(other)?.len() as i64),
        }),
        ScalarFunc::Unhex => unhex(&values[0]),
        ScalarFunc::MysqlSubstr => mysql_substr(&values),
        ScalarFunc::MysqlHex => mysql_hex(&values[0]),
        ScalarFunc::MysqlNullIf => mysql_nullif(&values, collation),
        ScalarFunc::MysqlRound => mysql_round(&values),
        // Straight from the injected generator: the core has no other source,
        // and a simulation replays the same stream.
        ScalarFunc::Random => {
            let raw = env.next_u64() as i64;
            Ok(Value::Integer(if raw < 0 {
                // Never `i64::MIN`, whose negation is itself.
                -(raw & i64::MAX)
            } else {
                raw
            }))
        }
        ScalarFunc::Date
        | ScalarFunc::Time
        | ScalarFunc::DateTime
        | ScalarFunc::Strftime
        | ScalarFunc::UnixEpoch => datetime::call(func, &values, env.now_micros),
        ScalarFunc::CurrentTimestamp => datetime::call(ScalarFunc::DateTime, &[], env.now_micros),
        ScalarFunc::CurrentDate => datetime::call(ScalarFunc::Date, &[], env.now_micros),
        ScalarFunc::CurrentTime => datetime::call(ScalarFunc::Time, &[], env.now_micros),
        ScalarFunc::Json => json_fn(&values[0]),
        ScalarFunc::JsonValid => Ok(json_valid_fn(&values[0])),
        ScalarFunc::JsonType => json_type_fn(&values),
        ScalarFunc::JsonQuote => Ok(Value::Text(json::write(&json_leaf(&values[0])?))),
        ScalarFunc::JsonArrayLength => json_array_length_fn(&values),
        ScalarFunc::JsonExtract => json_extract_fn(&values),
        ScalarFunc::JsonRemove => json_remove_fn(&values),
        // `json_set`/`json_insert`/`json_replace`/`json_array`/`json_object`
        // are handled before this match even starts — see the top of this
        // function — because their value arguments need the unevaluated
        // `args` for composition, which the generic `values` pass above
        // already threw away by unwrapping a single-path `json_extract`.
        ScalarFunc::JsonSet
        | ScalarFunc::JsonInsert
        | ScalarFunc::JsonReplace
        | ScalarFunc::JsonArray
        | ScalarFunc::JsonObject => {
            unreachable!("handled before the generic `values` pass")
        }
    }
}

/// A value as an `i64`, the way `sqlite3_value_int64` reads one. `NULL` is `0`.
fn value_i64(value: &Value) -> i64 {
    match value {
        Value::Null => 0,
        Value::Integer(i) => *i,
        Value::Real(r) => *r as i64,
        Value::Text(s) => text_to_integer(s),
        Value::Blob(bytes) => text_to_integer(&String::from_utf8_lossy(bytes)),
        Value::Vector(_) => 0,
    }
}

/// A value as an `f64`, the way `sqlite3_value_double` reads one.
fn value_double(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Integer(i) => *i as f64,
        Value::Real(r) => *r,
        Value::Text(s) => text_to_real(s),
        Value::Blob(bytes) => text_to_real(&String::from_utf8_lossy(bytes)),
        Value::Vector(_) => 0.0,
    }
}

/// The default `SQLITE_LIMIT_LENGTH`, which is what `substr(X, Y)` uses as its
/// unwritten third argument.
const SUBSTR_DEFAULT_LENGTH: i64 = 1_000_000_000;

fn substr(values: &[Value]) -> Result<Value> {
    if values[0] == Value::Null {
        return Ok(Value::Null);
    }
    let is_blob = matches!(values[0], Value::Blob(_));
    let units: Vec<char>;
    let bytes: Vec<u8>;
    let len: i64;
    if is_blob {
        bytes = match &values[0] {
            Value::Blob(b) => b.clone(),
            _ => unreachable!("checked above"),
        };
        units = Vec::new();
        len = bytes.len() as i64;
    } else {
        units = as_text(&values[0])?.chars().collect();
        bytes = Vec::new();
        len = units.len() as i64;
    }

    let mut p1 = i64::from(value_i64(&values[1]) as i32);
    let mut p2 = match values.get(2) {
        Some(value) => i64::from(value_i64(value) as i32),
        None => SUBSTR_DEFAULT_LENGTH,
    };

    // SQLite's index arithmetic, verbatim: `Y` is 1-based, a negative `Y`
    // counts back from the end, and a negative `Z` takes the characters
    // *preceding* `Y` rather than following it.
    if p1 < 0 {
        p1 += len;
        if p1 < 0 {
            p2 += p1;
            if p2 < 0 {
                p2 = 0;
            }
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 -= 1;
    } else if p2 > 0 {
        p2 -= 1;
    }
    if p2 < 0 {
        p1 += p2;
        p2 = -p2;
        if p1 < 0 {
            p2 += p1;
            p1 = 0;
        }
    }

    let start = p1.clamp(0, len) as usize;
    let end = (p1.saturating_add(p2)).clamp(0, len) as usize;
    Ok(if is_blob {
        Value::Blob(bytes[start..end].to_vec())
    } else {
        Value::Text(units[start..end].iter().collect())
    })
}

fn trim(func: ScalarFunc, values: &[Value]) -> Result<Value> {
    if values[0] == Value::Null {
        return Ok(Value::Null);
    }
    let set: Vec<char> = match values.get(1) {
        Some(Value::Null) => return Ok(Value::Null),
        Some(other) => as_text(other)?.chars().collect(),
        None => alloc::vec![' '],
    };
    let text: Vec<char> = as_text(&values[0])?.chars().collect();
    let mut start = 0;
    let mut end = text.len();
    if func != ScalarFunc::RTrim {
        while start < end && set.contains(&text[start]) {
            start += 1;
        }
    }
    if func != ScalarFunc::LTrim {
        while end > start && set.contains(&text[end - 1]) {
            end -= 1;
        }
    }
    Ok(Value::Text(text[start..end].iter().collect()))
}

fn replace(values: &[Value]) -> Result<Value> {
    if values.contains(&Value::Null) {
        return Ok(Value::Null);
    }
    let pattern = as_text(&values[1])?;
    // An empty pattern would match everywhere; SQLite hands the input back.
    if pattern.is_empty() {
        return Ok(values[0].clone());
    }
    let subject = as_text(&values[0])?;
    let replacement = as_text(&values[2])?;
    Ok(Value::Text(subject.replace(&pattern, &replacement)))
}

fn instr(values: &[Value]) -> Result<Value> {
    if values[0] == Value::Null || values[1] == Value::Null {
        return Ok(Value::Null);
    }
    // Two blobs search by byte; anything else searches by character, with a
    // lone blob read as text first.
    if let (Value::Blob(haystack), Value::Blob(needle)) = (&values[0], &values[1]) {
        if needle.is_empty() {
            return Ok(Value::Integer(1));
        }
        let found = haystack
            .windows(needle.len())
            .position(|window| window == needle.as_slice());
        return Ok(Value::Integer(found.map_or(0, |index| index as i64 + 1)));
    }
    let haystack = as_text(&values[0])?;
    let needle = as_text(&values[1])?;
    if needle.is_empty() {
        return Ok(Value::Integer(1));
    }
    Ok(Value::Integer(match haystack.find(&needle) {
        // `find` answers in bytes; SQLite answers in characters.
        Some(offset) => haystack[..offset].chars().count() as i64 + 1,
        None => 0,
    }))
}

fn round(values: &[Value]) -> Result<Value> {
    let digits = match values.get(1) {
        Some(Value::Null) => return Ok(Value::Null),
        Some(value) => (value_i64(value) as i32).clamp(0, 30),
        None => 0,
    };
    if values[0] == Value::Null {
        return Ok(Value::Null);
    }
    let mut r = value_double(&values[0]);
    // Beyond 2^52 a double has no fractional part left to round.
    if !(-4503599627370496.0..=4503599627370496.0).contains(&r) {
        // nothing to do
    } else if digits == 0 {
        r = ((r + if r < 0.0 { -0.5 } else { 0.5 }) as i64) as f64;
    } else {
        let rendered = alloc::format!("{:.*}", digits as usize, r);
        r = rendered.parse::<f64>().unwrap_or(r);
    }
    Ok(Value::Real(r))
}

/// `hex(X)`: the value's *blob* bytes in upper-case hexadecimal.
///
/// `NULL` is the empty string rather than `NULL`, because SQLite asks for the
/// value's bytes and a `NULL` has none.
fn hex_of(value: &Value) -> Result<String> {
    let bytes = match value {
        Value::Null => Vec::new(),
        Value::Blob(bytes) => bytes.clone(),
        other => as_text(other)?.into_bytes(),
    };
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    Ok(out)
}

/// `unhex(X)`: the blob `X` decodes to, or `NULL` when `X` is not a
/// well-formed hexadecimal string — an odd number of digits, or a character
/// outside `0-9A-Fa-f`. SQLite's single-argument form; the ignore-set second
/// argument is not implemented.
fn unhex(value: &Value) -> Result<Value> {
    if *value == Value::Null {
        return Ok(Value::Null);
    }
    let text = as_text(value)?;
    let bytes = text.as_bytes();
    if bytes.len() % 2 != 0 {
        return Ok(Value::Null);
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        match (hex_digit(pair[0]), hex_digit(pair[1])) {
            (Some(hi), Some(lo)) => out.push((hi << 4) | lo),
            _ => return Ok(Value::Null),
        }
    }
    Ok(Value::Blob(out))
}

/// The value of one ASCII hex digit, or `None` for anything else.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ------------------------------------------------------- MySQL primitives
//
// Shim-target-only engine primitives (AHL-465): MySQL behaviours with no
// SQLite spelling, so a plain SQL statement typed by hand never gets them by
// accident. `crates/inlaysql-server/src/mysqlfunc.rs` is the only intended
// caller, rewriting `SUBSTRING`/`HEX`/`NULLIF`/`ROUND` onto these before a
// statement reaches this engine. Each was checked against real MySQL 8.4.11
// (`docs/server.md`'s Divergences section records the corners that decided
// it) and nothing here refuses a direct call — this project refuses clauses
// it cannot honour, not functions it has.

/// `mysql_substr(s, pos[, len])` — MySQL's `SUBSTRING`, not SQLite's
/// `substr()`.
///
/// `pos` is 1-based; `0` or anything that lands outside the string is the
/// empty string rather than clamped to an end, and a non-positive `len` is
/// the empty string rather than "the characters before `pos`". Any `NULL`
/// argument is `NULL` — including `len`, which SQLite's own `substr()`
/// reads as `0` instead.
fn mysql_substr(values: &[Value]) -> Result<Value> {
    if values[0] == Value::Null || values[1] == Value::Null {
        return Ok(Value::Null);
    }
    if matches!(values.get(2), Some(Value::Null)) {
        return Ok(Value::Null);
    }

    let is_blob = matches!(values[0], Value::Blob(_));
    let units: Vec<char>;
    let bytes: Vec<u8>;
    let len: i64;
    if is_blob {
        bytes = match &values[0] {
            Value::Blob(b) => b.clone(),
            _ => unreachable!("checked above"),
        };
        units = Vec::new();
        len = bytes.len() as i64;
    } else {
        units = as_text(&values[0])?.chars().collect();
        bytes = Vec::new();
        len = units.len() as i64;
    }

    let empty = || {
        if is_blob {
            Value::Blob(Vec::new())
        } else {
            Value::Text(String::new())
        }
    };

    let pos = value_i64(&values[1]);
    if pos == 0 {
        return Ok(empty());
    }
    let start = if pos > 0 {
        pos.saturating_sub(1)
    } else {
        len.saturating_add(pos)
    };
    if start < 0 || start >= len {
        return Ok(empty());
    }

    let end = match values.get(2) {
        Some(length_value) => {
            let requested = value_i64(length_value);
            if requested <= 0 {
                start
            } else {
                start.saturating_add(requested).min(len)
            }
        }
        None => len,
    };
    if end <= start {
        return Ok(empty());
    }

    let (start, end) = (start as usize, end as usize);
    Ok(if is_blob {
        Value::Blob(bytes[start..end].to_vec())
    } else {
        Value::Text(units[start..end].iter().collect())
    })
}

/// `mysql_hex(X)` — MySQL's `HEX()`, not SQLite's `hex()`.
///
/// `NULL` stays `NULL` (SQLite's `hex()` answers `''`, because it asks for
/// the value's bytes and a `NULL` has none). A number is rendered as the
/// hexadecimal of its *value*, the way MySQL treats a numeric argument as a
/// 64-bit integer (`mysql_hex(255)` is `'FF'`, not the bytes of the text
/// `'255'`); a real is truncated toward zero first, since MySQL's own
/// behaviour for a `DOUBLE` argument goes through the same integer
/// conversion. Text and blob arguments are unchanged from `hex()`.
fn mysql_hex(value: &Value) -> Result<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Integer(i) => Ok(Value::Text(alloc::format!("{:X}", *i as u64))),
        Value::Real(r) => {
            let i = if r.is_finite() { *r as i64 } else { 0 };
            Ok(Value::Text(alloc::format!("{:X}", i as u64)))
        }
        other => Ok(Value::Text(hex_of(other)?)),
    }
}

/// Whether `a` and `b` are equal under MySQL's comparison coercion: two
/// numbers compare numerically, two strings compare byte for byte (checked
/// against MySQL's `_bin` collation, per `docs/server.md` — the general
/// `_ci` case is the separate, still-open collation gap), and a number
/// against a string reads the string as a number the way MySQL's `=` does,
/// falling back to `0` for one that is not — MySQL converts rather than
/// refusing, with a warning this project has nowhere to surface from inside
/// an expression.
///
/// `NULL` is never loosely equal to anything, including another `NULL`:
/// MySQL's own `=` is three-valued, and `mysql_nullif` (the only caller)
/// wants "was this comparison true", not "was it not false".
fn mysql_loose_eq(a: &Value, b: &Value, collation: Collation) -> Result<bool> {
    if *a == Value::Null || *b == Value::Null {
        return Ok(false);
    }
    let numeric = |v: &Value| matches!(v, Value::Integer(_) | Value::Real(_));
    if numeric(a) && numeric(b) {
        return Ok(value_double(a) == value_double(b));
    }
    if numeric(a) || numeric(b) {
        let (number, other) = if numeric(a) { (a, b) } else { (b, a) };
        let text = as_text(other)?;
        let coerced = full_number(&text).unwrap_or(0.0);
        return Ok(value_double(number) == coerced);
    }
    Ok(mem_cmp(a, b, collation) == Ordering::Equal)
}

/// `mysql_nullif(X, Y)` — MySQL's `NULLIF`, not SQLite's `nullif()`.
///
/// The two differ only in how they decide "equal": SQLite's `nullif()`
/// compares by storage class and a number never equals a string that spells
/// it, where MySQL's `=` coerces one side. `mysql_nullif(1, '1')` is `NULL`
/// in MySQL and `1` under SQLite's `nullif()`.
fn mysql_nullif(values: &[Value], collation: Collation) -> Result<Value> {
    if mysql_loose_eq(&values[0], &values[1], collation)? {
        Ok(Value::Null)
    } else {
        Ok(values[0].clone())
    }
}

/// `mysql_round(X[, Y])` — MySQL's `ROUND()` on a float argument, not
/// SQLite's `round()`.
///
/// The two differ on a halfway case: SQLite's `round()` rounds away from
/// zero (`round(2.5)` is `3`), where MySQL 8.4.11 rounds a `DOUBLE`
/// argument to even (`ROUND(2.5e0)` is `2`) — measured against a real
/// server, not assumed, and recorded in `docs/server.md`. This uses Rust's
/// own float-to-string rounding for the digit count requested, which ties
/// to even the same way, rather than SQLite's own `+0.5`-then-truncate rule
/// for zero digits. `Y` is not clamped to `0..=30` as `round()`'s is:
/// negative `Y` rounds to tens, hundreds, and so on, which is also what
/// MySQL does and `round()` cannot.
fn mysql_round(values: &[Value]) -> Result<Value> {
    let digits = match values.get(1) {
        Some(Value::Null) => return Ok(Value::Null),
        Some(value) => (value_i64(value) as i32).clamp(-30, 30),
        None => 0,
    };
    if values[0] == Value::Null {
        return Ok(Value::Null);
    }
    let r = value_double(&values[0]);
    // Beyond 2^52 a double has no fractional part left to round — the same
    // guard `round()` uses. `NaN` and the infinities fail the range check
    // too (neither `<=` comparison holds), so they take this path as well.
    if !(-4503599627370496.0..=4503599627370496.0).contains(&r) {
        return Ok(Value::Real(r));
    }
    let rounded = if digits >= 0 {
        alloc::format!("{:.*}", digits as usize, r)
            .parse::<f64>()
            .unwrap_or(r)
    } else {
        // `no_std`: no `f64::powi` without reaching for `libm`, and a plain
        // loop is simpler than a new dependency for an exponent this small
        // (`digits` is clamped to `-30..=30` just above).
        let scale = (0..-digits).fold(1.0f64, |acc, _| acc * 10.0);
        let scaled = r / scale;
        let rounded_scaled = alloc::format!("{:.0}", scaled)
            .parse::<f64>()
            .unwrap_or(scaled);
        rounded_scaled * scale
    };
    Ok(Value::Real(rounded))
}

// ----------------------------------------------------------- JSON (AHL-490)
//
// SQLite's json1 functions, over `crate::json`'s hand-rolled parser/
// serializer/path language. Every corner named in a comment below was
// checked against a real sqlite3 3.54 binary, not assumed —
// `crates/inlaysql/tests/sqllogictest/json.test` is where those checks live
// as pinned expectations.

/// Parse `value` as a JSON *document* — every JSON function's first
/// argument. `NULL` is `Ok(None)`; every caller checks that before doing
/// anything else, since `NULL` propagates rather than erroring. A `BLOB`'s
/// bytes are read as UTF-8 text without validation, matching sqlite3's own
/// `json_extract(x'7b2261223a317d', '$.a')` (typeof `integer`, value `1`); a
/// number goes through its ordinary text rendering (`as_text`), the same
/// conversion `CAST(x AS TEXT)`/`||` already use, matching
/// `json_extract(5, '$')` being `5`.
fn json_doc(value: &Value) -> Result<Option<Json>> {
    if *value == Value::Null {
        return Ok(None);
    }
    if matches!(value, Value::Vector(_)) {
        return Err(Error::Type("a VECTOR value is not JSON".to_string()));
    }
    let text = as_text(value)?;
    json::parse(&text)
        .map(Some)
        .map_err(|_| Error::Type("malformed JSON".to_string()))
}

/// Parse `value` as a JSON path. `NULL` is `Ok(None)`.
fn json_path_arg(value: &Value) -> Result<Option<json::Path>> {
    if *value == Value::Null {
        return Ok(None);
    }
    let text = as_text(value)?;
    json::parse_path(&text)
        .map(Some)
        .map_err(|_| Error::Type(alloc::format!("bad JSON path: '{text}'")))
}

/// A SQL value as an ordinary JSON leaf — not composed, just converted:
/// `NULL` to JSON `null`, a number to the matching JSON number, text to a
/// JSON string. A `BLOB`/`VECTOR` argument is an error — "JSON cannot hold
/// BLOB values" is sqlite3's own wording, checked directly
/// (`json_set('{"a":1}', '$.a', x'0102')`).
///
/// A `REAL` renders via `real_to_text` — the same fifteen-significant-digit
/// rule `CAST(x AS TEXT)` uses — deliberately unchanged by AHL-492: there is
/// no document text to preserve here, only a bare `f64`, so this stays the
/// direction `json_quote`/`json_array`/`json_object` were already checked
/// against sqlite3 on (`json_quote(3.7777777777777777)` is
/// `3.77777777777778`, `json_array(3.7777777777777777,1.0,0.1)` is
/// `[3.77777777777778,1.0,0.1]`). This same path also serves the *value*
/// argument of `json_set`/`json_insert`/`json_replace` when it isn't itself
/// JSON-producing (see `is_json_producing`/`json_composed_value` below); real
/// sqlite3 actually renders a bare `REAL` there at full round-trip precision
/// instead of fifteen digits — `json_set('{}','$.a',3.7777777777777777)` is
/// `{"a":3.7777777777777777}`, not `{"a":3.77777777777778}` — a second,
/// genuine divergence between `json_quote`-family and `json_set`-family
/// checked directly against sqlite3 while diagnosing this bug. It is left
/// unaddressed here: AHL-492 is the parser/serializer round-trip bug the
/// `json_parser` fuzz target found, this is a separate, pre-existing gap in
/// how a bare value composes into `json_set`, and closing it would mean
/// reverse-engineering sqlite3's own float-to-text chooser rather than
/// preserving text this crate already has.
fn json_leaf(value: &Value) -> Result<Json> {
    match value {
        Value::Null => Ok(Json::Null),
        Value::Integer(i) => Ok(Json::Int(*i)),
        Value::Real(r) => Ok(Json::Real(*r, real_to_text(*r))),
        Value::Text(s) => Ok(Json::Text(s.clone())),
        Value::Blob(_) => Err(Error::Type("JSON cannot hold BLOB values".to_string())),
        Value::Vector(_) => Err(Error::Type("JSON cannot hold VECTOR values".to_string())),
    }
}

/// A JSON node as its unwrapped SQL value — what a single-path
/// `json_extract`/`->>` produce for a scalar: a JSON string becomes SQL
/// `TEXT`, `true`/`false` become `1`/`0`. A composite (object/array) has no
/// unwrapped SQL form, so it stays its own JSON text — the one point where
/// `->` and `->>` answer identically, checked against sqlite3.
fn json_to_value(node: &Json) -> Value {
    match node {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Integer(i64::from(*b)),
        Json::Int(i) => Value::Integer(*i),
        Json::Real(r, _) => Value::Real(*r),
        Json::Text(s) => Value::Text(s.clone()),
        Json::Array(_) | Json::Object(_) => Value::Text(json::write(node)),
    }
}

/// Whether `expr` is one of the calls sqlite3's own "composing" rule
/// recognises when it appears as a value argument to `json_array`/
/// `json_object`/`json_set`/`json_insert`/`json_replace` — splice the JSON
/// it produces rather than stringify its `Value` result. See
/// [`crate::plan::ScalarFunc::JsonSet`]'s doc comment for what this does
/// and does not chase (a direct call, not a `CASE`/subquery wrapping one).
fn is_json_producing(expr: &Expr) -> bool {
    match expr {
        Expr::Func { func, .. } => matches!(
            func,
            ScalarFunc::Json
                | ScalarFunc::JsonExtract
                | ScalarFunc::JsonArray
                | ScalarFunc::JsonObject
                | ScalarFunc::JsonSet
                | ScalarFunc::JsonInsert
                | ScalarFunc::JsonReplace
                | ScalarFunc::JsonRemove
        ),
        Expr::Binary {
            op: BinaryOp::JsonExtractJson,
            ..
        } => true,
        _ => false,
    }
}

/// The JSON node a composing argument position (see [`is_json_producing`])
/// contributes — the single evaluation point every one of
/// `json_array`/`json_object`/`json_set`/`json_insert`/`json_replace`'s
/// value arguments goes through, instead of the generic `values` pass
/// [`call`] uses for its other functions.
///
/// That generic pass would unwrap a single-path `json_extract` straight to
/// its native SQL value, which loses exactly the distinction composition
/// needs: a JSON string `"str"` and the SQL text `str` are both
/// `Value::Text("str")`, and only evaluating `expr` here, rather than
/// reading back its already-unwrapped `Value`, still knows which one it was.
/// Checked against sqlite3:
/// `json_set('{"z":1}','$.z', json_extract('{"a":"str"}','$.a'))` is
/// `{"z":"str"}` (a JSON string), not `{"z":"\"str\""}`.
fn json_composed_value(
    expr: &Expr,
    row: &[Value],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<Json> {
    if let Expr::Func {
        func: ScalarFunc::JsonExtract,
        args,
        ..
    } = expr
    {
        if args.len() == 2 {
            let doc = evaluate(&args[0], row, computed, env)?;
            let Some(doc) = json_doc(&doc)? else {
                return Ok(Json::Null);
            };
            let path = evaluate(&args[1], row, computed, env)?;
            let Some(path) = json_path_arg(&path)? else {
                return Ok(Json::Null);
            };
            return Ok(json::get(&doc, &path).cloned().unwrap_or(Json::Null));
        }
    }
    if is_json_producing(expr) {
        match evaluate(expr, row, computed, env)? {
            Value::Null => Ok(Json::Null),
            Value::Text(s) => {
                json::parse(&s).map_err(|_| Error::Type("malformed JSON".to_string()))
            }
            other => json_leaf(&other),
        }
    } else {
        json_leaf(&evaluate(expr, row, computed, env)?)
    }
}

/// `json(X)`.
fn json_fn(value: &Value) -> Result<Value> {
    Ok(match json_doc(value)? {
        Some(doc) => Value::Text(json::write(&doc)),
        None => Value::Null,
    })
}

/// `json_valid(X)` — the one JSON function that never errors, since it
/// exists to be called on text that might not be JSON.
fn json_valid_fn(value: &Value) -> Value {
    if *value == Value::Null {
        return Value::Null;
    }
    let valid = match value {
        Value::Vector(_) => false,
        other => as_text(other)
            .ok()
            .is_some_and(|text| json::parse(&text).is_ok()),
    };
    Value::Integer(i64::from(valid))
}

/// The node `values[0]` names at `values[1]` (`$` when there is no second
/// argument), for `json_type`/`json_array_length`. `Ok(None)` is `NULL`
/// propagation (the document, or the path argument, was `NULL`); the inner
/// `Option` is whether the path — present or defaulted to `$` — matched
/// anything.
fn json_node_at<'a>(values: &'a [Value], doc: &'a Json) -> Result<Option<Option<&'a Json>>> {
    match values.get(1) {
        Some(path_value) => match json_path_arg(path_value)? {
            Some(path) => Ok(Some(json::get(doc, &path))),
            None => Ok(None),
        },
        None => Ok(Some(Some(doc))),
    }
}

/// `json_type(X[, P])`.
fn json_type_fn(values: &[Value]) -> Result<Value> {
    let Some(doc) = json_doc(&values[0])? else {
        return Ok(Value::Null);
    };
    Ok(match json_node_at(values, &doc)? {
        Some(Some(node)) => Value::Text(node.type_name().to_string()),
        _ => Value::Null,
    })
}

/// `json_array_length(X[, P])` — `0` for a node that is not an array,
/// including a scalar or an object; checked against sqlite3, which does not
/// error there.
fn json_array_length_fn(values: &[Value]) -> Result<Value> {
    let Some(doc) = json_doc(&values[0])? else {
        return Ok(Value::Null);
    };
    Ok(match json_node_at(values, &doc)? {
        Some(Some(Json::Array(items))) => Value::Integer(items.len() as i64),
        Some(Some(_)) => Value::Integer(0),
        _ => Value::Null,
    })
}

/// `json_extract(X, P, ...)` — one path unwraps to its SQL value; two or
/// more wrap the (possibly `NULL`-for-no-match) results in a JSON array.
/// `NULL` if `X` or any `P` is `NULL`.
fn json_extract_fn(values: &[Value]) -> Result<Value> {
    let Some(doc) = json_doc(&values[0])? else {
        return Ok(Value::Null);
    };
    let mut paths = Vec::with_capacity(values.len() - 1);
    for path_value in &values[1..] {
        match json_path_arg(path_value)? {
            Some(path) => paths.push(path),
            None => return Ok(Value::Null),
        }
    }
    if paths.len() == 1 {
        Ok(match json::get(&doc, &paths[0]) {
            Some(node) => json_to_value(node),
            None => Value::Null,
        })
    } else {
        let items = paths
            .iter()
            .map(|path| json::get(&doc, path).cloned().unwrap_or(Json::Null))
            .collect();
        Ok(Value::Text(json::write(&Json::Array(items))))
    }
}

/// `json_remove(X, P, ...)`. Unlike `json_set`'s pairs, a `NULL` `P` here
/// propagates `NULL` for the whole result rather than being skipped —
/// checked against sqlite3, and deliberately the other way round from
/// `json_put_fn` below. `P` equal to `$` also answers `NULL`: there is no
/// parent to remove the document out of.
fn json_remove_fn(values: &[Value]) -> Result<Value> {
    let Some(mut doc) = json_doc(&values[0])? else {
        return Ok(Value::Null);
    };
    for path_value in &values[1..] {
        let Some(path) = json_path_arg(path_value)? else {
            return Ok(Value::Null);
        };
        if json::is_root(&path) {
            return Ok(Value::Null);
        }
        if let Some(next) = json::remove(&doc, &path) {
            doc = next;
        }
    }
    Ok(Value::Text(json::write(&doc)))
}

/// `json_set`/`json_insert`/`json_replace` — one implementation, since the
/// three differ only in [`PutMode`].
fn json_put_fn(
    func: ScalarFunc,
    args: &[Expr],
    row: &[Value],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<Value> {
    let mode = match func {
        ScalarFunc::JsonSet => PutMode::Set,
        ScalarFunc::JsonInsert => PutMode::Insert,
        ScalarFunc::JsonReplace => PutMode::Replace,
        _ => unreachable!("json_put_fn only serves the Set/Insert/Replace family"),
    };
    let doc_value = evaluate(&args[0], row, computed, env)?;
    let Some(mut doc) = json_doc(&doc_value)? else {
        return Ok(Value::Null);
    };
    for pair in args[1..].as_chunks::<2>().0 {
        let path_value = evaluate(&pair[0], row, computed, env)?;
        // A `NULL` path skips this pair, leaving `doc` untouched by it —
        // checked against sqlite3; it is *not* `NULL`-propagating the way
        // `json_remove`'s paths are (see `json_remove_fn`).
        let Some(path) = json_path_arg(&path_value)? else {
            continue;
        };
        let value = json_composed_value(&pair[1], row, computed, env)?;
        doc = json::put(&doc, &path, &value, mode);
    }
    Ok(Value::Text(json::write(&doc)))
}

/// `json_array(X, ...)`.
fn json_array_fn(
    args: &[Expr],
    row: &[Value],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<Value> {
    let mut items = Vec::with_capacity(args.len());
    for arg in args {
        items.push(json_composed_value(arg, row, computed, env)?);
    }
    Ok(Value::Text(json::write(&Json::Array(items))))
}

/// `json_object(K, V, ...)` — every `K` must be `TEXT`, checked against
/// sqlite3's own wording ("json_object() labels must be TEXT"), which
/// refuses even a `NULL` label rather than treating it as a missing one.
fn json_object_fn(
    args: &[Expr],
    row: &[Value],
    computed: Computed<'_>,
    env: &Env<'_>,
) -> Result<Value> {
    let mut members = Vec::with_capacity(args.len() / 2);
    for pair in args.as_chunks::<2>().0 {
        let key_value = evaluate(&pair[0], row, computed, env)?;
        let Value::Text(key) = key_value else {
            return Err(Error::Type("json_object() labels must be TEXT".to_string()));
        };
        let value = json_composed_value(&pair[1], row, computed, env)?;
        members.push((key, value));
    }
    Ok(Value::Text(json::write(&Json::Object(members))))
}

/// `->`/`->>`.
fn json_arrow(op: BinaryOp, left: &Value, right: &Value) -> Result<Value> {
    let Some(doc) = json_doc(left)? else {
        return Ok(Value::Null);
    };
    let Some(path) = json_path_arg(right)? else {
        return Ok(Value::Null);
    };
    let Some(node) = json::get(&doc, &path) else {
        return Ok(Value::Null);
    };
    Ok(match op {
        BinaryOp::JsonExtractJson => Value::Text(json::write(node)),
        BinaryOp::JsonExtractText => json_to_value(node),
        _ => unreachable!("json_arrow only serves the -> and ->> operators"),
    })
}

/// The numeric prefix of `text` when nothing but whitespace follows it.
///
/// This is the difference between SQLite reading a number *out of* a string
/// and deciding the string *is* one: `CAST('3x' AS REAL)` is `3.0`, but `'3x'`
/// stored in a `NUMERIC` column stays text.
fn whole_number(text: &str) -> Option<&str> {
    let (prefix, _) = scan_number(text)?;
    let start = text.find(prefix)?;
    let rest = &text[start + prefix.len()..];
    rest.chars().all(char::is_whitespace).then_some(prefix)
}

/// The whole of `text` as a number, or `None` when anything but whitespace is
/// left over. This is SQLite's `sqlite3AtoF` returning a positive value.
fn full_number(text: &str) -> Option<f64> {
    whole_number(text)?.parse::<f64>().ok()
}

/// `text` under SQLite's `NUMERIC` affinity, or `None` when it does not
/// convert — the text-only half of [`numeric_affinity`], split out so that
/// [`affinity_conversion`]'s comparison-time use of the identical rule never
/// has to allocate an owned [`Value::Text`] just to ask the question, the way
/// calling [`numeric_affinity`] itself would.
fn numeric_affinity_of_text(text: &str) -> Option<Value> {
    let prefix = whole_number(text)?;
    let real = prefix.parse::<f64>().ok()?;
    // `alsoAnInt`: SQLite trusts a double-to-integer round trip only inside
    // 51 bits, so that neither direction can lose a digit; beyond that it
    // re-reads the text as an integer instead.
    const LOSSLESS: i64 = 1 << 51;
    let narrow = real as i64;
    if real == narrow as f64 && (-LOSSLESS..LOSSLESS).contains(&narrow) {
        return Some(Value::Integer(narrow));
    }
    Some(match prefix.parse::<i64>() {
        Ok(integer) => Value::Integer(integer),
        Err(_) => integer_affinity(real),
    })
}

/// Apply SQLite's `NUMERIC` affinity to a value on its way into a column.
///
/// This is `applyAffinity` with `SQLITE_AFF_NUMERIC`, and it is the reason
/// [`crate::value::DataType::Numeric`] is not a storage class: the column
/// holds whatever the value turns out to be. Text that *is* a number becomes
/// one, text that merely contains one does not, and a blob is left alone. A
/// real that an `i64` reproduces exactly is stored as an integer, which is why
/// `INSERT INTO t(n) VALUES (4.0)` reads back as `4`.
pub(crate) fn numeric_affinity(value: Value) -> Value {
    match value {
        Value::Text(text) => numeric_affinity_of_text(&text).unwrap_or(Value::Text(text)),
        Value::Real(real) => integer_affinity(real),
        other => other,
    }
}

/// `sqlite3VdbeIntegerAffinity`: a real becomes an integer when the conversion
/// is exact and lands strictly inside the 64-bit range.
///
/// The bounds are strict on purpose, and they are SQLite's: `i64::MAX` and
/// `i64::MIN` are not exactly representable as doubles, so a double that
/// converts to either of them did not come from one.
fn integer_affinity(real: f64) -> Value {
    if real.is_finite() {
        let integer = real as i64;
        if integer as f64 == real && integer > i64::MIN && integer < i64::MAX {
            return Value::Integer(integer);
        }
    }
    Value::Real(real)
}

// ------------------------------------------------------------------ date/time

/// SQLite's date and time functions, ported rather than approximated.
///
/// The whole family is one algorithm with several renderings: parse the
/// argument into a moment, apply each modifier in order, then print it. The
/// moment is held the way SQLite holds it — milliseconds of Julian Day in an
/// `i64`, plus a broken-down calendar form that is recomputed on demand — so
/// that the arithmetic matches digit for digit rather than nearly.
///
/// **The clock is injected.** `'now'` reads the microseconds this statement
/// captured from [`crate::traits::Clock`], never the host: `inlaysql-core` is
/// `no_std` and could not read a wall clock even if it wanted to, and the
/// deterministic simulation depends on that staying true.
///
/// Two modifiers are refused rather than implemented: `localtime` and `utc`
/// need the host's timezone database, which is exactly the kind of thing the
/// core cannot reach. They fail loudly instead of quietly meaning nothing.
mod datetime {
    use super::{full_number, value_double, Result};
    use crate::error::Error;
    use crate::plan::ScalarFunc;
    use crate::value::Value;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    /// Milliseconds of Julian Day at the Unix epoch: 1970-01-01T00:00:00Z.
    const UNIX_EPOCH_JD_MS: i64 = 210_866_760_000_000;
    /// The largest Julian-Day millisecond SQLite calls a date: 9999-12-31.
    const MAX_JD_MS: i64 = 464_269_060_799_999;

    /// A moment, in the two forms SQLite keeps it in.
    #[derive(Debug, Clone, Copy, Default)]
    struct Moment {
        /// Milliseconds of Julian Day.
        ijd: i64,
        y: i64,
        mo: i64,
        d: i64,
        h: i64,
        mi: i64,
        s: f64,
        /// Timezone offset in minutes, from a parsed `+HH:MM` suffix.
        tz: i64,
        valid_jd: bool,
        valid_ymd: bool,
        valid_hms: bool,
        /// The argument was a bare number whose meaning is not yet decided.
        raw_s: bool,
        /// Days a day-of-month overflow rolled forward, for `floor`.
        floor: i64,
        /// `subsec`: render fractional seconds.
        subsec: bool,
        error: bool,
    }

    impl Moment {
        fn fail(&mut self) {
            *self = Moment {
                error: true,
                ..Moment::default()
            };
        }

        fn compute_jd(&mut self) {
            if self.valid_jd {
                return;
            }
            let (mut y, mut m, d) = if self.valid_ymd {
                (self.y, self.mo, self.d)
            } else {
                (2000, 1, 1)
            };
            if !(-4713..=9999).contains(&y) || self.raw_s {
                self.fail();
                return;
            }
            if m <= 2 {
                y -= 1;
                m += 12;
            }
            let a = (y + 4800) / 100;
            let b = 38 - a + (a / 4);
            let x1 = 36525 * (y + 4716) / 100;
            let x2 = 306001 * (m + 1) / 10000;
            // The `- 1524.5` day offset is a half day, so the millisecond
            // count is computed as `(days * 2 - 3049) * 43_200_000` to stay in
            // integers: SQLite writes it as a float multiply, but every term
            // here is exact in a double, so the two agree.
            self.ijd = ((x1 + x2 + d + b) * 2 - 3049) * 43_200_000;
            self.valid_jd = true;
            if self.valid_hms {
                self.ijd += self.h * 3_600_000 + self.mi * 60_000 + (self.s * 1000.0 + 0.5) as i64;
                if self.tz != 0 {
                    self.ijd -= self.tz * 60_000;
                    self.valid_ymd = false;
                    self.valid_hms = false;
                    self.tz = 0;
                }
            }
        }

        fn compute_ymd(&mut self) {
            if self.valid_ymd {
                return;
            }
            if !self.valid_jd {
                self.y = 2000;
                self.mo = 1;
                self.d = 1;
            } else if !valid_jd(self.ijd) {
                self.fail();
                return;
            } else {
                let z = (self.ijd + 43_200_000).div_euclid(86_400_000);
                // The float constants below are SQLite's; each product is
                // exact at these magnitudes, so integer arithmetic would only
                // differ in how it is written.
                let alpha = (((z as f64) + 32044.75) / 36524.25) as i64 - 52;
                let a = z + 1 + alpha - ((alpha + 100) / 4) + 25;
                let b = a + 1524;
                let c = (((b as f64) - 122.1) / 365.25) as i64;
                let dd = (36525 * (c & 32767)) / 100;
                let e = (((b - dd) as f64) / 30.6001) as i64;
                let x1 = (30.6001 * e as f64) as i64;
                self.d = b - dd - x1;
                self.mo = if e < 14 { e - 1 } else { e - 13 };
                self.y = if self.mo > 2 { c - 4716 } else { c - 4715 };
            }
            self.valid_ymd = true;
        }

        fn compute_hms(&mut self) {
            if self.valid_hms {
                return;
            }
            self.compute_jd();
            if self.error {
                return;
            }
            let day_ms = (self.ijd + 43_200_000).rem_euclid(86_400_000);
            self.s = (day_ms % 60_000) as f64 / 1000.0;
            let day_min = day_ms / 60_000;
            self.mi = day_min % 60;
            self.h = day_min / 60;
            self.raw_s = false;
            self.valid_hms = true;
        }

        fn compute_ymd_hms(&mut self) {
            self.compute_ymd();
            self.compute_hms();
        }

        fn clear_ymd_hms_tz(&mut self) {
            self.valid_ymd = false;
            self.valid_hms = false;
            self.tz = 0;
        }

        /// How far a day-of-month overflow would roll forward, which is what
        /// the `floor` modifier rolls back.
        fn compute_floor(&mut self) {
            // The bitmask names the months with 31 days plus January, which
            // is why the first two arms answer the same thing: a day of 28 or
            // less never overflowed, and neither did one in a long month.
            self.floor = if self.d <= 28 || (1i64 << self.mo) & 0x15aa != 0 {
                0
            } else if self.mo != 2 {
                i64::from(self.d == 31)
            } else if self.y % 4 != 0 || (self.y % 100 == 0 && self.y % 400 != 0) {
                self.d - 28
            } else {
                self.d - 29
            };
        }

        fn set_raw(&mut self, r: f64) {
            self.s = r;
            self.raw_s = true;
            if (0.0..5_373_484.5).contains(&r) {
                self.ijd = (r * 86_400_000.0 + 0.5) as i64;
                self.valid_jd = true;
            }
        }

        fn days_after_monday(&self) -> i64 {
            (self.ijd + 43_200_000).div_euclid(86_400_000).rem_euclid(7)
        }

        fn days_after_sunday(&self) -> i64 {
            (self.ijd + 129_600_000)
                .div_euclid(86_400_000)
                .rem_euclid(7)
        }

        fn days_after_jan01(&self) -> i64 {
            let mut jan01 = *self;
            jan01.valid_jd = false;
            jan01.mo = 1;
            jan01.d = 1;
            jan01.compute_jd();
            (self.ijd - jan01.ijd + 43_200_000).div_euclid(86_400_000)
        }
    }

    fn valid_jd(ijd: i64) -> bool {
        (0..=MAX_JD_MS).contains(&ijd)
    }

    /// Read exactly `count` ASCII digits, checking the value against a range
    /// and the character that must follow. SQLite's `getDigits`, one spec at a
    /// time.
    fn digits(text: &[u8], at: usize, count: usize, min: i64, max: i64) -> Option<i64> {
        if at + count > text.len() {
            return None;
        }
        let mut value = 0i64;
        for byte in &text[at..at + count] {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        (min..=max).contains(&value).then_some(value)
    }

    /// `HH:MM[:SS[.FFF]]`, followed by an optional timezone.
    fn parse_hh_mm_ss(text: &[u8], moment: &mut Moment) -> bool {
        let Some(h) = digits(text, 0, 2, 0, 24) else {
            return false;
        };
        if text.get(2) != Some(&b':') {
            return false;
        }
        let Some(mi) = digits(text, 3, 2, 0, 59) else {
            return false;
        };
        let mut at = 5;
        let mut s = 0f64;
        if text.get(at) == Some(&b':') {
            at += 1;
            let Some(whole) = digits(text, at, 2, 0, 59) else {
                return false;
            };
            at += 2;
            s = whole as f64;
            if text.get(at) == Some(&b'.') && text.get(at + 1).is_some_and(u8::is_ascii_digit) {
                at += 1;
                let mut ms = 0f64;
                let mut scale = 1f64;
                while text.get(at).is_some_and(u8::is_ascii_digit) {
                    ms = ms * 10.0 + f64::from(text[at] - b'0');
                    scale *= 10.0;
                    at += 1;
                }
                ms /= scale;
                // SQLite truncates here rather than letting sub-millisecond
                // digits round the second up.
                s += if ms > 0.999 { 0.999 } else { ms };
            }
        }
        moment.valid_jd = false;
        moment.raw_s = false;
        moment.valid_hms = true;
        moment.h = h;
        moment.mi = mi;
        moment.s = s;
        parse_timezone(&text[at..], moment)
    }

    /// `Z`, `+HH:MM`, `-HH:MM`, or nothing at all.
    fn parse_timezone(text: &[u8], moment: &mut Moment) -> bool {
        let mut at = 0;
        while text.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        moment.tz = 0;
        let sign = match text.get(at) {
            Some(b'-') => -1,
            Some(b'+') => 1,
            Some(b'Z') | Some(b'z') => {
                at += 1;
                0
            }
            Some(_) => return false,
            None => return true,
        };
        if sign != 0 {
            at += 1;
            let Some(hours) = digits(text, at, 2, 0, 14) else {
                return false;
            };
            if text.get(at + 2) != Some(&b':') {
                return false;
            }
            let Some(minutes) = digits(text, at + 3, 2, 0, 59) else {
                return false;
            };
            at += 5;
            moment.tz = sign * (minutes + hours * 60);
        }
        while text.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at == text.len()
    }

    /// `YYYY-MM-DD` with an optional time after it.
    fn parse_yyyy_mm_dd(text: &[u8], moment: &mut Moment) -> bool {
        let (text, negative) = match text.first() {
            Some(b'-') => (&text[1..], true),
            _ => (text, false),
        };
        let (Some(y), Some(mo), Some(d)) = (
            digits(text, 0, 4, 0, 14712),
            digits(text, 5, 2, 1, 12),
            digits(text, 8, 2, 1, 31),
        ) else {
            return false;
        };
        if text.get(4) != Some(&b'-') || text.get(7) != Some(&b'-') {
            return false;
        }
        let mut at = 10;
        while text
            .get(at)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'T')
        {
            at += 1;
        }
        if at < text.len() {
            if !parse_hh_mm_ss(&text[at..], moment) {
                return false;
            }
        } else {
            moment.valid_hms = false;
        }
        moment.valid_jd = false;
        moment.valid_ymd = true;
        moment.y = if negative { -y } else { y };
        moment.mo = mo;
        moment.d = d;
        moment.compute_floor();
        if moment.tz != 0 {
            moment.compute_jd();
        }
        true
    }

    /// The current moment, from the clock this statement captured.
    fn now(now_micros: i64) -> Moment {
        Moment {
            ijd: UNIX_EPOCH_JD_MS + now_micros.div_euclid(1000),
            valid_jd: true,
            ..Moment::default()
        }
    }

    fn parse_date_or_time(text: &str, now_micros: i64, moment: &mut Moment) -> bool {
        let bytes = text.as_bytes();
        if parse_yyyy_mm_dd(bytes, moment) {
            return true;
        }
        *moment = Moment::default();
        if parse_hh_mm_ss(bytes, moment) {
            return true;
        }
        *moment = Moment::default();
        if text.eq_ignore_ascii_case("now") {
            *moment = now(now_micros);
            return true;
        }
        if let Some(r) = full_number(text) {
            moment.set_raw(r);
            return true;
        }
        if text.eq_ignore_ascii_case("subsec") || text.eq_ignore_ascii_case("subsecond") {
            *moment = now(now_micros);
            moment.subsec = true;
            return true;
        }
        false
    }

    /// The unit names `NNN days` and friends accept, with the limit SQLite
    /// puts on the count and the seconds each unit is worth.
    const UNITS: [(&str, f64, f64); 6] = [
        ("second", 4.6427e14, 1.0),
        ("minute", 7.7379e12, 60.0),
        ("hour", 1.2897e11, 3600.0),
        ("day", 5373485.0, 86400.0),
        ("month", 176546.0, 2592000.0),
        ("year", 14713.0, 31536000.0),
    ];

    /// Apply one modifier. `Ok(true)` applied it, `Ok(false)` could not make
    /// sense of it (which SQLite answers with `NULL`), and `Err` is a modifier
    /// this engine refuses rather than pretends to honour.
    fn apply_modifier(text: &str, index: usize, moment: &mut Moment) -> Result<bool> {
        let lower = text.to_ascii_lowercase();

        if lower == "localtime" || lower == "utc" {
            return Err(Error::Unsupported(alloc::format!(
                "the `{text}` date modifier needs the host's timezone, which inlaysql-core \
                 cannot read; convert in the application instead"
            )));
        }
        if lower == "auto" {
            if index > 1 {
                return Ok(false);
            }
            if !moment.raw_s || moment.valid_jd {
                moment.raw_s = false;
            } else if moment.s >= -210_866_760_000.0 && moment.s <= 253_402_300_799.0 {
                let r = moment.s * 1000.0 + UNIX_EPOCH_JD_MS as f64;
                moment.clear_ymd_hms_tz();
                moment.ijd = (r + 0.5) as i64;
                moment.valid_jd = true;
                moment.raw_s = false;
            }
            return Ok(true);
        }
        if lower == "julianday" {
            if index > 1 {
                return Ok(false);
            }
            if moment.valid_jd && moment.raw_s {
                moment.raw_s = false;
                return Ok(true);
            }
            return Ok(false);
        }
        if lower == "unixepoch" {
            if !moment.raw_s {
                return Ok(false);
            }
            if index > 1 {
                return Ok(false);
            }
            let r = moment.s * 1000.0 + UNIX_EPOCH_JD_MS as f64;
            if !(0.0..464_269_060_800_000.0).contains(&r) {
                return Ok(false);
            }
            moment.clear_ymd_hms_tz();
            moment.ijd = (r + 0.5) as i64;
            moment.valid_jd = true;
            moment.raw_s = false;
            return Ok(true);
        }
        if lower == "ceiling" {
            moment.compute_jd();
            moment.clear_ymd_hms_tz();
            moment.floor = 0;
            return Ok(true);
        }
        if lower == "floor" {
            moment.compute_jd();
            moment.ijd -= moment.floor * 86_400_000;
            moment.clear_ymd_hms_tz();
            return Ok(true);
        }
        if lower == "subsec" || lower == "subsecond" {
            moment.subsec = true;
            return Ok(true);
        }
        if let Some(rest) = lower.strip_prefix("start of ") {
            if !moment.valid_jd && !moment.valid_ymd && !moment.valid_hms {
                return Ok(false);
            }
            moment.compute_ymd();
            moment.valid_hms = true;
            moment.h = 0;
            moment.mi = 0;
            moment.s = 0.0;
            moment.raw_s = false;
            moment.tz = 0;
            moment.valid_jd = false;
            return Ok(match rest {
                "month" => {
                    moment.d = 1;
                    true
                }
                "year" => {
                    moment.mo = 1;
                    moment.d = 1;
                    true
                }
                "day" => true,
                _ => false,
            });
        }
        if let Some(rest) = lower.strip_prefix("weekday ") {
            let Some(r) = full_number(rest) else {
                return Ok(false);
            };
            if !(0.0..7.0).contains(&r) || (r as i64) as f64 != r {
                return Ok(false);
            }
            let target = r as i64;
            moment.compute_ymd_hms();
            moment.tz = 0;
            moment.valid_jd = false;
            moment.compute_jd();
            let mut current = moment.days_after_sunday();
            if current > target {
                current -= 7;
            }
            moment.ijd += (target - current) * 86_400_000;
            moment.clear_ymd_hms_tz();
            return Ok(true);
        }

        let bytes = text.as_bytes();
        if !matches!(bytes.first(), Some(b'+' | b'-' | b'0'..=b'9')) {
            return Ok(false);
        }
        Ok(apply_offset(text, moment))
    }

    /// The `(+|-)NNN unit`, `(+|-)YYYY-MM-DD` and `(+|-)HH:MM:SS.FFF` forms.
    fn apply_offset(text: &str, moment: &mut Moment) -> bool {
        let bytes = text.as_bytes();
        let sign = bytes[0];

        // How far the leading number runs. A `-` only ends it where it could
        // start the `YYYY-MM-DD` form.
        let mut n = 1;
        while n < bytes.len() {
            let byte = bytes[n];
            if byte == b':' || byte.is_ascii_whitespace() {
                break;
            }
            if byte == b'-' {
                if n == 5 && digits(bytes, 1, 4, 0, 14712).is_some() {
                    break;
                }
                if n == 6 && digits(bytes, 1, 5, 0, 14712).is_some() {
                    break;
                }
            }
            n += 1;
        }

        let Some(mut r) = full_number(&text[..n]) else {
            return false;
        };

        let mut tail = text;
        let mut tail_n = n;
        if bytes.get(n) == Some(&b'-') {
            // `(+|-)YYYY-MM-DD [HH:MM]`: whole years, months and days.
            if sign != b'+' && sign != b'-' {
                return false;
            }
            let (width, shift) = if n == 5 {
                (4usize, 0usize)
            } else {
                (5usize, 1usize)
            };
            let (Some(y), Some(mo), Some(d)) = (
                digits(bytes, 1, width, 0, 14712),
                digits(bytes, 2 + width, 2, 0, 12),
                digits(bytes, 5 + width, 2, 0, 31),
            ) else {
                return false;
            };
            if bytes.get(1 + width) != Some(&b'-') || bytes.get(4 + width) != Some(&b'-') {
                return false;
            }
            if mo >= 12 || d >= 31 {
                return false;
            }
            let mut days = d;
            moment.compute_ymd_hms();
            moment.valid_jd = false;
            if sign == b'-' {
                moment.y -= y;
                moment.mo -= mo;
                days = -d;
            } else {
                moment.y += y;
                moment.mo += mo;
            }
            let carry = if moment.mo > 0 {
                (moment.mo - 1) / 12
            } else {
                (moment.mo - 12) / 12
            };
            moment.y += carry;
            moment.mo -= carry * 12;
            moment.compute_floor();
            moment.compute_jd();
            moment.valid_hms = false;
            moment.valid_ymd = false;
            moment.ijd += days * 86_400_000;
            if bytes.len() == 11 + shift {
                return true;
            }
            // An `HH:MM` may follow the date part, separated by a space.
            if bytes.get(11 + shift).is_some_and(u8::is_ascii_whitespace)
                && digits(bytes, 12 + shift, 2, 0, 24).is_some()
                && bytes.get(14 + shift) == Some(&b':')
                && digits(bytes, 15 + shift, 2, 0, 59).is_some()
            {
                tail = &text[12 + shift..];
                tail_n = 2;
            } else {
                return false;
            }
        }

        if tail.as_bytes().get(tail_n) == Some(&b':') {
            // `(+|-)HH:MM[:SS[.FFF]]`: an offset within the day.
            let body = if tail.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                tail
            } else {
                &tail[1..]
            };
            let mut delta = Moment::default();
            if !parse_hh_mm_ss(body.as_bytes(), &mut delta) {
                return false;
            }
            delta.compute_jd();
            delta.ijd -= 43_200_000;
            let day = delta.ijd.div_euclid(86_400_000);
            delta.ijd -= day * 86_400_000;
            if sign == b'-' {
                delta.ijd = -delta.ijd;
            }
            moment.compute_jd();
            moment.clear_ymd_hms_tz();
            moment.ijd += delta.ijd;
            return true;
        }

        // `(+|-)NNN unit`.
        let unit = text[n..].trim_start();
        let mut width = unit.len();
        if !(3..=10).contains(&width) {
            return false;
        }
        if unit.as_bytes()[width - 1].eq_ignore_ascii_case(&b's') {
            width -= 1;
        }
        moment.compute_jd();
        let rounder = if r < 0.0 { -0.5 } else { 0.5 };
        moment.floor = 0;
        let mut applied = false;
        for (index, (name, limit, seconds)) in UNITS.iter().enumerate() {
            if name.len() != width
                || !unit[..width].eq_ignore_ascii_case(name)
                || !(-limit < r && r < *limit)
            {
                continue;
            }
            if index == 4 {
                moment.compute_ymd_hms();
                moment.mo += r as i64;
                let carry = if moment.mo > 0 {
                    (moment.mo - 1) / 12
                } else {
                    (moment.mo - 12) / 12
                };
                moment.y += carry;
                moment.mo -= carry * 12;
                moment.compute_floor();
                moment.valid_jd = false;
                r -= (r as i64) as f64;
            } else if index == 5 {
                moment.compute_ymd_hms();
                moment.y += r as i64;
                moment.compute_floor();
                moment.valid_jd = false;
                r -= (r as i64) as f64;
            }
            moment.compute_jd();
            moment.ijd += (r * 1000.0 * seconds + rounder) as i64;
            applied = true;
            break;
        }
        moment.clear_ymd_hms_tz();
        applied
    }

    /// Turn a function's arguments into a moment: the time value, then each
    /// modifier in order. `None` is SQLite's `NULL` result.
    fn resolve(args: &[Value], now_micros: i64) -> Result<Option<Moment>> {
        let mut moment = Moment::default();
        if args.is_empty() {
            moment = now(now_micros);
        } else {
            match &args[0] {
                Value::Null => return Ok(None),
                Value::Integer(_) | Value::Real(_) => moment.set_raw(value_double(&args[0])),
                other => {
                    let Ok(text) = super::as_text(other) else {
                        return Ok(None);
                    };
                    if !parse_date_or_time(&text, now_micros, &mut moment) {
                        return Ok(None);
                    }
                }
            }
        }
        for (offset, arg) in args.iter().enumerate().skip(1) {
            let Ok(text) = super::as_text(arg) else {
                return Ok(None);
            };
            if *arg == Value::Null || !apply_modifier(&text, offset, &mut moment)? {
                return Ok(None);
            }
        }
        moment.compute_jd();
        if moment.error || !valid_jd(moment.ijd) {
            return Ok(None);
        }
        // A written-out `YYYY-MM-DD` may name a day its month does not have;
        // recomputing from the Julian day normalises 2023-02-31 to 2023-03-03.
        if args.len() == 1 && moment.valid_ymd && moment.d > 28 {
            moment.valid_ymd = false;
        }
        Ok(Some(moment))
    }

    /// `%04d` of a year, with the sign in front rather than inside.
    fn year(y: i64) -> String {
        if y < 0 {
            alloc::format!("-{:04}", -y)
        } else {
            alloc::format!("{y:04}")
        }
    }

    fn seconds_text(moment: &Moment) -> String {
        if moment.subsec {
            let ms = (1000.0 * moment.s + 0.5) as i64;
            alloc::format!("{:02}.{:03}", ms / 1000, ms % 1000)
        } else {
            alloc::format!("{:02}", moment.s as i64)
        }
    }

    /// Evaluate one of the date/time functions over already-evaluated
    /// arguments.
    pub(super) fn call(func: ScalarFunc, args: &[Value], now_micros: i64) -> Result<Value> {
        if func == ScalarFunc::Strftime {
            let format = match &args[0] {
                Value::Null => return Ok(Value::Null),
                other => super::as_text(other)?,
            };
            let Some(mut moment) = resolve(&args[1..], now_micros)? else {
                return Ok(Value::Null);
            };
            moment.compute_ymd_hms();
            if moment.error {
                return Ok(Value::Null);
            }
            return Ok(match strftime(&format, &moment) {
                Some(text) => Value::Text(text),
                None => Value::Null,
            });
        }

        let Some(mut moment) = resolve(args, now_micros)? else {
            return Ok(Value::Null);
        };
        Ok(match func {
            ScalarFunc::Date => {
                moment.compute_ymd();
                if moment.error {
                    return Ok(Value::Null);
                }
                Value::Text(alloc::format!(
                    "{}-{:02}-{:02}",
                    year(moment.y),
                    moment.mo,
                    moment.d
                ))
            }
            ScalarFunc::Time => {
                moment.compute_hms();
                if moment.error {
                    return Ok(Value::Null);
                }
                Value::Text(alloc::format!(
                    "{:02}:{:02}:{}",
                    moment.h,
                    moment.mi,
                    seconds_text(&moment)
                ))
            }
            ScalarFunc::DateTime => {
                moment.compute_ymd_hms();
                if moment.error {
                    return Ok(Value::Null);
                }
                Value::Text(alloc::format!(
                    "{}-{:02}-{:02} {:02}:{:02}:{}",
                    year(moment.y),
                    moment.mo,
                    moment.d,
                    moment.h,
                    moment.mi,
                    seconds_text(&moment)
                ))
            }
            ScalarFunc::UnixEpoch => {
                if moment.subsec {
                    Value::Real((moment.ijd - UNIX_EPOCH_JD_MS) as f64 / 1000.0)
                } else {
                    Value::Integer(moment.ijd / 1000 - UNIX_EPOCH_JD_MS / 1000)
                }
            }
            other => {
                return Err(Error::Unsupported(alloc::format!(
                    "`{}` is not a date/time function",
                    other.name()
                )))
            }
        })
    }

    /// SQLite's `strftime` substitutions. `None` for an unrecognised `%X`,
    /// which SQLite answers with `NULL` for the whole call.
    fn strftime(format: &str, moment: &Moment) -> Option<String> {
        let bytes = format.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] != b'%' {
                out.push(bytes[at]);
                at += 1;
                continue;
            }
            at += 1;
            let spec = *bytes.get(at)?;
            at += 1;
            let piece = match spec {
                b'd' => alloc::format!("{:02}", moment.d),
                b'e' => alloc::format!("{:2}", moment.d),
                b'f' => alloc::format!("{:06.3}", moment.s),
                b'F' => alloc::format!("{}-{:02}-{:02}", year(moment.y), moment.mo, moment.d),
                b'G' | b'g' => {
                    let mut thursday = *moment;
                    thursday.ijd += (3 - moment.days_after_monday()) * 86_400_000;
                    thursday.valid_ymd = false;
                    thursday.compute_ymd();
                    if spec == b'g' {
                        alloc::format!("{:02}", thursday.y % 100)
                    } else {
                        year(thursday.y)
                    }
                }
                b'H' => alloc::format!("{:02}", moment.h),
                b'k' => alloc::format!("{:2}", moment.h),
                b'I' | b'l' => {
                    let mut h = moment.h;
                    if h > 12 {
                        h -= 12;
                    }
                    if h == 0 {
                        h = 12;
                    }
                    if spec == b'I' {
                        alloc::format!("{h:02}")
                    } else {
                        alloc::format!("{h:2}")
                    }
                }
                b'j' => alloc::format!("{:03}", moment.days_after_jan01() + 1),
                b'J' => super::format_g(moment.ijd as f64 / 86_400_000.0, 16, false),
                b'm' => alloc::format!("{:02}", moment.mo),
                b'M' => alloc::format!("{:02}", moment.mi),
                b'p' => if moment.h >= 12 { "PM" } else { "AM" }.to_string(),
                b'P' => if moment.h >= 12 { "pm" } else { "am" }.to_string(),
                b'R' => alloc::format!("{:02}:{:02}", moment.h, moment.mi),
                b's' => {
                    if moment.subsec {
                        alloc::format!("{:.3}", (moment.ijd - UNIX_EPOCH_JD_MS) as f64 / 1000.0)
                    } else {
                        alloc::format!("{}", moment.ijd / 1000 - UNIX_EPOCH_JD_MS / 1000)
                    }
                }
                b'S' => alloc::format!("{:02}", moment.s as i64),
                b'T' => alloc::format!("{:02}:{:02}:{:02}", moment.h, moment.mi, moment.s as i64),
                b'u' | b'w' => {
                    let day = moment.days_after_sunday();
                    if day == 0 && spec == b'u' {
                        "7".to_string()
                    } else {
                        alloc::format!("{day}")
                    }
                }
                b'U' => alloc::format!(
                    "{:02}",
                    (moment.days_after_jan01() - moment.days_after_sunday() + 7) / 7
                ),
                b'V' => {
                    let mut thursday = *moment;
                    thursday.ijd += (3 - moment.days_after_monday()) * 86_400_000;
                    thursday.valid_ymd = false;
                    thursday.compute_ymd();
                    alloc::format!("{:02}", thursday.days_after_jan01() / 7 + 1)
                }
                b'W' => alloc::format!(
                    "{:02}",
                    (moment.days_after_jan01() - moment.days_after_monday() + 7) / 7
                ),
                b'Y' => year(moment.y),
                b'%' => "%".to_string(),
                _ => return None,
            };
            out.extend_from_slice(piece.as_bytes());
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::SeededRng;
    use crate::plan::BinaryOp as Op;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    fn int(i: i64) -> Value {
        Value::Integer(i)
    }
    fn real(r: f64) -> Value {
        Value::Real(r)
    }

    /// A generator for the tests that do not care about randomness. Held
    /// separately so the `Env` can borrow it.
    fn generator() -> SharedRng {
        Rc::new(RefCell::new(Box::new(SeededRng::new(1)) as Box<dyn Rng>))
    }

    fn eval(expr: &Expr) -> Value {
        evaluate(expr, &[], Computed::NONE, &Env::new(&[], 0, generator())).unwrap()
    }

    fn bin(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
            collation: Collation::Binary,
            // Every operand these tests build is a literal, and a literal
            // carries no affinity of its own — `None` is what the real
            // planner would resolve for a tree with no column or `CAST` in
            // it either.
            affinity: CompareAffinity::None,
        }
    }

    /// The same, under a chosen collating sequence.
    fn bin_collated(op: BinaryOp, left: Expr, right: Expr, collation: Collation) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
            collation,
            affinity: CompareAffinity::None,
        }
    }
    fn lit(value: Value) -> Expr {
        Expr::Literal(value)
    }

    #[test]
    fn arithmetic_on_integers_stays_integer() {
        assert_eq!(eval(&bin(Op::Add, lit(int(1)), lit(int(2)))), int(3));
        assert_eq!(eval(&bin(Op::Sub, lit(int(5)), lit(int(3)))), int(2));
        assert_eq!(eval(&bin(Op::Mul, lit(int(2)), lit(int(3)))), int(6));
    }

    #[test]
    fn division_is_integer_when_both_operands_are_integer() {
        assert_eq!(eval(&bin(Op::Div, lit(int(10)), lit(int(4)))), int(2));
        assert_eq!(eval(&bin(Op::Div, lit(int(-10)), lit(int(4)))), int(-2));
        assert_eq!(eval(&bin(Op::Mod, lit(int(10)), lit(int(3)))), int(1));
    }

    #[test]
    fn division_by_zero_is_null() {
        assert_eq!(eval(&bin(Op::Div, lit(int(1)), lit(int(0)))), Value::Null);
        assert_eq!(
            eval(&bin(Op::Div, lit(real(1.0)), lit(real(0.0)))),
            Value::Null
        );
    }

    #[test]
    fn a_real_operand_widens_to_real() {
        assert_eq!(eval(&bin(Op::Add, lit(real(1.5)), lit(int(1)))), real(2.5));
        assert_eq!(eval(&bin(Op::Div, lit(real(10.0)), lit(int(4)))), real(2.5));
    }

    #[test]
    fn null_propagates() {
        assert_eq!(
            eval(&bin(Op::Add, lit(Value::Null), lit(int(1)))),
            Value::Null
        );
        assert_eq!(
            eval(&bin(Op::Lt, lit(Value::Null), lit(int(1)))),
            Value::Null
        );
    }

    #[test]
    fn comparison_yields_integer_truth() {
        assert_eq!(eval(&bin(Op::Gt, lit(int(2)), lit(int(1)))), int(1));
        assert_eq!(eval(&bin(Op::Eq, lit(int(2)), lit(int(1)))), int(0));
        assert_eq!(eval(&bin(Op::LtEq, lit(int(2)), lit(int(2)))), int(1));
        assert_eq!(
            eval(&bin(
                Op::NotEq,
                lit(Value::Text("a".to_string())),
                lit(Value::Text("b".to_string()))
            )),
            int(1)
        );
    }

    /// A collating sequence decides `TEXT` against `TEXT` and nothing else.
    #[test]
    fn a_collation_decides_a_text_comparison_and_no_other() {
        let t = |s: &str| lit(Value::Text(s.to_string()));

        // Case-insensitive under NOCASE, and not under BINARY.
        assert_eq!(
            eval(&bin_collated(Op::Eq, t("ada"), t("ADA"), Collation::NoCase)),
            int(1)
        );
        assert_eq!(
            eval(&bin_collated(Op::Eq, t("ada"), t("ADA"), Collation::Binary)),
            int(0)
        );
        // The *ordering* moves too, not only equality: folding is downward, so
        // `'A'` sorts above `'_'` under NOCASE and below it under BINARY.
        assert_eq!(
            eval(&bin_collated(Op::Gt, t("A"), t("_"), Collation::NoCase)),
            int(1)
        );
        assert_eq!(
            eval(&bin_collated(Op::Gt, t("A"), t("_"), Collation::Binary)),
            int(0)
        );
        // RTRIM ignores trailing spaces and only trailing spaces.
        assert_eq!(
            eval(&bin_collated(Op::Eq, t("a"), t("a   "), Collation::RTrim)),
            int(1)
        );
        assert_eq!(
            eval(&bin_collated(Op::Eq, t(" a"), t("a"), Collation::RTrim)),
            int(0)
        );
        // A number is a number and a blob is bytes, whatever the collation
        // says — SQLite consults a collating sequence for text alone.
        assert_eq!(
            eval(&bin_collated(
                Op::Eq,
                lit(int(1)),
                lit(Value::Real(1.0)),
                Collation::NoCase
            )),
            int(1)
        );
        assert_eq!(
            eval(&bin_collated(
                Op::Eq,
                lit(Value::Blob(alloc::vec![0x41])),
                lit(Value::Blob(alloc::vec![0x61])),
                Collation::NoCase
            )),
            int(0)
        );
        // And `NULL` is still unknown, before the collation is ever asked.
        assert_eq!(
            eval(&bin_collated(
                Op::Eq,
                lit(Value::Null),
                t("a"),
                Collation::NoCase
            )),
            Value::Null
        );
    }

    /// `nullif` and the scalar `min`/`max` are SQLite's three
    /// `SQLITE_FUNC_NEEDCOLL` scalars, and each compares under the collation
    /// the planner resolved for the call.
    #[test]
    fn the_three_collation_aware_scalars_use_the_resolved_collation() {
        assert_eq!(
            func_collated(
                ScalarFunc::NullIf,
                vec![text("ada"), text("ADA")],
                Collation::NoCase
            ),
            Value::Null
        );
        assert_eq!(
            func_collated(
                ScalarFunc::NullIf,
                vec![text("ada"), text("ADA")],
                Collation::Binary
            ),
            text("ada")
        );
        // `min`/`max` under a collation that makes the arguments *equal* still
        // have to pick one, and which one is not arbitrary: SQLite's
        // `minmaxFunc` takes the later argument on a tie for `min` and the
        // earlier one for `max`. Measured against sqlite3 3.54, which answers
        // `ADA` and `ada` for the NOCASE pair below.
        for collation in [Collation::Binary, Collation::NoCase] {
            assert_eq!(
                func_collated(ScalarFunc::Min, vec![text("ada"), text("ADA")], collation),
                text("ADA"),
                "{collation}"
            );
            assert_eq!(
                func_collated(ScalarFunc::Max, vec![text("ada"), text("ADA")], collation),
                text("ada"),
                "{collation}"
            );
        }
        // Where they are *not* equal under either collation, the collation is
        // what decides: `'_'` is 0x5f, between `'Z'` and `'a'`.
        assert_eq!(
            func_collated(
                ScalarFunc::Min,
                vec![text("A"), text("_")],
                Collation::Binary
            ),
            text("A")
        );
        assert_eq!(
            func_collated(
                ScalarFunc::Min,
                vec![text("A"), text("_")],
                Collation::NoCase
            ),
            text("_")
        );
    }

    #[test]
    fn unary_minus_negates() {
        let expr = Expr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(lit(int(5))),
        };
        assert_eq!(eval(&expr), int(-5));
    }

    #[test]
    fn a_column_reads_from_the_row() {
        let env = Env::new(&[], 0, generator());
        let row = [int(41), Value::Text("x".to_string())];
        assert_eq!(
            evaluate(&Expr::Column(0), &row, Computed::NONE, &env).unwrap(),
            int(41)
        );
        assert_eq!(
            evaluate(&Expr::Column(1), &row, Computed::NONE, &env).unwrap(),
            Value::Text("x".to_string())
        );
        assert!(evaluate(&Expr::Column(2), &row, Computed::NONE, &env).is_err());
    }

    #[test]
    fn a_placeholder_reads_from_the_parameters() {
        let params = [int(7), Value::Text("y".to_string())];
        let env = Env::new(&params, 0, generator());
        assert_eq!(
            evaluate(&Expr::Param(0), &[], Computed::NONE, &env).unwrap(),
            int(7)
        );
        assert_eq!(
            evaluate(&Expr::Param(1), &[], Computed::NONE, &env).unwrap(),
            Value::Text("y".to_string())
        );
        // The same plan, two bindings: the point of keeping `?` unresolved.
        let other = [int(9)];
        let env = Env::new(&other, 0, generator());
        assert_eq!(
            evaluate(&Expr::Param(0), &[], Computed::NONE, &env).unwrap(),
            int(9)
        );
    }

    #[test]
    fn an_unbound_placeholder_is_a_bind_error() {
        let params = [int(1)];
        let env = Env::new(&params, 0, generator());
        let error = evaluate(&Expr::Param(1), &[], Computed::NONE, &env).unwrap_err();
        assert!(matches!(error, Error::Bind(_)), "got {error}");
    }

    #[test]
    fn and_or_follow_three_valued_logic() {
        // 1 AND NULL -> NULL, 0 AND NULL -> 0, 1 OR NULL -> 1, 0 OR NULL -> NULL.
        assert_eq!(
            eval(&bin(Op::And, lit(int(1)), lit(Value::Null))),
            Value::Null
        );
        assert_eq!(eval(&bin(Op::And, lit(int(0)), lit(Value::Null))), int(0));
        assert_eq!(eval(&bin(Op::Or, lit(int(1)), lit(Value::Null))), int(1));
        assert_eq!(
            eval(&bin(Op::Or, lit(int(0)), lit(Value::Null))),
            Value::Null
        );
    }

    #[test]
    fn truthiness_accepts_nonzero_and_rejects_null() {
        assert!(is_truthy(&int(1)));
        assert!(is_truthy(&int(-1)));
        assert!(!is_truthy(&int(0)));
        assert!(!is_truthy(&Value::Null));
    }

    #[test]
    fn an_aggregate_reference_reads_the_computed_values() {
        let env = Env::new(&[], 0, generator());
        let expr = Expr::Agg(1);
        assert_eq!(
            evaluate(&expr, &[], Computed::aggregates(&[int(3), int(4)]), &env).unwrap(),
            int(4)
        );
        assert!(evaluate(&expr, &[], Computed::aggregates(&[int(3)]), &env).is_err());
    }

    #[test]
    fn a_window_reference_reads_the_computed_values_from_its_own_slice() {
        let env = Env::new(&[], 0, generator());
        let expr = Expr::Window(1);
        let computed = Computed {
            aggregates: &[int(100)],
            windows: &[int(3), int(4)],
        };
        assert_eq!(evaluate(&expr, &[], computed, &env).unwrap(), int(4));
        assert!(evaluate(
            &expr,
            &[],
            Computed {
                aggregates: &[],
                windows: &[int(3)]
            },
            &env
        )
        .is_err());
    }

    /// The executor hands computed a borrowed group; the tests build owned
    /// rows, so this is where the two meet.
    fn borrowed(group: &[Vec<Value>]) -> Vec<&[Value]> {
        group.iter().map(|row| row.as_slice()).collect()
    }

    fn agg(func: AggFunc, arg: Option<Expr>, group: &[Vec<Value>]) -> Value {
        let env = Env::new(&[], 0, generator());
        evaluate_aggregate(&Aggregate::plain(func, arg), &borrowed(group), &env).unwrap()
    }

    #[test]
    fn computed_ignore_nulls_and_count_rows() {
        let group = vec![vec![int(1)], vec![Value::Null], vec![int(3)]];
        let column = || Some(Expr::Column(0));

        // COUNT(*) counts rows, COUNT(col) counts non-NULL.
        assert_eq!(agg(AggFunc::Count, None, &group), int(3));
        assert_eq!(agg(AggFunc::Count, column(), &group), int(2));
        assert_eq!(agg(AggFunc::Sum, column(), &group), int(4));
        assert_eq!(agg(AggFunc::Min, column(), &group), int(1));
        assert_eq!(agg(AggFunc::Max, column(), &group), int(3));
        assert_eq!(agg(AggFunc::Avg, column(), &group), real(2.0));
    }

    #[test]
    fn an_empty_group_counts_zero_and_sums_null() {
        let group: Vec<Vec<Value>> = vec![];
        assert_eq!(agg(AggFunc::Count, None, &group), int(0));
        assert_eq!(
            agg(AggFunc::Sum, Some(Expr::Column(0)), &group),
            Value::Null
        );
        assert_eq!(
            agg(AggFunc::Avg, Some(Expr::Column(0)), &group),
            Value::Null
        );
    }

    #[test]
    fn distinct_folds_values_that_compare_equal() {
        let group = vec![
            vec![int(1)],
            vec![real(1.0)],
            vec![int(2)],
            vec![Value::Null],
            vec![Value::Text("1".to_string())],
        ];
        let env = Env::new(&[], 0, generator());
        let distinct = Aggregate {
            func: AggFunc::Count,
            arg: Some(Expr::Column(0)),
            distinct: true,
            separator: None,
            collation: Collation::Binary,
            filter: None,
        };
        // 1 and 1.0 are one value; the text '1' is a different storage class
        // and so is a second; NULL is not counted at all.
        assert_eq!(
            evaluate_aggregate(&distinct, &borrowed(&group), &env).unwrap(),
            int(3)
        );
    }

    #[test]
    fn group_concat_joins_non_null_values() {
        let group = vec![
            vec![Value::Text("a".to_string())],
            vec![Value::Null],
            vec![Value::Text("b".to_string())],
        ];
        assert_eq!(
            agg(AggFunc::GroupConcat, Some(Expr::Column(0)), &group),
            Value::Text("a,b".to_string())
        );
        assert_eq!(
            agg(AggFunc::GroupConcat, Some(Expr::Column(0)), &[]),
            Value::Null
        );
    }

    #[test]
    fn random_comes_from_the_injected_generator() {
        // The same seed twice gives the same value, which is the property the
        // deterministic simulation rests on.
        let draw = || {
            let rng: SharedRng =
                Rc::new(RefCell::new(Box::new(SeededRng::new(42)) as Box<dyn Rng>));
            let env = Env::new(&[], 0, rng);
            evaluate(
                &Expr::Func {
                    func: ScalarFunc::Random,
                    args: Vec::new(),
                    collation: Collation::Binary,
                },
                &[],
                Computed::NONE,
                &env,
            )
            .unwrap()
        };
        assert_eq!(draw(), draw());
        // And it is never the one value whose negation is itself.
        assert_ne!(draw(), int(i64::MIN));
    }

    #[test]
    fn now_comes_from_the_captured_clock_reading() {
        // 2001-09-09T01:46:40Z, in microseconds.
        let env = Env::new(&[], 1_000_000_000_000_000, generator());
        let call = |func: ScalarFunc| {
            evaluate(
                &Expr::Func {
                    func,
                    args: alloc::vec![Expr::Literal(Value::Text("now".to_string()))],
                    collation: Collation::Binary,
                },
                &[],
                Computed::NONE,
                &env,
            )
            .unwrap()
        };
        assert_eq!(
            call(ScalarFunc::DateTime),
            Value::Text("2001-09-09 01:46:40".to_string())
        );
        assert_eq!(
            call(ScalarFunc::Date),
            Value::Text("2001-09-09".to_string())
        );
        assert_eq!(call(ScalarFunc::UnixEpoch), int(1_000_000_000));
    }

    /// Call a scalar function over literal arguments.
    fn func(f: ScalarFunc, args: vec::Vec<Value>) -> Value {
        eval(&Expr::Func {
            func: f,
            args: args.into_iter().map(Expr::Literal).collect(),
            collation: Collation::Binary,
        })
    }

    /// Call a scalar function over literal arguments under a chosen collation.
    fn func_collated(f: ScalarFunc, args: vec::Vec<Value>, collation: Collation) -> Value {
        eval(&Expr::Func {
            func: f,
            args: args.into_iter().map(Expr::Literal).collect(),
            collation,
        })
    }

    fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }
    fn blob(bytes: &[u8]) -> Value {
        Value::Blob(bytes.to_vec())
    }

    // --------------------------------------------------------- AHL-465

    #[test]
    fn octet_length_counts_bytes_not_characters() {
        // `length('héllo')` is 5 (characters); `octet_length` is the byte
        // count MySQL's `LENGTH()` means — 6, because `é` is two UTF-8 bytes.
        assert_eq!(
            func(ScalarFunc::Length, vec![text("héllo")]),
            int(5),
            "length() counts characters"
        );
        assert_eq!(func(ScalarFunc::OctetLength, vec![text("héllo")]), int(6));
        assert_eq!(
            func(ScalarFunc::OctetLength, vec![blob(&[1, 2, 3])]),
            int(3)
        );
        assert_eq!(
            func(ScalarFunc::OctetLength, vec![Value::Null]),
            Value::Null
        );
        // A number renders as its text first, same fallback `length()` uses.
        assert_eq!(func(ScalarFunc::OctetLength, vec![int(255)]), int(3));
    }

    #[test]
    fn unhex_is_hexs_inverse_and_null_for_anything_malformed() {
        assert_eq!(func(ScalarFunc::Unhex, vec![text("4142")]), blob(b"AB"));
        assert_eq!(func(ScalarFunc::Unhex, vec![Value::Null]), Value::Null);
        // Odd number of digits.
        assert_eq!(func(ScalarFunc::Unhex, vec![text("414")]), Value::Null);
        // Not a hex digit.
        assert_eq!(func(ScalarFunc::Unhex, vec![text("zz")]), Value::Null);
        // Round-trips through `hex()`.
        assert_eq!(
            func(
                ScalarFunc::Unhex,
                vec![Value::Text(hex_of(&blob(b"round-trip")).unwrap())]
            ),
            blob(b"round-trip")
        );
    }

    #[test]
    fn mysql_substr_disagrees_with_sqlites_substr_on_every_measured_corner() {
        // Each of these is a row in `docs/server.md`'s Divergences table,
        // measured against MySQL 8.4.11 — `mysql_substr` must answer the
        // MySQL column, and plain `substr` (unchanged) still answers SQLite's.
        assert_eq!(
            func(ScalarFunc::MysqlSubstr, vec![text("hello"), int(0)]),
            text("")
        );
        assert_eq!(
            func(ScalarFunc::Substr, vec![text("hello"), int(0)]),
            text("hello"),
            "substr() is unchanged"
        );
        assert_eq!(
            func(ScalarFunc::MysqlSubstr, vec![text("hello"), int(0), int(3)]),
            text("")
        );
        assert_eq!(
            func(ScalarFunc::MysqlSubstr, vec![text("hello"), int(-10)]),
            text("")
        );
        assert_eq!(
            func(
                ScalarFunc::MysqlSubstr,
                vec![text("hello"), int(2), int(-1)]
            ),
            text("")
        );
        assert_eq!(
            func(
                ScalarFunc::MysqlSubstr,
                vec![text("hello"), int(1), Value::Null]
            ),
            Value::Null
        );
        assert_eq!(
            func(
                ScalarFunc::MysqlSubstr,
                vec![text("hello"), Value::Null, int(2)]
            ),
            Value::Null
        );
        // Ordinary cases still work the way MySQL's own examples do.
        assert_eq!(
            func(ScalarFunc::MysqlSubstr, vec![text("hello"), int(1)]),
            text("hello")
        );
        assert_eq!(
            func(ScalarFunc::MysqlSubstr, vec![text("hello"), int(2), int(3)]),
            text("ell")
        );
        assert_eq!(
            func(ScalarFunc::MysqlSubstr, vec![text("hello"), int(-3)]),
            text("llo")
        );
    }

    #[test]
    fn mysql_hex_treats_a_number_as_a_value_not_as_text_bytes() {
        assert_eq!(func(ScalarFunc::MysqlHex, vec![int(255)]), text("FF"));
        assert_eq!(func(ScalarFunc::Hex, vec![int(255)]), text("323535"));
        assert_eq!(func(ScalarFunc::MysqlHex, vec![Value::Null]), Value::Null);
        assert_eq!(func(ScalarFunc::Hex, vec![Value::Null]), text(""));
        assert_eq!(func(ScalarFunc::MysqlHex, vec![int(0)]), text("0"));
        // Text and blob arguments are unchanged from `hex()`.
        assert_eq!(
            func(ScalarFunc::MysqlHex, vec![text("255")]),
            text("323535")
        );
    }

    #[test]
    fn mysql_nullif_coerces_a_number_and_a_numeric_string() {
        assert_eq!(
            func(ScalarFunc::MysqlNullIf, vec![int(1), text("1")]),
            Value::Null
        );
        assert_eq!(
            func(ScalarFunc::NullIf, vec![int(1), text("1")]),
            int(1),
            "nullif() compares by storage class and never converts"
        );
        // Byte-wise comparison still applies between two strings — the
        // separate, still-open collation gap, not this primitive's job.
        assert_eq!(
            func(ScalarFunc::MysqlNullIf, vec![text("a"), text("A")]),
            text("a")
        );
        assert_eq!(func(ScalarFunc::MysqlNullIf, vec![int(1), int(2)]), int(1));
        assert_eq!(
            func(ScalarFunc::MysqlNullIf, vec![Value::Null, int(1)]),
            Value::Null
        );
    }

    #[test]
    fn mysql_round_ties_to_even_where_round_ties_away_from_zero() {
        assert_eq!(func(ScalarFunc::MysqlRound, vec![real(2.5)]), real(2.0));
        assert_eq!(
            func(ScalarFunc::Round, vec![real(2.5)]),
            real(3.0),
            "round() is unchanged: away from zero"
        );
        assert_eq!(func(ScalarFunc::MysqlRound, vec![real(3.5)]), real(4.0));
        assert_eq!(func(ScalarFunc::MysqlRound, vec![real(0.5)]), real(0.0));
        // Negative digit counts round to tens, hundreds, ... — `round()`
        // clamps the digit count to zero and cannot do this at all.
        assert_eq!(
            func(ScalarFunc::MysqlRound, vec![real(1234.5678), int(-2)]),
            real(1200.0)
        );
        assert_eq!(
            func(ScalarFunc::MysqlRound, vec![real(2.5), Value::Null]),
            Value::Null
        );
        assert_eq!(func(ScalarFunc::MysqlRound, vec![Value::Null]), Value::Null);
    }

    // --------------------------------------------------------- AHL-477

    /// A value set spanning every SQLite storage class at once — the shape
    /// `mem_cmp`'s old bug (and `engine.rs::compare_values`'s and
    /// `comparison`'s independent copies of it) needed to go wrong: some
    /// values equal by number but different in kind (`1` and `1.0`), some
    /// numbers that straddle `i64`, negative numbers, and at least two
    /// values of each of `TEXT` and `BLOB` so same-class ordering is
    /// exercised too, not only the boundaries between classes.
    fn mixed_class_corpus() -> vec::Vec<Value> {
        vec![
            Value::Null,
            int(-1_000_000),
            int(-1),
            int(0),
            int(1),
            int(1),
            real(1.0),
            int(2),
            real(1.5),
            real(-2.5),
            int(i64::MAX),
            real(1e300),
            text(""),
            text("Abc"),
            text("abc"),
            text("abd"),
            text("z"),
            blob(&[]),
            blob(&[0]),
            blob(&[1, 2]),
            blob(&[1, 2, 3]),
            blob(&[255]),
        ]
    }

    /// A borrowed comparison answers exactly what an owned one does, over
    /// every cross-class pair, every collation, and — since AHL-486 gave
    /// `compare_cells` a stage one — every affinity conversion too.
    ///
    /// `ValueRef` exists so a filter can read a row without allocating, which
    /// means a predicate can now be answered by *either* the borrowed path or
    /// the owned one depending on how the plan reached it. Two paths through
    /// the same rules is precisely the shape AHL-477 spent its run removing —
    /// there, an indexed answer could differ from a scanned one — so this
    /// pins the new pair together before it can drift. `compare_cells` is
    /// generic over [`Cell`] for that reason: one copy of the rules, checked
    /// here against both implementations of it. AHL-486 added
    /// [`affinity_conversion`], which reads `Cell::as_i64_cell` in a way the
    /// two implementations could in principle disagree about (`ValueRef`
    /// distinguishes `Integer` from `Real` directly; `Operand` delegates);
    /// looping over every affinity here is what would catch that.
    #[test]
    fn a_borrowed_comparison_answers_what_an_owned_one_does() {
        let values = mixed_class_corpus();
        for collation in [Collation::Binary, Collation::NoCase, Collation::RTrim] {
            for affinity in [
                CompareAffinity::None,
                CompareAffinity::Numeric,
                CompareAffinity::Text,
            ] {
                for a in &values {
                    for b in &values {
                        for op in [
                            BinaryOp::Eq,
                            BinaryOp::NotEq,
                            BinaryOp::Lt,
                            BinaryOp::LtEq,
                            BinaryOp::Gt,
                            BinaryOp::GtEq,
                        ] {
                            let owned = compare_cells(op, a, b, collation, affinity);
                            let borrowed = compare_cells(
                                op,
                                &ValueRef::from(a),
                                &ValueRef::from(b),
                                collation,
                                affinity,
                            );
                            match (&owned, &borrowed) {
                                (Ok(owned), Ok(borrowed)) => assert_eq!(
                                    owned, borrowed,
                                    "{a:?} {op:?} {b:?} under {collation:?}/{affinity:?}: \
                                     owned said {owned:?}, borrowed said {borrowed:?}"
                                ),
                                (Err(_), Err(_)) => {}
                                _ => panic!(
                                    "{a:?} {op:?} {b:?} under {collation:?}/{affinity:?}: one \
                                     path errored and the other did not ({owned:?} vs \
                                     {borrowed:?})"
                                ),
                            }
                        }
                    }
                }
            }
        }
    }

    /// [`mem_cmp`] is a genuine total order — reflexive, antisymmetric and
    /// transitive — over a value set that crosses every storage-class
    /// boundary, under every collation. This is the regression for the bug
    /// AHL-473 surfaced and AHL-477 fixed: the old comparator answered
    /// "equal" for a pair it had no rule for (a `TEXT`/`INTEGER` pair, say)
    /// instead of ranking it by class, which is not merely wrong for that
    /// pair — `sort_by` requires a total order, so a comparator that lies
    /// about one pair corrupts every sort built from it. Checking every
    /// triple exhaustively (rather than sampling) is a complete proof over
    /// this corpus, not a probabilistic one, and is cheap enough at this size
    /// to run on every `cargo test`.
    #[test]
    fn mem_cmp_is_a_total_order_over_every_storage_class() {
        for collation in [Collation::Binary, Collation::NoCase, Collation::RTrim] {
            let values = mixed_class_corpus();

            // Reflexive: every value compares equal to itself.
            for a in &values {
                assert_eq!(
                    mem_cmp(a, a, collation),
                    Ordering::Equal,
                    "{a:?} did not compare equal to itself under {collation:?}"
                );
            }

            // Antisymmetric: swapping the operands always reverses the
            // verdict (a non-order — the actual bug shape — answers `Equal`
            // for both directions of a pair it has no rule for, which this
            // catches just as well as a Less/Greater mixup would).
            for a in &values {
                for b in &values {
                    let forward = mem_cmp(a, b, collation);
                    let backward = mem_cmp(b, a, collation);
                    assert_eq!(
                        forward,
                        backward.reverse(),
                        "{a:?} vs {b:?} under {collation:?}: {forward:?} does not reverse to \
                         {backward:?}"
                    );
                }
            }

            // Transitive: a <= b and b <= c implies a <= c, over every
            // triple. `sort_by` assumes exactly this; a comparator that
            // breaks it can produce a silently corrupted order instead of an
            // error.
            for a in &values {
                for b in &values {
                    if mem_cmp(a, b, collation) == Ordering::Greater {
                        continue;
                    }
                    for c in &values {
                        if mem_cmp(b, c, collation) == Ordering::Greater {
                            continue;
                        }
                        assert_ne!(
                            mem_cmp(a, c, collation),
                            Ordering::Greater,
                            "{a:?} <= {b:?} <= {c:?} under {collation:?}, but {a:?} > {c:?}"
                        );
                    }
                }
            }
        }
    }

    /// The fixed class order itself, independent of the exhaustive property
    /// check above: `NULL` below every number, `INTEGER`/`REAL` interleaved
    /// by value (so `1` and `1.0` are equal, not merely both "numeric"),
    /// `TEXT` below `BLOB` — confirmed against a real sqlite3 3.54 binary.
    #[test]
    fn mem_cmp_orders_storage_classes_the_way_sqlite_does() {
        let c = Collation::Binary;
        assert_eq!(mem_cmp(&Value::Null, &int(0), c), Ordering::Less);
        assert_eq!(mem_cmp(&int(1), &real(1.0), c), Ordering::Equal);
        assert_eq!(mem_cmp(&real(1.5), &int(1), c), Ordering::Greater);
        assert_eq!(mem_cmp(&int(2), &text("abc"), c), Ordering::Less);
        assert_eq!(mem_cmp(&real(1e300), &text(""), c), Ordering::Less);
        assert_eq!(mem_cmp(&text("z"), &blob(&[0]), c), Ordering::Less);
        assert_eq!(
            mem_cmp(&blob(&[]), &text("\u{10ffff}"), c),
            Ordering::Greater
        );
    }

    /// `comparison` used to raise [`Error::Type`] for exactly this
    /// cross-class shape; it now answers by the same class order `mem_cmp`
    /// uses, matching sqlite3 (`1 < 'a'` is `1`, not an error) — **when
    /// neither side carries an affinity**, which is `CompareAffinity::None`
    /// here because every operand below is a bare literal. `1 = '1'` really
    /// is false in sqlite3 for the identical reason: a literal has no
    /// affinity for stage one to touch, confirmed against a real sqlite3
    /// 3.54 binary.
    #[test]
    fn comparison_answers_by_class_order_instead_of_erroring() {
        assert_eq!(
            comparison(
                Op::Lt,
                int(1),
                text("a"),
                Collation::Binary,
                CompareAffinity::None
            )
            .unwrap(),
            int(1)
        );
        assert_eq!(
            comparison(
                Op::Gt,
                int(1),
                text("a"),
                Collation::Binary,
                CompareAffinity::None
            )
            .unwrap(),
            int(0)
        );
        assert_eq!(
            comparison(
                Op::Eq,
                int(1),
                text("a"),
                Collation::Binary,
                CompareAffinity::None
            )
            .unwrap(),
            int(0)
        );
        assert_eq!(
            comparison(
                Op::Lt,
                text("a"),
                blob(b"a"),
                Collation::Binary,
                CompareAffinity::None
            )
            .unwrap(),
            int(1),
            "TEXT sorts below BLOB even with identical bytes"
        );
        assert_eq!(
            comparison(
                Op::Lt,
                real(1.5),
                blob(&[1, 2]),
                Collation::Binary,
                CompareAffinity::None
            )
            .unwrap(),
            int(1)
        );
        // NULL still owns three-valued logic — a cross-class pair does not
        // make a NULL operand answer by class order instead of `NULL`.
        assert_eq!(
            comparison(
                Op::Lt,
                Value::Null,
                int(1),
                Collation::Binary,
                CompareAffinity::None
            )
            .unwrap(),
            Value::Null
        );
    }

    /// Stage one of the comparison rule (AHL-486): a `TEXT` operand under
    /// `CompareAffinity::Numeric` converts when it is a well-formed number
    /// and stays text otherwise, and a numeric operand under
    /// `CompareAffinity::Text` renders as text — `INTEGER` without a
    /// trailing `.0`, matching [`crate::eval::as_text`]'s own rendering.
    /// Every one of these is checked directly against a real sqlite3 3.54
    /// binary in `sqllogictest`'s `affinity.test`; this pins the same
    /// corners at the function that decides them.
    #[test]
    fn comparison_affinity_converts_before_class_order_decides() {
        // `id = '1'` against an INTEGER column: the whole bug this closes.
        assert_eq!(
            comparison(
                Op::Eq,
                int(1),
                text("1"),
                Collation::Binary,
                CompareAffinity::Numeric
            )
            .unwrap(),
            int(1)
        );
        // `id = ' 1 '`: leading/trailing whitespace is part of "well-formed".
        assert_eq!(
            comparison(
                Op::Eq,
                int(1),
                text(" 1 "),
                Collation::Binary,
                CompareAffinity::Numeric
            )
            .unwrap(),
            int(1)
        );
        // `id = '1x'`: a partial number does not convert, so this stays a
        // class-order comparison and answers false, not an error.
        assert_eq!(
            comparison(
                Op::Eq,
                int(1),
                text("1x"),
                Collation::Binary,
                CompareAffinity::Numeric
            )
            .unwrap(),
            int(0)
        );
        // `id = 'abc'`: same — not well-formed, stays text, still no error.
        assert_eq!(
            comparison(
                Op::Eq,
                int(1),
                text("abc"),
                Collation::Binary,
                CompareAffinity::Numeric
            )
            .unwrap(),
            int(0)
        );
        // `id = '1.0'`: converts to a number that happens to equal 1.
        assert_eq!(
            comparison(
                Op::Eq,
                int(1),
                text("1.0"),
                Collation::Binary,
                CompareAffinity::Numeric
            )
            .unwrap(),
            int(1)
        );
        // `s = 1`: TEXT affinity renders the INTEGER as `"1"`, not the
        // reverse — `s` holding `'1'` matches, `'a'` does not.
        assert_eq!(
            comparison(
                Op::Eq,
                text("1"),
                int(1),
                Collation::Binary,
                CompareAffinity::Text
            )
            .unwrap(),
            int(1)
        );
        assert_eq!(
            comparison(
                Op::Eq,
                text("a"),
                int(1),
                Collation::Binary,
                CompareAffinity::Text
            )
            .unwrap(),
            int(0)
        );
        // `s = 1.0` matches a stored `'1.0'`, not a stored `'1'` — the
        // `INTEGER`/`REAL` rendering distinction `as_i64_cell` exists for.
        assert_eq!(
            comparison(
                Op::Eq,
                text("1.0"),
                real(1.0),
                Collation::Binary,
                CompareAffinity::Text
            )
            .unwrap(),
            int(1)
        );
        assert_eq!(
            comparison(
                Op::Eq,
                text("1"),
                real(1.0),
                Collation::Binary,
                CompareAffinity::Text
            )
            .unwrap(),
            int(0)
        );
        // A BLOB is never affinity-converted in either direction.
        assert_eq!(
            comparison(
                Op::Eq,
                int(1),
                blob(b"1"),
                Collation::Binary,
                CompareAffinity::Numeric
            )
            .unwrap(),
            int(0)
        );
        assert_eq!(
            comparison(
                Op::Eq,
                text("1"),
                blob(b"1"),
                Collation::Binary,
                CompareAffinity::Text
            )
            .unwrap(),
            int(0)
        );
        // NULL still short-circuits ahead of affinity entirely.
        assert_eq!(
            comparison(
                Op::Eq,
                Value::Null,
                text("1"),
                Collation::Binary,
                CompareAffinity::Numeric
            )
            .unwrap(),
            Value::Null
        );
    }
}
