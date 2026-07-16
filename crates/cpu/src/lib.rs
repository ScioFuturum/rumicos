#![no_std]
#![cfg(target_arch = "x86_64")]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod gdt;
pub mod idt;
pub mod percpu;
pub mod syscall;
pub mod tss;

pub use gdt::{Gdt, KERNEL_CS, KERNEL_DS, TSS_SEL, USER_CS, USER_DS, init_gdt};
pub use idt::{
    InterruptFrame, InterruptReturnHook, init_idt, register_handler, set_interrupt_return_hook,
};
pub use percpu::{
    CPU_ID, CPU_RSP_KERN, CPU_RSP_USER, PerCpuData, current_cpu_id, install_per_cpu_gs,
};
pub use syscall::{
    SyscallFrame, SyscallHandler, SyscallReturnHook, current_syscall_frame, init_syscall_msrs,
    set_syscall_handler, set_syscall_return_hook, syscall_dispatch,
};
pub use tss::{Tss, init_tss, set_tss_rsp0};

pub fn init_cpu(cpu_id: u32) {
    let gdt = gdt::init_gdt_mut();
    init_tss(cpu_id, gdt);
    init_idt();
    init_syscall_msrs();
    install_per_cpu_gs(cpu_id);
}
