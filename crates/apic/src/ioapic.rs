//! I/O APIC: routing legacy ISA device interrupts to CPU vectors.
//!
//! With the legacy 8259 PIC masked ([`crate::pic::disable_pic`]), the I/O
//! APIC is the only path by which a device interrupt — the PS/2 keyboard's
//! IRQ1 in particular — can reach a CPU at all.
//!
//! ## Where the topology comes from
//!
//! [`IoApicInfo`] and [`IsoInfo`] are filled in by the MADT walk in
//! `kernel_smp::acpi::parse_madt` (entry types 1 and 2). They are *defined*
//! here rather than there because `kernel-smp` already depends on
//! `kernel-apic`, so the dependency can only run in that direction.
//!
//! ## GSI vs. ISA IRQ
//!
//! Firmware is free to wire legacy ISA IRQ *n* to any Global System
//! Interrupt, and to change its polarity/trigger while doing so. An
//! Interrupt Source Override (MADT type 2) entry records that remapping.
//! [`resolve_isa_irq`] applies the override when one exists and falls back
//! to the ACPI-mandated ISA defaults (GSI == IRQ, active-high,
//! edge-triggered) when it does not — the usual case on QEMU's q35 for the
//! keyboard.
//!
//! ## MMIO
//!
//! The I/O APIC is reached through a two-register window (`IOREGSEL` /
//! `IOWIN`) at its physical base. That base sits in the PC MMIO hole, which
//! the Limine memory map does not report as usable RAM — `kernel_paging`'s
//! `map_apic_mmio` step maps it explicitly so `DIRECT_MAP_BASE + phys`
//! resolves here (the same addressing xAPIC MMIO uses).

/// Direct-map base; must match `kernel_paging`'s and `kernel_smp`'s.
/// Used by target MMIO glue and by a host test; dead on a plain host build.
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

/// `IOREGSEL` — write the index of the register to be accessed.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const IOREGSEL_OFF: usize = 0x00;
/// `IOWIN` — read/write the register selected by `IOREGSEL`.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const IOWIN_OFF: usize = 0x10;

/// Register 0x01: I/O APIC version; bits[23:16] hold (entry count - 1).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const IOAPIC_REG_VERSION: u8 = 0x01;
/// First redirection-table register. Each GSI occupies two consecutive
/// 32-bit registers, so GSI *n* starts at `0x10 + 2n`.
const IOAPIC_REG_REDIR_BASE: u8 = 0x10;

/// Interrupt vector this kernel routes the PS/2 keyboard's IRQ1 to.
///
/// 0x21 is the conventional "IRQ1" slot and is free here: 0x20 is
/// `crate::TIMER_VECTOR`, 0xfb/0xfc are the reschedule and TLB-shootdown
/// IPIs, 0xff is the LAPIC spurious vector, and the masked legacy PIC was
/// remapped clear to 0xE0-0xEF (`crate::pic`).
pub const KEYBOARD_VECTOR: u8 = 0x21;

/// Legacy ISA IRQ line the PS/2 keyboard sits on.
pub const ISA_IRQ_KEYBOARD: u8 = 1;

pub const MAX_IOAPICS: usize = 8;
pub const MAX_ISOS: usize = 16;

// ─── MADT-derived topology ────────────────────────────────────────────────

/// One I/O APIC, from MADT entry type 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicInfo {
    pub id: u8,
    /// Physical base of the MMIO window.
    pub phys_addr: u32,
    /// First Global System Interrupt this I/O APIC is responsible for.
    pub gsi_base: u32,
}

impl IoApicInfo {
    pub const fn zeroed() -> Self {
        Self {
            id: 0,
            phys_addr: 0,
            gsi_base: 0,
        }
    }
}

/// One Interrupt Source Override, from MADT entry type 2.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IsoInfo {
    /// Legacy ISA IRQ being overridden (bus 0).
    pub isa_irq: u8,
    /// The GSI it is actually wired to.
    pub gsi: u32,
    /// MPS INTI flags: bits[1:0] polarity, bits[3:2] trigger mode.
    pub flags: u16,
}

