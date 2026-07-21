//! `/bin/cat` — copy a file (or stdin) to stdout.
//!
//! With no arguments: read stdin until EOF. With one argument: open that
//! path read-only and copy it. Extra arguments are ignored (concatenating
//! several files is out of scope). Exits 0, or 1 if the open failed.

#![no_std]
#![no_main]

use liblow::fmt::write_all;
use liblow::{O_RDONLY, STDERR, STDIN, STDOUT, close, exit, open, read};

/// One I/O chunk. Matches the kernel's own 4 KiB syscall bounce buffer
/// (`kernel_fs::syscall` copies at most 4096 bytes per read/write), so a
/// bigger buffer here would just be short-read anyway.
const BUF_SIZE: usize = 4096;

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

/// Copy everything from `fd` to stdout until EOF.
///
/// Returns false if a write failed. Note the EOF condition: `read()`
/// returning exactly 0. On a pipe that means every writer closed; on a
/// regular ramfs file it means the offset reached the end.
fn cat_fd(fd: i32) -> bool {
    let mut buf = [0u8; BUF_SIZE];
    loop {
        let n = read(fd, &mut buf);
        if n == 0 {
            return true; // EOF
        }
        if n < 0 {
            return false; // read error
        }
        if !write_all(STDOUT, &buf[..n as usize]) {
            return false;
        }
    }
}

/// # Safety
/// Called only from the `_start` stub, with `stack` = the initial rsp.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(stack: *const u64) -> ! {
    // SAFETY: `stack` is the initial rsp the kernel entered us with.
    let args = unsafe { liblow::args_from_stack(stack) };
    let rest = args.rest();

    if rest.is_empty() {
        // No operand: act as a filter on stdin.
        let ok = cat_fd(STDIN);
        exit(if ok { 0 } else { 1 })
    }

    let path = rest[0];
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        let _ = write_all(STDERR, b"cat: cannot open ");
        let _ = write_all(STDERR, path.as_bytes());
        let _ = write_all(STDERR, b"\n");
        exit(1)
    }
    let ok = cat_fd(fd as i32);
    close(fd as i32);
    exit(if ok { 0 } else { 1 })
}
