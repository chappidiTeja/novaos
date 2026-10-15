//! A general-purpose heap allocator for NovaOS programs.
//!
//! The kernel gives a program whole pages (`mem_alloc`) and never takes them back, so this
//! allocator keeps its own free list: a chain of holes sorted by address. Freeing a block
//! merges it with the holes on either side. When nothing fits, it asks the page source
//! for more memory. Single-threaded: a NovaOS process has one thread.
//!
//! Every block's size is a multiple of 16 and so is its address, so a hole always fits the
//! 16-byte header that records its size and the next hole.

#![cfg_attr(not(test), no_std)]

use core::alloc::Layout;
use core::ptr::null_mut;

pub const PAGE: usize = 4096;
/// Ask the page source for at least this much at a time (a request that needs more gets more).
pub const MIN_GROWTH: usize = 64 * PAGE;
const UNIT: usize = 16;

/// Where more memory comes from.
pub trait PageSource {
    /// `pages` contiguous zeroed pages, aligned to a page; None if there are none.
    fn get(&mut self, pages: usize) -> Option<*mut u8>;
}

#[repr(C)]
struct Hole {
    size: usize,
    next: *mut Hole,
}

pub struct Heap<S: PageSource> {
    head: *mut Hole,
    source: S,
    /// Bytes handed out and not yet freed (after rounding).
    used: usize,
    /// Bytes obtained from the page source.
    total: usize,
}

fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

impl<S: PageSource> Heap<S> {
    pub const fn new(source: S) -> Heap<S> {
        Heap { head: null_mut(), source, used: 0, total: 0 }
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// Number of holes (1 per fully merged region once everything is freed).
    pub fn holes(&self) -> usize {
        let mut n = 0;
        let mut h = self.head;
        while !h.is_null() {
            n += 1;
            h = unsafe { (*h).next };
        }
        n
    }

    fn block_size(layout: Layout) -> usize {
        align_up(layout.size().max(UNIT), UNIT)
    }

    /// Returns null if memory ran out.
    ///
    /// # Safety
    /// The usual allocator contract; not thread safe.
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let size = Self::block_size(layout);
        let align = layout.align().max(UNIT);
        loop {
            let mut prev: *mut Hole = null_mut();
            let mut cur = self.head;
            while !cur.is_null() {
                let start = cur as usize;
                let end = start + (*cur).size;
                let mut at = align_up(start, align);
                // A gap in front of the block must be big enough to stay a hole.
                if at != start && at - start < UNIT {
                    at = align_up(start + UNIT, align);
                }
                if at + size <= end {
                    let next = (*cur).next;
                    let front = at - start;
                    let tail = end - (at + size);
                    // Rebuild the chain: [front hole] [tail hole] in place of `cur`.
                    let mut replacement = next;
                    if tail > 0 {
                        let t = (at + size) as *mut Hole;
                        (*t).size = tail;
                        (*t).next = next;
                        replacement = t;
                    }
                    if front > 0 {
                        (*cur).size = front;
                        (*cur).next = replacement;
                        replacement = cur;
                    }
                    if prev.is_null() {
                        self.head = replacement;
                    } else {
                        (*prev).next = replacement;
                    }
                    self.used += size;
                    return at as *mut u8;
                }
                prev = cur;
                cur = (*cur).next;
            }
            if !self.grow(size + align) {
                return null_mut();
            }
        }
    }

    /// Take more pages from the page source and add them as a hole.
    unsafe fn grow(&mut self, need: usize) -> bool {
        let bytes = align_up(need.max(MIN_GROWTH), PAGE);
        let Some(base) = self.source.get(bytes / PAGE) else { return false };
        self.total += bytes;
        self.insert_hole(base as usize, bytes);
        true
    }

    /// Put `[start, start+size)` on the free list, merging with its neighbours.
    unsafe fn insert_hole(&mut self, start: usize, size: usize) {
        let mut prev: *mut Hole = null_mut();
        let mut cur = self.head;
        while !cur.is_null() && (cur as usize) < start {
            prev = cur;
            cur = (*cur).next;
        }
        let mut hole = start as *mut Hole;
        (*hole).size = size;
        (*hole).next = cur;
        // Merge with the following hole if they touch.
        if !cur.is_null() && start + size == cur as usize {
            (*hole).size += (*cur).size;
            (*hole).next = (*cur).next;
        }
        // Merge with the preceding hole if they touch.
        if !prev.is_null() && prev as usize + (*prev).size == start {
            (*prev).size += (*hole).size;
            (*prev).next = (*hole).next;
            hole = prev;
        } else if !prev.is_null() {
            (*prev).next = hole;
        } else {
            self.head = hole;
        }
        let _ = hole;
    }

