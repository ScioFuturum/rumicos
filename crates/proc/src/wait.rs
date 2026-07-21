//! `wait4` / `waitpid`: block until a child exits, then reap it.
//!
//! ## LOCK ORDER INVARIANT
//!
//! ```text
//!   process.children  →  ptable bucket  →  process.fd_table
//! ```
//!
//! Never acquire a higher-order lock while holding a lower-order one.
//! `ptable_find`/`ptable_remove` acquire and release their bucket lock
//! entirely inside the call, so calling them is safe even while holding the
//! `children` lock (the bucket is an independent lock, never nested with a
//! second held lock here). And critically: [`kernel_sched::thread_block`]
//! is only ever called with NO lock held (see the call site in
//! [`sys_wait4`]) — blocking with a spinlock held would deadlock every
//! other CPU that touches the same lock.
//!
//! ## Zombie lifecycle
//!
//! `Process::exit` marks the process `Zombie`, wakes its parent, and stops
//! executing — but frees nothing and stays in `ptable`. The zombie exists
//! solely so the parent can read its `exit_code`. [`reap_zombie`] is what
//! actually frees the child's kernel stack, address space, and `Process`
//! frame, and removes it from both process tables. Until then only the
//! ~4 KiB `Process` frame plus the kernel stack sit idle (the thread is
//! `Dead` and never runs again).
//!
//! ## SIGCHLD + wait ordering
//!
//! A child exit does BOTH: raises `SIGCHLD` pending on the parent AND wakes
//! the parent's wait queue. So a parent blocked in `wait4` returns normally
//! (having found and reaped the zombie); then, on its return to user mode,
//! the syscall-return signal hook delivers the pending `SIGCHLD`. Both
//! happen, wait4 first — standard POSIX behavior.

use crate::process::{Process, ProcessState};
use crate::syscall::is_user_ptr;
use core::sync::atomic::Ordering;

/// Linux-compatible `wait4` syscall number.
pub const SYS_WAIT4: u64 = 61;
/// `wait4` option: return immediately (0) if no child has exited yet.
pub const WNOHANG: u32 = 1;

const ECHILD: i64 = -10;
const EINVAL: i64 = -22;

/// Direct-map base — a `Process`/kernel-stack physical frame `f` is reached
/// at `DMAP + f`, and a `Process` pointer maps back to its frame by
/// subtracting this. Matches the value used throughout kernel-proc. Only
/// the target-only `reap_zombie` frees frames, hence the cfg gate.
#[cfg(target_os = "none")]
const DMAP: usize = 0xFFFF_8000_0000_0000;

/// Encode an exit code into a `wait`-style status word.
///
/// This kernel only produces normally-exited children, so the layout is the
/// POSIX `WIFEXITED` case: low 7 bits (the "terminated by signal" field)
/// are zero — making `WIFEXITED(status)` true — and the exit code occupies
/// bits 15:8, where `WEXITSTATUS(status) = (status >> 8) & 0xFF` reads it.
/// Only the low 8 bits of the code survive, exactly like real Unix
/// (`exit(-1)` → 255, `exit(256)` → 0).
pub fn encode_exit_status(exit_code: i32) -> u32 {
    ((exit_code as u32) & 0xFF) << 8
}

