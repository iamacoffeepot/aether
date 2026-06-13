# ADR-0108: Multiplayer rooms and shared-world authority

- **Status:** Proposed
- **Date:** 2026-06-13

Supersedes **ADR-0037**'s addressing mechanism (substrate-as-client bubble-up). Builds on **ADR-0034** (hub-as-substrate), **ADR-0073** (substrate cluster consolidation — where the hub chassis lives), **ADR-0080** (settlement), and **ADR-0099** (actor identity and addressing), and reaps dropped members through the heartbeat/eviction liveness path (issue 1339, whose cadence is tuned by **ADR-0090**'s config layer).

## Context

We want two player engines to see one shared world: a server holds authoritative state, the players render it, and play stays consistent across machines. The wire topology that has to carry this is the forward model — every substrate is an RPC server, the hub is the RPC client (`RpcClient` dialing each substrate's `RpcServerCapability`). The retired `EngineToHub` / `HubToEngine` / `MailFrame` channel vocabulary is gone; mail crosses a process boundary only as a `WireFrame::Call` over that RPC link.

One direction of that topology is already built and proven. A hub→specific-engine relay carries a `Call` from an external client to a named engine's mailbox: `RpcServerCapability::handle_call` (`crates/aether-capabilities/src/rpc/server.rs`) reads `envelope.to.engine = Some(id)` (the wire address is `MailboxAddress { engine: Option<EngineId>, mailbox: MailboxId }`), emits a `RouteEnvelope` at the hub-resident `aether.engine` cap (`EngineServer`), whose `on_route` looks the engine up in its proxy table and re-emits a `ForwardEnvelope` at the per-engine `EngineProxy`; the proxy writes the `Call` to that engine's substrate and streams replies home correlated, closing on a `CallSettled`. FleetBench (`crates/aether-substrate-bundle/tests/fleetbench/mod.rs`) drives this over real forked headless substrates, so the server→client leg is load-tested wire, not a sketch.

Three things multiplayer needs that this relay does not yet give:

- **A room** — the membership of player engines a server fans state to, keyed by `EngineId`.
- **Server→members fan-out** — one-to-many delivery; the relay today carries one external `Call` to one engine.
- **An engine→hub upstream leg** — a client engine pushing unsolicited input *up* its connection. The forward topology has no path for this: a substrate's `RpcServerCapability` only ever *answers* a `Call`; it never originates a frame.

ADR-0037 answered cross-engine addressing under the previous topology, where each substrate was a hub *client* and unresolved mail bubbled *up* the client connection (`SourceAddr::EngineMailbox`, the `EgressBackend::egress_unresolved_mail` / `egress_to_engine_mailbox` surface, gated on `outbound.is_connected()` in `Mailer::route_mail`, with `DroppingBackend` as the disconnected default). In the forward model the substrate is the RPC server and never dials the hub, so that surface is dead code: `is_connected()` is false and the egress methods are unreachable. The upstream leg has to be re-founded on the forward topology, not revived.

## Decision

Build the hub as the authority first. The server→client leg already exists, so a hub that owns world state and fans snapshots down the proven relay reaches a shared world before the upstream wire exists at all — that ordering is the whole point of this decision. Four parts, the last deferred.

### 1. A room/membership capability, `aether.room`

A hub-resident native cap, sibling to `aether.engine` in the hub chassis (`crates/aether-substrate-bundle/src/hub/chassis.rs`). It owns room membership keyed by `EngineId`. Control kinds, hub-local (`engine = None`), the same settled control-mail shape as `aether.engine`:

- `aether.room.join { room: String, engine_id: String }` → `join_result`
- `aether.room.leave { room: String, engine_id: String }` → `leave_result`
- `aether.room.list_members { room: String }` → `members { room: String, members: Vec<String> }`

Membership is a `room → {EngineId}` table. Joins and leaves settle (ADR-0080) so a caller knows the roster changed before it acts. Every field is explicit and stringly self-describing for an agent caller — the `engine_id` is the tagged form a prior `spawn_substrate` / `list_engines` already handed back.

### 2. Fan-out: `BroadcastToRoom`, unsettled

A hub-internal kind `BroadcastToRoom { room: String, kind: KindId, payload: Vec<u8> }`. The room cap resolves the member set and re-emits one relay per member through the existing `aether.engine` → `EngineProxy` path — the same fan the external relay drives, once per member. Each re-emission is **one-way and unsettled**: `cid = None` end to end, mirroring `handle_call`'s `cid = None` arm that skips reply tracking entirely — no `in_flight` entry, no settlement subscription. (`RpcClient::call` mints a `cid` today; the unsettled leg adds a `cid = None` write on the proxy's existing connection.)

Unsettled is load-bearing for per-tick state. A settled fan-out would block the tick on N cross-process settlements, coupling the authority's liveness to every client's delivery, and a dropped frame would stall the producer. Snapshots are immediate-mode (resend every tick), so a lost one is corrected by the next. Settlement (ADR-0080) stays on the join/leave control path, where a caller genuinely waits on the result.

### 3. Authority: a hub-resident world cap, `aether.world`

A hub-resident native cap wrapping `aether-kit`'s `runtime::Locomotion` + `arena`. It owns the authoritative world state, accepts move-intents, ticks the simulation, and emits `WorldSnapshot { tick: u64, movers: Vec<MoverState> }` fanned to the room via `BroadcastToRoom`. Move-intents are the existing `aether.kit.locomotion.*` kinds, agent-driven initially — an agent sends an intent at the hub-resident cap through the relay.

