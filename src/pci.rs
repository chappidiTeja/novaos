//! PCI configuration space access (the legacy 0xCF8/0xCFC mechanism) and a bus
//! scan. Every modern device (disks, network cards, GPUs) is found this way.

use crate::port::{inl, outl};
use alloc::vec::Vec;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

pub const COMMAND: u8 = 0x04;
pub const COMMAND_IO_SPACE: u16 = 1 << 0;
pub const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
pub const COMMAND_BUS_MASTER: u16 = 1 << 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciAddress {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct PciDevice {
    pub addr: PciAddress,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub header_type: u8,
}

fn config_address(addr: PciAddress, offset: u8) -> u32 {
    0x8000_0000
        | (addr.bus as u32) << 16
        | (addr.slot as u32) << 11
        | (addr.func as u32) << 8
        | (offset & 0xFC) as u32
}

pub fn read32(addr: PciAddress, offset: u8) -> u32 {
    unsafe {
        outl(CONFIG_ADDRESS, config_address(addr, offset));
        inl(CONFIG_DATA)
    }
}

pub fn write32(addr: PciAddress, offset: u8, value: u32) {
    unsafe {
        outl(CONFIG_ADDRESS, config_address(addr, offset));
        outl(CONFIG_DATA, value);
    }
}

pub fn read16(addr: PciAddress, offset: u8) -> u16 {
    (read32(addr, offset) >> ((offset & 2) * 8)) as u16
}

impl PciDevice {
    /// Raw value of base address register `index` (0-5).
    pub fn bar(&self, index: u8) -> u32 {
        read32(self.addr, 0x10 + 4 * index)
    }

    /// The interrupt line (0-15) the firmware routed this device to; 0xFF if none.
    pub fn irq_line(&self) -> u8 {
        read32(self.addr, 0x3C) as u8
    }

    /// Allow the device to respond to I/O ports and to DMA into memory.
    pub fn enable(&self, flags: u16) {
        let current = read32(self.addr, COMMAND);
        let command = (current as u16) | flags;
        // The upper half of this register is the status register: write zeros
        // there (status bits are write-1-to-clear).
        write32(self.addr, COMMAND, command as u32);
    }
}

fn probe(addr: PciAddress) -> Option<PciDevice> {
    let id = read32(addr, 0x00);
    let vendor = id as u16;
    if vendor == 0xFFFF {
        return None;
    }
    let class = read32(addr, 0x08);
    let header = read32(addr, 0x0C);
    Some(PciDevice {
        addr,
        vendor,
        device: (id >> 16) as u16,
        class: (class >> 24) as u8,
        subclass: (class >> 16) as u8,
        prog_if: (class >> 8) as u8,
        header_type: (header >> 16) as u8,
    })
}

/// Every device on every bus.
pub fn scan() -> Vec<PciDevice> {
    let mut found = Vec::new();
    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            let first = PciAddress { bus, slot, func: 0 };
            let Some(device) = probe(first) else { continue };
            let multifunction = device.header_type & 0x80 != 0;
            found.push(device);
            if multifunction {
                for func in 1..8u8 {
                    if let Some(device) = probe(PciAddress { bus, slot, func }) {
                        found.push(device);
                    }
                }
            }
        }
    }
    found
}

pub fn class_name(class: u8, subclass: u8) -> &'static str {
    match (class, subclass) {
        (0x00, _) => "unclassified",
        (0x01, 0x00) => "SCSI storage",
        (0x01, 0x01) => "IDE storage",
        (0x01, 0x06) => "SATA storage",
        (0x01, 0x08) => "NVMe storage",
        (0x01, _) => "storage",
        (0x02, _) => "network",
        (0x03, _) => "display",
        (0x04, _) => "multimedia",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI bridge",
        (0x06, _) => "bridge",
        (0x0C, 0x03) => "USB controller",
        (0x0C, _) => "serial bus",
        _ => "other",
    }
}
