//! Cross-CPU TLB invalidation ("shootdown").
//!
//! ## Why this is now a real bug, not a hypothetical
//!
//! Before `crate::clone`'s `CLONE_VM` support, every [`crate::address_space::AddressSpace`]
//! had exactly one thread, on exactly one CPU, ever — so every existing
//! `flush_page`/`flush_pcid` call (in `try_resolve_cow`, `munmap`,
//! `do_execve`'s post-swap flush, `fork_cow`'s per-page CoW-marking flush)
//! only ever needed to invalidate the *local* CPU's TLB. After `CLONE_VM`,
//! two threads — possibly running on two different CPUs *right now* — can
//! share one `AddressSpace`. If one CPU remaps or unmaps a page in that
//! shared `AddressSpace` and only flushes its own local TLB, the other CPU
//! keeps a stale, possibly now-invalid or now-repointed translation cached
//! — a real, exploitable stale-TLB-use-after-remap bug, not a performance
//! nitpick. See `crate::address_space`'s retrofitted call sites for where
//! this module actually gets used, and its module docs for which one call
//! site (`do_execve`'s `flush_all_pcids`) is deliberately left local-only
//! and why.
//!
//! ## Design: minimal blocking shootdown
//!
//! Correctness first, not the fastest possible implementation — see
//! "Known limitations" in the checkpoint summary for the batched/deferred
//! design this punts on, and the missing stuck-CPU timeout.
//!
//! ## Locking discipline
//!
//! This module intentionally does NOT follow the bucket-locked,
//! Fibonacci-hashed, fixed-capacity table pattern the futex table,
//! cow table, and page cache all share. That pattern exists for sparse
//! per-*resource* tables (one entry per futex word / CoW frame / cached
//! page) where many independent entries are mutated concurrently and a
//! single global lock would serialize unrelated work. A shootdown request
//! is not a per-resource table at all — it's a single, short-lived,
//! system-wide broadcast, and this checkpoint's own design brief
//! explicitly sanctions "a short-lived per-shootdown-request lock" as a
//! different kind of lock from that pattern. Two locks are used here, for
//! two different jobs:
//!
//!   - [`PENDING_SHOOTDOWN`]'s own `SpinLock` guards just the read/write of
//!     the in-flight request's data, and is intentionally released before
//!     IPIs are sent so the remote handler(s) can acquire it too.
//!   - [`SHOOTDOWN_SERIALIZE`] guards the entire `shootdown_page` call,
//!     end-to-end. This is *not* in the prompt's own reference sketch, and
//!     is added deliberately: releasing `PENDING_SHOOTDOWN`'s lock before
//!     sending IPIs (required, or the remote handler could never acquire
//!     it) reopens a window where a second, unrelated concurrent
//!     initiator (a different CPU, a different AddressSpace) could
//!     overwrite the single global slot before the first initiator's
//!     spin-wait observes completion — corrupting or losing the first
//!     request. `SHOOTDOWN_SERIALIZE` closes that window by ensuring only
//!     one shootdown is ever in flight system-wide, without needing
//!     `PENDING_SHOOTDOWN`'s own lock to double as that serialization
//!     (which would deadlock: the initiator's spin-wait would never let
//!     go of a lock the remote handler needs to make progress).
//!
//! Neither lock is a bucket table, and this module never holds both at
//! once with anything blocking in between beyond the documented critical
//! sections above.

use core::sync::atomic::{AtomicU64, Ordering};
use kernel_paging::VirtAddr;
use kernel_sync::SpinLock;

/// Vector used for the shootdown IPI. Chosen as an unused slot in this
/// tree's IDT: `TIMER_VECTOR` (`kernel_apic::TIMER_VECTOR`) is `0x20`, the
/// spurious vector is `0xff`, and `pagefault::PF_VECTOR` is `14` — as of
/// this checkpoint those are the only three vectors
/// `kernel_cpu::register_handler` is ever called with, so `0xfc` is free.
pub const SHOOTDOWN_VECTOR: u8 = 0xfc;

