//! 2D drawing for NovaOS programs: a pixel surface with clipped drawing
//! operations, bitmap text, the mouse cursor, and a text grid for terminals.
//!
//! A pixel is a `u32` holding 0x00RRGGBB. Every drawing operation clips to the
//! surface, so coordinates that are negative, huge or nonsense draw less (or
//! nothing) and never touch memory outside the surface.

#![cfg_attr(not(test), no_std)]

pub mod textgrid;

use font8x8::legacy::BASIC_LEGACY;

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// Width and height of one glyph of the font, in pixels at scale 1.
pub const GLYPH: i32 = 8;

/// A rectangle of pixels in memory: `height` rows of `width` pixels, with row
/// `y` starting `stride` pixels after row `y - 1`.
pub struct Surface {
    ptr: *mut u32,
    width: usize,
    height: usize,
    stride: usize,
}

impl Surface {
    /// # Safety
    /// `ptr` must be valid for reads and writes of `height * stride` pixels for
    /// as long as the surface is used, with `stride >= width`.
    pub unsafe fn from_raw(ptr: *mut u32, width: usize, height: usize, stride: usize) -> Surface {
        debug_assert!(stride >= width);
        Surface { ptr, width, height, stride }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    fn row(&self, y: usize) -> &[u32] {
        debug_assert!(y < self.height);
        unsafe { core::slice::from_raw_parts(self.ptr.add(y * self.stride), self.width) }
    }

    fn row_mut(&mut self, y: usize) -> &mut [u32] {
        debug_assert!(y < self.height);
        unsafe { core::slice::from_raw_parts_mut(self.ptr.add(y * self.stride), self.width) }
    }

    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            None
        } else {
            Some(self.row(y as usize)[x as usize])
        }
    }

    pub fn put(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            self.row_mut(y as usize)[x as usize] = color;
        }
    }

    pub fn clear(&mut self, color: u32) {
        for y in 0..self.height {
            self.row_mut(y).fill(color);
        }
    }

    /// The part of the rectangle (x, y, w, h) that lies on the surface, as
    /// (x0, y0, x1, y1) with x1/y1 exclusive; None if nothing is left.
    fn clip(&self, x: i32, y: i32, w: i32, h: i32) -> Option<(usize, usize, usize, usize)> {
        if w <= 0 || h <= 0 {
            return None;
        }
        // i64 so that x + w cannot overflow whatever the caller passes.
        let x0 = (x as i64).max(0);
        let y0 = (y as i64).max(0);
        let x1 = (x as i64 + w as i64).min(self.width as i64);
        let y1 = (y as i64 + h as i64).min(self.height as i64);
        if x0 >= x1 || y0 >= y1 {
            None
        } else {
            Some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        if let Some((x0, y0, x1, y1)) = self.clip(x, y, w, h) {
            for row in y0..y1 {
                self.row_mut(row)[x0..x1].fill(color);
            }
        }
    }

    /// A rectangle's outline, `thickness` pixels wide, inside the rectangle.
    pub fn stroke_rect(&mut self, x: i32, y: i32, w: i32, h: i32, thickness: i32, color: u32) {
        self.fill_rect(x, y, w, thickness, color);
        self.fill_rect(x, y + h - thickness, w, thickness, color);
        self.fill_rect(x, y + thickness, thickness, h - 2 * thickness, color);
        self.fill_rect(x + w - thickness, y + thickness, thickness, h - 2 * thickness, color);
    }

    /// Copy all of `src` onto this surface with its top-left corner at (x, y).
    pub fn blit(&mut self, x: i32, y: i32, src: &Surface) {
        self.blit_part(x, y, src, 0, 0, src.width as i32, src.height as i32);
    }

    /// Copy the part of `src` starting at (sx, sy), `w` wide and `h` high.
    pub fn blit_part(&mut self, x: i32, y: i32, src: &Surface, sx: i32, sy: i32, w: i32, h: i32) {
        // Clip the source rectangle to `src`, shifting the destination with it.
        let (mut x, mut y, mut sx, mut sy, mut w, mut h) = (x as i64, y as i64, sx as i64, sy as i64, w as i64, h as i64);
        if sx < 0 {
            x -= sx;
            w += sx;
            sx = 0;
        }
        if sy < 0 {
            y -= sy;
            h += sy;
            sy = 0;
        }
        w = w.min(src.width as i64 - sx);
        h = h.min(src.height as i64 - sy);
        if w <= 0 || h <= 0 {
            return;
        }
        // Now clip the destination rectangle to this surface.
        let Some((x0, y0, x1, y1)) = self.clip(x.clamp(i32::MIN as i64, i32::MAX as i64) as i32, y.clamp(i32::MIN as i64, i32::MAX as i64) as i32, w as i32, h as i32) else {
            return;
        };
        let skip_x = x0 as i64 - x;
        let skip_y = y0 as i64 - y;
        let count = x1 - x0;
        for row in y0..y1 {
            let src_row = (sy + skip_y + (row - y0) as i64) as usize;
            let from = (sx + skip_x) as usize;
            let line = &src.row(src_row)[from..from + count];
            self.row_mut(row)[x0..x1].copy_from_slice(line);
        }
    }

    /// Draw one character of the 8x8 font, each font pixel `scale` pixels wide
    /// and high. Only the lit pixels are drawn: the background shows through.
    pub fn draw_char(&mut self, x: i32, y: i32, ch: u8, scale: i32, color: u32) {
        let glyph = BASIC_LEGACY[(ch & 0x7F) as usize];
        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..GLYPH {
                // Bit 0 of each row byte is the leftmost pixel.
                if bits >> gx & 1 != 0 {
                    self.fill_rect(x + gx * scale, y + gy as i32 * scale, scale, scale, color);
                }
            }
        }
    }

    /// Like `draw_char` but the whole cell is painted: lit pixels in `fg`,
    /// the rest in `bg`.
    pub fn draw_glyph_cell(&mut self, x: i32, y: i32, ch: u8, scale: i32, fg: u32, bg: u32) {
        self.fill_rect(x, y, GLYPH * scale, GLYPH * scale, bg);
        self.draw_char(x, y, ch, scale, fg);
    }

    /// Draw text (printable ASCII; anything else is shown as '?') starting at
    /// (x, y). Returns the x position after the last character.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &[u8], scale: i32, color: u32) -> i32 {
        let mut pen = x;
        for &byte in text {
            let ch = if (0x20..0x7F).contains(&byte) { byte } else { b'?' };
            self.draw_char(pen, y, ch, scale, color);
            pen = pen.saturating_add(GLYPH * scale);
        }
        pen
    }

    /// The mouse pointer, with its tip at (x, y).
    pub fn draw_cursor(&mut self, x: i32, y: i32) {
        for (row, line) in CURSOR.iter().enumerate() {
            for (col, shade) in line.bytes().enumerate() {
                match shade {
                    b'X' => self.put(x + col as i32, y + row as i32, CURSOR_OUTLINE),
                    b'.' => self.put(x + col as i32, y + row as i32, CURSOR_FILL),
                    _ => {}
                }
            }
        }
    }
}

