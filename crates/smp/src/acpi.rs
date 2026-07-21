use kernel_apic::apic_id;
use kernel_apic::ioapic::{IoApicInfo, IoApicList, IsoInfo, IsoList};

pub const MAX_CPUS: usize = 64;
pub const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
const SDT_HEADER_LEN: u64 = 36;
const MADT_ENTRY_START: u64 = SDT_HEADER_LEN + 8;
const RSDP_REVISION_OFF: u64 = 15;
const RSDP_RSDT_OFF: u64 = 16;
const RSDP_LENGTH_OFF: u64 = 20;
const RSDP_XSDT_OFF: u64 = 24;

const SDT_SIGNATURE_OFF: u64 = 0;
const SDT_LENGTH_OFF: u64 = 4;

const MADT_LAPIC: u8 = 0;
const MADT_IOAPIC: u8 = 1;
const MADT_ISO: u8 = 2;
const MADT_X2APIC: u8 = 9;
const MADT_LAPIC_LEN: u8 = 8;
const MADT_IOAPIC_LEN: u8 = 12;
const MADT_ISO_LEN: u8 = 10;
const MADT_X2APIC_LEN: u8 = 16;
const MADT_CPU_ENABLED: u32 = 1 << 0;
const MADT_CPU_ONLINE_CAPABLE: u32 = 1 << 1;

/// Represents one logical CPU found in MADT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuEntry {
    pub acpi_uid: u32,
    pub apic_id: u32,
    pub is_bsp: bool,
}

impl CpuEntry {
    pub const fn zeroed() -> Self {
        Self {
            acpi_uid: 0,
            apic_id: 0,
            is_bsp: false,
        }
    }
}

/// Everything a single MADT walk yields: the CPU list (BSP first) plus the
/// interrupt-routing topology the I/O APIC setup needs.
pub struct MadtInfo {
    pub cpus: [CpuEntry; MAX_CPUS],
    pub cpu_count: usize,
    pub ioapics: IoApicList,
    pub isos: IsoList,
}

/// Parse ACPI MADT and return discovered CPUs (BSP first) together with the
/// I/O APICs and Interrupt Source Overrides found in the SAME walk.
///
/// # Safety
/// `rsdp_phys` must be the physical address Limine reported for the ACPI RSDP,
/// and the kernel direct map must cover all ACPI table pages being read.
pub unsafe fn parse_madt(rsdp_phys: u64) -> MadtInfo {
    let mut info = MadtInfo {
        cpus: [CpuEntry::zeroed(); MAX_CPUS],
        cpu_count: 0,
        ioapics: IoApicList::new(),
        isos: IsoList::new(),
    };
    let bsp_apic_id = apic_id();

    if !rsdp_signature_valid(rsdp_phys) {
        return info;
    }

    let madt_phys = match unsafe { find_madt(rsdp_phys) } {
        Some(phys) => phys,
        None => return info,
    };

    let madt_len = unsafe { read_u32(madt_phys + SDT_LENGTH_OFF) } as u64;
    if madt_len < MADT_ENTRY_START {
        return info;
    }

    // Walk the WHOLE table: unlike the CPU-only version this replaces, the
    // loop must not stop once the CPU array fills — the I/O APIC and ISO
    // entries can appear anywhere in the table, including after the CPUs.
    let mut off = MADT_ENTRY_START;
    while off + 2 <= madt_len {
        let entry = madt_phys + off;
        let typ = unsafe { read_u8(entry) };
        let len = unsafe { read_u8(entry + 1) };
        if len < 2 || off + len as u64 > madt_len {
            break;
        }

        match (typ, len) {
            (MADT_LAPIC, MADT_LAPIC_LEN..) => {
                if let Some(mut cpu) = unsafe { parse_lapic_entry(entry) } {
                    cpu.is_bsp = cpu.apic_id == bsp_apic_id;
                    push_cpu_bsp_first(&mut info.cpus, &mut info.cpu_count, cpu);
                }
            }
            (MADT_X2APIC, MADT_X2APIC_LEN..) => {
                if let Some(mut cpu) = unsafe { parse_x2apic_entry(entry) } {
                    cpu.is_bsp = cpu.apic_id == bsp_apic_id;
                    push_cpu_bsp_first(&mut info.cpus, &mut info.cpu_count, cpu);
                }
            }
            (MADT_IOAPIC, MADT_IOAPIC_LEN..) => {
                info.ioapics.push(unsafe { parse_ioapic_entry(entry) });
            }
            (MADT_ISO, MADT_ISO_LEN..) => {
                info.isos.push(unsafe { parse_iso_entry(entry) });
            }
            _ => {}
        }

        off += len as u64;
    }

    info
}

