# Concurrency & blocking

> **Governing ADRs:** [ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-burst-unit-of-dispatch.md) (the burst dispatch model + scheduler), [ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md)
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
([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-burst-unit-of-dispatch.md)). The unit the pool hands around is a **burst** — one handler
execution's buffered sends, grouped by recipient and published for idle workers
to race — and a per-actor **run-token** admits one worker to a given actor at a
time. The token is what makes an actor single-threaded (so actor state is plain
fields with no locks) and what makes **per-recipient FIFO fall out for free**
while distinct recipients run concurrently — exactly the ordering spine the
[invariants](../foundations/invariants.md) and [mail](mail-and-kinds.md) pages
rely on. The full machinery — the outbound mail ring, burst formation, the
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
survives across turns through a typed request context: call
`send_with_context(&request, &context)` and later recover it with
`ctx.take_context::<Context>()`, so duplicate in-flight requests don't get
confused (the surviving half of [ADR-0042](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0042-synchronous-mail-wait.md)). The context should be small
bookkeeping; bulky state belongs in actor fields. Every request/reply in the
engine works this way: `aether.fs.read` → `…read_result`, reply-to-sender, and
the rest. Use the lower-level `send_tracked` / `ctx.in_reply_to()` only when the
raw request id is itself the domain key.

**2. Blocking I/O or slow off-thread work → `dispatch_blocking` + `#[handler(task)]`.**
When a (native) capability must make a genuinely blocking call — a multi-second
provider request, a subprocess — it hands the work *off* the scheduler thread
([ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md)):

```rust
#[handler::request]
fn on_generate(
    state: &mut Self::State,
    ctx: &mut NativeCtx<'_>,
    req: Generate,
) -> Pending<GenerateResult> {
    let provider = state.provider.clone();
    ctx.dispatch_blocking(move || provider.call(req))     // runs off the scheduler worker
}

#[handler(task)]                                           // a completion, not inbound mail
fn on_generate_done(
    _state: &mut Self::State,
    ctx: &mut NativeCtx<'_>,
    done: TaskDone<GenerateResult>,
) {
    done.resolve(ctx);                                     // re-reply, then drop the hold
}
```

`dispatch_blocking` spawns the closure on an off-scheduler thread and returns a
`Pending<R>` immediately. The request handler must return that receipt: it declares
the deferred reply kind and prevents the armed dispatch from being silently
discarded. The closure's output comes back later as `TaskDone<Output>` in a
`#[handler(task)]` handler. Task completions route by their Rust output/context
types, not by a wire `KindId`; the output only has to be `Send + 'static`, while
`R` in `Pending<R>` is the reply kind. In the same-output/reply shape above,
consuming `done.resolve(ctx)` sends the reply and then drops the settlement hold.
A completion can instead map a different output to `R` and resolve that value.
This is the sanctioned home for "reply in a later turn"; it replaced the
hand-rolled `InFlightDispatch` the content-gen capabilities used to carry. (Native
capabilities today; a wasm/FFI form is a deferred superset — guests use shapes 1
and 3.)

A panic inside the closure is fatal (ADR-0063): the worker escalates it through
the chassis `FatalAborter` the same way the scheduler escalates a handler panic,
and no completion lands. Return an expected failure as a value instead — a
`Result`-shaped output — and map it to an error reply in the completion.

**Staged work owes no reply → `stage_blocking` + `StagedTask`.** Work that is
not itself the answer to the current caller stages instead
([ADR-0243](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0243-typed-held-replies.md)
§9). `ctx.stage_blocking::<O>()`, or `ctx.stage_blocking_with::<O, C>(context)`
with a context kind, fixes the task's chain in the turn that stages it: it mints
the task's request id from the counter outbound requests use, takes the
settlement hold on this turn's chain, and stores the context under the id.
`StagedTask::start(ctx, work)` only spawns the worker, from whichever turn calls
it. The `#[handler(task)]` completion runs correlated to the task's request on
the staging chain: `ctx.in_reply_to()` is the task's `request()`, the context
comes back with `ctx.take_context::<C>()`, its sends inherit the chain, and
`done.into_output()` discharges it, because nothing is owed. A completion that
leaves a context holding a live `Held` untaken fails fast, as a reply handler
does. Dropping an unstarted task releases its chain and removes its context.

A child birth and a registry batch are staged tasks too. `ctx.spawn_child::<C>(..)`
ends with `.stage()`, or `.stage_with(context)` with a context kind, and
`ctx.stage_registry_batch(batch, context)` stages an owner batch the same way:
each mints a request id, holds the staging turn's chain, and stores its context,
and its `#[handler(task)]` completion (`TaskDone<SpawnOutcome<C>>`,
`TaskDone<RegistryBatchResult>`) takes the context from the ctx and owes
nothing. A birth refused before it is staged hands its context back beside the
`SpawnError`. The context is a kind that names the work, such as an index or a
path; live values the completion needs, a request's `Held` among them, wait in
actor state under the key it names. Requests that need the same work join as
waiters under one key, one list per reply kind, and the one completion answers
them all.

