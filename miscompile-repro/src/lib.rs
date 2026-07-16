//! Isolated reproduction matrix for the rustc 1.97.0 aggregate-copy
//! miscompile family (see `docs/miscompile-audit.md` at the repo root).
//!
//! Each `case_*` function replicates ONE of the aggregate-copy shapes found
//! in the kernel — including the three confirmed-miscompiled ones — and
//! returns the number of corrupted elements it observed (0 = copy was
//! compiled correctly in this build). All inputs flow through
//! `black_box` so LTO cannot const-fold the copy under test away, and every
//! case is `#[inline(never)]` so each shape gets its own codegen unit of
//! optimization context, mimicking the kernel's cross-function call sites.
//!
//! Three confirmed kernel bugs being replicated:
//!  * bug #1: `self.vmas[slot] = Some(Vma { backing: Backing::File {..} })`
//!    through a raw-pointer-derefed struct (crates/proc/src/address_space.rs)
//!  * bug #2: `let snap = *children_guard` on `SpinLock<[u32; 32]>`
//!    (crates/proc/src/wait.rs, since rewritten element-wise)
//!  * bug #3: `[u32; 32]` field of a struct literal not initialized through
//!    `ptr.write(Process { .. })` placement (crates/proc/src/process.rs)

#![cfg_attr(not(feature = "std"), no_std)]

use core::cell::UnsafeCell;
use core::hint::black_box;
use kernel_sync::SpinLock;

#[cfg(all(not(feature = "std"), not(test)))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ── replica types (layout-identical to the kernel originals) ──────────────

/// Replica of `kernel_proc::vma::Backing` (repr(C, u8), pointer payload).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, u8)]
pub enum BackingR {
    Anonymous,
    File {
        vnode_ptr: usize,
        file_offset: u64,
        shared: bool,
    },
}

/// Replica of `kernel_proc::vma::Vma` (`flags: u64` stands in for
/// `PageFlags`, a `#[repr(transparent)]`-equivalent u64 newtype).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct VmaR {
    pub start: u64,
    pub end: u64,
    pub flags: u64,
    pub backing: BackingR,
}

/// Same shape as `VmaR` but the enum payload has NO pointer-sized "address"
/// meaning — distinguishes "arrays of structs-with-pointers" from "arrays
/// of plain-integer structs" (test case d).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, u8)]
pub enum IntBackingR {
    Empty,
    Ints { a: u32, b: u32, c: u64, d: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct IntVmaR {
    pub start: u64,
    pub end: u64,
    pub flags: u64,
    pub backing: IntBackingR,
}

/// Replica of `kernel_proc::fd::FdEntry`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FdEntryR {
    pub vnode_ptr: usize,
    pub offset: u64,
    pub flags: u32,
    pub _reserved: u32,
}

pub const MAX_FDS: usize = 64;

/// Replica of `kernel_proc::fd::FdTable`, including `clone_for_fork`'s
/// wholesale `Self { fds: self.fds }` array copy.
pub struct FdTableR {
    pub fds: [Option<FdEntryR>; MAX_FDS],
}

impl FdTableR {
    pub const fn new() -> Self {
        Self {
            fds: [const { None }; MAX_FDS],
        }
    }
    #[inline(never)]
    pub fn clone_for_fork(&self) -> Self {
        Self { fds: self.fds }
    }
}

/// Replica of `kernel_proc::signal::SigAction` / `SigTable`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigActionR {
    Default,
    Ignore,
    Handler { user_fn: u64 },
}

pub const NSIG: usize = 32;

#[derive(Clone, Copy)]
pub struct SigTableR {
    pub actions: [SigActionR; NSIG],
}

pub const MAX_VMAS: usize = 32;
pub const MAX_CHILDREN: usize = 32;

/// Replica of the vmas-owning part of `AddressSpace`, accessed through a
/// raw pointer to a runtime address exactly like the kernel's
/// `(*(*proc).address_space).mmap(..)` path (bug #1's real access shape).
#[repr(C)]
pub struct AddrSpaceR {
    pub pml4_phys: u64,
    pub pcid: u16,
    pub vmas: [Option<VmaR>; MAX_VMAS],
    pub mmap_next: u64,
}

/// Same but with the pointer-free Vma variant (case d).
#[repr(C)]
pub struct IntAddrSpaceR {
    pub pml4_phys: u64,
    pub pcid: u16,
    pub vmas: [Option<IntVmaR>; MAX_VMAS],
    pub mmap_next: u64,
}

