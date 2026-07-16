#!/usr/bin/env bash
# Reproduction matrix for the rustc 1.97.0 / LLVM 22.1.6 aggregate-copy
# miscompile (docs/miscompile-audit.md). Run from miscompile-repro/.
#
# Requires the llvm-tools rustup component (for llvm-objdump).
set -u

OBJDUMP=$(ls "$HOME"/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-objdump* 2>/dev/null | head -1)
[ -n "$OBJDUMP" ] || { echo "llvm-objdump not found (rustup component add llvm-tools)"; exit 2; }

scan() { # build lib for x86_64-unknown-none, disassemble, count shredded runs
    local label="$1"; shift
    cargo build --lib --target x86_64-unknown-none --no-default-features "$@" >/dev/null 2>&1 \
        || { echo "$label: BUILD FAILED"; return; }
    local profdir="release"
    for a in "$@"; do case "$a" in --profile) :;; rel-*) profdir="$a";; esac; done
    "$OBJDUMP" -d --no-show-raw-insn "target/x86_64-unknown-none/$profdir/libmiscompile_repro.a" > target/scan-tmp.s 2>/dev/null
    ./target/release/scan_asm.exe target/scan-tmp.s | tail -1 | sed "s/^/$label: /"
}

echo "== 1. host runtime check (kernel-mirror profile: opt3, fat LTO, cgu=1, panic=abort)"
cargo build --release --bin repro >/dev/null 2>&1 && ./target/release/repro.exe
cargo build --release --bin scan_asm >/dev/null 2>&1

echo
echo "== 2. x86_64-unknown-none codegen scan (kernel rustflags from .cargo/config.toml)"
scan "release(kernel-mirror)"
for p in rel-o2 rel-o1 rel-o0 rel-nolto rel-thinlto rel-cgu16; do
    scan "$p" --profile "$p"
done

echo
echo "== 3. flag bisection (RUSTFLAGS override disables .cargo/config.toml flags)"
RUSTFLAGS="" scan "no kernel rustflags"
RUSTFLAGS="-C target-feature=+sse2" scan "+sse2 alone"
RUSTFLAGS="-C target-feature=+sse4.2" scan "+sse4.2 alone"
RUSTFLAGS="-C target-feature=+cmpxchg16b" scan "+cmpxchg16b alone"
RUSTFLAGS="-C target-feature=+xsave" scan "+xsave alone"
RUSTFLAGS="-C code-model=kernel" scan "code-model=kernel alone"
RUSTFLAGS="-C target-feature=+sse4.2,-soft-float" scan "+sse4.2,-soft-float"
RUSTFLAGS="-C code-model=kernel -C force-frame-pointers=yes -C no-redzone=yes -C relocation-model=static -C target-feature=+cmpxchg16b,+sse4.2,+xsave,-soft-float" \
    scan "ALL kernel flags + -soft-float"

echo
echo "== 4. shipped-kernel scan (if the kernel ELF has been built)"
KERNEL=../target/x86_64-unknown-none/release/kernel
if [ -f "$KERNEL" ]; then
    "$OBJDUMP" -d --demangle --no-show-raw-insn "$KERNEL" > target/kernel-full.s
    ./target/release/scan_asm.exe target/kernel-full.s
else
    echo "kernel ELF not found — run 'cargo kbuild --release' at the repo root first"
fi