impl IsoInfo {
    pub const fn zeroed() -> Self {
        Self {
            isa_irq: 0,
            gsi: 0,
            flags: 0,
        }
    }
}

/// Fixed-capacity list of discovered I/O APICs (no allocator in the kernel).
#[derive(Clone, Copy)]
pub struct IoApicList {
    pub entries: [IoApicInfo; MAX_IOAPICS],
    pub count: usize,
}

impl IoApicList {
    pub const fn new() -> Self {
        Self {
            entries: [IoApicInfo::zeroed(); MAX_IOAPICS],
            count: 0,
        }
    }

    /// Append `info`, ignoring it if the fixed capacity is exhausted.
    pub fn push(&mut self, info: IoApicInfo) {
        if self.count < MAX_IOAPICS {
            self.entries[self.count] = info;
            self.count += 1;
        }
    }

    pub fn as_slice(&self) -> &[IoApicInfo] {
        &self.entries[..self.count]
    }
}

impl Default for IoApicList {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed-capacity list of Interrupt Source Overrides.
#[derive(Clone, Copy)]
pub struct IsoList {
    pub entries: [IsoInfo; MAX_ISOS],
    pub count: usize,
}

impl IsoList {
    pub const fn new() -> Self {
        Self {
            entries: [IsoInfo::zeroed(); MAX_ISOS],
            count: 0,
        }
    }

    pub fn push(&mut self, info: IsoInfo) {
        if self.count < MAX_ISOS {
            self.entries[self.count] = info;
            self.count += 1;
        }
    }

    pub fn as_slice(&self) -> &[IsoInfo] {
        &self.entries[..self.count]
    }

