use kernel_cpu::InterruptFrame;
use kernel_paging::PageFlags;
#[cfg(target_os = "none")]
use kernel_paging::PhysAddr;

pub const PF_VECTOR: u8 = 14;

const PF_PRESENT: u64 = 1 << 0;
const PF_WRITE: u64 = 1 << 1;
const PF_USER: u64 = 1 << 2;
const SIGSEGV_EXIT: i32 = -11;

#[cfg(target_os = "none")]
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;
#[cfg(target_os = "none")]
const PAGE_SIZE: u64 = 4096;

pub fn pf_handler(frame: &mut InterruptFrame, _vector: u8) {
    let fault_addr = read_fault_addr();
    let is_write = (frame.error_code & PF_WRITE) != 0;
    let is_present = (frame.error_code & PF_PRESENT) != 0;
    let proc = crate::syscall::current_process();
    if proc.is_null() {
        panic!("page fault without current process at {fault_addr:#x}");
    }

    let resolved = unsafe {
        // SAFETY: current_process returned a live PCB for this faulting thread.
        let proc = &mut *proc;
        if is_present && is_write {
            // Present + write covers two distinct real cases, resolved by
            // ONE call here rather than two separate checks merged
            // together: (a) a PTE_COW page (either an anonymous CoW page
            // or a MAP_PRIVATE file mapping's still-shared cache frame),
            // which resolve_cow_fault actually handles; (b) a MAP_SHARED
            // file page being written. Case (b) does NOT need a second
            // call here: resolve_file_fault already maps a MAP_SHARED
            // page writable (and marks it dirty) on its very FIRST fault
            // whenever the VMA itself permits writes, so there is no
            // "read-only then upgrade to writable" state for a MAP_SHARED
            // page to ever land in — a present+write fault on one only
            // happens when the VMA genuinely forbids writes (no
            // PROT_WRITE), which correctly falls through to SIGSEGV
            // below. Kept as an explicit comment rather than merging the
            // two checks so this reasoning doesn't silently bit-rot if
            // resolve_file_fault's mapping policy ever changes.
            (*proc.address_space).resolve_cow_fault(fault_addr)
        } else if !is_present {
            (*proc.address_space).resolve_file_fault(fault_addr)
        } else {
            false
        }
    };

    if resolved {
        return;
    }

    if (frame.error_code & PF_USER) != 0 {
        // One compact diagnostic line before the kill. Killing a process
        // for an unresolved user fault is correct POSIX-ish behaviour, but
        // doing it in total silence cost this project hours of blind
        // debugging during the shell checkpoint — never again.
        #[cfg(target_os = "none")]
        unsafe {
            let hex = |v: u64| {
                for i in (0..16).rev() {
                    let d = ((v >> (i * 4)) & 0xf) as u8;
                    crate::syscall::serial_write_byte(if d < 10 { b'0' + d } else { b'a' + d - 10 });
                }
            };
            for b in b"\n[SIGSEGV addr=" {
                crate::syscall::serial_write_byte(*b);
            }
            hex(fault_addr);
            for b in b" rip=" {
                crate::syscall::serial_write_byte(*b);
            }
            hex(frame.rip);
            for b in b" err=" {
                crate::syscall::serial_write_byte(*b);
            }
            hex(frame.error_code);
            for b in b"]\n" {
                crate::syscall::serial_write_byte(*b);
            }
        }
        unsafe {
            // SAFETY: current_process returned this thread's process above.
            (*crate::syscall::current_process()).exit(SIGSEGV_EXIT);
        }
    }

    panic!(
        "unhandled kernel page fault at {fault_addr:#x}, rip={:#x}, err={:#x}",
        frame.rip, frame.error_code
    );
}

#[cfg(target_os = "none")]
fn read_fault_addr() -> u64 {
    unsafe {
        // SAFETY: page-fault handler runs in ring 0.
        kernel_paging::tlb::read_cr2()
    }
}

#[cfg(not(target_os = "none"))]
fn read_fault_addr() -> u64 {
    0
}

/// Only ever called from `AddressSpace::resolve_cow_fault_inner`/
/// `resolve_file_fault`, both `#[cfg(target_os = "none")]`-gated — by
/// design, these two helpers exist purely to serve the real kernel target
/// (there's no meaningful host equivalent of "flip a PTE's Writable/CoW
/// bits"). That means a host build of THIS crate as a plain library
/// dependency (e.g. kernel-fs pulling in kernel-proc, with no `--cfg
/// test`) has no reachable call site at all, which `-D warnings` would
/// otherwise report as `dead_code`. Exercised directly by
/// `cow_writable_flags_set_writable_and_clear_cow`/
/// `cow_readonly_flags_clear_writable_and_set_cow` below whenever
/// kernel-proc itself is the crate under test.
#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub(crate) fn cow_writable_flags(flags: PageFlags) -> PageFlags {
    flags.with_writable().without_cow()
}

#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]
pub(crate) fn cow_readonly_flags(flags: PageFlags) -> PageFlags {
    flags.without_writable().with_cow()
}

#[cfg(target_os = "none")]
pub(crate) unsafe fn copy_frame(dst: PhysAddr, src: PhysAddr) {
    unsafe {
        // SAFETY: both physical frames are mapped by the direct map and valid
        // for exactly one page.
        core::ptr::copy_nonoverlapping(
            (DIRECT_MAP_BASE + src.as_u64()) as *const u8,
            (DIRECT_MAP_BASE + dst.as_u64()) as *mut u8,
            PAGE_SIZE as usize,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cow_readonly_flags_clear_writable_and_set_cow() {
        let flags = PageFlags::new().with_present().with_writable();
        let cow = cow_readonly_flags(flags);
        assert_eq!(cow.as_u64() & PageFlags::WRITABLE, 0);
        assert!(cow.is_cow());
    }

    #[test]
    fn cow_writable_flags_set_writable_and_clear_cow() {
        let flags = PageFlags::new().with_present().with_cow();
        let writable = cow_writable_flags(flags);
        assert_ne!(writable.as_u64() & PageFlags::WRITABLE, 0);
        assert!(!writable.is_cow());
    }
}
