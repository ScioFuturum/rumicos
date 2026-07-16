//! `execve()` — replace the calling process's image with a new ELF binary.
//!
//! ## Atomicity contract
//!
//! `do_execve` is split into two phases:
//!
//!   * **Phase 1 (reversible).** Read the target file from the VFS, parse and
//!     validate its ELF header, and build a brand-new [`AddressSpace`] —
//!     mapping every `PT_LOAD` segment and the initial user stack — without
//!     touching anything that belongs to the *current* process. Every
//!     fallible step lives here. If any of them fails we free whatever the
//!     new address space had already allocated and return a negative errno;
//!     the caller's process, address space, and file descriptors are
//!     completely untouched.
//!
//!   * **Phase 2 (point of no return).** Once the new image is fully built
//!     and known-good, swap it into the current [`Process`], free the old
//!     address space, and transition the CPU directly into ring 3 at the new
//!     entry point. This phase cannot fail and never returns to its caller.
//!
//! ## Why this doesn't go through `kernel_sched::schedule()`
//!
//! kernel-sched's `switch_context` always captures the *literal* current
//! RSP/RIP of the thread being switched away from (see
//! `kernel_sched::switch::switch_context`'s `.Lswitch_return` label) — it has
//! no notion of "resume somewhere else next time". Calling `schedule()` from
//! here would, at best, suspend this thread *inside `do_execve` itself* and
//! later resume it right back here — exactly the "returns to the old image"
//! outcome execve must never produce. Worse, if this thread is the only
//! runnable one, `schedule()` pops it straight back off the run queue and
//! returns immediately, falling through to the syscall epilogue with the
//! *old* image's saved `RCX`/user `RSP`.
//!
//! So instead of asking the scheduler to switch to "this same thread,
//! later", Phase 2 does exactly what a brand-new process's first run does
//! (see `process::ring3_entry_rust`): it calls [`process::enter_user_mode`]
//! directly. The current kernel stack is simply abandoned at that point —
//! there is nothing left on it worth returning to.

#[cfg(target_os = "none")]
use crate::address_space::AddressSpace;
#[cfg(target_os = "none")]
use crate::elf::parse_elf;
#[cfg(target_os = "none")]
use crate::fd::FdTable;

/// Maximum number of `argv` entries accepted by `execve()`.
pub const MAX_ARGV: usize = 32;
/// Maximum number of `envp` entries accepted by `execve()`.
pub const MAX_ENVP: usize = 32;
// Only consumed by `setup_stack`, which is itself compiled for the real
// kernel target *or* under `cfg(test)` (see the comment on `setup_stack`
// below) — gated the same way so it isn't dead code on a plain host build.
#[cfg(any(target_os = "none", test))]
const MAX_ARGS_TOTAL: usize = MAX_ARGV + MAX_ENVP;

const ENOENT: i32 = -2;
#[cfg(target_os = "none")]
const EIO: i32 = -5;
const E2BIG: i32 = -7;
#[cfg(target_os = "none")]
const ENOEXEC: i32 = -8;

/// Direct-map base: `kern_virt = DIRECT_MAP_BASE + phys`. Must match the
/// value passed to `kernel_memory::init_frame_allocator` / used throughout
/// `kernel-proc` (see `address_space.rs`).
#[cfg(target_os = "none")]
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;

/// Maximum size of an executable this implementation will load, matching
/// ramfs's per-file cap (`RAMFS_MAX_BLOCKS * RAMFS_BLOCK_SIZE`). Kept in sync
/// manually since kernel-proc cannot depend on kernel-fs (see below).
#[cfg(target_os = "none")]
const MAX_ELF_SIZE: usize = 1024 * 1024;
/// `alloc_order` exponent that yields a `MAX_ELF_SIZE`-byte contiguous
/// buffer: `2^ELF_BUF_ORDER` frames of 4 KiB each.
#[cfg(target_os = "none")]
const ELF_BUF_ORDER: u8 = 8;

