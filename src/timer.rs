use crate::port::outb;
use core::sync::atomic::{AtomicU64, Ordering};

const PIT_FREQUENCY: u32 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Program PIT channel 0 to fire IRQ0 at `hz` interrupts per second.
pub fn init(hz: u32) {
    let divisor = (PIT_FREQUENCY / hz) as u16;
    unsafe {
        outb(0x43, 0x36); // channel 0, lobyte/hibyte, rate generator
        outb(0x40, divisor as u8);
        outb(0x40, (divisor >> 8) as u8);
    }
}

pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
