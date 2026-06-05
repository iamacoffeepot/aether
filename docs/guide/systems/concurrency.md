# Concurrency & blocking

> Governing ADRs: **ADR-0087** (the blob dispatch model + scheduler), **ADR-0093**
> (the hold-until-resolve offload primitive), **ADR-0080** (settlement). The
> *contracts* on this page — single-threaded actors, per-recipient FIFO, no
> blocking — are **stable**. The scheduler *internals* that enforce them (the
> per-actor mail ring, blob formation, the cursor-shared cooperative drain) are
> **live and still being tuned** — a run of perf PRs through mid-2026 reshaped
> them — so treat this section as the *shape*, not a spec, and read
> `aether-substrate` (and ADR-0087) for the current mechanism. Build on the
> contracts, not the internals.

The invariants page states three contracts as rules: an actor is single-threaded
from its own perspective, mail is per-recipient FIFO, and a handler must never
block. This page is the machinery behind them — how the scheduler makes the
first two hold, why the third is non-negotiable, and what you do *instead* of
blocking when a handler needs to wait.

## The model: cooperative scheduling

Actors don't each own a thread. There are far more actors than worker threads,
and the pool multiplexes them on demand (ADR-0087) — no actor is pinned to a
core. And the thing the pool actually hands around is not a message, nor "an
actor," but a **blob**.

Sends don't deliver eagerly. While a handler runs, each `send` is written into
that actor's own **outbound mail ring** — a fixed-size, single-producer byte
buffer the actor fills in place — and the sends are grouped **by recipient** into
one **blob**. At the handler boundary the blob is flushed and routed: a recipient
receives a lightweight *reference into the producer's ring*, not a copied buffer,
so cross-actor mail is largely zero-copy. The blob — one producer's grouped
fan-out — is the unit of work the scheduler hands around.

A blob isn't handed to one worker to own — it's **published and raced**. The
worker that produced it keeps it warm on its own queue by default (a relay chain
stays on one hot worker, no cross-thread handoff), but once a fan-out is big
enough to be worth spreading, the blob is published and idle workers are woken to
**join in on the same blob**: they race a single shared cursor — one small atomic
— each grabbing the next recipient-group, seizing that recipient, and dispatching
its mail in place, then coming back for the next. The scheduler never decides
*where* a piece of work should run; there is no placement step and no
load-balancer. Work is just made available, and whatever workers are free race to
drain it as fast as they can, contending only on that cursor. **That minimal
coordination is the speed**: claiming the next group is a single atomic operation,
not a scheduling decision about who should do what.

What keeps an actor single-threaded through all of this is its **run-token** — a
per-actor `Idle → Ready → Running` word, not anything the pool does by placing
actors. It guards two points. When mail arrives, only the waker that wins the
`Idle → Ready` CAS publishes the actor's work, so it is never enqueued twice. And
a worker racing the cursor must win that recipient's `→ Running` CAS before
dispatching to it — lose the race (the actor is *already* `Running` under another
worker) and the mail is re-deposited for later rather than run concurrently. So
two workers never run one actor; and because a single worker thereby walks a
given recipient's mail, **per-recipient FIFO falls out for free** while distinct
recipients run concurrently — exactly the ordering spine the
[invariants](../foundations/invariants.md) and
[mail](mail-and-kinds.md) pages rely on. And single-threaded-per-actor is *why*
actor state is plain fields with no locks: nothing else can touch it
concurrently.

Scheduling is **cooperative and non-preemptive**. Once a worker starts a handler,
that handler runs to completion — the scheduler cannot interrupt it. A worker
dispatches a bounded batch and then releases the run-token so other actors get
their turn, but *within* a single handler invocation there is no yield point. A
long **compute** handler is fine: it ties up only its own worker and blocks
nobody else's stealable work. The problem is a handler that **waits**.

## Why a handler must never block

Because scheduling is cooperative and the pool is shared, a handler that blocks —
sleeps, does blocking I/O, waits on a blocking lock or channel, busy-spins —
doesn't just stall itself. It **pins a worker** doing nothing, shrinking the
pool. Worse, it can **deadlock a reply chain**: if you block waiting for actor B
to reply, and B's mail is sitting in a queue behind the very worker you're
holding, neither of you ever makes progress.

