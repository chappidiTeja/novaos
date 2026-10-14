//! User processes: an address space, a ring-3 entry point, and a handle the
//! kernel can wait on. Each process runs on its own kernel thread, which
//! provides the kernel stack for its system calls and interrupts.

use crate::gdt::{USER_CS, USER_DS};
use crate::mem::{pmm, vmm, PAGE_SIZE};
use crate::{elf, println, sched};
use alloc::sync::Arc;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// User stack: 16 pages ending here, with nothing mapped below (a guard).
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
const USER_STACK_PAGES: u64 = 16;

/// Exit status of a process killed by a CPU exception: -(1000 + vector).
const KILLED_BASE: i64 = 1000;

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

/// Owns a process's page tables and user pages; frees them all when dropped.
pub struct AddressSpace {
    pub pml4: u64,
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        vmm::destroy_address_space(self.pml4);
    }
}

/// What the kernel keeps to observe a process from outside.
pub struct ProcHandle {
    pub pid: u64,
    done: AtomicBool,
    code: AtomicI64,
}

impl ProcHandle {
    pub fn finish(&self, code: i64) {
        self.code.store(code, Ordering::Relaxed);
        self.done.store(true, Ordering::Release);
    }

    pub fn exit_code(&self) -> Option<i64> {
        if self.done.load(Ordering::Acquire) {
            Some(self.code.load(Ordering::Relaxed))
        } else {
            None
        }
    }
}

/// Everything a thread needs to become a user process.
pub struct UserProc {
    pub space: AddressSpace,
    pub entry: u64,
    pub rsp: u64,
    pub arg: u64,
    pub handle: Arc<ProcHandle>,
}

#[derive(Debug)]
pub enum SpawnError {
    OutOfMemory,
    Elf(elf::ElfError),
}

impl From<elf::ElfError> for SpawnError {
    fn from(e: elf::ElfError) -> Self {
        SpawnError::Elf(e)
    }
}

/// Create a process from an ELF image and start it. `arg` arrives in rdi.
pub fn spawn_user(
    name: &'static str,
    image: &[u8],
    arg: u64,
) -> Result<Arc<ProcHandle>, SpawnError> {
    let pml4 = vmm::new_address_space().ok_or(SpawnError::OutOfMemory)?;
    // From here on, dropping `space` releases everything mapped so far.
    let space = AddressSpace { pml4 };

    let entry = elf::load(pml4, image)?;

    for i in 1..=USER_STACK_PAGES {
        let frame = pmm::alloc_zeroed_frame().ok_or(SpawnError::OutOfMemory)?;
        let flags = vmm::user_flags(true, false);
        if vmm::map_in(pml4, USER_STACK_TOP - i * PAGE_SIZE, frame, flags).is_err() {
            pmm::free_frame(frame);
            return Err(SpawnError::OutOfMemory);
        }
    }

    let handle = Arc::new(ProcHandle {
        pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
        done: AtomicBool::new(false),
        code: AtomicI64::new(0),
    });
    sched::spawn_user(
        name,
        UserProc {
            space,
            entry,
            // The compiler expects rsp = 8 mod 16 on function entry.
            rsp: USER_STACK_TOP - 8,
            arg,
            handle: handle.clone(),
        },
    );
    Ok(handle)
}

/// Block until the process finishes, or `timeout_ticks` pass.
pub fn wait(handle: &ProcHandle, timeout_ticks: u64) -> Option<i64> {
    for _ in 0..timeout_ticks {
        if let Some(code) = handle.exit_code() {
            return Some(code);
        }
        sched::sleep_ticks(1);
    }
    handle.exit_code()
}

pub fn current_pid() -> u64 {
    sched::current_user_handle().map_or(0, |h| h.pid)
}

/// The exit system call.
pub fn exit_current(code: i64) -> ! {
    if let Some(handle) = sched::current_user_handle() {
        handle.finish(code);
    }
    sched::exit()
}

