# Rumicos

A from-scratch x86_64 operating system kernel written in Rust, developed toward secure network infrastructure use — routers, firewalls, and other network appliances where a small, auditable, memory-safe codebase matters more than a broad application ecosystem.

Rumicos is not a fork or derivative of Linux, BSD, or any existing kernel. Every line of the kernel is original work.

## Why

Network-facing code parses untrusted input by definition, which makes it a primary source of memory-safety vulnerabilities in conventional C-based network stacks. Rumicos is built in Rust, where memory safety is a structural property of the language rather than a matter of review discipline.

Because the system targets network appliances rather than general-purpose computing, it does not need to carry the driver surface, desktop stack, or application compatibility layers that make general-purpose kernels large and difficult to audit.

## Current status

The kernel foundation is complete and verified on real hardware under KVM. The network stack is the current development focus and is **not yet implemented**.

| Layer | Status |
|---|---|
| Boot | Limine protocol, UEFI, higher-half kernel |
| CPU | GDT / TSS / IDT, SYSCALL/SYSRET, per-CPU state via GSBASE |
| Memory | 4-level paging, direct-map, PCID, NUMA-aware buddy allocator, SMAP/SMEP |
| SMP | ACPI/MADT parsing, AP bring-up, x2APIC/xAPIC with fallback, TLB shootdown |
| Scheduling | Preemptive MLFQ, per-CPU run queues, work-stealing, futex, mutexes |
| Processes | ELF64 loader, ring-3 execution, copy-on-write `fork`, `execve`, `CLONE_VM` |
| Memory mapping | Anonymous and file-backed `mmap`, `MAP_SHARED`, page cache, demand paging |
| Filesystem | VFS, ramfs, devfs, CPIO initrd |
| IPC | `pipe`, `dup`/`dup2`, POSIX signals, `wait4` |
| Userspace | Interactive shell with pipelines and redirection |
| **Network stack** | **Not implemented — in development** |
| **Packet filtering** | **Not implemented — planned** |
| **Persistent storage** | **Not implemented — planned** |

## Verification

Two independent layers:

- **Host unit tests** — pure-logic cores (allocator arithmetic, page-table encoding, signal state machines, parsers) are deliberately separated from unsafe hardware-facing code so they can be tested without hardware.
- **Boot regression under QEMU** — `cargo xtask qemu-test` boots the real kernel image and checks serial output against an ordered expected-output file. This is the only check that exercises boot handshake, page tables, SMP bring-up, and context switching.

The project additionally scans its own compiled binary for known-bad code generation patterns. This exists because three distinct compiler miscompilation bugs were found during development, each of which silently corrupted data and was only detectable by inspecting generated machine code — see `docs/miscompile-audit.md`.

## Building

Requires a nightly Rust toolchain pinned by `rust-toolchain.toml`, plus NASM and a linker for the userspace binaries.

```sh
rustup target add x86_64-unknown-none
cargo check --workspace --target x86_64-unknown-none
cargo test --workspace
```

## Running

```sh
cargo xtask qemu          # interactive boot, serial console
cargo xtask qemu-test     # headless boot regression check
```

KVM acceleration is used automatically on Linux when available; other hosts fall back to TCG emulation.

## License

Rumicos is free software licensed under the [GNU General Public License v3.0](LICENSE).

Commercial licensing is available separately for organizations that cannot accept the obligations of the GPL. Contact the maintainer for terms.

Contributions require a Contributor License Agreement — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgments

- [Limine](https://github.com/limine-bootloader/limine) for the boot protocol
- The OSDev community for x86_64 architecture reference material
