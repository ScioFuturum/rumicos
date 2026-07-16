#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod buddy;
pub mod constants;
pub mod pcid;
pub mod slab;
pub mod tagged_stack;

pub use buddy::{BumpAllocator, FrameAllocator, NumaBuddy, PhysAddr};
pub use pcid::{Pcid, PcidAllocator};
pub use slab::PerCpuSlab;
pub use tagged_stack::{AtomicTaggedStack, StackNode};

use kernel_sync::SpinLock;

/// Limine memory map entry (copied to avoid dependency)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LimineMemmapEntry {
    pub base: u64,
    pub length: u64,
    pub mem_type: u32,
}

static NUMA_BUDDY: SpinLock<NumaBuddy> = SpinLock::new(NumaBuddy::new_uninit());

// ─── allocation-tracking bitmap (double-alloc / bad-free detector) ────────
//
// One bit per 4 KiB frame for the first 2 GiB of physical memory (64 KiB of
// .bss). Every path through this module's public alloc/free API sets or
// clears bits and PANICS the moment a frame is handed out twice or freed
// while not allocated — the two failure modes that turn into wild memory
// corruption long after the fact. Frames above 2 GiB are not tracked.
const TRACK_LIMIT: u64 = 2 << 30;
const TRACK_WORDS: usize = (TRACK_LIMIT as usize / 4096) / 64;
static ALLOC_BITMAP: [core::sync::atomic::AtomicU64; TRACK_WORDS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; TRACK_WORDS];

fn track_alloc(frame: PhysAddr, order: u8) {
    use core::sync::atomic::Ordering;
    let base = frame.as_u64();
    for i in 0..(1u64 << order) {
        let addr = base + i * 4096;
        if addr >= TRACK_LIMIT {
            continue;
        }
        let bit = addr / 4096;
        let prev = ALLOC_BITMAP[(bit / 64) as usize].fetch_or(1 << (bit % 64), Ordering::AcqRel);
        assert!(
            prev & (1 << (bit % 64)) == 0,
            "frame allocator handed out {:#x} twice (order-{} alloc at base {:#x})",
            addr,
            order,
            base
        );
    }
}

fn track_free(frame: PhysAddr) {
    use core::sync::atomic::Ordering;
    let addr = frame.as_u64();
    if addr >= TRACK_LIMIT {
        return;
    }
    let bit = addr / 4096;
    let prev = ALLOC_BITMAP[(bit / 64) as usize].fetch_and(!(1 << (bit % 64)), Ordering::AcqRel);
    assert!(
        prev & (1 << (bit % 64)) != 0,
        "free_frame({:#x}): frame was not allocated (double free or bogus address)",
        addr
    );
}

/// Get access to the global NUMA_BUDDY (for setting as global allocator)
pub fn numa_buddy() -> &'static SpinLock<NumaBuddy> {
    &NUMA_BUDDY
}

/// Called once after kernel_paging::init().
///
/// # Safety
/// The direct-map must be live and the supplied physical ranges must describe
/// memory that should not be returned by the frame allocator.
pub unsafe fn init_frame_allocator(
    memmap: &[LimineMemmapEntry],
    bump_consumed: (PhysAddr, PhysAddr),
    kernel_phys_range: (PhysAddr, PhysAddr),
    extra_reserved: &[(PhysAddr, PhysAddr)],
) {
    let mut buddy = NUMA_BUDDY.lock();
    unsafe {
        buddy.init(
            memmap,
            bump_consumed,
            kernel_phys_range,
            extra_reserved,
            0xFFFF_8000_0000_0000_usize,
        )
    };
}

/// Allocate one 4 KiB frame from the global buddy allocator.
/// Panics on OOM.
pub fn alloc_frame() -> PhysAddr {
    let frame = NUMA_BUDDY
        .lock()
        .alloc_frame()
        .expect("OOM: no free frames");
    track_alloc(frame, 0);
    frame
}

/// Allocate one 4 KiB frame with physical address below 4 GiB.
/// Panics on OOM.
pub fn alloc_frame_below_4g() -> PhysAddr {
    let frame = NUMA_BUDDY
        .lock()
        .alloc_frame_below_4g()
        .expect("OOM: no free frames below 4 GiB");
    track_alloc(frame, 0);
    frame
}

/// Free a frame through the global NUMA buddy allocator.
///
/// # Safety
/// Caller must ensure the frame is no longer mapped or otherwise in use.
pub unsafe fn free_frame(frame: PhysAddr) {
    track_free(frame);
    unsafe { NUMA_BUDDY.lock().free_frame(frame) };
}

pub fn alloc_order(order: u8) -> PhysAddr {
    let frame = NUMA_BUDDY
        .lock()
        .alloc_order(order)
        .expect("OOM: no free frames at order");
    track_alloc(frame, order);
    frame
}
