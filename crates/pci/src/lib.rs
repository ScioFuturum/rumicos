//! Minimal PCI bus support: configuration-space access, brute-force bus
//! enumeration with BAR decoding, and MSI-X capability discovery/setup.
//!
//! Scope is deliberately small — exactly what the virtio-net checkpoint
//! needs. Access is port-based (`0xCF8`/`0xCFC`); interrupts are delivered
//! via MSI-X straight to a LAPIC, so no I/O APIC is involved. See
//! [`config`] and [`msix`] for the details and their documented limits.
#![no_std]

#[cfg(test)]
extern crate std;

pub mod config;
pub mod msix;

use config::{pci_config_read_u16, pci_config_read_u32, pci_config_read_u8, pci_config_write_u16,
             pci_config_write_u32};
use kernel_sync::SpinLock;

/// Maximum number of PCI functions recorded during enumeration. A brute-force
/// scan of q35 finds only a handful; on overflow, enumeration stops storing
/// (it does not panic).
pub const MAX_PCI_DEVICES: usize = 32;

// ─── configuration-space register offsets (header type 0) ──────────────────
const OFF_VENDOR_ID: u8 = 0x00;
const OFF_DEVICE_ID: u8 = 0x02;
const OFF_COMMAND: u8 = 0x04;
const OFF_STATUS: u8 = 0x06;
const OFF_PROG_IF: u8 = 0x09;
const OFF_SUBCLASS: u8 = 0x0a;
const OFF_CLASS: u8 = 0x0b;
const OFF_HEADER_TYPE: u8 = 0x0e;
const OFF_BAR0: u8 = 0x10;
const OFF_CAP_PTR: u8 = 0x34;

const COMMAND_IO_SPACE: u16 = 1 << 0;
const COMMAND_MEM_SPACE: u16 = 1 << 1;
const STATUS_CAP_LIST: u16 = 1 << 4;

/// A single enumerated PCI function.
#[derive(Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub header_type: u8,
    /// Decoded BAR base addresses (0 if unimplemented). A 64-bit BAR's value
    /// occupies its own slot; the consumed high slot is left zeroed.
    pub bars: [u64; 6],
    pub bar_is_mmio: [bool; 6],
    pub bar_sizes: [u64; 6],
    /// True if an MSI-X capability was found.
    pub has_msix: bool,
    /// MSI-X table entry count (0 if `!has_msix`).
    pub msix_table_size: u16,
}

impl PciDevice {
    const EMPTY: PciDevice = PciDevice {
        bus: 0,
        device: 0,
        function: 0,
        vendor_id: 0xffff,
        device_id: 0xffff,
        class: 0,
        subclass: 0,
        prog_if: 0,
        header_type: 0,
        bars: [0; 6],
        bar_is_mmio: [false; 6],
        bar_sizes: [0; 6],
        has_msix: false,
        msix_table_size: 0,
    };
}

struct PciTable {
    devices: [PciDevice; MAX_PCI_DEVICES],
    count: usize,
}

static PCI_TABLE: SpinLock<PciTable> = SpinLock::new(PciTable {
    devices: [PciDevice::EMPTY; MAX_PCI_DEVICES],
    count: 0,
});

// ─── pure BAR-decoding helpers (host-tested) ───────────────────────────────

/// A BAR whose bit 0 is set decodes into I/O space rather than memory.
pub fn bar_is_io(bar_low: u32) -> bool {
    bar_low & 0x1 == 1
}

/// A memory BAR whose type bits (2:1) are `0b10` is 64-bit: it consumes the
/// next BAR slot as its high dword.
pub fn bar_is_mmio_64(bar_low: u32) -> bool {
    bar_low & 0x1 == 0 && (bar_low >> 1) & 0x3 == 0x2
}

/// Region size of a 32-bit BAR from its all-ones readback with the low type
/// bits already cleared (`0` if unimplemented).
pub fn size32_from_masked_readback(masked: u32) -> u32 {
    if masked == 0 {
        0
    } else {
        (!masked).wrapping_add(1)
    }
}

/// Region size of a 64-bit BAR from its all-ones readback (low and high
/// dwords) with the low type bits already cleared (`0` if unimplemented).
pub fn size64_from_masked_readback(masked_lo: u32, masked_hi: u32) -> u64 {
    let masked = ((masked_hi as u64) << 32) | (masked_lo as u64);
    if masked == 0 {
        0
    } else {
        (!masked).wrapping_add(1)
    }
}

// ─── enumeration ───────────────────────────────────────────────────────────

