# The scheduler

> **Governing ADR:** [ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md) (the blob as the unit of dispatch + the
> work-stealing pool). The *contracts* the scheduler enforces — single-threaded
> actors, per-recipient FIFO, cooperative dispatch — are **stable**; they live on
> [Concurrency & blocking](concurrency.md) and the
> [invariants page](../foundations/invariants.md). The *mechanism* on this page
> is **live and still being tuned** — knob defaults and fairness levers shift
> under perf work — so read it as a reminder-grade map at named-module fidelity,
> with [ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md) and the source as the authority when a detail is
> load-bearing.

This page draws the machinery out: how a handler's sends become a **blob**, how
a blob reaches other workers, how the **run-token** keeps an actor
single-threaded, which levers govern wakeup and fairness, where causal **roots**
enter, and how to map a trace tree's timestamps back to all of it.
[Concurrency & blocking](concurrency.md) covers what the contracts mean for code
you write; this page is for the returning reader asking how dispatch works
underneath them.

The cast of modules, all under `crates/aether-substrate/src/`:

| module | owns |
|---|---|
| `scheduler/pool.rs` | the worker threads (`aether-worker-<n>`) and the acquire loop |
| `scheduler/slot.rs` | the run-token (`SlotState`), the per-cycle `BatchBudget`, `WakeHandle` / `WakeSink` / `SeizeHandle` |
| `scheduler/spin_park.rs` | `SpinPark` — the spin-then-park coordinator (route-to-spinner) |
| `scheduler/worker_deque.rs` | the per-worker deque, the keep-local valve, the chain backstop |
| `scheduler/calibrate.rs` | the measured cross-worker handoff cost the valves scale from |
| `mail/ring.rs` | `MailRing` — the per-actor outbound byte ring blobs are written into |
| `mail/mail_ref.rs` | `MailRef` — the `InRing` / `Owned` payload handle an inbox envelope carries |
| `actor/native/binding.rs` | the outbound buffer + `flush_outbound` at the handler boundary |
| `actor/native/blob_work.rs` | `BlobWork` / `BlobProducer` — the cursor-shared cooperative blob |
| `actor/native/blob_lifecycle.rs` | the packed `Lifecycle` word (cursor / len / done / seal) |

## The blob lifecycle

One handler execution's buffered output is the base unit of work — the **blob**
([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)'s axiom). Everything below is the life of one blob, from
the first `send` inside a handler to the ring bytes being reclaimed.

**1. Sends buffer into the producer's ring.** While a handler runs, each `send`
writes its payload bytes straight into that actor's own outbound `MailRing`
(`mail/ring.rs`) — a fixed-size (64 KiB, `ACTOR_RING_BYTES` in
`actor/native/binding.rs`), single-producer, multi-consumer, *reclaiming* byte
ring. The first send of a handler invocation opens a blob in the ring
(`open_blob`); each subsequent send appends in place (`append`) — no staging
buffer, no per-mail allocation. The ring **never blocks its producer**: when a
blob doesn't fit (`RingFull`), the payload is copied out to a heap buffer
instead (`MailRef::Owned`), so the ring is a bounded zero-copy fast path whose
fallback costs the same memcpy an eagerly-allocated envelope would. (These rings
are distinct from the loss-tolerant trace/log rings of ADR-0081 — a mail ring is
no-loss, and a region stays live until every reader has released it.)

**2. Flush at the handler boundary.** When the handler returns,
`NativeBinding::flush_outbound` seals the open blob (`MailRing::seal` publishes
the region's reclaim lock) and mints one `MailRef` per buffered mail —
`InRing` for ring-resident payloads, `Owned` for the copy-out fallback. The
whole window then routes as a single unit. This boundary is the amortization
point: a fan-out of N recipients costs one scheduling synchronization, never N
queue pushes and N wakeups.

**3. Group by recipient.** `BlobProducer::flush` (`actor/native/blob_work.rs`)
folds the routed mail into the actor's **single active `BlobWork`**, grouped by
recipient. A new recipient becomes a fresh **group**, written into the blob's
group array and published through the packed `Lifecycle` word; a recipient seen
before pushes onto its existing group's buffer. That append rule is what
preserves per-recipient FIFO *across* flushes: a producer has at most one
unclosed group per recipient across all of its live blobs, so two workers can
never hold same-recipient mail concurrently.

**4. Keep warm — and recruit when it's worth it.** Every flush schedules one
drainer copy of the blob through `WakeSink::schedule`. On a pool worker that
push lands on the worker's **own deque** (the keep-local path — same worker,
LIFO pop, no cross-thread handoff), so a chain or a cheap fan-out stays on one
warm worker. A flush whose fresh groups carry enough measured work additionally
**broadcast-recruits** siblings: `recruit_k` sizes the recruitment from the
per-handler execution-cost EWMAs (`K ≈ ceil(total_work / longest_pole)`, gated
by the box-calibrated wake break-even), and `WakeSink::recruit` pushes that many
clones of the blob to the shared injector and unparks that many workers. When
any contributing cost cell is untrustworthy, the decision falls back to the
width gate (`AETHER_BLOB_RECRUIT_MIN`, default 9 — so a fan-out 8 wide or
narrower stays local), capped by `AETHER_BLOB_RECRUIT_MAX` (default 32).

**5. The cursor race.** A blob is published and raced, never owned. Each worker
that picks up a copy runs `BlobWork::run_cycle`, which loops
`Lifecycle::claim` — one CAS on the packed lifecycle word
(`actor/native/blob_lifecycle.rs`: `[seal | done | len | cursor]` in a single
`AtomicU64`) — and each claim hands a whole recipient-group to exactly one
worker. There is no placement decision and no load balancer: N recruited
workers race the one cursor and split the groups between them, contending on a
single atomic. One worker owning a whole group is also what makes per-recipient
FIFO free — it dispatches that recipient's mail in send order, draining the
group's closeable buffer until empty and then closing it (the close is a FIFO
barrier: a producer push that arrives after the close deposits through
`route_mail` and lands strictly behind everything the worker dispatched).

