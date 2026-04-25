# ADR-0047: DAG submit/cancel/status (Phase 2 of ADR-0045)

- **Status:** Proposed
- **Date:** 2026-04-25

## Context

ADR-0045 committed to handles as a foundational primitive and laid out a three-phase shipping plan. Phase 1 (handle store, `Ref<K>` wire type, parked-mail dispatch) is shipping as a foundational PR sequence. Phase 1's value is byte-bypassing for chained `#[handler]`s — a component can grab a handle from one reply and embed it in the next mail's `Ref<K>` field without ever copying the underlying bytes. Components still write the chain by hand.

Phase 2 raises the abstraction. A caller — a component, the MCP harness, or a content-generation pipeline like ADR-0046 — declares a multi-node graph of operations to the substrate, hands over execution, and gets back terminal handles when the graph completes. The substrate becomes the DAG executor: validate topology + types up front, dispatch source nodes, await handle resolution via Phase 1's parked-mail mechanism, dispatch observer mail when terminal handles land. This is the primitive that lets pipeline composition stop being a per-call-site control-flow exercise.

Phase 2 ships **sources + observers only**. Transform nodes (the third `Node` variant ADR-0045 lists) are parked to ADR-0048 because they need a separate engine surface: a guest-side `#[transform]` macro, a wasm custom section parallel to `aether.kinds.inputs`, and wasmtime `Func::call` integration. A Phase 2 DAG that wants transform-shaped behaviour wraps the chain segment in a no-op observer that forwards the handle. The wire format described here reserves the `Transform` variant so Phase 3 can light it up without a breaking change.

ADR-0045's §5 sketched the wire shape and the executor's high-level role. This ADR is the focused review surface for the descriptor format details, validation phase ordering, executor scheduling under ADR-0038's actor-per-component model, cancellation semantics, status reply timing, the MCP tool, and chassis coverage. It's the piece a reviewer signs off on before substrate code lands.

## Decision

### 1. Mail surface

Three request kinds, three reply kinds, all on the existing `"aether.control"` mailbox alongside `load_component` / `replace_component` / `subscribe_input`:

```rust
aether.dag.submit { descriptor: DagDescriptor }
aether.dag.cancel { dag_id: u64 }
aether.dag.status { dag_id: u64 }

aether.dag.submit_result : Ok  { dag_id: u64, output_handles: Vec<(NodeId, u64)> }
                        | Err { error: DagError }
aether.dag.cancel_result : Ok  { cancelled: bool }
                         | Err { error: String }
aether.dag.status_result : Pending  { node_count: u32, ready: u32, in_flight: u32, parked: u32 }
                         | Running  { progress: Vec<NodeStatus> }
                         | Complete { outputs: Vec<(NodeId, u64)> }
                         | Failed   { node_id: NodeId, error: String }
```

Submit returns synchronously with `dag_id` as soon as validation completes — *before* any source dispatches. The reply also carries the full `output_handles` list (handle ids assigned to terminal nodes) so a caller can hand them to downstream consumers immediately, even though the values aren't resolved yet. This is consistent with ADR-0045 §4: `Ref::Handle` slots can travel before their values resolve; the substrate parks dispatch until they do.

Cancel returns `cancelled: true` if the DAG was found and torn down, `false` if it had already completed or never existed (idempotent). In-flight sink calls aren't aborted — the substrate stops awaiting their replies and discards them when they arrive. Parked mail tied to the cancelled DAG drops with a `CapabilityDenied`-shaped diagnostic.

Status is poll-shaped, not push-shaped. The session asks; the substrate replies with the current state. `Pending` covers the brief window between submit-ack and first source dispatch; `Running` covers everything until terminal handles land; `Complete` is terminal and persists until the dag_id is reaped (see §7); `Failed` is terminal and also persists. There is no streamed status — push-style observation goes through normal observer nodes the caller wires into the DAG.

### 2. DagDescriptor wire format

```rust
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

type NodeId = u32;
```

