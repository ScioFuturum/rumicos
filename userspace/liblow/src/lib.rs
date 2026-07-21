//! `liblow` — the entire "C library" of Rumicos userspace: raw syscall
//! wrappers and a handful of no-alloc helpers, shared by `/bin/shell`,
//! `/bin/echo` and `/bin/cat`.
//!
//! There is no libc, no heap and no runtime. Every buffer a caller needs is
//! a fixed-size array it owns; nothing here allocates.
//!
//! ## Syscall ABI (verified against the kernel, not assumed)
//!
//! Numbers are the Linux-compatible ones this kernel already established
//! (`kernel_proc::syscall`, `kernel_fs::syscall`). Two of them do NOT take
//! the shape you would guess from Linux, so read before using:
//!
//! * `open(path_ptr, path_len, flags)` takes a pointer **and a length** —
//!   NOT a NUL-terminated string (see `kernel_fs::syscall::sys_open`).
//! * `execve(path_ptr, argv_ptr, envp_ptr)` takes a **NUL-terminated** path
//!   and a **NULL-terminated array of pointers to NUL-terminated strings**
//!   (see `kernel_proc::syscall::sys_execve`).
//!
//! The SYSCALL instruction clobbers RCX (return RIP) and R11 (return
//! RFLAGS); every wrapper marks them clobbered so the compiler never keeps
//! a live value in them across the call.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod fmt;

/// The one panic handler every Rumicos userspace binary shares.
///
/// Gated on the bare-metal target so that host `cargo test` builds (which
/// link `std`, and therefore already have a handler) do not collide with it.
/// Deliberately does not format the `PanicInfo`: `core::fmt` would drag in
/// formatting and unwinding machinery for a message nobody can act on in a
/// process with no runtime. Exits 101, matching Rust's own panic exit code.
#[cfg(all(target_os = "none", not(test)))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let _ = write(STDERR, b"userspace: panic\n");
    exit(101)
}

// ─── syscall numbers (already established in this kernel) ─────────────────

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_OPEN: u64 = 2;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_PIPE: u64 = 22;
pub const SYS_DUP: u64 = 32;
pub const SYS_DUP2: u64 = 33;
pub const SYS_GETPID: u64 = 39;
pub const SYS_FORK: u64 = 57;
pub const SYS_EXECVE: u64 = 59;
pub const SYS_EXIT: u64 = 60;
pub const SYS_WAIT4: u64 = 61;

// ─── open(2) flags (mirror kernel_proc::fd) ───────────────────────────────

pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_CREAT: u32 = 64;
pub const O_TRUNC: u32 = 512;

// ─── standard fds ─────────────────────────────────────────────────────────

pub const STDIN: i32 = 0;
pub const STDOUT: i32 = 1;
pub const STDERR: i32 = 2;

// ─── raw syscall ──────────────────────────────────────────────────────────

/// Issue a syscall with six arguments.
///
/// Argument registers follow the kernel's SYSCALL trampoline convention,
/// confirmed in the CLONE_VM checkpoint: rdi, rsi, rdx, r10, r8, r9.
///
/// ## ABI deviation from Linux: argument registers are CLOBBERED
///
/// Linux's syscall entry saves and restores every GPR, so userspace sees
/// rdi/rsi/rdx/r10/r8/r9 unchanged after `syscall`. Rumicos' trampoline
/// does NOT restore them — on sysret they hold whatever the kernel's Rust
/// code left behind. Every argument register is therefore declared
/// `inlateout(...) => _` (input consumed, output garbage), so the
/// compiler never keeps a live value in one across the instruction.
///
/// This is not theoretical: with plain `in(...)` declarations, rustc kept
/// the shell's `wait4(-1, ..)` pid argument "alive" in rdi across the
/// first wait4 — whose sysret left the just-reaped pid there — and the
/// second wait4 then waited on that already-reaped pid forever. Found
/// live during the shell checkpoint (docs/shell-checkpoint.md).
///
/// # Safety
/// `nr` and the arguments must match a real kernel syscall's contract —
/// in particular any pointer argument must be valid user memory of the
/// length the kernel will read/write.
#[inline(always)]
pub unsafe fn syscall6(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> i64 {
    let ret: i64;
    // SAFETY: the caller guarantees nr/args form a valid syscall. RCX and
    // R11 are clobbered by the SYSCALL instruction itself; the argument
    // registers are clobbered by the kernel (see the ABI note above); all
    // are declared as such, so the compiler keeps nothing live in them.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr as i64 => ret,
            inlateout("rdi") a1 => _,
            inlateout("rsi") a2 => _,
            inlateout("rdx") a3 => _,
            inlateout("r10") a4 => _,
            inlateout("r8") a5 => _,
            inlateout("r9") a6 => _,
            out("rcx") _,
            out("r11") _,
            options(nostack)
        );
    }
    ret
}

