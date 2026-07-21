//! Modern (1.0+) virtio-over-PCI transport: capability discovery, the common
//! configuration structure, and the device initialization handshake.
//!
//! Modern virtio exposes its control surface through PCI vendor-specific
//! capabilities (ID `0x09`), each naming a BAR + offset where one structure
//! lives. All of those structures are MMIO: every access here is a
//! `read_volatile`/`write_volatile` of a single field at a computed address —
//! never an aggregate struct load/store, a rule this project holds to after
//! three miscompiled aggregate copies.

use kernel_pci::PciDevice;
use kernel_pci::config::pci_config_read_u8;

/// Direct-map base; must match kernel-paging / kernel-proc.
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

/// virtio PCI vendor-specific capability ID.
pub const VIRTIO_PCI_CAP_VENDOR: u8 = 0x09;

// cfg_type values inside a vendor cap.
pub const CFG_TYPE_COMMON: u8 = 1;
pub const CFG_TYPE_NOTIFY: u8 = 2;
pub const CFG_TYPE_ISR: u8 = 3;
pub const CFG_TYPE_DEVICE: u8 = 4;
pub const CFG_TYPE_PCI: u8 = 5;

// ─── device status bits ─────────────────────────────────────────────────────
pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;
pub const STATUS_NEEDS_RESET: u8 = 64;
pub const STATUS_FAILED: u8 = 128;

// ─── feature bits we negotiate ──────────────────────────────────────────────
/// `VIRTIO_NET_F_MAC` — device supplies a MAC address. Feature word 0, bit 5.
pub const VIRTIO_NET_F_MAC_BIT: u32 = 1 << 5;
/// `VIRTIO_F_VERSION_1` — modern device. Bit 32 = feature word 1, bit 0.
pub const VIRTIO_F_VERSION_1_BIT: u32 = 1 << 0;

// ─── common-configuration field offsets (VirtioPciCommonCfg) ────────────────
const CFG_DEVICE_FEATURE_SELECT: usize = 0;
const CFG_DEVICE_FEATURE: usize = 4;
const CFG_DRIVER_FEATURE_SELECT: usize = 8;
const CFG_DRIVER_FEATURE: usize = 12;
const CFG_NUM_QUEUES: usize = 18;
const CFG_DEVICE_STATUS: usize = 20;
const CFG_QUEUE_SELECT: usize = 22;
const CFG_QUEUE_SIZE: usize = 24;
const CFG_QUEUE_MSIX_VECTOR: usize = 26;
const CFG_QUEUE_ENABLE: usize = 28;
const CFG_QUEUE_NOTIFY_OFF: usize = 30;
const CFG_QUEUE_DESC: usize = 32;
const CFG_QUEUE_DRIVER: usize = 40;
const CFG_QUEUE_DEVICE: usize = 48;

// ─── pure helpers (host-testable) ───────────────────────────────────────────

/// Compute the modern-virtio structure's direct-map virtual address from the
/// BAR base physical address and the cap's byte offset.
#[inline]
pub const fn cfg_va(bar_phys: u64, offset: u32) -> usize {
    (DIRECT_MAP_BASE + bar_phys + offset as u64) as usize
}

/// The queue-notification address: `notify_base + queue_notify_off × mult`.
#[inline]
pub const fn notify_addr(notify_base_va: usize, queue_notify_off: u16, multiplier: u32) -> usize {
    notify_base_va + (queue_notify_off as usize) * (multiplier as usize)
}

/// Negotiate the minimal feature set from the device's two feature words.
///
/// Requests exactly `VIRTIO_NET_F_MAC` (word 0) and `VIRTIO_F_VERSION_1`
/// (word 1), and only if the device offers them. Returns
/// `(driver_word0, driver_word1)`.
#[inline]
pub const fn negotiate_features(device_lo: u32, device_hi: u32) -> (u32, u32) {
    (device_lo & VIRTIO_NET_F_MAC_BIT, device_hi & VIRTIO_F_VERSION_1_BIT)
}

/// `true` if the negotiated words include `VIRTIO_F_VERSION_1` — mandatory
/// for a modern device; if absent the device is legacy-only and we bail.
#[inline]
pub const fn has_version_1(driver_hi: u32) -> bool {
    driver_hi & VIRTIO_F_VERSION_1_BIT != 0
}

// ─── capability locations ───────────────────────────────────────────────────

/// One virtio structure's location: which BAR, byte offset, length, and (for
/// the notify cap only) the notify-offset multiplier.
#[derive(Clone, Copy, Default)]
pub struct CapLoc {
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
    pub notify_multiplier: u32,
    pub present: bool,
}