/// Execute a new ELF image in the context of the calling thread's process.
///
/// `path` is resolved through whatever loader kernel-fs registered via
/// [`crate::syscall::register_exec_loader`]. `argv`/`envp` must already be
/// safely copied out of user memory (see `crate::syscall::sys_execve`) —
/// `do_execve` itself never touches user pointers directly.
///
/// Returns a negative errno if Phase 1 fails; the caller is left completely
/// unchanged in that case. **Never returns on success** — Phase 2 transitions
/// the CPU into ring 3 directly and abandons the current kernel stack.
///
/// # Safety
/// Must be called with interrupts disabled (`IF=0`), on the kernel stack of
/// the thread belonging to the process being replaced, and only after
/// `argv`/`envp` have already been copied out of user memory — i.e. from
/// [`crate::syscall::sys_execve`] (the `SYS_EXECVE` syscall path) or from
/// [`crate::process::Process::exec`], which simply forwards to this.
pub unsafe fn do_execve(path: &str, argv: &[&str], envp: &[&str]) -> i32 {
    if argv.len() > MAX_ARGV || envp.len() > MAX_ENVP {
        return E2BIG;
    }

    #[cfg(target_os = "none")]
    {
        // ── Phase 1: load and validate the new image ──────────────────
        let buf_phys = kernel_memory::alloc_order(ELF_BUF_ORDER);
        let buf_virt = (DIRECT_MAP_BASE + buf_phys.as_u64()) as *mut u8;
        // SAFETY: buf_phys is a freshly allocated, exclusively-owned
        // ELF_BUF_ORDER-order (1 MiB) contiguous region; buf_virt is its
        // direct-map alias, valid for MAX_ELF_SIZE bytes.
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_virt, MAX_ELF_SIZE) };

        let bytes_read = crate::syscall::load_exec_file(path, buf);
        if bytes_read < 0 {
            free_elf_buf(buf_phys);
            return if bytes_read == -2 { ENOENT } else { EIO };
        }
        if bytes_read == 0 {
            free_elf_buf(buf_phys);
            return ENOEXEC;
        }

        let elf_data = &buf[..bytes_read as usize];
        let elf_info = match parse_elf(elf_data) {
            Ok(info) => info,
            Err(_) => {
                free_elf_buf(buf_phys);
                return ENOEXEC;
            }
        };

        // Build the replacement address space in full before touching
        // anything that belongs to the current process. If we panicked
        // here (e.g. host OOM in the underlying frame allocator) the
        // current process would still be intact — same pre-existing
        // limitation as `Process::create`, not something execve adds.
        let mut new_as = AddressSpace::new();
        for seg in &elf_info.segments[..elf_info.seg_count] {
            new_as.map_segment(seg, elf_data);
        }
        let stack_top = new_as.map_user_stack();
        // The replacement image is a from-scratch address space, so it
        // needs its own sigreturn trampoline mapping (fork/clone inherit
        // theirs, but execve builds a fresh AddressSpace).
        new_as.map_sigreturn_trampoline();
        let new_rsp = setup_stack(
            stack_top,
            new_as.pml4_phys.as_u64(),
            DIRECT_MAP_BASE,
            argv,
            envp,
        );

        // Phase 1 complete; the read buffer is no longer needed.
        free_elf_buf(buf_phys);

        // ── Phase 2: point of no return ────────────────────────────────
        let proc_ptr = crate::syscall::current_process();
        debug_assert!(!proc_ptr.is_null(), "do_execve: no current process");
        // SAFETY: current_process() returns the live PCB for this thread.
        let proc = unsafe { &mut *proc_ptr };

        // Before CLONE_VM, `proc.address_space` was an `AddressSpace` owned
        // by value and this whole block just swapped it in place. It's now
        // a `*mut AddressSpace` (see `AddressSpace::alloc_shared`), so the
        // old one must be *dropped* (refcounted) rather than unconditionally
        // torn down -- a CLONE_VM sibling thread could in principle still
        // be pointing at it (see the drop_ref call below).
        let old_as_ptr = proc.address_space;
        let this_cpu = kernel_cpu::current_cpu_id();

        // Promote the freshly built replacement onto its own dedicated
        // frame -- see AddressSpace::alloc_shared's doc comment for why
        // this indirection exists at all.
        let new_as_ptr = AddressSpace::alloc_shared(new_as);
        proc.address_space = new_as_ptr;
        // activate_address_space performs the full switch in the
        // shootdown-safe order (mark_active(new) -> CR3 load ->
        // mark_inactive(old)) and updates the per-CPU CURRENT_AS slot the
        // scheduler's context-switch hook keys off -- leaving that slot
        // pointing at old_as_ptr across the drop_ref below would hand the
        // hook a dangling pointer on the next switch.
        // SAFETY: new_as_ptr was just built above and is now this process's
        // own, exclusively-owned address space; IF=0 per do_execve's
        // contract; old_as_ptr (this CPU's recorded space) stays live until
        // drop_ref below.
        unsafe { crate::process::activate_address_space(new_as_ptr, this_cpu) };
        // SAFETY: CR3 was just reloaded with a fresh PCID above; this is a
        // defensive flush in case a PCID generation ever wraps and a stale
        // mapping for a reused PCID is still resident (see module docs in
        // address_space.rs on PcidAllocator generations). This flush is
        // intentionally LOCAL-only (kernel_paging::flush_all_pcids, not
        // crate::shootdown::shootdown_page/an equivalent broadcast): new_as
        // was just built from scratch above and is definitely not yet
        // CLONE_VM-shared with any other CPU -- no other CPU's TLB could
        // possibly have a cached translation for it yet. See Part 3's
        // retrofit notes in address_space.rs for the call sites that DO
        // need a broadcast.
        unsafe { kernel_paging::flush_all_pcids() };

        // Drop this process's reference to its OLD address space. In the
        // overwhelmingly common (non-CLONE_VM) case this process was its
        // sole owner, so this unconditionally tears it down -- exactly like
        // the pre-CLONE_VM code always did (free_user_mappings + free the
        // PML4 frame), now additionally freeing the AddressSpace's own
        // dedicated frame (see AddressSpace::alloc_shared). Only now that
        // CR3 no longer references it is it safe to free the old PML4
        // frame itself -- free_user_mappings() only frees frames reachable
        // from its *user* half; the top-level PML4 frame is the one piece
        // AddressSpace never frees on its own.
        //
        // KNOWN LIMITATION: if a CLONE_VM sibling thread still exists under
        // the OLD address space, this checkpoint's execve does NOT tear
        // those sibling threads down first the way Linux's real execve
        // does -- drop_ref then just decrements, and the sibling's own
        // pointer remains valid (this process no longer references it, but
        // the sibling still does) until the sibling itself exits. See the
        // checkpoint summary's "Known limitations" for why full
        // thread-group teardown on exec is out of scope here.
        // SAFETY: old_as_ptr came from AddressSpace::alloc_shared and this
        // process held one of its references; CR3 no longer references it
        // as of the activate() call above.
        unsafe { crate::address_space::AddressSpace::drop_ref(old_as_ptr) };

        proc.user_rip = elf_info.entry;
        proc.user_rsp = new_rsp;
        proc.user_rflags = 0x202;
        proc.user_rax = 0;

        // POSIX: execve resets caught signals to their default disposition
        // (the old handlers point into the old, now-unmapped image) and
        // starts the new image with a clean slate — no pending signals, no
        // handler in flight. Ignored dispositions are also reset here
        // (this checkpoint has no SA_* flags to preserve them).
        *proc.sig_table.lock() = crate::signal::SigTable::new();
        *proc.sig_frame.lock() = None;
        proc.pending.bits.store(0, core::sync::atomic::Ordering::Release);

        // POSIX: execve() closes every fd marked O_CLOEXEC. Rumicos doesn't
        // implement O_CLOEXEC yet (see fd.rs), so for now every fd survives
        // exec unchanged — safe (nothing is closed that shouldn't be) but
        // not yet POSIX-correct.
        close_cloexec_fds(&mut proc.fd_table.lock());

        let thread = proc.thread;
        debug_assert!(!thread.is_null(), "do_execve: process has no thread");
        // SAFETY: thread is the live TCB for this same kernel thread; its
        // kstack_top never changes across exec (the stack itself is reused).
        debug_assert!(
            unsafe { (*thread).kstack_top % 16 == 0 },
            "kstack_top misaligned after exec"
        );

        // CR3 | PCID — identical construction to ring3_entry_rust, which
        // performs this exact transition for a brand-new process's first
        // run. We deliberately do not go through kernel_sched::schedule()/
        // switch_context() here; see the module-level docs above for why.
        // SAFETY: proc.address_space (== new_as_ptr above) is this
        // process's own, live AddressSpace.
        let cr3 = unsafe { (*proc.address_space).pml4_phys.as_u64() | (*proc.address_space).pcid as u64 };
        // SAFETY: cr3 names the address space we just activated, and
        // user_rip/user_rsp were validated while building new_as above.
        unsafe {
            crate::process::enter_user_mode(
                cr3,
                proc.user_rip,
                proc.user_rsp,
                proc.user_rflags,
                proc.user_rax,
            )
        }
    }

    #[cfg(not(target_os = "none"))]
    {
        let _ = (path, argv, envp);
        ENOENT
    }
}

