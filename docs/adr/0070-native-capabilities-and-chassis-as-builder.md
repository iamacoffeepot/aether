# ADR-0070: Native capabilities and chassis-as-builder

- **Status:** Proposed
- **Date:** 2026-04-30

## Context

`aether-substrate-core` is the substrate runtime that every chassis binary (`aether-substrate-desktop`, `aether-substrate-headless`, `aether-substrate-hub`) shares. Today it holds three different kinds of code in one crate:

1. **Substrate runtime** — mail scheduler (ADR-0038 actor-per-component dispatch), mailbox registry, wasmtime host, `aether.kinds` manifest walker (ADR-0028/0032/0033), control-plane dispatch (`load_component`/`replace_component`/`drop_component`/`subscribe_input`), hub client + bubble-up (ADR-0037), substrate fail-fast (ADR-0063).
2. **Sink dispatchers and their state** — render, camera, io, net, audio, log, handle. Each is a hand-rolled match-on-`kind_id` loop plus a private state struct (wgpu device, adapter registry, ureq agent, cpal voice table, tracing bridge, handle store).
3. **Chassis-binary glue** — currently each binary picks which sinks to register at boot via inline wiring code. Desktop registers all seven; headless skips render+camera+audio; hub skips most.

Three pressures push against the current shape:

- **Adding a sink is N copies of the same boilerplate.** Each sink reimplements the dispatch loop, the kind-id match, the request/reply correlation. ADR-0050-replacement (LLM sink) and image-gen sinks are imminent; sink count is climbing from 7 toward 10+. Without a shared shape, "junk drawer" gets worse.
- **Chassis composition is invisible at the binary level.** Reading `aether-substrate-headless`'s `main.rs` doesn't tell you which sinks it has — that's buried inside `aether-substrate-core`'s boot path keyed by feature flags and runtime checks. There's no declarative "this chassis = these capabilities" surface.
- **Test composition is hardcoded.** `aether-substrate-test-bench` (ADR-0067) ships one chassis configuration. Tests that want a chassis with, say, only io and log have no path; they get the full TestBench whether they want it or not.

The pattern emerging from all seven sinks is that each one is a **gateway to a substrate capability** — render+camera gate the render pipeline, io gates the filesystem, net gates HTTPS egress, audio gates the cpal synth, log gates the tracing bridge, handle gates the handle store. The natural unit isn't the sink (mailbox dispatcher), it's the capability (the bundle of sink + state + bootstrap + lifecycle that gives one chassis-policy concern its mail-side surface).

ADR-0035 cut runtime from chassis-specific code by introducing the `Chassis` trait and per-chassis binaries. That left the *contents* of each chassis hand-wired. This ADR takes the next step: make the contents declarative.

The capability shape is also notable for its symmetry with wasm components. A wasm component owns kinds (`aether.kinds` custom section), a dispatcher (`receive_p32` export), state (linear memory), and lifecycle (`init`/`on_replace`/`on_drop`). A native capability would own kinds, a dispatcher (Rust fn), state (Rust struct), and lifecycle (`boot`/`shutdown`). Different deployment (compiled in vs loaded at runtime), same conceptual unit. "Native components that cannot be recompiled without making a new binary" is the working framing.

## Decision

Introduce a `Capability` trait in `aether-substrate-core` and refactor the existing chassis sinks into in-crate submodules under `src/capabilities/`, each implementing the trait. The hub client (today inline in the substrate as bubble-up) moves out of the substrate into a new `aether-hub` lib crate alongside a new `HubServerCapability`. Chassis binaries compose capabilities declaratively via a `Chassis::builder()` API.

### Scope

In: native sinks currently inside `aether-substrate-core` (render, camera, io, net, audio, log, handle). After this ADR they live as submodules of `aether-substrate-core`, addressed through the `Capability` trait.

In: hub client (currently substrate-side bubble-up). After this ADR it lives as `HubClientCapability` in the new `aether-hub` lib crate, alongside `HubServerCapability` (the TCP listener that today is inline in `aether-substrate-hub`). The substrate exposes a generic fallback-router slot the hub client claims; the substrate itself has zero hub knowledge.

Not in: per-capability standalone crates *for the sinks*. Submodules in `aether-substrate-core` suffice for the structural win (declarative composition, encapsulated state + lifecycle, testbench composition). The hub crate is the one exception — hub-flavored code doesn't belong in core, and the binaries that need it (desktop, headless, hub) span multiple chassis crates.

Not in: per-capability trunk rlib split of `aether-kinds`. Orthogonal refactor; could come before, after, or never. Today's flat `aether-kinds` continues to hold sink kind types.

