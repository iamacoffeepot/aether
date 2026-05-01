# ADR-0071: Driver Capabilities and Chassis Composition

- **Status:** Proposed
- **Date:** 2026-04-30

## Context

ADR-0070 introduced the `Capability` trait and refactored substrate sinks into `aether-substrate-core/src/capabilities/`. Phases 2–3 (handle, log, io, net, audio) shipped cleanly with one shape: capability claims a mailbox, spawns a dispatcher thread, runs an mpsc loop. Phase 3's last piece — render+camera — does not fit that shape.

Three things broke when we tried to extract render the same way:

1. **Render's threading model isn't dispatcher-thread.** ADR-0070's threading-model section already acknowledged this: render is "event-loop integrated," meaning the receiver is pumped from inside the chassis-binary's winit event loop. The chassis-binary, not the capability, owns the loop.
2. **Winit isn't a render-only concern.** On desktop, the winit event loop drives input dispatch, tick generation, window-mode mail, the capture-handoff proxy, and frame submission. Render is one consumer among several. Putting winit inside RenderCapability would make every other concern downstream of rendering, which is upside-down.
3. **The chassis-binary's `main()` does the loop driving today.** `aether-substrate-desktop/src/main.rs` is ~1300 lines, dominated by the winit `App` struct + its callbacks. The `Chassis` trait (`KIND`, `CAPABILITIES`, `run()`) was added in ADR-0035 to give each binary a uniform "what is this chassis" surface, but `run()` ended up holding the entire loop driver. ADR-0070 didn't address this — it deferred the question with "render last; wgpu/winit handle-passing needs the trait to have proven itself first." (This ADR also renames the `KIND` const to `PROFILE` to avoid clobbering the `Kind` / `KindId` / `KindShape` / `KindLabels` vocabulary throughout the data layer.)

The pattern that emerges from looking at all chassis is that **every chassis is a passive set + a single thread driver**. Desktop's driver is winit. Headless's is a `std::thread::sleep`-based tick loop. Hub's is a TCP accept loop. Audio already proves the "capability owns its driver thread" pattern internally (cpal's callback runs on a thread the capability claims). The chassis-level loop is just the same shape one level up: a chassis is composed of passive capabilities (dispatcher-thread sinks, accumulator sinks, control-plane handlers) plus exactly one driver capability that owns the chassis main thread.

Today's `Chassis` trait conflates two concerns:

- **Identity** — `PROFILE` ("desktop"/"headless"/"hub"), `CAPABILITIES` (HasGpu/HasWindow/HasTcpListener flags), the static facts that describe what kind of chassis this is.
- **Driving** — the `run(self) -> Result<()>` body that owns the main thread.

These should split. Identity is a static fact about a chassis type. Driving is what one specific capability does. Putting both in one trait forced the `App` struct (or its equivalents on headless and hub) to live in the chassis-binary crate, which is why the binary `main()` files are large.

A second forcing function: the test-bench. ADR-0067's `TestBench::start()` already builds something chassis-shaped without a winit loop — it boots the same passives the desktop chassis uses, but pokes at runnings directly via a synchronous test-driver API. After this ADR there's a clean name for what test-bench is: a chassis with no driver, where the test harness *is* the driver.

A third forcing function visible from ADR-0070's phase 5: extracting the hub server into `HubServerCapability`. The hub binary's main thread today is the TCP accept loop. Under this ADR's framing, that's a driver capability — same shape as the desktop's winit driver. ADR-0070 phase 5 lands cleanly under this ADR's trait shape rather than against ad-hoc structure.

## Decision

Split capabilities into two trait families, redefine the `Chassis` trait as static identity carrying composition info, and introduce a typed builder that distinguishes passives from the driver.

### Scope

In: `DriverCapability` / `DriverRunning` traits as a sibling family to `Capability` / `RunningCapability` from ADR-0070. Each chassis composes a set of passives plus exactly one driver. The driver owns the chassis main thread; passives are dispatcher-thread or accumulator-pump shape per ADR-0070.

