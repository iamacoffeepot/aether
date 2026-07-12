# Simulation and player sessions

Aether's reference simulation vocabulary separates deterministic tick facts
from network transport. The player-session tier builds a trusted identity and
pacing boundary over TCP, then talks to a `TurnSim` actor through ordinary mail.

ADR-0144 is Accepted and the reference simulation ships. ADR-0145, which
describes the player-session-over-TCP tier, is still **Proposed**, even though
substantial corresponding code and integration tests exist. Treat that API as
implemented but not yet an accepted stable design commitment.

## Simulation vocabulary

`TurnSim` consumes intent and produces tick-native facts:

- `aether.sim.config` establishes bounds and cadence;
- `aether.sim.spawn` creates an entity;
- `aether.sim.move_intent` proposes a direction for a tick;
- `aether.sim.trajectory_event` and `state_summary` describe facts;
- `aether.sim.tick_bundle` groups the atomic result of a tick;
- `aether.sim.poll`/`poll_result` support catch-up reads.

Intents are binned by tick; the reference policy is deterministic and resolves
multiple intents for the same entity/tick predictably. Clients reconcile
bundles and summaries instead of replaying presentation guesses as authority.

## Gateway/session topology

```text
TcpListenerActor
  └─ TcpSessionActor per connection
       → GameGatewayCapability (validates TCP lineage and supervises)
            └─ PlayerSessionActor per trusted connection
                 ↔ TurnSim
                 → TcpSessionActor (encoded PlayerFrame writes)
```

The TCP session actor owns socket framing. The gateway is the admission and
supervision boundary: it accepts session data only from the configured TCP
lineage, creates and monitors one player-session child per trusted connection,
and fans trusted `TickBundle` facts from `TurnSim` out to those children. The
`PlayerSessionActor` decodes player frames, replaces any client-supplied entity
id with its own mailbox identity, and sends the allowlisted intent directly to
the configured `TurnSim`. It also encodes outbound frames and writes them back
through `TcpCapability` to the exact listener/session. An untrusted client
therefore cannot claim an arbitrary entity by putting it in a frame.

## Player frames

`PlayerFrame` is the transport vocabulary. It is recipient-free: routing is
determined by the established session, not by a mailbox id supplied over the
network. Frames cover handshake/control, intent, state delivery, and close/error
conditions needed by the current implementation.

The frame rides the TCP session's length-prefixed data contract. Decode limits
and authentication must happen before a frame becomes privileged engine mail.

## Pacing and recovery

Beacon/tick pacing prevents a client from turning socket speed into simulation
authority. A client may poll or receive pushed facts, reconcile superseding
summaries, and present only the newest coherent tick state.

On reconnect, do not assume the old session actor or entity binding survives.
The proposed tier needs an explicit resume/identity policy before it can be
treated as a durable public protocol.

## Reference client

`aether-kit::client::PlayerClient` demonstrates client-side state and frame
handling. It is an in-tree reference, not proof that every external client must
be Rust or link the kit. The portable contract is the framed wire vocabulary
plus the gateway/session policy.

## Change checklist

- Preserve determinism independent of connection timing.
- Keep trusted entity binding out of client-controlled fields.
- Bound frame size, intent rate, catch-up range, and queued output.
- Test disconnect during handshake, intent, and delivery.
- Distinguish simulation tick ids from mail correlation and TCP frame order.
- Update ADR-0145's status/design deliberately when stabilizing the tier; source
  presence alone does not accept it.

## Change route

- Simulation kinds: `crates/aether-capabilities/src/game/kinds.rs`
- Reference sim: `crates/aether-kit/src/sim/`
- Gateway/session: `crates/aether-capabilities/src/game/player/`
- Player frame: `crates/aether-capabilities/src/game/player/frame.rs`
- Reference client: `crates/aether-kit/src/client/`
- Integration tests: `crates/aether-substrate-bundle/tests/player_gateway.rs`
- Decisions: accepted ADR-0144; proposed ADR-0145
