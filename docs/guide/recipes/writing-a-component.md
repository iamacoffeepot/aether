# Writing a component

> **Prereqs (the middle class):** `cargo` to compile *your* crate, and the
> [MCP harness](../mcp-harness.md) up to load and drive it. You build a wasm
> component to extend a running engine without rebuilding aether itself, so you're
> in both loops at once — compile your crate, then drive the live engine.

A component is the wasm host of an actor: code you write, compile to
`wasm32-unknown-unknown`, and load into a running substrate, where it talks to the
chassis by mail like any other actor. This recipe takes an empty crate all the way
to a loaded component answering mail — crate setup, the `#[actor]` block, `export!`,
the wasm build, `load_component`, and the first round-trip.

Read the [actor model](../foundations/actor-model.md) for how you *write* an actor
and [Components & lifecycle](../systems/components.md) for the wasm-specific
machinery; this recipe is the end-to-end loop those two pages describe in parts.

> **Verify against current code.** This recipe carries symbol names and file paths,
> so confirm them before you follow it: the SDK surface is `crates/aether-actor`, the
> worked exemplar is `crates/aether-mesh-viewer`, and the minimal smoke component is
> `crates/aether-actor/examples/hello.rs`. If a name below has moved, the fix is part
> of the work.

## 1. Set up the crate

A component is discovered **structurally** — no filename convention. The build walks
the workspace and treats a package as a component when it has both signals at once: a
`cdylib` library target, and a dependency on `aether-actor`. Those two lines in the
manifest are what make `cargo xtask dist` cross-build your crate to wasm.

```toml
# crates/my-component/Cargo.toml
[package]
name = "my-component"
version.workspace = true
edition.workspace = true

[lib]
crate-type = ["cdylib"]          # signal 1: the wasm cdylib target

[dependencies]
aether-actor = { path = "../aether-actor" }   # signal 2: the SDK dep
aether-capabilities = { path = "../aether-capabilities", default-features = false, features = ["render"] }
aether-kinds = { path = "../aether-kinds" }
serde = { workspace = true, default-features = false, features = ["derive", "alloc"] }

[lints]
workspace = true
```

`aether-capabilities` gives you the capability types you address by — `RenderCapability`,
`LifecycleCapability`, `InputCapability` — and `aether-kinds` is the substrate kind
vocabulary (`Tick`, `DrawTriangle`, `Key`, …). A component that defines *its own*
kinds with `#[derive(aether_data::Kind)]` also needs `inventory` reachable on native
builds, gated to non-wasm targets so the wasm guest skips the registration:

```toml
[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
inventory = { workspace = true }
```

A component and the peers that talk to it share the kind crate, so put any kinds you
invent in a sibling crate both sides depend on. `aether-mesh-viewer`'s manifest
(`crates/aether-mesh-viewer/Cargo.toml`) is the worked version of all of this, including
the dual-output `["cdylib", "rlib"]` shape a crate uses when host integration tests
link the same source. `aether-kit` packs several actors into one cdylib with
`export!(A, B, …)` (ADR-0096) — its `camera` export is the worked multi-actor example.

## 2. Write the actor block

The receive side is one `#[actor] impl WasmActor for C` block. Each `#[handler]`
method *is* a handler — the macro infers the kind it handles from the method's third
parameter, so there's no typelist to maintain.

```rust
use aether_actor::{BootError, WasmActor, WasmCtx, Resolver, actor};
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{LifecycleCapability, RenderCapability};
use aether_kinds::{DrawTriangle, Tick};

pub struct MyComponent {}

#[actor]
impl WasmActor for MyComponent {
    const NAMESPACE: &'static str = "my_component";   // default load name

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(MyComponent {})
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        // Subscribe the calling actor to the tick stream. This is the
        // first point sending is allowed.
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
    }

    #[handler]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        ctx.actor::<RenderCapability>().send(&TRIANGLE);
    }
}
```

Two parts earn attention:

- **`init` can't mail — subscribe in `wire`.** `init` runs while the actor is still
  being built and its mailbox isn't published yet, so its context is `Resolver`-only
  and has no `send`. Stream subscriptions and any other startup mail go in `wire`,
  which runs once the mailbox is live. The context type enforces this: a `send` from
  `init` doesn't compile. Subscriptions clear on drop and survive a `replace`, since
  the mailbox id is stable.
- **The third parameter is the kind.** `on_tick`'s `Tick` argument is what the macro
  routes on; the handler takes the decoded mail by value and `&mut self` because
  nothing else touches the state concurrently. Add an optional `#[fallback] fn(&mut
  self, ctx, mail: Mail<'_>)` to catch unhandled kinds, or omit it for a strict
  receiver.

