//! VFS-side syscall handlers.

use crate::dentry::path_lookup;
use crate::vnode::VNode;
use kernel_proc::is_user_ptr;

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_OPEN: u64 = 2;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_STAT: u64 = 4;
pub use crate::pipe::SYS_PIPE;

const ENOSYS: i64 = -38;
const EFAULT: i64 = -14;
const EBADF: i64 = -9;
const ENOENT: i64 = -2;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct StatBuf {
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_size: u64,
}

pub extern "C" fn fs_syscall_handler(
    nr: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
) -> i64 {
    match nr {
        SYS_READ => sys_read(a1 as i32, a2 as *mut u8, a3 as usize),
        SYS_WRITE => sys_write(a1 as i32, a2 as *const u8, a3 as usize),
        SYS_OPEN => sys_open(a1 as *const u8, a2 as usize, a3 as u32),
        SYS_CLOSE => sys_close(a1 as i32),
        SYS_STAT => sys_stat(a1 as *const u8, a2 as *mut StatBuf),
        SYS_PIPE => crate::pipe::sys_pipe(a1),
        _ => ENOSYS,
    }
}

fn sys_write(fd: i32, buf: *const u8, len: usize) -> i64 {
    if !is_user_ptr(buf as usize, len) {
        return EFAULT;
    }
    let (vn_ptr, offset, flags) = match fd_info(fd) {
        Some(x) => x,
        None => return EBADF,
    };
    // Enforce O_WRONLY / O_RDWR — Linux returns EBADF for wrong-direction access.
    if !fd_can_write(flags) {
        return EBADF;
    }
    let vn = unsafe { &*(vn_ptr as *const VNode) };
    let mut kbuf = [0u8; 4096];
    let n_copy = len.min(kbuf.len());
    user_copy_in(buf, &mut kbuf[..n_copy]);
    let n = (vn.ops.write)(vn, &kbuf[..n_copy], offset);
    if n > 0 {
        fd_advance(fd, n as u64);
    }
    n
}

fn sys_read(fd: i32, buf: *mut u8, len: usize) -> i64 {
    if !is_user_ptr(buf as usize, len) {
        return EFAULT;
    }
    let (vn_ptr, offset, flags) = match fd_info(fd) {
        Some(x) => x,
        None => return EBADF,
    };
    // Enforce O_RDONLY / O_RDWR — Linux returns EBADF for wrong-direction access.
    if !fd_can_read(flags) {
        return EBADF;
    }
    let vn = unsafe { &*(vn_ptr as *const VNode) };
    let mut kbuf = [0u8; 4096];
    let n_read = len.min(kbuf.len());
    let n = (vn.ops.read)(vn, &mut kbuf[..n_read], offset);
    if n > 0 {
        user_copy_out(&kbuf[..n as usize], buf);
        fd_advance(fd, n as u64);
    }
    n
}

/// Split an absolute path into `(parent_dir, basename)`.
///
/// `"/tmp/out.txt"` → `("/tmp", "out.txt")`; `"/foo"` → `("/", "foo")`.
/// Returns `None` when there is no basename to create — a bare `"/"`, an
/// empty path, a relative path, or anything with a trailing `/`.
///
/// Pure and host-testable; used only by `sys_open`'s `O_CREAT` path.
fn split_parent(path: &str) -> Option<(&str, &str)> {
    if !path.starts_with('/') || path.ends_with('/') {
        return None;
    }
    let idx = path.rfind('/')?;
    let base = &path[idx + 1..];
    if base.is_empty() {
        return None;
    }
    let parent = if idx == 0 { "/" } else { &path[..idx] };
    Some((parent, base))
}

fn sys_open(path_ptr: *const u8, path_len: usize, flags: u32) -> i64 {
    let len = path_len.min(255);
    if len == 0 || !is_user_ptr(path_ptr as usize, len) {
        return EFAULT;
    }
    let mut pb = [0u8; 256];
    user_copy_in(path_ptr, &mut pb[..len]);
    let path = match core::str::from_utf8(&pb[..len]) {
        Ok(s) => s,
        Err(_) => return EFAULT,
    };

    let vn = match path_lookup(path) {
        Some(v) => {
            // O_TRUNC on an existing file: drop its contents so re-running
            // `cmd > file` cannot leave a longer previous file's tail
            // behind. ramfs implements truncate; for device VNodes it is a
            // no-op, which is the right behaviour for /dev/serial.
            if flags & kernel_proc::O_TRUNC != 0 {
                // SAFETY: path_lookup only returns live VNode pointers.
                let vnr = unsafe { &*v };
                (vnr.ops.truncate)(vnr, 0);
            }
            v
        }
        None => {
            // O_CREAT: create the file in its parent directory. ramfs can
            // already do this — it is the same VNodeOps::create the CPIO
            // unpacker uses to populate the initrd at boot.
            if flags & kernel_proc::O_CREAT == 0 {
                return ENOENT;
            }
            let (parent, base) = match split_parent(path) {
                Some(x) => x,
                None => return ENOENT,
            };
            let dir = match path_lookup(parent) {
                Some(d) => d,
                None => return ENOENT, // parent directory must already exist
            };
            // SAFETY: path_lookup only returns live VNode pointers.
            let dirr = unsafe { &*dir };
            match (dirr.ops.create)(dirr, base, crate::vnode::VNodeType::Regular) {
                Some(v) => v,
                // The directory is full, out of frames, or is not a
                // directory at all (its create op is the no-op stub).
                None => return ENOENT,
            }
        }
    };

    let proc = kernel_proc::current_process();
    if proc.is_null() {
        return EBADF;
    }
    let fd = { unsafe { (*proc).fd_table.lock().alloc(vn as usize, flags) } };
    fd as i64
}