**6. Seize, or lose the CAS and deposit.** For each mail in a claimed group,
`dispatch_one` resolves the recipient (`Registry::route_lookup`) and tries to
run it **in place**: `SeizeHandle::try_seize` attempts the recipient run-token's
`Idle → Running` CAS, and a win runs the handler right there on this worker
(`Drainable::seize_and_run`) with no inbox round-trip. A lost CAS means the
recipient is already in flight — `Ready` (a sender's wake won) or `Running`
(another worker is draining it) — and the mail is **re-deposited** through
`Mailer::push` → `route_mail` onto the recipient's inbox instead, where the
current holder's drain (or the wake it triggers) picks it up. The inbox's own
ordering keeps FIFO intact on that path. Non-`Pooled` recipients (inline
handlers) and ADR-0045 ref-carrying kinds always take the deposit path.

**7. Reclamation.** Each blob region in the ring starts with its
`BlobHeader.lock` equal to its mail count; every `MailRef::InRing` holds one
count and releases it on drop (RAII — cloning acquires another). The producer
reclaims lazily: at the next `open_blob` it advances the ring's front cursor
past any frontmost regions whose lock has reached zero; a wrap writes a skip
filler and restarts at offset 0. The lock staying above zero for the entire
dispatch is what makes decode-in-place sound — a handler borrowing `&[u8]` into
a producer's ring can never have those bytes overwritten under it, because the
producer only reuses regions every consumer has released.

## The run-token

Each `Pooled` actor has one **run-token** — `SlotState` in `scheduler/slot.rs`,
a single `AtomicU8` over three states — and every path that could run the actor
must win a CAS on it first. The token is the entire single-threaded-actor
mechanism: the pool never reasons about placement, it just refuses to let two
workers hold one token.

```text
            try_wake                      enter_running
            (sender-side CAS)             (worker-side CAS)
   ┌──────┐ ────────────────► ┌───────┐ ────────────────► ┌─────────┐
   │ Idle │                   │ Ready │                   │ Running │
   └──────┘                   └───────┘                   └─────────┘
      │ ▲                         ▲                          │  │
      │ │                         │      mark_ready          │  │
      │ │                         └─(budget hit; re-pushed)──┘  │
      │ │                                                       │
      │ └── mark_idle (drained empty), then the post-empty ─────┘
      │     recheck: try_self_requeue re-runs Idle → Ready
      │
      └──── seize (demux-side CAS): Idle → Running direct ──► (Running)
```

Four transitions carry the load:

- **`try_wake` (`Idle → Ready`)** — the sender side. After depositing an
  envelope on the inbox, `WakeHandle::wake` runs this CAS; only the winner
  publishes the slot to the scheduler. This is the dedup: a slot already
  `Ready` or `Running` is never enqueued twice, however many senders race.
- **`enter_running` (`Ready → Running`)** — the worker side. The worker that
  popped the slot claims it for a drain cycle. Only the popper calls this, so
  one worker drains at a time.
- **`seize` (`Idle → Running`)** — the demux side. A blob worker holding a free
  recipient's mail in hand claims the token directly and dispatches in place,
  skipping the inbox round-trip. A loss leaves the state untouched and the mail
  deposits instead — the two CAS entry points (`try_wake` vs `seize`) agree, in
  one total order, on who owns an `Idle` slot, so a sender's wake and a demuxer's
  seize can never both proceed.
- **`Running → Idle` / `Running → Ready`** — the exits. Drained to empty:
  `mark_idle`, then the **post-empty recheck** (`try_self_requeue`) re-reads the
  inbox and re-runs the `Idle → Ready` CAS if mail arrived during the gap —
  closing the classic send-vs-drain race where a sender observed `Running` and
  skipped its wake just as the worker went idle. Budget hit: `mark_ready` keeps
  the slot `Ready` and the worker re-publishes it, so one chatty actor yields
  the worker without ever passing through `Idle`.

The racing sites — `try_wake`, `seize`, `mark_idle`, `try_self_requeue` — all
use `SeqCst` so they share one total order; the comments in `scheduler/slot.rs`
spell out which stranded-envelope case each ordering forbids.

## Wakeup and fairness

**The acquire loop.** A worker looks for work in a fixed order
(`acquire_slot` in `scheduler/pool.rs`): its **own deque** first (LIFO — the
freshest hop is the warmest), then a steal pass over the **shared injector**
(plus siblings' deque tails only when `AETHER_PEER_STEAL=1`; the default is
owner-only), then the `SpinPark` coordinator — spin for a bounded window
(`AETHER_SPIN_WINDOW_USEC`, default 50µs), re-running the steal scan, then park
on the thread token.

**Route-to-spinner.** A producer that pushes work calls `SpinPark::notify`,
which skips the futex wake entirely when some worker is already spinning — the
spinner's scan picks the work up for free. Only the genuine idle edge (no
spinner) pays a parked-worker unpark. The lost-wakeup race between "producer
skips the wake" and "spinner gives up and parks" is closed by symmetric `SeqCst`
fences plus a register-before-decrement rule on the idle list; the module doc
in `scheduler/spin_park.rs` is the full argument. Blob recruitment bypasses the
route-to-spinner gate on purpose (`SpinPark::wake_workers`): one spinner can't
drain N clones, so recruit unparks N siblings directly.

