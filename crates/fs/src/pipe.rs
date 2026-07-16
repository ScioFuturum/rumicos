//! Anonymous pipes: an in-kernel ring buffer with a read end and a write
//! end, each exposed as a [`VNode`] so it flows through the existing
//! `FdTable`/`VNodeOps`/`sys_read`/`sys_write` machinery with no
//! special-casing.
//!
//! ## Frame layout (mirrors ramfs's `RamfsInode`/`RamfsChildPage` split)
//!
//! `PIPE_BUF_SIZE` (4096) is one whole frame on its own, so — exactly like
//! ramfs, which split its inode metadata and its 4 KiB child page into two
//! frames for the same reason — a pipe uses **two** frames:
//!
//!   * a **control-block frame** holding the [`Pipe`] struct: the
//!     `SpinLock<PipeRing>` (ring positions + length), the two
//!     `WaitQueue`s, the data frame's physical address, and the two
//!     end-VNode addresses;
//!   * a separate **data frame** holding the `[u8; PIPE_BUF_SIZE]` ring
//!     bytes.
//!
//! The two end VNodes' `fs_data` both point at the control frame's physical
//! address (the same convention `RamfsInode` uses).
//!
//! ## Locking
//!
//! One `SpinLock<PipeRing>` guards the ring state (positions + length). The
//! two `WaitQueue`s live *outside* that lock (each has its own internal
//! lock), so a blocking reader/writer always drops the ring lock **before**
//! calling `thread_block` — the same no-lock-across-block invariant
//! `kernel_proc::wait::sys_wait4` documents. Lock order, when both are ever
//! held: `fd_table` (kernel-proc) first, then a pipe's ring lock — pipe
//! locks are treated as same-tier-or-lower than `fd_table`.
//!
//! ## Reader/writer liveness = end-VNode refcounts
//!
//! Rather than store separate `readers`/`writers` counters, "how many read
//! (write) fds are open" is read directly from the read-end (write-end)
//! VNode's `refcount`. This makes `dup()`/`dup2()` correct **for free**:
//! `dup` already `inc_ref`s the underlying VNode and `close` `dec_ref`s it
//! (via [`pipe_read_release`]/[`pipe_write_release`]), so EOF (reader sees
//! no writers) and EPIPE (writer sees no readers) key off `refcount == 0`
//! with no extra bookkeeping and no separate acquire hook. Each end VNode
//! starts at `refcount == 1` for the single fd `pipe()` hands back.

use crate::vnode::{
    VNode, VNodeOps, vnode_create_noop, vnode_lookup_noop, vnode_readdir_noop, vnode_truncate_noop,
};
// VNodeType is used by the target-only frame constructor and by host tests,
// but not by the plain host lib build.
#[cfg(any(target_os = "none", test))]
use crate::vnode::VNodeType;
#[cfg(target_os = "none")]
use crate::vnode::alloc_ino;
// Ordering/AtomicU32/WaitQueue are only used in the target-only glue.
#[cfg(target_os = "none")]
use core::sync::atomic::{AtomicU32, Ordering};
#[cfg(target_os = "none")]
use kernel_sched::WaitQueue;
// SpinLock backs the target-only Pipe control block and appears in tests.
#[cfg(any(target_os = "none", test))]
use kernel_sync::SpinLock;

/// POSIX `PIPE_BUF`: writes of at most this many bytes are the ones a real
/// pipe guarantees atomic. Sized to exactly one frame — the pipe's data
/// ring is a single 4 KiB page.
pub const PIPE_BUF_SIZE: usize = 4096;

/// Linux `pipe(2)` syscall number.
pub const SYS_PIPE: u64 = 22;

const EFAULT: i64 = -14;
#[cfg(target_os = "none")]
const ENOMEM: i64 = -12;
/// Broken pipe: write with no readers left.
pub const EPIPE: i64 = -32;
/// Wrong-direction access on a pipe end (read the write end, or vice-versa).
const EBADF: i64 = -9;

#[cfg(target_os = "none")]
const DMAP: usize = 0xFFFF_8000_0000_0000;

// ─── pure ring buffer (host-testable, no unsafe, no target dependency) ─────

/// The ring-buffer bookkeeping for a pipe. The backing bytes live in a
/// separate frame passed in by reference, so this whole type is a plain,
/// unit-testable value with no `unsafe` and no target coupling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeRing {
    read_pos: usize,
    write_pos: usize,
    len: usize,
}

