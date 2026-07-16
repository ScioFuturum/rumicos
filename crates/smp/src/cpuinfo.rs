use crate::MAX_CPUS;
use crate::ap_entry::AP_READY_COUNT;
use core::sync::atomic::{AtomicU32, Ordering};
use kernel_sync::SpinLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuInfo {
    pub cpu_id: u32,
    pub apic_id: u32,
    pub online: bool,
    pub stack_top: u64,
}

impl CpuInfo {
    pub const fn zeroed() -> Self {
        Self {
            cpu_id: 0,
            apic_id: 0,
            online: false,
            stack_top: 0,
        }
    }
}

static CPU_TABLE: SpinLock<[CpuInfo; MAX_CPUS]> =
    SpinLock::new([const { CpuInfo::zeroed() }; MAX_CPUS]);
static CPU_COUNT: AtomicU32 = AtomicU32::new(0);

pub fn register_cpu(cpu_id: u32, apic_id: u32, stack_top: u64) {
    let idx = cpu_id as usize;
    if idx >= MAX_CPUS {
        return;
    }

    let mut table = CPU_TABLE.lock();
    table[idx] = CpuInfo {
        cpu_id,
        apic_id,
        online: false,
        stack_top,
    };

    let wanted = cpu_id + 1;
    let mut current = CPU_COUNT.load(Ordering::Relaxed);
    while current < wanted {
        match CPU_COUNT.compare_exchange_weak(current, wanted, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

pub fn set_cpu_online(cpu_id: u32) {
    let idx = cpu_id as usize;
    if idx >= MAX_CPUS {
        return;
    }

    let mut table = CPU_TABLE.lock();
    table[idx].online = true;
}

pub fn apic_id_to_cpu_id(apic_id: u32) -> Option<u32> {
    let count = cpu_count().min(MAX_CPUS as u32) as usize;
    let table = CPU_TABLE.lock();
    table[..count]
        .iter()
        .find(|cpu| cpu.apic_id == apic_id)
        .map(|cpu| cpu.cpu_id)
}

/// The reverse of [`apic_id_to_cpu_id`]: the physical local-APIC ID for a
/// given scheduler `cpu_id`, or `None` if `cpu_id` was never registered via
/// [`register_cpu`] (out of range, or simply not brought up yet). New in
/// this checkpoint — needed to target a TLB-shootdown IPI (see
/// `kernel_proc::shootdown`) at a specific scheduler CPU rather than an
/// already-known APIC ID.
pub fn apic_id_for_cpu(cpu_id: u32) -> Option<u32> {
    let idx = cpu_id as usize;
    if idx >= MAX_CPUS {
        return None;
    }
    let count = cpu_count().min(MAX_CPUS as u32) as usize;
    if idx >= count {
        return None;
    }
    let table = CPU_TABLE.lock();
    Some(table[idx].apic_id)
}

pub fn online_count() -> u32 {
    AP_READY_COUNT.load(Ordering::Acquire) + 1
}

pub fn cpu_count() -> u32 {
    CPU_COUNT.load(Ordering::Relaxed)
}

pub fn stack_top(cpu_id: u32) -> Option<u64> {
    let idx = cpu_id as usize;
    if idx >= MAX_CPUS {
        return None;
    }

    let table = CPU_TABLE.lock();
    let stack = table[idx].stack_top;
    if stack == 0 { None } else { Some(stack) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_info_zeroed_is_offline() {
        let cpu = CpuInfo::zeroed();
        assert_eq!(cpu.cpu_id, 0);
        assert_eq!(cpu.apic_id, 0);
        assert!(!cpu.online);
        assert_eq!(cpu.stack_top, 0);
    }

    #[test]
    fn apic_id_lookup_finds_registered_cpu() {
        register_cpu(1, 5, 0x1000);
        assert_eq!(apic_id_to_cpu_id(5), Some(1));
    }

    #[test]
    fn apic_id_for_cpu_is_the_reverse_lookup() {
        register_cpu(2, 7, 0x2000);
        assert_eq!(apic_id_for_cpu(2), Some(7));
    }

    #[test]
    fn apic_id_for_cpu_out_of_range_is_none() {
        assert_eq!(apic_id_for_cpu(MAX_CPUS as u32), None);
    }

    #[test]
    fn online_count_includes_bsp_when_no_ap_ready() {
        assert_eq!(online_count(), 1);
    }
}
