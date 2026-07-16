# Full CI check: host unit tests, then the QEMU boot-regression test.
# Windows runs QEMU under TCG (no KVM), so qemu-test is slower here —
# this script is for the development loop; the primary CI target is Linux.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

cargo test --workspace -- --test-threads=1
if ($LASTEXITCODE -ne 0) { exit 1 }

cargo clippy --workspace -- -D warnings
if ($LASTEXITCODE -ne 0) { exit 1 }

cargo xtask qemu-test
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "CI: all checks passed"
