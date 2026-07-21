//! Desktop chassis: `DesktopChassis` (ADR-0035 / ADR-0071), the
//! `UserEvent` enum the winit event loop consumes, and the
//! [`DesktopChassis::build`] entry point that assembles the substrate
//! + driver into a [`BuiltChassis`] for `main()` to drive.
//!
//! Issue 603 retired `chassis_handler` entirely: capture goes through
//! `RenderCapability` (Phase 2), window kinds through driver-as-actor
//! on `aether.window` (Phase 3), and `platform_info` was deleted as a
//! kind (Phase 4) along with the closure-fallback that served it.
//! Two proxy events wake the loop under `ControlFlow::Wait`:
//! `UserEvent::Capture` so a queued `CaptureQueue` request gets pulled
//! on the next redraw, and `UserEvent::WindowMail` so `about_to_wait`
//! drains the `aether.window` inbox when window-control mail arrives at
//! an occluded window (iamacoffeepot/aether#1318).

use std::io;
use std::mem;
use std::sync::Arc;
use std::time::Duration;

use aether_audio::AudioCapability;
use aether_clipboard::{ClipboardCapability, ClipboardParams};
use aether_component::ComponentHostParams;
use aether_contentgen::ContentGenConfig;
use aether_fs::NamespaceRoots;
use aether_harness_substrate::UnsupportedSubstrateHarnessCapability;
use aether_http::HttpServerCapability;
use aether_kinds::BinaryManifest;
use aether_lifecycle::{LifecycleCapability, frame_lifecycle_params};
use aether_render::{RenderCapability, RenderParams};
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate::{Chassis, SubstrateBoot, capture::CaptureQueue};
use winit::event_loop::EventLoop;

use aether_chassis::{WindowConfig, WindowSettings};

use super::driver::DesktopDriverCapability;
use aether_chassis::autoload::{AutoloadComponent, autoload_mail, boot_manifest_autoload};
use aether_chassis::boot::{
    ActorRingConfig, ChassisBootConfig, CommonBoot, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
    chassis_residual_knobs, install_frame_size, load_chassis_config, with_common_caps, with_rpc_server,
};
use aether_chassis::cli::DesktopCli;
use aether_substrate::config::{
    ConfigError, ConfigManifest, ConfigSources, RingCapacities, SchedulerTuning, StageArgv, validate_env,
};
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::runtime::lifecycle::OutboundFatalAborter;
use std::path::Path;
use winit::event_loop::ControlFlow;

/// Event the event-loop thread consumes from the desktop chassis.
/// Just one variant today: a wake-up so the loop picks up a queued
/// capture on the next redraw, even under `ControlFlow::Wait` when
/// the window is occluded.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// A capture was just enqueued on `CaptureQueue`; wake the loop
    /// so `RedrawRequested` pulls and fulfils it.
    Capture,
    /// Window-control mail was enqueued on `aether.window`; wake the
    /// loop so `about_to_wait` drains the inbox even under
    /// `ControlFlow::Wait` (iamacoffeepot/aether#1318). Without this an
    /// `aether.window.focus` / `set_mode` / `set_title` mail sent to an
    /// occluded window sits undrained until an unrelated winit event
    /// nudges the loop.
    WindowMail,
    /// A SIGINT/SIGTERM was observed by the signal-watcher thread
    /// (iamacoffeepot/aether#1489). Carries no work itself — it only
    /// wakes the loop so `about_to_wait` observes the shutdown flag and
    /// runs the `Quit`-push path, mirroring `WindowMail`. Needed because
    /// an async-signal-safe handler can't poke winit, and a parked
    /// (`ControlFlow::Wait`, occluded) loop otherwise never runs
    /// `about_to_wait` to see the flag.
    Quit,
}

/// Marker type for the desktop chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<DesktopChassis>`] returned
/// by [`Self::build`]. The unit struct exists so the `chassis_builder`
/// machinery can parameterise over a concrete chassis kind for type
/// disambiguation, and so [`Chassis::PROFILE`] has a home.
pub struct DesktopChassis;

