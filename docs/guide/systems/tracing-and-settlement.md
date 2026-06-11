# Tracing & settlement

> **Governing ADRs:** [ADR-0080](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0080-substrate-mail-tracing-and-settlement.md) (mail lineage + settlement detection),
> [ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md) (settlement decoupled from the trace stream),
> [ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md) (hold-until-resolve dispatch),
> [ADR-0094](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0094-settlement-obligation-guard.md) (the owned-dispatch obligation guard). The **contract**
> — what "settled" means and what you must uphold for it — is **stable**. The
> **mechanism** that delivers it (the emit-time per-root counter, the per-actor
> trace rings, the guided walk that rebuilds a tree) is **settling**, so this page
> documents the contract and defers the internals to those ADRs.

Send one mail and the handler that receives it may send more, and those handlers
more again — a single message fans out into a cascade of downstream work.
**Settlement** is the engine's answer to the question that cascade raises: *has
everything this mail set in motion finished?* **Tracing** is the same causal
lineage seen as a tree — what happened, in what order, and how long each hop
took.

The two are built from one body of bookkeeping but serve different needs. An
agent driving the engine leans on settlement to know an effect has fully landed
before it reads a frame or sends the next thing, and on the trace tree to see
*why* an exchange was slow or *where* a chain stalled. An author writing a
capability or component needs the model because a handler that replies late owes
its chain an obligation — miss it and a caller hangs with nothing named.

## Why it exists

The engine needs to know a piece of work is fully *done* only at specific points
— when something has to happen atomically, once everything an earlier mail set in
motion has finished. A frame can't advance until the tick's effects have played
out; a component swap waits for the old instance to finish draining; a lifecycle
step waits for wiring to complete; an agent waits for its mail's effects before
it reads the result back. What's being waited on is never one mail but the whole
chain of work it set off, and no single mail can report that the chain has closed
— the work that matters is everything it *triggered*.

Without an exact signal, the only way to wait is to approximate — and every
approximation of "has this chain closed" races. You could poll a counter until it
looks quiet, wait out a fixed window, or send a redundant mail to ride FIFO
ordering; each holds up on a quiet machine and breaks under load, because nothing
tells the waiter that the last mail in the chain is still on its way. Settlement
removes the guesswork: it tracks the causal lineage of every mail, so a gate waits
on a fact rather than a timeout. That same lineage, kept anyway, doubles as a
complete cause-and-effect graph — the trace tree.

## The model: lineage and the settled condition

