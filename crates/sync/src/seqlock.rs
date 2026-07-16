use crate::{Backoff, SpinLock};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering, compiler_fence};

#[repr(C, align(64))]
pub struct SeqLock<T: Copy> {
    sequence: AtomicU64,
    writer: SpinLock<()>,
    value: UnsafeCell<T>,
}

unsafe impl<T: Copy + Send> Send for SeqLock<T> {}
unsafe impl<T: Copy + Send + Sync> Sync for SeqLock<T> {}

impl<T: Copy> SeqLock<T> {
    #[inline(always)]
    pub const fn new(value: T) -> Self {
        Self {
            sequence: AtomicU64::new(0),
            writer: SpinLock::new(()),
            value: UnsafeCell::new(value),
        }
    }

    #[inline(always)]
    pub fn read(&self) -> T {
        let mut backoff = Backoff::new();
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                backoff.snooze();
                continue;
            }

            let value = unsafe { ptr::read_volatile(self.value.get()) };
            compiler_fence(Ordering::Acquire);
            let after = self.sequence.load(Ordering::Acquire);
            if before == after {
                return value;
            }
            backoff.snooze();
        }
    }

    #[inline(always)]
    pub fn write(&self, f: impl FnOnce(&mut T)) {
        let _guard = self.writer.lock();
        self.sequence.fetch_add(1, Ordering::Release);
        compiler_fence(Ordering::Release);
        f(unsafe { &mut *self.value.get() });
        compiler_fence(Ordering::Release);
        self.sequence.fetch_add(1, Ordering::Release);
    }
}
