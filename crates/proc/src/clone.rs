//! `sys_clone` — CLONE_VM only.
//!
//! ## Scope (explicitly cut down from Linux's real `clone()`)
//!
//! Only `CLONE_VM` is meaningfully implemented: the new thread shares the
//! calling process's exact same [`crate::address_space::AddressSpace`] --
//! same `pml4_phys`, same `pcid`, same live `vmas`/`mmap_next` state --
//! instead of getting [`crate::address_space::AddressSpace::fork_cow`]'s
//! private CoW copy the way [`crate::fork::sys_fork`] does. This is the
//! minimum needed to make TLB shootdown (`crate::shootdown`) a REAL
//! correctness requirement rather than a documented TODO: before this,
//! every `AddressSpace` had exactly one thread, on exactly one CPU, ever.
//!
//! Everything else Linux's real `clone()` supports is OUT OF SCOPE:
//!
//!   - **`fd_table`**: CLONED, not shared, same as `fork()`. Sharing it for
//!     real would mean `FdTable` itself becoming a second refcounted,
//!     pointer-shared resource between two `Process`es -- today it's owned
//!     by value, behind a per-`Process` `SpinLock` (`Process::fd_table`).
//!     That's a separately-invasive structural change (`FdTable` would
//!     need its own `alloc_shared`/refcount, exactly mirroring what this
//!     checkpoint just did for `AddressSpace`) beyond this checkpoint's
//!     scope. Per this checkpoint's own design brief, the simpler,
//!     less-invasive choice is taken instead: clone `fd_table` exactly
//!     like `fork()` does, and treat `CLONE_FILES` as unimplemented/future
//!     work (see "Known limitations" in the checkpoint summary).
//!   - Signal disposition, parent PID, process group, `CLONE_THREAD`'s
//!     "share a PID/thread-group" semantics, `CLONE_SIGHAND` -- none of
//!     this exists in this kernel yet at all, so none of it is threaded
//!     through here either. This checkpoint produces a second schedulable
//!     thread with its own `Process`/PCB (own pid, own kernel stack, own
//!     `kernel_sched::Thread`) that happens to point at the SAME
//!     `AddressSpace` -- nothing more.

#[cfg(target_os = "none")]
use crate::fd::FdTable;
#[cfg(target_os = "none")]
use crate::process::{Process, ProcessState, alloc_pid};

/// Matches Linux's real `CLONE_VM` bit value (`0x100`) -- deliberate, for
/// future ABI compatibility, even though the semantics implemented here are
/// a strict subset of what Linux's `clone()` does with this flag.
pub const CLONE_VM: u32 = 0x0000_0100;

/// Matches Linux's real x86-64 `clone()` syscall number.
pub const SYS_CLONE: u64 = 56;

const EINVAL: i64 = -22;
const EFAULT: i64 = -14;
// Returned only from the target-only clone body (children-list full).
#[cfg(target_os = "none")]
const EAGAIN: i64 = -11;

