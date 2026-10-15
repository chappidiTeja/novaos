//! virtio-blk block device driver (legacy / "transitional" interface, I/O ports).
//!
//! The device and the driver share a "virtqueue" in physical memory:
//!   descriptor table   16 bytes each: buffer address, length, flags, next
//!   available ring     descriptor chains the driver has submitted
//!   used ring          chains the device has finished
//! One request is a chain of three descriptors: a 16-byte header, the 4096-byte
//! data buffer, and a 1-byte status the device fills in. Requests are issued one
//! at a time and completion is polled (no interrupts).

use crate::mem::{align_up, phys_to_virt, pmm, PAGE_SIZE};
use crate::pci::{self, PciDevice};
use crate::port::{inb, inl, inw, outb, outl, outw};
use crate::sched;
use core::sync::atomic::{fence, Ordering};
use novafs::{Block, BlockDevice, IoError, BLOCK_SIZE};

const VENDOR_VIRTIO: u16 = 0x1AF4;

/// The disk's I/O base (0 until the disk is found). Its interrupt line may be shared with the
/// network card's; the interrupt handler reads this ISR register too, or the line never drops.
pub static IO_BASE: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
const DEVICE_BLOCK_LEGACY: u16 = 0x1001;

// Legacy register offsets from the I/O base (BAR0).
const REG_GUEST_FEATURES: u16 = 4;
const REG_QUEUE_PFN: u16 = 8;
const REG_QUEUE_SIZE: u16 = 12;
const REG_QUEUE_SELECT: u16 = 14;
const REG_QUEUE_NOTIFY: u16 = 16;
const REG_STATUS: u16 = 18;
const REG_ISR: u16 = 19;
const REG_CONFIG: u16 = 20; // block device config: capacity in 512-byte sectors

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;

const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2; // the device writes into this buffer

const REQUEST_READ: u32 = 0;
const REQUEST_WRITE: u32 = 1;
const SECTOR_SIZE: u64 = 512;
const SECTORS_PER_BLOCK: u64 = BLOCK_SIZE as u64 / SECTOR_SIZE;

pub struct VirtioBlk {
    io: u16,
    queue_size: u16,
    queue: *mut u8,
    avail_offset: usize,
    used_offset: usize,
    /// One frame holding the request header (offset 0) and status byte (offset 16).
    request_phys: u64,
    /// One frame used as the DMA bounce buffer for the 4096-byte data.
    data_phys: u64,
    last_used: u16,
    capacity_sectors: u64,
    /// After a timeout the device may still write to our buffers: refuse all further I/O.
    dead: bool,
}

unsafe impl Send for VirtioBlk {}

/// Look through the PCI devices for a virtio block device and bring it up.
pub fn probe(devices: &[PciDevice]) -> Option<Result<VirtioBlk, &'static str>> {
    let device = devices
        .iter()
        .find(|d| d.vendor == VENDOR_VIRTIO && d.device == DEVICE_BLOCK_LEGACY)?;
    Some(unsafe { init(device) })
}

unsafe fn init(device: &PciDevice) -> Result<VirtioBlk, &'static str> {
    let bar0 = device.bar(0);
    if bar0 & 1 == 0 {
        return Err("virtio BAR0 is not an I/O port range");
    }
    let io = (bar0 & !3) as u16;
    IO_BASE.store(io, core::sync::atomic::Ordering::Relaxed);
    device.enable(pci::COMMAND_IO_SPACE | pci::COMMAND_BUS_MASTER);

    outb(io + REG_STATUS, 0); // reset
    outb(io + REG_STATUS, STATUS_ACKNOWLEDGE);
    outb(io + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    outl(io + REG_GUEST_FEATURES, 0); // we need no optional features

    outw(io + REG_QUEUE_SELECT, 0);
    let queue_size = inw(io + REG_QUEUE_SIZE);
    if queue_size == 0 || !queue_size.is_power_of_two() {
        return Err("virtio queue 0 is missing or has an invalid size");
    }

    // Legacy layout: descriptors, then the available ring, then (page
    // aligned) the used ring.
    let n = queue_size as usize;
    let avail_offset = 16 * n;
    let used_offset = align_up((avail_offset + 6 + 2 * n) as u64, PAGE_SIZE) as usize;
    let total = used_offset + align_up((6 + 8 * n) as u64, PAGE_SIZE) as usize;
    let pages = total / PAGE_SIZE as usize;
    let queue_phys =
        pmm::alloc_contiguous_zeroed(pages).ok_or("no contiguous memory for the virtqueue")?;
    let request_phys = pmm::alloc_zeroed_frame().ok_or("out of memory")?;
    let data_phys = pmm::alloc_zeroed_frame().ok_or("out of memory")?;

    outl(io + REG_QUEUE_PFN, (queue_phys / PAGE_SIZE) as u32);
    outb(io + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);

    let capacity_sectors = inl(io + REG_CONFIG) as u64 | (inl(io + REG_CONFIG + 4) as u64) << 32;
    Ok(VirtioBlk {
        io,
        queue_size,
        queue: phys_to_virt(queue_phys),
        avail_offset,
        used_offset,
        request_phys,
        data_phys,
        last_used: 0,
        capacity_sectors,
        dead: false,
    })
}

