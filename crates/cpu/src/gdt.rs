use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem;
use core::ptr;

pub const GDT_ENTRIES: usize = 7;

pub const NULL_SEL: u16 = 0;
pub const KERNEL_CS: u16 = 1 << 3;
pub const KERNEL_DS: u16 = 2 << 3;
pub const USER_DS: u16 = (3 << 3) | 3;
pub const USER_CS: u16 = (4 << 3) | 3;
pub const TSS_SEL: u16 = 5 << 3;

const ACCESS_PRESENT: u8 = 1 << 7;
const ACCESS_RING_3: u8 = 3 << 5;
const ACCESS_CODE_OR_DATA: u8 = 1 << 4;
const TYPE_CODE_EXEC_READ: u8 = 0b1010;
const TYPE_DATA_READ_WRITE: u8 = 0b0010;
const TYPE_TSS_AVAILABLE: u8 = 0b1001;
const FLAG_LONG_MODE: u8 = 1 << 1;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct DescriptorTablePointer {
    pub limit: u16,
    pub base: u64,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct SegmentDescriptor {
    pub value: u64,
}

impl SegmentDescriptor {
    pub const fn null() -> Self {
        Self { value: 0 }
    }

    pub const fn kernel_code() -> Self {
        Self::code_or_data(TYPE_CODE_EXEC_READ, 0, FLAG_LONG_MODE)
    }

    pub const fn kernel_data() -> Self {
        Self::code_or_data(TYPE_DATA_READ_WRITE, 0, 0)
    }

    pub const fn user_data() -> Self {
        Self::code_or_data(TYPE_DATA_READ_WRITE, ACCESS_RING_3, 0)
    }

    pub const fn user_code() -> Self {
        Self::code_or_data(TYPE_CODE_EXEC_READ, ACCESS_RING_3, FLAG_LONG_MODE)
    }

    const fn code_or_data(ty: u8, dpl: u8, flags: u8) -> Self {
        let access = ACCESS_PRESENT | dpl | ACCESS_CODE_OR_DATA | ty;
        let value = ((access as u64) << 40) | ((flags as u64) << 52);
        Self { value }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct TssDescriptor {
    limit_low: u16,
    base_low: u16,
    base_mid: u8,
    access: u8,
    granularity: u8,
    base_high: u8,
    base_upper: u32,
    reserved: u32,
}

impl TssDescriptor {
    pub const fn empty() -> Self {
        Self {
            limit_low: 0,
            base_low: 0,
            base_mid: 0,
            access: 0,
            granularity: 0,
            base_high: 0,
            base_upper: 0,
            reserved: 0,
        }
    }

    pub const fn new(base: u64, limit: u32) -> Self {
        Self {
            limit_low: limit as u16,
            base_low: base as u16,
            base_mid: (base >> 16) as u8,
            access: ACCESS_PRESENT | TYPE_TSS_AVAILABLE,
            granularity: ((limit >> 16) as u8) & 0x0f,
            base_high: (base >> 24) as u8,
            base_upper: (base >> 32) as u32,
            reserved: 0,
        }
    }

    pub const fn low_u64(self) -> u64 {
        (self.limit_low as u64)
            | ((self.base_low as u64) << 16)
            | ((self.base_mid as u64) << 32)
            | ((self.access as u64) << 40)
            | ((self.granularity as u64) << 48)
            | ((self.base_high as u64) << 56)
    }

    pub const fn high_u64(self) -> u64 {
        self.base_upper as u64
    }
}

#[repr(C, align(16))]
pub struct Gdt {
    entries: [u64; GDT_ENTRIES],
}

impl Gdt {
    pub const fn new() -> Self {
        Self {
            entries: [
                SegmentDescriptor::null().value,
                SegmentDescriptor::kernel_code().value,
                SegmentDescriptor::kernel_data().value,
                SegmentDescriptor::user_data().value,
                SegmentDescriptor::user_code().value,
                0,
                0,
            ],
        }
    }

    pub const fn entries(&self) -> &[u64; GDT_ENTRIES] {
        &self.entries
    }

    pub fn patch_tss(&mut self, base: u64, limit: u32) {
        let descriptor = TssDescriptor::new(base, limit);
        self.entries[5] = descriptor.low_u64();
        self.entries[6] = descriptor.high_u64();
    }

    fn pointer(&self) -> DescriptorTablePointer {
        DescriptorTablePointer {
            limit: (mem::size_of::<Self>() - 1) as u16,
            base: ptr::from_ref(self) as u64,
        }
    }
}

impl Default for Gdt {
    fn default() -> Self {
        Self::new()
    }
}

struct GdtCell(UnsafeCell<Gdt>);

unsafe impl Sync for GdtCell {}

impl GdtCell {
    const fn new() -> Self {
        Self(UnsafeCell::new(Gdt::new()))
    }

    fn get(&self) -> *mut Gdt {
        self.0.get()
    }
}

static GDT: GdtCell = GdtCell::new();

pub fn init_gdt() -> &'static Gdt {
    init_gdt_mut()
}

pub(crate) fn init_gdt_mut() -> &'static mut Gdt {
    // SAFETY: CPU bootstrap is serialized in this kernel checkpoint; the single
    // static GDT is initialized before interrupts and before other CPUs mutate it.
    let gdt = unsafe { &mut *GDT.get() };
    *gdt = Gdt::new();
    let pointer = gdt.pointer();
    // SAFETY: `pointer` references the static GDT for the duration of LGDT, and
    // segment reload uses the selector constants installed in that table.
    unsafe {
        lgdt(&pointer);
        reload_segments();
    }
    gdt
}

#[inline(always)]
unsafe fn lgdt(pointer: &DescriptorTablePointer) {
    // SAFETY: caller guarantees `pointer` describes a valid GDT descriptor.
    unsafe {
        asm!(
            "lgdt [{pointer}]",
            pointer = in(reg) pointer,
            options(readonly, nostack, preserves_flags),
        )
    };
}

#[inline(never)]
unsafe fn reload_segments() {
    // SAFETY: the loaded GDT contains valid kernel code/data descriptors at the
    // selector constants used below; retfq is used because far ljmp is brittle in
    // LLVM inline assembly on x86_64.
    unsafe {
        asm!(
            "push {cs}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            "mov ax, {ds}",
            "mov ss, ax",
            "mov ds, ax",
            "mov es, ax",
            "xor eax, eax",
            "mov fs, ax",
            "mov gs, ax",
            cs = const KERNEL_CS,
            ds = const KERNEL_DS,
            out("rax") _,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(desc: u64) -> u8 {
        ((desc >> 40) & 0xff) as u8
    }

    fn flags(desc: u64) -> u8 {
        ((desc >> 52) & 0x0f) as u8
    }

    #[test]
    fn gdt_descriptor_bit_layout() {
        let gdt = Gdt::new();
        let kernel_code = gdt.entries()[1];
        assert_eq!(access(kernel_code) & ACCESS_PRESENT, ACCESS_PRESENT);
        assert_eq!(
            access(kernel_code) & ACCESS_CODE_OR_DATA,
            ACCESS_CODE_OR_DATA
        );
        assert_eq!(access(kernel_code) & 0x0f, TYPE_CODE_EXEC_READ);
        assert_eq!((access(kernel_code) >> 5) & 0x03, 0);
        assert_eq!(flags(kernel_code) & FLAG_LONG_MODE, FLAG_LONG_MODE);
        assert_eq!(flags(kernel_code) & (1 << 2), 0);

        let user_data = gdt.entries()[3];
        assert_eq!(access(user_data) & 0x0f, TYPE_DATA_READ_WRITE);
        assert_eq!((access(user_data) >> 5) & 0x03, 3);

        let user_code = gdt.entries()[4];
        assert_eq!(access(user_code) & 0x0f, TYPE_CODE_EXEC_READ);
        assert_eq!((access(user_code) >> 5) & 0x03, 3);
        assert_eq!(flags(user_code) & FLAG_LONG_MODE, FLAG_LONG_MODE);
    }

    #[test]
    fn selector_values_match_table_order_and_sysret_abi() {
        assert_eq!(KERNEL_CS, 0x08);
        assert_eq!(KERNEL_DS, 0x10);
        assert_eq!(USER_DS, 0x1b);
        assert_eq!(USER_CS, 0x23);
        assert_eq!(TSS_SEL, 0x28);
        assert_eq!((USER_CS - 16) + 8, USER_DS);
        assert_eq!((USER_CS - 16) + 16, USER_CS);
    }
}
