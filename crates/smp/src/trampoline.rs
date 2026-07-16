use crate::acpi::CpuEntry;
use crate::ap_entry::{AP_READY_COUNT, ap_entry_rust};
use crate::cpuinfo;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::Ordering;

pub const TRAMPOLINE_PHYS: u64 = 0x8000;
pub const TRAMPOLINE_SIZE: u64 = 0x1000;
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;
const PAGE_SIZE: u64 = 4096;
const AP_STACK_ORDER: u8 = 2;
const AP_STACK_SIZE: u64 = PAGE_SIZE * (1 << AP_STACK_ORDER);
const AP_STARTUP_SPIN_LIMIT: u64 = 100_000;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
core::arch::global_asm!(
    r#"
    .section .text.trampoline,"ax",@progbits
    .set TRAMPOLINE_PHYS_CONST, 0x8000
    .global trampoline_start
    .global trampoline_end
    .global trampoline_pml4_phys
    .global trampoline_stack_top_ptr
    .global trampoline_entry_ptr
    .global trampoline_gdt_ptr
    .global trampoline_gdt
    .set trampoline_32_off, trampoline_32 - trampoline_start
    .set trampoline_64_off, trampoline_64 - trampoline_start
    .set trampoline_pml4_phys_off, trampoline_pml4_phys - trampoline_start
    .set trampoline_stack_top_ptr_off, trampoline_stack_top_ptr - trampoline_start
    .set trampoline_entry_ptr_off, trampoline_entry_ptr - trampoline_start
    .set trampoline_gdt_ptr_off, trampoline_gdt_ptr - trampoline_start

    .code16
    .align 16
trampoline_start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7c00

    /* Direct absolute memory operand. NOT `mov eax, SYM; lgdt [eax]`:
       in GAS Intel syntax `mov eax, SYM` assembles as a LOAD from [SYM]
       (moffs), so eax held the gdt_ptr's CONTENTS and lgdt read a garbage
       descriptor table — #GP(0x08) on the far jump, first real boot. */
    lgdt [TRAMPOLINE_PHYS_CONST + trampoline_gdt_ptr_off]

    mov eax, cr0
    or eax, 1
    mov cr0, eax

    .byte 0x66, 0xea
    .long TRAMPOLINE_PHYS_CONST + trampoline_32_off
    .word 0x08

    .code32
trampoline_32:
    mov ax, 0x10
    mov ds, ax
    mov ss, ax
    mov es, ax

    mov eax, cr4
    or eax, 0x20
    mov cr4, eax

    mov eax, dword ptr [TRAMPOLINE_PHYS_CONST + trampoline_pml4_phys_off]
    mov cr3, eax
    mov eax, cr3

    /* EFER: LME (bit 8) AND NXE (bit 11). The BSP's page tables mark
       data/stack/direct-map pages no-execute; with NXE=0 those PTEs' bit 63
       is RESERVED and the AP's very first stack access after `jmp rax`
       takes a reserved-bit #PF and triple-faults. */
    mov ecx, 0xc0000080
    rdmsr
    or eax, 0x900
    wrmsr

    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax

    .byte 0xea
    .long TRAMPOLINE_PHYS_CONST + trampoline_64_off
    .word 0x18

    .code64
trampoline_64:
    /* Same GAS-Intel-syntax pitfall as the lgdt above: `movabs rax, SYM`
       is a LOAD from [SYM], so the old two-step sequence double-derefed
       the patched values. Load the patch slots directly. */
    mov rsp, qword ptr [TRAMPOLINE_PHYS_CONST + trampoline_stack_top_ptr_off]
    and rsp, -16
    mov rax, qword ptr [TRAMPOLINE_PHYS_CONST + trampoline_entry_ptr_off]
    jmp rax

    .align 8
trampoline_pml4_phys:
    .long 0
trampoline_stack_top_ptr:
    .quad 0
trampoline_entry_ptr:
    .quad 0
trampoline_gdt_ptr:
    .word 0
    .long 0

    .align 8
trampoline_gdt:
    .quad 0x0000000000000000
    .quad 0x00cf9a000000ffff
    .quad 0x00cf92000000ffff
    .quad 0x00af9a000000ffff
    .quad 0x00af92000000ffff

    .align 16
trampoline_end:
    .code64
"#
);

