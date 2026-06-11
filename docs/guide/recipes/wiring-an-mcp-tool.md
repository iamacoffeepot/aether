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
- `crates/aether-mcp/src/tools.rs` — the `#[tool]` method on `impl Mcp`: parse the
  args, build the envelope, await the reply, decode it, map errors.

## The exemplar: `actor_cost`

[`actor_cost`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-mcp/src/tools.rs) dumps one actor's per-handler cost
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
    /// Engine UUID to pull from (from `list_engines`).
    pub engine_id: String,
    /// Mailbox name of the actor to query (e.g. `"aether.audio"`,
    /// `"aether.component/aether.embedded:camera"`).
    pub mailbox_name: String,
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
    pub mailbox_name: String,
    pub rows: Vec<ActorCostRow>,
}
```

What the conventions buy you here:

- **Tagged ids are `String`, not the typed newtype.** `engine_id` and `kind_id`
  arrive as strings the agent pastes back from a prior call (`list_engines`,
  `describe_kinds`), and you parse them in the tool body. JSON has no `KindId`.
- **`Option` + `#[serde(default)]` for every optional field**, with the doc
  comment stating what omitting it means. The agent reads the schema; spell the
  default behavior out rather than leaving it implicit.
- **The response struct mirrors the reply kind, rendered for JSON.**
  `ActorCostRow` carries `kind_id: String` (a tagged string), not the wire
  `KindId` — the same id-as-string rule, now on the way out.

## Step 2 — the `#[tool]` method (`tools.rs`)

The method lives on `impl Mcp` inside the `#[tool_router]` block. The `#[tool]`
attribute registers it and uses the `description` string plus the args struct's
schema as the surface the agent sees; `Parameters(args)` unwraps the deserialized
request.

```rust
#[tool(
    description = "Dump one actor's per-handler execution-cost EWMA table. \
                   Sends aether.cost.tail to the named mailbox and decodes \
                   aether.cost.tail_result. MEASURE-ONLY ..."
)]
pub async fn actor_cost(
    &self,
    Parameters(args): Parameters<ActorCostArgs>,
) -> Result<String, McpError> {
    let engine = parse_engine_id(&args.engine_id)?;
    let engine_id_str = args.engine_id.clone();
    let mailbox_name = args.mailbox_name.clone();

    // Parse the tagged id back into the wire newtype.
    let kind = match args.kind_id.as_deref() {
        Some(s) => Some(parse_kind_id(s)?),
        None => None,
    };

    // Build the typed request, address it, and await the reply.
    let request = CostTail { kind };
    let reply = self
        .session
        .call_one(engine_envelope(engine, &args.mailbox_name, &request))
        .await
        .map_err(internal)?;

    // Decode the reply kind and shape it for JSON.
    match CostTailResult::decode_from_bytes(&reply.payload) {
        Some(CostTailResult::Ok { rows }) => {
            let response = ActorCostResponse {
                engine_id: engine_id_str,
                mailbox_name,
                rows: rows.into_iter().map(/* CostRow -> ActorCostRow */).collect(),
            };
            json(&response)
        }
        Some(CostTailResult::Err { error }) => {
            Err(internal_msg(&format!("actor_cost: {mailbox_name} — {error}")))
        }
        None => Err(internal_msg("undecodable CostTailResult")),
    }
}
```

The skeleton every tool follows:

1. **Parse the string ids up front** — `parse_engine_id`, `parse_kind_id`,
   `parse_mailbox_id`. These return `McpError::invalid_params` on a malformed id, so
   a bad id is rejected before any mail moves.
2. **Build the typed request kind** and address it with `engine_envelope(engine,
   mailbox_name, &request)` — that hashes the mailbox name to a `MailboxId`, stamps
   `K::ID`, and encodes the payload. `self.session.call_one(...)` relays it as a
   wire `Call` and awaits the correlated reply.
3. **Decode the reply** with `CostTailResult::decode_from_bytes(&reply.payload)`,
   matching the kind's own variants — `Ok` becomes the JSON response, `Err` and an
   undecodable payload become `McpError`s.
4. **Render ids for the way out.** Each row's `KindId` goes back through
   `tagged_id::encode` so the agent receives `knd-…` strings, not raw integers
   ([ADR-0064](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0064-type-tagged-opaque-ids-on-the-mcp-wire.md)).
5. **`json(&response)`** serializes the struct to the string `rmcp` wraps as the
   tool's text content.

## The conventions checklist

These exist for the machine consumer. A tool that skips them compiles and still
mis-serves the agent.

- **Paths, not byte buffers.** If a tool needs file content (a wasm, a payload),
  the argument is a `String` path the harness reads — tool JSON never carries a
  byte buffer. `actor_cost` has no payload arg, but `load_component`'s
  `binary_path` is the rule.
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
  id to poll) and the work runs in the background, queried by a second tool. The
  `submit_dag` → `dag_status` / `dag_cancel` split is the worked example;
  `actor_cost` is a fast read, so it answers in one call.
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
`CostTailResult`, `engine_envelope`, `parse_kind_id`, `tagged_id::encode`. Before
following it, confirm they still exist in `crates/aether-mcp/src/{args,tools}.rs`
and `crates/aether-kinds/src/lib.rs`; if a name has moved, fix the recipe as part
of your change.
