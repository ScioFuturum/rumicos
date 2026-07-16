/// Kernel paging initialization
///
/// Performs the complete page table setup:
/// 1. Enable NXE in EFER
/// 2. Enable PGE, SMEP, SMAP in CR4
/// 3. Detect and enable PCID if available
/// 4. Build new page tables
/// 5. Map direct physical memory
/// 6. Map kernel sections with correct permissions
/// 7. Load new CR3
use crate::address::{PhysAddr, VirtAddr};
use crate::entry::PageFlags;
use crate::table::PageTableBuilder;
use crate::tlb::{
    self, flush_global, install_page_table, lfence, read_cr4, read_efer, write_cr4, write_efer,
};
use kernel_arch_x86_64::{CpuFeatures, detect_cpu_features};

const AP_TRAMPOLINE_PHYS: u64 = 0x8000;

/// Limine memory map entry type
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LimineMemMapEntry {
    pub base: u64,
    pub length: u64,
    pub mem_type: u32,
}

/// Limine memory map response
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LimineMemMap {
    pub entry_count: u64,
    pub entries: *const *const LimineMemMapEntry,
}

// Memory types from Limine protocol
pub const LIMINE_MEMMAP_USABLE: u32 = 0;
pub const LIMINE_MEMMAP_RESERVED: u32 = 1;
pub const LIMINE_MEMMAP_ACPI_RECLAIMABLE: u32 = 2;
pub const LIMINE_MEMMAP_ACPI_NVS: u32 = 3;
pub const LIMINE_MEMMAP_BAD_MEMORY: u32 = 4;
pub const LIMINE_MEMMAP_BOOTLOADER_RECLAIMABLE: u32 = 5;
pub const LIMINE_MEMMAP_EXECUTABLE_AND_MODULES: u32 = 6;
pub const LIMINE_MEMMAP_FRAMEBUFFER: u32 = 14;

/// Paging initialization structure
pub struct PagingInit {
    features: CpuFeatures,
    memmap: LimineMemMap,
    kernel_phys_base: PhysAddr,
    pcid_enabled: bool,
}

impl PagingInit {
    /// Create paging initializer
    pub fn new(memmap: LimineMemMap, kernel_phys_base: PhysAddr) -> Self {
        let features = detect_cpu_features();
        Self {
            features,
            memmap,
            kernel_phys_base,
            pcid_enabled: false,
        }
    }

    /// Get direct map base.
    ///
    /// Always the 4-level higher-half base: this kernel never sets
    /// CR4.LA57, so 5-level canonical addresses like 0xff00_0000_0000_0000
    /// do not exist even when the CPU advertises `la57` in CPUID (QEMU's
    /// `-cpu max` does — keying off the FEATURE bit here, as the code did
    /// before 2026-07-10, produced a non-canonical direct map base and no
    /// direct map at all). Every other crate hardcodes this same constant
    /// as DIRECT_MAP_BASE.
    fn direct_map_base(&self) -> VirtAddr {
        VirtAddr::new_unchecked(0xffff_8000_0000_0000)
    }

    /// Enable processor features needed for paging
    fn enable_features(&mut self) {
        // SAFETY: These MSR/CR writes are safe during boot.
        // CR4.PCIDE must only be enabled when CR3[11:0] == 0 to avoid #GP.
        // CR4.SMAP is enabled now, but user-space direct access without STAC/CLAC
        // must be added later before copy_from_user/copy_to_user paths.
        unsafe {
            // Enable NXE in EFER
            let efer = read_efer();
            let efer_with_nxe = efer | (1u64 << 11); // EFER.NXE bit
            write_efer(efer_with_nxe);
            serial_marker(b'a');

            // Enable CR4 flags that are safe on the bootloader's page tables.
            // SMEP/SMAP are enabled after Rumicos installs its own supervisor
            // mappings; Limine may use different U/S attributes temporarily.
            let cr4 = read_cr4();
            serial_marker(b'b');
            let mut new_cr4 = cr4 | (1u64 << 7); // PGE

            if self.features.pcid {
                let cr3 = tlb::read_cr3();
                serial_marker(b'c');
                if (cr3 & 0xFFF) != 0 {
                    panic!("Cannot enable PCIDE: CR3.PCID must be zero before PCIDE is set");
                }

                new_cr4 |= 1u64 << 17; // PCIDE
                self.pcid_enabled = true;
            }

            write_cr4(new_cr4);
            serial_marker(b'd');

            // Publish what was actually enabled so TLB helpers and
            // kernel-proc's PCID plumbing can adapt: QEMU's TCG (the
            // fallback when no hardware accelerator is available) exposes
            // neither PCID nor INVPCID, and an unconditional INVPCID would
            // #UD on the first execve.
            crate::tlb::set_pcid_support(self.pcid_enabled, self.features.invpcid);

            // LA57 check: TODO: set CR4.LA57 if supported
            // For now, we gate this at compile-time: 4-level paging only
            if self.features.la57 {
                // TODO: uncomment when 5-level paging is fully implemented
                // let cr4_with_la57 = new_cr4 | (1u64 << 12);
                // write_cr4(cr4_with_la57);
            }
        }
    }

