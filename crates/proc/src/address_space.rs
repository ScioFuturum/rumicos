use crate::elf::LoadSegment;
#[cfg(target_os = "none")]
use crate::elf::{PF_W, PF_X};
use crate::shootdown::ActiveCpuMask;
use crate::ustack::USTACK_TOP;
#[cfg(target_os = "none")]
use crate::ustack::{USTACK_PAGES, USTACK_SIZE};
use crate::vma::{MAX_VMAS, MMAP_BASE, MMAP_REGION_END, Vma};
use core::sync::atomic::{AtomicU32, Ordering};
#[cfg(target_os = "none")]
use kernel_memory::PcidAllocator;
use kernel_paging::PageFlags;
use kernel_paging::PhysAddr;
#[cfg(target_os = "none")]
use kernel_paging::{PageTableEntry, VirtAddr};

// Unlike before CLONE_VM, this is no longer only needed from
// `#[cfg(target_os = "none")]` code: `AddressSpace::alloc_shared`/
// `drop_ref` (below) need it unconditionally, mirroring
// `Process::create`'s own unconditional (if host-unreachable-in-practice)
// use of the same literal.
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;
const PAGE_SIZE: u64 = 4096;
#[cfg(target_os = "none")]
const USER_PML4_ENTRIES: usize = 256;

#[cfg(target_os = "none")]
static PCID_ALLOCATOR: PcidAllocator<64> = PcidAllocator::new();

pub struct AddressSpace {
    pub pml4_phys: PhysAddr,
    pub pcid: u16,
    pub vmas: [Option<Vma>; MAX_VMAS],
    pub mmap_next: u64,
    /// Shared-ownership refcount. `1` for an ordinary (non-`CLONE_VM`)
    /// address space; `>1` while more than one `Process`/`Thread` points
    /// at this exact same `AddressSpace` via `crate::clone::sys_clone`'s
    /// `CLONE_VM` path. See `inc_ref`/`dec_ref`/`alloc_shared`/`drop_ref`.
    refcount: AtomicU32,
    /// Which CPUs currently have this `AddressSpace`'s `pml4_phys` loaded
    /// in `CR3` right now, for cross-CPU TLB shootdown (`crate::shootdown`).
    pub active_cpus: ActiveCpuMask,
}

impl AddressSpace {
    pub fn new() -> Self {
        #[cfg(target_os = "none")]
        {
            let pml4_frame = kernel_memory::alloc_frame();
            let pml4_phys = PhysAddr::new(pml4_frame.as_u64());
            let dst = direct_map_table_mut(pml4_phys);
            unsafe {
                // SAFETY: pml4_phys was freshly allocated and is visible through the direct map.
                (*dst).zero();
            }

            let src_phys = PhysAddr::new(kernel_paging::current_pml4_phys());
            let src = direct_map_table(src_phys);
            for index in USER_PML4_ENTRIES..kernel_paging::PageTable::ENTRIES {
                let entry = unsafe {
                    // SAFETY: src points at the currently active PML4 via the direct map.
                    (*src).get(index)
                };
                unsafe {
                    // SAFETY: dst points at the freshly allocated PML4 via the direct map.
                    (*dst).set(index, entry);
                }
            }

            // PCID 0 when CR4.PCIDE is off (e.g. QEMU TCG, which has no
            // PCID support): with PCIDE disabled, CR3 bits 0-11 are PWT/PCD
            // and ignored bits, so a nonzero "PCID" would silently toggle
            // page-walk caching attributes instead of tagging the TLB.
            let pcid = if kernel_paging::tlb::pcid_enabled() {
                PCID_ALLOCATOR.alloc().expect("PCID space exhausted").id
            } else {
                0
            };
            Self {
                pml4_phys,
                pcid,
                vmas: [const { None }; MAX_VMAS],
                mmap_next: MMAP_BASE,
                refcount: AtomicU32::new(1),
                active_cpus: ActiveCpuMask::new(),
            }
        }

        #[cfg(not(target_os = "none"))]
        {
            Self {
                pml4_phys: PhysAddr::new(0),
                pcid: 0,
                vmas: [const { None }; MAX_VMAS],
                mmap_next: MMAP_BASE,
                refcount: AtomicU32::new(1),
                active_cpus: ActiveCpuMask::new(),
            }
        }
    }

    /// Increment the shared-ownership refcount. Call when a new
    /// `Process`/`Thread` begins pointing at this exact same
    /// `AddressSpace` (`CLONE_VM`) rather than getting a private
    /// `fork_cow()` copy.
    pub fn inc_ref(&self) {
        self.refcount.fetch_add(1, Ordering::AcqRel);
    }

    /// Decrement the shared-ownership refcount and report whether this was
    /// the last owner (mirrors `crate::cow::cow_release`'s combined
    /// decrement-and-report step: a separate refcount-read followed by a
    /// later "am I last?" check would be a TOCTOU race between two
    /// CLONE_VM siblings exiting on different CPUs at once). The caller
    /// must tear down (see `drop_ref`) if and only if this returns `true`.
    #[must_use]
    pub fn dec_ref(&self) -> bool {
        self.refcount.fetch_sub(1, Ordering::AcqRel) == 1
    }

