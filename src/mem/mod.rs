//! Memory management: physical frames (pmm), page tables (vmm), kernel heap.

pub mod heap;
pub mod pmm;
pub mod vmm;

use crate::limine::{self, MemmapEntry};
use crate::println;
use core::sync::atomic::{AtomicU64, Ordering};

pub const PAGE_SIZE: u64 = 4096;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Pointer through which the kernel can access physical address `phys`.
/// Limine maps all of RAM at a fixed offset (the higher-half direct map).
pub fn phys_to_virt(phys: u64) -> *mut u8 {
    (phys + HHDM_OFFSET.load(Ordering::Relaxed)) as *mut u8
}

pub fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

pub fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn kind_name(kind: u64) -> &'static str {
    match kind {
        limine::MEMMAP_USABLE => "usable",
        limine::MEMMAP_RESERVED => "reserved",
        limine::MEMMAP_ACPI_RECLAIMABLE => "ACPI reclaimable",
        limine::MEMMAP_ACPI_NVS => "ACPI NVS",
        limine::MEMMAP_BAD_MEMORY => "bad memory",
        limine::MEMMAP_BOOTLOADER_RECLAIMABLE => "bootloader reclaimable",
        limine::MEMMAP_EXECUTABLE_AND_MODULES => "kernel image",
        limine::MEMMAP_FRAMEBUFFER => "framebuffer",
        _ => "unknown",
    }
}

pub fn init() {
    let hhdm = limine::HHDM_REQUEST
        .response()
        .expect("bootloader gave no HHDM response");
    HHDM_OFFSET.store(hhdm.offset, Ordering::Relaxed);

    let memmap = limine::MEMMAP_REQUEST
        .response()
        .expect("bootloader gave no memory map");
    let entries: &[*const MemmapEntry] =
        unsafe { core::slice::from_raw_parts(memmap.entries, memmap.entry_count as usize) };

    println!("memory map (HHDM at {:#x}):", hhdm.offset);
    for &entry in entries {
        let e = unsafe { &*entry };
        println!(
            "  {:#012x}-{:#012x}  {}",
            e.base,
            e.base + e.length,
            kind_name(e.kind)
        );
    }

    pmm::init(entries);
    let (free, total) = pmm::stats();
    println!(
        "[ok] pmm: {} MiB usable, {} MiB free",
        total * PAGE_SIZE / (1024 * 1024),
        free * PAGE_SIZE / (1024 * 1024)
    );

    vmm::init();
    heap::init();
    println!(
        "[ok] heap: {} KiB at {:#x}",
        heap::HEAP_SIZE / 1024,
        heap::HEAP_START
    );
}

/// Exercises the frame allocator, page-table code and heap. Panics on any failure.
pub fn self_test() {
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::ptr::{read_volatile, write_volatile};

    // Physical frame allocator: distinct frames, and freeing restores the count.
    let (free_before, _) = pmm::stats();
    let a = pmm::alloc_frame().unwrap();
    let b = pmm::alloc_frame().unwrap();
    let c = pmm::alloc_frame().unwrap();
    assert!(a != b && b != c && a != c, "pmm returned duplicate frames");
    assert_eq!(pmm::stats().0, free_before - 3);
    pmm::free_frame(b);
    pmm::free_frame(a);
    pmm::free_frame(c);
    assert_eq!(pmm::stats().0, free_before, "pmm leaked frames");
    println!("[ok] pmm self-test");

    // Page tables: map a fresh frame, check both views of it, then unmap.
    const TEST_VIRT: u64 = 0xffff_d000_0000_0000;
    let frame = pmm::alloc_frame().unwrap();
    vmm::map(TEST_VIRT, frame, vmm::data_flags()).unwrap();
    assert!(
        matches!(
            vmm::map(TEST_VIRT, frame, vmm::data_flags()),
            Err(vmm::MapError::AlreadyMapped)
        ),
        "double map was not rejected"
    );
    assert_eq!(vmm::translate(TEST_VIRT), Some(frame));
    assert_eq!(vmm::translate(TEST_VIRT + 0x123), Some(frame + 0x123));
    unsafe {
        write_volatile(TEST_VIRT as *mut u64, 0x4e6f_7661_4f53_2121);
        let alias = phys_to_virt(frame) as *const u64;
        assert_eq!(read_volatile(alias), 0x4e6f_7661_4f53_2121, "aliases differ");
    }
    // The direct map uses huge pages, so this exercises that translate path too.
    assert_eq!(vmm::translate(phys_to_virt(frame) as u64), Some(frame));
    assert_eq!(vmm::unmap(TEST_VIRT), Some(frame));
    assert_eq!(vmm::translate(TEST_VIRT), None);
    assert_eq!(vmm::unmap(TEST_VIRT), None);
    pmm::free_frame(frame);
    println!("[ok] vmm self-test");

    // Heap: Box/Vec/String, alignment, and merging of freed blocks.
    let baseline = heap::used();
    let boxed = Box::new(0xdead_beef_u64);
    assert_eq!(*boxed, 0xdead_beef);
    let mut numbers = Vec::new();
    for i in 0..10_000u64 {
        numbers.push(i);
    }
    assert_eq!(numbers.iter().sum::<u64>(), 49_995_000);
    let text = String::from("heap strings work");
    assert_eq!(text.len(), 17);

    #[repr(align(4096))]
    struct Page([u8; 4096]);
    let page = Box::new(Page([7; 4096]));
    assert_eq!(&*page as *const Page as usize % 4096, 0, "alignment ignored");
    assert_eq!(page.0[4095], 7);
    drop((boxed, numbers, text, page));
    assert_eq!(heap::used(), baseline, "heap leaked");

    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for i in 0..100usize {
        blocks.push(alloc::vec![i as u8; 1 + i * 37]);
    }
    for i in (0..100).step_by(2) {
        blocks[i] = Vec::new();
    }
    for i in (1..100).step_by(2) {
        assert!(blocks[i].iter().all(|&b| b == i as u8), "heap corrupted data");
        blocks[i] = Vec::new();
    }
    drop(blocks);
    assert_eq!(heap::used(), baseline, "heap leaked after fragmentation");
    // Only possible if every freed block merged back into one.
    let big: Vec<u8> = Vec::with_capacity(heap::HEAP_SIZE as usize - 4096);
    drop(big);
    assert_eq!(heap::used(), baseline);
    println!("[ok] heap self-test");
}
