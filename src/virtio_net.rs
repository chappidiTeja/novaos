//! virtio-net network card driver (legacy / "transitional" interface, I/O ports).
//!
//! Two virtqueues: 0 receives, 1 transmits. Every packet travels with a 10-byte
//! `virtio_net_hdr` in front of the Ethernet frame (all zero for us: no checksum
//! offload, no segmentation). Receive buffers are posted up front and re-posted
//! as frames are consumed; transmit buffers come from a small free list.
//! Nothing is interrupt-driven: the kernel's network thread polls.

use crate::mem::{align_up, phys_to_virt, pmm, PAGE_SIZE};
use crate::pci::{self, PciDevice};
use crate::port::{inb, inl, inw, outb, outl, outw};
use alloc::vec::Vec;
use core::sync::atomic::{fence, Ordering};

const VENDOR_VIRTIO: u16 = 0x1AF4;
const DEVICE_NET_LEGACY: u16 = 0x1000;

const REG_DEVICE_FEATURES: u16 = 0;
const REG_GUEST_FEATURES: u16 = 4;
const REG_QUEUE_PFN: u16 = 8;
const REG_QUEUE_SIZE: u16 = 12;
const REG_QUEUE_SELECT: u16 = 14;
const REG_QUEUE_NOTIFY: u16 = 16;
const REG_STATUS: u16 = 18;
const REG_ISR: u16 = 19;
const REG_CONFIG: u16 = 20; // the MAC address, 6 bytes

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 128;

const FEATURE_MAC: u32 = 1 << 5;
const FEATURE_STATUS: u32 = 1 << 16;
const STATUS_LINK_UP: u16 = 1;

const DESC_WRITE: u16 = 2; // the device writes into this buffer

/// virtio_net_hdr without VIRTIO_NET_F_MRG_RXBUF: flags, gso_type, hdr_len, gso_size, csum_start, csum_offset.
const NET_HDR: usize = 10;
/// One page per buffer: header + largest frame fit with room to spare.
const BUF_SIZE: usize = PAGE_SIZE as usize;
const RX_BUFFERS: usize = 64;
const TX_BUFFERS: usize = 32;
pub const MAX_FRAME: usize = 1514;

struct Queue {
    io: u16,
    index: u16,
    size: u16,
    mem: *mut u8,
    avail_offset: usize,
    used_offset: usize,
    last_used: u16,
}

impl Queue {
    unsafe fn new(io: u16, index: u16) -> Result<Queue, &'static str> {
        outw(io + REG_QUEUE_SELECT, index);
        let size = inw(io + REG_QUEUE_SIZE);
        if size == 0 || !size.is_power_of_two() {
            return Err("virtio-net queue is missing or has an invalid size");
        }
        let n = size as usize;
        let avail_offset = 16 * n;
        let used_offset = align_up((avail_offset + 6 + 2 * n) as u64, PAGE_SIZE) as usize;
        let total = used_offset + align_up((6 + 8 * n) as u64, PAGE_SIZE) as usize;
        let phys = pmm::alloc_contiguous_zeroed(total / PAGE_SIZE as usize)
            .ok_or("no contiguous memory for a virtio-net queue")?;
        outl(io + REG_QUEUE_PFN, (phys / PAGE_SIZE) as u32);
        Ok(Queue { io, index, size, mem: phys_to_virt(phys), avail_offset, used_offset, last_used: 0 })
    }

    unsafe fn set_descriptor(&self, i: usize, addr: u64, len: u32, flags: u16) {
        let d = self.mem.add(16 * i);
        (d as *mut u64).write_volatile(addr);
        (d.add(8) as *mut u32).write_volatile(len);
        (d.add(12) as *mut u16).write_volatile(flags);
        (d.add(14) as *mut u16).write_volatile(0);
    }

    /// Offer descriptor `head` to the device (does not notify).
    unsafe fn push_avail(&self, head: u16) {
        let avail = self.mem.add(self.avail_offset);
        let idx = (avail.add(2) as *const u16).read_volatile();
        let slot = (idx % self.size) as usize;
        (avail.add(4 + 2 * slot) as *mut u16).write_volatile(head);
        fence(Ordering::SeqCst);
        (avail.add(2) as *mut u16).write_volatile(idx.wrapping_add(1));
        fence(Ordering::SeqCst);
    }

    unsafe fn notify(&self) {
        outw(self.io + REG_QUEUE_NOTIFY, self.index);
    }

    /// The next descriptor the device has finished with: (descriptor id, bytes written).
    unsafe fn pop_used(&mut self) -> Option<(u16, u32)> {
        let used = self.mem.add(self.used_offset);
        let idx = (used.add(2) as *const u16).read_volatile();
        if idx == self.last_used {
            return None;
        }
        fence(Ordering::SeqCst);
        let slot = (self.last_used % self.size) as usize;
        let elem = used.add(4 + 8 * slot);
        let id = (elem as *const u32).read_volatile();
        let len = (elem.add(4) as *const u32).read_volatile();
        self.last_used = self.last_used.wrapping_add(1);
        Some((id as u16, len))
    }
}

pub struct VirtioNet {
    io: u16,
    mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    rx_bufs: Vec<u64>,
    tx_bufs: Vec<u64>,
    tx_free: Vec<u16>,
    link_status: bool,
    irq: u8,
    /// Frames we could not send because every transmit buffer was busy.
    pub tx_dropped: u64,
}

