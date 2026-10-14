//! gfxtest MODE: tests for the screen, input, memory and window system calls.
//!
//! Self-test modes (run by the kernel's self-test boot; exit status 0 = all good):
//!   misuse     a plain program using calls it may not use, and bad arguments
//!   alloc      mem_alloc: contents, separation, limits
//!   lifecycle  acts as the desktop: claims the screen, starts a window-owning
//!              child, maps its window, checks the pixels, sends it an event,
//!              watches the window disappear when the child exits
//!   window     (child of lifecycle) owns a window
//!   claim2     (child) tries to claim the screen while it is taken
//!   crash      claims the screen, then crashes; the kernel must take it back
//!
//! Interactive modes (driven from outside, by scripts/gfx_test.py through QEMU):
//!   input      claims the screen and prints every input event as "EV kind a b c"
//!   draw       claims the screen and paints a known pattern over all of it

#![no_std]
#![no_main]

use core::ptr::{read_volatile, write_volatile};
use nova::errno::*;
use nova::*;

static mut FAILURES: u32 = 0;

fn fail(what: &str, got: i64, wanted: i64) {
    println!("FAIL {}: got {}, wanted {}", what, got, wanted);
    unsafe { FAILURES += 1 };
}

fn expect(what: &str, got: i64, wanted: i64) {
    if got != wanted {
        fail(what, got, wanted);
    }
}

fn expect_true(what: &str, ok: bool) {
    if !ok {
        println!("FAIL {}", what);
        unsafe { FAILURES += 1 };
    }
}

fn verdict(name: &str) -> i32 {
    let failures = unsafe { FAILURES };
    if failures == 0 {
        println!("gfxtest {}: all checks passed", name);
        0
    } else {
        println!("gfxtest {}: {} check(s) failed", name, failures);
        1
    }
}

/// The picture the window child draws, so the desktop can verify every pixel.
fn pattern(x: u32, y: u32) -> u32 {
    (((x * 7 + y * 13) & 0xFF) << 16) | (((x ^ y) & 0xFF) << 8) | 0x5A
}

fn spawn_mode(mode: &str) -> i64 {
    let mut blob = [0u8; 64];
    let mut len = 0;
    for part in ["gfxtest", mode] {
        blob[len..len + part.len()].copy_from_slice(part.as_bytes());
        len += part.len();
        blob[len] = 0;
        len += 1;
    }
    spawn(b"/bin/gfxtest\0", &blob[..len], 2, None)
}

// ---- misuse ---------------------------------------------------------------------------------

fn misuse() -> i32 {
    let mut event = Event::default();
    let mut list = [WinInfo::EMPTY; 4];

    // Only the desktop (the program that owns the screen) may use these.
    expect("win_list as an ordinary program", win_list(&mut list), EPERM);
    expect("win_map as an ordinary program", win_map(1), EPERM);
    expect("win_post as an ordinary program", win_post(1, &event), EPERM);
    expect("input_next as an ordinary program", input_next(&mut event, 0), EPERM);

    // Things that do not exist.
    expect("win_event on a missing window", win_event(9999, &mut event, 0), ENOENT);
    expect("win_flush on a missing window", win_flush(9999), ENOENT);
    expect("win_destroy on a missing window", win_destroy(9999), ENOENT);
    expect("win_destroy of the largest possible id", win_destroy(u32::MAX), ENOENT);
    expect(
        "win_destroy with an id too big for 32 bits",
        unsafe { syscall(SYS_WIN_DESTROY, 1 << 40, 0, 0, 0, 0) },
        EINVAL,
    );

    // Bad window sizes and titles.
    let size_error = |w: u32, h: u32| match win_create(w, h, b"x") {
        Ok(_) => 0,
        Err(e) => e,
    };
    expect("window of width 0", size_error(0, 10), EINVAL);
    expect("window of height 0", size_error(10, 0), EINVAL);
    expect("window too wide", size_error(4097, 10), EINVAL);
    expect("window too big in total", size_error(4096, 4096), EINVAL);
    let long_title = [b'a'; 100];
    expect(
        "title longer than allowed",
        match win_create(10, 10, &long_title) {
            Ok(_) => 0,
            Err(e) => e,
        },
        EINVAL,
    );

    // Bad pointers: the calls fail cleanly and change nothing.
    let mut out = [0u64; 2];
    let raw = |a1, a2, a3, a4, a5| unsafe { syscall(SYS_WIN_CREATE, a1, a2, a3, a4, a5) };
    expect("win_create with a bad title pointer", raw(10, 10, 0xdead, 3, out.as_mut_ptr() as u64), EFAULT);
    expect("win_create with a bad result pointer", raw(10, 10, b"ok".as_ptr() as u64, 2, 1), EFAULT);
    expect("win_create with a kernel result pointer", raw(10, 10, b"ok".as_ptr() as u64, 2, 0xffff_8000_0000_0000), EFAULT);
    expect("win_event with a bad result pointer", unsafe { syscall(SYS_WIN_EVENT, 1, 8, 0, 0, 0) }, EFAULT);
    expect("gfx_claim with a bad pointer", unsafe { syscall(SYS_GFX_CLAIM, 8, 0, 0, 0, 0) }, EFAULT);
    expect("gfx_claim with a kernel pointer", unsafe { syscall(SYS_GFX_CLAIM, 0xffff_8000_0000_0000, 0, 0, 0, 0) }, EFAULT);

    // Memory requests that make no sense.
    expect("mem_alloc of zero pages", mem_alloc(0), EINVAL);
    expect("mem_alloc of an absurd size", mem_alloc(1 << 40), EINVAL);
    expect("mem_alloc over the one-request limit", mem_alloc(16385), EINVAL);

    verdict("misuse")
}