/// Copy and patch the AP trampoline using a freshly allocated scratch stack.
///
/// # Safety
/// The direct map must be live, `TRAMPOLINE_PHYS` must be usable by AP startup,
/// and `pml4_phys` / `ap_entry_virt` must be valid for long-mode entry.
pub unsafe fn install_trampoline(pml4_phys: u64, ap_entry_virt: u64) {
    let stack_top = allocate_ap_stack();
    // SAFETY: caller guarantees the trampoline page is accessible through the
    // direct map and that the supplied addresses are valid.
    unsafe { install_trampoline_for_stack(pml4_phys, ap_entry_virt, stack_top) };
}

/// Start all APs found in MADT.
///
/// # Safety
/// Direct-map and LAPIC/IPI support must be initialized; this must be called
/// once on the BSP after CpuEntry discovery.
pub unsafe fn start_aps(cpus: &[CpuEntry], pml4_phys: u64, _rsdp_phys: u64) -> u32 {
    let bsp_apic_id = kernel_apic::apic_id();
    cpuinfo::register_cpu(0, bsp_apic_id, 0);
    cpuinfo::set_cpu_online(0);

    let mut next_cpu_id = 1u32;
    let mut started = 0u32;

    for cpu in cpus {
        if cpu.apic_id == bsp_apic_id {
            continue;
        }

        let cpu_id = next_cpu_id;
        next_cpu_id += 1;
        let stack_top = allocate_ap_stack();
        cpuinfo::register_cpu(cpu_id, cpu.apic_id, stack_top);

        // SAFETY: the trampoline page is direct-mapped and patched before SIPI.
        unsafe {
            install_trampoline_for_stack(
                pml4_phys,
                ap_entry_rust as *const () as usize as u64,
                stack_top,
            )
        };

        let ready_target = AP_READY_COUNT.load(Ordering::Acquire) + 1;
        kernel_apic::send_init(cpu.apic_id);
        kernel_apic::send_sipi(cpu.apic_id, trampoline_vector());
        wait_sipi_gap();
        kernel_apic::send_sipi(cpu.apic_id, trampoline_vector());

        if wait_for_ready(ready_target) {
            started += 1;
        }
    }

    started
}

fn allocate_ap_stack() -> u64 {
    let stack_phys = kernel_memory::alloc_order(AP_STACK_ORDER);
    (DIRECT_MAP_BASE + stack_phys.as_u64() + AP_STACK_SIZE) & !0xf
}

unsafe fn install_trampoline_for_stack(pml4_phys: u64, ap_entry_virt: u64, stack_top: u64) {
    let dst = trampoline_dst();
    let len = trampoline_len();
    let trampoline_pml4 = copy_pml4_below_4g(pml4_phys);
    assert!(len <= PAGE_SIZE as usize, "AP trampoline exceeds one page");
    debug_assert!(
        trampoline_pml4 < 0x1_0000_0000,
        "PML4 at {:#x} is above 4 GiB - trampoline CR3 will be truncated",
        trampoline_pml4
    );
    assert!(
        trampoline_pml4 <= u32::MAX as u64,
        "AP trampoline supports only low 4 GiB PML4 physical addresses"
    );

    // SAFETY: caller guarantees the destination page is direct-mapped and the
    // source trampoline symbols describe a resident linker section.
    unsafe { ptr::copy_nonoverlapping(trampoline_src(), dst, len) };

    // SAFETY: all patch offsets are computed from linker symbols inside the
    // copied trampoline image and stay within `len`.
    unsafe {
        ptr::write_unaligned(
            dst.add(trampoline_pml4_off()).cast::<u32>(),
            trampoline_pml4 as u32,
        );
        ptr::write_unaligned(dst.add(trampoline_stack_off()).cast::<u64>(), stack_top);
        ptr::write_unaligned(dst.add(trampoline_entry_off()).cast::<u64>(), ap_entry_virt);
        ptr::write_unaligned(
            dst.add(trampoline_gdt_ptr_off()).cast::<u16>(),
            (5 * 8 - 1) as u16,
        );
        ptr::write_unaligned(
            dst.add(trampoline_gdt_ptr_off() + 2).cast::<u32>(),
            (TRAMPOLINE_PHYS + trampoline_gdt_off() as u64) as u32,
        );
    }
}

