# Benchmark Pipeline

Hot-path benchmarks should use RDTSCP brackets from `kernel-arch_x86_64` and report:

- cycles per operation
- instructions per operation
- L1/L2/L3 cache miss counters
- branch misses
- p50/p99/p999 latency
- producer CAS failure rate for MPSC rings

Linux host command:

```bash
perf stat -e cycles,instructions,branches,branch-misses,cache-misses cargo bench --workspace
```

Kernel/QEMU command shape:

```bash
qemu-system-x86_64 -machine q35 -cpu host -accel kvm -m 2G -serial stdio
```

BOLT post-link flow after a representative workload:

```bash
llvm-bolt target/x86_64-unknown-none/release/kernel \
  -o target/x86_64-unknown-none/release/kernel.bolt \
  -data target/bolt/kernel.fdata \
  -reorder-blocks=ext-tsp \
  -reorder-functions=hfsort+ \
  -split-functions
```

