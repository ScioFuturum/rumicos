//! VFS-side fd helpers.
//!
//! The actual `FdTable` and `FdEntry` live in `kernel-proc` (to avoid a
//! circular crate dependency).  This module provides typed helpers that cast
//! the opaque `vnode_ptr: usize` to `*mut VNode`.

use crate::vnode::VNode;

/// Cast a `vnode_ptr` from `FdEntry` to a typed VNode reference.
///
/// # Safety
/// `ptr` must be a valid direct-map virtual address of a live `VNode`.
pub unsafe fn vnode_from_ptr(ptr: usize) -> *mut VNode {
    ptr as *mut VNode
}
