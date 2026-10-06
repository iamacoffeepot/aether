# Inventory, descriptors, and transforms

`aether.inventory` lets an out-of-process observer discover the selected
engine's names and receive contracts. It exists because a static copy compiled
into the MCP process would drift from a different chassis build or from kinds
registered by newly loaded components.

## Six inventory questions

| Request | Answers |
|---|---|
| `aether.inventory.manifest` | link-time names, kinds, transforms, and instanced-family templates |
| `aether.inventory.resolve` | reverse name for a dynamically minted engine-local id |
| `aether.inventory.resolve_address` | the canonical path of the live actor a canonical or short address names |
| `aether.inventory.kinds` | every kind/schema currently registered in this engine |
| `aether.inventory.handlers` | native handler input and optional reply contracts by namespace |
| `aether.inventory.memory` | the bytes the engine holds: the whole process, the blob store, and one row per owner |

The MCP tools project the selected engine's kind and native-handler views through
`describe_kinds` and `describe_handlers`. `describe_component` resolves a
lineage name against the engine when needed. `describe_transforms` is a separate
static view of the transform set linked into `aether-mcp`; it has no engine
argument and can differ from another substrate binary's manifest.

## Static names and dynamic instances

The manifest is not a flat list of every possible mailbox. It carries direct
name entries plus family templates:

- bounded families can be expanded over a known numeric range;
- declared families have a known domain;
- dynamic families require runtime resolution for minted instances.

The client folds static/template data into a reverse map and queries `Resolve`
only when it cannot derive a dynamic name. On a miss it can still render a
tagged id rather than inventing a name.

Names are diagnostic and addressing aids. The hashed ids remain the wire
identity. Do not persist a reverse-name cache across unrelated engine lifetimes.

## Live kind registry

The inventory actor holds the same shared `Registry` the component host updates.
After a component load registers kinds, `ListKinds` sees them without a separate
event or cache invalidation channel. The MCP encoder refreshes its per-engine
kind cache on a name miss so named JSON mail can target component-defined
schemas.

The public `describe_kinds` projection is less strict than the inventory mail:
it silently keeps a static/prior cache if its live refresh fails. Treat its
schemas as exact for the returned snapshot, not as a liveness result; pair a
freshness-sensitive lookup with a fleet read and harmless live request.

This is why the safe operator loop is:

```text
load component
  → inspect its handlers / refresh live kinds
  → encode named mail against that engine
```

Do not promote a component kind into `aether-kinds` merely so an older static
client can encode it. Fix discovery or the client cache.

## Handler inventory

Native `#[handler]` code generation submits link-time entries containing actor
namespace, input kind, and reply kind where one exists. `describe_handlers`
makes native capabilities as inspectable as loaded wasm actors.

The inventory describes accepted receive contracts; it does not prove a
particular external resource initialized successfully. For that, combine it
with logs and a bounded request.

## Memory by owner

`aether.inventory.memory` takes no fields and answers with
`aether.inventory.memory_result`. A debug overlay asks about once a second:
building the reply takes one lock and one platform read, so it is not a
per-frame question, and mail dispatch pays nothing for it.

| Field | What it counts |
|---|---|
| `process_bytes` | the whole process's resident set size, read when the reply is built; absent on a platform with no reader (Linux and macOS have one) |
| `blob_store.resident_bytes` | every owned blob entry's bytes plus every live slab, each counted once |
| `blob_store.slab_bytes` | every live slab's bytes |
| `blob_store.slab_member_bytes` | every live slab entry's bytes |
| `owners` | one `{ owner, label, bytes }` row per owner and label, sorted by owner then label |

Slab bytes are inside resident bytes, and slab member bytes are inside slab
bytes, so the three blob store numbers are never summed. Their differences are
the useful part: `slab_bytes - slab_member_bytes` is what slabs still retain for
entries that were already dropped.

An owner is named by its actor path, or by its tagged id text when the registry
holds no name for it. The rows today:

- `linear memory`, one row per live wasm component instance: the size of its
  wasm linear memory. A component and its inline children share one memory and
  so one row. A linear memory grows and never shrinks, so the row falls only
  when the instance drops, and it leaves with it. While a republish prepares a
  replacement, the old and the new instance are one row with their sum.
- `textures` on `aether.render`: the declared pixel bytes of every texture,
  texture array (every layer, written or not), and volume that mail created,
  sampled or writable. This is what each occupies once realized on the device.
- `geometry` on `aether.render`: the vertex plus index bytes of every geometry
  that mail created, including one destroyed while a draw set still holds it,
  until the last set lets go.

Each owner writes its own bytes as they change (a relaxed atomic add, subtract,
or store), and the request reads them; nothing is sampled or walked.

The rows do not sum to `process_bytes`. Not counted by any row: the engine's
own heap, render targets, pipelines, per-frame vertex buffers, instance
buffers, and audio. The process number is the only one that covers those, and
device memory may sit outside it. A render row counts the bytes a resource
occupies on the device; its staged copy is a blob, which the blob store counts.

An engine composed without `aether.inventory` has no one to ask.

## Native transforms

Transforms are link-time registered, named value operations. They are useful
for bounded conversions/folds that do not own actor state. `describe_transforms`
discovers the set compiled into the current MCP process, not a live chassis.

A transform is not:

- a general host function callable by arbitrary address;
- a substitute for stateful capability mail;
- evidence that every input is safe or cheap;
- a way around filesystem/network policy.

For example, `aether.fs.fetch` validates an `addr` and can fold the bytes
through a registered transform. The capability still owns trusted file access;
the transform owns only the value conversion.

## Cache and debugging rules

- Scope reverse-name and kind caches by engine id.
- Refresh on a live encode miss; do not assume component sets are static.
- If a name resolves but mail fails, distinguish missing recipient instance from
  unknown kind/schema.
- If code and the engine-scoped `describe_*` tools disagree, confirm you are
  querying the expected engine binary and component set. For
  `describe_transforms`, confirm the `aether-mcp` build instead.
- Use `full` or broad descriptor output selectively; bounded queries reduce
  context and response spill.

## Change route

- Capability: `crates/aether-inventory/src/`
- Shared inventory kinds: `crates/aether-kinds/src/lib.rs`
- Link-time entries and canonical ids: `crates/aether-data/src/`
- Registry: `crates/aether-substrate/src/mail/registry/`
- MCP caches/projection: `crates/aether-mcp/src/{reverse.rs,tools/describe.rs}`
- Decisions: ADR-0064, ADR-0088, ADR-0091, ADR-0109, ADR-0121
