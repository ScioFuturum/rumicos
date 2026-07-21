pub const USTACK_TOP: u64 = 0x0000_7fff_ffff_0000;
/// User stack size. 256 KiB (64 pages), mapped eagerly by
/// `AddressSpace::map_user_stack`. Was 8 MiB, but that meant every `fork()`
/// CoW-shared 2048 stack pages (and every `execve` eagerly allocated and
/// zeroed 2048 frames) for programs that use a few KiB — 32x the per-fork
/// and per-exec cost for no benefit. A demand-paged stack (fault-in on
/// touch) would be the real fix; until then this is a big, safe reduction.
pub const USTACK_SIZE: u64 = 256 * 1024;
pub const USTACK_PAGES: u64 = USTACK_SIZE / 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::assertions_on_constants)] // documents the invariant as a named test
    fn ustack_top_is_in_user_space() {
        assert!(USTACK_TOP < 0x0000_8000_0000_0000);
    }
}
