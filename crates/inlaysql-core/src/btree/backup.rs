//! Online backup: a consistent copy of a live database, taken without
//! stopping the writer.
//!
//! `inlaysql vacuum` is compaction, not backup — it opens read-write, holds
//! the exclusive advisory lock for the whole copy-and-rename, and therefore
//! cannot run at all against a server that is holding that lock for its
//! lifetime (`docs/enterprise-readiness.md`, blocker 2). This is the other
//! thing: it never writes a byte of the source, needs no lock of its own, and
//! can run while every other handle on the file keeps committing.
//!
//! # Why a copy of a committed root is already consistent
//!
//! Nothing here reconstructs anything. A committed root *is* an immutable,
//! consistent snapshot by construction: every mutation copies the root-to-leaf
//! path into freshly allocated pages and swaps the root, so the pages an older
//! root reaches are never touched again ([`super::tree`]'s module doc, D4,
//! `docs/architecture.md`). That is the same property MVCC readers already
//! rest on. So a backup is: pin a committed root, copy the pages reachable
//! from it, and write a file whose state block names that root — after which
//! the copy opens as exactly that snapshot, with no write-ahead log to replay
//! and nothing to recover.
//!
//! The copy is therefore never a mix of two commits, however many commits land
//! while it runs: a later commit writes *new* pages, at ids past everything
//! this walk will visit, and leaves every page it superseded exactly where it
//! was. It also cannot be a mix in the weaker, per-table sense a SQL-level
//! dump has to work to avoid — `vacuum`'s `SELECT * FROM t` per table would
//! read each table at whatever snapshot that statement's `refresh_snapshot`
//! landed on — because there is only one root and one walk.
//!
//! Page ids are preserved rather than renumbered. Renumbering would mean
//! rewriting every interior node's child pointers, which is a second
//! implementation of the tree's own invariants in the one place a bug is
//! silent; keeping the ids means the destination's data area is written at the
//! same offsets, unreachable ids are simply never written, and the result is a
//! sparse file the filesystem stores at its live size. A backup is not a
//! compaction and does not pretend to be one — that is what `vacuum` is for.
//!
//! # The one thing that can break it: page reuse
//!
//! [`super::tree::CowBTree::set_page_reuse`] (`EngineOptions::page_reuse`)
//! makes a page id reusable, which is precisely the assumption above being
//! false: a page this walk is about to read could be handed to a writer and
//! overwritten mid-copy, and because a page carries no checksum of its own,
//! the result would decode cleanly and be silently wrong. So this needs the
//! reuse question answered, not deferred, and there are exactly two answers:
//!
//! * **The snapshot is pinned.** A backup taken through a handle that
//!   registered a reader watermark ([`super::Device::register_reader`]) holds
//!   that watermark at its own committed sequence for the whole copy — a
//!   `&self` borrow means no `commit`/`refresh`/`checkpoint` on this handle
//!   can move it, all three taking `&mut self`. A page reachable from the root
//!   at sequence `S` was not superseded by any commit up to `S`, so it can
//!   only be freed at some sequence `> S`, and `refill_free_candidates`
//!   declines every candidate whose `freed_at` is not strictly below
//!   `min(commit_point.seq, min_reader_seq)`. With this handle holding
//!   `min_reader_seq` down at `S`, no page this walk can reach is reclaimable
//!   until the walk is over and the handle's `Drop` releases it. This is the
//!   ordinary case — every read-write `FileDevice` handle — and it holds *even
//!   with page reuse on*, in this process or, by the exclusive OS advisory
//!   lock `FileDevice::open` takes, any other.
//! * **The snapshot is not pinned**, which is exactly one device:
//!   `FileDevice::open_read_only`, which takes no lock by design and is
//!   therefore invisible to the reclaim proof in this process or any other
//!   (`docs/recovery.md`, "The cross-process answer, stated plainly"). There
//!   is no way to make that handle pinnable, so [`CowBTree::backup_to`]
//!   *refuses* rather than produce a copy it cannot vouch for — but only when
//!   the snapshot demonstrably contains free-list rows, since those exist if
//!   and only if some handle has committed to this file with reuse on. Be
//!   precise about what that proves: an empty free list does not prove reuse
//!   is off, only that nothing is currently recorded as reclaimable, so an
//!   unpinned backup of a file a writer has reuse on for remains unsound in
//!   the window before its first free-list row lands. Do not take an unpinned
//!   backup of a file any writer has reuse enabled for. The refusal is a net,
//!   not a proof.
//!
//! A device that tracks no readers *and* is not read-only (the simulated disk,
//! the WASM device) is pinned in the only sense that matters: `min_reader_seq`
//! answers `None` there, and `refill_free_candidates` treats `None` as
//! "unprovable, decline", so nothing on such a device ever reclaims a page at
//! all and no writer this device cannot see exists to do it either.
//!
//! [`CowBTree::backup_to`]: super::tree::CowBTree::backup_to

use alloc::vec;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::wal;

use super::device::Device;
use super::page::{self, Node, PageId, ValueRef};

/// What one snapshot copy contained. Returned by
/// [`CowBTree::backup_to`](super::tree::CowBTree::backup_to) so a caller can
/// report, log or assert on the copy without reopening it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupSummary {
    /// The committed root the copy was taken from. `0` is an empty tree.
    pub root: PageId,
    /// The commit sequence number that root belongs to — the snapshot's
    /// identity, and what the copy's state block records as already
    /// checkpointed.
    pub seq: u64,
    /// Data-area pages copied. Interior nodes, leaves and overflow pages;
    /// pages no longer reachable from `root` are not copied.
    pub pages: u64,
    /// Bytes written to the destination, including the header, the state block
    /// and the zeroed write-ahead log regions. Not the destination's apparent
    /// file size: preserved page ids leave holes, which a filesystem stores
    /// sparsely.
    pub bytes: u64,
    /// The page size the copy inherits from the source's immutable header.
    pub page_size: usize,
    /// The format version the copy inherits from the source's immutable
    /// header. A v3 or v4 database backs up as v3 or v4 — the region count,
    /// and therefore every data-area offset, is derived from it.
    pub format_version: u32,
}

