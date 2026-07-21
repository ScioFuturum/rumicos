use core::hint::spin_loop;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use kernel_arch_x86_64::detect_cpu_features;
use kernel_arch_x86_64::msr::{IA32_APIC_BASE, rdmsr, wrmsr};

const APIC_BASE_GLOBAL_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const APIC_BASE_PHYS_MASK: u64 = 0x000f_ffff_ffff_f000;

const DIRECT_MAP_BASE_4L: u64 = 0xffff_8000_0000_0000;
const DIRECT_MAP_BASE_5L: u64 = 0xff00_0000_0000_0000;

const X2APIC_EOI: u32 = 0x80b;
const X2APIC_ID: u32 = 0x802;
const X2APIC_ICR: u32 = 0x830;
const X2APIC_TPR: u32 = 0x808;
const X2APIC_SIVR: u32 = 0x80f;
const X2APIC_LVT_TIMER: u32 = 0x832;
const X2APIC_TIMER_INITIAL_COUNT: u32 = 0x838;
const X2APIC_TIMER_CURRENT_COUNT: u32 = 0x839;
const X2APIC_TIMER_DIVIDE: u32 = 0x83e;

const XAPIC_ID_OFF: usize = 0x020;
const XAPIC_VERSION_OFF: usize = 0x030;
const XAPIC_TPR_OFF: usize = 0x080;
const XAPIC_APR_OFF: usize = 0x090;
const XAPIC_PPR_OFF: usize = 0x0a0;
const XAPIC_EOI_OFF: usize = 0x0b0;
const XAPIC_LDR_OFF: usize = 0x0d0;
const XAPIC_DFR_OFF: usize = 0x0e0;
const XAPIC_SPURIOUS_OFF: usize = 0x0f0;
const XAPIC_ISR0_OFF: usize = 0x100;
const XAPIC_TMR0_OFF: usize = 0x180;
const XAPIC_IRR0_OFF: usize = 0x200;
const XAPIC_ESR_OFF: usize = 0x280;
const XAPIC_LVT_CMCI_OFF: usize = 0x2f0;
const XAPIC_ICR_LO_OFF: usize = 0x300;
const XAPIC_ICR_HI_OFF: usize = 0x310;
const XAPIC_LVT_TIMER_OFF: usize = 0x320;
const XAPIC_LVT_THERMAL_OFF: usize = 0x330;
const XAPIC_LVT_PERF_OFF: usize = 0x340;
const XAPIC_LVT_LINT0_OFF: usize = 0x350;
const XAPIC_LVT_LINT1_OFF: usize = 0x360;
const XAPIC_LVT_ERROR_OFF: usize = 0x370;
const XAPIC_TIMER_ICR_OFF: usize = 0x380;
const XAPIC_TIMER_CCR_OFF: usize = 0x390;
const XAPIC_TIMER_DCR_OFF: usize = 0x3e0;

pub const LVT_MASKED: u32 = 1 << 16;
pub const LVT_TIMER_PERIODIC: u32 = 1 << 17;
pub const LVT_TIMER_ONESHOT: u32 = 0 << 17;

const SPURIOUS_ENABLE: u32 = 1 << 8;
const SPURIOUS_VECTOR: u32 = 0xff;
const ICR_DELIVERY_PENDING: u32 = 1 << 12;
const ICR_DELIVERY_INIT: u64 = 0b101 << 8;
const ICR_DELIVERY_STARTUP: u64 = 0b110 << 8;
const ICR_LEVEL_ASSERT: u64 = 1 << 14;
const ICR_TRIGGER_LEVEL: u64 = 1 << 15;
const ICR_DEST_SHIFT: u32 = 32;
const IPI_POST_DELAY_SPINS: usize = 10_000;

const APIC_MODE_UNINITIALIZED: u8 = 0;
const APIC_MODE_XAPIC: u8 = 1;
const APIC_MODE_X2APIC: u8 = 2;

static APIC_MODE: AtomicU8 = AtomicU8::new(APIC_MODE_UNINITIALIZED);
static XAPIC_VIRT_BASE: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Eq, PartialEq)]
enum ApicMode {
    XApic,
    X2Apic,
}

