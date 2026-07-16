use crate::address_space::AddressSpace;
use crate::elf::parse_elf;
use crate::fd::FdTable;
use crate::signal::{PendingSet, SavedUserContext, SigTable};
#[cfg(target_os = "none")]
use crate::syscall::current_process;
use crate::syscall::{SERIAL_VNODE_PTR, register_process};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub type Pid = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    Zombie,
}

#[repr(C, align(64))]
pub struct Process {
    pub pid: Pid,
    pub state: ProcessState,
    pub exit_code: i32,
    /// A live, refcounted `AddressSpace` (see `AddressSpace::alloc_shared`).
    /// Before `CLONE_VM` (`crate::clone`) this was `AddressSpace` owned by
    /// value inline in this same struct -- exactly one `Process` ever
    /// pointed at it. `CLONE_VM` requires more than one `Process` to point
    /// at the exact same live `AddressSpace`, which is only possible once
    /// it has an address of its own; every other `Process` constructor
    /// (`create`, `fork`, `execve`'s Phase 2) now goes through
    /// `AddressSpace::alloc_shared` to get one, even though only
    /// `crate::clone::sys_clone`'s `CLONE_VM` path actually shares the
    /// resulting pointer.
    pub address_space: *mut AddressSpace,
    pub thread: *mut kernel_sched::Thread,
    pub name: [u8; 16],
    pub user_rip: u64,
    pub user_rsp: u64,
    pub user_rflags: u64,
    pub user_rax: i64,
    pub fd_table: kernel_sync::SpinLock<FdTable>,
    /// PID of the process that forked/cloned this one, `0` for the initial
    /// process (which has no parent). `sys_exit` raises SIGCHLD against it.
    pub parent_pid: Pid,
    /// Registered signal handlers.
    pub sig_table: kernel_sync::SpinLock<SigTable>,
    /// Pending-signal bitmask (see `crate::signal::PendingSet`); written by
    /// `kill()` from any CPU without taking the process lock.
    pub pending: PendingSet,
    /// Pre-signal user context saved during handler delivery, restored by
    /// `sigreturn`. `None` when no handler is currently executing.
    pub sig_frame: kernel_sync::SpinLock<Option<SavedUserContext>>,
    /// PIDs of this process's live-or-zombie children (`0` = empty slot).
    /// Only ever mutated by this process's own thread (in fork/clone/reap),
    /// never by a child, so there is no cross-process contention — the lock
    /// is for the memory model and `wait4`'s scan, not contention.
    pub children: kernel_sync::SpinLock<[u32; MAX_CHILDREN]>,
    /// Number of occupied `children` slots (a cached count for the fast
    /// `ECHILD` check in `sys_wait4` without scanning the whole array).
    pub child_count: AtomicU32,
    /// Queue the process blocks on in `sys_wait4` until a child exits and
    /// `Process::exit` wakes it. Embedded by value: `Process` is placement-
    /// written to a stable frame and never moved, so the `WaitQueue`'s
    /// lazily-initialized self-referential sentinel stays valid.
    pub wait_queue: kernel_sched::WaitQueue,
}

/// Maximum simultaneously-tracked children per process. A 33rd fork/clone
/// fails with `-EAGAIN` (see `sys_fork`/`sys_clone`).
pub const MAX_CHILDREN: usize = 32;

// A Process is placement-written to a single alloc_frame() (4 KiB). If it
// ever exceeds that, the tail fields spill past the frame boundary into an
// unowned physical frame — silent corruption (which is exactly how the
// children array first showed up as garbage). Keep it under one frame.
const _: () = assert!(
    core::mem::size_of::<Process>() <= 4096,
    "Process exceeds one 4 KiB frame; shrink it or allocate multiple frames"
);

unsafe impl Send for Process {}