Not in: the sink kit (`host_handlers!` macro, `EchoReply` trait, `ChassisGate` const). Deferred to a follow-up ADR once 5+ capability submodules have hand-rolled dispatch loops to factor commonality from. Pre-designing the kit from two examples is guessing; five is enough.

### Trait shape

```rust
pub trait Capability: Send + 'static {
    type Running: RunningCapability;
    fn boot(self, ctx: ChassisCtx<'_>) -> Result<Self::Running, BootError>;
}

pub trait RunningCapability: Send {
    fn shutdown(self: Box<Self>);
}
```

`ChassisCtx<'_>` is the substrate-side handle bundle a capability needs at boot. The ctx is shared (`&mut ChassisCtx<'_>`) across every `boot()` call in the builder — there is one mailbox registry, one mail-send router, one fallback-router slot, all shared:

- **Mailbox claim** — `&mut self` method that returns the `mpsc::Receiver` for a known `MailboxId` from `aether-kinds::mailboxes`. The capability owns the receiver afterward; the slot can only be claimed once.
- **Mail-send handle** — `Clone`-able sender that injects mail into the substrate's routing table. Capabilities clone this into their dispatcher state for sending mail to other mailboxes.
- **Fallback-router slot** — `&mut self` method that lets at most one capability register a `fn(envelope) -> Routed | Dropped` handler the substrate calls when local mailbox lookup fails. Generic — the substrate does not know what a "hub" is. `HubClientCapability` (in the `aether-hub` crate) is the standard implementation.
- **Config source** — env-var / file-config accessor.

`BootError` is the proposed-and-shipped variant of substrate fail-fast (ADR-0063): if a capability's boot returns `Err`, the chassis aborts before any user code runs. Capabilities cannot be partially booted.

### Crate structure

```
aether-substrate-core/
└── src/
    ├── lib.rs                      — re-exports + Chassis builder
    ├── runtime/                    — mail scheduler, registry, kind manifest walker, fallback-router slot
    ├── control_plane/              — load/replace/drop_component, subscribe_input
    ├── chassis.rs                  — Capability trait, ChassisCtx, builder
    └── capabilities/
        ├── mod.rs
        ├── render.rs               — render + camera sinks (gated by `render` feature)
        ├── io.rs                   — io sink + adapter registry
        ├── net.rs                  — net sink + ureq agent
        ├── audio.rs                — audio sink + cpal voice table (gated by `audio` feature)
        ├── log.rs                  — log sink + tracing bridge
        └── handle.rs               — handle sink + handle store
```

`aether-substrate-core` after this ADR has zero hub knowledge — no dep on `aether-hub-protocol`, no hub-specific code in the substrate. The fallback-router slot is generic; if no capability claims it, unresolved mail is simply dropped (with a `tracing::warn!`).

Cargo features (`render`, `audio`) replace what would have been per-crate dependency gating. Headless's binary doesn't enable them, so wgpu/cpal stay out of its compile graph the same way they do today.

A new `aether-hub` library crate is introduced alongside this ADR. It exports `HubClientCapability` (claims the fallback-router slot, forwards unresolved mail over TCP, also claims the well-known `aether.hub.broadcast` mailbox for observation egress) and `HubServerCapability` (TCP listener, session router). Both depend on `aether-hub-protocol` for framing. `aether-substrate-hub` (the binary) depends on `aether-hub` and uses `HubServerCapability`; `aether-substrate-desktop` and `aether-substrate-headless` depend on `aether-hub` and use `HubClientCapability` when `AETHER_HUB_URL` is set.

### Threading model

Capabilities own their threading. The substrate hands a capability the mailbox receiver (and a clonable mail-send handle); the capability decides whether to spawn a dispatcher thread, integrate into an existing event loop, or both. `RunningCapability::shutdown(self: Box<Self>)` is responsible for joining whatever the capability spawned.

The three threading shapes among today's sinks (and how they map to this trait):

- **Single dispatcher thread** (io, net, log, handle, hub_client): capability spawns one OS thread in `boot()` that loops on `recv()` and processes mail. Standard mpsc actor.
- **Dispatcher + driver-owned thread** (audio): capability spawns one dispatcher thread that mutates voice-table state, *plus* hands cpal a callback that runs on cpal's own driver-owned thread. The capability owns lifecycle of the dispatcher; cpal's thread joins implicitly when the stream is dropped.
- **Event-loop integrated** (render): capability spawns *no* dispatcher thread. It pumps its mailbox receiver from inside the chassis-binary's winit event loop, interleaved with frame submission. The render capability's `boot()` returns `Running` immediately; the actual mail consumption happens inside the binary's per-frame tick.

