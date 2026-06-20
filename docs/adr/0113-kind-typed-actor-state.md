# ADR-0113: Kind-typed actor state for hot-reload

- **Status:** Accepted
- **Date:** 2026-06-14
- **Accepted:** 2026-06-15 (implemented by #1884)

## Context

A wasm component carries state across `replace_component` by overriding two
`WasmActor` hooks: `on_dehydrate` saves a bundle on the dying instance, and
`on_rehydrate` reads it back on the replacement (ADR-0101 made both default
no-op methods, so there is no opt-in flag). The reference `stateful_replace`
fixture shows the shape:

```rust
fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
    ctx.save_state_kind::<CountReport>(0, &CountReport { count: self.count });
}
fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
    if let Some(saved) = prior.as_kind::<CountReport>() {
        self.count = saved.count;
    }
}
```

Two problems follow from this being hand-written per actor. The pair is
boilerplate that names the same state kind twice and threads the `version`
and the `as_kind` decode by hand. And it is a correctness hazard for a
composite actor with many children: a child that overrides `on_dehydrate`
but forgets `on_rehydrate` (or vice versa) leaves the rebuilt instance in a
mixed restored/default state, with no compile-time signal that the pair is
incomplete.

ADR-0040 (kind-typed state persistence) shipped the SDK framing this rests
on — `save_state_kind<K>` / `as_kind<K>`, where the bundle is `K::ID` (little-
endian, 8 bytes) followed by the postcard body, so schema identity rides in
the bytes and a reshaped `K` is rejected automatically. ADR-0040 then
**parked** the next step:

> **Parked, not committed**: typed `State` associated type on the
> `Component` trait (ADR-0040-phase-2 if the typed API gets used heavily).

It parked the associated type because committing to it then "commits to a
trait shape before we have one concrete component using it seriously." That
objection is now answerable. ADR-0090 landed `type Config` — an associated
`Kind` on `WasmActor` that defaults to `()`, is synthesized by the `#[actor]`
macro when omitted, and crosses the FFI as bytes. `type Config` is the
working precedent for "a declared kind the macro threads through a lifecycle
boundary," so the state side can mirror it rather than invent a shape.

## Decision

Add a `type State: aether_data::Kind` associated type to `WasmActor`,
mirroring `type Config`, and generate the dehydrate/rehydrate hooks from it.

**Trait shape.** `type State` defaults to `()` the same way `type Config`
does: stable Rust has no associated-type defaults (rust-lang/rust#29661), so
the `#[actor]` macro synthesizes `type State = ();` when the actor omits it,
gated at macro-expansion time on whether the declaration is present — not on
`State != ()` at runtime. An actor that needs no persistence is unchanged and
costs nothing.

**Accessors, not `type State = Self`.** The actor supplies two methods and the
macro turns them into the hooks:

```rust
fn dehydrate(&self) -> Self::State;            // snapshot of durable state
fn rehydrate(&mut self, prior: Self::State);   // absorb it back
```

State is bidirectional, which is what forces accessors rather than inferring
the producer. `type Config` is input-only — the chassis pushes it into `init`
and the macro synthesizes the whole decode. State is both produced (at
dehydrate) and consumed (at rehydrate), and only the actor knows which fields
are durable versus rebuilt in `init`. Cached `Mailbox` tokens and handle ids
are reconstructed in `init` and never serialized; the durable count, the
accumulated buffer, the cursor are. That split is the same one `type Config`
already draws between configured and computed fields, and the actor is the
only place it can be drawn.

**Generated hooks.** From the accessors the macro emits:

```rust
fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
    let state = self.dehydrate();
    ctx.save_state_kind::<Self::State>(0, &state);
}
fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
    match prior.as_kind::<Self::State>() {
        Some(state) => self.rehydrate(state),
        None => { /* decode-miss: boot fresh, warn if the bundle was non-empty */ }
    }
}
```

**Precedence.** A non-`()` `type State` and a manual `on_dehydrate` /
`on_rehydrate` are mutually exclusive — declaring both is a compile error.
The manual hooks stay as the escape hatch for custom or migration logic; the
macro will not generate hooks on top of them. An accessor without a
`type State`, or a `type State` missing one of the two accessors, is likewise
a compile error, so the incomplete-pair hazard becomes unrepresentable rather
than silent.

**Decode-miss policy: boot fresh, warn if the bundle was non-empty.** When
`as_kind` returns `None` — the saved bytes carry a different `K::ID` because
the state shape changed across versions — the replacement boots from its
`init` defaults and emits a `tracing::warn!` when the prior bundle was
non-empty. Erroring buys nothing here: the old instance is already gone and
cannot be rolled back, so the only choices are fresh state or a wedged
mailbox. A non-empty bundle that fails to decode is worth a log line because
it signals a state reshape silently dropping data; an empty bundle (a fresh
load that never dehydrated) is the normal case and stays quiet. Explicit
V1→V2 migration remains the manual-hook path.

**Size cap unchanged.** `save_state` keeps its 1 MiB cap. A `type State` with
an unbounded `Vec` can blow it; the failure is already loud (the replace path
restores the old instance and returns `Err`). This ADR does not change the
cap or the failure mode — it notes the footgun, since a declared state kind
makes large state easier to accumulate without noticing.

## Consequences

- `dehydrate` / `rehydrate` are the authoring surface for hot-reload state;
  the FFI hooks are generated, so a component never hand-writes the
  `K::ID`-prefixed framing or the `as_kind` decode. The incomplete-pair
  correctness hazard is gone — the type system requires both accessors.
- `type State` is wasm-only, on `WasmActor`. Native chassis caps
  (`NativeActor`) are not hot-swapped through `replace_component`, so they
  have no dehydrate/rehydrate hooks and gain no `type State`. The asymmetry is
  structural rather than a special case: only wasm components are replaced in
  place.
- Manual `on_dehydrate` / `on_rehydrate` keep working unchanged for actors
  that need custom save logic or a cross-version migration. The generated path
  is the default; the manual path is the escape hatch.
- Version-skew stays brittle for v1: this builds on the ADR-0040 machinery
  (whole-schema-hash `K::ID` + postcard via `save_state_kind` / `as_kind`), so
  any field change to `type State` reshapes `K::ID`, misses the decode, and
  boots fresh. That is acceptable for v1 and is the decode-miss policy above.
- Forward path to ADR-0059. ADR-0059 (content-hashed field tags for upgradable
  component storage) is the version-tolerance layer: a TLV format where fields
  self-identify by content hash, so add / remove / reorder of `Option` fields
  survives a schema change. It forks the kind trait into a live-wire side and a
  durable `Storage` side, and persistent state belongs on the `Storage` side.
  ADR-0059 is designed but unbuilt, parked awaiting a consumer. `type State` is
  whole-hash + postcard for now and migrates to a `Storage`/TLV kind when
  ADR-0059 lands, with hot-reload state being ADR-0059's first forcing
  consumer. Pulling ADR-0059 forward is out of scope here — it is a larger
  build (a third wire shape, the trait fork, and the attendant rename) and a
  separate decision.

## Alternatives considered

- **`type State = Self`** — persist the whole actor. Rejected: it cannot tell
  durable fields from the cached `Mailbox` tokens and handle ids that `init`
  rebuilds, so it would serialize and restore values that are invalid in the
  new instance. The accessor split is what carries that distinction.
- **A single combined hook returning `Option<State>`** — fold save and restore
  into one method. Rejected: dehydrate runs on the dying instance through
  `WasmDropCtx` and rehydrate on the replacement through `WasmCtx`; they are
  different instances at different lifecycle points with different ctxs, so a
  single method cannot serve both.
- **Error on decode-miss instead of booting fresh** — Rejected: the old
  instance is already gone when rehydrate runs, so there is nothing to roll
  back to; erroring only wedges the mailbox. Fresh-boot-with-warn keeps the
  actor live and surfaces the dropped state.
- **Pull ADR-0059 (Storage/TLV) forward now** — get version-tolerant state in
  one step. Rejected for this PR: it is a much larger change (third wire shape
  + trait fork + rename) and a different mission. Recorded above as the forward
  path; `type State` is the consumer that will force it.