**The batch budget.** A drain cycle is bounded by `BatchBudget` — 64 envelopes
(`BATCH_MAX_MAILS`) or 200µs (`BATCH_MAX_USEC`), whichever trips first; the
wallclock is checked only every 8th dispatch (`CLOCK_CHECK_STRIDE`), so a short
cycle never reads the clock. On a budget hit the worker releases the token
(`mark_ready`) and re-publishes the slot to the injector — the yield that lets
two perpetually-busy actors share one worker fairly. The budget bounds the gap
between handler turns, never a handler itself: dispatch is cooperative, and a
running handler is uninterruptible.

**The keep-local valve.** By default a worker inlines its whole local cascade —
every blob a running handler produces goes to its own deque — until the burst
has run longer than the **time valve**, at which point the backlog spills to the
injector so a heavy cascade parallelizes. The valve is adaptive
(`worker_deque::time_budget`): a small multiple (6×, clamped to 6–60µs) of the
**measured cross-worker handoff cost** — boot-probed and live-refined per box in
`scheduler/calibrate.rs` — because the handoff is the thing inlining
out-amortizes. `AETHER_LOCAL_TIME_BUDGET_US` pins the valve (0 disables it);
`AETHER_HANDOFF_COST_NS` pins the measurement; `AETHER_LOCAL_STICKY_MAX`
(default 256) is the deque-length backstop behind it all.

**The chain backstop.** A self-sustaining relay (A mails B, B mails A) oscillates
its own deque 0→1→0 and would never visit the injector. Every
`AETHER_LOCAL_CHAIN_BACKSTOP`-th consecutive own-deque pop (default 64) the
worker probes the injector once before continuing, so a captured worker starves
shared work for at most ~K cycles.

