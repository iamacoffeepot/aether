# Engine fleet

The hub supervises a fleet of substrate child processes. Every engine-facing
tool is routed through that hub by `engine_id`; the caller does not connect to a
substrate's RPC port directly.

This page covers fleet ownership. Component artifacts and live instances are
covered in [Component registry](component-registry.md).

## Current fleet tools

| Tool | Scope | Use it for |
|---|---|---|
| `list_engines` | hub | live engines and a bounded recently-dead sidecar |
| `spawn_substrate` | hub → new substrate | create an engine from a stored binary selector |
| `terminate_substrate` | hub → one substrate | force-stop and reap a supervised child |
| `upload_binary` | hub artifact store | ingest a chassis binary from a fleet-host path; optional `pin: true` |
| `list_binaries` | hub artifact store | discover stored binaries and their manifests |
| `pin_artifact` | hub artifact store | durable explicit pin by exact content hash |
| `unpin_artifact` | hub artifact store | drop only the explicit pin; a name still protects |

`list_engines` accepts `show: "alive"`, `"dead"`, or `"all"`. The unrequested
list is absent, not empty. Use `alive` for routine liveness checks and `dead`
for focused failure forensics.

## Engine identity and ownership

An `engine_id` is minted by the current hub process and indexes its current
in-memory fleet table. It is not a durable engine identity, a store key, or a
port alias. After a hub restart, reacquire the fleet with `list_engines`; do not
reuse an id captured before the restart. The process-local sequence resets, so
even the same UUID text appearing later is not proof of engine continuity.

The hub owns the child process mechanically. The caller still owns the
operational decision to keep or terminate an engine:

- If your task called `spawn_substrate`, record the returned id and terminate
  it during cleanup unless the user asked to preserve it.
- If you found an engine through `list_engines`, assume it is shared until its
  owner says otherwise.
- If a task loses track of its id, reconcile against a before/after fleet
  snapshot. Never terminate “the only engine” merely because it looks likely.

`terminate_substrate` removes the engine from the live table, records a
deliberate `terminated` death, and tells the proxy to shut down. Dropping that
proxy force-kills and reaps the child it spawned. The public surface is forceful,
not a graceful application shutdown handshake.

The hub materializes each executable under a per-engine scratch/handle-store
directory. Current termination does not remove that directory, and MCP exposes
no scratch cleanup tool. Retention or reclamation under the configured engine
store root is a hub-operator responsibility, not something an agent should
delete while cleaning one fleet row.

## Upload before selector

`spawn_substrate` resolves a selector against the hub's stored binary registry.
It does not accept a host executable path.

Use this order:

1. Call `list_binaries` with the chassis, linked-cap, or target filters you
   actually require.
2. If no stored entry matches, call `upload_binary` with its absolute
   fleet-host `staged_path` and, optionally, a useful name. Pass `pin: true`
   when the hash must survive LRU without a name; `pin: false` (the default)
   never clears an existing pin.
3. Capture the returned content hash.
4. Pass a hash, `name@version`, or name as the `spawn_substrate` selector.
5. Omit the selector only when the stored `default` headless chassis is the
   intended choice.

The hub reads and hashes the uploaded path and runs the binary's `--describe`
surface to capture its manifest. Re-uploading identical bytes deduplicates to
the same hash. A name is a movable pointer; a content hash selects exact bytes
for spawn. Durable explicit pin is a sidecar flag on that hash, set at upload
with `pin: true` or later with `pin_artifact`. `unpin_artifact` removes only
that flag — a name still protects, and there is no delete or unname tool.

The hub also protects, for as long as it is running them, the binaries of the
engines it supervises: an engine that is committed, still staging, or waiting
out a restart backoff holds its resolved content hash against LRU eviction. So
re-uploading over the name a live engine was spawned from cannot reclaim the
bytes it is executing, and its automatic restart still finds the recipe it
replays. That hold is not the operator's pin — it is never persisted, it
disappears with the hub process, and neither `pin_artifact` nor
`unpin_artifact` changes it. Loaded components are not held this way; the hub
does not supervise them.
Fleet and MCP must ship the same release: the `pin` field changes the typed
upload kind schema.

Running `--describe` is immediate native code execution, not passive manifest
inspection. Upload only a task-built executable in a stable private path or an
exact build the operator approved. Never upload a download, attachment, or
unknown executable to discover what it is, and do not treat its self-reported
manifest as attestation. Follow
[Artifact trust and provenance](artifacts/trust-and-provenance.md) for the
complete boundary.

### Selector choice