/// Add `child_pid` to `parent`'s children list. Returns `false` if the list
/// is full (all [`MAX_CHILDREN`] slots occupied).
///
/// # Safety
/// `parent` must be a live `Process`. Called only from the parent's own
/// thread (fork/clone), so no other thread mutates this list concurrently.
// Used by the target-only fork/clone bodies and reap, plus host tests.
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub(crate) unsafe fn add_child(parent: *mut Process, child_pid: u32) -> bool {
    // SAFETY: caller guarantees `parent` is live.
    let mut ch = unsafe { (*parent).children.lock() };
    for slot in ch.iter_mut() {
        if *slot == 0 {
            *slot = child_pid;
            drop(ch);
            // SAFETY: parent is live.
            unsafe { (*parent).child_count.fetch_add(1, Ordering::Relaxed) };
            return true;
        }
    }
    false
}

/// Remove `child_pid` from `parent`'s children list (no-op if absent).
///
/// # Safety
/// `parent` must be a live `Process`; called only from the parent's own
/// thread (reap).
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub(crate) unsafe fn remove_child(parent: *mut Process, child_pid: u32) {
    // SAFETY: caller guarantees `parent` is live.
    let mut ch = unsafe { (*parent).children.lock() };
    for slot in ch.iter_mut() {
        if *slot == child_pid {
            *slot = 0;
            drop(ch);
            // SAFETY: parent is live.
            unsafe { (*parent).child_count.fetch_sub(1, Ordering::Relaxed) };
            return;
        }
    }
}

/// `true` iff `parent`'s children list has no free slot. Checked early in
/// fork/clone (before building the child) so a full list fails cleanly with
/// `-EAGAIN` rather than needing to roll a half-built child back.
///
/// # Safety
/// `parent` must be a live `Process`; called from the parent's own thread.
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub(crate) unsafe fn children_full(parent: *mut Process) -> bool {
    // SAFETY: caller guarantees `parent` is live.
    let ch = unsafe { (*parent).children.lock() };
    ch.iter().all(|&s| s != 0)
}

impl Process {
    /// Create a new process from an ELF64 static executable byte slice.
    ///
    /// `elf_data` is only read during this call; the kernel does not keep a
    /// reference after `create()` returns.
    ///
    /// # Safety
    /// `elf_data` must be a valid ELF64 static executable for the Rumicos ABI.
    pub unsafe fn create(elf_data: &[u8], name: &str) -> *mut Process {
        let elf = parse_elf(elf_data).expect("invalid ELF");

        let mut address_space = AddressSpace::new();
        for seg in elf.segments[..elf.seg_count].iter() {
            address_space.map_segment(seg, elf_data);
        }
        let user_rsp = address_space.map_user_stack();
        // Every process needs the sigreturn trampoline mapped at its fixed
        // user VA so a returning signal handler has something to `ret` into
        // (see crate::signal). fork inherits it via the CoW copy and clone
        // via the shared address space, so this only maps it for a
        // from-scratch image (Process::create + execve's Phase 1).
        address_space.map_sigreturn_trampoline();
        // Promote onto its own dedicated frame -- see AddressSpace::alloc_shared's
        // doc comment for why this indirection exists at all.
        let address_space = AddressSpace::alloc_shared(address_space);

        #[cfg(target_os = "none")]
        let thread = unsafe { kernel_sched::alloc_kernel_thread_raw(ring3_entry_trampoline, 0) };
        #[cfg(not(target_os = "none"))]
        let thread = core::ptr::null_mut();

        // Pre-populate fd 0/1/2 with the serial VNode if VFS is already up.
        let mut fd_tab = FdTable::new();
        let serial_ptr = SERIAL_VNODE_PTR.load(Ordering::Acquire);
        if serial_ptr != 0 {
            fd_tab.alloc(serial_ptr, crate::fd::O_RDONLY); // fd 0 = stdin
            fd_tab.alloc(serial_ptr, crate::fd::O_WRONLY); // fd 1 = stdout
            fd_tab.alloc(serial_ptr, crate::fd::O_WRONLY); // fd 2 = stderr
        }

        let frame = kernel_memory::alloc_frame();
        let process_ptr = (0xffff_8000_0000_0000u64 + frame.as_u64()) as *mut Process;

        let mut p = Process {
            pid: alloc_pid(),
            state: ProcessState::Running,
            exit_code: 0,
            address_space,
            thread,
            name: [0; 16],
            user_rip: elf.entry,
            user_rsp,
            user_rflags: 0x202,
            user_rax: 0,
            fd_table: kernel_sync::SpinLock::new(fd_tab),
            parent_pid: 0,
            sig_table: kernel_sync::SpinLock::new(SigTable::new()),
            pending: PendingSet::new(),
            sig_frame: kernel_sync::SpinLock::new(None),
            children: kernel_sync::SpinLock::new([0; MAX_CHILDREN]),
            child_count: AtomicU32::new(0),
            wait_queue: kernel_sched::WaitQueue::new(),
        };
        copy_name(&mut p.name, name);
        let pid = p.pid;

        // SAFETY: freshly allocated frame; no other aliases.
        unsafe {
            process_ptr.write(p);
            // Explicitly zero the children array element-by-element AFTER
            // the placement-write. rustc 1.97.0 does not reliably initialize
            // this `[u32; MAX_CHILDREN]` through the aggregate `Process { .. }`
            // literal above (same aggregate-store miscompile family as the
            // vmas bug documented in address_space.rs) — the frame comes
            // from a non-zeroed alloc_frame, so a missed init leaves stale
            // garbage that sys_wait4 would read as phantom children. A
            // per-element loop compiles correctly.
            for slot in (*process_ptr).children.lock().iter_mut() {
                *slot = 0;
            }
            (*process_ptr).child_count.store(0, Ordering::Relaxed);
        }
        register_process(process_ptr);
        crate::ptable::ptable_insert(pid, process_ptr);
        process_ptr
    }