macro_rules! sc {
    ($nr:expr) => { unsafe { syscall6($nr, 0, 0, 0, 0, 0, 0) } };
    ($nr:expr, $a1:expr) => { unsafe { syscall6($nr, $a1, 0, 0, 0, 0, 0) } };
    ($nr:expr, $a1:expr, $a2:expr) => { unsafe { syscall6($nr, $a1, $a2, 0, 0, 0, 0) } };
    ($nr:expr, $a1:expr, $a2:expr, $a3:expr) => { unsafe { syscall6($nr, $a1, $a2, $a3, 0, 0, 0) } };
    ($nr:expr, $a1:expr, $a2:expr, $a3:expr, $a4:expr) => {
        unsafe { syscall6($nr, $a1, $a2, $a3, $a4, 0, 0) }
    };
}

// ─── thin wrappers ────────────────────────────────────────────────────────

/// `read(fd, buf)` — bytes read, 0 on EOF, or a negative errno.
pub fn read(fd: i32, buf: &mut [u8]) -> i64 {
    sc!(SYS_READ, fd as i64 as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
}

/// `write(fd, buf)` — bytes written, or a negative errno.
pub fn write(fd: i32, buf: &[u8]) -> i64 {
    sc!(SYS_WRITE, fd as i64 as u64, buf.as_ptr() as u64, buf.len() as u64)
}

/// `open(path, flags)` — a new fd, or a negative errno.
///
/// The kernel's `sys_open` takes `(ptr, len, flags)`, so the path is passed
/// as a plain byte slice with no NUL terminator.
pub fn open(path: &str, flags: u32) -> i64 {
    sc!(
        SYS_OPEN,
        path.as_ptr() as u64,
        path.len() as u64,
        flags as u64
    )
}

/// `close(fd)` — 0, or a negative errno.
pub fn close(fd: i32) -> i64 {
    sc!(SYS_CLOSE, fd as i64 as u64)
}

/// `fork()` — 0 in the child, the child's pid in the parent, negative on
/// error (matches this kernel's `sys_fork`, which is Linux-compatible).
pub fn fork() -> i64 {
    sc!(SYS_FORK)
}

/// `dup2(old_fd, new_fd)` — `new_fd`, or a negative errno.
pub fn dup2(old_fd: i32, new_fd: i32) -> i64 {
    sc!(SYS_DUP2, old_fd as i64 as u64, new_fd as i64 as u64)
}

/// `dup(old_fd)` — the new fd, or a negative errno.
pub fn dup(old_fd: i32) -> i64 {
    sc!(SYS_DUP, old_fd as i64 as u64)
}

/// `getpid()`.
pub fn getpid() -> i64 {
    sc!(SYS_GETPID)
}

/// `pipe()` — `(read_end, write_end)` or a negative errno.
pub fn pipe() -> Result<(i32, i32), i64> {
    let mut fds = [0i32; 2];
    let r = sc!(SYS_PIPE, fds.as_mut_ptr() as u64);
    if r < 0 { Err(r) } else { Ok((fds[0], fds[1])) }
}

/// `wait4(pid, &mut status)` — the reaped pid, or a negative errno.
/// `pid == -1` reaps whichever child exits next. `options`/`rusage` are 0.
pub fn wait4(pid: i64, status: &mut i32) -> i64 {
    sc!(SYS_WAIT4, pid as u64, status as *mut i32 as u64, 0, 0)
}

/// `exit(code)` — never returns.
pub fn exit(code: i32) -> ! {
    sc!(SYS_EXIT, code as i64 as u64);
    // The kernel never returns from SYS_EXIT; if it somehow did, there is
    // no sane way to continue with no runtime, so spin rather than fall off
    // the end of a `-> !` function.
    loop {
        core::hint::spin_loop();
    }
}

/// Decode a `wait4` status word into an exit code.
///
/// Uses the SAME bit layout the kernel encodes in
/// `kernel_proc::wait::encode_exit_status` — `((code & 0xFF) << 8)`, low 7
/// bits zero meaning "exited normally". Userspace cannot depend on
/// kernel-proc (different address space, no shared libc), so this is an
/// independent implementation of the identical layout, not a shared helper.
pub fn wifexited(status: i32) -> bool {
    status & 0x7f == 0
}

/// `WEXITSTATUS`: the low 8 bits of the code the child passed to `exit()`.
pub fn wexitstatus(status: i32) -> i32 {
    (status >> 8) & 0xff
}

// ─── execve ───────────────────────────────────────────────────────────────

/// Maximum argv entries `exec` accepts (matches the kernel's `MAX_ARGV`).
pub const MAX_ARGV: usize = 16;
/// Maximum bytes for one argv string including its NUL.
pub const MAX_ARG_LEN: usize = 128;

/// `execve(path, argv)` with an empty envp.
///
/// On success this never returns (the kernel replaces the image). On
/// failure it returns a negative errno, per the kernel's execve
/// phase-1-error contract.
///
/// Builds the exact layout `kernel_proc::syscall::sys_execve` reads: a
/// NUL-terminated path, and a NULL-terminated array of pointers to
/// NUL-terminated strings. Everything lives in caller-supplied fixed
/// buffers on this function's own stack — there is no heap.
pub fn execve(path: &str, argv: &[&str]) -> i64 {
    // NUL-terminated copies of the path and of every argv string.
    let mut path_buf = [0u8; 256];
    if path.len() >= path_buf.len() {
        return -36; // ENAMETOOLONG
    }
    path_buf[..path.len()].copy_from_slice(path.as_bytes());
    // path_buf[path.len()] stays 0 → NUL terminator.

    if argv.len() > MAX_ARGV {
        return -7; // E2BIG
    }
    let mut arg_store = [[0u8; MAX_ARG_LEN]; MAX_ARGV];
    // NULL-terminated pointer array: MAX_ARGV entries + the NULL sentinel.
    let mut arg_ptrs = [0u64; MAX_ARGV + 1];
    for (i, a) in argv.iter().enumerate() {
        if a.len() >= MAX_ARG_LEN {
            return -7; // E2BIG
        }
        arg_store[i][..a.len()].copy_from_slice(a.as_bytes());
        // arg_store[i][a.len()] stays 0 → NUL terminator.
        arg_ptrs[i] = arg_store[i].as_ptr() as u64;
    }
    // arg_ptrs[argv.len()] stays 0 → NULL sentinel ending the array.

    // envp: a bare NULL array (this shell has no environment).
    let envp_null = [0u64; 1];

    sc!(
        SYS_EXECVE,
        path_buf.as_ptr() as u64,
        arg_ptrs.as_ptr() as u64,
        envp_null.as_ptr() as u64
    )
}

// ─── argc/argv from the initial stack ─────────────────────────────────────

/// Maximum argv entries [`args`] will hand back.
pub const MAX_ARGC: usize = 16;

/// A borrowed view of this process's `argv`, decoded from the initial
/// stack the kernel built (`kernel_proc::exec::setup_stack`):
///
/// ```text
///   [rsp]      argc
///   [rsp+8]    argv[0] ... argv[argc-1], then NULL
/// ```
pub struct Args {
    argv: [&'static str; MAX_ARGC],
    argc: usize,
}

impl Args {
    /// All arguments, `argv[0]` first.
    pub fn all(&self) -> &[&'static str] {
        &self.argv[..self.argc]
    }
    /// Everything after `argv[0]` — the actual operands.
    pub fn rest(&self) -> &[&'static str] {
        if self.argc == 0 { &[] } else { &self.argv[1..self.argc] }
    }
    pub fn len(&self) -> usize {
        self.argc
    }
    pub fn is_empty(&self) -> bool {
        self.argc == 0
    }
}

/// Decode `argc`/`argv` from the initial stack pointer handed to `_start`.
///
/// # Safety
/// `stack` must be the exact `rsp` value `_start` was entered with, i.e. it
/// must point at `argc` with a NULL-terminated `argv` array right above it,
/// and every `argv[i]` must be a NUL-terminated string that outlives the
/// returned `Args` (they live in the process's initial stack pages, which
/// are never freed, hence the `'static`).
pub unsafe fn args_from_stack(stack: *const u64) -> Args {
    let mut out = Args {
        argv: [""; MAX_ARGC],
        argc: 0,
    };
    if stack.is_null() {
        return out;
    }
    // SAFETY: caller guarantees `stack` points at argc.
    let argc = unsafe { *stack } as usize;
    let n = if argc > MAX_ARGC { MAX_ARGC } else { argc };
    for i in 0..n {
        // SAFETY: argv[i] sits at stack + 1 + i, within the array the
        // caller guarantees; a NULL there just yields an empty string.
        let p = unsafe { *stack.add(1 + i) } as *const u8;
        if p.is_null() {
            break;
        }
        // SAFETY: caller guarantees each argv[i] is NUL-terminated.
        out.argv[out.argc] = unsafe { cstr_to_str(p) };
        out.argc += 1;
    }
    out
}

/// Borrow a NUL-terminated C string as a `&str`, stopping at the first NUL
/// or after `MAX_ARG_LEN` bytes. Invalid UTF-8 yields an empty string.
///
/// # Safety
/// `p` must point at a readable, NUL-terminated buffer.
unsafe fn cstr_to_str(p: *const u8) -> &'static str {
    let mut len = 0usize;
    // SAFETY: caller guarantees a NUL terminator; the bound stops a runaway
    // scan if that guarantee is ever broken.
    while len < MAX_ARG_LEN && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` bytes starting at p are readable per the above scan.
    let bytes = unsafe { core::slice::from_raw_parts(p, len) };
    core::str::from_utf8(bytes).unwrap_or_default()
}
