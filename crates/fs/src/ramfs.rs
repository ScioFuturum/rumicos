//! RAM filesystem.

use crate::vnode::{
    DirEntry, VNode, VNodeOps, VNodeType, vnode_create_noop, vnode_lookup_noop, vnode_read_noop,
    vnode_readdir_noop, vnode_release_noop, vnode_truncate_noop, vnode_write_noop,
};
// Only the target_os = "none" frame-allocating constructors need these.
#[cfg(target_os = "none")]
use crate::vnode::alloc_ino;
#[cfg(target_os = "none")]
use core::sync::atomic::AtomicU32;

pub const RAMFS_BLOCK_SIZE: usize = 4096;
pub const RAMFS_MAX_BLOCKS: usize = 256; // 256 × 4 KiB = 1 MiB max file size
pub const RAMFS_MAX_CHILDREN: usize = 128; // entries per RamfsChildPage (one frame)
pub const RAMFS_NAME_LEN: usize = 22; // max filename chars in a child entry

const DMAP: usize = 0xFFFF_8000_0000_0000;

// ─── on-frame structures ─────────────────────────────────────────────────
//
// Layout rationale (must fit in alloc_frame() = 4 KiB):
//
//   RamfsChild (32 bytes):
//     vnode_ptr u64  (8)  +  name_len u8  (1)  +  _pad u8  (1)  +  name [u8;22] (22)
//     → 32 bytes, alignment 8.
//
//   RamfsChildPage (4096 bytes):
//     128 × RamfsChild = 128 × 32 = 4096 bytes ← exactly one frame.
//     Allocated lazily on first child insert; its phys address is stored in
//     RamfsInode.children_phys (0 = not yet allocated).
//
//   RamfsInode (2088 bytes, well under 4096):
//     overhead (40)  +  blocks [u64; 256] (2048)  =  2088.

#[repr(C)]
pub struct RamfsChild {
    pub vnode_ptr: u64, // direct-map VA of child VNode (as u64)
    pub name_len: u8,
    pub _pad: u8,
    pub name: [u8; RAMFS_NAME_LEN], // 22 bytes
                                    // sizeof = 8 + 1 + 1 + 22 = 32
}

/// One 4 KiB frame worth of directory children (128 entries × 32 bytes = 4096).
#[repr(C)]
pub struct RamfsChildPage {
    pub entries: [RamfsChild; RAMFS_MAX_CHILDREN],
}

#[repr(C)]
pub struct RamfsInode {
    pub vtype: VNodeType,
    pub _pad: u32,
    pub size: u64,
    pub block_count: usize,
    pub child_count: usize,
    /// Physical address of the `RamfsChildPage` frame (0 = no children yet).
    pub children_phys: u64,
    pub blocks: [u64; RAMFS_MAX_BLOCKS], // 256 × 8 = 2 KiB
                                         // sizeof ≈ 2088 < 4096 ✓
}

// Compile-time size guards.
const _: () = assert!(
    core::mem::size_of::<RamfsChild>() == 32,
    "RamfsChild must be exactly 32 bytes"
);
const _: () = assert!(
    core::mem::size_of::<RamfsChildPage>() == 4096,
    "RamfsChildPage must be exactly one 4 KiB frame"
);
const _: () = assert!(
    core::mem::size_of::<RamfsInode>() <= 4096,
    "RamfsInode must fit in one 4 KiB frame"
);

// ─── VNodeOps ────────────────────────────────────────────────────────────

pub static RAMFS_FILE_OPS: VNodeOps = VNodeOps {
    read: ramfs_read,
    write: ramfs_write,
    lookup: vnode_lookup_noop,
    create: vnode_create_noop,
    readdir: vnode_readdir_noop,
    truncate: ramfs_truncate,
    release: vnode_release_noop,
};