    pub fn exit(&mut self, code: i32) {
        self.state = ProcessState::Zombie;
        self.exit_code = code;
        // Notify the parent (if any): raise SIGCHLD AND wake it if it is
        // blocked in wait4. Best-effort — if the parent has already exited
        // it is gone from ptable (this zombie is now an orphan; see the
        // orphan-leak limitation, reparenting to PID 1 is future work).
        //
        // Deliberately does NOT ptable_remove(self.pid): a zombie must stay
        // findable by PID so the parent's sys_wait4 can locate it and read
        // its exit_code. Only reap_zombie removes it from ptable and frees
        // its frames. The execution resources (kernel stack, address space)
        // stay allocated too until reaped — the thread is Dead and never
        // runs again, so only ~4 KiB (Process frame) + the kstack sit idle.
        if self.parent_pid != 0
            && let Some(parent) = crate::ptable::ptable_find(self.parent_pid)
        {
            // SAFETY: ptable only holds live Process pointers; the parent
            // cannot be mid-reap of THIS child (it would have to have found
            // us as a zombie first, which requires this function to have
            // already returned). raise touches only an atomic; wake_one
            // takes only the parent's wait-queue lock.
            unsafe {
                (*parent).pending.raise(crate::signal::SIGCHLD);
                kernel_sched::wake_one(&(*parent).wait_queue);
            }
        }
        unsafe {
            (*self.thread).state = kernel_sched::ThreadState::Dead;
            kernel_sched::schedule(kernel_cpu::current_cpu_id());
        }
    }

    /// Replace this process's image with the ELF at `path`, reusing the
    /// same PID and file descriptor table.
    ///
    /// A convenience entry point for callers that already hold a
    /// `&mut Process` — `path`/`argv`/`envp` must already be safely copied
    /// out of user memory. [`crate::syscall::sys_execve`] calls
    /// [`crate::exec::do_execve`] directly instead (it only has a raw
    /// `*mut Process` from [`crate::current_process`] at that point, and
    /// `do_execve` re-derives it anyway for Phase 2), but the two are
    /// equivalent. See `do_execve` for the full atomicity contract: on
    /// failure `self` is left completely unchanged; on success this never
    /// returns.
    ///
    /// # Safety
    /// Must be called with interrupts disabled, from `self`'s own kernel
    /// thread, exactly as `do_execve` requires.
    pub unsafe fn exec(&mut self, path: &str, argv: &[&str], envp: &[&str]) -> i32 {
        // SAFETY: caller upholds do_execve's preconditions.
        unsafe { crate::exec::do_execve(path, argv, envp) }
    }
}

