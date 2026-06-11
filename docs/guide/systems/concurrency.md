# Concurrency & blocking

> **Governing ADRs:** [ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md) (the blob dispatch model + scheduler), [ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md)
> (the hold-until-resolve offload primitive), [ADR-0080](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0080-substrate-mail-tracing-and-settlement.md) (settlement). The
> *contracts* on this page — single-threaded actors, per-recipient FIFO, no
> blocking — are **stable**. The scheduler *internals* that enforce them are
> drawn out on [The scheduler](scheduler.md) and are **live and still being
> tuned**. Build on the contracts, not the internals.

The invariants page states three contracts as rules: an actor is single-threaded
from its own perspective, mail is per-recipient FIFO, and a handler must never
block. This page is about writing code inside them — the scheduling model in
enough detail to reason from, why the third contract is non-negotiable, and what
you do *instead* of blocking when a handler needs to wait. The machinery that
makes the first two hold is drawn out on [The scheduler](scheduler.md).

## The model: cooperative scheduling

Actors don't each own a thread. There are far more actors than worker threads,
and a small work-stealing pool multiplexes them on demand
([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)). The unit the pool hands around is a **blob** — one handler
execution's buffered sends, grouped by recipient and published for idle workers
to race — and a per-actor **run-token** admits one worker to a given actor at a
time. The token is what makes an actor single-threaded (so actor state is plain
fields with no locks) and what makes **per-recipient FIFO fall out for free**
while distinct recipients run concurrently — exactly the ordering spine the
[invariants](../foundations/invariants.md) and [mail](mail-and-kinds.md) pages
rely on. The full machinery — the outbound mail ring, blob formation, the
cursor race, the run-token state machine, wakeup and fairness — is drawn out on
[The scheduler](scheduler.md).

What this page needs from that model is one property: scheduling is
**cooperative and non-preemptive**. Once a worker starts a handler, that handler
runs to completion — the scheduler cannot interrupt it. A worker dispatches a
bounded batch and then releases the run-token so other actors get their turn,
but *within* a single handler invocation there is no yield point. A long
**compute** handler is fine: it ties up only its own worker and blocks nobody
else's runnable work. The problem is a handler that **waits**.

## Why a handler must never block

Because scheduling is cooperative and the pool is shared, a handler that blocks —
sleeps, does blocking I/O, waits on a blocking lock or channel, busy-spins —
doesn't just stall itself. It **pins a worker** doing nothing, shrinking the
pool. Worse, it can **deadlock a reply chain**: if you block waiting for actor B
to reply, and B's mail is sitting in a queue behind the very worker you're
holding, neither of you ever makes progress.

This is history, not hypothetical. The engine once had a synchronous `wait_reply`
primitive ([ADR-0042](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0042-synchronous-mail-wait.md)). Once dispatch became pool-only — the `Dedicated`
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
in-flight requests don't get confused (the surviving half of [ADR-0042](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0042-synchronous-mail-wait.md)). Every
request/reply in the engine works this way: `aether.fs.read` →
`…read_result`, reply-to-sender, and the rest.

**2. Blocking I/O or slow off-thread work → `dispatch_blocking` + `#[handler(task)]`.**
When a (native) capability must make a genuinely blocking call — a multi-second
provider request, a subprocess — it hands the work *off* the scheduler thread
([ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md)):

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
See [The computation DAG](dag.md).

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
waiter is told "done" before the reply arrives ([ADR-0080](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0080-substrate-mail-tracing-and-settlement.md) §12).
`dispatch_blocking` acquires the hold *eagerly* — before the handler returns —
and `resolve` releases it *after* the reply, so "reply before release" is
structural rather than something you remember to order. (A hand-rolled drain that
consumes an owned dispatch carries the same obligation by hand: record completion
or settlement never fires. See the *hold the chain open* obligation on the
[invariants page](../foundations/invariants.md).)

A chain that never settles traces back to a missed hold here — picking the wrong
shape from this table, or a raw `std::thread::spawn` that opts the work out. The
[Debugging a hung settlement](../recipes/debugging-a-hung-settlement.md) recipe
walks from the `"timeout"` symptom back to the offending primitive.

## The rare dedicated thread

Some work genuinely blocks at the *edges* of the engine — a TCP listener's
`accept` loop, the audio output callback, an RPC server's socket. Those spawn a
real OS thread, deliberately: it's blocking I/O that *should* live off the
scheduler, isolated so it can't pin a pool worker. That's the **exception** — a
handful of infrastructure capabilities — not how actors run, and not something to
reach for from ordinary actor logic (use one of the three shapes above). It's a
cap-local spawn, scoped tightly to the blocking call ([ADR-0050](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0050-llm-completion-sink.md)).

## Where to read more

- The contracts this page implements — [Invariants & guarantees](../foundations/invariants.md).
- The dispatch machinery behind them, drawn out — [The scheduler](scheduler.md).
- The mail spine and the per-recipient ordering guarantee — [Mail, kinds & scheduling](mail-and-kinds.md).
- Settlement and the hold contract in depth — [Tracing & settlement](tracing-and-settlement.md).
- Offloading heavy compute — [The computation DAG](dag.md).
