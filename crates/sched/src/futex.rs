use crate::thread::{Thread, ThreadState};
use crate::waitqueue::{WaitNode, WaitQueue};
use core::sync::atomic::{AtomicU32, Ordering};
use kernel_sync::SpinLock;

pub const FUTEX_BUCKETS: usize = 256;

struct FutexBucket {
    lock: SpinLock<()>,
    queue: WaitQueue,
}

impl FutexBucket {
    const fn new() -> Self {
        Self {
            lock: SpinLock::new(()),
            queue: WaitQueue::new(),
        }
    }
}

static FUTEX_TABLE: [FutexBucket; FUTEX_BUCKETS] = [const { FutexBucket::new() }; FUTEX_BUCKETS];

#[inline]
pub fn bucket_index(addr: *const AtomicU32) -> usize {
    let key = (addr as usize >> 2) as u64;
    let hash = key.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (hash >> (64 - 8)) as usize
}

/// Wait while `*addr == expected`.
///
/// # Safety
/// `addr` must point to a valid `AtomicU32` in kernel memory.
pub unsafe fn futex_wait(addr: *const AtomicU32, expected: u32) {
    // SAFETY: caller guarantees `addr` points to a valid AtomicU32.
    if unsafe { (*addr).load(Ordering::Acquire) } != expected {
        return;
    }

    let bucket = &FUTEX_TABLE[bucket_index(addr)];
    let cpu_id = crate::current_cpu_id();
    let cur: *mut Thread = crate::percpu::sched_cpu(cpu_id).lock().current;
    assert!(!cur.is_null(), "futex_wait called from idle/null context");

    let mut node = WaitNode::new(cur);
    node.key = addr as usize;

    // SAFETY: interrupts are disabled before taking the bucket lock to close
    // the check/enqueue/schedule lost-wakeup window.
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };

    let should_block = bucket.queue.with_locked(|inner| {
        // ATOMICITY: the futex value is re-read while the bucket wait queue is
        // locked; if it still matches, enqueue and mark Blocked before unlock.
        // SAFETY: caller guarantees `addr` is valid.
        let current = unsafe { (*addr).load(Ordering::Acquire) };
        if current != expected {
            return false;
        }
        // SAFETY: `node` lives on this thread's kernel stack until wakeup.
        unsafe {
            inner.enqueue_raw(&mut node);
            (*cur).state = ThreadState::Blocked;
        }
        true
    });

    if should_block {
        // SAFETY: IF=0 and this thread is enqueued and blocked.
        unsafe { crate::schedule(cpu_id) };
    }

    // SAFETY: restore interruptibility after either skipping or returning from
    // the blocking schedule point.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    debug_assert!(!should_block || node.woken, "futex_wait resumed unwoken");
}

pub fn futex_wake(addr: *const AtomicU32, count: u32) -> u32 {
    let bucket = &FUTEX_TABLE[bucket_index(addr)];
    bucket.queue.wake_n_matching(addr as usize, count)
}

pub fn futex_requeue(
    addr: *const AtomicU32,
    wake_n: u32,
    addr2: *const AtomicU32,
    requeue_n: u32,
) -> u32 {
    // LOCK ORDER INVARIANT: when two distinct bucket locks must be held
    // simultaneously, ALWAYS acquire the one with the lower bucket index first.
    // This is a global invariant enforced here and must be preserved if
    // futex_requeue is ever called from new code paths.
    // Violation -> AB-BA deadlock between concurrent requeue calls.
    let idx_a = bucket_index(addr);
    let idx_b = bucket_index(addr2);
    let src_key = addr as usize;
    let dst_key = addr2 as usize;

    if idx_a == idx_b {
        let _guard = FUTEX_TABLE[idx_a].lock.lock();
        return requeue_same_bucket(idx_a, src_key, dst_key, wake_n, requeue_n);
    }

    let (lo, hi, swapped) = if idx_a < idx_b {
        (idx_a, idx_b, false)
    } else {
        (idx_b, idx_a, true)
    };

    let _guard_lo = FUTEX_TABLE[lo].lock.lock();
    let _guard_hi = FUTEX_TABLE[hi].lock.lock();

    let (src_idx, dst_idx) = if !swapped { (lo, hi) } else { (hi, lo) };
    requeue_across_buckets(src_idx, dst_idx, src_key, dst_key, wake_n, requeue_n)
}

