# Process topology and chassis

Aether uses the word *engine* for an actor runtime, not for every process in the
operator stack. A normal agent-driven session has several boundaries:

```text
agent or human client
        │ MCP
        ▼
stable tunnel endpoint
        │ stdio/HTTP transport
        ▼
aether-mcp coordinator
        │ framed Aether RPC
        ▼
hub chassis ───── content-addressed binary/component stores
        │
        ├── engine proxy ── child substrate: headless chassis
        ├── engine proxy ── child substrate: desktop chassis
        └── …
```

The tunnel keeps the client-facing MCP connection stable. `aether-mcp`
translates tool-shaped JSON into typed Aether mail and maintains operator-side
caches. The hub owns fleet supervision, stored artifacts, and routing identity:
it mints each process-local `engine_id` and assigns it to the child proxy. Each
child substrate owns an independent registry, scheduler, actor set, and live
runtime state.

## The four chassis profiles

The `aether-chassis-*` crates assemble the shared runtime into purpose-specific
profiles. The exact capability set is code and feature dependent, so treat this
table as intent rather than a hardcoded manifest.

| Profile | Entry binary | Primary job |
|---|---|---|
| Desktop | `aether-substrate` | window, GPU/input/audio integration and interactive frames |
| Headless | `aether-headless` | timer-driven engine without a desktop event loop |
| Hub | `aether-hub` | supervise child engines, store artifacts, and route RPC |
| Substrate harness | `aether-harness-substrate` | deterministic in-process operations and test evidence |

Their builders live under
`crates/aether-chassis-{desktop,headless,hub,harness}`. Shared
runtime mechanism remains in `aether-substrate`; each shared native actor
remains in its own `aether-<capability>` crate.

“Headless” is a process/profile statement, not permission to assume every
capability is absent. Some capabilities have explicit headless implementations
or unsupported marker actors so callers get a deterministic reply instead of a
missing mailbox. Check the chassis builder and live handlers for the exact
engine.

## What the hub owns

The hub's `FleetServer` is the fleet control plane. It owns:

- spawn and termination requests;
- the live proxy set and heartbeat state;
- recently departed engine records;
- binary and component artifact stores;
- selector resolution, materialization, persistence, and eviction policy;
- routing a per-engine RPC call to the right proxy.

An `FleetProxy` represents one connected child. It is not the child's actor
registry mirrored in full. Mail, replies, inventory queries, and lifecycle
events still cross the RPC boundary.

This is why `engine_id` must be carried explicitly by operator tools: mailbox
lineage is only meaningful within the selected engine.

## What a child substrate owns

Every engine has its own:

- mailbox registry and lineage namespace;
- mail rings and scheduler state;
- native capability instances selected by its chassis;
- wasm component instances and trampoline state;
- lifecycle/settlement graph and per-actor evidence;
- configuration overlay resolved at boot.

Terminating a child destroys that live state. Uploaded artifacts live in the
hub store and can outlive one engine; loaded component instances do not.

## One tool call end to end

A per-engine MCP call crosses two distinct translations:

```text
tool JSON
  → textual recipient resolution through selected engine inventory
  → engine returns live mailbox id + canonical path
  → schema-aware encode in aether-mcp
  → WireFrame / MailEnvelope over hub RPC
  → proxy selects engine
  → child registry resolves recipient
  → scheduler dispatches typed bytes to an actor
  → reply and settlement events travel back
  → tool projects bounded JSON/evidence
```

Failures at these layers look different. A schema error happens before mail is
sent. An unknown engine fails at the fleet boundary. An unknown recipient fails
inside the selected engine during address resolution. Canonical lineage and
ADR-0166 abbreviated spellings share that engine-owned seam; the MCP
coordinator keeps no alias table and never hashes operator paths. A handler
error or non-settling descendant happens
after dispatch. Start diagnosis at the earliest layer supported by evidence;
the [recovery runbook](../operating/recovery.md) is organized that way.

## Components at boot versus after boot

The hub can stage stored component bytes and a JSON-derived config into a boot
manifest for a new substrate. The child loads those components during startup
and readiness waits for the requested instances. Alternatively, operator tools
can load or replace components after the engine is live.

Both paths resolve registry selectors. Neither treats an arbitrary local wasm
path as a component identity. Upload first, select second. See
[Component registry and replacement](../operating/component-registry.md).

## Distribution is a different topology

`cargo xtask package` produces a shippable package depot: a selected desktop or
headless chassis binary alongside a content-addressed `pack/` of the chosen
components and their config. It is not the same thing as the development hub
fleet. The depot's autoload path builds the component set at process boot
without requiring an MCP coordinator or hub artifact store.

Read [Distribution and packaging](../building/distribution.md) before changing
autoload or packaging behavior.

## Implementation and decisions

- Shared chassis traits and frame loop: `crates/aether-substrate/src/chassis/`
- Process composition: `crates/aether-chassis/src/` + the per-chassis crates
- Fleet, proxy, and stores: `crates/aether-fleet/src/`
- Framed RPC: `crates/aether-rpc/src/`
- MCP translation: `crates/aether-mcp/src/`
- ADR-0034 and ADR-0073: chassis and bundle structure
- ADR-0074: MCP/RPC control path
- ADR-0089: stable tunnel boundary
- ADR-0115 and ADR-0116: binary and component registries
