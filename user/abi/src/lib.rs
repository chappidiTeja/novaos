//! Types shared between the NovaOS kernel interface, the `nova` library and the
//! window manager. Plain data with fixed layouts, nothing else (no panic handler,
//! no system calls), so it also builds on the host for unit tests.

#![cfg_attr(not(test), no_std)]

// Events to the desktop (the input owner).
pub const EV_MOUSE_MOVE: u32 = 1;
pub const EV_MOUSE_BUTTON: u32 = 2;
pub const EV_KEY: u32 = 3;
pub const EV_DAMAGE: u32 = 4;
// Events to a window's owner.
pub const EV_WIN_KEY: u32 = 10;
pub const EV_WIN_MOVE: u32 = 11;
pub const EV_WIN_BUTTON: u32 = 12;
pub const EV_WIN_CLOSE: u32 = 13;
pub const EV_WIN_FOCUS: u32 = 14;

pub const BUTTON_LEFT: i32 = 1;
pub const BUTTON_RIGHT: i32 = 2;
pub const BUTTON_MIDDLE: i32 = 4;

/// One event, 16 bytes, the same layout the kernel uses.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Event {
    pub kind: u32,
    pub a: i32,
    pub b: i32,
    pub c: i32,
}

impl Event {
    pub const fn new(kind: u32, a: i32, b: i32, c: i32) -> Event {
        Event { kind, a, b, c }
    }
}

/// What `gfx_claim` returns about the screen.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ScreenInfo {
    /// Where the framebuffer is mapped in this process.
    pub address: u64,
    pub width: u64,
    pub height: u64,
    /// Bytes per row.
    pub pitch: u64,
    pub red_shift: u64,
    pub green_shift: u64,
    pub blue_shift: u64,
    pub bytes: u64,
}

/// One entry of `win_list`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WinInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub title_len: u32,
    pub owner: u64,
    pub title: [u8; 32],
    /// Counts how often the owner has said the pixels changed.
    pub serial: u64,
}

impl WinInfo {
    pub const EMPTY: WinInfo =
        WinInfo { id: 0, width: 0, height: 0, title_len: 0, owner: 0, title: [0; 32], serial: 0 };

    pub fn title(&self) -> &[u8] {
        &self.title[..(self.title_len as usize).min(32)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    /// The kernel reads and writes these layouts byte for byte; a size change would
    /// silently break every system call that passes them.
    #[test]
    fn layouts_match_the_kernel() {
        assert_eq!(size_of::<Event>(), 16);
        assert_eq!(size_of::<ScreenInfo>(), 64);
        assert_eq!(size_of::<WinInfo>(), 64);
    }

    #[test]
    fn winfo_fields_sit_where_the_kernel_puts_them() {
        let mut raw = [0u8; 64];
        raw[0..4].copy_from_slice(&7u32.to_le_bytes());
        raw[4..8].copy_from_slice(&640u32.to_le_bytes());
        raw[8..12].copy_from_slice(&480u32.to_le_bytes());
        raw[12..16].copy_from_slice(&3u32.to_le_bytes());
        raw[16..24].copy_from_slice(&99u64.to_le_bytes());
        raw[24..27].copy_from_slice(b"abc");
        raw[56..64].copy_from_slice(&5u64.to_le_bytes());
        let info: WinInfo = unsafe { core::ptr::read_unaligned(raw.as_ptr() as *const WinInfo) };
        assert_eq!((info.id, info.width, info.height, info.title_len, info.owner, info.serial), (7, 640, 480, 3, 99, 5));
        assert_eq!(info.title(), b"abc");
    }
}
