# The frame lifecycle

> **Governing ADR:** [ADR-0082](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0082-application-declared-lifecycle-sequence.md)
> (application-declared lifecycle sequence). The model — a declared graph of
> stages, settlement-gated advance, per-stage subscription on `aether.lifecycle`
> — is **stable**, and the stage-kind vocabulary (`Tick`, `Render`, `Present`,
> `Shutdown`, `Quit`) ships today. The **per-chassis graphs are still being built
> out**: desktop and `test_bench` run a `Tick → Render → Present` frame; headless
> stays tick-only; the `InitCaps` / `InitComponents` boot stages exist in the kind
> vocabulary but aren't yet wired into a shipped graph. Build on the stage kinds
> and the subscribe surface; read `aether-capabilities` and the ADR for the graph
> a given chassis declares.

A component never runs a loop. The substrate owns the frame, and an actor reaches
the frame the same way it reaches anything else — by mail. Each binary that hosts
the engine (the desktop chassis, the headless chassis, a future replay or
fixed-timestep host) declares its lifecycle as an ordered graph of stages, and the
substrate ships one capability, `LifecycleCapability` at the `aether.lifecycle`
mailbox, that walks the graph and broadcasts each stage in turn. An actor that
wants a stage — `Tick` to step, `Render` to submit geometry — subscribes to it;
the cap fans the stage out to every subscriber. This is the same publish/subscribe
shape as [input streams](input.md), on a different mailbox.

## Why it exists

Every chassis runs the same skeleton — a cadence source, a per-frame broadcast,
the drain that waits for the frame's work to finish, and a present or capture at
the end — and each one drives a different cadence: the desktop chassis paces to
vsync, the headless chassis to a fixed timer, the test bench steps one frame at a
time. Encoding that skeleton as hand-written loop code in each binary forces the
same structure to be re-derived per chassis, and the only thing that actually
differs between them is the cadence and which stages they run.

Declaring the lifecycle as data collapses the loop to one shape. The application
states *what stages exist and in what order*; the driver owns *how to walk them*;
the chassis main loop owns *only the cadence* — when to ask for the next step.
Adding or reordering a stage is an edit to one graph, not four loops. The cadence
difference lives in the chassis's advance loop and the stages it declares, not in
duplicated frame structure. And because the driver is a normal actor at a real
mailbox, every frame-stage chain has a labelled root in the
[trace tree](tracing-and-settlement.md) — "what triggered this mail?" walks
straight up to `aether.lifecycle`'s broadcast of the stage.

## What it does

**One mailbox, a graph of stages.** Everything addresses `aether.lifecycle`, owned
by the `LifecycleCapability` actor — the sole owner of the compiled graph, the
subscriber table (`KindId → set of mailboxes`), the fan-out, and the settlement
gating. The cap is a bridged singleton, so a wasm guest names it by type:
`ctx.actor::<LifecycleCapability>()`.

**Stages are empty signals.** Each stage kind is a zero-sized type in
`aether-kinds`; the broadcast *is* the signal, carrying no payload. Any per-frame
data a subscriber needs rides its own mail — the camera computes a view-projection
matrix on `Tick` and publishes it to `aether.render`, rather than threading it
through a stage. The stage-kind vocabulary:

| Stage kind | Wire name | Role |
|---|---|---|
| `Tick` | `aether.lifecycle.tick` | per-frame step; the kind every component touches |
| `Render` | `aether.lifecycle.render` | submit geometry after the whole `Tick` chain settles |
| `Present` | `aether.lifecycle.present` | post-render ordering point and the graceful-quit drain edge |
| `Shutdown` | `aether.lifecycle.shutdown` | terminal; graceful cleanup with the mail surface still live |
| `InitCaps` | `aether.lifecycle.init_caps` | capability boot pass (in the vocabulary; not yet in a shipped graph) |
| `InitComponents` | `aether.lifecycle.init_components` | component boot pass (likewise) |
| `Quit` | `aether.lifecycle.quit` | the one escape signal, mailed in to request shutdown |

