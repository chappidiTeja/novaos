//! Input events (mouse and keyboard) and who receives them.
//!
//! Normally the keyboard feeds the console (stdin). When a program claims the
//! screen (the desktop), it also becomes the *input owner*: keyboard and mouse
//! then arrive as events on one queue that only it can read. If the owner exits
//! or crashes, ownership is released and the keyboard goes back to the console.

use crate::ipc::waitq::WaitQueue;
use crate::sync::SpinLock;
use crate::timer;
use core::sync::atomic::{AtomicU64, Ordering};

/// One input event: 16 bytes, the same layout in the kernel and in user space.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub kind: u32,
    pub a: i32,
    pub b: i32,
    pub c: i32,
}

impl Event {
    pub const fn new(kind: u32, a: i32, b: i32, c: i32) -> Event {
        Event { kind, a, b, c }
    }
}

// Events delivered to the input owner (the desktop):
/// The pointer moved: a = x, b = y (screen pixels), c = buttons held.
pub const EV_MOUSE_MOVE: u32 = 1;
/// A button changed: a = x, b = y, c = buttons held now.
pub const EV_MOUSE_BUTTON: u32 = 2;
/// A key was typed: a = ASCII code.
pub const EV_KEY: u32 = 3;
/// Something on screen changed (a window appeared, was redrawn or went away).
pub const EV_DAMAGE: u32 = 4;

// Events the desktop posts to a window's owner:
/// A key for this window: a = ASCII code.
pub const EV_WIN_KEY: u32 = 10;
/// The pointer moved over this window: a, b = position inside the window, c = buttons.
pub const EV_WIN_MOVE: u32 = 11;
/// A button changed over this window: a, b = position inside the window, c = buttons.
pub const EV_WIN_BUTTON: u32 = 12;
/// The user asked to close the window.
pub const EV_WIN_CLOSE: u32 = 13;
/// Focus changed: a = 1 gained, 0 lost.
pub const EV_WIN_FOCUS: u32 = 14;

pub const BUTTON_LEFT: i32 = 1;
pub const BUTTON_RIGHT: i32 = 2;
pub const BUTTON_MIDDLE: i32 = 4;

/// Events that only matter in their latest form, so a new one replaces the
/// previous one if it is still waiting.
fn coalesces(kind: u32) -> bool {
    kind == EV_MOUSE_MOVE || kind == EV_DAMAGE || kind == EV_WIN_MOVE
}

/// A fixed-size ring of events (no allocation, so interrupt handlers can use it).
pub struct EventRing<const N: usize> {
    events: [Event; N],
    head: usize,
    len: usize,
}

impl<const N: usize> EventRing<N> {
    pub const fn new() -> Self {
        EventRing { events: [Event::new(0, 0, 0, 0); N], head: 0, len: 0 }
    }

    /// Add an event. A move (or damage notice) merges into an identical kind
    /// that is still at the end of the queue. Returns false if the ring is full
    /// and the event was dropped.
    pub fn push(&mut self, event: Event) -> bool {
        if self.len > 0 && coalesces(event.kind) {
            let last = (self.head + self.len - 1) % N;
            if self.events[last].kind == event.kind {
                self.events[last] = event;
                return true;
            }
        }
        if self.len == N {
            return false;
        }
        let tail = (self.head + self.len) % N;
        self.events[tail] = event;
        self.len += 1;
        true
    }

    pub fn pop(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let event = self.events[self.head];
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(event)
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

static QUEUE: SpinLock<EventRing<256>> = SpinLock::new(EventRing::new());
static READABLE: WaitQueue = WaitQueue::new();
/// Process id of the input owner, or 0 for none.
static OWNER: AtomicU64 = AtomicU64::new(0);

struct Pointer {
    x: i32,
    y: i32,
    buttons: i32,
    max_x: i32,
    max_y: i32,
}

static POINTER: SpinLock<Pointer> = SpinLock::new(Pointer { x: 0, y: 0, buttons: 0, max_x: 1023, max_y: 767 });

/// Tell the pointer how big the screen is; it starts in the middle.
pub fn set_bounds(width: usize, height: usize) {
    let mut p = POINTER.lock();
    p.max_x = (width as i32 - 1).max(0);
    p.max_y = (height as i32 - 1).max(0);
    p.x = p.max_x / 2;
    p.y = p.max_y / 2;
    p.buttons = 0;
}

/// The pointer's position and the buttons held.
pub fn pointer() -> (i32, i32, i32) {
    let p = POINTER.lock();
    (p.x, p.y, p.buttons)
}

pub fn owner() -> u64 {
    OWNER.load(Ordering::Relaxed)
}

/// Become the input owner. The first event the owner sees is the pointer's
/// current position, so it does not have to guess where the pointer is.
pub fn claim(pid: u64) -> bool {
    if OWNER.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return false;
    }
    let (x, y, buttons) = pointer();
    let mut queue = QUEUE.lock();
    queue.clear();
    queue.push(Event::new(EV_MOUSE_MOVE, x, y, buttons));
    drop(queue);
    READABLE.wake_all();
    true
}

/// Give up ownership if `pid` holds it (also called when it exits or crashes).
pub fn release(pid: u64) {
    if OWNER.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        QUEUE.lock().clear();
    }
}

fn push_to_owner(event: Event) {
    if owner() == 0 {
        return;
    }
    QUEUE.lock().push(event);
    READABLE.wake_one();
}

/// A key was typed. It goes to the owner if there is one, otherwise to stdin.
pub fn key(code: u32) {
    if owner() != 0 {
        push_to_owner(Event::new(EV_KEY, code as i32, 0, 0));
    } else {
        if code < 0x100 {
            crate::ipc::console::push_input(code as u8);
        } else {
            for &b in crate::keyboard::escape_sequence(code) {
                crate::ipc::console::push_input(b);
            }
        }
    }
}

/// Something changed on screen; wake the desktop so it redraws.
pub fn damage() {
    push_to_owner(Event::new(EV_DAMAGE, 0, 0, 0));
}

/// One decoded mouse packet: movement in screen directions (down is positive)
/// and the buttons held now.
pub fn mouse_packet(dx: i32, dy: i32, buttons: i32) {
    let (x, y, moved, changed) = {
        let mut p = POINTER.lock();
        let x = (p.x + dx).clamp(0, p.max_x);
        let y = (p.y + dy).clamp(0, p.max_y);
        let moved = x != p.x || y != p.y;
        let changed = buttons != p.buttons;
        p.x = x;
        p.y = y;
        p.buttons = buttons;
        (x, y, moved, changed)
    };
    if owner() == 0 {
        return;
    }
    {
        let mut queue = QUEUE.lock();
        if moved {
            queue.push(Event::new(EV_MOUSE_MOVE, x, y, buttons));
        }
        if changed {
            queue.push(Event::new(EV_MOUSE_BUTTON, x, y, buttons));
        }
    }
    READABLE.wake_one();
}

/// The next event for the owner. `timeout_ticks`: None waits as long as it
/// takes, Some(0) only looks, Some(n) waits up to n timer ticks.
pub fn next(timeout_ticks: Option<u64>) -> Option<Event> {
    let mut got = None;
    let mut take = || match QUEUE.lock().pop() {
        Some(event) => {
            got = Some(event);
            true
        }
        None => false,
    };
    match timeout_ticks {
        None => {
            READABLE.wait_until_intr(&mut take);
        }
        Some(0) => {
            take();
        }
        Some(ticks) => {
            READABLE.wait_until_deadline_intr(timer::ticks() + ticks, &mut take);
        }
    }
    got
}
