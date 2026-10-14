//! The window system's kernel side: who owns the screen, shared memory, user
//! heap pages and the table of windows.
//!
//! The desktop (compositor) is an ordinary user program. It claims the screen
//! (the framebuffer is mapped into its address space), becomes the input owner,
//! and then draws every window itself. The kernel only provides the pieces:
//!
//!  * `claim_screen`: map the framebuffer, take over input, silence the text console.
//!  * `user_alloc`: fresh zeroed pages for a program's own use (back buffers).
//!  * windows: a program asks for a window and gets a pixel buffer in shared
//!    memory; the desktop maps the same memory and draws it. Events (keys, mouse,
//!    close) flow back to the window's owner through a per-window queue.
//!
//! Safety rules this module upholds:
//!  * Shared frames are owned by an `Arc<Shm>`. A process's mapping of them is
//!    marked NOFREE (address-space teardown never frees them), and the process
//!    keeps an `Arc` so the frames outlive its mapping.
//!  * If the desktop exits or crashes, the screen and the input are released and
//!    the text console is repainted.

use crate::gfx::console;
use crate::gfx::fb::FramebufferInfo;
use crate::input::{self, Event, EventRing};
use crate::ipc::waitq::WaitQueue;
use crate::mem::{pmm, vmm, PAGE_SIZE};
use crate::sync::SpinLock;
use crate::timer;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// Errors (negated by the system call layer).
pub const EPERM: i64 = -1;
pub const ENOENT: i64 = -2;
pub const ENOMEM: i64 = -12;
pub const EBUSY: i64 = -16;
pub const ENODEV: i64 = -19;
pub const EINVAL: i64 = -22;

/// Where the framebuffer appears in the desktop's address space.
pub const FB_USER_BASE: u64 = 0x0000_4000_0000_0000;
/// User heap (`mem_alloc`) and shared-memory mappings start here.
const HEAP_BASE: u64 = 0x0000_5000_0000_0000;
const SHM_BASE: u64 = 0x0000_5800_0000_0000;

/// Limits, so one program cannot exhaust the machine.
const MAX_ALLOC_PAGES: u64 = 16384; // one request: 64 MiB
const MAX_HEAP_PAGES: u64 = 32768; // per process: 128 MiB
const MAX_SHM_PAGES: u64 = 32768; // per process: 128 MiB
pub const MAX_WINDOW_DIM: u32 = 4096;
const MAX_WINDOW_BYTES: u64 = 16 * 1024 * 1024;
const MAX_WINDOWS: usize = 64;
const MAX_WINDOWS_PER_PROCESS: usize = 8;
pub const TITLE_MAX: usize = 31;

// ---- shared memory ---------------------------------------------------------------

/// A run of physical frames that several processes can map. The frames go back
/// to the allocator when the last `Arc` is dropped.
pub struct Shm {
    frames: Vec<u64>,
}

impl Shm {
    /// `bytes` of zeroed memory; None if the machine is out of frames.
    fn new(bytes: u64) -> Option<Arc<Shm>> {
        let pages = bytes.div_ceil(PAGE_SIZE) as usize;
        let mut frames = Vec::with_capacity(pages);
        for _ in 0..pages {
            match pmm::alloc_zeroed_frame() {
                Some(frame) => frames.push(frame),
                None => {
                    for frame in frames {
                        pmm::free_frame(frame);
                    }
                    return None;
                }
            }
        }
        Some(Arc::new(Shm { frames }))
    }

    pub fn pages(&self) -> u64 {
        self.frames.len() as u64
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        for &frame in &self.frames {
            pmm::free_frame(frame);
        }
    }
}

/// A process's dynamic memory: its heap pages and its shared mappings.
/// The cursors only ever move up; unmapped address space is never reused.
pub struct UserMem {
    heap_next: AtomicU64,
    heap_pages: AtomicU64,
    shm_next: AtomicU64,
    shm_pages: AtomicU64,
    /// Keeps shared frames alive for as long as this process may touch them.
    maps: SpinLock<Vec<Arc<Shm>>>,
}

impl UserMem {
    pub fn new() -> UserMem {
        UserMem {
            heap_next: AtomicU64::new(HEAP_BASE),
            heap_pages: AtomicU64::new(0),
            shm_next: AtomicU64::new(SHM_BASE),
            shm_pages: AtomicU64::new(0),
            maps: SpinLock::new(Vec::new()),
        }
    }
}

/// Undo the first `done` pages of a mapping run in the live address space.
/// `free` is true for pages whose frames this run allocated itself.
fn unmap_run(base: u64, done: u64, free: bool) {
    for i in 0..done {
        if let Some(frame) = vmm::unmap(base + i * PAGE_SIZE) {
            if free {
                pmm::free_frame(frame);
            }
        }
    }
}

