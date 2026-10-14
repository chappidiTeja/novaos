//! System calls via the `syscall` instruction.
//!
//! ABI: number in rax; arguments in rdi, rsi, rdx, r10, r8 (up to five); result in
//! rax (negative = error). rcx and r11 are clobbered by the instruction itself;
//! rdi, rsi, rdx, r10, r8 and r9 are clobbered by the kernel (they are zeroed on
//! return so no kernel values leak). All other registers are preserved.
//!
//! Numbers: 0 exit(code), 1 write(fd, ptr, len), 2 getpid(), 3 sleep(ticks),
//!          4 yield(), 5 ticks(), 6 read(fd, ptr, len), 7 pipe(fds_ptr),
//!          8 close(fd).
//!
//! `read` and `write` may sleep (empty/full pipe, no console input). `pipe`
//! stores the new read and write descriptors as two u64 at `fds_ptr`.

use crate::gdt::KERNEL_CS;
use crate::ipc::fd::File;
use crate::ipc::pipe::{self, BrokenPipe};
use crate::ipc::console;
use crate::mem::vmm;
use crate::{proc, sched, serial, timer};
use alloc::vec::Vec;
use core::arch::{asm, global_asm};

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_GETPID: u64 = 2;
const SYS_SLEEP: u64 = 3;
const SYS_YIELD: u64 = 4;
const SYS_TICKS: u64 = 5;
const SYS_READ: u64 = 6;
const SYS_PIPE: u64 = 7;
const SYS_CLOSE: u64 = 8;

const EBADF: i64 = -9;
const EFAULT: i64 = -14;
const EMFILE: i64 = -24;
const EINVAL: i64 = -22;
const EPIPE: i64 = -32;
const ENOSYS: i64 = -38;

/// Largest single read or write we are willing to buffer in the kernel.
const MAX_IO: u64 = 4096;
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

extern "C" fn dispatch(nr: u64, a1: u64, a2: u64, a3: u64, _a4: u64, _a5: u64) -> i64 {
    match nr {
        SYS_EXIT => proc::exit_current(a1 as i64),
        SYS_WRITE => sys_write(a1, a2, a3),
        SYS_READ => sys_read(a1, a2, a3),
        SYS_PIPE => sys_pipe(a1),
        SYS_CLOSE => sys_close(a1),
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
    };
    match copy_to_user(ptr, &buf[..n]) {
        Ok(()) => n as i64,
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
