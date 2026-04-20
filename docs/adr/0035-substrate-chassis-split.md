# ADR-0035: Substrate-chassis split

- **Status:** Proposed
- **Date:** 2026-04-20

## Context

`aether-substrate` is one crate with two responsibilities that have never been separated:

- **Runtime core**: wasmtime hosting, mail scheduler, component table, kind manifest, registry, control-plane dispatch, hub-socket client, sender table, input subscriber bookkeeping, log capture.
- **Desktop peripherals**: winit window + event loop, wgpu device + swapchain, frame renderer, offscreen capture, monitor enumeration, keyboard/mouse input translation.

Current split of the ~7500 lines in `aether-substrate/src/`: roughly **23%** (~1720 lines across `main.rs`, `render.rs`, `capture.rs`, `platform_info.rs`) is desktop-peripheral-specific; the rest is runtime core. Control plane (`control.rs`, ~2000 lines) handles the vast majority core-side but has three chassis-dependent handlers (`handle_capture_frame`, `handle_set_window_mode`, `platform_info` getters) that reach into winit/wgpu directly.

This split has been implicit and has held up fine while `aether-substrate` has exactly one chassis. Two forces push against that:

1. **ADR-0034 (hub-as-substrate).** Commits to the direction that a hub is a specialized substrate deployment. That requires at least a third chassis target (hub-chassis: TCP listener + MCP surface, no window, no GPU) and a trait surface broad enough to carry it. Phase 0 of ADR-0034's rollout is this split.
2. **Headless server demo.** An authoritative-server game world wants a dedicated server substrate without a visible window, ideally without wgpu init at all. Today the only way is to run the desktop substrate and ignore the window, which bolts GPU init and winit loops onto a workload that doesn't need them.

The chassis trait can't be designed in a vacuum — with only one target (desktop) the trait either gets over-specified to what winit/wgpu happen to expose, or under-specified to something abstract that the second target then wants to extend. The headless chassis is the minimal concrete second target that makes the trait surface honest; the hub-chassis is the known-future third target that informs anything the trait has to anticipate.

## Decision

Factor `aether-substrate` into a runtime core crate, a `Chassis` trait that abstracts peripheral I/O, and per-chassis binary crates.

### Crate layout

- **`aether-substrate-core`** (new crate): everything runtime. The current `scheduler.rs`, `queue.rs`, `mail.rs`, `component.rs`, `host_fns.rs`, `ctx.rs`, `sender_table.rs`, `kind_manifest.rs`, `registry.rs`, `log_capture.rs`, `hub_client.rs`, `input.rs` migrate here. Defines the `Chassis` trait and the `SubstrateCore` handle the chassis holds.
- **`aether-substrate-desktop`** (binary crate): winit event loop, wgpu device + renderer, frame capture, monitor enumeration, input translation. Implements `Chassis` for desktop and runs `main.rs`'s current startup + event loop. Inherits the `aether-substrate` binary name as the desktop chassis is the current default.
- **`aether-substrate-headless`** (binary crate): no winit, no wgpu. Tick driver (std timer), console logging, no-op implementations of peripheral operations that don't apply. Proves the trait surface works for a non-GPU target.
- **`aether-substrate-hub`** (binary crate, planned for ADR-0034 Phase 1 — **not implemented in this ADR**): the trait surface must accommodate its expected shape (accept-loop over TCP, child-process supervisor, session tracking, no GPU). Flagged here so the `Chassis` trait design doesn't regret anticipating it.

Old `aether-substrate` crate name is retired in favor of the explicit chassis binaries.

### `Chassis` trait sketch

The trait owns peripheral I/O + the main event loop; the core owns the scheduler + WASM hosting + control-plane dispatch. The chassis calls into the core to drive ticks and dispatch mail; the core calls into the chassis for operations that depend on peripherals.

Direction (methods will settle during implementation, this is the shape):

