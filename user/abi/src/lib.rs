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


// ---- signals, keys and processes -------------------------------------------------------------

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGKILL: u32 = 9;
pub const SIGUSR1: u32 = 10;
pub const SIGUSR2: u32 = 12;
pub const SIGALRM: u32 = 14;
pub const SIGTERM: u32 = 15;

/// Exit status of a process ended by signal `n`: `-(SIGNALED_BASE + n)`.
pub const SIGNALED_BASE: i64 = 2000;
/// Exit status of a process ended by a CPU exception: `-(KILLED_BASE + vector)`.
pub const KILLED_BASE: i64 = 1000;

/// The signal that ended a process, if its exit status says it was a signal.
pub fn signal_of(status: i64) -> Option<u32> {
    let n = -status - SIGNALED_BASE;
    if (1..32).contains(&n) {
        Some(n as u32)
    } else {
        None
    }
}

/// The name a person would type for a signal ("INT", "KILL"...).
pub fn signal_name(sig: u32) -> &'static str {
    match sig {
        SIGHUP => "HUP",
        SIGINT => "INT",
        SIGQUIT => "QUIT",
        SIGKILL => "KILL",
        SIGUSR1 => "USR1",
        SIGUSR2 => "USR2",
        SIGALRM => "ALRM",
        SIGTERM => "TERM",
        _ => "?",
    }
}

/// The signal for a name like "TERM" or "SIGTERM" (case-insensitive) or a number.
pub fn signal_from_name(s: &str) -> Option<u32> {
    let s = s.strip_prefix("SIG").or_else(|| s.strip_prefix("sig")).unwrap_or(s);
    if let Some(n) = s.bytes().all(|b| b.is_ascii_digit()).then(|| s.bytes().fold(0u32, |v, b| v * 10 + (b - b'0') as u32)) {
        return if s.is_empty() || n > 31 { None } else { Some(n) };
    }
    [SIGHUP, SIGINT, SIGQUIT, SIGKILL, SIGUSR1, SIGUSR2, SIGALRM, SIGTERM]
        .into_iter()
        .find(|&sig| signal_name(sig).eq_ignore_ascii_case(s))
}

/// Keys without an ASCII character arrive as these codes (keyboard events to the desktop).
pub const KEY_UP: i32 = 0x101;
pub const KEY_DOWN: i32 = 0x102;
pub const KEY_RIGHT: i32 = 0x103;
pub const KEY_LEFT: i32 = 0x104;
pub const KEY_HOME: i32 = 0x105;
pub const KEY_END: i32 = 0x106;
pub const KEY_DELETE: i32 = 0x107;
pub const KEY_PAGE_UP: i32 = 0x108;
pub const KEY_PAGE_DOWN: i32 = 0x109;
pub const KEY_INSERT: i32 = 0x10A;

/// What a text terminal receives for a special key (the kernel's console sends the same).
pub fn key_escape(code: i32) -> &'static [u8] {
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

pub const PROC_RUNNING: u32 = 0;
pub const PROC_WAITING: u32 = 1;
pub const PROC_ZOMBIE: u32 = 2;

/// One entry of `proclist` (64 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    /// PROC_RUNNING (running or ready), PROC_WAITING (blocked or sleeping), PROC_ZOMBIE (finished,
    /// not waited for).
    pub state: u32,
    pub pending_signals: u32,
    /// Timer tick (100 per second) at which the process started.
    pub start_tick: u64,
    /// Timer ticks it has spent running.
    pub cpu_ticks: u64,
    /// Pages of heap and shared memory.
    pub mem_pages: u64,
    /// Exit status (meaningful for zombies).
    pub exit_status: i64,
    pub name: [u8; 16],
}

impl ProcInfo {
    pub const EMPTY: ProcInfo = ProcInfo {
        pid: 0,
        ppid: 0,
        state: 0,
        pending_signals: 0,
        start_tick: 0,
        cpu_ticks: 0,
        mem_pages: 0,
        exit_status: 0,
        name: [0; 16],
    };

    pub fn name(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..len]).unwrap_or("?")
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

#[cfg(test)]
mod proc_tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn proc_info_layout_matches_the_kernel() {
        assert_eq!(size_of::<ProcInfo>(), 64);
        assert_eq!(offset_of!(ProcInfo, ppid), 4);
        assert_eq!(offset_of!(ProcInfo, state), 8);
        assert_eq!(offset_of!(ProcInfo, pending_signals), 12);
        assert_eq!(offset_of!(ProcInfo, start_tick), 16);
        assert_eq!(offset_of!(ProcInfo, cpu_ticks), 24);
        assert_eq!(offset_of!(ProcInfo, mem_pages), 32);
        assert_eq!(offset_of!(ProcInfo, exit_status), 40);
        assert_eq!(offset_of!(ProcInfo, name), 48);
    }

    #[test]
    fn signal_status_roundtrip() {
        for sig in [SIGHUP, SIGINT, SIGKILL, SIGTERM] {
            assert_eq!(signal_of(-(SIGNALED_BASE + sig as i64)), Some(sig));
        }
        assert_eq!(signal_of(0), None);
        assert_eq!(signal_of(1), None);
        assert_eq!(signal_of(-(KILLED_BASE + 14)), None, "a CPU exception is not a signal");
        assert_eq!(signal_of(-(SIGNALED_BASE + 40)), None);
    }

    #[test]
    fn signal_names_parse() {
        assert_eq!(signal_from_name("TERM"), Some(SIGTERM));
        assert_eq!(signal_from_name("SIGKILL"), Some(SIGKILL));
        assert_eq!(signal_from_name("int"), Some(SIGINT));
        assert_eq!(signal_from_name("9"), Some(9));
        assert_eq!(signal_from_name("0"), Some(0));
        assert_eq!(signal_from_name("99"), None);
        assert_eq!(signal_from_name(""), None);
        assert_eq!(signal_from_name("BOGUS"), None);
    }

    #[test]
    fn key_escape_sequences_are_the_usual_ones() {
        assert_eq!(key_escape(KEY_UP), b"\x1b[A");
        assert_eq!(key_escape(KEY_DELETE), b"\x1b[3~");
        assert_eq!(key_escape(b'a' as i32), b"");
    }
}
