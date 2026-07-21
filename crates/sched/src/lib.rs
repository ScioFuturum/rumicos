#![no_std]
#![cfg(target_arch = "x86_64")]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod block;
pub mod futex;
pub mod idle;
pub mod percpu;
pub mod queue;
pub mod steal;
pub mod switch;
pub mod thread;
pub mod waitqueue;

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub use block::{
    thread_block, thread_block_if, wake_all, wake_one, wake_one_and_yield, wake_one_from_irq,
};
pub use futex::{FUTEX_BUCKETS, KMutex, KMutexGuard, futex_requeue, futex_wait, futex_wake};
pub use percpu::{SchedCpu, sched_cpu};
pub use queue::{BOOST_INTERVAL, MLFQ_LEVELS, MlfqQueue, timeslice};
pub use steal::{busiest_cpu, least_loaded_cpu, try_steal};
pub use thread::{Context, KSTACK_ORDER, KSTACK_SIZE, Thread, ThreadId, ThreadState, alloc_tid};
pub use waitqueue::{WaitNode, WaitQueue};

pub static SCHEDULER_READY: AtomicBool = AtomicBool::new(false);

/// Context-switch hook, invoked with IF=0 immediately before every switch
/// to `next` (after the scheduler lock is released). kernel-proc registers
/// an implementation that reloads CR3 when `next` belongs to a different
/// address space — kernel-sched itself has no notion of processes or page
/// tables, and kernel-proc already depends on kernel-sched, so this follows
/// the same registration-hook pattern kernel-proc's own `CHAIN_HANDLER`/
/// `EXEC_LOADER` statics use to invert a dependency Cargo would otherwise
/// reject as a cycle.
static CONTEXT_SWITCH_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register `f` to run right before every context switch, with the thread
/// being switched TO and the CPU it will run on.
///
/// `f` runs with interrupts disabled, after the scheduler lock has been
/// released, on the *outgoing* thread's kernel stack. It must not take the
/// scheduler lock, block, or enable interrupts.
pub fn register_context_switch_hook(f: unsafe fn(next: *mut Thread, cpu_id: u32)) {
    CONTEXT_SWITCH_HOOK.store(f as usize, Ordering::Release);
}

#[inline(always)]
fn run_context_switch_hook(next: *mut Thread, cpu_id: u32) {
    let f = CONTEXT_SWITCH_HOOK.load(Ordering::Acquire);
    if f != 0 {
        // SAFETY: only register_context_switch_hook ever stores here, and it
        // only accepts fns of this exact signature.
        let f: unsafe fn(*mut Thread, u32) = unsafe { core::mem::transmute(f) };
        // SAFETY: both call sites run with IF=0, after the scheduler lock is
        // released, immediately before switching to `next`.
        unsafe { f(next, cpu_id) };
    }
}

pub fn init(cpu_count: u32) {
    let cpu_count = cpu_count.min(percpu::MAX_CPUS as u32);
    for cpu in 0..cpu_count {
        percpu::init_sched_cpu(cpu);
        idle::register_idle(cpu);
    }
    SCHEDULER_READY.store(true, Ordering::Release);
}