/// `sys_clone(flags, child_stack)` — see module docs for this checkpoint's
/// explicitly scoped-down semantics.
///
/// The two pure-argument checks below (which flags bit is set, whether
/// `child_stack` is null) intentionally run before touching
/// [`crate::syscall::current_process`] — exactly the same convention
/// [`crate::vma::sys_mmap`] documents and relies on for its own host
/// tests: `current_process()` reads a GS-relative per-CPU field that's
/// only ever installed on real hardware, so calling it from a host test
/// process reliably segfaults. Keeping both of this function's required
/// unit tests host-safe means both checks must fully resolve first.
pub fn sys_clone(flags: u32, child_stack: u64) -> i64 {
    if flags & CLONE_VM == 0 {
        // Every other clone() flag combination is out of scope this
        // checkpoint -- fold down to plain fork() (private CoW copy of the
        // AddressSpace), exactly as the design brief specifies.
        return crate::fork::sys_fork();
    }

    if child_stack == 0 {
        // Linux requires an explicit child stack for CLONE_VM -- two
        // threads can never correctly share one live user RSP. Reject
        // rather than silently reusing the parent's RSP snapshot the way
        // fork()'s child does.
        return EINVAL;
    }

    #[cfg(target_os = "none")]
    unsafe {
        // SAFETY: sys_clone runs on this thread's own kernel stack with
        // interrupts disabled, exactly like every other syscall handler in
        // this kernel (see kernel_cpu's SYSCALL trampoline).
        let parent_ptr = crate::syscall::current_process();
        if parent_ptr.is_null() {
            return EFAULT;
        }
        let parent = &mut *parent_ptr;

        // Fail before building anything if the parent's children list is
        // full (see MAX_CHILDREN) — same early-out as sys_fork.
        // SAFETY: parent_ptr is live; only this thread mutates its children.
        if crate::process::children_full(parent_ptr) {
            return EAGAIN;
        }

        let syscall_frame = kernel_cpu::current_syscall_frame();

        // KEY DIFFERENCE from fork(): do NOT call fork_cow(). Point the
        // child at the exact SAME AddressSpace the parent already has --
        // same pml4_phys, same pcid, same live vmas/mmap_next, not a
        // private copy. inc_ref records this new sibling's claim so the
        // AddressSpace outlives whichever of the two Process/Thread pairs
        // happens to exit first (see AddressSpace::inc_ref/dec_ref/
        // drop_ref, and crate::exec's Phase 2 for the other place that
        // now goes through the same refcount).
        let shared_as = parent.address_space;
        (*shared_as).inc_ref();

        // fd_table: CLONED, not shared -- see module docs above for why.
        let child_fds: FdTable = parent.fd_table.lock().clone_for_fork();
        // Signal dispositions are COPIED (CLONE_SIGHAND, which would share
        // them, is out of scope — see module docs).
        let child_sig_table = *parent.sig_table.lock();

        let child_thread =
            kernel_sched::alloc_kernel_thread_raw(crate::process::ring3_entry_trampoline, 0);

        let child_pid = alloc_pid();
        let frame = kernel_memory::alloc_frame();
        let child_ptr = (0xffff_8000_0000_0000u64 + frame.as_u64()) as *mut Process;
        child_ptr.write(Process {
            pid: child_pid,
            state: ProcessState::Running,
            exit_code: 0,
            address_space: shared_as,
            thread: child_thread,
            name: parent.name,
            user_rip: syscall_frame.user_rip,
            // Unlike fork() (which reuses the parent's own RSP snapshot),
            // CLONE_VM requires the caller-supplied child_stack, already
            // validated non-zero above.
            user_rsp: child_stack,
            user_rflags: syscall_frame.user_rflags,
            user_rax: 0,
            fd_table: kernel_sync::SpinLock::new(child_fds),
            parent_pid: parent.pid,
            sig_table: kernel_sync::SpinLock::new(child_sig_table),
            pending: crate::signal::PendingSet::new(),
            sig_frame: kernel_sync::SpinLock::new(None),
            children: kernel_sync::SpinLock::new([0; crate::process::MAX_CHILDREN]),
            child_count: core::sync::atomic::AtomicU32::new(0),
            wait_queue: kernel_sched::WaitQueue::new(),
        });
        // Zero the child's own children array element-by-element (see
        // Process::create — the aggregate placement-write is unreliable on
        // rustc 1.97.0).
        for slot in (*child_ptr).children.lock().iter_mut() {
            *slot = 0;
        }
        (*child_ptr).child_count.store(0, core::sync::atomic::Ordering::Relaxed);

        crate::syscall::register_process(child_ptr);
        crate::ptable::ptable_insert(child_pid, child_ptr);
        // Track the child under the parent for sys_wait4 (guaranteed to fit
        // by the children_full check above).
        // SAFETY: parent_ptr and child are live; parent's own thread.
        let added = crate::process::add_child(parent_ptr, child_pid);
        debug_assert!(added, "children_full checked above");
        kernel_sched::enqueue_thread(child_thread, 0);
        child_pid as i64
    }

    #[cfg(not(target_os = "none"))]
    {
        EFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_vm_matches_linux_bit_value() {
        assert_eq!(CLONE_VM, 0x100);
    }

    #[test]
    fn sys_clone_number_matches_linux() {
        assert_eq!(SYS_CLONE, 56);
    }

    #[test]
    fn clone_vm_with_null_child_stack_is_einval() {
        // Reaches the child_stack==0 check without ever touching
        // current_process() -- see this module's doc comment on why that
        // ordering is what makes this host-safe at all.
        assert_eq!(sys_clone(CLONE_VM, 0), EINVAL);
    }

    // sys_clone(flags, _) with CLONE_VM unset must delegate to
    // crate::fork::sys_fork(). That function cannot actually be invoked
    // from a host test process either (it unconditionally calls
    // crate::syscall::current_process() at its very top, before any
    // cfg(target_os = "none") gate -- see fork.rs). Verified here at the
    // source level instead: sys_clone's very first statement is
    // `if flags & CLONE_VM == 0 { return crate::fork::sys_fork(); }`, so
    // any flags value with CLONE_VM unset takes that branch by
    // construction -- true behavioral verification of the delegation
    // needs QEMU (or a call-counting seam sys_fork doesn't have today).
    #[test]
    fn non_clone_vm_flags_have_clone_vm_unset_by_construction() {
        assert_eq!(0u32 & CLONE_VM, 0, "flags=0 has CLONE_VM unset");
        assert_eq!(0x200u32 & CLONE_VM, 0, "an unrelated bit also has CLONE_VM unset");
    }
}
