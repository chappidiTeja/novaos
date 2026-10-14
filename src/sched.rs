//! Preemptive round-robin scheduler for kernel threads (single CPU).
//!
//! The timer interrupt calls `on_tick`, which wakes sleepers and, once a thread's
//! time slice is used up, switches to the next ready thread. Threads can also
//! give up the CPU themselves with `yield_now` and `sleep_ticks`.

use crate::mem::{heap, vmm};
use crate::proc::{ProcHandle, UserProc};
use crate::sync::{irq_restore, irq_save, IrqCell};
use crate::{gdt, println, syscall, timer};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, Ordering};

const STACK_SIZE: usize = 32 * 1024;
/// Timer ticks a thread may run before it is preempted.
const SLICE_TICKS: u32 = 2;
/// Slots reserved up front so ordinary spawning does not grow the tables.
const RESERVED_THREADS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ready,
    Running,
    /// Waiting for a timer deadline (`sleep_ticks`).
    Sleeping,
    /// Waiting for `wake` from another thread or an interrupt, optionally with
    /// a deadline (see `block_current`).
    Blocked,
    Dead,
}

struct Thread {
    name: &'static str,
    /// Saved stack pointer while the thread is not running.
    rsp: u64,
    state: State,
    wake_at: u64,
    /// Page tables to load when this thread runs (the kernel's, or its process's).
    cr3: u64,
    /// Top of this thread's kernel stack: where ring-3 interrupts and system
    /// calls land. 0 for the boot thread, which never runs user code.
    kstack_top: u64,
    /// Set if this thread is the body of a user process.
    user: Option<UserProc>,
    /// Owns the thread's stack memory.
    _stack: Vec<u8>,
}

/// What `schedule` has to do to move from one thread to the next.
struct Switch {
    save: *mut u64,
    to: u64,
    cr3: u64,
    kstack_top: u64,
}

struct Sched {
    /// Indexed by thread id. A slot is None once the thread has been reaped.
    threads: Vec<Option<Box<Thread>>>,
    free_slots: Vec<usize>,
    ready: VecDeque<usize>,
    sleeping: Vec<usize>,
    /// Exited threads whose stacks are freed by the next thread to run.
    zombies: Vec<usize>,
    current: usize,
    idle: usize,
    slice_left: u32,
    switches: u64,
}

static SCHED: IrqCell<Sched> = IrqCell::new(Sched {
    threads: Vec::new(),
    free_slots: Vec::new(),
    ready: VecDeque::new(),
    sleeping: Vec::new(),
    zombies: Vec::new(),
    current: 0,
    idle: 0,
    slice_left: SLICE_TICKS,
    switches: 0,
});

static STARTED: AtomicBool = AtomicBool::new(false);

extern "C" {
    /// Save callee-saved registers on the current stack, store the stack pointer
    /// at `*save`, load `to` as the new stack pointer and restore from it.
    fn switch_context(save: *mut u64, to: u64);
    /// First code a new thread runs (reached by `ret` from `switch_context`).
    fn thread_trampoline();
}

global_asm!(
    ".global switch_context",
    "switch_context:",
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rdi], rsp",
    "mov rsp, rsi",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    // New threads arrive here with their entry function in r12. Interrupts are
    // still off from the switch, so turn them on before running thread code.
    ".global thread_trampoline",
    "thread_trampoline:",
    "sti",
    "mov rdi, r12",
    "call {main}",
    "ud2",
    main = sym thread_main,
);

extern "C" fn thread_main(entry: usize) -> ! {
    let entry: fn() = unsafe { core::mem::transmute(entry) };
    entry();
    exit()
}

impl Sched {
    fn thread(&mut self, id: usize) -> &mut Thread {
        self.threads[id].as_mut().expect("scheduler: dead thread id")
    }

    fn add(&mut self, thread: Box<Thread>) -> usize {
        let id = match self.free_slots.pop() {
            Some(id) => id,
            None => {
                self.threads.push(None);
                self.threads.len() - 1
            }
        };
        self.threads[id] = Some(thread);
        id
    }

