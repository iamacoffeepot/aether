# Adding a substrate kind

**Class:** recompile. You edit aether's Rust and rebuild, so the loop is
`cargo` plus the pre-flight (`scripts/preflight.sh`). No running engine is
required to land the kind; the MCP harness is handy for the final
`describe_kinds` confirmation.

Adding a substrate kind is how the engine's native vocabulary grows: a new
message shape the hub can schema-encode from agent params and route to a
chassis mailbox. The dance is one declaration plus a handler — the descriptor
that puts the kind on the MCP-visible wire registers itself.

> **Verify against current code first.** This recipe names files and symbols,
> and they drift. Before you follow it, confirm the exemplar below still
> compiles as described — grep the cited symbols, and if one has moved, fix the
> recipe as part of your change.

## The exemplar

`aether.window.focus` is the worked example: a unit-payload request paired with
an `Ok` / `Err` reply, handled on desktop and `Err`-replied on the
window-less chassis. Trace it end to end through these four files:

- **Declaration** — `crates/aether-kinds/src/lib.rs`: `FocusWindow` and its
  reply `FocusWindowResult`.
- **Desktop handler** — `crates/aether-substrate-bundle/src/desktop/driver.rs`:
  `apply_window_focus`.
- **Window-less handler** — `crates/aether-capabilities/src/window/mod.rs`:
  `HeadlessWindowCapability::on_focus`.
- **Coverage guard** — `crates/aether-kinds/src/descriptors.rs`:
  `covers_every_substrate_kind`.

## 1. Declare the type

A kind is a Rust type carrying a name and a schema. Put it in the module that
owns its family inside `aether-kinds` (the window kinds live alongside
`SetWindowMode` and `SetWindowTitle`), and derive the full set:

```rust
/// `aether.window.focus` — bring the substrate window to the foreground.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.window.focus")]
pub struct FocusWindow {}
```

- `#[kind(name = "…")]` is the wire name agents send as `kind_name`. Follow the
  `aether.<family>.<verb>` convention so the kind sorts with its peers.
- `Kind` makes it a top-level, addressable payload (`const NAME`, `const ID`,
  encode/decode); `Schema` describes its shape so the wire layer can encode it
  from JSON. Every kind is a schema, so both derives are always present. The
  shape contract is in [The type system](../foundations/type-system.md).
- A doc comment on the type is not decoration: `describe_kinds` surfaces it to
  the agent driving the engine, so write it for that reader.

If the kind expects an answer, declare the reply kind too. Reply contracts are
the handler's business, not a property of the request — there is no
`Kind::REPLY` link, so a reply is just another kind the handler chooses to send
back. `aether.window.focus` pairs with an `Ok` / `Err` enum:

```rust
/// Reply to `FocusWindow`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.window.focus_result")]
pub enum FocusWindowResult {
    Ok,
    Err { error: String },
}
```

## 2. Registration is automatic

The descriptor that ships the kind to the hub at the `Hello` handshake — and so
puts it on the `send_mail` / `describe_kinds` surface — registers itself. The
`Kind` derive emits a `cfg(not(target_arch = "wasm32"))`-gated
`inventory::submit!` of a `DescriptorEntry`, and `descriptors::all()` in
`crates/aether-kinds/src/descriptors.rs` materializes the list by iterating that
inventory slot. Adding a kind is one place: the struct definition with its
derives. There is no manual descriptor `vec!` to append to.

What you *do* add is a line to the coverage guard. `covers_every_substrate_kind`
in `descriptors.rs` asserts each substrate kind name is present, so the linker
stripping the per-kind submission static fails the test instead of booting an
engine with a silently missing kind:

```rust
assert!(names.contains(&FocusWindow::NAME));
```

## 3. Handle it

A kind with no handler routes to "kind not found." Pick the mailbox that owns
the behaviour and add a handler for the new kind there. `aether.window.focus`
lands on the `aether.window` mailbox, which two actors claim depending on the
chassis:

- **Desktop** — the chassis driver owns `aether.window` directly (window
  mutations need the winit main thread). `apply_window_focus` in
  `desktop/driver.rs` un-minimizes, shows, and raises the window, then replies
  `FocusWindowResult::Ok`.
- **Headless / test-bench** — `HeadlessWindowCapability::on_focus` in
  `aether-capabilities/src/window/mod.rs` replies `FocusWindowResult::Err`, so an
  MCP caller on a window-less chassis fails fast instead of hanging on a reply
  that never comes.

A handler that answers sends its reply through the context's mailer to the
recorded reply target:

```rust
ctx.mailer().send_reply(ctx.reply_target(), &FocusWindowResult::Ok);
```

The recipient-name convention holds throughout: `kind_name` names the payload
shape (`aether.window.focus`), `recipient_name` names the mailbox
(`aether.window`). They share a prefix but route independently.

## 4. Verify

Three checks, cheapest first:

- **Coverage + schema shape** — `cargo nextest run -p aether-kinds` exercises
  `descriptors.rs`: `covers_every_substrate_kind` proves the kind reaches the
  hub-shipped list, and the shape tests (`signal_kinds_emit_unit`,
  `cast_kinds_emit_struct_with_repr_c`, the structured-vs-cast guards) catch a
  wire-format slip.
- **Round-trip through the chassis** — drive a `send_and_await` against the
  owning mailbox in the test bench and decode the reply. The pattern is in
  `crates/aether-substrate-bundle/tests/input_subscriptions.rs`:

  ```rust
  let out = bench
      .execute(vec![(
          "focus",
          BenchOp::send_and_await("aether.window", &FocusWindow {}),
      )])
      .expect("focus sequence");
  let reply = out.reply::<FocusWindowResult>("focus").expect("decode reply");
  ```

- **MCP surface** — with the harness up, `describe_kinds` lists the new name
  with its full schema and doc, confirming an agent can build the `send_mail`
  params for it. This is the agent-facing proof the kind is real.

## 5. The schema-change rule

Editing a kind's *shape* moves its `KindId`. The id hashes `name + schema`, so
adding or removing a field, changing a field's type, reordering fields, or
editing `#[kind(name = "…")]` all mint a new id; renaming a field or the Rust
struct does not (names are erased from the hashed bytes). The full table is in
[The type system](../foundations/type-system.md).

A moved id means producer and consumer must agree again:

- **Rebuild both sides.** A peer compiled against the old shape computes a
  different id, and the mail lands on "kind not found" rather than
  garbage-decoding. Rebuild every crate that sends or receives the kind.
- **Rebuild prebuilt component wasm.** A wasm component bakes the kind's id into
  its `aether.kinds` custom section at build time. Stale prebuilt wasm carries
  the old id, so a test bench that loads it observes nothing. Re-run the wasm
  cross-build (`scripts/preflight.sh` does this; CI pre-builds component wasm
  before `cargo test`).

## Staleness note

This recipe points at `aether.window.focus` rather than freezing its full diff,
because the snippet would rot while the pointer stays honest. If the cited
symbols have moved when you read this, treat fixing the recipe as part of the
work — that is the second half of the
[recipe staleness rule](../recipes.md#the-staleness-rule-for-recipes).
