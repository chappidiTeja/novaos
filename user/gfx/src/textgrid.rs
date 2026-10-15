//! A grid of characters with terminal behaviour, for the terminal window.
//!
//! It follows the same rules as the kernel's on-screen console, so the shell
//! looks the same in a window as it does on the bare screen:
//!   '\n'  start of the next line (the grid scrolls when it is already on the last)
//!   '\r'  back to the start of the line
//!   0x08  one column left (stops at the left edge)
//!   0x0C  clear the grid and go to the top left corner
//!   '\t'  spaces up to the next multiple of 8 columns
//!   printable ASCII is stored at the cursor, which moves right and wraps to a
//!   new line when it passes the last column; every other byte is ignored.

use crate::{rgb, Surface, GLYPH};

pub const BACKGROUND: u32 = rgb(16, 24, 40);
pub const FOREGROUND: u32 = rgb(220, 220, 220);

pub struct TextGrid<const COLS: usize, const ROWS: usize> {
    /// 0 means an empty cell (shown as a space).
    cells: [[u8; COLS]; ROWS],
    /// May equal COLS right after writing the last column: the next printable
    /// character then wraps first.
    col: usize,
    row: usize,
}

impl<const COLS: usize, const ROWS: usize> TextGrid<COLS, ROWS> {
    pub const fn new() -> Self {
        TextGrid { cells: [[0; COLS]; ROWS], col: 0, row: 0 }
    }

    pub fn cell(&self, col: usize, row: usize) -> u8 {
        self.cells[row][col]
    }

    /// Where the cursor is drawn (the last column while a wrap is pending).
    pub fn cursor(&self) -> (usize, usize) {
        (self.col.min(COLS - 1), self.row)
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < ROWS {
            self.row += 1;
        } else {
            self.cells.copy_within(1..ROWS, 0);
            self.cells[ROWS - 1] = [0; COLS];
        }
    }

    fn printable(&mut self, byte: u8) {
        if self.col >= COLS {
            self.newline();
        }
        self.cells[self.row][self.col] = byte;
        self.col += 1;
    }