    fn enable_supervisor_protections(&self) {
        unsafe {
            // SAFETY: Rumicos-owned page tables are active, with kernel mappings
            // supervisor-only and user mappings explicitly marked U/S.
            let mut cr4 = read_cr4();
            if self.features.smep {
                cr4 |= 1u64 << 20;
            }
            if self.features.smap {
                cr4 |= 1u64 << 21;
            }
            write_cr4(cr4);
        }
    }

    /// Build the direct physical map for all usable memory regions
    fn map_direct_memory(&self, builder: &mut PageTableBuilder) {
        let direct_map_base = self.direct_map_base();

        // SAFETY: memmap.entries is valid for memmap.entry_count entries
        let entries = unsafe {
            core::slice::from_raw_parts(self.memmap.entries, self.memmap.entry_count as usize)
        };

        for &entry_ptr in entries {
            let entry = unsafe {
                // SAFETY: Limine supplies `entry_count` valid pointers in the memmap response.
                &*entry_ptr
            };
            // Skip non-mappable regions. EXECUTABLE_AND_MODULES (the kernel
            // image itself) IS included: early page-table frames live in the
            // image (paging's bump pool), and code all over the kernel
            // accesses page tables and process frames through their
            // DIRECT_MAP_BASE + phys alias — the first AP bring-up
            // (copy_pml4_below_4g) faulted on exactly that missing alias on
            // the first real boot. The alias is writable but no-execute;
            // the image's own higher-half mappings keep the real W^X flags.
            match entry.mem_type {
                LIMINE_MEMMAP_USABLE
                | LIMINE_MEMMAP_BOOTLOADER_RECLAIMABLE
                | LIMINE_MEMMAP_ACPI_RECLAIMABLE
                | LIMINE_MEMMAP_EXECUTABLE_AND_MODULES
                | LIMINE_MEMMAP_FRAMEBUFFER => {}
                _ => continue,
            }

            let phys_start = PhysAddr::new(entry.base);
            let phys_end = PhysAddr::new(entry.base + entry.length);

            // Determine flags based on memory type
            let flags = if entry.mem_type == LIMINE_MEMMAP_FRAMEBUFFER {
                // Framebuffer: use WriteThrough as UC- substitute (TODO: MTRR/PAT)
                PageFlags::new()
                    .with_present()
                    .with_writable()
                    .with_global()
                    .with_write_through()
                    .with_no_execute()
            } else {
                // Regular memory: writable, global, no-execute
                PageFlags::new()
                    .with_present()
                    .with_writable()
                    .with_global()
                    .with_no_execute()
            };

            // Try to use 1 GiB pages where alignment allows
            let aligned_start_1g = phys_start.align_up_1g();
            let aligned_end_1g = phys_end.align_down_1g();

            if aligned_start_1g < aligned_end_1g {
                // Map 1 GiB pages in the middle
                let mut addr = aligned_start_1g;
                while addr < aligned_end_1g {
                    let virt = direct_map_base + addr.as_u64();
                    self.map_huge_page_1g(builder, virt, addr, flags);
                    addr = addr + (1024 * 1024 * 1024);
                }

                // Handle alignment head/tail with 2 MiB pages
                if phys_start < aligned_start_1g {
                    self.map_region_2m(builder, phys_start, aligned_start_1g, flags);
                }
                if aligned_end_1g < phys_end {
                    self.map_region_2m(builder, aligned_end_1g, phys_end, flags);
                }
            } else {
                // Region too small for 1 GiB mapping, use 2 MiB
                self.map_region_2m(builder, phys_start, phys_end, flags);
            }
        }
    }

