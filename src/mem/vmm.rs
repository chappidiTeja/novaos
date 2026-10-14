//! Virtual memory: edits 4-level page tables, both the live ones and those of
//! other address spaces. Page tables are reached through the higher-half
//! direct map.
//!
//! Address space layout: the lower half (PML4 slots 0-255) belongs to a process;
//! the upper half (slots 256-511) is the kernel and is shared by all of them.
//! Every upper-half slot is created at boot so that later kernel mappings only
//! ever add entries to tables that all address spaces already share.

use super::{phys_to_virt, pmm};
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
/// Software bit (ignored by the CPU): the frame behind this page is not owned by
/// the address space, so tearing the address space down must not free it. Used
/// for the framebuffer and for shared-memory windows, whose frames are owned by
/// something else.
pub const NOFREE: u64 = 1 << 9;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const MSR_EFER: u32 = 0xC000_0080;
const EFER_NXE: u64 = 1 << 11;

/// First address that is not user memory (the end of the canonical lower half).
pub const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

#[derive(Debug)]
pub enum MapError {
    OutOfMemory,
    AlreadyMapped,
    /// A 1 GiB or 2 MiB page already covers this address.
    HugePage,
}

/// The PML4 Limine gave us; the master copy of the kernel half.
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

pub fn read_cr3() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value & ADDR_MASK
}

/// Switch to the address space whose PML4 is at `pml4`.
pub fn activate(pml4: u64) {
    unsafe { asm!("mov cr3, {}", in(reg) pml4, options(nostack, preserves_flags)) };
}

pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(Ordering::Relaxed)
}

fn invlpg(virt: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)) };
}

fn read_msr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
             options(nomem, nostack, preserves_flags));
    }
    ((hi as u64) << 32) | lo as u64
}

/// The no-execute bit if the CPU has NX enabled, else 0 (setting bit 63
/// without EFER.NXE would fault).
pub fn nx_flag() -> u64 {
    if read_msr(MSR_EFER) & EFER_NXE != 0 {
        NO_EXECUTE
    } else {
        0
    }
}

/// Flags for ordinary kernel data: writable and non-executable.
pub fn data_flags() -> u64 {
    WRITABLE | nx_flag()
}

/// Flags for a user page with the given permissions.
pub fn user_flags(writable: bool, executable: bool) -> u64 {
    let mut flags = USER;
    if writable {
        flags |= WRITABLE;
    }
    if !executable {
        flags |= nx_flag();
    }
    flags
}

fn indices(virt: u64) -> [usize; 4] {
    [
        ((virt >> 39) & 0x1FF) as usize,
        ((virt >> 30) & 0x1FF) as usize,
        ((virt >> 21) & 0x1FF) as usize,
        ((virt >> 12) & 0x1FF) as usize,
    ]
}

unsafe fn entry(table_phys: u64, index: usize) -> *mut u64 {
    (phys_to_virt(table_phys) as *mut u64).add(index)
}

/// Record the boot page tables as the kernel's master copy and make sure every
/// kernel-half PML4 slot exists, so address spaces created later share them all.
pub fn init() {
    let master = read_cr3();
    KERNEL_CR3.store(master, Ordering::Relaxed);
    for index in 256..512 {
        let slot = unsafe { entry(master, index) };
        if unsafe { read_volatile(slot) } & PRESENT == 0 {
            let frame = pmm::alloc_zeroed_frame().expect("vmm: out of memory for kernel PML4");
            unsafe { write_volatile(slot, frame | PRESENT | WRITABLE) };
        }
    }
}

/// Map the 4 KiB page at `virt` to the frame at `phys` in the address space whose
/// PML4 is `pml4`, creating missing intermediate tables from the frame allocator.
pub fn map_in(pml4: u64, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    assert!(virt % 4096 == 0 && phys % 4096 == 0, "vmm: unaligned map");
    assert!(
        flags & USER == 0 || virt < USER_SPACE_END,
        "vmm: user page in kernel space"
    );
    let idx = indices(virt);
    let mut table = pml4;

    for level in 0..3 {
        let slot = unsafe { entry(table, idx[level]) };
        let value = unsafe { read_volatile(slot) };
        if value & PRESENT == 0 {
            let frame = pmm::alloc_zeroed_frame().ok_or(MapError::OutOfMemory)?;
            unsafe { write_volatile(slot, frame | PRESENT | WRITABLE | (flags & USER)) };
            table = frame;
        } else if value & HUGE != 0 {
            return Err(MapError::HugePage);
        } else {
            // Every level must allow user access for a user page to be reachable.
            if flags & USER != 0 && value & USER == 0 {
                unsafe { write_volatile(slot, value | USER) };
            }
            table = value & ADDR_MASK;
        }
    }

    let slot = unsafe { entry(table, idx[3]) };
    if unsafe { read_volatile(slot) } & PRESENT != 0 {
        return Err(MapError::AlreadyMapped);
    }
    unsafe { write_volatile(slot, phys | flags | PRESENT) };
    invlpg(virt);
    Ok(())
}

