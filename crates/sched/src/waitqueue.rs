use crate::percpu::sched_cpu;
use crate::thread::{Thread, ThreadState};
use core::cell::UnsafeCell;
use core::ptr;
use kernel_sync::SpinLock;

#[repr(C)]
pub struct WaitNode {
    pub thread: *mut Thread,
    pub next: *mut WaitNode,
    pub prev: *mut WaitNode,
    pub woken: bool,
    pub key: usize,
}

unsafe impl Send for WaitNode {}

impl WaitNode {
    pub const fn new(thread: *mut Thread) -> Self {
        Self {
            thread,
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
            woken: false,
            key: 0,
        }
    }
}

pub struct WaitQueue {
    sentinel: UnsafeCell<WaitNode>,
    inner: SpinLock<WaitQueueInner>,
}

pub(crate) struct WaitQueueInner {
    head: *mut WaitNode,
    len: u32,
}

unsafe impl Send for WaitQueue {}
unsafe impl Sync for WaitQueue {}
unsafe impl Send for WaitQueueInner {}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            sentinel: UnsafeCell::new(WaitNode::new(ptr::null_mut())),
            inner: SpinLock::new(WaitQueueInner::new()),
        }
    }

    /// Enqueue `node` at the tail of the queue.
    ///
    /// # Safety
    /// `node` must remain valid and unmoved until it is dequeued or woken.
    pub unsafe fn enqueue(&self, node: *mut WaitNode) {
        self.with_locked(|inner| {
            // SAFETY: caller guarantees `node` is valid until removal.
            unsafe { inner.enqueue_raw(node) };
        });
    }

    /// Remove `node` from this queue if it is currently linked.
    ///
    /// # Safety
    /// `node` must be a valid pointer that was previously enqueued.
    pub unsafe fn dequeue(&self, node: *mut WaitNode) {
        self.with_locked(|inner| {
            // SAFETY: caller guarantees `node` is valid; dequeue_raw tolerates
            // already-unlinked nodes.
            unsafe { inner.dequeue_raw(node) };
        });
    }

    pub fn wake_n(&self, n: u32) -> u32 {
        self.wake_n_matching(0, n)
    }

    pub fn wake_n_matching(&self, key: usize, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }

        self.with_locked(|inner| {
            let mut woken = 0u32;
            // SAFETY: `with_locked` initializes the sentinel and holds the list
            // lock for the full traversal/removal sequence.
            unsafe {
                let head = inner.head;
                let mut node = (*head).next;
                while node != head && woken < n {
                    let next = (*node).next;
                    if (key == 0 || (*node).key == key) && try_make_runnable((*node).thread) {
                        (*node).woken = true;
                        inner.dequeue_raw(node);
                        woken += 1;
                    }
                    node = next;
                }
            }
            woken
        })
    }

    pub fn wake_all(&self) -> u32 {
        self.wake_n(u32::MAX)
    }

    pub fn requeue_matching(&self, dst: &WaitQueue, key: usize, new_key: usize, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }

        if ptr::eq(self, dst) {
            return self.with_locked(|inner| {
                let mut moved = 0u32;
                // SAFETY: the queue lock is held and the sentinel is valid.
                unsafe {
                    let head = inner.head;
                    let mut node = (*head).next;
                    while node != head && moved < n {
                        if (*node).key == key {
                            (*node).key = new_key;
                            moved += 1;
                        }
                        node = (*node).next;
                    }
                }
                moved
            });
        }

        self.with_locked(|src| {
            dst.with_locked(|dst_inner| {
                let mut moved = 0u32;
                // SAFETY: both queue locks are held; nodes removed from `src`
                // are immediately linked into `dst_inner`.
                unsafe {
                    let head = src.head;
                    let mut node = (*head).next;
                    while node != head && moved < n {
                        let next = (*node).next;
                        if (*node).key == key {
                            src.dequeue_raw(node);
                            (*node).key = new_key;
                            (*node).woken = false;
                            dst_inner.enqueue_raw(node);
                            moved += 1;
                        }
                        node = next;
                    }
                }
                moved
            })
        })
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> u32 {
        self.with_locked(|inner| inner.len)
    }

    pub(crate) fn with_locked<R>(&self, f: impl FnOnce(&mut WaitQueueInner) -> R) -> R {
        let mut inner = self.inner.lock();
        // SAFETY: the sentinel is embedded in `self` and remains valid for the
        // lifetime of this wait queue.
        unsafe { inner.ensure_initialized(self.sentinel.get()) };
        f(&mut inner)
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitQueueInner {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            len: 0,
        }
    }

    unsafe fn ensure_initialized(&mut self, sentinel: *mut WaitNode) {
        if self.head.is_null() {
            // SAFETY: caller supplies the queue-owned sentinel and holds the
            // queue lock, so no other CPU observes a partially initialized list.
            unsafe {
                (*sentinel).next = sentinel;
                (*sentinel).prev = sentinel;
                (*sentinel).thread = ptr::null_mut();
                (*sentinel).woken = false;
                (*sentinel).key = 0;
            }
            self.head = sentinel;
        }
    }

    pub(crate) unsafe fn enqueue_raw(&mut self, node: *mut WaitNode) {
        debug_assert!(!self.head.is_null());
        // SAFETY: caller holds the queue lock and `node` is valid and unlinked.
        unsafe {
            let tail = (*self.head).prev;
            (*node).next = self.head;
            (*node).prev = tail;
            (*node).woken = false;
            (*tail).next = node;
            (*self.head).prev = node;
        }
        self.len = self.len.saturating_add(1);
    }

    pub(crate) unsafe fn dequeue_raw(&mut self, node: *mut WaitNode) {
        // SAFETY: caller holds the queue lock; null links mean the node is not
        // currently on a queue.
        unsafe {
            if (*node).next.is_null() || (*node).prev.is_null() {
                return;
            }
            let prev = (*node).prev;
            let next = (*node).next;
            (*prev).next = next;
            (*next).prev = prev;
            (*node).next = ptr::null_mut();
            (*node).prev = ptr::null_mut();
        }
        self.len = self.len.saturating_sub(1);
    }
}

