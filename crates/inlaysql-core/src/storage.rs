//! Row storage over the in-house copy-on-write B+ tree.
//!
//! This is the bridge between the engine's [`Storage`](crate::Storage) trait
//! and the tree that actually holds the bytes. It lives in the core crate
//! because it needs nothing from the operating system: hand it any
//! [`Device`] — a file, an `io_uring` ring, a `Vec<u8>` in a browser tab, a
//! simulated disk — and it is a complete database.

use alloc::string::String;
use alloc::vec::Vec;

use crate::btree::{
    BackupSummary, CommitOutcome, CowBTree, Device, Durability, DEFAULT_PAGE_CACHE_BYTES,
    DEFAULT_PAGE_SIZE,
};
use crate::collation::{self, Collation};
use crate::error::{Error, Result};
use crate::row::RowBuf;
use crate::traits::{RowId, Storage};
use crate::value::Value;

/// Separates the table name from the row id in a row key. A NUL byte cannot
/// appear in a SQL identifier, so the prefix is unambiguous.
const KEY_SEPARATOR: u8 = 0;

/// The bytes a row key adds to the table name: the separator and the row id.
const KEY_SUFFIX_LEN: usize = 1 + 8;

/// How much of a row key [`RowKeyBuf`] holds without touching the heap.
///
/// Sized so every realistic table name is inline — 55 bytes of name — while
/// the buffer still fits comfortably in a stack frame and a cache line pair.
const INLINE_ROW_KEY: usize = 64;

/// The key a row is stored under.
///
/// Prefer [`RowKeyBuf`] on a hot path: this returns an owned `Vec` and so
/// allocates, which is exactly what a point lookup does not want to do.
pub fn row_key(table: &str, id: RowId) -> Vec<u8> {
    let mut key = Vec::with_capacity(table.len() + KEY_SUFFIX_LEN);
    // Lowercase byte by byte rather than through `str::to_ascii_lowercase`,
    // which would allocate a `String` only to copy out of it again. The result
    // is identical: `u8::to_ascii_lowercase` touches only `A..=Z`, so every
    // byte of a multi-byte UTF-8 sequence (all of which are `>= 0x80`) passes
    // through unchanged.
    key.extend(table.as_bytes().iter().map(u8::to_ascii_lowercase));
    key.push(KEY_SEPARATOR);
    // Big-endian so lexicographic key order is row-id order, which is what
    // makes `scan` return rows sorted without a separate sort.
    key.extend_from_slice(&id.to_be_bytes());
    key
}

/// The bytes a `WITHOUT ROWID` table's row is stored under, given its
/// primary key's values in declaration order.
///
/// Collation-aware the same way a scalar index's entry key is
/// ([`crate::index::encode_value`]): two primary keys that compare equal
/// under a `NOCASE` column land on the same storage slot, which is what
/// makes a duplicate one a constraint violation (`Error::Constraint` from
/// the ordinary "already occupied" check every `put_row_keyed` caller
/// already makes) rather than two rows silently sharing a value.
///
/// This is the row-id-shaped *suffix* only — table-name-prefixed by the
/// caller, the same division [`row_key`] draws between a table's prefix and
/// one row's own key.
pub fn primary_key_bytes(values: &[&Value], collations: &[Collation]) -> Result<Vec<u8>> {
    let mut key = Vec::new();
    for (position, value) in values.iter().enumerate() {
        crate::index::encode_value(&mut key, value, collation::at(collations, position))?;
    }
    Ok(key)
}

/// Stack scratch space a row key is assembled in, so a point lookup reaches
/// the tree without allocating.
///
/// [`row_key`] allocates twice — once to lowercase the table name, once for
/// the key itself — and a point read by primary key does that on every
/// execution, for a key that is a short name plus nine bytes. This builds the
/// same bytes in place and hands out a borrowed slice. A name too long for
/// [`INLINE_ROW_KEY`] spills to a `Vec` that the buffer keeps and reuses.
pub struct RowKeyBuf {
    inline: [u8; INLINE_ROW_KEY],
    /// Only used for a table name too long to build inline.
    spilled: Vec<u8>,
}

impl RowKeyBuf {
    /// An empty buffer. Allocates nothing.
    pub fn new() -> Self {
        Self {
            inline: [0u8; INLINE_ROW_KEY],
            spilled: Vec::new(),
        }
    }