A substrate that imposed "one thread per capability" would over-constrain audio and break render. Letting capabilities own their threading keeps the trait simple and the model honest.

### Inter-capability communication

Default: capability-to-capability via mail through the substrate's mail scheduler. Same mechanism wasm components use to reach sinks. When capability A wants to mail capability B, A clones the mail-send handle from `ChassisCtx`, addresses an envelope to B's `MailboxId`, and the substrate routes it to B's mpsc queue (the receiver B got at boot).

Three exceptions:

1. **Tightly coupled state stays internal.** `RenderCapability` exposes two mailboxes (`render` and `camera`) with shared wgpu state held inside the capability struct. No cross-capability mail; the shared state is encapsulated.
2. **Tracing is direct.** Capabilities call `tracing::event!` in the conventional Rust way; the log capability owns the global subscriber that catches those events and forwards to `aether.sink.log`. Routing tracing through mail would defeat tracing's hot-path guarantees.
3. **Hub egress is a normal mail send.** Capabilities that emit observation mail to `hub.claude.broadcast` (e.g., log) just send mail addressed to that mailbox. If `HubClientCapability` is loaded, it has claimed the mailbox and forwards over TCP. If not, the mailbox doesn't exist and mail drops via the fallback-router (or is dropped outright if no fallback claimed). No special hub-mail-send API on `ChassisCtx`.

### Chassis composition

Each chassis binary becomes declarative:

```rust
// aether-substrate-desktop/src/main.rs
fn run(window: Arc<Window>) -> Result<(), BootError> {
    let mut builder = Chassis::builder()
        .with(LogCapability::new())          // first, so other capabilities' boot logs route correctly
        .with(IoCapability::default())
        .with(NetCapability::with_allowlist(...))
        .with(AudioCapability::new())
        .with(HandleCapability::new())
        .with(RenderCapability::new(window));
    if let Ok(url) = std::env::var("AETHER_HUB_URL") {
        builder = builder.with(HubClientCapability::new(url));
    }
    builder.build()?.run()
}

// aether-substrate-headless/src/main.rs — same builder, fewer capabilities
fn run() -> Result<(), BootError> {
    let mut builder = Chassis::builder()
        .with(LogCapability::new())
        .with(IoCapability::default())
        .with(NetCapability::with_allowlist(...))
        .with(HandleCapability::new());
    if let Ok(url) = std::env::var("AETHER_HUB_URL") {
        builder = builder.with(HubClientCapability::new(url));
    }
    builder.build()?.run()
}

// aether-substrate-hub/src/main.rs — uses HubServerCapability
fn run(addr: SocketAddr) -> Result<(), BootError> {
    Chassis::builder()
        .with(LogCapability::new())
        .with(HandleCapability::new())
        .with(HubServerCapability::new(addr))
        .build()?
        .run()
}
```

Each binary opts into hub bridging by adding `HubClientCapability` to its builder. The hub binary uses `HubServerCapability` instead — they are siblings in the `aether-hub` crate, not specially privileged in the substrate.

### Phasing

1. **Land traits + builder.** `Capability`, `RunningCapability`, `Chassis::builder()`, `ChassisCtx`, `BootError`, fallback-router slot, `src/capabilities/mod.rs`. No sinks moved yet. Existing chassis boot paths keep working unchanged.
2. **Extract first capability.** `handle` or `log` — least state, no chassis-feature gating. Validates the trait shape end-to-end. Chassis builds with both old (legacy boot) and new (capability boot) wired side by side until each remaining sink is migrated.
3. **Extract remaining core sinks, one PR each.** `io` → `net` → `audio` → `render`+`camera`. Render last; wgpu/winit handle-passing needs the trait to have proven itself first. Each PR removes the corresponding legacy boot code.
4. **Create `aether-hub` lib crate; extract `HubClientCapability`.** Move bubble-up from `aether-substrate-core` into `HubClientCapability` in the new crate. Drop the `aether-hub-protocol` dep from core. Desktop and headless adopt the capability via opt-in builder line.
5. **Implement `HubServerCapability` in `aether-hub`; refactor hub binary.** The TCP listener currently inline in `aether-substrate-hub`'s `main.rs` becomes a `Capability` impl. Hub binary's main becomes a `Chassis::builder()` call.
6. **TestBench rewrite.** `aether-substrate-test-bench::TestBench::start()` becomes a thin wrapper that builds a chassis with the capabilities tests need. Per-test composition replaces today's hardcoded subset.
7. **(Deferred) Sink kit ADR.** Once 5+ capabilities exist with hand-rolled dispatch, extract `host_handlers!` macro + `EchoReply` trait + `ChassisGate` const into `src/capabilities/common.rs`.