    #[cfg(test)]
    pub(crate) fn refcount_for_test(&self) -> u32 {
        self.refcount.load(Ordering::Acquire)
    }

    /// Move a freshly built `AddressSpace` value onto its own dedicated
    /// physical frame and return a stable pointer to it — the handle
    /// `Process::address_space` and `crate::clone::sys_clone` share.
    ///
    /// Before `CLONE_VM`, `AddressSpace` lived inline inside `Process`'s
    /// own frame (one owner, one `Process`, no pointer needed). `CLONE_VM`
    /// requires more than one `Process` to point at the exact same live
    /// `AddressSpace`, which is only possible once it has an address of
    /// its own, independent of any single `Process`'s storage — this is
    /// the single most invasive structural change in this checkpoint (see
    /// the checkpoint summary for the full rationale).
    pub fn alloc_shared(value: AddressSpace) -> *mut AddressSpace {
        let frame = kernel_memory::alloc_frame();
        let ptr = (DIRECT_MAP_BASE + frame.as_u64()) as *mut AddressSpace;
        // SAFETY: frame was freshly allocated and is visible through the
        // direct map; nothing else can be aliasing it yet.
        unsafe { ptr.write(value) };
        ptr
    }

    /// Drop one reference to the `AddressSpace` at `ptr`. If this was the
    /// last reference, tears down its user-space page tables, frees the
    /// PML4 frame, and frees `ptr`'s own backing frame. Returns `true` iff
    /// it was torn down.
    ///
    /// # Safety
    /// `ptr` must have come from `alloc_shared`, the caller must hold one
    /// of the references being dropped, and `ptr` must not be
    /// dereferenced by anyone (including the caller) after this call
    /// returns `true`.
    pub unsafe fn drop_ref(ptr: *mut AddressSpace) -> bool {
        // SAFETY: caller guarantees ptr is live and alloc_shared-allocated.
        let is_last = unsafe { (*ptr).dec_ref() };
        if is_last {
            // SAFETY: refcount just reached zero, so per this function's
            // own precondition no other owner can be concurrently
            // touching this AddressSpace -- exclusive access is safe.
            let pml4_phys = unsafe { (*ptr).pml4_phys.as_u64() };
            unsafe { (*ptr).free_user_mappings() };
            unsafe {
                kernel_memory::free_frame(kernel_memory::PhysAddr::new(pml4_phys));
            }
            let own_frame = (ptr as u64) - DIRECT_MAP_BASE;
            // SAFETY: ptr is no longer live/aliased past this point.
            unsafe {
                kernel_memory::free_frame(kernel_memory::PhysAddr::new(own_frame));
            }
        }
        is_last
    }

    pub fn map_segment(&mut self, seg: &LoadSegment, elf_data: &[u8]) {
        #[cfg(target_os = "none")]
        {
            let flags = user_flags_from_elf(seg.flags);
            let map_end = align_up(seg.vaddr.saturating_add(seg.memsz), PAGE_SIZE);
            let mut virt = seg.vaddr;
            while virt < map_end {
                let frame = kernel_memory::alloc_frame();
                let dst = (DIRECT_MAP_BASE + frame.as_u64()) as *mut u8;
                unsafe {
                    // SAFETY: frame was freshly allocated and is visible through the direct map.
                    core::ptr::write_bytes(dst, 0, PAGE_SIZE as usize);
                }

                let page_start = virt - seg.vaddr;
                if page_start < seg.filesz {
                    let copy_len = core::cmp::min(PAGE_SIZE, seg.filesz - page_start) as usize;
                    let src_off = (seg.data_off + page_start) as usize;
                    let src = elf_data[src_off..src_off + copy_len].as_ptr();
                    unsafe {
                        // SAFETY: parser validated file bounds; dst points into a zeroed frame.
                        core::ptr::copy_nonoverlapping(src, dst, copy_len);
                    }
                }

                unsafe {
                    // SAFETY: self owns the PML4 and the freshly allocated frame.
                    self.map_page(VirtAddr::new(virt), PhysAddr::new(frame.as_u64()), flags);
                }
                virt += PAGE_SIZE;
            }
        }

        #[cfg(not(target_os = "none"))]
        {
            let _ = (seg, elf_data);
        }
    }

    pub fn map_user_stack(&mut self) -> u64 {
        #[cfg(target_os = "none")]
        {
            let flags = PageFlags::new()
                .with_present()
                .with_writable()
                .with_user_accessible()
                .with_no_execute();
            let stack_base = USTACK_TOP - USTACK_SIZE;
            for page in 0..USTACK_PAGES {
                let frame = kernel_memory::alloc_frame();
                let dst = (DIRECT_MAP_BASE + frame.as_u64()) as *mut u8;
                unsafe {
                    // SAFETY: frame was freshly allocated and is visible through the direct map.
                    core::ptr::write_bytes(dst, 0, PAGE_SIZE as usize);
                    self.map_page(
                        VirtAddr::new(stack_base + page * PAGE_SIZE),
                        PhysAddr::new(frame.as_u64()),
                        flags,
                    );
                }
            }
        }

        USTACK_TOP - 8
    }

