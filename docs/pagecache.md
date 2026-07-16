# File-Backed mmap, MAP_SHARED, and the Page Cache

`kernel-fs` gains a new `pagecache` module: a bucket-locked, hash-indexed
`(vnode, page-aligned offset) → physical frame` table, mirroring
`kernel-proc`'s `cow` table's own design (which itself mirrors
`kernel-sched`'s futex table) — the third sparse table in this kernel using
the identical locking discipline, per this checkpoint's own carried-over
invariant. `kernel-proc`'s `Vma` gains a `Backing` enum (replacing the old
bare `anon: bool`), `AddressSpace::resolve_file_fault`/`fork_cow`/`munmap`
all learn to handle file-backed pages (both MAP_PRIVATE and MAP_SHARED)
alongside the existing anonymous case, and `sys_mmap` grows to accept a real
`fd`.

## A real bug found and fixed in the previous checkpoint's `cow_release`

Before touching any of this checkpoint's own work, reusing `cow_share`/
`cow_release` for file-backed private mappings (as the brief itself
suggests) required re-deriving the correctness of `cow_release`'s
refcount-boundary logic from scratch — and that derivation turned up a real,
pre-existing bug, not a hypothetical one.

**The bug:** `cow_release`'s previous implementation treated `refcount == 2`
as "this is the last of a pair resolving, safe to reuse the frame in place
(or free it, from a teardown path)." That's backwards. `cow_share` brings a
frame from absent (1, implicit) to 2 in a *single* call representing **two**
real claimants — e.g. a fork's parent PTE and child PTE. The *first* of
those two claimants to actually resolve (via a write fault or a
munmap/exit teardown) sees `refcount == 2`, and at that exact moment the
*other* claimant's PTE is still live and unresolved, still pointing at the
same frame. Treating that first resolution as "sole owner" has two distinct
bad consequences depending on the caller:

- From `resolve_cow_fault_inner` (write-fault resolution): the first writer
  mutates the frame in place while the second, unresolved peer's PTE — still
  present, still read-only — transparently observes the mutation on its next
  *read* (reads never fault against a present read-only page). A CoW
  isolation violation between fork siblings.
- From `unmap_pages`/`free_table` (munmap/exit teardown): the first peer to
  tear down its own mapping frees a frame the *other* peer's PTE **still
  maps** — a dangling page-table entry pointing at memory the buddy
  allocator may hand out to something completely unrelated. Worse than the
  isolation issue: physical-memory corruption, not just a leak.

**The fix:** an entry existing at all — regardless of its refcount — means
at least one other real peer's claim is still outstanding, so any call that
finds an entry must decrement it (removing it outright once it would drop to
the implicit "1" state) and report `false`. Only a call that finds **no**
entry at all — meaning every original claimant has already independently
resolved — may report `true`. This generalizes correctly to any starting
refcount N (verified by
`cow_release_combined_decrement_matches_manual_decrement_sequence` and the
new `cow_release_sequential_two_peer_resolution_first_false_second_true`),
and — not incidentally — is exactly the property this checkpoint's own page
cache needs from the same table (see below).

Two of the previous checkpoint's own tests encoded the old, wrong
expectation and were rewritten rather than deleted:
`cow_release_reports_sole_owner_when_last_pair_resolves` (asserted `true` at
refcount 2; now split into
`cow_release_on_an_existing_entry_always_reports_not_sole_regardless_of_refcount`,
asserting the corrected `false`, and the new sequential-resolution test
above, which walks through *two* calls to show where the eventual `true`
actually comes from) and
`cow_release_combined_decrement_matches_separate_refcount_then_unshare`
(same wrong boundary at the tail of a longer chain; renamed and corrected).
`cow_release`'s own doc comment now walks through the full derivation so a
future reader doesn't have to re-derive it.

## Why this fix is what makes the page cache's own design sound

