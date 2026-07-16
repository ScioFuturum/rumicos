use crate::thread::Thread;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use kernel_sync::SpinLock;

pub const MLFQ_LEVELS: usize = 8;
pub const QUEUE_DEPTH: usize = 256;
pub const BOOST_INTERVAL: u32 = 1000;

pub const fn timeslice(level: usize) -> u32 {
    1 << level
}

pub struct MlfqQueue {
    levels: [RingQueue; MLFQ_LEVELS],
    lock: SpinLock<()>,
    total: AtomicU32,
}

struct RingQueue {
    buf: [AtomicU64; QUEUE_DEPTH],
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl MlfqQueue {
    pub const fn new() -> Self {
        Self {
            levels: [const { RingQueue::new() }; MLFQ_LEVELS],
            lock: SpinLock::new(()),
            total: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    pub fn push(&self, thread: *mut Thread, level: usize) -> bool {
        let level = level.min(MLFQ_LEVELS - 1);
        if self.levels[level].push(thread) {
            self.total.fetch_add(1, Ordering::Release);
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn pop(&self) -> Option<*mut Thread> {
        for level in 0..MLFQ_LEVELS {
            if let Some(thread) = self.levels[level].pop() {
                self.total.fetch_sub(1, Ordering::Release);
                return Some(thread);
            }
        }
        None
    }

    pub fn steal_into(&self, dst: &MlfqQueue, n: usize) -> usize {
        let _guard = self.lock.lock();
        let mut stolen = 0usize;
        while stolen < n {
            let Some(thread) = self.pop() else {
                break;
            };
            // SAFETY: run queues only contain live Thread pointers; priority is
            // read to preserve MLFQ level on the destination CPU.
            let level = unsafe { (*thread).priority as usize };
            if !dst.push(thread, level) {
                let _ = self.push(thread, level);
                break;
            }
            stolen += 1;
        }
        stolen
    }

    pub fn boost_all(&self) {
        let _guard = self.lock.lock();
        for level in 1..MLFQ_LEVELS {
            while let Some(thread) = self.levels[level].pop() {
                // SAFETY: run queues only contain live Thread pointers; boost
                // updates scheduling metadata before moving to level 0.
                unsafe {
                    (*thread).priority = 0;
                    (*thread).ticks_used = 0;
                }
                if self.levels[0].push(thread) {
                    continue;
                }
                let _ = self.levels[level].push(thread);
                break;
            }
        }
    }

    #[inline(always)]
    pub fn len(&self) -> u32 {
        self.total.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MlfqQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl RingQueue {
    const fn new() -> Self {
        Self {
            buf: [const { AtomicU64::new(0) }; QUEUE_DEPTH],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    #[inline(always)]
    fn push(&self, thread: *mut Thread) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let next = (tail + 1) % QUEUE_DEPTH;
        if next == self.head.load(Ordering::Acquire) {
            return false;
        }

        self.buf[tail].store(thread as u64, Ordering::Relaxed);
        self.tail.store(next, Ordering::Release);
        true
    }

    #[inline(always)]
    fn pop(&self) -> Option<*mut Thread> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            if head == self.tail.load(Ordering::Acquire) {
                return None;
            }

            let raw = self.buf[head].load(Ordering::Relaxed);
            let next = (head + 1) % QUEUE_DEPTH;
            if self
                .head
                .compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(raw as *mut Thread);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::{ThreadState, test_thread};

    #[test]
    fn timeslice_is_power_of_two_by_level() {
        assert_eq!(timeslice(0), 1);
        assert_eq!(timeslice(3), 8);
        assert_eq!(timeslice(7), 128);
    }

    #[test]
    fn boost_interval_is_one_second_at_1000hz() {
        assert_eq!(BOOST_INTERVAL, 1000);
    }

    #[test]
    fn mlfq_push_pop_returns_highest_priority_first() {
        let queue = MlfqQueue::new();
        let mut t2 = test_thread(2, 2, ThreadState::Runnable);
        let mut t0 = test_thread(0, 0, ThreadState::Runnable);
        let mut t1 = test_thread(1, 1, ThreadState::Runnable);

        assert!(queue.push(&mut t2, 2));
        assert!(queue.push(&mut t0, 0));
        assert!(queue.push(&mut t1, 1));

        assert_eq!(queue.pop(), Some(&mut t0 as *mut Thread));
        assert_eq!(queue.pop(), Some(&mut t1 as *mut Thread));
        assert_eq!(queue.pop(), Some(&mut t2 as *mut Thread));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn steal_into_moves_requested_threads() {
        let donor = MlfqQueue::new();
        let receiver = MlfqQueue::new();
        let mut t0 = test_thread(0, 0, ThreadState::Runnable);
        let mut t1 = test_thread(1, 0, ThreadState::Runnable);
        let mut t2 = test_thread(2, 0, ThreadState::Runnable);
        let mut t3 = test_thread(3, 0, ThreadState::Runnable);

        assert!(donor.push(&mut t0, 0));
        assert!(donor.push(&mut t1, 0));
        assert!(donor.push(&mut t2, 0));
        assert!(donor.push(&mut t3, 0));

        assert_eq!(donor.steal_into(&receiver, 2), 2);
        assert_eq!(donor.len(), 2);
        assert_eq!(receiver.len(), 2);
    }
}
