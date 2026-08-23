//! A bounded cache of decoded, *committed* B-tree pages.
//!
//! Without it, every level of every tree descent allocates a page-sized buffer,
//! issues a device read, and decodes the page again — on every statement. A
//! point read by primary key pays that three or four times over before it has
//! looked at a single row. This cache removes the read and the decode for any
//! page it still holds.
//!
//! # Why no invalidation protocol is needed
//!
//! The tree is copy-on-write and its page allocator
//! ([`CowBTree::alloc_page`](super::tree::CowBTree)) is a monotonically
//! increasing counter that never hands out an id twice. A mutation therefore
//! writes *new* pages and leaves every live page exactly as it was, and a page
//! id names one immutable sequence of bytes for the lifetime of the file. Cache
//! key = page id; an entry can never go stale, so there is nothing to
//! invalidate and no version to check. This is decision **D4** in `docs/architecture.md`.
//!
//! # ⚠ The free list of Phase 2 item 6 breaks that — versioned, AHL-481
//!
//! **Reusing page ids invalidates the assumption this whole module rests on.**
//! The moment a freed page id can be handed out again — a free list, a vacuum
//! that compacts the data area, or anything else that recycles ids — a cached
//! entry for that id may describe the *previous* occupant of the page, and the
//! tree will happily serve the wrong node with no error anywhere: no checksum
//! fails, no decode fails, the read is simply wrong.
//!
//! AHL-481 (`CowBTree::set_page_reuse`) is what reclaims a page id, and it
//! versions the cache — coarser than the `(page id, commit sequence)` stamp
//! this warning originally asked for, and on purpose: every point where a
//! handle's committed root moves forward clears this cache (and the retained
//! read cursor, AHL-472) whenever reuse is on, rather than checking a
//! per-entry stamp on every lookup. "The cache is empty" is trivial to prove
//! correct; "every lookup path honours a stamp everywhere it could read one"
//! is not, and a more precise design that tried to read a durable "has
//! anything been reused" counter to decide *whether* to clear turned out to
//! be circular — answering that question needed a tree read, which itself
//! goes through this cache. See `CowBTree::invalidate_for_reuse` and
//! `docs/recovery.md`'s "The free list and page reuse" section for the full
//! design, its two-part durability-and-liveness reclaim proof, the
//! cross-process constraint (a read-only handle takes no lock and is
//! invisible to the liveness proof, so this is a handle-level opt-in, never
//! a default), and the reuse-specific DST sweep
//! (`free_list_reuse_dst.rs`) that exercises it against the same crash/
//! torn-write fault schedule the rest of this suite runs. With reuse off —
//! the default, and every existing caller — this cache is exactly what it
//! was before AHL-481: no entry is ever cleared for this reason, and no cost
//! is paid to decide not to.
//!
//! It has already been reached once *without* a free list. AHL-406: a `sync`
//! that reported success without reaching the platter left the committed state
//! naming an older commit than the handle had already written pages for, and
//! the handle rewound its page allocator along with its root — so the next
//! commit issued ids that were still occupied, and this cache served the
//! previous occupant of each. The database recovered to a tree made of two
//! different commits at once. Nothing errored; both timelines had written
//! well-formed, correctly checksummed pages. The invariant is now stated and
//! enforced in one place,
//! [`CowBTree::adopt_next_page_id`](super::tree::CowBTree): the allocator is
//! monotonic per handle and never rewinds, however far back the committed state
//! goes.
//!
//! # What is never cached
//!
//! Only *data-area* pages are immutable. The header, the state block and the
//! WAL regions are rewritten in place — the state block at every checkpoint,
//! a WAL region at every commit — so caching them would be exactly the bug
//! above. [`data_area_page`] is the guard, and it derives the boundary from
//! [`crate::wal`]'s own offset helpers rather than assuming where it is.
//!
//! Uncommitted pages are not cached either: a transaction's copied pages live
//! in the tree's `dirty` map until the commit that makes them real, and a
//! transaction that conflicts throws them away. Only
//! `CowBTree::committed_node` fills this cache, and it only ever reads the
//! data area.
//!
//! # Memory
//!
//! The bound is a byte budget over *decoded* nodes, not over raw pages, so it
//! is compared against an estimate of what an entry actually costs on the heap
//! (see [`node_footprint`]). It is resident memory a caller did not pay before
//! the cache existed: an engine opened with the default budget can hold
//! [`DEFAULT_PAGE_CACHE_BYTES`] of decoded pages per open database handle, on
//! top of everything else. Callers that care set
//! [`EngineOptions::page_cache_bytes`](crate::EngineOptions) — `0` disables the
//! cache entirely and restores the old read-every-time behaviour.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::mem::size_of;

use super::page::{Entry, Key, Node, PageId, Separator, ValueRef};