/// Called from exception handlers when ring-3 code faulted: report it and
/// terminate only that process. The kernel carries on.
pub fn kill_current(what: &str, vector: u8, rip: u64) -> ! {
    let pid = current_pid();
    println!(
        "[proc {} \"{}\"] killed: {} at rip={:#x}",
        pid,
        sched::current_name(),
        what,
        rip
    );
    if let Some(handle) = sched::current_user_handle() {
        handle.finish(-(KILLED_BASE + vector as i64));
    }
    sched::exit()
}

/// Entry function of a user process's kernel thread: drop to ring 3.
pub fn user_thread_main() {
    let (entry, rsp, arg) = sched::with_current_user(|u| (u.entry, u.rsp, u.arg))
        .expect("user_thread_main on a thread without a process");
    unsafe { enter_usermode(entry, rsp, arg) }
}

extern "C" {
    /// Build an iretq frame for ring 3 and jump: rdi = rip, rsi = rsp, rdx = arg
    /// (delivered in rdi). All other registers are cleared.
    fn enter_usermode(rip: u64, rsp: u64, arg: u64) -> !;
}

global_asm!(
    ".global enter_usermode",
    "enter_usermode:",
    "push {user_ss}",
    "push rsi",
    "push 0x202", // rflags: IF set, everything else clear
    "push {user_cs}",
    "push rdi",
    "mov rdi, rdx",
    "xor eax, eax",
    "xor ebx, ebx",
    "xor ecx, ecx",
    "xor edx, edx",
    "xor esi, esi",
    "xor ebp, ebp",
    "xor r8d, r8d",
    "xor r9d, r9d",
    "xor r10d, r10d",
    "xor r11d, r11d",
    "xor r12d, r12d",
    "xor r13d, r13d",
    "xor r14d, r14d",
    "xor r15d, r15d",
    "iretq",
    user_ss = const USER_DS as u64,
    user_cs = const USER_CS as u64,
);

/// Runs the bundled user program in several modes, including hostile ones,
/// and checks that the kernel contains each of them. Panics on failure.
pub fn self_test() {
    const HELLO: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/build/hello.elf"));

    // Exit status of a process killed by exception `vector`.
    const fn killed(vector: i64) -> i64 {
        -(KILLED_BASE + vector)
    }
    const PF: i64 = killed(14);
    const GP: i64 = killed(13);

    fn run(name: &'static str, mode: u64, expect: i64) {
        let handle = spawn_user(name, HELLO, mode).expect("could not start process");
        let code = wait(&handle, 500).unwrap_or_else(|| panic!("process {} timed out", name));
        assert_eq!(code, expect, "process {} ended with the wrong status", name);
        println!("[ok] user: {} -> {}", name, code);
    }

    sched::sleep_ticks(10); // let earlier threads be reaped before measuring
    let frames_before = pmm::stats().0;
    let heap_before = crate::mem::heap::used();
    let threads_before = sched::thread_count();

    run("hello", 0, 7);
    run("null-deref", 1, PF);
    run("privileged-hlt", 2, GP);
    run("bad-syscall-args", 3, 0);
    run("read-kernel-memory", 7, PF);
    run("stack-overflow", 8, PF);
    run("write-to-code", 9, PF);

    // Two processes using the same virtual address must not see each other.
    let a = spawn_user("isolation-a", HELLO, 4).expect("spawn a");
    let b = spawn_user("isolation-b", HELLO, 5).expect("spawn b");
    assert_eq!(wait(&a, 500), Some(0), "process a saw foreign memory");
    assert_eq!(wait(&b, 500), Some(0), "process b saw foreign memory");
    drop((a, b)); // the handles are heap allocations too; release before measuring leaks
    println!("[ok] user: address spaces are isolated");

    run("syscall-flood", 6, 0);

    // Everything the processes owned must come back.
    sched::sleep_ticks(10);
    assert_eq!(sched::thread_count(), threads_before, "process threads not cleaned up");
    assert_eq!(crate::mem::heap::used(), heap_before, "kernel heap leaked");
    assert_eq!(pmm::stats().0, frames_before, "physical frames leaked");
    println!("[ok] user: all frames and stacks reclaimed");
    println!("[ok] user-mode self-test");
}