/// Brute-force scan of all 256 buses × 32 devices × 8 functions. A
/// `vendor_id` of `0xFFFF` means "no device"; functions 1-7 are probed only
/// when function 0's header-type has the multifunction bit (7) set. Results
/// are stored in a fixed static table; returns the number found.
///
/// Safe to call once at boot on the BSP. Internally performs ring-0 port I/O.
pub fn enumerate_pci() -> usize {
    let mut table = PCI_TABLE.lock();
    table.count = 0;

    for bus in 0u16..256 {
        for device in 0u8..32 {
            // SAFETY: CPL0 boot context; probing a (bus,dev,func=0) triple.
            let Some(dev0) = (unsafe { probe_function(bus as u8, device, 0) }) else {
                continue;
            };
            let multifunction = dev0.header_type & 0x80 != 0;
            push(&mut table, dev0);
            if multifunction {
                for function in 1u8..8 {
                    // SAFETY: as above.
                    if let Some(dev) = unsafe { probe_function(bus as u8, device, function) } {
                        push(&mut table, dev);
                    }
                }
            }
        }
    }
    table.count
}

fn push(table: &mut PciTable, dev: PciDevice) {
    // Degrade gracefully on overflow: stop storing rather than panic.
    if table.count < MAX_PCI_DEVICES {
        table.devices[table.count] = dev;
        table.count += 1;
    }
}

/// Probe one (bus, device, function); `None` if absent (`vendor == 0xFFFF`).
///
/// # Safety
/// Ring-0 port I/O.
unsafe fn probe_function(bus: u8, device: u8, function: u8) -> Option<PciDevice> {
    // SAFETY: forwarded CPL0 obligation.
    let vendor_id = unsafe { pci_config_read_u16(bus, device, function, OFF_VENDOR_ID) };
    if vendor_id == 0xffff {
        return None;
    }
    // SAFETY: forwarded CPL0 obligation.
    let mut dev = unsafe {
        PciDevice {
            bus,
            device,
            function,
            vendor_id,
            device_id: pci_config_read_u16(bus, device, function, OFF_DEVICE_ID),
            class: pci_config_read_u8(bus, device, function, OFF_CLASS),
            subclass: pci_config_read_u8(bus, device, function, OFF_SUBCLASS),
            prog_if: pci_config_read_u8(bus, device, function, OFF_PROG_IF),
            header_type: pci_config_read_u8(bus, device, function, OFF_HEADER_TYPE),
            bars: [0; 6],
            bar_is_mmio: [false; 6],
            bar_sizes: [0; 6],
            has_msix: false,
            msix_table_size: 0,
        }
    };

    // BARs and capabilities only exist in this layout for header type 0
    // (general device). Bridges (type 1) have a different header.
    if dev.header_type & 0x7f == 0 {
        // SAFETY: forwarded CPL0 obligation; header type 0 confirmed.
        unsafe { decode_bars(&mut dev) };
        // SAFETY: forwarded CPL0 obligation.
        if let Some(cap) = unsafe { find_capability(bus, device, function, msix::MSIX_CAP_ID) } {
            // SAFETY: cap is a real MSI-X capability offset just found.
            let msix = unsafe { msix::read_msix_cap(&dev, cap) };
            dev.has_msix = true;
            dev.msix_table_size = msix.table_size;
        }
    }
    Some(dev)
}

/// Decode and size the six BARs of a header-type-0 device.
///
/// # Safety
/// Ring-0 port I/O; `dev` must be header type 0.
unsafe fn decode_bars(dev: &mut PciDevice) {
    let (bus, device, function) = (dev.bus, dev.device, dev.function);

    // Sizing a BAR means writing all-ones to it, so disable I/O and memory
    // decoding first to avoid the device briefly claiming a bogus window;
    // restore the command register afterward.
    // SAFETY: forwarded CPL0 obligation.
    let command = unsafe { pci_config_read_u16(bus, device, function, OFF_COMMAND) };
    // SAFETY: forwarded CPL0 obligation.
    unsafe {
        pci_config_write_u16(
            bus,
            device,
            function,
            OFF_COMMAND,
            command & !(COMMAND_IO_SPACE | COMMAND_MEM_SPACE),
        );
    }

    let mut i = 0usize;
    while i < 6 {
        let off = OFF_BAR0 + (i as u8) * 4;
        // SAFETY: forwarded CPL0 obligation.
        let original = unsafe { pci_config_read_u32(bus, device, function, off) };

        // Size: write all ones, read back, then RESTORE the original — a
        // BAR left corrupted here surfaces much later as inexplicable MMIO
        // failures.
        // SAFETY: forwarded CPL0 obligation.
        let readback = unsafe {
            pci_config_write_u32(bus, device, function, off, 0xffff_ffff);
            let rb = pci_config_read_u32(bus, device, function, off);
            pci_config_write_u32(bus, device, function, off, original);
            rb
        };

        if bar_is_io(original) {
            dev.bars[i] = (original & !0x3) as u64;
            dev.bar_is_mmio[i] = false;
            dev.bar_sizes[i] = size32_from_masked_readback(readback & !0x3) as u64;
            i += 1;
        } else if bar_is_mmio_64(original) {
            let off_hi = off + 4;
            // SAFETY: forwarded CPL0 obligation.
            let original_hi = unsafe { pci_config_read_u32(bus, device, function, off_hi) };
            // SAFETY: forwarded CPL0 obligation.
            let readback_hi = unsafe {
                pci_config_write_u32(bus, device, function, off_hi, 0xffff_ffff);
                let rb = pci_config_read_u32(bus, device, function, off_hi);
                pci_config_write_u32(bus, device, function, off_hi, original_hi);
                rb
            };
            dev.bars[i] = ((original_hi as u64) << 32) | (original & !0xf) as u64;
            dev.bar_is_mmio[i] = true;
            dev.bar_sizes[i] = size64_from_masked_readback(readback & !0xf, readback_hi);
            // The next slot is this BAR's high dword: skip it.
            i += 2;
        } else {
            dev.bars[i] = (original & !0xf) as u64;
            dev.bar_is_mmio[i] = true;
            dev.bar_sizes[i] = size32_from_masked_readback(readback & !0xf) as u64;
            i += 1;
        }
    }

    // Restore the original command register.
    // SAFETY: forwarded CPL0 obligation.
    unsafe { pci_config_write_u16(bus, device, function, OFF_COMMAND, command) };
}