/// Default page cache budget, in bytes, for one open database handle.
///
/// Eight mebibytes holds roughly two thousand decoded 4 KiB pages, which is
/// the whole of a small database and the upper levels of a large one. It is
/// resident memory per handle — see the module docs.
pub const DEFAULT_PAGE_CACHE_BYTES: usize = 8 << 20;

/// Per-allocation overhead charged on top of the bytes actually asked for, so
/// the budget is not wildly optimistic about a node made of many small `Vec`s.
/// A guess at malloc bookkeeping, deliberately on the generous side.
const ALLOC_OVERHEAD: usize = 16;

/// An estimate of the heap a decoded node occupies, used to bound the cache.
///
/// It is an estimate and says so: the exact cost depends on the allocator's
/// size classes. It counts the `Vec` of cells, each cell's key, and each
/// inline value, plus a constant per allocation.
pub fn node_footprint(node: &Node) -> usize {
    let mut total = size_of::<Node>() + ALLOC_OVERHEAD + node.bytes().len() + ALLOC_OVERHEAD;
    match node {
        Node::Leaf { entries, .. } => {
            total += entries.len() * size_of::<Entry>() + ALLOC_OVERHEAD;
            for entry in entries {
                if let Key::Owned(bytes) = &entry.key {
                    total += bytes.len() + ALLOC_OVERHEAD;
                }
                if let ValueRef::Inline(bytes) = &entry.value {
                    total += bytes.len() + ALLOC_OVERHEAD;
                }
            }
            total
        }
        Node::Internal { cells, .. } => {
            total += cells.len() * size_of::<Separator>() + ALLOC_OVERHEAD;
            for cell in cells {
                if let Key::Owned(bytes) = &cell.key {
                    total += bytes.len() + ALLOC_OVERHEAD;
                }
            }
            total
        }
    }
}

/// Whether page `id` really lives in the data area of a file with this page
/// size and format, and may therefore be cached.
///
/// The data area starts after the header, the state block and every WAL
/// region, and the answer is computed from [`crate::wal`]'s offset helpers so
/// it stays correct if the layout moves. In today's layout page id 0 maps
/// *inside* the last WAL region — it is the "empty tree" sentinel and is never
/// stored — which is exactly the kind of overlap this guard exists to catch.
pub fn data_area_page(page_size: usize, format_version: u32, id: PageId) -> bool {
    crate::wal::data_offset_for(page_size, format_version, id)
        >= crate::wal::all_regions_end(page_size, format_version)
}

/// One resident page.
struct Slot {
    id: PageId,
    node: Rc<Node>,
    footprint: usize,
    /// The clock/second-chance "referenced" bit. Set on every hit; cleared by
    /// the hand as it sweeps past looking for a victim. See the struct docs on
    /// [`PageCache`] for why a hit only ever touches this one `bool`.
    referenced: bool,
}

/// A cache of decoded committed pages, bounded by an estimated byte budget,
/// evicted under a clock (second-chance) policy.
///
/// It follows [`crate::hnsw_paged`]'s `NodeCache` in shape — a `BTreeMap` index
/// over `alloc`-only storage, no dependency, no `unsafe`. Eviction order used
/// to be exact LRU, kept as an intrusive doubly-linked list that every hit
/// re-linked to the most-recently-used end. That relink was the single hottest
/// function in a join profile (`PERF.md`, "The join and range profile"): a
/// join descends per outer row, so it was paid `depth × outer_rows` times, all
/// to maintain an order eviction only ever reads once it actually needs a
/// victim.
///
/// Clock trades exact recency for a hit that touches nothing but its own slot:
/// [`PageCache::get`] just sets a `referenced` bit — no list surgery, so a hit
/// is one `BTreeMap` lookup and one `bool` write, independent of how many
/// other entries are resident. Eviction is where the cost moves to: a `hand`
/// sweeps the slot array looking for a victim, clearing `referenced` bits it
/// passes over (giving that entry a "second chance") and evicting the first
/// slot it finds already clear. A slot's bit can be cleared and re-set several
/// times before it is finally chosen, so this approximates recency — a page
/// hit since the hand last passed survives another lap — without ever paying
/// for it on the hit path, which is the one this cache exists to make cheap.
pub struct PageCache {
    /// Byte budget. `0` disables the cache.
    budget: usize,
    /// Estimated bytes currently resident.
    bytes: usize,
    /// Slot storage. A `None` is a free slot, recycled through `free`.
    slots: Vec<Option<Slot>>,
    /// Indices of free slots.
    free: Vec<usize>,
    /// Page id to slot index.
    index: BTreeMap<PageId, usize>,
    /// The clock hand: the next slot index eviction will examine.
    hand: usize,
}

