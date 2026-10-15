//! Reading a command line with editing: cursor movement, history, Tab completion.
//!
//! The terminal is dumb (no escape-sequence handling), so redrawing uses only backspaces,
//! spaces and the text itself. A line longer than the screen is wide enough to wrap, which the
//! console cannot back over: editing such a line still works but the picture is untidy.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use nova::*;

pub enum Line {
    Text(String),
    /// End of input (Control-D on an empty line, or the input closed).
    Eof,
    /// Control-C: the line is abandoned.
    Interrupted,
}

/// Where completion found candidates: the word starts at byte `start` of the line, and each
/// candidate is a full replacement for it.
pub struct Completion {
    pub start: usize,
    pub candidates: Vec<String>,
}

pub struct Editor {
    pending: VecDeque<u8>,
}

fn put(bytes: &[u8]) {
    write_all(STDOUT, bytes);
}

fn spaces(n: usize) -> Vec<u8> {
    alloc::vec![b' '; n]
}

fn backs(n: usize) -> Vec<u8> {
    alloc::vec![0x08; n]
}

impl Editor {
    pub fn new() -> Editor {
        Editor { pending: VecDeque::new() }
    }

    fn next_byte(&mut self) -> Option<u8> {
        if let Some(b) = self.pending.pop_front() {
            return Some(b);
        }
        let mut buf = [0u8; 64];
        let n = read(STDIN, &mut buf);
        if n <= 0 {
            return None;
        }
        self.pending.extend(&buf[..n as usize]);
        self.pending.pop_front()
    }

    /// Read a line. `prompt` is only used to redraw after Control-L or a completion listing (the
    /// caller has already printed it).
    pub fn read_line(
        &mut self,
        prompt: &str,
        history: &[String],
        complete: &mut dyn FnMut(&str, usize) -> Completion,
    ) -> Line {
        let mut line: Vec<u8> = Vec::new();
        let mut cursor = 0usize;
        // Position in the history while browsing (None: editing the new line) and the new line
        // put aside while browsing.
        let mut browsing: Option<usize> = None;
        let mut saved: Vec<u8> = Vec::new();
        let mut last_was_tab = false;

        macro_rules! replace_all {
            ($new:expr, $new_cursor:expr) => {{
                let new: Vec<u8> = $new;
                let mut out = backs(cursor);
                out.extend_from_slice(&new);
                if new.len() < line.len() {
                    out.extend(spaces(line.len() - new.len()));
                    out.extend(backs(line.len() - new.len()));
                }
                let new_cursor: usize = $new_cursor;
                out.extend(backs(new.len() - new_cursor));
                put(&out);
                line = new;
                cursor = new_cursor;
            }};
        }

        loop {
            let Some(b) = self.next_byte() else {
                return if line.is_empty() { Line::Eof } else { Line::Text(String::from_utf8_lossy(&line).into_owned()) };
            };
            let was_tab = core::mem::replace(&mut last_was_tab, false);
            match b {
                b'\n' | b'\r' => {
                    put(b"\n");
                    return Line::Text(String::from_utf8_lossy(&line).into_owned());
                }
                0x03 => {
                    put(b"^C\n");
                    return Line::Interrupted;
                }
                0x04 => {
                    if line.is_empty() {
                        return Line::Eof;
                    }
                    if cursor < line.len() {
                        line.remove(cursor);
                        let mut out = line[cursor..].to_vec();
                        out.push(b' ');
                        out.extend(backs(line.len() - cursor + 1));
                        put(&out);
                    }
                }
                0x01 => {
                    put(&backs(cursor));
                    cursor = 0;
                }
                0x05 => {
                    put(&line[cursor..]);
                    cursor = line.len();
                }
                0x02 => {
                    if cursor > 0 {
                        cursor -= 1;
                        put(b"\x08");
                    }
                }
                0x06 => {
                    if cursor < line.len() {
                        put(&line[cursor..cursor + 1]);
                        cursor += 1;
                    }
                }
                0x08 | 0x7F => {
                    if cursor > 0 {
                        line.remove(cursor - 1);
                        cursor -= 1;
                        let mut out = alloc::vec![0x08u8];
                        out.extend_from_slice(&line[cursor..]);
                        out.push(b' ');
                        out.extend(backs(line.len() - cursor + 1));
                        put(&out);
                    }
                }
                0x0B => {
                    let n = line.len() - cursor;
                    let mut out = spaces(n);
                    out.extend(backs(n));
                    put(&out);
                    line.truncate(cursor);
                }
                0x15 => {
                    let tail: Vec<u8> = line[cursor..].to_vec();
                    replace_all!(tail, 0);
                }
                0x17 => {
                    // Delete the word before the cursor.
                    let mut start = cursor;
                    while start > 0 && line[start - 1] == b' ' {
                        start -= 1;
                    }
                    while start > 0 && line[start - 1] != b' ' {
                        start -= 1;
                    }
                    let mut new = line[..start].to_vec();
                    new.extend_from_slice(&line[cursor..]);
                    replace_all!(new, start);
                }
                0x0C => {
                    put(b"\x0c");
                    put(prompt.as_bytes());
                    put(&line);
                    put(&backs(line.len() - cursor));
                }
                0x10 | 0x0E => {
                    let up = b == 0x10;
                    if let Some(new) = self.browse(up, history, &mut browsing, &mut saved, &line) {
                        let n = new.len();
                        replace_all!(new, n);
                    }
                }
                b'\t' => {
                    last_was_tab = true;
                    let text = String::from_utf8_lossy(&line).into_owned();
                    let Completion { start, candidates } = complete(&text, cursor);
                    if candidates.is_empty() {
                        continue;
                    }
                    let current = &text[start..cursor];
                    let replacement = if candidates.len() == 1 {
                        let mut r = candidates[0].clone();
                        if !r.ends_with('/') {
                            r.push(' ');
                        }
                        Some(r)
                    } else {
                        let lcp = common_prefix(&candidates);
                        if lcp.len() > current.len() {
                            Some(lcp)
                        } else {
                            None
                        }
                    };
                    match replacement {
                        Some(r) => {
                            let mut new = line[..start].to_vec();
                            new.extend_from_slice(r.as_bytes());
                            new.extend_from_slice(&line[cursor..]);
                            let new_cursor = start + r.len();
                            replace_all!(new, new_cursor);
                        }
                        None if was_tab => {
                            // Second Tab in a row: list the candidates, then redraw the line.
                            put(b"\n");
                            put(columns(&candidates).as_bytes());
                            put(prompt.as_bytes());
                            put(&line);
                            put(&backs(line.len() - cursor));
                        }
                        None => {}
                    }
                }
                0x1B => match self.read_escape() {
                    key @ (Key::Up | Key::Down) => {
                        let up = key == Key::Up;
                        if let Some(new) = self.browse(up, history, &mut browsing, &mut saved, &line) {
                            let n = new.len();
                            replace_all!(new, n);
                        }
                    }
                    Key::Left => {
                        if cursor > 0 {
                            cursor -= 1;
                            put(b"\x08");
                        }
                    }
                    Key::Right => {
                        if cursor < line.len() {
                            put(&line[cursor..cursor + 1]);
                            cursor += 1;
                        }
                    }
                    Key::Home => {
                        put(&backs(cursor));
                        cursor = 0;
                    }
                    Key::End => {
                        put(&line[cursor..]);
                        cursor = line.len();
                    }
                    Key::Delete => {
                        if cursor < line.len() {
                            line.remove(cursor);
                            let mut out = line[cursor..].to_vec();
                            out.push(b' ');
                            out.extend(backs(line.len() - cursor + 1));
                            put(&out);
                        }
                    }
                    Key::Other => {}
                },
                0x20..=0x7E => {
                    line.insert(cursor, b);
                    let mut out = line[cursor..].to_vec();
                    out.extend(backs(line.len() - cursor - 1));
                    put(&out);
                    cursor += 1;
                }
                _ => {}
            }
        }
    }

