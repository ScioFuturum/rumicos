use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_arch_x86_64::cache::CachePadded;

#[repr(C, align(64))]
struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
}

unsafe impl<T: Send> Sync for Slot<T> {}

impl<T> Slot<T> {
    #[inline(always)]
    const fn new() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

#[repr(C)]
pub struct SpscRing<T: Copy, const N: usize> {
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    slots: [Slot<T>; N],
}

unsafe impl<T: Copy + Send, const N: usize> Send for SpscRing<T, N> {}
unsafe impl<T: Copy + Send, const N: usize> Sync for SpscRing<T, N> {}

impl<T: Copy, const N: usize> SpscRing<T, N> {
    #[inline(always)]
    pub const fn new() -> Self {
        assert!(N > 0 && N.is_power_of_two());
        Self {
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            slots: [const { Slot::new() }; N],
        }
    }

    #[inline(always)]
    pub fn capacity(&self) -> usize {
        N
    }

    #[inline(always)]
    pub fn push(&self, value: T) -> Result<(), T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) == N {
            return Err(value);
        }

        let index = tail & (N - 1);
        unsafe { (*self.slots[index].value.get()).write(value) };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    #[inline(always)]
    pub fn pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }

        let index = head & (N - 1);
        let value = unsafe { (*self.slots[index].value.get()).assume_init_read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

impl<T: Copy, const N: usize> Default for SpscRing<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::SpscRing;

    #[test]
    fn push_pop_wraps_without_reordering() {
        let ring = SpscRing::<u64, 4>::new();
        assert_eq!(ring.pop(), None);
        assert_eq!(ring.push(1), Ok(()));
        assert_eq!(ring.push(2), Ok(()));
        assert_eq!(ring.push(3), Ok(()));
        assert_eq!(ring.push(4), Ok(()));
        assert_eq!(ring.push(5), Err(5));
        assert_eq!(ring.pop(), Some(1));
        assert_eq!(ring.pop(), Some(2));
        assert_eq!(ring.push(5), Ok(()));
        assert_eq!(ring.push(6), Ok(()));
        assert_eq!(ring.pop(), Some(3));
        assert_eq!(ring.pop(), Some(4));
        assert_eq!(ring.pop(), Some(5));
        assert_eq!(ring.pop(), Some(6));
        assert_eq!(ring.pop(), None);
    }
}
