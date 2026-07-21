//! PS/2 Scancode Set 1 decoding for a US QWERTY layout.
//!
//! This module is **pure**: no `unsafe`, no port I/O, no syscalls. It is a
//! small state machine that turns a stream of raw scancode bytes into
//! [`KeyEvent`]s, so it can be exercised exhaustively with `cargo test` on
//! the host — the same "pure decision core, unsafe glue elsewhere"
//! discipline the shell parser and the CoW resolver follow.
//!
//! ## Scancode Set 1 in one paragraph
//!
//! Each key has a one-byte *make* code, sent on press. The matching
//! *break* code, sent on release, is `make | 0x80` — bit 7 set means
//! "released". A handful of keys to the right of the main block (arrows,
//! right-hand Ctrl/Alt, …) send a two-byte sequence prefixed with `0xE0`;
//! this checkpoint does not map any of them, but it consumes the prefix
//! correctly so it never leaks into the next keystroke. QEMU's i8042
//! delivers Set 1 by default (the controller's translate bit is on), which
//! is why we decode Set 1 rather than raw Set 2.

/// A modifier key whose press/release we track.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModKey {
    LShift,
    RShift,
    LCtrl,
    LAlt,
    CapsLock,
}

/// The result of feeding one scancode byte through [`KeyboardState::feed_scancode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyEvent {
    /// A decoded ASCII byte ready for the input stream.
    Char(u8),
    /// A modifier changed state (the state machine already applied it).
    Modifier { key: ModKey, pressed: bool },
    /// A scancode we intentionally do not map to anything — key releases of
    /// printable keys, unmapped keys, and every extended (`0xE0`-prefixed)
    /// sequence. Function keys and arrows fall here and are future work.
    Ignored,
}

// ─── make/break codes we care about ───────────────────────────────────────

const EXTENDED_PREFIX: u8 = 0xE0;
const BREAK_BIT: u8 = 0x80;

const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_LCTRL: u8 = 0x1D;
const SC_LALT: u8 = 0x38;
const SC_CAPS: u8 = 0x3A;

/// Per-keyboard decode state. Small and `Copy`: the ring buffer owns one.
#[derive(Clone, Copy, Debug)]
pub struct KeyboardState {
    pub lshift: bool,
    pub rshift: bool,
    pub lctrl: bool,
    pub caps_lock: bool,
    /// Set on seeing `0xE0`, consumed by the very next byte.
    pub pending_extended: bool,
}

impl KeyboardState {
    pub const fn new() -> Self {
        Self {
            lshift: false,
            rshift: false,
            lctrl: false,
            caps_lock: false,
            pending_extended: false,
        }
    }

    /// `true` while either shift key is held.
    #[inline]
    pub const fn shift(&self) -> bool {
        self.lshift || self.rshift
    }

    /// Feed one raw Set-1 scancode byte and return what it produced.
    ///
    /// Modifier state is updated in place; printable keys yield
    /// [`KeyEvent::Char`] on press only (releases are [`KeyEvent::Ignored`]).
    pub fn feed_scancode(&mut self, byte: u8) -> KeyEvent {
        // An extended sequence: the prefix set the flag last call; this
        // byte completes it. We map none of the extended keys, but we MUST
        // clear the flag so it cannot bleed into the following keystroke.
        if self.pending_extended {
            self.pending_extended = false;
            return KeyEvent::Ignored;
        }
        if byte == EXTENDED_PREFIX {
            self.pending_extended = true;
            return KeyEvent::Ignored;
        }

        let released = byte & BREAK_BIT != 0;
        let code = byte & !BREAK_BIT;

        // Modifiers: both edges matter (a shift release must un-shift).
        match code {
            SC_LSHIFT => {
                self.lshift = !released;
                return KeyEvent::Modifier {
                    key: ModKey::LShift,
                    pressed: !released,
                };
            }
            SC_RSHIFT => {
                self.rshift = !released;
                return KeyEvent::Modifier {
                    key: ModKey::RShift,
                    pressed: !released,
                };
            }
            SC_LCTRL => {
                self.lctrl = !released;
                return KeyEvent::Modifier {
                    key: ModKey::LCtrl,
                    pressed: !released,
                };
            }
            SC_LALT => {
                // Tracked as an event but no state field yet (out of scope).
                return KeyEvent::Modifier {
                    key: ModKey::LAlt,
                    pressed: !released,
                };
            }
            SC_CAPS => {
                // Caps Lock toggles on press and is inert on release.
                if !released {
                    self.caps_lock = !self.caps_lock;
                    return KeyEvent::Modifier {
                        key: ModKey::CapsLock,
                        pressed: true,
                    };
                }
                return KeyEvent::Ignored;
            }
            _ => {}
        }

        // Printable keys: only presses produce characters.
        if released {
            return KeyEvent::Ignored;
        }

        let (base, shifted) = set1_chars(code);
        if base == 0 {
            return KeyEvent::Ignored;
        }

        KeyEvent::Char(self.apply_modifiers(base, shifted))
    }

