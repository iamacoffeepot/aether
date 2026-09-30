# Writing a component

> **Prerequisites:** Rust with the `wasm32-unknown-unknown` target and a live
> [MCP harness](../mcp-harness.md). This recipe builds guest wasm; it does not
> rebuild the native chassis.

A component is a wasm module exporting one or more actors. This walkthrough
builds a minimal ping/pong actor, uploads its bytes to the hub registry, loads
an instance, sends one request, then replaces it without changing its mailbox.

Use `crates/aether-actor/examples/hello.rs` as the current in-tree exemplar and
`crates/aether-test-fixtures-*/` for load/publish edge cases.

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

#[actor(root)]
impl WasmActor for Echo {
    const NAMESPACE: &'static str = "example.echo";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    /// Echo a sequence number to the caller.
    #[handler::request]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, ping: Ping) -> Pong {
        Pong { seq: ping.seq }
    }
}

aether_actor::export!(public = [Echo]);
```

A ctx that omits its actor is typed by it: the macro reads `WasmCtx<'_>` as
`WasmCtx<'_, Self>`, so the ctx reaches only the actors the component declares
with `depends(R)`. The actor is the first parameter, the reply mode the second
(`WasmCtx<'_, Self, Unchecked>`); spell `WasmCtx<'_, Erased>` for the untyped view.

The contracts are visible in the types:

- `WasmInitCtx` cannot send startup mail before the mailbox is published. Put
  subscriptions and startup sends in `wire(&mut self, &mut WasmCtx)`.
- The handler's third argument is the input kind.
- `#[handler::request]` answers with its return type, the reply kind;
  `#[handler::tell]`, `#[handler::event]`, and `#[handler::response]` answer
  nothing and say why the mail arrives: a command, a subscription, or the
  answer to this actor's own request.
- Actor state is only touched through serialized `&mut self` dispatch.
- `export!` emits FFI and actor/kind manifests; do not write host exports by hand.

## 3. Select an export when the module has several

`export!` takes keyed entries only. For one actor, `export!(public = [Echo])` is
unambiguous. For several actors, every load must name one explicitly:

```rust
// A load must select Alpha or Beta explicitly.
aether_actor::export!(public = [Alpha, Beta]);
```

Declaration order carries **no** meaning: a bare load against a multi-export
module is refused, naming the exports (ADR-0241 §9, superseding ADR-0138's
opt-in default). A module that exports exactly one actor still loads without
a selector, and only that case emits `aether.namespace`.

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

### Use the watched development loop

When an engine is already running, `dev-component` owns the repetitive
build/upload/load-or-republish loop:

```sh
cargo xtask dev-component \
  --package my-component \
  --engine-id <engine UUID>
```

The package and engine are always explicit. The command infers the wasm target
only when that package exposes one component; otherwise add the exact artifact
stem, for example `--target my_component`. It connects to
`http://127.0.0.1:8890/mcp` by default; use `--mcp-endpoint <URL>` for another
configured streamable-HTTP endpoint. This flag changes the connection only: it
does not transfer the wasm. Run xtask on the MCP/fleet host, or on a filesystem
where the built artifact has the same absolute path visible to that host,
because `upload_component.staged_path` is read there.

Without an existing instance, the first successful pass uploads and loads the
component. The command prints and retains the loaded component's canonical
`address`; later passes upload and `publish` each new build, which republishes
the component at that address. For a module exporting several types, select the
type's namespace on that first load:

```sh
cargo xtask dev-component \
  --package my-component \
  --engine-id <engine UUID> \
  --namespace example.alpha
```

`--namespace` applies only to the initial load. Later replacements reuse the actor
already hosted by the component. To replace an existing instance from the first
pass, supply its address as `load_component` returned it:

```sh
cargo xtask dev-component \
  --package my-component \
  --engine-id <engine UUID> \
  --address example.echo
```

The flag takes an actor path; a malformed path is rejected before the watcher
starts, and a tagged `mbx-…` id is not accepted. A publish names no instance: it
republishes the module, and every live instance of its namespaces, this one
among them, moves to the new build. `--address` and `--namespace` conflict because
replace-first mode already has a hosted actor.

The package root is watched recursively and generated target output is ignored.
Edits are debounced, rebuilds are serialized, and edits arriving during a pass
coalesce into another pass. A build, upload, load, or replacement error is
reported without changing the command's last known live binding; the watcher
then waits for the next edit. Replacement itself retains the current MCP tool's
phase-dependent failure semantics, so inspect the selected component after a
replacement error before relying on its guest state.

Ctrl-C stops only this local developer loop. It does not drop the component,
terminate the engine, or clean up any lifecycle resource. You remain responsible
for the engine and component you explicitly selected.

## 5. Upload, then load

Stored artifacts and live instances are different resources:

```text
upload_component(staged_path = ".../my_component.wasm", name = "my-component-dev")
  → { hash, name, ... }

spawn_substrate()
  → { engine_id, ... }

load_component(engine_id, selector = "<returned hash or name>")
  → { engine_id, address, capabilities }
```

`upload_component` is the only step above that takes a host wasm path.
`load_component` resolves a registry selector. For a module exporting several
types, select an export (for example `module@actor`) as described by the live
tool schema.

Record the returned `address`, the component's published name: `example.echo`
for a singleton, `example.echo:key` for an instanced type. Do not substitute the
registry artifact name for the live mailbox.

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
  address: "<load_component address>",
  kind_name: "aether.ping",
  params: { "seq": 7 }
})
```

Expect a decoded pong with `seq: 7`. If the load succeeds but mail is unresolved,
check the exact lineage first. If the handler is missing, check the selected
export and rebuild the wasm instead of trusting an old artifact.

## 7. Replace in place

Edit the actor, rebuild, and upload the new bytes. Replacement is a `publish` of
the successor, which also resolves a registry selector:

```text
upload_component(staged_path = ".../my_component.wasm", name = "my-component-dev")
  → { hash: new_hash, ... }

publish(
  engine_id,
  selector = "<new_hash>"
)
```

Every live instance of the module's namespaces moves to the new build as one
group, or none does (ADR-0241 §7); each keeps its address and mailbox. The new
build must export every namespace the old one did and keep each one's handler
rows. An instance whose `Config` kind changed needs
`configs = [{ address, config }]`.
Replacement can preserve, migrate, reject, or reshape state through persistence
hooks and its compatibility contract. Test that path with the typed/reshaped
fixture patterns; a successful code swap alone does not prove state continuity.

Re-run `describe_component` after replacement and refresh named-kind discovery
before sending a newly introduced kind.

## 8. Clean up what you own

Drop a task-owned component from a shared engine only when other actors no
longer depend on it. Drop closes the instance's trampoline entirely — nothing
resident is left at that lineage — and the name tombstones for the engine's
lifetime; it is not a fresh reusable slot in the same engine.

If you spawned the engine for this recipe, terminate that exact `engine_id`.
Do not terminate an engine merely because it is the only one you can see.

## Common failures

| Symptom | Check |
|---|---|
| Wasm has no callable actor | `export!` is present in the wasm build |
| Bare load fails | module exports several types; select an export |
| Config decode fails | pass JSON shape, not encoded bytes |
| Mail warn-drops | use returned lineage, not namespace/artifact name |
| New build did not load | replacement selector still points at old hash |
| State disappeared | persistence/rehydrate contract was absent or rejected |

Continue with [Components and lifecycle](../systems/components.md),
[Component registry and replacement](../operating/component-registry.md), and
[Guest/native boundaries](../architecture/guest-native-boundary.md).
