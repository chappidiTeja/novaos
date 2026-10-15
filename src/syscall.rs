//! System calls via the `syscall` instruction.
//!
//! ABI: number in rax; arguments in rdi, rsi, rdx, r10, r8 (up to five); result in
//! rax (negative = error). rcx and r11 are clobbered by the instruction itself;
//! rdi, rsi, rdx, r10, r8 and r9 are clobbered by the kernel (they are zeroed on
//! return so no kernel values leak). All other registers are preserved.
//!
//! Numbers: 0 exit(code), 1 write(fd, ptr, len), 2 getpid(), 3 sleep(ticks),
//!          4 yield(), 5 ticks(), 6 read(fd, ptr, len), 7 pipe(fds_ptr),
//!          8 close(fd), 31 fd_ready(fd),
//!          9 open(path, flags), 10 lseek(fd, offset, whence), 11 fstat(fd, out),
//!          12 stat(path, out), 13 readdir(fd, out), 14 mkdir(path),
//!          15 unlink(path), 16 rmdir(path), 17 chdir(path), 18 getcwd(buf, len),
//!          19 spawn(path, argv, argv_len, argc, stdio), 20 waitpid(pid).
//!
//! `spawn` loads the ELF program at `path` and starts it as a new process, with
//! the caller's working directory. `argv` is `argc` NUL-terminated strings back
//! to back; the program's entry function is called with (argc, pointer to argv).
//! `stdio` is 0 (the child shares our stdin/stdout/stderr) or points to three
//! u64 descriptor numbers of this process to use as the child's 0, 1 and 2.
//! It returns the child's process id; `waitpid` waits for it and returns its exit
//! status (negative for a process killed by a CPU exception).
//!
//! Paths are NUL-terminated strings, at most 255 bytes, absolute or relative to
//! the process's working directory. `read` and `write` may sleep (empty/full
//! pipe, no console input, disk I/O). `pipe` stores the new read and write
//! descriptors as two u64 at `fds_ptr`.
//!
//! `fstat`/`stat` fill 5 u64: inode, kind (1 file, 2 directory), size, link count,
//! blocks. `readdir` fills one 88-byte entry: inode, kind, name length (3 u64),
//! then the name in 64 bytes; it returns 1 for an entry and 0 at the end.

use crate::gdt::KERNEL_CS;
use crate::ipc::console;
use crate::ipc::fd::File;
use crate::ipc::pipe::{self, BrokenPipe};
use crate::mem::vmm;
use crate::vfs;
use crate::{proc, sched, serial, timer};
// Screen, input and window calls live in their own file but share this module's helpers.
#[path = "syscall_gfx.rs"]
mod gfx;
// Sockets, ping, name lookup, clock.
#[path = "syscall_net.rs"]
mod netcalls;

use alloc::string::String;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use novafs::{FileKind, Stat};

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_GETPID: u64 = 2;
const SYS_SLEEP: u64 = 3;
const SYS_YIELD: u64 = 4;
const SYS_TICKS: u64 = 5;
const SYS_READ: u64 = 6;
const SYS_PIPE: u64 = 7;
const SYS_CLOSE: u64 = 8;
const SYS_OPEN: u64 = 9;
const SYS_LSEEK: u64 = 10;
const SYS_FSTAT: u64 = 11;
const SYS_STAT: u64 = 12;
const SYS_READDIR: u64 = 13;
const SYS_MKDIR: u64 = 14;
const SYS_UNLINK: u64 = 15;
const SYS_RMDIR: u64 = 16;
const SYS_CHDIR: u64 = 17;
const SYS_GETCWD: u64 = 18;
const SYS_SPAWN: u64 = 19;
const SYS_WAITPID: u64 = 20;
const SYS_GFX_CLAIM: u64 = 21;
const SYS_MEM_ALLOC: u64 = 22;
const SYS_INPUT_NEXT: u64 = 23;
const SYS_WIN_CREATE: u64 = 24;
const SYS_WIN_DESTROY: u64 = 25;
const SYS_WIN_LIST: u64 = 26;
const SYS_WIN_MAP: u64 = 27;
const SYS_WIN_POST: u64 = 28;
const SYS_WIN_EVENT: u64 = 29;
const SYS_WIN_FLUSH: u64 = 30;
const SYS_FD_READY: u64 = 31;
const SYS_SOCKET: u64 = 32;
const SYS_BIND: u64 = 33;
const SYS_SENDTO: u64 = 34;
const SYS_RECVFROM: u64 = 35;
const SYS_CONNECT: u64 = 36;
const SYS_NETINFO: u64 = 39;
const SYS_RESOLVE: u64 = 40;
const SYS_PING: u64 = 41;
const SYS_CLOCK_US: u64 = 42;
const SYS_SHUTDOWN: u64 = 43;
const SYS_GETSOCKNAME: u64 = 44;
const SYS_GETPEERNAME: u64 = 45;
const SYS_LISTEN: u64 = 37;
const SYS_KILL: u64 = 46;
const SYS_SIGIGNORE: u64 = 47;
const SYS_SETFG: u64 = 48;
const SYS_PROCLIST: u64 = 49;
const SYS_GETPPID: u64 = 50;
const SYS_SPAWN2: u64 = 51;
const SYS_WAIT: u64 = 53;
const SYS_ACCEPT: u64 = 38;