unsafe fn find_madt(rsdp_phys: u64) -> Option<u64> {
    let revision = unsafe { read_u8(rsdp_phys + RSDP_REVISION_OFF) };
    let xsdt_phys = if revision >= 2 {
        let length = unsafe { read_u32(rsdp_phys + RSDP_LENGTH_OFF) };
        if length >= 36 {
            unsafe { read_u64(rsdp_phys + RSDP_XSDT_OFF) }
        } else {
            0
        }
    } else {
        0
    };

    if xsdt_phys != 0
        && let Some(madt) = unsafe { find_table_in_sdt(xsdt_phys, true) }
    {
        return Some(madt);
    }

    let rsdt_phys = unsafe { read_u32(rsdp_phys + RSDP_RSDT_OFF) } as u64;
    if rsdt_phys != 0 {
        unsafe { find_table_in_sdt(rsdt_phys, false) }
    } else {
        None
    }
}

unsafe fn find_table_in_sdt(sdt_phys: u64, xsdt: bool) -> Option<u64> {
    if !unsafe { sdt_signature_is(sdt_phys, if xsdt { *b"XSDT" } else { *b"RSDT" }) } {
        return None;
    }

    let len = unsafe { read_u32(sdt_phys + SDT_LENGTH_OFF) } as u64;
    if len < SDT_HEADER_LEN {
        return None;
    }

    let entry_size = if xsdt { 8 } else { 4 };
    let entries = (len - SDT_HEADER_LEN) / entry_size;
    let mut idx = 0;
    while idx < entries {
        let ptr_off = sdt_phys + SDT_HEADER_LEN + idx * entry_size;
        let table_phys = if xsdt {
            unsafe { read_u64(ptr_off) }
        } else {
            unsafe { read_u32(ptr_off) as u64 }
        };
        if table_phys != 0 && unsafe { sdt_signature_is(table_phys, *b"APIC") } {
            return Some(table_phys);
        }
        idx += 1;
    }

    None
}

unsafe fn parse_lapic_entry(entry: u64) -> Option<CpuEntry> {
    let flags = unsafe { read_u32(entry + 4) };
    if !cpu_flags_usable(flags) {
        return None;
    }
    Some(CpuEntry {
        acpi_uid: unsafe { read_u8(entry + 2) } as u32,
        apic_id: unsafe { read_u8(entry + 3) } as u32,
        is_bsp: false,
    })
}

unsafe fn parse_x2apic_entry(entry: u64) -> Option<CpuEntry> {
    let flags = unsafe { read_u32(entry + 8) };
    if !cpu_flags_usable(flags) {
        return None;
    }
    Some(CpuEntry {
        acpi_uid: unsafe { read_u32(entry + 12) },
        apic_id: unsafe { read_u32(entry + 4) },
        is_bsp: false,
    })
}

/// Extract an I/O APIC entry from its raw MADT bytes (little-endian).
/// Pure and host-testable; the byte offsets live here as the single source
/// of truth for both the target parser and its tests.
///
/// Layout (12 bytes): type@0, length@1, id@2, reserved@3, addr@4 (u32),
/// gsi_base@8 (u32).
fn ioapic_from_bytes(b: &[u8; MADT_IOAPIC_LEN as usize]) -> IoApicInfo {
    IoApicInfo {
        id: b[2],
        phys_addr: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        gsi_base: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
    }
}