impl PageCache {
    /// A cache bounded by `budget` bytes of decoded pages. `0` disables it.
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            bytes: 0,
            slots: Vec::new(),
            free: Vec::new(),
            index: BTreeMap::new(),
            hand: 0,
        }
    }

    /// The byte budget resident entries are held under.
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Estimated bytes currently resident.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// How many pages are resident.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether the cache holds nothing.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.free.clear();
        self.index.clear();
        self.hand = 0;
        self.bytes = 0;
    }

    /// Change the budget, evicting under the clock policy until the new one
    /// holds.
    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        while self.bytes > self.budget && self.evict_one() {}
    }

    /// The cached node for `id`, if it is resident, giving it its "referenced"
    /// bit — its second chance the next time the clock hand passes it.
    ///
    /// This is the whole point of the clock policy: a hit is one `BTreeMap`
    /// lookup and one `bool` write, touching no other entry, where the LRU
    /// list this replaced had to detach and re-link a node on every hit.
    pub fn get(&mut self, id: PageId) -> Option<Rc<Node>> {
        let slot = *self.index.get(&id)?;
        let entry = self.slots.get_mut(slot)?.as_mut()?;
        entry.referenced = true;
        Some(Rc::clone(&entry.node))
    }

    /// Make `node` resident under `id`, evicting under the clock policy until
    /// the budget holds.
    ///
    /// A node whose own footprint exceeds the whole budget is not cached — it
    /// would evict everything else and then not fit.
    pub fn insert(&mut self, id: PageId, node: Rc<Node>) {
        if self.budget == 0 {
            return;
        }
        if let Some(&slot) = self.index.get(&id) {
            // Already resident. The bytes cannot have changed (a page id names
            // one immutable page), so this is only a reference-bit update —
            // the insert path counts as a touch too.
            if let Some(entry) = self.slots.get_mut(slot).and_then(Option::as_mut) {
                entry.referenced = true;
            }
            return;
        }
        let footprint = node_footprint(&node);
        if footprint > self.budget {
            return;
        }
        while self.bytes + footprint > self.budget && self.evict_one() {}
        if self.bytes + footprint > self.budget {
            return;
        }

        // Starts unreferenced: a page earns its first second-chance by being
        // hit before the hand reaches it, same as every other entry.
        let slot = Slot {
            id,
            node,
            footprint,
            referenced: false,
        };
        let at = match self.free.pop() {
            Some(at) => {
                match self.slots.get_mut(at) {
                    Some(place) => *place = Some(slot),
                    // Unreachable: `free` only ever holds indices into `slots`.
                    // Dropping the entry is the safe answer if it ever is.
                    None => return,
                }
                at
            }
            None => {
                self.slots.push(Some(slot));
                self.slots.len() - 1
            }
        };
        self.index.insert(id, at);
        self.bytes += footprint;
    }

    /// Evict one entry under the clock policy: sweep the hand forward,
    /// clearing the `referenced` bit of every occupied slot it passes and
    /// evicting the first one it finds already clear. `false` when nothing is
    /// resident to evict.
    ///
    /// Bounded to two full sweeps of the slot array: the first clears every
    /// remaining `referenced` bit it meets, so a second sweep is guaranteed to
    /// find a victim (or, if the cache is genuinely empty of occupied slots,
    /// terminate having found none).
    fn evict_one(&mut self) -> bool {
        let n = self.slots.len();
        if n == 0 || self.index.is_empty() {
            return false;
        }
        for _ in 0..(2 * n) {
            let at = self.hand;
            self.hand = (self.hand + 1) % n;
            let Some(place) = self.slots.get_mut(at) else {
                continue;
            };
            let Some(entry) = place.as_mut() else {
                continue;
            };
            if entry.referenced {
                entry.referenced = false;
                continue;
            }
            let slot = place.take().expect("just confirmed occupied above");
            self.index.remove(&slot.id);
            self.bytes = self.bytes.saturating_sub(slot.footprint);
            self.free.push(at);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn leaf(key: &[u8]) -> Rc<Node> {
        Rc::new(Node::Leaf {
            bytes: Rc::from(&[][..]),
            entries: vec![Entry {
                key: Key::Owned(key.to_vec()),
                value: ValueRef::Owned(Rc::from(vec![0u8; 32])),
            }],
        })
    }

    fn cache_of(entries: usize) -> PageCache {
        PageCache::new(node_footprint(&leaf(b"k")) * entries)
    }

    #[test]
    fn a_hit_returns_the_same_node_and_a_miss_returns_nothing() {
        let mut cache = cache_of(4);
        cache.insert(1, leaf(b"one"));
        assert_eq!(cache.get(1).as_deref(), Some(&*leaf(b"one")));
        assert!(cache.get(2).is_none());
    }

    #[test]
    fn the_least_recently_used_page_is_the_one_evicted() {
        // Every key here is the same length, and the budget is measured from
        // that same node. The cache is budgeted in *bytes*, so a longer key
        // means a larger footprint: sizing the budget from a one-byte key and
        // then inserting three-byte ones — or inserting a five-byte `three`
        // last — evicts on size rather than on recency, and the test stops
        // being about the LRU order it claims to check.
        let mut cache = PageCache::new(node_footprint(&leaf(b"one")) * 2);
        cache.insert(1, leaf(b"one"));
        cache.insert(2, leaf(b"two"));
        // Touching 1 makes 2 the eviction candidate.
        assert!(cache.get(1).is_some());
        cache.insert(3, leaf(b"thr"));
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
        assert!(cache.get(3).is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn a_referenced_page_gets_one_extra_sweep_but_not_immunity() {
        // Same shape as `the_least_recently_used_page_is_the_one_evicted`,
        // but goes one eviction further to show *why* it still passes under
        // clock: a touch buys a page one more lap of the hand, not a
        // permanent place — the thing that actually distinguishes second
        // chance from a naive FIFO that would evict id 1 immediately, and
        // from a real LRU that would keep id 1 resident indefinitely as long
        // as nothing else is touched.
        let mut cache = PageCache::new(node_footprint(&leaf(b"one")) * 2);
        cache.insert(1, leaf(b"one"));
        cache.insert(2, leaf(b"two"));
        assert!(cache.get(1).is_some()); // 1 is referenced; 2 is not.
        cache.insert(3, leaf(b"thr")); // Evicts 2; 1's bit is cleared, not 1.
        assert!(cache.get(3).is_some());
        // 1 was not touched again since its bit was cleared above, so this
        // eviction takes it — the second chance was spent, not renewed.
        cache.insert(4, leaf(b"fou"));
        assert!(cache.get(1).is_none());
        assert!(cache.get(3).is_some());
        assert!(cache.get(4).is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn eviction_recycles_slots_instead_of_growing_forever() {
        let mut cache = cache_of(2);
        for id in 0..64 {
            cache.insert(id, leaf(b"k"));
        }
        assert_eq!(cache.len(), 2);
        assert!(cache.bytes() <= cache.budget());
        // Two live slots, so the slab never needed more than two.
        assert_eq!(cache.slots.len(), 2);
    }

    #[test]
    fn a_zero_budget_caches_nothing() {
        let mut cache = PageCache::new(0);
        cache.insert(1, leaf(b"one"));
        assert!(cache.get(1).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn a_node_larger_than_the_whole_budget_is_not_cached() {
        let mut cache = PageCache::new(8);
        cache.insert(1, leaf(b"one"));
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn shrinking_the_budget_evicts_down_to_it() {
        let mut cache = cache_of(4);
        for id in 1..=4 {
            cache.insert(id, leaf(b"k"));
        }
        assert_eq!(cache.len(), 4);
        cache.set_budget(cache.budget() / 2);
        assert_eq!(cache.len(), 2);
        // The two most recently inserted survive.
        assert!(cache.get(3).is_some());
        assert!(cache.get(4).is_some());
    }

    #[test]
    fn reinserting_a_resident_page_does_not_double_count_its_bytes() {
        let mut cache = cache_of(4);
        cache.insert(1, leaf(b"one"));
        let bytes = cache.bytes();
        cache.insert(1, leaf(b"one"));
        assert_eq!(cache.bytes(), bytes);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn clearing_drops_everything() {
        let mut cache = cache_of(4);
        cache.insert(1, leaf(b"one"));
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
        assert!(cache.get(1).is_none());
    }

    #[test]
    fn page_zero_is_not_a_data_area_page_but_page_one_is() {
        // Page 0 is the "empty tree" sentinel and its offset lands inside the
        // last WAL region: caching it would cache a block that is rewritten in
        // place at every commit.
        for version in [4, 5] {
            assert!(!data_area_page(4096, version, 0));
            assert!(data_area_page(4096, version, 1));
            assert!(data_area_page(4096, version, 1_000_000));
        }
    }

    #[test]
    fn no_cacheable_page_overlaps_the_header_state_or_wal_regions() {
        for version in [4u32, 5] {
            for page_size in [64usize, 256, 4096] {
                let reserved_end = crate::wal::all_regions_end(page_size, version);
                for id in 0..8u64 {
                    if !data_area_page(page_size, version, id) {
                        continue;
                    }
                    let offset = crate::wal::data_offset_for(page_size, version, id);
                    assert!(offset >= reserved_end);
                    assert!(offset >= crate::wal::wal_end(page_size));
                    assert!(offset > crate::wal::state_offset(page_size));
                    assert!(offset > crate::wal::header_offset());
                }
            }
        }
    }
}
