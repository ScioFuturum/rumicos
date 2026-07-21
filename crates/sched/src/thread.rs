use crate::queue::{MLFQ_LEVELS, timeslice};
use core::sync::atomic::{AtomicU64, Ordering};

pub type ThreadId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ThreadState {
    Runnable,
    Running,
    Blocked,
    Dead,
}

// 32 KiB: sys_execve alone keeps ~16.5 KiB of argv/envp copy buffers on the
// kernel stack (see crate::syscall's MAX_ARGV/MAX_ENVP arrays), which
// overflowed the previous 16 KiB stack into the adjacent buddy frames on the
// first real QEMU boot — silent corruption, then a null-deref in execve.
pub const KSTACK_SIZE: usize = 32 * 1024;
pub const KSTACK_ORDER: u8 = 3;
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

const _: () = assert!(
    KSTACK_SIZE.is_multiple_of(16),
    "KSTACK_SIZE must be 16-byte aligned"
);

/// Per-thread XSAVE area size. 1 KiB — comfortably above the 576 bytes the
/// x87+SSE XSAVE image needs ([`kernel_arch_x86_64::xsave::XSAVE_X87_SSE_SIZE`])
/// and a multiple of 64, which XSAVE requires of the area's alignment.
pub const FPU_AREA_SIZE: usize = 1024;
const _: () = assert!(
    FPU_AREA_SIZE >= kernel_arch_x86_64::xsave::XSAVE_X87_SSE_SIZE,
    "FPU area must fit the x87+SSE XSAVE image"
);
const _: () = assert!(FPU_AREA_SIZE.is_multiple_of(64), "XSAVE area must be 64-byte aligned");

/// A 64-byte-aligned per-thread XSAVE save area.
///
/// XSAVE/XRSTOR require the area's ABSOLUTE address to be 64-byte aligned.
/// `Thread` is `repr(align(64))` and placed at a 64-aligned address, and this
/// field is itself `align(64)`, so its absolute address is always aligned.
#[repr(C, align(64))]
pub struct FpuArea {
    bytes: [u8; FPU_AREA_SIZE],
}

impl FpuArea {
    /// A freshly-initialized area. `XRSTOR` from it yields default FPU/SSE
    /// state: x87 reset, XMM registers zeroed, MXCSR = 0x1F80.
    ///
    /// The area is zeroed except the legacy MXCSR field (offset 24). XRSTOR
    /// loads MXCSR from the area even when the SSE component's XSTATE_BV bit
    /// is 0, so it must hold a valid value (0x1F80, all exceptions masked),
    /// not zero (which would unmask every SIMD exception). XSTATE_BV in the
    /// XSAVE header (offset 512) is left 0, so XRSTOR init-optimizes the x87
    /// and SSE components rather than loading them from the (empty) area.
    pub const fn new() -> Self {
        let mut bytes = [0u8; FPU_AREA_SIZE];
        bytes[24] = 0x80; // MXCSR low byte
        bytes[25] = 0x1f; // MXCSR high byte → 0x1F80
        Self { bytes }
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.bytes.as_ptr()
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }
}

impl Default for FpuArea {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(64))]
pub struct Thread {
    pub id: ThreadId,
    pub state: ThreadState,
    pub cpu_id: u32,
    pub priority: u8,
    pub ticks_used: u32,
    pub kstack_top: u64,
    pub kstack_phys: u64,
    pub context: Context,
    /// Extended (x87/SSE) state, saved/restored eagerly on every context
    /// switch (see `kernel_sched::schedule`). Last field so it cannot shift
    /// `context`'s offset, which `switch_context`'s asm depends on.
    pub fpu: FpuArea,
    _pad: [u8; 0],
}

impl Thread {
    /// Allocate a new kernel thread with a 16 KiB stack.
    ///
    /// # Safety
    /// `entry` must be a valid kernel function pointer that never returns.
    pub unsafe fn new_kernel(id: ThreadId, entry: fn() -> !, priority: u8) -> Self {
        let stack_phys = kernel_memory::alloc_order(KSTACK_ORDER);
        let stack_base = DIRECT_MAP_BASE + stack_phys.as_u64();
        let stack_end = stack_base + KSTACK_SIZE as u64;
        let tcb_top = stack_end - core::mem::size_of::<Thread>() as u64;
        let tcb_aligned = tcb_top & !((core::mem::align_of::<Thread>() as u64) - 1);
        let kstack_top = tcb_aligned & !0x0f;
        let priority = priority.min((MLFQ_LEVELS - 1) as u8);

        Self {
            id,
            state: ThreadState::Runnable,
            cpu_id: 0,
            priority,
            ticks_used: 0,
            kstack_top,
            kstack_phys: stack_phys.as_u64(),
            context: Context::new_kernel(entry, kstack_top),
            fpu: FpuArea::new(),
            _pad: [],
        }
    }

