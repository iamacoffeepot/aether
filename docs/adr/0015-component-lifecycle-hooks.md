# ADR-0015: Component lifecycle hooks

- **Status:** Proposed
- **Date:** 2026-04-14

## Context

ADR-0014 commits to a `Component` trait with two methods: `init` and `receive`. That's enough to run; it's not enough for a component to participate in its own lifecycle.

Three concrete pressures have surfaced:

- **Replace loses state (ADR-0010 §6).** The old instance's linear memory is dropped; the new instance starts fresh. ADR-0016 picks this up from the persistence angle, but *something* has to be the component-visible hook for "save yourself before the swap" and "here's what you were."
- **Drop is silent.** A component that owns external resources — open handles to a mailbox it spawned, cached work in flight, anything observable — can't run cleanup before it's torn down. Today "torn down" means the substrate drops the `Store` and the linear memory goes with it. Fine for pure-compute components; wrong for anything with externally visible state.
- **Tick / frame is a de-facto hook.** The substrate emits `aether.tick` on the main loop, and components handle it in `receive`. This works, but it conflates "I got a message" with "time passed." Components that care about the *distinction* (schedulers, animation, debouncing) have to pattern-match a kind id.

None of these are blockers for "run a component and see it do a thing." All three are blockers for *iteration at speed*: the exact workflow ADR-0010 was designed to unlock.

Forces at play:

- **Hooks must be additive.** Every hook added to the trait is a default-impl method so existing components don't break. ADR-0014 ships `init` + `receive`; this ADR adds methods without touching those.
- **Hooks must be substrate-driven, not component-synthesized.** A component can't decide to run its own `on_replace` — the substrate is the one doing the replacing. The trait surface has to be what the substrate calls at the right moment.
- **Hooks must be cheap when unused.** A component that doesn't override `on_drop` shouldn't pay for it. Default impls are zero-cost; the substrate's call site does an unconditional virtual/trait call, but it's once per lifecycle event, not per mail.
- **State migration is a separate concern (ADR-0016).** This ADR commits to the **hook shapes** — what gets called and when. What goes across the hook (serialized bytes, structured state, nothing) is ADR-0016's decision. Keeping them separate means the lifecycle surface can land without state serialization landing.
- **`aether.tick` staying as mail is fine.** The tick hook is cheap to add but not urgent. This ADR commits to the hooks actually needed for the replace-and-drop story and names the rest.

## Decision

Extend the `Component` trait (ADR-0014) with three additional lifecycle methods, all defaulted, all substrate-invoked at well-defined moments.

### 1. Trait additions

```rust
pub trait Component: Sized + 'static {
    fn init(ctx: &mut InitCtx<'_>) -> Self;
    fn receive(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>);

    // NEW — all with default no-op impls.

    /// Called once, on the old instance, immediately before a
    /// replace_component swap. The default impl does nothing.
    /// Override to emit state that the new instance can consume
    /// (see ADR-0016 for the serialization contract).
    fn on_replace(&mut self, ctx: &mut DropCtx<'_>) {}

    /// Called once, on the instance being dropped, immediately before
    /// the substrate tears down linear memory. For both drop_component
    /// and the old instance of replace_component. The default impl does
    /// nothing. Override for cleanup (sending "goodbye" mail, flushing
    /// work to a sibling component, logging).
    fn on_drop(&mut self, ctx: &mut DropCtx<'_>) {}

    /// Called after init on a freshly-instantiated component that is
    /// replacing an older instance, if the substrate has state from
    /// the old instance for us. The default impl ignores prior state.
    /// Components that persist across replace override this to rehydrate.
    /// Signature is ADR-0016's problem; shape committed here so the hook
    /// exists.
    fn on_rehydrate(&mut self, ctx: &mut Ctx<'_>, prior: PriorState<'_>) {
        let _ = prior;
    }
}
```

`PriorState<'_>` is opaque to this ADR — its internal shape is ADR-0016. The hook exists; what flows through it is decided there.

### 2. `DropCtx<'_>`

A narrowed context for shutdown hooks. Like `Ctx` but:

- `send` is still available (outgoing mail during shutdown is valid and useful — "I'm going away, here's the last thing I observed").
- No `reply` — sender handles invalidate on teardown; a reply attempt during `on_drop` can't be honored cleanly.
- No resolve — resolution only makes sense at init. Drop-time resolution would be a new capability with no use case.

The type separation mirrors the `InitCtx`/`Ctx` split from ADR-0014: capability fences are types.

### 3. Substrate call order

For `drop_component(target)`:
1. Scheduler takes the write lock.
2. `on_drop(&mut self, DropCtx)` runs on the target instance.
3. Outbound mail sent during `on_drop` is queued normally.
4. Instance is dropped; mailbox entry marked `Dropped`.

