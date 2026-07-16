#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod address;
mod init;
mod limine;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};
use kernel_arch_x86_64::cycles::{halt, pause};
use kernel_arch_x86_64::{CpuFeatures, detect_cpu_features};
use kernel_sync::SeqLock;

static CPU_FEATURES: SeqLock<CpuFeatures> = SeqLock::new(CpuFeatures::empty());
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    kernel_main();
    idle_forever()
}

fn kernel_main() {
    unsafe {
        serial_debug_init();
        serial_debug_byte(b'K');
    }
    let features = detect_cpu_features();
    CPU_FEATURES.write(|slot| *slot = features);
    let _direct_map = address::direct_map_addr(0, features.la57);
    let _page_geometry = (
        address::PAGE_SIZE,
        address::HUGE_PAGE_2M,
        address::HUGE_PAGE_1G,
        address::KERNEL_BASE,
    );

    kernel_cpu::init_cpu(0);
    unsafe { serial_debug_byte(b'C') }

    unsafe { serial_debug_byte(b'L') }
    let memmap = limine::get_memmap();
    unsafe { serial_debug_byte(b'N') }
    let kernel_phys = limine::get_executable_address();
    unsafe { serial_debug_byte(b'E') }

    let limine_memmap = kernel_paging::init::LimineMemMap {
        entry_count: memmap.entry_count,
        entries: memmap
            .entries
            .cast::<*const kernel_paging::init::LimineMemMapEntry>(),
    };
    kernel_paging::init::init(limine_memmap, kernel_phys);
    unsafe { serial_debug_byte(b'G') }

    let bump_range = kernel_paging::bump_consumed_range();
    unsafe { serial_debug_byte(b'h') }
    let kernel_range = limine::get_kernel_phys_range();
    unsafe { serial_debug_byte(b'i') }
    let bump_range = (
        kernel_memory::PhysAddr::new(bump_range.0.as_u64()),
        kernel_memory::PhysAddr::new(bump_range.1.as_u64()),
    );
    let kernel_range = (
        kernel_memory::PhysAddr::new(kernel_range.0.as_u64()),
        kernel_memory::PhysAddr::new(kernel_range.1.as_u64()),
    );

    const MAX_MEMMAP_ENTRIES: usize = 128;
    let memmap_ptrs =
        unsafe { core::slice::from_raw_parts(memmap.entries, memmap.entry_count as usize) };
    assert!(
        memmap_ptrs.len() <= MAX_MEMMAP_ENTRIES,
        "too many memmap entries"
    );
    let mut memmap_storage = [kernel_memory::LimineMemmapEntry {
        base: 0,
        length: 0,
        mem_type: 0,
    }; MAX_MEMMAP_ENTRIES];
    for (dst, &src) in memmap_storage.iter_mut().zip(memmap_ptrs.iter()) {
        let e = unsafe { &*src };
        *dst = kernel_memory::LimineMemmapEntry {
            base: e.base,
            length: e.length,
            mem_type: e.mem_type,
        };
    }
    unsafe { serial_debug_byte(b'j') }
    let memmap_entries = &memmap_storage[..memmap_ptrs.len()];
    let extra_reserved = [(
        kernel_memory::PhysAddr::new(kernel_smp::TRAMPOLINE_PHYS),
        kernel_memory::PhysAddr::new(kernel_smp::TRAMPOLINE_PHYS + kernel_smp::TRAMPOLINE_SIZE),
    )];
    unsafe {
        kernel_memory::init_frame_allocator(
            memmap_entries,
            bump_range,
            kernel_range,
            &extra_reserved,
        )
    }
    unsafe { serial_debug_byte(b'M') }

    kernel_paging::set_global_numa_buddy_allocator();
    kernel_apic::init_bsp();
    unsafe { serial_debug_byte(b'A') }
    kernel_cpu::register_handler(kernel_apic::TIMER_VECTOR, timer_handler);
    let _ticks_per_ms = kernel_apic::calibrate_timer();
    unsafe { serial_debug_byte(b'T') }

    let rsdp_phys = limine::get_rsdp_phys();
    let (cpus, cpu_count) = unsafe { kernel_smp::parse_madt(rsdp_phys) };
    unsafe { serial_debug_byte(b'D') }
    let pml4_phys = kernel_paging::current_pml4_phys();
    let ap_count =
        unsafe { kernel_smp::trampoline::start_aps(&cpus[..cpu_count], pml4_phys, rsdp_phys) };
    unsafe { serial_debug_byte(b'S') }
    while kernel_smp::ap_entry::AP_READY_COUNT.load(Ordering::Acquire) < ap_count {
        core::hint::spin_loop();
    }
    unsafe { serial_debug_byte(b'W') }

    kernel_sched::init(kernel_smp::online_count());
    unsafe { serial_debug_byte(b'R') }

    // ── VFS / process init ────────────────────────────────────────────────
    // Order: proc init → fs init (registers serial VNode + chain handler)
    //        → Process::create (pre-opens fd 0/1/2 to serial)
    kernel_proc::init();
    unsafe { serial_debug_byte(b'P') }

    kernel_fs::init_fs(); // mounts ramfs, unpacks initrd, registers VFS syscalls
    unsafe { serial_debug_byte(b'F') }

    // Spawn the embedded init process. init.asm itself now demonstrates
    // SYS_EXECVE by replacing its own image with /init2.elf from the
    // initrd — see init/init.asm and initrd/init2.asm — so init2 is no
    // longer spawned here as a second, separate process.
    let init_proc = unsafe {
        // SAFETY: INIT_ELF is a valid static ELF image embedded in the kernel.
        kernel_proc::Process::create(init::INIT_ELF, "init")
    };
    unsafe { serial_debug_byte(b'c') }
    kernel_sched::enqueue_thread(unsafe { (*init_proc).thread }, 0);
    // ─────────────────────────────────────────────────────────────────────

    kernel_sched::spawn(idle_kernel_thread, "kidle");
    kernel_smp::ap_entry::SCHEDULER_STARTED.store(true, Ordering::Release);
    kernel_apic::set_timer_oneshot(1);
    unsafe { serial_debug_byte(b's') }
    // SAFETY: first schedule; interrupts enabled via SYSRET RFLAGS for ring-3.
    unsafe {
        kernel_sched::schedule(0);
    }
}

