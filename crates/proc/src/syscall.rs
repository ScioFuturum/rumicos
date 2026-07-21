use crate::process::Process;
#[cfg(target_os = "none")]
use core::sync::atomic::AtomicBool;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_GETPID: u64 = 39;
pub const SYS_EXECVE: u64 = 59;
pub const SYS_EXIT: u64 = 60;

/// Linux `prctl(2)` syscall number. Only `PR_GET_NAME` is implemented (see
/// [`sys_prctl`]); every other option returns `EINVAL`.
pub const SYS_PRCTL: u64 = 157;
/// `prctl` option: copy the caller's 16-byte process name to `arg2`.
pub const PR_GET_NAME: u64 = 16;

/// Linux `dup(2)` / `dup2(2)` syscall numbers.
pub const SYS_DUP: u64 = 32;
pub const SYS_DUP2: u64 = 33;

const ENOENT: i64 = -2;
const EFAULT: i64 = -14;
const ENOSYS: i64 = -38;
const USER_TOP: usize = 0x0000_8000_0000_0000;
const USER_MIN: usize = 4096;
const MAX_PROCESSES: usize = 64;

static PROCESS_TABLE: [AtomicUsize; MAX_PROCESSES] = [const { AtomicUsize::new(0) }; MAX_PROCESSES];

/// Secondary syscall handler registered by kernel-fs.
/// Called before proc's own fallbacks for all non-EXIT/non-GETPID syscalls.
static CHAIN_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Loader registered by kernel-fs for `SYS_EXECVE` to resolve a path through
/// the real VFS. kernel-proc cannot depend on kernel-fs directly — kernel-fs
/// already depends on kernel-proc (for `current_process`, `is_user_ptr`,
/// `register_extra`) and Cargo doesn't allow the cycle that would create —
/// so this mirrors `CHAIN_HANDLER`/`SERIAL_VNODE_PTR`'s existing pattern of
/// a registration hook kernel-fs fills in at VFS init time. See
/// [`register_exec_loader`] / [`load_exec_file`] and `crate::exec`.
static EXEC_LOADER: AtomicUsize = AtomicUsize::new(0);

/// Direct-map VA of the /dev/serial VNode; set by kernel-fs on init.
/// Process::create reads this to pre-populate fd 0/1/2.
pub static SERIAL_VNODE_PTR: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "none")]
static SERIAL_READY: AtomicBool = AtomicBool::new(false);

// ─── public init API ──────────────────────────────────────────────────────

pub fn register() {
    #[cfg(target_os = "none")]
    unsafe {
        serial_init();
    }
    kernel_cpu::set_syscall_handler(proc_syscall_handler);
}

/// Register a secondary handler (e.g. kernel-fs VFS handler).
/// When set, it is called first for every syscall that proc does not own
/// exclusively (SYS_EXIT, SYS_GETPID).  If the chain handler returns
/// `ENOSYS` (-38) proc falls through to its own fallback.
pub fn register_extra(f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> i64) {
    CHAIN_HANDLER.store(f as usize, Ordering::Release);
}

/// Register the VFS loader `execve()` uses to read a file's contents.
///
/// `f(path, buf)` must resolve `path` and copy up to `buf.len()` bytes
/// starting at offset 0 into `buf`, returning the number of bytes copied
/// (`>= 0`), `-2` (`ENOENT`) if `path` doesn't resolve, or another negative
/// errno on a read error. Called by kernel-fs's `init_fs()`.
pub fn register_exec_loader(f: fn(&str, &mut [u8]) -> i64) {
    EXEC_LOADER.store(f as usize, Ordering::Release);
}

/// Load `path`'s contents into `buf` via the loader kernel-fs registered.
/// Returns `-2` (`ENOENT`) if no loader has been registered yet (i.e. the
/// VFS isn't up), or whatever the registered loader itself returns.
pub fn load_exec_file(path: &str, buf: &mut [u8]) -> i64 {
    let f = EXEC_LOADER.load(Ordering::Acquire);
    if f == 0 {
        return ENOENT;
    }
    // SAFETY: EXEC_LOADER only ever stores a valid `fn(&str, &mut [u8]) -> i64`
    // set via register_exec_loader.
    let f: fn(&str, &mut [u8]) -> i64 = unsafe { core::mem::transmute(f) };
    f(path, buf)
}

/// Store the direct-map VA of /dev/serial so Process::create can pre-open
/// fd 0/1/2 for new processes.
pub fn set_serial_vnode(ptr: usize) {
    SERIAL_VNODE_PTR.store(ptr, Ordering::Release);
}

/// Page-cache resolver registered by kernel-fs's `pagecache` module.
///
/// kernel-proc's page-fault handler (`AddressSpace::resolve_file_fault`)
/// needs to demand-fill and mark-dirty file-backed VMA pages, but the
/// page cache itself lives in kernel-fs (it operates on the concrete
/// `VNode`/`VNodeOps` types kernel-fs owns) — and kernel-fs already
/// depends on kernel-proc, so the dependency can't run the other way.
/// This mirrors `EXEC_LOADER`/`CHAIN_HANDLER`/`SERIAL_VNODE_PTR`'s
/// existing registration-hook pattern exactly: kernel-fs's `init_fs()`
/// fills these in once, kernel-proc calls through the stored fn pointers
/// with an opaque `vnode_ptr: usize` (the same convention `FdEntry`
/// already uses) rather than a typed `*mut VNode`.
static PAGE_CACHE_GET_OR_FILL: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_MARK_DIRTY: AtomicUsize = AtomicUsize::new(0);
static PAGE_CACHE_WRITEBACK: AtomicUsize = AtomicUsize::new(0);

