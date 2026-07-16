use kernel_paging::PageFlags;

pub const MAX_VMAS: usize = 32;
pub const MMAP_BASE: u64 = 0x0000_4000_0000_0000;
pub const MMAP_REGION_END: u64 = 0x0000_7000_0000_0000;

pub const PROT_READ: u32 = 1;
pub const PROT_WRITE: u32 = 2;
pub const PROT_EXEC: u32 = 4;
pub const MAP_SHARED: u32 = 0x01;
pub const MAP_PRIVATE: u32 = 0x02;
pub const MAP_ANONYMOUS: u32 = 0x20;

pub const SYS_MMAP: u64 = 9;
pub const SYS_MUNMAP: u64 = 11;

const EINVAL: i64 = -22;
const ENOMEM: i64 = -12;
const EBADF: i64 = -9;
const EACCES: i64 = -13;

/// What physical content backs a VMA's pages.
///
/// Replaces the previous `Vma.anon: bool` field. `File`'s `vnode_ptr` is a
/// raw, opaque direct-map VA (the exact same convention `FdEntry.vnode_ptr`
/// already uses) rather than a typed `*mut kernel_fs::vnode::VNode` — the
/// idealized design for this checkpoint assumed `Backing::File` could hold
/// a typed VNode pointer directly, but kernel-proc cannot depend on
/// kernel-fs (kernel-fs already depends on kernel-proc; Cargo doesn't
/// allow the cycle that would create), so this mirrors the exact same
/// substitution `FdEntry` already made for the same reason. Every place
/// that needs to actually dereference the vnode (page-cache fill, dirty
/// marking, writeback, refcounting) goes through the registration hooks in
/// `crate::syscall` (`page_cache_get_or_fill`, `page_cache_mark_dirty`,
/// `page_cache_writeback`, `vnode_inc_ref`, `vnode_dec_ref`) that
/// kernel-fs fills in at VFS init time — the same pattern already
/// established for `EXEC_LOADER`/`CHAIN_HANDLER`/`SERIAL_VNODE_PTR`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, u8)]
pub enum Backing {
    Anonymous,
    File {
        /// Direct-map VA of the backing `VNode` (opaque to kernel-proc).
        vnode_ptr: usize,
        /// Byte offset into the file at `Vma::start`. Always page-aligned.
        file_offset: u64,
        /// `true` = MAP_SHARED (writes go back to the file, never CoW).
        /// `false` = MAP_PRIVATE (demand-paged from the page cache,
        /// copy-on-write on the first actual write).
        shared: bool,
    },
}
// ^ #[repr(C, u8)]: a stable, guaranteed layout. Kept deliberately after a
// debugging incident on

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub flags: PageFlags,
    pub backing: Backing,
}

impl Vma {
    pub const fn contains(&self, addr: u64) -> bool {
        self.start <= addr && addr < self.end
    }

    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// A `Vma` always spans at least one page in practice (`mmap`
    /// rejects `length == 0` before ever constructing one), but this is
    /// provided alongside `len()` so the pair satisfies the conventional
    /// `len`/`is_empty` API shape rather than leaving `is_empty` implied.
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// `true` for `Backing::Anonymous` or a MAP_PRIVATE file mapping —
    /// both get ordinary CoW-on-fork/CoW-on-write treatment. `false` only
    /// for MAP_SHARED file mappings, which must never be marked CoW (see
    /// `AddressSpace::fork_cow`'s early-out and `resolve_file_fault`).
    pub const fn is_cow_eligible(&self) -> bool {
        !matches!(self.backing, Backing::File { shared: true, .. })
    }
}

pub const fn flags_writable(flags: PageFlags) -> bool {
    (flags.as_u64() & PageFlags::WRITABLE) != 0
}

pub fn prot_to_page_flags(prot: u32) -> PageFlags {
    let mut flags = PageFlags::new().with_present().with_user_accessible();
    if (prot & PROT_WRITE) != 0 {
        flags = flags.with_writable();
    }
    if (prot & PROT_EXEC) == 0 {
        flags = flags.with_no_execute();
    }
    flags
}

pub fn align_up_page(value: u64) -> Option<u64> {
    value.checked_add(4095).map(|v| v & !4095)
}

pub fn align_down_page(value: u64) -> u64 {
    value & !4095
}