impl Chassis for DesktopChassis {
    const PROFILE: &'static str = "desktop";
    type Driver = DesktopDriverCapability;
    type Env = DesktopEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
    }
}

impl DesktopChassis {
    /// The `--describe` manifest (ADR-0115, amended by ADR-0155): the
    /// chassis profile, the mailbox namespaces this binary claims, and the
    /// `build.rs` provenance. Resolves the chassis config the same
    /// argv/env/file way a real boot does (config only — the winit event
    /// loop and capture queue are Start-stage handles, ADR-0155 §4), composes
    /// the exact capability chain `build_inner` runs (via `compose`), then
    /// runs the ADR-0155 claim-only terminal and
    /// reads the claimed namespaces off the registry — the driver's
    /// `aether.window` claim rides the `DriverCapability::claim` hook, so it
    /// appears without an event loop. `--describe` therefore captures a
    /// desktop binary's manifest on a headless host, opening no window and
    /// binding no socket.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when config resolution ([`DesktopEnv::from_env`]),
    /// substrate boot, or the claim pass fails.
    pub fn describe_manifest() -> Result<BinaryManifest, BootError> {
        let env = DesktopEnv::from_env().map_err(|e| BootError::Other(Box::new(e)))?;
        let boot = SubstrateBoot::build()?;
        let caps = Self::compose(&boot, env).claim_namespaces()?;
        Ok(aether_chassis::binary_manifest(Self::PROFILE, caps))
    }

    /// The ADR-0156 §4 composition-derived config aggregate: resolve the
    /// chassis config, compose the exact capability chain `build_inner` runs
    /// (config only — the winit event loop / capture queue are Start-stage
    /// handles), then read [`Builder::config_manifest`] — the sibling of
    /// `describe_manifest`'s claim terminal. Desktop reports the window knobs
    /// (its driver's members) but not the headless tick knob.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when config resolution or substrate boot fails.
    pub fn config_manifest() -> Result<ConfigManifest, BootError> {
        let env = DesktopEnv::from_env().map_err(|e| BootError::Other(Box::new(e)))?;
        let boot = SubstrateBoot::build()?;
        Ok(Self::compose(&boot, env).config_manifest())
    }

    /// Render the `--print-config` discovery dump from the composition-derived
    /// manifest plus the residual hand-registered knobs.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when config resolution or substrate boot fails.
    pub fn config_dump() -> Result<String, BootError> {
        Ok(Self::config_manifest()?.dump(&chassis_residual_knobs()))
    }
}

