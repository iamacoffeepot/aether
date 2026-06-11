# The computation DAG

> **Governing ADRs:** [ADR-0045](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0045-computation-dag-and-typed-handles.md)
> (computation as a graph over typed handles, the founding decision),
> [ADR-0047](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0047-dag-submit-cancel-status.md)
> (the descriptor, validator, and executor),
> [ADR-0048](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0048-transforms-and-content-addressed-handles.md)
> (native transforms and content-addressed outputs). The descriptor wire shape and
> the `submit` / `status` / `cancel` surface are **stable**. Like the
> [handle store](handles.md) it rides on, the machinery is covered by tests but
> **lightly exercised** in practice — its heaviest intended consumer, the
> content-generation pipeline ([ADR-0084](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0084-plato-content-generation-on-the-dag.md)),
> isn't built yet. When in doubt, the ADRs are authoritative.

A computation DAG hands the substrate a multi-step job — fetch this, run that
over it, deliver the result there — as one declarative submission instead of a
chain of hand-orchestrated mail. You describe the work as a graph: each node
names something the substrate already knows how to run, each edge says whose
output feeds whose input, and every intermediate value travels as a
[handle](handles.md) rather than as bytes through anything that merely
forwards it. The substrate validates the whole graph up front, executes it
asynchronously, and parks each consumer until the handles feeding it exist.

You drive it over MCP with three tools: `submit_dag` to hand a descriptor in,
`dag_status` to poll, `dag_cancel` to stop one in flight.

## Why it exists

Multi-step async work composes badly inside a single handler. A handler runs to
completion and [promises nothing about replies](mail-and-kinds.md), so an actor
that wants fetch → transform → deliver has to hold correlation state across
turns, match each reply back to the step that asked, and re-implement timeout
and failure policy at every hop — orchestration boilerplate that grows with the
graph, written inside the component least equipped to observe it.

The DAG moves that orchestration into the substrate, where the pieces already
live. The handle store gives intermediate values a place to wait; settlement
([Tracing & settlement](tracing-and-settlement.md)) already knows when a
dispatched chain has finished; the parking table already holds mail whose
referenced handle hasn't been published. A descriptor names the steps and the
data flow, and the substrate does what it would have coached every component
into doing by hand — with one validator deciding up front whether the whole
graph can run, instead of each step discovering its own failure mid-flight.

Splitting the cheap verdict from the expensive work is the surface's design
point: `submit_dag` returns the validation verdict synchronously, so a caller —
an agent assembling a descriptor from tool output, especially — learns about a
bad graph in one round trip, while execution proceeds asynchronously and is
polled.

## What it does

**A descriptor is nodes plus edges.** Four node variants cover the graph:

| Node | Position | Effectful? | What it does |
|---|---|---|---|
| `Source` | root | yes | dispatches its `payload` to a mailbox as `kind_id`; the reply feeds downstream |
| `Transform` | mid-graph | no | runs a registered native `#[transform]` fn; declares its `output_kind_id` |
| `Call` | mid-graph | yes | assembles a request from its incoming edges, dispatches it, and accumulates the correlated replies into a `Bundle` that closes on settlement |
| `Observer` | terminal | yes | assembles `kind_id` from its incoming edges and dispatches it to a recipient |

Sources have no incoming edges; observers have no outgoing ones. A `Call`
produces a `Bundle` — its replies are heterogeneous and self-describing, so it
declares no output kind of its own.

**Edges fill `Ref<K>` slots.** An edge `{ from, to, slot }` says: the output
handle of node `from` fills slot `slot` of node `to`, where slots index the
consumer kind's `Ref<K>` fields in declaration order. At dispatch the substrate
assembles the consumer's request with a wire `Ref::Handle` in each slot and the
[handle resolution walk](handles.md) splices the stored bytes inline — or parks
the mail until the handle is published. This is why a kind with `Ref<K>` fields
is automatically a DAG consumer: the slot surface *is* its field list.

**Validation is synchronous; execution is asynchronous.** Submit runs a version
gate and then three phases — structural integrity (unique node ids, acyclic,
sources/observers in legal positions, size caps), dispatchability (every
mailbox exists and accepts the kind named at it, every transform is
registered), and type compatibility (each edge's producer output canonically
matches the consumer slot's declared `K`). The reply is `{ dag_id,
output_handles }` — the per-node handle assignment, minted at submit — or the
first structured `DagError`, in one round trip. Only after the ack do sources
dispatch. Size caps are env-tunable: `AETHER_DAG_MAX_NODES` (256),
`AETHER_DAG_MAX_EDGES` (1024), `AETHER_DAG_MAX_DESCRIPTOR_BYTES` (1 MiB).

**Transforms are pure, content-addressed, and cached.** A transform node names
a native fn registered with `#[transform]` (at most 8 inputs, ADR-0048). Its
output handle id derives from the transform's identity plus its input ids, so
the same transform over the same inputs lands on the same id — and on a cache
hit the executor skips the invocation entirely and serves the stored value.
Transforms run off-thread on a compute pool (`AETHER_TRANSFORM_POOL_THREADS`),
each under a wall-clock deadline (`AETHER_TRANSFORM_TIMEOUT_MS`, default 30 s,
per-node override in the descriptor) and an output size cap
(`AETHER_TRANSFORM_MAX_OUTPUT_BYTES`, default 64 MiB).