// ---- alloc ----------------------------------------------------------------------------------------

fn alloc_test() -> i32 {
    const PAGE: u64 = 4096;
    let a = mem_alloc(8);
    expect_true("first allocation succeeds", a > 0);
    let words = (8 * PAGE / 8) as usize;
    let first = unsafe { core::slice::from_raw_parts_mut(a as *mut u64, words) };
    expect_true("new memory is zeroed", first.iter().all(|&w| w == 0));
    for (i, word) in first.iter_mut().enumerate() {
        *word = i as u64 ^ 0xA5A5_A5A5_0000_0000;
    }

    let b = mem_alloc(4);
    expect_true("second allocation succeeds", b > 0);
    // Separate runs, with an unmapped guard page between them.
    expect_true("second run starts after the first plus a guard page", b as u64 >= a as u64 + 9 * PAGE);
    let second = unsafe { core::slice::from_raw_parts_mut(b as *mut u64, (4 * PAGE / 8) as usize) };
    expect_true("second run is zeroed", second.iter().all(|&w| w == 0));
    second.fill(0x1111_2222_3333_4444);

    let intact = first
        .iter()
        .enumerate()
        .all(|(i, &w)| w == i as u64 ^ 0xA5A5_A5A5_0000_0000);
    expect_true("writing the second run did not disturb the first", intact);

    // The per-process limit is 32768 pages: two requests of 16384 fit, a third does not.
    let big1 = mem_alloc(16384);
    expect_true("a 64 MiB allocation works", big1 > 0);
    let big2 = mem_alloc(16384);
    // 8 + 4 pages are already in use, so the second big one is over the limit.
    expect("allocation over the per-process limit", big2, ENOMEM);
    let freed_then_small = mem_alloc(1);
    expect_true("a small allocation still works after hitting the limit", freed_then_small > 0);
    verdict("alloc")
}

// ---- the desktop role, tested from both sides ------------------------------------------------

