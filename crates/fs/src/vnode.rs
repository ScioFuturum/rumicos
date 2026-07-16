//! VNode – in-memory inode abstraction.
//!
//! Each file, directory, device, or symlink is represented by one VNode.
//! All operations are dispatched through the `ops` vtable which is a
//! `&'static VNodeOps` struct of concrete function pointers (no fat pointers,
//! no heap allocation).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VNodeType {
    Regular,
    Directory,
    CharDevice,
    Symlink,
}

/// Directory entry returned by `readdir`.
#[repr(C)]
pub struct DirEntry {
    pub ino: u64,
    pub vtype: VNodeType,
    pub name_len: u8,
    pub name: [u8; 256],
}

/// Vtable of filesystem operations.
///
/// All functions receive `&VNode`.  Functions that mutate the VNode (e.g.
/// `write`, `truncate`) must acquire `VNode.lock` internally.
pub struct VNodeOps {
    pub read: fn(vn: &VNode, buf: &mut [u8], offset: u64) -> i64,
    pub write: fn(vn: &VNode, buf: &[u8], offset: u64) -> i64,
    pub lookup: fn(vn: &VNode, name: &str) -> Option<*mut VNode>,
    pub create: fn(vn: &VNode, name: &str, vtype: VNodeType) -> Option<*mut VNode>,
    pub readdir: fn(vn: &VNode, offset: u64, entries: &mut [DirEntry]) -> i64,
    pub truncate: fn(vn: &VNode, new_size: u64) -> i32,
    pub release: fn(vn: *mut VNode),
}

/// In-memory inode.  Allocated one-per-frame; accessed via the direct map.
#[repr(C, align(64))]
pub struct VNode {
    pub vtype: VNodeType,
    pub ino: u64,
    pub size: u64,
    pub refcount: AtomicU32,
    pub ops: &'static VNodeOps,
    /// Physical address of the fs-private data block (e.g. `RamfsInode`).
    /// Zero for device VNodes that carry no file data.
    pub fs_data: u64,
    pub lock: kernel_sync::SpinLock<()>,
}

// Raw pointers inside VNode; synchronised via lock.
unsafe impl Send for VNode {}
unsafe impl Sync for VNode {}

static NEXT_INO: AtomicU64 = AtomicU64::new(1);
pub fn alloc_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

impl VNode {
    pub fn inc_ref(&self) {
        self.refcount.fetch_add(1, Ordering::AcqRel);
    }
    pub fn dec_ref(&self) -> u32 {
        self.refcount.fetch_sub(1, Ordering::AcqRel)
    }
}

// ─── noop operations (used as placeholder slots) ───────────────────────────
pub fn vnode_read_noop(_: &VNode, _: &mut [u8], _: u64) -> i64 {
    -22
}
pub fn vnode_write_noop(_: &VNode, _: &[u8], _: u64) -> i64 {
    -22
}
pub fn vnode_lookup_noop(_: &VNode, _: &str) -> Option<*mut VNode> {
    None
}
pub fn vnode_create_noop(_: &VNode, _: &str, _: VNodeType) -> Option<*mut VNode> {
    None
}
pub fn vnode_readdir_noop(_: &VNode, _: u64, _: &mut [DirEntry]) -> i64 {
    0
}
pub fn vnode_truncate_noop(_: &VNode, _: u64) -> i32 {
    0
}
pub fn vnode_release_noop(_: *mut VNode) {}

pub static VNODE_NOOP_OPS: VNodeOps = VNodeOps {
    read: vnode_read_noop,
    write: vnode_write_noop,
    lookup: vnode_lookup_noop,
    create: vnode_create_noop,
    readdir: vnode_readdir_noop,
    truncate: vnode_truncate_noop,
    release: vnode_release_noop,
};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dir_entry_name_fits_256() {
        assert_eq!(core::mem::size_of::<[u8; 256]>(), 256);
        let de = DirEntry {
            ino: 1,
            vtype: VNodeType::Regular,
            name_len: 3,
            name: [0; 256],
        };
        assert_eq!(de.name.len(), 256);
    }
}
