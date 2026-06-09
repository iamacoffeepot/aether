# ADR-0047: DAG submit/cancel/status (Phase 2 of ADR-0045)

- **Status:** Accepted
- **Date:** 2026-04-25
- **Revised:** 2026-05-19 (iamacoffeepot/aether#972) — mailbox renamed `aether.control` → `aether.dag`; raw `u64` wire ids replaced with typed newtypes (tagged-string serialization per ADR-0064/0065); "sink" terminology swept to "mailbox"/"cap" per ADR-0074 Phase 5; retired broadcast-sink reference removed (issue #775). Submit/validation semantics unchanged.
- **Revised:** 2026-05-20 (iamacoffeepot/aether#1017) — adds the `Call` mid-graph effectful node (input handles → cap dispatch → output **bundle**), filling the previously-empty mid+effectful cell of the position×effect grid and completing the node taxonomy before the wire freezes. `Call`'s output is an **ordered, heterogeneous, self-describing bundle of the replies it received, closed when the call settles** (ADR-0080) — each element carries its own `KindId` (a `(KindId, payload)` reply) and the bundle may mix kinds, so the `Call` variant declares **no** `output_kind_id`; a single-reply cap yields a 1-element bundle, a multi-reply cap yields N elements, and there is no single-reply enforcement. Also adds a `version` field to `DagDescriptor` so a future node-set change fails cleanly rather than mis-decodes. `Call` dispatches as **its own causal root** so settlement scopes to the call (not the whole DAG), uses the inherited `send` path + `spawn_inherit` so replies and worker threads descend from that root, and requires ADR-0080's **reliable settlement** (iamacoffeepot/aether#1031) — closing a bundle on a premature `Settled` would drop late replies into a single-assignment handle. See §2 (the variant + grid + bundle semantics), §3 (validation), §4 (executor + the three conditions + timeout parity), and §10 (versioning + deferred axes).
- **Revised:** 2026-05-20 (iamacoffeepot/aether#1037) — **§3 reply-kind correction.** Handlers promise nothing about what they reply (zero/one/many replies, any kinds), so a request kind's reply kind is not a property of the kind and is not knowable at submit time. The prior §3 line "a source's output kind = `Source.kind_id`'s reply kind (`aether.fs.read` → `ReadResult`)" was wrong on this basis and is struck: edges *out of a `Source`* are not type-checked. Type-compat on edges checks only **statically declared** output kinds — a `Transform`'s `output_kind_id` and a `Call`'s `Bundle`. Phase 2 dispatchability accept-set queries (`accepts` / `has_fallback`) are served by a new substrate capability registry (iamacoffeepot/aether#1037), since the routing registry does not carry accept-sets. Reply *contracts* would be a separate, deliberately-planned new handler type with an explicit declared return — not assumable by the validator today.
- **Revised:** 2026-05-20 (iamacoffeepot/aether#1031) — **settlement is exact, not a hint.** The "one-batch quiescence window" this ADR cited for the `Call` bundle-close was abandoned (see ADR-0080 §6, revised): reliable settlement is achieved by the hold contract (a deferred reply holds the chain via a `SettlementHold` until its last send), under which `Settled { call_root }` fires exactly when the call's chain reaches `(in_flight == 0 && held_open == 0)`. The `Call` executor closes its bundle on that exact signal — no window.

## Context

ADR-0045 committed to handles as a foundational primitive and laid out a three-phase shipping plan. Phase 1 (handle store, `Ref<K>` wire type, parked-mail dispatch) is shipping as a foundational PR sequence. Phase 1's value is byte-bypassing for chained `#[handler]`s — a component can grab a handle from one reply and embed it in the next mail's `Ref<K>` field without ever copying the underlying bytes. Components still write the chain by hand.

Phase 2 raises the abstraction. A caller — a component, the MCP harness, or a content-generation pipeline like ADR-0046 — declares a multi-node graph of operations to the substrate, hands over execution, and gets back terminal handles when the graph completes. The substrate becomes the DAG executor: validate topology + types up front, dispatch source nodes, await handle resolution via Phase 1's parked-mail mechanism, dispatch observer mail when terminal handles land. This is the primitive that lets pipeline composition stop being a per-call-site control-flow exercise.

Phase 2 ships **sources + observers only**. Transform nodes (the third `Node` variant ADR-0045 lists) are parked to ADR-0048 because they need a separate engine surface: a guest-side `#[transform]` macro, a wasm custom section parallel to `aether.kinds.inputs`, and wasmtime `Func::call` integration. A Phase 2 DAG that wants transform-shaped behaviour wraps the chain segment in a no-op observer that forwards the handle. The wire format described here reserves the `Transform` variant so Phase 3 can light it up without a breaking change.

ADR-0045's §5 sketched the wire shape and the executor's high-level role. This ADR is the focused review surface for the descriptor format details, validation phase ordering, executor scheduling under ADR-0038's actor-per-component model, cancellation semantics, status reply timing, the MCP tool, and chassis coverage. It's the piece a reviewer signs off on before substrate code lands.

## Decision

### 1. Mail surface

Three request kinds, three reply kinds, all on the `"aether.dag"` mailbox alongside the other `aether.<name>` chassis mailboxes (`aether.component`, `aether.input`, etc.). ADR-0074 Phase 5 retired the `aether.control` name this ADR originally used.

```rust
aether.dag.submit { descriptor: DagDescriptor }
aether.dag.cancel { dag_id: DagId }
aether.dag.status { dag_id: DagId }

aether.dag.submit_result : Ok  { dag_id: DagId, output_handles: Vec<(NodeId, HandleId)> }
                        | Err { error: DagError }
aether.dag.cancel_result : Ok  { cancelled: bool }
                         | Err { error: String }
aether.dag.status_result : Pending  { node_count: u32, ready: u32, in_flight: u32, parked: u32 }
                         | Running  { progress: Vec<NodeStatus> }
                         | Complete { outputs: Vec<(NodeId, HandleId)> }
                         | Failed   { node_id: NodeId, error: String }
```

`DagId`, `MailboxId`, `KindId`, and `HandleId` are `u64`-backed newtypes that serialize as tagged strings (`dag-`/`mbx-`/`knd-`/`hdl-XXXX-XXXX-XXXX`) on the MCP JSON wire per ADR-0064/0065; the Rust definitions here use the newtype forms.

Submit returns synchronously with `dag_id` as soon as validation completes — *before* any source dispatches. The reply also carries the full `output_handles` list (handle ids assigned to terminal nodes) so a caller can hand them to downstream consumers immediately, even though the values aren't resolved yet. This is consistent with ADR-0045 §4: `Ref::Handle` slots can travel before their values resolve; the substrate parks dispatch until they do.

Cancel returns `cancelled: true` if the DAG was found and torn down, `false` if it had already completed or never existed (idempotent). In-flight mailbox calls aren't aborted — the substrate stops awaiting their replies and discards them when they arrive. Parked mail tied to the cancelled DAG drops with a `CapabilityDenied`-shaped diagnostic.

Status is poll-shaped, not push-shaped. The session asks; the substrate replies with the current state. `Pending` covers the brief window between submit-ack and first source dispatch; `Running` covers everything until terminal handles land; `Complete` is terminal and persists until the dag_id is reaped (see §7); `Failed` is terminal and also persists. There is no streamed status — push-style observation goes through normal observer nodes the caller wires into the DAG.

### 2. DagDescriptor wire format

```rust
struct DagDescriptor {
    version: u16,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

enum Node {
    Source    { id: NodeId, mailbox: MailboxId, kind_id: KindId, payload: Vec<u8> },
    Transform { id: NodeId, transform: TransformRef, output_kind_id: KindId },
    Call      { id: NodeId, recipient: MailboxId, kind_id: KindId },
    Observer  { id: NodeId, recipient: MailboxId, kind_id: KindId },
}

struct Edge { from: NodeId, to: NodeId, slot: u32 }

struct TransformRef { component: MailboxId, index: u32 }

type NodeId = u32;
```

The shape locks in for Phase 2 even though `Transform` doesn't dispatch yet. Phase 3 lights it up by adding the `aether.dag.transforms` custom section read at component load and the dispatch path that calls into wasmtime when input handles all resolve.

#### The `Call` node — mid-graph effectful (iamacoffeepot/aether#1017)

The original three variants were enumerated by use case — "fire a mailbox call at the root" (`Source`), "compute purely in the middle" (`Transform`, ADR-0048), "dispatch the assembled result somewhere terminal" (`Observer`). That left a hole: every cap call between the first and last in a pipeline. A content-gen recipe that frames a prompt, distills it, then translates it (ADR-0046) is three *effectful* cap calls in series, each consuming the prior's output and feeding the next — but `Source` takes no inputs (it's a root) and `Observer` produces no output (it's terminal). The middle calls have nowhere to live. ADR-0047 as merged forced such a recipe to wrap each mid-graph cap call in a component `#[handler]` segment, giving up DAG-shaped composition for exactly the part a content pipeline is mostly made of.

The nodes are really a cross-product of two axes — **position** (where in the graph: root / mid / terminal) × **effect** (whether the node touches a cap: pure / effectful):

| | **root** | **mid** | **terminal** |
|---|---|---|---|
| **effectful** | `Source` | `Call` | `Observer` |
| **pure** | (degenerate) | `Transform` | (degenerate) |

`Call` is the mid+effectful cell — the one the original by-use-case enumeration missed. The two remaining cells are degenerate, not missing:

- **root+pure** — a pure node with no inputs is a constant. A `Source` payload already carries that constant, so a dedicated node buys nothing.
- **terminal+pure** — a terminal node with no output and no effect does nothing. There's no observable result and no side effect, so there's nothing to wire it to.

So the three originals were enumerated by use case; `Call` completes the *derived* grid. After it, the useful taxonomy is complete — every (position, effect) cell that does something has a node.

`Call` is, equivalently:

- **"an `Observer` that captures its replies as an output handle"** — like `Observer`, its inputs arrive via **incoming edges** and the substrate assembles them into a request of `kind_id` (the same slot-fill mechanism §4 describes for observers); unlike `Observer`, it dispatches to a *capability* `recipient` and the **correlated replies (each self-describing, carrying its own `KindId`, and possibly of differing kinds) accumulate into the node's output bundle**.
- **"a `Source` that also takes inputs"** — like `Source`, it dispatches a cap call and collects the correlated reply traffic; unlike `Source`, the request is assembled from upstream handles rather than carried as an opaque `payload`.

##### Output is a settlement-closed reply bundle

`Call`'s output is an **ordered bundle of the replies it received, closed when the call settles** — not a single value. Concretely, the executor:

1. dispatches the assembled request (of `kind_id`) to `recipient` **as its own causal root** (`call_root`) — *not* inheriting the DAG's root — so settlement (ADR-0080) scopes to *this call* rather than the whole DAG;
2. **collects every correlated reply** (matching the dispatch's correlation) into an **ordered bundle** as they arrive — order is arrival order on the executor's inbox, which is the cap's emission order (FIFO per sender);
3. subscribes `SubscribeSettlement { root: call_root }` (ADR-0080 §3/§4); on `Settled { root: call_root }` the bundle **closes** and the output handle resolves to an **ordered, self-describing bundle** — each element a `(KindId, payload)` reply, and the bundle may mix kinds. A single-reply cap (e.g. `aether.fs.read` → a 1-element bundle whose single element is a `ReadResult`) yields a **1-element bundle**; a cap that emits N correlated replies yields an **N-element bundle**.

There is **no single-reply enforcement**. Multiple replies are the *feature*, not a contract violation — this revision deletes the prior version's "police exactly one reply / log extras as a violation" machinery. A cap that streams several correlated replies before settling is a first-class producer of a multi-element bundle.

`Bundle` is itself a **first-class meta-type** — a `Kind` whose schema is an ordered `Vec<(KindId, payload)>` of self-describing elements — and is the **uniform output of every `Call`** (single-reply → 1-element, zero replies → empty, N replies → N). The heterogeneity lives in the tagged *elements*, not in `Bundle`'s own (fixed) schema, so it is an ordinary handle-able value that slots into the existing kind/handle system with no special-casing: a downstream node simply declares a `Bundle` input.

##### Three conditions the bundle model rests on

The bundle is correct only if three things hold. They are load-bearing — spell them out at the executor and respect them in iamacoffeepot/aether#976.

1. **Per-`Call` root.** The dispatch must be its own causal root (`call_root`) so settlement is scoped to the call. Subscribing on the *DAG's* root would wait for the entire DAG to settle before any one call's bundle could close — useless. The pattern already exists in the engine: ADR-0082's lifecycle driver does broadcast-then-`SubscribeSettlement` per stage against a per-stage root. The exact mechanics of minting a fresh root *from inside the executor* (rather than from a handler context that inherits one) is an iamacoffeepot/aether#976 implementation detail — ADR-0080 §5 mints a root at any "no in-flight mail" send site, and the executor's per-call dispatch is one.

2. **Inherited dispatch + `spawn_inherit`.** The cap's handling, its replies, and any ephemeral worker thread it spawns (the canonical case being a spawn-and-die HTTP worker) must all *descend* from `call_root` so the settlement counter sees them. That means: the dispatch uses the inherited `send` path — **not** `send_detached`, which per ADR-0080 §7 mints no parent linkage so the reply inherits the *receiver's* tree and settlement on `call_root` never sees it — and any worker thread is `spawn_inherit` (ADR-0080 §12: settlement gates the root on those threads exiting). Detach the dispatch or the worker and the bundle closes blind: `Settled` fires while replies are still in flight under a different (or no) root, and they never land in the bundle.

3. **Reliable settlement (a real dependency, not free).** ADR-0080 §6 fires `Settled` as a **hint** that can fire early (the counter briefly hits zero from out-of-order trace events) and **re-fire** (a late `Sent` bumps it back up, then it transitions to zero again). That's fine for a consumer that merely *unblocks a gate* — the first fire unblocks, duplicates are idempotent no-ops, no state is destroyed. But **closing a bundle on an early fire drops every late reply**, and the output handle is single-assignment, so a closed bundle cannot be re-opened. Idempotency doesn't save it: the damage is the close, not a repeated unblock. So the bundle-`Call` is the **first consumer that needs *reliable* settlement**, and it requires ADR-0080 §6's deferred **"one-batch quiescence window"** (fire `Settled { root }` only after `counter[root]` has been zero for one full batch interval, so a late out-of-order `Sent` has landed before the fire). This is a **hard prerequisite** for the bundle-`Call` executor (iamacoffeepot/aether#976), tracked in iamacoffeepot/aether#1031.

##### Unbounded / never-settling caps

A cap that never settles (emits replies forever, or never replies at all) would otherwise keep the bundle open forever. The `Call`'s **timeout** bounds it — the same cancellation/timeout parity §4 describes. On timeout the node *fails* (`Failed { node_id, error }` per §6) rather than buffering replies forever; a non-settling recipient cannot hang the DAG.

`Call` introduces **no new node-level runtime mechanism beyond the settlement subscription**. The request-assembly-from-inputs is `Observer`'s path; the per-call root + reply-collection + settlement-close is the same broadcast-then-subscribe shape ADR-0082's lifecycle driver runs. Downstream nodes park on the unresolved output handle exactly as they park on a source's per ADR-0045 §4 — they just receive a bundle when it resolves rather than a scalar.

`NodeId` is descriptor-local — a `u32` index assigned by the submitter, not a globally unique handle id. Edges reference NodeIds; the substrate maps NodeIds to handle ids during execution. Two DAGs submitted in parallel can both have a `NodeId(0)` without collision because the namespaces don't cross.

`Source` carries `payload: Vec<u8>` rather than a structured kind value because the substrate doesn't decode source payloads on the submit path — it forwards them to the named mailbox as opaque bytes, identical to how `send_mail` works today. Validation only checks that `kind_id` is in the mailbox's accept set (see §3); deep payload validation happens inside the mailbox's handler at dispatch time.

`Observer` names a recipient by `MailboxId` and a `kind_id` it'll receive. This is how a DAG terminates: when the observer's input handles all resolve, the substrate dispatches the assembled mail to the recipient using normal mailbox dispatch — `aether.kinds.inputs` (ADR-0033) gates whether the kind is acceptable.

`Edge.slot` is the consumer-side input slot index. For sources (which take no inputs) this field is unused. For transforms (Phase 3) it disambiguates multi-input transforms — a `compose(prompt, embedding)` transform is reachable via `Edge { from: prompt_node, to: compose, slot: 0 }` and `Edge { from: embedding_node, to: compose, slot: 1 }`. Observers and calls also use slots: each fills a `Ref<K>` field in its assembled-kind schema (`Observer.kind_id` / `Call.kind_id`) from the edge whose `slot` matches the field's declaration order.

`output_kind_id` on `Transform` declares what the node produces. It's stored on the descriptor (rather than read out of the component's custom section at validate time) so a reviewer reading the descriptor knows the wire shape without cross-referencing component state; Phase 3 still cross-checks against the loaded component's `aether.dag.transforms` section to catch mismatches. A `Call` declares no output kind — its replies are heterogeneous and self-describing, so the output handle is a `Bundle` typed at the bundle level, not against a declared element kind.

### 3. Validation rules and ordering

Validation runs synchronously on the submit path and returns `Err` before any work dispatches. Ordering matters because some checks are cheap (and would mask the source of a real bug if a later check ran first) and some need a populated graph. The phases:

1. **Structural integrity.** Every `Edge.from` and `Edge.to` references a NodeId that exists in `nodes`. NodeIds are unique within `nodes`. Source nodes have no incoming edges; observer nodes have no outgoing edges. The graph is acyclic (Kahn's-algorithm topological sort succeeds).
2. **Dispatchability.** Every `Source.mailbox` resolves to a registered mailbox on this substrate (chassis-specific; see §8). Every `Source.kind_id` is in the mailbox's accept set. Every `Observer.recipient` is a live mailbox. Every `Observer.kind_id` is in the recipient's `aether.kinds.inputs` manifest *or* the recipient declares a `#[fallback]` (ADR-0033). Every `Transform.transform.component` is a live mailbox; the loaded component's `aether.dag.transforms` section contains an entry at `Transform.transform.index` whose declared `output_kind_id` matches the descriptor. Every `Call.recipient` is a live mailbox; every `Call.kind_id` is in its accept set (same check `Source` runs, since a `Call` is a cap dispatch).
3. **Type compatibility on edges.** A node's output kind is type-checked against a downstream input slot only where the output is **statically declared**. The downstream *input* side is always declared: for observers and calls the `slot` maps to a `Ref<K>` field in the consumer's `kind_id` schema (`Observer.kind_id` / `Call.kind_id`); for transforms (Phase 3) it maps to a transform input parameter. The upstream *output* side is knowable for only two node kinds — a `Transform` produces its declared `Transform.output_kind_id`, and a `Call` produces a **self-describing `Bundle`** (heterogeneous: the validator can only check that the consumer's input slot **accepts a `Bundle`**, not the per-element kinds, which are dynamic and dispatched in the consumer's body at runtime; a `Transform` or observer downstream of a `Call` must work with `Bundle`s). **Edges out of a `Source` are not type-checked**: a source's output is whatever its mailbox replies, and a handler promises nothing about what it replies (zero/one/many replies, any kinds), so the output kind is not knowable at submit time — Phase 2 having confirmed the source kind is dispatchable to its mailbox (`accepts`) is all that can be said. (A `Call`'s *input* side is checked the same way an observer's is: each incoming edge's `slot` fills a `Ref<K>` field of the call's assembled-request schema — but whether the *upstream producer* actually supplies a `K` is verifiable only when that producer is itself a `Call` (→ `Bundle`) or a `Transform` (→ declared output), not when it is a `Source`.) Mismatch where both sides are known → `EdgeTypeMismatch { edge_index, expected_kind, got_kind }`.

Validation phases short-circuit on first failure. The reply's `DagError` carries a structured variant indicating which phase failed and which node/edge tripped it:

```rust
enum DagError {
    DuplicateNodeId(NodeId),
    UnknownNodeId(NodeId),
    Cycle(Vec<NodeId>),
    SourceWithIncomingEdge(NodeId),
    ObserverWithOutgoingEdge(NodeId),
    UnknownSink(String),
    UnknownRecipient(MailboxId),
    KindNotAccepted { node: NodeId, kind_id: KindId, mailbox_or_recipient: String },
    UnknownTransform { node: NodeId, component: MailboxId, index: u32 },
    TransformOutputMismatch { node: NodeId, declared: KindId, manifest: KindId },
    EdgeTypeMismatch { edge_index: u32, expected_kind: KindId, got_kind: KindId },
    TooLarge { reason: String },
}
```

`TooLarge` covers cap violations (descriptor byte count, node count, edge count). Defaults: 256 nodes, 1024 edges, 1MB descriptor wire size. Configurable via `AETHER_DAG_MAX_NODES`, `AETHER_DAG_MAX_EDGES`, `AETHER_DAG_MAX_DESCRIPTOR_BYTES`. The caps are belt-and-suspenders against pathological submissions; ordinary content-pipeline DAGs are well under them.

### 4. Executor semantics

On `Ok`, the substrate allocates a `dag_id` (monotonic per-substrate, with a session-namespacing salt to keep ids from colliding across reconnect cycles) and an in-memory `DagState`:

```rust
struct DagState {
    dag_id: DagId,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    handles: HashMap<NodeId, HandleId>,    // assigned at submit; resolved at dispatch
    pending_inputs: HashMap<NodeId, u32>,  // remaining unresolved-input count
    status: DagStatus,
    submitted_at: Instant,
    completed_at: Option<Instant>,
}
```

Handle ids for *every* node — including transforms (Phase 3), calls, and observers — are allocated at submit time so they're available in the `submit_result.output_handles` reply and so downstream `Ref<K>` slots can be substituted with `Ref::Handle { id, kind_id }` immediately. Source handle ids are ephemeral monotonic per ADR-0045 §3; call handle ids are likewise ephemeral monotonic, but they resolve to an **ordered bundle** closed on settlement (see the `Call` clause below) rather than to a single reply value; transform handle ids in Phase 2 are placeholder (Phase 3 swaps in content-addressed ids per ADR-0048); observer handle ids are unused (observers don't produce a handle, they consume).

Execution proceeds:

1. **Sources fire.** Every source node's payload is submitted to its named mailbox as if `send_mail` had been called. The substrate retains the mailbox's reply correlation so the response routes back to the DAG executor, not to the submitting session. When the reply arrives, the corresponding `HandleId` resolves in the handle store; ADR-0045 §4's parked-mail flush runs identically to today.
2. **Transforms fire when their inputs all resolve.** Phase 2 has no transforms; this clause activates in Phase 3. The executor decrements `pending_inputs[node]` on each input resolution; at zero, it dispatches the transform via wasmtime `Func::call` (per ADR-0048).
3. **Calls dispatch when their inputs all resolve, then collect a bundle until settlement.** Same `pending_inputs` mechanism as observers gates the dispatch. The executor assembles the call's request kind (`Call.kind_id`) from the resolved input handles — each handle's value substituted into the matching `Ref<K>` slot, exactly as for an observer — then dispatches it to `Call.recipient` **as its own causal root** (`call_root`, *not* the DAG's root) via the inherited `send` path, and subscribes `SubscribeSettlement { root: call_root }` (ADR-0080). The three conditions (per-call root / inherited dispatch + `spawn_inherit` for any cap-spawned worker / reliable settlement) are spelled out in the `Call` clause of §2 and are load-bearing here. As each **correlated reply** arrives (any kind — the bundle is heterogeneous) on the executor's inbox, it appends to the call's **ordered bundle** (arrival order = the cap's emission order, FIFO per sender). On `Settled { root: call_root }` the bundle **closes** and the call's `HandleId` resolves to the ordered self-describing bundle, and the parked-mail flush runs. A single-reply cap closes after one element; a multi-reply cap accumulates N. So a `Call` is "observer-assemble, then dispatch-as-own-root, collect correlated replies, close on settle": input resolution gates the dispatch, settlement gates the output. Because closing a bundle on an *early* `Settled` would drop late replies into a single-assignment handle, this path requires reliable settlement (ADR-0080 §6's one-batch quiescence window, iamacoffeepot/aether#1031) — a hard prerequisite, not the hint-grade `Settled` the idempotent gate consumers tolerate.
4. **Observer mail dispatches when its inputs all resolve.** Same `pending_inputs` mechanism. The substrate assembles the observer's kind from the input handles (each handle's value substituted into the matching `Ref<K>` slot), and dispatches the resulting mail to the observer's recipient mailbox. The observer is just a regular mail recipient; `aether.kinds.inputs` and `#[handlers]` (ADR-0033) handle dispatch normally.

Source dispatch is parallel (sources have no edges between them, so they can fire concurrently). Transform / observer dispatch is constrained by the actor-per-component scheduler (ADR-0038): a transform pinned to component X runs on X's actor thread, serialised with X's other mail. An observer dispatched to recipient Y is just normal mail on Y's mpsc queue.

When all observer nodes have dispatched (or the DAG has no observers — in which case "all transforms and calls have resolved"), the DAG is complete. `status` flips to `Complete`. Handles assigned to terminal nodes remain in the handle store until their refcounts drop per normal lifecycle (ADR-0045 §9).

A `Call`'s reply collection gets the **same timeout and cancellation handling as a `Source`'s reply**. A cap that **never settles** — never replies at all, or streams replies forever without closing — would otherwise hold the bundle open indefinitely. The `Call`'s timeout bounds it: a recipient that hasn't settled by the timeout **fails the `Call` node** (surfacing as `Failed { node_id, error }` in §6), and parks/drops its downstream consumers per the cancellation rules in §5. The node *fails* rather than resolving a partial bundle — buffering replies forever, or silently truncating an unsettled stream into a "good enough" bundle, are both wrong; an unbounded producer is a node failure, not a node result. A non-settling `Call` recipient must **not** be able to hang the DAG: the timeout fires, the node fails, and the DAG tears down its parked mail rather than waiting forever on a settlement that never comes. This reuses the existing source-reply timeout/cancellation machinery applied to a mid-graph node, not a new timeout mechanism.

### 5. Cancellation

`cancel(dag_id)` walks the DAG state and:

- Marks `status` as `Cancelled` (an internal terminal state; reads back as `Failed { node_id: 0, error: "cancelled" }` from `status_result` — there's no separate `Cancelled` reply variant because callers either know they cancelled or they don't, and `Failed` carries the right semantics for "downstream consumers should release their refs").
- Drops all parked mail tied to the DAG's handle ids (ADR-0045 §4's parking table). Each parked mail's sender — the DAG executor itself — releases its refs on the parked envelope's `Ref::Handle` slots.
- Releases all DAG-held refs on its own handles. Source and call replies that arrive after cancel are discarded silently (the executor no longer has a routing entry for them).
- Marks the DAG state as reapable. Reaping happens on a slow tick; the entry persists for a short window so a `status` poll racing with a cancel still sees a coherent reply.

In-flight mailbox work isn't aborted. A `Fetch` already in flight to the network completes server-side; its bytes arrive at the substrate, get routed to the cancelled DAG, and get dropped. This is the same shape as cancellation semantics for `wait_reply` (ADR-0042): the substrate doesn't pretend it can unsend.

### 6. Status semantics

`status(dag_id)` reads the DAG state and replies:

- **`Pending`** — submit succeeded but no source has dispatched yet (purely transient — the substrate dispatches sources synchronously after submit, so this state is only observable in tests).
- **`Running { progress: Vec<NodeStatus> }`** — at least one node has resolved; some haven't. `NodeStatus { node_id, state }` where state is `Pending | Resolved | Failed`.
- **`Complete { outputs: Vec<(NodeId, HandleId)> }`** — all observer dispatches fired (or all terminal handles resolved if no observers). Outputs are the same `(NodeId, HandleId)` pairs `submit_result` returned, restated for callers that didn't keep the submit reply around.
- **`Failed { node_id, error }`** — at least one node produced an error and downstream couldn't proceed. Specific failure modes:
  - A source's mailbox replied `Err` for a kind whose reply variant is `Result`-shaped (e.g., `ReadResult::Err`). This isn't a *DAG* failure by default — the `Err` value resolves the handle and downstream nodes consume it via match on the variant. But: an observer whose input is `Ref<ReadResult>` and whose handler doesn't accept `Err` will reject at dispatch — that surfaces as `Failed` here.
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

- **Desktop** — full executor. All mailboxes dispatchable as DAG sources. `aether.dag.*` accepted.
- **Headless** — same as desktop except chassis-restricted kinds (capture_frame, set_window_mode, etc. — see ADR-0035) reject in DAG validation phase 2 with `UnknownSink` or `KindNotAccepted`.
- **Hub** — no executor. `aether.dag.submit` replies `Err { error: DagError::TooLarge { reason: "unsupported on hub chassis" } }` (reusing the existing variant rather than inventing a new one keeps wire churn low; alternatively a future revision adds `ChassisUnsupported`). The hub bubbles mail to its substrate children per ADR-0037; DAG submission is a substrate concern, not a hub one.

### 9. MCP `submit_dag` tool

The hub exposes `mcp__aether-hub__submit_dag(engine_id, descriptor, timeout_ms?)` as a thin wrapper around `aether.dag.submit`. Same await-reply mechanism `capture_frame` and `load_component` use; the hub forwards the descriptor, awaits `submit_result` via the pending-replies queue, returns the response inline. `descriptor` is a structured JSON object the hub encodes against the `DagDescriptor` schema before forwarding (symmetric to `send_mail`'s param decoding; ADR-0007).

Default `timeout_ms = 5000`. The tool returns once validation completes (the submit-ack); it does not wait for DAG execution. A separate `mcp__aether-hub__dag_status(engine_id, dag_id)` polls; `mcp__aether-hub__dag_cancel(engine_id, dag_id)` cancels. Three tools, mirroring the three mail kinds.

This is the surface ADR-0046 names when it says "Claude-in-harness can declare pipelines without writing a component." A content-gen recipe loaded by a harness session becomes a `DagDescriptor` the harness submits via this tool; the substrate validates, dispatches, and the harness polls for completion.

### 10. Wire stability

Phase 3 (transforms, ADR-0048) doesn't change the wire. The `Transform` Node variant is already in the descriptor; Phase 3 lights up its dispatch path. The `aether.dag.transforms` custom section is read at component load — orthogonal to the descriptor wire. Cross-version submitters work: a Phase 2 client submitting a descriptor without `Transform` nodes runs unchanged on Phase 3 substrates.

ADR-0045 Phase 4+ work (incremental recompute, distributed handle stores) lives behind the descriptor — executor sophistication, not wire surface. Future ADRs may add a `node_options` field to nodes for things like "pin this transform's output" or "memoize aggressively across restarts," but those are additive optional fields; the canonical-bytes machinery (ADR-0032) handles the encoding.

#### The `DagDescriptor.version` field

`DagDescriptor` carries a `version: u16`. Be precise about what it does and doesn't do.

It makes a *future* node-set or wire change **fail cleanly**: a substrate that doesn't recognise the descriptor version rejects the submit with a clear `DagError` rather than mis-decoding a `Node` enum it doesn't understand into garbage and dispatching it. A v1 substrate handed a v2 descriptor says "I don't speak version 2" instead of silently corrupting execution. That's the whole job — a guard rail on the decode boundary.

It does **not** make node-set changes non-breaking. Kind ids are schema-hashed (ADR-0030): `Kind::ID = fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))`. Adding, removing, or reshaping a `Node` variant changes the `DagDescriptor` schema, which changes the `aether.dag.submit` kind id — so any node-set change is a *new kind version* regardless of what the `version` field says. The field doesn't dodge that; it just turns the failure mode from "mis-decode" into "explicit reject" for any decoder that hasn't been taught the new shape.

The engine's established path for a post-1.0 breaking node-set change is a **sibling kind**: `aether.dag.submit.v2` coexists with `aether.dag.submit` (v1), each with its own schema-hashed id, and a substrate accepts whichever it implements. This is consistent with the kind-versioning philosophy elsewhere in the engine — kinds are immutable once their schema hashes, and a breaking change is a new sibling, not a mutation of the old. Adding `Call` *now* (pre-1.0, wire not yet frozen) is the cheap path: it reshapes v1 in place before anyone depends on the frozen bytes. The `version` field exists so the *next* such change — if it lands after the freeze — fails clean while the sibling kind carries the new shape.

#### Deferred future axes

Adding `Call` completes the position×effect grid (§2). It does **not** complete the design space of what a node *could* be. Three further axes are explicitly out of scope here and are each a **larger** change than adding an enum variant — they touch the `Edge` model, the acyclic assumption, or the one-handle-per-node output model, not just the `Node` enum:

- **Multiplicity (mux / demux / map-over-`Bundle`)** — now that `Bundle` is a first-class type, this axis has a concrete shape: **mux** (gather N edges → one `Bundle`), **demux** (route a `Bundle`'s elements to N downstream *edges*), and **scatter-gather / map-over-`Bundle`** (a `Bundle` of N → N parallel sub-pipelines → regather, N unknown at submit). Two clarifications keep it from looking like a hole. (a) *Fixed-arity mux and demux-by-kind are already expressible and are not on this axis*: a `Transform` that outputs a `Bundle` muxes; a `Transform` that consumes a `Bundle` and switches on element `KindId` demuxes-to-one — and that kind-switch is exactly the dispatch `#[handlers]` already codegens, so a future `#[demux]` sugar (per-kind handlers + `#[fallback]`, the same codegen pointed at bundle elements instead of an actor inbox) is an *SDK ergonomic*, not new DAG vocabulary. (b) *The genuine node-level gap is the dynamic part* — routing to N *edges* (multi-output) and runtime-determined fan-out/gather — which breaks the one-handle-per-node output model and the static edge count (`Edge` + the handle-allocation scheme would both need rework). **Both halves are deferred until a forcing function**: the sugar until bundle-consuming transforms are common enough that hand-writing the kind-switch hurts; the dynamic nodes until a dynamic-fan-out consumer forces them (e.g. `batch-gen` as a single DAG — today it stays incremental / fixed-fan). Keeping both out of 0.4 is deliberate scope control, and `Bundle` is the type they will operate on, so the groundwork is laid.
- **Control flow** — branch / conditional / loop nodes. This breaks the acyclic, static-shape assumption the validator's topological sort relies on (§3 phase 1) and the executor's "fire when inputs resolve" monotonic progress model (§4). A graph with a loop isn't a DAG.
- **Streaming** — a node that emits an *unbounded* stream of outputs over time rather than a single resolved handle. **Bounded multi-reply is no longer on this axis** — it's absorbed into `Call`, whose output is a settlement-closed ordered bundle (a multi-reply cap is just a multi-element bundle, §2). What remains deferred is *truly unbounded / never-settling* streaming: a producer that never settles, which a settlement-closed bundle can only ever *time out and fail* (it can't represent "an infinite stream you consume incrementally"). That would want a distinct **streaming handle** with incremental downstream flush semantics, breaking the "one resolution per handle" assumption ADR-0045 §3 builds on; in the meantime the `Call` timeout bounds such a producer into a node failure rather than buffering forever.

These are recorded deliberately so the next hole is a *decision*, not a surprise. They're to be done before the wire freezes (pre-1.0, reshaping the kind in place as `Call` does here) or, post-freeze, as sibling-kind versions (`aether.dag.submit.v2`, per above), consistent with the engine's kind-versioning philosophy.

## Consequences

### Positive

- **Pipeline declaration becomes a first-class operation.** A caller stops writing chained `#[handler]`s for graph-shaped work and instead hands a descriptor to the substrate. ADR-0046's content-gen pipeline lands here as the headline customer; render DAGs and asset-pipeline workflows pick this up next.
- **Validation up front.** A pipeline that's structurally bad fails on submit, not halfway through. Type errors, unknown mailboxes, missing recipients all surface synchronously with structured error variants.
- **Claude-in-harness composition.** The MCP `submit_dag` tool gives the harness a way to compose multi-step jobs without authoring a component. ADR-0008's observation path + this tool cover declarative-side and observation-side parity.
- **Wire foundation for Phase 3+.** The `Transform` variant is already in the descriptor; ADR-0048 lights up dispatch without changing wire format. Same property for ADR-0045's parked Phase 4+ work — node options, persistence hints, recomputation directives all add as optional fields.
- **Cancellation is real.** Long-running DAGs (image generation, long fetches) can be aborted from the harness side; in-flight calls complete server-side but the substrate stops paying attention.

### Negative

- **DAG state is one more substrate registry.** Adds `HashMap<DagId, DagState>` plus a reaping tick. Test scaffolding and lifecycle tracing grow commensurately.
- **In-flight cancellation isn't real.** Mailbox calls that have already dispatched complete on the remote side; cancel only stops the substrate from caring. A user who cancels because they noticed their fetch URL was wrong is still on the hook for the bandwidth. This is the same semantics as `wait_reply` and not novel — but it bears mention because DAG cancellation reads as "stop the work" and isn't.
- **Error variants are narrow at first.** Phase 2 `DagError` covers structural and dispatchability errors; runtime errors surface as `Failed` with a `String` message. Phase 3+ work likely wants a `Failed` reply with structured per-node error info, which means another wire revision (additive).
- **Validator complexity is moderate.** Cycle detection, type-compatibility cross-checks, mailbox/recipient lookup, and component-manifest cross-validation are all simple individually; ordering them so error messages are useful (rather than "first failure wins, regardless of which is the real problem") takes care.

### Neutral

- **Existing kinds dispatch unchanged.** A DAG source for `aether.fs.read` builds a normal `Read` request and forwards it to the `aether.fs` mailbox; the cap doesn't know it came from a DAG. The `aether.fs` (ADR-0041), `aether.http` / `aether.tcp` (ADR-0043), and `aether.audio` (ADR-0039) cap contracts hold.
- **Actor-per-component scheduling unchanged.** ADR-0038 keeps its semantics — the DAG executor is a substrate-level orchestrator, not a scheduling layer; transform / observer dispatches funnel through the same per-component mpsc queues as any other mail.
- **Hub mail bubbling unchanged.** ADR-0037 keeps its semantics — the hub doesn't host DAGs, it forwards mail to substrates.

## Alternatives considered

- **Push-based status (server-streamed).** Rejected for v1: the harness already polls `engine_logs` and receive_mail on its own cadence; adding a push channel is design surface that doesn't pay off until DAGs are long-lived enough that polling becomes wasteful. Forward-compatible: a future ADR can add a `subscribe_dag_status` kind without changing the descriptor or other reply kinds.
- **Compile transforms into the descriptor as wasm bytecode.** Rejected for the obvious reason: descriptors would explode in size and the substrate would need to instantiate ad-hoc components. Phase 3's `transform_id = (component_mailbox, transform_index)` keeps transforms tied to loaded components; the descriptor only references them.
- **Inline observer kinds (carry the observer's kind value verbatim, no recipient).** Rejected: a DAG that "observes" by attaching a kind value to the executor and sending it nowhere is weird. The observer-as-mail-dispatch shape composes with the existing mailbox / handler infrastructure (ADR-0033). A DAG that needs to fan out to multiple recipients wires multiple observer nodes — one per recipient.
- **Synchronous submit (block until DAG completes).** Rejected: long-running DAGs (image gen at 60s/image, fetch chains) would block the submitting actor. Async submit with `dag_id` + poll/wait separation matches the rest of the substrate's mail-shaped surface. A caller that *wants* synchronous semantics composes them on top: submit, then `wait_reply` for an observer-emitted "DAG complete" mail.
- **DAG submit as a host-fn instead of a mail kind.** Rejected: ADR-0002 keeps the privileged FFI surface small. Mail-shaped submit gives Claude observability for free, plays nicely with capability gating (ADR-0044), and follows the same pattern as ADR-0041's `aether.fs` cap and ADR-0043's `aether.http` / `aether.tcp` caps.
- **Allow nested DAGs (a node's payload is itself a `DagDescriptor`).** Rejected for v1: composability via observers (DAG A's observer is a component method that submits DAG B) is sufficient and keeps the executor's invariants simple. Nesting can come back as a future ADR if recipe ergonomics demand it.
- **Ship Phase 2 + Phase 3 as one ADR.** Tempting because both reference the same descriptor format. Rejected: transforms add a guest-side macro, custom section, and wasmtime integration — separate engineering surface deserving separate review. Phase 2 alone is shippable and useful (sources + observers cover the "declare a pipeline made of mailbox calls + dispatch the result somewhere" workflow).

## Follow-up work

- **PR**: kinds + schema-derive — `DagDescriptor` and the three reply types in `aether-kinds`, with `#[derive(Schema)]` covering the enum variants. New `NodeId` / `TransformRef` types as wire-stable aliases.
- **PR**: substrate executor — `DagState`, validation phases, source dispatch + parked-mail integration, observer dispatch, reaping tick, cancellation. Integration tests covering each failure path of `DagError` and the happy-path source → observer flow.
- **PR**: hub MCP surface — `submit_dag`, `dag_status`, `dag_cancel` tools wrapping the three mail kinds.
- **`Call` node + `version` field (iamacoffeepot/aether#1017).** The implementing work lands in the existing DAG issues: the `Node` / `DagDescriptor` wire change (the `Call` variant + `version: u16`; the `Call` variant has no `output_kind_id`) in iamacoffeepot/aether#974, the validator arm (dispatchability + bidirectional edge type-checking for `Call`, with the output typed as a self-describing `Bundle` (heterogeneous, no declared element kind); consumers accept a `Bundle`) in iamacoffeepot/aether#975, and the executor arm in iamacoffeepot/aether#976: resolve inputs → dispatch to the cap mailbox **as its own causal root** via the inherited `send` path (with `spawn_inherit` for any cap-spawned worker) → collect correlated replies in arrival order → close the bundle on `Settled { call_root }` → resolve the output handle to the ordered bundle, with source-reply timeout/cancellation parity bounding a never-settling producer into a node failure.
- **Reliable settlement (iamacoffeepot/aether#1031) — hard prerequisite for iamacoffeepot/aether#976.** The bundle-`Call` executor cannot close its bundle on the hint-grade `Settled` of ADR-0080 §6 (an early fire drops late replies into a single-assignment handle). It requires ADR-0080 §6's deferred "one-batch quiescence window" (fire `Settled { root }` only after `counter[root]` has been zero for one full batch interval). This is the first consumer that needs *reliable* rather than hint-grade settlement.
- **Parked, ADR-0048**: transforms (Phase 3) — `#[transform]` macro, `aether.dag.transforms` custom section, wasmtime `Func::call` integration, content-addressed transform handle ids, persistent handle store.
- **Parked, future ADR**: structured per-node error info on `Failed` (replacing `error: String`).
- **Parked, future ADR**: server-streamed status / observer-pushed completion notification, if polling becomes the bottleneck.
- **Parked, future ADR**: nested DAGs (`Source { kind_id: aether.dag.submit }`) if recipe ergonomics demand it.