/// Extract an Interrupt Source Override from its raw MADT bytes.
///
/// Layout (10 bytes): type@0, length@1, bus_source@2 (0=ISA), irq_source@3,
/// gsi@4 (u32), flags@8 (u16).
fn iso_from_bytes(b: &[u8; MADT_ISO_LEN as usize]) -> IsoInfo {
    IsoInfo {
        isa_irq: b[3],
        gsi: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        flags: u16::from_le_bytes([b[8], b[9]]),
    }
}

/// MADT type 1 (I/O APIC): copy the 12-byte entry out of ACPI memory and
/// decode it with [`ioapic_from_bytes`].
unsafe fn parse_ioapic_entry(entry: u64) -> IoApicInfo {
    let mut buf = [0u8; MADT_IOAPIC_LEN as usize];
    for (i, b) in buf.iter_mut().enumerate() {
        // SAFETY: caller (parse_madt) already checked the entry's declared
        // length covers these bytes and that they lie within the MADT.
        *b = unsafe { read_u8(entry + i as u64) };
    }
    ioapic_from_bytes(&buf)
}

/// MADT type 2 (Interrupt Source Override): copy the 10-byte entry out and
/// decode it with [`iso_from_bytes`].
unsafe fn parse_iso_entry(entry: u64) -> IsoInfo {
    let mut buf = [0u8; MADT_ISO_LEN as usize];
    for (i, b) in buf.iter_mut().enumerate() {
        // SAFETY: as in parse_ioapic_entry.
        *b = unsafe { read_u8(entry + i as u64) };
    }
    iso_from_bytes(&buf)
}

const fn cpu_flags_usable(flags: u32) -> bool {
    (flags & (MADT_CPU_ENABLED | MADT_CPU_ONLINE_CAPABLE)) != 0
}

fn push_cpu_bsp_first(cpus: &mut [CpuEntry; MAX_CPUS], count: &mut usize, cpu: CpuEntry) {
    if *count >= MAX_CPUS {
        return;
    }

    if cpu.is_bsp {
        if *count == 0 {
            cpus[0] = cpu;
        } else {
            cpus[*count] = cpus[0];
            cpus[0] = cpu;
        }
    } else {
        cpus[*count] = cpu;
    }
    *count += 1;
}

fn rsdp_signature_valid(rsdp_phys: u64) -> bool {
    let mut i = 0u64;
    while i < RSDP_SIGNATURE.len() as u64 {
        // SAFETY: caller supplied a direct-mapped RSDP physical address.
        if unsafe { read_u8(rsdp_phys + i) } != RSDP_SIGNATURE[i as usize] {
            return false;
        }
        i += 1;
    }
    true
}

unsafe fn sdt_signature_is(phys: u64, sig: [u8; 4]) -> bool {
    let mut i = 0u64;
    while i < sig.len() as u64 {
        if unsafe { read_u8(phys + SDT_SIGNATURE_OFF + i) } != sig[i as usize] {
            return false;
        }
        i += 1;
    }
    true
}

#[inline(always)]
unsafe fn read_u8(phys: u64) -> u8 {
    // SAFETY: caller guarantees the physical byte is covered by the direct map.
    unsafe { core::ptr::read_volatile(phys_to_virt(phys) as *const u8) }
}

#[inline(always)]
unsafe fn read_u32(phys: u64) -> u32 {
    // SAFETY: caller guarantees the physical field is covered by the direct map;
    // ACPI tables are byte-packed, so unaligned reads are required.
    unsafe { core::ptr::read_unaligned(phys_to_virt(phys) as *const u32) }
}