unsafe impl Send for VirtioNet {}

pub fn probe(devices: &[PciDevice]) -> Option<Result<VirtioNet, &'static str>> {
    let device = devices.iter().find(|d| d.vendor == VENDOR_VIRTIO && d.device == DEVICE_NET_LEGACY)?;
    Some(unsafe { init(device) })
}

unsafe fn init(device: &PciDevice) -> Result<VirtioNet, &'static str> {
    let bar0 = device.bar(0);
    if bar0 & 1 == 0 {
        return Err("virtio-net BAR0 is not an I/O port range");
    }
    let io = (bar0 & !3) as u16;
    device.enable(pci::COMMAND_IO_SPACE | pci::COMMAND_BUS_MASTER);

    outb(io + REG_STATUS, 0);
    outb(io + REG_STATUS, STATUS_ACKNOWLEDGE);
    outb(io + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    let offered = inl(io + REG_DEVICE_FEATURES);
    if offered & FEATURE_MAC == 0 {
        outb(io + REG_STATUS, STATUS_FAILED);
        return Err("virtio-net device does not report a MAC address");
    }
    // Only what we use: the MAC address (and the link status field if offered).
    outl(io + REG_GUEST_FEATURES, offered & (FEATURE_MAC | FEATURE_STATUS));
    let has_status = offered & FEATURE_STATUS != 0;

    let rx = Queue::new(io, 0)?;
    let tx = Queue::new(io, 1)?;
    // We reclaim transmit buffers when we next send, so the device need not interrupt for them
    // (VRING_AVAIL_F_NO_INTERRUPT in the available ring's flags).
    (tx.mem.add(tx.avail_offset) as *mut u16).write_volatile(1);
    if (rx.size as usize) < RX_BUFFERS || (tx.size as usize) < TX_BUFFERS {
        return Err("virtio-net queues are smaller than the buffer pools");
    }

    let mut rx_bufs = Vec::with_capacity(RX_BUFFERS);
    for i in 0..RX_BUFFERS {
        let phys = pmm::alloc_zeroed_frame().ok_or("out of memory for receive buffers")?;
        rx.set_descriptor(i, phys, BUF_SIZE as u32, DESC_WRITE);
        rx.push_avail(i as u16);
        rx_bufs.push(phys);
    }
    let mut tx_bufs = Vec::with_capacity(TX_BUFFERS);
    for _ in 0..TX_BUFFERS {
        tx_bufs.push(pmm::alloc_zeroed_frame().ok_or("out of memory for transmit buffers")?);
    }
    let tx_free: Vec<u16> = (0..TX_BUFFERS as u16).rev().collect();

    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = inb(io + REG_CONFIG + i as u16);
    }

    outb(io + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
    rx.notify();
    let mut dev = VirtioNet { io, mac, rx, tx, rx_bufs, tx_bufs, tx_free, link_status: true, irq: device.irq_line(), tx_dropped: 0 };
    if has_status {
        dev.link_status = inw(io + REG_CONFIG + 6) & STATUS_LINK_UP != 0;
    }
    Ok(dev)
}

impl VirtioNet {
    /// I/O port base (the interrupt handler reads the ISR register there).
    pub fn io_base(&self) -> u16 {
        self.io
    }

    /// The interrupt line the firmware assigned (0xFF = none).
    pub fn irq_line(&self) -> u8 {
        self.irq
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn link_up(&self) -> bool {
        self.link_status
    }

    /// Hand every received frame to `f`, then give the buffer back to the device.
    pub fn poll_rx(&mut self, mut f: impl FnMut(&[u8])) -> usize {
        let mut count = 0;
        unsafe {
            while let Some((id, len)) = self.rx.pop_used() {
                let i = id as usize;
                if i < RX_BUFFERS {
                    let len = len as usize;
                    // A frame shorter than the header, or longer than the buffer, is garbage.
                    if len > NET_HDR && len <= BUF_SIZE {
                        let buf = phys_to_virt(self.rx_bufs[i]);
                        f(core::slice::from_raw_parts(buf.add(NET_HDR), len - NET_HDR));
                        count += 1;
                    }
                    self.rx.push_avail(id);
                }
            }
            if count > 0 {
                self.rx.notify();
            }
            let _ = inb(self.io + REG_ISR); // acknowledge the interrupt line
        }
        count
    }

    fn reclaim_tx(&mut self) {
        unsafe {
            while let Some((id, _)) = self.tx.pop_used() {
                if (id as usize) < TX_BUFFERS {
                    self.tx_free.push(id);
                }
            }
        }
    }

    /// Is there a free transmit buffer right now?
    pub fn tx_space(&mut self) -> bool {
        self.reclaim_tx();
        !self.tx_free.is_empty()
    }

    /// Queue one Ethernet frame for sending. False if it did not fit or no buffer was free.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME || frame.len() + NET_HDR > BUF_SIZE {
            return false;
        }
        self.reclaim_tx();
        unsafe {
            let Some(id) = self.tx_free.pop() else {
                self.tx_dropped += 1;
                return false;
            };
            let buf = phys_to_virt(self.tx_bufs[id as usize]);
            core::ptr::write_bytes(buf, 0, NET_HDR);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buf.add(NET_HDR), frame.len());
            self.tx.set_descriptor(id as usize, self.tx_bufs[id as usize], (NET_HDR + frame.len()) as u32, 0);
            self.tx.push_avail(id);
            self.tx.notify();
        }
        true
    }
}
