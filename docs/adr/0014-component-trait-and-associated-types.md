# ADR-0014: Component trait and associated types

- **Status:** Accepted
- **Date:** 2026-04-14
- **Accepted:** 2026-04-15

## Context

ADR-0012 introduced `aether-component` as a guest-side SDK, committed to typed `Sink<K>` and `KindId<K>` handles for the send side, and **explicitly deferred** the receive-side dispatch shape. The stated reason: one real component (`aether-hello-component`) doesn't distinguish between plausible trait shapes, and the forcing case was expected to appear with the second component.

That deferral was pragmatic but it left a hole: without a committed trait, the SDK's `init` / `receive` shim has no stable signature to call into, which means components today still write their own `#[unsafe(no_mangle)] pub unsafe extern "C" fn init` and pretend the SDK only exists for send helpers. The ergonomic win ADR-0012 promised can't be collected without a receive-side contract.

Two external pressures also pulled the decision forward:

- **ADR-0013 changes the receive shim ABI** (adds a `sender` parameter). If the guest writes the shim by hand, every component breaks when reply-to-sender lands. If the SDK owns the shim, the change lands once.
- **ADR-0016 (persistent state, proposed)** needs somewhere to put "state the component owns across a replace." The natural home is an associated `State` type on a component trait. Without a trait, state ownership has to be reinvented per component.

Forces at play:

- **One user of the trait is not two.** The trait this ADR commits to is the one that works for `aether-hello-component` today and leaves explicit extension points for the known near-future uses (reply, hooks, state). Generalizing beyond that is speculative.
- **State ownership has to live somewhere.** The component needs a place to keep cached kind ids, cached sink handles, and whatever domain state it cares about. Putting it on `self` via a trait implementor is the Rust-idiomatic answer.
- **The SDK must own `#[no_mangle]`.** If components write their own exports, ABI changes (ADR-0013's `sender` param, future fn-signature evolution) cascade. If the SDK writes them, it's one place to evolve.
- **Macros are glue, not the core.** A trait + a small `export!` macro keeps the types visible and the generated code minimal. A macro-DSL component definition hides types and costs IDE experience.
- **Dispatch stays simple for now.** A single `receive` method that sees `(kind_id, bytes, sender)` is honest about what WASM actually gives us. Per-kind routing can be a layer on top without a trait change.

## Decision

Commit to a `Component` trait in `aether-component` with one associated type and three methods. Commit to `InitCtx` and `Ctx` as the context objects passed to those methods. Commit to an `export!` macro as the only supported way to bind a `Component` impl into the `#[no_mangle]` init/receive exports.

### 1. The trait

```rust
pub trait Component: Sized + 'static {
    fn init(ctx: &mut InitCtx<'_>) -> Self;

    fn receive(
        &mut self,
        ctx: &mut Ctx<'_>,
        mail: Mail<'_>,
    );
}
```

- `Self` **is** the component's state. Cached kind ids, cached sink handles, and any domain fields live on `self`. No separate `State` associated type; pulling state out of `Self` adds a layer without use.
- `init` returns `Self` and is called exactly once by the SDK-owned shim, before any `receive`. Resolution of kinds and sinks happens here through `InitCtx`.
- `receive` gets `&mut self` plus the inbound mail. No return value: sends happen through `ctx` during the call.

### 2. `Mail<'_>` — the inbound

```rust
pub struct Mail<'a> {
    pub kind: UntypedKindId,
    pub bytes: &'a [u8],
    pub sender: Option<Sender>,   // None when originating address was Broadcast
}

impl<'a> Mail<'a> {
    pub fn decode<K: Kind>(&self, kind_id: KindId<K>) -> Option<&'a K::Payload>;
}
```

- `bytes` is a view into a substrate-owned buffer valid for the duration of the `receive` call. Holding a reference past return is not supported; the bound makes this a compile error.
- `decode::<K>` performs a kind-id match and a `bytemuck`-style cast. Mismatch returns `None` instead of panicking — components routinely branch on kind.
- `sender` is the ADR-0013 reply handle. `None` for broadcast-origin mail.

Per-kind dispatch remains the component's responsibility: a `match mail.kind` against the cached `KindId<K>` values, with `mail.decode(self.kind_tick)?` inside each arm. A macro sugar on top is possible later; the trait doesn't depend on it.

### 3. `InitCtx<'_>` and `Ctx<'_>`

Both are opaque handles holding whatever the SDK needs internally (today: nothing state-ful; tomorrow: logging, timing, sender table hooks).

`InitCtx` surface — init-only capabilities:

```rust
impl InitCtx<'_> {
    pub fn resolve<K: Kind>(&self) -> KindId<K>;
    pub fn resolve_sink<K: Kind>(&self, name: &str) -> Sink<K>;
}
```

`Ctx` surface — per-receive capabilities:

```rust
impl Ctx<'_> {
    pub fn send<K: Kind>(&self, sink: &Sink<K>, payload: &K::Payload);
    pub fn send_many<K: Kind>(&self, sink: &Sink<K>, payload: &[K::Payload]);
    pub fn reply<K: Kind>(&self, sender: &Sender, payload: &K::Payload);  // ADR-0013
}
```

The `InitCtx` / `Ctx` split is deliberate: resolution is only valid during init (it mutates substrate-side caches), sending is only valid during receive (there's nowhere meaningful to send *from* during init — the component isn't wired into the dispatch path yet). Splitting the types makes these invariants type-checked.

### 4. The `export!` macro

```rust
aether_component::export!(MyComponent);
```

Generates:

- `#[no_mangle] extern "C" fn init() -> u32` — calls `MyComponent::init`, stores the returned `Self` in a `OnceCell`.
- `#[no_mangle] extern "C" fn receive(kind: u32, ptr: u32, count: u32, sender: u32) -> u32` — builds `Ctx` and `Mail`, calls `MyComponent::receive` on the stored instance.
- A static backing store for the `MyComponent` instance.

The macro is the **only** supported wiring. Components don't write `#[no_mangle]`. ABI changes (like ADR-0013's `sender` param) land in the macro, not in user code.