    pub fn feed(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            // Form feed: clear everything and go back to the top left.
            0x0C => {
                self.cells = [[0; COLS]; ROWS];
                self.row = 0;
                self.col = 0;
            }
            b'\t' => {
                let next = (self.col / 8 + 1) * 8;
                while self.col < next.min(COLS) {
                    self.printable(b' ');
                }
            }
            0x20..=0x7E => self.printable(byte),
            _ => {}
        }
    }

    pub fn feed_all(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed(b);
        }
    }

    /// Draw the whole grid with its top-left corner at (x0, y0): cells of
    /// `GLYPH * scale` pixels, then the cursor as a bar along the cell's bottom.
    pub fn render(&self, surface: &mut Surface, x0: i32, y0: i32, scale: i32) {
        let cell = GLYPH * scale;
        for row in 0..ROWS {
            for col in 0..COLS {
                let ch = match self.cells[row][col] {
                    0 => b' ',
                    c => c,
                };
                surface.draw_glyph_cell(x0 + col as i32 * cell, y0 + row as i32 * cell, ch, scale, FOREGROUND, BACKGROUND);
            }
        }
        let (cc, cr) = self.cursor();
        surface.fill_rect(
            x0 + cc as i32 * cell,
            y0 + cr as i32 * cell + cell - scale,
            cell,
            scale,
            FOREGROUND,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text<const C: usize, const R: usize>(g: &TextGrid<C, R>) -> Vec<String> {
        (0..R)
            .map(|r| {
                (0..C)
                    .map(|c| match g.cell(c, r) {
                        0 => ' ',
                        b => b as char,
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn text_lands_where_the_cursor_is() {
        let mut g = TextGrid::<10, 4>::new();
        g.feed_all(b"hi\r\nyou");
        assert_eq!(text(&g), ["hi", "you", "", ""]);
        assert_eq!(g.cursor(), (3, 1));
    }

    #[test]
    fn long_lines_wrap() {
        let mut g = TextGrid::<5, 3>::new();
        g.feed_all(b"abcdefgh");
        assert_eq!(text(&g), ["abcde", "fgh", ""]);
        // Exactly full: the wrap waits for the next character.
        let mut g = TextGrid::<5, 3>::new();
        g.feed_all(b"abcde");
        assert_eq!(g.cursor(), (4, 0));
        assert_eq!(text(&g), ["abcde", "", ""]);
        g.feed(b'f');
        assert_eq!(text(&g), ["abcde", "f", ""]);
    }

    #[test]
    fn form_feed_clears_the_grid_and_homes_the_cursor() {
        let mut g = TextGrid::<6, 3>::new();
        g.feed_all(b"one\ntwo\nthree");
        g.feed(0x0C);
        assert_eq!(text(&g), ["", "", ""]);
        assert_eq!(g.cursor(), (0, 0));
        g.feed_all(b"new");
        assert_eq!(text(&g), ["new", "", ""]);
    }

    #[test]
    fn a_full_grid_scrolls() {
        let mut g = TextGrid::<4, 3>::new();
        g.feed_all(b"1\n2\n3\n4\n5");
        assert_eq!(text(&g), ["3", "4", "5"]);
        assert_eq!(g.cursor(), (1, 2));
    }

    #[test]
    fn backspace_returns_to_the_left_and_stops_at_the_edge() {
        let mut g = TextGrid::<8, 2>::new();
        g.feed_all(b"abc");
        g.feed(0x08);
        g.feed(b'X');
        assert_eq!(text(&g), ["abX", ""]);
        g.feed_all(&[0x08; 10]);
        assert_eq!(g.cursor(), (0, 0));
        // The shell's erase sequence: back, space, back.
        let mut g = TextGrid::<8, 2>::new();
        g.feed_all(b"ab");
        g.feed_all(b"\x08 \x08");
        assert_eq!(text(&g), ["a", ""]);
        assert_eq!(g.cursor(), (1, 0));
    }

    #[test]
    fn carriage_return_overwrites() {
        let mut g = TextGrid::<10, 2>::new();
        g.feed_all(b"hello\rJ");
        assert_eq!(text(&g), ["Jello", ""]);
    }

    #[test]
    fn tab_goes_to_the_next_multiple_of_eight() {
        let mut g = TextGrid::<20, 2>::new();
        g.feed_all(b"ab\tc\td");
        assert_eq!(g.cell(8, 0), b'c');
        assert_eq!(g.cell(16, 0), b'd', "from column 9 the next stop is 16");
        // Two tabs in a row from a stop go two stops on (here past the 20-column
        // edge, so the next character wraps).
        let mut g = TextGrid::<20, 2>::new();
        g.feed_all(b"ab\t\t\td");
        assert_eq!(g.cell(0, 1), b'd');
        // A tab at the end of a narrow grid does not run past it.
        let mut g = TextGrid::<6, 2>::new();
        g.feed_all(b"abc\t");
        assert_eq!(g.cursor(), (5, 0));
    }

    #[test]
    fn other_bytes_are_ignored() {
        let mut g = TextGrid::<10, 2>::new();
        g.feed_all(&[b'a', 0x00, 0x1b, 0x7f, 0x80, 0xff, b'b']);
        assert_eq!(text(&g), ["ab", ""]);
    }

    #[test]
    fn rendering_draws_cells_and_a_cursor_bar() {
        let mut g = TextGrid::<3, 2>::new();
        g.feed(b'H');
        let mut data = vec![0x123456u32; 60 * 40];
        let mut s = unsafe { Surface::from_raw(data.as_mut_ptr(), 60, 40, 60) };
        g.render(&mut s, 4, 5, 2);
        // The H is two bars with a gap between them (scale 2, so each font pixel is 2 wide):
        // the left bar is columns 0-3, the gap 4-7, the right bar 8-11.
        assert_eq!(s.get(4 + 1, 5 + 1), Some(FOREGROUND));
        assert_eq!(s.get(4 + 5, 5 + 1), Some(BACKGROUND), "the gap");
        assert_eq!(s.get(4 + 9, 5 + 1), Some(FOREGROUND));
        assert_eq!(s.get(4 + 13, 5 + 1), Some(BACKGROUND), "right of the H");
        // Blank cells are background.
        assert_eq!(s.get(4 + 20, 5 + 3), Some(BACKGROUND));
        // The cursor (now in column 1, row 0) is a bar at the bottom of its cell.
        assert_eq!(s.get(4 + 16 + 3, 5 + 16 - 1), Some(FOREGROUND));
        assert_eq!(s.get(4 + 16 + 3, 5 + 16 - 3), Some(BACKGROUND));
        // Outside the grid nothing was painted.
        assert_eq!(s.get(3, 5), Some(0x123456));
        assert_eq!(s.get(4 + 48, 5), Some(0x123456));
        assert_eq!(s.get(4, 5 + 32), Some(0x123456));
    }
}
