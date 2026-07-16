#!/bin/sh
# Run pre-built host tests.  New tests in kernel-proc and kernel-fs require
# `cargo test -p kernel-proc -p kernel-fs` to build first.
cd "$(dirname "$0")"
PASS=0
for f in target/debug/deps/kernel_*; do
  [ -x "$f" ] || continue
  name=$(basename "$f" | sed 's/-[0-9a-f]*$//')
  result=$("$f" 2>&1)
  p=$(echo "$result" | grep -o '[0-9]* passed' | awk '{print $1}')
  PASS=$((PASS + ${p:-0}))
  printf "  %-30s %s\n" "$name" "$(echo "$result" | grep 'test result')"
done
echo "─────────────────────────────────────────────────────────────────"
echo "  Pre-built: $PASS tests pass"
echo "  After full build (cargo test): expected 104 (74 + 15 proc + 15 fs)"
