pub const PAGE_SHIFT: usize = 12;
pub const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub const HUGE_2M_ORDER: usize = 9;
pub const HUGE_1G_ORDER: usize = 18;
pub const MAX_PCID: usize = 4096;

#[inline(always)]
pub const fn order_pages(order: usize) -> usize {
    1usize << order
}

#[inline(always)]
pub const fn order_bytes(order: usize) -> usize {
    order_pages(order) << PAGE_SHIFT
}