In: `Chassis` trait redefined to carry static identity (`PROFILE`, `Driver` associated type, `Env` associated type) plus a uniform `build(env)` method that produces a `BuiltChassis<Self>`. The const is named `PROFILE` rather than `KIND` to avoid clobbering the `Kind` / `KindId` / `KindShape` / `KindLabels` vocabulary throughout the data layer.

In: typed builder (`BuiltChassis::<C>::builder()`) with `.with(impl Capability)` and `.driver(impl DriverCapability)` slots. Builder enforces "exactly one driver per chassis with a driver" structurally (type-state). Embedders that drive manually (test-bench) build a `PassiveChassis` via the no-driver path.

In: per-chassis-binary crate ownership of the chassis's driver capability. `aether-substrate-desktop` owns `DesktopDriverCapability` (winit + Window + Surface + per-frame tick). `aether-substrate-headless` owns `HeadlessTimerCapability` (std-timer cadence). `aether-hub` (per ADR-0070 phase 5) owns `HubServerCapability` (TCP accept loop) plus `HubClientCapability` (passive). `aether-substrate-core` owns every chassis-policy-agnostic passive.

In: render capability (ADR-0070 phase 3 piece) becomes winit-agnostic and exposes encoder-level primitives. The driver creates the per-frame encoder; render records into it; the driver submits and presents. This separation is what lets desktop (with surface) and test-bench (no surface) share the capability.

In: revised phasing for ADR-0070 phase 3 — render extraction now waits on this ADR's trait scaffolding to land first. ADR-0070 phases 1–2 (trait + handle/log/io/net/audio) are unchanged.

In: removal of ADR-0035's `Chassis::CAPABILITIES` static struct. The const-flag shape (`has_gpu`, `has_window`, `has_tcp_listener`) was right for ADR-0035's hardcoded chassis but gets worse after this refactor on two axes: the name clobbers the `Capability` trait, and the flags become an unreliable narrator (a chassis can in principle compose any combination of capabilities, not just the three the original struct enumerated). The const is removed from the trait. Hub protocol's `Hello` frame currently carries these flags — for this ADR's transition window the hub sends placeholder/zeroed values; the slot stays on the wire (preserving frame layout) but the data is not load-bearing until self-description lands. Future direction: a runtime introspection method on `BuiltChassis<C>` (or the chassis runtime) returning the actual list of booted capabilities — self-describing, accurate, extensible. Tracked as a follow-on issue, out of scope for this ADR.

Not in: layered configuration precedence (CLI > env > TOML > defaults). The chassis `Env` type is "the bag of resolved configs"; how `main()` populates it (env vars, TOML, CLI) is open. ADR-0041's planned precedence stack lives in a follow-up ADR or convention; it doesn't change the trait shape.

Not in: derive macros for chassis composition. Each chassis's `build(env)` body declares its passive list explicitly. A `#[derive(Chassis)]` attribute that auto-generates `build()` is reasonable future sugar but adds compile-time machinery with limited payoff at three chassis. Defer until a fourth chassis variant lands.

Not in: hot-reload of capability config. The `Env` is consumed at build time; rebuild requires restart. Revisit when a forcing function arrives.

Not in: conditional composition (e.g. desktop only adds `HubClientCapability` when `AETHER_HUB_URL` is set). The current pattern — `if let Ok(url) = std::env::var(...) { builder = builder.with(...) }` — works inside the `build()` body. The trait shape supports it without ceremony.

### Trait shapes

```rust
// Unchanged from ADR-0070.
pub trait Capability: Send + 'static {
    type Running: RunningCapability;
    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError>;
}
pub trait RunningCapability: Send + 'static {
    fn shutdown(self: Box<Self>);
}

// New in this ADR.
pub trait DriverCapability: Send + 'static {
    type Running: DriverRunning;
    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError>;
}

pub trait DriverRunning {
    /// Block the calling thread until the driver exits (window close,
    /// shutdown signal, accept-loop drain). On return, the chassis
    /// tears down every passive via RunningCapability::shutdown.
    fn run(self: Box<Self>) -> Result<(), RunError>;
}
```

`DriverCtx<'_>` extends `ChassisCtx<'_>` with typed access to already-booted passive runnings:

```rust
pub struct DriverCtx<'a> { /* ... */ }
impl DriverCtx<'_> {
    // ChassisCtx forwards.
    pub fn claim_mailbox(&mut self, name: &str) -> Result<MailboxClaim, BootError>;
    pub fn mail_send_handle(&self) -> Arc<Mailer>;

    // Driver-only.
    pub fn expect<R: RunningCapability + 'static>(&self) -> Arc<R>;   // panics if not booted
    pub fn try_get<R: RunningCapability + 'static>(&self) -> Option<Arc<R>>;
}
```

Driver `boot()` runs after every passive `boot()` has returned. `expect::<RenderRunning>()` retrieves the passive's running handle so the driver can call `pump_mail()` / `record_frame()` / etc. inside its `run()` body. The driver clones whatever `Arc<R>` references it needs into its own running for use during the loop.

### Chassis trait

The ADR-0035 `Chassis` trait is redefined:

```rust
pub trait Chassis: Sized + 'static {
    /// Stable identifier — "desktop", "headless", "hub". Used in boot
    /// logging, hub registration, and `engine_logs` filtering. Named
    /// `PROFILE` rather than `KIND` to avoid clobbering the data
    /// layer's `Kind` vocabulary.
    const PROFILE: &'static str;

    /// The driver capability this chassis composes. Builder constrains
    /// `.driver(d)` to `Self::Driver`; the chassis's `BuiltChassis<Self>`
    /// is paired with this driver type.
    type Driver: DriverCapability;

    /// Resolved configuration bag: every value the chassis needs at
    /// build time (window handle, namespace roots, net config, ...).
    /// Each chassis defines its own `Env` because chassis genuinely
    /// take different inputs. `main()` is responsible for populating
    /// `Env` — from env vars (today), TOML (later), CLI (later).
    type Env;

    /// Build the chassis from resolved config. Returns a typed
    /// `BuiltChassis<Self>` whose `run()` delegates to the driver's
    /// `DriverRunning::run()`.
    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError>;
}
```

`BuiltChassis<C: Chassis>` is the chassis instance. Type parameter on the chassis kind keeps `BuiltChassis<DesktopChassis>` distinct from `BuiltChassis<HeadlessChassis>` so generic harnesses can be written over `C`.

### Builder API

```rust
// Type-state encodes "exactly one driver" structurally.
pub struct Builder<C: Chassis, S: BuilderState> { /* ... */ }
pub trait BuilderState {}
pub struct NoDriver;     impl BuilderState for NoDriver {}
pub struct HasDriver;    impl BuilderState for HasDriver {}

impl<C: Chassis> Builder<C, NoDriver> {
    pub fn with<P: Capability>(self, cap: P) -> Self { /* ... */ }
    pub fn driver(self, d: C::Driver) -> Builder<C, HasDriver> { /* ... */ }

    /// No-driver build path. Used by test-bench: produces a chassis
    /// where the embedder is the driver.
    pub fn build_passive(self) -> Result<PassiveChassis<C>, BootError>;
}

impl<C: Chassis> Builder<C, HasDriver> {
    pub fn with<P: Capability>(self, cap: P) -> Self { /* ... */ }
    pub fn build(self) -> Result<BuiltChassis<C>, BootError>;
}

impl<C: Chassis> BuiltChassis<C> {
    pub fn run(self) -> Result<(), RunError>;   // delegates to driver
}

impl<C: Chassis> PassiveChassis<C> {
    pub fn capability<R: RunningCapability + 'static>(&self) -> Arc<R>;
}
```

Boot order at `build()` / `build_passive()`: passives in `.with()` declaration order, then (for `build()`) the driver. Each passive's `boot()` populates a typed running store; the driver's `DriverCtx::expect::<R>()` reads from it.

Errors propagate as `BootError` (ADR-0070 fail-fast). Any capability or driver failing `boot()` aborts the build before any user code runs. Built chassis can't be partially live.

### A sketch: desktop chassis

