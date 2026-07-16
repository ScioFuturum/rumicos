use crate::cycles::pause;
use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{Ordering, compiler_fence};

#[repr(C, align(16))]
pub struct AtomicU128 {
    value: UnsafeCell<u128>,
}

unsafe impl Send for AtomicU128 {}
unsafe impl Sync for AtomicU128 {}

impl AtomicU128 {
    #[inline(always)]
    pub const fn new(value: u128) -> Self {
        Self {
            value: UnsafeCell::new(value),
        }
    }

    #[inline(always)]
    pub fn load(&self, order: Ordering) -> u128 {
        fence_for_load(order);
        let value = unsafe { atomic_load_128(self.value.get()) };
        fence_for_load(order);
        value
    }

    #[inline(always)]
    pub fn store(&self, value: u128, order: Ordering) {
        loop {
            let current = self.load(Ordering::Acquire);
            if self
                .compare_exchange(current, value, order, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            pause();
        }
    }

    #[inline(always)]
    pub fn compare_exchange(
        &self,
        current: u128,
        new: u128,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u128, u128> {
        fence_for_store(success);
        let (matched, observed) = unsafe { cmpxchg16b(self.value.get(), current, new) };
        if matched {
            fence_for_load(success);
            Ok(observed)
        } else {
            fence_for_load(failure);
            Err(observed)
        }
    }
}

#[inline(always)]
fn fence_for_load(order: Ordering) {
    match order {
        Ordering::Acquire | Ordering::AcqRel | Ordering::SeqCst => {
            compiler_fence(Ordering::Acquire)
        }
        Ordering::Relaxed | Ordering::Release => {}
        _ => compiler_fence(Ordering::Acquire),
    }
}

#[inline(always)]
fn fence_for_store(order: Ordering) {
    match order {
        Ordering::Release | Ordering::AcqRel | Ordering::SeqCst => {
            compiler_fence(Ordering::Release)
        }
        Ordering::Relaxed | Ordering::Acquire => {}
        _ => compiler_fence(Ordering::Release),
    }
}

#[inline(always)]
unsafe fn atomic_load_128(addr: *mut u128) -> u128 {
    let (_matched, observed) = unsafe { cmpxchg16b(addr, 0, 0) };
    observed
}

#[inline(always)]
unsafe fn cmpxchg16b(addr: *mut u128, current: u128, new: u128) -> (bool, u128) {
    let mut low = current as u64;
    let mut high = (current >> 64) as u64;
    let new_low = new as u64;
    let new_high = (new >> 64) as u64;
    let matched: u8;

    unsafe {
        asm!(
            "push rbx",
            "mov rbx, {new_low}",
            "lock cmpxchg16b [{addr}]",
            "setz {matched}",
            "pop rbx",
            addr = in(reg) addr,
            new_low = in(reg) new_low,
            inout("rax") low,
            inout("rdx") high,
            in("rcx") new_high,
            matched = lateout(reg_byte) matched,
        );
    }

    let observed = ((high as u128) << 64) | low as u128;
    (matched != 0, observed)
}
