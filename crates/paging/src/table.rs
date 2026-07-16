/// 4-level page table implementation for x86_64
///
/// Manages a single 4-level page table hierarchy (PML4 -> PDPT -> PD -> PT).
/// Lazy allocation of intermediate tables via BumpAllocator.
use crate::address::{PhysAddr, VirtAddr};
use crate::allocator::alloc_frame;
use crate::entry::{PageFlags, PageTableEntry};
use core::ptr::NonNull;

/// Page table level: 4 (PML4), 3 (PDPT), 2 (PD), 1 (PT)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageTableLevel {
    /// PML4 (top level, contains 512 entries)
    PML4 = 4,
    /// PDPT (2 GiB mappings via huge pages)
    PDPT = 3,
    /// PD (2 MiB page directory, contains 2 MiB mappings)
    PD = 2,
    /// PT (4 KiB page table, contains 4 KiB mappings)
    PT = 1,
}

/// Single 4 KiB page table (512 entries × 8 bytes)
#[repr(C, align(4096))]
pub struct PageTable {
    entries: [PageTableEntry; 512],
}

impl PageTable {
    /// Number of entries per table
    pub const ENTRIES: usize = 512;

    /// Get entry by index
    #[inline]
    pub const fn get(&self, index: usize) -> PageTableEntry {
        if index >= 512 {
            panic!("Page table index out of bounds");
        }
        self.entries[index]
    }

    /// Set entry by index
    #[inline]
    pub fn set(&mut self, index: usize, entry: PageTableEntry) {
        if index >= 512 {
            panic!("Page table index out of bounds");
        }
        self.entries[index] = entry;
    }

    /// Get mutable reference to entry for in-place modification
    #[inline]
    pub fn get_mut(&mut self, index: usize) -> &mut PageTableEntry {
        if index >= 512 {
            panic!("Page table index out of bounds");
        }
        &mut self.entries[index]
    }

    /// Zero-initialize a page table
    pub fn zero(&mut self) {
        for entry in &mut self.entries {
            *entry = PageTableEntry::empty();
        }
    }

    /// Get physical address of this page table
    pub fn phys_addr(&self) -> PhysAddr {
        PhysAddr::new(self as *const _ as u64)
    }
}

/// Page table walker for 4-level paging
///
/// Walks the live page tables from CR3 to find mappings for debugging.
pub struct PageTableWalker {
    pml4_phys: PhysAddr,
    direct_map_base: VirtAddr,
}

impl PageTableWalker {
    /// Create a walker from PML4 physical address and direct-map base
    pub fn new(pml4_phys: PhysAddr, direct_map_base: VirtAddr) -> Self {
        Self {
            pml4_phys,
            direct_map_base,
        }
    }

    #[inline]
    fn phys_to_virt(&self, phys: PhysAddr) -> *const PageTable {
        (self.direct_map_base.as_u64() + phys.as_u64()) as *const PageTable
    }

    /// Walk the page table for a virtual address, returning the physical address if mapped
    ///
    /// # SAFETY
    /// - Requires that the PML4 address points to valid page tables
    /// - Requires that paging is enabled
    /// - Must have valid mappings to read page tables
    pub unsafe fn translate(&self, virt: VirtAddr) -> Option<PhysAddr> {
        // Read PML4 via direct map
        let pml4_ptr = self.phys_to_virt(self.pml4_phys);
        let pml4 = unsafe { &*pml4_ptr };

        let pml4_idx = virt.pml4_index();
        let pml4_entry = pml4.get(pml4_idx);

        if !pml4_entry.is_present() {
            return None;
        }

        // Read PDPT
        let pdpt_ptr = self.phys_to_virt(pml4_entry.frame());
        let pdpt = unsafe { &*pdpt_ptr };

        let pdpt_idx = virt.pdpt_index();
        let pdpt_entry = pdpt.get(pdpt_idx);

        if !pdpt_entry.is_present() {
            return None;
        }

        // Check for 1 GiB page
        if pdpt_entry.is_huge_page() {
            let page_base = pdpt_entry.frame();
            let offset = virt.as_u64() & 0x3FFF_FFFF;
            return Some(page_base + offset);
        }

        // Read PD
        let pd_ptr = self.phys_to_virt(pdpt_entry.frame());
        let pd = unsafe { &*pd_ptr };

        let pd_idx = virt.pd_index();
        let pd_entry = pd.get(pd_idx);

        if !pd_entry.is_present() {
            return None;
        }

        // Check for 2 MiB page
        if pd_entry.is_huge_page() {
            let page_base = pd_entry.frame();
            let offset = virt.as_u64() & 0x1F_FFFF;
            return Some(page_base + offset);
        }

        // Read PT
        let pt_ptr = self.phys_to_virt(pd_entry.frame());
        let pt = unsafe { &*pt_ptr };

        let pt_idx = virt.pt_index();
        let pt_entry = pt.get(pt_idx);

        if !pt_entry.is_present() {
            return None;
        }

        // Return 4 KiB page address with offset
        let page_base = pt_entry.frame();
        let offset = virt.page_offset();
        Some(page_base + offset)
    }
}

