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
    // Mirror the BSP's CR4 feature bits (SMAP/SMEP/PGE/PCIDE): the
    // trampoline only set PAE, and running user threads on an AP whose
    // CR4.SMAP is clear makes the kernel's own STAC/CLAC user-copy
    // sequences #UD. See kernel_paging::tlb::apply_bsp_cr4_on_ap.
    // SAFETY: AP bring-up, before this CPU runs any user thread; the
    // trampoline's CR3 low bits are zero so PCIDE can be set.
    unsafe { kernel_paging::tlb::apply_bsp_cr4_on_ap() };
    // CR4.OSXSAVE was just mirrored from the BSP; XCR0 is not part of CR4, so
    // program it on this AP too or its XSAVE/XRSTOR in the context switch
    // would fault. Enables exactly x87 + SSE, matching the BSP.
    // SAFETY: AP bring-up, CR4.OSXSAVE set by the mirror above.
    unsafe { kernel_arch_x86_64::xsave::enable_xcr0_x87_sse() };

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

    // Deliberately NOT arming this AP's LAPIC timer. User threads are
    // pinned to the BSP's run queue for now (see kernel_sched::
    // enqueue_thread), so an AP tick would only add concurrent schedule()
    // entries — the exact latent-SMP-race surface this checkpoint chose
    // not to open (docs/shell-checkpoint.md, Known limitations). The AP
    // still enables interrupts: it must take TLB-shootdown and reschedule
    // IPIs (both actually deliverable only since the x2APIC
    // software-enable fix in kernel_apic::lapic::init_lapic).
    // SAFETY: this AP has CPU, IDT, LAPIC, and scheduler state initialized
    // before interrupts are enabled.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    kernel_sched::idle_loop();
}