    /// Map the one-page sigreturn trampoline (`crate::signal`) read+exec
    /// (no write, no NX) at its fixed user VA. Called once per from-scratch
    /// image (`Process::create`, `execve` Phase 1); `fork` inherits the
    /// mapping through its CoW copy and `CLONE_VM` through the shared
    /// address space, so neither re-maps it.
    pub fn map_sigreturn_trampoline(&mut self) {
        #[cfg(target_os = "none")]
        {
            let frame = kernel_memory::alloc_frame();
            let dst = (DIRECT_MAP_BASE + frame.as_u64()) as *mut u8;
            unsafe {
                // SAFETY: freshly allocated frame, visible through the direct map.
                core::ptr::write_bytes(dst, 0, PAGE_SIZE as usize);
                core::ptr::copy_nonoverlapping(
                    crate::signal::SIGRETURN_TRAMPOLINE_CODE.as_ptr(),
                    dst,
                    crate::signal::SIGRETURN_TRAMPOLINE_CODE.len(),
                );
            }
            // Present + user + executable, NOT writable: user code must be
            // able to execute the trampoline but never modify it.
            let flags = PageFlags::new().with_present().with_user_accessible();
            unsafe {
                // SAFETY: self owns its PML4 and the freshly allocated frame.
                self.map_page(
                    VirtAddr::new(crate::signal::SIGRETURN_TRAMPOLINE_VA),
                    PhysAddr::new(frame.as_u64()),
                    flags,
                );
            }
        }
    }

    pub fn activate(&self) {
        #[cfg(target_os = "none")]
        unsafe {
            // SAFETY: the PML4 contains the shared kernel half and private user mappings.
            kernel_paging::tlb::install_page_table(kernel_paging::make_cr3(
                self.pml4_phys,
                self.pcid,
                false,
            ));
        }
    }

    pub fn free_user_mappings(&mut self) {
        #[cfg(target_os = "none")]
        unsafe {
            // SAFETY: self owns user half of this PML4; kernel half is intentionally preserved.
            free_user_half(self.pml4_phys);
        }
    }

    pub fn find_vma(&self, addr: u64) -> Option<Vma> {
        self.vmas
            .iter()
            .flatten()
            .copied()
            .find(|vma| vma.contains(addr))
    }

    /// Reserve `len` bytes in the mmap region backed by `backing`. Does NOT
    /// fault in any physical frames — pages are faulted in lazily by
    /// `resolve_file_fault` (which despite the name handles BOTH
    /// `Backing::Anonymous` and `Backing::File`, per `Vma::backing`).
    pub fn mmap(&mut self, len: u64, flags: PageFlags, backing: crate::vma::Backing) -> Option<u64> {
        if len == 0 || len & (PAGE_SIZE - 1) != 0 {
            return None;
        }
        let slot = self.vmas.iter().position(Option::is_none)?;
        let mut start = crate::vma::align_up_page(self.mmap_next)?;
        loop {
            let end = start.checked_add(len)?;
            if end > MMAP_REGION_END {
                return None;
            }
            if !self.vma_overlaps(start, end) {
                // The File payload is written FIELD BY FIELD through raw
                // primitive stores rather than by copying a whole `Backing`
                // value into the slot. rustc 1.97.0 compiles the plain
                // `self.vmas[slot] = Some(Vma { backing, .. })` aggregate
                // copy into byte-wise moves of ONLY the even-offset bytes of
                // `Backing::File::vnode_ptr` (the odd bytes are treated as
                // padding and keep their previous 0x02 Option-tag values),
                // so every file mmap later dereferenced a shredded pointer.
                // The miscompile survived the default layout, repr(C, u8),
                // -O2, -O3+LTO, and write_volatile of the whole Option;
                // field reads are compiled correctly, so extracting the
                // primitives and storing them at repr(C, u8)-guaranteed
                // offsets (tag@0, vnode_ptr@8, file_offset@16, shared@24) is
                // the one form that survives. Caught with a QEMU gdbstub
                // write-watchpoint on the first real boot; see
                // [[first-boot-debugging]].
                self.vmas[slot] = Some(Vma {
                    start,
                    end,
                    flags,
                    // Dataless variant first: this copy only moves the tag
                    // byte, which the buggy aggregate copy handles fine.
                    backing: crate::vma::Backing::Anonymous,
                });
                if let crate::vma::Backing::File {
                    vnode_ptr,
                    file_offset,
                    shared,
                } = backing
                {
                    let vma_ref = self.vmas[slot].as_mut().expect("just stored Some above");
                    let b = (&mut vma_ref.backing) as *mut crate::vma::Backing as *mut u8;
                    // SAFETY: Backing is repr(C, u8) — tag@0, then the File
                    // payload as a C struct (usize@8, u64@16, bool@24); the
                    // slot is exclusively borrowed here.
                    unsafe {
                        b.add(8).cast::<usize>().write_volatile(vnode_ptr);
                        b.add(16).cast::<u64>().write_volatile(file_offset);
                        b.add(24).write_volatile(shared as u8);
                        // Tag written last so the slot never reads as File
                        // with a half-populated payload.
                        b.write_volatile(1);
                    }
                }
                self.mmap_next = end;
                return Some(start);
            }
            start = end;
        }
    }

