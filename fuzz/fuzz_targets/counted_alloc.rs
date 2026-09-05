//! A global allocator that counts, so "this parse allocated too much" is a
//! crash with an input file rather than a number nobody reads.
//!
//! Included by the four `server_*` targets. The property they assert —
//! *allocation is proportional to the bytes that arrived, not to a number in
//! the header* — cannot be checked by anything outside the process. libFuzzer
//! has `-rss_limit_mb` and the Trust workflow pins it at 2048, but a
//! process-level limit reports that the fuzzer died; it does not report that
//! four bytes became sixteen mebibytes, and it does not name the input that
//! did it. That distinction is the whole reason this file exists, and it is
//! the lesson AHL-500 taught the hard way: the first sighting of an
//! exponential parse was a 46-minute job with no named input, because the only
//! instrument watching was a budget around the harness instead of an assertion
//! inside it.
//!
//! Measured as *live bytes above a baseline taken immediately before the call*
//! rather than as bytes ever allocated. A decoder that allocates a kilobyte
//! and frees it a thousand times is doing something reasonable; one that holds
//! sixteen mebibytes is not, and it is the holding that costs a
//! thread-per-connection server its memory.
//!
//! The counters are `Relaxed` on purpose. libFuzzer drives one input on one
//! thread, the numbers are read after the call that wrote them, and ordering
//! stronger than this would put a fence on every allocation in a harness whose
//! throughput is the budget.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Bytes currently allocated and not yet freed.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The high-water mark of `LIVE` since [`start`].
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// What `LIVE` was when [`start`] last ran.
static BASE: AtomicUsize = AtomicUsize::new(0);

/// `System`, plus a running total.
pub struct Counting;

fn allocated(size: usize) {
    let live = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: every method forwards to `System` unchanged and only adds two
// relaxed integer updates around it, so the allocator's contract is whatever
// `System`'s is.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            allocated(new_size);
        }
        moved
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }
}

/// Begin a measurement. Everything allocated before this is the baseline.
pub fn start() {
    let live = LIVE.load(Relaxed);
    BASE.store(live, Relaxed);
    PEAK.store(live, Relaxed);
}

/// The most this process held at once since [`start`], above the baseline.
pub fn peak() -> usize {
    PEAK.load(Relaxed).saturating_sub(BASE.load(Relaxed))
}
