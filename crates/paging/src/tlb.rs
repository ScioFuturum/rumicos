/// TLB management primitives for x86_64
///
/// Provides cache-optimized TLB invalidation routines.
/// Prefers INVPCID when available (CPUID.07H:EBX[10]).
use crate::address::VirtAddr;
use core::sync::atomic::{AtomicBool, Ordering};
use kernel_arch_x86_64::tlb::{InvpcidKind, invlpg, invpcid};

/// Whether CR4.PCIDE was actually enabled at paging init, and whether the
/// INVPCID instruction exists. QEMU's TCG fallback has neither, so the
/// flush helpers below must degrade gracefully instead of #UD'ing.
static PCID_ENABLED: AtomicBool = AtomicBool::new(false);
static INVPCID_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Record PCID/INVPCID availability. Called once by `init::init` on the BSP.
pub(crate) fn set_pcid_support(pcid: bool, invpcid: bool) {
    PCID_ENABLED.store(pcid, Ordering::Release);
    INVPCID_AVAILABLE.store(invpcid, Ordering::Release);
}

/// Whether CR4.PCIDE is enabled — i.e. whether CR3 bits 0-11 carry a PCID.
/// kernel-proc consults this before allocating per-address-space PCIDs.
pub fn pcid_enabled() -> bool {
    PCID_ENABLED.load(Ordering::Acquire)
}

/// Flush a single page from TLB using INVLPG
///
/// # SAFETY
/// - Only safe if paging is enabled (CR0.PG = 1)
/// - Does not perform LFENCE; caller responsible if needed for visibility
#[inline]
pub unsafe fn flush_page(virt: VirtAddr) {
    // SAFETY: INVLPG is always available on paging-capable CPUs
    unsafe { invlpg(virt.as_u64() as usize) };
}

/// Flush all entries for a given PCID — INVPCID when available, otherwise a
/// full flush via CR4.PGE toggle (coarser, always correct).
///
/// # SAFETY
/// - Only safe if paging is enabled and, on the INVPCID path, PCIDE is set
#[inline]
pub unsafe fn flush_pcid(pcid: u16) {
    if INVPCID_AVAILABLE.load(Ordering::Acquire) {
        // SAFETY: availability just checked.
        unsafe { invpcid(InvpcidKind::SingleContext, pcid, 0) };
    } else {
        // SAFETY: PGE is enabled at paging init on every supported CPU.
        unsafe { flush_global() };
    }
}

/// Flush all non-global TLB entries across all PCIDs — INVPCID when
/// available, otherwise a full flush via CR4.PGE toggle.
///
/// # SAFETY
/// - Only safe if paging is enabled
#[inline]
pub unsafe fn flush_all_pcids() {
    if INVPCID_AVAILABLE.load(Ordering::Acquire) {
        // SAFETY: availability just checked.
        unsafe { invpcid(InvpcidKind::AllNonGlobal, 0, 0) };
    } else {
        // SAFETY: PGE is enabled at paging init on every supported CPU.
        unsafe { flush_global() };
    }
}

/// Flush global TLB entries by toggling CR4.PGE
///
/// This is slower than INVPCID but works on all CPUs with PGE support.
/// Used as fallback on older CPUs without INVPCID.
///
/// # SAFETY
/// - Only safe if CR4.PGE is enabled
/// - Requires inline assembly to read/write CR4
#[inline]
pub unsafe fn flush_global() {
    unsafe {
        // Read CR4
        let cr4: u64;
        core::arch::asm!("mov {}, cr4", lateout(reg) cr4, options(nostack, preserves_flags));

        // Clear PGE bit
        let cr4_no_pge = cr4 & !(1u64 << 7);
        core::arch::asm!("mov cr4, {}", in(reg) cr4_no_pge, options(nostack, preserves_flags));

        // Re-enable PGE (this flushes global entries)
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));
    }
}

/// Write CR3 to load a new page table
///
/// # SAFETY
/// - Caller must ensure the new page table is properly constructed
/// - Interrupt handlers must have valid mappings before the write completes
/// - Consider using _lfence() after if other cores can observe effects
#[inline]
pub unsafe fn install_page_table(cr3: u64) {
    unsafe {
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) cr3,
            options(nostack, preserves_flags)
        );
    }
}

