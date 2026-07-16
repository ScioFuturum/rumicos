use crate::tss;
use core::cell::UnsafeCell;
use kernel_arch_x86_64::msr::{IA32_GS_BASE, IA32_KERNEL_GS_BASE, wrmsr};

pub const CPU_RSP_USER: usize = 0;
pub const CPU_RSP_KERN: usize = 64;
pub const CPU_ID: usize = 128;

#[repr(C, align(64))]
pub struct PerCpuData {
    pub rsp_user: u64,
    pad0: [u8; 56],
    pub rsp_kern: u64,
    pad1: [u8; 56],
    pub cpu_id: u32,
    pad2: [u8; 60],
}

impl PerCpuData {
    pub const fn new() -> Self {
        Self {
            rsp_user: 0,
            pad0: [0; 56],
            rsp_kern: 0,
            pad1: [0; 56],
            cpu_id: 0,
            pad2: [0; 60],
        }
    }
}

impl Default for PerCpuData {
    fn default() -> Self {
        Self::new()
    }
}

struct PerCpuTable([UnsafeCell<PerCpuData>; tss::MAX_CPUS]);

unsafe impl Sync for PerCpuTable {}

impl PerCpuTable {
    const fn new() -> Self {
        Self([const { UnsafeCell::new(PerCpuData::new()) }; tss::MAX_CPUS])
    }

    fn get(&self, cpu_id: u32) -> *mut PerCpuData {
        self.0[(cpu_id as usize) % tss::MAX_CPUS].get()
    }
}

static PER_CPU: PerCpuTable = PerCpuTable::new();

pub fn install_per_cpu_gs(cpu_id: u32) {
    // SAFETY: each CPU writes its own per-CPU slot during serialized bring-up.
    let data = unsafe { &mut *PER_CPU.get(cpu_id) };
    data.cpu_id = cpu_id;
    if data.rsp_kern == 0 {
        data.rsp_kern = tss::kernel_stack_top(cpu_id);
    }
    let base = data as *mut PerCpuData as u64;
    // SAFETY: writing GS-base MSRs is a privileged boot-time operation. Both
    // bases point to per-CPU data until user GS state is introduced.
    unsafe {
        wrmsr(IA32_GS_BASE, base);
        wrmsr(IA32_KERNEL_GS_BASE, base);
    }
}

pub fn set_kernel_rsp(cpu_id: u32, rsp: u64) {
    // SAFETY: TSS initialization and per-CPU initialization are serialized for
    // the selected CPU slot.
    let data = unsafe { &mut *PER_CPU.get(cpu_id) };
    data.rsp_kern = rsp;
}

pub fn set_user_rsp(cpu_id: u32, rsp: u64) {
    // SAFETY: the selected CPU owns this slot; syscall entry also updates the
    // live CPU slot via GS when user mode exists.
    let data = unsafe { &mut *PER_CPU.get(cpu_id) };
    data.rsp_user = rsp;
}

#[inline(always)]
pub fn current_cpu_id() -> u32 {
    let cpu_id: u32;
    // SAFETY: `install_per_cpu_gs` installs a valid GSBASE before scheduler
    // use; `CPU_ID` is the stable offset of `PerCpuData::cpu_id`.
    unsafe {
        core::arch::asm!(
            "mov {cpu_id:e}, dword ptr gs:[{cpu_id_off}]",
            cpu_id = lateout(reg) cpu_id,
            cpu_id_off = const CPU_ID,
            options(nomem, nostack, preserves_flags),
        )
    };
    cpu_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem;

    #[test]
    fn percpu_rsp_slots_are_cache_line_separated() {
        assert_eq!(CPU_RSP_USER, 0);
        assert_eq!(CPU_RSP_KERN, 64);
        assert_eq!(CPU_ID, 128);
        assert_eq!(mem::align_of::<PerCpuData>(), 64);
        assert_eq!(mem::size_of::<PerCpuData>(), 192);
    }
}
