//! Support library for NovaOS programs: system calls, printing, arguments.
//!
//! A program looks like this:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//! use nova::*;
//!
//! fn main(args: &Args) -> i32 {
//!     println!("hello, {}", args.get(1));
//!     0
//! }
//! entry!(main);
//! ```

#![no_std]

// The types that cross the system call boundary live in their own tiny crate, so
// the window manager can share them without pulling in this library's runtime.
pub use novaabi::*;

use core::arch::asm;
use core::fmt;
use core::panic::PanicInfo;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_GETPID: u64 = 2;
pub const SYS_SLEEP: u64 = 3;
pub const SYS_YIELD: u64 = 4;
pub const SYS_TICKS: u64 = 5;
pub const SYS_READ: u64 = 6;
pub const SYS_PIPE: u64 = 7;
pub const SYS_CLOSE: u64 = 8;
pub const SYS_OPEN: u64 = 9;
pub const SYS_LSEEK: u64 = 10;
pub const SYS_FSTAT: u64 = 11;
pub const SYS_STAT: u64 = 12;
pub const SYS_READDIR: u64 = 13;
pub const SYS_MKDIR: u64 = 14;
pub const SYS_UNLINK: u64 = 15;
pub const SYS_RMDIR: u64 = 16;
pub const SYS_CHDIR: u64 = 17;
pub const SYS_GETCWD: u64 = 18;
pub const SYS_SPAWN: u64 = 19;
pub const SYS_WAITPID: u64 = 20;

pub const O_RDONLY: u64 = 0;
pub const O_WRONLY: u64 = 1;
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0x40;
pub const O_EXCL: u64 = 0x80;
pub const O_TRUNC: u64 = 0x200;
pub const O_APPEND: u64 = 0x400;

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

pub const STDIN: u64 = 0;
pub const STDOUT: u64 = 1;
pub const STDERR: u64 = 2;

pub const KIND_FILE: u64 = 1;
pub const KIND_DIR: u64 = 2;

