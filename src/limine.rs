//! Minimal Limine boot protocol bindings (only what the kernel currently asks for).
//! Layouts follow limine.h from the v9.x protocol.

use core::ptr::read_volatile;

const COMMON_MAGIC_0: u64 = 0xc7b1dd30df4c8b88;
const COMMON_MAGIC_1: u64 = 0x0a82e883a194f07b;

/// A request the bootloader fills in before jumping to the kernel.
/// `response` starts out null and is written by Limine, so it is only ever
/// read with `read_volatile` (the compiler must not fold it to null).
#[repr(C)]
pub struct Request<T> {
    id: [u64; 4],
    revision: u64,
    response: *const T,
}

unsafe impl<T> Sync for Request<T> {}

impl<T> Request<T> {
    pub const fn new(id2: u64, id3: u64) -> Self {
        Request {
            id: [COMMON_MAGIC_0, COMMON_MAGIC_1, id2, id3],
            revision: 0,
            response: core::ptr::null(),
        }
    }

    pub fn response(&self) -> Option<&'static T> {
        let ptr = unsafe { read_volatile(&self.response) };
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }
}

#[repr(C)]
pub struct HhdmResponse {
    pub revision: u64,
    /// Virtual address where all physical memory is mapped (phys + offset = virt).
    pub offset: u64,
}

#[repr(C)]
pub struct MemmapResponse {
    pub revision: u64,
    pub entry_count: u64,
    pub entries: *const *const MemmapEntry,
}

#[repr(C)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

pub const MEMMAP_USABLE: u64 = 0;
pub const MEMMAP_RESERVED: u64 = 1;
pub const MEMMAP_ACPI_RECLAIMABLE: u64 = 2;
pub const MEMMAP_ACPI_NVS: u64 = 3;
pub const MEMMAP_BAD_MEMORY: u64 = 4;
pub const MEMMAP_BOOTLOADER_RECLAIMABLE: u64 = 5;
pub const MEMMAP_EXECUTABLE_AND_MODULES: u64 = 6;
pub const MEMMAP_FRAMEBUFFER: u64 = 7;

#[repr(C)]
pub struct CmdlineResponse {
    pub revision: u64,
    /// NUL-terminated kernel command line from the boot menu entry.
    pub cmdline: *const u8,
}

#[used]
#[link_section = ".requests"]
pub static CMDLINE_REQUEST: Request<CmdlineResponse> =
    Request::new(0x4b161536e598651e, 0xb390ad4a2f1f303a);

/// True if the boot entry's `cmdline:` contains `flag` as a whole word.
pub fn cmdline_has(flag: &str) -> bool {
    let Some(response) = CMDLINE_REQUEST.response() else { return false };
    if response.cmdline.is_null() {
        return false;
    }
    let mut len = 0;
    while len < 1024 && unsafe { *response.cmdline.add(len) } != 0 {
        len += 1;
    }
    let bytes = unsafe { core::slice::from_raw_parts(response.cmdline, len) };
    core::str::from_utf8(bytes)
        .map(|line| line.split_whitespace().any(|word| word == flag))
        .unwrap_or(false)
}

#[used]
#[link_section = ".requests"]
pub static HHDM_REQUEST: Request<HhdmResponse> =
    Request::new(0x48dcf1cb8ad2b852, 0x63984e959a98244b);

#[used]
#[link_section = ".requests"]
pub static MEMMAP_REQUEST: Request<MemmapResponse> =
    Request::new(0x67cf3d9d378a806f, 0xe304acdfc50c3c62);
