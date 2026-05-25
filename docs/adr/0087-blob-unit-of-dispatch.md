# ADR-0087: The mail blob is the unit of dispatch — per-producer rings + work-stealing pool

- **Status:** Proposed
- **Date:** 2026-05-23

## Context

ADR-0086 phase 3c (#1100) retired the last central `SegQueue` in the substrate. With the trace pipeline decentralized to per-actor rings, the remaining cross-worker contention is in the **scheduler dispatch path** — the umbrella tracked by #1059 (cut pool dispatch latency µs → ns).

Today's pool (issue #635) dispatches one **actor-slot** at a time:

- Each actor has a channel **inbox** of owned `Envelope { recipient, kind, payload: Vec<u8>, .. }` (`crates/aether-substrate/src/mail/mod.rs`).
- Each actor has a run-token — `SlotState` (`Idle`/`Ready`/`Running`) with `try_wake` / `enter_running` / `BatchBudget` (`crates/aether-substrate/src/scheduler/slot.rs`).
- Ready slots are pushed to a **shared MPMC** `crossbeam_channel` ready-queue; workers pull one slot, claim its token, and drain its inbox up to `BatchBudget` (64 mails / 200µs) in `run_cycle` (`crates/aether-substrate/src/actor/native/dispatcher_slot.rs`).
- A fixed-K local cell (`try_stash_next` / `AETHER_LOCAL_STICKY_MAX`, default 1, `crates/aether-substrate/src/scheduler/local_slot.rs`) keeps the first successor warm; the fan-out remainder spills to the shared queue + `SpinPark::notify` (#1064/#1065).

The forces:

1. **Per-mail synchronization dominates the tiny-handler regime.** Most handlers are sub-µs (route / transform / emit). Fan-out to N distinct recipients costs N shared-queue pushes + up to N parked-worker wakeups. Our latency breakdown puts a parked wakeup at ~4.3µs — each scheduling sync dwarfs the handler it schedules. The fan-out shape (#960, measured by the #1057 harness) is where this bites; chains never pay it (they ride the warm-worker cell).

2. **The shared ready-queue is the scatter point.** Every fan-out child hits one shared tail. `AETHER_LOCAL_STICKY_MAX` masks this for trivial leaves but is a fixed band-aid: the phase-3c perf win *vanished at `STICKY=16`*, i.e. stickiness already masks the same contention work-stealing would fix principledly. A fixed K can't adapt — it serializes heavy fan-out at high K and scatters trivial fan-out at low K.

3. **Owned per-mail envelopes allocate eagerly.** Every routed mail is a `Vec<u8>` alloc + copy, regardless of whether the recipient is busy.

The design question: what is the **unit of work** a worker receives, and how is its synchronization amortized? The phased execution plan lives in #1101.

## Decision

**The base unit of work a worker receives is a blob of mails** — a single handler execution's buffered output — not a single mail and not a single actor-slot. This is the load-bearing axiom; the rest of the design derives from it.

### 1. Blobs live in per-producer rings

A blob is one **contiguous region of a per-producer (per-actor) byte ring**. Each actor owns a no-loss, reclaiming output ring (distinct from the loss-tolerant `VecDeque` trace/log rings of ADR-0081); the chassis owns one ring for off-actor producers (`Tick`, MCP sends, injected/test mail), mirroring the existing `chassis_host_ring` role.

A blob is self-describing and **cast in place** — in-process, single arch, so native `#[repr(C)]` layout with no serialization (unlike the cross-process hub wire, which must postcard):

```
[BlobHeader][MailEntry_0][payload_0]..[MailEntry_{N-1}][payload_{N-1}]   (each padded to 8)

BlobHeader { lock: AtomicU32, n_mails: u32, total_len: u32 }
MailEntry  { len: u32, recipient: u64 /*MailboxId*/, kind: u64 /*KindId*/ }
```

The fixed header fields are plain `Pod`; the `AtomicU32` lock is the one field that forces a raw `&*(ptr as *const BlobHeader)` cast (atomics aren't `Pod`) with an alignment guarantee. The payload behind each entry stays opaque bytes the recipient decodes per-kind (`Kind::decode_from_bytes`), exactly as today — just from a ring sub-slice instead of an owned `Vec<u8>`.

### 2. Buffering forms the blob

A handler's outbound mail is buffered and flushed — at handler return, or **before a blocking await** (`wait_reply`, or the awaited mail never goes out and the handler deadlocks) — as one contiguous region. This promotes the #1073 buffer+flush mechanism from "optional" to the **mechanism that creates the base unit**. Flushing publishes exactly **one** blob-ref to the scheduler — the single amortized cross-worker synchronization per handler execution.

### 3. The pool becomes work-stealing; the blob is the stolen unit

Per-worker deques replace the shared ready-queue + fixed-K cell. A worker pushes its produced blobs to its own deque (uncontended), pops LIFO, and when it would go idle it spin-then-parks (reusing `SpinPark`) and **steals** from a sibling's deque tail. A steal claims the recipient's run-token via the existing `enter_running` CAS — a `Running` actor is never stolen (preserving the single-threaded-actor invariant); the only cooperative release is the `BatchBudget` boundary. `try_stash_next` / `AETHER_LOCAL_STICKY_MAX` are subsumed (the knob retires or repurposes as the deque bound). Placement of cheap-vs-heavy work is decided **lazily by stealing**, not by a cost predictor: a fast child finishes before a thief arrives; a heavy/surplus child is stolen only when a worker is genuinely idle.

### 4. Multi-recipient demux, claim-or-deposit

A blob spans recipients `[A,B,C,D]`. The worker that takes a blob walks its entries: for a **free** recipient it claims the token and dispatches in place (then batches one `lock.fetch_sub(hits)` at the end); for a **busy** recipient it deposits a `MailRef` into that recipient's inbox and marks it ready. Inboxes therefore hold refs, not owned envelopes:

```
MailRef = InRing { ring_id, off }   // zero-copy hot path
        | Owned(Box<[u8]>)          // boundary mail + the copy-out fallback
```

The spilled (busy) recipients' mails are processed when their holder drains them (`run_cycle`, now ref-based), each decrementing the source blob's `lock` by 1.

### 5. Reclamation: per-blob lock, producer-advanced front, never block the producer

The per-blob `lock` counts down as its mails are processed (one batched decrement for inline hits; one per spilled mail on drain). The **single producer** lazily advances a front cursor past any frontmost `lock == 0` blobs before its next allocation; consumers do nothing but the atomic decrement. Wrap-around writes a skip-filler to the buffer end and restarts at 0 (blobs stay contiguous).

**The ring never blocks the producer.** A full ring copies the blob out to an `Owned` `MailRef` (heap). That one rule collapses three concerns into one mechanism: the full-ring policy, head-of-line relief (a stuck frontmost blob can't wedge the producer), and cyclic-backpressure deadlock avoidance (A→B and B→A both full *can't* deadlock if neither producer blocks). The cost is a memcpy under pressure — the same heap allocation today's envelopes pay eagerly on *every* mail — so the ring is a bounded zero-copy fast path with a fallback no worse than the status quo.

### Decode-in-place safety invariant

A handler dispatched in place borrows `&[u8]` into a producer's ring. This is sound because the region's `lock > 0` for the entire dispatch (the decrement lands *after* the handler returns), and the producer reclaims only `lock == 0` regions — so the tail can never overwrite bytes a handler is mid-read on. This invariant is the correctness crux and must be enforced/tested explicitly.

## Consequences

- **Synchronization is amortized per blob, not per mail.** One enqueue + one wake per handler execution (vs N for a fan-out of N); lock decrements are uncontended RMWs on an already-hot cache line, and the all-hits common case is a single batched decrement.
- **Fan-out misses are zero-copy.** A busy-recipient miss costs a ref deposit, not a decode/encode.
- **The run-token spine is retained.** `SlotState` / `enter_running` / `BatchBudget` / `run_cycle` survive; what changes is the queue structure (per-worker deque vs shared channel) and the inbox payload (`MailRef` vs owned `Envelope`).
- **Deep blast radius — this is the spine, not a leaf.** Ref-inboxes touch `Envelope` and `crates/aether-substrate/src/mail/{mod,registry,outbound}.rs` (today's `payload: Vec<u8>`), every test that constructs an envelope, and the scenario / `TestBench` injection path.
- **Supersedes two prior framings.** #1073 (sender-side bundle) is absorbed — buffering is how the blob is formed, not a separate optimization. The incremental "add K-adapt / affinity to the shipped spine" direction is reshaped: this changes the base unit (slot → blob), so it restructures the spine rather than tuning it.
- **Lazy placement dissolves the cheap/heavy question.** No runtime-cost predictor; a single long handler ties up only its own worker (cooperative scheduler — uninterruptible, blocks nobody else's stealable work).
- **Gated on measurement.** Profile-first veto: the #1057 / #1072 harness + the #960 fan-out workload must show the scatter/wakeup is the hot slice before code lands; p99 (makespan) must stay flat-or-better.
- **Deferred (phase 2+, not in the base):** affinity-biased stealing + actor home-worker tags (tie-break, never a gate); inline-atomic vs side-table refcount; explicit heavy-child handling beyond what lazy stealing gives.

## Alternatives considered

- **Keep the shared ready-queue, tune `AETHER_LOCAL_STICKY_MAX`.** A fixed-K band-aid; the `STICKY=16` finding shows it masks the contention rather than fixing it, and a static K can't adapt across fan-out widths.
- **Eager per-mail push to deques (no buffering).** Loses both the sync amortization and the 1:1 blob↔ring-region mapping the zero-copy handoff needs. Its only edge — exposing fan-out children mid-handler — matters solely for long-emit-early handlers, which are rare and arguably an anti-pattern.
- **Per-mail lock instead of per-blob.** Finer-grained reclaim, but one atomic per mail — rejected for the one-sync-per-blob axiom (with a batched decrement covering the all-hits case).
- **Explicit cheap/heavy cost prediction for placement.** Rejected; lazy work-stealing decides placement implicitly and never mispredicts (it only "spills" via a steal when a worker is genuinely idle).
- **Side-table refcount vs inline atomic.** Left as an open implementation fork (simpler `Pod` cast vs one extra indirection), not decided here.

## Amendment — 2026-05-24: ordering spine + in-place demux phasing

Implementation experience (#1134 hop decomposition, #1135) refined two points in §3/§4.

### The mail-ordering spine

The base ordering invariant the dispatch model guarantees:

> **Same recipient + same sender context → handled in declaration order (per-recipient FIFO). Different recipients → no ordering guarantee; each send is async, like a server call.**

Strict cross-recipient execution order is explicitly *not* a contract. Enforcing it is head-of-line blocking — one slow or blocked recipient would stall every later one in a handler's fan-out and re-couple independent actors, a starvation hazard. Cross-recipient effect sequencing is causal (B triggered by A's completion / A mails B), never inferred from send order. The spine is the minimum that makes local reasoning sound — a handler's repeated sends to one actor arrive in order — without the global coupling. It is the foundation both the in-place demux (§4) and the cooperative multi-worker demux (#1137) rest on: per-recipient FIFO is preserved by one-worker-per-recipient; cross-recipient concurrency is sound precisely because the spine does not order it.

### §4 is in-place dispatch; the shipped 3b path was a shortcut

§4 specifies that a free recipient is dispatched **in place** ("claim the token and dispatch in place"). The shipped Phase 3b demux instead deposited each mail through `route_mail`, collected the woken slot, and re-`try_recv`'d it — a deposit→repop round-trip #1134 measured as the residence half of the fan-out hop. #1135 realises §4 as written: the demux **seizes** a free recipient (`SlotState` `Idle→Running`) and runs its handler in place via `DispatcherSlot::dispatch_one`, depositing only when the seize loses (busy). The deposit-collect machinery (`run_demux` / `try_collect_demux`) retires.

### Phasing: single-worker in-place first, cooperative multi-worker deferred

§3 frames a blob as stolen whole by one owner. That holds for the single-worker in-place demux #1135 ships — order-safe under the spine (send-order walk, one worker). The **cooperative multi-worker** variant — several workers draining one blob via a shared cursor (#1137) — is a later phase that supersedes §3's one-owner framing. It is sound under the spine (cross-recipient async) but adds concurrency machinery (packed lifecycle word, per-group closeable-stack merge handshake, recruitment) and is gated on whether #1135's in-place dispatch still leaves serialization residence on wide/heavy fan-out.
