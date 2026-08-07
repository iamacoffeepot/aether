# Adding a chassis capability

**Class: recompile.** You're editing aether's Rust and rebuilding the
substrate — `cargo` plus the [pre-flight loop](../recipes.md#the-one-structural-seam-does-it-recompile).
A chassis capability is a native actor: an identity struct, a `#[runtime]
impl NativeActor` block, and a line in a chassis builder that puts its
mailbox on the air. By the end you have a mailbox reachable by mail on
whichever chassis you wire it into.

This is the native half of the actor model. The authoring shape — `init`
/ `wire` / `unwire`, `#[handler::<class>]`, addressing by type — is the same one
[The actor model](../foundations/actor-model.md) walks for components;
read that first if the `#[actor]` shape is new. The capability-specific
parts are the host machinery: where the code lives, the builder
registration that publishes the mailbox, and the in-process test path.
For the normative module shape every capability converges on — directory
layout, identity/runtime split, test placement — see
[Capability module anatomy](../capability-anatomy.md).

## The exemplar

Trace [`crates/aether-text/src/`][text] while you read.
`TextCapability` owns the `aether.text` mailbox: a config-free CPU-only
cap that keeps a little per-session state — a font registry and a glyph
atlas — while typed request contexts correlate the work in flight. It
answers both kind flavors this recipe teaches. `aether.text.draw` is fire-and-forget;
`aether.text.load_font` is reply-bearing. It's small enough to hold in
your head and exercises every step below. Verify its names against the
current source as you go — a capability is a recompile-class recipe, so
the symbols here rot faster than the explainers (see
[the staleness rule](#staleness)).

The identity lives in [`text/lib.rs`][lib]; the state and handler bodies
in [`text/runtime/mod.rs`][runtime]; the owned kinds in
[`text/kinds.rs`][kinds].

[text]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-text/src
[lib]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-text/src/lib.rs
[runtime]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-text/src/runtime/mod.rs
[kinds]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-text/src/kinds.rs

## 1. Name the mailbox

A capability's mailbox name is its `NAMESPACE` const. Chassis-owned
mailboxes live under the `aether.<name>` prefix — `aether.text`,
`aether.audio`, `aether.fs`. Peers address the cap by type —
`ctx.actor::<TextCapability>().send(&kind)` — which resolves to a
compile-time-const mailbox id derived from `NAMESPACE`, so there's no
host round-trip for addressing. Pick a name that isn't already claimed;
the builder rejects a collision at boot
([step 4](#4-register-with-the-chassis-builder)).

## 2. Write the actor

A capability is split into two halves (ADR-0122). The **identity** is a
ZST struct carrying only the addressing; the state-bearing **runtime**
lives in a feature-gated `runtime` module. `#[actor(singleton)]` sits on
the identity in `lib.rs`, and a separate `#[runtime] impl NativeActor for
X` in the runtime module names the runtime through `type State`
(ADR-0123):

```rust
// text/lib.rs — the identity half, always-on.
use aether_actor::actor;

/// `aether.text` cap identity: a ZST carrying only the addressing —
/// `Addressable` (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind`
/// markers, and the singleton name-inventory entry, all emitted always-on
/// by `#[actor]`.
#[actor(singleton)]
pub struct TextCapability;

// The runtime half — state, substrate-typed imports, and the `#[runtime]
// impl NativeActor` — lives in `runtime/`, gated once here on the cap's
// feature. The struct-hosted `#[actor]` above reads that module off disk
// to lift the identity.
#[cfg(feature = "runtime")]
mod runtime;
```

```rust
// text/runtime/mod.rs — the runtime half, gated by the `mod runtime;`
// line above. The substrate-typed imports enter only on a native build.
use super::TextCapability;
use super::kinds::{DrawText, LoadFont, LoadFontResult};
use crate::fs::{FsCapability, Read, ReadResult};
use aether_actor::runtime;
use aether_substrate::Manual;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

/// The cap's mutable state — the font registry and glyph atlas.
/// `#[handler::<class>]`s receive it as `state: &mut Self::State`.
pub struct TextCapabilityState { /* … */ }

#[runtime]
impl NativeActor for TextCapability {
    type State = TextCapabilityState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.text";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<TextCapabilityState, BootError> {
        Ok(TextCapabilityState::new())
    }

    // Fire-and-forget: the handler returns `()`. `draw` lays the string
    // out and emits textured quads to `aether.render` the same tick.
    #[handler::single]
    fn on_draw_text(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: DrawText) {
        // … rasterize glyphs, send the quad batch …
    }

    // Reply-bearing, deferred: attach a typed context to the forwarded
    // `aether.fs.read`, then reply later from `on_read_result`.
    // See "the reply is deferred here" below.
    #[handler::manual]
    fn on_load_font(_state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: LoadFont) {
        TextCapabilityState::forward_font_read(
            ctx,
            mail.namespace,
            mail.path,
            PendingReply::LoadFont,
        );
    }
}
```

The pieces:

- **The identity ZST** — `pub struct TextCapability;` carries no state.
  `#[actor(singleton)]` emits its always-on `Addressable` + `HandlesKind<K>`
  markers, so a wasm guest writing
  `ctx.actor::<TextCapability>().send(&kind)` compile-checks even on a
  build where the runtime half is gated out.
- **`#[actor(singleton)]`** declares the cardinality — `singleton` for a
  chassis cap; `instanced` is the counterpart for per-instance actors (the
  engine proxy uses it). It reads the sibling runtime module off disk to
  lift the identity from the `#[runtime] impl`.
- **`#[runtime] impl NativeActor for TextCapability`** carries the
  behaviour. The `#[runtime]` attribute emits the runtime surface ungated
  — the `#[cfg(feature = "runtime")]` rides the `mod runtime;` line in
  `lib.rs`, so every impl already exists only on a build where the runtime
  module is present. No inner `#[cfg]` is needed.
- **`type State`** names the runtime struct holding the cap's mutable
  state. It lives in the feature-gated `runtime` module so it never
  compiles into a wasm build, and `#[handler::<class>]`s receive it as
  `state: &mut Self::State`.
- **`type Config`** is `()` for a config-free cap, or a real struct
  ([step 3](#3-give-it-a-config-if-it-needs-one)). The chassis builder
  threads it into `init`.
- **`init(config, ctx)`** builds the runtime state (it returns
  `Self::State`, not `Self`). The mailbox is already claimed; `ctx` is a
  `NativeInitCtx` exposing `ctx.mailer()` (the shared `Mailer`, which
  carries the `Registry`) for caps that pull a shared resource at boot —
  text just builds plain CPU state. `init` runs before the dispatcher
  starts and before any peer's dispatcher runs — no mail yet. Return
  `Err(BootError::…)` to abort the chassis build.
- **`wire(&mut self, ctx)`** (optional, default no-op) is the post-init
  mail-allowed hook: peers are addressable here, so subscribe to input
  streams or announce yourself from `wire`, not `init`.
  **`unwire(&mut self, ctx)`** (optional) is the symmetric pre-shutdown
  hook. Text needs neither.
- **`#[handler::<class>] fn on_x(state: &mut Self::State, ctx, mail: K)`** infers
  the kind from its third parameter. The first parameter is the runtime
  state, threaded explicitly because the identity carries none — take
  `&Self::State` for a read-only handler, `&mut Self::State` to mutate; the
  dispatcher owns the cap on one thread, so state is [plain fields, no
  locks](../foundations/actor-model.md). The handler receives `mail` by
  value.

### Reply-bearing handlers, and text's deferred variant

A self-contained reply-bearing handler returns its reply kind (`-> R`,
ADR-0112): the handler computes the answer this turn and the dispatcher
sends it back. A fire-and-forget handler returns `()`.

Text's `load_font` is the **deferred** variant, because it can't answer
this turn — it must round-trip `aether.fs` first. So its handlers are the
`Manual` reply class (`#[handler::manual]`), which hands the reply timing
to the handler rather than a returned value:

1. **`on_load_font`** captures the original caller's `Source` in a
   `FontLoadContext` together with the owed `PendingReply` shape, then forwards
   an `aether.fs.read` with
   `ctx.actor::<FsCapability>().send_with_context(&read, &context)`.
   Correlation lives in the binding's request table, not a path-keyed actor-state
   map, so two requests for the same path remain distinct.
2. **`on_read_result`** recovers the matching context with
   `ctx.take_context::<FontLoadContext>()`. On the error arm it replies to
   `context.source` with `ctx.reply_to`; on success it carries that source and a
   `FontParseContext` into the off-thread parse.
3. **`on_font_parsed`** (the `#[handler(task)]` completion) receives
   `TaskDone<FontParseOutput, FontParseContext>`, registers the parsed font under
   a session-scoped `font_id`, and re-replies through the captured caller with
   `done.resolve_value(ctx, &LoadFontResult::Ok { … })`.

`ctx.reply(&result)` replies to the current handler's caller this turn;
`ctx.reply_to(source, &result)` replies to a `Source` captured earlier —
the deferred path text uses. Trace the full correlation in
`runtime/mod.rs` rather than reading it re-explained here.

The kinds a handler receives must exist in the substrate kind inventory
so the dispatcher can decode the wire bytes. Text **owns** its kinds:
`LoadFont`, `LoadFontResult`, `DrawText`, and the rest live in
[`text/kinds.rs`][kinds] beside the cap that dispatches them (ADR-0121),
always-on and wasm-safe (they need only `aether-data` + `serde`). Their
`inventory::submit!` descriptor entries ride the `Kind` derive, so
`aether_kinds::descriptors::all()` surfaces them. Registering a kind whose
decode the dispatcher must find is the separate *Adding a substrate kind*
recipe.

### The reply gotcha

`ctx.reply(&result)` / `ctx.reply_to(source, &result)` — the
`NativeBinding` handler-reply path — is the complete router: it reaches
every `SourceAddr`, including the `Component` local-RPC-server reply target
an MCP-spawned engine tags. If you instead reach for the raw
`HubOutbound::send_reply`, note that it silently drops a
`SourceAddr::Component` target (iamacoffeepot/aether#1321), so an
MCP-spawned caller's reply never lands. Reply through `ctx.reply` /
`ctx.reply_to`; the headless window cap's `runtime.rs` records the same
hazard.

## 3. Give it a config if it needs one

A config-free cap uses `type Config = ();` — text does, holding only CPU
state. A cap with tunables declares a struct and derives `Config` on it,
so its knobs flow through the same config-file/env/argv source stack every
other cap uses rather than a raw `env::var` read. That dance —
`#[derive(aether_substrate::Config)]`, the emitted overlay,
`resolve_with_file`, wiring into the chassis CLI and TOML section — is
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
builder.with_actor::<TextCapability>(())
```

Where that line goes depends on which chassis should carry the cap:

- **Desktop and headless together** — add it to `with_common_caps` in
  [`crates/aether-chassis/src/boot.rs`][common], the
  shared composition those two chassis call. `TextCapability` lives here.
  Adding it to the `.with_actor::<_>()` chain is all it takes: the
  `--describe` manifest is claim-derived ([ADR-0155][adr155]), so a cap
  appears in the roster the moment it claims a mailbox — there is no
  parallel namespace list to keep in lockstep.
- **The substrate-harness chassis** — the in-process harness does not call
  `with_common_caps`; it has a separate, reduced builder chain in
  [`crates/aether-harness-substrate/src/chassis.rs`][substrateharness];
  add the capability there too when scenarios should drive it, and thread any
  required config through `SubstrateHarnessEnv`. `TextCapability` is registered in both
  compositions for this reason.
- **One chassis only** — add it to that chassis's own builder chain:
  `desktop/chassis.rs`, `headless/chassis.rs`, or `hub/chassis.rs` in
  the chassis crates. The desktop renderer
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
[adr155]: https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0155-staged-chassis-boot-and-claim-derived-describe.md
[common]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-chassis/src/boot.rs
[substrateharness]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-harness-substrate/src/chassis.rs

## 5. Passive cap or driver?

Most capabilities are **passive**: they sit on a dispatcher and answer
mail, added with `with_actor`. `TextCapability` is passive. An executable chassis
also composes exactly one **driver** — the cap that owns the chassis main thread
and its lifetime (the winit loop on desktop and the std timer on headless). The
hub's `HubServerDriverCapability` instead owns the `SubstrateBoot` and blocks that
thread on SIGINT/SIGTERM; `RpcServerCapability`, a passive actor, owns the socket
listener. A driver implements `DriverCapability` (not `NativeActor`) and is
supplied with `.driver(d)` rather than `.with_actor`; the type-state builder
enforces exactly one. The in-process SubstrateHarness uses `build_passive` and lets its
embedder drive it, so it deliberately has no driver capability.

If the cap drives — owns a loop or a peripheral — its name carries
`Driver`: `DesktopDriverCapability`, `HeadlessTimerDriverCapability`. A
plain `FooCapability` reads as a passive sink. Don't name a passive cap
`*DriverCapability`. Most new caps are passive; you reach for a driver
only when standing up a new chassis kind.

### Heavy native deps

Text's runtime half pulls `fontdue`, a native-only dependency, through its
generic `runtime` feature on the `mod runtime;` line:
`#[cfg(feature = "runtime")] mod runtime;`. The identity markers stay
always-on (so guests still address the cap by type) while `fontdue` only
enters when the feature is on. The renderer's `wgpu` (`render-runtime`)
and audio's `cpal` (`audio-runtime`) gate the same way. A cap whose runtime
needs no heavy dep still gates its `mod runtime;` line on the generic
`runtime` feature, so the split holds without minting a cap-specific gate.

## 6. Test it in-process

A native cap compiles into the substrate, so its tests drive a real
handler with mail — no wasm, no FFI, no MCP session. (`export!`'s FFI
shims are wasm32-only and belong to *components*, not capabilities; a
native cap has nothing to cross-compile.) Text's in-crate pattern, in its
`#[cfg(all(test, feature = "runtime"))] mod tests`:

1. Build a `NativeBinding` over a loopback mailer with `ctx_binding()` —
   `test_mailer_and_rx()` gives a `Mailer` plus the `Receiver<EgressEvent>`
   its outbound bubbles to.
2. Construct fresh state (`TextCapabilityState::new()`) and a `NativeCtx`
   over the binding, then call the handler directly:
   `TextCapability::on_load_font(&mut state, &mut ctx, LoadFont { … })`.
3. Assert what the handler *sent* by draining egress with
   `assert_next_send_kind::<K>(&binding, &rx)` (which flushes the buffered
   outbound first, the way `NativeCtx`'s drop would at the end of a real
   turn), and assert what it *replied* with
   `decode_session_reply::<R>(&rx)`.

Three tests anchor the deferred-reply flow:
`load_font_forwards_read_with_context` drives `on_load_font` and asserts the
forwarded `aether.fs.read` has a nonzero correlation id;
`read_err_replies_load_font_err_via_request_context` feeds its correlated
`ReadResult::Err` and asserts the cap relays `LoadFontResult::Err` to the caller;
and `same_path_loads_reply_to_their_own_request_contexts` proves concurrent reads
for the same path still reply to their respective sessions.

For an end-to-end check across the real in-process boundaries — rendering, the
frame loop, and the capabilities explicitly installed by its reduced builder —
drive [SubstrateHarness](../testing/substrateharness-and-fleetharness.md) instead. It boots that
chassis from a Rust thread and sends mail through the same encode path the MCP
tool uses. Supply namespace roots when the scenario needs `aether.fs`.

## 7. Smoke it over MCP

If the cap fronts a load-bearing path, exercise it once live: bring up the
[MCP harness](../mcp-harness.md), `spawn_substrate`, `send_mail` one of
its kinds at the cap's mailbox name (`send_mail` a `draw` or `load_font`
at `aether.text`), and read `actor_logs` for `aether.text`. Unit tests and
clippy don't exercise the spawned-engine reply route (the
`SourceAddr::Component` reply gotcha lives there), so a live smoke catches
what the in-process test can't.

## Staleness

This recipe carries file paths and symbol names, so confirm them against
the current source before following it. The exemplar is
[`crates/aether-text/src/`][text] — if a name here doesn't
match what's in the tree, fix the recipe as part of your change. The
pointer is to the real cap, not a frozen copy, exactly so it tracks the
code.
