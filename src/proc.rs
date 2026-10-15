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
use alloc::vec::Vec;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};

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

// ---- signals ------------------------------------------------------------------------------------
//
// Every signal NovaOS knows ends the process (there are no user-mode handlers); a process can
// ask to ignore any of them except SIGKILL. A signal that has been sent wakes the target out
// of whatever it is waiting for and takes effect as its current system call ends (or at the
// next timer tick if it is running in ring 3).

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGKILL: u32 = 9;
pub const SIGUSR1: u32 = 10;
pub const SIGUSR2: u32 = 12;
pub const SIGALRM: u32 = 14;
pub const SIGTERM: u32 = 15;
const VALID_SIGNALS: u32 = (1 << SIGHUP) | (1 << SIGINT) | (1 << SIGQUIT) | (1 << SIGKILL) | (1 << SIGUSR1)
    | (1 << SIGUSR2) | (1 << SIGALRM) | (1 << SIGTERM);
/// Exit status of a process ended by signal `n`: -(2000 + n).
pub const SIGNALED_BASE: i64 = 2000;

pub const ESRCH: i64 = -3;
pub const ECHILD: i64 = -10;

/// What the kernel keeps to observe a process from outside.
pub struct ProcHandle {
    pub pid: u64,
    /// The process that started it (0 if the kernel did).
    pub ppid: u64,
    /// Process group: Control-C and `kill -pgid` reach every member. A shell gives each job its
    /// own group.
    pub pgid: AtomicU64,
    pub name: String,
    pub start_tick: u64,
    done: AtomicBool,
    code: AtomicI64,
    finished: WaitQueue,
    /// Signals sent and not yet acted on (bit n = signal n).
    pub pending: AtomicU32,
    /// Signals the process asked to ignore (SIGKILL cannot be).
    pub ignored: AtomicU32,
    /// The kernel thread that runs it (usize::MAX until it exists).
    pub thread: AtomicUsize,
    /// Timer ticks spent running.
    pub cpu_ticks: AtomicU64,
    pub mem: Weak<UserMem>,
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

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Signals that are pending and not ignored.
    fn deliverable(&self) -> u32 {
        let pending = self.pending.load(Ordering::Acquire);
        let ignored = self.ignored.load(Ordering::Relaxed) & !(1 << SIGKILL);
        pending & !ignored
    }
}

/// Every process that exists or is a zombie nobody has waited for yet.
static ALL: SpinLock<BTreeMap<u64, Arc<ProcHandle>>> = SpinLock::new(BTreeMap::new());

/// Look a process up by id.
pub fn find(pid: u64) -> Option<Arc<ProcHandle>> {
    ALL.lock().get(&pid).cloned()
}

/// Snapshot of the process table, in pid order.
pub fn all_processes() -> alloc::vec::Vec<Arc<ProcHandle>> {
    ALL.lock().values().cloned().collect()
}

fn valid_signal(sig: u32) -> bool {
    sig == 0 || (sig <= 31 && VALID_SIGNALS & (1 << sig) != 0)
}

/// Mark `sig` pending for a process and wake it. Allocation-free and without any blocking, so
/// it can run in an interrupt handler (Control-C does).
fn deliver(handle: &ProcHandle, sig: u32) {
    if sig == 0 || handle.is_done() {
        return;
    }
    let bit = 1u32 << sig;
    if sig != SIGKILL && handle.ignored.load(Ordering::Relaxed) & bit != 0 {
        return;
    }
    handle.pending.fetch_or(bit, Ordering::AcqRel);
    let thread = handle.thread.load(Ordering::Acquire);
    if thread != usize::MAX {
        sched::kick(thread);
    }
}

/// Send `sig` to process `pid` (0 only checks that it exists). Safe in interrupt handlers.
pub fn send_signal(pid: u64, sig: u32) -> Result<(), i64> {
    if !valid_signal(sig) {
        return Err(-22); // EINVAL
    }
    let all = ALL.lock();
    let handle = all.get(&pid).ok_or(ESRCH)?;
    deliver(handle, sig);
    Ok(())
}