/// Cut-down replica of `Process` for the placement-write case (bug #3):
/// a struct literal containing `SpinLock<[u32; 32]>` written to a raw
/// "frame" address via `ptr.write`.
#[repr(C, align(64))]
pub struct ProcessR {
    pub pid: u32,
    pub state: u32,
    pub name: [u8; 16],
    pub user_rip: u64,
    pub user_rsp: u64,
    pub children: SpinLock<[u32; MAX_CHILDREN]>,
    pub child_count: u32,
}

/// 1-byte fieldless enum, matching the real `ProcessState` — its trailing
/// 3+ padding bytes inside `ProcessF` are what the LLVM padding-model bug
/// family (rust-lang/rust#159035) appears to need.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StateR {
    Running,
    Zombie,
}

/// Field-faithful replica of the head of the real `Process` (through
/// `name`), for the live-confirmed `name: parent.name` shredding observed
/// in the shipped kernel's sys_fork/sys_clone (see docs/miscompile-audit.md).
#[repr(C, align(64))]
pub struct ProcessF {
    pub pid: u32,
    pub state: StateR, // 1 byte @4, padding 5..8 — the suspected trigger
    pub exit_code: i32,
    pub address_space: *mut u8,
    pub thread: *mut u8,
    pub name: [u8; 16], // @0x20 — the field shredded in the real binary
    pub user_rip: u64,
    pub user_rsp: u64,
    pub user_rflags: u64,
    pub user_rax: i64,
    pub children: SpinLock<[u32; MAX_CHILDREN]>,
    pub child_count: u32,
}

// ── fake "physical frames" for placement writes ────────────────────────────
// The kernel placement-writes Process/AddressSpace into direct-mapped
// alloc_frame() memory that is NOT pre-zeroed. A static byte buffer
// pre-filled with 0xAA mimics that: any field the aggregate write fails to
// initialize keeps visibly stale bytes.

#[repr(C, align(4096))]
struct Frame(UnsafeCell<[u8; 4096]>);
unsafe impl Sync for Frame {}

static FRAME_C: Frame = Frame(UnsafeCell::new([0xAA; 4096]));
static FRAME_D: Frame = Frame(UnsafeCell::new([0xAA; 4096]));
static FRAME_K: Frame = Frame(UnsafeCell::new([0xAA; 4096]));

// ── statics mirroring the kernel's table shapes ────────────────────────────

static CHILDREN_A: SpinLock<[u32; MAX_CHILDREN]> = SpinLock::new([0; MAX_CHILDREN]);
static CHILDREN_B: SpinLock<[u32; MAX_CHILDREN]> = SpinLock::new([0; MAX_CHILDREN]);
static ARR_8: SpinLock<[u32; 8]> = SpinLock::new([0; 8]);
static ARR_16: SpinLock<[u32; 16]> = SpinLock::new([0; 16]);
static ARR_64: SpinLock<[u32; 64]> = SpinLock::new([0; 64]);
static SIG_TABLE: SpinLock<SigTableR> = SpinLock::new(SigTableR {
    actions: [SigActionR::Default; NSIG],
});
static FD_TABLE: SpinLock<FdTableR> = SpinLock::new(FdTableR::new());

#[inline(always)]
fn expect_u32(seed: u32, i: usize) -> u32 {
    // Same derivation the populate loops use; kept trivial but seed-opaque.
    seed.wrapping_mul(0x9e37_79b9).wrapping_add(i as u32) | 1
}

// ── case a: bug #2 exact — `let snap = *guard` on SpinLock<[u32; 32]> ─────

/// Populate element-wise through the guard (exactly `add_child`'s shape),
/// then aggregate-copy the whole array out through the guard deref
/// (exactly the pre-fix `find_zombie` shape) and compare.
#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_a_snapshot_guard_u32_32(seed: u32) -> u64 {
    let seed = black_box(seed);
    {
        let mut g = CHILDREN_A.lock();
        for i in 0..MAX_CHILDREN {
            g[i] = expect_u32(seed, i);
        }
    }
    let snap = {
        let g = CHILDREN_A.lock();
        *g // ← the aggregate copy under test
    };
    let snap = black_box(snap);
    let mut bad = 0u64;
    for i in 0..MAX_CHILDREN {
        if snap[i] != expect_u32(seed, i) {
            bad += 1;
        }
    }
    bad
}

