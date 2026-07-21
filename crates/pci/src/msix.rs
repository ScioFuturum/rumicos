//! MSI-X: message-signalled interrupts, extended.
//!
//! MSI-X is preferred over legacy INTx here specifically because an MSI-X
//! "interrupt" is just a memory write the device performs to the local APIC
//! MMIO window (`0xFEE0_0000...`). That delivers straight to a LAPIC with no
//! I/O APIC parsing or routing — the existing `kernel-apic` LAPIC code is a
//! sufficient foundation. Legacy INTx would require implementing I/O APIC
//! support first.
//!
//! ## Address/data encoding (x86 LAPIC delivery)
//!
//! * address = `0xFEE0_0000 | (destination_apic_id << 12)` — physical
//!   destination mode, no redirection hint, no destination-mode bit.
//! * data    = `vector` in bits 7:0 — delivery mode `000` (fixed), edge
//!   trigger; every other bit 0.
//!
//! This address form encodes only an **8-bit** APIC ID. Delivering to an
//! x2APIC ID above 255 needs the interrupt-remapping unit, which is out of
//! scope; [`msix_enable`] asserts the destination fits in 8 bits.

use crate::PciDevice;
use crate::config::{pci_config_read_u16, pci_config_write_u16};

/// PCI capability ID for MSI-X.
pub const MSIX_CAP_ID: u8 = 0x11;

/// Direct-map base; must match `kernel_paging` / `kernel_memory`
/// (`0xFFFF_8000_0000_0000` on 4-level paging). Used only to reach an
/// already-mapped BAR from kernel virtual space.
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

/// Message-control field bits (capability offset `0x02`).
const MSGCTL_TABLE_SIZE_MASK: u16 = 0x07ff;
const MSGCTL_FUNCTION_MASK: u16 = 1 << 14;
const MSGCTL_ENABLE: u16 = 1 << 15;

/// Compose the MSI-X message address for physical delivery to `dest_apic_id`.
pub fn msix_message_address(dest_apic_id: u8) -> u32 {
    0xFEE0_0000 | ((dest_apic_id as u32) << 12)
}

/// Compose the MSI-X message data for a fixed, edge-triggered `vector`.
pub fn msix_message_data(vector: u8) -> u32 {
    // Delivery mode 000 (fixed), edge trigger, all other control bits 0.
    vector as u32
}

/// Decode the MSI-X table size from a message-control value. The field
/// (bits 10:0) holds table-size-**minus-one**, so a raw value of 2 means 3
/// entries — a classic off-by-one.
pub fn msix_table_size_from_control(msg_ctrl: u16) -> u16 {
    (msg_ctrl & MSGCTL_TABLE_SIZE_MASK) + 1
}

/// A parsed MSI-X capability structure.
#[derive(Clone, Copy, Debug)]
pub struct MsixCap {
    /// Offset of the capability within configuration space.
    pub cap_offset: u8,
    /// Number of table entries (already the +1-adjusted value).
    pub table_size: u16,
    /// BAR index (BIR) the MSI-X table lives in.
    pub table_bir: u8,
    /// Byte offset of the table within that BAR (already 8-byte aligned).
    pub table_offset: u32,
    /// BAR index (BIR) the pending-bit array lives in.
    pub pba_bir: u8,
    /// Byte offset of the PBA within that BAR.
    pub pba_offset: u32,
}

/// Read and decode the MSI-X capability at `cap_offset`.
///
/// # Safety
/// Ring-0 port I/O; `cap_offset` must point at a real MSI-X capability
/// (ID `0x11`) on `dev`.
pub unsafe fn read_msix_cap(dev: &PciDevice, cap_offset: u8) -> MsixCap {
    // SAFETY: forwarded CPL0 obligation; caller vouches for cap_offset.
    let (msg_ctrl, table, pba) = unsafe {
        let msg_ctrl = pci_config_read_u16(dev.bus, dev.device, dev.function, cap_offset + 0x02);
        // table offset/BIR and PBA offset/BIR are dword fields.
        let table = crate::config::pci_config_read_u32(
            dev.bus,
            dev.device,
            dev.function,
            cap_offset + 0x04,
        );
        let pba = crate::config::pci_config_read_u32(
            dev.bus,
            dev.device,
            dev.function,
            cap_offset + 0x08,
        );
        (msg_ctrl, table, pba)
    };
    MsixCap {
        cap_offset,
        table_size: msix_table_size_from_control(msg_ctrl),
        table_bir: (table & 0x7) as u8,
        table_offset: table & !0x7,
        pba_bir: (pba & 0x7) as u8,
        pba_offset: pba & !0x7,
    }
}

