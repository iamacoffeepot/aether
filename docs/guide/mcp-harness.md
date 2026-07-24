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
the fleet: it forks substrates, assigns each a localhost RPC port, optionally
heartbeats them, and routes your mail to the right one by `engine_id`. A
**substrate** is one running engine — a full chassis — and you can have several
at once.

An engine in `list_engines` is still supervised; that row alone does not prove
every handler is reachable. With heartbeats enabled, a low
`last_heartbeat_age_millis` confirms that the proxy recently answered a ping, and
a rising value is an early sign that it is slow or wedged. A zero heartbeat
interval or miss limit disables that check: the age then grows from initial
connection even for a healthy engine, and the hub learns of death only when the
connection closes. The fleet row does not expose whether heartbeats are enabled,
so confirm hub configuration before using its age as a diagnosis.

To restart the hub after a rebuild *without* losing your session, hit the
tunnel's admin endpoint (`POST /admin/restart-hub`); the tunnel re-forks the hub
and aether-mcp re-dials it on your next call. This is a fleet-wide destructive
operation, not a harmless reconnect: use it only with authority over the whole
fleet and one coordinated request. Restarting aether-mcp itself does drop the
session. Read [Harness lifecycle and fleet-wide mutations](operating/harness-lifecycle.md)
before cycling either process.

## Bringing it up

The stack isn't running by default — a cold build of the tunnel can take long
enough to look like a frozen session, so it's left to the point of use. Bring it up
yourself with `scripts/ensure-tunnel.sh`: it is idempotent and starts the local
stack only when needed. Treat `.codex/config.toml`, `.mcp.json`, the helper, and
the active MCP schema as the current connection contract; do not translate
another agent surface's harness syntax by analogy.

Codex sessions in a trusted checkout pick up the `aether-hub` MCP server from
`.codex/config.toml`; if the `mcp__aether-hub__*` tools are missing after the
tunnel starts, run `/mcp` in the active Codex surface to reconnect them.

## The session loop

Per-engine work is keyed by `engine_id`. A session has a recognizable arc:

1. **Get an engine.** `spawn_substrate(selector)` forks a fresh substrate and
   returns its `engine_id` (and RPC port); omit `selector` for `default` — the
   stored headless chassis normally staged by the tunnel helper. Without that
   artifact, the bare spawn fails selector resolution. `list_engines` shows the
   ones already running. You hand
   that `engine_id` to every later per-engine call; hub artifact operations and
   the MCP-build-static `describe_transforms` do not take one.
2. **Set it up.** Stage the wasm into the hub's component registry with
   `upload_component(staged_path)` — it returns `{hash, name}` — then
   `load_component(engine_id, selector)` resolves that selector and loads the
   component, returning its `mailbox_id`, resolved `name`, and advertised
   capabilities.
3. **Drive it.** `send_mail(…)` delivers a kind to a mailbox. By default it blocks
   until the dispatch chain settles and hands you the correlated reply.
4. **Watch it.** `capture_frame` reads the rendered frame back as a PNG;
   `actor_logs` pulls one actor's log ring; `describe_kinds`,
   `describe_handlers`, and `describe_component` report engine/component
   contracts. `describe_transforms` instead reports the MCP build's static
   transform set.
5. **Settle precisely.** `send_mail_traced` when you need to know a whole causal
   chain finished, with its trace tree, rather than a single reply.
6. **Tear down.** `terminate_substrate(engine_id)` when you're done with an engine.

## The tools

**Fleet.** `list_engines`, `spawn_substrate`, `terminate_substrate`.
`list_engines` and `spawn_substrate` reveal engine ids; `terminate_substrate`
consumes one and returns termination status. Use the id for engine-scoped tools,
not for hub artifact-store or MCP-local queries.

**Sending mail.** `send_mail` is the workhorse. You give it a batch of items, each
`{engine_id, recipient_name, kind_name, params}` — the **mailbox** to deliver to,
the **kind** to deliver, and the structured params, which the tool schema-encodes to
wire bytes against that kind's descriptor. A textual `recipient_name` may be a
canonical lineage (`aether.component/aether.embedded:camera`) or an ADR-0166
abbreviation (`aether.component://camera`). The selected engine resolves either
spelling to the same live mailbox id and canonical path before dispatch;
aether-mcp does not hash operator strings or keep an alias cache. A tagged
`mbx-…` remains direct on tools that accept it. By default each item *blocks* until its
chain settles. The batch-level `replies` projection defaults to `terminal`: it
keeps the last arrival-ordered reply plus any reply recognized as an error from
its decoded `Err` shape or exact kind-name error suffix. Use `none` to suppress
non-errors or `all` for the complete decoded stream; neither explicit mode caps
the stream, and the generic whole-response guard stages an oversized complete
result instead of truncating it. A request/reply (mail `aether.fs.read`, get the
bytes back) is therefore a single call with no polling. Decoded `Bytes` leaves
over 16 KiB stage to a host file before that outer response guard. A handler can
emit no application reply and still settle, so that alone is not a reason to
use `fire_and_forget`. Set it only for dispatch whose completion and ordering
you deliberately do not need; use settled mail or `capture_frame.mails` when a
mutation must precede observation. It returns no replies regardless of the
requested projection. Items are independent: one bad item does not abort its
siblings.

