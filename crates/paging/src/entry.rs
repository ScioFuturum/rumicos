/// Page table entry representation for x86_64
///
/// Encodes a 64-bit PTE with flags and physical frame number.
/// Bit layout:
/// [0]       = Present (P)
/// [1]       = Writable (W)
/// [2]       = User Accessible (U/S)
/// [3]       = Write-Through (PWT)
/// [4]       = Cache Disable (PCD)
/// [5]       = Accessed (A)
/// [6]       = Dirty (D)
/// [7]       = Huge Page (PS) — 2 MiB at PD, 1 GiB at PDPT
/// [8]       = Global (G)
/// [11:9]    = Available (AVL) — bit [9] is "owned", bit [10] is "copy-on-write"
/// [51:12]   = Physical Frame Number (PFN)
/// [62]      = Reserved (zero on current CPUs, used for flags in some kernels)
/// [63]      = No-Execute (NX) — requires EFER.NXE = 1
use crate::address::PhysAddr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

/// Page flags (subset of PTE bits)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct PageFlags(u64);

impl PageFlags {
    pub const PRESENT: u64 = 1 << 0;
    pub const WRITABLE: u64 = 1 << 1;
    pub const USER_ACCESSIBLE: u64 = 1 << 2;
    pub const WRITE_THROUGH: u64 = 1 << 3;
    pub const CACHE_DISABLE: u64 = 1 << 4;
    pub const ACCESSED: u64 = 1 << 5;
    pub const DIRTY: u64 = 1 << 6;
    pub const HUGE_PAGE: u64 = 1 << 7;
    pub const GLOBAL: u64 = 1 << 8;
    pub const OWNED: u64 = 1 << 9; // Available bit for allocator
    pub const COW: u64 = 1 << 10; // Available bit for process COW mappings
    pub const NO_EXECUTE: u64 = 1 << 63;

    #[inline]
    pub const fn new() -> Self {
        Self(0)
    }

    #[inline]
    pub const fn with_present(self) -> Self {
        Self(self.0 | Self::PRESENT)
    }

    #[inline]
    pub const fn with_writable(self) -> Self {
        Self(self.0 | Self::WRITABLE)
    }

    #[inline]
    pub const fn with_user_accessible(self) -> Self {
        Self(self.0 | Self::USER_ACCESSIBLE)
    }

    #[inline]
    pub const fn with_write_through(self) -> Self {
        Self(self.0 | Self::WRITE_THROUGH)
    }

    #[inline]
    pub const fn with_cache_disable(self) -> Self {
        Self(self.0 | Self::CACHE_DISABLE)
    }

    #[inline]
    pub const fn with_accessed(self) -> Self {
        Self(self.0 | Self::ACCESSED)
    }

    #[inline]
    pub const fn with_dirty(self) -> Self {
        Self(self.0 | Self::DIRTY)
    }

    #[inline]
    pub const fn with_huge_page(self) -> Self {
        Self(self.0 | Self::HUGE_PAGE)
    }

    #[inline]
    pub const fn with_global(self) -> Self {
        Self(self.0 | Self::GLOBAL)
    }

    #[inline]
    pub const fn with_no_execute(self) -> Self {
        Self(self.0 | Self::NO_EXECUTE)
    }

    #[inline]
    pub const fn with_cow(self) -> Self {
        Self(self.0 | Self::COW)
    }

    #[inline]
    pub const fn without_writable(self) -> Self {
        Self(self.0 & !Self::WRITABLE)
    }

    #[inline]
    pub const fn without_cow(self) -> Self {
        Self(self.0 & !Self::COW)
    }

    #[inline]
    pub const fn is_cow(self) -> bool {
        (self.0 & Self::COW) != 0
    }

    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl Default for PageFlags {
    fn default() -> Self {
        Self::new()
    }
}

impl PageTableEntry {
    /// Create a new page table entry for a data page
    #[inline]
    pub const fn new_page(phys: PhysAddr, flags: PageFlags) -> Self {
        let pfn = phys.frame_bits();
        Self((pfn << 12) | flags.as_u64())
    }

    /// Create a new page table entry for an intermediate table (always present, writable)
    #[inline]
    pub const fn new_table(phys: PhysAddr) -> Self {
        let flags = PageFlags::new().with_present().with_writable();
        Self::new_page(phys, flags)
    }

    /// Create an empty entry (not present)
    #[inline]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Get the physical address from this entry
    #[inline]
    pub const fn frame(self) -> PhysAddr {
        // Extract bits [51:12] as physical frame number, shift back to get address
        PhysAddr::new(((self.0 >> 12) & 0xF_FFFF_FFFF) << 12)
    }

