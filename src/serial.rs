use crate::port::{inb, outb};
use crate::sync::SpinLock;
use core::fmt::{self, Write};

const COM1: u16 = 0x3F8;

/// Minimal 16550 UART driver on COM1.
pub struct Serial;

pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // disable UART interrupts
        outb(COM1 + 3, 0x80); // enable DLAB to set baud divisor
        outb(COM1, 0x01); // divisor 1 -> 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8 data bits, no parity, 1 stop bit
        outb(COM1 + 2, 0xC7); // enable and clear FIFOs
        outb(COM1 + 4, 0x0B); // RTS/DSR set, OUT2 set
    }
}

fn write_byte(b: u8) {
    unsafe {
        while inb(COM1 + 5) & 0x20 == 0 {}
        outb(COM1, b);
    }
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                write_byte(b'\r');
            }
            write_byte(b);
        }
        Ok(())
    }
}

// Interrupts are off while this is held, so a line from one thread (or an
// interrupt handler) is never split by another.
static PRINT_LOCK: SpinLock<()> = SpinLock::new(());

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _guard = PRINT_LOCK.lock();
    let _ = Serial.write_fmt(args);
}

/// Write raw bytes (e.g. from a user process), keeping them together.
pub fn write_bytes(bytes: &[u8]) {
    let _guard = PRINT_LOCK.lock();
    for &b in bytes {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

/// For crash paths: make sure the report can be printed even if the code that
/// crashed was in the middle of printing.
pub fn force_unlock() {
    unsafe { PRINT_LOCK.force_unlock() };
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::serial::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
