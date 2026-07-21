# Contributing to Rumicos

Thanks for jumping in. This document captures the conventions this project
has settled on so far — read it before your first change, it'll save you
from re-learning things the hard way.

## Before anything else

- `cargo check --workspace --target x86_64-unknown-none` must stay clean
- `cargo clippy --workspace -- -D warnings` must stay clean
- `cargo test --workspace` must stay green
- `cargo xtask qemu-test` must stay green — this is the only check that
  boots the real kernel image under QEMU and verifies serial output
  against `tests/expected-boot.txt`. If your change adds a new boot-time
  demo line, add it there deliberately; if the check fails on a line you
  didn't mean to touch, that's a real regression, not a flake.

Run all of them before opening a PR. `scripts/ci.sh` (Linux) /
`scripts/ci.ps1` (Windows) run the full sequence in one command.

## Coordinating on parallel work

If you're both working at the same time: say out loud (in an issue, a
chat, whatever) which crate or subsystem you're about to touch before
starting. Two people driving independent large changes across the same
files at once is the fastest way to get a merge conflict neither of you
can cleanly resolve — this is *especially* true if either of you is
generating large diffs via an AI coding agent rather than hand-editing,
since those changes tend to touch more of a file at once than a small
manual edit would.

Prefer short-lived feature branches + a quick review of each other's
diff before merging to `main`, even if it's just a five-minute read-
through. Direct pushes to `main` are fine for typo-level fixes.

## Engineering conventions established so far

These aren't arbitrary style preferences — most of them exist because an
earlier version of the code broke a specific way without them. Follow
them for new code in the same areas.

### `unsafe` code
- Every `unsafe` block gets a `// SAFETY:` comment explaining exactly
  why the operation is sound at that call site — not just "this is
  unsafe because Rust says so," but the actual invariant being relied on.
- Every `pub unsafe fn` gets a `/// # Safety` doc comment stating the
  caller's obligations.

### Sparse per-resource tables (locks, futexes, page cache, process
table, CoW refcounts, etc.)
- Use the established pattern: N fixed-capacity buckets, each behind
  its own `SpinLock`, indexed by a Fibonacci hash of the key
  (`key.wrapping_mul(0x9e3779b97f4a7c15) >> (64 - log2(N))`).
- Document the bucket capacity and what happens on overflow (usually:
  degrade gracefully — e.g. skip the optimization, don't panic).
- **Never hold two bucket locks (same table or different tables)
  simultaneously.** If an operation genuinely needs two, document the
  lock order explicitly as a comment at the top of the file, and never
  violate it elsewhere.
- Never call a blocking operation (`thread_block`, anything that can
  call `schedule()`) while holding *any* lock. This has been a real bug
  more than once — grep for `thread_block` call sites and confirm no
  lock guard is still in scope before you add a new one.

### Aggregate array/struct copies
- This codebase has hit **three separate rustc 1.97.0 miscompiles**,
  all in the same family: certain whole-array or whole-struct copies
  (`let x = *guard`, `arr[i] = SomeStruct { .. }`, `dst.field = src.field`
  where the field is a fixed-size array) were silently corrupted by the
  compiler under specific target-feature flags. The `-soft-float`
  target-feature fix (see `docs/miscompile-audit.md`) closed the known
  trigger, but if you're touching code with a similar shape, run it
  through `miscompile-repro/scan_asm.rs` against the built kernel binary
  before trusting it, especially if the array/struct involved contains
  a pointer field.

### Pure logic vs. unsafe glue
- Wherever possible, extract the actual decision-making logic into a
  small function with **zero `unsafe`, zero syscalls, zero hardware
  access** — testable with a plain `cargo test` on the host. Established
  examples: `decide_next_signal`, `reader_step_when_empty` /
  `writer_step_when_full` (pipe), `parse_line` (shell). The unsafe,
  target-only code around it should just mechanically execute what the
  pure function decided.
- This is why the test suite has grown past 270 tests without most of
  them ever touching real hardware — keep that ratio up in new code.

### No heap, fixed capacities everywhere
- No `alloc` crate anywhere in kernel or userspace code. Every buffer
  is a fixed-size array with a named capacity constant
  (`MAX_CHILDREN`, `PIPE_BUF_SIZE`, `MAX_ARGS`, etc.) — not a magic
  number inline.
- If a structure needs to grow past what fits in one 4 KiB frame,
  split it the way `RamfsInode`/`RamfsChildPage` did: a compact
  control-block frame plus a separate data frame, linked by a stored
  physical address. Don't reach for a different pattern without a
  reason.

### Dependencies
- The project has stayed at effectively zero external crate
  dependencies by design (everything from the buddy allocator to the
  QEMU test harness is hand-rolled). Don't add one without discussing
  it first — there's almost always a reason a prior checkpoint didn't
  reach for an existing crate.

## How most of this codebase actually got written

A lot of Rumicos was built by writing a very detailed technical
specification (architecture, exact function signatures, the specific
x86_64 instruction sequences involved, edge cases, test requirements)
and handing it to an AI coding agent with access to the real repository
and a compiler, then reviewing the diff it produced against the spec
before merging. If you want to work the same way: the conventions above
are exactly what such a spec needs to reference so the agent doesn't
reinvent a pattern that's already established elsewhere in the tree —
point it at this file and at the relevant existing crate before it
starts. If you'd rather hand-write code directly: the same engineering
invariants above still apply, just applied by you instead of an agent.
Either is fine — the tests and CI don't care how a diff was produced,
only whether it's correct.

## When you find a bug in existing code

This project has a track record of finding real, subtle bugs (SMP
races, ABI mismatches, a compiler miscompile) through live QEMU/hardware
debugging rather than code review alone. If something looks wrong:
reproduce it under `cargo xtask qemu-test` or manual QEMU first if you
can, rather than guessing from reading the source — several past bugs
here looked correct on paper and were only caught by tracing actual
execution.

## Contributor License Agreement (required)

**Before your first commit**, you need to sign a Contributor License Agreement.

This is not bureaucracy for its own sake. Rumicos is released under GPL-3.0, and
commercial licenses are offered separately to organizations that cannot accept
GPL obligations. That dual-licensing model only works if a single party holds
the rights to the whole codebase — the moment the repository contains
contributed code that the maintainer doesn't hold rights to, commercial
licensing of the project as a whole becomes impossible.

A CLA solves this by having you grant the maintainer the rights needed to
relicense your contribution, while you keep your own copyright and can still
use your own work however you like.

This is standard practice — the Apache Software Foundation, Google, Qt, and
essentially every dual-licensed project require one.

**Practically:** get this signed before writing code, not after. Retroactively
collecting a CLA is possible but requires tracking down every contributor and
getting their agreement, and a single person who declines (or becomes
unreachable) can permanently block relicensing of the affected code.

Use an established template rather than drafting one from scratch — the Apache
Individual CLA is a reasonable starting point. If real money is going to be
involved, have an IP lawyer review the final text.

## Licensing

Rumicos is licensed under GPL-3.0 (see `LICENSE`). By contributing under the
CLA above, your contributions are distributed under those same terms, and may
also be included in commercially licensed distributions of the project.
