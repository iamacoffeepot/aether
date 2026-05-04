# ADR-0075: Actor-typed sender API and chassis cap marker split

- **Status:** Proposed
- **Date:** 2026-05-04

## Context

Two papercuts in the post-ADR-0074 mailbox SDK share a root cause:

1. **Magic strings wire mailboxes together.** Every sender names its receiver as a free-form string — `resolve_mailbox::<DrawTriangle>("aether.render")`, `resolve_mailbox::<Note>("hub.claude.broadcast")`, `resolve_mailbox::<Input>("camera")`. Typos compile clean and fail silently — wrong-mailbox sends warn-drop on the receiver. Renaming a mailbox is a workspace-wide grep across components, examples, tests, and demos.

2. **One mailbox needs N handles.** Sending DrawTriangle and Camera to the same mailbox (`"aether.render"`) needs two `Mailbox<K>` declarations because the K param is sender-side inference sugar that pretends a runtime invariant exists where none does. ADR-0074 §Decision 7 folded the camera mailbox into render; the camera-component still keeps two handles to that one mailbox.

Both come from the same root: senders address mailboxes with a `(string, kind)` pair the compiler can't validate. A mailbox at runtime accepts any kind whose dispatcher entry exists; the K param on the sender doesn't reflect that runtime shape.

The actor-trait split (ADR-0074 phases 1–5, plus issue 525 phases 1A/1B/2/3/4a/4b) makes the receiver actor's interface (NAMESPACE, handler kind list) cheap to extract — both at compile time (`#[handlers]` already pins the handler list into a wasm custom section) and at organizational layer (chassis caps are the single-type singletons they always wanted to be).

## Decision

The sender SDK addresses mailboxes by **receiver actor type** instead of by `(name, kind)` pair. Runtime is unchanged — mailbox names remain the address; the substrate's `name → MailboxId` table is untouched. What changes is the compile-time API:

```rust
// today
const RENDER_DRAW: Mailbox<DrawTriangle> = resolve_mailbox::<DrawTriangle>("aether.render");
const RENDER_CAMERA: Mailbox<Camera> = resolve_mailbox::<Camera>("aether.render");
ctx.send(&RENDER_DRAW, &triangle);
ctx.send(&RENDER_CAMERA, &camera);

// proposed (singleton — chassis caps, uniquely-loaded user components)
ctx.send::<RenderCapability>(&triangle);
ctx.send::<RenderCapability>(&camera);
ctx.send::<RenderCapability>(&note_on);   // compile error — RenderCapability doesn't handle NoteOn

// proposed (multi-instance — handle resolved by name once)
let player_1: Mailbox<PlayerComponent> = ctx.resolve_actor::<PlayerComponent>("player_1");
player_1.send(ctx.transport(), &input);   // type-checked against PlayerComponent's HandlesKind impls
```

### Five concrete pieces

**1. New SDK traits in `aether-actor`.**

```rust
/// Marker: only one instance of this actor per substrate. Required by
/// `Ctx::send::<R>` so the type→mailbox lookup is unambiguous.
pub trait Singleton: Actor {}

/// Auto-emitted by `#[handlers]`, one impl per handler kind. Gates
/// `Ctx::send::<R>(&K)` and `Mailbox<R>::send::<K>` so the compiler
/// rejects sends to a kind the receiver doesn't handle.
pub trait HandlesKind<K: Kind>: Actor {}
```

`#[handlers] impl Component for X { ... }` emits `impl HandlesKind<K> for X {}` for every `#[handler] fn on_x(_, _, k: K)` it sees, alongside the existing dispatch table emission. The handler list is the single source of truth — adding a `#[handler]` automatically updates the senders' compile-time check.

The user never writes `HandlesKind` impls by hand. Blanket impls (e.g. `impl<T: Into<DrawTriangle>> HandlesKind<T> for RenderCapability`) are an opt-in extension if a real conversion case wants them; default is strict so wire bytes stay obvious.

**2. New `Ctx::send::<R>(&K)` and `Ctx::resolve_actor::<R>(name)`.**

```rust
impl<'a, T: MailTransport> Ctx<'a, T> {
    /// Singleton path: address the unique instance of R by R::NAMESPACE.
    pub fn send<R, K>(&mut self, kind: &K)
    where R: Singleton + HandlesKind<K>, K: Kind { ... }

    /// Multi-instance path: resolve a typed handle from a runtime name.
    /// The name surfaces ONCE per handle; subsequent sends are string-free.
    pub fn resolve_actor<R: Actor>(&self, name: &str) -> Mailbox<R> { ... }
}
```

