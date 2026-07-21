//! No-alloc output helpers.
//!
//! There is no `println!` here: `core::fmt` would pull in formatting
//! machinery and panicking paths for what amounts to "write these bytes to
//! a fd". Everything below writes through [`crate::write`] directly.

use crate::{STDERR, STDOUT, write};

/// Write every byte of `buf` to `fd`, looping until it is all out or the
/// kernel returns an error. Short writes are normal on a pipe whose buffer
/// is nearly full, so a single `write()` is not enough.
///
/// Returns `true` if the whole buffer was written.
pub fn write_all(fd: i32, buf: &[u8]) -> bool {
    let mut off = 0usize;
    while off < buf.len() {
        let n = write(fd, &buf[off..]);
        if n <= 0 {
            return false; // error, or a 0-length write we cannot make progress on
        }
        off += n as usize;
    }
    true
}

/// Write a string to `fd`.
pub fn puts_fd(fd: i32, s: &str) -> bool {
    write_all(fd, s.as_bytes())
}

/// Write a string to stdout.
pub fn print(s: &str) {
    let _ = puts_fd(STDOUT, s);
}

/// Write a string plus a newline to stdout.
pub fn println(s: &str) {
    let _ = puts_fd(STDOUT, s);
    let _ = write_all(STDOUT, b"\n");
}

/// Write a string plus a newline to stderr.
pub fn eprintln(s: &str) {
    let _ = puts_fd(STDERR, s);
    let _ = write_all(STDERR, b"\n");
}

/// Render `v` as decimal into `buf`, returning the filled sub-slice.
/// Handles negatives; `i64::MIN` included (built via u64 magnitude).
pub fn i64_to_str(v: i64, buf: &mut [u8; 24]) -> &str {
    let neg = v < 0;
    // Take the magnitude in u64 so i64::MIN does not overflow on negation.
    let mut mag: u64 = if neg { (v as i128).unsigned_abs() as u64 } else { v as u64 };
    let mut i = buf.len();
    if mag == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while mag > 0 {
        i -= 1;
        buf[i] = b'0' + (mag % 10) as u8;
        mag /= 10;
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    // SAFETY-free: every byte written above is ASCII.
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}

/// Print `parts` joined with no separator, then a newline, to `fd`.
/// A tiny stand-in for `writeln!` that never allocates or panics.
pub fn write_parts(fd: i32, parts: &[&str]) {
    for p in parts {
        let _ = puts_fd(fd, p);
    }
    let _ = write_all(fd, b"\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i64_to_str_zero() {
        let mut b = [0u8; 24];
        assert_eq!(i64_to_str(0, &mut b), "0");
    }

    #[test]
    fn i64_to_str_positive() {
        let mut b = [0u8; 24];
        assert_eq!(i64_to_str(12345, &mut b), "12345");
    }

    #[test]
    fn i64_to_str_negative() {
        let mut b = [0u8; 24];
        assert_eq!(i64_to_str(-42, &mut b), "-42");
    }

    #[test]
    fn i64_to_str_min_does_not_overflow() {
        let mut b = [0u8; 24];
        assert_eq!(i64_to_str(i64::MIN, &mut b), "-9223372036854775808");
    }
}
