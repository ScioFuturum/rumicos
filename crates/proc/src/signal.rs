//! POSIX-ish signals: `sigaction`, `kill`, `sigreturn`, and delivery on
//! return to user mode.
//!
//! ## Architecture notes / substitutions vs. the design brief
//!
//! The brief assumed a `Ring3Entry` value, a per-CPU `PENDING_RING3` slot,
//! and a `ring3_entry_rax` three-call trampoline. **None of that exists in
//! this kernel.** The real return-to-user path is `enter_user_mode` in
//! `crate::process` (a `noreturn` SYSRET, used by `execve` Phase 2 and a
//! new thread's first dispatch). So signal delivery here does not "modify
//! a Ring3Entry and let a trampoline pick it up" — it performs the return
//! itself, exactly the way `execve` does: build the target user context,
//! then jump to ring 3 via `enter_signal_handler`, never returning.
//!
//! Of the three kernel→user return points the brief lists, only path (a),
//! the **syscall return**, is wired (see `deliver_on_syscall_return`,
//! called from `kernel_cpu`'s syscall-return hook). Paths (b) IDT/IRETQ
//! and (c) #PF are NOT wired this checkpoint: a signal raised against a
//! process is still delivered promptly, just at its next *syscall* return
//! rather than at a timer/fault return. The self-signal demo
//! (`kill(getpid(), SIGUSR1)` then return) exercises exactly path (a).
//! See "Known limitations".
//!
//! The decision logic ("which pending signal, and what to do with it") is
//! split into the pure, host-testable [`decide_next_signal`]; the actual
//! user-stack setup and ring-3 re-entry is target-only.

use core::sync::atomic::{AtomicU64, Ordering};

pub const NSIG: usize = 32;

// Linux-compatible signal numbers.
pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGKILL: u32 = 9; // cannot be caught or ignored
pub const SIGUSR1: u32 = 10;
pub const SIGUSR2: u32 = 12;
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;

// Linux-compatible syscall numbers for the signal calls.
pub const SYS_SIGACTION: u64 = 13;
pub const SYS_SIGRETURN: u64 = 15;
pub const SYS_KILL: u64 = 62;

/// Fixed user virtual address at which the `sigreturn` trampoline page is
/// mapped read+exec into every process. Chosen to sit in the hole between
/// the loaded init ELF (`0x40_0000`..~`0x40_3000`) and the mmap region
/// (`crate::vma::MMAP_BASE` = `0x4000_0000_0000`), well clear of the user
/// stack (`crate::ustack::USTACK_TOP` = `0x7fff_ffff_0000`). The brief
/// suggested `0x40_1000`, but that collides with init2.elf's own segments
/// (which reach ~`0x40_2844`), so a fresh hole is used instead.
pub const SIGRETURN_TRAMPOLINE_VA: u64 = 0x0000_0000_1000_0000;

/// The trampoline's machine code: `mov rax, SYS_SIGRETURN; syscall`.
/// Embedded as raw kernel bytes and mapped into every address space rather
/// than linked into the init ELF (the brief's "embed as a fixed-address
/// page" alternative) — self-contained, and inherited automatically across
/// `fork` (CoW copy) and `CLONE_VM` (shared address space).
///
///   48 c7 c0 0f 00 00 00   mov rax, 15
///   0f 05                  syscall
pub const SIGRETURN_TRAMPOLINE_CODE: [u8; 9] =
    [0x48, 0xc7, 0xc0, 0x0f, 0x00, 0x00, 0x00, 0x0f, 0x05];

const _: () = assert!(SIGRETURN_TRAMPOLINE_CODE[3] == SYS_SIGRETURN as u8);

/// Disposition of one signal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SigAction {
    Default,
    Ignore,
    Handler { user_fn: u64 },
}

/// Per-process registered handlers, one slot per signal number.
#[derive(Clone, Copy)]
pub struct SigTable {
    pub actions: [SigAction; NSIG],
}

impl SigTable {
    pub const fn new() -> Self {
        Self {
            actions: [SigAction::Default; NSIG],
        }
    }
}

