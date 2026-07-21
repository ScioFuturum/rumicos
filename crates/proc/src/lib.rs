#![no_std]
#![cfg(target_arch = "x86_64")]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod address_space;
pub mod clone;
pub mod cow;
pub mod elf;
pub mod exec;
pub mod fd;
pub mod fork;
pub mod pagefault;
pub mod process;
pub mod ptable;
pub mod shootdown;
pub mod signal;
pub mod syscall;
pub mod ustack;
pub mod vma;
pub mod wait;

pub use address_space::AddressSpace;
pub use clone::{CLONE_VM, SYS_CLONE};
pub use elf::{ElfError, ElfInfo, LoadSegment, parse_elf};
pub use fd::{FdEntry, FdTable, MAX_FDS, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
pub use fork::SYS_FORK;
pub use process::{Pid, Process, ProcessState, alloc_pid};
pub use syscall::{
    SYS_EXECVE, current_process, is_user_ptr, register_exec_loader, register_extra,
    register_page_cache_hooks, register_vnode_refcount_hooks, register_vnode_release_hook,
    set_serial_vnode,
};
pub use vma::{SYS_MMAP, SYS_MUNMAP};

pub fn init() {
    #[cfg(target_os = "none")]
    kernel_cpu::register_handler(pagefault::PF_VECTOR, pagefault::pf_handler);
    // TLB-shootdown IPI handler (Part 3) -- only reachable once CLONE_VM
    // (crate::clone) can put more than one CPU's thread under the same
    // AddressSpace; registered unconditionally at boot anyway, exactly
    // like the page-fault handler above, since there's no harm in it
    // sitting idle for a single-CPU/single-threaded-per-AS boot.
    #[cfg(target_os = "none")]
    kernel_cpu::register_handler(shootdown::SHOOTDOWN_VECTOR, shootdown::shootdown_handler);
    // CR3 reload on cross-process context switches. Without this, CR3 was
    // only ever loaded on a thread's first dispatch and in execve's Phase 2,
    // so switching between two already-running processes resumed the next
    // one under the previous one's page tables. Registered before any
    // process exists (kernel_main calls kernel_proc::init() before the
    // first Process::create), so no switch can ever miss it.
    kernel_sched::register_context_switch_hook(process::sched_context_switch_hook);
    // Deliver pending signals on the return-to-user-mode path of every
    // syscall (see signal::deliver_on_syscall_return). Registered before
    // any process runs, so no syscall return can miss it.
    #[cfg(target_os = "none")]
    kernel_cpu::set_syscall_return_hook(signal::deliver_on_syscall_return_hook);
    // Fix B: also deliver on the interrupt/exception return path (timer,
    // device IRQ, resolved #PF) whenever it returns to ring 3, so a signal
    // reaches a process spinning in user space within one timer tick (1ms)
    // instead of only at its next syscall. #PF gets this for free — it runs
    // through the same IDT common stub as every other vector.
    #[cfg(target_os = "none")]
    kernel_cpu::set_interrupt_return_hook(signal::check_and_deliver_iret_hook);
    syscall::register();
    #[cfg(target_os = "none")]
    {
        use kernel_arch_x86_64::msr::{IA32_STAR, rdmsr};
        let star = unsafe {
            // SAFETY: called in ring 0 after MSR initialisation.
            rdmsr(IA32_STAR)
        };
        assert_eq!(
            (star >> 48) & 0xffff,
            kernel_cpu::USER_CS as u64 - 16,
            "STAR[63:48] misconfigured"
        );
    }
}