/// Walk the PCI capability list for a capability of `cap_id`; return its
/// configuration-space offset if present.
///
/// # Safety
/// Ring-0 port I/O.
pub unsafe fn find_capability(bus: u8, device: u8, function: u8, cap_id: u8) -> Option<u8> {
    // SAFETY: forwarded CPL0 obligation.
    let status = unsafe { pci_config_read_u16(bus, device, function, OFF_STATUS) };
    if status & STATUS_CAP_LIST == 0 {
        return None;
    }
    // SAFETY: forwarded CPL0 obligation.
    let mut ptr = unsafe { pci_config_read_u8(bus, device, function, OFF_CAP_PTR) } & 0xfc;
    // Bounded walk: the list lives in the 256-byte config space, so at most
    // ~48 capabilities; the guard also stops a malformed cyclic list.
    let mut guard = 0;
    while ptr != 0 && guard < 48 {
        // SAFETY: forwarded CPL0 obligation.
        let id = unsafe { pci_config_read_u8(bus, device, function, ptr) };
        if id == cap_id {
            return Some(ptr);
        }
        // SAFETY: forwarded CPL0 obligation.
        ptr = unsafe { pci_config_read_u8(bus, device, function, ptr + 1) } & 0xfc;
        guard += 1;
    }
    None
}

// ─── accessors for the enumerated table ────────────────────────────────────

/// Number of devices recorded by the last [`enumerate_pci`].
pub fn count() -> usize {
    PCI_TABLE.lock().count
}

/// Copy of the `index`-th enumerated device, if any (`PciDevice` is `Copy`).
pub fn get(index: usize) -> Option<PciDevice> {
    let table = PCI_TABLE.lock();
    if index < table.count {
        Some(table.devices[index])
    } else {
        None
    }
}

/// First device matching `(vendor_id, device_id)`, if enumerated.
pub fn find_device(vendor_id: u16, device_id: u16) -> Option<PciDevice> {
    let table = PCI_TABLE.lock();
    table.devices[..table.count]
        .iter()
        .copied()
        .find(|d| d.vendor_id == vendor_id && d.device_id == device_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_bar_is_detected_by_bit0() {
        assert!(bar_is_io(0x0000_0001));
        assert!(!bar_is_io(0x0000_0000));
        assert!(!bar_is_io(0xfebd_0000));
    }

    #[test]
    fn mmio_64bit_bar_detected_by_type_bits_10() {
        // type bits (2:1) = 0b10, bit0 = 0.
        assert!(bar_is_mmio_64(0x0000_0004));
        assert!(bar_is_mmio_64(0xfe00_000c & !0x1)); // still type 10, bit0 cleared
        // 32-bit memory BAR: type bits 00.
        assert!(!bar_is_mmio_64(0x0000_0000));
        // I/O BAR is never "mmio 64".
        assert!(!bar_is_mmio_64(0x0000_0005));
    }

    #[test]
    fn bar_size_from_32bit_readback() {
        // 4 KiB window: masked readback 0xFFFFF000.
        assert_eq!(size32_from_masked_readback(0xffff_f000), 0x1000);
        // 128 bytes: 0xFFFFFF80.
        assert_eq!(size32_from_masked_readback(0xffff_ff80), 0x80);
        // Unimplemented BAR.
        assert_eq!(size32_from_masked_readback(0), 0);
    }

    #[test]
    fn bar_size_from_64bit_readback() {
        // 16 KiB, entirely within the low dword.
        assert_eq!(size64_from_masked_readback(0xffff_c000, 0xffff_ffff), 0x4000);
        // 8 GiB window: crosses into the high dword.
        assert_eq!(size64_from_masked_readback(0x0000_0000, 0xffff_fffe), 0x2_0000_0000);
        assert_eq!(size64_from_masked_readback(0, 0), 0);
    }
}
