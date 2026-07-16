use core::ptr;
use core::sync::atomic::Ordering;
use kernel_arch_x86_64::atomic128::AtomicU128;
use kernel_arch_x86_64::cycles::pause;

#[repr(C)]
pub struct StackNode {
    pub next: *mut StackNode,
}

#[repr(C, align(64))]
pub struct AtomicTaggedStack {
    head: AtomicU128,
}

unsafe impl Send for AtomicTaggedStack {}
unsafe impl Sync for AtomicTaggedStack {}

impl AtomicTaggedStack {
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            head: AtomicU128::new(0),
        }
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        unpack(self.head.load(Ordering::Acquire)).0.is_null()
    }

    #[inline(always)]
    /// Push a node onto the stack.
    ///
    /// # Safety
    /// `node` must be valid, uniquely owned by the caller, and remain writable
    /// until it is popped from the stack.
    pub unsafe fn push(&self, node: *mut StackNode) {
        loop {
            let current = self.head.load(Ordering::Acquire);
            let (head, tag) = unpack(current);
            unsafe { (*node).next = head };
            let next = pack(node, tag.wrapping_add(1));
            if self
                .head
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
            pause();
        }
    }

    #[inline(always)]
    pub fn pop(&self) -> *mut StackNode {
        loop {
            let current = self.head.load(Ordering::Acquire);
            let (head, tag) = unpack(current);
            if head.is_null() {
                return ptr::null_mut();
            }

            let next_ptr = unsafe { (*head).next };
            let next = pack(next_ptr, tag.wrapping_add(1));
            if self
                .head
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return head;
            }
            pause();
        }
    }
}

impl Default for AtomicTaggedStack {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn pack(ptr: *mut StackNode, tag: u64) -> u128 {
    ((tag as u128) << 64) | (ptr as u64 as u128)
}

#[inline(always)]
fn unpack(value: u128) -> (*mut StackNode, u64) {
    (value as u64 as *mut StackNode, (value >> 64) as u64)
}
