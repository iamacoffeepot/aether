# Wiring an MCP tool

**Class:** recompile. You edit aether's Rust and rebuild, so the prereq is the
`cargo` + pre-flight loop, not a running harness — though you'll want the harness
([the MCP harness](../mcp-harness.md)) up at the end to call the tool live.

A capability already speaks mail: it answers some kind on its mailbox and replies
with another. An MCP tool is the adapter that turns that mail surface into
something the agent can call by name, with a JSON schema the client renders live.
This recipe wires one: an args struct the agent fills in, a `#[tool]` method that
builds the wire `Call` and decodes the reply, and the machine-consumer conventions
the JSON surface has to honor.

The seam is two files in `aether-mcp`:

- `crates/aether-mcp/src/args.rs` — the request/response structs the agent sees,
  with their `JsonSchema` doc comments. **The doc comments are the agent-facing
  contract** — the schema your MCP client shows is generated from them.
- `crates/aether-mcp/src/tools/mod.rs` — the `#[tool_router]` block on `impl Mcp`.
  Each `#[tool]` method here is a thin registration: it deserializes
  `Parameters(args)` and hands off to the body in a sibling module under
  `crates/aether-mcp/src/tools/`, wrapping the result in `guard_response_size`.
  The bodies live beside their subject — `logs_cost.rs`, `mail.rs`, `capture.rs`,
  `components.rs`, `describe.rs`, `engine.rs` — so the tool surface reads as a
  file listing rather than one flat file.

An adapter that groups several live component kinds follows the same contract
with one extra boundary. Keep its ergonomic DTOs in `args.rs` and its
task-to-live JSON conversion and settled relay in that module: resolve request
and reply descriptors from the per-engine live kind cache, dispatch through
`MailSpec` / `deliver_one`, and decode through `decode_reply_events`. Do not
link the component crate or copy its Rust wire types into the coordinator.

## The exemplar: `actor_cost`

[`actor_cost`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mcp/src/tools/logs_cost.rs) dumps one actor's per-handler cost
table. It's small but exercises every interesting part: a tagged-id filter
argument, a wire request/reply round-trip, decode of a reply kind into a JSON
response, and error mapping. Read it alongside this page; the steps below name its
real symbols.

The mail surface it fronts is two kinds in `aether-kinds`: the request
`CostTail { kind: Option<KindId> }` (kind name `aether.cost.tail`) and the reply
`CostTailResult::{ Ok { rows }, Err { error } }` (`aether.cost.tail_result`). Every
actor answers `aether.cost.tail` through the framework dispatch arm, so the tool
addresses any mailbox by name.

## Step 1 — the args and response structs (`args.rs`)

Declare what the agent sends and what comes back. Both derive the schema; the
request derives `Deserialize` (the agent fills it), the responses derive
`Serialize` (you hand them back).

```rust
/// `actor_cost` arguments — dump one actor's per-handler
/// execution-cost EWMA table. Measure-only — no scheduling effect.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActorCostArgs {
    /// Engine UUID to pull from (from `list_engines`). Omit to target the
    /// sole supervised engine; with zero or several engines an omitted id
    /// is an error naming the situation, never a guess.
    #[serde(default)]
    pub engine_id: Option<String>,
    /// Address of the actor to query (e.g. `"aether.audio"`,
    /// `"aether.component/aether.embedded:camera"`, or a tagged `mbx-…` id).
    pub address: String,
    /// Optional kind-id filter (tagged `knd-XXXX-XXXX-XXXX` or raw
    /// decimal). Omitted dumps every handler row the actor declares.
    #[serde(default)]
    pub kind_id: Option<String>,
}

/// `actor_cost` response. `rows` is one [`ActorCostRow`] per handler
/// the queried actor declares (filtered to `kind_id` when set).
#[derive(Debug, Serialize, JsonSchema)]
pub struct ActorCostResponse {
    pub engine_id: String,
    pub address: String,
    pub rows: Vec<ActorCostRow>,
}
```

