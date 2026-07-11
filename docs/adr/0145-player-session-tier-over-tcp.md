# ADR-0145: Player session tier over tcp

- **Status:** Proposed
- **Date:** 2026-07-11

## Context

ADR-0144 pinned the `aether.sim.*` tick-native vocabulary — intents flowing up
binned per tick, facts flowing down as a per-tick bundle (trajectory events, a
state summary, a supersession watermark), with a named-but-trivial interest
projection seam. That ADR deliberately scoped itself to the *vocabulary*: it took
"no dependency on the tcp capability" and is TestBench-drivable standalone. It
names, as one of its two consumers, "a future player session tier over tcp" — but
leaves the tcp-facing half undesigned.

That half is this ADR. The transport underneath is the `aether.tcp` capability
(ADR-0079 instanced session actors; ADR-0072 length-prefix postcard framing),
completed on both legs by two sibling issues: inbound delivery to a bound consumer
with frame reassembly (#3046) and outbound connect for client-side dials (#3047).
The `aether.tcp` layer moves *opaque* length-prefix frames between a socket and a
bound consumer mailbox; it knows nothing about who is on the other end or what the
frame bytes mean. Something must sit between that opaque transport and the
authoritative simulation and decide: who is this peer, is the frame they sent
allowed, whose intent does it become, and what does this connection get told back
each tick.

Nothing named `aether.game` or a player-session tier exists in the tree today. The
forces:

- **Untrusted input.** A tcp peer is unauthenticated bytes until proven otherwise.
  A decoded frame must never be able to name an arbitrary recipient mailbox, forge
  another player's identity, or dispatch a kind the server did not opt into. The
  security boundary is the session tier, not the sim.
- **Identity is a server fact, not a client claim.** Which player an intent belongs
  to must be stamped by the server from the connection, never read from the frame —
  otherwise any peer can act as any player.
- **Two clocks.** Game state is tick-denominated and float/wall-clock-free
  (ADR-0144's determinism invariant). But a client rendering the world needs to
  know the server's tick cadence to pace interpolation and detect lag. Wall-clock
  pacing must ride the *transport*, never leak into game state.
- **Symmetry with the transport already chosen.** The session tier is a per-
  connection instanced actor — exactly the ADR-0079 shape the `aether.tcp` session
  actor already is. It should reuse that category and the ADR-0122 identity/runtime
  split, not invent a parallel lifecycle.
- **Decoupling.** The session tier lives in `aether-capabilities` (the lower crate);
  the reference sim lives in `aether-kit` (the higher crate, #3049). The tier must
  speak the `aether.sim.*` vocabulary as *wire names* against its handler-table
  allowlist, taking no Rust dependency on `aether-kit` — the same decoupling
  ADR-0144 mandated from the other side.

## Decision

Introduce a **player session tier**: a per-connection instanced actor in
`aether-capabilities` that binds an `aether.tcp` session (transport) to the
`aether.sim.*` simulation (game), owning the handshake, the identity binding, the
untrusted-frame allowlist decode, the per-tick outbound bundle assembly, and the
tick clock beacon. One session actor per connected client. It is a general tier —
the toy world is its first consumer, not a special case baked into it.

### 1. Session lifecycle: handshake → active → closed

The tier actor is spawned per connection and named as that `aether.tcp` session's
**bound consumer** (the #3046 delivery contract) so every reassembled inbound frame
arrives as one `SessionData { session_name, peer, bytes }` mail, and `SessionClosed`
signals teardown. It writes outbound by mailing `SessionWrite { bytes }` back to the
session, and closes via `SessionClose` — the identical surface an accept-side and a
connect-side (#3047) session present, so the tier is transport-origin-agnostic.

A session begins in **handshake** state. The first inbound frame must decode to a
`Hello` (a new tier kind, following the `aether.rpc` `Hello`/`HelloAck` precedent):
a wire version and the client's declared self-identification. The server validates
the wire version, **assigns** the connection an identity (see §2), and replies
`HelloAck { wire_version, session_identity, tick, interval }`. Only after a
successful handshake does the session transition to **active**; an inbound frame
that arrives before `Hello`, or a version mismatch, closes the session. Auth in v1
is identity *assignment* (the connection is given a fresh server-side identity), not
credential verification — the credential seam is named and deferred (§ Consequences).

### 2. Identity is stamped from the connection

On successful handshake the server binds a **session identity** to the connection
and holds it in the tier actor's state. Every intent this session forwards to the
sim is attributed to that identity by the *server*; the identity is never read from
an inbound frame. A frame therefore cannot forge another player's identity — the
worst a malicious peer can do is send garbage on its own connection, which the
allowlist decode (§3) rejects.

### 3. Inbound: untrusted frame → allowlisted intent kind

Each `SessionData` body is a length-prefix-framed, kind-tagged intent payload. The
tier decodes it by **kind-id against its handler table, which is the allowlist**: a
frame names a *kind*, never a recipient (ADR-0033 dispatch by kind-id). A kind-id
outside the allowed intent set is dropped and logged — it is not dispatched, and it
cannot address an arbitrary mailbox. An allowed intent is forwarded to the sim's
intent mailbox, attributed to the session identity (§2), where ADR-0144's per-tick
binning and last-writer-wins supersession take over. The allowed set is the tier's
configured intent vocabulary (the `aether.sim.*` intent kinds in v1), carried as
wire names — no Rust dependency on the sim crate.

### 4. Outbound: per-tick bundle assembly with an interest projection seam

The tier binds as the sim's **fact-sink** (ADR-0144's push path) or polls it
(`aether.sim.poll { since_tick }`); either way it receives the authoritative
`aether.sim.tick_bundle` per tick. It applies the connection's **interest
projection** — in v1 the identity projection (every connection sees every entity,
"trivially large K") — frames the projected bundle, and `SessionWrite`s it to the
tcp session. The bundle contract is inherited whole from ADR-0144: atomic (one
bundle per tick, fully applied), tick-ordered, superseding (summary + watermark),
and bounded (fixed-depth retained ring). Per-connection projection is the tier's
job; ADR-0144's sim exposes the full set and the seam. v1 keeps the projection the
identity so the pipe is proven before the projection is real.

### 5. Tick clock beacon rides the transport, not game state

Each tick the tier emits a **beacon** to its client: the current tick number, a
server-monotonic timestamp, and the current tick interval. The beacon lets a client
pace interpolation and detect lag without inferring cadence from bundle arrival
jitter. It is a *transport-pacing* signal carried in its own tier kind — the
wall-clock timestamp lives here and never enters an `aether.sim.*` game-state kind,
preserving ADR-0144's determinism invariant. The beacon cadence is driven by the
substrate `Tick` stream the tier subscribes in its `wire` hook (ADR-0021/0068).

### 6. Kinds and crate placement

The tier's own kinds (`Hello`, `HelloAck`, the tick beacon, and any tier-level
close/error) live in the session-tier module of `aether-capabilities` under the
ADR-0122 identity/runtime split — the capability owns its kinds (ADR-0121). The
`aether.sim.*` intent and fact kinds are *not* owned here: they are ADR-0144's
vocabulary, spoken by wire name. The tier takes no dependency on `aether-kit`.

## Consequences

- The client/server slice has an agreed session protocol: #3050's client
  presentation actor implements the client side of the handshake, consumes the
  beacon, and applies the projected bundle; #3051's perf harness measures a real
  handshake-through-bundle round trip. Both build against this fixed shape.
- The security boundary is explicit and testable: identity is server-stamped, the
  allowlist is the handler table, and a frame can name only a kind. The negative
  test (a peer sending a non-allowlisted kind, or forging an identity field) has a
  defined outcome — drop and log, never dispatch.
- Determinism is preserved: the only wall-clock value in the whole path is the
  beacon timestamp, which is transport pacing and never reaches game state.
- The tier reuses ADR-0079 instanced session actors and the ADR-0122 identity/
  runtime split, so a transport-only build never pulls the runtime state, and the
  accept-side and connect-side sessions present one surface.
- Deferred, seams named: (a) **credential auth** — v1 assigns identity rather than
  verifying a credential; a real auth exchange slots into the handshake without
  reshaping it. (b) **real interest projection** — v1 is the identity; per-connection
  visibility culling attaches at the §4 seam. (c) **backpressure** — the intents-up
  path is low-volume; per-session outbound rate limiting (ADR-0128-style credit) is
  deferred, matching #3046's stance. None change the wire contract.
- Ordering: this tier consumes the `aether.sim.*` vocabulary (#3049 / ADR-0144) and
  both `aether.tcp` transport legs (#3046 inbound delivery, #3047 outbound connect).
  It is buildable only once those land; the client slice (#3050) builds on it.

## Alternatives considered

- **Fold the session tier into the sim (no separate actor).** The sim would decode
  untrusted tcp bytes directly. Rejected: it welds ADR-0144's determinism-critical,
  TestBench-standalone sim to a socket and an untrusted-input security boundary, and
  breaks the crate decoupling (the sim lives in the higher crate). The tier is the
  security and transport-binding boundary; the sim stays pure.
- **A wasm component for the session tier instead of a native cap.** The tier is the
  trusted security boundary that stamps identity and enforces the allowlist over raw
  sockets — native chassis territory (ADR-0079's motivating example is exactly a
  per-connection native session actor). A wasm tier would push the trust boundary
  into guest code and add a wasm hop on every inbound frame. Rejected for v1;
  game-specific *logic* still lives in the sim/kit, which the tier feeds.
- **Frame names a recipient mailbox (route-by-address).** Let the client address any
  mailbox directly. Rejected outright: it is the injection vector the allowlist
  exists to close — an untrusted peer must never name a recipient, only a kind the
  server opted into.
- **Read identity from the frame.** Simpler client, but any peer could act as any
  player. Rejected: identity is a server fact stamped from the connection.
- **Put the tick timestamp in the sim bundle.** Would give the client cadence for
  free, but reintroduces wall-clock into game state and breaks ADR-0144's
  determinism invariant. Rejected: cadence rides the transport beacon instead.
- **No handshake — first frame is an intent.** Saves a round trip, but leaves no
  place to negotiate wire version or assign identity, and no seam for future
  credential auth. Rejected: the handshake is cheap and is where identity is bound.
