# ADR-0016: Persistent component state across hot reload

- **Status:** Accepted
- **Date:** 2026-04-14
- **Accepted:** 2026-04-17

## Context

ADR-0010 §6 explicitly parked state migration: "The new component starts fresh. WASM linear memory from the old instance is not transferred. If a component needs persistent state across swaps, it's the component's responsibility to externalize it." That was the right call for the runtime-loading PR arc — state migration is its own design problem and would have bloated ADR-0010 past its scope.

It's also the wall the harness hits the moment components do anything beyond emit draws. A physics component accumulating rigid-body state, a dialogue component tracking a tree position, an input-mapping component remembering bindings — all of them become unusable under `replace_component` because "iterate the code, swap it in" silently becomes "wipe the world and start over."

The user's framing: *"as it stands reloading a component drops its memory and starts over. Until this is solved we cannot do serious work."* That's the commit-worthy signal — the parked item has become the active one.

Forces at play:

- **WASM linear memory is not directly transferable.** The old instance's memory is tied to its `Store`; it goes away when the `Store` does. Pointer values, internal references, allocator state are all meaningless to the new instance. Migration has to happen through a **serialization step** where the old component's state is reduced to bytes and the new component rebuilds from them.
- **Schema evolution is the real hazard.** The point of hot-reload is to change the component's code. If the `State` shape changed between old and new versions, naive byte-copy migration silently corrupts. Version-tagging the state payload is the minimum honest defense.
- **Not every component wants this.** A pure render component with no state doesn't need a migration path; paying for one would be pure overhead. The mechanism has to be opt-in.
- **Atomicity matters.** If a save step succeeds but the rehydrate step fails, what's the state of the mailbox? "Old gone, new broken" is worse than "nothing happened." The replace has to either complete with state transferred or roll back.
- **This is not a persistence system.** State survives *within a running substrate session* across `replace_component`. It does **not** survive substrate shutdown, crash, or spawn/terminate cycles. Cross-session persistence is a different ADR if it ever happens.
- **ADR-0015 gave us the hooks.** `on_replace` (old instance, pre-drop) and `on_rehydrate` (new instance, post-init) are the anchor points. This ADR fills in what flows through them.

## Decision

Introduce opt-in state migration on `replace_component`. The old instance serializes its migration payload into a substrate-owned byte buffer during `on_replace`; the substrate hands that buffer to the new instance during `on_rehydrate`. The payload is versioned, opaque to the substrate, and bounded in size.

### 1. State bundle shape

A **state bundle** is:

```rust
pub struct StateBundle {
    pub schema_version: u32,   // component-defined; the substrate does not interpret it
    pub bytes: Vec<u8>,        // component-defined layout
}
```

The substrate treats both fields as opaque. The component owns versioning (what version means, when to bump it, how to migrate).

### 2. Save side — `on_replace`

During `replace_component`, after the scheduler takes the write lock and before the old instance is dropped, the substrate calls the old instance's `on_replace` hook (ADR-0015). The hook can call a new `DropCtx` method:

```rust
impl DropCtx<'_> {
    pub fn save_state(&mut self, version: u32, bytes: &[u8]);
}
```

- `save_state` may be called zero or one times per `on_replace`. A second call overwrites.
- If `on_replace` returns without calling `save_state`, no bundle is produced and the new instance will not see `on_rehydrate` — it only sees `init`.
- Bytes are copied into a substrate-owned buffer immediately. The component's linear memory is freed normally when the instance drops.
- Size is capped at the same `MAX_FRAME_SIZE` (1 MiB, ADR-0006). A component that tries to save more fails `on_replace`; the replace is aborted, old instance remains live. This is conservative and revisitable.

On the host-fn side: `save_state` translates to a new `aether::save_state(version: u32, ptr: u32, len: u32) -> u32` host fn. Status codes mirror the existing envelope.

### 3. Load side — `on_rehydrate`

The new instance's shim calls `init` first (ADR-0014). Then, if a bundle exists:

```rust
pub trait Component {
    fn on_rehydrate(&mut self, ctx: &mut Ctx<'_>, prior: PriorState<'_>) {
        let _ = prior;  // default: ignore
    }
}

pub struct PriorState<'a> {
    pub schema_version: u32,
    pub bytes: &'a [u8],
}
```

- `PriorState<'_>` is a view into a substrate-owned buffer valid only for the duration of the `on_rehydrate` call. Holding past return is a lifetime error.
- A component that doesn't override `on_rehydrate` silently discards the bundle. Compatible with ADR-0015's "default hook is no-op."
- Version mismatch (prior.schema_version doesn't match what the component expects) is **the component's problem**. The substrate doesn't interpret. A component override typically branches:

```rust
fn on_rehydrate(&mut self, ctx: &mut Ctx<'_>, prior: PriorState<'_>) {
    match prior.schema_version {
        1 => self.rehydrate_v1(prior.bytes),
        2 => self.rehydrate_v2(prior.bytes),
        _ => {}  // unknown version; continue with init defaults
    }
}
```

Host-fn-wise: the new instance receives the bundle via a new `aether::load_state(buf_ptr: u32, buf_len: u32) -> u32` host fn called from within the SDK's `on_rehydrate` shim. Returns `0` for "no prior state," `len` for success, `u32::MAX` for "buffer too small." The SDK hides this plumbing — component authors only see `PriorState`.

### 4. Atomicity

The replace sequence under this ADR:

1. Scheduler takes write lock.
2. Old `on_replace` runs. If it calls `save_state`, bundle is captured; if the bundle exceeds `MAX_FRAME_SIZE`, abort.
3. Old `on_drop` runs (ADR-0015).
4. New instance instantiated. `init` runs.
5. New `on_rehydrate` runs if a bundle exists.
6. Mailbox atomically rebound.

Failure at steps 4 or 5 (instantiate error, WASM trap in `init`/`on_rehydrate`) aborts the replace: old instance stays live, new instance is dropped, mailbox binding unchanged, bundle discarded. Step 3's `on_drop` ran already — this is a wart. `on_drop` running on an instance that ends up not being replaced is observable but not incorrect (the instance *is* being replaced from its own perspective; the rollback is a substrate concern). Revisit if this becomes surprising in practice.

### 5. Interaction with `drop_component`

`drop_component` does not trigger state saving. `on_replace` only fires for `replace_component`. A component dropped with `drop_component` has no successor to hand state to; running a save would be burning cycles for a buffer with no reader.

If a component wants its state preserved across *arbitrary* drops (with the expectation that a later load rehydrates from it), that's a different shape — a persistence sidecar component, or a `persist_state` primitive that stashes the bundle under the component's name. Named as a future direction, not committed.

### 6. What this ADR does not do

- **No serialization framework.** Bytes in, bytes out. The component picks postcard, bincode, hand-rolled, whatever. The substrate is indifferent.
- **No cross-session persistence.** Substrate shutdown drops the bundle with everything else.
- **No automatic schema migration.** The version tag is the only schema-awareness baked in; the component owns migration logic.
- **No state at `drop_component`-time.** Only at `replace_component`-time.
- **No partial / streaming migration.** The bundle is handed across in one shot.

## Consequences

### Positive

- **Unblocks iterative development of stateful components.** Replace a physics component; the world doesn't reset. This is the workflow ADR-0010 was aiming at.
- **Opt-in — no cost for stateless components.** `aether-hello-component` and every pure-render component stay green with zero new code.
- **Versioning is explicit.** `schema_version` lives on the bundle, not in the bytes. Components are forced to think about it.
- **Composes cleanly with ADR-0015.** The hook names and call sites were already committed; this ADR fills in the payload story without trait shape churn.
- **Serialization is the component's choice.** Postcard, custom, whatever. No framework lock-in.

### Negative

- **Migration code is a component burden.** Every stateful component writes `rehydrate_v1`, `rehydrate_v2`, etc. No magic; schema evolution is hard work and this ADR does nothing to make it easier beyond handing a version tag.
- **Version tag is trust-me.** If a component bumps its schema but forgets to bump the version, old-format bytes get handed to new-format logic. This is the footgun; only discipline prevents it.
- **1 MiB cap is tight for rich state.** A physics component with thousands of bodies, or a UI state with many widgets, can exceed this. Cap is revisitable; chunked/streamed migration is a parked direction.
- **`on_drop`-ran-then-rolled-back is a wart.** The ordering of the call sequence means a replace that fails at instantiation has already fired `on_drop` on the old instance. Not incorrect, but a sharp edge that probably bites someone.
- **Atomicity failure modes are rare but real.** A new instance whose `on_rehydrate` panics gets rolled back; the bundle is discarded; the *old* instance is still there with its `on_drop` already fired. Needs clear logging so the failure is visible.

### Neutral

- **Bytes are not validated.** The substrate copies them verbatim. A component that corrupts its own state on save gets corrupted state on load. Fine — the component is the one who cares.
- **In-flight mail policy unchanged.** ADR-0010 §5's "drop on swap" still holds. A component mid-replace loses queued mail; state migration doesn't change that.
- **`StateBundle` is internal.** Never crosses the hub wire, never reaches Claude. Component authors see `save_state` / `PriorState`; nothing else.

## Alternatives considered

- **Push state via mail to a persistence component.** Old component emits `aether.state.save` mail before replace; a long-lived persistence component holds it; new component pulls it. Rejected for V0: relies on a sidecar being present, introduces N-way ordering problems (save before replace? at what mail-queue position?), and the substrate-owned slot is strictly simpler. The sidecar model stays available for components that want cross-drop or cross-session persistence.
- **Transfer WASM linear memory directly.** Copy the old `Store`'s memory into the new `Store`. Rejected: pointer values and allocator internals make this incorrect across code changes. Works only for byte-identical code, which defeats the purpose.
- **Substrate-provided serialization framework.** Bake in serde/postcard/bincode as the required format. Rejected: picks winners, couples the substrate to a particular ecosystem, forecloses components that want something else. Bytes-in/bytes-out is the escape hatch.
- **Automatic migration via schema descriptors.** Register a schema per version; substrate runs automated transforms. Rejected as enormous scope for questionable payoff — schema migration in general-purpose form is a research problem; hand-rolled migration in component code is the pragmatic path.
- **Tag the bundle with component identity, enforce match.** Substrate refuses to hand a bundle from component A to replacement B if names differ. Rejected: replace_component is already keyed by `MailboxId`, which the substrate owns; the component has no identity beyond that. Cross-name migration is not a meaningful operation at this level.
- **No version tag — just bytes.** Rejected: every nontrivial use of the system will want versioning, so bake it in at the shape level so components get it right by default.

## Follow-up work

- Substrate: per-mailbox state bundle slot, wiring in the `replace_component` sequence per §4. Cleared on `drop_component` and on spawn teardown.
- New host fns: `aether::save_state(version, ptr, len) -> u32` and `aether::load_state(buf_ptr, buf_len) -> u32`.
- SDK additions in `aether-component`: `DropCtx::save_state(version, &[u8])`, `PriorState<'_>`, `on_rehydrate` default impl.
- `StateBundle` internal type in the substrate; never crosses the hub.
- Port a test component (new or adapted `hello`) to demonstrate a counter that survives `replace_component`. Smoke test: send N ticks → replace → send M more → observation mail shows counter = N+M.
- Documentation: schema-version discipline in the component-author guide, including a worked example of `rehydrate_v1` → `rehydrate_v2`.
- **Parked, not committed:** cross-session persistence, persistence-sidecar pattern, chunked/streaming migration for state >1 MiB, substrate-provided serialization framework, automatic schema migration, `drop_component` state preservation, transferring in-flight mail across the swap (ADR-0010's drain-on-swap), state bundle compression.
