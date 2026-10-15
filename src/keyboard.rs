//! PS/2 keyboard (scancode set 1): letters, digits, Shift, Control, Caps Lock, and the
//! "extended" keys (arrows, Home, End, Delete, Page Up/Down) that arrive after a 0xE0 byte.

use crate::port::inb;
use core::sync::atomic::{AtomicBool, Ordering};

// Scancode set 1, make codes 0x00..0x39. 0 means "no printable character".
const NORMAL: &[u8] = b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: &[u8] = b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

/// Key codes above the byte range, for keys that have no ASCII character. The desktop gets
/// them as they are; the text console gets the usual ANSI escape sequence (`escape_sequence`).
pub const KEY_UP: u32 = 0x101;
pub const KEY_DOWN: u32 = 0x102;
pub const KEY_RIGHT: u32 = 0x103;
pub const KEY_LEFT: u32 = 0x104;
pub const KEY_HOME: u32 = 0x105;
pub const KEY_END: u32 = 0x106;
pub const KEY_DELETE: u32 = 0x107;
pub const KEY_PAGE_UP: u32 = 0x108;
pub const KEY_PAGE_DOWN: u32 = 0x109;
pub const KEY_INSERT: u32 = 0x10A;

static SHIFT: AtomicBool = AtomicBool::new(false);
static CTRL: AtomicBool = AtomicBool::new(false);
static ALT: AtomicBool = AtomicBool::new(false);
static CAPS: AtomicBool = AtomicBool::new(false);
/// The previous byte was the 0xE0 prefix.
static EXTENDED: AtomicBool = AtomicBool::new(false);

/// What a text terminal receives for a special key.
pub fn escape_sequence(code: u32) -> &'static [u8] {
    match code {
        KEY_UP => b"\x1b[A",
        KEY_DOWN => b"\x1b[B",
        KEY_RIGHT => b"\x1b[C",
        KEY_LEFT => b"\x1b[D",
        KEY_HOME => b"\x1b[H",
        KEY_END => b"\x1b[F",
        KEY_DELETE => b"\x1b[3~",
        KEY_PAGE_UP => b"\x1b[5~",
        KEY_PAGE_DOWN => b"\x1b[6~",
        KEY_INSERT => b"\x1b[2~",
        _ => b"",
    }
}

fn extended_key(code: u8) -> Option<u32> {
    Some(match code {
        0x48 => KEY_UP,
        0x50 => KEY_DOWN,
        0x4D => KEY_RIGHT,
        0x4B => KEY_LEFT,
        0x47 => KEY_HOME,
        0x4F => KEY_END,
        0x53 => KEY_DELETE,
        0x49 => KEY_PAGE_UP,
        0x51 => KEY_PAGE_DOWN,
        0x52 => KEY_INSERT,
        0x1C => b'\n' as u32, // keypad Enter
        0x35 => b'/' as u32,  // keypad /
        _ => return None,
    })
}

/// The character for make code `code` with the modifiers held now.
fn printable(code: u8) -> Option<u32> {
    let base = *NORMAL.get(code as usize)?;
    if base == 0 {
        return None;
    }
    let letter = base.is_ascii_lowercase();
    let shift = SHIFT.load(Ordering::Relaxed);
    // Caps Lock flips the case of letters only.
    let upper = shift ^ (letter && CAPS.load(Ordering::Relaxed));
    let table = if letter { if upper { SHIFTED } else { NORMAL } } else if shift { SHIFTED } else { NORMAL };
    let ch = *table.get(code as usize)?;
    if CTRL.load(Ordering::Relaxed) {
        // Control-letter is the letter's position in the alphabet (Control-C = 3); a few
        // punctuation keys have control codes too.
        return match base {
            b'a'..=b'z' => Some((base & 0x1F) as u32),
            b'[' => Some(0x1B),
            b'\\' => Some(0x1C),
            b']' => Some(0x1D),
            b' ' => Some(0),
            _ => Some(ch as u32),
        };
    }
    Some(ch as u32)
}

pub fn handle_interrupt() {
    let scancode = unsafe { inb(0x60) };
    if scancode == 0xE0 {
        EXTENDED.store(true, Ordering::Relaxed);
        return;
    }
    if scancode == 0xE1 {
        return; // the Pause key sends a long sequence we do not use
    }
    let extended = EXTENDED.swap(false, Ordering::Relaxed);
    let released = scancode & 0x80 != 0;
    let code = scancode & 0x7F;

    match (extended, code) {
        // Some keyboards send a fake Shift around extended keys; it is not a real Shift.
        (true, 0x2A | 0x36) => {}
        (_, 0x2A | 0x36) => SHIFT.store(!released, Ordering::Relaxed),
        (_, 0x1D) => CTRL.store(!released, Ordering::Relaxed),
        (_, 0x38) => ALT.store(!released, Ordering::Relaxed),
        (false, 0x3A) if !released => {
            CAPS.fetch_xor(true, Ordering::Relaxed);
        }
        _ if released => {}
        (true, c) => {
            if let Some(key) = extended_key(c) {
                // To the desktop if one owns the input, otherwise to stdin.
                crate::input::key(key);
            }
        }
        (false, c) => {
            if let Some(key) = printable(c) {
                crate::input::key(key);
            }
        }
    }
    let _ = ALT.load(Ordering::Relaxed); // tracked for future shortcuts
}
