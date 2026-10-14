//! PS/2 mouse, through the same 8042 controller as the keyboard.
//!
//! The mouse sends 3-byte packets: [buttons+flags, dx, dy]. Byte 0 always has
//! bit 3 set, which is how we notice and recover if we ever fall out of step.

use crate::port::{inb, outb};
use crate::sync::SpinLock;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
const STATUS_FROM_MOUSE: u8 = 1 << 5;

const ACK: u8 = 0xFA;

/// Spin this many times waiting for the controller before giving up.
const PATIENCE: u32 = 200_000;

fn wait_to_write() -> bool {
    (0..PATIENCE).any(|_| unsafe { inb(STATUS) } & STATUS_INPUT_FULL == 0)
}

fn wait_to_read() -> bool {
    (0..PATIENCE).any(|_| unsafe { inb(STATUS) } & STATUS_OUTPUT_FULL != 0)
}

fn controller(command: u8) -> bool {
    if !wait_to_write() {
        return false;
    }
    unsafe { outb(COMMAND, command) };
    true
}

fn write_data(byte: u8) -> bool {
    if !wait_to_write() {
        return false;
    }
    unsafe { outb(DATA, byte) };
    true
}

fn read_data() -> Option<u8> {
    if wait_to_read() {
        Some(unsafe { inb(DATA) })
    } else {
        None
    }
}

/// Send a command byte to the mouse itself and wait for its acknowledgement.
fn mouse_command(byte: u8) -> bool {
    controller(0xD4) && write_data(byte) && read_data() == Some(ACK)
}

/// Throw away anything the controller is still holding.
fn flush() {
    for _ in 0..64 {
        if unsafe { inb(STATUS) } & STATUS_OUTPUT_FULL == 0 {
            break;
        }
        unsafe { inb(DATA) };
    }
}

/// Turn the mouse on. False if there is none (or the controller does not answer).
pub fn init() -> bool {
    flush();
    // Enable the second (mouse) port.
    if !controller(0xA8) {
        return false;
    }
    // Read the controller's configuration byte, switch on the mouse interrupt
    // (bit 1) and the mouse clock (clear bit 5), and write it back.
    if !controller(0x20) {
        return false;
    }
    let Some(config) = read_data() else { return false };
    let config = (config | 0x02) & !0x20;
    if !controller(0x60) || !write_data(config) {
        return false;
    }
    // Defaults, then start sending packets.
    mouse_command(0xF6) && mouse_command(0xF4)
}

struct Assembler {
    bytes: [u8; 3],
    have: usize,
}

static PACKET: SpinLock<Assembler> = SpinLock::new(Assembler { bytes: [0; 3], have: 0 });

/// Called from the mouse interrupt.
pub fn handle_interrupt() {
    // Only take the byte if the controller says it came from the mouse.
    if unsafe { inb(STATUS) } & STATUS_FROM_MOUSE == 0 {
        return;
    }
    let byte = unsafe { inb(DATA) };
    let mut assembler = PACKET.lock();

    // The first byte of a packet always has bit 3 set; if it does not, we are
    // out of step, so skip bytes until one does.
    if assembler.have == 0 && byte & 0x08 == 0 {
        return;
    }
    let at = assembler.have;
    assembler.bytes[at] = byte;
    assembler.have += 1;
    if assembler.have < 3 {
        return;
    }
    assembler.have = 0;
    let [flags, raw_dx, raw_dy] = assembler.bytes;
    drop(assembler);

    let (dx, dy) = decode(flags, raw_dx, raw_dy);
    crate::input::mouse_packet(dx, -dy, (flags & 0x07) as i32);
}

/// Movement from the packet bytes: 9-bit two's complement, sign bits in `flags`.
/// A packet whose overflow bits are set carries no usable movement.
fn decode(flags: u8, raw_dx: u8, raw_dy: u8) -> (i32, i32) {
    if flags & 0xC0 != 0 {
        return (0, 0);
    }
    let dx = raw_dx as i32 - if flags & 0x10 != 0 { 256 } else { 0 };
    let dy = raw_dy as i32 - if flags & 0x20 != 0 { 256 } else { 0 };
    (dx, dy)
}