const E2BIG: i64 = -7;
const ENOEXEC: i64 = -8;
const ECHILD: i64 = -10;
const ENOMEM: i64 = -12;

const EBADF: i64 = -9;
const EFAULT: i64 = -14;
const EMFILE: i64 = -24;
const EINVAL: i64 = -22;
const EPIPE: i64 = -32;
const ENOSYS: i64 = -38;

/// Longest path accepted from user space.
const PATH_MAX: usize = 255;

/// Largest single read or write we are willing to buffer in the kernel.
const MAX_IO: u64 = 65536;
const MAX_SLEEP_TICKS: u64 = 1000;

const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const EFER_SCE: u64 = 1;
/// RFLAGS bits cleared on entry: TF, IF, DF.
const SFMASK: u64 = 0x100 | 0x200 | 0x400;

// Single CPU: one slot for the kernel stack to switch to, one scratch slot for
// the user's rsp. Both are only touched with interrupts off.
static mut KERNEL_RSP: u64 = 0;
static mut USER_RSP_SCRATCH: u64 = 0;

extern "C" {
    fn syscall_entry();
}

global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    // Interrupts are masked (SFMASK) until we are on the kernel stack.
    "mov [rip + {user_rsp}], rsp",
    "mov rsp, [rip + {kernel_rsp}]",
    "push qword ptr [rip + {user_rsp}]",
    "push rcx", // user rip
    "push r11", // user rflags
    "push rbp",
    // Move (nr, a1..a5) from the syscall convention to the C convention.
    "mov r9, r8",
    "mov r8, r10",
    "mov rcx, rdx",
    "mov rdx, rsi",
    "mov rsi, rdi",
    "mov rdi, rax",
    "sti",
    "call {dispatch}",
    "cli",
    "pop rbp",
    "pop r11",
    "pop rcx",
    // Do not leak kernel values in argument registers.
    "xor edi, edi",
    "xor esi, esi",
    "xor edx, edx",
    "xor r8d, r8d",
    "xor r9d, r9d",
    "xor r10d, r10d",
    "pop rsp", // back onto the user stack
    "sysretq",
    user_rsp = sym USER_RSP_SCRATCH,
    kernel_rsp = sym KERNEL_RSP,
    dispatch = sym dispatch,
);

unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
         options(nomem, nostack, preserves_flags));
    ((hi as u64) << 32) | lo as u64
}

unsafe fn wrmsr(msr: u32, value: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
         options(nomem, nostack, preserves_flags));
}

pub fn init() {
    unsafe {
        wrmsr(MSR_EFER, rdmsr(MSR_EFER) | EFER_SCE);
        // SYSCALL loads CS from STAR[47:32] (SS = that + 8); SYSRET loads
        // SS from STAR[63:48] + 8 and CS from STAR[63:48] + 16.
        wrmsr(MSR_STAR, (0x10u64 << 48) | ((KERNEL_CS as u64) << 32));
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
        wrmsr(MSR_SFMASK, SFMASK);
    }
}

/// Tell the entry stub which kernel stack to use. Called on every context
/// switch with the incoming thread's stack top.
pub fn set_kernel_stack(top: u64) {
    unsafe { core::ptr::addr_of_mut!(KERNEL_RSP).write_volatile(top) };
}

/// Every system call goes through here: run it, then let a signal that was sent to the caller
/// (during the call, or while it slept in it) end the process.
extern "C" fn dispatch(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let result = dispatch_inner(nr, a1, a2, a3, a4, a5);
    proc::act_on_signals();
    result
}