/// Matches `kernel_smp::MAX_CPUS`/`kernel_sched::percpu::MAX_CPUS` (64) —
/// duplicated as a plain literal here (rather than depending on either
/// crate just for this constant) since it only needs to bound the width
/// of the `u64` bitmask below.
const MAX_SHOOTDOWN_CPUS: u32 = 64;

/// Which CPUs currently have a thread running with a given `AddressSpace`
/// actively loaded in `CR3` *right now* — not just "has ever run a thread
/// from this `AddressSpace`". Identity is implicit: one `ActiveCpuMask`
/// lives inside each `AddressSpace` itself (a stable pointer, per
/// `AddressSpace::alloc_shared`), rather than being keyed by `pml4_phys` —
/// a freed-and-reused physical PML4 frame could otherwise alias two
/// unrelated `AddressSpace`s across time.
pub struct ActiveCpuMask {
    bits: AtomicU64,
}

impl ActiveCpuMask {
    pub const fn new() -> Self {
        Self {
            bits: AtomicU64::new(0),
        }
    }

    /// Record that `cpu_id` now has this `AddressSpace` loaded in `CR3`.
    /// Must be called no later than the `CR3` write itself — see
    /// `crate::process::ring3_entry_rust`/`crate::exec`'s call sites and
    /// this module's own "ActiveCpuMask races" note in the checkpoint
    /// summary for why the ordering (never *after*) is the direction that
    /// actually matters for correctness.
    pub fn mark_active(&self, cpu_id: u32) {
        self.bits
            .fetch_or(1u64 << (cpu_id % MAX_SHOOTDOWN_CPUS), Ordering::AcqRel);
    }

    /// Record that `cpu_id` no longer has this `AddressSpace` loaded.
    pub fn mark_inactive(&self, cpu_id: u32) {
        self.bits
            .fetch_and(!(1u64 << (cpu_id % MAX_SHOOTDOWN_CPUS)), Ordering::AcqRel);
    }

    /// A point-in-time snapshot of which CPUs are active. Racy by
    /// construction (a CPU can join or leave between this read and
    /// whatever the caller does with it) — see the module-level docs on
    /// which direction of race is safe (extra, harmless IPIs) and which
    /// would be dangerous (a newly-active CPU missed entirely); the
    /// mark_active-before-CR3-write ordering above is what rules out the
    /// dangerous direction.
    pub fn snapshot(&self) -> u64 {
        self.bits.load(Ordering::Acquire)
    }
}

impl Default for ActiveCpuMask {
    fn default() -> Self {
        Self::new()
    }
}

/// A pending shootdown request: filled in by the initiator, decremented by
/// every target CPU once it has actually executed the invalidation.
///
/// No `ShootdownKind`/`AllPcid` variant here (unlike this checkpoint's own
/// design sketch): every existing retrofit call site in
/// `crate::address_space` invalidates a single page, never a whole PCID —
/// `do_execve`'s one `flush_all_pcids()` call is exempted as local-only
/// (see that module's docs) precisely because it never needs a broadcast.
/// Keeping an always-`Page`, never-`AllPcid` variant around would trip
/// `-D warnings`'s dead-code lint on the unconstructed variant; adding a
/// `kind` field back is straightforward future work if a PCID-granularity
/// call site ever needs one.
#[repr(C, align(64))]
struct ShootdownRequest {
    target_mask: AtomicU64,
    remaining: AtomicU64,
    virt: u64,
}

/// Guards the read/write of the single in-flight shootdown request's data.
/// Released before IPIs are sent (see module docs) so [`shootdown_handler`]
/// can acquire it on the target CPU(s).
static PENDING_SHOOTDOWN: SpinLock<Option<ShootdownRequest>> = SpinLock::new(None);

/// Serializes [`shootdown_page`] end-to-end, system-wide. See module docs
/// for why this has to be a lock *separate* from `PENDING_SHOOTDOWN`'s own.
static SHOOTDOWN_SERIALIZE: SpinLock<()> = SpinLock::new(());