pub fn init_bsp() {
    let _mode = init_lapic();
    // SAFETY: local APIC was enabled above on the boot CPU; masking the timer
    // leaves it quiescent until timer calibration arms it explicitly.
    unsafe {
        timer_set_lvt(LVT_MASKED);
        timer_set_initial_count(0);
    }
}

pub fn init_ap() {
    let _mode = init_lapic();
    // SAFETY: each AP initializes its own local APIC timer state before
    // interrupts are enabled on that CPU.
    unsafe {
        timer_set_lvt(LVT_MASKED);
        timer_set_initial_count(0);
    }
}

/// Initialize the local APIC in the fastest available mode.
///
/// xAPIC availability:
/// - QEMU default (-cpu qemu64): x2APIC disabled -> exercises xAPIC fallback
/// - QEMU with (-cpu host, KVM):  x2APIC usually enabled -> exercises x2APIC path
/// - Real hardware (Intel Nehalem+): x2APIC available
/// - Real hardware (AMD Phenom): no x2APIC -> xAPIC fallback
///
/// To test xAPIC path in QEMU without KVM:
///   qemu-system-x86_64 -cpu qemu64 -machine q35 -m 512M ...
/// To test x2APIC path:
///   qemu-system-x86_64 -cpu host -enable-kvm -machine q35 -m 512M ...
fn init_lapic() -> ApicMode {
    let features = detect_cpu_features();
    // SAFETY: IA32_APIC_BASE exists on x86_64 CPUs with local APIC support and
    // bootstrap/AP init runs at CPL0.
    let apic_base = unsafe { rdmsr(IA32_APIC_BASE) };
    let phys = xapic_base_from_msr(apic_base);
    XAPIC_VIRT_BASE.store(xapic_virt_from_phys(phys, features.la57), Ordering::Release);

    if features.x2apic {
        // SAFETY: setting ENABLE|X2APIC on IA32_APIC_BASE enables MSR-backed
        // local APIC access on the current CPU.
        unsafe {
            wrmsr(
                IA32_APIC_BASE,
                apic_base | APIC_BASE_GLOBAL_ENABLE | APIC_BASE_X2APIC,
            )
        };
        // SOFTWARE-enable the LAPIC (SVR bit 8) and open the task-priority
        // gate, exactly as the xAPIC branch below always did. This was
        // missing here: the BSP worked anyway because UEFI firmware leaves
        // its LAPIC software-enabled, but an AP arrives from INIT/SIPI with
        // SVR.enable = 0 — a software-DISABLED APIC, which still accepts
        // INIT/SIPI (so bring-up "worked") but silently swallows every
        // fixed-vector interrupt: the armed LAPIC timer never fired and
        // reschedule IPIs never arrived, so an AP never drained its run
        // queue — a pipeline stage enqueued there starved forever.
        // SAFETY: x2APIC MSR space is active per the wrmsr above; TPR=0
        // accepts all priorities, SIVR sets enable + spurious vector.
        unsafe {
            wrmsr(X2APIC_TPR, 0);
            wrmsr(X2APIC_SIVR, (SPURIOUS_ENABLE | SPURIOUS_VECTOR) as u64);
        }
        APIC_MODE.store(APIC_MODE_X2APIC, Ordering::Release);
        ApicMode::X2Apic
    } else {
        // SAFETY: this keeps bit 10 clear and globally enables legacy xAPIC
        // MMIO mode on the current CPU.
        unsafe {
            wrmsr(
                IA32_APIC_BASE,
                (apic_base | APIC_BASE_GLOBAL_ENABLE) & !APIC_BASE_X2APIC,
            )
        };

        // SAFETY: the xAPIC MMIO page was cached above and must be mapped by
        // the direct map before the fallback path is used.
        unsafe {
            xapic_write(XAPIC_ESR_OFF, 0);
            xapic_write(XAPIC_ESR_OFF, 0);
            xapic_write(XAPIC_TPR_OFF, 0);
            xapic_write(XAPIC_DFR_OFF, 0xffff_ffff);
            xapic_write(XAPIC_LDR_OFF, 0);
            xapic_write(XAPIC_SPURIOUS_OFF, SPURIOUS_ENABLE | SPURIOUS_VECTOR);
            xapic_write(XAPIC_LVT_THERMAL_OFF, LVT_MASKED);
            xapic_write(XAPIC_LVT_PERF_OFF, LVT_MASKED);
            xapic_write(XAPIC_LVT_LINT0_OFF, LVT_MASKED);
            xapic_write(XAPIC_LVT_LINT1_OFF, LVT_MASKED);
            xapic_write(XAPIC_LVT_ERROR_OFF, LVT_MASKED);
            xapic_write(XAPIC_LVT_TIMER_OFF, LVT_MASKED);
        }

        APIC_MODE.store(APIC_MODE_XAPIC, Ordering::Release);
        ApicMode::XApic
    }
}

