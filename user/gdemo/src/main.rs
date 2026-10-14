//! gdemo [about]: a small graphical program.
//!
//! Without an argument: a colour gradient with a square that jumps to where you
//! press the left button, follows the pointer while it is held (turning red), and
//! stays where you let go. With `about`: a
//! plain information window. Both close when the window's close button is used.

#![no_std]
#![no_main]

use gfx::{rgb, Surface};
use nova::*;

const DEMO_W: u32 = 320;
const DEMO_H: u32 = 200;
const SQUARE: i32 = 24;

/// The gradient behind the demo.
pub fn gradient(x: u32, y: u32) -> u32 {
    rgb((x * 255 / (DEMO_W - 1)) as u8, (y * 255 / (DEMO_H - 1)) as u8, 128)
}

fn draw_demo(surface: &mut Surface, pointer: (i32, i32), clicked: bool) {
    for y in 0..DEMO_H {
        for x in 0..DEMO_W {
            surface.put(x as i32, y as i32, gradient(x, y));
        }
    }
    surface.draw_text(16, 16, b"NovaOS", 3, rgb(255, 255, 255));
    surface.draw_text(16, 56, b"Move the mouse here", 1, rgb(255, 255, 255));
    let color = if clicked { rgb(255, 80, 80) } else { rgb(255, 230, 60) };
    surface.fill_rect(pointer.0 - SQUARE / 2, pointer.1 - SQUARE / 2, SQUARE, SQUARE, color);
}

const ABOUT_W: u32 = 360;
const ABOUT_H: u32 = 150;
const ABOUT_BG: u32 = rgb(24, 28, 40);

fn draw_about(surface: &mut Surface) {
    surface.clear(ABOUT_BG);
    surface.draw_text(16, 14, b"NovaOS", 3, rgb(120, 170, 255));
    surface.draw_text(16, 56, b"An operating system", 2, rgb(230, 230, 230));
    surface.draw_text(16, 80, b"written from scratch", 2, rgb(230, 230, 230));
    surface.draw_text(16, 104, b"in Rust.", 2, rgb(230, 230, 230));
}

fn main(args: &Args) -> i32 {
    let about = args.get(1) == "about";
    let (width, height, title): (u32, u32, &[u8]) = if about {
        (ABOUT_W, ABOUT_H, b"About NovaOS")
    } else {
        (DEMO_W, DEMO_H, b"Graphics demo")
    };
    let (id, address) = match win_create(width, height, title) {
        Ok(created) => created,
        Err(e) => {
            eprintln!("gdemo: cannot create a window: {}", strerror(e));
            return 1;
        }
    };
    let mut surface = unsafe { Surface::from_raw(address as *mut u32, width as usize, height as usize, width as usize) };

    // Where the square is (the middle until the pointer shows up), and its colour.
    let mut pointer = (DEMO_W as i32 / 2, DEMO_H as i32 / 2);
    let mut clicked = false;
    if about {
        draw_about(&mut surface);
    } else {
        draw_demo(&mut surface, pointer, clicked);
    }
    win_flush(id);

    let mut event = Event::default();
    loop {
        if win_event(id, &mut event, FOREVER) != 1 {
            continue;
        }
        let mut redraw = false;
        match event.kind {
            EV_WIN_CLOSE => break,
            EV_WIN_MOVE if !about && event.c & BUTTON_LEFT != 0 => {
                pointer = (event.a, event.b);
                redraw = true;
            }
            EV_WIN_BUTTON if !about => {
                // Left button down turns the square red; up turns it back.
                let down = event.c & BUTTON_LEFT != 0;
                if down != clicked {
                    clicked = down;
                    redraw = true;
                }
                pointer = (event.a, event.b);
            }
            _ => {}
        }
        if redraw {
            draw_demo(&mut surface, pointer, clicked);
            win_flush(id);
        }
    }
    win_destroy(id);
    0
}

entry!(main);
