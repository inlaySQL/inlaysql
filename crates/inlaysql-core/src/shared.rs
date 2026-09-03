//! One storage backend, held by more than one owner.
//!
//! The engine owns the row store. An index that keeps its own structure *in
//! that same store* — [`crate::hnsw_paged::PagedHnswIndex`] — has to write
//! through the same handle, or its writes land in a different transaction from
//! the rows they describe and a crash between the two leaves the index and the
//! table disagreeing. That is the failure this project exists to remove, so it
//! is not acceptable inside the engine either.
//!
//! [`SharedStorage`] is the handle both of them hold. It is an `Rc<RefCell<_>>`
//! and deliberately not an `Arc<Mutex<_>>`: requiring `Send` here would push
//! that bound through every trait in the core, which is what breaks the
//! simulation harness (it shares a fault-injecting disk as `Rc<RefCell<_>>`)
//! and the single-threaded WASM build. A database is owned by one thread — the
//! async API gives it a dedicated I/O thread for exactly that reason — so a
//! reference count is the right tool.
//!
//! # Why the borrows cannot overlap
//!
//! Every [`Storage`] method takes what it needs, does its work, and returns an
//! owned value. None of them hands out a reference into the backend, so a
//! borrow never outlives a single call and two holders of a `SharedStorage` can
//! never be inside it at once. [`crate::row::RowBuf`] (`AHL-478`) keeps this
//! true while still letting a row read share bytes with the page cache: it
//! carries no lifetime and can be held, moved or dropped exactly like a
//! `Vec<u8>` could, the sharing is an implementation detail behind an `Rc`,
//! not a borrow the type system tracks.
//!
//! **This is why the streaming scan is batched.** A cursor that borrowed into
//! the tree would have to hold a [`RefCell`] guard for as long as the query
//! ran, and any write through another handle in that window — the paged ANN
//! index writing through the engine's transaction is exactly that — would
//! panic rather than fail to compile. [`crate::traits::Storage::scan_batch`]
//! returns an owned run of rows and a resume token instead, so
//! [`crate::traits::RowScan`] can stream over a `SharedStorage` while holding
//! nothing but an ordinary `&`.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::btree::{BackupSummary, Device};
use crate::error::Result;
use crate::row::RowBuf;
use crate::traits::{RowId, Storage};

/// A [`Storage`] backend shared by the engine and the indexes that persist
/// themselves inside it.
///
/// Cloning is a reference count, not a copy: every clone drives the same
/// backend, and therefore the same open transaction.
pub struct SharedStorage {
    inner: Rc<RefCell<Box<dyn Storage>>>,
}

impl SharedStorage {
    /// Take ownership of `storage` and hand back a shareable handle to it.
    pub fn new(storage: Box<dyn Storage>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(storage)),
        }
    }

    /// How many handles exist. Diagnostic, and how a test proves an index
    /// really is writing through the engine's transaction rather than its own.
    pub fn handles(&self) -> usize {
        Rc::strong_count(&self.inner)
    }
}

