//! A copy-on-write B+ tree over a [`Device`], with write-ahead logging and
//! multi-version concurrency control.
//!
//! The tree is the durable core of the storage engine. Its defining property
//! is that **a page is never modified in place**: every change copies the path
//! from the root to the touched leaf into freshly allocated pages and swaps in
//! a new root pointer. Old pages stay untouched, so any reader that pinned an
//! earlier root sees a consistent snapshot for as long as it holds it — that is
//! the MVCC read side, and it falls out of the copy-on-write discipline for
//! free.
//!
//! The write side is optimistic: several writers on one database can each
//! buffer a transaction against the committed state, and on commit the tree
//! re-reads the committed root. A stale transaction touching disjoint keys is
//! rebased on the winner; a key changed by both transactions aborts with
//! [`CommitOutcome::Conflict`] — first-committer-wins at the overlapping write.
//!
//! What the tree cannot survive on its own is a *torn* write to the metadata
//! that names the current root. That is the write-ahead log's job: each commit
//! appends a self-contained record — sequence/predecessor, new root,
//! next-free-page, and a copy of every page it wrote — to one of several WAL
//! regions and syncs *that*, the commit point, leaving the state block to be
//! rewritten lazily at checkpoint. On open, [`CowBTree::open`] orders and
//! replays every accepted cross-region record newer than the state block. See
//! [`crate::wal`] for the full protocol and
//! `docs/recovery.md` for the prose version.
//!
//! Writes are buffered in an in-memory transaction and only reach the device
//! on [`CowBTree::commit`], matching the existing `Storage` contract. Reads
//! always see the last committed state.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::error::{Error, Result};
use crate::row::RowBuf;
use crate::traits::RowId;

use super::cache::{self, PageCache, DEFAULT_PAGE_CACHE_BYTES};
use super::device::{CommitPoint, Device};
use super::page::{self, Entry, Key, Node, PageId, Separator, ValueRef};

/// The magic bytes at the front of the header.
const MAGIC: &[u8; 8] = b"INLAYSQL";
/// Current on-disk format version.
///
/// 1 — the walking skeleton (redb-backed).
/// 2 — the copy-on-write B-tree with a write-ahead log.
/// 3 — overflow pages: a leaf cell may point at a chain of overflow pages that
///     hold a value larger than one page, rather than storing it inline.
/// 4 — opt-in int8 vector row/catalog/index payloads.
/// 5 — four per-writer WAL regions with an explicitly ordered recovery chain.
///
/// The version is checked on open and a mismatch is reported as
/// [`Error::FormatVersion`], never corruption; see `docs/recovery.md`.
pub const FORMAT_VERSION: u32 = 5;
/// Version 3 exact-vector files remain readable. They cannot acquire a
/// v4-only column type because the storage capability is exposed to the SQL
/// engine and checked at `CREATE TABLE`.
const MIN_READABLE_FORMAT_VERSION: u32 = 3;

// ------------------------------------------------------------- header (block 0)

/// The header is written once, at create, and never overwritten. It is the one
/// structure a torn write cannot be recovered from, because it names the page
/// size everything else is addressed in.
const HEADER_LEN: usize = 24;
const H_PAGE_SIZE: usize = 8;
const H_VERSION: usize = 12;
const H_CHECKSUM: usize = 16;

// -------------------------------------------------------------- state (block 1)

/// The state block records the tree's committed root and next-free-page plus
/// the highest log sequence number that has been checkpointed. It is rewritten
/// at each checkpoint and is recoverable from the log if torn.
const STATE_LEN: usize = 32;
const S_ROOT: usize = 0;
const S_NEXT: usize = 8;
const S_SEQ: usize = 16;
const S_CHECKSUM: usize = 24;

/// The result of inserting into a subtree: either the subtree was replaced by
/// one new page, or it split into two new pages around a separator.
enum InsertOutcome {
    Replaced {
        id: PageId,
    },
    Split {
        left: PageId,
        right: PageId,
        separator: Vec<u8>,
    },
}

/// What happened to a transaction on [`CowBTree::commit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The transaction was written to the log and is durable.
    Committed,
    /// Another writer committed an overlapping key first, so this transaction
    /// was aborted (first-committer-wins). The tree reloads the winner's state,
    /// so the next transaction starts from it.
    Conflict,
}

/// A copy-on-write B+ tree backed by a device.
pub struct CowBTree<D: Device> {
    device: D,
    page_size: usize,
    format_version: u32,
    /// The committed root page id (0 = empty tree).
    root: PageId,
    /// The next free page id at the last commit.
    next_page_id: PageId,
    /// The sequence number the next commit will write to the log.
    next_seq: u64,
    /// The highest sequence number persisted in the state block.
    checkpoint_seq: u64,
    /// Newly allocated pages of the open transaction, keyed by page id.
    dirty: BTreeMap<PageId, Vec<u8>>,
    /// The working root of the open transaction.
    pending_root: PageId,
    /// The working next-free-page counter of the open transaction.
    pending_next: PageId,
    /// Whether a transaction is open (writes are buffered but not committed).
    has_pending: bool,
    /// Final logical mutation for every key touched by the transaction.
    ///
    /// Keeping this beside the copied pages lets a writer rebase disjoint-key
    /// work onto a newer root inside the reservation gate. Overlapping row
    /// changes still conflict; monotonic engine metadata is merged explicitly.
    pending_ops: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// The [`Device::commit_generation`] this handle's committed state was read
    /// at, when the device counts commits at all.
    ///
    /// `None` means "no idea" — either the device does not count, or this
    /// handle has not established a value yet — and every such refresh reads
    /// the device. See [`CowBTree::refresh`].
    seen_generation: Option<u64>,
    /// Decoded *committed* pages, held so a descent does not re-read and
    /// re-decode the same nodes on every statement.
    ///
    /// Interior mutability because reads take `&self`: a lookup is logically a
    /// read even though it fills a cache. Only [`CowBTree::committed_node`]
    /// touches it, and only with data-area pages — see [`super::cache`] for why
    /// that needs no invalidation protocol and what would break it.
    cache: RefCell<PageCache>,
    /// One reusable page-sized buffer for device reads, so a cache miss does
    /// not allocate a fresh page buffer the way every read used to.
    ///
    /// Sound to reuse because [`Device::read`] fills the whole buffer or fails:
    /// no implementation leaves part of it holding the previous page's bytes.
    scratch: RefCell<Vec<u8>>,
    /// The root-to-leaf path the last committed-read point lookup descended,
    /// so the next one can reseek from wherever it is still valid instead of
    /// walking from the root again. See [`ReadCursor`] for the shape and
    /// [`CowBTree::reseek`] for how it is used and invalidated.
    cursor: RefCell<Option<ReadCursor>>,
    /// Whether [`CowBTree::alloc_page`] may hand out a page id the free list
    /// (Phase 2 item 6) has recorded as reclaimable. `false` by default and
    /// for every existing caller: with this off, allocation is exactly the
    /// monotonic counter it always was, and no free-list bookkeeping row is
    /// ever written — a database nobody opts in for is byte-for-byte
    /// unaffected. See [`CowBTree::set_page_reuse`].
    reuse_enabled: bool,
    /// `(freed_at, id)` pairs the free list currently believes are
    /// reclaimable and this handle has proven durable and live, read ahead
    /// from the tree so [`CowBTree::alloc_page`] does not pay a descent on
    /// every call. Refilled from [`FREE_LIST_PREFIX`] when empty; discarded
    /// (never leaked, just re-derived) on conflict or rebase, since either
    /// means the free list this batch was read against may no longer be
    /// current. See [`CowBTree::refill_free_candidates`].
    free_candidates: Vec<(u64, PageId)>,
    /// Committed page ids this open transaction has superseded (replaced or
    /// dropped an entry from) and not itself reused, recorded here until
    /// [`CowBTree::finalize_free_list`] turns them into durable free-list
    /// rows as part of the same commit. Cleared on rollback or conflict —
    /// see [`CowBTree::supersede`].
    freed_this_txn: Vec<PageId>,
    /// `(freed_at, id)` pairs this open transaction has drawn from
    /// [`CowBTree::free_candidates`] and used, whose free-list row therefore
    /// needs deleting as part of the same commit — see
    /// [`CowBTree::finalize_free_list`], which drains this with `pop`.
    /// Cleared on rollback or conflict, same as `freed_this_txn`; the ids
    /// themselves are never lost, only the bookkeeping is discarded, because
    /// a conflict or rebase re-derives it from a fresh scan against the
    /// winning root.
    consumed_this_txn: Vec<(u64, PageId)>,
    /// Every `(freed_at, id)` pair this transaction has ever drawn from the
    /// free list, for as long as the transaction is open. Unlike
    /// `consumed_this_txn` — which `finalize_free_list` drains with `pop`,
    /// so an entry stops appearing there the instant its deletion is
    /// underway — this one is append-only until the transaction itself ends
    /// (same clear points as `consumed_this_txn`).
    ///
    /// The distinction is load-bearing, not cosmetic: `finalize_free_list`
    /// deletes a consumed candidate's row by calling `self.delete`, which can
    /// itself need a fresh page id to rewrite the free-list subtree and so
    /// calls back into `alloc_page` while that very row's own delete is
    /// still in flight. If `refill_free_candidates` checked
    /// `consumed_this_txn` (already popped for this entry) instead of this
    /// field, it would see the row still physically present — its delete
    /// has not committed to the tree yet — conclude nothing has claimed it,
    /// and hand the *same id* out a second time: to whatever needed a fresh
    /// page, while the original delete was still using it as `source`. That
    /// aliases one id to two logical nodes, which is exactly the tree-cycle
    /// this field exists to prevent — it was a real (if short-lived)
    /// history of this implementation, reached by
    /// `free_list_reuse_dst.rs`'s heavy-churn workload within a handful of
    /// commits, and is why this comment is this specific.
    consumed_ever_this_txn: Vec<(u64, PageId)>,
    /// This handle's [`Device::register_reader`] token, when the device
    /// tracks readers at all. Kept current by [`CowBTree::update_watermark`]
    /// and released on `Drop`.
    reader_token: Option<u64>,
    /// How many pages this handle has drawn from the free list instead of
    /// allocating fresh, over its whole lifetime. Diagnostic only — a test's
    /// way to prove reclamation actually fired rather than silently declining
    /// every candidate, the same role `page_cache_len` plays for the cache.
    pages_reused: u64,
}

impl<D: Device> Drop for CowBTree<D> {
    /// Release this handle's reader watermark, if it registered one, so a
    /// later reclaim decision on this device does not wait on a handle that
    /// no longer exists.
    fn drop(&mut self) {
        if let Some(token) = self.reader_token {
            self.device.release_reader(token);
        }
    }
}

impl<D: Device> CowBTree<D> {
    /// Open an existing tree, running recovery from the write-ahead log.
    pub fn open(device: D) -> Result<Self> {
        let mut header = vec![0u8; HEADER_LEN];
        device.read(crate::wal::header_offset(), &mut header)?;
        let (page_size, format_version) = parse_header(&header)?;

        // Read the generation *before* the state, never after: a commit that
        // lands in between then leaves this handle holding the older value and
        // costs it one scan, where the other order would leave it holding a
        // generation newer than the state it actually read and skip that scan
        // forever. Every read of the counter in this file is ordered this way.
        let generation = device.commit_generation();
        let (root, next, checkpoint_seq, replay) =
            read_committed_state(&device, page_size, format_version)?;
        let mut tree = Self::new(
            device,
            page_size,
            format_version,
            root,
            next,
            checkpoint_seq,
        );
        tree.seen_generation = generation;
        tree.update_watermark(checkpoint_seq);

        // If the committed state came from the log (the state block was torn or
        // behind), replay the record's pages into the data area — healing any
        // torn writes — then checkpoint to heal the state block.
        //
        // `root`/`next`/`checkpoint_seq` above already are the walked-forward
        // values `read_committed_state` derived from `replay` — that part is a
        // read, already done, and already reflected in `tree`. What follows is
        // purely about making that durable: writing the healed pages back and
        // folding the log into a fresh checkpoint. Skip it on a device that can
        // never write (see [`Device::is_read_only`]) rather than fail the open:
        // `replay` is non-empty on essentially every open that follows any
        // uncheckpointed commit — the ordinary case, not a crash — since only a
        // full WAL region forces a checkpoint on its own. A read-only handle
        // that could not open until the last writer happened to checkpoint
        // would not open in practice. What is actually lost by skipping: none
        // of the pages this loop would rewrite, since a writer's commit already
        // wrote and synced them before appending the record that names them; a
        // page a real crash left torn stays torn until a writer next opens the
        // file, and surfaces as `Error::Corrupt` on the read that touches it —
        // the identical safety net `CowBTree::refresh` already relies on for a
        // live (non-crash) reader; see its doc comment, "Why the log records
        // are not replayed here".
        if !replay.is_empty() && !tree.device.is_read_only() {
            for record in &replay {
                for (id, bytes) in &record.pages {
                    tree.device.write(
                        crate::wal::data_offset_for(page_size, format_version, *id),
                        bytes,
                    )?;
                }
            }
            tree.checkpoint()?;
        }
        Ok(tree)
    }

    /// Open the tree on `device`, creating it if the device holds no database.
    ///
    /// "Holds no database" means the header is unreadable or does not carry our
    /// magic — a device shorter than the header, or one full of zeros. That is
    /// the only ambiguity worth resolving here: a device that *does* start with
    /// a valid header is opened (and recovered) rather than overwritten, so a
    /// real I/O error on a real database still surfaces as an error from
    /// [`CowBTree::open`] instead of silently erasing it.
    ///
    /// This is what keeps the choice of I/O backend out of the caller's way:
    /// any [`Device`] — a blocking file, an `io_uring` ring, a simulated disk —
    /// can be handed straight to the engine without the caller first having to
    /// ask the operating system whether the file is empty.
    pub fn open_or_create(device: D, page_size: usize) -> Result<Self> {
        let mut header = vec![0u8; HEADER_LEN];
        let existing = device
            .read(crate::wal::header_offset(), &mut header)
            .is_ok()
            && header.starts_with(MAGIC);
        if existing {
            Self::open(device)
        } else {
            Self::create(device, page_size)
        }
    }

    /// [`CowBTree::open_or_create`] with an explicit page cache budget in
    /// bytes. `0` disables the cache; see [`CowBTree::set_page_cache_bytes`].
    pub fn open_or_create_with_cache(
        device: D,
        page_size: usize,
        cache_bytes: usize,
    ) -> Result<Self> {
        let mut tree = Self::open_or_create(device, page_size)?;
        tree.set_page_cache_bytes(cache_bytes);
        Ok(tree)
    }

    /// Create a new empty tree on `device`, writing the header and state block.
    pub fn create(device: D, page_size: usize) -> Result<Self> {
        if page_size < page::MIN_PAGE_SIZE {
            return Err(Error::Storage(alloc::format!(
                "page size {page_size} is below the minimum {}",
                page::MIN_PAGE_SIZE
            )));
        }
        let generation = device.commit_generation();
        let mut tree = Self::new(device, page_size, FORMAT_VERSION, 0, 1, 0);
        tree.seen_generation = generation;
        tree.update_watermark(0);
        tree.device
            .write(crate::wal::header_offset(), &encode_header(page_size))?;
        tree.device
            .write(crate::wal::state_offset(page_size), &encode_state(0, 1, 0))?;
        // The log must start empty so recovery does not mistake stale bytes for
        // records on a device that was not freshly zeroed.
        let zeros = vec![
            0u8;
            crate::wal::all_regions_end(page_size, FORMAT_VERSION)
                - crate::wal::wal_start(page_size)
        ];
        tree.device
            .write(crate::wal::wal_start(page_size), &zeros)?;
        tree.device.sync()?;
        // Everything the device may have been remembering describes a database
        // that no longer exists — this one has just overwritten the header, the
        // state block and every log region. Creation is the one path that
        // rewrites all three from outside the reservation gate, so it hands the
        // question back rather than an answer; the first commit re-derives it
        // from what was just written. See [`Device::set_commit_point`].
        tree.device.set_commit_point(0, None);
        Ok(tree)
    }

    fn new(
        device: D,
        page_size: usize,
        format_version: u32,
        root: PageId,
        next: PageId,
        checkpoint_seq: u64,
    ) -> Self {
        let reader_token = device.register_reader();
        Self {
            device,
            page_size,
            format_version,
            root,
            next_page_id: next,
            next_seq: checkpoint_seq + 1,
            checkpoint_seq,
            dirty: BTreeMap::new(),
            pending_root: 0,
            pending_next: 0,
            has_pending: false,
            pending_ops: BTreeMap::new(),
            seen_generation: None,
            cache: RefCell::new(PageCache::new(DEFAULT_PAGE_CACHE_BYTES)),
            scratch: RefCell::new(vec![0u8; page_size]),
            cursor: RefCell::new(None),
            reuse_enabled: false,
            free_candidates: Vec::new(),
            freed_this_txn: Vec::new(),
            consumed_this_txn: Vec::new(),
            consumed_ever_this_txn: Vec::new(),
            reader_token,
            pages_reused: 0,
        }
    }

    /// Opt this handle into drawing on the free list (Phase 2 item 6) when
    /// [`CowBTree::alloc_page`] needs a page id, instead of always bumping
    /// the monotonic counter.
    ///
    /// Off by default and for every existing caller — see the field doc on
    /// `reuse_enabled`. Turning it on does not retroactively reclaim
    /// anything; it only changes what future commits from *this handle* are
    /// willing to do, and every reclaim decision still passes the durability
    /// and liveness checks in [`CowBTree::refill_free_candidates`]. It also
    /// makes every future root change clear this handle's page cache and
    /// retained read cursor — see [`CowBTree::invalidate_for_reuse`] — which
    /// is the entire cost this trades away for reclaiming space; a handle
    /// that never turns it on pays none of it.
    ///
    /// # Read this before enabling it
    ///
    /// Reclamation can prove liveness only for readers this process's
    /// reservation gate can see — every read-write [`CowBTree`] sharing this
    /// device. A handle opened with a read-only device (no OS lock, by
    /// design — see `FileDevice::open_read_only`) is invisible to that
    /// proof, in this process or any other, and there is no way for this
    /// method or anything else to rule one out. **Do not enable this on a
    /// file any process might open read-only while a writer here has it
    /// on.** That is a real, load-bearing constraint, not a caveat: it is
    /// the reason this is a handle-level opt-in instead of the default.
    pub fn set_page_reuse(&mut self, enabled: bool) {
        if enabled {
            // Page ids may now be reissued with new content, which breaks the
            // immutability assumption every cache keyed by page id or data
            // offset rests on (`super::cache`, D4). This handle's own decoded
            // cache and read cursor are handled by `invalidate_for_reuse` at
            // every root change; the device gets told here, once, so a
            // device-level cache shared by several handles — `FileDevice`'s
            // raw-page cache, in the `inlaysql` crate — can flush itself and
            // stay off. See [`Device::note_page_reuse_enabled`].
            self.device.note_page_reuse_enabled();
        }
        self.reuse_enabled = enabled;
    }

    /// Whether this handle will draw on the free list. See
    /// [`CowBTree::set_page_reuse`].
    pub fn page_reuse(&self) -> bool {
        self.reuse_enabled
    }

    /// How many pages this handle has drawn from the free list instead of
    /// allocating fresh, over its whole lifetime. See the field doc on
    /// `pages_reused`.
    pub fn pages_reused(&self) -> u64 {
        self.pages_reused
    }

    /// Clear this handle's page cache and retained read cursor, when page
    /// reuse is on — a no-op, including the two `RefCell` borrows, when it
    /// is off, which is every existing caller and the default.
    ///
    /// Called at every point `self.root` (or the state a conflict/rebase/
    /// checkpoint adopts) changes to a value this handle has not already
    /// built its cache from. This is the whole of what [`super::cache`]'s
    /// "the free list must version cache entries" warning resolves to here:
    /// not a per-entry `(page id, commit seq)` stamp, but a coarse,
    /// trivially-provable epoch — "no entry survives a root change once
    /// reuse is possible" needs only "the cache is empty", not "every
    /// lookup path checks a stamp everywhere it could read one". The
    /// alternative — reading a durable counter to decide *whether* to clear
    /// — was tried and rejected: the read to answer that question would
    /// itself walk the tree through the very cache it exists to decide
    /// whether to trust, which is circular. This is deliberately coarser (a
    /// root change from *any* commit clears the cache, not only one that
    /// reused a page) in exchange for having no such circularity — see
    /// `CowBTree::set_page_reuse`'s doc comment for the cost this puts only
    /// on a handle that opted in.
    fn invalidate_for_reuse(&self) {
        if self.reuse_enabled {
            self.cache.borrow_mut().clear();
            *self.cursor.borrow_mut() = None;
        }
    }

    /// Tell the device this handle now needs nothing older than `seq`,
    /// keeping [`Device::min_reader_seq`] current for a reclaim decision
    /// elsewhere on this device. A no-op on a device that does not track
    /// readers (`self.reader_token` is `None`) — see
    /// [`Device::register_reader`].
    fn update_watermark(&self, seq: u64) {
        if let Some(token) = self.reader_token {
            self.device.update_reader(token, seq);
        }
    }

    /// The device, exposed for the simulation harness to inspect state.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// Bound the decoded-page cache to `bytes` of resident memory. `0` turns it
    /// off, which restores the read-and-decode-every-time behaviour.
    ///
    /// This is memory the process holds on top of everything else, per open
    /// tree; see [`super::cache`] for what it buys and what it costs.
    pub fn set_page_cache_bytes(&mut self, bytes: usize) {
        self.cache.borrow_mut().set_budget(bytes);
    }

    /// The cache's byte budget.
    pub fn page_cache_bytes(&self) -> usize {
        self.cache.borrow().budget()
    }

