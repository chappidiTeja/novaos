//! The framebuffer: a block of memory where each 32-bit word is one pixel.
//! Everything drawn is clipped to the screen, so a bad coordinate draws
//! nothing (or less) instead of writing outside the buffer.

use crate::limine::{self, LimineFramebuffer};
use core::ptr::{copy, write_volatile};

/// A colour as red, green, blue (0-255 each).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b }
    }
}

/// What a program needs to know to draw on the screen directly.
#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
    pub phys: u64,
    pub width: u64,
    pub height: u64,
    /// Bytes per row.
    pub pitch: u64,
    pub red_shift: u64,
    pub green_shift: u64,
    pub blue_shift: u64,
    /// Total size: pitch * height.
    pub bytes: u64,
}

/// Largest dimensions accepted (guards the arithmetic below against garbage).
const MAX_DIMENSION: u64 = 16384;

pub struct Framebuffer {
    base: *mut u8,
    pub width: usize,
    pub height: usize,
    /// Bytes per row; at least `width * 4`.
    pitch: usize,
    red_shift: u32,
    green_shift: u32,
    blue_shift: u32,
}

// The pointer is to device memory that nothing else owns.
unsafe impl Send for Framebuffer {}

impl Framebuffer {
    /// Describe the bootloader's framebuffer. None if there is none, or if it
    /// is not a plain 32-bit RGB buffer (the only kind this driver draws to).
    pub fn from_bootloader() -> Option<Framebuffer> {
        Self::from_description(limine::first_framebuffer()?)
    }

    fn from_description(d: &LimineFramebuffer) -> Option<Framebuffer> {
        if d.address.is_null()
            || d.bpp != 32
            || d.memory_model != limine::FRAMEBUFFER_RGB
            || d.red_mask_size != 8
            || d.green_mask_size != 8
            || d.blue_mask_size != 8
            || d.width == 0
            || d.height == 0
            || d.width > MAX_DIMENSION
            || d.height > MAX_DIMENSION
            || d.pitch < d.width * 4
            || d.pitch > MAX_DIMENSION * 4
        {
            return None;
        }
        // Each channel is 8 bits wide inside a 32-bit pixel: the shift must leave room.
        if d.red_mask_shift > 24 || d.green_mask_shift > 24 || d.blue_mask_shift > 24 {
            return None;
        }
        Some(Framebuffer {
            base: d.address,
            width: d.width as usize,
            height: d.height as usize,
            pitch: d.pitch as usize,
            red_shift: d.red_mask_shift as u32,
            green_shift: d.green_mask_shift as u32,
            blue_shift: d.blue_mask_shift as u32,
        })
    }

    /// Everything a program needs to draw on this screen itself.
    pub fn describe(&self, hhdm_offset: u64) -> Option<FramebufferInfo> {
        let virt = self.base as u64;
        // The bootloader gives a direct-map address; turn it back into the
        // physical address the pixels really live at.
        let phys = virt.checked_sub(hhdm_offset)?;
        Some(FramebufferInfo {
            phys,
            width: self.width as u64,
            height: self.height as u64,
            pitch: self.pitch as u64,
            red_shift: self.red_shift as u64,
            green_shift: self.green_shift as u64,
            blue_shift: self.blue_shift as u64,
            bytes: (self.pitch * self.height) as u64,
        })
    }

    /// The 32-bit word that shows as `c` on this screen.
    pub fn pack(&self, c: Color) -> u32 {
        (c.r as u32) << self.red_shift | (c.g as u32) << self.green_shift | (c.b as u32) << self.blue_shift
    }

    fn row_ptr(&self, y: usize) -> *mut u32 {
        // y < height is checked by every caller.
        unsafe { self.base.add(y * self.pitch) as *mut u32 }
    }

    pub fn put_pixel(&mut self, x: usize, y: usize, c: Color) {
        if x < self.width && y < self.height {
            unsafe { write_volatile(self.row_ptr(y).add(x), self.pack(c)) };
        }
    }

    /// Fill the rectangle with its top-left corner at (x, y); parts outside
    /// the screen are skipped.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Color) {
        if x >= self.width || y >= self.height {
            return;
        }
        let w = w.min(self.width - x);
        let h = h.min(self.height - y);
        let word = self.pack(c);
        for row in y..y + h {
            let line = self.row_ptr(row);
            for col in x..x + w {
                unsafe { write_volatile(line.add(col), word) };
            }
        }
    }

    pub fn clear(&mut self, c: Color) {
        self.fill_rect(0, 0, self.width, self.height, c);
    }

    /// Move the picture up by `rows` pixel rows and fill the freed bottom with `fill`.
    pub fn scroll_up(&mut self, rows: usize, fill: Color) {
        if rows == 0 {
            return;
        }
        if rows >= self.height {
            self.clear(fill);
            return;
        }
        let keep = self.height - rows;
        unsafe {
            // Overlapping ranges: `copy` is memmove.
            copy(self.base.add(rows * self.pitch), self.base, keep * self.pitch);
        }
        self.fill_rect(0, keep, self.width, rows, fill);
    }
}
