# ADR-0021: Publish/subscribe routing for substrate input streams

- **Status:** Proposed
- **Date:** 2026-04-17

## Context

The substrate owns the winit event loop, the GPU surface, and the per-frame tick. Events that originate at the platform layer — `aether.tick`, `aether.input.key`, `aether.input.mouse_move`, `aether.input.mouse_button` — need a path to the components that care about them. Today none exists. `App::default_component: Option<MailboxId>` is initialized to `None` and never assigned, so:

- An empty substrate (`spawn_substrate` with no preloaded components) has no recipient. Winit ticks fire 60 times per second and drop on the floor.
- Loading the hello-component via `aether.control.load_component` registers the component but doesn't wire it as a recipient. The engine is alive, the component is loaded, the GPU is drawing — but the only way to drive a frame is for the agent to mail `aether.tick` directly to the hello-component's mailbox, once per intended frame.
- The "load component, see triangle" demo from the ADR-0010 arc was blocked behind this. We worked around it by manually firing ticks via MCP `send_mail`. Workable for proving the pipeline; falls over the moment a component wants to react to actual keyboard input.

The natural shape isn't "who owns input." It's "who's listening." Multiple components plausibly want the same stream:

- A renderer + a physics step + a telemetry collector all want `tick`. With single-owner routing, two of them have to fan out through the third or be combined into one component — exactly the kind of incidental coupling the mail-first architecture exists to avoid.
- A debug overlay logging keystrokes alongside a game component reacting to them: both subscribe; neither cares about the other.
- "Focus" — a UI panel that swallows keys when active — is a higher-layer concern that components negotiate among themselves (toggle their own subscriptions), not something the substrate needs a primitive for.

Broadcast-shaped input also matches what the rest of the system already does. Observation mail (ADR-0008) fans out to every attached MCP session; reply-to-sender (ADR-0013) targets one. The substrate has no third pattern for "platform events to interested components" — this ADR adds it.

## Decision

The substrate publishes input streams; components subscribe and unsubscribe via reserved control-plane kinds. The substrate maintains a per-stream subscriber set and fans each event out to every subscriber. No notion of ownership; no `default_component`.

### 1. Stream identifier

```rust
pub enum InputStream {
    Tick,
    Key,
    MouseMove,
    MouseButton,
}
```

One variant per substrate-published input kind. Closed enum — adding a new platform event (e.g. `Resize`) is an additive variant plus the substrate-side publisher. Identifying streams by enum rather than kind name keeps the subscription surface tight; agents can't typo a stream name into silently-no-subscriptions.

### 2. Subscribe / unsubscribe kinds

```rust
#[derive(aether_mail::Kind, aether_mail::Schema, ...)]
#[kind(name = "aether.control.subscribe_input")]
pub struct SubscribeInput {
    pub stream: InputStream,
    pub mailbox: u32,
}

#[derive(aether_mail::Kind, aether_mail::Schema, ...)]
#[kind(name = "aether.control.unsubscribe_input")]
pub struct UnsubscribeInput {
    pub stream: InputStream,
    pub mailbox: u32,
}

#[derive(aether_mail::Kind, aether_mail::Schema, ...)]
#[kind(name = "aether.control.subscribe_input_result")]
pub enum SubscribeInputResult {
    Ok,
    Err { reason: String },
}
```

Both subscribe and unsubscribe reply via the same `SubscribeInputResult` shape (reply-to-sender, ADR-0013). Validation:

- Unknown mailbox id → `Err { reason: "no such mailbox" }`.
- Duplicate subscribe (already subscribed to this stream) → `Ok`. Subscriptions are a set, not a counter.
- Unsubscribe of a non-subscriber → `Ok`. Idempotent.

### 3. Substrate plumbing

`App` gains `input_subscribers: HashMap<InputStream, BTreeSet<MailboxId>>`. The platform thread, on each event:

1. Read the subscriber set for the event's stream (read lock or copy-on-write — small set, 60 Hz worst case).
2. Enqueue the mail once per subscriber via the existing scheduler dispatch.

Subscribe and unsubscribe handlers in `control.rs` take the same write lock as `handle_load`/`handle_drop` so subscription mutations are serialized against component table changes.

`BTreeSet` rather than `Vec` so subscriber order is deterministic across runs (useful for reproducing test failures), and so duplicate-subscribe is naturally idempotent.

### 4. Lifecycle interaction

- **Drop.** When `drop_component` tears down a mailbox, the substrate removes that mailbox id from every subscriber set under the same write lock as the drop. Half-dropped subscriptions can't survive.
- **Replace.** `replace_component` preserves the mailbox id (per ADR-0010 §5 and ADR-0022), so subscriptions carry over to the new instance automatically. The new instance starts with the same subscriptions the old one had.
- **Stale subscriptions can't accrue** — every subscriber id in the set always references a live mailbox.

### 5. Empty subscriber sets

A stream with zero subscribers means the substrate publishes nothing — the event is dropped at the source rather than enqueued and discarded. Same end-state as today's "no default," but explicit: `subscribe_input` to enable, `unsubscribe_input` to silence.

### 6. What this ADR does *not* do

- **No exclusive-input primitive.** A component that wants to "swallow" input does so by being the only subscriber (the agent or another component manages that). The substrate has no concept of focus or capture.
- **No per-window streams.** Single-window assumption; if multi-window lands later, the stream enum widens with a window id.
- **No subscribe-all sugar.** Agents call `subscribe_input` once per stream they care about. Cheap; explicit beats magic.
- **No boot-time subscription.** Spawn-substrate starts with empty subscriber sets. The agent sends `load_component` then `subscribe_input`. An additive `subscribe_to: Vec<InputStream>` field on `LoadComponentPayload` is plausible later if the two-call sequence gets annoying.
- **No generalization to "subscribe to any kind."** This ADR addresses substrate-published platform events. Component-to-component subscription is its own design question and not in scope.