/// Privileged instruction wrapper: read CR0
///
/// # Safety
/// Caller must execute at a privilege level where CR0 reads are legal.
#[inline]
pub unsafe fn read_cr0() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr0", lateout(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Privileged instruction wrapper: read CR2 (fault address)
///
/// # Safety
/// Caller must execute at a privilege level where CR2 reads are legal.
#[inline]
pub unsafe fn read_cr2() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", lateout(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Privileged instruction wrapper: read CR3 (page table base)
///
/// # Safety
/// Caller must execute at a privilege level where CR3 reads are legal.
#[inline]
pub unsafe fn read_cr3() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr3", lateout(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Privileged instruction wrapper: read CR4
///
/// # Safety
/// Caller must execute at a privilege level where CR4 reads are legal.
#[inline]
pub unsafe fn read_cr4() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr4", lateout(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Privileged instruction wrapper: write CR4
///
/// # Safety
/// Caller must ensure `value` is architecturally valid and that changing CR4
/// will not invalidate currently executing code/data mappings.
#[inline]
pub unsafe fn write_cr4(value: u64) {
    unsafe {
        core::arch::asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

/// The BSP's final CR4 (PGE/PCIDE/SMEP/SMAP as probed and enabled during
/// paging init), published for APs to mirror. `0` = not published yet.
static BSP_CR4: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Record the BSP's final CR4. Called once at the end of paging init.
pub fn publish_bsp_cr4(cr4: u64) {
    BSP_CR4.store(cr4, core::sync::atomic::Ordering::Release);
}

/// Mirror the BSP's published CR4 onto the calling AP.
///
/// The AP trampoline leaves CR4 at PAE-only; every feature bit the kernel
/// relies on (SMAP for its STAC/CLAC user-copy discipline, SMEP, PGE,
/// PCIDE for the tagged-TLB plumbing) must be replayed here or the AP
/// #UDs on its first STAC / runs with silently different TLB semantics.
/// Safe to call before the PCID-tagged CR3 is ever loaded on this AP:
/// the trampoline's CR3 has zero low bits, satisfying the PCIDE
/// enable-time constraint.
///
/// # Safety
/// Must run on an AP during bring-up, after the BSP published its CR4 and
/// before this AP first enters user mode or touches user memory.
pub unsafe fn apply_bsp_cr4_on_ap() {
    let cr4 = BSP_CR4.load(core::sync::atomic::Ordering::Acquire);
    if cr4 != 0 {
        // SAFETY: caller guarantees AP bring-up context; the value was a
        // valid CR4 on the BSP of this same homogeneous system.
        unsafe { write_cr4(cr4) };
    }
}

/// Privileged instruction wrapper: read EFER MSR
///
/// # Safety
/// Caller must execute at a privilege level where EFER reads are legal.
#[inline]
pub unsafe fn read_efer() -> u64 {
    // SAFETY: Caller guarantees this privileged MSR read is valid in the current CPU mode.
    unsafe { kernel_arch_x86_64::msr::rdmsr(kernel_arch_x86_64::msr::IA32_EFER) }
}

/// Privileged instruction wrapper: write EFER MSR
///
/// # Safety
/// Caller must ensure `value` is a valid EFER value for this CPU.
#[inline]
pub unsafe fn write_efer(value: u64) {
    // SAFETY: Caller guarantees this privileged MSR write uses a valid EFER value.
    unsafe { kernel_arch_x86_64::msr::wrmsr(kernel_arch_x86_64::msr::IA32_EFER, value) };
}

/// Memory fence to ensure previous memory operations are visible
#[inline]
pub fn mfence() {
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
}

/// LFENCE to ensure previous memory operations are visible (lightweight)
#[inline]
pub fn lfence() {
    unsafe { core::arch::asm!("lfence", options(nostack, preserves_flags)) };
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_cr3_layout() {
        // Just ensure functions compile without errors
        // Runtime testing requires actual paging setup
    }
}
