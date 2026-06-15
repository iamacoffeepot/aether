# ADR-0114: Inline Child Actors

- **Status:** Proposed
- **Date:** 2026-06-15

## Context

The widget direction (above the immediate-mode floor of ADR-0107) needs many stateful widgets on screen at once. Two spikes settled the cost model:

- **#1793** — an actor-backed widget costs ~1.3µs/frame, count-independent. The WASM boundary is not the bottleneck.
- **#1852** — the per-widget handler cost stays linear, but routing every widget's output through one mailbox goes super-linear past ~1024 senders, compounded by memory pressure from thousands of resident instances.

An actor-per-widget is the clean model but falls over at scale on render fan-in plus instance count. A single dyn-dispatch widget object is cheap but is not composable and forces hand-rolled per-widget messaging and a standardized paint surface.

The existing spawn primitive, `spawn_child` (ADR-0097), creates a **detached** peer: its own WASM instance, dispatcher slot, and run-token. That is correct and load-bearing for the actors that genuinely need an independent run-token — component loading (`spawn_child::<WasmTrampoline>`), the hub's per-engine tracker (`spawn_child::<EngineProxy>`), and the TCP server actors. A screen of thousands of detached widget actors, though, is exactly the #1852 fan-in.

We want widgets that *are* actors — same model, surface, lifecycle, and addressing — with the per-instance cost (separate instance, slot, draw sender) removed, and with the lift hidden rather than expressed as a new programming model.

## Decision

A WASM component can spawn an **inline child**: a co-located child actor that shares the parent's WASM instance, slot, and run-token, while being addressed and mailed like any actor. The name is by analogy to compiler inlining — the child's actor is expanded into the parent's instance the way an inlined function's body is expanded into its caller. The semantics are unchanged; only the cost is.

1. **A new verb, guest-only, mirroring `spawn_child`.** The signature is identical to the detached `spawn_child` (ADR-0097), which keeps its name and meaning:

   ```rust
   pub fn spawn_inline_child<A>(
       &self,
       subname: Subname<'_>,
       config: &A::Config,
   ) -> Result<MailboxId, SpawnError>
   where
       A: Instanced + FfiActor;
   ```

   Like `spawn_child`, it spawns an instance of an `Instanced` type discriminated by a `Subname` (`Counter`, where the host appends a monotonic discriminator, or `Named`, a validated segment); the only difference from `spawn_child` is co-residency (decision 2). A unique inline child is an `Instanced` type spawned once with a fixed `Named` subname; a type-level `Singleton` (ADR-0098) is chassis-registered and is not an inline child. Inline spawning is `FfiCtx`-only: the lift (one instance, one draw sender, local routing) exists only for a WASM guest co-locating sub-actors. Native capabilities are scheduled directly with nothing to co-locate, so the verb does not appear on `NativeCtx` (native symmetry is not precluded, only unmotivated).

2. **First-class addressing via an alias.** The inline child gets an ordinary ADR-0099 lineage-folded `MailboxId` (the `subname` is the lineage segment that folds in — e.g. `aether.component/aether.embedded:inventory/column-0-0`), registered as an **alias** that routes to the parent's slot. The child is reached by sending directly to that address; there is no selector qualifier. At the substrate level the child is the same actor as the parent (one `Box`, one slot, one run-token) serving a subtree of addresses.

3. **The parent's `receive` is a membrane that demuxes on recipient.** Because every alias routes to the parent's slot, the recipient `MailboxId` is what distinguishes them. The generated `receive` dispatches the parent's own handlers when the recipient is the parent, and otherwise looks up the inline child by recipient and dispatches to it. The child registry lives ctx-side in a slot-shaped structure (take-out / dispatch / reinsert) that mirrors the native dispatcher slot, so a running child can spawn or mutate siblings, and mail re-addressed to a child already running queues rather than re-entering it.