`Mailbox<R>` (the `R` param is now the receiver actor, not the kind) has one method `send::<K>` gated on `R: HandlesKind<K>`.

**3. Chassis cap markers move to `aether-kinds`.**

`aether-kinds` already publishes the substrate's vocabulary (kinds). Its scope expands to "the substrate's published vocabulary — kinds and chassis cap markers." Both wasm components and substrate already depend on it; no new crate.

```rust
// aether-kinds (new module, alongside the existing kind types)
pub struct RenderCapability;
impl Actor      for RenderCapability { const NAMESPACE: &'static str = "aether.render"; }
impl Singleton  for RenderCapability {}
impl HandlesKind<DrawTriangle> for RenderCapability {}
impl HandlesKind<Camera>       for RenderCapability {}

pub struct AudioCapability;       // mailbox: "aether.audio"
pub struct IoCapability;          // mailbox: "aether.io"
pub struct NetCapability;         // mailbox: "aether.net"
pub struct HandleCapability;      // mailbox: "aether.handle"
pub struct LogCapability;         // mailbox: "aether.log"
pub struct ControlCapability;     // mailbox: "aether.control"

/// Synthetic singleton for hub-broadcast fan-out. No real actor on the
/// receiving end — the substrate forwards to every attached session.
/// Wildcard HandlesKind so any kind compiles.
pub struct HubBroadcast;
impl Singleton for HubBroadcast {}
impl<K: Kind> HandlesKind<K> for HubBroadcast {}
```

User-component markers (PlayerComponent, etc.) still live in their own trunk-rlib per ADR-0066 — `aether-kinds` is only for substrate-provided actors.

**4. `NativeActor` gains an associated `type State`; `boot` becomes a static factory.**

The marker carries identity (NAMESPACE, HandlesKind impls) — pure compile-time metadata, no runtime state. The cap state (wgpu device, encoder, channels) lives in a sibling struct the substrate owns. Drop moves to the state struct.

```rust
// aether-actor (trait)
pub trait NativeActor: Actor {
    type State: Send + 'static;
    fn boot(ctx: &mut ChassisCtx<'_>) -> Result<Self::State, BootError>;
}

// aether-substrate
pub(crate) struct RenderState {
    device: wgpu::Device,
    queue:  wgpu::Queue,
    // ...
}

impl NativeActor for RenderCapability {
    type State = RenderState;
    fn boot(ctx: &mut ChassisCtx<'_>) -> Result<RenderState, BootError> { ... }
}

impl Drop for RenderState { /* drop wgpu */ }
```

This is exactly the static-factory shift parked as Phase 2b in issue 525. Separating marker from state makes it the only shape that fits — there is no `self` to take at boot, so `boot(ctx)` is the natural signature.

Chassis storage shifts from `Vec<Box<dyn ActorErased>>` (which holds caps with state today) to `Vec<Box<dyn ChassisState>>` (just the state). The marker type is recoverable from the storage entry via a TypeId field on the wrapper.

**5. Hub-broadcast is the only "untyped fan-out" escape hatch.**

`HubBroadcast` synthetic singleton (above) replaces `resolve_mailbox::<K>("hub.claude.broadcast")` for every kind. No general `ctx.send_to_name(name, &kind)` fallback in v1 — if a real use case forces a string-addressed escape hatch later, it gets its own synthetic actor first.

## Consequences

**Positive:**

- Magic-string mailbox addressing dies on the sender side. Runtime routing path is unchanged.
- One handle per mailbox. The dual-handle pattern (camera + draw to render) collapses to a single typed sender.
- Wrong-mailbox sends become compile errors instead of silent runtime warn-drops.
- Phase 2b (static-factory `boot()`) ships incidentally — separating marker from state is what unblocks it.
- Chassis caps cleanly separate identity (importable from wasm) from runtime state (substrate-only).
- Renames of chassis mailbox names become a one-line edit on `<Cap>::NAMESPACE`, not a workspace-wide grep.

**Negative:**

