//! A prepared statement: parsed once, planned once, bound many times.
//!
//! # Why this exists
//!
//! [`crate::Engine::execute`] parses and plans from text on every call. For a
//! point read that is most of the work: one tree descent costs a few
//! microseconds, and tokenising, parsing and resolving the statement that asks
//! for it costs about as much again. A [`Statement`] does that work once and
//! keeps it.
//!
//! # The schema a plan was built for
//!
//! A plan holds column *ordinals*, not names. That is what makes it fast, and
//! it is also what makes it dangerous to keep: run a plan that says "project
//! column 2" against a table whose columns have since changed and you get the
//! wrong column back, silently and with no error anywhere.
//!
//! So a statement carries the table definition it was resolved against and
//! re-checks it before every execution. The check is an equality test on a
//! handful of names and type tags — cheap next to the tree descent it guards —
//! and a mismatch is [`Error::Stale`], never a wrong row.
//!
//! Statements are deliberately not tied to the handle that prepared them: they
//! are plain owned data, which is what lets [`crate::Engine`] hand one across a
//! thread to an async I/O worker. The schema stamp is what keeps that safe.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::catalog::{Catalog, Table};
use crate::error::{Error, Result};
use crate::plan::{ColumnInfo, Plan};
use crate::value::Value;

/// A statement that has been parsed and planned, ready to run many times.
///
/// Build one with [`crate::Engine::prepare`] and run it with
/// [`crate::Engine::run`].
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    sql: String,
    plan: Plan,
    parameters: usize,
    /// The tables the plan's ordinals refer to, as they stood at prepare time.
    /// Empty for statements that depend on no existing table.
    schema: Vec<Table>,
    /// This statement's output columns, computed once at prepare time from
    /// `plan` and `schema` — see [`Statement::columns`].
    columns: Vec<ColumnInfo>,
}

impl Statement {
    /// Assemble a statement from a freshly resolved plan.
    pub(crate) fn new(sql: &str, plan: Plan, parameters: usize, catalog: &Catalog) -> Self {
        // The plan was resolved against this catalog a moment ago, so the
        // lookups cannot miss; taking the tables from the catalog rather than
        // threading them out of the planner keeps the two in step by
        // construction.
        let schema: Vec<Table> = plan
            .tables()
            .into_iter()
            .filter_map(|name| catalog.table(name))
            .cloned()
            .collect();
        let columns = plan.output_columns(&schema);
        Self {
            sql: sql.to_string(),
            plan,
            parameters,
            schema,
            columns,
        }
    }

    /// The statement text this was prepared from.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The resolved plan.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// How many `?` placeholders the statement has.
    pub fn parameter_count(&self) -> usize {
        self.parameters
    }

    /// This statement's output columns, in projection order.
    ///
    /// Empty for a statement that produces no rows (`CREATE TABLE`, an
    /// `INSERT` without `RETURNING`, `BEGIN`, and so on) — a caller such as
    /// `COM_STMT_PREPARE` that needs to know "does this even have a result
    /// set" can read that off the length alone. A column's type is `None`
    /// where the plan does not statically know one: a computed expression,
    /// a retrieval score, or a `SELECT` with no `FROM`. SQLite draws the
    /// same line for prepared-statement metadata — `sqlite3_column_decltype`
    /// answers `NULL` for an expression too.
    pub fn columns(&self) -> &[ColumnInfo] {
        &self.columns
    }

    /// Whether running this statement can only read.
    pub fn is_read_only(&self) -> bool {
        self.plan.is_read_only()
    }

    /// Check that `params` and `catalog` are what this plan was built for.
    ///
    /// Run before every execution. Both failures are the same kind of bug —
    /// a plan being used against something it was not resolved against — and
    /// both are reported rather than absorbed.
    pub fn validate(&self, catalog: &Catalog, params: &[Value]) -> Result<()> {
        self.check_parameters(params)?;
        self.check_schema(catalog)
    }

    /// Check the bound parameters against the placeholders.
    pub fn check_parameters(&self, params: &[Value]) -> Result<()> {
        if params.len() != self.parameters {
            return Err(Error::Bind(alloc::format!(
                "statement has {} placeholder(s) but {} parameter(s) were bound",
                self.parameters,
                params.len()
            )));
        }
        Ok(())
    }

