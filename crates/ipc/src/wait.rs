use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};
use kernel_arch_x86_64::cycles::pause;

#[inline(always)]
/// Arm MONITOR for the cache line containing `addr`.
///
/// # Safety
/// Caller must ensure MONITOR/MWAIT is supported and enabled, and that `addr`
/// is a valid monitored address.
pub unsafe fn monitor<T>(addr: *const T) {
    unsafe {
        asm!(
            "monitor",
            in("rax") addr,
            in("ecx") 0u32,
            in("edx") 0u32,
            options(nostack, preserves_flags),
        )
    };
}

#[inline(always)]
/// Enter MWAIT with the supplied hints.
///
/// # Safety
/// Caller must ensure MONITOR has been armed appropriately and that executing
/// MWAIT is valid at the current privilege level.
pub unsafe fn mwait(hints: u32, extensions: u32) {
    unsafe {
        asm!(
            "mwait",
            in("eax") hints,
            in("ecx") extensions,
            options(nostack, preserves_flags),
        )
    };
}

#[inline(always)]
pub fn wait_flag_change(flag: &AtomicU32, old: u32, spins: u32) -> u32 {
    for _ in 0..spins {
        let observed = flag.load(Ordering::Acquire);
        if observed != old {
            return observed;
        }
        pause();
    }
    flag.load(Ordering::Acquire)
}
