//! Per-process file-descriptor table.
//!
//! `FdEntry` stores a raw VNode pointer (direct-map VA) plus the current
//! seek position and open flags.  The concrete `VNode` type lives in
//! `kernel-fs`; proc sees only a `usize`.

pub const MAX_FDS: usize = 64;

/// Linux-compatible open(2) access-mode flags (low two bits of `flags`).
pub const O_RDONLY: u32 = 0; // open for reading only
pub const O_WRONLY: u32 = 1; // open for writing only
pub const O_RDWR: u32 = 2; // open for reading and writing
pub const O_CREAT: u32 = 64; // create file if not exists (future)
pub const O_TRUNC: u32 = 512; // truncate on open (future)

/// An open file-descriptor entry.
#[derive(Clone, Copy)]
pub struct FdEntry {
    /// Direct-map virtual address of the `VNode` (0 = empty slot).
    pub vnode_ptr: usize,
    /// Current read/write position.
    pub offset: u64,
    /// Open flags: O_RDONLY=0, O_WRONLY=1, O_RDWR=2.
    pub flags: u32,
    pub _reserved: u32,
}

impl FdEntry {
    /// Returns `true` if this fd was opened for reading.
    ///
    /// Linux returns `EBADF` (not `EACCES`) when the access direction is wrong.
    #[inline]
    pub fn can_read(&self) -> bool {
        let mode = self.flags & 3;
        mode == O_RDONLY || mode == O_RDWR
    }

    /// Returns `true` if this fd was opened for writing.
    #[inline]
    pub fn can_write(&self) -> bool {
        let mode = self.flags & 3;
        mode == O_WRONLY || mode == O_RDWR
    }
}

/// Per-process file-descriptor table.
pub struct FdTable {
    pub fds: [Option<FdEntry>; MAX_FDS],
}

// FdTable contains raw pointers (usize) which are not Send/Sync by default.
// Synchronisation is provided by the surrounding SpinLock.
unsafe impl Send for FdTable {}
unsafe impl Sync for FdTable {}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FdTable {
    pub const fn new() -> Self {
        Self {
            fds: [const { None }; MAX_FDS],
        }
    }