    /// Map a region using 2 MiB pages
    fn map_region_2m(
        &self,
        builder: &mut PageTableBuilder,
        phys_start: PhysAddr,
        phys_end: PhysAddr,
        flags: PageFlags,
    ) {
        let direct_map_base = self.direct_map_base();

        let aligned_start = phys_start.align_up_2m();
        let aligned_end = phys_end.align_down_2m();

        // Map 2 MiB pages
        if aligned_start < aligned_end {
            let mut phys = aligned_start;
            while phys < aligned_end {
                let virt = direct_map_base + phys.as_u64();
                self.map_huge_page_2m(builder, virt, phys, flags);
                phys = phys + (2 * 1024 * 1024);
            }
        }

        // Handle unaligned head/tail with 4 KiB pages
        if phys_start < aligned_start {
            self.map_region_4k(builder, phys_start, aligned_start, flags);
        }
        if aligned_end < phys_end {
            self.map_region_4k(builder, aligned_end, phys_end, flags);
        }
    }

    /// Map a region using 4 KiB pages
    fn map_region_4k(
        &self,
        builder: &mut PageTableBuilder,
        phys_start: PhysAddr,
        phys_end: PhysAddr,
        flags: PageFlags,
    ) {
        let direct_map_base = self.direct_map_base();

        let mut phys = phys_start.align_down_4k();
        let end = phys_end.align_up_4k();

        while phys < end {
            let virt = direct_map_base + phys.as_u64();
            builder.map_page(virt, phys, flags);
            phys = phys + 4096;
        }
    }

    /// Map a 1 GiB huge page
    fn map_huge_page_1g(
        &self,
        builder: &mut PageTableBuilder,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageFlags,
    ) {
        // TODO: Implement 1 GiB huge page mapping
        // For now, fall back to 2 MiB mapping
        self.map_huge_page_2m(builder, virt, phys, flags);
    }

    /// Map a 2 MiB huge page.
    ///
    /// A REAL huge page (PS bit in the PD entry) — the pre-2026-07-10
    /// "fall back to 512 x 4 KiB pages" stub needed ~256 page-table frames
    /// per GiB of RAM and exhausted the 128-frame early bump pool on the
    /// very first boot before the direct map was half built.
    fn map_huge_page_2m(
        &self,
        builder: &mut PageTableBuilder,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageFlags,
    ) {
        builder.map_huge_2m(virt, phys, flags);
    }