/// The virtio configuration structures a modern net driver needs.
#[derive(Clone, Copy, Default)]
pub struct VirtioCaps {
    pub common: CapLoc,
    pub notify: CapLoc,
    pub device: CapLoc,
}

/// Walk the PCI capability list and collect the virtio COMMON / NOTIFY /
/// DEVICE configuration structures (ISR and PCI-CFG are not needed with
/// MSI-X).
///
/// # Safety
/// Ring-0 port I/O against a real PCI function's config space.
pub unsafe fn walk_virtio_caps(dev: &PciDevice) -> VirtioCaps {
    let (bus, device, function) = (dev.bus, dev.device, dev.function);
    let mut caps = VirtioCaps::default();

    // Capability list pointer at 0x34 (low byte, dword-aligned).
    // SAFETY: ring-0 config read.
    let mut ptr = unsafe { pci_config_read_u8(bus, device, function, 0x34) } & 0xfc;
    let mut guard = 0;
    while ptr != 0 && guard < 48 {
        // SAFETY: ring-0 config reads within the 256-byte config space.
        let id = unsafe { pci_config_read_u8(bus, device, function, ptr) };
        let next = unsafe { pci_config_read_u8(bus, device, function, ptr + 1) } & 0xfc;
        if id == VIRTIO_PCI_CAP_VENDOR {
            // SAFETY: ring-0 config reads at documented cap offsets.
            let cfg_type = unsafe { pci_config_read_u8(bus, device, function, ptr + 3) };
            let bar = unsafe { pci_config_read_u8(bus, device, function, ptr + 4) };
            let offset = unsafe { read_cap_u32(bus, device, function, ptr + 8) };
            let length = unsafe { read_cap_u32(bus, device, function, ptr + 12) };
            let mut loc = CapLoc {
                bar,
                offset,
                length,
                notify_multiplier: 0,
                present: true,
            };
            match cfg_type {
                CFG_TYPE_COMMON => caps.common = loc,
                CFG_TYPE_NOTIFY => {
                    // The notify cap carries an extra u32 at +0x10.
                    // SAFETY: ring-0 config read.
                    loc.notify_multiplier =
                        unsafe { read_cap_u32(bus, device, function, ptr + 16) };
                    caps.notify = loc;
                }
                CFG_TYPE_DEVICE => caps.device = loc,
                _ => {}
            }
        }
        ptr = next;
        guard += 1;
    }
    caps
}

/// Read a u32 from config space assembled from four byte reads (config offsets
/// here may not be dword-aligned relative to our helper's needs, and byte
/// reads are always safe).
///
/// # Safety: ring-0 port I/O.
unsafe fn read_cap_u32(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    // SAFETY: four in-bounds ring-0 config byte reads.
    unsafe {
        let b0 = pci_config_read_u8(bus, dev, func, off) as u32;
        let b1 = pci_config_read_u8(bus, dev, func, off + 1) as u32;
        let b2 = pci_config_read_u8(bus, dev, func, off + 2) as u32;
        let b3 = pci_config_read_u8(bus, dev, func, off + 3) as u32;
        b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
    }
}

// ─── the transport ──────────────────────────────────────────────────────────

/// A modern virtio device: its PCI identity plus the direct-map VAs of its
/// common-config, notify, and device-config regions.
pub struct VirtioTransport {
    pub common_va: usize,
    pub notify_base_va: usize,
    pub notify_multiplier: u32,
    pub device_cfg_va: usize,
}

impl VirtioTransport {
    /// Build a transport from a device and its parsed caps. `dev`'s BARs must
    /// already be mapped (see `lib::map_device_bars`).
    pub fn new(dev: &PciDevice, caps: &VirtioCaps) -> Self {
        let common_bar = dev.bars[caps.common.bar as usize];
        let notify_bar = dev.bars[caps.notify.bar as usize];
        let device_bar = dev.bars[caps.device.bar as usize];
        Self {
            common_va: cfg_va(common_bar, caps.common.offset),
            notify_base_va: cfg_va(notify_bar, caps.notify.offset),
            notify_multiplier: caps.notify.notify_multiplier,
            device_cfg_va: cfg_va(device_bar, caps.device.offset),
        }
    }

    // ── common-cfg field access (all volatile) ──

    /// # Safety: common region mapped.
    unsafe fn status(&self) -> u8 {
        // SAFETY: common region is mapped MMIO.
        unsafe { core::ptr::read_volatile((self.common_va + CFG_DEVICE_STATUS) as *const u8) }
    }
    /// # Safety: common region mapped.
    unsafe fn set_status(&self, v: u8) {
        // SAFETY: common region is mapped MMIO.
        unsafe { core::ptr::write_volatile((self.common_va + CFG_DEVICE_STATUS) as *mut u8, v) };
    }

