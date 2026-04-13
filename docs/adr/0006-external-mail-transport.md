# ADR-0006: External mail transport and MCP hub

- **Status:** Accepted
- **Date:** 2026-04-13

## Context

ADR-0002 established that Claude is "just another mail sender" — no privileged API, the same mail system engine components use. What it left open was the wire: how does a Claude process (or any other external controller) actually reach a running substrate, across machines if needed?

Two vision-level facts shape the problem:

- **Claude-as-player is a first-class use case.** Players should be able to point their own Claude at a running engine and interact with it the same way a developer's Claude would. This means remote access is not a niche concern — it's the product.
- **Engines may sit behind NATs.** A player running aether on their home desktop does not have an inbound-reachable IP. A design where the engine listens and controllers dial in does not work without port-forwarding or tunnelling — both of which are non-starters for end users.
- **Multiple Claudes may want to attach to one engine, and one Claude may want to attach to multiple engines.** Co-op play, observation-while-driving, and Claude-watches-many-instances are all natural. A 1:1 transport channel between engine and controller forecloses these.

Prior thinking walked through two shapes and rejected both:

- **Substrate listens on a TCP socket; MCP bridges dial in.** Inverts the NAT direction (bad for remote engines) and requires either a discovery mechanism or fixed ports (bad for multi-instance).
- **Substrate IS the MCP server.** One binary, but drags `tokio` + `rmcp` into the substrate crate, couples the engine's lifecycle to MCP session state, and still doesn't solve the NAT-for-remote-engine problem.

This ADR records the shape settled on: a **central hub with engines and Claudes both connecting out as clients**.

## Decision

Introduce an `aether-hub` binary that owns all external-facing networking. Engines and Claudes are *both* clients of the hub — engines over plain TCP with postcard-framed mail, Claudes over whatever transport rmcp negotiates (SSE / streamable-HTTP). The substrate does not listen on any socket; it dials out to the hub on startup.

```
    Claude_1 (dev)     ──MCP──\
    Claude_2 (player) ──MCP────→ [aether-hub]  ←──TCP──── aether-substrate (instance A)
    Claude_3 (player) ──MCP──/        ↑                    
                                      └────TCP──────────── aether-substrate (instance B)
```

### Topology

- **Hub** (`aether-hub`, new binary): long-running process. Two listeners.
  - rmcp-managed transport for Claudes, on `AETHER_MCP_PORT` (default 8888).
  - Plain TCP for engines, on `AETHER_ENGINE_PORT` (default 8889). Postcard-framed bidirectional messages; the engine does not act as a listener.
- **Substrate** (`aether-substrate`, existing): on startup, opens a TCP connection to the hub's engine port. Performs a registration handshake, then runs the mail loop as usual. The hub connection is one more thing feeding the scheduler's queue.
- **Claudes** connect via their normal MCP client (Claude Code, or another MCP-aware client). The hub URL is the entire external config.

### Protocol — engines ↔ hub

