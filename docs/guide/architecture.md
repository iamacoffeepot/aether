# Architecture overview

Aether separates portable actor behavior from native resource ownership and
out-of-process operation.

```text
operator (agent, human, test, client)
├─ MCP or framed RPC → hub fleet control plane
│                         ├─ engine proxy → desktop/headless child
│                         └─ binary select/fork ──────────────┐
└─ REST or typed RPC ─────────────────────────────────────────┤
                                                            ▼
                                             Bloomery application
                                             chassis + stores

Each hosted process composes the shared runtime layers:
 registry → mail rings → scheduler → actor handlers
                              ├─ native state
                              └─ wasm state
 lifecycle + settlement + logs/traces/cost evidence
```

## The boundaries

**Operator boundary.** `aether-mcp` adapts task-shaped JSON tools to the same
typed mail/RPC contracts other clients can use. A stable tunnel can preserve an
MCP session while volatile backends restart. The hub supervises a fleet; every
per-engine operation names an `engine_id`. Bloomery is a dedicated first-party
development control-plane application with its own chassis, stores, and
operator ingress. It can run standalone or be uploaded, selected, and forked
through the hub's binary/fleet path; its chassis does not itself own the hub's
`FleetServer`.

**Process boundary.** Framed RPC carries control calls and mail between the hub
and child substrates. The hub owns artifact stores and proxy/heartbeat state.
Each child owns its own registry and runtime state.

**Actor boundary.** A mailbox selects an actor instance; a kind selects the
message schema. Native and wasm actors receive through the same scheduler and
reply/settlement model.

**Privilege boundary.** Native capabilities own files, sockets, windows,
GPU/audio devices, credentials, and process control. Guest code asks them to do
bounded work by mail; it does not receive raw handles.

Read [Process topology and chassis](architecture/process-topology.md) and
[Guest, native, and wire boundaries](architecture/guest-native-boundary.md) for
the detailed models.

## One mail operation

1. A caller chooses an engine, recipient mailbox name, and kind name.
2. At a JSON boundary, the descriptor/schema encodes parameters into canonical
   wire bytes.
3. The hub/proxy routes the envelope to the selected child engine.
4. The registry resolves the recipient and kind; the scheduler queues work.
5. Generated or manual dispatch decodes the value and invokes one handler.
6. Handler mail inherits causal lineage unless deliberately detached.
7. Replies return to the caller; settlement completes when every tracked
   descendant and explicit hold resolves.
8. Logs, traces, cost tables, and captures provide evidence at their respective
   layers.

Mail is fire-and-forget by default at the actor API. Reply classes make a reply
contract explicit; an operator tool may additionally wait for settlement and
project replies.

## Layer map

| Layer | Main crates | Responsibility |
|---|---|---|
| Data/wire | `aether-data`, `aether-codec`, `aether-math`, `aether-kinds` | ids, schemas, canonical encoding, framing, shared vocabulary |
| Guest SDK | `aether-actor`, `aether-behavior` and derive crates | actor/behavior authoring, exports, contexts, replies |
| Runtime | `aether-substrate` | registry, mail, scheduler, native/wasm host, settlement |
| Native services | one `aether-<capability>` crate per cap | chassis resource actors and public capability kinds |
| Process profiles | `aether-chassis` + `aether-chassis-*` | desktop/headless/hub/harness composition plus the dedicated Bloomery application chassis; the shippable package depot comes from `cargo xtask package` |
| Development control plane | `aether-bloomery`, `aether-bloomery-github`, `aether-chassis-bloomery` | bounded development state/reduction, GitHub projection and source adapter, and the Bloomery host process |
| Product actors | `aether-kit-*`, `aether-mesh`, `aether-puppet` | camera, UI, world/terrain, sim, geometry authoring, mascot rendering |
| Operator bridge | `aether-mcp` | live tools, JSON/schema adaptation, hub RPC and caches |
| Build/test tooling | `xtask`, fixtures, `fuzz/` | artifact discovery, bundles, compatibility fixtures, fuzz targets |

The [repository map](orientation/repository-map.md) routes changes across the
full workspace. Capability messages such as render/audio/filesystem kinds live
with their own capability crate, not in a universal central kind catalog
(ADR-0121).

## Chassis composition

Five checked-in chassis profiles reuse the substrate but install different
drivers and capabilities: desktop, headless, hub, substrate harness, and
Bloomery. Bloomery's dedicated application chassis can run directly or through
the fleet launch path; the generic hub and headless profiles do not absorb its
development services or become build servers. Source presence does not imply
every chassis has a working actor. Some unsupported surfaces deliberately
install a fail-fast fallback so requests resolve with errors rather than hang.

Ask the live engine with `describe_handlers`/`describe_kinds`, or inspect the
specific builder in its `aether-chassis-<chassis>` crate.

## Architecture change discipline

When changing a boundary, trace all owners:

- public kind and schema;
- marker/runtime feature split;
- native or wasm handler;
- chassis installation and config;
- MCP/client projection if task-shaped access exists;
- unit, SubstrateHarness, and process tests at the appropriate boundary;
- accepted ADR and any amendments/supersession.

Use [Choose the owning extension point](building/extension-points.md) before
adding a new layer.