// ── case b: same array, `.clone()` through the guard ──────────────────────

#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_b_clone_guard_u32_32(seed: u32) -> u64 {
    let seed = black_box(seed);
    {
        let mut g = CHILDREN_B.lock();
        for i in 0..MAX_CHILDREN {
            g[i] = expect_u32(seed, i);
        }
    }
    let snap = {
        let g = CHILDREN_B.lock();
        g.clone() // auto-derefs to <[u32; 32] as Clone>::clone
    };
    let snap = black_box(snap);
    let mut bad = 0u64;
    for i in 0..MAX_CHILDREN {
        if snap[i] != expect_u32(seed, i) {
            bad += 1;
        }
    }
    bad
}

// ── case c: bug #1 exact — arr[slot] = Some(Vma{ File{..} }) via raw ptr ──

/// Mirrors `AddressSpace::mmap`'s original (miscompiled) form: the
/// `AddrSpaceR` lives at a runtime raw-pointer address (placement-written
/// into a fake frame), and the store is one aggregate
/// `vmas[slot] = Some(VmaR { backing: File { .. } })`.
#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_c_slot_store_file_vma(seed: u32) -> u64 {
    let seed = black_box(seed) as u64;
    let ptr = FRAME_C.0.get() as *mut AddrSpaceR;
    // SAFETY: repro-only static frame, single-threaded access, 4096 bytes
    // is asserted (below) to fit AddrSpaceR.
    unsafe {
        ptr.write(AddrSpaceR {
            pml4_phys: seed,
            pcid: 1,
            vmas: [const { None }; MAX_VMAS],
            mmap_next: 0x4000_0000_0000,
        });
        let aspace = &mut *ptr;

        let mut bad = 0u64;
        for slot in 0..MAX_VMAS {
            let vnode_ptr = (0xffff_8000_0000_0000u64 | (seed << 12) | (slot as u64)) as usize;
            let start = 0x4000_0000_0000u64 + (slot as u64) * 0x1000;
            // ← the aggregate store under test (bug #1's exact shape)
            aspace.vmas[slot] = Some(VmaR {
                start,
                end: start + 0x1000,
                flags: 0x8000_0000_0000_0007,
                backing: BackingR::File {
                    vnode_ptr,
                    file_offset: seed << 12,
                    shared: slot & 1 == 0,
                },
            });
        }
        let aspace = black_box(&mut *ptr);
        for slot in 0..MAX_VMAS {
            let vnode_expect =
                (0xffff_8000_0000_0000u64 | (seed << 12) | (slot as u64)) as usize;
            let start = 0x4000_0000_0000u64 + (slot as u64) * 0x1000;
            match aspace.vmas[slot] {
                Some(VmaR {
                    start: s,
                    end: e,
                    flags: f,
                    backing:
                        BackingR::File {
                            vnode_ptr: v,
                            file_offset: off,
                            shared: sh,
                        },
                }) if s == start
                    && e == start + 0x1000
                    && f == 0x8000_0000_0000_0007
                    && v == vnode_expect
                    && off == seed << 12
                    && sh == (slot & 1 == 0) => {}
                _ => bad += 1,
            }
        }
        bad
    }
}

// ── case d: same as (c) but the payload struct holds NO pointer field ─────

#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_d_slot_store_int_vma(seed: u32) -> u64 {
    let seed = black_box(seed) as u64;
    let ptr = FRAME_D.0.get() as *mut IntAddrSpaceR;
    // SAFETY: as in case c.
    unsafe {
        ptr.write(IntAddrSpaceR {
            pml4_phys: seed,
            pcid: 1,
            vmas: [const { None }; MAX_VMAS],
            mmap_next: 0,
        });
        let aspace = &mut *ptr;
        let mut bad = 0u64;
        for slot in 0..MAX_VMAS {
            let a = (seed as u32).wrapping_add(slot as u32);
            aspace.vmas[slot] = Some(IntVmaR {
                start: slot as u64,
                end: slot as u64 + 1,
                flags: seed,
                backing: IntBackingR::Ints {
                    a,
                    b: !a,
                    c: seed ^ (slot as u64),
                    d: slot & 1 == 0,
                },
            });
        }
        let aspace = black_box(&mut *ptr);
        for slot in 0..MAX_VMAS {
            let a = (seed as u32).wrapping_add(slot as u32);
            match aspace.vmas[slot] {
                Some(IntVmaR {
                    start: s,
                    end: e,
                    flags: f,
                    backing: IntBackingR::Ints { a: aa, b, c, d },
                }) if s == slot as u64
                    && e == slot as u64 + 1
                    && f == seed
                    && aa == a
                    && b == !a
                    && c == seed ^ (slot as u64)
                    && d == (slot & 1 == 0) => {}
                _ => bad += 1,
            }
        }
        bad
    }
}

