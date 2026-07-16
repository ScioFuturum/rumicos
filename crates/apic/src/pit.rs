use core::arch::asm;
use core::hint::spin_loop;

pub const PIT_HZ: u64 = 1_193_182;

pub const PIT_CHANNEL0_DATA: u16 = 0x40;
pub const PIT_CHANNEL2_DATA: u16 = 0x42;
pub const PIT_CMD: u16 = 0x43;
pub const KBC_PORT_B: u16 = 0x61;

const PIT_CHANNEL2_ONESHOT: u8 = 0xb0;
const KBC_GATE2: u8 = 1 << 0;
const KBC_SPEAKER: u8 = 1 << 1;
const KBC_OUT2: u8 = 1 << 5;
const POST_PORT: u16 = 0x80;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PitChannel {
    Channel0,
    Channel2,
}

impl PitChannel {
    pub const fn data_port(self) -> u16 {
        match self {
            Self::Channel0 => PIT_CHANNEL0_DATA,
            Self::Channel2 => PIT_CHANNEL2_DATA,
        }
    }
}

/// Write a byte to an I/O port.
///
/// # Safety
/// Caller must run at CPL0 and ensure the port is valid for byte output.
#[inline(always)]
pub unsafe fn outb(port: u16, val: u8) {
    // SAFETY: caller guarantees CPL0 I/O permission and a valid byte port.
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") val,
            options(nomem, nostack, preserves_flags),
        )
    };
}

/// Read a byte from an I/O port.
///
/// # Safety
/// Caller must run at CPL0 and ensure the port is valid for byte input.
#[inline(always)]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: caller guarantees CPL0 I/O permission and a valid byte port.
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            lateout("al") val,
            options(nomem, nostack, preserves_flags),
        )
    };
    val
}

/// Delay long enough for legacy I/O devices to settle.
///
/// # Safety
/// Caller must run at CPL0. Port 0x80 is the conventional POST delay port.
#[inline(always)]
pub unsafe fn io_delay() {
    // SAFETY: caller guarantees CPL0 I/O permission; POST port writes are used
    // only for timing delay and the value is ignored by modern systems.
    unsafe { outb(POST_PORT, 0) };
}

/// Set PIT channel 2 to count down from `ticks`.
///
/// Does not start the countdown yet.
///
/// # Safety
/// I/O port access must be from ring 0 and the caller must serialize PIT
/// calibration against other legacy PIT users.
pub unsafe fn pit_prepare(ticks: u16) {
    // SAFETY: caller guarantees CPL0 I/O access to the KBC/PIT ports.
    let port_b = unsafe { inb(KBC_PORT_B) };
    let gated_silent = (port_b | KBC_GATE2) & !KBC_SPEAKER;
    // SAFETY: caller guarantees CPL0 I/O access; this enables PIT channel 2
    // gate while keeping the speaker off.
    unsafe { outb(KBC_PORT_B, gated_silent) };
    // SAFETY: caller guarantees CPL0 I/O access; delay orders legacy port I/O.
    unsafe { io_delay() };

    // SAFETY: caller guarantees CPL0 I/O access; 0xb0 selects channel 2,
    // lo/hi byte access, mode 0 one-shot, binary counting.
    unsafe { outb(PIT_CMD, PIT_CHANNEL2_ONESHOT) };
    // SAFETY: caller guarantees CPL0 I/O access; delay orders legacy port I/O.
    unsafe { io_delay() };
    // SAFETY: caller guarantees CPL0 I/O access; PIT expects low byte first.
    unsafe { outb(PIT_CHANNEL2_DATA, ticks as u8) };
    // SAFETY: caller guarantees CPL0 I/O access; delay orders legacy port I/O.
    unsafe { io_delay() };
    // SAFETY: caller guarantees CPL0 I/O access; high byte completes reload.
    unsafe { outb(PIT_CHANNEL2_DATA, (ticks >> 8) as u8) };
    // SAFETY: caller guarantees CPL0 I/O access; delay orders legacy port I/O.
    unsafe { io_delay() };
}

/// Start PIT channel 2 countdown by toggling the gate.
///
/// # Safety
/// I/O port access must be from ring 0 and channel 2 must have been prepared.
pub unsafe fn pit_start() {
    // SAFETY: caller guarantees CPL0 I/O access to the KBC/PIT ports.
    let port_b = unsafe { inb(KBC_PORT_B) } & !KBC_SPEAKER;
    // SAFETY: caller guarantees CPL0 I/O access; clearing gate arms the edge.
    unsafe { outb(KBC_PORT_B, port_b & !KBC_GATE2) };
    // SAFETY: caller guarantees CPL0 I/O access; delay orders legacy port I/O.
    unsafe { io_delay() };
    // SAFETY: caller guarantees CPL0 I/O access; setting gate starts mode 0.
    unsafe { outb(KBC_PORT_B, port_b | KBC_GATE2) };
    // SAFETY: caller guarantees CPL0 I/O access; delay orders legacy port I/O.
    unsafe { io_delay() };
}

/// Wait until PIT channel 2 OUT goes high.
///
/// Returns the number of spin iterations used while polling.
///
/// # Safety
/// I/O port access must be from ring 0 and channel 2 must be counting.
pub unsafe fn pit_wait() -> u64 {
    let mut spins = 0u64;
    loop {
        // SAFETY: caller guarantees CPL0 I/O access to the KBC port.
        let port_b = unsafe { inb(KBC_PORT_B) };
        if (port_b & KBC_OUT2) != 0 {
            return spins;
        }
        spins = spins.wrapping_add(1);
        spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pit_channel_data_ports_match_arch_ports() {
        assert_eq!(PitChannel::Channel0.data_port(), 0x40);
        assert_eq!(PitChannel::Channel2.data_port(), 0x42);
    }
}