/// `VNode::inc_ref`/`dec_ref`, registered for the same reason as the
/// page-cache hooks above — `AddressSpace::mmap`/`munmap` need to manage a
/// file-backed VMA's vnode refcount without a typed VNode pointer.
static VNODE_INC_REF: AtomicUsize = AtomicUsize::new(0);
static VNODE_DEC_REF: AtomicUsize = AtomicUsize::new(0);
/// The full close-time hook: runs the VNode's `ops.release` (which for a
/// pipe end decrements reader/writer liveness AND wakes the opposite end),
/// not just a bare refcount decrement. `Process::exit` calls this for every
/// fd the dying process still holds open — without it, a pipeline stage
/// that exits without explicitly closing its stdout pipe end would leave
/// the write-side refcount high forever and the downstream reader would
/// never see EOF.
static VNODE_RELEASE: AtomicUsize = AtomicUsize::new(0);

/// Register the page cache's `get_or_fill`/`mark_dirty`/`writeback_vnode`
/// functions. Called once by kernel-fs's `init_fs()`.
///
/// `get_or_fill(vnode_ptr, page_aligned_file_offset) -> Option<frame>`
/// demand-fills (or returns an already-cached) physical frame number,
/// `None` only on an unrecoverable read error with no cache room to fall
/// back on. `mark_dirty(vnode_ptr, page_aligned_file_offset)` flags a
/// cached page dirty. `writeback_vnode(vnode_ptr) -> errno` flushes every
/// dirty page belonging to that vnode.
pub fn register_page_cache_hooks(
    get_or_fill: unsafe fn(usize, u64) -> Option<u64>,
    mark_dirty: fn(usize, u64),
    writeback: unsafe fn(usize) -> i32,
) {
    PAGE_CACHE_GET_OR_FILL.store(get_or_fill as usize, Ordering::Release);
    PAGE_CACHE_MARK_DIRTY.store(mark_dirty as usize, Ordering::Release);
    PAGE_CACHE_WRITEBACK.store(writeback as usize, Ordering::Release);
}

/// Register `VNode::inc_ref`/`dec_ref` shims. Called once by kernel-fs's
/// `init_fs()`.
pub fn register_vnode_refcount_hooks(inc_ref: unsafe fn(usize), dec_ref: unsafe fn(usize)) {
    VNODE_INC_REF.store(inc_ref as usize, Ordering::Release);
    VNODE_DEC_REF.store(dec_ref as usize, Ordering::Release);
}

/// Register the full close-time release shim (see [`VNODE_RELEASE`]).
/// Called once by kernel-fs's `init_fs()`.
pub fn register_vnode_release_hook(release: unsafe fn(usize)) {
    VNODE_RELEASE.store(release as usize, Ordering::Release);
}

/// # Safety
/// `vnode_ptr` must be a valid direct-map VA of a live, refcounted VNode;
/// `file_offset` must already be page-aligned; the direct map must be live.
///
/// Only ever called from `AddressSpace::resolve_file_fault`, which is
/// `#[cfg(target_os = "none")]`-gated (see that same rationale on
/// `pagefault::cow_writable_flags` for why a host-lib-dependency build has
/// no reachable call site, and why this is `-D warnings`-safe anyway once
/// this crate is itself the test target: see
/// `page_cache_and_vnode_refcount_hooks_round_trip` below).
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub(crate) unsafe fn page_cache_get_or_fill(vnode_ptr: usize, file_offset: u64) -> Option<u64> {
    let f = PAGE_CACHE_GET_OR_FILL.load(Ordering::Acquire);
    if f == 0 {
        return None;
    }
    // SAFETY: only ever stores a fn matching this signature, set via
    // register_page_cache_hooks.
    let f: unsafe fn(usize, u64) -> Option<u64> = unsafe { core::mem::transmute(f) };
    // SAFETY: forwards this function's own preconditions.
    unsafe { f(vnode_ptr, file_offset) }
}

#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub(crate) fn page_cache_mark_dirty(vnode_ptr: usize, file_offset: u64) {
    let f = PAGE_CACHE_MARK_DIRTY.load(Ordering::Acquire);
    if f == 0 {
        return;
    }
    // SAFETY: only ever stores a fn matching this signature.
    let f: fn(usize, u64) = unsafe { core::mem::transmute(f) };
    f(vnode_ptr, file_offset);
}

/// # Safety
/// `vnode_ptr` must be a valid direct-map VA of a live VNode; direct map live.
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub(crate) unsafe fn page_cache_writeback(vnode_ptr: usize) -> i32 {
    let f = PAGE_CACHE_WRITEBACK.load(Ordering::Acquire);
    if f == 0 {
        return 0; // nothing registered yet (VFS not up) => nothing to flush
    }
    // SAFETY: only ever stores a fn matching this signature.
    let f: unsafe fn(usize) -> i32 = unsafe { core::mem::transmute(f) };
    // SAFETY: forwards this function's own preconditions.
    unsafe { f(vnode_ptr) }
}

/// # Safety: `vnode_ptr` must be a valid direct-map VA of a live VNode.
pub(crate) unsafe fn vnode_inc_ref(vnode_ptr: usize) {
    let f = VNODE_INC_REF.load(Ordering::Acquire);
    if f == 0 {
        return;
    }
    // SAFETY: only ever stores a fn matching this signature.
    let f: unsafe fn(usize) = unsafe { core::mem::transmute(f) };
    // SAFETY: forwards this function's own precondition.
    unsafe { f(vnode_ptr) };
}

