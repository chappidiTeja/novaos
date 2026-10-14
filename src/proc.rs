//! User processes: an address space, a ring-3 entry point, a file descriptor
//! table, and a handle the kernel can wait on. Each process runs on its own
//! kernel thread, which provides the kernel stack for its system calls and
//! interrupts.

use crate::gdt::{USER_CS, USER_DS};
use crate::ipc::console;
use crate::ipc::fd::{FdTable, File};
use crate::ipc::pipe;
use crate::ipc::waitq::WaitQueue;
use crate::mem::{pmm, vmm, PAGE_SIZE};
use crate::sync::SpinLock;
use crate::vfs::Cwd;
use crate::winsys::UserMem;
use crate::{elf, println, sched, timer};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// User stack: 16 pages ending here, with nothing mapped below (a guard).
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
const USER_STACK_PAGES: u64 = 16;

/// Exit status of a process killed by a CPU exception: -(1000 + vector).
const KILLED_BASE: i64 = 1000;

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

/// The bundled test/demo program (see user/hello).
pub const HELLO: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/build/hello.elf"));

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
    finished: WaitQueue,
}

impl ProcHandle {
    pub fn finish(&self, code: i64) {
        self.code.store(code, Ordering::Relaxed);
        self.done.store(true, Ordering::Release);
        self.finished.wake_all();
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
    /// First two arguments of the entry function (rdi, rsi).
    pub arg: u64,
    pub arg1: u64,
    pub handle: Arc<ProcHandle>,
    /// Shared so system calls can use it without holding the scheduler's data.
    pub fds: Arc<SpinLock<FdTable>>,
    pub cwd: Arc<SpinLock<Cwd>>,
    /// Heap pages and shared mappings. Declared after `space` on purpose:
    /// fields are dropped in order, so the address space (and with it every page
    /// table entry pointing at shared frames) is gone before the `Arc`s that keep
    /// those frames alive are released.
    pub mem: Arc<UserMem>,
}

#[derive(Debug)]
pub enum SpawnError {
    OutOfMemory,
    Elf(elf::ElfError),
    /// The command-line arguments do not fit.
    BadArguments,
}

/// Largest argument block (all argument strings together) a process can be given.
pub const MAX_ARGV_BYTES: usize = 2048;

impl From<elf::ElfError> for SpawnError {
    fn from(e: elf::ElfError) -> Self {
        SpawnError::Elf(e)
    }
}

/// Create a process from an ELF image with stdin/stdout/stderr on the console,
/// and start it. `arg` arrives in rdi.
pub fn spawn_user(
    name: &str,
    image: &[u8],
    arg: u64,
) -> Result<Arc<ProcHandle>, SpawnError> {
    spawn_user_with(name, image, arg, FdTable::with_stdio())
}

/// Like `spawn_user`, with a caller-prepared descriptor table (to hand the
/// process pipe ends, for example).
pub fn spawn_user_with(
    name: &str,
    image: &[u8],
    arg: u64,
    fds: FdTable,
) -> Result<Arc<ProcHandle>, SpawnError> {
    spawn_user_in(name, image, arg, fds, Cwd::root(), &[], 0)
}

/// The full form: descriptors and working directory chosen by the caller, and
/// optionally a command line. `argv` is `argc` NUL-terminated strings back to
/// back; it is copied to the top of the new process's stack, and the entry
/// function receives `(argc, pointer to argv)`. With no `argv` it receives `arg`.
pub fn spawn_user_in(
    name: &str,
    image: &[u8],
    arg: u64,
    fds: FdTable,
    cwd: Cwd,
    argv: &[u8],
    argc: usize,
) -> Result<Arc<ProcHandle>, SpawnError> {
    if argv.len() > MAX_ARGV_BYTES || (argc > 0 && argv.last() != Some(&0)) {
        return Err(SpawnError::BadArguments);
    }
    let pml4 = vmm::new_address_space().ok_or(SpawnError::OutOfMemory)?;
    // From here on, dropping `space` releases everything mapped so far.
    let space = AddressSpace { pml4 };

    let entry = elf::load(pml4, image)?;

    let mut top_frame = 0;
    for i in 1..=USER_STACK_PAGES {
        let frame = pmm::alloc_zeroed_frame().ok_or(SpawnError::OutOfMemory)?;
        let flags = vmm::user_flags(true, false);
        if vmm::map_in(pml4, USER_STACK_TOP - i * PAGE_SIZE, frame, flags).is_err() {
            pmm::free_frame(frame);
            return Err(SpawnError::OutOfMemory);
        }
        if i == 1 {
            top_frame = frame;
        }
    }

    // The compiler expects rsp = 8 mod 16 on function entry.
    let (rsp, arg0, arg1) = if argv.is_empty() {
        (USER_STACK_TOP - 8, arg, 0)
    } else {
        let reserved = crate::mem::align_up(argv.len() as u64, 16);
        let argv_user = USER_STACK_TOP - reserved;
        unsafe {
            core::ptr::copy_nonoverlapping(
                argv.as_ptr(),
                crate::mem::phys_to_virt(top_frame + PAGE_SIZE - reserved),
                argv.len(),
            );
        }
        (argv_user - 8, argc as u64, argv_user)
    };

    let handle = Arc::new(ProcHandle {
        pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
        done: AtomicBool::new(false),
        code: AtomicI64::new(0),
        finished: WaitQueue::new(),
    });
    sched::spawn_user(
        name,
        UserProc {
            space,
            entry,
            rsp,
            arg: arg0,
            arg1,
            handle: handle.clone(),
            fds: Arc::new(SpinLock::new(fds)),
            cwd: Arc::new(SpinLock::new(cwd)),
            mem: Arc::new(UserMem::new()),
        },
    );
    Ok(handle)
}

/// Processes started by system calls, until someone waits for them.
static CHILDREN: SpinLock<BTreeMap<u64, Arc<ProcHandle>>> = SpinLock::new(BTreeMap::new());

pub fn register_child(handle: Arc<ProcHandle>) {
    CHILDREN.lock().insert(handle.pid, handle);
}

pub fn take_child(pid: u64) -> Option<Arc<ProcHandle>> {
    CHILDREN.lock().remove(&pid)
}

/// Sleep until the process finishes, however long that takes.
pub fn wait_forever(handle: &ProcHandle) -> i64 {
    handle.finished.wait_until(|| handle.done.load(Ordering::Acquire));
    handle.exit_code().unwrap_or(0)
}

/// Start the first user program: the desktop if the boot menu entry asks for it,
/// otherwise the shell, both from the disk. Returns the program's path, or None if
/// there is no disk or nothing to start on it.
pub fn spawn_init() -> Option<&'static str> {
    // The boot menu entry can ask for the graphical desktop instead of the shell.
    if crate::limine::cmdline_has("desktop") {
        if spawn_from_disk("/bin/desktop", "desktop", b"desktop\0") {
            return Some("/bin/desktop");
        }
        println!("[init] no desktop on the disk; starting the shell instead");
    }
    if spawn_from_disk("/bin/sh", "sh", b"sh\0") {
        Some("/bin/sh")
    } else {
        None
    }
}

