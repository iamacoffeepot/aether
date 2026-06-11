# The MCP harness

> **Governing ADR:** [ADR-0089](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0089-mcp-hub-lifecycle-tunnel.md) (the tunnel), over the per-subsystem ADRs the tools
> front. The harness is **stable in shape** but its tools evolve — so treat this
> page as the map and the mental model, not a parameter reference. Each tool's
> exact arguments live in its own schema, which your MCP client shows you live;
> that schema is the source of truth and is more current than any prose (this page
> included). When the two disagree, believe the tool.

An agent doesn't link against the engine or call it in-process. It drives a
*running* engine from the outside, over **MCP** (Model Context Protocol): each
tool call becomes mail against a live substrate, or a query about one. This is the
concrete form of the "agent in a harness" idea — the engine runs, the agent pokes
it, watches what happens, and adjusts. If you're an agent reading this guide, this
is the page that turns everything else into something you can actually *do*: the
other pages tell you what to send; this one is how you send it.

## The shape: three processes and a fleet

The harness is three processes nested one inside the next, fronting a fleet of
engines:

```
:8890  aether-tunnel        — the stable MCP front your client connects to
  ├─ :8891  aether-mcp      — translates each tool call into a wire Call to the hub
  └─ :8901  aether-substrate-hub  — supervises the fleet
        ├─ substrate (engine A)   — one running chassis
        ├─ substrate (engine B)
        └─ …
```

The **tunnel** is the only thing your MCP client talks to. It supervises and
re-forks the two backends below it, which is the point: you can rebuild and
restart the hub without your MCP session ever dropping. **aether-mcp** is the RPC
client — it turns a tool call into a wire `Call` and relays it. The **hub** owns
the fleet: it forks substrates, assigns each a localhost RPC port, heartbeats
them, and routes your mail to the right one by `engine_id`. A **substrate** is one
running engine — a full chassis — and you can have several at once.

Because the hub heartbeats every engine and evicts a dead or wedged one, an engine
that shows up in `list_engines` is one you can actually reach. Each entry also
carries a `last_heartbeat_age_millis`, and it's the first thing to check when an
engine stops behaving: a healthy engine's age stays small, so a climbing value is
the early sign it's gone slow or unresponsive — wedged but not yet past the miss
limit the hub evicts on. If your mail to an engine isn't landing, read that age
before anything else; it tells you whether the engine is struggling or the problem
is on your side.

To restart the hub
after a rebuild *without* losing your session, hit the tunnel's admin endpoint
(`POST /admin/restart-hub`); the tunnel re-forks the hub and aether-mcp re-dials
it on your next call. Restarting aether-mcp itself does drop the session, so prefer
restarting the hub.

## Bringing it up

The stack isn't running by default — a cold build of the tunnel can take long
enough to look like a frozen session, so it's left to the point of use. Bring it up
yourself with `scripts/ensure-tunnel.sh`: it's idempotent (a no-op if `:8890` is
already bound, otherwise it launches the tunnel detached). Ports and the rest are in
`CLAUDE.md`'s MCP-harness section, which is the operational reference for the stack.

## The session loop

Everything is keyed by `engine_id`. A session has a recognizable arc:

1. **Get an engine.** `spawn_substrate(binary_path)` forks a fresh substrate and
   returns its `engine_id` (and RPC port); `list_engines` shows the ones already
   running. You hand that `engine_id` to every later call.
2. **Set it up.** `load_component(engine_id, binary_path)` loads a wasm component
   and returns its `mailbox_id`, resolved `name`, and advertised capabilities.
3. **Drive it.** `send_mail(…)` delivers a kind to a mailbox. By default it blocks
   until the dispatch chain settles and hands you the correlated reply.
4. **Watch it.** `capture_frame` reads the rendered frame back as a PNG;
   `actor_logs` pulls one actor's log ring; the `describe_*` tools report the
   engine's types, a component's handlers, the handle store.
5. **Settle precisely.** `send_mail_traced` when you need to know a whole causal
   chain finished, with its trace tree, rather than a single reply.
6. **Tear down.** `terminate_substrate(engine_id)` when you're done with an engine.

## The tools

**Fleet.** `list_engines`, `spawn_substrate`, `terminate_substrate`. The `engine_id`
each returns is the handle every other tool needs.

**Sending mail.** `send_mail` is the workhorse. You give it a batch of items, each
`{engine_id, recipient_name, kind_name, params}` — the **mailbox** to deliver to,
the **kind** to deliver, and the structured params, which the tool schema-encodes to
wire bytes against that kind's descriptor. By default each item *blocks* until its
chain settles and returns the correlated reply payloads, so a request/reply (mail
`aether.fs.read`, get the bytes back) is a single call with no polling. Set
`fire_and_forget: true` for a poke you don't wait on — a `DrawTriangle` right before
a `capture_frame`, or a cap that never replies. Items are independent: one bad item
doesn't abort its siblings.

