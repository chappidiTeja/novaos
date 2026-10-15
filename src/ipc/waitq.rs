//! Wait queues: the one blocking primitive everything else is built on.

use crate::sched;
use crate::sync::{irq_restore, irq_save, SpinLock};
use crate::timer;
use alloc::collections::VecDeque;

/// Threads sleeping until some condition becomes true.
///
/// The waiting side checks its condition and registers itself with interrupts
/// disabled; the waking side changes the state and then calls `wake_one` or
/// `wake_all`. On a single CPU nothing can run in between, so a wake-up cannot
/// slip through the gap between "condition is false" and "go to sleep".
pub struct WaitQueue {
    waiters: SpinLock<VecDeque<usize>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        WaitQueue {
            waiters: SpinLock::new(VecDeque::new()),
        }
    }

    /// Block until `cond` returns true. `cond` runs with interrupts disabled, so
    /// what it inspects cannot change under it, and it may consume what it
    /// finds (take an item, claim a lock). It must not block or take a
    /// `Mutex`. Never call this from an interrupt handler.
    pub fn wait_until(&self, mut cond: impl FnMut() -> bool) {
        let flags = irq_save();
        let me = sched::current_id();
        while !cond() {
            self.waiters.lock().push_back(me);
            sched::block_current(None);
        }
        irq_restore(flags);
    }

    /// Like `wait_until`, but gives up once the tick counter reaches `deadline`.
    /// Returns true if the condition was met, false on timeout.
    pub fn wait_until_deadline(&self, deadline: u64, mut cond: impl FnMut() -> bool) -> bool {
        let flags = irq_save();
        let me = sched::current_id();
        let met = loop {
            if cond() {
                break true;
            }
            if timer::ticks() >= deadline {
                break false;
            }
            self.waiters.lock().push_back(me);
            sched::block_current(Some(deadline));
            // Woken by timeout rather than by a waker: our entry is still queued.
            let mut waiters = self.waiters.lock();
            waiters.retain(|&id| id != me);
            if waiters.is_empty() {
                waiters.shrink_to_fit();
            }
        };
        irq_restore(flags);
        met
    }


    /// Like `wait_until`, but gives up (returning false) when the calling process has been sent
    /// a signal that is going to end it. Callers must tolerate an early return: the process is
    /// finished off as its system call ends, so whatever they return is never looked at.
    /// Kernel threads are never interrupted.
    pub fn wait_until_intr(&self, mut cond: impl FnMut() -> bool) -> bool {
        let flags = irq_save();
        let me = sched::current_id();
        let met = loop {
            if cond() {
                break true;
            }
            if crate::proc::fatal_signal_pending() {
                break false;
            }
            self.waiters.lock().push_back(me);
            sched::block_current(None);
            let mut waiters = self.waiters.lock();
            waiters.retain(|&id| id != me);
            if waiters.is_empty() {
                waiters.shrink_to_fit();
            }
        };
        irq_restore(flags);
        met
    }

    /// `wait_until_deadline` that a fatal signal also ends (see `wait_until_intr`).
    pub fn wait_until_deadline_intr(&self, deadline: u64, mut cond: impl FnMut() -> bool) -> bool {
        let flags = irq_save();
        let me = sched::current_id();
        let met = loop {
            if cond() {
                break true;
            }
            if timer::ticks() >= deadline || crate::proc::fatal_signal_pending() {
                break false;
            }
            self.waiters.lock().push_back(me);
            sched::block_current(Some(deadline));
            let mut waiters = self.waiters.lock();
            waiters.retain(|&id| id != me);
            if waiters.is_empty() {
                waiters.shrink_to_fit();
            }
        };
        irq_restore(flags);
        met
    }

    /// Wake the longest-waiting thread, if any. Safe in interrupt handlers.
    pub fn wake_one(&self) -> bool {
        loop {
            let next = {
                let mut waiters = self.waiters.lock();
                let next = waiters.pop_front();
                if waiters.is_empty() {
                    // Idle queues should not hold memory.
                    waiters.shrink_to_fit();
                }
                next
            };
            match next {
                Some(id) => {
                    if sched::wake(id) {
                        return true;
                    }
                }
                None => return false,
            }
        }
    }

    /// Wake every waiting thread. Safe in interrupt handlers.
    pub fn wake_all(&self) {
        let all = {
            let mut waiters = self.waiters.lock();
            let all = core::mem::take(&mut *waiters);
            all
        };
        for id in all {
            sched::wake(id);
        }
    }
}
