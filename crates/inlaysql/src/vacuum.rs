//! Whole-file compaction (Phase 2 item 6).
//!
//! `EngineOptions::page_reuse` reclaims individual pages as they are
//! superseded, which is what stops a database growing forever under
//! steady-state churn — but it never shrinks a file that already grew large
//! from, say, one big one-time `DELETE`: the free list holds those pages for
//! *this handle's own future writes*, not for giving the space back to the
//! filesystem. `vacuum` is for that case.
//!
//! # Why this is a copy-and-rename, not an in-place rewrite
//!
//! The obvious alternative — compacting the file's own pages in place — would
//! be new code in the copy-on-write B+ tree's crash-recovery-sensitive path,
//! exactly the code this project tests against ten thousand seeded crash
//! schedules before trusting it at all. This does none of that: it builds an
//! entirely new file using the same `CREATE TABLE`/`CREATE INDEX`/`INSERT`
//! statements any other caller uses — already covered by that same testing —
//! and only ever touches the *original* file with one `rename`, which is
//! atomic on the same filesystem. If anything fails before the rename, the
//! original file was never opened for writing by the temporary copy and is
//! untouched. This is the same algorithm real SQLite's own `VACUUM` uses.
//!
//! # Why this is safe under concurrent readers
//!
//! [`Database::open`] takes an exclusive OS advisory lock, held here for the
//! *entire* operation — including the final rename — so no second read-write
//! handle can open the original file while this runs; a concurrent writer is
//! not possible, not merely unlikely. A concurrent **read-only** handle (no
//! lock, by design — see [`Database::open_read_only`]) is a different story
//! but not a hazard here specifically: `vacuum` never mutates a single byte
//! of the original file. A reader already holding it open keeps its own file
//! descriptor and keeps reading a perfectly consistent pre-vacuum snapshot
//! through it, unaffected by the rename; a reader that opens the path *after*
//! the rename gets the vacuumed file, also consistent. This is the opposite
//! situation from `EngineOptions::page_reuse`, which physically overwrites
//! pages a lock-free reader might still reference — see that option's doc
//! for why *that* one needs the caller's own care.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Catalog, Collation, Database, Error, IndexKind, Result};