- Wire: plain TCP, length-prefixed postcard frames (ADR-0005's structural tier as the framing format).
- Registration: on connect, engine sends `Hello { name, pid, started_unix, version }`. Hub replies `Welcome { engine_id: Uuid }`. All subsequent frames carry `engine_id` for self-identification.
- Direction: **uni-directional for V0** — frames flow Claude → hub → engine. The substrate consumes mail from the hub connection and pushes it onto its existing scheduler queue. No engine-originated replies.
- Liveness: periodic heartbeat frames in both directions. A connection that misses N consecutive expected heartbeats is dropped and any associated state (hub's cache of the engine, engine's cache of the hub url) is reaped.
- Reconnect: on disconnect, the engine reconnects and receives a fresh UUID. Resume-with-previous-id is explicitly not V0.

### Protocol — Claudes ↔ hub

- Transport: whatever rmcp supports (SSE or streamable-HTTP). Hub delegates the transport and session management to rmcp.
- Identity: the MCP session is the Claude identity unit. Claude Code passes a display name in MCP initialization; the hub stores it per-session for human-readable listing.
- Tool surface (V0, minimum): `send_mail(engine_id, kind, payload)`, `list_engines()`, `list_claudes()`. Each is a thin forwarder; names are resolved server-side using the engine's registry (reachable via the hub connection).
- Returns from `send_mail` are delivery acks — "the hub got the frame to the engine's socket" — not execution acknowledgements from the engine.

### Identity

- **Engines**: hub-assigned UUID at handshake time. The UUID is the routing key; the name/pid metadata is for display only.
- **Claudes**: MCP session id assigned by rmcp. Cross-session persistence is not provided.

### Scope for V0

- Single hub instance per machine, single substrate instance per machine, fixed ports (with env-var overrides).
- Localhost-only. Hub binds `127.0.0.1`.
- Uni-directional Claude → engine only. No pub-sub, no request-response, no engine-originated events.
- No auth. The localhost perimeter is the security model, stated explicitly rather than pretended-at.

## Consequences

### Positive

- **NAT-friendly for engines.** Engines dial out to the hub. A player running aether behind a home NAT needs zero port-forwarding to have a Claude — theirs or somebody else's — attach to it. This is the single biggest win and the reason the topology is shaped this way.
- **One endpoint to know.** The hub URL. Every Claude client config is that URL; every engine's `AETHER_HUB_URL` is that URL. No descriptor files, no discovery protocol, no port scanning.
- **Substrate stays sync and stays a game engine.** No `tokio`, no `rmcp`, no HTTP framework in `aether-substrate`. The hub connection is one TCP socket and a thread that funnels incoming frames into the existing scheduler queue.
- **Multi-engine, multi-Claude is a natural extension.** Hub's routing table is `{engine_id → TCP connection}` and `{claude_session → MCP session}`; sending to a specific engine is one lookup. Co-op (two Claudes, one engine) and observation (one Claude, many engines) fall out without extra machinery.
- **Remote deploy requires no wire change.** V0 hub binds to `127.0.0.1`. Flipping to LAN (`0.0.0.0`) or a public IP is a config change plus an auth layer — neither touches the frame format, neither touches the substrate.

### Negative

- **Hub is required infrastructure.** There is no two-process "substrate + Claude, nothing else" mode. For local dev this means three processes (hub, substrate, Claude); not a burden in practice but a real change from the current shape.
- **Hub is a single point of failure.** If it crashes, every connected Claude and every running substrate goes dark. Acceptable for a dev tool; worth revisiting if the hub ever becomes "the service" people run in prod.
- **One network hop added.** Claude → hub → substrate instead of direct. Negligible on localhost; visible if hub is remote relative to substrate. Fine until someone measures a problem.
- **Engine can't respond to Claude.** V0's uni-directional choice means Claude cannot get information *back* from the engine beyond "frame delivered." Pub-sub is parked until we know what observations matter. Users who want to "see the game Claude is playing" will hit this wall immediately; it's a deliberate V0 scope cut, not a permanent limit.
- **Two binaries now, plus two Cargo crates.** `aether-hub` is new. Compared to the two-process "bridge + substrate" design, we are not adding a binary — we are reshaping the one we were going to add. Compared to "substrate-only, fat substrate" we are adding one.

### Neutral

- **Engine-originated traffic is not precluded.** The wire is already bidirectional (heartbeats flow both ways). Adding engine → hub → Claude events later is additive: a new frame type on the engine wire, a new MCP notification or resource on the Claude side.
- **Remote / LAN / auth / multi-instance are all parked, not foreclosed.** Each is an incremental change to the hub only. The substrate and the mail system are unaffected.

## Alternatives considered

- **Substrate listens; MCP bridges dial in.** Rejected: engine-side NAT kills the remote case.
- **Substrate is the MCP server.** Rejected: drags `tokio` + `rmcp` into the substrate and couples engine lifecycle to MCP session state without solving remote.
- **Per-Claude stdio bridge (rmcp spawned as a subprocess of Claude Code).** Rejected: good for local single-Claude use, bad for multi-Claude and impossible for remote. The hub topology covers those cases without becoming materially harder to use for the single-Claude-dev-laptop case.
- **Hand-rolled MCP on the hub.** Considered. rmcp chosen for the transport flexibility (SSE / HTTP / future WebSocket) that becomes real the moment a non-localhost Claude connects. The hub is small enough that rewriting later is not blocked.
- **Fixed port with filesystem descriptor for substrate.** Rejected once the engine became a *client* — there's nothing to discover. The descriptor pattern was right for the old "substrate listens" shape; inverting the direction dissolves the problem.
- **Single TCP port with path-based multiplexing on the hub (MCP at `/mcp`, engines at `/engine`).** Rejected for V0 on simplicity grounds — two ports is 20 fewer lines of routing code. Revisitable when a single public ingress becomes a requirement.
- **Bidirectional engine↔Claude mail in V0.** Rejected on scope: uni-directional is the honest minimum that unblocks input-driving; the engine-to-Claude path is parked until the observations we actually want are clear (screenshots? state blobs? arbitrary mail?).

## Follow-up work

- **`aether-hub` crate** with the rmcp tool surface, engine TCP listener, heartbeat and reaping logic, and routing tables for engines and Claude sessions.
- **Substrate TCP client** that dials the hub on startup, runs the handshake, and feeds received frames into the mail queue. Existing scheduler and mail flow are unchanged.
- **Shared types crate** (probably an extension of `aether-substrate-mail` or a new sibling) for the engine ↔ hub wire — `Hello`, `Welcome`, `Heartbeat`, `MailFrame`, `Goodbye`. These are structural per ADR-0005 and postcard-encoded.
- **Docs** on hub URL configuration, Claude Code MCP config snippet, and the localhost-only V0 security model.
- **Parked, not committed:** pub-sub / engine-originated events, request-response mail, multi-instance discovery, LAN/remote deploy and auth. Each is additive and covered by the topology; none are designed in detail here.