    /// Convenience wrapper over [`mmap`](Self::mmap) for the common
    /// anonymous case — kept as a thin wrapper (rather than removed
    /// outright) since it's still the simplest entry point for the many
    /// call sites (tests, non-file-backed kernel-internal mappings) that
    /// never need a `Backing` at all.
    pub fn mmap_anon(&mut self, len: u64, flags: PageFlags) -> Option<u64> {
        self.mmap(len, flags, crate::vma::Backing::Anonymous)
    }

    pub fn munmap(&mut self, addr: u64, len: u64) -> bool {
        let end = match addr.checked_add(len) {
            Some(end) => end,
            None => return false,
        };
        let Some(slot) = self
            .vmas
            .iter()
            .position(|vma| matches!(vma, Some(vma) if vma.start == addr && vma.end == end))
        else {
            return false;
        };
        // Vma is Copy — grab it before clearing the slot below.
        let vma = self.vmas[slot].expect("slot located by position() above");
        #[cfg(not(target_os = "none"))]
        let _ = vma;

        #[cfg(target_os = "none")]
        unsafe {
            match vma.backing {
                crate::vma::Backing::Anonymous => {
                    // SAFETY: the VMA belongs to this address space and the range is page-aligned.
                    self.unmap_pages(addr, end);
                }
                crate::vma::Backing::File {
                    vnode_ptr, shared, ..
                } => {
                    if shared {
                        // SAFETY: vnode_ptr came from a live, refcounted
                        // FdEntry at mmap time and has been inc_ref'd since.
                        let _ = crate::syscall::page_cache_writeback(vnode_ptr);
                    }
                    // SAFETY: the VMA belongs to this address space and the range is page-aligned.
                    self.unmap_file_pages(addr, end, shared);
                    // SAFETY: vnode_ptr valid per the same contract as above.
                    crate::syscall::vnode_dec_ref(vnode_ptr);
                }
            }
        }
        self.vmas[slot] = None;
        true
    }

    fn vma_overlaps(&self, start: u64, end: u64) -> bool {
        self.vmas
            .iter()
            .flatten()
            .any(|vma| start < vma.end && vma.start < end)
    }

    pub fn fork_cow(&mut self) -> AddressSpace {
        let mut child = AddressSpace::new();
        child.vmas = self.vmas;
        child.mmap_next = self.mmap_next;

        #[cfg(target_os = "none")]
        unsafe {
            // SAFETY: parent and child address spaces are exclusively borrowed.
            self.clone_user_half_cow(&mut child);
        }
        child
    }

    pub fn resolve_cow_fault(&mut self, addr: u64) -> bool {
        #[cfg(target_os = "none")]
        unsafe {
            // SAFETY: page-fault handler has exclusive access to the running process.
            self.resolve_cow_fault_inner(addr)
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = addr;
            false
        }
    }

