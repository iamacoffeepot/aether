# ADR-0075: Actor-typed sender API and chassis cap marker split

- **Status:** Superseded by ADR-0076
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

**3. Chassis caps are facades over substrate-side backends, declared in `aether-kinds`.**

`aether-kinds` already publishes the substrate's vocabulary (kinds). Its scope expands to "the substrate's published vocabulary — kinds and chassis cap actor types." Both wasm components and substrate already depend on it; no new crate.

The orphan rule blocks the natural shape ("marker in `aether-kinds`, `#[handlers]` impl in `aether-substrate`") because `HandlesKind<K> for <Marker>` impls would land in substrate against a foreign type and a foreign trait. The facade pattern works around it: the cap is a generic struct in `aether-kinds` parameterized by a substrate-provided backend trait. The `#[handlers]` impl lives next to the marker in `aether-kinds` and emits HandlesKind impls there (orphan satisfied — local type). Handler bodies in the facade are thin delegation to backend methods. The substrate impls the backend trait against its own concrete state struct, where wgpu / std mpsc / wasmtime types are free to live.

```rust
// aether-kinds — no_std, wasm-importable
pub trait RenderBackend {
    fn on_draw_triangle(&mut self, ctx: &mut ChassisCtx<'_>, t: &DrawTriangle);
    fn on_camera(&mut self, ctx: &mut ChassisCtx<'_>, c: &Camera);
}

/// Default backend for sender-side type resolution. Senders write
/// `RenderCapability` (defaulting to ErasedRenderBackend); the chassis
/// installs `RenderCapability<WgpuRenderBackend>` at boot. Type-erased
/// at the routing boundary — the backend type doesn't appear in mail.
pub struct ErasedRenderBackend;
impl RenderBackend for ErasedRenderBackend {
    fn on_draw_triangle(&mut self, _, _) { unreachable!("erased backend used at runtime") }
    fn on_camera(&mut self, _, _) { unreachable!("erased backend used at runtime") }
}

pub struct RenderCapability<B: RenderBackend = ErasedRenderBackend> {
    backend: B,
}

impl<B: RenderBackend> Actor for RenderCapability<B> {
    const NAMESPACE: &'static str = "aether.render";
}
impl<B: RenderBackend> Singleton for RenderCapability<B> {}

#[handlers]
impl<B: RenderBackend> NativeActor for RenderCapability<B> {
    fn boot(ctx: &mut ChassisCtx<'_>) -> Result<Self, BootError> {
        let backend = ctx.acquire_backend::<B>()?;
        Ok(RenderCapability { backend })
    }

    #[handler]
    fn on_draw_triangle(&mut self, ctx: &mut ChassisCtx<'_>, t: &DrawTriangle) {
        self.backend.on_draw_triangle(ctx, t);
    }

    #[handler]
    fn on_camera(&mut self, ctx: &mut ChassisCtx<'_>, c: &Camera) {
        self.backend.on_camera(ctx, c);
    }
}
// `#[handlers]` emits in aether-kinds (orphan satisfied — RenderCapability is local):
//   impl<B: RenderBackend> HandlesKind<DrawTriangle> for RenderCapability<B> {}
//   impl<B: RenderBackend> HandlesKind<Camera>       for RenderCapability<B> {}
```

```rust
// aether-substrate — std, wgpu
pub struct WgpuRenderBackend {
    device: wgpu::Device,
    queue:  wgpu::Queue,
    // ...
}
impl Drop for WgpuRenderBackend { /* wgpu cleanup */ }

impl aether_kinds::RenderBackend for WgpuRenderBackend {
    fn on_draw_triangle(&mut self, ctx, t: &DrawTriangle) { /* real wgpu work */ }
    fn on_camera(&mut self, ctx, c: &Camera) { /* real wgpu work */ }
}

// At chassis boot: chassis registers `RenderCapability { backend: WgpuRenderBackend::new(...)? }`.
```

Senders see only the facade:

```rust
use aether_kinds::RenderCapability;
ctx.send::<RenderCapability>(&triangle);   // resolves to RenderCapability<ErasedRenderBackend>
                                            // at type level; runtime routes by NAMESPACE
                                            // to whatever instance is registered.
