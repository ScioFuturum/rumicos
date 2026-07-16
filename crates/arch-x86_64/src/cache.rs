use core::arch::asm;
use core::arch::x86_64::{_mm_sfence, _mm_stream_si64};
use core::ops::{Deref, DerefMut};

pub const CACHE_LINE: usize = 64;

#[repr(C, align(64))]
pub struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    #[inline(always)]
    pub const fn new(value: T) -> Self {
        Self { value }
    }

    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for CachePadded<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

#[inline(always)]
/// Flush the cache line containing `ptr` with CLFLUSHOPT.
///
/// # Safety
/// Caller must ensure the CPU supports CLFLUSHOPT and that `ptr` is a valid
/// address for the current privilege level.
pub unsafe fn clflushopt<T>(ptr: *const T) {
    unsafe { asm!("clflushopt [{ptr}]", ptr = in(reg) ptr, options(nostack, preserves_flags)) };
}

#[inline(always)]
/// Write back the cache line containing `ptr` with CLWB.
///
/// # Safety
/// Caller must ensure the CPU supports CLWB and that `ptr` is a valid address
/// for the current privilege level.
pub unsafe fn clwb<T>(ptr: *const T) {
    unsafe { asm!("clwb [{ptr}]", ptr = in(reg) ptr, options(nostack, preserves_flags)) };
}

#[inline(always)]
pub fn sfence() {
    unsafe { _mm_sfence() };
}

#[inline(always)]
/// Prefetch the line containing `ptr` for write ownership.
///
/// # Safety
/// Caller must ensure the CPU supports PREFETCHW and that `ptr` is a valid
/// address to use as a prefetch hint.
pub unsafe fn prefetchw<T>(ptr: *const T) {
    unsafe {
        asm!("prefetchw [{ptr}]", ptr = in(reg) ptr, options(nostack, preserves_flags, readonly))
    };
}

#[inline(always)]
/// Prefetch the line containing `ptr` into the L1 cache with temporal intent.
///
/// # Safety
/// Caller must ensure the CPU supports PREFETCHWT1 and that `ptr` is a valid
/// address to use as a prefetch hint.
pub unsafe fn prefetchwt1<T>(ptr: *const T) {
    unsafe {
        asm!("prefetchwt1 [{ptr}]", ptr = in(reg) ptr, options(nostack, preserves_flags, readonly))
    };
}

#[inline(always)]
/// Store a u64 using a non-temporal streaming store.
///
/// # Safety
/// Caller must ensure `dst` is valid and properly aligned for a u64 store, and
/// that bypassing the normal cache hierarchy is correct for this memory range.
pub unsafe fn nt_store_u64(dst: *mut u64, value: u64) {
    unsafe { _mm_stream_si64(dst.cast::<i64>(), value as i64) };
}