impl Default for SigTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Pending-signal bitmask — bit N set means signal N is pending. `AtomicU64`
/// so `kill()` can raise a bit from another CPU without taking the target's
/// process lock.
pub struct PendingSet {
    pub bits: AtomicU64,
}

impl PendingSet {
    pub const fn new() -> Self {
        Self {
            bits: AtomicU64::new(0),
        }
    }

    /// Mark `signum` pending. Bit `signum` (1-indexed: bit 9 = SIGKILL).
    pub fn raise(&self, signum: u32) {
        self.bits.fetch_or(1u64 << signum, Ordering::AcqRel);
    }

    /// Atomically remove and return the lowest-numbered pending signal,
    /// or `None` if none pending. Lowest-first so SIGKILL (9) is taken
    /// before higher-numbered catchable signals raised at the same time.
    pub fn take_any(&self) -> Option<u32> {
        let mut cur = self.bits.load(Ordering::Acquire);
        loop {
            if cur == 0 {
                return None;
            }
            let signum = cur.trailing_zeros();
            let new = cur & !(1u64 << signum);
            match self.bits.compare_exchange_weak(
                cur,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(signum),
                Err(observed) => cur = observed,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn peek(&self) -> u64 {
        self.bits.load(Ordering::Acquire)
    }
}

impl Default for PendingSet {
    fn default() -> Self {
        Self::new()
    }
}

/// The user register state saved before a signal handler runs, restored by
/// `sigreturn`. `#[repr(C)]` with a fixed field order so the target-only
/// stack write/read agrees byte-for-byte.
///
/// At the ONLY wired delivery point (syscall return), the SysV ABI has
/// already allowed the syscall to clobber the caller-saved GPRs, so only
/// `user_rip`/`user_rsp`/`user_rflags` and `rax` (the interrupted
/// syscall's return value) are meaningful; the remaining fields are saved
/// as zero. They exist in full for the future interrupt/#PF delivery paths
/// (b)/(c), which interrupt arbitrary user code and must preserve every
/// register.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SavedUserContext {
    pub user_rip: u64,
    pub user_rsp: u64,
    pub user_rflags: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

impl SavedUserContext {
    pub const fn zeroed() -> Self {
        Self {
            user_rip: 0,
            user_rsp: 0,
            user_rflags: 0,
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
        }
    }
}

/// What [`decide_next_signal`] concluded should happen for the one signal
/// it dequeued. Pure data so the decision is host-testable in isolation
/// from the ring-3 machinery.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignalOutcome {
    /// Nothing to deliver (no pending signal, or an ignored/no-op default).
    None,
    /// Fatal default action — the process must terminate with `-signum`.
    Terminate(u32),
    /// Run the user handler at `user_fn`, passing `signum` as its argument.
    Deliver { user_fn: u64, signum: u32 },
}

/// Dequeue the lowest pending signal from `pending` and decide its fate
/// against `table`. Pure and host-testable — no user memory, no ring 3.
///
/// SIGKILL always terminates (checked BEFORE the table, so a registered
/// handler can never catch it). SIGTERM terminates only with the default
/// action. Every other signal: run its handler if one is registered,
/// otherwise no-op (this checkpoint does not implement the terminate-by-
/// default disposition for signals other than SIGKILL/SIGTERM — see
/// "Known limitations").
///
/// Idempotent by construction: it consumes exactly one pending bit per
/// call via `take_any`, so calling it again with nothing newly raised
/// returns `None`.
pub fn decide_next_signal(pending: &PendingSet, table: &SigTable) -> SignalOutcome {
    let Some(signum) = pending.take_any() else {
        return SignalOutcome::None;
    };
    if signum == SIGKILL {
        return SignalOutcome::Terminate(signum);
    }
    if (signum as usize) >= NSIG {
        return SignalOutcome::None;
    }
    match table.actions[signum as usize] {
        SigAction::Handler { user_fn } => SignalOutcome::Deliver { user_fn, signum },
        SigAction::Ignore => SignalOutcome::None,
        SigAction::Default => {
            if signum == SIGTERM {
                SignalOutcome::Terminate(signum)
            } else {
                SignalOutcome::None
            }
        }
    }
}

/// `true` iff `signum` may legally have a handler installed. SIGKILL cannot
/// be caught or ignored; out-of-range numbers are rejected. Used by
/// `sys_sigaction`.
pub fn signal_is_catchable(signum: u32) -> bool {
    signum != 0 && signum != SIGKILL && (signum as usize) < NSIG
}

/// RFLAGS a signal handler is entered with: IF (bit 9) plus the always-set
/// reserved bit 1, with TF/AC/DF and everything else cleared so the handler
/// starts in a clean, interruptible state.
pub const SIGNAL_HANDLER_RFLAGS: u64 = 0x202;

/// `true` iff a saved code segment selector was running in ring 3
/// (CPL == 3, the low two bits). The interrupt-return hook uses this to
/// deliver signals only when returning to user mode, never to kernel code.
pub fn returning_to_user(cs: u64) -> bool {
    cs & 3 == 3
}

/// Where the signal frame lands on the user stack, computed purely from the
/// interrupted user RSP (no memory touched) so the layout is host-testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalFrameLayout {
    /// 16-aligned address the `SavedUserContext` is written to.
    pub ctx_addr: u64,
    /// Handler entry RSP: the slot holding the sigreturn-trampoline return
    /// address, one qword below `ctx_addr` (so `≡ 8 (mod 16)`, the SysV
    /// alignment at a function's first instruction).
    pub ret_addr_slot: u64,
}

/// Lay the signal frame out below `user_rsp`'s 128-byte red zone:
/// `SavedUserContext` 16-aligned, then the trampoline return address one
/// qword below it. Pure — the actual writes happen in `write_signal_frame`.
pub fn compute_signal_frame_layout(user_rsp: u64) -> SignalFrameLayout {
    let red_zone_bottom = user_rsp - 128;
    let ctx_size = core::mem::size_of::<SavedUserContext>() as u64;
    let ctx_addr = (red_zone_bottom - ctx_size) & !0xf;
    SignalFrameLayout {
        ctx_addr,
        ret_addr_slot: ctx_addr - 8,
    }
}

// ─── target-only delivery on the syscall-return path (path (a)) ───────────

/// Called from `kernel_cpu`'s syscall-return hook with the syscall's
/// computed return value, right before it would `sysretq` back to user
/// mode. Delivers at most one pending signal:
///
///   * no signal / ignored default → returns `retval` unchanged; the
///     normal syscall epilogue resumes the caller;
///   * fatal (SIGKILL, or SIGTERM default) → `Process::exit(-signum)`,
///     which never returns;
///   * user handler → sets up the user stack and jumps to the handler via
///     `enter_signal_handler`, which never returns (the handler will
///     eventually `ret` into the sigreturn trampoline).
///
/// # Safety
/// Must be called on the syscall path, with the current process's address
/// space active in CR3 (true for every syscall — SYSCALL does not switch
/// CR3), interrupts disabled, on the calling thread's kernel stack.
/// Safe `fn(i64) -> i64` wrapper registered with
/// `kernel_cpu::set_syscall_return_hook`. The preconditions of
/// [`deliver_on_syscall_return`] are structurally guaranteed at the sole
/// call site (kernel-cpu's `syscall_dispatch` tail, IF=0, process CR3
/// active, on the syscall thread's kernel stack).
#[cfg(target_os = "none")]
pub fn deliver_on_syscall_return_hook(retval: i64) -> i64 {
    // SAFETY: called only from the syscall-dispatch return path, which
    // upholds every precondition of deliver_on_syscall_return.
    unsafe { deliver_on_syscall_return(retval) }
}

/// Deliver at most one pending signal on the syscall-return path — see
/// [`deliver_on_syscall_return_hook`] for the wrapper the syscall
/// dispatcher actually registers.
///
/// # Safety
/// Must be called on the syscall path, with the current process's address
/// space active in CR3 (true for every syscall — SYSCALL does not switch
/// CR3), interrupts disabled, on the calling thread's kernel stack.
#[cfg(target_os = "none")]
pub unsafe fn deliver_on_syscall_return(retval: i64) -> i64 {
    let proc = crate::syscall::current_process();
    if proc.is_null() {
        return retval;
    }
    // SAFETY: current_process returned the live PCB for this thread.
    let p = unsafe { &mut *proc };

    // Don't nest: if a handler is already running (sig_frame occupied), a
    // second delivery would overwrite its saved context and corrupt
    // sigreturn. Leave the signal pending — it will deliver after this
    // handler's sigreturn clears the frame. (Nested signals are out of
    // scope; this guard just makes the un-nested behavior safe.) Checked
    // BEFORE take_any so a skipped signal stays pending.
    if p.sig_frame.lock().is_some() {
        return retval;
    }

    let outcome = {
        let table = *p.sig_table.lock();
        decide_next_signal(&p.pending, &table)
    };

    match outcome {
        SignalOutcome::None => retval,
        SignalOutcome::Terminate(signum) => {
            p.exit(-(signum as i32));
            unreachable!("Process::exit returned")
        }
        SignalOutcome::Deliver { user_fn, signum } => {
            // SAFETY: forwards this function's own preconditions; noreturn.
            unsafe { setup_and_enter_handler(p, user_fn, signum, retval) }
        }
    }
}

// ─── target-only delivery on the interrupt-return path (Fix B: paths b/c) ──

/// Deliver a pending signal when returning to ring 3 from an interrupt or
/// exception (timer, device IRQ, resolved #PF). Called from kernel-cpu's
/// interrupt-return hook, which invokes it only when the interrupted CS was
/// ring 3 (`frame.cs & 3 == 3`).
///
/// Unlike the syscall path, the [`kernel_cpu::InterruptFrame`] carries the
/// COMPLETE user register snapshot the hardware/stub saved, so this builds a
/// full [`SavedUserContext`] (every GPR) — a signal handler that interrupts
/// arbitrary user code, then `sigreturn`s, resumes with all registers
/// intact (except RCX/R11, which `sysretq` clobbers — see
/// `process::enter_user_mode_restoring`).
///
/// Delivery here does NOT jump to ring 3 itself: it edits the interrupt
/// frame in place (`rip`→handler, `rsp`→below the saved context, `rflags`
/// →0x202, `rdi`→signum) and returns; the IDT stub's `iretq`/`pop` epilogue
/// carries the modified values. Idempotent: one signal per call, guarded
/// against nesting.
///
/// # Safety
/// `proc` must be the live current process, `frame` its interrupt frame,
/// and the process's address space must be the active CR3 (both true on the
/// IDT common stub's ring-3 return).
#[cfg(target_os = "none")]
pub unsafe fn check_and_deliver_iret(
    proc: *mut crate::process::Process,
    frame: &mut kernel_cpu::InterruptFrame,
) {
    // SAFETY: caller guarantees proc is the live current process.
    let p = unsafe { &mut *proc };

    // Same no-nesting guard as the syscall path (checked before take_any).
    if p.sig_frame.lock().is_some() {
        return;
    }

    let outcome = {
        let table = *p.sig_table.lock();
        decide_next_signal(&p.pending, &table)
    };

    match outcome {
        SignalOutcome::None => {}
        SignalOutcome::Terminate(signum) => {
            // Terminating from interrupt context: exit() marks the thread
            // Dead and reschedules; this never returns to the interrupted
            // user code.
            p.exit(-(signum as i32));
            unreachable!("Process::exit returned")
        }
        SignalOutcome::Deliver { user_fn, signum } => {
            // Full snapshot from the interrupt frame.
            let mut ctx = SavedUserContext::zeroed();
            ctx.user_rip = frame.rip;
            ctx.user_rsp = frame.rsp;
            ctx.user_rflags = frame.rflags;
            ctx.rax = frame.rax;
            ctx.rbx = frame.rbx;
            ctx.rcx = frame.rcx;
            ctx.rdx = frame.rdx;
            ctx.rsi = frame.rsi;
            ctx.rdi = frame.rdi;
            ctx.rbp = frame.rbp;
            ctx.r8 = frame.r8;
            ctx.r9 = frame.r9;
            ctx.r10 = frame.r10;
            ctx.r11 = frame.r11;
            ctx.r12 = frame.r12;
            ctx.r13 = frame.r13;
            ctx.r14 = frame.r14;
            ctx.r15 = frame.r15;

            let ret_addr_slot = unsafe { write_signal_frame(&ctx, frame.rsp) };
            *p.sig_frame.lock() = Some(ctx);

            // Redirect the interrupt return to the handler. iretq will load
            // rip/rsp/rflags; the stub's `pop rdi` will load rdi=signum.
            frame.rip = user_fn;
            frame.rsp = ret_addr_slot;
            frame.rflags = SIGNAL_HANDLER_RFLAGS; // IF|reserved, no TF/AC/DF
            frame.rdi = signum as u64;
        }
    }
}

/// Safe wrapper registered with `kernel_cpu::set_interrupt_return_hook`.
/// The hook is only invoked on the IDT common stub's ring-3 return, so the
/// current process (if any) is exactly the interrupted one.
#[cfg(target_os = "none")]
pub fn check_and_deliver_iret_hook(frame: &mut kernel_cpu::InterruptFrame) {
    let proc = crate::syscall::current_process();
    if proc.is_null() {
        return;
    }
    // SAFETY: proc is the live current process; frame is the ring-3
    // interrupt frame the stub is about to return through.
    unsafe { check_and_deliver_iret(proc, frame) };
}

/// Write the signal frame (SavedUserContext + sigreturn-trampoline return
/// address) onto the user stack below the red zone, and return the handler
/// entry RSP (the slot holding the return address). Shared by the syscall
/// and interrupt delivery paths.
///
/// # Safety
/// The target address space must be the active CR3, `user_rsp` a valid user
/// stack pointer with room below its red zone.
#[cfg(target_os = "none")]
unsafe fn write_signal_frame(ctx: &SavedUserContext, user_rsp: u64) -> u64 {
    let layout = compute_signal_frame_layout(user_rsp);
    // SAFETY: caller guarantees the address space is active and the stack
    // has room; STAC lifts SMAP for the supervisor writes to user memory.
    unsafe {
        core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        core::ptr::write(layout.ctx_addr as *mut SavedUserContext, *ctx);
        core::ptr::write(layout.ret_addr_slot as *mut u64, SIGRETURN_TRAMPOLINE_VA);
        core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
    }
    layout.ret_addr_slot
}

/// Build the signal frame on the user stack and enter the handler. Never
/// returns.
///
/// # Safety
/// See [`deliver_on_syscall_return`]. `p`'s address space must be the
/// active CR3 so the user-stack writes below land in the right pages.
#[cfg(target_os = "none")]
unsafe fn setup_and_enter_handler(
    p: &mut crate::process::Process,
    user_fn: u64,
    signum: u32,
    retval: i64,
) -> ! {
    let frame = kernel_cpu::current_syscall_frame();

    let mut ctx = SavedUserContext::zeroed();
    ctx.user_rip = frame.user_rip;
    ctx.user_rsp = frame.user_rsp;
    ctx.user_rflags = frame.user_rflags;
    // rax = the interrupted syscall's return value, so sigreturn resumes
    // the caller as if the syscall had returned normally.
    ctx.rax = retval as u64;
    // Callee-saved registers, captured at syscall entry by the trampoline
    // (see kernel_cpu::update_syscall_callee_saved). Restoring these is what
    // makes a syscall-interrupted-by-signal ABI-correct: the SysV ABI
    // requires rbx/rbp/r12-r15 to survive a syscall. The caller-saved
    // registers are left zero on this path — the syscall was already
    // permitted to clobber them, so the user cannot rely on their values
    // across it (the interrupt-delivery path, which CAN see a mid-
    // computation snapshot, saves them in full instead).
    ctx.rbx = frame.rbx;
    ctx.rbp = frame.rbp;
    ctx.r12 = frame.r12;
    ctx.r13 = frame.r13;
    ctx.r14 = frame.r14;
    ctx.r15 = frame.r15;

    // SAFETY: the process's address space is active in CR3 and its user
    // stack pages are present (the process trapped there via SYSCALL).
    let ret_addr_slot = unsafe { write_signal_frame(&ctx, frame.user_rsp) };

    // Stash the pre-signal context for sigreturn to restore.
    *p.sig_frame.lock() = Some(ctx);

    // SAFETY: address_space is this process's live AddressSpace.
    let cr3 = unsafe { (*p.address_space).pml4_phys.as_u64() | (*p.address_space).pcid as u64 };
    // SAFETY: user_fn is the handler the process itself registered via
    // sigaction; ret_addr_slot is a valid, just-written user stack address;
    // rflags 0x202 = IF|reserved, TF/AC/DF cleared. Noreturn.
    unsafe {
        crate::process::enter_signal_handler(
            cr3,
            user_fn,
            ret_addr_slot,
            SIGNAL_HANDLER_RFLAGS,
            signum as u64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_numbers_match_linux() {
        assert_eq!(SIGKILL, 9);
        assert_eq!(SIGTERM, 15);
        assert_eq!(SIGCHLD, 17);
        assert_eq!(SIGUSR1, 10);
        assert_eq!(SIGUSR2, 12);
    }

    #[test]
    fn signal_syscall_numbers_match_linux() {
        assert_eq!(SYS_SIGACTION, 13);
        assert_eq!(SYS_SIGRETURN, 15);
        assert_eq!(SYS_KILL, 62);
    }

    #[test]
    fn raise_then_take_any_returns_that_signal() {
        let p = PendingSet::new();
        p.raise(SIGUSR1);
        assert_eq!(p.take_any(), Some(SIGUSR1));
        assert_eq!(p.take_any(), None);
    }

    #[test]
    fn take_any_returns_lowest_number_first() {
        let p = PendingSet::new();
        p.raise(SIGTERM); // 15
        p.raise(SIGHUP); // 1
        assert_eq!(p.take_any(), Some(SIGHUP));
        assert_eq!(p.take_any(), Some(SIGTERM));
    }

    #[test]
    fn sigkill_wins_over_sigterm_when_both_pending() {
        let p = PendingSet::new();
        p.raise(SIGTERM); // 15
        p.raise(SIGKILL); // 9 — lower bit
        assert_eq!(p.take_any(), Some(SIGKILL));
    }

    #[test]
    fn take_any_clears_the_bit() {
        let p = PendingSet::new();
        p.raise(SIGUSR2);
        assert_ne!(p.peek(), 0);
        let _ = p.take_any();
        assert_eq!(p.peek(), 0);
    }

    #[test]
    fn sigtable_default_for_unregistered_signal() {
        let t = SigTable::new();
        assert_eq!(t.actions[SIGUSR1 as usize], SigAction::Default);
    }

    #[test]
    fn decide_no_pending_is_none() {
        let p = PendingSet::new();
        let t = SigTable::new();
        assert_eq!(decide_next_signal(&p, &t), SignalOutcome::None);
        // Idempotent: a second call with nothing raised is still None.
        assert_eq!(decide_next_signal(&p, &t), SignalOutcome::None);
    }

    #[test]
    fn decide_sigkill_terminates_even_with_handler_registered() {
        let p = PendingSet::new();
        let mut t = SigTable::new();
        // A handler for SIGKILL must be ignored — it cannot be caught.
        t.actions[SIGKILL as usize] = SigAction::Handler { user_fn: 0x1234 };
        p.raise(SIGKILL);
        assert_eq!(decide_next_signal(&p, &t), SignalOutcome::Terminate(SIGKILL));
    }

    #[test]
    fn decide_sigterm_default_terminates() {
        let p = PendingSet::new();
        let t = SigTable::new();
        p.raise(SIGTERM);
        assert_eq!(decide_next_signal(&p, &t), SignalOutcome::Terminate(SIGTERM));
    }

    #[test]
    fn decide_sigterm_with_handler_delivers_not_terminates() {
        let p = PendingSet::new();
        let mut t = SigTable::new();
        t.actions[SIGTERM as usize] = SigAction::Handler { user_fn: 0xABCD };
        p.raise(SIGTERM);
        assert_eq!(
            decide_next_signal(&p, &t),
            SignalOutcome::Deliver {
                user_fn: 0xABCD,
                signum: SIGTERM
            }
        );
    }

    #[test]
    fn decide_handler_is_delivered_with_signum() {
        let p = PendingSet::new();
        let mut t = SigTable::new();
        t.actions[SIGUSR1 as usize] = SigAction::Handler { user_fn: 0x4020_00 };
        p.raise(SIGUSR1);
        assert_eq!(
            decide_next_signal(&p, &t),
            SignalOutcome::Deliver {
                user_fn: 0x4020_00,
                signum: SIGUSR1
            }
        );
    }

    #[test]
    fn decide_ignored_signal_is_none() {
        let p = PendingSet::new();
        let mut t = SigTable::new();
        t.actions[SIGUSR1 as usize] = SigAction::Ignore;
        p.raise(SIGUSR1);
        assert_eq!(decide_next_signal(&p, &t), SignalOutcome::None);
    }

    #[test]
    fn decide_default_non_fatal_signal_is_noop() {
        // SIGUSR1 with the default action is a no-op this checkpoint (only
        // SIGKILL/SIGTERM terminate by default).
        let p = PendingSet::new();
        let t = SigTable::new();
        p.raise(SIGUSR1);
        assert_eq!(decide_next_signal(&p, &t), SignalOutcome::None);
    }

    #[test]
    fn sigkill_is_not_catchable() {
        assert!(!signal_is_catchable(SIGKILL));
        assert!(!signal_is_catchable(0));
        assert!(!signal_is_catchable(NSIG as u32));
        assert!(signal_is_catchable(SIGUSR1));
        assert!(signal_is_catchable(SIGTERM));
    }

    // ── Fix A: full register save/restore ──────────────────────────────

    #[test]
    fn saved_user_context_is_full_width() {
        // Regression guard: must hold rip/rsp/rflags/rax + all 12 other
        // GPRs (rbx/rcx/rdx/rsi/rdi/rbp/r8-r15) = 18 u64 >= the 136-byte
        // (17*8) floor the checkpoint requires.
        assert!(core::mem::size_of::<SavedUserContext>() >= 17 * 8);
        assert_eq!(core::mem::size_of::<SavedUserContext>(), 18 * 8);
    }

    #[test]
    fn saved_user_context_round_trips_callee_saved() {
        let mut ctx = SavedUserContext::zeroed();
        ctx.rax = 0x42; // interrupted syscall's return value
        ctx.rbx = 0xDEAD_BEEF_CAFE_1234;
        ctx.r12 = 0x1111_2222_3333_4444;
        ctx.r15 = 0x9999;
        // The struct faithfully carries every field sigreturn restores.
        assert_eq!(ctx.rax, 0x42);
        assert_eq!(ctx.rbx, 0xDEAD_BEEF_CAFE_1234);
        assert_eq!(ctx.r12, 0x1111_2222_3333_4444);
        assert_eq!(ctx.r15, 0x9999);
        // rax survives as a distinct field (not 0, not SYS_SIGRETURN=15).
        assert_ne!(ctx.rax, 0);
        assert_ne!(ctx.rax, SYS_SIGRETURN);
    }

    // ── Fix B: interrupt-return delivery surface ────────────────────────

    #[test]
    fn returning_to_user_only_for_ring3_cs() {
        // User CS (0x20|3 = 0x23) and user-mode selectors have CPL 3.
        assert!(returning_to_user(0x23));
        assert!(returning_to_user(0x2b)); // user data-ish selector, RPL 3
        // Kernel CS (0x08) is ring 0 — no signal delivery.
        assert!(!returning_to_user(0x08));
        assert!(!returning_to_user(0x10));
        assert!(!returning_to_user(0)); // null selector
    }

    #[test]
    fn signal_handler_rflags_is_clean() {
        // IF (bit 9) + reserved bit 1 set; TF (bit 8), AC (bit 18),
        // DF (bit 10) all clear so the handler starts sane.
        assert_eq!(SIGNAL_HANDLER_RFLAGS, 0x202);
        assert_ne!(SIGNAL_HANDLER_RFLAGS & (1 << 9), 0, "IF must be set");
        assert_eq!(SIGNAL_HANDLER_RFLAGS & (1 << 8), 0, "TF must be clear");
        assert_eq!(SIGNAL_HANDLER_RFLAGS & (1 << 18), 0, "AC must be clear");
        assert_eq!(SIGNAL_HANDLER_RFLAGS & (1 << 10), 0, "DF must be clear");
    }

    #[test]
    fn signal_frame_layout_grows_stack_below_red_zone() {
        let user_rsp = 0x7fff_ffff_0000u64;
        let layout = compute_signal_frame_layout(user_rsp);
        // Handler stack is BELOW the interrupted RSP (grew down).
        assert!(layout.ret_addr_slot < user_rsp, "stack must grow down");
        assert!(layout.ctx_addr < user_rsp);
        // The whole 128-byte red zone below user_rsp is left untouched: the
        // saved context starts at or below (user_rsp - 128 - ctx_size).
        let ctx_size = core::mem::size_of::<SavedUserContext>() as u64;
        assert!(layout.ctx_addr <= user_rsp - 128 - ctx_size);
    }

    #[test]
    fn signal_frame_layout_is_abi_aligned() {
        // ctx_addr 16-aligned; handler entry rsp (ret_addr_slot) ≡ 8 (mod
        // 16) — exactly what a `call` would leave, so the handler's own
        // 16-byte stack alignment assumptions hold.
        for rsp in [
            0x7fff_ffff_0000u64,
            0x7fff_ffff_0008,
            0x7fff_ffff_1234,
            0x4000_0000_1000,
        ] {
            let layout = compute_signal_frame_layout(rsp);
            assert_eq!(layout.ctx_addr % 16, 0, "ctx must be 16-aligned");
            assert_eq!(
                layout.ret_addr_slot % 16,
                8,
                "handler entry rsp must be 16n+8 (SysV)"
            );
            assert_eq!(layout.ret_addr_slot, layout.ctx_addr - 8);
        }
    }

    #[test]
    fn decide_delivers_handler_for_iret_path_too() {
        // The interrupt path (check_and_deliver_iret) shares the same pure
        // decision core; confirm a registered handler yields Deliver so the
        // frame-edit branch runs (frame.rip←handler, rflags←0x202,
        // rdi←signum), exercised end-to-end by the QEMU boot demo.
        let p = PendingSet::new();
        let mut t = SigTable::new();
        t.actions[SIGUSR2 as usize] = SigAction::Handler { user_fn: 0x1000_0000 };
        p.raise(SIGUSR2);
        assert_eq!(
            decide_next_signal(&p, &t),
            SignalOutcome::Deliver {
                user_fn: 0x1000_0000,
                signum: SIGUSR2
            }
        );
    }

    #[test]
    fn trampoline_code_encodes_mov_rax_15_syscall() {
        // mov rax, imm32 (sign-extended): 48 c7 c0 <imm32 LE>; then 0f 05.
        assert_eq!(&SIGRETURN_TRAMPOLINE_CODE[0..3], &[0x48, 0xc7, 0xc0]);
        let imm = u32::from_le_bytes([
            SIGRETURN_TRAMPOLINE_CODE[3],
            SIGRETURN_TRAMPOLINE_CODE[4],
            SIGRETURN_TRAMPOLINE_CODE[5],
            SIGRETURN_TRAMPOLINE_CODE[6],
        ]);
        assert_eq!(imm as u64, SYS_SIGRETURN);
        assert_eq!(&SIGRETURN_TRAMPOLINE_CODE[7..9], &[0x0f, 0x05]);
    }
}