4. **The recipient is the dispatch identity (symmetric I/O).** A dispatch's self-identity is its inbound recipient. Outbound origin stamping, settlement-root derivation, and reply routing all read it (today a single `self_mailbox` source), so a child's sends stamp the child's address as origin, replies route back to the child, and the membrane demuxes the return path identically to the inbound path. For a non-inline actor the recipient equals its own id, so nothing changes.

5. **Reload reconstructs from `export!`.** Each inline child persists via its own `type State` (ADR-0113); the parent's dehydrate walks the registry into a kind-tagged bundle, and rehydrate reconstructs each child by kind using the actor-type set the module already declares in `export!` (ADR-0096). There is no new author declaration and no generic instantiate-by-kind. (The `A: Instanced` types reachable by `spawn_inline_child` and the rehydratable types are the same `export!` set.)

6. **No parallel API.** An inline child (a widget or otherwise) is a plain actor written with `#[actor]`, spawned with the one new verb and mailed like any child. The membrane, the alias registry, recipient scoping, and reconstruction are substrate- and SDK-internal and derived from existing declarations. The only new author-facing surface is the verb.

### Settlement and tracing

- **Settlement is unaffected.** It is a per-root causal counter (ADR-0080 / ADR-0106), identity-agnostic, so a shared slot is invisible to it; inheritance keeps the upstream root, so downstream replies settle across the child boundary. Its one requirement — that a child's outbound mail carry the child's identity — is met by the recipient-as-identity rule (4).
- **The trace tree is correct; attribution is component-granular in v1.** The causal trace tree stitches on mail lineage and is correct. Span attribution, the per-handler cost EWMA, and the per-actor log rings are keyed by the physical dispatcher slot, so inline children attribute to the parent component. v1 accepts component-granular observability: control is per-child, observability is per-component. Per-alias observability is a later upgrade (per-alias actor slots, no redesign — the alias ids already exist).
- **Teardown closes the chain.** An inline child dropped mid-chain must route its orphan replies through the standard dispatch tail so the chain's root settles and ADR-0094's obligation is discharged. This is the same close-on-drop need tracked as the settlement-closure work; inline-child teardown is a consumer of that, not a bespoke seam.

## Consequences

- A screen is a handful of widget components (one render sender each), not N senders, so the #1852 fan-in does not arise.
- An inline child costs mail plus state, not a slot, run-token, or instance; #1793's ~1.3µs/widget per-frame cost is the model.
- Inline children are co-resident and serialized under one run-token: a slow child handler blocks its siblings. Acceptable for cooperative UI.
- The substrate footprint is bounded: thread the recipient to the guest (the dispatch envelope, the `receive` FFI, the guest `Mail` accessor, the `export!` shim), make outbound stamping read the dispatch recipient, register alias entries that route to the parent's slot, and add recipient-aware membrane dispatch in the SDK. The scheduler, the run-token model, the one-`Box`/one-slot model, and `MailboxId` semantics are untouched.
- Observability is component-granular until per-alias actor slots are added.
- Reload granularity is the whole component, as for any WASM instance — changing one inline child's code reloads the tree.

## Alternatives considered

- **Actor-per-widget (all detached)** — the clean model; rejected on #1852 (super-linear render fan-in plus instance memory past ~1024).
- **Repurpose `spawn_child` to be inline** — rejected: `spawn_child` is the load-bearing detached primitive (component loading, engine proxy, TCP), all of which need independent run-tokens.
- **A parallel widget/composite API** (typed child handles, a child trait, a declared child-type list, an author-written membrane) — rejected: it forces the author to load the whole composite model to write one widget. An inline child is just an actor.
- **`?selector` routing as the primary address** — rejected as primary; demoted to an optional generic dynamic-routing feature any actor may offer, orthogonal to the static first-class child addresses.
- **A single dyn-dispatch widget object** — cheap, but not composable, and it forces hand-rolled per-widget messaging and a standardized paint surface.

## Open questions

- Whether per-alias observability (trace, cost, logs) is wanted before v1 ships or is a clean follow-on.
- The ordering of the inline-child teardown work relative to the general settlement-closure primitive.
- The `Widget` trait surface and the draw/compositing handshake are deferred to the consumer ADRs, not settled here.
