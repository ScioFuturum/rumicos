use crate::gdt::{KERNEL_CS, USER_CS};
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::percpu::{CPU_RSP_KERN, CPU_RSP_USER};
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use kernel_arch_x86_64::msr::{IA32_EFER, IA32_FMASK, IA32_LSTAR, IA32_STAR, rdmsr, wrmsr};

const IA32_EFER_SCE: u64 = 1;
const RFLAGS_IF: u64 = 1 << 9;
const ENOSYS: i64 = -38;

pub type SyscallHandler = extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> i64;

/// A hook run at the tail of `syscall_dispatch`, on the syscall's computed
/// return value, right before the trampoline restores registers and
/// `sysretq`s back to user mode. kernel-proc registers one to deliver
/// pending signals here (see `kernel_proc::signal::deliver_on_syscall_return`).
/// It returns the value to actually place in RAX; it MAY diverge instead
/// (never returning) when it redirects execution into a signal handler or
/// terminates the process.
pub type SyscallReturnHook = fn(i64) -> i64;

static SYSCALL_HANDLER: AtomicUsize = AtomicUsize::new(0);
static SYSCALL_RETURN_HOOK: AtomicUsize = AtomicUsize::new(0);

const MAX_SYSCALL_CPUS: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct SyscallFrame {
    pub user_rip: u64,
    pub user_rsp: u64,
    pub user_rflags: u64,
    // SysV callee-saved registers, captured at syscall entry (see
    // `update_syscall_callee_saved` and the syscall trampoline). These hold
    // the USER's values: the trampoline saves them on the kernel stack and
    // restores them only at its exit, so they cannot be read live from the
    // Rust syscall-return hook (the handler's own Rust call chain has since
    // clobbered them). Signal delivery on the syscall path reads them from
    // here to build a complete `SavedUserContext`.
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

struct SyscallFrameSlot {
    rip: AtomicU64,
    rsp: AtomicU64,
    rflags: AtomicU64,
    rbx: AtomicU64,
    rbp: AtomicU64,
    r12: AtomicU64,
    r13: AtomicU64,
    r14: AtomicU64,
    r15: AtomicU64,
}

impl SyscallFrameSlot {
    pub const fn new() -> Self {
        Self {
            rip: AtomicU64::new(0),
            rsp: AtomicU64::new(0),
            rflags: AtomicU64::new(0),
            rbx: AtomicU64::new(0),
            rbp: AtomicU64::new(0),
            r12: AtomicU64::new(0),
            r13: AtomicU64::new(0),
            r14: AtomicU64::new(0),
            r15: AtomicU64::new(0),
        }
    }
}

static CURRENT_SYSCALL_FRAME: [SyscallFrameSlot; MAX_SYSCALL_CPUS] =
    [const { SyscallFrameSlot::new() }; MAX_SYSCALL_CPUS];

pub fn current_syscall_frame() -> SyscallFrame {
    let cpu = syscall_frame_cpu_index();
    let slot = &CURRENT_SYSCALL_FRAME[cpu];
    SyscallFrame {
        user_rip: slot.rip.load(Ordering::Acquire),
        user_rsp: slot.rsp.load(Ordering::Acquire),
        user_rflags: slot.rflags.load(Ordering::Acquire),
        rbx: slot.rbx.load(Ordering::Acquire),
        rbp: slot.rbp.load(Ordering::Acquire),
        r12: slot.r12.load(Ordering::Acquire),
        r13: slot.r13.load(Ordering::Acquire),
        r14: slot.r14.load(Ordering::Acquire),
        r15: slot.r15.load(Ordering::Acquire),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn update_current_syscall_frame(user_rip: u64, user_rsp: u64, user_rflags: u64) {
    let cpu = syscall_frame_cpu_index();
    let slot = &CURRENT_SYSCALL_FRAME[cpu];
    slot.rip.store(user_rip, Ordering::Release);
    slot.rsp.store(user_rsp, Ordering::Release);
    slot.rflags.store(user_rflags, Ordering::Release);
}

/// Capture the user's SysV callee-saved registers into the per-CPU syscall
/// frame. Called by the syscall trampoline immediately after
/// `update_current_syscall_frame` returns — at that instant rbx/rbp/r12-r15
/// still hold the USER's values, because `update_current_syscall_frame` is
/// itself a well-formed function that preserved them across its own call.
/// Split from `update_current_syscall_frame` (rather than adding six more
/// args) so the six values arrive naturally in the SysV arg registers with
/// no stack-offset coupling to the trampoline's push order.
#[unsafe(no_mangle)]
pub extern "C" fn update_syscall_callee_saved(
    rbx: u64,
    rbp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
) {
    let cpu = syscall_frame_cpu_index();
    let slot = &CURRENT_SYSCALL_FRAME[cpu];
    slot.rbx.store(rbx, Ordering::Release);
    slot.rbp.store(rbp, Ordering::Release);
    slot.r12.store(r12, Ordering::Release);
    slot.r13.store(r13, Ordering::Release);
    slot.r14.store(r14, Ordering::Release);
    slot.r15.store(r15, Ordering::Release);
}

#[cfg(target_os = "none")]
fn syscall_frame_cpu_index() -> usize {
    (crate::current_cpu_id() as usize).min(MAX_SYSCALL_CPUS - 1)
}

#[cfg(not(target_os = "none"))]
fn syscall_frame_cpu_index() -> usize {
    0
}

#[cfg(all(
    target_arch = "x86_64",
    target_os = "none",
    feature = "amd_lfence_sysret"
))]
global_asm!(
    r#"
    .section .text.syscall_entry,"ax",@progbits
    .global syscall_entry
    .type syscall_entry, @function
    .align 16
syscall_entry:
    .byte 0xf3, 0x0f, 0x1e, 0xfa
    swapgs
    mov qword ptr gs:[{cpu_rsp_user}], rsp
    mov rsp, qword ptr gs:[{cpu_rsp_kern}]
    push rcx
    push r11
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    push rax
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9
    mov rdi, rcx
    mov rsi, qword ptr gs:[{cpu_rsp_user}]
    mov rdx, r11
    call update_current_syscall_frame
    mov rdi, rbx
    mov rsi, rbp
    mov rdx, r12
    mov rcx, r13
    mov r8, r14
    mov r9, r15
    call update_syscall_callee_saved
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop rax
    push r9
    push r9
    mov r9, r8
    mov r8, r10
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    call syscall_dispatch
    add rsp, 16
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    pop r11
    pop rcx
    mov rsp, qword ptr gs:[{cpu_rsp_user}]
    swapgs
    lfence
    sysretq
    .size syscall_entry, . - syscall_entry
    "#,
    cpu_rsp_user = const CPU_RSP_USER,
    cpu_rsp_kern = const CPU_RSP_KERN,
);

