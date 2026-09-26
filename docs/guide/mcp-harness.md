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
  └─ :8901  aether-hub  — supervises the fleet
        ├─ substrate (engine A)   — one running chassis
        ├─ substrate (engine B)
        └─ …
```

The **tunnel** is the only thing your MCP client talks to. It supervises and
re-forks the two backends below it, which is the point: you can rebuild and
restart the hub without your MCP session ever dropping. **aether-mcp** is the RPC
client — it turns a tool call into a wire `Call` and relays it. The **hub** owns
the fleet: it forks substrates, learns the localhost RPC port each one picks
and reports, optionally heartbeats them, and routes your mail to the right one by `engine_id`. A
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
yourself with `scripts/ensure-tunnel.sh`: it is idempotent — a no-op only when the running tunnel's supervised children are both alive — and starts the local
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
   component, returning its canonical lineage `address` and advertised
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
`{engine_id, address, kind_name, params}` — the **mailbox** to deliver to,
the **kind** to deliver, and the structured params, which the tool schema-encodes to
wire bytes against that kind's descriptor. A textual `address` may be a
canonical lineage (`aether.component/aether.embedded:camera`) or an ADR-0166
short path (`aether.component/:camera`). The selected engine resolves either
spelling to the same canonical path before dispatch, and the `Call` names its
recipient by that `ActorPath`; aether-mcp does not hash operator strings or keep
an alias cache. A tagged `mbx-…` id, on tools that accept it, is sent to the
selected engine's `aether.inventory.resolve` for its canonical path, and the
mail goes by that path. By default each item *blocks* until its
chain settles. The batch-level `replies` projection defaults to `terminal`: it
keeps the last arrival-ordered reply plus any reply recognized as an error from
its decoded `Err` shape or exact kind-name error suffix. Use `none` to suppress
non-errors or `all` for the complete decoded stream; neither explicit mode caps
the stream, and the generic whole-response guard stages an oversized complete
result instead of truncating it. A request/reply (mail `aether.fs.read`, get the
bytes back) is therefore a single call with no polling. Decoded `Bytes` leaves
over `AETHER_MCP_REPLY_INLINE_MAX_BYTES` (default 16 KiB) stage to a host file and
render as `{"file": …}` before that outer response guard, which stages any
text response over `AETHER_MCP_RESPONSE_INLINE_MAX_BYTES` (default 32 KiB) as
`{file, bytes, summary}`. A handler can
emit no application reply and still settle, so that alone is not a reason to
use `fire_and_forget`. Set it only for dispatch whose completion and ordering
you deliberately do not need; use settled mail or `capture_frame.mails` when a
mutation must precede observation. It returns no replies regardless of the
requested projection. Items are independent: one bad item does not abort its
siblings.

A byte field in `params` takes its literal JSON form or one `$`-sigil embed
object. A `Bytes` or `Blob` field takes a byte array or `{"$file": path}` (a
file on the harness host), `{"$base64": s}`, `{"$text": s}` (UTF-8), or
`{"$hex": s}`. `$hex` also writes two leaf types the others do not: a
`[u8; N]` field, such as a 32-byte digest, takes exactly `2 * N` characters,
two per byte in index order; an integer field takes two characters per byte of
its type, most significant digit first, with a signed value spelled as its
two's-complement bit pattern (`u32` 300 is `"0000012c"`, `i8` -1 is `"ff"`).
Hex is lowercase `0-9a-f` with no prefix; uppercase, a `0x` prefix, and a wrong
length are refused, so every value has one spelling. A `u32` and a `[u8; 4]`
holding the same wire bytes spell differently, because each follows how its
own type reads. A function on a field type outside its set, such as `$base64`
at a `[u8; 32]` or `$hex` at a `String`, is refused, naming the function and
the field type. The same embeds work in `send_mail_traced`, the
`capture_frame` and `spawn_substrate` mail bundles, and component `config`.

`format` renders chosen reply leaves in that same hex spelling. It is an
object keyed by exact reply kind name, each value a mask that mirrors the JSON
the kind decodes to: `"$hex"` at a `[u8; N]`, `Bytes`, `Blob`, or integer leaf;
an object over struct field names or over the names of enum variants that
carry a payload (a one-field tuple variant takes its field's mask directly, a
struct variant an object over its fields); a one-element array `[mask]` for
every element of a `Vec` or array, or every value of a `Map`; and, for a tuple
variant with two or more fields, an array of that length where `null` leaves a
position alone. `Option` layers are transparent. Over a `[u8; N]`, `"$hex"`
spells the whole array as one string, while `["$hex"]` spells each byte on its
own.

```json
{"aether.bloomery.journal.publish_result": {"Committed": {"artifacts": ["$hex"]}}}
```

The sole key `"*"` with a function, `{"*": "$hex"}`, formats every
`[u8; N]`, `Bytes`, and `Blob` leaf in every reply, and leaves integers alone.
The mask is validated against each item's engine before any mail is sent: an
unknown kind, a key the kind's schema lacks, a unit variant, a function on a
leaf outside its set, or `"*"` beside another key refuses the whole call,
naming the kind and the JSON path. A reply of a kind the mask does not name
renders as it does without one. Formatting runs on each decoded reply before
the per-leaf `Bytes` spill, so a formatted leaf is already a string and stays
inline; the 32 KiB whole-response guard still applies to the result. A mask
only re-spells leaves: it never renames, drops, or adds a field, so the
`replies` projection recognizes errors the same way.

`send_mail_traced` is the same idea with a shared trace root. Every item in the
batch lands under one chassis-level trace root. The settled default returns a
compact one-line-per-node `tree`, a matching `node_count`, and `mails: null`;
each line names `sender → recipient`, kind, and handler duration, with indentation
for causal depth. Pass `trace: "nodes"` to restore the complete `mails` node values;
that form omits `tree` and carries the same `node_count`. Both forms also carry
the complete flat reply list and rely on the generic response spill rather than
truncating. Its `format` is the same reply mask as `send_mail`'s, applied to
that reply list and validated before the batch is encoded; a string `format`,
such as the retired `"nodes"`, is refused. Reach for it when you
need exact whole-chain settlement — proof that everything a mail set off has finished
— or all-or-nothing dispatch where a single bad item aborts the batch before any mail
moves. For independent items where you just want each reply, plain `send_mail` is the
simpler tool.

**Introspection.** `describe_kinds` is how you learn what to put in `params`. The
default call returns a compact `[{name, shape}]` listing of every kind — a one-line
field rendering per kind, small enough to read in one shot. Start with
`families: true` for a sorted `[{family, count}]` digest; combine it with a
case-sensitive `prefix` to digest one subtree (`detail` is ignored in this mode).
Compact enum shapes use the externally tagged JSON envelope that `send_mail`
accepts: a unit variant is a quoted string such as `"Windowed"`, a tuple
variant is a one-key object such as `{ "Ok": value }` (or an array body for
multiple fields), and a struct variant is a one-key object such as
`{ "Err": { reason: String } }`. When an enum appears inside a struct, that
notation remains inside its named field; it does not become a top-level key.
Use `names: ["aether.fs.write"]` for exact kinds, then add `detail: "schema"` when you
need their nested `SchemaType`. `names` cannot combine with `families` or
`prefix`, and a bare unfiltered `detail: "schema"` call is refused so schema output
stays bounded. `describe_component` reports a loaded component's handler kinds,
their docs, whether it has a fallback, and its boot-config kind, addressed by
the component's loaded lineage name or an unambiguous ADR-0166 short path.
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
native handler inventory, including reply contracts: each handler carries
`reply_class` (`none`, `one`, or `manual`), and `reply_id` / `reply_name` are
set only for `one`. `describe_transforms`
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
`instances: [{address}, …]` rather than repeating capabilities per
replica; docs on that block also follow the summary-vs-`full` projection.
`replace_component` names its target by lineage address (canonical or short),
sends it to the engine as the `aether.component.replace` target, and prints the
address back; a tagged `mbx-…` address is refused with a pointer to the lineage
address.

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

**Observation.** `capture_frame` returns one window's current frame as an
optional inline PNG, bounded by a 768-pixel long-edge ceiling by default (never
upscaled). `window_id` is required: pass the tagged `mbx-…` string
`aether.window.list` reports, since capture never guesses a window.
Pass a finite `scale` in `(0, 1]` for proportional reduction, then
`max_dimension` to clamp the scaled long edge; those controls compose in that
order. A capture with `checks` returns the verdict and omits the image by default,
while `include_image` explicitly overrides either default. `save_path` always writes
the original full-resolution PNG bytes. The capture can also carry two mail bundles
dispatched atomically around the readback — `mails` before (the state that should
appear) and `after_mails` after (cleanup). How that frame is produced — world-space
geometry, the camera matrix, the depth convention — is covered in
[Rendering & camera](systems/rendering.md).
`actor_logs` and `actor_cost` resolve the actor address, text or a tagged
`mbx-…` id, inside the selected engine to its canonical path and query by that
path. `actor_logs` pulls
recent entries from that actor's per-actor log ring; pass `contains` to filter message bodies by a case-sensitive substring
substrate-side, before entries cross the wire. Thread the reply's `next_since`
back as `since` to page forward without re-reading. Only in-actor `tracing::*` events reach a ring — see
[Logging](systems/logging.md) for the in-actor versus stderr boundary.
`actor_cost` reads each actor's per-handler execution-cost EWMA table
(mean and MAD in nanoseconds, plus a sample count); pass a `kind_id` to filter to
one handler.

## Conventions that bite

- **Mailbox vs kind.** `address` is the mailbox; `kind_name` is the payload.
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
  short paths such as `root/:name` are checked against the selected
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
  Before reading back a backgrounded or minimized window, mail
  `aether.window.focus` to a named child recipient such as
  `aether.window/:main` (canonical
  `aether.window/aether.window.instance:main`). Success acknowledges the focus
  request; the OS may decline it or apply it asynchronously, so it does not
  prove the window is already foregrounded. See [Window](systems/window.md).
- **`describe_component` resolves names before consulting its cache.** Address
  it by the lineage returned by `load_component`, an unambiguous short path,
  or a retained boot-spec lineage. The selected engine first returns the live
  canonical path; aether-mcp then checks capabilities cached under that engine
  and path and asks the component host only on a cache miss. A tagged `mbx-`
  address is refused with a pointer to the lineage address. Registry
  `list_components` rows are stored artifacts, not lineage names.

## Where to read more

- What a mailbox and a kind actually are — [Mail, kinds & scheduling](systems/mail-and-kinds.md).
- The ids and schemas the tools hand around — [The type system](foundations/type-system.md).
- Loading, replacing, and inspecting components — [Components & lifecycle](systems/components.md).
- Settlement and the trace tree behind `send_mail_traced` — [Tracing & settlement](systems/tracing-and-settlement.md).
- Adding your own tool to this surface — [Wiring an MCP tool](recipes/wiring-an-mcp-tool.md).
- Engine ownership, evidence, and recovery — [Operating a live engine](operating/index.md).
- Connection and process ownership — [Process topology and chassis](architecture/process-topology.md).
