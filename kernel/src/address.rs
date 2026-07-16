pub const PAGE_SIZE: u64 = 4096;
pub const HUGE_PAGE_2M: u64 = 2 * 1024 * 1024;
pub const HUGE_PAGE_1G: u64 = 1024 * 1024 * 1024;

pub const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;
pub const DIRECT_MAP_BASE_4L: u64 = 0xffff_8000_0000_0000;
pub const DIRECT_MAP_BASE_5L: u64 = 0xff00_0000_0000_0000;

#[inline(always)]
pub const fn direct_map_addr(paddr: u64, la57: bool) -> u64 {
    if la57 {
        DIRECT_MAP_BASE_5L.wrapping_add(paddr)
    } else {
        DIRECT_MAP_BASE_4L.wrapping_add(paddr)
    }
}
