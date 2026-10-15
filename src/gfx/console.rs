//! A text console on the framebuffer. It shows the same bytes the serial port
//! gets: the kernel's messages, and everything programs write to stdout/stderr.
//!
//! Each character is an 8x8 font glyph drawn at 2x, so a cell is 16x16 pixels.
//! The characters are also kept in a grid in memory, so the cursor can be moved
//! without damaging the text under it.
//!
//! Layout (the screen test re-implements it independently, so keep them in step):
//!   cell (col, row) has its top-left corner at (PAD + col*CELL, PAD + row*CELL)
//!   cols = (width - 2*PAD) / CELL, rows = (height - 2*PAD) / CELL
//!   the margin and any leftover pixels are the background colour.

use super::fb::{Color, Framebuffer};
use crate::sync::SpinLock;
use core::ptr::addr_of_mut;
use font8x8::legacy::BASIC_LEGACY;

const SCALE: usize = 2;
const GLYPH: usize = 8;
const CELL: usize = GLYPH * SCALE;
const PAD: usize = 8;
const TAB: usize = 8;

pub const BACKGROUND: Color = Color::rgb(16, 24, 40);
pub const FOREGROUND: Color = Color::rgb(220, 220, 220);

/// Room for the character grid. It is a static (zero-filled, so it costs no
/// space in the kernel file) because the boot stack is far too small to hold
/// it, and no heap is needed, so output appears from the first line of boot.
/// A 4K screen at this cell size needs about 32,000.
const MAX_CELLS: usize = 65536;

/// 0 means an empty cell (shown as a space).
static mut CELL_STORE: [u8; MAX_CELLS] = [0; MAX_CELLS];

struct Console {
    fb: Framebuffer,
    /// False while a program owns the screen: text still updates the grid (so
    /// nothing is lost) but nothing is drawn.
    visible: bool,
    cols: usize,
    rows: usize,
    /// May equal `cols` right after writing the last column; the next
    /// printable character then wraps to a new line first.
    col: usize,
    row: usize,
    cells: &'static mut [u8],
}

static CONSOLE: SpinLock<Option<Console>> = SpinLock::new(None);

/// Set up the console on the bootloader's framebuffer. Returns (width, height,
/// columns, rows) on success; None if there is no usable screen.
pub fn init() -> Option<(usize, usize, usize, usize)> {
    let fb = Framebuffer::from_bootloader()?;
    if fb.width < 2 * PAD + CELL || fb.height < 2 * PAD + CELL {
        return None;
    }
    let cols = (fb.width - 2 * PAD) / CELL;
    let rows = ((fb.height - 2 * PAD) / CELL).min(MAX_CELLS / cols);
    if rows == 0 {
        return None;
    }
    // Only this function ever creates the slice, and it runs once.
    let cells = unsafe { core::slice::from_raw_parts_mut(addr_of_mut!(CELL_STORE) as *mut u8, MAX_CELLS) };
    cells.fill(0);
    let mut console = Console { fb, visible: true, cols, rows, col: 0, row: 0, cells };
    console.fb.clear(BACKGROUND);
    let report = (console.fb.width, console.fb.height, cols, rows);
    console.draw_cursor(true);
    *CONSOLE.lock() = Some(console);
    Some(report)
}

/// Feed one byte of output to the screen. Does nothing if there is no screen.
pub fn put_byte(b: u8) {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.put(b);
    }
}

/// Repaint the whole screen from the character grid (as after something else
/// has used the screen, or the screen has been resized).
pub fn redraw_all() {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.redraw();
    }
}

/// Whether there is a screen at all.
pub fn is_active() -> bool {
    CONSOLE.lock().is_some()
}

/// The screen's description, for handing it to a program that will draw on it.
pub fn framebuffer_info() -> Option<super::fb::FramebufferInfo> {
    let guard = CONSOLE.lock();
    guard.as_ref()?.fb.describe(crate::mem::hhdm_offset())
}