    /// The key row `id` of `table` is stored under, borrowed from this buffer.
    ///
    /// Byte-for-byte what [`row_key`] returns.
    pub fn key(&mut self, table: &str, id: RowId) -> &[u8] {
        let name = table.as_bytes();
        let len = name.len() + KEY_SUFFIX_LEN;
        if len > INLINE_ROW_KEY {
            self.spilled.clear();
            self.spilled.reserve(len);
            self.spilled.extend(name.iter().map(u8::to_ascii_lowercase));
            self.spilled.push(KEY_SEPARATOR);
            self.spilled.extend_from_slice(&id.to_be_bytes());
            return &self.spilled;
        }
        for (slot, byte) in self.inline[..name.len()].iter_mut().zip(name) {
            *slot = byte.to_ascii_lowercase();
        }
        self.inline[name.len()] = KEY_SEPARATOR;
        self.inline[name.len() + 1..len].copy_from_slice(&id.to_be_bytes());
        &self.inline[..len]
    }
}

impl Default for RowKeyBuf {
    fn default() -> Self {
        Self::new()
    }
}

/// A metadata key, in a namespace no row key can collide with: it begins with
/// a NUL byte, and a row key always begins with a non-empty table name (SQL
/// identifiers cannot contain NUL).
pub fn meta_key(key: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + key.len());
    out.push(0);
    out.extend_from_slice(key.as_bytes());
    out
}

/// The prefix every row key of a table shares.
pub fn table_prefix(table: &str) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(table.len() + 1);
    prefix.extend(table.as_bytes().iter().map(u8::to_ascii_lowercase));
    prefix.push(KEY_SEPARATOR);
    prefix
}

/// Recover the row id from a key produced by [`row_key`].
pub fn row_id_from_key(key: &[u8]) -> Result<RowId> {
    let id = key
        .get(key.len().saturating_sub(8)..)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .ok_or_else(|| Error::Corrupt(String::from("row key is too short")))?;
    Ok(RowId::from_be_bytes(id))
}

/// [`Storage`] backed by the copy-on-write B+ tree and its write-ahead log.
///
/// Rows and metadata share one tree, and therefore one file. Writes are
/// buffered by the tree until [`Storage::commit`], which is exactly the tree's
/// single-sync commit; reads see the committed state, matching the engine's
/// statement-at-a-time usage.
pub struct TreeStorage<D: Device> {
    tree: CowBTree<D>,
}

impl<D: Device> TreeStorage<D> {
    /// Open (or create) a database on an arbitrary I/O backend.
    ///
    /// Whether the device already holds a database is decided from its header,
    /// not from a filesystem, so any [`Device`] works here — including one that
    /// has no notion of a file at all.
    pub fn open_on(device: D) -> Result<Self> {
        Self::open_on_with_cache(device, DEFAULT_PAGE_CACHE_BYTES)
    }

    /// Open (or create) a database whose page cache is bounded by
    /// `cache_bytes`, rather than by the default.
    ///
    /// `0` turns the cache off: every read goes to the device and every page is
    /// decoded again, which is what this engine did before the cache existed.
    /// The budget is resident memory per handle — see
    /// [`crate::btree::cache`] and
    /// [`EngineOptions::page_cache_bytes`](crate::EngineOptions).
    pub fn open_on_with_cache(device: D, cache_bytes: usize) -> Result<Self> {
        Self::open_on_with_options(device, cache_bytes, false, Durability::Full)
    }

    /// Open (or create) a database with an explicit cache budget, free-list
    /// choice and durability level, rather than every default.
    ///
    /// See [`EngineOptions::page_reuse`](crate::EngineOptions) before passing
    /// `true` for `page_reuse` — it is a real, load-bearing safety constraint
    /// on any file a reader might open read-only, not a tuning knob. See
    /// [`EngineOptions::durability`](crate::EngineOptions) before passing
    /// anything other than [`Durability::Full`] — the loss bound is real and
    /// the level is effectively per-file, not per-handle, on a shared device.
    pub fn open_on_with_options(
        device: D,
        cache_bytes: usize,
        page_reuse: bool,
        durability: Durability,
    ) -> Result<Self> {
        let mut tree = CowBTree::open_or_create_with_cache(device, DEFAULT_PAGE_SIZE, cache_bytes)?;
        tree.set_page_reuse(page_reuse);
        tree.set_durability(durability);
        Ok(Self { tree })
    }