    /// The override for legacy ISA `irq`, if firmware published one.
    pub fn find(&self, irq: u8) -> Option<IsoInfo> {
        self.as_slice().iter().copied().find(|e| e.isa_irq == irq)
    }
}

impl Default for IsoList {
    fn default() -> Self {
        Self::new()
    }
}

// ─── pure helpers (host-testable: no MMIO, no unsafe) ─────────────────────

/// Register index of the LOW dword of GSI `gsi`'s redirection entry.
#[inline]
pub const fn redir_low_reg(gsi: u32) -> u8 {
    (IOAPIC_REG_REDIR_BASE as u32 + 2 * gsi) as u8
}

/// Register index of the HIGH dword of GSI `gsi`'s redirection entry.
#[inline]
pub const fn redir_high_reg(gsi: u32) -> u8 {
    (IOAPIC_REG_REDIR_BASE as u32 + 2 * gsi + 1) as u8
}

/// Number of redirection entries encoded in an I/O APIC version register:
/// bits[23:16] hold *one less than* the entry count.
#[inline]
pub const fn redir_entry_count(version_reg: u32) -> u32 {
    ((version_reg >> 16) & 0xFF) + 1
}

/// Decode MPS INTI flags into `(active_low, level_triggered)`.
///
/// Polarity is bits[1:0] and trigger mode bits[3:2]; the value `0` means
/// "conforms to the bus", which for the ISA bus means active-high and
/// edge-triggered — the same as the explicit encodings 1 and 1.
#[inline]
pub const fn decode_iso_flags(flags: u16) -> (bool, bool) {
    let active_low = (flags & 0b11) == 0b11;
    let level_triggered = ((flags >> 2) & 0b11) == 0b11;
    (active_low, level_triggered)
}

/// Resolve a legacy ISA IRQ to `(gsi, active_low, level_triggered)`.
///
/// Applies an Interrupt Source Override when firmware published one for
/// this IRQ; otherwise falls back to the ACPI ISA defaults (GSI == IRQ,
/// active-high, edge-triggered).
pub fn resolve_isa_irq(irq: u8, isos: &IsoList) -> (u32, bool, bool) {
    match isos.find(irq) {
        Some(iso) => {
            let (active_low, level) = decode_iso_flags(iso.flags);
            (iso.gsi, active_low, level)
        }
        None => (irq as u32, false, false),
    }
}

/// `true` if `gsi` falls within the range this I/O APIC serves, given how
/// many redirection entries it reported.
#[inline]
pub const fn owns_gsi(info: &IoApicInfo, entry_count: u32, gsi: u32) -> bool {
    gsi >= info.gsi_base && gsi < info.gsi_base + entry_count
}

/// Build the LOW dword of a redirection entry.
///
/// Fixed delivery mode (000), physical destination mode (0), unmasked.
#[inline]
pub const fn redir_low_value(vector: u8, active_low: bool, level_triggered: bool) -> u32 {
    let mut low = vector as u32; // bits[7:0]; delivery mode 000, dest mode 0
    if active_low {
        low |= 1 << 13;
    }
    if level_triggered {
        low |= 1 << 15;
    }
    low // bit 16 (mask) left clear = enabled
}

/// Build the HIGH dword: destination APIC ID in bits[31:24].
#[inline]
pub const fn redir_high_value(dest_apic_id: u32) -> u32 {
    (dest_apic_id & 0xFF) << 24
}

// ─── MMIO access (target-only) ────────────────────────────────────────────

/// Kernel virtual address of an I/O APIC's MMIO window.
/// Used by target MMIO glue and by a host test; dead on a plain host build.
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
#[inline]
const fn ioapic_virt(phys_addr: u32) -> usize {
    DIRECT_MAP_BASE.wrapping_add(phys_addr as u64) as usize
}

/// # Safety
/// `ioapic_virt` must be the mapped MMIO base of a real I/O APIC and `reg`
/// a valid register index for it.
#[cfg(target_os = "none")]
unsafe fn ioapic_read(ioapic_virt: usize, reg: u8) -> u32 {
    // SAFETY: caller guarantees the window is mapped; IOREGSEL selects the
    // register and IOWIN then exposes its 32-bit value. Both accesses must
    // be volatile — they are device registers, not memory.
    unsafe {
        core::ptr::write_volatile((ioapic_virt + IOREGSEL_OFF) as *mut u32, reg as u32);
        core::ptr::read_volatile((ioapic_virt + IOWIN_OFF) as *const u32)
    }
}

/// # Safety
/// Same contract as [`ioapic_read`].
#[cfg(target_os = "none")]
unsafe fn ioapic_write(ioapic_virt: usize, reg: u8, val: u32) {
    // SAFETY: as above; IOREGSEL then IOWIN, both volatile.
    unsafe {
        core::ptr::write_volatile((ioapic_virt + IOREGSEL_OFF) as *mut u32, reg as u32);
        core::ptr::write_volatile((ioapic_virt + IOWIN_OFF) as *mut u32, val);
    }
}

/// Find the I/O APIC that owns `gsi`, consulting each one's reported
/// redirection-entry count rather than assuming a single I/O APIC.
///
/// # Safety
/// Every entry's MMIO window must be mapped (see the module docs).
#[cfg(target_os = "none")]
unsafe fn ioapic_for_gsi(gsi: u32, ioapics: &IoApicList) -> Option<IoApicInfo> {
    for info in ioapics.as_slice() {
        let virt = ioapic_virt(info.phys_addr);
        // SAFETY: caller guarantees this window is mapped.
        let version = unsafe { ioapic_read(virt, IOAPIC_REG_VERSION) };
        if owns_gsi(info, redir_entry_count(version), gsi) {
            return Some(*info);
        }
    }
    None
}

/// Route `gsi` to `vector` on `dest_apic_id`, unmasked.
///
/// Returns `false` if no discovered I/O APIC claims that GSI.
///
/// # Safety
/// Must run at boot on the BSP with interrupts disabled, after the I/O APIC
/// MMIO windows are mapped and before the vector's handler can fire.
#[cfg(target_os = "none")]
pub unsafe fn route_irq(
    gsi: u32,
    vector: u8,
    dest_apic_id: u32,
    active_low: bool,
    level_triggered: bool,
    ioapics: &IoApicList,
) -> bool {
    // SAFETY: forwards this function's own preconditions.
    let Some(info) = (unsafe { ioapic_for_gsi(gsi, ioapics) }) else {
        return false;
    };
    let virt = ioapic_virt(info.phys_addr);
    let index = gsi - info.gsi_base;

    // SAFETY: `index` is within this I/O APIC's redirection table (checked
    // by ioapic_for_gsi). Destination is written BEFORE the low dword so
    // the entry is never briefly unmasked while pointing at APIC ID 0.
    unsafe {
        ioapic_write(virt, redir_high_reg(index), redir_high_value(dest_apic_id));
        ioapic_write(
            virt,
            redir_low_reg(index),
            redir_low_value(vector, active_low, level_triggered),
        );
    }
    true
}

/// Route the PS/2 keyboard's ISA IRQ1 to [`KEYBOARD_VECTOR`] on the BSP.
///
/// Returns `false` if the IRQ could not be routed (no I/O APIC covering its
/// GSI), which the caller should report rather than treat as fatal — the
/// system still boots, it just has no keyboard.
///
/// # Safety
/// Same context requirements as [`route_irq`].
#[cfg(target_os = "none")]
pub unsafe fn init_keyboard_irq_routing(
    bsp_apic_id: u32,
    ioapics: &IoApicList,
    isos: &IsoList,
) -> bool {
    let (gsi, active_low, level) = resolve_isa_irq(ISA_IRQ_KEYBOARD, isos);
    // SAFETY: forwards this function's own preconditions.
    unsafe { route_irq(gsi, KEYBOARD_VECTOR, bsp_apic_id, active_low, level, ioapics) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirection_register_math_is_two_per_gsi() {
        // The classic off-by-one source: entry n starts at 0x10 + 2n.
        assert_eq!(redir_low_reg(0), 0x10);
        assert_eq!(redir_high_reg(0), 0x11);
        assert_eq!(redir_low_reg(1), 0x12);
        assert_eq!(redir_high_reg(1), 0x13);
        assert_eq!(redir_low_reg(2), 0x14);
        assert_eq!(redir_low_reg(23), 0x10 + 46);
    }

    #[test]
    fn version_register_entry_count_is_one_more_than_encoded() {
        // QEMU's q35 I/O APIC reports 23 (0x17) => 24 entries.
        assert_eq!(redir_entry_count(0x0017_0011), 24);
        assert_eq!(redir_entry_count(0x0000_0011), 1);
    }

    #[test]
    fn iso_flags_default_to_isa_conventions() {
        // 0 = "conforms to bus specification" => ISA: active high, edge.
        assert_eq!(decode_iso_flags(0), (false, false));
    }

    #[test]
    fn iso_flags_decode_active_low_and_level() {
        // polarity=11 (active low), trigger=11 (level) => 0b1111.
        assert_eq!(decode_iso_flags(0b1111), (true, true));
        // polarity=01 (active high), trigger=01 (edge).
        assert_eq!(decode_iso_flags(0b0101), (false, false));
        // Active low, still edge triggered.
        assert_eq!(decode_iso_flags(0b0011), (true, false));
    }

    #[test]
    fn isa_irq_without_override_uses_identity_mapping() {
        let isos = IsoList::new();
        // No overrides at all: keyboard stays on GSI 1, active high, edge.
        assert_eq!(resolve_isa_irq(ISA_IRQ_KEYBOARD, &isos), (1, false, false));
    }

    #[test]
    fn isa_irq_with_override_follows_the_override() {
        let mut isos = IsoList::new();
        // Firmware moved the timer to GSI 2 (the common real override)...
        isos.push(IsoInfo {
            isa_irq: 0,
            gsi: 2,
            flags: 0,
        });
        // ...and, hypothetically, the keyboard to GSI 17, active low/level.
        isos.push(IsoInfo {
            isa_irq: 1,
            gsi: 17,
            flags: 0b1111,
        });
        assert_eq!(resolve_isa_irq(1, &isos), (17, true, true));
        assert_eq!(resolve_isa_irq(0, &isos), (2, false, false));
        // An IRQ with no override still falls back to identity.
        assert_eq!(resolve_isa_irq(4, &isos), (4, false, false));
    }

    #[test]
    fn gsi_ownership_respects_base_and_entry_count() {
        let info = IoApicInfo {
            id: 0,
            phys_addr: 0xFEC0_0000,
            gsi_base: 0,
        };
        assert!(owns_gsi(&info, 24, 0));
        assert!(owns_gsi(&info, 24, 23));
        assert!(!owns_gsi(&info, 24, 24));

        // A second I/O APIC starting at GSI 24 owns the next block.
        let second = IoApicInfo {
            id: 1,
            phys_addr: 0xFEC1_0000,
            gsi_base: 24,
        };
        assert!(!owns_gsi(&second, 24, 23));
        assert!(owns_gsi(&second, 24, 24));
    }

    #[test]
    fn redirection_low_dword_encodes_vector_and_is_unmasked() {
        let low = redir_low_value(KEYBOARD_VECTOR, false, false);
        assert_eq!(low & 0xFF, KEYBOARD_VECTOR as u32, "vector in bits[7:0]");
        assert_eq!(low & (1 << 16), 0, "entry must be unmasked");
        assert_eq!(low & (0b111 << 8), 0, "fixed delivery mode");
        assert_eq!(low & (1 << 11), 0, "physical destination mode");
        assert_eq!(low & (1 << 13), 0, "active high");
        assert_eq!(low & (1 << 15), 0, "edge triggered");
    }

    #[test]
    fn redirection_low_dword_sets_polarity_and_trigger_bits() {
        let low = redir_low_value(0x21, true, true);
        assert_ne!(low & (1 << 13), 0, "active low sets bit 13");
        assert_ne!(low & (1 << 15), 0, "level triggered sets bit 15");
    }

    #[test]
    fn redirection_high_dword_puts_apic_id_in_top_byte() {
        assert_eq!(redir_high_value(0), 0);
        assert_eq!(redir_high_value(3), 3 << 24);
        // Only the low 8 bits of the APIC ID fit the physical-mode field.
        assert_eq!(redir_high_value(0x1FF), 0xFF << 24);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // documents an invariant as a named test
    fn keyboard_vector_does_not_collide_with_other_kernel_vectors() {
        // Every vector this kernel installs a handler for, plus the LAPIC
        // spurious vector. A collision here silently steals interrupts.
        const TIMER_VECTOR: u8 = crate::TIMER_VECTOR;
        const RESCHED_VECTOR: u8 = 0xfb;
        const SHOOTDOWN_VECTOR: u8 = 0xfc;
        const SPURIOUS_VECTOR: u8 = 0xff;
        const SYSCALL_INT: u8 = 0x80;

        for other in [
            TIMER_VECTOR,
            RESCHED_VECTOR,
            SHOOTDOWN_VECTOR,
            SPURIOUS_VECTOR,
            SYSCALL_INT,
        ] {
            assert_ne!(KEYBOARD_VECTOR, other, "vector collision at {other:#x}");
        }
        // Must also sit above the CPU exception range.
        assert!(KEYBOARD_VECTOR > 0x1F);
    }

    #[test]
    fn lists_respect_their_fixed_capacity() {
        let mut ioapics = IoApicList::new();
        for i in 0..(MAX_IOAPICS + 3) {
            ioapics.push(IoApicInfo {
                id: i as u8,
                phys_addr: 0xFEC0_0000,
                gsi_base: 0,
            });
        }
        assert_eq!(ioapics.count, MAX_IOAPICS, "must not overflow capacity");

        let mut isos = IsoList::new();
        for i in 0..(MAX_ISOS + 3) {
            isos.push(IsoInfo {
                isa_irq: i as u8,
                gsi: i as u32,
                flags: 0,
            });
        }
        assert_eq!(isos.count, MAX_ISOS);
    }

    #[test]
    fn ioapic_mmio_window_lands_in_the_direct_map() {
        // QEMU q35's I/O APIC base; must resolve through the direct map,
        // which kernel_paging::map_apic_mmio is responsible for mapping.
        assert_eq!(
            ioapic_virt(0xFEC0_0000),
            (0xffff_8000_0000_0000u64 + 0xFEC0_0000) as usize
        );
    }
}