/// Send `sig` to every process in group `pgid`. ESRCH if the group is empty. Safe in interrupt
/// handlers.
pub fn send_signal_group(pgid: u64, sig: u32) -> Result<(), i64> {
    if !valid_signal(sig) {
        return Err(-22);
    }
    let all = ALL.lock();
    let mut found = false;
    for handle in all.values() {
        if handle.pgid.load(Ordering::Relaxed) == pgid && !handle.is_done() {
            found = true;
            deliver(handle, sig);
        }
    }
    if found {
        Ok(())
    } else {
        Err(ESRCH)
    }
}

/// Is a signal waiting that will end the calling process? (False for kernel threads.)
pub fn fatal_signal_pending() -> bool {
    sched::with_current_user(|u| u.handle.deliverable() != 0).unwrap_or(false)
}

/// If the calling process has a deliverable signal, finish it off (never returns).
/// Called as every system call ends and from the timer interrupt that interrupted ring 3.
pub fn act_on_signals() {
    let sig = sched::with_current_user(|u| {
        let live = u.handle.deliverable();
        if live == 0 {
            return None;
        }
        // SIGKILL first, then the lowest number.
        let sig = if live & (1 << SIGKILL) != 0 { SIGKILL } else { live.trailing_zeros() };
        u.handle.pending.fetch_and(!(1 << sig), Ordering::AcqRel);
        Some(sig)
    })
    .flatten();
    if let Some(sig) = sig {
        finish_current(-(SIGNALED_BASE + sig as i64));
        sched::exit();
    }
}