Two more kinds are the cadence wire, not stage broadcasts: `LifecycleAdvance`
(`aether.lifecycle.advance`) is what the chassis main loop sends to ask for the
next step, and `LifecycleAdvanceComplete` (`aether.lifecycle.advance_complete`) is
the reply it waits on.

**The graph is a builder over kind types.** A chassis builds its graph with
`LifecycleGraphData::builder()`, naming each stage by its kind and the edge out of
it. The desktop and test-bench frame:

```rust
LifecycleGraphData::builder()
    .state::<Tick>()    .next::<Render>()
    .state::<Render>()  .next::<Present>()
    .state::<Present>() .next::<Tick>()    .quit::<Shutdown>()
    .terminal::<Shutdown>()
    .start::<Tick>()
    .build()
```

The builder's type-state enforces at compile time that every `.state::<S>()` is
followed by a `.next::<T>()` before the next state, and that exactly one
`.start::<S>()` is set. `.build()` returns a `BuildError` if an edge targets an
unregistered kind, a kind is registered twice, or the graph has no terminal — so
a malformed lifecycle fails at chassis-build, not at runtime.

**Settlement gates each advance.** The chassis main loop drives cadence by mailing
`LifecycleAdvance` to the cap once per step. On each advance the cap broadcasts the
current stage to its subscribers, subscribes settlement on that broadcast's chain
root, and waits: it advances the state pointer along the resolved edge and replies
`LifecycleAdvanceComplete` only once that stage's whole chain has
[settled](tracing-and-settlement.md). Cadence couples to actual work completion —
`Render` broadcasts only after every actor's `Tick` handler has finished, so a
render producer submits against fully-settled cross-actor state, never a
half-updated frame. A chassis that overruns its cadence and sends a second
`LifecycleAdvance` while one is still in flight sees it warn-drop rather than skip
a stage. (When no settlement registry is wired — a bare test harness — the cap
falls back to fire-and-advance, replying immediately.)

**Quit is one signal with one consumption point.** A chassis bridges OS-level
termination — ctrlc, the window's close button, a future hub-shutdown mail — to a
single `Quit` mail. The cap sets a `quit_pending` flag on receipt and consumes it
at the next state whose graph declares a `quit` edge. With the quit edge on
`Present`, a quit requested mid-frame lets the in-flight `Tick → Render → Present`
cycle finish before the lifecycle advances to `Shutdown` — the frame drains before
exit. Placing the quit edge elsewhere expresses a different shutdown policy; the
topology spells out the answer.

**`Shutdown` and `unwire` are different teardowns.** The `Shutdown` broadcast
arrives as mail with the actor's full mail surface still operational — work that
needs to talk to peers (save state, flush a write, post a metric) belongs in a
`Shutdown` handler, and the driver waits for the `Shutdown` chain to settle before
the loop exits. *Then* the chassis tears actors down, running each actor's
`unwire` finaliser (release native handles, drop GPU resources). `Shutdown` is the
graceful "everything still works" cleanup; `unwire` is the post-lifecycle "the
world is going away" finaliser. The two aren't interchangeable.

**The interrupt/stage split.** Key presses, mouse movement, and resizes are
asynchronous *interrupts* — they arrive whenever the platform produces them, on
`aether.input`. `Tick`, `Render`, and `Present` are frame *stages* — they fire in
declared order, paced by the chassis, on `aether.lifecycle`. Both reach an actor
as ordinary mail in arrival order on its single dispatcher thread, and both use the
same subscribe shape, but they're different mailboxes for a reason: one is the
frame, the other is everything that interrupts it.

## How to use it

**Subscribe a stage in `wire`.** `init` can't send mail (its context is
resolver-only), so a stage subscription goes in the `wire` hook, which runs
post-init with mail allowed — the same site as an [input](input.md) subscribe,
addressing a different cap:

```rust
fn wire(&mut self, ctx: &mut FfiCtx<'_>) {
    let lifecycle = ctx.actor::<LifecycleCapability>();
    lifecycle.subscribe::<Tick>();
    lifecycle.subscribe::<Render>();
}
```