The shape locks in for Phase 2 even though `Transform` doesn't dispatch yet. Phase 3 lights it up by adding the `aether.dag.transforms` custom section read at component load and the dispatch path that calls into wasmtime when input handles all resolve.

`NodeId` is descriptor-local — a `u32` index assigned by the submitter, not a globally unique handle id. Edges reference NodeIds; the substrate maps NodeIds to handle ids during execution. Two DAGs submitted in parallel can both have a `NodeId(0)` without collision because the namespaces don't cross.

`Source` carries `payload: Vec<u8>` rather than a structured kind value because the substrate doesn't decode source payloads on the submit path — it forwards them to the named sink as opaque bytes, identical to how `send_mail` works today. Validation only checks that `kind_id` is in the sink's accept set (see §3); deep payload validation happens inside the sink adapter at dispatch time.

`Observer` names a recipient by `MailboxId` and a `kind_id` it'll receive. This is how a DAG terminates: when the observer's input handles all resolve, the substrate dispatches the assembled mail to the recipient using normal mailbox dispatch — `aether.kinds.inputs` (ADR-0033) gates whether the kind is acceptable.

`Edge.slot` is the consumer-side input slot index. For sources (which take no inputs) this field is unused. For transforms (Phase 3) it disambiguates multi-input transforms — a `compose(prompt, embedding)` transform is reachable via `Edge { from: prompt_node, to: compose, slot: 0 }` and `Edge { from: embedding_node, to: compose, slot: 1 }`. Observers also use slots: an observer that receives a kind with multiple `Ref<K>` fields gets each field filled by the edge whose `slot` matches the field's declaration order in the kind schema.