fn lifecycle() -> i32 {
    let mut info = ScreenInfo::default();
    expect("gfx_claim", gfx_claim(&mut info), 0);
    expect_true("screen is at least 640x480", info.width >= 640 && info.height >= 480);
    expect_true("rows are at least as long as the pixels in them", info.pitch >= info.width * 4);
    expect_true("framebuffer is mapped", info.address != 0 && info.bytes >= info.pitch * info.height);

    let mut second = ScreenInfo::default();
    expect("claiming twice", gfx_claim(&mut second), EBUSY);

    // The first event tells the desktop where the pointer is.
    let mut event = Event::default();
    expect("first input event", input_next(&mut event, 0), 1);
    expect("it is a pointer position", event.kind as i64, EV_MOUSE_MOVE as i64);
    expect("no further events yet", input_next(&mut event, 0), 0);

    let mut list = [WinInfo::EMPTY; 8];
    expect("no windows at the start", win_list(&mut list), 0);

    // A failed creation must not leave a window behind.
    let raw_create = unsafe { syscall(SYS_WIN_CREATE, 10, 10, b"ok".as_ptr() as u64, 2, 1) };
    expect("window creation with a bad result pointer", raw_create, EFAULT);
    expect("and no window was left behind", win_list(&mut list), 0);

    // Someone else cannot claim the screen while we hold it.
    let claimer = spawn_mode("claim2");
    expect_true("claim2 started", claimer > 0);
    expect("claim2 reports EBUSY and exits happily", waitpid(claimer as u64), 0);

    // Start a program that owns a window.
    let child = spawn_mode("window");
    expect_true("window child started", child > 0);
    let mut found = 0;
    for _ in 0..300 {
        input_next(&mut event, 10);
        found = win_list(&mut list);
        if found >= 1 {
            break;
        }
    }
    expect("the window appears", found, 1);
    let w = list[0];
    expect("window width", w.width as i64, 64);
    expect("window height", w.height as i64, 48);
    expect_true("window title", w.title() == b"gfx-test");
    expect_true("window owner is the child", w.owner == child as u64);

    // Wait for the child to finish drawing (it flushes once when done).
    let mut drawn = false;
    for _ in 0..300 {
        let mut now = [WinInfo::EMPTY; 8];
        win_list(&mut now);
        if now[0].serial >= 1 {
            drawn = true;
            break;
        }
        input_next(&mut event, 10);
    }
    expect_true("the window was flushed after drawing", drawn);

    // A window belongs to the program that made it: nobody else, not even the
    // desktop, may flush it, destroy it or read its events.
    expect("flushing someone else's window", win_flush(w.id), EPERM);
    expect("destroying someone else's window", win_destroy(w.id), EPERM);
    let mut stolen = Event::default();
    expect("reading someone else's window events", win_event(w.id, &mut stolen, 0), EPERM);

    let addr = win_map(w.id);
    expect_true("window maps", addr > 0);
    let pixels = unsafe { core::slice::from_raw_parts(addr as *const u32, 64 * 48) };
    let mut wrong = 0;
    for y in 0..48u32 {
        for x in 0..64u32 {
            if pixels[(y * 64 + x) as usize] != pattern(x, y) {
                wrong += 1;
            }
        }
    }
    expect("window pixels match what the child drew", wrong, 0);

    // The desktop may send window events but cannot forge input events.
    let key = Event { kind: EV_WIN_KEY, a: b'k' as i32, b: 0, c: 0 };
    expect("win_post of a key event", win_post(w.id, &key), 0);
    let forged = Event { kind: EV_KEY, a: b'k' as i32, b: 0, c: 0 };
    expect("win_post of an input event is refused", win_post(w.id, &forged), EINVAL);
    expect("win_post to a missing window", win_post(12345, &key), ENOENT);

    expect("the child saw the event, checked its pixels and exited happily", waitpid(child as u64), 0);

    // The window goes away with its owner...
    expect("the window is gone after its owner exits", win_list(&mut list), 0);

    // ...but this process's mapping of it must stay valid: its memory is kept
    // alive until nobody maps it. Make the kernel hand out plenty of fresh memory
    // and check the old window's pixels are still exactly as they were.
    let scratch = mem_alloc(2048);
    expect_true("scratch memory", scratch > 0);
    unsafe { core::slice::from_raw_parts_mut(scratch as *mut u32, 2048 * 1024).fill(0xFFFF_FFFF) };
    let mut still_wrong = 0;
    for y in 0..48u32 {
        for x in 0..64u32 {
            if pixels[(y * 64 + x) as usize] != pattern(x, y) {
                still_wrong += 1;
            }
        }
    }
    expect("a dead window's memory is not reused while still mapped", still_wrong, 0);

    // Windows can be destroyed by their owner: do it ourselves (we may create too).
    match win_create(16, 16, b"mine") {
        Ok((id, address)) => {
            unsafe { write_volatile(address as *mut u32, 0x00AB_CDEF) };
            expect("destroying our own window", win_destroy(id), 0);
            expect("destroying it twice", win_destroy(id), ENOENT);
            expect("the window list is empty again", win_list(&mut list), 0);
        }
        Err(e) => fail("creating our own window", e, 0),
    }

    verdict("lifecycle")
}

