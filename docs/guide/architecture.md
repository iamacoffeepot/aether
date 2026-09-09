# Architecture overview

Aether separates portable actor behavior from native resource ownership and
out-of-process operation.

```text
operator (agent, human, test, client)
├─ MCP or framed RPC → hub fleet control plane
│                         ├─ engine proxy → desktop/headless child
│                         └─ binary select/fork ──────────────┐
└─ an application's own ingress ──────────────────────────────┤
   (REST or typed RPC, e.g. Bloomery's)                       ▼
                                             application chassis + stores

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
per-engine operation names an `engine_id`. An application built on the engine
may add an ingress of its own: Bloomery, the first-party development control
plane, has its own chassis, stores, and REST/typed-RPC surface, and can run
standalone or be uploaded, selected, and forked through the hub's binary/fleet
path. Such a chassis does not itself own the hub's `FleetServer`.

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
| Process profiles | `aether-chassis` + `aether-chassis-*` | desktop/headless/hub/harness composition; the shippable package depot comes from `cargo xtask package` |
| Operator bridge | `aether-mcp` | live tools, JSON/schema adaptation, hub RPC and caches |
| Test harnesses | `aether-harness-*` | in-process substrate, real-process fleet, capture, and perf drivers |
| Build tooling | `xtask`, fixtures, `fuzz/` | artifact discovery, package depots, compatibility fixtures, fuzz targets |
| Guest actors shipped with the engine | `aether-kit-commons`, `aether-kit-widget`, `aether-mesh`, `aether-puppet` | camera and camera-controller, console overlay, mesh viewer, the widget set, geometry authoring, mascot rendering |
| Applications built on it | `aether-bloomery*`, `aether-chassis-bloomery` | Bloomery's bounded development state/reduction, git and GitHub adapters, console, and host process |

The [repository map](orientation/repository-map.md) routes changes across the
full workspace. Capability messages such as render/audio/filesystem kinds live
with their own capability crate, not in a universal central kind catalog
(ADR-0121).

## Chassis composition

Five checked-in chassis profiles reuse the substrate but install different
drivers and capabilities: desktop, headless, hub, and substrate harness are the
engine's own; Bloomery's is an application profile built the same way. It can
run directly or through the fleet launch path; the generic hub and headless
profiles do not absorb its development services or become build servers.

Source presence does not imply every chassis has a working actor. Some
unsupported surfaces deliberately install a fail-fast fallback so requests
resolve with errors rather than hang.

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
