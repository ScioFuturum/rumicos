//! Limine bootloader protocol integration.
//!
//! Provides access to Limine's memory map and kernel executable address.

/// Limine memory map entry
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LimineMemMapEntry {
    pub base: u64,
    pub length: u64,
    pub mem_type: u32,
    pub _unused: u32,
}

/// Limine memory map response
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LimineMemMapResponse {
    pub revision: u64,
    pub entry_count: u64,
    pub entries: *const *const LimineMemMapEntry,
}

/// Limine executable address response
#[repr(C)]
pub struct LimineExecutableAddressResponse {
    pub revision: u64,
    pub physical_base: u64,
    pub virtual_base: u64,
}

#[repr(C)]
pub struct LimineRsdpResponse {
    pub revision: u64,
    pub address: u64,
}

/// Limine request header.
///
/// `response` is written by the BOOTLOADER, from outside the Rust abstract
/// machine, into what rustc otherwise considers an immutable static — so it
/// must be `UnsafeCell` and read with `read_volatile`. With a plain field,
/// release builds constant-fold the null initializer and every `get_*`
/// accessor compiles into an unconditional "request not fulfilled" panic
/// regardless of what Limine actually wrote (observed on the first real
/// QEMU boot).
#[repr(C)]
pub struct LimineRequestHeader {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: core::cell::UnsafeCell<*mut core::ffi::c_void>,
}

unsafe impl Sync for LimineRequestHeader {}

impl LimineRequestHeader {
    /// Volatile read of the bootloader-written response pointer.
    fn response_ptr(&self) -> *mut core::ffi::c_void {
        // SAFETY: the cell is only ever written once, by the bootloader,
        // before the kernel gets control; volatile forbids const-folding.
        unsafe { core::ptr::read_volatile(self.response.get()) }
    }
}

#[used]
#[unsafe(link_section = ".requests_start_marker")]
pub static LIMINE_REQUESTS_START_MARKER: [u64; 4] = [
    0xf6b8_f4b3_9de7_d1ae,
    0xfab9_1a69_40fc_b9cf,
    0x785c_6ed0_15d3_e316,
    0x181e_920a_7852_b9d9,
];

/// Limine base revision tag. Without it the executable claims base
/// revision 0, which current Limine releases no longer support — the
/// bootloader still loads and jumps to the kernel but fulfills NO feature
/// requests, so every `response` pointer stays null (observed on the first
/// real QEMU boot). The bootloader rewrites the third word to 0 when the
/// requested revision is supported.
///
/// Base revision 3 also makes the RSDP response's `address` field a
/// physical address, which is exactly what `get_rsdp_phys`/`parse_madt`
/// already assume.
#[used]
#[unsafe(link_section = ".requests")]
pub static LIMINE_BASE_REVISION: [u64; 3] = [0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, 3];

/// Limine memmap request.
///
/// Request IDs are `LIMINE_COMMON_MAGIC` (two words shared by every Limine
/// feature request) followed by two feature-specific words — the values
/// below must match the limine protocol spec exactly or the bootloader
/// silently leaves `response` null (which is exactly what happened on the
/// first real QEMU boot: the pre-2026-07-10 IDs here were fabricated and
/// only the RSDP request ever had the real magic).
#[used]
#[unsafe(link_section = ".requests")]
pub static MEMMAP_REQUEST: LimineRequestHeader = LimineRequestHeader {
    id: [
        0xc7b1dd30df4c8b88,
        0x0a82e883a194f07b,
        0x67cf3d9d378a806f,
        0xe304acdfc50c3c62,
    ],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null_mut()),
};

/// Limine executable address request
#[used]
#[unsafe(link_section = ".requests")]
pub static EXECUTABLE_ADDRESS_REQUEST: LimineRequestHeader = LimineRequestHeader {
    id: [
        0xc7b1dd30df4c8b88,
        0x0a82e883a194f07b,
        0x71ba76863cc55f63,
        0xb2644a48c516a487,
    ],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null_mut()),
};

/// Limine RSDP request provides the physical address of ACPI RSDP.
#[used]
#[unsafe(link_section = ".requests")]
pub static RSDP_REQUEST: LimineRequestHeader = LimineRequestHeader {
    id: [
        0xc7b1dd30df4c8b88,
        0x0a82e883a194f07b,
        0xc5e77b6b397e7b43,
        0x27637845accdcf3c,
    ],
    revision: 0,
    response: core::cell::UnsafeCell::new(core::ptr::null_mut()),
};

#[used]
#[unsafe(link_section = ".requests_end_marker")]
pub static LIMINE_REQUESTS_END_MARKER: [u64; 2] = [0xadc0_e053_1bb1_0d03, 0x9572_709f_3176_4c62];

/// Get the memory map from Limine
pub fn get_memmap() -> LimineMemMapResponse {
    let resp = MEMMAP_REQUEST.response_ptr() as *const LimineMemMapResponse;
    if resp.is_null() {
        panic!("Limine memmap request not fulfilled");
    }
    // SAFETY: non-null response pointers from Limine point at a valid,
    // bootloader-populated response structure.
    unsafe { *resp }
}

/// Get the kernel executable physical base from Limine
pub fn get_executable_address() -> kernel_paging::address::PhysAddr {
    let resp = EXECUTABLE_ADDRESS_REQUEST.response_ptr() as *const LimineExecutableAddressResponse;
    if resp.is_null() {
        panic!("Limine executable address request not fulfilled");
    }
    // SAFETY: see get_memmap.
    unsafe { kernel_paging::address::PhysAddr::new((*resp).physical_base) }
}

/// Get the kernel's physical address range (base, end)
pub fn get_kernel_phys_range() -> (
    kernel_paging::address::PhysAddr,
    kernel_paging::address::PhysAddr,
) {
    // The image's true loaded extent, from the linker script — the old
    // "assume 1 MiB" estimate undershot the real (3+ MiB) kernel.
    unsafe extern "C" {
        static __bss_end: u8;
    }
    const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;
    let base = get_executable_address();
    let size = (&raw const __bss_end) as u64 - KERNEL_BASE;
    let end = kernel_paging::address::PhysAddr::new(base.as_u64() + size);
    (base, end)
}

pub fn get_rsdp_phys() -> u64 {
    let resp = RSDP_REQUEST.response_ptr() as *const LimineRsdpResponse;
    if resp.is_null() {
        panic!("Limine RSDP request not fulfilled");
    }
    // SAFETY: see get_memmap.
    unsafe { (*resp).address }
}