**Every mail belongs to a causal chain, named by its root.** A *root* is a mail
sent from outside any handler — the chassis dispatching a `Tick`, a lifecycle
step (`init` / `wire` / `unwire`), a mail injected over MCP or bridged from the
hub, a capability's worker thread reacting to a socket read. Mail sent from
*inside* a handler inherits its trigger's root instead of starting a new one. So
one external stimulus and the entire cascade it sets off all carry the same root
id, and the whole chain is identified by that single originating mail. (Chains
that the chassis itself originates are marked by a sender of `aether.chassis` —
the tagged id `mbx-aaaa-aaaa-aaaa` you'll see at the top of a trace tree.)

**A chain is settled when it is closed.** Under each root the engine keeps two
counts: `in_flight` — mail sent but not yet finished handling — and `held_open`
— explicit holds, described next. The chain settles the instant both reach zero.
Given the hold contract below is honoured, that signal is **exact**: the counts
never transiently dip to zero with work still on the way, so the chain reports
settled *once*, as a fact rather than a hint. That exactness is the whole point —
it's what lets a waiter trust the signal outright instead of falling back to a
timeout. The [invariants page](../foundations/invariants.md) states this as a
guarantee; here is the obligation that earns it.

## The hold contract

The counts are only exact if every mail that is still *coming* is already
counted. For a **synchronous reply** that falls out for free: a handler that
replies before it returns records the reply's send ahead of its own finish, so
the chain can't reach zero with that reply outstanding. Nothing to do.

The gap is **deferred work** — a handler that kicks off a slow call and replies
in a *later* turn, after it has already returned. In the window between the
return and the eventual reply, that handler's mail is finished and nothing new is
yet in flight, so the counts would fall to zero and the chain would settle early,
reporting "done" while the real answer is still being computed. A waiter would be
told the work finished before its reply ever arrived.

A **settlement hold** closes that gap. A handler with deferred work takes a hold
on its root before returning and keeps it until the last send goes out; the hold
sits in `held_open`, so the chain stays open across the window, and releasing it
*after* the reply is what finally settles the chain. You rarely take a hold by
hand — the sanctioned offload primitives manage it for you. `ctx.dispatch_blocking`
(and the `#[handler(task)]` completion sugar) acquire the hold when they spawn
the worker and release it when you `resolve`, reply-then-release in that order so
the settle can't beat the reply; `spawn_inherit` holds for the worker's lifetime.
`acquire_settlement_hold` is the explicit handle for the rare hand-rolled case,
such as buffering a request to drain in a later turn. The full offload story —
which primitive for which shape, and why a raw `std::thread::spawn` silently
opts work *out* of settlement — lives on
[Concurrency & blocking](concurrency.md); this page is the *why* behind the hold.

## The obligation guard

There is a second way to break a chain, on the receiving side. A capability that
owns an **inbox** mailbox — one that takes an *owned* dispatch and is expected to
move it onward to a downstream channel — carries the finish obligation itself: it
must record the inbound mail as `Finished`, or its chain never closes. The
substrate brackets the two synchronous mailbox shapes automatically — an inline
handler, and the standard actor dispatcher at handler exit — so an ordinary actor
never has to think about this. What's on its own is a mailbox drained *by hand*,
off the pool, and the engine has exactly one such case today: the **desktop
chassis's window driver**. Window operations have to run on the OS event-loop
thread rather than a pool worker, so `aether.window` is registered as an inbox the
event loop drains itself, recording each inbound mail's `Finished` as it applies
the op and sends the reply ([Window](window.md) covers that mailbox's surface).
That's the lone precedent — but the same obligation lands on any future driver
that owns and drains its own mailbox off-thread.

Forget it and you get the worst diagnostic the runtime offers: a silent
multi-second hang with no actor and no mail named, ending only when the
settlement or MCP timeout fires. [ADR-0094](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0094-settlement-obligation-guard.md) converts that into an immediate, located
failure in debug builds. Every owned dispatch must end one of two ways —
**discharged** (*the obligation ends here; I am recording `Finished`*) or
**transferred** (*the obligation moves with the work onto a downstream
envelope*) — and an owned dispatch dropped while still armed panics at the leaking
seam, naming the `mail_id`, kind, and mailbox. Release builds carry no field and
no check, so the guard costs nothing where it isn't wanted. The rule for any drain
you write: discharge what ends with you, transfer what you pass on.

## The trace tree

The same lineage keys that drive settlement — each mail's id, its parent, its
root — also reconstruct the full causal graph. A trace tree is a set of **mail
nodes**; each names who sent the mail, who received it, the kind, and a series of
timestamps sampled as the mail moves through the engine. You walk a chain by
following `parent` edges down from the root, and the timestamps localize the slow
hop in a cascade without instrumenting anything. Here is where each one is taken,
for a single mail:

```text
●  t_construct_start    the sender's outbound blob opens (its first buffered send)
│     construct         the rest of the sending handler runs, buffering more sends
●  t_sent               flush — at the handler boundary the blob is routed
│     queued            wakeup + scheduling: waiting for a worker to take the blob
●  t_enqueue            a worker picks up the blob (enters its run cycle)
│     drain             earlier mail in the blob is dispatched ahead of this one
●  t_received           this mail's handler is entered
│     handler           the handler runs
●  t_finished           the handler returns
```

The two spans you reach for most are **queue latency** — how long the mail waited
before a handler ran it — and **handler duration** (`t_finished − t_received`).
The finer **queued** / **drain** split says *why* a hop waited: scheduling
pressure (no worker free yet) versus a long serial fan-out dispatched ahead of it
in the same blob (the blob model is on [Concurrency & blocking](concurrency.md)).
A node still missing `t_finished` is mail that hasn't finished handling yet.

**Where it lives, and how to read it.** Trace events aren't kept centrally. Each
actor holds its own **trace ring** — the same per-actor storage logs use
([ADR-0081](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0081-decentralized-per-actor-log-storage.md) / [ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md)) — recording the events that passed through it. A tree is
rebuilt by a **guided walk**: start at the root's sender, read its ring
(`aether.trace.tail`, the sibling of the `aether.log.tail` behind `actor_logs`),
follow each onward `Sent` to the recipient's ring, and stitch. `send_mail_traced`
runs that walk and hands back the stitched tree — and over MCP that's the surface,
since there's no standalone per-actor trace tool the way `actor_logs` exposes the
log rings. The tree it returns carries `t_construct_start`, `t_sent`,
`t_received`, and `t_finished` per node; the `t_enqueue` pickup point and the
ready-queue depth live in the rings but aren't surfaced there, so over MCP queue
latency reads as one `t_sent → t_received` span.

Tracing and settlement are deliberately **decoupled** ([ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md)) because they have
opposite requirements. Settlement is control-plane: exact, on the frame's critical
path, never allowed to lag or settle early. Tracing is observability: best-effort
and off the critical path, so it can lose data without ever affecting whether or
when a chain settles. The line to hold onto is that the settled signal is never
wrong while a trace tree can be honestly incomplete — so don't reach for the tree
to decide whether work is *done*; that's settlement's job. Reach for it to see
*what* happened and *how long* it took.

**Gotcha — a trace is a bounded window, not a durable log.** The rings wrap: under
load, or simply with enough elapsed time, an actor's oldest entries are
overwritten. Because overwriting any one node leaves a hole its tree can't be
faithfully rebuilt from, the whole chain is dropped rather than served partial —
and a chain still in flight when its entries lap is dropped with a warning. A tree
self-reports where it was truncated, but old or high-volume chains can come back
incomplete or not at all. Read a trace promptly after the work; don't count on
reconstructing something from minutes ago or buried under a burst.

## How an agent uses it

`send_mail` already rides settlement on your behalf. Each item blocks until its
chain settles and hands back the correlated reply, so a request/reply — mail
`aether.fs.read`, get the bytes — is a single call with no polling.
`fire_and_forget` opts an item out for a poke you don't wait on.

`send_mail_traced` is the tool for when you want the *tree*, not just the reply.
It dispatches a batch under one shared root and, once that whole chain settles,
returns the combined trace tree, the correlated replies, and a `status`:

- `"settled"` — the chain closed; `mails` holds the tree (nodes with `parent`
  edges and timings) and `in_flight` reads `0`.
- `"timeout"` — the chain didn't settle within `settlement_timeout_ms` (default
  300s, clamped to 600s). A timeout is the bound on a hung chain, and the usual
  cause is exactly the two failures above: a deferred reply that never held its
  chain open, or a drain that dropped its finish obligation upstream. The triage
  path from this status to the offending handler is the
  [Debugging a hung settlement](../recipes/debugging-a-hung-settlement.md) recipe.
- `"dispatched"` — only with `fire_and_forget`; the shared root acked, no
  settlement wait.

Reach for `send_mail_traced` over `send_mail` when you need proof a whole cascade
finished rather than just one reply, the timing breakdown of a slow exchange, or
an all-or-nothing batch where a bad spec aborts before any mail moves. For
independent items where each reply is all you want, plain `send_mail` is simpler.
Both surfaces are covered operationally on [The MCP harness](../mcp-harness.md).

## Where to read more

- The rules this page earns, stated as guarantees — [Invariants & guarantees](../foundations/invariants.md).
- How to offload without blocking — the three spawn shapes and the hold in
  practice — [Concurrency & blocking](concurrency.md).
- The agent-facing tool surface for `send_mail` / `send_mail_traced` —
  [The MCP harness](../mcp-harness.md).
- The lineage model and the emit-time counter —
  [ADR-0080](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0080-substrate-mail-tracing-and-settlement.md)
  and [ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md);
  the hold-until-resolve dispatch primitive —
  [ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md);
  the owned-dispatch obligation guard —
  [ADR-0094](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0094-settlement-obligation-guard.md).
