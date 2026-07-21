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

/// Block the current thread on `wq` — but only if `should_block()`, checked
/// **under the queue lock**, still reports the wait condition holds.
///
/// This is the lost-wakeup-safe variant of [`thread_block`]. The caller's
/// condition is re-evaluated at the exact instant the thread is about to be
/// enqueued, holding the same lock a waker must acquire to deliver a wake.
/// So a waker that runs concurrently is serialized either entirely before
/// the check (→ `should_block` returns false, we do not sleep) or entirely
/// after the enqueue (→ the wake finds our node). There is no window in
/// which a wake is lost, and — because the enqueue-or-not decision is atomic
/// under the lock — no thread is ever left enqueued-but-should-not-be, so no
/// stale run-queue entry can arise from cancelling a wait.
///
/// `should_block` MUST be cheap and lock-free (typically a single atomic
/// load compared against a value sampled before the caller's own condition
/// scan). It runs with interrupts disabled while the queue's spinlock is
/// held: it must not block, allocate, or acquire another lock.
///
/// Returns `true` if it actually blocked (and has since been woken), `false`
/// if `should_block()` was false and it returned without sleeping.
///
/// # Safety
/// Same context as [`thread_block`]: kernel thread context, not interrupt
/// context, and the caller must hold no lock that the waker must acquire.
pub unsafe fn thread_block_if(wq: &WaitQueue, should_block: impl FnOnce() -> bool) -> bool {
    let cpu_id = crate::current_cpu_id();
    let cur: *mut Thread = crate::percpu::sched_cpu(cpu_id).lock().current;
    assert!(!cur.is_null(), "thread_block_if called from idle/null context");

    let mut node = WaitNode::new(cur);

    // SAFETY: interrupts stay disabled from the under-lock check through
    // schedule() so the local timer cannot open a lost-wakeup window either.
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };

    let mut blocked = false;
    wq.with_locked(|inner| {
        // Re-check the condition under the queue lock. Only enqueue + mark
        // Blocked if it still holds; otherwise leave the thread Running.
        if should_block() {
            // SAFETY: `node` lives on this thread's kernel stack until
            // thread_block_if returns after wakeup.
            unsafe {
                inner.enqueue_raw(&mut node);
                (*cur).state = ThreadState::Blocked;
            }
            blocked = true;
        }
    });

    if blocked {
        // SAFETY: IF=0 and current thread is marked Blocked and enqueued.
        unsafe { crate::schedule(cpu_id) };
        debug_assert!(node.woken, "thread_block_if returned without woken flag");
    }

    // SAFETY: restore interrupts; the thread is Running again either way.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    blocked
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