fn dispatch_inner(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    match nr {
        SYS_SPAWN => sys_spawn(a1, a2, a3, a4, a5),
        SYS_WAITPID => sys_waitpid(a1),
        SYS_GFX_CLAIM => gfx::sys_gfx_claim(a1),
        SYS_MEM_ALLOC => gfx::sys_mem_alloc(a1),
        SYS_INPUT_NEXT => gfx::sys_input_next(a1, a2),
        SYS_WIN_CREATE => gfx::sys_win_create(a1, a2, a3, a4, a5),
        SYS_WIN_DESTROY => gfx::sys_win_destroy(a1),
        SYS_WIN_LIST => gfx::sys_win_list(a1, a2),
        SYS_WIN_MAP => gfx::sys_win_map(a1),
        SYS_WIN_POST => gfx::sys_win_post(a1, a2),
        SYS_WIN_EVENT => gfx::sys_win_event(a1, a2, a3),
        SYS_WIN_FLUSH => gfx::sys_win_flush(a1),
        SYS_FD_READY => sys_fd_ready(a1),
        SYS_SOCKET => netcalls::sys_socket(a1),
        SYS_BIND => netcalls::sys_bind(a1, a2),
        SYS_SENDTO => netcalls::sys_sendto(a1, a2, a3, a4),
        SYS_RECVFROM => netcalls::sys_recvfrom(a1, a2, a3, a4, a5),
        SYS_CONNECT => netcalls::sys_connect(a1, a2, a3),
        SYS_LISTEN => netcalls::sys_listen(a1, a2),
        SYS_ACCEPT => netcalls::sys_accept(a1, a2, a3),
        SYS_SHUTDOWN => netcalls::sys_shutdown(a1, a2),
        SYS_KILL => sys_kill(a1, a2),
        SYS_SIGIGNORE => {
            proc::set_ignored(a1 as u32);
            0
        }
        SYS_SETFG => {
            console::set_foreground(a1);
            0
        }
        SYS_PROCLIST => sys_proclist(a1, a2),
        SYS_GETPPID => sched::current_user_handle().map_or(0, |h| h.ppid as i64),
        SYS_SPAWN2 => sys_spawn2(a1),
        SYS_WAIT => sys_wait(a1, a2, a3),
        SYS_GETSOCKNAME => netcalls::sys_getsockname(a1, a2),
        SYS_GETPEERNAME => netcalls::sys_getpeername(a1, a2),
        SYS_NETINFO => netcalls::sys_netinfo(a1),
        SYS_RESOLVE => netcalls::sys_resolve(a1, a2, a3, a4),
        SYS_PING => netcalls::sys_ping(a1, a2, a3),
        SYS_CLOCK_US => netcalls::sys_clock_us(),
        SYS_EXIT => proc::exit_current(a1 as i64),
        SYS_WRITE => sys_write(a1, a2, a3),
        SYS_READ => sys_read(a1, a2, a3),
        SYS_PIPE => sys_pipe(a1),
        SYS_CLOSE => sys_close(a1),
        SYS_OPEN => sys_open(a1, a2),
        SYS_LSEEK => sys_lseek(a1, a2 as i64, a3),
        SYS_FSTAT => sys_fstat(a1, a2),
        SYS_STAT => sys_stat(a1, a2),
        SYS_READDIR => sys_readdir(a1, a2),
        SYS_MKDIR => sys_path_op(a1, vfs::mkdir),
        SYS_UNLINK => sys_path_op(a1, vfs::unlink),
        SYS_RMDIR => sys_path_op(a1, vfs::rmdir),
        SYS_CHDIR => sys_chdir(a1),
        SYS_GETCWD => sys_getcwd(a1, a2),
        SYS_GETPID => proc::current_pid() as i64,
        SYS_SLEEP => {
            sched::sleep_ticks(a1.min(MAX_SLEEP_TICKS));
            0
        }
        SYS_YIELD => {
            sched::yield_now();
            0
        }
        SYS_TICKS => timer::ticks() as i64,
        _ => ENOSYS,
    }
}

fn lookup_fd(fd: u64) -> Option<alloc::sync::Arc<File>> {
    // The descriptor table is cloned out of the scheduler's data and locked
    // only briefly: reading or writing may sleep, and must not hold either.
    proc::current_fds()?.lock().get(fd)
}

