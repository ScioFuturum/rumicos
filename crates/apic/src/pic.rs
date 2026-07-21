//! Legacy 8259A PIC shutdown.
//!
//! Rumicos is APIC-native: the LAPIC timer drives scheduling, IPIs carry
//! TLB shootdowns and reschedule kicks, and ISA device interrupts (the
//! keyboard's IRQ1) are routed through the I/O APIC (see [`crate::ioapic`]).
//! The legacy PIC has no role in that architecture, so it is remapped and
//! then fully masked once at boot.
//!
//! ## Why remap a PIC we are about to mask
//!
//! Masking alone is not enough. Both PICs power on delivering IRQ0-15 on
//! vectors 0x08-0x0F and 0x70-0x77 — 0x08-0x0F overlap the CPU's own
//! exception vectors (#DF is 0x08, #GP is 0x0D). A single spurious IRQ
//! slipping through during the transition would be decoded by the CPU as a
//! double fault or a general protection fault instead of a device
//! interrupt. Every x86 kernel therefore remaps first and masks second,
//! even when it plans to disable the PIC entirely — that is what the
//! sequence below does.
//!
//! The remap target ([`PIC1_VECTOR_BASE`]) deliberately avoids this
//! kernel's live vectors: 0x20 is `kernel_apic::TIMER_VECTOR`, 0x21 is
//! [`crate::ioapic::KEYBOARD_VECTOR`], 0xfb/0xfc are the reschedule and
//! shootdown IPIs, 0xff is the LAPIC spurious vector. 0xE0-0xEF is free.

use core::arch::asm;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// ICW1: begin initialization, expect ICW4.
const ICW1_INIT_WITH_ICW4: u8 = 0x11;
/// ICW3 for the master: a slave PIC is cascaded on IRQ2 (bit 2).
const ICW3_MASTER_SLAVE_ON_IRQ2: u8 = 0x04;
/// ICW3 for the slave: its cascade identity is 2.
const ICW3_SLAVE_IDENTITY: u8 = 0x02;
/// ICW4: 8086/88 mode.
const ICW4_8086_MODE: u8 = 0x01;
/// OCW1 with every bit set: mask all eight lines on this PIC.
const MASK_ALL: u8 = 0xFF;

/// Vector base the master PIC is remapped to before being masked. Purely
/// defensive — see the module docs for why this is not left at the
/// power-on default, and why 0xE0 specifically is safe in this kernel.
pub const PIC1_VECTOR_BASE: u8 = 0xE0;
/// Vector base for the slave PIC (IRQ8-15), immediately after the master's.
pub const PIC2_VECTOR_BASE: u8 = 0xE8;

/// Remap both 8259A PICs clear of the CPU exception range, then mask every
/// one of their 16 IRQ lines.
///
/// After this returns, no interrupt can originate from the legacy PIC and
/// all device interrupts must be routed through the I/O APIC.
///
/// # Safety
/// Must run exactly once at boot, on the BSP, with interrupts disabled and
/// before any I/O APIC routing is programmed. Writes to the PIC command
/// and data ports are unconditional and would corrupt in-flight interrupt
/// state if issued while interrupts were live.
pub unsafe fn disable_pic() {
    // SAFETY: caller guarantees boot context with IF=0; these ports are the
    // architectural 8259A command/data registers, present on every PC-
    // compatible system (and emulated by QEMU's q35 machine).
    unsafe {
        // ICW1: start the initialization sequence on both chips.
        outb(PIC1_CMD, ICW1_INIT_WITH_ICW4);
        io_wait();
        outb(PIC2_CMD, ICW1_INIT_WITH_ICW4);
        io_wait();

        // ICW2: vector base for each chip.
        outb(PIC1_DATA, PIC1_VECTOR_BASE);
        io_wait();
        outb(PIC2_DATA, PIC2_VECTOR_BASE);
        io_wait();

        // ICW3: cascade wiring (slave hangs off the master's IRQ2).
        outb(PIC1_DATA, ICW3_MASTER_SLAVE_ON_IRQ2);
        io_wait();
        outb(PIC2_DATA, ICW3_SLAVE_IDENTITY);
        io_wait();

        // ICW4: 8086 mode (as opposed to the obsolete MCS-80/85 mode).
        outb(PIC1_DATA, ICW4_8086_MODE);
        io_wait();
        outb(PIC2_DATA, ICW4_8086_MODE);
        io_wait();

        // OCW1: mask every line on both chips. This is the actual
        // "disable"; everything above only made the transition safe.
        outb(PIC1_DATA, MASK_ALL);
        io_wait();
        outb(PIC2_DATA, MASK_ALL);
        io_wait();
    }
}

