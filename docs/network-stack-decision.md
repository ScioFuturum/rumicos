# Network stack: own implementation vs. smoltcp — decision analysis

Status: **decision paper, no code.** Written to inform a choice that can be
made now. Rumicos already transmits and receives raw Ethernet frames through
its own virtio-net driver (Part A/B); the question is what goes on top.

The tension is specific to the grant framing: a ready-made stack unblocks the
network fast, but **a ready-made stack does not count as an R&D result**, so
vendoring smoltcp wholesale forfeits the network layer as a research
deliverable — which, given the project's positioning, is the *central* one.

---

## 1. The gating question (answer this first — the whole boundary depends on it)

"Does not count as an R&D result" is not precise enough to draw the own/smoltcp
line, and guessing it wrong wastes the most expensive work. **What does the
application actually count as НИОКР?** Three plausible readings, each moving the
boundary:

- **(I) Original protocol implementation.** The result must be our own code
  implementing the protocols (TCP state machine, IP reassembly, etc.). Then any
  layer we vendor from smoltcp is a hole in the result, and the boundary is
  drawn at "how much of the stack must be our code to be defensible" — probably
  *the whole data path*, at least through the layer we headline.

- **(II) Novelty in architecture / integration / a specific property.** The
  result is the *memory-safety property* and the *appliance architecture*, not
  the line count. Then smoltcp on the lower layers is acceptable *if* the
  novel claim (e.g. a formally-argued memory-safe fast path, a specific
  isolation model, a verified parser) lives in our code. The boundary is drawn
  around the novel mechanism, not around each protocol.

- **(III) Volume / share of own code.** The result is measured by the amount of
  original engineering. Then a hybrid is fine as long as our share is
  substantial and the vendored part is clearly a dependency, not the headline.

The recommendation in §5 is written for reading **(I)** (the strictest, and the
safest to assume for a state R&D grant where reviewers check claims), and I flag
exactly which parts of it collapse under (II)/(III). **Please confirm which
reading the application uses before the first line of protocol code is written.**

---

## 2. Layer-by-layer verdict

Two axes per layer: **cost** (engineering + risk to the November demo) and
**research value** (does our own implementation produce something defensible as
R&D, or is it re-typing a textbook?).

| Layer | Cost (own) | Research value (own) | Verdict |
|---|---|---|---|
| **NIC driver** (virtio-net) | — already done | Medium — a from-scratch memory-safe modern-virtio driver is real work, and it is *ours* | **Own — done.** Keep. |
| **Ethernet + ARP** | Low | Low-medium — trivial framing, but ARP is untrusted-input parsing, which is the project's stated novelty surface | **Own.** Cheap, and it is exactly the "parse untrusted input safely" thesis. |
| **IPv4 (+ fragmentation)** | Low-medium | Medium — header/option parsing and reassembly are a classic memory-safety minefield in C stacks; a safe reassembly path is a genuine talking point | **Own.** Aligns with the thesis; reassembly is where the novelty argument is strongest. |
| **ICMP** (echo) | Low | Low | **Own** (falls out of IPv4 almost for free; needed for a "ping works" demo). |
| **UDP** | Low | Low-medium | **Own.** Small, and it is what makes an *end-to-end* demo reachable without TCP. |
| **TCP** (state machine, windows, RTO/retransmit, congestion) | **High** | **High — this is the headline R&D.** The state machine, timer management, and window logic are where both the difficulty and the defensible novelty live | **Own — the flagship deliverable.** This is the one layer where vendoring smoltcp would forfeit the most valuable result. |
| **Socket layer over VFS** | Medium | Medium — sockets as `fd`s wired into *this* kernel's VFS/blocking-syscall model is genuine integration work, and it is unavoidably ours (smoltcp has no syscall layer) | **Own** regardless of every other choice — smoltcp cannot supply it. |

Reading of the table: the lower layers (Eth/ARP/IPv4/ICMP/UDP) are **cheap AND
on-thesis** — reimplementing them is not "rewriting the well-known for no gain,"
because the gain the grant claims *is* memory-safe parsing of untrusted input,
and those layers are that parsing. TCP is **expensive AND the single most
valuable R&D result.** The socket/VFS layer is ours no matter what.

The only layer where "own vs. smoltcp" is a real trade rather than an obvious
"own" is **TCP** — and that is precisely the layer the grant most needs to be
ours.

---

## 3. Scenarios

- **(A) Own stack, entire.** Cost: highest. Risk to November demo: high if TCP
  is on the demo's critical path. Research value: maximal — the whole data path
  is ours. Failure mode: TCP slips, and there is no working network demo at all.

- **(B) smoltcp, entire.** Cost: lowest, fastest to "network works." Research
  value: **near-zero for the network layer** under reading (I) — the headline
  deliverable becomes a dependency. Also incurs an integration cost that is
  *not* free (see §4): smoltcp is poll-driven and must be adapted to this
  kernel's blocking-syscall model anyway. You pay integration cost *and* forfeit
  the result.

- **(C) Hybrid — own upper layers (TCP + sockets), own or smoltcp lower.**
  Two sub-variants:
  - **(C1) Own everything, smoltcp nowhere.** Effectively (A) but sequenced so
    the *demo* rides on UDP (own, cheap) while TCP (own) is developed off the
    demo's critical path. Best alignment of "working demo by November" with
    "network is our R&D result."
  - **(C2) smoltcp lower layers, own TCP.** Saves the (cheap, on-thesis) lower
    layers to buy time. But it forfeits the reassembly/parsing novelty (§2's
    strongest thesis points) for little schedule gain, and still needs the
    smoltcp↔own-TCP boundary glued. Weak trade.

