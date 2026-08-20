//! Row storage on top of [redb].
//!
//! Rows and metadata share one redb file, which is what keeps an InlaySQL
//! database a single file you can copy, back up or ship.
//!
//! Writes are buffered until [`Storage::commit`] rather than held in an open
//! redb write transaction. That keeps the [`Storage`] trait free of lifetimes
//! and it is enough for this stage, where the engine commits once per
//! statement. Real transactions spanning statements arrive with MVCC.

use std::collections::BTreeMap;
use std::path::Path;

use inlaysql_core::row::RowBuf;
use inlaysql_core::storage::{row_id_from_key, row_key, table_prefix};
use inlaysql_core::{Error, Result, RowId, Storage};
use redb::{Database, ReadableDatabase, TableDefinition, TableError};

/// `table\0<row id big-endian>` -> encoded row.
const ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("rows");
/// Engine metadata: the catalog and the row-id counter.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// [`Storage`] backed by a single redb file.
pub struct RedbStorage {
    db: Database,
    /// Buffered row writes; `None` marks a delete.
    rows: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// Buffered metadata writes.
    meta: BTreeMap<String, Vec<u8>>,
}

impl RedbStorage {
    /// Open (or create) the database file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path).map_err(storage_error)?;
        Ok(Self {
            db,
            rows: BTreeMap::new(),
            meta: BTreeMap::new(),
        })
    }

    fn read_row(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(storage_error)?;
        let table = match txn.open_table(ROWS) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(storage_error(e)),
        };
        Ok(table
            .get(key)
            .map_err(storage_error)?
            .map(|value| value.value().to_vec()))
    }
}

