use crate::tagged_stack::{AtomicTaggedStack, StackNode};
use core::mem;
use core::ptr::NonNull;

#[repr(C, align(64))]
pub struct PerCpuSlab<const OBJECT_SIZE: usize, const OBJECT_ALIGN: usize> {
    free: AtomicTaggedStack,
}

unsafe impl<const OBJECT_SIZE: usize, const OBJECT_ALIGN: usize> Send
    for PerCpuSlab<OBJECT_SIZE, OBJECT_ALIGN>
{
}
unsafe impl<const OBJECT_SIZE: usize, const OBJECT_ALIGN: usize> Sync
    for PerCpuSlab<OBJECT_SIZE, OBJECT_ALIGN>
{
}

impl<const OBJECT_SIZE: usize, const OBJECT_ALIGN: usize> PerCpuSlab<OBJECT_SIZE, OBJECT_ALIGN> {
    #[inline(always)]
    pub const fn new() -> Self {
        assert!(OBJECT_SIZE >= mem::size_of::<StackNode>());
        assert!(OBJECT_ALIGN >= mem::align_of::<StackNode>());
        assert!(OBJECT_ALIGN.is_power_of_two());
        Self {
            free: AtomicTaggedStack::new(),
        }
    }

    #[inline(always)]
    pub fn alloc(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.free.pop().cast::<u8>())
    }

    #[inline(always)]
    /// Return an object to this slab.
    ///
    /// # Safety
    /// `object` must have been allocated from this slab and must not be used
    /// after this call.
    pub unsafe fn free(&self, object: NonNull<u8>) {
        unsafe { self.free.push(object.as_ptr().cast::<StackNode>()) };
    }

    /// Add a backing memory range to the slab free list.
    ///
    /// # Safety
    /// `base..base + bytes` must be valid, exclusive slab backing storage with
    /// alignment suitable for `OBJECT_ALIGN`.
    pub unsafe fn add_slab(&self, base: NonNull<u8>, bytes: usize) -> usize {
        let start = align_up(base.as_ptr() as usize, OBJECT_ALIGN);
        let end = base.as_ptr() as usize + bytes;
        let stride = align_up(OBJECT_SIZE, OBJECT_ALIGN);
        let mut cursor = start;
        let mut count = 0usize;

        while cursor + stride <= end {
            unsafe { self.free.push(cursor as *mut StackNode) };
            cursor += stride;
            count += 1;
        }

        count
    }
}

impl<const OBJECT_SIZE: usize, const OBJECT_ALIGN: usize> Default
    for PerCpuSlab<OBJECT_SIZE, OBJECT_ALIGN>
{
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