/// `wait4(pid, status, options, rusage)`.
///
/// * `pid > 0`  — wait for that specific child.
/// * `pid == -1` or `pid == 0` — wait for any child (process groups aren't
///   implemented, so `0` is treated as "any" rather than "my group").
/// * `pid < -1` — process-group wait, unimplemented → `ECHILD`.
///
/// Blocks on the caller's own `wait_queue` until a matching child is a
/// zombie, unless `WNOHANG` is set (then returns `0` if none is ready).
/// `rusage` is ignored (always pass NULL).
pub fn sys_wait4(pid: i64, status_ptr: u64, options: u32, _rusage: u64) -> i64 {
    let parent = crate::syscall::current_process();
    if parent.is_null() {
        return EINVAL;
    }
    // pid < -1 selects a process group; unsupported.
    if pid < -1 {
        return ECHILD;
    }

    // On the host there is no scheduler, so the block step returns instead
    // of looping — the never_loop lint is expected there and suppressed;
    // the real kernel build keeps the lint active (and does loop).
    #[cfg_attr(not(target_os = "none"), allow(clippy::never_loop))]
    loop {
        // No children at all — nothing to ever wait for.
        // SAFETY: current_process returned this thread's live PCB.
        if unsafe { (*parent).child_count.load(Ordering::Acquire) } == 0 {
            return ECHILD;
        }

        // Sample the child-exit generation BEFORE scanning. `Process::exit`
        // bumps this counter (then wakes) whenever a child of `parent`
        // becomes a zombie. If a child exits at any point after this load —
        // including in the window between the scan below and the block —
        // the counter will differ from `seen_seq`, and the under-lock
        // re-check in `thread_block_if` will decline to block. This is what
        // closes the lost-wakeup window (see A1/A2 in the checkpoint).
        // SAFETY: parent is live.
        let seen_seq = unsafe { (*parent).child_exit_seq.load(Ordering::Acquire) };

        // SAFETY: parent is live; find_zombie takes and releases the
        // children lock and any ptable bucket lock internally.
        if let Some((child_pid, exit_code)) = unsafe { find_zombie(parent, pid) } {
            if status_ptr != 0 && is_user_ptr(status_ptr as usize, 4) {
                let encoded = encode_exit_status(exit_code);
                // SAFETY: status_ptr is a validated 4-byte user address in
                // this process's (currently active) address space; STAC
                // lifts SMAP for the guarded supervisor write.
                #[cfg(target_os = "none")]
                unsafe {
                    core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
                    core::ptr::write(status_ptr as *mut u32, encoded);
                    core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
                }
                #[cfg(not(target_os = "none"))]
                let _ = encoded;
            }
            // SAFETY: the child is a zombie belonging to this parent.
            unsafe { reap_zombie(parent, child_pid) };
            return child_pid as i64;
        }

        if options & WNOHANG != 0 {
            return 0;
        }

        // Block until a child exits, but only if no exit has been signalled
        // since `seen_seq` was sampled. thread_block_if re-reads the counter
        // UNDER the wait_queue lock — the same lock Process::exit's wake_one
        // acquires — so an exit that races this decision is serialized: it
        // either bumps the counter before we check (we don't block, we loop
        // and reap) or finds our thread already enqueued (and wakes it). No
        // lock is held across the block; find_zombie already released the
        // children lock, and the closure only does a lock-free atomic load.
        // SAFETY: this thread's own syscall context; no locks held; the
        // closure takes no locks. parent is live.
        #[cfg(target_os = "none")]
        unsafe {
            kernel_sched::thread_block_if(&(*parent).wait_queue, || {
                (*parent).child_exit_seq.load(Ordering::Acquire) == seen_seq
            });
        }
        // On host there is no scheduler to block on; avoid an infinite spin.
        #[cfg(not(target_os = "none"))]
        {
            let _ = seen_seq;
            return 0;
        }
    }
}

/// Scan `parent`'s children for a zombie matching the `pid` filter and
/// return `(child_pid, exit_code)`, or `None`.
///
/// # Safety
/// `parent` must be a live `Process`.
pub(crate) unsafe fn find_zombie(parent: *mut Process, pid: i64) -> Option<(u32, i32)> {
    if pid < -1 {
        return None;
    }
    // SAFETY: parent is live. Snapshot the child PIDs under the lock, then
    // drop it BEFORE any ptable_find — respecting the children → ptable lock
    // order and never holding children across the ptable bucket lock.
    // Iterate the children array ELEMENT-BY-ELEMENT under the lock rather
    // than copying it out first (`let snap = *guard`). rustc 1.97.0
    // miscompiles that aggregate `[u32; MAX_CHILDREN]` copy — the snapshot
    // read phantom garbage where the real array (read element-wise by
    // add_child) held the true child PIDs, so wait4 never matched its own
    // child. Same aggregate-copy miscompile family as the vmas bug in
    // address_space.rs. Indexed reads compile correctly.
    //
    // ptable_find is called WHILE holding the children lock: that is the
    // documented children → ptable lock order (never the reverse), so it
    // cannot deadlock — no path acquires children while holding a ptable
    // bucket.
    // SAFETY: parent is live.
    let ch = unsafe { (*parent).children.lock() };
    for i in 0..crate::process::MAX_CHILDREN {
        let child_pid = ch[i];
        if child_pid == 0 {
            continue;
        }
        if pid > 0 && child_pid != pid as u32 {
            continue;
        }
        if let Some(child) = crate::ptable::ptable_find(child_pid) {
            // SAFETY: ptable only holds live Process pointers; the child
            // cannot be reaped concurrently (only this parent reaps, and
            // it is right here).
            if unsafe { (*child).state } == ProcessState::Zombie {
                let code = unsafe { (*child).exit_code };
                return Some((child_pid, code));
            }
        }
    }
    None
}

