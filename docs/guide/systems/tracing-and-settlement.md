# Tracing & settlement

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

> Governing ADRs: **[ADR-0080](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0080-substrate-mail-tracing-and-settlement.md)** (mail lineage + settlement detection),
> **[ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md)** (settlement decoupled from the trace stream),
> **[ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md)** (hold-until-resolve dispatch),
> **[ADR-0094](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0094-settlement-obligation-guard.md)** (the owned-dispatch obligation guard). The **contract**
> — what "settled" means and what you must uphold for it — is **stable**. The
> **mechanism** that delivers it (the emit-time per-root counter, the per-actor
> trace rings, the guided walk that rebuilds a tree) is **settling**, so this page
> documents the contract and defers the internals to those ADRs.

## Why it exists

The engine is forever asking whether a piece of work is *done*. A frame can't
advance until the tick's effects have played out; a component swap waits for the
old instance to finish draining; a lifecycle step waits for wiring to complete;
an agent waits for its mail's effects before reading the result back. Every one
of these is the same question — is this causal chain closed? — and none of them
can be answered by looking at a single mail, because the work that matters is
everything that mail *triggered*.

The old answer was to guess with a deadline: poll a counter until it looks quiet,
wait a fixed window, send a redundant mail to ride FIFO ordering. Those
heuristics raced — they passed on a quiet machine and flaked under load, because
the substrate had no way to know the last broadcast was still coming and could
only assume it had already arrived. Settlement replaces the guess with an exact
signal, and it
does so by tracking the one thing the guess was missing: the causal lineage of
every mail. That lineage, kept anyway, is also a complete cause-and-effect graph
— which is the trace tree.

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
substrate brackets the two synchronous mailbox shapes automatically (an inline
handler, and the standard actor dispatcher at handler exit), but a hand-rolled
drain — the desktop window driver is the precedent — is on its own.

Forget it and you get the worst diagnostic the runtime offers: a silent
multi-second hang with no actor and no mail named, ending only when the
settlement or MCP timeout fires. ADR-0094 converts that into an immediate, located
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
nodes**; each node names who sent the mail, who received it, the kind, and three
timestamps: `t_sent`, `t_received` (handler entry), and `t_finished` (handler
exit). You walk a chain by following `parent` edges down from the root, and the
timestamps break each hop into its queue latency (`t_received − t_sent`) and its
handler duration (`t_finished − t_received`) — enough to find the slow hop in a
cascade without instrumenting anything. A node still missing `t_finished` is mail
that hasn't finished handling yet.

Tracing and settlement are deliberately **decoupled** (ADR-0086), because they
have opposite requirements. Settlement is control-plane: exact, on the frame's
critical path, and never allowed to lag or settle early. Tracing is observability:
best-effort, off the critical path, kept in per-actor rings that wrap and drop
their oldest nodes under load. The consequence worth holding onto is that a trace
tree can be honestly *incomplete* — it self-reports where it was truncated —
while the settled signal is never wrong. So don't reach for the tree to decide
whether work is done; that's settlement's job. Reach for it to see *what*
happened and *how long* it took.

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
  chain open, or a drain that dropped its finish obligation upstream.
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
