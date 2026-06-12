# Debugging a hung settlement

**Class:** drive-only. **Prereq:** the MCP harness is up against a running
engine — no build. This recipe is a triage runbook for the engine's worst
diagnostic shape: a multi-second silence ending in a timeout with no actor
named. Read it symptom-first; each step narrows from "the chain hung" to the
exact handler or drain that owes the chain its close.

The model behind the moves here — what "settled" means, the hold contract, and
the obligation guard — lives on [Tracing & settlement](../systems/tracing-and-settlement.md).
This page assumes that model and walks the path from a stuck chain to its cause.

## The symptom

You get one of two things back:

- `send_mail_traced` returns `status: "timeout"` — no `mails` tree, `in_flight`
  unresolved. The chain didn't settle within `settlement_timeout_ms` (default
  300s, clamped to 600s).
- A plain `send_mail` item hangs for the full settlement window and then fails.
  `send_mail` waits on settlement per item, so a chain that never closes reads
  as a multi-second stall with nothing named.

Both mean the same thing: a chain opened and never closed. Two faults account
for almost every one — a deferred reply that never held its chain open, and an
owned drain that dropped its finish obligation. The third possibility is a chain
that is merely slow. The steps below tell them apart.

## Step 1 — get the partial tree and find the frontier

Re-issue the work under `send_mail_traced` so you have the trace surface, and
lower the wait so you are not blocked for the full default window:

```text
send_mail_traced(engine_id, mails, settlement_timeout_ms: 5000)
```

On a genuinely hung chain this still returns `status: "timeout"` with no tree —
a timeout carries no root, tree, or replies. To read the partial lineage,
re-issue against a window long enough to let the trace walk run but short of the
real hang, or inspect the chain that is still in flight by walking from its
sender. The node you want is the **frontier**: the deepest mail node that has a
`t_received` but no `t_finished`. A node missing `t_finished` is mail whose
handler was entered and never returned a finish — that is where the cascade
stopped. Its recipient is the actor to look at next.

If the tree comes back at all, it self-reports where it was truncated; the
frontier is the live edge of the lineage, not the truncation point. If it comes
back empty under `"timeout"`, fall through to Step 2 using the recipient you
mailed and the actors one hop downstream of it.

## Step 2 — read the frontier actor's logs

Point `actor_logs` at the frontier actor's mailbox to see what its handler was
doing when the chain stopped:

```text
actor_logs(engine_id, mailbox_name: "<frontier mailbox>", max: 100)
```

Use the full mailbox address — a chassis mailbox (`"aether.window"`,
`"aether.fs"`) or a loaded component's lineage address
(`"aether.component/aether.embedded:NAME"`). Only in-actor `tracing::*` events
reach the rings, so what you get is the handler's own narration: the off-thread
call it kicked off, the reply it meant to send, the drain it was running. That
narration plus the frontier's shape is what classifies the fault.

## Step 3 — classify the fault

**A. Deferred reply that never held the chain open.** The frontier is a handler
that started slow off-thread work and returned, but the chain settled (or would
settle) before the reply went out — or the reply is simply never sent. This is a
missing settlement hold: a handler that replies in a *later* turn must hold its
root across the window between return and reply. The fix is to route the offload
through a sanctioned primitive that takes the hold for you — `dispatch_blocking`
(with the `#[handler(task)]` completion that calls `resolve`) holds until you
resolve; `spawn_inherit` holds for the worker's lifetime — rather than a raw
`std::thread::spawn`, which mints rootless mail the settlement umbrella can't
see. The hold table on [Concurrency & blocking](../systems/concurrency.md)
matches each primitive to the shape of work it covers. `acquire_settlement_hold`
is the explicit handle for the rare hand-rolled buffer-and-drain case.

**B. Owned dispatch that dropped its finish obligation.** The frontier is a
mailbox where a capability took an *owned* dispatch and was supposed to record it
`Finished` or transfer the obligation downstream, and did neither. The
claimed-mailbox drain handles this by construction now — a `ClaimedInbox` yields
each mail as a guard that settles on scope exit ([ADR-0106](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0106-settlement-safe-by-construction.md)), so the live
suspects are the in-crate relay / park / fan-out seams that move a dispatch onward
by hand. In debug builds the obligation guard
([ADR-0094](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0094-settlement-obligation-guard.md))
turns a dropped obligation into an immediate panic at the leaking seam, naming the
`mail_id`, kind, and mailbox — so check engine stderr for that panic before
anything else; it points straight at the seam. In a release build the guard is
absent and the chain just hangs, leaving the frontier-plus-logs walk as the way
in. The rule for any relay you write: discharge what ends with you, transfer what
you pass on.

**C. The chain is merely slow.** No handler is stuck — the frontier keeps
advancing across re-reads and the timing spans show a real, long hop (a
multi-second provider call, a large fan-out). Raise `settlement_timeout_ms`
toward its 600s ceiling and confirm against the tree: the slow node's
`t_received → t_finished` span (handler duration) or its `t_sent → t_received`
span (queue latency) should account for the wait. If the timing adds up, the
chain was never hung — it needed a wider window.

## Reproduce on demand

To stage a hung chain deliberately — to confirm a fix or to study the frontier
shape — inject the offending mail under `send_mail_traced` against a fresh
engine from `spawn_substrate`, with a short `settlement_timeout_ms` so the
`"timeout"` lands fast. A handler that defers a reply without taking a hold, or
a relay closure that skips its `transfer`, reproduces fault A or B respectively.
The desktop chassis's `aether.window` driver is the lead claimed-mailbox drain in
the tree, so window ops are the live example of the settle-by-construction
shape — its guards close the chain whether the op applies, the payload fails to
decode, or the window is torn down mid-drain.

## Gotcha — read the trace promptly

A trace is a bounded window, not a durable log. Each actor's trace ring wraps,
so under load or after enough elapsed time the oldest entries are overwritten,
and overwriting any one node drops the whole tree — a faithful rebuild needs
every node. A chain still in flight when its entries lap comes back incomplete
or not at all, with a warning. Pull the trace right after the hang reproduces;
don't expect to reconstruct one from minutes ago or from under a burst.

## Verify against current code

Confirm the named surfaces still exist before following this recipe: the
`send_mail_traced` / `actor_logs` tools and their `settlement_timeout_ms` /
`mailbox_name` arguments in `aether-mcp`; the offload primitives
(`dispatch_blocking`, `spawn_inherit`, `acquire_settlement_hold`) in
`aether-substrate`; and the obligation guard in
[ADR-0094](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0094-settlement-obligation-guard.md).
If a name has drifted, fix the recipe as part of the work — see the staleness
rule on [Recipes](../recipes.md).