/// Compact the database at `path` in place: copy every table, constraint and
/// index into a fresh file and atomically replace the original with it.
///
/// Takes roughly as long as reading and rewriting the whole database once,
/// and needs free disk space for a second full copy while it runs (removed
/// automatically, whether this succeeds or fails).
pub fn vacuum(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    // `Database::open` creates a missing file rather than erroring — the
    // right default for opening a database in general, the wrong one here:
    // a typo'd path silently "vacuuming" a database that never existed into
    // being is exactly the mistake `Database::open_read_only`'s doc already
    // refuses for the same reason.
    if !path.exists() {
        return Err(Error::Storage(format!(
            "{} does not exist; vacuum only compacts an existing database",
            path.display()
        )));
    }
    // Held for the whole function, including the rename below — see the
    // module doc's "safe under concurrent readers" section for why that is
    // the load-bearing part of this function's safety, not a formality.
    let mut source = Database::open(path)?;

    let tmp_path = temp_path_beside(path, "vacuum")?;
    let result = (|| -> Result<()> {
        let mut dest = Database::open(&tmp_path)?;
        copy_schema_and_data(&mut source, &mut dest)?;
        dest.checkpoint()?;
        drop(dest);
        fs::rename(&tmp_path, path).map_err(io_error)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    drop(source);
    result
}

/// A path beside `path`, in the same directory (so the final `rename` stays
/// on one filesystem and is atomic), guaranteed to name nothing that exists
/// yet — a leftover from a previous crashed attempt at the same PID would
/// otherwise be reopened as if it already held a copy in progress rather
/// than started fresh.
///
/// `kind` names the operation in the temporary file's name, so an
/// interrupted `vacuum` and an interrupted `backup` leave distinguishable
/// debris and can never collide with each other on the same directory.
pub(crate) fn temp_path_beside(path: &Path, kind: &str) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::Storage(format!("{} has no file name", path.display())))?
        .to_string_lossy()
        .into_owned();
    let mut tmp = path.to_path_buf();
    tmp.set_file_name(format!(".{file_name}.{kind}-{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    Ok(tmp)
}

pub(crate) fn io_error(error: std::io::Error) -> Error {
    Error::Storage(error.to_string())
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn copy_schema_and_data(source: &mut Database, dest: &mut Database) -> Result<()> {
    let table_names: Vec<String> = source.catalog().tables().map(|t| t.name.clone()).collect();

    for name in &table_names {
        let sql = create_table_sql(source.catalog(), name)?;
        dest.execute(&sql, &[])?;
    }

    for name in &table_names {
        copy_rows(source, dest, name)?;
    }

    // Indexes after the data: a named `UNIQUE` (`CREATE UNIQUE INDEX`) and
    // every retrieval/scalar index declared with `CREATE INDEX`. Unnamed
    // `UNIQUE`/`CHECK`/`FOREIGN KEY` constraints and the primary key are
    // already inline in the `CREATE TABLE` text above — see
    // `create_table_sql`.
    for sql in index_statements(source.catalog()) {
        dest.execute(&sql, &[])?;
    }

    Ok(())
}

/// The `CREATE TABLE` statement that reproduces `table`, including its
/// primary key, `NOT NULL`/`DEFAULT`/`CHECK`/`FOREIGN KEY` and every unnamed
/// `UNIQUE` constraint — everything that has to be declared inside
/// `CREATE TABLE` itself rather than a separate statement afterward.
fn create_table_sql(catalog: &Catalog, table_name: &str) -> Result<String> {
    let table = catalog
        .table(table_name)
        .ok_or_else(|| Error::Catalog(format!("table `{table_name}` vanished mid-vacuum")))?;
    let constraints = catalog.constraints(table_name);

    let primary_key_columns: Vec<&str> = table
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.as_str())
        .collect();
    if primary_key_columns.len() > 1 {
        // Not a shape this engine's own CREATE TABLE grammar accepts today
        // (see `Table::rowid_alias`); refuse rather than emit SQL that would
        // not parse back.
        return Err(Error::Catalog(format!(
            "table `{table_name}` has {} primary-key columns; vacuum only \
             knows how to reconstruct the single-column form",
            primary_key_columns.len()
        )));
    }

    let mut clauses: Vec<String> = table
        .columns
        .iter()
        .map(|column| {
            let mut clause = format!("{} {}", quote_ident(&column.name), column.ty);
            if column.primary_key {
                clause.push_str(" PRIMARY KEY");
            }
            if column.not_null {
                clause.push_str(" NOT NULL");
            }
            if let Some(default) = &column.default {
                clause.push_str(" DEFAULT (");
                clause.push_str(default);
                clause.push(')');
            }
            if column.collation != Collation::Binary {
                clause.push_str(" COLLATE ");
                clause.push_str(&column.collation.to_string());
            }
            clause
        })
        .collect();

    if let Some(constraints) = constraints {
        for unique in &constraints.unique {
            if unique.name.is_none() {
                let cols = unique
                    .columns
                    .iter()
                    .map(|c| quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                clauses.push(format!("UNIQUE ({cols})"));
            }
        }
        for check in &constraints.checks {
            clauses.push(format!("CHECK ({check})"));
        }
        for fk in &constraints.foreign_keys {
            let cols = fk
                .columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            let mut clause = format!("FOREIGN KEY ({cols}) REFERENCES {}", quote_ident(&fk.table));
            if !fk.referenced.is_empty() {
                let referenced = fk
                    .referenced
                    .iter()
                    .map(|c| quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                clause.push_str(&format!(" ({referenced})"));
            }
            if let Some(on_delete) = &fk.on_delete {
                clause.push_str(" ON DELETE ");
                clause.push_str(on_delete);
            }
            if let Some(on_update) = &fk.on_update {
                clause.push_str(" ON UPDATE ");
                clause.push_str(on_update);
            }
            clauses.push(clause);
        }
    }

    Ok(format!(
        "CREATE TABLE {} ({})",
        quote_ident(&table.name),
        clauses.join(", ")
    ))
}

/// `CREATE [UNIQUE] INDEX` for every index the catalog owns that is not
/// already implied by `create_table_sql` above: a *named* `UNIQUE`
/// constraint (which came from its own `CREATE UNIQUE INDEX` originally, not
/// from inside `CREATE TABLE`) and every plain retrieval/scalar index.
/// System-generated backing indexes for an *unnamed* `UNIQUE`/primary key are
/// skipped — `CREATE TABLE` above already regenerates them as a side effect,
/// the same way it did the first time the table was created.
fn index_statements(catalog: &Catalog) -> Vec<String> {
    catalog
        .indexes()
        .filter(|index| !is_system_generated(&index.name))
        .map(|index| {
            let unique = if index.unique { "UNIQUE " } else { "" };
            // `USING BTREE` only needs to be said when it overrides the
            // column's own default inference (`docs/architecture.md`,
            // `IndexKind`'s doc): a plain `CREATE INDEX` already gets a
            // `TEXT` column full-text and a `VECTOR` column ANN by default.
            // Saying it for every `BTree` index regardless is harmless — it
            // is accepted whether or not it was necessary — and simpler than
            // working out which column types actually needed the override.
            let using = match index.kind {
                IndexKind::BTree if index.columns.len() == 1 => " USING BTREE",
                _ => "",
            };
            let cols = index
                .columns
                .iter()
                .zip(&index.collations)
                .map(|(col, collation)| {
                    if *collation == Collation::Binary {
                        quote_ident(col)
                    } else {
                        format!("{} COLLATE {collation}", quote_ident(col))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "CREATE {unique}INDEX {} ON {} ({cols}){using}",
                quote_ident(&index.name),
                quote_ident(&index.table)
            )
        })
        .collect()
}

/// Whether `name` is one this catalog generates itself for an unnamed
/// `UNIQUE`/primary-key backing index, rather than one a `CREATE INDEX`
/// statement declared — see `inlaysql_core::catalog::auto_unique_index_name`
/// and `auto_index_name`. Matched by prefix rather than imported directly:
/// those helpers are `pub(crate)` to `inlaysql-core` on purpose (an
/// application should never construct one of these names itself), and the
/// prefix is the one part of the contract a caller outside that crate is
/// meant to rely on.
fn is_system_generated(name: &str) -> bool {
    name.starts_with("__inlaysql_")
}

fn copy_rows(source: &mut Database, dest: &mut Database, table_name: &str) -> Result<()> {
    let quoted = quote_ident(table_name);
    let rows = source.query(&format!("SELECT * FROM {quoted}"), &[])?;
    if rows.rows.is_empty() {
        return Ok(());
    }

    let columns = rows
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = vec!["?"; rows.columns.len()].join(", ");
    let insert = format!("INSERT INTO {quoted} ({columns}) VALUES ({placeholders})");

    for row in rows.rows {
        dest.execute(&insert, &row)?;
    }
    Ok(())
}