    /// Step through history (`up` = older). `apply` redraws the line with the chosen text.
    fn browse(
        &mut self,
        up: bool,
        history: &[String],
        browsing: &mut Option<usize>,
        saved: &mut Vec<u8>,
        current: &[u8],
    ) -> Option<Vec<u8>> {
        if history.is_empty() {
            return None;
        }
        let next = match (*browsing, up) {
            (None, true) => {
                *saved = current.to_vec();
                Some(history.len() - 1)
            }
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i + 1 < history.len() => Some(i + 1),
            (Some(_), false) => None,
            (None, false) => return None,
        };
        *browsing = next;
        let text = match next {
            Some(i) => history[i].as_bytes().to_vec(),
            None => saved.clone(),
        };
        Some(text)
    }

    fn read_escape(&mut self) -> Key {
        // The bytes of one sequence arrive together. If nothing at all follows the Escape, it was
        // the Escape key itself.
        if self.pending.is_empty() && fd_ready(STDIN) != 1 {
            return Key::Other;
        }
        match self.next_byte() {
            Some(b'[') => {}
            Some(b'O') => {
                return match self.next_byte() {
                    Some(b'H') => Key::Home,
                    Some(b'F') => Key::End,
                    Some(b'A') => Key::Up,
                    Some(b'B') => Key::Down,
                    Some(b'C') => Key::Right,
                    Some(b'D') => Key::Left,
                    _ => Key::Other,
                };
            }
            _ => return Key::Other,
        }
        let mut number = 0u32;
        loop {
            match self.next_byte() {
                Some(d @ b'0'..=b'9') => number = number * 10 + (d - b'0') as u32,
                Some(b';') => number = 0,
                Some(b'A') => return Key::Up,
                Some(b'B') => return Key::Down,
                Some(b'C') => return Key::Right,
                Some(b'D') => return Key::Left,
                Some(b'H') => return Key::Home,
                Some(b'F') => return Key::End,
                Some(b'~') => {
                    return match number {
                        1 | 7 => Key::Home,
                        4 | 8 => Key::End,
                        3 => Key::Delete,
                        _ => Key::Other,
                    }
                }
                _ => return Key::Other,
            }
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Other,
}


/// The longest text all candidates start with.
fn common_prefix(candidates: &[String]) -> String {
    let first = &candidates[0];
    let mut len = first.len();
    for c in &candidates[1..] {
        len = len.min(first.bytes().zip(c.bytes()).take_while(|(a, b)| a == b).count());
    }
    String::from(&first[..len])
}

/// Candidates laid out in columns for a screen 79 characters wide.
fn columns(candidates: &[String]) -> String {
    let width = candidates.iter().map(|c| c.len()).max().unwrap_or(1) + 2;
    let per_row = (78 / width).max(1);
    let mut out = String::new();
    for (i, c) in candidates.iter().enumerate() {
        out.push_str(c);
        if (i + 1) % per_row == 0 || i + 1 == candidates.len() {
            out.push('\n');
        } else {
            out.extend(core::iter::repeat(' ').take(width - c.len()));
        }
    }
    out
}