    /// Resolve the final ASCII byte for a key given the current modifiers.
    ///
    /// Caps Lock affects letters only (XORed with Shift); Shift affects both
    /// letters and the shifted punctuation/number symbols.
    #[inline]
    const fn apply_modifiers(&self, base: u8, shifted: u8) -> u8 {
        if base.is_ascii_lowercase() {
            // Letters: upper-case when Shift XOR Caps Lock.
            let upper = self.shift() ^ self.caps_lock;
            if upper { base - 0x20 } else { base }
        } else if self.shift() && shifted != 0 {
            shifted
        } else {
            base
        }
    }
}

impl Default for KeyboardState {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a Set-1 make code to its `(unshifted, shifted)` ASCII bytes for a US
/// QWERTY layout. `(0, 0)` means "not a printable key we map".
///
/// Enter, Tab and Backspace decode to their control bytes (`\n`, `\t`,
/// `0x08`) so the byte stream matches exactly what `/dev/serial` delivered
/// and `read_line()` already understands.
pub const fn set1_chars(code: u8) -> (u8, u8) {
    match code {
        // number row
        0x02 => (b'1', b'!'),
        0x03 => (b'2', b'@'),
        0x04 => (b'3', b'#'),
        0x05 => (b'4', b'$'),
        0x06 => (b'5', b'%'),
        0x07 => (b'6', b'^'),
        0x08 => (b'7', b'&'),
        0x09 => (b'8', b'*'),
        0x0A => (b'9', b'('),
        0x0B => (b'0', b')'),
        0x0C => (b'-', b'_'),
        0x0D => (b'=', b'+'),
        0x0E => (0x08, 0x08), // Backspace
        0x0F => (b'\t', b'\t'), // Tab
        // top letter row
        0x10 => (b'q', b'Q'),
        0x11 => (b'w', b'W'),
        0x12 => (b'e', b'E'),
        0x13 => (b'r', b'R'),
        0x14 => (b't', b'T'),
        0x15 => (b'y', b'Y'),
        0x16 => (b'u', b'U'),
        0x17 => (b'i', b'I'),
        0x18 => (b'o', b'O'),
        0x19 => (b'p', b'P'),
        0x1A => (b'[', b'{'),
        0x1B => (b']', b'}'),
        0x1C => (b'\n', b'\n'), // Enter
        // home letter row
        0x1E => (b'a', b'A'),
        0x1F => (b's', b'S'),
        0x20 => (b'd', b'D'),
        0x21 => (b'f', b'F'),
        0x22 => (b'g', b'G'),
        0x23 => (b'h', b'H'),
        0x24 => (b'j', b'J'),
        0x25 => (b'k', b'K'),
        0x26 => (b'l', b'L'),
        0x27 => (b';', b':'),
        0x28 => (b'\'', b'"'),
        0x29 => (b'`', b'~'),
        0x2B => (b'\\', b'|'),
        // bottom letter row
        0x2C => (b'z', b'Z'),
        0x2D => (b'x', b'X'),
        0x2E => (b'c', b'C'),
        0x2F => (b'v', b'V'),
        0x30 => (b'b', b'B'),
        0x31 => (b'n', b'N'),
        0x32 => (b'm', b'M'),
        0x33 => (b',', b'<'),
        0x34 => (b'.', b'>'),
        0x35 => (b'/', b'?'),
        0x37 => (b'*', b'*'), // keypad *
        0x39 => (b' ', b' '), // Space
        _ => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(state: &mut KeyboardState, bytes: &[u8]) -> KeyEvent {
        let mut last = KeyEvent::Ignored;
        for &b in bytes {
            last = state.feed_scancode(b);
        }
        last
    }

    #[test]
    fn plain_letter_press_decodes_to_lowercase() {
        let mut s = KeyboardState::new();
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'a'));
    }

    #[test]
    fn enter_space_backspace_tab_decode_to_control_bytes() {
        let mut s = KeyboardState::new();
        assert_eq!(s.feed_scancode(0x1C), KeyEvent::Char(b'\n'));
        assert_eq!(s.feed_scancode(0x39), KeyEvent::Char(b' '));
        assert_eq!(s.feed_scancode(0x0E), KeyEvent::Char(0x08));
        assert_eq!(s.feed_scancode(0x0F), KeyEvent::Char(b'\t'));
    }