/// Child of `lifecycle`: owns a window, draws a known picture, waits for a key.
fn window_child() -> i32 {
    let (id, addr) = match win_create(64, 48, b"gfx-test") {
        Ok(created) => created,
        Err(e) => {
            fail("win_create in the child", e, 0);
            return verdict("window");
        }
    };
    let pixels = unsafe { core::slice::from_raw_parts_mut(addr as *mut u32, 64 * 48) };
    for y in 0..48u32 {
        for x in 0..64u32 {
            pixels[(y * 64 + x) as usize] = pattern(x, y);
        }
    }
    expect("win_flush", win_flush(id), 0);

    // An ordinary window owner cannot use the desktop's calls.
    let mut list = [WinInfo::EMPTY; 2];
    expect("win_list from a window owner", win_list(&mut list), EPERM);
    expect("win_post from a window owner", win_post(id, &Event::default()), EPERM);

    // Wait for the desktop's event.
    let mut event = Event::default();
    let got = win_event(id, &mut event, 600);
    expect("an event arrives", got, 1);
    expect("it is the key the desktop sent", (event.kind as i64) << 8 | event.a as i64, (EV_WIN_KEY as i64) << 8 | b'k' as i64);
    // Nothing else is waiting.
    expect("no more events", win_event(id, &mut event, 0), 0);

    // The picture is still intact.
    let mut wrong = 0;
    for y in 0..48u32 {
        for x in 0..64u32 {
            if pixels[(y * 64 + x) as usize] != pattern(x, y) {
                wrong += 1;
            }
        }
    }
    expect("the child's picture is intact", wrong, 0);
    // Exit without destroying the window: the kernel must clean it up.
    verdict("window")
}

fn claim2() -> i32 {
    let mut info = ScreenInfo::default();
    expect("gfx_claim while another program has the screen", gfx_claim(&mut info), EBUSY);
    verdict("claim2")
}

fn crash() -> i32 {
    let mut info = ScreenInfo::default();
    expect("gfx_claim before crashing", gfx_claim(&mut info), 0);
    println!("claimed the screen; now crashing");
    // Read address 0: the kernel kills this process, which must give the
    // screen and the keyboard back.
    let value = unsafe { read_volatile(core::ptr::null::<u8>()) };
    value as i32
}

// ---- interactive modes ----------------------------------------------------------------------------

fn input_mode() -> i32 {
    let mut info = ScreenInfo::default();
    if gfx_claim(&mut info) != 0 {
        println!("input: could not claim the screen");
        return 1;
    }
    println!("input: ready {}x{}", info.width, info.height);
    let mut event = Event::default();
    loop {
        if input_next(&mut event, FOREVER) == 1 {
            println!("EV {} {} {} {}", event.kind, event.a, event.b, event.c);
            if event.kind == EV_KEY && event.a == b'q' as i32 {
                println!("input: done");
                return 0;
            }
        }
    }
}

/// What `draw` paints at (x, y), as red, green, blue.
fn drawn_color(x: u64, y: u64, w: u64, h: u64) -> (u8, u8, u8) {
    if x < 3 || y < 3 || x >= w - 3 || y >= h - 3 {
        return (255, 255, 255);
    }
    match ((x >> 6) + (y >> 6)) % 4 {
        0 => (220, 30, 30),
        1 => (30, 220, 30),
        2 => (30, 30, 220),
        _ => (240, 240, 240),
    }
}

fn draw_mode() -> i32 {
    let mut info = ScreenInfo::default();
    if gfx_claim(&mut info) != 0 {
        println!("draw: could not claim the screen");
        return 1;
    }
    let (w, h) = (info.width, info.height);
    let base = info.address as *mut u8;
    for y in 0..h {
        let row = unsafe { base.add((y * info.pitch) as usize) } as *mut u32;
        for x in 0..w {
            let (r, g, b) = drawn_color(x, y, w, h);
            let pixel = (r as u32) << info.red_shift | (g as u32) << info.green_shift | (b as u32) << info.blue_shift;
            unsafe { write_volatile(row.add(x as usize), pixel) };
        }
    }
    println!("draw: done {}x{}", w, h);
    let mut event = Event::default();
    loop {
        if input_next(&mut event, FOREVER) == 1 && event.kind == EV_KEY && event.a == b'q' as i32 {
            return 0;
        }
    }
}

fn main(args: &Args) -> i32 {
    match args.get(1) {
        "misuse" => misuse(),
        "alloc" => alloc_test(),
        "lifecycle" => lifecycle(),
        "window" => window_child(),
        "claim2" => claim2(),
        "crash" => crash(),
        "input" => input_mode(),
        "draw" => draw_mode(),
        other => {
            println!("usage: gfxtest misuse|alloc|lifecycle|input|draw (got {:?})", other);
            2
        }
    }
}

entry!(main);
