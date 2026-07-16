use crate::thread::{Context, Thread};

pub const CONTEXT_OFF: usize = core::mem::offset_of!(Thread, context);

const _: () = assert!(core::mem::size_of::<Context>() == 64);
const _: () = assert!(core::mem::offset_of!(Thread, context) == CONTEXT_OFF);
const _: () = assert!(
    core::mem::offset_of!(Context, r15) == 0,
    "Context::r15 must be at offset 0"
);
const _: () = assert!(
    core::mem::offset_of!(Context, rip) == 48,
    "Context::rip must be at offset 48"
);
const _: () = assert!(
    core::mem::offset_of!(Context, rsp) == 56,
    "Context::rsp must be at offset 56"
);

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
core::arch::global_asm!(
    r#"
    .section .text.switch_context,"ax",@progbits
    .global switch_context
    .type switch_context, @function
    .align 16
switch_context:
    .byte 0xf3, 0x0f, 0x1e, 0xfa
    mov qword ptr [rdi + {context_off} + 0],  r15
    mov qword ptr [rdi + {context_off} + 8],  r14
    mov qword ptr [rdi + {context_off} + 16], r13
    mov qword ptr [rdi + {context_off} + 24], r12
    mov qword ptr [rdi + {context_off} + 32], rbx
    mov qword ptr [rdi + {context_off} + 40], rbp
    lea rax, [rip + .Lswitch_return]
    mov qword ptr [rdi + {context_off} + 48], rax
    mov qword ptr [rdi + {context_off} + 56], rsp

    mov r15, qword ptr [rsi + {context_off} + 0]
    mov r14, qword ptr [rsi + {context_off} + 8]
    mov r13, qword ptr [rsi + {context_off} + 16]
    mov r12, qword ptr [rsi + {context_off} + 24]
    mov rbx, qword ptr [rsi + {context_off} + 32]
    mov rbp, qword ptr [rsi + {context_off} + 40]
    mov rsp, qword ptr [rsi + {context_off} + 56]
    jmp qword ptr [rsi + {context_off} + 48]
.Lswitch_return:
    ret
    .size switch_context, . - switch_context

    .global switch_first
    .type switch_first, @function
    .align 16
switch_first:
    .byte 0xf3, 0x0f, 0x1e, 0xfa
    mov rsi, rdi
    mov r15, qword ptr [rsi + {context_off} + 0]
    mov r14, qword ptr [rsi + {context_off} + 8]
    mov r13, qword ptr [rsi + {context_off} + 16]
    mov r12, qword ptr [rsi + {context_off} + 24]
    mov rbx, qword ptr [rsi + {context_off} + 32]
    mov rbp, qword ptr [rsi + {context_off} + 40]
    mov rsp, qword ptr [rsi + {context_off} + 56]
    jmp qword ptr [rsi + {context_off} + 48]
    .size switch_first, . - switch_first
    "#,
    context_off = const CONTEXT_OFF,
);

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe extern "C" {
    pub fn switch_context(prev: *mut Thread, next: *mut Thread);
    pub fn switch_first(next: *mut Thread) -> !;
}

/// Host-build stub of the bare-metal context switch; does nothing.
///
/// # Safety
/// Trivially safe (no-op); `unsafe` only to keep the signature identical
/// to the real bare-metal implementation above.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub unsafe extern "C" fn switch_context(_prev: *mut Thread, _next: *mut Thread) {}

/// Host-build stub of the bare-metal first dispatch; always panics.
///
/// # Safety
/// Trivially safe (always panics before doing anything); `unsafe` only to
/// keep the signature identical to the real bare-metal implementation.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub unsafe extern "C" fn switch_first(_next: *mut Thread) -> ! {
    panic!("switch_first is only available on bare-metal x86_64")
}

/// Update TSS.rsp0 for the next thread's kernel stack.
///
/// # Safety
/// `cpu_id` must identify an initialized CPU and `rsp0` must be the top of a
/// valid kernel stack for the thread being switched to.
#[inline(always)]
pub unsafe fn update_tss_rsp0(cpu_id: u32, rsp0: u64) {
    // SAFETY: caller guarantees the CPU slot and stack pointer are valid.
    unsafe { kernel_cpu::set_tss_rsp0(cpu_id, rsp0) };
}