#[inline(always)]
pub fn apic_id() -> u32 {
    match apic_mode() {
        ApicMode::X2Apic => {
            // SAFETY: x2APIC mode is active for the current CPU.
            unsafe { rdmsr(X2APIC_ID) as u32 }
        }
        ApicMode::XApic => {
            // SAFETY: xAPIC MMIO is initialized and mapped.
            let id = unsafe { xapic_read(XAPIC_ID_OFF) };
            apic_id_from_xapic_reg(id)
        }
    }
}

#[inline(always)]
pub fn eoi() {
    match apic_mode() {
        ApicMode::X2Apic => {
            // SAFETY: x2APIC EOI is a write-only MSR. Writing zero acknowledges
            // the in-service interrupt for the current local APIC.
            unsafe { wrmsr(X2APIC_EOI, 0) };
        }
        ApicMode::XApic => {
            // SAFETY: xAPIC MMIO is initialized and mapped; writing zero to EOI
            // acknowledges the in-service interrupt.
            unsafe { xapic_write(XAPIC_EOI_OFF, 0) };
        }
    }
}

/// Program the LAPIC timer LVT register.
///
/// # Safety
/// Caller must ensure the local APIC is initialized and `value` is a valid LVT
/// timer encoding for the current CPU.
#[inline(always)]
pub unsafe fn timer_set_lvt(value: u32) {
    match apic_mode() {
        ApicMode::X2Apic => {
            // SAFETY: caller guarantees the APIC mode is live and the LVT value
            // is architecturally valid.
            unsafe { wrmsr(X2APIC_LVT_TIMER, value as u64) };
        }
        ApicMode::XApic => {
            // SAFETY: caller guarantees xAPIC MMIO is initialized and mapped.
            unsafe { xapic_write(XAPIC_LVT_TIMER_OFF, value) };
        }
    }
}

/// Program the LAPIC timer divide configuration register.
///
/// # Safety
/// Caller must ensure the local APIC is initialized and `value` uses the
/// architectural divide-register encoding.
#[inline(always)]
pub unsafe fn timer_set_divide(value: u32) {
    match apic_mode() {
        ApicMode::X2Apic => {
            // SAFETY: caller guarantees the APIC mode is live and `value` is a
            // valid DCR encoding.
            unsafe { wrmsr(X2APIC_TIMER_DIVIDE, value as u64) };
        }
        ApicMode::XApic => {
            // SAFETY: caller guarantees xAPIC MMIO is initialized and mapped.
            unsafe { xapic_write(XAPIC_TIMER_DCR_OFF, value) };
        }
    }
}

/// Program the LAPIC timer initial count register.
///
/// # Safety
/// Caller must ensure the local APIC is initialized. Writing this register
/// starts the local timer countdown immediately.
#[inline(always)]
pub unsafe fn timer_set_initial_count(value: u32) {
    match apic_mode() {
        ApicMode::X2Apic => {
            // SAFETY: caller guarantees the APIC mode is live; any u32 value is
            // valid for the initial-count register.
            unsafe { wrmsr(X2APIC_TIMER_INITIAL_COUNT, value as u64) };
        }
        ApicMode::XApic => {
            // SAFETY: caller guarantees xAPIC MMIO is initialized and mapped.
            unsafe { xapic_write(XAPIC_TIMER_ICR_OFF, value) };
        }
    }
}

