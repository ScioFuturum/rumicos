# Shell checkpoint — `/bin/shell`, `/bin/echo`, `/bin/cat`

A minimal userspace shell (REPL + pipelines + redirection) written in
freestanding Rust, plus the kernel bugs it flushed out. CI gate:
`cargo xtask qemu-test` matches 30 serial lines, including the shell's
own `shell: ok ...` self-test assertions.

## What was built

```
userspace/
  liblow/       no-libc syscall layer shared by all three binaries
  shell/        parse.rs (pure, host-tested) + exec.rs + input.rs + selftest.rs + main.rs
  bin/echo/     argv[1..] joined by spaces + newline to stdout
  bin/cat/      stdin-until-EOF or one file operand to stdout
```

* `parse.rs` is `#![no_std]` (via the crate), zero unsafe, zero syscalls:
  a byte-scanning tokenizer (so `a>out` splits without whitespace) and a
  fixed-capacity `Pipeline` planner. 21 host unit tests
  (`cd userspace && cargo test -p shell`).
* `exec.rs` implements the classic pipeline order: all pipes up front →
  fork every stage → child dup2s pipe ends onto 0/1, applies `<`/`>`
  (which take precedence over pipes), closes EVERY original pipe fd →
  parent closes all its pipe fds → reaps exactly `stage_count` children,
  reporting the last stage's status. Failed execve exits 127.
* Boot flow: `init.elf` → execve `/init2.elf` (unchanged tests) →
  init2's parent path reaps its CoW-test child, then **execve
  `/bin/shell`**. The shell runs its self-test (same parse+exec code the
  REPL uses), then drops into the interactive REPL on the serial
  console (`cargo xtask qemu`).
* `cargo xtask build-userspace` builds the three binaries
  (`x86_64-unknown-none`, RUSTFLAGS override — see xtask docs);
  `mkinitrd` packs them at `/bin/*` plus an empty `/tmp` into the newc
  CPIO the kernel embeds.

## Kernel bugs found and fixed (all found live, in this order)

1. **fd refcounts did not survive fork, and exit closed nothing.**
   `FdTable::clone_for_fork` copied descriptors without `inc_ref`, and
   `Process::exit` never released fds. A pipe end's reader/writer
   liveness IS its VNode refcount, so the standard shell fd dance
   drove counts to zero early (then wrapped) and EOF either fired
   early or never. Fix: fork inc_refs every inherited fd via the
   existing hook; exit runs the full `ops.release` per open fd through
   a new `vnode_release` hook registered by kernel-fs.

2. **The syscall trampoline restored the user RSP from a per-CPU slot**
   (`gs:[CPU_RSP_USER]`) that every syscall entry on that CPU
   overwrites. A BLOCKING syscall (wait4) that resumed after another
   process ran syscalls on the same CPU sysret'd with that other
   process's RSP. Since all processes share the same stack VAs, the
   shell "returned" into the middle of its own data and a later `ret`
   jumped to rip=0x1. Diagnosed with a canary + SEGV stack dump; fixed
   by parking the user RSP on the per-thread kernel stack (mirroring
   rcx/r11) and restoring it with `mov rsp, [rsp]` at the exit edge.
   init2 never caught this because parent and child ran the same image
   at the same depth — the clobbered RSP was accidentally correct.

3. **AP LAPICs were never software-enabled in x2APIC mode.**
   `init_lapic`'s x2APIC branch set the MSR enable bits but never wrote
   SIVR (SVR.enable) or TPR — the xAPIC branch did both. The BSP worked
   only because UEFI firmware leaves its LAPIC enabled; APs arrived
   from INIT/SIPI software-disabled, which still accepts INIT/SIPI (so
   bring-up "worked") but silently swallows every fixed-vector
   interrupt. AP timers never fired; IPIs to APs were never delivered
   (including TLB-shootdown IPIs — nothing had ever actually exercised
   one). Fixed by programming TPR=0 + SIVR in the x2APIC branch.