    /// Get the flags from this entry
    #[inline]
    pub const fn flags(self) -> PageFlags {
        // Mask out the frame number bits [51:12]
        PageFlags(self.0 & 0xFFF | (self.0 & (1u64 << 63)))
    }

    /// Return this entry with the same frame and different flags.
    #[inline]
    pub const fn with_flags(self, flags: PageFlags) -> Self {
        Self::new_page(self.frame(), flags)
    }

    /// Check if entry is present
    #[inline]
    pub const fn is_present(self) -> bool {
        (self.0 & PageFlags::PRESENT) != 0
    }

    /// Check if entry is writable
    #[inline]
    pub const fn is_writable(self) -> bool {
        (self.0 & PageFlags::WRITABLE) != 0
    }

    /// Check if entry is user accessible
    #[inline]
    pub const fn is_user_accessible(self) -> bool {
        (self.0 & PageFlags::USER_ACCESSIBLE) != 0
    }

    /// Check if entry has huge page bit set (2 MiB or 1 GiB)
    #[inline]
    pub const fn is_huge_page(self) -> bool {
        (self.0 & PageFlags::HUGE_PAGE) != 0
    }

    /// Check if entry is global (skips TLB flush on CR3 reload with PGE enabled)
    #[inline]
    pub const fn is_global(self) -> bool {
        (self.0 & PageFlags::GLOBAL) != 0
    }

    /// Check if entry has no-execute bit set
    #[inline]
    pub const fn is_no_execute(self) -> bool {
        (self.0 & PageFlags::NO_EXECUTE) != 0
    }

    /// Check if entry is marked copy-on-write.
    #[inline]
    pub const fn is_cow(self) -> bool {
        (self.0 & PageFlags::COW) != 0
    }

    /// Get raw u64 value
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Construct CR3 value from PML4 address, PCID, and NOFLUSH bit
#[inline]
pub const fn make_cr3(pml4_phys: PhysAddr, pcid: u16, noflush: bool) -> u64 {
    let mut cr3 = pml4_phys.frame_bits() << 12;
    cr3 |= (pcid as u64) & 0xFFF;
    if noflush {
        cr3 |= 1u64 << 63;
    }
    cr3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_flags_builder() {
        let flags = PageFlags::new()
            .with_present()
            .with_writable()
            .with_global()
            .with_no_execute();

        assert_eq!(
            flags.as_u64(),
            PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::GLOBAL | PageFlags::NO_EXECUTE
        );
    }

    #[test]
    fn test_page_table_entry_round_trip() {
        let phys = PhysAddr::new(0x1234_5000);
        let flags = PageFlags::new()
            .with_present()
            .with_writable()
            .with_global();

        let entry = PageTableEntry::new_page(phys, flags);

        assert_eq!(entry.frame(), phys);
        assert!(entry.is_present());
        assert!(entry.is_writable());
        assert!(entry.is_global());
    }

    #[test]
    fn test_make_cr3_layout() {
        let pml4_addr = PhysAddr::new(0x1000);
        let cr3 = make_cr3(pml4_addr, 0, false);

        // PML4 address should be in bits [51:12]
        assert_eq!((cr3 >> 12) & 0xF_FFFF_FFFF, pml4_addr.frame_bits());
        // PCID should be 0
        assert_eq!(cr3 & 0xFFF, 0);
        // NOFLUSH bit should be 0
        assert_eq!((cr3 >> 63) & 1, 0);
    }

    #[test]
    fn test_make_cr3_with_pcid() {
        let pml4_addr = PhysAddr::new(0x1000);
        let cr3 = make_cr3(pml4_addr, 42, true);

        // PCID should be 42
        assert_eq!(cr3 & 0xFFF, 42);
        // NOFLUSH bit should be 1
        assert_eq!((cr3 >> 63) & 1, 1);
    }

    #[test]
    fn cow_uses_available_bit_ten_without_colliding_with_owned() {
        assert_eq!(PageFlags::COW, 1 << 10);
        assert_eq!(PageFlags::OWNED, 1 << 9);
        assert_eq!(PageFlags::COW & PageFlags::OWNED, 0);
    }

    #[test]
    fn cow_flags_can_clear_writable_and_restore() {
        let flags = PageFlags::new().with_present().with_writable().with_cow();
        assert!(flags.is_cow());
        let ro_cow = flags.without_writable();
        assert_eq!(ro_cow.as_u64() & PageFlags::WRITABLE, 0);
        assert!(ro_cow.is_cow());
        assert!(!ro_cow.without_cow().is_cow());
    }
}