`send_mail_traced` is the same idea with a shared trace root. Every item in the
batch lands under one chassis-level trace root. The settled default returns a
compact one-line-per-node `tree`, a matching `node_count`, and `mails: null`;
each line names `sender → recipient`, kind, and handler duration, with indentation
for causal depth. Pass `full: true` to restore the complete `mails` node values;
that form omits `tree` and carries the same `node_count`. Both forms also carry
the complete flat reply list and rely on the generic response spill rather than
truncating. Reach for it when you
need exact whole-chain settlement — proof that everything a mail set off has finished
— or all-or-nothing dispatch where a single bad item aborts the batch before any mail
moves. For independent items where you just want each reply, plain `send_mail` is the
simpler tool.

**Terrain authoring.** Eight task-level tools wrap the loaded terrain
components while retaining the same live-schema, settled-mail path:
`terrain_marks`, `terrain_editor`, `apply_terrain_brush`,
`run_terrain_automaton`, `propose_terrain_edit`,
`commit_terrain_proposal`, `discard_terrain_proposal`, and
`set_terrain_proposal_preview`. Pass each tool the exact loaded component
mailbox from `LoadResult.name` — `mark_book_mailbox`, `terra_mailbox`, or
`world_mailbox` as applicable. These are normally
`aether.component/aether.embedded:<load-name>` strings; they are not actor
namespaces, tagged mailbox ids, registry selectors, or inferred defaults. Use
`load_component` and `describe_component` to discover and verify them. A loaded
TerraEditor must already carry a `TerraConfig.mark_book_mailbox` pointing to its
MarkBook.

The grouped mark/editor tools select their documented `aether.kit.mark.*` or
`aether.kit.terra.*` request and return the component's exact decoded result.
Immediate brush/automaton tools and the corresponding proposal variants first
validate the supplied revisioned `MarkRef` through the named MarkBook; missing,
stale, or wrong-shaped source marks fail before WorldView mail is sent. Proposal
tools preserve the full staged/preview/commit/discard result vocabulary,
including domain rejections such as `StagedProposalLimitReached`. Task tools
render mark ids as `{value}` records; generic `send_mail` continues to expose
the live codec's newtype shape.

Terrain mailbox arguments use the same direct-mail resolver as `send_mail`.
Canonical loaded lineages and unambiguous ADR-0166 abbreviations therefore
route identically, with liveness checked by the selected engine before the
task request is sent.

**Introspection.** `describe_kinds` is how you learn what to put in `params`. The
default call returns a compact `[{name, shape}]` listing of every kind — a one-line
field rendering per kind, small enough to read in one shot. Start with
`families: true` for a sorted `[{family, count}]` digest; combine it with a
case-sensitive `prefix` to digest one subtree (`full` is ignored in this mode).
Use `names: ["aether.fs.write"]` for exact kinds, then add `full: true` when you
need their nested `SchemaType`. `names` cannot combine with `families` or
`prefix`, and a bare unfiltered `full: true` call is refused so schema output
stays bounded. `describe_component` reports a loaded component's handler kinds,
their docs, whether it has a fallback, and its boot-config kind, addressed by
the component's loaded lineage name or an unambiguous ADR-0166 abbreviation.
`load_component` returns the canonical
`aether.component/aether.embedded:NAME` address. For a boot load, retain the
configured name from the component spec or derive the expected lineage from
that spec; `spawn_substrate` itself returns only engine information. Registry
`list_components` entries describe stored artifacts and are not loaded lineage
addresses.

An engine-scoped `describe_kinds` is currently best-effort: it starts from the
static baseline and attempts a live inventory refresh, but an RPC/decode failure
can leave a prior/static snapshot and still return success. Its full schemas are
exact for the returned snapshot, not proof that the engine was reachable. Pair
freshness-sensitive use with `list_engines` and a harmless bounded live probe.
Handler and component docs default to the first rustdoc line; pass `full: true`
for the complete strings. `describe_handlers` reads the selected engine's
native handler inventory, including reply contracts. `describe_transforms`
lists the native transforms linked into the current `aether-mcp` process; it
does not query an engine.

**Components.** `upload_component` takes the filesystem path to a `.wasm` and
stages it in the hub's component registry. `load_component` and
`replace_component` then take that upload's registry `selector` (hash, name, or
`module@actor`), never a host wasm path or inline wasm bytes. For a typed-config
component, pass either `config` as inline structured JSON or `config_path` as a
path to a JSON file; they are mutually exclusive. The harness schema-encodes the
JSON to the Config kind that `describe_component` identifies; `describe_kinds`
shows its schema. `config_path` does not contain pre-encoded wire bytes.
`load_component` with `replicas: N` returns one shared `capabilities` block plus
`instances: [{mailbox_id, name}, …]` rather than repeating capabilities per
replica; docs on that block also follow the summary-vs-`full` projection.