/// Start the program at `path` on the disk with the console as its stdin/stdout.
fn spawn_from_disk(path: &str, name: &str, argv: &[u8]) -> bool {
    let image = match crate::vfs::read_file(path, novafs::ROOT_INO, 4 << 20) {
        Ok(image) => image,
        Err(_) => return false,
    };
    match spawn_user_in(name, &image, 0, FdTable::with_stdio(), Cwd::root(), argv, 1) {
        Ok(_) => true,
        Err(e) => {
            println!("[init] could not start {}: {:?}", path, e);
            false
        }
    }
}

/// Sleep until the process finishes, or `timeout_ticks` pass.
pub fn wait(handle: &ProcHandle, timeout_ticks: u64) -> Option<i64> {
    let deadline = timer::ticks() + timeout_ticks;
    handle
        .finished
        .wait_until_deadline(deadline, || handle.done.load(Ordering::Acquire));
    handle.exit_code()
}

pub fn current_pid() -> u64 {
    sched::current_user_handle().map_or(0, |h| h.pid)
}

/// The calling process's descriptor table.
pub fn current_fds() -> Option<Arc<SpinLock<FdTable>>> {
    sched::with_current_user(|u| u.fds.clone())
}

/// The calling process's working directory.
pub fn current_cwd() -> Option<Arc<SpinLock<Cwd>>> {
    sched::with_current_user(|u| u.cwd.clone())
}