impl VirtioBlk {
    pub fn capacity_blocks(&self) -> u64 {
        self.capacity_sectors / SECTORS_PER_BLOCK
    }

    unsafe fn set_descriptor(&self, index: usize, addr: u64, len: u32, flags: u16, next: u16) {
        let d = self.queue.add(16 * index);
        (d as *mut u64).write_volatile(addr);
        (d.add(8) as *mut u32).write_volatile(len);
        (d.add(12) as *mut u16).write_volatile(flags);
        (d.add(14) as *mut u16).write_volatile(next);
    }

    /// Transfer the 4096 bytes in the bounce buffer to or from block `block`.
    fn transfer(&mut self, write: bool, block: u64) -> Result<(), IoError> {
        if self.dead || block >= self.capacity_blocks() {
            return Err(IoError);
        }
        unsafe {
            let header = phys_to_virt(self.request_phys);
            (header as *mut u32).write_volatile(if write { REQUEST_WRITE } else { REQUEST_READ });
            (header.add(4) as *mut u32).write_volatile(0);
            (header.add(8) as *mut u64).write_volatile(block * SECTORS_PER_BLOCK);
            header.add(16).write_volatile(0xFF); // device overwrites with 0 on success

            self.set_descriptor(0, self.request_phys, 16, DESC_NEXT, 1);
            let data_flags = DESC_NEXT | if write { 0 } else { DESC_WRITE };
            self.set_descriptor(1, self.data_phys, BLOCK_SIZE as u32, data_flags, 2);
            self.set_descriptor(2, self.request_phys + 16, 1, DESC_WRITE, 0);

            // Publish descriptor chain 0 on the available ring, then tell the device.
            let avail = self.queue.add(self.avail_offset);
            let index = (avail.add(2) as *const u16).read_volatile();
            let slot = (index % self.queue_size) as usize;
            (avail.add(4 + 2 * slot) as *mut u16).write_volatile(0);
            fence(Ordering::SeqCst);
            (avail.add(2) as *mut u16).write_volatile(index.wrapping_add(1));
            fence(Ordering::SeqCst);
            outw(self.io + REG_QUEUE_NOTIFY, 0);

            let used_idx = self.queue.add(self.used_offset + 2) as *const u16;
            let mut spins: u64 = 0;
            while used_idx.read_volatile() == self.last_used {
                spins += 1;
                if spins < 200_000 {
                    core::hint::spin_loop();
                } else if sched::started() {
                    sched::yield_now();
                    if spins > 20_000_000 {
                        self.dead = true;
                        return Err(IoError);
                    }
                } else if spins > 100_000_000 {
                    self.dead = true;
                    return Err(IoError);
                }
            }
            self.last_used = self.last_used.wrapping_add(1);
            fence(Ordering::SeqCst);
            let status = header.add(16).read_volatile();
            let _ = inb(self.io + REG_ISR); // acknowledge the device's interrupt line
            if status == 0 {
                Ok(())
            } else {
                Err(IoError)
            }
        }
    }
}

/// The device as the filesystem sees it: an array of 4096-byte blocks.
pub struct VirtioDisk(pub VirtioBlk);

impl BlockDevice for VirtioDisk {
    fn block_count(&self) -> u64 {
        self.0.capacity_blocks()
    }

    fn read_block(&mut self, index: u64, buf: &mut Block) -> Result<(), IoError> {
        self.0.transfer(false, index)?;
        let src = phys_to_virt(self.0.data_phys);
        unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), BLOCK_SIZE) };
        Ok(())
    }

    fn write_block(&mut self, index: u64, buf: &Block) -> Result<(), IoError> {
        let dst = phys_to_virt(self.0.data_phys);
        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), dst, BLOCK_SIZE) };
        self.0.transfer(true, index)
    }
}