/// Main scheduling function.
///
/// # Safety
/// Must be called with interrupts disabled on the current CPU.
#[inline(always)]
pub unsafe fn schedule(cpu_id: u32) {
    let rflags: u64;
    // SAFETY: pushfq/pop reads architectural flags and leaves RFLAGS unchanged.
    unsafe {
        core::arch::asm!(
            "pushfq; pop {}",
            out(reg) rflags,
            options(nomem, preserves_flags)
        );
    }
    debug_assert!(
        rflags & (1 << 9) == 0,
        "schedule() called with interrupts enabled"
    );

    let (cur, next) = {
        let mut sc = sched_cpu(cpu_id).lock();
        sc.tick_count = sc.tick_count.wrapping_add(1);
        if sc.tick_count.is_multiple_of(BOOST_INTERVAL as u64) {
            sc.run_queue.boost_all();
        }

        let cur = sc.current;
        let idle = sc.idle_thread;
        if !cur.is_null() && cur != idle {
            // SAFETY: `current` is installed only from live TCB pointers.
            let thread = unsafe { &mut *cur };
            match thread.state {
                ThreadState::Running => {
                    if !thread::charge_tick(thread) {
                        return;
                    }
                    thread.state = ThreadState::Runnable;
                    let _ = sc.run_queue.push(cur, thread.priority as usize);
                }
                ThreadState::Runnable => {
                    let _ = sc.run_queue.push(cur, thread.priority as usize);
                }
                ThreadState::Blocked | ThreadState::Dead => {
                    thread.ticks_used = 0;
                }
            }
        }

        let next = sc
            .run_queue
            .pop()
            .or_else(|| steal::try_steal(cpu_id, &mut sc))
            .unwrap_or(idle);

        if next == cur {
            if !cur.is_null() {
                // SAFETY: `cur` remains the running thread because no switch occurs.
                unsafe { (*cur).state = ThreadState::Running };
            }
            return;
        }

        sc.current = next;
        // SAFETY: `next` comes from a run queue or this CPU's idle TCB.
        unsafe {
            (*next).state = ThreadState::Running;
            (*next).cpu_id = cpu_id;
            switch::update_tss_rsp0(cpu_id, (*next).kstack_top);
        }
        (cur, next)
    };

    // Lock released - switch_context must follow immediately.
    run_context_switch_hook(next, cpu_id);
    if cur.is_null() {
        // SAFETY: `next` is a valid runnable TCB and no previous context exists.
        unsafe { switch::switch_first(next) };
    } else {
        // SAFETY: IF=0 and both TCB pointers are valid.
        unsafe { switch::switch_context(cur, next) };
    }
}

