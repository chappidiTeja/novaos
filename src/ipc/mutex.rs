//! Blocking mutex and counting semaphore. Waiters sleep instead of spinning, so
//! these may be held across blocking calls. Thread context only, never in an
//! interrupt handler (use `SpinLock` there).

use super::waitq::WaitQueue;
use crate::timer;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub struct Mutex<T> {
    locked: AtomicBool,
    waiters: WaitQueue,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Mutex {
            locked: AtomicBool::new(false),
            waiters: WaitQueue::new(),
            data: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.waiters.wait_until(|| {
            self.locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        });
        MutexGuard { mutex: self }
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
        self.mutex.waiters.wake_one();
    }
}

pub struct Semaphore {
    count: AtomicUsize,
    waiters: WaitQueue,
}

impl Semaphore {
    pub const fn new(count: usize) -> Self {
        Semaphore {
            count: AtomicUsize::new(count),
            waiters: WaitQueue::new(),
        }
    }

    fn try_take(&self) -> bool {
        let mut current = self.count.load(Ordering::Relaxed);
        while current > 0 {
            match self.count.compare_exchange_weak(
                current,
                current - 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
        false
    }

    /// Take one unit, sleeping until one is available.
    pub fn down(&self) {
        self.waiters.wait_until(|| self.try_take());
    }

    /// Like `down`, but gives up after `ticks` timer ticks. True if a unit was taken.
    pub fn down_timeout(&self, ticks: u64) -> bool {
        let deadline = timer::ticks() + ticks;
        self.waiters
            .wait_until_deadline(deadline, || self.try_take())
    }

    /// Release one unit, waking a waiter.
    pub fn up(&self) {
        self.count.fetch_add(1, Ordering::Release);
        self.waiters.wake_one();
    }
}