## Consequences

### Positive

- **Multi-subscriber is the default.** Renderer + physics + telemetry can all tick from the same source; debug overlays can observe keystrokes alongside game components. No fan-out hack required.
- **No ownership choreography.** Handing input "control" between components is just a pair of `subscribe` / `unsubscribe` calls — no atomic-swap dance, no risk of dropping events during the handoff.
- **Symmetric with the rest of the architecture.** Observation mail broadcasts to sessions; input mail broadcasts to subscribers. One pattern instead of two.
- **Lifecycle is uniform with the rest of the control plane.** Subscribe / unsubscribe / result shape mirrors load / replace / drop; auto-cleanup on drop matches the rest of the table.
- **Empty-subscribers is a real state, not a bug.** A substrate with no subscribers is silent by design, not silent because nobody assigned a default. The semantics are explicit.
- **No `default_component` field.** One fewer piece of `App` state, one fewer invariant ("the default points at a live mailbox or is None") to maintain.

### Negative

- **Fan-out cost scales with subscriber count.** A tick at 60 Hz × 4 subscribers = 240 dispatches/sec — trivial. Worst case is "many small components all subscribe to mouse_move," which can fire at hundreds of Hz; even so, dispatch is in-process and cheap. Mitigated: the substrate is single-process, and per-mail dispatch is the existing hot path.
- **Two reserved kinds instead of one.** Subscribe and unsubscribe rather than a single set-default. Cheap; the namespace is closed and additive.
- **No built-in "exactly one subscriber" enforcement.** A workflow that wants exclusive input has to enforce it socially (don't subscribe a second component to keys). Acceptable: the only realistic violations are agent-side mistakes the agent can fix; substrate-side enforcement would invent the focus primitive we're explicitly avoiding.

### Neutral

- **Empty subscriber sets drop events at the source** rather than enqueue-and-discard. Slightly different mechanics from "no default" today, same observable behavior.
- **Subscriptions are per-component (mailbox id), not per-instance.** Across an ADR-0010 replace the new instance inherits the old's subscriptions; intended, matches "the mailbox is the addressable unit."
- **Boot still requires explicit subscription.** Same two-step `load` → `subscribe` sequence as the previous ownership design; the count is the same, the meaning is "say you want it" rather than "claim ownership."

## Alternatives considered

- **Single-owner default-input routing (this ADR's first draft).** `App::default_component: Option<MailboxId>` set via `aether.control.set_default_input`. Rejected: forces multi-listener cases to fan out through one component, invents a "focus" primitive at the wrong layer, and asymmetric with the broadcast pattern observation mail already uses.
- **Subscribe via well-known sink name.** Substrate publishes to a sink like `"substrate.input.tick.broadcast"` and components register interest by sink name (similar to ADR-0010's `resolve_mailbox`). Rejected: the substrate's per-mailbox scheduler is the existing dispatch path; sinks are a separate primitive used for outbound (engine → hub) broadcast. Adding a substrate-internal sink mechanism just to express subscription is more machinery than a per-stream subscriber set.
- **Always-broadcast to every loaded component; let guests filter.** Skip the subscribe step entirely — every component receives every input event and ignores what it doesn't want. Rejected: forces every component to handle every input kind even if uninterested; wastes guest fuel (per ADR-0023) on dispatches the component immediately discards; obscures which components actually care about a stream.
- **Subscribe by kind name string, not stream enum.** More extensible (works for any future input kind without an enum bump). Rejected: typo'd kind names silently subscribe to nothing; the input set is closed and small enough that the enum is fine. Generalizing to kind-name subscription is a future ADR if substrate-published non-input kinds emerge.
- **CLI flag / env var for boot-time subscription.** Solves the demo case without an extra mail round-trip. Rejected as the *only* mechanism — doesn't cover dynamic subscribe/unsubscribe at all. Reasonable additive sugar later.
- **`subscribe_to: Vec<InputStream>` field on `LoadComponentPayload`.** Combine load + subscribe in one call. Rejected for now as the *only* mechanism (doesn't handle subscription changes after load), but the obvious additive shortcut once two-step gets annoying.
- **Status quo: agents fire ticks via `send_mail` per frame.** Rejected: works for poking the engine in tests, doesn't scale to interactive workloads, blocks the demo.

## Follow-up work

- **`aether-substrate-mail`**: add `InputStream`, `SubscribeInput`, `UnsubscribeInput`, `SubscribeInputResult` kinds (gated by `descriptors`).
- **`aether-substrate`**: add `input_subscribers: HashMap<InputStream, BTreeSet<MailboxId>>` to `App`; thread subscribe/unsubscribe handlers through `control.rs`; remove subscriber entries when their mailbox is dropped; rewrite the platform-thread dispatch to fan out per stream.
- **CLAUDE.md**: document the subscribe/unsubscribe pattern in the MCP harness section (the new "load → subscribe to drive ticks" sequence).
- **Tests**: subscribe + tick reaches the subscriber; second subscriber gets the same tick; unsubscribe stops delivery; drop_component removes subscriptions; replace preserves them.
- **Parked, not committed:**
  - `subscribe_to: Vec<InputStream>` boot-time convenience on `LoadComponent`.
  - CLI / env-var boot subscription.
  - Per-window stream identifiers (multi-window).
  - Generalizing subscription to arbitrary substrate-published kinds (not just input).
