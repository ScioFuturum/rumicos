use crate::percpu::{SchedCpu, cpu_count, sched_cpu};
use crate::thread::Thread;

const MAX_STEAL_BATCH: usize = 8;

pub fn try_steal(cpu_id: u32, sc: &mut SchedCpu) -> Option<*mut Thread> {
    let count = cpu_count();
    if count <= 1 {
        return None;
    }

    let mut scanned = 0u32;
    let mut attempts = 0u32;
    while scanned < count && attempts < count - 1 {
        let victim = sc.steal_cursor % count;
        sc.steal_cursor = (victim + 1) % count;
        scanned += 1;
        if victim == cpu_id {
            continue;
        }
        attempts += 1;

        let Some(victim_cpu) = sched_cpu(victim).try_lock() else {
            continue;
        };
        let victim_queue_len = victim_cpu.run_queue.len();
        if victim_queue_len < 2 {
            continue;
        }

        let batch = ((victim_queue_len / 2).max(1) as usize).min(MAX_STEAL_BATCH);
        let stolen = victim_cpu.run_queue.steal_into(&sc.run_queue, batch);
        if stolen != 0 {
            return sc.run_queue.pop();
        }
    }

    None
}

pub fn busiest_cpu() -> u32 {
    let count = cpu_count();
    let mut best_cpu = 0u32;
    let mut best_len = 0u32;
    let mut cpu = 0u32;
    while cpu < count {
        let len = sched_cpu(cpu).lock().run_queue.len();
        if len > best_len {
            best_len = len;
            best_cpu = cpu;
        }
        cpu += 1;
    }
    best_cpu
}

pub fn least_loaded_cpu() -> u32 {
    let count = cpu_count();
    if count == 0 {
        return 0;
    }

    let mut best_cpu = 0u32;
    let mut best_len = u32::MAX;
    let mut cpu = 0u32;
    while cpu < count {
        let len = sched_cpu(cpu).lock().run_queue.len();
        if len < best_len {
            best_len = len;
            best_cpu = cpu;
        }
        cpu += 1;
    }
    best_cpu
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::percpu::reset_for_tests;
    use crate::thread::{Thread, ThreadState, test_thread};
    use std::sync::{Mutex, MutexGuard};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().expect("steal test lock poisoned")
    }

    #[test]
    fn least_loaded_cpu_picks_shortest_queue() {
        let _guard = test_lock();
        reset_for_tests(2);
        let mut cpu0_threads = [
            test_thread(0, 0, ThreadState::Runnable),
            test_thread(1, 0, ThreadState::Runnable),
            test_thread(2, 0, ThreadState::Runnable),
            test_thread(3, 0, ThreadState::Runnable),
            test_thread(4, 0, ThreadState::Runnable),
        ];
        let mut cpu1_threads = [
            test_thread(5, 0, ThreadState::Runnable),
            test_thread(6, 0, ThreadState::Runnable),
        ];

        {
            let cpu0 = sched_cpu(0).lock();
            for thread in &mut cpu0_threads {
                assert!(cpu0.run_queue.push(thread as *mut Thread, 0));
            }
        }
        {
            let cpu1 = sched_cpu(1).lock();
            for thread in &mut cpu1_threads {
                assert!(cpu1.run_queue.push(thread as *mut Thread, 0));
            }
        }

        assert_eq!(least_loaded_cpu(), 1);
    }

    #[test]
    fn steal_cursor_advances_after_empty_attempt() {
        let _guard = test_lock();
        reset_for_tests(2);
        let mut local = sched_cpu(0).lock();
        local.steal_cursor = 1;

        assert_eq!(try_steal(0, &mut local), None);
        assert_eq!(local.steal_cursor, 0);
    }

    #[test]
    fn steal_cursor_skips_self_and_tries_next_cpu() {
        let _guard = test_lock();
        reset_for_tests(2);
        let mut local = sched_cpu(0).lock();
        local.steal_cursor = 0;

        assert_eq!(try_steal(0, &mut local), None);
        assert_eq!(local.steal_cursor, 0);
    }
}