```rust
// aether-substrate-desktop crate.
pub struct DesktopChassis;
pub struct DesktopEnv {
    pub window: Arc<winit::window::Window>,
    pub event_loop: winit::event_loop::EventLoop<UserEvent>,
    pub log: LogConfig,
    pub io: NamespaceRoots,
    pub net: NetConfig,
    pub audio: AudioConfig,
    pub render: RenderConfig,
    pub driver: DesktopDriverConfig,
}

impl Chassis for DesktopChassis {
    const PROFILE: &'static str = "desktop";
    type Driver = DesktopDriverCapability;
    type Env = DesktopEnv;

    fn build(env: DesktopEnv) -> Result<BuiltChassis<Self>, BootError> {
        BuiltChassis::<Self>::builder()
            .with(LogCapability::new(env.log))
            .with(IoCapability::new(env.io))
            .with(NetCapability::new(env.net))
            .with(AudioCapability::new(env.audio))
            .with(HandleCapability::default())
            .with(RenderCapability::new(env.render))
            .driver(DesktopDriverCapability::new(
                env.window,
                env.event_loop,
                env.driver,
            ))
            .build()
    }
}

// aether-substrate-desktop/src/main.rs becomes:
fn main() -> Result<(), BootError> {
    let env = DesktopEnv::from_env()?;   // env-var read happens here
    DesktopChassis::build(env)?.run()
}
```

`DesktopDriverCapability::run()` body holds what's currently the desktop binary's `App` struct + winit event-loop driving. It reads its passives' runnings via `DriverCtx::expect` at boot, stores the relevant `Arc`s on its `Running`, and blocks on the winit loop. Per-frame work calls `render.pump_mail()`, drains the substrate mail queue, reads accumulator state, asks render to record passes into a wgpu encoder, blits to the swapchain, presents.

### Render capability — winit-agnostic, encoder-level primitives

`RenderCapability` (the ADR-0070 phase 3 piece) lives in `aether-substrate-core/src/capabilities/render/`. It claims `aether.sink.render` + `aether.sink.camera`, owns Device + Queue + Pipeline + Targets + capture-readback, exposes:

```rust
pub struct RenderRunning {
    // Sink-side accumulator state, exposed for the driver to read each frame.
    frame_vertices: Arc<Mutex<Vec<u8>>>,
    triangles_rendered: Arc<AtomicU64>,
    camera_state: Arc<Mutex<[f32; 16]>>,
    // Wgpu state, exposed so future capabilities can share the device.
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    targets: Mutex<Targets>,
    /* ... */
}

impl RenderRunning {
    pub fn pump_mail(&self);                                     // drain receivers, mutate accumulators
    pub fn record_frame(&self, encoder: &mut wgpu::CommandEncoder) -> Result<(), RenderError>;
    pub fn record_capture_copy(&self, encoder: &mut wgpu::CommandEncoder) -> CaptureMeta;
    pub fn finish_capture(&self, meta: &CaptureMeta) -> Result<Vec<u8>, String>;
    pub fn resize(&self, width: u32, height: u32);
    pub fn device(&self) -> &wgpu::Device;
    pub fn queue(&self) -> &wgpu::Queue;
    pub fn color_texture(&self) -> &wgpu::Texture;
}
```

Crucially: encoder-level primitives, not "render this frame." The driver creates the encoder, asks render to record its pass, optionally records a capture copy, copies offscreen → swapchain, submits, presents. RenderCapability doesn't know about swapchains, presentation, or surfaces; the driver does. RenderCapability also doesn't know whether it's running under winit (desktop) or under a test harness (test-bench with no driver).

Today render owns one pipeline (the main triangle pass). Future render-pass needs — if and when they arrive — are deferred. The encoder-primitive shape doesn't lock anything in: the choices when a second pipeline lands (grow the config, add a builder, add a second `record_*` method, refactor to a pass list) are all additive from today's API. Designing a render-graph trait from one example is guessing.

### Crate placement

