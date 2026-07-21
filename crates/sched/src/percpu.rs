use crate::queue::MlfqQueue;
use crate::thread::Thread;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use kernel_sync::SpinLock;

pub const MAX_CPUS: usize = 64;

#[repr(C, align(64))]
pub struct SchedCpu {
    pub cpu_id: u32,
    pub current: *mut Thread,
    pub idle_thread: *mut Thread,
    pub run_queue: MlfqQueue,
    pub tick_count: u64,
    pub steal_cursor: u32,
    _pad: [u8; 0],
}

unsafe impl Send for SchedCpu {}
unsafe impl Sync for SchedCpu {}

impl SchedCpu {
    pub const fn uninit() -> Self {
        Self {
            cpu_id: 0,
            current: ptr::null_mut(),
            idle_thread: ptr::null_mut(),
            run_queue: MlfqQueue::new(),
            tick_count: 0,
            steal_cursor: 0,
            _pad: [],
        }
    }
}

static SCHED_CPUS: [SpinLock<SchedCpu>; MAX_CPUS] =
    [const { SpinLock::new(SchedCpu::uninit()) }; MAX_CPUS];
static SCHED_CPU_COUNT: AtomicU32 = AtomicU32::new(0);

#[inline(always)]
pub fn sched_cpu(cpu_id: u32) -> &'static SpinLock<SchedCpu> {
    &SCHED_CPUS[cpu_id as usize % MAX_CPUS]
}

pub fn init_sched_cpu(cpu_id: u32) {
    let idx = cpu_id as usize;
    if idx >= MAX_CPUS {
        return;
    }

    let mut cpu = SCHED_CPUS[idx].lock();
    *cpu = SchedCpu::uninit();
    cpu.cpu_id = cpu_id;
    cpu.steal_cursor = (cpu_id + 1) % MAX_CPUS as u32;
    drop(cpu);

    let wanted = cpu_id + 1;
    let mut current = SCHED_CPU_COUNT.load(Ordering::Relaxed);
    while current < wanted {
        match SCHED_CPU_COUNT.compare_exchange_weak(
            current,
            wanted,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

pub(crate) fn set_idle_thread(cpu_id: u32, idle: *mut Thread) {
    let mut cpu = sched_cpu(cpu_id).lock();
    cpu.idle_thread = idle;
    if cpu.current.is_null() {
        cpu.current = idle;
        // SAFETY: `idle` points to this CPU's permanently live idle TCB.
        unsafe { (*idle).state = crate::thread::ThreadState::Running };
    }
}

#[inline(always)]
pub fn cpu_count() -> u32 {
    SCHED_CPU_COUNT.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn reset_for_tests(count: u32) {
    for (idx, slot) in SCHED_CPUS.iter().enumerate() {
        let mut cpu = slot.lock();
        *cpu = SchedCpu::uninit();
        cpu.cpu_id = idx as u32;
    }
    SCHED_CPU_COUNT.store(count, Ordering::Release);
}
