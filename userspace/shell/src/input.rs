//! Line input from fd 0.
//!
//! ## Why this busy-polls
//!
//! `/dev/serial`'s `read()` (`kernel_fs::devfs::devserial_read`) is a
//! **non-blocking poll**: it checks the UART's line-status "data ready" bit
//! and returns immediately — returning **0** when the FIFO is empty rather
//! than sleeping. No IRQ + WaitQueue blocking read was ever wired up (it was
//! noted as future work in the original devfs checkpoint, and is still
//! future work now). So `read_line` must spin: call `read()` in a tight
//! loop, discard the 0-length "nothing yet" results, and only accumulate
//! when it returns > 0.
//!
//! **This burns one CPU core at 100% between keystrokes.** That is accepted
//! for this checkpoint — it matches the existing serial driver's limits and
//! only affects the interactive mode, which is a manual convenience. The
//! real fix is a blocking read backed by a serial IRQ handler and a
//! `WaitQueue`, exactly like the pipe's blocking reader already does.
//!
//! ## Why this echoes
//!
//! There is no tty layer and no line discipline: the serial line is raw, so
//! nothing echoes typed characters back. Without the explicit echo below,
//! an interactive user would type blind. Backspace support is a minimal
//! approximation of cooked mode — enough to fix a typo, not a real line
//! editor (no arrow keys, no history, no kill-line). A proper tty is
//! deferred to a future keyboard/tty checkpoint.

use liblow::fmt::write_all;
use liblow::{STDIN, STDOUT, read};

pub const LINE_BUF_SIZE: usize = 256;

const CR: u8 = b'\r';
const LF: u8 = b'\n';
const BS: u8 = 0x08;
const DEL: u8 = 0x7f;

/// Read one line from fd 0 into `buf`, stopping at a newline (which is not
/// stored) or when the buffer is full.
///
/// Returns the number of bytes in the line. Returns 0 for a genuinely empty
/// line, so callers cannot distinguish "blank line" from EOF by the return
/// value alone — the parser reports `ParseError::Empty` for a blank line and
/// the REPL just reprompts, which makes the distinction moot in practice.
/// A real EOF is not observable on this device: the serial `read()` never
/// signals one, it only ever returns 0 for "no data yet".
pub fn read_line(buf: &mut [u8; LINE_BUF_SIZE]) -> usize {
    let mut len = 0usize;
    let mut one = [0u8; 1];

    loop {
        let n = read(STDIN, &mut one);
        if n <= 0 {
            // Non-blocking device: 0 means "nothing queued yet", not EOF.
            // Negative would be an error; treat it the same and keep
            // polling rather than dying on a transient.
            core::hint::spin_loop();
            continue;
        }

        match one[0] {
            CR | LF => {
                // Terminate the line. Echo a newline so the cursor moves.
                let _ = write_all(STDOUT, b"\n");
                return len;
            }
            BS | DEL => {
                if len > 0 {
                    len -= 1;
                    // Destructive backspace: move left, overwrite with a
                    // space, move left again.
                    let _ = write_all(STDOUT, b"\x08 \x08");
                }
            }
            c => {
                if len < buf.len() {
                    buf[len] = c;
                    len += 1;
                    let _ = write_all(STDOUT, &one);
                } else {
                    // Full: refuse the byte rather than overflow, and let
                    // the user hear about it.
                    let _ = write_all(STDOUT, b"\x07"); // BEL
                }
            }
        }
    }
}