For `replace_component(target, new_bytes)`:
1. Scheduler takes the write lock.
2. `on_replace(&mut self, DropCtx)` runs on the old instance. ADR-0016 decides what state exits through this call.
3. `on_drop(&mut self, DropCtx)` runs on the old instance. (Both hooks fire: `on_replace` is the migration-specific hook, `on_drop` is the universal shutdown hook.)
4. New instance is instantiated; its `init` runs.
5. If ADR-0016's state bundle is present, `on_rehydrate(&mut self, Ctx, prior)` runs on the new instance.
6. Mailbox is atomically rebound to the new instance.

This ordering is the subject of follow-up bikeshed: `on_replace` + `on_drop` firing back-to-back is redundant if the state migration already captured everything. Keeping both firing is the honest answer — `on_drop` is for universal cleanup; `on_replace` is the migration-specific moment. Overrides don't have to implement both.

### 4. What stays parked

- **`on_tick` as a first-class hook.** Continues to come in via `receive` as `aether.tick` mail. Revisit when the pattern-matching cost is visible.
- **`on_pause` / `on_resume`.** No pause concept exists yet.
- **`on_error`.** Component-visible error channel is a larger surface (what counts as an error? who reports it?). Out of scope.
- **Async hooks.** Same stance as ADR-0014: sync until pressure.

## Consequences

### Positive

- **Makes replace and drop non-destructive to externally-visible state.** Components that own handles, in-flight work, or observability contracts can clean up before teardown.
- **Unblocks ADR-0016.** The hook shape for state migration is committed; the data shape is ADR-0016's problem but has a home to flow through.
- **Additive to ADR-0014.** Existing components don't break. `aether-hello-component` doesn't implement any of these and stays green.
- **Capability fencing stays typed.** `DropCtx` vs `Ctx` enforces shutdown-only semantics at compile time.

### Negative

- **Trait surface grows.** Three defaulted methods added, but three more things a component author has to know exist. Mitigated by all being opt-in no-ops.
- **`on_replace` + `on_drop` call order is judgment-y.** The rule is defensible but not the only option; either could fire first. Picked "on_replace first" because it's the migration-specific semantic and `on_drop` is the universal cleanup — migration happens, then cleanup.
- **Hooks must not panic mid-teardown.** A component that panics in `on_drop` can't be cleanly torn down. The substrate needs to catch and log rather than propagate. Same constraint as `on_replace` and `on_rehydrate` — the substrate treats hook panic as "hook opted out, continue teardown."

### Neutral

- **No runtime cost for unused hooks.** Default impls are empty; the call happens but does nothing.
- **ADR-0016 can land without an ABI change.** The hooks are trait-level; what `PriorState<'_>` contains is internal to `aether-component`.
- **Component author's concept count grows slowly.** ADR-0014 introduced `init`/`receive`. ADR-0015 adds three hooks with obvious names. Nothing subtle.

## Alternatives considered

- **Single `on_lifecycle(event)` method.** Variant-based dispatch. Rejected: conflates three very different semantics into one method that components have to `match` on. Separate methods are more Rust-idiomatic and each can have a distinct `Ctx` type.
- **No `on_replace`; rely on `on_drop` + `on_rehydrate` alone.** The migration state would exit through `on_drop`. Rejected: `on_drop` is the universal shutdown path; making it also the migration serialization path overloads it. `on_replace` is where migration-specific logic belongs.
- **Hooks as separate trait implementations (`DropHook for MyComponent`).** Opt-in via additional `impl` blocks. Rejected: makes `export!` macro-logic harder (it has to detect which traits are implemented to wire the right shim path), and multi-trait lifecycle is a shape that pays off only if hooks multiply dramatically.
- **Hooks as mail kinds (`aether.lifecycle.replace`, etc.).** Rejected: lifecycle events aren't observations or commands; they're control flow. The substrate driving them directly as trait methods is clearer and avoids confusing "component is being dropped" with "component received a drop message."
- **`on_tick` committed in this ADR.** Rejected: the existing `aether.tick` mail path works and no component has expressed pattern-matching pain. Add when the pain is real.

## Follow-up work

- Trait extensions on `Component` in `aether-component`: `on_replace`, `on_drop`, `on_rehydrate` with default no-op impls.
- `DropCtx<'_>` type in `aether-component`. Subset of `Ctx` capabilities.
- Substrate `Scheduler::remove_component` grows a hook-call step before drop.
- Substrate `Scheduler::replace_component` grows hook-call steps in the order §3 describes.
- Hook-panic containment: the substrate wraps each hook call, logs panics, and continues teardown.
- **Parked, not committed:** `on_tick`, `on_pause` / `on_resume`, `on_error`, async hooks, lifecycle-as-mail-kinds alternative.
