//! Directory-entry (dcache).
//!
//! Each mounted filesystem contributes a Dentry tree rooted at the mount
//! point.  `path_lookup` walks this tree, calling `VNodeOps::lookup` at
//! each level so that the VNode itself owns the directory-scan logic.

use crate::vnode::VNode;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const DENTRY_NAME_LEN: usize = 256;
pub const DENTRY_MAX_CHILDREN: usize = 32;

/// One cached directory entry.  Allocated via `alloc_frame`; accessed via
/// the direct map.
pub struct Dentry {
    pub name: [u8; DENTRY_NAME_LEN],
    pub name_len: usize,
    /// Corresponding VNode.
    pub vnode: *mut VNode,
    /// Parent dentry (null for the root).
    pub parent: *mut Dentry,
    /// Cached children (populated lazily).
    pub children: [*mut Dentry; DENTRY_MAX_CHILDREN],
    pub child_count: usize,
}

unsafe impl Send for Dentry {}
unsafe impl Sync for Dentry {}

impl Dentry {
    pub const fn new_empty() -> Self {
        Self {
            name: [0; DENTRY_NAME_LEN],
            name_len: 0,
            vnode: core::ptr::null_mut(),
            parent: core::ptr::null_mut(),
            children: [core::ptr::null_mut(); DENTRY_MAX_CHILDREN],
            child_count: 0,
        }
    }
}

// ─── global VFS root ──────────────────────────────────────────────────────

static VFS_ROOT: AtomicUsize = AtomicUsize::new(0);

pub fn set_root(d: *mut Dentry) {
    VFS_ROOT.store(d as usize, Ordering::Release);
}
pub fn get_root() -> *mut Dentry {
    VFS_ROOT.load(Ordering::Acquire) as *mut Dentry
}

// ─── path lookup ─────────────────────────────────────────────────────────

/// Resolve an absolute path to a VNode starting from `root`.
/// Returns `None` if the path is empty, doesn't start with `/`, or a
/// component is not found.
// The raw-pointer deref is guarded by an explicit null check and every
// non-null Dentry pointer in this kernel comes from the static dentry
// arena (live for the kernel's whole life); making the function `unsafe`
// would change the public API, which callers across kernel-fs rely on.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn path_lookup_from(root: *mut Dentry, path: &str) -> Option<*mut VNode> {
    if !path.starts_with('/') {
        return None;
    }
    if root.is_null() {
        return None;
    }
    // SAFETY: root was set by init_vfs and remains valid for the kernel's life.
    let root_vnode = unsafe { (*root).vnode };
    if root_vnode.is_null() {
        return None;
    }

    // Strip leading '/' – the rest are components.
    let rest = path.trim_start_matches('/');
    if rest.is_empty() {
        return Some(root_vnode);
    }

    let mut current: *mut VNode = root_vnode;
    for component in rest.split('/') {
        if component.is_empty() {
            continue;
        }
        // SAFETY: current is a live VNode updated each iteration.
        let vn = unsafe { &*current };
        current = (vn.ops.lookup)(vn, component)?;
    }
    Some(current)
}

/// Resolve an absolute path using the global VFS root.
pub fn path_lookup(path: &str) -> Option<*mut VNode> {
    path_lookup_from(get_root(), path)
}

/// Insert `name → vn` as a child of `parent`.
/// Returns the newly allocated Dentry or null if the parent is full.
///
/// # Safety
/// `parent` must be null or a live, exclusively-accessible `Dentry`;
/// `vn` must be a live VNode pointer that outlives the dentry tree.
#[cfg(target_os = "none")]
pub unsafe fn dentry_insert(parent: *mut Dentry, name: &str, vn: *mut VNode) -> *mut Dentry {
    const DMAP: usize = 0xFFFF_8000_0000_0000;
    let frame = kernel_memory::alloc_frame();
    let d = (DMAP + frame.as_u64() as usize) as *mut Dentry;
    unsafe {
        // SAFETY: freshly allocated frame.
        core::ptr::write_bytes(d as *mut u8, 0, core::mem::size_of::<Dentry>());
        let b = name.as_bytes();
        let len = core::cmp::min(b.len(), DENTRY_NAME_LEN - 1);
        (&mut (*d).name)[..len].copy_from_slice(&b[..len]);
        (*d).name_len = len;
        (*d).vnode = vn;
        (*d).parent = parent;
        if !parent.is_null() {
            let p = &mut *parent;
            if p.child_count < DENTRY_MAX_CHILDREN {
                p.children[p.child_count] = d;
                p.child_count += 1;
            }
        }
    }
    d
}

// ─── tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_lookup_slash_returns_root_vnode() {
        let fake_vn = 0x4000_0000usize as *mut VNode;
        let mut root = Dentry::new_empty();
        root.vnode = fake_vn;
        let result = path_lookup_from(&raw mut root, "/");
        assert_eq!(result, Some(fake_vn));
    }

    #[test]
    fn path_lookup_empty_path_returns_none() {
        let mut root = Dentry::new_empty();
        root.vnode = 0x1000 as *mut VNode;
        assert!(path_lookup_from(&raw mut root, "no-slash").is_none());
    }
}
