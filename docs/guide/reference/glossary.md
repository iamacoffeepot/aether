# Glossary

This glossary defines terms as Aether uses them. It is intentionally more
precise than their generic software meanings.

## Runtime and identity

**Actor**

A state owner that receives typed mail serially through a mailbox. An actor may
be native or hosted in wasm.

**Mailbox**

The addressable inbox of one actor instance. A mailbox name identifies the
recipient; it is not a message kind.

**Mailbox lineage**

The hierarchical name of an actor within one engine, including component and
inline-child/instance placement. The same lineage may exist in another engine.

**Kind**

A canonical message contract: stable name/id plus a schema and encoding.

**Schema**

The portable structural description used to encode/decode values at boundaries,
including MCP JSON adaptation.

**Canonical id**

The deterministic typed id derived from the contract's canonical name/shape as
specified by the data layer. Tagged rendering is diagnostic; the integer is the
wire identity.

**Mail**

One addressed kind payload plus source/correlation/lineage metadata.

**Chain root**

The mail id whose causal descendants form one settlement tree.

**Settlement**

The state in which all tracked descendants and explicit holds for a chain root
have completed. It is stronger than “the first handler returned.”

**Hold**

An explicit promise that asynchronous work still belongs to a settlement chain
and will later resolve or release it.

**Detached mail**

Mail started as a new causal root rather than a descendant of the current
handler. Long-lived stream chunks use short detached chains.

**Registry**

Engine-local mappings for mailboxes, kinds, handlers, and related runtime
metadata. Do not confuse it with the hub's artifact stores.

## Hosted and native code

**Guest**

Code hosted by the substrate, normally a wasm component or behavior script.

**Component**

A deployable wasm module exporting one or more actor identities. The module is
stored/selected as an artifact; loading creates actor instances.

**Export**

One actor identity declared by a multi-actor wasm module. Export order does not
implicitly choose the default.

**Default actor**

The sole actor in a single-actor module, or the explicit multi-actor default
selected with `export!(default = …)`. A defaultless multi-actor module requires a
selector naming an export.

**Trampoline**

The native/wasm host actor machinery that routes mail into one loaded component
instance and supports replacement/state transfer.

**Inline actor / cluster**

Actors co-located in one wasm instance and composed below a component root. They
still have distinct mailbox lineage.

**Behavior**

A small wasm filter interpreted by a `BehaviorHost` at one actor-tree position.
It owns no mailbox or new kinds and fails open on script faults.

**Capability**

A native actor that owns host policy or resources—filesystem, render, HTTP,
audio, lifecycle, and so on—behind typed mail.

**Marker layer**

Lightweight identity/kind types that guest code can use without linking a
native runtime implementation.

**Runtime layer**

Native state, adapters, handlers, and heavyweight dependencies enabled by a
runtime feature and installed by a chassis.

**Transform**

A linked, discoverable, bounded value-to-value native operation. It is not a
stateful actor or arbitrary host call.

## Processes and operation

**Substrate**

The native runtime mechanism: registry, scheduler, mail, actor hosts, settlement,
and chassis integration.

**Engine**

One running substrate instance with its own engine id, registry, scheduler,
capabilities, and components.

**Chassis**

A process composition that selects drivers, capability runtimes, and frame-loop
behavior. Desktop, headless, hub, substrate harness, and Bloomery are distinct
checked-in chassis profiles.

**Hub**

The control-plane chassis that stores binary/component artifacts, supervises a
fleet of child engines, and routes engine RPC.

**Bloomery**

The first-party Aether application and dedicated chassis for bounded software
development work. Its checked-in reducer, host services, API, and adapters are
substantial realization of Proposed ADR-0149, not evidence that the ADR is
Accepted. The binary can run standalone or through the hub's binary/fleet launch
path, but does not itself own `FleetServer`. GitHub is a source/projection
adapter, not Bloomery's state authority.

**Engine proxy**

The hub-side actor/state representing one connected child engine and its
heartbeat/routing relationship.

**MCP coordinator**

`aether-mcp`: the agent-facing service that converts tool JSON to engine
RPC/mail and projects replies/evidence.

**Tunnel**

The stable MCP-facing endpoint in front of volatile coordinator/hub processes.
Starting it cannot retroactively add tools to an already-open client session.

**Binary selector**

A name/version/hash or attribute query resolved against the hub's chassis binary
store. It is not a host executable path.

**Component selector**

A hash, stored name, or module/export selection resolved against the hub's wasm
store. Upload precedes selection for new bytes.

**Boot manifest**

Staged component/config instructions consumed while a substrate starts.

**Bundle**

A standalone desktop/headless executable with ordered component/config material
embedded at build time. It does not require the development hub.

## Frames, rendering, and I/O

**Frame lifecycle**

The ordered lifecycle stages that advance input/tick, render, present, and
shutdown work for one engine frame.

**Capture**

A bounded readback of the render target, optionally with pre/after mail applied
at defined points around it.

**Namespace root**

One configured filesystem root (`save`, `assets`, `config`) exposed through a
logical capability path rather than raw guest host access.

**Stream id**

An explicit id naming one long-lived HTTP/websocket data phase across many short
mail chains. It is not the engine id or a mailbox id.

**Credit**

A bounded allowance for producer chunks. Exceeding credit is a protocol error;
not granting more applies backpressure.

## Workflow and maturity

**ADR**

An Architecture Decision Record under `docs/adr/`. Accepted governs intended
design; Proposed does not; Superseded remains history.

**Bloom**

Bloomery's bounded source transaction: once sealed, an immutable promise over
specific workpiece scope revisions, a base, policy/toolchain inputs, and a
budget. Its members integrate and land as one artifact, or a successor bloom
supersedes it.

**Workpiece**

The stable identity of one intended change admitted to a bloom. A GitHub issue
can project a workpiece, but the issue is not the canonical identity and an
umbrella collection is not itself an admissible workpiece.

**Realization**

How much of a decision or proposal current code implements. Realization and ADR
status are independent facts.

**Managed Plan**

The scope-owned issue-body sections that define the problem, design,
implementation steps, dependencies, declared surface, dogfood brief, and exact
size/model routing lines. Their canonical digest is the approval identity.

**Approval record**

A trusted hidden issue-body record binding one managed-Plan digest and route to
an exact base commit and resolved approval policy. Labels and visible comments
are not substitutes.

**Direct-review verdict**

The implementer's human-readable handoff that the exact current-head diff was
inspected against the approved Plan and has no unresolved defect. It is not a
visible JSON/HTML PR marker, and it is independent of native GitHub review
decisions and unresolved threads.

**Landable draft**

A draft pull request whose approval ancestry and declared-surface containment
hold, required current-head checks pass, direct review accepts, native change
requests and threads are clear, and required dogfood evidence is current. It
still needs explicit landing authorization.

**Dogfood**

A consumer-view trial of a public surface. It is evidence about usability, not
a substitute for correctness tests.

**Worktree owner**

The task/agent that created or was explicitly handed a worktree. Existence or
set difference does not prove cleanup authority.
