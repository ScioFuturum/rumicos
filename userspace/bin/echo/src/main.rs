//! `/bin/echo` — write `argv[1..]` joined by single spaces, plus a trailing
//! newline, to stdout. Always exits 0.
//!
//! No flag parsing: `-n` and friends are out of scope, so `echo -n x` prints
//! a literal `-n x`.

#![no_std]
#![no_main]

use liblow::fmt::write_all;
use liblow::{STDOUT, exit};

// The kernel enters a fresh image at its ELF entry point with the initial
// SysV stack laid out by kernel_proc::exec::setup_stack — rsp points at
// argc. A normal `extern "C" fn` would emit a prologue that moves rsp
// before we could read it, so the entry point is raw asm that captures rsp
// into the first argument register and then hands off to Rust.
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "xor rbp, rbp",   // outermost frame: no caller to chain to
    "mov rdi, rsp",   // arg0 = the untouched initial rsp (points at argc)
    "and rsp, -16",   // SysV requires 16-byte stack alignment at a call
    "call rust_start",
    "ud2",            // rust_start is `-> !`; never reached
);

/// # Safety
/// Called only from the `_start` stub above, with `stack` = the process's
/// initial rsp.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(stack: *const u64) -> ! {
    // SAFETY: `stack` is the initial rsp the kernel entered us with, which
    // points at argc with a NULL-terminated argv array above it.
    let args = unsafe { liblow::args_from_stack(stack) };

    let mut first = true;
    for a in args.rest() {
        if !first {
            let _ = write_all(STDOUT, b" ");
        }
        let _ = write_all(STDOUT, a.as_bytes());
        first = false;
    }
    let _ = write_all(STDOUT, b"\n");
    exit(0)
}
