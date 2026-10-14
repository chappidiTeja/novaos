//! System calls via the `syscall` instruction.
//!
//! ABI: number in rax; arguments in rdi, rsi, rdx, r10, r8 (up to five); result in
//! rax (negative = error). rcx and r11 are clobbered by the instruction itself;
//! rdi, rsi, rdx, r10, r8 and r9 are clobbered by the kernel (they are zeroed on
//! return so no kernel values leak). All other registers are preserved.
//!
//! Numbers: 0 exit(code), 1 write(fd, ptr, len), 2 getpid(), 3 sleep(ticks),
//!          4 yield(), 5 ticks().

use crate::gdt::KERNEL_CS;
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

const EBADF: i64 = -9;
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const ENOSYS: i64 = -38;

/// Largest single write we are willing to copy into the kernel.
const MAX_WRITE: u64 = 4096;
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

fn sys_write(fd: u64, ptr: u64, len: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return EBADF;
    }
    if len > MAX_WRITE {
        return EINVAL;
    }
    match copy_from_user(ptr, len) {
        Ok(bytes) => {
            serial::write_bytes(&bytes);
            bytes.len() as i64
        }
        Err(e) => e,
    }
}

/// Copy `len` bytes from user address `ptr`. Every page is checked in the
/// caller's page tables to be mapped and user-accessible, and is read through
/// the direct map, so a bad pointer gives EFAULT instead of a kernel fault.
fn copy_from_user(ptr: u64, len: u64) -> Result<Vec<u8>, i64> {
    let end = ptr.checked_add(len).ok_or(EFAULT)?;
    if end > vmm::USER_SPACE_END {
        return Err(EFAULT);
    }
    let pml4 = vmm::read_cr3();
    let mut out = Vec::with_capacity(len as usize);
    let mut addr = ptr;
    while addr < end {
        let page = vmm::translate_in(pml4, addr).ok_or(EFAULT)?;
        if !page.user {
            return Err(EFAULT);
        }
        let chunk = (((addr & !0xFFF) + 0x1000).min(end) - addr) as usize;
        let src = crate::mem::phys_to_virt(page.phys);
        out.extend_from_slice(unsafe { core::slice::from_raw_parts(src, chunk) });
        addr += chunk as u64;
    }
    Ok(out)
}
