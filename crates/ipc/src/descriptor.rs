#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcDescriptor {
    pub offset: u64,
    pub len: u32,
    pub flags: u32,
    pub epoch: u64,
    pub user0: u64,
    pub user1: u64,
    pub reserved: [u64; 3],
}

impl IpcDescriptor {
    #[inline(always)]
    pub const fn new(offset: u64, len: u32, flags: u32) -> Self {
        Self {
            offset,
            len,
            flags,
            epoch: 0,
            user0: 0,
            user1: 0,
            reserved: [0; 3],
        }
    }
}
