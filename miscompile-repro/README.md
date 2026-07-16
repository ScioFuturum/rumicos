# miscompile-repro

Isolated reproduction + detection tooling for the rustc 1.97.0 (LLVM 22.1.6)
aggregate-copy miscompile documented in [`../docs/miscompile-audit.md`](../docs/miscompile-audit.md).

**This crate is deliberately NOT part of the kernel workspace** (its
`Cargo.toml` has an empty `[workspace]` table), so `cargo check --workspace`
and `cargo xtask qemu-test` at the repo root never touch it.

## Contents

- `src/lib.rs` — 13 `case_*` functions, each replicating one aggregate-copy
  shape from the kernel (including the three historically confirmed
  miscompiles and the still-live `name: parent.name` fork copy).
- `src/main.rs` (`repro` bin) — runs every case on the **host** with the
  kernel-mirror release profile and reports runtime corruption.
- `src/bin/scan_asm.rs` — scans an llvm-objdump disassembly for the
  miscompile's signature (runs of register-source `movb` stores whose
  displacements step by 2 — the "even bytes only" shredded copy).
- `run-matrix.sh` — the whole matrix: host run, per-profile target builds,
  RUSTFLAGS bisection, and a scan of the shipped kernel ELF.

## Result summary (2026-07-15, rustc 1.97.0 2d8144b78)

- Host (x86_64-pc-windows-msvc), all profiles: **clean**.
- x86_64-unknown-none **with the kernel's `-C target-feature=+sse4.2`**
  (and `+soft-float` still on, the target default): `[u32; N<=32]` copies
  (cases a, b, e, f8, f16) are emitted as **byte stores at stride-2
  offsets** — data smeared/dropped. Reproduces at every opt-level (0–3),
  with and without LTO, any codegen-units.
- Same build with `+sse4.2,-soft-float`, or without `+sse*`: **clean**.
- The shipped kernel ELF contains the same shredding live in
  `sys_fork` / `proc_syscall_handler` (`Process.name` copies) — see
  `../docs/miscompile-evidence/`.

Trigger, in one line: **`+sse*` target-features combined with the
x86_64-unknown-none default `+soft-float` break LLVM 22.1.6's scalarized
aggregate-copy lowering.**