fn storage_error(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

impl Storage for RedbStorage {
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        self.rows.insert(row_key(table, id), Some(bytes.to_vec()));
        Ok(())
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        let key = row_key(table, id);
        match self.rows.get(&key) {
            Some(pending) => Ok(pending.clone().map(RowBuf::Owned)),
            None => Ok(self.read_row(&key)?.map(RowBuf::Owned)),
        }
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        self.rows.insert(row_key(table, id), None);
        Ok(())
    }

    /// One batch of a table's rows, in row-id order.
    ///
    /// redb's `range` is lazy, so the committed side streams; `after` moves its
    /// lower edge and `limit` ends the walk. The uncommitted overlay is a
    /// `BTreeMap` this struct owns, so the same range of it is read up front —
    /// it holds only this statement's own writes — and merged as the disk side
    /// is walked. A buffered write shadows the row on disk; a buffered delete
    /// removes it.
    ///
    /// The one rule that is easy to get wrong: **a short batch must mean the
    /// table is exhausted**, because that is how
    /// [`inlaysql_core::traits::RowScan`] decides to stop. Buffered deletes
    /// remove rows from the middle of a batch, so the walk keeps going until it
    /// has `limit` rows or the range really has ended, rather than stopping
    /// after `limit` *entries read*.
    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        let prefix = table_prefix(table);
        // The bound is exclusive, and a row key is the table prefix plus a
        // big-endian row id, so "after `id`" is exactly "at or after `id + 1`".
        // A resume past the last representable row id has nothing left to read.
        let first = match after {
            Some(id) => match id.checked_add(1) {
                Some(next) => next,
                None => return Ok(Vec::new()),
            },
            None => RowId::MIN,
        };
        let low = row_key(table, first);
        let high = row_key(table, RowId::MAX);

        let mut buffered: Vec<(RowId, Option<&Vec<u8>>)> = Vec::new();
        for (key, value) in self.rows.range(low.clone()..=high.clone()) {
            if !key.starts_with(&prefix) {
                break;
            }
            buffered.push((row_id_from_key(key)?, value.as_ref()));
        }

        let mut out: Vec<(RowId, RowBuf)> = Vec::new();
        let mut next_buffered = 0usize;
        let mut disk_exhausted = true;

        let txn = self.db.begin_read().map_err(storage_error)?;
        match txn.open_table(ROWS) {
            Ok(rows) => {
                let range = rows
                    .range(low.as_slice()..=high.as_slice())
                    .map_err(storage_error)?;
                for entry in range {
                    if out.len() >= limit {
                        disk_exhausted = false;
                        break;
                    }
                    let (key, value) = entry.map_err(storage_error)?;
                    if !key.value().starts_with(&prefix) {
                        break;
                    }
                    let id = row_id_from_key(key.value())?;
                    // Everything buffered below this row comes first, so the
                    // answer stays in row-id order.
                    while next_buffered < buffered.len()
                        && buffered[next_buffered].0 < id
                        && out.len() < limit
                    {
                        if let Some(bytes) = buffered[next_buffered].1 {
                            out.push((buffered[next_buffered].0, RowBuf::Owned(bytes.clone())));
                        }
                        next_buffered += 1;
                    }
                    if out.len() >= limit {
                        disk_exhausted = false;
                        break;
                    }
                    match buffered.get(next_buffered) {
                        Some((buffered_id, overlay)) if *buffered_id == id => {
                            if let Some(bytes) = overlay {
                                out.push((id, RowBuf::Owned((*bytes).clone())));
                            }
                            next_buffered += 1;
                        }
                        _ => out.push((id, RowBuf::Owned(value.value().to_vec()))),
                    }
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(storage_error(e)),
        }

        // Buffered rows past the end of what is on disk, but only once the disk
        // side really has ended: a walk cut short by `limit` says nothing about
        // the rows beyond it, and emitting a later buffered row now would put
        // the answer out of order.
        if disk_exhausted {
            while next_buffered < buffered.len() && out.len() < limit {
                if let Some(bytes) = buffered[next_buffered].1 {
                    out.push((buffered[next_buffered].0, RowBuf::Owned(bytes.clone())));
                }
                next_buffered += 1;
            }
        }
        Ok(out)
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.meta.insert(key.to_string(), bytes.to_vec());
        Ok(())
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        if let Some(pending) = self.meta.get(key) {
            return Ok(Some(pending.clone()));
        }
        let txn = self.db.begin_read().map_err(storage_error)?;
        let table = match txn.open_table(META) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(storage_error(e)),
        };
        Ok(table
            .get(key)
            .map_err(storage_error)?
            .map(|value| value.value().to_vec()))
    }

    fn commit(&mut self) -> Result<()> {
        if self.rows.is_empty() && self.meta.is_empty() {
            return Ok(());
        }

        let txn = self.db.begin_write().map_err(storage_error)?;
        {
            let mut rows = txn.open_table(ROWS).map_err(storage_error)?;
            for (key, value) in &self.rows {
                match value {
                    Some(bytes) => {
                        rows.insert(key.as_slice(), bytes.as_slice())
                            .map_err(storage_error)?;
                    }
                    None => {
                        rows.remove(key.as_slice()).map_err(storage_error)?;
                    }
                }
            }
            let mut meta = txn.open_table(META).map_err(storage_error)?;
            for (key, value) in &self.meta {
                meta.insert(key.as_str(), value.as_slice())
                    .map_err(storage_error)?;
            }
        }
        txn.commit().map_err(storage_error)?;

        self.rows.clear();
        self.meta.clear();
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.rows.clear();
        self.meta.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inlaysql_core::traits::scan_all;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "inlaysql-storage-{name}-{}.inlay",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn uncommitted_writes_are_visible_to_the_same_statement() {
        let path = temp_path("pending");
        let mut storage = RedbStorage::open(&path).unwrap();
        storage.put_row("docs", 1, b"hello").unwrap();
        assert_eq!(
            storage.get_row("docs", 1).unwrap().as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(scan_all(&storage, "docs").unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn committed_rows_survive_reopening() {
        let path = temp_path("reopen");
        {
            let mut storage = RedbStorage::open(&path).unwrap();
            storage.put_row("docs", 2, b"world").unwrap();
            storage.put_meta("catalog", b"bytes").unwrap();
            storage.commit().unwrap();
        }
        let storage = RedbStorage::open(&path).unwrap();
        assert_eq!(
            storage.get_row("docs", 2).unwrap().as_deref(),
            Some(&b"world"[..])
        );
        assert_eq!(
            storage.get_meta("catalog").unwrap().as_deref(),
            Some(&b"bytes"[..])
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scans_are_scoped_to_one_table_and_ordered_by_row_id() {
        let path = temp_path("scan");
        let mut storage = RedbStorage::open(&path).unwrap();
        for id in [5u64, 1, 300] {
            storage.put_row("docs", id, &[id as u8]).unwrap();
        }
        storage.put_row("other", 1, b"x").unwrap();
        storage.commit().unwrap();

        let ids: Vec<RowId> = scan_all(&storage, "docs")
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(ids, vec![1, 5, 300]);
        assert_eq!(scan_all(&storage, "other").unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deletes_are_applied_on_commit() {
        let path = temp_path("delete");
        let mut storage = RedbStorage::open(&path).unwrap();
        storage.put_row("docs", 1, b"a").unwrap();
        storage.commit().unwrap();
        storage.delete_row("docs", 1).unwrap();
        assert!(
            scan_all(&storage, "docs").unwrap().is_empty(),
            "pending delete not applied"
        );
        storage.commit().unwrap();
        assert!(storage.get_row("docs", 1).unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// The batch contract, and the shape that breaks a naive implementation: a
    /// buffered delete removes a row from the *middle* of a batch, so a walk
    /// that stopped after `limit` entries *read* would come back short and tell
    /// `RowScan` the table had ended.
    #[test]
    fn a_batch_is_short_only_when_the_table_is_exhausted() {
        let path = temp_path("batch");
        let mut storage = RedbStorage::open(&path).unwrap();
        for id in 1..=20u64 {
            storage.put_row("docs", id, &[id as u8]).unwrap();
        }
        storage.commit().unwrap();
        for id in (1..=20u64).filter(|id| id % 2 == 0) {
            storage.delete_row("docs", id).unwrap();
        }

        assert_eq!(
            ids(storage.scan_batch("docs", None, 4).unwrap()),
            vec![1, 3, 5, 7]
        );
        assert_eq!(
            ids(storage.scan_batch("docs", Some(7), 4).unwrap()),
            vec![9, 11, 13, 15]
        );
        assert_eq!(
            ids(storage.scan_batch("docs", Some(15), 4).unwrap()),
            vec![17, 19],
            "the last batch is the only short one"
        );
        assert_eq!(
            ids(scan_all(&storage, "docs").unwrap()),
            (1..=20).filter(|id| id % 2 == 1).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A buffered write past the end of what is on disk must land in row-id
    /// order — and must not jump ahead of committed rows a full batch has not
    /// reached yet.
    #[test]
    fn a_buffered_write_lands_in_row_id_order_across_batches() {
        let path = temp_path("overlay-order");
        let mut storage = RedbStorage::open(&path).unwrap();
        for id in [1u64, 2, 3, 8, 9] {
            storage.put_row("docs", id, &[id as u8]).unwrap();
        }
        storage.commit().unwrap();
        // One before the committed rows end, one after them.
        storage.put_row("docs", 5, &[50]).unwrap();
        storage.put_row("docs", 12, &[120]).unwrap();
        // Overwrite a committed row too.
        storage.put_row("docs", 2, &[22]).unwrap();

        assert_eq!(
            ids(storage.scan_batch("docs", None, 2).unwrap()),
            vec![1, 2],
            "a full batch must not reach past the rows it read"
        );
        assert_eq!(
            ids(scan_all(&storage, "docs").unwrap()),
            vec![1, 2, 3, 5, 8, 9, 12]
        );
        assert_eq!(
            storage.get_row("docs", 2).unwrap().as_deref(),
            Some(&[22u8][..])
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Nothing is on disk at all: the answer is the overlay, and it still has to
    /// respect `after` and `limit`.
    #[test]
    fn an_overlay_only_scan_still_batches_and_resumes() {
        let path = temp_path("overlay-only");
        let mut storage = RedbStorage::open(&path).unwrap();
        for id in 1..=5u64 {
            storage.put_row("docs", id, &[id as u8]).unwrap();
        }
        assert_eq!(
            ids(storage.scan_batch("docs", None, 2).unwrap()),
            vec![1, 2]
        );
        assert_eq!(
            ids(storage.scan_batch("docs", Some(2), 2).unwrap()),
            vec![3, 4]
        );
        assert_eq!(
            ids(scan_all(&storage, "docs").unwrap()),
            vec![1, 2, 3, 4, 5]
        );
        let _ = std::fs::remove_file(&path);
    }

    fn ids(rows: Vec<(RowId, RowBuf)>) -> Vec<RowId> {
        rows.into_iter().map(|row| row.0).collect()
    }
}
