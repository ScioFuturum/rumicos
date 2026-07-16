use core::arch::asm;
use core::arch::x86_64::{__rdtscp, _mm_lfence, _mm_pause, _rdtsc};

#[inline(always)]
pub fn pause() {
    _mm_pause();
}

#[inline(always)]
/// Halt the current CPU until the next external interrupt.
///
/// # Safety
/// Caller must ensure halting is valid in the current CPU state and will not
/// deadlock the system.
pub unsafe fn halt() {
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

#[inline(always)]
pub fn rdtsc_ordered() -> u64 {
    unsafe {
        _mm_lfence();
        _rdtsc()
    }
}

#[inline(always)]
pub fn rdtscp_ordered() -> (u64, u32) {
    let mut aux = 0u32;
    let cycles = unsafe { __rdtscp(&mut aux) };
    unsafe { _mm_lfence() };
    (cycles, aux)
}

#[inline(always)]
pub fn spin_n(iterations: u32) {
    for _ in 0..iterations {
        pause();
    }
}