    /// Resolve a not-present fault by checking the faulting process's VMA
    /// list. Handles both `Backing::Anonymous` (demand-zero) and
    /// `Backing::File` (demand-fill from the page cache) — despite the
    /// name (kept from the previous checkpoint's `resolve_anon_fault`,
    /// generalized here rather than introducing a second, near-duplicate
    /// entry point), this is the single not-present-fault resolver for
    /// every VMA-backed page.
    pub fn resolve_file_fault(&mut self, addr: u64) -> bool {
        let page = addr & !(PAGE_SIZE - 1);
        let Some(vma) = self.find_vma(page) else {
            return false;
        };
        #[cfg(not(target_os = "none"))]
        let _ = vma;

        #[cfg(target_os = "none")]
        unsafe {
            let page_offset_in_vma = page - vma.start;
            let (frame, writable_now, mark_cow) = match vma.backing {
                crate::vma::Backing::Anonymous => {
                    let frame = kernel_memory::alloc_frame();
                    let dst = (DIRECT_MAP_BASE + frame.as_u64()) as *mut u8;
                    // SAFETY: freshly allocated frame is visible through the direct map.
                    core::ptr::write_bytes(dst, 0, PAGE_SIZE as usize);
                    (frame.as_u64(), true, false)
                }
                crate::vma::Backing::File {
                    vnode_ptr,
                    file_offset,
                    shared,
                } => {
                    // A VNode pointer is always a direct-map VA; anything
                    // else means this VMA was corrupted after mmap stored it.
                    assert!(
                        vnode_ptr >= 0xffff_8000_0000_0000usize,
                        "resolve_file_fault: garbage vnode_ptr {:#x} for page {:#x}",
                        vnode_ptr,
                        page
                    );
                    let file_off = file_offset + page_offset_in_vma;
                    // SAFETY: vnode_ptr came from a live, refcounted
                    // FdEntry at mmap time; direct map is live.
                    let Some(frame) = crate::syscall::page_cache_get_or_fill(vnode_ptr, file_off)
                    else {
                        return false; // unrecoverable read error -> SIGSEGV
                    };
                    if shared {
                        // MAP_SHARED: map the cache frame DIRECTLY, never
                        // CoW — writes must be visible to every other
                        // mapper of this same file region. Any writable
                        // mapping of a MAP_SHARED page is treated as
                        // dirty-on-touch (see page_cache_mark_dirty's own
                        // call site below for why this simplification is
                        // accepted for this checkpoint rather than only
                        // marking dirty on an observed PF_WRITE fault).
                        let writable = crate::vma::flags_writable(vma.flags);
                        if writable {
                            crate::syscall::page_cache_mark_dirty(vnode_ptr, file_off);
                        }
                        (frame, writable, false)
                    } else {
                        // MAP_PRIVATE file mapping: map the cache frame
                        // READ-ONLY + PTE_COW regardless of vma.flags'
                        // writable bit — the FIRST write must fault and
                        // copy-away from the shared cache frame via the
                        // EXISTING resolve_cow_fault_inner path. cow_share
                        // registers this mapping's claim on the frame so
                        // that path never reuses the cache's frame in
                        // place (see cow_release's doc comment for why
                        // this is now safe: an entry existing at all,
                        // regardless of its refcount, always means
                        // "copy away", which is exactly the property a
                        // page-cache frame needs from every one of its
                        // private mappers).
                        crate::cow::cow_share(PhysAddr::new(frame));
                        (frame, false, true)
                    }
                }
            };

            let mut pte_flags = vma.flags;
            pte_flags = if writable_now {
                pte_flags.with_writable()
            } else {
                pte_flags.without_writable()
            };
            if mark_cow {
                pte_flags = pte_flags.with_cow();
            }
            self.map_page(VirtAddr::new(page), PhysAddr::new(frame), pte_flags);
            // This mapping may be visible to another CPU running a
            // CLONE_VM sibling thread under this same AddressSpace right
            // now -- shootdown_page flushes locally first, then IPIs any
            // remote CPU recorded in active_cpus.
            crate::shootdown::shootdown_page(self.active_cpus.snapshot(), page);
            true
        }
        #[cfg(not(target_os = "none"))]
        {
            false
        }
    }

    #[cfg(target_os = "none")]
    pub(crate) unsafe fn map_page(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) {
        let pml4 = direct_map_table_mut(self.pml4_phys);
        let pml4_idx = virt.pml4_index();
        debug_assert!(pml4_idx < USER_PML4_ENTRIES);
        let pdpt = unsafe {
            // SAFETY: pml4 points at this address space's top-level table.
            ensure_next_table(pml4, pml4_idx, true)
        };
        let pd = unsafe {
            // SAFETY: pdpt is a present page-table page owned by this address space.
            ensure_next_table(pdpt, virt.pdpt_index(), true)
        };
        let pt = unsafe {
            // SAFETY: pd is a present page-table page owned by this address space.
            ensure_next_table(pd, virt.pd_index(), true)
        };
        unsafe {
            // SAFETY: pt is the final page table for virt.
            (*pt).set(virt.pt_index(), PageTableEntry::new_page(phys, flags));
        }
    }

