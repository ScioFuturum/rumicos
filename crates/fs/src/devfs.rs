//! Minimal device filesystem.
//!
//! Creates /dev/null, /dev/zero, /dev/serial VNodes and inserts them into
//! the /dev ramfs directory.  The serial VNode's address is registered with
//! kernel-proc so that new processes get it on fd 0/1/2.

use crate::ramfs::ramfs_dir_add_child;
use crate::vnode::{
    VNode, VNodeOps, vnode_create_noop, vnode_lookup_noop, vnode_readdir_noop,
    vnode_release_noop, vnode_truncate_noop,
};
// Only the target_os = "none" vnode constructors below need these.
#[cfg(target_os = "none")]
use crate::vnode::{VNodeType, alloc_ino};
#[cfg(target_os = "none")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicUsize, Ordering};

// All consumed only by the target_os = "none" device implementations.
#[cfg(target_os = "none")]
const DMAP: usize = 0xFFFF_8000_0000_0000;
#[cfg(target_os = "none")]
const COM1_DATA: u16 = 0x3F8;
#[cfg(target_os = "none")]
const COM1_LSR: u16 = 0x3FD;
#[cfg(target_os = "none")]
const LSR_DR: u8 = 0x01; // data ready
#[cfg(target_os = "none")]
const LSR_THRE: u8 = 0x20; // TX holding register empty

// ─── /dev/null ───────────────────────────────────────────────────────────

fn devnull_read(_: &VNode, _: &mut [u8], _: u64) -> i64 {
    0
}
fn devnull_write(_: &VNode, buf: &[u8], _: u64) -> i64 {
    buf.len() as i64
}

pub static DEVNULL_OPS: VNodeOps = VNodeOps {
    read: devnull_read,
    write: devnull_write,
    lookup: vnode_lookup_noop,
    create: vnode_create_noop,
    readdir: vnode_readdir_noop,
    truncate: vnode_truncate_noop,
    release: vnode_release_noop,
};

// ─── /dev/zero ───────────────────────────────────────────────────────────

fn devzero_read(_: &VNode, buf: &mut [u8], _: u64) -> i64 {
    for b in buf.iter_mut() {
        *b = 0;
    }
    buf.len() as i64
}
fn devzero_write(_: &VNode, buf: &[u8], _: u64) -> i64 {
    buf.len() as i64
}

pub static DEVZERO_OPS: VNodeOps = VNodeOps {
    read: devzero_read,
    write: devzero_write,
    lookup: vnode_lookup_noop,
    create: vnode_create_noop,
    readdir: vnode_readdir_noop,
    truncate: vnode_truncate_noop,
    release: vnode_release_noop,
};

// ─── /dev/serial ─────────────────────────────────────────────────────────

fn devserial_write(_: &VNode, buf: &[u8], _: u64) -> i64 {
    #[cfg(target_os = "none")]
    for &b in buf {
        unsafe {
            serial_write_byte(b);
        }
    }
    buf.len() as i64
}

fn devserial_read(_: &VNode, buf: &mut [u8], _: u64) -> i64 {
    #[cfg(target_os = "none")]
    {
        let mut n = 0i64;
        for b in buf.iter_mut() {
            if unsafe { inb(COM1_LSR) } & LSR_DR == 0 {
                break;
            }
            *b = unsafe { inb(COM1_DATA) };
            n += 1;
        }
        n
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = buf;
        0
    }
}

pub static DEVSERIAL_OPS: VNodeOps = VNodeOps {
    read: devserial_read,
    write: devserial_write,
    lookup: vnode_lookup_noop,
    create: vnode_create_noop,
    readdir: vnode_readdir_noop,
    truncate: vnode_truncate_noop,
    release: vnode_release_noop,
};

// ─── global serial VNode pointer ─────────────────────────────────────────

static SERIAL_VN: AtomicUsize = AtomicUsize::new(0);

/// Return the /dev/serial VNode pointer (direct-map VA).
pub fn serial_vnode() -> *mut VNode {
    SERIAL_VN.load(Ordering::Acquire) as *mut VNode
}

// ─── init ─────────────────────────────────────────────────────────────────

/// Populate `dev_dir` with /dev/null, /dev/zero, /dev/serial and register
/// the serial VNode with kernel-proc.
pub fn init_devfs(dev_dir: *mut VNode) {
    let null_vn = make_device_vnode(&DEVNULL_OPS);
    let zero_vn = make_device_vnode(&DEVZERO_OPS);
    let serial_vn = make_device_vnode(&DEVSERIAL_OPS);

    ramfs_dir_add_child(dev_dir, "null", null_vn);
    ramfs_dir_add_child(dev_dir, "zero", zero_vn);
    ramfs_dir_add_child(dev_dir, "serial", serial_vn);

    SERIAL_VN.store(serial_vn as usize, Ordering::Release);
    // Tell kernel-proc about the serial VNode so Process::create can
    // pre-open fd 0/1/2 for new processes.
    kernel_proc::set_serial_vnode(serial_vn as usize);
}

fn make_device_vnode(ops: &'static VNodeOps) -> *mut VNode {
    #[cfg(target_os = "none")]
    {
        let frame = kernel_memory::alloc_frame();
        let vn = (DMAP + frame.as_u64() as usize) as *mut VNode;
        unsafe {
            core::ptr::write(
                vn,
                VNode {
                    vtype: VNodeType::CharDevice,
                    ino: alloc_ino(),
                    size: 0,
                    refcount: AtomicU32::new(1),
                    ops,
                    fs_data: 0,
                    lock: kernel_sync::SpinLock::new(()),
                },
            );
        }
        vn
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = ops;
        core::ptr::null_mut()
    }
}

// ─── I/O helpers ─────────────────────────────────────────────────────────

#[cfg(target_os = "none")]
unsafe fn serial_write_byte(b: u8) {
    unsafe {
        while inb(COM1_LSR) & LSR_THRE == 0 {
            core::hint::spin_loop();
        }
        outb(COM1_DATA, b);
    }
}

#[cfg(target_os = "none")]
unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe {
        core::arch::asm!("in al, dx", in("dx") port, lateout("al") v,
        options(nomem, nostack));
    }
    v
}

#[cfg(target_os = "none")]
unsafe fn outb(port: u16, v: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") v,
        options(nomem, nostack));
    }
}
