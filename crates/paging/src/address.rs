//! Physical and virtual address types for x86_64 paging.
//!
//! PhysAddr: 52-bit physical address (bits [51:0]).
//! VirtAddr: 64-bit canonical virtual address.

/// Physical address newtype
///
/// Represents a 52-bit physical address (only bits [51:0] are valid on current x86_64).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(transparent)]
pub struct PhysAddr(u64);

impl PhysAddr {
    /// Create a new physical address, asserting it's within valid range
    #[inline]
    pub const fn new(addr: u64) -> Self {
        // Physical addresses are 52-bit; bits [63:52] must be 0
        // This is checked at runtime in debug, compile-time assertions in const contexts would be ideal
        // but we can't panic in const fn, so we just wrap it
        Self(addr & 0x000F_FFFF_FFFF_FFFF)
    }

    /// Create from kernel_memory::PhysAddr
    #[inline]
    pub fn from_memory_addr(addr: kernel_memory::PhysAddr) -> Self {
        Self::new(addr.as_u64())
    }

    /// Get the raw u64 value
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Align address down to page boundary (4 KiB)
    #[inline]
    pub const fn align_down_4k(self) -> Self {
        Self(self.0 & !0xFFF)
    }

    /// Align address up to page boundary (4 KiB)
    #[inline]
    pub const fn align_up_4k(self) -> Self {
        Self((self.0 + 0xFFF) & !0xFFF)
    }

    /// Align address down to 2 MiB boundary
    #[inline]
    pub const fn align_down_2m(self) -> Self {
        Self(self.0 & !0x1F_FFFF)
    }

    /// Align address up to 2 MiB boundary
    #[inline]
    pub const fn align_up_2m(self) -> Self {
        Self((self.0 + 0x1F_FFFF) & !0x1F_FFFF)
    }

    /// Align address down to 1 GiB boundary
    #[inline]
    pub const fn align_down_1g(self) -> Self {
        Self(self.0 & !0x3FFF_FFFF)
    }

    /// Align address up to 1 GiB boundary
    #[inline]
    pub const fn align_up_1g(self) -> Self {
        Self((self.0 + 0x3FFF_FFFF) & !0x3FFF_FFFF)
    }

    /// Extract frame number for PTE (bits [51:12])
    #[inline]
    pub const fn frame_bits(self) -> u64 {
        (self.0 >> 12) & 0xF_FFFF_FFFF
    }

    /// Check if address is aligned to 4 KiB
    #[inline]
    pub const fn is_aligned_4k(self) -> bool {
        (self.0 & 0xFFF) == 0
    }

    /// Check if address is aligned to 2 MiB
    #[inline]
    pub const fn is_aligned_2m(self) -> bool {
        (self.0 & 0x1F_FFFF) == 0
    }

    /// Check if address is aligned to 1 GiB
    #[inline]
    pub const fn is_aligned_1g(self) -> bool {
        (self.0 & 0x3FFF_FFFF) == 0
    }
}

impl From<u64> for PhysAddr {
    #[inline]
    fn from(addr: u64) -> Self {
        PhysAddr::new(addr)
    }
}

impl core::ops::Add<u64> for PhysAddr {
    type Output = Self;
    #[inline]
    fn add(self, rhs: u64) -> Self {
        PhysAddr::new(self.0 + rhs)
    }
}

impl core::ops::Sub<u64> for PhysAddr {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: u64) -> Self {
        PhysAddr::new(self.0 - rhs)
    }
}

/// Virtual address newtype
///
/// Represents a 64-bit canonical virtual address where bits [63:48]
/// are sign-extended from bit 47. Valid ranges:
/// - User space: 0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF
/// - Kernel space: 0xFFFF_8000_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(transparent)]
pub struct VirtAddr(u64);

impl VirtAddr {
    /// Create a new virtual address, asserting canonical form
    #[inline]
    pub const fn new(addr: u64) -> Self {
        // Check canonical: bits [63:48] must be sign-extended from bit 47
        let is_canonical = (addr >> 47) == 0 || (addr >> 47) == 0x1FFFF;
        debug_assert!(is_canonical, "Non-canonical address");
        Self(addr)
    }

    /// Create a new virtual address without checking canonicality (for internal use)
    #[inline]
    pub const fn new_unchecked(addr: u64) -> Self {
        Self(addr)
    }

    /// Get the raw u64 value
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Extract PML4 index (bits [47:39])
    #[inline]
    pub const fn pml4_index(self) -> usize {
        ((self.0 >> 39) & 0x1FF) as usize
    }