    #[cfg(target_os = "none")]
    unsafe fn leaf_entry_mut(&mut self, virt: VirtAddr) -> Option<&'static mut PageTableEntry> {
        let pml4 = direct_map_table_mut(self.pml4_phys);
        let pml4e = unsafe { (*pml4).get(virt.pml4_index()) };
        if !pml4e.is_present() || virt.pml4_index() >= USER_PML4_ENTRIES {
            return None;
        }
        let pdpt = direct_map_table_mut(pml4e.frame());
        let pdpte = unsafe { (*pdpt).get(virt.pdpt_index()) };
        if !pdpte.is_present() || pdpte.is_huge_page() {
            return None;
        }
        let pd = direct_map_table_mut(pdpte.frame());
        let pde = unsafe { (*pd).get(virt.pd_index()) };
        if !pde.is_present() || pde.is_huge_page() {
            return None;
        }
        let pt = direct_map_table_mut(pde.frame());
        Some(unsafe { (*pt).get_mut(virt.pt_index()) })
    }

    #[cfg(target_os = "none")]
    unsafe fn clone_user_half_cow(&mut self, child: &mut AddressSpace) {
        let parent_pml4 = direct_map_table_mut(self.pml4_phys);
        for pml4_idx in 0..USER_PML4_ENTRIES {
            let pml4e = unsafe { (*parent_pml4).get(pml4_idx) };
            if !pml4e.is_present() {
                continue;
            }
            let pdpt = direct_map_table_mut(pml4e.frame());
            for pdpt_idx in 0..kernel_paging::PageTable::ENTRIES {
                let pdpte = unsafe { (*pdpt).get(pdpt_idx) };
                if !pdpte.is_present() || pdpte.is_huge_page() {
                    continue;
                }
                let pd = direct_map_table_mut(pdpte.frame());
                for pd_idx in 0..kernel_paging::PageTable::ENTRIES {
                    let pde = unsafe { (*pd).get(pd_idx) };
                    if !pde.is_present() || pde.is_huge_page() {
                        continue;
                    }
                    let pt = direct_map_table_mut(pde.frame());
                    for pt_idx in 0..kernel_paging::PageTable::ENTRIES {
                        let entry = unsafe { (*pt).get(pt_idx) };
                        if !entry.is_present() {
                            continue;
                        }
                        let virt = ((pml4_idx as u64) << 39)
                            | ((pdpt_idx as u64) << 30)
                            | ((pd_idx as u64) << 21)
                            | ((pt_idx as u64) << 12);

                        // MAP_SHARED file pages must stay mutually
                        // writable across fork — never CoW, never
                        // touching COW_TABLE at all. Every other present
                        // page (Backing::Anonymous, a MAP_PRIVATE file
                        // page, or a page outside any VMA at all such as
                        // ELF segments/the user stack, for which
                        // find_vma returns None) keeps the ordinary CoW
                        // treatment below, unchanged from before this
                        // checkpoint.
                        if let Some(vma) = self.find_vma(virt)
                            && let crate::vma::Backing::File { shared: true, .. } = vma.backing
                        {
                            unsafe {
                                child.map_page(VirtAddr::new(virt), entry.frame(), entry.flags())
                            };
                            continue;
                        }

                        let cow_flags = crate::pagefault::cow_readonly_flags(entry.flags());
                        unsafe { (*pt).set(pt_idx, entry.with_flags(cow_flags)) };
                        crate::cow::cow_share(entry.frame());
                        unsafe { child.map_page(VirtAddr::new(virt), entry.frame(), cow_flags) };
                        // This is `self` (the PARENT)'s own PTE being
                        // marked read-only/CoW -- self may itself be
                        // CLONE_VM-shared with a sibling thread running on
                        // another CPU at the exact moment fork() is
                        // called, so this needs a real shootdown, not just
                        // a local flush. The CHILD's brand-new mapping
                        // above needs no such flush: it was never visible
                        // to any CPU before this instant.
                        unsafe { crate::shootdown::shootdown_page(self.active_cpus.snapshot(), virt) };
                    }
                }
            }
        }
    }

    #[cfg(target_os = "none")]
    unsafe fn resolve_cow_fault_inner(&mut self, addr: u64) -> bool {
        let page = addr & !(PAGE_SIZE - 1);
        let Some(entry) = (unsafe { self.leaf_entry_mut(VirtAddr::new(page)) }) else {
            return false;
        };
        let old = *entry;
        if !old.is_present() || !old.is_cow() {
            return false;
        }
        let old_frame = old.frame();
        let new_flags = crate::pagefault::cow_writable_flags(old.flags());

        // cow_release() makes the "is any other real peer's claim still
        // outstanding?" check and the refcount decrement a single atomic
        // step (see its doc comment for why the refcount==2 boundary
        // specifically matters here) — using a separate cow_refcount()
        // read here followed by a later cow_unshare() call would be a
        // TOCTOU race: a sibling process on another CPU could be
        // resolving its own CoW fault on this exact physical frame at
        // the same time.
        if crate::cow::cow_release(old_frame) {
            // We were the sole remaining owner — reuse the frame in place,
            // no copy needed.
            *entry = old.with_flags(new_flags);
            // SAFETY: this AddressSpace may be CLONE_VM-shared with a
            // thread on another CPU right now; shootdown_page flushes
            // locally first, then IPIs any remote CPU in active_cpus.
            unsafe { crate::shootdown::shootdown_page(self.active_cpus.snapshot(), page) };
            return true;
        }

        let new_frame = kernel_memory::alloc_frame();
        unsafe {
            crate::pagefault::copy_frame(PhysAddr::new(new_frame.as_u64()), old_frame);
        }
        *entry = PageTableEntry::new_page(PhysAddr::new(new_frame.as_u64()), new_flags);
        // SAFETY: see the sole-owner branch above.
        unsafe { crate::shootdown::shootdown_page(self.active_cpus.snapshot(), page) };
        true
    }

    #[cfg(target_os = "none")]
    unsafe fn unmap_pages(&mut self, start: u64, end: u64) {
        let mut page = start;
        while page < end {
            if let Some(entry) = unsafe { self.leaf_entry_mut(VirtAddr::new(page)) } {
                let old = *entry;
                if old.is_present() {
                    let frame = old.frame();
                    *entry = PageTableEntry::empty();
                    // See cow_release()'s doc comment: this must be one
                    // atomic "does another real peer's claim still exist?"
                    // step, not a separate refcount-read followed by
                    // unshare — two different CPUs racing to free their
                    // own mapping of the same physical frame must not
                    // both conclude "someone else still owns it" (leak)
                    // NOR must one conclude "I'm sole owner" while the
                    // other peer's PTE still maps this frame (dangling
                    // page-table entry / physical-memory corruption).
                    let sole_owner = !old.is_cow() || crate::cow::cow_release(frame);
                    if sole_owner {
                        unsafe {
                            kernel_memory::free_frame(kernel_memory::PhysAddr::new(frame.as_u64()));
                        }
                    }
                    // SAFETY: this AddressSpace may be CLONE_VM-shared
                    // with a thread on another CPU right now.
                    unsafe { crate::shootdown::shootdown_page(self.active_cpus.snapshot(), page) };
                }
            }
            page += PAGE_SIZE;
        }
    }

    /// `unmap_pages`'s counterpart for a `Backing::File` VMA's page range.
    ///
    /// The one rule this function exists to enforce, per this checkpoint's
    /// design (see `docs/pagecache.md`): **a page-cache-backed frame is
    /// never `free_frame()`'d here, under any circumstance** — the page
    /// cache (`kernel_fs::pagecache`) is a distinct, independent owner
    /// that may still hand the same frame to a brand-new mapper long
    /// after this VMA is gone; freeing it here would corrupt that cache.
    ///
    /// - `shared == true` (MAP_SHARED): every present PTE in this VMA's
    ///   range is, by construction, always exactly the page-cache frame —
    ///   `resolve_file_fault` never CoW-copies a shared mapping, so there
    ///   is no "private copy" case to distinguish here at all. Just clear
    ///   the PTEs.
    /// - `shared == false` (MAP_PRIVATE): a present PTE is in one of two
    ///   states, and the *existing* `PTE_COW` bit is exactly the signal
    ///   that tells them apart, with no extra bookkeeping needed:
    ///     - Still `PTE_COW`: this mapping has never been written to, so
    ///       it still points directly at the page cache's frame.
    ///       `resolve_file_fault` registered this claim with `cow_share`
    ///       (see its own doc comment on why that's now safe), so
    ///       `cow_release` correctly reports "not sole" for as long as
    ///       the page cache's own implicit interest (or another fork
    ///       sibling's real claim) is still outstanding — meaning this
    ///       branch, just like `unmap_pages`'s, never frees a frame the
    ///       cache still needs.
    ///     - `PTE_COW` already cleared: a prior write fault already
    ///       copied this page away into a fresh, exclusively-owned frame
    ///       (see `resolve_cow_fault_inner`) that was *never* registered
    ///       with `cow_share` at all and has no page-cache entry pointing
    ///       at it either — always safe to free directly.
    #[cfg(target_os = "none")]
    unsafe fn unmap_file_pages(&mut self, start: u64, end: u64, shared: bool) {
        let mut page = start;
        while page < end {
            if let Some(entry) = unsafe { self.leaf_entry_mut(VirtAddr::new(page)) } {
                let old = *entry;
                if old.is_present() {
                    *entry = PageTableEntry::empty();
                    if !shared {
                        let frame = old.frame();
                        let sole_owner = !old.is_cow() || crate::cow::cow_release(frame);
                        if sole_owner {
                            unsafe {
                                kernel_memory::free_frame(kernel_memory::PhysAddr::new(
                                    frame.as_u64(),
                                ));
                            }
                        }
                    }
                    // shared == true: never free — always the page cache's frame.
                    // SAFETY: this AddressSpace may be CLONE_VM-shared
                    // with a thread on another CPU right now.
                    unsafe { crate::shootdown::shootdown_page(self.active_cpus.snapshot(), page) };
                }
            }
            page += PAGE_SIZE;
        }
    }
}