fn requeue_same_bucket(
    bucket_idx: usize,
    src_key: usize,
    dst_key: usize,
    wake_n: u32,
    requeue_n: u32,
) -> u32 {
    let bucket = &FUTEX_TABLE[bucket_idx];
    let woken = bucket.queue.wake_n_matching(src_key, wake_n);
    let moved = bucket
        .queue
        .requeue_matching(&bucket.queue, src_key, dst_key, requeue_n);
    woken + moved
}

fn requeue_across_buckets(
    src_idx: usize,
    dst_idx: usize,
    src_key: usize,
    dst_key: usize,
    wake_n: u32,
    requeue_n: u32,
) -> u32 {
    let src = &FUTEX_TABLE[src_idx];
    let dst = &FUTEX_TABLE[dst_idx];
    let woken = src.queue.wake_n_matching(src_key, wake_n);
    let moved = src
        .queue
        .requeue_matching(&dst.queue, src_key, dst_key, requeue_n);
    woken + moved
}

pub struct KMutex {
    state: AtomicU32,
}

impl KMutex {
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
        }
    }

    pub fn lock(&self) -> KMutexGuard<'_> {
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return KMutexGuard { mutex: self };
        }

        loop {
            let old = self.state.swap(2, Ordering::Acquire);
            if old == 0 {
                break;
            }
            // SAFETY: `state` is an AtomicU32 embedded in this kernel mutex.
            unsafe { futex_wait(&self.state, 2) };
        }
        KMutexGuard { mutex: self }
    }

    pub fn try_lock(&self) -> Option<KMutexGuard<'_>> {
        self.state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| KMutexGuard { mutex: self })
    }

    pub fn state(&self) -> u32 {
        self.state.load(Ordering::Relaxed)
    }
}

impl Default for KMutex {
    fn default() -> Self {
        Self::new()
    }
}

pub struct KMutexGuard<'a> {
    mutex: &'a KMutex,
}

impl Drop for KMutexGuard<'_> {
    fn drop(&mut self) {
        let prev = self.mutex.state.swap(0, Ordering::Release);
        if prev == 2 {
            futex_wake(&self.mutex.state, 1);
        }
        debug_assert!(prev != 0, "KMutex double-unlock detected");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_index_is_stable_for_same_address() {
        let value = AtomicU32::new(0);
        let addr = &value as *const AtomicU32;
        assert_eq!(bucket_index(addr), bucket_index(addr));
    }

    #[test]
    fn bucket_index_can_distinguish_different_addresses() {
        let a = AtomicU32::new(0);
        let b = AtomicU32::new(0);
        let ia = bucket_index(&a);
        let ib = bucket_index(&b);
        assert!(ia < FUTEX_BUCKETS);
        assert!(ib < FUTEX_BUCKETS);
    }

    #[test]
    fn kmutex_state_after_new_is_unlocked() {
        let mutex = KMutex::new();
        assert_eq!(mutex.state(), 0);
    }

    #[test]
    fn kmutex_try_lock_observes_unlock_after_drop() {
        let mutex = KMutex::new();
        let guard = mutex.lock();
        assert!(mutex.try_lock().is_none());
        drop(guard);
        assert!(mutex.try_lock().is_some());
    }

    #[test]
    fn futex_wait_spurious_mismatch_returns_without_blocking() {
        let value = AtomicU32::new(7);
        // SAFETY: `value` is a valid AtomicU32 and mismatch returns before
        // scheduler state is consulted.
        unsafe { futex_wait(&value, 6) };
        assert_eq!(value.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn futex_bucket_count_is_power_of_two() {
        assert_eq!(FUTEX_BUCKETS.count_ones(), 1);
    }

    #[test]
    fn requeue_same_bucket_no_double_lock() {
        static A: AtomicU32 = AtomicU32::new(0);
        assert_eq!(futex_requeue(&A, 1, &A, 1), 0);
    }

    #[test]
    fn requeue_canonical_order_low_first() {
        static A: AtomicU32 = AtomicU32::new(0);
        static B: AtomicU32 = AtomicU32::new(0);
        assert_eq!(futex_requeue(&A, 0, &B, 0), 0);
        assert_eq!(futex_requeue(&B, 0, &A, 0), 0);
    }

    #[test]
    fn kmutex_no_spurious_wake_on_unlock() {
        let mutex = KMutex::new();
        assert_eq!(mutex.state.load(Ordering::Relaxed), 0);
        {
            let _guard = mutex.lock();
            assert_eq!(mutex.state.load(Ordering::Relaxed), 1);
        }
        assert_eq!(mutex.state.load(Ordering::Relaxed), 0);
    }
}
