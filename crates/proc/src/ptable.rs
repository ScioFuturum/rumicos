//! Global PID → `*mut Process` table.
//!
//! Needed by `sys_kill` to find a target process by PID from another
//! process's context. Deliberately the SAME bucket-locked, fixed-capacity
//! shape as `crate::shootdown` / `kernel-fs`'s pagecache / `kernel-sched`'s
//! futex table (see `crate::signal` and the pagecache module docs for why
//! this repeated shape is intentional): at most one bucket lock held at a
//! time, no allocation, no cross-table lock ordering to deadlock on.
//!
//! This coexists with `crate::syscall`'s existing flat `PROCESS_TABLE`
//! (a 64-slot linear array scanned by *thread* pointer, used by
//! `current_process()`); that one answers "which process is THIS thread",
//! this one answers "which process has THIS pid". They're kept separate
//! rather than merged because they're keyed differently and `sys_kill`
//! needs O(1)-ish pid lookup without a linear scan racing thread install.

use crate::process::Process;
use kernel_sync::SpinLock;

const PTABLE_BUCKETS: usize = 64;
/// Per-bucket capacity. A full bucket drops the insert (the process is
/// simply unreachable by `kill`, never a memory-safety problem) — matching
/// every other fixed-capacity table in this kernel; 16 × 64 = 1024 live
/// PIDs, far beyond anything this checkpoint spawns.
const PTABLE_BUCKET_CAP: usize = 16;

#[derive(Clone, Copy)]
struct PTableEntry {
    pid: u32,
    /// `*mut Process` stored as usize — raw pointers aren't `Send`, and the
    /// surrounding `SpinLock` already provides the synchronization, exactly
    /// as `FdEntry.vnode_ptr` and `crate::vma::Backing::File.vnode_ptr` do.
    proc_ptr: usize,
}

static PTABLE: [SpinLock<[Option<PTableEntry>; PTABLE_BUCKET_CAP]>; PTABLE_BUCKETS] =
    [const { SpinLock::new([None; PTABLE_BUCKET_CAP]) }; PTABLE_BUCKETS];

/// Fibonacci-hash the PID into a bucket. Same multiply-then-shift shape as
/// the other tables; a 32-bit multiply is enough since PIDs are `u32`.
fn ptable_bucket(pid: u32) -> usize {
    ((pid.wrapping_mul(0x9e37_79b9)) >> (32 - 6)) as usize
}

/// Insert (or overwrite) the mapping `pid → proc`. Called from
/// `Process::create`, `sys_fork`, and `sys_clone`.
pub fn ptable_insert(pid: u32, proc: *mut Process) {
    let mut bucket = PTABLE[ptable_bucket(pid)].lock();
    // Overwrite an existing entry for this pid if one somehow exists
    // (PIDs are monotonic and never reused in this kernel, so this is
    // defensive), else take the first free slot. Computed as an index in
    // one pass to avoid a second mutable borrow of `bucket`.
    let mut target = None;
    for (i, slot) in bucket.iter().enumerate() {
        match slot {
            Some(e) if e.pid == pid => {
                target = Some(i);
                break;
            }
            None if target.is_none() => target = Some(i),
            _ => {}
        }
    }
    if let Some(i) = target {
        bucket[i] = Some(PTableEntry {
            pid,
            proc_ptr: proc as usize,
        });
    }
    // else: bucket full — process is unreachable by kill(), not unsafe.
}

/// Remove the mapping for `pid`. Called from `Process::exit`.
pub fn ptable_remove(pid: u32) {
    let mut bucket = PTABLE[ptable_bucket(pid)].lock();
    for slot in bucket.iter_mut() {
        if matches!(slot, Some(e) if e.pid == pid) {
            *slot = None;
        }
    }
}

/// Find the live `*mut Process` for `pid`, or `None`.
pub fn ptable_find(pid: u32) -> Option<*mut Process> {
    let bucket = PTABLE[ptable_bucket(pid)].lock();
    bucket
        .iter()
        .flatten()
        .find(|e| e.pid == pid)
        .map(|e| e.proc_ptr as *mut Process)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A dangling-but-never-dereferenced pointer: ptable only ever stores
    // and returns the pointer value, so these tests exercise the table's
    // bookkeeping without needing a real Process.
    fn fake(p: usize) -> *mut Process {
        p as *mut Process
    }

    #[test]
    fn insert_then_find_returns_the_pointer() {
        ptable_insert(1001, fake(0xdead_0000));
        assert_eq!(ptable_find(1001), Some(fake(0xdead_0000)));
        ptable_remove(1001);
    }

    #[test]
    fn find_after_remove_is_none() {
        ptable_insert(1002, fake(0xbeef_0000));
        ptable_remove(1002);
        assert_eq!(ptable_find(1002), None);
    }

    #[test]
    fn find_unknown_pid_is_none() {
        assert_eq!(ptable_find(999_999), None);
    }

    #[test]
    fn bucket_spreads_low_pids() {
        // PIDs 1..64 must not all collide into one bucket — the same
        // spread guarantee every other hashed table in this kernel makes.
        let mut seen = [false; PTABLE_BUCKETS];
        let mut distinct = 0;
        for pid in 1..64u32 {
            let b = ptable_bucket(pid);
            if !seen[b] {
                seen[b] = true;
                distinct += 1;
            }
        }
        assert!(
            distinct >= 8,
            "expected >= 8 distinct buckets for PIDs 1..64, got {distinct}"
        );
    }
}