```

The compile-time check `RenderCapability<ErasedRenderBackend>: Singleton + HandlesKind<DrawTriangle>` passes via the blanket impls. Runtime routing reads `RenderCapability::<ErasedRenderBackend>::NAMESPACE` (which doesn't depend on B), looks up the registered instance ("aether.render"), and dispatches. The substrate's actual `RenderCapability<WgpuRenderBackend>` runs the dispatcher; `self.backend.on_draw_triangle(ctx, t)` delegates to the wgpu impl.

Per cap, the author writes:

- One Backend trait declaration in `aether-kinds` (one method per handler kind).
- One ErasedBackend struct + `unreachable!()` impls in `aether-kinds`.
- One `Cap<B>` struct + `#[handlers] impl NativeActor for Cap<B>` block in `aether-kinds` (delegation bodies).
- One concrete state struct + Backend trait impl in `aether-substrate` (the substantive bits — same handler signatures as today, just inside a trait impl).

Roughly 25–30 lines of facade scaffolding per cap, plus the substantive backend impl. Verbose but transparent; no macro magic beyond `#[handlers]`. All seven chassis caps follow the same pattern.

User-component markers (PlayerComponent, etc.) still live in their own trunk-rlib per ADR-0066 — `aether-kinds` is only for substrate-provided actors. Trunk-rlib component impls don't need a facade (see "Wasm component pattern" below).

**4. `NativeActor::boot` becomes a static factory.**