**A bounded queue holds each reply and stages each request's work.** A native
capability that bounds its concurrent blocking calls uses `TaskQueue<R>`
(`aether-http`'s per-sender egress and the workspace run queue are the same
shape). `submit` runs in the request's own turn: it holds the reply with
`ctx.hold::<R>()`, keeps the `Held<R>`, and stages the work, so the task holds
that request's chain whether it starts now or waits for a slot. The completion
is one line:

```rust
#[handler(task)]
fn on_run_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<RunResult>) {
    state.tasks.complete(ctx, done);   // answer the Held, then start the next waiting task
}
```

`complete` finds the finished task's `Held` by the request its completion is
correlated to, answers it with the output, and starts the next waiting task in
the freed slot. The waiting task's chain is still its own request's, so a
request's chain settles when that request is answered, never when another
request's work finishes.

**3. Heavy async compute → off-thread, by reference.** Multi-step compute that
produces handles belongs off the actor thread entirely; stage it through the
offload primitives below and pass results by handle rather than copying them
through mail.

**4. A reply owed until a later turn, with no worker → `ctx.hold`.** When a
(native) handler answers one exact reply kind later from a turn no worker drives
— after a peer's reply, a settlement or monitor notice, a long-poll wake, or a
frame loop — it arms the obligation with `ctx.hold::<R>()`
([ADR-0243](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0243-typed-held-replies.md)):

```rust
#[handler::request]
fn on_watch_head(&mut self, ctx: &mut NativeCtx<'_>, _m: WatchHead) -> Pending<WatchHeadResult> {
    let (pending, held) = ctx.hold::<WatchHeadResult>();
    self.watchers.push(held);                              // the debt waits in state
    pending                                                // the receipt sets the row
}

// later, from any handler on this actor:
held.answer(ctx, &WatchHeadResult { head });               // reply, then drop the hold
```

`hold` parks the caller's settlement hold and reply target in the actor's
in-flight ledger, the same table `dispatch_blocking` fills, and returns the
`Pending<R>` receipt with a move-only `Held<R>` ticket. `answer` replies to the
captured caller with its correlation, whichever turn runs it, and only an `R`
compiles. Work the actor stages for a held request (an offload, a child birth, a
registry batch) owes no reply: the `Held` waits in state under the key the
task's context names, and the completion answers it. The component host's
`continue_from` and `stage_registry_batch_from`, which carry a `Held` onto a
successor that answers the same caller, remain until #7008. A second `hold` in one dispatch panics. Dropping a `Held` unanswered
releases the hold and panics; an actor that closes with tickets still live or
parked while the engine keeps running answers each with its reply kind's
`unanswered()` (the `HeldReply` trait) before releasing its hold, and an
engine teardown releases them silently.

## The offload shapes, and the hold

Settlement — how `send_mail_traced` knows a chain of mail is *fully* done rather
than guessing with a timeout — requires every unit of in-flight work to stay
visible to the trace umbrella. A raw `std::thread::spawn` pushes rootless mail
the umbrella can't see, silently opting the work out. So offloading goes through
one of the sanctioned primitives below, which differ only in *how long they hold
the causal chain open*:

| primitive | holds the chain? | for |
|---|---|---|
| `spawn_inherit` | yes — for the worker thread's lifetime | offloaded work that replies *before* the worker ends |
| `spawn_detached` | no — the worker holds no chain and sends no mail | true fire-and-forget background work |
| `dispatch_blocking` (hold-until-resolve) | yes — until you `resolve`, *outliving* the worker | the "reply in a later turn" shape above |
| `stage_blocking` (staged task) | yes — the staging turn's chain, until the completion handler ends | work that owes no reply, and a bounded queue's waiting work |

A panic in any of them is fatal
([ADR-0063](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0063-fail-fast-on-abnormal-component-lifecycle.md)):
the worker escalates it through the chassis aborter with the panic payload in the
reason, as the scheduler does for a handler panic.

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
reach for from ordinary actor logic (use one of the shapes above). It's a
cap-local spawn, scoped tightly to the blocking call ([ADR-0050](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0050-llm-completion-sink.md)).

A cap spawns that thread from the handle `ctx.self_wake::<K>()` returns (on the
`init` ctx as well as a handler's): `wake.spawn_sidecar(name, body)`. A panic in
`body` fails the chassis fast, like a handler panic, even after the actor has
closed. The thread wakes its actor through the same `SelfWake<K>`, never through
a stored mailbox id plus a mailer. The handle names no position and sends nothing
but that one wake, and a wake after the actor has dropped does nothing
([ADR-0230](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0230-proven-actor-references.md)).

A thread that must decide about a peer before it wakes its actor also holds the
`ActorProbe` that `ctx.actor_probe()` returns on the `init` ctx. It answers
whether the actor a proven reference names is `Live` now and whether it accepts a
kind, and nothing else.

## Where to read more

- The contracts this page implements — [Invariants & guarantees](../foundations/invariants.md).
- The dispatch machinery behind them, drawn out — [The scheduler](scheduler.md).
- The mail spine and the per-recipient ordering guarantee — [Mail, kinds & scheduling](mail-and-kinds.md).
- Settlement and the hold contract in depth — [Tracing & settlement](tracing-and-settlement.md).