`crates/aether-actor/examples/hello.rs` is this skeleton fleshed out (a static
triangle plus a ping/pong reply); `crates/aether-mesh-viewer/src/runtime.rs` is the
full-scale version with a mail family of handlers.

## 3. Add `export!`

One line, required, with no native-side equivalent:

```rust
aether_actor::export!(MyComponent);
```

This emits the `#[no_mangle]` FFI entry points the host calls across the wasm
boundary and the `aether.kinds.inputs` custom section that `describe_component` reads
back. Without it the wasm carries no exports and the substrate can't drive the actor.
You never write `extern "C"` by hand. The emitted shims are **wasm32-only**, so a
host (rlib) build of the same crate carries no FFI symbols — host-side tests drive a
component through the in-process transport instead.

## 4. Build for wasm32

```console
$ rustup target add wasm32-unknown-unknown    # once per toolchain
$ cargo build --target wasm32-unknown-unknown -p my-component
```

The artifact lands at `target/wasm32-unknown-unknown/debug/my_component.wasm` (the
crate name with dashes turned to underscores). CI and the pre-flight cross-build
every discovered component with `cargo xtask dist --no-bins`, which runs this same
per-package wasm build; reach for `xtask dist` to mirror CI, the direct `cargo build`
to iterate fast.

## 5. Load it over MCP

`load_component(engine_id, binary_path)` forwards the wasm to the engine's
`aether.component` mailbox and returns a `LoadResult`. The tool takes a filesystem
**path** and reads the bytes for you — tool JSON never carries the wasm buffer.

```text
spawn_substrate(binary_path = ".../aether-substrate")        → engine_id
load_component(engine_id, ".../my_component.wasm")           → LoadResult
```

`LoadResult::Ok` carries the assigned `mailbox_id`, the **resolved name**, and the
advertised capabilities read from the manifest. A loaded component registers at
**`aether.component/aether.embedded:my_component`** — `NAMESPACE` rendered into the
runtime lineage. Read that full string off `LoadResult.name`; it's the address every
later send targets.

> **Bare names warn-drop.** Sending to `"my_component"` (the bare `NAMESPACE`) goes
> nowhere — that name is never registered, and the mail is dropped with a warning.
> Always address the full `aether.component/aether.embedded:…` name from
> `LoadResult.name`.

## 6. Send it mail and read the logs

Address mail to the resolved name. `recipient_name` names the **mailbox**;
`kind_name` names the **payload shape** — they route independently even when they
share a prefix.

```text
send_mail([{
  engine_id,
  recipient_name = "aether.component/aether.embedded:my_component",
  kind_name      = "aether.lifecycle.tick",
  params         = {}
}])
```

To see what the component did, pull its log ring with `actor_logs(engine_id,
"aether.component/aether.embedded:my_component")`. Only in-actor `tracing::*` events
land in the ring (a `tracing::warn!` in a rejection arm, say); host events go to
stderr. For a component that renders, `capture_frame(engine_id)` reads the frame back
as a PNG — dispatch the state-producing mail in the call's `mails` bundle so it lands
before the readback.

## 7. Iterate with `replace_component`

Once it's loaded, edit the Rust, rebuild the wasm (step 4), and hot-swap it in place:

```text
replace_component(engine_id, mailbox_id, ".../my_component.wasm")
```

The swap replaces the wasm Module behind the **same mailbox id**, so peers, route
caches, and input subscriptions all stay valid — no reload, no re-addressing. State
continuity across the swap is opt-in through the `on_dehydrate` / `on_rehydrate`
hooks; see the [hot reload](../systems/components.md#hot-reload) section for carrying
state forward.

> **Rebuild the wasm after a chassis kind or mailbox rename.** A prebuilt
> `.wasm` carries the kind names it was compiled against. Rename a chassis kind or
> mailbox and reload a stale artifact, and its mail routes to nothing — the symptom
> is a component that loads cleanly but observes no kinds. Rebuild before you load.

## Where to read more

- How you write an actor at all — its lifecycle, `#[actor]`, handlers, addressing
  by type — [The actor model](../foundations/actor-model.md).
- The wasm-specific machinery — the FFI trampoline, `export!`'s custom sections,
  multi-actor modules, hot reload — [Components & lifecycle](../systems/components.md).
- The load / send / capture / log tools in operational detail —
  [The MCP harness](../mcp-harness.md).
