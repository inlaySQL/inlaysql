//! Row storage in a `BTreeMap`.
//!
//! Like the on-disk backends, writes are buffered until [`Storage::commit`]:
//! a `put_row` goes into a pending overlay that reads see immediately but that
//! only reaches the committed maps on `commit`. That is what makes an explicit
//! rollback meaningful here — discarding the overlay discards the writes.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ops::Bound;

use crate::error::Result;
use crate::row::RowBuf;
use crate::traits::{RowId, Storage};

/// In-memory [`Storage`].
///
/// `commit` folds the pending overlay into the committed maps; there is nothing
/// to make durable, but the engine still calls it, which keeps the call pattern
/// identical to the on-disk backends. `rollback` drops the overlay.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemStorage {
    tables: BTreeMap<String, BTreeMap<RowId, Vec<u8>>>,
    meta: BTreeMap<String, Vec<u8>>,
    /// Buffered row writes; `None` marks a delete.
    pending_rows: BTreeMap<String, BTreeMap<RowId, Option<Vec<u8>>>>,
    /// Buffered metadata writes.
    pending_meta: BTreeMap<String, Vec<u8>>,
    /// A `WITHOUT ROWID` table's rows, keyed by primary key bytes rather
    /// than row id — a second map rather than folding into `tables` because
    /// nothing here needs the two to share a key space the way the on-disk
    /// backend's one tree does; keeping them apart is what let every
    /// existing row-id path above stay untouched by this addition.
    tables_keyed: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>,
    /// [`MemStorage::tables_keyed`]'s buffered writes, the same shape
    /// [`MemStorage::pending_rows`] is to `tables`.
    pending_rows_keyed: BTreeMap<String, BTreeMap<Vec<u8>, Option<Vec<u8>>>>,
    /// Scalar secondary index entries. A `BTreeMap` keyed by the raw entry key
    /// is the same ordered key space the tree gives, which is all an index
    /// needs from a backend.
    index: BTreeMap<Vec<u8>, ()>,
    /// Buffered index writes; `None` marks a delete.
    pending_index: BTreeMap<Vec<u8>, Option<()>>,
    commits: usize,
}

impl MemStorage {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times [`Storage::commit`] has been called. Simulation tests use
    /// this to check the engine commits once per statement.
    pub fn commit_count(&self) -> usize {
        self.commits
    }
}