What the conventions buy you here:

- **Tagged ids are `String`, not the typed newtype.** `engine_id` and `kind_id`
  arrive as strings the agent pastes back from a prior call (`list_engines`,
  `describe_kinds`), and you parse them in the tool body. JSON has no `KindId`.
- **`engine_id` is `Option<String>`, and the response echoes the resolved one.**
  Every engine-taking tool resolves it the same way, so an auto-resolved answer
  says which engine produced it.
- **The recipient argument is `address`.** One spelling across the whole tool
  surface, accepting a canonical lineage, an ADR-0166 abbreviation, or a tagged
  `mbx-…` id.
- **`Option` + `#[serde(default)]` for every optional field**, with the doc
  comment stating what omitting it means. The agent reads the schema; spell the
  default behavior out rather than leaving it implicit.
- **The response struct mirrors the reply kind, rendered for JSON.**
  `ActorCostRow` carries `kind_id: String` (a tagged string), not the wire
  `KindId` — the same id-as-string rule, now on the way out.

## Step 2 — the registration and the body

The registration lives on `impl Mcp` inside the `#[tool_router]` block in
`tools/mod.rs`. The `#[tool]` attribute registers it and uses the `description`
string plus the args struct's schema as the surface the agent sees;
`Parameters(args)` unwraps the deserialized request, and the method delegates
straight to its module:

```rust
#[tool(
    description = "Dump one actor's per-handler execution-cost EWMA table. \
                   Sends aether.cost.tail to the addressed mailbox and decodes \
                   aether.cost.tail_result. MEASURE-ONLY ..."
)]
pub async fn actor_cost(&self, Parameters(args): Parameters<ActorCostArgs>) -> Result<String, McpError> {
    guard_response_size("actor_cost", logs_cost::actor_cost(self, args).await)
}
```

The body is a free function in `tools/logs_cost.rs`:

```rust
pub(super) async fn actor_cost(mcp: &Mcp, args: ActorCostArgs) -> Result<String, McpError> {
    let (engine, engine_id) = mcp.resolve_engine(args.engine_id.as_deref()).await?;
    let address = args.address.clone();

    // Parse the tagged id back into the wire newtype.
    let kind = match args.kind_id.as_deref() {
        Some(s) => Some(parse_kind_id(s)?),
        None => None,
    };

    // Build the typed request, resolve the recipient the agent named,
    // address it by id, and await the reply.
    let request = CostTail { kind };
    let (mailbox_id, _) = mcp.resolve_engine_address(engine, &args.address).await.map_err(internal)?;
    let reply = mcp
        .session
        .call_one(engine_envelope_by_id(engine, mailbox_id, &request))
        .await
        .map_err(internal)?;

    // Decode the reply kind and shape it for JSON.
    match CostTailResult::decode_from_bytes(&reply.payload) {
        Some(CostTailResult::Ok { rows }) => {
            let response = ActorCostResponse {
                engine_id,
                address,
                rows: rows.into_iter().map(/* CostRow -> ActorCostRow */).collect(),
            };
            json(&response)
        }
        Some(CostTailResult::Err { error }) => {
            Err(internal_msg(&format!("actor_cost: {} — {error}", args.address)))
        }
        None => Err(internal_msg("undecodable CostTailResult")),
    }
}
```

The skeleton every tool follows:

1. **Resolve the engine and parse the string ids up front** —
   `mcp.resolve_engine(args.engine_id.as_deref())` returns the wire id plus the
   string to echo, and `parse_kind_id` / `parse_mailbox_id` return
   `McpError::invalid_params` on a malformed id, so a bad id is rejected before
   any mail moves.