pub static RAMFS_DIR_OPS: VNodeOps = VNodeOps {
    read: vnode_read_noop,
    write: vnode_write_noop,
    lookup: ramfs_dir_lookup,
    create: ramfs_dir_create_op,
    readdir: ramfs_dir_readdir,
    truncate: vnode_truncate_noop,
    release: vnode_release_noop,
};

// ─── frame allocation helpers ─────────────────────────────────────────────

#[cfg(target_os = "none")]
fn alloc_inode(vtype: VNodeType) -> (u64, *mut RamfsInode) {
    let frame = kernel_memory::alloc_frame();
    let phys = frame.as_u64();
    let ptr = (DMAP + phys as usize) as *mut RamfsInode;
    unsafe {
        core::ptr::write_bytes(ptr as *mut u8, 0, core::mem::size_of::<RamfsInode>());
        (*ptr).vtype = vtype;
    }
    (phys, ptr)
}

#[cfg(target_os = "none")]
fn alloc_vnode(ops: &'static VNodeOps, vtype: VNodeType, inode_phys: u64) -> *mut VNode {
    let frame = kernel_memory::alloc_frame();
    let vn = (DMAP + frame.as_u64() as usize) as *mut VNode;
    unsafe {
        core::ptr::write(
            vn,
            VNode {
                vtype,
                ino: alloc_ino(),
                size: 0,
                refcount: AtomicU32::new(1),
                ops,
                fs_data: inode_phys,
                lock: kernel_sync::SpinLock::new(()),
            },
        );
    }
    vn
}

// The &mut does NOT alias the &VNode input: it points at a DIFFERENT frame
// (the RamfsInode, reached via the fs_data physical address), and callers
// serialize inode mutation behind VNode.lock. A pre-existing design this
// checkpoint keeps; a `*mut` return would just move the deref to every
// call site.
#[allow(clippy::mut_from_ref)]
#[inline]
fn inode_of(vn: &VNode) -> &mut RamfsInode {
    // SAFETY: fs_data holds the physical address of this VNode's RamfsInode frame.
    unsafe { &mut *((DMAP + vn.fs_data as usize) as *mut RamfsInode) }
}

// ─── file operations ──────────────────────────────────────────────────────

fn ramfs_read(vn: &VNode, buf: &mut [u8], offset: u64) -> i64 {
    let ino = inode_of(vn);
    if offset >= ino.size {
        return 0;
    }
    let avail = (ino.size - offset) as usize;
    let to_read = buf.len().min(avail);
    let mut done = 0usize;
    let mut fpos = offset as usize;
    while done < to_read {
        let bi = fpos / RAMFS_BLOCK_SIZE;
        let bof = fpos % RAMFS_BLOCK_SIZE;
        if bi >= ino.block_count {
            break;
        }
        let blk = unsafe {
            core::slice::from_raw_parts(
                (DMAP + ino.blocks[bi] as usize) as *const u8,
                RAMFS_BLOCK_SIZE,
            )
        };
        let chunk = (RAMFS_BLOCK_SIZE - bof).min(to_read - done);
        buf[done..done + chunk].copy_from_slice(&blk[bof..bof + chunk]);
        done += chunk;
        fpos += chunk;
    }
    done as i64
}