Each phase is its own PR. Phase 1 is mechanical; phases 2-3 are one capability each; phase 4-5 are paired (hub crate creation + extractions); phase 6 is mechanical; phase 7 is a separate ADR.

### Resolved decisions

These were worked through during ADR drafting; recorded here as load-bearing for review.

1. **Render + camera: one capability with two mailboxes.** Camera is configuration of the render pipeline (publishes a view-proj matrix that render reads), not a peer concern. Same shape as a hypothetical "bg color" mailbox; you wouldn't extract a separate capability for that. The trait supports N mailboxes per capability — `boot()` claims as many as it owns.
2. **Hub client lives in `aether-hub`, not in the substrate.** The substrate exposes a generic fallback-router slot; `HubClientCapability` is the standard implementation. `aether-substrate-core` ends this ADR with zero hub knowledge — no `aether-hub-protocol` dep, no hub-specific code. Symmetric: `HubServerCapability` lives alongside in `aether-hub`. Other binaries opt into either by adding the capability to their builder.
3. **Boot ordering: declaration order.** Builder boots capabilities in `with()` call order. Real boot-time deps between capabilities are weak (the soft preference "log first" is captured by writing it first in the binary; tracing falls through to the default subscriber if log isn't up yet). If hard deps emerge later, an explicit `Capability::depends_on()` method is non-breaking to add.
4. **`ChassisCtx` lifetime: `&mut ChassisCtx<'_>` shared across boot calls.** One ctx live for the duration of the build. Mailbox-claim is `&mut self` (consumes the slot, fails on duplicate); mail-send and config accessors return `Clone`-able handles capabilities stash into their dispatcher state. No split BootCtx vs RunningCtx — simpler, fits how Rust ecosystem does this elsewhere.
5. **`describe_capability` MCP tool: deferred.** Symmetric to `describe_component` (which walks loaded wasm components and surfaces handler vocabulary). Useful for Claude sessions to see "what does this chassis actually do" without grepping a binary. Tracked as a follow-up issue, not in this ADR; depends on the trait being shipped first.
6. **Mid-migration safety: builder errors on duplicate `MailboxId` claim.** Phases 2-5 ship side-by-side legacy + capability boot until each PR removes the legacy code. Builder must reject a second claim for the same `MailboxId` so the situation is loud, not silent. Each capability extraction PR removes the legacy boot path in the same diff.

## Consequences

**Positive**

- Chassis composition is declarative and visible at the binary level. Reading `aether-substrate-desktop/src/main.rs` tells you exactly which capabilities are active.
- Each capability's state and lifecycle is encapsulated in one submodule. Adding a new sink (Gemini, image-gen) becomes "add `src/capabilities/gemini.rs`, register it in chassis builders that want it." No more reaching into the substrate to wire dispatch.
- TestBench becomes per-test composable. Tests that don't need render skip wgpu entirely; tests that exercise io+log only get exactly that.
- `aether-substrate-core` becomes purely runtime mechanism. No hub knowledge, no chassis-policy code. The substrate can be reasoned about as scheduler + registry + wasmtime + control plane + fallback-router slot — and nothing else.
- The `Capability` trait is reachable from external crates, so hub-flavored capabilities (`HubClientCapability`, `HubServerCapability` in the new `aether-hub` crate), binary-specific capabilities, and future third-party capabilities don't pollute the substrate.
- Forcing function for the sink kit becomes natural: once 5+ submodules implement the trait, the shared shape is concrete and extraction is mechanical.
- Symmetry with wasm components clarifies the architecture story. A chassis is composed of native capabilities (compiled in) plus dynamic components (wasm-loaded); both communicate by mail; both have the same lifecycle vocabulary.

**Negative**

- One-time refactor across `aether-substrate-core`, `aether-substrate-hub`, and (new) `aether-hub`. Every existing sink dispatcher moves; chassis boot paths rewrite; bubble-up moves out of the substrate; `aether-substrate-test-bench` rewrites. Diffs land per phase but the cumulative motion is large.
- ADR-0035 (substrate-chassis split) needs an addendum: the chassis trait still holds, gains capability composition.
- Capabilities live in submodules of one crate, so editing `io.rs` recompiles all of `aether-substrate-core`. Acceptable cost; per-crate compile isolation is deferred to a future ADR if friction emerges.
- The trait introduces a generic associated type (`type Running: RunningCapability`) which compounds rust-analyzer / docs noise slightly. Manageable.
- One new workspace crate (`aether-hub`). Justified by the substrate purification it enables; the cost is one Cargo.toml.

**Neutral**

- Wire format unchanged. Mail dispatch, kind ids, custom sections — every byte boundary holds.
- ADR-0035 still describes the runtime / chassis split correctly; this ADR is a refinement, not a replacement.
- ADR-0066 (per-component trunk rlibs) is unaffected; native capabilities follow the same pattern *if* the kinds split happens (separate ADR).
- ADR-0067 (TestBench + scenario runner) gains capability composition; the scenario runner itself is unchanged.

**Follow-on work**

- Phase-1 PR: trait + builder + ctx scaffolding + fallback-router slot.
- One PR per core capability extraction (5: handle/log first, then io/net/audio/render+camera).
- One PR creating `aether-hub` lib crate + extracting `HubClientCapability`.
- One PR implementing `HubServerCapability` + refactoring the hub binary.
- TestBench rewrite PR.
- Sink kit ADR (separate, deferred).
- Issue: `Capability::depends_on()` trait method for explicit boot-order deps (deferred — only land when soft "log first" preference becomes a hard dep).
- Issue: `describe_capability` MCP tool symmetric to `describe_component` (deferred — depends on trait being shipped).
- Optional ADR: per-capability trunk rlib split of `aether-kinds`.
- Optional ADR: per-capability standalone crates (only if a forcing function arrives).

## Alternatives considered

- **Per-capability standalone crates** (one crate per capability + trunk rlib peer). Considered as the more structurally pure form of this refactor. Rejected for now — gives compile-time isolation and per-crate dependency gating, but adds 6+ new workspace crates plus 6+ trunk rlibs, and the structural benefit is duplicative with submodules + Cargo features for today's needs. Deferable until a forcing function arrives (third-party native capabilities, build-time pressure).
- **Status quo + hand-rolled sinks indefinitely.** Rejected — three concurrent pressures (Gemini imminent, sink count climbing, test composition rigid) make the absence of an abstraction a real cost, not a theoretical one.
- **Sink kit first, capability abstraction later.** Considered as the smaller-surgery path. Rejected as the leading move — kit standardizes the *inside* of a sink (dispatch loop), but doesn't give chassis-level composition. Capability is the outer shape; kit is the inner shape; outer should land first because it gives the most visible win and creates the right home (`capabilities/common.rs`) for the kit when it lands.
- **Inline trait without a builder; `Chassis::new(vec![Box<dyn Capability>])`.** Considered for simplicity. Rejected — `vec![Box<dyn>]` loses the typed-state composition (each capability has a different `Self` config struct); a `with(impl Capability)` builder preserves typing through `T::Running` associated types and lets boot return concrete errors.
- **One capability = one mailbox.** Rejected for the render+camera case — they share wgpu state tightly; splitting them forces shared-state-via-Arc plumbing for no real win. The trait supports N mailboxes per capability; render owns two, every other capability today owns one.
- **Hub client in the substrate** (the original draft). Rejected after pushback during ADR drafting — hub client is a chassis-policy concern, not a substrate one. Putting it in the substrate made `aether-substrate-core` depend on `aether-hub-protocol` for a feature that not every chassis uses. Replaced by a generic fallback-router slot in the substrate + `HubClientCapability` in the new `aether-hub` crate.
- **Hub capabilities as `[lib]`+`[[bin]]` inside `aether-substrate-hub`.** Considered as the lower-crate-count form of hosting `HubClientCapability` and `HubServerCapability`. Rejected — the workspace convention (per ADR-0035) is binaries are binaries, libs are libs. Splitting into `aether-hub` (lib) + `aether-substrate-hub` (binary) keeps the convention and gives the lib a clear name; the cost of one extra crate is small.

## References

- ADR-0035 — substrate-chassis split; this ADR is a refinement.
- ADR-0038 — actor-per-component dispatch; capabilities inherit the one-thread-per-mailbox model.
- ADR-0050 — early LLM sink shape (parked); the sink kit work that follows this ADR clarifies the right shape for ADR-0050's eventual replacement.
- ADR-0063 — substrate fail-fast; capability boot errors abort the chassis.
- ADR-0066 — per-component trunk rlibs; native capabilities follow the same pattern if/when kinds split.
- ADR-0067 — TestBench + scenario runner; TestBench rewrites as a thin builder wrapper.
- ADR-0069 — data layer split; orthogonal predecessor that cleaned up the *data* crates and made it sensible to look at the *runtime* crate next.