    /// How many decoded pages are resident. Diagnostic, and how a test proves
    /// eviction really happens.
    pub fn page_cache_len(&self) -> usize {
        self.cache.borrow().len()
    }

    /// The committed root page id. Readers pin this for a snapshot.
    pub fn root(&self) -> PageId {
        self.root
    }

    /// The database format stamped in this file's immutable header.
    pub fn format_version(&self) -> u32 {
        self.format_version
    }

    /// Whether uncommitted writes are buffered.
    pub fn is_dirty(&self) -> bool {
        self.has_pending
    }

    /// Bytes the open transaction's log record would occupy.
    ///
    /// Exact, not an estimate: it is the size [`crate::wal::encode_record`]
    /// produces, computed without building the record. A caller writing a
    /// large amount of data can use it, against [`CowBTree::log_capacity`], to
    /// decide when to commit — which it has to do, because a transaction
    /// larger than the log region cannot be committed at all.
    pub fn pending_record_len(&self) -> usize {
        // Length prefix, seq, root, next, page count, and the trailing
        // checksum — then each page with its id and length.
        let header = 4 + 8 + 8 + 8 + 4 + 8;
        header
            + self
                .dirty
                .values()
                .map(|bytes| 8 + 4 + bytes.len())
                .sum::<usize>()
    }

    /// The largest transaction this tree can commit, in bytes.
    pub fn log_capacity(&self) -> usize {
        crate::wal::max_record_len(self.page_size)
    }

    /// Look up `key`, seeing the open transaction's own writes.
    ///
    /// A writer reads what it just wrote: the lookup starts at
    /// [`CowBTree::pending_root`](Self::read_root) and resolves pages out of the
    /// transaction's dirty set before the data area. Without that, a
    /// multi-statement transaction could `INSERT` a row and then not find it in
    /// the very next `SELECT`, and any caller that builds a structure *inside* a
    /// transaction by reading back what it wrote — the paged ANN index does
    /// exactly this — would read a hole where its own record is.
    ///
    /// Snapshot reads that must ignore the open transaction use
    /// [`CowBTree::get_at`] with a committed root.
    pub fn get(&self, key: &[u8]) -> Result<Option<RowBuf>> {
        self.get_from(self.read_root(), key, self.has_pending)
    }

    /// Look up `key` in the snapshot rooted at `root`. As long as `root` was
    /// committed, the pages it reaches are never overwritten, so the read is a
    /// consistent snapshot even after later commits.
    pub fn get_at(&self, root: PageId, key: &[u8]) -> Result<Option<RowBuf>> {
        self.get_from(root, key, false)
    }

    /// A committed read (`pending` is false) tries [`CowBTree::reseek`] first
    /// — reusing the previous lookup's leaf directly when `key` is still
    /// inside the span it covers — and only walks from the root when that
    /// leaf cannot answer this key at all. A pending (in-transaction) read
    /// always walks from the root: [`CowBTree::reseek`] only ever retains a
    /// committed leaf, so there is nothing to try there. Either way, the
    /// descent this function does becomes the new retained leaf for next
    /// time.
    fn get_from(&self, root: PageId, key: &[u8], pending: bool) -> Result<Option<RowBuf>> {
        if !pending {
            if let Some(hit) = self.reseek(root, key)? {
                return Ok(hit);
            }
        }
        let mut id = root;
        // Which ancestor's separator currently supplies the cumulative lower
        // (respectively upper) bound of `id`'s subtree — the internal node
        // and the index into its cells, not the key bytes themselves. A
        // level that routes through its leftmost (rightmost) child leaves
        // the bound on that side exactly as its own parent left it, so most
        // levels touch neither: kept this way, a descent that never reaches
        // `retain_cursor` (every write, and every probe once `reseek` starts
        // hitting) allocates nothing for it. Only whichever bound is still
        // active once a leaf is actually reached gets cloned, in
        // `retain_cursor` — at most two small clones no matter how deep the
        // tree is, not one pair per level.
        let mut low_source: Option<(Rc<Node>, usize)> = None;
        let mut high_source: Option<(Rc<Node>, usize)> = None;
        loop {
            if id == 0 {
                return Ok(None);
            }
            // Borrowed, not owned: a cache hit hands back a shared decoded node
            // and the descent only reads it, so nothing is copied per level.
            let node = self.node_at(id, pending)?;
            match &*node {
                Node::Leaf { entries, .. } => {
                    let result = match entries.binary_search_by(|e| node.key(&e.key).cmp(key)) {
                        Ok(i) => self
                            .resolve_value_at(Some(node.bytes()), &entries[i].value, pending)
                            .map(Some)?,
                        Err(_) => None,
                    };
                    if !pending {
                        self.retain_cursor(root, id, low_source, high_source);
                    }
                    return Ok(result);
                }
                Node::Internal {
                    leftmost, cells, ..
                } => {
                    let idx = child_index(node.bytes(), cells, key);
                    id = if idx == 0 {
                        *leftmost
                    } else {
                        cells[idx - 1].child
                    };
                    // Skipped entirely for a pending read: it never reaches
                    // `retain_cursor` below, so tracking a bound source for it
                    // would only be a wasted refcount bump per level.
                    if !pending {
                        if idx > 0 {
                            low_source = Some((Rc::clone(&node), idx - 1));
                        }
                        if idx < cells.len() {
                            high_source = Some((Rc::clone(&node), idx));
                        }
                    }
                }
            }
        }
    }

    /// Try to answer `key` under `root` from the previous committed lookup's
    /// retained leaf, when `key` is still inside the span that leaf covers —
    /// the cursor behaviour SQLite gets by tracking `(page, position)` across
    /// seeks of the same cursor, narrowed to the one case that is both by far
    /// the most common for a join probe (the row ids [`IndexProbe::prepare`]
    /// fetches are sorted, so consecutive fetches are typically adjacent keys
    /// on the same leaf) and the cheapest to keep correct: this tree's nodes
    /// carry no parent or sibling pointer, so answering anything past "is it
    /// still on this exact leaf" would mean walking back up a retained path,
    /// which is the more general design `PERF.md` leaves as a further step if
    /// this narrower one stops paying.
    ///
    /// Returns `Ok(None)` when the retained leaf cannot answer this lookup at
    /// all — nothing retained yet, a different `root`, or `key` outside its
    /// span — and the caller falls back to [`CowBTree::get_from`]'s full
    /// descent from the root, which repopulates the cursor for next time. A
    /// `root` mismatch is the common miss case across a write: `root` only
    /// changes on [`CowBTree::commit`], [`CowBTree::refresh`] or a rebase, all
    /// of which pass a *new* root value into the next call here, so a leaf
    /// retained under the old one simply stops matching — there is no
    /// separate invalidation step to remember.
    ///
    /// # Why this is sound without a page-reuse guard of its own
    ///
    /// A retained cursor names a page id and the key span it answers for.
    /// Reusing it later is exactly as sound as [`super::cache::PageCache`]
    /// reusing a cached node for the same id — same reasoning, same
    /// prerequisite: a page id names one immutable sequence of bytes for the
    /// life of the file (`super::cache`'s "Why no invalidation protocol is
    /// needed", and [`CowBTree::adopt_next_page_id`] is what keeps it true
    /// after AHL-406). Whoever lands the Phase 2 item 6 free list must extend
    /// *this* cursor the same way it must extend the page cache: page reuse
    /// makes a retained id stale exactly like it makes a cached node stale.
    ///
    /// # Why a pending write cannot invalidate this mid-scan
    ///
    /// This is only ever consulted for a committed read (`get_from`'s
    /// `pending` is false), and the streaming pipeline that drives a join
    /// probe holds its `Storage` as `&dyn Storage` for the whole scan — a
    /// shared reference, which the borrow checker guarantees excludes any
    /// concurrent `&mut Storage` write (`put_row`, `commit`, ...) on the same
    /// handle for as long as it is held. So no commit, and therefore no root
    /// change, can happen between one probe's lookups and the next; the
    /// `root` check above is what protects the case where the pipeline as a
    /// whole spans a commit (a later statement, a different handle after a
    /// `refresh`), not concurrent mutation during one scan.
    fn reseek(&self, root: PageId, key: &[u8]) -> Result<Option<Option<RowBuf>>> {
        let Ok(slot) = self.cursor.try_borrow() else {
            return Ok(None);
        };
        let Some(cursor) = slot.as_ref() else {
            return Ok(None);
        };
        if cursor.root != root || !cursor.admits(key) {
            return Ok(None);
        }
        let leaf = cursor.leaf;
        // Release the borrow before the lookup: `node_at` never touches
        // `self.cursor`, but there is no reason to hold it any longer than
        // this check needs.
        drop(slot);
        let node = self.node_at(leaf, false)?;
        let Node::Leaf { entries, .. } = &*node else {
            // A page id this cursor named is no longer a leaf. Unreachable
            // under the invariant `reseek`'s doc comment states — a page id
            // names one immutable node for the file's lifetime — but a full
            // descent is always a correct answer, so fall back rather than
            // trust a cursor that turned out not to describe a leaf.
            return Ok(None);
        };
        let result = match entries.binary_search_by(|e| node.key(&e.key).cmp(key)) {
            Ok(i) => Some(self.resolve_value_at(Some(node.bytes()), &entries[i].value, false)?),
            Err(_) => None,
        };
        Ok(Some(result))
    }

    /// Retain `leaf` — reached under `root`, with its key span still named by
    /// `low_source`/`high_source` — as the cursor to reseek from next time,
    /// replacing whatever was there. This is the one place the span's key
    /// bytes are actually cloned, at most once per side.
    ///
    /// Best-effort: if the cursor is somehow already borrowed, the retained
    /// leaf is simply not updated rather than panic — the next lookup falls
    /// back to a full descent, which is always correct, only slower.
    fn retain_cursor(
        &self,
        root: PageId,
        leaf: PageId,
        low_source: Option<(Rc<Node>, usize)>,
        high_source: Option<(Rc<Node>, usize)>,
    ) {
        let low = bound_key(low_source);
        let high = bound_key(high_source);
        if let Ok(mut slot) = self.cursor.try_borrow_mut() {
            *slot = Some(ReadCursor {
                root,
                leaf,
                low,
                high,
            });
        }
    }

