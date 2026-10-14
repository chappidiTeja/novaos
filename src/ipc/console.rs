//! Console input: bytes typed on the PS/2 keyboard or sent to the serial port
//! collect here until a process reads them from stdin (file descriptor 0).

use super::waitq::WaitQueue;
use crate::sync::SpinLock;

const CAPACITY: usize = 256;

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

/// Add a received byte; drops it if the buffer is full. Called from interrupt
/// handlers and, in tests, from threads.
pub fn push_input(byte: u8) {
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

/// Read up to `out.len()` bytes, sleeping until at least one is available.
pub fn read(out: &mut [u8]) -> usize {
    if out.is_empty() {
        return 0;
    }
    let mut n = 0;
    READABLE.wait_until(|| {
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
