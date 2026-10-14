//! Loader for static x86-64 ELF executables (no interpreter, no relocations).

use crate::mem::{align_down, align_up, phys_to_virt, pmm, vmm, PAGE_SIZE};

#[derive(Debug)]
pub enum ElfError {
    Truncated,
    BadMagic,
    Unsupported(&'static str),
    BadSegment(&'static str),
    Overlap,
    TooBig,
    OutOfMemory,
}

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PHDR_SIZE: u64 = 56;

/// Programs load above the first pages (so null dereferences fault) and below
/// the stack region.
const LOAD_MIN: u64 = 0x1000;
const LOAD_MAX: u64 = 0x0000_7000_0000_0000;
/// Upper bound on the memory one image may claim (16 MiB).
const MAX_PAGES: u64 = 4096;

fn slice(bytes: &[u8], offset: u64, len: u64) -> Result<&[u8], ElfError> {
    let end = offset.checked_add(len).ok_or(ElfError::Truncated)?;
    bytes
        .get(offset as usize..end as usize)
        .ok_or(ElfError::Truncated)
}

fn u16_at(bytes: &[u8], offset: u64) -> Result<u16, ElfError> {
    Ok(u16::from_le_bytes(slice(bytes, offset, 2)?.try_into().unwrap()))
}

fn u32_at(bytes: &[u8], offset: u64) -> Result<u32, ElfError> {
    Ok(u32::from_le_bytes(slice(bytes, offset, 4)?.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], offset: u64) -> Result<u64, ElfError> {
    Ok(u64::from_le_bytes(slice(bytes, offset, 8)?.try_into().unwrap()))
}

/// Map every PT_LOAD segment of `image` into the address space `pml4`, and
/// return the entry point. On error the caller destroys the address space,
/// which frees whatever was mapped so far.
pub fn load(pml4: u64, image: &[u8]) -> Result<u64, ElfError> {
    if slice(image, 0, 4)? != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if image[4] != 2 {
        return Err(ElfError::Unsupported("not a 64-bit ELF"));
    }
    if image[5] != 1 {
        return Err(ElfError::Unsupported("not little-endian"));
    }
    if u16_at(image, 16)? != 2 {
        return Err(ElfError::Unsupported("not a static executable (ET_EXEC)"));
    }
    if u16_at(image, 18)? != 0x3E {
        return Err(ElfError::Unsupported("not x86-64"));
    }
    let entry = u64_at(image, 24)?;
    let phoff = u64_at(image, 32)?;
    if u16_at(image, 54)? as u64 != PHDR_SIZE {
        return Err(ElfError::Unsupported("unexpected program header size"));
    }
    let phnum = u16_at(image, 56)? as u64;

    let mut total_pages = 0u64;
    let mut entry_is_code = false;

    for i in 0..phnum {
        let ph = slice(image, phoff + i * PHDR_SIZE, PHDR_SIZE)?;
        if u32_at(ph, 0)? != PT_LOAD {
            continue;
        }
        let flags = u32_at(ph, 4)?;
        let offset = u64_at(ph, 8)?;
        let vaddr = u64_at(ph, 16)?;
        let filesz = u64_at(ph, 32)?;
        let memsz = u64_at(ph, 40)?;
        if memsz == 0 {
            continue;
        }

        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err(ElfError::BadSegment("segment is both writable and executable"));
        }
        if filesz > memsz {
            return Err(ElfError::BadSegment("file size exceeds memory size"));
        }
        let end = vaddr
            .checked_add(memsz)
            .ok_or(ElfError::BadSegment("segment wraps around"))?;
        if vaddr < LOAD_MIN || end > LOAD_MAX {
            return Err(ElfError::BadSegment("segment outside the user range"));
        }
        let file_bytes = slice(image, offset, filesz)?;
        if flags & PF_X != 0 && (vaddr..end).contains(&entry) {
            entry_is_code = true;
        }

        let map_flags = vmm::user_flags(flags & PF_W != 0, flags & PF_X != 0);
        let mut page = align_down(vaddr, PAGE_SIZE);
        let last = align_up(end, PAGE_SIZE);
        while page < last {
            total_pages += 1;
            if total_pages > MAX_PAGES {
                return Err(ElfError::TooBig);
            }
            let frame = pmm::alloc_zeroed_frame().ok_or(ElfError::OutOfMemory)?;
            if let Err(e) = vmm::map_in(pml4, page, frame, map_flags) {
                pmm::free_frame(frame);
                return Err(match e {
                    vmm::MapError::OutOfMemory => ElfError::OutOfMemory,
                    _ => ElfError::Overlap,
                });
            }

            // Fill the part of this page that the file provides; the rest stays zero.
            let copy_start = page.max(vaddr);
            let copy_end = (page + PAGE_SIZE).min(vaddr + filesz);
            if copy_start < copy_end {
                let src = &file_bytes[(copy_start - vaddr) as usize..(copy_end - vaddr) as usize];
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src.as_ptr(),
                        phys_to_virt(frame + (copy_start - page)),
                        src.len(),
                    );
                }
            }
            page += PAGE_SIZE;
        }
    }

    if !entry_is_code {
        return Err(ElfError::BadSegment("entry point is not in an executable segment"));
    }
    Ok(entry)
}