/// The kernel clobbers rdi, rsi, rdx, r10, r8 and r9 on every system call.
#[inline(always)]
pub unsafe fn syscall(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") nr as i64 => ret,
        inlateout("rdi") a1 => _,
        inlateout("rsi") a2 => _,
        inlateout("rdx") a3 => _,
        inlateout("r10") a4 => _,
        inlateout("r8") a5 => _,
        out("r9") _,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

// ---- thin wrappers -----------------------------------------------------------------

pub fn exit(code: i64) -> ! {
    unsafe { syscall(SYS_EXIT, code as u64, 0, 0, 0, 0) };
    loop {}
}

pub fn getpid() -> u64 {
    unsafe { syscall(SYS_GETPID, 0, 0, 0, 0, 0) as u64 }
}

pub fn sleep(ticks: u64) {
    unsafe { syscall(SYS_SLEEP, ticks, 0, 0, 0, 0) };
}

pub fn ticks() -> u64 {
    unsafe { syscall(SYS_TICKS, 0, 0, 0, 0, 0) as u64 }
}

pub fn write(fd: u64, data: &[u8]) -> i64 {
    unsafe { syscall(SYS_WRITE, fd, data.as_ptr() as u64, data.len() as u64, 0, 0) }
}

/// Write all of `data`, retrying after short writes.
pub fn write_all(fd: u64, mut data: &[u8]) -> i64 {
    while !data.is_empty() {
        let n = write(fd, data);
        if n < 0 {
            return n;
        }
        if n == 0 {
            return -5;
        }
        data = &data[n as usize..];
    }
    0
}

pub fn read(fd: u64, buf: &mut [u8]) -> i64 {
    unsafe { syscall(SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) }
}

pub fn close(fd: u64) -> i64 {
    unsafe { syscall(SYS_CLOSE, fd, 0, 0, 0, 0) }
}

/// `path` must end with a NUL byte.
pub fn open(path: &[u8], flags: u64) -> i64 {
    debug_assert!(path.last() == Some(&0));
    unsafe { syscall(SYS_OPEN, path.as_ptr() as u64, flags, 0, 0, 0) }
}

pub fn lseek(fd: u64, offset: i64, whence: u64) -> i64 {
    unsafe { syscall(SYS_LSEEK, fd, offset as u64, whence, 0, 0) }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Stat {
    pub ino: u64,
    pub kind: u64,
    pub size: u64,
    pub nlink: u64,
    pub blocks: u64,
}

pub fn fstat(fd: u64, out: &mut Stat) -> i64 {
    unsafe { syscall(SYS_FSTAT, fd, out as *mut Stat as u64, 0, 0, 0) }
}

/// `path` must end with a NUL byte.
pub fn stat(path: &[u8], out: &mut Stat) -> i64 {
    unsafe { syscall(SYS_STAT, path.as_ptr() as u64, out as *mut Stat as u64, 0, 0, 0) }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    pub ino: u64,
    pub kind: u64,
    pub name_len: u64,
    pub name: [u8; 64],
}

impl DirEntry {
    pub const EMPTY: DirEntry = DirEntry { ino: 0, kind: 0, name_len: 0, name: [0; 64] };

    pub fn name(&self) -> &[u8] {
        &self.name[..(self.name_len as usize).min(64)]
    }
}

/// 1 = filled in an entry, 0 = no more, negative = error.
pub fn readdir(fd: u64, out: &mut DirEntry) -> i64 {
    unsafe { syscall(SYS_READDIR, fd, out as *mut DirEntry as u64, 0, 0, 0) }
}

fn path_call(nr: u64, path: &[u8]) -> i64 {
    unsafe { syscall(nr, path.as_ptr() as u64, 0, 0, 0, 0) }
}

pub fn mkdir(path: &[u8]) -> i64 {
    path_call(SYS_MKDIR, path)
}

pub fn unlink(path: &[u8]) -> i64 {
    path_call(SYS_UNLINK, path)
}

pub fn rmdir(path: &[u8]) -> i64 {
    path_call(SYS_RMDIR, path)
}

pub fn chdir(path: &[u8]) -> i64 {
    path_call(SYS_CHDIR, path)
}

/// The working directory as text, without the NUL.
pub fn getcwd(buf: &mut [u8]) -> Option<&str> {
    let n = unsafe { syscall(SYS_GETCWD, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0) };
    if n <= 0 {
        return None;
    }
    core::str::from_utf8(&buf[..n as usize - 1]).ok()
}

/// Returns (read end, write end).
pub fn pipe() -> Result<(u64, u64), i64> {
    let mut fds = [0u64; 2];
    let r = unsafe { syscall(SYS_PIPE, fds.as_mut_ptr() as u64, 0, 0, 0, 0) };
    if r < 0 {
        Err(r)
    } else {
        Ok((fds[0], fds[1]))
    }
}

/// Start the program at `path` (NUL-terminated). `argv` is `argc` NUL-terminated
/// strings back to back. `stdio` names, as descriptors of this process, what the
/// new process gets as stdin, stdout and stderr; None means the same as ours.
/// Returns the new process id, or a negative error.
pub fn spawn(path: &[u8], argv: &[u8], argc: usize, stdio: Option<&[u64; 3]>) -> i64 {
    let stdio_ptr = stdio.map_or(0, |s| s.as_ptr() as u64);
    unsafe {
        syscall(
            SYS_SPAWN,
            path.as_ptr() as u64,
            argv.as_ptr() as u64,
            argv.len() as u64,
            argc as u64,
            stdio_ptr,
        )
    }
}

/// Wait for process `pid` to finish; returns its exit status (negative for a
/// process killed by a CPU exception: -(1000 + vector)).
pub fn waitpid(pid: u64) -> i64 {
    unsafe { syscall(SYS_WAITPID, pid, 0, 0, 0, 0) }
}

// ---- screen, input and windows ----------------------------------------------------------

pub const SYS_GFX_CLAIM: u64 = 21;
pub const SYS_MEM_ALLOC: u64 = 22;
pub const SYS_INPUT_NEXT: u64 = 23;
pub const SYS_WIN_CREATE: u64 = 24;
pub const SYS_WIN_DESTROY: u64 = 25;
pub const SYS_WIN_LIST: u64 = 26;
pub const SYS_WIN_MAP: u64 = 27;
pub const SYS_WIN_POST: u64 = 28;
pub const SYS_WIN_EVENT: u64 = 29;
pub const SYS_WIN_FLUSH: u64 = 30;
pub const SYS_FD_READY: u64 = 31;

/// Error numbers (the system calls return them negated).
pub mod errno {
    pub const EPERM: i64 = -1;
    pub const ENOENT: i64 = -2;
    pub const EBADF: i64 = -9;
    pub const ENOMEM: i64 = -12;
    pub const EFAULT: i64 = -14;
    pub const EBUSY: i64 = -16;
    pub const ENODEV: i64 = -19;
    pub const EINVAL: i64 = -22;
    pub const EAGAIN: i64 = -11;
    pub const EMFILE: i64 = -24;
    pub const EPIPE: i64 = -32;
    pub const ENOTSOCK: i64 = -88;
    pub const EDESTADDRREQ: i64 = -89;
    pub const EMSGSIZE: i64 = -90;
    pub const EADDRINUSE: i64 = -98;
    pub const EADDRNOTAVAIL: i64 = -99;
    pub const ENETDOWN: i64 = -100;
    pub const ENETUNREACH: i64 = -101;
    pub const ECONNRESET: i64 = -104;
    pub const ENOTCONN: i64 = -107;
    pub const ETIMEDOUT: i64 = -110;
    pub const ECONNREFUSED: i64 = -111;
}

/// Wait as long as it takes.
pub const FOREVER: u64 = u64::MAX;

/// Take over the screen and the keyboard and mouse. On success the framebuffer
/// is mapped and `info` describes it. Fails with EBUSY if someone else has it.
pub fn gfx_claim(info: &mut ScreenInfo) -> i64 {
    unsafe { syscall(SYS_GFX_CLAIM, info as *mut ScreenInfo as u64, 0, 0, 0, 0) }
}

/// `pages` fresh zeroed pages of memory (4096 bytes each); returns the address,
/// or a negative error.
pub fn mem_alloc(pages: u64) -> i64 {
    unsafe { syscall(SYS_MEM_ALLOC, pages, 0, 0, 0, 0) }
}

/// The desktop's next input event: 1 = filled `event`, 0 = timed out.
pub fn input_next(event: &mut Event, timeout_ticks: u64) -> i64 {
    unsafe { syscall(SYS_INPUT_NEXT, event as *mut Event as u64, timeout_ticks, 0, 0, 0) }
}

/// Create a window. On success returns (id, address of its pixels); the pixels
/// are `width * height` 32-bit values, 0x00RRGGBB (red in bits 16-23), whatever
/// the screen's own channel order is; the desktop converts when it paints.
pub fn win_create(width: u32, height: u32, title: &[u8]) -> Result<(u32, u64), i64> {
    let mut out = [0u64; 2];
    let r = unsafe {
        syscall(
            SYS_WIN_CREATE,
            width as u64,
            height as u64,
            title.as_ptr() as u64,
            title.len() as u64,
            out.as_mut_ptr() as u64,
        )
    };
    if r < 0 {
        Err(r)
    } else {
        Ok((out[0] as u32, out[1]))
    }
}

pub fn win_destroy(id: u32) -> i64 {
    unsafe { syscall(SYS_WIN_DESTROY, id as u64, 0, 0, 0, 0) }
}

/// The desktop only: fill `list` with the windows that exist; returns how many.
pub fn win_list(list: &mut [WinInfo]) -> i64 {
    unsafe { syscall(SYS_WIN_LIST, list.as_mut_ptr() as u64, list.len() as u64, 0, 0, 0) }
}

/// The desktop only: map window `id`'s pixels read-only; returns the address.
pub fn win_map(id: u32) -> i64 {
    unsafe { syscall(SYS_WIN_MAP, id as u64, 0, 0, 0, 0) }
}

/// The desktop only: send `event` (one of the EV_WIN_* kinds) to the window's owner.
pub fn win_post(id: u32, event: &Event) -> i64 {
    unsafe { syscall(SYS_WIN_POST, id as u64, event as *const Event as u64, 0, 0, 0) }
}

/// A window's owner: the next event for it. 1 = filled `event`, 0 = timed out.
pub fn win_event(id: u32, event: &mut Event, timeout_ticks: u64) -> i64 {
    unsafe { syscall(SYS_WIN_EVENT, id as u64, event as *mut Event as u64, timeout_ticks, 0, 0) }
}

/// 1 if reading `fd` (or writing, for a write end) would not make the caller
/// sleep, 0 if it would; negative on error. For watching two things at once.
pub fn fd_ready(fd: u64) -> i64 {
    unsafe { syscall(SYS_FD_READY, fd, 0, 0, 0, 0) }
}

/// A window's owner: tell the desktop the pixels changed.
pub fn win_flush(id: u32) -> i64 {
    unsafe { syscall(SYS_WIN_FLUSH, id as u64, 0, 0, 0, 0) }
}

pub const SYS_SPAWN2: u64 = 51;

/// Start the program at `path` (NUL-terminated). `argv` is `argc` NUL-terminated strings back to
/// back. `fds` lists `[child descriptor, our descriptor]` pairs: the child gets those of our
/// descriptors under those numbers (an empty list means our 0, 1 and 2). `env` is the child's
/// environment (NAME=value strings each ending in a NUL), or None to pass ours on. `pgid` is 0 to
/// stay in our process group, 1 to start a new one, or the id of a group to join.
/// Returns the new process id, or a negative error.
pub fn spawn_ex(path: &[u8], argv: &[u8], argc: usize, fds: &[[u64; 2]], env: Option<&[u8]>, pgid: u64) -> i64 {
    let req: [u64; 10] = [
        path.as_ptr() as u64,
        argv.as_ptr() as u64,
        argv.len() as u64,
        argc as u64,
        fds.as_ptr() as u64,
        fds.len() as u64,
        env.map_or(0, |e| e.as_ptr() as u64),
        env.map_or(0, |e| e.len() as u64),
        0,
        pgid,
    ];
    unsafe { syscall(SYS_SPAWN2, req.as_ptr() as u64, 0, 0, 0, 0) }
}

pub const SYS_WAIT: u64 = 53;
pub const WNOHANG: u64 = 1;

/// Wait for a child to finish: `pid` is a child's id or `ANY_CHILD`. Returns the child's id and
/// stores its exit status in `status`; with WNOHANG returns 0 if none has finished; ECHILD
/// (-10) if there is no such child.
pub fn wait(pid: u64, flags: u64, status: &mut i64) -> i64 {
    unsafe { syscall(SYS_WAIT, pid, flags, status as *mut i64 as u64, 0, 0) }
}

pub const ANY_CHILD: u64 = u64::MAX;

/// Send `sig` to every process in group `pgid`.
pub fn kill_group(pgid: u64, sig: u32) -> i64 {
    unsafe { syscall(SYS_KILL, (-(pgid as i64)) as u64, sig as u64, 0, 0, 0) }
}

// ---- signals and processes ---------------------------------------------------------------------

pub const SYS_KILL: u64 = 46;
pub const SYS_SIGIGNORE: u64 = 47;
pub const SYS_SETFG: u64 = 48;
pub const SYS_PROCLIST: u64 = 49;
pub const SYS_GETPPID: u64 = 50;

/// Send signal `sig` to process `pid`. pid 0 means the foreground process (where Control-C
/// goes). Signal 0 only checks that the process exists.
pub fn kill(pid: u64, sig: u32) -> i64 {
    unsafe { syscall(SYS_KILL, pid, sig as u64, 0, 0, 0) }
}

/// Ignore the signals whose bits are set in `mask` (bit n = signal n; SIGKILL cannot be ignored).
pub fn sigignore(mask: u32) {
    unsafe { syscall(SYS_SIGIGNORE, mask as u64, 0, 0, 0, 0) };
}

/// Make `pid` the foreground process: Control-C on the console signals it. 0 = nobody.
pub fn setfg(pid: u64) {
    unsafe { syscall(SYS_SETFG, pid, 0, 0, 0, 0) };
}

/// Fill `list` with the processes that exist; returns how many were stored.
pub fn proclist(list: &mut [ProcInfo]) -> i64 {
    unsafe { syscall(SYS_PROCLIST, list.as_mut_ptr() as u64, list.len() as u64, 0, 0, 0) }
}

pub fn getppid() -> u64 {
    unsafe { syscall(SYS_GETPPID, 0, 0, 0, 0, 0) as u64 }
}

// ---- networking ------------------------------------------------------------------------------

pub const SYS_SOCKET: u64 = 32;
pub const SYS_BIND: u64 = 33;
pub const SYS_SENDTO: u64 = 34;
pub const SYS_RECVFROM: u64 = 35;
pub const SYS_CONNECT: u64 = 36;
pub const SYS_NETINFO: u64 = 39;
pub const SYS_RESOLVE: u64 = 40;
pub const SYS_PING: u64 = 41;
pub const SYS_CLOCK_US: u64 = 42;

pub const SOCK_UDP: u64 = 1;
pub const SOCK_TCP: u64 = 2;
pub const SYS_LISTEN: u64 = 37;
pub const SYS_ACCEPT: u64 = 38;
pub const SYS_SHUTDOWN: u64 = 43;
pub const SYS_GETSOCKNAME: u64 = 44;
pub const SYS_GETPEERNAME: u64 = 45;

/// A new socket; returns its descriptor.
pub fn socket(kind: u64) -> i64 {
    unsafe { syscall(SYS_SOCKET, kind, 0, 0, 0, 0) }
}

/// Claim a local port (0 = any free one); returns the port.
pub fn bind(fd: u64, port: u16) -> i64 {
    unsafe { syscall(SYS_BIND, fd, port as u64, 0, 0, 0) }
}

/// Send one datagram to `to` (or to the connected peer if None); returns bytes sent.
pub fn sendto(fd: u64, data: &[u8], to: Option<&SockAddr>) -> i64 {
    let addr = to.map_or(0, |a| a as *const SockAddr as u64);
    unsafe { syscall(SYS_SENDTO, fd, data.as_ptr() as u64, data.len() as u64, addr, 0) }
}

/// Receive one datagram (cut to `buf`'s size). `timeout_ticks`: 0 only looks, FOREVER waits.
pub fn recvfrom(fd: u64, buf: &mut [u8], from: Option<&mut SockAddr>, timeout_ticks: u64) -> i64 {
    let addr = from.map_or(0, |a| a as *mut SockAddr as u64);
    unsafe { syscall(SYS_RECVFROM, fd, buf.as_mut_ptr() as u64, buf.len() as u64, addr, timeout_ticks) }
}

/// UDP: set the default peer. TCP: open the connection and wait for it (FOREVER).
pub fn connect(fd: u64, to: &SockAddr) -> i64 {
    connect_timeout(fd, to, FOREVER)
}

/// Like `connect`, but a TCP connection attempt gives up after `timeout_ticks`.
pub fn connect_timeout(fd: u64, to: &SockAddr, timeout_ticks: u64) -> i64 {
    unsafe { syscall(SYS_CONNECT, fd, to as *const SockAddr as u64, timeout_ticks, 0, 0) }
}

/// TCP: start accepting connections on the port given to `bind`.
pub fn listen(fd: u64, backlog: u64) -> i64 {
    unsafe { syscall(SYS_LISTEN, fd, backlog, 0, 0, 0) }
}

/// TCP: the next connection as a new descriptor; `peer` gets the client's address.
pub fn accept(fd: u64, peer: Option<&mut SockAddr>, timeout_ticks: u64) -> i64 {
    let addr = peer.map_or(0, |a| a as *mut SockAddr as u64);
    unsafe { syscall(SYS_ACCEPT, fd, addr, timeout_ticks, 0, 0) }
}

pub const SHUT_WR: u64 = 1;

/// TCP: stop sending; the other side reads end-of-file once it has everything.
pub fn shutdown(fd: u64, how: u64) -> i64 {
    unsafe { syscall(SYS_SHUTDOWN, fd, how, 0, 0, 0) }
}

pub fn getsockname(fd: u64, out: &mut SockAddr) -> i64 {
    unsafe { syscall(SYS_GETSOCKNAME, fd, out as *mut SockAddr as u64, 0, 0, 0) }
}

pub fn getpeername(fd: u64, out: &mut SockAddr) -> i64 {
    unsafe { syscall(SYS_GETPEERNAME, fd, out as *mut SockAddr as u64, 0, 0, 0) }
}

pub fn netinfo(out: &mut NetInfo) -> i64 {
    unsafe { syscall(SYS_NETINFO, out as *mut NetInfo as u64, 0, 0, 0, 0) }
}

/// Look up `name` (NUL-terminated; a dotted address is returned as is). `server` overrides
/// the resolver the network gave us.
pub fn resolve(name: &[u8], out: &mut [u8; 4], timeout_ticks: u64, server: Option<&SockAddr>) -> i64 {
    let srv = server.map_or(0, |s| s as *const SockAddr as u64);
    unsafe { syscall(SYS_RESOLVE, name.as_ptr() as u64, out.as_mut_ptr() as u64, timeout_ticks, srv, 0) }
}

/// Send one echo request; returns the round trip in microseconds, or a negative error.
pub fn ping(ip: [u8; 4], payload_len: u64, timeout_ticks: u64) -> i64 {
    unsafe { syscall(SYS_PING, u32::from_be_bytes(ip) as u64, payload_len, timeout_ticks, 0, 0) }
}

/// Microseconds since boot.
pub fn clock_us() -> u64 {
    unsafe { syscall(SYS_CLOCK_US, 0, 0, 0, 0, 0) as u64 }
}

pub fn strerror(code: i64) -> &'static str {
    match code {
        -1 => "Operation not permitted",
        -16 => "Device or resource busy",
        -19 => "No such device",
        -2 => "No such file or directory",
        -3 => "No such process",
        -4 => "Interrupted system call",
        -5 => "I/O error",
        -9 => "Bad file descriptor",
        -10 => "No child processes",
        -12 => "Out of memory",
        -14 => "Bad address",
        -17 => "File exists",
        -20 => "Not a directory",
        -21 => "Is a directory",
        -22 => "Invalid argument",
        -24 => "Too many open files",
        -27 => "File too large",
        -28 => "No space left on device",
        -29 => "Illegal seek",
        -32 => "Broken pipe",
        -34 => "Result too large",
        -36 => "File name too long",
        -38 => "Function not implemented",
        -39 => "Directory not empty",
        -117 => "Filesystem is corrupted",
        -11 => "Resource temporarily unavailable",
        -88 => "Not a socket",
        -89 => "Destination address required",
        -90 => "Message too long",
        -98 => "Address already in use",
        -99 => "Cannot assign requested address",
        -100 => "Network is down",
        -101 => "Network is unreachable",
        -104 => "Connection reset by peer",
        -107 => "Socket is not connected",
        -110 => "Connection timed out",
        -111 => "Connection refused",
        _ => "Unknown error",
    }
}

// ---- printing --------------------------------------------------------------------------

/// Buffers a formatted message so it leaves in one system call where possible.
struct Buffered {
    fd: u64,
    buf: [u8; 256],
    len: usize,
}

impl Buffered {
    fn flush(&mut self) {
        if self.len > 0 {
            write_all(self.fd, &self.buf[..self.len]);
            self.len = 0;
        }
    }
}

impl fmt::Write for Buffered {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &byte in s.as_bytes() {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = byte;
            self.len += 1;
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(fd: u64, args: fmt::Arguments) {
    use fmt::Write;
    let mut out = Buffered { fd, buf: [0; 256], len: 0 };
    let _ = out.write_fmt(args);
    out.flush();
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::_print($crate::STDOUT, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::_print($crate::STDOUT, format_args!("{}\n", format_args!($($arg)*))) };
}

#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => { $crate::_print($crate::STDERR, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! eprintln {
    () => { $crate::eprint!("\n") };
    ($($arg:tt)*) => { $crate::_print($crate::STDERR, format_args!("{}\n", format_args!($($arg)*))) };
}

// ---- arguments and entry point -----------------------------------------------------------

/// The command line: `argc` NUL-terminated strings stored back to back.
pub struct Args {
    argc: usize,
    argv: *const u8,
    env: *const u8,
    env_len: usize,
}

impl Args {
    #[doc(hidden)]
    pub fn new(argc: usize, argv: *const u8, env: *const u8, env_len: usize) -> Args {
        Args { argc, argv, env, env_len }
    }

    /// The environment as "NAME=value" strings.
    pub fn env(&self) -> impl Iterator<Item = &'static str> + '_ {
        let block: &'static [u8] = if self.env.is_null() || self.env_len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.env, self.env_len) }
        };
        block.split(|&b| b == 0).filter(|s| !s.is_empty()).filter_map(|s| core::str::from_utf8(s).ok())
    }

    /// The value of environment variable `name`.
    pub fn getenv(&self, name: &str) -> Option<&'static str> {
        self.env().find_map(|kv| kv.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')))
    }

    /// The whole environment block (strings back to back, each ending in a NUL).
    pub fn env_block(&self) -> &'static [u8] {
        if self.env.is_null() || self.env_len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.env, self.env_len) }
        }
    }

    pub fn len(&self) -> usize {
        self.argc
    }

    /// Argument `i` including its terminating NUL, for passing to system calls.
    pub fn cstr(&self, i: usize) -> &'static [u8] {
        if i >= self.argc || self.argv.is_null() {
            return b"\0";
        }
        unsafe {
            let mut p = self.argv;
            for _ in 0..i {
                while *p != 0 {
                    p = p.add(1);
                }
                p = p.add(1);
            }
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            core::slice::from_raw_parts(p, len + 1)
        }
    }

    /// Argument `i` as text ("" if missing or not valid UTF-8).
    pub fn get(&self, i: usize) -> &'static str {
        let c = self.cstr(i);
        core::str::from_utf8(&c[..c.len() - 1]).unwrap_or("")
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static str> + '_ {
        (0..self.argc).map(move |i| self.get(i))
    }
}

