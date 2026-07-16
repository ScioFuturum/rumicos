use core::arch::asm;

pub const IA32_APIC_BASE: u32 = 0x0000_001b;
pub const IA32_EFER: u32 = 0xc000_0080;
pub const IA32_STAR: u32 = 0xc000_0081;
pub const IA32_LSTAR: u32 = 0xc000_0082;
pub const IA32_FMASK: u32 = 0xc000_0084;
pub const IA32_FS_BASE: u32 = 0xc000_0100;
pub const IA32_GS_BASE: u32 = 0xc000_0101;
pub const IA32_KERNEL_GS_BASE: u32 = 0xc000_0102;
pub const IA32_TSC_AUX: u32 = 0xc000_0103;
pub const IA32_X2APIC_BASE: u32 = 0x0000_0800;

#[inline(always)]
/// Read a model-specific register.
///
/// # Safety
/// Caller must ensure `msr` is valid on this CPU and that reading it is legal
/// at the current privilege level.
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            lateout("eax") low,
            lateout("edx") high,
            options(nomem, nostack, preserves_flags),
        )
    };
    ((high as u64) << 32) | low as u64
}

#[inline(always)]
/// Write a model-specific register.
///
/// # Safety
/// Caller must ensure `msr` is valid on this CPU and `value` satisfies the
/// architectural constraints for that register.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        )
    };
}

#[inline(always)]
/// Read GSBASE with FSGSBASE.
///
/// # Safety
/// Caller must ensure FSGSBASE is enabled and available on this CPU.
pub unsafe fn rdgsbase() -> u64 {
    let value: u64;
    unsafe {
        asm!("rdgsbase {value}", value = lateout(reg) value, options(nomem, nostack, preserves_flags))
    };
    value
}

#[inline(always)]
/// Write GSBASE with FSGSBASE.
///
/// # Safety
/// Caller must ensure FSGSBASE is enabled and that `value` is a canonical
/// address appropriate for GS-relative accesses.
pub unsafe fn wrgsbase(value: u64) {
    unsafe {
        asm!("wrgsbase {value}", value = in(reg) value, options(nomem, nostack, preserves_flags))
    };
}

#[inline(always)]
/// Read FSBASE with FSGSBASE.
///
/// # Safety
/// Caller must ensure FSGSBASE is enabled and available on this CPU.
pub unsafe fn rdfsbase() -> u64 {
    let value: u64;
    unsafe {
        asm!("rdfsbase {value}", value = lateout(reg) value, options(nomem, nostack, preserves_flags))
    };
    value
}

#[inline(always)]
/// Write FSBASE with FSGSBASE.
///
/// # Safety
/// Caller must ensure FSGSBASE is enabled and that `value` is a canonical
/// address appropriate for FS-relative accesses.
pub unsafe fn wrfsbase(value: u64) {
    unsafe {
        asm!("wrfsbase {value}", value = in(reg) value, options(nomem, nostack, preserves_flags))
    };
}