4. **A thread enqueued on an idle AP starved forever.** Only the BSP
   has a periodic timer; an idle AP parks in `sti; hlt` and never
   drains its own run queue, and the BSP's `try_steal` never fires
   because its own queue always holds `kidle`. The second pipeline
   stage landed on CPU2 and never ran. Two measures: a reschedule-kick
   IPI (broadcast vector 0xfb) sent when a thread lands on a remote
   queue, and — because enabling real 4-way scheduling immediately
   exposed further latent SMP races (this system had effectively been
   interrupt-single-CPU its whole life) — user threads are pinned to
   the BSP's queue for now. Multi-CPU thread placement is deliberate
   follow-up work.

5. **liblow's `syscall6` under-declared clobbers.** Linux preserves all
   GPRs across `syscall`; Rumicos' trampoline does NOT restore
   rdi/rsi/rdx/r10/r8/r9. With plain `in(...)` operands rustc kept the
   shell's `wait4(-1, ..)` argument "alive" in rdi across the first
   wait4 — whose sysret left the just-reaped pid there — so the second
   wait4 waited on an already-reaped pid forever. All six argument
   registers are now `inlateout(...) => _`. This is a documented ABI
   deviation from Linux (hand-written asm must reload args after every
   syscall; init2.asm always did).

Also fixed in passing: PCIDs are now returned to the allocator on
address-space teardown (previously each fork+execve leaked two of
4096), and an unregistered CPU exception now prints `!vv:rip` instead
of silently re-faulting forever.

Disproven: the previous session's suspicion that execve-after-fork
teardown "deterministically corrupts the living parent". Frame-by-frame
tracing of every `cow_resolve` decision through a live boot showed the
CoW accounting is exact; the corruption was bug #2 above.

## Substitutions vs. the brief

* **Language**: Rust freestanding (`#![no_std]`/`#![no_main]`), as the
  brief recommended — no NASM fallback needed. Linker flags are NOT the
  GNU-ld recipe: rust-lld takes `--image-base=0x400000` (it rejects
  `-Ttext-segment=`), plus `-z noseparate-code -z norelro
  --no-eh-frame-hdr`, `relocation-model=static`, passed via the
  RUSTFLAGS environment variable (a nested `.cargo/config.toml` cannot
  override the workspace root's kernel linker script).
* **Serial read** is still the original non-blocking poll
  (`devserial_read` returns 0 when the FIFO is empty) — `read_line`
  busy-polls, burning one core between keystrokes in interactive mode.
  Blocking read via IRQ + WaitQueue remains future work.
* **O_CREAT**: Option A. `sys_open` creates a missing file in an
  existing parent directory via the same `VNodeOps::create` the CPIO
  unpacker uses, and `O_TRUNC` truncates on open. `> /tmp/new.txt`
  genuinely creates files; the initrd ships an empty `/tmp`.
* **Init wiring**: one combined binary (self-test, then REPL), launched
  by init2's parent path execve'ing `/bin/shell` after reaping its
  CoW-test child (so the shell starts with zero children and wait4
  accounting stays exact).
* **`>>`** is accepted but behaves like `>` on this kernel: no O_APPEND
  in the write path. Documented, not silent.
* **wait4 status**: userspace re-implements `WIFEXITED`/`WEXITSTATUS`
  against the kernel's `(code & 0xFF) << 8` packing (no shared libc).

## Known limitations

* No job control, no `&`, no environment variables, no `cd`/cwd, no
  quoting/escaping/expansion/globbing, no `;`/`&&`/`||`, no PATH search
  (absolute command paths only).
* Busy-poll stdin; destructive backspace is the entire line discipline.
* `>>` truncates (see above).
* Interactive mode is usable by hand via `cargo xtask qemu` but is not
  exercised by the CI harness (which cannot type).
* User threads all run on the BSP; APs idle (they still service
  shootdown/kick IPIs). True SMP scheduling is follow-up work, as is
  the related reap-vs-still-exiting-child race and the per-CPU
  syscall-frame clobbering that signal delivery after a BLOCKING
  syscall would observe (irrelevant while wait4's caller has no
  SIGCHLD handler registered).
* `sys_pipe` leaks its four frames if the fd table is full; pipe
  frames are never reclaimed after both ends close (pre-existing).