/// Free every resource of the zombie `child_pid` and remove it from both
/// process tables. After this returns the child pointer is INVALID.
///
/// Free order (each step must precede the next):
///   1. `remove_child` — drop it from the parent's children list.
///   2. `ptable_remove` — the PID becomes reusable.
///   3. `unregister_process` — drop the flat thread-keyed table entry, or
///      `current_process` would later dereference this freed frame.
///   4. `AddressSpace::drop_ref` — under `CLONE_VM` this only tears the
///      address space down when the last sharer drops it.
///   5. free the kernel stack — an order-`KSTACK_ORDER` block; the Thread
///      TCB lives *inside* it (see `kernel_sched::thread`), so this frees
///      the thread too. There is no separate thread frame.
///   6. free the `Process` frame LAST.
///
/// # Safety
/// `parent` must be live and `child_pid` must name a zombie child of it.
pub(crate) unsafe fn reap_zombie(parent: *mut Process, child_pid: u32) {
    debug_assert!(!parent.is_null());
    let Some(child) = crate::ptable::ptable_find(child_pid) else {
        return;
    };
    // SAFETY: caller guarantees the child is a live zombie.
    debug_assert!(
        unsafe { (*child).state } == ProcessState::Zombie,
        "reap_zombie called on a non-zombie"
    );

    // Read everything we need out of the child BEFORE freeing any of its
    // storage — the Thread lives inside the kstack we are about to free,
    // and the child pointer itself points into the Process frame.
    // SAFETY: child is live until we free its frames below.
    let (thread, address_space) = unsafe { ((*child).thread, (*child).address_space) };

    // Step 1 + 2 + 3: unlink from every table.
    // SAFETY: parent is live; this is the parent's own reaping thread.
    unsafe { crate::process::remove_child(parent, child_pid) };
    crate::ptable::ptable_remove(child_pid);
    crate::syscall::unregister_process(child);

    // fd_table needs no attention here: `Process::exit` already closed
    // every fd (running the full `vnode_release` hook per entry, balancing
    // `clone_for_fork`'s inc_ref) before the process became a zombie, so
    // by reap time the table is empty. Its storage is inside the Process
    // frame freed in the last step.

    #[cfg(target_os = "none")]
    unsafe {
        // Step 4: drop this process's reference to its address space.
        crate::address_space::AddressSpace::drop_ref(address_space);

        // Step 5: free the kernel stack (which contains the Thread TCB).
        // kernel-memory exposes only free_frame, so free the order-block one
        // 4 KiB frame at a time (NumaBuddy coalesces internally) — the same
        // approach exec.rs::free_elf_buf uses.
        // SAFETY: thread is non-null for a real process; kstack_phys is the
        // base of its order-KSTACK_ORDER allocation. Read it before freeing.
        let kstack_phys = (*thread).kstack_phys;
        let frames = 1u64 << kernel_sched::KSTACK_ORDER;
        for i in 0..frames {
            kernel_memory::free_frame(kernel_memory::PhysAddr::new(kstack_phys + i * 4096));
        }

        // Step 6: free the Process frame LAST. `child` is invalid after this.
        let child_phys = (child as usize - DMAP) as u64;
        kernel_memory::free_frame(kernel_memory::PhysAddr::new(child_phys));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (thread, address_space);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syscall_numbers_match_linux() {
        assert_eq!(SYS_WAIT4, 61);
        assert_eq!(WNOHANG, 1);
    }

    #[test]
    fn encode_status_zero_is_wifexited_zero() {
        let s = encode_exit_status(0);
        // WIFEXITED: low 7 bits == 0.
        assert_eq!(s & 0x7f, 0);
        // WEXITSTATUS == 0.
        assert_eq!((s >> 8) & 0xff, 0);
    }

    #[test]
    fn encode_status_42_round_trips() {
        let s = encode_exit_status(42);
        assert_eq!(s & 0x7f, 0, "WIFEXITED must hold");
        assert_eq!((s >> 8) & 0xff, 42, "WEXITSTATUS == 42");
    }

    #[test]
    fn encode_status_negative_one_is_255() {
        // exit(-1) shows up as 255 — only the low 8 bits survive.
        assert_eq!((encode_exit_status(-1) >> 8) & 0xff, 255);
    }

    #[test]
    fn encode_status_256_overflows_to_zero() {
        assert_eq!((encode_exit_status(256) >> 8) & 0xff, 0);
    }

    // ── lost-wakeup fix: the child-exit generation counter ────────────────
    //
    // The real race needs two CPUs and is unreachable while threads are
    // pinned to CPU0, so these test the *decision logic* that closes the
    // window: sys_wait4 samples `child_exit_seq` before scanning, and blocks
    // only if it is unchanged when re-checked under the wait_queue lock
    // (modelled here by the same load `thread_block_if`'s closure performs).

    #[test]
    fn block_predicate_is_true_when_no_child_exited_since_sample() {
        let p = crate::process::test_process();
        let seen = p.child_exit_seq.load(Ordering::Acquire);
        // No exit happened → the block predicate (seq unchanged) holds, so
        // the thread would legitimately block.
        let should_block = p.child_exit_seq.load(Ordering::Acquire) == seen;
        assert!(should_block, "with no exit, wait4 must be allowed to block");
    }

    #[test]
    fn block_predicate_is_false_when_a_child_exited_in_the_window() {
        let p = crate::process::test_process();
        let seen = p.child_exit_seq.load(Ordering::Acquire);
        // Simulate Process::exit bumping the parent's counter in the window
        // between wait4's scan and its block decision.
        p.child_exit_seq.fetch_add(1, Ordering::AcqRel);
        // The under-lock re-check now sees a different generation → wait4
        // declines to block, loops, and reaps instead. This is exactly the
        // lost wakeup that the old thread_block() could not prevent.
        let should_block = p.child_exit_seq.load(Ordering::Acquire) == seen;
        assert!(
            !should_block,
            "an exit in the window must force wait4 NOT to block"
        );
    }

    #[test]
    fn generation_counter_starts_at_zero() {
        let p = crate::process::test_process();
        assert_eq!(p.child_exit_seq.load(Ordering::Acquire), 0);
    }

    #[test]
    fn add_child_then_present() {
        let p = crate::process::test_process();
        let pp = &p as *const Process as *mut Process;
        // SAFETY: pp points at a live stack Process for this test.
        unsafe {
            assert!(crate::process::add_child(pp, 7));
        }
        let ch = p.children.lock();
        assert!(ch.contains(&7));
        assert_eq!(p.child_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn remove_child_clears_slot_and_count() {
        let p = crate::process::test_process();
        let pp = &p as *const Process as *mut Process;
        // SAFETY: pp is a live stack Process.
        unsafe {
            crate::process::add_child(pp, 9);
            crate::process::remove_child(pp, 9);
        }
        let ch = p.children.lock();
        assert!(!ch.contains(&9), "slot must be cleared to 0");
        assert_eq!(p.child_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn add_child_up_to_max_then_full() {
        let p = crate::process::test_process();
        let pp = &p as *const Process as *mut Process;
        // SAFETY: pp is a live stack Process.
        unsafe {
            for pid in 1..=crate::process::MAX_CHILDREN as u32 {
                assert!(crate::process::add_child(pp, pid), "child {pid} should fit");
            }
            // The (MAX_CHILDREN+1)-th must fail.
            assert!(
                !crate::process::add_child(pp, 999),
                "list must be full at MAX_CHILDREN"
            );
            assert!(crate::process::children_full(pp));
        }
        assert_eq!(
            p.child_count.load(Ordering::Relaxed),
            crate::process::MAX_CHILDREN as u32
        );
    }

    #[test]
    fn find_zombie_with_no_children_is_none() {
        let p = crate::process::test_process();
        let pp = &p as *const Process as *mut Process;
        // Empty children list → None without ever touching ptable.
        // SAFETY: pp is a live stack Process with an all-zero children list.
        unsafe {
            assert_eq!(find_zombie(pp, -1), None);
            assert_eq!(find_zombie(pp, 5), None);
        }
    }

    #[test]
    fn find_zombie_rejects_process_group_pid() {
        let p = crate::process::test_process();
        let pp = &p as *const Process as *mut Process;
        // SAFETY: pp is a live stack Process.
        unsafe {
            assert_eq!(find_zombie(pp, -2), None, "pid < -1 is a group wait");
        }
    }

    #[test]
    fn ptable_find_after_remove_is_none_regression() {
        // Guards the reap_zombie step-2 assumption: once ptable_remove runs,
        // the PID is no longer findable.
        crate::ptable::ptable_insert(54321, 0xabc0_0000 as *mut Process);
        assert!(crate::ptable::ptable_find(54321).is_some());
        crate::ptable::ptable_remove(54321);
        assert_eq!(crate::ptable::ptable_find(54321), None);
    }
}