/// # Safety: `vnode_ptr` must be a valid direct-map VA of a live VNode.
pub(crate) unsafe fn vnode_dec_ref(vnode_ptr: usize) {
    let f = VNODE_DEC_REF.load(Ordering::Acquire);
    if f == 0 {
        return;
    }
    // SAFETY: only ever stores a fn matching this signature.
    let f: unsafe fn(usize) = unsafe { core::mem::transmute(f) };
    // SAFETY: forwards this function's own precondition.
    unsafe { f(vnode_ptr) };
}

/// Run `vnode_ptr`'s full `ops.release` hook — the same thing `sys_close`
/// does after removing an fd slot. See [`VNODE_RELEASE`] for why exit needs
/// this rather than a bare `vnode_dec_ref`.
///
/// # Safety: `vnode_ptr` must be a valid direct-map VA of a live VNode.
pub(crate) unsafe fn vnode_release(vnode_ptr: usize) {
    let f = VNODE_RELEASE.load(Ordering::Acquire);
    if f == 0 {
        return;
    }
    // SAFETY: only ever stores a fn matching this signature.
    let f: unsafe fn(usize) = unsafe { core::mem::transmute(f) };
    // SAFETY: forwards this function's own precondition.
    unsafe { f(vnode_ptr) };
}

// ─── main dispatch ────────────────────────────────────────────────────────