/// Free the order-[`ELF_BUF_ORDER`] buffer allocated to read the ELF file.
/// Freed one 4 KiB frame at a time via the ordinary `free_frame` path
/// (rather than a single order-8 free) because `kernel-memory` only exposes
/// `free_frame`/`alloc_order` publicly; doing so is correct — `NumaBuddy`'s
/// allocation/free paths operate per-frame internally regardless of the
/// order an allocation was originally requested at.
#[cfg(target_os = "none")]
fn free_elf_buf(buf_phys: kernel_memory::PhysAddr) {
    let base = buf_phys.as_u64();
    let frames: u64 = 1u64 << ELF_BUF_ORDER;
    for i in 0..frames {
        // SAFETY: every 4 KiB sub-frame of this freshly allocated,
        // exclusively-owned block is unmapped and referenced nowhere else.
        unsafe { kernel_memory::free_frame(kernel_memory::PhysAddr::new(base + i * 4096)) };
    }
}

/// POSIX: execve() closes every fd marked `O_CLOEXEC`.
///
/// TODO: once `open()`/`FdEntry` track an O_CLOEXEC flag, iterate `fd_table`
/// here and close every flagged entry. For now this is an intentional no-op:
/// every fd survives exec unchanged, which is safe but not POSIX-correct.
#[cfg(target_os = "none")]
fn close_cloexec_fds(_fd_table: &mut FdTable) {}

