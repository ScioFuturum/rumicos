use core::sync::atomic::{AtomicU64, Ordering};

/// Simple physical address type
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysAddr(u64);

impl PhysAddr {
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl core::ops::Add<u64> for PhysAddr {
    type Output = PhysAddr;

    fn add(self, rhs: u64) -> PhysAddr {
        PhysAddr::new(self.0 + rhs)
    }
}

impl core::ops::Add<PhysAddr> for PhysAddr {
    type Output = PhysAddr;

    fn add(self, rhs: PhysAddr) -> PhysAddr {
        PhysAddr::new(self.0 + rhs.0)
    }
}

/// FrameAllocator trait for physical frame allocation.
///
/// # Safety
/// Implementors must return unique, page-aligned frames and must not hand out a
/// freed frame while any live mapping or owner can still access it.
pub unsafe trait FrameAllocator: Send {
    /// Allocate one 4 KiB physically contiguous frame.
    /// Returns None if OOM.
    fn alloc_frame(&mut self) -> Option<PhysAddr>;

    /// Free a frame previously returned by alloc_frame.
    /// # Safety
    /// Caller must ensure the frame is no longer mapped anywhere.
    unsafe fn free_frame(&mut self, frame: PhysAddr);

    /// Allocate `order` frames (2^order contiguous 4 KiB frames).
    /// Default implementation calls alloc_frame 2^order times — override for efficiency.
    fn alloc_order(&mut self, order: u8) -> Option<PhysAddr> {
        if order == 0 {
            return self.alloc_frame();
        }
        // default: allocate 2^order separate frames (may not be contiguous)
        // implementors SHOULD override this
        self.alloc_frame() // stub — real impl in NumaBuddy
    }
}

const MAX_ORDER: u8 = 10; // 2^10 pages = 4 MiB max contiguous block
const PAGE_SIZE: usize = 4096;
const MAX_NUMA_NODES: usize = 8;
const DIRECT_MAP_BASE_DEFAULT: usize = 0xFFFF_8000_0000_0000_usize;
const LOW_4G_LIMIT: u64 = 0x1_0000_0000;
const MAX_LOW_FRAME_TRIES: usize = 256;

#[derive(Clone, Copy)]
struct ExclusionSet<'a> {
    bump_consumed: (PhysAddr, PhysAddr),
    kernel_phys_range: (PhysAddr, PhysAddr),
    extra_reserved: &'a [(PhysAddr, PhysAddr)],
}

/// Free-list node format (stored IN the free physical frames themselves)
#[repr(C)]
struct FreeFrame {
    next_phys: u64,  // PhysAddr of next free frame at this order (0 = end)
    generation: u64, // ABA counter
}

/// Tagged pointer: [63:16] = phys frame number, [15:0] = generation
struct FreeList {
    head: AtomicU64,
}

impl FreeList {
    const fn new() -> Self {
        Self {
            head: AtomicU64::new(0),
        }
    }

    /// Highest plausible physical address (44 bits = 16 TiB). Anything
    /// past this in a free-list header or a freed frame is corruption —
    /// e.g. the 0xFFFF... pattern that sent the first real boot's
    /// allocator walking a non-canonical direct-map address into a #GP
    /// loop while holding the allocator lock.
    const MAX_PLAUSIBLE_PHYS: u64 = 1 << 44;