use crate::allocator::{AllocatorRef, get_global_frame_allocator};

/// High-level page table builder
pub struct PageTableBuilder {
    pml4: NonNull<PageTable>,
    pml4_phys: PhysAddr,
    allocator: Option<AllocatorRef>,
}

/// Writable pointer to the page-table frame at `phys` — via the kernel-image
/// mapping for early-boot pool frames, via the direct map otherwise. The
/// pre-2026-07-10 code cast the "physical" address itself to a pointer,
/// which only ever appeared to make sense because the bump allocator was
/// (incorrectly) handing out virtual addresses; see allocator.rs.
fn table_ptr(phys: PhysAddr) -> *mut PageTable {
    crate::allocator::boot_table_va(phys.as_u64()) as *mut PageTable
}

impl PageTableBuilder {
    /// Create a new page table builder with the global bump allocator
    pub fn new() -> Self {
        let pml4_frame = PhysAddr::from_memory_addr(alloc_frame());

        // SAFETY: We just allocated this frame, and it's 4 KiB aligned
        let pml4 = unsafe {
            let mut pml4 = NonNull::new_unchecked(table_ptr(pml4_frame));
            pml4.as_mut().zero();
            pml4
        };

        Self {
            pml4,
            pml4_phys: pml4_frame,
            allocator: None,
        }
    }

    /// Create a new page table builder with the global frame allocator
    pub fn new_with_global() -> Self {
        let allocator = get_global_frame_allocator().expect("Global frame allocator not set");
        Self::with_allocator(allocator)
    }

    /// Create a new page table builder with a custom allocator
    pub fn with_allocator(mut allocator: AllocatorRef) -> Self {
        let pml4_frame = allocator.alloc().expect("Failed to allocate PML4 frame");
        let pml4_frame = PhysAddr::from_memory_addr(pml4_frame);

        // SAFETY: We just allocated this frame, and it's 4 KiB aligned
        let pml4 = unsafe {
            let mut pml4 = NonNull::new_unchecked(table_ptr(pml4_frame));
            pml4.as_mut().zero();
            pml4
        };

        Self {
            pml4,
            pml4_phys: pml4_frame,
            allocator: Some(allocator),
        }
    }

    /// Allocate a frame using the builder's allocator or global bump allocator
    fn alloc_frame(&mut self) -> PhysAddr {
        if let Some(ref mut alloc) = self.allocator {
            PhysAddr::from_memory_addr(alloc.alloc().expect("Frame allocation failed"))
        } else {
            PhysAddr::from_memory_addr(alloc_frame())
        }
    }

    /// Get the PML4 physical address
    pub fn pml4_phys(&self) -> PhysAddr {
        self.pml4_phys
    }

    /// Map a single 4 KiB page
    pub fn map_page(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) {
        // SAFETY: We maintain the invariant that self.pml4 is valid
        unsafe {
            self.map_page_internal(virt, phys, flags);
        }
    }