`output_kind_id` on `Transform` declares what the transform produces. Stored on the descriptor (rather than read out of the component's custom section at validate time) so a reviewer reading the descriptor knows the wire shape without cross-referencing component state. Phase 3 still cross-checks against the loaded component's `aether.dag.transforms` section to catch mismatches.

### 3. Validation rules and ordering

Validation runs synchronously on the submit path and returns `Err` before any work dispatches. Ordering matters because some checks are cheap (and would mask the source of a real bug if a later check ran first) and some need a populated graph. The phases:

1. **Structural integrity.** Every `Edge.from` and `Edge.to` references a NodeId that exists in `nodes`. NodeIds are unique within `nodes`. Source nodes have no incoming edges; observer nodes have no outgoing edges. The graph is acyclic (Kahn's-algorithm topological sort succeeds).
2. **Dispatchability.** Every `Source.sink` resolves to a registered sink on this substrate (chassis-specific; see §8). Every `Source.kind_id` is in the sink's accept set. Every `Observer.recipient` is a live mailbox. Every `Observer.kind_id` is in the recipient's `aether.kinds.inputs` manifest *or* the recipient declares a `#[fallback]` (ADR-0033). Every `Transform.transform.component` is a live mailbox; the loaded component's `aether.dag.transforms` section contains an entry at `Transform.transform.index` whose declared `output_kind_id` matches the descriptor.
3. **Type compatibility on edges.** For each edge `Edge { from, to, slot }`: the `from` node's output kind matches the input kind expected at `to`'s `slot`. For sources, output kind = `Source.kind_id`'s reply kind (e.g., a source that dispatches `aether.io.read` produces a `ReadResult`). For transforms, output kind = `Transform.output_kind_id`. For observers, the slot maps to a `Ref<K>` field in the observer's `kind_id` schema; that field's `K` must match the upstream output.

Validation phases short-circuit on first failure. The reply's `DagError` carries a structured variant indicating which phase failed and which node/edge tripped it:

```rust
enum DagError {
    DuplicateNodeId(NodeId),
    UnknownNodeId(NodeId),
    Cycle(Vec<NodeId>),
    SourceWithIncomingEdge(NodeId),
    ObserverWithOutgoingEdge(NodeId),
    UnknownSink(String),
    UnknownRecipient(u64),
    KindNotAccepted { node: NodeId, kind_id: u64, sink_or_recipient: String },
    UnknownTransform { node: NodeId, component: u64, index: u32 },
    TransformOutputMismatch { node: NodeId, declared: u64, manifest: u64 },
    EdgeTypeMismatch { edge_index: u32, expected_kind: u64, got_kind: u64 },
    TooLarge { reason: String },
}
```

`TooLarge` covers cap violations (descriptor byte count, node count, edge count). Defaults: 256 nodes, 1024 edges, 1MB descriptor wire size. Configurable via `AETHER_DAG_MAX_NODES`, `AETHER_DAG_MAX_EDGES`, `AETHER_DAG_MAX_DESCRIPTOR_BYTES`. The caps are belt-and-suspenders against pathological submissions; ordinary content-pipeline DAGs are well under them.

### 4. Executor semantics

On `Ok`, the substrate allocates a `dag_id` (monotonic per-substrate, with a session-namespacing salt to keep ids from colliding across reconnect cycles) and an in-memory `DagState`:

```rust
struct DagState {
    dag_id: u64,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    handles: HashMap<NodeId, HandleId>,    // assigned at submit; resolved at dispatch
    pending_inputs: HashMap<NodeId, u32>,  // remaining unresolved-input count
    status: DagStatus,
    submitted_at: Instant,
    completed_at: Option<Instant>,
}
```

Handle ids for *every* node — including transforms (Phase 3) and observers — are allocated at submit time so they're available in the `submit_result.output_handles` reply and so observer `Ref<K>` slots can be substituted with `Ref::Handle { id, kind_id }` immediately. Source handle ids are ephemeral monotonic per ADR-0045 §3; transform handle ids in Phase 2 are placeholder (Phase 3 swaps in content-addressed ids per ADR-0048); observer handle ids are unused (observers don't produce a handle, they consume).

Execution proceeds:

1. **Sources fire.** Every source node's payload is submitted to its named sink as if `send_mail` had been called. The substrate retains the sink's reply correlation so the response routes back to the DAG executor, not to the submitting session. When the reply arrives, the corresponding `HandleId` resolves in the handle store; ADR-0045 §4's parked-mail flush runs identically to today.
2. **Transforms fire when their inputs all resolve.** Phase 2 has no transforms; this clause activates in Phase 3. The executor decrements `pending_inputs[node]` on each input resolution; at zero, it dispatches the transform via wasmtime `Func::call` (per ADR-0048).
3. **Observer mail dispatches when its inputs all resolve.** Same `pending_inputs` mechanism. The substrate assembles the observer's kind from the input handles (each handle's value substituted into the matching `Ref<K>` slot), and dispatches the resulting mail to the observer's recipient mailbox. The observer is just a regular mail recipient; `aether.kinds.inputs` and `#[handlers]` (ADR-0033) handle dispatch normally.

Source dispatch is parallel (sources have no edges between them, so they can fire concurrently). Transform / observer dispatch is constrained by the actor-per-component scheduler (ADR-0038): a transform pinned to component X runs on X's actor thread, serialised with X's other mail. An observer dispatched to recipient Y is just normal mail on Y's mpsc queue.

When all observer nodes have dispatched (or the DAG has no observers — in which case "all transforms have resolved"), the DAG is complete. `status` flips to `Complete`. Handles assigned to terminal nodes remain in the handle store until their refcounts drop per normal lifecycle (ADR-0045 §9).

### 5. Cancellation

`cancel(dag_id)` walks the DAG state and:

- Marks `status` as `Cancelled` (an internal terminal state; reads back as `Failed { node_id: 0, error: "cancelled" }` from `status_result` — there's no separate `Cancelled` reply variant because callers either know they cancelled or they don't, and `Failed` carries the right semantics for "downstream consumers should release their refs").
- Drops all parked mail tied to the DAG's handle ids (ADR-0045 §4's parking table). Each parked mail's sender — the DAG executor itself — releases its refs on the parked envelope's `Ref::Handle` slots.
- Releases all DAG-held refs on its own handles. Source replies that arrive after cancel are discarded silently (the executor no longer has a routing entry for them).
- Marks the DAG state as reapable. Reaping happens on a slow tick; the entry persists for a short window so a `status` poll racing with a cancel still sees a coherent reply.