    /// Take exited threads (other than the one still running on its own stack)
    /// out of the table. The caller drops them after leaving the scheduler's
    /// critical section: dropping a process closes its pipes, which wakes
    /// waiters, which needs the scheduler again.
    fn reap(&mut self) -> Vec<Box<Thread>> {
        let mut dead = Vec::new();
        let mut i = 0;
        while i < self.zombies.len() {
            let id = self.zombies[i];
            if id == self.current {
                i += 1;
            } else {
                self.zombies.swap_remove(i);
                if let Some(thread) = self.threads[id].take() {
                    dead.push(thread);
                }
                self.free_slots.push(id);
            }
        }
        dead
    }

    /// Make a Blocked thread ready again. Returns false if it was not blocked
    /// (already woken, timed out, or gone), so callers can skip stale entries.
    fn wake(&mut self, id: usize) -> bool {
        match self.threads.get_mut(id) {
            Some(Some(thread)) if thread.state == State::Blocked => {
                thread.state = State::Ready;
                // A timed wait registered a deadline too; it no longer applies.
                self.sleeping.retain(|&s| s != id);
                self.ready.push_back(id);
                true
            }
            _ => false,
        }
    }

    /// Choose the next thread, or None if the current thread keeps running.
    fn pick_next(&mut self) -> Option<Switch> {
        let old = self.current;
        if self.thread(old).state == State::Running {
            self.thread(old).state = State::Ready;
            if old != self.idle {
                self.ready.push_back(old);
            }
        }

        let next = self.ready.pop_front().unwrap_or(self.idle);
        self.slice_left = SLICE_TICKS;
        self.thread(next).state = State::Running;
        if next == old {
            return None;
        }

        self.current = next;
        self.switches += 1;
        let save = &mut self.thread(old).rsp as *mut u64;
        let incoming = self.thread(next);
        Some(Switch {
            save,
            to: incoming.rsp,
            cr3: incoming.cr3,
            kstack_top: incoming.kstack_top,
        })
    }
}

/// Switch to the next thread. Must be called with interrupts disabled.
fn schedule() {
    let (dead, switch) = SCHED.with(|s| {
        let dead = s.reap();
        (dead, s.pick_next())
    });
    drop(dead);
    if let Some(sw) = switch {
        // The kernel half is identical in every address space, so it is safe to
        // change page tables while still running on the outgoing thread's stack.
        if sw.cr3 != vmm::read_cr3() {
            vmm::activate(sw.cr3);
        }
        gdt::set_kernel_stack(sw.kstack_top);
        syscall::set_kernel_stack(sw.kstack_top);
        unsafe { switch_context(sw.save, sw.to) };
    }
}

/// Build a thread whose first switch-in runs `entry` on a fresh stack.
fn new_thread(name: &'static str, entry: fn()) -> Box<Thread> {
    let mut stack = alloc::vec![0u8; STACK_SIZE];
    let top = (stack.as_mut_ptr() as usize + STACK_SIZE) & !15;
    // Seven words that `switch_context` will pop: r15 r14 r13 r12 rbx rbp, then
    // the return address. After `ret` rsp is `top`, 16-byte aligned, as the
    // trampoline's `call` needs.
    let sp = top - 7 * 8;
    unsafe {
        let frame = sp as *mut u64;
        frame.add(3).write(entry as usize as u64); // r12
        frame.add(6).write(thread_trampoline as *const () as usize as u64);
    }
    Box::new(Thread {
        name,
        rsp: sp as u64,
        state: State::Ready,
        wake_at: 0,
        cr3: vmm::kernel_cr3(),
        kstack_top: top as u64,
        user: None,
        _stack: stack,
    })
}

fn idle_main() {
    loop {
        unsafe { asm!("hlt", options(nomem, nostack)) };
    }
}

/// Turn the running kernel code into thread 0 ("main") and create the idle
/// thread. Interrupts must be enabled afterwards for preemption to happen.
pub fn init() {
    let main = Box::new(Thread {
        name: "main",
        rsp: 0,
        state: State::Running,
        wake_at: 0,
        cr3: vmm::kernel_cr3(),
        kstack_top: 0,
        user: None,
        _stack: Vec::new(),
    });
    let idle = new_thread("idle", idle_main);

    SCHED.with(|s| {
        s.threads.reserve(RESERVED_THREADS);
        s.free_slots.reserve(RESERVED_THREADS);
        s.ready.reserve(RESERVED_THREADS);
        s.sleeping.reserve(RESERVED_THREADS);
        s.zombies.reserve(RESERVED_THREADS);
        s.current = s.add(main);
        // The idle thread is never queued: it only runs when nothing else can.
        s.idle = s.add(idle);
    });
    STARTED.store(true, Ordering::Release);
}

