# Adding a chassis capability

**Class: recompile.** You're editing aether's Rust and rebuilding the
substrate — `cargo` plus the [pre-flight loop](../recipes.md#the-one-structural-seam-does-it-recompile).
A chassis capability is a native actor: one struct, one `#[actor] impl
NativeActor` block, and a line in a chassis builder that puts its mailbox
on the air. By the end you have a mailbox reachable by mail on whichever
chassis you wire it into.

This is the native half of the actor model. The authoring shape — `init`
/ `wire` / `unwire`, `#[handler]`, addressing by type — is the same one
[The actor model](../foundations/actor-model.md) walks for components;
read that first if the `#[actor]` shape is new. The capability-specific
parts are the host machinery: where the code lives, the builder
registration that publishes the mailbox, and the in-process test path.

## The exemplar

Trace [`crates/aether-capabilities/src/handle.rs`][handle] while you
read. `HandleCapability` owns the `aether.handle` mailbox: a config-free
cap that pulls a shared resource off the substrate at `init` and answers
five request kinds, each with a reply. It's small enough to hold in your
head and exercises every step below. Verify its names against the current
source as you go — a capability is a recompile-class recipe, so the
symbols here rot faster than the explainers (see [the staleness
rule](#staleness)).

[handle]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/handle.rs

## 1. Name the mailbox

A capability's mailbox name is its `NAMESPACE` const. Chassis-owned
mailboxes live under the `aether.<name>` prefix — `aether.handle`,
`aether.audio`, `aether.fs`. The mailbox id is
`aether_data::mailbox_id_from_name(NAMESPACE)`, a compile-time const, so
peers address the cap by type with no host round-trip. Pick a name that
isn't already claimed; the builder rejects a collision at boot
([step 4](#4-register-with-the-chassis-builder)).

## 2. Write the actor

The capability is a struct plus an `#[actor] impl NativeActor for X`
block, wrapped in a `#[bridge(singleton)] mod native`:

```rust
use aether_kinds::{HandlePublish, HandleRelease /* … the handler kinds */};

#[aether_actor::bridge(singleton)]
mod native {
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    use super::{HandlePublish, HandleRelease};

    pub struct HandleCapability {
        store: Arc<HandleStore>,
    }

    #[actor]
    impl NativeActor for HandleCapability {
        type Config = ();
        const NAMESPACE: &'static str = "aether.handle";

        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let store = Arc::clone(ctx.mailer().handle_store());
            Ok(Self { store })
        }

        #[handler]
        fn on_publish(&self, ctx: &mut NativeCtx<'_>, mail: HandlePublish) {
            // … do the work, then reply:
            ctx.reply(&HandlePublishResult::Ok { /* … */ });
        }
    }
}
```

The pieces:

- **`#[bridge(singleton)]`** wraps the `mod native` that holds the native
  impl. It emits the always-on `Actor` + `HandlesKind<K>` markers at file
  root (outside any cfg gate) so a wasm guest can write
  `ctx.actor::<HandleCapability>().send(&kind)` and have it compile-check,
  while the substrate-side impl and its imports sit behind
  `#[cfg(not(target_arch = "wasm32"))]`. The inner `#[actor]` is rewritten
  to `#[actor(skip_markers)]` so the markers aren't duplicated. Singleton
  is the cardinality for a chassis cap; `instanced` is the counterpart for
  per-instance actors. The handler-signature kind types must be imported
  at file root (as `super::*` shows) because the markers land as siblings
  of the mod.
- **`type Config`** is `()` for a config-free cap, or a real struct
  ([step 3](#3-give-it-a-config-if-it-needs-one)). The chassis builder
  threads it into `init`.
- **`init(config, ctx)`** builds the struct. The mailbox is already
  claimed; `ctx` is a `NativeInitCtx` exposing `ctx.mailer()` (the shared
  `Mailer`, which carries the `Registry` and `HandleStore`) for caps that
  pull a shared resource at boot. `init` runs before the dispatcher
  starts and before any peer's dispatcher runs — no mail yet. Return
  `Err(BootError::…)` to abort the chassis build.
- **`wire(&mut self, ctx)`** (optional, default no-op) is the post-init
  mail-allowed hook: peers are addressable here, so subscribe to input
  streams or announce yourself from `wire`, not `init`.
  **`unwire(&mut self, ctx)`** (optional) is the symmetric pre-shutdown
  hook.
- **`#[handler] fn on_x(&self, ctx, mail: K)`** infers the kind from its
  third parameter. Take `&self` for a read-only or stateless handler,
  `&mut self` to mutate cap state — the dispatcher owns the cap on one
  thread, so state is [plain fields, no locks](../foundations/actor-model.md).
  The handler receives `mail` by value; reply with `ctx.reply(&result)`.
  Handlers promise nothing about replies — a fire-and-forget kind simply
  doesn't call `ctx.reply`.

The kinds a handler receives must exist in the substrate kind inventory
so the dispatcher can decode the wire bytes — that's the *Adding a
substrate kind* recipe, separate from this one.

### The reply gotcha

`ctx.reply(&result)` is the normal path. If you instead reach for
`ctx.mailer().send_reply(ctx.reply_target(), &result)` (as the headless
window cap does to fail-fast), note that `HubOutbound::send_reply`
silently drops a `SourceAddr::Component` reply target — the tag an
MCP-spawned engine carries. Routing through `ctx.mailer().send_reply`
(the complete router) reaches every reply target;
`HubOutbound::send_reply` does not.

## 3. Give it a config if it needs one

A config-free cap uses `type Config = ();`. A cap with tunables declares a
struct and derives `Config` on it, so its knobs flow through the same
layered env/argv overlay every other cap uses rather than a raw
`env::var` read. That dance — `#[derive(aether_substrate::Config)]`, the
emitted overlay, `from_argv_then_env`, wiring into the chassis CLI — is
[Configuration](../systems/configuration.md). Pass the resolved struct as
the `with_actor::<X>(config)` argument in the next step. Keep an empty
config a struct rather than `()` if you expect knobs later, so the
composition site doesn't churn when the first one lands (the input cap's
`InputConfig` does exactly this).

## 4. Register with the chassis builder

A mailbox is only on the air once a chassis builder claims it. The
builder is `aether_substrate::chassis::builder::Builder`; you add a cap
with `with_actor::<X>(config)` ([ADR-0070][adr70] / [ADR-0071][adr71]):

```rust
builder.with_actor::<HandleCapability>(())
```

Where that line goes depends on which chassis should carry the cap:

- **Every full-stack chassis** — add it to `with_common_caps` in
  [`crates/aether-substrate-bundle/src/chassis_common.rs`][common], the
  shared composition desktop, headless, and the embedded test bench all
  call. `HandleCapability` lives here.
- **One chassis only** — add it to that chassis's own builder chain:
  `desktop/chassis.rs`, `headless/chassis.rs`, or `hub/chassis.rs` in
  `aether-substrate-bundle`. The desktop renderer
  (`with_actor::<RenderCapability>(render_config)`) is desktop-only this
  way; the headless companion (`HeadlessRenderCapability`) claims the same
  `aether.render` name on the headless chassis.

The builder claims `A::NAMESPACE` as it boots each cap and enforces
**one claimant per name**: a second cap claiming an already-owned mailbox
fails the build with `BootError::MailboxAlreadyClaimed { name }` (or a
namespace-ownership error for a `NAMESPACE` collision across types). This
is the guarantee that lets two chassis define different caps behind the
same well-known name (the desktop vs headless renderer) without either
silently shadowing the other — each composition picks exactly one.

Boot is multi-pass across every cap: `claim → init → wire → spawn`,
synchronized so that at `init` time every peer mailbox is claimed and at
`wire` time every peer has an instance. Declaration order is boot order.

[adr70]: https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0070-native-capabilities-and-chassis-as-builder.md
[adr71]: https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0071-driver-capabilities-and-chassis-composition.md
[common]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-substrate-bundle/src/chassis_common.rs

## 5. Passive cap or driver?

Most capabilities are **passive**: they sit on a dispatcher and answer
mail, added with `with_actor`. A chassis also composes exactly one
**driver** — the cap that owns the chassis main thread and runs its loop
(winit on desktop, a std timer on headless, the TCP accept loop on the
hub). A driver implements `DriverCapability` (not `NativeActor`) and is
supplied with `.driver(d)` rather than `.with_actor`; the type-state
builder enforces exactly one.

If the cap drives — owns a loop or a peripheral — its name carries
`Driver`: `DesktopDriverCapability`, `HeadlessTimerDriverCapability`. A
plain `FooCapability` reads as a passive sink. Don't name a passive cap
`*DriverCapability`. Most new caps are passive; you reach for a driver
only when standing up a new chassis kind.

### Heavy native deps

A cap whose native impl pulls a heavy native-only dependency (the
renderer's wgpu, audio's cpal) gates the inner module behind a cargo
feature: `#[bridge(singleton, feature = "render-native")]`. The wasm-side
markers stay always-on (so guests still address the cap by type) while
the native dep set only enters when the feature is on. A cap with no
native-only deps — like `HandleCapability` — needs no feature.

## 6. Test it in-process

A native cap compiles into the substrate, so its tests boot a real
chassis in-process and drive it with mail — no wasm, no FFI, no MCP
session. (`export!`'s FFI shims are wasm32-only and belong to *components*,
not capabilities; a native cap has nothing to cross-compile.) The
in-crate pattern, in the cap's `#[cfg(test)] mod tests`:

1. Seed a substrate with `test_chassis::fresh_substrate()` →
   `(Arc<Registry>, Arc<Mailer>)`, the registry pre-loaded with the kind
   descriptors.
2. Boot the cap alone with
   `test_chassis::boot_test_chassis_with::<X>(&registry, &mailer, config)`
   (or an inline `Builder::<TestChassis>::new(…).with_actor::<X>(cfg).build_passive()`
   when the scenario needs more than one cap).
3. Look up the registered mailbox by `X::NAMESPACE`, enqueue an envelope,
   and read the reply off the loopback `EgressEvent` channel
   (`HubOutbound::attached_loopback` gives you the receiver).

The cap dispatches on a real pool thread, so a round-trip test that
sleep-polls the loopback channel under a deadline is timing-sensitive;
keep that deadline generous so it tolerates a busy machine.
`HandleCapability`'s `capability_routes_publish_through_dispatcher_thread`
shows the full round trip; its `duplicate_claim_rejects_with_typed_error`
asserts the one-claimant guarantee by pre-registering the name and
expecting `BootError::MailboxAlreadyClaimed`.

For an end-to-end check across the real chassis — rendering, the frame
loop, multiple caps — drive
[`aether_substrate_bundle::test_bench`](../mcp-harness.md) instead: it
boots a full chassis from a Rust thread and sends mail the same encode
path the MCP tool uses.

## 7. Smoke it over MCP

If the cap fronts a load-bearing path, exercise it once live: bring up the
[MCP harness](../mcp-harness.md), `spawn_substrate`, `send_mail` one of
its kinds at the cap's mailbox name, and read `actor_logs` for that
mailbox. Unit tests and clippy don't exercise the spawned-engine reply
route (the `SourceAddr::Component` reply gotcha lives there), so a live
smoke catches what the in-process test can't.

## Staleness

This recipe carries file paths and symbol names, so confirm them against
the current source before following it. The exemplar is
[`crates/aether-capabilities/src/handle.rs`][handle] — if a name here
doesn't match what's in the tree, fix the recipe as part of your change.
The pointer is to the real cap, not a frozen copy, exactly so it tracks
the code.