- **Lifecycle.** The chassis owns `main()`. It constructs a `SubstrateCore`, hands itself to the core as the peripheral handle, runs its event loop, and tears down on shutdown.
- **Peripheral operations.** Methods the control plane reaches for when a chassis-dependent kind arrives: `capture_frame(params) -> Result<Png, ChassisError>`, `set_window_mode(mode) -> Result<AppliedMode, ChassisError>`, `platform_info() -> PlatformInfoResult`. The core matches on the kind and routes to the trait method; a no-op / unsupported response is a legal return for a chassis that doesn't apply (headless returns `ChassisError::Unsupported` for `capture_frame`; hub-chassis returns the same for `set_window_mode`).
- **Input source.** Chassis-specific events (keyboard, mouse, tick from a timer vs. vsync) get translated by the chassis and published into the core's input dispatch path via a method on `SubstrateCore` (`publish_input(kind_id, payload)`). The core's existing subscriber machinery (ADR-0021, unchanged) fans out to bound mailboxes.

### Control plane migration

Chassis-dependent handlers in `control.rs`:

- `handle_capture_frame` — calls `chassis.capture_frame(...)` and emits the result mail.
- `handle_set_window_mode` — calls `chassis.set_window_mode(...)`.
- `platform_info` reads — calls `chassis.platform_info()`.

Core handlers (`handle_load`, `handle_drop`, `handle_subscribe`, `handle_unsubscribe`, `handle_replace`) stay unchanged. They never touched peripherals.

### Boot-time chassis config

Per-chassis CLI / env flags currently handled in `main.rs` (`AETHER_WINDOW_MODE`, `AETHER_LOG_FILTER`, `AETHER_HUB_URL`) belong to their respective chassis binaries. `AETHER_HUB_URL` is core (shared by every chassis that wants to dial a hub). `AETHER_WINDOW_MODE` is desktop-only. `AETHER_LOG_FILTER` is core (log capture is core-side, ADR-0023).

## Consequences

### Positive

- **Trait surface designed against three targets.** Desktop ships as today; headless is the minimal concrete second; hub-chassis is the known third that keeps anyone from locking the trait to desktop concepts. No speculative abstraction — two concrete implementations land in this ADR's rollout.
- **Cleaner dependency graph.** The core crate loses its winit/wgpu dep tree; downstream consumers (hub, tooling, tests) that care about the runtime but not peripherals can depend on core directly.
- **Headless server workloads unlock immediately.** Game server for the tic-tac-toe demo, dedicated world servers for future MMOs, CI test harness without a GPU — all run on `substrate-headless` without the winit/wgpu bolt-on.
- **Foundation for ADR-0034.** Hub-chassis in Phase 1 of ADR-0034 fits into the slot this ADR carves out; it isn't a redesign.
- **Test surface improves.** Core-only tests don't need a GPU or window. Headless is the natural test chassis.

### Negative

- **Three binaries to build and ship** where there was one. Desktop is the default (inherits the binary name), but CI and release artifacts triple.
- **Trait shape has to be right.** Underpowered and chassis needs grow it with every new peripheral operation; overpowered and every chassis carries no-op methods for things most of them don't do. The hub-chassis target is the biggest open question — its operations (TCP accept, child process spawn, session tracking) don't overlap with desktop's (GPU, window, frame capture) at all.
- **Refactor scope.** ~1720 lines move between crates; `main.rs` splits into chassis-specific entry points; `control.rs` gains trait calls at the three chassis-dependent sites. Roughly 2–3 days of focused work. Low risk — code is moving, not fundamentally changing — but tedious.
- **`main.rs` event loop becomes chassis-specific.** Today a single event loop drives both winit events and the substrate tick. Splitting means each chassis writes its own drive loop. Pattern is the same (pump events → tick core → repeat) but the code diverges.

### Neutral

- **Mail kind vocabulary unchanged.** `aether.control.capture_frame`, `aether.control.set_window_mode`, etc., keep their current names and wire shapes. A headless chassis receiving `set_window_mode` replies with an unsupported error, same as any other unhandled control kind on a real chassis.
- **Hub client stays core.** Every chassis wants to dial a hub (desktop for test harness, headless for game servers, hub-chassis for federation). The socket code moves with the core.
- **Input subscriber machinery stays core** (ADR-0021). Chassis publishes events into the core; core fans out to subscribers. Nothing changes downstream of `publish_input`.

