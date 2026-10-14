//! Physical frame allocator: one bit per 4 KiB frame (1 = in use).
//! The bitmap itself lives in the first usable region large enough to hold it.

use super::{align_down, align_up, phys_to_virt, PAGE_SIZE};
use crate::limine::{MemmapEntry, MEMMAP_USABLE};
use crate::sync::SpinLock;

struct Pmm {
    bitmap: *mut u8,
    /// Frames covered by the bitmap (everything below the highest usable address).
    frames: usize,
    /// Where the next search starts, so repeated allocations stay cheap.
    hint: usize,
    free: usize,
    usable: usize,
}

unsafe impl Send for Pmm {}

static PMM: SpinLock<Pmm> = SpinLock::new(Pmm {
    bitmap: core::ptr::null_mut(),
    frames: 0,
    hint: 0,
    free: 0,
    usable: 0,
});

impl Pmm {
    fn is_used(&self, frame: usize) -> bool {
        unsafe { *self.bitmap.add(frame / 8) & (1 << (frame % 8)) != 0 }
    }

    fn set_used(&mut self, frame: usize) {
        unsafe { *self.bitmap.add(frame / 8) |= 1 << (frame % 8) };
    }

    fn set_free(&mut self, frame: usize) {
        unsafe { *self.bitmap.add(frame / 8) &= !(1 << (frame % 8)) };
    }

    fn alloc(&mut self) -> Option<u64> {
        if self.free == 0 {
            return None;
        }
        let bytes = (self.frames + 7) / 8;
        let start = self.hint / 8;
        for i in 0..bytes {
            let byte = (start + i) % bytes;
            let value = unsafe { *self.bitmap.add(byte) };
            if value == 0xFF {
                continue;
            }
            let frame = byte * 8 + (!value).trailing_zeros() as usize;
            if frame >= self.frames {
                continue;
            }
            self.set_used(frame);
            self.free -= 1;
            self.hint = frame;
            return Some(frame as u64 * PAGE_SIZE);
        }
        None
    }

    /// `count` consecutive free frames, above the first MiB (legacy low memory).
    fn alloc_contiguous(&mut self, count: usize) -> Option<u64> {
        if count == 0 || self.free < count {
            return None;
        }
        let (mut run, mut start) = (0usize, 0usize);
        for frame in 0x100..self.frames {
            if self.is_used(frame) {
                run = 0;
                continue;
            }
            if run == 0 {
                start = frame;
            }
            run += 1;
            if run == count {
                for f in start..start + count {
                    self.set_used(f);
                }
                self.free -= count;
                return Some(start as u64 * PAGE_SIZE);
            }
        }
        None
    }

    fn free_frame(&mut self, phys: u64) {
        assert!(phys % PAGE_SIZE == 0, "pmm: unaligned free {:#x}", phys);
        let frame = (phys / PAGE_SIZE) as usize;
        assert!(frame < self.frames, "pmm: free of unmanaged frame {:#x}", phys);
        assert!(self.is_used(frame), "pmm: double free of frame {:#x}", phys);
        self.set_free(frame);
        self.free += 1;
        if frame < self.hint {
            self.hint = frame;
        }
    }
}

pub fn init(entries: &[*const MemmapEntry]) {
    let usable = |e: &MemmapEntry| e.kind == MEMMAP_USABLE;

    let mut top = 0u64;
    for &entry in entries {
        let e = unsafe { &*entry };
        if usable(e) {
            top = top.max(e.base + e.length);
        }
    }
    let frames = (top / PAGE_SIZE) as usize;
    let bitmap_bytes = ((frames + 7) / 8) as u64;

    // Put the bitmap in the first usable region (above 1 MiB) that can hold it.
    let mut bitmap_phys = None;
    for &entry in entries {
        let e = unsafe { &*entry };
        if usable(e) && e.base >= 0x10_0000 && e.length >= bitmap_bytes {
            bitmap_phys = Some(e.base);
            break;
        }
    }
    let bitmap_phys = bitmap_phys.expect("pmm: no usable region large enough for the bitmap");

    let mut pmm = PMM.lock();
    pmm.bitmap = phys_to_virt(bitmap_phys);
    pmm.frames = frames;

    // Start with every frame marked used, then release the usable ones.
    unsafe { core::ptr::write_bytes(pmm.bitmap, 0xFF, bitmap_bytes as usize) };
    for &entry in entries {
        let e = unsafe { &*entry };
        if !usable(e) {
            continue;
        }
        let first = align_up(e.base, PAGE_SIZE) / PAGE_SIZE;
        let last = align_down(e.base + e.length, PAGE_SIZE) / PAGE_SIZE;
        for frame in first..last {
            pmm.set_free(frame as usize);
            pmm.free += 1;
            pmm.usable += 1;
        }
    }

    // The bitmap's own frames, and frame 0 (null-pointer guard), are not allocatable.
    let bitmap_first = (bitmap_phys / PAGE_SIZE) as usize;
    let bitmap_frames = align_up(bitmap_bytes, PAGE_SIZE) / PAGE_SIZE;
    for frame in bitmap_first..bitmap_first + bitmap_frames as usize {
        if !pmm.is_used(frame) {
            pmm.set_used(frame);
            pmm.free -= 1;
        }
    }
    if !pmm.is_used(0) {
        pmm.set_used(0);
        pmm.free -= 1;
    }
}

/// Allocate one 4 KiB frame; returns its physical address.
pub fn alloc_frame() -> Option<u64> {
    PMM.lock().alloc()
}

/// Like `alloc_frame`, but the frame is zero-filled.
pub fn alloc_zeroed_frame() -> Option<u64> {
    let phys = alloc_frame()?;
    unsafe { core::ptr::write_bytes(phys_to_virt(phys), 0, PAGE_SIZE as usize) };
    Some(phys)
}

pub fn free_frame(phys: u64) {
    PMM.lock().free_frame(phys);
}

/// `count` physically consecutive, zero-filled frames (for DMA structures that
/// must not be scattered). Returns the physical address of the first.
pub fn alloc_contiguous_zeroed(count: usize) -> Option<u64> {
    let phys = PMM.lock().alloc_contiguous(count)?;
    unsafe { core::ptr::write_bytes(phys_to_virt(phys), 0, count * PAGE_SIZE as usize) };
    Some(phys)
}

/// (free frames, total usable frames)
pub fn stats() -> (u64, u64) {
    let pmm = PMM.lock();
    (pmm.free as u64, pmm.usable as u64)
}