pub extern "C" fn proc_syscall_handler(
    nr: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> i64 {
    // Proc-owned syscalls: never delegated.
    match nr {
        SYS_EXIT => return sys_exit(a1 as i32),
        SYS_GETPID => return sys_getpid(),
        SYS_EXECVE => return sys_execve(a1, a2, a3),
        crate::fork::SYS_FORK => return crate::fork::sys_fork(),
        crate::clone::SYS_CLONE => return crate::clone::sys_clone(a1 as u32, a2),
        crate::signal::SYS_SIGACTION => return sys_sigaction(a1 as u32, a2, a3),
        crate::signal::SYS_KILL => return sys_kill(a1 as u32, a2 as u32),
        crate::signal::SYS_SIGRETURN => return sys_sigreturn(),
        SYS_PRCTL => return sys_prctl(a1, a2),
        SYS_DUP => return sys_dup(a1 as i32),
        SYS_DUP2 => return sys_dup2(a1 as i32, a2 as i32),
        crate::wait::SYS_WAIT4 => {
            return crate::wait::sys_wait4(a1 as i64, a2, a3 as u32, a4);
        }
        crate::vma::SYS_MMAP => {
            // The SYSCALL trampoline now carries a genuine 6th argument
            // (see kernel-cpu's syscall_entry global_asm! + a6 threaded
            // all the way through syscall_dispatch), so fd/offset arrive
            // natively as a5/a6 — no more packing `(offset << 32) | fd`
            // into a single register the way this arm used to.
            //
            // fd is an `int` in the Linux ABI: userland legitimately sets
            // it with a 32-bit mov (`mov r8d, -1`), which zero-extends to
            // 0xFFFF_FFFF in r8 — so sign-extend from the LOW 32 BITS, not
            // the full register. `a5 as i64` (the pre-2026-07-10 code) saw
            // 4294967295 instead of -1 and EINVAL'd every anonymous
            // mapping on the first real boot.
            let fd = a5 as u32 as i32 as i64;
            return crate::vma::sys_mmap(a1, a2, a3 as u32, a4 as u32, fd, a6);
        }
        crate::vma::SYS_MUNMAP => return crate::vma::sys_munmap(a1, a2),
        _ => {}
    }

    // Try chain handler (VFS layer).
    let chain = CHAIN_HANDLER.load(Ordering::Acquire);
    if chain != 0 {
        // SAFETY: CHAIN_HANDLER stores a valid fn pointer set via register_extra.
        let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> i64 =
            unsafe { core::mem::transmute(chain) };
        let r = f(nr, a1, a2, a3, a4, a5, a6);
        if r != ENOSYS {
            return r;
        }
    }

    // Fallback for syscalls proc handles directly when VFS isn't ready.
    match nr {
        SYS_WRITE => sys_write_direct(a1 as i32, a2 as *const u8, a3 as usize),
        _ => {
            let _ = (a4, a5, a6);
            ENOSYS
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

pub fn is_user_ptr(addr: usize, len: usize) -> bool {
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    addr >= USER_MIN && end <= USER_TOP
}

pub fn register_process(process: *mut Process) {
    let value = process as usize;
    for slot in &PROCESS_TABLE {
        if slot
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
    }
    panic!("process table full");
}

/// Remove `process` from the flat thread-keyed table. MUST be called by
/// `reap_zombie` before the `Process` frame (and its thread) are freed —
/// otherwise `current_process`/`process_for_thread` would keep scanning a
/// dangling pointer and dereference freed memory.
pub fn unregister_process(process: *mut Process) {
    let value = process as usize;
    for slot in &PROCESS_TABLE {
        let _ = slot.compare_exchange(value, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

pub fn current_process() -> *mut Process {
    process_for_thread(kernel_sched::current_thread())
}

/// Find the `Process` whose kernel thread is `thread`, or null if `thread`
/// is a pure kernel thread (idle, `kidle`) with no PCB. The same table scan
/// `current_process` has always done, factored out so the scheduler's
/// context-switch hook (`crate::process::sched_context_switch_hook`) can
/// resolve an arbitrary, not-yet-current thread.
pub(crate) fn process_for_thread(thread: *mut kernel_sched::Thread) -> *mut Process {
    if thread.is_null() {
        return core::ptr::null_mut();
    }
    for slot in &PROCESS_TABLE {
        let proc = slot.load(Ordering::Acquire) as *mut Process;
        if proc.is_null() {
            continue;
        }
        // SAFETY: non-null entries in PROCESS_TABLE are live Process pointers.
        if unsafe { (*proc).thread == thread } {
            return proc;
        }
    }
    core::ptr::null_mut()
}

pub fn set_current_process(_cpu_id: u32, _process: *mut Process) {}

// ─── direct-serial fallback (used before VFS is ready) ───────────────────

fn sys_write_direct(fd: i32, buf: *const u8, len: usize) -> i64 {
    if fd != 1 && fd != 2 {
        return ENOSYS;
    }
    if !is_user_ptr(buf as usize, len) {
        return EFAULT;
    }
    #[cfg(target_os = "none")]
    unsafe {
        // SAFETY: range validated above; STAC allows SMAP-guarded user reads.
        core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        for i in 0..len {
            serial_write_byte(*buf.add(i));
        }
        core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    let _ = buf;
    len as i64
}

fn sys_exit(code: i32) -> i64 {
    let p = current_process();
    if !p.is_null() {
        // SAFETY: current_process() returned a live PCB for this thread.
        unsafe {
            (*p).exit(code);
        }
    }
    unreachable!("sys_exit returned")
}

fn sys_getpid() -> i64 {
    let p = current_process();
    if p.is_null() {
        return EFAULT;
    }
    unsafe { (*p).pid as i64 }
}

/// `prctl(option, arg2, ...)`. Only `PR_GET_NAME` is supported: it copies
/// the caller's fixed 16-byte process `name` field to the user buffer at
/// `arg2` (Linux copies up to 16 bytes including a NUL — this kernel's
/// `name` is exactly 16 bytes, zero-padded). Every other option returns
/// `EINVAL`.
///
/// This is also the first reader of `Process.name`, which until this
/// checkpoint was write-only — see docs/miscompile-audit.md: the
/// `name: parent.name` copy in `fork`/`clone` was silently shredded by the
/// rustc 1.97.0 aggregate-copy miscompile, invisible precisely because
/// nothing ever read it back. The `fork: child name intact` boot check
/// (initrd/init2.asm) exercises this syscall in a forked child to confirm
/// the flag fix restored a faithful copy.
fn sys_prctl(option: u64, arg2: u64) -> i64 {
    if option != PR_GET_NAME {
        return EINVAL;
    }
    if !is_user_ptr(arg2 as usize, 16) {
        return EFAULT;
    }
    let p = current_process();
    if p.is_null() {
        return EFAULT;
    }
    // Copy the name field out element-by-element into a local, then write
    // the local to user memory. Reading `(*p).name` wholesale would be an
    // aggregate array copy — safe under this checkpoint's soft-float flags,
    // but the element-wise read matches the defensive convention in
    // docs/miscompile-audit.md and cannot regress if the flags ever change.
    let mut name = [0u8; 16];
    // SAFETY: current_process returned this thread's live PCB; `name` is a
    // 16-byte field owned by it.
    unsafe {
        for (i, slot) in name.iter_mut().enumerate() {
            *slot = (*p).name[i];
        }
    }
    #[cfg(target_os = "none")]
    unsafe {
        // SAFETY: arg2 validated for 16 bytes above; STAC lifts SMAP for the
        // guarded user write, CLAC re-arms it immediately after.
        core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        core::ptr::copy_nonoverlapping(name.as_ptr(), arg2 as *mut u8, 16);
        core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    let _ = &name;
    0
}

// ─── dup / dup2 ───────────────────────────────────────────────────────────

/// `dup(oldfd)` — see [`FdTable::dup`]. Duplicates to the lowest free fd.
fn sys_dup(old_fd: i32) -> i64 {
    let proc = current_process();
    if proc.is_null() {
        return EFAULT;
    }
    // SAFETY: current_process returned this thread's live PCB.
    unsafe { (*proc).fd_table.lock().dup(old_fd) as i64 }
}

/// `dup2(oldfd, newfd)` — see [`FdTable::dup2`].
fn sys_dup2(old_fd: i32, new_fd: i32) -> i64 {
    let proc = current_process();
    if proc.is_null() {
        return EFAULT;
    }
    // SAFETY: current_process returned this thread's live PCB.
    unsafe { (*proc).fd_table.lock().dup2(old_fd, new_fd) as i64 }
}

// ─── signals ────────────────────────────────────────────────────────────────

const EINVAL: i64 = -22;
const ESRCH: i64 = -3;

/// User-space `sigaction` ABI: a single `u64` at `act_ptr`/`oldact_ptr`
/// holding the disposition — `0` = SIG_DFL (default), `1` = SIG_IGN
/// (ignore), any other value = the handler function's address. This is a
/// deliberately minimal stand-in for Linux's full `struct sigaction`
/// (no `sa_mask`, `sa_flags`, or `sa_restorer` — those are out of scope,
/// see the signal module's "Known limitations").
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

fn action_to_user(action: crate::signal::SigAction) -> u64 {
    match action {
        crate::signal::SigAction::Default => SIG_DFL,
        crate::signal::SigAction::Ignore => SIG_IGN,
        crate::signal::SigAction::Handler { user_fn } => user_fn,
    }
}

fn action_from_user(value: u64) -> crate::signal::SigAction {
    match value {
        SIG_DFL => crate::signal::SigAction::Default,
        SIG_IGN => crate::signal::SigAction::Ignore,
        user_fn => crate::signal::SigAction::Handler { user_fn },
    }
}

/// `sigaction(signum, act, oldact)` — install a handler and/or read the
/// current one. `signum` may not be SIGKILL (which cannot be caught) — the
/// pure-argument check runs FIRST, before `current_process`, so the EINVAL
/// path is host-testable (same convention as `sys_mmap`/`sys_clone`).
fn sys_sigaction(signum: u32, act_ptr: u64, oldact_ptr: u64) -> i64 {
    if !crate::signal::signal_is_catchable(signum) {
        return EINVAL;
    }
    let p = current_process();
    if p.is_null() {
        return EFAULT;
    }

    // Read the current disposition under the lock, install the new one if
    // provided, all in one critical section.
    let old = {
        // SAFETY: current_process returned this thread's live PCB.
        let mut table = unsafe { (*p).sig_table.lock() };
        let old = table.actions[signum as usize];
        if act_ptr != 0 {
            if !is_user_ptr(act_ptr as usize, 8) {
                return EFAULT;
            }
            // SAFETY: just validated.
            let value = unsafe { read_u64_from_user(act_ptr) };
            table.actions[signum as usize] = action_from_user(value);
        }
        old
    };

    if oldact_ptr != 0 {
        if !is_user_ptr(oldact_ptr as usize, 8) {
            return EFAULT;
        }
        // SAFETY: just validated.
        unsafe { write_u64_to_user(oldact_ptr, action_to_user(old)) };
    }
    0
}

/// `kill(pid, signum)` — raise `signum` on the target process. The signal
/// is delivered at the target's next return to user mode (for a self-kill,
/// that's THIS syscall's own return — see
/// `crate::signal::deliver_on_syscall_return`). Cross-process delivery to a
/// process not currently mid-syscall waits until it next traps; forcibly
/// preempting it (an IPI-driven reschedule) is future work.
fn sys_kill(pid: u32, signum: u32) -> i64 {
    if signum == 0 || (signum as usize) >= crate::signal::NSIG {
        return EINVAL;
    }
    match crate::ptable::ptable_find(pid) {
        Some(target) => {
            // SAFETY: ptable only holds live Process pointers; raising a
            // pending bit only touches an atomic.
            unsafe { (*target).pending.raise(signum) };
            0
        }
        None => ESRCH,
    }
}

/// `sigreturn()` — restore the pre-signal user context saved during handler
/// delivery. Invoked by the sigreturn trampoline when a handler returns.
/// Never returns on the real path: it re-enters ring 3 at the interrupted
/// instruction with the interrupted syscall's original return value in RAX.
fn sys_sigreturn() -> i64 {
    let p = current_process();
    if p.is_null() {
        return EFAULT;
    }
    // SAFETY: current_process returned this thread's live PCB.
    let ctx = unsafe { (*p).sig_frame.lock().take() };
    let Some(ctx) = ctx else {
        // No handler in flight: a bogus sigreturn. Kill the process rather
        // than trusting a half-restored context.
        // SAFETY: p is this thread's live process.
        unsafe { (*p).exit(-(crate::signal::SIGKILL as i32)) };
        unreachable!("exit returned")
    };

    #[cfg(target_os = "none")]
    unsafe {
        // SAFETY: p's address space is this process's own, currently active.
        let cr3 = (*(*p).address_space).pml4_phys.as_u64() | (*(*p).address_space).pcid as u64;
        // Restore the FULL saved context (all GPRs incl. callee-saved rbx/
        // rbp/r12-r15) — not just rip/rsp/rflags/rax — so a syscall or
        // interrupt that a signal preempted resumes with its registers
        // intact (RCX/R11 excepted; sysretq clobbers them). `ctx` outlives
        // this call: it is a local on this kernel stack, read before the
        // asm switches away.
        // SAFETY: ctx is a valid SavedUserContext with user rip/rsp valid
        // in this address space; noreturn.
        crate::process::enter_user_mode_restoring(&ctx as *const _, cr3)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = ctx;
        0
    }
}

// ─── execve ────────────────────────────────────────────────────────────────

/// Index of the first `\0` in `buf`, or `buf.len()` if there isn't one.
/// Factored out of the path/argv/envp extraction below purely so it's
/// independently unit-testable without needing real user memory.
fn nul_terminated_len(buf: &[u8]) -> usize {
    buf.iter().position(|&b| b == 0).unwrap_or(buf.len())
}

/// Copy `dst.len()` bytes from a *validated* user-space pointer into `dst`.
///
/// # Safety
/// Caller must have already checked `[src, src + dst.len())` via
/// [`is_user_ptr`].
unsafe fn copy_from_user(src: *const u8, dst: &mut [u8]) {
    #[cfg(target_os = "none")]
    unsafe {
        // SAFETY: range validated by caller; STAC allows the SMAP-guarded
        // user read that follows, CLAC re-arms the guard immediately after.
        core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len());
        core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    // SAFETY: same precondition as above; no SMAP on the host build.
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len());
    }
}

/// Read one `u64` from a *validated* user-space pointer.
///
/// # Safety
/// Caller must have already checked `[ptr, ptr + 8)` via [`is_user_ptr`].
unsafe fn read_u64_from_user(ptr: u64) -> u64 {
    let mut buf = [0u8; 8];
    // SAFETY: forwards the same precondition this function requires.
    unsafe { copy_from_user(ptr as *const u8, &mut buf) };
    u64::from_le_bytes(buf)
}

/// Write one `u64` to a *validated* user-space pointer.
///
/// # Safety
/// Caller must have already checked `[ptr, ptr + 8)` via [`is_user_ptr`],
/// and the current CR3 must be the target process's address space.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
unsafe fn write_u64_to_user(ptr: u64, value: u64) {
    let bytes = value.to_le_bytes();
    #[cfg(target_os = "none")]
    unsafe {
        // SAFETY: range validated by caller; STAC lifts SMAP for the
        // guarded user write, CLAC re-arms it immediately after.
        core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, 8);
        core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    // SAFETY: same precondition; no SMAP on host.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, 8);
    }
}

/// `execve(path, argv, envp)` — see `crate::exec` for the core algorithm.
///
/// This function's only job is the user/kernel boundary: validate and copy
/// `path`, `argv[]`, and `envp[]` out of user memory into fixed-size kernel
/// buffers, then hand them to [`crate::exec::do_execve`], which requires
/// `IF=0` across both of its phases (see its module docs) — already
/// guaranteed here, since every syscall handler in this kernel runs with
/// interrupts disabled (see the comment further down, by the call site).
fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> i64 {
    // 1. Copy the path string.
    if !is_user_ptr(path_ptr as usize, 1) {
        return EFAULT;
    }
    let mut path_buf = [0u8; 256];
    // SAFETY: path_ptr was just validated for 1 byte above; copy_from_user
    // itself never reads more than path_buf.len() bytes regardless, and the
    // VFS's own path-length limits bound how much of that the caller could
    // have legitimately populated.
    unsafe { copy_from_user(path_ptr as *const u8, &mut path_buf) };
    let path_len = nul_terminated_len(&path_buf);
    let path = match core::str::from_utf8(&path_buf[..path_len]) {
        Ok(s) => s,
        Err(_) => return ENOENT,
    };

    // 2. Copy argv[] — a NULL-terminated array of user pointers.
    let mut argv_strs: [[u8; 256]; crate::exec::MAX_ARGV] = [[0; 256]; crate::exec::MAX_ARGV];
    let mut argv_lens: [usize; crate::exec::MAX_ARGV] = [0; crate::exec::MAX_ARGV];
    let mut argv_refs: [&str; crate::exec::MAX_ARGV] = [""; crate::exec::MAX_ARGV];
    let mut argc = 0usize;
    if argv_ptr != 0 {
        let mut ptr = argv_ptr;
        while argc < crate::exec::MAX_ARGV {
            if !is_user_ptr(ptr as usize, 8) {
                return EFAULT;
            }
            // SAFETY: just validated.
            let arg_ptr = unsafe { read_u64_from_user(ptr) };
            if arg_ptr == 0 {
                break;
            }
            if !is_user_ptr(arg_ptr as usize, 1) {
                return EFAULT;
            }
            // SAFETY: just validated.
            unsafe { copy_from_user(arg_ptr as *const u8, &mut argv_strs[argc]) };
            argv_lens[argc] = nul_terminated_len(&argv_strs[argc]);
            argc += 1;
            ptr += 8;
        }
    }
    for i in 0..argc {
        argv_refs[i] = core::str::from_utf8(&argv_strs[i][..argv_lens[i]]).unwrap_or("");
    }

    // 3. Copy envp[] the same way.
    let mut envp_strs: [[u8; 256]; crate::exec::MAX_ENVP] = [[0; 256]; crate::exec::MAX_ENVP];
    let mut envp_lens: [usize; crate::exec::MAX_ENVP] = [0; crate::exec::MAX_ENVP];
    let mut envp_refs: [&str; crate::exec::MAX_ENVP] = [""; crate::exec::MAX_ENVP];
    let mut envc = 0usize;
    if envp_ptr != 0 {
        let mut ptr = envp_ptr;
        while envc < crate::exec::MAX_ENVP {
            if !is_user_ptr(ptr as usize, 8) {
                return EFAULT;
            }
            // SAFETY: just validated.
            let e_ptr = unsafe { read_u64_from_user(ptr) };
            if e_ptr == 0 {
                break;
            }
            if !is_user_ptr(e_ptr as usize, 1) {
                return EFAULT;
            }
            // SAFETY: just validated.
            unsafe { copy_from_user(e_ptr as *const u8, &mut envp_strs[envc]) };
            envp_lens[envc] = nul_terminated_len(&envp_strs[envc]);
            envc += 1;
            ptr += 8;
        }
    }
    for i in 0..envc {
        envp_refs[i] = core::str::from_utf8(&envp_strs[i][..envp_lens[i]]).unwrap_or("");
    }

    // do_execve requires IF=0 across both of its phases (see its module
    // docs). That's already guaranteed here without any extra cli: every
    // syscall handler in this kernel runs with IF=0, because IA32_FMASK is
    // set to RFLAGS_IF at syscall-MSR init time (see
    // kernel_cpu::syscall::init_syscall_msrs), so the SYSCALL instruction
    // itself clears IF on entry. Interrupts come back only via the eventual
    // `sysretq` restoring the user's original RFLAGS from R11 — which is
    // exactly what Phase 2 does directly (see `process::enter_user_mode`),
    // and exactly what happens naturally if do_execve returns here instead.
    // SAFETY: IF=0 is guaranteed by the syscall entry path described above.
    unsafe { crate::exec::do_execve(path, &argv_refs[..argc], &envp_refs[..envc]) as i64 }
}

// ─── COM1 serial helpers ──────────────────────────────────────────────────

/// Write one byte to COM1, spinning until the transmit holding register
/// is empty.
///
/// # Safety
/// Must run at a point where raw port I/O to COM1 (0x3f8) is permitted —
/// i.e. ring 0 with the UART initialized by [`serial_init`] (done in
/// [`register`]). Concurrent callers interleave bytes but cannot corrupt
/// state.
#[cfg(target_os = "none")]
pub unsafe fn serial_write_byte(b: u8) {
    unsafe {
        while (serial_in(0x3fd) & 0x20) == 0 {
            core::hint::spin_loop();
        }
        core::arch::asm!("out dx, al",
            in("dx") 0x3f8u16, in("al") b, options(nomem, nostack));
    }
}

#[cfg(target_os = "none")]
unsafe fn serial_init() {
    if SERIAL_READY.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe {
        serial_out(0x3f9, 0x00);
        serial_out(0x3fb, 0x80);
        serial_out(0x3f8, 0x03);
        serial_out(0x3f9, 0x00);
        serial_out(0x3fb, 0x03);
        serial_out(0x3fa, 0xc7);
        serial_out(0x3fc, 0x0b);
    }
}

#[cfg(target_os = "none")]
unsafe fn serial_out(port: u16, v: u8) {
    unsafe {
        core::arch::asm!("out dx, al",
        in("dx") port, in("al") v, options(nomem, nostack));
    }
}

/// Read one byte from a COM1-range I/O port.
///
/// # Safety
/// `port` must be a valid UART register port for this machine (0x3f8..=0x3ff);
/// ring 0 only.
#[cfg(target_os = "none")]
pub unsafe fn serial_in(port: u16) -> u8 {
    let v: u8;
    unsafe {
        core::arch::asm!("in al, dx",
        in("dx") port, lateout("al") v, options(nomem, nostack));
    }
    v
}

// ─── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn user_ptr_rejects_null_page() {
        assert!(!is_user_ptr(0, 1));
    }
    #[test]
    fn user_ptr_accepts_normal() {
        assert!(is_user_ptr(0x1000, 0x1000));
    }
    #[test]
    fn user_ptr_rejects_kernel_space() {
        assert!(!is_user_ptr(0xffff_8000_0000_0000, 1));
    }
    #[test]
    fn user_ptr_rejects_overflow() {
        assert!(!is_user_ptr(usize::MAX, 1));
    }

    #[test]
    fn sys_execve_number_is_59() {
        assert_eq!(SYS_EXECVE, 59);
    }

    // ── Part A: prctl(PR_GET_NAME) — the fork-name-copy read path ──────────

    #[test]
    fn prctl_syscall_numbers_match_linux() {
        assert_eq!(SYS_PRCTL, 157);
        assert_eq!(PR_GET_NAME, 16);
    }

    #[test]
    fn dup_syscall_numbers_match_linux() {
        assert_eq!(SYS_DUP, 32);
        assert_eq!(SYS_DUP2, 33);
    }

    #[test]
    fn sys_prctl_rejects_unknown_option() {
        // The option check runs BEFORE current_process(), so this EINVAL
        // path is host-safe (never reaches the GS-relative per-CPU read
        // that segfaults off real hardware — same convention as sys_mmap).
        assert_eq!(sys_prctl(0, 0x1000), EINVAL, "PR_SET_NAME/unknown must be EINVAL");
        assert_eq!(sys_prctl(9999, 0x1000), EINVAL);
    }

    #[test]
    fn sys_prctl_get_name_rejects_null_ptr() {
        // is_user_ptr(0, 16) is false → EFAULT, still before current_process.
        assert_eq!(sys_prctl(PR_GET_NAME, 0), EFAULT);
    }

    #[test]
    fn sys_prctl_get_name_rejects_kernel_ptr() {
        // A kernel-half destination fails is_user_ptr → EFAULT, before
        // current_process() is ever called.
        assert_eq!(sys_prctl(PR_GET_NAME, 0xffff_8000_0000_0000), EFAULT);
    }

    #[test]
    fn prctl_dispatch_arm_is_wired() {
        // Drive the real dispatcher: a bad prctl option must fold to EINVAL
        // through proc_syscall_handler's SYS_PRCTL arm, proving the arm
        // exists and forwards a1/a2 (host-safe: EINVAL returns before any
        // current_process() call).
        assert_eq!(
            proc_syscall_handler(SYS_PRCTL, 0xdead, 0x1000, 0, 0, 0, 0),
            EINVAL
        );
    }

    #[test]
    fn proc_syscall_numbers_include_vm_and_fork() {
        assert_eq!(crate::fork::SYS_FORK, 57);
        assert_eq!(crate::clone::SYS_CLONE, 56);
        assert_eq!(crate::vma::SYS_MMAP, 9);
        assert_eq!(crate::vma::SYS_MUNMAP, 11);
    }

    // The fd/offset packing scheme is gone (Part 1: the trampoline now
    // carries a genuine 6th argument), so these two tests replace the old
    // pack/unpack round-trip test: each drives proc_syscall_handler's
    // SYS_MMAP arm through a distinct, host-safe (no current_process()
    // call reached) EINVAL/EBADF path that only fires when a5/a6 landed
    // in sys_mmap's `fd`/`offset` parameters exactly as sent, proving the
    // dispatch arm's direct passthrough (`a5 as i64, a6`) is wired up
    // correctly without needing real hardware.
    #[test]
    fn sys_mmap_arm_threads_a5_into_fd() {
        // MAP_PRIVATE (no MAP_ANONYMOUS) with fd = -1 (from a5) must hit
        // sys_mmap's "non-anonymous mapping needs a real fd" EBADF check
        // -- a distinct errno from the generic EINVAL used everywhere
        // else, so this can only be explained by a5 arriving as fd == -1.
        const PROT_READ: u64 = 1;
        const MAP_PRIVATE: u64 = 0x02;
        let ret = proc_syscall_handler(
            crate::vma::SYS_MMAP,
            0,          // addr_hint
            4096,       // length
            PROT_READ,  // prot
            MAP_PRIVATE, // flags
            (-1i64) as u64, // a5 -> fd
            0,          // a6 -> offset
        );
        assert_eq!(ret, -9, "expected EBADF from fd=-1 on a non-anonymous mapping");
    }

    #[test]
    fn sys_mmap_arm_threads_a6_into_offset() {
        // MAP_PRIVATE|MAP_ANONYMOUS with a genuinely misaligned offset
        // (from a6) must hit sys_mmap's own offset-alignment EINVAL check,
        // which runs before the flags/fd checks -- only reachable if a6
        // arrived as the literal offset value, unshifted.
        const PROT_READ: u64 = 1;
        const MAP_PRIVATE: u64 = 0x02;
        const MAP_ANONYMOUS: u64 = 0x20;
        let ret = proc_syscall_handler(
            crate::vma::SYS_MMAP,
            0,                          // addr_hint
            4096,                       // length
            PROT_READ,                  // prot
            MAP_PRIVATE | MAP_ANONYMOUS, // flags
            (-1i64) as u64,             // a5 -> fd (valid for anonymous)
            4097,                       // a6 -> offset (misaligned)
        );
        assert_eq!(ret, -22, "expected EINVAL from a misaligned offset");
    }

    #[test]
    fn nul_terminated_len_finds_embedded_null() {
        assert_eq!(nul_terminated_len(b"hello\0world"), 5);
    }

    #[test]
    fn nul_terminated_len_without_null_returns_full_len() {
        let buf = [b'a'; 16];
        assert_eq!(nul_terminated_len(&buf), 16);
    }

    #[test]
    fn nul_terminated_len_empty_buf_is_zero() {
        assert_eq!(nul_terminated_len(&[]), 0);
    }

    #[test]
    fn exec_loader_registration_round_trip() {
        // EXEC_LOADER is a single process-wide static, so "unregistered" vs
        // "registered" must be observed within one sequential test rather
        // than as two independent #[test] functions — those can run in
        // parallel/in either order and would otherwise race on this state.
        let mut buf = [0u8; 4];
        assert_eq!(
            load_exec_file("/anything", &mut buf),
            ENOENT,
            "no loader registered yet"
        );

        fn fake_loader(path: &str, buf: &mut [u8]) -> i64 {
            if path != "/ok" {
                return -2;
            }
            buf[0] = 0x42;
            1
        }
        register_exec_loader(fake_loader);

        assert_eq!(load_exec_file("/ok", &mut buf), 1);
        assert_eq!(buf[0], 0x42);
        assert_eq!(load_exec_file("/missing", &mut buf), -2);
    }

    #[test]
    fn page_cache_and_vnode_refcount_hooks_round_trip() {
        // Same single-sequential-test rationale as exec_loader's own
        // round-trip test above: these are process-wide statics.
        static FILL_CALLS: AtomicUsize = AtomicUsize::new(0);
        static DIRTY_CALLS: AtomicUsize = AtomicUsize::new(0);
        static WRITEBACK_CALLS: AtomicUsize = AtomicUsize::new(0);
        static INC_REF_CALLS: AtomicUsize = AtomicUsize::new(0);
        static DEC_REF_CALLS: AtomicUsize = AtomicUsize::new(0);

        unsafe fn fake_get_or_fill(vnode_ptr: usize, file_offset: u64) -> Option<u64> {
            FILL_CALLS.fetch_add(1, Ordering::SeqCst);
            if vnode_ptr == 0 {
                return None;
            }
            Some(vnode_ptr as u64 + file_offset)
        }
        fn fake_mark_dirty(_vnode_ptr: usize, _file_offset: u64) {
            DIRTY_CALLS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe fn fake_writeback(_vnode_ptr: usize) -> i32 {
            WRITEBACK_CALLS.fetch_add(1, Ordering::SeqCst);
            0
        }
        unsafe fn fake_inc_ref(_vnode_ptr: usize) {
            INC_REF_CALLS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe fn fake_dec_ref(_vnode_ptr: usize) {
            DEC_REF_CALLS.fetch_add(1, Ordering::SeqCst);
        }

        register_page_cache_hooks(fake_get_or_fill, fake_mark_dirty, fake_writeback);
        register_vnode_refcount_hooks(fake_inc_ref, fake_dec_ref);

        // SAFETY: fake_get_or_fill has no real preconditions on vnode_ptr.
        let filled = unsafe { page_cache_get_or_fill(0x1000, 0x2000) };
        assert_eq!(filled, Some(0x3000));
        assert_eq!(FILL_CALLS.load(Ordering::SeqCst), 1);

        page_cache_mark_dirty(0x1000, 0);
        assert_eq!(DIRTY_CALLS.load(Ordering::SeqCst), 1);

        // SAFETY: fake_writeback has no real preconditions.
        assert_eq!(unsafe { page_cache_writeback(0x1000) }, 0);
        assert_eq!(WRITEBACK_CALLS.load(Ordering::SeqCst), 1);

        // SAFETY: fake_inc_ref/dec_ref have no real preconditions.
        unsafe {
            vnode_inc_ref(0x1000);
            vnode_dec_ref(0x1000);
        }
        assert_eq!(INC_REF_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(DEC_REF_CALLS.load(Ordering::SeqCst), 1);

        // SAFETY: fake_get_or_fill treats vnode_ptr==0 as "fail".
        assert_eq!(unsafe { page_cache_get_or_fill(0, 0) }, None);
    }
}