```
aether-substrate-core/                — runtime + every passive capability
└── src/capabilities/
    ├── render/                       — winit-agnostic; pure wgpu
    ├── audio.rs
    ├── io.rs
    ├── net.rs
    ├── log.rs
    └── handle.rs

aether-hub/                           — ADR-0070 phase 5; owns hub-shaped capabilities
├── HubClientCapability (passive)
└── HubServerCapability (driver — used by aether-substrate-hub binary)

aether-substrate-desktop/             — driver + thin main
├── lib.rs                            — pub use DesktopDriverCapability + DesktopChassis
├── driver.rs                         — winit App body, ~1000 lines
├── chassis.rs                        — DesktopChassis Chassis impl + DesktopEnv
└── main.rs                           — ~30 lines: env reads + DesktopChassis::build(env)?.run()

aether-substrate-headless/            — driver + thin main
├── lib.rs / driver.rs / chassis.rs   — HeadlessTimerCapability + HeadlessChassis
└── main.rs                           — ~25 lines

aether-substrate-hub/                 — thin main, delegates to aether-hub
└── main.rs                           — ~20 lines: HubChassis::build(env)?.run()
```

Per-chassis binary crates exist for three reasons that survive after this ADR:

1. **Cargo features unify per-crate.** Headless cannot share a crate with desktop without compiling wgpu+winit+cpal into headless's binary. Per-crate boundary keeps headless's deployment artifact small.
2. **Driver capabilities have to live somewhere, and core is the wrong place.** `DesktopDriverCapability` is winit-coupled. Putting it in `aether-substrate-core` would force core to depend on winit (transitively via a feature). Substrate stays GUI-toolkit-agnostic.
3. **Deployment artifacts are intrinsically separate.** `cargo run -p aether-substrate-hub` is what `mcp__aether-hub__spawn_substrate` invokes. The binary names are real product surfaces.

### Boot ordering and cross-capability access

`Chassis::build(env)` boots passives in `with()` declaration order, then the driver. Each passive boot returns a `Running` that the builder stores in a typed map keyed by `TypeId`. The driver's `DriverCtx::expect::<R>()` retrieves an `Arc<R>` from the map.

Soft conventions on declaration order (ADR-0070 carried these, this ADR retains them):

- Log first, so other capabilities' boot tracing routes through the log capture.
- Render last among passives, so other capabilities can claim mailboxes without colliding with render's two.
- Driver always last (enforced by builder).

If hard ordering deps emerge later, an explicit `Capability::depends_on()` method is non-breaking to add.

### What `Chassis::run()` does

`BuiltChassis::run()` blocks the calling thread by delegating to the driver's `DriverRunning::run()`. When that returns (window closed, accept loop drained, etc.), the chassis tears down every passive via `RunningCapability::shutdown()` in reverse boot order. ADR-0063's fail-fast `SubstrateDying` broadcast still happens — `flush_now` flushes outbound, then exit.

### Phasing

This ADR supersedes ADR-0070's phase-3 ordering for render. Render extraction now follows trait scaffolding rather than preceding it. Revised plan:

1. **Ship `Capability` + `RunningCapability` from ADR-0070.** Done — phases 1–2 of 0070 are merged. Handle, log, io, net, audio capabilities live at `aether-substrate-core/src/capabilities/*`.
2. **Land driver-capability traits + builder type-state + `Chassis` redefinition + `BuiltChassis<C>` / `PassiveChassis<C>`.** No drivers extracted yet; existing chassis trait impls keep their `run()` body as the eventual driver-capability content. Mid-migration safety: the new trait family lands alongside the old `Chassis::run()`; the test-bench validates the no-driver path.
3. **Extract `DesktopDriverCapability`.** The current `App` struct + winit event-loop body moves into `DesktopDriverCapability::run()`. Desktop's `main.rs` shrinks to ~30 lines. The existing `RenderCapability`-shaped sinks (today registered inline as `register_sink` calls) stay where they are for this PR — render's full extraction is the next phase.
4. **Extract `RenderCapability` (ADR-0070 phase 3).** Render+camera mailboxes, accumulator state, wgpu pipeline, capture readback all move into `aether-substrate-core/src/capabilities/render/`. Test-bench rewires to use the same `RenderCapability` via a `PassiveChassis` build.
5. **Extract `HeadlessTimerCapability`.** Headless's std-timer tick loop moves into a driver capability. Headless `main.rs` shrinks parallel to desktop.
6. **`aether-hub` crate (ADR-0070 phase 4–5).** `HubClientCapability` + `HubServerCapability` land in the new crate. `aether-substrate-hub`'s main shrinks to a builder + run.

