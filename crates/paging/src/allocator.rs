use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};
use kernel_memory::FrameAllocator;
/// Early-boot page frame allocator
///
/// Provides a simple bump allocator backed by a static array of 4 KiB frames.
/// Once the main NumaBuddy allocator is online, this should be discarded.
/// Thread-safe via SpinLock (for potential use in MP boot code).
use kernel_memory::PhysAddr;
use kernel_sync::SpinLock;

const EARLY_FRAMES: usize = 128; // 512 KiB total, enough for more page tables on large RAM systems
const FRAME_SIZE: usize = 4096;

/// Direct map base used once the kernel's own page tables are live (matches
/// Limine's default HHDM base for 4-level paging, `kaslr: no`).
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

/// Frame storage (must be 4 KiB aligned)
#[repr(C, align(4096))]
#[derive(Copy, Clone)]
struct AllocFrame([u8; FRAME_SIZE]);

/// The pool frames are written through (page tables are built in them), so
/// the storage must live in a writable section. A plain `static [AllocFrame; N]`
/// lands in `.rodata`, and the very first `zero()` of a fresh table then
/// faults before any #PF handler exists — a silent triple fault on the first
/// real boot. `UnsafeCell` puts it in `.bss` and makes the mutation defined.
struct FramePool(UnsafeCell<[AllocFrame; EARLY_FRAMES]>);

// SAFETY: all access is serialized by GLOBAL_ALLOCATOR's SpinLock; each
// frame is handed out exactly once.
unsafe impl Sync for FramePool {}

/// Static frame pool
static FRAME_POOL: FramePool = FramePool(UnsafeCell::new([AllocFrame([0; FRAME_SIZE]); EARLY_FRAMES]));

/// `virt = phys + offset` for the kernel image (and thus for the frame pool,
/// which lives inside the image). Set once by `init::init()` before any
/// frame is handed out; 0 means "not set" (host tests), in which case
/// addresses pass through unchanged.
///
/// The pre-2026-07-10 code had no such translation at all: the bump
/// allocator returned kernel-image VIRTUAL addresses (0xffffffff8xxxxxxx)
/// as `PhysAddr`, which `PhysAddr::new`'s 52-bit mask silently mangled —
/// page-table entries and eventually CR3 pointed at garbage. Never caught
/// because the kernel had never been booted for real.
static KERNEL_IMAGE_VA_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Record `virt - phys` for the kernel image. Must be called before the
/// first bump allocation (see `init::init`).
pub fn set_kernel_image_va_offset(offset: u64) {
    KERNEL_IMAGE_VA_OFFSET.store(offset, Ordering::Release);
}

#[inline]
fn pool_base_va() -> u64 {
    FRAME_POOL.0.get() as u64
}

/// Virtual address through which a page-table frame at `phys` can be
/// written *right now*:
///   - pool frames (inside the kernel image) via the image mapping, which
///     both Limine's and the kernel's own page tables provide;
///   - anything else via the direct map (only valid once the kernel's own
///     tables are live — exactly when non-pool frames start appearing);
///   - identity when no offset is set (host unit tests).
pub(crate) fn boot_table_va(phys: u64) -> u64 {
    let offset = KERNEL_IMAGE_VA_OFFSET.load(Ordering::Acquire);
    if offset == 0 {
        return phys;
    }
    let va = phys.wrapping_add(offset);
    let pool_start = pool_base_va();
    let pool_end = pool_start + (EARLY_FRAMES * FRAME_SIZE) as u64;
    if va >= pool_start && va < pool_end {
        va
    } else {
        DIRECT_MAP_BASE + phys
    }
}

/// Bump allocator state
pub struct BumpAllocator<const N: usize> {
    next: usize,
}

impl<const N: usize> BumpAllocator<N> {
    /// Create a new bump allocator
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    /// Allocate one frame, panics if pool exhausted
    pub fn alloc_frame(&mut self) -> PhysAddr {
        if self.next >= N {
            panic!(
                "BumpAllocator: frame pool exhausted ({} >= {})",
                self.next, N
            );
        }

        // SAFETY: pointer arithmetic within the pool array; the frame is
        // handed out exactly once under GLOBAL_ALLOCATOR's lock.
        let frame_va = unsafe { (FRAME_POOL.0.get() as *mut AllocFrame).add(self.next) } as u64;
        self.next += 1;

        let offset = KERNEL_IMAGE_VA_OFFSET.load(Ordering::Acquire);
        PhysAddr::new(frame_va.wrapping_sub(offset))
    }

    /// Check if allocator is exhausted
    pub const fn is_exhausted(&self) -> bool {
        self.next >= N
    }

    /// Get count of allocated frames
    pub const fn allocated(&self) -> usize {
        self.next
    }

    /// Get the range of physical memory consumed by this allocator
    pub fn consumed_range(&self) -> (PhysAddr, PhysAddr) {
        let offset = KERNEL_IMAGE_VA_OFFSET.load(Ordering::Acquire);
        let base = pool_base_va();
        let start = base.wrapping_sub(offset);
        let end = (base + (self.next * FRAME_SIZE) as u64).wrapping_sub(offset);
        (PhysAddr::new(start), PhysAddr::new(end))
    }
}

