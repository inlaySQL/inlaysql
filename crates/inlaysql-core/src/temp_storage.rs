//! Storage for `CREATE TEMPORARY TABLE`: one backend wrapping another.
//!
//! A temporary table is an ordinary, row-id-keyed table in every way except
//! where it lives — it is never durable and never visible to another
//! [`crate::engine::Engine`] on the same file. [`TempTableRouter`] is what
//! makes that true underneath [`crate::traits::Storage`]: it wraps whichever
//! durable backend [`crate::engine::Engine::open`] was given and routes each
//! table-keyed call to an internal [`MemStorage`] instead, by table name,
//! whenever [`Engine`](crate::engine::Engine) has told it that table is
//! temporary ([`Storage::set_temp_table`]). Every other call — metadata, the
//! scalar-index methods, backup — always reaches the durable backend; see
//! each method below for why.

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use crate::btree::{BackupSummary, Device};
use crate::error::Result;
use crate::mem::MemStorage;
use crate::row::RowBuf;
use crate::traits::{RowId, Storage};

/// Wraps a durable [`Storage`] and gives every table marked temporary
/// ([`TempTableRouter::set_temp_table`]) its rows in an in-memory backend
/// instead.
pub struct TempTableRouter {
    durable: Box<dyn Storage>,
    temp: MemStorage,
    /// Lowercased names of the tables currently routed to `temp`, kept in
    /// lockstep with [`crate::catalog::Catalog::temp_tables`] by the engine.
    temp_tables: BTreeSet<String>,
}

impl TempTableRouter {
    /// Wrap `durable`, with no temporary tables yet.
    pub fn new(durable: Box<dyn Storage>) -> Self {
        Self {
            durable,
            temp: MemStorage::new(),
            temp_tables: BTreeSet::new(),
        }
    }

    /// Whether `table` is currently routed to the in-memory side.
    ///
    /// Short-circuits on the empty set without lowercasing anything: the
    /// overwhelmingly common case is a database that has never created a
    /// temporary table at all, and this sits behind every row read and
    /// write, the same hot path [`crate::catalog::Catalog::table`]'s own
    /// doc comment measures — a `to_ascii_lowercase` allocation on every
    /// point read for a feature that database never uses would be exactly
    /// the cost that comment exists to avoid.
    fn is_temp(&self, table: &str) -> bool {
        if self.temp_tables.is_empty() {
            return false;
        }
        if table.bytes().any(|byte| byte.is_ascii_uppercase()) {
            self.temp_tables.contains(&table.to_ascii_lowercase())
        } else {
            self.temp_tables.contains(table)
        }
    }
}