impl Storage for MemStorage {
    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        self.pending_rows
            .entry(table.to_ascii_lowercase())
            .or_default()
            .insert(id, Some(bytes.to_vec()));
        Ok(())
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        let table = table.to_ascii_lowercase();
        if let Some(pending) = self.pending_rows.get(&table).and_then(|rows| rows.get(&id)) {
            return Ok(pending.clone().map(RowBuf::Owned));
        }
        Ok(self
            .tables
            .get(&table)
            .and_then(|rows| rows.get(&id))
            .cloned()
            .map(RowBuf::Owned))
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        self.pending_rows
            .entry(table.to_ascii_lowercase())
            .or_default()
            .insert(id, None);
        Ok(())
    }

    /// One batch of a table's rows, committed state with the open
    /// transaction's overlay applied.
    ///
    /// The two maps are merged as they are read rather than copied and folded:
    /// a batch is bounded work, and cloning the whole table to answer
    /// `LIMIT 1` is exactly what the streaming executor exists to stop doing.
    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        let table = table.to_ascii_lowercase();
        let start = match after {
            Some(id) => Bound::Excluded(id),
            None => Bound::Unbounded,
        };
        let empty_rows = BTreeMap::new();
        let empty_pending = BTreeMap::new();
        let mut committed = self
            .tables
            .get(&table)
            .unwrap_or(&empty_rows)
            .range((start, Bound::Unbounded))
            .peekable();
        let mut pending = self
            .pending_rows
            .get(&table)
            .unwrap_or(&empty_pending)
            .range((start, Bound::Unbounded))
            .peekable();

        let mut out = Vec::new();
        while out.len() < limit {
            // Whichever id comes next; when both sides hold it, the pending
            // write is the one a reader must see, and a pending delete removes
            // the committed row rather than shadowing it with bytes.
            let next = match (committed.peek(), pending.peek()) {
                (None, None) => break,
                (Some((id, _)), None) => **id,
                (None, Some((id, _))) => **id,
                (Some((left, _)), Some((right, _))) => (**left).min(**right),
            };
            let overlay = pending.next_if(|(id, _)| **id == next).map(|(_, v)| v);
            let stored = committed.next_if(|(id, _)| **id == next).map(|(_, v)| v);
            match overlay {
                Some(Some(bytes)) => out.push((next, RowBuf::Owned(bytes.clone()))),
                Some(None) => {}
                None => {
                    if let Some(bytes) = stored {
                        out.push((next, RowBuf::Owned(bytes.clone())));
                    }
                }
            }
        }
        Ok(out)
    }

    fn put_row_keyed(&mut self, table: &str, key: &[u8], bytes: &[u8]) -> Result<()> {
        self.pending_rows_keyed
            .entry(table.to_ascii_lowercase())
            .or_default()
            .insert(key.to_vec(), Some(bytes.to_vec()));
        Ok(())
    }

    fn get_row_keyed(&self, table: &str, key: &[u8]) -> Result<Option<RowBuf>> {
        let table = table.to_ascii_lowercase();
        if let Some(pending) = self
            .pending_rows_keyed
            .get(&table)
            .and_then(|rows| rows.get(key))
        {
            return Ok(pending.clone().map(RowBuf::Owned));
        }
        Ok(self
            .tables_keyed
            .get(&table)
            .and_then(|rows| rows.get(key))
            .cloned()
            .map(RowBuf::Owned))
    }

    fn delete_row_keyed(&mut self, table: &str, key: &[u8]) -> Result<()> {
        self.pending_rows_keyed
            .entry(table.to_ascii_lowercase())
            .or_default()
            .insert(key.to_vec(), None);
        Ok(())
    }

    /// The same committed-plus-overlay merge [`MemStorage::scan_batch`]
    /// does, keyed by primary key bytes instead of row id.
    fn scan_batch_keyed(
        &self,
        table: &str,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        let table = table.to_ascii_lowercase();
        let start = match after {
            Some(key) => Bound::Excluded(key.to_vec()),
            None => Bound::Unbounded,
        };
        let empty_rows = BTreeMap::new();
        let empty_pending = BTreeMap::new();
        let mut committed = self
            .tables_keyed
            .get(&table)
            .unwrap_or(&empty_rows)
            .range((start.clone(), Bound::Unbounded))
            .peekable();
        let mut pending = self
            .pending_rows_keyed
            .get(&table)
            .unwrap_or(&empty_pending)
            .range((start, Bound::Unbounded))
            .peekable();

        let mut out = Vec::new();
        while out.len() < limit {
            let next = match (committed.peek(), pending.peek()) {
                (None, None) => break,
                (Some((key, _)), None) => (*key).clone(),
                (None, Some((key, _))) => (*key).clone(),
                (Some((left, _)), Some((right, _))) => (*left).min(*right).clone(),
            };
            let overlay = pending.next_if(|(key, _)| **key == next).map(|(_, v)| v);
            let stored = committed.next_if(|(key, _)| **key == next).map(|(_, v)| v);
            match overlay {
                Some(Some(bytes)) => out.push((next, RowBuf::Owned(bytes.clone()))),
                Some(None) => {}
                None => {
                    if let Some(bytes) = stored {
                        out.push((next, RowBuf::Owned(bytes.clone())));
                    }
                }
            }
        }
        Ok(out)
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.pending_meta.insert(key.to_string(), bytes.to_vec());
        Ok(())
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        if let Some(pending) = self.pending_meta.get(key) {
            return Ok(Some(pending.clone()));
        }
        Ok(self.meta.get(key).cloned())
    }

    fn put_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.pending_index.insert(key.to_vec(), Some(()));
        Ok(())
    }

    fn delete_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.pending_index.insert(key.to_vec(), None);
        Ok(())
    }

    fn scan_index_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        let in_range = |key: &[u8]| key >= start && end.is_none_or(|end| key < end);
        let mut keys: alloc::collections::BTreeSet<Vec<u8>> = self
            .index
            .keys()
            .filter(|key| in_range(key))
            .cloned()
            .collect();
        // The open transaction's own writes shadow what was committed, exactly
        // as they do for rows.
        for (key, present) in &self.pending_index {
            if !in_range(key) {
                continue;
            }
            match present {
                Some(()) => {
                    keys.insert(key.clone());
                }
                None => {
                    keys.remove(key);
                }
            }
        }
        Ok(keys.into_iter().collect())
    }

    fn commit(&mut self) -> Result<()> {
        self.commits += 1;
        for (key, present) in core::mem::take(&mut self.pending_index) {
            match present {
                Some(()) => {
                    self.index.insert(key, ());
                }
                None => {
                    self.index.remove(&key);
                }
            }
        }
        for (table, rows) in core::mem::take(&mut self.pending_rows) {
            let committed = self.tables.entry(table).or_default();
            for (id, value) in rows {
                match value {
                    Some(bytes) => {
                        committed.insert(id, bytes);
                    }
                    None => {
                        committed.remove(&id);
                    }
                }
            }
        }
        for (table, rows) in core::mem::take(&mut self.pending_rows_keyed) {
            let committed = self.tables_keyed.entry(table).or_default();
            for (key, value) in rows {
                match value {
                    Some(bytes) => {
                        committed.insert(key, bytes);
                    }
                    None => {
                        committed.remove(&key);
                    }
                }
            }
        }
        for (key, value) in core::mem::take(&mut self.pending_meta) {
            self.meta.insert(key, value);
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.pending_rows.clear();
        self.pending_rows_keyed.clear();
        self.pending_meta.clear();
        self.pending_index.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_in_row_id_order() {
        let mut storage = MemStorage::new();
        for id in [3u64, 1, 2] {
            storage.put_row("docs", id, &[id as u8]).unwrap();
        }
        let ids: Vec<RowId> = crate::traits::scan_all(&storage, "docs")
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(ids, alloc::vec![1, 2, 3]);
    }

    /// A batch that skips over deleted rows must still come back full, because
    /// a short batch is how [`crate::traits::RowScan`] decides a scan is over.
    /// Interleaving deletes with live rows is the shape that breaks a naive
    /// "stop when the merge yields nothing" loop.
    #[test]
    fn a_batch_is_short_only_when_the_table_is_exhausted() {
        let mut storage = MemStorage::new();
        for id in 1..=20u64 {
            storage.put_row("docs", id, &[id as u8]).unwrap();
        }
        storage.commit().unwrap();
        for id in (1..=20u64).filter(|id| id % 2 == 0) {
            storage.delete_row("docs", id).unwrap();
        }
        let batch = storage.scan_batch("docs", None, 4).unwrap();
        assert_eq!(
            batch.iter().map(|row| row.0).collect::<Vec<_>>(),
            alloc::vec![1, 3, 5, 7]
        );
        let resumed = storage.scan_batch("docs", Some(7), 4).unwrap();
        assert_eq!(
            resumed.iter().map(|row| row.0).collect::<Vec<_>>(),
            alloc::vec![9, 11, 13, 15]
        );
        // And the whole thing still reads as one stream.
        let ids: Vec<RowId> = crate::traits::scan_all(&storage, "docs")
            .unwrap()
            .into_iter()
            .map(|row| row.0)
            .collect();
        assert_eq!(ids, (1..=20).filter(|id| id % 2 == 1).collect::<Vec<_>>());
    }

    /// An uncommitted write is visible to a scan, and lands in row-id order
    /// among the committed rows rather than after them.
    #[test]
    fn a_pending_write_is_merged_into_its_place_in_the_order() {
        let mut storage = MemStorage::new();
        for id in [1u64, 3, 5] {
            storage.put_row("docs", id, &[id as u8]).unwrap();
        }
        storage.commit().unwrap();
        storage.put_row("docs", 4, &[40]).unwrap();
        storage.put_row("docs", 3, &[30]).unwrap();
        let rows = crate::traits::scan_all(&storage, "docs").unwrap();
        assert_eq!(
            rows,
            alloc::vec![
                (1u64, RowBuf::Owned(alloc::vec![1u8])),
                (3, RowBuf::Owned(alloc::vec![30])),
                (4, RowBuf::Owned(alloc::vec![40])),
                (5, RowBuf::Owned(alloc::vec![5])),
            ]
        );
    }

    #[test]
    fn table_names_are_case_insensitive() {
        let mut storage = MemStorage::new();
        storage.put_row("Docs", 1, &[7]).unwrap();
        assert_eq!(
            storage.get_row("docs", 1).unwrap(),
            Some(RowBuf::Owned(alloc::vec![7]))
        );
    }

    #[test]
    fn a_rollback_discards_uncommitted_writes() {
        let mut storage = MemStorage::new();
        storage.put_row("docs", 1, b"committed").unwrap();
        storage.commit().unwrap();
        storage.put_row("docs", 2, b"uncommitted").unwrap();
        storage.rollback().unwrap();
        assert_eq!(
            storage.get_row("docs", 1).unwrap(),
            Some(RowBuf::Owned(b"committed".to_vec()))
        );
        assert_eq!(storage.get_row("docs", 2).unwrap(), None);
    }

    #[test]
    fn a_rollback_restores_a_deleted_row() {
        let mut storage = MemStorage::new();
        storage.put_row("docs", 1, b"here").unwrap();
        storage.commit().unwrap();
        storage.delete_row("docs", 1).unwrap();
        storage.rollback().unwrap();
        assert_eq!(
            storage.get_row("docs", 1).unwrap(),
            Some(RowBuf::Owned(b"here".to_vec()))
        );
    }
}