// ── case e: plain local arrays, no lock-guard deref anywhere ──────────────

#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_e_local_array_copy(seed: u32) -> u64 {
    let seed = black_box(seed);
    let mut src = [0u32; MAX_CHILDREN];
    for (i, s) in src.iter_mut().enumerate() {
        *s = expect_u32(seed, i);
    }
    let src = black_box(src);
    let snap = src; // ← plain by-value array copy under test
    let snap = black_box(snap);
    let mut bad = 0u64;
    for i in 0..MAX_CHILDREN {
        if snap[i] != expect_u32(seed, i) {
            bad += 1;
        }
    }
    bad
}

// ── case f: guard-deref snapshots at other array sizes ─────────────────────

macro_rules! size_case {
    ($fn_name:ident, $static:ident, $n:expr) => {
        #[unsafe(no_mangle)]
        #[inline(never)]
        pub fn $fn_name(seed: u32) -> u64 {
            let seed = black_box(seed);
            {
                let mut g = $static.lock();
                for i in 0..$n {
                    g[i] = expect_u32(seed, i);
                }
            }
            let snap = {
                let g = $static.lock();
                *g
            };
            let snap = black_box(snap);
            let mut bad = 0u64;
            for i in 0..$n {
                if snap[i] != expect_u32(seed, i) {
                    bad += 1;
                }
            }
            bad
        }
    };
}

size_case!(case_f_snapshot_guard_u32_8, ARR_8, 8);
size_case!(case_f_snapshot_guard_u32_16, ARR_16, 16);
size_case!(case_f_snapshot_guard_u32_64, ARR_64, 64);

// ── case h: fork_cow's `child.vmas = self.vmas` whole-array copy ───────────

#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_h_vmas_whole_copy(seed: u32) -> u64 {
    let seed = black_box(seed) as u64;
    let mut parent = AddrSpaceR {
        pml4_phys: seed,
        pcid: 1,
        vmas: [const { None }; MAX_VMAS],
        mmap_next: 0,
    };
    for slot in 0..MAX_VMAS {
        // Populate FIELD-BY-FIELD via the write pattern the kernel's fixed
        // mmap uses, so the parent's array is known-good before the copy.
        let vnode_ptr = (0xffff_8000_0000_0000u64 | (seed << 16) | (slot as u64)) as usize;
        parent.vmas[slot] = Some(VmaR {
            start: slot as u64,
            end: slot as u64 + 1,
            flags: 7,
            backing: BackingR::Anonymous,
        });
        if slot & 1 == 0 {
            let v = parent.vmas[slot].as_mut().unwrap();
            let b = (&mut v.backing) as *mut BackingR as *mut u8;
            // SAFETY: BackingR is repr(C, u8): tag@0, vnode_ptr@8,
            // file_offset@16, shared@24 — same offsets the kernel fix uses.
            unsafe {
                b.add(8).cast::<usize>().write_volatile(vnode_ptr);
                b.add(16).cast::<u64>().write_volatile(seed << 8);
                b.add(24).write_volatile(1);
                b.write_volatile(1);
            }
        }
    }
    let parent = black_box(&parent);

    let mut child = AddrSpaceR {
        pml4_phys: 0,
        pcid: 2,
        vmas: [const { None }; MAX_VMAS],
        mmap_next: 0,
    };
    child.vmas = parent.vmas; // ← fork_cow's exact aggregate copy under test
    child.mmap_next = parent.mmap_next;

    let child = black_box(&child);
    let mut bad = 0u64;
    for slot in 0..MAX_VMAS {
        let vnode_expect = (0xffff_8000_0000_0000u64 | (seed << 16) | (slot as u64)) as usize;
        let ok = match child.vmas[slot] {
            Some(v) if v.start == slot as u64 && v.end == slot as u64 + 1 && v.flags == 7 => {
                if slot & 1 == 0 {
                    matches!(
                        v.backing,
                        BackingR::File { vnode_ptr, file_offset, shared: true }
                            if vnode_ptr == vnode_expect && file_offset == seed << 8
                    )
                } else {
                    v.backing == BackingR::Anonymous
                }
            }
            _ => false,
        };
        if !ok {
            bad += 1;
        }
    }
    bad
}

