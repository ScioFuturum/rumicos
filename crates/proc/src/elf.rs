use core::mem;

pub const PT_LOAD: u32 = 1;
pub const PT_NULL: u32 = 0;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const ELF64_PHDR_SIZE: u16 = 56;
const MAX_LOAD_SEGMENTS: usize = 8;
const PAGE_SIZE: u64 = 4096;
const USER_TOP: u64 = 0x0000_8000_0000_0000;

#[repr(C, packed)]
pub struct Elf64Header {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

#[repr(C, packed)]
pub struct Elf64Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElfInfo {
    pub entry: u64,
    pub segments: [LoadSegment; MAX_LOAD_SEGMENTS],
    pub seg_count: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoadSegment {
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub data_off: u64,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ElfError {
    TooShort,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    NotExecutable,
    NotX86_64,
    TooManySegments,
    BadAlignment,
    SegmentInKernelSpace,
    OffsetOutOfBounds,
}

pub fn parse_elf(data: &[u8]) -> Result<ElfInfo, ElfError> {
    if data.len() < mem::size_of::<Elf64Header>() {
        return Err(ElfError::TooShort);
    }

    let header = read_header(data)?;
    if header.e_ident[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ElfError::BadMagic);
    }
    if header.e_ident[EI_CLASS] != ELFCLASS64 {
        return Err(ElfError::NotElf64);
    }
    if header.e_ident[EI_DATA] != ELFDATA2LSB {
        return Err(ElfError::NotLittleEndian);
    }
    if header.e_type != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if header.e_machine != EM_X86_64 {
        return Err(ElfError::NotX86_64);
    }
    if header.e_phentsize != ELF64_PHDR_SIZE {
        return Err(ElfError::OffsetOutOfBounds);
    }
    if header.e_phnum as usize > MAX_LOAD_SEGMENTS {
        return Err(ElfError::TooManySegments);
    }

    let phoff = header.e_phoff as usize;
    let phnum = header.e_phnum as usize;
    let phentsize = header.e_phentsize as usize;
    let phdr_bytes = phnum
        .checked_mul(phentsize)
        .ok_or(ElfError::OffsetOutOfBounds)?;
    let phdr_end = phoff
        .checked_add(phdr_bytes)
        .ok_or(ElfError::OffsetOutOfBounds)?;
    if phdr_end > data.len() {
        return Err(ElfError::OffsetOutOfBounds);
    }

    let mut info = ElfInfo {
        entry: header.e_entry,
        segments: [LoadSegment::default(); MAX_LOAD_SEGMENTS],
        seg_count: 0,
    };

    for idx in 0..phnum {
        let off = phoff + idx * phentsize;
        let phdr = read_phdr(&data[off..off + phentsize])?;
        if phdr.p_type == PT_NULL {
            continue;
        }
        if phdr.p_type != PT_LOAD {
            continue;
        }
        if info.seg_count == MAX_LOAD_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        if phdr.p_align < PAGE_SIZE || !phdr.p_align.is_power_of_two() {
            return Err(ElfError::BadAlignment);
        }
        if phdr.p_vaddr >= USER_TOP {
            return Err(ElfError::SegmentInKernelSpace);
        }
        let mem_end = phdr
            .p_vaddr
            .checked_add(phdr.p_memsz)
            .ok_or(ElfError::SegmentInKernelSpace)?;
        if mem_end > USER_TOP {
            return Err(ElfError::SegmentInKernelSpace);
        }
        if phdr.p_memsz < phdr.p_filesz {
            return Err(ElfError::OffsetOutOfBounds);
        }
        let file_end = phdr
            .p_offset
            .checked_add(phdr.p_filesz)
            .ok_or(ElfError::OffsetOutOfBounds)?;
        if file_end as usize > data.len() {
            return Err(ElfError::OffsetOutOfBounds);
        }

        let page_offset = phdr.p_vaddr & (PAGE_SIZE - 1);
        if phdr.p_offset < page_offset {
            return Err(ElfError::OffsetOutOfBounds);
        }

        info.segments[info.seg_count] = LoadSegment {
            vaddr: phdr.p_vaddr & !(PAGE_SIZE - 1),
            filesz: phdr.p_filesz + page_offset,
            memsz: phdr.p_memsz + page_offset,
            data_off: phdr.p_offset - page_offset,
            flags: phdr.p_flags,
        };
        info.seg_count += 1;
    }

    Ok(info)
}

fn read_header(data: &[u8]) -> Result<Elf64Header, ElfError> {
    if data.len() < mem::size_of::<Elf64Header>() {
        return Err(ElfError::TooShort);
    }
    let ptr = data.as_ptr().cast::<Elf64Header>();
    let header = unsafe {
        // SAFETY: length was checked; read_unaligned handles packed alignment.
        ptr.read_unaligned()
    };
    Ok(header)
}

fn read_phdr(data: &[u8]) -> Result<Elf64Phdr, ElfError> {
    if data.len() < mem::size_of::<Elf64Phdr>() {
        return Err(ElfError::OffsetOutOfBounds);
    }
    let ptr = data.as_ptr().cast::<Elf64Phdr>();
    let phdr = unsafe {
        // SAFETY: length was checked; read_unaligned handles packed alignment.
        ptr.read_unaligned()
    };
    Ok(phdr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_header() -> [u8; 64] {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        data[4] = ELFCLASS64;
        data[5] = ELFDATA2LSB;
        data[6] = 1;
        data[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        data[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        data[20..24].copy_from_slice(&1u32.to_le_bytes());
        data[24..32].copy_from_slice(&0x400000u64.to_le_bytes());
        data[32..40].copy_from_slice(&64u64.to_le_bytes());
        data[52..54].copy_from_slice(&64u16.to_le_bytes());
        data[54..56].copy_from_slice(&ELF64_PHDR_SIZE.to_le_bytes());
        data
    }

    #[test]
    fn parse_elf_accepts_minimal_valid_header() {
        let data = valid_header();
        let info = parse_elf(&data).unwrap();
        assert_eq!(info.entry, 0x400000);
        assert_eq!(info.seg_count, 0);
    }

    #[test]
    fn parse_elf_rejects_bad_magic() {
        let mut data = valid_header();
        data[0] = 0;
        assert_eq!(parse_elf(&data), Err(ElfError::BadMagic));
    }

    #[test]
    fn parse_elf_rejects_non_x86_64() {
        let mut data = valid_header();
        data[18..20].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(parse_elf(&data), Err(ElfError::NotX86_64));
    }

    #[test]
    fn parse_elf_rejects_kernel_space_segment() {
        let mut data = [0u8; 120];
        data[..64].copy_from_slice(&valid_header());
        data[56..58].copy_from_slice(&1u16.to_le_bytes());
        let ph = &mut data[64..120];
        ph[0..4].copy_from_slice(&PT_LOAD.to_le_bytes());
        ph[4..8].copy_from_slice(&PF_R.to_le_bytes());
        ph[16..24].copy_from_slice(&0xffff_8000_0000_0000u64.to_le_bytes());
        ph[32..40].copy_from_slice(&1u64.to_le_bytes());
        ph[40..48].copy_from_slice(&1u64.to_le_bytes());
        ph[48..56].copy_from_slice(&PAGE_SIZE.to_le_bytes());
        assert_eq!(parse_elf(&data), Err(ElfError::SegmentInKernelSpace));
    }

    #[test]
    fn elf_error_is_debug_printable() {
        let text = std::format!("{:?}", ElfError::BadMagic);
        assert_eq!(text, "BadMagic");
    }
}