**Panics.** A handler panic is caught at the worker boundary and escalated
through the chassis `FatalAborter` (fail-fast per ADR-0063) — the pool never
silently loses a worker thread.

## Where roots enter

Every causal chain starts with a **root** — mail sent from outside any handler
([Tracing & settlement](tracing-and-settlement.md)). Roots reach the scheduler
through `Mailer::push_chassis_root_mail` / `Mailer::push` (`mail/mailer.rs`)
from threads that are *outside* the pool:

- the chassis frame loop's `Tick` fan-out and the lifecycle driver's
  `init` / `wire` / `unwire` steps;
- MCP-injected and hub-bridged mail, arriving on the hub client's socket thread;
- a capability's worker thread (`spawn_inherit` / `dispatch_blocking`
  completions, a listener reacting to a socket read) feeding results back as
  mail.

An off-pool thread has no own deque, so its wake always takes the spill path:
deposit on the recipient inbox, `try_wake`, push to the shared injector,
`notify`. A root therefore pays at most one injector push and one wakeup — and
the entire cascade it triggers then rides the warm in-pool paths above. Root
payloads arrive as `MailRef::Owned` (cross-boundary bytes); only mail produced
*inside* a handler rides a producer ring.

## The dedicated-thread edges

A few infrastructure capabilities own a real OS thread because the work at the
engine's edge genuinely blocks: a TCP listener's `accept` loop, the audio output
callback, the RPC server's socket, the desktop window driver on the OS
event-loop thread. These threads sit outside the scheduler's model — the pool
neither runs nor steals from them — and they interact with it only as
off-worker producers (the root path above) or, in the window driver's case, as
a hand-drained inbox carrying its own finish obligation
([Tracing & settlement](tracing-and-settlement.md) covers that guard). When to
reach for one — almost never — is [Concurrency & blocking](concurrency.md)'s
topic; from the scheduler's perspective they are simply the boundary.

## Reading the scheduler

The trace tree's per-mail timestamps
([Tracing & settlement](tracing-and-settlement.md)) sample exactly the
lifecycle above, so a slow span names a mechanism:

```text
●  t_construct_start    open_blob — the handler's first buffered send
│     construct         the rest of the handler runs, appending sends to the ring
●  t_sent               flush_outbound — the handler boundary; the blob routes
│     queued            scheduling: waiting for a worker to pick the blob up
●  t_enqueue            a worker's BlobWork::run_cycle begins (the pickup stamp)
│     drain             groups/mail this worker dispatches ahead of this one
●  t_received           the recipient's handler is entered
│     handler           the handler runs
●  t_finished           the handler returns
```

- **construct** is producer time: the sending handler still running after its
  first send. A long construct is a long handler, never scheduling pressure.
- **queued** (`t_sent → t_enqueue`) is the scheduler's share. Kept-local work
  reads near zero (the producing worker pops its own deque next); a spilled or
  recruited blob pays the injector trip and possibly a parked-worker wakeup —
  on the order of the calibrated handoff cost, single-digit µs. A *large*
  queued span means real pressure: every worker busy, or work waiting behind a
  backlog (the per-envelope `enqueue_depth` in the rings records the depth
  observed at deposit).
- **drain** (`t_enqueue → t_received`) is in-blob serialization: groups the
  same worker claimed ahead of this one, plus earlier mail in this mail's own
  recipient-group. A long drain on a wide fan-out means it under-parallelized —
  check whether recruitment fell back to the width gate (untrusted cost cells)
  or the fan-out sat below `AETHER_BLOB_RECRUIT_MIN`. A single fat group is the
  longest pole: groups never split across workers, so no recruitment shortens
  one recipient's serial share.
- Each recruited sibling stamps its own `t_enqueue` at its own `run_cycle`
  entry, so one blob's mails can carry different pickup instants — that spread
  *is* the cooperative drain, made visible.

Over MCP the tree surfaces `t_construct_start` / `t_sent` / `t_received` /
`t_finished` (no `t_enqueue`), so queue latency reads as the combined
`t_sent → t_received` span there.

## Where to read more

- The contracts this machinery enforces, and how to wait without blocking —
  [Concurrency & blocking](concurrency.md).
- The ordering spine and the mailbox/kind model —
  [Mail, kinds & scheduling](mail-and-kinds.md).
- The timestamp vocabulary and settlement —
  [Tracing & settlement](tracing-and-settlement.md).
- The design record — [ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md), including the forward-notes that
  track where implementation refined the original framing.