    /// Insert or overwrite `key`, buffering the change until [`CowBTree::commit`].
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if !key_fits(self.page_size, key) {
            return Err(Error::Storage("key is too large for a page".to_string()));
        }
        self.begin_txn();
        match self.insert_into(self.pending_root, key, value)? {
            InsertOutcome::Replaced { id } => self.pending_root = id,
            InsertOutcome::Split {
                left,
                right,
                separator,
            } => {
                let cells = vec![Separator {
                    key: Key::Owned(separator),
                    child: right,
                }];
                let id = self.alloc_page();
                // Every key is owned, so no shared page buffer is indexed here.
                self.dirty.insert(
                    id,
                    page::encode_internal(self.page_size, &[], left, &cells)?,
                );
                self.pending_root = id;
            }
        }
        self.pending_ops.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }

    /// Remove `key`, buffering the change until [`CowBTree::commit`]. Deleting
    /// a missing key is not an error.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.begin_txn();
        self.pending_root = self.delete_from(self.pending_root, key)?;
        self.pending_ops.insert(key.to_vec(), None);
        Ok(())
    }

    /// Commit the buffered transaction to the write-ahead log and make it
    /// durable.
    ///
    /// The new pages are written to the data area *and* copied into the commit
    /// record, then the record is appended and the device synced once. That
    /// single sync is the commit point: the record and its pages become durable
    /// together, and because the record carries the pages, recovery can rebuild
    /// them if a later fault loses the data-area copies.
    ///
    /// The state block is *not* rewritten here — that happens on
    /// [`CowBTree::checkpoint`], so the hot path stays at one sync per commit.
    ///
    /// # First-committer-wins
    ///
    /// The committed root is refreshed under the device's short reservation
    /// gate. A stale transaction is rebased when none of its row keys changed;
    /// if both writers changed a row, this one aborts with
    /// [`CommitOutcome::Conflict`] and reloads the winner. Sequence/page
    /// reservation and WAL placement are ordered, while the expensive sync is
    /// outside the gate and can overlap with other writer regions.
    pub fn commit(&mut self) -> Result<CommitOutcome> {
        if !self.has_pending {
            return Ok(CommitOutcome::Committed);
        }
        self.device.begin_commit()?;
        let region = self.device.wal_region() % crate::wal::region_count(self.format_version);
        // What the gate exists to establish: the committed state to rebase
        // against, and where this region's next record goes. A device that can
        // speak for every writer on the file hands both back from memory; every
        // other device re-derives them by reading the state block and scanning
        // the log. See [`Device::commit_point`] — this is the whole of AHL-468.
        let cached = self.device.commit_point(region);
        let prepared = (|| {
            let (current_root, current_next, current_seq) = match cached {
                Some(point) => (point.root, point.next, point.seq),
                None => {
                    let (root, next, seq, _) =
                        read_committed_state(&self.device, self.page_size, self.format_version)?;
                    (root, next, seq)
                }
            };
            // Rebuild even when the root did not move: an Engine handle can
            // have had its previous transaction rebased by the tree, leaving
            // its in-memory counters behind the values actually reserved on
            // disk. Normalising monotonic metadata on every commit prevents a
            // later statement from moving those counters backwards.
            if !self.rebase_pending(current_root, current_next, current_seq)? {
                return Ok(None);
            }

            let seq = current_seq + 1;
            // Turn this transaction's superseded/reused pages into durable
            // free-list rows before the record below is built, so they ride
            // the same commit — see `CowBTree::finalize_free_list`.
            self.finalize_free_list(seq)?;
            let record = crate::wal::WalRecord {
                seq,
                prev_seq: current_seq,
                prev_root: current_root,
                root: self.pending_root,
                next: self.pending_next,
                pages: self
                    .dirty
                    .iter()
                    .map(|(&id, bytes)| (id, bytes.clone()))
                    .collect(),
            };
            let encoded = if self.format_version >= crate::wal::MULTI_REGION_FORMAT_VERSION {
                crate::wal::encode_record(&record)
            } else {
                crate::wal::encode_legacy_record(&record)
            };
            if encoded.len() > crate::wal::max_record_len(self.page_size) {
                return Err(Error::Storage(alloc::format!(
                    "transaction does not fit the write-ahead log ({} > {} bytes)",
                    encoded.len(),
                    crate::wal::max_record_len(self.page_size)
                )));
            }

            let mut append_offset = match cached {
                Some(point) => point.append_offset,
                None => {
                    crate::wal::scan_region(
                        &self.device,
                        self.page_size,
                        self.format_version,
                        region,
                    )?
                    .append_offset
                }
            };
            if append_offset + encoded.len()
                > crate::wal::region_end(self.page_size, self.format_version, region)
            {
                // The region is about to be reused, so the cached answer stops
                // being true the instant the zeroing write lands — and if that
                // write fails part-way there is no answer to replace it with.
                // Forget first, publish only once the wrap has completed.
                self.device.set_commit_point(region, None);
                self.write_state_values(current_root, current_next, current_seq)?;
                let zeros = vec![0u8; crate::wal::wal_region_len(self.page_size)];
                append_offset =
                    crate::wal::region_start(self.page_size, self.format_version, region);
                self.device.write(append_offset, &zeros)?;
            }
            self.write_dirty_pages()?;
            self.device.write(append_offset, &encoded)?;
            // Published after the record it describes is written and before the
            // gate is left, so the next committer reads a state whose pages and
            // record are already on the file — the ordering `end_commit` uses,
            // for the same reason.
            self.device.set_commit_point(
                region,
                Some(CommitPoint {
                    root: self.pending_root,
                    next: self.pending_next,
                    seq,
                    append_offset: append_offset + encoded.len(),
                }),
            );
            Ok(Some(seq))
        })();
        if prepared.is_err() {
            // Still inside the gate. Something between reserving and appending
            // failed, and this commit is the only thing that knows how far it
            // got, so it hands back the question rather than an answer.
            self.device.set_commit_point(region, None);
        }
        // The generation this commit produced, read atomically with the
        // increment that produced it — see [`Device::end_commit`].
        let generation = self.device.end_commit();

        let Some(seq) = prepared? else {
            // The conflict path re-reads outside the gate, so it takes the
            // generation the ordinary way: counter first, state second.
            let generation = self.device.commit_generation();
            let (current_root, current_next, current_seq) = match self.device.commit_point(region) {
                Some(point) => (point.root, point.next, point.seq),
                None => {
                    let (root, next, seq, _) =
                        read_committed_state(&self.device, self.page_size, self.format_version)?;
                    (root, next, seq)
                }
            };
            self.dirty.clear();
            self.pending_ops.clear();
            self.has_pending = false;
            self.free_candidates.clear();
            self.freed_this_txn.clear();
            self.consumed_this_txn.clear();
            self.consumed_ever_this_txn.clear();
            self.root = current_root;
            self.adopt_next_page_id(current_next);
            self.checkpoint_seq = current_seq;
            self.next_seq = current_seq + 1;
            self.seen_generation = generation;
            self.invalidate_for_reuse();
            self.update_watermark(current_seq);
            return Ok(CommitOutcome::Conflict);
        };

        // The commit record is already ordered and lives in this writer's own
        // region. Durability is intentionally outside the reservation gate:
        // this is the expensive operation parallel writers are allowed to
        // overlap.
        self.device.sync()?;

        self.next_seq = seq + 1;
        self.root = self.pending_root;
        self.next_page_id = self.pending_next;
        self.dirty.clear();
        self.pending_ops.clear();
        self.has_pending = false;
        // This handle's state is now the newest committed state there is: it
        // read the previous one under the gate and added its own record to it,
        // and no other writer could have committed in between, because they
        // hold the same gate. Recording the generation is what keeps the
        // statement after a commit off the scanning path.
        self.seen_generation = generation;
        // This commit may itself have reused a page (`consumed_this_txn`,
        // drained by `finalize_free_list` above), so this handle's own cache
        // needs the same treatment a conflict or refresh gets — see
        // `CowBTree::invalidate_for_reuse`.
        self.invalidate_for_reuse();
        self.update_watermark(seq);
        Ok(CommitOutcome::Committed)
    }

    /// Discard the buffered transaction without writing anything to the device.
    ///
    /// Unlike a conflict — where another writer committed first and the tree
    /// reloads the winner's state — an explicit rollback discards writes
    /// against the *same* committed root, so the tree simply forgets its dirty
    /// pages. The next transaction starts from the committed state exactly as
    /// before.
    pub fn rollback(&mut self) {
        self.dirty.clear();
        self.pending_ops.clear();
        self.has_pending = false;
        self.free_candidates.clear();
        self.freed_this_txn.clear();
        self.consumed_this_txn.clear();
        self.consumed_ever_this_txn.clear();
    }

    /// Adopt the state another writer has committed since this handle last
    /// looked, and report whether anything moved.
    ///
    /// A handle caches the committed root at open and only refreshes it inside
    /// [`CowBTree::commit`] and [`CowBTree::checkpoint`], so a handle that never
    /// writes never advances: it reads the snapshot it opened on, forever. That
    /// is fine for a snapshot pinned on purpose and wrong for a handle that is
    /// simply between statements. This is how the layer above steps such a
    /// handle forward.
    ///
    /// Returns `false` — changing nothing — when a transaction is open. The
    /// buffered writes are rooted at the snapshot they were built against, and
    /// moving the committed root out from under them would silently rebase a
    /// transaction the caller believes is pinned. A stale transaction is
    /// rebased at commit, under the reservation gate, where a genuine overlap
    /// can still be reported as [`CommitOutcome::Conflict`]; that is the only
    /// place the decision belongs.
    ///
    /// # Why the log records are not replayed here
    ///
    /// `read_committed_state` hands back the log records the state block is
    /// behind, and [`CowBTree::open`] writes their pages into the data area
    /// before trusting the root they name. That replay is crash repair: a
    /// commit writes its pages to the data area *before* the record that makes
    /// them durable, so a power loss can leave a data-area page torn while the
    /// log holds a whole copy.
    ///
    /// A live refresh is not recovering from a crash. Between checkpoints the
    /// returned records are simply the ordinary commits since the last one —
    /// non-empty is the normal case, not the damaged one — and the pages they
    /// describe were written by a writer that is still running, in program
    /// order, ahead of the record this handle just read. A reader that can see
    /// the record can see the pages: same file, same page cache, no crash in
    /// between. Replaying them would be a *write* issued from a read path,
    /// outside the reservation gate, by a handle that may hold the file
    /// read-only — strictly worse than not replaying, and it would run on every
    /// statement.
    ///
    /// This is also exactly what [`CowBTree::commit`] and
    /// [`CowBTree::checkpoint`] already do: both re-read the committed state,
    /// adopt the root a concurrent writer left, and discard the replay list. A
    /// refresh that healed pages they do not would be defending against a
    /// failure the commit path is already exposed to. The repair belongs where
    /// it is — on the open that follows the crash.
    ///
    /// The residual risk is bounded and visible: if a data-area page really
    /// were unreadable, [`page::decode`] rejects it and the read fails with
    /// [`Error::Corrupt`]. A refresh cannot turn a torn page into a wrong
    /// answer, only into an error that the next open repairs.
    ///
    /// # Cost — why nothing changed has to be free
    ///
    /// `read_committed_state` re-reads the state block and then scans every WAL
    /// region from its start, and [`crate::wal::scan_region`] reads and decodes
    /// each record *whole* — every 4 KiB data page the commit copied, plus a
    /// byte-at-a-time checksum over all of it — because that is what recovery
    /// needs. A refresh throws all of it away and keeps five integers.
    ///
    /// The cost is therefore proportional to the bytes committed since the last
    /// checkpoint, not to what changed, and it falls on every statement. When
    /// this method was introduced it paid that on every call, and
    /// `SUITE=points ./bench/run.sh` (20,000 rows, 5,000 primary key lookups)
    /// put point-read p50 at 236.00 µs where the commit before it read 6.50 µs
    /// — a 36x regression on the narrowest read path there is. With the
    /// generation check below the same suite reads 6.79 µs, inside the
    /// run-to-run spread of the pre-refresh baseline.
    ///
    /// So the common answer is given without touching the device at all. If
    /// [`Device::commit_generation`] reports a value and it is the one this
    /// handle recorded when it last read the committed state, then nothing has
    /// been committed since and there is nothing to adopt: return `false`,
    /// having read no state block and scanned no log. The counter is read
    /// *before* the state, so a commit landing in between costs a scan rather
    /// than being skipped.
    ///
    /// A device that answers `None` — the simulated disk and the fault
    /// injector, deliberately — takes the full scan every time, which is what
    /// keeps the deterministic simulation exercising the real path rather than
    /// this shortcut. Read [`Device::commit_generation`] before making another
    /// device answer `Some`: it is sound here only because no writer outside
    /// this process can exist.
    ///
    /// # What is still not incremental
    ///
    /// When the generation *has* moved, the scan is still a full one, and so is
    /// the one [`CowBTree::commit`] does under the gate. Closing that needs an
    /// incremental scan: remember each region's validated append offset and
    /// resume from it, re-deriving the whole chain only when the state block's
    /// checkpoint sequence moves (which is what a wrap or a checkpoint — the
    /// only things that rewind a region — always writes) or when the last
    /// record this handle validated is no longer where it left it. That is a
    /// separate change with its own DST pass; it is deliberately not folded in
    /// here, because the generation check already takes the cost off the read
    /// path, where it was measured.
    pub fn refresh(&mut self) -> Result<bool> {
        if self.has_pending {
            return Ok(false);
        }
        let generation = self.device.commit_generation();
        if generation.is_some() && generation == self.seen_generation {
            return Ok(false);
        }
        // The generation moved, so the scan below is the cost this method's
        // fast path cannot avoid — unless the device can hand back what the
        // scan would derive. Under several writers the generation moves on
        // essentially every statement, so this is the common case there rather
        // than the rare one (AHL-468).
        let region = self.device.wal_region() % crate::wal::region_count(self.format_version);
        let (root, next, seq) = match self.device.commit_point(region) {
            Some(point) => (point.root, point.next, point.seq),
            None => {
                let (root, next, seq, _replay) =
                    read_committed_state(&self.device, self.page_size, self.format_version)?;
                (root, next, seq)
            }
        };
        self.seen_generation = generation;
        if root == self.root {
            return Ok(false);
        }
        self.root = root;
        self.adopt_next_page_id(next);
        self.checkpoint_seq = seq;
        self.next_seq = seq + 1;
        self.invalidate_for_reuse();
        self.update_watermark(seq);
        Ok(true)
    }

    /// Persist the current committed state to the state block, sync, and
    /// truncate the log.
    ///
    /// Until this runs, a crash is recovered by replaying the log; after it
    /// runs, the state block alone names the committed tree and the log region
    /// can be reused. Called automatically when the log fills, and explicitly
    /// by callers that want a clean close.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.device.begin_commit()?;
        let region = self.device.wal_region() % crate::wal::region_count(self.format_version);
        let result = (|| {
            let (root, next, seq) = match self.device.commit_point(region) {
                Some(point) => (point.root, point.next, point.seq),
                None => {
                    let (root, next, seq, _) =
                        read_committed_state(&self.device, self.page_size, self.format_version)?;
                    (root, next, seq)
                }
            };
            self.root = root;
            self.adopt_next_page_id(next);
            self.next_seq = seq + 1;
            self.checkpoint_seq = seq;
            self.invalidate_for_reuse();
            self.update_watermark(seq);
            // The region below is about to be reused, so where its next record
            // goes changes here. Forget before the writes rather than after, so
            // a failure part-way through leaves "unknown" rather than "wrong".
            self.device.set_commit_point(region, None);
            self.write_state()?;

            // Reuse only this handle's region. Records in every other region
            // remain harmless because the checkpoint sequence makes them old.
            let zeros = vec![0u8; crate::wal::wal_region_len(self.page_size)];
            let start = crate::wal::region_start(self.page_size, self.format_version, region);
            self.device.write(start, &zeros)?;
            self.device.set_commit_point(
                region,
                Some(CommitPoint {
                    root,
                    next: self.next_page_id,
                    seq,
                    append_offset: start,
                }),
            );
            Ok(())
        })();
        if result.is_err() {
            self.device.set_commit_point(region, None);
        }
        // Same argument as [`CowBTree::commit`]: the state was read under the
        // gate, no other writer could have committed while it was held, and
        // this value is atomic with the increment leaving the gate produced.
        let generation = self.device.end_commit();
        if result.is_ok() {
            self.seen_generation = generation;
        }
        result
    }

    /// Read every `(key, value)` pair in the tree, in key order, including the
    /// open transaction's own writes.
    pub fn scan(&self) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        self.scan_prefix(&[])
    }

    /// Read the `(key, value)` pairs whose key starts with `prefix`, in key
    /// order, including the open transaction's own writes.
    ///
    /// The walk prunes: a subtree is descended into only when its key range can
    /// still contain `prefix`. That matters because the whole database lives in
    /// one tree — rows of every table plus the engine's metadata — so an
    /// unpruned walk would decode and materialise every row in the file to
    /// answer a scan of one table. With the prefix, the cost is the matching
    /// range plus the path down to it.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        // A prefix walk *is* a range walk: the keys starting with `prefix` are
        // exactly those in `[prefix, prefix_upper_bound(prefix))`. Saying it
        // once means one traversal to reason about and one traversal the DST
        // sweeps cover.
        let upper = prefix_upper_bound(prefix);
        self.scan_range(prefix, upper.as_deref())
    }

    /// Read the `(key, value)` pairs whose key is in `[start, end)`, in key
    /// order, including the open transaction's own writes.
    ///
    /// `end` of `None` runs to the end of the key space. The walk prunes the
    /// same way [`CowBTree::scan_prefix`] does — a subtree is descended into
    /// only when its key range can still intersect the range asked for — which
    /// is what keeps a secondary index probe from touching the whole database.
    pub fn scan_range(&self, start: &[u8], end: Option<&[u8]>) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        self.scan_range_from(start, end, None, usize::MAX)
    }

    /// At most `limit` `(key, value)` pairs whose key starts with `prefix` and
    /// is strictly greater than `after`, in key order.
    ///
    /// This is the batched form of [`CowBTree::scan_prefix`], and it is what
    /// makes a streaming scan possible above the storage seam: the caller reads
    /// a bounded run, does something with it, and comes back with the last key
    /// it saw. Both bounds prune — a subtree whose whole key range is at or
    /// below `after` is never descended into, so resuming costs a descent
    /// rather than a re-walk of everything already returned.
    ///
    /// Ordering, and therefore correctness of the resume, is the same argument
    /// [`crate::storage::row_key`] makes: keys are ordered lexicographically
    /// and a row key is its table's prefix followed by a big-endian row id, so
    /// key order within a prefix *is* row-id order.
    pub fn scan_prefix_from(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        let upper = prefix_upper_bound(prefix);
        self.scan_range_from(prefix, upper.as_deref(), after, limit)
    }

    /// At most `limit` `(key, value)` pairs whose key is in `[start, end)` and
    /// is strictly greater than `after`, in key order.
    ///
    /// The one traversal every read above goes through — the prefix scan, the
    /// secondary-index range probe (AHL-423) and the batched streaming scan
    /// (AHL-462) are the same walk asked three different questions, because a
    /// prefix *is* a range and a resume point *is* a tighter lower bound. One
    /// traversal is one thing to reason about and one thing the DST sweeps
    /// cover.
    pub fn scan_range_from(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, RowBuf)>> {
        let mut out = Vec::new();
        if limit == 0 || end.is_some_and(|end| end <= start) {
            return Ok(out);
        }
        let bounds = WalkBounds {
            start,
            end,
            after,
            limit,
        };
        self.walk(self.read_root(), &bounds, self.has_pending, &mut out)?;
        Ok(out)
    }

    /// At most `limit` row ids among entries in `[start, end)` and strictly
    /// greater than `after`, in the order the walk visits them.
    ///
    /// **Not necessarily row-id order** — a range spanning more than one
    /// value groups by value first (`crate::index`'s module docs), so a
    /// caller that needs row-id order still sorts the result, exactly as it
    /// would sort [`CowBTree::scan_range_from`]'s keys after decoding a row id
    /// out of each one.
    ///
    /// This is the row-id-only sibling of `scan_range_from`, for the one
    /// caller that only ever wanted the row id an entry names — a secondary
    /// index probe (`AHL-479`) — and never the key bytes that describe *why*
    /// it matched or the value, which for an index entry is always empty
    /// (`crate::index`'s module docs). `scan_range_from` clones every admitted
    /// key into an owned `Vec<u8>` and resolves its value before an index
    /// probe immediately decodes eight bytes out of the key and throws the
    /// rest away; this walk reads those eight bytes straight out of the
    /// borrowed entry instead, so a probe over a wide range no longer pays an
    /// allocation and a value resolution per entry it is going to discard.
    ///
    /// Sound only because **every key this tree stores under the engine's own
    /// encodings ends with its row id as eight big-endian bytes** — a table
    /// row's key (`crate::storage::row_key`) and a secondary index entry's key
    /// (`crate::index::entry_key`) both do, by construction. The tree itself
    /// stays agnostic of which encoding a caller is using — this only relies
    /// on the *shape* both share, not on index keys specifically — but a
    /// caller over a key space that does not share it would get eight
    /// meaningless bytes back with no error, which is why this is not the
    /// general-purpose walk. `an_index_row_id_walk_agrees_with_the_general_entry_walk`
    /// in this module's tests pins the one caller that exists today
    /// (`Storage::scan_index_row_ids`) against `scan_range_from` plus the
    /// ordinary decode, rather than trusting the two walks to stay in step by
    /// inspection alone — the discipline `AGENTS.md` asks of every new fast
    /// path next to the slow one it replaces.
    pub fn scan_range_row_ids_from(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<RowId>> {
        let mut out = Vec::new();
        if limit == 0 || end.is_some_and(|end| end <= start) {
            return Ok(out);
        }
        let bounds = WalkBounds {
            start,
            end,
            after,
            limit,
        };
        self.walk_row_ids(self.read_root(), &bounds, self.has_pending, &mut out)?;
        Ok(out)
    }

    /// At most `limit` `(row id, value)` pairs whose key starts with `prefix`
    /// and is strictly greater than `after`, in row-id order.
    ///
    /// The table-scan sibling of [`CowBTree::scan_prefix_from`]: a table row's
    /// key is its prefix followed by a big-endian row id, so a scan of one
    /// table wants the row id and the value, and never the key bytes it would
    /// decode the row id out of and throw away. This walk reads the row id
    /// straight out of the borrowed entry — the same shape argument
    /// [`CowBTree::scan_range_row_ids_from`] relies on — and resolves the
    /// value, without cloning the key.
    ///
    /// **Precondition:** `prefix` must be a *table* prefix — every key under it
    /// ends with its row id as eight big-endian bytes, the shape
    /// [`crate::storage::row_key`] builds. Over any other key space the eight
    /// bytes are meaningless, which is why this is not the general walk and why
    /// it is crate-private.
    ///
    /// This is the decoded-node parity oracle for the raw-leaf walk
    /// ([`CowBTree::scan_prefix_row_values_raw_from`]); test-only.
    #[cfg(test)]
    pub(crate) fn scan_prefix_row_values_from(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        let upper = prefix_upper_bound(prefix);
        self.scan_range_row_values_from(prefix, upper.as_deref(), after, limit)
    }

    /// At most `limit` `(row id, value)` pairs whose key is in `[start, end)`
    /// and is strictly greater than `after`, in the order the walk visits them.
    ///
    /// The row-id-and-value form of [`CowBTree::scan_range_from`]: same pruning,
    /// same order, same `WalkBounds` semantics — only the leaf branch differs
    /// (row id out of the borrowed key, value resolved, no key clone). Crate-
    /// private for the same reason as [`CowBTree::scan_prefix_row_values_from`]:
    /// it only answers correctly over a key space whose keys end in an eight-
    /// byte big-endian row id. Test-only parity oracle for the raw-leaf walk.
    #[cfg(test)]
    pub(crate) fn scan_range_row_values_from(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        let mut out = Vec::new();
        if limit == 0 || end.is_some_and(|end| end <= start) {
            return Ok(out);
        }
        let bounds = WalkBounds {
            start,
            end,
            after,
            limit,
        };
        self.walk_row_values(self.read_root(), &bounds, self.has_pending, &mut out)?;
        Ok(out)
    }

    /// The raw-leaf form of [`CowBTree::scan_prefix_row_values_from`] — same
    /// bounds, order, resume and value semantics, but leaf pages are parsed in
    /// place rather than decoded into a cached node. This is the production
    /// path for a table scan; the decoded walk is its parity oracle.
    pub(crate) fn scan_prefix_row_values_raw_from(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        let upper = prefix_upper_bound(prefix);
        self.scan_range_row_values_raw_from(prefix, upper.as_deref(), after, limit)
    }

    /// The raw-leaf form of [`CowBTree::scan_range_row_values_from`].
    pub(crate) fn scan_range_row_values_raw_from(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(RowId, RowBuf)>> {
        let mut out = Vec::new();
        if limit == 0 || end.is_some_and(|end| end <= start) {
            return Ok(out);
        }
        let bounds = WalkBounds {
            start,
            end,
            after,
            limit,
        };
        self.walk_raw_row_values(self.read_root(), &bounds, self.has_pending, &mut out)?;
        Ok(out)
    }

    // -------------------------------------------------------------- internals

    fn write_state(&mut self) -> Result<()> {
        self.write_state_values(self.root, self.next_page_id, self.checkpoint_seq)
    }

    fn write_state_values(&mut self, root: PageId, next: PageId, seq: u64) -> Result<()> {
        let state = encode_state(root, next, seq);
        self.device
            .write(crate::wal::state_offset(self.page_size), &state)?;
        self.device.sync()?;
        Ok(())
    }

    fn begin_txn(&mut self) {
        if !self.has_pending {
            self.pending_root = self.root;
            self.pending_next = self.next_page_id;
            self.has_pending = true;
        }
    }

    /// Adopt a committed state's page counter **without ever moving the
    /// allocator backwards** (AHL-406).
    ///
    /// Every other field of a committed state is adopted wholesale: the root,
    /// the sequence number, the checkpoint sequence. The next-free-page counter
    /// is the one that cannot be, because it is not a description of the past —
    /// it is a promise about the future. The design this tree, its snapshots and
    /// its page cache all rest on is that **a page id names one immutable
    /// sequence of bytes for the lifetime of the file** (`super::cache`, "Why no
    /// invalidation protocol is needed"). Copy-on-write plus a
    /// monotonically-increasing allocator is what makes that true. Handing an id
    /// out a second time makes it false, and the failure is silent: no checksum
    /// fails, no decode fails, the tree simply serves the previous occupant of
    /// the page.
    ///
    /// The committed counter *can* be behind this handle's. It happens when the
    /// state the device reports has gone backwards past commits this handle had
    /// already written pages for — a `sync` that reported success without
    /// reaching the platter, followed by a log region the wrap had already
    /// truncated, which is exactly the schedule AHL-406 reproduces. Without the
    /// `max` below the allocator restarted inside a range of page ids that were
    /// already written and possibly still cached, the next commits grafted
    /// pages from the abandoned timeline onto the recovered one, and the file
    /// recovered to a tree that mixed the two — the state no commit ever
    /// produced.
    ///
    /// Keeping the counter is always safe: a higher `next` reserves page ids
    /// nothing has written, so it can only skip space, never alias it. The
    /// pages the abandoned commits left behind are unreachable from any
    /// committed root and are simply never reused.
    fn adopt_next_page_id(&mut self, committed_next: PageId) {
        self.next_page_id = committed_next.max(self.next_page_id);
    }

    /// Rebase a transaction whose snapshot moved, when its row writes do not
    /// overlap the winner's. The SQL engine's monotonic metadata is adjusted to
    /// the newly reserved commit order; arbitrary metadata changes still use
    /// first-committer-wins and therefore conflict.
    fn rebase_pending(
        &mut self,
        current_root: PageId,
        current_next: PageId,
        current_seq: u64,
    ) -> Result<bool> {
        // Before any read against `current_root`: a committer between this
        // handle's own root and `current_root` may have reused a page, and
        // this handle's cache has not been told yet — see
        // `CowBTree::invalidate_for_reuse`. Doing this after the loop below,
        // which itself reads `current_root`, would be one commit too late.
        self.invalidate_for_reuse();
        for key in self.pending_ops.keys() {
            if self.get_at(self.root, key)? != self.get_at(current_root, key)?
                && !mergeable_metadata_key(key)
            {
                return Ok(false);
            }
        }

        let mut ops = core::mem::take(&mut self.pending_ops);
        merge_monotonic_metadata(self, current_root, &mut ops)?;

        self.dirty.clear();
        self.free_candidates.clear();
        self.freed_this_txn.clear();
        self.consumed_this_txn.clear();
        self.consumed_ever_this_txn.clear();
        self.root = current_root;
        self.adopt_next_page_id(current_next);
        self.next_seq = current_seq + 1;
        self.pending_root = current_root;
        self.pending_next = self.next_page_id;
        self.has_pending = false;
        self.update_watermark(current_seq);
        for (key, value) in ops {
            match value {
                Some(value) => self.put(&key, &value)?,
                None => self.delete(&key)?,
            }
        }
        Ok(true)
    }

    /// A fresh page id: from the free list when reuse is on and a candidate
    /// has cleared both eligibility proofs, otherwise the next never-used id.
    ///
    /// Drawing a candidate here only pops it off the in-memory
    /// `free_candidates` buffer and records `(freed_at, id)` in
    /// `consumed_this_txn` — it does **not** touch `pending_root`/`dirty`
    /// synchronously. `alloc_page` is called from deep inside an in-flight
    /// `insert_into`/`delete_from`/`store_value` recursion, which threads its
    /// own working root through its call stack and only writes it back to
    /// `self.pending_root` when the *outermost* call returns; a reentrant
    /// top-level `self.put`/`self.delete` here (to remove the free-list row
    /// immediately) would update `self.pending_root` out from under that
    /// outer call, which would then overwrite it with a root computed before
    /// the reentrant change — silently losing it. `CowBTree::commit` turns
    /// `consumed_this_txn` into the actual row deletions once every ordinary
    /// `put`/`delete` for this transaction has already returned; see
    /// [`CowBTree::finalize_free_list`].
    fn alloc_page(&mut self) -> PageId {
        if self.reuse_enabled {
            if self.free_candidates.is_empty() {
                // Best-effort: a scan failure here is not this call's to
                // report (`alloc_page` cannot fail), and falling back to the
                // monotonic counter is always correct, only less space-
                // efficient. The next allocation call tries again.
                let _ = self.refill_free_candidates();
            }
            if let Some((freed_at, id)) = self.free_candidates.pop() {
                self.consumed_this_txn.push((freed_at, id));
                self.consumed_ever_this_txn.push((freed_at, id));
                self.pages_reused += 1;
                return id;
            }
        }
        let id = self.pending_next;
        self.pending_next += 1;
        id
    }

    /// Read up to [`FREE_CANDIDATE_BATCH`] reclaimable page ids into
    /// `free_candidates`, offering only ones this handle can prove safe.
    ///
    /// Safety is two separate proofs, both required, and either answering
    /// "unknown" declines every candidate rather than assuming safety:
    ///
    /// * **Durable**, via [`Device::commit_point`] — never this handle's own
    ///   `self.checkpoint_seq`/`self.next_seq`. Those are updated the moment
    ///   this handle *believes* a sync succeeded, and on the fault-injecting
    ///   simulation device that belief can be wrong the exact way AHL-406
    ///   was: a checkpoint's own sync can report success without reaching
    ///   the platter while a *later*, unrelated sync makes some other write
    ///   durable regardless, so trusting in-memory state here would silently
    ///   reopen that bug one level up — a recycled page whose freeing commit
    ///   turns out not to have survived. `commit_point` is the one answer in
    ///   this codebase already built to be trustworthy here: it is `Some`
    ///   only for a device that holds this process's exclusive OS lock
    ///   (`FileDevice::open`) or is otherwise provably single-writer, and
    ///   `None` from every fault-injecting or read-only device — see its doc
    ///   comment. A device that never answers `Some` here — the default —
    ///   simply never has a page reclaimed, which is always safe, only less
    ///   space-efficient.
    /// * **Live**, via [`Device::min_reader_seq`]: no reader this device can
    ///   see is pinned to a root older than the freeing commit. Same rule:
    ///   `None` declines rather than assumes there is no reader to worry
    ///   about.
    ///
    /// The free list is stored oldest-freed-first (`free_list_key`'s field
    /// order), so the scan can stop at the first ineligible row instead of
    /// filtering the whole prefix.
    fn refill_free_candidates(&mut self) -> Result<()> {
        let region = self.device.wal_region() % crate::wal::region_count(self.format_version);
        let Some(point) = self.device.commit_point(region) else {
            return Ok(());
        };
        let Some(min_reader) = self.device.min_reader_seq() else {
            return Ok(());
        };
        let eligible_before = point.seq.min(min_reader);
        let rows = self.scan_prefix_from(FREE_LIST_PREFIX, None, FREE_CANDIDATE_BATCH)?;
        for (key, _) in rows {
            let Some((freed_at, id)) = decode_free_list_key(&key) else {
                continue;
            };
            if freed_at >= eligible_before {
                break;
            }
            // The row's own deletion (from an earlier `alloc_page` call in
            // *this* transaction) is deferred to `CowBTree::finalize_free_list`
            // at commit time, so the scan above still sees it — without this
            // check the same id would be offered, and handed out, twice in
            // one transaction. Checked against `consumed_ever_this_txn`, not
            // `consumed_this_txn`: the latter is a drain queue
            // `finalize_free_list` pops from, so an entry can be *absent*
            // from it while its own `self.delete` is still in flight — and a
            // `refill_free_candidates` call reentered from inside that very
            // delete (rewriting the free-list subtree can need a fresh page)
            // must still see it as spoken for. See the field doc on
            // `consumed_ever_this_txn` for how this was actually reached.
            if self
                .consumed_ever_this_txn
                .iter()
                .any(|&(fa, i)| fa == freed_at && i == id)
            {
                continue;
            }
            self.free_candidates.push((freed_at, id));
        }
        Ok(())
    }

    /// The page id to write this transaction's next version of `source` into.
    ///
    /// Copy-on-write's rule is that a page a *reader* could be looking at is
    /// never overwritten — that is what makes a snapshot a snapshot, and what
    /// [`CowBTree::adopt_next_page_id`] protects (AHL-406). A page this open
    /// transaction allocated itself is not such a page: its id came from
    /// [`CowBTree::alloc_page`], so it is past every committed state's
    /// next-free-page counter; it exists only in `dirty`, so it has never been
    /// on the device; and it is reachable only from `pending_root`, which no
    /// other handle can see. Writing the next version of it back into the same
    /// slot is therefore invisible to everything except this transaction.
    ///
    /// What that saves is real: a statement usually issues several `put`s (the
    /// row, then the engine's row-id and change-version metadata), and without
    /// this each one re-copied the whole root-to-leaf path to fresh ids and
    /// left the previous copy behind as garbage — garbage that was still
    /// written to the data area *and* copied into the commit record, under the
    /// reservation gate, on the way to being unreachable (AHL-468).
    ///
    /// A page that is *not* in `dirty` is a committed page and must be copied,
    /// which is the ordinary path and the one every reader depends on. It is
    /// recorded via [`CowBTree::supersede`] so the free list can eventually
    /// reclaim it, when this handle has opted into the free list at all.
    fn page_slot(&mut self, source: PageId) -> PageId {
        if self.dirty.contains_key(&source) {
            source
        } else {
            self.supersede(source);
            self.alloc_page()
        }
    }

    /// Mark `id` as no longer reachable from the pending transaction's tree.
    ///
    /// If this transaction is what allocated it, nothing outside the
    /// transaction has ever seen it: drop it from `dirty` so it is never
    /// written to the data area or copied into the commit record — the same
    /// reasoning `delete_from`'s leaf-becomes-empty case has always used.
    /// That cleanup happens unconditionally, reuse or not, because it was
    /// already correct before this feature existed.
    ///
    /// Otherwise `id` is a page some committed root — and, until reclaimed,
    /// some possible reader — could still reference. It is recorded in
    /// `freed_this_txn` for [`CowBTree::finalize_free_list`] to persist as
    /// freed at this transaction's commit sequence **only when
    /// `reuse_enabled`** — with it off, this is exactly the no-op it always
    /// was: no bookkeeping row is written, so a database nobody opts in for
    /// stays byte-for-byte what it always produced. That gate is not a
    /// micro-optimisation: unconditionally, this row would show up in a raw
    /// `scan()` of the whole tree, which is exactly what the existing DST
    /// sweeps and several unit tests do and compare against a workload's own
    /// expected contents — writing it for every handle would have made this
    /// change observable (and load-bearing-test-breaking) for every existing
    /// caller, not just ones that asked for it.
    fn supersede(&mut self, id: PageId) {
        if self.dirty.remove(&id).is_some() {
            return;
        }
        if self.reuse_enabled {
            self.freed_this_txn.push(id);
        }
    }

    /// Walk an overflow chain starting at `first`, superseding every page in
    /// it (see [`CowBTree::supersede`]). Used when a value that used to
    /// overflow is replaced or its row deleted — without this, freeing the
    /// leaf entry that pointed at the chain left every page in it as
    /// unrecorded garbage, exactly the shape of leak the free list exists to
    /// close.
    fn free_overflow_chain(&mut self, first: PageId) -> Result<()> {
        // Gated here too, not only inside `supersede`: with reuse off this
        // skips walking the chain at all, so replacing or deleting an
        // overflowing value costs exactly what it always did — no extra
        // reads, on top of writing no extra rows.
        if !self.reuse_enabled {
            return Ok(());
        }
        let mut id = first;
        while id != 0 {
            let (next, _) = self.read_overflow_page(id, true)?;
            self.supersede(id);
            id = next;
        }
        Ok(())
    }

    /// Turn this transaction's free-list bookkeeping into durable tree rows,
    /// as part of the same commit whose sequence number is `seq`.
    ///
    /// Called from [`CowBTree::commit`], strictly after `rebase_pending` has
    /// already settled `self.pending_root` for this transaction's *own*
    /// changes and strictly before the commit record is built — every row
    /// this writes rides the same WAL record, the same sync and the same
    /// crash-atomicity guarantee as the rest of the transaction, because
    /// there is no separate recovery path to get right: a free-list row is
    /// an ordinary row (`crate::index`'s entries use the same trick).
    ///
    /// `freed_this_txn` (pages this transaction superseded) and
    /// `consumed_this_txn` (pages it drew from the free list) can both grow
    /// while this drains them: rewriting a free-list leaf or internal page
    /// can itself supersede an older one, or need a fresh page id this same
    /// commit's own free list can supply. Draining with `pop` rather than a
    /// fixed iteration picks those up; depth is bounded by the free list's
    /// own tree height, the same as any B-tree operation.
    ///
    /// Deleting a consumed candidate's row here — rather than the instant it
    /// is popped in `alloc_page` — is what makes two writers racing for the
    /// same freed id safe without any bespoke locking: the delete is an
    /// ordinary `pending_ops` entry, so if a *different* writer already
    /// consumed the same row first, `CowBTree::rebase_pending`'s ordinary
    /// first-committer-wins comparison sees this transaction's base value for
    /// that key (present) disagree with the winner's (absent) and reports a
    /// conflict — the same protection every other row already gets, not a
    /// free-list-specific case.
    fn finalize_free_list(&mut self, seq: u64) -> Result<()> {
        let mut iterations = 0usize;
        loop {
            let freed = self.freed_this_txn.pop();
            let consumed = self.consumed_this_txn.pop();
            if freed.is_none() && consumed.is_none() {
                break;
            }
            iterations += 1;
            if iterations > 100_000 {
                return Err(Error::Storage(alloc::format!(
                    "finalize_free_list did not converge after {iterations} iterations \
                     (freed_this_txn={}, consumed_this_txn={}) — probable free-list bug",
                    self.freed_this_txn.len(),
                    self.consumed_this_txn.len()
                )));
            }
            if let Some(id) = freed {
                self.put(&free_list_key(seq, id), &[])?;
            }
            if let Some((freed_at, id)) = consumed {
                self.delete(&free_list_key(freed_at, id))?;
            }
        }
        Ok(())
    }

    /// Write the transaction's pages to the data area, one device write per
    /// contiguous run of page ids.
    ///
    /// The same bytes land at the same offsets as writing each page on its own
    /// — this only changes how many calls it takes. That matters because the
    /// calls happen under the reservation gate, where they are serialised
    /// across every writer on the file, and because each one extends the file:
    /// a transaction allocates its page ids consecutively, so what used to be
    /// one extending write per page is normally one for the whole commit
    /// (AHL-468).
    ///
    /// A run is only extended while the pages really are adjacent — ids
    /// consecutive *and* each page exactly `page_size` long, which
    /// [`page::decode`] requires of anything it will later be asked to read.
    /// Anything else starts a new run, so a gap in the ids can never make a
    /// page land on top of its neighbour.
    fn write_dirty_pages(&mut self) -> Result<()> {
        let page_size = self.page_size;
        let format_version = self.format_version;
        let mut run: Vec<u8> = Vec::with_capacity(self.dirty.len() * page_size);
        let mut run_start: PageId = 0;
        let mut run_end: PageId = 0;
        for (&id, bytes) in &self.dirty {
            if bytes.len() != page_size {
                // Not a whole page, so nothing can be adjacent to it. Flush
                // whatever run is pending and write this one on its own.
                if !run.is_empty() {
                    self.device.write(
                        crate::wal::data_offset_for(page_size, format_version, run_start),
                        &run,
                    )?;
                    run.clear();
                }
                self.device.write(
                    crate::wal::data_offset_for(page_size, format_version, id),
                    bytes,
                )?;
                continue;
            }
            if !run.is_empty() && id != run_end {
                self.device.write(
                    crate::wal::data_offset_for(page_size, format_version, run_start),
                    &run,
                )?;
                run.clear();
            }
            if run.is_empty() {
                run_start = id;
            }
            run.extend_from_slice(bytes);
            run_end = id + 1;
        }
        if !run.is_empty() {
            self.device.write(
                crate::wal::data_offset_for(page_size, format_version, run_start),
                &run,
            )?;
        }
        Ok(())
    }

    fn insert_into(&mut self, id: PageId, key: &[u8], value: &[u8]) -> Result<InsertOutcome> {
        if id == 0 {
            let entries = vec![Entry {
                key: Key::Owned(key.to_vec()),
                value: self.store_value(key, value)?,
            }];
            let new_id = self.alloc_page();
            // Every key is owned, so no shared page buffer is indexed here.
            self.dirty
                .insert(new_id, page::encode_leaf(self.page_size, &[], &entries)?);
            return Ok(InsertOutcome::Replaced { id: new_id });
        }

        match self.read_node(id)? {
            Node::Leaf { bytes, mut entries } => {
                match entries.binary_search_by(|e| e.key.resolve(&bytes).cmp(key)) {
                    Ok(i) => {
                        let new_value = self.store_value(key, value)?;
                        let old_value = core::mem::replace(&mut entries[i].value, new_value);
                        if let ValueRef::Overflow { first, .. } = old_value {
                            self.free_overflow_chain(first)?;
                        }
                    }
                    Err(i) => entries.insert(
                        i,
                        Entry {
                            key: Key::Owned(key.to_vec()),
                            value: self.store_value(key, value)?,
                        },
                    ),
                }
                if page::leaf_size(&bytes, &entries) <= self.page_size {
                    let new_id = self.page_slot(id);
                    self.dirty
                        .insert(new_id, page::encode_leaf(self.page_size, &bytes, &entries)?);
                    Ok(InsertOutcome::Replaced { id: new_id })
                } else {
                    let mid = leaf_split_point(&bytes, &entries, self.page_size);
                    let right = entries.split_off(mid);
                    // The promoted separator is copied out of the leaf's bytes:
                    // the parent's page is a different buffer, so the key cannot
                    // stay borrowed across the boundary.
                    let separator = right[0].key.resolve(&bytes).to_vec();
                    let left_id = self.page_slot(id);
                    let right_id = self.alloc_page();
                    self.dirty.insert(
                        left_id,
                        page::encode_leaf(self.page_size, &bytes, &entries)?,
                    );
                    self.dirty
                        .insert(right_id, page::encode_leaf(self.page_size, &bytes, &right)?);
                    Ok(InsertOutcome::Split {
                        left: left_id,
                        right: right_id,
                        separator,
                    })
                }
            }
            Node::Internal {
                bytes,
                leftmost,
                cells,
            } => {
                let idx = child_index(&bytes, &cells, key);
                let child = child_pointer(&bytes, &cells, leftmost, key);
                match self.insert_into(child, key, value)? {
                    InsertOutcome::Replaced { id: new_child } => {
                        let mut new_leftmost = leftmost;
                        let mut new_cells = cells;
                        replace_child(&mut new_cells, &mut new_leftmost, idx, new_child);
                        let new_id = self.page_slot(id);
                        self.dirty.insert(
                            new_id,
                            page::encode_internal(
                                self.page_size,
                                &bytes,
                                new_leftmost,
                                &new_cells,
                            )?,
                        );
                        Ok(InsertOutcome::Replaced { id: new_id })
                    }
                    InsertOutcome::Split {
                        left,
                        right,
                        separator,
                    } => {
                        let mut new_leftmost = leftmost;
                        let mut new_cells = cells;
                        if idx == 0 {
                            new_leftmost = left;
                            new_cells.insert(
                                0,
                                Separator {
                                    key: Key::Owned(separator),
                                    child: right,
                                },
                            );
                        } else {
                            new_cells[idx - 1].child = left;
                            new_cells.insert(
                                idx,
                                Separator {
                                    key: Key::Owned(separator),
                                    child: right,
                                },
                            );
                        }
                        if page::internal_size(&bytes, &new_cells) <= self.page_size {
                            let new_id = self.page_slot(id);
                            self.dirty.insert(
                                new_id,
                                page::encode_internal(
                                    self.page_size,
                                    &bytes,
                                    new_leftmost,
                                    &new_cells,
                                )?,
                            );
                            Ok(InsertOutcome::Replaced { id: new_id })
                        } else {
                            // Split the internal node. With the entry-size
                            // guard in `put`, `new_cells` always has at least
                            // two cells here, so both halves are non-empty.
                            let mid = internal_split_point(&bytes, &new_cells, self.page_size);
                            let right_cells = new_cells.split_off(mid);
                            let promoted = right_cells[0].key.resolve(&bytes).to_vec();
                            let right_leftmost = right_cells[0].child;
                            let right_rest = right_cells[1..].to_vec();
                            let left_id = self.page_slot(id);
                            let right_id = self.alloc_page();
                            self.dirty.insert(
                                left_id,
                                page::encode_internal(
                                    self.page_size,
                                    &bytes,
                                    new_leftmost,
                                    &new_cells,
                                )?,
                            );
                            self.dirty.insert(
                                right_id,
                                page::encode_internal(
                                    self.page_size,
                                    &bytes,
                                    right_leftmost,
                                    &right_rest,
                                )?,
                            );
                            Ok(InsertOutcome::Split {
                                left: left_id,
                                right: right_id,
                                separator: promoted,
                            })
                        }
                    }
                }
            }
        }
    }

    fn delete_from(&mut self, id: PageId, key: &[u8]) -> Result<PageId> {
        if id == 0 {
            return Ok(0);
        }
        match self.read_node(id)? {
            Node::Leaf { bytes, mut entries } => {
                if let Ok(i) = entries.binary_search_by(|e| e.key.resolve(&bytes).cmp(key)) {
                    let removed = entries.remove(i);
                    if let ValueRef::Overflow { first, .. } = removed.value {
                        self.free_overflow_chain(first)?;
                    }
                }
                if entries.is_empty() {
                    // The page is now unreachable. `supersede` drops it from
                    // `dirty` without a free-list row if this transaction is
                    // what allocated it (nothing outside the transaction has
                    // ever seen it, so there is no dead page to write to the
                    // data area or copy into the commit record); otherwise it
                    // records it as freed, which this exact case never used
                    // to do — see `docs/recovery.md`'s "Space reclamation".
                    self.supersede(id);
                    Ok(0)
                } else {
                    let new_id = self.page_slot(id);
                    self.dirty
                        .insert(new_id, page::encode_leaf(self.page_size, &bytes, &entries)?);
                    Ok(new_id)
                }
            }
            Node::Internal {
                bytes,
                leftmost,
                cells,
            } => {
                let idx = child_index(&bytes, &cells, key);
                let child = child_pointer(&bytes, &cells, leftmost, key);
                let new_child = self.delete_from(child, key)?;
                let mut new_leftmost = leftmost;
                let mut new_cells = cells;
                replace_child(&mut new_cells, &mut new_leftmost, idx, new_child);
                let new_id = self.page_slot(id);
                self.dirty.insert(
                    new_id,
                    page::encode_internal(self.page_size, &bytes, new_leftmost, &new_cells)?,
                );
                Ok(new_id)
            }
        }
    }

    /// The root a read sees: the open transaction's working root when there is
    /// one, the committed root otherwise.
    fn read_root(&self) -> PageId {
        if self.has_pending {
            self.pending_root
        } else {
            self.root
        }
    }

    /// An owned node, for the write paths that decode a page in order to
    /// change it. Copy-on-write means the result is about to be superseded by
    /// a freshly allocated page, so it cannot be shared.
    fn read_node(&self, id: PageId) -> Result<Node> {
        Ok((*self.node_at(id, true)?).clone())
    }

    /// Decode page `id`, taking the open transaction's copy when `pending` is
    /// set. A copy-on-write page is only ever written to the data area at
    /// commit, so a read that must see uncommitted work has to look here first.
    ///
    /// The order matters and is load-bearing: an open transaction's dirty page
    /// always wins over the committed cache, because the cached page is the
    /// *previous* version of that data and the transaction must read its own
    /// writes. Dirty pages are never put in the cache — they are not committed
    /// yet, and a conflict throws them away.
    fn node_at(&self, id: PageId, pending: bool) -> Result<Rc<Node>> {
        if pending {
            if let Some(bytes) = self.dirty.get(&id) {
                return Ok(Rc::new(page::decode(self.page_size, bytes)?));
            }
        }
        self.committed_node(id)
    }

    /// A committed page, from the cache when it is resident and from the device
    /// otherwise.
    ///
    /// Only data-area pages are cached: the header, the state block and the WAL
    /// regions are rewritten in place, so a page id that maps into them is read
    /// through every time. `id` 0 is exactly such an id in today's layout — it
    /// is the empty-tree sentinel — and callers already never reach here with
    /// it, but the guard is the enforcement rather than the assumption.
    fn committed_node(&self, id: PageId) -> Result<Rc<Node>> {
        if !cache::data_area_page(self.page_size, self.format_version, id) {
            return Ok(Rc::new(self.read_committed_node(id)?));
        }
        if let Ok(mut cache) = self.cache.try_borrow_mut() {
            if let Some(node) = cache.get(id) {
                return Ok(node);
            }
        }
        let node = Rc::new(self.read_committed_node(id)?);
        if let Ok(mut cache) = self.cache.try_borrow_mut() {
            cache.insert(id, Rc::clone(&node));
        }
        Ok(node)
    }

    fn read_committed_node(&self, id: PageId) -> Result<Node> {
        let offset = crate::wal::data_offset_for(self.page_size, self.format_version, id);
        self.with_page_bytes(offset, page::decode)
    }

    /// Read one page into the reusable scratch buffer and hand it to `f`.
    ///
    /// The buffer is what removes the per-read heap allocation the tree used to
    /// pay on every level of every descent. `try_borrow_mut` rather than
    /// `borrow_mut`: no current path reads a page while already holding the
    /// buffer, and if one ever does it allocates instead of panicking.
    fn with_page_bytes<T>(
        &self,
        offset: usize,
        f: impl FnOnce(usize, &[u8]) -> Result<T>,
    ) -> Result<T> {
        match self.scratch.try_borrow_mut() {
            Ok(mut buf) => {
                if buf.len() != self.page_size {
                    buf.resize(self.page_size, 0);
                }
                self.device.read(offset, &mut buf)?;
                f(self.page_size, &buf)
            }
            Err(_) => {
                let mut buf = vec![0u8; self.page_size];
                self.device.read(offset, &mut buf)?;
                f(self.page_size, &buf)
            }
        }
    }

    /// Read a page's *raw* bytes — the open transaction's copy when `pending` is
    /// set, the committed data area otherwise — and hand them to `f`.
    ///
    /// [`CowBTree::with_page_bytes`] for a page that is not yet decoded: the raw
    /// scan fast path reads the bytes and parses leaf cells in place, so it
    /// needs the bytes, not a cached [`Node`].
    fn with_raw_page<T>(
        &self,
        id: PageId,
        pending: bool,
        f: impl FnOnce(usize, &[u8]) -> Result<T>,
    ) -> Result<T> {
        if pending {
            if let Some(bytes) = self.dirty.get(&id) {
                return f(self.page_size, bytes);
            }
        }
        let offset = crate::wal::data_offset_for(self.page_size, self.format_version, id);
        self.with_page_bytes(offset, f)
    }

    /// Turn a value being written into its leaf representation: inline bytes
    /// when they fit a page with their key, otherwise a freshly allocated
    /// overflow chain.
    ///
    /// Overflow pages are copy-on-write pages like any other: they are
    /// allocated, added to the transaction's dirty set, and reach the data area
    /// and the commit record on [`CowBTree::commit`]. They are never modified in
    /// place, so a committed overflow chain stays readable by a snapshot just as
    /// a leaf does.
    fn store_value(&mut self, key: &[u8], value: &[u8]) -> Result<ValueRef> {
        if page::inline_entry_fits(self.page_size, key, value) {
            return Ok(ValueRef::Owned(Rc::from(value)));
        }
        let payload = page::overflow_payload_size(self.page_size);
        let count = value.len().div_ceil(payload);
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            ids.push(self.alloc_page());
        }
        for (i, chunk) in value.chunks(payload).enumerate() {
            let next = ids.get(i + 1).copied().unwrap_or(0);
            let bytes = page::encode_overflow(self.page_size, next, chunk)?;
            self.dirty.insert(ids[i], bytes);
        }
        Ok(ValueRef::Overflow {
            first: ids[0],
            len: value.len(),
        })
    }

    /// Reassemble a leaf value, following an overflow chain when there is one.
    ///
    /// `pending` decides whether the chain may be resolved out of the open
    /// transaction's dirty pages: a value written in this transaction and large
    /// enough to overflow lives only there until the commit.
    ///
    /// The inline case is a refcount bump, not a byte copy (`AHL-478`): the
    /// bytes are already shared out of the cached page via `Rc<[u8]>`, so
    /// `RowBuf::Shared` just clones the handle. Only the overflow chain still
    /// allocates and copies — reassembling several pages into one
    /// contiguous buffer is not something an `Rc` clone can avoid, and it was
    /// never the common case this file's profile is about (`PERF.md`'s fixed
    /// payload is well under one page).
    fn resolve_value_at(
        &self,
        node_bytes: Option<&Rc<[u8]>>,
        value: &ValueRef,
        pending: bool,
    ) -> Result<RowBuf> {
        match value {
            ValueRef::Inline(range) => {
                // `None` is only ever passed by the raw-leaf scan, whose values
                // are always `Owned` — a borrowed inline value there would be a
                // bug in that path, not a corrupt file.
                let Some(bytes) = node_bytes else {
                    return Err(Error::Corrupt(
                        "borrowed inline value outside a decoded page".to_string(),
                    ));
                };
                Ok(RowBuf::Shared {
                    bytes: Rc::clone(bytes),
                    range: range.clone(),
                })
            }
            ValueRef::Owned(bytes) => Ok(RowBuf::Shared {
                bytes: Rc::clone(bytes),
                range: 0..bytes.len(),
            }),
            ValueRef::Overflow { first, len } => {
                // A value larger than the write-ahead-log region could never
                // have been committed (its record would not fit), so this bound
                // is safe and keeps a corrupt length from forcing a huge
                // allocation.
                let max = crate::wal::max_record_len(self.page_size);
                if *len > max {
                    return Err(Error::Corrupt(alloc::format!(
                        "overflow value length {len} exceeds the write-ahead log capacity {max}"
                    )));
                }
                let mut out = Vec::with_capacity(*len);
                let mut id = *first;
                while out.len() < *len {
                    if id == 0 {
                        return Err(Error::Corrupt(
                            "overflow chain ended before the value was complete".to_string(),
                        ));
                    }
                    let (next, data) = self.read_overflow_page(id, pending)?;
                    let take = (*len - out.len()).min(data.len());
                    out.extend_from_slice(&data[..take]);
                    id = next;
                }
                Ok(RowBuf::Owned(out))
            }
        }
    }

    /// One page of an overflow chain.
    ///
    /// Committed overflow pages are immutable for the same reason nodes are,
    /// but they are deliberately *not* cached: an overflow page is one slice of
    /// one large value, so it is read once per read of that value and caching
    /// it would spend the budget that keeps the upper levels of the tree
    /// resident. It does share the scratch buffer, so it no longer allocates a
    /// page per chain link.
    fn read_overflow_page(&self, id: PageId, pending: bool) -> Result<(PageId, Vec<u8>)> {
        if pending {
            if let Some(bytes) = self.dirty.get(&id) {
                return page::decode_overflow(self.page_size, bytes);
            }
        }
        let offset = crate::wal::data_offset_for(self.page_size, self.format_version, id);
        self.with_page_bytes(offset, |page_size, bytes| {
            page::decode_overflow(page_size, bytes)
        })
    }

    /// Collect the entries under `id` that [`WalkBounds`] admits.
    ///
    /// Pruning is decided from the separators of the node in hand: child slot
    /// `i` spans `[cells[i-1].key, cells[i].key)`, and it is descended only when
    /// that span overlaps the requested range. Each level narrows
    /// independently, so no ancestor bounds have to be threaded down.
    fn walk(
        &self,
        id: PageId,
        bounds: &WalkBounds<'_>,
        pending: bool,
        out: &mut Vec<(Vec<u8>, RowBuf)>,
    ) -> Result<()> {
        if id == 0 || out.len() >= bounds.limit {
            return Ok(());
        }
        // The node is shared, so a matching entry's key is copied out rather
        // than moved out. That is the same allocation the decode used to make
        // per entry — the walk pays it only for the entries it actually keeps.
        let node = self.node_at(id, pending)?;
        match &*node {
            Node::Leaf { entries, .. } => {
                for entry in entries {
                    if out.len() >= bounds.limit {
                        return Ok(());
                    }
                    let key = node.key(&entry.key);
                    if !bounds.admits(key) {
                        continue;
                    }
                    let value = self.resolve_value_at(Some(node.bytes()), &entry.value, pending)?;
                    out.push((key.to_vec(), value));
                }
            }
            Node::Internal {
                leftmost, cells, ..
            } => {
                // Below the first separator, so only its lower edge constrains.
                if cells.is_empty() || bounds.starts_below(node.key(&cells[0].key)) {
                    self.walk(*leftmost, bounds, pending, out)?;
                }
                for (i, separator) in cells.iter().enumerate() {
                    if out.len() >= bounds.limit {
                        return Ok(());
                    }
                    let below_upper = match bounds.end {
                        Some(end) => node.key(&separator.key) < end,
                        None => true,
                    };
                    // The slot spans `[separator.key, next.key)`, so it is worth
                    // descending only when the caller's lower bounds fall inside
                    // it — `start` for the range, `after` for the resume.
                    let above_lower = match cells.get(i + 1) {
                        Some(next) => bounds.starts_below(node.key(&next.key)),
                        None => true,
                    };
                    if below_upper && above_lower {
                        self.walk(separator.child, bounds, pending, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// [`CowBTree::walk`]'s row-id-only sibling, for
    /// [`CowBTree::scan_range_row_ids_from`].
    ///
    /// The internal-node branch is [`CowBTree::walk`]'s, unchanged: pruning is
    /// entirely [`WalkBounds::admits`] and [`WalkBounds::starts_below`], so the
    /// two walks visit exactly the same entries by construction and can only
    /// ever differ in what they do with one once it is admitted. Only the leaf
    /// branch differs — no key clone, no [`CowBTree::resolve_value_at`], just
    /// the row id out of the entry already in hand.
    fn walk_row_ids(
        &self,
        id: PageId,
        bounds: &WalkBounds<'_>,
        pending: bool,
        out: &mut Vec<RowId>,
    ) -> Result<()> {
        if id == 0 || out.len() >= bounds.limit {
            return Ok(());
        }
        let node = self.node_at(id, pending)?;
        match &*node {
            Node::Leaf { entries, .. } => {
                for entry in entries {
                    if out.len() >= bounds.limit {
                        return Ok(());
                    }
                    let key = node.key(&entry.key);
                    if !bounds.admits(key) {
                        continue;
                    }
                    out.push(trailing_row_id(key)?);
                }
            }
            Node::Internal {
                leftmost, cells, ..
            } => {
                if cells.is_empty() || bounds.starts_below(node.key(&cells[0].key)) {
                    self.walk_row_ids(*leftmost, bounds, pending, out)?;
                }
                for (i, separator) in cells.iter().enumerate() {
                    if out.len() >= bounds.limit {
                        return Ok(());
                    }
                    let below_upper = match bounds.end {
                        Some(end) => node.key(&separator.key) < end,
                        None => true,
                    };
                    let above_lower = match cells.get(i + 1) {
                        Some(next) => bounds.starts_below(node.key(&next.key)),
                        None => true,
                    };
                    if below_upper && above_lower {
                        self.walk_row_ids(separator.child, bounds, pending, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// [`CowBTree::walk`]'s row-id-and-value sibling, for the table scan.
    ///
    /// The internal-node branch is [`CowBTree::walk`]'s, unchanged: pruning is
    /// entirely [`WalkBounds`]'s, so the two walks visit exactly the same
    /// entries by construction. The leaf branch differs twice: the key is read
    /// for its row id (`trailing_row_id`, the same eight bytes
    /// [`CowBTree::walk_row_ids`] reads) instead of being cloned into an owned
    /// `Vec<u8>`, and the value is resolved exactly as [`CowBTree::walk`]
    /// resolves it. A table scan decodes the row id out of the key and throws
    /// the rest away, so this walk skips that clone-and-discard.
    ///
    /// Test-only: the raw-leaf walk ([`CowBTree::walk_raw_row_values`]) is the
    /// production path, and this decoded walk is its parity oracle.
    #[cfg(test)]
    fn walk_row_values(
        &self,
        id: PageId,
        bounds: &WalkBounds<'_>,
        pending: bool,
        out: &mut Vec<(RowId, RowBuf)>,
    ) -> Result<()> {
        if id == 0 || out.len() >= bounds.limit {
            return Ok(());
        }
        let node = self.node_at(id, pending)?;
        match &*node {
            Node::Leaf { entries, .. } => {
                for entry in entries {
                    if out.len() >= bounds.limit {
                        return Ok(());
                    }
                    let key = node.key(&entry.key);
                    if !bounds.admits(key) {
                        continue;
                    }
                    let value = self.resolve_value_at(Some(node.bytes()), &entry.value, pending)?;
                    out.push((trailing_row_id(key)?, value));
                }
            }
            Node::Internal {
                leftmost, cells, ..
            } => {
                if cells.is_empty() || bounds.starts_below(node.key(&cells[0].key)) {
                    self.walk_row_values(*leftmost, bounds, pending, out)?;
                }
                for (i, separator) in cells.iter().enumerate() {
                    if out.len() >= bounds.limit {
                        return Ok(());
                    }
                    let below_upper = match bounds.end {
                        Some(end) => node.key(&separator.key) < end,
                        None => true,
                    };
                    let above_lower = match cells.get(i + 1) {
                        Some(next) => bounds.starts_below(node.key(&next.key)),
                        None => true,
                    };
                    if below_upper && above_lower {
                        self.walk_row_values(separator.child, bounds, pending, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// [`CowBTree::walk_row_values`]'s raw-leaf sibling, for the sequential
    /// scan.
    ///
    /// Internal nodes are decoded and navigated exactly as the general walk
    /// does — they are few, and their separators have to be read to descend.
    /// Leaf pages are *not* decoded into a cached [`Node`]: their cells are
    /// parsed with the key borrowed straight off the page bytes
    /// ([`page::scan_leaf_cells`]), so a scan of a leaf allocates no `Rc<Node>`,
    /// no `Vec<Entry>` and no per-cell key `Vec`.
    ///
    /// That is a scan-only win and deliberately not the universal read path: a
    /// point lookup / `get` still decodes once and serves from the cache, where
    /// the owned key is already paid for and re-reading raw bytes would be
    /// strictly worse (`PERF.md`, AHL-493's cache-resident regression).
    fn walk_raw_row_values(
        &self,
        id: PageId,
        bounds: &WalkBounds<'_>,
        pending: bool,
        out: &mut Vec<(RowId, RowBuf)>,
    ) -> Result<()> {
        if id == 0 || out.len() >= bounds.limit {
            return Ok(());
        }
        // Read the raw bytes once and dispatch on the kind byte: a leaf is
        // parsed in place, an internal node is decoded for navigation.
        let internal = self.with_raw_page(id, pending, |page_size, bytes| {
            match bytes[page::OFF_KIND] {
                page::KIND_LEAF => {
                    page::scan_leaf_cells(bytes, page_size, |key, value| {
                        if out.len() >= bounds.limit {
                            return Ok(());
                        }
                        if !bounds.admits(key) {
                            return Ok(());
                        }
                        let value = self.resolve_value_at(None, &value, pending)?;
                        out.push((trailing_row_id(key)?, value));
                        Ok(())
                    })?;
                    Ok(None)
                }
                page::KIND_INTERNAL => Ok(Some(page::decode(page_size, bytes)?)),
                other => Err(Error::Corrupt(alloc::format!("unknown node kind {other}"))),
            }
        })?;

        if let Some(Node::Internal {
            bytes,
            leftmost,
            cells,
        }) = internal
        {
            if cells.is_empty() || bounds.starts_below(cells[0].key.resolve(&bytes)) {
                self.walk_raw_row_values(leftmost, bounds, pending, out)?;
            }
            for (i, separator) in cells.iter().enumerate() {
                if out.len() >= bounds.limit {
                    return Ok(());
                }
                let below_upper = match bounds.end {
                    Some(end) => separator.key.resolve(&bytes) < end,
                    None => true,
                };
                let above_lower = match cells.get(i + 1) {
                    Some(next) => bounds.starts_below(next.key.resolve(&bytes)),
                    None => true,
                };
                if below_upper && above_lower {
                    self.walk_raw_row_values(separator.child, bounds, pending, out)?;
                }
            }
        }
        Ok(())
    }
}

/// The row id the last eight bytes of `key` encode, big-endian.
///
/// Every key [`CowBTree::scan_range_row_ids_from`] is meant to be called over
/// ends this way by construction: a table row's key
/// (`crate::storage::row_key`) and a secondary index entry's key
/// (`crate::index::entry_key`) both close with the row id, big-endian, which
/// is what keeps key order equal to row-id order within one value and is what
/// lets `crate::storage::row_id_from_key` and `crate::index::row_id_from_entry`
/// each recover it the same way at their own layer. This is a third copy of
/// that same eight-byte slice rather than a call to either of them, on
/// purpose: the tree stays the layer that knows nothing about what a key
/// *means* beyond how it orders, so it cannot import a decoder that belongs to
/// one specific encoding above it. `an_index_row_id_walk_agrees_with_the_general_entry_walk`
/// is what keeps this copy from silently drifting from the other two instead
/// of a comment asking nicely.
fn trailing_row_id(key: &[u8]) -> Result<RowId> {
    let bytes = key
        .get(key.len().wrapping_sub(8)..)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .ok_or_else(|| Error::Corrupt("key is too short to carry a row id".to_string()))?;
    Ok(RowId::from_be_bytes(bytes))
}

/// The range one [`CowBTree::scan_range_from`] is walking.
///
/// Held in one struct rather than threaded as four arguments because the walk
/// is recursive and every level asks the same questions of it. A prefix scan
/// arrives here as `[prefix, prefix_upper_bound(prefix))`, an index probe as
/// the two memcomparable keys the planner built, and a resumed streaming batch
/// as either of those plus `after` — so there is one traversal, not three.
struct WalkBounds<'a> {
    /// Inclusive lower bound: no key below this is returned.
    start: &'a [u8],
    /// Exclusive upper bound; `None` runs to the end of the key space.
    end: Option<&'a [u8]>,
    /// Exclusive lower bound: where a resumed scan picks up.
    after: Option<&'a [u8]>,
    /// How many entries the caller asked for.
    limit: usize,
}

impl WalkBounds<'_> {
    /// Whether one leaf entry belongs in the answer.
    fn admits(&self, key: &[u8]) -> bool {
        if key < self.start {
            return false;
        }
        if self.end.is_some_and(|end| key >= end) {
            return false;
        }
        match self.after {
            Some(after) => key > after,
            None => true,
        }
    }

    /// Whether a subtree bounded above by `edge` (exclusive) can still hold a
    /// wanted key — that is, whether *both* lower bounds fall below that edge:
    /// `start` for the range, `after` for the resume.
    fn starts_below(&self, edge: &[u8]) -> bool {
        if self.start >= edge {
            return false;
        }
        match self.after {
            Some(after) => after < edge,
            None => true,
        }
    }
}

/// A committed read's leaf, retained across [`CowBTree::get`] calls so the
/// next lookup can try [`CowBTree::reseek`] before walking from the root. See
/// `reseek`'s doc comment for the soundness argument and its relationship to
/// [`super::cache::PageCache`].
struct ReadCursor {
    /// The committed root `leaf` was reached under. Compared against on every
    /// use; see `reseek`.
    root: PageId,
    /// The leaf a lookup that reseeks successfully will search.
    leaf: PageId,
    /// Inclusive lower bound of the key span `leaf` answers for — the
    /// cumulative bound [`CowBTree::get_from`] tracked while descending to
    /// it, not just its immediate parent's. `None` is unbounded.
    low: Option<Vec<u8>>,
    /// Exclusive upper bound, on the same terms.
    high: Option<Vec<u8>>,
}

impl ReadCursor {
    /// Whether `key` falls inside `leaf`'s span, and so is guaranteed to
    /// resolve on `leaf` — found or not — without walking from the root.
    fn admits(&self, key: &[u8]) -> bool {
        if let Some(low) = &self.low {
            if key < low.as_slice() {
                return false;
            }
        }
        if let Some(high) = &self.high {
            if key >= high.as_slice() {
                return false;
            }
        }
        true
    }
}

/// Clone the separator key `source` names, if any. The one place
/// [`CowBTree::retain_cursor`] actually pays for a key byte copy.
fn bound_key(source: Option<(Rc<Node>, usize)>) -> Option<Vec<u8>> {
    let (node, idx) = source?;
    match &*node {
        Node::Internal { cells, .. } => cells
            .get(idx)
            .map(|cell| cell.key.resolve(node.bytes()).to_vec()),
        // Unreachable: `get_from` only ever records a source while looking at
        // an `Internal` node's own cells.
        Node::Leaf { .. } => None,
    }
}

/// The first key that does *not* start with `prefix`, or `None` when every key
/// at or above `prefix` does (an empty prefix, or one that is all `0xff`).
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    while let Some(last) = upper.pop() {
        if last != u8::MAX {
            upper.push(last + 1);
            return Some(upper);
        }
    }
    None
}

/// The child pointer to follow for `key`, given `cells` and `leftmost`.
fn child_pointer(bytes: &[u8], cells: &[Separator], leftmost: PageId, key: &[u8]) -> PageId {
    let idx = child_index(bytes, cells, key);
    if idx == 0 {
        leftmost
    } else {
        cells[idx - 1].child
    }
}

const NEXT_ROW_ID_META: &[u8] = b"\0next_row_id";
const WRITE_VERSION_META: &[u8] = b"\0write_version";
const CDC_FLOOR_META: &[u8] = b"\0cdc_floor";
const CDC_META_PREFIX: &[u8] = b"\0cdc:";

/// Reserved namespace for free-list bookkeeping rows (Phase 2 item 6): one
/// row per page id currently believed reclaimable, ordinary rows in the same
/// tree under the same discipline [`crate::index`]'s entries use. Byte
/// `\x02` cannot start a real table name (SQL identifiers cannot begin with
/// a control byte) and is disjoint from `\x01idx:` (secondary index entries)
/// and `\0` (this file's own metadata keys), so a free-list row can never be
/// mistaken for, or collide with, either.
const FREE_LIST_PREFIX: &[u8] = b"\x02free\0";

/// How many reclaimable page ids [`CowBTree::refill_free_candidates`] reads
/// ahead at once. Small enough that a refill is a bounded, cheap range scan
/// (the free list is sorted oldest-first, so this is a shallow prefix of the
/// whole thing); large enough that a churn workload is not paying a tree
/// descent for every single page it reuses.
const FREE_CANDIDATE_BATCH: usize = 64;

/// The free-list row key for page `id`, freed at commit sequence `freed_at`.
///
/// Ordering the key by `freed_at` first, then `id`, means a prefix scan of
/// [`FREE_LIST_PREFIX`] visits rows oldest-freed-first — not load-bearing for
/// correctness (any reclaimable id is as good as any other), but it means
/// [`CowBTree::refill_free_candidates`] naturally offers the pages that have
/// had the longest to clear a liveness check before newer ones.
fn free_list_key(freed_at: u64, id: PageId) -> Vec<u8> {
    let mut key = Vec::with_capacity(FREE_LIST_PREFIX.len() + 16);
    key.extend_from_slice(FREE_LIST_PREFIX);
    key.extend_from_slice(&freed_at.to_be_bytes());
    key.extend_from_slice(&id.to_be_bytes());
    key
}

/// Decode a [`free_list_key`] back into `(freed_at, id)`. `None` for
/// anything that is not exactly the shape this module writes — defensive
/// against a corrupt or foreign row rather than trusting the prefix alone.
fn decode_free_list_key(key: &[u8]) -> Option<(u64, PageId)> {
    let rest = key.strip_prefix(FREE_LIST_PREFIX)?;
    if rest.len() != 16 {
        return None;
    }
    let freed_at = u64::from_be_bytes(rest[..8].try_into().ok()?);
    let id = PageId::from_be_bytes(rest[8..].try_into().ok()?);
    Some((freed_at, id))
}

fn mergeable_metadata_key(key: &[u8]) -> bool {
    key == NEXT_ROW_ID_META
        || key == WRITE_VERSION_META
        || key == CDC_FLOOR_META
        || key.starts_with(CDC_META_PREFIX)
}

fn merge_monotonic_metadata<D: Device>(
    tree: &CowBTree<D>,
    current_root: PageId,
    ops: &mut BTreeMap<Vec<u8>, Option<Vec<u8>>>,
) -> Result<()> {
    merge_max_counter(tree, current_root, ops, NEXT_ROW_ID_META)?;
    merge_max_counter(tree, current_root, ops, CDC_FLOOR_META)?;

    let Some(pending_version) = ops
        .get(WRITE_VERSION_META)
        .and_then(Option::as_deref)
        .map(decode_counter)
        .transpose()?
    else {
        return Ok(());
    };
    let current_version = tree
        .get_at(current_root, WRITE_VERSION_META)?
        .as_deref()
        .map(decode_counter)
        .transpose()?
        .unwrap_or(0);
    let rebased_version = current_version + 1;
    ops.insert(
        WRITE_VERSION_META.to_vec(),
        Some(rebased_version.to_le_bytes().to_vec()),
    );

    let old_cdc_key = alloc::format!("\0cdc:{pending_version:016x}").into_bytes();
    if let Some(record) = ops.remove(&old_cdc_key) {
        let new_cdc_key = alloc::format!("\0cdc:{rebased_version:016x}").into_bytes();
        ops.insert(new_cdc_key, record);
    }
    Ok(())
}

fn merge_max_counter<D: Device>(
    tree: &CowBTree<D>,
    current_root: PageId,
    ops: &mut BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    key: &[u8],
) -> Result<()> {
    let Some(pending) = ops
        .get(key)
        .and_then(Option::as_deref)
        .map(decode_counter)
        .transpose()?
    else {
        return Ok(());
    };
    let current = tree
        .get_at(current_root, key)?
        .as_deref()
        .map(decode_counter)
        .transpose()?
        .unwrap_or(0);
    ops.insert(
        key.to_vec(),
        Some(pending.max(current).to_le_bytes().to_vec()),
    );
    Ok(())
}

fn decode_counter(bytes: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = bytes
        .try_into()
        .map_err(|_| Error::Corrupt("monotonic metadata counter is not eight bytes".to_string()))?;
    Ok(u64::from_le_bytes(bytes))
}

/// The child *index* (`0..=cells.len()`) that `key` belongs in. Index 0 is the
/// leftmost child; index `i` (`1..=cells.len()`) is `cells[i-1].child`.
///
/// A separator is the smallest key of its (right) child's subtree, so the left
/// neighbour holds strictly smaller keys. The child for `key` is therefore the
/// one whose separator is the last separator `<= key`; the leftmost child is
/// used when `key` is smaller than every separator.
fn child_index(bytes: &[u8], cells: &[Separator], key: &[u8]) -> usize {
    cells.partition_point(|c| c.key.resolve(bytes) <= key)
}

/// Point child `idx` at `new_child`. Index 0 is the leftmost pointer.
fn replace_child(cells: &mut [Separator], leftmost: &mut PageId, idx: usize, new_child: PageId) {
    if idx == 0 {
        *leftmost = new_child;
    } else {
        cells[idx - 1].child = new_child;
    }
}

/// Whether a key alone fits both a leaf cell (in its worst case, an overflow
/// pointer) and an internal separator, so splitting always terminates. Values
/// never factor into this anymore: one that does not fit inline spills to an
/// overflow chain, which keeps the leaf cell small no matter how large the
/// value is.
///
/// The ceiling is half a page for the same reason as
/// [`page::inline_entry_fits`]: every entry must fit in half a page so that a
/// split always produces two fitting halves.
fn key_fits(page_size: usize, key: &[u8]) -> bool {
    // HEADER_SIZE + SLOT_SIZE for the page overhead, then the cell sizes.
    let leaf = 16 + 2 + 2 + key.len() + 1 + 16;
    let internal = 16 + 2 + 2 + key.len() + 8;
    leaf <= page_size / 2 && internal <= page_size / 2
}

/// The split point for an overfull leaf: the largest number of leading entries
/// that still fit a page. Splitting here packs the left half as full as
/// possible, which keeps the right half small enough to fit — important when
/// entries have very different sizes (e.g. small metadata rows next to large
/// rows carrying a vector). Every entry fits alone — either inline or as an
/// overflow pointer (see [`key_fits`]) — so the result is always at least one.
fn leaf_split_point(bytes: &[u8], entries: &[Entry], page_size: usize) -> usize {
    let mut split = 1;
    while split < entries.len() && page::leaf_size(bytes, &entries[..split]) <= page_size {
        split += 1;
    }
    split - 1
}

/// The split point for an overfull internal node: the largest number of leading
/// separators that still fit a page.
fn internal_split_point(bytes: &[u8], cells: &[Separator], page_size: usize) -> usize {
    let mut split = 1;
    while split < cells.len() && page::internal_size(bytes, &cells[..split]) <= page_size {
        split += 1;
    }
    split - 1
}

fn encode_header(page_size: usize) -> Vec<u8> {
    encode_header_with_version(page_size, FORMAT_VERSION)
}

/// Encode a header stamped with an explicit version, for the format-version
/// mismatch tests.
fn encode_header_with_version(page_size: usize, version: u32) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_LEN];
    buf[..8].copy_from_slice(MAGIC);
    buf[H_PAGE_SIZE..H_PAGE_SIZE + 4].copy_from_slice(&(page_size as u32).to_le_bytes());
    buf[H_VERSION..H_VERSION + 4].copy_from_slice(&version.to_le_bytes());
    let checksum = crate::checksum::fnv1a(&buf[..H_CHECKSUM]);
    buf[H_CHECKSUM..H_CHECKSUM + 8].copy_from_slice(&checksum.to_le_bytes());
    buf
}

/// Parse and validate the fixed-size file header, returning the page size and
/// format version it declares.
///
/// This is how a device learns the on-disk layout: `FileDevice` (in the
/// `inlaysql` crate) observes the header the tree reads or writes and uses
/// this to derive where the immutable data area begins, so it can cache raw
/// data pages without ever caching the header, the state block or a WAL
/// region, all of which are rewritten in place. See
/// [`crate::btree::cache::data_area_page`] for the same boundary in decoded
/// form.
pub fn parse_header(bytes: &[u8]) -> Result<(usize, u32)> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::Corrupt("header is truncated".to_string()));
    }
    if &bytes[..8] != MAGIC {
        return Err(Error::Corrupt("not an InlaySQL database".to_string()));
    }
    if crate::checksum::fnv1a(&bytes[..H_CHECKSUM]) != read_u64(bytes, H_CHECKSUM) {
        return Err(Error::Corrupt(
            "header checksum mismatch (torn write?)".to_string(),
        ));
    }
    let page_size = read_u32(bytes, H_PAGE_SIZE) as usize;
    let version = read_u32(bytes, H_VERSION);
    // A version mismatch is not corruption — the file is either from a newer
    // binary or from an older one, and the error says which. See
    // `docs/recovery.md` for the written policy.
    if version > FORMAT_VERSION {
        return Err(Error::FormatVersion(alloc::format!(
            "file format version {version} is newer than this build supports ({FORMAT_VERSION}); \
             upgrade InlaySQL"
        )));
    }
    if version < MIN_READABLE_FORMAT_VERSION {
        return Err(Error::FormatVersion(alloc::format!(
            "file format version {version} is older than this build supports \
             ({MIN_READABLE_FORMAT_VERSION}..={FORMAT_VERSION}); \
             recreate the database"
        )));
    }
    if page_size < page::MIN_PAGE_SIZE {
        return Err(Error::Corrupt(alloc::format!(
            "page size {page_size} below the minimum"
        )));
    }
    Ok((page_size, version))
}

fn encode_state(root: PageId, next: PageId, checkpoint_seq: u64) -> Vec<u8> {
    let mut buf = vec![0u8; STATE_LEN];
    buf[S_ROOT..S_ROOT + 8].copy_from_slice(&root.to_le_bytes());
    buf[S_NEXT..S_NEXT + 8].copy_from_slice(&next.to_le_bytes());
    buf[S_SEQ..S_SEQ + 8].copy_from_slice(&checkpoint_seq.to_le_bytes());
    let checksum = crate::checksum::fnv1a(&buf[..S_CHECKSUM]);
    buf[S_CHECKSUM..S_CHECKSUM + 8].copy_from_slice(&checksum.to_le_bytes());
    buf
}

/// The decoded state block. `None` means it was torn or unreadable.
fn parse_state(bytes: &[u8]) -> Option<State> {
    if bytes.len() < STATE_LEN {
        return None;
    }
    if crate::checksum::fnv1a(&bytes[..S_CHECKSUM]) != read_u64(bytes, S_CHECKSUM) {
        return None;
    }
    Some(State {
        root: read_u64(bytes, S_ROOT),
        next: read_u64(bytes, S_NEXT),
        checkpoint_seq: read_u64(bytes, S_SEQ),
    })
}

/// A decoded state block.
struct State {
    root: PageId,
    next: PageId,
    checkpoint_seq: u64,
}

/// Read and decode the state block, returning `None` if it is torn.
fn read_state<D: Device>(device: &D, page_size: usize) -> Result<Option<State>> {
    let mut bytes = vec![0u8; STATE_LEN];
    device.read(crate::wal::state_offset(page_size), &mut bytes)?;
    Ok(parse_state(&bytes))
}

/// Read the committed state — root, next free page and checkpoint sequence —
/// from the device without mutating it. Returns the newest log record when the
/// state block is torn or behind (so recovery can replay its pages), or `None`
/// when the state block alone is authoritative.
fn read_committed_state<D: Device>(
    device: &D,
    page_size: usize,
    format_version: u32,
) -> Result<(PageId, PageId, u64, Vec<crate::wal::WalRecord>)> {
    let state = read_state(device, page_size)?;
    let records = crate::wal::scan_all(device, page_size, format_version)?;

    if let Some(state) = state {
        let mut root = state.root;
        let mut next = state.next;
        let mut seq = state.checkpoint_seq;
        let mut replay = Vec::new();
        for record in records
            .into_iter()
            .filter(|record| record.seq > state.checkpoint_seq)
        {
            let follows = record.seq == seq + 1
                && (format_version < crate::wal::MULTI_REGION_FORMAT_VERSION
                    || (record.prev_seq == seq && record.prev_root == root));
            if !follows {
                break;
            }
            root = record.root;
            next = record.next;
            seq = record.seq;
            replay.push(record);
        }
        return Ok((root, next, seq, replay));
    }

    // A torn state block can still be recovered from a self-contained WAL
    // chain. A region may already have been reused after an older checkpoint,
    // so the first surviving record is allowed to name a predecessor that is
    // no longer in the log; every subsequent record must link exactly.
    let mut best: Vec<crate::wal::WalRecord> = Vec::new();
    let mut chain: Vec<crate::wal::WalRecord> = Vec::new();
    for record in records {
        let follows = chain.last().is_some_and(|previous| {
            record.seq == previous.seq + 1
                && (format_version < crate::wal::MULTI_REGION_FORMAT_VERSION
                    || (record.prev_seq == previous.seq && record.prev_root == previous.root))
        });
        if !follows {
            if chain
                .last()
                .is_some_and(|last| best.last().is_none_or(|winner| last.seq > winner.seq))
            {
                best = chain;
            }
            chain = Vec::new();
        }
        chain.push(record);
    }
    if chain
        .last()
        .is_some_and(|last| best.last().is_none_or(|winner| last.seq > winner.seq))
    {
        best = chain;
    }
    let newest = best.last().cloned().ok_or_else(|| {
        Error::Corrupt(
            "no recoverable state: the state block is torn and every WAL region is empty"
                .to_string(),
        )
    })?;
    Ok((newest.root, newest.next, newest.seq, best))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(arr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::disk::Fault;
    use crate::sim::{FaultSchedule, SimDisk, Simulator};
    use alloc::format;
    use alloc::rc::Rc;
    use alloc::string::String;
    use core::cell::{Cell, RefCell};

    const PAGE: usize = 256;
    const CAPACITY: usize = 8 << 20;

    fn disk() -> SimDisk {
        SimDisk::with_block_size(512, CAPACITY)
    }

    fn reopen(db: &CowBTree<SimDisk>) -> CowBTree<SimDisk> {
        let image = db.device().durable().to_vec();
        CowBTree::open(SimDisk::with_image(512, &image)).unwrap()
    }

    /// A disk that counts the reads made of it and can be told whether to
    /// report a [`Device::commit_generation`].
    ///
    /// The read counter is what makes "this cost no I/O" an assertion rather
    /// than a claim, and the switch is what lets one test pin the fast path and
    /// the next pin that a device answering `None` still takes the full scan —
    /// which is the property the whole deterministic simulation rests on.
    struct CountingDisk {
        disk: SimDisk,
        reads: Cell<usize>,
        generation: Cell<u64>,
        counts_commits: bool,
    }

    impl CountingDisk {
        fn new(counts_commits: bool) -> Self {
            Self {
                disk: SimDisk::with_block_size(512, CAPACITY),
                reads: Cell::new(0),
                generation: Cell::new(0),
                counts_commits,
            }
        }
    }

    impl Device for CountingDisk {
        fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
            self.reads.set(self.reads.get() + 1);
            Device::read(&self.disk, offset, buf)
        }

        fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
            Device::write(&mut self.disk, offset, data)
        }

        fn sync(&mut self) -> Result<()> {
            Device::sync(&mut self.disk)
        }

        fn end_commit(&self) -> Option<u64> {
            if !self.counts_commits {
                return None;
            }
            let generation = self.generation.get() + 1;
            self.generation.set(generation);
            Some(generation)
        }

        fn commit_generation(&self) -> Option<u64> {
            self.counts_commits.then(|| self.generation.get())
        }
    }

    /// Reads made since the last call, so an assertion reads as "this operation
    /// touched the device N times".
    fn reads_since(tree: &CowBTree<Rc<RefCell<CountingDisk>>>) -> usize {
        let device = tree.device().borrow();
        let reads = device.reads.get();
        device.reads.set(0);
        reads
    }

    #[test]
    fn an_unchanged_snapshot_is_refreshed_without_touching_the_device() {
        let device = Rc::new(RefCell::new(CountingDisk::new(true)));
        let mut tree = CowBTree::create(device, PAGE).unwrap();
        tree.put(b"k", b"v").unwrap();
        tree.commit().unwrap();

        // Nothing has been committed since this handle's own commit, so the
        // question is answered from the generation alone: no state block, no
        // log scan, no reads at all. This is the whole of AHL-403.
        reads_since(&tree);
        assert!(!tree.refresh().unwrap());
        assert_eq!(
            reads_since(&tree),
            0,
            "an unchanged snapshot must cost no I/O"
        );

        // And it stays free — a handle between statements asks constantly.
        for _ in 0..10 {
            assert!(!tree.refresh().unwrap());
        }
        assert_eq!(reads_since(&tree), 0);
    }

    #[test]
    fn a_device_that_cannot_count_commits_still_scans_the_log() {
        // `None` means "assume something changed", which is what keeps the
        // simulated disk — and therefore every DST seed — on the real path.
        let device = Rc::new(RefCell::new(CountingDisk::new(false)));
        let mut tree = CowBTree::create(device, PAGE).unwrap();
        tree.put(b"k", b"v").unwrap();
        tree.commit().unwrap();

        reads_since(&tree);
        assert!(!tree.refresh().unwrap());
        assert!(
            reads_since(&tree) > 0,
            "a device that cannot report a generation must still be read"
        );
    }

    /// A disk that caches a [`CommitPoint`] the way a real file device does —
    /// and re-derives it from the bytes on every single read, asserting the two
    /// agree.
    ///
    /// This is the assertion the shortcut in [`CowBTree::commit`] needs and the
    /// deterministic sweeps cannot make. `SimDisk` answers `None` to
    /// [`Device::commit_point`] on purpose (a simulated fault rolls the
    /// *readable* image back to the durable one, which a real file cannot do),
    /// so the sweeps keep exercising the derivation. That leaves the question
    /// of whether what the tree *publishes* is what the file would say — and a
    /// wrong answer there is not an error, it is a commit built on a root
    /// nobody wrote. Here the two are compared on every commit, checkpoint and
    /// refresh, including across a log-region wrap.
    struct VerifyingDisk {
        disk: SimDisk,
        state: Cell<Option<(PageId, PageId, u64)>>,
        append: Cell<Option<usize>>,
        generation: Cell<u64>,
        /// How many times a cached answer was actually served and checked, so
        /// the test can prove it was exercising the fast path at all.
        checks: Cell<usize>,
    }

    impl VerifyingDisk {
        fn new() -> Self {
            Self {
                disk: SimDisk::with_block_size(512, CAPACITY),
                state: Cell::new(None),
                append: Cell::new(None),
                generation: Cell::new(0),
                checks: Cell::new(0),
            }
        }
    }

    impl Device for VerifyingDisk {
        fn read(&self, offset: usize, buf: &mut [u8]) -> Result<()> {
            Device::read(&self.disk, offset, buf)
        }

        fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
            Device::write(&mut self.disk, offset, data)
        }

        fn sync(&mut self) -> Result<()> {
            Device::sync(&mut self.disk)
        }

        fn end_commit(&self) -> Option<u64> {
            let generation = self.generation.get() + 1;
            self.generation.set(generation);
            Some(generation)
        }

        fn commit_generation(&self) -> Option<u64> {
            Some(self.generation.get())
        }

        fn commit_point(&self, region: usize) -> Option<CommitPoint> {
            let (root, next, seq) = self.state.get()?;
            let append_offset = self.append.get()?;
            let (disk_root, disk_next, disk_seq, _) =
                read_committed_state(&self.disk, PAGE, FORMAT_VERSION).unwrap();
            let disk_append = crate::wal::scan_region(&self.disk, PAGE, FORMAT_VERSION, region)
                .unwrap()
                .append_offset;
            assert_eq!(
                (root, next, seq, append_offset),
                (disk_root, disk_next, disk_seq, disk_append),
                "the cached commit point must be exactly what reading the file derives"
            );
            self.checks.set(self.checks.get() + 1);
            Some(CommitPoint {
                root,
                next,
                seq,
                append_offset,
            })
        }

        fn set_commit_point(&self, _region: usize, point: Option<CommitPoint>) {
            match point {
                Some(point) => {
                    self.state.set(Some((point.root, point.next, point.seq)));
                    self.append.set(Some(point.append_offset));
                }
                None => {
                    self.state.set(None);
                    self.append.set(None);
                }
            }
        }
    }

    #[test]
    fn a_cached_commit_point_is_always_what_reading_the_file_would_derive() {
        let device = Rc::new(RefCell::new(VerifyingDisk::new()));
        let mut a = CowBTree::create(device.clone(), PAGE).unwrap();
        let mut b = CowBTree::open(device.clone()).unwrap();

        // Long enough, and fat enough per record, to wrap the log region
        // several times at PAGE = 256 — the one place a cached append offset
        // stops being true — with explicit checkpoints and deletes mixed in.
        let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for round in 0..300u64 {
            let writer = if round % 2 == 0 { &mut a } else { &mut b };
            let key = format!("k{round:04}").into_bytes();
            let value = vec![b'v'; 200];
            writer.put(&key, &value).unwrap();
            // A second put in the same transaction, so the commit exercises
            // the reused page slots as well as the cached reservation.
            writer.put(b"counter", &round.to_le_bytes()).unwrap();
            assert_eq!(writer.commit().unwrap(), CommitOutcome::Committed);
            expected.insert(key, value);
            expected.insert(b"counter".to_vec(), round.to_le_bytes().to_vec());

            if round % 17 == 0 {
                let stale = format!("k{:04}", round / 2).into_bytes();
                writer.delete(&stale).unwrap();
                assert_eq!(writer.commit().unwrap(), CommitOutcome::Committed);
                expected.remove(&stale);
            }
            if round % 41 == 0 {
                writer.checkpoint().unwrap();
            }
            a.refresh().unwrap();
            b.refresh().unwrap();
        }

        assert!(
            device.borrow().checks.get() > 300,
            "the test must have served cached answers, not just derived ones"
        );

        // And the tree the cached path built is the tree it says it built,
        // read back through a handle that derives everything from the bytes.
        let image = device.borrow().disk.durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        let recovered: BTreeMap<Vec<u8>, Vec<u8>> = reopened
            .scan()
            .unwrap()
            .into_iter()
            .map(|(key, value)| (key, value.into_vec()))
            .collect();
        assert_eq!(recovered, expected);
    }

    #[test]
    fn a_refresh_still_adopts_another_handles_commit() {
        let device = Rc::new(RefCell::new(CountingDisk::new(true)));
        let mut writer = CowBTree::create(device.clone(), PAGE).unwrap();
        writer.put(b"first", b"1").unwrap();
        writer.commit().unwrap();

        let mut reader = CowBTree::open(device).unwrap();
        assert_eq!(reader.get(b"second").unwrap(), None);
        // The reader is current, so this is the free answer.
        reads_since(&reader);
        assert!(!reader.refresh().unwrap());
        assert_eq!(reads_since(&reader), 0);

        writer.put(b"second", b"2").unwrap();
        writer.commit().unwrap();

        // The generation moved, so the reader pays for the scan — and adopts.
        assert!(reader.refresh().unwrap());
        assert!(reads_since(&reader) > 0);
        assert_eq!(
            reader.get(b"second").unwrap(),
            Some(RowBuf::Owned(b"2".to_vec()))
        );
        assert_eq!(reader.root(), writer.root());

        // Having caught up, it is back on the free path. (Discard the reads the
        // assertions above made walking the tree.)
        reads_since(&reader);
        assert!(!reader.refresh().unwrap());
        assert_eq!(reads_since(&reader), 0);
    }

    #[test]
    fn an_open_transaction_is_never_refreshed_out_from_under_itself() {
        let device = Rc::new(RefCell::new(CountingDisk::new(true)));
        let mut writer = CowBTree::create(device.clone(), PAGE).unwrap();
        writer.put(b"first", b"1").unwrap();
        writer.commit().unwrap();

        let mut reader = CowBTree::open(device).unwrap();
        reader.put(b"pending", b"x").unwrap();

        writer.put(b"second", b"2").unwrap();
        writer.commit().unwrap();

        // The generation moved, but this handle has buffered writes rooted at
        // the snapshot they were built against: refuse, and leave the recorded
        // generation alone so the refresh after the transaction still scans.
        assert!(!reader.refresh().unwrap());
        assert_eq!(reader.get(b"second").unwrap(), None);

        reader.rollback();
        assert!(reader.refresh().unwrap());
        assert_eq!(
            reader.get(b"second").unwrap(),
            Some(RowBuf::Owned(b"2".to_vec()))
        );
    }

    #[test]
    fn create_put_commit_get() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"k", b"v").unwrap();
        db.commit().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(b"v".to_vec())));
    }

    #[test]
    fn open_or_create_creates_a_fresh_device_and_opens_an_existing_one() {
        // A device with nothing on it: created.
        let mut db = CowBTree::open_or_create(disk(), PAGE).unwrap();
        db.put(b"k", b"v").unwrap();
        db.commit().unwrap();
        let image = db.device().durable().to_vec();

        // The same bytes again: opened, not overwritten.
        let db = CowBTree::open_or_create(SimDisk::with_image(512, &image), PAGE).unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(b"v".to_vec())));
    }

    #[test]
    fn open_or_create_does_not_overwrite_a_database_with_a_different_page_size() {
        // The page size lives in the header, so an existing database keeps its
        // own — the caller's argument only applies when creating.
        let mut db = CowBTree::open_or_create(disk(), 512).unwrap();
        db.put(b"k", b"v").unwrap();
        db.commit().unwrap();
        let image = db.device().durable().to_vec();

        let db = CowBTree::open_or_create(SimDisk::with_image(512, &image), PAGE).unwrap();
        assert_eq!(db.page_size, 512);
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(b"v".to_vec())));
    }

    #[test]
    fn a_writer_reads_its_own_uncommitted_writes() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"k", b"v").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(b"v".to_vec())));
        db.delete(b"k").unwrap();
        assert_eq!(db.get(b"k").unwrap(), None);
    }

    #[test]
    fn a_pinned_committed_root_does_not_see_the_open_transaction() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"k", b"1").unwrap();
        db.commit().unwrap();
        let snapshot = db.root();

        db.put(b"k", b"2").unwrap();
        db.put(b"later", b"x").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(b"2".to_vec())));
        assert_eq!(
            db.get_at(snapshot, b"k").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(db.get_at(snapshot, b"later").unwrap(), None);
    }

    #[test]
    fn a_rollback_takes_back_what_the_writer_could_see() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"k", b"v").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(b"v".to_vec())));
        db.rollback();
        assert_eq!(db.get(b"k").unwrap(), None);
        assert!(db.scan().unwrap().is_empty());
    }

    #[test]
    fn an_uncommitted_overflow_value_reads_back_whole() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        let big = vec![7u8; PAGE * 3];
        db.put(b"big", &big).unwrap();
        // The chain exists only in the transaction's dirty pages until commit.
        assert_eq!(db.get(b"big").unwrap(), Some(RowBuf::Owned(big.clone())));
        assert_eq!(
            db.scan().unwrap(),
            vec![(b"big".to_vec(), RowBuf::Owned(big))]
        );
    }

    #[test]
    fn a_prefix_scan_returns_only_the_prefix_and_skips_the_rest() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        // Enough keys to force several levels, so pruning has something to skip.
        for i in 0..400u32 {
            for table in ["a", "b", "c"] {
                let mut key = table.as_bytes().to_vec();
                key.push(0);
                key.extend_from_slice(&i.to_be_bytes());
                db.put(&key, &i.to_le_bytes()).unwrap();
            }
            // The log region bounds one transaction, so commit as we go.
            db.commit().unwrap();
        }

        let rows = db.scan_prefix(b"b\0").unwrap();
        assert_eq!(rows.len(), 400);
        assert!(rows.iter().all(|(key, _)| key.starts_with(b"b\0")));
        // Key order within the prefix is preserved.
        let ids: Vec<u32> = rows
            .iter()
            .map(|(key, _)| u32::from_be_bytes(key[2..].try_into().unwrap()))
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        // An unbounded scan still sees everything.
        assert_eq!(db.scan().unwrap().len(), 1200);
        // A prefix nothing starts with is empty, not everything.
        assert!(db.scan_prefix(b"z\0").unwrap().is_empty());
    }

    #[test]
    fn a_prefix_scan_sees_the_open_transaction() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"t\0a", b"1").unwrap();
        db.commit().unwrap();
        db.put(b"t\0b", b"2").unwrap();
        db.put(b"u\0a", b"3").unwrap();

        let rows = db.scan_prefix(b"t\0").unwrap();
        assert_eq!(
            rows,
            vec![
                (b"t\0a".to_vec(), RowBuf::Owned(b"1".to_vec())),
                (b"t\0b".to_vec(), RowBuf::Owned(b"2".to_vec())),
            ]
        );
    }

    /// The prefix scan, the index range probe and the batched streaming scan
    /// are one traversal, so they have to agree — and the combination none of
    /// them used alone (a *range* resumed with `after`, under a `limit`) has to
    /// return each key exactly once, in order.
    #[test]
    fn one_walk_answers_the_prefix_the_range_and_the_resume_alike() {
        let key = |i: u32| {
            let mut key = b"t\0".to_vec();
            key.extend_from_slice(&i.to_be_bytes());
            key
        };
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        for i in 0..400u32 {
            db.put(&key(i), &i.to_le_bytes()).unwrap();
            // Another table on each side, so a walk that ignored its bounds
            // would be visibly wrong rather than merely slow.
            db.put(&[b"s\0".to_vec(), i.to_be_bytes().to_vec()].concat(), b"x")
                .unwrap();
            db.put(&[b"u\0".to_vec(), i.to_be_bytes().to_vec()].concat(), b"x")
                .unwrap();
            db.commit().unwrap();
        }

        // Every entry point is the same walk over the same keys.
        let by_prefix = db.scan_prefix(b"t\0").unwrap();
        assert_eq!(by_prefix.len(), 400);
        assert_eq!(by_prefix, db.scan_range(b"t\0", Some(b"t\x01")).unwrap());
        assert_eq!(
            by_prefix,
            db.scan_prefix_from(b"t\0", None, usize::MAX).unwrap()
        );
        assert_eq!(
            by_prefix,
            db.scan_range_from(b"t\0", Some(b"t\x01"), None, usize::MAX)
                .unwrap()
        );

        // A range narrower than the prefix — what an index probe asks for —
        // pulled through in batches of seven from a resume token, which is what
        // a streaming scan asks for. Together they must reproduce the range
        // exactly: no key repeated, none skipped, none from outside it.
        let (start, end) = (key(100), key(250));
        let expected = db.scan_range(&start, Some(&end)).unwrap();
        assert_eq!(expected.len(), 150);
        let mut resumed = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let batch = db
                .scan_range_from(&start, Some(&end), after.as_deref(), 7)
                .unwrap();
            if batch.is_empty() {
                break;
            }
            after = Some(batch[batch.len() - 1].0.clone());
            resumed.extend(batch);
        }
        assert_eq!(resumed, expected);

        // The limit is a ceiling, not a target, and it is honoured from the
        // range's own start rather than the prefix's.
        assert_eq!(
            db.scan_range_from(&start, Some(&end), None, 3)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            db.scan_range_from(&start, Some(&end), None, 3).unwrap()[0].0,
            start
        );

        // Degenerate bounds are empty rather than everything.
        assert!(db.scan_range(&end, Some(&start)).unwrap().is_empty());
        assert!(db.scan_range(&start, Some(&start)).unwrap().is_empty());
        assert!(db
            .scan_range_from(&start, Some(&end), None, 0)
            .unwrap()
            .is_empty());
        assert!(db
            .scan_range_from(&start, Some(&end), Some(&key(9999)), usize::MAX)
            .unwrap()
            .is_empty());
    }

    /// `AHL-479`'s fast path against the walk it replaces: over the same
    /// range, [`CowBTree::scan_range_row_ids_from`] must return exactly the
    /// row ids [`CowBTree::scan_range_from`] plus the ordinary trailing-eight-
    /// bytes decode does, in the same order — the discipline `AGENTS.md` asks
    /// of a new fast path next to the slow one it sits beside, since the two
    /// now share a pruning rule ([`WalkBounds`]) but not a leaf branch.
    ///
    /// Keys here are shaped like real index entries — several row ids sharing
    /// one encoded value, then the row id big-endian on the end
    /// (`crate::index::entry_key`) — rather than the flat counter the other
    /// walk test above uses, so a leaf with more than one entry per "value"
    /// is actually exercised.
    #[test]
    fn an_index_row_id_walk_agrees_with_the_general_entry_walk() {
        // Row ids are `value * 3 + id`, globally unique across every value
        // rather than 0..3 repeated 300 times — a real index never reuses a
        // row id, and the resume step below needs to invert a row id back to
        // the entry it came from unambiguously.
        let entry = |value: u32, id: u64| {
            let mut key = b"\x01idx:i\0".to_vec();
            key.extend_from_slice(&value.to_be_bytes());
            key.extend_from_slice(&(value as u64 * 3 + id).to_be_bytes());
            key
        };
        let decode = |key: &[u8]| -> RowId {
            let bytes: [u8; 8] = key[key.len() - 8..].try_into().unwrap();
            RowId::from_be_bytes(bytes)
        };
        let entry_for_row_id = |row_id: RowId| entry((row_id / 3) as u32, row_id % 3);

        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        // 300 values, 3 row ids apiece — enough entries, and enough entries
        // per value, to span several leaves either way.
        for value in 0..300u32 {
            for id in 0..3u64 {
                db.put(&entry(value, id), &[]).unwrap();
            }
            // Noise in an adjacent key space, so a walk that leaked past its
            // bounds would be wrong rather than merely slow — the same point
            // `one_walk_answers_the_prefix_the_range_and_the_resume_alike`
            // makes for the general walk.
            db.put(
                &[b"\x01idx:j\0".to_vec(), value.to_be_bytes().to_vec()].concat(),
                b"x",
            )
            .unwrap();
            db.commit().unwrap();
        }

        let assert_agrees =
            |start: &[u8], end: Option<&[u8]>, after: Option<&[u8]>, limit: usize| {
                let by_row_id = db
                    .scan_range_row_ids_from(start, end, after, limit)
                    .unwrap();
                let by_general_walk: Vec<RowId> = db
                    .scan_range_from(start, end, after, limit)
                    .unwrap()
                    .iter()
                    .map(|(key, _)| decode(key))
                    .collect();
                assert_eq!(
                    by_row_id, by_general_walk,
                    "row-id walk disagreed with the general walk over [{start:?}, {end:?}) after \
                 {after:?} limit {limit}"
                );
            };

        let prefix = b"\x01idx:i\0";
        let upper = prefix_upper_bound(prefix);

        // The whole index, unbounded.
        assert_agrees(prefix, upper.as_deref(), None, usize::MAX);
        assert_eq!(
            db.scan_range_row_ids_from(prefix, upper.as_deref(), None, usize::MAX)
                .unwrap()
                .len(),
            900
        );

        // A range covering many values (and therefore many entries per leaf
        // group), which is the shape an equality or a `BETWEEN` probe builds.
        let (start, end) = (entry(50, 0), entry(120, 0));
        assert_agrees(&start, Some(&end), None, usize::MAX);

        // The same range, pulled through in small resumed batches — the shape
        // a streaming caller would use, exercised here even though today's
        // only caller (`Storage::scan_index_row_ids`) still asks for the
        // whole range in one call. Reassembled, the batches must reproduce
        // the whole-range answer exactly: no row id repeated, none skipped.
        let expected = db
            .scan_range_row_ids_from(&start, Some(&end), None, usize::MAX)
            .unwrap();
        assert_eq!(expected.len(), 210);
        let mut resumed = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let batch = db
                .scan_range_row_ids_from(&start, Some(&end), after.as_deref(), 5)
                .unwrap();
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= 5);
            // The resume key has to be a real key from this batch, not the row
            // id alone — `after` compares against whole keys.
            after = Some(entry_for_row_id(*batch.last().unwrap()));
            resumed.extend(batch);
        }
        assert_eq!(resumed, expected);

        // Degenerate bounds: empty, not "the whole index".
        assert!(db
            .scan_range_row_ids_from(&end, Some(&start), None, usize::MAX)
            .unwrap()
            .is_empty());
        assert!(db
            .scan_range_row_ids_from(&start, Some(&start), None, usize::MAX)
            .unwrap()
            .is_empty());
        assert!(db
            .scan_range_row_ids_from(&start, Some(&end), None, 0)
            .unwrap()
            .is_empty());
    }

    /// The row-id-and-value walk is the table scan's fast path, so it has to
    /// agree with the general `(key, value)` walk row for row: same row ids
    /// (decoded out of the key), same values, same order.
    #[test]
    fn a_row_values_walk_agrees_with_the_general_walk() {
        let row_key = |id: u64| {
            let mut key = b"\x01tbl\0".to_vec();
            key.extend_from_slice(&id.to_be_bytes());
            key
        };
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        // Enough rows to span several leaves, with adjacent-key noise so a walk
        // that leaked past its bounds would be wrong, not merely slow.
        for id in 1..=900u64 {
            db.put(&row_key(id), format!("row-{id}").as_bytes())
                .unwrap();
            db.put(
                &[b"\x01tblz\0".to_vec(), id.to_be_bytes().to_vec()].concat(),
                b"noise",
            )
            .unwrap();
            db.commit().unwrap();
        }

        let by_raw = db
            .scan_prefix_row_values_raw_from(b"\x01tbl\0", None, usize::MAX)
            .unwrap();
        let by_general_walk = db.scan_prefix(b"\x01tbl\0").unwrap();
        assert_eq!(by_raw.len(), by_general_walk.len());
        assert_eq!(by_raw.len(), 900);
        for ((row_id, value), (key, general_value)) in by_raw.iter().zip(by_general_walk.iter()) {
            assert_eq!(
                *row_id,
                crate::storage::row_id_from_key(key).unwrap(),
                "raw row-value walk disagreed with the general walk's row id"
            );
            assert_eq!(value.as_slice(), general_value.as_slice());
        }

        // The decoded-node walk is the parity oracle: raw and decoded agree row
        // for row, so a change to either cannot drift silently from the other.
        let by_decoded = db
            .scan_prefix_row_values_from(b"\x01tbl\0", None, usize::MAX)
            .unwrap();
        assert_eq!(by_raw, by_decoded);

        // A resumed, batched read reassembles to the whole-range answer.
        let expected = by_raw;
        let mut resumed = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let batch = db
                .scan_prefix_row_values_raw_from(b"\x01tbl\0", after.as_deref(), 5)
                .unwrap();
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= 5);
            // `after` compares against whole keys, so resume from the last
            // batch's row key, not its row id alone.
            after = Some(row_key(batch.last().unwrap().0));
            resumed.extend(batch);
        }
        assert_eq!(resumed.len(), expected.len());
        for ((row_id, value), (expected_id, expected_value)) in resumed.iter().zip(expected.iter())
        {
            assert_eq!(row_id, expected_id);
            assert_eq!(value.as_slice(), expected_value.as_slice());
        }
    }

    /// The row-values walk has to see the open transaction's uncommitted rows
    /// exactly as the general walk does — a pending page is resolved the same
    /// way on both paths, so the fast path cannot skip the in-transaction
    /// state.
    #[test]
    fn a_row_values_walk_sees_the_open_transaction() {
        let row_key = |id: u64| {
            let mut key = b"t\0".to_vec();
            key.extend_from_slice(&id.to_be_bytes());
            key
        };
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(&row_key(1), b"one").unwrap();
        db.commit().unwrap();
        db.put(&row_key(2), b"two").unwrap();
        db.put(&row_key(3), b"three").unwrap();

        let by_row_values = db
            .scan_prefix_row_values_raw_from(b"t\0", None, usize::MAX)
            .unwrap();
        let by_general_walk = db.scan_prefix(b"t\0").unwrap();
        assert_eq!(by_row_values.len(), 3);
        assert_eq!(by_row_values.len(), by_general_walk.len());
        for ((row_id, value), (key, general_value)) in
            by_row_values.iter().zip(by_general_walk.iter())
        {
            assert_eq!(*row_id, crate::storage::row_id_from_key(key).unwrap());
            assert_eq!(value.as_slice(), general_value.as_slice());
        }
    }

    /// An overflow-backed row value is resolved whole by the row-values walk,
    /// agreeing with the general walk: inline and overflow values round-trip
    /// identically through the fast path.
    #[test]
    fn a_row_values_walk_resolves_overflow_values() {
        let row_key = |id: u64| {
            let mut key = b"t\0".to_vec();
            key.extend_from_slice(&id.to_be_bytes());
            key
        };
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        let small = b"small".to_vec();
        let big = vec![7u8; PAGE * 3];
        db.put(&row_key(1), &small).unwrap();
        db.put(&row_key(2), &big).unwrap();
        db.commit().unwrap();

        let by_row_values = db
            .scan_prefix_row_values_raw_from(b"t\0", None, usize::MAX)
            .unwrap();
        let by_general_walk = db.scan_prefix(b"t\0").unwrap();
        assert_eq!(by_row_values.len(), 2);
        for ((row_id, value), (key, general_value)) in
            by_row_values.iter().zip(by_general_walk.iter())
        {
            assert_eq!(*row_id, crate::storage::row_id_from_key(key).unwrap());
            assert_eq!(value.as_slice(), general_value.as_slice());
        }
        // The large value really did spill to overflow, and it came back whole.
        assert_eq!(by_row_values[1].1.as_slice(), &big[..]);
    }

    /// A too-short key is corruption, not a panic — the row-id walk has to
    /// answer the same way [`row_id_from_entry`]-style decoding would rather
    /// than unwrap past a key the general walk would have accepted just fine
    /// (a value with no row id at all, which nothing but a corrupt tree could
    /// produce).
    #[test]
    fn a_row_id_walk_over_a_too_short_key_reports_corruption_not_a_panic() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        // Three bytes: nowhere near the eight a row id needs, and nothing but
        // a corrupt tree could produce this under the real encodings — every
        // real row key and index entry ends with a full eight-byte row id.
        db.put(b"abc", b"").unwrap();
        db.commit().unwrap();
        // The general walk has no trouble with it...
        assert_eq!(db.scan_range(b"a", None).unwrap().len(), 1);
        // ...but the row-id walk cannot recover a row id from three bytes, and
        // reports corruption rather than panicking on the underflow.
        let err = db
            .scan_range_row_ids_from(b"a", None, None, usize::MAX)
            .unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
    }

    #[test]
    fn a_prefix_of_all_high_bytes_has_no_upper_bound() {
        assert_eq!(prefix_upper_bound(b""), None);
        assert_eq!(prefix_upper_bound(&[0xff, 0xff]), None);
        assert_eq!(prefix_upper_bound(&[0x01, 0xff]), Some(vec![0x02]));
        assert_eq!(prefix_upper_bound(b"ab"), Some(b"ac".to_vec()));

        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(&[0xff, 0xff, 0x01], b"v").unwrap();
        db.put(&[0xfe], b"w").unwrap();
        db.commit().unwrap();
        assert_eq!(
            db.scan_prefix(&[0xff, 0xff]).unwrap(),
            vec![(vec![0xff, 0xff, 0x01], RowBuf::Owned(b"v".to_vec()))]
        );
    }

    #[test]
    fn put_overwrites() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"k", b"1").unwrap();
        db.commit().unwrap();
        db.put(b"k", b"2").unwrap();
        db.commit().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(b"2".to_vec())));
    }

    #[test]
    fn delete_removes_a_key() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        db.commit().unwrap();
        db.delete(b"a").unwrap();
        db.commit().unwrap();
        assert_eq!(db.get(b"a").unwrap(), None);
        assert_eq!(db.get(b"b").unwrap(), Some(RowBuf::Owned(b"2".to_vec())));
    }

    #[test]
    fn deleting_a_missing_key_is_a_no_op() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.delete(b"zzz").unwrap();
        db.commit().unwrap();
        assert_eq!(
            db.scan().unwrap(),
            vec![(b"a".to_vec(), RowBuf::Owned(b"1".to_vec()))]
        );
    }

    #[test]
    fn scan_returns_keys_in_order() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        for i in (0..32u32).rev() {
            db.put(&format!("key-{i:02}").into_bytes(), &i.to_le_bytes())
                .unwrap();
        }
        db.commit().unwrap();
        let rows = db.scan().unwrap();
        let keys: Vec<Vec<u8>> = rows.iter().map(|(k, _)| k.clone()).collect();
        let mut expected: Vec<Vec<u8>> = (0..32u32)
            .map(|i| format!("key-{i:02}").into_bytes())
            .collect();
        expected.sort();
        assert_eq!(keys, expected);
    }

    #[test]
    fn grows_beyond_a_single_page_and_round_trips() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        let n = 2000u32;
        // Commit in batches: a single commit cannot hold more pages than the
        // write-ahead-log region, and real statements are far smaller.
        for i in 0..n {
            db.put(&format!("key-{i:05}").into_bytes(), &i.to_le_bytes())
                .unwrap();
            if i % 32 == 31 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();

        for i in 0..n {
            let key = format!("key-{i:05}").into_bytes();
            assert_eq!(
                db.get(&key).unwrap(),
                Some(RowBuf::Owned(i.to_le_bytes().to_vec())),
                "missing key {i}"
            );
        }

        let reopened = reopen(&db);
        let rows = reopened.scan().unwrap();
        assert_eq!(rows.len(), n as usize);
        for (k, v) in rows {
            let i = String::from_utf8(k[4..].to_vec())
                .unwrap()
                .parse::<u32>()
                .unwrap();
            assert_eq!(v, i.to_le_bytes().to_vec());
        }
    }

    #[test]
    fn an_old_root_reads_as_a_snapshot_after_new_commits() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"k", b"old").unwrap();
        db.commit().unwrap();
        let old_root = db.root();

        db.put(b"k", b"new").unwrap();
        db.commit().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(b"new".to_vec())));
        assert_eq!(
            db.get_at(old_root, b"k").unwrap(),
            Some(RowBuf::Owned(b"old".to_vec()))
        );
    }

    /// The retained read cursor (AHL-472 step 2) must answer identically to a
    /// fresh descent regardless of the order lookups arrive in: ascending
    /// (mostly same-leaf or adjacent-leaf reseeks — the pattern a join probe
    /// produces, since `IndexProbe::prepare` fetches sorted row ids),
    /// descending (essentially every lookup is a cursor miss), a fixed
    /// non-adjacent jump pattern (thrashes the cursor even harder than
    /// descending), and misses interleaved with hits.
    #[test]
    fn point_reads_agree_whether_ascending_descending_or_jumbled() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        let n = 500u32;
        // Enough keys at this page size to span several leaves and at least
        // one internal level, so a reseek has both same-leaf and
        // sibling-leaf/root-fallback cases to exercise.
        for i in 0..n {
            db.put(&format!("key-{i:05}").into_bytes(), &i.to_le_bytes())
                .unwrap();
            if i % 32 == 31 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();

        let expect = |db: &CowBTree<SimDisk>, i: u32| {
            let key = format!("key-{i:05}").into_bytes();
            assert_eq!(
                db.get(&key).unwrap(),
                Some(RowBuf::Owned(i.to_le_bytes().to_vec())),
                "key {i}"
            );
        };

        for i in 0..n {
            expect(&db, i);
        }
        for i in (0..n).rev() {
            expect(&db, i);
        }
        // A stride coprime with `n` visits every key exactly once in an order
        // where consecutive lookups are almost never on the same or an
        // adjacent leaf.
        let stride = 97u32;
        for step in 0..n {
            expect(&db, (step * stride) % n);
        }
        // Misses interleaved with hits: whatever leaf the cursor last landed
        // on, a key strictly between two present ones that is itself absent
        // must still answer `None`, not the neighbour's value.
        for i in 0..n {
            let key = format!("key-{i:05}-missing").into_bytes();
            assert_eq!(db.get(&key).unwrap(), None, "key {i} should be absent");
            expect(&db, i);
        }
    }

    /// A single retained cursor slot, thrashed between two different
    /// committed roots on the *same* keys, must never answer from the wrong
    /// root's leaf — each `get_at` either reseeks correctly under its own
    /// root or falls back to a fresh descent, but it may not silently reuse
    /// the other root's cached span.
    #[test]
    fn a_thrashed_cursor_never_crosses_between_two_roots() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        let n = 200u32;
        for i in 0..n {
            db.put(&format!("key-{i:05}").into_bytes(), b"old").unwrap();
            if i % 32 == 31 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();
        let old_root = db.root();

        for i in 0..n {
            db.put(&format!("key-{i:05}").into_bytes(), b"new").unwrap();
            if i % 32 == 31 {
                db.commit().unwrap();
            }
        }
        db.commit().unwrap();
        let new_root = db.root();
        assert_ne!(old_root, new_root, "an update must allocate a fresh root");

        for i in 0..n {
            let key = format!("key-{i:05}").into_bytes();
            assert_eq!(
                db.get_at(old_root, &key).unwrap(),
                Some(RowBuf::Owned(b"old".to_vec())),
                "key {i} under the old root"
            );
            assert_eq!(
                db.get_at(new_root, &key).unwrap(),
                Some(RowBuf::Owned(b"new".to_vec())),
                "key {i} under the new root"
            );
        }
    }

    #[test]
    fn a_value_larger_than_a_page_spills_to_overflow_and_round_trips() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        // Several pages' worth, not merely "more than one".
        let huge = vec![0xABu8; PAGE * 5];
        db.put(b"k", &huge).unwrap();
        db.commit().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(huge.clone())));

        // The spill survives a reopen byte-identically.
        let reopened = reopen(&db);
        assert_eq!(reopened.get(b"k").unwrap(), Some(RowBuf::Owned(huge)));
    }

    #[test]
    fn values_near_the_page_boundary_choose_inline_or_overflow_correctly() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        // Smallest value that must overflow: one more byte than an inline cell
        // for key "k" can hold. The inline ceiling is half a page, so the value
        // sits just below it.
        let inline_max = PAGE / 2 - (16 + 2 + 2 + 1 + 1 + 4);
        db.put(b"k", &vec![0u8; inline_max]).unwrap();
        db.put(b"l", &vec![0u8; inline_max + 1]).unwrap();
        db.commit().unwrap();
        assert_eq!(
            db.get(b"k").unwrap(),
            Some(RowBuf::Owned(vec![0u8; inline_max]))
        );
        assert_eq!(
            db.get(b"l").unwrap(),
            Some(RowBuf::Owned(vec![0u8; inline_max + 1]))
        );
    }

    #[test]
    fn a_key_that_cannot_fit_is_rejected() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        let huge_key = vec![0u8; PAGE];
        assert!(db.put(&huge_key, b"v").is_err());
    }

    #[test]
    fn a_crash_loses_only_the_uncommitted_write() {
        // Faults: clean create, clean commit of "a", then a crash on the commit
        // of "b". Reopening must see "a" and not "b".
        let sim = Simulator::with_disk(
            1,
            SimDisk::with_block_size(512, CAPACITY),
            FaultSchedule::script(&[Fault::None, Fault::None, Fault::Crash]),
        );
        let mut db = CowBTree::create(sim, PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.commit().unwrap();
        db.put(b"b", b"2").unwrap();
        db.commit().unwrap();

        let image = db.device().disk().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"a").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(reopened.get(b"b").unwrap(), None);
    }

    #[test]
    fn a_torn_state_block_is_recovered_from_the_wal() {
        // Commit "a" and checkpoint it cleanly, commit "b", then tear the
        // checkpoint's state-block write. Recovery must replay "b" from the log.
        let sim = Simulator::with_disk(
            2,
            SimDisk::with_block_size(512, CAPACITY),
            FaultSchedule::script(&[
                Fault::None,                    // create
                Fault::None,                    // commit "a"
                Fault::None,                    // checkpoint "a"
                Fault::None,                    // commit "b"
                Fault::TornWrite { prefix: 8 }, // checkpoint "b" -> torn state
            ]),
        );
        let mut db = CowBTree::create(sim, PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        db.put(b"b", b"2").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();

        let image = db.device().disk().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"a").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(
            reopened.get(b"b").unwrap(),
            Some(RowBuf::Owned(b"2".to_vec()))
        );
    }

    #[test]
    fn a_crash_between_commit_and_checkpoint_recovers_the_commit() {
        // The commit of "b" reaches the log, but the checkpoint crashes before
        // persisting the state block. Recovery must still see "b".
        let sim = Simulator::with_disk(
            3,
            SimDisk::with_block_size(512, CAPACITY),
            FaultSchedule::script(&[
                Fault::None,  // create
                Fault::None,  // commit "a"
                Fault::None,  // checkpoint "a"
                Fault::None,  // commit "b"
                Fault::Crash, // checkpoint "b" -> crash
            ]),
        );
        let mut db = CowBTree::create(sim, PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        db.put(b"b", b"2").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();

        let image = db.device().disk().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"a").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(
            reopened.get(b"b").unwrap(),
            Some(RowBuf::Owned(b"2".to_vec()))
        );
    }

    #[test]
    fn a_torn_log_record_is_not_a_commit() {
        // The commit of "b" writes a record that is torn before it syncs. It is
        // not a commit: recovery must see "a" only.
        let sim = Simulator::with_disk(
            4,
            SimDisk::with_block_size(512, CAPACITY),
            FaultSchedule::script(&[
                Fault::None,                    // create
                Fault::None,                    // commit "a"
                Fault::None,                    // checkpoint "a"
                Fault::TornWrite { prefix: 4 }, // commit "b" -> torn log record
            ]),
        );
        let mut db = CowBTree::create(sim, PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        db.put(b"b", b"2").unwrap();
        db.commit().unwrap();

        let image = db.device().disk().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"a").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(reopened.get(b"b").unwrap(), None);
    }

    /// A committed state that has gone *backwards* must not restart the page
    /// allocator inside ids this handle has already written (AHL-406).
    ///
    /// The checkpoint after `b` loses the sync that publishes its state block,
    /// and the log region it truncates goes with it. The next commit therefore
    /// reads a committed state older than the one this handle has already
    /// written pages for. Adopting that state's root is right — those commits
    /// are gone. Adopting its next-free-page counter is not: the allocator would
    /// restart inside a range of ids that are already occupied, and a page id is
    /// the only key the page cache has (see [`super::cache`]). The result was a
    /// tree whose root reached pages from two different timelines at once, with
    /// no checksum or decode failing anywhere.
    #[test]
    fn a_rewound_committed_state_never_recycles_a_page_id() {
        let sim = Simulator::with_disk(
            11,
            SimDisk::with_block_size(512, CAPACITY),
            FaultSchedule::script(&[
                Fault::None,  // create
                Fault::None,  // commit "a"
                Fault::None,  // checkpoint "a"
                Fault::None,  // commit "b"
                Fault::Crash, // checkpoint "b": the state block never lands
            ]),
        );
        let mut db = CowBTree::create(sim, PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        db.put(b"b", b"2").unwrap();
        db.commit().unwrap();

        // Every page id below this has been written to the data area.
        let high_water = db.next_page_id;
        db.checkpoint().unwrap();

        // The rewind is adopted here, when the commit re-reads a committed
        // state older than the pages this handle has already written.
        db.put(b"c", b"3").unwrap();
        db.commit().unwrap();
        assert!(
            db.next_page_id > high_water,
            "the allocator rewound to {} after the committed state went backwards, \
             handing out page ids below the {high_water} already written",
            db.next_page_id
        );

        // And what recovery finds is a state some commit really produced: `b`
        // was lost with the region that held its record, `a` and `c` are the
        // tree the last commit wrote.
        let image = db.device().disk().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"a").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(
            reopened.get(b"c").unwrap(),
            Some(RowBuf::Owned(b"3".to_vec()))
        );
    }

    #[test]
    fn a_torn_record_survives_and_heals_lost_pages() {
        // The commit of "b" writes its page and its record together. A torn
        // write that keeps the whole record (it is small) but loses the page
        // must still recover "b" — the record carries the page, so recovery
        // rebuilds it.
        let sim = Simulator::with_disk(
            5,
            SimDisk::with_block_size(512, CAPACITY),
            FaultSchedule::script(&[
                Fault::None,                       // create
                Fault::None,                       // commit "a"
                Fault::None,                       // checkpoint "a"
                Fault::TornWrite { prefix: 4096 }, // commit "b" -> record survives, page lost
            ]),
        );
        let mut db = CowBTree::create(sim, PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        db.put(b"b", b"2").unwrap();
        db.commit().unwrap();

        let image = db.device().disk().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"a").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(
            reopened.get(b"b").unwrap(),
            Some(RowBuf::Owned(b"2".to_vec()))
        );
    }

    #[test]
    fn a_crash_mid_overflow_write_loses_the_whole_row_not_half_of_it() {
        // A crash while a multi-page value is being committed must leave the
        // row absent, never half-written: its overflow pages and its record
        // reach the device in one sync, so either all of them are durable or
        // none are.
        let sim = Simulator::with_disk(
            6,
            SimDisk::with_block_size(512, CAPACITY),
            FaultSchedule::script(&[
                Fault::None,  // create
                Fault::None,  // commit "a"
                Fault::None,  // checkpoint "a"
                Fault::Crash, // commit the big value -> crash before sync
            ]),
        );
        let mut db = CowBTree::create(sim, PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        let big = vec![0xCDu8; PAGE * 4];
        db.put(b"big", &big).unwrap();
        db.commit().unwrap();

        let image = db.device().disk().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"a").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(reopened.get(b"big").unwrap(), None);
    }

    #[test]
    fn a_torn_overflow_page_is_healed_from_the_record() {
        // The record for a multi-page value is written last; a torn write that
        // keeps the whole record but loses its data-area overflow pages must
        // still recover the value byte-identically, because the record carries
        // every page the commit wrote — overflow pages included.
        let sim = Simulator::with_disk(
            7,
            SimDisk::with_block_size(512, CAPACITY),
            FaultSchedule::script(&[
                Fault::None,                       // create
                Fault::None,                       // commit "a"
                Fault::None,                       // checkpoint "a"
                Fault::TornWrite { prefix: 8192 }, // commit the big value -> record survives
            ]),
        );
        let mut db = CowBTree::create(sim, PAGE).unwrap();
        db.put(b"a", b"1").unwrap();
        db.commit().unwrap();
        db.checkpoint().unwrap();
        let big = vec![0xEFu8; PAGE * 8];
        db.put(b"big", &big).unwrap();
        db.commit().unwrap();

        let image = db.device().disk().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"a").unwrap(),
            Some(RowBuf::Owned(b"1".to_vec()))
        );
        assert_eq!(reopened.get(b"big").unwrap(), Some(RowBuf::Owned(big)));
    }

    #[test]
    fn a_newer_format_file_is_refused_as_newer_not_corrupt() {
        let mut disk = SimDisk::with_block_size(512, CAPACITY);
        disk.write(0, &encode_header_with_version(PAGE, FORMAT_VERSION + 1))
            .unwrap();
        let err = match CowBTree::open(disk) {
            Err(err) => err,
            Ok(_) => panic!("a newer-format file was accepted"),
        };
        assert!(matches!(err, Error::FormatVersion(_)), "{err}");
        assert!(err.to_string().contains("newer"), "{err}");
    }

    #[test]
    fn an_older_format_file_is_refused_as_older_not_corrupt() {
        let mut disk = SimDisk::with_block_size(512, CAPACITY);
        disk.write(
            0,
            &encode_header_with_version(PAGE, MIN_READABLE_FORMAT_VERSION - 1),
        )
        .unwrap();
        let err = match CowBTree::open(disk) {
            Err(err) => err,
            Ok(_) => panic!("an older-format file was accepted"),
        };
        assert!(matches!(err, Error::FormatVersion(_)), "{err}");
        assert!(err.to_string().contains("older"), "{err}");
    }

    #[test]
    fn a_version_three_exact_database_header_is_grandfathered() {
        let (page_size, version) = parse_header(&encode_header_with_version(PAGE, 3)).unwrap();
        assert_eq!(page_size, PAGE);
        assert_eq!(version, 3);
    }

    #[test]
    fn a_single_region_version_four_database_remains_read_write_compatible() {
        let mut legacy = disk();
        legacy
            .write(0, &encode_header_with_version(PAGE, 4))
            .unwrap();
        legacy.write(PAGE, &encode_state(0, 1, 0)).unwrap();
        legacy
            .write(
                crate::wal::wal_start(PAGE),
                &vec![0; crate::wal::wal_region_len(PAGE)],
            )
            .unwrap();
        legacy.sync(Fault::None);

        let mut db = CowBTree::open(legacy).unwrap();
        assert_eq!(db.format_version(), 4);
        db.put(b"legacy", b"still opens").unwrap();
        db.commit().unwrap();

        let image = db.device().durable().to_vec();
        let reopened = CowBTree::open(SimDisk::with_image(512, &image)).unwrap();
        assert_eq!(
            reopened.get(b"legacy").unwrap(),
            Some(RowBuf::Owned(b"still opens".to_vec()))
        );
    }

    #[test]
    fn a_torn_header_is_unrecoverable() {
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        db.put(b"k", b"v").unwrap();
        db.commit().unwrap();

        let mut image = db.device().durable().to_vec();
        image[3] ^= 0x01; // corrupt the header's magic/version area
        assert!(CowBTree::open(SimDisk::with_image(512, &image)).is_err());
    }

    #[test]
    fn the_log_wraps_when_full_and_still_recovers() {
        // Each commit appends a small record; commit enough times that the log
        // wraps (checkpoints and reuses the region), then recover.
        let mut db = CowBTree::create(disk(), PAGE).unwrap();
        for i in 0..(crate::wal::WAL_BLOCKS * 3) {
            db.put(b"k", &i.to_le_bytes()).unwrap();
            db.commit().unwrap();
        }
        let expected = (crate::wal::WAL_BLOCKS * 3 - 1).to_le_bytes().to_vec();
        assert_eq!(db.get(b"k").unwrap(), Some(RowBuf::Owned(expected.clone())));

        let reopened = reopen(&db);
        assert_eq!(reopened.get(b"k").unwrap(), Some(RowBuf::Owned(expected)));
    }

    #[test]
    fn the_same_seed_replays_the_same_durable_image() {
        fn reordered(db: &CowBTree<Simulator>) -> bool {
            matches!(
                db.device().disk().trace().last(),
                Some(crate::sim::TraceEvent::Sync {
                    fault: Fault::ReorderedSync { .. },
                    ..
                })
            )
        }
        fn run(seed: u64) -> Vec<u8> {
            let sim = Simulator::new(seed);
            let mut db = CowBTree::create(sim, PAGE).unwrap();
            for i in 0..64u32 {
                if db.device().crashed() || reordered(&db) {
                    break;
                }
                db.put(&format!("k{i:03}").into_bytes(), &i.to_le_bytes())
                    .unwrap();
                if i % 8 == 0 {
                    db.commit().unwrap();
                }
            }
            if !db.device().crashed() && !reordered(&db) {
                db.commit().unwrap();
            }
            db.device().disk().durable().to_vec()
        }
        // Reproducibility is the guarantee: a seed must replay byte-for-byte.
        assert_eq!(run(42), run(42));
        assert_eq!(run(7), run(7));
    }

    // ------------------------------------------------------------- MVCC

    /// A database shared by several trees, as several writers (or a writer and
    /// a reader) in one process would share it.
    fn shared_disk() -> alloc::rc::Rc<core::cell::RefCell<SimDisk>> {
        alloc::rc::Rc::new(core::cell::RefCell::new(SimDisk::with_block_size(
            512, CAPACITY,
        )))
    }

    #[test]
    fn a_conflicting_transaction_aborts_and_reloads_the_winner() {
        let disk = shared_disk();
        let mut writer_a = CowBTree::create(disk.clone(), PAGE).unwrap();
        writer_a.put(b"k", b"v1").unwrap();
        writer_a.commit().unwrap();

        // A second writer opens the same database and sees v1.
        let mut writer_b = CowBTree::open(disk.clone()).unwrap();
        assert_eq!(
            writer_b.get(b"k").unwrap(),
            Some(RowBuf::Owned(b"v1".to_vec()))
        );

        // Both begin a transaction from the same committed root.
        writer_a.put(b"k", b"v2").unwrap();
        writer_b.put(b"k", b"v3").unwrap();

        // A commits first and wins.
        assert_eq!(writer_a.commit().unwrap(), CommitOutcome::Committed);

        // B is stale: its commit conflicts, its write is discarded, and it
        // reloads the winner's state.
        assert_eq!(writer_b.commit().unwrap(), CommitOutcome::Conflict);
        assert_eq!(
            writer_b.get(b"k").unwrap(),
            Some(RowBuf::Owned(b"v2".to_vec()))
        );

        // A fresh transaction from B can now commit cleanly.
        writer_b.put(b"k", b"v3").unwrap();
        assert_eq!(writer_b.commit().unwrap(), CommitOutcome::Committed);
        assert_eq!(
            writer_b.get(b"k").unwrap(),
            Some(RowBuf::Owned(b"v3".to_vec()))
        );
    }

    #[test]
    fn first_of_many_writers_wins_without_corruption() {
        let disk = shared_disk();
        let mut seed = CowBTree::create(disk.clone(), PAGE).unwrap();
        seed.put(b"k", b"0").unwrap();
        seed.commit().unwrap();

        // Eight writers, all based on the same snapshot, all write to "k".
        let mut writers: Vec<CowBTree<_>> = (0..8)
            .map(|_| CowBTree::open(disk.clone()).unwrap())
            .collect();
        for (i, writer) in writers.iter_mut().enumerate() {
            writer.put(b"k", &[(i + 1) as u8]).unwrap();
        }

        // Committed in order: exactly one wins, the rest abort cleanly.
        let mut committed = 0;
        let mut conflicts = 0;
        for writer in &mut writers {
            match writer.commit().unwrap() {
                CommitOutcome::Committed => committed += 1,
                CommitOutcome::Conflict => conflicts += 1,
            }
        }
        assert_eq!(committed, 1);
        assert_eq!(conflicts, 7);

        // The database is consistent: exactly the first writer's value.
        let reopened = CowBTree::open(disk.clone()).unwrap();
        assert_eq!(reopened.get(b"k").unwrap(), Some(RowBuf::Owned(vec![1u8])));
    }

    #[test]
    fn a_reader_sees_a_stable_snapshot_across_a_concurrent_commit() {
        let disk = shared_disk();
        let mut writer = CowBTree::create(disk.clone(), PAGE).unwrap();
        writer.put(b"k", b"old").unwrap();
        writer.commit().unwrap();
        let snapshot = writer.root();

        // Another writer commits an update to the same key.
        let mut other = CowBTree::open(disk.clone()).unwrap();
        other.put(b"k", b"new").unwrap();
        other.commit().unwrap();

        // The pinned snapshot still reads the old value…
        assert_eq!(
            writer.get_at(snapshot, b"k").unwrap(),
            Some(RowBuf::Owned(b"old".to_vec()))
        );
        // …and so does the writer, until it re-opens or reloads.
        assert_eq!(
            writer.get(b"k").unwrap(),
            Some(RowBuf::Owned(b"old".to_vec()))
        );

        // A fresh open sees the new value.
        let fresh = CowBTree::open(disk.clone()).unwrap();
        assert_eq!(
            fresh.get(b"k").unwrap(),
            Some(RowBuf::Owned(b"new".to_vec()))
        );
    }

    #[test]
    fn splits_pack_by_size_when_entries_vary_widely() {
        // Large values (rows carrying a VECTOR) mixed with tiny ones must split
        // into pages that each fit — a count-based split can leave a half still
        // too big for the page.
        let mut db = CowBTree::create(disk(), 4096).unwrap();
        db.put(b"\0a", b"tiny").unwrap();
        db.put(b"\0b", b"tiny").unwrap();
        for i in 0..64u64 {
            let mut key = b"row".to_vec();
            key.extend_from_slice(&i.to_be_bytes());
            db.put(&key, &vec![0xABu8; 1600]).unwrap();
        }
        db.commit().unwrap();

        assert_eq!(db.scan().unwrap().len(), 66);
        for i in 0..64u64 {
            let mut key = b"row".to_vec();
            key.extend_from_slice(&i.to_be_bytes());
            assert_eq!(
                db.get(&key).unwrap(),
                Some(RowBuf::Owned(vec![0xABu8; 1600]))
            );
        }

        let reopened = reopen(&db);
        assert_eq!(reopened.scan().unwrap().len(), 66);
    }
}
