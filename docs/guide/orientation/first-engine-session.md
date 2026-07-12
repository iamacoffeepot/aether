# First live-engine session

This walkthrough establishes the smallest useful operator loop: connect to the
hub, own an engine, inspect its vocabulary, gather evidence, and clean up. It
does not require a custom component.

## 1. Start the stable front door

From the repository root:

```sh
scripts/ensure-tunnel.sh
```

The helper is idempotent. It keeps the MCP-facing endpoint stable while the hub
behind it may restart. In Codex, the project endpoint is configured as
`aether-hub` in `.codex/config.toml`. If the `mcp__aether-hub__*` tools do not
appear after starting the tunnel, reconnect that MCP server in the active
surface; a subprocess cannot add tools to an already-open agent session.

Read [MCP architecture and startup](../mcp-harness.md) before debugging ports or
processes by hand.

## 2. Observe before creating

Call `list_engines` first. The result distinguishes live engines from recently
departed ones. A recently dead record can tell you whether a process was
deliberately terminated, crashed, evicted after missed heartbeats, or failed to
spawn.

This initial read prevents two common mistakes:

- creating a duplicate engine when a suitable one is already owned by the
  current task;
- treating a dead engine id as a routing or schema failure.

Do not borrow an engine merely because it exists. Ownership may belong to
another agent or test. Spawn a new one unless the task explicitly hands you an
id.

## 3. Spawn and retain the returned id

Call `spawn_substrate` with no selector for the stored default headless artifact
normally staged by the tunnel helper, or choose a stored binary by
selector/attributes when the task requires a particular chassis. A bare spawn
fails when that default is absent. The hub returns an `engine_id`; carry that
exact value through every later per-engine call.

Binary selectors refer to the hub's content-addressed store. They are not host
paths. Use `upload_binary` before selecting a binary that the hub does not
already know. The same rule applies to wasm with `upload_component`.

Treat successful spawn as resource ownership:

```text
spawn returns engine_id
        │
        ├── use that id for mail, inspection, capture, logs, and loads
        │
        └── terminate that id when the task is finished
```

## 4. Ask the engine what it knows

Use live discovery before composing mail:

- `describe_kinds` gives a best-effort kind snapshot. It attempts a live refresh
  but can return a prior/static snapshot if that refresh fails, so narrow by
  family/name and pair freshness-sensitive use with a harmless live probe.
- `describe_handlers` reports native receive contracts.
- `describe_transforms` lists the static transforms linked into `aether-mcp`;
  it does not inspect the selected engine.
- `list_components` shows component artifacts stored by the hub, not live
  instances.
- `load_component` returns the live lineage name and mailbox id;
  `describe_component` reports that lineage's handlers and boot contract. It is
  cache-first and asks the substrate by name only on a miss, so pair a cached
  description with a safe live probe when liveness matters.

This matters because chassis selection and dynamically loaded components change
the available surface. Static docs explain meaning; explicit-engine inspection
plus a bounded live probe tells you what is addressable now.

## 5. Perform one bounded observation

Choose evidence suited to the engine:

- `actor_logs` for recent structured entries from a known mailbox;
- `actor_cost` for per-handler execution-cost estimates;
- `capture_frame` for a rendered frame when the chassis has a render surface;
- `send_mail_traced` when you need settlement and reply evidence for one action.

The current headless chassis installs a fail-fast render fallback:
`capture_frame` returns an unsupported error rather than hanging. Use a desktop
or otherwise render-capable chassis when the observation requires a frame;
headless remains useful for mail, logs, costs, and settlement evidence.

If you send mail, use the recipient lineage name and kind name returned by live
inspection. Mailbox names and kind names are different namespaces.

## 6. Clean up and verify the outcome

Call `terminate_substrate` with the exact engine id you own. Then call
`list_engines` and confirm it is no longer live. A recently-dead record with a
termination reason is expected evidence, not a failure.

If termination fails or the process disappears first, do not guess that cleanup
succeeded. Re-read the fleet and use the [recovery runbook](../operating/recovery.md)
to distinguish an already-dead engine from a hub or routing failure.

## Where to go next

- To load or replace wasm, read [Component registry and replacement](../operating/component-registry.md).
- To send meaningful mail, read [Mail, kinds and scheduling](../systems/mail-and-kinds.md).
- To diagnose a failure, read [Inspection and debugging](../operating/inspect-and-debug.md).
- To author the recipient, read [Writing guest code](../writing-guest-code.md).