// ── case i: `let t = *sig_table.lock()` — [SigAction; 32] via guard ───────

#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_i_sigtable_guard_copy(seed: u32) -> u64 {
    let seed = black_box(seed) as u64;
    {
        let mut g = SIG_TABLE.lock();
        for i in 0..NSIG {
            g.actions[i] = match i % 3 {
                0 => SigActionR::Handler {
                    user_fn: 0x1000_0000 + (seed << 8) + i as u64,
                },
                1 => SigActionR::Ignore,
                _ => SigActionR::Default,
            };
        }
    }
    let table = {
        let g = SIG_TABLE.lock();
        *g // ← signal.rs / fork.rs / clone.rs shape under test
    };
    let table = black_box(table);
    let mut bad = 0u64;
    for i in 0..NSIG {
        let expect = match i % 3 {
            0 => SigActionR::Handler {
                user_fn: 0x1000_0000 + (seed << 8) + i as u64,
            },
            1 => SigActionR::Ignore,
            _ => SigActionR::Default,
        };
        if table.actions[i] != expect {
            bad += 1;
        }
    }
    bad
}

// ── case j: FdTable::clone_for_fork — [Option<FdEntry>; 64] wholesale ─────

#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_j_fdtable_clone_for_fork(seed: u32) -> u64 {
    let seed = black_box(seed) as usize;
    {
        let mut g = FD_TABLE.lock();
        for i in 0..MAX_FDS {
            g.fds[i] = if i % 5 == 4 {
                None // leave holes like a real fd table after close()
            } else {
                Some(FdEntryR {
                    vnode_ptr: 0xffff_8000_0000_0000usize | (seed << 12) | i,
                    offset: (seed as u64) << 4,
                    flags: (i % 3) as u32,
                    _reserved: 0,
                })
            };
        }
    }
    // fork.rs shape: `parent.fd_table.lock().clone_for_fork()`
    let child = FD_TABLE.lock().clone_for_fork();
    let child = black_box(child);
    let mut bad = 0u64;
    for i in 0..MAX_FDS {
        let expect = if i % 5 == 4 {
            None
        } else {
            Some(FdEntryR {
                vnode_ptr: 0xffff_8000_0000_0000usize | (seed << 12) | i,
                offset: (seed as u64) << 4,
                flags: (i % 3) as u32,
                _reserved: 0,
            })
        };
        if child.fds[i] != expect {
            bad += 1;
        }
    }
    bad
}

// ── case k: bug #3 exact — ptr.write(struct literal with array field) ─────

#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_k_placement_write_struct(seed: u32) -> u64 {
    let seed = black_box(seed);
    let ptr = FRAME_K.0.get() as *mut ProcessR;
    // SAFETY: repro-only static frame pre-filled with 0xAA (mimics a
    // non-zeroed alloc_frame), single-threaded.
    unsafe {
        let mut p = ProcessR {
            pid: seed,
            state: 1,
            name: [0; 16],
            user_rip: 0x40_0000,
            user_rsp: 0x7fff_ffff_0000,
            children: SpinLock::new([0; MAX_CHILDREN]), // ← must land as zeros
            child_count: 0,
        };
        p.name[0] = b'r'; // mimic Process::create's copy_name mutation
        ptr.write(p); // ← the placement aggregate write under test

        let mut bad = 0u64;
        let g = (*black_box(ptr)).children.lock();
        for i in 0..MAX_CHILDREN {
            if g[i] != 0 {
                bad += 1;
            }
        }
        drop(g);
        if (*ptr).pid != seed || (*ptr).name[0] != b'r' {
            bad += 1;
        }
        bad
    }
}

// ── case l: sys_fork's live-confirmed `name: parent.name` shredding ───────

static FRAME_L_PARENT: Frame = Frame(UnsafeCell::new([0xAA; 4096]));
static FRAME_L_CHILD: Frame = Frame(UnsafeCell::new([0xAA; 4096]));

