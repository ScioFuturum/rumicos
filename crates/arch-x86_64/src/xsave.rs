use core::arch::asm;

#[inline(always)]
/// Read an extended control register.
///
/// # Safety
/// Caller must ensure `index` names a valid XCR and that XGETBV is legal in the
/// current CPU state.
pub unsafe fn xgetbv(index: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!(
            "xgetbv",
            in("ecx") index,
            lateout("eax") low,
            lateout("edx") high,
            options(nomem, nostack, preserves_flags),
        )
    };
    ((high as u64) << 32) | low as u64
}

#[inline(always)]
/// Write an extended control register.
///
/// # Safety
/// Caller must ensure `index` names a valid XCR and `value` is architecturally
/// valid for the current CPU feature set.
pub unsafe fn xsetbv(index: u32, value: u64) {
    unsafe {
        asm!(
            "xsetbv",
            in("ecx") index,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        )
    };
}

#[inline(always)]
/// Save enabled extended processor state into `area`.
///
/// # Safety
/// Caller must ensure `area` points to a sufficiently large, correctly aligned
/// XSAVE area for `mask`.
pub unsafe fn xsave64(area: *mut u8, mask: u64) {
    unsafe {
        asm!(
            "xsave64 [{area}]",
            area = in(reg) area,
            in("eax") mask as u32,
            in("edx") (mask >> 32) as u32,
            options(nostack),
        )
    };
}

#[inline(always)]
/// Restore enabled extended processor state from `area`.
///
/// # Safety
/// Caller must ensure `area` points to a valid XSAVE image compatible with
/// `mask` and the current CPU state.
pub unsafe fn xrstor64(area: *const u8, mask: u64) {
    unsafe {
        asm!(
            "xrstor64 [{area}]",
            area = in(reg) area,
            in("eax") mask as u32,
            in("edx") (mask >> 32) as u32,
            options(nostack, readonly),
        )
    };
}
