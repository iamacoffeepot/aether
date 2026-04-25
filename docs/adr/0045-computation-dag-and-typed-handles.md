# ADR-0045: Computation DAG and typed handles

- **Status:** Proposed
- **Date:** 2026-04-24

## Context

The substrate today exposes five sinks (render, camera, audio, io, net), each running one operation per mail and replying with a result. Components compose operations by chaining handlers: send op A, handle reply A, send op B, handle reply B. The chain's intermediate state lives on `&mut self` and the control flow lives across multiple `#[handler]` methods.

This works for two-step chains. It collapses past three:

- **Asset pipeline.** Generate a prompt → embed it → run an image model → produce variants → save outputs. Five linked operations; the `&mut self` state needed to reconstruct context at each handler grows with chain length, every reply lands as a sink-level allocation, and Claude has no way to declare "these five operations are one logical job" except by reading the source.
- **Render compositions.** A component that fetches a texture, decodes it, uploads it to GPU, and uses it in a draw call has the bytes flow through wasm linear memory three times (in, decoded, ready-to-upload) when the component never actually needs them — only the GPU does.
- **Cross-component reuse.** Two components that both fetch the same URL pay for two fetches. Two components that both embed the same prompt pay for two embeddings. There is no way to express "if anyone has already produced this value, give me theirs."

We considered shipping `async/await` with a single-threaded executor to clean up the per-handler state machinery (issue threads on `#[flow]` macros, `Future`/`Pin`/`Poll` integration, drain-on-swap behaviour under suspended futures) and concluded the existing `ctx.send` + `#[handler]` shape **is** async — adding `async fn` would reinvent what we have, just with prettier syntax. ADR-0042's `wait_reply` covers the legitimate blocking cases. The composition gap is real but it isn't a syntax problem; it's a missing primitive.

The missing primitive is *handles*: a typed reference to a value that hasn't been produced yet (or that's already cached substrate-side from someone else's request). Handles let mail flow through pipelines without bytes flowing through components, let the substrate cache and dedupe pure work, and give the substrate the topology it needs to fan-out, fan-in, and replay.

This ADR commits to handles as a foundational primitive and the computation DAG that sits on top of them. Wire format, handle store, dispatch semantics, transform model, and a three-phase shipping plan. It supersedes the parked CachedBytes and render-DAG threads — both fall out as special cases.

## Decision

### 1. `Handle<K>` and the `Ref<K>` wire type

A handle is a typed reference to a (possibly-future) kind value:

```rust
pub struct Handle<K: Kind> {
    id: u64,
    _phantom: PhantomData<K>,
}
```

The wire form is `Ref<K>`:

```rust
pub enum Ref<K: Kind> {
    Inline(K),
    Handle { id: u64, kind_id: u64 },
}
```

Anywhere a kind value flows on the wire, a field declared `Ref<K>` lets the sender choose between inlining the value or referring to a substrate-cached one. The wire `kind_id` lets the substrate validate type compatibility before adapter dispatch — a `Ref::Handle { kind_id: ReadResult::ID }` cannot be substituted into a `Ref<FetchResult>` slot.

Handles are typed by `Kind`, not by raw bytes. The kind registry (ADR-0028, ADR-0030, ADR-0032) **is** the type system; no parallel typing layer. Reply kinds (`ReadResult`, `FetchResult`, future `EmbedResult`, …) are the natural handle types because they already carry the structure consumers want, including the echoed correlation fields ADR-0041 added.