/// Stop drawing (a program owns the screen). Text keeps updating the grid.
pub fn suspend() {
    if let Some(console) = CONSOLE.lock().as_mut() {
        console.visible = false;
    }
}

/// Take the screen back and repaint everything, including what was printed
/// while it was away.
pub fn resume() {
    redraw_all();
}

/// For crash paths: the report must print even if the crash was inside the console.
pub fn force_unlock() {
    unsafe { CONSOLE.force_unlock() };
}

impl Console {
    fn cell_origin(col: usize, row: usize) -> (usize, usize) {
        (PAD + col * CELL, PAD + row * CELL)
    }

    /// Draw character `ch` (0 = blank) in cell (col, row), replacing what was there.
    fn draw_cell(&mut self, col: usize, row: usize, ch: u8) {
        if !self.visible {
            return;
        }
        let ch = if ch == 0 { b' ' } else { ch };
        let glyph = BASIC_LEGACY[(ch & 0x7F) as usize];
        let (x0, y0) = Self::cell_origin(col, row);
        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..GLYPH {
                // Bit 0 of each row byte is the leftmost pixel.
                let on = bits >> gx & 1 != 0;
                let color = if on { FOREGROUND } else { BACKGROUND };
                self.fb
                    .fill_rect(x0 + gx * SCALE, y0 + gy * SCALE, SCALE, SCALE, color);
            }
        }
    }

    /// Clear the screen and draw every character of the grid, then the cursor.
    fn redraw(&mut self) {
        self.visible = true;
        self.fb.clear(BACKGROUND);
        for row in 0..self.rows {
            for col in 0..self.cols {
                let ch = self.cells[row * self.cols + col];
                if ch != 0 && ch != b' ' {
                    self.draw_cell(col, row, ch);
                }
            }
        }
        self.draw_cursor(true);
    }

    /// The cell the cursor is drawn in (the last column while a wrap is pending).
    fn cursor_cell(&self) -> (usize, usize) {
        (self.col.min(self.cols - 1), self.row)
    }

    /// Show or hide the cursor: a bar along the bottom of its cell.
    fn draw_cursor(&mut self, visible: bool) {
        if !self.visible {
            return;
        }
        let (col, row) = self.cursor_cell();
        let (x0, y0) = Self::cell_origin(col, row);
        if visible {
            self.fb.fill_rect(x0, y0 + CELL - SCALE, CELL, SCALE, FOREGROUND);
        } else {
            let ch = self.cells[row * self.cols + col];
            self.draw_cell(col, row, ch);
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
            return;
        }
        // Scroll the grid and the picture up by one text row.
        let (cols, rows) = (self.cols, self.rows);
        self.cells.copy_within(cols..rows * cols, 0);
        self.cells[(rows - 1) * cols..rows * cols].fill(0);
        if !self.visible {
            return;
        }
        self.fb.scroll_up(CELL, BACKGROUND);
        // Moving the whole picture dragged some text pixels into the top margin.
        let width = self.fb.width;
        self.fb.fill_rect(0, 0, width, PAD, BACKGROUND);
    }

    fn put(&mut self, b: u8) {
        self.draw_cursor(false);
        match b {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            // Form feed: clear the screen and start again at the top left (`clear` sends it).
            0x0C => {
                self.cells.fill(0);
                self.row = 0;
                self.col = 0;
                if self.visible {
                    let (w, h) = (self.fb.width, self.fb.height);
                    self.fb.fill_rect(0, 0, w, h, BACKGROUND);
                }
            }
            b'\t' => {
                let next = (self.col / TAB + 1) * TAB;
                while self.col < next.min(self.cols) {
                    self.put_printable(b' ');
                }
            }
            0x20..=0x7E => self.put_printable(b),
            _ => {}
        }
        self.draw_cursor(true);
    }

    fn put_printable(&mut self, b: u8) {
        if self.col >= self.cols {
            self.newline();
        }
        self.cells[self.row * self.cols + self.col] = b;
        self.draw_cell(self.col, self.row, b);
        self.col += 1;
    }
}