#[cfg(all(
    target_arch = "x86_64",
    target_os = "none",
    not(feature = "amd_lfence_sysret")
))]
global_asm!(
    r#"
    .section .text.syscall_entry,"ax",@progbits
    .global syscall_entry
    .type syscall_entry, @function
    .align 16
syscall_entry:
    .byte 0xf3, 0x0f, 0x1e, 0xfa
    swapgs
    mov qword ptr gs:[{cpu_rsp_user}], rsp
    mov rsp, qword ptr gs:[{cpu_rsp_kern}]
    push rcx
    push r11
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    push rax
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9
    mov rdi, rcx
    mov rsi, qword ptr gs:[{cpu_rsp_user}]
    mov rdx, r11
    call update_current_syscall_frame
    mov rdi, rbx
    mov rsi, rbp
    mov rdx, r12
    mov rcx, r13
    mov r8, r14
    mov r9, r15
    call update_syscall_callee_saved
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop rax
    push r9
    push r9
    mov r9, r8
    mov r8, r10
    mov rcx, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    call syscall_dispatch
    add rsp, 16
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    pop r11
    pop rcx
    mov rsp, qword ptr gs:[{cpu_rsp_user}]
    swapgs
    sysretq
    .size syscall_entry, . - syscall_entry
    "#,
    cpu_rsp_user = const CPU_RSP_USER,
    cpu_rsp_kern = const CPU_RSP_KERN,
);