/// Bag of resolved config *data* the desktop chassis takes at build time.
/// `main()` populates it from env vars (per ADR-0070's "substrate-core
/// never reads env" invariant); tests construct one directly.
///
/// ADR-0155 §4: this is config only — every field resolves through the
/// argv/env/file path a real boot uses, so `--describe` can resolve it on
/// a headless host. The Start-stage runtime handles that used to ride here
/// — the winit `EventLoop` and the capture-handoff `CaptureQueue` — are not
/// config: they are constructed on the boot path in `DesktopChassis::build_inner`
/// (winit's `EventLoop` is `!Send` on macOS and is the chassis's main
/// thread, so it stays local to the boot call `main()` makes).
pub struct DesktopEnv {
    /// ADR-0156 §5: the config source stack (file + per-cap argv overlays) the
    /// builder resolves each composed cap's `Config` off (http / http-server /
    /// audio / render tuning / lifecycle). The cap `Config`s no longer ride as
    /// fields — a test stages programmatic overrides here.
    pub sources: ConfigSources,
    pub namespace_roots: NamespaceRoots,
    /// Content-gen staging config (ADR-0090). Resolved chassis-side; folded
    /// into the staging root in `with_common_caps`.
    pub generated_asset_staging: ContentGenConfig,
    /// Lowered desktop window boot knobs (mode / size / title / wireframe),
    /// grouped into one embedded unit like the other knob groups and
    /// threaded to the desktop driver. Produced by [`WindowConfig::lower`];
    /// `wireframe` reaches `Gpu::new` via `WireframeMode::from_config_value`,
    /// which owns the tri-state parse.
    pub window: WindowSettings,
    /// The substrate runtime knobs (#3849), resolved off the source stack. Only
    /// [`RuntimeConfig::log_filter`] is consumed chassis-side (re-applied after
    /// the subscriber installs, in `DesktopChassis::build_inner`). The
    /// `aether.rpc.server` bind port rides the source stack too now
    /// (`RpcServerConfig`) — the builder resolves it (unset → claimed but
    /// unbound, ADR-0155 §3), so there is no separate `rpc_address` field.
    pub runtime: RuntimeConfig,
    /// Issue 745: optional worker-pool size override. Populated from
    /// `AETHER_WORKERS` / `--workers`; `None` keeps `PoolConfig::default()`
    /// behavior (`available_parallelism() - 1`, min 1).
    pub workers: Option<usize>,
    /// Issue 1990: per-actor ring capacities resolved from the
    /// `ActorRingConfig` knob (`AETHER_ACTOR_LOG_RING_SIZE` /
    /// `AETHER_ACTOR_TRACE_RING_SIZE`). Default is
    /// [`RingCapacities::default`] (the `aether-actor` const caps).
    pub ring_capacities: RingCapacities,
    /// Issue 2485: scheduler hot-path tuning resolved from the
    /// `SchedulerTuningConfig` knob (`AETHER_SPIN_WINDOW_USEC` /
    /// `AETHER_LOCAL_STICKY_MAX` / …). Default is
    /// [`SchedulerTuning::default`] (the built-in scheduler literals).
    pub scheduler_tuning: SchedulerTuning,
    /// Issue #2509: cumulative patience for the instanced-actor teardown
    /// close-done gate, resolved from `AETHER_SETTLEMENT_CAP_SECS` /
    /// `[settlement]`.
    pub teardown_budget: Duration,
    /// Components to auto-load on boot, in order. A bundled standalone build
    /// populates this so the game comes up with no hub; the normal desktop bin
    /// leaves it empty and loads components over the hub instead.
    pub autoload: Vec<AutoloadComponent>,
}