2. **Build the typed request kind, then resolve the recipient before you
   address it.** An address the agent typed is often a rendered lineage —
   `aether.component/aether.embedded:web`, the form `load_component`
   hands back — which is a path of nodes rather than one name to hash.
   `mcp.resolve_engine_address(engine, address)` takes whichever form arrives: a
   tagged `mbx-…` parses locally, and anything else goes to the engine's
   inventory cap, which folds the path and replies with the id plus its
   canonical rendering. Pass that id to `engine_envelope_by_id(engine,
   mailbox_id, &request)`, which stamps `K::ID` and encodes the payload;
   `mcp.session.call_one(...)` relays it as a wire `Call` and awaits the
   correlated reply. The by-name `engine_envelope(engine, name, &request)`
   hashes its name as a single segment, so it is for the fixed chassis-cap
   constants a tool writes itself (`INVENTORY_CAP`, `RENDER_CAP`,
   `COMPONENT_CAP`) and never for a name that came in over the tool surface.
3. **Decode the reply** with `CostTailResult::decode_from_bytes(&reply.payload)`,
   matching the kind's own variants — `Ok` becomes the JSON response, `Err` and an
   undecodable payload become `McpError`s.
4. **Render ids for the way out.** Each row's `KindId` goes back through
   `tagged_id::encode` so the agent receives `knd-…` strings, not raw integers
   ([ADR-0064](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0064-type-tagged-opaque-ids-on-the-mcp-wire.md)).
5. **`json(&response)`** serializes the struct to the string `rmcp` wraps as the
   tool's text content, and the registration wraps that in `guard_response_size`
   so an oversized response spills to a host file instead of flooding the tool
   channel.

## The conventions checklist

These exist for the machine consumer. A tool that skips them compiles and still
mis-serves the agent.

- **Paths, not byte buffers.** If a tool needs file content (a wasm, a payload),
  the argument is a `String` path the harness reads — tool JSON never carries a
  byte buffer. `actor_cost` has no payload arg, but `upload_component` /
  `upload_binary`'s `staged_path` is the rule — `load_component` itself takes a
  registry `selector`, not a path.
- **Ids cross the wire as tagged strings, parsed at the edges.** In via
  `parse_*` (rejecting a malformed id as `invalid_params`), out via
  `tagged_id::encode`. The agent only ever sees `mbx-…` / `knd-…` / `hdl-…` and
  hands them back verbatim ([ADR-0064](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0064-type-tagged-opaque-ids-on-the-mcp-wire.md)).
- **Explicit nulls, not absent fields.** An optional argument is `Option<T>` with
  `#[serde(default)]`; an optional reply field is `Option<T>` that serializes to
  `null`. The agent reads a present `null` as a decision; a missing key reads as a
  question. Spell out in the doc comment what each null means.
- **Cheap synchronous verdict, async execution polled separately.** When the op is
  slow, the tool returns immediately with a cheap result (a validation verdict, an
  id to poll) and the work runs in the background, queried by a second tool.
  `actor_cost`, by contrast, is a fast read, so it answers in one call.
- **The description and the doc comments are the contract.** The agent picks and
  fills the tool from its schema alone. Write the `description` and every field's
  doc comment as the instructions they are — state the units, the defaults, and
  where each value comes from.

## Verify it live

After the build, bring the harness up and call the tool against a real engine:

1. `scripts/ensure-tunnel.sh` — start the tunnel + aether-mcp + hub (idempotent).
2. `spawn_substrate` a chassis, then `load_component` something with handlers (or
   just query a chassis mailbox like `"aether.render"`).
3. Call your new tool. Confirm the args schema renders as you wrote it and the
   response shape matches.
4. Cross-check the kind names against `describe_kinds` — the request and reply your
   tool sends should be the ones the static vocabulary lists.

## Verify against current code

This recipe names live symbols — `ActorCostArgs`, `Mcp::actor_cost`, `CostTail` /
`CostTailResult`, `resolve_engine`, `resolve_engine_address`, `engine_envelope`,
`parse_kind_id`, `guard_response_size`, `tagged_id::encode`. Before
following it, confirm they still exist in `crates/aether-mcp/src/args.rs` and
`crates/aether-mcp/src/tools/`
and `crates/aether-kinds/src/lib.rs`; if a name has moved, fix the recipe as part
of your change.
