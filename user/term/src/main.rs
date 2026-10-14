//! term: a terminal in a window. It starts the shell with its input and output
//! connected to pipes, draws what the shell prints into the window, and sends
//! the keys typed in the window to the shell.
//!
//! The window is 50 x 30 characters (each 16x16 pixels), drawn with the same
//! text rules as the kernel's console. Closing the window (or typing `exit`)
//! ends the shell and the terminal.

#![no_std]
#![no_main]

use gfx::textgrid::TextGrid;
use gfx::Surface;
use nova::*;

const COLS: usize = 50;
const ROWS: usize = 30;
const SCALE: i32 = 2;
const WIDTH: u32 = (COLS as u32) * 16;
const HEIGHT: u32 = (ROWS as u32) * 16;

fn main(_args: &Args) -> i32 {
    let (id, address) = match win_create(WIDTH, HEIGHT, b"Terminal") {
        Ok(created) => created,
        Err(e) => {
            eprintln!("term: cannot create a window: {}", strerror(e));
            return 1;
        }
    };
    let mut surface = unsafe { Surface::from_raw(address as *mut u32, WIDTH as usize, HEIGHT as usize, WIDTH as usize) };
    let mut grid = TextGrid::<COLS, ROWS>::new();

    // Two pipes: keys go in to the shell, its output comes back.
    let (shell_in_read, shell_in_write) = match pipe() {
        Ok(ends) => ends,
        Err(e) => {
            eprintln!("term: no pipe: {}", strerror(e));
            return 1;
        }
    };
    let (shell_out_read, shell_out_write) = match pipe() {
        Ok(ends) => ends,
        Err(e) => {
            eprintln!("term: no pipe: {}", strerror(e));
            return 1;
        }
    };
    let stdio = [shell_in_read, shell_out_write, shell_out_write];
    let shell = spawn(b"/bin/sh\0", b"sh\0", 1, Some(&stdio));
    // The shell has its own copies now. Keeping ours would stop us from ever
    // seeing end-of-file when the shell exits.
    close(shell_in_read);
    close(shell_out_write);
    if shell < 0 {
        eprintln!("term: cannot start the shell: {}", strerror(shell));
        return 1;
    }

    grid.render(&mut surface, 0, 0, SCALE);
    win_flush(id);

    let mut buffer = [0u8; 256];
    loop {
        // Whatever the shell has printed.
        let mut changed = false;
        let mut ended = false;
        while fd_ready(shell_out_read) == 1 {
            let n = read(shell_out_read, &mut buffer);
            if n <= 0 {
                ended = true;
                break;
            }
            for &byte in &buffer[..n as usize] {
                grid.feed(byte);
            }
            changed = true;
        }
        if changed {
            grid.render(&mut surface, 0, 0, SCALE);
            win_flush(id);
        }
        if ended {
            break;
        }

        // Whatever the user did in the window (waiting a moment, then looking at the shell again).
        let mut event = Event::default();
        if win_event(id, &mut event, 1) == 1 {
            match event.kind {
                EV_WIN_KEY => {
                    // An escape would make the shell wait for the rest of a sequence
                    // that never comes, so it is not passed on.
                    if event.a != 27 {
                        write(shell_in_write, &[event.a as u8]);
                    }
                }
                EV_WIN_CLOSE => break,
                _ => {}
            }
        }
    }

    // Closing the shell's input makes it read end-of-file and exit.
    close(shell_in_write);
    close(shell_out_read);
    waitpid(shell as u64);
    win_destroy(id);
    0
}

entry!(main);