/// Give the calling process `pages` fresh zeroed pages; returns their address.
/// Must run in the process's own address space (a system call does).
pub fn user_alloc(mem: &UserMem, pages: u64) -> Result<u64, i64> {
    if pages == 0 || pages > MAX_ALLOC_PAGES {
        return Err(EINVAL);
    }
    if mem.heap_pages.load(Ordering::Relaxed) + pages > MAX_HEAP_PAGES {
        return Err(ENOMEM);
    }
    let base = mem.heap_next.load(Ordering::Relaxed);
    for i in 0..pages {
        let Some(frame) = pmm::alloc_zeroed_frame() else {
            unmap_run(base, i, true);
            return Err(ENOMEM);
        };
        if vmm::map(base + i * PAGE_SIZE, frame, vmm::user_flags(true, false)).is_err() {
            pmm::free_frame(frame);
            unmap_run(base, i, true);
            return Err(ENOMEM);
        }
    }
    // Leave one unmapped page after the run, so running off the end faults.
    mem.heap_next.store(base + (pages + 1) * PAGE_SIZE, Ordering::Relaxed);
    mem.heap_pages.fetch_add(pages, Ordering::Relaxed);
    Ok(base)
}

/// Map shared memory into the calling process; returns its address.
fn map_shm(mem: &UserMem, shm: &Arc<Shm>, writable: bool) -> Result<u64, i64> {
    let pages = shm.pages();
    if mem.shm_pages.load(Ordering::Relaxed) + pages > MAX_SHM_PAGES {
        return Err(ENOMEM);
    }
    let base = mem.shm_next.load(Ordering::Relaxed);
    let flags = vmm::user_flags(writable, false) | vmm::NOFREE;
    for (i, &frame) in shm.frames.iter().enumerate() {
        if vmm::map(base + i as u64 * PAGE_SIZE, frame, flags).is_err() {
            unmap_run(base, i as u64, false);
            return Err(ENOMEM);
        }
    }
    mem.maps.lock().push(shm.clone());
    mem.shm_next.store(base + (pages + 1) * PAGE_SIZE, Ordering::Relaxed);
    mem.shm_pages.fetch_add(pages, Ordering::Relaxed);
    Ok(base)
}

// ---- the screen -----------------------------------------------------------------------

/// Process id of the desktop (the program that owns the screen), or 0.
static COMPOSITOR: AtomicU64 = AtomicU64::new(0);

pub fn is_compositor(pid: u64) -> bool {
    pid != 0 && COMPOSITOR.load(Ordering::Relaxed) == pid
}

/// Take over the screen: map the framebuffer into the caller, become the input
/// owner, and stop the kernel's text console from drawing.
pub fn claim_screen(pid: u64) -> Result<FramebufferInfo, i64> {
    let info = console::framebuffer_info().ok_or(ENODEV)?;
    let pages = info.bytes.div_ceil(PAGE_SIZE);
    if pages == 0 || pages > MAX_HEAP_PAGES {
        return Err(EINVAL);
    }
    if COMPOSITOR
        .compare_exchange(0, pid, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return Err(EBUSY);
    }
    let flags = vmm::user_flags(true, false) | vmm::NOFREE;
    for i in 0..pages {
        if vmm::map(FB_USER_BASE + i * PAGE_SIZE, info.phys + i * PAGE_SIZE, flags).is_err() {
            unmap_run(FB_USER_BASE, i, false);
            COMPOSITOR.store(0, Ordering::Release);
            return Err(ENOMEM);
        }
    }
    if !input::claim(pid) {
        unmap_run(FB_USER_BASE, pages, false);
        COMPOSITOR.store(0, Ordering::Release);
        return Err(EBUSY);
    }
    console::suspend();
    Ok(info)
}

/// Give the screen and the keyboard back to the kernel console.
fn release_screen(pid: u64) {
    if COMPOSITOR
        .compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        input::release(pid);
        console::resume();
    }
}

// ---- windows -----------------------------------------------------------------------------

pub struct Window {
    pub id: u32,
    pub owner: u64,
    pub w: u32,
    pub h: u32,
    pub title: [u8; 32],
    pub title_len: usize,
    shm: Arc<Shm>,
    /// Bumped each time the owner says the contents changed.
    serial: AtomicU64,
    events: SpinLock<EventRing<64>>,
    readable: WaitQueue,
}

impl Window {
    pub fn serial(&self) -> u64 {
        self.serial.load(Ordering::Relaxed)
    }
}