/// The calling process's dynamic memory (heap pages, shared mappings).
pub fn current_mem() -> Option<Arc<UserMem>> {
    sched::with_current_user(|u| u.mem.clone())
}

/// Close every descriptor (so pipe peers see EOF), let go of the screen and any
/// windows, and publish the exit status.
fn finish_current(code: i64) {
    if let Some(fds) = current_fds() {
        fds.lock().close_all();
    }
    if let Some(handle) = sched::current_user_handle() {
        // Before publishing the exit: anyone waiting for this process must find
        // the screen already released.
        crate::winsys::on_process_exit(handle.pid);
        handle.finish(code);
    }
}

/// The exit system call.
pub fn exit_current(code: i64) -> ! {
    finish_current(code);
    sched::exit()
}

/// Called from exception handlers when ring-3 code faulted: report it and
/// terminate only that process. The kernel carries on.
pub fn kill_current(what: &str, vector: u8, rip: u64) -> ! {
    println!(
        "[proc {} \"{}\"] killed: {} at rip={:#x}",
        current_pid(),
        sched::current_name(),
        what,
        rip
    );
    finish_current(-(KILLED_BASE + vector as i64));
    sched::exit()
}

/// Entry function of a user process's kernel thread: drop to ring 3.
pub fn user_thread_main() {
    let (entry, rsp, arg, arg1) = sched::with_current_user(|u| (u.entry, u.rsp, u.arg, u.arg1))
        .expect("user_thread_main on a thread without a process");
    unsafe { enter_usermode(entry, rsp, arg, arg1) }
}