In-flight sink work isn't aborted. A `Fetch` already in flight to the network completes server-side; its bytes arrive at the substrate, get routed to the cancelled DAG, and get dropped. This is the same shape as cancellation semantics for `wait_reply` (ADR-0042): the substrate doesn't pretend it can unsend.

### 6. Status semantics

`status(dag_id)` reads the DAG state and replies:

- **`Pending`** — submit succeeded but no source has dispatched yet (purely transient — the substrate dispatches sources synchronously after submit, so this state is only observable in tests).
- **`Running { progress: Vec<NodeStatus> }`** — at least one node has resolved; some haven't. `NodeStatus { node_id, state }` where state is `Pending | Resolved | Failed`.
- **`Complete { outputs: Vec<(NodeId, u64)> }`** — all observer dispatches fired (or all terminal handles resolved if no observers). Outputs are the same `(NodeId, HandleId)` pairs `submit_result` returned, restated for callers that didn't keep the submit reply around.
- **`Failed { node_id, error }`** — at least one node produced an error and downstream couldn't proceed. Specific failure modes:
  - A source's sink replied `Err` for a kind whose reply variant is `Result`-shaped (e.g., `ReadResult::Err`). This isn't a *DAG* failure by default — the `Err` value resolves the handle and downstream nodes consume it via match on the variant. But: an observer whose input is `Ref<ReadResult>` and whose handler doesn't accept `Err` will reject at dispatch — that surfaces as `Failed` here.
  - A transform panic (Phase 3) is a hard `Failed`; the DAG aborts, all unresolved handles drop, parked mail clears.
  - Validation can't fail post-submit (it ran on submit), so `Failed` is always a runtime issue.

The substrate doesn't push status; sessions poll. A pipeline driver (Claude session, content-gen component) decides poll cadence based on its own priorities. ADR-0023's `engine_logs` carries per-node tracing if a richer view is needed.

### 7. DAG state lifecycle

DAG state lives in a `HashMap<DagId, DagState>` on the substrate. State persists past completion so `status` polls are answerable. Reaping runs on a slow tick (default 30s) and removes:

- Entries with `status = Complete` and `completed_at > 60s ago`.
- Entries with `status = Failed | Cancelled` and `completed_at > 300s ago` (longer retention so post-mortem polls work).

Configurable via `AETHER_DAG_RETENTION_COMPLETE_MS`, `AETHER_DAG_RETENTION_FAILED_MS`. Reaping a DAG drops the executor's refs on its handles; the handles themselves only evict when their global refcount hits zero per ADR-0045 §9.

Across `replace_component` (ADR-0022): a DAG whose nodes reference the replaced component is *not* aborted. The new instance inherits the same `MailboxId`; observer dispatches and transform calls (Phase 3) route to the new instance. Mail in flight during the freeze parks per ADR-0022's freeze-drain-swap rules.

Across substrate restart: DAG state is in-memory only and doesn't survive. Persistent handle store (ADR-0048) survives, but DAG topology doesn't — a caller wanting resumability mails `submit` again with the same descriptor and the substrate's content-addressed transform handles (ADR-0048) skip already-computed work.

### 8. Chassis coverage

- **Desktop** — full executor. All sinks dispatchable as DAG sources. `aether.dag.*` accepted.
- **Headless** — same as desktop except chassis-restricted kinds (capture_frame, set_window_mode, etc. — see ADR-0035) reject in DAG validation phase 2 with `UnknownSink` or `KindNotAccepted`.
- **Hub** — no executor. `aether.dag.submit` replies `Err { error: DagError::TooLarge { reason: "unsupported on hub chassis" } }` (reusing the existing variant rather than inventing a new one keeps wire churn low; alternatively a future revision adds `ChassisUnsupported`). The hub bubbles mail to its substrate children per ADR-0037; DAG submission is a substrate concern, not a hub one.

### 9. MCP `submit_dag` tool