Schema-derive integration is a Phase 1 follow-up (the existing `#[derive(Schema)]` and canonical-bytes pipeline doesn't generic-over-K today). Likely shape: a `#[schema(ref)]` field attribute that wraps the field type as `Ref<K>` at wire time. The ADR commits to the wire format; codegen path is implementation detail.

### 2. Substrate-side handle store

The substrate holds a refcounted cache of resolved handle values:

```rust
struct HandleStore {
    entries: HashMap<HandleId, HandleEntry>,
    parked: HashMap<HandleId, VecDeque<ParkedMail>>,
    total_bytes: usize,
    max_bytes: usize, // AETHER_HANDLE_STORE_MAX_BYTES
}

struct HandleEntry {
    kind_id: u64,
    bytes: Vec<u8>,
    refcount: u32,
    pinned: bool,
    last_access: Instant,
}
```

The store is **substrate-global, not per-component**. Two components asking for the same transform handle hit the same cache entry; one component holding a handle keeps it alive until refs drop and LRU eviction reclaims it.

Lifecycle:

- **Refcounted.** Active references — handles held by components, `Ref::Handle` slots in pending mail, transform inputs the executor has pinned — bump the count. Refcount-zero entries become eligible for eviction.
- **LRU under pressure.** When `total_bytes > max_bytes`, evict refcount-zero entries by `last_access` until under cap. Default cap is 256MB, configurable via `AETHER_HANDLE_STORE_MAX_BYTES`.
- **Pinned flag.** Asset-pipeline use cases ("don't let this prompt JSON evict mid-pipeline") mark a handle pinned at creation; pinned entries never evict, even at zero refs. Explicit unpin required to reclaim.
- **Per-source caps still apply.** A single handle's bytes still respect the originating sink's cap (16MB net per ADR-0043, 8MB io per ADR-0041). The store cap is a global ceiling on the sum.

### 3. Handle id derivation

Two id schemes, chosen per node type:

- **Source handles** (output of a sink op like `Fetch` or `Read`): ephemeral monotonic. `HandleId = fnv64(MailboxId ++ monotonic_counter)`. A `Fetch(url)` today and the same `Fetch(url)` tomorrow are two distinct observations and get two distinct ids. Caching across observations is opt-in (the user holds the handle and re-uses it), not automatic, because external state changes between calls. Two concurrent fetches of the same URL also get distinct ids — networks aren't pure functions.

- **Transform handles** (output of a pure guest function): content-addressed over inputs. `HandleId = fnv64(component_mailbox ++ transform_index ++ input_handle_ids)`, where `(component_mailbox, transform_index)` is the position-based `transform_id` from §6. The same transform applied to the same inputs produces the same handle id; memoization falls out automatically *within a single transform-hosting component*. Two components that each ship their own `parse_json` get distinct `transform_id`s (different `component_mailbox`), so they don't auto-dedup with each other — cross-component dedup requires both components to reference a shared transform-hosting component. Auto-dedup keyed on transform *implementation* (Merkle DAG over normalised wasm-body hashes) is a forward-compatible upgrade deferred to Phase 4+; see Alternatives and Follow-up work.

This split is intentional: sources observe a changing world, transforms compute over fixed inputs. Folding source state into the addressing scheme would let the substrate fold two concurrent fetches into one, which is wrong. Ephemeralness for sources keeps observations honest. Content-addressing for transforms makes "asset-pipeline-with-shared-stages" the cheap default within one component's transform set.

### 4. Handle-aware mail dispatch (Phase 1 MVP)

A mail addressed to a sink may carry handle refs in any field typed `Ref<K>`. The substrate's dispatch path becomes:

1. Decode the request kind (existing behaviour).
2. Walk every `Ref<K>` field in the payload.
   - `Ref::Inline(value)`: pass through.
   - `Ref::Handle { id, kind_id }`: look up `id` in the store. If resolved, substitute the inline value (cheap — bytes already decoded for the sender's adapter call). If unresolved, **park** the mail on a pending queue keyed by `id`.
3. When all refs are inline, hand off to the sink adapter as today.

Parked mail isn't dropped — when a handle resolves (its source replies, or its transform completes), the substrate flushes the queue for that id in **FIFO submission order**. Each parked mail re-runs step 2 with the new value substituted; if other refs in the same mail are still unresolved, it parks on the next pending queue. A mail with five unresolved refs walks five queues before it dispatches.

This is the Phase 1 MVP. Components still chain via `#[handler]`s, but a handler can grab a handle from one reply and embed it in the next mail's `Ref<K>` field without ever loading the underlying bytes into wasm linear memory.

**Nested handles** — a `Ref::Inline(K)` whose inline value itself contains another `Ref::Handle` — are not supported in v1. The substrate substitutes inline-once and validates no inner refs survive; chains form via DAG topology, not via handle nesting.

### 5. DAG primitive (Phase 2)

Phase 2 layers a DAG vocabulary on top of handle-aware dispatch:

```rust
aether.dag.submit         { descriptor: DagDescriptor }
aether.dag.cancel         { dag_id: u64 }
aether.dag.status         { dag_id: u64 }

aether.dag.submit_result  : Ok  { dag_id, output_handles: Vec<(NodeId, u64)> }
                          | Err { error: DagError }
aether.dag.status_result  : Running  { progress: Vec<NodeStatus> }
                          | Complete { outputs: Vec<(NodeId, u64)> }
                          | Failed   { node_id: NodeId, error: String }

struct DagDescriptor {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

enum Node {
    Source    { id: NodeId, sink: String, kind_id: u64, payload: Vec<u8> },
    Transform { id: NodeId, transform: TransformRef, output_kind_id: u64 },
    Observer  { id: NodeId, recipient: u64, kind_id: u64 },
}

struct Edge { from: NodeId, to: NodeId, slot: u32 }

struct TransformRef { component: u64, index: u32 }
```

Submit validates topology + types **before** any work runs:

- Every edge's source kind matches the destination's expected input kind on the given `slot` (multi-input transforms select by slot index).
- Transform refs name a `(component_mailbox, transform_index)` pair that exists in the component's `aether.dag.transforms` custom section (§6).
- No cycles; the topology must be a DAG.
- Source `kind_id` is dispatchable on the named `sink`.
- Observer `kind_id` is in the recipient's `aether.kinds.inputs` manifest (ADR-0033).

On accept, the substrate becomes the DAG executor: dispatches sources, awaits handle resolution via §4, fires transforms when their inputs land, and dispatches observer mail when terminal handles land. The "DAG runtime" is just the §4 dispatcher with one rule added (fire a transform when its inputs are all resolved) and topology bookkeeping.

Phase 2 ships **sources + observers only**. Transforms remain open-coded inside components — a component can still write Phase-1-style handler chains, and a Phase 2 DAG composes them by wrapping each chain segment in a source/observer pair.

### 6. Transforms (Phase 3)

Transforms are guest-exported pure functions. A component declares them with an attribute macro that emits an `aether.dag.transforms` wasm custom section parallel to `aether.kinds.inputs`:

```rust
#[transform]
fn extract_bytes(input: ReadResult) -> Vec<u8> {
    match input {
        ReadResult::Ok { bytes, .. } => bytes,
        ReadResult::Err { .. } => alloc::vec![],
    }
}

#[transform]
fn parse_prompt_manifest(bytes: Vec<u8>) -> PromptManifest {
    postcard::from_bytes(&bytes).unwrap_or_default()
}
```

The macro:
- Records `(transform_index, input_kind_ids, output_kind_id, function_table_index)` per declared transform into the custom section so the hub can build a transform catalog at `load_component`.
- Generates an FFI shim (`aether::__transform_<index>_p32(in_ptrs, in_lens, out_ptr, out_cap) -> u32`) that decodes inputs from the handle store, calls the user fn, and encodes the output.

**Transform identity is position-based**: `transform_id = (component_mailbox, transform_index)`. Two components shipping a byte-identical `parse_json` get distinct `transform_id`s and don't share cache entries — cross-component dedup requires both to reference a shared transform-hosting component. This is the simplest workable scheme and what §3's transform handle id derivation builds on. Content-addressed transform identity (hash the normalised wasm function body, fold into a Merkle DAG over handle ids) is a forward-compatible upgrade — `transform_id` is opaque on the wire, so the derivation can change without breaking the descriptor format. See Alternatives and Follow-up work.

Substrate calls transforms via `wasmtime::Func::call` on the owning component's instance. **No `&mut self`, no host-fn calls permitted inside a transform** — transforms are pure compute, no side effects. State changes only at observer nodes (via normal `#[handler]` reception). A transform that wants to short-circuit on input errors returns its output kind's `Err` variant directly; the substrate doesn't impose error-aware semantics.

Phases 1 and 2 don't need transforms — Phase 1 is just handle plumbing, Phase 2 is DAG topology with sources and observers only. A Phase 2 DAG that wants to mimic transform behaviour wraps each step in a no-op observer that forwards the handle.

### 7. Existing kinds: leave alone

Existing kinds (`Read`, `Fetch`, `Write`, `DrawTriangle`, audio events, …) keep their `Vec<u8>` and inline shapes. New kinds adopt `Ref<K>` from day one where pipeline composition matters; primitive byte fields where ergonomics win stay byte-shaped. The substrate dispatcher handles both forms — the field walker in §4 is a no-op on a kind with no `Ref` fields.

No big-bang migration; incremental adoption per kind, per use case. When the asset-pipeline kinds land — `aether.image.embed { input: Ref<FetchResult> }`, `aether.image.generate { embedding: Ref<EmbedResult> }`, etc. — they ship as new kinds alongside the old ones, not as breaking changes.

### 8. Error propagation

Poisoned handles aren't a new wire concept — they ride the existing `Result`-shaped reply kinds. A `Handle<ReadResult>` whose source replied with `ReadResult::Err { … }` resolves to that `Err` value; downstream consumers match on the variant in their own logic, identically to today. No separate error channel, no special handle state, no "is it alive" check.

This is why §1 commits to handles being typed over reply kinds (`Handle<ReadResult>`) rather than over raw bytes: the type system already carries the failure mode. A bytes-typed handle would need a parallel error path.

DAG-level failures (validation rejection, cancellation, transform panic) surface through the `aether.dag.status_result::Failed` reply kind on the originating session. A panicking transform is an unrecoverable DAG failure — the DAG aborts, downstream handles never resolve, parked mail is dropped with a `CapabilityDenied`-shaped diagnostic on the affected sinks. Per-DAG state recovery is a Phase 4+ concern.

### 9. Handle lifecycle and refcounts

- **Component-held handles.** A `Handle<K>` value in a guest is just an `(id, PhantomData<K>)`. Construction registers a refcount slot in the substrate via the SDK; `Drop` decrements. `Clone` increments. The SDK derives these automatically — components never call refcount host-fns directly.
- **In-flight mail.** A `Ref::Handle` in unresolved (parked) mail counts toward refcount; once dispatched, the count moves to the consumed-by node (transform input, sink adapter call, …).
- **Across `replace_component` (ADR-0022).** Freeze-drain-swap doesn't touch the handle store. Handles created by the old instance survive the swap and can be re-acquired by the new instance via DAG state restoration. Phase 1 doesn't ship DAG persistence — handles outlive the component instance, but a replaced component re-emits its DAG from scratch. Phase 4+ may add stable DAG identifiers that survive replacement.
- **Across `drop_component`.** All handles owned by the dropping component decrement; entries with zero refs become evictable.

### 10. Phasing

- **Phase 1 (shippable, ~3-4 weeks substrate work).** Handle store, handle-aware mail dispatch, `Ref<K>` wire type, SDK's `Handle<K>` + `Ref<K>` + drop tracking, env-var memory cap. **No DAG submit primitive.** Components chain via `#[handler]`s; each handler can build a `Ref::Handle` from the previous step's reply and embed it in the next mail. Cross-component dedup of source results requires the user to explicitly share a handle — which is fine because Phase 1's value is byte-bypassing, not auto-caching.

- **Phase 2.** `aether.dag.{submit, cancel, status}` mail, `DagDescriptor` validation, substrate-side DAG executor for sources + observers (no transforms yet). MCP `submit_dag` tool that lets Claude declare pipelines without writing a component.

- **Phase 3.** Transforms — `#[transform]` macro, `aether.dag.transforms` custom section, wasmtime `Func::call` integration, content-addressed handle ids for transform outputs. Cross-component transform caching becomes automatic.

- **Phase 4+ (separate ADRs).** Incremental recompute (salsa-style invalidation when a source changes), handle persistence across substrate restart, distributed handle stores. The Phase 1 wire is forward-compatible; each later phase adds executor sophistication, not a new wire format.

### 11. Chassis coverage

- **Desktop chassis.** Full handle store + dispatcher. All sinks become handle-aware in Phase 1.
- **Headless chassis.** Same as desktop — asset-pipeline workloads run on headless and depend on handle bypassing for any non-trivial graph size.
- **Hub chassis.** No handle store. Mail bubbled via ADR-0037 has refs already resolved (the originating substrate substitutes inline before bubbling); the hub-as-coordination-plane doesn't host long-lived handles. Submitting a `aether.dag.*` kind to a hub-chassis substrate replies `Err { error: "unsupported on hub chassis" }`, same shape as the existing chassis-only kind rejections (ADR-0035).

## Consequences

### Positive

- **Bytes don't flow through components for pipeline-shaped work.** A texture fetched by component A and consumed by GPU sink C never touches A's wasm linear memory. The asset pipeline's "fetch prompt → embed → image_gen → save" stops paying for four copies of every intermediate.
- **Memoisation for transforms (Phase 3).** Same transform on same inputs = one compute, within a transform-hosting component's surface. Two pipelines that route through a shared embed-hosting component share the embedding handle for free; two components each shipping their own copy of `parse_json` don't share cache entries (their `transform_id`s differ). Cross-component dedup-by-implementation is a forward-compatible upgrade — see Alternatives.
- **No new state machinery in components.** Phase 1 keeps the existing `#[handler]` model; handles slot in as a wire type, not a control-flow primitive. Async/await stays scrapped.
- **Forward-compatible wire.** Phase 2 / 3 / 4 layer on; the wire format from Phase 1 doesn't change. ADR-0044's "phase enforcement, not wire" model applies here too — author code written for Phase 1 handles becomes a DAG node automatically when Phase 2 lands.
- **Render DAG and CachedBytes both subsumed.** A render DAG is the special case where source nodes are draw operations and the output sink is GPU. A CachedBytes-style asset-pipeline cache is just the handle store with `pinned: true`. One foundational primitive replaces two parked design threads.
- **Claude-in-harness can declare pipelines without writing a component (Phase 2).** `aether.dag.submit` is mailable; the MCP `submit_dag` tool gives the harness a way to compose existing sinks into multi-step jobs as a first-class operation.

### Negative

- **New substrate state.** Handle store is a third registry alongside mailbox + kind. Boot grows a `HashMap<HandleId, HandleEntry>` plus a parking table for unresolved mail. Test scaffolding grows commensurately.
- **Wire complexity.** `Ref<K>` is one more enum variant in the schema-derive's surface. Per-field cost is one byte for the discriminant in `Inline` cases; existing kinds opt out by not declaring `Ref` fields.
- **Phase 1 doesn't auto-dedupe sources.** Two components fetching the same URL still pay twice in Phase 1. Source-level dedup has its own design surface (when is "same fetch" the right collapse? always? within a time window? declared by caller?) — out of scope here, deferred to a Phase 4+ ADR if a forcing function emerges.
- **Transform debugging is harder.** A transform fails on the substrate side; the failure surfaces as a poisoned handle but the wasm stack is gone by the time a downstream observer sees it. Phase 3 needs a transform-error reply path that captures more than just "the output handle is `Err`" — likely a side-channel `engine_logs` entry per transform invocation.
- **Schema-derive needs work.** `Ref<K>` is generic; the existing `#[derive(Schema)]` and canonical-bytes pipeline (ADR-0031, ADR-0032) doesn't generalise over type params. Solved by a `#[schema(ref)]` field attribute or by codegen-time monomorphisation; not a wire change but a Phase 1 implementation gate.
- **Capabilities (ADR-0044) interaction is loose in v1.** Cross-component handle-passing is unrestricted — a component without `net` capability can consume a handle whose source was a fetch, getting at the bytes indirectly. Acceptable until the data-leak threat model surfaces; revisit in the ADR-0044 unparking conversation.

### Neutral

- **Mail surface unchanged for v1 sinks.** Net, io, audio, render, camera dispatch identically; Phase 1 only adds the field-walking step for `Ref<K>` fields, which existing kinds don't use. Existing components keep working with no rebuild.
- **Postcard/cast encoding unchanged.** `Ref<K>` is a normal enum encoded with the existing schema-derive machinery (ADR-0007, ADR-0019).
- **Capabilities orthogonal in v1.** Caps gate sink access; handles flow values through sinks. The two systems compose via "the caller of `aether.dag.submit` needs whatever caps the source nodes consume."
- **Scheduling unchanged.** ADR-0038's actor-per-component dispatch keeps its semantics — handle-aware dispatch is a sink-handler-internal step, not a scheduling change. A parked mail does not block its sender's actor thread.

## Alternatives considered

- **Async/await with a single-threaded executor.** Reinventing what `ctx.send` + `#[handler]` already gives us. The composition gap is missing primitive (handles), not missing syntax (await). Cost (executor, `Future`/`Pin`/`Poll` integration, `&mut self` lifetime acrobatics, drain-on-swap behaviour under suspended futures) wasn't worth the syntax win.
- **`#[flow]` macro for sequential pipelines.** Sugar for chained handlers. Subsumed by the DAG primitive — a linear flow is the trivial DAG case. Macro becomes unnecessary once handles ship.
- **Substrate-side closures for transforms.** Closures don't cross the FFI cleanly. Transforms are pure guest function exports, called by index. Same effect, no closure machinery, no memory-aliasing hazards.
- **Handles parameterised by raw bytes (`Handle<Vec<u8>>`).** Loses the echoed correlation fields ADR-0041 added (`namespace`, `path`, `url`) and reintroduces the parallel-typing problem the kind registry was meant to retire. Reply kinds carry richer structure; transforms project bytes out via `extract_bytes` when needed.
- **Per-component handle stores instead of substrate-global.** Cuts cross-component caching, the headline asset-pipeline win. Rejected; data-leak concerns are addressed by capabilities (ADR-0044) when needed, not by store partitioning.
- **Content-address all handles, including sources.** Two fetches of the same URL collapse to one observation, which is wrong — networks aren't pure functions. Sources observe; transforms compute. Source ids must be ephemeral.
- **Content-addressed transform identity (`transform_id = hash(normalised_wasm_function_body)`).** Auto-dedup of byte-identical transforms across components, plus a full Merkle DAG over handle ids — every result hash recurses through input handles + transform-body hashes all the way to source observations. Buys distribution (sparse sync between substrates Git-style), recompute-to-verify provenance, and "rebuild correctly invalidates without bumping a version string." Rejected for v1: the wins all need forcing functions we don't have (multi-substrate sync, untrusted-author dedup, replay/audit pipelines), and they pay an engineering tax for wasm normalisation — strip cargo metadata + debug info + name section, hash function bodies post-validate so two semantically-identical compilations land on the same hash and a rustc-instruction-selection change correctly bumps it. Forward-compatible upgrade because `transform_id` is opaque on the wire.
- **Ship Phase 2 (DAG submit) and Phase 1 (handles) as one PR.** Tempting because the executor is "just" handle dispatch + topology bookkeeping, but the wire surface (descriptor format, validation rules, status reply shape) is enough design surface to deserve its own ADR review cycle. Phase 1 alone is shippable and useful; Phase 2 builds on a stable foundation.
- **Ship transforms (Phase 3) before DAG submit (Phase 2).** Transforms without a DAG submit primitive have no caller — components could in principle invoke transforms via a new mail kind, but that recovers the chained-handler problem inside transforms. Phase 2 first means transforms have a real consumer when they ship.

## Follow-up work

- **PR**: foundational substrate work — `HandleStore`, `HandleEntry`, refcount/LRU/pinned semantics, parked-mail dispatch in the sink dispatcher, `AETHER_HANDLE_STORE_MAX_BYTES` env var, integration tests covering source replies + parked mail + LRU eviction.
- **PR**: kinds + schema-derive — `Ref<K>` wire type, `#[schema(ref)]` field attribute (or equivalent codegen path), one demonstrative new kind that uses it.
- **PR**: SDK — `Handle<K>` newtype with `Drop`/`Clone`, `Ref<K>` enum, refcount host-fn shims, `ctx`-side helpers for emit-reply-as-handle.
- **PR**: hub MCP surface — `describe_handles(engine_id)` exposing the substrate's handle store for debugging.
- **Parked, not committed**: ADR-0046 — DAG submit/cancel/status mail, descriptor validation, executor for sources + observers (Phase 2). Probably its own ADR because the descriptor wire is enough surface to deserve focused review.
- **Parked, not committed**: ADR-0047 — `#[transform]` macro, `aether.dag.transforms` custom section, wasmtime `Func::call` integration, content-addressed transform ids (Phase 3).
- **Parked, future ADR**: incremental recompute / salsa-style invalidation when a source changes upstream of cached transforms.
- **Parked, future ADR**: handle persistence across substrate restart and DAG resume after `replace_component`.
- **Parked, future ADR**: distributed handle stores for multi-substrate clusters (motivating use case TBD; not on the near-term roadmap).
- **Parked, future ADR**: source-level dedup (collapse two concurrent fetches of the same URL into one observation, opt-in per call site).
- **Parked, future ADR**: content-addressed transform identity. Replace the position-based `transform_id` with a hash of the transform's normalised wasm bytecode body so byte-identical transforms across components share one cache entry, and `HandleId = fnv64(transform_body_hash ++ input_handle_ids)` recurses into a Merkle DAG over the whole computation. Forcing function is distribution (multiple substrates syncing results sparsely) or verifiable replay (recomputing a chain to check a result). Until then the position-based scheme covers in-tree dedup at zero engineering cost.
