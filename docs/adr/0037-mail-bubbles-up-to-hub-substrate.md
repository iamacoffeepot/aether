# ADR-0037: Mail bubbles up to the hub-substrate

- **Status:** Proposed
- **Date:** 2026-04-20

## Context

ADR-0034 committed the direction "hub becomes a substrate chassis" — the hub will eventually run wasmtime, host components, and speak the same protocol as every other substrate. ADR-0035 shipped the chassis split that makes that structurally possible. Neither ADR specified **how a component on one substrate addresses a mailbox on a different substrate.**

The demo that forced the question: PR #155 shipped a tic-tac-toe server component. The natural sequel is a desktop tic-tac-toe **client** component that renders a board and sends moves to the server. The obvious deployment is:

- Game server on a headless (or future hub) substrate.
- Game client on a desktop substrate.
- Client addresses server by name: `"tic_tac_toe.server"`.

Under ADR-0029 every mailbox id is a compile-time hash of its name, so the client and the server independently compute the same `u64` id. What's missing: the client's substrate has no local mailbox registered under that id, so under today's scheduler the outbound mail is warn-dropped.

Two approaches to closing that gap were weighed in design chat before this ADR:

1. **Hub-side routing table.** The hub maintains `global_name → (engine_id, mailbox_id)`. Participating substrates pre-register the names they serve. Engine-to-engine mail is forwarded through the hub using the table. The guest SDK gains a new `RemoteSink<K>` type that clients use to mark routed destinations explicitly. The hub becomes a *dispatcher*.

2. **Hub-substrate as the destination.** The server component lives on the hub's substrate, not on a separate engine. The hub doesn't route between engines — it *is* the destination. Clients mail the server's mailbox id the same way they mail a local component. When the local substrate doesn't resolve the id, its unresolved-mailbox path forwards the mail to the hub-substrate, whose own control plane dispatches against its own registry.

We chose (2). The hub is the client's API boundary. What's behind that boundary — a single locally-loaded component, a cluster, or a future routing component that forwards onward to shards — is the hub's internal concern.

## Decision

**A substrate that fails to resolve a mailbox id locally forwards the mail to its hub-substrate parent.** The hub-substrate receives the forwarded mail through its chassis peripheral, resolves the id against its own registry, and dispatches locally. Replies follow the reverse path.

Concretely:

1. **Fallback semantics.** A substrate with an attached hub (`AETHER_HUB_URL` set, `HubClient::connect` succeeded) gains a "forward unresolved mail to hub" path in its scheduler. A mailbox id that hits neither a local component nor a local sink becomes a forwarded mail frame addressed upstream. This replaces today's unconditional warn+drop for hub-attached engines; disconnected substrates (no hub parent) still warn+drop.

2. **No new sink type in the guest SDK.** `ctx.resolve_sink::<K>("tic_tac_toe.server")` stays a plain `Sink<K>` that computes a mailbox id the same way local resolution does. The SDK does not distinguish local from hub-resident sinks — deployment decides where the component actually lives. A server component ported from a standalone headless substrate to the hub-substrate requires zero client-side recompilation.

3. **Protocol extension.** The engine-to-hub wire protocol gains a new address variant for mail whose destination is a mailbox on the hub-substrate itself (as opposed to a Claude session or a broadcast). The hub-substrate's chassis peripheral accepts the inbound frame, decodes the payload against the registered kind descriptor, and pushes onto its local mail queue. Indistinguishable from locally-originated mail from the scheduler's perspective.

4. **Replies travel the reverse path.** The inbound mail frame carries the source engine's id and the source component's mailbox id, packed into the sender handle delivered to the hub-resident component. When that component calls `ctx.reply(sender, ...)`, the hub-chassis's reply peripheral recognises the sender as an engine mailbox (not a Claude session), and emits an outbound mail frame addressed back to the source `(engine_id, mailbox_id)` pair. The receiving engine's hub client decodes the frame and dispatches locally — again indistinguishable from a local reply.

5. **Sender identity widens.** The `Sender` opaque handle (ADR-0013) today identifies a Claude session for reply-to-sender. It widens to also identify an engine mailbox. Component code remains unchanged — `ctx.reply(sender, ...)` works identically in both cases; the hub-chassis interprets the handle's internal discriminant and routes accordingly.

Reply routing is called out explicitly because it's the subtlety that defeats a naïve "forward unresolved and forget." Without the reverse path, a hub-resident server can broadcast state updates but cannot targeted-reject a bad move back to the specific client that sent it — which is the shape of most real protocols.

## Consequences

### Positive