**A domain `Err` is a successful output.** Node state is `Pending` /
`Resolved` / `Failed`, and `Failed` means the *machinery* failed — a timeout, a
malformed reply, a dispatch error. A step whose reply is its kind's own `Err`
value has *resolved*: the error is a value, content-addressed like any other,
and downstream nodes consume it as one. There is no value-conditional
execution — no edge that fires only when a check passes. Gating an expensive
`Call` on a cheap check is two submissions: submit the check DAG, poll its
result, and conditionally submit the generation DAG.

**Status, cancellation, and reaping.** `dag_status` reports `Pending` (nothing
resolved yet), `Running` with a per-node progress list, `Complete` with the
same `(node, handle)` pairs the submit returned, or `Failed` naming the node
and a reason. `dag_cancel` stops an in-flight DAG; it surfaces as `Failed` with
`"cancelled"` as the reason. A `Call` that never settles is bounded by
`AETHER_DAG_CALL_TIMEOUT_MS` (default 30 s). Terminal DAGs are reaped after a
retention window — `AETHER_DAG_RETENTION_COMPLETE_MS` (60 s) /
`AETHER_DAG_RETENTION_FAILED_MS` (5 min) — after which `dag_status` no longer
knows the id; the output *handles* live on in the store under its own policy.

## How to use it

**From an agent over MCP.** Build the descriptor from what the engine tells
you: `describe_kinds` / `describe_component` give the kinds and their `Ref<K>`
slots, `describe_transforms` gives every registered transform with its input
and output kind ids. A minimal fetch-transform-deliver graph:

```json
{
  "version": 1,
  "nodes": [
    { "Source":    { "id": 1, "mailbox": "...", "kind_id": "...", "payload_path": "/tmp/req.bin" } },
    { "Transform": { "id": 2, "transform_id": "...", "output_kind_id": "..." } },
    { "Observer":  { "id": 3, "recipient": "...", "kind_id": "..." } }
  ],
  "edges": [
    { "from": 1, "to": 2, "slot": 0 },
    { "from": 2, "to": 3, "slot": 0 }
  ]
}
```

`payload_path` is a tool-layer convenience: the wire `Source` carries inline
payload bytes, but byte arrays don't belong in tool JSON, so the tool takes a
file path and reads it into the wire field. Submit, then poll `dag_status`
until `Complete` or `Failed`; inspect what landed in the store with
`describe_handles`. Ids in tool replies are tagged strings (`dag-…`, `hdl-…`,
`knd-…`) — opaque tokens you hand back verbatim.

**From a component or capability.** The mail surface behind the tools is
`aether.dag.{submit,status,cancel}` on the `aether.dag` mailbox, and consuming
a DAG's output takes no DAG-specific code at all: an observer node dispatches
an ordinary kind to an ordinary mailbox, with its `Ref<K>` fields resolved
before the handler runs. A component that handles the kind is a valid DAG
terminal today.

## How to extend or reuse it

The seam is the transform registry. A new pure step is a free function under
`#[transform]`:

```rust
#[transform]
fn mat4_apply(input: Mat4Apply) -> Vec4 {
    input.matrix * input.vector
}
```

The macro registers it in a link-time inventory — the chassis binaries and
`describe_transforms` pick it up with no extra wiring — and rejects, at compile
time, bodies that reach for what a pure function can't have: host fns, handler
context, `std::env`, `std::time`. The deny-list is a tripwire rather than a
proof, which is acceptable because transforms are first-party reviewed code.
Keep a transform unary where you can (`Mat4Apply` bundles both operands into
one kind) — it keeps the node's slot surface trivial.

Effectful steps need no registration at all: any mailbox that accepts a kind
is already a valid `Source` or `Call` target, and any kind with `Ref<K>`
fields is already a fillable consumer. Growing the DAG vocabulary is mostly
growing the kind vocabulary — see
[Adding a substrate kind](../recipes/adding-a-substrate-kind.md).

## Where to read more

- The founding decision and the handle model it rides on —
  [ADR-0045](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0045-computation-dag-and-typed-handles.md);
  descriptor, validator phases, executor, cancellation, reaping —
  [ADR-0047](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0047-dag-submit-cancel-status.md);
  the `#[transform]` macro, the purity deny-list, and content addressing —
  [ADR-0048](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0048-transforms-and-content-addressed-handles.md).
- The store DAG outputs land in, and the `Ref<K>` wire type edges fill —
  [Handles](handles.md).
- Why a `Call` closes its `Bundle` on settlement —
  [Tracing & settlement](tracing-and-settlement.md).
- The `submit_dag` / `dag_status` / `dag_cancel` tools in context —
  [The MCP harness](../mcp-harness.md).