impl DesktopEnv {
    /// Resolve every chassis-relevant env var into a fresh `DesktopEnv` of
    /// config *data*. The single env-reading edge for the desktop chassis
    /// (per issue 464). Tests bypass this by constructing `DesktopEnv`
    /// directly.
    ///
    /// ADR-0155 §4: env resolution produces config only — the winit
    /// `EventLoop` and the capture `CaptureQueue` are Start-stage runtime
    /// handles constructed on the boot path in
    /// `DesktopChassis::build_inner`, not here — so the only fallible step
    /// is the ADR-0090 §4 config validation / parse path.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known `AETHER_*` env var (or argv
    /// overlay value) holds an unparseable value.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_argv(DesktopCli::default())
    }

    /// ADR-0090 unit d (issue 1258): resolve every cap config through
    /// the argv-then-env overlay. `cli` carries `Option<T>` flags;
    /// unset fields fall through to env-only resolution, so an empty
    /// argv (the path the existing `from_env` callers exercise) is
    /// byte-identical to the pre-d behaviour.
    ///
    /// # Errors
    ///
    /// See [`Self::from_env`].
    pub fn from_env_with_argv(cli: DesktopCli) -> Result<Self, ConfigError> {
        // ADR-0156 §4: the unknown-`AETHER_*` sweep moved to `build_inner`,
        // where the composed builder's `config_manifest` supplies the
        // per-chassis known-key set (desktop no longer "knows" the headless
        // tick knob).
        // The bin handles `--print-config` / `--describe` (print + exit) before
        // this resolver runs; `config` names the file source and takes no part
        // in staging.
        let config_file = load_chassis_config(cli.config.clone())?;

        // ADR-0156 §5 (issue 3872): assemble the source stack — the loaded
        // config file plus every cap member's typed argv overlay, staged in one
        // derived `StageArgv` call off the CLI declaration itself (each `*Overlay`
        // carries a leaf `StageArgv` and each root delegates to its fields). No
        // hand-maintained per-cap `set_argv` block to forget, and a
        // staged-but-never-composed overlay fails boot loudly. Section identity
        // comes from each member's `ConfigMember` declaration, so no chassis-side
        // section string survives.
        let mut sources = ConfigSources::new(config_file);
        cli.stage(&mut sources);

        // Chassis-side reads of resolved members (ADR-0156 §5): the derived
        // staging inputs (fs roots + content-gen), the driver-only window boot
        // knobs, and the non-cap pool / ring / scheduler / teardown knobs. Each
        // resolves off the same stack via its `ConfigMember` section.
        let chassis_boot = sources.resolve::<ChassisBootConfig>()?;
        let namespace_roots = sources.resolve::<NamespaceRoots>()?;
        let generated_asset_staging = sources.resolve::<ContentGenConfig>()?;
        let window_config = sources.resolve::<WindowConfig>()?;
        let ring_capacities = sources.resolve::<ActorRingConfig>()?.to_ring_capacities();
        let scheduler_tuning = sources.resolve::<SchedulerTuningConfig>()?.to_scheduler_tuning();
        let teardown_budget = sources.resolve::<SettlementConfig>()?.to_cap();
        // #3849: resolve the substrate runtime knobs (log filter + panic-hook
        // knobs) off the same stack; `build_inner` re-applies `log_filter`.
        let runtime = sources.resolve::<RuntimeConfig>()?;
        // ADR-0156 §6 (#3850): push the resolved wire-frame cap into the codec
        // here, before the RPC server binds and any framing runs — the codec
        // cannot pull the knob itself.
        install_frame_size(&mut sources)?;

        // Boot manifest: argv wins over `AETHER_BOOT_MANIFEST` (resolved
        // through `ChassisBootConfig`). When set, the listed components'
        // wasm + config are read into the autoload list `build_inner`
        // drains into `aether.component.load`; an unreadable manifest
        // aborts boot (ADR-0090 §4) via `ConfigError`.
        let autoload = match chassis_boot.boot_manifest.clone() {
            Some(path) => boot_manifest_autoload(Path::new(&path))?,
            None => Vec::new(),
        };

        // Window boot knobs: resolved through `WindowConfig` (argv > env >
        // default) and lowered as a unit. `lower` delegates the mode to
        // `parse_window_mode_env` — a present-but-bad `AETHER_WINDOW_MODE`
        // aborts boot via `ConfigError` (ADR-0090 §4), an absent value
        // resolves to `Windowed` — and maps `None` / empty title to `"aether"`.
        let window = window_config.lower()?;

        let workers = chassis_boot.to_workers();

        Ok(Self {
            sources,
            namespace_roots,
            generated_asset_staging,
            window,
            runtime,
            workers,
            ring_capacities,
            scheduler_tuning,
            teardown_budget,
            autoload,
        })
    }
}

