# Inventory, descriptors, and transforms

`aether.inventory` lets an out-of-process observer discover the selected
engine's names and receive contracts. It exists because a static copy compiled
into the MCP process would drift from a different chassis build or from kinds
registered by newly loaded components.

## Four inventory questions

| Request | Answers |
|---|---|
| `aether.inventory.manifest` | link-time names, kinds, transforms, and instanced-family templates |
| `aether.inventory.resolve` | reverse name for a dynamically minted engine-local id |
| `aether.inventory.kinds` | every kind/schema currently registered in this engine |
| `aether.inventory.handlers` | native handler input and optional reply contracts by namespace |

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

## Native transforms

Transforms are link-time registered, named value operations. They are useful
for bounded conversions/folds that do not own actor state. `describe_transforms`
discovers the set compiled into the current MCP process, not a live chassis.

A transform is not:

- a general host function callable by arbitrary address;
- a substitute for stateful capability mail;
- evidence that every input is safe or cheap;
- a way around filesystem/network policy.

For example, `aether.fs.fetch` validates a namespace/path and can fold the bytes
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

- Capability: `crates/aether-capabilities/src/inventory/`
- Shared inventory kinds: `crates/aether-kinds/src/lib.rs`
- Link-time entries and canonical ids: `crates/aether-data/src/`
- Registry: `crates/aether-substrate/src/mail/registry/`
- MCP caches/projection: `crates/aether-mcp/src/{reverse.rs,tools/describe.rs}`
- Decisions: ADR-0064, ADR-0088, ADR-0091, ADR-0109, ADR-0121