fn try_make_runnable(thread: *mut Thread) -> bool {
    if thread.is_null() {
        return false;
    }

    // Woken threads go to the BSP's queue, same pinning rationale as
    // enqueue_thread: only the BSP's queue is reliably drained today.
    // Two attempts cover transient sched-lock contention.
    let target = 0;
    if try_push_to_cpu(thread, target) || try_push_to_cpu(thread, target) {
        // A push from an AP onto the BSP's queue needs the BSP poked in
        // case it is idling (see KICK_REMOTE_HOOK).
        if target != crate::current_cpu_id() {
            crate::kick_remote_cpus();
        }
        return true;
    }
    false
}

fn try_push_to_cpu(thread: *mut Thread, cpu_id: u32) -> bool {
    let Some(cpu) = sched_cpu(cpu_id).try_lock() else {
        return false;
    };
    // SAFETY: caller passes a live TCB that is being transitioned from blocked
    // to runnable while still protected by its wait queue.
    let level = unsafe {
        (*thread).state = ThreadState::Runnable;
        (*thread).priority as usize
    };
    cpu.run_queue.push(thread, level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::{ThreadState, test_thread};

    #[test]
    fn new_queue_is_empty() {
        let queue = WaitQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn enqueue_and_dequeue_updates_len() {
        let queue = WaitQueue::new();
        let mut thread = test_thread(1, 0, ThreadState::Blocked);
        let mut node = WaitNode::new(&mut thread);

        // SAFETY: `node` and `thread` live until after dequeue.
        unsafe { queue.enqueue(&mut node) };
        assert_eq!(queue.len(), 1);
        assert!(!queue.is_empty());

        // SAFETY: `node` was enqueued in this queue above.
        unsafe { queue.dequeue(&mut node) };
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn wake_zero_does_not_modify_queue() {
        let queue = WaitQueue::new();
        let mut thread = test_thread(1, 0, ThreadState::Blocked);
        let mut node = WaitNode::new(&mut thread);

        // SAFETY: `node` and `thread` live for the duration of the test.
        unsafe { queue.enqueue(&mut node) };
        assert_eq!(queue.wake_n(0), 0);
        assert_eq!(queue.len(), 1);
        assert!(!node.woken);

        // SAFETY: cleanup for the still-enqueued node.
        unsafe { queue.dequeue(&mut node) };
    }
}
