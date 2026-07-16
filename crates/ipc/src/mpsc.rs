use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_arch_x86_64::cache::CachePadded;
use kernel_arch_x86_64::cycles::pause;

#[repr(C, align(64))]
struct Slot<T> {
    ready: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

unsafe impl<T: Send> Sync for Slot<T> {}

impl<T> Slot<T> {
    #[inline(always)]
    const fn new() -> Self {
        Self {
            ready: AtomicUsize::new(0),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

#[repr(C)]
pub struct MpscRing<T: Copy, const N: usize> {
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
    slots: [Slot<T>; N],
}

unsafe impl<T: Copy + Send, const N: usize> Send for MpscRing<T, N> {}
unsafe impl<T: Copy + Send, const N: usize> Sync for MpscRing<T, N> {}

impl<T: Copy, const N: usize> MpscRing<T, N> {
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
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            let head = self.head.load(Ordering::Acquire);
            if tail.wrapping_sub(head) == N {
                return Err(value);
            }

            match self.tail.compare_exchange_weak(
                tail,
                tail.wrapping_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(position) => {
                    let index = position & (N - 1);
                    unsafe { (*self.slots[index].value.get()).write(value) };
                    self.slots[index]
                        .ready
                        .store(position.wrapping_add(1), Ordering::Release);
                    return Ok(());
                }
                Err(observed) => {
                    tail = observed;
                    pause();
                }
            }
        }
    }

    #[inline(always)]
    pub fn pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let index = head & (N - 1);
        let expected = head.wrapping_add(1);
        if self.slots[index].ready.load(Ordering::Acquire) != expected {
            return None;
        }

        let value = unsafe { (*self.slots[index].value.get()).assume_init_read() };
        self.slots[index].ready.store(0, Ordering::Release);
        self.head.store(expected, Ordering::Release);
        Some(value)
    }
}

impl<T: Copy, const N: usize> Default for MpscRing<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::MpscRing;

    #[test]
    fn ordered_single_threaded_mpsc_path() {
        let ring = MpscRing::<u64, 2>::new();
        assert_eq!(ring.push(10), Ok(()));
        assert_eq!(ring.push(11), Ok(()));
        assert_eq!(ring.push(12), Err(12));
        assert_eq!(ring.pop(), Some(10));
        assert_eq!(ring.push(12), Ok(()));
        assert_eq!(ring.pop(), Some(11));
        assert_eq!(ring.pop(), Some(12));
        assert_eq!(ring.pop(), None);
    }
}
