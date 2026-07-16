use crate::cpuinfo;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

pub static AP_READY_COUNT: AtomicU32 = AtomicU32::new(0);
pub static SCHEDULER_STARTED: AtomicBool = AtomicBool::new(false);

/// Called by trampoline assembly in 64-bit long mode.
///
/// Stack is already loaded by the trampoline. This function never returns.
///
/// # Safety
/// Must be entered exactly once per AP after the BSP has populated CpuInfo and
/// patched the trampoline with a valid stack and entry address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ap_entry_rust() -> ! {
    kernel_apic::init_ap();
    let my_apic_id = kernel_apic::apic_id();
    let cpu_id =
        cpuinfo::apic_id_to_cpu_id(my_apic_id).expect("AP APIC ID not found in CpuInfo table");

    kernel_cpu::init_cpu(cpu_id);
    kernel_apic::init_ap();

    cpuinfo::set_cpu_online(cpu_id);
    fence(Ordering::SeqCst);
    AP_READY_COUNT.fetch_add(1, Ordering::Release);

    while !SCHEDULER_STARTED.load(Ordering::Acquire) {
        // SAFETY: PAUSE is valid in a spin-wait loop and does not touch memory.
        unsafe { core::arch::asm!("pause", options(nomem, nostack)) };
    }

    while !kernel_sched::SCHEDULER_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    kernel_apic::set_timer_oneshot(1);
    // SAFETY: this AP has CPU, IDT, LAPIC, and scheduler state initialized
    // before interrupts are enabled.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    kernel_sched::idle_loop();
}
