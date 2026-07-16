use crate::percpu;
use crate::queue::MLFQ_LEVELS;
use crate::thread::{self, Thread};

pub fn create_idle_thread(cpu_id: u32) -> *mut Thread {
    // SAFETY: `idle_entry` is a valid non-returning kernel function.
    let idle = unsafe { thread::alloc_thread(0, idle_entry, (MLFQ_LEVELS - 1) as u8) };
    // SAFETY: `idle` points to a freshly initialized TCB.
    unsafe { (*idle).cpu_id = cpu_id };
    idle
}

pub fn register_idle(cpu_id: u32) {
    let idle = create_idle_thread(cpu_id);
    percpu::set_idle_thread(cpu_id, idle);
}

fn idle_entry() -> ! {
    loop {
        // SAFETY: the idle thread enables interrupts, halts until the next IRQ,
        // then disables interrupts before returning to scheduler-controlled code.
        unsafe {
            core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack));
        }
    }
}
