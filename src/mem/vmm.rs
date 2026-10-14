//! Virtual memory: edits the live 4-level page tables (the ones Limine set up).
//! Page tables are reached through the higher-half direct map.

use super::{phys_to_virt, pmm};
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
const HUGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const MSR_EFER: u32 = 0xC000_0080;
const EFER_NXE: u64 = 1 << 11;

#[derive(Debug)]
pub enum MapError {
    OutOfMemory,
    AlreadyMapped,
    /// A 1 GiB or 2 MiB page already covers this address.
    HugePage,
}

fn read_cr3() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value & ADDR_MASK
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

/// Flags for ordinary kernel data: writable, and non-executable if the CPU
/// has NX enabled (setting bit 63 without EFER.NXE would fault).
pub fn data_flags() -> u64 {
    if read_msr(MSR_EFER) & EFER_NXE != 0 {
        WRITABLE | NO_EXECUTE
    } else {
        WRITABLE
    }
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

/// Map the 4 KiB page at `virt` to the frame at `phys`, creating any missing
/// intermediate tables from the physical allocator.
pub fn map(virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    assert!(virt % 4096 == 0 && phys % 4096 == 0, "vmm: unaligned map");
    let idx = indices(virt);
    let mut table = read_cr3();

    for level in 0..3 {
        let slot = unsafe { entry(table, idx[level]) };
        let value = unsafe { read_volatile(slot) };
        if value & PRESENT == 0 {
            let frame = pmm::alloc_zeroed_frame().ok_or(MapError::OutOfMemory)?;
            unsafe { write_volatile(slot, frame | PRESENT | WRITABLE) };
            table = frame;
        } else if value & HUGE != 0 {
            return Err(MapError::HugePage);
        } else {
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

/// Walk the page tables and return the physical address `virt` maps to,
/// understanding 1 GiB and 2 MiB pages.
pub fn translate(virt: u64) -> Option<u64> {
    let idx = indices(virt);
    let mut table = read_cr3();

    for level in 0..4 {
        let value = unsafe { read_volatile(entry(table, idx[level])) };
        if value & PRESENT == 0 {
            return None;
        }
        if level == 1 && value & HUGE != 0 {
            return Some((value & ADDR_MASK & !0x3FFF_FFFF) | (virt & 0x3FFF_FFFF));
        }
        if level == 2 && value & HUGE != 0 {
            return Some((value & ADDR_MASK & !0x1F_FFFF) | (virt & 0x1F_FFFF));
        }
        if level == 3 {
            return Some((value & ADDR_MASK) | (virt & 0xFFF));
        }
        table = value & ADDR_MASK;
    }
    None
}
