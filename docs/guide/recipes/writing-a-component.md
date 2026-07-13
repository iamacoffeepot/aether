# Writing a component

> **Prerequisites:** Rust with the `wasm32-unknown-unknown` target and a live
> [MCP harness](../mcp-harness.md). This recipe builds guest wasm; it does not
> rebuild the native chassis.

A component is a wasm module exporting one or more actors. This walkthrough
builds a minimal ping/pong actor, uploads its bytes to the hub registry, loads
an instance, sends one request, then replaces it without changing its mailbox.

Use `crates/aether-actor/examples/hello.rs` as the current in-tree exemplar and
`crates/aether-test-fixtures/` for load/replace edge cases.

## 1. Create a dual-purpose crate

A package is discovered as a component when it depends on `aether-actor` and
exposes a `cdylib`. Add `rlib` when other Rust crates/tests should import its
public kinds or helpers.

```toml
[package]
name = "my-component"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
aether-actor = { path = "../aether-actor" }
aether-kinds = { path = "../aether-kinds" }
```

In this workspace, inherit version/dependencies/lints as neighboring crates do.
If the component defines public kinds, keep them in its always-on public surface
or a small sibling contract crate so callers can encode the same schema without
linking the guest runtime implementation.

## 2. Implement one actor

```rust
use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{Ping, Pong};

pub struct Echo;

#[actor]
impl WasmActor for Echo {
    const NAMESPACE: &'static str = "example.echo";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    /// Echo a sequence number to the caller.
    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}

aether_actor::export!(Echo);
```

The contracts are visible in the types:

- `WasmInitCtx` cannot send startup mail before the mailbox is published. Put
  subscriptions and startup sends in `wire(&mut self, &mut WasmCtx)`.
- The handler's third argument is the input kind.
- `#[handler::single]` declares one reply; the return type is the reply kind.
- Actor state is only touched through serialized `&mut self` dispatch.
- `export!` emits FFI and actor/kind manifests; do not write host exports by hand.

## 3. Make default selection explicit

For one actor, `export!(Echo)` is unambiguous. For several actors choose whether
the module has a default:

```rust
// Bare loads select Console; other actors remain selectable.
aether_actor::export!(default = Console, Inspector, Worker);

// No default: every load must select Alpha or Beta explicitly.
aether_actor::export!(Alpha, Beta);
```

Declaration order does **not** make the first actor the default. Defaultless
modules omit `aether.namespace` and a bare load fails (ADR-0138).

## 4. Build the wasm artifact

```sh
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown -p my-component
```

The debug artifact is normally:

```text
target/wasm32-unknown-unknown/debug/my_component.wasm
```

`cargo xtask dist --no-bins` structurally discovers and builds the workspace
component set. Use the direct package build for iteration and the distribution
command when validating packaging/discovery.

## 5. Upload, then load

Stored artifacts and live instances are different resources:

```text
upload_component(staged_path = ".../my_component.wasm", name = "my-component-dev")
  → { hash, name, ... }

spawn_substrate()
  → { engine_id, ... }

load_component(engine_id, selector = "<returned hash or name>")
  → { mailbox_id, name, capabilities, ... }
```

`upload_component` is the only step above that takes a host wasm path.
`load_component` resolves a registry selector. For a defaultless module, select
an export (for example `module@actor`) as described by the live tool schema.

Record the returned loaded `name`, normally a full lineage such as
`aether.component/aether.embedded:example.echo`. Do not send to the bare Rust
namespace and do not substitute the registry artifact name for the live mailbox.

If the actor has typed config, pass either inline `config` JSON or
`config_path` pointing to a JSON file. MCP schema-encodes that JSON against the
component's config kind. `config_path` is not a pre-encoded binary blob.

## 6. Inspect and send

Use the loaded lineage with `describe_component`. Confirm `aether.ping` is in
the handler set and its reply is `aether.pong`. Use `describe_kinds` for the
current parameter shape.

```text
send_mail({
  engine_id,
  recipient_name: "<LoadResult.name>",
  kind_name: "aether.ping",
  params: { "seq": 7 }
})
```

Expect a decoded pong with `seq: 7`. If the load succeeds but mail is unresolved,
check the exact lineage first. If the handler is missing, check the selected
export and rebuild the wasm instead of trusting an old artifact.

## 7. Replace in place

Edit the actor, rebuild, and upload the new bytes. Replacement also resolves a
registry selector:

```text
upload_component(staged_path = ".../my_component.wasm", name = "my-component-dev")
  → { hash: new_hash, ... }

replace_component(
  engine_id,
  component = "<loaded lineage or mailbox id required by live schema>",
  selector = "<new_hash>"
)
```

The trampoline/mailbox lineage remains stable while the module changes.
Replacement can preserve, migrate, reject, or reshape state through persistence
hooks and its compatibility contract. Test that path with the typed/reshaped
fixture patterns; a successful code swap alone does not prove state continuity.

Re-run `describe_component` after replacement and refresh named-kind discovery
before sending a newly introduced kind.

## 8. Clean up what you own

Drop a task-owned component from a shared engine only when other actors no
longer depend on it. Drop clears the guest but leaves an empty trampoline at
that lineage; the name is not a fresh reusable slot in the same engine.

If you spawned the engine for this recipe, terminate that exact `engine_id`.
Do not terminate an engine merely because it is the only one you can see.

## Common failures

| Symptom | Check |
|---|---|
| Wasm has no callable actor | `export!` is present in the wasm build |
| Bare load fails | module is defaultless; select an export |
| Config decode fails | pass JSON shape, not encoded bytes |
| Mail warn-drops | use returned lineage, not namespace/artifact name |
| New build did not load | replacement selector still points at old hash |
| State disappeared | persistence/rehydrate contract was absent or rejected |

Continue with [Components and lifecycle](../systems/components.md),
[Component registry and replacement](../operating/component-registry.md), and
[Guest/native boundaries](../architecture/guest-native-boundary.md).
