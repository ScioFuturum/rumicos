#![no_std]
#![cfg(target_arch = "x86_64")]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod cpio;
pub mod dentry;
pub mod devfs;
pub mod fd;
pub mod mount;
pub mod pagecache;
pub mod pipe;
pub mod ramfs;
pub mod syscall;
pub mod vnode;

pub use dentry::path_lookup;
pub use vnode::{VNode, VNodeType};

/// Embedded initrd CPIO archive built from `initrd/`.
/// Path is relative to this source file: crates/fs/src/ → ../../../ = workspace root.
#[cfg(target_os = "none")]
static INITRD: &[u8] = include_bytes!("../../../initrd/initrd.cpio");

// Only the target_os = "none" init/shim paths translate physical frames.
#[cfg(target_os = "none")]
const DMAP: usize = 0xFFFF_8000_0000_0000;

/// Initialise the VFS layer.
///
/// Call order in kernel::main:
///   kernel_proc::init() → kernel_fs::init_fs() → Process::create()
///
/// After this function returns:
///   - ramfs is mounted at "/"
///   - devfs is mounted under /dev  (/dev/null, /dev/zero, /dev/serial)
///   - initrd has been unpacked into ramfs
///   - fd 0/1/2 of new processes will point to /dev/serial
///   - VFS syscall handler (SYS_READ/WRITE/OPEN/CLOSE/STAT) is active
#[cfg(target_os = "none")]
pub fn init_fs() {
    // 1. Create ramfs root VNode.
    let ramfs_root = ramfs::ramfs_create_root();

    // 2. Allocate a Dentry for the root mount point.
    let frame = kernel_memory::alloc_frame();
    let root_dentry = (DMAP + frame.as_u64() as usize) as *mut dentry::Dentry;
    unsafe {
        // SAFETY: freshly allocated frame, no aliases.
        core::ptr::write_bytes(
            root_dentry as *mut u8,
            0,
            core::mem::size_of::<dentry::Dentry>(),
        );
        (*root_dentry).vnode = ramfs_root;
        (*root_dentry).name[0] = b'/';
        (*root_dentry).name_len = 1;
    }
    dentry::set_root(root_dentry);

    // 3. Register in the mount table.
    unsafe {
        mount::mount("/", ramfs_root, "ramfs");
    }

    // 4. Create /dev directory in ramfs.
    let dev_dir = ramfs::ramfs_create_child(ramfs_root, "dev", VNodeType::Directory)
        .expect("/dev creation failed");

    // 5. Populate /dev and register serial VNode with kernel-proc.
    devfs::init_devfs(dev_dir);

    // 6. Unpack the embedded CPIO initrd.
    unsafe {
        cpio::unpack_cpio(INITRD, ramfs_root);
    }

    // 7. Register the VFS syscall dispatch table.
    kernel_proc::register_extra(syscall::fs_syscall_handler);

    // 8. Register the loader execve() uses to resolve a path through the
    // real VFS. kernel-proc cannot call back into kernel-fs directly (this
    // crate already depends on kernel-proc, and Cargo doesn't allow the
    // cycle the reverse dependency would create) — see
    // `kernel_proc::register_exec_loader`'s docs for the full rationale.
    kernel_proc::register_exec_loader(exec_load_file);

    // 9. Register the page cache's get_or_fill/mark_dirty/writeback_vnode
    // and VNode::inc_ref/dec_ref for kernel-proc's mmap/munmap and
    // page-fault handler to call through — same cross-crate-dependency
    // rationale as step 8's exec loader. Page-cache filling itself stays
    // purely on-demand (no entries exist until a real page fault or
    // mmap() call asks for one), so this step only ever registers
    // function pointers, never allocates or reads anything.
    kernel_proc::register_page_cache_hooks(
        pagecache::get_or_fill,
        pagecache::mark_dirty,
        pagecache::writeback_vnode,
    );
    kernel_proc::register_vnode_refcount_hooks(vnode_inc_ref_shim, vnode_dec_ref_shim);
}

#[cfg(not(target_os = "none"))]
pub fn init_fs() {}

/// # Safety: `vnode_ptr` must be a valid direct-map VA of a live VNode.
#[cfg(target_os = "none")]
unsafe fn vnode_inc_ref_shim(vnode_ptr: usize) {
    // SAFETY: forwards this function's own precondition.
    let vn = unsafe { &*(vnode_ptr as *const VNode) };
    vn.inc_ref();
}

/// # Safety: `vnode_ptr` must be a valid direct-map VA of a live VNode.
///
/// Deliberately does NOT call `(vnode.ops.release)(vnode)` even if this
/// decrement brings the refcount to 0. Per this checkpoint's own scope
/// note: the existing fd-close path (`FdTable`'s slot-clearing in
/// kernel-proc) has the identical pre-existing gap — it never calls
/// `VNode::dec_ref` at all today, let alone `release` at zero — and
/// fixing that is explicitly out of scope for this checkpoint ("note it
/// as a pre-existing gap, don't silently fix unrelated code"). This shim
/// mirrors that same boundary for mmap/munmap's own refcounting rather
/// than reaching further than what was asked.
#[cfg(target_os = "none")]
unsafe fn vnode_dec_ref_shim(vnode_ptr: usize) {
    // SAFETY: forwards this function's own precondition.
    let vn = unsafe { &*(vnode_ptr as *const VNode) };
    vn.dec_ref();
}

/// Loader registered with kernel-proc for `SYS_EXECVE`. Resolves `path`
/// through the real VFS and reads up to `buf.len()` bytes starting at
/// offset 0, exactly like [`read_file_to_frame`] but into a caller-supplied
/// buffer of arbitrary size rather than a single freshly allocated frame.
#[cfg(target_os = "none")]
fn exec_load_file(path: &str, buf: &mut [u8]) -> i64 {
    let Some(vn_ptr) = path_lookup(path) else {
        return -2;
    }; // ENOENT
    // SAFETY: path_lookup only ever returns a live VNode pointer.
    let vn = unsafe { &*vn_ptr };
    (vn.ops.read)(vn, buf, 0)
}

/// Kernel-internal: read an entire ramfs file into a freshly allocated
/// 4 KiB frame.  Returns (direct-map pointer, byte count) on success.
///
/// # Safety
/// Caller must ensure the frame remains valid while in use.
#[cfg(target_os = "none")]
pub unsafe fn read_file_to_frame(path: &str) -> Option<(*mut u8, usize)> {
    let vn_ptr = path_lookup(path)?;
    let vn = unsafe { &*vn_ptr };
    let frame = kernel_memory::alloc_frame();
    let buf = (DMAP + frame.as_u64() as usize) as *mut u8;
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, 4096) };
    let n = (vn.ops.read)(vn, slice, 0);
    if n > 0 { Some((buf, n as usize)) } else { None }
}
