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

// ---- high-resolution time -------------------------------------------------------------
//
// The 100 Hz tick is too coarse to time a network round trip. The CPU's cycle
// counter (TSC) is fast, so it is measured once against the PIT's channel 2, which
// counts at a known 1.193182 MHz, and then used as a microsecond clock.

use crate::port::inb;

static TSC_KHZ: AtomicU64 = AtomicU64::new(0);
static TSC_AT_BOOT: AtomicU64 = AtomicU64::new(0);

fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Measure the cycle counter's speed (takes about 20 ms; needs no interrupts).
pub fn calibrate_tsc() {
    const MEASURE_MS: u64 = 20;
    let count = (PIT_FREQUENCY as u64 * MEASURE_MS / 1000) as u16;
    let (start, end);
    unsafe {
        // Gate channel 2 off, speaker off; program mode 0 (one-shot), then open the gate.
        outb(0x61, inb(0x61) & !0x03);
        outb(0x43, 0xB0);
        outb(0x42, count as u8);
        outb(0x42, (count >> 8) as u8);
        outb(0x61, (inb(0x61) & !0x02) | 0x01);
        start = rdtsc();
        // OUT2 (bit 5 of port 0x61) goes high when the count reaches zero.
        let mut guard = 0u64;
        while inb(0x61) & 0x20 == 0 {
            guard += 1;
            if guard > 50_000_000 {
                return; // no PIT channel 2 (should not happen); keep the clock at zero
            }
        }
        end = rdtsc();
    }
    let khz = (end - start) / MEASURE_MS;
    TSC_KHZ.store(khz.max(1), Ordering::Relaxed);
    TSC_AT_BOOT.store(end, Ordering::Relaxed);
}

/// Microseconds since `calibrate_tsc` ran (0 if it did not).
pub fn now_us() -> u64 {
    let khz = TSC_KHZ.load(Ordering::Relaxed);
    if khz == 0 {
        return 0;
    }
    let delta = rdtsc().wrapping_sub(TSC_AT_BOOT.load(Ordering::Relaxed));
    ((delta as u128 * 1000) / khz as u128) as u64
}

/// The cycle counter's frequency in kHz (0 if unknown).
pub fn tsc_khz() -> u64 {
    TSC_KHZ.load(Ordering::Relaxed)
}