    /// Read one 32-bit device-feature word (`select` = 0 or 1).
    /// # Safety: common region mapped.
    unsafe fn device_feature(&self, select: u32) -> u32 {
        // SAFETY: common region is mapped MMIO.
        unsafe {
            core::ptr::write_volatile(
                (self.common_va + CFG_DEVICE_FEATURE_SELECT) as *mut u32,
                select,
            );
            core::ptr::read_volatile((self.common_va + CFG_DEVICE_FEATURE) as *const u32)
        }
    }

    /// Write one 32-bit driver-feature word (`select` = 0 or 1).
    /// # Safety: common region mapped.
    unsafe fn set_driver_feature(&self, select: u32, value: u32) {
        // SAFETY: common region is mapped MMIO.
        unsafe {
            core::ptr::write_volatile(
                (self.common_va + CFG_DRIVER_FEATURE_SELECT) as *mut u32,
                select,
            );
            core::ptr::write_volatile((self.common_va + CFG_DRIVER_FEATURE) as *mut u32, value);
        }
    }

    /// Number of queues the device offers.
    /// # Safety: common region mapped.
    pub unsafe fn num_queues(&self) -> u16 {
        // SAFETY: common region is mapped MMIO.
        unsafe { core::ptr::read_volatile((self.common_va + CFG_NUM_QUEUES) as *const u16) }
    }

    /// Run the reset → ACK → DRIVER → features → FEATURES_OK handshake.
    ///
    /// Returns `false` if the device is legacy-only (no VERSION_1) or rejects
    /// the feature set (clears FEATURES_OK) — the caller must not continue.
    ///
    /// # Safety: common region mapped; call once at init before queue setup.
    pub unsafe fn negotiate(&self) -> bool {
        // SAFETY: all accesses target the mapped common-config MMIO.
        unsafe {
            // 1. Reset and wait for the device to acknowledge (status reads 0).
            self.set_status(0);
            let mut spins = 0;
            while self.status() != 0 && spins < 1_000_000 {
                core::hint::spin_loop();
                spins += 1;
            }
            // 2-3. ACKNOWLEDGE, then DRIVER.
            self.set_status(STATUS_ACKNOWLEDGE);
            self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);

            // 4. Read the two device-feature words.
            let dev_lo = self.device_feature(0);
            let dev_hi = self.device_feature(1);
            // 5. Negotiate the minimal set and write it back.
            let (drv_lo, drv_hi) = negotiate_features(dev_lo, dev_hi);
            if !has_version_1(drv_hi) {
                // Legacy-only device: refuse rather than misbehave.
                self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FAILED);
                return false;
            }
            self.set_driver_feature(0, drv_lo);
            self.set_driver_feature(1, drv_hi);

