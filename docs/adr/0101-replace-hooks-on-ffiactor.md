# ADR-0101: Replace hooks on FfiActor

- **Status:** Proposed
- **Date:** 2026-06-08

## Context

`replace_component` (ADR-0022) swaps any component's wasm module behind a stable mailbox. Nothing opts in — the swap works on every component. The `Replaceable` trait and the `export!(X, replaceable)` flag (ADR-0016 / ADR-0040) govern only whether the instance carries state *across* that swap, through a save hook on the old instance (`on_replace` in today's code) and a restore hook on the new one (`on_rehydrate`). The name `Replaceable` implies a gate on replaceability; it gates only state-migration behavior.

Those two hooks are the same lifecycle-hook shape as `wire` / `unwire`, which already sit on `FfiActor` as default-no-op methods an actor overrides if it cares (ADR-0015, "the default hook is a no-op"). The replace hooks are the outlier: split into a subtrait reached through an `export!` flag.

A multi-actor module (ADR-0096) boxes each instance as `Slot<Box<dyn ErasedFfiActor>>`. `ErasedFfiActor` (`aether-actor/src/ffi/mod.rs`) erases `erased_namespace` / `erased_dispatch` / `erased_wire` / `erased_unwire` only — not the replace hooks — so a boxed instance has no route to its concrete replace logic. The multi-actor `export!` arm therefore ships the save / `on_rehydrate_p32` exports as no-ops, and every `replace_component` on a multi-actor module resets the instance to a fresh `init`.

The host already supports the full swap. The trampoline's `handle_replace` (`aether-capabilities/src/trampoline.rs`) threads the resident instance's `type_tag` through `Component::instantiate` → `init_typed_p32`, reconstructing the same exported type the trampoline loaded; it runs `old.unwire()`, then the old instance's save hook, lifts any saved-state bundle, and calls `new.call_on_rehydrate(bundle)`. The save / `on_rehydrate_p32` exports are resolved as `Option<TypedFunc>` and invoked unconditionally when present. The missing surface is guest-side.

ADR-0016 made state migration opt-in deliberately: *"A pure render component with no state doesn't need a migration path; paying for one would be pure overhead. The mechanism has to be opt-in."* The overhead is two no-op wasm exports per component — the same cost `wire` / `unwire` already impose on every component without objection. That rationale does not survive its own precedent.

## Decision

Make `on_dehydrate` / `on_rehydrate` default-no-op methods on `FfiActor`, beside `wire` / `unwire`. Retire the `Replaceable` subtrait and every opt-in spelling. An actor that wants state continuity across `replace_component` overrides the two hooks; one that does not gets a fresh instance, the existing default. `replace_component` itself is unchanged.

**Naming.** The save-side hook is `on_replace` in today's code. This ADR renames it to `on_dehydrate` so it pairs with `on_rehydrate`: the save step *is* dehydration (serialize to a dry bundle), and `on_replace` named the trigger rather than the action. The hook serializes by calling `ctx.save_state` / `ctx.save_state_kind` in its body, the same calls it accepts today. The rename carries through the trait method, the erased method, the wasm export name, and the host's lookup string; everything below uses `on_dehydrate`.

1. **`FfiActor` gains two lifecycle hooks.** `fn on_dehydrate(&mut self, ctx: &mut FfiDropCtx<'_>) {}` and `fn on_rehydrate(&mut self, ctx: &mut FfiCtx<'_>, prior: PriorState<'_>) {}`, default no-op, beside `wire` / `unwire`. `FfiDropCtx` carries `Persistence::save_state` (so `on_dehydrate` serializes there); `FfiCtx` carries the send surface — the ctx types the hooks already used as `Replaceable`.

2. **`ErasedFfiActor` gains the erased pair.** `erased_on_dehydrate(&mut self, &mut FfiDropCtx<'_>)` and `erased_on_rehydrate(&mut self, &mut FfiCtx<'_>, PriorState<'_>)`, joining `erased_wire` / `erased_unwire`. The concrete ctx types keep the trait object-safe.

3. **`#[actor]` forwards them uniformly.** Every `ErasedFfiActor` impl forwards the erased pair to the type's `FfiActor::on_dehydrate` / `on_rehydrate` — the same unconditional forwarding it already emits for `wire` / `unwire`. No flag, no per-type branch.

4. **Both `export!` arms emit one shim shape.** The single-actor and multi-actor `on_dehydrate` / `on_rehydrate_p32` exports route to the instance's hooks — directly for single-actor, through the box for multi-actor. The single-actor `replaceable` / `no_replaceable` split and the multi-actor no-ops collapse into one forwarding shape that derives the rehydrate ctx's self-mailbox id from the instance's namespace.

5. **The host changes by one name only.** `handle_replace`'s logic is unchanged — it reconstructs the same exported type via `init_typed_p32(self.type_tag)` before calling `on_rehydrate_p32`, so the box is already the correct type and there is no re-tagging at rehydrate. The single edit is the save-hook lookup string: `get_typed_func(…, "on_replace")` becomes `"on_dehydrate"`. Export signatures are unchanged.