static NEXT_PID: AtomicU32 = AtomicU32::new(1);
pub fn alloc_pid() -> Pid {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}

/// Build a bare `Process` on the host for unit tests. `thread`/
/// `address_space` are null: only the fields the children/wait bookkeeping
/// touches (children, child_count, wait_queue, state, exit_code) are
/// meaningful — the tests that use it never dereference thread/address_space.
#[cfg(test)]
pub(crate) fn test_process() -> Process {
    Process {
        pid: 0,
        state: ProcessState::Running,
        exit_code: 0,
        address_space: core::ptr::null_mut(),
        thread: core::ptr::null_mut(),
        name: [0; 16],
        user_rip: 0,
        user_rsp: 0,
        user_rflags: 0,
        user_rax: 0,
        fd_table: kernel_sync::SpinLock::new(FdTable::new()),
        parent_pid: 0,
        sig_table: kernel_sync::SpinLock::new(SigTable::new()),
        pending: PendingSet::new(),
        sig_frame: kernel_sync::SpinLock::new(None),
        children: kernel_sync::SpinLock::new([0; MAX_CHILDREN]),
        child_count: AtomicU32::new(0),
        wait_queue: kernel_sched::WaitQueue::new(),
    }
}

fn copy_name(dst: &mut [u8; 16], src: &str) {
    let b = src.as_bytes();
    let n = core::cmp::min(b.len(), dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&b[..n]);
}

// ─── per-CPU address-space tracking / context-switch hook ────────────────

/// Matches `kernel_sched::percpu::MAX_CPUS` and shootdown's
/// `MAX_SHOOTDOWN_CPUS` (64) — duplicated as a plain literal for the same
/// reason shootdown.rs documents on its own copy.
const MAX_CPUS: usize = 64;

/// Which `AddressSpace` each CPU currently has loaded in `CR3` (`0` = none
/// yet, i.e. that CPU is still on the boot page tables). Each slot is only
/// ever written by the CPU it names — from [`activate_address_space`], which
/// runs with IF=0 on that CPU — so no cross-CPU write race exists; other
/// CPUs never read these slots at all.
static CPU_CURRENT_AS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

/// Make `new_as` the active address space on `cpu_id`: mark it active for
/// TLB shootdown, load its `CR3` (PCID in bits 0-11, no-flush bit clear so
/// any stale entries for a reused PCID are flushed), and only then mark the
/// previously loaded `AddressSpace` inactive.
///
/// The ordering is load-bearing (see `crate::shootdown`'s docs): the
/// dangerous race direction is a CPU *using* an `AddressSpace` while not
/// marked in its `active_cpus` — so the new space is marked active BEFORE
/// its `CR3` goes live, and the old one is unmarked only AFTER `CR3` no
/// longer references it. In between both are marked, which at worst costs
/// one harmless extra shootdown IPI.
///
/// No-op when `new_as` is already this CPU's active space (e.g. switching
/// between two `CLONE_VM` sibling threads) — that skip is what keeps the
/// TLB warm across same-address-space switches.
///
/// # Safety
/// Must run on CPU `cpu_id` with interrupts disabled. `new_as` must be a
/// live `AddressSpace` from [`AddressSpace::alloc_shared`], and the pointer
/// previously recorded in this CPU's slot (if any) must still be live —
/// teardown (`AddressSpace::drop_ref` reaching zero) must not race with a
/// CPU that still has the space recorded here.
pub(crate) unsafe fn activate_address_space(new_as: *mut AddressSpace, cpu_id: u32) {
    let slot = &CPU_CURRENT_AS[cpu_id as usize % MAX_CPUS];
    let old_as = slot.load(Ordering::Acquire) as *mut AddressSpace;
    if old_as == new_as {
        return;
    }
    // SAFETY: new_as is live per this function's own precondition.
    unsafe { (*new_as).active_cpus.mark_active(cpu_id) };
    slot.store(new_as as usize, Ordering::Release);
    // SAFETY: new_as's PML4 contains the shared kernel half, so kernel
    // execution continues unaffected across the CR3 write.
    unsafe { (*new_as).activate() };
    if !old_as.is_null() {
        // SAFETY: old_as was recorded by a previous activate on this CPU and
        // is still live per this function's precondition.
        unsafe { (*old_as).active_cpus.mark_inactive(cpu_id) };
    }
}