fn sys_write(fd: u64, ptr: u64, len: u64) -> i64 {
    if len > MAX_IO {
        return EINVAL;
    }
    let file = match lookup_fd(fd) {
        Some(file) => file,
        None => return EBADF,
    };
    let bytes = match copy_from_user(ptr, len) {
        Ok(bytes) => bytes,
        Err(e) => return e,
    };
    match &*file {
        File::Console => {
            serial::write_bytes(&bytes);
            bytes.len() as i64
        }
        File::PipeWrite(pipe) => match pipe.write(&bytes) {
            Ok(n) => n as i64,
            Err(BrokenPipe) => EPIPE,
        },
        File::PipeRead(_) => EBADF,
        File::Disk(file) => match file.write(&bytes) {
            Ok(n) => n as i64,
            Err(e) => e,
        },
        File::Tcp(sock) => match sock.send(&bytes) {
            Ok(n) => n as i64,
            Err(e) => e,
        },
        File::Udp(sock) => match sock.send_to(&bytes, None) {
            Ok(n) => n as i64,
            Err(e) => e,
        },
    }
}

fn sys_read(fd: u64, ptr: u64, len: u64) -> i64 {
    if len > MAX_IO {
        return EINVAL;
    }
    let file = match lookup_fd(fd) {
        Some(file) => file,
        None => return EBADF,
    };
    // Check the destination before reading: input consumed and then lost to a
    // bad pointer could never be recovered.
    if let Err(e) = walk_user(ptr, len, true, |_, _| {}) {
        return e;
    }
    if len == 0 {
        return 0;
    }
    let mut buf = alloc::vec![0u8; len as usize];
    let n = match &*file {
        File::Console => console::read(&mut buf),
        File::PipeRead(pipe) => pipe.read(&mut buf),
        File::PipeWrite(_) => return EBADF,
        File::Disk(file) => match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => return e,
        },
        File::Tcp(sock) => match sock.recv(buf.len(), None) {
            Ok(data) => {
                buf[..data.len()].copy_from_slice(&data);
                data.len()
            }
            Err(e) => return e,
        },
        File::Udp(sock) => match sock.recv_from(buf.len(), None) {
            Ok((data, _, _)) => {
                buf[..data.len()].copy_from_slice(&data);
                data.len()
            }
            Err(e) => return e,
        },
    };
    match copy_to_user(ptr, &buf[..n]) {
        Ok(()) => n as i64,
        Err(e) => e,
    }
}

// ---- files and directories --------------------------------------------------------

/// A NUL-terminated string from user space.
fn copy_cstr_from_user(ptr: u64, max: usize) -> Result<String, i64> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut addr = ptr;
    while bytes.len() <= max {
        let in_page = (0x1000 - (addr & 0xFFF)) as usize;
        let chunk = in_page.min(max + 1 - bytes.len());
        let mut terminated = false;
        walk_user(addr, chunk as u64, false, |src, n| {
            let piece = unsafe { core::slice::from_raw_parts(src, n) };
            match piece.iter().position(|&b| b == 0) {
                Some(end) => {
                    bytes.extend_from_slice(&piece[..end]);
                    terminated = true;
                }
                None => bytes.extend_from_slice(piece),
            }
        })?;
        if terminated {
            return String::from_utf8(bytes).map_err(|_| EINVAL);
        }
        addr += chunk as u64;
    }
    Err(vfs::ENAMETOOLONG)
}

fn user_path(ptr: u64) -> Result<String, i64> {
    copy_cstr_from_user(ptr, PATH_MAX)
}

fn cwd_ino() -> u32 {
    proc::current_cwd().map_or(novafs::ROOT_INO, |c| c.lock().ino)
}