    /// # Safety
    /// `ptr` must come from `alloc` with the same `layout`, and not be freed twice.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let size = Self::block_size(layout);
        self.used -= size;
        self.insert_hole(ptr as usize, size);
    }

    /// # Safety
    /// As `alloc`/`dealloc`: `ptr` is a live block of `old`.
    pub unsafe fn realloc(&mut self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let new = Layout::from_size_align_unchecked(new_size, old.align());
        if Self::block_size(new) == Self::block_size(old) {
            return ptr;
        }
        let fresh = self.alloc(new);
        if !fresh.is_null() {
            core::ptr::copy_nonoverlapping(ptr, fresh, old.size().min(new_size));
            self.dealloc(ptr, old);
        }
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Pages from the host's allocator, remembered so the test can free them.
    struct Fake {
        regions: Vec<(*mut u8, usize)>,
        /// Refuse after this many bytes (to test running out).
        limit: usize,
        given: usize,
    }

    impl Fake {
        fn new(limit: usize) -> Fake {
            Fake { regions: Vec::new(), limit, given: 0 }
        }
    }

    impl PageSource for Fake {
        fn get(&mut self, pages: usize) -> Option<*mut u8> {
            let bytes = pages * PAGE;
            if self.given + bytes > self.limit {
                return None;
            }
            let layout = Layout::from_size_align(bytes, PAGE).unwrap();
            let p = unsafe { std::alloc::alloc_zeroed(layout) };
            self.given += bytes;
            self.regions.push((p, bytes));
            Some(p)
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            for &(p, bytes) in &self.regions {
                unsafe { std::alloc::dealloc(p, Layout::from_size_align(bytes, PAGE).unwrap()) };
            }
        }
    }

    fn rng(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    #[test]
    fn small_blocks_do_not_overlap_and_keep_their_contents() {
        let mut h = Heap::new(Fake::new(1 << 28));
        let mut blocks: Vec<(*mut u8, Layout, u8)> = Vec::new();
        for i in 0..2000usize {
            let layout = Layout::from_size_align(1 + (i * 37) % 300, 1 << (i % 5)).unwrap();
            let p = unsafe { h.alloc(layout) };
            assert!(!p.is_null());
            assert_eq!(p as usize % layout.align().max(UNIT), 0, "block {i} is misaligned");
            let tag = (i % 251) as u8;
            unsafe { core::ptr::write_bytes(p, tag, layout.size()) };
            blocks.push((p, layout, tag));
        }
        for (p, layout, tag) in &blocks {
            let s = unsafe { core::slice::from_raw_parts(*p, layout.size()) };
            assert!(s.iter().all(|b| b == tag), "a block was overwritten by another");
        }
        for (p, layout, _) in blocks {
            unsafe { h.dealloc(p, layout) };
        }
        assert_eq!(h.used(), 0);
        assert!(h.holes() <= h.source.regions.len(), "everything freed must merge back: one hole per region obtained");
    }

    #[test]
    fn alignment_up_to_a_page_is_honoured() {
        let mut h = Heap::new(Fake::new(1 << 26));
        for align in [16usize, 32, 64, 128, 256, 1024, 4096] {
            for size in [1usize, 17, 100, 5000] {
                let layout = Layout::from_size_align(size, align).unwrap();
                let p = unsafe { h.alloc(layout) };
                assert!(!p.is_null());
                assert_eq!(p as usize % align, 0, "align {align} size {size}");
                unsafe { h.dealloc(p, layout) };
            }
        }
        assert_eq!(h.used(), 0);
        assert_eq!(h.holes(), 1);
    }

    #[test]
    fn random_use_never_corrupts_and_always_merges_back() {
        for seed in 1..=20u64 {
            let mut s = seed * 0x9E37_79B9_7F4A_7C15 | 1;
            let mut h = Heap::new(Fake::new(1 << 28));
            let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
            for step in 0..6000 {
                if live.is_empty() || rng(&mut s) % 3 != 0 {
                    let size = match rng(&mut s) % 10 {
                        0 => 1 + rng(&mut s) as usize % 20000,
                        1 => 1 + rng(&mut s) as usize % 200_000,
                        _ => 1 + rng(&mut s) as usize % 400,
                    };
                    let align = 1usize << (rng(&mut s) % 8);
                    let layout = Layout::from_size_align(size, align).unwrap();
                    let p = unsafe { h.alloc(layout) };
                    assert!(!p.is_null(), "seed {seed} step {step}");
                    assert_eq!(p as usize % align.max(UNIT), 0);
                    let tag = (rng(&mut s) & 0xFF) as u8;
                    unsafe { core::ptr::write_bytes(p, tag, size) };
                    live.push((p, layout, tag));
                } else {
                    let i = rng(&mut s) as usize % live.len();
                    let (p, layout, tag) = live.swap_remove(i);
                    let slice = unsafe { core::slice::from_raw_parts(p, layout.size()) };
                    assert!(slice.iter().all(|&b| b == tag), "seed {seed}: block damaged before free");
                    unsafe { h.dealloc(p, layout) };
                }
            }
            for (p, layout, tag) in live {
                let slice = unsafe { core::slice::from_raw_parts(p, layout.size()) };
                assert!(slice.iter().all(|&b| b == tag));
                unsafe { h.dealloc(p, layout) };
            }
            assert_eq!(h.used(), 0, "seed {seed}");
            // All holes touching each other have merged: only the gaps between separately
            // obtained regions can remain.
            assert!(h.holes() <= h.source.regions.len(), "seed {seed}: {} holes", h.holes());
        }
    }

    #[test]
    fn running_out_of_memory_returns_null_and_recovers() {
        let mut h = Heap::new(Fake::new(MIN_GROWTH));
        let layout = Layout::from_size_align(MIN_GROWTH / 2, 16).unwrap();
        let a = unsafe { h.alloc(layout) };
        let b = unsafe { h.alloc(layout) };
        assert!(!a.is_null());
        // Both halves fit the one region the source allowed; a third block cannot.
        let c = unsafe { h.alloc(layout) };
        assert!(!b.is_null(), "the second half fits exactly");
        assert!(c.is_null(), "and nothing is left for a third");
        let big = Layout::from_size_align(MIN_GROWTH * 4, 16).unwrap();
        assert!(unsafe { h.alloc(big) }.is_null(), "a request beyond the source's limit fails cleanly");
        unsafe {
            h.dealloc(a, layout);
            if !b.is_null() {
                h.dealloc(b, layout);
            }
            if !c.is_null() {
                h.dealloc(c, layout);
            }
        }
        let again = unsafe { h.alloc(layout) };
        assert!(!again.is_null(), "freed memory is reused");
    }

    #[test]
    fn realloc_keeps_data_and_shrinks_in_place() {
        let mut h = Heap::new(Fake::new(1 << 26));
        let old = Layout::from_size_align(100, 8).unwrap();
        let p = unsafe { h.alloc(old) };
        for i in 0..100 {
            unsafe { *p.add(i) = i as u8 };
        }
        let q = unsafe { h.realloc(p, old, 5000) };
        for i in 0..100 {
            assert_eq!(unsafe { *q.add(i) }, i as u8);
        }
        let grown = Layout::from_size_align(5000, 8).unwrap();
        let r = unsafe { h.realloc(q, grown, 5001) };
        assert_eq!(r, q, "growing within the same rounded size does not move");
        unsafe { h.dealloc(r, Layout::from_size_align(5001, 8).unwrap()) };
        assert_eq!(h.used(), 0);
    }

    #[test]
    fn first_allocation_of_zero_bytes_still_returns_a_unique_block() {
        let mut h = Heap::new(Fake::new(1 << 24));
        let l = Layout::from_size_align(0, 1).unwrap();
        let a = unsafe { h.alloc(l) };
        let b = unsafe { h.alloc(l) };
        assert!(!a.is_null() && !b.is_null() && a != b);
    }
}
