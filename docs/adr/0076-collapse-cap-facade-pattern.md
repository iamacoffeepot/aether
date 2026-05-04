# ADR-0076: Collapse the chassis cap facade pattern

- **Status:** Accepted
- **Date:** 2026-05-04

## Context

ADR-0075 split each chassis cap into two pieces: a generic facade (`Cap<B: Backend>`) in `aether-kinds` carrying the `Actor` / `Singleton` / `HandlesKind` markers and a delegating `#[actor]` impl, and a concrete `*Backend` struct in `aether-substrate` carrying the runtime state. The split existed to satisfy two competing constraints:

1. Wasm components needed to address chassis caps by type — `ctx.send::<RenderCapability>(&triangle)` — which meant the cap struct had to be importable from the wasm-accessible `aether-kinds` crate.
2. The substantive cap state (wgpu, cpal, std mpsc, wasmtime) couldn't compile inside `aether-kinds` (which is `no_std` + alloc).

The resulting facade pattern shipped through issue 533 PRs A–D4 (May 2026). Each cap cost ~25–30 lines of scaffolding (Backend trait, `ErasedBackend` with `unreachable!()` impls, `Cap<B>` generic struct, `#[actor]` block delegating to the backend) on top of the substantive impl. The post-PR-D feedback was that the boilerplate was unwanted: native caps and wasm components should be written in the same shape.

The structural reason for the split was the orphan rule, not the wasm-import constraint per se. Wasm components address caps by type today only in *aspirational* code — Phase 3 of issue 533 (the planned migration of `resolve_mailbox(name)` call sites to `ctx.send::<R>`) hadn't shipped. No production wasm component referenced a cap struct from `aether-kinds`. The cap-struct-in-`aether-kinds` requirement was paying for a future capability nothing was using.

Two stopgaps fell out of this:

- The actor markers (`Actor`, `Singleton`, `HandlesKind`) moved from `aether-actor` to `aether-data` to break a cycle the facade pattern introduced (`aether-actor` already depended on `aether-kinds`; the facade required `aether-kinds` to depend on `aether-actor` for the markers). Marked stopgap at the time.
- The `Capability` trait (`NativeActor` alias) sat alongside the facade `with_facade` builder method; chassis builders had two cap-attach paths.

## Decision

The facade pattern retires. Every chassis cap is a regular `#[actor]` block in `aether-substrate`, the same shape as a wasm component:

```rust
// aether-substrate/src/capabilities/handle.rs — full cap, no split
pub struct HandleCapability {
    store: Arc<HandleStore>,
    mailer: Arc<Mailer>,
}

#[actor]
impl HandleCapability {
    #[handler]
    fn on_publish(&mut self, sender: ReplyTo, mail: HandlePublish) {
        // actual impl, no delegation
    }
    // ...
}
```

`aether-kinds` shrinks back to "kind types and the identity registry" — what the crate was named for. The Backend traits, `Erased*Backend` markers, and generic `*Capability<B>` structs are gone. The chassis builder has one cap-attach method (`with`); the per-cap `Capability` trait + `boot()` retire because every cap dispatches identically through `spawn_actor_dispatcher`.

### Render is not special-cased

Render's complications — `FRAME_BARRIER = true` and driver-supplied wgpu state — fit the unified pattern without exception:

- `spawn_actor_dispatcher` checks `Actor::FRAME_BARRIER` and claims through the frame-bound path, so render's pending counter still registers in the chassis frame loop's drain list.
- Driver-facing state (accumulators, GPU bundle) lives on a `RenderHandles` bundle the cap exposes via `cap.handles()` *before* the cap moves into the chassis builder. The driver retains a cheap clone (every field is `Arc`-shared) and calls encoder-level methods on it. Pre-collapse the driver pulled `Arc<RenderCapability>` from `DriverCtx::expect`; post-collapse the cap is owned by its dispatcher thread (facade caps don't go through the typed runnings map) and the bundle is the only access.

The `RenderHandles` split is the pattern any future cap needing driver-side access follows. No `with_frame_bound_facade` / `expect::<C>` plumbing.

### Audio's worker thread stays

`cpal::Stream` is `!Send` on macOS — it must live on the thread that constructed it. The chassis dispatcher requires the cap struct to be `Send`. AudioCapability spawns its own audio worker thread at construction; the worker holds the cpal stream and parks on a shutdown channel. Backend stays `Send` (just holds queue Arc + worker JoinHandle); chassis dispatcher is unchanged. Documented as the deliberate exception to "no per-cap threads" — every other cap is single-threaded by design.

### Markers move back to aether-actor

The aether-data → aether-kinds → aether-actor cycle the facade pattern would have introduced was the only reason `Actor` / `Singleton` / `HandlesKind` / `Dispatch` lived in `aether-data`. With caps no longer in `aether-kinds`, the cycle evaporates. The markers return to `aether-actor` alongside the rest of the SDK.

The native-side `ReplyTo` / `ReplyTarget` stay in `aether-data` to avoid a name clash with `aether-actor`'s wasm-side `ReplyTo` (a `u32` FFI handle, distinct shape). `Dispatch`'s signature references `aether_data::ReplyTo` directly; consumers reach for the right type by which transport they're on.

### Macro extension: `mail: &[K]` slice handlers

`aether.draw_triangle` is sent in batches via `Mailbox::send_many` (ADR-0019). The macro's `#[handler]` decoded one K per envelope, which would have silently dropped all but the first triangle in each batch. Render's handler now takes `mails: &[DrawTriangle]` and the macro emits a `bytemuck::cast_slice` decode for cast-shape kinds. Postcard kinds reject `&[K]` (no batched postcard wire).

## Consequences

**Positive:**

- One shape for every cap. ~25–30 lines of scaffolding per cap retired.
- aether-kinds shrinks back to its named scope (kind types).
- aether-data retires the actor module + Dispatch trait.
- aether-actor reclaims the markers (the PR C stopgap unwinds).
- Single chassis builder method (`with` / `add_capability`); one dispatch path; one set of tests for the boot-failure / duplicate-claim invariants.
- `TypedRunnings`, `passive.capability::<R>()`, `DriverCtx::expect::<R>()`, `ArcShutdown` all retire — drivers reach for cap state via pre-build accessors instead.

**Negative:**

- ADR-0075's Phase 3 (the type-checked sender API for wasm components — `ctx.send::<RenderCapability>(&t)`) needs rework. Caps no longer live in a wasm-accessible crate, so wasm components can't reference cap types directly. Phase 3 was never implemented; no caller breaks today, but the design will be revisited (likely a thin marker type per cap in a wasm-accessible location, separate from the substrate-side state struct).
- `HubClientCapability` (the `Capability`-impl wrapper around `HubClient::connect`) was dead code (every chassis used the bare `connect_hub_client()` function) and got deleted along the way.

**Neutral:**

- The Backend-trait split was supposed to support cleanly mocking caps for tests via swap-in alternate backends. In practice no test ever swapped — every cap test booted the real cap and asserted against the real path. The facade-removal collapses both production and test code to the same struct.

**Implementation phases (issue 545, PRs E1–E5):**

1. **Collapse the 5 facade caps (Log/Handle/Io/Net/Audio).** Move structs + `#[actor]` impls from aether-kinds to aether-substrate. Inline backend impls. Delete `*Backend` traits.
2. **Convert render to `#[actor]` shape.** Drop manual `impl Capability + boot()`. Build the `RenderHandles` split. Macro picks up `mail: &[K]` slice handlers for batched cast-shape kinds.
3. **Collapse `with_facade` into `with`. Drop `Capability` + `boot()`.** All caps go through one `with()`. Chassis builder branches internally on `FRAME_BARRIER`. `add_facade` collapses into `add_capability`.
4. **Move markers back to aether-actor.** `Actor` / `Singleton` / `HandlesKind` / `Dispatch` move out of aether-data.
5. **This ADR.** Documents the shipped state.

## Alternatives considered

- **Keep the facade pattern, accept the boilerplate.** Rejected — user feedback was explicit that the facade was unwanted. The forward-looking value (Phase 3 sender resolution) wasn't real today; postponing the cleanup until Phase 3 forced the issue would have been backwards (let an unimplemented future capability dictate a present-day shape).
- **Make the facade scaffolding macro-generated.** Cuts the LOC cost without changing the structural split. Rejected because the asymmetry between native caps (split) and wasm components (one struct) was the part the user objected to — symmetry was the goal, not LOC count.
- **Move only some caps.** Keep render on the facade pattern (driver-supplied state is genuinely structural) and collapse the simpler 5. Rejected — render doing its own thing was the original drift the user wanted closed. The `RenderHandles` split absorbs the driver-state complexity inside a regular `#[actor]` block; no need for two patterns.
- **Wait for Phase 3 to land before deciding.** Rejected for the same reason as the first alternative — Phase 3's design follows from where caps live, not the other way around.
