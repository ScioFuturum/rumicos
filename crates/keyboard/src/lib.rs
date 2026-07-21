//! Interrupt-driven PS/2 keyboard input.
//!
//! Layered like the rest of this kernel's device code:
//!
//! * [`scancode`] — pure Set-1 decode state machine, host-tested.
//! * [`KeyboardRing`] — a byte ring buffer plus the decode state, guarded by
//!   a `SpinLock`, with a separate `WaitQueue` for blocked readers. This
//!   mirrors `kernel_fs::pipe`'s `PipeRing` exactly: the wait queue lives
//!   *outside* the ring lock so a blocking reader always drops the lock
//!   before `thread_block`, and the IRQ handler wakes readers through the
//!   IRQ-safe path (`wake_one_from_irq`) without ever calling `schedule()`.
//! * [`keyboard_irq_handler`] — the IRQ1 handler: read the data port (which
//!   clears the device-level interrupt), EOI the LAPIC, then decode, buffer
//!   and wake.
//! * [`keyboard_read`] — the blocking read backing `/dev/keyboard`.
//!
//! The byte stream `keyboard_read` produces is deliberately identical in
//! shape to what `/dev/serial` delivered: raw ASCII, `\n` on Enter, `0x08`
//! on Backspace. So the shell's `read_line()` — and everything above the
//! syscall layer — needs no change; only the device backing fd 0 changes.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod scancode;

use scancode::{KeyEvent, KeyboardState};

/// Keystroke buffer capacity. 256 bytes is far more than a human can
/// outrun between reads; a full ring drops its oldest byte rather than
/// growing or blocking in interrupt context.
pub const KBD_RING_SIZE: usize = 256;

// ─── ring buffer (pure: host-testable, no target deps) ─────────────────────

/// The keyboard's byte ring plus its decode state. Pure logic; the
/// `SpinLock` and `WaitQueue` that make it usable from an IRQ live outside
/// it (see [`KBD_RING`] / [`KBD_WAITERS`]).
pub struct KeyboardRing {
    buf: [u8; KBD_RING_SIZE],
    read_pos: usize,
    write_pos: usize,
    len: usize,
    state: KeyboardState,
}