    /// Internal mapping logic (unsafe because it dereferences page tables)
    unsafe fn map_page_internal(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) {
        // SAFETY: walk_to_pd returns a valid, exclusively-owned PD frame;
        // the raw pointer (rather than &mut) is only so alloc_frame below
        // can re-borrow self.
        let pd = unsafe { &mut *self.walk_to_pd(virt) };
        let pd_idx = virt.pd_index();
        let mut pd_entry = pd.get(pd_idx);

        // Allocate PT if needed
        if !pd_entry.is_present() {
            let pt_frame = self.alloc_frame();
            let pt = table_ptr(pt_frame);
            unsafe { (*pt).zero() };

            let pt_flags = PageFlags::new().with_present().with_writable();
            pd_entry = PageTableEntry::new_page(pt_frame, pt_flags);
            pd.set(pd_idx, pd_entry);
        }

        // SAFETY: The PD entry either existed or was just populated with a valid
        // page-table frame, so the frame address points to a PageTable.
        let pt = unsafe { table_ptr(pd_entry.frame()).as_mut().unwrap() };
        let pt_idx = virt.pt_index();

        // Install page entry
        pt.set(pt_idx, PageTableEntry::new_page(phys, flags));
    }

    /// Map a 2 MiB huge page: sets the PS bit directly in the PD entry.
    /// `virt` and `phys` must both be 2 MiB-aligned.
    pub fn map_huge_2m(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) {
        debug_assert!(virt.as_u64() & 0x1F_FFFF == 0, "virt not 2 MiB aligned");
        debug_assert!(phys.as_u64() & 0x1F_FFFF == 0, "phys not 2 MiB aligned");
        // SAFETY: same invariants as map_page_internal.
        let pd = unsafe { &mut *self.walk_to_pd(virt) };
        pd.set(
            virt.pd_index(),
            PageTableEntry::new_page(phys, flags.with_huge_page()),
        );
    }

    /// Walk (allocating as needed) PML4 → PDPT → PD for `virt` and return
    /// the PD as a raw pointer (so callers can re-borrow `self`).
    ///
    /// # Safety
    /// `self.pml4` must point at a valid top-level table whose present
    /// entries reference valid page-table frames.
    unsafe fn walk_to_pd(&mut self, virt: VirtAddr) -> *mut PageTable {
        // SAFETY: `self.pml4` is initialized from an allocated, zeroed page-table frame.
        let pml4 = unsafe { self.pml4.as_mut() };

        let pml4_idx = virt.pml4_index();
        let mut pml4_entry = pml4.get(pml4_idx);

        // Allocate PDPT if needed
        if !pml4_entry.is_present() {
            let pdpt_frame = self.alloc_frame();
            let pdpt = table_ptr(pdpt_frame);
            unsafe { (*pdpt).zero() };

            let pdpt_flags = PageFlags::new().with_present().with_writable();
            pml4_entry = PageTableEntry::new_page(pdpt_frame, pdpt_flags);
            pml4.set(pml4_idx, pml4_entry);
        }

        // SAFETY: The PML4 entry either existed or was just populated with a valid
        // page-table frame, so the frame address points to a PageTable.
        let pdpt = unsafe { table_ptr(pml4_entry.frame()).as_mut().unwrap() };
        let pdpt_idx = virt.pdpt_index();
        let mut pdpt_entry = pdpt.get(pdpt_idx);

        // Allocate PD if needed
        if !pdpt_entry.is_present() {
            let pd_frame = self.alloc_frame();
            let pd = table_ptr(pd_frame);
            unsafe { (*pd).zero() };

            let pd_flags = PageFlags::new().with_present().with_writable();
            pdpt_entry = PageTableEntry::new_page(pd_frame, pd_flags);
            pdpt.set(pdpt_idx, pdpt_entry);
        }

        // The PDPT entry either existed or was just populated with a valid
        // page-table frame, so the frame address points to a PageTable.
        table_ptr(pdpt_entry.frame())
    }
}

impl Default for PageTableBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_table_size() {
        assert_eq!(core::mem::size_of::<PageTable>(), 4096);
        assert_eq!(core::mem::align_of::<PageTable>(), 4096);
    }

    #[test]
    fn test_page_table_index_bounds() {
        // Just verify that indices are within bounds
        assert_eq!(VirtAddr::new(0).pml4_index(), 0);
        assert_eq!(VirtAddr::new(0xFFFF_FFFF_8000_0000).pml4_index(), 511);
    }
}