| Need | Selector posture |
|---|---|
| exact reproducibility or rollback | content hash |
| an explicitly versioned build | `name@version` |
| latest artifact under an operator-managed alias | name |
| ordinary bare test engine | omit selector for `default` |
| capability-specific chassis discovery | omit selector and use `chassis`, `caps`, and/or `target` query fields |

Selector resolution happens before the hub allocates an engine id. A selector
miss therefore leaves no engine to reap.

## Spawn checklist

Before spawning:

- [ ] Snapshot `list_engines(show: "alive")`.
- [ ] Confirm the binary exists with `list_binaries`.
- [ ] Decide whether headless or desktop behavior is required.
- [ ] Upload every boot component before referring to its selector.
- [ ] Give boot components explicit names when later automation must address
      them predictably.
- [ ] Ensure every derived boot lineage is unique across component specs and
      replica suffixes.

After spawning:

- [ ] Store the exact returned `engine_id` and `rpc_port` as evidence.
- [ ] Confirm the id appears in `list_engines(show: "alive")`.
- [ ] If components were requested at boot, inspect each configured/expected
      lineage with `describe_component`.
- [ ] Query `describe_handlers` or a narrow `describe_kinds` selection before
      sending unfamiliar mail.
- [ ] Establish log cursors for actors you will monitor.

`spawn_substrate` may take a `components` list. The MCP layer first resolves
each component selector from the hub registry, stages temporary wasm/config
files and a boot manifest, and asks the new substrate to read them. The
substrate loads the boot components in manifest order, one at a time, and binds
its RPC port only after every one has answered its load `Ok`, so the proxy's
first successful dial means every boot component is live. A boot component that
fails to load, including two specs that derive the same lineage name, makes the
substrate exit nonzero with the component and the host's error on its stderr,
and the spawn fails with a `spawn_failed` entry rather than leaving a
half-booted engine running. A boot load that does not answer within the
boot-load budget (`AETHER_BOOT_LOAD_BUDGET_SECS`, default 20 s) fails the boot
the same way, naming the component, so the spawn fails with that name in its
`spawn_failed` detail. A successful tool reply therefore proves that every
boot instance loaded. Describe a component before relying on its handlers.

`spawn_substrate` may also take a `mails` list — init mail (each entry
`{address, kind_name, params?}`, a `send_mail` item without `engine_id`)
dispatched straight after the spawn reply, when every boot component is
already live, so an entry addressed at a boot component never races its load. Each item settles like a `send_mail` item and
the response carries a per-item `mails` status list alongside the engine
information, so a failed init is visible in the spawn reply itself. Items are
best-effort: the engine is live by the time the bundle runs, so one item's
failure aborts neither the spawn nor its siblings — read the statuses. This is
the world-initialization surface; keep `capture_frame.mails` for frame-scoped
placement at observation time.

## Liveness and death evidence

Each live row includes `last_heartbeat_age_millis`. It is an observation of the
last confirmed proxy heartbeat, not an application health check. A low age
shows the substrate is answering pings; it does not prove a particular actor or
handler is correct. This signal is meaningful only while hub heartbeats are
enabled; when they are disabled, the age grows from initial connection even for
a healthy engine. With heartbeats enabled, a rising age is a reason to stop
adding mutations and collect what evidence is still reachable. The fleet row
does not expose the enablement setting; confirm it from hub configuration or the
hub operator.

Recently-dead rows distinguish:

| Reason | Meaning | First response |
|---|---|---|
| `terminated` | an operator deliberately called terminate | verify expected ownership |
| `crashed` | the substrate closed its proxy connection | preserve detail and host logs |
| `evicted` | heartbeat misses crossed the hub's limit | treat the engine as gone; inspect pressure or wedge evidence |
| `spawn_failed` | startup failed after an id was allocated | correlate the id and detail; do not invent a replacement identity |

The recently-dead list is a small bounded ring, not an audit log. Capture a
relevant row promptly. `died_age_millis` is relative observation, not a durable
timestamp.

If startup fails after id allocation, the error includes that id and the hub
records a matching `spawn_failed` entry. If the failure occurs before allocation
(for example, selector resolution), no id exists. A failed spawn whose
substrate never reported a port records `rpc_port` 0.

`prepare_fork` materializes the stored binary and fork+execs it for both the
initial spawn and an automatic restart. A materialization or process-spawn
failure's `detail` — the same string on the spawn `Err` and the matching
`spawn_failed` ring entry — names the stage, the binary content hash, and a
path-free IO category (`ErrorKind` and an OS error code when the host supplies
one). It does not include the realized executable path, the store source, the
fleet scratch root, or the application-name filename.

