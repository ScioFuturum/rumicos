# Copy-on-Write fork(), the #PF Handler, and Anonymous mmap()

`kernel-proc` gains four new modules — `cow`, `vma`, `pagefault`, `fork` — that
share one page-fault handler. `kernel-cpu` gains a per-CPU syscall-frame
snapshot so `fork()` can recover the parent's user RIP/RSP/RFLAGS without a
new ABI surface on the SYSCALL trampoline.

## Substitutions from the design brief

The original task description sketched idealized names and a
`PENDING_RING3`/`Ring3Entry.return_value` staging mechanism for getting a
fresh thread's first RAX value right. The real tree differs in a few places;
all of them were verified against the actual code before writing anything:

- **No `PENDING_RING3` table exists, and none was added.** `Process` already
  carries `user_rip`/`user_rsp`/`user_rflags` *and* a `user_rax: i64` field
  directly, and `ring3_entry_rust()` reads all four straight off the
  `Process` struct found via `current_process()` (a `PROCESS_TABLE` lookup
  keyed by thread pointer) before calling `enter_user_mode`, whose asm tail
  already does `mov rax, {rax}` unconditionally. This sidesteps the entire
  race the brief flagged ("two freshly-created threads scheduled on the same
  CPU before either runs") by construction — each child's resume state lives
  on *its own* `Process`, not a transient per-CPU slot, so there is nothing
  to re-key. `fork()` simply sets `user_rax: 0` on the child's `Process`
  literal, exactly like `Process::create`/`do_execve` already set it to `0`
  for their own (don't-care) cases. Part 5b's three-call
  `ring3_entry_rax`/destructive-read concern does not apply to this design
  and was not implemented.
- `kernel_cpu::SyscallFrame` matches the brief's sketch almost exactly, but
  is populated by a small Rust shim (`update_current_syscall_frame`, called
  from the trampoline with `rcx`/saved-user-`rsp`/`r11` already shuffled into
  `rdi`/`rsi`/`rdx`) rather than three inline `mov`s into a `SpinLock`-guarded
  static — same effect (a few extra cache-resident stores ahead of
  `syscall_dispatch`), implemented as atomics (`AtomicU64` triplet) instead
  of a spinlock, which avoids taking a lock on the hottest path in the
  kernel.
- `PageFlags::COW` is bit 10, as suggested, and the entry.rs doc comment
  already documents bit 9 as reserved for the frame-allocator "owned" flag —
  confirmed no collision (`entry::tests::cow_uses_available_bit_ten_without_colliding_with_owned`).
- `AddressSpace::fork_cow` takes `&mut self`, not `&self` — it must mutate
  the *parent's* PTEs in place (clear Writable, set CoW) as part of the
  walk, so an immutable receiver was never going to typecheck; this is a
  necessary, not cosmetic, deviation from the brief's signature.
- `sys_mmap`'s real signature is `(addr_hint, length, prot, flags, fd) -> i64`
  — five arguments, no `offset`. Since file-backed mmap (the only thing that
  would consume an offset) is out of scope this checkpoint and explicitly
  rejected with `EINVAL`, the offset parameter was simply dropped rather than
  threaded through and ignored. `syscall_dispatch` already carries six
  argument registers (`a1..a5` plus the unused `a6` slot was never added),
  so no dispatch-signature change was needed either.
- `MMAP_BASE`/`MMAP_REGION_END` use `0x0000_4000_0000_0000`/
  `0x0000_7000_0000_0000` rather than the brief's `0x0000_7000_.../0x0000_7F00_...`
  — still well below `USTACK_TOP` (`0x0000_7fff_ffff_0000`) with a large gap
  on both sides, just shifted down to leave more headroom between the mmap
  region and the stack.

## `cow` — per-frame share-count table

Performance rationale: CoW pages are the minority of mapped pages (only
pages actually shared by `fork()`), so a sparse 256-bucket table with a
bounded 32-entry open list per bucket is the right shape — no per-physical-page
fixed array, no allocator dependency (this crate is `#![no_std]` with no
`alloc`).

x86_64 tricks used: none directly; this module is pure bookkeeping over a
frame-number key.

Cache behavior: `bucket_index` is a Fibonacci multiplicative hash
(`frame_number.wrapping_mul(0x9e3779b97f4a7c15) >> 56`), bit-for-bit the same
construction as `kernel_sched::futex::bucket_index`, deliberately kept
consistent with that proven design rather than just "functionally
equivalent" via a cheaper mask. Each bucket is independently
`SpinLock`-guarded; bucket locks are always acquired one at a time, never
nested.

Implementation: `crates/proc/src/cow.rs` exposes `cow_share`, `cow_refcount`,
`cow_unshare`, and a fourth function not in the original brief —
**`cow_release`**, added during this pass to close a real TOCTOU race: a
separate "read refcount, then decide whether to call `cow_unshare`" pair (as
literally specified in Part 3 of the brief) is unsound the moment two
processes can run concurrently on different cores, which this kernel's AP
bring-up (`kernel_smp::trampoline::start_aps`) already makes possible. If a
fork() parent and its child both resolve a CoW fault on the same physical
frame at the same instant and both read `refcount == 2` before either's
decrement lands, both conclude "someone else still owns this" and neither
frees it — a silent permanent leak with no PTE pointing at the frame
afterward. `cow_release` folds the read-and-decide into one bucket-lock
critical section and is now what `resolve_cow_fault_inner`/`unmap_pages`/
`free_table` in `address_space.rs` actually call; `cow_unshare` remains
available as a separate primitive (and is still exercised directly by
`cow.rs`'s own unit tests) but is no longer used internally for exactly this
reason. The bucket-overflow degrade path (treat as refcount-1/not-shared
when a bucket's 32 slots are full) is unchanged from the brief: a missed
optimization, not a correctness bug.

Known limitations: `cow_release` only makes the *count* atomic. The
surrounding page-table edit (the PTE's Writable/CoW bit flip) is a separate
critical section in a different lock domain (`AddressSpace`'s own page-table
walk), so a parent re-forking an already-shared page can still narrowly race
a sibling's concurrent CoW resolution of that frame before the next fault
re-synchronizes the two page tables. Closing that fully needs the per-frame
lock held across the PTE mutation too — flagged here as a follow-up
alongside the brief's own SMP TODOs, not solved in this checkpoint.

## `vma` / `AddressSpace` — VMA list and anonymous mmap

Performance rationale: a fixed 32-entry `[Option<Vma>; MAX_VMAS]` array
avoids any allocator dependency; `mmap_anon` does a linear first-fit scan of
both the array (for a free slot) and existing VMAs (for overlap) since this
checkpoint never expects more than a handful of mappings per process.

x86_64 tricks used: PROT_*/MAP_* flag translation mirrors the existing
ELF-segment-flags-to-`PageFlags` translation already proven for `execve`
(`PF_W`→Writable, absence of `PF_X`→`NO_EXECUTE`), so `mmap`'s translation in
`prot_to_page_flags` is the same shape as `user_flags_from_elf`.

Cache behavior: `vmas`/`mmap_next` live inline in `AddressSpace`, no
separate allocation or indirection; `find_vma` is `O(MAX_VMAS)` linear scan,
acceptable at this scale.

Implementation: `crates/proc/src/vma.rs` defines `Vma`, the `PROT_*`/`MAP_*`/
`SYS_MMAP`/`SYS_MUNMAP` constants, and `sys_mmap`/`sys_munmap`. The VMA
storage, `find_vma`, `mmap_anon`, and `munmap` live on `AddressSpace` itself
in `crates/proc/src/address_space.rs` (since they need direct access to the
page-table walk helpers that are private to that module).
`AddressSpace::fork_cow` also copies the parent's VMA array into the child
by value as part of the same walk that CoW-shares the underlying frames.
`munmap` requires an exact `[addr, addr+length)` match against a prior
`mmap_anon` call (no splitting/partial unmap), matching the brief's stated
checkpoint scope.

Known limitations: `MAP_FIXED`, file-backed mmap, and `MAP_SHARED` are all
explicitly rejected with `EINVAL`/`ENOSYS` rather than silently
misinterpreted. W^X is not enforced (`PROT_WRITE|PROT_EXEC` together is
allowed) — accepted scope per the brief, not a bug.

## `pagefault` — vector 14 dispatch

Performance rationale: the handler does the minimum work to distinguish its
three cases from `error_code` bits alone before touching any process state,
and never calls `schedule()` — it returns normally so the common IDT stub's
`IRETQ` re-executes the faulting instruction.

x86_64 tricks used: `CR2` is read exactly once at handler entry via
`kernel_paging::tlb::read_cr2()` (`mov rax, cr2`), before any other memory
access, per the brief's ordering requirement; `PF_PRESENT`/`PF_WRITE`/
`PF_USER` are decoded straight from the hardware-supplied `error_code` in
`InterruptFrame`.

Cache behavior: not relevant beyond what's already covered by `cow`'s bucket
locks and the page-table direct-map accesses in `AddressSpace`.

Implementation: `crates/proc/src/pagefault.rs` registers `pf_handler` for
`PF_VECTOR = 14`. Case 1 (present + write) tries
`AddressSpace::resolve_cow_fault`; case 2 (not-present) tries
`resolve_anon_fault`; case 3 (everything else, or either resolver returning
`false`) calls `(*proc).exit(-11)` for a user-mode fault, or panics only for
the genuine kernel bug of `current_process()` returning null. The actual
PTE-walk-and-fix-up logic for both cases lives on `AddressSpace` in
`address_space.rs` (`resolve_cow_fault_inner`, `resolve_anon_fault`) for the
same "needs the private page-table helpers" reason as `vma` above;
`pagefault.rs` itself keeps only the vector-14 entry point, the CR2 read,
and the two small pure-flag-transform helpers (`cow_writable_flags`/
`cow_readonly_flags`) that both the fault handler and `fork_cow` share.

Known limitations: same SMP caveat as `cow` — `flush_page()` only
invalidates the local CPU's TLB, which is sound today (one thread per
process, no `CLONE_VM`) but will need a `flush_pcid()` broadcast IPI once
multi-threaded processes exist. File-backed demand paging is out of scope;
an unresolvable not-present fault simply falls through to case 3.

## `fork` — sys_fork core logic

Performance rationale: `fork()` does not touch the parent's kernel stack or
call chain at all — it is an ordinary Rust function call/return on the
parent's existing thread. The child gets a brand-new kernel stack via
`kernel_sched::alloc_kernel_thread_raw`, never a raw copy of the parent's
stack bytes, per this checkpoint's carried-over invariant (RBP/return-address
chains inside a copied stack would point at the *old* stack's direct-map
virtual address).

x86_64 tricks used: the child resumes via the same
`ring3_entry_trampoline`/`enter_user_mode` path already proven for
`Process::create`, with `cr3` built from `child_as.pml4_phys | child_as.pcid`
exactly as `ring3_entry_rust` already does for any process.

Cache behavior: not relevant; this is cold, infrequent control-plane code.

Implementation: `crates/proc/src/fork.rs`'s `sys_fork()` reads
`kernel_cpu::current_syscall_frame()` for the parent's user RIP/RSP/RFLAGS,
calls `parent.address_space.fork_cow()`, duplicates the fd table via
`FdTable::clone_for_fork()`, allocates a new PID/thread/`Process`, copies the
syscall frame into the child's `user_rip`/`user_rsp`/`user_rflags` with
`user_rax: 0`, registers the child in `PROCESS_TABLE`
(`crate::syscall::register_process`), and enqueues its thread
(`kernel_sched::enqueue_thread`) — mirroring whatever `Process::create`
already does for getting a fresh thread scheduled, since nothing additional
was needed beyond that existing path. The parent's own return value is the
new PID, exactly like a normal function return — `fork()`'s child path is
entirely driven by the scheduler eventually running the new thread, not by
anything in this function's control flow.

Known limitations, as called out inline in `fork.rs`'s doc comment on
`FdTable::clone_for_fork`: each duplicated fd gets its own independent
`offset` field rather than sharing one via a refcounted file-description
object, so parent/child seeks on inherited fds diverge immediately after
`fork()` — unlike real POSIX semantics, where they stay mutually visible
until one side reopens the file. `kernel-proc` also cannot reach into
`kernel-fs` to increment a VNode refcount on each duplicated fd (the
dependency would cycle the other way), so that accounting is left as a
documented gap rather than worked around with a layering violation.

## `kernel-cpu`'s syscall-frame snapshot (Part 0)

Implementation: `crates/cpu/src/syscall.rs` adds `SyscallFrame { user_rip,
user_rsp, user_rflags }`, a `[SyscallFrameSlot; MAX_SYSCALL_CPUS]` of
`AtomicU64` triplets, and `current_syscall_frame()`/
`update_current_syscall_frame()`. The trampoline (both the plain and
`amd_lfence_sysret` variants) calls `update_current_syscall_frame` with the
user RIP (saved `rcx`), the saved user RSP (`gs:[cpu_rsp_user]`, written
moments earlier in the same prologue), and the user RFLAGS (saved `r11`) —
immediately after the existing push sequence and *before* `call
syscall_dispatch`, so the snapshot is always one syscall ahead of whatever
`fork()` might read concurrently from another CPU servicing a different
process's syscall. Verified by
`syscall::tests::syscall_frame_update_roundtrip_on_host`.

Known limitations: none beyond what's already covered above — this is a
narrow, self-contained addition with no further scope.

## QEMU-observable demonstration

`initrd/init2.asm` (still launched the same way, via `init.asm`'s
`SYS_EXECVE("/init2.elf", ...)`) was extended in place, keeping the original
14-byte `"execve works!\n"` smoke test as step 0, then adding:

1. **Anonymous mmap + demand-zero + write/read-back.** `sys_mmap(0, 4096,
   PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1)`, then a *read*
   before any write (must observe `0` — proves the demand-zero fault path,
   not just "didn't crash"), then a write of `0xAB` and a read-back compare
   (proves the installed PTE is actually Writable), then `sys_munmap`.
2. **`fork()` + CoW**, using a one-byte `.data` global (`shared_byte`,
   seeded to `0x42` before the fork). Both the parent and the child
   independently read `shared_byte` immediately after `fork()` returns and
   compare against `0x42` *before* either writes to it — this is a real
   correctness check on `fork_cow`'s copy step and on `resolve_cow_fault`'s
   "copy old content, don't hand back a stray zero frame" behavior, not a
   tautology. Each side then writes its own marker (`'P'`/`'C'`) and
   confirms the write stuck.

Every check branches on a `cmp`/`jne` against a concrete expected byte and
prints a distinct OK/FAIL line for that specific check — see the assembly
for exact register usage (Linux syscall ABI: args in `rdi`/`rsi`/`rdx`/`r10`/
`r8`, `rax` = syscall number; `r10` carries `mmap`'s `flags` argument rather
than `rcx`, since `rcx` is clobbered by the `SYSCALL` instruction holding the
return RIP). The binary was built with the project's existing
`initrd/Makefile` (`nasm` → `ld -static -Ttext-segment=0x400000 ...` → `cpio
-o --format=newc`) and the resulting `init2.elf`/`initrd.cpio` were
disassembled and traced instruction-by-instruction to confirm every syscall
argument register, jump target, and string length (`equ $-label`-computed,
so exact) matches the intended design before being committed.

### Expected serial output

With everything working, one full boot of this initrd produces, in order:

```
Hello Rumicos
execve works!
mmap: demand-zero page reads 0 before any write - OK
mmap: write 0xAB then read back 0xAB - OK
```

followed by — in whichever order the scheduler happens to run the two
threads, since there is no `wait()`/IPC syscall to force an ordering —
four more lines, two from each process:

```
parent: shared_byte inherited as 0x42 - OK
parent: CoW write resolved, shared_byte='P' - OK
```

and

```
child: shared_byte inherited as 0x42 - OK
child: CoW write resolved, shared_byte='C' - OK
```

with the kernel otherwise idle afterward (both processes spin at their own
`SYS_EXIT(0)` → zombie state; nothing else in this checkpoint reaps them).
Any `FAIL` line, a kernel panic, a triple fault, or this initrd's output
simply stopping partway through are all directly attributable to a specific
broken step (the line immediately before the gap), which is the point of
checking each step explicitly rather than only checking for the absence of
a crash.

**This was not booted under real QEMU in the sandbox this work was done
in.** The sandbox's network is restricted to a vetted allowlist that does
not include `rust-lang.org`/`static.rust-lang.org`, so the project's pinned
`rust-version = "1.97"` toolchain could not be installed; the newest
available via `apt` was `rustc`/`cargo` 1.91, sufficient for every
`#![cfg(test)]` code path (the actual host-target test suite — 152 tests,
all passing) but not for cross-compiling the `x86_64-unknown-none` kernel
binary itself, which needs `-Z build-std` (nightly-only) even to build
`core` for that target. That `build-std` attempt was made anyway (via
`RUSTC_BOOTSTRAP=1`) and failed with an LLVM backend error ("Do not know how
to soften this operator's operand") while compiling `compiler_builtins`
under this kernel's `code-model=kernel`/`no-redzone` flags — an
environment/toolchain-version limitation (likely fixed in the project's
actual pinned 1.97 toolchain, which has a newer bundled LLVM), not a defect
in this checkpoint's code. The expected-output trace above was produced by
manually tracing the verified assembly against the verified Rust
fault/fork/mmap logic, the same way the parent prompt's own "describe
expected serial output" framing anticipated for a written deliverable.

## Test totals

`cargo test --workspace` (host target, all `#![cfg(test)]` code): **152
passing, 0 failing**, up from the 126 baseline noted at the top of this
checkpoint's brief (60 of those 152 are in `kernel-proc` alone, covering
`cow`, `vma`, `pagefault`, `fork`, and `address_space`). Scoped per-crate with `--no-deps --tests` (so `-D warnings` sees the same
surface a real `cargo clippy --workspace -D warnings` run exercises),
`kernel-cpu`, `kernel-memory`, `kernel-sync`, `kernel-apic`, `kernel-smp`,
`kernel-ipc`, and `kernel-arch-x86_64` are all completely clean. `kernel-proc`
itself is clean *except* for one pre-existing finding this checkpoint did
not introduce and is not in its assigned file list: `ustack.rs`'s
`assert!(USTACK_TOP < 0x0000_8000_0000_0000)` compares two `const`s, so
clippy's `assertions_on_constants` lint (correctly) flags it as dead code
that the compiler will optimize away. Two further crates this checkpoint
touches but does not own outright have their own pre-existing,
likewise-untouched findings: `kernel-sched` has a `needless_range_loop` in
`percpu.rs` and two missing `# Safety` docs on `switch.rs`'s
linked-but-unimplemented `switch_context`/`switch_first` stubs;
`kernel-paging`'s `entry.rs` defines `make_cr3` after its own `mod tests`
block, which triggers `items_after_test_module`. `kernel-fs` (not touched by
this checkpoint at all) has substantially more lint debt of its own
(unused imports/constants, a few missing `# Safety` annotations, a
`div_ceil` reimplementation) — left entirely alone, as expected for a crate
outside this checkpoint's scope. None of the above is reachable from, or
caused by, anything this checkpoint added.
