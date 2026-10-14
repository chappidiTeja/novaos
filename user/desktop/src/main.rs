//! desktop: the NovaOS graphical desktop (the compositor).
//!
//! It takes over the screen and the keyboard and mouse, then loops: read input,
//! ask the kernel which windows exist, let the window manager (`wm`) decide what
//! the input means, carry out what it asks for (send an event to a program, start
//! a program, quit), and when something changed, draw the whole desktop into a
//! back buffer and copy it to the screen.
//!
//! Choosing "Exit desktop" in the Start menu ends it, and the kernel gives the
//! text console back.

#![no_std]
#![no_main]

use gfx::Surface;
use nova::*;
use wm::{Action, Actions, Desktop};

/// Programs the Start menu starts, as (path, argument block, argument count).
/// An argument block is the arguments as NUL-terminated strings back to back.
const PROGRAMS: [(&[u8], &[u8], usize); 3] = [
    (b"/bin/term\0", b"term\0", 1),
    (b"/bin/gdemo\0", b"gdemo\0", 1),
    (b"/bin/gdemo\0", b"gdemo\0about\0", 2),
];

/// Copy the finished picture to the screen, converting the channel order if
/// the screen's is not the 0x00RRGGBB the picture uses.
fn present(back: &Surface, back_pixels: *const u32, info: &ScreenInfo) {
    let (w, h) = (back.width(), back.height());
    let fast = info.red_shift == 16 && info.green_shift == 8 && info.blue_shift == 0;
    for y in 0..h {
        let src = unsafe { back_pixels.add(y * w) };
        let dst = (info.address + y as u64 * info.pitch) as *mut u32;
        if fast {
            unsafe { core::ptr::copy_nonoverlapping(src, dst, w) };
        } else {
            for x in 0..w {
                let p = unsafe { *src.add(x) };
                let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
                let out = (r << info.red_shift) | (g << info.green_shift) | (b << info.blue_shift);
                unsafe { dst.add(x).write(out) };
            }
        }
    }
}

fn launch(index: usize) {
    if let Some(&(path, argv, argc)) = PROGRAMS.get(index) {
        let pid = spawn(path, argv, argc, None);
        if pid < 0 {
            println!("desktop: could not start program {}: {}", index, strerror(pid));
        }
    }
}

fn main(_args: &Args) -> i32 {
    let mut info = ScreenInfo::default();
    let r = gfx_claim(&mut info);
    if r != 0 {
        eprintln!("desktop: cannot take the screen: {}", strerror(r));
        return 1;
    }
    let (w, h) = (info.width as usize, info.height as usize);

    // The picture is drawn here, then copied to the screen in one go.
    let pages = (w * h * 4).div_ceil(4096) as u64;
    let back_addr = mem_alloc(pages);
    if back_addr <= 0 {
        eprintln!("desktop: no memory for the back buffer: {}", strerror(back_addr));
        return 1;
    }
    let back_pixels = back_addr as *mut u32;
    let mut back = unsafe { Surface::from_raw(back_pixels, w, h, w) };

    let mut desk = Desktop::new(w as i32, h as i32);
    let mut actions = Actions::new();
    let mut list = [WinInfo::EMPTY; 32];
    let mut last_second = u64::MAX;
    // The kernel's first event tells us where the pointer is.
    let mut event = Event::default();
    if input_next(&mut event, 0) == 1 {
        desk.handle(event, &mut actions);
    }
    println!("desktop: ready {}x{}", w, h);

    loop {
        // Wait for input; wake every half second at the latest so the clock moves.
        if input_next(&mut event, 50) == 1 {
            desk.handle(event, &mut actions);
            // Take everything that has queued up, so one redraw covers it all.
            while input_next(&mut event, 0) == 1 {
                desk.handle(event, &mut actions);
            }
        }

        let count = win_list(&mut list);
        if count >= 0 {
            desk.sync(
                &list[..count as usize],
                &mut |id| {
                    let address = win_map(id);
                    if address > 0 {
                        Some(address as *const u32)
                    } else {
                        None
                    }
                },
                &mut actions,
            );
        }

        let mut quit = false;
        for action in actions.iter() {
            match action {
                Action::Post(id, event) => {
                    win_post(id, &event);
                }
                Action::Launch(index) => launch(index),
                Action::Quit => quit = true,
            }
        }
        actions.clear();
        if quit {
            println!("desktop: bye");
            return 0;
        }

        let second = ticks() / 100;
        if desk.take_dirty() || second != last_second {
            last_second = second;
            desk.render(&mut back, second);
            present(&back, back_pixels, &info);
        }
    }
}

entry!(main);
