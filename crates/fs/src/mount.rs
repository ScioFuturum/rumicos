//! Mount table.  Up to 16 simultaneous mounts.

use crate::dentry::Dentry;
use crate::vnode::VNode;
use kernel_sync::SpinLock;

pub struct Mount {
    pub mountpoint: *mut Dentry,
    pub root_vnode: *mut VNode,
    pub fs_type: &'static str,
}

unsafe impl Send for Mount {}

const MAX_MOUNTS: usize = 16;

static MOUNT_TABLE: SpinLock<[Option<Mount>; MAX_MOUNTS]> =
    SpinLock::new([const { None }; MAX_MOUNTS]);

/// Register a filesystem mount.
///
/// # Safety
/// `root_vnode` must remain valid for the kernel's lifetime.
pub unsafe fn mount(_path: &str, root_vnode: *mut VNode, fs_type: &'static str) {
    let mut table = MOUNT_TABLE.lock();
    for slot in table.iter_mut() {
        if slot.is_none() {
            *slot = Some(Mount {
                mountpoint: core::ptr::null_mut(),
                root_vnode,
                fs_type,
            });
            return;
        }
    }
    panic!("mount table full");
}