fn sys_open(path_ptr: u64, flags: u64) -> i64 {
    let path = match user_path(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let file = match vfs::open(&path, flags, cwd_ino()) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let fds = match proc::current_fds() {
        Some(fds) => fds,
        None => return EBADF,
    };
    let installed = fds.lock().install(File::Disk(file));
    match installed {
        Some(fd) => fd as i64,
        None => EMFILE, // the closure moved `file` in and dropped it: its release is queued
    }
}

fn sys_lseek(fd: u64, offset: i64, whence: u64) -> i64 {
    match lookup_fd(fd).as_deref() {
        Some(File::Disk(file)) => match file.seek(offset, whence) {
            Ok(pos) => pos as i64,
            Err(e) => e,
        },
        Some(_) => -29, // ESPIPE: pipes and the console cannot seek
        None => EBADF,
    }
}

fn kind_code(kind: FileKind) -> u64 {
    match kind {
        FileKind::File => 1,
        FileKind::Dir => 2,
    }
}

fn store_stat(out_ptr: u64, stat: &Stat) -> i64 {
    let mut raw = [0u8; 40];
    for (i, value) in [
        stat.ino as u64,
        kind_code(stat.kind),
        stat.size,
        stat.nlink as u64,
        stat.blocks as u64,
    ]
    .iter()
    .enumerate()
    {
        raw[8 * i..8 * i + 8].copy_from_slice(&value.to_le_bytes());
    }
    match copy_to_user(out_ptr, &raw) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

fn sys_fstat(fd: u64, out_ptr: u64) -> i64 {
    match lookup_fd(fd).as_deref() {
        Some(File::Disk(file)) => match file.stat() {
            Ok(stat) => store_stat(out_ptr, &stat),
            Err(e) => e,
        },
        Some(_) => {
            // Not a disk file: report an empty regular file.
            let stat = Stat { ino: 0, kind: FileKind::File, size: 0, nlink: 1, blocks: 0 };
            store_stat(out_ptr, &stat)
        }
        None => EBADF,
    }
}

fn sys_stat(path_ptr: u64, out_ptr: u64) -> i64 {
    let path = match user_path(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match vfs::stat_path(&path, cwd_ino()) {
        Ok(stat) => store_stat(out_ptr, &stat),
        Err(e) => e,
    }
}

fn sys_readdir(fd: u64, out_ptr: u64) -> i64 {
    let file = match lookup_fd(fd) {
        Some(file) => file,
        None => return EBADF,
    };
    let File::Disk(dir) = &*file else {
        return vfs::ENOTDIR;
    };
    if let Err(e) = walk_user(out_ptr, 88, true, |_, _| {}) {
        return e;
    }
    match dir.next_entry() {
        Err(e) => e,
        Ok(None) => 0,
        Ok(Some(entry)) => {
            let mut raw = [0u8; 88];
            raw[0..8].copy_from_slice(&(entry.ino as u64).to_le_bytes());
            raw[8..16].copy_from_slice(&kind_code(entry.kind).to_le_bytes());
            raw[16..24].copy_from_slice(&(entry.name().len() as u64).to_le_bytes());
            raw[24..24 + entry.name().len()].copy_from_slice(entry.name());
            match copy_to_user(out_ptr, &raw) {
                Ok(()) => 1,
                Err(e) => e,
            }
        }
    }
}

/// mkdir, unlink and rmdir all take one path.
fn sys_path_op(path_ptr: u64, op: fn(&str, u32) -> Result<(), i64>) -> i64 {
    let path = match user_path(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match op(&path, cwd_ino()) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// spawn(path, argv, argv_len, argc, stdio): the child gets our descriptors `stdio[0..3]` as its
/// 0, 1 and 2 (or our own 0, 1, 2 if `stdio` is 0), our working directory and our environment.
fn sys_spawn(path_ptr: u64, argv_ptr: u64, argv_len: u64, argc: u64, stdio_ptr: u64) -> i64 {
    let mapping: [(usize, u64); 3] = if stdio_ptr == 0 {
        [(0, 0), (1, 1), (2, 2)]
    } else {
        match copy_from_user(stdio_ptr, 24) {
            Ok(raw) => {
                let word = |i: usize| u64::from_le_bytes(raw[8 * i..8 * i + 8].try_into().unwrap());
                [(0, word(0)), (1, word(1)), (2, word(2))]
            }
            Err(e) => return e,
        }
    };
    spawn_common(path_ptr, argv_ptr, argv_len, argc, &mapping, None, 0)
}

/// spawn2(req): like spawn, with more control. `req` points to ten u64: path, argv, argv_len,
/// argc, fds, nfds, env, env_len, flags, pgid. `fds` is `nfds` pairs (child descriptor, our
/// descriptor); with nfds = 0 the child gets our 0, 1 and 2. `env` is 0 (inherit ours) or a
/// block of NAME=value strings each ending in a NUL. No flags are defined yet. `pgid` is 0 (join our
/// process group), 1 (start a new group) or an existing group to join.
fn sys_spawn2(req_ptr: u64) -> i64 {
    let raw = match copy_from_user(req_ptr, 80) {
        Ok(raw) => raw,
        Err(e) => return e,
    };
    let word = |i: usize| u64::from_le_bytes(raw[8 * i..8 * i + 8].try_into().unwrap());
    let (path, argv, argv_len, argc) = (word(0), word(1), word(2), word(3));
    let (fds_ptr, nfds, env_ptr, env_len, flags, pgid) = (word(4), word(5), word(6), word(7), word(8), word(9));
    if flags != 0 {
        return EINVAL;
    }
    let mut mapping = alloc::vec::Vec::new();
    if nfds == 0 {
        mapping.extend_from_slice(&[(0usize, 0u64), (1, 1), (2, 2)]);
    } else {
        if nfds > 16 {
            return EINVAL;
        }
        let pairs = match copy_from_user(fds_ptr, nfds * 16) {
            Ok(p) => p,
            Err(e) => return e,
        };
        for i in 0..nfds as usize {
            let child = u64::from_le_bytes(pairs[16 * i..16 * i + 8].try_into().unwrap());
            let parent = u64::from_le_bytes(pairs[16 * i + 8..16 * i + 16].try_into().unwrap());
            if child >= crate::ipc::fd::MAX_FDS as u64 {
                return EBADF;
            }
            mapping.push((child as usize, parent));
        }
    }
    let env = if env_ptr == 0 {
        None
    } else {
        if env_len as usize > proc::MAX_ENV_BYTES {
            return E2BIG;
        }
        match copy_from_user(env_ptr, env_len) {
            Ok(e) => Some(e),
            Err(e) => return e,
        }
    };
    spawn_common(path, argv, argv_len, argc, &mapping, env, pgid)
}

fn spawn_common(
    path_ptr: u64,
    argv_ptr: u64,
    argv_len: u64,
    argc: u64,
    mapping: &[(usize, u64)],
    env: Option<alloc::vec::Vec<u8>>,
    pgid: u64,
) -> i64 {
    let path = match user_path(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if argv_len as usize > proc::MAX_ARGV_BYTES || argc > 64 {
        return E2BIG;
    }
    let argv = match copy_from_user(argv_ptr, argv_len) {
        Ok(bytes) => bytes,
        Err(e) => return e,
    };
    // The block must hold exactly `argc` complete strings.
    if argv.iter().filter(|&&b| b == 0).count() != argc as usize
        || (argc > 0 && argv.last() != Some(&0))
    {
        return EINVAL;
    }
    if let Some(env) = &env {
        if !env.is_empty() && env.last() != Some(&0) {
            return EINVAL;
        }
    }

    // Which of our descriptors the child gets, and under which numbers.
    let parent_fds = match proc::current_fds() {
        Some(fds) => fds,
        None => return EBADF,
    };
    let mut child_fds = crate::ipc::fd::FdTable::empty();
    {
        let table = parent_fds.lock();
        for &(child, parent) in mapping {
            match table.get(parent) {
                Some(file) => child_fds.share(child, file),
                None => return EBADF,
            }
        }
    }

    let cwd = match proc::current_cwd() {
        Some(cwd) => cwd.lock().clone(),
        None => return EBADF,
    };
    // Our environment, unless the caller gave the child one.
    let env = match env {
        Some(e) => e,
        None => sched::with_current_user(|u| u.env.to_vec()).unwrap_or_default(),
    };
    let image = match vfs::read_file(&path, cwd.ino, 4 << 20) {
        Ok(image) => image,
        Err(e) => return e,
    };
    let name = path.rsplit('/').next().unwrap_or("proc");
    match proc::spawn_user_env(name, &image, 0, child_fds, cwd, &argv, argc as usize, &env, pgid) {
        Ok(handle) => {
            let pid = handle.pid as i64;
            proc::register_child(handle);
            pid
        }
        Err(proc::SpawnError::OutOfMemory) => ENOMEM,
        Err(proc::SpawnError::Elf(_)) => ENOEXEC,
        Err(proc::SpawnError::BadArguments) => E2BIG,
    }
}

fn sys_waitpid(pid: u64) -> i64 {
    match proc::wait_child(Some(pid), false) {
        Ok(Some((_, status))) => status,
        Ok(None) => 0, // interrupted by a signal that ends us; the value is never seen
        Err(e) => e,
    }
}

/// kill(pid, sig): a positive pid is one process; a negative one (as a signed number) is a whole
/// process group; 0 means the foreground group (the one Control-C goes to).
fn sys_kill(pid: u64, sig: u64) -> i64 {
    let Ok(sig) = u32::try_from(sig) else { return EINVAL };
    let result = match pid as i64 {
        0 => match console::foreground() {
            0 => Err(proc::ESRCH),
            group => proc::send_signal_group(group, sig),
        },
        p if p < 0 => proc::send_signal_group(p.unsigned_abs(), sig),
        p => proc::send_signal(p as u64, sig),
    };
    match result {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// wait(pid, flags, out): wait for a child to finish. `pid` is a child's id, or u64::MAX for any
/// child; flag 1 (WNOHANG) returns 0 at once if none has finished. Stores the exit status as an i64
/// at `out` (if not 0) and returns the child's id; ECHILD if there is nothing to wait for.
fn sys_wait(pid: u64, flags: u64, out: u64) -> i64 {
    if flags & !1 != 0 {
        return EINVAL;
    }
    if out != 0 {
        if let Err(e) = walk_user(out, 8, true, |_, _| {}) {
            return e;
        }
    }
    let want = if pid == u64::MAX { None } else { Some(pid) };
    match proc::wait_child(want, flags & 1 != 0) {
        Ok(Some((child, status))) => {
            if out != 0 {
                let _ = copy_to_user(out, &status.to_le_bytes());
            }
            child as i64
        }
        Ok(None) => 0,
        Err(e) => e,
    }
}

/// proclist(buf, max): fill up to `max` 64-byte records, return how many. Layout:
/// pid u32, ppid u32, state u32 (0 running or ready, 1 waiting, 2 finished but not waited for),
/// pending-signals u32, start tick u64, cpu ticks u64, memory pages u64, exit status i64, name[16].
fn sys_proclist(buf: u64, max: u64) -> i64 {
    if max == 0 || max > 256 {
        return EINVAL;
    }
    let procs = proc::all_processes();
    let n = procs.len().min(max as usize);
    let mut raw = alloc::vec![0u8; n * 64];
    for (i, p) in procs.iter().take(n).enumerate() {
        let rec = &mut raw[i * 64..(i + 1) * 64];
        let state: u32 = if p.is_done() {
            2
        } else {
            let tid = p.thread.load(core::sync::atomic::Ordering::Acquire);
            (tid != usize::MAX && sched::is_waiting(tid)) as u32
        };
        rec[0..4].copy_from_slice(&(p.pid as u32).to_le_bytes());
        rec[4..8].copy_from_slice(&(p.ppid as u32).to_le_bytes());
        rec[8..12].copy_from_slice(&state.to_le_bytes());
        rec[12..16].copy_from_slice(&p.pending.load(core::sync::atomic::Ordering::Relaxed).to_le_bytes());
        rec[16..24].copy_from_slice(&p.start_tick.to_le_bytes());
        rec[24..32].copy_from_slice(&p.cpu_ticks.load(core::sync::atomic::Ordering::Relaxed).to_le_bytes());
        let pages = p.mem.upgrade().map_or(0, |m| m.pages());
        rec[32..40].copy_from_slice(&pages.to_le_bytes());
        rec[40..48].copy_from_slice(&p.exit_code().unwrap_or(0).to_le_bytes());
        let name = p.name.as_bytes();
        rec[48..48 + name.len().min(16)].copy_from_slice(&name[..name.len().min(16)]);
    }
    match copy_to_user(buf, &raw) {
        Ok(()) => n as i64,
        Err(e) => e,
    }
}

fn sys_chdir(path_ptr: u64) -> i64 {
    let path = match user_path(path_ptr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let cwd = match proc::current_cwd() {
        Some(cwd) => cwd,
        None => return EBADF,
    };
    let (current_ino, current_path) = {
        let guard = cwd.lock();
        (guard.ino, guard.path.clone())
    };
    let ino = match vfs::resolve_dir(&path, current_ino) {
        Ok(ino) => ino,
        Err(e) => return e,
    };
    let mut guard = cwd.lock();
    guard.ino = ino;
    guard.path = vfs::normalize(&current_path, &path);
    0
}

fn sys_getcwd(buf_ptr: u64, len: u64) -> i64 {
    let path = match proc::current_cwd() {
        Some(cwd) => cwd.lock().path.clone(),
        None => return EBADF,
    };
    let needed = path.len() as u64 + 1;
    if needed > len {
        return vfs::ERANGE;
    }
    let mut raw = path.into_bytes();
    raw.push(0);
    match copy_to_user(buf_ptr, &raw) {
        Ok(()) => needed as i64,
        Err(e) => e,
    }
}

fn sys_pipe(fds_ptr: u64) -> i64 {
    if let Err(e) = walk_user(fds_ptr, 16, true, |_, _| {}) {
        return e;
    }
    let (read_end, write_end) = pipe::pipe();
    let fds = match proc::current_fds() {
        Some(fds) => fds,
        None => return EBADF,
    };
    let (read_fd, write_fd) = {
        let mut table = fds.lock();
        let read_fd = match table.install(File::PipeRead(read_end)) {
            Some(fd) => fd,
            None => return EMFILE,
        };
        match table.install(File::PipeWrite(write_end)) {
            Some(write_fd) => (read_fd, write_fd),
            None => {
                let undone = table.take(read_fd as u64);
                drop(table);
                drop(undone);
                return EMFILE;
            }
        }
    };
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&(read_fd as u64).to_le_bytes());
    out[8..].copy_from_slice(&(write_fd as u64).to_le_bytes());
    match copy_to_user(fds_ptr, &out) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// 31: fd_ready(fd). 1 if reading (or, for a write end, writing) would not make
/// the caller sleep, 0 if it would. A program that must watch two things at once
/// (the terminal window: its own events and the shell's output) polls with this.
fn sys_fd_ready(fd: u64) -> i64 {
    let file = match lookup_fd(fd) {
        Some(file) => file,
        None => return EBADF,
    };
    let ready = match &*file {
        File::Console => console::input_ready(),
        File::PipeRead(pipe) => pipe.ready(),
        File::PipeWrite(pipe) => pipe.ready(),
        File::Disk(_) => true,
        File::Udp(sock) => sock.ready(),
        File::Tcp(sock) => sock.ready(),
    };
    ready as i64
}

fn sys_close(fd: u64) -> i64 {
    let fds = match proc::current_fds() {
        Some(fds) => fds,
        None => return EBADF,
    };
    // Drop the file after the table lock is released: closing a pipe end wakes
    // other threads.
    let file = fds.lock().take(fd);
    match file {
        Some(file) => {
            drop(file);
            // A disk file that was unlinked while open is freed by this.
            vfs::flush_pending();
            0
        }
        None => EBADF,
    }
}

/// Visit each page of the user range [ptr, ptr+len) in the caller's address
/// space, calling `f(kernel pointer, chunk length)`. Fails with EFAULT unless
/// every page is mapped and accessible from ring 3 (and writable, if `write`).
/// Pages are reached through the direct map, so a bad pointer is an error
/// return, never a kernel fault.
fn walk_user(
    ptr: u64,
    len: u64,
    write: bool,
    mut f: impl FnMut(*mut u8, usize),
) -> Result<(), i64> {
    let end = ptr.checked_add(len).ok_or(EFAULT)?;
    if end > vmm::USER_SPACE_END {
        return Err(EFAULT);
    }
    let pml4 = vmm::read_cr3();
    let mut addr = ptr;
    while addr < end {
        let page = vmm::translate_in(pml4, addr).ok_or(EFAULT)?;
        if !page.user || (write && !page.writable) {
            return Err(EFAULT);
        }
        let chunk = (((addr & !0xFFF) + 0x1000).min(end) - addr) as usize;
        f(crate::mem::phys_to_virt(page.phys), chunk);
        addr += chunk as u64;
    }
    Ok(())
}

fn copy_from_user(ptr: u64, len: u64) -> Result<Vec<u8>, i64> {
    let mut out = Vec::with_capacity(len as usize);
    walk_user(ptr, len, false, |src, chunk| {
        out.extend_from_slice(unsafe { core::slice::from_raw_parts(src, chunk) });
    })?;
    Ok(out)
}

fn copy_to_user(ptr: u64, data: &[u8]) -> Result<(), i64> {
    // Validate everything first so a failure never leaves a half-written buffer.
    walk_user(ptr, data.len() as u64, true, |_, _| {})?;
    let mut copied = 0;
    walk_user(ptr, data.len() as u64, true, |dst, chunk| {
        unsafe { core::ptr::copy_nonoverlapping(data[copied..].as_ptr(), dst, chunk) };
        copied += chunk;
    })
}
