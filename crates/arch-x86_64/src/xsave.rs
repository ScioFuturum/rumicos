use core::arch::asm;

// ─── FPU/SSE/XSAVE enable constants and helpers ────────────────────────────

/// CR4.OSFXSR (bit 9): enables SSE and FXSAVE/FXRSTOR.
pub const CR4_OSFXSR: u64 = 1 << 9;
/// CR4.OSXMMEXCPT (bit 10): unmasked SIMD exceptions raise #XM.
pub const CR4_OSXMMEXCPT: u64 = 1 << 10;
/// CR4.OSXSAVE (bit 18): enables XSAVE/XRSTOR and XGETBV/XSETBV.
pub const CR4_OSXSAVE: u64 = 1 << 18;
/// All CR4 bits this kernel needs for userspace SSE + XSAVE.
pub const CR4_FPU_BITS: u64 = CR4_OSFXSR | CR4_OSXMMEXCPT | CR4_OSXSAVE;

/// XCR0 feature mask this kernel enables: x87 (bit 0) + SSE (bit 1). Both are
/// mandatory whenever XSAVE is present, so enabling exactly them needs no
/// CPUID gate; the per-thread save area is sized for them. AVX (bit 2) and
/// beyond are deliberately NOT enabled — doing so would enlarge the required
/// save area (see [`xsave_area_size`]) without any user for it yet.
pub const XCR0_X87_SSE: u64 = 0b11;

/// XSAVE-area byte size x87+SSE state requires: a 512-byte legacy region plus
/// the 64-byte XSAVE header. `CPUID.0DH:EBX` reports this for the enabled
/// XCR0 at runtime (see [`xsave_area_size`]); it is 576 on every x86-64 CPU
/// for exactly x87+SSE.
pub const XSAVE_X87_SSE_SIZE: usize = 576;

/// Program XCR0 to manage x87 + SSE state.
///
/// # Safety
/// Run once per CPU during init, AFTER `CR4.OSXSAVE` is set (otherwise
/// XSETBV faults). Enables exactly [`XCR0_X87_SSE`].
pub unsafe fn enable_xcr0_x87_sse() {
    // SAFETY: caller guarantees CR4.OSXSAVE is set; bits 0/1 are always valid.
    unsafe { xsetbv(0, XCR0_X87_SSE) };
}

/// `CPUID.0DH:EBX` — the XSAVE-area size in bytes for the currently-enabled
/// XCR0 features. Callers assert their fixed save area is at least this big.
///
/// # Safety
/// `CR4.OSXSAVE` must be set and XCR0 programmed first.
pub unsafe fn xsave_area_size() -> u32 {
    // __cpuid_count is safe to execute on any x86-64 CPU; leaf 0x0D sub-leaf
    // 0 returns the enabled-feature save size in EBX. (This fn stays `unsafe`
    // to document that the value is only meaningful once CR4.OSXSAVE is set
    // and XCR0 is programmed.)
    core::arch::x86_64::__cpuid_count(0x0D, 0).ebx
}

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