/// `map_in` for the live address space.
pub fn map(virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    map_in(read_cr3(), virt, phys, flags)
}

/// Remove the mapping for the 4 KiB page at `virt`; returns the frame it pointed
/// to (the caller decides whether to free it). Intermediate tables are kept.
pub fn unmap(virt: u64) -> Option<u64> {
    let idx = indices(virt);
    let mut table = read_cr3();

    for level in 0..3 {
        let value = unsafe { read_volatile(entry(table, idx[level])) };
        if value & PRESENT == 0 || value & HUGE != 0 {
            return None;
        }
        table = value & ADDR_MASK;
    }

    let slot = unsafe { entry(table, idx[3]) };
    let value = unsafe { read_volatile(slot) };
    if value & PRESENT == 0 {
        return None;
    }
    unsafe { write_volatile(slot, 0) };
    invlpg(virt);
    Some(value & ADDR_MASK)
}

pub struct Translation {
    pub phys: u64,
    /// Every level of the walk allows ring-3 access.
    pub user: bool,
    /// Every level of the walk allows writes.
    pub writable: bool,
}

/// Walk the page tables of `pml4`, understanding 1 GiB and 2 MiB pages.
pub fn translate_in(pml4: u64, virt: u64) -> Option<Translation> {
    let idx = indices(virt);
    let mut table = pml4;
    let mut user = true;
    let mut writable = true;

    for level in 0..4 {
        let value = unsafe { read_volatile(entry(table, idx[level])) };
        if value & PRESENT == 0 {
            return None;
        }
        user &= value & USER != 0;
        writable &= value & WRITABLE != 0;

        let phys = if level == 1 && value & HUGE != 0 {
            Some((value & ADDR_MASK & !0x3FFF_FFFF) | (virt & 0x3FFF_FFFF))
        } else if level == 2 && value & HUGE != 0 {
            Some((value & ADDR_MASK & !0x1F_FFFF) | (virt & 0x1F_FFFF))
        } else if level == 3 {
            Some((value & ADDR_MASK) | (virt & 0xFFF))
        } else {
            None
        };
        if let Some(phys) = phys {
            return Some(Translation { phys, user, writable });
        }
        table = value & ADDR_MASK;
    }
    None
}

/// Physical address `virt` maps to in the live address space.
pub fn translate(virt: u64) -> Option<u64> {
    translate_in(read_cr3(), virt).map(|t| t.phys)
}

/// Create an empty address space: no user mappings, kernel half shared with
/// every other address space. Returns the PML4's physical address.
pub fn new_address_space() -> Option<u64> {
    let pml4 = pmm::alloc_zeroed_frame()?;
    let master = kernel_cr3();
    for index in 256..512 {
        unsafe {
            let value = read_volatile(entry(master, index));
            write_volatile(entry(pml4, index), value);
        }
    }
    Some(pml4)
}

/// Free everything a process owns: its mapped user pages, its page tables and
/// the PML4. Kernel-half tables are shared and left alone. The address space
/// must not be the live one.
pub fn destroy_address_space(pml4: u64) {
    assert!(pml4 != read_cr3(), "vmm: destroying the live address space");
    for i4 in 0..256 {
        let e4 = unsafe { read_volatile(entry(pml4, i4)) };
        if e4 & PRESENT == 0 {
            continue;
        }
        let pdpt = e4 & ADDR_MASK;
        for i3 in 0..512 {
            let e3 = unsafe { read_volatile(entry(pdpt, i3)) };
            if e3 & PRESENT == 0 || e3 & HUGE != 0 {
                continue;
            }
            let pd = e3 & ADDR_MASK;
            for i2 in 0..512 {
                let e2 = unsafe { read_volatile(entry(pd, i2)) };
                if e2 & PRESENT == 0 || e2 & HUGE != 0 {
                    continue;
                }
                let pt = e2 & ADDR_MASK;
                for i1 in 0..512 {
                    let e1 = unsafe { read_volatile(entry(pt, i1)) };
                    if e1 & PRESENT != 0 && e1 & NOFREE == 0 {
                        pmm::free_frame(e1 & ADDR_MASK);
                    }
                }
                pmm::free_frame(pt);
            }
            pmm::free_frame(pd);
        }
        pmm::free_frame(pdpt);
    }
    pmm::free_frame(pml4);
}