/// Read the LAPIC timer current count register.
///
/// # Safety
/// Caller must ensure the local APIC is initialized.
#[inline(always)]
pub unsafe fn timer_current_count() -> u32 {
    match apic_mode() {
        ApicMode::X2Apic => {
            // SAFETY: caller guarantees the APIC mode is live.
            unsafe { rdmsr(X2APIC_TIMER_CURRENT_COUNT) as u32 }
        }
        ApicMode::XApic => {
            // SAFETY: caller guarantees xAPIC MMIO is initialized and mapped.
            unsafe { xapic_read(XAPIC_TIMER_CCR_OFF) }
        }
    }
}

/// Write the local APIC interrupt command register.
///
/// # Safety
/// Caller must ensure the ICR value is valid for the active APIC mode and that
/// the target APIC ID is valid for the running topology.
#[inline(always)]
pub unsafe fn write_icr(icr: u64) {
    match apic_mode() {
        ApicMode::X2Apic => {
            // SAFETY: caller guarantees x2APIC mode is active and `icr` is a
            // valid interrupt-command value.
            unsafe { wrmsr(X2APIC_ICR, icr) };
        }
        ApicMode::XApic => {
            let (hi, lo) = split_icr_xapic(icr);
            // SAFETY: caller guarantees xAPIC MMIO is initialized and mapped.
            unsafe {
                xapic_write(XAPIC_ICR_HI_OFF, hi);
                xapic_write(XAPIC_ICR_LO_OFF, lo);
                wait_icr_idle_xapic();
            }
        }
    }
}

#[inline(always)]
pub fn send_init(apic_id: u32) {
    let dest = icr_dest(apic_id);
    // SAFETY: physical destination mode INIT IPI is valid for AP startup.
    unsafe { write_icr(dest | ICR_DELIVERY_INIT | ICR_LEVEL_ASSERT | ICR_TRIGGER_LEVEL) };
    ipi_post_delay();
    // SAFETY: INIT level deassert completes the INIT sequence on xAPIC-era CPUs.
    unsafe { write_icr(dest | ICR_DELIVERY_INIT | ICR_TRIGGER_LEVEL) };
    ipi_post_delay();
}

#[inline(always)]
pub fn send_sipi(apic_id: u32, trampoline_page: u8) {
    let icr = icr_dest(apic_id) | ICR_DELIVERY_STARTUP | trampoline_page as u64;
    // SAFETY: SIPI vector is the low 8-bit page number below 1 MiB.
    unsafe { write_icr(icr) };
    ipi_post_delay();
}

#[inline(always)]
fn icr_dest(apic_id: u32) -> u64 {
    (apic_id as u64) << ICR_DEST_SHIFT
}

fn ipi_post_delay() {
    for _ in 0..IPI_POST_DELAY_SPINS {
        spin_loop();
    }
}

#[inline(always)]
fn apic_mode() -> ApicMode {
    match APIC_MODE.load(Ordering::Acquire) {
        APIC_MODE_XAPIC => ApicMode::XApic,
        APIC_MODE_X2APIC => ApicMode::X2Apic,
        _ => apic_not_initialized(),
    }
}

#[cold]
#[inline(never)]
fn apic_not_initialized() -> ! {
    panic!("local APIC not initialized");
}

#[inline(always)]
fn xapic_base_virt() -> usize {
    XAPIC_VIRT_BASE.load(Ordering::Acquire)
}