impl PipeRing {
    pub const fn new() -> Self {
        Self {
            read_pos: 0,
            write_pos: 0,
            len: 0,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len == PIPE_BUF_SIZE
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Copy up to `dst.len()` buffered bytes into `dst`, advancing the read
    /// cursor. Returns the number of bytes copied (0 iff empty). Byte-by-byte
    /// on purpose — never an aggregate array copy (see docs/miscompile-audit.md).
    pub fn read_into(&mut self, data: &[u8; PIPE_BUF_SIZE], dst: &mut [u8]) -> usize {
        let n = core::cmp::min(dst.len(), self.len);
        for (i, d) in dst.iter_mut().enumerate().take(n) {
            *d = data[(self.read_pos + i) % PIPE_BUF_SIZE];
        }
        self.read_pos = (self.read_pos + n) % PIPE_BUF_SIZE;
        self.len -= n;
        n
    }

    /// Copy as many of `src`'s bytes as fit into free ring space, advancing
    /// the write cursor. Returns the number written (0 iff full).
    pub fn write_from(&mut self, data: &mut [u8; PIPE_BUF_SIZE], src: &[u8]) -> usize {
        let space = PIPE_BUF_SIZE - self.len;
        let n = core::cmp::min(src.len(), space);
        for (i, &b) in src.iter().enumerate().take(n) {
            data[(self.write_pos + i) % PIPE_BUF_SIZE] = b;
        }
        self.write_pos = (self.write_pos + n) % PIPE_BUF_SIZE;
        self.len += n;
        n
    }
}

impl Default for PipeRing {
    fn default() -> Self {
        Self::new()
    }
}

// ─── pure blocking-decision helpers (host-testable) ────────────────────────

/// What a reader that found the ring empty should do next.
#[derive(Debug, PartialEq, Eq)]
pub enum ReaderStep {
    /// Return this value to userspace (0 = EOF: no writers remain).
    Return(i64),
    /// Block on the read wait queue and retry.
    Block,
}

/// Reader hit an empty ring: EOF (return 0) iff no writer remains, else block.
pub fn reader_step_when_empty(writers_present: bool) -> ReaderStep {
    if writers_present {
        ReaderStep::Block
    } else {
        ReaderStep::Return(0)
    }
}

/// What a writer that found the ring full should do next.
#[derive(Debug, PartialEq, Eq)]
pub enum WriterStep {
    /// Return this value to userspace (EPIPE: no readers remain).
    Return(i64),
    /// Block on the write wait queue and retry.
    Block,
}

/// Writer hit a full ring: EPIPE iff no reader remains, else block.
pub fn writer_step_when_full(readers_present: bool) -> WriterStep {
    if readers_present {
        WriterStep::Block
    } else {
        WriterStep::Return(EPIPE)
    }
}

// ─── control block (one frame) ─────────────────────────────────────────────

/// Shared pipe state, one per `pipe()` call, placement-written into its own
/// control frame and pointed at by BOTH end VNodes' `fs_data`.
#[cfg(target_os = "none")]
#[repr(C)]
pub struct Pipe {
    ring: SpinLock<PipeRing>,
    read_waiters: WaitQueue,
    write_waiters: WaitQueue,
    /// Physical address of the separate 4 KiB data frame.
    data_phys: u64,
    /// Direct-map VA of the read-end VNode (its refcount = open read fds).
    read_vn: u64,
    /// Direct-map VA of the write-end VNode (its refcount = open write fds).
    write_vn: u64,
}

// The control block must fit in one frame (the data ring is elsewhere).
#[cfg(target_os = "none")]
const _: () = assert!(
    core::mem::size_of::<Pipe>() <= 4096,
    "Pipe control block must fit in one 4 KiB frame"
);

// ─── VNodeOps ──────────────────────────────────────────────────────────────

pub static PIPE_READ_OPS: VNodeOps = VNodeOps {
    read: pipe_read,
    write: pipe_write_bad,
    lookup: vnode_lookup_noop,
    create: vnode_create_noop,
    readdir: vnode_readdir_noop,
    truncate: vnode_truncate_noop,
    release: pipe_read_release,
};

pub static PIPE_WRITE_OPS: VNodeOps = VNodeOps {
    read: pipe_read_bad,
    write: pipe_write,
    lookup: vnode_lookup_noop,
    create: vnode_create_noop,
    readdir: vnode_readdir_noop,
    truncate: vnode_truncate_noop,
    release: pipe_write_release,
};

/// Reading the write end is EBADF (defence in depth — `sys_read` already
/// rejects it on the O_WRONLY flag before dispatch).
fn pipe_read_bad(_: &VNode, _: &mut [u8], _: u64) -> i64 {
    EBADF
}

/// Writing the read end is EBADF (same belt-and-braces rationale).
fn pipe_write_bad(_: &VNode, _: &[u8], _: u64) -> i64 {
    EBADF
}

// ─── read/write glue (target-only frame access) ────────────────────────────

fn pipe_read(vn: &VNode, buf: &mut [u8], _offset: u64) -> i64 {
    #[cfg(target_os = "none")]
    {
        // SAFETY: a PIPE_READ_OPS VNode's fs_data is the control frame phys
        // written by sys_pipe; the direct map is live.
        let pipe = unsafe { &*((DMAP + vn.fs_data as usize) as *const Pipe) };
        loop {
            let (n, was_full) = {
                let mut ring = pipe.ring.lock();
                if ring.is_empty() {
                    (0usize, false)
                } else {
                    let was_full = ring.is_full();
                    // SAFETY: data_phys is this pipe's own data frame, live
                    // for the pipe's lifetime; the ring lock serialises access.
                    let data =
                        unsafe { &mut *((DMAP + pipe.data_phys as usize) as *mut [u8; PIPE_BUF_SIZE]) };
                    (ring.read_into(data, buf), was_full)
                }
            };
            if n > 0 {
                if was_full {
                    // Room freed — wake a blocked writer.
                    kernel_sched::wake_one(&pipe.write_waiters);
                }
                return n as i64;
            }
            // Empty: EOF if no writers remain, else block and retry.
            // SAFETY: write_vn is the live write-end VNode for this pipe's lifetime.
            let writers = unsafe { (*(pipe.write_vn as *const VNode)).refcount.load(Ordering::Acquire) };
            match reader_step_when_empty(writers > 0) {
                ReaderStep::Return(v) => return v,
                ReaderStep::Block => {
                    // SAFETY: the ring lock was dropped above; no lock is held
                    // across the block, per the wait4 invariant.
                    unsafe { kernel_sched::thread_block(&pipe.read_waiters) };
                }
            }
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (vn, buf);
        -38 // ENOSYS on host
    }
}

fn pipe_write(vn: &VNode, buf: &[u8], _offset: u64) -> i64 {
    #[cfg(target_os = "none")]
    {
        // SAFETY: a PIPE_WRITE_OPS VNode's fs_data is the control frame phys.
        let pipe = unsafe { &*((DMAP + vn.fs_data as usize) as *const Pipe) };
        // POSIX: writing a pipe with no readers is EPIPE (checked before
        // touching the buffer). SIGPIPE is NOT raised — see the module's
        // known limitations.
        // SAFETY: read_vn is the live read-end VNode for this pipe's lifetime.
        let readers = unsafe { (*(pipe.read_vn as *const VNode)).refcount.load(Ordering::Acquire) };
        if readers == 0 {
            return EPIPE;
        }
        loop {
            let (n, was_empty) = {
                let mut ring = pipe.ring.lock();
                if ring.is_full() {
                    (0usize, false)
                } else {
                    let was_empty = ring.is_empty();
                    // SAFETY: as in pipe_read; the ring lock serialises access.
                    let data =
                        unsafe { &mut *((DMAP + pipe.data_phys as usize) as *mut [u8; PIPE_BUF_SIZE]) };
                    // Single-shot: write what fits and return (a SHORT WRITE
                    // is legal POSIX for counts > PIPE_BUF_SIZE). Only a write
                    // that finds the ring completely full blocks; this kernel
                    // does not provide the <= PIPE_BUF_SIZE all-or-nothing
                    // atomicity guarantee (see known limitations).
                    (ring.write_from(data, buf), was_empty)
                }
            };
            if n > 0 {
                if was_empty {
                    kernel_sched::wake_one(&pipe.read_waiters);
                }
                return n as i64;
            }
            // Full: EPIPE if readers all left meanwhile, else block and retry.
            // SAFETY: read_vn live for the pipe's lifetime.
            let readers = unsafe { (*(pipe.read_vn as *const VNode)).refcount.load(Ordering::Acquire) };
            match writer_step_when_full(readers > 0) {
                WriterStep::Return(v) => return v,
                WriterStep::Block => {
                    // SAFETY: ring lock dropped above; no lock held across block.
                    unsafe { kernel_sched::thread_block(&pipe.write_waiters) };
                }
            }
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (vn, buf);
        -38
    }
}

// ─── release (close) glue: decrement liveness, wake the opposite end ───────

fn pipe_read_release(vn: *mut VNode) {
    #[cfg(target_os = "none")]
    {
        // SAFETY: vn is the live read-end VNode whose last fd is closing.
        let v = unsafe { &*vn };
        // dec_ref returns the PREVIOUS refcount; == 1 means it just hit 0.
        if v.dec_ref() == 1 {
            // Last reader gone: wake every blocked writer so it observes EPIPE.
            // SAFETY: fs_data is this pipe's control frame; still live (the
            // write end keeps the control/data frames referenced).
            let pipe = unsafe { &*((DMAP + v.fs_data as usize) as *const Pipe) };
            kernel_sched::wake_all(&pipe.write_waiters);
        }
    }
    #[cfg(not(target_os = "none"))]
    let _ = vn;
}

fn pipe_write_release(vn: *mut VNode) {
    #[cfg(target_os = "none")]
    {
        // SAFETY: vn is the live write-end VNode whose last fd is closing.
        let v = unsafe { &*vn };
        if v.dec_ref() == 1 {
            // Last writer gone: wake every blocked reader so it observes EOF.
            // SAFETY: as above.
            let pipe = unsafe { &*((DMAP + v.fs_data as usize) as *const Pipe) };
            kernel_sched::wake_all(&pipe.read_waiters);
        }
    }
    #[cfg(not(target_os = "none"))]
    let _ = vn;
}

// ─── sys_pipe ──────────────────────────────────────────────────────────────

/// `pipe(fds)` — create a pipe and install its two ends as the two lowest
/// free fds: `fds[0]` = read end (O_RDONLY), `fds[1]` = write end
/// (O_WRONLY), POSIX order.
pub fn sys_pipe(fds_ptr: u64) -> i64 {
    if !kernel_proc::is_user_ptr(fds_ptr as usize, 8) {
        return EFAULT;
    }
    #[cfg(target_os = "none")]
    {
        // Two frames: control block + data ring (the ramfs-style split).
        let ctrl_phys = kernel_memory::alloc_frame().as_u64();
        let data_phys = kernel_memory::alloc_frame().as_u64();

        let read_vn = create_pipe_vnode(ctrl_phys, &PIPE_READ_OPS);
        let write_vn = create_pipe_vnode(ctrl_phys, &PIPE_WRITE_OPS);

        // Placement-write the control block. No large array field lives here
        // (the ring bytes are in data_phys), so this is a small struct move.
        let pipe_ptr = (DMAP + ctrl_phys as usize) as *mut Pipe;
        // SAFETY: freshly allocated control frame, no other aliases yet.
        unsafe {
            pipe_ptr.write(Pipe {
                ring: SpinLock::new(PipeRing::new()),
                read_waiters: WaitQueue::new(),
                write_waiters: WaitQueue::new(),
                data_phys,
                read_vn: read_vn as u64,
                write_vn: write_vn as u64,
            });
        }

        let proc = kernel_proc::current_process();
        if proc.is_null() {
            return ENOMEM;
        }
        // SAFETY: proc is this thread's live PCB.
        let (fd_r, fd_w) = unsafe {
            let mut fdt = (*proc).fd_table.lock();
            let r = fdt.alloc(read_vn as usize, kernel_proc::O_RDONLY);
            if r < 0 {
                // Out of fds: the two pipe frames + two VNode frames leak
                // here (known limitation — pipe frames are never reclaimed).
                return r as i64;
            }
            let w = fdt.alloc(write_vn as usize, kernel_proc::O_WRONLY);
            if w < 0 {
                fdt.close(r); // roll back the read fd
                return w as i64;
            }
            (r, w)
        };

        // Write the two fd numbers back to the user array.
        // SAFETY: fds_ptr validated for 8 bytes above; STAC lifts SMAP for
        // the guarded user write, CLAC re-arms it.
        unsafe {
            core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
            core::ptr::write(fds_ptr as *mut i32, fd_r);
            core::ptr::write((fds_ptr + 4) as *mut i32, fd_w);
            core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
        }
        0
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = fds_ptr;
        -38
    }
}

/// Allocate a fresh VNode for one pipe end on its own frame, with `fs_data`
/// pointing at the shared control block and `refcount == 1` (the single fd
/// `pipe()` returns for this end).
#[cfg(target_os = "none")]
fn create_pipe_vnode(ctrl_phys: u64, ops: &'static VNodeOps) -> *mut VNode {
    let frame = kernel_memory::alloc_frame();
    let vn = (DMAP + frame.as_u64() as usize) as *mut VNode;
    // SAFETY: freshly allocated frame, no aliases.
    unsafe {
        core::ptr::write(
            vn,
            VNode {
                vtype: VNodeType::CharDevice,
                ino: alloc_ino(),
                size: 0,
                refcount: AtomicU32::new(1),
                ops,
                fs_data: ctrl_phys,
                lock: SpinLock::new(()),
            },
        );
    }
    vn
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_buf_size_is_one_page() {
        assert_eq!(PIPE_BUF_SIZE, 4096);
    }

    #[test]
    fn sys_pipe_number_matches_linux() {
        assert_eq!(SYS_PIPE, 22);
    }

    #[test]
    fn ring_write_then_read_roundtrips_bytes() {
        let mut data = [0u8; PIPE_BUF_SIZE];
        let mut ring = PipeRing::new();
        assert!(ring.is_empty());
        assert_eq!(ring.write_from(&mut data, b"ping"), 4);
        assert_eq!(ring.len(), 4);
        assert!(!ring.is_empty());
        let mut out = [0u8; 8];
        assert_eq!(ring.read_into(&data, &mut out), 4);
        assert_eq!(&out[..4], b"ping");
        assert!(ring.is_empty());
    }

    #[test]
    fn ring_wraps_around_end_of_buffer() {
        let mut data = [0u8; PIPE_BUF_SIZE];
        let mut ring = PipeRing::new();
        // Fill almost to the end, drain, then write across the wrap point.
        let filler = [0xAAu8; PIPE_BUF_SIZE - 2];
        assert_eq!(ring.write_from(&mut data, &filler), PIPE_BUF_SIZE - 2);
        let mut sink = [0u8; PIPE_BUF_SIZE - 2];
        assert_eq!(ring.read_into(&data, &mut sink), PIPE_BUF_SIZE - 2);
        // write_pos is now near the end; this write straddles the wrap.
        assert_eq!(ring.write_from(&mut data, b"WRAP"), 4);
        let mut out = [0u8; 4];
        assert_eq!(ring.read_into(&data, &mut out), 4);
        assert_eq!(&out, b"WRAP");
    }

    #[test]
    fn ring_write_saturates_at_capacity_short_write() {
        let mut data = [0u8; PIPE_BUF_SIZE];
        let mut ring = PipeRing::new();
        // A 5000-byte write into an empty ring writes only PIPE_BUF_SIZE.
        let big = [0x5Au8; 5000];
        assert_eq!(ring.write_from(&mut data, &big), PIPE_BUF_SIZE);
        assert!(ring.is_full());
        // A further write returns 0 (full).
        assert_eq!(ring.write_from(&mut data, b"x"), 0);
    }

    #[test]
    fn ring_read_on_empty_returns_zero() {
        let data = [0u8; PIPE_BUF_SIZE];
        let mut ring = PipeRing::new();
        let mut out = [0u8; 4];
        assert_eq!(ring.read_into(&data, &mut out), 0);
    }

    #[test]
    fn reader_on_empty_with_no_writers_is_eof() {
        // len == 0 and writers == 0 → return 0 (EOF), not block.
        assert_eq!(reader_step_when_empty(false), ReaderStep::Return(0));
    }

    #[test]
    fn reader_on_empty_with_writers_blocks() {
        assert_eq!(reader_step_when_empty(true), ReaderStep::Block);
    }

    #[test]
    fn writer_on_full_with_no_readers_is_epipe() {
        // full and readers == 0 → EPIPE (-32).
        assert_eq!(writer_step_when_full(false), WriterStep::Return(EPIPE));
        assert_eq!(EPIPE, -32);
    }

    #[test]
    fn writer_on_full_with_readers_blocks() {
        assert_eq!(writer_step_when_full(true), WriterStep::Block);
    }

    #[test]
    fn pipe_ends_reject_wrong_direction() {
        // The wrong-direction ops return EBADF (belt-and-braces; the flag
        // check in sys_read/sys_write catches this first in practice).
        let dummy = crate::vnode::VNode {
            vtype: VNodeType::CharDevice,
            ino: 1,
            size: 0,
            refcount: core::sync::atomic::AtomicU32::new(1),
            ops: &PIPE_READ_OPS,
            fs_data: 0,
            lock: SpinLock::new(()),
        };
        let mut rbuf = [0u8; 4];
        assert_eq!(pipe_read_bad(&dummy, &mut rbuf, 0), EBADF);
        assert_eq!(pipe_write_bad(&dummy, b"x", 0), EBADF);
    }
}