`send_mail_traced` is the same idea with a shared trace root. Every item in the
batch lands under one chassis-level trace root, and the call returns the full,
settled **trace tree** for the whole batch (plus the replies). Reach for it when you
need exact whole-chain settlement — proof that everything a mail set off has finished
— or all-or-nothing dispatch where a single bad item aborts the batch before any mail
moves. For independent items where you just want each reply, plain `send_mail` is the
simpler tool.

**Introspection.** `describe_kinds` is how you learn what to put in `params` — it
returns the static kind vocabulary with full schemas. `describe_component` reports a
loaded component's handler kinds, their docs, whether it has a fallback, and its
boot-config kind. `describe_transforms` lists the native transforms a DAG can wire;
`describe_handles` inspects the persistent handle store.

**Components.** `load_component` and `replace_component` take a filesystem **path** to
the `.wasm` — the tool reads the bytes; you never inline a wasm buffer through a tool
call. Both accept a `config_path` for a typed-config component (a file holding the
config kind's wire bytes; `describe_component` tells you which kind it wants).

**Observation.** `capture_frame` returns the engine's current frame as inline PNG,
and can carry two mail bundles dispatched atomically around the readback — `mails`
before (the state that should appear) and `after_mails` after (cleanup). How that
frame is produced — world-space geometry, the camera matrix, the depth convention —
is covered in [Rendering & camera](systems/rendering.md).
`actor_logs` pulls recent entries from one actor's per-actor log ring by mailbox
name; thread the reply's `next_since` back as `since` to page forward without
re-reading. Only in-actor `tracing::*` events reach a ring — see
[Logging](systems/logging.md) for the in-actor versus stderr boundary.
`actor_cost` reads each actor's per-handler execution-cost EWMA table
(mean and MAD in nanoseconds, plus a sample count); pass a `kind_id` to filter to
one handler.

**Computation DAG.** `submit_dag` validates a descriptor synchronously (you get the
verdict and the output handle ids immediately) and then executes the sources
asynchronously; `dag_status` polls execution, `dag_cancel` stops an in-flight DAG.

## Conventions that bite

- **Mailbox vs kind.** `recipient_name` is the mailbox; `kind_name` is the payload.
  They route independently even when they share a prefix — send the kind
  `aether.audio.note_on` to the mailbox `aether.audio`. See
  [Mail, kinds & scheduling](systems/mail-and-kinds.md).
- **Paths, not bytes.** `load_component`, `replace_component`, and a DAG source's
  payload take a filesystem path the harness reads. Tool JSON never carries the wasm
  or payload buffer itself.
- **Wire ids are tagged strings.** Mailbox, kind, and handle ids come back as
  `mbx-…`, `knd-…`, `hdl-…` — hand them back verbatim, don't reformat or parse them.
  See [The type system](foundations/type-system.md).
- **`send_mail` blocks by default.** It now waits for settlement and returns the
  reply; use `fire_and_forget` for a poke or a no-reply cap. (If you've seen it
  described as best-effort fire-and-forget, that's the older behavior — the default
  flipped.)
- **Desktop-only surfaces fail fast.** `capture_frame` and the window ops need the
  desktop chassis; the headless chassis replies with an error rather than hanging.
  To read back a backgrounded or minimized window, mail `aether.window.focus`
  first to foreground it — see [Window](systems/window.md).
- **`describe_component` reads a cache.** It's populated by `load_component` /
  `replace_component`, so describing a component this aether-mcp didn't load — or one
  loaded before an aether-mcp restart — returns an error; reload it.

## Where to read more

- What a mailbox and a kind actually are — [Mail, kinds & scheduling](systems/mail-and-kinds.md).
- The ids and schemas the tools hand around — [The type system](foundations/type-system.md).
- Loading, replacing, and inspecting components — [Components & lifecycle](systems/components.md).
- Settlement and the trace tree behind `send_mail_traced` — [Tracing & settlement](systems/tracing-and-settlement.md).
- The computation DAG the `*_dag` tools drive — [The computation DAG](systems/dag.md).
- The handle store `describe_handles` inspects — [Handles](systems/handles.md).
- Adding your own tool to this surface — [Wiring an MCP tool](recipes/wiring-an-mcp-tool.md).
- The operational reference — ports, env overrides, `restart-hub` — `CLAUDE.md`.
