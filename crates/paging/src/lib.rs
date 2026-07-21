#![no_std]
#![cfg(target_arch = "x86_64")]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod address;
pub mod allocator;
pub mod entry;
pub mod init;
pub mod mmio;
pub mod table;
pub mod tlb;

pub use address::{PhysAddr, VirtAddr};
pub use allocator::{
    BumpAllocator, bump_consumed_range, set_global_frame_allocator, set_global_numa_buddy_allocator,
};
pub use entry::{PageFlags, PageTableEntry, make_cr3};
pub use init::init;
pub use table::PageTable;
pub use tlb::{flush_all_pcids, flush_global, flush_page, flush_pcid};

pub const PTE_COW: u64 = PageFlags::COW;

/// Returns the physical address of the currently loaded PML4.
#[inline]
pub fn current_pml4_phys() -> u64 {
    let cr3: u64;
    // SAFETY: reading CR3 is valid in kernel mode; masking strips PCID/flags.
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack)) };
    pml4_phys_from_cr3(cr3)
}

#[inline]
pub const fn pml4_phys_from_cr3(cr3: u64) -> u64 {
    cr3 & !0xfff
}

/*
Kernel paging bring-up for x86_64

Performance notes:
- BumpAllocator uses a SpinLock-protected static array for minimal overhead during boot.
- Page table walks cached in CPU TLB; INVLPG/INVPCID used for targeted invalidations.
- Direct physical map uses 1 GiB pages where possible (best TLB coverage).
- Global pages (G bit) skip TLB invalidation on CR3 reload, saving cycles.
- PCID (Process-Context Identifier) enables parallel per-ASID TLB entries on capable CPUs.

Next steps:
- LA57 (5-level paging) support with CR4.LA57 = 1
- MTRR/PAT for proper framebuffer memory typing (UC-, WC)
- NumaBuddy integration for persistent allocator
- Huge-page promotion logic in NumaBuddy
- KASLR (kernel address space layout randomization)
*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_pml4_phys_masks_cr3_low_bits() {
        assert_eq!(
            pml4_phys_from_cr3(0x1234_5678_abcd_1fff),
            0x1234_5678_abcd_1000
        );
    }
}