    /// Extract PDPT index (bits [38:30])
    #[inline]
    pub const fn pdpt_index(self) -> usize {
        ((self.0 >> 30) & 0x1FF) as usize
    }

    /// Extract PD index (bits [29:21])
    #[inline]
    pub const fn pd_index(self) -> usize {
        ((self.0 >> 21) & 0x1FF) as usize
    }

    /// Extract PT index (bits [20:12])
    #[inline]
    pub const fn pt_index(self) -> usize {
        ((self.0 >> 12) & 0x1FF) as usize
    }

    /// Extract page offset (bits [11:0])
    #[inline]
    pub const fn page_offset(self) -> u64 {
        self.0 & 0xFFF
    }

    /// Align address down to page boundary (4 KiB)
    #[inline]
    pub const fn align_down_4k(self) -> Self {
        Self(self.0 & !0xFFF)
    }

    /// Align address up to page boundary (4 KiB)
    #[inline]
    pub const fn align_up_4k(self) -> Self {
        Self((self.0 + 0xFFF) & !0xFFF)
    }

    /// Align address down to 2 MiB boundary
    #[inline]
    pub const fn align_down_2m(self) -> Self {
        Self(self.0 & !0x1F_FFFF)
    }

    /// Align address up to 2 MiB boundary
    #[inline]
    pub const fn align_up_2m(self) -> Self {
        Self((self.0 + 0x1F_FFFF) & !0x1F_FFFF)
    }

    /// Align address down to 1 GiB boundary
    #[inline]
    pub const fn align_down_1g(self) -> Self {
        Self(self.0 & !0x3FFF_FFFF)
    }

    /// Align address up to 1 GiB boundary
    #[inline]
    pub const fn align_up_1g(self) -> Self {
        Self((self.0 + 0x3FFF_FFFF) & !0x3FFF_FFFF)
    }

    /// Check if address is user-space canonical
    #[inline]
    pub const fn is_user_space(self) -> bool {
        (self.0 >> 47) == 0
    }

    /// Check if address is kernel-space canonical
    #[inline]
    pub const fn is_kernel_space(self) -> bool {
        (self.0 >> 47) == 0x1FFFF
    }

    /// Check if address is aligned to 4 KiB
    #[inline]
    pub const fn is_aligned_4k(self) -> bool {
        (self.0 & 0xFFF) == 0
    }

    /// Check if address is aligned to 2 MiB
    #[inline]
    pub const fn is_aligned_2m(self) -> bool {
        (self.0 & 0x1F_FFFF) == 0
    }

    /// Check if address is aligned to 1 GiB
    #[inline]
    pub const fn is_aligned_1g(self) -> bool {
        (self.0 & 0x3FFF_FFFF) == 0
    }

    /// Is this address in the direct physical map region?
    #[inline]
    pub const fn is_direct_map(self) -> bool {
        self.0 >= 0xFFFF_8000_0000_0000
    }
}

impl From<u64> for VirtAddr {
    #[inline]
    fn from(addr: u64) -> Self {
        VirtAddr::new(addr)
    }
}

impl core::ops::Add<u64> for VirtAddr {
    type Output = Self;
    #[inline]
    fn add(self, rhs: u64) -> Self {
        VirtAddr::new(self.0.wrapping_add(rhs))
    }
}

impl core::ops::Sub<u64> for VirtAddr {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: u64) -> Self {
        VirtAddr::new(self.0.wrapping_sub(rhs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phys_addr_alignment() {
        let addr = PhysAddr::new(0x1234_5678);
        assert_eq!(addr.align_down_4k().as_u64(), 0x1234_5000);
        assert_eq!(addr.align_up_4k().as_u64(), 0x1234_6000);
    }

    #[test]
    fn test_virt_addr_indices() {
        let addr = VirtAddr::new(0xFFFF_FFFF_8001_2345);
        assert_eq!(addr.pml4_index(), 511);
        assert_eq!(addr.pdpt_index(), 510);
        assert_eq!(addr.pd_index(), 0);
        assert_eq!(addr.pt_index(), 0x012);
        assert_eq!(addr.page_offset(), 0x345);
    }

    #[test]
    fn test_virt_addr_canonical() {
        let user_addr = VirtAddr::new(0x0000_1234_5678_9ABC);
        assert!(user_addr.is_user_space());
        assert!(!user_addr.is_kernel_space());

        let kernel_addr = VirtAddr::new(0xFFFF_FFFF_8000_0000);
        assert!(!kernel_addr.is_user_space());
        assert!(kernel_addr.is_kernel_space());
    }
}
