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

pub fn strerror(code: i64) -> &'static str {
    match code {
        -2 => "No such file or directory",
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
}

impl Args {
    #[doc(hidden)]
    pub fn new(argc: usize, argv: *const u8) -> Args {
        Args { argc, argv }
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
        pub extern "C" fn _start(argc: usize, argv: *const u8) -> ! {
            let args = $crate::Args::new(argc, argv);
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
