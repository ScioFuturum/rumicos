pub const USTACK_TOP: u64 = 0x0000_7fff_ffff_0000;
pub const USTACK_SIZE: u64 = 8 * 1024 * 1024;
pub const USTACK_PAGES: u64 = USTACK_SIZE / 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ustack_top_is_in_user_space() {
        assert!(USTACK_TOP < 0x0000_8000_0000_0000);
    }
}