/// # Safety: xAPIC MMIO must be mapped (direct-map live), off must be valid.
#[inline(always)]
unsafe fn xapic_read(off: usize) -> u32 {
    let base = xapic_base_virt();
    debug_assert!(base != 0, "xAPIC not initialized");
    debug_assert!(xapic_offset_valid(off), "invalid xAPIC register offset");
    // SAFETY: caller guarantees the xAPIC MMIO page is mapped and `off` names a
    // valid 32-bit xAPIC register.
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

/// # Safety: xAPIC MMIO must be mapped (direct-map live), off must be valid.
#[inline(always)]
unsafe fn xapic_write(off: usize, val: u32) {
    let base = xapic_base_virt();
    debug_assert!(base != 0, "xAPIC not initialized");
    debug_assert!(xapic_offset_valid(off), "invalid xAPIC register offset");
    // SAFETY: caller guarantees the xAPIC MMIO page is mapped and `off` names a
    // valid 32-bit xAPIC register.
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) };
}

unsafe fn wait_icr_idle_xapic() {
    loop {
        // SAFETY: caller guarantees xAPIC MMIO is initialized and mapped.
        let lo = unsafe { xapic_read(XAPIC_ICR_LO_OFF) };
        if lo & ICR_DELIVERY_PENDING == 0 {
            break;
        }
        spin_loop();
    }
}

const fn xapic_base_from_msr(msr: u64) -> u64 {
    msr & APIC_BASE_PHYS_MASK
}

const fn xapic_virt_from_phys(phys: u64, la57: bool) -> usize {
    let base = if la57 {
        DIRECT_MAP_BASE_5L
    } else {
        DIRECT_MAP_BASE_4L
    };
    base.wrapping_add(phys) as usize
}

const fn apic_id_from_xapic_reg(value: u32) -> u32 {
    (value >> 24) & 0xff
}

const fn split_icr_xapic(icr: u64) -> (u32, u32) {
    let hi = (((icr >> 32) as u32) & 0xff) << 24;
    let lo = (icr & 0xffff_ffff) as u32;
    (hi, lo)
}

const fn xapic_offset_valid(off: usize) -> bool {
    matches!(
        off,
        XAPIC_ID_OFF
            | XAPIC_VERSION_OFF
            | XAPIC_TPR_OFF
            | XAPIC_APR_OFF
            | XAPIC_PPR_OFF
            | XAPIC_EOI_OFF
            | XAPIC_LDR_OFF
            | XAPIC_DFR_OFF
            | XAPIC_SPURIOUS_OFF
            | XAPIC_ISR0_OFF
            | XAPIC_TMR0_OFF
            | XAPIC_IRR0_OFF
            | XAPIC_ESR_OFF
            | XAPIC_LVT_CMCI_OFF
            | XAPIC_ICR_LO_OFF
            | XAPIC_ICR_HI_OFF
            | XAPIC_LVT_TIMER_OFF
            | XAPIC_LVT_THERMAL_OFF
            | XAPIC_LVT_PERF_OFF
            | XAPIC_LVT_LINT0_OFF
            | XAPIC_LVT_LINT1_OFF
            | XAPIC_LVT_ERROR_OFF
            | XAPIC_TIMER_ICR_OFF
            | XAPIC_TIMER_CCR_OFF
            | XAPIC_TIMER_DCR_OFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xapic_base_from_msr_extracts_phys_base() {
        assert_eq!(xapic_base_from_msr(0x0000_0000_fee0_0900), 0xfee0_0000);
    }

    #[test]
    fn apic_id_xapic_encoding_extracts_high_byte() {
        assert_eq!(apic_id_from_xapic_reg(0x0f00_0000), 0x0f);
    }

    #[test]
    fn write_icr_split_extracts_destination_and_command() {
        let (hi, lo) = split_icr_xapic(0x0000_000f_0004_4100);
        assert_eq!(hi, 0x0f00_0000);
        assert_eq!(lo, 0x0004_4100);
    }

    #[test]
    fn lvt_masked_bit_matches_arch_encoding() {
        assert_eq!(LVT_MASKED, 1 << 16);
    }

    #[test]
    fn lvt_timer_periodic_bit_matches_arch_encoding() {
        assert_eq!(LVT_TIMER_PERIODIC, 1 << 17);
    }

    #[test]
    fn spurious_enable_bit_matches_arch_encoding() {
        assert_eq!(SPURIOUS_ENABLE, 1 << 8);
    }
}