/// Registered with [`kernel_sched::register_context_switch_hook`] by
/// [`crate::init`]. Runs with IF=0 immediately before every context switch,
/// on the outgoing thread's kernel stack.
///
/// This is what makes multitasking between *different* processes correct at
/// the memory level: before it existed, `CR3` was only ever loaded on a
/// thread's very first dispatch (`ring3_entry_rust`) and in `execve`'s
/// Phase 2, so an ordinary A→B switch between two already-running processes
/// would resume B's user code under A's page tables.
pub(crate) unsafe fn sched_context_switch_hook(next: *mut kernel_sched::Thread, cpu_id: u32) {
    let proc = crate::syscall::process_for_thread(next);
    if proc.is_null() {
        // Pure kernel thread (idle, kidle): it has no address space of its
        // own, and every AddressSpace maps the kernel half, so the previous
        // CR3 stays loaded. The previous AddressSpace deliberately stays
        // marked active too — its page tables are still what this CPU walks
        // on any TLB miss, so shootdowns must keep reaching it.
        return;
    }
    // SAFETY: non-null PROCESS_TABLE entries are live; their address_space
    // came from AddressSpace::alloc_shared and is kept alive by the process
    // itself. The hook contract (IF=0, on-CPU) forwards to activate's.
    unsafe { activate_address_space((*proc).address_space, cpu_id) };
}

// ─── ring-3 entry trampoline ─────────────────────────────────────────────

#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .section .text.ring3_entry_trampoline,"ax",@progbits
    .global ring3_entry_trampoline
    .type   ring3_entry_trampoline, @function
    .align  16
ring3_entry_trampoline:
    .byte 0xf3, 0x0f, 0x1e, 0xfa      /* endbr64 */
    sub  rsp, 8
    call ring3_entry_rust
    ud2
    .size ring3_entry_trampoline, . - ring3_entry_trampoline
    "#
);
#[cfg(target_os = "none")]
unsafe extern "C" {
    pub(crate) fn ring3_entry_trampoline() -> !;
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn ring3_entry_rust() -> ! {
    let p = current_process();
    assert!(!p.is_null(), "ring3_entry without current process");
    let p = unsafe { &mut *p };
    let as_ptr = p.address_space;
    // The scheduler's context-switch hook (sched_context_switch_hook) has
    // normally already activated this AddressSpace on this CPU by the time
    // this first dispatch runs, making this a no-op; calling it again keeps
    // the mark_active-before-CR3 ordering and the per-CPU CURRENT_AS slot
    // correct even on a path that reached here without the hook.
    // SAFETY: as_ptr is a live AddressSpace (see AddressSpace::alloc_shared)
    // and this runs with IF=0 on this thread's own first dispatch.
    unsafe { activate_address_space(as_ptr, kernel_cpu::current_cpu_id()) };
    // CR3 = PML4 physical base | PCID (bits 0-11)
    // The PML4 frame is 4 KiB-aligned so its low 12 bits are 0.
    // SAFETY: as_ptr is live per the same contract as above.
    let cr3 = unsafe { (*as_ptr).pml4_phys.as_u64() | (*as_ptr).pcid as u64 };
    unsafe { enter_user_mode(cr3, p.user_rip, p.user_rsp, p.user_rflags, p.user_rax) }
}

/// Load `cr3`, set up the registers `SYSRET` expects, and drop to ring 3 at
/// `rip` with stack pointer `rsp`. Used both for a brand-new process's very
/// first run (via [`ring3_entry_rust`]) and directly by `execve()`'s Phase 2
/// (`crate::exec::do_execve`) once it has installed the new address space —
/// `execve` doesn't go through `ring3_entry_trampoline`/`schedule()` since
/// it isn't "switching to" a different thread, just continuing the current
/// one with a different image; see the module docs in `exec.rs`.
#[cfg(target_os = "none")]
pub(crate) unsafe fn enter_user_mode(cr3: u64, rip: u64, rsp: u64, rflags: u64, rax: i64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov cr3, {cr3}",   // activate user address space
            "mov rcx, {rip}",   // RCX → RIP on SYSRET
            "mov rsp, {rsp}",   // user RSP
            "mov r11, {rflags}", // RFLAGS restored by SYSRET
            "mov rax, {rax}",   // syscall return value
            "swapgs",           // kernel GS ↔ IA32_KERNEL_GS_BASE
            "sysretq",
            cr3 = in(reg) cr3,
            rip = in(reg) rip,
            rsp = in(reg) rsp,
            rflags = in(reg) rflags,
            rax = in(reg) rax,
            options(noreturn)
        )
    }
}

