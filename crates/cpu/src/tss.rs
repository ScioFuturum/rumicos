use crate::gdt::{Gdt, TSS_SEL};
use crate::percpu;
use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem;
use core::ptr;

pub const MAX_CPUS: usize = 256;
pub const KERNEL_STACK_SIZE: usize = 8 * 1024;
pub const INTERRUPT_STACK_SIZE: usize = 4 * 1024;

#[repr(C, packed)]
pub struct Tss {
    reserved0: u32,
    pub rsp: [u64; 3],
    reserved1: u64,
    pub ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    pub iomap_base: u16,
}

impl Tss {
    pub const fn new() -> Self {
        Self {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: mem::size_of::<Tss>() as u16,
        }
    }
}

impl Default for Tss {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(16))]
struct KernelStack([u8; KERNEL_STACK_SIZE]);

impl KernelStack {
    const fn new() -> Self {
        Self([0; KERNEL_STACK_SIZE])
    }

    fn top(&self) -> u64 {
        (ptr::addr_of!(self.0) as u64 + KERNEL_STACK_SIZE as u64) & !0x0f
    }
}

#[repr(C, align(16))]
struct InterruptStack([u8; INTERRUPT_STACK_SIZE]);

impl InterruptStack {
    const fn new() -> Self {
        Self([0; INTERRUPT_STACK_SIZE])
    }

    fn top(&self) -> u64 {
        (ptr::addr_of!(self.0) as u64 + INTERRUPT_STACK_SIZE as u64) & !0x0f
    }
}

struct TssTable([UnsafeCell<Tss>; MAX_CPUS]);

unsafe impl Sync for TssTable {}

impl TssTable {
    const fn new() -> Self {
        Self([const { UnsafeCell::new(Tss::new()) }; MAX_CPUS])
    }

    fn get(&self, cpu_id: u32) -> *mut Tss {
        self.0[cpu_index(cpu_id)].get()
    }
}

static TSS_TABLE: TssTable = TssTable::new();
static KERNEL_STACKS: [KernelStack; MAX_CPUS] = [const { KernelStack::new() }; MAX_CPUS];
static INTERRUPT_STACKS: [InterruptStack; MAX_CPUS] = [const { InterruptStack::new() }; MAX_CPUS];

pub fn init_tss(cpu_id: u32, gdt: &mut Gdt) {
    let index = cpu_index(cpu_id);
    let rsp0 = KERNEL_STACKS[index].top();
    let ist1 = INTERRUPT_STACKS[index].top();
    // SAFETY: each CPU owns exactly one TSS slot selected by `cpu_id`; bootstrap
    // initializes the slot before LTR exposes it to hardware.
    let tss = unsafe { &mut *TSS_TABLE.get(cpu_id) };
    *tss = Tss::new();
    tss.rsp[0] = rsp0;
    tss.ist[0] = ist1;
    tss.iomap_base = mem::size_of::<Tss>() as u16;

    gdt.patch_tss(
        ptr::from_ref(tss) as u64,
        (mem::size_of::<Tss>() - 1) as u32,
    );
    percpu::set_kernel_rsp(cpu_id, rsp0);
    // SAFETY: the GDT TSS descriptor has just been patched with this CPU's TSS
    // base and limit, so LTR can load the selector.
    unsafe { ltr(TSS_SEL) };
}

pub fn kernel_stack_top(cpu_id: u32) -> u64 {
    KERNEL_STACKS[cpu_index(cpu_id)].top()
}

pub fn interrupt_stack_top(cpu_id: u32) -> u64 {
    INTERRUPT_STACKS[cpu_index(cpu_id)].top()
}

/// Update TSS.rsp0 for a CPU after switching to a thread with a different
/// kernel stack.
///
/// # Safety
/// `cpu_id` must name an initialized TSS slot and `rsp0` must be a canonical
/// top-of-stack pointer that remains valid for interrupt delivery.
pub unsafe fn set_tss_rsp0(cpu_id: u32, rsp0: u64) {
    // SAFETY: caller guarantees the selected TSS slot is initialized and owned
    // by the target CPU.
    let tss = unsafe { &mut *TSS_TABLE.get(cpu_id) };
    tss.rsp[0] = rsp0;
    percpu::set_kernel_rsp(cpu_id, rsp0);
}

fn cpu_index(cpu_id: u32) -> usize {
    (cpu_id as usize) % MAX_CPUS
}

#[inline(always)]
unsafe fn ltr(selector: u16) {
    // SAFETY: caller guarantees `selector` names a valid available 64-bit TSS.
    unsafe {
        asm!(
            "ltr ax",
            in("ax") selector,
            options(nostack, preserves_flags),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::Tss;
    use core::mem;

    #[test]
    fn tss_size_matches_amd64_abi() {
        assert_eq!(mem::size_of::<Tss>(), 104);
    }
}