Server authority rests on determinism. aether-kit positions are fixed-point integers (octimeters, `1 tile = 256 octimeters`), and a step is a pure fixed-point function of `(state, input)`, so the simulation is bit-exact across machines — the precondition for one authoritative copy and for deterministic replay. The cap hands clients authoritative state; a client never asserts its own position.

### 4. The engine→hub upstream leg (deferred to its own PR)

The one direction the forward topology lacks: a member engine pushing unsolicited input up to the hub. The substrate-side uplink writes a frame on the connection its `RpcServerCapability` already holds (the one the proxy dialed), reusing `WireFrame::Call { cid: None, envelope }` in the reverse, server→client direction rather than minting a new frame. The proxy's reader (`EngineProxy::on_inbound_ready`, which today logs an unexpected inbound `Call` and drops it) gains an inbound route that delivers the frame to a hub mailbox, **stamping the source `EngineId` from the proxy's own connection identity**, never from the payload — exactly as `on_route` derives the engine from the proxy table, not from caller-supplied bytes.

This supersedes ADR-0037's mechanism. It is deferred to a separate PR; until it lands, movement is agent-driven and the demo is a shared-world view — both clients render authoritative state pushed down the relay.

### Trust boundary

Engines are adversarial by default, so identity and authority are stamped and validated hub-side:

- **Source identity comes from the connection.** The upstream leg stamps the originating `EngineId` from the proxy's connection, never from the frame's payload — mirroring `on_route`, which derives the engine from the proxy table.
- **The authority validates intents.** Clients send move-intents; the `aether.world` cap decides the resulting position. A client never sends an authoritative position.
- **The upstream leg is rate- and size-bounded.** It reuses `RpcServerCapability`'s existing frame cap (`aether_codec::frame::max_frame_size`, surfaced as `RpcError::FrameTooLarge`) plus a per-connection rate limit, so a flooding client is bounded by the machinery a malformed `Call` already hits.
- **A dropped client is reaped idempotently.** The room cap subscribes to the heartbeat/eviction liveness path (issue 1339): an `EngineDied` — the proxy observed its connection close or its liveness heartbeat cross the miss limit — removes the member from every room, idempotently, so a corpse never lingers in a roster.

Cross-engine addressing stays `(engine_id, well-known mailbox)`. Lineage (ADR-0099) does not cross engines — a `MailboxId` is a hash-chain over one engine's runtime lineage, meaningless on another — so a room member is addressed by its `EngineId` plus a well-known mailbox name, which the wire `MailboxAddress { engine, mailbox }` already carries.

## Consequences

**Positive**

- The entire server→client leg reuses the proven relay fan — no new wire frame, no new routing seam. `BroadcastToRoom` is one `aether.engine` re-emission per member over a path FleetBench already exercises.
- The view-only shared world lands before the upstream leg exists. Parts 1–3 demo a real multiplayer view on wire that is built today; part 4 is additive.
- Determinism comes for free: aether-kit's fixed-point state is already bit-exact, so authority and replay need no new numeric work.

**Neutral / cost**

- The hub takes on hosting game state. Authority here is native (`aether.world` wraps aether-kit directly). A wasm authority would need the component runtime wired into the hub chassis, which is out of scope — the hub chassis hosts native caps plus the RPC server today, not a guest loader.
- Two new hub-resident caps (`aether.room`, `aether.world`) and one hub-internal kind (`BroadcastToRoom`) join the hub chassis builder beside `EngineServer` and `RpcServerCapability`.

**Negative / risk**

- The cross-engine path is covered only by FleetBench, which is heavy and serialized (`mod heavy`) — TestBench is single-engine and loopback, so it cannot see the relay or the fan-out at all. Multiplayer regressions surface only in the slow test tier.
- Fire-and-forget fan-out does not retry. A dropped `WorldSnapshot` is superseded by the next tick; a client on a lossy link sees stutter, and the next snapshot is authoritative.

**Follow-on**

- The engine→hub upstream leg (part 4) as its own PR: the reverse-direction `Call`, the proxy inbound route, the connection-stamped source.
- Engine-as-authority — the symmetric model where one player engine is the server — once the upstream leg exists in both directions.
- Revive or retire `SourceAddr::EngineMailbox` and the `EgressBackend::egress_*` surface. The upstream leg re-founds the capability ADR-0037 reached for; once it lands, the dead bubble-up surface is either repurposed or deleted.

## Alternatives considered

- **Engine-as-authority first ("one engine is the server").** A player engine holds authoritative state and the others are its clients. Rejected as the first step: it needs the new upstream wire in *both* directions before anything demos — clients push input up to the authority engine, and the authority pushes snapshots back down — and neither path exists in the forward topology today. Hub-as-authority needs the new wire in neither direction (it fans down the proven relay and accepts agent-driven intents), so it reaches a shared world far sooner. This is a deliberate deviation from the engine-as-server framing; the symmetric model is the follow-on once the upstream leg lands.
- **Settled fan-out.** Await settlement on each per-member snapshot relay. Rejected: it serializes the tick on N cross-process round-trips and couples the authority's liveness to every client's delivery, so one slow or dropped client stalls the whole world. Per-tick relay must be unsettled; settlement stays on the control path.
- **Trusting client-supplied identity.** Let the upstream frame carry its own `EngineId`. Rejected: an adversarial engine would spoof any peer. Identity is stamped from the connection, the one fact the sender cannot forge.