/// Replicates the exact fork shape whose `[u8; 16]` name copy is shredded
/// to even-bytes-only in the SHIPPED kernel binary (sys_fork at
/// ffffffff80015972 / ffffffff80015af8, kernel built 2026-07-14): read the
/// parent through a raw pointer, build a child `ProcessF` literal with
/// `name: parent.name`, placement-write it to a second raw frame.
#[unsafe(no_mangle)]
#[inline(never)]
pub fn case_l_fork_name_copy(seed: u32) -> u64 {
    let seed = black_box(seed);
    let parent_ptr = FRAME_L_PARENT.0.get() as *mut ProcessF;
    let child_ptr = FRAME_L_CHILD.0.get() as *mut ProcessF;
    // SAFETY: repro-only static frames, single-threaded.
    unsafe {
        let mut name = [0u8; 16];
        for (i, b) in name.iter_mut().enumerate() {
            *b = (seed as u8).wrapping_add(i as u8) | 1;
        }
        parent_ptr.write(ProcessF {
            pid: seed,
            state: StateR::Running,
            exit_code: 0,
            address_space: core::ptr::null_mut(),
            thread: core::ptr::null_mut(),
            name,
            user_rip: 0x40_0000,
            user_rsp: 0x7fff_ffff_0000,
            user_rflags: 0x202,
            user_rax: 0,
            children: SpinLock::new([0; MAX_CHILDREN]),
            child_count: 0,
        });
        let parent = &mut *black_box(parent_ptr);

        // sys_fork's child construction shape (fork.rs:47).
        child_ptr.write(ProcessF {
            pid: seed.wrapping_add(1),
            state: StateR::Running,
            exit_code: 0,
            address_space: core::ptr::null_mut(),
            thread: core::ptr::null_mut(),
            name: parent.name, // ← the live-shredded copy under test
            user_rip: parent.user_rip,
            user_rsp: parent.user_rsp,
            user_rflags: parent.user_rflags,
            user_rax: 0,
            children: SpinLock::new([0; MAX_CHILDREN]),
            child_count: 0,
        });

        let child = &*black_box(child_ptr);
        let mut bad = 0u64;
        for i in 0..16 {
            if child.name[i] != (seed as u8).wrapping_add(i as u8) | 1 {
                bad += 1;
            }
        }
        bad
    }
}

// ── layout sanity: replicas must match the kernel originals' sizes ────────

const _: () = {
    // Backing: repr(C,u8) tag@0 + File{usize@8, u64@16, bool@24} → 32.
    assert!(core::mem::size_of::<BackingR>() == 32);
    // Vma: 3×u64 + Backing → 56.
    assert!(core::mem::size_of::<VmaR>() == 56);
    assert!(core::mem::size_of::<FdEntryR>() == 24);
    assert!(core::mem::size_of::<AddrSpaceR>() <= 4096);
    assert!(core::mem::size_of::<IntAddrSpaceR>() <= 4096);
    assert!(core::mem::size_of::<ProcessR>() <= 4096);
};

/// Every case with its name, for the runner.
pub const CASES: &[(&str, fn(u32) -> u64)] = &[
    ("a  snapshot  *guard          [u32;32]  (bug #2 exact)", case_a_snapshot_guard_u32_32),
    ("b  clone      guard.clone()  [u32;32]", case_b_clone_guard_u32_32),
    ("c  slot store Some(File Vma) via raw ptr (bug #1 exact)", case_c_slot_store_file_vma),
    ("d  slot store Some(int Vma)  — no pointer field", case_d_slot_store_int_vma),
    ("e  local      by-value copy  [u32;32] — no guard", case_e_local_array_copy),
    ("f  snapshot  *guard          [u32;8]", case_f_snapshot_guard_u32_8),
    ("f  snapshot  *guard          [u32;16]", case_f_snapshot_guard_u32_16),
    ("f  snapshot  *guard          [u32;64]", case_f_snapshot_guard_u32_64),
    ("h  fork_cow   child.vmas = parent.vmas  [Option<Vma>;32]", case_h_vmas_whole_copy),
    ("i  sigtable  *guard          [SigAction;32]", case_i_sigtable_guard_copy),
    ("j  fdtable    clone_for_fork [Option<FdEntry>;64]", case_j_fdtable_clone_for_fork),
    ("k  placement  ptr.write(struct literal)  (bug #3 exact)", case_k_placement_write_struct),
    ("l  fork name  [u8;16] via struct literal (live in sys_fork)", case_l_fork_name_copy),
];