### 5. What this ADR doesn't commit to

- **Lifecycle hooks beyond init/receive.** `on_drop`, `on_replace`, state-migration hooks — all are ADR-0015's territory. Additive trait methods with default impls, so this ADR's shape doesn't churn.
- **Typed per-kind dispatch.** No `#[on(Kind)]` macro. The `mail.decode` pattern is what the SDK ships with; a macro layer is purely additive.
- **Async receive.** WASM components are single-threaded per instance today; async is not needed yet and would complicate the shim contract.

## Consequences

### Positive

- **Unblocks ADR-0012's promise.** Components stop writing `#[no_mangle]` and `unsafe extern`; the `unsafe` surface collapses into `aether-component`.
- **ABI changes land once.** Adding `sender` (ADR-0013), adding hooks (ADR-0015), rehydration plumbing (ADR-0016) — all happen in the SDK, not per-component.
- **State has a natural home.** `self` is the component's state. No parallel "state slot" concept to invent later.
- **Type-level separation of init vs receive capabilities.** `InitCtx` and `Ctx` make the "when can I do what" rules compile-time, not convention.

### Negative

- **`Sized + 'static` bounds lock in one implementation shape.** Components that want to be generic over some trait object won't work without boxing. Fine for V0; a limitation to name.
- **`OnceCell` backing store is macro-hidden state.** If the macro is ever replaced, the store migrates with it. The cost is small (one static) but it's indirection added for ergonomics.
- **`match mail.kind` is hand-written per component.** Not elegant; the simplest honest shape. Sugar can come later.

### Neutral

- **No associated `State` type.** `Self` is the state. Trait implementors who want an explicit split can use a wrapper type.
- **No async.** Present-day WASM is fine with sync; revisit when component-model adoption or wasmtime async changes pressure it.
- **Macro is small.** ~50 lines of generated code per component, all mechanical. Readable in `cargo expand` if debugging ever needs it.

## Alternatives considered

- **Trait with `type State` associated type.** `fn init() -> Self::State; fn receive(state: &mut Self::State, ...)`. Rejected: adds a type-level indirection for zero benefit over `Self`-is-state. Worth the simpler shape.
- **Free-function exports with attribute macros.** `#[aether_component::init] fn init() -> State { ... }`. Rejected: attribute-macro flow makes the state threading invisible and the macro carries more logic than `export!(Type)` does.
- **Stateless `fn receive` + externalized state container.** SDK owns an arena; component references it by key. Rejected: reinvents `&mut self`. The `Self`-based shape is idiomatic Rust and works.
- **Enum-of-all-kinds `Mail` instead of `Mail { kind, bytes }`.** `enum Mail { Tick(Tick), DrawTriangle(DrawTriangle), ... }`. Rejected: forces the component to declare its full inbound universe up front, which doesn't match the substrate's "kinds resolved by name at init" model. A component that loads with a new inbound kind shouldn't need a trait-level enum change.
- **Async receive via `Future`.** Rejected as premature: no pressure yet, and the WASM runtime story for async is still component-model-dependent.

## Follow-up work

- `Component` trait, `InitCtx`, `Ctx`, `Mail`, `KindId<K>`, `Sink<K>`, `Sender` in `aether-component`.
- `export!` macro (decl macro or `proc-macro` — decl is likely enough) emitting `init` / `receive` shims.
- Port `aether-hello-component` to the trait as the measurement of "did the ergonomics move." Diff size and `unsafe` count are the headline numbers.
- Update substrate's documented ABI contract so the `receive(kind, ptr, count, sender)` signature is the canonical shape (ADR-0013 lands the fourth param).
- **Parked, not committed:** per-kind dispatch sugar (`#[on(Kind)]`), async receive, multi-instance-per-crate exports (one `export!` per component crate is the rule), component-local logging/metrics surface on `Ctx`.