impl Default for AddressSpace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "none")]
fn direct_map_table(phys: PhysAddr) -> *const kernel_paging::PageTable {
    (DIRECT_MAP_BASE + phys.as_u64()) as *const kernel_paging::PageTable
}

#[cfg(target_os = "none")]
fn direct_map_table_mut(phys: PhysAddr) -> *mut kernel_paging::PageTable {
    (DIRECT_MAP_BASE + phys.as_u64()) as *mut kernel_paging::PageTable
}

#[cfg(target_os = "none")]
unsafe fn ensure_next_table(
    table: *mut kernel_paging::PageTable,
    index: usize,
    user: bool,
) -> *mut kernel_paging::PageTable {
    let mut entry = unsafe {
        // SAFETY: caller guarantees table points at a valid page table.
        (*table).get(index)
    };
    if !entry.is_present() {
        let frame = kernel_memory::alloc_frame();
        let next = direct_map_table_mut(PhysAddr::new(frame.as_u64()));
        unsafe {
            // SAFETY: frame is freshly allocated and directly mapped.
            (*next).zero();
        }
        let mut flags = PageFlags::new().with_present().with_writable();
        if user {
            flags = flags.with_user_accessible();
        }
        entry = PageTableEntry::new_page(PhysAddr::new(frame.as_u64()), flags);
        unsafe {
            // SAFETY: caller holds unique access during address-space construction.
            (*table).set(index, entry);
        }
    }
    direct_map_table_mut(entry.frame())
}