The brief's own suggestion — call `cow_share` once when a MAP_PRIVATE file
page is first demand-filled, "since this page cache frame is now
effectively shared between the page cache's one logical owner and this
VMA's private CoW copy" — only works if an entry existing can *never* be
mistaken for "the page cache is done with this frame." With the fix, it
provably can't: every private mapper of a cached page (whether from a fresh
`mmap`+fault or from `fork` CoW-copying an already-present private mapping)
calls `cow_share` once for its own claim; the *very first* such call is also
what folds the page cache's own permanent, never-independently-decrementing
interest into the same entry (going straight from absent to 2 covers both
in one call). Because an entry existing always means "false, copy away"
regardless of refcount, no real mapper can ever be told "you're the last
one, reuse the cache's frame in place" — which is exactly the property a
page-cache-backed frame needs: it must never be mutated in place by a
private mapper, no matter how many real mappers have come and gone, since
the cache may hand that same frame to a brand-new mapper at any point in the
future. This is verified end-to-end by
`AddressSpace::resolve_file_fault`/`unmap_file_pages`'s use of the existing,
unmodified `resolve_cow_fault_inner`/`cow_release` machinery — no
file-backed-specific CoW logic needed at all once the underlying primitive
is correct.

## Substitutions from the design brief

- **`Backing::File`'s vnode field is `vnode_ptr: usize`, not a typed `*mut
  kernel_fs::vnode::VNode`.** The brief's idealized `Backing` enum assumed
  kernel-proc could hold a typed VNode pointer directly, but kernel-fs
  already depends on kernel-proc (Cargo doesn't allow the reverse cycle that
  would create) — the exact same constraint `FdEntry.vnode_ptr: usize`
  already navigated. Every place that needs to actually dereference the
  vnode (page-cache fill, dirty-marking, writeback, refcounting) goes
  through registration hooks kernel-fs's `init_fs()` fills in once, mirroring
  `EXEC_LOADER`/`CHAIN_HANDLER`/`SERIAL_VNODE_PTR`'s existing pattern
  exactly: `register_page_cache_hooks`/`register_vnode_refcount_hooks` in
  `kernel_proc::syscall`.
- **`sys_mmap`/`sys_munmap` stayed in `kernel-proc/vma.rs`, not relocated to
  `kernel-fs/syscall.rs`.** The brief flagged this as an open question
  ("wherever sys_mmap/munmap dispatch already lives ... possibly relocated
  to kernel-fs"). It turns out kernel-proc's own `FdEntry` already carries
  `can_read()`/`can_write()` and an opaque `vnode_ptr` — the *only*
  kernel-fs-typed operations `sys_mmap` actually needs are `VNode::inc_ref`/
  `dec_ref`, which the same registration-hook mechanism above covers. So
  `sys_mmap` stayed exactly where it was, gaining an `fd`/`offset`-aware
  branch, rather than moving to a different crate for one refcount call.
- **`sys_mmap`'s `offset` argument is packed into `fd`'s register, not
  threaded through as a genuine 6th syscall argument.** The hand-written
  SYSCALL asm trampoline (`kernel-cpu`'s `syscall_entry`) only ever forwards
  five arguments (Linux's own `rdi`/`rsi`/`rdx`/`r10`/`r8` → `a1..a5`); its
  6th register (`r9`) is popped off the stack and discarded before
  `syscall_dispatch` is ever called. Extending that asm to thread a genuine
  6th argument through is a real SysV calling-convention change (a
  7-argument `extern "C"` call needs a stack argument, with all the
  alignment/cleanup that implies) — exactly the kind of hand-edited
  `global_asm!` change that's easy to get subtly wrong and hard to verify
  without real hardware. Per this checkpoint's own explicitly-sanctioned
  alternative ("pack offset+fd into one register and unpack here"), `a5` is
  packed by the caller as `(offset << 32) | (fd as u32 as u64)` — byte-exact
  (no bits shifted away), so a genuinely unaligned `offset` from userspace
  still reaches `sys_mmap`'s own `EINVAL` check unchanged, at the cost of
  capping `offset` to 4 GiB (`ramfs`'s 1 MiB/file cap makes this a
  non-limitation for the foreseeable future). Verified round-trip by
  `mmap_fd_offset_packing_round_trips`, and traced instruction-by-instruction
  in the demo binary's disassembly (see below) — this bit me once already
  during this checkpoint: the previous checkpoint's own demo asm used
  `mov r8, -1` (a 64-bit sign-extending move, setting *every* bit of `r8`)
  for the anonymous-mmap `fd = -1` case, which under this new packing scheme
  decodes to `offset = 0xFFFFFFFF` — unaligned, and rejected with `EINVAL`
  even though the anonymous path never uses `offset` for anything. Fixed to
  `mov r8d, -1` (32-bit, zero-extends into `r8`), confirmed correct in the
  disassembly this time.
- **`AddressSpace::resolve_anon_fault` was generalized in place and renamed
  to `resolve_file_fault`**, rather than adding a second, near-duplicate
  entry point the way the brief's own `try_resolve_anon_demand` →
  `try_resolve_file_demand` rename suggested (that split doesn't actually
  exist in this tree the way the brief's idealized `pagefault.rs`
  describes: the real not-present-fault resolution logic lives as an
  `AddressSpace` method, not a free function in `pagefault.rs` — `pf_handler`
  itself only holds the vector-14 entry point, the `CR2` read, and two small
  shared flag-transform helpers).
- **The MAP_SHARED "separate write-fault" branch the brief asked to call out
  explicitly is confirmed genuinely unreachable, not merely assumed so.**
  `resolve_file_fault` maps a MAP_SHARED page writable (and dirty-marks it)
  on its very first fault whenever the VMA itself permits writes, so there's
  no "read-only then upgrade" state for such a page to ever occupy — a
  present+write fault on one only happens when the VMA genuinely forbids
  writes, which correctly falls through to `SIGSEGV`. Documented as an
  explicit comment in `pf_handler` rather than added as dead code.
- **Writeback-past-EOF: clamp, not grow — verified against `ramfs_write`'s
  actual behavior, not assumed.** `ramfs_write` does grow a file on any
  write whose `offset + len` exceeds its current size (confirmed by reading
  its source, not inferred). A MAP_SHARED page straddling or entirely past
  EOF gets its zero-filled tail dirtied the instant any byte in that page is
  touched (this checkpoint's accepted "any writable touch is dirty"
  simplification — see `resolve_file_fault`'s own doc comment). Writing that
  whole page back verbatim would silently grow the file with a run of zero
  bytes it never legitimately gained — map a file 1000× bigger than its
  actual size, touch one byte, and the file balloons to match on the next
  writeback. `writeback_vnode` instead clamps every writeback to
  `min(PAGE_SIZE, vnode.size - file_offset)` bytes and skips the write
  entirely once `file_offset >= vnode.size` — matching real Unix `mmap`/
  `msync` behavior, where a page mapped past EOF never grows the file on
  writeback; only an explicit `write()`/`ftruncate()` does that. Verified by
  `writeback_clamps_to_file_size_past_eof` and
  `writeback_skips_page_entirely_past_eof`.
- **`evict_vnode` is implemented and tested but never called from anywhere
  in this checkpoint** — exactly as the brief itself anticipates ("only safe
  to call after writeback_vnode() has already run ... this checkpoint's only
  caller is the last-munmap path, AFTER writeback_vnode()"). Tracing the
  actual munmap path found there's no reliable "this was the *last*
  reference anywhere" signal available yet: this crate's own `fd` close path
  (`FdTable::close`) has the identical pre-existing gap — it never calls
  `VNode::dec_ref` at all today, let alone frees the page cache at zero —
  and the previous checkpoint's own `FdTable::clone_for_fork` doc comment
  already flags the sibling gap (fork'd fd copies don't share a refcounted
  file-description object). `munmap`'s file-backed release path therefore
  only ever calls `writeback_vnode` (if shared) then `dec_ref` — never
  `evict_vnode` — matching this same boundary rather than reaching further
  than what was asked.
- **`vnode_dec_ref_shim` deliberately does not call `(vnode.ops.release)`
  even at refcount zero**, for the same reason as the point above — noted
  as a pre-existing gap (the fd-close path has never called this either),
  not silently patched over.

## `pagecache` — the page cache itself

Performance rationale: same as `cow.rs` — a sparse, bucket-locked table
avoids a `struct page`-per-physical-page array (tens of MiB fixed cost on a
hobby kernel for what would be, in practice, a small minority of frames ever
being file-backed and cached at once).

x86_64/systems tricks: none new — this module is bookkeeping plus a direct
read/zero-fill of one 4 KiB frame per miss, using the same direct-map
addressing convention the rest of this kernel already uses.

Cache behavior / hashing: `bucket_index` folds `vnode_ptr` and the offset's
*page number* into one multiplicative (Fibonacci) hash chain, rather than
hashing each part separately and combining the two results afterward —
hashing separately clusters badly for the overwhelmingly common case of one
file's offsets being sequential multiples of 4096. Verified by
`bucket_index_spreads_sequential_offsets` (16 sequential pages of one file
must land in more than 4 distinct buckets, not collapse into one).

Implementation: `crates/fs/src/pagecache.rs`. `get_or_fill` is a cache
hit-or-miss lookup with an explicit double-insert race guard (two CPUs
racing the exact same miss both do the I/O, but only the first to reacquire
the bucket lock's insert wins — the loser frees its own redundant frame and
returns the winner's, so page-cache entries stay unique per key). The
byte-level fill logic (`fill_page_from_vnode`: read, zero-fill any short
tail) is factored out as a pure function taking an already-valid
destination slice, with no allocator or direct-map dependency at all — this
is what makes it (and, by extension, `get_or_fill` itself) directly
host-testable with a synthetic `VNode` and mock `VNodeOps`, rather than
being entirely `#[cfg(target_os = "none")]`-gated and untested on host the
way e.g. `ramfs.rs`'s own allocation-touching helpers are. Getting real
byte-level host tests for frame content (cache hit/miss call counting, EOF
zero-fill, dirty-byte round-tripping) required the same substitution
`kernel-memory`'s `buddy.rs` (`test_direct_map`) and `kernel-proc`'s
`exec.rs` (`RawPage`) already made for the identical reason: on host, a
"frame" is simply a `std::alloc`'d host pointer's own numeric value, with no
real direct-map translation needed (equivalent to a direct-map base of 0).