    #[inline(always)]
    pub fn is_runnable(&self) -> bool {
        self.state == ThreadState::Runnable
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Context {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub rip: u64,
    pub rsp: u64,
}

impl Context {
    pub const fn zero() -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: 0,
            rip: 0,
            rsp: 0,
        }
    }

    pub fn new_kernel(entry: fn() -> !, stack_top: u64) -> Self {
        // Invariant: stack_top must be 16-byte aligned so that stack_top - 8
        // satisfies the SysV AMD64 ABI at function entry.
        debug_assert!(
            stack_top.is_multiple_of(16),
            "stack_top {:#x} is not 16-byte aligned",
            stack_top
        );
        Self {
            rip: entry as *const () as usize as u64,
            rsp: stack_top - 8,
            ..Self::zero()
        }
    }

    pub fn new_raw(entry: unsafe extern "C" fn() -> !, stack_top: u64) -> Self {
        debug_assert!(
            stack_top.is_multiple_of(16),
            "stack_top {:#x} is not 16-byte aligned",
            stack_top
        );
        Self {
            rip: entry as usize as u64,
            rsp: stack_top - 8,
            ..Self::zero()
        }
    }
}

static NEXT_TID: AtomicU64 = AtomicU64::new(1);

#[inline(always)]
pub fn alloc_tid() -> ThreadId {
    NEXT_TID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) unsafe fn alloc_thread(id: ThreadId, entry: fn() -> !, priority: u8) -> *mut Thread {
    // SAFETY: caller supplies a valid non-returning entry point.
    let thread = unsafe { Thread::new_kernel(id, entry, priority) };
    let ptr = thread.kstack_top as *mut Thread;
    // SAFETY: `kstack_top` reserves aligned space for the TCB at the high end
    // of the freshly allocated stack and is not aliased yet.
    unsafe { ptr.write(thread) };
    ptr
}

/// Allocate a kernel thread whose entry point uses the C ABI.
///
/// # Safety
/// `entry` must be a valid non-returning kernel function. The returned TCB
/// pointer must be enqueued at most once and must remain owned by the scheduler.
pub unsafe fn alloc_thread_raw(
    id: ThreadId,
    entry: unsafe extern "C" fn() -> !,
    priority: u8,
) -> *mut Thread {
    // SAFETY: caller supplies a valid non-returning entry point.
    let mut thread = unsafe { Thread::new_kernel(id, crate::idle_loop, priority) };
    thread.context = Context::new_raw(entry, thread.kstack_top);
    let ptr = thread.kstack_top as *mut Thread;
    // SAFETY: `kstack_top` reserves aligned space for the TCB in the fresh stack.
    unsafe { ptr.write(thread) };
    ptr
}

#[inline(always)]
pub(crate) fn charge_tick(thread: &mut Thread) -> bool {
    thread.ticks_used = thread.ticks_used.saturating_add(1);
    if thread.ticks_used < timeslice(thread.priority as usize) {
        return false;
    }

    thread.ticks_used = 0;
    if thread.priority < (MLFQ_LEVELS - 1) as u8 {
        thread.priority += 1;
    }
    true
}

#[cfg(test)]
pub(crate) fn test_thread(id: ThreadId, priority: u8, state: ThreadState) -> Thread {
    Thread {
        id,
        state,
        cpu_id: 0,
        priority,
        ticks_used: 0,
        kstack_top: 0,
        kstack_phys: 0,
        context: Context::zero(),
        fpu: FpuArea::new(),
        _pad: [],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> ! {
        panic!("test entry should not run")
    }

    #[test]
    fn context_new_kernel_sets_entry_and_aligned_stack() {
        let ctx = Context::new_kernel(never, 0x8000);
        assert_eq!(ctx.rip, never as *const () as usize as u64);
        assert_eq!(ctx.rsp, 0x7ff8);
    }

    #[test]
    fn context_rsp_alignment() {
        let ctx = Context::new_kernel(never, 4096);
        assert_eq!(ctx.rsp % 16, 8, "RSP should be 16n-8 for SysV ABI");
        assert_eq!(ctx.rip, never as *const () as usize as u64);
    }

    #[test]
    fn kstack_size_aligned() {
        assert!(KSTACK_SIZE.is_multiple_of(16));
    }

    #[test]
    fn thread_demotes_after_timeslice() {
        let mut thread = test_thread(1, 1, ThreadState::Running);
        assert!(!charge_tick(&mut thread));
        assert!(charge_tick(&mut thread));
        assert_eq!(thread.priority, 2);
        assert_eq!(thread.ticks_used, 0);
    }

    #[test]
    fn thread_is_runnable_checks_state() {
        let runnable = test_thread(1, 0, ThreadState::Runnable);
        let running = test_thread(2, 0, ThreadState::Running);
        assert!(runnable.is_runnable());
        assert!(!running.is_runnable());
    }
}