6. **Retire `Replaceable` and the flags.** Remove the `Replaceable` subtrait and the `export!(X, replaceable)` arm; do not introduce a `replaceable` marker on `#[actor]` or `export!`. The state-bundle protocol (ADR-0016 / ADR-0040) — versioned opaque payload, `save_state` / `PriorState` — is unchanged; only the opt-in gating retires. Migration is free: no `export!(X, replaceable)` call site and no `impl Replaceable` exists in the tree.

### Revises ADR-0016

ADR-0016 gated state migration behind an opt-in to spare stateless components a cost. That cost is two no-op wasm exports, the `wire` / `unwire` cost already accepted everywhere. This ADR revises that one decision: the replace hooks join the other lifecycle hooks as `FfiActor` defaults. ADR-0016's state-bundle protocol stands unchanged.

### Principle this sets

Lifecycle hooks live on `FfiActor` (and `NativeActor`) as default-no-op methods, overridden when an actor cares. `wire`, `unwire`, and now `on_dehydrate` / `on_rehydrate` all take that shape. The only required entry points are `init` (the constructor — it returns `Self`, so it cannot be a no-op) and `receive` (dispatch). `Replaceable` was the single hook gated behind an opt-in subtrait, and it retires. A future lifecycle hook is added the same way — a defaulted method on the actor trait, never an opt-in subtrait or an `export!` flag. An audit at this ADR's writing confirmed those two were the only hooks off the pattern.

## Consequences

### Positive

- **One lifecycle-hook model.** `on_dehydrate` / `on_rehydrate` sit beside `wire` / `unwire`, overridden when wanted and defaulted otherwise — no subtrait, no flag, and no per-type granularity decision to make for multi-actor modules.
- **The names line up.** `replace_component` was never gated by `Replaceable`; removing the trait stops the API implying it was, and the dehydrate / rehydrate pair reads as the save / restore it is.
- **Multi-actor hot-swap falls out as a side effect.** Because the erased forwarding is uniform, a multi-actor module preserves state across `replace_component` with no multi-actor-specific machinery. #1479's `aether-camera` stays state-preservingly hot-swappable once it goes multi-actor — live-iterating the fly controller no longer resets the viewpoint on each swap.
- **Blast radius is the SDK macro layer plus one host string.** `handle_replace`'s logic and the `_p32` export signatures are unchanged; only the save-hook's export name renames (`on_replace` → `on_dehydrate`), a one-string edit in its func lookup. The rest is the `FfiActor` defaults, the erased pair, the `#[actor]` forwarding, and the unified shim body.

### Negative

- **Every component emits two real (forwarding-to-default) replace shims** instead of, for a stateless single-actor component, no-ops. The cost is a handful of wasm bytes per component — the `wire` / `unwire` cost, already paid.
- **A public macro spelling retires** (`export!(X, replaceable)`), the `Replaceable` trait is removed, and the `on_replace` hook name changes to `on_dehydrate`. Zero call sites migrate (no consumers exist).

### Neutral

- **Addressing, wire format, and the state-bundle protocol are unchanged.** The swap reconstructs the same exported type at the same mailbox id (ADR-0022 §4); the `_p32` contract keeps its shapes, the save-hook export keeps its (no-arg) shape under the new name.

### Follow-on

- ADR-0016 gains a revision pointer to this ADR once this is accepted.

## Alternatives considered

- **Keep the `Replaceable` opt-in (status quo).** Rejected: it gates state-migration behavior behind a name that implies it gates replaceability, and it splits two lifecycle hooks off the trait that holds the others — to avoid a cost the `wire` / `unwire` precedent already pays.
- **Per-type opt-in for multi-actor (`#[actor(replaceable)]`), keeping the opt-in concept.** Rejected: it preserves the opt-in distinction and forces a granularity decision (per-type vs whole-module) to save a cost that does not exist. Unifying onto `FfiActor` dissolves the question.
- **Keep the `on_replace` name.** Rejected: it pairs a trigger-named hook (`on_replace`) with an action-named one (`on_rehydrate`), and `unwire` already covers pre-swap teardown. `on_dehydrate` names the save action and makes the dehydrate / rehydrate pair symmetric.
- **Autodetect via `Replaceable` impl presence.** Moot once the trait retires; it would have needed a fragile autoref-specialization shim to let `#[actor]` see whether `impl Replaceable for X` exists.

## Related

- ADR-0096 — multi-actor wasm modules; this gives their instances hot-swap by unifying the hooks rather than by adding an opt-in.
- ADR-0097 — wasm sibling spawn; the prior ADR-0096 follow-on.
- ADR-0024 — dual-target `_p32` shims; the contract whose replace exports become uniform.
- ADR-0040 / ADR-0016 — the state-bundle protocol; ADR-0016's opt-in decision is revised here, its protocol kept.
- ADR-0022 — `replace_component` binding-stable swap; the host path, unchanged but for the save-hook lookup name.
- ADR-0015 — component lifecycle hooks; `on_dehydrate` / `on_rehydrate` join `wire` / `unwire` under its default-no-op rule.
- iamacoffeepot/aether#1480 — the tracking issue.
- iamacoffeepot/aether#1479 — the multi-actor `aether-camera` consumer that benefits.