This is history, not hypothetical. The engine once had a synchronous `wait_reply`
primitive (ADR-0042). Once dispatch became pool-only — the `Dedicated`
per-actor-thread opt-in was removed (#1187) — an in-handler `wait_reply` could
park a shared worker and deadlock if the awaited reply needed that worker. It was
a latent footgun with no users, so it was retired (#1190). There is deliberately
**no blocking-await primitive** in the engine.

So never wait *inside* a handler. The sanctioned ways to wait are all the same
shape: **return now, continue later.**

## How to wait, without blocking

**1. Request/reply across actors → return, and match the reply in a later turn.**
The idiomatic shape is a small state machine spread across handler invocations:
send the request, *return* from the handler (freeing the worker), and handle the
reply when it arrives as a *separate* mail in a later handler call. Correlation
survives across turns — a handler can stash the correlation id of its send
(`prev_correlation`) and match it against the inbound reply's id, so multiple
in-flight requests don't get confused (the surviving half of ADR-0042). Every
request/reply in the engine works this way: `aether.fs.read` →
`…read_result`, reply-to-sender, and the rest.

**2. Blocking I/O or slow off-thread work → `dispatch_blocking` + `#[handler(task)]`.**
When a (native) capability must make a genuinely blocking call — a multi-second
provider request, a subprocess — it hands the work *off* the scheduler thread
(ADR-0093):

```rust
#[handler]
fn on_generate(&mut self, ctx, req: NanobananaGenerate) {
    let provider = self.provider.clone();
    ctx.dispatch_blocking(move || provider.call(&req));   // runs off the worker
}

#[handler(task)]                                          // a completion, not inbound mail
fn on_generate_done(&mut self, ctx, done: TaskDone<NanobananaResult>) {
    done.resolve(ctx);                                    // re-reply, then drop the hold
}
```

`dispatch_blocking` spawns the closure on a worker thread and returns
immediately; the result comes back later as `TaskDone<Output>` in a
`#[handler(task)]` handler — a *variant* of `#[handler]`, matched by its
`TaskDone<K>` parameter the way a mail handler matches its kind. `resolve(ctx)`
sends the reply and drops the settlement hold. This is the sanctioned home for
"reply in a later turn"; it replaced the hand-rolled `InFlightDispatch` the
content-gen capabilities used to carry. (Native capabilities today; a wasm/FFI
form is a deferred superset — guests use shapes 1 and 3.)

**3. Heavy async compute pipelines → the DAG.** Multi-step compute that produces
handles belongs off the actor thread entirely, expressed as a computation DAG.
See [The computation DAG & handles]().

## The three offload shapes, and the hold

Settlement — how `send_mail_traced` knows a chain of mail is *fully* done rather
than guessing with a timeout — requires every unit of in-flight work to stay
visible to the trace umbrella. A raw `std::thread::spawn` pushes rootless mail
the umbrella can't see, silently opting the work out. So offloading goes through
one of three sanctioned primitives, which differ only in *how long they hold the
causal chain open*:

| primitive | holds the chain? | for |
|---|---|---|
| `spawn_inherit` | yes — for the worker thread's lifetime | offloaded work that replies *before* the worker ends |
| `spawn_detached` | no — each send mints a fresh root | true fire-and-forget background work |
| `dispatch_blocking` (hold-until-resolve) | yes — until you `resolve`, *outliving* the worker | the "reply in a later turn" shape above |

The hold is what stops a deferred reply from settling early: if a handler kicks
off work that replies later, the chain must stay open until that last send, or a
waiter is told "done" before the reply arrives (ADR-0080 §12).
`dispatch_blocking` acquires the hold *eagerly* — before the handler returns —
and `resolve` releases it *after* the reply, so "reply before release" is
structural rather than something you remember to order. (A hand-rolled drain that
consumes an owned dispatch carries the same obligation by hand: record completion
or settlement never fires. See the *hold the chain open* obligation on the
[invariants page](../foundations/invariants.md).)

## The rare dedicated thread

Some work genuinely blocks at the *edges* of the engine — a TCP listener's
`accept` loop, the audio output callback, an RPC server's socket. Those spawn a
real OS thread, deliberately: it's blocking I/O that *should* live off the
scheduler, isolated so it can't pin a pool worker. That's the **exception** — a
handful of infrastructure capabilities — not how actors run, and not something to
reach for from ordinary actor logic (use one of the three shapes above). It's a
cap-local spawn, scoped tightly to the blocking call (ADR-0050).

## Where to read more

- The contracts this page implements — [Invariants & guarantees](../foundations/invariants.md).
- The mail spine and the per-recipient ordering guarantee — [Mail, kinds & scheduling](mail-and-kinds.md).
- Settlement and the hold contract in depth — [Tracing & settlement](tracing-and-settlement.md).
- Offloading heavy compute — [The computation DAG & handles]().