fn ramfs_write(vn: &VNode, buf: &[u8], offset: u64) -> i64 {
    let ino = inode_of(vn);
    let end = offset as usize + buf.len();
    let need = end.div_ceil(RAMFS_BLOCK_SIZE);
    #[cfg(not(target_os = "none"))]
    let _ = need; // only the target-only block-provisioning loop reads it
    #[cfg(target_os = "none")]
    while ino.block_count < need && ino.block_count < RAMFS_MAX_BLOCKS {
        let frame = kernel_memory::alloc_frame();
        unsafe {
            core::ptr::write_bytes(
                (DMAP + frame.as_u64() as usize) as *mut u8,
                0,
                RAMFS_BLOCK_SIZE,
            );
        }
        ino.blocks[ino.block_count] = frame.as_u64();
        ino.block_count += 1;
    }
    let mut done = 0usize;
    let mut fpos = offset as usize;
    while done < buf.len() {
        let bi = fpos / RAMFS_BLOCK_SIZE;
        let bof = fpos % RAMFS_BLOCK_SIZE;
        if bi >= ino.block_count {
            break;
        }
        let blk = unsafe {
            core::slice::from_raw_parts_mut(
                (DMAP + ino.blocks[bi] as usize) as *mut u8,
                RAMFS_BLOCK_SIZE,
            )
        };
        let chunk = (RAMFS_BLOCK_SIZE - bof).min(buf.len() - done);
        blk[bof..bof + chunk].copy_from_slice(&buf[done..done + chunk]);
        done += chunk;
        fpos += chunk;
    }
    if end as u64 > ino.size {
        ino.size = end as u64;
        // SAFETY: caller holds exclusive write access (VNode.lock).
        unsafe {
            (*(vn as *const VNode as *mut VNode)).size = ino.size;
        }
    }
    done as i64
}

fn ramfs_truncate(vn: &VNode, new_size: u64) -> i32 {
    let ino = inode_of(vn);
    ino.size = new_size;
    unsafe {
        (*(vn as *const VNode as *mut VNode)).size = new_size;
    }
    0
}

// ─── directory operations ─────────────────────────────────────────────────

fn ramfs_dir_lookup(vn: &VNode, name: &str) -> Option<*mut VNode> {
    let ino = inode_of(vn);
    if ino.children_phys == 0 || ino.child_count == 0 {
        return None;
    }
    // SAFETY: children_phys is a valid direct-map frame set by ensure_children.
    let page = unsafe { &*((DMAP + ino.children_phys as usize) as *const RamfsChildPage) };
    for i in 0..ino.child_count {
        let c = &page.entries[i];
        let nl = c.name_len as usize;
        if nl == name.len() && &c.name[..nl] == name.as_bytes() {
            return Some(c.vnode_ptr as *mut VNode);
        }
    }
    None
}

fn ramfs_dir_create_op(vn: &VNode, name: &str, vtype: VNodeType) -> Option<*mut VNode> {
    ramfs_create_child(vn as *const VNode as *mut VNode, name, vtype)
}

fn ramfs_dir_readdir(vn: &VNode, offset: u64, out: &mut [DirEntry]) -> i64 {
    let ino = inode_of(vn);
    if ino.children_phys == 0 {
        return 0;
    }
    // SAFETY: children_phys is a valid direct-map frame set by ensure_children.
    let page = unsafe { &*((DMAP + ino.children_phys as usize) as *const RamfsChildPage) };
    let start = offset as usize;
    let mut count = 0usize;
    let mut i = start;
    while i < ino.child_count && count < out.len() {
        let c = &page.entries[i];
        let child_vn = unsafe { &*(c.vnode_ptr as *const VNode) };
        let e = &mut out[count];
        e.ino = child_vn.ino;
        e.vtype = child_vn.vtype;
        let len = (c.name_len as usize).min(255);
        e.name_len = len as u8;
        e.name[..len].copy_from_slice(&c.name[..len]);
        count += 1;
        i += 1;
    }
    count as i64
}

/// Lazily allocate (or return existing) the children page for `ino`.
#[cfg(target_os = "none")]
fn ensure_children(ino: &mut RamfsInode) -> &mut RamfsChildPage {
    if ino.children_phys == 0 {
        let frame = kernel_memory::alloc_frame();
        let ptr = (DMAP + frame.as_u64() as usize) as *mut RamfsChildPage;
        // SAFETY: freshly allocated frame; zero it before first use.
        unsafe {
            core::ptr::write_bytes(ptr as *mut u8, 0, RAMFS_BLOCK_SIZE);
        }
        ino.children_phys = frame.as_u64();
    }
    // SAFETY: children_phys is now a valid direct-map pointer.
    unsafe { &mut *((DMAP + ino.children_phys as usize) as *mut RamfsChildPage) }
}