/// Like [`enter_user_mode`], but enters a signal handler: `rip` is the
/// user handler, `rsp` already points at the pushed sigreturn-trampoline
/// return address, and `arg` (the signum) is placed in RDI — the SysV
/// first-argument register — so the handler receives it as its parameter.
/// RAX is zeroed (the handler's return value is meaningless). Never
/// returns; the handler eventually `ret`s into the sigreturn trampoline.
///
/// # Safety
/// Same contract as [`enter_user_mode`]: `cr3` names the target process's
/// live address space, and `rip`/`rsp` are valid user addresses within it.
#[cfg(target_os = "none")]
pub(crate) unsafe fn enter_signal_handler(
    cr3: u64,
    rip: u64,
    rsp: u64,
    rflags: u64,
    arg: u64,
) -> ! {
    unsafe {
        core::arch::asm!(
            "mov cr3, {cr3}",
            "mov rcx, {rip}",    // RCX → RIP on SYSRET
            "mov rsp, {rsp}",    // handler RSP (points at trampoline ret-addr)
            "mov r11, {rflags}", // RFLAGS restored by SYSRET
            "mov rdi, {arg}",    // SysV arg0 = signum
            "xor eax, eax",      // handler return value is unused
            "swapgs",
            "sysretq",
            cr3 = in(reg) cr3,
            rip = in(reg) rip,
            rsp = in(reg) rsp,
            rflags = in(reg) rflags,
            arg = in(reg) arg,
            options(noreturn)
        )
    }
}

