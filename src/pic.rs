use crate::port::{io_wait, outb};

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// First vector used for hardware IRQs; vectors 0-31 are CPU exceptions.
pub const IRQ_BASE: u8 = 0x20;

/// Remap the legacy 8259 PICs away from the exception vectors and unmask
/// only the timer (IRQ0), keyboard (IRQ1), first serial port (IRQ4) and mouse (IRQ12).
pub fn init() {
    unsafe {
        outb(PIC1_CMD, 0x11); // start initialization, expect ICW4
        io_wait();
        outb(PIC2_CMD, 0x11);
        io_wait();
        outb(PIC1_DATA, IRQ_BASE); // master vector offset
        io_wait();
        outb(PIC2_DATA, IRQ_BASE + 8); // slave vector offset
        io_wait();
        outb(PIC1_DATA, 4); // slave PIC is on IRQ2
        io_wait();
        outb(PIC2_DATA, 2); // slave cascade identity
        io_wait();
        outb(PIC1_DATA, 0x01); // 8086 mode
        io_wait();
        outb(PIC2_DATA, 0x01);
        io_wait();
        // A set bit masks the line. Master: unmask IRQ0 (timer), IRQ1 (keyboard),
        // IRQ2 (the cascade to the second PIC) and IRQ4 (serial). Second PIC:
        // unmask IRQ12 (mouse).
        outb(PIC1_DATA, 0xE8);
        outb(PIC2_DATA, 0xEF);
    }
}

pub fn end_of_interrupt(vector: u8) {
    unsafe {
        if vector >= IRQ_BASE + 8 {
            outb(PIC2_CMD, 0x20);
        }
        outb(PIC1_CMD, 0x20);
    }
}
