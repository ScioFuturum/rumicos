#!/usr/bin/env bash
# Full CI check: host unit tests, then the QEMU boot-regression test.
# Runs on Linux (KVM when /dev/kvm is available, TCG otherwise).
set -euo pipefail
cd "$(dirname "$0")/.."

# Unit tests first (single-threaded: several crates' tests share
# process-wide statics and are serialized by in-crate mutexes, but one
# runner thread also keeps the output readable in CI logs).
cargo test --workspace -- --test-threads=1

# Lint gate.
cargo clippy --workspace -- -D warnings

# Boot regression: boots the real kernel under Limine/UEFI in QEMU and
# checks the serial output against tests/expected-boot.txt.
cargo xtask qemu-test

echo "CI: all checks passed"