            // 6-7. FEATURES_OK, then confirm the device kept it.
            self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
            if self.status() & STATUS_FEATURES_OK == 0 {
                self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FAILED);
                return false;
            }
            true
        }
    }

    /// Final step: tell the device the driver is ready (after queue setup).
    /// # Safety: common region mapped; queues already configured.
    pub unsafe fn set_driver_ok(&self) {
        // SAFETY: common region is mapped MMIO.
        unsafe {
            self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK)
        };
    }

    /// Program one virtqueue's three physical addresses, size, MSI-X vector,
    /// and enable it. Returns the device-reported `queue_size` and the
    /// `queue_notify_off` (both read after selecting the queue).
    ///
    /// # Safety: common region mapped; `desc/avail/used` are live DMA frames.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn setup_queue(
        &self,
        queue_index: u16,
        queue_size: u16,
        desc_phys: u64,
        avail_phys: u64,
        used_phys: u64,
        msix_vector: u16,
    ) -> u16 {
        // SAFETY: all accesses target mapped common-config MMIO.
        unsafe {
            let base = self.common_va;
            core::ptr::write_volatile((base + CFG_QUEUE_SELECT) as *mut u16, queue_index);
            core::ptr::write_volatile((base + CFG_QUEUE_SIZE) as *mut u16, queue_size);
            core::ptr::write_volatile((base + CFG_QUEUE_DESC) as *mut u64, desc_phys);
            core::ptr::write_volatile((base + CFG_QUEUE_DRIVER) as *mut u64, avail_phys);
            core::ptr::write_volatile((base + CFG_QUEUE_DEVICE) as *mut u64, used_phys);
            core::ptr::write_volatile((base + CFG_QUEUE_MSIX_VECTOR) as *mut u16, msix_vector);
            let notify_off =
                core::ptr::read_volatile((base + CFG_QUEUE_NOTIFY_OFF) as *const u16);
            core::ptr::write_volatile((base + CFG_QUEUE_ENABLE) as *mut u16, 1);
            notify_off
        }
    }

    /// The device-reported size of queue `queue_index` (0 = queue absent).
    /// # Safety: common region mapped.
    pub unsafe fn queue_size(&self, queue_index: u16) -> u16 {
        // SAFETY: common region is mapped MMIO.
        unsafe {
            core::ptr::write_volatile((self.common_va + CFG_QUEUE_SELECT) as *mut u16, queue_index);
            core::ptr::read_volatile((self.common_va + CFG_QUEUE_SIZE) as *const u16)
        }
    }

    /// Notify the device that queue `queue_index` has new buffers.
    /// # Safety: notify region mapped; `notify_off` from `setup_queue`.
    pub unsafe fn notify_queue(&self, queue_index: u16, notify_off: u16) {
        let addr = notify_addr(self.notify_base_va, notify_off, self.notify_multiplier);
        // SAFETY: notify region is mapped MMIO.
        unsafe { core::ptr::write_volatile(addr as *mut u16, queue_index) };
    }

    /// Read `n` bytes of device-specific config (e.g. the MAC) into `out`.
    /// # Safety: device-config region mapped and at least `out.len()` long.
    pub unsafe fn read_device_config(&self, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: caller guarantees the region covers these bytes.
            *b = unsafe { core::ptr::read_volatile((self.device_cfg_va + i) as *const u8) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_requests_exactly_version1_and_mac_when_offered() {
        // Device offers a broad feature set; we must pick out only two bits.
        let dev_lo = 0xFFFF_FFFF; // all word-0 features offered
        let dev_hi = 0xFFFF_FFFF; // all word-1 features offered
        let (lo, hi) = negotiate_features(dev_lo, dev_hi);
        assert_eq!(lo, VIRTIO_NET_F_MAC_BIT, "word 0: only MAC");
        assert_eq!(hi, VIRTIO_F_VERSION_1_BIT, "word 1: only VERSION_1");
    }

    #[test]
    fn negotiate_drops_features_the_device_does_not_offer() {
        // Device offers neither MAC nor VERSION_1.
        let (lo, hi) = negotiate_features(0, 0);
        assert_eq!(lo, 0);
        assert_eq!(hi, 0);
        assert!(!has_version_1(hi), "no VERSION_1 => legacy, must bail");
    }

    #[test]
    fn version_1_detection() {
        assert!(has_version_1(VIRTIO_F_VERSION_1_BIT));
        assert!(has_version_1(0xFFFF_FFFF));
        assert!(!has_version_1(0));
        assert!(!has_version_1(0xFFFF_FFFE)); // bit 0 clear
    }

    #[test]
    fn notify_address_is_base_plus_off_times_multiplier() {
        // The exact arithmetic the spec mandates.
        assert_eq!(notify_addr(0x1000, 0, 4), 0x1000);
        assert_eq!(notify_addr(0x1000, 1, 4), 0x1004);
        assert_eq!(notify_addr(0x1000, 3, 4), 0x100C);
        // A zero multiplier means every queue notifies at the same address.
        assert_eq!(notify_addr(0x2000, 7, 0), 0x2000);
    }

    #[test]
    fn cfg_va_is_direct_map_plus_bar_plus_offset() {
        assert_eq!(
            cfg_va(0x8104_1000, 0x2000),
            (0xffff_8000_0000_0000u64 + 0x8104_1000 + 0x2000) as usize
        );
    }

    #[test]
    fn cfg_type_constants_match_the_spec() {
        assert_eq!(CFG_TYPE_COMMON, 1);
        assert_eq!(CFG_TYPE_NOTIFY, 2);
        assert_eq!(CFG_TYPE_ISR, 3);
        assert_eq!(CFG_TYPE_DEVICE, 4);
        assert_eq!(CFG_TYPE_PCI, 5);
        assert_eq!(VIRTIO_PCI_CAP_VENDOR, 0x09);
    }

    #[test]
    fn feature_bit_positions_are_correct() {
        // NET_F_MAC is bit 5 of word 0; VERSION_1 is bit 32 = bit 0 of word 1.
        assert_eq!(VIRTIO_NET_F_MAC_BIT, 0x20);
        assert_eq!(VIRTIO_F_VERSION_1_BIT, 0x1);
    }
}