- **Server components are location-independent.** The tic-tac-toe server shipped in PR #155 runs on a headless substrate today; with zero code changes it can also run on the hub-substrate. Deployment picks where authoritative state lives, not the component author.
- **Client mental model is flat.** The client's view: "I mail the server by name." What's behind the name (local component, hub-resident component, or future sharded cluster) is invisible. Client code has no concept of remoteness.
- **Sharding and routing are later, orthogonal decisions.** Future ADRs can introduce hub-side logical-name routing, sharding by world region, or federation across hubs without breaking client code. The bubbles-up contract becomes the trivial case; shard-aware names get intercepted earlier in the hub's dispatch pipeline.
- **Uses existing primitives.** Mailbox ids are already globally unique hashes (ADR-0029). Hub-substrate local dispatch is identical to any other substrate's local dispatch. Only the engine↔hub wire protocol and the hub-chassis's reply peripheral need net-new code.
- **Observation path unchanged.** `hub.claude.broadcast` works the same way whether the broadcasting component is on engine A, engine B, or on the hub-substrate itself. Cross-substrate observation doesn't need the bubbles-up mechanism — it already fan-outs through the hub.

### Negative

- **Silent forwarding of typoed names.** A component that mails a typoed mailbox name today warn-drops locally. Post-change, unresolved ids go up to the hub, which then warn-drops. One extra network hop per typo, and the warning appears in hub logs rather than local engine logs. Mitigation in follow-up work: route the "forwarded but still unresolved" warn back to the originating engine so local `engine_logs` surfaces it.
- **Hub becomes critical path for cross-substrate work.** A hub outage takes down coordination between substrates. This was already true for session-targeted mail; it's now also true for engine-to-engine workloads. Single-substrate demos are unaffected.
- **Reply-path complexity.** The sender-handle widening and the hub-chassis's reply peripheral are new infrastructure. Not conceptually deep but load-bearing — a bug in reply routing shows up as "server replies land nowhere," which is hard to debug without explicit tooling.
- **Performance: hub hop per cross-substrate mail.** Same concern ADR-0034 raised for its routing hop. For tic-tac-toe this is one extra round-trip per move, unmeasurable. For high-throughput workloads, mitigations (locality hints, direct engine-to-engine optimisations, hub-internal sharding) are future work.

### Neutral

- **Server portability is a design constraint, not just a happy accident.** Server components that lean on hub-substrate-only capabilities (e.g., "enumerate connected engines") won't run portably on a plain headless substrate. This ADR doesn't forbid those capabilities; it observes that portability survives as long as the component sticks to the common mail surface.
- **Scope does not cover hub-resident special capabilities.** Enumerating engines, addressing a specific engine directly, registering global names for future routing tables — all plausibly useful for routing, management, and operator-Claude components. This ADR covers the bubbles-up addressing contract only. Those capabilities are future ADR territory when a concrete component needs them.

## Alternatives considered

- **Explicit `RemoteSink<K>` in the guest SDK.** Add a new sink type whose FFI path carries "this is routed" as a flag. Rejected because it makes locality of the destination a compile-time property of the caller, defeating location-independent deployment. A server moved from engine A to the hub-substrate would require recompiling every client — the opposite of the "hub is the API boundary" goal.
- **Convention-based name prefix (`hub.*` forwards).** Names starting with `hub.` forward up; others stay local. Rejected because mailbox ids are already opaque name-hashes by the time the substrate sees them; enforcing the prefix would require a client-side SDK branch anyway. Fallback-on-unresolved is simpler and achieves the same effect without an explicit namespace carve-out.
- **Hub-side routing table with pre-registered global names.** Substrates publish the names they serve; the hub routes on match. Rejected *for this ADR* as premature. It is a plausible future extension (sharding, federation, load-balancing) layered on top of the bubbles-up model — but paying for the design cost before a concrete cross-substrate workload needs it spends budget that the tic-tac-toe demo does not.
- **Direct engine-to-engine mail, bypassing the hub.** Substrates open peer-to-peer connections and route among themselves. Rejected as architectural overreach at this scale and opposed to ADR-0034's "hub is the coordination layer" direction. Always available later as a targeted performance optimisation for specific high-throughput paths; never as the default.

## Follow-up work

- **Phase dependency.** This ADR requires ADR-0034 Phase 1 (the hub-chassis binary). Implementation order: hub-chassis ships → bubbles-up lands on top → tic-tac-toe-client demo exercises the full round-trip.
- **Reply-path peripheral.** The hub-chassis's reply mechanism — how it distinguishes engine-mailbox senders from session senders, how it routes outbound mail by `(engine_id, mailbox_id)` — needs a concrete implementation sketch inside the hub-chassis PR. Not ADR-worthy on its own but load-bearing.
- **Typo diagnostics.** Design the "forwarded but unresolved" log path so a misrouted mail produces one clear warn at the caller's local `engine_logs`, not a silent drop at the hub. Candidate: hub sends a `mail.unresolved` observation back to the originating engine, which re-warns locally with full provenance.
- **Hub-resident capability ADR.** First time a hub-resident component legitimately needs to enumerate engines, target a specific engine, or register a globally-routed name, capture those capabilities as their own ADR. Tic-tac-toe does not force this.
- **Sharding and hub-side routing.** Explicitly deferred. Load-aware routing, shard-by-region, federated hubs — all plausible, none needed for the first multi-substrate demo. Revisit when a workload measurably demands it.