fn copy_pml4_below_4g(pml4_phys: u64) -> u64 {
    let ap_pml4_phys = kernel_memory::alloc_frame_below_4g();
    let src = (DIRECT_MAP_BASE + pml4_phys) as *const u64;
    let dst = (DIRECT_MAP_BASE + ap_pml4_phys.as_u64()) as *mut u64;
    // SAFETY: both physical frames are accessible through the direct map; the
    // destination is freshly allocated and the PML4 is exactly 512 u64 entries.
    unsafe { ptr::copy_nonoverlapping(src, dst, 512) };
    ap_pml4_phys.as_u64()
}

fn wait_for_ready(target: u32) -> bool {
    let mut spins = 0u64;
    while AP_READY_COUNT.load(Ordering::Acquire) < target {
        spin_loop();
        spins += 1;
        if spins > AP_STARTUP_SPIN_LIMIT {
            return false;
        }
    }
    true
}

fn wait_sipi_gap() {
    for _ in 0..10_000 {
        spin_loop();
    }
}

pub const fn trampoline_vector() -> u8 {
    (TRAMPOLINE_PHYS >> 12) as u8
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn trampoline_src() -> *const u8 {
    unsafe extern "C" {
        static trampoline_start: u8;
    }
    ptr::addr_of!(trampoline_start)
}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn trampoline_src() -> *const u8 {
    core::ptr::null()
}

fn trampoline_dst() -> *mut u8 {
    (DIRECT_MAP_BASE + TRAMPOLINE_PHYS) as *mut u8
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn trampoline_len() -> usize {
    unsafe extern "C" {
        static trampoline_start: u8;
        static trampoline_end: u8;
    }
    // SAFETY: linker emits both symbols in the same section, start <= end.
    unsafe { ptr::addr_of!(trampoline_end).offset_from(ptr::addr_of!(trampoline_start)) as usize }
}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn trampoline_len() -> usize {
    0
}

macro_rules! trampoline_offset_fn {
    ($name:ident, $symbol:ident) => {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        fn $name() -> usize {
            unsafe extern "C" {
                static trampoline_start: u8;
                static $symbol: u8;
            }
            // SAFETY: linker emits both symbols in the same trampoline section.
            unsafe { ptr::addr_of!($symbol).offset_from(ptr::addr_of!(trampoline_start)) as usize }
        }

        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        fn $name() -> usize {
            0
        }
    };
}

trampoline_offset_fn!(trampoline_pml4_off, trampoline_pml4_phys);
trampoline_offset_fn!(trampoline_stack_off, trampoline_stack_top_ptr);
trampoline_offset_fn!(trampoline_entry_off, trampoline_entry_ptr);
trampoline_offset_fn!(trampoline_gdt_ptr_off, trampoline_gdt_ptr);
trampoline_offset_fn!(trampoline_gdt_off, trampoline_gdt);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trampoline_sipi_vector_is_page_number() {
        assert_eq!(TRAMPOLINE_PHYS >> 12, 8);
        assert_eq!(trampoline_vector(), 8);
    }
}