The hub exposes `mcp__aether-hub__submit_dag(engine_id, descriptor, timeout_ms?)` as a thin wrapper around `aether.dag.submit`. Same await-reply mechanism `capture_frame` and `load_component` use; the hub forwards the descriptor, awaits `submit_result` via the pending-replies queue, returns the response inline. `descriptor` is a structured JSON object the hub encodes against the `DagDescriptor` schema before forwarding (symmetric to `send_mail`'s param decoding; ADR-0007).

Default `timeout_ms = 5000`. The tool returns immediately on submit-ack — it does not wait for DAG completion. A separate `mcp__aether-hub__dag_status(engine_id, dag_id)` polls; `mcp__aether-hub__dag_cancel(engine_id, dag_id)` cancels. Three tools, mirroring the three mail kinds.

This is the surface ADR-0046 names when it says "Claude-in-harness can declare pipelines without writing a component." A content-gen recipe loaded by a harness session becomes a `DagDescriptor` the harness submits via this tool; the substrate validates, dispatches, and the harness polls for completion.

### 10. Wire stability

Phase 3 (transforms, ADR-0048) doesn't change the wire. The `Transform` Node variant is already in the descriptor; Phase 3 lights up its dispatch path. The `aether.dag.transforms` custom section is read at component load — orthogonal to the descriptor wire. Cross-version submitters work: a Phase 2 client submitting a descriptor without `Transform` nodes runs unchanged on Phase 3 substrates.

ADR-0045 Phase 4+ work (incremental recompute, distributed handle stores) lives behind the descriptor — executor sophistication, not wire surface. Future ADRs may add a `node_options` field to nodes for things like "pin this transform's output" or "memoize aggressively across restarts," but those are additive optional fields; the canonical-bytes machinery (ADR-0032) handles the encoding.

## Consequences

### Positive

- **Pipeline declaration becomes a first-class operation.** A caller stops writing chained `#[handler]`s for graph-shaped work and instead hands a descriptor to the substrate. ADR-0046's content-gen pipeline lands here as the headline customer; render DAGs and asset-pipeline workflows pick this up next.
- **Validation up front.** A pipeline that's structurally bad fails on submit, not halfway through. Type errors, unknown sinks, missing recipients all surface synchronously with structured error variants.
- **Claude-in-harness composition.** The MCP `submit_dag` tool gives the harness a way to compose multi-step jobs without authoring a component. ADR-0008's observation path + this tool cover declarative-side and observation-side parity.
- **Wire foundation for Phase 3+.** The `Transform` variant is already in the descriptor; ADR-0048 lights up dispatch without changing wire format. Same property for ADR-0045's parked Phase 4+ work — node options, persistence hints, recomputation directives all add as optional fields.
- **Cancellation is real.** Long-running DAGs (image generation, long fetches) can be aborted from the harness side; in-flight sinks complete server-side but the substrate stops paying attention.

### Negative

- **DAG state is one more substrate registry.** Adds `HashMap<DagId, DagState>` plus a reaping tick. Test scaffolding and lifecycle tracing grow commensurately.
- **In-flight cancellation isn't real.** Sinks that have already dispatched complete on the remote side; cancel only stops the substrate from caring. A user who cancels because they noticed their fetch URL was wrong is still on the hook for the bandwidth. This is the same semantics as `wait_reply` and not novel — but it bears mention because DAG cancellation reads as "stop the work" and isn't.
- **Error variants are narrow at first.** Phase 2 `DagError` covers structural and dispatchability errors; runtime errors surface as `Failed` with a `String` message. Phase 3+ work likely wants a `Failed` reply with structured per-node error info, which means another wire revision (additive).
- **Validator complexity is moderate.** Cycle detection, type-compatibility cross-checks, sink/recipient lookup, and component-manifest cross-validation are all simple individually; ordering them so error messages are useful (rather than "first failure wins, regardless of which is the real problem") takes care.

### Neutral

- **Existing kinds dispatch unchanged.** A DAG source for `aether.io.read` builds a normal `Read` request and forwards it to the io sink; the sink doesn't know it came from a DAG. ADR-0041 / ADR-0043 / ADR-0039 / ADR-0025 sink contracts hold.
- **Actor-per-component scheduling unchanged.** ADR-0038 keeps its semantics — the DAG executor is a substrate-level orchestrator, not a scheduling layer; transform / observer dispatches funnel through the same per-component mpsc queues as any other mail.
- **Hub mail bubbling unchanged.** ADR-0037 keeps its semantics — the hub doesn't host DAGs, it forwards mail to substrates.

## Alternatives considered

- **Push-based status (server-streamed).** Rejected for v1: the harness already polls `engine_logs` and receive_mail on its own cadence; adding a push channel is design surface that doesn't pay off until DAGs are long-lived enough that polling becomes wasteful. Forward-compatible: a future ADR can add a `subscribe_dag_status` kind without changing the descriptor or other reply kinds.
- **Compile transforms into the descriptor as wasm bytecode.** Rejected for the obvious reason: descriptors would explode in size and the substrate would need to instantiate ad-hoc components. Phase 3's `transform_id = (component_mailbox, transform_index)` keeps transforms tied to loaded components; the descriptor only references them.
- **Inline observer kinds (carry the observer's kind value verbatim, no recipient).** Rejected: a DAG that "observes" by attaching a kind value to the executor and sending it nowhere is weird. The observer-as-mail-dispatch shape composes with the existing mailbox / handler infrastructure (ADR-0033). A DAG that needs to "fan out to all sessions" is doing observation, which ADR-0008 already handles via the broadcast sink.
- **Synchronous submit (block until DAG completes).** Rejected: long-running DAGs (image gen at 60s/image, fetch chains) would block the submitting actor. Async submit with `dag_id` + poll/wait separation matches the rest of the substrate's mail-shaped surface. A caller that *wants* synchronous semantics composes them on top: submit, then `wait_reply` for an observer-emitted "DAG complete" mail.
- **DAG submit as a host-fn instead of a mail kind.** Rejected: ADR-0002 keeps the privileged FFI surface small. Mail-shaped submit gives Claude observability for free, plays nicely with capability gating (ADR-0044), and follows the same pattern as ADR-0041's io sink and ADR-0043's net sink.
- **Allow nested DAGs (a node's payload is itself a `DagDescriptor`).** Rejected for v1: composability via observers (DAG A's observer is a component method that submits DAG B) is sufficient and keeps the executor's invariants simple. Nesting can come back as a future ADR if recipe ergonomics demand it.
- **Ship Phase 2 + Phase 3 as one ADR.** Tempting because both reference the same descriptor format. Rejected: transforms add a guest-side macro, custom section, and wasmtime integration — separate engineering surface deserving separate review. Phase 2 alone is shippable and useful (sources + observers cover the "declare a pipeline made of sink calls + dispatch the result somewhere" workflow).

## Follow-up work

- **PR**: kinds + schema-derive — `DagDescriptor` and the three reply types in `aether-kinds`, with `#[derive(Schema)]` covering the enum variants. New `NodeId` / `TransformRef` types as wire-stable aliases.
- **PR**: substrate executor — `DagState`, validation phases, source dispatch + parked-mail integration, observer dispatch, reaping tick, cancellation. Integration tests covering each failure path of `DagError` and the happy-path source → observer flow.
- **PR**: hub MCP surface — `submit_dag`, `dag_status`, `dag_cancel` tools wrapping the three mail kinds.
- **Parked, ADR-0048**: transforms (Phase 3) — `#[transform]` macro, `aether.dag.transforms` custom section, wasmtime `Func::call` integration, content-addressed transform handle ids, persistent handle store.
- **Parked, future ADR**: structured per-node error info on `Failed` (replacing `error: String`).
- **Parked, future ADR**: server-streamed status / observer-pushed completion notification, if polling becomes the bottleneck.
- **Parked, future ADR**: nested DAGs (`Source { kind_id: aether.dag.submit }`) if recipe ergonomics demand it.
