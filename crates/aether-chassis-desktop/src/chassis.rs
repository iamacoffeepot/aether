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
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aether_anthropic::AnthropicConfig;
use aether_audio::{AudioCapability, AudioConfig as AudioConf};
use aether_clipboard::{ClipboardCapability, ClipboardConfig};
use aether_component::ComponentHostConfig;
use aether_contentgen::ContentGenConfig;
use aether_fs::NamespaceRoots;
use aether_gemini::GeminiConfig;
use aether_harness_substrate::UnsupportedSubstrateHarnessCapability;
use aether_http::{HttpConfig as HttpConf, HttpServerCapability, HttpServerConfig};
use aether_input::InputConfig;
use aether_kinds::BinaryManifest;
use aether_kinds::WindowMode;
use aether_lifecycle::{LifecycleCapability, frame_lifecycle_config};
use aether_render::{RenderCapability, RenderConfig, RenderTuningConfig};
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot, capture::CaptureQueue};
use winit::event_loop::EventLoop;

use aether_chassis::WindowConfig;

use super::driver::DesktopDriverCapability;
use aether_chassis::autoload::{AutoloadComponent, autoload_mail, boot_manifest_autoload};
use aether_chassis::boot::{
    ActorRingConfig, ChassisBootConfig, CommonBoot, SchedulerTuningConfig, chassis_known_keys, load_chassis_config,
    resolve_env_with_file, resolve_teardown_cap_with_file, resolve_with_file, rpc_port_from_env, with_common_caps,
    with_rpc_server,
};
use aether_chassis::cli::{CommonOverlay, DesktopCli};
use aether_substrate::config::{ConfigError, RingCapacities, SchedulerTuning, validate_env};
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
        let boot = SubstrateBoot::builder("hello-triangle", env!("CARGO_PKG_VERSION")).build()?;
        let caps = Self::compose(&boot, env).claim_namespaces()?;
        Ok(aether_chassis::binary_manifest(Self::PROFILE, caps))
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
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConf,
    /// ADR-0050 `aether.anthropic` cap config (issue 1014). Resolved
    /// from `ANTHROPIC_API_KEY` + `AETHER_ANTHROPIC_*`.
    pub anthropic: AnthropicConfig,
    /// ADR-0050 `aether.gemini` cap config (issue 1015). Resolved from
    /// `GEMINI_API_KEY` + `AETHER_GEMINI_*`.
    pub gemini: GeminiConfig,
    /// Content-gen staging config (ADR-0090). Resolved from
    /// `AETHER_GEN_DIR` / `--gen-dir`; folded into the staging root in
    /// `with_common_caps`.
    pub contentgen: ContentGenConfig,
    pub audio: AudioConf,
    pub boot_mode: WindowMode,
    pub boot_size: Option<(u32, u32)>,
    pub boot_title: String,
    /// Resolved `AETHER_WIREFRAME` config value (`WindowConfig::wireframe`,
    /// argv > env > default), threaded to `Gpu::new` at window creation.
    /// `WireframeMode::from_config_value` owns the tri-state parse.
    pub boot_wireframe: Option<String>,
    /// The resolved `aether.http.server` init config (ADR-0108, ADR-0155 §3).
    /// The cap is always composed and always claims `aether.http.server`;
    /// its `enabled` flag (`AETHER_HTTP_SERVER_ENABLED` /
    /// `--http-server-enabled`, default off) gates only whether Start binds
    /// the socket, so an unconfigured chassis binds no HTTP port yet still
    /// answers mail rather than warn-dropping it.
    pub http_server: HttpServerConfig,
    /// The resolved `aether.rpc.server` bind address (ADR-0155 §3). The cap
    /// is always composed and always claims `aether.rpc.server`; the
    /// address (from `AETHER_RPC_PORT`) gates only whether Start binds the
    /// socket — `None` (default) leaves the mailbox claimed but unbound.
    pub rpc_addr: Option<SocketAddr>,
    /// Issue 745: optional worker-pool size override. Populated from
    /// `AETHER_WORKERS` / `--workers`; `None` keeps `PoolConfig::default()`
    /// behavior (`available_parallelism() - 1`, min 1).
    pub workers: Option<usize>,
    /// Issue 1990: per-actor ring capacities resolved from the
    /// `ActorRingConfig` knob (`AETHER_ACTOR_LOG_RING_SIZE` /
    /// `AETHER_ACTOR_TRACE_RING_SIZE`). Default is
    /// [`RingCapacities::default`] (the `aether-actor` const caps).
    pub ring_caps: RingCapacities,
    /// Issue 2485: scheduler hot-path tuning resolved from the
    /// `SchedulerTuningConfig` knob (`AETHER_SPIN_WINDOW_USEC` /
    /// `AETHER_LOCAL_STICKY_MAX` / …). Default is
    /// [`SchedulerTuning::default`] (the built-in scheduler literals).
    pub scheduler_tuning: SchedulerTuning,
    /// Issue #2509: cumulative patience for the instanced-actor teardown
    /// close-done gate, resolved from `AETHER_SETTLEMENT_CAP_SECS` /
    /// `[settlement]`.
    pub teardown_cap: Duration,
    /// Issue 2706: render boot knobs resolved from the
    /// `RenderTuningConfig` knob (`AETHER_RENDER_VERTEX_BUFFER_BYTES`).
    /// Threaded into the render cap's `RenderConfig` in `build_inner`.
    pub render_tuning: RenderTuningConfig,
    /// Force-complete deadline (ms) for a pending lifecycle advance's
    /// `Settled` (issue 1048). Resolved from
    /// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS` via `ChassisBootConfig`;
    /// default [`aether_lifecycle::LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT`].
    pub lifecycle_advance_timeout_millis: u64,
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
        // ADR-0090 §4 (e1): warn on any unknown AETHER_ env var.
        validate_env(&chassis_known_keys())?;
        let DesktopCli {
            common,
            audio: audio_overlay,
            window: window_overlay,
            // The bin handles `--print-config` / `--describe` (print + exit)
            // before this resolver runs.
            config,
            print_config: _,
            describe: _,
        } = cli;
        let config_file = load_chassis_config(config)?;
        let config_file = config_file.as_ref();
        let CommonOverlay {
            http,
            http_server: http_server_overlay,
            fs,
            anthropic,
            gemini,
            contentgen,
            chassis_boot: chassis_boot_overlay,
            rpc_port: cli_rpc_port,
        } = common;

        let chassis_boot =
            resolve_with_file::<ChassisBootConfig>(chassis_boot_overlay.into_layer(), config_file, "chassis")?;
        let window_config = resolve_with_file::<WindowConfig>(window_overlay.into_layer(), config_file, "window")?;

        // Boot manifest: argv wins over `AETHER_BOOT_MANIFEST` (resolved
        // through `ChassisBootConfig`). When set, the listed components'
        // wasm + config are read into the autoload list `build_inner`
        // drains into `aether.component.load`; an unreadable manifest
        // aborts boot (ADR-0090 §4) via `ConfigError`.
        let autoload = match chassis_boot.boot_manifest.clone() {
            Some(path) => boot_manifest_autoload(Path::new(&path))?,
            None => Vec::new(),
        };

        let http = resolve_with_file::<HttpConf>(http.into_layer(), config_file, "http")?;
        let anthropic = resolve_with_file::<AnthropicConfig>(anthropic.into_layer(), config_file, "anthropic")?;
        let gemini = resolve_with_file::<GeminiConfig>(gemini.into_layer(), config_file, "gemini")?;
        let contentgen = resolve_with_file::<ContentGenConfig>(contentgen.into_layer(), config_file, "contentgen")?;
        let namespace_roots = resolve_with_file::<NamespaceRoots>(fs.into_layer(), config_file, "fs")?;
        // ADR-0155 §3: the HTTP server cap is always composed and always
        // claims its mailbox; its `enabled` flag (default off) gates only
        // whether Start binds the socket. Resolve the whole config and hand
        // it over — an unconfigured chassis binds no HTTP port but still
        // answers `aether.http.server` mail with a fail-fast `Err`.
        let http_server =
            resolve_with_file::<HttpServerConfig>(http_server_overlay.into_layer(), config_file, "http-server")?;
        let audio = resolve_with_file::<AudioConf>(audio_overlay.into_layer(), config_file, "audio")?;

        // Window mode and title: resolved through `WindowConfig` (argv > env >
        // default). `to_boot_mode` delegates to `parse_window_mode_env` and
        // soft-falls back to `Windowed` on a bad value; `to_boot_title` maps
        // `None` / empty to `"aether"`.
        let (boot_mode, boot_size) = window_config.to_boot_mode();
        let boot_title = window_config.to_boot_title();
        let boot_wireframe = window_config.wireframe;

        let rpc_addr = {
            use std::net::{IpAddr, Ipv4Addr};
            cli_rpc_port.or_else(rpc_port_from_env).map(|p| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p))
        };

        let workers = chassis_boot.to_workers();
        let lifecycle_advance_timeout_millis = chassis_boot.lifecycle_advance_timeout_millis;
        // Issue 1990: resolve the per-actor ring capacities from
        // `AETHER_ACTOR_{LOG,TRACE}_RING_SIZE` (ADR-0090 §4 hard-error on
        // an unparseable known value, surfaced as `DesktopBootError::Config`).
        let ring_caps = resolve_env_with_file::<ActorRingConfig>(config_file, "actor")?.to_ring_capacities();
        // Issue 2485: resolve the scheduler hot-path tuning from
        // `AETHER_SPIN_WINDOW_USEC` / `AETHER_LOCAL_*` / `AETHER_BLOB_*` /
        // `AETHER_*_COST_*` (ADR-0090 §4 hard-error on an unparseable
        // known value, surfaced as `DesktopBootError::Config`).
        let scheduler_tuning =
            resolve_env_with_file::<SchedulerTuningConfig>(config_file, "scheduler")?.to_scheduler_tuning();
        let teardown_cap = resolve_teardown_cap_with_file(config_file)?;
        // Issue 2706: resolve the render boot knobs
        // (`AETHER_RENDER_VERTEX_BUFFER_BYTES`; ADR-0090 §4 hard-error
        // on an unparseable known value).
        let render_tuning = resolve_env_with_file::<RenderTuningConfig>(config_file, "render")?;

        Ok(Self {
            namespace_roots,
            http,
            anthropic,
            gemini,
            contentgen,
            audio,
            boot_mode,
            boot_size,
            boot_title,
            boot_wireframe,
            http_server,
            rpc_addr,
            workers,
            ring_caps,
            scheduler_tuning,
            teardown_cap,
            render_tuning,
            lifecycle_advance_timeout_millis,
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
            namespace_roots,
            http,
            http_server,
            anthropic,
            gemini,
            contentgen,
            audio,
            rpc_addr,
            workers,
            ring_caps,
            scheduler_tuning,
            teardown_cap,
            render_tuning,
            lifecycle_advance_timeout_millis,
            boot_mode: _,
            boot_size: _,
            boot_title: _,
            boot_wireframe: _,
            autoload: _,
        } = env;

        let component_host_config = ComponentHostConfig {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        let input_config = InputConfig::default();
        // ADR-0155 §4: the capture backend is a Start-stage handoff, not a
        // config field. The driver builds it from the `CaptureQueue` +
        // `EventLoopProxy` wake + reply egress and installs it into the
        // published `RenderHandles` in its `boot` (Start), so the cap's
        // `on_capture_frame` reads it there. The render *config* stays pure
        // data.
        let render_config = RenderConfig {
            // Issue 2706: the resolved vertex-buffer cap sizes both the
            // cap accumulator's truncation and (via `RenderHandles`)
            // the GPU vertex buffer the driver creates.
            vertex_buffer_bytes: render_tuning.vertex_buffer_bytes,
            // The `capture_frame` similarity check (iamacoffeepot/aether#1780)
            // reads its reference image from the same `assets` root the fs
            // cap serves, so the render cap loads it off the hot path.
            assets_dir: Some(namespace_roots.assets.clone()),
            ..RenderConfig::default()
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
            ring_caps,
            scheduler_tuning,
            // Issue #2509: the instanced-actor teardown gate honors the
            // same `AETHER_SETTLEMENT_CAP_SECS` knob (including its
            // `0 → wait forever` sentinel) as the settlement gates.
            teardown_cap,
            input_config,
            component_host_config,
            namespace_roots,
            http,
            anthropic,
            gemini,
            contentgen,
            game_gateway: aether_game::GameGatewayConfig::default(),
        };
        // ADR-0082 §11 / issues 1378 + 1489: desktop drives the shared
        // `Tick → Render → Present → Tick` frame graph, with the `Quit`
        // escape to `Shutdown` on `Present` so OS-close / ctrlc drain the
        // in-flight frame before shutting down (see the driver's
        // `CloseRequested` → `Quit` bridge and terminal-reached exit).
        let builder = with_common_caps(Builder::<Self>::new(registry, mailer), common)
            .with_actor::<AudioCapability>(audio, ())
            .with_actor::<ClipboardCapability>(ClipboardConfig::System, ())
            .with_actor::<RenderCapability>(render_config, ())
            .with_actor::<UnsupportedSubstrateHarnessCapability>((), ())
            .with_actor::<LifecycleCapability>(frame_lifecycle_config(lifecycle_advance_timeout_millis), ());
        with_rpc_server(builder, rpc_addr, "aether-desktop").with_actor::<HttpServerCapability>(http_server, ())
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

        let boot = SubstrateBoot::builder("hello-triangle", env!("CARGO_PKG_VERSION")).build()?;
        let mailer = Arc::clone(&boot.queue);

        // Driver-only / post-build fields, read out before `compose` consumes
        // `env`: the window boot knobs ride the desktop driver, the autoload
        // list is drained after build. `WindowMode` is `Clone` (not `Copy`);
        // these are tiny, so the clones are free.
        let workers = env.workers;
        let boot_mode = env.boot_mode.clone();
        let boot_size = env.boot_size;
        let boot_title = env.boot_title.clone();
        let boot_wireframe = env.boot_wireframe.clone();
        let autoload = mem::take(&mut env.autoload);

        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            "componentless boot — close window to exit; load a component via aether.component.load",
        );

        let builder = Self::compose(&boot, env);
        // Issue 552 stage 2d: render is a NativeActor. The chassis builder
        // constructs the cap inside `init` (called from
        // `with_actor::<RenderCapability>(config)`); `init` publishes the
        // `RenderHandles` bundle on the exported-handle map, and the driver
        // fetches it via `DriverCtx::handle::<RenderHandles>()`. `boot` moves
        // into the driver here, after `compose` finished borrowing it.
        let driver = DesktopDriverCapability {
            event_loop,
            boot,
            capture_queue,
            boot_mode,
            boot_size,
            boot_title,
            boot_wireframe,
        };
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
