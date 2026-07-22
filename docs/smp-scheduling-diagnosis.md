# Part C — fair SMP scheduling: diagnosis (workaround retained)

> **Reserved work — do not resume without a separate go-ahead.** Fair SMP
> scheduling (removing the pinning, thread migration, work-stealing, and TLB
> shootdown under real multi-core execution) is a distinct result to be funded
> and delivered on its own. The pinning stays in place; `least_loaded_cpu`
> spreading and AP timers are **not** to be re-enabled, and the migration
> races below are **not** to be fixed, as part of ongoing work. This document
> is the preserved diagnosis, not a task list to pick up now.

**Outcome (per the checkpoint's C5 clause):** removing the CPU0 pinning
reproduces a real, timing-dependent multi-CPU hang. The failure was diagnosed
to a specific trigger and a small set of contended structures, but it is a
multi-front hardening task rather than a single fixable bug, so the pinning
workaround is **left in place** and this is the recommended next investigation.
A correct diagnosis with the workaround retained is more valuable than a
speculative fix that passes once and reintroduces the race under other timing.

## What "the pinning" is

Two cooperating mechanisms kept every thread on the BSP:

1. `kernel_sched::enqueue_thread` forced `target_cpu = 0` regardless of load.
2. `kernel_smp::ap_entry` never armed an AP's LAPIC timer, so an AP only ran
   the scheduler when an IPI happened to wake it — it never preempted.

The experiment removed both: `enqueue_thread` → `least_loaded_cpu()`, and each
AP arms its timer (`set_timer_oneshot(1)`) before `idle_loop()`.

## Failure signature (captured BEFORE any fix)

With pinning removed, boot completes every init2 self-test *most* of the way,
then hangs (QEMU timeout, no fault, no panic — a true hang). The **hang point
moves between runs** as instrumentation perturbs timing — the hallmark of a
genuine race:

- clean run: hung at the shell's second pipeline (`echo | cat`, two processes
  now on different CPUs) after `shell: ok echo`.
- lightly instrumented: hung in the fork-name-test `wait4`/reap.

Serial-marker trace of the fork-name test at one hang:

```
[b0]            parent blocks in wait4 on CPU0
... child runs, prints "wait: child running" ...
[k3W]           child exits on CPU3, wake_one() reports a waiter was made runnable
[u3]            parent RESUMES on CPU3 (migrated from CPU0 via work-stealing)
[L3]            parent re-scans in wait4 on CPU3
[F]             find_zombie() found the zombie, calls reap_zombie()
<hang>          reap_zombie never reaches its first internal marker
```

Finer markers showed reap sometimes completes (`<123[D]`) and the hang lands
later (shell pipeline) instead — confirming a nondeterministic, timing-driven
race, not a fixed deadlock.

## What was ruled OUT (verified, not assumed)

- **Lost wakeup (Part A).** The wake fires and reports success (`[k*W]` = a
  waiter was made runnable). Part A's `child_exit_seq` + `thread_block_if`
  hold; the parent is not left asleep. The problem is strictly *after* resume.
- **CR3 not reloaded on migration.** `sched_context_switch_hook` →
  `activate_address_space` runs in `schedule()` before `switch_context`, so a
  thread that resumes on a new CPU gets its CR3 loaded first. Confirmed by code
  path; the migrated parent does execute correctly up to `reap_zombie`.
- **Per-CPU current-process pointer.** `set_current_process` is a no-op;
  `current_process()` derives the process from the *current thread*
  (`sched_cpu(cpu).current` → `process_for_thread`), which `schedule()` sets on
  the target CPU. So it is correct after migration — the parent's `find_zombie`
  ran correctly on CPU3.

## The actual trigger and the contended structures

**Trigger:** with threads spread across CPUs, a thread that blocks on CPU A is
woken and then **work-stolen onto CPU B** (`try_steal`), so it resumes on a
different CPU than it blocked on. Everything up to that is fine; the hang is in
what the migrated thread does next against globally-shared structures under
genuine concurrency that pinning had made effectively single-threaded:

Ranked by the evidence (`[F]` reached, first reap marker not):

1. **`ptable_find` / ptable bucket locks in `reap_zombie`'s prologue.** The
   hang sits between `find_zombie` returning and `reap_zombie`'s first marker —
   i.e. in the second `ptable_find`, which takes a bucket spinlock. Under real
   concurrency other CPUs (fork/exit/current_process) hit the same buckets.
   Because it is timing-dependent (reap *sometimes* completes), this is
   contention/ordering, not a hard cycle — but it is the prime suspect.
2. **TLB shootdown under real concurrency.** `reap_zombie` →
   `AddressSpace::drop_ref` → `free_user_mappings` → `shootdown_page`, which
   **spin-waits for `remaining` to reach zero** on IPI acknowledgement from
   every CPU that still has the child's address space marked active. This
   spin-wait and `SHOOTDOWN_SERIALIZE` have never run with threads genuinely
   executing on multiple CPUs at once (C2). A missed/￼delayed shootdown IPI ACK
   hangs the initiator exactly here.
3. **Run-queue push from IRQ vs concurrent pop, and the `try_lock` fallback.**
   `try_make_runnable` → `try_push_to_cpu` uses `try_lock` and, on failure,
   silently does not enqueue the woken thread (a dropped wakeup). Under
   contention this can strand a thread; it is a latent second-order hazard even
   though it was not the observed first hang.
4. **Pipe blocking read/write** carried the *same* check-then-block-across-
   two-locks lost-wakeup shape Part A fixed for `wait4` (condition under the
   ring lock, block under the wait-queue lock). Harmless while pinned; a real
   lost-wakeup once `echo` and `cat` run on different CPUs — this explained the
   shell-pipeline hang variant. **This one is now closed** (independently of
   Part C, as pre-work): `pipe_read`, `pipe_write` and `keyboard_read` were
   converted to `thread_block_if` with per-object generation counters, exactly
   like the wait4 fix (commits `fs/pipe:` and `keyboard:`). So a future Part C
   re-attempt starts with the lost-wakeup class already eliminated kernel-wide;
   the remaining fronts are 1–3 above.

## Recommended next step

When Part C is eventually funded as its own checkpoint, do it bottom-up, each
step verified in isolation under `-smp 4`:

1. ~~Convert `pipe_read`/`pipe_write`/`keyboard_read` to `thread_block_if`.~~
   **Done** as pre-work (see above) — the lost-wakeup class is already gone.
2. Make `try_make_runnable` never drop a wakeup: on `try_lock` failure, fall
   back to a lock (or defer) rather than skipping the enqueue.
3. Stress the TLB-shootdown path deliberately with threads pinned to *all*
   CPUs doing concurrent `munmap`/exit, and instrument the `remaining`
   spin-wait to catch a missed ACK; verify `SHOOTDOWN_SERIALIZE` ordering.
4. Only then re-enable spreading + AP timers and run the existing boot.

Steps 2–4 are the reserved work; step 1 is already merged.

## Status

Pinning restored; `enqueue_thread` pins to CPU0 and APs do not arm timers, as
before. qemu-test 38/38, host tests and clippy unchanged. APs still take
TLB-shootdown and reschedule IPIs; they simply do not run user threads yet.
