//! Console input: bytes typed on the PS/2 keyboard or sent to the serial port
//! collect here until a process reads them from stdin (file descriptor 0).
//!
//! One process at a time is the *foreground* process (the shell sets it while a command
//! runs). Control-C (byte 3) does not reach the reader then: it sends SIGINT to the
//! foreground process instead.

use super::waitq::WaitQueue;
use crate::sync::SpinLock;
use core::sync::atomic::{AtomicU64, Ordering};

const CAPACITY: usize = 256;
const CTRL_C: u8 = 3;

/// Fixed ring so that `push_input` never allocates in interrupt context.
struct Ring {
    bytes: [u8; CAPACITY],
    head: usize,
    len: usize,
}

static INPUT: SpinLock<Ring> = SpinLock::new(Ring {
    bytes: [0; CAPACITY],
    head: 0,
    len: 0,
});
static READABLE: WaitQueue = WaitQueue::new();
/// Process group that Control-C is sent to; 0 = nobody (the byte is delivered as input).
static FOREGROUND: AtomicU64 = AtomicU64::new(0);

pub fn set_foreground(pid: u64) {
    FOREGROUND.store(pid, Ordering::Release);
}

pub fn foreground() -> u64 {
    FOREGROUND.load(Ordering::Acquire)
}

/// A process that ends stops being the foreground process.
pub fn clear_foreground(pid: u64) {
    let _ = FOREGROUND.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed);
}

/// Add a received byte; drops it if the buffer is full. Called from interrupt
/// handlers and, in tests, from threads.
pub fn push_input(byte: u8) {
    if byte == CTRL_C {
        let fg = FOREGROUND.load(Ordering::Acquire);
        if fg != 0 && crate::proc::send_signal_group(fg, crate::proc::SIGINT).is_ok() {
            return;
        }
    }
    {
        let mut ring = INPUT.lock();
        if ring.len == CAPACITY {
            return;
        }
        let tail = (ring.head + ring.len) % CAPACITY;
        ring.bytes[tail] = byte;
        ring.len += 1;
    }
    READABLE.wake_one();
}

/// Whether `read` would return without sleeping.
pub fn input_ready() -> bool {
    INPUT.lock().len > 0
}

/// Read up to `out.len()` bytes, sleeping until at least one is available (or the reading
/// process is sent a signal that ends it, in which case 0 comes back).
pub fn read(out: &mut [u8]) -> usize {
    if out.is_empty() {
        return 0;
    }
    let mut n = 0;
    READABLE.wait_until_intr(|| {
        let mut ring = INPUT.lock();
        if ring.len == 0 {
            return false;
        }
        while n < out.len() && ring.len > 0 {
            out[n] = ring.bytes[ring.head];
            ring.head = (ring.head + 1) % CAPACITY;
            ring.len -= 1;
            n += 1;
        }
        true
    });
    n
}
