//! Runtime MMIO mapping into the kernel's direct-map region.
//!
//! The boot-time direct map (`init::map_direct_memory`) only covers RAM and
//! the framebuffer; the fixed APIC MMIO window is added by
//! `init::map_apic_mmio`. PCI device BARs, however, are assigned by firmware
//! at addresses in the PCI MMIO hole that Limine reports as reserved — so
//! they are not mapped, and a driver that touches `DIRECT_MAP_BASE + bar`
//! would fault.
//!
//! [`map_mmio_region`] fills that gap: it maps a physical MMIO range into the
//! live kernel page tables at `DIRECT_MAP_BASE + phys`, 4 KiB at a time, as
//! uncacheable (write-through) no-execute pages.
//!
//! ## Why it is safe to add to the *live* tables
//!
//! The mapping is inserted under `pml4[256]` — the kernel half, which is
//! shared by construction: `AddressSpace::new` copies the kernel-half PML4
//! entries `[256, 512)` shallowly, so every process points at the same
//! lower-level tables. As long as this runs during early boot **before any
//! user `AddressSpace` is created** (virtio init happens well before
//! `Process::create`), the new pages are visible in every address space that
//! is created afterwards. The direct-map PDPT under `pml4[256]` already
//! exists, so this only ever adds PD/PT levels beneath it — never a new
//! top-level entry that a prior copy would have missed.

use crate::allocator::alloc_frame;
use crate::entry::{PageFlags, PageTableEntry};
use crate::table::PageTable;
use crate::{PhysAddr, VirtAddr, current_pml4_phys, flush_page};

/// Must match the value used throughout kernel-paging / kernel-proc.
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;
const PAGE_SIZE: u64 = 4096;

#[inline]
fn direct_map_table(phys: u64) -> *mut PageTable {
    (DIRECT_MAP_BASE + phys) as *mut PageTable
}

/// Ensure the next-level table under `table[index]` exists, allocating and
/// zeroing it if absent; return its physical address.
///
/// # Safety
/// `table` must point at a live page table reachable through the direct map.
unsafe fn ensure_next_table(table: *mut PageTable, index: usize) -> u64 {
    // SAFETY: caller guarantees `table` is a live, direct-mapped page table.
    let entry = unsafe { (*table).get(index) };
    if entry.is_present() {
        return entry.frame().as_u64();
    }
    // `alloc_frame` yields a kernel_memory::PhysAddr; kernel-paging has its
    // own PhysAddr type, so bridge through the raw u64.
    let frame_u64 = alloc_frame().as_u64();
    let next = direct_map_table(frame_u64);
    // SAFETY: freshly allocated frame, visible through the direct map.
    unsafe { (*next).zero() };
    // Intermediate kernel tables: present + writable, no user bit.
    let flags = PageFlags::new().with_present().with_writable();
    // SAFETY: `table` is live; we just built a valid next-level table.
    unsafe { (*table).set(index, PageTableEntry::new_page(PhysAddr::new(frame_u64), flags)) };
    frame_u64
}

/// Map `[phys_base, phys_base + size)` as uncacheable MMIO at
/// `DIRECT_MAP_BASE + phys` in the live kernel address space, using 4 KiB
/// pages. Rounds the range out to page boundaries. Safe to call more than
/// once for overlapping ranges (a leaf is simply rewritten).
///
/// # Safety
/// `phys_base` must name real device MMIO (a PCI BAR). Must run on the BSP
/// during early boot, before any user `AddressSpace` is created — see the
/// module docs for why that ordering is what makes the mapping shared.
pub unsafe fn map_mmio_region(phys_base: u64, size: u64) {
    if size == 0 {
        return;
    }
    let start = phys_base & !(PAGE_SIZE - 1);
    let end = (phys_base + size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let pml4 = direct_map_table(current_pml4_phys());
    let leaf_flags = PageFlags::new()
        .with_present()
        .with_writable()
        .with_global()
        .with_write_through()
        .with_no_execute();

    let mut phys = start;
    while phys < end {
        let virt = DIRECT_MAP_BASE + phys;
        let pml4_i = ((virt >> 39) & 0x1ff) as usize;
        let pdpt_i = ((virt >> 30) & 0x1ff) as usize;
        let pd_i = ((virt >> 21) & 0x1ff) as usize;
        let pt_i = ((virt >> 12) & 0x1ff) as usize;

        // SAFETY: pml4 is the live top-level table; ensure_next_table builds
        // and returns each present lower level, all reached via the direct map.
        unsafe {
            let pdpt = direct_map_table(ensure_next_table(pml4, pml4_i));
            let pd = direct_map_table(ensure_next_table(pdpt, pdpt_i));
            let pt = direct_map_table(ensure_next_table(pd, pd_i));
            (*pt).set(pt_i, PageTableEntry::new_page(PhysAddr::new(phys), leaf_flags));
        }
        // New mapping: flush any stale not-present cached translation locally.
        // SAFETY: valid VA in the kernel half we just populated.
        unsafe { flush_page(VirtAddr::new(virt)) };
        phys += PAGE_SIZE;
    }
}
