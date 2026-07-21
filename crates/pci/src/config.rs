//! PCI configuration space access via the legacy `0xCF8`/`0xCFC` I/O ports.
//!
//! This is the port-based ("mechanism #1") access method. It reaches the
//! first 256 bytes of every function's configuration space, which is all
//! this checkpoint needs.
//!
//! The modern alternative — ECAM / MMIO configuration access via the ACPI
//! MCFG table — is required only for PCIe *extended* configuration space
//! (offsets `0x100..=0xFFF`). Nothing here reads past offset `0xFF`, so
//! port-based access is sufficient; ECAM is noted as future work.

use core::arch::asm;

/// CONFIG_ADDRESS port: the 32-bit BDF+offset selector is written here.
pub const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
/// CONFIG_DATA port: the dword at the selected address is read/written here.
pub const PCI_CONFIG_DATA: u16 = 0xCFC;

/// Assemble a CONFIG_ADDRESS value:
///
/// * bit 31     — enable
/// * bits 23:16 — bus
/// * bits 15:11 — device (slot)
/// * bits 10:8  — function
/// * bits 7:2   — register offset (dword-aligned; low two bits forced to 0)
/// * bits 1:0   — always 0
pub fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    const ENABLE: u32 = 1 << 31;
    ENABLE
        | ((bus as u32) << 16)
        | (((device as u32) & 0x1f) << 11)
        | (((function as u32) & 0x07) << 8)
        | ((offset as u32) & 0xfc)
}

/// Write a u32 to an I/O port.
///
/// # Safety
/// Ring-0 port I/O; caller must be at CPL0.
#[inline]
unsafe fn outl(port: u16, val: u32) {
    // SAFETY: caller guarantees CPL0 I/O permission; `dx`/`eax` are the
    // architectural operands of `out`.
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") val,
             options(nomem, nostack, preserves_flags));
    }
}

/// Read a u32 from an I/O port.
///
/// # Safety
/// Ring-0 port I/O; caller must be at CPL0.
#[inline]
unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    // SAFETY: caller guarantees CPL0 I/O permission; `dx`/`eax` are the
    // architectural operands of `in`.
    unsafe {
        asm!("in eax, dx", out("eax") val, in("dx") port,
             options(nomem, nostack, preserves_flags));
    }
    val
}

/// Read the aligned dword at `(bus, dev, func, offset & !3)`.
///
/// # Safety
/// Ring-0 port I/O; caller must be at CPL0.
pub unsafe fn pci_config_read_u32(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    // SAFETY: forwarded CPL0 obligation.
    unsafe {
        outl(PCI_CONFIG_ADDRESS, config_address(bus, dev, func, off));
        inl(PCI_CONFIG_DATA)
    }
}

/// Write the aligned dword at `(bus, dev, func, offset & !3)`.
///
/// # Safety
/// Ring-0 port I/O; caller must be at CPL0.
pub unsafe fn pci_config_write_u32(bus: u8, dev: u8, func: u8, off: u8, val: u32) {
    // SAFETY: forwarded CPL0 obligation.
    unsafe {
        outl(PCI_CONFIG_ADDRESS, config_address(bus, dev, func, off));
        outl(PCI_CONFIG_DATA, val);
    }
}

/// Read a u16 from config space, selecting the correct byte lane from the
/// containing dword (`offset & 2` picks the low or high half).
///
/// # Safety
/// Ring-0 port I/O; caller must be at CPL0.
pub unsafe fn pci_config_read_u16(bus: u8, dev: u8, func: u8, off: u8) -> u16 {
    // SAFETY: forwarded CPL0 obligation.
    let dword = unsafe { pci_config_read_u32(bus, dev, func, off & 0xfc) };
    (dword >> ((off & 2) * 8)) as u16
}

/// Read a u8 from config space, selecting the correct byte lane
/// (`offset & 3`).
///
/// # Safety
/// Ring-0 port I/O; caller must be at CPL0.
pub unsafe fn pci_config_read_u8(bus: u8, dev: u8, func: u8, off: u8) -> u8 {
    // SAFETY: forwarded CPL0 obligation.
    let dword = unsafe { pci_config_read_u32(bus, dev, func, off & 0xfc) };
    (dword >> ((off & 3) * 8)) as u8
}

/// Write a u16 to config space via read-modify-write of the containing
/// dword (config space is dword-addressed through the data port).
///
/// # Safety
/// Ring-0 port I/O; caller must be at CPL0.
pub unsafe fn pci_config_write_u16(bus: u8, dev: u8, func: u8, off: u8, val: u16) {
    let aligned = off & 0xfc;
    let shift = (off & 2) * 8;
    // SAFETY: forwarded CPL0 obligation.
    let mut dword = unsafe { pci_config_read_u32(bus, dev, func, aligned) };
    dword &= !(0xffffu32 << shift);
    dword |= (val as u32) << shift;
    // SAFETY: forwarded CPL0 obligation.
    unsafe { pci_config_write_u32(bus, dev, func, aligned, dword) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_address_packs_each_field_at_the_right_offset() {
        // Enable bit only.
        assert_eq!(config_address(0, 0, 0, 0), 0x8000_0000);
        // bus -> bits 23:16.
        assert_eq!(config_address(1, 0, 0, 0), 0x8001_0000);
        // device -> bits 15:11.
        assert_eq!(config_address(0, 2, 0, 0), 0x8000_1000);
        // function -> bits 10:8.
        assert_eq!(config_address(0, 0, 3, 0), 0x8000_0300);
        // offset -> bits 7:2.
        assert_eq!(config_address(0, 0, 0, 0x10), 0x8000_0010);
    }

    #[test]
    fn config_address_forces_low_two_offset_bits_to_zero() {
        // 0x13 is dword-aligned down to 0x10.
        assert_eq!(config_address(0, 0, 0, 0x13), 0x8000_0010);
        // A fully populated selector.
        let a = config_address(0xab, 0x1f, 0x7, 0xff);
        assert_eq!(
            a,
            0x8000_0000 | (0xab << 16) | (0x1f << 11) | (0x7 << 8) | 0xfc
        );
    }

    #[test]
    fn config_address_masks_device_and_function_to_their_field_widths() {
        // device is 5 bits; 0x3f wraps to 0x1f.
        assert_eq!(config_address(0, 0x3f, 0, 0), config_address(0, 0x1f, 0, 0));
        // function is 3 bits; 0x0f wraps to 0x07.
        assert_eq!(config_address(0, 0, 0x0f, 0), config_address(0, 0, 0x07, 0));
    }
}