/// Width in pixels of `len` characters at `scale`.
pub const fn text_width(len: usize, scale: i32) -> i32 {
    len as i32 * GLYPH * scale
}

pub const CURSOR_OUTLINE: u32 = rgb(0, 0, 0);
pub const CURSOR_FILL: u32 = rgb(255, 255, 255);
pub const CURSOR_W: usize = 12;
pub const CURSOR_H: usize = 19;

/// 'X' is the black outline, '.' the white inside; spaces are transparent.
pub const CURSOR: [&str; CURSOR_H] = [
    "X           ",
    "XX          ",
    "X.X         ",
    "X..X        ",
    "X...X       ",
    "X....X      ",
    "X.....X     ",
    "X......X    ",
    "X.......X   ",
    "X........X  ",
    "X.........X ",
    "X......XXXXX",
    "X...X..X    ",
    "X..X X..X   ",
    "X.X  X..X   ",
    "XX    X..X  ",
    "X     X..X  ",
    "       X..X ",
    "       XXX  ",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A surface over a Vec, for tests.
    struct Canvas {
        data: Vec<u32>,
        w: usize,
        h: usize,
    }

    impl Canvas {
        fn new(w: usize, h: usize, fill: u32) -> Canvas {
            Canvas { data: vec![fill; w * h], w, h }
        }
        fn surface(&mut self) -> Surface {
            unsafe { Surface::from_raw(self.data.as_mut_ptr(), self.w, self.h, self.w) }
        }
        fn at(&self, x: usize, y: usize) -> u32 {
            self.data[y * self.w + x]
        }
    }

    const BG: u32 = 0x11_22_33;
    const FG: u32 = 0xAA_BB_CC;

    #[test]
    fn rgb_packs_channels() {
        assert_eq!(rgb(0x12, 0x34, 0x56), 0x0012_3456);
    }

    #[test]
    fn fill_rect_inside() {
        let mut c = Canvas::new(10, 8, BG);
        c.surface().fill_rect(2, 3, 4, 2, FG);
        for y in 0..8 {
            for x in 0..10 {
                let inside = (2..6).contains(&x) && (3..5).contains(&y);
                assert_eq!(c.at(x, y), if inside { FG } else { BG }, "({x},{y})");
            }
        }
    }

    #[test]
    fn fill_rect_clips_every_side_and_survives_nonsense() {
        let mut c = Canvas::new(10, 8, BG);
        let mut s = c.surface();
        s.fill_rect(-3, -3, 5, 5, FG); // overlaps the top-left corner
        s.fill_rect(8, 6, 100, 100, FG); // overlaps the bottom-right corner
        s.fill_rect(i32::MAX - 1, 0, 100, 3, FG); // x + w would overflow i32
        s.fill_rect(0, i32::MIN, 5, i32::MAX, FG); // ends one row above the surface
        s.fill_rect(5, 5, -4, 3, FG); // negative size draws nothing
        s.fill_rect(5, 5, 3, 0, FG);
        s.fill_rect(-1000, -1000, 10, 10, FG); // entirely off screen
        s.fill_rect(1000, 1000, 10, 10, FG);
        // top-left 2x2 got painted by the first call, the column rect painted x 0..5
        assert_eq!(c.at(0, 0), FG);
        assert_eq!(c.at(1, 1), FG);
        assert_eq!(c.at(9, 7), FG);
        assert_eq!(c.at(8, 6), FG);
        assert_eq!(c.at(7, 7), BG);
        // nothing outside the buffer was touched (Vec would have panicked or corrupted)
        assert_eq!(c.data.len(), 80);
    }

    #[test]
    fn put_and_get_check_bounds() {
        let mut c = Canvas::new(4, 4, BG);
        let mut s = c.surface();
        s.put(-1, 0, FG);
        s.put(0, -1, FG);
        s.put(4, 0, FG);
        s.put(0, 4, FG);
        s.put(3, 3, FG);
        assert_eq!(s.get(3, 3), Some(FG));
        assert_eq!(s.get(4, 3), None);
        assert_eq!(s.get(-1, -1), None);
        assert_eq!(c.data.iter().filter(|&&p| p == FG).count(), 1);
    }

    #[test]
    fn stride_larger_than_width_is_respected() {
        // A 4-wide surface inside rows that are 7 pixels apart.
        let mut backing = vec![BG; 7 * 3];
        let mut s = unsafe { Surface::from_raw(backing.as_mut_ptr(), 4, 3, 7) };
        s.clear(FG);
        s.fill_rect(0, 0, 100, 100, FG);
        for y in 0..3 {
            for x in 0..7 {
                assert_eq!(backing[y * 7 + x], if x < 4 { FG } else { BG }, "({x},{y})");
            }
        }
    }

    #[test]
    fn stroke_rect_draws_only_the_outline() {
        let mut c = Canvas::new(10, 10, BG);
        c.surface().stroke_rect(1, 1, 8, 6, 2, FG);
        for y in 0..10usize {
            for x in 0..10usize {
                let inside_outer = (1..9).contains(&x) && (1..7).contains(&y);
                let inside_inner = (3..7).contains(&x) && (3..5).contains(&y);
                let expected = if inside_outer && !inside_inner { FG } else { BG };
                assert_eq!(c.at(x, y), expected, "({x},{y})");
            }
        }
    }

    #[test]
    fn blit_copies_and_clips() {
        let mut src = Canvas::new(4, 3, 0);
        for (i, p) in src.data.iter_mut().enumerate() {
            *p = 100 + i as u32;
        }
        let sv = src.surface();

        let mut dst = Canvas::new(10, 10, BG);
        dst.surface().blit(2, 3, &sv);
        for y in 0..3 {
            for x in 0..4 {
                assert_eq!(dst.at(2 + x, 3 + y), 100 + (y * 4 + x) as u32);
            }
        }
        assert_eq!(dst.at(1, 3), BG);
        assert_eq!(dst.at(6, 3), BG);

        // Hanging off the top-left: only the visible part arrives, from the right place.
        let mut dst = Canvas::new(10, 10, BG);
        dst.surface().blit(-2, -1, &sv);
        assert_eq!(dst.at(0, 0), 100 + (1 * 4 + 2) as u32);
        assert_eq!(dst.at(1, 0), 100 + (1 * 4 + 3) as u32);
        assert_eq!(dst.at(2, 0), BG);
        assert_eq!(dst.at(0, 1), 100 + (2 * 4 + 2) as u32);
        assert_eq!(dst.at(0, 2), BG);

        // Hanging off the bottom-right.
        let mut dst = Canvas::new(10, 10, BG);
        dst.surface().blit(8, 8, &sv);
        assert_eq!(dst.at(8, 8), 100);
        assert_eq!(dst.at(9, 8), 101);
        assert_eq!(dst.at(8, 9), 104);
        assert_eq!(dst.at(9, 9), 105);

        // Completely outside, and absurd positions.
        let mut dst = Canvas::new(10, 10, BG);
        let mut d = dst.surface();
        d.blit(10, 0, &sv);
        d.blit(0, 10, &sv);
        d.blit(-4, 0, &sv);
        d.blit(0, -3, &sv);
        d.blit(i32::MAX, i32::MAX, &sv);
        d.blit(i32::MIN, i32::MIN, &sv);
        assert!(dst.data.iter().all(|&p| p == BG));
    }

    #[test]
    fn blit_part_takes_the_requested_piece() {
        let mut src = Canvas::new(6, 6, 0);
        for (i, p) in src.data.iter_mut().enumerate() {
            *p = i as u32;
        }
        let sv = src.surface();
        let mut dst = Canvas::new(8, 8, BG);
        dst.surface().blit_part(1, 1, &sv, 2, 3, 3, 2);
        for y in 0..2usize {
            for x in 0..3usize {
                assert_eq!(dst.at(1 + x, 1 + y), ((3 + y) * 6 + 2 + x) as u32);
            }
        }
        assert_eq!(dst.at(4, 1), BG);
        assert_eq!(dst.at(1, 3), BG);
        // A request that sticks out of the source is trimmed, one that starts
        // before it is shifted.
        let mut dst = Canvas::new(8, 8, BG);
        dst.surface().blit_part(0, 0, &sv, 4, 4, 10, 10);
        assert_eq!(dst.at(0, 0), (4 * 6 + 4) as u32);
        assert_eq!(dst.at(1, 1), (5 * 6 + 5) as u32);
        assert_eq!(dst.at(2, 0), BG);
        let mut dst = Canvas::new(8, 8, BG);
        dst.surface().blit_part(3, 3, &sv, -2, -2, 4, 4);
        assert_eq!(dst.at(5, 5), 0);
        assert_eq!(dst.at(6, 6), 7);
        assert_eq!(dst.at(4, 4), BG);
    }

    #[test]
    fn a_character_matches_the_font_at_every_scale() {
        for scale in 1..=3 {
            let mut c = Canvas::new(40, 40, BG);
            c.surface().draw_char(2, 3, b'H', scale, FG);
            let glyph = BASIC_LEGACY[b'H' as usize];
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let lit = glyph[y as usize] >> x & 1 != 0;
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let (px, py) = (2 + x * scale + dx, 3 + y * scale + dy);
                            assert_eq!(c.at(px as usize, py as usize), if lit { FG } else { BG });
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_letter_h_looks_like_an_h() {
        // Independent of the table: an H has two vertical bars joined in the middle.
        let mut c = Canvas::new(8, 8, BG);
        c.surface().draw_char(0, 0, b'H', 1, FG);
        let art: Vec<String> = (0..8)
            .map(|y| (0..8).map(|x| if c.at(x, y) == FG { '#' } else { '.' }).collect())
            .collect();
        assert_eq!(art[0], "##..##..");
        assert_eq!(art[3], "######..");
        assert_eq!(art[6], "##..##..");
        assert_eq!(art[7], "........");
    }

    #[test]
    fn glyph_cell_paints_the_background_too() {
        let mut c = Canvas::new(20, 20, 0x555555);
        c.surface().draw_glyph_cell(2, 2, b'.', 2, FG, BG);
        assert_eq!(c.at(2, 2), BG);
        assert_eq!(c.at(17, 17), BG);
        assert_eq!(c.at(1, 1), 0x555555);
        assert_eq!(c.at(18, 18), 0x555555);
    }

    #[test]
    fn text_advances_by_the_glyph_width_and_replaces_odd_bytes() {
        let mut c = Canvas::new(100, 20, BG);
        let end = c.surface().draw_text(4, 2, b"ab", 2, FG);
        assert_eq!(end, 4 + 2 * 16);
        let mut d = Canvas::new(100, 20, BG);
        d.surface().draw_text(4, 2, &[b'a', 0x01, 0xFF], 1, FG);
        let mut e = Canvas::new(100, 20, BG);
        e.surface().draw_text(4, 2, b"a??", 1, FG);
        assert_eq!(d.data, e.data);
        assert_eq!(text_width(5, 2), 80);
    }

    #[test]
    fn text_off_the_edge_is_clipped() {
        let mut c = Canvas::new(30, 12, BG);
        c.surface().draw_text(-10, -4, b"WIDE TEXT", 2, FG);
        c.surface().draw_text(20, 8, b"WIDE TEXT", 2, FG);
        c.surface().draw_text(i32::MAX - 5, 0, b"abc", 2, FG);
        assert_eq!(c.data.len(), 360);
    }

    #[test]
    fn the_cursor_is_well_formed() {
        for line in CURSOR.iter() {
            assert_eq!(line.len(), CURSOR_W);
        }
        let mut c = Canvas::new(30, 30, BG);
        c.surface().draw_cursor(5, 6);
        assert_eq!(c.at(5, 6), CURSOR_OUTLINE, "the tip is at the given point");
        assert_eq!(c.at(5, 7), CURSOR_OUTLINE);
        assert_eq!(c.at(6, 7), CURSOR_OUTLINE);
        assert_eq!(c.at(6, 8), CURSOR_FILL);
        assert_eq!(c.at(4, 6), BG);
        // Clipped at the screen edge without trouble.
        let mut c = Canvas::new(8, 8, BG);
        c.surface().draw_cursor(6, 6);
        c.surface().draw_cursor(-5, -5);
        c.surface().draw_cursor(i32::MAX, i32::MAX);
    }
}