    #[test]
    fn key_release_of_a_printable_key_emits_nothing() {
        let mut s = KeyboardState::new();
        // 'a' make then 'a' break (0x1E | 0x80 = 0x9E).
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'a'));
        assert_eq!(s.feed_scancode(0x9E), KeyEvent::Ignored);
    }

    #[test]
    fn shift_held_uppercases_letters_and_shifts_symbols() {
        let mut s = KeyboardState::new();
        // Left shift down.
        assert_eq!(
            s.feed_scancode(SC_LSHIFT),
            KeyEvent::Modifier {
                key: ModKey::LShift,
                pressed: true
            }
        );
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'A'));
        assert_eq!(s.feed_scancode(0x02), KeyEvent::Char(b'!')); // shift+1
        // Left shift up: back to lowercase.
        assert_eq!(
            s.feed_scancode(SC_LSHIFT | BREAK_BIT),
            KeyEvent::Modifier {
                key: ModKey::LShift,
                pressed: false
            }
        );
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'a'));
        assert_eq!(s.feed_scancode(0x02), KeyEvent::Char(b'1'));
    }

    #[test]
    fn right_shift_works_the_same_as_left() {
        let mut s = KeyboardState::new();
        s.feed_scancode(SC_RSHIFT);
        assert_eq!(s.feed_scancode(0x15), KeyEvent::Char(b'Y'));
    }

    #[test]
    fn caps_lock_affects_letters_only_not_symbols() {
        let mut s = KeyboardState::new();
        // Toggle caps lock on.
        assert_eq!(
            s.feed_scancode(SC_CAPS),
            KeyEvent::Modifier {
                key: ModKey::CapsLock,
                pressed: true
            }
        );
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'A')); // letter uppercased
        assert_eq!(s.feed_scancode(0x02), KeyEvent::Char(b'1')); // digit UNAFFECTED
    }

    #[test]
    fn caps_lock_release_is_inert_and_toggle_needs_a_second_press() {
        let mut s = KeyboardState::new();
        s.feed_scancode(SC_CAPS); // on
        assert_eq!(s.feed_scancode(SC_CAPS | BREAK_BIT), KeyEvent::Ignored); // release inert
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'A')); // still on
        s.feed_scancode(SC_CAPS); // off
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'a'));
    }

    #[test]
    fn shift_and_caps_lock_cancel_for_letters() {
        let mut s = KeyboardState::new();
        s.feed_scancode(SC_CAPS); // caps on
        s.feed_scancode(SC_LSHIFT); // shift down
        // Shift XOR Caps = false => lowercase.
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'a'));
    }

    #[test]
    fn extended_prefix_then_unmapped_key_is_ignored_and_state_resets() {
        let mut s = KeyboardState::new();
        // 0xE0 0x48 is the Up arrow: prefix ignored, extended byte ignored.
        assert_eq!(s.feed_scancode(0xE0), KeyEvent::Ignored);
        assert!(s.pending_extended, "prefix must arm the extended flag");
        assert_eq!(s.feed_scancode(0x48), KeyEvent::Ignored);
        assert!(!s.pending_extended, "extended flag must clear after one byte");
        // The very next NORMAL key must decode correctly — this is the bug
        // class where extended-prefix state leaks into the next keystroke.
        assert_eq!(s.feed_scancode(0x1E), KeyEvent::Char(b'a'));
    }

    #[test]
    fn extended_ctrl_release_does_not_corrupt_following_key() {
        let mut s = KeyboardState::new();
        // Right Ctrl release is 0xE0 0x9D. Both bytes ignored; then 'b'.
        feed(&mut s, &[0xE0, 0x9D]);
        assert_eq!(s.feed_scancode(0x30), KeyEvent::Char(b'b'));
    }

    #[test]
    fn unmapped_scancode_is_ignored_without_panicking() {
        let mut s = KeyboardState::new();
        // 0x59 is unmapped in our table; must not panic, must not emit.
        assert_eq!(s.feed_scancode(0x59), KeyEvent::Ignored);
    }

    #[test]
    fn ctrl_press_and_release_tracks_state() {
        let mut s = KeyboardState::new();
        assert_eq!(
            s.feed_scancode(SC_LCTRL),
            KeyEvent::Modifier {
                key: ModKey::LCtrl,
                pressed: true
            }
        );
        assert!(s.lctrl);
        assert_eq!(
            s.feed_scancode(SC_LCTRL | BREAK_BIT),
            KeyEvent::Modifier {
                key: ModKey::LCtrl,
                pressed: false
            }
        );
        assert!(!s.lctrl);
    }

    #[test]
    fn types_out_a_whole_word() {
        // "echo hi\n" as the make codes QEMU's sendkey would deliver.
        let mut s = KeyboardState::new();
        let codes = [
            (0x12, b'e'),
            (0x2E, b'c'),
            (0x23, b'h'),
            (0x18, b'o'),
            (0x39, b' '),
            (0x23, b'h'),
            (0x17, b'i'),
        ];
        for (code, expected) in codes {
            assert_eq!(s.feed_scancode(code), KeyEvent::Char(expected));
        }
        assert_eq!(s.feed_scancode(0x1C), KeyEvent::Char(b'\n'));
    }

    #[test]
    fn shifted_punctuation_covers_the_common_symbols() {
        let mut s = KeyboardState::new();
        s.feed_scancode(SC_LSHIFT);
        for (code, sym) in [
            (0x27u8, b':'),
            (0x28, b'"'),
            (0x33, b'<'),
            (0x34, b'>'),
            (0x35, b'?'),
            (0x0C, b'_'),
        ] {
            assert_eq!(s.feed_scancode(code), KeyEvent::Char(sym));
        }
    }
}
