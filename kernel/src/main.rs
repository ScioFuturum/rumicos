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
    // Mask the legacy 8259 PICs outright: this kernel routes every interrupt
    // through the LAPIC/I/O APIC, so a live PIC could only ever double-
    // deliver or inject spurious IRQs. Must happen before interrupts are
    // enabled (they are, at the scheduler start below).
    // SAFETY: boot context, IF=0, run exactly once on the BSP.
    unsafe { kernel_apic::disable_pic() };
    unsafe { serial_debug_byte(b'A') }
    kernel_cpu::register_handler(kernel_apic::TIMER_VECTOR, timer_handler);
    let _ticks_per_ms = kernel_apic::calibrate_timer();
    unsafe { serial_debug_byte(b'T') }

    let rsdp_phys = limine::get_rsdp_phys();
    let madt = unsafe { kernel_smp::parse_madt(rsdp_phys) };
    let bsp_apic_id = kernel_apic::apic_id();
    unsafe { serial_debug_byte(b'D') }
    let pml4_phys = kernel_paging::current_pml4_phys();
    let ap_count = unsafe {
        kernel_smp::trampoline::start_aps(&madt.cpus[..madt.cpu_count], pml4_phys, rsdp_phys)
    };
    unsafe { serial_debug_byte(b'S') }
    while kernel_smp::ap_entry::AP_READY_COUNT.load(Ordering::Acquire) < ap_count {
        core::hint::spin_loop();
    }
    unsafe { serial_debug_byte(b'W') }

    // ── PCI enumeration (virtio-net Part A) ───────────────────────────────
    // Brute-force scan the PCI buses and report every function found, its
    // decoded BARs/sizes, and any MSI-X capability. Uses config-space port
    // I/O only (no MMIO, no I/O APIC); MSI-X delivers straight to a LAPIC.
    pci_report();

    // ── virtio-net (Part B) ───────────────────────────────────────────────
    // Bind the driver to the enumerated device, run the transmit self-test,
    // and register the RX completion handler. Must run before any user
    // address space is created: mapping the device BARs adds kernel-half
    // page-table entries that later address spaces inherit by sharing
    // (see kernel_paging::mmio::map_mmio_region).
    kernel_cpu::register_handler(VIRTIO_RX_VECTOR, virtio_rx_handler);
    // SAFETY: PCI is enumerated; BSP boot context, no user address space yet.
    match unsafe { kernel_virtio::init(bsp_apic_id, VIRTIO_RX_VECTOR) } {
        Some(r) => {
            boot_log(format_args!(
                "virtio-net: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (device {:04x})\r\n",
                r.mac[0], r.mac[1], r.mac[2], r.mac[3], r.mac[4], r.mac[5], r.device_id,
            ));
            boot_log(format_args!(
                "virtio-net: tx self-test {}\r\n",
                if r.tx_ok {
                    "OK - frame returned in used ring"
                } else {
                    "FAIL - no completion"
                },
            ));
        }
        None => boot_log(format_args!("virtio-net: no device or init failed\r\n")),
    }

    kernel_sched::init(kernel_smp::online_count());
    // Reschedule kick: when kernel-sched parks a thread on a REMOTE CPU's
    // run queue, broadcast RESCHED_VECTOR so idle APs (which have no
    // periodic timer — only the BSP does) wake from `sti; hlt` and drain
    // their queues. Without this a pipeline stage forked onto an idle AP
    // starves forever; see kernel_sched::register_kick_hook's docs.
    kernel_cpu::register_handler(RESCHED_VECTOR, resched_handler);
    kernel_sched::register_kick_hook(resched_kick);
    unsafe { serial_debug_byte(b'R') }

    // ── PS/2 keyboard ─────────────────────────────────────────────────────
    // Route the keyboard's ISA IRQ1 through the I/O APIC to KEYBOARD_VECTOR
    // on the BSP, register its handler, then bring the i8042 controller up.
    // The APIC MMIO window was mapped in paging init; the legacy PIC was
    // masked above. If routing fails (no I/O APIC covers the GSI) the system
    // still boots — it just has no keyboard, so don't init the controller.
    kernel_cpu::register_handler(
        kernel_apic::KEYBOARD_VECTOR,
        kernel_keyboard::keyboard_irq_handler,
    );
    // SAFETY: boot context, IF=0; the I/O APIC MMIO is mapped and the topology
    // came from this boot's MADT.
    let kbd_routed =
        unsafe { kernel_apic::ioapic::init_keyboard_irq_routing(bsp_apic_id, &madt.ioapics, &madt.isos) };
    if kbd_routed {
        // SAFETY: run once at boot, IF=0, after the IRQ is routed.
        unsafe { kernel_keyboard::init_ps2_keyboard() };
    }
    unsafe { serial_debug_byte(if kbd_routed { b'K' } else { b'k' }) }

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