    /// Push a frame onto the free list
    unsafe fn push(&self, frame: PhysAddr, dmap: usize) {
        assert!(
            frame.as_u64() < Self::MAX_PLAUSIBLE_PHYS && frame.as_u64() & 0xFFF == 0,
            "buddy: bogus frame {:#x} pushed to free list",
            frame.as_u64()
        );
        loop {
            let current = self.head.load(Ordering::Acquire);
            let (head_frame, tag) = Self::unpack(current);

            // Write FreeFrame header into the frame. `next_phys` holds the
            // next frame's physical ADDRESS (as documented on FreeFrame) —
            // `head_frame` from unpack() is a frame NUMBER, so shift it
            // back. Storing the bare number (as this did before 2026-07-10)
            // made pop's `next_phys >> 12` double-shift the chain: every
            // pop past the head returned a bogus rounded-down frame, and
            // the allocator handed out aliased/overlapping memory.
            let frame_virt = (dmap as u64 + frame.as_u64()) as *mut FreeFrame;
            // SAFETY: direct-map ensures this is valid
            unsafe {
                (*frame_virt).next_phys = head_frame << 12;
                (*frame_virt).generation = tag;
            }

            let new_head = Self::pack(frame.as_u64() >> 12, tag.wrapping_add(1));
            if self
                .head
                .compare_exchange(current, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Pop a frame from the free list
    unsafe fn pop(&self, dmap: usize) -> Option<PhysAddr> {
        loop {
            let current = self.head.load(Ordering::Acquire);
            let (head_frame, tag) = Self::unpack(current);
            if head_frame == 0 {
                return None;
            }

            let frame = PhysAddr::new(head_frame << 12);
            assert!(
                frame.as_u64() < Self::MAX_PLAUSIBLE_PHYS,
                "buddy: corrupt free-list head {:#x} (frame {:#x})",
                current,
                frame.as_u64()
            );
            let frame_virt = (dmap as u64 + frame.as_u64()) as *mut FreeFrame;
            // SAFETY: direct-map ensures this is valid
            let next_phys = unsafe { (*frame_virt).next_phys };
            assert!(
                next_phys < Self::MAX_PLAUSIBLE_PHYS && next_phys & 0xFFF == 0,
                "buddy: free frame {:#x} header overwritten (next_phys {:#x}) - use-after-free of a freed frame or a double allocation",
                frame.as_u64(),
                next_phys
            );

            let new_head = Self::pack(next_phys >> 12, tag.wrapping_add(1));
            if self
                .head
                .compare_exchange(current, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(frame);
            }
        }
    }

    fn pack(frame_num: u64, tag: u64) -> u64 {
        (frame_num << 16) | (tag & 0xFFFF)
    }

    fn unpack(packed: u64) -> (u64, u64) {
        (packed >> 16, packed & 0xFFFF)
    }
}

/// One NUMA node's allocator. Cache-line padded to avoid false sharing.
#[repr(C, align(64))]
pub struct BuddyNode {
    free_lists: [FreeList; MAX_ORDER as usize + 1],
    base: PhysAddr,  // first frame of this node's managed memory
    size: u64,       // total frames managed
    free: AtomicU64, // current free frame count (for stats)
    node_id: u32,
    _pad: [u8; 20], // pad to 64 bytes
}

unsafe impl Send for BuddyNode {}
unsafe impl Sync for BuddyNode {}

impl BuddyNode {
    const fn new(node_id: u32) -> Self {
        Self {
            free_lists: [const { FreeList::new() }; MAX_ORDER as usize + 1],
            base: PhysAddr::new(0),
            size: 0,
            free: AtomicU64::new(0),
            node_id,
            _pad: [0; 20],
        }
    }

    pub fn alloc_order(&self, order: u8, dmap: usize) -> Option<PhysAddr> {
        if order > MAX_ORDER {
            return None;
        }

        // Find smallest available order >= requested order
        for source_order in order as usize..=MAX_ORDER as usize {
            // SAFETY: dmap is valid direct-map base
            if let Some(frame) = unsafe { self.free_lists[source_order].pop(dmap) } {
                // Split down to requested order
                for split_order in (order as usize..source_order).rev() {
                    let buddy = PhysAddr::new(frame.as_u64() + (1 << (split_order + 12)));
                    // SAFETY: buddy is within managed memory
                    unsafe { self.free_lists[split_order].push(buddy, dmap) };
                }
                self.free.fetch_sub(1 << order, Ordering::Relaxed);
                return Some(frame);
            }
        }
        None
    }

    /// Free a frame block of the specified buddy order.
    ///
    /// # Safety
    /// Caller must ensure the block was allocated from this node, is no longer
    /// mapped, and `dmap` is the valid direct-map base.
    pub unsafe fn free_order(&self, frame: PhysAddr, order: u8, dmap: usize) {
        if order > MAX_ORDER {
            return;
        }

        let mut current_order = order;
        let mut current_frame = frame;

        loop {
            // Calculate buddy address: XOR with 1 << (order + 12)
            let buddy_addr =
                PhysAddr::new(current_frame.as_u64() ^ (1u64 << (current_order as usize + 12)));

            // Attempt to pop buddy from free list at current order
            // If buddy is available, merge and try to merge again at next order
            let buddy_in_list = {
                // Try to find and remove buddy from free list
                // This is a simple check - in practice we'd need a more sophisticated lookup
                // For now, we'll push back since verifying buddy presence is complex
                false
            };

            if buddy_in_list && current_order < MAX_ORDER {
                // Buddy found and available for merging
                // Merged frame is the smaller address
                let merged_frame = if current_frame.as_u64() < buddy_addr.as_u64() {
                    current_frame
                } else {
                    buddy_addr
                };

                // Continue merging at next order
                current_frame = merged_frame;
                current_order += 1;
                // Don't increment free count since we're merging
            } else {
                // Can't merge, push to free list
                unsafe { self.free_lists[current_order as usize].push(current_frame, dmap) };
                self.free
                    .fetch_add(1u64 << current_order, Ordering::Relaxed);
                break;
            }
        }
    }
}

pub struct NumaBuddy {
    nodes: [Option<BuddyNode>; MAX_NUMA_NODES],
    node_count: usize,
    direct_map_base: usize,
}

impl NumaBuddy {
    pub const fn new_uninit() -> Self {
        Self {
            nodes: [const { None }; MAX_NUMA_NODES],
            node_count: 0,
            direct_map_base: DIRECT_MAP_BASE_DEFAULT,
        }
    }

    /// # Safety
    /// Must be called AFTER kernel_paging::init() has installed the direct-map.
    /// `memmap` must be the Limine memory map.
    /// `direct_map_base` = 0xFFFF_8000_0000_0000 as usize
    pub unsafe fn init(
        &mut self,
        memmap: &[crate::LimineMemmapEntry],
        bump_consumed: (PhysAddr, PhysAddr),
        kernel_phys_range: (PhysAddr, PhysAddr),
        extra_reserved: &[(PhysAddr, PhysAddr)],
        direct_map_base: usize,
    ) {
        self.direct_map_base = direct_map_base;
        let exclusions = ExclusionSet {
            bump_consumed,
            kernel_phys_range,
            extra_reserved,
        };
        // USABLE entries only. BOOTLOADER_RECLAIMABLE (type 5) must NOT be
        // freed here: the BSP is still running on the Limine-provided stack,
        // which lives in exactly that memory. Freeing it sprays FreeFrame
        // list headers across the live stack and hands its frames out to
        // subsequent allocations — the resulting stack corruption sent the
        // first real boot on a wild jump into no-execute memory. Reclaiming
        // type-5 memory is only legal after the kernel has moved off every
        // bootloader-owned structure (stack included), which this kernel
        // never does today.
        for entry in memmap.iter() {
            if entry.mem_type != 0 {
                // USABLE
                continue;
            }

            if entry.length == 0 {
                continue;
            }

            unsafe {
                self.add_region_excluding(
                    0,
                    entry.base,
                    entry.base + entry.length,
                    exclusions,
                    direct_map_base,
                )
            };
        }
    }

    unsafe fn add_region_excluding(
        &mut self,
        node_id: usize,
        start: u64,
        end: u64,
        exclusions: ExclusionSet<'_>,
        direct_map_base: usize,
    ) {
        if start >= end {
            return;
        }

        if let Some((reserved_start, reserved_end)) =
            first_exclusion_overlap(start, end, exclusions)
        {
            if start < reserved_start {
                unsafe {
                    self.add_region_excluding(
                        node_id,
                        start,
                        reserved_start,
                        exclusions,
                        direct_map_base,
                    )
                };
            }
            if reserved_end < end {
                unsafe {
                    self.add_region_excluding(
                        node_id,
                        reserved_end,
                        end,
                        exclusions,
                        direct_map_base,
                    )
                };
            }
            return;
        }

        let aligned_base = PhysAddr::new((start + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1));
        let aligned_end = PhysAddr::new(end & !(PAGE_SIZE as u64 - 1));
        if aligned_end <= aligned_base {
            return;
        }

        let frame_count = (aligned_end.as_u64() - aligned_base.as_u64()) / PAGE_SIZE as u64;
        unsafe { self.add_region(node_id, aligned_base, frame_count, direct_map_base) };
    }

    /// Allocate one 4 KiB frame with physical address below 4 GiB.
    ///
    /// Required for structures that must be accessible from 32-bit protected
    /// mode during AP trampoline bring-up.
    pub fn alloc_frame_below_4g(&mut self) -> Option<PhysAddr> {
        let mut rejected: [Option<PhysAddr>; MAX_LOW_FRAME_TRIES] = [None; MAX_LOW_FRAME_TRIES];
        let mut n_rejected = 0usize;
        let mut result = None;

        for _ in 0..MAX_LOW_FRAME_TRIES {
            match <Self as FrameAllocator>::alloc_frame(self) {
                None => break,
                Some(frame) if frame.as_u64() < LOW_4G_LIMIT => {
                    result = Some(frame);
                    break;
                }
                Some(frame) => {
                    rejected[n_rejected] = Some(frame);
                    n_rejected += 1;
                }
            }
        }

        for frame in rejected[..n_rejected].iter().flatten() {
            // SAFETY: rejected frames were just allocated above and were not
            // mapped or exposed to any caller.
            unsafe { <Self as FrameAllocator>::free_frame(self, *frame) };
        }

        result
    }

    /// Add a contiguous physical region to node's free lists.
    /// Splits into buddy-aligned blocks automatically.
    unsafe fn add_region(
        &mut self,
        node_id: usize,
        base: PhysAddr,
        frames: u64,
        direct_map_base: usize,
    ) {
        if self.nodes[node_id].is_none() {
            self.nodes[node_id] = Some(BuddyNode::new(node_id as u32));
            self.node_count += 1;
        }

        let node = self.nodes[node_id].as_mut().unwrap();
        if node.size == 0 || base < node.base {
            node.base = base;
        }
        node.size += frames;
        node.free.fetch_add(frames, Ordering::Relaxed);

        let mut paddr = base.as_u64();
        let end = paddr + frames * PAGE_SIZE as u64;

        while paddr < end {
            let remaining = (end - paddr) as usize;
            let mut order = MAX_ORDER as usize;
            while order > 0 {
                let size = 1usize << (order + 12);
                if size <= remaining && (paddr as usize).is_multiple_of(size) {
                    break;
                }
                order -= 1;
            }

            let frame = PhysAddr::new(paddr);
            unsafe { node.free_lists[order].push(frame, direct_map_base) };
            paddr += 1u64 << (order + 12);
        }
    }
}

fn first_exclusion_overlap(
    start: u64,
    end: u64,
    exclusions: ExclusionSet<'_>,
) -> Option<(u64, u64)> {
    if let Some(overlap) = exclusion_overlap(start, end, exclusions.bump_consumed) {
        return Some(overlap);
    }
    if let Some(overlap) = exclusion_overlap(start, end, exclusions.kernel_phys_range) {
        return Some(overlap);
    }
    for reserved in exclusions.extra_reserved {
        if let Some(overlap) = exclusion_overlap(start, end, *reserved) {
            return Some(overlap);
        }
    }
    None
}

fn exclusion_overlap(start: u64, end: u64, reserved: (PhysAddr, PhysAddr)) -> Option<(u64, u64)> {
    let reserved_start = reserved.0.as_u64().max(start);
    let reserved_end = reserved.1.as_u64().min(end);
    if reserved_start < reserved_end {
        Some((reserved_start, reserved_end))
    } else {
        None
    }
}

// FrameAllocator impl for NumaBuddy
unsafe impl FrameAllocator for NumaBuddy {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        // Try local NUMA node first (cpu_id → node_id via future SRAT), fall back to other nodes
        self.nodes[0].as_ref()?.alloc_order(0, self.direct_map_base)
    }

    unsafe fn free_frame(&mut self, frame: PhysAddr) {
        unsafe {
            self.nodes[0]
                .as_ref()
                .unwrap()
                .free_order(frame, 0, self.direct_map_base)
        };
    }

    fn alloc_order(&mut self, order: u8) -> Option<PhysAddr> {
        self.nodes[0]
            .as_ref()?
            .alloc_order(order, self.direct_map_base)
    }
}

/// Bump allocator for testing
pub struct BumpAllocator<const N: usize> {
    next: usize,
}

impl<const N: usize> BumpAllocator<N> {
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    pub fn alloc_frame(&mut self) -> Option<PhysAddr> {
        if self.next >= N {
            return None;
        }
        let addr = self.next as u64 * 4096;
        self.next += 1;
        Some(PhysAddr::new(addr))
    }

    /// Free is a no-op for this test bump allocator.
    ///
    /// # Safety
    /// Caller must ensure any logical ownership invariants are upheld; the
    /// frame will not be reused by this allocator.
    pub unsafe fn free_frame(&mut self, _frame: PhysAddr) {
        // no-op
    }

    pub fn consumed_range(&self) -> (PhysAddr, PhysAddr) {
        (PhysAddr::new(0), PhysAddr::new(self.next as u64 * 4096))
    }
}

impl<const N: usize> Default for BumpAllocator<N> {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl<const N: usize> FrameAllocator for BumpAllocator<N> {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        self.alloc_frame()
    }
    unsafe fn free_frame(&mut self, frame: PhysAddr) {
        unsafe { self.free_frame(frame) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr::NonNull;
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    struct TestMapping {
        ptr: NonNull<u8>,
        layout: Layout,
        direct_map_base: usize,
    }

    impl Drop for TestMapping {
        fn drop(&mut self) {
            // SAFETY: `ptr` was allocated with this exact `layout` in
            // `test_direct_map` and is dropped exactly once here.
            unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
        }
    }

    fn test_direct_map(size: usize, phys_base: u64) -> TestMapping {
        let layout = Layout::from_size_align(size, PAGE_SIZE).expect("valid test layout");
        // SAFETY: layout is non-zero and page-aligned; null is checked below.
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).expect("test backing allocation failed");
        let direct_map_base = (ptr.as_ptr() as usize).wrapping_sub(phys_base as usize);
        TestMapping {
            ptr,
            layout,
            direct_map_base,
        }
    }

    fn free_list_covers(
        list: &FreeList,
        order: usize,
        target: PhysAddr,
        direct_map_base: usize,
    ) -> bool {
        let (head_frame, _) = FreeList::unpack(list.head.load(Ordering::Acquire));
        let mut phys = head_frame << 12;
        let block_size = 1u64 << (order + 12);
        let mut visited = 0usize;

        while phys != 0 && visited < 4096 {
            if target.as_u64() >= phys && target.as_u64() < phys + block_size {
                return true;
            }
            let frame_virt = (direct_map_base as u64 + phys) as *const FreeFrame;
            // SAFETY: tests construct `direct_map_base` to point into backing
            // memory covering every frame reachable from this free list.
            phys = unsafe { (*frame_virt).next_phys };
            visited += 1;
        }

        false
    }

    #[test]
    fn test_bump_allocator_consumed_range() {
        let mut alloc = BumpAllocator::<128>::new();
        let initial = alloc.consumed_range();
        assert_eq!(initial.0.as_u64(), initial.1.as_u64()); // empty

        let _f1 = alloc.alloc_frame();
        let range = alloc.consumed_range();
        assert_eq!(range.1.as_u64() - range.0.as_u64(), 4096);
    }

    #[test]
    fn test_frame_allocator_blanket_impl() {
        let mut alloc = BumpAllocator::<128>::new();
        let frame = alloc.alloc_frame().unwrap();
        unsafe { alloc.free_frame(frame) };
        let frame2 = alloc.alloc_frame().unwrap();
        // May or may not be the same frame, depending on impl
        assert!(frame2.as_u64() >= frame.as_u64());
    }

    #[test]
    fn test_buddy_node_alloc_order_zero() {
        let node = BuddyNode::new(0);
        let base = PhysAddr::new(0x1000_0000);
        let mapping = test_direct_map(0x4000, base.as_u64());
        let direct_map_base = mapping.direct_map_base;

        // Manually initialize the node (since add_region is internal)
        // Push several free frames onto order-0 list
        unsafe {
            node.free_lists[0].push(base, direct_map_base);
            node.free_lists[0].push(base + 4096u64, direct_map_base);
            node.free_lists[0].push(base + 8192u64, direct_map_base);
        }

        // Allocate one frame
        let frame = node.alloc_order(0, direct_map_base);
        assert!(frame.is_some());
        let frame = frame.unwrap();
        assert_eq!(frame.as_u64() & 0xFFF, 0); // 4KiB aligned
    }

    #[test]
    fn test_buddy_node_alloc_order_split() {
        let node = BuddyNode::new(0);
        let base = PhysAddr::new(0x2000_0000);
        let mapping = test_direct_map(0x8000, base.as_u64());
        let direct_map_base = mapping.direct_map_base;

        // Push one order-2 frame (4 x 4KiB = 16KiB)
        unsafe {
            node.free_lists[2].push(base, direct_map_base);
        }

        // Allocate order-0 (should split from order-2)
        let frame1 = node.alloc_order(0, direct_map_base);
        assert!(frame1.is_some());
        assert_eq!(frame1.unwrap().as_u64(), base.as_u64());

        // Allocate another order-0 (should split remainder)
        let frame2 = node.alloc_order(0, direct_map_base);
        assert!(frame2.is_some());
        // Should be 4KiB after first frame
        assert_eq!(frame2.unwrap().as_u64(), base.as_u64() + 4096);
    }

    #[test]
    fn free_list_chain_pops_return_exact_pushed_frames() {
        let node = BuddyNode::new(0);
        let base = PhysAddr::new(0x4000_0000);
        let mapping = test_direct_map(0x4000, base.as_u64());
        let dmap = mapping.direct_map_base;

        unsafe {
            node.free_lists[0].push(base, dmap);
            node.free_lists[0].push(base + 4096u64, dmap);
            node.free_lists[0].push(base + 8192u64, dmap);
        }

        // LIFO: every pop must return exactly a pushed address, including
        // pops that FOLLOW the chain (not just the head). The
        // pre-2026-07-10 push stored a frame NUMBER in next_phys while pop
        // expected an ADDRESS, so only the first pop was correct and every
        // deeper link came back as a bogus rounded-down frame — none of
        // the older tests ever popped twice from one chain.
        let a = unsafe { node.free_lists[0].pop(dmap) }.expect("first pop");
        let b = unsafe { node.free_lists[0].pop(dmap) }.expect("second pop");
        let c = unsafe { node.free_lists[0].pop(dmap) }.expect("third pop");
        assert_eq!(a.as_u64(), base.as_u64() + 8192);
        assert_eq!(b.as_u64(), base.as_u64() + 4096);
        assert_eq!(c.as_u64(), base.as_u64());
        assert!(
            unsafe { node.free_lists[0].pop(dmap) }.is_none(),
            "list must be empty after popping all three"
        );
    }

    #[test]
    fn test_buddy_node_free_and_realloc() {
        let node = BuddyNode::new(0);
        let base = PhysAddr::new(0x3000_0000);
        let mapping = test_direct_map(0x4000, base.as_u64());
        let direct_map_base = mapping.direct_map_base;

        // Initialize with frames
        unsafe {
            node.free_lists[0].push(base, direct_map_base);
            node.free_lists[0].push(base + 4096u64, direct_map_base);
        }

        // Allocate and free
        let frame1 = node.alloc_order(0, direct_map_base).unwrap();
        unsafe { node.free_order(frame1, 0, direct_map_base) };

        // Should be able to allocate again
        let frame2 = node.alloc_order(0, direct_map_base);
        assert!(frame2.is_some());
        // Same frame should be available (might have different generation tag)
        assert!(frame2.unwrap().as_u64() > 0);
    }

    #[test]
    fn test_phys_addr_operations() {
        let addr = PhysAddr::new(0x1000);
        assert_eq!(addr.as_u64(), 0x1000);

        let addr2 = addr + 0x1000u64;
        assert_eq!(addr2.as_u64(), 0x2000);

        let addr3 = addr + PhysAddr::new(0x500);
        assert_eq!(addr3.as_u64(), 0x1500);
    }

    #[test]
    fn test_buddy_node_multiple_orders() {
        let node = BuddyNode::new(0);
        let base = PhysAddr::new(0x4000_0000);
        let mapping = test_direct_map(0x40_000, base.as_u64());
        let direct_map_base = mapping.direct_map_base;

        // Initialize with distinct free blocks at multiple orders.
        unsafe {
            node.free_lists[0].push(base, direct_map_base);
            node.free_lists[1].push(base + 0x10_000, direct_map_base);
            node.free_lists[2].push(base + 0x20_000, direct_map_base);
        }

        // Allocate different orders
        let order0 = node.alloc_order(0, direct_map_base);
        let order1 = node.alloc_order(1, direct_map_base);
        let order2 = node.alloc_order(2, direct_map_base);

        assert!(order0.is_some());
        assert!(order1.is_some());
        assert!(order2.is_some());

        // Verify addresses don't overlap (rough check)
        let addr0 = order0.unwrap().as_u64();
        let addr1 = order1.unwrap().as_u64();
        let addr2 = order2.unwrap().as_u64();

        assert_ne!(addr0, addr1);
        assert_ne!(addr1, addr2);
        assert_ne!(addr0, addr2);
    }

    #[test]
    fn test_numa_buddy_basic() {
        // Test NumaBuddy basic interface
        let buddy = NumaBuddy::new_uninit();
        assert_eq!(buddy.node_count, 0);
    }

    #[test]
    fn trampoline_page_excluded() {
        const SIZE: usize = 0x10_0000;
        let mapping = test_direct_map(SIZE, 0);
        let direct_map_base = mapping.direct_map_base;
        let memmap = [crate::LimineMemmapEntry {
            base: 0,
            length: SIZE as u64,
            mem_type: 0,
        }];
        let mut buddy = NumaBuddy::new_uninit();

        unsafe {
            buddy.init(
                &memmap,
                (PhysAddr::new(0), PhysAddr::new(0)),
                (PhysAddr::new(0), PhysAddr::new(0)),
                &[(PhysAddr::new(0x8000), PhysAddr::new(0x9000))],
                direct_map_base,
            )
        };

        let node = buddy.nodes[0].as_ref().unwrap();
        for order in 0..=MAX_ORDER as usize {
            assert!(
                !free_list_covers(
                    &node.free_lists[order],
                    order,
                    PhysAddr::new(0x8000),
                    direct_map_base
                ),
                "trampoline page present in free list order {order}"
            );
        }
    }

    #[test]
    fn alloc_frame_below_4g_returns_low_address() {
        const SIZE: usize = 0x20_000;
        let mapping = test_direct_map(SIZE, 0x1000);
        let direct_map_base = mapping.direct_map_base;
        let memmap = [crate::LimineMemmapEntry {
            base: 0x1000,
            length: 0x2_0000_0000 - 0x1000,
            mem_type: 0,
        }];
        let mut buddy = NumaBuddy::new_uninit();

        unsafe {
            buddy.init(
                &memmap,
                (PhysAddr::new(0), PhysAddr::new(0)),
                (PhysAddr::new(0), PhysAddr::new(0)),
                &[(PhysAddr::new(SIZE as u64), PhysAddr::new(0x2_0000_0000))],
                direct_map_base,
            )
        };

        for _ in 0..10 {
            let frame = buddy.alloc_frame_below_4g().expect("low frame available");
            assert!(frame.as_u64() < LOW_4G_LIMIT);
        }
    }
}