    /// Allocate the lowest free fd slot.  Returns fd ≥ 0 or -24 (EMFILE).
    pub fn alloc(&mut self, vnode_ptr: usize, flags: u32) -> i32 {
        for (i, slot) in self.fds.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(FdEntry {
                    vnode_ptr,
                    offset: 0,
                    flags,
                    _reserved: 0,
                });
                return i as i32;
            }
        }
        -24 // EMFILE
    }

    /// Return a shared reference to the entry or `None` if fd is invalid.
    pub fn get(&self, fd: i32) -> Option<&FdEntry> {
        if fd < 0 || fd as usize >= MAX_FDS {
            return None;
        }
        self.fds[fd as usize].as_ref()
    }

    /// Return a mutable reference to the entry or `None` if fd is invalid.
    pub fn get_mut(&mut self, fd: i32) -> Option<&mut FdEntry> {
        if fd < 0 || fd as usize >= MAX_FDS {
            return None;
        }
        self.fds[fd as usize].as_mut()
    }

    /// Close fd. Returns 0 on success or -9 (EBADF).
    pub fn close(&mut self, fd: i32) -> i32 {
        if fd < 0 || fd as usize >= MAX_FDS {
            return -9;
        }
        if self.fds[fd as usize].is_none() {
            return -9;
        }
        self.fds[fd as usize] = None;
        0
    }

    /// Return the `vnode_ptr` for `fd`, or 0 if the fd is not open.
    pub fn get_vnode_ptr(&self, fd: i32) -> usize {
        self.get(fd).map(|e| e.vnode_ptr).unwrap_or(0)
    }

    /// Clone fd entries for fork/clone.
    ///
    /// Every inherited descriptor is one more real reference to its VNode,
    /// so each occupied slot bumps the refcount through the registered
    /// `vnode_inc_ref` hook (`kernel-proc` cannot name the concrete VNode
    /// type — same cross-crate indirection `dup`/`dup2` already use; a
    /// no-op on host builds, where kernel-fs never registers it).
    ///
    /// This is load-bearing for pipes: a pipe end's reader/writer liveness
    /// IS its end-VNode refcount (see `kernel_fs::pipe`). Before this hook
    /// was wired, a forked child inheriting a pipe fd added a real holder
    /// without adding a reference, so the child's explicit `close()` (the
    /// standard shell-pipeline fd dance) drove the count to zero — and then
    /// past it, wrapping — while fds still referenced the end. Readers then
    /// saw EOF early, or after the wrap never saw it at all.
    pub fn clone_for_fork(&self) -> Self {
        for entry in self.fds.iter().flatten() {
            if entry.vnode_ptr != 0 {
                // SAFETY: vnode_ptr came from a live FdEntry in this table.
                unsafe { crate::syscall::vnode_inc_ref(entry.vnode_ptr) };
            }
        }
        Self { fds: self.fds }
    }

    /// `dup(oldfd)` — duplicate `old_fd` to the lowest free fd number.
    ///
    /// Both fds then reference the SAME underlying VNode (the new slot is a
    /// by-value copy of the old `FdEntry`), and the VNode's refcount is
    /// incremented via the registered `vnode_inc_ref` hook — this is what
    /// keeps a pipe end alive while any dup of it is open (pipe reader/writer
    /// liveness IS the end-VNode refcount; see `kernel_fs::pipe`).
    ///
    /// OFFSET SEMANTICS (Option 2, deliberate): the duplicated fd gets its
    /// OWN `offset` (a plain by-value `FdEntry` copy), so the two fds' file
    /// positions diverge after the first seek/rw — NOT POSIX `dup()`, which
    /// shares one file position via a shared open-file description. This
    /// matches this kernel's EXISTING accepted simplification for `fork()`
    /// (see `FdTable::clone_for_fork`: forked fds also get independent
    /// offsets). Implementing true shared offsets needs a refcounted
    /// open-file-description object between `FdTable` and `VNode`, a
    /// separately-invasive structural change left as future work. Pipes are
    /// unaffected either way (pipe read/write ignore the offset).
    ///
    /// Returns the new fd (>= 0), `-9` (EBADF) if `old_fd` is invalid, or
    /// `-24` (EMFILE) if the table is full. Preserves `old_fd`'s access mode
    /// (unlike a fresh `open`).
    pub fn dup(&mut self, old_fd: i32) -> i32 {
        let Some(entry) = self.get(old_fd).copied() else {
            return -9; // EBADF
        };
        for (i, slot) in self.fds.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(entry);
                // SAFETY: entry.vnode_ptr came from a live FdEntry; the hook
                // is a no-op until kernel-fs registers it (host build).
                unsafe { crate::syscall::vnode_inc_ref(entry.vnode_ptr) };
                return i as i32;
            }
        }
        -24 // EMFILE
    }

    /// `dup2(oldfd, newfd)` — make `new_fd` an alias of `old_fd`.
    ///
    /// POSIX special case: if `old_fd == new_fd`, return `new_fd` unchanged
    /// (do NOT close/reopen — that would transiently drop the refcount).
    /// Otherwise, if `new_fd` is already open it is closed first (silently),
    /// then aliased to `old_fd`'s VNode with its refcount incremented.
    ///
    /// Offset semantics are Option 2, exactly as [`dup`](Self::dup).
    ///
    /// KNOWN LIMITATION: closing the previous `new_fd` occupant here only
    /// decrements its VNode refcount (via `vnode_dec_ref`); it does NOT run
    /// that VNode's full `release` hook, so if the replaced fd happened to be
    /// the last reader/writer of a pipe with a peer blocked at that instant,
    /// the peer is not woken until the next pipe event. `close()` proper does
    /// run the full release. Not exercised by any current path (dup2 targets
    /// are unopened fds in practice).
    pub fn dup2(&mut self, old_fd: i32, new_fd: i32) -> i32 {
        if old_fd == new_fd {
            // Valid oldfd → return newfd untouched; invalid → EBADF.
            return if self.get(old_fd).is_some() { new_fd } else { -9 };
        }
        let Some(entry) = self.get(old_fd).copied() else {
            return -9; // EBADF
        };
        if new_fd < 0 || new_fd as usize >= MAX_FDS {
            return -9; // EBADF: target out of range
        }
        // Drop any existing occupant of new_fd first (see limitation above).
        if let Some(old) = self.fds[new_fd as usize].take() {
            // SAFETY: old.vnode_ptr came from a live FdEntry.
            unsafe { crate::syscall::vnode_dec_ref(old.vnode_ptr) };
        }
        self.fds[new_fd as usize] = Some(entry);
        // SAFETY: entry.vnode_ptr came from a live FdEntry.
        unsafe { crate::syscall::vnode_inc_ref(entry.vnode_ptr) };
        new_fd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_table_new_all_none() {
        let table = FdTable::new();
        for i in 0..MAX_FDS {
            assert!(table.get(i as i32).is_none(), "slot {i} should be empty");
        }
    }

    #[test]
    fn fd_table_default_matches_new() {
        let table = FdTable::default();
        for i in 0..MAX_FDS {
            assert!(
                table.get(i as i32).is_none(),
                "default-constructed slot {i} should be empty"
            );
        }
    }

    #[test]
    fn fd_table_alloc_get_close_roundtrip() {
        let mut table = FdTable::new();
        let fd = table.alloc(0x1000, 1);
        assert!(fd >= 0, "alloc should succeed");
        let e = table.get(fd).expect("get should return entry");
        assert_eq!(e.vnode_ptr, 0x1000);
        assert_eq!(e.flags, 1);
        assert_eq!(table.close(fd), 0);
        assert!(table.get(fd).is_none(), "slot should be empty after close");
    }

    #[test]
    fn fd_table_emfile_on_full() {
        let mut table = FdTable::new();
        for _ in 0..MAX_FDS {
            let fd = table.alloc(0x1000, 0);
            assert!(fd >= 0);
        }
        assert_eq!(table.alloc(0x1000, 0), -24, "65th alloc must return EMFILE");
    }

    #[test]
    fn fd_table_ebadf_on_invalid() {
        let mut table = FdTable::new();
        assert_eq!(table.close(0), -9, "closing empty slot must be EBADF");
        assert!(table.get(-1).is_none());
    }

    // ── Fix 2: O_RDONLY / O_WRONLY flag enforcement ───────────────────────

    #[test]
    fn stdin_stdout_stderr_constants_have_linux_values() {
        assert_eq!(O_RDONLY, 0, "O_RDONLY must be 0 (Linux ABI)");
        assert_eq!(O_WRONLY, 1, "O_WRONLY must be 1 (Linux ABI)");
        assert_eq!(O_RDWR, 2, "O_RDWR must be 2 (Linux ABI)");
    }

    #[test]
    fn fd_can_read_rdonly() {
        let mut t = FdTable::new();
        let fd = t.alloc(0x1000, O_RDONLY);
        let e = t.get(fd).unwrap();
        assert!(e.can_read(), "O_RDONLY fd must be readable");
        assert!(!e.can_write(), "O_RDONLY fd must not be writable");
    }

    #[test]
    fn fd_can_write_wronly() {
        let mut t = FdTable::new();
        let fd = t.alloc(0x1000, O_WRONLY);
        let e = t.get(fd).unwrap();
        assert!(!e.can_read(), "O_WRONLY fd must not be readable");
        assert!(e.can_write(), "O_WRONLY fd must be writable");
    }

    #[test]
    fn fd_rdwr_allows_both() {
        let mut t = FdTable::new();
        let fd = t.alloc(0x1000, O_RDWR);
        let e = t.get(fd).unwrap();
        assert!(e.can_read(), "O_RDWR fd must be readable");
        assert!(e.can_write(), "O_RDWR fd must be writable");
    }

    #[test]
    fn clone_for_fork_preserves_entries() {
        let mut t = FdTable::new();
        let fd = t.alloc(0xfeed, O_RDWR);
        t.get_mut(fd).unwrap().offset = 42;
        let child = t.clone_for_fork();
        let e = child.get(fd).unwrap();
        assert_eq!(e.vnode_ptr, 0xfeed);
        assert_eq!(e.offset, 42);
        assert_eq!(e.flags, O_RDWR);
    }

    // ── Part B: dup / dup2 ────────────────────────────────────────────────
    //
    // These are pure FdTable tests. `dup`/`dup2` also call the registered
    // `vnode_inc_ref`/`vnode_dec_ref` hooks; on host those are no-ops (no
    // hook registered), so these tests deliberately assert only the slot
    // bookkeeping, never touching the process-wide hook statics (which the
    // syscall.rs refcount round-trip test owns exclusively).

    #[test]
    fn dup_aliases_vnode_at_lowest_free_fd() {
        let mut t = FdTable::new();
        let fd = t.alloc(0xcafe, O_RDWR);
        let dup_fd = t.dup(fd);
        assert!(dup_fd >= 0, "dup should succeed");
        assert_ne!(dup_fd, fd, "dup must allocate a NEW fd number");
        // Both fds reference the SAME underlying vnode (the sharing).
        assert_eq!(t.get(dup_fd).unwrap().vnode_ptr, 0xcafe);
        assert_eq!(t.get(fd).unwrap().vnode_ptr, 0xcafe);
        // dup preserves the access mode.
        assert_eq!(t.get(dup_fd).unwrap().flags, O_RDWR);
    }

    #[test]
    fn dup_offsets_are_independent_option2() {
        // Documents the Option-2 choice: dup'd fds do NOT share a file
        // position (matching fork's accepted simplification). Mutating one
        // fd's offset must not disturb the other's.
        let mut t = FdTable::new();
        let fd = t.alloc(0x1000, O_RDWR);
        t.get_mut(fd).unwrap().offset = 10;
        let dup_fd = t.dup(fd);
        t.get_mut(fd).unwrap().offset = 999;
        assert_eq!(t.get(dup_fd).unwrap().offset, 10, "dup'd fd keeps its own offset");
        assert_eq!(t.get(fd).unwrap().offset, 999);
    }

    #[test]
    fn dup_bad_fd_is_ebadf() {
        let mut t = FdTable::new();
        assert_eq!(t.dup(0), -9, "dup of an unopened fd is EBADF");
        assert_eq!(t.dup(-1), -9);
    }

    #[test]
    fn dup_on_full_table_is_emfile() {
        let mut t = FdTable::new();
        let fd = t.alloc(0x2000, O_RDONLY);
        // Fill the rest of the table.
        while t.alloc(0x3000, O_RDONLY) >= 0 {}
        assert_eq!(t.dup(fd), -24, "dup on a full table is EMFILE");
    }

    #[test]
    fn dup2_same_fd_is_noop_returns_fd() {
        let mut t = FdTable::new();
        let fd = t.alloc(0x4000, O_RDWR);
        // POSIX: dup2(fd, fd) returns fd without touching anything.
        assert_eq!(t.dup2(fd, fd), fd);
        assert_eq!(t.get(fd).unwrap().vnode_ptr, 0x4000);
    }

    #[test]
    fn dup2_same_fd_but_invalid_is_ebadf() {
        let mut t = FdTable::new();
        // dup2(bad, bad): even the same-fd fast path must reject an
        // unopened fd (POSIX returns EBADF).
        assert_eq!(t.dup2(5, 5), -9);
    }

    #[test]
    fn dup2_aliases_onto_chosen_fd() {
        let mut t = FdTable::new();
        let fd = t.alloc(0x5000, O_WRONLY);
        let target = 20;
        assert!(t.get(target).is_none(), "target starts free");
        assert_eq!(t.dup2(fd, target), target);
        assert_eq!(t.get(target).unwrap().vnode_ptr, 0x5000);
        assert_eq!(t.get(target).unwrap().flags, O_WRONLY);
        // Original still valid too.
        assert_eq!(t.get(fd).unwrap().vnode_ptr, 0x5000);
    }

    #[test]
    fn dup2_replaces_open_target() {
        let mut t = FdTable::new();
        let a = t.alloc(0x6000, O_RDONLY);
        let b = t.alloc(0x7000, O_WRONLY);
        // dup2(a, b): b's old vnode (0x7000) is dropped, b now aliases a.
        assert_eq!(t.dup2(a, b), b);
        assert_eq!(t.get(b).unwrap().vnode_ptr, 0x6000);
        assert_eq!(t.get(b).unwrap().flags, O_RDONLY);
    }

    #[test]
    fn dup2_bad_old_fd_is_ebadf() {
        let mut t = FdTable::new();
        assert_eq!(t.dup2(3, 4), -9, "dup2 of an unopened oldfd is EBADF");
    }

    #[test]
    fn dup2_out_of_range_new_fd_is_ebadf() {
        let mut t = FdTable::new();
        let fd = t.alloc(0x8000, O_RDWR);
        assert_eq!(t.dup2(fd, MAX_FDS as i32), -9, "newfd past MAX_FDS is EBADF");
        assert_eq!(t.dup2(fd, -1), -9);
    }
}