Today (post-#525 Phase 2): `fn boot(self, ctx) -> Result<Self, BootError>`. The cap is constructed with placeholder state by the chassis, then `boot` mutates `self` to install runtime state.

Under the facade pattern, the cap has nothing to construct before boot — the backend is acquired inside `boot` itself. So the `self` parameter isn't useful. NativeActor's boot becomes a static factory:

```rust
pub trait NativeActor: Actor {
    fn boot(ctx: &mut ChassisCtx<'_>) -> Result<Self, BootError>;
}
```

This is exactly Phase 2b parked from issue 525, and it ships incidentally as part of this ADR. No associated `type State` is needed — the cap (the facade) holds the backend, which IS the state.

Drop impls live where they did pre-facade — on the substrate-side state. For `RenderCapability<WgpuRenderBackend>`, dropping the cap drops the `WgpuRenderBackend` field, which runs its `Drop` impl (wgpu cleanup). One line of code per cap moves location compared to today.

Chassis storage type stays `Vec<Box<dyn ActorErased>>` (just renamed from #525's transitional `Vec<Box<dyn ChassisActor>>` if needed). The marker is recoverable from the storage entry via a `TypeId` field on the wrapper.

**5. Hub-broadcast is the only "untyped fan-out" escape hatch.**

`HubBroadcast` is the one chassis cap that doesn't follow the facade pattern. It has no `NativeActor` impl — the substrate handles `"hub.claude.broadcast"` mail with its own internal session fan-out, not a typed dispatcher. Just a marker with a wildcard HandlesKind impl, hand-written (no facade boilerplate to derive):

```rust
// aether-kinds
pub struct HubBroadcast;
impl Actor for HubBroadcast { const NAMESPACE: &'static str = "hub.claude.broadcast"; }
impl Singleton for HubBroadcast {}
impl<K: Kind> HandlesKind<K> for HubBroadcast {}   // wildcard — any kind compiles
```

No general `ctx.send_to_name(name, &kind)` fallback in v1 — if a real use case forces a string-addressed escape hatch later, it gets its own synthetic actor first.

### Wasm component pattern (no facade)

Chassis caps need the facade because their state has wgpu, std mpsc, wasmtime — types that can't compile in `aether-kinds`. Wasm components have no such constraint — their state is no_std-friendly and compiles for native fine.

So for wasm components, **the marker IS the impl type**. No facade, no Backend trait, no ErasedBackend. The `#[handlers] impl WasmActor for X { ... }` block lives in the trunk rlib (where the marker also lives); the cdylib is a one-liner `aether_component::export!(trunk::X);`.

```rust
// aether-camera (trunk rlib) — marker, state, and impl all in one place
pub struct CameraComponent { /* state */ }

#[handlers]
impl WasmActor for CameraComponent {
    fn init(ctx) -> Result<Self, BootError> { ... }
    #[handler] fn on_tick(&mut self, ctx, _: Tick) { ... }
    // emits: impl HandlesKind<Tick> for CameraComponent {}, ...
}

// aether-camera-component (cdylib)
aether_component::export!(aether_camera::CameraComponent);
```

Other components do `use aether_camera::CameraComponent; ctx.send::<CameraComponent>(&kind);` — the trunk's `HandlesKind` impls compile-check it. `WasmActor::init` keeps `-> Result<Self, BootError>` (no associated `type State`).

**Trunk-rlib promotion is opt-in.** Components addressed only from within their own cdylib (examples, test-fixture-probe, standalone demos) keep their impl in the cdylib — `#[handlers]` emits HandlesKind impls there, used only locally. A component grows a trunk rlib when another component needs to address it.

**Bonus side effect:** component logic becomes host-testable. The trunk rlib compiles for native, so pure logic (state mutations, math, parsing) is testable with `#[test]` — no wasm runtime needed. Anything that calls into `WasmTransport` host fns still requires wasm to run, but that's a smaller surface than today.

### `#[handlers]` is the single auto-generation path

`HandlesKind<K>` impls are never hand-written. The `#[handlers]` macro emits them on the impl block, alongside its existing emissions (dispatch table, auto-subscribe prologue, `aether.kinds.inputs` custom section).

Same macro for both shapes:

- **Chassis cap**: `#[handlers] impl<B: Backend> NativeActor for Cap<B> { ... }` in `aether-kinds`. Emits `impl<B: Backend> HandlesKind<K> for Cap<B>`. Orphan satisfied — `Cap<B>` is local.
- **Wasm component**: `#[handlers] impl WasmActor for Comp { ... }` in trunk rlib (or cdylib for standalone). Emits `impl HandlesKind<K> for Comp`. Orphan satisfied — `Comp` is local.

The handler-method list is the single source of truth in both cases. Adding a `#[handler]` fn updates senders' compile-time checks automatically. For chassis caps, the substrate's Backend impl is forced to provide the corresponding method (compile error against the trait if missing) — so the facade declaration and the substrate impl stay in sync by construction.

## Consequences

**Positive:**

- Magic-string mailbox addressing dies on the sender side. Runtime routing path is unchanged.
- One handle per mailbox. The dual-handle pattern (camera + draw to render) collapses to a single typed sender.
- Wrong-mailbox sends become compile errors instead of silent runtime warn-drops.
- Phase 2b (static-factory `boot()`) ships incidentally — separating marker from state is what unblocks it.
- Chassis caps cleanly separate identity (importable from wasm) from runtime state (substrate-only).
- Renames of chassis mailbox names become a one-line edit on `<Cap>::NAMESPACE`, not a workspace-wide grep.

**Negative:**

- Each chassis cap costs ~25–30 lines of facade scaffolding in `aether-kinds` (Backend trait, ErasedBackend, `Cap<B>`, delegation impl) on top of the substantive backend impl in `aether-substrate`. Verbose but transparent — no hidden macro magic beyond `#[handlers]`.
- `aether-kinds` scope expands from "kinds" to "kinds + chassis cap actor types." The crate name still loosely covers it; a future rename to `aether-vocab` is a one-PR mechanical change if it starts feeling stretched.
- `NativeActor::boot` becomes a static factory (drops the `self` parameter) — minor breaking change to anyone implementing it outside the workspace (zero today). Phase 2b parked from issue 525 ships incidentally.
- Multi-instance acquirers still write the instance name once at `resolve_actor` — the string surfaces, but at one site per handle instead of every send. That's the honest tradeoff for picking which instance you mean.
- Every wasm component artifact rebuilds. Same coordination cost as past mailbox-rename cuts (see issue 525 Phase 5 / ADR-0074 phase 5 precedent).

**Neutral / out-of-scope for v1:**

- Multi-instance handle distribution beyond `ctx.resolve_actor::<R>(name)` (parent-passes-handle, registry topologies) — same question every actor system answers; not unique to this proposal. v1 ships the resolver; richer distribution is a future ADR if the resolver alone bites.
- Bubble-up (ADR-0037) is unchanged — substrate still routes by name string. Local lookup → forward to hub → hub forwards to hosting engine. The hub may need to learn which engines host which actor types for typed cross-engine routing; that addition is a follow-up if cross-engine actor sends become common.
- Multi-subscriber input streams (Tick / Key / MouseMove / MouseButton) keep their existing fan-out machinery — heterogeneous subscriber sets don't fit a single receiver type by construction.

**Implementation phases (PR per phase):**

1. **SDK primitives, parallel API.** `aether-actor` adds `Singleton`, `HandlesKind`, `Ctx::send::<R>`, `Ctx::resolve_actor::<R>`. `#[handlers]` emits `HandlesKind<K>` impls. `Mailbox<R>` (R = actor) lives alongside existing `Mailbox<K>` (K = kind). Nothing breaks; new API is opt-in. `NativeActor::boot` becomes a static factory at the same time (Phase 2b).
2. **`aether-kinds` chassis cap facades + substrate backend impls.** For each chassis cap: declare Backend trait + ErasedBackend + `Cap<B>` + `#[handlers]` impl in `aether-kinds`. In `aether-substrate`, fold the cap's runtime state into a `<Name>Backend` struct that impls the trait. Drop impls relocate to the backend struct. Compile-time assert each marker's `NAMESPACE` matches the prior literal. `HubBroadcast` lands hand-written (no facade).
3. **Migrate senders.** Walk every `resolve_mailbox(...)` call across components, examples, tests. Singleton receivers → `ctx.send::<R>(&kind)`. Multi-instance receivers → `ctx.resolve_actor::<R>(name)` + `handle.send`. `hub.claude.broadcast` → `HubBroadcast`. Promote any cdylib-only component that needs cross-addressing into a trunk rlib (move `#[handlers]` impl up; cdylib becomes `export!`). Rebuild every wasm artifact.
4. **Retire the kind-typed API.** Delete `resolve_mailbox`, kind-typed `Mailbox<K>`, the `K` param on the const-resolver path. Update CLAUDE.md (recipient-name convention paragraph).

Each phase is independently shippable. Phase 1 is purely additive; Phase 2 is the chassis refactor; Phase 3 is mass migration; Phase 4 is cleanup.

Implementation is tracked in issue 533.

## Alternatives considered

- **Actor-typed param without hiding strings.** Just change `Mailbox<K>` to `Mailbox<R>` for the type-level check, keep `resolve_mailbox(name)` everywhere. Buys the dual-handle fix but not the magic-string fix; small enough win that it didn't justify the breaking change. Rejected for being half a solution.
- **Drop the K param without typed routing.** `Mailbox` becomes untyped, `mailbox.send::<K>(&kind)` works for any K. Removes the dual-handle nuisance but loses every compile-time guarantee. Rejected for going the wrong direction.
- **Separate `aether-chassis` rlib for markers.** Cleanest separation but introduces a new crate. Rejected per "we don't want more crates" — `aether-kinds` is already the substrate-vocabulary home; markers fit there.
- **Markers in `aether-actor`.** Tiniest possible split. Rejected because aether-actor is pure SDK primitives today (Actor, MailTransport, Ctx) — adding chassis-specific names breaks that purity.
- **Hand-written `HandlesKind` impls in `aether-kinds`.** Three lines per cap, no autogen. Rejected because it forces the kind list to be hand-maintained in two places (the markers and the substrate's actual handler dispatch); drift is silent.
- **Declarative `chassis_actor!` macro emitting marker + HandlesKind + handler trait.** Single-source the kind list in a macro invocation; substrate impls a generated trait. Rejected because the source-of-truth lives outside the actual handler bodies — adding a kind requires updating the macro args, which feels like a parallel declaration. The facade pattern routes `#[handlers]` over actual delegation methods, making the handler list directly visible at the impl site.
- **Singleton-by-loaded-instance-count, runtime check.** No `Singleton` marker; substrate enforces "send::<R> requires R has exactly one live instance." Rejected because it loses the compile-time guarantee — a `send::<R>` call could compile and panic at runtime depending on load-order.
- **General string escape hatch (`ctx.send_to_name`).** Available alongside the typed API for rare cases. Rejected for v1 — keeping the escape hatch tempts users to reach for it instead of declaring synthetic actors. `HubBroadcast` is the only exception today; if more turn up, each gets its own synthetic actor.
- **Receiver-actor-type as the address (no name strings even at runtime).** Substrate routes by `TypeId` instead of by name hash. Rejected because multi-instance, bubble-up to a hub-managed engine, and external observability (engine_logs, MCP describe_component) all key on the name string at runtime — making them work without names is a separate, larger design.