fn sys_close(fd: i32) -> i64 {
    let proc = kernel_proc::current_process();
    if proc.is_null() {
        return EBADF;
    }
    // Capture the VNode behind this fd BEFORE clearing the slot, so its
    // `release` hook can run once the fd is gone. For pipes that decrements
    // reader/writer liveness and wakes the opposite end (EOF/EPIPE); for
    // every other VNode `release` is a no-op, so this is behaviour-neutral
    // for ramfs/devfs. Only fires when the close actually removed a slot.
    // SAFETY: proc is this thread's live PCB.
    let vn_ptr = unsafe {
        (*proc)
            .fd_table
            .lock()
            .get(fd)
            .map(|e| e.vnode_ptr)
            .unwrap_or(0)
    };
    // SAFETY: same live PCB.
    let r = unsafe { (*proc).fd_table.lock().close(fd) };
    if r == 0 && vn_ptr != 0 {
        // SAFETY: vn_ptr came from a live FdEntry; the VNode outlives this
        // close (pipe ends keep their control/data frames referenced until
        // both ends are closed).
        let vn = unsafe { &*(vn_ptr as *const VNode) };
        (vn.ops.release)(vn_ptr as *mut VNode);
    }
    r as i64
}

fn sys_stat(path_ptr: *const u8, stat_ptr: *mut StatBuf) -> i64 {
    if !is_user_ptr(path_ptr as usize, 1) {
        return EFAULT;
    }
    if !is_user_ptr(stat_ptr as usize, core::mem::size_of::<StatBuf>()) {
        return EFAULT;
    }
    let mut pb = [0u8; 256];
    user_copy_in(path_ptr, &mut pb[..255]);
    let len = pb.iter().position(|&b| b == 0).unwrap_or(255);
    let path = match core::str::from_utf8(&pb[..len]) {
        Ok(s) => s,
        Err(_) => return EFAULT,
    };
    let vn_ptr = match path_lookup(path) {
        Some(v) => v,
        None => return ENOENT,
    };
    let vn = unsafe { &*vn_ptr };
    let mode = match vn.vtype {
        crate::vnode::VNodeType::Regular => 0o100644u32,
        crate::vnode::VNodeType::Directory => 0o040755,
        crate::vnode::VNodeType::CharDevice => 0o020666,
        crate::vnode::VNodeType::Symlink => 0o120777,
    };
    unsafe {
        user_write(
            stat_ptr,
            StatBuf {
                st_ino: vn.ino,
                st_mode: mode,
                st_size: vn.size,
            },
        );
    }
    0
}

// ─── fd helpers (explicit guard lifetime) ────────────────────────────────

/// Returns `(vnode_ptr, offset, flags)` for the given fd, or `None`.
fn fd_info(fd: i32) -> Option<(usize, u64, u32)> {
    let proc = kernel_proc::current_process();
    if proc.is_null() {
        return None;
    }
    let (vp, off, fl) = {
        let guard = unsafe { (*proc).fd_table.lock() };
        let e = guard.get(fd)?;
        if e.vnode_ptr == 0 {
            return None;
        }
        (e.vnode_ptr, e.offset, e.flags)
    };
    Some((vp, off, fl))
}

/// Returns `true` if `flags` permit reading (O_RDONLY=0 or O_RDWR=2).
#[inline]
fn fd_can_read(flags: u32) -> bool {
    let m = flags & 3;
    m == 0 || m == 2
}

/// Returns `true` if `flags` permit writing (O_WRONLY=1 or O_RDWR=2).
#[inline]
fn fd_can_write(flags: u32) -> bool {
    let m = flags & 3;
    m == 1 || m == 2
}

fn fd_advance(fd: i32, n: u64) {
    let proc = kernel_proc::current_process();
    if proc.is_null() {
        return;
    }
    let mut guard = unsafe { (*proc).fd_table.lock() };
    if let Some(e) = guard.get_mut(fd) {
        e.offset += n;
    }
}

// ─── STAC/CLAC guarded user-space copies ─────────────────────────────────

fn user_copy_in(src: *const u8, dst: &mut [u8]) {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len());
        core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len());
    }
}

fn user_copy_out(src: &[u8], dst: *mut u8) {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
        core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
    }
}

unsafe fn user_write<T: Copy>(dst: *mut T, val: T) {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("stac", options(nomem, nostack, preserves_flags));
        core::ptr::write(dst, val);
        core::arch::asm!("clac", options(nomem, nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    unsafe {
        core::ptr::write(dst, val);
    }
}

#[cfg(test)]
mod tests {
    use super::split_parent;

    #[test]
    fn split_parent_nested_path() {
        assert_eq!(split_parent("/tmp/out.txt"), Some(("/tmp", "out.txt")));
        assert_eq!(split_parent("/bin/echo"), Some(("/bin", "echo")));
        assert_eq!(split_parent("/a/b/c/d"), Some(("/a/b/c", "d")));
    }

    #[test]
    fn split_parent_top_level_file_has_root_parent() {
        assert_eq!(split_parent("/foo"), Some(("/", "foo")));
    }

    #[test]
    fn split_parent_rejects_paths_with_no_basename() {
        assert_eq!(split_parent("/"), None);
        assert_eq!(split_parent(""), None);
        // A trailing slash names a directory, not a file to create.
        assert_eq!(split_parent("/tmp/"), None);
    }

    #[test]
    fn split_parent_rejects_relative_paths() {
        // The VFS only resolves absolute paths (see path_lookup_from).
        assert_eq!(split_parent("foo.txt"), None);
        assert_eq!(split_parent("tmp/out.txt"), None);
    }
}
