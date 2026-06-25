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

Trace [`crates/aether-labyrinth/src/trajectory.rs`][traj] while you read.
`TrajectoryRecorderCapability` owns the `aether.trajectory` mailbox: a
config-free cap that keeps a little per-session state and answers two
kinds — a fire-and-forget `TrajectorySample` and a reply-bearing
`TrajectoryEnd`. It's small enough to hold in your head and exercises
every step below. Verify its names against the current source as you go —
a capability is a recompile-class recipe, so the symbols here rot faster
than the explainers (see [the staleness rule](#staleness)).

[traj]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-labyrinth/src/trajectory.rs

## 1. Name the mailbox

A capability's mailbox name is its `NAMESPACE` const. Chassis-owned
mailboxes live under the `aether.<name>` prefix — `aether.trajectory`,
`aether.audio`, `aether.fs`. The mailbox id is
`aether_data::mailbox_id_from_name(NAMESPACE)`, a compile-time const, so
peers address the cap by type with no host round-trip. Pick a name that
isn't already claimed; the builder rejects a collision at boot
([step 4](#4-register-with-the-chassis-builder)).

## 2. Write the actor

A capability is split into two halves (ADR-0122). The **identity** is a
ZST struct carrying only the addressing; the state-bearing **runtime**
lives in a feature-gated `runtime` module. The `#[actor] impl NativeActor
for X` block sits on the identity and names the runtime via `type State`:

```rust
use aether_kinds::{RecordResult, TrajectoryEnd, TrajectorySample};

// The runtime half is reached through a single glob seam.
#[allow(clippy::wildcard_imports)]
use runtime::*;

/// `aether.trajectory` cap identity: a ZST carrying only the addressing —
/// `Addressable` (`NAMESPACE`, `Resolver`) and the per-handler
/// `HandlesKind` markers, emitted always-on by `#[actor]`.
pub struct TrajectoryRecorderCapability;

#[actor(singleton, runtime_feature = "native")]
impl NativeActor for TrajectoryRecorderCapability {
    type State = TrajectoryRecorderCapabilityState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.trajectory";

    fn init(
        (): (),
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<TrajectoryRecorderCapabilityState, BootError> {
        Ok(TrajectoryRecorderCapabilityState { sessions: HashMap::new() })
    }

    // A reply-bearing handler returns its reply kind (ADR-0112). The first
    // parameter is the runtime state, threaded explicitly (the identity is
    // a ZST):
    #[handler]
    fn on_end(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        e: TrajectoryEnd,
    ) -> RecordResult {
        // … flush the session, then return the reply value:
        RecordResult::Ok { /* … */ }
    }
}

// The runtime half: the cap's state plus its substrate-typed imports. The
// whole module is gated behind the cap's feature at its declaration (here
// `#[cfg(feature = "native")] pub mod trajectory;` in `lib.rs`), so every
// line compiles only on a native build — no inner `#[cfg]` is needed.
mod runtime {
    pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    pub use aether_substrate::chassis::error::BootError;
    pub use std::collections::HashMap;

    pub struct TrajectoryRecorderCapabilityState {
        pub(super) sessions: HashMap<u64, Vec<…>>,
    }
}
```

The pieces:

- **The identity ZST** — `pub struct TrajectoryRecorderCapability;` carries
  no state. `#[actor]` emits its always-on `Addressable` + `HandlesKind<K>`
  markers, so a wasm guest writing
  `ctx.actor::<TrajectoryRecorderCapability>().send(&kind)` compile-checks
  even on a build where the runtime half is gated out.
- **`#[actor(singleton, runtime_feature = "native")]`** declares the
  cardinality — `singleton` for a chassis cap; `instanced` is the
  counterpart for per-instance actors — and names the feature its runtime
  `Lifecycle` / `Dispatch` / `NativeActor` impls gate behind. Omit
  `runtime_feature` to gate on the default `runtime` feature (the
  `aether.fs` cap does this); name a cap-specific feature when the native
  half pulls a heavy dep ([heavy native deps](#heavy-native-deps)).
- **`type State`** names the runtime struct holding the cap's mutable
  state. It lives in the feature-gated `runtime` module so it never
  compiles into a wasm build, and `#[handler]`s receive it as
  `state: &mut Self::State`.
- **`type Config`** is `()` for a config-free cap, or a real struct
  ([step 3](#3-give-it-a-config-if-it-needs-one)). The chassis builder
  threads it into `init`.
- **`init(config, ctx)`** builds the runtime state (it returns
  `Self::State`, not `Self`). The mailbox is already claimed; `ctx` is a
  `NativeInitCtx` exposing `ctx.mailer()` (the shared `Mailer`, which
  carries the `Registry`) for caps that pull a shared resource at boot —
  this one just builds plain state. `init` runs before the dispatcher
  starts and before any peer's dispatcher runs — no mail yet. Return
  `Err(BootError::…)` to abort the chassis build.
- **`wire(&mut self, ctx)`** (optional, default no-op) is the post-init
  mail-allowed hook: peers are addressable here, so subscribe to input
  streams or announce yourself from `wire`, not `init`.
  **`unwire(&mut self, ctx)`** (optional) is the symmetric pre-shutdown
  hook.
- **`#[handler] fn on_x(state: &mut Self::State, ctx, mail: K)`** infers
  the kind from its third parameter. The first parameter is the runtime
  state, threaded explicitly because the identity carries none — take
  `&Self::State` for a read-only handler, `&mut Self::State` to mutate; the
  dispatcher owns the cap on one thread, so state is [plain fields, no
  locks](../foundations/actor-model.md). The handler receives `mail` by
  value. A reply-bearing handler returns its reply kind (`-> R`, ADR-0112);
  a fire-and-forget handler returns `()`. For an imperative mid-handler
  reply, `ctx.reply(&result)` is the alternative.

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
builder.with_actor::<TrajectoryRecorderCapability>(())
```

Where that line goes depends on which chassis should carry the cap:

- **Every full-stack chassis** — add it to `with_common_caps` in
  [`crates/aether-substrate-bundle/src/chassis_common.rs`][common], the
  shared composition desktop, headless, and the embedded test bench all
  call. `TrajectoryRecorderCapability` lives here.
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

A cap whose runtime half pulls a heavy native-only dependency (the
renderer's wgpu, audio's cpal) names a cap-specific feature in its
`runtime_feature` override: `#[actor(singleton, runtime_feature =
"render-native")]`, with the `runtime` module gated `#[cfg(feature =
"render-native")]`. The identity markers stay always-on (so guests still
address the cap by type) while the native dep set only enters when the
feature is on. A cap whose runtime needs no heavy dep omits
`runtime_feature` and gates on the default `runtime` feature.

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
`TrajectoryRecorderCapability`'s `capability_routes_end_through_dispatcher_thread`
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
[`crates/aether-labyrinth/src/trajectory.rs`][traj] — if a name here
doesn't match what's in the tree, fix the recipe as part of your change.
The pointer is to the real cap, not a frozen copy, exactly so it tracks
the code.
