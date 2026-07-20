# Harness lifecycle and fleet-wide mutations

A hub restart is not a transport refresh. It replaces the process that owns the
fleet and therefore destroys live engine state. Use this page before restarting
the hub, restarting `aether-mcp`, stopping the tunnel, or treating one of those
actions as recovery.

For ordinary engine ownership and termination, see
[Engine fleet](engine-fleet.md). For symptom-first recovery, see
[Recovery](recovery.md).

## Bootstrap is not restart

These surfaces operate at different scopes:

| Surface | Scope | Mutation |
|---|---|---|
| `scripts/ensure-tunnel.sh` while `:8890` answers | harness discovery | none; it exits without replacing the live stack |
| `scripts/ensure-tunnel.sh` on a cold host | host process stack | builds and starts the tunnel, hub, and `aether-mcp` |
| `GET /admin/status` | tunnel observation | none; reports child liveness, PIDs, and ports |
| `terminate_substrate(engine_id)` | one supervised engine | force-stops that engine |
| `POST /admin/restart-hub` | entire supervised fleet | terminates and replaces the hub process |
| graceful tunnel shutdown by `SIGINT` or `SIGTERM` | entire harness | asks the tunnel to terminate the hub, `aether-mcp`, and supervised fleet |

The tunnel's admin endpoint is loopback control, not an ownership system. It
does not authenticate a task or establish which session may restart the fleet.
Likewise, `/admin/status` and `list_engines` expose no owner or hub epoch. A
reachable endpoint, PID, port, or fleet row is evidence of existence—not
authority to destroy it.

## Authority boundary

Treat the tunnel as shared host state. It can outlive the session that started
it, and several agents or humans can drive the same hub.

- Terminate one engine only when your task spawned that exact current-epoch
  `engine_id`, ownership was explicitly handed to you, or the user authorized
  that engine's termination.
- Restart the hub only with explicit authority over the whole fleet and after
  coordinating with other active harness users.
- Stop the tunnel only when you own the complete harness or the operator has
  authorized a full shutdown.
- An empty fleet is not proof that another session has no in-flight spawn,
  upload, or other hub operation.

If one owned engine is unhealthy, terminate that engine. Do not use a hub
restart as per-engine cleanup.

## Blast radius

| Action | Destroyed | Preserved |
|---|---|---|
| `terminate_substrate(engine_id)` | that engine's live actors, components, scheduler, and in-memory state | other engines, tunnel, MCP session, stored artifacts |
| `POST /admin/restart-hub` | hub process, fleet table, proxies, all hub-spawned engines, loaded components, and recently-dead memory | tunnel, `aether-mcp`, MCP session shape, on-disk artifact stores |
| restart `aether-mcp` | MCP backend process, client session, and its local caches | hub, fleet, tunnel, stored artifacts |
| graceful tunnel shutdown | tunnel, hub, `aether-mcp`, MCP session, and supervised fleet | only separately persisted host state |

The hub process owns engine proxies. Proxy teardown force-kills and reaps the
child substrate it spawned, so replacing the hub also removes its supervised
children. This is intentionally stronger than reconnecting a socket.

Do not force-kill the tunnel and call that fleet cleanup. `SIGKILL`, a crash, or
another unorderly exit bypasses its shutdown path and can leave separately
spawned child process groups alive. Use the orderly `SIGINT`/`SIGTERM` path;
after an abnormal exit, reconcile process ownership explicitly before starting
another stack.

A hub-only restart leaves `aether-mcp` alive. Its caches have no hub-epoch key,
while a new hub can reuse old `engine_id` text. A cached kind or component
description can therefore refer to the prior epoch.

## Before a hub restart

1. Select one coordinator and stop new mutations.
2. Take a fresh `list_engines(show: "alive")` snapshot.
3. Identify the owner of every engine. If any owner is unknown or another
   harness user may still be active, stop and ask.
4. Preserve logs, traces, frames, recently-dead details, and application-owned
   state that will be needed afterward.
5. Resolve interrupted operations. A lost reply does not prove that the
   mutation never landed.
6. Decide whether terminating one owned engine is sufficient.

Do not infer permission from a quiet fleet, an unhealthy heartbeat, or the fact
that your session originally started the tunnel.

## Issue one restart

Use one coordinator and send one `POST /admin/restart-hub`. Do not issue
concurrent restart requests, and do not blindly retry an uncertain response.
The admin route does not carry an idempotency key or task lease; reconcile the
observable process and fleet state before deciding whether another request is
necessary.

This operator discipline does not make restart transactional. The admin route
and the tunnel's automatic supervisor do not share a restart lease, so even one
request can race a hub death detected by the supervisor. Avoid an admin restart
while the hub is flapping or status is unstable. A race can hang the request or
leave child tracking ambiguous; stop and reconcile process ownership instead of
sending another restart.

A successful admin response proves that its replacement fork returned. It does
not prove exclusive child tracking, that hub RPC is already ready, that an
engine was restored, or that the old application state survived.

If the call fails or its reply is lost:

1. read `/admin/status` once;
2. wait for a read-only `list_engines` call to succeed;
3. treat the result as a new epoch if the hub was replaced;
4. escalate rather than repeatedly cycling an ambiguous shared stack.

## Begin the new epoch

After a restart:

- discard every pre-restart `engine_id`, mailbox id, loaded-component handle,
  heartbeat age, and recently-dead assumption;
- reacquire the fleet with `list_engines`;
- re-list stored binaries and components, using exact hashes where identity
  matters;
- spawn replacement engines explicitly;
- rebuild live state only from known inputs or an application persistence
  contract;
- treat cached descriptions as hints until a harmless live probe confirms the
  new engine;
- restart and reconnect `aether-mcp` only when a clean cache boundary is worth
  losing the current MCP session.

Transport reconnection is not state recovery. Do not describe a re-dialed hub
or a reused identifier as the same engine.

## Per-engine termination remains forceful

`terminate_substrate` is narrower than a hub restart, but it is still not an
application shutdown handshake. Preserve evidence and application state first.
The tool removes the fleet row and asks the proxy to shut down; proxy teardown
then force-kills and reaps its owned child. Verify the row disappears, but do
not interpret the response timestamp as proof that every OS teardown side
effect was already visible at that instant.

## Source routes

- Tunnel topology and lifecycle decision:
  [ADR-0089](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0089-mcp-hub-lifecycle-tunnel.md)
- Tunnel supervisor and admin routes:
  `crates/aether-mcp/src/bin/aether-tunnel.rs`
- Fleet table and logical termination:
  `crates/aether-engine/src/server/runtime.rs`
- Proxy child ownership and forceful teardown:
  `crates/aether-engine/src/proxy/runtime.rs`
- MCP fleet orchestration: `crates/aether-mcp/src/tools/engine.rs`