- **(D) smoltcp temporarily, own stack later.** Cost: low now. **Risk that
  "later" never comes: high, and it is the honest danger here.** Three
  compounding reasons: (i) once the demo works on smoltcp, replacing a working
  TCP with your own is pure downside-risk work with zero visible progress, so it
  is deprioritized indefinitely; (ii) the integration layer gets built against
  smoltcp's *polling* model, so the eventual swap to a blocking-native own stack
  is a second integration, not a drop-in; (iii) for the grant, "we will write it
  later" is not a result — the reporting period wants a result *in* it. Treat (D)
  as "(B) with good intentions" and price it as (B).

---

## 4. Integration with the existing kernel (constrains the choice)

- **Sockets are `fd`s on the VFS.** A socket becomes a VNode with `read`/`write`
  (and a small `socket()`/`bind()`/`connect()`/`accept()` syscall surface),
  exactly like the pipe and `/dev/keyboard` ends already are. This layer is ours
  by necessity and is where the blocking semantics live.

- **Blocking `recv()`/`accept()` MUST use `thread_block_if`.** This is the direct
  link to the track that was just finished: a blocking socket read is the *same*
  "check for data → block" construction as `pipe_read`/`keyboard_read`. If it is
  written with a bare `thread_block`, it becomes the *fourth* lost-wakeup site.
  The waker (the RX path: virtio-net IRQ → stack → socket receive queue) must
  publish the received segment and bump a per-socket generation counter before
  waking, and `recv()` must re-check that counter under the socket's wait-queue
  lock. This is a hard requirement on whatever stack is chosen, and it shapes the
  choice: **an own stack is blocking-native and drops straight into this model; a
  poll-driven stack (smoltcp) does not.**

- **Cooperative vs. blocking model — the real friction with smoltcp.** smoltcp is
  not blocking-socket-native. It exposes a `poll(timestamp)` you call in a loop
  against a `Device`, and sockets you inspect for readiness; it assumes an
  event-loop driver, typically single-threaded and cooperative. Bolting that onto
  a preemptive kernel with blocking syscalls means running smoltcp's `poll()` on
  a dedicated kernel thread and hand-rolling the bridge that turns "smoltcp socket
  became readable" into "wake the thread blocked in `recv()` via `thread_block_if`."
  That bridge is non-trivial, it is *ours* to write and debug, and it is the same
  bridge whether smoltcp stays or is later replaced. So a large part of smoltcp's
  apparent time saving is spent on integration glue that (a) does not count as
  protocol R&D and (b) must be redone if (D) ever unwinds.

- **Retained CPU0 pinning is not a blocker.** The stack is effectively
  single-threaded today (all user threads on the BSP), which actually *simplifies*
  the first cut: the RX-IRQ→socket handoff and the `recv()` block/wake run on one
  CPU, so the only concurrency is IRQ-vs-thread, already handled by the
  `thread_block_if` + `cli` discipline. When Part C's pinning is eventually
  removed, the socket wait/wake paths are already built on the same primitive that
  was designed to be SMP-correct — no rework needed. Pinning helps here; it does
  not constrain the stack choice.

---

## 5. Recommendation

**Scenario (C1): own stack, top to bottom, sequenced so the November demo rides
on the cheap-and-on-thesis lower layers, with TCP developed as the flagship R&D
result off the demo's critical path. Do not vendor smoltcp at all.**

Concretely, in order:

1. Own Ethernet + ARP + IPv4 + ICMP on top of the existing virtio-net driver →
   **"ping works" is the first milestone** (own code, low risk, on thesis).
2. Own UDP + the socket/VFS layer with `thread_block_if`-based blocking →
   **an end-to-end UDP echo (or DHCP/DNS round-trip) is the November demo.** This
   is a genuine working networked appliance demo and it does **not** require TCP.
3. Own TCP as the headline research deliverable, developed after the demo path is
   green. If TCP is not fully finished by November, it is *in-progress own R&D
   within the reporting period* (a defensible технический задел), not a hole —
   which is strictly better than a finished-but-vendored TCP that counts for
   nothing.

**Why this and not the alternatives:** it is the only option that keeps the
network layer as *our* R&D result (killing (B) and, honestly priced, (D)) while
still guaranteeing a working network demo by November (killing the "(A) with TCP
on the critical path" failure mode). The lower layers are cheap enough that
"own" costs little over vendoring, and they *are* the memory-safety-of-untrusted-
input thesis, so own is both the cheaper-than-it-looks and the higher-value
choice there. smoltcp's time saving is largely illusory once its poll-model
integration glue — which we must write and maintain either way — is counted.

**Assumptions this recommendation rests on, and what breaks it:**

- It assumes reading **(I)** or **(III)** of §1. Under reading **(II)** — if the
  application's novelty claim is a *specific property or architecture* that our
  own code can carry while smoltcp does the plumbing — then (C2) or even a
  disciplined (B)-with-a-novel-wrapper becomes defensible, and the case for
  writing our own lower layers weakens sharply. **A confirmation that the grant
  counts (II) is the single answer that would change this recommendation.**
- It assumes a UDP-level end-to-end demo is an acceptable November milestone. If
  the milestone specifically requires *TCP* (e.g. an HTTP or SSH-shaped demo),
  the demo re-enters TCP's critical path and the risk calculus shifts toward
  needing a fallback — at which point a *time-boxed* smoltcp-for-TCP-only, with a
  hard, scheduled swap-out and the boundary designed for replacement from day
  one, becomes the least-bad hedge (a disciplined (D), priced with eyes open).
- It assumes the socket/VFS + `thread_block_if` integration layer is built once,
  natively blocking. That layer is ours under every scenario; building it against
  smoltcp's poll model first (as (D) forces) is the thing that makes "temporary"
  become permanent.