// ─── PCI enumeration report (virtio-net Part A) ───────────────────────────

/// Format one line to COM1. Reuses the panic path's raw polled writer; this
/// runs on the BSP at boot with no other CPU touching the serial port yet.
fn boot_log(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let _ = PanicSerial.write_fmt(args);
}

/// Enumerate the PCI buses and print a report: every function, its BARs and
/// their sizes, its MSI-X table size if present, and a distinct line for any
/// virtio device (vendor `0x1AF4`). This is the Part A verification output.
fn pci_report() {
    let n = kernel_pci::enumerate_pci();
    boot_log(format_args!("\r\npci: enumerated {n} function(s)\r\n"));
    for i in 0..n {
        let Some(d) = kernel_pci::get(i) else { continue };
        boot_log(format_args!(
            "pci: {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}:{:02x} prog_if {:02x}\r\n",
            d.bus, d.device, d.function, d.vendor_id, d.device_id, d.class, d.subclass, d.prog_if,
        ));
        for b in 0..6 {
            if d.bar_sizes[b] != 0 {
                boot_log(format_args!(
                    "pci:   bar{} {} base {:#x} size {:#x}\r\n",
                    b,
                    if d.bar_is_mmio[b] { "mmio" } else { "io  " },
                    d.bars[b],
                    d.bar_sizes[b],
                ));
            }
        }
        if d.has_msix {
            boot_log(format_args!(
                "pci:   msix capability, table size {}\r\n",
                d.msix_table_size,
            ));
        }
        if d.vendor_id == 0x1af4 {
            boot_log(format_args!(
                "virtio: device {:04x}:{:04x} found at {:02x}:{:02x}.{}\r\n",
                d.vendor_id, d.device_id, d.bus, d.device, d.function,
            ));
        }
    }
}

// ─── timer / scheduling ───────────────────────────────────────────────────

/// Reschedule-kick IPI vector. 0x20 is the timer, 0xfc the TLB shootdown
/// (see kernel_proc::shootdown); 0xfb is otherwise unused.
const RESCHED_VECTOR: u8 = 0xfb;

/// virtio-net RX-completion MSI-X vector. Clear of 0x20 (timer), 0x21
/// (keyboard), 0x80 (syscall INT), 0xfb/0xfc (IPIs) and 0xff (spurious).
const VIRTIO_RX_VECTOR: u8 = 0x40;

/// MSI-X handler for virtio-net receive completions. EOI first (matching
/// every other IRQ handler here), then drain the RX ring — printing each
/// frame's length and first bytes, which is all "processing" means until the
/// network stack arrives. Never blocks or schedules.
fn virtio_rx_handler(_frame: &mut kernel_cpu::InterruptFrame, _vec: u8) {
    kernel_apic::eoi();
    // SAFETY: interrupt context on the BSP; the device was initialized at boot.
    unsafe {
        kernel_virtio::rx_poll(|frame| {
            boot_log(format_args!("virtio-net: rx {} bytes:", frame.len()));
            for b in frame.iter().take(16) {
                boot_log(format_args!(" {:02x}", b));
            }
            boot_log(format_args!("\r\n"));
        });
    }
}

/// Broadcast the reschedule kick to every other CPU. Registered with
/// kernel-sched as its remote-kick hook. Broadcasting (rather than
/// targeting the one CPU) avoids needing a cpu-index → APIC-ID map here;
/// a CPU with nothing new in its queue just schedules and goes back to
/// sleep.
fn resched_kick() {
    // SAFETY: LAPICs are initialized on every online CPU before the hook
    // is registered; RESCHED_VECTOR has a registered handler.
    unsafe {
        kernel_apic::send_ipi(
            kernel_apic::IpiDest::Others,
            kernel_apic::IpiDelivery::Fixed(RESCHED_VECTOR),
        );
    }
}

/// Handler for [`RESCHED_VECTOR`]: acknowledge and let the scheduler pick
/// up whatever just landed in this CPU's run queue.
fn resched_handler(_frame: &mut kernel_cpu::InterruptFrame, _vec: u8) {
    kernel_apic::eoi();
    if kernel_smp::ap_entry::SCHEDULER_STARTED.load(Ordering::Acquire) {
        let cpu_id = kernel_sched::current_cpu_id();
        unsafe {
            kernel_sched::schedule(cpu_id);
        }
    }
}

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