- Two types per chassis cap (marker in `aether-kinds`, state in `aether-substrate`) where today there's one. Drop moves; one line of code per cap.
- `aether-kinds` scope expands from "kinds" to "kinds + chassis cap markers." The crate name still loosely covers it; a future rename to `aether-vocab` is a one-PR mechanical change if it starts feeling stretched.
- `NativeActor` trait gains `type State` — minor breaking change to anyone implementing it outside the workspace (zero today).
- Multi-instance acquirers still write the instance name once at `resolve_actor` — the string surfaces, but at one site per handle instead of every send. That's the honest tradeoff for picking which instance you mean.
- Every wasm component artifact rebuilds. Same coordination cost as past mailbox-rename cuts (see issue 525 Phase 5 / ADR-0074 phase 5 precedent).

**Neutral / out-of-scope for v1:**

- Multi-instance handle distribution beyond `ctx.resolve_actor::<R>(name)` (parent-passes-handle, registry topologies) — same question every actor system answers; not unique to this proposal. v1 ships the resolver; richer distribution is a future ADR if the resolver alone bites.
- Bubble-up (ADR-0037) is unchanged — substrate still routes by name string. Local lookup → forward to hub → hub forwards to hosting engine. The hub may need to learn which engines host which actor types for typed cross-engine routing; that addition is a follow-up if cross-engine actor sends become common.
- Multi-subscriber input streams (Tick / Key / MouseMove / MouseButton) keep their existing fan-out machinery — heterogeneous subscriber sets don't fit a single receiver type by construction.

**Implementation phases (PR per phase):**

1. **SDK primitives, parallel API.** `aether-actor` adds `Singleton`, `HandlesKind`, `Ctx::send::<R>`, `Ctx::resolve_actor::<R>`. `#[handlers]` emits `HandlesKind<K>` impls. `Mailbox<R>` (R = actor) lives alongside existing `Mailbox<K>` (K = kind). Nothing breaks; new API is opt-in.
2. **`aether-kinds` chassis markers + chassis cap split.** Marker types land in `aether-kinds`. Each cap's runtime state moves into a sibling `XxxState` in `aether-substrate`. `NativeActor` gets `type State`; `boot` becomes the static factory. Compile-time assert each marker's NAMESPACE matches the prior literal.
3. **Migrate senders.** Walk every `resolve_mailbox(...)` call across components, examples, tests. Singleton receivers → `ctx.send::<R>(&kind)`. Multi-instance receivers → `ctx.resolve_actor::<R>(name)` + `handle.send`. `hub.claude.broadcast` → `HubBroadcast`. Rebuild every wasm artifact.
4. **Retire the kind-typed API.** Delete `resolve_mailbox`, kind-typed `Mailbox<K>`, the `K` param on the const-resolver path. Update CLAUDE.md (recipient-name convention paragraph).

Each phase is independently shippable. Phase 1 is purely additive; Phase 2 is the chassis refactor; Phase 3 is mass migration; Phase 4 is cleanup.

Implementation is tracked in issue 533.

## Alternatives considered

- **Actor-typed param without hiding strings.** Just change `Mailbox<K>` to `Mailbox<R>` for the type-level check, keep `resolve_mailbox(name)` everywhere. Buys the dual-handle fix but not the magic-string fix; small enough win that it didn't justify the breaking change. Rejected for being half a solution.
- **Drop the K param without typed routing.** `Mailbox` becomes untyped, `mailbox.send::<K>(&kind)` works for any K. Removes the dual-handle nuisance but loses every compile-time guarantee. Rejected for going the wrong direction.
- **Separate `aether-chassis` rlib for markers.** Cleanest separation but introduces a new crate. Rejected per "we don't want more crates" — `aether-kinds` is already the substrate-vocabulary home; markers fit there.
- **Markers in `aether-actor`.** Tiniest possible split. Rejected because aether-actor is pure SDK primitives today (Actor, MailTransport, Ctx) — adding chassis-specific names breaks that purity.
- **Singleton-by-loaded-instance-count, runtime check.** No `Singleton` marker; substrate enforces "send::<R> requires R has exactly one live instance." Rejected because it loses the compile-time guarantee — a `send::<R>` call could compile and panic at runtime depending on load-order.
- **General string escape hatch (`ctx.send_to_name`).** Available alongside the typed API for rare cases. Rejected for v1 — keeping the escape hatch tempts users to reach for it instead of declaring synthetic actors. `HubBroadcast` is the only exception today; if more turn up, each gets its own synthetic actor.
- **Receiver-actor-type as the address (no name strings even at runtime).** Substrate routes by `TypeId` instead of by name hash. Rejected because multi-instance, bubble-up to a hub-managed engine, and external observability (engine_logs, MCP describe_component) all key on the name string at runtime — making them work without names is a separate, larger design.
