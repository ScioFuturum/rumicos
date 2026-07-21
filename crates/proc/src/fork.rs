#[cfg(target_os = "none")]
use crate::fd::FdTable;
#[cfg(target_os = "none")]
use crate::process::{Process, ProcessState, alloc_pid};

pub const SYS_FORK: u64 = 57;

const EFAULT: i64 = -14;
// Returned only from the target-only fork body (children-list full).
#[cfg(target_os = "none")]
const EAGAIN: i64 = -11;

pub fn sys_fork() -> i64 {
    let parent_ptr = crate::syscall::current_process();
    if parent_ptr.is_null() {
        return EFAULT;
    }

    #[cfg(target_os = "none")]
    unsafe {
        // SAFETY: current_process returned the live parent for this syscall.
        let parent = &mut *parent_ptr;

        // Fail cleanly BEFORE building anything if the parent can't track a
        // 33rd child (see MAX_CHILDREN) — rolling a fully-built child back
        // would be far more work than this early check.
        // SAFETY: parent_ptr is live; only this thread mutates its children.
        if crate::process::children_full(parent_ptr) {
            return EAGAIN;
        }

        let syscall_frame = kernel_cpu::current_syscall_frame();
        // fork() always gets its OWN private CoW copy (never shared) --
        // unlike CLONE_VM (crate::clone::sys_clone), which points the
        // child at the exact same AddressSpace via inc_ref instead.
        let child_as_value = (*parent.address_space).fork_cow();
        let child_as = crate::address_space::AddressSpace::alloc_shared(child_as_value);
        let child_fds: FdTable = parent.fd_table.lock().clone_for_fork();
        // POSIX: fork inherits the parent's signal dispositions.
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
            address_space: child_as,
            thread: child_thread,
            name: parent.name,
            user_rip: syscall_frame.user_rip,
            user_rsp: syscall_frame.user_rsp,
            user_rflags: syscall_frame.user_rflags,
            user_rax: 0,
            // Set element-wise below (see the children note) once the child
            // frame is live.
            user_callee: [0; 6],
            fd_table: kernel_sync::SpinLock::new(child_fds),
            parent_pid: parent.pid,
            sig_table: kernel_sync::SpinLock::new(child_sig_table),
            // The child starts with no pending signals and no handler
            // in flight, regardless of the parent's transient state.
            pending: crate::signal::PendingSet::new(),
            sig_frame: kernel_sync::SpinLock::new(None),
            // The child has no children of its own yet.
            children: kernel_sync::SpinLock::new([0; crate::process::MAX_CHILDREN]),
            child_count: core::sync::atomic::AtomicU32::new(0),
            wait_queue: kernel_sched::WaitQueue::new(),
        });
        // Zero the child's own children array element-by-element — the
        // aggregate Process placement-write above does not reliably init it
        // on rustc 1.97.0 (see Process::create for the full rationale).
        for slot in (*child_ptr).children.lock().iter_mut() {
            *slot = 0;
        }
        (*child_ptr).child_count.store(0, core::sync::atomic::Ordering::Relaxed);

        // The child must resume the fork() call with the parent's
        // callee-saved registers intact (see Process::user_callee). The
        // syscall trampoline captured them into the per-CPU frame at entry.
        (*child_ptr).user_callee[0] = syscall_frame.rbx;
        (*child_ptr).user_callee[1] = syscall_frame.rbp;
        (*child_ptr).user_callee[2] = syscall_frame.r12;
        (*child_ptr).user_callee[3] = syscall_frame.r13;
        (*child_ptr).user_callee[4] = syscall_frame.r14;
        (*child_ptr).user_callee[5] = syscall_frame.r15;

        crate::syscall::register_process(child_ptr);
        crate::ptable::ptable_insert(child_pid, child_ptr);
        // Record the child under the parent so sys_wait4 can find it. The
        // early children_full check above guarantees a free slot, so this
        // cannot fail here.
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
    fn fork_syscall_number_matches_linux() {
        assert_eq!(SYS_FORK, 57);
    }
}