impl Clone for SharedStorage {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl Storage for SharedStorage {
    fn supports_quantized_vectors(&self) -> bool {
        self.inner.borrow().supports_quantized_vectors()
    }

    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        self.inner.borrow_mut().put_row(table, id, bytes)
    }

    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        self.inner.borrow().get_row(table, id)
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        self.inner.borrow_mut().delete_row(table, id)
    }

    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        self.inner.borrow().scan_batch(table, after, limit)
    }

    fn scan_batch_with(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
        row: &mut dyn FnMut(RowId, &[u8]) -> Result<()>,
    ) -> Result<(usize, Option<RowId>)> {
        self.inner
            .borrow()
            .scan_batch_with(table, after, limit, row)
    }

    /// Delegated explicitly for the same reason [`Storage::scan_index_row_ids`]
    /// is below: the trait's default reaches the last row through *this*
    /// type's own [`Storage::scan_batch`], which forwards correctly, but
    /// [`Storage::last_in_table`]'s default is a full scan — the inner
    /// backend's one-descent override (`TreeStorage`'s, through the tree's
    /// `last_in_prefix`) is unreachable unless a wrapper that forwards
    /// everything else forwards this too.
    fn first_in_table(&self, table: &str) -> Result<Option<(RowId, RowBuf)>> {
        self.inner.borrow().first_in_table(table)
    }

    fn last_in_table(&self, table: &str) -> Result<Option<(RowId, RowBuf)>> {
        self.inner.borrow().last_in_table(table)
    }

    /// Forwarded by name for the same reason `first_in_table` is: the
    /// default counts through a scan, and the inner backend's leaf-count
    /// override (`TreeStorage`'s) is unreachable otherwise.
    fn count_rows(&self, table: &str) -> Result<u64> {
        self.inner.borrow().count_rows(table)
    }

    fn put_row_keyed(&mut self, table: &str, key: &[u8], bytes: &[u8]) -> Result<()> {
        self.inner.borrow_mut().put_row_keyed(table, key, bytes)
    }

    fn get_row_keyed(&self, table: &str, key: &[u8]) -> Result<Option<RowBuf>> {
        self.inner.borrow().get_row_keyed(table, key)
    }

    fn delete_row_keyed(&mut self, table: &str, key: &[u8]) -> Result<()> {
        self.inner.borrow_mut().delete_row_keyed(table, key)
    }

    fn scan_batch_keyed(
        &self,
        table: &str,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        self.inner.borrow().scan_batch_keyed(table, after, limit)
    }

    fn set_temp_table(&mut self, table: &str, temporary: bool) {
        self.inner.borrow_mut().set_temp_table(table, temporary)
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.inner.borrow_mut().put_meta(key, bytes)
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.borrow().get_meta(key)
    }

    fn put_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.inner.borrow_mut().put_index_entry(key)
    }

    fn delete_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.inner.borrow_mut().delete_index_entry(key)
    }

    fn scan_index_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        self.inner.borrow().scan_index_range(start, end)
    }

    /// Delegated explicitly, like every other method here, rather than left to
    /// the trait's default: the default reaches the row ids through *this*
    /// type's own `scan_index_range`, which would forward to the inner
    /// backend's general walk and throw away the point of overriding it there
    /// (`AHL-479`) — a wrapper that forwards everything else has to forward
    /// this too, or the fast path it wraps stops being reachable through it.
    fn scan_index_row_ids(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<RowId>> {
        self.inner.borrow().scan_index_row_ids(start, end)
    }

    /// Delegated for the same reason as [`Storage::scan_index_row_ids`]
    /// above: `TreeStorage`'s one-descent override is unreachable through the
    /// default otherwise.
    fn first_index_entry(&self, start: &[u8], end: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
        self.inner.borrow().first_index_entry(start, end)
    }

    fn last_index_entry(&self, start: &[u8], end: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
        self.inner.borrow().last_index_entry(start, end)
    }

    fn commit(&mut self) -> Result<()> {
        self.inner.borrow_mut().commit()
    }

    fn rollback(&mut self) -> Result<()> {
        self.inner.borrow_mut().rollback()
    }

    fn refresh(&mut self) -> Result<bool> {
        self.inner.borrow_mut().refresh()
    }

    fn transaction_is_nearly_full(&self) -> bool {
        self.inner.borrow().transaction_is_nearly_full()
    }

    /// The `RefCell` borrow is what keeps the copied snapshot pinned across
    /// every holder of this handle, not only the caller: the paged ANN index
    /// writes through its own clone of this `SharedStorage`, and a shared
    /// borrow held for the whole copy is exactly what stops it (or anything
    /// else) taking the `borrow_mut` a commit would need part-way through.
    fn backup_to(&self, dest: &mut dyn Device) -> Result<BackupSummary> {
        self.inner.borrow().backup_to(dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::MemStorage;

    /// A backend whose [`Storage::count_rows`] override is distinguishable
    /// from the trait's default: it holds no rows, so the default would
    /// count zero, and it answers a sentinel instead.
    struct CountsFortyTwo;

    impl Storage for CountsFortyTwo {
        fn put_row(&mut self, _: &str, _: RowId, _: &[u8]) -> Result<()> {
            Ok(())
        }
        fn get_row(&self, _: &str, _: RowId) -> Result<Option<RowBuf>> {
            Ok(None)
        }
        fn delete_row(&mut self, _: &str, _: RowId) -> Result<()> {
            Ok(())
        }
        fn scan_batch(&self, _: &str, _: Option<RowId>, _: usize) -> Result<Vec<(RowId, RowBuf)>> {
            Ok(Vec::new())
        }
        fn count_rows(&self, _: &str) -> Result<u64> {
            Ok(42)
        }
        fn put_meta(&mut self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }
        fn get_meta(&self, _: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn commit(&mut self) -> Result<()> {
            Ok(())
        }
        fn rollback(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// The wrapper reaches the backend's own `count_rows`, not the trait's
    /// default over the wrapper's `scan_batch_with` — the trap every
    /// forwarded-by-name method here exists to avoid.
    #[test]
    fn count_rows_reaches_the_backend_s_override() {
        let shared = SharedStorage::new(Box::new(CountsFortyTwo));
        assert_eq!(shared.count_rows("t").unwrap(), 42);
    }

    #[test]
    fn every_clone_drives_the_same_backend_and_the_same_transaction() {
        let mut engine_side = SharedStorage::new(Box::new(MemStorage::new()));
        let mut index_side = engine_side.clone();
        assert_eq!(engine_side.handles(), 2);

        engine_side.put_row("docs", 1, b"row").unwrap();
        index_side.put_row("\u{1}ann:docs.v", 0, b"node").unwrap();

        // Each sees the other's buffered write, because there is only one
        // transaction to see.
        assert_eq!(
            index_side.get_row("docs", 1).unwrap().as_deref(),
            Some(&b"row"[..])
        );
        assert_eq!(
            engine_side
                .get_row("\u{1}ann:docs.v", 0)
                .unwrap()
                .as_deref(),
            Some(&b"node"[..])
        );

        // And one rollback takes both back.
        engine_side.rollback().unwrap();
        assert_eq!(index_side.get_row("docs", 1).unwrap(), None);
        assert_eq!(engine_side.get_row("\u{1}ann:docs.v", 0).unwrap(), None);
    }
}