/// Pure mask arithmetic, split out of [`shootdown_page`] purely so it's
/// host-testable without touching real hardware state (reading the actual
/// CPU ID, executing `INVLPG`, or programming the local APIC — all of
/// which `shootdown_page` does and none of which are safe to run outside
/// `#[cfg(target_os = "none")]`). `shootdown_page`'s early return ("no
/// remote CPUs have this AddressSpace active, so return after the local
/// flush without ever touching `PENDING_SHOOTDOWN` or sending an IPI") is a
/// direct, unconditional consequence of this returning `0` — there is no
/// other code between that check and the lock/`send_ipi` calls. True
/// end-to-end verification that no IPI is actually sent needs multi-vCPU
/// QEMU; out of scope for a host unit test (see this function's own tests
/// below).
fn remote_targets(active_mask: u64, self_id: u32) -> u64 {
    active_mask & !(1u64 << (self_id % MAX_SHOOTDOWN_CPUS))
}

/// Broadcast a page invalidation to every CPU in `active_mask` (excluding
/// self — self invalidates locally, synchronously, first, since that's
/// free and needs no round-trip confirmation).
///
/// # Safety
/// `virt` must be an address that was actually unmapped/remapped by the
/// caller immediately before this call — this function only invalidates
/// cached translations, it never mutates any page table itself.
pub unsafe fn shootdown_page(active_mask: u64, virt: u64) {
    let self_id = kernel_cpu::current_cpu_id();
    // SAFETY: forwarded from this function's own precondition.
    unsafe { kernel_paging::flush_page(VirtAddr::new(virt)) };

    let remote_mask = remote_targets(active_mask, self_id);
    if remote_mask == 0 {
        return;
    }

    // Serialize with any other concurrent shootdown initiator system-wide
    // -- see this module's docs (SHOOTDOWN_SERIALIZE) for why this can't
    // just be PENDING_SHOOTDOWN's own lock.
    let _serialize_guard = SHOOTDOWN_SERIALIZE.lock();

    {
        let mut slot = PENDING_SHOOTDOWN.lock();
        *slot = Some(ShootdownRequest {
            target_mask: AtomicU64::new(remote_mask),
            remaining: AtomicU64::new(remote_mask.count_ones() as u64),
            virt,
        });
    } // lock released before sending IPIs -- shootdown_handler needs it too.

    for cpu in 0..MAX_SHOOTDOWN_CPUS {
        if remote_mask & (1u64 << cpu) != 0
            && let Some(apic_id) = kernel_smp::apic_id_for_cpu(cpu)
        {
            // SAFETY: kernel_apic is initialized by the time a second
            // CPU could possibly be running a CLONE_VM sibling thread;
            // `cpu` is an online CPU per `active_mask`, and
            // SHOOTDOWN_VECTOR is registered with shootdown_handler at
            // boot (see crate::init).
            unsafe {
                kernel_apic::send_ipi(
                    kernel_apic::IpiDest::ApicId(apic_id),
                    kernel_apic::IpiDelivery::Fixed(SHOOTDOWN_VECTOR),
                );
            }
        }
    }

    // send_ipi is asynchronous -- it returns before the target has
    // necessarily even received the interrupt, let alone run the handler.
    // This spin-wait on `remaining` is the actual synchronization point;
    // a production kernel would want a timeout/deadlock-detection net
    // here in case a target CPU is stuck with interrupts disabled -- this
    // checkpoint does not implement that (see "Known limitations").
    loop {
        let done = {
            let slot = PENDING_SHOOTDOWN.lock();
            slot.as_ref()
                .map(|r| r.remaining.load(Ordering::Acquire) == 0)
                .unwrap_or(true)
        };
        if done {
            break;
        }
        core::hint::spin_loop();
    }
}