/// `sys_mmap(addr_hint, length, prot, flags, fd, offset) -> i64`.
///
/// The design brief's idealized signature carries `fd`/`offset` alongside
/// `addr_hint`/`length`/`prot`/`flags` — matches exactly; the previous
/// checkpoint had dropped `offset` since file-backed mmap wasn't
/// implemented yet (nothing consumed it). It's back now.
///
/// Every check that depends only on the arguments themselves — never on
/// `current_process()` or any other live kernel state — runs first, in
/// one unbroken block, before `current_process()` is called at all. This
/// isn't just style: `current_process()` reaches
/// `kernel_cpu::current_cpu_id()`, which reads a GS-segment-relative
/// per-CPU field that's only ever installed by `install_per_cpu_gs` on
/// real hardware — calling it from a host `cargo test` process (no GS
/// base installed) reads through an arbitrary offset into whatever the
/// host OS happens to have GS pointed at, which reliably segfaults.
/// Keeping every pure-argument-validation EINVAL/EBADF check ahead of
/// that call is what makes this function's early-rejection paths
/// host-testable at all (see the `sys_mmap_rejects_*` tests below).
pub fn sys_mmap(addr_hint: u64, length: u64, prot: u32, flags: u32, fd: i64, offset: u64) -> i64 {
    if addr_hint != 0 {
        return EINVAL; // MAP_FIXED still unsupported
    }
    if length == 0 {
        return EINVAL;
    }
    if offset & 4095 != 0 {
        return EINVAL; // offset must be page-aligned
    }
    if (prot & !(PROT_READ | PROT_WRITE | PROT_EXEC)) != 0 {
        return EINVAL;
    }
    if (flags & (MAP_SHARED | MAP_PRIVATE)) == 0 || (flags & (MAP_SHARED | MAP_PRIVATE)) == (MAP_SHARED | MAP_PRIVATE)
    {
        return EINVAL; // exactly one of MAP_SHARED/MAP_PRIVATE, Linux-style
    }
    let is_anon = (flags & MAP_ANONYMOUS) != 0;
    if is_anon && fd != -1 {
        return EINVAL; // MAP_ANONYMOUS with a real fd is nonsense
    }
    if !is_anon && fd < 0 {
        return EBADF;
    }
    let len = match align_up_page(length) {
        Some(v) if v != 0 => v,
        _ => return EINVAL,
    };

    // Every check above depends only on this call's own arguments. Past
    // this point, resolving a real fd or reserving a VMA needs the live
    // current process — see this function's doc comment for why that
    // split matters.
    let proc = crate::syscall::current_process();
    if proc.is_null() {
        return ENOMEM;
    }

    let backing = if is_anon {
        Backing::Anonymous
    } else {
        // SAFETY: proc is the live process for this syscall.
        let entry = unsafe { (*proc).fd_table.lock().get(fd as i32).copied() };
        let Some(entry) = entry else {
            return EBADF;
        };
        if (flags & MAP_SHARED) != 0 && (prot & PROT_WRITE) != 0 && !entry.can_write() {
            return EACCES; // shared+writable mapping needs a writable fd
        }
        if entry.vnode_ptr == 0 {
            return EBADF;
        }
        // SAFETY: entry.vnode_ptr came from a live FdEntry, which is only
        // ever populated with a valid, refcounted VNode's direct-map VA.
        unsafe { crate::syscall::vnode_inc_ref(entry.vnode_ptr) };
        Backing::File {
            vnode_ptr: entry.vnode_ptr,
            file_offset: offset,
            shared: (flags & MAP_SHARED) != 0,
        }
    };

    let page_flags = prot_to_page_flags(prot);
    unsafe {
        // SAFETY: current_process returned the live process for this
        // syscall; its address_space pointer is a live, refcounted
        // AddressSpace (see AddressSpace::alloc_shared).
        match (*(*proc).address_space).mmap(len, page_flags, backing) {
            Some(addr) => addr as i64,
            None => {
                // Roll back the inc_ref() above — the mapping never took.
                if let Backing::File { vnode_ptr, .. } = backing {
                    crate::syscall::vnode_dec_ref(vnode_ptr);
                }
                ENOMEM
            }
        }
    }
}