pub fn thread_yield() {
    let cpu_id = current_cpu_id();
    // SAFETY: CLI/STI bracket the scheduler critical section on this CPU.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
        schedule_yield(cpu_id);
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

/// Spawn a new kernel thread at priority 0.
pub fn spawn(entry: fn() -> !, _name: &'static str) -> ThreadId {
    let id = alloc_tid();
    // SAFETY: callers provide a valid non-returning kernel entry point.
    let thread = unsafe { thread::alloc_thread(id, entry, 0) };
    let target_cpu = least_loaded_cpu();
    let queued = sched_cpu(target_cpu).lock().run_queue.push(thread, 0);
    assert!(queued, "scheduler run queue full");
    id
}

/// Allocate a non-enqueued kernel thread whose entry point uses the C ABI.
///
/// # Safety
/// `entry` must be a valid non-returning kernel function. The returned thread
/// pointer must either be enqueued with `enqueue_thread` or otherwise kept out
/// of scheduler queues; it must not be freed while reachable by the scheduler.
pub unsafe fn alloc_kernel_thread_raw(
    entry: unsafe extern "C" fn() -> !,
    priority: u8,
) -> *mut Thread {
    let id = alloc_tid();
    // SAFETY: caller supplies a valid non-returning kernel entry point.
    unsafe { thread::alloc_thread_raw(id, entry, priority) }
}

/// Hook that pokes remote CPUs after a thread lands on one of their run
/// queues (registered by the kernel crate: a broadcast reschedule IPI).
///
/// Load-bearing, not an optimization: only the BSP has a periodic timer,
/// and an idle AP parks in `sti; hlt` — it drains its own run queue only
/// when an interrupt arrives. Before this hook existed, a thread enqueued
/// onto an idle AP could sit there FOREVER: the AP never woke to pop it,
/// and the BSP's `try_steal` never fired because its own queue always
/// holds the `kidle` kernel thread (`schedule` only steals when the local
/// pop comes up empty). A shell pipeline's second stage landed exactly
/// there and starved.
static KICK_REMOTE_HOOK: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Register the remote-CPU kick (see [`KICK_REMOTE_HOOK`]). Called once at
/// boot by the kernel crate after the APs are online.
pub fn register_kick_hook(hook: fn()) {
    KICK_REMOTE_HOOK.store(hook as usize, core::sync::atomic::Ordering::Release);
}

pub(crate) fn kick_remote_cpus() {
    let hook = KICK_REMOTE_HOOK.load(core::sync::atomic::Ordering::Acquire);
    if hook != 0 {
        // SAFETY: register_kick_hook only ever stores a `fn()`.
        let hook: fn() = unsafe { core::mem::transmute(hook) };
        hook();
    }
}

pub fn enqueue_thread(thread: *mut Thread, priority: usize) {
    // Pin every new thread to the BSP's queue for now. This codifies what
    // was silently true anyway: only the BSP has a periodic timer, so only
    // its queue is reliably drained. Spreading via least_loaded_cpu()
    // parked threads on idle APs — and the FIRST time an AP actually ran a
    // user thread (this checkpoint's shell pipeline) it exposed a stack of
    // latent SMP holes (AP LAPICs were software-disabled, so neither their
    // timers nor reschedule IPIs ever fired; with those fixed, true
    // concurrent scheduling then hit further unsolved races). Genuinely
    // spreading user threads across CPUs is deliberate follow-up work —
    // see docs/shell-checkpoint.md "Known limitations".
    let target_cpu = 0;
    let queued = sched_cpu(target_cpu)
        .lock()
        .run_queue
        .push(thread, priority);
    assert!(queued, "scheduler run queue full");
    if target_cpu != current_cpu_id() {
        kick_remote_cpus();
    }
}

pub fn current_thread() -> *mut Thread {
    sched_cpu(current_cpu_id()).lock().current
}

#[inline(always)]
pub fn current_cpu_id() -> u32 {
    kernel_cpu::current_cpu_id()
}

pub fn idle_loop() -> ! {
    loop {
        // SAFETY: STI;HLT is the canonical idle pair — STI takes effect
        // after the NEXT instruction, so no wakeup can slip in between.
        // A bare HLT (as before 2026-07-10) parked idle CPUs with IF=0
        // forever: schedule() reaches this via timer/interrupt context
        // (IF=0), so timer re-arms and TLB-shootdown IPIs aimed at an idle
        // CPU were never delivered and the initiating CPU spun forever in
        // shootdown_page's wait loop (observed on the first munmap of a
        // migrated process on the first real boot).
        unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
    }
}

unsafe fn schedule_yield(cpu_id: u32) {
    debug_assert!(!interrupts_enabled(), "schedule_yield() requires IF=0");

    let mut sc = sched_cpu(cpu_id).lock();
    let cur = sc.current;
    if cur.is_null() || cur == sc.idle_thread {
        drop(sc);
        // SAFETY: caller disabled interrupts.
        unsafe { schedule(cpu_id) };
        return;
    }

    // SAFETY: `current` is a live TCB for the running thread.
    unsafe {
        (*cur).state = ThreadState::Runnable;
        (*cur).ticks_used = 0;
    }
    // SAFETY: `cur` points to the live current thread.
    let level = unsafe { (*cur).priority as usize };
    let _ = sc.run_queue.push(cur, level);

    let next = sc
        .run_queue
        .pop()
        .or_else(|| steal::try_steal(cpu_id, &mut sc))
        .unwrap_or(sc.idle_thread);
    if next == cur {
        // SAFETY: `cur` remains the running thread because no switch occurs.
        unsafe { (*cur).state = ThreadState::Running };
        return;
    }

    sc.current = next;
    // SAFETY: `next` comes from a run queue or idle slot.
    unsafe {
        (*next).state = ThreadState::Running;
        (*next).cpu_id = cpu_id;
        switch::update_tss_rsp0(cpu_id, (*next).kstack_top);
    }
    drop(sc);

    run_context_switch_hook(next, cpu_id);
    // SAFETY: caller disabled interrupts and both contexts are valid.
    unsafe { switch::switch_context(cur, next) };
}

#[inline(always)]
pub(crate) fn interrupts_enabled() -> bool {
    let rflags: u64;
    // SAFETY: pushfq/pop reads architectural flags without modifying memory.
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {}",
            out(reg) rflags,
            options(nomem, preserves_flags)
        );
    }
    (rflags & (1 << 9)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU32;

    // CONTEXT_SWITCH_HOOK is a single process-wide static, so the
    // "unregistered is a no-op" and "registered hook fires" observations
    // must live in one sequential test rather than two independent #[test]
    // functions racing on the same slot.
    #[test]
    fn context_switch_hook_noop_until_registered_then_fires() {
        static SEEN_CPU: AtomicU32 = AtomicU32::new(u32::MAX);
        unsafe fn hook(_next: *mut Thread, cpu_id: u32) {
            SEEN_CPU.store(cpu_id, Ordering::SeqCst);
        }

        run_context_switch_hook(core::ptr::null_mut(), 7);
        assert_eq!(
            SEEN_CPU.load(Ordering::SeqCst),
            u32::MAX,
            "unregistered hook must be a silent no-op"
        );

        register_context_switch_hook(hook);
        run_context_switch_hook(core::ptr::null_mut(), 3);
        assert_eq!(SEEN_CPU.load(Ordering::SeqCst), 3, "hook must see the target cpu_id");
    }
}
