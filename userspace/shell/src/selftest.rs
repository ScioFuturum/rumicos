//! The CI-safe self-test.
//!
//! `cargo xtask qemu-test` captures serial output and has no way to feed
//! keystrokes, so an interactive REPL blocking on stdin would just hang
//! until the timeout and fail. This module drives a fixed script through
//! **exactly the same** [`crate::parse::parse_line`] and
//! [`crate::exec::execute_pipeline`] the interactive REPL uses — no
//! parallel, untested code path.
//!
//! Each step prints `shell: ok <what>` or `shell: FAIL <what>`, so
//! `tests/expected-boot.txt` can assert on real behaviour rather than on
//! "it didn't crash".

use crate::exec::execute_pipeline;
use crate::parse::parse_line;
use liblow::fmt::write_all;
use liblow::{O_RDONLY, STDOUT, close, open, read};

/// The script. Absolute paths only — there is no PATH search.
pub const SELFTEST_SCRIPT: &[&str] = &[
    "/bin/echo hello pipeline",
    "/bin/echo hello | /bin/cat",
    "/bin/echo piped twice | /bin/cat | /bin/cat",
    "/bin/echo redirected > /tmp/shellout.txt",
    "/bin/cat < /tmp/shellout.txt",
];

/// What `/tmp/shellout.txt` must contain after step 4 — `echo` adds the
/// trailing newline.
const EXPECTED_FILE: &[u8] = b"redirected\n";

fn report(ok: bool, what: &str) {
    let _ = write_all(STDOUT, if ok { b"shell: ok " } else { b"shell: FAIL " });
    let _ = write_all(STDOUT, what.as_bytes());
    let _ = write_all(STDOUT, b"\n");
}

/// Run `line` through the real parser + executor, reporting `what`.
/// Returns true iff the pipeline exited 0.
fn run_step(line: &str, what: &str) -> bool {
    match parse_line(line) {
        Ok(p) => {
            let status = execute_pipeline(&p);
            let ok = status == 0;
            report(ok, what);
            ok
        }
        Err(_) => {
            report(false, what);
            false
        }
    }
}

/// Read `path` and compare it byte-for-byte with `expect`.
///
/// This is what turns the `>` step from "exited 0" into a real assertion:
/// it proves the redirect actually created the file AND wrote the right
/// bytes into it, independently of whatever scrolled past on the console.
fn file_matches(path: &str, expect: &[u8]) -> bool {
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = read(fd as i32, &mut buf);
    close(fd as i32);
    if n < 0 {
        return false;
    }
    &buf[..n as usize] == expect
}

/// Run the whole self-test. Returns true iff every step passed.
pub fn run() -> bool {
    let mut all_ok = true;

    all_ok &= run_step(SELFTEST_SCRIPT[0], "echo");
    all_ok &= run_step(SELFTEST_SCRIPT[1], "pipe echo|cat");
    all_ok &= run_step(SELFTEST_SCRIPT[2], "pipe echo|cat|cat");
    all_ok &= run_step(SELFTEST_SCRIPT[3], "redirect > file");

    // Content check, not just an exit status.
    let contents_ok = file_matches("/tmp/shellout.txt", EXPECTED_FILE);
    report(contents_ok, "redirected file contents match");
    all_ok &= contents_ok;

    all_ok &= run_step(SELFTEST_SCRIPT[4], "redirect < file");

    let _ = write_all(STDOUT, b"shell: self-test complete\n");
    all_ok
}
