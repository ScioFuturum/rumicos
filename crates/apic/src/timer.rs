use crate::lapic::{
    LVT_MASKED, LVT_TIMER_ONESHOT, LVT_TIMER_PERIODIC, timer_current_count, timer_set_divide,
    timer_set_initial_count, timer_set_lvt,
};
use crate::pit::{PIT_HZ, pit_prepare, pit_start, pit_wait};
use core::sync::atomic::{AtomicU64, Ordering};

pub static LAPIC_TICKS_PER_MS: AtomicU64 = AtomicU64::new(0);

pub const TIMER_VECTOR: u8 = 0x20;
pub const CALIBRATION_MS: u64 = 10;
pub const PIT_TICKS_10MS: u16 = ((PIT_HZ * CALIBRATION_MS) / 1000) as u16;

const LAPIC_DIVIDE_BY_1: u32 = 0x0b;
// Sanity bounds only — reject obviously-broken calibration (0, or garbage
// from a misprogrammed PIT), not legitimate hardware variation. QEMU's
// emulated LAPIC ticks at 1 GHz (1_000_000 ticks/ms), which the previous
// 25_000..=500_000 window rejected on the first real boot; physical CPUs
// with divide-by-1 can run the timer at the full bus clock (multi-GHz).
const MIN_LAPIC_TICKS_PER_MS: u64 = 1_000; // 1 MHz
const MAX_LAPIC_TICKS_PER_MS: u64 = 10_000_000; // 10 GHz

pub fn calibrate_timer() -> u64 {
    // SAFETY: calibration runs after local APIC initialization at CPL0 with
    // interrupts disabled; PIT channel 2 is used in polling mode and does not
    // require IRQ routing.
    unsafe {
        timer_set_divide(LAPIC_DIVIDE_BY_1);
        timer_set_lvt(LVT_TIMER_ONESHOT | LVT_MASKED | TIMER_VECTOR as u32);
        timer_set_initial_count(u32::MAX);

        pit_prepare(PIT_TICKS_10MS);
        let start_lapic = timer_current_count();
        pit_start();
        let _spins = pit_wait();
        let end_lapic = timer_current_count();

        let elapsed = start_lapic.wrapping_sub(end_lapic) as u64;
        let ticks_per_ms = elapsed / CALIBRATION_MS;
        if !(MIN_LAPIC_TICKS_PER_MS..=MAX_LAPIC_TICKS_PER_MS).contains(&ticks_per_ms) {
            panic!("LAPIC calibration failed: {}", ticks_per_ms);
        }

        LAPIC_TICKS_PER_MS.store(ticks_per_ms, Ordering::Release);
        ticks_per_ms
    }
}

/// Program the LAPIC timer to fire once after `ms` milliseconds.
///
/// # Panics
/// Panics if `calibrate_timer()` has not populated the LAPIC rate.
pub fn set_timer_oneshot(ms: u64) {
    let ticks_per_ms = LAPIC_TICKS_PER_MS.load(Ordering::Acquire);
    assert!(
        ticks_per_ms > 0,
        "LAPIC not calibrated - call calibrate_timer() first"
    );

    let count = timer_count_for_ms(ticks_per_ms, ms);

    // SAFETY: LAPIC initialization is required before this public API is used;
    // two MSR writes are the complete hot path for one-shot programming.
    unsafe {
        timer_set_lvt(LVT_TIMER_ONESHOT | TIMER_VECTOR as u32);
        timer_set_initial_count(count);
    }
}

/// Program the LAPIC timer in periodic mode at `hz` interrupts per second.
///
/// # Panics
/// Panics if the timer is not calibrated or if `hz` is outside 1..=10000.
pub fn set_timer_periodic(hz: u64) {
    let ticks_per_ms = LAPIC_TICKS_PER_MS.load(Ordering::Acquire);
    assert!(ticks_per_ms > 0, "LAPIC not calibrated");
    assert!(hz > 0 && hz <= 10_000, "hz out of range 1..=10000");

    let ticks_per_period = ticks_per_ms.saturating_mul(1000) / hz;
    let count = ticks_per_period.min(u32::MAX as u64) as u32;

    // SAFETY: LAPIC initialization is required before this public API is used;
    // the LVT periodic bit plus initial count fully arms the periodic timer.
    unsafe {
        timer_set_lvt(LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
        timer_set_initial_count(count);
    }
}

pub fn ticks_per_ms() -> u64 {
    LAPIC_TICKS_PER_MS.load(Ordering::Relaxed)
}

pub fn ns_to_ticks(ns: u64) -> u64 {
    let ticks_per_ms = LAPIC_TICKS_PER_MS.load(Ordering::Relaxed);
    if ticks_per_ms == 0 {
        return 0;
    }
    (ns as u128 * ticks_per_ms as u128 / 1_000_000) as u64
}

#[inline(always)]
fn timer_count_for_ms(ticks_per_ms: u64, ms: u64) -> u32 {
    ticks_per_ms.saturating_mul(ms).min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pit_ticks_10ms_is_expected() {
        assert!((11931..=11932).contains(&PIT_TICKS_10MS));
    }

    #[test]
    fn ns_to_ticks_uses_ticks_per_millisecond() {
        LAPIC_TICKS_PER_MS.store(100_000, Ordering::Relaxed);
        assert_eq!(ns_to_ticks(1_000), 100);
        assert_eq!(ns_to_ticks(500), 50);
        assert_eq!(ns_to_ticks(1_000_000), 100_000);
    }

    #[test]
    fn set_timer_oneshot_overflow_guard_clamps() {
        assert_eq!(timer_count_for_ms(100_000, u64::MAX), u32::MAX);
    }

    #[test]
    fn lvt_timer_periodic_bit_matches_arch_encoding() {
        assert_eq!(LVT_TIMER_PERIODIC, 1 << 17);
    }
}