/// Restore a full [`SavedUserContext`] and return to ring 3 — the
/// `sigreturn` counterpart to [`enter_signal_handler`]. Loads every GPR
/// from `ctx` (rax = the interrupted syscall's return value; rbx/rbp/r12-r15
/// = the user's callee-saved; rdx/rsi/rdi/r8/r9/r10 = the rest), then
/// `sysretq`s to `ctx.user_rip` with `ctx.user_rsp`/`ctx.user_rflags`.
///
/// `ctx` lives in kernel memory (the process's `sig_frame`), which is
/// direct-mapped in every address space, so it stays readable across the
/// `mov cr3` below.
///
/// LIMITATION: `sysretq` forcibly loads RIP from RCX and RFLAGS from R11,
/// so the user's original RCX and R11 cannot be restored here — they are
/// clobbered across signal delivery. Callee-saved registers (the ones the
/// ABI actually requires to survive) ARE restored. A future IRETQ-based
/// sigreturn would preserve RCX/R11 too.
///
/// # Safety
/// `ctx` must point at a valid `SavedUserContext` whose `user_rip`/
/// `user_rsp` are valid user addresses in the address space named by `cr3`,
/// and `cr3` must be that process's live address space. Never returns.
#[cfg(target_os = "none")]
pub(crate) unsafe fn enter_user_mode_restoring(ctx: *const SavedUserContext, cr3: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov cr3, {cr3}",
            "mov rax, {ctx}",             // rax = ctx pointer (temporary base)
            "mov rcx, [rax + {off_rip}]",    // RCX → RIP on SYSRET
            "mov r11, [rax + {off_rflags}]", // R11 → RFLAGS on SYSRET
            "mov rbx, [rax + {off_rbx}]",
            "mov rdx, [rax + {off_rdx}]",
            "mov rsi, [rax + {off_rsi}]",
            "mov rdi, [rax + {off_rdi}]",
            "mov rbp, [rax + {off_rbp}]",
            "mov r8,  [rax + {off_r8}]",
            "mov r9,  [rax + {off_r9}]",
            "mov r10, [rax + {off_r10}]",
            "mov r12, [rax + {off_r12}]",
            "mov r13, [rax + {off_r13}]",
            "mov r14, [rax + {off_r14}]",
            "mov r15, [rax + {off_r15}]",
            "mov rsp, [rax + {off_rsp}]",    // user RSP
            "mov rax, [rax + {off_rax}]",    // rax = return value (frees base last)
            "swapgs",
            "sysretq",
            cr3 = in(reg) cr3,
            ctx = in(reg) ctx,
            off_rip = const core::mem::offset_of!(SavedUserContext, user_rip),
            off_rsp = const core::mem::offset_of!(SavedUserContext, user_rsp),
            off_rflags = const core::mem::offset_of!(SavedUserContext, user_rflags),
            off_rax = const core::mem::offset_of!(SavedUserContext, rax),
            off_rbx = const core::mem::offset_of!(SavedUserContext, rbx),
            off_rdx = const core::mem::offset_of!(SavedUserContext, rdx),
            off_rsi = const core::mem::offset_of!(SavedUserContext, rsi),
            off_rdi = const core::mem::offset_of!(SavedUserContext, rdi),
            off_rbp = const core::mem::offset_of!(SavedUserContext, rbp),
            off_r8 = const core::mem::offset_of!(SavedUserContext, r8),
            off_r9 = const core::mem::offset_of!(SavedUserContext, r9),
            off_r10 = const core::mem::offset_of!(SavedUserContext, r10),
            off_r12 = const core::mem::offset_of!(SavedUserContext, r12),
            off_r13 = const core::mem::offset_of!(SavedUserContext, r13),
            off_r14 = const core::mem::offset_of!(SavedUserContext, r14),
            off_r15 = const core::mem::offset_of!(SavedUserContext, r15),
            options(noreturn)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address_space::AddressSpace;

    #[test]
    fn alloc_pid_increments() {
        let a = alloc_pid();
        let b = alloc_pid();
        assert_eq!(a + 1, b);
    }

    // CPU_CURRENT_AS is a process-wide static indexed by cpu_id, so this
    // whole first-activation/no-op/switch sequence lives in ONE sequential
    // test on a cpu_id no other test uses (the same single-sequential-test
    // rationale syscall.rs's hook round-trip tests document). On the host,
    // AddressSpace::activate() is a no-op, so this exercises exactly the
    // bookkeeping half: active_cpus transitions and the per-CPU slot.
    #[test]
    fn activate_address_space_tracks_active_cpus_and_current_slot() {
        const CPU: u32 = 42;
        let mut a = AddressSpace::new();
        let mut b = AddressSpace::new();
        let a_ptr = &mut a as *mut AddressSpace;
        let b_ptr = &mut b as *mut AddressSpace;

        // First activation from the boot state (slot empty): only `a` marked.
        // SAFETY: host build; activate() is a no-op, pointers are live locals.
        unsafe { activate_address_space(a_ptr, CPU) };
        assert_eq!(a.active_cpus.snapshot(), 1 << CPU);
        assert_eq!(b.active_cpus.snapshot(), 0);

        // Re-activating the already-active space must be a no-op (this is
        // the CLONE_VM-sibling / warm-TLB fast path).
        // SAFETY: as above.
        unsafe { activate_address_space(a_ptr, CPU) };
        assert_eq!(a.active_cpus.snapshot(), 1 << CPU);

        // Switching to `b` marks it active and unmarks `a` — never leaving
        // this CPU tracked in neither.
        // SAFETY: as above.
        unsafe { activate_address_space(b_ptr, CPU) };
        assert_eq!(b.active_cpus.snapshot(), 1 << CPU);
        assert_eq!(a.active_cpus.snapshot(), 0);
    }
}
