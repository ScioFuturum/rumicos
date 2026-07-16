# Architecture notes — performance foundation

> These are the original per-component design notes from the project's first checkpoint
> (the performance-critical foundation the rest of the kernel was built on). Each component
> is described by: performance rationale · x86-64 tricks · cache behavior · Rust
> implementation · benchmark plan · known bottlenecks. Kept for reference; higher-level
> engineering write-ups live alongside this file in `docs/`.

## x86-64 Architecture Layer

1. Performance rationale: hot paths call x86_64 instructions directly instead of hiding them behind portable abstractions. This avoids unnecessary fences, syscalls, MMIO LAPIC access, or generic CPU dispatch.
2. x86_64 tricks: CPUID feature discovery, RDTSC/RDTSCP, PAUSE, CLFLUSHOPT, CLWB, PREFETCHW, PREFETCHWT1, NT stores, FSGSBASE, RDMSR/WRMSR, INVLPG, INVPCID, XSAVE/XRSTOR, XGETBV/XSETBV, and CMPXCHG16B.
3. Cache behavior: cache-line padding is standardized at 64 bytes; explicit writeback and non-temporal stores are available for paths where cache pollution is more expensive than memory bandwidth.
4. Rust implementation: `kernel-arch-x86_64` is `#![no_std]`, uses isolated unsafe blocks for privileged instructions, and exposes narrow typed wrappers.
5. Benchmark plan: measure RDTSCP latency, INVPCID latency, CLWB+SFENCE persistence cost, and PAUSE-loop contention behavior with `perf stat` counters for cycles, branches, branch misses, and cache misses.
6. Known bottlenecks: feature-specific SIMD kernels still need per-call-site runtime dispatch; AVX-512 must be kept out of the scheduler hot path unless XSAVE area growth is justified by measured throughput.

## Synchronization

1. Performance rationale: spinlocks use acquire/release CAS and PAUSE backoff; seqlocks make read-mostly state lock-free for readers.
2. x86_64 tricks: PAUSE prevents destructive speculation in spin loops; PREFETCHW is used before contended lock acquisition.
3. Cache behavior: locks and sequence counters are cache-line aligned to avoid false sharing with protected data.
4. Rust implementation: `kernel-sync` provides `SpinLock`, `Backoff`, and `SeqLock<T: Copy>` with small audited unsafe sections.
5. Benchmark plan: compare uncontended lock/unlock cycles, contended handoff latency, reader seqlock retry rate, and branch miss rate under writer pressure.
6. Known bottlenecks: priority inheritance is not implemented yet; blocking escalation needs scheduler integration.

## IPC Rings

1. Performance rationale: rings pass fixed-size descriptors through shared memory. Payloads stay in shared buffers, so the hot path copies only one cache-line descriptor and avoids kernel entry.
2. x86_64 tricks: PAUSE in producer retry loops; MONITOR/MWAIT wrappers are available for privileged wait paths that want low-latency blocking without syscalls.
3. Cache behavior: producer and consumer cursors live on separate cache lines; every descriptor is exactly 64 bytes to make slot ownership visible at cache-line granularity.
4. Rust implementation: `kernel-ipc` provides SPSC and ordered MPSC bounded rings over `T: Copy`, plus a 64-byte `IpcDescriptor`.
5. Benchmark plan: measure push/pop cycle counts with RDTSCP, p50/p99/p999 latency histograms, producer CAS failure rate, L1/L2 miss rate, and branch misses.
6. Known bottlenecks: ordered MPSC can be head-of-line blocked by a producer that reserves a slot and stalls before publishing.

## Memory

1. Performance rationale: the fast path is a per-CPU slab backed by a CMPXCHG16B tagged free stack, avoiding locks and ABA on object reuse.
2. x86_64 tricks: CMPXCHG16B provides atomic pointer+generation updates; PCID allocation is generation-aware so TLB flushes can be targeted instead of global.
3. Cache behavior: free-list heads are 16-byte aligned for CMPXCHG16B and padded away from unrelated state. Slab objects reuse their own first word as the next pointer, avoiding external metadata in the hot path.
4. Rust implementation: `kernel-memory` provides `AtomicTaggedStack`, `PerCpuSlab`, `NumaBuddy`, and `PcidAllocator`.
5. Benchmark plan: measure alloc/free cycles from the slab, L2 fallback frequency, PCID allocation latency, and cache misses while hammering per-CPU caches.
6. Known bottlenecks: buddy coalescing is intentionally deferred; full NUMA SRAT/SLIT discovery and page metadata are the next layer.

## Kernel Entry

1. Performance rationale: the kernel starts with a tiny higher-half entry that performs CPU feature discovery before any allocator or scheduler work.
2. x86_64 tricks: higher-half kernel code model, RDTSC-capable arch layer, and HALT+PAUSE idle loop.
3. Cache behavior: boot code is minimal and cold; hot subsystems live in separate crates for later BOLT layout work.
4. Rust implementation: `kernel` is `#![no_std]` and `#![no_main]`, linked by `kernel/linker/x86_64.ld`.
5. Benchmark plan: count cycles from `_start` to feature detection completion.
6. Historical note: at the first checkpoint, Limine request parsing, page-table construction, x2APIC setup, ring-3 entry, KPTI/PCID split, and real syscall tables were still to come — all of which the later checkpoints implemented (see the rest of `docs/`).

## CPU Bring-Up

`kernel-cpu` implements GDT, TSS, IDT, per-CPU GS data, and SYSCALL/SYSRET entry. See
[docs/cpu.md](cpu.md) for the per-component performance rationale, x86_64 details, cache
behavior, benchmark plan, and limitations.