## Alternatives considered

- **Keep substrate as one crate; add `#[cfg]` flags for headless/hub modes.** Simpler to write, uglier to maintain — every chassis-sensitive file grows conditional branches and the dependency graph gets worse, not better (both winit and non-winit code live in the same crate, gated). Rejected because it doesn't buy the clean dependency isolation and doesn't inform a trait.
- **Only do desktop + headless; defer hub-chassis until ADR-0034 Phase 1 and redesign the trait then.** Simpler now, more rework later when hub-chassis surfaces a need the trait doesn't accommodate. Rejected because the marginal cost of keeping hub-chassis's shape in mind while designing is low.
- **Design the trait to be maximally generic (e.g., every chassis operation is a mail-kind handler the chassis registers).** Elegant but abstract — would delay the split with no concrete payoff. Better to land a straightforward trait with explicit methods and evolve it when a fourth or fifth chassis target surfaces.
- **Split, but name it something other than "chassis".** Considered shell, rig, carapace, frame, host, form, vessel, peripheral. Chassis is specific ("outer structural body that houses peripherals"), pairs with substrate cleanly (substrate = biological base, chassis = structural body), and avoids the overloads of shell/host in the tech vocabulary. Tracked in memory; decision stands.

## Follow-up work

### Phased rollout

**Phase 1 — Core carve-out.** New `aether-substrate-core` crate. Move the runtime files unchanged. No chassis yet — a temporary `DesktopChassis` struct in the existing `aether-substrate` binary crate holds the current peripheral code and implements the trait. Everything still ships as one binary. Goal: prove the core compiles standalone and the trait surface carries the current control-plane behavior. ~1 day.

**Phase 2 — Desktop chassis crate.** Split `aether-substrate` into `aether-substrate-desktop` (binary, holds current `main.rs` + render + capture + platform_info + input translation) and retires the old crate name. CI picks up the new artifact. ~0.5 days.

**Phase 3 — Headless chassis.** New `aether-substrate-headless` binary. Trait impl returns `Unsupported` for GPU / window operations. Tick driver is a std timer. Hub client still works. Proves the trait. ~1 day.

**Phase 4 — Follow-up.** Update tests that depended on the substrate being one crate. Update docs (CLAUDE.md, README) to reflect the chassis taxonomy. Address any trait-surface issues surfaced by the headless chassis before they fossilize.

### CI matrix scope

After the split, CI OS-matrix testing should narrow to the chassis crate that actually exercises per-OS behavior:

- **`aether-substrate-desktop`** — full matrix (ubuntu + macos + windows). This is where winit event loops, wgpu driver init, and platform-specific frame-capture paths live; per-OS coverage catches real bugs here.
- **`aether-substrate-core`, `aether-substrate-headless`, `aether-substrate-hub`** — linux only. Pure Rust runtime code with no OS-specific system calls; the matrix would just triple CI time without catching anything the linux run doesn't.
- **Workspace-wide `cargo test --workspace`** — linux only. Non-chassis crates (aether-kinds, aether-mail, aether-hub-protocol, aether-component, aether-mail-derive, demo components) have no per-OS behavior.

The Phase 2 PR (or a follow-up `ci/` PR) flips `.github/workflows/ci.yml`: the existing matrix `test` job becomes linux-only and runs the workspace; a separate `desktop` job matrix-tests `cargo test -p aether-substrate-desktop --all-targets` on macos + windows.

### Deferred

- **Hub-chassis** — owned by ADR-0034 Phase 1, not this ADR.
- **Web chassis** (wasm target, canvas + webgpu) — a plausible fourth target, parked pending actual pressure. If someone wants to host a substrate in the browser, the trait's shape from the three initial targets should bear the weight or get extended.
- **Chassis-specific mail kind namespacing.** Today `aether.control.set_window_mode` is a universally-registered kind even though only desktop handles it. A future improvement lets chassis declare which control kinds they accept, so the hub can route more intelligently. Parked — trait method returning `Unsupported` is sufficient for now.
