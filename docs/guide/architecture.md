# Architecture overview

This page is the map. It names the pieces, shows how they stack, and traces a
single piece of mail from an agent's tool call to a handler and back. Each
subsystem has its own explainer in [The systems](systems.md); here we only
establish where everything sits.

## The stack

```
┌──────────────────────────────────────────────────┐
│  agent: Claude in a harness                      │
└────────────────────────┬─────────────────────────┘
                         │ MCP (tool calls)
┌────────────────────────v─────────────────────────┐
│  aether-tunnel        stable MCP front (:8890)   │   ADR-0089
│   |- aether-mcp       dials the hub   (:8891)    │
│   `- aether-substrate-hub  RPC server (:8901)    │   ADR-0034
└────────────────────────┬─────────────────────────┘
                         │ wire Call / MailFrame
┌────────────────────────v─────────────────────────┐
│  substrate (a chassis)                           │   ADR-0035/0073
│  ┌────────────────────────────────────────────┐  │
│  │ mail scheduler  (blob dispatch)            │  │   ADR-0087
│  ├────────────────────────────────────────────┤  │
│  │ native capabilities (actors)               │  │   ADR-0070/0071
│  │  render, audio, fs, input, window          │  │
│  │  component-loader, handle-store, dag       │  │
│  ├────────────────────────────────────────────┤  │
│  │ wasm runtime -> components (actors)        │  │   ADR-0010/0074
│  │ aether.component/aether.embedded:NAME      │  │
│  └────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────┘
```

Everything inside the substrate is an **actor**, and actors only ever talk by
**mail**. The native capabilities and the wasm components are the *same actor
model* ([ADR-0074](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0074-unified-actor-model-for-substrate-and-guests.md)) — the renderer is an actor, your component is an actor, and
they address each other identically.

## The crates

Two layers: infrastructure (describes and moves typed bytes) and runtime
(hosts and runs actors).

**Infrastructure — non-actor:**

| Crate | Role |
|---|---|
| `aether-data` | The universal data layer (`no_std`). Typed-id newtypes (`MailboxId`, `KindId`, `HandleId`), wire identity, the schema vocabulary (`SchemaType`, `KindShape`), the `Kind` / `Schema` traits, `Ref<K>`, encode/decode, and the native descriptor/transform inventories. Everything that describes typed bytes depends on it. |
| `aether-codec` | Schema-driven JSON ↔ wire bytes (`encode_schema` / `decode_schema`) plus postcard stream framing ([ADR-0072](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0072-fold-hub-protocol-into-codec-and-hub.md)). |
| `aether-kinds` | The substrate kind vocabulary — `Tick`, `Key`, `WindowSize`, `DrawTriangle`, and the `aether.{audio,fs,render,window,input,component,camera,log,handle,dag}.*` families. |
| `aether-math` | `Vec2/3/4`, `Mat4`, `Quat`, `Aabb` — column-major, right-handed Y-up, `f32`, `no_std`. Reach here before hand-rolling vector math. |

**Runtime + chassis ([ADR-0073](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0073-substrate-cluster-consolidation.md)):**

| Crate | Role |
|---|---|
| `aether-substrate` | The shared runtime: mail scheduler, wasm host, the actor machinery. |
| `aether-capabilities` | Native capabilities (the chassis actors): render, audio, fs, input, component-loader, handle store, etc. |
| `aether-substrate-bundle` | The four chassis as submodules (`desktop` / `headless` / `hub` / `test_bench`) with one binary each, plus the hub library and its wire vocabulary. |
| `aether-actor` | The guest/actor SDK: the `Actor` / `WasmActor` traits, `Mailbox<K>`, `WasmCtx`, the `#[actor]` macro, and `export!`. |

**The harness (out-of-process, [ADR-0089](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0089-mcp-hub-lifecycle-tunnel.md)):** `aether-tunnel` (the stable MCP
front that supervises and re-forks the volatile backends), `aether-mcp` (the
RPC client that relays each tool call as a wire `Call`), and the hub inside
`aether-substrate-bundle`.

## Anatomy of a piece of mail

A *kind* is a typed payload shape; a *mailbox* is an address. They are
**independent**: you send the kind `aether.audio.note_on` to the mailbox
`aether.audio`. They often share a prefix but route separately — this trips up
everyone once, so internalize it early.

Trace one `send_mail` from the agent:

1. **Tool call.** The agent calls `send_mail` with
   `{engine_id, recipient_name, kind_name, params}`. `aether-mcp` looks up the
   kind's schema and **encodes `params` (JSON) into wire bytes** against the
   substrate vocabulary ([ADR-0007](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0007-schema-driven-mail-at-hub.md)), producing a wire `Call`.
2. **Route to the engine.** The hub forwards the `Call` to the right substrate
   over the channel ([ADR-0034](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0034-hub-as-substrate.md)/[ADR-0037](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0037-mail-bubbles-up-to-hub-substrate.md)). The ids on the wire are type-tagged
   strings (`mbx-…`, `knd-…`), so nothing is guessed ([ADR-0064](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0064-type-tagged-opaque-ids-on-the-mcp-wire.md)/[ADR-0065](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0065-typed-id-newtypes-and-first-class-type-ids-in-the-schema.md)).
3. **Schedule.** The substrate's scheduler hands the mail to the addressed
   actor as part of a *blob* of work ([ADR-0087](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0087-blob-unit-of-dispatch.md)). Each actor is single-threaded
   from its own perspective — no locks in actor state.
4. **Decode + dispatch.** The actor decodes the bytes back into the kind and
   the `#[actor]`-generated dispatch table routes it to the handler whose
   third parameter is that kind ([ADR-0033](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0033-handler-driven-inputs-manifest.md)). No typelist, no `is::<K>()`.
5. **Effects and replies.** The handler does its work — maybe sending more
   mail (`ctx.actor::<RenderCapability>().send(&kind)`), maybe replying. Mail
   is **fire-and-forget unless a reply kind is part of the contract**; a
   handler promises nothing about a reply on its own.

Two cross-cutting systems watch this flow without it opting in:

- **Tracing & settlement** ([ADR-0080](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0080-substrate-mail-tracing-and-settlement.md)/[ADR-0086](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0086-decouple-settlement-from-trace.md)): a traced batch shares a trace root
  and the agent gets the full subtree once the chain *settles* — no polling, no
  window-guessing. Settlement is exact via a hold contract ([ADR-0093](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0093-hold-until-resolve-dispatch-primitive.md)/[ADR-0094](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0094-settlement-obligation-guard.md)).
- **Per-actor logging** ([ADR-0081](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0081-decentralized-per-actor-log-storage.md)): each actor has its own log ring, queryable
  by mailbox name through `actor_logs`.

## How an agent reaches all this

The agent never touches the substrate directly. It calls MCP tools
(`mcp__aether-hub__*`) — `spawn_substrate`, `load_component`, `send_mail`,
`capture_frame`, `submit_dag`, and the introspection trio (`describe_kinds`,
`describe_component`, `describe_transforms`). The tunnel keeps the MCP session
alive across hub restarts, so the agent can rebuild and relaunch the engine
mid-task without re-initialising. The full tool list and the operational
details live in `CLAUDE.md`; the [reference](reference.md) page points there.

From here, the [subsystem map](systems.md) takes each box in the stack and
explains what it's for, what you mail it, and how to extend it.