/// IDT handler for [`SHOOTDOWN_VECTOR`], run on the *target* CPU.
/// Registered once at boot in `crate::init` via
/// `kernel_cpu::register_handler(SHOOTDOWN_VECTOR, shootdown_handler)`.
///
/// Runs with interrupts already disabled (it's an interrupt handler) — it
/// must not take the scheduler lock or call `schedule()`; it only touches
/// `PENDING_SHOOTDOWN`'s `SpinLock` and `kernel_paging::flush_page`, both
/// of which are safe from interrupt context (`flush_page` is just
/// `INVLPG`, with no hidden `IF`-dependent assumption).
pub fn shootdown_handler(_frame: &mut kernel_cpu::InterruptFrame, _vector: u8) {
    let cpu_id = kernel_cpu::current_cpu_id();
    let virt = {
        let slot = PENDING_SHOOTDOWN.lock();
        let my_bit = 1u64 << (cpu_id % MAX_SHOOTDOWN_CPUS);
        match slot.as_ref() {
            Some(req) if req.target_mask.load(Ordering::Acquire) & my_bit != 0 => Some(req.virt),
            _ => None, // spurious / not-for-us -- should not happen, but don't panic on it
        }
    };

    if let Some(virt) = virt {
        // SAFETY: the initiator already mutated the page-table entry for
        // `virt` before broadcasting; this only invalidates this CPU's
        // own cached translation for it.
        unsafe { kernel_paging::flush_page(VirtAddr::new(virt)) };
    }

    // ALWAYS eoi before decrementing/returning -- same ordering rule as
    // every other IRQ handler in this kernel (see kernel-apic's callers).
    kernel_apic::eoi();

    let slot = PENDING_SHOOTDOWN.lock();
    if let Some(req) = slot.as_ref() {
        req.remaining.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_cpu_mask_mark_active_sets_only_that_bit() {
        let mask = ActiveCpuMask::new();
        mask.mark_active(3);
        assert_eq!(mask.snapshot(), 1 << 3);
    }

    #[test]
    fn active_cpu_mask_mark_inactive_clears_only_that_bit() {
        let mask = ActiveCpuMask::new();
        mask.mark_active(3);
        mask.mark_active(5);
        mask.mark_inactive(3);
        assert_eq!(mask.snapshot(), 1 << 5);
    }

    #[test]
    fn active_cpu_mask_default_is_empty() {
        let mask = ActiveCpuMask::default();
        assert_eq!(mask.snapshot(), 0);
    }

    #[test]
    fn shootdown_request_remaining_starts_at_target_count() {
        let mask = 0b1011u64; // 3 bits set
        let req = ShootdownRequest {
            target_mask: AtomicU64::new(mask),
            remaining: AtomicU64::new(mask.count_ones() as u64),
            virt: 0x1000,
        };
        assert_eq!(req.remaining.load(Ordering::Relaxed), 3);
        assert_eq!(mask.count_ones(), 3);
    }

    #[test]
    fn remote_targets_excludes_self() {
        // self is bit 0 of a mask with bits 0 and 2 set -> only bit 2 remains.
        assert_eq!(remote_targets(0b101, 0), 0b100);
    }

    #[test]
    fn remote_targets_only_self_active_is_empty() {
        // This is exactly the condition shootdown_page's early return
        // checks: active_mask == (1 << self_cpu) only, no remote CPUs.
        assert_eq!(remote_targets(1 << 3, 3), 0);
    }

    #[test]
    fn remote_targets_self_not_in_mask_is_unaffected() {
        assert_eq!(remote_targets(0b110, 0), 0b110);
    }

    #[test]
    fn shootdown_vector_does_not_collide_with_timer_or_spurious() {
        assert_ne!(SHOOTDOWN_VECTOR, 0x20, "collides with TIMER_VECTOR");
        assert_ne!(SHOOTDOWN_VECTOR, 0xff, "collides with the spurious vector");
        assert_ne!(SHOOTDOWN_VECTOR, 14, "collides with PF_VECTOR");
    }
}