/// Walk `pml4_phys`'s page tables (reachable through the direct map at
/// `dmap`) to translate a virtual address to a physical one, *without*
/// requiring that page table to be the active `CR3`. This is what lets
/// [`setup_stack`] write argv/envp into the new address space before Phase 2
/// installs it.
///
/// `dmap` is threaded through explicitly (rather than hard-coding
/// [`DIRECT_MAP_BASE`]) so this can be exercised on the host with a
/// synthetic page table hierarchy backed by ordinary heap memory — see the
/// tests below. That's also why this (and the three helpers below) are
/// compiled for the real kernel target *or* under `cfg(test)`, rather than
/// `target_os = "none"` alone: on a plain host `cargo check`/`clippy` pass
/// (no `--test`, default host target) they would otherwise be unreachable
/// dead code.
#[cfg(any(target_os = "none", test))]
fn virt_to_phys_via_pagewalk(pml4_phys: u64, dmap: u64, virt: u64) -> Option<u64> {
    let walker = kernel_paging::table::PageTableWalker::new(
        kernel_paging::PhysAddr::new(pml4_phys),
        kernel_paging::VirtAddr::new(dmap),
    );
    // SAFETY: callers only ever pass a `pml4_phys` whose hierarchy is fully
    // populated (by `map_segment`/`map_user_stack`, or by a test) for the
    // region being walked, and `dmap` is a valid direct-map alias for it.
    unsafe { walker.translate(kernel_paging::VirtAddr::new(virt)) }.map(|p| p.as_u64())
}

/// Copy `data` into the address space named by `pml4_phys`, starting at user
/// virtual address `virt`, before that address space is necessarily the
/// active `CR3`. Handles copies that cross a 4 KiB page boundary.
#[cfg(any(target_os = "none", test))]
fn write_bytes_to_user(pml4_phys: u64, dmap: u64, virt: u64, data: &[u8]) {
    let mut off = 0usize;
    while off < data.len() {
        let cur_virt = virt + off as u64;
        let page_off = (cur_virt & 0xFFF) as usize;
        let chunk = (4096 - page_off).min(data.len() - off);
        let phys = virt_to_phys_via_pagewalk(pml4_phys, dmap, cur_virt)
            .expect("write_bytes_to_user: destination not mapped in new address space");
        let dst = (dmap + phys) as *mut u8;
        // SAFETY: phys was just resolved by walking this address space's own
        // (freshly built, writable, present) page tables; dst is reachable
        // through the direct map at `dmap`, and [off, off+chunk) stays
        // within `data`.
        unsafe { core::ptr::copy_nonoverlapping(data[off..off + chunk].as_ptr(), dst, chunk) };
        off += chunk;
    }
}