    /// Map kernel text/data/bss sections
    fn map_kernel_sections(&self, builder: &mut PageTableBuilder) {
        // These symbols are defined by the linker script
        unsafe extern "C" {
            static __text_start: u8;
            static __text_end: u8;
            static __rodata_start: u8;
            static __rodata_end: u8;
            static __data_start: u8;
            static __data_end: u8;
            static __bss_start: u8;
            static __bss_end: u8;
        }

        let kernel_base = VirtAddr::new(0xFFFF_FFFF_8000_0000);

        // Map text section (no write, no execute)
        let text_start = unsafe { &__text_start as *const u8 as u64 };
        let text_end = unsafe { &__text_end as *const u8 as u64 };
        let text_phys_start =
            PhysAddr::new(text_start - kernel_base.as_u64() + self.kernel_phys_base.as_u64());

        for i in (text_phys_start.as_u64()..text_phys_start.as_u64() + (text_end - text_start))
            .step_by(4096)
        {
            let virt = VirtAddr::new(i - self.kernel_phys_base.as_u64() + kernel_base.as_u64());
            let phys = PhysAddr::new(i);
            let flags = PageFlags::new().with_present().with_global();
            builder.map_page(virt, phys, flags);
        }

        // Map rodata section (no write, no execute)
        let rodata_start = unsafe { &__rodata_start as *const u8 as u64 };
        let rodata_end = unsafe { &__rodata_end as *const u8 as u64 };
        let rodata_phys_start =
            PhysAddr::new(rodata_start - kernel_base.as_u64() + self.kernel_phys_base.as_u64());

        for i in (rodata_phys_start.as_u64()
            ..rodata_phys_start.as_u64() + (rodata_end - rodata_start))
            .step_by(4096)
        {
            let virt = VirtAddr::new(i - self.kernel_phys_base.as_u64() + kernel_base.as_u64());
            let phys = PhysAddr::new(i);
            let flags = PageFlags::new()
                .with_present()
                .with_global()
                .with_no_execute();
            builder.map_page(virt, phys, flags);
        }

        // Map data section (writable, no execute)
        let data_start = unsafe { &__data_start as *const u8 as u64 };
        let data_end = unsafe { &__data_end as *const u8 as u64 };
        let data_phys_start =
            PhysAddr::new(data_start - kernel_base.as_u64() + self.kernel_phys_base.as_u64());

        for i in (data_phys_start.as_u64()..data_phys_start.as_u64() + (data_end - data_start))
            .step_by(4096)
        {
            let virt = VirtAddr::new(i - self.kernel_phys_base.as_u64() + kernel_base.as_u64());
            let phys = PhysAddr::new(i);
            let flags = PageFlags::new()
                .with_present()
                .with_writable()
                .with_global()
                .with_no_execute();
            builder.map_page(virt, phys, flags);
        }

        // Map bss section (writable, no execute)
        let bss_start = unsafe { &__bss_start as *const u8 as u64 };
        let bss_end = unsafe { &__bss_end as *const u8 as u64 };
        let bss_phys_start =
            PhysAddr::new(bss_start - kernel_base.as_u64() + self.kernel_phys_base.as_u64());

        for i in
            (bss_phys_start.as_u64()..bss_phys_start.as_u64() + (bss_end - bss_start)).step_by(4096)
        {
            let virt = VirtAddr::new(i - self.kernel_phys_base.as_u64() + kernel_base.as_u64());
            let phys = PhysAddr::new(i);
            let flags = PageFlags::new()
                .with_present()
                .with_writable()
                .with_global()
                .with_no_execute();
            builder.map_page(virt, phys, flags);
        }
    }

    /// Identity-map the low AP trampoline page used immediately after SIPI.
    fn map_ap_trampoline_identity(&self, builder: &mut PageTableBuilder) {
        let addr = PhysAddr::new(AP_TRAMPOLINE_PHYS);
        let flags = PageFlags::new()
            .with_present()
            .with_writable()
            .with_global();
        builder.map_page(VirtAddr::new(AP_TRAMPOLINE_PHYS), addr, flags);
    }

    /// Perform the complete initialization
    pub fn init(mut self) {
        // Step 1: Enable required features
        self.enable_features();
        unsafe { serial_marker(b'1') };

        // Step 2: Build page tables
        let mut builder = PageTableBuilder::new();
        unsafe { serial_marker(b'2') };

        // Step 3: Map direct physical memory
        self.map_direct_memory(&mut builder);
        unsafe { serial_marker(b'3') };

        // Step 4: Map kernel sections
        self.map_kernel_sections(&mut builder);
        unsafe { serial_marker(b'4') };

        // Step 5: Map AP startup trampoline identity page
        self.map_ap_trampoline_identity(&mut builder);
        unsafe { serial_marker(b'5') };

        // Step 6: Load new page table
        let pml4_phys = builder.pml4_phys();
        let cr3 = crate::entry::make_cr3(pml4_phys, 0, false);
        unsafe { serial_marker(b'6') };

        unsafe {
            install_page_table(cr3);
            flush_global();
            lfence();
        }
        self.enable_supervisor_protections();
        unsafe { serial_marker(b'7') };
    }
}

unsafe fn serial_marker(byte: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") 0x3f8u16, in("al") byte, options(nomem, nostack));
    }
}

/// Initialize paging subsystem
pub fn init(memmap: LimineMemMap, kernel_phys_base: PhysAddr) {
    unsafe { serial_marker(b'x') };
    // The early bump allocator's frames live inside the kernel image; give
    // it the image's virt→phys offset BEFORE the first frame is allocated
    // so it can hand out real physical addresses (see allocator.rs).
    crate::allocator::set_kernel_image_va_offset(
        0xFFFF_FFFF_8000_0000u64.wrapping_sub(kernel_phys_base.as_u64()),
    );
    let paging = PagingInit::new(memmap, kernel_phys_base);
    unsafe { serial_marker(b'y') };
    paging.init();
}