#[cfg(target_os = "none")]
unsafe fn free_user_half(pml4_phys: PhysAddr) {
    let pml4 = direct_map_table_mut(pml4_phys);
    for index in 0..USER_PML4_ENTRIES {
        let entry = unsafe {
            // SAFETY: pml4 points at this process's top-level table.
            (*pml4).get(index)
        };
        if entry.is_present() {
            unsafe {
                // SAFETY: user half was allocated by this address space.
                free_table(entry.frame(), 3);
                (*pml4).set(index, PageTableEntry::empty());
            }
        }
    }
}

#[cfg(target_os = "none")]
unsafe fn free_table(phys: PhysAddr, level: u8) {
    let table = direct_map_table_mut(phys);
    for index in 0..kernel_paging::PageTable::ENTRIES {
        let entry = unsafe {
            // SAFETY: table points at an allocated page-table page.
            (*table).get(index)
        };
        if !entry.is_present() {
            continue;
        }
        if level == 1 || entry.is_huge_page() {
            let frame = entry.frame();
            // See cow_release()'s doc comment: combine the "does another
            // real peer's claim still exist?" check and the decrement
            // into one atomic step rather than a separate cow_refcount()
            // read + cow_unshare() call, since a sibling process tearing
            // down its own mapping of this same frame (process exit,
            // munmap) can run concurrently on another CPU.
            let sole_owner = !entry.is_cow() || crate::cow::cow_release(frame);
            if sole_owner {
                unsafe {
                    // SAFETY: leaf frame is no longer reachable after its owning table is freed.
                    kernel_memory::free_frame(kernel_memory::PhysAddr::new(frame.as_u64()));
                }
            }
        } else {
            unsafe {
                // SAFETY: child page-table page belongs to the same user mapping tree.
                free_table(entry.frame(), level - 1);
            }
        }
    }
    unsafe {
        // SAFETY: all children are gone and the table frame is no longer referenced.
        kernel_memory::free_frame(kernel_memory::PhysAddr::new(phys.as_u64()));
    }
}

#[cfg(target_os = "none")]
fn user_flags_from_elf(elf_flags: u32) -> PageFlags {
    let mut flags = PageFlags::new().with_present().with_user_accessible();
    if (elf_flags & PF_W) != 0 {
        flags = flags.with_writable();
    }
    if (elf_flags & PF_X) == 0 {
        flags = flags.with_no_execute();
    }
    flags
}

#[cfg(target_os = "none")]
const fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_address_space_starts_mmap_at_base() {
        let aspace = AddressSpace::new();
        assert_eq!(aspace.mmap_next, MMAP_BASE);
        assert!(aspace.vmas.iter().all(Option::is_none));
    }

    #[test]
    fn mmap_anon_records_vma_without_mapping_on_host() {
        let mut aspace = AddressSpace::new();
        let addr = aspace
            .mmap_anon(
                4096,
                PageFlags::new()
                    .with_present()
                    .with_user_accessible()
                    .with_writable(),
            )
            .unwrap();
        assert_eq!(addr, MMAP_BASE);
        assert!(matches!(
            aspace.find_vma(addr).unwrap().backing,
            crate::vma::Backing::Anonymous
        ));
    }

    #[test]
    fn mmap_anon_rejects_unaligned_len() {
        let mut aspace = AddressSpace::new();
        assert!(aspace.mmap_anon(123, PageFlags::new()).is_none());
    }

    #[test]
    fn munmap_requires_exact_vma_range() {
        let mut aspace = AddressSpace::new();
        let addr = aspace.mmap_anon(4096, PageFlags::new()).unwrap();
        assert!(!aspace.munmap(addr, 8192));
        assert!(aspace.munmap(addr, 4096));
        assert!(aspace.find_vma(addr).is_none());
    }

    #[test]
    fn fork_cow_copies_vma_metadata_on_host() {
        let mut aspace = AddressSpace::new();
        let addr = aspace.mmap_anon(4096, PageFlags::new()).unwrap();
        let child = aspace.fork_cow();
        assert_eq!(child.find_vma(addr), aspace.find_vma(addr));
        assert_eq!(child.mmap_next, aspace.mmap_next);
    }

    #[test]
    fn new_address_space_starts_with_refcount_one() {
        let aspace = AddressSpace::new();
        assert_eq!(aspace.refcount_for_test(), 1);
    }

    #[test]
    fn inc_ref_then_dec_ref_returns_to_sole_owner() {
        let aspace = AddressSpace::new();
        aspace.inc_ref(); // simulates a CLONE_VM sibling attaching
        assert_eq!(aspace.refcount_for_test(), 2);
        assert!(!aspace.dec_ref(), "sibling exiting first is not the last owner");
        assert_eq!(aspace.refcount_for_test(), 1);
        assert!(aspace.dec_ref(), "the original owner exiting last IS the last owner");
    }

    #[test]
    fn dec_ref_on_sole_owner_reports_last() {
        let aspace = AddressSpace::new();
        assert!(aspace.dec_ref());
    }

    #[test]
    fn new_address_space_starts_with_no_active_cpus() {
        let aspace = AddressSpace::new();
        assert_eq!(aspace.active_cpus.snapshot(), 0);
    }
}