/// Read back both PIC mask registers as `(master, slave)`.
///
/// Only used to verify [`disable_pic`] took effect (both should read
/// `0xFF`); kept because a silently-unmasked PIC produces interrupts that
/// are very hard to attribute after the fact.
///
/// # Safety
/// Same context requirements as [`disable_pic`].
pub unsafe fn read_masks() -> (u8, u8) {
    // SAFETY: reading a PIC data port with no ICW sequence in progress
    // returns OCW1, the interrupt mask register.
    unsafe { (inb(PIC1_DATA), inb(PIC2_DATA)) }
}

/// Give the PIC time to latch a command byte.
///
/// The 8259A needs a short settle between back-to-back writes. The
/// traditional trick is a write to POST port 0x80, which is unused on
/// modern systems and costs roughly one ISA bus cycle.
///
/// # Safety
/// Writing port 0x80 is inert on PC-compatible systems (and in QEMU).
#[inline(always)]
unsafe fn io_wait() {
    // SAFETY: port 0x80 is the POST diagnostic port; writes have no effect.
    unsafe { outb(0x80, 0) };
}

/// # Safety
/// `port` must be a valid I/O port for an 8-bit write in this context.
#[inline(always)]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: caller guarantees the port is valid to write.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack));
    }
}

/// # Safety
/// `port` must be a valid I/O port for an 8-bit read in this context.
#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: caller guarantees the port is valid to read.
    unsafe {
        asm!("in al, dx", in("dx") port, lateout("al") value, options(nomem, nostack));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::assertions_on_constants)] // documents an invariant as a named test
    fn remap_base_avoids_cpu_exception_vectors() {
        // The whole point of remapping: IRQ0-15 must not land on 0x00-0x1F,
        // where the CPU's own exceptions live.
        assert!(PIC1_VECTOR_BASE >= 0x20);
        assert!(PIC2_VECTOR_BASE >= 0x20);
    }

    #[test]
    fn remap_bases_are_contiguous_and_eight_apart() {
        // Each PIC owns eight consecutive vectors; the slave follows the
        // master immediately, so the 16 legacy lines form one block.
        assert_eq!(PIC2_VECTOR_BASE, PIC1_VECTOR_BASE + 8);
    }

    #[test]
    fn remap_range_does_not_collide_with_live_kernel_vectors() {
        // Vectors this kernel actually uses (see the module docs).
        const TIMER: u8 = 0x20;
        const KEYBOARD: u8 = crate::ioapic::KEYBOARD_VECTOR;
        const RESCHED: u8 = 0xfb;
        const SHOOTDOWN: u8 = 0xfc;
        const SPURIOUS: u8 = 0xff;

        let pic_range = PIC1_VECTOR_BASE..=(PIC2_VECTOR_BASE + 7);
        for live in [TIMER, KEYBOARD, RESCHED, SHOOTDOWN, SPURIOUS] {
            assert!(
                !pic_range.contains(&live),
                "PIC remap range collides with live vector {live:#x}"
            );
        }
    }

    #[test]
    fn icw4_selects_8086_mode() {
        assert_eq!(ICW4_8086_MODE, 1);
        // ICW1 must request that ICW4 is sent at all, or the chip never
        // reads the 8086-mode byte.
        assert_eq!(ICW1_INIT_WITH_ICW4 & 0x01, 1);
    }

    #[test]
    fn mask_all_masks_every_line() {
        assert_eq!(MASK_ALL.count_ones(), 8);
    }
}
