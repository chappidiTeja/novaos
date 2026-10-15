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


// ---- networking ----------------------------------------------------------------------

/// An IPv4 address and port, as passed to the socket calls (8 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct SockAddr {
    pub ip: [u8; 4],
    /// Host byte order (the kernel stores it little endian, like every other integer here).
    pub port: u16,
    pub pad: u16,
}

impl SockAddr {
    pub const fn new(ip: [u8; 4], port: u16) -> SockAddr {
        SockAddr { ip, port, pad: 0 }
    }
}

pub const NET_LINK_UP: u32 = 1;
pub const NET_HAS_ADDRESS: u32 = 2;
pub const NET_DHCP_BOUND: u32 = 4;

/// What `netinfo` returns (64 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct NetInfo {
    pub flags: u32,
    pub mac: [u8; 6],
    pub pad0: u16,
    pub ip: [u8; 4],
    pub mask: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
    pub pad1: u32,
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}

/// "a.b.c.d" -> address. Strict: four decimal numbers 0-255 and nothing else.
pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for slot in out.iter_mut() {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut v: u32 = 0;
        for b in p.bytes() {
            v = v * 10 + (b - b'0') as u32;
        }
        if v > 255 {
            return None;
        }
        *slot = v as u8;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// Prints as a dotted address.
pub struct Ip4(pub [u8; 4]);

impl core::fmt::Display for Ip4 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
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

#[cfg(test)]
mod net_tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn network_layouts_match_the_kernel() {
        assert_eq!(size_of::<SockAddr>(), 8);
        assert_eq!(size_of::<NetInfo>(), 64);
        assert_eq!(offset_of!(NetInfo, mac), 4);
        assert_eq!(offset_of!(NetInfo, ip), 12);
        assert_eq!(offset_of!(NetInfo, mask), 16);
        assert_eq!(offset_of!(NetInfo, gateway), 20);
        assert_eq!(offset_of!(NetInfo, dns), 24);
        assert_eq!(offset_of!(NetInfo, rx_frames), 32);
        assert_eq!(offset_of!(NetInfo, tx_dropped), 56);
        assert_eq!(offset_of!(SockAddr, port), 4);
    }

    #[test]
    fn parse_ip_is_strict() {
        assert_eq!(parse_ip("10.0.2.15"), Some([10, 0, 2, 15]));
        assert_eq!(parse_ip("0.0.0.0"), Some([0; 4]));
        assert_eq!(parse_ip("255.255.255.255"), Some([255; 4]));
        for bad in ["", "1.2.3", "1.2.3.4.5", "256.1.1.1", "1..2.3", "1.2.3.", ".1.2.3", "a.b.c.d", "1.2.3.0x4", "+1.2.3.4", "1.2.3.4 ", " 1.2.3.4", "1.2.3.-4", "0001.2.3.4", "999.1.1.1"] {
            assert_eq!(parse_ip(bad), None, "{bad:?} must be refused");
        }
    }

    #[test]
    fn ip_prints_dotted() {
        use core::fmt::Write;
        let mut s = heapless_string::S::default();
        write!(s, "{}", Ip4([192, 168, 1, 7])).unwrap();
        assert_eq!(s.as_str(), "192.168.1.7");
    }

    mod heapless_string {
        #[derive(Default)]
        pub struct S {
            buf: [u8; 32],
            len: usize,
        }
        impl S {
            pub fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..self.len]).unwrap()
            }
        }
        impl core::fmt::Write for S {
            fn write_str(&mut self, t: &str) -> core::fmt::Result {
                self.buf[self.len..self.len + t.len()].copy_from_slice(t.as_bytes());
                self.len += t.len();
                Ok(())
            }
        }
    }
}