// ─── serial debug helpers ─────────────────────────────────────────────────

unsafe fn serial_debug_byte(b: u8) {
    unsafe {
        while serial_debug_read(0x3fd) & 0x20 == 0 {
            core::hint::spin_loop();
        }
        core::arch::asm!("out dx, al", in("dx") 0x3f8u16, in("al") b,
            options(nomem, nostack));
    }
}
unsafe fn serial_debug_init() {
    unsafe {
        serial_debug_write(0x3f9, 0x00);
        serial_debug_write(0x3fb, 0x80);
        serial_debug_write(0x3f8, 0x03);
        serial_debug_write(0x3f9, 0x00);
        serial_debug_write(0x3fb, 0x03);
        serial_debug_write(0x3fa, 0xc7);
        serial_debug_write(0x3fc, 0x0b);
    }
}
unsafe fn serial_debug_write(port: u16, v: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") v,
        options(nomem, nostack));
    }
}
unsafe fn serial_debug_read(port: u16) -> u8 {
    let v: u8;
    unsafe {
        core::arch::asm!("in al, dx", in("dx") port, lateout("al") v,
        options(nomem, nostack));
    }
    v
}

// ─── timer / scheduling ───────────────────────────────────────────────────

fn timer_handler(_frame: &mut kernel_cpu::InterruptFrame, _vec: u8) {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    kernel_apic::eoi();
    kernel_apic::set_timer_oneshot(1);
    let cpu_id = kernel_sched::current_cpu_id();
    unsafe {
        kernel_sched::schedule(cpu_id);
    }
}

pub fn uptime_ms() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

fn idle_kernel_thread() -> ! {
    loop {
        kernel_sched::thread_yield();
    }
}

#[inline(never)]
fn idle_forever() -> ! {
    loop {
        pause();
        unsafe { halt() };
    }
}

/// `core::fmt::Write` sink over the raw COM1 debug port, for the panic path
/// only — no locks, no allocation.
struct PanicSerial;

impl core::fmt::Write for PanicSerial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            // SAFETY: raw polled COM1 write; serial was initialized at boot.
            unsafe { serial_debug_byte(b) };
        }
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    // Get the full panic report out over COM1 — a silent spin loop here
    // cost a full debugging round on the first real QEMU boot.
    use core::fmt::Write;
    let _ = write!(PanicSerial, "\r\n!PANIC {}\r\n", info);
    loop {
        pause();
    }
}
