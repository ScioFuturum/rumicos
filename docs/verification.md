# Verification summary

How Rumicos is checked, and the current evidence. Two independent layers plus
targeted negative controls that prove specific tests actually detect the bug
they target. Run both layers with `scripts/ci.sh` (or `scripts/ci.ps1`).

## Layer 1 — host unit tests

Pure-logic cores (allocator arithmetic, page-table/ELF encoding, parsers,
signal and scheduler decision state machines, protocol/PCI field decoding) are
deliberately separated from unsafe hardware-facing code so they run on the
host with no target or emulator. `cargo test --workspace`.

Current counts (kernel workspace, all passing):

| Crate | Tests | Covers |
|---|---:|---|
| kernel-proc | 146 | processes, fork/CoW, wait4 (incl. `child_exit_seq` lost-wakeup logic), fd table, address space, exec |
| kernel-fs | 42 | ramfs, VFS, pipe ring + pipe/keyboard-style generation-counter logic, devfs, CPIO |
| kernel-apic | 34 | PIC remap/mask, I/O APIC redirection math, ISO decode, vector non-collision |
| kernel-sched | 25 | run queue / MLFQ, work-stealing cursor, wait queue, FPU-area asserts |
| kernel-keyboard | 22 | Scancode Set-1 decode, ring buffer, keystroke generation-counter logic |
| kernel-virtio | 17 | virtio caps, feature negotiation, virtqueue index wrap, header/desc sizes |
| kernel-paging | 16 | page-table entry encoding, CR3, MMIO-map math |
| kernel-smp | 13 | MADT parse (CPU, I/O APIC, ISO entries) |
| kernel-memory | 12 | buddy allocator, PCID |
| kernel-pci | 10 | config address packing, BAR sizing, 64-bit BAR, MSI-X encode |
| kernel-cpu | 8 | GDT/TSS/IDT, syscall frame |
| kernel-ipc | 2 | — |
| **Total** | **347** | |

The userspace shell's pure command parser adds **21** more in its own workspace
(`cd userspace && cargo test -p shell`).

## Layer 2 — boot regression under QEMU

`cargo xtask qemu-test` boots the real kernel image (Limine/UEFI on q35, KVM
where available else TCG) and matches its serial output, in order, against
`tests/expected-boot.txt`. This is the only check that exercises the boot
handshake, page tables, SMP bring-up, context switching, and the userspace
path end to end.

**Currently 38 expected lines match**, covering: execve of a real ELF; `mmap`
demand-zero and file-backed writeback; signal delivery + `sigreturn` +
callee-saved preservation; `pipe`/`dup2`; `fork`/`wait4`/CoW; **FPU/SSE state
surviving a context switch**; PCI enumeration finding the virtio device with a
decoded 64-bit BAR and an MSI-X capability; virtio-net MAC read and a
transmitted Ethernet frame confirmed via the used ring; and the userspace
shell's self-test driving real pipelines and redirection.

## Negative controls (proving a test has teeth)

A test that passes both with and without the fix proves nothing. Where a
deterministic negative control is honestly constructible, it is recorded here.

- **FPU/SSE save-restore — has a real negative control.** init2 seeds xmm0–3
  with a sentinel before `fork`, the child loads a different pattern, and after
  `wait4` forces a switch away and back the parent checks its xmm survived.
  Disabling the `xsave64`/`xrstor64` pair in `schedule()` (verified by editing
  it out and rebooting) flips the line to
  `fpu: xmm CLOBBERED across context switch - FAIL` and **fails qemu-test**;
  restoring it returns `fpu: xmm survived context switch - OK` and 38/38. So the
  test genuinely detects a missing save/restore.

- **Lost-wakeup fixes (wait4, pipe, keyboard) — no honest boot-level negative
  control while pinned, by design.** The race needs two threads on different
  CPUs, which cannot happen while every thread is pinned to CPU0 (Part C, the
  reserved SMP work). So reverting these fixes does **not** make qemu-test fail —
  a boot-level negative control here would be theatre, not evidence. Instead the
  value is (a) provably-correct decision logic, tested deterministically on the
  host: the generation counter flips exactly when a real ring publish / child
  exit / keystroke happens (`child_exit_seq`, pipe `read_seq`/`write_seq`,
  `KBD_SEQ` tests), and (b) no regression of the existing single-CPU behaviour
  (the pipe-heavy shell self-test stays green). This limitation is stated rather
  than papered over.

## Compiler-miscompile scan

Beyond the two layers, the build scans its own compiled kernel for the
stride-2 scalarized-aggregate-copy pattern that three separate rustc miscompiles
produced during development (see `docs/miscompile-audit.md`). The target's
`-sse*` disable in `.cargo/config.toml` is the fix; the scan is the guard.