impl<const N: usize> Default for BumpAllocator<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Implement FrameAllocator for BumpAllocator so existing code compiles unchanged.
unsafe impl<const N: usize> FrameAllocator for BumpAllocator<N> {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        Some(self.alloc_frame())
    }
    unsafe fn free_frame(&mut self, _: PhysAddr) {
        // bump can't free
    }
}

/// Type-erased allocator pointer for use in PageTableBuilder.
/// Stored as a raw pointer to avoid lifetime parameters on the builder.
#[derive(Clone)]
pub struct AllocatorRef(AllocatorRefKind);

#[derive(Clone)]
enum AllocatorRefKind {
    Raw(*mut dyn FrameAllocator),
    GlobalNumaBuddy,
}

unsafe impl Send for AllocatorRef {}

impl AllocatorRef {
    /// Create a type-erased allocator reference.
    ///
    /// # Safety
    /// `a` must remain valid and uniquely mutable for every cloned
    /// `AllocatorRef` derived from this value.
    pub unsafe fn new<A: FrameAllocator + 'static>(a: &mut A) -> Self {
        Self(AllocatorRefKind::Raw(
            a as *mut A as *mut dyn FrameAllocator,
        ))
    }
    unsafe fn from_dyn(a: &'static mut dyn FrameAllocator) -> Self {
        Self(AllocatorRefKind::Raw(a as *mut dyn FrameAllocator))
    }
    fn global_numa_buddy() -> Self {
        Self(AllocatorRefKind::GlobalNumaBuddy)
    }
    pub fn alloc(&mut self) -> Option<PhysAddr> {
        match self.0 {
            AllocatorRefKind::Raw(ptr) => {
                // SAFETY: We maintain the invariant that the pointer is valid.
                unsafe { (*ptr).alloc_frame() }
            }
            AllocatorRefKind::GlobalNumaBuddy => kernel_memory::numa_buddy().lock().alloc_frame(),
        }
    }
    /// Free a frame through this allocator reference.
    ///
    /// # Safety
    /// Caller must ensure `f` is no longer mapped or otherwise in use.
    pub unsafe fn free(&mut self, f: PhysAddr) {
        match self.0 {
            AllocatorRefKind::Raw(ptr) => {
                // SAFETY: Caller ensures frame is unmapped.
                unsafe { (*ptr).free_frame(f) }
            }
            AllocatorRefKind::GlobalNumaBuddy => {
                // SAFETY: Caller ensures frame is unmapped.
                unsafe { kernel_memory::numa_buddy().lock().free_frame(f) }
            }
        }
    }
}

/// Global allocator instance, protected by spinlock
static GLOBAL_ALLOCATOR: SpinLock<BumpAllocator<128>> = SpinLock::new(BumpAllocator::new());

/// Allocate a frame from the global pool
pub fn alloc_frame() -> PhysAddr {
    let mut alloc = GLOBAL_ALLOCATOR.lock();
    alloc.alloc_frame()
}

/// Get current allocation statistics
pub fn allocator_stats() -> (usize, usize) {
    let alloc = GLOBAL_ALLOCATOR.lock();
    (alloc.allocated(), EARLY_FRAMES)
}

/// Get the consumed range of the global allocator
pub fn bump_consumed_range() -> (PhysAddr, PhysAddr) {
    let alloc = GLOBAL_ALLOCATOR.lock();
    alloc.consumed_range()
}

/// Global frame allocator reference (set after NumaBuddy is online)
static GLOBAL_FRAME_ALLOCATOR: SpinLock<Option<AllocatorRef>> = SpinLock::new(None);

/// Set the global frame allocator used by PageTableBuilder when no local allocator is provided.
/// Must be called after init_frame_allocator().
pub fn set_global_frame_allocator(alloc: &'static mut dyn FrameAllocator) {
    // SAFETY: The caller provides a `'static` allocator that remains valid while
    // stored as the global frame allocator.
    let alloc_ref = unsafe { AllocatorRef::from_dyn(alloc) };
    *GLOBAL_FRAME_ALLOCATOR.lock() = Some(alloc_ref);
}

/// Use the kernel-memory global NumaBuddy as the frame allocator for new tables.
pub fn set_global_numa_buddy_allocator() {
    *GLOBAL_FRAME_ALLOCATOR.lock() = Some(AllocatorRef::global_numa_buddy());
}

/// Get the global frame allocator
pub fn get_global_frame_allocator() -> Option<AllocatorRef> {
    GLOBAL_FRAME_ALLOCATOR.lock().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bump_allocator_basic() {
        let mut alloc = BumpAllocator::<128>::new();

        let frame1 = alloc.alloc_frame();
        let frame2 = alloc.alloc_frame();

        // Frames should be different
        assert_ne!(frame1.as_u64(), frame2.as_u64());
        assert_eq!(alloc.allocated(), 2);
    }

    #[test]
    #[should_panic(expected = "frame pool exhausted")]
    fn test_bump_allocator_exhaustion() {
        let mut alloc = BumpAllocator::<128>::new();

        // Allocate all frames
        for _ in 0..EARLY_FRAMES {
            let _ = alloc.alloc_frame();
        }

        // This should panic
        let _ = alloc.alloc_frame();
    }

    #[test]
    fn test_bump_allocator_alignment() {
        let mut alloc = BumpAllocator::<128>::new();
        let frame = alloc.alloc_frame();

        // Frame should be 4 KiB aligned
        assert_eq!(frame.as_u64() & 0xfff, 0);
    }
}