/// Enable MSI-X on `dev`, routing table entry `index` to `vector` on
/// `dest_apic_id`, and unmask both the entry and the whole function.
///
/// This programs the MSI-X table, which lives in device MMIO (a BAR). It is
/// implemented here as part of the MSI-X support, but is exercised by Part B
/// (where a queue's interrupt is actually wired to a handler); Part A only
/// enumerates and reports the capability.
///
/// # Safety
/// * `dev` must have an MSI-X capability.
/// * The table BAR (`cap.table_bir`) must be mapped and reachable through
///   the direct map (`DIRECT_MAP_BASE + bar_phys`).
/// * Ring-0.
pub unsafe fn msix_enable(dev: &PciDevice, index: u16, vector: u8, dest_apic_id: u32) {
    assert!(
        dest_apic_id <= 0xff,
        "MSI-X message address carries an 8-bit APIC ID; delivering to an \
         x2APIC ID > 255 requires the interrupt-remapping unit (out of scope)"
    );

    // SAFETY: ring-0 (function contract); a capability walk over config space.
    let cap_offset = unsafe { crate::find_capability(dev.bus, dev.device, dev.function, MSIX_CAP_ID) }
        .expect("msix_enable called on a device without an MSI-X capability");
    // SAFETY: cap_offset came from the capability walk, so it is a real cap.
    let cap = unsafe { read_msix_cap(dev, cap_offset) };
    assert!(index < cap.table_size, "MSI-X table entry index out of range");

    let bar_phys = dev.bars[cap.table_bir as usize];
    let entry_virt =
        DIRECT_MAP_BASE + bar_phys + cap.table_offset as u64 + (index as u64) * 16;

    // SAFETY: the table BAR is mapped per the function contract; each field
    // is a naturally aligned MMIO dword written volatile, one at a time
    // (never as an aggregate — see the project's aggregate-copy history).
    unsafe {
        core::ptr::write_volatile(entry_virt as *mut u32, msix_message_address(dest_apic_id as u8));
        core::ptr::write_volatile((entry_virt + 0x4) as *mut u32, 0); // address high
        core::ptr::write_volatile((entry_virt + 0x8) as *mut u32, msix_message_data(vector));
        core::ptr::write_volatile((entry_virt + 0xc) as *mut u32, 0); // vector control: unmask
    }

    // Enable MSI-X and clear the function mask in the message-control field.
    // SAFETY: forwarded CPL0 obligation.
    unsafe {
        let mut ctrl =
            pci_config_read_u16(dev.bus, dev.device, dev.function, cap_offset + 0x02);
        ctrl |= MSGCTL_ENABLE;
        ctrl &= !MSGCTL_FUNCTION_MASK;
        pci_config_write_u16(dev.bus, dev.device, dev.function, cap_offset + 0x02, ctrl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_address_encodes_destination_apic_id() {
        assert_eq!(msix_message_address(0), 0xFEE0_0000);
        assert_eq!(msix_message_address(1), 0xFEE0_1000);
        assert_eq!(msix_message_address(0xff), 0xFEE0_0000 | (0xff << 12));
    }

    #[test]
    fn message_data_is_the_vector_with_fixed_edge_delivery() {
        assert_eq!(msix_message_data(0x40), 0x40);
        assert_eq!(msix_message_data(0xfe), 0xfe);
        // No delivery-mode / trigger bits set.
        assert_eq!(msix_message_data(0x40) & !0xff, 0);
    }

    #[test]
    fn table_size_is_control_field_plus_one() {
        // Raw 2 -> 3 entries.
        assert_eq!(msix_table_size_from_control(2), 3);
        // Only the low 11 bits count; the enable/mask bits are ignored.
        assert_eq!(msix_table_size_from_control(0), 1);
        assert_eq!(
            msix_table_size_from_control(MSGCTL_ENABLE | MSGCTL_FUNCTION_MASK | 2),
            3
        );
        // Max table size is 2048.
        assert_eq!(msix_table_size_from_control(0x7ff), 2048);
    }
}