/// Defines the program's entry point: `entry!(main)` where
/// `fn main(args: &Args) -> i32`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        pub extern "C" fn _start(argc: usize, argv: *const u8, env: *const u8, env_len: usize) -> ! {
            let args = $crate::Args::new(argc, argv, env, env_len);
            let code: i32 = $main(&args);
            $crate::exit(code as i64)
        }
    };
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    eprintln!("panic: {}", info);
    exit(101)
}

// ---- small helpers for building NUL-terminated paths -------------------------------------

/// A fixed-size, NUL-terminated string buffer (there is no heap in user space).
pub struct CString<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> CString<N> {
    pub const fn new() -> Self {
        CString { buf: [0; N], len: 0 }
    }

    /// Append `s`; false if it does not fit (nothing is appended then).
    pub fn push_str(&mut self, s: &str) -> bool {
        self.push_bytes(s.as_bytes())
    }

    pub fn push_bytes(&mut self, s: &[u8]) -> bool {
        if self.len + s.len() + 1 > N {
            return false;
        }
        self.buf[self.len..self.len + s.len()].copy_from_slice(s);
        self.len += s.len();
        self.buf[self.len] = 0;
        true
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.buf[0] = 0;
    }

    /// A copy of `s`, cut off if it does not fit.
    pub fn from_str(s: &str) -> Self {
        let mut c = Self::new();
        let mut end = s.len().min(N.saturating_sub(1));
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        c.push_str(&s[..end]);
        c
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// The text followed by its NUL, ready for a system call.
    pub fn as_cstr(&self) -> &[u8] {
        &self.buf[..self.len + 1]
    }
}

/// Report `what: message` for a failed system call, as `program: what: message`.
pub fn report(program: &str, what: &str, code: i64) {
    eprintln!("{}: {}: {}", program, what, strerror(code));
}

/// `write!(string, "...")` into a CString; an error if the text does not fit.
impl<const N: usize> fmt::Write for CString<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.push_str(s) {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

// ---- the heap (opt-in: `nova = { ..., features = ["heap"] }` + `extern crate alloc;`) ----------

#[cfg(feature = "heap")]
mod heap_glue {
    use super::mem_alloc;
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use novaheap::{Heap, PageSource};

    /// Pages from the kernel (`mem_alloc`); they stay with the process until it exits.
    struct Pages;

    impl PageSource for Pages {
        fn get(&mut self, pages: usize) -> Option<*mut u8> {
            let r = mem_alloc(pages as u64);
            if r < 0 {
                None
            } else {
                Some(r as *mut u8)
            }
        }
    }

    struct Global(UnsafeCell<Heap<Pages>>);

    // A NovaOS process has a single thread, so nothing can race on the heap.
    unsafe impl Sync for Global {}

    unsafe impl GlobalAlloc for Global {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            (*self.0.get()).alloc(layout)
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            (*self.0.get()).dealloc(ptr, layout)
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            (*self.0.get()).realloc(ptr, layout, new_size)
        }
    }

    #[global_allocator]
    static ALLOCATOR: Global = Global(UnsafeCell::new(Heap::new(Pages)));

    /// Bytes currently allocated / obtained from the kernel, for programs that report them.
    pub fn heap_used() -> usize {
        unsafe { (*ALLOCATOR.0.get()).used() }
    }

    pub fn heap_total() -> usize {
        unsafe { (*ALLOCATOR.0.get()).total() }
    }
}

#[cfg(feature = "heap")]
pub use heap_glue::{heap_total, heap_used};
