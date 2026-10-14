//! Kernel heap: a dedicated virtual region, mapped page by page from physical
//! frames, managed by an address-sorted free list that merges neighbours on free.

use super::{pmm, vmm, PAGE_SIZE};
use crate::sync::SpinLock;
use core::alloc::{GlobalAlloc, Layout};

/// PML4 slot 384, well clear of the HHDM and the kernel image.
pub const HEAP_START: u64 = 0xffff_c000_0000_0000;
pub const HEAP_SIZE: u64 = 4 * 1024 * 1024;

/// Every block (and every gap between blocks) is a multiple of this, so a free
/// block can always hold a FreeNode and no unusably small fragments appear.
const GRANULE: usize = 16;

#[repr(C)]
struct FreeNode {
    size: usize,
    next: *mut FreeNode,
}

struct Heap {
    /// Dummy list head (size 0); `head.next` is the lowest free block.
    head: FreeNode,
    used: usize,
}

unsafe impl Send for Heap {}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap {
    head: FreeNode {
        size: 0,
        next: core::ptr::null_mut(),
    },
    used: 0,
});

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

impl Heap {
    /// Insert [addr, addr+size) into the sorted free list, merging with
    /// the neighbouring blocks when they touch.
    unsafe fn add_free(&mut self, addr: usize, size: usize) {
        let head: *mut FreeNode = &mut self.head;
        let mut prev = head;
        while !(*prev).next.is_null() && ((*prev).next as usize) < addr {
            prev = (*prev).next;
        }
        let next = (*prev).next;

        let node = addr as *mut FreeNode;
        (*node).size = size;
        (*node).next = next;
        (*prev).next = node;

        if !next.is_null() && addr + size == next as usize {
            (*node).size += (*next).size;
            (*node).next = (*next).next;
        }
        if prev != head && prev as usize + (*prev).size == addr {
            (*prev).size += (*node).size;
            (*prev).next = (*node).next;
        }
    }

    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let size = align_up(layout.size().max(1), GRANULE);
        let align = layout.align().max(GRANULE);

        let mut prev: *mut FreeNode = &mut self.head;
        while !(*prev).next.is_null() {
            let cur = (*prev).next;
            let cur_start = cur as usize;
            let cur_end = cur_start + (*cur).size;
            let next = (*cur).next;

            let start = align_up(cur_start, align);
            let end = start + size;
            if end <= cur_end {
                let pad = start - cur_start; // 0, or at least GRANULE
                let tail = cur_end - end; // 0, or at least GRANULE

                if pad == 0 {
                    // Block begins exactly at the allocation: replace it with its tail.
                    let replacement = if tail > 0 {
                        let t = end as *mut FreeNode;
                        (*t).size = tail;
                        (*t).next = next;
                        t
                    } else {
                        next
                    };
                    (*prev).next = replacement;
                } else {
                    // Keep the front gap as a free block, and add the tail after it.
                    (*cur).size = pad;
                    if tail > 0 {
                        let t = end as *mut FreeNode;
                        (*t).size = tail;
                        (*t).next = next;
                        (*cur).next = t;
                    }
                }
                self.used += size;
                return start as *mut u8;
            }
            prev = cur;
        }
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let size = align_up(layout.size().max(1), GRANULE);
        self.used -= size;
        self.add_free(ptr as usize, size);
    }
}

pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP.lock().alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HEAP.lock().dealloc(ptr, layout)
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

pub fn init() {
    let flags = vmm::data_flags();
    for page in 0..HEAP_SIZE / PAGE_SIZE {
        let frame = pmm::alloc_frame().expect("heap: out of physical memory");
        vmm::map(HEAP_START + page * PAGE_SIZE, frame, flags).expect("heap: map failed");
    }
    unsafe { HEAP.lock().add_free(HEAP_START as usize, HEAP_SIZE as usize) };
}

/// Bytes currently allocated from the heap.
pub fn used() -> usize {
    HEAP.lock().used
}