    /// The underlying tree, for tests and tooling.
    pub fn tree(&self) -> &CowBTree<D> {
        &self.tree
    }

    /// The device the tree is writing to.
    pub fn device(&self) -> &D {
        self.tree.device()
    }
}

impl<D: Device> Storage for TreeStorage<D> {
    fn supports_quantized_vectors(&self) -> bool {
        self.tree.format_version() >= 4
    }

    fn put_row(&mut self, table: &str, id: RowId, bytes: &[u8]) -> Result<()> {
        let mut key = RowKeyBuf::new();
        self.tree.put(key.key(table, id), bytes)
    }

    /// The point read. The key is built on the stack — see [`RowKeyBuf`] — so
    /// the lookup reaches the tree without a heap allocation, and a cache hit
    /// no longer copies the row bytes either (`AHL-478`) — see
    /// [`RowBuf`](crate::row::RowBuf).
    fn get_row(&self, table: &str, id: RowId) -> Result<Option<RowBuf>> {
        let mut key = RowKeyBuf::new();
        self.tree.get(key.key(table, id))
    }

    fn delete_row(&mut self, table: &str, id: RowId) -> Result<()> {
        let mut key = RowKeyBuf::new();
        self.tree.delete(key.key(table, id))
    }

    /// One batch of a table's rows, in row-id order.
    ///
    /// The walk is bounded by the table's key prefix rather than filtered after
    /// the fact: rows of every table share one tree, so an unbounded walk would
    /// decode and materialise the whole database to answer a scan of one table.
    /// `after` bounds it from below as well, which is what lets the layer above
    /// resume where it stopped instead of re-reading what it has already seen.
    fn scan_batch(
        &self,
        table: &str,
        after: Option<RowId>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        let mut resume = RowKeyBuf::new();
        let resume = after.map(|id| resume.key(table, id));
        // The raw-leaf row-id-and-value walk: a table scan reads the row id out
        // of the key and throws the rest away, so it parses leaf cells in place
        // — no `Rc<Node>`, no per-cell key `Vec` — rather than decoding the
        // page only to discard it (the same fast path `scan_index_row_ids`
        // gives the index probe).
        self.tree
            .scan_prefix_row_values_raw_from(&table_prefix(table), resume, limit)
    }

    fn put_row_keyed(&mut self, table: &str, key: &[u8], bytes: &[u8]) -> Result<()> {
        let mut full = table_prefix(table);
        full.extend_from_slice(key);
        self.tree.put(&full, bytes)
    }

    fn get_row_keyed(&self, table: &str, key: &[u8]) -> Result<Option<RowBuf>> {
        let mut full = table_prefix(table);
        full.extend_from_slice(key);
        self.tree.get(&full)
    }

    fn delete_row_keyed(&mut self, table: &str, key: &[u8]) -> Result<()> {
        let mut full = table_prefix(table);
        full.extend_from_slice(key);
        self.tree.delete(&full)
    }

    /// Reuses [`CowBTree::scan_prefix_from`] — the same one-traversal
    /// primitive a scalar index's range probe and the batched streaming row
    /// scan both already go through — rather than a bespoke walk, so this
    /// is proven code answering a new question, not new code to prove.
    fn scan_batch_keyed(
        &self,
        table: &str,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        let prefix = table_prefix(table);
        let full_after = after.map(|suffix| {
            let mut key = prefix.clone();
            key.extend_from_slice(suffix);
            key
        });
        let rows = self
            .tree
            .scan_prefix_from(&prefix, full_after.as_deref(), limit)?;
        Ok(rows
            .into_iter()
            .map(|(key, value)| (key[prefix.len()..].to_vec(), value))
            .collect())
    }

    fn put_meta(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.tree.put(&meta_key(key), bytes)
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        // Metadata reads are once-per-refresh, not per row — the whole point
        // of `RowBuf` is the per-row read path, so this takes the plain,
        // always-owned answer rather than threading sharing through the
        // catalog too.
        Ok(self.tree.get(&meta_key(key))?.map(RowBuf::into_vec))
    }