// ─── public API ──────────────────────────────────────────────────────────

#[cfg(target_os = "none")]
pub fn ramfs_create_root() -> *mut VNode {
    let (phys, _) = alloc_inode(VNodeType::Directory);
    alloc_vnode(&RAMFS_DIR_OPS, VNodeType::Directory, phys)
}
#[cfg(not(target_os = "none"))]
pub fn ramfs_create_root() -> *mut VNode {
    core::ptr::null_mut()
}

/// Create or look up a direct child of `dir_vn`.
#[cfg(target_os = "none")]
pub fn ramfs_create_child(dir_vn: *mut VNode, name: &str, vtype: VNodeType) -> Option<*mut VNode> {
    if dir_vn.is_null() {
        return None;
    }
    let ops = match vtype {
        VNodeType::Regular => &RAMFS_FILE_OPS,
        VNodeType::Directory => &RAMFS_DIR_OPS,
        _ => return None,
    };
    let (phys, _) = alloc_inode(vtype);
    let child = alloc_vnode(ops, vtype, phys);
    ramfs_dir_add_child(dir_vn, name, child);
    Some(child)
}
#[cfg(not(target_os = "none"))]
pub fn ramfs_create_child(_: *mut VNode, _: &str, _: VNodeType) -> Option<*mut VNode> {
    None
}

/// Insert `child` into the directory `dir_vn`'s child list.
/// Silently drops the entry if the directory is already full (RAMFS_MAX_CHILDREN).
// Deref is guarded by the null check below; all non-null VNode pointers in
// this kernel are live direct-map frames. Keeping the signature non-unsafe
// preserves the existing public API.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn ramfs_dir_add_child(dir_vn: *mut VNode, name: &str, child: *mut VNode) {
    if dir_vn.is_null() || child.is_null() {
        return;
    }
    // SAFETY: dir_vn is a live kernel-mode VNode.
    let ino = inode_of(unsafe { &*dir_vn });
    if ino.child_count >= RAMFS_MAX_CHILDREN {
        // Directory is full — drop silently (log would go here in a fuller kernel).
        return;
    }
    #[cfg(not(target_os = "none"))]
    let _ = name; // only the target-only insertion block reads it
    #[cfg(target_os = "none")]
    {
        let i = ino.child_count;
        let page = ensure_children(ino);
        let b = name.as_bytes();
        let len = b.len().min(RAMFS_NAME_LEN);
        page.entries[i].vnode_ptr = child as u64;
        page.entries[i].name_len = len as u8;
        page.entries[i].name[..len].copy_from_slice(&b[..len]);
        ino.child_count += 1;
    }
}

/// Write `data` into a ramfs file at offset 0 (used during initrd unpack).
// Same null-check-guarded deref rationale as ramfs_dir_add_child above.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn ramfs_write_file(vn: *mut VNode, data: &[u8]) {
    if vn.is_null() {
        return;
    }
    ramfs_write(unsafe { &*vn }, data, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramfs_inode_fits_in_frame() {
        assert!(
            core::mem::size_of::<RamfsInode>() <= 4096,
            "RamfsInode ({}B) must fit in one 4 KiB frame",
            core::mem::size_of::<RamfsInode>()
        );
    }

    #[test]
    fn ramfs_max_file_size_is_1mib() {
        assert_eq!(
            RAMFS_MAX_BLOCKS * RAMFS_BLOCK_SIZE,
            1024 * 1024,
            "RAMFS_MAX_BLOCKS × RAMFS_BLOCK_SIZE must equal exactly 1 MiB"
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // documents the invariant as a named test
    fn ramfs_max_children_at_least_128() {
        assert!(
            RAMFS_MAX_CHILDREN >= 128,
            "RAMFS_MAX_CHILDREN ({}) must be >= 128",
            RAMFS_MAX_CHILDREN
        );
    }
}
