use crate::port::inb;
use core::sync::atomic::{AtomicBool, Ordering};

// Scancode set 1, make codes 0x00..0x39. 0 means "no printable character".
const NORMAL: &[u8] = b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: &[u8] = b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

static SHIFT: AtomicBool = AtomicBool::new(false);

pub fn handle_interrupt() {
    let scancode = unsafe { inb(0x60) };
    let released = scancode & 0x80 != 0;
    let code = scancode & 0x7F;

    match code {
        0x2A | 0x36 => SHIFT.store(!released, Ordering::Relaxed),
        _ if released => {}
        _ => {
            let table = if SHIFT.load(Ordering::Relaxed) { SHIFTED } else { NORMAL };
            if let Some(&c) = table.get(code as usize) {
                if c != 0 {
                    // To the desktop if one owns the input, otherwise to stdin.
                    crate::input::key(c);
                }
            }
        }
    }
}