/// Write the snapshot rooted at `root` from `source` into `dest`.
///
/// `next` is the source snapshot's next-free-page counter and `seq` its commit
/// sequence number. Both are copied into the destination's state block, so the
/// result opens as that snapshot with an empty log and nothing to replay.
///
/// The caller owns the pinning argument in this module's doc; this function
/// assumes it and only enforces what it can see locally — that every page id
/// it is asked to follow is one the snapshot could actually contain.
pub(super) fn copy_snapshot<S: Device + ?Sized>(
    source: &S,
    dest: &mut dyn Device,
    page_size: usize,
    format_version: u32,
    root: PageId,
    next: PageId,
    seq: u64,
) -> Result<BackupSummary> {
    let mut written = 0u64;

    // The header first, because it names the page size every other offset in
    // the file is expressed in and it is the one block that is never rewritten.
    let header = super::tree::encode_header_with_version(page_size, format_version);
    dest.write(wal::header_offset(), &header)?;
    written += header.len() as u64;

    // Every log region explicitly zeroed, for the same reason `CowBTree::create`
    // zeroes them: recovery stops a region at its first empty slot, and a
    // destination that was not freshly zeroed (a reused file, a device with
    // stale bytes) would otherwise have recovery mistake whatever is there for
    // records belonging to this snapshot. One region-sized buffer, reused,
    // rather than one allocation spanning all four.
    let zeros = vec![0u8; wal::wal_region_len(page_size)];
    for region in 0..wal::region_count(format_version) {
        dest.write(wal::region_start(page_size, format_version, region), &zeros)?;
        written += zeros.len() as u64;
    }

    let pages = copy_reachable_pages(source, dest, page_size, format_version, root, next)?;
    written += pages * page_size as u64;

    // The state block last: it is the pointer that makes everything above mean
    // something, and writing it first would leave an interrupted copy naming a
    // root whose pages are not all there yet. `seq` goes in as the *checkpointed*
    // sequence, so the empty log above is not merely ignored but correct — there
    // is nothing newer than the state block to replay.
    let state = super::tree::encode_state(root, next, seq);
    dest.write(wal::state_offset(page_size), &state)?;
    written += state.len() as u64;

    dest.sync()?;

    Ok(BackupSummary {
        root,
        seq,
        pages,
        bytes: written,
        page_size,
        format_version,
    })
}

/// Copy every data-area page reachable from `root`, returning how many there
/// were.
///
/// Depth-first over an explicit stack rather than recursion: the tree is
/// shallow, but a corrupt file is not obliged to be, and this is a path that
/// runs over bytes nobody has validated since they were written.
///
/// There is deliberately no visited set. Within one committed snapshot each
/// page has exactly one parent — copy-on-write copies a leaf's overflow
/// *pointer* when it copies the leaf, so two roots can share a chain but two
/// cells of one root never can — so a visited set would cost a growing
/// allocation to prove something the structure already guarantees. What guards
/// against a corrupt file that violates it is cheaper and stricter: every id
/// must be below the snapshot's own next-free-page counter (nothing at or
/// above it has ever been allocated), and the copy stops with
/// [`Error::Corrupt`] if it ever visits more pages than that counter allows.
/// A cycle or a shared subtree therefore terminates as an error rather than
/// spinning or exhausting memory.
fn copy_reachable_pages<S: Device + ?Sized>(
    source: &S,
    dest: &mut dyn Device,
    page_size: usize,
    format_version: u32,
    root: PageId,
    next: PageId,
) -> Result<u64> {
    let mut copied = 0u64;
    if root == 0 {
        return Ok(copied);
    }

    let mut buf = vec![0u8; page_size];
    let mut stack: Vec<PageId> = vec![root];
    while let Some(id) = stack.pop() {
        if id == 0 {
            continue;
        }
        if id >= next {
            return Err(Error::Corrupt(alloc::format!(
                "backup: page {id} is at or past the snapshot's next free page \
                 ({next}), so it was never allocated"
            )));
        }
        if copied >= next {
            return Err(Error::Corrupt(alloc::format!(
                "backup: the snapshot rooted at {root} reaches more than {next} \
                 pages, so it is not a tree"
            )));
        }

        let offset = wal::data_offset_for(page_size, format_version, id);
        source.read(offset, &mut buf)?;
        dest.write(offset, &buf)?;
        copied += 1;

        // An overflow page is not a node and `page::decode` rejects it, so it
        // is classified from its kind byte before decoding. Its payload is
        // never looked at — only the link to the next page of the chain.
        if buf[page::OFF_KIND] == page::KIND_OVERFLOW {
            stack.push(page::overflow_next(page_size, &buf)?);
            continue;
        }
        match page::decode(page_size, &buf)? {
            Node::Internal {
                leftmost, cells, ..
            } => {
                stack.push(leftmost);
                stack.extend(cells.iter().map(|cell| cell.child));
            }
            Node::Leaf { entries, .. } => {
                stack.extend(entries.iter().filter_map(|entry| match entry.value {
                    ValueRef::Overflow { first, .. } => Some(first),
                    ValueRef::Inline(_) | ValueRef::Owned(_) => None,
                }));
            }
        }
    }
    Ok(copied)
}