#[cfg(any(target_os = "none", test))]
fn write_u64_to_user(pml4_phys: u64, dmap: u64, virt: u64, val: u64) {
    write_bytes_to_user(pml4_phys, dmap, virt, &val.to_le_bytes());
}

/// Lay out the initial SysV AMD64 user stack below `stack_top`:
///
/// ```text
///   [rsp]            argc
///   [rsp+8]          argv[0]
///   ...
///   [rsp+8n]         argv[argc] = NULL
///   [rsp+8(n+1)]     envp[0]
///   ...
///   [rsp+8m]         envp[envc] = NULL
///   [rsp+8(m+1)]     AT_NULL auxv terminator (a_type, a_val) = (0, 0)
///   [above]          argv/envp string data, NUL-terminated
/// ```
///
/// Returns the final `rsp`, pointing at `argc` — the value to load into the
/// new image's `RSP` on entry.
///
/// All writes go through [`write_bytes_to_user`] via the direct map, since
/// `pml4_phys` need not be (and during Phase 1, never is) the active `CR3`.
#[cfg(any(target_os = "none", test))]
fn setup_stack(stack_top: u64, pml4_phys: u64, dmap: u64, argv: &[&str], envp: &[&str]) -> u64 {
    debug_assert!(argv.len() <= MAX_ARGV && envp.len() <= MAX_ENVP);

    // Pack argv/envp string data downward from stack_top, recording each
    // string's resulting user VA so the pointer arrays below can refer to it.
    let mut string_ptrs = [0u64; MAX_ARGS_TOTAL];
    let mut str_rsp = stack_top;
    for (i, s) in argv.iter().chain(envp.iter()).enumerate() {
        let bytes = s.as_bytes();
        str_rsp -= bytes.len() as u64 + 1; // +1 for the NUL terminator
        write_bytes_to_user(pml4_phys, dmap, str_rsp, bytes);
        write_bytes_to_user(pml4_phys, dmap, str_rsp + bytes.len() as u64, &[0u8]);
        string_ptrs[i] = str_rsp;
    }

    let argc = argv.len() as u64;
    let envc = envp.len() as u64;
    // argc(1) + argv pointers + NULL(1) + envp pointers + NULL(1) + AT_NULL(2)
    let block_qwords = 5 + argc + envc;
    let block_bytes = block_qwords * 8;

    // Align so that the *final* rsp (after every push below) lands on a
    // 16-byte boundary, as the SysV AMD64 ABI requires at process entry.
    let mut rsp = str_rsp & !0xF;
    if rsp % 16 != block_bytes % 16 {
        rsp -= 8;
    }

    // AT_NULL auxv terminator: push a_val then a_type, so a_type ends up at
    // the lower address — matching Elf64_auxv_t's {a_type, a_val} layout.
    rsp -= 8;
    write_u64_to_user(pml4_phys, dmap, rsp, 0); // a_val
    rsp -= 8;
    write_u64_to_user(pml4_phys, dmap, rsp, 0); // a_type

    rsp -= 8;
    write_u64_to_user(pml4_phys, dmap, rsp, 0); // envp NULL terminator
    for i in (0..envp.len()).rev() {
        rsp -= 8;
        write_u64_to_user(pml4_phys, dmap, rsp, string_ptrs[argv.len() + i]);
    }

    rsp -= 8;
    write_u64_to_user(pml4_phys, dmap, rsp, 0); // argv NULL terminator
    for i in (0..argv.len()).rev() {
        rsp -= 8;
        write_u64_to_user(pml4_phys, dmap, rsp, string_ptrs[i]);
    }

    rsp -= 8;
    write_u64_to_user(pml4_phys, dmap, rsp, argc);

    rsp
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel_paging::table::{PageTable, PageTableWalker};
    use kernel_paging::{PageFlags, PageTableEntry, PhysAddr as PPhys, VirtAddr as PVirt};
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    /// A single 4 KiB, 4 KiB-aligned host allocation, usable either as a
    /// synthetic [`PageTable`] (via `table()`) or as a flat data page (via
    /// `bytes()`). Lets host tests build a real, walkable page-table
    /// hierarchy out of ordinary heap memory — mirroring the pattern already
    /// used in `kernel-memory`'s own buddy-allocator tests.
    struct RawPage {
        ptr: *mut u8,
        layout: Layout,
    }

    impl RawPage {
        fn new() -> Self {
            let layout = Layout::from_size_align(4096, 4096).expect("valid layout");
            // SAFETY: layout is non-zero-sized and properly aligned.
            let ptr = unsafe { alloc_zeroed(layout) };
            assert!(!ptr.is_null(), "test backing allocation failed");
            Self { ptr, layout }
        }

        fn phys(&self) -> PPhys {
            PPhys::new(self.ptr as u64)
        }

        fn table(&mut self) -> &mut PageTable {
            // SAFETY: backing storage is exactly one zeroed, 4 KiB-aligned
            // page — the same layout `PageTable` itself requires.
            unsafe { &mut *(self.ptr as *mut PageTable) }
        }

        fn bytes(&self) -> &[u8] {
            // SAFETY: ptr is valid for 4096 bytes for the lifetime of self.
            unsafe { core::slice::from_raw_parts(self.ptr, 4096) }
        }
    }

    impl Drop for RawPage {
        fn drop(&mut self) {
            // SAFETY: ptr was allocated with this exact layout in `new()`
            // and is dropped exactly once here.
            unsafe { dealloc(self.ptr, self.layout) };
        }
    }

    /// Build a synthetic PML4 -> PDPT -> PD -> PT chain mapping exactly one
    /// 4 KiB page at virtual address `0x1000` to `leaf`'s own backing page,
    /// with `dmap = 0` (so the synthetic "physical" addresses used here are
    /// literally the host pointers backing each level).
    struct SyntheticMapping {
        pml4: RawPage,
        _pdpt: RawPage,
        _pd: RawPage,
        _pt: RawPage,
        leaf: RawPage,
    }

    impl SyntheticMapping {
        fn new() -> Self {
            let mut pml4 = RawPage::new();
            let mut pdpt = RawPage::new();
            let mut pd = RawPage::new();
            let mut pt = RawPage::new();
            let leaf = RawPage::new();

            let rw = PageFlags::new().with_present().with_writable();
            pml4.table()
                .set(0, PageTableEntry::new_page(pdpt.phys(), rw));
            pdpt.table().set(0, PageTableEntry::new_page(pd.phys(), rw));
            pd.table().set(0, PageTableEntry::new_page(pt.phys(), rw));
            pt.table().set(1, PageTableEntry::new_page(leaf.phys(), rw));

            Self {
                pml4,
                _pdpt: pdpt,
                _pd: pd,
                _pt: pt,
                leaf,
            }
        }

        fn pml4_phys(&self) -> u64 {
            self.pml4.phys().as_u64()
        }

        /// Read a little-endian u64 at virtual address `virt`, which must
        /// fall within the single mapped page `[0x1000, 0x2000)`.
        fn read_u64(&self, virt: u64) -> u64 {
            let off = (virt - 0x1000) as usize;
            let b = self.leaf.bytes();
            u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
        }
    }

    #[test]
    fn virt_to_phys_pagewalk_resolves_4k_mapping() {
        let mapping = SyntheticMapping::new();
        // virt 0x1000 -> pml4_idx=0, pdpt_idx=0, pd_idx=0, pt_idx=1, offset=0
        // (pt index 1 is exactly where SyntheticMapping wired the leaf page).
        let result = virt_to_phys_via_pagewalk(mapping.pml4_phys(), 0, 0x1000);
        assert_eq!(result, Some(mapping.leaf.phys().as_u64()));
    }

    #[test]
    fn virt_to_phys_pagewalk_returns_none_when_not_present() {
        let mapping = SyntheticMapping::new();
        // pt index 2 (virt 0x2000) was never populated.
        let result = virt_to_phys_via_pagewalk(mapping.pml4_phys(), 0, 0x2000);
        assert_eq!(result, None);
    }

    #[test]
    fn virt_to_phys_pagewalk_matches_raw_walker_for_sanity() {
        // Cross-check our wrapper against PageTableWalker directly so a
        // future refactor of the wrapper can't silently drift.
        let mapping = SyntheticMapping::new();
        let walker = PageTableWalker::new(PPhys::new(mapping.pml4_phys()), PVirt::new(0));
        let expected = unsafe { walker.translate(PVirt::new(0x1000)) };
        assert_eq!(
            virt_to_phys_via_pagewalk(mapping.pml4_phys(), 0, 0x1000),
            expected.map(|p| p.as_u64())
        );
    }

    #[test]
    fn setup_stack_argc_matches_argv_len() {
        let mapping = SyntheticMapping::new();
        let argv = ["a", "bb", "ccc"];
        let envp: [&str; 0] = [];
        let rsp = setup_stack(0x1FF8, mapping.pml4_phys(), 0, &argv, &envp);
        assert_eq!(mapping.read_u64(rsp), argv.len() as u64);
    }

    #[test]
    fn setup_stack_places_argc_at_returned_rsp() {
        let mapping = SyntheticMapping::new();
        let argv = ["init"];
        let envp: [&str; 0] = [];
        let rsp = setup_stack(0x1FF8, mapping.pml4_phys(), 0, &argv, &envp);
        // argc lives at exactly the returned rsp.
        assert_eq!(mapping.read_u64(rsp), 1);
    }

    #[test]
    fn setup_stack_writes_argv_and_envp_null_terminators() {
        let mapping = SyntheticMapping::new();
        let argv = ["init"];
        let envp: [&str; 0] = [];
        let rsp = setup_stack(0x1FF8, mapping.pml4_phys(), 0, &argv, &envp);

        let argv0_ptr = mapping.read_u64(rsp + 8);
        assert_ne!(argv0_ptr, 0, "argv[0] pointer must be non-NULL");
        assert_eq!(mapping.read_u64(rsp + 16), 0, "argv NULL terminator");
        assert_eq!(mapping.read_u64(rsp + 24), 0, "envp NULL terminator");
    }

    #[test]
    fn setup_stack_envp_pointers_are_distinct_and_terminated() {
        let mapping = SyntheticMapping::new();
        let argv = ["init"];
        let envp = ["HOME=/", "PATH=/bin"];
        let rsp = setup_stack(0x1FF8, mapping.pml4_phys(), 0, &argv, &envp);

        // Layout: argc, argv[0], argvNULL, envp[0], envp[1], envpNULL, ...
        let envp0 = mapping.read_u64(rsp + 24);
        let envp1 = mapping.read_u64(rsp + 32);
        let envp_null = mapping.read_u64(rsp + 40);
        assert_ne!(envp0, 0);
        assert_ne!(envp1, 0);
        assert_ne!(
            envp0, envp1,
            "distinct envp strings must get distinct pointers"
        );
        assert_eq!(envp_null, 0);
    }

    #[test]
    fn setup_stack_result_is_16_byte_aligned_with_no_args() {
        // argc + envc == 0 exercises the "needs an extra 8-byte nudge"
        // branch of the alignment adjustment (5 qwords -> 40 bytes, which is
        // not itself a multiple of 16).
        let mapping = SyntheticMapping::new();
        let argv: [&str; 0] = [];
        let envp: [&str; 0] = [];
        let rsp = setup_stack(0x1FF8, mapping.pml4_phys(), 0, &argv, &envp);
        assert_eq!(rsp % 16, 0);
    }

    #[test]
    fn setup_stack_result_is_16_byte_aligned_with_args() {
        let mapping = SyntheticMapping::new();
        let argv = ["init", "extra"];
        let envp = ["X=1"];
        let rsp = setup_stack(0x1FF8, mapping.pml4_phys(), 0, &argv, &envp);
        assert_eq!(rsp % 16, 0);
    }

    #[test]
    fn do_execve_rejects_too_many_argv() {
        let too_many = ["x"; MAX_ARGV + 1];
        // SAFETY: host build short-circuits before touching any hardware
        // state; the bounds check happens before that.
        let ret = unsafe { do_execve("/doesnotmatter", &too_many, &[]) };
        assert_eq!(ret, E2BIG);
    }

    #[test]
    fn do_execve_rejects_too_many_envp() {
        let too_many = ["x"; MAX_ENVP + 1];
        // SAFETY: see above.
        let ret = unsafe { do_execve("/doesnotmatter", &[], &too_many) };
        assert_eq!(ret, E2BIG);
    }

    #[cfg(not(target_os = "none"))]
    #[test]
    fn do_execve_on_host_returns_enoent_without_touching_hardware() {
        // On host there is no VFS/paging/thread to exercise; do_execve must
        // short-circuit cleanly rather than touch any hardware state.
        // SAFETY: host build never reaches the target_os="none" branch.
        let ret = unsafe { do_execve("/init2.elf", &["init2.elf"], &[]) };
        assert_eq!(ret, ENOENT);
    }
}
