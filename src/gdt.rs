use core::arch::asm;
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut};

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
const TSS_SEL: u16 = 0x18;

/// IDT IST index (1-based) that points at the dedicated double-fault stack.
pub const DOUBLE_FAULT_IST: u8 = 1;

#[repr(C, packed)]
struct Tss {
    _r0: u32,
    rsp: [u64; 3],
    _r1: u64,
    ist: [u64; 7],
    _r2: u64,
    _r3: u16,
    iomap_base: u16,
}

#[repr(C, align(16))]
struct Stack([u8; 16384]);

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

static mut TSS: Tss = Tss {
    _r0: 0,
    rsp: [0; 3],
    _r1: 0,
    ist: [0; 7],
    _r2: 0,
    _r3: 0,
    iomap_base: size_of::<Tss>() as u16,
};
static mut DOUBLE_FAULT_STACK: Stack = Stack([0; 16384]);
static mut GDT: [u64; 5] = [0; 5];

pub fn init() {
    unsafe {
        let stack_top = addr_of!(DOUBLE_FAULT_STACK) as u64 + size_of::<Stack>() as u64;
        let mut ist = [0u64; 7];
        ist[0] = stack_top;
        addr_of_mut!(TSS.ist).write_unaligned(ist);

        let base = addr_of!(TSS) as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        let tss_low = (limit & 0xFFFF)
            | ((base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40) // present, 64-bit TSS (available)
            | (((limit >> 16) & 0xF) << 48)
            | (((base >> 24) & 0xFF) << 56);
        let tss_high = base >> 32;

        addr_of_mut!(GDT).write([
            0,
            0x00AF_9A00_0000_FFFF, // kernel code, 64-bit
            0x00CF_9200_0000_FFFF, // kernel data
            tss_low,
            tss_high,
        ]);

        let ptr = DescriptorPointer {
            limit: (size_of::<[u64; 5]>() - 1) as u16,
            base: addr_of!(GDT) as u64,
        };
        asm!("lgdt [{}]", in(reg) addr_of!(ptr), options(readonly, nostack, preserves_flags));

        // Reload CS with a far return, then the data segment registers.
        asm!(
            "push {sel}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            sel = in(reg) KERNEL_CS as u64,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );
        asm!(
            "mov ds, {0:x}",
            "mov es, {0:x}",
            "mov ss, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            in(reg) KERNEL_DS as u32,
            options(nostack, preserves_flags),
        );
        asm!("ltr {0:x}", in(reg) TSS_SEL as u32, options(nostack, preserves_flags));
    }
}
