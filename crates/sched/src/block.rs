use crate::thread::{Thread, ThreadState};
use crate::waitqueue::{WaitNode, WaitQueue};

/// Block the current thread on `wq` until a waker makes it runnable again.
///
/// # Safety
/// Must be called from kernel thread context, not interrupt context. The caller
/// must not hold locks that the eventual waker must acquire.
pub unsafe fn thread_block(wq: &WaitQueue) {
    let cpu_id = crate::current_cpu_id();
    let cur: *mut Thread = crate::percpu::sched_cpu(cpu_id).lock().current;
    assert!(!cur.is_null(), "thread_block called from idle/null context");

    let mut node = WaitNode::new(cur);

    // SAFETY: interrupts must stay disabled from enqueue through schedule() to
    // avoid a timer-driven lost wakeup window.
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };

    wq.with_locked(|inner| {
        // SAFETY: `node` lives on this blocked thread's kernel stack until
        // thread_block returns after wakeup.
        unsafe {
            inner.enqueue_raw(&mut node);
            (*cur).state = ThreadState::Blocked;
        }
    });

    // SAFETY: IF=0 and current thread is marked Blocked and enqueued.
    unsafe { crate::schedule(cpu_id) };

    // SAFETY: the thread has resumed after being made runnable.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    debug_assert!(node.woken, "thread_block returned without woken flag");
}

pub fn wake_one(wq: &WaitQueue) -> bool {
    wq.wake_n(1) > 0
}

pub fn wake_one_from_irq(wq: &WaitQueue) -> bool {
    debug_assert!(
        !crate::interrupts_enabled(),
        "wake_one_from_irq called with interrupts enabled"
    );
    wake_one(wq)
}

pub fn wake_all(wq: &WaitQueue) -> u32 {
    wq.wake_all()
}

pub fn wake_one_and_yield(wq: &WaitQueue) {
    wake_one(wq);
    crate::thread_yield();
}