impl DesktopChassis {
    /// Compose the desktop capability chain — the single claim/build path
    /// (ADR-0155) both [`Self::build_inner`] and [`Self::describe_manifest`]
    /// run, so the manifest roster can never drift from what boots. Composes
    /// the common caps plus the audio / clipboard / render / substrate-harness
    /// / lifecycle caps and the always-claim RPC + HTTP servers (ADR-0155 §3).
    /// Returns the composed builder before the driver is installed:
    /// `build_inner` adds the desktop driver and starts (the driver's
    /// `aether.window` claim rides its Claim-stage hook either way), while
    /// `describe_manifest` calls `claim_namespaces` on it.
    ///
    /// Takes the boot handle by reference — `build_inner` moves the same
    /// `boot` into the driver afterward. The window boot knobs (mode / size /
    /// title / wireframe) and the `autoload` list ride [`DesktopEnv`] but take
    /// no part in the claim chain, so they are ignored here.
    fn compose(boot: &SubstrateBoot, env: DesktopEnv) -> Builder<Self> {
        let DesktopEnv {
            sources,
            namespace_roots,
            generated_asset_staging,
            workers,
            ring_capacities,
            scheduler_tuning,
            teardown_budget,
            window: _,
            runtime: _,
            autoload: _,
        } = env;

        let component_host_params = ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        // ADR-0155 §4: the capture backend is a Start-stage handoff, not a
        // config field. The driver builds it from the `CaptureQueue` +
        // `EventLoopProxy` wake + reply egress and installs it into the
        // published `RenderHandles` in its `boot` (Start), so the cap's
        // `on_capture_frame` reads it there. Issue 2706: the resolved
        // vertex-buffer cap (`RenderTuningConfig`) sizes both the cap
        // accumulator's truncation and (via `RenderHandles`) the GPU vertex
        // buffer the driver creates; the assets-root wiring rides `RenderParams`.
        let render_params = RenderParams {
            // The `capture_frame` similarity check (iamacoffeepot/aether#1780)
            // reads its reference image from the same `assets` root the fs
            // cap serves, so the render cap loads it off the hot path.
            assets_dir: Some(namespace_roots.assets.clone()),
            ..RenderParams::default()
        };

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // ADR-0063: production chassis configures the fatal-abort
        // aborter so a wasm guest trap exits the substrate via
        // `lifecycle::fatal_abort` instead of unwinding.
        let aborter: Arc<dyn FatalAborter> = Arc::new(OutboundFatalAborter::new(Arc::clone(&boot.outbound)));

        // Boot order is declaration order — `with_common_caps` runs
        // log first so other capabilities' boot tracing routes
        // through the log capture; render last so it claims its
        // mailboxes after every other chassis cap.
        let common = CommonBoot {
            aborter,
            workers,
            ring_capacities,
            scheduler_tuning,
            // Issue #2509: the instanced-actor teardown gate honors the
            // same `AETHER_SETTLEMENT_CAP_SECS` knob (including its
            // `0 → wait forever` sentinel) as the settlement gates.
            teardown_budget,
            component_host_params,
            namespace_roots,
            generated_asset_staging,
            game_gateway_params: aether_game::GameGatewayParams::default(),
        };
        // ADR-0082 §11 / issues 1378 + 1489: desktop drives the shared
        // `Tick → Render → Present → Tick` frame graph, with the `Quit`
        // escape to `Shutdown` on `Present` so OS-close / ctrlc drain the
        // in-flight frame before shutting down (see the driver's
        // `CloseRequested` → `Quit` bridge and terminal-reached exit).
        //
        // ADR-0156 §5: hand the builder the source stack (`with_config_sources`)
        // so it resolves each composed cap's `Config` (audio / render tuning /
        // lifecycle / http-server) off it; the caps compose with
        // `with_actor(params)` alone. `RenderParams` (the assets root) is
        // composer-supplied construction input derived from the resolved fs
        // roots, so it still rides `with_actor`.
        let builder = with_common_caps(Builder::<Self>::new(registry, mailer).with_config_sources(sources), common)
            .with_actor::<AudioCapability>(())
            .with_actor::<ClipboardCapability>(ClipboardParams::System)
            .with_actor::<RenderCapability>(render_params)
            .with_actor::<UnsupportedSubstrateHarnessCapability>(())
            .with_actor::<LifecycleCapability>(frame_lifecycle_params());
        with_rpc_server(builder, "aether-desktop").with_actor::<HttpServerCapability>(())
    }

