use kernel_apic::apic_id;

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
const MADT_X2APIC: u8 = 9;
const MADT_LAPIC_LEN: u8 = 8;
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

/// Parse ACPI MADT and return discovered CPUs with the BSP entry first.
///
/// # Safety
/// `rsdp_phys` must be the physical address Limine reported for the ACPI RSDP,
/// and the kernel direct map must cover all ACPI table pages being read.
pub unsafe fn parse_madt(rsdp_phys: u64) -> ([CpuEntry; MAX_CPUS], usize) {
    let mut cpus = [CpuEntry::zeroed(); MAX_CPUS];
    let bsp_apic_id = apic_id();

    if !rsdp_signature_valid(rsdp_phys) {
        return (cpus, 0);
    }

    let madt_phys = match unsafe { find_madt(rsdp_phys) } {
        Some(phys) => phys,
        None => return (cpus, 0),
    };

    let madt_len = unsafe { read_u32(madt_phys + SDT_LENGTH_OFF) } as u64;
    if madt_len < MADT_ENTRY_START {
        return (cpus, 0);
    }

    let mut off = MADT_ENTRY_START;
    let mut count = 0usize;
    while off + 2 <= madt_len && count < MAX_CPUS {
        let entry = madt_phys + off;
        let typ = unsafe { read_u8(entry) };
        let len = unsafe { read_u8(entry + 1) };
        if len < 2 || off + len as u64 > madt_len {
            break;
        }

        let parsed = match (typ, len) {
            (MADT_LAPIC, MADT_LAPIC_LEN..) => unsafe { parse_lapic_entry(entry) },
            (MADT_X2APIC, MADT_X2APIC_LEN..) => unsafe { parse_x2apic_entry(entry) },
            _ => None,
        };

        if let Some(mut cpu) = parsed {
            cpu.is_bsp = cpu.apic_id == bsp_apic_id;
            push_cpu_bsp_first(&mut cpus, &mut count, cpu);
        }

        off += len as u64;
    }

    (cpus, count)
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
}