/// Start a new thread running `entry`; returns its id.
pub fn spawn(name: &'static str, entry: fn()) -> usize {
    let thread = new_thread(name, entry);
    SCHED.with(|s| {
        let id = s.add(thread);
        s.ready.push_back(id);
        id
    })
}

/// Start a thread that runs `proc` in ring 3, with its own address space.
pub fn spawn_user(name: &'static str, proc: UserProc) -> usize {
    let mut thread = new_thread(name, crate::proc::user_thread_main);
    thread.cr3 = proc.space.pml4;
    thread.user = Some(proc);
    SCHED.with(|s| {
        let id = s.add(thread);
        s.ready.push_back(id);
        id
    })
}

/// Run `f` on the current thread's process, if it has one.
pub fn with_current_user<R>(f: impl FnOnce(&UserProc) -> R) -> Option<R> {
    SCHED.with(|s| {
        let id = s.current;
        s.thread(id).user.as_ref().map(f)
    })
}

pub fn current_user_handle() -> Option<Arc<ProcHandle>> {
    with_current_user(|u| u.handle.clone())
}

/// Called from the timer interrupt (interrupts are off, EOI already sent).
pub fn on_tick() {
    if !STARTED.load(Ordering::Acquire) {
        return;
    }
    let now = timer::ticks();
    let reschedule = SCHED.with(|s| {
        let mut i = 0;
        while i < s.sleeping.len() {
            let id = s.sleeping[i];
            if s.thread(id).wake_at <= now {
                s.sleeping.swap_remove(i);
                s.thread(id).state = State::Ready;
                s.ready.push_back(id);
            } else {
                i += 1;
            }
        }
        s.slice_left = s.slice_left.saturating_sub(1);
        if s.current == s.idle {
            !s.ready.is_empty()
        } else {
            s.slice_left == 0 && !s.ready.is_empty()
        }
    });
    if reschedule {
        schedule();
    }
}

/// Give up the rest of this thread's time slice.
pub fn yield_now() {
    let flags = irq_save();
    schedule();
    irq_restore(flags);
}

/// Block the calling thread for at least `ticks` timer ticks (minimum 1).
pub fn sleep_ticks(ticks: u64) {
    let flags = irq_save();
    SCHED.with(|s| {
        let id = s.current;
        assert!(id != s.idle, "the idle thread cannot sleep");
        let wake_at = timer::ticks() + ticks.max(1);
        let thread = s.thread(id);
        thread.state = State::Sleeping;
        thread.wake_at = wake_at;
        s.sleeping.push(id);
    });
    schedule();
    irq_restore(flags);
}

/// Park the calling thread until another thread or an interrupt handler calls
/// `wake` for it, or until the timer reaches `deadline` (if given).
///
/// Interrupts must already be disabled by the caller, and the caller must have
/// put its id where the waker will find it *before* calling this; that keeps
/// "check condition, register, sleep" atomic on one CPU, so no wake-up is lost.
/// Returning does not mean the condition holds: re-check it.
pub fn block_current(deadline: Option<u64>) {
    SCHED.with(|s| {
        let id = s.current;
        assert!(id != s.idle, "the idle thread cannot block");
        let thread = s.thread(id);
        thread.state = State::Blocked;
        if let Some(deadline) = deadline {
            thread.wake_at = deadline;
            s.sleeping.push(id);
        }
    });
    schedule();
}

/// Wake a thread parked in `block_current`. Safe to call from interrupt
/// handlers. Returns false if the thread was not blocked.
pub fn wake(id: usize) -> bool {
    SCHED.with(|s| s.wake(id))
}

pub fn sleep_ms(ms: u64) {
    sleep_ticks((ms * crate::TIMER_HZ as u64 + 999) / 1000);
}

/// Terminate the calling thread. Its stack is freed by the next thread to run.
pub fn exit() -> ! {
    irq_save(); // never restored: this thread does not run again
    SCHED.with(|s| {
        let id = s.current;
        s.thread(id).state = State::Dead;
        s.zombies.push(id);
    });
    schedule();
    unreachable!("a dead thread was scheduled");
}

pub fn current_id() -> usize {
    SCHED.with(|s| s.current)
}

pub fn current_name() -> &'static str {
    SCHED.with(|s| {
        let id = s.current;
        s.thread(id).name
    })
}