    /// Build the desktop chassis: construct the Start-stage runtime handles
    /// (winit event loop + capture queue), stand up substrate-core internals,
    /// compose the capability chain via [`Self::compose`], then wrap
    /// everything in a [`DesktopDriverCapability`] and hand it to the builder.
    /// Returns a [`BuiltChassis`] whose [`BuiltChassis::run`] blocks on the
    /// winit event loop.
    ///
    /// The trait method [`Chassis::build`] forwards here.
    fn build_inner(mut env: DesktopEnv) -> Result<BuiltChassis<Self>, BootError> {
        // ADR-0155 §4: the winit `EventLoop` and the capture `CaptureQueue`
        // are Start-stage runtime handles, not config — construct them here
        // on the boot path (`main()` calls this on the chassis main thread,
        // where winit's `!Send` `EventLoop` must live). `--describe` never
        // reaches this method, so it opens no event loop. The `EventLoop`
        // build fault (never `Send + Sync` across winit's platform impls) is
        // stringified into `BootError::Other`, the same shape a wasmtime boot
        // fault takes.
        let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| {
            BootError::Other(Box::new(io::Error::other(format!("desktop event loop build failed: {e}"))))
        })?;
        event_loop.set_control_flow(ControlFlow::Poll);
        let capture_queue = CaptureQueue::new();

        let boot = SubstrateBoot::build()?;
        // #3849: `SubstrateBoot::build` installed the subscriber with an
        // env-or-`info` filter (before the config file loaded); re-apply the
        // fully-resolved `AETHER_LOG_FILTER` directive (env > `[runtime]` file >
        // `info`) now so a filter set only in the config file takes effect.
        apply_filter(&env.runtime.log_filter);
        let mailer = Arc::clone(&boot.queue);

        // Driver-only / post-build fields, read out before `compose` consumes
        // `env`: the window boot knobs ride the desktop driver, the autoload
        // list is drained after build. `WindowSettings` is `Clone` (not `Copy`);
        // it is tiny, so the clone is free.
        let workers = env.workers;
        let window = env.window.clone();
        let autoload = mem::take(&mut env.autoload);

        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            "componentless boot — close window to exit; load a component via aether.component.load",
        );

        let builder = Self::compose(&boot, env);
        // ADR-0156 §4 (was ADR-0090 §4 e1): warn on any unknown `AETHER_*` env
        // var, sweeping against the composition-derived known-key set plus the
        // residual hand records. Runs here (not in `from_env`) because the
        // per-chassis known keys come from the composed builder's manifest.
        validate_env(&builder.config_manifest().known_keys(&chassis_residual_knobs()))?;
        // Issue 552 stage 2d: render is a NativeActor. The chassis builder
        // constructs the cap inside `init` (called from
        // `with_actor::<RenderCapability>(config)`); `init` publishes the
        // `RenderHandles` bundle on the exported-handle map, and the driver
        // fetches it via `DriverCtx::handle::<RenderHandles>()`. `boot` moves
        // into the driver here, after `compose` finished borrowing it.
        let driver = DesktopDriverCapability { event_loop, boot, capture_queue, window };
        let built = builder.driver(driver).build()?;
        // Auto-load any bundled components, in order, before the run loop
        // starts. Fire-and-forward: the component host dispatches each load off
        // the worker pool (already up after `build`), so the game is live
        // shortly after `run` begins — no hub required.
        for component in autoload {
            mailer.push(autoload_mail(component));
        }
        Ok(built)
    }
}

#[cfg(test)]
mod config_manifest_tests {
    use super::DesktopChassis;
    use aether_chassis::boot::chassis_residual_knobs;

    #[test]
    fn desktop_known_keys_drop_tick_and_keep_window_audio_render() {
        // ADR-0156 §4 acceptance: desktop drives from winit, not a std timer,
        // so its aggregate no longer includes the headless tick knob (the old
        // flat registry over-claimed it). Membership is asserted from the
        // composition walk, not a hand list.
        let manifest = DesktopChassis::config_manifest().expect("desktop config manifest");
        let known = manifest.known_keys(&chassis_residual_knobs());
        assert!(!known.contains("AETHER_TICK_HZ"), "desktop drives from winit — must not claim the headless tick knob");
        assert!(known.contains("AETHER_WINDOW_MODE"), "desktop must claim its window-driver knob");
        assert!(known.contains("AETHER_AUDIO_DISABLE"), "desktop must claim the composed audio cap knob");
        assert!(known.contains("AETHER_RENDER_VERTEX_BUFFER_BYTES"), "desktop must claim the composed render cap knob");
    }
}
