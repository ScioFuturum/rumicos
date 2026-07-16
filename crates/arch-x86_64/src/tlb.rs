use core::arch::asm;

#[repr(u64)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvpcidKind {
    IndividualAddress = 0,
    SingleContext = 1,
    AllNonGlobal = 2,
    AllIncludingGlobal = 3,
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InvpcidDescriptor {
    pub pcid: u64,
    pub address: u64,
}

#[inline(always)]
/// Invalidate the TLB entry for a single virtual address.
///
/// # Safety
/// Caller must ensure paging is enabled and `addr` is meaningful in the current
/// address space.
pub unsafe fn invlpg(addr: usize) {
    unsafe { asm!("invlpg [{addr}]", addr = in(reg) addr, options(nostack, preserves_flags)) };
}

#[inline(always)]
/// Invalidate TLB entries with INVPCID.
///
/// # Safety
/// Caller must ensure INVPCID is supported and that PCID state is configured
/// consistently with `kind`, `pcid`, and `address`.
pub unsafe fn invpcid(kind: InvpcidKind, pcid: u16, address: u64) {
    let descriptor = InvpcidDescriptor {
        pcid: pcid as u64,
        address,
    };
    unsafe {
        asm!(
            "invpcid {kind}, [{descriptor}]",
            kind = in(reg) kind as u64,
            descriptor = in(reg) &descriptor,
            options(nostack, preserves_flags, readonly),
        )
    };
}