static WINDOWS: SpinLock<BTreeMap<u32, Arc<Window>>> = SpinLock::new(BTreeMap::new());
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Create a window of `w` x `h` pixels owned by `owner`. Each pixel is 4 bytes,
/// 0x00RRGGBB (red in bits 16-23) whatever the screen's own channel order is;
/// the desktop converts when it paints. Returns the window's id and the address
/// of its pixels in the owner's address space.
pub fn create_window(owner: u64, mem: &UserMem, w: u32, h: u32, title: &[u8]) -> Result<(u32, u64), i64> {
    if w == 0 || h == 0 || w > MAX_WINDOW_DIM || h > MAX_WINDOW_DIM {
        return Err(EINVAL);
    }
    let bytes = w as u64 * h as u64 * 4;
    if bytes > MAX_WINDOW_BYTES {
        return Err(EINVAL);
    }
    {
        let table = WINDOWS.lock();
        if table.len() >= MAX_WINDOWS || table.values().filter(|win| win.owner == owner).count() >= MAX_WINDOWS_PER_PROCESS {
            return Err(ENOMEM);
        }
    }
    let shm = Shm::new(bytes).ok_or(ENOMEM)?;
    let addr = map_shm(mem, &shm, true)?;

    let mut title_bytes = [0u8; 32];
    let title_len = title.len().min(TITLE_MAX);
    title_bytes[..title_len].copy_from_slice(&title[..title_len]);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let window = Arc::new(Window {
        id,
        owner,
        w,
        h,
        title: title_bytes,
        title_len,
        shm,
        serial: AtomicU64::new(0),
        events: SpinLock::new(EventRing::new()),
        readable: WaitQueue::new(),
    });
    WINDOWS.lock().insert(id, window);
    input::damage();
    Ok((id, addr))
}

fn find(id: u32) -> Option<Arc<Window>> {
    WINDOWS.lock().get(&id).cloned()
}

/// Remove a window; only its owner may.
pub fn destroy_window(pid: u64, id: u32) -> Result<(), i64> {
    let mut table = WINDOWS.lock();
    match table.get(&id) {
        None => return Err(ENOENT),
        Some(window) if window.owner != pid => return Err(EPERM),
        Some(_) => {}
    }
    table.remove(&id);
    drop(table);
    input::damage();
    Ok(())
}

/// The owner says the window's pixels changed.
pub fn flush_window(pid: u64, id: u32) -> Result<(), i64> {
    let window = find(id).ok_or(ENOENT)?;
    if window.owner != pid {
        return Err(EPERM);
    }
    window.serial.fetch_add(1, Ordering::Relaxed);
    input::damage();
    Ok(())
}

/// A snapshot of every window, oldest first.
pub fn list_windows() -> Vec<Arc<Window>> {
    WINDOWS.lock().values().cloned().collect()
}

/// Map a window's pixels (read-only) into the calling process (the desktop).
pub fn map_window(mem: &UserMem, id: u32) -> Result<u64, i64> {
    let window = find(id).ok_or(ENOENT)?;
    map_shm(mem, &window.shm, false)
}

/// Deliver an event to a window's owner.
pub fn post_event(id: u32, event: Event) -> Result<(), i64> {
    let window = find(id).ok_or(ENOENT)?;
    window.events.lock().push(event);
    window.readable.wake_one();
    Ok(())
}

/// The next event for a window, for its owner. `timeout_ticks`: None waits as
/// long as it takes, Some(0) only looks, Some(n) waits up to n timer ticks.
pub fn next_window_event(pid: u64, id: u32, timeout_ticks: Option<u64>) -> Result<Option<Event>, i64> {
    let window = find(id).ok_or(ENOENT)?;
    if window.owner != pid {
        return Err(EPERM);
    }
    let mut got = None;
    let mut take = || match window.events.lock().pop() {
        Some(event) => {
            got = Some(event);
            true
        }
        None => false,
    };
    match timeout_ticks {
        None => window.readable.wait_until(&mut take),
        Some(0) => {
            take();
        }
        Some(ticks) => {
            window.readable.wait_until_deadline(timer::ticks() + ticks, &mut take);
        }
    }
    Ok(got)
}

/// A process ended (exited or was killed): its windows go away, and if it was
/// the desktop, the screen and keyboard return to the text console.
pub fn on_process_exit(pid: u64) {
    let had_windows = {
        let mut table = WINDOWS.lock();
        let before = table.len();
        table.retain(|_, window| window.owner != pid);
        table.len() != before
    };
    if had_windows {
        input::damage();
    }
    release_screen(pid);
}