Each phase is its own PR. Phase 2 is mechanical (new traits, no behaviour change). Phases 3 and 5 are per-binary refactors that keep the diff focused. Phase 4 is the largest PR in this sequence — it carries wgpu/winit handle reorganization plus the encoder-level primitive shape.

### Resolved decisions

These were worked through during ADR drafting; recorded here for review.

1. **Two trait families, not one.** A single `Capability` with a `role()` accessor and an optional `run()` method was considered. Rejected: it pushes "can this be the driver" into runtime state and forces every capability to opt out of `run()`. Two families is more honest — drivers and passives genuinely have different lifecycles.
2. **Type-state on the builder, not runtime.** `Builder<C, NoDriver>` → `.driver(d)` → `Builder<C, HasDriver>` enforces "exactly one driver" at compile time. The runtime alternative (refuse second `.driver()` call at runtime) was considered and rejected for being detectable later than necessary.
3. **`PassiveChassis` for test-bench, not a no-op driver.** Test-bench is the driver. Adding a placeholder `EmbedderDriverCapability` would hide that fact behind ceremony. `Builder<C, NoDriver>::build_passive()` is honest about there being no auto-loop.
4. **Encoder-level render primitives, not a render-graph trait.** Today there's one render pipeline. A render-graph trait designed from one example is guessing. Encoder-level primitives keep the driver-vs-render boundary clean (driver owns encoder lifecycle; render owns recorded GPU work) without committing to a multi-pass shape. The choices for adding a second pipeline are additive from today's API. Revisit when a real second pipeline lands and its constraints make the right shape obvious.
5. **Driver capabilities live in per-chassis binary crates, not core.** Winit, std-timer scheduling, TCP accept loops are chassis-policy. Keeping them out of core preserves "core has no chassis-policy code" — the ADR-0070 invariant that's already paid off in the audio extraction.
6. **`Chassis::Env` is per-chassis, not a shared trait.** Each chassis takes genuinely different inputs (desktop needs Window, headless doesn't). A shared `ChassisEnv` trait was considered and rejected as ceremony — every concrete chassis defines its own Env struct, and that's fine.
7. **Layered config (CLI > env > TOML > defaults) is `Env`-construction-side concern.** ADR-0041 commits to a precedence stack. Under this ADR, the stack lives in `Env::from_layered(...)` — a constructor on the chassis's Env type, not in capability config or trait surface. Capabilities take resolved configs; how those configs got resolved is opaque.
8. **Derive macro deferred.** `#[derive(Chassis)]` that auto-generates `build(env)` is reasonable sugar but adds compile-time machinery for a 10-line body. Three chassis don't justify the macro yet. Revisit if a fourth chassis variant lands or if the per-chassis Env types start showing high-leverage common shape.
9. **Rename `KIND` to `PROFILE`.** ADR-0035's original `Chassis::KIND` const clobbers the `Kind` / `KindId` / `KindShape` / `KindLabels` vocabulary that runs through the data layer (ADR-0030/0032/0069). Reading `Chassis::KIND` next to `Kind::ID` is sloppy. Candidates considered: `LABEL` (mild overlap with `KindLabels`), `SLUG` (zero collision but informal), `VARIANT` (type-system-adjacent), `FLAVOR` (informal), `PROFILE` (deployment-shape vocab, zero collision). Picked `PROFILE` because the chassis's identifier really is a deployment profile — desktop / headless / hub are different shapes of the same runtime, and "the desktop profile" reads naturally in logs and registry contexts.
10. **Remove `Chassis::CAPABILITIES`, send placeholder over the wire.** Considered: rename to `FEATURES` (keeps the static const, drops the collision); keep as-is (accept the collision); remove outright. Picked remove. The const-flag shape is doubly wrong post-refactor — the name clobbers `Capability`, and the three boolean flags can't describe a chassis that composes capabilities outside the original `(gpu, window, tcp_listener)` set. The hub `Hello` frame keeps its slot but the substrate sends zeroed/placeholder values during the transition; consumers (today: hub UI surfaces, MCP `list_engines`) treat the field as unreliable until a self-describing introspection method lands. The wire-format slot survives so existing hub builds don't break framing; the data is just non-load-bearing.

## Consequences

**Positive**

- Chassis composition is uniform: passives + exactly one driver. Reading a chassis's `Chassis` impl tells you what passives it composes and what driver it uses.
- Chassis-binary `main()` files shrink dramatically. Desktop drops from ~1300 lines to ~30. The chassis-specific run-loop code lives inside the driver capability where it belongs structurally.
- Test-bench is honestly typed as `PassiveChassis<TestBenchChassis>` — no fake driver, no special-case.
- `RenderCapability` becomes winit-agnostic and encoder-primitive-shaped. Test-bench and desktop share one render capability with different drivers.
- Render is winit-agnostic and decoupled from presentation. The same `RenderCapability` works under any driver — desktop with a Surface, test-bench with no Surface, future drivers (browser, XR, offline recording) with whatever they have.
- Generic harnesses become possible: `fn launch<C: Chassis>(env: C::Env) -> Result<(), BootError>` factors panic-hook and log-capture init across chassis.
- ADR-0070 phase 5 (hub crate) lands cleanly under this ADR's trait shape rather than against ad-hoc structure. `HubServerCapability` is a driver; `HubClientCapability` is a passive; the substrate's fallback-router slot is the integration point.

**Negative**

- One more concept on the trait surface: drivers vs passives. Authors of new chassis or new capabilities must understand which family they're impling. The distinction is principled (different lifecycles, different access patterns) but it's still a concept tax.
- `Chassis` trait gains an associated `Env` type, which compounds rust-analyzer / docs noise modestly.
- Render extraction (ADR-0070 phase 3) now waits on this ADR's phase 2 trait scaffolding. Net delay on phase-3 close is one PR.
- The driver-capability extractions per chassis are not small PRs — the desktop driver moves ~1000 lines wholesale. Reviewers can scope by leaving behaviour unchanged in the same PR; semantic refactors happen separately.

**Neutral**

- Wire format unchanged. Mail dispatch, kind ids, custom sections — every byte boundary holds.
- ADR-0035 still describes the runtime / chassis split correctly; this ADR refines it again. Each refinement (0035 → 0070 → 0071) tightens the chassis's responsibility surface.
- ADR-0067's TestBench reorganizes around `PassiveChassis<TestBenchChassis>` but its public API (`bench.advance()`, `bench.capture()`) is unchanged.
- ADR-0041's planned config precedence stack is unaffected — it lives in `Env::from_layered(...)` per chassis, decoupled from this ADR's trait shape.

**Follow-on work**

- Phase-2 PR: trait + builder type-state + `Chassis` redefinition + `PassiveChassis<C>` scaffolding. No driver extracted; existing chassis impls keep their old shape side by side.
- Phase-3 PR: `DesktopDriverCapability` extraction.
- Phase-4 PR: `RenderCapability` extraction (the deferred ADR-0070 phase 3).
- Phase-5 PR: `HeadlessTimerCapability` extraction.
- Phase-6 PR: TestBench rewires to `PassiveChassis<TestBenchChassis>`.
- Phase-7 PRs: ADR-0070 phase 5 hub work — `aether-hub` crate, `HubClientCapability`, `HubServerCapability`.
- Issue: self-describing chassis introspection. `BuiltChassis<C>` (or the runtime) returns the actual list of booted capabilities; replaces the placeholder data the hub `Hello` frame currently carries in the `CAPABILITIES`-shaped slot. Eventual home for cross-cutting questions like "does this chassis have audio?" without hardcoded enum flags.
- Issue: layered-config ADR (CLI > env > TOML > defaults), once the stack ships beyond env-only.
- Issue: `#[derive(Chassis)]` macro, when chassis count justifies the proc-macro crate.
- Issue: hot-reload of chassis Env, if a forcing function arrives.

## Alternatives considered

- **Single trait with a `role()` discriminant.** `Capability` with `role() -> Passive | Driver` and an optional `run()` method on `RunningCapability`. Rejected — pushes "is this a driver" to runtime, forces every passive to either explicitly return-immediate from `run()` or leave it `unimplemented!()`. Two trait families is more honest about the distinct lifecycles.
- **Winit moves into RenderCapability directly.** RenderCapability owns Window + Surface + Device + Queue + winit event loop. Rejected — splits the wgpu pipeline code across render variants (desktop with winit, test-bench without), undoing the unification core::render gives us today.
- **Winit stays chassis-side; render is just a wgpu capability.** ADR-0070's existing framing. Rejected — keeps chassis-binary `main()` ~1000 lines, doesn't generalize the driver-capability concept that audio already proves and hub will need.
- **Collapse per-chassis binary crates into one with multiple `[[bin]]` targets.** One `aether-substrate` crate, `src/bin/desktop.rs`, `src/bin/headless.rs`, `src/bin/hub.rs`. Rejected — Cargo feature unification is per-crate, so headless's binary would link wgpu+winit+cpal whether it uses them or not. Per-crate boundary is what keeps deployment artifacts targeted.
- **Derive-macro chassis composition (`#[derive(Chassis)]`).** Auto-generate `build(env)` from an attribute listing passives + driver. Considered as the maximally-declarative form. Deferred — three chassis don't justify a proc-macro crate, and conditional composition (e.g. desktop only adds hub-client when `AETHER_HUB_URL` is set) requires either macro extension or escape hatches. Revisit when chassis count or composition complexity climbs.
- **Layered config (CLI > env > TOML > defaults) in this ADR.** Considered for completeness. Rejected as in-scope — `Env` construction is open by design; `Env::from_layered(...)` is a per-chassis constructor that can ship whenever the precedence stack does. ADR-0070 invariant ("substrate-core never reads env") survives either way.
- **`ChassisConfig` trait that capabilities query at boot.** A `dyn Config` trait the builder passes to capabilities, replacing per-capability config structs. Rejected — capabilities already take typed config structs (NetConfig, NamespaceRoots, etc.); the chassis Env is just "the bag of resolved configs." Adding a runtime config trait is ceremony.
- **Hot-reload of capability config via `Env` change observation.** Considered for forward-looking flexibility. Deferred — no current forcing function, and the right design (which capabilities can hot-swap their config, what state survives, what restarts) isn't clear from one example.
- **Pre-designing a render-pass trait or render-graph API.** Considered for "extensibility." Rejected — today there's one render pipeline; the right shape for multi-pass support will be obvious from the second pipeline's constraints. Encoder-level primitives keep today's design forward-compatible: when the time comes, growing the config, adding a builder, adding `record_*` methods, or refactoring to a pass list are all additive. Designing the trait now is guessing.
- **Render as an "orchestrator capability" hosting sub-pass capabilities.** Considered as the multi-pass extension shape — RenderCapability holds a registration slot, sub-pass capabilities depend on it and register at boot. Rejected — bifurcates the capability model (top-level capabilities vs render-pass capabilities), requires extending `ChassisCtx` with cross-capability boot-time access (which contradicts ADR-0070's "passives are independent"), and the third-party-render-pass use case isn't real (render passes are wgpu code, not wasm-loadable). When a second pipeline lands, render's internal pass set grows as construction config, not as a parallel capability hierarchy.

## References

- ADR-0035 — substrate-chassis split; this ADR refines the chassis trait again.
- ADR-0038 — actor-per-component dispatch; passives inherit the one-thread-per-mailbox model.
- ADR-0041 — substrate file I/O; commits to layered config precedence that lives in `Env` constructors under this ADR.
- ADR-0063 — substrate fail-fast; capability + driver boot errors abort the chassis.
- ADR-0067 — TestBench + scenario runner; TestBench rewrites as `PassiveChassis<TestBenchChassis>` under this ADR.
- ADR-0070 — native capabilities and chassis-as-builder; introduces the `Capability` trait this ADR extends with the driver-capability sibling family. ADR-0070 phase 3 (render extraction) is rephased to wait on this ADR's trait scaffolding.
