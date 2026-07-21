//! `/bin/shell` — the Rumicos shell binary.
//!
//! Runs the self-test first, then falls into the interactive REPL. One
//! binary serves both modes on purpose:
//!
//! * `cargo xtask qemu-test` matches the self-test's `shell: ok ...` lines
//!   and then kills the VM — it polls the serial log until every expected
//!   line has matched and does NOT wait for QEMU to exit (the kernel never
//!   exits anyway; it idles). So the REPL running forever afterwards is
//!   invisible to CI.
//! * `cargo xtask qemu` by hand shows the self-test scrolling past and then
//!   leaves a live `$ ` prompt on the serial console.

#![no_std]
#![no_main]

use liblow::fmt::write_all;
use liblow::{STDERR, STDOUT, exit};
use shell::exec::execute_pipeline;
use shell::input::{LINE_BUF_SIZE, read_line};
use shell::parse::{ParseError, parse_line};
use shell::selftest;

core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "xor rbp, rbp",
    "mov rdi, rsp",
    "and rsp, -16",
    "call rust_start",
    "ud2",
);

/// # Safety
/// Called only from the `_start` stub, with `stack` = the initial rsp.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(_stack: *const u64) -> ! {
    // Phase 1: the deterministic, CI-checkable self-test.
    selftest::run();

    // Phase 2: interactive REPL. Never returns.
    repl()
}

fn repl() -> ! {
    let _ = write_all(STDOUT, b"shell: interactive mode; type a command\n");
    let mut buf = [0u8; LINE_BUF_SIZE];

    loop {
        let _ = write_all(STDOUT, b"$ ");
        let len = read_line(&mut buf);

        // The serial device cannot report EOF (its read() returns 0 for
        // "no data yet", not "closed"), so a 0-length result here is just
        // a blank line — reprompt. If stdin ever becomes a real pipe/file
        // that can signal EOF, parse_line returns Empty and we still
        // reprompt, which is the same visible behaviour.
        let line = match core::str::from_utf8(&buf[..len]) {
            Ok(s) => s,
            Err(_) => {
                let _ = write_all(STDERR, b"shell: invalid utf-8\n");
                continue;
            }
        };

        match parse_line(line) {
            Ok(p) => {
                execute_pipeline(&p);
            }
            // A blank line is not an error worth printing.
            Err(ParseError::Empty) => {}
            Err(e) => {
                let _ = write_all(STDERR, b"shell: ");
                let _ = write_all(STDERR, e.message().as_bytes());
                let _ = write_all(STDERR, b"\n");
            }
        }
    }
}

/// Unused, but keeps `exit` referenced for the linker in every config.
#[allow(dead_code)]
fn quit() -> ! {
    exit(0)
}
