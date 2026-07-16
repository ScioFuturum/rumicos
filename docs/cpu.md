# CPU Bring-Up: GDT, TSS, IDT, SYSCALL

`kernel-cpu` owns CPU-local ABI state. `kernel-arch_x86_64` remains a thin instruction wrapper crate; this crate composes those primitives into boot-time tables, per-CPU stacks, interrupt entry, and syscall entry.

## GDT

Performance rationale: GDT setup is cold boot code, so the implementation favors exact ABI layout over cleverness. Selectors are `pub const u16` so hot assembly paths embed immediates without loads.

x86_64 tricks used: `lgdt`, a `retfq` CS reload sequence instead of far `ljmp`, zeroed FS/GS selectors, and long-mode code descriptors with `L=1, D=0`.

Cache behavior: the GDT is 7 entries and cache-cold after boot. It is `align(16)` to keep descriptor loads naturally aligned while the descriptor structs themselves are packed to match hardware layout.

Implementation: `crates/cpu/src/gdt.rs` exposes `KERNEL_CS`, `KERNEL_DS`, `USER_DS`, `USER_CS`, `TSS_SEL`, `init_gdt()`, and the internal mutable initializer used by `init_cpu()`.

Benchmark plan: no steady-state benchmark; measure boot cycles from `_start` through `init_cpu()` with RDTSCP once serial output exists.

Known limitations: current GDT is a single static table. AP bring-up should move to per-CPU GDTs before concurrent SMP initialization.

## TSS

Performance rationale: TSS is not on the SYSCALL fast path, but valid `rsp0` and IST stacks prevent expensive fault cascades on privilege-transition interrupts and critical exceptions.

x86_64 tricks used: 64-bit available TSS descriptor, `ltr`, 8 KiB 16-byte-aligned kernel stacks, 4 KiB IST1 stacks for #NMI/#DF/#MC.

Cache behavior: TSS and stacks are per CPU. The TSS is cold except for hardware interrupt stack switching; per-CPU stack tops are cached in `PerCpuData` for the syscall trampoline.

Implementation: `crates/cpu/src/tss.rs` defines the 104-byte packed AMD64 TSS and wires `rsp0`/`ist1` into the GDT descriptor before `ltr`.

Benchmark plan: measure interrupt entry latency for user-to-kernel IDT transitions with and without IST assignment.

Known limitations: stack arrays are statically provisioned for 256 CPUs. NUMA-local stack allocation should replace static BSS stacks after the allocator is online.

## IDT

Performance rationale: the IDT load is cold, but entry latency matters. The common path saves only GPRs, avoids XMM/AVX state, and uses one generated common stub reached from 256 generated vector stubs.

x86_64 tricks used: 128-bit interrupt gates, ENDBR64 first in every generated stub, `cld`, conditional `swapgs` when saved CS shows CPL3, IST=1 for vectors 2/8/18, DPL3 only for vector 0x80.

Cache behavior: IDT entries are cold and 16-byte aligned. Handler slots are atomics indexed by vector; the interrupt stack frame is linear and uses only stack cache lines already touched by hardware entry.

Implementation: `crates/cpu/build.rs` generates the 256 assembly stubs and stub pointer table. `crates/cpu/src/idt.rs` fills the IDT, loads it with `lidt`, and dispatches through `register_handler`.

Benchmark plan: measure cycles from vector entry to Rust handler using RDTSCP in a test handler, plus branch misses and i-cache misses under high-rate timer/IPI load.

Known limitations: same-CPL interrupts do not synthesize meaningful `rsp`/`ss` fields in `InterruptFrame`; those fields are hardware-valid on CPL transitions. x2APIC EOI and per-vector fast handlers are next.

## SYSCALL / SYSRET

Performance rationale: SYSCALL/SYSRET avoids IDT lookup and interrupt-frame construction. The trampoline saves only RCX/R11 plus callee-saved registers and leaves XMM/AVX untouched.

x86_64 tricks used: `IA32_EFER.SCE`, `IA32_STAR`, `IA32_LSTAR`, `IA32_FMASK`, `swapgs`, GS-relative per-CPU RSP slots, and optional `lfence` before `sysretq` behind the `amd_lfence_sysret` feature.

Cache behavior: `rsp_user` and `rsp_kern` are separated by a full 64-byte cache line in `PerCpuData`, avoiding false sharing with adjacent per-CPU fields.

Implementation: `crates/cpu/src/syscall.rs` provides the global-asm `syscall_entry`, `init_syscall_msrs()`, and `syscall_dispatch()`. Unknown syscalls return `-38`.

Benchmark plan: use a userspace ping syscall once ring-3 entry exists; target low hundreds of cycles on Zen 4/Sapphire Rapids before real dispatch work.

Known limitations: no userspace entry path yet, no canonical-address SYSRET fallback to IRET, no KPTI/PCID split, and no syscall table beyond `ENOSYS`.