`mark_dirty`/`writeback_vnode`/`evict_vnode` all follow the same "never hold
two bucket locks simultaneously, even across a full 256-bucket scan — lock,
inspect, unlock, one bucket at a time" discipline `cow.rs` already
established, since one vnode's cached pages are scattered across buckets by
hash.

`PAGECACHE_BUCKET_CAP`'s overflow contract mirrors `cow.rs`'s own
documented one exactly: a full bucket degrades to "not cacheable" for any
*further* entry at that hash — `get_or_fill` still returns a valid frame for
that one call, it just isn't remembered, so the next lookup at the same key
re-reads from the vnode. A missed sharing optimization, not a correctness
bug.

### A subtlety `get_or_fill`/`writeback_vnode`/`evict_vnode` needed beyond
### what a first pass compiles cleanly

These three functions call frame-provisioning helpers (`raw_alloc_zeroed_frame`/
`raw_frame_bytes_mut`/`raw_free_frame`) that only exist under
`#[cfg(target_os = "none")]` or `#[cfg(all(test, not(target_os = "none")))]`
— meaning a *third* configuration exists where neither is true: a plain host
build of this crate with no `--cfg test` at all, which is exactly what
happens when `kernel-fs` is compiled as a library dependency (e.g. by
`kernel-proc`'s own build graph, or by `cargo clippy -p kernel-fs --no-deps`
without `--tests`). Left unguarded, the three public functions would
reference undefined helpers in that configuration — a genuine compile
error, not just a lint. They're now gated `#[cfg(any(target_os = "none",
test))]` themselves, matching their own dependencies' availability exactly.
The narrower helpers they call internally (`cache_lookup`,
`fill_page_from_vnode`, and the `frame` field on `PageCacheEntry`) get a
targeted `#[cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]`
for the same reason `pagefault.rs`'s pre-existing `cow_writable_flags`/
`cow_readonly_flags` already needed one before this checkpoint touched
anything: by design, they're reachable only from `target_os = "none"` code
or from this crate's own dedicated tests, and a *lib-only, non-test* host
build of the crate that defines them (verified to be a pre-existing,
unrelated-to-this-checkpoint characteristic — confirmed by checking that
`cow_writable_flags`/`cow_readonly_flags` already needed this exact same
treatment before any of this checkpoint's own changes existed) has no
reachable call site for them at all, which `-D warnings` would otherwise
report as `dead_code`.

## `Vma`/`Backing` and `AddressSpace` changes (`kernel-proc`)

`Vma.anon: bool` → `Vma.backing: Backing`, where `Backing` is `Anonymous` or
`File { vnode_ptr: usize, file_offset: u64, shared: bool }` (see the
substitution note above for why `vnode_ptr` is opaque). `Vma::is_cow_eligible()`
is `true` for `Anonymous` and MAP_PRIVATE `File`, `false` only for MAP_SHARED
`File` — this is what `fork_cow`'s per-page loop consults.

`AddressSpace::mmap_anon` became a thin wrapper over a new, general
`AddressSpace::mmap(len, flags, backing)`; both are still exposed (the
former for the many call sites — tests, kernel-internal mappings — that
never need a `Backing` at all).

`AddressSpace::resolve_file_fault` (renamed from `resolve_anon_fault`, see
above) now matches on `vma.backing`:

- `Anonymous`: unchanged — `alloc_frame`, zero-fill, map writable.
- `File { shared: true, .. }`: `page_cache_get_or_fill`, map the returned
  frame **directly** — writable if the VMA's own `PROT_WRITE` permits it,
  **never** `PTE_COW` — and, if mapped writable, `page_cache_mark_dirty`
  unconditionally (this checkpoint's accepted "any writable touch of a
  MAP_SHARED page is dirty-on-touch" simplification; the more precise
  alternative — only marking dirty on an observed `PF_WRITE` fault to a
  read-only-mapped shared page — needs a second fault round-trip per page
  and is explicitly deferred, per the brief's own framing).
- `File { shared: false, .. }`: `page_cache_get_or_fill`, map the returned
  frame **read-only + `PTE_COW`** regardless of the VMA's own writable bit,
  and call `cow_share` on it — this is exactly the "fold the page cache's
  permanent interest into the same entry as the first real mapper's claim"
  design the corrected `cow_release` makes sound (see above). The *existing,
  unmodified* `resolve_cow_fault_inner` handles the eventual first write with
  zero file-backed-specific logic at all.

`AddressSpace::fork_cow`'s per-page walk (`clone_user_half_cow`) gained one
early-out, checked before its existing generic CoW-sharing logic: if the
faulting virtual address falls inside a `File { shared: true, .. }` VMA
(found via `find_vma`), the child's PTE is an exact copy of the parent's —
same frame, same permissions, no `PTE_COW`, no `cow_share` call at all.
Every other present page (`Anonymous`, a MAP_PRIVATE `File` page, or a page
outside any VMA entirely — ELF segments, the user stack, for which
`find_vma` returns `None`) keeps the ordinary CoW treatment, byte-for-byte
unchanged from the previous checkpoint.

`AddressSpace::munmap` gained a `Backing`-aware branch:

- `Anonymous`: unchanged, calls the existing `unmap_pages`.
- `File { shared: true, .. }`: `page_cache_writeback` first (flushing every
  dirty page for that vnode), then a new `unmap_file_pages(shared: true)`
  that clears every PTE in the range but **never** calls `free_frame` — by
  construction, a MAP_SHARED page is always exactly the page-cache frame,
  never a private copy — then `vnode_dec_ref`.
- `File { shared: false, .. }`: `unmap_file_pages(shared: false)`, which
  distinguishes a still-`PTE_COW` page (still pointing at the page cache's
  frame — `cow_release` correctly reports "not sole" for as long as the
  cache's own implicit interest, or a fork sibling's real claim, is still
  outstanding, so it's never freed here) from a page whose `PTE_COW` bit is
  already clear (a private write-triggered copy from `resolve_cow_fault_inner`,
  never registered with `cow_share` at all and never page-cache-tracked
  either — always safe to free directly). No new bookkeeping needed for this
  distinction: the existing `PTE_COW` bit *is* the signal.

## Integration diff

- `kernel_proc::syscall` gains `register_page_cache_hooks`/
  `register_vnode_refcount_hooks` and their corresponding `pub(crate)`
  call-through functions, mirroring `EXEC_LOADER`/`CHAIN_HANDLER`/
  `SERIAL_VNODE_PTR`'s existing pattern.
- `kernel_fs::init_fs()` calls both registration functions once, alongside
  its existing `register_exec_loader` call. **This is the one new
  init-time step this checkpoint needed** — contrary to what a first read
  of the brief's own framing ("page cache is purely on-demand, no boot-time
  setup") might suggest at first glance, *registering the callback function
  pointers themselves* is a one-time init step, even though the page cache's
  own data (actual cached frames) remains entirely lazy — nothing is
  allocated or read until a real page fault or `mmap()` call asks for one.
  No other new boot-time call was needed.
- `crates/proc/src/syscall.rs`'s `SYS_MMAP` dispatch arm unpacks the
  packed `fd`/`offset` register (see the substitution note above) before
  calling `vma::sys_mmap`.
- No changes to `kernel-cpu`'s SYSCALL trampoline itself — deliberately
  avoided, per the substitution note above.

## Test totals

`cargo test --workspace` (host target, all `#[cfg(test)]` code): **172
passing, 0 failing** — up from the 152 baseline this checkpoint's brief
noted at the top (11 kernel-arch-x86_64 + 0 kernel-apic + 7 kernel-cpu + 24
kernel-fs + 2 kernel-ipc + 11 kernel-memory + 16 kernel-paging + 71
kernel-proc + 24 kernel-sched + 6 kernel-sync + 0 xtask). 9 of kernel-fs's
24 are new (`pagecache`'s own tests); kernel-proc's 71 (up from 61 after the
`cow_release` fix alone, up from 60 before that) include the `vma`/
`address_space`/`syscall` changes plus the fix-related `cow` test
rewrites — comfortably over the ≥12-new/≥164-total bar the brief set,
without padding: every new test asserts something this checkpoint's own
code needed to get right (the two `cow_release` fix regressions chief among
them).

`cargo clippy -p X --no-deps --tests -- -D warnings` is clean on every crate
this checkpoint touches (`kernel-proc`, `kernel-fs`, plus the untouched-but-
re-verified `kernel-cpu`/`kernel-paging`/`kernel-memory`/`kernel-sync`).
Also verified clean specifically in the narrower "kernel-fs as a lib-only,
non-test host dependency" configuration that originally surfaced the
`page_cache_*` dead-code issue described above (`cargo clippy -p kernel-fs
--no-deps -- -D warnings`, no `--tests`). Pre-existing, unrelated findings
in files outside this checkpoint's scope were left untouched exactly as
before: `kernel-proc/ustack.rs`'s `assert!` on two `const`s, `kernel-paging/
entry.rs`'s `make_cr3` defined after its own `mod tests`, and — newly
enumerated here since this checkpoint's own files sit right next to them —
substantially more pre-existing lint debt in `kernel-fs`'s `ramfs.rs`/
`devfs.rs`/`dentry.rs` (unused imports, a `div_ceil` reimplementation, a
few missing `# Safety` sections, an unneeded `return`) that predates this
checkpoint and isn't reachable from anything it added.

## QEMU-observable demonstration

`initrd/init2.asm` was extended in place again (still launched the same way
via `init.asm`'s `SYS_EXECVE("/init2.elf", ...)`), keeping the previous
checkpoint's execve/anon-mmap/fork-CoW sections intact and inserting a new
section 2 (file-backed MAP_SHARED mmap) between the anonymous-mmap section
and the fork section. A new file, `initrd/testfile.txt` (17 bytes,
`"PAGECACHE-TEST-0\n"`), is bundled into the CPIO archive alongside
`init2.elf` via an updated `initrd/Makefile`.

The new section: `sys_open("/testfile.txt", 13, O_RDWR)`, then
`sys_mmap(0, 4096, PROT_READ|PROT_WRITE, MAP_SHARED, fd, offset=0)` (`fd`/
`offset` packed into `r8` as described above), then three real checks, each
with its own concrete `cmp`/`jne` and a distinct OK/FAIL line:

1. **Initial content check**: the very first byte read *through the
   mapping* must be `'P'` (0x50) — the real first byte of the bundled
   file's content, demand-filled by `get_or_fill` from the actual file via
   the page cache. A stray zero here would mean the file-backed demand-fill
   path silently fell back to anonymous zero-fill, or read from the wrong
   offset/vnode.
2. A byte is written **through the mapping** (`'P'` → `'X'`), then
   `sys_munmap` is called — which must trigger `writeback_vnode` before the
   PTE is torn down, since this is `MAP_SHARED`.
3. **Writeback check**: the file is closed, then re-opened *fresh* (a
   brand-new fd, nothing reused from the just-torn-down mapping) and
   `sys_read` directly — bypassing the discarded mapping and the in-memory
   page-cache frame entirely from this check's point of view. The first
   byte read back must be `'X'`. A match here can only mean the write in
   step 2 actually reached the underlying `ramfs` file's own storage, not
   merely a mapping that's now gone.

The built `init2.elf`/`initrd.cpio` were disassembled and traced
instruction-by-instruction again (see the substitution note above for the
one real bug this caught: the anonymous-mmap section's `fd = -1` register
setup needed fixing for the new fd/offset packing scheme) to confirm every
syscall argument register, packed value, jump target, and string length
(`equ $-label`-computed, so exact) matches the intended design before being
committed.

### Expected serial output

A full boot of this initrd, in order, still produces:

```
Hello Rumicos
execve works!
mmap: demand-zero page reads 0 before any write - OK
mmap: write 0xAB then read back 0xAB - OK
```

followed by the three new file-backed-mmap lines:

```
file mmap: initial content is 'P' from real file - OK
file mmap: munmap writeback landed in file ('X') - OK
```

(only two OK lines print in the success path — `open`/`mmap`/`re-open`
failures each have their own distinct FAIL line instead, printed in place
of continuing the section, and the section always falls through to the
fork section afterward regardless of outcome)

and finally — in whichever order the scheduler happens to run the two
forked threads, since there is still no `wait()`/IPC syscall to force an
ordering — four more lines, two from each process, byte-for-byte unchanged
from the previous checkpoint:

```
parent: shared_byte inherited as 0x42 - OK
parent: CoW write resolved, shared_byte='P' - OK
```

and

```
child: shared_byte inherited as 0x42 - OK
child: CoW write resolved, shared_byte='C' - OK
```

Any `FAIL` line, a kernel panic, a triple fault, or the output simply
stopping partway through are all directly attributable to the specific
check immediately before the gap.

**This was not booted under real QEMU in the sandbox this work was done
in**, for the identical reason as the previous checkpoint: this sandbox's
network doesn't reach `rust-lang.org`, so the project's pinned `rust-version
= "1.97"` toolchain couldn't be installed (the newest available via `apt`
was 1.91 — sufficient for every `#[cfg(test)]` path, insufficient for
`-Z build-std`-ing `core` for `x86_64-unknown-none`, which failed with an
LLVM backend error under this kernel's `code-model=kernel`/`no-redzone`
flags, as before). The expected-output trace above comes from manually
tracing the verified, disassembled assembly against the verified Rust
fault/mmap/writeback logic — the same honest framing as the previous
checkpoint's own demo section, not represented as an actual QEMU run.

## Known limitations and next steps

- **Page-cache eviction under memory pressure**: not implemented.
  `evict_vnode` exists and is tested but has no caller in this checkpoint
  (see the substitution note above on why — no reliable "last reference"
  signal exists yet). Cached pages simply persist forever once created.
- **`msync()` syscall**: not wired up. `writeback_vnode` is already the
  right body for it; only the syscall-number dispatch arm is missing.
- **Reverse mapping / precise dirty tracking**: this checkpoint's
  "any writable touch of a MAP_SHARED page is dirty" simplification means a
  page that's mapped writable but never actually written still gets flushed
  on `munmap`/writeback (a wasted, harmless write-back of unchanged content,
  not a correctness issue — `ramfs_write` is idempotent for identical
  bytes). A future checkpoint could track real per-page write faults instead,
  at the cost of a second fault round-trip per page (read-only first,
  upgrade to writable only on an observed `PF_WRITE`).
- **mmap'd binary execution / demand-paged execve**: `do_execve` still
  eagerly loads the whole binary into a bounce buffer rather than mapping it
  through this new page cache. A future checkpoint could unify the two.
- **`fd`-offset sharing via a refcounted `FileDescription`**: unchanged from
  the previous checkpoint's own noted gap — each duplicated fd (via `fork`)
  still gets its own independent offset field.
- **Readahead**: `get_or_fill` only ever fills the exact page requested,
  never speculatively fills neighboring pages.
- **Multi-threaded CoW TLB shootdown**: unchanged from the previous
  checkpoint's own noted SMP limitation — still single-thread-per-process
  only, `flush_page` still only invalidates the local CPU's TLB.
