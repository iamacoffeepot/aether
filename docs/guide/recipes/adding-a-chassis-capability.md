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

Trace [`crates/aether-audio/src/`][audio] while you read.
`AudioCapability` owns the `aether.audio` mailbox: a capability with a
small config, a synth on a thread of its own, and per-session state — the
bank assemblies and track loads in flight, each holding the reply it owes.
It answers both kind flavors this recipe teaches. `aether.audio.note_on`
is fire-and-forget; `aether.audio.load_instrument` is reply-bearing, and
answered later. The synth and its DSP are most of the crate and none of
this recipe: read the identity, the `#[runtime] impl`, and the load path,
which between them exercise every step below. Verify its names against the
current source as you go — a capability is a recompile-class recipe, so
the symbols here rot faster than the explainers (see
[the staleness rule](#staleness)).

The identity lives in [`audio/lib.rs`][lib]; the state and the handler
list in [`audio/runtime/mod.rs`][runtime], with the handler bodies in
[`audio/runtime/handlers.rs`][handlers] and the load bookkeeping in
[`audio/runtime/load.rs`][load]; the owned kinds in
[`audio/kinds.rs`][kinds].

[audio]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src
[lib]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/lib.rs
[runtime]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/mod.rs
[handlers]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/handlers.rs
[load]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/runtime/load.rs
[kinds]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-audio/src/kinds.rs

## 1. Name the mailbox

A capability's mailbox name is its `NAMESPACE` const. Chassis-owned
mailboxes live under the `aether.<name>` prefix — `aether.audio`,
`aether.render`, `aether.fs`. Peers that declare `depends(AudioCapability)`
address the cap by type — `ctx.send::<AudioCapability>(&kind)` — which resolves to a
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
// audio/lib.rs — the identity half, always-on.
use aether_actor::actor;

/// `aether.audio` cap identity: a ZST carrying only the addressing —
/// `Addressable` (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind`
/// markers, and the singleton name-inventory entry, all emitted always-on
/// by `#[actor]`. Its `aether.fs` reads are declared.
#[actor(singleton, root, depends(FsCapability))]
pub struct AudioCapability;

// The runtime half — state, substrate-typed imports, and the `#[runtime]
// impl NativeActor` — lives in `runtime/`, gated once here on the cap's
// feature. The struct-hosted `#[actor]` above reads that module off disk
// to lift the identity.
#[cfg(feature = "runtime")]
mod runtime;
```

```rust
// audio/runtime/mod.rs — the runtime half, gated by the `mod runtime;`
// line above. The substrate-typed imports enter only on a native build.
use super::AudioCapability;
use super::kinds::{LoadInstrument, LoadInstrumentResult, NoteOn};
use aether_actor::runtime;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::chassis::error::BootError;

/// The cap's mutable state — the synth's event queue and the loads in
/// flight. `#[handler::<class>]`s receive it as `state: &mut Self::State`.
pub struct AudioCapabilityState { /* … */ }

#[runtime]
impl NativeActor for AudioCapability {
    type State = AudioCapabilityState;
    type Config = AudioConfig;
    const NAMESPACE: &'static str = "aether.audio";

    fn init(config: AudioConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<AudioCapabilityState, BootError> {
        // … start the synth `config.output` asks for, or run without one …
    }

    // Fire-and-forget: the handler returns `()`. `note_on` pushes one
    // event onto the synth's queue.
    #[handler::tell]
    fn on_note_on(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: NoteOn) {
        state.handle_note_on(ctx, mail);
    }

    // Reply-bearing, answered later: hold the owed `LoadInstrumentResult`
    // and forward the `.sfz` read with the held reply as its context.
    // See "audio's held-reply variant" below.
    #[handler::request]
    fn on_load_instrument(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: LoadInstrument,
    ) -> Pending<LoadInstrumentResult> {
        state.handle_load_instrument(ctx, mail)
    }
}
```

The pieces:

- **The identity ZST** — `pub struct AudioCapability;` carries no state.
  `#[actor(singleton)]` emits its always-on `Addressable` + `HandlesKind<K>`
  markers, so a wasm guest writing
  `ctx.send::<AudioCapability>(&kind)` under `depends(AudioCapability)`
  compile-checks even on a
  build where the runtime half is gated out.
- **`#[actor(singleton)]`** declares the cardinality — `singleton` for a
  chassis cap; `instanced` is the counterpart for per-instance actors (the
  engine proxy uses it). It reads the sibling runtime module off disk to
  lift the identity from the `#[runtime] impl`. It carries that module's
  own `use` items along with the markers, which is why the identity file
  above imports nothing but `actor` — spell the kind imports in the runtime
  module absolutely (`crate::kinds::…`, not `super::kinds::…`) and the
  identity never restates them. A `super`-rooted, glob, or `#[cfg]`-gated
  `use` is left behind (it would resolve differently one module down), so a
  kind reached that way still needs an import in the identity file.
- **`#[runtime] impl NativeActor for AudioCapability`** carries the
  behaviour. The `#[runtime]` attribute emits the runtime surface ungated
  — the `#[cfg(feature = "runtime")]` rides the `mod runtime;` line in
  `lib.rs`, so every impl already exists only on a build where the runtime
  module is present. No inner `#[cfg]` is needed.
- **`type State`** names the runtime struct holding the cap's mutable
  state. It lives in the feature-gated `runtime` module so it never
  compiles into a wasm build, and `#[handler::<class>]`s receive it as
  `state: &mut Self::State`.
- **`type Config`** is the struct of the cap's knobs — `AudioConfig`
  here — or `()` for a cap with none
  ([step 3](#3-give-it-a-config-if-it-needs-one)). The chassis resolves it
  and hands it into `init`.
- **`init(config, ctx)`** builds the runtime state (it returns
  `Self::State`, not `Self`). The mailbox is already claimed; `ctx` is a
  `NativeInitCtx` exposing `self_wake::<K>()` and `actor_probe()` for a
  thread the cap spawns, `publish_handle` for a driver-facing handle
  bundle, and `guest_ctx` for a wasm guest host — audio uses none of them:
  it starts its synth thread and keeps the handle. `init` runs before the dispatcher
  starts and before any peer's dispatcher runs — no mail yet. Return
  `Err(BootError::…)` to abort the chassis build.
- **`wire(&mut self, ctx) -> Result<(), BootError>`** (optional, default
  `Ok(())`) is the post-init mail-allowed hook: peers are addressable here,
  so subscribe to input streams or announce yourself from `wire`, not
  `init`. Return `Err(BootError::…)` to abort the chassis build; the cap's
  `unwire` still runs, with every cap that wired before it.
  **`unwire(&mut self, ctx)`** (optional) is the symmetric pre-shutdown
  hook. Audio needs neither.
- **`#[handler::<class>] fn on_x(state: &mut Self::State, ctx, mail: K)`** infers
  the kind from its third parameter. The first parameter is the runtime
  state, threaded explicitly because the identity carries none — take
  `&Self::State` for a read-only handler, `&mut Self::State` to mutate; the
  dispatcher owns the cap on one thread, so state is [plain fields, no
  locks](../foundations/actor-model.md). The handler receives `mail` by
  value.

### Reply-bearing handlers, and audio's held-reply variant

A self-contained reply-bearing handler returns its reply kind (`-> R`,
ADR-0112): the handler computes the answer this turn and the dispatcher
sends it back. A fire-and-forget handler returns `()`.

Audio's `load_instrument` answers **later**, because it can't answer this
turn — it must round-trip `aether.fs` first, once for the `.sfz` file and
once for each sample it names. It is still a `#[handler::request]`,
declared `-> Pending<LoadInstrumentResult>` (ADR-0243):
`ctx.hold::<LoadInstrumentResult>()` returns the
`Pending<LoadInstrumentResult>` receipt the handler returns, which sets its
row, and a `Held<LoadInstrumentResult>` ticket that answers the one
`LoadInstrumentResult` from a later turn. `hold` requires the reply kind to
implement `HeldReply`, whose hand-written `unanswered()` is the failure the
engine sends in its place if the capability closes first while the engine
keeps running (an engine teardown sends nothing):

1. **`on_load_instrument`** holds the reply. With no synth to load into it
   answers `Err` at once. Otherwise it forwards an `aether.fs.read` for the
   `.sfz` with
   `ctx.send_with_context::<FsCapability>(&read, AudioLoadContext::Instrument { held })`,
   which compiles because `AudioCapability` declares `depends(FsCapability)`.
   The held reply rides the read's request context, so each request has
   its own read and its own reply: nothing is shared between two loads,
   even of one file.
2. **`on_read_result`** takes the context back with
   `ctx.take_context::<AudioLoadContext>()`, and the context says which
   read this answers. For the `.sfz` it parses the file, moves the held
   reply into a `BankAssembly` kept in state under a minted `assembly_id`,
   and sends one read per sample, each with
   `AudioLoadContext::Sample { assembly_id, slot }`. A read or parse that
   fails answers the held reply with `Err` there and then.
3. When the last sample arrives it stages the decode and assembly off the
   actor's turn
   ([ADR-0243](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0243-typed-held-replies.md)
   §9). The task owes no reply of its own: the assembly stays in state
   with its held reply, and the task carries only a kind naming it:

   ```rust
   ctx.stage_blocking_with::<BankAssemblyOutput, BankAssemblyKey>(BankAssemblyKey { assembly_id })
       .start(ctx, move || assemble_bank(name, &regions, &sample_bytes, target_rate));
   ```
4. **`on_instrument_assembled`** (the `#[handler(task)]` completion)
   receives `TaskDone<BankAssemblyOutput>`, takes its `BankAssemblyKey`
   with `ctx.take_context()`, removes the assembly, registers the bank
   under a session-scoped instrument id, and answers the request with
   `assembly.held.answer(ctx, …)`.

Trace the full correlation in `runtime/handlers.rs` and `runtime/load.rs`
rather than reading it re-explained here.

The kinds a handler receives must exist in the substrate kind inventory
so the dispatcher can decode the wire bytes. Audio **owns** its kinds:
`NoteOn`, `LoadInstrument`, `LoadInstrumentResult`, and the rest live in
[`audio/kinds.rs`][kinds] beside the cap that dispatches them (ADR-0121),
always-on and wasm-safe (they need only `aether-data` + `serde`). Their
`inventory::submit!` descriptor entries ride the `Kind` derive, so
`aether_kinds::descriptors::all()` surfaces them. Registering a kind whose
decode the dispatcher must find is the separate *Adding a substrate kind*
recipe.

### The reply gotcha

`ctx.reply(&result)` / `ctx.reply_to(source, &result)` — the
`NativeBinding` handler-reply path — is the complete router: it reaches
every `SourceAddr`, including the `Component` local-RPC-server reply target
an MCP-spawned engine tags. A reply that must outlive the handler (answered
later from an embedder loop, say) retains the request's `InboundMail` guard
and replies through it, which takes the same route. Reply through one of
these and nothing else: a hub-only reply path once answered just `Session`
and `EngineMailbox` senders and silently dropped the `Component` reply, so
an MCP-spawned caller's reply never landed (iamacoffeepot/aether#1321).

## 3. Give it a config if it needs one

A cap with tunables declares a struct and derives `Config` on it, so its
knobs flow through the same config-file/env/argv source stack every other
cap uses rather than a raw `env::var` read. Audio's is `AudioConfig`: where
the synth's samples go, and the sample rate to ask for. A cap with no knobs
uses `type Config = ();`. That dance —
`#[derive(aether_substrate::Config)]`, the emitted overlay, the struct's
TOML section, and flattening the overlay into the chassis CLI — is
[Configuration](../systems/configuration.md). You do not hand the resolved
struct to the builder: composing the cap declares its config *type*, and the
chassis resolves the value off the source stack. Keep an empty config a struct
rather than `()` if you expect knobs later, so the composition site doesn't
churn when the first one lands.

## 4. Register with the chassis builder

A mailbox is only on the air once a chassis builder claims it. The
builder is `aether_substrate::chassis::builder::Builder`; you add a cap
with `with_actor::<X>(params)` ([ADR-0070][adr70] / [ADR-0071][adr71]),
where `params` is the cap's composer-supplied construction input (`A::Params`),
not its config:

```rust
builder.with_actor::<AudioCapability>(())
```

Composing a cap also accumulates its `A::Config` member into the chassis config
aggregate (ADR-0156), which is what puts its knobs in `--print-config` and the
unknown-key sweep. When a composer needs to pin an explicit config value rather
than let the stack resolve it, `with_actor_configured::<X>(params, config)` is
the paired form — the `A::Config` type binds the value to the actor at the call,
so an orphaned override can't be written. Audio's handler tests boot it that
way, to run the synth with no device:

```rust
builder.with_actor_configured::<AudioCapability>((), AudioConfig { output: AudioOutput::Null, requested_sample_rate: None })
```

Where that line goes depends on which chassis should carry the cap:

- **Desktop and headless together** — add it to `with_common_caps` in
  [`crates/aether-chassis/src/boot.rs`][common], the
  shared composition those two chassis call. `FsCapability` lives here. Put
  a cap here only when both chassis serve it and everything it depends on;
  `AudioCapability` needs an audio device, which headless does not serve,
  so it is not here. Adding it to the `.with_actor::<_>()` chain is all it takes: the
  `--describe` manifest is claim-derived ([ADR-0155][adr155]), so a cap
  appears in the roster the moment it claims a mailbox — there is no
  parallel namespace list to keep in lockstep.
- **The substrate-harness chassis** — the in-process harness does not call
  `with_common_caps`; it has a separate, reduced builder chain in
  [`crates/aether-harness-substrate/src/chassis.rs`][substrateharness];
  add the capability there too when scenarios should drive it, and thread any
  required config through `SubstrateHarnessEnv`.
- **One chassis only** — add it to that chassis's own builder chain:
  `desktop/chassis.rs`, `headless/chassis.rs`, or `hub/chassis.rs` in
  the chassis crates. `AudioCapability` is composed in the desktop chain
  alone. The desktop
  `RenderCapability` is booted as a pumped actor by the desktop driver
  (`ctx.boot_pumped_actor::<RenderCapability>(…)`) because it must run on the
  winit thread.

A chassis that cannot serve a capability composes nothing for it, not a stub
claiming its mailbox ([ADR-0232 §6][adr232]), so a component that depends on it
is refused at load there.

A pumped actor is **reserved at the Claim stage**, before any passive's
`init`: on a driver chassis the driver's `claim` hook calls
`ctx.claim_driver_mailbox(…)`, and on a passive chassis the composition calls
`reserve_pumped::<A>()` and terminates in `build_passive_with_start(|passive| …)`,
whose start boots the actor with `passive.boot_pumped_actor::<A>(…)`. The
reservation is a live route, so a passive may declare `depends(A)` on a pumped
actor, and mail sent to
the slot waits in its inbox until the pump boots. A reservation that is never
booted fails the build, naming the slot: a plain `build_passive()` boots
nothing, so a reservation fails it too.

The builder claims `A::NAMESPACE` as it boots each cap and enforces
**one claimant per name**: a second cap claiming an already-owned mailbox
fails the build with `BootError::MailboxAlreadyClaimed { name }` (or a
namespace-ownership error for a `NAMESPACE` collision across types). This
is the guarantee that a well-known name has at most one claimant in any
composition, so no cap silently shadows another.

Boot is multi-pass across every cap: `claim → init → wire → spawn`,
synchronized so that at `init` time every peer mailbox is claimed and at
`wire` time every peer has an instance. Declaration order is boot order.

[adr70]: https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0070-native-capabilities-and-chassis-as-builder.md
[adr71]: https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0071-driver-capabilities-and-chassis-composition.md
[adr155]: https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0155-staged-chassis-boot-and-claim-derived-describe.md
[adr232]: https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0232-flat-ctx-send-verbs.md
[common]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-chassis/src/boot.rs
[substrateharness]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-harness-substrate/src/chassis.rs
[fsruntime]: https://github.com/iamacoffeepot/aether/blob/main/crates/aether-fs/src/runtime/mod.rs

## 5. Passive cap or driver?

Most capabilities are **passive**: they sit on a dispatcher and answer
mail, added with `with_actor`. `AudioCapability` is passive. An executable chassis
also composes exactly one **driver** — the cap that owns the chassis main thread
and its lifetime (the winit loop on desktop and the std timer on headless). The
shared `SignalDriverCapability` from `aether-chassis`, which the hub and the
Bloomery compose, instead owns the `SubstrateBoot` and blocks that thread on
SIGINT/SIGTERM; `RpcServerCapability`, a passive actor, owns the socket
listener. A driver implements `DriverCapability` (not `NativeActor`) and is
supplied with `.driver(d)` rather than `.with_actor`; the type-state builder
enforces exactly one. The in-process SubstrateHarness uses `build_passive_with_start` and lets its
embedder drive it, so it deliberately has no driver capability.

If the cap drives — owns a loop or a peripheral — its name carries
`Driver`: `DesktopDriverCapability`, `HeadlessTimerDriverCapability`. A
plain `FooCapability` reads as a passive sink. Don't name a passive cap
`*DriverCapability`. Most new caps are passive; you reach for a driver
only when standing up a new chassis kind.

### Heavy native deps

Audio's runtime half pulls `cpal`, a native-only dependency, through its
generic `runtime` feature on the `mod runtime;` line:
`#[cfg(feature = "runtime")] mod runtime;`. The identity markers stay
always-on (so guests still address the cap by type) while `cpal` only
enters when the feature is on. The renderer's `wgpu` and `fontdue` gate
the same way. A cap whose runtime
needs no heavy dep still gates its `mod runtime;` line on the generic
`runtime` feature, so the split holds without minting a cap-specific gate.

## 6. Test it in-process

A native cap compiles into the substrate, so its tests drive a real
handler with mail — no wasm, no FFI, no MCP session. (`export!`'s FFI
shims are wasm32-only and belong to *components*, not capabilities; a
native cap has nothing to cross-compile.) The in-crate pattern is the
pumped rig, which audio's own unit tests do not use; read it in
[`crates/aether-fs/src/runtime/mod.rs`][fsruntime], whose `PumpedFs`
fixture boots `FsCapability` this way:

1. Boot the cap pumped on a real chassis, driven the way a pumped chassis
   driver drives its slot (ADR-0161 §Decision 2): `fresh_substrate()` gives
   the `(Arc<Registry>, Arc<Mailer>)` seed, `boot_bare_test_chassis(&registry,
   &mailer)` builds the passive chassis over it, and
   `PumpedDriver::boot(chassis, config, params)` boots the cap on that
   chassis and drains once. A cap that depends on another registers a
   stand-in for it first, with `testing::registered_ref`, whose closure
   forwards each dispatch it receives onto a channel the test reads.
2. Send mail through the pumped slot, never call a handler directly:
   `cap.send_and_settle(cap.chassis().actor_ref::<FsCapability>(), &Read { … }, None)`
   tracks the send as a chassis root and pumps the slot until that root
   settles, so the mail runs through the cap's `#[actor]`-generated dispatch
   exactly as production would — including a `-> Pending<R>` handler's
   `Unchecked` arm, which accepts the returned receipt itself. The test never
   hand-disarms a `Pending`.
3. Assert what the handler *sent* by reading the stand-in's channel (the
   `registered_ref` closure forwards each dispatch it receives), and assert
   what it *replied* with by sending as a session-origin mail
   (`Some(ReplyTarget::Session { session, correlation })`) and decoding the
   settled root's `EgressEvent::ToSession` off the egress receiver
   `fresh_substrate_and_rx` returns beside the seed. For a look at state no
   sent or replied mail exposes, `cap.host_turn(|state, ctx| { … })` runs a
   closure directly against `State` between sends.

A test that needs a proven peer (a subscriber, a sender, a shard) registers
it with `testing::registered_ref`, which returns the peer's reference, and
`testing::registered_binding` returns the binding beside its own reference.
`registered_ref` takes a root name or a `/`-rendered lineage path, so a test
can stand a route at a nested position (a collision a spawn will claim), and
`testing::drop_ref` retires it.
A test that needs a local component as the inbound's sender takes the
`Source`, and the chain root when it needs one, from a mail its
`registered_binding` binding actually sent, and writes a root that is only a
token as `testing::token_root(n)`, so it names no mailbox. The in-flight
lineage a `NativeCtx` constructor takes is optional: `None` for a context with
no inbound chain, `Some(root)` for one running inside a chain.

Three tests in `aether-audio`'s `tests/handlers.rs` anchor the held-reply
flow, over a `SubstrateHarness` composing the real cap beside `aether.fs`.
`load_instrument_assembles_the_bank_and_assigns_increasing_ids` proves the
read, the sample fan-out and the assembly end in one answer per request.
`concurrent_bank_loads_of_one_sample_answer_their_own_callers` proves two
loads in flight at once, naming the same sample, each answer their own
caller, because each read is told apart by its request context and not by
its path. `load_instrument_replies_err_for_each_failed_step` proves every
failure arm answers the held reply.

For an end-to-end check across the real in-process boundaries — rendering, the
frame loop, and the capabilities explicitly installed by its reduced builder —
drive [SubstrateHarness](../testing/substrateharness-and-fleetharness.md) instead. It boots that
chassis from a Rust thread and sends mail through the same encode path the MCP
tool uses. Supply namespace roots when the scenario needs `aether.fs`.

## 7. Smoke it over MCP

If the cap fronts a load-bearing path, exercise it once live: bring up the
[MCP harness](../mcp-harness.md), `spawn_substrate`, `send_mail` one of
its kinds at the cap's mailbox name (`send_mail` a `note_on` or a
`set_master_gain` at `aether.audio`), and read `actor_logs` for
`aether.audio`. Unit tests and
clippy don't exercise the spawned-engine reply route (the
`SourceAddr::Component` reply gotcha lives there), so a live smoke catches
what the in-process test can't.

## Staleness

This recipe carries file paths and symbol names, so confirm them against
the current source before following it. The exemplar is
[`crates/aether-audio/src/`][audio] — if a name here doesn't
match what's in the tree, fix the recipe as part of your change. The
pointer is to the real cap, not a frozen copy, exactly so it tracks the
code.