/// The calling process stops caring about the signals in `mask` (bit n = signal n).
pub fn set_ignored(mask: u32) {
    if let Some(h) = sched::current_user_handle() {
        h.ignored.store(mask & VALID_SIGNALS & !(1 << SIGKILL), Ordering::Relaxed);
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
    /// Third and fourth arguments of the entry function (rdx, rcx): the environment block's
    /// address and length on the new stack.
    pub arg2: u64,
    pub arg3: u64,
    /// The environment ("NAME=value" strings, each ending in a NUL): inherited by children
    /// started without one of their own.
    pub env: Arc<Vec<u8>>,
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
pub const MAX_ARGV_BYTES: usize = 4096;
/// Largest environment block.
pub const MAX_ENV_BYTES: usize = 4096;

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

/// The environment given to programs the kernel starts itself (init, tests).
pub const DEFAULT_ENV: &[u8] = b"PATH=/bin\0HOME=/\0USER=root\0TERM=nova\0";

/// Descriptors and working directory chosen by the caller, and optionally a command line, with
/// the default environment. See `spawn_user_env`.
pub fn spawn_user_in(
    name: &str,
    image: &[u8],
    arg: u64,
    fds: FdTable,
    cwd: Cwd,
    argv: &[u8],
    argc: usize,
) -> Result<Arc<ProcHandle>, SpawnError> {
    spawn_user_env(name, image, arg, fds, cwd, argv, argc, DEFAULT_ENV, 0)
}

/// Copy `data` to the user stack at address `addr`, which must lie within the top two pages
/// (`frames[0]` is the topmost page).
fn copy_to_stack(frames: &[u64; 2], addr: u64, data: &[u8]) {
    let mut at = addr;
    let mut rest = data;
    while !rest.is_empty() {
        let page = ((USER_STACK_TOP - 1 - at) / PAGE_SIZE) as usize;
        let page_base = USER_STACK_TOP - (page as u64 + 1) * PAGE_SIZE;
        let offset = at - page_base;
        let n = rest.len().min((PAGE_SIZE - offset) as usize);
        unsafe {
            core::ptr::copy_nonoverlapping(rest.as_ptr(), crate::mem::phys_to_virt(frames[page] + offset), n);
        }
        at += n as u64;
        rest = &rest[n..];
    }
}

/// The full form. `argv` is `argc` NUL-terminated strings back to back and `env` is any number
/// of "NAME=value" strings, each ending in a NUL. Both are copied to the top of the new
/// process's stack, and the entry function receives
/// `(argc, pointer to argv, pointer to env, length of env)`. With no `argv` it receives `arg`
/// first (and the environment all the same).
pub fn spawn_user_env(
    name: &str,
    image: &[u8],
    arg: u64,
    fds: FdTable,
    cwd: Cwd,
    argv: &[u8],
    argc: usize,
    env: &[u8],
    pgid: u64,
) -> Result<Arc<ProcHandle>, SpawnError> {
    if argv.len() > MAX_ARGV_BYTES
        || env.len() > MAX_ENV_BYTES
        || (argc > 0 && argv.last() != Some(&0))
        || (!env.is_empty() && env.last() != Some(&0))
    {
        return Err(SpawnError::BadArguments);
    }
    let pml4 = vmm::new_address_space().ok_or(SpawnError::OutOfMemory)?;
    // From here on, dropping `space` releases everything mapped so far.
    let space = AddressSpace { pml4 };

    let entry = elf::load(pml4, image)?;

    let mut top_frames = [0u64; 2];
    for i in 1..=USER_STACK_PAGES {
        let frame = pmm::alloc_zeroed_frame().ok_or(SpawnError::OutOfMemory)?;
        let flags = vmm::user_flags(true, false);
        if vmm::map_in(pml4, USER_STACK_TOP - i * PAGE_SIZE, frame, flags).is_err() {
            pmm::free_frame(frame);
            return Err(SpawnError::OutOfMemory);
        }
        if i <= 2 {
            top_frames[(i - 1) as usize] = frame;
        }
    }

    // Environment at the very top, command line below it, then the stack proper. The compiler
    // expects rsp = 8 mod 16 on function entry.
    let env_reserved = crate::mem::align_up(env.len() as u64, 16);
    let argv_reserved = crate::mem::align_up(argv.len() as u64, 16);
    let env_user = USER_STACK_TOP - env_reserved;
    let argv_user = env_user - argv_reserved;
    copy_to_stack(&top_frames, env_user, env);
    copy_to_stack(&top_frames, argv_user, argv);
    let rsp = argv_user - 8;
    let (arg0, arg1) = if argv.is_empty() { (arg, 0) } else { (argc as u64, argv_user) };

    let mem = Arc::new(UserMem::new());
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    // 0 = the spawner's group, 1 = a new group of its own, anything else = join that group.
    let group = match pgid {
        0 => find(current_pid()).map_or(pid, |p| p.pgid.load(Ordering::Relaxed)),
        1 => pid,
        n => n,
    };
    let mut short_name = String::from(name);
    short_name.truncate(15);
    let handle = Arc::new(ProcHandle {
        pid,
        pgid: AtomicU64::new(group),
        ppid: current_pid(),
        name: short_name,
        start_tick: timer::ticks(),
        done: AtomicBool::new(false),
        code: AtomicI64::new(0),
        finished: WaitQueue::new(),
        pending: AtomicU32::new(0),
        ignored: AtomicU32::new(0),
        thread: AtomicUsize::new(usize::MAX),
        cpu_ticks: AtomicU64::new(0),
        mem: Arc::downgrade(&mem),
    });
    ALL.lock().insert(handle.pid, handle.clone());
    let thread = sched::spawn_user(
        name,
        UserProc {
            space,
            entry,
            rsp,
            arg: arg0,
            arg1,
            arg2: env_user,
            arg3: env.len() as u64,
            env: Arc::new(env.to_vec()),
            handle: handle.clone(),
            fds: Arc::new(SpinLock::new(fds)),
            cwd: Arc::new(SpinLock::new(cwd)),
            mem,
        },
    );
    handle.thread.store(thread, Ordering::Release);
    Ok(handle)
}

/// Processes started by system calls, until someone waits for them. (A short list that gives
/// its memory back when empty: a map would keep an empty node allocated for ever.)
static CHILDREN: SpinLock<Vec<Arc<ProcHandle>>> = SpinLock::new(Vec::new());

pub fn register_child(handle: Arc<ProcHandle>) {
    CHILDREN.lock().push(handle);
}

fn remove_child(pid: u64) -> Option<Arc<ProcHandle>> {
    let mut children = CHILDREN.lock();
    let at = children.iter().position(|h| h.pid == pid)?;
    let handle = children.swap_remove(at);
    if children.is_empty() {
        children.shrink_to_fit();
    }
    Some(handle)
}

fn is_child(pid: u64) -> bool {
    CHILDREN.lock().iter().any(|h| h.pid == pid)
}

/// A child that has been waited for (or is being waited for): out of the child table.
pub fn take_child(pid: u64) -> Option<Arc<ProcHandle>> {
    remove_child(pid)
}

/// Forget a finished process everywhere once nobody can ask about it any more.
pub fn reap(pid: u64) {
    remove_child(pid);
    ALL.lock().remove(&pid);
}

/// Sleep until the process finishes, however long that takes. If the waiting process is itself
/// killed meanwhile this returns early; the caller is about to end and never uses the value.
pub fn wait_forever(handle: &ProcHandle) -> i64 {
    handle.finished.wait_until_intr(|| handle.done.load(Ordering::Acquire));
    handle.exit_code().unwrap_or(0)
}


/// Woken whenever a process finishes: parents waiting for "any child" look again.
static CHILD_EVENT: WaitQueue = WaitQueue::new();
/// Counts finished processes, so a waiter can tell "something finished since I looked".
static CHILD_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Wait for a child of the calling process to finish and collect it. `want` is a particular
/// child's id, or None for any. Returns Some((pid, status)); None only with `nohang` when no
/// child has finished yet; Err(ECHILD) when there is no such child (any more).
pub fn wait_child(want: Option<u64>, nohang: bool) -> Result<Option<(u64, i64)>, i64> {
    let me = current_pid();
    // Looks at the child table once: a finished child (taken out of the table), a
    // running one, or none at all.
    let scan = || -> Result<Option<Arc<ProcHandle>>, i64> {
        let mut children = CHILDREN.lock();
        let mut any = false;
        let mut done_at = None;
        for (i, h) in children.iter().enumerate() {
            if h.ppid != me || want.map_or(false, |w| w != h.pid) {
                continue;
            }
            any = true;
            if h.is_done() {
                done_at = Some(i);
                break;
            }
        }
        match done_at {
            Some(i) => {
                let h = children.swap_remove(i);
                if children.is_empty() {
                    children.shrink_to_fit();
                }
                Ok(Some(h))
            }
            None if any => Ok(None),
            None => Err(ECHILD),
        }
    };
    loop {
        let seen = CHILD_EVENTS.load(Ordering::SeqCst); // before looking: a finish after this is not missed
        match scan()? {
            Some(h) => {
                let status = h.exit_code().unwrap_or(0);
                reap(h.pid);
                return Ok(Some((h.pid, status)));
            }
            None if nohang => return Ok(None),
            None => {
                // Sleep until some process finishes (re-scanning is cheap), or until a signal
                // that ends us arrives: then the value is never used.
                CHILD_EVENT.wait_until_intr(|| CHILD_EVENTS.load(Ordering::SeqCst) != seen);
                if fatal_signal_pending() {
                    return Ok(None);
                }
            }
        }
    }
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
        console::clear_foreground(handle.pid);
        // Zombie children nobody will ever wait for now are forgotten (running ones are
        // forgotten when they finish, below).
        let orphans: alloc::vec::Vec<u64> = {
            let children = CHILDREN.lock();
            children.iter().filter(|h| h.ppid == handle.pid && h.is_done()).map(|h| h.pid).collect()
        };
        for pid in orphans {
            reap(pid);
        }
        // A process nobody can wait for (not started by a system call, or its parent is gone)
        // leaves the table at once instead of lingering as a zombie. Done before the exit is
        // announced, so whoever wakes up for it already finds the table in its final state.
        let registered = is_child(handle.pid);
        let parent_alive = ALL.lock().get(&handle.ppid).map_or(false, |p| !p.is_done());
        if !registered || !parent_alive {
            reap(handle.pid);
        }
        handle.finish(code);
        CHILD_EVENTS.fetch_add(1, Ordering::SeqCst);
        CHILD_EVENT.wake_all();
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
    let (entry, rsp, arg, arg1, arg2, arg3) =
        sched::with_current_user(|u| (u.entry, u.rsp, u.arg, u.arg1, u.arg2, u.arg3))
            .expect("user_thread_main on a thread without a process");
    unsafe { enter_usermode(entry, rsp, arg, arg1, arg2, arg3) }
}

extern "C" {
    /// Build an iretq frame for ring 3 and jump: rdi = rip, rsi = rsp, rdx = first
    /// argument (delivered in rdi), rcx = second argument (delivered in rsi).
    /// All other registers are cleared.
    fn enter_usermode(rip: u64, rsp: u64, arg: u64, arg1: u64, arg2: u64, arg3: u64) -> !;
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
    "mov rdx, r8",
    "mov rcx, r9",
    "xor eax, eax",
    "xor ebx, ebx",
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

    // Programs read from the disk fill the block cache the first time. Read the one the waiter
    // test spawns now, so that warm-up does not look like a leak below.
    if crate::vfs::is_mounted() {
        let _ = crate::vfs::read_file("/bin/hello", novafs::ROOT_INO, 4 << 20);
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

    {
    // ---- signals -----------------------------------------------------------------------------------
    //
    // A fatal signal must end a process wherever it is: running in ring 3 with no system call to
    // notice it (only the timer can), or waiting in the kernel for a pipe, the console, a timer or
    // a child. And a signal it ignores must change nothing.
    let sig_exit = |sig: u32| -(SIGNALED_BASE + sig as i64);
    let kill_and_wait = |h: &Arc<ProcHandle>, sig: u32, what: &str| {
        send_signal(h.pid, sig).unwrap_or_else(|e| panic!("{}: kill failed with {}", what, e));
        let code = wait(h, 100).unwrap_or_else(|| panic!("{}: still running 1 s after signal {}", what, sig));
        assert_eq!(code, sig_exit(sig), "{}: wrong exit status", what);
    };

    let spinner = spawn_user("spinner", HELLO, 17).expect("spawn spinner");
    sched::sleep_ticks(5);
    assert!(spinner.exit_code().is_none(), "the spinning process exited by itself");
    assert!(spinner.cpu_ticks.load(Ordering::Relaxed) > 0, "CPU time was not counted");
    kill_and_wait(&spinner, SIGKILL, "spinning in ring 3");
    println!("[ok] signals: SIGKILL ends a process that never makes a system call");
    sched::sleep_ticks(20);

    let spinner = spawn_user("spinner", HELLO, 17).expect("spawn spinner");
    sched::sleep_ticks(3);
    kill_and_wait(&spinner, SIGTERM, "spinning, SIGTERM");

    let blocked = spawn_user("pipe-blocked", HELLO, 18).expect("spawn pipe reader");
    sched::sleep_ticks(5);
    assert!(sched::is_waiting(blocked.thread.load(Ordering::Acquire)), "the process should be waiting on its pipe");
    kill_and_wait(&blocked, SIGINT, "blocked reading a pipe");

    let sleeper = spawn_user("sleeper", HELLO, 19).expect("spawn sleeper");
    sched::sleep_ticks(5);
    kill_and_wait(&sleeper, SIGHUP, "asleep");

    let console_reader = spawn_user("console-blocked", HELLO, 21).expect("spawn console reader");
    sched::sleep_ticks(5);
    kill_and_wait(&console_reader, SIGTERM, "blocked reading the console");
    assert!(!console::input_ready(), "a killed reader took or left console input");
    println!("[ok] signals: end a process blocked in a pipe, a sleep and the console");

    // Ignored signals do nothing; SIGKILL cannot be ignored.
    let stubborn = spawn_user("stubborn", HELLO, 20).expect("spawn stubborn");
    sched::sleep_ticks(5);
    send_signal(stubborn.pid, SIGTERM).unwrap();
    send_signal(stubborn.pid, SIGINT).unwrap();
    sched::sleep_ticks(10);
    assert!(stubborn.exit_code().is_none(), "an ignored signal killed the process");
    assert_eq!(stubborn.pending.load(Ordering::Relaxed), 0, "ignored signals must not stay pending");
    kill_and_wait(&stubborn, SIGKILL, "ignoring TERM and INT");
    println!("[ok] signals: ignoring works for everything but SIGKILL");

    // A process waiting for its child dies when signalled; the child carries on and is killed
    // separately. Nothing may be left in the process table afterwards.
    if crate::vfs::is_mounted() {
        let parent = spawn_user("waiter", HELLO, 22).expect("spawn waiter");
        sched::sleep_ticks(30);
        assert!(parent.exit_code().is_none(), "the waiter exited with its child still running");
        let child_pid = all_processes()
            .iter()
            .find(|p| p.ppid == parent.pid)
            .map(|p| p.pid)
            .expect("the waiter's child is not in the process table");
        kill_and_wait(&parent, SIGTERM, "waiting for a child");
        assert!(find(child_pid).is_some_and(|c| !c.is_done()), "the child died with its parent");
        send_signal(child_pid, SIGKILL).unwrap();
        sched::sleep_ticks(10);
        assert!(find(child_pid).is_none(), "an orphan that has finished must leave the table");
        println!("[ok] signals: a waiting parent is killed, its orphan can be killed too, the table is clean");
    }

    // Bad requests.
    assert_eq!(send_signal(999_999, SIGTERM), Err(ESRCH));
    let done = spawn_user("short-lived", HELLO, 0).expect("spawn");
    assert!(wait(&done, 200).is_some());
    assert_eq!(send_signal(done.pid, 0), Err(ESRCH), "a process that finished and is gone");
    let live = spawn_user("alive", HELLO, 19).expect("spawn");
    assert_eq!(send_signal(live.pid, 0), Ok(()), "signal 0 only checks that the process exists");
    assert_eq!(send_signal(live.pid, 7), Err(-22), "an unsupported signal number");
    assert_eq!(send_signal(live.pid, 99), Err(-22));
    kill_and_wait(&live, SIGKILL, "cleanup");
    drop((spinner, blocked, sleeper, console_reader, stubborn, done, live));

    // Control-C goes to the foreground process, and only if there is one.
    let target = spawn_user("foreground", HELLO, 17).expect("spawn");
    sched::sleep_ticks(3);
    console::set_foreground(target.pid);
    console::push_input(3);
    assert_eq!(wait(&target, 100), Some(sig_exit(SIGINT)), "Control-C did not stop the foreground process");
    assert!(!console::input_ready(), "Control-C must not also arrive as input");
    assert_eq!(console::foreground(), 0, "a finished foreground process must stop being foreground");
    console::push_input(3);
    assert!(console::input_ready(), "with no foreground process, Control-C is ordinary input");
    let mut sink = [0u8; 4];
    assert_eq!(console::read(&mut sink), 1);
    assert_eq!(sink[0], 3);
    drop(target);
    println!("[ok] signals: Control-C stops the foreground process only");

    } // the handles above are gone before anything is measured

    // Everything the processes owned must come back.
    sched::sleep_ticks(10);
    assert_eq!(sched::thread_count(), threads_before, "process threads not cleaned up");
    assert_eq!(crate::mem::heap::used(), heap_before, "kernel heap leaked");
    assert_eq!(pmm::stats().0, frames_before, "physical frames leaked");
    println!("[ok] user: all frames and stacks reclaimed");
    println!("[ok] user-mode self-test");
}