    /// Check the plan's tables against the catalog as it stands now.
    pub fn check_schema(&self, catalog: &Catalog) -> Result<()> {
        for stamped in &self.schema {
            match catalog.table(&stamped.name) {
                Some(current) if current == stamped => {}
                Some(_) => {
                    return Err(Error::Stale(alloc::format!(
                    "table `{}` has changed since this statement was prepared; prepare it again",
                    stamped.name
                )))
                }
                None => {
                    return Err(Error::Stale(alloc::format!(
                        "table `{}` no longer exists in this database; prepare the statement again",
                        stamped.name
                    )))
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Column;
    use crate::value::DataType;
    use alloc::vec;

    fn catalog_with(columns: vec::Vec<Column>) -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(Table {
                name: "docs".to_string(),
                columns,
            })
            .unwrap();
        catalog
    }

    fn docs() -> Catalog {
        catalog_with(vec![
            Column::new("id", DataType::Integer),
            Column::new("body", DataType::Text),
        ])
    }

    fn prepared(catalog: &Catalog) -> Statement {
        crate::sql::prepare("SELECT body FROM docs WHERE id = ?", catalog).unwrap()
    }

    #[test]
    fn a_statement_counts_its_placeholders() {
        let statement = prepared(&docs());
        assert_eq!(statement.parameter_count(), 1);
        assert!(statement.check_parameters(&[Value::Integer(1)]).is_ok());
        assert!(matches!(
            statement.check_parameters(&[]).unwrap_err(),
            Error::Bind(_)
        ));
    }

    #[test]
    fn the_schema_it_was_planned_against_still_validates() {
        let catalog = docs();
        assert!(prepared(&catalog).check_schema(&catalog).is_ok());
    }

    #[test]
    fn a_reordered_table_makes_the_statement_stale() {
        // Same names, same types, swapped ordinals — the case that would
        // silently return the wrong column rather than fail.
        let statement = prepared(&docs());
        let reordered = catalog_with(vec![
            Column::new("body", DataType::Text),
            Column::new("id", DataType::Integer),
        ]);
        assert!(matches!(
            statement.check_schema(&reordered).unwrap_err(),
            Error::Stale(_)
        ));
    }

    #[test]
    fn a_retyped_column_makes_the_statement_stale() {
        let statement = prepared(&docs());
        let retyped = catalog_with(vec![
            Column::new("id", DataType::Integer),
            Column::new("body", DataType::Blob),
        ]);
        assert!(matches!(
            statement.check_schema(&retyped).unwrap_err(),
            Error::Stale(_)
        ));
    }

    #[test]
    fn a_missing_table_makes_the_statement_stale() {
        let statement = prepared(&docs());
        assert!(matches!(
            statement.check_schema(&Catalog::new()).unwrap_err(),
            Error::Stale(_)
        ));
    }

    #[test]
    fn an_unrelated_table_does_not_invalidate_anything() {
        let statement = prepared(&docs());
        let mut catalog = docs();
        catalog
            .create_table(Table {
                name: "other".to_string(),
                columns: vec![Column::new("a", DataType::Integer)],
            })
            .unwrap();
        assert!(statement.check_schema(&catalog).is_ok());
    }

    #[test]
    fn a_statement_that_depends_on_no_table_is_always_valid() {
        let statement = crate::sql::prepare("SELECT 1 + ?", &Catalog::new()).unwrap();
        assert!(statement.check_schema(&Catalog::new()).is_ok());
        assert!(statement.check_schema(&docs()).is_ok());
        assert!(statement.is_read_only());
    }

    // -------------------------------------------------------- AHL-466: columns()

    #[test]
    fn a_select_of_stored_columns_reports_their_names_and_types() {
        let statement =
            crate::sql::prepare("SELECT id, body FROM docs WHERE id = ?", &docs()).unwrap();
        let columns = statement.columns();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].ty, Some(DataType::Integer));
        assert_eq!(columns[1].name, "body");
        assert_eq!(columns[1].ty, Some(DataType::Text));
    }

    #[test]
    fn a_computed_projection_has_a_label_but_no_type() {
        let statement =
            crate::sql::prepare("SELECT id + 1 AS next_id, body FROM docs", &docs()).unwrap();
        let columns = statement.columns();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "next_id");
        assert_eq!(
            columns[0].ty, None,
            "a computed expression's type is not known without running it, like SQLite's own \
             sqlite3_column_decltype"
        );
        assert_eq!(columns[1].name, "body");
        assert_eq!(columns[1].ty, Some(DataType::Text));
    }

    #[test]
    fn a_join_resolves_a_columns_type_against_the_table_it_actually_came_from() {
        let mut catalog = docs();
        catalog
            .create_table(Table {
                name: "tags".to_string(),
                columns: vec![
                    Column::new("doc_id", DataType::Integer),
                    Column::new("label", DataType::Real),
                ],
            })
            .unwrap();
        let statement = crate::sql::prepare(
            "SELECT docs.body, tags.label FROM docs JOIN tags ON docs.id = tags.doc_id",
            &catalog,
        )
        .unwrap();
        let columns = statement.columns();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "body");
        assert_eq!(columns[0].ty, Some(DataType::Text));
        assert_eq!(columns[1].name, "label");
        assert_eq!(
            columns[1].ty,
            Some(DataType::Real),
            "the joined row's ordinal for `label` lands past the whole of `docs`, not just `tags`"
        );
    }

    #[test]
    fn insert_returning_resolves_columns_against_the_target_table_only() {
        let statement = crate::sql::prepare(
            "INSERT INTO docs (id, body) VALUES (?, ?) RETURNING id, body",
            &docs(),
        )
        .unwrap();
        let columns = statement.columns();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].ty, Some(DataType::Integer));
        assert_eq!(columns[1].name, "body");
        assert_eq!(columns[1].ty, Some(DataType::Text));
    }

    #[test]
    fn a_statement_with_no_result_set_reports_no_columns() {
        for sql in [
            "INSERT INTO docs (id, body) VALUES (?, ?)",
            "CREATE TABLE other (a INTEGER)",
            "BEGIN",
        ] {
            let statement = crate::sql::prepare(sql, &docs()).unwrap();
            assert!(
                statement.columns().is_empty(),
                "{sql} should report no columns, got {:?}",
                statement.columns()
            );
        }
    }

    #[test]
    fn a_scalar_select_has_a_label_but_no_type() {
        let statement = crate::sql::prepare("SELECT 1 + ? AS total", &Catalog::new()).unwrap();
        let columns = statement.columns();
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "total");
        assert_eq!(columns[0].ty, None);
    }
}