/// Threads that have not exited (including main and idle).
pub fn thread_count() -> usize {
    SCHED.with(|s| {
        s.threads
            .iter()
            .flatten()
            .filter(|t| t.state != State::Dead)
            .count()
    })
}

pub fn context_switches() -> u64 {
    SCHED.with(|s| s.switches)
}

/// Exercises preemption, yielding, sleeping, thread exit and stack reclamation.
/// Panics on any failure.
pub fn self_test() {
    use crate::sync::SpinLock;
    use core::sync::atomic::AtomicU64;

    // 1. Preemption: three threads that never yield must all make progress.
    static STOP: AtomicBool = AtomicBool::new(false);
    static COUNTS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
    fn spinner<const N: usize>() {
        while !STOP.load(Ordering::Relaxed) {
            COUNTS[N].fetch_add(1, Ordering::Relaxed);
        }
    }
    spawn("spinner0", spinner::<0>);
    spawn("spinner1", spinner::<1>);
    spawn("spinner2", spinner::<2>);
    sleep_ticks(50);
    STOP.store(true, Ordering::Relaxed);
    let counts = [
        COUNTS[0].load(Ordering::Relaxed),
        COUNTS[1].load(Ordering::Relaxed),
        COUNTS[2].load(Ordering::Relaxed),
    ];
    let min = *counts.iter().min().unwrap();
    let max = *counts.iter().max().unwrap();
    assert!(min > 0, "a spinner never ran: {:?}", counts);
    assert!(min * 10 > max, "scheduling is badly unfair: {:?}", counts);
    println!("[ok] preemption: spinners ran {:?} iterations", counts);
    sleep_ticks(20); // let them see STOP, exit and be reaped

    // 2. Sleeping: threads must wake in order of their deadlines.
    static LOG: SpinLock<Vec<u8>> = SpinLock::new(Vec::new());
    fn sleeper<const ID: u8, const TICKS: u64>() {
        sleep_ticks(TICKS);
        LOG.lock().push(ID);
    }
    spawn("sleeper0", sleeper::<0, 30>);
    spawn("sleeper1", sleeper::<1, 10>);
    spawn("sleeper2", sleeper::<2, 20>);
    sleep_ticks(50);
    assert_eq!(LOG.lock().as_slice(), &[1u8, 2, 0], "wake order is wrong");
    println!("[ok] sleep: threads woke in deadline order");

    // 3. Yielding: two threads that strictly alternate through a shared counter.
    static TURN: AtomicU64 = AtomicU64::new(0);
    fn pong<const ME: u64>() {
        for _ in 0..500 {
            while TURN.load(Ordering::Acquire) % 2 != ME {
                yield_now();
            }
            TURN.fetch_add(1, Ordering::Release);
        }
    }
    spawn("pong0", pong::<0>);
    spawn("pong1", pong::<1>);
    sleep_ticks(30);
    assert_eq!(TURN.load(Ordering::Acquire), 1000, "ping-pong did not finish");
    println!("[ok] yield: 1000 strict alternations");

    // 4. Exit and reclamation: threads run with interrupts on, can use the heap,
    //    and give their stacks back when they finish.
    static DONE: AtomicU64 = AtomicU64::new(0);
    fn quick() {
        let flags: u64;
        unsafe { asm!("pushfq", "pop {}", out(reg) flags) };
        assert!(flags & (1 << 9) != 0, "interrupts are off inside a thread");
        let numbers: Vec<u64> = (0..100).collect();
        let label = alloc::format!("{}#{}", current_name(), current_id());
        assert_eq!(numbers.iter().sum::<u64>(), 4950);
        assert!(label.starts_with("quick#"));
        DONE.fetch_add(1, Ordering::Relaxed);
    }
    sleep_ticks(10);
    let heap_before = heap::used();
    let threads_before = thread_count();
    for _ in 0..20 {
        spawn("quick", quick);
    }
    sleep_ticks(20);
    assert_eq!(DONE.load(Ordering::Relaxed), 20, "not every thread ran");
    assert_eq!(thread_count(), threads_before, "threads were not cleaned up");
    assert_eq!(heap::used(), heap_before, "thread stacks leaked");
    println!("[ok] exit: 20 threads ran and were reclaimed");

    println!(
        "[ok] scheduler self-test ({} context switches)",
        context_switches()
    );
}