extern "C" {
    /// Build an iretq frame for ring 3 and jump: rdi = rip, rsi = rsp, rdx = first
    /// argument (delivered in rdi), rcx = second argument (delivered in rsi).
    /// All other registers are cleared.
    fn enter_usermode(rip: u64, rsp: u64, arg: u64, arg1: u64) -> !;
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
    "mov rsi, rcx",
    "xor eax, eax",
    "xor ebx, ebx",
    "xor ecx, ecx",
    "xor edx, edx",
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

/// Start the interactive demo: a user program that reads a line from stdin
/// (keyboard or serial) and echoes it back. Runs detached.
pub fn spawn_console_demo() {
    let _ = spawn_user("console-demo", HELLO, 11).expect("could not start the console demo");
}

/// Runs the bundled user program in several modes, including hostile ones,
/// and checks that the kernel contains each of them. Panics on failure.
pub fn self_test() {
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

    // File system calls from user mode. Run first: the block cache fills as the
    // disk is used, which would look like a leak in the measurements below.
    if crate::vfs::is_mounted() {
        let before = crate::vfs::with_fs(|fs| fs.statfs()).expect("filesystem vanished");
        run("fs-syscalls", 16, 0);
        crate::vfs::flush_pending(); // frees files that were unlinked while open
        let after = crate::vfs::with_fs(|fs| {
            fs.check().expect("filesystem inconsistent after the user-mode test");
            fs.statfs()
        })
        .expect("filesystem vanished");
        assert_eq!(after, before, "file system calls leaked blocks or inodes");
        println!("[ok] user: file system calls work, and leave the disk as they found it");
    } else {
        println!("[skip] user: no disk, file system call tests not run");
    }

    // The screen, input and window system calls, tested with the gfxtest program
    // from the disk. They need a screen, so they are skipped on a machine without one.
    // The whole cycle runs twice: the first run warms up things that are allocated once
    // and kept (disk blocks in the cache, the first node of a lookup table). After
    // that, a second run must leave the heap, the free frames and the thread count
    // exactly as it found them, so any per-run leak shows up.
    if crate::gfx::console::is_active() && crate::vfs::is_mounted() {
        let run_gfx = |mode: &'static str, expect: i64| {
            // The file image and handle live only inside this block, so that they are
            // gone before anything is measured.
            let code = {
                let image = crate::vfs::read_file("/bin/gfxtest", novafs::ROOT_INO, 4 << 20)
                    .expect("/bin/gfxtest is missing from the disk");
                let mut argv = alloc::vec::Vec::new();
                for part in ["gfxtest", mode] {
                    argv.extend_from_slice(part.as_bytes());
                    argv.push(0);
                }
                let handle = spawn_user_in("gfxtest", &image, 0, FdTable::with_stdio(), Cwd::root(), &argv, 2)
                    .expect("could not start gfxtest");
                wait(&handle, 3000).unwrap_or_else(|| panic!("gfxtest {} timed out", mode))
            };
            assert_eq!(code, expect, "gfxtest {} ended with the wrong status", mode);
            println!("[ok] gfx: {} -> {}", mode, code);
        };
        let cycle = || {
            run_gfx("misuse", 0);
            run_gfx("alloc", 0);
            // A desktop that crashes must give the screen and keyboard back: the very
            // next claim (inside "lifecycle") only works if it did.
            run_gfx("crash", PF);
            run_gfx("lifecycle", 0);
            sched::sleep_ticks(20); // let every process thread be reaped before measuring
        };
        cycle();
        let (frames_a, heap_a, threads_a) = (pmm::stats().0, crate::mem::heap::used(), sched::thread_count());
        cycle();
        let (frames_b, heap_b, threads_b) = (pmm::stats().0, crate::mem::heap::used(), sched::thread_count());
        assert_eq!(frames_b, frames_a, "the graphics tests leaked physical frames");
        assert_eq!(heap_b, heap_a, "the graphics tests leaked kernel heap");
        assert_eq!(threads_b, threads_a, "the graphics tests leaked threads");
        println!("[ok] gfx: screen released after exit and after a crash; a second run leaks nothing");
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

    // Stdin: the process blocks in read(0); typed bytes must wake it.
    let reader = spawn_user("stdin-reader", HELLO, 10).expect("spawn stdin reader");
    sched::sleep_ticks(5);
    assert!(reader.exit_code().is_none(), "stdin reader did not block");
    for &byte in b"abc" {
        console::push_input(byte);
    }
    assert_eq!(wait(&reader, 500), Some(0), "stdin reader got the wrong bytes");
    drop(reader);
    println!("[ok] user: blocked in read(0), woken by input");

    // Descriptors and pipes from inside a process.
    run("pipe-syscalls", 14, 0);

    // Two processes talk through a pipe the kernel created for them. The writer
    // never closes it explicitly: exiting must close its end and give the
    // reader end-of-file.
    let (read_end, write_end) = pipe::pipe();
    let mut reader_fds = FdTable::with_stdio();
    reader_fds.install_at(3, File::PipeRead(read_end));
    let mut writer_fds = FdTable::with_stdio();
    writer_fds.install_at(3, File::PipeWrite(write_end));
    let consumer = spawn_user_with("pipe-reader", HELLO, 13, reader_fds).expect("spawn reader");
    let producer = spawn_user_with("pipe-writer", HELLO, 12, writer_fds).expect("spawn writer");
    assert_eq!(wait(&producer, 1000), Some(0), "pipe writer failed");
    assert_eq!(wait(&consumer, 1000), Some(0), "pipe reader saw wrong data");
    drop((producer, consumer));
    println!("[ok] user: 50000 bytes through a pipe between two processes");

    // Writing to a pipe nobody can read must fail, not hang.
    let (read_end, write_end) = pipe::pipe();
    drop(read_end);
    let mut fds = FdTable::with_stdio();
    fds.install_at(3, File::PipeWrite(write_end));
    let orphan = spawn_user_with("broken-pipe", HELLO, 15, fds).expect("spawn orphan writer");
    assert_eq!(wait(&orphan, 500), Some(0), "broken pipe was not reported");
    drop(orphan);
    println!("[ok] user: write to an unread pipe returns an error");

    // Everything the processes owned must come back.
    sched::sleep_ticks(10);
    assert_eq!(sched::thread_count(), threads_before, "process threads not cleaned up");
    assert_eq!(crate::mem::heap::used(), heap_before, "kernel heap leaked");
    assert_eq!(pmm::stats().0, frames_before, "physical frames leaked");
    println!("[ok] user: all frames and stacks reclaimed");
    println!("[ok] user-mode self-test");
}