`list_binaries` and registry `list_components` return
`{entries, total_matched, shown, truncated, notice}` in stable newest-first
first-ingest order. Their default page is the newest 20 name-pointed entries;
pass `include_history: true` for unnamed historical hashes and an explicit
`limit` to change the cap (`0` returns no entries while retaining
`total_matched`). To retrieve a complete matched history, first call with
`include_history: true`, then repeat with `limit` set to that call's
`total_matched`. Component actor `handled_kinds` are readable static kind names
with tagged `knd-…` fallbacks; the redundant manifest-wide handled-kind union
is omitted. These registry rows identify stored wasm, not live component
instances; use the lineage returned by `load_component`, or the known expected
lineage of a boot spec, for `describe_component`.

**Observation.** `capture_frame` returns the engine's current frame as an optional
inline PNG, bounded by a 768-pixel long-edge ceiling by default (never upscaled).
Pass a finite `scale` in `(0, 1]` for proportional reduction, then
`max_dimension` to clamp the scaled long edge; those controls compose in that
order. A capture with `checks` returns the verdict and omits the image by default,
while `include_image` explicitly overrides either default. `save_path` always writes
the original full-resolution PNG bytes. The capture can also carry two mail bundles
dispatched atomically around the readback — `mails` before (the state that should
appear) and `after_mails` after (cleanup). How that frame is produced — world-space
geometry, the camera matrix, the depth convention — is covered in
[Rendering & camera](systems/rendering.md).
`actor_logs` and `actor_cost` resolve textual actor addresses inside the
selected engine before querying its returned mailbox id. `actor_logs` pulls
recent entries from that actor's per-actor log ring; pass `contains` to filter message bodies by a case-sensitive substring
substrate-side, before entries cross the wire. Thread the reply's `next_since`
back as `since` to page forward without re-reading. Only in-actor `tracing::*` events reach a ring — see
[Logging](systems/logging.md) for the in-actor versus stderr boundary.
`actor_cost` reads each actor's per-handler execution-cost EWMA table
(mean and MAD in nanoseconds, plus a sample count); pass a `kind_id` to filter to
one handler.

## Conventions that bite

- **Mailbox vs kind.** `recipient_name` is the mailbox; `kind_name` is the payload.
  They route independently even when they share a prefix — send the kind
  `aether.audio.note_on` to the mailbox `aether.audio`. See
  [Mail, kinds & scheduling](systems/mail-and-kinds.md).
- **Paths, not bytes.** `upload_component` takes the fleet-host filesystem path;
  `load_component` and `replace_component` take registry selectors. Tool JSON
  never carries the wasm buffer itself. Host paths are not sandboxed task paths;
  see [Host paths and artifacts](operating/host-paths-and-artifacts.md).
- **Wire ids are tagged strings.** Mailbox, kind, and handle ids come back as
  `mbx-…`, `knd-…`, `hdl-…` — hand them back verbatim, don't reformat or parse them.
  See [The type system](foundations/type-system.md).
- **The engine resolves textual actor addresses.** Canonical paths and
  `root.namespace://relative` abbreviations are checked against the selected
  engine's declared topology and live registry. Do not derive a mailbox id
  from the spelling.
- **`send_mail` blocks and projects replies by default.** It waits for settlement
  and returns the terminal reply plus recognized errors. Request `replies: "all"`
  when every event matters or `"none"` when only failures matter. A no-reply
  handler still settles normally; reserve `fire_and_forget` for work whose
  completion and ordering are intentionally unobserved. (If you've seen it
  described as best-effort fire-and-forget, that's the older behavior — the default
  flipped.)
- **Desktop-only surfaces fail fast.** `capture_frame` and the window ops need the
  desktop chassis; the headless chassis replies with an error rather than hanging.
  To read back a backgrounded or minimized window, mail `aether.window.focus`
  first to foreground it — see [Window](systems/window.md).
- **`describe_component` resolves names before consulting its cache.** Address
  it by the lineage returned by `load_component`, an unambiguous abbreviation,
  or a retained boot-spec lineage. The selected engine first returns the live
  mailbox id and canonical path; aether-mcp then checks capabilities cached
  under that real id and asks the component host only on a cache miss. A tagged
  `mbx-` id remains cache-only, so that form alone does not prove liveness.
  Registry `list_components`
  rows are stored artifacts, not lineage names. A `mbx-` id is only a local
  cache fast-path and needs a prior `load_component` / `replace_component`.

## Where to read more

- What a mailbox and a kind actually are — [Mail, kinds & scheduling](systems/mail-and-kinds.md).
- The ids and schemas the tools hand around — [The type system](foundations/type-system.md).
- Loading, replacing, and inspecting components — [Components & lifecycle](systems/components.md).
- Settlement and the trace tree behind `send_mail_traced` — [Tracing & settlement](systems/tracing-and-settlement.md).
- Adding your own tool to this surface — [Wiring an MCP tool](recipes/wiring-an-mcp-tool.md).
- Engine ownership, evidence, and recovery — [Operating a live engine](operating/index.md).
- Connection and process ownership — [Process topology and chassis](architecture/process-topology.md).