impl Storage for TempTableRouter {
    fn supports_quantized_vectors(&self) -> bool {
        self.durable.supports_quantized_vectors()
    }

    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        if self.is_temp(table) {
            self.temp.put_row(table, id, bytes)
        } else {
            self.durable.put_row(table, id, bytes)
        }
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        if self.is_temp(table) {
            self.temp.get_row(table, id)
        } else {
            self.durable.get_row(table, id)
        }
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        if self.is_temp(table) {
            self.temp.delete_row(table, id)
        } else {
            self.durable.delete_row(table, id)
        }
    }

    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        if self.is_temp(table) {
            self.temp.scan_batch(table, after, limit)
        } else {
            self.durable.scan_batch(table, after, limit)
        }
    }

    // A `WITHOUT ROWID` temporary table is a real combination — nothing about
    // the two features conflicts — so the keyed methods route exactly like
    // the row-id ones above, by the same table name and the same flag.

    fn put_row_keyed(&mut self, table: &str, key: &[u8], bytes: &[u8]) -> Result<()> {
        if self.is_temp(table) {
            self.temp.put_row_keyed(table, key, bytes)
        } else {
            self.durable.put_row_keyed(table, key, bytes)
        }
    }

    fn get_row_keyed(&self, table: &str, key: &[u8]) -> Result<Option<RowBuf>> {
        if self.is_temp(table) {
            self.temp.get_row_keyed(table, key)
        } else {
            self.durable.get_row_keyed(table, key)
        }
    }

    fn delete_row_keyed(&mut self, table: &str, key: &[u8]) -> Result<()> {
        if self.is_temp(table) {
            self.temp.delete_row_keyed(table, key)
        } else {
            self.durable.delete_row_keyed(table, key)
        }
    }

    fn scan_batch_keyed(
        &self,
        table: &str,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        if self.is_temp(table) {
            self.temp.scan_batch_keyed(table, after, limit)
        } else {
            self.durable.scan_batch_keyed(table, after, limit)
        }
    }

    // Metadata — the catalog, the row-id counter, CDC records — is never
    // table-keyed and always durable: a temporary table has no metadata of
    // its own, and letting a statement that only touched one skip a durable
    // write is exactly the point.
    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.durable.put_meta(key, bytes)
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.durable.get_meta(key)
    }

    // A scalar index entry's key carries the *index's* name, not the table's
    // (`crate::index::entry_key`) — there is no table name here to route by,
    // which is exactly why `CREATE INDEX` on a temporary table is refused at
    // plan time (`sql::plan_create_index`). A temporary table therefore never
    // generates a call here, and these three always mean the durable schema.
    fn put_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.durable.put_index_entry(key)
    }

    fn delete_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.durable.delete_index_entry(key)
    }

    fn scan_index_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        self.durable.scan_index_range(start, end)
    }

    fn scan_index_row_ids(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<RowId>> {
        self.durable.scan_index_row_ids(start, end)
    }

    /// Durable first, temporary only if that succeeds.
    ///
    /// A failed durable commit — `Err(Error::Conflict)` in particular —
    /// means the durable backend already discarded its own buffered writes
    /// on the way to returning the error (see
    /// [`Engine::commit_storage`](crate::engine::Engine)'s doc). Committing
    /// the temporary side unconditionally would half-promote a statement
    /// that, from the caller's point of view, never happened: the durable
    /// half rolled back, the temporary half did not. Rolling the temporary
    /// side back to match on that path is what keeps one statement's write
    /// to both an ordinary and a temporary table one atomic unit, whichever
    /// half a fault lands in.
    fn commit(&mut self) -> Result<()> {
        match self.durable.commit() {
            Ok(()) => self.temp.commit(),
            Err(error) => {
                let _ = self.temp.rollback();
                Err(error)
            }
        }
    }

    fn rollback(&mut self) -> Result<()> {
        let temp_result = self.temp.rollback();
        self.durable.rollback()?;
        temp_result
    }

    fn refresh(&mut self) -> Result<bool> {
        // Only the durable side has other handles to catch up to — a
        // temporary table belongs to this handle alone, the same as
        // sqlite3's `TEMP` schema belongs to one connection alone.
        self.durable.refresh()
    }

    fn transaction_is_nearly_full(&self) -> bool {
        self.durable.transaction_is_nearly_full()
    }

    fn backup_to(&self, dest: &mut dyn Device) -> Result<BackupSummary> {
        // A temporary table is excluded from a backup by definition —
        // confirmed against sqlite3: a `.backup`'d file's schema has no
        // trace of the source connection's temporary tables — so this is
        // exactly `Storage::backup_to` on the durable side, nothing to
        // compose.
        self.durable.backup_to(dest)
    }

    fn set_temp_table(&mut self, table: &str, temporary: bool) {
        let key = table.to_ascii_lowercase();
        if temporary {
            self.temp_tables.insert(key);
        } else {
            self.temp_tables.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> TempTableRouter {
        TempTableRouter::new(Box::new(MemStorage::new()))
    }

    #[test]
    fn a_temp_table_s_rows_are_invisible_to_the_durable_backend() {
        let mut router = router();
        router.set_temp_table("t", true);
        router.put_row("t", 1, b"temp row").unwrap();
        assert_eq!(
            router.get_row("t", 1).unwrap().as_deref(),
            Some(&b"temp row"[..])
        );
        // The same name, not marked temporary, is a different place.
        router.set_temp_table("t", false);
        assert_eq!(router.get_row("t", 1).unwrap(), None);
    }

    #[test]
    fn rolling_back_undoes_both_sides_together() {
        let mut router = router();
        router.set_temp_table("temp_t", true);
        router.put_row("durable_t", 1, b"durable").unwrap();
        router.put_row("temp_t", 1, b"temp").unwrap();
        router.rollback().unwrap();
        assert_eq!(router.get_row("durable_t", 1).unwrap(), None);
        assert_eq!(router.get_row("temp_t", 1).unwrap(), None);
    }

    #[test]
    fn committing_makes_both_sides_durable_together() {
        let mut router = router();
        router.set_temp_table("temp_t", true);
        router.put_row("durable_t", 1, b"durable").unwrap();
        router.put_row("temp_t", 1, b"temp").unwrap();
        router.commit().unwrap();
        // A rollback after commit touches nothing: both are already
        // committed, exactly the guarantee `Storage::commit` promises.
        router.rollback().unwrap();
        assert_eq!(
            router.get_row("durable_t", 1).unwrap().as_deref(),
            Some(&b"durable"[..])
        );
        assert_eq!(
            router.get_row("temp_t", 1).unwrap().as_deref(),
            Some(&b"temp"[..])
        );
    }
}