`subscribe::<K>()` subscribes the calling actor — the cap reads the subscriber off
the inbound's host-stamped `Source`, so you name neither the stage id nor your own
mailbox. To subscribe a *different* mailbox (the rare cross-mailbox case) use
`subscribe_for::<K>(other_mailbox)`; the reflexive `unsubscribe::<K>()` and the
explicit `unsubscribe_for::<K>(mailbox)` are the teardown twins. You don't
unsubscribe on the way out — the host clears your subscriptions when the component
drops.

Then handle each stage as its kind, like any other mail:

```rust
#[handler]
fn on_tick(&mut self, ctx: &mut FfiCtx<'_>, _tick: Tick) { /* advance one frame */ }
```

The reference `aether-camera` subscribes `Tick` and `Render` this way — it
computes its camera matrix on `Tick` and publishes it to `aether.render` on
`Render`; `aether-mesh-viewer` subscribes `Render` to replay its mesh each frame.

**A stage your chassis doesn't declare fails fast.** Subscribing to a stage the
local graph doesn't carry replies `aether.lifecycle.subscribe_result` with an
`Err` — an actor authored against a stage its chassis omits learns at boot, not by
silently never firing. On the fire-and-forget subscribe path the error
warn-drops, so a component that subscribes `Render` runs unchanged on a chassis
that lacks the stage: it simply never receives `Render`.

**From an agent over MCP.** Ticks are the substrate's own cadence — you don't pump
them by hand. The lifecycle advances under the chassis loop, so the way you observe
it is through a subscribed component's behavior or its logs. Mailing a stage kind
to `aether.lifecycle` yourself bypasses the driver and isn't the way to step the
engine; let the chassis drive the frame.

## The per-chassis graphs today

Which stages a chassis declares is its own choice; the driver walks whatever graph
it's handed. The shipped graphs:

- **Desktop and `test_bench`** run `frame_lifecycle_config` —
  `Tick → Render → Present → Tick`, looping, with the `Quit` escape to a
  `Shutdown` terminal on `Present`. A full `Tick → Render → Present` cycle runs
  per frame; `Render` broadcasts only after the `Tick` chain settles, and GPU
  submit/present runs after `Render` settles, so a submission integrates the
  fully-settled state of the frame.
- **Headless** runs `tick_only_lifecycle_config` — `Tick → Tick`, looping, with
  the `Quit` escape to `Shutdown` on `Tick`. Its render capability is a no-op, so
  a `Render` stage would settle to no work; a component that subscribes `Render`
  here gets the fail-fast `Err` and is a no-op on render, while its `Tick` path
  runs unchanged.
- **The hub** is a coordinator, not a frame-driven host — it runs no lifecycle
  graph.

The `InitCaps` / `InitComponents` boot stages are declared in the kind vocabulary
and the ADR's stage model, but the shipped graphs start at `Tick`; a chassis that
needs a two-pass boot broadcast adds them to its own graph.

## How to extend or reuse it

- **A new chassis** declares its own graph and drives its own cadence. A replay
  host that steps to recorded frame times, a fixed-timestep physics host, or a
  test harness that steps once all drop in by building a graph and looping
  `LifecycleAdvance` until the reply's `next` reaches a terminal — the driver is
  unchanged.
- **A new stage** is a kind plus a `.state::<K>().next::<…>()` edge in the chassis
  graph; actors that want it subscribe by type. There's no driver code to widen —
  the cap fans out by `KindId`, and a chassis that omits the stage simply doesn't
  declare it.
- **Any actor can subscribe**, not just components — a native capability that needs
  the tick subscribes through the same mail. What an actor receives is exactly the
  stages it asked for on a chassis that declares them.

## Where to read more

- The declared-graph decision, the builder type-state, the quit-edge semantics,
  and the per-chassis realizations —
  [ADR-0082](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0082-application-declared-lifecycle-sequence.md).
- How a stage advance waits on its whole chain, and what "settled" means —
  [Tracing & settlement](tracing-and-settlement.md).
- The interrupt side of the split — `Key`, `MouseMove`, `WindowSize` on
  `aether.input` — [Input streams](input.md).
- The `wire` hook, `init` versus `wire`, and writing handlers —
  [Components & lifecycle](components.md).