pub fn sys_munmap(addr: u64, length: u64) -> i64 {
    if addr == 0 || length == 0 || addr & 4095 != 0 {
        return EINVAL;
    }
    let len = match align_up_page(length) {
        Some(v) if v != 0 => v,
        _ => return EINVAL,
    };
    let proc = crate::syscall::current_process();
    if proc.is_null() {
        return EINVAL;
    }
    unsafe {
        // SAFETY: current_process returned the live process for this
        // syscall; its address_space pointer is a live, refcounted
        // AddressSpace (see AddressSpace::alloc_shared).
        if (*(*proc).address_space).munmap(addr, len) {
            0
        } else {
            EINVAL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmap_syscall_numbers_match_linux() {
        assert_eq!(SYS_MMAP, 9);
        assert_eq!(SYS_MUNMAP, 11);
    }

    #[test]
    fn map_shared_flag_matches_linux() {
        assert_eq!(MAP_SHARED, 0x01);
        assert_eq!(MAP_PRIVATE, 0x02);
    }

    #[test]
    fn prot_write_sets_writable() {
        let flags = prot_to_page_flags(PROT_READ | PROT_WRITE);
        assert_eq!(flags.as_u64() & PageFlags::WRITABLE, PageFlags::WRITABLE);
        assert_ne!(flags.as_u64() & PageFlags::NO_EXECUTE, 0);
    }

    #[test]
    fn prot_exec_clears_no_execute() {
        let flags = prot_to_page_flags(PROT_READ | PROT_EXEC);
        assert_eq!(flags.as_u64() & PageFlags::NO_EXECUTE, 0);
    }

    #[test]
    fn vma_contains_start_but_not_end() {
        let vma = Vma {
            start: 0x1000,
            end: 0x3000,
            flags: PageFlags::new(),
            backing: Backing::Anonymous,
        };
        assert!(vma.contains(0x1000));
        assert!(vma.contains(0x2fff));
        assert!(!vma.contains(0x3000));
    }

    #[test]
    fn vma_len_and_is_empty_agree() {
        let nonempty = Vma {
            start: 0x1000,
            end: 0x3000,
            flags: PageFlags::new(),
            backing: Backing::Anonymous,
        };
        assert_eq!(nonempty.len(), 0x2000);
        assert!(!nonempty.is_empty());

        let empty = Vma {
            start: 0x5000,
            end: 0x5000,
            flags: PageFlags::new(),
            backing: Backing::Anonymous,
        };
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
    }

    #[test]
    fn anonymous_and_private_file_backing_are_cow_eligible() {
        let anon = Vma {
            start: 0,
            end: 0x1000,
            flags: PageFlags::new(),
            backing: Backing::Anonymous,
        };
        assert!(anon.is_cow_eligible());

        let private_file = Vma {
            start: 0,
            end: 0x1000,
            flags: PageFlags::new(),
            backing: Backing::File {
                vnode_ptr: 0x9000,
                file_offset: 0,
                shared: false,
            },
        };
        assert!(private_file.is_cow_eligible());
    }

    #[test]
    fn shared_file_backing_is_never_cow_eligible() {
        let shared_file = Vma {
            start: 0,
            end: 0x1000,
            flags: PageFlags::new(),
            backing: Backing::File {
                vnode_ptr: 0x9000,
                file_offset: 0,
                shared: true,
            },
        };
        assert!(!shared_file.is_cow_eligible());
    }

    #[test]
    fn flags_writable_reflects_writable_bit() {
        assert!(flags_writable(PageFlags::new().with_writable()));
        assert!(!flags_writable(PageFlags::new()));
    }

    #[test]
    fn sys_mmap_rejects_map_anonymous_with_real_fd() {
        assert_eq!(
            sys_mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, 3, 0),
            EINVAL
        );
    }

    #[test]
    fn sys_mmap_rejects_unaligned_offset() {
        assert_eq!(
            sys_mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 100),
            EINVAL
        );
    }

    #[test]
    fn sys_mmap_rejects_neither_shared_nor_private() {
        assert_eq!(sys_mmap(0, 4096, PROT_READ, MAP_ANONYMOUS, -1, 0), EINVAL);
    }

    #[test]
    fn sys_mmap_rejects_both_shared_and_private() {
        assert_eq!(
            sys_mmap(
                0,
                4096,
                PROT_READ,
                MAP_SHARED | MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0
            ),
            EINVAL
        );
    }
}
