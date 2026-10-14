use core::arch::asm;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

const RFLAGS_IF: u64 = 1 << 9;

/// Disable interrupts and return the previous RFLAGS for `irq_restore`.
pub fn irq_save() -> u64 {
    let flags: u64;
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags) };
    flags
}

/// Re-enable interrupts if they were enabled when `irq_save` returned `flags`.
pub fn irq_restore(flags: u64) {
    if flags & RFLAGS_IF != 0 {
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
}

/// Data only touched with interrupts disabled. On a single CPU that gives
/// exclusive access without a lock, which matters for the scheduler: a thread
/// switch happens while the data is in use, and a spinlock owned by a
/// suspended thread would deadlock whoever runs next.
///
/// `with` must never be nested on the same cell.
pub struct IrqCell<T>(UnsafeCell<T>);

unsafe impl<T: Send> Sync for IrqCell<T> {}

impl<T> IrqCell<T> {
    pub const fn new(value: T) -> Self {
        IrqCell(UnsafeCell::new(value))
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let flags = irq_save();
        let result = f(unsafe { &mut *self.0.get() });
        irq_restore(flags);
        result
    }
}

/// Spinlock that also disables interrupts while held, so an interrupt handler
/// can never deadlock against the code it interrupted on the same CPU.
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    interrupts_were_enabled: bool,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Break the lock without a guard. Only for crash paths (panic, fatal
    /// exception) that must print even if the faulting code held the lock.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    pub fn lock(&self) -> SpinGuard<'_, T> {
        let flags = irq_save();
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard {
            lock: self,
            interrupts_were_enabled: flags & RFLAGS_IF != 0,
        }
    }
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.interrupts_were_enabled {
            irq_restore(RFLAGS_IF);
        }
    }
}