impl KeyboardRing {
    pub const fn new() -> Self {
        Self {
            buf: [0; KBD_RING_SIZE],
            read_pos: 0,
            write_pos: 0,
            len: 0,
            state: KeyboardState::new(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push one decoded byte, dropping the OLDEST byte if the ring is full.
    /// Never blocks or grows — it runs in interrupt context.
    pub fn push(&mut self, byte: u8) {
        if self.len == KBD_RING_SIZE {
            // Full: discard the oldest unread byte to make room.
            self.read_pos = (self.read_pos + 1) % KBD_RING_SIZE;
            self.len -= 1;
        }
        self.buf[self.write_pos] = byte;
        self.write_pos = (self.write_pos + 1) % KBD_RING_SIZE;
        self.len += 1;
    }

    /// Copy up to `dst.len()` buffered bytes into `dst`, advancing the read
    /// cursor. Returns the number copied (0 iff empty).
    pub fn pop_into(&mut self, dst: &mut [u8]) -> usize {
        let n = core::cmp::min(dst.len(), self.len);
        for (i, d) in dst.iter_mut().enumerate().take(n) {
            *d = self.buf[(self.read_pos + i) % KBD_RING_SIZE];
        }
        self.read_pos = (self.read_pos + n) % KBD_RING_SIZE;
        self.len -= n;
        n
    }

    /// Feed a raw scancode through the decode state machine, buffering a
    /// byte if one was produced. Returns `true` iff a byte was pushed (i.e.
    /// a waiter should be woken). Kept here, off the target-only IRQ path,
    /// so the decode-to-buffer step is itself host-testable.
    pub fn feed(&mut self, scancode: u8) -> bool {
        match self.state.feed_scancode(scancode) {
            KeyEvent::Char(byte) => {
                self.push(byte);
                true
            }
            _ => false,
        }
    }
}

impl Default for KeyboardRing {
    fn default() -> Self {
        Self::new()
    }
}

// ─── shared state ──────────────────────────────────────────────────────────

use kernel_sched::WaitQueue;
use kernel_sync::SpinLock;

// The shared state and PS/2 port definitions below are consumed only by the
// target-only IRQ handler / blocking read / controller init; on a host build
// (where those are cfg'd out) they are legitimately dead.

/// The one keyboard's ring. `SpinLock` guards the buffer + decode state.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
static KBD_RING: SpinLock<KeyboardRing> = SpinLock::new(KeyboardRing::new());

/// Readers blocked in [`keyboard_read`]. Outside the ring lock, per the
/// no-lock-across-block discipline (same as `pipe.rs`).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
static KBD_WAITERS: WaitQueue = WaitQueue::new();

// ─── PS/2 controller ports ─────────────────────────────────────────────────

/// Data port (read a scancode, write a device command).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const PS2_DATA: u16 = 0x60;
/// Status register (read) — same port as the command register (write).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const PS2_STATUS: u16 = 0x64;
/// Command register (write).
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const PS2_CMD: u16 = 0x64;

#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const PS2_STATUS_OUTPUT_FULL: u8 = 1 << 0; // a byte is ready to read
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
const PS2_STATUS_INPUT_FULL: u8 = 1 << 1; // controller busy, do not write

/// Bounded poll count — a wedged controller must not hang the boot. At a few
/// nanoseconds per `in`, this is well under a millisecond of spinning.
#[cfg(target_os = "none")]
const PS2_POLL_LIMIT: u32 = 100_000;

// ─── IRQ handler ───────────────────────────────────────────────────────────

/// IRQ1 handler for [`kernel_apic::KEYBOARD_VECTOR`].
///
/// Signature matches this kernel's `Handler` (a plain `fn`, as `timer_handler`
/// uses) rather than the `extern "C" fn` the checkpoint brief sketched —
/// registration goes through the same `kernel_cpu::register_handler` path.
///
/// Reading the data port is what clears the interrupt at the DEVICE level;
/// EOI clears it at the LAPIC level. Both are required, and the data-port
/// read comes first (matching the timer handler's "read then EOI then do
/// the rest" ordering).
#[cfg(target_os = "none")]
pub fn keyboard_irq_handler(_frame: &mut kernel_cpu::InterruptFrame, _vec: u8) {
    // SAFETY: registered only for KEYBOARD_VECTOR; reading 0x60 acknowledges
    // the i8042's output-buffer-full condition so it will raise IRQ1 again.
    let scancode = unsafe { inb(PS2_DATA) };
    kernel_apic::eoi();

    let produced = {
        let mut ring = KBD_RING.lock();
        ring.feed(scancode)
    }; // ring lock dropped here, before any wake

    if produced {
        // IRQ-safe wake: marks one waiter runnable and enqueues it; never
        // calls schedule() from interrupt context (the next tick picks it up).
        kernel_sched::wake_one_from_irq(&KBD_WAITERS);
    }
}

// ─── blocking read (backs /dev/keyboard) ───────────────────────────────────

/// Blocking read for `/dev/keyboard`.
///
/// Returns immediately with whatever is buffered; if the ring is empty it
/// drops the lock and blocks on [`KBD_WAITERS`] until the IRQ handler wakes
/// it, then re-checks (spurious-wakeup safe — same shape as `pipe_read`).
///
/// The `/dev/serial` read this replaces polled and returned 0 on an empty
/// FIFO; this one genuinely blocks, so the shell's `read_line()` no longer
/// busy-spins a core between keystrokes. The byte stream it returns is
/// otherwise identical, so nothing above the syscall layer changes.
pub fn keyboard_read(buf: &mut [u8]) -> i64 {
    #[cfg(target_os = "none")]
    {
        if buf.is_empty() {
            return 0;
        }
        loop {
            {
                let mut ring = KBD_RING.lock();
                if !ring.is_empty() {
                    return ring.pop_into(buf) as i64;
                }
            } // drop the ring lock BEFORE blocking
            // SAFETY: no lock is held here; the IRQ handler wakes this queue
            // when a byte arrives. A lost wakeup at worst waits for the next
            // keystroke — acceptable for human-speed input, and the same
            // window pipe.rs's blocking reader accepts.
            unsafe { kernel_sched::thread_block(&KBD_WAITERS) };
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = buf;
        -38 // ENOSYS on host
    }
}

// ─── PS/2 controller init ──────────────────────────────────────────────────

/// Initialize the i8042 PS/2 controller and the keyboard device, and enable
/// IRQ1 delivery. Translation is left ON, so the controller delivers
/// Scancode Set 1 (what [`scancode`] decodes and what QEMU's i8042 emits by
/// default). Every diagnostic step (self-test, device reset) logs-and-
/// continues rather than panicking, so odd emulation or hardware never
/// wedges the boot.
///
/// # Safety
/// Run once at boot, before interrupts are enabled and after the keyboard
/// IRQ is routed through the I/O APIC (no point taking an IRQ for a device
/// that is not up yet).
#[cfg(target_os = "none")]
pub unsafe fn init_ps2_keyboard() {
    // SAFETY: single-threaded boot context; these are the architectural
    // i8042 ports. Each helper bounds its own polling.
    unsafe {
        // 1. Disable both device ports so nothing interferes during setup.
        ps2_command(0xAD); // disable port 1 (keyboard)
        ps2_command(0xA7); // disable port 2 (mouse)

        // 2. Flush any stale byte sitting in the output buffer.
        flush_output_buffer();

        // 3. Read the controller configuration byte (command 0x20).
        ps2_command(0x20);
        let mut config = ps2_read_data();

        // 4. Enable IRQ1 (bit 0), disable the mouse IRQ12 (bit 1), and leave
        //    translation (bit 6) untouched — it is on, so Set 1 is delivered.
        config |= 1 << 0;
        config &= !(1 << 1);

        // 5. Write the configuration byte back (command 0x60).
        ps2_command(0x60);
        ps2_write_data(config);

        // 6. Controller self-test (0xAA -> 0x55). Log, don't hard-fail.
        ps2_command(0xAA);
        let _self_test = ps2_read_data(); // expect 0x55

        // 7. Enable the keyboard port.
        ps2_command(0xAE);

        // 8. Reset the keyboard device (0xFF -> 0xFA ACK, then 0xAA pass).
        ps2_write_data(0xFF);
        let _ack = ps2_read_data(); // expect 0xFA
        let _pass = ps2_read_data(); // expect 0xAA
    }
}

// ─── port I/O + bounded polling (target-only) ──────────────────────────────

/// Wait until the controller can accept a write (input buffer empty), or the
/// poll budget runs out. Returns `false` on timeout.
///
/// # Safety: reads the i8042 status port.
#[cfg(target_os = "none")]
unsafe fn wait_input_clear() -> bool {
    for _ in 0..PS2_POLL_LIMIT {
        // SAFETY: 0x64 read returns the status register.
        if unsafe { inb(PS2_STATUS) } & PS2_STATUS_INPUT_FULL == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Wait until a byte is available to read (output buffer full), or the poll
/// budget runs out. Returns `false` on timeout.
///
/// # Safety: reads the i8042 status port.
#[cfg(target_os = "none")]
unsafe fn wait_output_full() -> bool {
    for _ in 0..PS2_POLL_LIMIT {
        // SAFETY: 0x64 read returns the status register.
        if unsafe { inb(PS2_STATUS) } & PS2_STATUS_OUTPUT_FULL != 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Send a command byte to the controller (port 0x64), waiting for it to be
/// ready first.
///
/// # Safety: issues a controller command; caller sequences the protocol.
#[cfg(target_os = "none")]
unsafe fn ps2_command(cmd: u8) {
    // SAFETY: bounded wait, then a write to the command port.
    unsafe {
        wait_input_clear();
        outb(PS2_CMD, cmd);
    }
}

/// Write a data byte (port 0x60), waiting for the controller to be ready.
///
/// # Safety: writes the data port; caller sequences the protocol.
#[cfg(target_os = "none")]
unsafe fn ps2_write_data(data: u8) {
    // SAFETY: bounded wait, then a write to the data port.
    unsafe {
        wait_input_clear();
        outb(PS2_DATA, data);
    }
}

/// Read a data byte (port 0x60), waiting for one to be available. Returns 0
/// on timeout (callers treat every device reply as advisory).
///
/// # Safety: reads the data port.
#[cfg(target_os = "none")]
unsafe fn ps2_read_data() -> u8 {
    // SAFETY: bounded wait, then a read of the data port.
    unsafe {
        if wait_output_full() {
            inb(PS2_DATA)
        } else {
            0
        }
    }
}

/// Drain any bytes already sitting in the output buffer.
///
/// # Safety: reads status and data ports.
#[cfg(target_os = "none")]
unsafe fn flush_output_buffer() {
    for _ in 0..PS2_POLL_LIMIT {
        // SAFETY: status read; if empty we are done.
        if unsafe { inb(PS2_STATUS) } & PS2_STATUS_OUTPUT_FULL == 0 {
            return;
        }
        // SAFETY: discard the pending byte.
        let _ = unsafe { inb(PS2_DATA) };
    }
}

/// # Safety: `port` must be a valid I/O port for an 8-bit read here.
#[cfg(target_os = "none")]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: caller guarantees the port is valid to read.
    unsafe {
        core::arch::asm!("in al, dx", in("dx") port, lateout("al") value,
            options(nomem, nostack));
    }
    value
}

/// # Safety: `port` must be a valid I/O port for an 8-bit write here.
#[cfg(target_os = "none")]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: caller guarantees the port is valid to write.
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") value,
            options(nomem, nostack));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_push_pop_roundtrips_bytes() {
        let mut r = KeyboardRing::new();
        assert!(r.is_empty());
        for b in b"hi" {
            r.push(*b);
        }
        assert!(!r.is_empty());
        let mut out = [0u8; 8];
        assert_eq!(r.pop_into(&mut out), 2);
        assert_eq!(&out[..2], b"hi");
        assert!(r.is_empty());
    }

    #[test]
    fn ring_partial_read_leaves_the_rest() {
        let mut r = KeyboardRing::new();
        for b in b"abc" {
            r.push(*b);
        }
        let mut one = [0u8; 1];
        assert_eq!(r.pop_into(&mut one), 1);
        assert_eq!(one[0], b'a');
        let mut rest = [0u8; 8];
        assert_eq!(r.pop_into(&mut rest), 2);
        assert_eq!(&rest[..2], b"bc");
    }

    #[test]
    fn ring_drops_oldest_when_full() {
        let mut r = KeyboardRing::new();
        // Fill exactly to capacity with a known pattern.
        for i in 0..KBD_RING_SIZE {
            r.push(i as u8);
        }
        // One more byte drops the oldest (0) and appends 0xFF.
        r.push(0xFF);
        let mut out = [0u8; KBD_RING_SIZE];
        let n = r.pop_into(&mut out);
        assert_eq!(n, KBD_RING_SIZE);
        assert_eq!(out[0], 1, "oldest byte (0) must have been dropped");
        assert_eq!(out[KBD_RING_SIZE - 1], 0xFF, "newest byte kept");
    }

    #[test]
    fn ring_read_on_empty_returns_zero() {
        let mut r = KeyboardRing::new();
        let mut out = [0u8; 4];
        assert_eq!(r.pop_into(&mut out), 0);
    }

    #[test]
    fn feed_buffers_a_decoded_char_and_reports_it() {
        let mut r = KeyboardRing::new();
        // 'a' make code produces a byte; a modifier does not.
        assert!(r.feed(0x1E), "printable key should buffer a byte");
        assert!(!r.feed(0x2A), "shift press buffers nothing");
        let mut out = [0u8; 4];
        assert_eq!(r.pop_into(&mut out), 1);
        assert_eq!(out[0], b'a');
    }

    #[test]
    fn feed_wraps_around_the_ring() {
        let mut r = KeyboardRing::new();
        // Advance read/write near the end, then feed across the wrap.
        for _ in 0..(KBD_RING_SIZE - 2) {
            r.push(0);
        }
        let mut sink = [0u8; KBD_RING_SIZE - 2];
        r.pop_into(&mut sink);
        // write_pos is now near the end; feed 'a' 'b' straddling the wrap.
        assert!(r.feed(0x1E)); // 'a'
        assert!(r.feed(0x30)); // 'b'
        let mut out = [0u8; 2];
        assert_eq!(r.pop_into(&mut out), 2);
        assert_eq!(&out, b"ab");
    }

    #[test]
    fn ring_size_is_a_documented_constant() {
        assert_eq!(KBD_RING_SIZE, 256);
    }
}