A substrate that exits during startup — an unknown flag, a boot error —
reports `the spawned substrate exited during startup (exit code N)` followed by
`; stderr:` and at most 2 KiB of the end of what the child wrote to stderr, such
as clap's usage error. A proxy that fails to connect for any other reason keeps
its proxy error and gains the same stderr suffix when the child wrote anything.
In that tail the realized executable path and the fleet scratch root are
redacted to `<substrate>` and `<fleet-store>`, and terminal colors are stripped.
The tail can still name the application name the caller passed, since clap
prints it in its usage line, and it can carry any other host path the child
itself chose to print. Pre-allocation errors are unchanged.

A live engine's stderr still streams to the hub's log as the engine writes it;
only a failed spawn's detail carries a copy of the end of it. Stdout stays
inherited by the hub.

## Restarts and persistence

A hub restart requires authority over the whole fleet; discovering a process or
engine row does not supply it. Coordinate one request with every active harness
user and follow [Harness lifecycle](harness-lifecycle.md) before using the admin
endpoint.

A tunnel-admin hub restart preserves the MCP session shape and the on-disk
artifact store, but replaces the hub process. Its engine table, proxies, and
child substrates do not survive as usable fleet members. The MCP RPC client can
re-dial the fresh hub, but that transport recovery does not revive engines or
make their old ids valid.

After any hub restart:

1. Retry a read-only `list_engines` if the call that crossed the restart failed.
2. Treat all pre-restart engine ids and loaded component addresses as stale.
3. Re-list stored binaries/components; their content store is intended to
   persist.
4. Spawn replacement engines explicitly and rebuild live state from known
   inputs. Do not claim guest state recovery unless the application has a
   separate persistence contract.

Restarting `aether-mcp` itself invalidates the MCP session and clears its local
description caches. Name-addressed `describe_component` remains able to ask a
live substrate directly after reconnect; it refuses a bare `mbx-…` id.

A hub-only restart intentionally leaves `aether-mcp` running, so its caches are
not cleared. They are keyed by `engine_id`, and a fresh hub can reuse the same
id text. Treat cached kind/name/component descriptions as potentially belonging
to the prior epoch. If exact clean introspection is required, a deliberate
`aether-mcp` restart plus MCP reconnect is the current cache-reset boundary.

## Time bounds and honest readiness

Fleet operations do not share one universal timeout:

- The hub forks each substrate with `--rpc-port 0 --rpc-port-file <path>`: the
  substrate binds a port it picks and writes it to that file once it is
  reachable, and the proxy dials only that reported port while the substrate is
  alive. A foreign RPC server on the host therefore cannot answer for an
  engine.
- Proxy startup has a hub configuration budget that covers both waiting for the
  port report and dialing the reported port; a zero configuration is
  explicitly the wait-forever sentinel.
- A substrate that exits during startup is reported on the first attempt, with
  its exit code or signal and its stderr in the detail.
- Boot-component loads run inside the proxy startup dial: the substrate binds
  only after they answer, so the same connect budget covers them, and a failed
  load is a startup exit reported with the substrate's stderr.
- Ordinary hub RPC calls such as list and terminate do not gain the
  `send_mail` settlement timeout merely because they are MCP tools.

Consult the live tool result and fleet state after any client-side interruption.
An interrupted spawn is not evidence that no child exists.

## Cleanup checklist

- [ ] Save any logs, trace output, frame files, or failure details first.
- [ ] Call `terminate_substrate` only for ids your task owns.
- [ ] Confirm the id disappeared from `list_engines(show: "alive")`.
- [ ] Optionally confirm a matching `terminated` row with
      `list_engines(show: "dead")`.
- [ ] Do not delete or rewrite the artifact store as engine cleanup.
- [ ] Report any per-engine scratch/store retention need to the hub operator.

## Source routes

- MCP contracts: `crates/aether-mcp/src/tools/mod.rs` and `args.rs`
- Spawn orchestration: `crates/aether-mcp/src/tools/engine.rs`
- Boot order (build, load boot components, bind):
  `crates/aether-chassis/src/boot.rs`
- Fleet table, id allocation, death ring, and termination:
  `crates/aether-fleet/src/server/runtime.rs`
- Child ownership and heartbeats:
  `crates/aether-fleet/src/proxy/runtime.rs`
- Fleet configuration: `crates/aether-fleet/src/server/config.rs`
- Artifact persistence and eviction:
  `crates/aether-fleet/src/store/`

See [Recovery](recovery.md) for symptom-first branches and
[Inspect and debug](inspect-and-debug.md) for evidence collection.