pub fn init_syscall_msrs() {
    // SAFETY: SYSCALL MSRs are privileged CPU-local boot state; selector values
    // are installed by the GDT initializer before this function is called.
    unsafe {
        let efer = rdmsr(IA32_EFER) | IA32_EFER_SCE;
        wrmsr(IA32_EFER, efer);
        wrmsr(IA32_STAR, star_value());
        wrmsr(IA32_LSTAR, syscall_entry_addr());
        wrmsr(IA32_FMASK, RFLAGS_IF);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_dispatch(
    nr: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> i64 {
    let handler = SYSCALL_HANDLER.load(Ordering::Acquire);
    if handler == 0 {
        return default_syscall_handler(nr, a1, a2, a3, a4, a5, a6);
    }
    let handler: SyscallHandler = unsafe {
        // SAFETY: set_syscall_handler stores only valid SyscallHandler function pointers.
        core::mem::transmute(handler)
    };
    let ret = handler(nr, a1, a2, a3, a4, a5, a6);

    // Signal-delivery point: on return to user mode, let the registered
    // hook (if any) deliver a pending signal. It either returns the value
    // to leave in RAX, or diverges (into a handler / process exit).
    let hook = SYSCALL_RETURN_HOOK.load(Ordering::Acquire);
    if hook != 0 {
        // SAFETY: set_syscall_return_hook stores only valid
        // SyscallReturnHook function pointers.
        let hook: SyscallReturnHook = unsafe { core::mem::transmute(hook) };
        return hook(ret);
    }
    ret
}

pub fn set_syscall_handler(handler: SyscallHandler) {
    SYSCALL_HANDLER.store(handler as usize, Ordering::Release);
}

/// Register the syscall-return hook (see [`SyscallReturnHook`]). Called
/// once by kernel-proc's init.
pub fn set_syscall_return_hook(hook: SyscallReturnHook) {
    SYSCALL_RETURN_HOOK.store(hook as usize, Ordering::Release);
}

extern "C" fn default_syscall_handler(
    _nr: u64,
    _a1: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
) -> i64 {
    ENOSYS
}

pub const fn star_value() -> u64 {
    (((USER_CS as u64) - 16) << 48) | ((KERNEL_CS as u64) << 32)
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn syscall_entry_addr() -> u64 {
    unsafe extern "C" {
        fn syscall_entry();
    }

    syscall_entry as *const () as usize as u64
}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn syscall_entry_addr() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gdt::{USER_CS, USER_DS};

    #[test]
    fn star_selector_arithmetic_matches_sysret_abi() {
        let user_base = USER_CS - 16;
        assert_eq!(user_base + 8, USER_DS);
        assert_eq!(user_base + 16, USER_CS);
        assert_eq!(
            star_value(),
            ((user_base as u64) << 48) | ((KERNEL_CS as u64) << 32)
        );
    }

    #[test]
    fn syscall_frame_update_roundtrip_on_host() {
        update_current_syscall_frame(1, 2, 3);
        update_syscall_callee_saved(11, 12, 13, 14, 15, 16);
        assert_eq!(
            current_syscall_frame(),
            SyscallFrame {
                user_rip: 1,
                user_rsp: 2,
                user_rflags: 3,
                rbx: 11,
                rbp: 12,
                r12: 13,
                r13: 14,
                r14: 15,
                r15: 16,
            }
        );
    }

    // SYSCALL_HANDLER is a process-wide static (see set_syscall_handler),
    // so -- like this crate's other single-slot statics -- registration
    // and use have to happen inside one sequential test rather than be
    // split across independently-schedulable #[test] functions. This is
    // the Part 1 round-trip: proves a6 (the new 6th logical argument,
    // carried through the trampoline's r9 and, since it no longer fits in
    // the 6 SysV integer-argument registers alongside nr/a1..a5, the
    // stack -- see the global_asm! blocks above) survives all the way to
    // a registered handler unmodified, alongside sentinels for every
    // other argument so a shuffle bug anywhere in the chain would show up
    // as a mismatch here rather than just "a6 happens to work".
    #[test]
    fn syscall_dispatch_forwards_all_six_args_to_registered_handler() {
        static SEEN: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

        extern "C" fn mock_handler(
            nr: u64,
            a1: u64,
            a2: u64,
            a3: u64,
            a4: u64,
            a5: u64,
            a6: u64,
        ) -> i64 {
            SEEN[0].store(nr, Ordering::SeqCst);
            SEEN[1].store(a1, Ordering::SeqCst);
            SEEN[2].store(a2, Ordering::SeqCst);
            SEEN[3].store(a3, Ordering::SeqCst);
            SEEN[4].store(a4, Ordering::SeqCst);
            SEEN[5].store(a5, Ordering::SeqCst);
            SEEN[6].store(a6, Ordering::SeqCst);
            0
        }

        set_syscall_handler(mock_handler);
        let ret = syscall_dispatch(0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16);
        assert_eq!(ret, 0);
        let expected: [u64; 7] = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16];
        for (i, exp) in expected.iter().enumerate() {
            assert_eq!(SEEN[i].load(Ordering::SeqCst), *exp, "arg slot {i} mismatch");
        }
    }
}