#[inline(always)]
unsafe fn read_u64(phys: u64) -> u64 {
    // SAFETY: caller guarantees the physical field is covered by the direct map;
    // ACPI tables are byte-packed, so unaligned reads are required.
    unsafe { core::ptr::read_unaligned(phys_to_virt(phys) as *const u64) }
}

#[inline(always)]
const fn phys_to_virt(phys: u64) -> usize {
    DIRECT_MAP_BASE.wrapping_add(phys) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C, packed)]
    struct LocalApicEntry {
        typ: u8,
        len: u8,
        acpi_uid: u8,
        apic_id: u8,
        flags: u32,
    }

    #[repr(C, packed)]
    struct LocalX2ApicEntry {
        typ: u8,
        len: u8,
        reserved: u16,
        x2apic_id: u32,
        flags: u32,
        acpi_uid: u32,
    }

    #[test]
    fn madt_entry_sizes_match_acpi() {
        assert_eq!(core::mem::size_of::<LocalApicEntry>(), 8);
        assert_eq!(core::mem::size_of::<LocalX2ApicEntry>(), 16);
    }

    #[test]
    fn cpu_flags_accept_enabled_or_online_capable() {
        assert!(cpu_flags_usable(1));
        assert!(cpu_flags_usable(2));
        assert!(!cpu_flags_usable(0));
    }

    #[test]
    fn madt_ioapic_and_iso_entry_sizes_match_acpi() {
        assert_eq!(MADT_IOAPIC_LEN, 12);
        assert_eq!(MADT_ISO_LEN, 10);
    }

    #[test]
    fn ioapic_entry_extracts_id_addr_and_gsi_base() {
        // type=1, len=12, id=2, reserved=0, addr=0xFEC00000, gsi_base=0.
        let bytes: [u8; 12] = [
            1, 12, 2, 0, // type, len, id, reserved
            0x00, 0x00, 0xC0, 0xFE, // addr = 0xFEC00000, little-endian
            0x00, 0x00, 0x00, 0x00, // gsi_base = 0
        ];
        let info = ioapic_from_bytes(&bytes);
        assert_eq!(info.id, 2);
        assert_eq!(info.phys_addr, 0xFEC0_0000);
        assert_eq!(info.gsi_base, 0);
    }

    #[test]
    fn ioapic_entry_reads_a_nonzero_gsi_base() {
        // A second I/O APIC handling GSIs starting at 24.
        let bytes: [u8; 12] = [
            1, 12, 5, 0, 0x00, 0x00, 0xC1, 0xFE, // addr = 0xFEC10000
            24, 0, 0, 0, // gsi_base = 24
        ];
        let info = ioapic_from_bytes(&bytes);
        assert_eq!(info.id, 5);
        assert_eq!(info.phys_addr, 0xFEC1_0000);
        assert_eq!(info.gsi_base, 24);
    }

    #[test]
    fn iso_entry_extracts_keyboard_irq_override() {
        // type=2, len=10, bus=0 (ISA), irq_source=1 (keyboard), gsi=17,
        // flags=0b1111 (active low, level triggered).
        let bytes: [u8; 10] = [
            2, 10, 0, 1, // type, len, bus_source, irq_source
            17, 0, 0, 0, // gsi = 17
            0x0F, 0x00, // flags = 0b1111
        ];
        let iso = iso_from_bytes(&bytes);
        assert_eq!(iso.isa_irq, 1);
        assert_eq!(iso.gsi, 17);
        assert_eq!(iso.flags, 0b1111);
    }

    #[test]
    fn iso_entry_extracts_the_common_timer_override() {
        // The override almost every PC ships: ISA IRQ0 -> GSI 2, flags 0.
        let bytes: [u8; 10] = [2, 10, 0, 0, 2, 0, 0, 0, 0, 0];
        let iso = iso_from_bytes(&bytes);
        assert_eq!(iso.isa_irq, 0);
        assert_eq!(iso.gsi, 2);
        assert_eq!(iso.flags, 0);
    }
}