    /// An index entry is a row of the same tree with an empty value.
    ///
    /// That is the whole of decision D3: it joins whatever transaction is
    /// open, reaches the log with the row it describes, is rebased by the same
    /// MVCC rules, and is replayed by the same recovery. There is no second
    /// structure to keep in step, so there is no way for one to be ahead of
    /// the other after a crash.
    fn put_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.tree.put(key, &[])
    }

    fn delete_index_entry(&mut self, key: &[u8]) -> Result<()> {
        self.tree.delete(key)
    }

    fn scan_index_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .tree
            .scan_range(start, end)?
            .into_iter()
            .map(|(key, _)| key)
            .collect())
    }

    /// The row-id-only fast path (`AHL-479`): [`CowBTree::scan_range_row_ids_from`]
    /// reads the eight-byte row id straight out of each admitted entry instead
    /// of cloning the whole key and resolving its (always empty) value first,
    /// which is what [`Storage::scan_index_range`] above has to do because it
    /// promises the caller the real key bytes. See that method's doc comment
    /// for the property this relies on and the test that pins it against
    /// [`Storage::scan_index_range`] plus the ordinary decode.
    fn scan_index_row_ids(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<RowId>> {
        self.tree
            .scan_range_row_ids_from(start, end, None, usize::MAX)
    }

    /// Commit, turning a lost race into an error rather than a lie.
    ///
    /// The tree reports first-committer-wins by returning
    /// [`CommitOutcome::Conflict`], which has no room in the `Storage`
    /// contract's `Result<()>` — so this used to be discarded, and a writer
    /// whose transaction had just been thrown away was told it committed.
    /// [`Error::Conflict`] is that outcome given a name: nothing was written,
    /// and the caller can retry. `crates/inlaysql-bench/src/concurrency.rs`
    /// measures how often it happens.
    fn commit(&mut self) -> Result<()> {
        match self.tree.commit()? {
            CommitOutcome::Committed => Ok(()),
            CommitOutcome::Conflict => Err(Error::Conflict),
        }
    }

    fn rollback(&mut self) -> Result<()> {
        self.tree.rollback();
        Ok(())
    }

    /// Adopt whatever another handle on this file has committed since the last
    /// look. See [`CowBTree::refresh`] for how "nothing was committed" is
    /// answered without reading the device, and for why the log records the
    /// state block is behind are not replayed here.
    fn refresh(&mut self) -> Result<bool> {
        self.tree.refresh()
    }

    /// True once the open transaction has grown past half the log region —
    /// counting what committing it will still add.
    ///
    /// The ceiling is hard: a record larger than the region cannot be written,
    /// and the commit fails. Half of it is the margin — the caller checks
    /// between writes, and the write that follows a `true` answer can still
    /// copy a whole root-to-leaf path into the transaction before the commit
    /// happens.
    ///
    /// The size asked about is [`CowBTree::projected_record_len`] and not
    /// `pending_record_len`, which is the exact size of the record *as it
    /// stands*: with `page_reuse` on, the commit writes free-list rows of its
    /// own before it builds that record, so the exact answer to the wrong
    /// question let a batch run past the ceiling and strand itself. See that
    /// method — the difference between the two is the whole of this
    /// contract's "warn before the transaction becomes uncommittable".
    fn transaction_is_nearly_full(&self) -> bool {
        self.tree.projected_record_len() * 2 >= self.tree.log_capacity()
    }

    /// The one backend that can answer this: a committed root already *is* an
    /// immutable snapshot, so a backup is a page copy rather than a rebuild.
    /// See [`CowBTree::backup_to`] and [`crate::btree::backup`].
    fn backup_to(&self, dest: &mut dyn Device) -> Result<BackupSummary> {
        self.tree.backup_to(dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    /// The whole point of [`RowKeyBuf`] is that it changes nothing: a row
    /// written before it existed has to be found by a key built with it. Every
    /// case that reaches a different branch is checked against [`row_key`],
    /// which is the definition of the on-disk key.
    #[test]
    fn the_stack_built_key_is_byte_identical_to_the_allocating_one() {
        let long = "t".repeat(INLINE_ROW_KEY);
        let names = [
            "kv",
            "KV",
            "MixedCase",
            "a",
            // Exactly at, either side of, and far past the inline boundary.
            &"n".repeat(INLINE_ROW_KEY - KEY_SUFFIX_LEN - 1),
            &"n".repeat(INLINE_ROW_KEY - KEY_SUFFIX_LEN),
            &"n".repeat(INLINE_ROW_KEY - KEY_SUFFIX_LEN + 1),
            &long,
            // Non-ASCII must pass through untouched, as `to_ascii_lowercase`
            // leaves it.
            "TÄBLE",
        ];
        let mut buf = RowKeyBuf::new();
        for name in names {
            for id in [0, 1, 42, RowId::MAX] {
                assert_eq!(
                    buf.key(name, id),
                    row_key(name, id).as_slice(),
                    "name {name:?}, id {id}"
                );
            }
        }
    }

    /// One buffer serves many lookups, including a spill followed by an inline
    /// key — the case where a stale `spilled` or stale inline bytes would leak
    /// into the next key.
    #[test]
    fn reusing_one_buffer_never_leaks_the_previous_key() {
        let long = "l".repeat(INLINE_ROW_KEY * 2);
        let mut buf = RowKeyBuf::new();
        for _ in 0..3 {
            assert_eq!(buf.key(&long, 7), row_key(&long, 7).as_slice());
            assert_eq!(buf.key("kv", 7), row_key("kv", 7).as_slice());
            assert_eq!(buf.key("a", 9), row_key("a", 9).as_slice());
        }
    }

    #[test]
    fn a_row_key_still_round_trips_through_its_row_id() {
        let mut buf = RowKeyBuf::new();
        for id in [0, 1, 42, RowId::MAX] {
            assert_eq!(row_id_from_key(buf.key("kv", id)).unwrap(), id);
        }
    }

    /// The claim decision D3 rests on: an index entry's key can never be read
    /// as a row, as engine metadata, or as an ANN graph node, and none of
    /// those can be read as an index entry.
    ///
    /// The three namespaces are separated by their first byte and, for the two
    /// that share `\x01`, by the tag that follows it. This walks the boundary
    /// cases rather than asserting the rule abstractly: a table whose name is
    /// literally `idx:`, a metadata key that spells the same thing, a table
    /// name that starts where the prefix ends.
    #[test]
    fn an_index_entry_key_cannot_collide_with_a_row_or_metadata_key() {
        use crate::index::{entry_key, index_prefix};
        use crate::value::Value;

        let entries = [
            entry_key("i", &[&Value::Null], &[], 0).unwrap(),
            entry_key("idx", &[&Value::Integer(0)], &[], 1).unwrap(),
            entry_key("ann:t.c", &[&Value::Text("x".to_string().into())], &[], 2).unwrap(),
            entry_key("", &[&Value::Blob(alloc::vec![0, 1, 2])], &[], RowId::MAX).unwrap(),
        ];
        // Every index key begins with the one byte no identifier and no
        // metadata key can produce.
        for entry in &entries {
            assert_eq!(entry[0], 1, "{entry:?}");
        }

        let others = [
            row_key("idx:i", 0),
            row_key("kv", 7),
            row_key("\u{1}ann:t.c", 3),
            meta_key("catalog"),
            meta_key("idx:i"),
            meta_key("index:t:c"),
            table_prefix("idx:i"),
            // The paged ANN index's namespace, as `engine::vector_index_namespace`
            // spells it. It shares the `\x01` byte and is told apart by its tag.
            row_key("\u{1}ann:docs.embedding", 5),
        ];
        for other in &others {
            for entry in &entries {
                assert!(
                    !entry.starts_with(other) && !other.starts_with(entry),
                    "index key {entry:?} and non-index key {other:?} share a prefix"
                );
            }
        }

        // Two indexes never read each other's entries, however their names
        // nest, because the prefix ends in a NUL an index name cannot contain.
        assert!(!index_prefix("orders_total").starts_with(&index_prefix("orders")));
    }

    /// `row_key` no longer builds a `String` first; the lowercasing has to be
    /// the same all the same, and the prefix has to stay the key's prefix.
    #[test]
    fn a_row_key_starts_with_its_table_prefix() {
        for name in ["kv", "KV", "MixedCase", "TÄBLE"] {
            let prefix = table_prefix(name);
            assert!(row_key(name, 3).starts_with(&prefix));
            assert_eq!(prefix, table_prefix(&name.to_ascii_lowercase()));
            assert_eq!(
                prefix,
                {
                    let mut expected = name.to_ascii_lowercase().into_bytes();
                    expected.push(KEY_SEPARATOR);
                    expected
                },
                "prefix for {name:?}"
            );
        }
        // Two tables whose names differ only in case are one table.
        assert_eq!(row_key("KV", 1), row_key("kv", 1));
        assert_eq!("TÄBLE".to_string().to_ascii_lowercase(), "tÄble");
    }
}
